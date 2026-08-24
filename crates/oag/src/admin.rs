//! `oag admin` — the operations a human runs from a shell.
//!
//! Enough to stand up a working gateway without the UI existing yet, and to
//! recover one when the UI is the thing that is broken.

use clap::Subcommand;
use oag_core::{Kek, Result, credential::SecretMaterial};
use oag_store::{Db, repo};
use rand::Rng;
use rust_decimal::Decimal;
use uuid::Uuid;

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Create the first principal, a default route, and an API key.
    ///
    /// Idempotent on the principal and route; always mints a new key.
    Init {
        #[arg(long, default_value = "admin@localhost")]
        email: String,
        #[arg(long, default_value = "default")]
        route: String,
        /// Monthly spend cap in USD. Omit for uncapped.
        #[arg(long)]
        budget_usd: Option<Decimal>,
    },
    /// Mint an API key for an existing principal and route.
    Key {
        #[arg(long)]
        email: String,
        #[arg(long, default_value = "default")]
        route: String,
        #[arg(long, default_value = "cli")]
        name: String,
        /// Never route below this tier, whatever the classifier says.
        #[arg(long)]
        floor_tier: Option<String>,
        /// Mint an admin key: one that can reach the admin API and perform
        /// writes. Deliberately opt-in — an inference key must not be able to
        /// disable credentials just because its owner happens to be an admin.
        #[arg(long)]
        admin: bool,
    },
    /// Revoke an inbound key by its displayed prefix.
    ///
    /// The one write that genuinely needs a CLI: during an incident the prefix
    /// is what an operator can actually see (in a log, in the dashboard), and
    /// psql alone cannot evict the shared auth cache, so a row update there
    /// leaves the key working on every replica for up to its cache TTL.
    RevokeKey {
        #[arg(long)]
        prefix: String,
    },
    /// Register an upstream credential.
    AddAccount {
        #[arg(long)]
        name: String,
        /// Not needed with `--from-grok`, which knows its provider.
        #[arg(long, required_unless_present = "from_grok")]
        provider: Option<String>,
        /// The provider API key. Read from `OAG_ACCOUNT_SECRET` if omitted, so it
        /// need not appear in shell history or the process table.
        #[arg(
            long,
            env = "OAG_ACCOUNT_SECRET",
            hide_env_values = true,
            required_unless_present = "from_grok",
            conflicts_with = "from_grok"
        )]
        secret: Option<String>,
        /// Import every signed-in Grok CLI session as an xAI OAuth credential.
        /// Reads `~/.grok/auth.json` (or `--auth-file`), never writes it.
        #[arg(long)]
        from_grok: bool,
        /// Where to read Grok CLI sessions from. Repeatable; the first file a
        /// token appears in wins.
        #[arg(long, requires = "from_grok")]
        auth_file: Vec<String>,
        #[arg(long, default_value = "default")]
        route: String,
        #[arg(long, default_value_t = 8)]
        max_concurrency: i32,
        #[arg(long, default_value_t = 0)]
        priority: i16,
        /// Bind to one principal instead of the shared pool. See
        /// docs/compliance.md.
        #[arg(long)]
        owner_email: Option<String>,
        /// Put an OAuth seat in the shared pool anyway. Deliberate opt-in:
        /// subscription seats are sanctioned for the holder's own use, so the
        /// default for `--from-grok` is per-principal binding.
        #[arg(long, requires = "from_grok", conflicts_with = "owner_email")]
        shared: bool,
    },
    /// Load model pricing into the catalog.
    SeedCatalog {
        /// A LiteLLM-format `model_prices_and_context_window.json`. Omit to use
        /// the small built-in set.
        #[arg(long)]
        from: Option<String>,
    },
    /// Choose whether a concrete model name is honoured or overridden.
    SetMode {
        #[arg(long, default_value = "default")]
        route: String,
        /// `passthrough` honours a named model; `managed` applies policy to
        /// every request. Virtual `oag/*` names are always managed.
        #[arg(long)]
        mode: String,
    },
    /// Set a route's tier ladder from JSON.
    SetTiers {
        #[arg(long, default_value = "default")]
        route: String,
        /// `[{"name":"cheap","models":["kimi/k2"]}, ...]`, cheapest first.
        #[arg(long)]
        tiers: String,
    },
    /// Drop the shared auth cache.
    ///
    /// Budget, quota, and floor-tier changes are read through a cache, so they
    /// take up to five minutes to reach every replica. This clears the shared
    /// tier immediately; each replica's own short-lived cache expires within
    /// fifteen seconds, which bounds the rest.
    FlushCache,
    /// Show routes, credentials, and this month's spend.
    Status,
}

