//! The Anthropic Messages dialect.
//!
//! This is the hub's native shape, so rendering is close to identity and
//! parsing is where the work is. Anthropic streams a response as a sequence of
//! typed events with content blocks addressed by index, which does not line up
//! with either OpenAI dialect — that mismatch is why translation needs a state
//! machine and not a map.

use crate::canonical::{CacheControl, CanonicalRequest, ContentBlock, Message, Role, Tool};
use crate::stream::{StopReason, StreamAccumulator, StreamEvent};
use oag_core::Result;
use oag_router::Usage;
use serde_json::{Value, json};

/// The API version header value. Anthropic requires it on every request and
/// changes behaviour without it.
pub const API_VERSION: &str = "2023-06-01";

/// Canonical → Anthropic wire JSON.
pub fn render_request(req: &CanonicalRequest, upstream_model: &str) -> Result<Value> {
    let mut body = json!({
        "model": upstream_model,
        "max_tokens": req.max_tokens,
        "stream": req.stream,
        "messages": req.messages.iter().map(render_message).collect::<Vec<_>>(),
    });

    if !req.system.is_empty() {
        body["system"] = Value::Array(req.system.iter().map(render_block).collect());
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(render_tool).collect());
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(budget) = req.thinking_budget {
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
    Ok(body)
}

fn render_message(m: &Message) -> Value {
    json!({
        "role": match m.role {
            // Anthropic has no `system` or `tool` message role: system is a
            // top-level field, and tool results are user-turn content blocks.
            Role::Assistant => "assistant",
            _ => "user",
        },
        "content": m.content.iter().map(render_block).collect::<Vec<_>>(),
    })
}

fn render_block(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text {
            text,
            cache_control,
        } => {
            let mut v = json!({ "type": "text", "text": text });
            if cache_control.is_some() {
                v["cache_control"] = json!({ "type": "ephemeral" });
            }
            v
        }
        ContentBlock::Image { media_type, data } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        }),
        ContentBlock::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        ContentBlock::Thinking { text, signature } => {
            let mut v = json!({ "type": "thinking", "thinking": text });
            if let Some(sig) = signature {
                // Verbatim. Anthropic rejects a replayed thinking block whose
                // signature does not match what it issued, so this must not be
                // normalised, re-encoded, or trimmed.
                v["signature"] = json!(sig);
            }
            v
        }
    }
}

fn render_tool(t: &Tool) -> Value {
    let mut v = json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.input_schema,
    });
    if t.cache_control.is_some() {
        v["cache_control"] = json!({ "type": "ephemeral" });
    }
    v
}

/// Anthropic wire JSON → canonical.
///
/// Used for inbound requests, where a client speaks Anthropic natively.
pub fn parse_request(body: &Value) -> Result<CanonicalRequest> {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let max_tokens = u32::try_from(body["max_tokens"].as_u64().unwrap_or(4096)).unwrap_or(4096);

    // `system` is either a bare string or an array of blocks, and the array
    // form is the one that carries cache breakpoints — which is what session
    // affinity keys on, so both forms must parse.
    let system = match &body["system"] {
        Value::String(s) => vec![ContentBlock::Text {
            text: s.clone(),
            cache_control: None,
        }],
        Value::Array(items) => items.iter().filter_map(parse_block).collect(),
        _ => vec![],
    };

    let messages = body["messages"]
        .as_array()
        .map(|arr| arr.iter().filter_map(parse_message).collect())
        .unwrap_or_default();

    let tools = body["tools"]
        .as_array()
        .map(|arr| arr.iter().filter_map(parse_tool).collect())
        .unwrap_or_default();

    let thinking_budget = body["thinking"]["budget_tokens"]
        .as_u64()
        .and_then(|b| u32::try_from(b).ok());

    // Claude Code puts a session id inside metadata.user_id. Preferred over
    // content hashing for affinity because it is exact and survives the
    // conversation growing past its cache breakpoints.
    let client_session = body["metadata"]["user_id"]
        .as_str()
        .map(std::borrow::ToOwned::to_owned);

    Ok(CanonicalRequest {
        model,
        system,
        messages,
        tools,
        max_tokens,
        stream: body["stream"].as_bool().unwrap_or(false),
        // Temperature is 0.0-2.0 with two useful decimals; f32 holds it
        // exactly enough and matches what every provider accepts on the wire.
        #[allow(clippy::cast_possible_truncation)]
        temperature: body["temperature"].as_f64().map(|t| t as f32),
        thinking_budget,
        client_session,
    })
}

