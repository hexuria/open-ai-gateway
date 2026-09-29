//! System One: Jev's question-answering API, served beside the chat surfaces.
//!
//! `POST /jev/v1/systemone` takes a `state` and named `questions` and answers
//! each with a typed answer and its confidence. It is not a conversation, and
//! none of the chat pipeline applies to it: no canonical form, no ladder, no
//! escalation, no stream. Only a Jev upstream can answer it, so there is no
//! fallback either — a route without a Jev credential refuses, and says so.
//!
//! What it does share with the chat path is everything about the credential.
//! The caller's key, rate limit and spend caps admit it; a Jev account on the
//! caller's route is leased through [`select::lease`], so seat ownership,
//! concurrency slots and the sticky pin apply; the per-credential breaker,
//! same-credential retries and failover follow the chat path's rules through
//! its own helpers; and every answer is a ledger row.
//!
//! The bytes are the SDK's. The request is checked against
//! [`SystemOneRequest`] and forwarded as it arrived; the answer is checked
//! against [`SystemOneResponse`] and returned as it arrived. Mounted under
//! `/jev`, so the unmodified `typesafe_sdk::Client` works with
//! `base_url = https://<gateway>/jev`.

use super::climb::meter_context;
use super::failover::{
    Step, apply_disposition, backoff, may_try_another, step_for, transport_failure,
    upstream_retry_after,
};
use super::respond::{identity_headers, truncate};
use super::{Caller, budgets_for, error_response, meter, refresh, select};
use crate::AppState;
use crate::breakers::Dispatch;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use oag_core::credential::SecretMaterial;
use oag_core::{AccountId, Error, Provider, RequestId, Result};
use oag_pool::SessionKey;
use oag_router::{
    BudgetPressure, Capabilities, Catalog, ModelId, ModelSpec, Pricing, RoutingDecision,
    SelectionReason,
};
use oag_upstream::{JevUpstream, Transport as _, TransportKey};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use typesafe_sdk::wire::{ListModelsResponse, SystemOneRequest, SystemOneResponse};

/// Where Jev names the request that produced an answer, and where the SDK's
/// `request_id()` reads it. Passed through, so a caller can quote it to Jev's
/// operators; the SDK keeps its own copy of the name private.
const REQUEST_ID_HEADER: &str = "x-typesafe-request-id";

/// `POST /jev/v1/systemone`.
///
/// [`Caller`] before `Bytes`, as on every inference route: an unauthenticated
/// request is refused on its head, before its body is buffered.
pub async fn system_one(
    State(state): State<Arc<AppState>>,
    Caller(auth): Caller,
    body: axum::body::Bytes,
) -> Response {
    // Moved into the ledger write, which outlives the response: a shutdown
    // drain waits for the row as it does for a chat answer's.
    let guard = state.lifecycle.track();
    answered(ask(&state, &auth, body, RequestId::new(), guard).await)
}

/// `GET /jev/v1/models`: the leased Jev credential's own listing.
///
/// Jev's, not the catalog's. What a Jev key can ask is Jev's to say, and a
/// caller reading this is asking exactly that.
pub async fn models(State(state): State<Arc<AppState>>, Caller(auth): Caller) -> Response {
    answered(list(&state, &auth, RequestId::new()).await)
}

fn answered(result: Result<Response>) -> Response {
    result.unwrap_or_else(|e| {
        metrics::counter!("oag_requests_total", "outcome" => "error").increment(1);
        error_response(&e)
    })
}

async fn ask(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    body: axum::body::Bytes,
    request_id: RequestId,
    guard: crate::shutdown::InFlightGuard,
) -> Result<Response> {
    let started = Instant::now();
    // The SDK's own decoder is the check. A body the client would refuse to
    // send is a body this refuses to forward, as a 400 that costs no credential
    // and no upstream call: not an object, no `state`, no question, a question
    // of no shape the client knows — each error names what is wrong. The value
    // is only read; what goes upstream is `body`, as it arrived.
    let request: SystemOneRequest = serde_json::from_slice(&body)?;
    let route = admit(state, auth).await?;
    tracing::debug!(
        %request_id,
        model = ?request.model,
        questions = request.questions.len(),
        "asking Jev"
    );

    let answer = call(
        state,
        auth,
        &route,
        request_id,
        |jev, credential| jev.system_one(credential, body.clone()),
        decoded::<SystemOneResponse>,
    )
    .await?;

    // The model Jev says answered, which is what ran, and so what the ledger
    // and `x-oag-model` name — not the one asked for, which may be an alias.
    let response = &answer.reply.decoded;
    let decision = RoutingDecision {
        model: priced(&*state.system_one_catalog().await, &response.model),
        tier: None,
        reason: SelectionReason::Passthrough,
        capability_escalated_from: None,
        ceiling_model: None,
    };
    let usage = oag_router::Usage {
        input_tokens: tokens(response.usage.input_tokens),
        output_tokens: tokens(response.usage.output_tokens),
        ..oag_router::Usage::default()
    };
    let ctx = meter_context(
        auth,
        &decision,
        &answer.lease,
        request_id,
        started,
        answer.attempt,
    );
    // Before the ledger write, which is ours rather than the credential's.
    answer.lease.release().await;

    // Detached, as the chat path's writes are: a client that hangs up now
    // cancels this future, and Jev has answered and billed either way.
    let writer = Arc::clone(state);
    tokio::spawn(async move {
        let _guard = guard;
        meter::record_answer(&writer, &ctx, usage).await;
    });

    Ok(relay(
        answer.reply,
        Some(decision.model.id.as_str()),
        request_id,
    ))
}

