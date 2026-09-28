//! `oag admin account`: upstream credentials, seat imports, reserves and slots.

use super::{AccountAddArgs, AccountCommand, AccountSource};
use oag_core::{Kek, Result, credential::SecretMaterial};
use oag_store::Db;
use rust_decimal::Decimal;
use uuid::Uuid;

pub(super) async fn account_cmd(
    db: &Db,
    kek: &Kek,
    cmd: AccountCommand,
    redis_url: &str,
) -> Result<()> {
    match cmd {
        AccountCommand::Add { args } => add_account_from_args(db, kek, args).await,
        AccountCommand::List => list_accounts(db).await,
        AccountCommand::Rename { from, to } => rename_account(db, &from, &to).await,
        AccountCommand::Disable { name } => set_account_schedulable(db, &name, false).await,
        AccountCommand::Enable { name } => set_account_schedulable(db, &name, true).await,
        AccountCommand::SetCost { name, monthly_cost } => {
            set_account_cost(db, &name, monthly_cost).await
        }
        AccountCommand::SetReserve { name, pct } => set_account_reserve(db, &name, pct).await,
        AccountCommand::ClearSlots { name } => clear_account_slots(db, redis_url, &name).await,
    }
}

/// Set or clear a seat's monthly price.
///
/// Accepts any account rather than only a flat-rate one: a price on a metered
/// key is meaningless but harmless, and refusing it would mean explaining the
/// kinds taxonomy at the moment someone is trying to correct a typo.
async fn set_account_cost(db: &Db, name: &str, monthly_cost: Option<Decimal>) -> Result<()> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "UPDATE account SET monthly_cost_usd = $2, updated_at = now() \
         WHERE name = $1 RETURNING name, kind",
    )
    .bind(name)
    .bind(monthly_cost)
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("updating account: {e}")))?;

    let Some((_, kind)) = rows.first() else {
        return Err(oag_core::Error::Config(format!(
            "no credential named {name}; see `oag admin account list`"
        )));
    };

    match monthly_cost {
        Some(cost) => println!("{name} costs ${cost}/month"),
        None => println!("{name} has no monthly price; its saving will read as unknown"),
    }
    // Worth saying once: the figure only ever surfaces on a flat-rate line, so
    // setting it on an API key looks like it did nothing.
    if kind != "oauth" {
        println!("  note: {name} is a {kind} credential, and only subscription");
        println!("  seats are metered against a monthly price");
    }
    Ok(())
}

/// Reject a reserve the column would reject anyway, before it costs a round
/// trip and comes back as a constraint violation.
///
/// The message names the range rather than merely saying "invalid", because
/// `--pct 0.15` is the mistake somebody makes once: a fraction where a
/// percentage was wanted parses as 0, and a reserve of 0 is a reserve that
/// never fires. Separated from the update so the range can be tested without a
/// database, which is the half of this that is worth testing.
pub(super) fn validated_reserve(pct: Option<i16>) -> Result<Option<i16>> {
    match pct {
        Some(p) if !(0..=100).contains(&p) => Err(oag_core::Error::Config(format!(
            "reserve {p} is not a percentage; --pct takes 0-100"
        ))),
        other => Ok(other),
    }
}

