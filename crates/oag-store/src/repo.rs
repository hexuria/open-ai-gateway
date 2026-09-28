//! Queries.

use crate::Db;
use crate::rows::{
    AccountRow, AuthContext, ChannelStatusRow, ModelRow, RouteRow, ServiceRow, Spend, UsageWrite,
};
use oag_core::{AccountId, Error, Result};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

/// Hash an inbound key for lookup.
///
/// The key is never stored in the clear, so this is also the only way to find
/// one. Plaintext keys matched on column equality would turn read access to
/// one table into every client's credential.
/// What every key this gateway has ever issued looks like: the prefix, then
/// 32 bytes of entropy as lowercase hex. See `mint_key`.
pub const KEY_PREFIX: &str = "oag_live_";
/// The stored, loggable prefix of an issued key: `KEY_PREFIX` plus seven hex.
///
/// Sixteen characters, the same slice `mint_key` records in `api_key.key_prefix`,
/// so a prefix in a log lines up with a prefix in the database and an operator
/// can join the two without ever handling a value.
///
/// **Only ever call this on a string that has passed [`is_issued_key_shape`].**
/// The point of the prefix is that it identifies one of *our* keys; the first
/// sixteen characters of an arbitrary bearer token are the first sixteen
/// characters of somebody else's secret, and a caller who pastes an Anthropic
/// key into the wrong client must not have a third of it written to a log.
#[must_use]
pub fn loggable_key_prefix(raw: &str) -> String {
    raw.chars().take(16).collect()
}

/// `KEY_PREFIX` plus 64 hex digits.
pub const KEY_LEN: usize = 9 + 64;

/// Whether `raw` has the shape of a key `mint_key` could have produced.
///
/// A syntactic check, and the *only* thing that runs before the database on
/// the inference surface. Every issued key has exactly this shape, so a
/// string without it is not an unknown key — it is not a key — and refusing
/// it here costs nothing. Without this, a flood of arbitrary strings in the
/// `Authorization` header bought a Redis GET and a Postgres probe apiece from
/// anyone who could reach the port, ahead of any rate limit: the one lookup
/// on the request path that no valid credential was needed to trigger.
///
/// What this is not: a negative cache. A random key per request misses a
/// negative cache exactly as it misses the positive one, and a fleet-wide
/// "this key does not exist" entry would lock a freshly minted key out for
/// its TTL. Shape is the cheap filter; a permit around the lookup itself
/// (`AuthCache`) is the bound on what gets past it.
#[must_use]
pub fn is_issued_key_shape(raw: &str) -> bool {
    raw.len() == KEY_LEN
        && raw.starts_with(KEY_PREFIX)
        && raw[KEY_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[must_use]
pub fn hash_key(raw: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Look up an inbound key and everything the request path needs from it.
///
/// One query rather than three. Auth is on the hot path of every request, and
/// the difference between one round trip and three is the difference between a
/// cache miss costing 1ms and 3ms.
pub async fn authenticate(db: &Db, raw_key: &str) -> Result<Option<AuthContext>> {
    let hash = hash_key(raw_key);
    let now = OffsetDateTime::now_utc();

    // Identity and limits only. Spend is not here, on purpose: this row is
    // what the auth cache holds for minutes, and a spend figure cached for
    // minutes is a cap that N concurrent requests all pass together. Spend is
    // read fresh by `spend_for`, per request, from the columns `record_usage`
    // maintains. (This query used to SUM the principal's month from the
    // ledger on every cache miss, and then cache the answer — the worst of
    // both: a scan, and stale.)
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            Uuid,
            Option<String>,
            Option<Decimal>,
            Option<Decimal>,
            Decimal,
            bool,
            Option<OffsetDateTime>,
        ),
    >(
        r"
        SELECT k.id, k.principal_id, k.route_id, k.floor_tier,
               k.quota_usd,
               p.monthly_budget_usd, p.hard_stop_multiple,
               k.admin, k.expires_at
        FROM api_key k
        JOIN principal p ON p.id = k.principal_id
        JOIN route    r ON r.id = k.route_id
        WHERE k.key_hash = $1
          AND k.active AND p.active AND r.active
          AND (k.expires_at IS NULL OR k.expires_at > $2)
        ",
    )
    .bind(&hash)
    .bind(now)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("authenticating: {e}")))?;

    Ok(row.map(|r| AuthContext {
        api_key_id: r.0,
        principal_id: r.1,
        route_id: r.2,
        key_floor_tier: r.3,
        quota_usd: r.4,
        principal_budget_usd: r.5,
        principal_hard_stop_multiple: r.6,
        admin: r.7,
        expires_at: r.8,
    }))
}

/// The caller's spend, fresh.
///
/// One primary-key read on each of two rows, never a SUM: `record_usage`
/// maintains `api_key.spent_usd` (lifetime) and `principal.spent_usd` (the
/// month named by `spent_month`) in the same statement as the ledger insert,
/// so this is exactly as current as the ledger is. A month that has rolled
/// over reads as zero until the first write of the new month resets the row.
///
/// `Err(Unauthenticated)` rather than zeros when the key is gone: a key
/// deleted between authentication and here must not spend as if uncapped for
/// the rest of the cache window.
pub async fn spend_for(db: &Db, api_key_id: Uuid, principal_id: Uuid) -> Result<Spend> {
    let row = sqlx::query_as::<_, (Decimal, Decimal)>(
        r"
        SELECT k.spent_usd,
               CASE WHEN p.spent_month = date_trunc('month', now())::date
                    THEN p.spent_usd ELSE 0 END
          FROM api_key k
          JOIN principal p ON p.id = $2
         WHERE k.id = $1
        ",
    )
    .bind(api_key_id)
    .bind(principal_id)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("reading spend: {e}")))?;

    let (key_usd, principal_usd) = row.ok_or(Error::Unauthenticated)?;
    Ok(Spend {
        key_usd,
        principal_usd,
    })
}

/// What one pass of [`reconcile_monthly_spend`] rewrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub principals: u64,
    pub routes: u64,
}

/// Bring every budgeted principal's and route's monthly counter back into
/// agreement with the ledger.
///
/// `record_usage` maintains the counters in the same statement as the ledger
/// insert, so in steady state there is nothing to do. The gap is the
/// rolling-deploy window: the release that introduced the counters backfilled
/// them once, and the previous release keeps writing ledger rows without
/// touching them for as long as its replicas serve — half an hour by default
/// on every platform — so the spend of that window was invisible to the cap
/// for the rest of the month. This closes it, and closes any drift with the
/// same cause after it.
///
/// One transaction per row, and the row is locked BEFORE the sum is taken.
/// The obvious single statement, `UPDATE ... SET spent_usd = (SELECT SUM ...)`,
/// loses a concurrent debit: when it waits on the row lock the debit holds and
/// then re-evaluates, Postgres re-checks the WHERE clause against the new row
/// version but does not re-run the subquery, so the sum predates the debit and
/// overwrites it. With the lock taken first, a concurrent `record_usage` waits
/// on it, and its ledger row and its debit land together after the sum — which
/// then adds to the reconciled value rather than being lost from it.
///
/// Budgeted rows only: they are the only ones enforced, and every other row's
/// counter is a number nothing reads. The month only moves forward, as in the
/// debit: a row already stamped with a later month is left alone.
///
/// Imported ledger rows carry no principal or route, so a sum keyed on either
/// excludes them without asking, exactly as the backfill did.
pub async fn reconcile_monthly_spend(db: &Db) -> Result<Reconciled> {
    let principals = reconcile_rows(
        db,
        "SELECT id FROM principal WHERE monthly_budget_usd IS NOT NULL",
        "SELECT 1 FROM principal WHERE id = $1 FOR UPDATE",
        r"
        UPDATE principal p
           SET spent_usd = COALESCE((
                   SELECT SUM(u.cost_usd) FROM usage_event u
                    WHERE u.principal_id = p.id
                      AND u.occurred_at >= date_trunc('month', now())
               ), 0),
               spent_month = date_trunc('month', now())::date
         WHERE p.id = $1
           AND (p.spent_month IS NULL OR p.spent_month <= date_trunc('month', now())::date)
        ",
    )
    .await?;
    let routes = reconcile_rows(
        db,
        "SELECT id FROM route WHERE monthly_budget_usd IS NOT NULL",
        "SELECT 1 FROM route WHERE id = $1 FOR UPDATE",
        r"
        UPDATE route r
           SET spent_usd = COALESCE((
                   SELECT SUM(u.cost_usd) FROM usage_event u
                    WHERE u.route_id = r.id
                      AND u.occurred_at >= date_trunc('month', now())
               ), 0),
               spent_month = date_trunc('month', now())::date
         WHERE r.id = $1
           AND (r.spent_month IS NULL OR r.spent_month <= date_trunc('month', now())::date)
        ",
    )
    .await?;
    Ok(Reconciled { principals, routes })
}