async fn list(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    request_id: RequestId,
) -> Result<Response> {
    let route = admit(state, auth).await?;
    let answer = call(
        state,
        auth,
        &route,
        request_id,
        JevUpstream::models,
        decoded::<ListModelsResponse>,
    )
    .await?;
    answer.lease.release().await;
    Ok(relay(answer.reply, None, request_id))
}

/// The chat path's admission without its ladder: the route's rate limit, then
/// the caller's spend caps.
///
/// Budget pressure has one meaning here. `Constrained` moves a chat request to
/// a cheaper rung, and System One has no cheaper rung to move to — so only
/// `Exhausted`, the hard stop, refuses.
async fn admit(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
) -> Result<oag_store::RouteRow> {
    let route = oag_store::repo::route_by_id(&state.db, auth.route_id)
        .await?
        .ok_or_else(|| Error::Internal("route vanished between auth and admission".to_owned()))?;
    if let Some(rpm) = route.rpm_limit
        && let Ok(rpm) = u32::try_from(rpm)
        && let Some(retry_after) = state.cache.take_rate_token(route.id, rpm).await?
    {
        return Err(Error::RateLimited { retry_after });
    }
    let spend = oag_store::repo::spend_for(&state.db, auth.api_key_id, auth.principal_id).await?;
    let budgets = budgets_for(auth, &route, &spend);
    if budgets.pressure() == BudgetPressure::Exhausted {
        return Err(Error::BudgetExhausted {
            scope: budgets.binding(),
        });
    }
    Ok(route)
}

/// A 2xx from Jev, read whole and checked.
struct Reply<T> {
    status: StatusCode,
    headers: reqwest::header::HeaderMap,
    body: bytes::Bytes,
    decoded: T,
}

/// A Jev answer, with the credential that gave it.
struct Answered<T> {
    lease: select::Lease,
    /// Which credential of this request's answered, counted from zero: the
    /// ledger row's `attempt`.
    attempt: u8,
    reply: Reply<T>,
}

/// Send one request through a Jev credential on the caller's route, failing
/// over between them under the chat path's rules.
///
/// `build` makes the request for whichever credential was leased, and
/// `decode` checks that a 2xx body is what it claims to be. A 2xx that is not
/// is not an answer, and moves to the next credential as a 5xx would — the
/// chat path's rule for a body that is not JSON.
///
/// Two bounds, as there: `max_account_switches` on how many credentials, and
/// `failover_budget` on how long, checked only between them.
async fn call<T>(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    route: &oag_store::RouteRow,
    request_id: RequestId,
    build: impl Fn(&JevUpstream, &SecretMaterial) -> Result<reqwest::Request>,
    decode: impl Fn(&[u8]) -> Result<T>,
) -> Result<Answered<T>> {
    // One pin per caller: a coarse affinity, the chat path's fallback form.
    // There is no conversation for a finer one to follow.
    let session = SessionKey::from_caller(&auth.api_key_id.to_string(), Provider::Jev.as_str());
    let mut excluded: HashSet<AccountId> = HashSet::new();
    let mut last_error: Option<Error> = None;
    let began = Instant::now();
    let budget = state.config.gateway.failover_budget;

    for switch in 0..=state.config.gateway.max_account_switches {
        if !may_try_another(switch, began.elapsed(), budget) {
            tracing::warn!(
                %request_id,
                switch,
                "failover budget spent; returning the last Jev error"
            );
            metrics::counter!("oag_failover_budget_exhausted_total").increment(1);
            break;
        }
        let lease = match select::lease(
            state,
            auth.route_id,
            auth.principal_id,
            Provider::Jev,
            &session,
            &excluded,
            &request_id.to_string(),
            None,
        )
        .await
        {
            Ok(lease) => lease,
            // After a failover the interesting error is why the credential
            // already tried failed, not that there is none left to try.
            Err(e) => {
                return Err(match last_error {
                    Some(last) => last,
                    None => unconfigured(state, auth, route, e).await,
                });
            }
        };
        let account = lease.account.account_id();
        match attempt_on(state, &lease, request_id, &build, &decode).await {
            Tried::Answered(reply) => {
                metrics::counter!(
                    "oag_requests_total",
                    "outcome" => "ok",
                    "provider" => Provider::Jev.as_str(),
                )
                .increment(1);
                return Ok(Answered {
                    lease,
                    attempt: switch,
                    reply: *reply,
                });
            }
            Tried::Fatal(e) => {
                lease.release().await;
                return Err(e);
            }
            Tried::Switch(e) => {
                tracing::warn!(%request_id, %account, error = %e, "switching Jev credential");
                last_error = Some(e);
                lease.release().await;
                excluded.insert(account);
            }
            // Nothing was sent and nothing failed: say nothing about this one.
            Tried::Raced => {
                lease.release().await;
                excluded.insert(account);
            }
        }
    }

    Err(last_error.unwrap_or(Error::NoCredential {
        provider: Provider::Jev,
    }))
}