fn parse_message(v: &Value) -> Option<Message> {
    let role = match v["role"].as_str()? {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    };
    let content = match &v["content"] {
        Value::String(s) => vec![ContentBlock::Text {
            text: s.clone(),
            cache_control: None,
        }],
        Value::Array(items) => items.iter().filter_map(parse_block).collect(),
        _ => vec![],
    };
    Some(Message { role, content })
}

fn parse_block(v: &Value) -> Option<ContentBlock> {
    let cache_control = v["cache_control"]["type"]
        .as_str()
        .map(|_| CacheControl::Ephemeral);

    match v["type"].as_str()? {
        "text" => Some(ContentBlock::Text {
            text: v["text"].as_str()?.to_owned(),
            cache_control,
        }),
        "image" => Some(ContentBlock::Image {
            media_type: v["source"]["media_type"].as_str()?.to_owned(),
            data: v["source"]["data"].as_str()?.to_owned(),
        }),
        "tool_use" => Some(ContentBlock::ToolUse {
            id: v["id"].as_str()?.to_owned(),
            name: v["name"].as_str()?.to_owned(),
            input: v["input"].clone(),
        }),
        "tool_result" => Some(ContentBlock::ToolResult {
            tool_use_id: v["tool_use_id"].as_str()?.to_owned(),
            content: match &v["content"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            is_error: v["is_error"].as_bool().unwrap_or(false),
        }),
        "thinking" => Some(ContentBlock::Thinking {
            text: v["thinking"].as_str().unwrap_or_default().to_owned(),
            signature: v["signature"].as_str().map(std::borrow::ToOwned::to_owned),
        }),
        _ => None,
    }
}

fn parse_tool(v: &Value) -> Option<Tool> {
    Some(Tool {
        name: v["name"].as_str()?.to_owned(),
        description: v["description"].as_str().unwrap_or_default().to_owned(),
        input_schema: v["input_schema"].clone(),
        cache_control: v["cache_control"]["type"]
            .as_str()
            .map(|_| CacheControl::Ephemeral),
    })
}

/// One SSE `data:` payload → canonical events.
///
/// Takes the JSON payload, not the whole `event:`/`data:` pair — the event name
/// duplicates the payload's own `type` field, and trusting one source is better
/// than reconciling two.
///
/// An empty result is normal: `ping` and the block-lifecycle events carry
/// nothing the canonical form needs.
pub fn parse_event(payload: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
    let v: Value = serde_json::from_str(payload)?;

    Ok(match v["type"].as_str().unwrap_or_default() {
        "message_start" => {
            let msg = &v["message"];
            vec![StreamEvent::Start {
                model: msg["model"].as_str().unwrap_or_default().to_owned(),
                usage: parse_usage(&msg["usage"]),
            }]
        }

        "content_block_start" => {
            // Only tool blocks need an opening event; text and thinking are
            // fully described by their deltas.
            let block = &v["content_block"];
            match block["type"].as_str().unwrap_or_default() {
                "tool_use" => vec![StreamEvent::ToolUseStart {
                    id: block["id"].as_str().unwrap_or_default().to_owned(),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                }],
                _ => vec![],
            }
        }

        "content_block_delta" => {
            let delta = &v["delta"];
            match delta["type"].as_str().unwrap_or_default() {
                "text_delta" => vec![StreamEvent::TextDelta {
                    text: delta["text"].as_str().unwrap_or_default().to_owned(),
                }],
                "thinking_delta" => vec![StreamEvent::ThinkingDelta {
                    text: delta["thinking"].as_str().unwrap_or_default().to_owned(),
                }],
                "input_json_delta" => vec![StreamEvent::ToolUseDelta {
                    // Anthropic addresses blocks by index, not id, in deltas.
                    // The accumulator holds the mapping.
                    id: acc.current_tool_id().unwrap_or_default(),
                    partial_json: delta["partial_json"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                }],
                // signature_delta carries the thinking signature, which matters
                // only when replaying the block on a later turn; it is not part
                // of the streamed answer.
                _ => vec![],
            }
        }

        "content_block_stop" => acc
            .current_tool_id()
            .map(|id| vec![StreamEvent::ToolUseEnd { id }])
            .unwrap_or_default(),

        "message_delta" => {
            let mut events = vec![StreamEvent::UsageUpdate {
                usage: parse_usage(&v["usage"]),
            }];
            if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                events.push(StreamEvent::Stop {
                    reason: parse_stop_reason(reason),
                    usage: parse_usage(&v["usage"]),
                });
            }
            events
        }

        // An error inside a 200 body. Its own variant because it is not an HTTP
        // failure, and whether we can still fail over depends on whether any
        // bytes have already reached the client.
        "error" => vec![StreamEvent::Error {
            message: v["error"]["message"]
                .as_str()
                .unwrap_or("upstream error")
                .to_owned(),
        }],

        // ping, message_stop, and anything Anthropic adds later.
        _ => vec![],
    })
}

// ── rendering canonical events back into this dialect ─────────────────────────

/// State carried while rendering canonical events as Anthropic SSE.
///
/// This dialect is the more structured of the two: content arrives as indexed
/// *blocks* that must be explicitly opened and closed, where Chat Completions
/// just streams deltas. So a renderer has to track which block is open and
/// close it before opening another — a client that receives a
/// `content_block_delta` for a block it was never told about will drop it.
#[derive(Debug, Clone, Default)]
pub struct RenderState {
    id: String,
    model: String,
    started: bool,
    /// The block index currently open, if any.
    open_block: Option<usize>,
    next_index: usize,
    /// Whether the open block is a tool call, which closes differently.
    open_is_tool: bool,
    usage: Usage,
    finished: bool,
}

impl RenderState {
    #[must_use]
    pub fn new(request_id: &str, model: &str) -> Self {
        Self {
            id: format!("msg_{request_id}"),
            model: model.to_owned(),
            ..Self::default()
        }
    }

    fn frame(event: &str, body: &Value) -> String {
        // Both lines: this dialect's clients dispatch on the `event:` name, and
        // omitting it makes an SDK ignore the frame entirely.
        format!("event: {event}\ndata: {body}\n\n")
    }

    fn close_open_block(&mut self, out: &mut String) {
        if let Some(index) = self.open_block.take() {
            out.push_str(&Self::frame(
                "content_block_stop",
                &json!({ "type": "content_block_stop", "index": index }),
            ));
        }
        self.open_is_tool = false;
    }

    fn ensure_started(&mut self, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        out.push_str(&Self::frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "usage": usage_json(&self.usage),
                }
            }),
        ));
    }
}