/// Set or clear the share of a subscription's quota to leave unspent.
///
/// Accepts any account, on the same reasoning as `set_account_cost`: a reserve
/// on a metered key is inert — nothing polls it, so its remaining percentage
/// stays NULL and the scheduler never holds it back — and refusing one would
/// mean explaining the kinds taxonomy to somebody halfway through protecting a
/// seat.
async fn set_account_reserve(db: &Db, name: &str, pct: Option<i16>) -> Result<()> {
    let pct = validated_reserve(pct)?;
    let rows: Vec<(String, String, Option<rust_decimal::Decimal>)> = sqlx::query_as(
        "UPDATE account SET usage_reserve_pct = $2, updated_at = now() \
         WHERE name = $1 RETURNING name, kind, usage_remaining_pct",
    )
    .bind(name)
    .bind(pct)
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("updating account: {e}")))?;

    let Some((_, kind, remaining)) = rows.first() else {
        return Err(oag_core::Error::Config(format!(
            "no credential named {name}; see `oag admin account list`"
        )));
    };

    match pct {
        Some(p) => println!("{name} stops being scheduled at {p}% remaining"),
        None => println!("{name} has no reserve; it will be scheduled until the provider refuses"),
    }
    if kind != "oauth" {
        println!("  note: {name} is a {kind} credential, and only subscription");
        println!("  seats report how much of an allowance is left");
    } else if pct.is_some() && remaining.is_none() {
        // An unknown reading never holds a seat back, so a reserve set before
        // the first poll silently does nothing. Better said here than
        // discovered a week later when the seat is empty anyway.
        println!("  note: nothing has polled {name} yet, so the reserve holds");
        println!("  nothing back until a reading arrives");
    }
    Ok(())
}