/// The per-row half of [`reconcile_monthly_spend`]: list, then lock and
/// rewrite each in its own transaction.
///
/// Static SQL, handed in: the two tables differ only in name, and sqlx will
/// not take a table name as a parameter.
async fn reconcile_rows(
    db: &Db,
    list: &'static str,
    lock: &'static str,
    rewrite: &'static str,
) -> Result<u64> {
    let ids = sqlx::query_scalar::<_, Uuid>(list)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("listing budgeted rows: {e}")))?;

    let mut rewritten = 0u64;
    for id in ids {
        let mut tx = db
            .pool()
            .begin()
            .await
            .map_err(|e| Error::Internal(format!("starting reconcile: {e}")))?;
        // A row deleted since the listing is nothing to reconcile.
        let held = sqlx::query_scalar::<_, i32>(lock)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| Error::Internal(format!("locking row for reconcile: {e}")))?;
        if held.is_none() {
            continue;
        }
        let done = sqlx::query(rewrite)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::Internal(format!("reconciling monthly spend: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| Error::Internal(format!("committing reconcile: {e}")))?;
        rewritten += done.rows_affected();
    }
    Ok(rewritten)
}

pub async fn route_by_id(db: &Db, id: Uuid) -> Result<Option<RouteRow>> {
    sqlx::query_as::<_, RouteRow>(
        // One primary-key read. This used to SUM the route's month from the
        // ledger whenever the route had a budget — on every inference request,
        // every /v1/models call and every count_tokens call, uncached, over a
        // range that grew all month. Setting `monthly_budget_usd`, an ordinary
        // documented control, changed the asymptotic cost of the request path.
        // `record_usage` now maintains the column; the CASE reads it as zero
        // once the month it names has passed.
        "SELECT id, name, tiers, default_mode, floor_tier, rpm_limit, monthly_budget_usd, active,
                CASE WHEN spent_month = date_trunc('month', now())::date
                     THEN spent_usd ELSE 0 END AS spent_usd
         FROM route WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading route: {e}")))
}

/// Credentials a route may draw on for one provider.
///
/// Personal credentials are filtered here rather than in the scheduler: a
/// credential bound to someone else must never appear in another principal's
/// candidate set, and enforcing that in SQL means it cannot be forgotten by a
/// later change to selection policy.
pub async fn candidates(
    db: &Db,
    route_id: Uuid,
    provider: &str,
    principal_id: Uuid,
) -> Result<Vec<AccountRow>> {
    sqlx::query_as::<_, AccountRow>(
        r"
        SELECT a.id, a.name, a.provider, a.kind,
               a.credentials_sealed, a.credentials_nonce, a.token_version, a.token_expires_at,
               a.owner_principal_id, a.proxy_url, a.priority, a.max_concurrency,
               a.schedulable, a.cooldown_until, a.rate_limited_until, a.window_resets_at,
               a.usage_remaining_pct, a.usage_reserve_pct,
               a.last_used_at
        FROM account a
        JOIN account_route ar ON ar.account_id = a.id
        WHERE ar.route_id = $1
          AND a.provider = $2
          AND (a.owner_principal_id IS NULL OR a.owner_principal_id = $3)
        ",
    )
    .bind(route_id)
    .bind(provider)
    .bind(principal_id)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading candidates: {e}")))
}

pub async fn account_by_id(db: &Db, id: AccountId) -> Result<Option<AccountRow>> {
    sqlx::query_as::<_, AccountRow>(
        r"
        SELECT id, name, provider, kind, credentials_sealed, credentials_nonce,
               token_version, token_expires_at, owner_principal_id, proxy_url,
               priority, max_concurrency, schedulable, cooldown_until,
               rate_limited_until, window_resets_at,
               usage_remaining_pct, usage_reserve_pct, last_used_at
        FROM account WHERE id = $1
        ",
    )
    .bind(id.as_uuid())
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading account: {e}")))
}

/// Identity and display name for every credential, for the slot sweep.
///
/// The sweep publishes `oag_slots_in_use` from Redis, including zero, so a
/// gauge that last observed a full seat does not stay full after the key is
/// gone. Names, not ids, because the gauge is labelled by `account.name`.
pub async fn account_slot_labels(db: &Db) -> Result<Vec<(AccountId, String)>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, name FROM account")
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("listing accounts for slot sweep: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| (AccountId::from_uuid(id), name))
        .collect())
}

/// Providers this route holds usable credentials for, and by which credential
/// kind, for one principal.
///
/// The kind rides along rather than being a second query because the listing
/// needs both answers about the same instant: it offers `<model>@sub` only
/// where a subscription is actually reachable, and two queries could disagree
/// about that across a credential being disabled between them.
///
/// Mirrors the personal-credential predicate in `candidates`: a credential
/// bound to another principal must never appear in this principal's view. Adds
/// `a.schedulable`, which `candidates` leaves to the scheduler — correct here
/// because a disabled credential is an operator decision, not a transient
/// state, and advertising a model nobody can reach is worse than omitting it.
///
/// Access-token `token_expires_at` is not a filter: that is the OAuth access
/// token TTL, refreshed on the request path, not the subscription. Hiding on
/// it would empty a picker every time a fifteen-minute token lapsed between
/// polls. Subscription expiry is `usage_remaining_pct`.
pub async fn route_channels(
    db: &Db,
    route_id: Uuid,
    principal_id: Uuid,
) -> Result<Vec<(String, String, Option<Vec<String>>)>> {
    // `served_models` rides along because it is a property of the same
    // credential row and the listing needs both together: which channels exist,
    // and what each one will actually accept. A NULL here is "never asked", and
    // the caller must treat it as unknown rather than as empty.
    sqlx::query_as::<_, (String, String, Option<Vec<String>>)>(
        r"
        SELECT DISTINCT a.provider, a.kind, a.served_models
        FROM account a
        JOIN account_route ar ON ar.account_id = a.id
        WHERE ar.route_id = $1
          AND a.schedulable
          AND (a.owner_principal_id IS NULL OR a.owner_principal_id = $2)
          -- Exhausted for a while, not merely busy. A seat whose weekly pool is
          -- spent cannot serve a request today, so offering its models lists
          -- something that is certain to fail. The breaker's own cooldown is
          -- deliberately NOT consulted: it lasts seconds, while a client can
          -- cache this list for far longer, so hiding a model mid-blip removes
          -- it until the client next refreshes -- worse than briefly offering
          -- one that fails over to another credential anyway.
          AND (a.rate_limited_until IS NULL OR a.rate_limited_until <= now())
          -- A spent subscription cannot serve, even when no reserve was set.
          -- COALESCE(reserve, 0) makes an unset reserve a floor of zero:
          -- remaining 50 lists, remaining 0 does not. NULL remaining stays
          -- listed — unknown is not empty, and a provider with no usage API
          -- must not vanish from the catalogue for want of a reading.
          AND (a.usage_remaining_pct IS NULL
               OR a.usage_remaining_pct > COALESCE(a.usage_reserve_pct, 0))
        ",
    )
    .bind(route_id)
    .bind(principal_id)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading route providers: {e}")))
}

/// Record what a credential told us it serves.
///
/// Written by the discovery sweep, never by hand. Storing the timestamp
/// alongside means a stale answer is visible as stale rather than merely old:
/// an operator debugging a missing model wants to know whether we ever asked.
pub async fn set_served_models(db: &Db, account: Uuid, models: &[String]) -> Result<()> {
    sqlx::query(
        "UPDATE account SET served_models = $2, served_models_at = now(), \
         updated_at = now() WHERE id = $1",
    )
    .bind(account)
    .bind(models)
    .execute(db.pool())
    .await
    .map(|_| ())
    .map_err(|e| Error::Internal(format!("recording served models: {e}")))
}

/// Every credential this principal may draw on for this route, including ones
/// that cannot serve right now.
///
/// [`route_channels`] is the picker: it hides a spent, reserved, rate-limited,
/// or disabled seat so `/v1/models` `data` does not advertise a 503. This is
/// the status panel: the same owner predicate, none of those serving filters,
/// so a caller whose picker is empty can still learn *why*. Name and sealed
/// material stay off the SELECT — an inference key is not an inventory dump.
pub async fn route_channel_status(
    db: &Db,
    route_id: Uuid,
    principal_id: Uuid,
) -> Result<Vec<ChannelStatusRow>> {
    sqlx::query_as::<_, ChannelStatusRow>(
        r"
        SELECT a.provider, a.kind, a.schedulable,
               a.rate_limited_until, a.window_resets_at,
               a.usage_remaining_pct, a.usage_reserve_pct
        FROM account a
        JOIN account_route ar ON ar.account_id = a.id
        WHERE ar.route_id = $1
          AND (a.owner_principal_id IS NULL OR a.owner_principal_id = $2)
        ",
    )
    .bind(route_id)
    .bind(principal_id)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading route credential status: {e}")))
}

/// Take a credential out of rotation, or put it back. Returns its name, or
/// `None` if no such credential — which is the caller's 404.
pub async fn set_schedulable(db: &Db, id: AccountId, value: bool) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>(
        "UPDATE account SET schedulable = $2, updated_at = now() WHERE id = $1 RETURNING name",
    )
    .bind(id.as_uuid())
    .bind(value)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("setting schedulable: {e}")))
}

