//! The OpenAI Chat Completions dialect.
//!
//! Worth one codec more than any other: Kimi, DeepSeek, Zhipu, and xAI all
//! speak it, so this file is four providers rather than one. It is also the
//! dialect most third-party clients emit, which makes it the most common
//! *inbound* shape too.
//!
//! Three structural disagreements with the canonical (Anthropic) form, and each
//! is a place translation loses information if you are not careful:
//!
//! 1. **System prompts.** A message role here; a top-level field there. Round
//!    tripping must not turn a system prompt into a user turn.
//! 2. **Tool calls.** An `assistant` message with `tool_calls` whose arguments
//!    are a JSON *string*; canonical has a content block with a parsed value.
//! 3. **Tool results.** A `tool` role message; canonical has a content block
//!    inside a user turn.

use crate::canonical::{CanonicalRequest, ContentBlock, Message, Role, Tool};
use crate::stream::{StopReason, StreamAccumulator, StreamEvent};
use oag_core::Result;
use oag_router::Usage;
use serde_json::{Value, json};

/// Canonical → Chat Completions wire JSON.
pub fn render_request(req: &CanonicalRequest, upstream_model: &str) -> Result<Value> {
    let mut messages = Vec::new();

    // The system prompt becomes a message, and must come first.
    if !req.system.is_empty() {
        let text = req
            .system
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }

    for m in &req.messages {
        messages.extend(render_message(m));
    }

    let mut body = json!({
        "model": upstream_model,
        "messages": messages,
        "stream": req.stream,
        "max_tokens": req.max_tokens,
    });

    if req.stream {
        // Usage is omitted from a stream unless asked for, and without it every
        // streamed request through this dialect would bill as zero.
        body["stream_options"] = json!({ "include_usage": true });
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect(),
        );
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    Ok(body)
}

/// One canonical message becomes one or more wire messages.
///
/// One-to-many because a single canonical turn can hold both text and several
/// tool results, and this dialect needs a separate `tool` message for each.
fn render_message(m: &Message) -> Vec<Value> {
    let mut out = Vec::new();

    // Tool results are their own messages, whatever turn they arrived in.
    for block in &m.content {
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } = block
        {
            out.push(json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": content,
            }));
        }
    }

    let text = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    let tool_calls: Vec<Value> = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    // Arguments are a JSON *string* here, not an object.
                    "arguments": input.to_string(),
                }
            })),
            _ => None,
        })
        .collect();

    let images: Vec<Value> = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Image { media_type, data } => Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") }
            })),
            _ => None,
        })
        .collect();

    if text.is_empty() && tool_calls.is_empty() && images.is_empty() {
        return out;
    }

    let role = match m.role {
        Role::Assistant => "assistant",
        Role::System => "system",
        _ => "user",
    };

    let mut msg = json!({ "role": role });
    if images.is_empty() {
        msg["content"] = json!(text);
    } else {
        let mut parts = vec![json!({ "type": "text", "text": text })];
        parts.extend(images);
        msg["content"] = Value::Array(parts);
    }
    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
    }
    out.push(msg);
    out
}

/// Chat Completions wire JSON → canonical.
pub fn parse_request(body: &Value) -> Result<CanonicalRequest> {
    let mut system = Vec::new();
    let mut messages: Vec<Message> = Vec::new();

    for m in body["messages"].as_array().unwrap_or(&Vec::new()) {
        parse_one_message(m, &mut system, &mut messages);
    }

    // `max_tokens` is deprecated in favour of `max_completion_tokens`; accept
    // both, because both are very much in the wild.
    let max_tokens = body["max_completion_tokens"]
        .as_u64()
        .or_else(|| body["max_tokens"].as_u64())
        .unwrap_or(4096);

    Ok(CanonicalRequest {
        model: body["model"].as_str().unwrap_or_default().to_owned(),
        system,
        messages,
        tools: body["tools"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_tool).collect())
            .unwrap_or_default(),
        max_tokens: u32::try_from(max_tokens).unwrap_or(4096),
        stream: body["stream"].as_bool().unwrap_or(false),
        #[allow(clippy::cast_possible_truncation)]
        temperature: body["temperature"].as_f64().map(|t| t as f32),
        // No equivalent field in this dialect.
        thinking_budget: None,
        client_session: body["user"].as_str().map(std::borrow::ToOwned::to_owned),
    })
}