pub async fn run(cmd: AdminCommand, db: &Db, kek: &Kek, redis_url: &str) -> Result<()> {
    match cmd {
        AdminCommand::Init {
            email,
            route,
            budget_usd,
        } => init(db, &email, &route, budget_usd).await,
        AdminCommand::Key {
            email,
            route,
            name,
            floor_tier,
            admin,
        } => {
            let key = mint_key(db, &email, &route, &name, floor_tier.as_deref(), admin).await?;
            print_key(&key);
            Ok(())
        }
        AdminCommand::AddAccount {
            name,
            provider,
            secret,
            from_grok,
            auth_file,
            route,
            max_concurrency,
            priority,
            owner_email,
            shared,
        } => {
            if from_grok {
                import_grok(
                    db,
                    kek,
                    &name,
                    &auth_file,
                    &route,
                    max_concurrency,
                    priority,
                    owner_email.as_deref(),
                    shared,
                )
                .await
            } else {
                // clap enforces both when --from-grok is absent; this is the
                // belt to that suspender.
                let (Some(provider), Some(secret)) = (provider, secret) else {
                    return Err(oag_core::Error::Config(
                        "--provider and --secret are required without --from-grok".to_owned(),
                    ));
                };
                add_account(
                    db,
                    kek,
                    &name,
                    &provider,
                    &secret,
                    &route,
                    max_concurrency,
                    priority,
                    owner_email.as_deref(),
                )
                .await
            }
        }
        AdminCommand::SeedCatalog { from } => seed_catalog(db, from.as_deref()).await,
        AdminCommand::SetMode { route, mode } => set_mode(db, &route, &mode).await,
        AdminCommand::SetTiers { route, tiers } => set_tiers(db, &route, &tiers).await,
        AdminCommand::RevokeKey { prefix } => revoke_key(db, redis_url, &prefix).await,
        AdminCommand::FlushCache => flush_cache(redis_url).await,
        AdminCommand::Status => status(db).await,
    }
}

async fn init(db: &Db, email: &str, route: &str, budget: Option<Decimal>) -> Result<()> {
    let principal_id = upsert_principal(db, email, "admin", budget).await?;
    let route_id = upsert_route(db, route).await?;
    println!("principal {email} -> {principal_id}");
    println!("route     {route} -> {route_id}");
    let key = mint_key(db, email, route, "initial", None, true).await?;
    print_key(&key);
    println!("\nNext:");
    println!("  This is an ADMIN key: it can disable credentials and revoke keys.");
    println!("  Do not paste it into a client. Mint a separate one for SDKs:");
    println!("      oag admin key --email {email} --route {route} --name codex");
    println!();
    println!("  oag admin seed-catalog");
    println!("  oag admin add-account --name <n> --provider anthropic --secret <key>");
    println!();
    println!("  This route is in passthrough mode: a client that names a concrete");
    println!("  model gets that model. Clients asking for oag/auto are routed by");
    println!("  policy. To apply policy to every request, including ones that name");
    println!("  a model:");
    println!("      oag admin set-mode --route {route} --mode managed");
    Ok(())
}

async fn upsert_principal(
    db: &Db,
    email: &str,
    role: &str,
    budget: Option<Decimal>,
) -> Result<Uuid> {
    let id: (Uuid,) = sqlx::query_as(
        r"
        INSERT INTO principal (id, email, role, monthly_budget_usd)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (email) DO UPDATE SET
            role = EXCLUDED.role,
            monthly_budget_usd = COALESCE(EXCLUDED.monthly_budget_usd, principal.monthly_budget_usd),
            updated_at = now()
        RETURNING id
        ",
    )
    .bind(Uuid::now_v7())
    .bind(email)
    .bind(role)
    .bind(budget)
    .fetch_one(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("creating principal: {e}")))?;
    Ok(id.0)
}

/// A starter ladder. Deliberately three rungs with one model each: it is the
/// smallest thing that demonstrates classification, escalation, and budget
/// downgrade all doing something.
const DEFAULT_TIERS: &str = r#"[
  {"name": "cheap",    "models": ["anthropic/claude-haiku-4.5"]},
  {"name": "balanced", "models": ["anthropic/claude-sonnet-4.5"]},
  {"name": "frontier", "models": ["anthropic/claude-opus-5"]}
]"#;

