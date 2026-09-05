//! The Anthropic Messages dialect.
//!
//! This is the hub's native shape, so rendering is close to identity and
//! parsing is where the work is. Anthropic streams a response as a sequence of
//! typed events with content blocks addressed by index, which does not line up
//! with either OpenAI dialect — that mismatch is why translation needs a state
//! machine and not a map.

use crate::canonical::{
    CacheControl, CanonicalRequest, ContentBlock, Effort, Message, ResponseFormat, Role, Tool,
    ToolChoice, ToolResultContent,
};
use crate::stream::{StopReason, StreamAccumulator, StreamEvent};
use oag_core::provider::Dialect;
use oag_core::{Error, Result};
use oag_router::Usage;
use serde_json::{Value, json};

/// The API version header value. Anthropic requires it on every request and
/// changes behaviour without it.
pub const API_VERSION: &str = "2023-06-01";

const DIALECT: Dialect = Dialect::AnthropicMessages;

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
    // A level from a client that speaks levels becomes a budget here, the
    // mirror of the OpenAI renderer turning a budget into the nearest level.
    // `openai::parse_request` deliberately leaves `thinking_budget` empty, so
    // gating on the budget alone meant a Chat Completions client's
    // `reasoning_effort: "high"` had nowhere to land: the request reached a
    // thinking model with thinking switched off, at frontier prices, and
    // nothing in the answer said the field had been dropped.
    if let Some(budget) = req
        .thinking_budget
        .or_else(|| req.thinking_effort.map(Effort::as_budget))
    {
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }

    // Two fields this dialect simply does not have. Structured output is not
    // available on Messages at all — prompting for JSON is not the same
    // promise — and there is no stored-response id to continue from.
    if req
        .response_format
        .as_ref()
        .is_some_and(ResponseFormat::constrains_output)
    {
        return Err(Error::UnsupportedField {
            field: "response_format",
            dialect: DIALECT,
        });
    }
    if req.previous_response_id.is_some() {
        return Err(Error::UnsupportedField {
            field: "previous_response_id",
            dialect: DIALECT,
        });
    }

    if let Some(choice) = &req.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::Auto => json!({ "type": "auto" }),
            // `any` here, `required` everywhere else.
            ToolChoice::Required => json!({ "type": "any" }),
            ToolChoice::None => json!({ "type": "none" }),
            ToolChoice::Tool { name } => json!({ "type": "tool", "name": name }),
        };
    }
    if !req.stop.is_empty() {
        body["stop_sequences"] = json!(req.stop);
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
        } => {
            // A string stays a string and blocks stay blocks: this is the one
            // dialect that can carry an image inside a result, and the round
            // trip through here must be byte-faithful.
            let content = match content {
                ToolResultContent::Text(text) => json!(text),
                ToolResultContent::Blocks(blocks) => {
                    Value::Array(blocks.iter().map(render_block).collect())
                }
            };
            json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            })
        }
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
        // This dialect speaks budgets. Carry the nearest level too, so a hop to
        // one that speaks levels does not silently drop the request to think.
        thinking_effort: thinking_budget.map(Effort::from_budget),
        client_session,
        tool_choice: parse_tool_choice(&body["tool_choice"]),
        // Neither exists in this dialect, so a client speaking it never set one.
        response_format: None,
        stop: body["stop_sequences"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(std::borrow::ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        previous_response_id: None,
    })
}