/// Clear an operator-visible cooldown.
///
/// Deliberately not `rate_limited_until`: that one is the provider's own
/// `Retry-After`, and discarding it fleet-wide turns a throttle into an
/// account action. Deliberately not `window_resets_at` either — eligibility
/// gates on `schedulable`, `cooldown_until` and `rate_limited_until` only.
pub async fn clear_cooldown(db: &Db, id: AccountId) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>(
        r"UPDATE account
             SET cooldown_until = NULL, cooldown_reason = NULL, updated_at = now()
           WHERE id = $1 RETURNING name",
    )
    .bind(id.as_uuid())
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("clearing cooldown: {e}")))
}

/// Revoke an inbound key. Returns `(key_hash, name, key_prefix)`.
///
/// The hash comes back because the caller must evict the auth cache and never
/// holds the plaintext. It must not be put in a response body.
pub async fn revoke_key(db: &Db, id: Uuid) -> Result<Option<(String, String, String)>> {
    sqlx::query_as::<_, (String, String, String)>(
        "UPDATE api_key SET active = false WHERE id = $1 RETURNING key_hash, name, key_prefix",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("revoking key: {e}")))
}

/// A freshly minted key. `key` is the plaintext, and this is the only time it
/// exists anywhere — it is hashed on the way into the row.
#[derive(Debug, Clone)]
pub struct MintedKey {
    pub id: Uuid,
    pub prefix: String,
    pub key: String,
}

/// One principal's spend and its budget — the rollup a partner service shows for
/// the org bound to this principal.
#[derive(Debug, Clone)]
pub struct PrincipalUsage {
    pub principal_id: Uuid,
    pub email: String,
    pub monthly_budget_usd: Option<Decimal>,
    /// Month-to-date, from the first of the current UTC month.
    pub month_to_date_usd: Decimal,
    pub requests: i64,
}

/// Create or update a principal, returning its id.
///
/// The identity-integration surface: a partner service (`OpenGrok`) binds each of
/// its orgs to one principal, so the org's budget and usage rollup are this
/// row's. Idempotent on `email`, which is the only stable handle a caller that
/// stores no gateway ids can use.
///
/// `budget` is `COALESCE`d rather than overwritten so an upsert-before-mint
/// cannot silently erase a budget an operator set at the CLI; clearing one is
/// [`set_principal_budget`]'s job, where it is the caller's stated intent.
///
/// **`role` IS NOT UPDATED ON CONFLICT, and that is the point.** This path can only
/// ever ask for `member`, so updating the role would mean an upsert against an
/// existing admin's email SILENTLY DEMOTES them — and since the admin gate wants
/// both an admin key and an admin principal, that locks a human operator out of
/// the admin API without touching their key. An idempotent bind must not be able
/// to remove authority. Granting or changing a role stays the CLI's job (the CLI
/// keeps its own upsert, which does write the role, because promoting the first
/// admin is exactly what `oag admin init` is for).
pub async fn upsert_principal(
    db: &Db,
    email: &str,
    role: &str,
    budget: Option<Decimal>,
) -> Result<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        r"
        INSERT INTO principal (id, email, role, monthly_budget_usd)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (email) DO UPDATE SET
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
    .map_err(|e| Error::Internal(format!("upserting principal: {e}")))
}

/// Mint an inbound key on an existing principal and route. Returns the plaintext
/// ONCE — it is hashed on the way in and is not recoverable afterwards.
///
/// `None` means the principal or route does not exist, so a caller naming either
/// wrongly is told rather than silently given nothing.
///
/// `quota_usd` is the per-key spend cap (the per-member cap in the identity
/// integration); `None` leaves the key uncapped and bounded only by the
/// principal's monthly budget.
pub async fn mint_key(
    db: &Db,
    principal_email: &str,
    route: &str,
    name: &str,
    quota_usd: Option<Decimal>,
) -> Result<Option<MintedKey>> {
    use std::fmt::Write as _;

    // 32 bytes of entropy. The prefix exists so a leaked key is recognisable in
    // a log and greppable during an incident.
    let mut raw = [0u8; 32];
    // The thread-local CSPRNG, seeded from the OS.
    rand::fill(&mut raw);
    let key = format!(
        "{KEY_PREFIX}{}",
        raw.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    );
    let hash = hash_key(&key);
    let prefix = loggable_key_prefix(&key);

    // Never `admin`: a key minted over HTTP must not be able to mint more keys.
    // Admin authority is the CLI's to grant (`oag admin key create --admin`).
    let id = sqlx::query_scalar::<_, Uuid>(
        r"
        INSERT INTO api_key
            (id, key_hash, key_prefix, name, principal_id, route_id, quota_usd, admin)
        SELECT $1, $2, $3, $4, p.id, r.id, $7, false
        FROM principal p, route r
        WHERE p.email = $5 AND r.name = $6
        RETURNING id
        ",
    )
    .bind(Uuid::now_v7())
    .bind(&hash)
    .bind(&prefix)
    .bind(name)
    .bind(principal_email)
    .bind(route)
    .bind(quota_usd)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("minting key: {e}")))?;

    Ok(id.map(|id| MintedKey { id, prefix, key }))
}

/// Set (or clear, with `None`) a principal's monthly budget. `None` return means
/// no principal with that email.
pub async fn set_principal_budget(
    db: &Db,
    email: &str,
    budget: Option<Decimal>,
) -> Result<Option<Uuid>> {
    sqlx::query_scalar::<_, Uuid>(
        "UPDATE principal SET monthly_budget_usd = $2, updated_at = now()
         WHERE email = $1 RETURNING id",
    )
    .bind(email)
    .bind(budget)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("setting principal budget: {e}")))
}

/// Set (or clear) one key's spend cap. Returns `(name, key_prefix, key_hash)`;
/// `None` means no key with that id.
///
/// The hash is returned so the caller can evict the key's cached identity:
/// the cap lives in `AuthContext`, which every tier holds for minutes, and a
/// lowered cap that nothing invalidates is not enforced until the entry
/// happens to expire — while the 200 the operator got asserted the new value.
pub async fn set_key_quota(
    db: &Db,
    id: Uuid,
    quota_usd: Option<Decimal>,
) -> Result<Option<(String, String, String)>> {
    sqlx::query_as::<_, (String, String, String)>(
        "UPDATE api_key SET quota_usd = $2 WHERE id = $1 RETURNING name, key_prefix, key_hash",
    )
    .bind(id)
    .bind(quota_usd)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("setting key quota: {e}")))
}

/// Every key hash a principal owns, for evicting them all after a write to
/// the principal's own limits — which every one of those keys carries in its
/// cached identity.
pub async fn key_hashes_for_principal(db: &Db, principal_id: Uuid) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>("SELECT key_hash FROM api_key WHERE principal_id = $1")
        .bind(principal_id)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("listing a principal's keys: {e}")))
}

/// The statement [`principal_usage`] runs.
///
/// Lifted out so a test can `EXPLAIN` the statement that runs rather than a
/// copy of it, exactly as `KEY_USAGE_SQL` was. A copy in a test drifts from its
/// original silently, and the drift is invisible precisely when it matters —
/// which is the defect class this whole round is about.
const PRINCIPAL_USAGE_SQL: &str = r"
        SELECT p.id,
               p.email,
               p.monthly_budget_usd,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
               ), 0)::numeric(16,8),
               COUNT(u.request_id) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
                     AND u.selection_reason NOT IN ('abandoned', 'lost')
               )
        FROM principal p
        -- The window bound belongs in the JOIN, not only inside the FILTERs.
        -- Every figure below is month-to-date, but with the bound stated only
        -- in the FILTERs the join still read every row this principal has ever
        -- written, aggregating a whole history to report one month of it. On a
        -- ledger of any age that is a sequential scan per admin request.
        --
        -- `ON` rather than `WHERE`, because this is a LEFT JOIN: in `WHERE` it
        -- would drop principals who have spent nothing this month, which is
        -- the row a budget check most needs to see.
        LEFT JOIN usage_event u
               ON u.principal_id = p.id
              AND u.occurred_at >= date_trunc('month', now())
        WHERE p.email = $1
        GROUP BY p.id, p.email, p.monthly_budget_usd
";

/// A principal's budget and month-to-date spend. `None` means no such principal.
///
/// Month-to-date is computed from the ledger rather than a running counter: the
/// ledger is the record, and a counter that drifts from it is a bill nobody can
/// reconcile.
///
/// The spend sums every row and the request count does not, which is the split
/// 0014 made necessary rather than an inconsistency. Since the ledger's key
/// contracted onto `(request_id, attempt)`, one client request can leave
/// several rows: the answer it was served, plus any attempt a quality gate
/// abandoned or a stream lost. All of them were generated and invoiced, so all
/// of them are money; only one of them was a request. Counting the attempts
/// would report a principal making twice the requests they made on exactly the
/// traffic where escalation is working.
pub async fn principal_usage(db: &Db, email: &str) -> Result<Option<PrincipalUsage>> {
    sqlx::query_as::<_, (Uuid, String, Option<Decimal>, Decimal, i64)>(PRINCIPAL_USAGE_SQL)
        .bind(email)
        .fetch_optional(db.pool())
        .await
        .map(|row| {
            row.map(
                |(principal_id, email, monthly_budget_usd, month_to_date_usd, requests)| {
                    PrincipalUsage {
                        principal_id,
                        email,
                        monthly_budget_usd,
                        month_to_date_usd,
                        requests,
                    }
                },
            )
        })
        .map_err(|e| Error::Internal(format!("reading principal usage: {e}")))
}

