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
/// one. sub2api stores inbound keys plaintext and matches on column equality,
/// which turns read access to one table into every client's credential.
/// What every key this gateway has ever issued looks like: the prefix, then
/// 32 bytes of entropy as lowercase hex. See `mint_key`.
pub const KEY_PREFIX: &str = "oag_live_";
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
        ),
    >(
        r"
        SELECT k.id, k.principal_id, k.route_id, k.floor_tier,
               k.quota_usd,
               p.monthly_budget_usd, p.hard_stop_multiple,
               k.admin
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
        key_hash: hash,
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
    use rand::Rng as _;
    use std::fmt::Write as _;

    // 32 bytes of entropy. The prefix exists so a leaked key is recognisable in
    // a log and greppable during an incident.
    let mut raw = [0u8; 32];
    rand::thread_rng().fill(&mut raw);
    let key = format!(
        "{KEY_PREFIX}{}",
        raw.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    );
    let hash = hash_key(&key);
    let prefix: String = key.chars().take(16).collect();

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
    sqlx::query_as::<_, (Uuid, String, Option<Decimal>, Decimal, i64)>(
        r"
        SELECT p.id,
               p.email,
               p.monthly_budget_usd,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
               ), 0)::numeric(14,6),
               COUNT(u.request_id) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
                     AND u.selection_reason NOT IN ('abandoned', 'lost')
               )
        FROM principal p
        LEFT JOIN usage_event u ON u.principal_id = p.id
        WHERE p.email = $1
        GROUP BY p.id, p.email, p.monthly_budget_usd
        ",
    )
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
/// statement as the ledger row. The three windows are the ledger summed since an instant —
/// a rolling five hours, a rolling seven days, the first of the current UTC month — the shape
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
               COUNT(*) FILTER (
                   WHERE selection_reason NOT IN ('abandoned', 'lost')
               ) AS requests,
               COALESCE(SUM(input_tokens), 0)::bigint AS input_tokens,
               COALESCE(SUM(output_tokens), 0)::bigint AS output_tokens,
               COALESCE(SUM(cache_read_tokens), 0)::bigint AS cache_read_tokens,
               COALESCE(SUM(cache_write_tokens), 0)::bigint AS cache_write_tokens,
               COALESCE(SUM(cost_usd), 0)::numeric(14,6) AS cost_usd,
               COALESCE(SUM(counterfactual_api_usd), 0)::numeric(14,6) AS list_usd,
               CASE WHEN $3::numeric IS NULL THEN NULL
                    ELSE SUM(ROUND(counterfactual_api_usd * 1000000 / $3::numeric))::bigint
               END AS points
        FROM usage_event
        WHERE api_key_id = $1 AND occurred_at >= $2
        GROUP BY model_id
        ORDER BY list_usd DESC, model_id
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

