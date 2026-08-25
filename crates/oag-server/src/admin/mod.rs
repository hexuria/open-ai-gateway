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
pub mod models;
pub mod services;
pub mod write;

pub use auth::{AdminActor, require_admin_layer};
pub use models::{list_models, update_model};
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

// name, requests, api-value, monthly-fee, remaining-pct, window-label.
type SeatTuple = (
    String,
    i64,
    rust_decimal::Decimal,
    Option<rust_decimal::Decimal>,
    Option<rust_decimal::Decimal>,
    Option<String>,
);

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
    /// One row per subscription seat, metered individually — three Grok seats
    /// are three rows, so each subscription's own cost and subsidy is legible
    /// rather than blurred into a fleet total. Separate from the frontier
    /// figures above, which describe only metered per-token traffic.
    pub subscriptions: Vec<SeatRow>,
}

/// One subscription seat's economics over the window.
///
/// The headline figures above are per-token traffic; a seat is flat-rate, so it
/// gets its own row here answering a different question — what its usage would
/// have cost billed per token, against the fixed fee that displaced that bill.
#[derive(Debug, Serialize)]
pub struct SeatRow {
    pub name: String,
    /// Requests this seat served in the window.
    pub requests: i64,
    /// What this seat's usage would have cost at the served models' list API
    /// prices: `SUM(counterfactual_api_usd)` for its rows. The bill the flat
    /// fee displaced.
    pub api_value_usd: String,
    /// The seat's own flat fee, prorated to the window. `None` when no monthly
    /// price was recorded — an unpriced seat is not a free one, so its saving
    /// cannot be computed rather than being shown as the whole API value.
    pub plan_cost_usd: Option<String>,
    /// `api_value_usd - plan_cost_usd`: what this one subscription saved, net of
    /// its fee. `None` when the seat is unpriced. Negative means this seat's
    /// traffic has not yet earned its fee back.
    pub saved_usd: Option<String>,
    /// Remaining allowance from the provider's own usage API, 0..100. `None`
    /// until the poller has read it — an unpolled seat's headroom is unknown,
    /// not full.
    pub remaining_pct: Option<String>,
    /// The window the percentage is measured over ("weekly"), for the label
    /// beside it. `None` when never polled.
    pub usage_window: Option<String>,
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

/// One row per subscription seat, each metered on its own.
///
/// Each flat-rate account (`kind='oauth'`) is its own row so three Grok seats
/// read as three lines, not one blur. A `LEFT JOIN` keeps a seat with no
/// traffic this window visible at zero rather than vanishing, and the seat-row
/// predicate on the join (`cost_usd = 0 AND counterfactual_api_usd > 0`) counts
/// only what the seat actually served. Failures degrade to an empty list — a
/// missing subscriptions table should not take down the whole summary.
async fn seat_summaries(db: &oag_store::Db, days: i32) -> Vec<SeatRow> {
    let seats: Vec<SeatTuple> = sqlx::query_as(
        r"
            SELECT a.name,
                   COUNT(u.request_id),
                   COALESCE(SUM(u.counterfactual_api_usd), 0),
                   a.monthly_cost_usd,
                   a.usage_remaining_pct,
                   a.usage_window_label
            FROM account a
            LEFT JOIN usage_event u
                   ON u.account_id = a.id
                  AND u.occurred_at > now() - make_interval(days => $1)
                  AND u.cost_usd = 0
                  AND u.counterfactual_api_usd > 0
            WHERE a.kind = 'oauth'
            GROUP BY a.id, a.name, a.monthly_cost_usd, a.usage_remaining_pct, a.usage_window_label
            ORDER BY COALESCE(SUM(u.counterfactual_api_usd), 0) DESC, a.name
            ",
    )
    .bind(days)
    .fetch_all(db.pool())
    .await
    .unwrap_or_default();

    let day_frac = rust_decimal::Decimal::from(days) / rust_decimal::Decimal::from(30);
    seats
        .into_iter()
        .map(|(name, requests, api_value, monthly, remaining, window)| {
            // Prorate each seat's own fee to the window. An unpriced seat yields
            // None for both cost and saving — its API value is still shown, but
            // a saving cannot be invented from a fee nobody entered.
            let (plan_cost_usd, saved_usd) = match monthly {
                Some(m) => {
                    let plan = m * day_frac;
                    (
                        Some(format!("{plan:.4}")),
                        Some(format!("{:.4}", api_value - plan)),
                    )
                }
                None => (None, None),
            };
            SeatRow {
                name,
                requests,
                api_value_usd: format!("{api_value:.4}"),
                plan_cost_usd,
                saved_usd,
                remaining_pct: remaining.map(|r| format!("{r:.0}")),
                usage_window: window,
            }
        })
        .collect()
}

pub async fn summary(State(state): State<Arc<AppState>>, Query(window): Query<Window>) -> Response {
    let days = window.days.unwrap_or(30).clamp(1, 3650);

    // The headline is per-token traffic only. A seat row (cost 0, real
    // API-equivalent price) is flat-rate, so folding it in here would let its
    // zero marginal cost inflate the frontier saving — the subscription's worth
    // is a separate question, answered per seat below.
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
              AND NOT (cost_usd = 0 AND counterfactual_api_usd > 0)
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
              AND NOT (cost_usd = 0 AND counterfactual_api_usd > 0)
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

    let subscriptions = seat_summaries(&state.db, days).await;

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
        subscriptions,
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

// provider, kind, accounts.
type ProviderCountTuple = (String, String, i64);

/// How many credentials of one kind are registered for a provider.
#[derive(Debug, Serialize)]
pub struct KindCount {
    pub kind: String,
    pub accounts: i64,
}

/// One row of the support matrix.
///
/// Two questions in one row, because answering either alone is misleading:
/// what this build can do with a provider (from [`oag_core::Provider::support`],
/// a total match over the enum) and what this deployment has actually
/// registered against it.
#[derive(Debug, Serialize)]
pub struct ProviderView {
    /// The spelling the CLI, the config, and `account.provider` all use.
    pub provider: String,
    pub display_name: String,
    pub aliases: Vec<String>,
    pub dialect: String,
    pub credential_kinds: Vec<String>,
    pub api_key: bool,
    /// Tri-state, serialised from the core enum rather than restated here: a
    /// second copy of "served / import-only / not offered" is a second copy to
    /// get wrong.
    pub subscription: oag_core::provider::SubscriptionSupport,
    pub note: Option<String>,
    /// Whether this build registers an adapter for the provider. Vertex has a
    /// credential kind, a dialect, and no adapter — so it can be registered and
    /// cannot serve, and a matrix that omitted this would say it works.
    pub adapter: bool,
    pub accounts: i64,
    pub by_kind: Vec<KindCount>,
}

/// The provider support matrix: what is possible, beside what is set up.
///
/// The capability half is compiled in and needs no database; the configured
/// half is one grouped count. They are served together because the question an
/// operator actually asks — "can I use my Grok subscription, and have I?" — is
/// answered by neither alone.
pub async fn providers(State(state): State<Arc<AppState>>) -> Response {
    let counts: Result<Vec<ProviderCountTuple>, _> = sqlx::query_as(
        r"
        SELECT provider, kind, COUNT(*)
        FROM account GROUP BY provider, kind ORDER BY provider, kind
        ",
    )
    .fetch_all(state.db.pool())
    .await;

    let counts = match counts {
        Ok(c) => c,
        // Deliberately not `.unwrap_or_default()` like the reads above. An
        // empty result renders as "nothing configured", which is a real and
        // actionable state — and is exactly the wrong answer to give when the
        // truth is that the query failed.
        Err(e) => return failed(&oag_core::Error::Internal(format!("providers: {e}"))),
    };

    let adapters = state.providers();

    let out: Vec<ProviderView> = oag_core::Provider::ALL
        .iter()
        .map(|&p| {
            let s = p.support();
            // A `provider` string the enum does not know has no row here: it
            // cannot be routed either, so the matrix has nothing to say about
            // it. `oag admin add-account` parses the flag, so producing one
            // takes a hand-written INSERT.
            let mine = counts.iter().filter(|c| c.0 == p.as_str());
            let by_kind: Vec<KindCount> = mine
                .clone()
                .map(|c| KindCount {
                    kind: c.1.clone(),
                    accounts: c.2,
                })
                .collect();

            ProviderView {
                provider: p.as_str().to_owned(),
                display_name: s.display_name.to_owned(),
                aliases: s.aliases.iter().map(|a| (*a).to_owned()).collect(),
                dialect: s.dialect().to_string(),
                credential_kinds: s.credential_kinds.iter().map(ToString::to_string).collect(),
                api_key: s.api_key(),
                subscription: s.subscription,
                note: s.note.map(ToOwned::to_owned),
                adapter: adapters.contains(&p),
                accounts: mine.map(|c| c.2).sum(),
                by_kind,
            }
        })
        .collect();

    Json(out).into_response()
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