/// One canonical event → Anthropic SSE frames, if it produces any.
pub fn render_event(event: &StreamEvent, st: &mut RenderState) -> Option<String> {
    let mut out = String::new();

    match event {
        StreamEvent::Start { model, usage } => {
            if !model.is_empty() {
                st.model.clone_from(model);
            }
            st.usage.merge(usage);
            st.ensure_started(&mut out);
        }

        StreamEvent::TextDelta { text } => render_text(st, &mut out, text, false),
        StreamEvent::ThinkingDelta { text } => render_text(st, &mut out, text, true),

        StreamEvent::ToolUseStart { id, name } => {
            st.ensure_started(&mut out);
            st.close_open_block(&mut out);
            let index = st.next_index;
            st.next_index += 1;
            st.open_block = Some(index);
            st.open_is_tool = true;
            out.push_str(&RenderState::frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start", "index": index,
                    "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
                }),
            ));
        }

        StreamEvent::ToolUseDelta { partial_json, .. } => {
            let index = st.open_block?;
            out.push_str(&RenderState::frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta", "index": index,
                    "delta": { "type": "input_json_delta", "partial_json": partial_json }
                }),
            ));
        }

        StreamEvent::ToolUseEnd { .. } => st.close_open_block(&mut out),

        StreamEvent::UsageUpdate { usage } => {
            st.usage.merge(usage);
            return None;
        }

        StreamEvent::Stop { reason, usage } => {
            if st.finished {
                return None;
            }
            st.finished = true;
            st.usage.merge(usage);
            st.ensure_started(&mut out);
            st.close_open_block(&mut out);

            out.push_str(&RenderState::frame(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": stop_reason_str(*reason), "stop_sequence": Value::Null },
                    "usage": { "output_tokens": st.usage.output_tokens }
                }),
            ));
            out.push_str(&RenderState::frame(
                "message_stop",
                &json!({ "type": "message_stop" }),
            ));
        }

        StreamEvent::Error { message } => {
            out.push_str(&RenderState::frame(
                "error",
                &json!({ "type": "error",
                         "error": { "type": "api_error", "message": message } }),
            ));
        }
    }

    (!out.is_empty()).then_some(out)
}