pub(super) async fn add_account_from_args(db: &Db, kek: &Kek, args: AccountAddArgs) -> Result<()> {
    let AccountAddArgs {
        name,
        provider,
        secret,
        from,
        from_grok,
        from_codex,
        auth_file,
        route,
        max_concurrency,
        priority,
        owner_email,
        shared,
        monthly_cost,
    } = args;
    let source = if from_grok {
        Some(AccountSource::Grok)
    } else if from_codex {
        Some(AccountSource::Codex)
    } else {
        from
    };

    // The exclusion clap used to express, enforced where the distinction is
    // visible. An imported seat takes its credential from a signed-in CLI's
    // session file, so a `--secret` typed alongside `--from` would be silently
    // ignored — worth an error. One sitting in the environment is not: it is
    // there for every other invocation and says nothing about this one.
    if source.is_some() && secret.is_some() {
        return Err(oag_core::Error::Config(
            "--secret cannot be combined with --from: an imported seat takes its \
             credential from the CLI session file, so a secret passed here would be \
             ignored. A secret in OAG_ACCOUNT_SECRET is fine — this is only about the \
             flag."
                .to_owned(),
        ));
    }

    // Only after the conflict check, so the fallback cannot resurrect it.
    let secret = secret.or_else(|| std::env::var("OAG_ACCOUNT_SECRET").ok());
    match source {
        Some(AccountSource::Grok) => {
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
                monthly_cost,
            )
            .await
        }
        Some(AccountSource::Codex) => {
            import_codex(
                db,
                kek,
                &name,
                &auth_file,
                &route,
                max_concurrency,
                priority,
                owner_email.as_deref(),
                shared,
                monthly_cost,
            )
            .await
        }
        None => {
            let (Some(provider), Some(secret)) = (provider, secret) else {
                return Err(oag_core::Error::Config(
                    "--provider and --secret are required without --from. The secret may \
                     come from OAG_ACCOUNT_SECRET instead of the flag, which keeps it out \
                     of shell history."
                        .to_owned(),
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
                monthly_cost,
            )
            .await
        }
    }
}

type AccountListRow = (
    String,
    String,
    String,
    bool,
    Option<time::OffsetDateTime>,
    Option<time::OffsetDateTime>,
    i16,
    Option<rust_decimal::Decimal>,
    Option<i16>,
    Option<String>,
);

async fn list_accounts(db: &Db) -> Result<()> {
    let rows: Vec<AccountListRow> = sqlx::query_as(
        r"
        -- `owner_principal_id` joined to an email, because a credential bound
        -- to a principal serves that principal and nobody else — the scheduler
        -- filters on it — and no CLI output mentioned the binding at all. A
        -- bound seat listed as `ready` is true and misleading in the same
        -- breath: ready for one person.
        SELECT a.name, a.provider, a.kind, a.schedulable, a.cooldown_until,
               a.rate_limited_until, a.priority,
               a.usage_remaining_pct, a.usage_reserve_pct, p.email
        FROM account a
        LEFT JOIN principal p ON p.id = a.owner_principal_id
        ORDER BY a.provider, a.name
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing accounts: {e}")))?;

    if rows.is_empty() {
        println!("no credentials; add one with `oag admin account add`");
        return Ok(());
    }
    println!(
        "NAME                 PROVIDER     KIND       STATE          PRIORITY  RESERVE  OWNER"
    );
    let now = time::OffsetDateTime::now_utc();
    for (
        name,
        provider,
        kind,
        schedulable,
        cooldown,
        rate_limited,
        priority,
        remaining,
        reserve,
        owner,
    ) in rows
    {
        // "held back" outranks "ready" and nothing else: a reserved-out seat is
        // as unschedulable as a rate limited one, and a listing that called it
        // ready would contradict the request that just failed on it.
        let state = if !schedulable {
            "disabled"
        } else if cooldown.is_some_and(|t| t > now) {
            "cooling down"
        } else if rate_limited.is_some_and(|t| t > now) {
            "rate limited"
        } else if reserve_holds(remaining, reserve) {
            "held back"
        } else {
            "ready"
        };
        // A dash rather than a blank where no reserve is set: a column that
        // simply stops has already been read as "the listing is truncated".
        let reserve = reserve.map_or_else(|| "-".to_owned(), |p| format!("{p}%"));
        // A bound credential reads `ready` and is ready for exactly one person.
        // The scheduler has always filtered on this; no CLI output said so.
        let owner = owner.unwrap_or_else(|| "-".to_owned());
        println!(
            "{name:<20} {provider:<12} {kind:<10} {state:<14} {priority:<9} {reserve:<8} {owner}"
        );
    }
    Ok(())
}

/// Whether a reserve is currently holding a credential out of the pool.
///
/// Borrowed from the scheduler rather than restated, so a listing can never
/// call a seat ready that the next request will refuse to use.
pub(super) fn reserve_holds(
    remaining: Option<rust_decimal::Decimal>,
    reserve: Option<i16>,
) -> bool {
    oag_pool::held_by_reserve(remaining, reserve.map(rust_decimal::Decimal::from))
}

async fn set_account_schedulable(db: &Db, name: &str, value: bool) -> Result<()> {
    let names: Vec<String> = sqlx::query_scalar(
        "UPDATE account SET schedulable = $2, updated_at = now() WHERE name = $1 RETURNING name",
    )
    .bind(name)
    .bind(value)
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("updating account: {e}")))?;
    if names.is_empty() {
        return Err(oag_core::Error::Config(format!(
            "no credential named {name}; see `oag admin account list`"
        )));
    }
    let verb = if value { "enabled" } else { "disabled" };
    println!("{verb} {name}");
    Ok(())
}

pub(super) async fn clear_account_slots(db: &Db, redis_url: &str, name: &str) -> Result<()> {
    let rows: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM account WHERE name = $1")
            .bind(name)
            .fetch_all(db.pool())
            .await
            .map_err(|e| oag_core::Error::Internal(format!("looking up account: {e}")))?;
    let Some((id, name)) = rows.into_iter().next() else {
        return Err(oag_core::Error::Config(format!(
            "no credential named {name}; see `oag admin account list`"
        )));
    };
    let cache = oag_store::Cache::connect(redis_url)?;
    let dropped = cache
        .clear_slots(oag_core::AccountId::from_uuid(id))
        .await?;
    println!("{}", clear_slots_report(&name, id, dropped));
    Ok(())
}

