//! Prometheus metrics.
//!
//! sub2api ships no metrics endpoint at all — its observability is an admin
//! dashboard backed by aggregate rows in its own Postgres. That works until you
//! want to alert on something, or correlate a gateway symptom with anything
//! else in the fleet.

use crate::AppState;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::Arc;

/// Install the global recorder.
///
/// Called once at boot. Returns the handle the `/metrics` route renders from.
pub fn install() -> Result<PrometheusHandle, oag_core::Error> {
    PrometheusBuilder::new()
        // Latency buckets chosen for this traffic: an LLM call's interesting
        // range is hundreds of milliseconds to tens of seconds, so the default
        // buckets — which top out around ten seconds — put most of the
        // distribution in +Inf and make p99 unreadable.
        .set_buckets(&[
            0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 40.0, 80.0, 160.0, 320.0,
        ])
        .map_err(|e| oag_core::Error::Internal(format!("metric buckets: {e}")))?
        .install_recorder()
        .map_err(|e| oag_core::Error::Internal(format!("installing recorder: {e}")))
}

/// Describe every metric once, so `/metrics` carries HELP and TYPE lines.
pub fn describe() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "oag_requests_total",
        "Inference requests, by route and outcome."
    );
    describe_counter!(
        "oag_escalations_total",
        "Requests retried one tier up after a quality gate tripped."
    );
    describe_counter!(
        "oag_failovers_total",
        "Requests moved to a different credential after an upstream failure."
    );
    describe_counter!(
        "oag_slot_accounting_degraded_total",
        "Requests admitted without a concurrency-slot answer from Redis, by \
         operation. Non-zero means selection is running open: credentials can \
         be oversubscribed until Redis returns. Alert on it."
    );
    describe_counter!(
        "oag_tokens_total",
        "Tokens by kind: input, output, cache read, cache write."
    );
    describe_counter!(
        "oag_cost_microusd_total",
        "Actual spend in micro-USD; divide by 1e6. Pair with the counterfactual for the saving."
    );
    describe_counter!(
        "oag_counterfactual_microusd_total",
        "What the same traffic would have cost on each route's top tier, in micro-USD."
    );
    describe_counter!(
        "oag_selection_total",
        "Credential selections, by which cascade stage decided."
    );
    describe_counter!(
        "oag_client_disconnects_total",
        "Requests where the client hung up before the upstream finished."
    );
    describe_counter!(
        "oag_at_capacity_total",
        "Requests refused because every healthy credential was at its concurrency limit. \
         A sizing signal, not a fault."
    );
    describe_counter!(
        "oag_escalations_suppressed_total",
        "Unusable answers left unescalated because the principal was near their budget."
    );
    describe_counter!(
        "oag_usage_write_failures_total",
        "Spend that could not be written to the ledger. Should always be zero."
    );
    describe_histogram!(
        "oag_request_duration_seconds",
        "End-to-end request latency."
    );
    describe_histogram!(
        "oag_time_to_first_token_seconds",
        "Latency to the first streamed token. The number users actually feel."
    );
    describe_gauge!(
        "oag_credentials_schedulable",
        "Credentials currently eligible, by provider."
    );
    describe_gauge!("oag_slots_in_use", "Concurrency slots held, by credential.");
    describe_gauge!(
        "oag_draining",
        "1 while this replica is shutting down and refusing new work."
    );

    // Give the lifecycle gauge a value immediately. `describe!` alone emits
    // nothing, so a freshly booted replica would serve an empty /metrics body —
    // which is indistinguishable from a broken exporter to whoever is scraping
    // it, and to the alert that fires when the scrape returns no series.
    metrics::gauge!("oag_draining").set(0.0);
}

pub async fn render(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(handle) = state.lifecycle.metrics() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "metrics recorder not installed\n".to_owned(),
        );
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        handle.render(),
    )
}