async fn upsert_route(db: &Db, name: &str) -> Result<Uuid> {
    let tiers: serde_json::Value =
        serde_json::from_str(DEFAULT_TIERS).map_err(oag_core::Error::Serde)?;
    let id: (Uuid,) = sqlx::query_as(
        r"
        INSERT INTO route (id, name, tiers, default_mode)
        VALUES ($1, $2, $3, 'passthrough')
        ON CONFLICT (name) DO UPDATE SET updated_at = now()
        RETURNING id
        ",
    )
    .bind(Uuid::now_v7())
    .bind(name)
    .bind(tiers)
    .fetch_one(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("creating route: {e}")))?;
    Ok(id.0)
}

/// Mint a key. The plaintext is returned once and never stored.
async fn mint_key(
    db: &Db,
    email: &str,
    route: &str,
    name: &str,
    floor_tier: Option<&str>,
    admin: bool,
) -> Result<String> {
    use std::fmt::Write as _;

    // 32 bytes of entropy. The prefix is there so a leaked key is recognisable
    // in a log and can be grepped for during an incident.
    let mut raw = [0u8; 32];
    rand::thread_rng().fill(&mut raw);
    let key = format!(
        "oag_live_{}",
        raw.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    );

    let hash = repo::hash_key(&key);
    let prefix: String = key.chars().take(16).collect();

    sqlx::query(
        r"
        INSERT INTO api_key
            (id, key_hash, key_prefix, name, principal_id, route_id, floor_tier, admin)
        SELECT $1, $2, $3, $4, p.id, r.id, $7, $8
        FROM principal p, route r
        WHERE p.email = $5 AND r.name = $6
        ",
    )
    .bind(Uuid::now_v7())
    .bind(&hash)
    .bind(&prefix)
    .bind(name)
    .bind(email)
    .bind(route)
    .bind(floor_tier)
    .bind(admin)
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("minting key: {e}")))?;

    Ok(key)
}

fn print_key(key: &str) {
    println!("\n  {key}\n");
    println!("  This is shown once. Only its SHA-256 is stored, so it cannot be recovered.");
}

#[allow(clippy::too_many_arguments)]
async fn add_account(
    db: &Db,
    kek: &Kek,
    name: &str,
    provider: &str,
    secret: &str,
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_email: Option<&str>,
) -> Result<()> {
    // Validate before storing, so a typo fails here rather than on the first
    // request with an opaque upstream 404.
    let provider: oag_core::Provider = provider.parse()?;

    let material = SecretMaterial {
        access_token: secret.to_owned(),
        refresh_token: None,
        expires_at: None,
        version: 0,
        client_id: None,
    };

    let owner_id = find_owner(db, owner_email).await?;
    let id = insert_account(
        db,
        kek,
        name,
        provider,
        "api_key",
        &material,
        route,
        max_concurrency,
        priority,
        owner_id,
    )
    .await?;

    println!("account {name} ({provider}) -> {id}");
    println!(
        "  sealed at rest, attached to route '{route}', {}",
        scope_of(owner_id)
    );
    Ok(())
}

/// Import every signed-in Grok CLI session as an xAI OAuth credential.
///
/// Reads the CLI's `auth.json` and never writes it: the CLI owns that file,
/// and rotated tokens land in the `account` row instead, where `ensure_fresh`
/// persists them version-guarded.
#[allow(clippy::too_many_arguments)]
async fn import_grok(
    db: &Db,
    kek: &Kek,
    name: &str,
    auth_files: &[String],
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_email: Option<&str>,
    shared: bool,
) -> Result<()> {
    // A subscription seat is sanctioned for its holder's own use, so binding
    // to a principal is the default and pooling is the explicit choice —
    // docs/compliance.md has the distinction this encodes.
    if owner_email.is_none() && !shared {
        return Err(oag_core::Error::Config(
            "a subscription seat binds to one principal by default: pass \
             --owner-email <email>, or --shared to pool it deliberately"
                .to_owned(),
        ));
    }

    let paths: Vec<String> = if auth_files.is_empty() {
        let home = std::env::var("HOME")
            .map_err(|_| oag_core::Error::Config("HOME is not set; pass --auth-file".to_owned()))?;
        vec![format!("{home}/.grok/auth.json")]
    } else {
        auth_files.to_vec()
    };

    let mut batches = Vec::new();
    for path in &paths {
        let json = std::fs::read_to_string(path)
            .map_err(|e| oag_core::Error::Config(format!("reading {path}: {e}")))?;
        batches.push(
            oag_upstream::xai_oauth::sessions_from_json(&json)
                .map_err(|e| oag_core::Error::Config(format!("{path}: {e}")))?,
        );
    }
    let sessions = oag_upstream::xai_oauth::union_sessions(batches);
    if sessions.is_empty() {
        return Err(oag_core::Error::Config(format!(
            "no signed-in xAI session in {}; run `grok` and log in first",
            paths.join(", ")
        )));
    }

    let owner_id = find_owner(db, owner_email).await?;
    let many = sessions.len() > 1;
    for (i, session) in sessions.into_iter().enumerate() {
        let row_name = if many {
            format!("{name}-{}", i + 1)
        } else {
            name.to_owned()
        };
        let refreshable = session.refresh_token.is_some();
        let id = insert_account(
            db,
            kek,
            &row_name,
            oag_core::Provider::XAI,
            "oauth",
            &session.into_material(),
            route,
            max_concurrency,
            priority,
            owner_id,
        )
        .await?;
        println!("account {row_name} (xai, oauth) -> {id}");
        if !refreshable {
            println!("  no refresh token in this session: it will die at expiry");
        }
    }
    println!(
        "  sealed at rest, attached to route '{route}', {}",
        scope_of(owner_id)
    );
    println!("  auth.json was read, not written; the Grok CLI stays signed in");
    Ok(())
}

