//! Queries.

use crate::Db;
use crate::rows::{AccountRow, AuthContext, ModelRow, RouteRow, UsageWrite};
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

    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            Uuid,
            Option<String>,
            Option<Decimal>,
            Decimal,
            Option<Decimal>,
            Decimal,
            Decimal,
        ),
    >(
        r"
        SELECT k.id, k.principal_id, k.route_id, k.floor_tier,
               k.quota_usd, k.spent_usd,
               p.monthly_budget_usd, p.hard_stop_multiple,
               COALESCE((
                   SELECT SUM(u.cost_usd) FROM usage_event u
                   WHERE u.principal_id = p.id
                     AND u.occurred_at >= date_trunc('month', now())
               ), 0)
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
        spent_usd: r.5,
        principal_budget_usd: r.6,
        principal_hard_stop_multiple: r.7,
        principal_spent_usd: r.8,
    }))
}

pub async fn route_by_id(db: &Db, id: Uuid) -> Result<Option<RouteRow>> {
    sqlx::query_as::<_, RouteRow>(
        "SELECT id, name, tiers, default_mode, floor_tier, rpm_limit, monthly_budget_usd, active
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
               a.owner_principal_id, a.proxy_url, a.priority, a.max_concurrency, a.weight,
               a.schedulable, a.cooldown_until, a.rate_limited_until, a.window_resets_at,
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
               priority, max_concurrency, weight, schedulable, cooldown_until,
               rate_limited_until, window_resets_at, last_used_at
        FROM account WHERE id = $1
        ",
    )
    .bind(id.as_uuid())
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading account: {e}")))
}

pub async fn touch_account(db: &Db, id: AccountId) -> Result<()> {
    sqlx::query("UPDATE account SET last_used_at = now() WHERE id = $1")
        .bind(id.as_uuid())
        .execute(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("touching account: {e}")))?;
    Ok(())
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

/// Append to the usage ledger.
///
/// `ON CONFLICT DO NOTHING` on the request id makes metering idempotent: a
/// retried write after a partial failure conflicts instead of billing twice.
pub async fn record_usage(db: &Db, w: &UsageWrite) -> Result<()> {
    sqlx::query(
        r"
        INSERT INTO usage_event (
            request_id, principal_id, api_key_id, route_id, account_id,
            model_id, tier, selection_reason, escalated_from_tier, escalation_gate,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            cost_usd, counterfactual_usd, counterfactual_model_id,
            status, latency_ms, ttft_ms, streamed
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)
        ON CONFLICT (request_id) DO NOTHING
        ",
    )
    .bind(w.request_id)
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
    .bind(w.status)
    .bind(w.latency_ms)
    .bind(w.ttft_ms)
    .bind(w.streamed)
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("recording usage: {e}")))?;

    // Key spend is denormalised for the quota check, which must not run a
    // SUM over the ledger on every request.
    sqlx::query(
        "UPDATE api_key SET spent_usd = spent_usd + $2, last_used_at = now() WHERE id = $1",
    )
    .bind(w.api_key_id)
    .bind(w.cost_usd)
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("updating key spend: {e}")))?;

    Ok(())
}

/// The whole model catalog.
pub async fn catalog(db: &Db) -> Result<Vec<ModelRow>> {
    sqlx::query_as::<_, ModelRow>(
        r"
        SELECT id, provider, upstream_name, input_per_mtok, output_per_mtok,
               cache_read_per_mtok, cache_write_per_mtok, context_window,
               max_output_tokens, supports_vision, supports_tools,
               supports_reasoning, supports_prompt_cache
        FROM model_catalog
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading catalog: {e}")))
}

/// Insert or update a catalog entry, never clobbering an operator override.
pub async fn upsert_model(db: &Db, m: &ModelRow, is_override: bool) -> Result<()> {
    sqlx::query(
        r"
        INSERT INTO model_catalog (
            id, provider, upstream_name, input_per_mtok, output_per_mtok,
            cache_read_per_mtok, cache_write_per_mtok, context_window, max_output_tokens,
            supports_vision, supports_tools, supports_reasoning, supports_prompt_cache,
            is_override
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
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
        -- An operator who edited a price meant it. A catalog refresh from
        -- upstream pricing data must not silently undo that.
        WHERE model_catalog.is_override = false
        ",
    )
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
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("upserting model: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_stable_and_hex() {
        let h = hash_key("oag_live_abc123");
        assert_eq!(h.len(), 64);
        assert_eq!(h, hash_key("oag_live_abc123"));
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_keys_hash_differently() {
        assert_ne!(hash_key("oag_live_a"), hash_key("oag_live_b"));
    }

    #[test]
    fn the_hash_does_not_contain_the_key() {
        assert!(!hash_key("oag_live_secret").contains("secret"));
    }
}