/// What `account clear-slots` prints: what was dropped, and what that means
/// for any of it that was a live request.
pub(super) fn clear_slots_report(name: &str, id: uuid::Uuid, dropped: u32) -> String {
    let mut out = format!(
        "cleared {dropped} slot(s) on {name} ({id})\n  \
         Redis is empty fleet-wide; each replica zeros oag_slots_in_use on its sweep\n  \
         or immediately via POST /admin/api/accounts/{id}/clear-slots"
    );
    if dropped > 0 {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "\n  those were ghosts and live requests alike. A live one tries to retake its \
             seat on its next heartbeat, under the limit; if the seat has filled by then it \
             is refused, and {name} runs over its limit until that request finishes \
             (watch oag_slot_lost_total{{reason=\"oversubscribed\"}})"
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn add_account(
    db: &Db,
    kek: &Kek,
    name: &str,
    provider: &str,
    secret: &str,
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_email: Option<&str>,
    monthly_cost: Option<Decimal>,
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
        account_id: None,
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
        monthly_cost,
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
    monthly_cost: Option<Decimal>,
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
            monthly_cost,
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
    warn_if_unpriced(name, monthly_cost);
    Ok(())
}

/// Say so when a seat has no price.
///
/// The saving column nets a seat's fee against what its traffic would have cost,
/// and with no fee it can only show a dash. Import is the moment the operator
/// knows the number, so it is the moment to ask — a dash discovered weeks later
/// looks like a broken report rather than an unanswered question.
fn warn_if_unpriced(name: &str, monthly_cost: Option<Decimal>) {
    if monthly_cost.is_none() {
        println!("  no monthly price set, so this seat's saving will read as unknown");
        println!("    set it with: oag admin account set-cost {name} --monthly-cost <price>");
    }
}

/// Import the signed-in Codex CLI session as an OpenAI OAuth credential.
///
/// The mirror of `import_grok` for a ChatGPT/Codex subscription: reads the
/// CLI's `auth.json` and never writes it, storing the OAuth pair (and the
/// account id Codex sends as a header) sealed in the `account` row, where
/// `ensure_fresh` rotates it.
#[allow(clippy::too_many_arguments)]
async fn import_codex(
    db: &Db,
    kek: &Kek,
    name: &str,
    auth_files: &[String],
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_email: Option<&str>,
    shared: bool,
    monthly_cost: Option<Decimal>,
) -> Result<()> {
    // Same stance as a Grok seat: sanctioned for the holder's own use, so it
    // binds to a principal by default and pooling is the explicit choice.
    if owner_email.is_none() && !shared {
        return Err(oag_core::Error::Config(
            "a subscription seat binds to one principal by default: pass \
             --owner-email <email>, or --shared to pool it deliberately"
                .to_owned(),
        ));
    }

    let paths: Vec<String> = if auth_files.is_empty() {
        codex_auth_paths()?
    } else {
        auth_files.to_vec()
    };

    // The first path that carries a usable OAuth session wins; an API-key-only
    // auth.json parses to None and is skipped.
    let mut session = None;
    let mut tried = Vec::new();
    for path in &paths {
        let Ok(json) = std::fs::read_to_string(path) else {
            continue;
        };
        tried.push(path.clone());
        if let Some(s) = oag_upstream::openai_oauth::session_from_json(&json)
            .map_err(|e| oag_core::Error::Config(format!("{path}: {e}")))?
        {
            session = Some(s);
            break;
        }
    }
    let Some(session) = session else {
        return Err(oag_core::Error::Config(format!(
            "no signed-in Codex OAuth session in {}; run `codex` and log in with ChatGPT first",
            if tried.is_empty() {
                paths.join(", ")
            } else {
                tried.join(", ")
            }
        )));
    };

    let owner_id = find_owner(db, owner_email).await?;
    let id = insert_account(
        db,
        kek,
        name,
        oag_core::Provider::OpenAI,
        "oauth",
        &session.into_material(),
        route,
        max_concurrency,
        priority,
        owner_id,
        monthly_cost,
    )
    .await?;

    println!("account {name} (openai, oauth) -> {id}");
    println!(
        "  sealed at rest, attached to route '{route}', {}",
        scope_of(owner_id)
    );
    println!("  auth.json was read, not written; the Codex CLI stays signed in");
    warn_if_unpriced(name, monthly_cost);
    Ok(())
}

/// The default places a Codex CLI session lives, in the order the CLI itself
/// resolves them: `$CODEX_HOME`, then `~/.codex`, then `~/.config/codex`.
fn codex_auth_paths() -> Result<Vec<String>> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return Ok(vec![format!("{home}/auth.json")]);
    }
    let home = std::env::var("HOME")
        .map_err(|_| oag_core::Error::Config("HOME is not set; pass --auth-file".to_owned()))?;
    Ok(vec![
        format!("{home}/.codex/auth.json"),
        format!("{home}/.config/codex/auth.json"),
    ])
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

/// Refuse a credential kind the provider does not offer.
///
/// `Provider::support` is the build's statement of what each provider takes,
/// and the schema's CHECK only knows the kinds, not the pairs. Without this an
/// `anthropic` row of kind `oauth` — a Claude subscription, which this gateway
/// must never serve with — was insertable by anything that reached here.
pub(super) fn kind_is_offered(provider: oag_core::Provider, kind: &str) -> Result<()> {
    let support = provider.support();
    let offered = oag_core::credential::CredentialKind::from_column(kind)
        .is_some_and(|k| support.credential_kinds.contains(&k));
    if offered {
        return Ok(());
    }
    let takes = support
        .credential_kinds
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Err(oag_core::Error::Config(format!(
        "{} does not take a '{kind}' credential; it takes: {takes}. See docs/compliance.md.",
        support.display_name
    )))
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
    monthly_cost: Option<Decimal>,
) -> Result<Uuid> {
    kind_is_offered(provider, kind)?;
    let sealed = kek.seal_json(material)?;
    // Denormalised so the scheduler can skip expired credentials without
    // decrypting every candidate; see the schema comment.
    let expires = material
        .expires_at
        .and_then(|e| time::OffsetDateTime::from_unix_timestamp(e).ok());

    // `account.name` carries no unique constraint, and every CLI command that
    // addresses a credential does so by name: `disable`, `enable`, `set-cost`,
    // `set-reserve`. A second credential with an existing name is therefore
    // creatable and then unaddressable — `disable` updates both or neither, and
    // nothing in the CLI can tell them apart or rename one.
    //
    // Refused here rather than by a unique index, because an index would fail
    // to build on any deployment that already has a pair, which is precisely
    // the deployment that needs the tool. `account rename` is the way out for
    // those, and this is the way in for everyone else.
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM account WHERE name = $1)")
        .bind(name)
        .fetch_one(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("checking the credential name: {e}")))?;
    if taken {
        return Err(oag_core::Error::Config(format!(
            "a credential named '{name}' already exists. Names are how every other \
             command addresses one, so two would leave both unaddressable. Pick another \
             name, or rename the existing one with `oag admin account rename --from {name} \
             --to <new>`."
        )));
    }

    let id = Uuid::now_v7();
    sqlx::query(
        r"
        INSERT INTO account (
            id, name, provider, kind, credentials_sealed, credentials_nonce,
            token_expires_at, owner_principal_id, priority, max_concurrency,
            monthly_cost_usd
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
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
    .bind(monthly_cost)
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("creating account: {e}")))?;

    // `rows_affected`, because the SELECT is the whole statement's source: a
    // route name that does not match yields no rows, the INSERT writes nothing,
    // and `execute` calls that a success. The command then printed "attached to
    // route 'prod'" over a credential joined to nothing — schedulable, listed
    // as ready, and unreachable from any route, so every request through the
    // gateway failed `no_viable_model` while the CLI insisted the credential
    // was fine.
    //
    // The account row itself is left in place rather than rolled back. It holds
    // a sealed secret the operator has just supplied and may not have kept, and
    // destroying that to tidy up a typo is the worse of the two failures — the
    // message below says exactly what is missing and the fix is one command.
    let attached = sqlx::query(
        "INSERT INTO account_route (account_id, route_id) SELECT $1, id FROM route WHERE name = $2",
    )
    .bind(id)
    .bind(route)
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("attaching account to route: {e}")))?;

    if attached.rows_affected() == 0 {
        return Err(oag_core::Error::Config(format!(
            "credential '{name}' was created but there is no route named '{route}', so it is \
             attached to nothing and no request can reach it. Create the route with \
             `oag admin init --route {route}`, then attach this credential to it — \
             re-running this command is refused, because the name is now taken by the \
             row it just made. The secret is stored and does not need supplying again."
        )));
    }

    Ok(id)
}