/// Emit a text or reasoning delta, opening a block first if none is open.
fn render_text(st: &mut RenderState, out: &mut String, text: &str, thinking: bool) {
    st.ensure_started(out);

    // A tool block has to be closed before text can resume: this dialect has
    // exactly one block open at a time, and interleaving them makes a client
    // attach the text to the tool call.
    if st.open_is_tool {
        st.close_open_block(out);
    }

    if st.open_block.is_none() {
        let index = st.next_index;
        st.next_index += 1;
        st.open_block = Some(index);
        let empty = if thinking {
            json!({ "type": "thinking", "thinking": "" })
        } else {
            json!({ "type": "text", "text": "" })
        };
        out.push_str(&RenderState::frame(
            "content_block_start",
            &json!({ "type": "content_block_start", "index": index, "content_block": empty }),
        ));
    }

    let index = st.open_block.unwrap_or(0);
    let delta = if thinking {
        json!({ "type": "thinking_delta", "thinking": text })
    } else {
        json!({ "type": "text_delta", "text": text })
    };
    out.push_str(&RenderState::frame(
        "content_block_delta",
        &json!({ "type": "content_block_delta", "index": index, "delta": delta }),
    ));
}

/// A Chat Completions response → an Anthropic Messages response.
#[must_use]
pub fn render_message_response(completion: &Value, request_id: &str) -> Value {
    let choice = &completion["choices"][0];
    let message = &choice["message"];
    let mut content = Vec::new();

    if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
        content.push(json!({ "type": "text", "text": text }));
    }
    for call in message["tool_calls"].as_array().unwrap_or(&Vec::new()) {
        content.push(json!({
            "type": "tool_use",
            "id": call["id"],
            "name": call["function"]["name"],
            // A parsed value here, a JSON string there.
            "input": call["function"]["arguments"]
                .as_str()
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .unwrap_or_else(|| json!({})),
        }));
    }

    let u = &completion["usage"];
    let cached = u["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);

    json!({
        "id": format!("msg_{request_id}"),
        "type": "message",
        "role": "assistant",
        "model": completion["model"],
        "content": content,
        "stop_reason": match choice["finish_reason"].as_str().unwrap_or("stop") {
            "length" => "max_tokens",
            "tool_calls" | "function_call" => "tool_use",
            "content_filter" => "refusal",
            _ => "end_turn",
        },
        "stop_sequence": Value::Null,
        "usage": {
            // Split back apart: this dialect reports the uncached prompt and
            // the cached prefix separately.
            "input_tokens": u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
            "output_tokens": u["completion_tokens"].as_u64().unwrap_or(0),
            "cache_read_input_tokens": cached,
            "cache_creation_input_tokens": 0,
        }
    })
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cache_read_input_tokens": u.cache_read_tokens,
        "cache_creation_input_tokens": u.cache_write_tokens,
    })
}