/// Name the refusal for a route that holds no Jev credential at all.
///
/// `lease` answers `NoCredential` for a route with no Jev account and for one
/// whose Jev accounts are all disabled or cooling down. Only the first is "not
/// configured", so it is asked separately — on this failure path only, where
/// the extra read costs a request that is refused either way.
async fn unconfigured(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    route: &oag_store::RouteRow,
    e: Error,
) -> Error {
    if matches!(e, Error::NoCredential { .. })
        && oag_store::repo::candidates(
            &state.db,
            auth.route_id,
            Provider::Jev.as_str(),
            auth.principal_id,
        )
        .await
        .is_ok_and(|accounts| accounts.is_empty())
    {
        return Error::SystemOneNotConfigured {
            route: route.name.clone(),
        };
    }
    e
}

/// What one Jev credential's attempts came to.
enum Tried<T> {
    /// Boxed: a whole reply beside the error variants would make every one of
    /// them as large.
    Answered(Box<Reply<T>>),
    /// Try another credential.
    Switch(Error),
    /// Another request took this credential's half-open probe between
    /// selection and dispatch. Nothing was sent.
    Raced,
    /// Another credential cannot help.
    Fatal(Error),
}

/// One credential, with bounded same-credential retries: `try_credential`'s
/// rules, for a body that is sent whole and read whole.
async fn attempt_on<T>(
    state: &Arc<AppState>,
    lease: &select::Lease,
    request_id: RequestId,
    build: &impl Fn(&JevUpstream, &SecretMaterial) -> Result<reqwest::Request>,
    decode: &impl Fn(&[u8]) -> Result<T>,
) -> Tried<T> {
    let account = lease.account.account_id();
    // For an API key this is the unseal, trimmed: it never expires, so there
    // is nothing to refresh.
    let credential = match refresh::ensure_fresh(state, &lease.account).await {
        Ok(credential) => credential,
        Err(e) => return Tried::Switch(e),
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let Some(mut dispatch) = Dispatch::claim(&state.breakers, account, now) else {
        return Tried::Raced;
    };

    let retries = state.config.gateway.same_account_retries;
    let mut last = Error::NoCredential {
        provider: Provider::Jev,
    };
    for attempt in 0..=retries {
        let request = match build(state.jev(), &credential) {
            Ok(request) => request,
            // A request that cannot be built for one credential cannot be
            // built for another.
            Err(e) => return Tried::Fatal(e),
        };
        let transport = match state
            .transports
            .get(&TransportKey {
                account,
                proxy: lease.account.proxy_url.clone(),
            })
            .await
        {
            Ok(transport) => transport,
            Err(e) => return Tried::Switch(e),
        };
        let retries_left = attempt < retries;

        dispatch.sent();
        let step = match transport.execute(request).await {
            Ok(response) if response.status().is_success() => {
                let ceiling = state.config.gateway.max_stream_duration;
                return match read(response, decode, ceiling).await {
                    Ok(reply) => {
                        state.breakers.record_success(account);
                        Tried::Answered(Box::new(reply))
                    }
                    Err(e) => {
                        tracing::warn!(
                            %request_id, %account, error = %e,
                            "Jev answered 2xx with no answer in it"
                        );
                        state.breakers.record_failure(account);
                        Tried::Switch(e)
                    }
                };
            }
            Ok(response) => {
                last = refusal(response, account).await;
                let disposition = last.disposition();
                tracing::warn!(%request_id, %account, error = %last, ?disposition, "Jev refused");
                state.breakers.record_failure(account);
                apply_disposition(state, account, disposition).await;
                step_for(disposition, retries_left)
            }
            // Silent for the whole headers deadline. Fail over at once, as the
            // chat path does: a same-credential retry would spend that deadline
            // again before another credential was tried.
            Err(e @ Error::UpstreamTimeout { .. }) => {
                tracing::warn!(%request_id, %account, error = %e, "Jev silent");
                state.breakers.record_failure(account);
                apply_disposition(state, account, e.disposition()).await;
                return Tried::Switch(e);
            }
            // Nothing came back at all: connect, TLS or DNS.
            Err(e) => {
                tracing::warn!(
                    %request_id, %account, error = %e, retrying = retries_left,
                    "Jev unreachable"
                );
                if let Some(d) = transport_failure(&state.breakers, account, retries_left) {
                    apply_disposition(state, account, d).await;
                }
                last = e;
                if retries_left {
                    Step::Retry
                } else {
                    Step::Switch
                }
            }
        };

        match step {
            // The breaker may have opened on the failure just recorded, and a
            // credential it has this moment called unhealthy gets no more of
            // this request's retries.
            Step::Retry
                if state
                    .breakers
                    .permits(account, time::OffsetDateTime::now_utc().unix_timestamp()) =>
            {
                tokio::time::sleep(backoff(attempt)).await;
            }
            // A bigger model is the chat path's answer to a request too large
            // for this one. System One has no rung to climb, so the caller has
            // the refusal, like any other that no credential can fix.
            Step::Escalate | Step::Fatal => return Tried::Fatal(last),
            Step::Retry | Step::Switch => return Tried::Switch(last),
        }
    }
    Tried::Switch(last)
}

/// A 2xx body, read whole under the same ceiling a chat body is, then checked.
async fn read<T>(
    response: reqwest::Response,
    decode: &impl Fn(&[u8]) -> Result<T>,
    ceiling: Duration,
) -> Result<Reply<T>> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = tokio::time::timeout(ceiling, response.bytes())
        .await
        .map_err(|_| {
            Error::Internal(format!(
                "Jev's answer was not complete after {}s",
                ceiling.as_secs()
            ))
        })?
        .map_err(|e| Error::Internal(format!("reading Jev's answer: {e}")))?;
    let decoded = decode(&body)?;
    Ok(Reply {
        status,
        headers,
        body,
        decoded,
    })
}

