//! The admin API.
//!
//! Served only on the admin listener, which is bound to the internal network —
//! but it still authenticates. "It is on a private network" is one control, not
//! a complete one, and an admin API that trusts its listener is one
//! misconfigured ingress away from being public.
//!
//! Authentication reuses the inbound-key path, but authority is a property of
//! the **key** (`api_key.admin`), not of the principal. An operator's ordinary
//! inference key gets pasted into SDK configs and CI; it must not also be able
//! to disable a credential. `oag admin key --admin` mints the one that can.
//!
//! Every `/admin/api` route is authenticated by one layer applied in
//! [`crate::admin_routes`] rather than by a call inside each handler, because a
//! handler that forgets the call is silently public and nothing about it looks
//! wrong. Reads live here; incident writes are in [`write`]; the service
//! catalog is in [`services`].

pub mod auth;
pub mod services;
pub mod write;

pub use auth::{AdminActor, require_admin_layer};
pub use services::{
    check_service, create_service, disable_service, enable_service, list_services, update_service,
};
pub use write::{clear_cooldown, disable_account, enable_account, revoke_key};

use crate::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

/// Reload the catalog on this replica, immediately.
///
/// The periodic refresh means a change lands on its own within the interval;
/// this is for when waiting is not acceptable. It affects **only the replica
/// that serves the request** — with several behind a load balancer, the
/// periodic refresh is what makes the change fleet-wide.
pub async fn reload_catalog(State(state): State<Arc<AppState>>) -> Response {
    match state.reload_catalog().await {
        Ok(n) => Json(json!({
            "models": n,
            "note": "this replica only; others pick the change up on their refresh interval",
        }))
        .into_response(),
        Err(e) => failed(&e),
    }
}

/// The dashboard.
///
/// One self-contained file, embedded in the binary. A build toolchain,
/// `node_modules`, and a second CI lane for four read views is exactly the kind
/// of weight this project was defined against — and an operator debugging a
/// gateway at 3am should not need `npm install` first. The admin API is a
/// normal REST surface, so a richer UI can be added later without the server
/// changing at all.
pub async fn dashboard() -> Response {
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // Narrows the exfiltration channel; it does not close it. Nothing
            // in CSP constrains top-level navigation, so a script that got in
            // could still send the key somewhere via `location =`. The real
            // exposure is that the key lives in `localStorage` at all — this
            // header is a reduction, not a fix.
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                 connect-src 'self'; form-action 'none'; frame-ancestors 'none'",
            ),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../../../../web/index.html"),
    )
        .into_response()
}

pub(crate) fn failed(e: &oag_core::Error) -> Response {
    tracing::error!(error = %e, "admin query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "query failed" })),
    )
        .into_response()
}

pub(crate) fn invalid(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

pub(crate) fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
}

// Column tuples for the read queries. Named aliases rather than inline tuples
// because a sixteen-element type in a signature is unreadable, and because the
// order has to match the SELECT exactly.
type AccountRowTuple = (
    uuid::Uuid,
    String,
    String,
    String,
    bool,
    Option<time::OffsetDateTime>,
    Option<time::OffsetDateTime>,
    i16,
    i32,
    Option<uuid::Uuid>,
    Option<String>,
);

type RouteRowTuple = (
    uuid::Uuid,
    String,
    String,
    Option<String>,
    serde_json::Value,
    i64,
    bool,
);

type UsageRowTuple = (
    uuid::Uuid,
    time::OffsetDateTime,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    rust_decimal::Decimal,
    rust_decimal::Decimal,
    i64,
    i64,
    i64,
    Option<i32>,
    Option<i32>,
    bool,
    i16,
);

type SummaryTotals = (i64, rust_decimal::Decimal, rust_decimal::Decimal, i64, i64);

type TierTotals = (String, i64, rust_decimal::Decimal, rust_decimal::Decimal);

/// The headline numbers: what was spent, and what it would have cost.
#[derive(Debug, Serialize)]
pub struct Summary {
    pub requests: i64,
    pub spent_usd: String,
    pub counterfactual_usd: String,
    pub saved_usd: String,
    /// Percentage saved against frontier-for-everything.
    pub saved_pct: String,
    pub by_tier: Vec<TierRow>,
    pub cache_hit_rate: String,
}

#[derive(Debug, Serialize)]
pub struct TierRow {
    pub tier: String,
    pub requests: i64,
    pub spent_usd: String,
    pub counterfactual_usd: String,
    /// Computed here, from the exact `Decimal`s. Subtracting the two rounded
    /// strings on the client gives an answer that disagrees with the headline
    /// figure by a cent — small, but the sort of thing that makes someone stop
    /// trusting the whole page.
    pub saved_usd: String,
}

