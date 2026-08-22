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
    },
    /// Register an upstream credential.
    AddAccount {
        #[arg(long)]
        name: String,
        #[arg(long)]
        provider: String,
        /// The provider API key. Read from `OAG_ACCOUNT_SECRET` if omitted, so it
        /// need not appear in shell history or the process table.
        #[arg(long, env = "OAG_ACCOUNT_SECRET", hide_env_values = true)]
        secret: String,
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
        } => {
            let key = mint_key(db, &email, &route, &name, floor_tier.as_deref()).await?;
            print_key(&key);
            Ok(())
        }
        AdminCommand::AddAccount {
            name,
            provider,
            secret,
            route,
            max_concurrency,
            priority,
            owner_email,
        } => {
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
        AdminCommand::SeedCatalog { from } => seed_catalog(db, from.as_deref()).await,
        AdminCommand::SetMode { route, mode } => set_mode(db, &route, &mode).await,
        AdminCommand::SetTiers { route, tiers } => set_tiers(db, &route, &tiers).await,
        AdminCommand::FlushCache => flush_cache(redis_url).await,
        AdminCommand::Status => status(db).await,
    }
}

async fn init(db: &Db, email: &str, route: &str, budget: Option<Decimal>) -> Result<()> {
    let principal_id = upsert_principal(db, email, "admin", budget).await?;
    let route_id = upsert_route(db, route).await?;
    println!("principal {email} -> {principal_id}");
    println!("route     {route} -> {route_id}");
    let key = mint_key(db, email, route, "initial", None).await?;
    print_key(&key);
    println!("\nNext:");
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
        INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id, floor_tier)
        SELECT $1, $2, $3, $4, p.id, r.id, $7
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
    };
    let sealed = kek.seal_json(&material)?;

    let owner_id = match owner_email {
        Some(email) => {
            let row: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM principal WHERE email = $1")
                .bind(email)
                .fetch_optional(db.pool())
                .await
                .map_err(|e| oag_core::Error::Internal(format!("finding owner: {e}")))?;
            Some(
                row.ok_or_else(|| {
                    oag_core::Error::Config(format!("no principal with email {email}"))
                })?
                .0,
            )
        }
        None => None,
    };

    let id = Uuid::now_v7();
    sqlx::query(
        r"
        INSERT INTO account (
            id, name, provider, kind, credentials_sealed, credentials_nonce,
            owner_principal_id, priority, max_concurrency
        ) VALUES ($1,$2,$3,'api_key',$4,$5,$6,$7,$8)
        ",
    )
    .bind(id)
    .bind(name)
    .bind(provider.as_str())
    .bind(&sealed.ciphertext)
    .bind(&sealed.nonce)
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

    let scope = if owner_id.is_some() {
        "personal (bound to one principal)"
    } else {
        "shared pool"
    };
    println!("account {name} ({provider}) -> {id}");
    println!("  sealed at rest, attached to route '{route}', {scope}");
    Ok(())
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
