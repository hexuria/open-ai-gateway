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
pub async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    if state.lifecycle.is_draining() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ready": false, "reason": "draining" })),
        );
    }

    let r = oag_store::readiness(&state.db, &state.cache).await;
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
