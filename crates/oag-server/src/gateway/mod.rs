//! The inference request path.

pub mod meter;
pub mod refresh;
pub mod select;
pub mod sse;

use crate::AppState;
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
pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    dispatch(state, headers, body, Dialect::AnthropicMessages).await
}

/// `POST /v1/chat/completions` — the OpenAI-shaped surface.
///
/// The same pipeline: only the codec at each end differs, which is the point of
/// having a canonical form in the middle.
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    dispatch(state, headers, body, Dialect::OpenAIChatCompletions).await
}

/// `POST /v1/responses` — OpenAI's newer surface, and the one their current
/// SDKs default to.
pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    dispatch(state, headers, body, Dialect::OpenAIResponses).await
}

/// `POST /v1beta/models/{model}:generateContent` — the Gemini surface.
///
/// The model and the streaming mode are in the path in this dialect, so they
/// are recovered from it rather than from the body.
pub async fn gemini_generate(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(model_action): axum::extract::Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (model, action) = model_action
        .rsplit_once(':')
        .unwrap_or((model_action.as_str(), "generateContent"));
    let stream = action.starts_with("stream");

    // Re-express the path's model and mode as body fields, so the rest of the
    // pipeline sees one shape whatever dialect the client used.
    let mut wire: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(&Error::Serde(e)),
    };
    wire["__oag_model"] = serde_json::Value::String(model.to_owned());
    wire["__oag_stream"] = serde_json::Value::Bool(stream);
    let Ok(patched) = serde_json::to_vec(&wire) else {
        return error_response(&Error::Internal("re-encoding request".to_owned()));
    };

    dispatch(
        state,
        headers,
        axum::body::Bytes::from(patched),
        Dialect::GeminiGenerateContent,
    )
    .await
}

async fn dispatch(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
    ingress: Dialect,
) -> Response {
    // The guard is *moved* down the call chain and, for a streamed response,
    // into the task that pumps it. It must outlive the response body, not just
    // the handler: a handler returns as soon as the headers are decided, and a
    // guard dropped there tells shutdown the request is finished while its
    // stream still has minutes to run — so a rolling deploy severs it.
    let guard = state.lifecycle.track();
    let request_id = RequestId::new();

    match handle(&state, &headers, &body, request_id, ingress, guard).await {
        Ok(response) => response,
        Err(e) => {
            metrics::counter!("oag_requests_total", "outcome" => "error").increment(1);
            error_response(&e)
        }
    }
}