fn parse_tool_choice(v: &Value) -> Option<ToolChoice> {
    match v["type"].as_str()? {
        "auto" => Some(ToolChoice::Auto),
        "any" => Some(ToolChoice::Required),
        "none" => Some(ToolChoice::None),
        "tool" => Some(ToolChoice::Tool {
            name: v["name"].as_str()?.to_owned(),
        }),
        _ => None,
    }
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
                Value::String(s) => ToolResultContent::Text(s.clone()),
                // Blocks, kept as blocks. Stringifying this array was how an
                // image in a tool result reached the model as a JSON string
                // of its own base64.
                Value::Array(items) => {
                    ToolResultContent::Blocks(items.iter().filter_map(parse_block).collect())
                }
                other => ToolResultContent::Text(other.to_string()),
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

        // Every block ends with this event, whatever kind it was, and the
        // event does not say which. `current_tool_id` answered "the last call
        // opened", so a text block closing after a tool call had already
        // finished emitted a second end for that call — and a renderer that
        // acts on the end (Gemini emits the call there, Anthropic closes its
        // block) acted on it twice.
        "content_block_stop" => acc
            .open_tool_id()
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

/// A complete non-streamed response → the events its stream would have carried.
///
/// The counterpart to [`parse_event`], and it has to reach the same accumulator
/// state: the quality gate reads the text and tool-call counts, so a reader
/// that takes only usage leaves every non-streamed response looking empty.
#[must_use]
pub fn parse_response(body: &Value) -> Vec<StreamEvent> {
    let usage = parse_usage(&body["usage"]);
    let mut events = vec![StreamEvent::UsageUpdate { usage }];

    for block in body["content"].as_array().unwrap_or(&Vec::new()) {
        match block["type"].as_str().unwrap_or_default() {
            "text" => events.push(StreamEvent::TextDelta {
                text: block["text"].as_str().unwrap_or_default().to_owned(),
            }),
            "thinking" => events.push(StreamEvent::ThinkingDelta {
                text: block["thinking"].as_str().unwrap_or_default().to_owned(),
            }),
            "tool_use" => {
                let id = block["id"].as_str().unwrap_or_default().to_owned();
                events.push(StreamEvent::ToolUseStart {
                    id: id.clone(),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                });
                // Whole, not fragmented — a non-streamed tool call is already
                // complete JSON, so the malformed-arguments gate should never
                // fire on one.
                events.push(StreamEvent::ToolUseDelta {
                    id: id.clone(),
                    partial_json: block["input"].to_string(),
                });
                events.push(StreamEvent::ToolUseEnd { id });
            }
            _ => {}
        }
    }

    if let Some(reason) = body["stop_reason"].as_str() {
        events.push(StreamEvent::Stop {
            reason: parse_stop_reason(reason),
            usage,
        });
    }

    events
}

// ── rendering canonical events back into this dialect ─────────────────────────

/// State carried while rendering canonical events as Anthropic SSE.
///
/// This dialect is the more structured of the two: content arrives as indexed
/// *blocks* that must be explicitly opened and closed, where Chat Completions
/// just streams deltas. So a renderer has to track which blocks are open and
/// address every delta to the right one — a client that receives a
/// `content_block_delta` for a block it was never told about will drop it,
/// and one that receives it under the wrong index attaches the fragment to
/// the wrong call.
///
/// Text and tool blocks are held apart. There is one text block open at a
/// time, because a text delta names no block and can only mean the current
/// one. Tool blocks are a list keyed by call id, because a Chat Completions
/// upstream streams parallel calls interleaved and every fragment says which
/// call it belongs to. A single "open block" slot, as this used to be, wrote
/// every fragment into whichever call opened last: one call shipped with
/// `input: {}` and the other with two calls' arguments run together.
#[derive(Debug, Clone, Default)]
pub struct RenderState {
    id: String,
    model: String,
    started: bool,
    /// The text or thinking block currently open, if any.
    open_text: Option<usize>,
    /// Tool blocks opened and not yet closed: call id and block index, in
    /// the order they opened.
    open_tools: Vec<(String, usize)>,
    next_index: usize,
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

    fn block_stop(out: &mut String, index: usize) {
        out.push_str(&Self::frame(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": index }),
        ));
    }

    fn close_text(&mut self, out: &mut String) {
        if let Some(index) = self.open_text.take() {
            Self::block_stop(out, index);
        }
    }

    /// Close the tool block for `id`, if it is open.
    fn close_tool(&mut self, out: &mut String, id: &str) {
        if let Some(at) = self.open_tools.iter().position(|(open, _)| open == id) {
            let (_, index) = self.open_tools.remove(at);
            Self::block_stop(out, index);
        }
    }

    /// Close every open tool block, in the order they opened.
    fn close_tools(&mut self, out: &mut String) {
        for (_, index) in self.open_tools.drain(..) {
            Self::block_stop(out, index);
        }
    }

    /// The block a tool fragment belongs to.
    ///
    /// The one opened under its id, or, for a fragment whose id opened
    /// nothing, the most recently opened call — the Responses parser
    /// addresses fragments by whichever call is current, and that is the one
    /// it means.
    fn tool_block(&self, id: &str) -> Option<usize> {
        self.open_tools
            .iter()
            .find(|(open, _)| open == id)
            .or_else(|| self.open_tools.last())
            .map(|(_, index)| *index)
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
            crate::stream::adopt_model(&mut st.model, model);
            st.usage.merge(usage);
            st.ensure_started(&mut out);
        }

        StreamEvent::TextDelta { text } => render_text(st, &mut out, text, false),
        StreamEvent::ThinkingDelta { text } => render_text(st, &mut out, text, true),

        // Opens a block and leaves any open tool block alone: another call
        // opening says nothing about whether this one has more fragments
        // coming. Its stop goes out when its own end does.
        StreamEvent::ToolUseStart { id, name } => {
            st.ensure_started(&mut out);
            st.close_text(&mut out);
            let index = st.next_index;
            st.next_index += 1;
            st.open_tools.push((id.clone(), index));
            out.push_str(&RenderState::frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start", "index": index,
                    "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
                }),
            ));
        }

        StreamEvent::ToolUseDelta { id, partial_json } => {
            let index = st.tool_block(id)?;
            out.push_str(&RenderState::frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta", "index": index,
                    "delta": { "type": "input_json_delta", "partial_json": partial_json }
                }),
            ));
        }

        StreamEvent::ToolUseEnd { id } => st.close_tool(&mut out, id),

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
            st.close_text(&mut out);
            st.close_tools(&mut out);

            out.push_str(&RenderState::frame(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": stop_reason_str(*reason), "stop_sequence": Value::Null },
                    // The full merged usage, not `output_tokens` alone.
                    //
                    // A native Anthropic upstream reports input and cache
                    // counts in `message_start`, so repeating only the output
                    // count on the terminal frame is what the dialect itself
                    // does. But `message_start` is rendered before any upstream
                    // has reported usage, and a Chat Completions upstream
                    // reports all of it at the end — so an Anthropic client
                    // reading a non-Anthropic model was told, permanently, that
                    // its prompt cost zero tokens. The other three renderers
                    // put the whole merged figure on their terminal frame.
                    // Repeating counts a native upstream already sent is
                    // harmless: a client merges these, as this gateway does.
                    "usage": {
                        "input_tokens": st.usage.input_tokens,
                        "output_tokens": st.usage.output_tokens,
                        "cache_read_input_tokens": st.usage.cache_read_tokens,
                        "cache_creation_input_tokens": st.usage.cache_write_tokens,
                    }
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

    // Every tool block has to be closed before text can resume: a text delta
    // names no block, and a client with a tool block still open attaches the
    // text to the tool call.
    st.close_tools(out);

    if st.open_text.is_none() {
        let index = st.next_index;
        st.next_index += 1;
        st.open_text = Some(index);
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

    let index = st.open_text.unwrap_or(0);
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

/// A content block under assembly by `render_from_events`.
enum OpenBlock {
    Text(String),
    Thinking(String),
    Tool {
        id: String,
        name: String,
        input: String,
    },
}

impl OpenBlock {
    /// The finished block, as this dialect writes it.
    fn into_json(self) -> Value {
        match self {
            Self::Text(text) => json!({ "type": "text", "text": text }),
            Self::Thinking(text) => json!({ "type": "thinking", "thinking": text }),
            Self::Tool { id, name, input } => json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                // Whole again. Fragments that never became an object are the
                // quality gate's to escalate on; here they render as empty.
                "input": serde_json::from_str::<Value>(&input).unwrap_or_else(|_| json!({})),
            }),
        }
    }
}

/// Close the open block, if any, into `content`.
fn close_block(open: &mut Option<OpenBlock>, content: &mut Vec<Value>) {
    if let Some(block) = open.take() {
        content.push(block.into_json());
    }
}

/// Canonical events → a complete Anthropic Messages response body.
///
/// The hub for every non-streamed answer. Whatever the upstream was — any
/// dialect, and streamed or not — its response is read into canonical events
/// (`parse_response` for a body, the adapter's `parse_event` for a stream),
/// rendered into this shape, and only then converted to the client's dialect
/// by the same converters that already take an Anthropic body:
/// `openai::render_completion`, `gemini::render_message_response`,
/// `responses::render_response`.
///
/// One hub rather than one converter per pair, because a converter per pair
/// is how eight of the twelve pairs came to return an empty 200: each
/// converter read exactly one upstream shape, the caller picked among them by
/// the *client's* dialect, and a body in any other shape yielded nothing.
///
/// The inverse of `parse_response`, and pinned as such: parsing what this
/// renders yields the events that were rendered.
#[must_use]
pub fn render_from_events(events: &[StreamEvent], request_id: &str, model: &str) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut open: Option<OpenBlock> = None;
    let mut usage = Usage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut model = model.to_owned();

    for event in events {
        match event {
            StreamEvent::Start {
                model: announced,
                usage: u,
            } => {
                if !announced.is_empty() {
                    model.clone_from(announced);
                }
                usage.merge(u);
            }
            StreamEvent::UsageUpdate { usage: u } => usage.merge(u),
            StreamEvent::TextDelta { text } => {
                if let Some(OpenBlock::Text(buf)) = open.as_mut() {
                    buf.push_str(text);
                } else {
                    close_block(&mut open, &mut content);
                    open = Some(OpenBlock::Text(text.clone()));
                }
            }
            StreamEvent::ThinkingDelta { text } => {
                if let Some(OpenBlock::Thinking(buf)) = open.as_mut() {
                    buf.push_str(text);
                } else {
                    close_block(&mut open, &mut content);
                    open = Some(OpenBlock::Thinking(text.clone()));
                }
            }
            StreamEvent::ToolUseStart { id, name } => {
                close_block(&mut open, &mut content);
                open = Some(OpenBlock::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    input: String::new(),
                });
            }
            StreamEvent::ToolUseDelta { id, partial_json } => {
                if let Some(OpenBlock::Tool {
                    id: open_id, input, ..
                }) = open.as_mut()
                    && open_id == id
                {
                    input.push_str(partial_json);
                } else {
                    // A fragment for a call that is not the open one.
                    // Whole-body parses never do this; a stream could, and
                    // the fragment is still worth more attached to a call
                    // than dropped.
                    close_block(&mut open, &mut content);
                    open = Some(OpenBlock::Tool {
                        id: id.clone(),
                        name: id.clone(),
                        input: partial_json.clone(),
                    });
                }
            }
            StreamEvent::ToolUseEnd { .. } => close_block(&mut open, &mut content),
            StreamEvent::Stop { reason, usage: u } => {
                usage.merge(u);
                stop_reason = Some(*reason);
            }
            StreamEvent::Error { .. } => {}
        }
    }
    close_block(&mut open, &mut content);

    // This dialect reports `tool_use` whenever the answer carries a tool call.
    // A stream from a dialect that does not distinguish (a Responses stream
    // completes with the same event either way) arrives here as `end_turn`,
    // and a Chat Completions client rendered from that would see
    // `finish_reason: "stop"` beside its `tool_calls`. Normalise at the hub,
    // so every converter downstream sees what an Anthropic body would say.
    let has_tool_use = content.iter().any(|b| b["type"] == "tool_use");
    let stop_reason = match stop_reason {
        Some(StopReason::EndTurn) if has_tool_use => Some(StopReason::ToolUse),
        other => other,
    };

    json!({
        "id": format!("msg_{request_id}"),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason.map_or(Value::Null, |r| json!(stop_reason_str(r))),
        "stop_sequence": Value::Null,
        "usage": usage_json(&usage),
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

    /// H3. A client that asks for reasoning in levels still gets reasoning.
    #[test]
    fn a_reasoning_effort_becomes_a_thinking_budget() {
        // `openai::parse_request` deliberately leaves `thinking_budget` empty,
        // so gating on the budget alone meant a Chat Completions client's
        // `reasoning_effort: "high"` had nowhere to land: the request reached a
        // thinking model with thinking off, at frontier prices, and nothing in
        // the answer said the field had been dropped.
        let mut req = CanonicalRequest {
            model: "m".to_owned(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            max_tokens: 1024,
            stream: false,
            temperature: None,
            thinking_budget: None,
            thinking_effort: Some(Effort::High),
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
        };
        let body = render_request(&req, "claude-opus-5").expect("renders");
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(
            body["thinking"]["budget_tokens"],
            json!(Effort::High.as_budget())
        );

        // An explicit budget still wins: it is the more precise of the two.
        req.thinking_budget = Some(2048);
        let body = render_request(&req, "claude-opus-5").expect("renders");
        assert_eq!(body["thinking"]["budget_tokens"], json!(2048));
    }

    /// P5. The terminal frame carries the whole bill, not just the output half.
    #[test]
    fn the_terminal_frame_reports_the_prompt_count_too() {
        // A native Anthropic upstream reports input and cache counts in
        // `message_start`, which this renderer emits before any upstream has
        // said anything. Over a Chat Completions upstream, which reports all of
        // it at the end, an Anthropic client was told permanently that its
        // prompt cost nothing.
        let mut st = RenderState::default();

        // The order a Chat Completions upstream produces: the stream opens
        // before anything is known about the bill, and every count arrives at
        // the end. `message_start` is therefore rendered with zeroes.
        let start = render_event(
            &StreamEvent::Start {
                model: "claude-opus-5".to_owned(),
                usage: Usage::default(),
            },
            &mut st,
        )
        .expect("a message_start");
        assert!(
            start.contains(r#""input_tokens":0"#),
            "message_start goes out before the upstream has said anything: {start}"
        );

        let usage = Usage {
            input_tokens: 1200,
            output_tokens: 142,
            cache_read_tokens: 18000,
            cache_write_tokens: 300,
        };
        let _ = render_event(&StreamEvent::UsageUpdate { usage }, &mut st);
        let out = render_event(
            &StreamEvent::Stop {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
            &mut st,
        )
        .expect("a terminal frame");

        // The `message_delta` frame specifically, not the whole string: the
        // rest of `out` carries other frames, and asserting over all of it is
        // how this test first passed with the fix reverted.
        let delta = out
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(|l| serde_json::from_str::<Value>(l).expect("a frame"))
            .find(|f| f["type"] == "message_delta")
            .expect("a message_delta frame");
        assert_eq!(delta["usage"]["input_tokens"], json!(1200), "{delta}");
        assert_eq!(delta["usage"]["output_tokens"], json!(142), "{delta}");
        assert_eq!(
            delta["usage"]["cache_read_input_tokens"],
            json!(18000),
            "{delta}"
        );
        assert_eq!(
            delta["usage"]["cache_creation_input_tokens"],
            json!(300),
            "{delta}"
        );
    }

    /// P13. Only a tool block's close ends a tool call.
    #[test]
    fn a_text_block_closing_after_a_tool_call_does_not_end_it_again() {
        // Every block ends with the same `content_block_stop`, and the event
        // does not say which kind it closed. Reading "the last call opened"
        // emitted a second end for a call that had already finished, and a
        // renderer that acts on the end acted on it twice.
        let (events, _) = drive(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
        ]);
        let ends = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUseEnd { .. }))
            .count();
        assert_eq!(ends, 1, "one call opened, so one end: {events:?}");
    }

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

    #[test]
    fn tool_choice_and_stop_sequences_survive_a_round_trip() {
        // `any` here is `required` everywhere else, which is exactly why it goes
        // through a canonical form instead of being forwarded verbatim.
        let wire = serde_json::json!({
            "model": "m", "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": {"type": "any"},
            "stop_sequences": ["\n\nHuman:"],
        });
        let c = parse_request(&wire).expect("parses");
        assert_eq!(c.tool_choice, Some(ToolChoice::Required));
        assert_eq!(c.stop, vec!["\n\nHuman:".to_owned()]);

        let back = render_request(&c, "m").expect("renders");
        assert_eq!(back["tool_choice"]["type"], "any", "not `required`");
        assert_eq!(back["stop_sequences"], serde_json::json!(["\n\nHuman:"]));
    }

    #[test]
    fn a_named_tool_choice_keeps_this_dialects_spelling() {
        let c = crate::openai::parse_request(&serde_json::json!({
            "model": "m", "messages": [],
            "tool_choice": {"type": "function", "function": {"name": "read_file"}}
        }))
        .expect("parses");
        let back = render_request(&c, "m").expect("renders");
        assert_eq!(back["tool_choice"]["type"], "tool");
        assert_eq!(back["tool_choice"]["name"], "read_file");
    }

    #[test]
    fn a_response_format_this_dialect_cannot_express_is_refused_not_dropped() {
        // Messages has no structured-output field. Dropping it sends a client
        // that is about to call `JSON.parse` a paragraph of prose, and nothing
        // in the response says the constraint was removed — so the client
        // blames the model for something the gateway did.
        let c = crate::openai::parse_request(&serde_json::json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_object"},
        }))
        .expect("parses");

        let err = render_request(&c, "claude-opus-5").expect_err("must not be dropped");
        assert!(matches!(
            err,
            Error::UnsupportedField {
                field: "response_format",
                dialect: Dialect::AnthropicMessages,
            }
        ));
    }

    #[test]
    fn asking_explicitly_for_plain_text_is_not_a_failure() {
        // `{"type": "text"}` names the behaviour this dialect already has, so
        // refusing it would 400 a request that is perfectly servable.
        let c = crate::openai::parse_request(&serde_json::json!({
            "model": "m", "messages": [], "response_format": {"type": "text"},
        }))
        .expect("parses");
        assert_eq!(c.response_format, Some(ResponseFormat::Text));
        let back = render_request(&c, "m").expect("plain text is expressible");
        assert!(back.get("response_format").is_none());
    }

    #[test]
    fn a_stored_response_id_this_dialect_cannot_express_is_refused() {
        let c = crate::responses::parse_request(&serde_json::json!({
            "model": "gpt-5", "input": "go on", "previous_response_id": "resp_abc",
        }))
        .expect("parses");
        let err = render_request(&c, "claude-opus-5").expect_err("must not be dropped");
        assert!(matches!(
            err,
            Error::UnsupportedField {
                field: "previous_response_id",
                ..
            }
        ));
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
    fn interleaved_parallel_tool_calls_keep_their_own_blocks() {
        // The shape a Chat Completions upstream streams for parallel calls,
        // and the one `openai::tests::parallel_tool_calls_are_addressed_by_index`
        // declares real: two calls open, then their fragments alternate. A
        // renderer with one "open block" slot wrote every fragment into
        // whichever call opened last, so one call reached the client as
        // `input: {}` and the other with both calls' arguments run together.
        let mut st = RenderState::new("req1", "m");
        let events = [
            StreamEvent::ToolUseStart {
                id: "call_a".to_owned(),
                name: "read".to_owned(),
            },
            StreamEvent::ToolUseStart {
                id: "call_b".to_owned(),
                name: "write".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_a".to_owned(),
                partial_json: "{\"p\":".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_b".to_owned(),
                partial_json: "{\"q\":".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_a".to_owned(),
                partial_json: "1}".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_b".to_owned(),
                partial_json: "2}".to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call_a".to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call_b".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ];
        let raw: String = events
            .iter()
            .filter_map(|e| render_event(e, &mut st))
            .collect();

        // Reassemble each block's input from the wire, by index, exactly as
        // an SDK does — and then check that block's index maps to the call
        // its `content_block_start` announced.
        let mut id_at = std::collections::BTreeMap::<u64, String>::new();
        let mut input_at = std::collections::BTreeMap::<u64, String>::new();
        let mut stops = Vec::new();
        for line in raw.lines() {
            let Some(d) = line.strip_prefix("data: ") else {
                continue;
            };
            let v: Value = serde_json::from_str(d).expect("valid json");
            let index = v["index"].as_u64().unwrap_or(u64::MAX);
            match v["type"].as_str() {
                Some("content_block_start") => {
                    id_at.insert(index, v["content_block"]["id"].as_str().unwrap().to_owned());
                }
                Some("content_block_delta") => {
                    input_at
                        .entry(index)
                        .or_default()
                        .push_str(v["delta"]["partial_json"].as_str().unwrap());
                }
                Some("content_block_stop") => stops.push(index),
                _ => {}
            }
        }
        assert_eq!(id_at[&0], "call_a");
        assert_eq!(id_at[&1], "call_b");
        assert_eq!(input_at[&0], r#"{"p":1}"#, "{raw}");
        assert_eq!(input_at[&1], r#"{"q":2}"#, "{raw}");
        assert_eq!(stops, [0, 1], "each block stops once, on its own end");
        assert!(st.open_tools.is_empty());
    }

    #[test]
    fn text_after_tool_calls_closes_every_open_call_first() {
        // Two calls open with no end in sight — a Chat Completions upstream
        // ends every call at its terminal chunk — and then text arrives. Both
        // must stop before the text block opens, or the client attaches the
        // text to a call.
        let (names, text) = render_all(&[
            StreamEvent::ToolUseStart {
                id: "call_a".to_owned(),
                name: "a".to_owned(),
            },
            StreamEvent::ToolUseStart {
                id: "call_b".to_owned(),
                name: "b".to_owned(),
            },
            StreamEvent::TextDelta {
                text: "done".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ]);
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_start",
                "content_block_stop",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(text, "done");
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
    fn a_response_round_trips_through_events_and_back() {
        // The property the hub rests on: `parse_response` and
        // `render_from_events` are inverses. Whatever a body says, reading it
        // into events and rendering those back must say the same thing — text,
        // thinking, a whole tool call, the stop reason, and every usage count.
        let body = serde_json::json!({
            "id": "msg_orig",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4",
            "content": [
                {"type": "thinking", "thinking": "let me look"},
                {"type": "text", "text": "Reading it now."},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file",
                 "input": {"path": "a.rs", "lines": [1, 2]}},
            ],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 1200, "output_tokens": 142,
                      "cache_read_input_tokens": 18000, "cache_creation_input_tokens": 7}
        });

        let events = parse_response(&body);
        let out = render_from_events(&events, "req1", "fallback-model");

        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["id"], "msg_req1");
        assert_eq!(out["model"], "fallback-model", "a body announces no model");
        assert_eq!(out["content"], body["content"], "blocks survive whole");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"], body["usage"]);

        // And the events themselves are stable across a second pass.
        assert_eq!(parse_response(&out), events);
    }

    #[test]
    fn fragmented_events_assemble_into_whole_blocks() {
        // The streamed shape: text in runs, tool arguments in pieces, usage in
        // the opening and the close. One block per run, arguments parsed.
        let events = [
            StreamEvent::Start {
                model: "served-model".to_owned(),
                usage: Usage {
                    input_tokens: 10,
                    ..Usage::default()
                },
            },
            StreamEvent::TextDelta {
                text: "Hel".to_owned(),
            },
            StreamEvent::TextDelta {
                text: "lo".to_owned(),
            },
            StreamEvent::ToolUseStart {
                id: "call_1".to_owned(),
                name: "f".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_1".to_owned(),
                partial_json: "{\"a\":".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_1".to_owned(),
                partial_json: "1}".to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call_1".to_owned(),
            },
            StreamEvent::TextDelta {
                text: "done".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                usage: Usage {
                    output_tokens: 5,
                    ..Usage::default()
                },
            },
        ];
        let out = render_from_events(&events, "r", "requested-model");

        assert_eq!(
            out["model"], "served-model",
            "the stream's announcement wins"
        );
        assert_eq!(out["content"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            out["content"][0],
            serde_json::json!({"type": "text", "text": "Hello"})
        );
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["content"][1]["name"], "f");
        assert_eq!(out["content"][1]["input"]["a"], 1);
        assert_eq!(
            out["content"][2],
            serde_json::json!({"type": "text", "text": "done"})
        );
        // The stream said `end_turn`; the answer carries a tool call. This
        // dialect reports `tool_use` for that, and so does the hub — a Chat
        // Completions client rendered from an `end_turn` here would have seen
        // `finish_reason: "stop"` beside its `tool_calls`.
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 5);
    }

    #[test]
    fn an_empty_event_list_renders_an_empty_message_not_a_crash() {
        let out = render_from_events(&[], "r", "m");
        assert_eq!(out["content"], serde_json::json!([]));
        assert!(out["stop_reason"].is_null());
    }

    #[test]
    fn a_tool_result_of_blocks_round_trips_as_blocks() {
        // The shape a browser or screenshot tool returns: text beside an
        // image, inside the result. Before, the array was stringified into
        // one text block — the image's base64 became prompt text the model
        // could not see, billed as text at ~150× the image's real cost. Now
        // blocks in are blocks out, byte for byte, and a string stays a
        // string.
        let blocks = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": [
                {"type": "text", "text": "screenshot attached"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}},
            ],
            "is_error": false,
        });
        let parsed = parse_block(&blocks).expect("parses");
        assert!(matches!(
            &parsed,
            ContentBlock::ToolResult { content: ToolResultContent::Blocks(b), .. } if b.len() == 2
        ));
        assert_eq!(render_block(&parsed), blocks);

        let plain = serde_json::json!({
            "type": "tool_result", "tool_use_id": "toolu_2", "content": "just text", "is_error": true,
        });
        let parsed = parse_block(&plain).expect("parses");
        assert!(matches!(
            &parsed,
            ContentBlock::ToolResult { content: ToolResultContent::Text(t), is_error: true, .. } if t == "just text"
        ));
        assert_eq!(render_block(&parsed), plain);
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
            thinking_effort: None,
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
        };
        let out = render_request(&req, "m").expect("renders");
        assert_eq!(out["messages"][0]["content"][0]["signature"], sig);
    }
}
