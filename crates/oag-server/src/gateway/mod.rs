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
use oag_proto::{anthropic, extract_cache_blocks};
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
    // The attempt a gate condemned, waiting to be metered. Held rather than
    // written on the spot: the served row has to reach the ledger first, because
    // until the primary key contracts onto `(request_id, attempt)` only the
    // first row for a request survives.
    let mut abandoned: Option<meter::Abandoned> = None;

    loop {
        let attempt = match forward_with_failover(
            state, auth, &decision, canonical, session, request_id, channel,
        )
        .await
        {
            Ok(attempt) => attempt,
            // The retry died, so there is no served row to come — but the
            // attempt we abandoned to make it was still generated and
            // invoiced. Failing the request does not make that spend go
            // away, and this is the last chance to record it.
            Err(e) => {
                if let Some(abandoned) = &abandoned {
                    meter::record_abandoned(state, abandoned).await;
                }
                return Err(e);
            }
        };

        // An answer to judge, or a refusal to escalate on. Both ask the same
        // question — is a rung up worth trying — so they share the one loop
        // below rather than growing a second escalation path.
        let answer = match attempt {
            // Streaming: the bytes are already on their way to the client, so
            // there is nothing left to judge. See the note on MAX_ESCALATIONS.
            Attempt::Streaming { response, lease } => {
                return stream_response(
                    state, response, lease, auth, &decision, request_id, started, ingress, guard,
                );
            }
            Attempt::Rejected(e) => Err(e),
            Attempt::Collected {
                body,
                events,
                accumulator,
                lease,
            } => Ok((body, events, accumulator, lease)),
        };

        let gate = match &answer {
            // The model would not take the request at all. Nothing was billed
            // and nothing reached the client, so this is the one escalation a
            // streaming request can also take.
            Err(_) => Some(oag_router::QualityGate::ContextOverflow),
            Ok((_, _, accumulator, _)) => accumulator.quality_gate(),
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
            // tokens — capture the abandoned attempt now and write it after
            // the served row, so the surviving primary key keeps the answer
            // the client actually got.
            //
            // Released here rather than left to the drop, and awaited: the
            // rung above may pick this same credential, and a release still
            // in flight would look like a credential with no room.
            if let Ok((_, _, accumulator, lease)) = &answer {
                abandoned = Some(meter::abandon(
                    meter_context(auth, &decision, lease, request_id, started, escalations),
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

        if gate.is_some() && pressure != oag_router::BudgetPressure::Normal {
            tracing::info!(
                %request_id, ?gate,
                "not escalating: this principal is near their budget, so a worse \
                 answer is the intended outcome"
            );
            metrics::counter!("oag_escalations_suppressed_total").increment(1);
        }

        // No rung left to try. A refusal is now the caller's error — the same
        // one they used to get before the first attempt was allowed to climb.
        let (body, events, accumulator, lease) = answer?;

        // Either it was fine, or nothing better exists. Record the gate either
        // way: a gate we could not act on is exactly the signal that a rung is
        // mis-set for this workload.
        let ctx = meter_context(auth, &decision, &lease, request_id, started, escalations);
        // Read while the lease is still here: both are facts about the adapter
        // this account got, and `release` below takes the account with it.
        // Falling back to the provider's dialect once it is gone is exactly the
        // bug this call site had.
        let (upstream_dialect, always_streams) =
            adapter_for(state, decision.model.provider, &lease.account).map_or_else(
                |_| (decision.model.provider.native_dialect(), false),
                |a| (a.dialect(), a.always_streams()),
            );
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
            // Second, and only ever second: this is the row the surviving
            // primary key drops, and the served one above is the row that must
            // not be dropped.
            if let Some(abandoned) = &abandoned {
                meter::record_abandoned(&state2, abandoned).await;
            }
        });

        return Ok(json_response(
            &body,
            &events,
            &decision,
            request_id,
            ingress,
            upstream_dialect,
            always_streams,
        ));
    }
}

/// What the ledger needs about one forwarding attempt.
///
/// Built in one place because an attempt is metered from three: the streamed
/// path, the collected path, and the attempt a quality gate abandons. A second
/// copy of this literal is how one of them ends up attributing spend to the
/// wrong account.
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
    let requested_tier = headers
        .get("x-oag-tier")
        .and_then(|v| v.to_str().ok())
        .map(TierName::from)
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
) -> Result<sse::Egress> {
    let model = decision.model.id.as_str().to_owned();
    let request_id = request_id.to_string();

    // Matching dialects are not sufficient: passthrough forwards the upstream's
    // *bytes*, so it also requires that those bytes are already SSE. Bedrock's
    // dialect is Anthropic and its framing is binary — passing that through
    // would hand a client expecting `data:` lines a length-prefixed envelope.
    if ingress == upstream && framing == oag_upstream::Framing::Sse {
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
    state: &Arc<AppState>,
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

fn json_response(
    body: &bytes::Bytes,
    events: &[oag_proto::StreamEvent],
    decision: &RoutingDecision,
    request_id: RequestId,
    ingress: Dialect,
    // The dialect the chosen ADAPTER speaks. See `adapter_for`.
    upstream_dialect: Dialect,
    // Whether that adapter streamed the upstream regardless of the client's
    // request, in which case `body` is not a JSON body at all.
    always_streams: bool,
) -> Response {
    // Verbatim when the dialects agree and the upstream actually sent a body
    // — the upstream's own bytes are the most faithful answer we can give,
    // and re-serialising can only differ from them. An adapter that streamed
    // has no such bytes: what it has is events, and a body is rendered from
    // those like any translated pair's.
    let out = if ingress == upstream_dialect && !always_streams {
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

/// Routing identity on the way out. `x-oag-tier` is omitted when the model
/// sat on no rung — a named off-ladder pin is not `cheap`.
fn oag_headers(
    builder: axum::http::response::Builder,
    decision: &RoutingDecision,
    request_id: RequestId,
) -> axum::http::response::Builder {
    let builder = builder
        .header("x-oag-model", decision.model.id.as_str())
        .header("x-oag-request-id", request_id.to_string());
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
    request_id: RequestId,
    channel: Option<oag_core::credential::CredentialKind>,
) -> Result<Attempt> {
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

        match try_credential(state, decision, canonical, &lease, request_id).await {
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

/// Try one credential, with bounded same-credential retries.
///
/// The retries here are for failures that are about *the moment* — a timeout, a
/// conflict — rather than about the credential. Anything that says the
/// credential itself is unhealthy returns `Switch` immediately rather than
/// spending the retry budget on it.
async fn try_credential(
    state: &Arc<AppState>,
    decision: &RoutingDecision,
    canonical: &oag_proto::CanonicalRequest,
    lease: &select::Lease,
    request_id: RequestId,
) -> Outcome {
    let provider = decision.model.provider;
    let account = lease.account.account_id();

    let adapter = match adapter_for(state, provider, &lease.account) {
        Ok(a) => a,
        Err(e) => return Outcome::Fatal(e),
    };
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
                return succeeded(state, provider, lease, response, canonical.stream).await;
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
                    Step::Retry => tokio::time::sleep(backoff(attempt)).await,
                    Step::Escalate => return Outcome::Escalate(last),
                    Step::Switch => return Outcome::Switch(last),
                    Step::Fatal => return Outcome::Fatal(last),
                }
            }

            // Nothing came back at all: connect, TLS, DNS, or a timeout.
            Err(e) => {
                last = e;
                let retrying = attempt < state.config.gateway.same_account_retries;
                tracing::warn!(%request_id, %account, error = %last, retrying, "upstream unreachable");
                if let Some(d) = transport_failure(&state.breakers, account, retrying) {
                    apply_disposition(state, account, d).await;
                }
                if retrying {
                    tokio::time::sleep(backoff(attempt)).await;
                } else {
                    return Outcome::Switch(last);
                }
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
fn stream_response(
    state: &Arc<AppState>,
    response: reqwest::Response,
    lease: select::Lease,
    auth: &oag_store::AuthContext,
    decision: &RoutingDecision,
    request_id: RequestId,
    started: Instant,
    ingress: Dialect,
    guard: crate::shutdown::InFlightGuard,
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
    // and never retried: there is only ever one attempt.
    let ctx = meter_context(auth, decision, &lease, request_id, started, 0);

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
        let outcome = sse::pump(response, adapter, tx, deadlines, egress).await;
        lease.release().await;
        meter::record(&state2, &ctx, &outcome).await;
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
        sse::collect_stream(response, adapter, idle, max)
            .await
            .map(|(events, accumulator)| (bytes::Bytes::new(), events, accumulator))
    } else {
        sse::collect(response, adapter.dialect()).await
    };
    match collected {
        Ok((body, events, accumulator)) => Outcome::Ok(Box::new(Attempt::Collected {
            body,
            events,
            accumulator,
            lease: lease.clone(),
        })),
        Err(e) => Outcome::Switch(e),
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

/// Exponential backoff, capped.
fn backoff(attempt: u8) -> std::time::Duration {
    let ms = 300u64.saturating_mul(1 << u32::from(attempt.min(4)));
    std::time::Duration::from_millis(ms.min(3_000))
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
mod tests {
    use super::*;
    use std::time::Duration;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).expect("name"),
                v.parse().expect("value"),
            );
        }
        h
    }

    #[test]
    fn every_ecosystems_auth_header_is_accepted() {
        // Rejecting any of these makes that ecosystem's SDK unusable.
        assert_eq!(
            extract_key(&headers(&[("authorization", "Bearer oag_live_1")])),
            Some("oag_live_1")
        );
        assert_eq!(
            extract_key(&headers(&[("x-api-key", "oag_live_2")])),
            Some("oag_live_2")
        );
        assert_eq!(
            extract_key(&headers(&[("x-goog-api-key", "oag_live_3")])),
            Some("oag_live_3")
        );
        assert_eq!(extract_key(&HeaderMap::new()), None);
    }

    #[test]
    fn a_non_bearer_authorization_falls_through() {
        assert_eq!(
            extract_key(&headers(&[("authorization", "Basic abc")])),
            None
        );
    }

    #[test]
    fn a_gemini_body_that_is_not_an_object_is_a_client_error() {
        // The router no longer reaches this without a key, so the guard is
        // asserted here rather than through a request. Every one of these
        // used to panic inside `IndexMut`, severing the connection — which on
        // HTTP/2 resets every sibling stream multiplexed onto it. `null` did
        // not panic; `IndexMut` silently turned it into an object.
        for body in ["[]", "123", "\"x\"", "true", "null", "{", ""] {
            let Err(err) = require_object_body(body.as_bytes()) else {
                panic!("a body of {body} is not a request and must not be accepted as one");
            };
            assert!(
                matches!(err, Error::Serde(_)),
                "a body of {body} must map to a 400, got {err}"
            );
        }
    }

    #[test]
    fn a_gemini_object_body_passes_the_guard() {
        // The other half: it must refuse the shape the pipeline cannot read and
        // nothing else. A guard that rejected this would refuse every real
        // request.
        assert!(
            require_object_body(br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#)
                .is_ok()
        );
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert!(backoff(0) < backoff(1));
        assert!(backoff(1) < backoff(2));
        assert!(backoff(9) <= std::time::Duration::from_secs(3));
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let s = "é".repeat(400);
        let out = truncate(&s, 51);
        assert!(out.len() <= 54);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn short_bodies_are_not_truncated() {
        assert_eq!(truncate("brief", 512), "brief");
    }

    #[test]
    fn no_viable_model_names_the_route_and_the_fix() {
        let ladder = TierLadder::new(vec![oag_router::ladder::Rung {
            name: oag_core::TierName::from("cheap"),
            models: vec![oag_router::ModelId::new("anthropic/claude-haiku-4.5")],
        }])
        .expect("ladder");
        let msg = no_viable_message("default", "xai/grok-4.3", &ladder);
        assert!(msg.contains("route 'default'"), "{msg}");
        assert!(msg.contains("no xai models"), "{msg}");
        assert!(
            msg.contains("oag admin route tiers --route default cheap=xai/grok-4.3"),
            "{msg}"
        );
    }

    #[test]
    fn transport_error_records_breaker_failure() {
        // A credential behind a dead proxy never returns a status, so nothing
        // on the HTTP path records anything against it. Left unrecorded it
        // fails fastest, looks idlest, and the least-loaded stage keeps picking
        // it — for ever, because it never trips.
        let breakers = Breakers::new();
        let account = AccountId::new();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();

        for _ in 0..64 {
            transport_failure(&breakers, account, true);
        }
        assert!(
            !breakers.permits(account, now),
            "a run of unreachable attempts must trip the breaker"
        );
        assert_eq!(breakers.open_count(now), 1);
    }

    #[test]
    fn a_transport_failure_cools_the_credential_down_once_retries_are_spent() {
        // Only at the end: a cooldown written between two attempts on the same
        // credential is contradicted by the very next attempt.
        let breakers = Breakers::new();
        let account = AccountId::new();

        assert_eq!(transport_failure(&breakers, account, true), None);
        assert_eq!(
            transport_failure(&breakers, account, false),
            Some(Disposition::FailoverAccount {
                cooldown: TRANSPORT_COOLDOWN
            }),
            "the same failover the HTTP path applies"
        );
    }

    fn upstream(status: u16, body: &str) -> Error {
        Error::Upstream {
            provider: oag_core::Provider::Anthropic,
            account: AccountId::new(),
            status,
            body: body.to_owned(),
            retry_after: None,
        }
    }

    /// What the client actually parses. A status assertion alone would have
    /// missed the double-encoded body this file used to send.
    async fn json_body(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a complete body");
        serde_json::from_slice(&bytes).expect("an error envelope is JSON")
    }

    fn retry_after_of(response: &Response) -> Option<&str> {
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
    }

    #[test]
    fn upstream_413_is_outcome_escalate_not_fatal() {
        // A 413 from a 128k rung used to end the request: `EscalateTier` was
        // mapped to `Fatal` here and never reached `policy.escalate`, so the
        // client got the provider's rejection while the rung that could have
        // held the prompt sat one step up, untried.
        assert_eq!(
            step_for(upstream(413, "").disposition(), false),
            Step::Escalate
        );
        assert_eq!(
            step_for(
                upstream(400, "prompt is too long: 210000 tokens").disposition(),
                false
            ),
            Step::Escalate
        );
    }

    #[test]
    fn a_bad_request_still_fails_the_caller() {
        // The other half of the same decision: escalation costs money, so only
        // a capability rejection buys a rung.
        assert_eq!(
            step_for(upstream(400, "messages: required").disposition(), false),
            Step::Fatal
        );
    }

    #[test]
    fn an_unhealthy_credential_is_switched_rather_than_escalated() {
        // Escalating here would migrate the fleet onto expensive models every
        // time a provider had a bad afternoon.
        assert_eq!(
            step_for(upstream(503, "").disposition(), false),
            Step::Switch
        );
        assert_eq!(
            step_for(upstream(429, "").disposition(), false),
            Step::Switch
        );
        // Transient, and the retry budget decides which of the two it is.
        assert_eq!(step_for(upstream(408, "").disposition(), true), Step::Retry);
        assert_eq!(
            step_for(upstream(408, "").disposition(), false),
            Step::Switch
        );
    }

    fn decision_for(provider: oag_core::Provider) -> RoutingDecision {
        use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
        RoutingDecision {
            model: ModelSpec {
                id: ModelId::new("p/m"),
                provider,
                upstream_name: "m".to_owned(),
                pricing: Pricing {
                    input_per_mtok: rust_decimal::Decimal::ONE,
                    output_per_mtok: rust_decimal::Decimal::ONE,
                    cache_read_per_mtok: None,
                    cache_write_per_mtok: None,
                },
                context_window: 1000,
                max_output_tokens: 100,
                capabilities: Capabilities::default(),
                display_label: None,
            },
            tier: Some(oag_core::Tier::new("cheap", 0)),
            reason: oag_router::SelectionReason::Classified,
            capability_escalated_from: None,
            ceiling_model: None,
        }
    }

    #[test]
    fn a_client_reaches_a_same_dialect_upstream_without_re_serialising() {
        // The bug this pins: OpenAI declared its dialect as Responses while the
        // adapter serving it spoke Chat Completions, so this case never took
        // the passthrough path and round-tripped every frame for nothing.
        let d = decision_for(oag_core::Provider::OpenAI);
        let e = egress_for(
            Dialect::OpenAIChatCompletions,
            &d,
            RequestId::new(),
            oag_upstream::Framing::Sse,
            Dialect::OpenAIChatCompletions,
        )
        .expect("supported");
        assert!(matches!(
            e,
            sse::Egress::Passthrough {
                dialect: Dialect::OpenAIChatCompletions,
                ..
            }
        ));
    }

    #[test]
    fn a_binary_framed_upstream_is_never_passed_through() {
        // Bedrock's dialect *is* Anthropic, so dialect alone would say
        // passthrough — and hand a client expecting `data:` lines a
        // length-prefixed binary envelope.
        let d = decision_for(oag_core::Provider::Bedrock);
        let e = egress_for(
            Dialect::AnthropicMessages,
            &d,
            RequestId::new(),
            oag_upstream::Framing::AwsEventStream,
            Dialect::AnthropicMessages,
        )
        .expect("supported");
        assert!(
            matches!(e, sse::Egress::AnthropicMessages { .. }),
            "must be rendered, not forwarded"
        );
    }

    #[test]
    fn a_cross_dialect_pair_selects_the_client_s_renderer() {
        let d = decision_for(oag_core::Provider::Anthropic);
        let e = egress_for(
            Dialect::OpenAIChatCompletions,
            &d,
            RequestId::new(),
            oag_upstream::Framing::Sse,
            Dialect::AnthropicMessages,
        )
        .expect("supported");
        assert!(matches!(e, sse::Egress::ChatCompletions { .. }));
    }

    #[test]
    fn a_codex_seat_is_rendered_rather_than_forwarded_to_a_chat_client() {
        // THE BUG THIS PINS. A Codex subscription is `Provider::OpenAI`, whose
        // native dialect is Chat Completions, but the adapter serving it speaks
        // Responses. While this function asked the PROVIDER, the two dialects
        // appeared to agree and the Responses bytes were forwarded verbatim —
        // a 200 that a Chat Completions client reads as an empty answer, which
        // is how it reached a person as "the model is broken".
        //
        // The upstream dialect is a parameter now precisely so this case can
        // differ from the provider's.
        let d = decision_for(oag_core::Provider::OpenAI);
        let e = egress_for(
            Dialect::OpenAIChatCompletions,
            &d,
            RequestId::new(),
            oag_upstream::Framing::Sse,
            Dialect::OpenAIResponses,
        )
        .expect("supported");
        assert!(
            matches!(e, sse::Egress::ChatCompletions { .. }),
            "must be rendered into the client's dialect, not forwarded"
        );
    }

    #[test]
    fn the_codex_adapter_says_it_speaks_responses() {
        // The other half: the parameter above is only right if the adapter
        // reports its own dialect rather than inheriting the provider's.
        use oag_upstream::ProviderAdapter;
        let codex = oag_upstream::codex::CodexAdapter::new();
        assert_eq!(codex.provider(), oag_core::Provider::OpenAI);
        assert_eq!(codex.dialect(), Dialect::OpenAIResponses);
        assert_ne!(codex.dialect(), codex.provider().native_dialect());
        // And that it streams regardless of the client: the third adapter
        // fact the response path has to ask for rather than assume.
        assert!(codex.always_streams());
        assert!(!oag_upstream::AnthropicAdapter::default().always_streams());
    }

    #[test]
    fn every_client_dialect_gets_a_body_with_the_answer_in_it() {
        // THE MATRIX. One answer as canonical events — text, a whole tool
        // call, a stop, usage — rendered for each client dialect, then read
        // back by that dialect's own reader. Every body must carry the text
        // and the tool call and non-zero usage. Before the hub, the converter
        // was picked by the client's dialect and handed whatever the upstream
        // sent, and for eight of the twelve pairs that body had a shape the
        // converter did not read: a well-formed, fully-billed, empty 200.
        use oag_proto::{StopReason, StreamEvent};
        let events = [
            StreamEvent::UsageUpdate {
                usage: oag_router::Usage {
                    input_tokens: 1000,
                    cache_read_tokens: 200,
                    ..oag_router::Usage::default()
                },
            },
            StreamEvent::TextDelta {
                text: "Reading it now.".to_owned(),
            },
            StreamEvent::ToolUseStart {
                id: "toolu_1".to_owned(),
                name: "read_file".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "toolu_1".to_owned(),
                partial_json: r#"{"path":"a.rs"}"#.to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "toolu_1".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                usage: oag_router::Usage {
                    output_tokens: 42,
                    ..oag_router::Usage::default()
                },
            },
        ];

        for ingress in [
            Dialect::AnthropicMessages,
            Dialect::OpenAIChatCompletions,
            Dialect::GeminiGenerateContent,
            Dialect::OpenAIResponses,
        ] {
            let bytes = render_collected(&events, ingress, "req1", "oag/auto")
                .unwrap_or_else(|e| panic!("{ingress:?}: {e}"));
            let v: serde_json::Value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{ingress:?}: not JSON: {e}"));

            // Read back with the dialect's own reader — the same one the
            // gateway would use if this body came from an upstream.
            let back = match ingress {
                Dialect::AnthropicMessages => oag_proto::anthropic::parse_response(&v),
                Dialect::OpenAIChatCompletions => oag_proto::openai::parse_response(&v),
                Dialect::GeminiGenerateContent => oag_proto::gemini::parse_response(&v),
                Dialect::OpenAIResponses => oag_proto::responses::parse_response(&v),
                _ => unreachable!(),
            };
            let text: String = back
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "Reading it now.", "{ingress:?} lost the text: {v}");
            assert!(
                back.iter().any(|e| matches!(
                    e, StreamEvent::ToolUseStart { name, .. } if name == "read_file"
                )),
                "{ingress:?} lost the tool call: {v}"
            );
            let mut acc = oag_proto::StreamAccumulator::new();
            for e in &back {
                acc.observe(e);
            }
            assert!(
                acc.usage().output_tokens > 0,
                "{ingress:?} lost the usage: {v}"
            );
            assert_eq!(acc.quality_gate(), None, "{ingress:?}: {v}");
        }
    }

    #[test]
    fn a_client_dialect_with_no_converter_is_an_error_not_the_upstream_body() {
        // `Dialect` is non-exhaustive; nothing constructs an unknown one here,
        // so what this pins is that an *empty* answer still renders as a
        // well-formed body rather than a crash — the crash being the one
        // silence worse than the empty 200.
        let bytes =
            render_collected(&[], Dialect::OpenAIChatCompletions, "r", "m").expect("renders");
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert!(v["choices"].is_array());
    }

    /// A state that dials nothing: `Db::connect` builds a lazy pool and
    /// `Cache::connect` only opens a redis client, so the adapter lookup these
    /// tests are about runs long before any backend would.
    fn state() -> Arc<AppState> {
        let src = r#"
database:
  url: "postgres://oag:oag@127.0.0.1:1/oag"
redis:
  url: "redis://127.0.0.1:1"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
"#;
        let config = oag_core::config::Config::from_yaml(src).expect("test config");
        let db = oag_store::Db::connect(&config.database.url, 1).expect("lazy pool");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
        Arc::new(AppState::new(config, db, cache).expect("state"))
    }

    fn auth_context() -> oag_store::AuthContext {
        oag_store::AuthContext {
            api_key_id: uuid::Uuid::nil(),
            principal_id: uuid::Uuid::nil(),
            route_id: uuid::Uuid::nil(),
            key_floor_tier: None,
            admin: false,
            quota_usd: None,
            principal_budget_usd: None,
            principal_hard_stop_multiple: rust_decimal::Decimal::ONE,
            key_hash: String::new(),
        }
    }

    #[tokio::test]
    async fn streaming_adapter_or_egress_error_releases_slot() {
        // `vertex` is a routable provider with no adapter registered, so the
        // streaming arm fails *after* a credential has been leased — the same
        // shape as a dialect pair with no renderer. Both used to return past
        // every release, stranding the slot for the whole SLOT_TTL; eight of
        // those on one credential and it answers AtCapacity with nothing in
        // flight.
        let state = state();
        let slots = Arc::new(select::testing::CountingSlots::default());

        let result = stream_response(
            &state,
            reqwest::Response::from(http::Response::new("stub")),
            select::testing::lease(&slots),
            &auth_context(),
            &decision_for(oag_core::Provider::Vertex),
            RequestId::new(),
            Instant::now(),
            Dialect::AnthropicMessages,
            state.lifecycle.track(),
        );

        assert!(result.is_err(), "there is no adapter for vertex");
        assert_eq!(slots.settled().await, 1, "and the slot came back");
    }

    #[tokio::test]
    async fn a_live_stream_keeps_its_slot_until_the_pump_is_done() {
        // The other half of it. The lease is moved into the pump task rather
        // than dropped when the handler returns, because a handler returns as
        // soon as the headers are decided — releasing there would hand back a
        // slot that is still streaming, which oversubscribes the credential
        // rather than merely leaking from it.
        let state = state();
        let slots = Arc::new(select::testing::CountingSlots::default());

        // A body that never yields, so the pump is still running when the
        // assertion below looks.
        let body = reqwest::Body::wrap_stream(futures_util::stream::pending::<
            std::result::Result<bytes::Bytes, std::io::Error>,
        >());

        let result = stream_response(
            &state,
            reqwest::Response::from(http::Response::new(body)),
            select::testing::lease(&slots),
            &auth_context(),
            &decision_for(oag_core::Provider::Anthropic),
            RequestId::new(),
            Instant::now(),
            Dialect::AnthropicMessages,
            state.lifecycle.track(),
        );

        assert!(result.is_ok());
        assert_eq!(slots.settled().await, 0, "still in flight");
    }

    #[test]
    fn an_empty_ladder_is_rejected_rather_than_serving_nothing() {
        assert!(parse_ladder(&serde_json::json!([])).is_err());
        assert!(parse_ladder(&serde_json::json!("not a ladder")).is_err());
        assert!(
            parse_ladder(&serde_json::json!([{"name": "cheap", "models": ["kimi/k2"]}])).is_ok()
        );
    }

    #[tokio::test]
    async fn internal_errors_do_not_leak_their_message() {
        // They can carry connection strings and file paths. Asserting on the
        // status alone never checked the thing the test is named for.
        let response = error_response(&Error::Internal("postgres://user:pw@host/db".to_owned()));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = json_body(response).await;
        assert_eq!(body["error"]["type"], "internal_error");
        assert_eq!(body["error"]["message"], "internal error");
        assert!(
            !body.to_string().contains("postgres://"),
            "nothing from the error may reach the client: {body}"
        );
    }

    #[tokio::test]
    async fn upstream_401_maps_to_502() {
        // The collision: 401 is *our* "your gateway key is wrong". A 401 here
        // means the provider credentials are expired, and every one has already
        // been tried — but an SDK reading the status deletes the gateway key the
        // operator just issued and sends the user to re-authenticate against a
        // key that was never the problem.
        let response = error_response(&upstream(
            401,
            r#"{"error":{"type":"authentication_error","message":"OAuth token has expired"}}"#,
        ));
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let body = json_body(response).await;
        assert_eq!(body["error"]["type"], "upstream_error");
        // Still diagnosable: the provider's status is reported, just not as ours.
        assert_eq!(body["error"]["upstream_status"], 401);
        // And its body is a value a parser can walk into, not a string that
        // happens to hold JSON.
        assert_eq!(
            body["error"]["upstream"]["error"]["message"],
            "OAuth token has expired"
        );
        // Above all, not something the client mistakes for its own auth failure.
        assert_ne!(body["error"]["type"], "authentication_error");
    }

    #[test]
    fn our_own_credential_failures_are_never_dressed_as_the_clients() {
        // 402 is BudgetExhausted, 403 is a route this key may not use, and 407
        // asks the client to authenticate to our proxy. Every one of them sends
        // the caller to fix something on their side that is already fine.
        for status in [401u16, 402, 403, 407] {
            let response = error_response(&upstream(status, ""));
            assert_eq!(
                response.status(),
                StatusCode::BAD_GATEWAY,
                "upstream {status} must not become the client's {status}"
            );
        }
    }

    #[test]
    fn a_request_the_client_can_fix_keeps_the_providers_status() {
        // The other half. These are about the bytes the client sent, so the
        // client is the only party who can act on them — and 413 in particular
        // is what the ladder failed to escalate past, which is worth saying
        // plainly rather than collapsing into "bad gateway".
        for status in [400u16, 413, 422] {
            let response = error_response(&upstream(status, ""));
            assert_eq!(response.status().as_u16(), status);
        }
    }

    #[tokio::test]
    async fn upstream_429_forwards_retry_after() {
        // The provider told us how long to wait and we dropped it on the floor:
        // the header was only ever set for our own inbound throttle, so a
        // forwarded 429 reached the client with nothing to back off by.
        let response = error_response(&Error::Upstream {
            provider: oag_core::Provider::Anthropic,
            account: AccountId::new(),
            status: 429,
            body: r#"{"error":{"message":"rate limit"}}"#.to_owned(),
            retry_after: Some(std::time::Duration::from_secs(30)),
        });
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_after_of(&response), Some("30"));
        assert_eq!(
            json_body(response).await["error"]["upstream"]["error"]["message"],
            "rate limit"
        );
    }

    #[test]
    fn an_upstream_429_without_a_hint_still_names_a_wait() {
        // No header from the provider is not licence to omit ours: a 429 with
        // no Retry-After is an invitation to retry immediately.
        assert_eq!(
            retry_after_of(&error_response(&upstream(429, ""))),
            Some("1")
        );
    }

    #[test]
    fn a_providers_retry_after_is_read_from_the_response() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("42"));
        assert_eq!(
            upstream_retry_after(&h),
            Some(std::time::Duration::from_secs(42))
        );

        // The HTTP-date form is legal and unparsed on purpose: the caller's
        // default beats a date read wrongly.
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(upstream_retry_after(&h), None);
        assert_eq!(upstream_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn an_unusable_retry_after_is_treated_as_no_header_at_all() {
        // This number is persisted as `rate_limited_until`, and
        // `repo::clear_cooldown` deliberately leaves that column alone — so
        // anything accepted here is a credential no operator can get back
        // without psql. It used to be accepted verbatim.
        for (value, why) in [
            (
                "0",
                "Cloudflare sends it, and it un-benches the credential the \
                 provider just throttled",
            ),
            (
                "1756000000",
                "an epoch timestamp in seconds holds the credential out until the 2080s",
            ),
            (
                "1756000000000",
                "the same mistake in milliseconds overflowed the clock and panicked",
            ),
            ("-5", "a negative wait is not a wait"),
            (
                "86400",
                "a day is longer than we will hold a credential out on a provider's word",
            ),
            ("3601", "one second past the ceiling is still past it"),
            ("", "no value"),
            ("later", "not a number"),
        ] {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                reqwest::header::RETRY_AFTER,
                reqwest::header::HeaderValue::from_str(value).expect("a valid header value"),
            );
            assert_eq!(
                upstream_retry_after(&h),
                None,
                "Retry-After: {value} — {why}"
            );
        }

        // And the edges of what is accepted, so the bound is pinned from both
        // sides rather than only rejected from one.
        for secs in [1u64, MAX_RETRY_AFTER] {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                reqwest::header::RETRY_AFTER,
                reqwest::header::HeaderValue::from_str(&secs.to_string()).expect("value"),
            );
            assert_eq!(
                upstream_retry_after(&h),
                Some(std::time::Duration::from_secs(secs))
            );
        }
    }

    #[test]
    fn no_provider_header_can_bench_a_credential_past_the_ceiling() {
        // The arithmetic `apply_disposition` performs, without a database
        // behind it. The millisecond case used to panic on this very addition
        // and take the request task with it; the second case landed in 2081,
        // and 0 released the credential immediately.
        let now = time::OffsetDateTime::now_utc();
        let ceiling = now + std::time::Duration::from_secs(MAX_RETRY_AFTER);

        for value in [
            "1756000000000",
            "1756000000",
            "0",
            "-5",
            "99999999999999999999",
            "86400",
            "30",
        ] {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                reqwest::header::RETRY_AFTER,
                reqwest::header::HeaderValue::from_str(value).expect("a valid header value"),
            );

            // Exactly what `apply_disposition` computes for a rate limit, and
            // it panics here rather than returning if the wait is absurd.
            let wait = upstream_retry_after(&h).unwrap_or(std::time::Duration::from_mins(1));
            let until = now + wait;

            assert!(
                until > now,
                "Retry-After: {value} must still hold the credential out for something"
            );
            assert!(
                until <= ceiling,
                "Retry-After: {value} must not outlast the ceiling"
            );
        }
    }

    /// Where the client-facing catalogue lives.
    const ERROR_FIXTURES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/test/api/errors.json"
    );

    /// Render every `Error` variant to the bytes a client actually receives, and
    /// hold the committed fixture to them.
    ///
    /// This is the mechanism behind `deploy/test/api/ERRORS.md`. Two ways it
    /// stays honest, and both matter:
    ///
    /// 1. `every_variant()` is exhaustive over `Error` — it lives in `oag-core`
    ///    where `#[non_exhaustive]` does not apply, and its match has no wildcard
    ///    arm. A new variant stops THAT crate compiling.
    /// 2. This asserts the rendered shapes equal the committed file. A change to
    ///    a status code or a `type` string fails here rather than in somebody's
    ///    client, months later, as an unhandled case.
    ///
    /// Regenerate deliberately after an intended change:
    ///
    /// ```text
    /// UPDATE_ERROR_FIXTURES=1 cargo test -p oag-server error_shape
    /// ```
    #[tokio::test]
    async fn every_error_shape_matches_the_committed_catalogue() {
        let mut rendered = Vec::new();
        for error in oag_core::error::every_variant() {
            // The Rust variant that produced this shape. Worth recording because
            // the mapping is many-to-one and deliberately so: three different
            // internal failures all render as `internal_error` with the same
            // redacted message, and without this the fixture shows three
            // identical entries and no way to tell why.
            let variant = format!("{error:?}");
            let variant = variant
                .split(['(', ' ', '{'])
                .next()
                .unwrap_or("?")
                .to_owned();
            let response = error_response(&error);
            let status = response.status().as_u16();
            let retry_after = retry_after_of(&response).map(str::to_owned);
            let body = json_body(response).await;
            let kind = body["error"]["type"]
                .as_str()
                .expect("every envelope names its kind")
                .to_owned();

            // The envelope shape itself, asserted for every variant rather than
            // trusted: a client branches on these two fields and nothing else.
            assert_eq!(body["type"], "error", "{kind} lost its outer type");
            assert!(
                body["error"]["message"].is_string(),
                "{kind} has no message"
            );

            let mut entry = serde_json::json!({
                "type": kind,
                "status": status,
                "variant": variant,
                "body": body,
            });
            if let Some(wait) = retry_after {
                entry["retry_after_header"] = serde_json::json!(wait);
            }
            rendered.push(entry);
        }

        // Sorted so the file is stable and a diff shows a real change rather
        // than a reordering.
        rendered.sort_by(|a, b| {
            (a["type"].as_str(), a["variant"].as_str())
                .cmp(&(b["type"].as_str(), b["variant"].as_str()))
        });
        let actual = format!(
            "{}\n",
            serde_json::to_string_pretty(&rendered).expect("serialisable")
        );

        if std::env::var_os("UPDATE_ERROR_FIXTURES").is_some() {
            std::fs::write(ERROR_FIXTURES, &actual).expect("write the catalogue");
            return;
        }

        let expected = std::fs::read_to_string(ERROR_FIXTURES).unwrap_or_default();
        assert_eq!(
            actual.trim(),
            expected.trim(),
            "the error shapes a client receives have changed.\n\
             If that was intended, regenerate the catalogue and update ERRORS.md:\n\
             \n    UPDATE_ERROR_FIXTURES=1 cargo test -p oag-server error_shapes\n"
        );
    }

    /// The catalogue is only useful if the kinds are distinct. Two variants
    /// rendering to one `type` would leave a client unable to tell them apart —
    /// and the two would have to be handled differently, or they would not be
    /// two variants.
    #[tokio::test]
    async fn no_two_errors_are_indistinguishable_to_a_client() {
        let mut seen: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
        for error in oag_core::error::every_variant() {
            let response = error_response(&error);
            let status = response.status().as_u16();
            let body = json_body(response).await;
            let kind = body["error"]["type"].as_str().expect("a kind").to_owned();
            // Sharing a kind is allowed only when the status also matches: those
            // are deliberate groupings, like the two model-qualifier failures.
            if let Some(previous) = seen.insert(kind.clone(), status) {
                assert_eq!(
                    previous, status,
                    "`{kind}` is returned with two different statuses, so a \
                     client branching on it cannot know which it has"
                );
            }
        }
    }

    #[test]
    fn the_first_attempt_is_never_refused_by_the_failover_budget() {
        // A request that arrives during a slow moment must still reach an
        // upstream. Refusing before trying at all would turn "this took a while"
        // into "this never happened", which is worse than the wait.
        assert!(may_try_another(
            0,
            Duration::from_mins(10),
            Duration::from_mins(2)
        ));
    }

    #[test]
    fn another_credential_is_tried_while_the_budget_holds() {
        assert!(may_try_another(
            1,
            Duration::from_secs(30),
            Duration::from_mins(2)
        ));
    }

    #[test]
    fn a_spent_budget_stops_the_next_credential_not_the_current_one() {
        // The point of the whole mechanism: once the time is gone, stop STARTING
        // work. Anything already in flight runs to completion elsewhere — this
        // decision is only ever consulted between attempts.
        assert!(!may_try_another(
            1,
            Duration::from_mins(2),
            Duration::from_mins(2)
        ));
        assert!(!may_try_another(
            3,
            Duration::from_secs(500),
            Duration::from_mins(2)
        ));
    }

    #[test]
    fn budget_exhaustion_is_distinguishable_from_an_auth_failure() {
        // The client needs to tell "you are out of money" from "your key is
        // wrong": one is fixed by waiting, the other never is.
        assert_eq!(
            error_response(&Error::BudgetExhausted {
                scope: oag_core::BudgetScope::Principal,
            })
            .status(),
            StatusCode::PAYMENT_REQUIRED
        );
        assert_eq!(
            error_response(&Error::Unauthenticated).status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn a_field_the_target_dialect_cannot_express_is_the_client_s_error() {
        // Not a 500: nothing is broken here, the request simply asked for
        // something the model it routed to has no way to do. And not a silent
        // success either — the whole point is that the caller is told.
        let r = error_response(&Error::UnsupportedField {
            field: "response_format",
            dialect: Dialect::AnthropicMessages,
        });
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn throttling_answers_429_and_says_how_long_to_wait() {
        let response = error_response(&Error::RateLimited {
            retry_after: std::time::Duration::from_millis(1500),
        });
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        // Rounded up: a client told to wait 1s when a token lands at 1.5s
        // simply comes back too early and is refused again.
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("2")
        );
    }

    #[test]
    fn a_sub_second_wait_still_asks_for_at_least_one_second() {
        let response = error_response(&Error::RateLimited {
            retry_after: std::time::Duration::from_millis(1),
        });
        // Retry-After: 0 is an invitation to hot-loop.
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    #[test]
    fn a_virtual_model_name_pins_its_rung_and_auto_pins_nothing() {
        // `oag/cheap` and `oag/frontier` used to be indistinguishable from
        // `oag/auto`: the prefix forced managed mode and the rung after it was
        // never read, so every virtual name meant "classify for me".
        assert_eq!(virtual_tier("oag/cheap"), Some(TierName::from("cheap")));
        assert_eq!(
            virtual_tier("oag/frontier"),
            Some(TierName::from("frontier"))
        );
        assert_eq!(virtual_tier("oag/auto"), None, "auto is the unpinned one");
        assert_eq!(virtual_tier("claude-opus-5"), None);
        assert_eq!(virtual_tier("anthropic/claude-opus-5"), None);
    }
}
