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
}
