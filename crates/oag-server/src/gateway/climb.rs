//! Climbing the ladder when an answer is not good enough, and metering each attempt.

use super::failover::{Attempt, forward_with_failover};
use super::plan::Plan;
use super::respond::{json_response, stream_response};
use super::{meter, select};
use crate::AppState;
use axum::response::Response;
use oag_core::provider::Dialect;
use oag_core::{RequestId, Result};
use oag_pool::SessionKey;
use oag_router::{RoutingDecision, RoutingPolicy};
use std::sync::Arc;
use std::time::Instant;

/// Whether the budget, and nothing else, is what stopped this climb.
///
/// G4. `oag_escalations_suppressed_total` answers one question — how much
/// answer quality is my budget costing me — and it was incremented whenever a
/// gate tripped and the principal happened to be near their cap, regardless of
/// whether the budget had anything to do with it. Four other things stop a
/// climb, and on a constrained principal every one of them was counted as the
/// budget's doing, so the number an operator would act on was inflated by
/// exactly the cases where raising the cap would change nothing.
///
/// The question is counterfactual and has to be: would this have climbed if the
/// principal had headroom? Everything but the pressure is re-asked with
/// `Normal` substituted — including `policy.escalate`, because a gate with no
/// rung above it is not a suppression whatever the budget says.
///
/// A named predicate rather than a condition inline at the counter, because
/// the counter sits inside `run_with_escalation` and cannot be reached without
/// a credential, a database and a live request. The rule is the part worth
/// testing and this is the shape that lets it be tested.
#[allow(clippy::too_many_arguments)]
pub(super) fn budget_alone_prevented_the_climb(
    gate: Option<oag_router::QualityGate>,
    pressure: oag_router::BudgetPressure,
    decision: &RoutingDecision,
    escalations: u8,
    max_tokens: u32,
    policy: &RoutingPolicy,
    signal: &oag_router::RequestSignal,
    catalog: &oag_router::Catalog,
    served: &std::collections::HashSet<String>,
) -> bool {
    let Some(gate) = gate else { return false };
    if pressure == oag_router::BudgetPressure::Normal {
        return false;
    }
    if !should_climb(
        &decision.reason,
        gate,
        oag_router::BudgetPressure::Normal,
        escalations,
        max_tokens,
        decision.model.max_output_tokens,
    ) {
        return false;
    }
    let Some(from) = decision.tier.as_ref() else {
        return false;
    };
    policy
        .escalate(from, gate, signal, catalog, max_tokens, served)
        .is_some()
}

/// Whether this attempt may be retried one rung up.
///
/// Failover (same model, another credential) is a different path. Climbing
/// changes the model. A named passthrough request must not walk onto the
/// next ladder provider; hitting the caller's own `max_tokens` is not a
/// weaker-model failure; budget pressure must not undo a downgrade.
pub(super) fn should_climb(
    reason: &oag_router::SelectionReason,
    gate: oag_router::QualityGate,
    pressure: oag_router::BudgetPressure,
    escalations: u8,
    requested_max: u32,
    model_max: u32,
) -> bool {
    oag_router::climb_allowed(reason)
        && !(gate == oag_router::QualityGate::Truncated
            && oag_router::truncated_by_client_cap(requested_max, model_max))
        && oag_router::escalation_allowed(pressure, escalations, MAX_ESCALATIONS)
}