async fn handle(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    body: &[u8],
    request_id: RequestId,
    ingress: Dialect,
    guard: crate::shutdown::InFlightGuard,
) -> Result<Response> {
    let started = Instant::now();

    // ── authenticate ──────────────────────────────────────────────────────────
    let raw_key = extract_key(headers).ok_or(Error::Unauthenticated)?;
    let auth = state
        .auth
        .authenticate(raw_key)
        .await?
        .ok_or(Error::Unauthenticated)?;

    // ── parse ─────────────────────────────────────────────────────────────────
    let wire: serde_json::Value = serde_json::from_slice(body)?;
    let mut canonical = match ingress {
        Dialect::OpenAIChatCompletions => oag_proto::openai::parse_request(&wire)?,
        Dialect::OpenAIResponses => oag_proto::responses::parse_request(&wire)?,
        Dialect::GeminiGenerateContent => {
            let mut c = oag_proto::gemini::parse_request(&wire)?;
            // Recovered from the path by `gemini_generate`.
            wire["__oag_model"]
                .as_str()
                .unwrap_or_default()
                .clone_into(&mut c.model);
            c.stream = wire["__oag_stream"].as_bool().unwrap_or(false);
            c
        }
        _ => anthropic::parse_request(&wire)?,
    };

    let plan = plan_request(state, &auth, &canonical, headers).await?;

    tracing::info!(
        %request_id,
        model = %plan.decision.model.id,
        tier = %plan.decision.tier,
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
        &auth,
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

/// Forward, failing over between credentials, and escalate a rung if the
/// answer comes back unusable.
///
/// Escalation sits *outside* failover, and the nesting is the point: failover
/// asks "is this credential healthy", escalation asks "is this model good
/// enough". Collapsing them would mean a provider outage silently migrated the
/// fleet onto expensive models.
#[allow(clippy::too_many_arguments)]
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
    } = plan;
    //
    // Escalation sits *outside* failover, and the nesting is the point:
    // failover asks "is this credential healthy", escalation asks "is this
    // model good enough". Collapsing them would mean a provider outage silently
    // migrated the fleet onto expensive models.
    let mut escalations = 0u8;
    // The gate that *caused* an escalation, not the last one observed. Recording
    // the final attempt's gate would leave this empty on exactly the rows where
    // it matters, because a successful escalation trips no gate.
    let mut triggering_gate: Option<oag_router::QualityGate> = None;

    loop {
        let attempt =
            forward_with_failover(state, auth, &decision, canonical, session, request_id).await?;

        let (body, accumulator, lease) = match attempt {
            // Streaming: the bytes are already on their way to the client, so
            // there is nothing left to judge. See the note on MAX_ESCALATIONS.
            Attempt::Streaming { response, lease } => {
                return Ok(stream_response(
                    state,
                    response,
                    state.adapter(decision.model.provider)?,
                    &lease,
                    auth,
                    &decision,
                    request_id,
                    started,
                    ingress,
                    guard,
                ));
            }
            Attempt::Collected {
                body,
                accumulator,
                lease,
            } => (body, accumulator, lease),
        };

        let gate = accumulator.quality_gate();

        // Retry one rung up when the answer was unusable and a rung is left.
        //
        // Not under budget pressure, though. A principal near their cap has
        // already been downgraded on purpose; escalating them back up to the
        // most expensive model would undo the very saving the downgrade exists
        // to make. Accepting the worse answer *is* the policy at that point.
        if let Some(gate) = gate
            && oag_router::escalation_allowed(pressure, escalations, MAX_ESCALATIONS)
            && let Some(next) = policy.escalate(
                &decision.tier,
                gate,
                &signal,
                &catalog,
                canonical.max_tokens,
            )
        {
            tracing::info!(
                %request_id, from = %decision.tier, to = %next.tier, ?gate,
                "escalating: the cheaper model produced an unusable answer"
            );
            metrics::counter!(
                "oag_escalations_total",
                "from" => decision.tier.name.to_string(),
                "gate" => format!("{gate:?}"),
            )
            .increment(1);

            select::release(state, lease.account.account_id(), &lease.request_id).await;
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

        // Either it was fine, or nothing better exists. Record the gate either
        // way: a gate we could not act on is exactly the signal that a rung is
        // mis-set for this workload.
        let ctx = meter::Context {
            request_id,
            auth: auth.clone(),
            decision: decision.clone(),
            account: lease.account.account_id(),
            started,
        };
        select::release(state, lease.account.account_id(), &lease.request_id).await;
        // `triggering_gate` when we escalated, otherwise whatever this attempt
        // tripped — so the ledger always names the reason, never nothing.
        meter::record_collected(state, &ctx, &accumulator, triggering_gate.or(gate)).await;

        return Ok(json_response(&body, &decision, request_id, ingress));
    }
}

/// Everything routing decided, before a single byte goes upstream.
struct Plan {
    policy: RoutingPolicy,
    decision: RoutingDecision,
    signal: oag_router::RequestSignal,
    catalog: Arc<oag_router::Catalog>,
    pressure: oag_router::BudgetPressure,
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
) -> Result<Plan> {
    let route = oag_store::repo::route_by_id(&state.db, auth.route_id)
        .await?
        .ok_or_else(|| Error::Internal("route vanished between auth and routing".to_owned()))?;

    // Throttle before doing any of the expensive work below — classification,
    // catalog lookup, credential selection. A request that is going to be
    // refused should be refused cheaply.
    if let Some(rpm) = route.rpm_limit
        && let Ok(rpm) = u32::try_from(rpm)
        && let Some(retry_after) = state.cache.take_rate_token(route.id, rpm).await?
    {
        return Err(Error::RateLimited { retry_after });
    }

    let ladder = parse_ladder(&route.tiers)?;
    let catalog = state.catalog().await;

    let mut signal = canonical.signal();
    // A header hint is an explicit instruction, so it outranks classification.
    signal.explicit_tier = headers
        .get("x-oag-tier")
        .and_then(|v| v.to_str().ok())
        .map(TierName::from);

    // A key's floor beats the route's: it is the narrower grant, and the point
    // of pinning one key to `frontier` is that it applies to that key alone.
    let floor = auth
        .key_floor_tier
        .as_deref()
        .or(route.floor_tier.as_deref())
        .map(TierName::from)
        .and_then(|n| ladder.tier(&n));

    let policy = RoutingPolicy::new(ladder, Box::new(oag_router::HeuristicClassifier::default()))
        .with_floor(floor);

    // `oag/` names are virtual and always managed; otherwise the route decides.
    let mode = if canonical.model.starts_with("oag/") || route.default_mode == "managed" {
        RoutingMode::Managed
    } else {
        RoutingMode::Passthrough
    };

    let budget = Budgets {
        // A per-key quota is a wall at the number written on it: it still
        // degrades through the constrained band first, but it does not get the
        // principal's overshoot grace. An operator who writes `quota_usd = 50`
        // means fifty.
        key: BudgetState {
            spent_usd: auth.spent_usd,
            limit_usd: auth.quota_usd,
            hard_stop_multiple: rust_decimal::Decimal::ONE,
        },
        // A route budget is the team-level cap: it bounds everyone sharing the
        // route, regardless of whose key they used. Like the key quota it is a
        // wall at its number rather than inheriting the principal's grace.
        route: BudgetState {
            spent_usd: route.spent_usd,
            limit_usd: route.monthly_budget_usd,
            hard_stop_multiple: rust_decimal::Decimal::ONE,
        },
        principal: BudgetState {
            spent_usd: auth.principal_spent_usd,
            limit_usd: auth.principal_budget_usd,
            hard_stop_multiple: auth.principal_hard_stop_multiple,
        },
    };

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

    let decision = policy.decide(
        &mode,
        Some(&canonical.model),
        &signal,
        &budget,
        &catalog,
        canonical.max_tokens,
    )?;

    Ok(Plan {
        policy,
        decision,
        signal,
        catalog,
        pressure: budget.pressure(),
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
        body: bytes::Bytes,
        accumulator: oag_proto::StreamAccumulator,
        lease: select::Lease,
    },
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
) -> Result<sse::Egress> {
    let upstream = decision.model.provider.native_dialect();

    // Matching dialects are not sufficient: passthrough forwards the upstream's
    // *bytes*, so it also requires that those bytes are already SSE. Bedrock's
    // dialect is Anthropic and its framing is binary — passing that through
    // would hand a client expecting `data:` lines a length-prefixed envelope.
    if ingress == upstream && framing == oag_upstream::Framing::Sse {
        return Ok(sse::Egress::Passthrough);
    }
    let model = decision.model.id.as_str().to_owned();
    let request_id = request_id.to_string();
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

fn json_response(
    body: &bytes::Bytes,
    decision: &RoutingDecision,
    request_id: RequestId,
    ingress: Dialect,
) -> Response {
    let upstream_dialect = decision.model.provider.native_dialect();

    // Framing does not come into it here: a non-streamed response is a single
    // JSON body whatever the provider streams, so dialect alone decides.
    //
    // Verbatim when the dialects agree — the upstream's own bytes are the most
    // faithful answer we can give, and re-serialising can only differ from it.
    let out = if ingress == upstream_dialect {
        body.clone()
    } else {
        let id = request_id.to_string();
        serde_json::from_slice::<serde_json::Value>(body).map_or_else(
            |_| body.clone(),
            |v| match ingress {
                Dialect::OpenAIChatCompletions => {
                    bytes::Bytes::from(oag_proto::openai::render_completion(&v, &id).to_string())
                }
                Dialect::AnthropicMessages => bytes::Bytes::from(
                    oag_proto::anthropic::render_message_response(&v, &id).to_string(),
                ),
                Dialect::GeminiGenerateContent => {
                    bytes::Bytes::from(oag_proto::gemini::render_message_response(&v).to_string())
                }
                Dialect::OpenAIResponses => {
                    bytes::Bytes::from(oag_proto::responses::render_response(&v, &id).to_string())
                }
                // No converter for this dialect. Hand back the upstream's own
                // body rather than something half-translated — but say so:
                // a silent wildcard here is exactly how a missing arm went
                // unnoticed once already, returning the wrong shape with
                // nothing to show for it.
                other => {
                    tracing::warn!(
                        ingress = ?other,
                        upstream = ?upstream_dialect,
                        "no non-streaming converter for this dialect pair; \
                         returning the upstream body unchanged"
                    );
                    body.clone()
                }
            },
        )
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-oag-model", decision.model.id.as_str())
        .header("x-oag-tier", decision.tier.name.as_str())
        .header("x-oag-request-id", request_id.to_string())
        .body(Body::from(out))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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
) -> Result<Attempt> {
    let provider = decision.model.provider;
    let mut excluded: HashSet<AccountId> = HashSet::new();
    let mut last_error = Error::NoCredential { provider };

    for switch in 0..=state.config.gateway.max_account_switches {
        let lease = match select::lease(
            state,
            auth.route_id,
            auth.principal_id,
            provider,
            session,
            &excluded,
            &request_id.to_string(),
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
                select::release(state, account, &lease.request_id).await;
                return Err(e);
            }
            Outcome::Switch(e) => {
                tracing::warn!(%request_id, %account, error = %e, "switching credential");
                last_error = e;
                select::release(state, account, &lease.request_id).await;
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
    /// Stop: another credential cannot help.
    Fatal(Error),
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

    let adapter = match state.adapter(provider) {
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

        match transport.execute(request).await {
            Ok(response) if response.status().is_success() => {
                let _ = oag_store::repo::touch_account(&state.db, account).await;
                state.breakers.record_success(account);
                metrics::counter!(
                    "oag_requests_total",
                    "outcome" => "ok",
                    "provider" => provider.as_str(),
                )
                .increment(1);

                return if canonical.stream {
                    Outcome::Ok(Box::new(Attempt::Streaming {
                        response,
                        lease: lease.clone(),
                    }))
                } else {
                    match sse::collect(response).await {
                        Ok((body, accumulator)) => Outcome::Ok(Box::new(Attempt::Collected {
                            body,
                            accumulator,
                            lease: lease.clone(),
                        })),
                        Err(e) => Outcome::Switch(e),
                    }
                };
            }

            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                let err = Error::Upstream {
                    provider,
                    account,
                    status,
                    body: truncate(&body, 512),
                };
                let disposition = err.disposition();
                tracing::warn!(%request_id, status, ?disposition, "upstream rejected");
                state.breakers.record_failure(account);
                apply_disposition(state, account, disposition).await;
                last = err;

                match disposition {
                    Disposition::RetrySameAccount
                        if attempt < state.config.gateway.same_account_retries =>
                    {
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Disposition::Fatal | Disposition::EscalateTier => return Outcome::Fatal(last),
                    _ => return Outcome::Switch(last),
                }
            }

            Err(e) => {
                last = e;
                if attempt < state.config.gateway.same_account_retries {
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
#[allow(clippy::too_many_arguments)]
fn stream_response(
    state: &Arc<AppState>,
    response: reqwest::Response,
    adapter: Arc<dyn oag_upstream::ProviderAdapter>,
    lease: &select::Lease,
    auth: &oag_store::AuthContext,
    decision: &RoutingDecision,
    request_id: RequestId,
    started: Instant,
    ingress: Dialect,
    guard: crate::shutdown::InFlightGuard,
) -> Response {
    // Bounded: a slow client parks the reader instead of buffering the whole
    // response in memory.
    let (tx, rx) = mpsc::channel::<sse::Chunk>(64);

    let idle = state.config.gateway.stream_idle_timeout;
    let max = state.config.gateway.max_stream_duration;
    let account = lease.account.account_id();

    let egress = match egress_for(ingress, decision, request_id, adapter.framing()) {
        Ok(e) => e,
        Err(e) => return error_response(&e),
    };

    let ctx = meter::Context {
        request_id,
        auth: auth.clone(),
        decision: decision.clone(),
        account,
        started,
    };

    let state2 = Arc::clone(state);
    let lease_id = lease.request_id.clone();

    // The pump runs as its own task so it outlives the client's connection.
    // If the client hangs up, this keeps draining and still records what the
    // provider is going to bill us for.
    tokio::spawn(async move {
        // The guard rides along and is dropped here, when the stream is
        // genuinely finished — which is what makes the shutdown drain wait for
        // it rather than exiting out from under it.
        let _guard = guard;
        let outcome = sse::pump(response, adapter, tx, idle, max, egress).await;
        select::release(&state2, account, &lease_id).await;
        meter::record(&state2, &ctx, &outcome).await;
    });

    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        // Belt and braces for an intermediary we do not control: nginx honours
        // this even when its own buffering config says otherwise.
        .header("x-accel-buffering", "no")
        .header("x-oag-model", decision.model.id.as_str())
        .header("x-oag-tier", decision.tier.name.as_str())
        .header("x-oag-request-id", request_id.to_string())
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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

/// Map an error to a response.
///
/// Upstream bodies are passed through so a client sees the provider's own
/// message rather than a generic 502 it cannot act on. Internal errors are not:
/// they can carry connection strings and file paths.
fn error_response(e: &Error) -> Response {
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
        Error::NoCredential { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no_credential",
            e.to_string(),
        ),
        Error::AtCapacity { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "at_capacity",
            e.to_string(),
        ),
        Error::NoViableModel => (StatusCode::BAD_REQUEST, "no_viable_model", e.to_string()),
        Error::Upstream { status, body, .. } => (
            StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY),
            "upstream_error",
            body.clone(),
        ),
        Error::Serde(_) => (StatusCode::BAD_REQUEST, "invalid_request", e.to_string()),
        Error::StreamIdle(_) => (StatusCode::GATEWAY_TIMEOUT, "stream_idle", e.to_string()),
        _ => {
            tracing::error!(error = %e, "internal error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error".to_owned(),
            )
        }
    };

    let mut response = (
        status,
        axum::Json(serde_json::json!({
            "type": "error",
            "error": { "type": kind, "message": message }
        })),
    )
        .into_response();

    // A 429 without Retry-After leaves every client to guess, and they guess
    // badly — usually by retrying immediately, which is the one thing the
    // limit exists to prevent. Rounded up, and never zero.
    if let Error::RateLimited { retry_after } = e {
        let secs = retry_after.as_secs_f64().ceil().max(1.0);
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

#[cfg(test)]
mod tests {
    use super::*;

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
            },
            tier: oag_core::Tier::new("cheap", 0),
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
        )
        .expect("supported");
        assert!(matches!(e, sse::Egress::Passthrough));
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
        )
        .expect("supported");
        assert!(matches!(e, sse::Egress::ChatCompletions { .. }));
    }

    #[test]
    fn an_empty_ladder_is_rejected_rather_than_serving_nothing() {
        assert!(parse_ladder(&serde_json::json!([])).is_err());
        assert!(parse_ladder(&serde_json::json!("not a ladder")).is_err());
        assert!(
            parse_ladder(&serde_json::json!([{"name": "cheap", "models": ["kimi/k2"]}])).is_ok()
        );
    }

    #[test]
    fn internal_errors_do_not_leak_their_message() {
        // They can carry connection strings and file paths.
        let r = error_response(&Error::Internal("postgres://user:pw@host/db".to_owned()));
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
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
}