/// Rename a credential, which is the only way out of a duplicate pair.
///
/// Exists because `account.name` has no unique constraint and never gained one:
/// an index would fail to build on exactly the deployments that already hold a
/// pair. Renaming is what makes those addressable again.
pub(super) async fn rename_account(db: &Db, from: &str, to: &str) -> Result<()> {
    if from == to {
        return Err(oag_core::Error::Config(
            "the new name is the same as the old one".to_owned(),
        ));
    }
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM account WHERE name = $1)")
        .bind(to)
        .fetch_one(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("checking the credential name: {e}")))?;
    if taken {
        return Err(oag_core::Error::Config(format!(
            "a credential named '{to}' already exists"
        )));
    }

    // `rows_affected`, and it is allowed to be more than one: renaming is the
    // command for undoing a duplicate, so refusing to act on a pair would
    // refuse the only case it exists for. It says how many it moved, because
    // moving two when you meant one is worth knowing immediately.
    let moved = sqlx::query("UPDATE account SET name = $2, updated_at = now() WHERE name = $1")
        .bind(from)
        .bind(to)
        .execute(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("renaming credential: {e}")))?;

    match moved.rows_affected() {
        0 => Err(oag_core::Error::Config(format!(
            "no credential named '{from}'; see `oag admin account list`"
        ))),
        1 => {
            println!("renamed {from} -> {to}");
            Ok(())
        }
        n => {
            println!("renamed {n} credentials named '{from}' -> '{to}'");
            println!("  They were duplicates and are now one name again — which is still");
            println!("  ambiguous. Rename them apart one at a time, or disable the spare.");
            Ok(())
        }
    }
}