/// Forward, failing over between credentials, and escalate a rung if the
/// answer comes back unusable.
///
/// Escalation sits *outside* failover, and the nesting is the point: failover
/// asks "is this credential healthy", escalation asks "is this model good
/// enough". Collapsing them would mean a provider outage silently migrated the
/// fleet onto expensive models.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn run_with_escalation(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    plan: Plan,
    canonical: &mut oag_proto::CanonicalRequest,
    session: &SessionKey,
    request_id: RequestId,
    started: Instant,
    ingress: Dialect,
    guard: crate::shutdown::InFlightGuard,
) -> Result<Response> {
    let Plan {
        policy,
        mut decision,
        signal,
        catalog,
        pressure,
        channel,
        served,
    } = plan;
    let mut escalations = 0u8;
    // The gate that *caused* an escalation, not the last one observed. Recording
    // the final attempt's gate would leave this empty on exactly the rows where
    // it matters, because a successful escalation trips no gate.
    let mut triggering_gate: Option<oag_router::QualityGate> = None;
    // The attempts this request paid for and did not serve: one a gate
    // condemned, and any the stream lost on the way to a credential that
    // worked. Held rather than written where they happen so that all of a
    // request's ledger writes run together on one detached task — off the
    // request future, which a client hanging up cancels.
    let mut abandoned: Option<meter::Abandoned> = None;
    let mut lost: Vec<meter::Lost> = Vec::new();
    let mut dispatches = Dispatches::new(request_id, started);

    loop {
        let attempt = match forward_with_failover(
            state,
            auth,
            &decision,
            canonical,
            session,
            &mut dispatches,
            &mut lost,
            channel,
        )
        .await
        {
            Ok(attempt) => attempt,
            Err(e) => {
                // Some failures here are about this rung rather than about the
                // request, and `Error::disposition` already says which: an
                // empty credential pool, a seat held back by its reserve, and a
                // rung with no viable model all classify as `EscalateTier`.
                // Nothing read that. A route whose only Anthropic seat was
                // parked at its reserve returned 503 while a frontier rung
                // naming a different provider sat there able to serve.
                //
                // Only to a rung naming a *different* provider: another rung
                // on the same one re-runs the selection that has just failed
                // for a reason the rung cannot change. Same-provider rungs are
                // skipped rather than settled for — looking one rung up left a
                // `[kimi, kimi-2, anthropic]` ladder returning 503 when kimi's
                // only seat was at its reserve, because kimi-2 was all it
                // looked at. Skipping is not escalating: nothing is dispatched
                // to the rungs passed over, so the climb costs one escalation.
                //
                // `climb_allowed` still applies. A caller who named a model
                // must not be quietly moved onto another provider's, however
                // unavailable theirs is — that is the same rule that stops a
                // quality gate doing it.
                //
                // And so does the budget, by `escalation_allowed`, which is the
                // same call `should_climb` makes sixty lines below. This branch
                // checked the escalation count alone, so a principal the router
                // had just downgraded for being near their cap — reason
                // `BudgetDowngraded`, which `climb_allowed` permits — was
                // promoted to a rung fifteen times dearer the moment their
                // cheap rung had no credential, and debited against the very
                // budget the downgrade was protecting.
                //
                // The two paths ask the same question and gave opposite
                // answers, which is the defect whichever answer is right. This
                // is the documented one: the refusal is a 503 naming the rung
                // that could not be dispatched to, which is a truthful answer an
                // operator can act on, and `hard_stop_multiple` remains the
                // wall rather than this.
                if matches!(e.disposition(), oag_core::Disposition::EscalateTier)
                    && oag_router::climb_allowed(&decision.reason)
                    && oag_router::escalation_allowed(pressure, escalations, MAX_ESCALATIONS)
                    && let Some(from) = decision.tier.as_ref()
                    && let Some(next) = policy.escalate_past_provider(
                        from,
                        decision.model.provider,
                        &signal,
                        &catalog,
                        canonical.max_tokens,
                        &served,
                    )
                {
                    tracing::info!(
                        %request_id, from = ?decision.rung_name(), to = ?next.rung_name(),
                        error = %e,
                        "escalating: nothing on this rung could be dispatched to"
                    );
                    metrics::counter!(
                        "oag_escalations_total",
                        "from" => from.name.as_str().to_owned(),
                        "gate" => "NoCredential".to_owned(),
                    )
                    .increment(1);
                    canonical.model.clone_from(&next.model.upstream_name);
                    decision = next;
                    escalations += 1;
                    triggering_gate = Some(oag_router::QualityGate::NoCredential);
                    continue;
                }

                // Nowhere left to go. There is no served row to come — but
                // every attempt made to get here was generated and invoiced,
                // and failing the request does not make that spend go away.
                spawn_unserved(state, abandoned, lost);
                return Err(e);
            }
        };

        // An answer to judge, or a refusal to escalate on. Both ask the same
        // question — is a rung up worth trying — so they share the one loop
        // below rather than growing a second escalation path.
        let answer = match attempt {
            // Streaming: the bytes are already on their way to the client, so
            // there is nothing left to judge. See the note on MAX_ESCALATIONS.
            Attempt::Streaming {
                response,
                lease,
                attempt,
                names,
                adapter,
            } => {
                // Empty as the code stands: `Outcome::Lost` is produced only
                // after `succeeded` has decided the client did not ask for a
                // stream, so a streamed request cannot have captured one. This
                // is here because that invariant lives three functions away,
                // and the failure if it ever changes is a ledger row that
                // silently never gets written. Costs a branch on an empty Vec.
                spawn_unserved(state, abandoned, lost);
                return stream_response(
                    state,
                    response,
                    lease,
                    auth,
                    &decision,
                    request_id,
                    started,
                    attempt,
                    ingress,
                    triggering_gate,
                    guard,
                    names,
                    adapter,
                );
            }
            Attempt::Rejected(e) => Err(e),
            Attempt::Collected {
                body,
                events,
                accumulator,
                lease,
                attempt,
                adapter,
            } => Ok((body, events, accumulator, lease, attempt, adapter)),
        };

        let gate = match &answer {
            // The model would not take the request at all. Nothing was billed
            // and nothing reached the client, so this is the one escalation a
            // streaming request can also take.
            Err(_) => Some(oag_router::QualityGate::ContextOverflow),
            Ok((_, _, accumulator, _, _, _)) => accumulator.quality_gate(),
        };

        // Retry one rung up when the answer was unusable and a rung is left.
        if let Some(gate) = gate
            && should_climb(
                &decision.reason,
                gate,
                pressure,
                escalations,
                canonical.max_tokens,
                decision.model.max_output_tokens,
            )
            && let Some(from) = decision.tier.as_ref()
            && let Some(next) =
                policy.escalate(from, gate, &signal, &catalog, canonical.max_tokens, &served)
        {
            tracing::info!(
                %request_id, from = ?decision.rung_name(), to = ?next.rung_name(), ?gate,
                "escalating: this rung could not answer the request"
            );
            metrics::counter!(
                "oag_escalations_total",
                "from" => from.name.as_str().to_owned(),
                "gate" => format!("{gate:?}"),
            )
            .increment(1);

            // A rejection released its lease on the way out and was never
            // generated, so it is not owed a ledger row. A collected answer
            // still holds a lease, and the provider already invoiced those
            // tokens — capture the abandoned attempt here, while its latency
            // and usage are still its own rather than the retry's, and let the
            // detached task at the end of the request write it.
            //
            // At most one of these is ever outstanding: `MAX_ESCALATIONS` is 1,
            // so a second pass through this branch cannot happen and no
            // capture is overwritten.
            //
            // Released here rather than left to the drop, and awaited: the
            // rung above may pick this same credential, and a release still
            // in flight would look like a credential with no room.
            if let Ok((_, _, accumulator, lease, attempt, _)) = &answer {
                abandoned = Some(meter::abandon(
                    meter_context(auth, &decision, lease, request_id, started, *attempt),
                    accumulator,
                    gate,
                ));
                lease.release().await;
            }
            canonical.model.clone_from(&next.model.upstream_name);
            decision = next;
            escalations += 1;
            triggering_gate = Some(gate);
            continue;
        }

        // Counted only when budget pressure was the *sole* reason we did not
        // climb. There are four other reasons — a passthrough request must not
        // be migrated onto a model it did not name, the caller's own
        // `max_tokens` is not a weaker-model failure, the escalation ceiling is
        // reached, and there may simply be no rung above — and this counter
        // fired on all of them, so long as the principal happened to be near
        // their cap. An operator reading it to answer "how much quality is my
        // budget costing me" was reading a number mostly made of requests the
        // budget had nothing to do with.
        //
        // The test is counterfactual and has to be: would this have climbed if
        // the principal had headroom? Everything but the pressure is re-asked
        // with `Normal` substituted, including `policy.escalate`, because a
        // gate with no rung above it is not a suppression whatever the budget
        // says.
        if budget_alone_prevented_the_climb(
            gate,
            pressure,
            &decision,
            escalations,
            canonical.max_tokens,
            &policy,
            &signal,
            &catalog,
            &served,
        ) {
            tracing::info!(
                %request_id, ?gate,
                "not escalating: this principal is near their budget, so a worse \
                 answer is the intended outcome"
            );
            metrics::counter!("oag_escalations_suppressed_total").increment(1);
        }

        // No rung left to try. A refusal is now the caller's error — the same
        // one they used to get before the first attempt was allowed to climb.
        // The attempts made to get here were still generated and invoiced, and
        // with no served row coming this is their last chance to reach the
        // ledger — exactly as when the retry itself died above.
        let (body, events, accumulator, lease, attempt, adapter) = match answer {
            Ok(answer) => answer,
            Err(e) => {
                spawn_unserved(state, abandoned, lost);
                return Err(e);
            }
        };

        // Either it was fine, or nothing better exists. Record the gate either
        // way: a gate we could not act on is exactly the signal that a rung is
        // mis-set for this workload.
        let ctx = meter_context(auth, &decision, &lease, request_id, started, attempt);
        // Both facts about the adapter that sent the request and read its
        // answer, which the attempt carries. Looked up again here, a reload
        // in between answered with another adapter or none, and the fallback
        // to the provider's dialect is exactly the bug this call site had.
        let (upstream_dialect, always_streams) = (adapter.dialect(), adapter.always_streams());
        let rewrite_tool_names = accumulator.function_names().rewrites();
        // Before the ledger write, which is ours rather than the credential's.
        lease.release().await;

        // The ledger writes run as their own task, exactly as the streamed
        // path's do. This future is the request handler's: a client that hangs
        // up while the writes are in flight cancels it, and an inline `.await`
        // here went with it — no row, no debit, while the provider had already
        // generated and invoiced the answer. Detached, the writes finish
        // whatever the connection does. The guard rides along so a shutdown
        // drain waits for them rather than exiting out from under the ledger.
        //
        // `triggering_gate` when we escalated, otherwise whatever this attempt
        // tripped — so the ledger always names the reason, never nothing.
        let recorded_gate = triggering_gate.or(gate);
        let state2 = Arc::clone(state);
        tokio::spawn(async move {
            let _guard = guard;
            meter::record_collected(&state2, &ctx, &accumulator, recorded_gate).await;
            // After the served row, deliberately. Since 0014 contracted the key
            // onto `(request_id, attempt)` all of these land, so the order no
            // longer decides which survives — but it still decides which row a
            // reader sees first, and the answer the client got is the one that
            // should be there before the attempts that failed to be it.
            if let Some(abandoned) = &abandoned {
                meter::record_abandoned(&state2, abandoned).await;
            }
            for lost in &lost {
                meter::record_lost(&state2, lost).await;
            }
        });

        return Ok(json_response(
            &body,
            &events,
            gate,
            &decision,
            request_id,
            ingress,
            upstream_dialect,
            always_streams,
            rewrite_tool_names,
        ));
    }
}

