//! Queries.

use crate::Db;
use crate::rows::{AccountRow, AuthContext, ModelRow, RouteRow, ServiceRow, UsageWrite};
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
            bool,
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
               ), 0),
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
        spent_usd: r.5,
        principal_budget_usd: r.6,
        principal_hard_stop_multiple: r.7,
        principal_spent_usd: r.8,
        admin: r.9,
    }))
}

pub async fn route_by_id(db: &Db, id: Uuid) -> Result<Option<RouteRow>> {
    sqlx::query_as::<_, RouteRow>(
        // The CASE matters: a route with no budget skips the aggregate
        // entirely, so the common path costs one primary-key lookup. Routes
        // that do have a budget pay an index range scan on
        // usage_event_route_idx, which is what that index is for.
        "SELECT id, name, tiers, default_mode, floor_tier, rpm_limit, monthly_budget_usd, active,
                CASE WHEN monthly_budget_usd IS NULL THEN 0 ELSE COALESCE((
                    SELECT SUM(u.cost_usd) FROM usage_event u
                    WHERE u.route_id = route.id
                      AND u.occurred_at >= date_trunc('month', now())
                ), 0) END AS spent_usd
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
               rate_limited_until, window_resets_at, last_used_at
        FROM account WHERE id = $1
        ",
    )
    .bind(id.as_uuid())
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading account: {e}")))
}

/// Providers this route holds usable credentials for, for one principal.
///
/// Mirrors the personal-credential predicate in `candidates`: a credential
/// bound to another principal must never appear in this principal's view. Adds
/// `a.schedulable`, which `candidates` leaves to the scheduler — correct here
/// because a disabled credential is an operator decision, not a transient
/// state, and advertising a model nobody can reach is worse than omitting it.
pub async fn route_providers(db: &Db, route_id: Uuid, principal_id: Uuid) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        r"
        SELECT DISTINCT a.provider
        FROM account a
        JOIN account_route ar ON ar.account_id = a.id
        WHERE ar.route_id = $1
          AND a.schedulable
          AND (a.owner_principal_id IS NULL OR a.owner_principal_id = $2)
        ",
    )
    .bind(route_id)
    .bind(principal_id)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading route providers: {e}")))
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
            request_id, attempt, principal_id, api_key_id, route_id, account_id,
            model_id, tier, selection_reason, escalated_from_tier, escalation_gate,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            cost_usd, counterfactual_usd, counterfactual_model_id,
            status, latency_ms, ttft_ms, streamed
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22)
        ON CONFLICT (request_id, attempt) DO NOTHING
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
    use crate::Db;

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

    /// `route_by_id` against a real Postgres.
    ///
    /// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it. The month
    /// boundary and the route filter are the kind of thing that cannot be
    /// tested without a database, and the aggregate is skipped entirely for
    /// routes with no budget — a behaviour worth pinning, since getting it
    /// wrong means an index scan over the ledger on every single request.
    #[tokio::test]
    async fn route_spend_counts_this_month_and_this_route_only() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let capped: Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers, default_mode, monthly_budget_usd)
             VALUES (gen_random_uuid(), $1, '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb, 'managed', 500)
             RETURNING id",
        )
        .bind(format!("capped-{}", Uuid::new_v4()))
        .fetch_one(db.pool())
        .await
        .expect("insert capped route");

        let other: Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers, default_mode)
             VALUES (gen_random_uuid(), $1, '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb, 'managed')
             RETURNING id",
        )
        .bind(format!("uncapped-{}", Uuid::new_v4()))
        .fetch_one(db.pool())
        .await
        .expect("insert uncapped route");

        for (route, cost, ago) in [
            (capped, "120.50", "0 days"),
            (capped, "80.25", "0 days"),
            // Two months back: a monthly cap must not see it.
            (capped, "999.00", "2 months"),
            // A different route's spend must not leak in.
            (other, "777.00", "0 days"),
        ] {
            sqlx::query(
                "INSERT INTO usage_event
                   (request_id, route_id, cost_usd, occurred_at, model_id, tier, selection_reason, status)
                 VALUES (gen_random_uuid(), $1, $2::numeric, now() - $3::interval,
                         'kimi-k2', 'cheap', 'classified', 200)",
            )
            .bind(route)
            .bind(cost)
            .bind(ago)
            .execute(db.pool())
            .await
            .expect("insert usage");
        }

        let row = route_by_id(&db, capped)
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(
            row.spent_usd.to_string(),
            "200.75000000",
            "only this month, only this route"
        );

        // No budget means no aggregate: the value is zero regardless of the
        // 777.00 sitting in the ledger for that route.
        let row = route_by_id(&db, other)
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(row.monthly_budget_usd, None);
        assert!(row.spent_usd.is_zero(), "uncapped routes skip the sum");
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

    fn test_db() -> Option<Db> {
        let url = std::env::var("OAG_TEST_DATABASE_URL").ok()?;
        Some(Db::connect(&url, 2).expect("connect"))
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
    async fn route_providers_hides_what_the_caller_cannot_use() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;

        assert_eq!(
            route_providers(&db, route, principal).await.expect("list"),
            vec!["anthropic".to_owned()]
        );

        // Disabled is an operator decision, not a transient state: advertising
        // a model nobody can reach moves the failure away from its cause.
        set_schedulable(&db, account, false).await.expect("disable");
        assert!(
            route_providers(&db, route, principal)
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
            route_providers(&db, route, principal)
                .await
                .expect("list")
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

    #[tokio::test]
    async fn both_attempts_of_an_escalated_request_reach_the_ledger() {
        let Some(db) = test_db() else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        db.migrate().await.expect("migrate");
        let (principal, route, account) = seed(&db).await;

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
            status: 200,
            latency_ms: Some(10),
            ttft_ms: None,
            streamed: false,
        };

        record_usage(&db, &write(0, "abandoned", "0.25"))
            .await
            .expect("abandoned attempt");
        record_usage(&db, &write(1, "escalated", "1.75"))
            .await
            .expect("served attempt");

        let (rows, spend): (i64, Decimal) = sqlx::query_as(
            "SELECT count(*), coalesce(sum(cost_usd), 0) FROM usage_event WHERE request_id = $1",
        )
        .bind(request_id)
        .fetch_one(db.pool())
        .await
        .expect("read back");

        // Keyed on the request id alone, the second write conflicted with the
        // first and `DO NOTHING` discarded it — so one attempt's spend was
        // absent from the ledger while the invoice still charged for it.
        assert_eq!(rows, 2, "one row per attempt");
        assert_eq!(spend.to_string(), "2.00000000", "both attempts are billed");

        // Per-attempt, not per-request: idempotence is what stops a retried
        // write from double-billing, and it has to survive the wider key.
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
}
