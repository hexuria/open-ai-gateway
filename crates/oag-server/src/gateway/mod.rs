//! The inference request path.

pub mod alias;
pub mod authn;
pub mod count_tokens;
pub mod meter;
pub mod models;
mod presence;
pub mod refresh;
pub mod select;
pub mod sse;

pub use authn::{Caller, require_key_layer};

use crate::AppState;
use crate::breakers::{Breakers, Dispatch};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use oag_core::provider::Dialect;
use oag_core::tier::RoutingMode;
use oag_core::{AccountId, Disposition, Error, RequestId, Result, TierName};
use oag_pool::SessionKey;
use oag_proto::{FunctionNameMap, anthropic, extract_cache_blocks};
use oag_router::{BudgetState, Budgets, RoutingDecision, RoutingPolicy, TierLadder};
use oag_upstream::Transport as _;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// `POST /v1/messages` — the Anthropic-native surface.
///
/// [`Caller`] before `Bytes` is load-bearing, not stylistic: it is a
/// head-only extractor, so an unauthenticated request is answered without the
/// body ever being buffered. See [`authn`].
pub async fn messages(
    State(state): State<Arc<AppState>>,
    Caller(auth): Caller,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    dispatch(state, auth, headers, body, Dialect::AnthropicMessages, None).await
}

/// `POST /v1/chat/completions` — the OpenAI-shaped surface.
///
/// The same pipeline: only the codec at each end differs, which is the point of
/// having a canonical form in the middle.
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Caller(auth): Caller,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    dispatch(
        state,
        auth,
        headers,
        body,
        Dialect::OpenAIChatCompletions,
        None,
    )
    .await
}

/// `POST /v1/responses` — OpenAI's newer surface, and the one their current
/// SDKs default to.
pub async fn responses(
    State(state): State<Arc<AppState>>,
    Caller(auth): Caller,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    dispatch(state, auth, headers, body, Dialect::OpenAIResponses, None).await
}

/// What the Gemini dialect carries in the path rather than in the body.
///
/// Handed down as an argument. It used to travel *inside* the body: the handler
/// parsed the client's JSON, assigned `wire["__oag_model"]` and
/// `wire["__oag_stream"]`, and re-encoded the whole document for `handle` to
/// read the two fields back out of. Two things were wrong with that. `IndexMut`
/// on a `serde_json::Value` panics for anything that is not an object or null,
/// and the re-encode copied a body that may be up to `server.max_body_bytes`
/// for the sake of two values the handler already had in hand.
#[derive(Debug, Clone, Copy)]
struct PathFields<'a> {
    model: &'a str,
    stream: bool,
}

/// `POST /v1beta/models/{model}:generateContent` — the Gemini surface.
///
/// The model and the streaming mode are in the path in this dialect, so they
/// are recovered from it rather than from the body.
pub async fn gemini_generate(
    State(state): State<Arc<AppState>>,
    Caller(auth): Caller,
    axum::extract::Path(model_action): axum::extract::Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (model, action) = model_action
        .rsplit_once(':')
        .unwrap_or((model_action.as_str(), "generateContent"));

    // Every action used to fall through to a billed completion. `:countTokens`
    // in particular leased a credential, ran a full request, metered the spend,
    // and returned a body with no `totalTokens` in it — a preflight call that
    // silently cost money and answered nothing.
    match action {
        "generateContent" | "streamGenerateContent" => {}
        "countTokens" => return count_tokens::gemini_count(&state, &auth, &body).await,
        other => {
            return error_response(&Error::UnsupportedAction {
                action: other.to_owned(),
            });
        }
    }
    let stream = action.starts_with("stream");

    if let Err(e) = require_object_body(&body) {
        return error_response(&e);
    }

    dispatch(
        state,
        auth,
        headers,
        body,
        Dialect::GeminiGenerateContent,
        Some(PathFields { model, stream }),
    )
    .await
}

/// Refuse a body this dialect's pipeline cannot read, as the client error it is.
///
/// It used to be assumed rather than checked: the path's model and mode were
/// written into the parsed body with `IndexMut`, which panics on any `Value`
/// that is not an object or null — so `[]`, `123`, `"x"` or `true` aborted the
/// request task, and with nothing catching the unwind that severed the
/// connection instead of answering on it. On HTTP/2 severing resets every
/// sibling stream multiplexed onto the same connection.
///
/// Deserialising into a map *is* the check. Malformed JSON and well-formed
/// non-objects both fail it, both with a message naming what was wrong, and
/// both are already a 400 through `Error::Serde`.
fn require_object_body(body: &[u8]) -> Result<()> {
    serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(body)
        .map(|_| ())
        .map_err(Error::Serde)
}

