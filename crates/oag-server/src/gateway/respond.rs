//! Building what the caller gets back: bodies, streams, headers and errors.

use super::climb::meter_context;
use super::failover::egress_for;
use super::{adapter_for, meter, select, sse};
use crate::AppState;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use oag_core::provider::Dialect;
use oag_core::{Error, RequestId, Result};
use oag_proto::FunctionNameMap;
use oag_router::{RoutingDecision, TierLadder};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

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
pub(super) fn render_collected(
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
pub(super) fn json_response(
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
pub(super) fn client_got_nothing(
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
    let builder = identity_headers(
        builder.header("x-oag-model", decision.model.id.as_str()),
        request_id,
    );
    match decision.rung_name() {
        Some(tier) => builder.header("x-oag-tier", tier),
        None => builder,
    }
}

/// Which request this was and which build answered it: the part of
/// [`oag_headers`] that is not about routing, so a System One answer — which
/// was never routed — carries it too.
pub(super) fn identity_headers(
    builder: axum::http::response::Builder,
    request_id: RequestId,
) -> axum::http::response::Builder {
    builder
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
        .header(crate::BUILD_HEADER, crate::build_id())
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
pub(super) fn stream_response(
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

pub(super) fn truncate(s: &str, max: usize) -> String {
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
/// A **3xx** is 502 as well. It tells the client to send its request, gateway
/// key and all, somewhere else, and names nowhere: the provider's `Location` is
/// not forwarded, and is not the client's to follow. The upstream transport
/// follows no redirect, because a key would go with it, so one arriving here is
/// a provider that has moved, and its base URL is ours to fix.
///
/// Everything else keeps the provider's status, because everything else is
/// already about the right party: 400, 413 and 422 are the client's own request,
/// and 5xx already reads as ours.
fn client_status_for(upstream: u16) -> StatusCode {
    match upstream {
        300..=399 | 401..=403 | 407 => StatusCode::BAD_GATEWAY,
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
// Long because it is a table — one arm per client-facing kind — and not
// branching logic. Split up, the one place a status is decided for a client
// would be several.
#[allow(clippy::too_many_lines)]
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
        // 503, not the 500 of `internal_error`: nothing here broke, and no
        // credential the route holds could be made ready to send. Its own
        // kind, because its fix is not another's: a key replaced, or a token
        // endpoint back. The message names Google's refusal and nothing of
        // the key.
        Error::UpstreamUnavailable { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
            e.to_string(),
        ),
        // Its own kind, not `no_credential`: that one is also what a route
        // whose Jev keys are all cooling down gets, and a client that branches
        // on the kind should not wait out a key nobody has added.
        Error::SystemOneNotConfigured { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "system_one_not_configured",
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
pub(super) fn no_viable_message(route: &str, requested: &str, ladder: &TierLadder) -> String {
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

    // Not a ladder problem, so not the ladder fix: the chat catalog holds no
    // System One model, so a rung naming one would serve nothing either.
    if let Some(name) = provider
        && name
            .parse::<oag_core::Provider>()
            .is_ok_and(|p| !p.native_dialect().is_chat())
    {
        return format!(
            "'{requested}' is a System One model: it answers questions rather than continuing \
             a conversation, so no chat route serves it. Send it to POST /jev/v1/systemone"
        );
    }

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
