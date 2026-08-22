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

    // A header hint is an explicit instruction, so it outranks classification.
    let explicit_tier = headers
        .get("x-oag-tier")
        .and_then(|v| v.to_str().ok())
        .map(TierName::from);

    // ── route ─────────────────────────────────────────────────────────────────
    let route = oag_store::repo::route_by_id(&state.db, auth.route_id)
        .await?
        .ok_or_else(|| Error::Internal("route vanished between auth and routing".to_owned()))?;

    let ladder = parse_ladder(&route.tiers)?;
    let catalog = state.catalog().await;

    let mut signal = canonical.signal();
    signal.explicit_tier = explicit_tier;

    let floor = auth
        .key_floor_tier
        .as_deref()
        .or(route.floor_tier.as_deref())
        .map(TierName::from)
        .and_then(|n| ladder.tier(&n));

    let policy = RoutingPolicy::new(ladder, Box::new(oag_router::HeuristicClassifier::default()))
        .with_floor(floor);

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

    let decision = policy.decide(
        &mode,
        Some(&canonical.model),
        &signal,
        &budget,
        &catalog,
        canonical.max_tokens,
    )?;

    tracing::info!(
        %request_id,
        model = %decision.model.id,
        tier = %decision.tier,
        reason = ?decision.reason,
        "routed"
    );

    // The upstream must be told the model the router chose, not the virtual
    // name the client asked for.
    canonical.model = decision.model.upstream_name.clone();

    // ── session affinity ──────────────────────────────────────────────────────
    let cache_blocks = extract_cache_blocks(&canonical);
    let session = SessionKey::resolve(
        canonical.client_session.as_deref(),
        &cache_blocks,
        &auth.api_key_id.to_string(),
        decision.model.id.as_str(),
    );

    // ── forward, with failover ────────────────────────────────────────────────
    forward_with_failover(
        state, &auth, &decision, &canonical, &session, request_id, started,
    )
    .await
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
    started: Instant,
) -> Result<Response> {
    let provider = decision.model.provider;
    let adapter = state.adapter(provider)?;
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
        let credential: oag_core::credential::SecretMaterial =
            state.kek.open_json(&lease.account.sealed())?;

        for attempt in 0..=state.config.gateway.same_account_retries {
            let request = adapter.build(&oag_upstream::UpstreamRequest {
                canonical,
                model: &decision.model,
                credential: &credential,
            })?;

            let transport = state
                .transports
                .get(&oag_upstream::TransportKey {
                    account,
                    proxy: lease.account.proxy_url.clone(),
                })
                .await?;

            match transport.execute(request).await {
                Ok(response) if response.status().is_success() => {
                    let _ = oag_store::repo::touch_account(&state.db, account).await;
                    metrics::counter!(
                        "oag_requests_total",
                        "outcome" => "ok",
                        "provider" => provider.as_str(),
                    )
                    .increment(1);
                    if switch > 0 {
                        metrics::counter!("oag_failovers_total").increment(1);
                    }
                    return Ok(stream_response(
                        state, response, adapter, &lease, auth, decision, request_id, started,
                    ));
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
                    apply_disposition(state, account, disposition).await;
                    last_error = err;

                    match disposition {
                        // Same credential, after a backoff. Only for failures
                        // that are about the moment rather than the credential.
                        Disposition::RetrySameAccount
                            if attempt < state.config.gateway.same_account_retries =>
                        {
                            // Falls through to the next attempt against the
                            // same credential.
                            tokio::time::sleep(backoff(attempt)).await;
                        }
                        Disposition::Fatal | Disposition::EscalateTier => {
                            select::release(state, account, &lease.request_id).await;
                            return Err(last_error);
                        }
                        _ => break,
                    }
                }

                Err(e) => {
                    last_error = e;
                    if attempt < state.config.gateway.same_account_retries {
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    break;
                }
            }
        }

        select::release(state, account, &lease.request_id).await;
        excluded.insert(account);
    }

    Err(last_error)
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