const fn stop_reason_str(r: StopReason) -> &'static str {
    match r {
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::Refusal => "refusal",
        StopReason::EndTurn => "end_turn",
    }
}

fn parse_usage(v: &Value) -> Usage {
    Usage {
        input_tokens: v["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: v["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: v["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: v["cache_creation_input_tokens"].as_u64().unwrap_or(0),
    }
}

fn parse_stop_reason(raw: &str) -> StopReason {
    match raw {
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::extract_cache_blocks;

    /// A realistic streamed response: cached prefix, some text, a tool call
    /// assembled from fragments, then a stop with output counts.
    ///
    /// Shaped exactly as Anthropic sends it, including the parts that are easy
    /// to get wrong: usage split across the first and last events, tool
    /// arguments arriving as partial JSON, and deltas addressed by index rather
    /// than by the id the opening event carried.
    const STREAM: &[&str] = &[
        r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1200,"cache_read_input_tokens":18000,"cache_creation_input_tokens":300,"output_tokens":0}}}"#,
        r#"{"type":"ping"}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me "}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"check that."}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_abc","name":"read_file","input":{}}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":": \"src/main.rs\"}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":142}}"#,
        r#"{"type":"message_stop"}"#,
    ];

    fn drive(lines: &[&str]) -> (Vec<StreamEvent>, StreamAccumulator) {
        let mut acc = StreamAccumulator::new();
        let mut out = Vec::new();
        for line in lines {
            let events = parse_event(line, &mut acc).expect("parses");
            for e in &events {
                acc.observe(e);
            }
            out.extend(events);
        }
        (out, acc)
    }

    #[test]
    fn a_full_stream_produces_the_expected_events() {
        let (events, _) = drive(STREAM);
        assert!(matches!(events.first(), Some(StreamEvent::Start { .. })));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Let me check that.");
    }

    #[test]
    fn usage_survives_the_split_across_first_and_last_events() {
        // The bug this guards: input and cache counts arrive in message_start
        // and are never repeated, so assigning rather than merging silently
        // under-bills every streamed request by the entire prompt.
        let (_, acc) = drive(STREAM);
        let u = acc.usage();
        assert_eq!(u.input_tokens, 1_200);
        assert_eq!(u.cache_read_tokens, 18_000);
        assert_eq!(u.cache_write_tokens, 300);
        assert_eq!(u.output_tokens, 142);
    }

    #[test]
    fn cache_hit_rate_is_computed_from_the_stream() {
        let (_, acc) = drive(STREAM);
        // 18000 cached of 19200 prompt tokens — the number that tells you
        // session affinity is working.
        assert!(acc.usage().cache_hit_rate() > rust_decimal::dec!(0.9));
    }

    #[test]
    fn tool_deltas_are_reattached_to_the_id_from_the_opening_event() {
        // Anthropic addresses deltas by block index; canonical events carry the
        // id. Losing the mapping produces tool calls with empty ids that never
        // reassemble.
        let (events, acc) = drive(STREAM);
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseDelta { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["toolu_abc", "toolu_abc"]);
        assert_eq!(acc.quality_gate(), None, "the fragments form valid JSON");
    }

    #[test]
    fn a_truncated_tool_call_trips_the_quality_gate() {
        // Same stream, minus the fragment that closes the JSON. This is the
        // classic small-model failure and the most valuable escalation signal.
        let mut lines = STREAM.to_vec();
        lines.remove(8);
        let (_, acc) = drive(&lines);
        assert_eq!(
            acc.quality_gate(),
            Some(oag_router::QualityGate::MalformedToolCall)
        );
    }

    #[test]
    fn ping_and_lifecycle_events_produce_nothing() {
        let mut acc = StreamAccumulator::new();
        assert!(
            parse_event(r#"{"type":"ping"}"#, &mut acc)
                .expect("parses")
                .is_empty()
        );
        assert!(
            parse_event(r#"{"type":"message_stop"}"#, &mut acc)
                .expect("parses")
                .is_empty()
        );
    }

    #[test]
    fn an_error_inside_a_200_body_becomes_an_error_event() {
        let mut acc = StreamAccumulator::new();
        let events = parse_event(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            &mut acc,
        )
        .expect("parses");
        assert_eq!(
            events,
            vec![StreamEvent::Error {
                message: "Overloaded".to_owned()
            }]
        );
    }

    #[test]
    fn unknown_event_types_are_ignored_rather_than_fatal() {
        // Providers add event types without warning. Failing the stream on one
        // would break every request the day they ship it.
        let mut acc = StreamAccumulator::new();
        let events = parse_event(r#"{"type":"something_new_in_2027"}"#, &mut acc);
        assert!(events.expect("must not error").is_empty());
    }

    #[test]
    fn a_request_round_trips_through_the_canonical_form() {
        let wire = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 4096,
            "stream": true,
            "system": [
                {"type": "text", "text": "You are helpful.",
                 "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "read_file", "description": "reads",
                       "input_schema": {"type": "object"}}],
            "thinking": {"type": "enabled", "budget_tokens": 8000},
            "metadata": {"user_id": "user_x_session_abc123"}
        });

        let canonical = parse_request(&wire).expect("parses");
        assert_eq!(canonical.max_tokens, 4096);
        assert!(canonical.stream);
        assert_eq!(canonical.thinking_budget, Some(8000));
        assert_eq!(canonical.tools.len(), 1);
        assert_eq!(
            canonical.client_session.as_deref(),
            Some("user_x_session_abc123"),
            "Claude Code's session id must survive; affinity depends on it"
        );
        assert_eq!(extract_cache_blocks(&canonical), vec!["You are helpful."]);

        let rendered = render_request(&canonical, "claude-opus-5").expect("renders");
        assert_eq!(rendered["max_tokens"], 4096);
        assert_eq!(rendered["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(rendered["thinking"]["budget_tokens"], 8000);
    }

    #[test]
    fn a_bare_string_system_prompt_parses() {
        // Both forms are in the wild; only the array form carries cache
        // breakpoints, but rejecting the string form would break plain clients.
        let wire = serde_json::json!({
            "model": "m", "max_tokens": 100,
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let c = parse_request(&wire).expect("parses");
        assert_eq!(c.system.len(), 1);
        assert_eq!(c.messages.len(), 1);
        assert!(
            extract_cache_blocks(&c).is_empty(),
            "no breakpoint was marked"
        );
    }

    /// Drive canonical events through the Anthropic renderer and return the
    /// `event:` names in order, plus the reassembled text.
    fn render_all(events: &[StreamEvent]) -> (Vec<String>, String) {
        let mut st = RenderState::new("req1", "kimi/k2");
        let mut raw = String::new();
        for e in events {
            if let Some(f) = render_event(e, &mut st) {
                raw.push_str(&f);
            }
        }
        let mut names = Vec::new();
        let mut text = String::new();
        for frame in raw.split("\n\n").filter(|f| !f.trim().is_empty()) {
            for line in frame.lines() {
                if let Some(n) = line.strip_prefix("event: ") {
                    names.push(n.to_owned());
                }
                if let Some(d) = line.strip_prefix("data: ") {
                    let v: Value = serde_json::from_str(d).expect("valid json");
                    if v["type"] == "content_block_delta" {
                        text.push_str(v["delta"]["text"].as_str().unwrap_or_default());
                    }
                }
            }
        }
        (names, text)
    }

    #[test]
    fn rendering_produces_the_dialects_exact_event_sequence() {
        // A client here dispatches on the `event:` name and on blocks being
        // opened before they are written to. Getting the order wrong makes an
        // SDK drop content rather than error, which is worse.
        let (names, text) = render_all(&[
            StreamEvent::Start {
                model: "kimi/k2".to_owned(),
                usage: Usage {
                    input_tokens: 900,
                    ..Usage::default()
                },
            },
            StreamEvent::TextDelta {
                text: "Cheap ".to_owned(),
            },
            StreamEvent::TextDelta {
                text: "answer.".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                usage: Usage {
                    output_tokens: 30,
                    ..Usage::default()
                },
            },
        ]);

        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(text, "Cheap answer.");
    }

    #[test]
    fn a_tool_call_closes_the_text_block_before_opening_its_own() {
        // One block open at a time in this dialect; interleaving them makes a
        // client attach the text to the tool call.
        let (names, _) = render_all(&[
            StreamEvent::TextDelta {
                text: "thinking...".to_owned(),
            },
            StreamEvent::ToolUseStart {
                id: "toolu_1".to_owned(),
                name: "f".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "toolu_1".to_owned(),
                partial_json: "{}".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]);

        let starts: Vec<usize> = names
            .iter()
            .enumerate()
            .filter(|(_, n)| *n == "content_block_start")
            .map(|(i, _)| i)
            .collect();
        let stops: Vec<usize> = names
            .iter()
            .enumerate()
            .filter(|(_, n)| *n == "content_block_stop")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(starts.len(), 2, "text block, then tool block");
        assert!(
            stops[0] < starts[1],
            "the text block closes before the tool opens"
        );
    }

    #[test]
    fn a_stream_that_starts_with_content_still_emits_message_start_first() {
        // Some upstreams send no opening event at all; a client that never
        // receives message_start ignores everything after it.
        let (names, _) = render_all(&[StreamEvent::TextDelta {
            text: "hi".to_owned(),
        }]);
        assert_eq!(names.first().map(String::as_str), Some("message_start"));
    }

    #[test]
    fn a_duplicate_stop_does_not_terminate_the_stream_twice() {
        let mut st = RenderState::new("r", "m");
        let stop = StreamEvent::Stop {
            reason: StopReason::EndTurn,
            usage: Usage::default(),
        };
        assert!(render_event(&stop, &mut st).is_some());
        assert!(render_event(&stop, &mut st).is_none());
    }

    #[test]
    fn a_completion_response_renders_back_into_this_dialect() {
        let completion = serde_json::json!({
            "model": "kimi-k2",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "f", "arguments": "{\"a\":1}"}}]
            }}],
            "usage": {"prompt_tokens": 900, "completion_tokens": 30,
                      "prompt_tokens_details": {"cached_tokens": 400}}
        });
        let out = render_message_response(&completion, "req1");

        assert_eq!(out["type"], "message");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["content"][0]["type"], "tool_use");
        // Arguments become a parsed value here, not a JSON string.
        assert_eq!(out["content"][0]["input"]["a"], 1);
        // And the prompt splits back into uncached and cached.
        assert_eq!(out["usage"]["input_tokens"], 500);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 400);
    }

    #[test]
    fn thinking_signatures_survive_a_render() {
        // Anthropic rejects a replayed thinking block whose signature changed,
        // so the render must emit it byte-for-byte.
        let sig = "EqQBCkYIBRgCKkC+abc/123==";
        let req = CanonicalRequest {
            model: "m".to_owned(),
            system: vec![],
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    text: "reasoning".to_owned(),
                    signature: Some(sig.to_owned()),
                }],
            }],
            tools: vec![],
            max_tokens: 100,
            stream: false,
            temperature: None,
            thinking_budget: None,
            client_session: None,
        };
        let out = render_request(&req, "m").expect("renders");
        assert_eq!(out["messages"][0]["content"][0]["signature"], sig);
    }
}