/// One key's cap and spend — what a partner service shows next to the member (or the
/// coworker) that holds the key, and what it evaluates a per-key limit against.
///
/// Four spend figures, on purpose. `spent_usd` is the counter the gateway's own quota check
/// runs against: lifetime, denormalised on `api_key`, debited by `record_usage` in the same
/// statement as the ledger row. The windows are the ledger summed since an instant —
/// a rolling five hours, a rolling twenty-four hours, a rolling seven days, the first of the
/// current UTC month — the shape
/// of a subscription's limits, which is what a partner service writes its rules in. A cap on
/// the key is a wall on the first number; a service that showed a window figure as if it were
/// what that cap measures would be lying about when the wall is reached, so all four are given.
///
/// A rolling window has no boundary: its "resets at" is the moment the OLDEST spend still
/// inside it ages out — the earliest instant the figure drops at all — which is `oldest +
/// window`, handed back as `frees_at` (`None` when the window is empty). The month resets on the
/// first of the next month.
#[derive(Debug, Clone)]
pub struct KeyUsage {
    pub key_id: Uuid,
    pub name: String,
    pub prefix: String,
    pub principal_email: String,
    pub active: bool,
    pub quota_usd: Option<Decimal>,
    /// Lifetime, and what `quota_usd` is enforced against.
    pub spent_usd: Decimal,
    /// From the first of the current UTC month, out of the ledger.
    pub month_to_date_usd: Decimal,
    /// Requests this month.
    pub requests: i64,
    pub month_resets_at: OffsetDateTime,
    pub five_hour_usd: Decimal,
    pub five_hour_frees_at: Option<OffsetDateTime>,
    pub seven_day_usd: Decimal,
    pub seven_day_frees_at: Option<OffsetDateTime>,
    /// Requests inside the rolling windows; the month's are `requests`.
    pub five_hour_requests: i64,
    pub seven_day_requests: i64,
    /// What the same tokens would have cost at the model's own list API price
    /// (`counterfactual_api_usd`): for a subscription seat, the pay-per-token bill it displaced —
    /// the figure a seat's usage is shown against, since its `cost_usd` is truthfully zero; for
    /// a metered credential it equals the cost. NOT the top-rung `counterfactual_usd`, which is
    /// the routing story, not the seat's.
    pub month_counterfactual_usd: Decimal,
    pub five_hour_counterfactual_usd: Decimal,
    pub seven_day_counterfactual_usd: Decimal,
    /// The rolling day — the optional daily brake a coworker's owner may set.
    pub day_usd: Decimal,
    pub day_frees_at: Option<OffsetDateTime>,
    pub day_requests: i64,
    pub day_counterfactual_usd: Decimal,
    /// Points per window: each request's list-price cost over the reference price, rounded
    /// half up per request and summed. `None` while no reference price is set.
    pub month_points: Option<i64>,
    pub five_hour_points: Option<i64>,
    pub day_points: Option<i64>,
    pub seven_day_points: Option<i64>,
}

/// The rolling windows a partner service meters a key over, plus the calendar month. Rolling
/// windows are measured back from now; the month is the UTC month.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindow {
    FiveHours,
    Day,
    SevenDays,
    Month,
}

impl UsageWindow {
    /// The wire spelling, the one the partner service and the desktop use.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "5h" => Some(Self::FiveHours),
            "24h" => Some(Self::Day),
            "7d" => Some(Self::SevenDays),
            "month" => Some(Self::Month),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveHours => "5h",
            Self::Day => "24h",
            Self::SevenDays => "7d",
            Self::Month => "month",
        }
    }

    /// The length of a rolling window; the month has none.
    pub fn length(self) -> Option<time::Duration> {
        match self {
            Self::FiveHours => Some(time::Duration::hours(5)),
            Self::Day => Some(time::Duration::hours(24)),
            Self::SevenDays => Some(time::Duration::days(7)),
            Self::Month => None,
        }
    }

    /// The instant the window starts at, as of `now`.
    pub fn since(self, now: OffsetDateTime) -> OffsetDateTime {
        match self.length() {
            Some(length) => now - length,
            None => first_of_month(now),
        }
    }

    /// When the window next frees up: a rolling window's oldest spend ageing out (none when
    /// it is empty); the month's reset.
    pub fn frees_at(
        self,
        oldest: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> Option<OffsetDateTime> {
        match self.length() {
            Some(length) => oldest.map(|oldest| oldest + length),
            None => Some(first_of_next_month(now)),
        }
    }
}

fn first_of_month(now: OffsetDateTime) -> OffsetDateTime {
    let now = now.to_offset(time::UtcOffset::UTC);
    now.replace_day(1)
        .and_then(|d| d.replace_time(time::Time::MIDNIGHT).replace_nanosecond(0))
        .unwrap_or(now)
}

fn first_of_next_month(now: OffsetDateTime) -> OffsetDateTime {
    let start = first_of_month(now);
    let (year, month) = if start.month() == time::Month::December {
        (start.year() + 1, time::Month::January)
    } else {
        (start.year(), start.month().next())
    };
    start
        .replace_year(year)
        .and_then(|d| d.replace_month(month))
        .unwrap_or(start)
}

/// One model's share of a key's usage inside a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsage {
    pub model_id: String,
    /// The ladder rung this row's requests were routed through, or `None` when
    /// the caller pinned the model directly and no rung was involved.
    ///
    /// A rung is a property of a *request*, not of a model: the same model
    /// reached as `cheap` and hard-pinned by name are the same `model_id` and
    /// genuinely different things. So this is part of the grouping key, and a
    /// model used both ways is two rows rather than one row that averages them.
    pub tier: Option<String>,
    /// Every attempt, served or not — always `>= requests`.
    ///
    /// `requests` excludes `abandoned` and `lost`, so the difference is what
    /// was tried and not served. It cannot be recovered from the money: over a
    /// window with failures `list_usd` can sit *below* `cost_usd`, because the
    /// gateway paid for tokens that displaced no API bill. Two questions, two
    /// fields.
    pub attempts: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: Decimal,
    /// What the same tokens would have cost at the model's list API price.
    pub list_usd: Decimal,
    /// `None` while no reference price is set.
    pub points: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct ModelUsageRow {
    model_id: String,
    /// `NOT NULL` in the schema: a direct pin stores the empty string, which
    /// [`key_usage_by_model`] maps to `None` rather than passing on a rung
    /// whose name is "".
    tier: String,
    attempts: i64,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: Decimal,
    list_usd: Decimal,
    points: Option<i64>,
}

/// Whether an id names a key at all — an empty per-model report needs to know.
pub async fn key_exists(db: &Db, id: Uuid) -> Result<bool> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM api_key WHERE id = $1)")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("looking up a key: {e}")))
}

/// A key's usage inside a window, per model: requests, tokens by class, cost, list price and
/// points (rounded half up per request, summed as integers; `None` without a reference).
pub async fn key_usage_by_model(
    db: &Db,
    id: Uuid,
    window: UsageWindow,
    reference: Option<Decimal>,
    now: OffsetDateTime,
) -> Result<Vec<ModelUsage>> {
    sqlx::query_as::<_, ModelUsageRow>(
        r"
        SELECT model_id,
               tier,
               COUNT(*) AS attempts,
               COUNT(*) FILTER (
                   WHERE selection_reason NOT IN ('abandoned', 'lost')
               ) AS requests,
               COALESCE(SUM(input_tokens), 0)::bigint AS input_tokens,
               COALESCE(SUM(output_tokens), 0)::bigint AS output_tokens,
               COALESCE(SUM(cache_read_tokens), 0)::bigint AS cache_read_tokens,
               COALESCE(SUM(cache_write_tokens), 0)::bigint AS cache_write_tokens,
               COALESCE(SUM(cost_usd), 0)::numeric(16,8) AS cost_usd,
               -- `list_usd` and `points` read `counterfactual_api_usd`, which is
               -- zero on an abandoned or lost attempt while `cost_usd` is not.
               -- So over a window with unserved attempts `list_usd` can sit
               -- below `cost_usd` on a metered key: the gateway paid for tokens
               -- the member is not charged points for. Deliberate; see the note
               -- at `meter.rs` where the column is written.
               COALESCE(SUM(counterfactual_api_usd), 0)::numeric(16,8) AS list_usd,
               CASE WHEN $3::numeric IS NULL THEN NULL
                    ELSE SUM(ROUND(counterfactual_api_usd * 1000000 / $3::numeric))::bigint
               END AS points
        FROM usage_event
        WHERE api_key_id = $1 AND occurred_at >= $2
        GROUP BY model_id, tier
        ORDER BY list_usd DESC, model_id, tier
        ",
    )
    .bind(id)
    .bind(window.since(now))
    .bind(reference)
    .fetch_all(db.pool())
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|row| ModelUsage {
                model_id: row.model_id,
                // The empty string is how "no rung" is spelled in a NOT NULL
                // column; JSON has a null for exactly this and should use it.
                tier: (!row.tier.is_empty()).then_some(row.tier),
                attempts: row.attempts,
                requests: row.requests,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                cache_read_tokens: row.cache_read_tokens,
                cache_write_tokens: row.cache_write_tokens,
                cost_usd: row.cost_usd,
                list_usd: row.list_usd,
                points: row.points,
            })
            .collect()
    })
    .map_err(|e| Error::Internal(format!("reading a key's usage by model: {e}")))
}