async fn find_owner(db: &Db, owner_email: Option<&str>) -> Result<Option<Uuid>> {
    match owner_email {
        Some(email) => {
            let row: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM principal WHERE email = $1")
                .bind(email)
                .fetch_optional(db.pool())
                .await
                .map_err(|e| oag_core::Error::Internal(format!("finding owner: {e}")))?;
            let id = row
                .ok_or_else(|| oag_core::Error::Config(format!("no principal with email {email}")))?
                .0;
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

const fn scope_of(owner_id: Option<Uuid>) -> &'static str {
    if owner_id.is_some() {
        "personal (bound to one principal)"
    } else {
        "shared pool"
    }
}

/// Seal the material and insert one account row, attached to a route.
#[allow(clippy::too_many_arguments)]
async fn insert_account(
    db: &Db,
    kek: &Kek,
    name: &str,
    provider: oag_core::Provider,
    kind: &str,
    material: &SecretMaterial,
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_id: Option<Uuid>,
) -> Result<Uuid> {
    let sealed = kek.seal_json(material)?;
    // Denormalised so the scheduler can skip expired credentials without
    // decrypting every candidate; see the schema comment.
    let expires = material
        .expires_at
        .and_then(|e| time::OffsetDateTime::from_unix_timestamp(e).ok());

    let id = Uuid::now_v7();
    sqlx::query(
        r"
        INSERT INTO account (
            id, name, provider, kind, credentials_sealed, credentials_nonce,
            token_expires_at, owner_principal_id, priority, max_concurrency
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
        ",
    )
    .bind(id)
    .bind(name)
    .bind(provider.as_str())
    .bind(kind)
    .bind(&sealed.ciphertext)
    .bind(&sealed.nonce)
    .bind(expires)
    .bind(owner_id)
    .bind(priority)
    .bind(max_concurrency)
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("creating account: {e}")))?;

    sqlx::query(
        "INSERT INTO account_route (account_id, route_id) SELECT $1, id FROM route WHERE name = $2",
    )
    .bind(id)
    .bind(route)
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("attaching account to route: {e}")))?;

    Ok(id)
}

async fn set_mode(db: &Db, route: &str, mode: &str) -> Result<()> {
    if !matches!(mode, "passthrough" | "managed") {
        return Err(oag_core::Error::Config(format!(
            "mode must be 'passthrough' or 'managed', not '{mode}'"
        )));
    }
    let n = sqlx::query("UPDATE route SET default_mode = $2, updated_at = now() WHERE name = $1")
        .bind(route)
        .bind(mode)
        .execute(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("setting mode: {e}")))?;
    if n.rows_affected() == 0 {
        return Err(oag_core::Error::Config(format!("no route named {route}")));
    }
    println!("route '{route}' mode: {mode}");
    if mode == "managed" {
        println!("  concrete model names will now be overridden by policy");
    } else {
        println!("  concrete model names will be honoured; oag/* stays managed");
    }
    Ok(())
}

