//! Liveness and readiness.
//!
//! The distinction is the whole point, and getting it wrong is why a load
//! balancer keeps sending traffic to a replica that cannot serve it.

use crate::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;
use std::sync::Arc;

/// The process is running.
///
/// Never checks dependencies. A liveness probe that fails when the database is
/// down causes the orchestrator to *restart* every replica during a database
/// outage, which turns a recoverable incident into a crash loop.
pub async fn live() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(json!({ "status": "live" })))
}

/// The process can serve a request right now.
///
/// Checks Postgres and Redis, and reports not-ready during shutdown drain so
/// the load balancer stops sending new work while in-flight streams finish.
/// The readiness answer, recomputed at most once a second.
///
/// Held per process rather than per handler so every prober shares it. The lock
/// is only ever held across a clone of a small struct or the lookup itself; a
/// second caller arriving mid-lookup waits for it rather than starting another,
/// which is the contention this exists to avoid.
async fn cached_readiness(state: &AppState) -> oag_store::Readiness {
    use std::time::{Duration, Instant};

    const FRESH_FOR: Duration = Duration::from_secs(1);
    static CACHED: std::sync::OnceLock<
        tokio::sync::Mutex<Option<(Instant, oag_store::Readiness)>>,
    > = std::sync::OnceLock::new();

    let slot = CACHED.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut held = slot.lock().await;
    if let Some((at, cached)) = held.as_ref()
        && at.elapsed() < FRESH_FOR
    {
        return cached.clone();
    }
    let fresh = oag_store::readiness(&state.db, &state.cache).await;
    *held = Some((Instant::now(), fresh.clone()));
    fresh
}

pub async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    if state.lifecycle.is_draining() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ready": false, "reason": "draining" })),
        );
    }

    // Memoised for a second.
    //
    // This route sits outside the in-flight ceiling on purpose — a replica that
    // is full is still alive, and shedding a probe restarts a busy pod — but
    // that also means nothing bounds how often it runs. Every call pings
    // Postgres, which takes a pooled connection; a Kubernetes readiness probe
    // per replica plus a load balancer's own health check plus whatever an
    // operator is refreshing is a steady draw on the pool the requests need,
    // and it is at its worst exactly when the pool is already contended.
    //
    // A second is shorter than any sane probe interval, so a prober still sees
    // a fresh answer, and long enough that a burst of them costs one lookup.
    let r = cached_readiness(&state).await;
    let code = if r.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(json!({
            "ready": r.ready,
            "database": r.database,
            "redis": r.redis,
        })),
    )
}