/// Points spent inside a window by each of several keys — one query, the partner service's
/// pool read (a member's pool is the sum over that member's coworker keys). Keys with no rows
/// are absent; the caller says 0 for them.
/// A principal's id from its email, for the routes that address it that way.
///
/// `None` means no such principal — distinct from a principal with no ledger
/// rows, which is a real zero and must not be reported as a missing one.
pub async fn principal_id_for_email(db: &Db, email: &str) -> Result<Option<Uuid>> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM principal WHERE email = $1")
        .bind(email)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("looking up a principal: {e}")))
}

/// One principal's points and its unbilled spend inside a window.
///
/// Every coworker key an organisation mints sits on one principal, so the
/// organisation's spend is this — **one indexed aggregate**, not a sum over an
/// enumerated key list. `usage_event_principal_idx (principal_id,
/// occurred_at DESC)` serves it exactly, and none of `points_for_keys`'
/// per-key ceiling applies, because there is no list to cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalPoints {
    /// `None` while no reference price is set — never zero, which is a spend.
    pub points: Option<i64>,
    /// What we paid for attempts nobody was served: the leak.
    ///
    /// Real money on a metered credential and **zero on a subscription seat by
    /// construction**, since `cost_usd` is zero there whatever the fate. That
    /// is correct rather than a gap: the leak is only a bill where there is a
    /// bill.
    pub unbilled_cost_usd: Decimal,
    /// Everything we paid in the window, served or not, for the ratio.
    pub total_cost_usd: Decimal,
    /// How many attempts went unserved, so a leak of zero can be told from no
    /// attempts at all.
    pub unserved_attempts: i64,
}

/// A principal's points and leak inside a window, in one pass over the ledger.
///
/// `selection_reason`, never `counterfactual_api_usd = 0`, is the
/// discriminator for "unserved": a *served* row can be zero too, when the
/// request spent no tokens or the model carries no price, and counting those
/// as leaks would inflate the figure with successes.
pub async fn points_for_principal(
    db: &Db,
    principal_id: Uuid,
    window: UsageWindow,
    reference: Option<Decimal>,
    now: OffsetDateTime,
) -> Result<PrincipalPoints> {
    sqlx::query_as::<_, (Option<i64>, Decimal, Decimal, i64)>(
        r"
        SELECT CASE WHEN $3::numeric IS NULL THEN NULL
                    ELSE COALESCE(
                        SUM(ROUND(counterfactual_api_usd * 1000000 / $3::numeric)), 0
                    )::bigint
               END,
               COALESCE(SUM(cost_usd) FILTER (
                   WHERE selection_reason IN ('abandoned', 'lost')
               ), 0)::numeric(16,8),
               COALESCE(SUM(cost_usd), 0)::numeric(16,8),
               COUNT(*) FILTER (
                   WHERE selection_reason IN ('abandoned', 'lost')
               )::bigint
        FROM usage_event
        WHERE principal_id = $1 AND occurred_at >= $2
        ",
    )
    .bind(principal_id)
    .bind(window.since(now))
    .bind(reference)
    .fetch_one(db.pool())
    .await
    .map(|row| PrincipalPoints {
        points: row.0,
        unbilled_cost_usd: row.1,
        total_cost_usd: row.2,
        unserved_attempts: row.3,
    })
    .map_err(|e| Error::Internal(format!("reading a principal's points: {e}")))
}

pub async fn points_for_keys(
    db: &Db,
    keys: &[Uuid],
    window: UsageWindow,
    reference: Decimal,
    now: OffsetDateTime,
) -> Result<Vec<(Uuid, i64)>> {
    sqlx::query_as::<_, (Uuid, i64)>(
        r"
        SELECT api_key_id,
               SUM(ROUND(counterfactual_api_usd * 1000000 / $3::numeric))::bigint
        FROM usage_event
        WHERE api_key_id = ANY($1) AND occurred_at >= $2
        GROUP BY api_key_id
        ",
    )
    .bind(keys)
    .bind(window.since(now))
    .bind(reference)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("reading points over keys: {e}")))
}

/// The row `key_usage` reads, named: nineteen columns is past what a tuple can carry.
#[derive(sqlx::FromRow)]
struct KeyUsageRow {
    id: Uuid,
    name: String,
    key_prefix: String,
    email: String,
    active: bool,
    quota_usd: Option<Decimal>,
    spent_usd: Decimal,
    month_usd: Decimal,
    month_requests: i64,
    month_resets_at: OffsetDateTime,
    five_hour_usd: Decimal,
    five_hour_frees_at: Option<OffsetDateTime>,
    seven_day_usd: Decimal,
    seven_day_frees_at: Option<OffsetDateTime>,
    five_hour_requests: i64,
    seven_day_requests: i64,
    month_counterfactual_usd: Decimal,
    five_hour_counterfactual_usd: Decimal,
    seven_day_counterfactual_usd: Decimal,
    day_usd: Decimal,
    day_frees_at: Option<OffsetDateTime>,
    day_requests: i64,
    day_counterfactual_usd: Decimal,
    month_points: Option<i64>,
    five_hour_points: Option<i64>,
    day_points: Option<i64>,
    seven_day_points: Option<i64>,
}

/// The key-usage panel, as one statement.
///
/// A `const` so the `EXPLAIN` test can plan the statement this function
/// actually runs. The test used to inline the join "verbatim from its `FROM`
/// onwards", which meant deleting S1's window bound from the real query left
/// it green: a copy of a fix cannot fail with it.
const KEY_USAGE_SQL: &str = r"
        SELECT k.id,
               k.name,
               k.key_prefix,
               p.email,
               k.active,
               k.quota_usd,
               k.spent_usd,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
               ), 0)::numeric(16,8) AS month_usd,
               COUNT(u.request_id) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
                     AND u.selection_reason NOT IN ('abandoned', 'lost')
               ) AS month_requests,
               date_trunc('month', now()) + interval '1 month' AS month_resets_at,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '5 hours'
               ), 0)::numeric(16,8) AS five_hour_usd,
               MIN(u.occurred_at) FILTER (
                   WHERE u.occurred_at >= now() - interval '5 hours'
               ) + interval '5 hours' AS five_hour_frees_at,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '7 days'
               ), 0)::numeric(16,8) AS seven_day_usd,
               MIN(u.occurred_at) FILTER (
                   WHERE u.occurred_at >= now() - interval '7 days'
               ) + interval '7 days' AS seven_day_frees_at,
               COUNT(u.request_id) FILTER (
                   WHERE u.occurred_at >= now() - interval '5 hours'
                     AND u.selection_reason NOT IN ('abandoned', 'lost')
               ) AS five_hour_requests,
               COUNT(u.request_id) FILTER (
                   WHERE u.occurred_at >= now() - interval '7 days'
                     AND u.selection_reason NOT IN ('abandoned', 'lost')
               ) AS seven_day_requests,
               COALESCE(SUM(u.counterfactual_api_usd) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
               ), 0)::numeric(16,8) AS month_counterfactual_usd,
               COALESCE(SUM(u.counterfactual_api_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '5 hours'
               ), 0)::numeric(16,8) AS five_hour_counterfactual_usd,
               COALESCE(SUM(u.counterfactual_api_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '7 days'
               ), 0)::numeric(16,8) AS seven_day_counterfactual_usd,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
               ), 0)::numeric(16,8) AS day_usd,
               MIN(u.occurred_at) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
               ) + interval '24 hours' AS day_frees_at,
               COUNT(u.request_id) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
                     AND u.selection_reason NOT IN ('abandoned', 'lost')
               ) AS day_requests,
               COALESCE(SUM(u.counterfactual_api_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
               ), 0)::numeric(16,8) AS day_counterfactual_usd,
               CASE WHEN $2::numeric IS NULL THEN NULL ELSE COALESCE(SUM(ROUND(u.counterfactual_api_usd * 1000000 / $2::numeric)) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
               ), 0)::bigint END AS month_points,
               CASE WHEN $2::numeric IS NULL THEN NULL ELSE COALESCE(SUM(ROUND(u.counterfactual_api_usd * 1000000 / $2::numeric)) FILTER (
                   WHERE u.occurred_at >= now() - interval '5 hours'
               ), 0)::bigint END AS five_hour_points,
               CASE WHEN $2::numeric IS NULL THEN NULL ELSE COALESCE(SUM(ROUND(u.counterfactual_api_usd * 1000000 / $2::numeric)) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
               ), 0)::bigint END AS day_points,
               CASE WHEN $2::numeric IS NULL THEN NULL ELSE COALESCE(SUM(ROUND(u.counterfactual_api_usd * 1000000 / $2::numeric)) FILTER (
                   WHERE u.occurred_at >= now() - interval '7 days'
               ), 0)::bigint END AS seven_day_points
        FROM api_key k
        JOIN principal p ON p.id = k.principal_id
        -- The widest of the four windows below, in the JOIN. Stated only
        -- inside the FILTERs, this join read every row the key had ever
        -- written in order to report a rolling five hours — and this is the
        -- query a partner service calls before each model call, per member.
        -- `usage_event_key_idx` is `(api_key_id, occurred_at DESC)`, so with a
        -- bound here the read is a range scan of the window instead of a walk
        -- of the key's whole history.
        --
        -- LEAST, because which of the two is wider changes through the month:
        -- month-to-date is hours wide on the 1st and 31 days wide on the 31st,
        -- while seven days is seven days. Taking the earlier keeps every
        -- FILTER below able to see the rows it needs.
        --
        -- `ON` rather than `WHERE`: a LEFT JOIN, so a key that has not been
        -- used in the window must still return its row.
        LEFT JOIN usage_event u
               ON u.api_key_id = k.id
              AND u.occurred_at >= LEAST(
                      date_trunc('month', now()),
                      now() - interval '7 days'
                  )
        WHERE k.id = $1
        GROUP BY k.id, k.name, k.key_prefix, p.email, k.active, k.quota_usd, k.spent_usd