/// Write the rows a request that served nothing still owes.
///
/// There is no served row to follow here — the request failed — but every
/// attempt captured on the way was generated by a provider and will appear on
/// an invoice. Detached for the same reason the served path detaches: this runs
/// on the way out of a request whose client may have hung up already, and an
/// inline `.await` on the request's own future goes with it when it does.
///
/// Takes its own in-flight guard rather than the request's: a shutdown drain
/// must still wait for these writes, and one caller will hand the
/// request's guard to the streaming path. Nothing to write takes no guard.
pub(super) fn spawn_unserved(
    state: &Arc<AppState>,
    abandoned: Option<meter::Abandoned>,
    lost: Vec<meter::Lost>,
) {
    if abandoned.is_none() && lost.is_empty() {
        return;
    }
    let guard = state.lifecycle.track();
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let _guard = guard;
        if let Some(abandoned) = &abandoned {
            meter::record_abandoned(&state, abandoned).await;
        }
        for lost in &lost {
            meter::record_lost(&state, lost).await;
        }
    });
}

/// What the ledger needs about one forwarding attempt.
///
/// Built in one place because an attempt is metered from three: the streamed
/// path, the collected path, and the attempt a quality gate abandons. A second
/// copy of this literal is how one of them ends up attributing spend to the
/// wrong account.
/// The count of times this request has been sent to a provider, shared by
/// the loops that send it.
///
/// The ledger's identity is `(request_id, attempt)`, and two loops dispatch:
/// escalation climbs rungs, and failover inside it switches credentials. The
/// number used to be the escalation index alone, so two dispatches on one
/// rung — a credential that generated an answer and then lost the stream,
/// and the credential that served the retry — shared a row identity, and
/// only one of them reached the ledger. Every dispatch now takes the next
/// number, whichever loop asked.
pub(super) struct Dispatches {
    pub(super) request_id: RequestId,
    pub(super) started: Instant,
    next: u8,
}