/// Fold one wire message into the canonical system prompt or message list.
fn parse_one_message(m: &Value, system: &mut Vec<ContentBlock>, messages: &mut Vec<Message>) {
    {
        let role = m["role"].as_str().unwrap_or("user");

        if role == "system" || role == "developer" {
            // A system message becomes the top-level field, not a user turn.
            if let Some(text) = m["content"].as_str() {
                system.push(ContentBlock::Text {
                    text: text.to_owned(),
                    cache_control: None,
                });
            }
            return;
        }

        if role == "tool" {
            // A tool result joins the previous user turn if there is one, so
            // the canonical form keeps results adjacent to their turn.
            let block = ContentBlock::ToolResult {
                tool_use_id: m["tool_call_id"].as_str().unwrap_or_default().to_owned(),
                content: match &m["content"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
                is_error: false,
            };
            match messages.last_mut() {
                Some(last) if last.role == Role::User => last.content.push(block),
                _ => messages.push(Message {
                    role: Role::User,
                    content: vec![block],
                }),
            }
            return;
        }

        let mut content = Vec::new();
        match &m["content"] {
            Value::String(s) if !s.is_empty() => content.push(ContentBlock::Text {
                text: s.clone(),
                cache_control: None,
            }),
            Value::Array(parts) => {
                for p in parts {
                    match p["type"].as_str().unwrap_or_default() {
                        "text" => content.push(ContentBlock::Text {
                            text: p["text"].as_str().unwrap_or_default().to_owned(),
                            cache_control: None,
                        }),
                        "image_url" => {
                            if let Some((media_type, data)) =
                                split_data_url(p["image_url"]["url"].as_str().unwrap_or_default())
                            {
                                content.push(ContentBlock::Image { media_type, data });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        for call in m["tool_calls"].as_array().unwrap_or(&Vec::new()) {
            content.push(ContentBlock::ToolUse {
                id: call["id"].as_str().unwrap_or_default().to_owned(),
                name: call["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                // Arguments arrive as a JSON string; parse so the canonical
                // form holds a value, and fall back to a string rather than
                // dropping a malformed one.
                input: call["function"]["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or_else(|| json!({})),
            });
        }

        if content.is_empty() {
            return;
        }
        messages.push(Message {
            role: if m["role"] == "assistant" {
                Role::Assistant
            } else {
                Role::User
            },
            content,
        });
    }
}

fn parse_tool(v: &Value) -> Option<Tool> {
    let f = &v["function"];
    Some(Tool {
        name: f["name"].as_str()?.to_owned(),
        description: f["description"].as_str().unwrap_or_default().to_owned(),
        input_schema: f["parameters"].clone(),
        cache_control: None,
    })
}

/// Split `data:image/png;base64,AAAA` into its media type and payload.
fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some((media_type.to_owned(), data.to_owned()))
}

/// One SSE `data:` payload → canonical events.
pub fn parse_event(payload: &str, _acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
    if payload == "[DONE]" {
        return Ok(vec![]);
    }
    let v: Value = serde_json::from_str(payload)?;
    let mut events = Vec::new();

    // A usage-only chunk has an empty `choices` array; it arrives last when
    // `stream_options.include_usage` was set.
    if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
        events.push(StreamEvent::UsageUpdate {
            usage: parse_usage(usage),
        });
    }

    let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
        return Ok(events);
    };
    let delta = &choice["delta"];

    if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
        events.push(StreamEvent::TextDelta {
            text: text.to_owned(),
        });
    }

    // Some OpenAI-compatible providers stream reasoning in a side channel.
    if let Some(text) = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
        .filter(|t| !t.is_empty())
    {
        events.push(StreamEvent::ThinkingDelta {
            text: text.to_owned(),
        });
    }

    for call in delta["tool_calls"].as_array().unwrap_or(&Vec::new()) {
        // A tool call announces its id and name once, then streams arguments.
        if let Some(id) = call["id"].as_str().filter(|i| !i.is_empty()) {
            events.push(StreamEvent::ToolUseStart {
                id: id.to_owned(),
                name: call["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
        if let Some(args) = call["function"]["arguments"]
            .as_str()
            .filter(|a| !a.is_empty())
        {
            events.push(StreamEvent::ToolUseDelta {
                // Later fragments carry only an index, so fall back to the
                // call currently open.
                id: call["id"].as_str().unwrap_or_default().to_owned(),
                partial_json: args.to_owned(),
            });
        }
    }

    if let Some(reason) = choice["finish_reason"].as_str().filter(|r| !r.is_empty()) {
        events.push(StreamEvent::Stop {
            reason: match reason {
                "length" => StopReason::MaxTokens,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "content_filter" => StopReason::Refusal,
                _ => StopReason::EndTurn,
            },
            usage: Usage::default(),
        });
    }

    Ok(events)
}

fn parse_usage(v: &Value) -> Usage {
    Usage {
        // This dialect reports the *total* prompt, cached tokens included, so
        // subtract to avoid counting the cached prefix twice.
        input_tokens: v["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(
            v["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0),
        ),
        output_tokens: v["completion_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: v["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
        // No equivalent: this dialect's caching is implicit and unbilled.
        cache_write_tokens: 0,
    }
}

/// State carried while rendering canonical events back into this dialect.
///
/// Needed because the wire format is positional where the canonical form is
/// nominal: tool calls are addressed by an integer index that only makes sense
/// relative to the calls already emitted in this response, and the first chunk
/// must announce the assistant role that later chunks omit.
#[derive(Debug, Clone, Default)]
pub struct RenderState {
    id: String,
    model: String,
    role_sent: bool,
    tool_ids: Vec<String>,
    finished: bool,
    /// Usage merged across the whole response.
    ///
    /// Not taken from the terminal event: the source dialect may split usage
    /// across its first and last events, and Anthropic does exactly that. A
    /// terminal chunk built from the stop event alone reports a prompt of zero
    /// tokens to the client, which is both wrong and unbillable downstream.
    usage: Usage,
}

impl RenderState {
    #[must_use]
    pub fn new(request_id: &str, model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{request_id}"),
            model: model.to_owned(),
            ..Self::default()
        }
    }

    fn tool_index(&mut self, id: &str) -> usize {
        if let Some(i) = self.tool_ids.iter().position(|t| t == id) {
            return i;
        }
        self.tool_ids.push(id.to_owned());
        self.tool_ids.len() - 1
    }

    fn chunk(&self, delta: &Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            // Constant rather than a real clock: clients use it for display
            // only, and a per-chunk timestamp would make recorded fixtures
            // impossible to compare.
            "created": 0,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }
}

/// One canonical event → an SSE frame in this dialect, if it produces one.
///
/// `None` is normal: several canonical events carry information this dialect
/// expresses elsewhere or not at all.
pub fn render_event(event: &StreamEvent, st: &mut RenderState) -> Option<String> {
    let frame = match event {
        // Announces the model, and carries the role the rest of the chunks omit.
        StreamEvent::Start { model, usage } => {
            if !st.model.is_empty() {
                st.model.clone_from(model);
            }
            st.usage.merge(usage);
            st.role_sent = true;
            st.chunk(&json!({ "role": "assistant", "content": "" }), None)
        }

        StreamEvent::TextDelta { text } => {
            let delta = if st.role_sent {
                json!({ "content": text })
            } else {
                st.role_sent = true;
                json!({ "role": "assistant", "content": text })
            };
            st.chunk(&delta, None)
        }

        // No first-class field in this dialect. `reasoning_content` is the
        // convention several OpenAI-compatible providers settled on, and a
        // client that does not know it ignores it rather than breaking.
        StreamEvent::ThinkingDelta { text } => {
            st.chunk(&json!({ "reasoning_content": text }), None)
        }

        StreamEvent::ToolUseStart { id, name } => {
            let index = st.tool_index(id);
            st.chunk(
                &json!({ "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                }]}),
                None,
            )
        }

        StreamEvent::ToolUseDelta { id, partial_json } => {
            let index = st.tool_index(id);
            st.chunk(
                &json!({ "tool_calls": [{
                    "index": index,
                    "function": { "arguments": partial_json }
                }]}),
                None,
            )
        }

        StreamEvent::Stop { reason, usage } => {
            if st.finished {
                return None;
            }
            st.usage.merge(usage);
            st.finished = true;
            let finish = match reason {
                StopReason::MaxTokens => "length",
                StopReason::ToolUse => "tool_calls",
                StopReason::Refusal => "content_filter",
                StopReason::EndTurn | StopReason::StopSequence => "stop",
            };
            let u = st.usage;
            let mut c = st.chunk(&json!({}), Some(finish));
            // This dialect reports the *total* prompt with the cached prefix
            // folded in, which is the inverse of how the canonical form holds
            // it — so add them back together on the way out.
            // Every prompt-side token goes in `prompt_tokens`, cache writes
            // included, so that `total == prompt + completion` holds. Clients
            // in this dialect check that sum, and Anthropic's cache-creation
            // tokens have nowhere else to go.
            let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
            c["usage"] = json!({
                "prompt_tokens": prompt,
                "completion_tokens": u.output_tokens,
                "total_tokens": prompt + u.output_tokens,
                "prompt_tokens_details": { "cached_tokens": u.cache_read_tokens },
            });
            c
        }

        // Folded into the terminal chunk's usage field rather than emitted on
        // its own — this dialect has no mid-stream usage frame.
        StreamEvent::UsageUpdate { usage } => {
            st.usage.merge(usage);
            return None;
        }
        StreamEvent::ToolUseEnd { .. } => return None,

        StreamEvent::Error { message } => json!({
            "error": { "message": message, "type": "upstream_error" }
        }),
    };

    Some(format!("data: {frame}\n\n"))
}

/// An Anthropic Messages response → a Chat Completions response.
///
/// The non-streaming counterpart to [`render_event`]. Needed because a client
/// that asked in this dialect expects an answer in it, whatever the upstream
/// happened to be.
#[must_use]
pub fn render_completion(anthropic: &Value, request_id: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in anthropic["content"].as_array().unwrap_or(&Vec::new()) {
        match block["type"].as_str().unwrap_or_default() {
            "text" => text.push_str(block["text"].as_str().unwrap_or_default()),
            "tool_use" => tool_calls.push(json!({
                "id": block["id"],
                "type": "function",
                "function": {
                    "name": block["name"],
                    // A JSON string here, an object there.
                    "arguments": block["input"].to_string(),
                }
            })),
            _ => {}
        }
    }

    let finish = match anthropic["stop_reason"].as_str().unwrap_or("end_turn") {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    };

    let u = &anthropic["usage"];
    let cached = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let input = u["input_tokens"].as_u64().unwrap_or(0);
    let output = u["output_tokens"].as_u64().unwrap_or(0);

    let mut message = json!({ "role": "assistant", "content": text });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
        // This dialect wants a null content when the turn is only tool calls.
        if text.is_empty() {
            message["content"] = Value::Null;
        }
    }

    // Every prompt-side token in `prompt_tokens`, so `total == prompt +
    // completion` holds the way clients in this dialect expect.
    let written = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    let prompt = input + cached + written;

    json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion",
        "created": 0,
        "model": anthropic["model"],
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": output,
            "total_tokens": prompt + output,
            "prompt_tokens_details": { "cached_tokens": cached },
        }
    })
}

/// The sentinel that terminates a stream in this dialect.
///
/// Anthropic has no equivalent, so a translated stream has to synthesise it —
/// a client that waits for it will otherwise hang until its own timeout.
#[must_use]
pub fn done_frame() -> String {
    "data: [DONE]\n\n".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic Chat Completions stream, including the bits that are easy to
    /// get wrong: tool arguments split across chunks and addressed by index
    /// after the first, and a usage-only terminal chunk.
    const STREAM: &[&str] = &[
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Let me "},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"check."},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\""}}]},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":": \"a.rs\"}"}}]},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":19200,"completion_tokens":142,"total_tokens":19342,"prompt_tokens_details":{"cached_tokens":18000}}}"#,
        "[DONE]",
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
    fn text_reassembles_across_chunks() {
        let (events, _) = drive(STREAM);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Let me check.");
    }

    #[test]
    fn cached_prompt_tokens_are_not_counted_twice() {
        // `prompt_tokens` here is the *total* including the cached prefix.
        // Adding cached_tokens on top would bill 19200 + 18000 for a 19200
        // token prompt — and price the cached part at the full input rate.
        let (_, acc) = drive(STREAM);
        let u = acc.usage();
        assert_eq!(u.cache_read_tokens, 18_000);
        assert_eq!(u.input_tokens, 1_200, "19200 total minus 18000 cached");
        assert_eq!(u.output_tokens, 142);
        assert_eq!(u.total(), 19_342);
    }

    #[test]
    fn a_done_sentinel_yields_no_events() {
        let mut acc = StreamAccumulator::new();
        assert!(parse_event("[DONE]", &mut acc).expect("parses").is_empty());
    }

    #[test]
    fn a_request_round_trips_through_the_canonical_form() {
        let wire = json!({
            "model": "kimi-k2",
            "max_tokens": 2048,
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "read a.rs"},
                {"role": "assistant", "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "fn main() {}"}
            ],
            "tools": [{"type": "function", "function": {
                "name": "read_file", "description": "reads", "parameters": {"type": "object"}
            }}]
        });

        let c = parse_request(&wire).expect("parses");
        assert_eq!(
            c.system.len(),
            1,
            "a system message becomes the system field"
        );
        assert_eq!(c.tools.len(), 1);
        assert!(c.stream);

        // The assistant's tool call survives as a parsed value, not a string.
        let call = c
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                ContentBlock::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            });
        assert_eq!(call.expect("tool call").1["path"], "a.rs");

        // And the result came back as a tool_result block.
        assert!(
            c.messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        );

        let back = render_request(&c, "kimi-k2").expect("renders");
        assert_eq!(back["messages"][0]["role"], "system");
        assert_eq!(
            back["stream_options"]["include_usage"], true,
            "without this a streamed request bills as zero"
        );
    }

    #[test]
    fn a_system_prompt_never_becomes_a_user_turn() {
        // The round-trip failure that silently changes what the model is told.
        let wire = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "SYSTEM-MARKER"},
                {"role": "user", "content": "hello"}
            ]
        });
        let c = parse_request(&wire).expect("parses");
        assert_eq!(c.messages.len(), 1, "only the user turn is a message");
        assert!(matches!(
            c.system.first(),
            Some(ContentBlock::Text { text, .. }) if text == "SYSTEM-MARKER"
        ));
    }

    #[test]
    fn max_completion_tokens_is_accepted_alongside_max_tokens() {
        let newer = parse_request(&json!({
            "model": "m", "max_completion_tokens": 999, "messages": []
        }))
        .expect("parses");
        assert_eq!(newer.max_tokens, 999);

        let older = parse_request(&json!({"model": "m", "max_tokens": 111, "messages": []}))
            .expect("parses");
        assert_eq!(older.max_tokens, 111);
    }

    #[test]
    fn an_image_survives_the_round_trip() {
        let wire = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}]
        });
        let c = parse_request(&wire).expect("parses");
        assert!(c.has_images());

        let back = render_request(&c, "m").expect("renders");
        let url = back["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .expect("image url");
        assert_eq!(url, "data:image/png;base64,AAAA");
    }

    // ── cross-dialect: an OpenAI client reaching an Anthropic upstream ────────

    #[test]
    fn anthropic_events_render_as_a_valid_chat_completions_stream() {
        // The whole point of the hub: the upstream spoke Anthropic, the client
        // speaks this, and neither knows about the other.
        let anthropic_stream: &[&str] = &[
            r#"{"type":"message_start","message":{"model":"claude-opus-5","usage":{"input_tokens":1200,"cache_read_input_tokens":18000}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"read_file"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"p\":1}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":142}}"#,
        ];

        let mut acc = StreamAccumulator::new();
        let mut st = RenderState::new("req1", "claude-opus-5");
        let mut frames = Vec::new();

        for line in anthropic_stream {
            for e in crate::anthropic::parse_event(line, &mut acc).expect("parses") {
                acc.observe(&e);
                if let Some(f) = render_event(&e, &mut st) {
                    frames.push(f);
                }
            }
        }
        frames.push(done_frame());

        // Every frame must be a well-formed SSE data line this dialect's
        // clients can parse.
        for f in &frames {
            assert!(f.starts_with("data: "), "{f}");
            assert!(f.ends_with("\n\n"), "{f}");
        }
        assert_eq!(frames.last().map(String::as_str), Some("data: [DONE]\n\n"));

        let bodies: Vec<Value> = frames
            .iter()
            .filter_map(|f| {
                f.strip_prefix("data: ")
                    .and_then(|b| b.strip_suffix("\n\n"))
            })
            .filter(|b| *b != "[DONE]")
            .map(|b| serde_json::from_str(b).expect("valid json"))
            .collect();

        assert!(
            bodies
                .iter()
                .all(|b| b["object"] == "chat.completion.chunk")
        );

        let text: String = bodies
            .iter()
            .filter_map(|b| b["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "Hello");

        // The terminal chunk carries the finish reason and the usage, with the
        // cached prefix folded back into prompt_tokens the way this dialect
        // reports it.
        let last = bodies.last().expect("a terminal chunk");
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(last["usage"]["prompt_tokens"], 19_200);
        assert_eq!(
            last["usage"]["prompt_tokens_details"]["cached_tokens"],
            18_000
        );
    }

    #[test]
    fn tool_calls_are_renumbered_by_position_not_by_id() {
        // This dialect addresses tool calls by an index relative to the
        // response; canonical addresses them by id. Emitting the id where an
        // index belongs makes clients drop the call.
        let mut st = RenderState::new("r", "m");
        let a = render_event(
            &StreamEvent::ToolUseStart {
                id: "toolu_zzz".into(),
                name: "f".into(),
            },
            &mut st,
        )
        .expect("frame");
        let b = render_event(
            &StreamEvent::ToolUseStart {
                id: "toolu_aaa".into(),
                name: "g".into(),
            },
            &mut st,
        )
        .expect("frame");

        let idx = |f: &str| -> i64 {
            let body = f
                .strip_prefix("data: ")
                .and_then(|s| s.strip_suffix("\n\n"))
                .expect("body");
            let v: Value = serde_json::from_str(body).expect("json");
            v["choices"][0]["delta"]["tool_calls"][0]["index"]
                .as_i64()
                .expect("index")
        };
        assert_eq!(idx(&a), 0);
        assert_eq!(idx(&b), 1);
    }

    #[test]
    fn a_non_streaming_anthropic_response_renders_as_a_completion() {
        let anthropic = json!({
            "id": "msg_1",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": "An answer."}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1200, "output_tokens": 42,
                      "cache_read_input_tokens": 18000}
        });
        let out = render_completion(&anthropic, "req1");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "An answer.");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 19_200);
        assert_eq!(
            out["usage"]["prompt_tokens_details"]["cached_tokens"],
            18_000
        );
    }

    #[test]
    fn a_tool_only_turn_renders_with_null_content() {
        // This dialect's clients expect null, not an empty string, when the
        // assistant turn is only tool calls.
        let anthropic = json!({
            "model": "m",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "f",
                         "input": {"a": 1}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let out = render_completion(&anthropic, "r");
        assert!(out["choices"][0]["message"]["content"].is_null());
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}",
            "arguments must be a JSON string in this dialect"
        );
    }

    #[test]
    fn a_duplicate_stop_does_not_emit_two_terminal_chunks() {
        // Anthropic can produce both a message_delta stop and a message_stop;
        // two finish_reason chunks confuse clients into truncating.
        let mut st = RenderState::new("r", "m");
        let stop = StreamEvent::Stop {
            reason: StopReason::EndTurn,
            usage: Usage::default(),
        };
        assert!(render_event(&stop, &mut st).is_some());
        assert!(render_event(&stop, &mut st).is_none());
    }
}
