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
    /// Which forwarding attempt this row accounts for, counted from zero.
    ///
    /// One client request can pay for two: a quality gate can abandon a cheap
    /// answer and retry a rung up. Both are real spend, so both need a row, and
    /// the request id alone cannot tell them apart.
    ///
    /// Recorded now, load-bearing after the ledger's primary key contracts onto
    /// `(request_id, attempt)`. Until then the second row is dropped.
    pub attempt: i16,
    /// Whether the credential that served this request is a flat-rate seat.
    ///
    /// A subscription seat has already been paid for by its monthly fee, so the
    /// marginal dollar cost of one more request is zero — and recording the
    /// model's list price as `cost_usd` would put money in the ledger that no
    /// invoice will ever match. The list price is still worth knowing, as the
    /// pay-per-token bill the subscription displaced; it goes to
    /// `counterfactual_api_usd` instead.
    pub flat_rate: bool,
}

/// What became of the attempt being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// The client got this answer.
    Served,
    /// Paid for, judged unusable, and replaced by a retry a rung up.
    Abandoned,
}

/// Append one row and emit the matching metrics.
///
/// Runs after the response has been fully streamed, in the pump's task rather
/// than the request's — so a client that hung up early still gets billed for
/// what the provider generated.
pub async fn record(state: &AppState, ctx: &Context, outcome: &StreamOutcome) {
    let gate = outcome.accumulator.quality_gate();
    record_with_gate(state, ctx, outcome, gate, true, Fate::Served).await;
}

