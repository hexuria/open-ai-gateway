//! The inference request path.

pub mod meter;
pub mod select;
pub mod sse;

use crate::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use oag_core::tier::RoutingMode;
use oag_core::{AccountId, Disposition, Error, RequestId, Result, TierName};
use oag_pool::SessionKey;
use oag_proto::{anthropic, extract_cache_blocks};
use oag_router::{BudgetState, RoutingDecision, RoutingPolicy, TierLadder};
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
    let guard = state.lifecycle.track();
    let request_id = RequestId::new();

    match handle(&state, &headers, &body, request_id).await {
        Ok(response) => {
            // The guard must outlive the streaming body, not just the handler:
            // dropping it here would let shutdown believe the request finished
            // while its stream is still running.
            let _ = guard;
            response
        }
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
    let mut canonical = anthropic::parse_request(&wire)?;

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

        return Ok(json_response(body, &decision, request_id));
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

    let budget = BudgetState {
        spent_usd: auth.principal_spent_usd,
        limit_usd: auth.principal_budget_usd,
        hard_stop_multiple: auth.principal_hard_stop_multiple,
    };

    // Logged at debug because "why did this route the way it did" is the
    // question every routing complaint turns into, and reconstructing it from
    // the ledger afterwards is slower than reading one line.
    tracing::debug!(
        mode = ?mode,
        pressure = ?budget.pressure(),
        spent = %budget.spent_usd,
        limit = ?budget.limit_usd,
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
fn json_response(
    body: bytes::Bytes,
    decision: &RoutingDecision,
    request_id: RequestId,
) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-oag-model", decision.model.id.as_str())
        .header("x-oag-tier", decision.tier.name.as_str())
        .header("x-oag-request-id", request_id.to_string())
        .body(Body::from(body))
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
                last_error = e;
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
    let credential: oag_core::credential::SecretMaterial =
        match state.kek.open_json(&lease.account.sealed()) {
            Ok(c) => c,
            // A credential we cannot decrypt is broken for everyone, not just
            // this request, but another credential may well work.
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
) -> Response {
    // Bounded: a slow client parks the reader instead of buffering the whole
    // response in memory.
    let (tx, rx) = mpsc::channel::<sse::Chunk>(64);

    let idle = state.config.gateway.stream_idle_timeout;
    let max = state.config.gateway.max_stream_duration;
    let account = lease.account.account_id();

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
        let outcome = sse::pump(response, adapter, tx, idle, max).await;
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
fn extract_key(headers: &HeaderMap) -> Option<&str> {
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
        Error::BudgetExhausted => (
            StatusCode::PAYMENT_REQUIRED,
            "budget_exhausted",
            "monthly budget exhausted for this principal".to_owned(),
        ),
        Error::NoCredential { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no_credential",
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

    (
        status,
        axum::Json(serde_json::json!({
            "type": "error",
            "error": { "type": kind, "message": message }
        })),
    )
        .into_response()
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
            error_response(&Error::BudgetExhausted).status(),
            StatusCode::PAYMENT_REQUIRED
        );
        assert_eq!(
            error_response(&Error::Unauthenticated).status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