async fn dispatch(
    state: Arc<AppState>,
    auth: Arc<oag_store::AuthContext>,
    headers: HeaderMap,
    body: axum::body::Bytes,
    ingress: Dialect,
    path: Option<PathFields<'_>>,
) -> Response {
    // The guard is *moved* down the call chain and, for a streamed response,
    // into the task that pumps it. It must outlive the response body, not just
    // the handler: a handler returns as soon as the headers are decided, and a
    // guard dropped there tells shutdown the request is finished while its
    // stream still has minutes to run — so a rolling deploy severs it.
    let guard = state.lifecycle.track();
    let request_id = RequestId::new();

    match handle(
        &state, &auth, &headers, &body, request_id, ingress, path, guard,
    )
    .await
    {
        Ok(response) => response,
        Err(e) => {
            metrics::counter!("oag_requests_total", "outcome" => "error").increment(1);
            error_response(&e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    state: &Arc<AppState>,
    auth: &Arc<oag_store::AuthContext>,
    headers: &HeaderMap,
    body: &[u8],
    request_id: RequestId,
    ingress: Dialect,
    path: Option<PathFields<'_>>,
    guard: crate::shutdown::InFlightGuard,
) -> Result<Response> {
    let started = Instant::now();

    // Authentication already happened, in `require_key_layer`, before the body
    // extractor ran.
    //
    // ── parse ─────────────────────────────────────────────────────────────────
    let wire: serde_json::Value = serde_json::from_slice(body)?;
    let mut canonical = match ingress {
        Dialect::OpenAIChatCompletions => oag_proto::openai::parse_request(&wire)?,
        Dialect::OpenAIResponses => oag_proto::responses::parse_request(&wire)?,
        Dialect::GeminiGenerateContent => {
            let mut c = oag_proto::gemini::parse_request(&wire)?;
            // In this dialect the model and the mode are in the path, not the
            // body, so `gemini_generate` passes them down.
            if let Some(PathFields { model, stream }) = path {
                model.clone_into(&mut c.model);
                c.stream = stream;
            }
            c
        }
        _ => anthropic::parse_request(&wire)?,
    };

    // One snapshot, taken here rather than inside `plan_request`, because the
    // normalisation below and the routing that follows it must agree about what
    // the catalog holds: a refresh landing between two snapshots could strip a
    // name down to a model the router then cannot find.
    let catalog = state.catalog().await;

    // The single place an inbound model name is normalised. Claude Code only
    // keeps discovered ids that start with `anthropic`, so the listing offers
    // prefixed twins and this is where one comes back; an `@api` / `@sub`
    // qualifier comes off here too. Everything downstream — `virtual_tier`, the
    // passthrough lookup, the ledger — sees the canonical name, and the pin
    // travels beside it as a value rather than inside the string. See
    // [`alias`].
    let alias::Normalised { model, channel } = alias::normalise(&canonical.model, &catalog)?;
    if let Some(canonical_name) = model {
        canonical.model = canonical_name;
    }

    let plan = plan_request(state, auth, &canonical, headers, catalog, channel).await?;

    tracing::info!(
        %request_id,
        model = %plan.decision.model.id,
        tier = ?plan.decision.rung_name(),
        reason = ?plan.decision.reason,
        "routed"
    );

    // The upstream must be told the model the router chose, not the virtual
    // name the client asked for.
    canonical.model = plan.decision.model.upstream_name.clone();

    let cache_blocks = extract_cache_blocks(&canonical);
    let session = SessionKey::resolve(
        &auth.principal_id.to_string(),
        canonical.client_session.as_deref(),
        &cache_blocks,
        &auth.api_key_id.to_string(),
        plan.decision.model.id.as_str(),
    );

    run_with_escalation(
        state,
        auth,
        plan,
        &mut canonical,
        &session,
        request_id,
        started,
        ingress,
        guard,
    )
    .await
}

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
fn budget_alone_prevented_the_climb(
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
fn should_climb(
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
async fn run_with_escalation(
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
                );
            }
            Attempt::Rejected(e) => Err(e),
            Attempt::Collected {
                body,
                events,
                accumulator,
                lease,
                attempt,
            } => Ok((body, events, accumulator, lease, attempt)),
        };

        let gate = match &answer {
            // The model would not take the request at all. Nothing was billed
            // and nothing reached the client, so this is the one escalation a
            // streaming request can also take.
            Err(_) => Some(oag_router::QualityGate::ContextOverflow),
            Ok((_, _, accumulator, _, _)) => accumulator.quality_gate(),
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
            if let Ok((_, _, accumulator, lease, attempt)) = &answer {
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
        let (body, events, accumulator, lease, attempt) = match answer {
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
        // Read while the lease is still here: both are facts about the adapter
        // this account got, and `release` below takes the account with it.
        // Falling back to the provider's dialect once it is gone is exactly the
        // bug this call site had.
        let (upstream_dialect, always_streams) =
            adapter_for(state, decision.model.provider, &lease.account).map_or_else(
                |_| (decision.model.provider.native_dialect(), false),
                |a| (a.dialect(), a.always_streams()),
            );
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
fn spawn_unserved(
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
struct Dispatches {
    request_id: RequestId,
    started: Instant,
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
    fn take(&mut self) -> u8 {
        let attempt = self.next;
        self.next = self.next.saturating_add(1);
        attempt
    }
}

fn meter_context(
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

/// Everything routing decided, before a single byte goes upstream.
struct Plan {
    policy: RoutingPolicy,
    decision: RoutingDecision,
    signal: oag_router::RequestSignal,
    catalog: Arc<oag_router::Catalog>,
    pressure: oag_router::BudgetPressure,
    /// The credential kind the request pinned, from the `@api` / `@sub`
    /// qualifier on the model name.
    ///
    /// Decided at normalisation rather than by routing, and carried here
    /// anyway: its only consumer is credential selection, two calls further
    /// down, and a plan is what already makes that journey. Threading a tenth
    /// argument through the same two frames would be the same coupling written
    /// less visibly.
    channel: Option<oag_core::credential::CredentialKind>,
    /// What the route's credentials serve, for the savings baseline. Read
    /// once for `decide` and carried so `escalate` prices its row against the
    /// same baseline rather than the ladder's top rung.
    served: HashSet<String>,
}

/// Load the caller's route and build the policy it implies.
///
/// Split out of `plan_request` because `/v1/models` needs the route and the
/// policy, and [`budgets_for`] the spend state — none of the rate token or the
/// model decision.
pub(crate) async fn policy_for(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
) -> Result<(oag_store::RouteRow, RoutingPolicy, oag_store::Spend)> {
    let route = oag_store::repo::route_by_id(&state.db, auth.route_id)
        .await?
        .ok_or_else(|| Error::Internal("route vanished between auth and routing".to_owned()))?;
    // Fresh, per request, beside the route: the one read that must never be
    // behind the ledger. See `Spend` for why it is not on `auth`.
    let spend = oag_store::repo::spend_for(&state.db, auth.api_key_id, auth.principal_id).await?;

    let ladder = parse_ladder(&route.tiers)?;

    // A key's floor beats the route's: it is the narrower grant, and the point
    // of pinning one key to `frontier` is that it applies to that key alone.
    let named = auth
        .key_floor_tier
        .as_deref()
        .or(route.floor_tier.as_deref())
        .map(TierName::from);
    let floor = named.as_ref().and_then(|n| ladder.tier(n));
    if let Some(name) = &named
        && floor.is_none()
    {
        // Written by the CLI or by psql, neither of which validates against the
        // ladder. Silently ignoring it means a key pinned to `frontier` quietly
        // serving from `cheap`, with nothing anywhere saying why.
        tracing::warn!(
            route = %route.name,
            floor = %name.as_str(),
            "floor tier names no rung in this ladder; ignoring it"
        );
    }

    let policy = RoutingPolicy::new(ladder, Box::new(oag_router::HeuristicClassifier::default()))
        .with_floor(floor);
    Ok((route, policy, spend))
}

/// The same spend caps inference consults, so `/v1/models` can refuse to
/// advertise what the next turn would hard-stop.
///
/// A per-key quota is a wall at the number written on it: it still degrades
/// through the constrained band first, but it does not get the principal's
/// overshoot grace. An operator who writes `quota_usd = 50` means fifty. A
/// route budget is the same shape at team scope.
///
/// Limits come from `auth`, which is cached; spend comes from `spend`, which
/// is read per request. Keeping the two apart is the whole fix: a cap checked
/// against a cached spend figure was a cap N concurrent requests all passed.
pub(crate) fn budgets_for(
    auth: &oag_store::AuthContext,
    route: &oag_store::RouteRow,
    spend: &oag_store::Spend,
) -> Budgets {
    Budgets {
        key: BudgetState {
            spent_usd: spend.key_usd,
            limit_usd: auth.quota_usd,
            hard_stop_multiple: rust_decimal::Decimal::ONE,
        },
        route: BudgetState {
            spent_usd: route.spent_usd,
            limit_usd: route.monthly_budget_usd,
            hard_stop_multiple: rust_decimal::Decimal::ONE,
        },
        principal: BudgetState {
            spent_usd: spend.principal_usd,
            limit_usd: auth.principal_budget_usd,
            hard_stop_multiple: auth.principal_hard_stop_multiple,
        },
    }
}

/// The rung an `oag/...` model name pins, if any. `oag/auto` pins nothing.
pub(crate) fn virtual_tier(model: &str) -> Option<TierName> {
    model
        .strip_prefix("oag/")
        .filter(|s| *s != "auto")
        .map(TierName::from)
}

/// Every upstream model name this route's credentials say they serve.
///
/// Empty means nothing has been discovered yet, and the caller must read that
/// as "unknown" rather than "nothing" — the same distinction
/// `account.served_models` draws between NULL and an empty array. A failure
/// here degrades the savings baseline and must never fail the request: the
/// answer has been paid for either way.
async fn served_models_for(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
) -> HashSet<String> {
    match oag_store::repo::route_channels(&state.db, auth.route_id, auth.principal_id).await {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|(_, _, served)| served)
            .flatten()
            .collect(),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "served models unavailable; savings baseline falls back to the ladder"
            );
            HashSet::new()
        }
    }
}

/// Resolve the route, build its policy, and choose a model.
///
/// Separated from `handle` because it is the part with no I/O side effects
/// beyond two reads — which makes it the part worth reasoning about on its own.
async fn plan_request(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    canonical: &oag_proto::CanonicalRequest,
    headers: &HeaderMap,
    catalog: Arc<oag_router::Catalog>,
    channel: Option<oag_core::credential::CredentialKind>,
) -> Result<Plan> {
    let (route, policy, spend) = policy_for(state, auth).await?;

    // Throttle before doing any of the expensive work below — classification,
    // model selection, credential selection. A request that is going to be
    // refused should be refused cheaply.
    if let Some(rpm) = route.rpm_limit
        && let Ok(rpm) = u32::try_from(rpm)
        && let Some(retry_after) = state.cache.take_rate_token(route.id, rpm).await?
    {
        return Err(Error::RateLimited { retry_after });
    }

    let mut signal = canonical.signal();

    // `x-oag-tier` outranks the body's model name: the header is what a caller
    // adds deliberately, often when the body is generated by a tool they do not
    // control. Both resolve through the ladder, and an unrecognised rung stays
    // `None` on purpose — `decide` maps an unknown tier to `ladder.floor()`, so
    // a typo that reached it would silently pin the *cheapest* rung.
    let header_tier = headers
        .get("x-oag-tier")
        .and_then(|v| v.to_str().ok())
        .map(TierName::from);
    if let Some(name) = &header_tier
        && policy.rung(name).is_none()
    {
        tracing::warn!(
            tier = %name.as_str(),
            route = %route.name,
            "x-oag-tier names no rung in this ladder; ignoring it"
        );
    }
    // The header is filtered for validity BEFORE the body is consulted.
    //
    // `.map(...).or_else(...)` never reached the body once the header was
    // present, valid or not — so a typo in `x-oag-tier` silently discarded an
    // `oag/frontier` in the request body and the request fell back to
    // classification. Two pins, one of them correct, and the wrong one won by
    // being in the wrong place.
    //
    // The header still outranks the body when it names a real rung: it is what
    // a caller adds deliberately, often because the body is generated by a tool
    // they do not control.
    let requested_tier = header_tier
        .filter(|n| policy.rung(n).is_some())
        .or_else(|| virtual_tier(&canonical.model));
    if let Some(name) = &requested_tier
        && policy.rung(name).is_none()
    {
        tracing::warn!(
            tier = %name.as_str(),
            route = %route.name,
            "requested tier names no rung in this ladder; falling back to classification"
        );
    }
    signal.explicit_tier = requested_tier.filter(|n| policy.rung(n).is_some());

    // An explicit tier is only ever consulted by the classifier, and `decide`
    // only classifies outside its passthrough branch. Without the third arm,
    // `x-oag-tier` and `oag/<rung>` are both no-ops on a stock route, whose
    // `default_mode` is `passthrough`.
    let mode = if canonical.model.starts_with("oag/")
        || route.default_mode == "managed"
        || signal.explicit_tier.is_some()
    {
        RoutingMode::Managed
    } else {
        RoutingMode::Passthrough
    };

    let budget = budgets_for(auth, &route, &spend);

    // Logged at debug because "why did this route the way it did" is the
    // question every routing complaint turns into, and reconstructing it from
    // the ledger afterwards is slower than reading one line.
    tracing::debug!(
        mode = ?mode,
        pressure = ?budget.pressure(),
        binding = %budget.binding(),
        key_spent = %budget.key.spent_usd,
        key_quota = ?budget.key.limit_usd,
        route_spent = %budget.route.spent_usd,
        route_budget = ?budget.route.limit_usd,
        spent = %budget.principal.spent_usd,
        limit = ?budget.principal.limit_usd,
        floor = ?policy.floor_name(),
        "budget and mode"
    );

    // The savings baseline. Taken from what this route's credentials actually
    // serve rather than from the ladder's top rung: see `RoutingPolicy::decide`
    // for why the ladder cannot supply it. Empty means nothing has been asked
    // yet, and the ladder's ceiling stands — the pre-discovery behaviour.
    let served = served_models_for(state, auth).await;
    let decision = match policy.decide(
        &mode,
        Some(&canonical.model),
        &signal,
        &budget,
        &catalog,
        canonical.max_tokens,
        &served,
    ) {
        Ok(d) => d,
        Err(Error::NoViableModel(_)) => {
            return Err(Error::NoViableModel(no_viable_message(
                &route.name,
                &canonical.model,
                policy.ladder(),
            )));
        }
        Err(e) => return Err(e),
    };

    Ok(Plan {
        policy,
        decision,
        signal,
        catalog,
        pressure: budget.pressure(),
        channel,
        served,
    })
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
const MAX_ESCALATIONS: u8 = 1;

/// What one forwarding attempt produced.
enum Attempt {
    /// Handed to the client as a stream. Nothing further can be decided.
    Streaming {
        response: reqwest::Response,
        lease: select::Lease,
        /// Which dispatch of this request produced it; see [`Dispatches`].
        attempt: u8,
        /// Original ↔ wire function names, for restoring `tool_calls`.
        names: FunctionNameMap,
    },
    /// Read in full, so the answer can still be judged and retried.
    Collected {
        /// The upstream's own bytes, for the one case they can be forwarded
        /// as they are: a client in the upstream's dialect, over an adapter
        /// that sent a JSON body rather than a stream. Empty otherwise.
        body: bytes::Bytes,
        /// The answer as canonical events, which is what every other case
        /// renders the client's body from.
        events: Vec<oag_proto::StreamEvent>,
        accumulator: oag_proto::StreamAccumulator,
        lease: select::Lease,
        /// Which dispatch of this request produced it; see [`Dispatches`].
        attempt: u8,
    },
    /// The model refused the request itself — too long, or beyond what it can
    /// do. No credential can help and the lease is already released, but a
    /// rung up can, so this is carried back rather than returned as an error.
    Rejected(Error),
}

/// A complete, non-streamed response.
/// Choose how to produce the client's bytes.
///
/// Passthrough whenever the dialects agree, which is both faster and more
/// faithful — we hand back the bytes the upstream considered correct.
fn egress_for(
    ingress: Dialect,
    decision: &RoutingDecision,
    request_id: RequestId,
    framing: oag_upstream::Framing,
    // The dialect the chosen ADAPTER speaks, which is not always the provider's:
    // a Codex seat is `Provider::OpenAI` and speaks Responses.
    upstream: Dialect,
    // True when tool names were rewritten for the OpenAI wire. Verbatim
    // passthrough would then hand the client the sanitised names, which it
    // cannot dispatch.
    rewrite_tool_names: bool,
) -> Result<sse::Egress> {
    let model = decision.model.id.as_str().to_owned();
    let request_id = request_id.to_string();

    // Matching dialects are not sufficient: passthrough forwards the upstream's
    // *bytes*, so it also requires that those bytes are already SSE. Bedrock's
    // dialect is Anthropic and its framing is binary — passing that through
    // would hand a client expecting `data:` lines a length-prefixed envelope.
    if ingress == upstream && framing == oag_upstream::Framing::Sse && !rewrite_tool_names {
        return Ok(sse::Egress::Passthrough {
            dialect: ingress,
            request_id,
            model,
        });
    }
    Ok(match ingress {
        Dialect::OpenAIChatCompletions => sse::Egress::ChatCompletions { request_id, model },
        Dialect::AnthropicMessages => sse::Egress::AnthropicMessages { request_id, model },
        Dialect::GeminiGenerateContent => sse::Egress::Gemini,
        Dialect::OpenAIResponses => sse::Egress::Responses { request_id, model },
        // Falling back to passthrough here would send the upstream's dialect to
        // a client expecting a different one — bytes that parse as nothing and
        // fail somewhere far from the cause. An error names the problem.
        // `Dialect` is non-exhaustive, so this arm also catches anything added
        // later — the safe direction: a new dialect fails loudly here until
        // someone writes its renderer, rather than silently passing bytes
        // through in the wrong shape.
        _ => {
            return Err(Error::Internal(format!(
                "no renderer from {upstream:?} to {ingress:?}; \
                 route this request to a {ingress:?}-native provider instead"
            )));
        }
    })
}

/// Which adapter this account actually gets.
///
/// An OpenAI OAuth seat is a Codex subscription: the same provider key, a
/// different dialect and backend, so it takes the Codex adapter rather than the
/// Chat Completions one. Every other account uses its provider's adapter.
///
/// ONE PLACE, because the answer is needed twice and the two must agree. The
/// request path picks an adapter to build with; the response path needs the
/// same adapter's dialect and framing to decide whether the upstream's bytes
/// can be forwarded as they are. When only the first knew about Codex seats,
/// the second asked the provider instead, was told Chat Completions, and passed
/// Responses bytes through to a client that reads them as an empty answer.
pub(crate) fn adapter_for(
    state: &AppState,
    provider: oag_core::Provider,
    account: &oag_store::AccountRow,
) -> Result<Arc<dyn oag_upstream::ProviderAdapter>> {
    let is_codex_seat = matches!(provider, oag_core::Provider::OpenAI)
        && oag_core::credential::CredentialKind::from_column(&account.kind)
            .is_some_and(|k| matches!(k, oag_core::credential::CredentialKind::OAuth));
    if is_codex_seat {
        Ok(state.codex_adapter())
    } else {
        state.adapter(provider)
    }
}

/// Function names as an OpenAI-shaped upstream must see them.
///
/// Canonical keeps the client's names. Only Chat Completions and Responses
/// rewrite, because those are the dialects whose wire pattern luna enforces
/// with a 400. Other dialects keep identity, so same-dialect passthrough is
/// undisturbed.
fn openai_function_names(
    canonical: &oag_proto::CanonicalRequest,
    upstream: Dialect,
) -> FunctionNameMap {
    match upstream {
        Dialect::OpenAIChatCompletions | Dialect::OpenAIResponses => {
            let names = FunctionNameMap::from_request(canonical);
            if names.rewrites() {
                for (original, wire) in names.rewritten() {
                    tracing::debug!(original, wire, "sanitized OpenAI function name");
                }
            }
            names
        }
        _ => FunctionNameMap::identity(),
    }
}

/// Render a collected answer as one body in the client's dialect.
///
/// Every pair goes through the same hub: the events become an Anthropic
/// message (`anthropic::render_from_events`), and that message is converted by
/// the client-dialect converter that already takes one. The converter is
/// therefore chosen by the client's dialect *and* fed a shape it reads — the
/// two halves that used to be decided separately. When the converter was
/// picked by the client's dialect alone and handed the upstream's body as it
/// came, each converter read exactly one upstream shape and eight of the
/// twelve pairs rendered a well-formed, fully-billed, empty answer.
fn render_collected(
    events: &[oag_proto::StreamEvent],
    ingress: Dialect,
    request_id: &str,
    model: &str,
) -> Result<bytes::Bytes> {
    let hub = oag_proto::anthropic::render_from_events(events, request_id, model);
    let out = match ingress {
        Dialect::AnthropicMessages => hub,
        Dialect::OpenAIChatCompletions => oag_proto::openai::render_completion(&hub, request_id),
        Dialect::GeminiGenerateContent => oag_proto::gemini::render_message_response(&hub),
        Dialect::OpenAIResponses => oag_proto::responses::render_response(&hub, request_id),
        // `Dialect` is non-exhaustive. A client dialect with no converter gets
        // an error naming it — never the upstream's body, which is the silent
        // wrong shape this function exists to stop.
        other => {
            return Err(Error::Internal(format!(
                "no non-streaming converter into the {other:?} dialect"
            )));
        }
    };
    Ok(bytes::Bytes::from(out.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn json_response(
    body: &bytes::Bytes,
    events: &[oag_proto::StreamEvent],
    // What the served attempt's own accumulator judged it. Not necessarily
    // what the ledger records: after an escalation the ledger names the gate
    // that triggered the climb, while this is about the answer actually being
    // sent. This used to rebuild a second accumulator over every event just
    // to ask the same question again: a full extra pass on every collected
    // response.
    gate: Option<oag_router::QualityGate>,
    decision: &RoutingDecision,
    request_id: RequestId,
    ingress: Dialect,
    // The dialect the chosen ADAPTER speaks. See `adapter_for`.
    upstream_dialect: Dialect,
    // Whether that adapter streamed the upstream regardless of the client's
    // request, in which case `body` is not a JSON body at all.
    always_streams: bool,
    // Tool names were rewritten on the way to an OpenAI-shaped upstream, so
    // the upstream's own body carries wire names the client cannot dispatch.
    rewrite_tool_names: bool,
) -> Response {
    // Verbatim when the dialects agree and the upstream actually sent a body
    // — the upstream's own bytes are the most faithful answer we can give,
    // and re-serialising can only differ from them. An adapter that streamed
    // has no such bytes: what it has is events, and a body is rendered from
    // those like any translated pair's. Rewritten tool names are the other
    // exception: the events have been restored, the body has not.
    let out = if ingress == upstream_dialect && !always_streams && !rewrite_tool_names {
        body.clone()
    } else {
        match render_collected(
            events,
            ingress,
            &request_id.to_string(),
            decision.model.id.as_str(),
        ) {
            Ok(out) => out,
            Err(e) => return error_response(&e),
        }
    };

    if client_got_nothing(gate, always_streams, ingress, upstream_dialect, body) {
        tracing::error!(
            %request_id,
            ?ingress,
            body_len = body.len(),
            "completion had no content for the client"
        );
    }

    oag_headers(
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json"),
        decision,
        request_id,
    )
    .body(Body::from(out))
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Whether an empty answer reached the client as nothing at all.
///
/// An empty completion passed through verbatim is the provider's own body, and
/// the client sees exactly what the provider said. Rendered -- because the
/// adapter streamed, or the dialects differ, or there was no body to pass
/// through -- it is ours, and an empty one is worth an error.
fn client_got_nothing(
    gate: Option<oag_router::QualityGate>,
    always_streams: bool,
    ingress: Dialect,
    upstream_dialect: Dialect,
    body: &bytes::Bytes,
) -> bool {
    gate == Some(oag_router::QualityGate::EmptyResponse)
        && (always_streams || ingress != upstream_dialect || body.is_empty())
}

/// Routing identity on the way out. `x-oag-tier` is omitted when the model
/// sat on no rung — a named off-ladder pin is not `cheap`.
fn oag_headers(
    builder: axum::http::response::Builder,
    decision: &RoutingDecision,
    request_id: RequestId,
) -> axum::http::response::Builder {
    let builder = builder
        .header("x-oag-model", decision.model.id.as_str())
        .header("x-oag-request-id", request_id.to_string())
        // Which build answered, on the response the caller is already reading.
        //
        // `/health/ready` carries the same identity but lives on the **admin**
        // listener, and a consumer holds an inference key by definition — so
        // the one party that most needs to know which build produced a payload
        // could not ask. Answering it here rather than opening `/health/ready`
        // on the public listener keeps it behind authentication: nothing new
        // is readable without a key, and the answer arrives on the very
        // request whose shape is in question rather than on a second probe
        // that could hit a different replica.
        .header(crate::BUILD_HEADER, crate::build_id());
    match decision.rung_name() {
        Some(tier) => builder.header("x-oag-tier", tier),
        None => builder,
    }
}

/// Try credentials until one works or the budget of attempts runs out.
///
/// Two nested bounds, and they count different things:
///
/// - `same_account_retries` covers a *transient* failure — the credential is
///   fine, the moment was not.
/// - `max_account_switches` covers an *unhealthy* credential, and each switch
///   adds the failed one to an exclusion set so the cascade cannot hand it back.
///
/// Both are bounded because an unbounded retry loop against a provider having a
/// bad afternoon is indistinguishable from an attack on it.
#[allow(clippy::too_many_arguments)]
async fn forward_with_failover(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    decision: &RoutingDecision,
    canonical: &oag_proto::CanonicalRequest,
    session: &SessionKey,
    dispatches: &mut Dispatches,
    lost: &mut Vec<meter::Lost>,
    channel: Option<oag_core::credential::CredentialKind>,
) -> Result<Attempt> {
    let request_id = dispatches.request_id;
    let provider = decision.model.provider;
    let mut excluded: HashSet<AccountId> = HashSet::new();
    let mut last_error = Error::NoCredential { provider };

    let began = std::time::Instant::now();
    let budget = state.config.gateway.failover_budget;

    for switch in 0..=state.config.gateway.max_account_switches {
        // Between attempts only. `max_account_switches` bounds how MANY
        // credentials are tried and nothing bounded how long that took, so a
        // provider slow to answer rather than quick to refuse could hold a
        // caller indefinitely: the upstream client sets no total-response
        // timeout on purpose, and `stream_idle_timeout` starts only once a
        // response exists, so neither covers the gap before first headers.
        //
        // Never interrupts an attempt in flight. Giving up on a slow-but-working
        // stream is the failure this must not cause, so the check sits here,
        // where the only thing it can prevent is starting ANOTHER one.
        if !may_try_another(switch, began.elapsed(), budget) {
            tracing::warn!(
                %request_id, switch, elapsed_ms = began.elapsed().as_millis(),
                error = %last_error,
                "failover budget spent; returning the last upstream error rather than trying another credential"
            );
            metrics::counter!("oag_failover_budget_exhausted_total").increment(1);
            break;
        }
        let lease = match select::lease(
            state,
            auth.route_id,
            auth.principal_id,
            provider,
            session,
            &excluded,
            &request_id.to_string(),
            channel,
        )
        .await
        {
            Ok(l) => l,
            Err(e) => {
                // Running out of candidates is only the real cause on the first
                // pass. After a failover, the interesting error is why the
                // credential we already tried failed — "no credential
                // available" would bury it and send whoever is debugging to
                // look at the pool, which is fine.
                if matches!(last_error, Error::NoCredential { .. }) {
                    last_error = e;
                }
                tracing::debug!(%request_id, switch, error = %last_error, "no further candidates");
                break;
            }
        };
        let account = lease.account.account_id();
        let attempt = dispatches.take();

        match try_credential(state, decision, canonical, &lease, request_id, attempt).await {
            Outcome::Ok(attempt) => {
                if switch > 0 {
                    metrics::counter!("oag_failovers_total").increment(1);
                }
                return Ok(*attempt);
            }
            // Nothing about another credential would help.
            Outcome::Fatal(e) => {
                lease.release().await;
                return Err(e);
            }
            // The credential did its job; the *model* would not take the
            // request. Every other credential reaches the same model, so
            // failing over is pointless — hand it up to escalation instead.
            Outcome::Escalate(e) => {
                lease.release().await;
                return Ok(Attempt::Rejected(e));
            }
            Outcome::Switch(e) => {
                tracing::warn!(%request_id, %account, error = %e, "switching credential");
                last_error = e;
                lease.release().await;
                excluded.insert(account);
            }
            // The credential generated an answer and the stream died before
            // it was whole. The provider invoiced those tokens, and the retry
            // on another credential used to be the only attempt metered, so
            // the ledger saw one generation for every two paid for.
            //
            // Captured here, under its own dispatch number and while the lease
            // that names the account is still in hand; written by the caller,
            // on the detached task, after the served row. Writing it here put
            // it on the request future, where the client hanging up during the
            // retry — the very thing a lost stream makes likely — cancelled it.
            Outcome::Lost(e, accumulator) => {
                tracing::warn!(%request_id, %account, error = %e, "switching credential after a lost answer");
                let ctx = meter_context(
                    auth,
                    decision,
                    &lease,
                    request_id,
                    dispatches.started,
                    attempt,
                );
                lost.push(meter::lose(ctx, &accumulator, &e));
                last_error = e;
                lease.release().await;
                excluded.insert(account);
            }
            // Not an error and not this credential's fault: move on without
            // touching `last_error`, which still names whatever genuinely
            // went wrong before.
            Outcome::Raced => {
                tracing::debug!(%request_id, %account, "half-open probe already taken; trying another credential");
                lease.release().await;
                excluded.insert(account);
            }
        }
    }

    Err(last_error)
}

/// What one credential's attempts came to.
///
/// The success variant is boxed: it carries a whole `reqwest::Response` and a
/// lease, which makes every `Outcome` — including the common error ones — as
/// large as the largest variant otherwise.
enum Outcome {
    Ok(Box<Attempt>),
    /// Try a different credential.
    Switch(Error),
    /// Try a different credential, and meter what this one generated first:
    /// the answer was read far enough to cost something before it was lost.
    Lost(Error, oag_proto::StreamAccumulator),
    /// Another request took this credential's half-open probe between
    /// selection and dispatch. Nothing was sent and nothing failed: try a
    /// different credential, and say nothing about this one.
    ///
    /// Its own variant rather than `Switch(NoCredential)`, which is what it
    /// was: that placeholder became `last_error`, overwriting the genuine
    /// upstream error from the credential tried before — so a request that
    /// failed on a real 5xx and then raced a probe told the caller "no
    /// credential available", and sent whoever read the log to stare at a
    /// healthy pool.
    Raced,
    /// Stop switching credentials and try a better model instead.
    Escalate(Error),
    /// Stop: another credential cannot help.
    Fatal(Error),
}

/// What a rejected attempt says to do next.
///
/// Split out of [`try_credential`] as a pure function because it is the point
/// where a context-length rejection either climbs the ladder or fails the
/// caller, and that decision should be testable without a transport, a lease,
/// and a database behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Same credential, after a backoff.
    Retry,
    /// A bigger model.
    Escalate,
    /// A different credential.
    Switch,
    /// Nothing.
    Fatal,
}

/// Whether another CREDENTIAL may be tried, given how long this request has
/// already spent failing over.
///
/// Pure, and split out for the same reason as [`step_for`]: it is a decision
/// about a caller's wait, and it should be checkable without a transport, a
/// lease and a database behind it.
///
/// The first attempt is always allowed however long the request has taken. A
/// budget that could refuse to try at all would turn a slow moment into a
/// request that never reached an upstream, which is a worse failure than the
/// wait it exists to bound.
const fn may_try_another(
    switch: u8,
    elapsed: std::time::Duration,
    budget: std::time::Duration,
) -> bool {
    switch == 0 || elapsed.as_millis() < budget.as_millis()
}

fn step_for(disposition: Disposition, retries_left: bool) -> Step {
    match disposition {
        Disposition::RetrySameAccount if retries_left => Step::Retry,
        Disposition::EscalateTier => Step::Escalate,
        Disposition::Fatal => Step::Fatal,
        // Rate limited, unhealthy, or out of same-credential retries: all of
        // them are answered by somebody else's credential.
        _ => Step::Switch,
    }
}

/// Say so when a client's vendor fields are about to be dropped.
///
/// A `CanonicalRequest` carries what the four dialects have in common, so a
/// field only one vendor defines has nowhere to land. It rides along in
/// `passthrough` and a renderer for its own dialect puts it back; a renderer
/// for any other dialect leaves it, which is right — we do not invent
/// translations for fields we do not understand.
///
/// What was wrong was that this cost nothing to notice. Losing a Gemini
/// `audioTranscriptionConfig` on the way to another upstream is a 200 with an
/// empty transcript, and nothing anywhere said a field had gone. Now the
/// count is on `/metrics` and the names are one log level away.
///
/// Here rather than in `oag-proto`, which is a pure crate with no recorder,
/// and here rather than at the routing decision, which knows the provider but
/// not the adapter: a Codex seat is `Provider::OpenAI` and speaks Responses.
/// This is the one place per dispatch that holds both the request and the
/// adapter that will actually render it.
fn note_dropped_vendor_fields(
    canonical: &oag_proto::CanonicalRequest,
    upstream: Dialect,
    request_id: RequestId,
) {
    let Some(extra) = &canonical.passthrough else {
        return;
    };
    if extra.dialect == upstream {
        return;
    }
    metrics::counter!(
        "oag_vendor_fields_dropped_total",
        "ingress" => extra.dialect.as_str(),
        "upstream" => upstream.as_str(),
    )
    .increment(1);
    tracing::debug!(
        %request_id,
        ingress = %extra.dialect.as_str(),
        upstream = %upstream.as_str(),
        fields = ?extra.body.as_object().map(|m| m.keys().collect::<Vec<_>>()),
        "vendor fields dropped translating between dialects"
    );
}

/// Try one credential, with bounded same-credential retries.
///
/// The retries here are for failures that are about *the moment* — a timeout, a
/// conflict — rather than about the credential. Anything that says the
/// credential itself is unhealthy returns `Switch` immediately rather than
/// spending the retry budget on it.
#[allow(clippy::too_many_lines)]
async fn try_credential(
    state: &Arc<AppState>,
    decision: &RoutingDecision,
    canonical: &oag_proto::CanonicalRequest,
    lease: &select::Lease,
    request_id: RequestId,
    // `attempt` in the ledger's sense — this request's dispatch ordinal — as
    // distinct from the same-credential retry index the loop below counts.
    ordinal: u8,
) -> Outcome {
    let provider = decision.model.provider;
    let account = lease.account.account_id();

    let adapter = match adapter_for(state, provider, &lease.account) {
        Ok(a) => a,
        Err(e) => return Outcome::Fatal(e),
    };
    let names = openai_function_names(canonical, adapter.dialect());
    note_dropped_vendor_fields(canonical, adapter.dialect(), request_id);
    // Refreshes first if the token is close to expiry. A credential that is
    // merely expiring must not be treated as a credential that is broken.
    let credential = match refresh::ensure_fresh(state, &lease.account).await {
        Ok(c) => c,
        // Broken for everyone, not just this request — but another credential
        // may well work, so switch rather than fail the request outright.
        Err(e) => return Outcome::Switch(e),
    };

    let mut last = Error::NoCredential { provider };

    // Claim the breaker here rather than in selection. Selection reads, so a
    // recovering credential keeps its half-open probe until something is
    // actually about to be sent to it; the guard hands the probe back if we
    // return before reaching the wire.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let Some(mut dispatch) = Dispatch::claim(&state.breakers, account, now) else {
        // Raced: another request took the probe between the filter and here.
        return Outcome::Raced;
    };

    for attempt in 0..=state.config.gateway.same_account_retries {
        let request = match adapter.build(&oag_upstream::UpstreamRequest {
            canonical,
            model: &decision.model,
            credential: &credential,
        }) {
            Ok(r) => r,
            // We built a bad request; a different credential will build the
            // same bad request.
            Err(e) => return Outcome::Fatal(e),
        };

        let transport = match state
            .transports
            .get(&oag_upstream::TransportKey {
                account,
                proxy: lease.account.proxy_url.clone(),
            })
            .await
        {
            Ok(t) => t,
            Err(e) => return Outcome::Switch(e),
        };

        dispatch.sent();
        match transport.execute(request).await {
            Ok(response) if response.status().is_success() => {
                return succeeded(
                    state,
                    provider,
                    lease,
                    response,
                    canonical.stream,
                    ordinal,
                    names,
                )
                .await;
            }

            Ok(response) => {
                let status = response.status().as_u16();
                // Read before the body is consumed: `text()` takes the whole
                // response, headers included.
                let retry_after = upstream_retry_after(response.headers());
                let body = response.text().await.unwrap_or_default();
                let err = Error::Upstream {
                    provider,
                    account,
                    status,
                    body: truncate(&body, 512),
                    retry_after,
                };
                let disposition = err.disposition();
                tracing::warn!(%request_id, status, ?disposition, "upstream rejected");
                state.breakers.record_failure(account);
                apply_disposition(state, account, disposition).await;
                last = err;

                let retries_left = attempt < state.config.gateway.same_account_retries;
                match step_for(disposition, retries_left) {
                    Step::Retry => {
                        // The breaker may have opened on the failure just
                        // recorded. `Dispatch::claim` is taken once, above the
                        // loop, so nothing re-checked it — a credential that has
                        // this moment tripped its breaker still received every
                        // remaining same-credential retry, which is exactly the
                        // traffic a breaker exists to stop, aimed at exactly the
                        // credential it has just decided is unhealthy.
                        let now = time::OffsetDateTime::now_utc().unix_timestamp();
                        if !state.breakers.permits(account, now) {
                            return Outcome::Switch(last);
                        }
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Step::Escalate => return Outcome::Escalate(last),
                    Step::Switch => return Outcome::Switch(last),
                    Step::Fatal => return Outcome::Fatal(last),
                }
            }

            // The provider accepted the connection and then said nothing for
            // the whole headers deadline. That is not a transport blip worth
            // a same-credential retry: it already cost the caller the full
            // deadline, and two more attempts here would cost it twice more
            // before another credential was tried. The error's own
            // disposition says fail over like a 5xx, and until now nothing
            // on this path read it.
            Err(e @ Error::UpstreamTimeout { .. }) => {
                let disposition = e.disposition();
                tracing::warn!(%request_id, %account, error = %e, ?disposition, "upstream silent");
                state.breakers.record_failure(account);
                apply_disposition(state, account, disposition).await;
                return Outcome::Switch(e);
            }

            // Nothing came back at all: connect, TLS, or DNS.
            Err(e) => {
                last = e;
                let retrying = attempt < state.config.gateway.same_account_retries;
                tracing::warn!(%request_id, %account, error = %last, retrying, "upstream unreachable");
                if let Some(d) = transport_failure(&state.breakers, account, retrying) {
                    apply_disposition(state, account, d).await;
                }
                if !retrying {
                    return Outcome::Switch(last);
                }
                // The same re-check the `Step::Retry` arm makes, for the same
                // reason and against the same failure it has just recorded.
                // G7 added it there and not here, so a connect, TLS or DNS
                // failure that tripped the breaker still received every
                // remaining same-credential retry — the traffic a breaker
                // exists to stop, aimed at the credential it has this moment
                // decided is unhealthy. `transport_failure` above is what
                // records that failure, so the breaker's answer here is fresh.
                let now = time::OffsetDateTime::now_utc().unix_timestamp();
                if !state.breakers.permits(account, now) {
                    return Outcome::Switch(last);
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }

    Outcome::Switch(last)
}

/// Hand the upstream stream to the client.
///
/// Takes the lease by value, and resolves everything that can still fail before
/// committing it to the pump task. Both of those failures used to happen with
/// the lease held by somebody else: the adapter lookup `?`d out of the caller
/// and the renderer returned a response from here, and neither released the
/// credential's slot — so the slot sat there for the full `SLOT_TTL` while
/// nothing was in flight. Now they are two `?`s over an owned lease, and the
/// lease's guard hands the slot back on the way out.
#[allow(clippy::too_many_arguments)]
///
/// `triggering_gate` is the gate that *caused* an escalation, if one happened
/// before this attempt streamed. Without it the ledger took the gate from the
/// accumulator alone — the gate this final attempt tripped, which is `None`
/// precisely when escalation worked. So every streamed request that climbed a
/// rung recorded no reason for having climbed, and "which rung is mis-set for
/// this workload" could only be answered from non-streamed traffic. The
/// collected path has threaded it for a while; this one had not.
fn stream_response(
    state: &Arc<AppState>,
    response: reqwest::Response,
    lease: select::Lease,
    auth: &oag_store::AuthContext,
    decision: &RoutingDecision,
    request_id: RequestId,
    started: Instant,
    attempt: u8,
    ingress: Dialect,
    triggering_gate: Option<oag_router::QualityGate>,
    guard: crate::shutdown::InFlightGuard,
    names: FunctionNameMap,
) -> Result<Response> {
    // The adapter this lease actually gets, not the provider's default one:
    // both the framing and the dialect below are facts about that adapter, and
    // asking the provider is what forwarded Responses bytes to a Chat
    // Completions client as a 200 it could not read.
    let adapter = adapter_for(state, decision.model.provider, &lease.account)?;
    let egress = egress_for(
        ingress,
        decision,
        request_id,
        adapter.framing(),
        adapter.dialect(),
        names.rewrites(),
    )?;

    // Bounded: a slow client parks the reader instead of buffering the whole
    // response in memory.
    let (tx, rx) = mpsc::channel::<sse::Chunk>(64);

    let deadlines = sse::Deadlines {
        idle: state.config.gateway.stream_idle_timeout,
        max: state.config.gateway.max_stream_duration,
        client_write: state.config.gateway.client_write_timeout,
        keepalive: state.config.gateway.stream_keepalive_interval,
    };

    // A streamed response is delivered as it arrives, so it is never abandoned
    // and never retried — but a credential may have been switched, and lost,
    // before it, so its dispatch number is not always zero.
    let ctx = meter_context(auth, decision, &lease, request_id, started, attempt);

    let state2 = Arc::clone(state);

    // The pump runs as its own task so it outlives the client's connection.
    // If the client hangs up, this keeps draining and still records what the
    // provider is going to bill us for.
    tokio::spawn(async move {
        // Both the guard and the lease ride along and finish here, when the
        // stream genuinely ends. The guard is what makes the shutdown drain
        // wait for the stream rather than exiting out from under it; the lease
        // is what keeps the credential's slot held for exactly as long as it is
        // really in use.
        let _guard = guard;
        // The slot is NOT released when the client goes: the seat counts what the
        // provider is carrying, and the provider is still carrying this stream
        // until the drain ends. Releasing early made N aborted streams N live
        // upstream connections nobody counted, which is the ghost this seat
        // exists to prevent, from the other side.
        let (gone_tx, _gone_rx) = tokio::sync::watch::channel(false);
        let outcome = sse::pump_with(
            response,
            adapter,
            tx,
            deadlines,
            egress,
            names,
            Some(gone_tx),
        )
        .await;
        if outcome.client_gone {
            tracing::warn!(
                %request_id,
                "client disconnected; slot held while the upstream drains for accounting"
            );
        }
        lease.release().await;
        // `triggering_gate` when we escalated to get here, otherwise whatever
        // this attempt tripped — the same rule the collected path applies, so
        // the ledger names a reason on both.
        meter::record(&state2, &ctx, &outcome, triggering_gate).await;
    });

    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));

    Ok(oag_headers(
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            // Belt and braces for an intermediary we do not control: nginx honours
            // this even when its own buffering config says otherwise.
            .header("x-accel-buffering", "no"),
        decision,
        request_id,
    )
    .body(body)
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

/// Turn a successful response into the attempt the caller returns.
///
/// The body is collected here unless the client asked for a stream: only a
/// streaming client can be handed the upstream body as it arrives.
async fn succeeded(
    state: &Arc<AppState>,
    provider: oag_core::Provider,
    lease: &select::Lease,
    response: reqwest::Response,
    stream: bool,
    attempt: u8,
    names: FunctionNameMap,
) -> Outcome {
    let account = lease.account.account_id();
    // No `touch_account` here any more. It was a Postgres write awaited
    // between "the upstream answered" and "the first byte reaches the client"
    // — on the one latency the user feels — to stamp `last_used_at`, which
    // the ledger write now stamps in the same statement as the row. Recency
    // for the scheduler's tie-break moves from "last dispatched to" to "last
    // completed on", which is a finer definition of used anyway.
    state.breakers.record_success(account);
    metrics::counter!(
        "oag_requests_total",
        "outcome" => "ok",
        "provider" => provider.as_str(),
    )
    .increment(1);

    if stream {
        return Outcome::Ok(Box::new(Attempt::Streaming {
            response,
            lease: lease.clone(),
            attempt,
            names,
        }));
    }
    // The ADAPTER's facts, not the provider's: a Codex seat is
    // `Provider::OpenAI`, speaks Responses, and streams whatever the client
    // asked. Asking the provider parsed a Responses stream as Chat
    // Completions, found nothing it recognised, and collected an empty body —
    // the 200 that reached a client as "no completion in it". And asking the
    // client's `stream` flag whether the upstream streamed read that stream
    // as a JSON body, handed the raw `data:` lines back, and metered zero.
    let adapter = match adapter_for(state, provider, &lease.account) {
        Ok(adapter) => adapter,
        Err(e) => return Outcome::Switch(e),
    };
    let collected = if adapter.always_streams() {
        let idle = state.config.gateway.stream_idle_timeout;
        let max = state.config.gateway.max_stream_duration;
        match sse::collect_stream_with(response, adapter, idle, max, names).await {
            Ok((events, accumulator)) => Ok((bytes::Bytes::new(), events, accumulator)),
            Err(failure) => return collect_failed(failure),
        }
    } else {
        sse::collect_with(
            response,
            adapter.dialect(),
            &names,
            state.config.gateway.max_stream_duration,
        )
        .await
    };
    match collected {
        Ok((body, events, accumulator)) => Outcome::Ok(Box::new(Attempt::Collected {
            body,
            events,
            accumulator,
            lease: lease.clone(),
            attempt,
        })),
        Err(e) => Outcome::Switch(e),
    }
}

/// A collected stream that failed: `Lost` once the provider had generated
/// part of the answer, because that much is invoiced whether or not it was
/// whole, so it goes out with the error rather than being dropped with it.
/// Before any output it is a plain failover.
fn collect_failed(failure: sse::StreamFailure) -> Outcome {
    let (e, accumulator) = *failure;
    if accumulator.usage().output_tokens > 0 {
        Outcome::Lost(e, accumulator)
    } else {
        Outcome::Switch(e)
    }
}

/// How long a credential sits out after the transport itself failed.
///
/// The same thirty seconds a 5xx gets, for the same reason: the credential is
/// probably fine and something between us and the provider is not, so the pause
/// wants to be long enough to stop hammering and short enough to come back.
const TRANSPORT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// Account for a failure that never produced an HTTP status — connect, TLS,
/// DNS, timeout.
///
/// [`Error::disposition`] cannot classify these: all it sees is `Internal`, so
/// it says fatal, and this path used to skip the breaker entirely as a result.
/// The consequence is backwards. A credential behind a dead proxy fails
/// *fastest*, so it always carries the lowest in-flight count, so the
/// least-loaded stage prefers it over every healthy credential — and it never
/// trips, because nothing ever records the failures.
///
/// Returns the disposition to persist, or `None` while same-credential retries
/// remain: a cooldown written between two attempts on the same credential would
/// only be contradicted by the next one.
fn transport_failure(
    breakers: &Breakers,
    account: AccountId,
    retrying: bool,
) -> Option<Disposition> {
    breakers.record_failure(account);
    if retrying {
        None
    } else {
        Some(Disposition::FailoverAccount {
            cooldown: TRANSPORT_COOLDOWN,
        })
    }
}

/// Persist what a failure says about a credential.
async fn apply_disposition(state: &AppState, account: AccountId, d: Disposition) {
    use time::OffsetDateTime;
    match d {
        Disposition::FailoverAccount { cooldown } => {
            let until = OffsetDateTime::now_utc() + cooldown;
            let _ = oag_store::repo::cool_down(&state.db, account, until, "upstream error").await;
        }
        Disposition::RateLimited { retry_after } => {
            let wait = retry_after.unwrap_or(std::time::Duration::from_mins(1));
            let until = OffsetDateTime::now_utc() + wait;
            let _ = oag_store::repo::rate_limit(&state.db, account, until).await;
        }
        _ => {}
    }
}

/// The longest a provider's own `Retry-After` may bench one of our credentials.
///
/// An hour, and the ceiling matters more than the number. A genuinely day-long
/// quota costs at most one refused request per hour past this, which the breaker
/// and the cooldown then absorb — whereas trusting the provider's arithmetic
/// costs a credential nobody can get back.
const MAX_RETRY_AFTER: u64 = 3_600;

/// How long the provider asked us to wait, if it said — and if the answer is
/// usable.
///
/// Only the delta-seconds form. The HTTP-date form is equally legal and no
/// provider we speak to sends it, and a date read wrongly is worse than no hint
/// at all — the caller has a sane default and a misparsed one would override it.
///
/// Anything outside one second to [`MAX_RETRY_AFTER`] is treated as no header at
/// all, and that validation is not decoration: this value is *persisted* as
/// `rate_limited_until`, and `repo::clear_cooldown` deliberately does not clear
/// that column, so a number we accept here cannot be undone from the admin
/// surface at all. Somebody has to reach for psql. Two values in the wild:
///
/// - **`0`**, which Cloudflare sends in front of several providers. Taken
///   literally it means "wait no time", so the credential the provider has just
///   throttled becomes immediately selectable again — strictly worse than the
///   one-minute default it replaced.
/// - **an epoch timestamp**, from confusing `Retry-After` with a reset time. In
///   seconds it benches the credential until the 2080s. In milliseconds it
///   overflows the `OffsetDateTime` addition in [`apply_disposition`] and takes
///   the request down with it.
fn upstream_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    // A negative or fractional value fails this parse and is thereby rejected
    // too, which is the right answer for both.
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (1..=MAX_RETRY_AFTER)
        .contains(&secs)
        .then(|| std::time::Duration::from_secs(secs))
}

/// The longest [`backoff`] waits. The slot heartbeat's lifetime counts it.
pub(crate) const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(3);

/// Exponential backoff, capped at [`MAX_BACKOFF`].
fn backoff(attempt: u8) -> std::time::Duration {
    let ms = 300u64.saturating_mul(1 << u32::from(attempt.min(4)));
    std::time::Duration::from_millis(ms).min(MAX_BACKOFF)
}

/// The inbound key, from any header a client might use.
///
/// Three spellings because three ecosystems: `Authorization` for OpenAI-shaped
/// clients, `x-api-key` for Anthropic's, `x-goog-api-key` for Gemini's. A
/// gateway that accepts only one makes the others' SDKs unusable.
pub(crate) fn extract_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(token) = v.strip_prefix("Bearer ")
    {
        return Some(token);
    }
    headers
        .get("x-api-key")
        .or_else(|| headers.get("x-goog-api-key"))
        .and_then(|v| v.to_str().ok())
}

fn parse_ladder(tiers: &serde_json::Value) -> Result<TierLadder> {
    let rungs: Vec<oag_router::ladder::Rung> = serde_json::from_value(tiers.clone())
        .map_err(|e| Error::Config(format!("route.tiers is not a ladder: {e}")))?;
    TierLadder::new(rungs)
        .ok_or_else(|| Error::Config("route.tiers is empty; a route must have a rung".to_owned()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    // Do not split a UTF-8 character.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The status a client should see for a failure that came from a provider.
///
/// Three upstream statuses mean something else entirely at *our* edge, and
/// forwarding them verbatim tells the client the opposite of what happened:
///
/// - **401** is "your gateway key is wrong". An SDK handed one deletes the key
///   the operator just issued and asks the user to re-authenticate — while the
///   real fault is that our own provider credentials have expired.
/// - **402** is [`Error::BudgetExhausted`], i.e. "you are out of money", which
///   sends the caller to top up an account that is fine.
/// - **403** is "this key may not use this route".
/// - **407** asks the *client* to authenticate to a proxy it does not know
///   exists. Ours is the only proxy in the path, configured per credential, so
///   this is our own configuration failing.
///
/// All four are ours to fix, and by the time one reaches here every credential in
/// the pool has already been tried and failed over. So the honest answer is the
/// one that says the gateway cannot reach a working upstream: 502. Not 503 —
/// that is [`Error::NoCredential`] and [`Error::AtCapacity`], both of which mean
/// "come back shortly" and neither of which is true of a pool whose keys are all
/// dead.
///
/// Everything else keeps the provider's status, because everything else is
/// already about the right party: 400, 413 and 422 are the client's own request,
/// and 5xx already reads as ours.
fn client_status_for(upstream: u16) -> StatusCode {
    match upstream {
        401..=403 | 407 => StatusCode::BAD_GATEWAY,
        other => StatusCode::from_u16(other).unwrap_or(StatusCode::BAD_GATEWAY),
    }
}

/// Map an error to a response.
///
/// The provider's own body is still surfaced, so a client sees more than a bare
/// 502 — but under `error.upstream`, beside our error rather than as it. Its
/// *status* is deliberately not always ours: see [`client_status_for`].
/// Internal errors are surfaced as nothing at all: they can carry connection
/// strings and file paths.
pub(crate) fn error_response(e: &Error) -> Response {
    let (status, kind, message) = match e {
        Error::Unauthenticated => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            e.to_string(),
        ),
        Error::BudgetExhausted { scope } => (
            StatusCode::PAYMENT_REQUIRED,
            "budget_exhausted",
            format!("{scope} is exhausted"),
        ),
        Error::RateLimited { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            e.to_string(),
        ),
        // 503 with its own kind: a client should retry, and a balancer with
        // another replica behind it will land the retry somewhere with room.
        // `at_capacity` means every credential is busy and waiting helps;
        // this means this replica is busy and *another* one helps.
        Error::Overloaded => (StatusCode::SERVICE_UNAVAILABLE, "overloaded", e.to_string()),
        Error::UnsupportedAction { .. } => (StatusCode::NOT_FOUND, "not_found", e.to_string()),
        Error::NoCredential { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no_credential",
            e.to_string(),
        ),
        // The caller's own string is what is wrong, and the message names the
        // qualifiers that would have worked, so they can fix it from the
        // response alone.
        Error::UnknownModelChannel { .. } | Error::ChannelNotOffered { .. } => (
            StatusCode::BAD_REQUEST,
            "invalid_model_qualifier",
            e.to_string(),
        ),
        // Not the generic `no_credential`: an operator reading that goes and
        // looks at a pool with three healthy keys in it. The kind is the whole
        // content of this failure.
        Error::NoCredentialOfKind { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no_credential_of_kind",
            e.to_string(),
        ),
        // Also not the generic `no_credential`: the pool is not empty, it is
        // being held back on purpose, and the message names the line and the
        // three things that move it.
        Error::ReserveHeld { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "quota_reserve_held",
            e.to_string(),
        ),
        Error::AtCapacity { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "at_capacity",
            e.to_string(),
        ),
        Error::NoViableModel(_) => (StatusCode::BAD_REQUEST, "no_viable_model", e.to_string()),
        // The client's own request is the thing that cannot be served, and the
        // message names the field and the dialect — enough to either drop the
        // field or pin the request to a provider that has it.
        Error::UnsupportedField { .. } => {
            (StatusCode::BAD_REQUEST, "unsupported_field", e.to_string())
        }
        // The message is ours; the provider's is nested below rather than
        // substituted for it.
        Error::Upstream { status, .. } => {
            (client_status_for(*status), "upstream_error", e.to_string())
        }
        Error::Serde(_) => (StatusCode::BAD_REQUEST, "invalid_request", e.to_string()),
        Error::StreamIdle(_) => (StatusCode::GATEWAY_TIMEOUT, "stream_idle", e.to_string()),
        // Its own kind, not `stream_idle`: one is a response that went quiet,
        // the other is a response that never began. Both are 504, and a
        // client retries either — but an operator reading the log needs to
        // know which half of the provider is broken.
        Error::UpstreamTimeout { .. } => (
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            e.to_string(),
        ),
        _ => {
            tracing::error!(error = %e, "internal error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error".to_owned(),
            )
        }
    };

    let mut payload = serde_json::json!({
        "type": "error",
        "error": { "type": kind, "message": message }
    });

    // The provider's error as a *value*, next to ours. It used to be our
    // `error.message`, which meant an SDK reading `error.message` found a whole
    // JSON document encoded into a string — so every parser that looked for a
    // message read one and reported gibberish, and anything looking deeper found
    // nothing. The provider's status goes with it, since it is no longer
    // necessarily the status line.
    if let Error::Upstream { status, body, .. } = e {
        payload["error"]["upstream_status"] = serde_json::json!(*status);
        if !body.is_empty() {
            // Truncated bodies stop being valid JSON, so a string is the
            // fallback rather than the failure.
            payload["error"]["upstream"] = serde_json::from_str(body)
                .unwrap_or_else(|_| serde_json::Value::String(body.clone()));
        }
    }

    let mut response = (status, axum::Json(payload)).into_response();

    // A 429 without Retry-After leaves every client to guess, and they guess
    // badly — usually by retrying immediately, which is the one thing the limit
    // exists to prevent. Rounded up, and never zero.
    //
    // A forwarded upstream throttle needs this every bit as much as our own
    // inbound one, and used to get nothing: the header was set for
    // `RateLimited` alone, so the 429s a client is most likely to see arrived
    // bare.
    let wait = match e {
        Error::RateLimited { retry_after } => Some(*retry_after),
        Error::Upstream {
            status: 429,
            retry_after,
            ..
        } => Some(retry_after.unwrap_or(std::time::Duration::from_secs(1))),
        // Shed load clears in the time it takes one admitted request to
        // finish; a second is the honest lower bound, and a balancer retrying
        // elsewhere needs no more of a hint than "not immediately, here".
        Error::Overloaded => Some(std::time::Duration::from_secs(1)),
        _ => None,
    };
    if let Some(wait) = wait {
        let secs = wait.as_secs_f64().ceil().max(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let secs = secs as u64;
        if let Ok(value) = axum::http::HeaderValue::from_str(&secs.to_string()) {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
    }

    response
}

/// Operator-facing `no_viable_model`: which route, and the command that
/// puts a serving model on its ladder.
fn no_viable_message(route: &str, requested: &str, ladder: &TierLadder) -> String {
    let requested = requested.trim();
    let on_ladder: Vec<&str> = ladder
        .rungs()
        .iter()
        .flat_map(|r| r.models.iter().map(oag_router::ModelId::as_str))
        .collect();
    let ladder_providers: Vec<&str> = on_ladder
        .iter()
        .filter_map(|id| id.split_once('/').map(|(p, _)| p))
        .collect();
    let provider = requested
        .split_once('/')
        .map(|(p, _)| p)
        .filter(|p| *p != "oag");

    if let Some(provider) = provider {
        if !ladder_providers.contains(&provider) {
            return format!(
                "route '{route}' has no {provider} models on its ladder; add one with: oag admin route tiers --route {route} cheap={requested}"
            );
        }
        return format!(
            "route '{route}' has no model on its ladder that can serve '{requested}'; set one with: oag admin route tiers --route {route} cheap={requested}"
        );
    }
    let example = on_ladder.first().copied().unwrap_or("provider/model");
    format!(
        "route '{route}' has no model on its ladder that can serve this request; add one with: oag admin route tiers --route {route} cheap={example}"
    )
}

#[cfg(test)]
mod tests;