/// Pick the credential a price fetch authenticates with.
///
/// Any credential for the provider returns the same price list, so this takes
/// the first rather than making the operator name one; schedulable first,
/// because a disabled seat is usually disabled for a reason that will also stop
/// this call. There is no refresh here — the CLI has no `AppState` to hold the
/// fleet-wide lock — so a seat whose token has expired since the server last
/// touched it surfaces as a 401, and `--account` is the way past it.
pub(super) async fn price_account(
    db: &Db,
    provider: oag_core::Provider,
    name: Option<&str>,
) -> Result<oag_store::AccountRow> {
    let row: Option<oag_store::AccountRow> = sqlx::query_as(
        r"
        SELECT id, name, provider, kind, credentials_sealed, credentials_nonce,
               token_version, token_expires_at, owner_principal_id, proxy_url,
               priority, max_concurrency, schedulable, cooldown_until,
               rate_limited_until, window_resets_at, last_used_at
        FROM account
        WHERE provider = $1 AND ($2::text IS NULL OR name = $2)
        ORDER BY schedulable DESC, priority DESC, name
        LIMIT 1
        ",
    )
    .bind(provider.as_str())
    .bind(name)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("finding a {provider} credential: {e}")))?;

    row.ok_or_else(|| {
        oag_core::Error::Config(match name {
            Some(n) => format!("no {provider} credential named {n}"),
            None => format!(
                "no {provider} credential; add one with `oag admin account add --provider {provider}`"
            ),
        })
    })
}