";

/// One key's cap and spend; `None` for an id that is not a key. Every figure comes from the
/// ledger, not the counter, for the same reason `principal_usage` reads the ledger: the ledger
/// is the record. One statement, four windows, the key's own rows only.
/// `reference` is the points price, read first by the caller; without one the points fields
/// are `None`, never zero.
// One statement, four windows, ten figures each: the length is the SELECT list, and splitting
// it would read the ledger twice.
#[allow(clippy::too_many_lines)]
pub async fn key_usage(db: &Db, id: Uuid, reference: Option<Decimal>) -> Result<Option<KeyUsage>> {
    sqlx::query_as::<_, KeyUsageRow>(KEY_USAGE_SQL)
        .bind(id)
        .bind(reference)
        .fetch_optional(db.pool())
        .await
        .map(|row| {
            row.map(|row| KeyUsage {
                key_id: row.id,
                name: row.name,
                prefix: row.key_prefix,
                principal_email: row.email,
                active: row.active,
                quota_usd: row.quota_usd,
                spent_usd: row.spent_usd,
                month_to_date_usd: row.month_usd,
                requests: row.month_requests,
                month_resets_at: row.month_resets_at,
                five_hour_usd: row.five_hour_usd,
                five_hour_frees_at: row.five_hour_frees_at,
                seven_day_usd: row.seven_day_usd,
                seven_day_frees_at: row.seven_day_frees_at,
                five_hour_requests: row.five_hour_requests,
                seven_day_requests: row.seven_day_requests,
                month_counterfactual_usd: row.month_counterfactual_usd,
                five_hour_counterfactual_usd: row.five_hour_counterfactual_usd,
                seven_day_counterfactual_usd: row.seven_day_counterfactual_usd,
                day_usd: row.day_usd,
                day_frees_at: row.day_frees_at,
                day_requests: row.day_requests,
                day_counterfactual_usd: row.day_counterfactual_usd,
                month_points: row.month_points,
                five_hour_points: row.five_hour_points,
                day_points: row.day_points,
                seven_day_points: row.seven_day_points,
            })
        })
        .map_err(|e| Error::Internal(format!("reading key usage: {e}")))
}

/// The points reference price — one token at this many USD per million is one point — if the
/// admin has set one. `None` until then: no multiplier and no points figure can be derived.
pub async fn points_reference(db: &Db) -> Result<Option<Decimal>> {
    sqlx::query_scalar::<_, Decimal>("SELECT usd_per_mtok FROM points_reference WHERE only_row")
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("reading the points reference: {e}")))
}

/// Set the points reference price. One row, replaced; the caller has already refused a price
/// that is not positive, and the table's own check refuses it again.
pub async fn set_points_reference(db: &Db, usd_per_mtok: Decimal) -> Result<()> {
    sqlx::query(
        "INSERT INTO points_reference (only_row, usd_per_mtok) VALUES (true, $1)
         ON CONFLICT (only_row) DO UPDATE SET usd_per_mtok = EXCLUDED.usd_per_mtok, updated_at = now()",
    )
    .bind(usd_per_mtok)
    .execute(db.pool())
    .await
    .map(|_| ())
    .map_err(|e| Error::Internal(format!("setting the points reference: {e}")))
}

/// Revoke by the displayed prefix, for the CLI — during an incident the prefix
/// is what an operator can actually see.
///
/// Every match, and every match is returned. `key_prefix` carries no unique
/// index and this UPDATE has no LIMIT, so a collision has always deactivated
/// more than one row; what it did not do was say so. The caller took
/// `fetch_optional`, which keeps the first row and drops the rest — so the
/// other keys were deactivated in the database, their hashes were never evicted
/// from the shared cache, and they went on authenticating from L2 for the five
/// minutes of its TTL. An operator revoking a leaked key during an incident was
/// told one key was revoked, and got one eviction, while some other customer's
/// key had been switched off behind their back and the leaked one might not
/// have stopped.
///
/// Over-revoking on a collision is the right direction for an incident tool —
/// under-revoking is what gets someone breached — but only if it is visible.
/// Returning the whole set is what makes it visible.
pub async fn revoke_key_by_prefix(db: &Db, prefix: &str) -> Result<Vec<(String, String, String)>> {
    sqlx::query_as::<_, (String, String, String)>(
        "UPDATE api_key SET active = false WHERE key_prefix = $1 AND active
         RETURNING key_hash, name, key_prefix",
    )
    .bind(prefix)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("revoking key by prefix: {e}")))
}

/// Put a credential in cooldown after a failure.
pub async fn cool_down(db: &Db, id: AccountId, until: OffsetDateTime, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE account SET cooldown_until = $2, cooldown_reason = $3, updated_at = now()
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(until)
    .bind(reason)
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("cooling down account: {e}")))?;
    Ok(())
}

/// Record a provider-declared rate limit window.
pub async fn rate_limit(db: &Db, id: AccountId, until: OffsetDateTime) -> Result<()> {
    sqlx::query("UPDATE account SET rate_limited_until = $2, updated_at = now() WHERE id = $1")
        .bind(id.as_uuid())
        .bind(until)
        .execute(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("rate limiting account: {e}")))?;
    Ok(())
}

/// Schedulable credentials of one provider and kind.
///
/// The usage poller asks an xAI API key for its model list. That key has no
/// quota to read, so it is not in [`schedulable_oauth_accounts`], but a new
/// model still has to land in the catalog or the picker never grows.
pub async fn schedulable_accounts(db: &Db, provider: &str, kind: &str) -> Result<Vec<AccountRow>> {
    sqlx::query_as::<_, AccountRow>(
        r"
        SELECT id, name, provider, kind, credentials_sealed, credentials_nonce,
               token_version, token_expires_at, owner_principal_id, proxy_url,
               priority, max_concurrency, schedulable, cooldown_until,
               rate_limited_until, window_resets_at,
               usage_remaining_pct, usage_reserve_pct, last_used_at
        FROM account
        WHERE provider = $1 AND kind = $2 AND schedulable
        ",
    )
    .bind(provider)
    .bind(kind)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading {provider} {kind} accounts: {e}")))
}

/// Every OAuth (subscription seat) account, for the usage poller to sweep.
///
/// Not filtered by route or principal like `candidates`: the poller reads a
/// seat's remaining quota regardless of who may use it. Disabled seats are
/// skipped — polling one nobody will schedule spends a request for nothing.
pub async fn schedulable_oauth_accounts(db: &Db) -> Result<Vec<AccountRow>> {
    sqlx::query_as::<_, AccountRow>(
        r"
        SELECT id, name, provider, kind, credentials_sealed, credentials_nonce,
               token_version, token_expires_at, owner_principal_id, proxy_url,
               priority, max_concurrency, schedulable, cooldown_until,
               rate_limited_until, window_resets_at,
               usage_remaining_pct, usage_reserve_pct, last_used_at
        FROM account WHERE kind = 'oauth' AND schedulable
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading oauth accounts: {e}")))
}

/// Store a usage-poll reading: the remaining-quota columns, and the window
/// reset the scheduler prefers to drain first. `resets_at` lands in
/// `window_resets_at`, which this poller is the only writer of. (The failover
/// path's `Retry-After` writes `rate_limited_until` — a different column with
/// a different meaning: one is when a quota window turns over, the other is
/// when a provider will take requests again.) The value is never cleared once
/// the moment passes; readers must compare it to now, and the scheduler does.
pub async fn record_usage_poll(
    db: &Db,
    id: AccountId,
    remaining_pct: f64,
    window_label: &str,
    resets_at: Option<OffsetDateTime>,
) -> Result<()> {
    sqlx::query(
        r"
        UPDATE account
           SET usage_remaining_pct = $2,
               usage_window_label  = $3,
               usage_polled_at     = now(),
               window_resets_at    = COALESCE($4, window_resets_at),
               updated_at          = now()
         WHERE id = $1
        ",
    )
    .bind(id.as_uuid())
    .bind(rust_decimal::Decimal::try_from(remaining_pct).unwrap_or_default())
    .bind(window_label)
    .bind(resets_at)
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("recording usage poll: {e}")))?;
    Ok(())
}