async fn set_tiers(db: &Db, route: &str, tiers: &str) -> Result<()> {
    // Parse through the real type, so a malformed ladder is rejected here and
    // not on the first request that route serves.
    let rungs: Vec<oag_router::ladder::Rung> =
        serde_json::from_str(tiers).map_err(oag_core::Error::Serde)?;
    if oag_router::TierLadder::new(rungs.clone()).is_none() {
        return Err(oag_core::Error::Config(
            "a ladder needs at least one rung".to_owned(),
        ));
    }
    let value: serde_json::Value = serde_json::from_str(tiers).map_err(oag_core::Error::Serde)?;

    let n = sqlx::query("UPDATE route SET tiers = $2, updated_at = now() WHERE name = $1")
        .bind(route)
        .bind(value)
        .execute(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("setting tiers: {e}")))?;

    if n.rows_affected() == 0 {
        return Err(oag_core::Error::Config(format!("no route named {route}")));
    }
    println!("route '{route}' ladder set: {} rungs", rungs.len());
    for (i, r) in rungs.iter().enumerate() {
        println!("  {i}. {} -> {}", r.name, r.models.len());
    }
    Ok(())
}

async fn seed_catalog(db: &Db, from: Option<&str>) -> Result<()> {
    let entries = match from {
        Some(path) => crate::catalog::from_litellm_file(path)?,
        None => crate::catalog::builtin(),
    };
    let n = entries.len();
    for m in &entries {
        repo::upsert_model(db, m, false).await?;
    }
    println!("catalog: {n} models");
    Ok(())
}

async fn revoke_key(db: &Db, redis_url: &str, prefix: &str) -> Result<()> {
    let Some((hash, name, prefix)) = repo::revoke_key_by_prefix(db, prefix).await? else {
        println!("no active key with prefix {prefix}");
        return Ok(());
    };

    // The row update alone is not a revocation: every replica caches auth by
    // hash, so without this the key keeps working until those entries expire.
    oag_store::Cache::connect(redis_url)?
        .auth_invalidate(&hash)
        .await;

    // Same target and shape as the server's audit line, so the CLI is not a
    // hole in the trail.
    tracing::warn!(
        target: "oag::audit",
        actor = "cli",
        action = "key.revoke",
        subject = %prefix,
        name,
        "admin write"
    );
    println!("revoked {name} ({prefix})");
    println!("  shared cache evicted; each replica's in-process cache expires within 15s");
    Ok(())
}

async fn flush_cache(redis_url: &str) -> Result<()> {
    let cache = oag_store::Cache::connect(redis_url)?;
    let n = cache.flush_auth_cache().await?;
    println!("dropped {n} cached auth entries");
    println!("  each replica's in-process cache expires within 15s");
    Ok(())
}

async fn status(db: &Db) -> Result<()> {
    let routes: Vec<(String, i64, Option<Decimal>)> = sqlx::query_as(
        r"
        SELECT r.name, count(ar.account_id), r.monthly_budget_usd
        FROM route r LEFT JOIN account_route ar ON ar.route_id = r.id
        GROUP BY r.id ORDER BY r.name
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing routes: {e}")))?;

    println!("routes");
    for (name, accounts, budget) in routes {
        let b = budget.map_or_else(|| "uncapped".to_owned(), |b| format!("${b}/mo"));
        println!("  {name:<20} {accounts} credential(s)  {b}");
    }

    let accounts: Vec<(String, String, bool, Option<time::OffsetDateTime>)> = sqlx::query_as(
        "SELECT name, provider, schedulable, cooldown_until FROM account ORDER BY provider, name",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing accounts: {e}")))?;

    println!("\ncredentials");
    for (name, provider, schedulable, cooldown) in accounts {
        let state = if !schedulable {
            "disabled"
        } else if cooldown.is_some_and(|t| t > time::OffsetDateTime::now_utc()) {
            "cooling down"
        } else {
            "ready"
        };
        println!("  {name:<20} {provider:<12} {state}");
    }

    // The headline number: what the gateway saved this month.
    let spend: Option<(Decimal, Decimal, i64)> = sqlx::query_as(
        r"
        SELECT COALESCE(SUM(cost_usd),0), COALESCE(SUM(counterfactual_usd),0), COUNT(*)
        FROM usage_event WHERE occurred_at >= date_trunc('month', now())
        ",
    )
    .fetch_optional(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("summing spend: {e}")))?;

    if let Some((cost, counterfactual, n)) = spend {
        println!("\nthis month  {n} requests");
        println!("  spent            ${cost:.4}");
        println!("  frontier-for-all ${counterfactual:.4}");
        println!("  saved            ${:.4}", counterfactual - cost);
    }
    Ok(())
}
