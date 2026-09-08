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

/// The readiness answer, recomputed at most once a second.
///
/// Held on the `AppState` so every prober of *this* gateway shares it. The lock
/// is only ever held across a clone of a small struct or the lookup itself; a
/// second caller arriving mid-lookup waits for it rather than starting another,
/// which is the contention this exists to avoid.
///
/// A8: it was a process-global `OnceLock`, which is a different thing. Two
/// `AppState`s in one process — two gateways, or a test beside the code it
/// tests — shared one memo, so one answered for the other's backends for a
/// second at a time. Nothing else on `AppState` is process-global and this had
/// no reason to be.
///
/// The drain check is not here — see `ready`. This answers "can the backends be
/// reached", which is a question about Postgres and Redis and not about this
/// process's own lifecycle.
async fn cached_readiness(state: &AppState) -> oag_store::Readiness {
    use std::time::{Duration, Instant};

    const FRESH_FOR: Duration = Duration::from_secs(1);

    let mut held = state.readiness.lock().await;
    if let Some((at, cached)) = held.as_ref()
        && at.elapsed() < FRESH_FOR
    {
        return cached.clone();
    }
    let fresh = oag_store::readiness(&state.db, &state.cache).await;
    *held = Some((Instant::now(), fresh.clone()));
    fresh
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
            "schema": r.schema,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::cached_readiness;
    use crate::AppState;
    use std::sync::Arc;

    /// Closed ports: `readiness` answers "not ready" without waiting, which is
    /// all this test needs — the question is whose memo answered.
    fn state() -> Arc<AppState> {
        crate::testing::state("")
    }

    /// A8. The readiness memo belongs to one gateway, not to the process.
    ///
    /// It was a `static OnceLock`, so every `AppState` in the process shared
    /// one slot: prime one and another answers from it for a second, with no
    /// lookup of its own. In a binary running two gateways that is one
    /// reporting the other's backends; in a test it is one test's answer
    /// leaking into the next.
    ///
    /// Asserted on the slot rather than on the answer, because both states here
    /// reach the same closed ports and so agree on the answer — which is the
    /// point: a shared memo is invisible in the result and visible only in
    /// whether the second state ever looked.
    #[tokio::test]
    async fn priming_one_state_does_not_answer_for_another() {
        let a = state();
        let b = state();

        cached_readiness(&a).await;
        assert!(
            a.readiness.lock().await.is_some(),
            "the state that was probed holds its answer"
        );
        assert!(
            b.readiness.lock().await.is_none(),
            "and the other one has not been given it — with a process-global \
             slot this is where the leak shows"
        );

        cached_readiness(&b).await;
        assert!(
            b.readiness.lock().await.is_some(),
            "and it memoises its own once it is asked"
        );
    }
}