/// Persist refreshed credential material.
///
/// The `token_version` guard is a compare-and-swap: two replicas that refresh
/// the same expiring credential at once must not have the loser's older token
/// overwrite the winner's newer one. Returns whether this call won.
pub async fn store_credentials(
    db: &Db,
    id: AccountId,
    sealed: &oag_core::Sealed,
    expected_version: i64,
    expires_at: Option<OffsetDateTime>,
) -> Result<bool> {
    let result = sqlx::query(
        r"
        UPDATE account
        SET credentials_sealed = $2, credentials_nonce = $3,
            token_version = token_version + 1, token_expires_at = $5, updated_at = now()
        WHERE id = $1 AND token_version = $4
        ",
    )
    .bind(id.as_uuid())
    .bind(&sealed.ciphertext)
    .bind(&sealed.nonce)
    .bind(expected_version)
    .bind(expires_at)
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("storing credentials: {e}")))?;

    Ok(result.rows_affected() == 1)
}

/// Append to the usage ledger and debit the key that paid for it.
///
/// `ON CONFLICT DO NOTHING` makes metering idempotent: a retried write after a
/// partial failure conflicts instead of billing twice.
///
/// Deliberately with no conflict target, which is the only form that is correct
/// on both sides of the ledger's key change. Naming one is naming an index that
/// has to exist: `(request_id)` breaks the moment the primary key is contracted
/// away, and `(request_id, attempt)` breaks on any database that has not had
/// that index built yet — either way with 42P10, mid-deploy, on the write that
/// carries the spend. Untargeted, every unique constraint is an arbiter, so
/// while the primary key survives a second attempt is silently dropped, and once
/// it is gone the same statement starts keeping both rows with no code change.
///
/// One statement, because the row and the debit are one fact. Two statements on
/// two pooled connections is two transactions: a crash between them leaves spend
/// in the ledger that the quota check cannot see, and — worse — an
/// unconditional `UPDATE` charges for inserts that never happened. Both writes
/// the conflict clause exists to swallow, the replay and the second attempt the
/// surviving primary key drops, still moved `spent_usd`. `RETURNING` into the
/// CTE ties the two together: no row inserted, nothing to join against, no
/// debit. The idempotence now covers the money and not just the row.
pub async fn record_usage(db: &Db, w: &UsageWrite) -> Result<()> {
    sqlx::query(
        r"
        WITH ins AS (
            INSERT INTO usage_event (
                request_id, attempt, principal_id, api_key_id, route_id, account_id,
                model_id, tier, selection_reason, escalated_from_tier, escalation_gate,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                cost_usd, counterfactual_usd, counterfactual_model_id, counterfactual_api_usd,
                status, latency_ms, ttft_ms, streamed
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23)
            ON CONFLICT DO NOTHING
            RETURNING api_key_id, principal_id, route_id, account_id, cost_usd
        ),
        -- The credential's recency, for the scheduler's last-resort tie-break.
        -- This was its own UPDATE on the response path, awaited between the
        -- upstream answering and the first byte going out; here it costs
        -- nothing the ledger write was not already paying.
        account_touch AS (
            UPDATE account a
               SET last_used_at = now()
              FROM ins
             WHERE a.id = ins.account_id
        ),
        -- Spend is denormalised for the cap checks, which must not run a SUM
        -- over the ledger on every request — and must not read a cached copy
        -- either. Debiting the amount the ledger accepted, rather than the
        -- amount passed in, is what keeps each counter and the rows it stands
        -- for from drifting apart. All three in one statement with the
        -- insert, for the same reason the first one was: the row and the
        -- debits are one fact.
        key_debit AS (
            UPDATE api_key k
               SET spent_usd = k.spent_usd + ins.cost_usd, last_used_at = now()
              FROM ins
             WHERE k.id = ins.api_key_id
        ),
        -- Monthly counters reset at the boundary by the first write of the
        -- new month, so no job has to. A row whose month has passed and has
        -- not been written yet reads as zero (see `spend_for`, `route_by_id`).
        --
        -- The month only moves forward. `now()` is the transaction's start,
        -- and a write that started before midnight can be re-evaluated,
        -- after waiting on the row lock, against a row a new-month write has
        -- just committed. An equality test there saw a month not its own, wrote
        -- its own cost alone and stamped the month back — dropping the
        -- other write's spend from the cap. A row already in a later month
        -- is accumulated into and left where it is.
        principal_debit AS (
            UPDATE principal p
               SET spent_usd = CASE WHEN p.spent_month >= date_trunc('month', now())::date
                                    THEN p.spent_usd + ins.cost_usd
                                    ELSE ins.cost_usd END,
                   spent_month = GREATEST(p.spent_month, date_trunc('month', now())::date)
              FROM ins
             WHERE p.id = ins.principal_id
        )
        UPDATE route r
           SET spent_usd = CASE WHEN r.spent_month >= date_trunc('month', now())::date
                                THEN r.spent_usd + ins.cost_usd
                                ELSE ins.cost_usd END,
               spent_month = GREATEST(r.spent_month, date_trunc('month', now())::date)
          FROM ins
         WHERE r.id = ins.route_id
        ",
    )
    .bind(w.request_id)
    .bind(w.attempt)
    .bind(w.principal_id)
    .bind(w.api_key_id)
    .bind(w.route_id)
    .bind(w.account_id)
    .bind(&w.model_id)
    .bind(&w.tier)
    .bind(&w.selection_reason)
    .bind(&w.escalated_from_tier)
    .bind(&w.escalation_gate)
    .bind(i64::try_from(w.usage.input_tokens).unwrap_or(i64::MAX))
    .bind(i64::try_from(w.usage.output_tokens).unwrap_or(i64::MAX))
    .bind(i64::try_from(w.usage.cache_read_tokens).unwrap_or(i64::MAX))
    .bind(i64::try_from(w.usage.cache_write_tokens).unwrap_or(i64::MAX))
    .bind(w.cost_usd)
    .bind(w.counterfactual_usd)
    .bind(&w.counterfactual_model_id)
    .bind(w.counterfactual_api_usd)
    .bind(w.status)
    .bind(w.latency_ms)
    .bind(w.ttft_ms)
    .bind(w.streamed)
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("recording usage: {e}")))?;

    Ok(())
}

/// When each gateway-served row happened, and the four token counts it holds.
///
/// The importer's only question of the ledger. It cannot ask "was this session
/// proxied" directly — a CLI transcript records no base URL, no endpoint and no
/// upstream request id — so it asks whether a call with these exact counts was
/// already metered around this time, which is the same question asked of
/// evidence the ledger does hold.
///
/// Imported rows are excluded by `origin`. Including them would make a second
/// import agree with the first about everything and skip the whole corpus,
/// which looks identical to a clean re-run and is not.
pub async fn gateway_fingerprints(
    db: &Db,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<(OffsetDateTime, i64, i64, i64, i64)>> {
    sqlx::query_as::<_, (OffsetDateTime, i64, i64, i64, i64)>(
        r"
        SELECT occurred_at, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens
        FROM usage_event
        WHERE origin = 'gateway'
          AND occurred_at >= $1
          AND occurred_at <= $2
        ",
    )
    .bind(from)
    .bind(to)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading ledger fingerprints: {e}")))
}

/// When this gateway served one provider, and nothing else about those rows.
///
/// The weaker question, for a source whose own records cannot be compared to a
/// ledger row at all. The Grok CLI logs one aggregate per user turn covering
/// every model call the turn made; the ledger holds one row per call. No
/// fingerprint can ever line up across that, so the only thing left to ask is
/// whether this gateway was serving the provider at all while the session ran.
///
/// Scoped by the provider segment of `model_id` rather than by a join onto the
/// catalog: `model_id` is plain text with no foreign key, so a model since
/// removed from the catalog would drop out of a join and take its evidence of
/// proxying with it. A row whose id has lost its provider prefix is simply not
/// evidence, which is the direction that skips rather than the one that
/// double counts.
///
/// Imported rows are excluded by `origin` for the same reason they are in
/// [`gateway_fingerprints`]: a second import must not find the first one and
/// conclude the whole corpus was proxied.
pub async fn gateway_activity(
    db: &Db,
    provider: &str,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<OffsetDateTime>> {
    sqlx::query_scalar::<_, OffsetDateTime>(
        r"
        SELECT occurred_at
        FROM usage_event
        WHERE origin = 'gateway'
          AND model_id LIKE $1
          AND occurred_at >= $2
          AND occurred_at <= $3
        ",
    )
    .bind(format!("{provider}/%"))
    .bind(from)
    .bind(to)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading ledger activity: {e}")))
}

/// The whole model catalog.
pub async fn catalog(db: &Db) -> Result<Vec<ModelRow>> {
    sqlx::query_as::<_, ModelRow>(
        r"
        SELECT id, provider, upstream_name, input_per_mtok, output_per_mtok,
               cache_read_per_mtok, cache_write_per_mtok, context_window,
               max_output_tokens, supports_vision, supports_tools,
               supports_reasoning, supports_prompt_cache, display_label
        FROM model_catalog
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading catalog: {e}")))
}

/// Name a model, or hand it back to the derived default.
///
/// `None` clears the column, which is not the same as writing the derived
/// string into it: a cleared row keeps following the provider's spelling, while
/// a stored copy of today's derivation would go stale the moment the catalog is
/// refreshed.
///
/// No `is_override` guard here, unlike every other write to this table. That
/// flag protects an operator's numbers from an automated refresh, and this *is*
/// the operator — refusing their rename because they had once edited a price
/// would be the guard firing at the person it exists for.
///
/// Returns the id when a row was renamed, `None` when there is no such model,
/// which is the caller's 404.
pub async fn set_model_label(db: &Db, id: &str, label: Option<&str>) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>(
        "UPDATE model_catalog SET display_label = $2, updated_at = now() \
         WHERE id = $1 RETURNING id",
    )
    .bind(id)
    .bind(label)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("labelling model: {e}")))
}

