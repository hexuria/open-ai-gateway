//! Prompt-size preflight.
//!
//! Claude Code and similar clients call this before nearly every turn to decide
//! whether to compact. Without it they either skip compaction or guess.
//!
//! The number is an **estimate**. No tokeniser is linked, so it is marked as an
//! estimate in the body and in a header rather than presented as a count. It is
//! deliberately not `estimated_prompt_tokens`, which feeds a routing threshold
//! and is biased low in ways that are harmless there and are not harmless here:
//! a client that under-counts never compacts and then takes a hard
//! context-overflow error from the provider.
//!
//! Costs nothing upstream. It does not lease a concurrency slot — a preflight
//! must not consume `max_concurrency` on a path hit before every turn — and it
//! writes no ledger row, because no spend occurred. It *does* take a rate-limit
//! token: that is a throughput guard, not a money guard, and an unthrottled
//! preflight is still traffic.

use super::{Caller, error_response};
use crate::AppState;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use oag_core::{Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;

/// `POST /v1/messages/count_tokens`.
pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    Caller(auth): Caller,
    body: axum::body::Bytes,
) -> Response {
    match count(&state, &auth, &body, Body::Anthropic).await {
        Ok(tokens) => (
            [("x-oag-token-count", "estimate")],
            axum::Json(json!({ "input_tokens": tokens, "oag_estimate": true })),
        )
            .into_response(),
        Err(e) => error_response(&e),
    }
}

/// The Gemini spelling of the same thing, reached via `models/x:countTokens`.
pub(super) async fn gemini_count(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    body: &axum::body::Bytes,
) -> Response {
    match count(state, auth, body, Body::Gemini).await {
        Ok(tokens) => (
            [("x-oag-token-count", "estimate")],
            axum::Json(json!({ "totalTokens": tokens, "oag_estimate": true })),
        )
            .into_response(),
        Err(e) => error_response(&e),
    }
}

/// Which dialect the body is in. The path decides, not the content: a Gemini
/// body parsed as an Anthropic one has no `messages` key and counts as zero,
/// which is the most dangerous possible answer for a compaction trigger.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Anthropic,
    Gemini,
}

async fn count(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    body: &[u8],
    dialect: Body,
) -> Result<u64> {
    let route = oag_store::repo::route_by_id(&state.db, auth.route_id)
        .await?
        .ok_or_else(|| Error::Internal("route vanished between auth and counting".to_owned()))?;
    if let Some(rpm) = route.rpm_limit
        && let Ok(rpm) = u32::try_from(rpm)
        && let Some(retry_after) = state.cache.take_rate_token(route.id, rpm).await?
    {
        return Err(Error::RateLimited { retry_after });
    }

    let wire: Value = serde_json::from_slice(body)?;
    let canonical = match dialect {
        Body::Anthropic => oag_proto::anthropic::parse_request(&wire)?,
        Body::Gemini => oag_proto::gemini::parse_request(&wire)?,
    };

    // The residual walk is Anthropic-shaped, and it exists because
    // `anthropic::parse_block` drops block types it does not model. Applying it
    // to a Gemini body would find nothing and cost a pointless traversal.
    let residual = if dialect == Body::Anthropic {
        residual(&wire)
    } else {
        0
    };
    Ok(oag_proto::count_input_tokens(&canonical) + residual)
}

/// Tokens the canonical parse drops on the floor.
///
/// `anthropic::parse_block` returns `None` for any block type it does not model
/// — `document`, `redacted_thinking`, `server_tool_use`, `web_search_tool_result`
/// — and its image arm requires a base64 `source.data`, so a URL-source image
/// vanishes too. All of them cost real tokens upstream, and reporting zero for
/// them is the low-bias failure this endpoint exists to avoid.
fn residual(wire: &Value) -> u64 {
    const MODELLED: [&str; 5] = ["text", "image", "tool_use", "tool_result", "thinking"];

    let system = wire["system"].as_array().into_iter().flatten();
    let content = wire["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_array())
        .flatten();

    system
        .chain(content)
        .map(|block| {
            let kind = block["type"].as_str().unwrap_or_default();
            if kind == "image" {
                // Counted by the canonical walk only when `source.data` is a
                // base64 string; a URL source parses to nothing.
                return if block["source"]["data"].as_str().is_some() {
                    0
                } else {
                    1_500
                };
            }
            if MODELLED.contains(&kind) {
                return 0;
            }
            (block.to_string().len() / 4) as u64
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::residual;
    use serde_json::json;

    #[test]
    fn block_types_the_canonical_parse_drops_still_cost_tokens() {
        // `anthropic::parse_block` returns None for anything it does not model,
        // so without this walk a prompt made largely of `document` blocks
        // reports close to zero and the client never compacts.
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "document", "source": { "type": "text", "data": "x".repeat(400) } },
                ],
            }],
        });
        assert!(
            residual(&body) > 50,
            "an unmodelled block must contribute something"
        );
    }

    #[test]
    fn a_url_source_image_is_counted_and_a_base64_one_is_not_double_counted() {
        // The canonical walk prices an image only when `source.data` is a
        // base64 string; a URL source parses to nothing at all.
        let url_image = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": { "type": "url", "url": "https://example.invalid/a.png" },
                }],
            }],
        });
        assert_eq!(residual(&url_image), 1_500);

        let inline_image = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" },
                }],
            }],
        });
        assert_eq!(
            residual(&inline_image),
            0,
            "the canonical walk already priced this one"
        );
    }

    #[test]
    fn modelled_blocks_contribute_nothing_here() {
        let body = json!({
            "system": [{ "type": "text", "text": "be brief" }],
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "hi" },
                    { "type": "tool_use", "id": "t1", "name": "ls", "input": {} },
                    { "type": "tool_result", "tool_use_id": "t1", "content": "ok" },
                    { "type": "thinking", "thinking": "hmm" },
                ],
            }],
        });
        assert_eq!(residual(&body), 0, "counting these twice would overstate");
    }

    #[test]
    fn a_body_with_no_messages_is_not_an_error() {
        assert_eq!(residual(&json!({})), 0);
        assert_eq!(residual(&json!({ "messages": "not an array" })), 0);
    }
}