/// One key's cap and spend; `None` for an id that is not a key. Every figure comes from the
/// ledger, not the counter, for the same reason `principal_usage` reads the ledger: the ledger
/// is the record. One statement, three windows, the key's own rows only.
/// `reference` is the points price, read first by the caller; without one the points fields
/// are `None`, never zero.
// One statement, four windows, ten figures each: the length is the SELECT list, and splitting
// it would read the ledger twice.
#[allow(clippy::too_many_lines)]
pub async fn key_usage(db: &Db, id: Uuid, reference: Option<Decimal>) -> Result<Option<KeyUsage>> {
    sqlx::query_as::<_, KeyUsageRow>(
        r"
        SELECT k.id,
               k.name,
               k.key_prefix,
               p.email,
               k.active,
               k.quota_usd,
               k.spent_usd,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
               ), 0)::numeric(14,6) AS month_usd,
               COUNT(u.request_id) FILTER (
                   WHERE u.occurred_at >= date_trunc('month', now())
                     AND u.selection_reason NOT IN ('abandoned', 'lost')
               ) AS month_requests,
               date_trunc('month', now()) + interval '1 month' AS month_resets_at,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '5 hours'
               ), 0)::numeric(14,6) AS five_hour_usd,
               MIN(u.occurred_at) FILTER (
                   WHERE u.occurred_at >= now() - interval '5 hours'
               ) + interval '5 hours' AS five_hour_frees_at,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '7 days'
               ), 0)::numeric(14,6) AS seven_day_usd,
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
               ), 0)::numeric(14,6) AS month_counterfactual_usd,
               COALESCE(SUM(u.counterfactual_api_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '5 hours'
               ), 0)::numeric(14,6) AS five_hour_counterfactual_usd,
               COALESCE(SUM(u.counterfactual_api_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '7 days'
               ), 0)::numeric(14,6) AS seven_day_counterfactual_usd,
               COALESCE(SUM(u.cost_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
               ), 0)::numeric(14,6) AS day_usd,
               MIN(u.occurred_at) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
               ) + interval '24 hours' AS day_frees_at,
               COUNT(u.request_id) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
                     AND u.selection_reason NOT IN ('abandoned', 'lost')
               ) AS day_requests,
               COALESCE(SUM(u.counterfactual_api_usd) FILTER (
                   WHERE u.occurred_at >= now() - interval '24 hours'
               ), 0)::numeric(14,6) AS day_counterfactual_usd,
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
        LEFT JOIN usage_event u ON u.api_key_id = k.id
        WHERE k.id = $1
        GROUP BY k.id, k.name, k.key_prefix, p.email, k.active, k.quota_usd, k.spent_usd
        ",
    )
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
pub async fn revoke_key_by_prefix(
    db: &Db,
    prefix: &str,
) -> Result<Option<(String, String, String)>> {
    sqlx::query_as::<_, (String, String, String)>(
        "UPDATE api_key SET active = false WHERE key_prefix = $1 AND active
         RETURNING key_hash, name, key_prefix",
    )
    .bind(prefix)
    .fetch_optional(db.pool())
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
mod tests {
    use super::*;
    use crate::Db;
    use rust_decimal::dec;

    #[test]
    fn hashing_is_stable_and_hex() {
        let h = hash_key("oag_live_abc123");
        assert_eq!(h.len(), 64);
        assert_eq!(h, hash_key("oag_live_abc123"));
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn only_a_key_this_gateway_could_have_minted_has_the_issued_shape() {
        let minted = format!("{KEY_PREFIX}{}", "0123456789abcdef".repeat(4));
        assert_eq!(minted.len(), KEY_LEN);
        assert!(is_issued_key_shape(&minted));

        // Every way a string can fail to be one of ours, each refused
        // without a lookup: wrong prefix, wrong length either way, a
        // character outside lowercase hex, and the fake key errors.hurl
        // sends to prove a 401.
        assert!(!is_issued_key_shape("sk-ant-0123456789abcdef"));
        assert!(!is_issued_key_shape(&minted[..KEY_LEN - 1]));
        assert!(!is_issued_key_shape(&format!("{minted}0")));
        assert!(!is_issued_key_shape(&minted.to_uppercase()));
        assert!(!is_issued_key_shape(&format!(
            "{KEY_PREFIX}{}",
            "g".repeat(64)
        )));
        assert!(!is_issued_key_shape("oag_live_definitely_not_a_real_key"));
        assert!(!is_issued_key_shape(""));
    }

    #[test]
    fn different_keys_hash_differently() {
        assert_ne!(hash_key("oag_live_a"), hash_key("oag_live_b"));
    }

    #[test]
    fn the_hash_does_not_contain_the_key() {
        assert!(!hash_key("oag_live_secret").contains("secret"));
    }

    /// The identity-integration round trip: bind a principal, mint a key on it,
    /// and confirm the key authenticates, carries its cap, is never admin, and
    /// stops working once revoked.
    ///
    /// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
    #[tokio::test]
    async fn a_minted_key_authenticates_is_capped_and_is_never_admin() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let email = format!("org-{}@gateway.local", Uuid::new_v4());
        let route = format!("route-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO route (id, name, tiers, default_mode)
             VALUES (gen_random_uuid(), $1, '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb, 'passthrough')",
        )
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("insert route");

        // Upsert is idempotent on email: a second call must not create a twin.
        let first = upsert_principal(&db, &email, "member", Some(dec!(25.00)))
            .await
            .expect("upsert");
        let again = upsert_principal(&db, &email, "member", None)
            .await
            .expect("upsert again");
        assert_eq!(first, again, "upsert is idempotent on email");

        // ...and a budget already set is not erased by an upsert that omits one.
        let usage = principal_usage(&db, &email)
            .await
            .expect("usage")
            .expect("principal exists");
        assert_eq!(usage.monthly_budget_usd, Some(dec!(25.000000)));
        assert_eq!(usage.month_to_date_usd, dec!(0));

        let minted = mint_key(&db, &email, &route, "member-key", Some(dec!(5.00)))
            .await
            .expect("mint")
            .expect("principal and route exist");
        assert!(minted.key.starts_with("oag_live_"));
        assert_eq!(minted.prefix, minted.key[..16]);

        let context = authenticate(&db, &minted.key)
            .await
            .expect("authenticate")
            .expect("the minted key is live");
        assert_eq!(context.principal_id, first);
        assert!(
            !context.admin,
            "a key minted over HTTP must never carry admin authority"
        );

        // The cap landed, and can be cleared.
        let quota: Option<Decimal> =
            sqlx::query_scalar("SELECT quota_usd FROM api_key WHERE id = $1")
                .bind(minted.id)
                .fetch_one(db.pool())
                .await
                .expect("read quota");
        assert_eq!(quota, Some(dec!(5.000000)));
        set_key_quota(&db, minted.id, None)
            .await
            .expect("clear quota")
            .expect("key exists");

        // The org budget can be raised.
        set_principal_budget(&db, &email, Some(dec!(99.00)))
            .await
            .expect("set budget")
            .expect("principal exists");
        let raised = principal_usage(&db, &email)
            .await
            .expect("usage")
            .expect("principal exists");
        assert_eq!(raised.monthly_budget_usd, Some(dec!(99.000000)));

        // Revocation is what makes the key stop working.
        revoke_key(&db, minted.id).await.expect("revoke");
        assert!(
            authenticate(&db, &minted.key)
                .await
                .expect("authenticate")
                .is_none(),
            "a revoked key must not authenticate"
        );
    }

    /// An idempotent bind MUST NOT be able to remove authority: upserting against
    /// an existing admin's email leaves their role alone. Getting this wrong locks
    /// a human operator out of the admin API — the gate wants an admin key AND an
    /// admin principal — without their key ever changing.
    /// The plumbing a budget or quota write evicts through: every hash of a
    /// principal's keys, and the hash of the one key a quota write touched.
    ///
    /// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
    #[tokio::test]
    async fn a_principals_key_hashes_are_all_listed_and_a_quota_write_names_its_own() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let email = format!("org-{}@gateway.local", Uuid::new_v4());
        let route = format!("route-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO route (id, name, tiers, default_mode)
             VALUES (gen_random_uuid(), $1, '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb, 'passthrough')",
        )
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("insert route");
        let principal = upsert_principal(&db, &email, "member", None)
            .await
            .expect("upsert");

        let one = mint_key(&db, &email, &route, "one", None)
            .await
            .expect("mint")
            .expect("principal and route exist");
        let two = mint_key(&db, &email, &route, "two", None)
            .await
            .expect("mint")
            .expect("principal and route exist");

        let mut listed = key_hashes_for_principal(&db, principal)
            .await
            .expect("list");
        listed.sort();
        let mut minted = vec![hash_key(&one.key), hash_key(&two.key)];
        minted.sort();
        assert_eq!(
            listed, minted,
            "both keys, by the hash the caches are keyed on"
        );

        let (_, _, touched) = set_key_quota(&db, two.id, Some(dec!(1)))
            .await
            .expect("set quota")
            .expect("the key exists");
        assert_eq!(
            touched,
            hash_key(&two.key),
            "the hash of the key written, not another"
        );
    }

    #[tokio::test]
    async fn upserting_a_principal_never_demotes_an_existing_admin() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let email = format!("operator-{}@example.com", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'admin')",
        )
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("seed an admin principal");

        // The identity-integration path can only ever ask for `member`.
        upsert_principal(&db, &email, "member", Some(dec!(10.00)))
            .await
            .expect("upsert");

        let role: String = sqlx::query_scalar("SELECT role FROM principal WHERE email = $1")
            .bind(&email)
            .fetch_one(db.pool())
            .await
            .expect("read role");
        assert_eq!(
            role, "admin",
            "an upsert must not strip an existing principal's admin role"
        );
        // ...while still doing its actual job.
        let usage = principal_usage(&db, &email)
            .await
            .expect("usage")
            .expect("exists");
        assert_eq!(usage.monthly_budget_usd, Some(dec!(10.000000)));
    }

    /// Naming a principal or route that does not exist is reported, not silently
    /// swallowed — otherwise a caller believes it minted a key that never was.
    #[tokio::test]
    async fn minting_on_a_missing_principal_or_route_is_none_not_a_phantom_key() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let nobody = format!("nobody-{}@gateway.local", Uuid::new_v4());
        assert!(
            mint_key(&db, &nobody, "default", "k", None)
                .await
                .expect("mint")
                .is_none()
        );
        assert!(
            set_principal_budget(&db, &nobody, Some(dec!(1.00)))
                .await
                .expect("budget")
                .is_none()
        );
        assert!(
            principal_usage(&db, &nobody)
                .await
                .expect("usage")
                .is_none()
        );
    }

    /// `route_by_id` against a real Postgres.
    ///
    /// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it. This used to
    /// pin that the route's month was summed from the ledger — on every
    /// request, for any route with a budget. It now pins the opposite: the
    /// spend is a column `record_usage` maintains, and the read is one
    /// primary-key lookup gated on the month the column names.
    #[tokio::test]
    async fn route_spend_is_its_column_read_as_zero_once_its_month_has_passed() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let insert = |name: &str, month: &str| {
            let db = db.clone();
            let name = format!("{name}-{}", Uuid::new_v4());
            let month = month.to_owned();
            async move {
                sqlx::query_scalar::<_, Uuid>(
                    "INSERT INTO route (id, name, tiers, default_mode, monthly_budget_usd,
                                        spent_usd, spent_month)
                     VALUES (gen_random_uuid(), $1,
                             '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb,
                             'managed', 500, 200.75,
                             CASE $2 WHEN 'this' THEN date_trunc('month', now())::date
                                     WHEN 'last' THEN (date_trunc('month', now()) - interval '1 month')::date
                                     ELSE NULL END)
                     RETURNING id",
                )
                .bind(name)
                .bind(month)
                .fetch_one(db.pool())
                .await
                .expect("insert route")
            }
        };
        let current = insert("current", "this").await;
        let stale = insert("stale", "last").await;
        let never = insert("never", "none").await;

        let spent = |id: Uuid| {
            let db = db.clone();
            async move {
                route_by_id(&db, id)
                    .await
                    .expect("load")
                    .expect("exists")
                    .spent_usd
            }
        };
        assert_eq!(
            spent(current).await,
            dec!(200.75),
            "this month's column, as is"
        );
        assert!(
            spent(stale).await.is_zero(),
            "a column naming last month reads as zero: the month rolled over"
        );
        assert!(spent(never).await.is_zero(), "never spent");
    }

    /// One ledger write moves all three counters, in one statement, and the
    /// monthly two reset at the boundary without a job.
    ///
    /// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
    #[tokio::test]
    async fn one_recorded_usage_debits_the_key_the_principal_and_the_route_together() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;
        let key = capped_key(&db, principal, route, format!("debit-{}", Uuid::new_v4())).await;

        let write = |cost: &str| UsageWrite {
            request_id: Uuid::new_v4(),
            attempt: 0,
            principal_id: Some(principal),
            api_key_id: Some(key),
            route_id: Some(route),
            account_id: Some(account.as_uuid()),
            model_id: "kimi-k2".to_owned(),
            tier: "cheap".to_owned(),
            selection_reason: "default".to_owned(),
            escalated_from_tier: None,
            escalation_gate: None,
            usage: oag_router::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            cost_usd: cost.parse().expect("decimal"),
            counterfactual_usd: Decimal::ZERO,
            counterfactual_model_id: None,
            counterfactual_api_usd: Decimal::ZERO,
            status: 200,
            latency_ms: Some(10),
            ttft_ms: None,
            streamed: false,
        };

        record_usage(&db, &write("1.25")).await.expect("record");
        record_usage(&db, &write("0.50")).await.expect("record");

        // THE FIX. This read is what the cap is enforced against, and it is
        // exactly as current as the ledger: not a five-minute-old snapshot,
        // not a SUM.
        let spend = spend_for(&db, key, principal).await.expect("spend");
        assert_eq!(spend.key_usd, dec!(1.75), "lifetime, on the key");
        assert_eq!(
            spend.principal_usd,
            dec!(1.75),
            "this month, on the principal"
        );
        let row = route_by_id(&db, route)
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(row.spent_usd, dec!(1.75), "this month, on the route");

        // The month rolls over. Nothing runs at midnight; the column simply
        // names a month that is not this one, and reads as zero — then the
        // first write of the new month resets it to that write alone, while
        // the key's lifetime counter carries on.
        sqlx::query(
            "UPDATE principal SET spent_month = (date_trunc('month', now()) - interval '1 month')::date
              WHERE id = $1",
        )
        .bind(principal)
        .execute(db.pool())
        .await
        .expect("age the principal's month");
        let rolled = spend_for(&db, key, principal).await.expect("spend");
        assert!(
            rolled.principal_usd.is_zero(),
            "last month's spend is not this month's"
        );
        assert_eq!(rolled.key_usd, dec!(1.75), "the key's cap is lifetime");

        record_usage(&db, &write("0.25")).await.expect("record");
        let fresh = spend_for(&db, key, principal).await.expect("spend");
        assert_eq!(
            fresh.principal_usd,
            dec!(0.25),
            "reset to this month's first write"
        );
        assert_eq!(fresh.key_usd, dec!(2.00));

        // The month never moves backwards. A write whose transaction began
        // before midnight, re-evaluated against a row the first write of
        // the new month has just committed, accumulates into that month
        // rather than overwriting it with its own cost and stamping the old
        // month back. Reproduced here from the row's side: a row already a
        // month ahead of `now()` is what such a write sees.
        sqlx::query(
            "UPDATE principal
                SET spent_usd = 5, spent_month = (date_trunc('month', now()) + interval '1 month')::date
              WHERE id = $1",
        )
        .bind(principal)
        .execute(db.pool())
        .await
        .expect("advance the principal's month");
        record_usage(&db, &write("0.25")).await.expect("record");
        let (ahead_usd, ahead_month): (Decimal, time::Date) =
            sqlx::query_as("SELECT spent_usd, spent_month FROM principal WHERE id = $1")
                .bind(principal)
                .fetch_one(db.pool())
                .await
                .expect("read the row");
        let next_month: time::Date =
            sqlx::query_scalar("SELECT (date_trunc('month', now()) + interval '1 month')::date")
                .fetch_one(db.pool())
                .await
                .expect("next month");
        assert_eq!(ahead_usd, dec!(5.25), "accumulated into the later month");
        assert_eq!(ahead_month, next_month, "and the month stayed where it was");

        // A key that no longer exists cannot spend as if uncapped.
        assert!(
            matches!(
                spend_for(&db, Uuid::new_v4(), principal).await,
                Err(Error::Unauthenticated)
            ),
            "a missing key is a refusal, not zeros"
        );
    }

    /// A ledger row as the previous release writes one: inserted, and no
    /// counter touched. What every old replica does for the whole rolling
    /// deploy after the counters exist.
    async fn old_binary_row(db: &Db, principal: Uuid, key: Uuid, route: Uuid, cost: &str) {
        sqlx::query(
            "INSERT INTO usage_event (
                 request_id, attempt, principal_id, api_key_id, route_id,
                 model_id, tier, selection_reason,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 cost_usd, counterfactual_usd, counterfactual_api_usd,
                 status, latency_ms, streamed)
             VALUES (gen_random_uuid(), 0, $1, $2, $3, 'kimi-k2', 'cheap', 'classified',
                     10, 5, 0, 0, $4, 0, $4, 200, 10, false)",
        )
        .bind(principal)
        .bind(key)
        .bind(route)
        .bind(cost.parse::<Decimal>().expect("decimal"))
        .execute(db.pool())
        .await
        .expect("insert an old-binary row");
    }

    async fn set_budgets(db: &Db, principal: Uuid, route: Uuid) {
        sqlx::query("UPDATE principal SET monthly_budget_usd = 100 WHERE id = $1")
            .bind(principal)
            .execute(db.pool())
            .await
            .expect("budget the principal");
        sqlx::query("UPDATE route SET monthly_budget_usd = 100 WHERE id = $1")
            .bind(route)
            .execute(db.pool())
            .await
            .expect("budget the route");
    }

    /// The rolling-deploy window, reproduced: rows the old binary wrote after
    /// the backfill are invisible to the cap until something reconciles.
    ///
    /// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
    #[tokio::test]
    async fn reconcile_catches_up_the_spend_an_old_binary_recorded() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;
        let key = capped_key(
            &db,
            principal,
            route,
            format!("reconcile-{}", Uuid::new_v4()),
        )
        .await;
        set_budgets(&db, principal, route).await;

        // The new binary's write debits; the old binary's does not.
        record_usage(
            &db,
            &UsageWrite {
                request_id: Uuid::new_v4(),
                attempt: 0,
                principal_id: Some(principal),
                api_key_id: Some(key),
                route_id: Some(route),
                account_id: Some(account.as_uuid()),
                model_id: "kimi-k2".to_owned(),
                tier: "cheap".to_owned(),
                selection_reason: "default".to_owned(),
                escalated_from_tier: None,
                escalation_gate: None,
                usage: oag_router::Usage::default(),
                cost_usd: dec!(1.00),
                counterfactual_usd: Decimal::ZERO,
                counterfactual_model_id: None,
                counterfactual_api_usd: Decimal::ZERO,
                status: 200,
                latency_ms: Some(10),
                ttft_ms: None,
                streamed: false,
            },
        )
        .await
        .expect("record");
        old_binary_row(&db, principal, key, route, "0.50").await;

        let before = spend_for(&db, key, principal).await.expect("spend");
        assert_eq!(
            before.principal_usd,
            dec!(1.00),
            "the old row is invisible to the cap"
        );

        let done = reconcile_monthly_spend(&db).await.expect("reconcile");
        assert!(done.principals >= 1 && done.routes >= 1, "{done:?}");

        let after = spend_for(&db, key, principal).await.expect("spend");
        assert_eq!(
            after.principal_usd,
            dec!(1.50),
            "the cap now sees the ledger"
        );
        let row = route_by_id(&db, route)
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(row.spent_usd, dec!(1.50));

        // A second pass is a no-op in effect: same number, not doubled.
        reconcile_monthly_spend(&db).await.expect("reconcile again");
        let again = spend_for(&db, key, principal).await.expect("spend");
        assert_eq!(again.principal_usd, dec!(1.50));

        // The month only moves forward, here as in the debit. A row already
        // stamped with a later month is left exactly as it is.
        sqlx::query(
            "UPDATE principal
                SET spent_usd = 5, spent_month = (date_trunc('month', now()) + interval '1 month')::date
              WHERE id = $1",
        )
        .bind(principal)
        .execute(db.pool())
        .await
        .expect("advance the month");
        reconcile_monthly_spend(&db).await.expect("reconcile");
        let (usd, ahead): (Decimal, bool) = sqlx::query_as(
            "SELECT spent_usd, spent_month > date_trunc('month', now())::date
               FROM principal WHERE id = $1",
        )
        .bind(principal)
        .fetch_one(db.pool())
        .await
        .expect("read");
        assert_eq!(usd, dec!(5));
        assert!(ahead, "a later month is not pulled back");
    }

    /// The race the row lock exists for: debits landing while a pass runs are
    /// never lost from the counter.
    ///
    /// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
    #[tokio::test]
    async fn reconcile_does_not_lose_a_debit_that_lands_while_it_runs() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 8).expect("connect");
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;
        let key = capped_key(&db, principal, route, format!("race-{}", Uuid::new_v4())).await;
        set_budgets(&db, principal, route).await;
        old_binary_row(&db, principal, key, route, "0.25").await;

        let write = move || UsageWrite {
            request_id: Uuid::new_v4(),
            attempt: 0,
            principal_id: Some(principal),
            api_key_id: Some(key),
            route_id: Some(route),
            account_id: Some(account.as_uuid()),
            model_id: "kimi-k2".to_owned(),
            tier: "cheap".to_owned(),
            selection_reason: "default".to_owned(),
            escalated_from_tier: None,
            escalation_gate: None,
            usage: oag_router::Usage::default(),
            cost_usd: dec!(0.10),
            counterfactual_usd: Decimal::ZERO,
            counterfactual_model_id: None,
            counterfactual_api_usd: Decimal::ZERO,
            status: 200,
            latency_ms: Some(10),
            ttft_ms: None,
            streamed: false,
        };

        let mut tasks = Vec::new();
        for _ in 0..24 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                record_usage(&db, &write()).await.expect("record");
            }));
        }
        for _ in 0..6 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                reconcile_monthly_spend(&db).await.expect("reconcile");
            }));
        }
        for task in tasks {
            task.await.expect("task");
        }

        let ledger: Decimal = sqlx::query_scalar(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_event
              WHERE principal_id = $1 AND occurred_at >= date_trunc('month', now())",
        )
        .bind(principal)
        .fetch_one(db.pool())
        .await
        .expect("sum");
        assert_eq!(ledger, dec!(2.65), "24 debits and the old row");
        let spend = spend_for(&db, key, principal).await.expect("spend");
        assert_eq!(
            spend.principal_usd, ledger,
            "every debit that landed mid-pass is kept"
        );
        let row = route_by_id(&db, route)
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(row.spent_usd, ledger);
    }

    /// Fixture: one principal, one route, one shared credential joined to it.
    async fn seed(db: &Db) -> (Uuid, Uuid, AccountId) {
        let tag = Uuid::new_v4();
        let principal: Uuid = sqlx::query_scalar(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')
             RETURNING id",
        )
        .bind(format!("{tag}@example.invalid"))
        .fetch_one(db.pool())
        .await
        .expect("principal");

        let route: Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers, default_mode)
             VALUES (gen_random_uuid(), $1, '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb, 'managed')
             RETURNING id",
        )
        .bind(format!("route-{tag}"))
        .fetch_one(db.pool())
        .await
        .expect("route");

        let account: Uuid = sqlx::query_scalar(
            "INSERT INTO account
                 (id, name, provider, kind, credentials_sealed, credentials_nonce)
             VALUES (gen_random_uuid(), $1, 'anthropic', 'api_key', '\\x00', '\\x00')
             RETURNING id",
        )
        .bind(format!("acct-{tag}"))
        .fetch_one(db.pool())
        .await
        .expect("account");

        sqlx::query("INSERT INTO account_route (account_id, route_id) VALUES ($1, $2)")
            .bind(account)
            .bind(route)
            .execute(db.pool())
            .await
            .expect("join");

        (principal, route, AccountId::from_uuid(account))
    }

    /// The three windows after one spend six hours ago and one just now: the five-hour window
    /// holds only the recent one and frees up when it ages out; the week holds both and frees
    /// up when the older one does; the month resets on the first.
    fn assert_windows(usage: &KeyUsage) {
        assert_eq!(
            usage.five_hour_usd,
            dec!(0.500000),
            "the six-hour-old spend is outside"
        );
        assert_eq!(usage.seven_day_usd, dec!(1.750000), "and inside the week");
        let now = OffsetDateTime::now_utc();
        let frees = usage
            .five_hour_frees_at
            .expect("a non-empty window frees up");
        let minutes = (frees - now).whole_minutes();
        assert!(
            (4 * 60 + 58..=5 * 60).contains(&minutes),
            "the five-hour window frees up when its oldest (just-now) spend ages out: {frees}"
        );
        let frees = usage
            .seven_day_frees_at
            .expect("a non-empty window frees up");
        let hours = (frees - now).whole_hours();
        assert!(
            (7 * 24 - 7..=7 * 24 - 5).contains(&hours),
            "the seven-day window frees up when the six-hour-old spend ages out: {frees}"
        );
        assert!(
            usage.month_resets_at > now,
            "the month resets on the first of next month"
        );
    }

    /// A capped key on `principal`, straight into the table — `mint_key` would
    /// do, but a test about the ledger should not depend on the mint path.
    async fn capped_key(db: &Db, principal: Uuid, route: Uuid, name: String) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id, quota_usd)
             VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4, $5)
             RETURNING id",
        )
        .bind(hash_key(&name))
        .bind(&name)
        .bind(principal)
        .bind(route)
        .bind(Some(dec!(5.00)))
        .fetch_one(db.pool())
        .await
        .expect("mint")
    }

    fn test_db() -> Option<Db> {
        let url = std::env::var("OAG_TEST_DATABASE_URL").ok()?;
        Some(Db::connect(&url, 2).expect("connect"))
    }

    /// A key's usage is its OWN ledger rows: another key on the same principal
    /// does not count, the month figure is the ledger's sum, and the lifetime
    /// counter is what the cap is enforced against. An id that is not a key is
    /// `None`, never a zeroed row.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one ledger, three windows, two keys: the setup is the test
    async fn key_usage_reads_one_keys_ledger_and_its_cap() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;
        let email: String = sqlx::query_scalar("SELECT email FROM principal WHERE id = $1")
            .bind(principal)
            .fetch_one(db.pool())
            .await
            .expect("principal email");

        let own = capped_key(
            &db,
            principal,
            route,
            format!("usage-own-{}", Uuid::new_v4()),
        )
        .await;
        let theirs = capped_key(
            &db,
            principal,
            route,
            format!("usage-theirs-{}", Uuid::new_v4()),
        )
        .await;

        let write = |key: Uuid, cost: &str, api: &str| UsageWrite {
            request_id: Uuid::new_v4(),
            attempt: 0,
            principal_id: Some(principal),
            api_key_id: Some(key),
            route_id: Some(route),
            account_id: Some(account.as_uuid()),
            model_id: "kimi-k2".to_owned(),
            tier: "cheap".to_owned(),
            selection_reason: "default".to_owned(),
            escalated_from_tier: None,
            escalation_gate: None,
            usage: oag_router::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            cost_usd: cost.parse().expect("decimal"),
            counterfactual_usd: Decimal::ZERO,
            counterfactual_model_id: None,
            // What the same tokens would cost at the model's list API price — for a seat, the
            // bill it displaced while `cost_usd` stays truthfully what it is.
            counterfactual_api_usd: api.parse().expect("decimal"),
            status: 200,
            latency_ms: Some(10),
            ttft_ms: None,
            streamed: false,
        };
        let early = write(own, "1.25", "2.00");
        record_usage(&db, &early).await.expect("record");
        record_usage(&db, &write(own, "0.50", "0.80"))
            .await
            .expect("record");
        record_usage(&db, &write(theirs, "9.00", "9.00"))
            .await
            .expect("record");
        // The first spend happened six hours ago: inside the week and the month, outside the
        // five-hour window.
        sqlx::query(
            "UPDATE usage_event SET occurred_at = now() - interval '6 hours' WHERE request_id = $1",
        )
        .bind(early.request_id)
        .execute(db.pool())
        .await
        .expect("backdate");

        let usage = key_usage(&db, own, Some(dec!(0.20)))
            .await
            .expect("usage")
            .expect("the key exists");
        assert_eq!(usage.key_id, own);
        assert_eq!(usage.principal_email, email);
        assert!(usage.active);
        assert_eq!(usage.quota_usd, Some(dec!(5.000000)));
        assert_eq!(
            usage.spent_usd,
            dec!(1.750000),
            "the counter the cap is enforced against"
        );
        assert_eq!(
            usage.month_to_date_usd,
            dec!(1.750000),
            "this key's rows only"
        );
        assert_eq!(usage.requests, 2);
        assert_windows(&usage);
        assert_eq!(
            usage.five_hour_requests, 1,
            "only the recent spend is inside five hours"
        );
        assert_eq!(usage.seven_day_requests, 2);
        assert_eq!(
            usage.month_counterfactual_usd,
            dec!(2.800000),
            "the list-price bill the same tokens would have carried"
        );
        assert_eq!(usage.five_hour_counterfactual_usd, dec!(0.800000));
        assert_eq!(usage.seven_day_counterfactual_usd, dec!(2.800000));
        // The rolling day holds both (six hours ago is inside it).
        assert_eq!(usage.day_usd, dec!(1.750000));
        assert_eq!(usage.day_requests, 2);
        assert_eq!(usage.day_counterfactual_usd, dec!(2.800000));
        assert!(usage.day_frees_at.is_some());
        // Points at R = 0.20: list price × 1e6 / 0.20, per request, summed.
        assert_eq!(
            usage.month_points,
            Some(14_000_000),
            "2.00 and 0.80 at list price"
        );
        assert_eq!(usage.five_hour_points, Some(4_000_000));
        assert_eq!(usage.day_points, Some(14_000_000));
        assert_eq!(usage.seven_day_points, Some(14_000_000));
        // Per model, inside the month and inside five hours.
        let by_model = key_usage_by_model(
            &db,
            own,
            UsageWindow::Month,
            Some(dec!(0.20)),
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("by model");
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].model_id, "kimi-k2");
        assert_eq!(by_model[0].requests, 2);
        assert_eq!(
            (by_model[0].input_tokens, by_model[0].output_tokens),
            (20, 10)
        );
        assert_eq!(by_model[0].cost_usd, dec!(1.750000));
        assert_eq!(by_model[0].list_usd, dec!(2.800000));
        assert_eq!(by_model[0].points, Some(14_000_000));
        let recent = key_usage_by_model(
            &db,
            own,
            UsageWindow::FiveHours,
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("by model");
        assert_eq!(recent[0].requests, 1);
        assert_eq!(
            recent[0].points, None,
            "no reference, no points — never zero"
        );
        // The batch: three keys, one query; the empty one is absent.
        let pool = points_for_keys(
            &db,
            &[own, theirs],
            UsageWindow::Month,
            dec!(0.20),
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("points");
        let of = |key: Uuid| pool.iter().find(|(k, _)| *k == key).map(|(_, p)| *p);
        assert_eq!(of(own), Some(14_000_000));
        assert_eq!(of(theirs), Some(45_000_000), "9.00 at list price over 0.20");

        let other = key_usage(&db, theirs, None)
            .await
            .expect("usage")
            .expect("the key exists");
        assert_eq!(other.month_to_date_usd, dec!(9.000000));
        assert_eq!(other.requests, 1);
        assert_eq!(other.month_points, None, "read without a reference");
        let empty_key = capped_key(
            &db,
            principal,
            route,
            format!("usage-empty-{}", Uuid::new_v4()),
        )
        .await;
        let empty = key_usage(&db, empty_key, Some(dec!(0.20)))
            .await
            .expect("usage")
            .expect("the key exists");
        assert_eq!(empty.five_hour_usd, dec!(0));
        assert_eq!(
            (
                empty.five_hour_requests,
                empty.seven_day_requests,
                empty.requests
            ),
            (0, 0, 0)
        );
        assert_eq!(empty.month_counterfactual_usd, dec!(0));
        assert_eq!(
            empty.month_points,
            Some(0),
            "a reference and no rows is zero points"
        );
        assert!(empty.day_frees_at.is_none());
        assert!(
            key_usage_by_model(
                &db,
                empty_key,
                UsageWindow::Day,
                Some(dec!(0.20)),
                OffsetDateTime::now_utc()
            )
            .await
            .expect("by model")
            .is_empty()
        );
        assert!(
            empty.five_hour_frees_at.is_none() && empty.seven_day_frees_at.is_none(),
            "an empty window has nothing to free up"
        );

        assert!(
            key_usage(&db, Uuid::new_v4(), None)
                .await
                .expect("usage")
                .is_none(),
            "an unknown id is None, not a zeroed row"
        );
    }

    #[tokio::test]
    async fn the_points_reference_is_one_row_the_admin_replaces() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        // Another test may have set it; what this proves is replace-in-place and the read-back.
        set_points_reference(&db, dec!(0.20)).await.expect("set");
        assert_eq!(points_reference(&db).await.expect("read"), Some(dec!(0.20)));
        set_points_reference(&db, dec!(0.25))
            .await
            .expect("replace");
        assert_eq!(points_reference(&db).await.expect("read"), Some(dec!(0.25)));
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM points_reference")
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(rows, 1, "one row, replaced, never a second");
        assert!(
            set_points_reference(&db, dec!(0)).await.is_err(),
            "the table refuses a price that is not positive even if a caller forgot to"
        );
    }

    #[test]
    fn a_window_starts_where_it_says_and_frees_when_its_oldest_spend_ages_out() {
        use time::macros::datetime;
        let now = datetime!(2026-09-03 10:30:00 UTC);
        assert_eq!(UsageWindow::parse("5h"), Some(UsageWindow::FiveHours));
        assert_eq!(UsageWindow::parse("24h"), Some(UsageWindow::Day));
        assert_eq!(UsageWindow::parse("7d"), Some(UsageWindow::SevenDays));
        assert_eq!(UsageWindow::parse(" month "), Some(UsageWindow::Month));
        assert_eq!(UsageWindow::parse("1d"), None);
        assert_eq!(
            UsageWindow::Day.since(now),
            datetime!(2026-09-02 10:30:00 UTC)
        );
        assert_eq!(
            UsageWindow::Month.since(now),
            datetime!(2026-09-01 00:00:00 UTC)
        );
        let oldest = datetime!(2026-09-03 08:00:00 UTC);
        assert_eq!(
            UsageWindow::FiveHours.frees_at(Some(oldest), now),
            Some(datetime!(2026-09-03 13:00:00 UTC))
        );
        assert_eq!(
            UsageWindow::FiveHours.frees_at(None, now),
            None,
            "empty: nothing to free"
        );
        assert_eq!(
            UsageWindow::Month.frees_at(None, now),
            Some(datetime!(2026-10-01 00:00:00 UTC))
        );
        assert_eq!(
            UsageWindow::Month.frees_at(None, datetime!(2026-12-15 00:00:00 UTC)),
            Some(datetime!(2027-01-01 00:00:00 UTC))
        );
    }

    /// The queries whose SELECT lists name every column by hand.
    ///
    /// `rows.rs` justifies hand-written `FromRow` structs on the grounds that a
    /// column mistake "surfaces as a runtime error on first query, which the
    /// integration tests catch" — but nothing exercised either query, so that
    /// claim was unbacked until now. Both SELECT lists were edited in this
    /// change, which is exactly when it needed to be true.
    #[tokio::test]
    async fn the_hand_written_account_selects_match_the_schema() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;

        let found = candidates(&db, route, "anthropic", principal)
            .await
            .expect("candidates must not fail on a column name");
        assert_eq!(
            found.len(),
            1,
            "the seeded credential should be a candidate"
        );

        let row = account_by_id(&db, account)
            .await
            .expect("account_by_id must not fail on a column name")
            .expect("exists");
        assert_eq!(row.provider, "anthropic");
        assert_eq!(row.max_concurrency, 8);
    }

    #[tokio::test]
    async fn route_channels_hides_what_the_caller_cannot_use() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;

        assert_eq!(
            route_channels(&db, route, principal).await.expect("list"),
            vec![("anthropic".to_owned(), "api_key".to_owned(), None)],
            "the kind rides along, so the listing knows which qualifiers to offer, \
             and the served set so it knows which models each will take"
        );

        // Disabled is an operator decision, not a transient state: advertising
        // a model nobody can reach moves the failure away from its cause.
        set_schedulable(&db, account, false).await.expect("disable");
        assert!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .is_empty()
        );
        set_schedulable(&db, account, true).await.expect("enable");

        // A credential bound to someone else must not show up here either.
        let other: Uuid = sqlx::query_scalar(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')
             RETURNING id",
        )
        .bind(format!("other-{}@example.invalid", Uuid::new_v4()))
        .fetch_one(db.pool())
        .await
        .expect("other principal");
        sqlx::query("UPDATE account SET owner_principal_id = $2 WHERE id = $1")
            .bind(account.as_uuid())
            .bind(other)
            .execute(db.pool())
            .await
            .expect("bind");
        assert!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .is_empty(),
            "another principal's personal credential is not this caller's to see"
        );
    }

    #[tokio::test]
    async fn route_channels_hides_a_spent_subscription_even_without_a_reserve() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;
        let id = account.as_uuid();

        // Unread remaining is unknown, not empty: a provider with no usage
        // endpoint must not vanish from the picker.
        assert_eq!(
            route_channels(&db, route, principal).await.expect("list"),
            vec![("anthropic".to_owned(), "api_key".to_owned(), None)]
        );

        sqlx::query("UPDATE account SET usage_remaining_pct = 50 WHERE id = $1")
            .bind(id)
            .execute(db.pool())
            .await
            .expect("half remaining");
        assert_eq!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .len(),
            1,
            "headroom and no reserve is still a live credential"
        );

        sqlx::query("UPDATE account SET usage_remaining_pct = 0 WHERE id = $1")
            .bind(id)
            .execute(db.pool())
            .await
            .expect("spent");
        assert!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .is_empty(),
            "a spent seat cannot serve today, reserve or not"
        );

        sqlx::query(
            "UPDATE account SET usage_remaining_pct = 20, usage_reserve_pct = 10 WHERE id = $1",
        )
        .bind(id)
        .execute(db.pool())
        .await
        .expect("above reserve");
        assert_eq!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .len(),
            1,
            "above the reserve is listed"
        );

        sqlx::query("UPDATE account SET usage_remaining_pct = 10 WHERE id = $1")
            .bind(id)
            .execute(db.pool())
            .await
            .expect("at reserve");
        assert!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .is_empty(),
            "at the reserve line is held back"
        );
    }

    #[tokio::test]
    async fn route_channel_status_includes_what_the_listing_hides() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;
        let id = account.as_uuid();

        assert_eq!(
            route_channel_status(&db, route, principal)
                .await
                .expect("status")
                .len(),
            1,
            "a live credential is visible to both the picker and the panel"
        );

        set_schedulable(&db, account, false).await.expect("disable");
        assert!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .is_empty(),
            "disabled is hidden from the picker"
        );
        let disabled = route_channel_status(&db, route, principal)
            .await
            .expect("status");
        assert_eq!(disabled.len(), 1);
        assert!(!disabled[0].schedulable);
        set_schedulable(&db, account, true).await.expect("enable");

        sqlx::query(
            "UPDATE account SET usage_remaining_pct = 8, usage_reserve_pct = 15 WHERE id = $1",
        )
        .bind(id)
        .execute(db.pool())
        .await
        .expect("reserve");
        assert!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .is_empty(),
            "reserved is hidden from the picker"
        );
        let reserved = &route_channel_status(&db, route, principal)
            .await
            .expect("status")[0];
        assert_eq!(reserved.usage_remaining_pct, Some(dec!(8)));
        assert_eq!(reserved.usage_reserve_pct, Some(15));

        let until = OffsetDateTime::now_utc() + time::Duration::days(15);
        sqlx::query(
            "UPDATE account
                SET usage_remaining_pct = NULL, usage_reserve_pct = NULL,
                    rate_limited_until = $2
              WHERE id = $1",
        )
        .bind(id)
        .bind(until)
        .execute(db.pool())
        .await
        .expect("rate limit");
        assert!(
            route_channels(&db, route, principal)
                .await
                .expect("list")
                .is_empty(),
            "rate-limited is hidden from the picker"
        );
        let limited = &route_channel_status(&db, route, principal)
            .await
            .expect("status")[0];
        assert!(
            limited
                .rate_limited_until
                .is_some_and(|t| t > OffsetDateTime::now_utc())
        );

        // A credential bound to someone else is not this caller's to diagnose,
        // the same way it is not theirs to pick.
        let other: Uuid = sqlx::query_scalar(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')
             RETURNING id",
        )
        .bind(format!("other-{}@example.invalid", Uuid::new_v4()))
        .fetch_one(db.pool())
        .await
        .expect("other principal");
        sqlx::query("UPDATE account SET owner_principal_id = $2 WHERE id = $1")
            .bind(id)
            .bind(other)
            .execute(db.pool())
            .await
            .expect("bind");
        assert!(
            route_channel_status(&db, route, principal)
                .await
                .expect("status")
                .is_empty(),
            "another principal's personal credential is not this caller's to see"
        );
    }

    #[tokio::test]
    async fn clearing_a_cooldown_leaves_the_providers_own_backoff_alone() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (_, _, account) = seed(&db).await;

        sqlx::query(
            "UPDATE account
                SET cooldown_until = now() + interval '1 hour',
                    cooldown_reason = 'test',
                    rate_limited_until = now() + interval '1 hour',
                    window_resets_at = now() + interval '1 hour'
              WHERE id = $1",
        )
        .bind(account.as_uuid())
        .execute(db.pool())
        .await
        .expect("cool down");

        clear_cooldown(&db, account).await.expect("clear");

        let row = account_by_id(&db, account)
            .await
            .expect("load")
            .expect("row");
        assert!(row.cooldown_until.is_none());
        assert!(
            row.rate_limited_until.is_some(),
            "rate_limited_until is the provider's own Retry-After; discarding it \
             fleet-wide turns a throttle into an account action"
        );
        assert!(row.window_resets_at.is_some());
    }

    #[tokio::test]
    async fn admin_authority_is_carried_by_the_key_not_the_principal() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, _) = seed(&db).await;

        let mint = |key: &'static str, admin: bool| {
            let db = &db;
            async move {
                sqlx::query(
                    "INSERT INTO api_key
                         (id, key_hash, key_prefix, name, principal_id, route_id, admin)
                     VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4, $5)",
                )
                .bind(hash_key(key))
                .bind(key)
                .bind(principal)
                .bind(route)
                .bind(admin)
                .execute(db.pool())
                .await
                .expect("mint");
            }
        };

        let plain = format!("plain-{}", Uuid::new_v4());
        let elevated = format!("admin-{}", Uuid::new_v4());
        let plain: &'static str = Box::leak(plain.into_boxed_str());
        let elevated: &'static str = Box::leak(elevated.into_boxed_str());
        mint(plain, false).await;
        mint(elevated, true).await;

        assert!(
            !authenticate(&db, plain)
                .await
                .expect("auth")
                .expect("found")
                .admin,
            "an inference key must not carry admin authority just because its \
             principal is an admin"
        );
        assert!(
            authenticate(&db, elevated)
                .await
                .expect("auth")
                .expect("found")
                .admin
        );
    }

    async fn insert_named_service(db: &Db, name: &str) -> ServiceRow {
        insert_service(
            db,
            &NewService {
                id: Uuid::now_v7(),
                name,
                kind: "sandbox",
                base_url: "http://127.0.0.1:9",
                health_path: "/health",
                dashboard_url: Some("http://127.0.0.1:9/ui"),
                auth_ref: None,
            },
        )
        .await
        .expect("insert service")
    }

    #[tokio::test]
    async fn the_service_catalog_round_trips_and_records_health() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let tag = Uuid::new_v4();
        let name = format!("orgo-{tag}");

        let row = insert_named_service(&db, &name).await;
        assert!(row.enabled);
        assert!(row.last_ok.is_none());
        assert!(row.last_error.is_none());

        let listed = list_services(&db).await.expect("list");
        assert!(
            listed.iter().any(|s| s.id == row.id),
            "inserted service must appear in the catalog"
        );

        let ok = record_service_health(&db, row.id, true, None)
            .await
            .expect("record ok")
            .expect("exists");
        assert!(ok.last_ok.is_some());
        assert!(ok.last_error.is_none());

        let bad = record_service_health(&db, row.id, false, Some("health returned HTTP 503"))
            .await
            .expect("record err")
            .expect("exists");
        assert_eq!(bad.last_ok, ok.last_ok, "a failed probe must keep last_ok");
        assert_eq!(bad.last_error.as_deref(), Some("health returned HTTP 503"));

        set_service_enabled(&db, row.id, false)
            .await
            .expect("disable")
            .expect("exists");
        let disabled = service_by_id(&db, row.id)
            .await
            .expect("load")
            .expect("exists");
        assert!(!disabled.enabled);

        let updated = update_service(
            &db,
            row.id,
            &ServiceUpdate {
                name: &name,
                kind: "browser",
                base_url: "http://127.0.0.1:19",
                health_path: "/ready",
                dashboard_url: None,
                auth_ref: None,
                enabled: true,
            },
        )
        .await
        .expect("update")
        .expect("exists");
        assert_eq!(updated.kind, "browser");
        assert!(updated.enabled);
        assert!(updated.dashboard_url.is_none());
    }

    #[tokio::test]
    async fn a_duplicate_service_name_is_a_config_error() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let name = format!("dup-{}", Uuid::new_v4());
        insert_named_service(&db, &name).await;
        let err = insert_service(
            &db,
            &NewService {
                id: Uuid::now_v7(),
                name: &name,
                kind: "tool",
                base_url: "http://127.0.0.1:9",
                health_path: "/health",
                dashboard_url: None,
                auth_ref: None,
            },
        )
        .await
        .expect_err("duplicate name");
        assert!(
            matches!(err, Error::Config(_)),
            "unique name must fail as Config, not Internal: {err}"
        );
    }

    #[tokio::test]
    async fn auth_ref_must_point_at_an_existing_credential() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let err = insert_service(
            &db,
            &NewService {
                id: Uuid::now_v7(),
                name: &format!("ref-{}", Uuid::new_v4()),
                kind: "tool",
                base_url: "http://127.0.0.1:9",
                health_path: "/health",
                dashboard_url: None,
                auth_ref: Some(Uuid::now_v7()),
            },
        )
        .await
        .expect_err("missing credential");
        assert!(
            matches!(err, Error::Config(_)),
            "a dangling auth_ref is a config error: {err}"
        );
    }

    /// One client request can pay for several attempts, and every one of them
    /// reaches the ledger.
    ///
    /// 0014 contracted the key onto `(request_id, attempt)`, which is what the
    /// expand half in 0003 was built for. Before it, the primary key on
    /// `request_id` alone admitted the first row and `ON CONFLICT DO NOTHING`
    /// silently dropped the rest — so an abandoned attempt never once landed,
    /// and a lost one landed only by displacing the answer the client got.
    ///
    /// What has to keep holding across that change is idempotence: a replayed
    /// write of the same `(request_id, attempt)` is still a no-op, because that
    /// is what makes a retried ledger write safe.
    #[tokio::test]
    async fn both_attempts_of_one_request_reach_the_ledger() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;

        // The key this release contracted onto. Asserted rather than assumed,
        // because every row below lands or is dropped according to it.
        let pk: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint
             WHERE conname = 'usage_event_pkey'",
        )
        .fetch_one(db.pool())
        .await
        .expect("the ledger has a primary key");
        assert_eq!(pk, "PRIMARY KEY (request_id, attempt)");

        let raw = format!("meter-{}", Uuid::new_v4());
        let key: Uuid = sqlx::query_scalar(
            "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id)
             VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4)
             RETURNING id",
        )
        .bind(hash_key(&raw))
        .bind(&raw)
        .bind(principal)
        .bind(route)
        .fetch_one(db.pool())
        .await
        .expect("mint");

        // One client request, two attempts: a cheap answer that tripped a
        // quality gate, and the retry a rung up that was actually served.
        let request_id = Uuid::new_v4();
        let write = |attempt: i16, reason: &str, cost: &str| UsageWrite {
            request_id,
            attempt,
            principal_id: Some(principal),
            api_key_id: Some(key),
            route_id: Some(route),
            account_id: Some(account.as_uuid()),
            model_id: "kimi-k2".to_owned(),
            tier: "cheap".to_owned(),
            selection_reason: reason.to_owned(),
            escalated_from_tier: None,
            escalation_gate: Some("Refusal".to_owned()),
            usage: oag_router::Usage {
                input_tokens: 1_000,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            cost_usd: cost.parse().expect("decimal"),
            counterfactual_usd: Decimal::ZERO,
            counterfactual_model_id: None,
            counterfactual_api_usd: Decimal::ZERO,
            status: 200,
            latency_ms: Some(10),
            ttft_ms: None,
            streamed: false,
        };

        // The served row still goes first, which is no longer load-bearing for
        // survival but is the order the request path writes in.
        record_usage(&db, &write(1, "escalated", "1.75"))
            .await
            .expect("served attempt");
        record_usage(&db, &write(0, "abandoned", "0.25"))
            .await
            .expect("abandoned attempt");

        let reasons: Vec<String> = sqlx::query_scalar(
            "SELECT selection_reason FROM usage_event
              WHERE request_id = $1 ORDER BY attempt",
        )
        .bind(request_id)
        .fetch_all(db.pool())
        .await
        .expect("read back");

        assert_eq!(
            reasons,
            vec!["abandoned".to_owned(), "escalated".to_owned()],
            "both attempts are in the ledger, each under its own dispatch number"
        );

        // Idempotence is the reason the conflict clause is there in the first
        // place, and it had to outlive the key change.
        record_usage(&db, &write(1, "escalated", "1.75"))
            .await
            .expect("replay");
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE request_id = $1")
                .bind(request_id)
                .fetch_one(db.pool())
                .await
                .expect("count");
        assert_eq!(rows, 2, "a replayed write is still a no-op");
    }

    /// A key with no spend yet, and a way to build ledger writes against it.
    async fn metered_key(db: &Db) -> (Uuid, impl Fn(Uuid, i16, &str, &str) -> UsageWrite) {
        let (principal, route, account) = seed(db).await;
        let raw = format!("meter-{}", Uuid::new_v4());
        let key: Uuid = sqlx::query_scalar(
            "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id)
             VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4)
             RETURNING id",
        )
        .bind(hash_key(&raw))
        .bind(&raw)
        .bind(principal)
        .bind(route)
        .fetch_one(db.pool())
        .await
        .expect("mint");

        let build = move |request_id: Uuid, attempt: i16, reason: &str, cost: &str| UsageWrite {
            request_id,
            attempt,
            principal_id: Some(principal),
            api_key_id: Some(key),
            route_id: Some(route),
            account_id: Some(account.as_uuid()),
            model_id: "kimi-k2".to_owned(),
            tier: "cheap".to_owned(),
            selection_reason: reason.to_owned(),
            escalated_from_tier: None,
            escalation_gate: None,
            usage: oag_router::Usage {
                input_tokens: 1_000,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            cost_usd: cost.parse().expect("decimal"),
            counterfactual_usd: Decimal::ZERO,
            counterfactual_model_id: None,
            counterfactual_api_usd: Decimal::ZERO,
            status: 200,
            latency_ms: Some(10),
            ttft_ms: None,
            streamed: false,
        };

        (key, build)
    }

    /// The denormalised counter the quota check reads.
    async fn key_spend(db: &Db, key: Uuid) -> Decimal {
        sqlx::query_scalar("SELECT spent_usd FROM api_key WHERE id = $1")
            .bind(key)
            .fetch_one(db.pool())
            .await
            .expect("read spend")
    }

    /// Idempotence has to cover the debit, not just the row.
    ///
    /// Metering retries: the write can fail after the row lands, and the caller
    /// replays it. `ON CONFLICT DO NOTHING` makes the second insert a no-op, so
    /// the spend it carries has already been counted — charging for it again
    /// walks a key toward its quota on nothing but a retry.
    #[tokio::test]
    async fn a_replayed_write_does_not_debit_the_key_twice() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (key, write) = metered_key(&db).await;

        let request_id = Uuid::new_v4();
        record_usage(&db, &write(request_id, 0, "classified", "1.50"))
            .await
            .expect("first write");
        assert_eq!(key_spend(&db, key).await, dec!(1.50));

        record_usage(&db, &write(request_id, 0, "classified", "1.50"))
            .await
            .expect("replay");
        assert_eq!(
            key_spend(&db, key).await,
            dec!(1.50),
            "the replay added no ledger row, so it must add no spend either"
        );
    }

    /// The debit follows the row, and now every attempt has one.
    ///
    /// `SUM(cost_usd)` over the ledger and `api_key.spent_usd` are two views of
    /// the same money. Before 0014 the second attempt's row was dropped and its
    /// debit had to be dropped with it, or the two would disagree with nothing
    /// to reconcile against. Now both rows land, so both debits must.
    #[tokio::test]
    async fn every_attempt_that_lands_debits_the_key() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");

        let (key, write) = metered_key(&db).await;
        let request_id = Uuid::new_v4();

        record_usage(&db, &write(request_id, 1, "escalated", "1.75"))
            .await
            .expect("served attempt");
        record_usage(&db, &write(request_id, 0, "abandoned", "0.25"))
            .await
            .expect("abandoned attempt");

        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE request_id = $1")
                .bind(request_id)
                .fetch_one(db.pool())
                .await
                .expect("count");
        assert_eq!(rows, 2, "both attempts are rows");
        assert_eq!(
            key_spend(&db, key).await,
            dec!(2.00),
            "the provider generated and invoiced both, so the key pays for both"
        );

        // And a replay of either still moves nothing.
        record_usage(&db, &write(request_id, 0, "abandoned", "0.25"))
            .await
            .expect("replay");
        assert_eq!(key_spend(&db, key).await, dec!(2.00), "replay is a no-op");
    }

    /// A lost stream and the retry that replaced it: two rows, one request.
    ///
    /// The credential generated an answer and the connection died before it was
    /// whole; another credential served the retry. The provider will invoice
    /// both generations, so both are rows and both are money — but the client
    /// made one request, and every count over the ledger has to keep saying so.
    ///
    /// That split is the whole reason the request counts filter on
    /// `selection_reason` and the spend sums do not. Before 0014 the question
    /// could not arise: the lost row displaced the served one, so the ledger
    /// held a single row that was 502, cost the retry nothing, and carried no
    /// counterfactual — the answer the client actually got was unbillable.
    #[tokio::test]
    async fn a_lost_attempt_and_its_retry_are_two_rows_but_one_request() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");

        let (key, build) = metered_key(&db).await;
        let request_id = Uuid::new_v4();

        // Attempt 0: generated, then the stream died. 502, and no
        // counterfactual — there is one baseline per client request and the
        // served row below is the row that carries it.
        let mut lost = build(request_id, 0, "lost", "0.40");
        lost.status = 502;

        // Attempt 1: what the client was served, priced against the ladder's
        // ceiling like any other served answer.
        let mut served = build(request_id, 1, "classified", "1.10");
        served.counterfactual_usd = dec!(9.00);
        served.counterfactual_model_id = Some("anthropic/claude-opus-5".to_owned());

        record_usage(&db, &served).await.expect("served attempt");
        record_usage(&db, &lost).await.expect("lost attempt");

        let rows: Vec<(i16, String, i16, Decimal, Decimal)> = sqlx::query_as(
            "SELECT attempt, selection_reason, status, cost_usd, counterfactual_usd
               FROM usage_event WHERE request_id = $1 ORDER BY attempt",
        )
        .bind(request_id)
        .fetch_all(db.pool())
        .await
        .expect("read back");

        assert_eq!(
            rows,
            vec![
                (0, "lost".to_owned(), 502, dec!(0.40), Decimal::ZERO),
                (1, "classified".to_owned(), 200, dec!(1.10), dec!(9.00)),
            ],
            "the lost attempt sits beside the served one rather than in place of it"
        );

        // Both were generated, so both are debited.
        assert_eq!(
            key_spend(&db, key).await,
            dec!(1.50),
            "the key pays for the generation that was lost as well as the one served"
        );

        let usage = key_usage(&db, key, None)
            .await
            .expect("read the key's usage")
            .expect("the key exists");
        assert_eq!(
            usage.month_to_date_usd,
            dec!(1.50),
            "spend counts every attempt: leaving the lost one out is what made \
             failover look free"
        );
        assert_eq!(
            usage.requests, 1,
            "and the request count does not: one client request was made, however \
             many generations it took to answer it"
        );
    }

    /// A catalog row as a seed builds one: no label, because a seed has no
    /// opinion about what to call anything.
    fn seed_model(id: &str, input: Decimal) -> ModelRow {
        ModelRow {
            id: id.to_owned(),
            provider: "xai".to_owned(),
            upstream_name: "grok-4.6".to_owned(),
            input_per_mtok: input,
            output_per_mtok: input * Decimal::from(4),
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
            context_window: 131_072,
            max_output_tokens: 8_192,
            supports_vision: false,
            supports_tools: false,
            supports_reasoning: false,
            supports_prompt_cache: false,
            display_label: None,
        }
    }

    #[test]
    fn the_upsert_refreshes_prices_without_naming_the_label_column() {
        // The guard against the whole failure, readable without a database:
        // `display_label` may appear in the INSERT, never in the conflict
        // branch. The moment it joins that list, every re-seed writes NULL over
        // whatever an operator called the model — and a nightly price sync
        // makes renaming look like it silently stopped working.
        let conflict = UPSERT_MODEL_SQL
            .split_once("DO UPDATE SET")
            .expect("the upsert has a conflict branch")
            .1;
        // The assignments alone: comments stripped, because the one above the
        // WHERE clause explains this very rule and names the column while doing
        // it, and cut at the WHERE, which reads `is_override` on purpose.
        let assignments: String = conflict
            .lines()
            .take_while(|l| !l.trim_start().starts_with("WHERE"))
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !assignments.contains("display_label"),
            "a refresh must not carry a label: {assignments}"
        );
        assert!(
            !assignments.contains("is_override"),
            "nor the override flag it is modelled on: {assignments}"
        );
        assert!(
            assignments.contains("input_per_mtok"),
            "the prices really are refreshed: {assignments}"
        );
    }

    #[tokio::test]
    async fn an_operators_label_outlives_a_reseed_and_a_price_sync() {
        // The reason the column is not in the conflict branch, end to end. An
        // operator renames a model once; a nightly LiteLLM seed and a provider
        // price sync both run over it afterwards, and neither knows the name
        // exists. If either carried the column, the rename would last until the
        // next tick and nobody would connect the two.
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");

        let id = format!("xai/grok-{}", Uuid::new_v4());
        upsert_model(&db, &seed_model(&id, dec!(3)), false)
            .await
            .expect("seed");

        let labelled = set_model_label(&db, &id, Some("Grok, the fast one"))
            .await
            .expect("label");
        assert_eq!(labelled.as_deref(), Some(id.as_str()));

        // A second seed, exactly as `oag admin seed-catalog` runs it.
        upsert_model(&db, &seed_model(&id, dec!(5)), false)
            .await
            .expect("reseed");
        // And a native price sync, which takes the other write path.
        assert!(
            update_model_prices(&db, &id, dec!(7), dec!(28), Some(dec!(0.7)))
                .await
                .expect("reprice")
        );

        let row = catalog(&db)
            .await
            .expect("catalog")
            .into_iter()
            .find(|m| m.id == id)
            .expect("the row is still there");
        assert_eq!(
            row.display_label.as_deref(),
            Some("Grok, the fast one"),
            "a seed and a sync both know nothing about names"
        );
        // The prices did move, so this is not a row nothing touched.
        assert_eq!(row.input_per_mtok, dec!(7));

        // And clearing it is a distinct state from naming it the derived
        // default: the row goes back to following the provider's spelling.
        set_model_label(&db, &id, None).await.expect("clear");
        let row = catalog(&db)
            .await
            .expect("catalog")
            .into_iter()
            .find(|m| m.id == id)
            .expect("row");
        assert_eq!(row.display_label, None);
        assert_eq!(row.derived_label(), "xAI: grok-4.6");

        assert_eq!(
            set_model_label(&db, "xai/not-a-model", Some("x"))
                .await
                .expect("query"),
            None,
            "renaming a model that does not exist is the caller's 404"
        );
    }

    #[tokio::test]
    async fn the_catalog_select_matches_the_schema() {
        // `rows.rs` takes hand-written `FromRow` structs on the grounds that a
        // column mistake shows up as a runtime error on the first query. That
        // is only true if something runs the query, and this SELECT grew a
        // column in this change.
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let id = format!("xai/grok-{}", Uuid::new_v4());
        upsert_model(&db, &seed_model(&id, dec!(3)), false)
            .await
            .expect("seed");

        let rows = catalog(&db)
            .await
            .expect("catalog must not fail on a column name");
        assert!(rows.iter().any(|m| m.id == id));
    }
}