#[derive(Debug, Deserialize)]
pub struct Window {
    /// Days to look back. Defaults to the current month.
    pub days: Option<i32>,
}

pub async fn summary(State(state): State<Arc<AppState>>, Query(window): Query<Window>) -> Response {
    let days = window.days.unwrap_or(30).clamp(1, 3650);

    let totals: Result<SummaryTotals, _> = sqlx::query_as(
        r"
            -- Explicit ::bigint on the token sums: SUM over a bigint column
            -- returns numeric in Postgres, which will not decode into i64.
            SELECT COUNT(*),
                   COALESCE(SUM(cost_usd), 0),
                   COALESCE(SUM(counterfactual_usd), 0),
                   COALESCE(SUM(cache_read_tokens), 0)::bigint,
                   COALESCE(SUM(input_tokens + cache_read_tokens), 0)::bigint
            FROM usage_event
            WHERE occurred_at > now() - make_interval(days => $1)
            ",
    )
    .bind(days)
    .fetch_one(state.db.pool())
    .await;

    let (requests, spent, counterfactual, cached, prompt) = match totals {
        Ok(t) => t,
        // Carry the real error: an admin endpoint that says only "query
        // failed" makes the next person guess, and I just spent a round doing
        // exactly that.
        Err(e) => return failed(&oag_core::Error::Internal(format!("summary: {e}"))),
    };

    let by_tier: Vec<TierTotals> = sqlx::query_as(
        r"
            SELECT tier, COUNT(*),
                   COALESCE(SUM(cost_usd), 0),
                   COALESCE(SUM(counterfactual_usd), 0)
            FROM usage_event
            WHERE occurred_at > now() - make_interval(days => $1)
            GROUP BY tier ORDER BY SUM(counterfactual_usd - cost_usd) DESC
            ",
    )
    .bind(days)
    .fetch_all(state.db.pool())
    .await
    .unwrap_or_default();

    let saved = counterfactual - spent;
    let pct = if counterfactual > rust_decimal::Decimal::ZERO {
        (saved / counterfactual) * rust_decimal::Decimal::from(100)
    } else {
        rust_decimal::Decimal::ZERO
    };
    let hit_rate = if prompt > 0 {
        rust_decimal::Decimal::from(cached) / rust_decimal::Decimal::from(prompt)
            * rust_decimal::Decimal::from(100)
    } else {
        rust_decimal::Decimal::ZERO
    };

    Json(Summary {
        requests,
        spent_usd: format!("{spent:.4}"),
        counterfactual_usd: format!("{counterfactual:.4}"),
        saved_usd: format!("{saved:.4}"),
        saved_pct: format!("{pct:.1}"),
        cache_hit_rate: format!("{hit_rate:.1}"),
        by_tier: by_tier
            .into_iter()
            .map(|(tier, requests, spent, cf)| TierRow {
                tier,
                requests,
                spent_usd: format!("{spent:.4}"),
                counterfactual_usd: format!("{cf:.4}"),
                saved_usd: format!("{:.4}", cf - spent),
            })
            .collect(),
    })
    .into_response()
}

#[derive(Debug, Serialize)]
pub struct AccountView {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub kind: String,
    pub schedulable: bool,
    pub state: &'static str,
    pub priority: i16,
    pub max_concurrency: i32,
    /// True when this credential belongs to one principal rather than the pool.
    pub personal: bool,
    /// Why it is cooling down, when it is. Written by the failover path.
    pub reason: Option<String>,
}

pub async fn accounts(State(state): State<Arc<AppState>>) -> Response {
    let rows: Vec<AccountRowTuple> = sqlx::query_as(
        r"
        SELECT id, name, provider, kind, schedulable, cooldown_until,
               rate_limited_until, priority, max_concurrency, owner_principal_id,
               cooldown_reason
        FROM account ORDER BY provider, name
        ",
    )
    .fetch_all(state.db.pool())
    .await
    .unwrap_or_default();

    let now = time::OffsetDateTime::now_utc();
    let out: Vec<AccountView> = rows
        .into_iter()
        .map(|r| AccountView {
            id: r.0.to_string(),
            name: r.1,
            provider: r.2,
            kind: r.3,
            schedulable: r.4,
            // Never returns the credential itself, sealed or otherwise: the
            // admin API has no reason to hand one back, so it cannot leak one.
            state: if !r.4 {
                "disabled"
            } else if r.5.is_some_and(|t| t > now) {
                "cooling down"
            } else if r.6.is_some_and(|t| t > now) {
                "rate limited"
            } else {
                "ready"
            },
            priority: r.7,
            max_concurrency: r.8,
            personal: r.9.is_some(),
            reason: r.10,
        })
        .collect();

    Json(out).into_response()
}

