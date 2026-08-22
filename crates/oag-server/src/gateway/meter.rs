//! Recording what a request cost, and what it would have cost.

use crate::AppState;
use crate::gateway::sse::StreamOutcome;
use oag_core::{AccountId, RequestId};
use oag_router::{RoutingDecision, SelectionReason};
use oag_store::{AuthContext, UsageWrite};
use rust_decimal::Decimal;
use std::time::Instant;

/// Everything about a request that the ledger needs, captured before the
/// stream starts — the handler has returned by the time metering runs.
#[derive(Debug, Clone)]
pub struct Context {
    pub request_id: RequestId,
    pub auth: AuthContext,
    pub decision: RoutingDecision,
    pub account: AccountId,
    pub started: Instant,
}

/// Append one row and emit the matching metrics.
///
/// Runs after the response has been fully streamed, in the pump's task rather
/// than the request's — so a client that hung up early still gets billed for
/// what the provider generated.
pub async fn record(state: &AppState, ctx: &Context, outcome: &StreamOutcome) {
    let usage = *outcome.accumulator.usage();
    let cost = ctx.decision.model.pricing.cost(&usage);

    // What the same tokens would have cost on the route's top rung. The
    // difference, summed, is the number that justifies the gateway.
    let counterfactual = ctx
        .decision
        .ceiling_model
        .as_ref()
        .map_or(cost, |m| m.pricing.cost(&usage));

    let (escalated_from, gate) = match &ctx.decision.reason {
        SelectionReason::Escalated { from, gate } => {
            (Some(from.to_string()), Some(format!("{gate:?}")))
        }
        _ => (None, None),
    };

    let status = if outcome.error.is_some() { 502 } else { 200 };

    let write = UsageWrite {
        request_id: ctx.request_id.as_uuid(),
        principal_id: Some(ctx.auth.principal_id),
        api_key_id: Some(ctx.auth.api_key_id),
        route_id: Some(ctx.auth.route_id),
        account_id: Some(ctx.account.as_uuid()),
        model_id: ctx.decision.model.id.as_str().to_owned(),
        tier: ctx.decision.tier.name.to_string(),
        selection_reason: reason_label(&ctx.decision.reason).to_owned(),
        escalated_from_tier: escalated_from,
        escalation_gate: gate,
        usage,
        cost_usd: cost,
        counterfactual_usd: counterfactual,
        counterfactual_model_id: ctx
            .decision
            .ceiling_model
            .as_ref()
            .map(|m| m.id.as_str().to_owned()),
        status,
        latency_ms: i32::try_from(outcome.total.as_millis()).ok(),
        ttft_ms: outcome.ttft.and_then(|d| i32::try_from(d.as_millis()).ok()),
        streamed: true,
    };

    if let Err(e) = oag_store::repo::record_usage(&state.db, &write).await {
        // Losing a ledger row is a real loss, but failing here would achieve
        // nothing: the response has already been delivered and the provider has
        // already billed us. Log loudly and move on.
        tracing::error!(
            request_id = %ctx.request_id,
            error = %e,
            cost_usd = %cost,
            "FAILED TO RECORD USAGE — this spend is not in the ledger"
        );
        metrics::counter!("oag_usage_write_failures_total").increment(1);
        return;
    }

    emit_metrics(ctx, outcome, &usage, cost, counterfactual);
}

fn emit_metrics(
    ctx: &Context,
    outcome: &StreamOutcome,
    usage: &oag_router::Usage,
    cost: Decimal,
    counterfactual: Decimal,
) {
    let model = ctx.decision.model.id.as_str().to_owned();
    let tier = ctx.decision.tier.name.to_string();
    let provider = ctx.decision.model.provider.as_str();

    metrics::counter!("oag_tokens_total", "kind" => "input", "model" => model.clone())
        .increment(usage.input_tokens);
    metrics::counter!("oag_tokens_total", "kind" => "output", "model" => model.clone())
        .increment(usage.output_tokens);
    metrics::counter!("oag_tokens_total", "kind" => "cache_read", "model" => model.clone())
        .increment(usage.cache_read_tokens);
    metrics::counter!("oag_tokens_total", "kind" => "cache_write", "model" => model.clone())
        .increment(usage.cache_write_tokens);

    // Micro-USD in a u64 counter, not dollars in a float.
    //
    // A monotonic total accumulated over millions of requests is exactly where
    // float drift shows up, and a counter is the honest metric type for
    // something that only goes up. Dashboards divide by 1e6. The ledger keeps
    // the exact `Decimal` either way; this is for graphs, not invoices.
    metrics::counter!("oag_cost_microusd_total", "model" => model.clone(), "tier" => tier.clone())
        .increment(to_micros(cost));
    metrics::counter!("oag_counterfactual_microusd_total", "tier" => tier.clone())
        .increment(to_micros(counterfactual));

    metrics::histogram!("oag_request_duration_seconds", "provider" => provider)
        .record(outcome.total.as_secs_f64());
    if let Some(ttft) = outcome.ttft {
        metrics::histogram!("oag_time_to_first_token_seconds", "provider" => provider)
            .record(ttft.as_secs_f64());
    }
    if outcome.client_gone {
        metrics::counter!("oag_client_disconnects_total").increment(1);
    }

    tracing::info!(
        request_id = %ctx.request_id,
        model = %model,
        tier = %tier,
        input = usage.input_tokens,
        output = usage.output_tokens,
        cached = usage.cache_read_tokens,
        cost_usd = %cost,
        would_have_cost_usd = %counterfactual,
        latency_ms = outcome.total.as_millis(),
        client_gone = outcome.client_gone,
        "metered"
    );
}

/// A stable, low-cardinality label. `Debug` output would leak the escalation
/// tier name into the label and blow up the metric's cardinality.
const fn reason_label(r: &SelectionReason) -> &'static str {
    match r {
        SelectionReason::Passthrough => "passthrough",
        SelectionReason::Classified => "classified",
        SelectionReason::FloorPinned => "floor_pinned",
        SelectionReason::BudgetDowngraded => "budget_downgraded",
        SelectionReason::Escalated { .. } => "escalated",
    }
}

/// USD as whole micro-dollars, saturating at zero.
///
/// Exact for every price we deal with: provider pricing has at most six
/// decimal places, which is precisely one micro-dollar.
fn to_micros(d: Decimal) -> u64 {
    use rust_decimal::prelude::ToPrimitive;
    (d * Decimal::from(1_000_000u32))
        .round()
        .to_u64()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_core::TierName;
    use oag_router::QualityGate;

    #[test]
    fn reason_labels_are_low_cardinality() {
        // Debug output would embed the tier name and turn one label into as
        // many series as there are rungs.
        assert_eq!(
            reason_label(&SelectionReason::Escalated {
                from: TierName::new("cheap"),
                gate: QualityGate::Refusal,
            }),
            "escalated"
        );
        assert_eq!(reason_label(&SelectionReason::Classified), "classified");
    }

    #[test]
    fn cost_converts_to_micro_usd_exactly() {
        use rust_decimal::dec;
        assert_eq!(to_micros(dec!(1)), 1_000_000);
        assert_eq!(to_micros(dec!(0.000001)), 1);
        assert_eq!(to_micros(Decimal::ZERO), 0);
        // Six decimal places is the finest granularity provider pricing uses,
        // so nothing real is lost here.
        assert_eq!(to_micros(dec!(0.123456)), 123_456);
    }

    #[test]
    fn a_negative_cost_cannot_underflow_the_counter() {
        use rust_decimal::dec;
        assert_eq!(to_micros(dec!(-5)), 0);
    }
}
