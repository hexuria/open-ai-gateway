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