const LIST_SERVICES_SQL: &str = concat!(
    "SELECT ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at ",
    "FROM service ORDER BY name"
);
const SERVICE_BY_ID_SQL: &str = concat!(
    "SELECT ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at ",
    "FROM service WHERE id = $1"
);
const INSERT_SERVICE_SQL: &str = concat!(
    "INSERT INTO service (",
    "id, name, kind, base_url, health_path, dashboard_url, auth_ref",
    ") VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at"
);
const UPDATE_SERVICE_SQL: &str = concat!(
    "UPDATE service SET ",
    "name = $2, kind = $3, base_url = $4, health_path = $5, ",
    "dashboard_url = $6, auth_ref = $7, enabled = $8 ",
    "WHERE id = $1 RETURNING ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at"
);
const RECORD_HEALTH_SQL: &str = concat!(
    "UPDATE service SET ",
    "last_ok = CASE WHEN $2 THEN now() ELSE last_ok END, ",
    "last_error = $3 ",
    "WHERE id = $1 RETURNING ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at"
);

/// Values to insert a catalog row. Validation of URLs and kind belongs to
/// the caller — the store persists what it is given, and the SQL CHECKs are
/// the second line.
#[derive(Debug, Clone)]
pub struct NewService<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub kind: &'a str,
    pub base_url: &'a str,
    pub health_path: &'a str,
    pub dashboard_url: Option<&'a str>,
    pub auth_ref: Option<Uuid>,
}

/// Replacement values for a catalog row. Health columns are not here: they
/// are written only by [`record_service_health`].
#[derive(Debug, Clone)]
pub struct ServiceUpdate<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub base_url: &'a str,
    pub health_path: &'a str,
    pub dashboard_url: Option<&'a str>,
    pub auth_ref: Option<Uuid>,
    pub enabled: bool,
}

pub async fn list_services(db: &Db) -> Result<Vec<ServiceRow>> {
    sqlx::query_as::<_, ServiceRow>(LIST_SERVICES_SQL)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("listing services: {e}")))
}

pub async fn service_by_id(db: &Db, id: Uuid) -> Result<Option<ServiceRow>> {
    sqlx::query_as::<_, ServiceRow>(SERVICE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("loading service: {e}")))
}

pub async fn insert_service(db: &Db, s: &NewService<'_>) -> Result<ServiceRow> {
    sqlx::query_as::<_, ServiceRow>(INSERT_SERVICE_SQL)
        .bind(s.id)
        .bind(s.name)
        .bind(s.kind)
        .bind(s.base_url)
        .bind(s.health_path)
        .bind(s.dashboard_url)
        .bind(s.auth_ref)
        .fetch_one(db.pool())
        .await
        .map_err(|e| map_service_write_error("creating service", &e))
}

pub async fn update_service(
    db: &Db,
    id: Uuid,
    s: &ServiceUpdate<'_>,
) -> Result<Option<ServiceRow>> {
    sqlx::query_as::<_, ServiceRow>(UPDATE_SERVICE_SQL)
        .bind(id)
        .bind(s.name)
        .bind(s.kind)
        .bind(s.base_url)
        .bind(s.health_path)
        .bind(s.dashboard_url)
        .bind(s.auth_ref)
        .bind(s.enabled)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| map_service_write_error("updating service", &e))
}

/// Take a service out of the catalog's active set, or put it back.
pub async fn set_service_enabled(db: &Db, id: Uuid, enabled: bool) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>("UPDATE service SET enabled = $2 WHERE id = $1 RETURNING name")
        .bind(id)
        .bind(enabled)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("setting service enabled: {e}")))
}

/// Record the outcome of a health probe.
///
/// A success stamps `last_ok` and clears `last_error`. A failure writes the
/// error and leaves `last_ok` alone, so "was healthy, now is not" stays
/// visible.
pub async fn record_service_health(
    db: &Db,
    id: Uuid,
    ok: bool,
    error: Option<&str>,
) -> Result<Option<ServiceRow>> {
    sqlx::query_as::<_, ServiceRow>(RECORD_HEALTH_SQL)
        .bind(id)
        .bind(ok)
        .bind(error)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("recording service health: {e}")))
}

fn map_service_write_error(what: &str, e: &sqlx::Error) -> Error {
    if let Some(db) = e.as_database_error() {
        match db.code().as_deref() {
            Some("23505") => {
                return Error::Config("a service with that name already exists".to_owned());
            }
            Some("23503") => {
                return Error::Config(
                    "auth_ref does not match a credential in the pool".to_owned(),
                );
            }
            Some("23514") => {
                return Error::Config("service row failed a database check".to_owned());
            }
            _ => {}
        }
    }
    Error::Internal(format!("{what}: {e}"))
}

/// The upsert, as a named constant so a test can read what the conflict branch
/// does and does not touch. The columns it leaves out are the point of it.
const UPSERT_MODEL_SQL: &str = r"
        INSERT INTO model_catalog (
            id, provider, upstream_name, input_per_mtok, output_per_mtok,
            cache_read_per_mtok, cache_write_per_mtok, context_window, max_output_tokens,
            supports_vision, supports_tools, supports_reasoning, supports_prompt_cache,
            is_override, display_label
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
        ON CONFLICT (id) DO UPDATE SET
            provider = EXCLUDED.provider,
            upstream_name = EXCLUDED.upstream_name,
            input_per_mtok = EXCLUDED.input_per_mtok,
            output_per_mtok = EXCLUDED.output_per_mtok,
            cache_read_per_mtok = EXCLUDED.cache_read_per_mtok,
            cache_write_per_mtok = EXCLUDED.cache_write_per_mtok,
            context_window = EXCLUDED.context_window,
            max_output_tokens = EXCLUDED.max_output_tokens,
            supports_vision = EXCLUDED.supports_vision,
            supports_tools = EXCLUDED.supports_tools,
            supports_reasoning = EXCLUDED.supports_reasoning,
            supports_prompt_cache = EXCLUDED.supports_prompt_cache,
            updated_at = now()
        -- `display_label` is missing from that list on purpose, exactly as
        -- `is_override` is: a seed carries no label and would write NULL over
        -- whatever the operator called the model. The name is theirs, so only
        -- `set_model_label` writes it, and a re-seed leaves it where it was.
        --
        -- An operator who edited a price meant it. A catalog refresh from
        -- upstream pricing data must not silently undo that.
        WHERE model_catalog.is_override = false
        ";

/// Insert or update a catalog entry, never clobbering an operator override.
pub async fn upsert_model(db: &Db, m: &ModelRow, is_override: bool) -> Result<()> {
    sqlx::query(UPSERT_MODEL_SQL)
        .bind(&m.id)
        .bind(&m.provider)
        .bind(&m.upstream_name)
        .bind(m.input_per_mtok)
        .bind(m.output_per_mtok)
        .bind(m.cache_read_per_mtok)
        .bind(m.cache_write_per_mtok)
        .bind(m.context_window)
        .bind(m.max_output_tokens)
        .bind(m.supports_vision)
        .bind(m.supports_tools)
        .bind(m.supports_reasoning)
        .bind(m.supports_prompt_cache)
        .bind(is_override)
        // Only ever reaches an INSERT: a seed builds rows with no label, and the
        // conflict branch above does not name the column.
        .bind(m.display_label.as_deref())
        .execute(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("upserting model: {e}")))?;
    Ok(())
}

/// Refresh only the prices of an existing catalog entry.
///
/// Deliberately not `upsert_model` with a rebuilt row. A provider's own price
/// API is authoritative about money and silent about context windows, so an
/// upsert would carry whatever the caller guessed into `context_window` and
/// `max_output_tokens` — and a window that shrinks from 500k to a guess is a
/// router that quietly stops offering the one model a long request fits in.
/// The columns not named here keep whatever a LiteLLM seed or an operator put
/// there.
///
/// A `None` cache price means the provider did not state one, which is not the
/// same as stating zero, so the existing value survives.
///
/// Returns false when nothing was updated: either no such id, or an operator
/// override, which is left alone for the same reason `upsert_model` leaves it.
pub async fn update_model_prices(
    db: &Db,
    id: &str,
    input_per_mtok: Decimal,
    output_per_mtok: Decimal,
    cache_read_per_mtok: Option<Decimal>,
) -> Result<bool> {
    let done = sqlx::query(
        r"
        UPDATE model_catalog SET
            input_per_mtok = $2,
            output_per_mtok = $3,
            cache_read_per_mtok = COALESCE($4, cache_read_per_mtok),
            updated_at = now()
        WHERE id = $1 AND is_override = false
        ",
    )
    .bind(id)
    .bind(input_per_mtok)
    .bind(output_per_mtok)
    .bind(cache_read_per_mtok)
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("repricing model: {e}")))?;

    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests;