async fn record_with_gate(
    state: &AppState,
    ctx: &Context,
    outcome: &StreamOutcome,
    gate: Option<oag_router::QualityGate>,
    streamed: bool,
    fate: Fate,
) {
    let write = usage_write(ctx, outcome, gate, streamed, fate);
    let (usage, cost, counterfactual) = (write.usage, write.cost_usd, write.counterfactual_usd);

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

/// The ledger row for one attempt.
///
/// Pure, because the identity of a row is the part worth pinning: two attempts
/// of the same client request must be distinguishable from each other and still
/// attributable to the one request the client made.
fn usage_write(
    ctx: &Context,
    outcome: &StreamOutcome,
    gate: Option<oag_router::QualityGate>,
    streamed: bool,
    fate: Fate,
) -> UsageWrite {
    let usage = *outcome.accumulator.usage();

    // What these tokens list for at the served model's own API price. For a
    // metered credential this *is* the cost; for a flat-rate seat it is the
    // pay-per-token bill the subscription displaced, and the actual marginal
    // cost is zero. Recorded on every row so a SUM across mixed traffic is
    // meaningful: metered rows contribute nothing to (api - cost), seat rows
    // contribute exactly what the seat saved.
    let api_equivalent = ctx.decision.model.pricing.cost(&usage);
    let cost = if ctx.flat_rate {
        Decimal::ZERO
    } else {
        api_equivalent
    };

    // What the same tokens would have cost on the route's top rung. The
    // difference, summed, is the number that justifies the gateway — which is
    // why an abandoned attempt gets none of it. There is one baseline per client
    // request, and the served row already carries it; giving the attempt we
    // threw away a second full-price baseline would book its wasted cost as
    // savings, so the requests that cost us the most extra would be the ones
    // reporting that they saved the most.
    let (counterfactual, counterfactual_model) = match fate {
        Fate::Served => (
            ctx.decision
                .ceiling_model
                .as_ref()
                .map_or(cost, |m| m.pricing.cost(&usage)),
            ctx.decision
                .ceiling_model
                .as_ref()
                .map(|m| m.id.as_str().to_owned()),
        ),
        Fate::Abandoned => (Decimal::ZERO, None),
    };

    // `escalated_from_tier` is set only when we actually climbed a rung;
    // `escalation_gate` is set whenever a gate tripped. Keeping them separate
    // is what lets you count missed escalation opportunities — the streamed
    // requests where the answer was unusable and it was too late to retry.
    let escalated_from = match &ctx.decision.reason {
        SelectionReason::Escalated { from, .. } => Some(from.to_string()),
        _ => None,
    };

    UsageWrite {
        request_id: ctx.request_id.as_uuid(),
        attempt: ctx.attempt,
        principal_id: Some(ctx.auth.principal_id),
        api_key_id: Some(ctx.auth.api_key_id),
        route_id: Some(ctx.auth.route_id),
        account_id: Some(ctx.account.as_uuid()),
        model_id: ctx.decision.model.id.as_str().to_owned(),
        // Empty when the model sat on no rung (passthrough off-ladder). The
        // column is NOT NULL; inventing `cheap` is what made a named Grok
        // request show up as a cheap-rung spend in the dashboard.
        tier: ctx.decision.rung_name().unwrap_or("").to_owned(),
        selection_reason: match fate {
            Fate::Served => reason_label(&ctx.decision.reason).to_owned(),
            // Why this model was picked matters less on this row than the fact
            // that its answer was thrown away, and that is the thing nothing
            // else records: `model_id` and `tier` still say what ran, and
            // `escalation_gate` says what was wrong with what it produced.
            Fate::Abandoned => "abandoned".to_owned(),
        },
        escalated_from_tier: escalated_from,
        escalation_gate: gate.map(|g| format!("{g:?}")),
        usage,
        cost_usd: cost,
        counterfactual_usd: counterfactual,
        counterfactual_model_id: counterfactual_model,
        counterfactual_api_usd: api_equivalent,
        status: if outcome.error.is_some() { 502 } else { 200 },
        latency_ms: i32::try_from(outcome.total.as_millis()).ok(),
        ttft_ms: outcome.ttft.and_then(|d| i32::try_from(d.as_millis()).ok()),
        streamed,
    }
}

/// Record a non-streamed response.
///
/// Takes the quality gate whether or not we acted on it. A gate we could not
/// act on still belongs in the ledger: it is the signal that a rung is
/// mis-configured for this workload, and it is invisible everywhere else.
pub async fn record_collected(
    state: &AppState,
    ctx: &Context,
    accumulator: &oag_proto::StreamAccumulator,
    gate: Option<oag_router::QualityGate>,
) {
    let outcome = collected(ctx, accumulator);
    record_with_gate(state, ctx, &outcome, gate, false, Fate::Served).await;
}

/// An attempt the quality gate condemned, captured at the moment we gave up on
/// it and held until the request it belongs to is finished.
///
/// Captured rather than written there and then because of when it has to reach
/// the ledger, not what it contains. Until the primary key is contracted onto
/// `(request_id, attempt)`, only the first row for a request survives — and the
/// row that has to survive is the one the client was served. Captured here, its
/// latency and usage are still its own rather than the retry's.
#[derive(Debug, Clone)]
pub struct Abandoned {
    ctx: Context,
    outcome: StreamOutcome,
    gate: oag_router::QualityGate,
}

/// Take note of an attempt we are about to abandon.
pub fn abandon(
    ctx: Context,
    accumulator: &oag_proto::StreamAccumulator,
    gate: oag_router::QualityGate,
) -> Abandoned {
    let outcome = collected(&ctx, accumulator);
    Abandoned { ctx, outcome, gate }
}

/// Record an attempt we paid for and then threw away.
///
/// The provider generated those tokens and will invoice them; the quality gate
/// only got an opinion afterwards. Leaving the row out does not make the spend
/// go away, it makes escalation look free — and "what did escalation cost us
/// this month" is the whole question the ledger exists to answer.
///
/// The row carries the client's request id, so it stays attributable to the one
/// request that was made, and `attempt` plus a `selection_reason` of
/// `abandoned` separate it from the answer that was actually served. Until the
/// ledger's key contracts, this write loses to the served row it follows and is
/// dropped; the served row is the one that must not be.
pub async fn record_abandoned(state: &AppState, abandoned: &Abandoned) {
    record_with_gate(
        state,
        &abandoned.ctx,
        &abandoned.outcome,
        Some(abandoned.gate),
        false,
        Fate::Abandoned,
    )
    .await;
}

/// The outcome of a response that arrived in one piece.
fn collected(ctx: &Context, accumulator: &oag_proto::StreamAccumulator) -> StreamOutcome {
    StreamOutcome {
        accumulator: accumulator.clone(),
        // A non-streamed response has no meaningful first-token time: the whole
        // body arrives at once. Reporting the total here would quietly corrupt
        // the TTFT histogram with values that are not TTFT.
        ttft: None,
        total: ctx.started.elapsed(),
        client_gone: false,
        error: None,
    }
}

fn emit_metrics(
    ctx: &Context,
    outcome: &StreamOutcome,
    usage: &oag_router::Usage,
    cost: Decimal,
    counterfactual: Decimal,
) {
    let model = ctx.decision.model.id.as_str().to_owned();
    let tier = ctx.decision.rung_name().unwrap_or("").to_owned();
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

    fn context(attempt: i16) -> Context {
        use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
        use uuid::Uuid;

        Context {
            request_id: RequestId::new(),
            auth: oag_store::AuthContext {
                api_key_id: Uuid::new_v4(),
                principal_id: Uuid::new_v4(),
                route_id: Uuid::new_v4(),
                key_floor_tier: None,
                admin: false,
                quota_usd: None,
                spent_usd: Decimal::ZERO,
                principal_budget_usd: None,
                principal_hard_stop_multiple: Decimal::ONE,
                principal_spent_usd: Decimal::ZERO,
            },
            decision: RoutingDecision {
                model: ModelSpec {
                    id: ModelId::new("kimi/k2"),
                    provider: oag_core::Provider::Kimi,
                    upstream_name: "k2".to_owned(),
                    pricing: Pricing {
                        input_per_mtok: Decimal::ONE,
                        output_per_mtok: Decimal::ONE,
                        cache_read_per_mtok: None,
                        cache_write_per_mtok: None,
                    },
                    context_window: 1_000,
                    max_output_tokens: 100,
                    capabilities: Capabilities::default(),
                    display_label: None,
                },
                tier: Some(oag_core::Tier::new("cheap", 0)),
                reason: SelectionReason::Classified,
                capability_escalated_from: None,
                // Ten times the price, so a row that wrongly claims the full
                // baseline is visibly different from one that claims none.
                ceiling_model: Some(ModelSpec {
                    id: ModelId::new("frontier/big"),
                    provider: oag_core::Provider::Anthropic,
                    upstream_name: "big".to_owned(),
                    pricing: Pricing {
                        input_per_mtok: Decimal::TEN,
                        output_per_mtok: Decimal::TEN,
                        cache_read_per_mtok: None,
                        cache_write_per_mtok: None,
                    },
                    context_window: 1_000,
                    max_output_tokens: 100,
                    capabilities: Capabilities::default(),
                    display_label: None,
                }),
            },
            account: oag_core::AccountId::from_uuid(Uuid::new_v4()),
            started: Instant::now(),
            attempt,
            flat_rate: false,
        }
    }

    fn outcome(output_tokens: u64) -> StreamOutcome {
        let mut accumulator = oag_proto::StreamAccumulator::new();
        accumulator.observe(&oag_proto::StreamEvent::TextDelta {
            text: "an answer".to_owned(),
        });
        accumulator.observe(&oag_proto::StreamEvent::UsageUpdate {
            usage: oag_router::Usage {
                input_tokens: 1_000,
                output_tokens,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        StreamOutcome {
            accumulator,
            ttft: None,
            total: std::time::Duration::from_millis(10),
            client_gone: false,
            error: None,
        }
    }

    #[test]
    fn the_ledger_records_the_model_the_router_chose_whatever_the_client_typed() {
        // `usage_event.model_id` is a join key: every cost report, every
        // per-model rollup and every catalog join reads it. It has to be the
        // canonical id, never a decorated spelling of it — a `@sub` or an
        // `anthropic/` prefix reaching this column would split one model's
        // spend across three names that no query knows are the same model.
        //
        // Which channel actually served the request is already recorded, in
        // `account_id`, which is the same information without the split.
        let ctx = context(0);
        let row = usage_write(&ctx, &outcome(300), None, false, Fate::Served);
        assert_eq!(row.model_id, ctx.decision.model.id.as_str());
        assert!(!row.model_id.contains('@'), "{}", row.model_id);
        assert!(row.account_id.is_some(), "the channel is recorded here");
    }

    #[test]
    fn a_metered_row_prices_actual_and_api_equivalent_identically() {
        // The invariant that keeps a mixed-traffic SUM honest: on a metered
        // credential the API-equivalent price *is* the cost, so (api - cost)
        // is zero and contributes nothing to the subscription-savings figure.
        let ctx = context(0); // flat_rate: false
        let row = usage_write(&ctx, &outcome(300), None, false, Fate::Served);
        assert!(row.cost_usd > Decimal::ZERO);
        assert_eq!(
            row.counterfactual_api_usd, row.cost_usd,
            "a metered row must not look like it saved anything against API pricing"
        );
    }

    #[test]
    fn a_seat_row_costs_nothing_and_books_the_avoided_api_bill() {
        // A flat-rate seat: the tokens are paid for by the monthly fee, so the
        // marginal cost is zero, and what would have been the cost becomes the
        // pay-per-token bill the subscription displaced.
        let mut ctx = context(0);
        ctx.flat_rate = true;
        let row = usage_write(&ctx, &outcome(300), None, false, Fate::Served);

        assert_eq!(
            row.cost_usd,
            Decimal::ZERO,
            "no invoice will match a per-request charge on a flat-rate seat"
        );
        assert!(
            row.counterfactual_api_usd > Decimal::ZERO,
            "the API bill the seat displaced is the whole point of recording it"
        );
        // The subscription's worth on this row is exactly what it displaced.
        assert_eq!(
            row.counterfactual_api_usd - row.cost_usd,
            row.counterfactual_api_usd
        );
        // The frontier baseline is still recorded; it is a different question
        // (what the top rung would have cost) and stays on its own column.
        assert!(row.counterfactual_usd > Decimal::ZERO);
    }

    #[test]
    fn escalation_meters_the_abandoned_attempt_as_its_own_row() {
        // The bug: on escalation the loop retried without recording anything
        // for the attempt it threw away, so the cheap model's tokens vanished
        // and escalation looked free. Both rows have to exist, be tellable
        // apart, and still name the one request the client made.
        let mut first = context(0);
        let abandoned = usage_write(
            &first,
            &outcome(20),
            Some(QualityGate::Refusal),
            false,
            Fate::Abandoned,
        );

        // What the retry a rung up looks like: same client request, next
        // attempt, and a reason that says it was escalated into.
        first.attempt = 1;
        first.decision.reason = SelectionReason::Escalated {
            from: TierName::new("cheap"),
            gate: QualityGate::Refusal,
        };
        let served = usage_write(&first, &outcome(300), None, false, Fate::Served);

        assert_eq!(
            abandoned.request_id, served.request_id,
            "both rows belong to the one request the client made"
        );
        assert_ne!(
            abandoned.attempt, served.attempt,
            "and the ledger's key has to separate them, or one overwrites the other"
        );
        assert_eq!(abandoned.selection_reason, "abandoned");
        assert_eq!(served.selection_reason, "escalated");
        assert_eq!(
            abandoned.escalation_gate.as_deref(),
            Some("Refusal"),
            "the abandoned row names why its answer was dropped"
        );
        assert_eq!(
            abandoned.usage.output_tokens, 20,
            "the tokens we were billed for, not zero"
        );
        assert!(
            abandoned.cost_usd > Decimal::ZERO,
            "escalation is not free, and the ledger has to say so"
        );

        // The savings figure is `SUM(counterfactual - cost)`, so a baseline on
        // the abandoned row is not a harmless duplicate: it books the cost of
        // the answer we threw away as money saved. Escalated requests, the ones
        // that cost the most extra, would report saving the most.
        assert_eq!(
            abandoned.counterfactual_usd,
            Decimal::ZERO,
            "there is one baseline per client request, and the served row has it"
        );
        assert_eq!(abandoned.counterfactual_model_id, None);
        assert!(
            served.counterfactual_usd > served.cost_usd,
            "the served row still carries the real baseline"
        );
        assert_eq!(
            served.counterfactual_model_id.as_deref(),
            Some("frontier/big")
        );
        assert!(
            served.counterfactual_usd - served.cost_usd - abandoned.cost_usd > Decimal::ZERO,
            "and the two rows together net out to a saving, not an invented one"
        );
    }
}