/// A 2xx body checked against the wire type it claims to be.
fn decoded<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| {
        Error::Internal(format!(
            "Jev answered with a body that is not a {}: {e}",
            std::any::type_name::<T>()
        ))
    })
}

/// Jev's refusal, as the error every upstream refusal is.
async fn refusal(response: reqwest::Response, account: AccountId) -> Error {
    let status = response.status().as_u16();
    // Read before the body is consumed: `text()` takes the whole response.
    let retry_after = upstream_retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    Error::Upstream {
        provider: Provider::Jev,
        account,
        status,
        body: truncate(&body, 512),
        retry_after,
    }
}

/// Jev's bytes as they arrived, with Jev's request id and this gateway's
/// identity beside them.
fn relay<T>(reply: Reply<T>, model: Option<&str>, request_id: RequestId) -> Response {
    let content_type = reply
        .headers
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));
    let mut builder = identity_headers(Response::builder().status(reply.status), request_id)
        .header(header::CONTENT_TYPE, content_type);
    if let Some(id) = reply.headers.get(REQUEST_ID_HEADER) {
        builder = builder.header(REQUEST_ID_HEADER, id.clone());
    }
    // Named by Jev's answer, so not trusted to be a header value: a model name
    // that is not one loses this header, never the answer it names.
    if let Some(model) = model.and_then(|m| HeaderValue::from_str(m).ok()) {
        builder = builder.header("x-oag-model", model);
    }
    builder
        .body(Body::from(reply.body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The catalog's entry for the model Jev says answered, or an unpriced stand-in
/// for one it has no row for.
///
/// Unpriced rather than refused: the answer has been given and billed either
/// way, and a row with no cost is a pricing gap the dashboard shows, where no
/// row at all is spend nobody sees.
fn priced(catalog: &Catalog, model: &str) -> ModelSpec {
    let id = ModelId::new(format!("{}/{model}", Provider::Jev));
    catalog.get(&id).cloned().unwrap_or_else(|| ModelSpec {
        id,
        provider: Provider::Jev,
        upstream_name: model.to_owned(),
        pricing: Pricing {
            input_per_mtok: rust_decimal::Decimal::ZERO,
            output_per_mtok: rust_decimal::Decimal::ZERO,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        },
        context_window: 0,
        max_output_tokens: 0,
        capabilities: Capabilities::default(),
        display_label: None,
    })
}

/// A count Jev reported, as the ledger counts. Absent or negative is zero:
/// nothing anyone can be billed for.
fn tokens(count: Option<i64>) -> u64 {
    count.and_then(|n| u64::try_from(n).ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests;