/// Inbound keys, so the dashboard can show what exists and offer revoke.
///
/// `key_hash` is never selected. It is the only stored form of the credential,
/// and a list endpoint has no use for it — the revoke path reads it server-side
/// and never returns it either.
pub async fn keys(State(state): State<Arc<AppState>>) -> Response {
    let rows: Vec<KeyRowTuple> = sqlx::query_as(
        r"
        SELECT k.id, k.name, k.key_prefix, p.email, r.name,
               k.active, k.admin, k.last_used_at
        FROM api_key k
        JOIN principal p ON p.id = k.principal_id
        JOIN route     r ON r.id = k.route_id
        ORDER BY k.active DESC, k.created_at DESC
        LIMIT 500
        ",
    )
    .fetch_all(state.db.pool())
    .await
    .unwrap_or_default();

    let out: Vec<KeyView> = rows
        .into_iter()
        .map(|r| KeyView {
            id: r.0.to_string(),
            name: r.1,
            key_prefix: r.2,
            principal: r.3,
            route: r.4,
            active: r.5,
            admin: r.6,
            last_used_at: r.7.map(|t| t.to_string()),
        })
        .collect();

    Json(out).into_response()
}

type KeyRowTuple = (
    uuid::Uuid,
    String,
    String,
    String,
    String,
    bool,
    bool,
    Option<time::OffsetDateTime>,
);

#[derive(Debug, Serialize)]
pub struct KeyView {
    pub id: String,
    pub name: String,
    /// The displayed prefix, not the key. There is no field here that could
    /// carry the secret: the plaintext is unrecoverable and the hash is unread.
    pub key_prefix: String,
    pub principal: String,
    pub route: String,
    pub active: bool,
    pub admin: bool,
    pub last_used_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RouteView {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub floor_tier: Option<String>,
    pub tiers: serde_json::Value,
    pub credentials: i64,
    pub active: bool,
}

pub async fn routes(State(state): State<Arc<AppState>>) -> Response {
    let rows: Vec<RouteRowTuple> = sqlx::query_as(
        r"
            SELECT r.id, r.name, r.default_mode, r.floor_tier, r.tiers,
                   COUNT(ar.account_id), r.active
            FROM route r LEFT JOIN account_route ar ON ar.route_id = r.id
            GROUP BY r.id ORDER BY r.name
            ",
    )
    .fetch_all(state.db.pool())
    .await
    .unwrap_or_default();

    Json(
        rows.into_iter()
            .map(|r| RouteView {
                id: r.0.to_string(),
                name: r.1,
                mode: r.2,
                floor_tier: r.3,
                tiers: r.4,
                credentials: r.5,
                active: r.6,
            })
            .collect::<Vec<_>>(),
    )
    .into_response()
}

#[derive(Debug, Serialize)]
pub struct UsageRow {
    pub request_id: String,
    pub at: String,
    pub model: String,
    pub tier: String,
    pub reason: String,
    pub escalated_from: Option<String>,
    pub gate: Option<String>,
    pub cost_usd: String,
    pub counterfactual_usd: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub latency_ms: Option<i32>,
    pub ttft_ms: Option<i32>,
    pub streamed: bool,
    pub status: i16,
}

#[derive(Debug, Deserialize)]
pub struct Page {
    pub limit: Option<i64>,
}

pub async fn usage(State(state): State<Arc<AppState>>, Query(page): Query<Page>) -> Response {
    // Clamped: an unbounded limit on the largest table in the system is a
    // denial-of-service handed to whoever holds an admin key.
    let limit = page.limit.unwrap_or(50).clamp(1, 500);

    let rows: Vec<UsageRowTuple> = sqlx::query_as(
        r"
        SELECT request_id, occurred_at, model_id, tier, selection_reason,
               escalated_from_tier, escalation_gate, cost_usd, counterfactual_usd,
               input_tokens, output_tokens, cache_read_tokens,
               latency_ms, ttft_ms, streamed, status
        FROM usage_event ORDER BY occurred_at DESC LIMIT $1
        ",
    )
    .bind(limit)
    .fetch_all(state.db.pool())
    .await
    .unwrap_or_default();

    Json(
        rows.into_iter()
            .map(|r| UsageRow {
                request_id: r.0.to_string(),
                at: r
                    .1
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                model: r.2,
                tier: r.3,
                reason: r.4,
                escalated_from: r.5,
                gate: r.6,
                cost_usd: format!("{:.6}", r.7),
                counterfactual_usd: format!("{:.6}", r.8),
                input_tokens: r.9,
                output_tokens: r.10,
                cache_read_tokens: r.11,
                latency_ms: r.12,
                ttft_ms: r.13,
                streamed: r.14,
                status: r.15,
            })
            .collect::<Vec<_>>(),
    )
    .into_response()
}