impl Dispatches {
    const fn new(request_id: RequestId, started: Instant) -> Self {
        Self {
            request_id,
            started,
            next: 0,
        }
    }

    /// The number for the dispatch about to happen.
    pub(super) fn take(&mut self) -> u8 {
        let attempt = self.next;
        self.next = self.next.saturating_add(1);
        attempt
    }
}

pub(super) fn meter_context(
    auth: &oag_store::AuthContext,
    decision: &RoutingDecision,
    lease: &select::Lease,
    request_id: RequestId,
    started: Instant,
    attempt: u8,
) -> meter::Context {
    meter::Context {
        request_id,
        auth: auth.clone(),
        decision: decision.clone(),
        account: lease.account.account_id(),
        started,
        attempt: i16::from(attempt),
        // An unrecognised kind is treated as metered: better to record a real
        // per-request cost than to silently zero one because a discriminator
        // was misspelled.
        flat_rate: oag_core::credential::CredentialKind::from_column(&lease.account.kind)
            .is_some_and(oag_core::credential::CredentialKind::flat_rate),
    }
}

/// How many rungs one request may climb.
///
/// One. A second escalation would mean the classifier was wrong by two rungs,
/// which is a configuration problem to fix rather than a cost to keep paying at
/// runtime.
///
/// **Reactive escalation applies only to non-streaming requests**, and that is
/// a real limit rather than an oversight: a quality gate is knowable only once
/// the answer is complete, and by then a streamed response has already been
/// delivered. Retrying would mean the client saw two answers.
///
/// Streamed responses still have their gate recorded, so an operator can see
/// how often a rung produces unusable answers and move the rung — which is the
/// durable fix anyway.
///
/// A *rejected* request is the exception, streamed or not: the upstream refused
/// it before sending anything, so there is no half-delivered answer to protect
/// and the client has seen nothing to contradict.
pub(super) const MAX_ESCALATIONS: u8 = 1;
