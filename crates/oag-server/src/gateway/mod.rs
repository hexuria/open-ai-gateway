//! The inference request path.

pub mod alias;
pub mod authn;
mod climb;
pub mod count_tokens;
mod failover;
pub mod meter;
pub mod models;
mod plan;
mod presence;
pub mod refresh;
mod respond;
pub mod select;
pub mod sse;

pub use authn::{Caller, require_key_layer};

use crate::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use climb::run_with_escalation;
use oag_core::provider::Dialect;
use oag_core::{Error, RequestId, Result};
use oag_pool::SessionKey;
use oag_proto::{anthropic, extract_cache_blocks};
use plan::plan_request;
use std::sync::Arc;
use std::time::Instant;

pub(crate) use failover::{MAX_BACKOFF, adapter_for};
#[cfg(test)]
use plan::virtual_tier;
pub(crate) use plan::{budgets_for, extract_key, policy_for};
pub(crate) use respond::error_response;

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

#[cfg(test)]
mod tests;
