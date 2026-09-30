//! Upstream accounts: lookup, scheduling state, cooldowns and sealed credentials.

use crate::Db;
use crate::rows::AccountRow;
use oag_core::{AccountId, Error, Result};
use time::OffsetDateTime;
use uuid::Uuid;

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

/// The credential to read an endpoint's model list with: the one `name`d,
/// whether or not it is schedulable, or else the endpoint's first schedulable
/// one, lowest priority first.
pub async fn endpoint_account(
    db: &Db,
    endpoint: &str,
    name: Option<&str>,
) -> Result<Option<AccountRow>> {
    sqlx::query_as::<_, AccountRow>(
        r"
        SELECT id, name, provider, kind, credentials_sealed, credentials_nonce,
               token_version, token_expires_at, owner_principal_id, proxy_url,
               priority, max_concurrency, schedulable, cooldown_until,
               rate_limited_until, window_resets_at,
               usage_remaining_pct, usage_reserve_pct, last_used_at
        FROM account
        WHERE provider = $1
          AND (($2::text IS NULL AND schedulable) OR name = $2)
        ORDER BY priority, name
        LIMIT 1
        ",
    )
    .bind(endpoint)
    .bind(name)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("finding a credential for endpoint {endpoint}: {e}")))
}

/// API keys the usage poller asks for the models they serve: schedulable, and
/// filed under an endpoint whose `discover_models` is set.
pub async fn discovering_endpoint_accounts(db: &Db) -> Result<Vec<AccountRow>> {
    sqlx::query_as::<_, AccountRow>(
        r"
        SELECT a.id, a.name, a.provider, a.kind, a.credentials_sealed, a.credentials_nonce,
               a.token_version, a.token_expires_at, a.owner_principal_id, a.proxy_url,
               a.priority, a.max_concurrency, a.schedulable, a.cooldown_until,
               a.rate_limited_until, a.window_resets_at,
               a.usage_remaining_pct, a.usage_reserve_pct, a.last_used_at
        FROM account a
        JOIN endpoint e ON e.name = a.provider
        WHERE e.discover_models AND a.kind = 'api_key' AND a.schedulable
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading endpoint keys to discover: {e}")))
}

/// Forget the served set of every key whose endpoint no longer discovers its
/// models, and say how many were forgotten.
///
/// Discovery is the only writer of an endpoint key's `served_models`, and a
/// set it stopped maintaining is one that goes stale: it goes on hiding every
/// model the endpoint gains after it, with nothing left to correct it. NULL is
/// "never asked", which lists what the catalog holds for the endpoint, and that
/// is what turning discovery off means. A built-in provider's keys have no
/// endpoint row, and are never touched.
pub async fn forget_undiscovered_served_models(db: &Db) -> Result<u64> {
    sqlx::query(
        r"
        UPDATE account a
           SET served_models = NULL, served_models_at = NULL, updated_at = now()
          FROM endpoint e
         WHERE e.name = a.provider AND NOT e.discover_models AND a.served_models IS NOT NULL
        ",
    )
    .execute(db.pool())
    .await
    .map(|done| done.rows_affected())
    .map_err(|e| Error::Internal(format!("forgetting undiscovered served models: {e}")))
}

/// [`schedulable_oauth_accounts`]' statement, a constant so a test can run it
/// beside a row the schema no longer lets anything create.
pub(super) const SCHEDULABLE_OAUTH_SQL: &str = r"
        SELECT id, name, provider, kind, credentials_sealed, credentials_nonce,
               token_version, token_expires_at, owner_principal_id, proxy_url,
               priority, max_concurrency, schedulable, cooldown_until,
               rate_limited_until, window_resets_at,
               usage_remaining_pct, usage_reserve_pct, last_used_at
        FROM account WHERE kind = 'oauth' AND schedulable
          -- An owner-less seat serves no one, so nothing reads or refreshes
          -- it: a refresh would rotate its token for no request, and a seat
          -- nobody owns is not one to spend the owner's quota checking.
          AND owner_principal_id IS NOT NULL
        ";

/// Every OAuth (subscription seat) account, for the usage poller to sweep.
///
/// Not filtered by route or principal like `candidates`: the poller reads a
/// seat's remaining quota regardless of who may use it. Disabled seats are
/// skipped — polling one nobody will schedule spends a request for nothing.
pub async fn schedulable_oauth_accounts(db: &Db) -> Result<Vec<AccountRow>> {
    sqlx::query_as::<_, AccountRow>(SCHEDULABLE_OAUTH_SQL)
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

/// A subscription seat whose owner holds more than one live inference key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatWithManyKeys {
    pub seat: String,
    pub owner_email: String,
    pub keys: i64,
}

/// Seats whose owner could be handing them to other people.
///
/// A seat serves only its owner, but "its owner" is a principal, and a
/// principal can mint any number of keys. Several keys are fine when they are
/// all one person's (a laptop and a CI job); a coworker's key on the owner's
/// principal is the seat being shared, which the schema cannot tell apart. So
/// this is reported, not refused. Admin keys are not counted: `init` mints one
/// beside every operator's inference key, and it cannot make model calls on
/// anyone's behalf that the operator could not.
///
/// `principal` narrows it to one owner, which is what minting a key asks.
pub async fn seats_with_many_keys(
    db: &Db,
    principal: Option<Uuid>,
) -> Result<Vec<SeatWithManyKeys>> {
    sqlx::query_as::<_, (String, String, i64)>(
        r"
        SELECT a.name, p.email, count(k.id)
        FROM account a
        JOIN principal p ON p.id = a.owner_principal_id
        JOIN api_key k ON k.principal_id = p.id
                      AND k.active AND NOT k.admin
                      AND (k.expires_at IS NULL OR k.expires_at > now())
        WHERE a.kind = 'oauth'
          AND ($1::uuid IS NULL OR a.owner_principal_id = $1)
        GROUP BY a.name, p.email
        HAVING count(k.id) > 1
        ORDER BY a.name
        ",
    )
    .bind(principal)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("counting seat owners' keys: {e}")))
    .map(|rows| {
        rows.into_iter()
            .map(|(seat, owner_email, keys)| SeatWithManyKeys {
                seat,
                owner_email,
                keys,
            })
            .collect()
    })
}
