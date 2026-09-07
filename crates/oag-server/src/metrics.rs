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
        "oag_channel_unavailable_total",
        "Requests whose `@api`/`@sub` pin left no credential of that kind on the route."
    );
    describe_counter!(
        "oag_failovers_total",
        "Requests moved to a different credential after an upstream failure."
    );
    describe_counter!(
        "oag_spend_reconcile_total",
        "Passes bringing the monthly spend counters into agreement with the ledger, by outcome."
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
    describe_counter!(
        "oag_failover_budget_exhausted_total",
        "Requests that ran out of failover budget before a credential answered. \
         Rising means `gateway.failover_budget` is shorter than the fleet's slow path."
    );
    describe_counter!(
        "oag_panics_total",
        "Handler panics caught and answered as 500. Should be zero; any value at all \
         names a bug the `catch_panic` layer only stopped from severing the connection."
    );
    describe_counter!(
        "oag_token_refreshes_total",
        "OAuth credential refreshes, by outcome. A rising `failed` means seats are \
         about to start 401ing, some minutes before they do."
    );
    describe_gauge!(
        "oag_slots_in_use",
        "Concurrency slots held by credential, fleet-wide, as last read by this replica. \
         Every replica reports the same shared count: aggregate with max by (account), never sum."
    );
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

#[cfg(test)]
mod tests {
    /// A6. Every counter this crate emits has a description.
    ///
    /// An undescribed series still appears in `/metrics`; what it lacks is the
    /// `# HELP` line, so the operator reading a dashboard has the name and
    /// nothing else. The finding named three; two were described and
    /// `oag_channel_unavailable_total` — added by G5's `@api`/`@sub` pin, after
    /// the sweep — was not, so the fix was one short of its own scope.
    ///
    /// A source scan, and a whole-crate one: the alternative is installing a
    /// recorder and provoking every counter, which needs the live paths each of
    /// them sits on. What is checkable without that is the pairing, and a name
    /// added anywhere in the crate fails this the moment it lands.
    #[test]
    fn every_metric_this_crate_emits_is_described() {
        // Every `.rs` in the crate, so a counter introduced in any module is
        // covered by where it lives rather than by being remembered here.
        let sources = [
            include_str!("metrics.rs"),
            include_str!("lib.rs"),
            include_str!("health.rs"),
            include_str!("state.rs"),
            include_str!("usage_poll.rs"),
            include_str!("breakers.rs"),
            include_str!("shutdown.rs"),
            include_str!("listen.rs"),
            include_str!("gateway/mod.rs"),
            include_str!("gateway/select.rs"),
            include_str!("gateway/meter.rs"),
            include_str!("gateway/sse.rs"),
            include_str!("gateway/refresh.rs"),
            include_str!("gateway/models.rs"),
            include_str!("gateway/alias.rs"),
            include_str!("gateway/authn.rs"),
            include_str!("gateway/presence.rs"),
            include_str!("gateway/count_tokens.rs"),
            include_str!("admin/mod.rs"),
            include_str!("admin/auth.rs"),
            include_str!("admin/write.rs"),
            include_str!("admin/points.rs"),
            include_str!("admin/services.rs"),
        ];

        // A name is emitted where it follows `counter!(`, `histogram!(` or
        // `gauge!(`, and described where it follows `describe_`. This file
        // holds both, so the two sets are read from what precedes the name
        // rather than from which file it is in.
        let names = |kinds: [&str; 3]| {
            let mut found = std::collections::BTreeSet::new();
            for src in sources {
                for kind in kinds {
                    for (at, _) in src.match_indices(kind) {
                        let rest = &src[at + kind.len()..];
                        let Some(open) = rest.find('"') else { continue };
                        // Only whitespace between the macro and its first
                        // argument, or this is some other call entirely.
                        if !rest[..open].trim().is_empty() {
                            continue;
                        }
                        let rest = &rest[open + 1..];
                        if let Some(close) = rest.find('"')
                            && rest[..close].starts_with("oag_")
                        {
                            found.insert(rest[..close].to_owned());
                        }
                    }
                }
            }
            found
        };

        let described = names([
            "describe_counter!(",
            "describe_histogram!(",
            "describe_gauge!(",
        ]);
        let mut emitted = names(["\ncounter!(", " counter!(", "::counter!("]);
        emitted.extend(names(["\nhistogram!(", " histogram!(", "::histogram!("]));
        emitted.extend(names(["\ngauge!(", " gauge!(", "::gauge!("]));

        assert!(
            !emitted.is_empty() && !described.is_empty(),
            "the scan found nothing, so it is asserting nothing: {} emitted, {} described",
            emitted.len(),
            described.len()
        );
        let undescribed: Vec<&String> = emitted.difference(&described).collect();
        assert!(
            undescribed.is_empty(),
            "these reach /metrics with no HELP line, so a dashboard shows the \
             name and nothing else: {undescribed:?}"
        );
    }
}
