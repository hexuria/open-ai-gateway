//! The OpenAI Responses dialect (`POST /v1/responses`).
//!
//! OpenAI's newer surface, and the one their current SDKs default to — so a
//! gateway that does not speak it is unusable by a growing share of clients
//! however well it speaks Chat Completions.
//!
//! Where it differs from Chat Completions, which is nearly everywhere:
//!
//! - The system prompt is `instructions`, a top-level string, not a message.
//! - Messages are `input`, and the array is heterogeneous: message items sit
//!   alongside `function_call` and `function_call_output` items as siblings,
//!   rather than tool results being a message role.
//! - Content parts are typed by direction — `input_text` going up,
//!   `output_text` coming back — so a round trip has to flip them.
//! - Tools are flat (`{"type":"function","name":...}`), not nested under a
//!   `function` key.
//! - It is `max_output_tokens`, not `max_tokens`.
//! - Streaming events are *named* (`response.output_text.delta`) and carry a
//!   bare string delta, rather than a chunk with a choices array.
//! - Usage is `input_tokens`/`output_tokens`, and the input count includes the
//!   cached prefix.
//! - Structured output is `text.format`, nested, rather than a top-level
//!   `response_format`.
//! - There is no `stop`. It is the one carried request field this dialect has
//!   no home for, so a client that set stop sequences cannot be served here.
//! - Conversations continue by `previous_response_id`, which no other dialect
//!   has, because no other dialect stores the turn.

use crate::canonical::{
    CanonicalRequest, ContentBlock, Message, ResponseFormat, Role, Tool, ToolChoice,
};
use crate::stream::{StopReason, StreamAccumulator, StreamEvent};
use oag_core::provider::Dialect;
use oag_core::{Error, Result};
use oag_router::Usage;
use serde_json::{Value, json};

const DIALECT: Dialect = Dialect::OpenAIResponses;

/// Canonical → Responses wire JSON.
pub fn render_request(req: &CanonicalRequest, upstream_model: &str) -> Result<Value> {
    let mut input = Vec::new();
    for m in &req.messages {
        render_message_into(m, &mut input);
    }

    let mut body = json!({
        "model": upstream_model,
        "input": input,
        "max_output_tokens": req.max_tokens,
        "stream": req.stream,
    });

    if !req.system.is_empty() {
        body["instructions"] = json!(
            req.system
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        );
    }

    if !req.tools.is_empty() {
        // Flat, not nested under a `function` key.
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    })
                })
                .collect(),
        );
    }

    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if req.thinking_budget.is_some() {
        // No token budget in this dialect; effort is the closest thing it has.
        body["reasoning"] = json!({ "effort": "medium" });
    }

    // The one field this dialect cannot take. Chat Completions has `stop`,
    // Anthropic has `stop_sequences`, Gemini has `stopSequences`; Responses
    // dropped the concept, so a request that relies on it has to be refused
    // rather than silently allowed to run past its stopping point.
    if !req.stop.is_empty() {
        return Err(Error::UnsupportedField {
            field: "stop",
            dialect: DIALECT,
        });
    }

    if let Some(choice) = &req.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::Required => json!("required"),
            ToolChoice::None => json!("none"),
            // Flat, unlike Chat Completions' nested `function` wrapper.
            ToolChoice::Tool { name } => json!({ "type": "function", "name": name }),
        };
    }
    if let Some(format) = &req.response_format {
        // Nested under `text`, and the schema fields are flat inside it rather
        // than wrapped in a `json_schema` object as in Chat Completions.
        body["text"] = json!({ "format": match format {
            ResponseFormat::Text => json!({ "type": "text" }),
            ResponseFormat::JsonObject => json!({ "type": "json_object" }),
            ResponseFormat::JsonSchema { name, schema, strict } => json!({
                "type": "json_schema", "name": name, "schema": schema, "strict": strict,
            }),
        }});
    }
    if let Some(id) = &req.previous_response_id {
        body["previous_response_id"] = json!(id);
    }
    Ok(body)
}

/// One canonical message becomes one or more `input` items.
fn render_message_into(m: &Message, out: &mut Vec<Value>) {
    // Tool results are their own top-level items, not part of a message.
    for block in &m.content {
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } = block
        {
            out.push(json!({
                "type": "function_call_output",
                "call_id": tool_use_id,
                "output": content,
            }));
        }
    }

    let assistant = m.role == Role::Assistant;
    let mut parts = Vec::new();
    for block in &m.content {
        match block {
            ContentBlock::Text { text, .. } => parts.push(json!({
                // Typed by direction: what we send up is input, what came back
                // was output. Getting this backwards is rejected.
                "type": if assistant { "output_text" } else { "input_text" },
                "text": text,
            })),
            ContentBlock::Image { media_type, data } => parts.push(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            })),
            _ => {}
        }
    }

    if !parts.is_empty() {
        out.push(json!({
            "type": "message",
            "role": if assistant { "assistant" } else { "user" },
            "content": parts,
        }));
    }

    // Tool calls, likewise their own items.
    for block in &m.content {
        if let ContentBlock::ToolUse { id, name, input } = block {
            out.push(json!({
                "type": "function_call",
                "call_id": id,
                "name": name,
                // A JSON string here, as in Chat Completions.
                "arguments": input.to_string(),
            }));
        }
    }
}

/// Responses wire JSON → canonical.
pub fn parse_request(body: &Value) -> Result<CanonicalRequest> {
    let mut system = Vec::new();
    if let Some(text) = body["instructions"].as_str().filter(|s| !s.is_empty()) {
        system.push(ContentBlock::Text {
            text: text.to_owned(),
            cache_control: None,
        });
    }

    let mut messages: Vec<Message> = Vec::new();
    match &body["input"] {
        // The shorthand: a bare string is one user turn.
        Value::String(s) => messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: s.clone(),
                cache_control: None,
            }],
        }),
        Value::Array(items) => {
            for item in items {
                parse_input_item(item, &mut messages);
            }
        }
        _ => {}
    }

    let tools = body["tools"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    Some(Tool {
                        name: t["name"].as_str()?.to_owned(),
                        description: t["description"].as_str().unwrap_or_default().to_owned(),
                        input_schema: t["parameters"].clone(),
                        cache_control: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(CanonicalRequest {
        model: body["model"].as_str().unwrap_or_default().to_owned(),
        system,
        messages,
        tools,
        max_tokens: body["max_output_tokens"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(4096),
        stream: body["stream"].as_bool().unwrap_or(false),
        #[allow(clippy::cast_possible_truncation)]
        temperature: body["temperature"].as_f64().map(|t| t as f32),
        // Effort rather than a token budget; map any effort to a nominal one so
        // the classifier sees that reasoning was asked for.
        thinking_budget: body["reasoning"]["effort"].as_str().map(|_| 4096),
        client_session: body["user"].as_str().map(std::borrow::ToOwned::to_owned),
        tool_choice: parse_tool_choice(&body["tool_choice"]),
        response_format: parse_response_format(&body["text"]["format"]),
        // No such field in this dialect.
        stop: Vec::new(),
        previous_response_id: body["previous_response_id"]
            .as_str()
            .map(std::borrow::ToOwned::to_owned),
    })
}

fn parse_tool_choice(v: &Value) -> Option<ToolChoice> {
    if let Some(s) = v.as_str() {
        return match s {
            "auto" => Some(ToolChoice::Auto),
            "required" => Some(ToolChoice::Required),
            "none" => Some(ToolChoice::None),
            _ => None,
        };
    }
    Some(ToolChoice::Tool {
        name: v["name"].as_str()?.to_owned(),
    })
}

fn parse_response_format(v: &Value) -> Option<ResponseFormat> {
    match v["type"].as_str()? {
        "text" => Some(ResponseFormat::Text),
        "json_object" => Some(ResponseFormat::JsonObject),
        // Flat here: no `json_schema` wrapper around the name and schema.
        "json_schema" => Some(ResponseFormat::JsonSchema {
            name: v["name"].as_str().unwrap_or("response").to_owned(),
            schema: v["schema"].clone(),
            strict: v["strict"].as_bool().unwrap_or(false),
        }),
        _ => None,
    }
}

/// Fold one `input` item into the canonical message list.
fn parse_input_item(item: &Value, messages: &mut Vec<Message>) {
    match item["type"].as_str().unwrap_or("message") {
        "function_call" => {
            let block = ContentBlock::ToolUse {
                id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                name: item["name"].as_str().unwrap_or_default().to_owned(),
                input: item["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or_else(|| json!({})),
            };
            // Joins the assistant turn it belongs to, if there is one.
            match messages.last_mut() {
                Some(last) if last.role == Role::Assistant => last.content.push(block),
                _ => messages.push(Message {
                    role: Role::Assistant,
                    content: vec![block],
                }),
            }
        }

        "function_call_output" => {
            let block = ContentBlock::ToolResult {
                tool_use_id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                content: match &item["output"] {
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
        }

        // Reasoning items are opaque and cannot be replayed; dropping is the
        // honest lossy choice.
        "reasoning" => {}

        _ => {
            let assistant = item["role"].as_str() == Some("assistant");
            let mut content = Vec::new();
            match &item["content"] {
                Value::String(s) if !s.is_empty() => content.push(ContentBlock::Text {
                    text: s.clone(),
                    cache_control: None,
                }),
                Value::Array(parts) => {
                    for p in parts {
                        match p["type"].as_str().unwrap_or_default() {
                            // Both directions, because a conversation replayed
                            // to us contains items we previously sent back.
                            "input_text" | "output_text" | "text" => {
                                content.push(ContentBlock::Text {
                                    text: p["text"].as_str().unwrap_or_default().to_owned(),
                                    cache_control: None,
                                });
                            }
                            "input_image" => {
                                if let Some((media_type, data)) =
                                    split_data_url(p["image_url"].as_str().unwrap_or_default())
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
            if !content.is_empty() {
                messages.push(Message {
                    role: if assistant {
                        Role::Assistant
                    } else {
                        Role::User
                    },
                    content,
                });
            }
        }
    }
}

fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some((media_type.to_owned(), data.to_owned()))
}

/// One SSE `data:` payload → canonical events.
pub fn parse_event(payload: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
    let v: Value = serde_json::from_str(payload)?;
    let kind = v["type"].as_str().unwrap_or_default();

    Ok(match kind {
        "response.created" => vec![StreamEvent::Start {
            model: v["response"]["model"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            usage: parse_usage(&v["response"]["usage"]),
        }],

        // The delta is a bare string here, not an object.
        "response.output_text.delta" => vec![StreamEvent::TextDelta {
            text: v["delta"].as_str().unwrap_or_default().to_owned(),
        }],

        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            vec![StreamEvent::ThinkingDelta {
                text: v["delta"].as_str().unwrap_or_default().to_owned(),
            }]
        }

        // A tool call announces itself when its output item is added.
        "response.output_item.added" => {
            let item = &v["item"];
            if item["type"].as_str() == Some("function_call") {
                vec![StreamEvent::ToolUseStart {
                    id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                    name: item["name"].as_str().unwrap_or_default().to_owned(),
                }]
            } else {
                vec![]
            }
        }

        // Deltas are addressed by *item* id, but the call is identified by its
        // `call_id` — and only the opening event carries both. Keying on the
        // item id would leave every buffer empty, so the arguments never
        // reassemble and every tool call looks malformed.
        "response.function_call_arguments.delta" => vec![StreamEvent::ToolUseDelta {
            id: acc.current_tool_id().unwrap_or_default(),
            partial_json: v["delta"].as_str().unwrap_or_default().to_owned(),
        }],

        "response.output_item.done" => {
            let item = &v["item"];
            if item["type"].as_str() == Some("function_call") {
                vec![StreamEvent::ToolUseEnd {
                    // `call_id` when present, else whichever call is open —
                    // the `done` event does not always echo it back.
                    id: item["call_id"]
                        .as_str()
                        .map(std::borrow::ToOwned::to_owned)
                        .or_else(|| acc.current_tool_id())
                        .unwrap_or_default(),
                }]
            } else {
                vec![]
            }
        }

        "response.completed" | "response.incomplete" => {
            let response = &v["response"];
            let usage = parse_usage(&response["usage"]);
            let reason = match response["incomplete_details"]["reason"].as_str() {
                Some("max_output_tokens") => StopReason::MaxTokens,
                Some("content_filter") => StopReason::Refusal,
                _ => {
                    // A response whose only output is a refusal part.
                    if response["output"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(has_refusal))
                    {
                        StopReason::Refusal
                    } else {
                        StopReason::EndTurn
                    }
                }
            };
            vec![
                StreamEvent::UsageUpdate { usage },
                StreamEvent::Stop { reason, usage },
            ]
        }

        "response.failed" | "error" => vec![StreamEvent::Error {
            message: v["response"]["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .unwrap_or("upstream error")
                .to_owned(),
        }],

        // Every other lifecycle event carries nothing the canonical form needs.
        _ => vec![],
    })
}

fn has_refusal(item: &Value) -> bool {
    item["content"]
        .as_array()
        .is_some_and(|parts| parts.iter().any(|p| p["type"].as_str() == Some("refusal")))
}

fn parse_usage(v: &Value) -> Usage {
    let cached = v["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    Usage {
        // `input_tokens` includes the cached prefix, as in Chat Completions.
        input_tokens: v["input_tokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(cached),
        output_tokens: v["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
    }
}

// ── rendering canonical events back into this dialect ─────────────────────────

/// State carried while rendering canonical events as Responses SSE.
///
/// The most bookkeeping of the four dialects. Output is a list of *items*, each
/// with its own index, and a message item contains *content parts* with their
/// own indices — and both have explicit `added`/`done` events. A client that
/// receives a delta for an item it was never told about drops it, so the
/// lifecycle has to be emitted in order and closed before the next item opens.
#[derive(Debug, Clone, Default)]
pub struct RenderState {
    id: String,
    model: String,
    created: bool,
    /// The next free output-item index.
    next_index: usize,
    /// The open message item, if any: its index and id.
    message: Option<(usize, String)>,
    /// Text accumulated in the open message, needed for its `done` events.
    text: String,
    /// The open reasoning item, if any: its index and id.
    ///
    /// Reasoning is an output item of its own, with the same added/done
    /// lifecycle as a message. Emitting a summary delta for an item the
    /// client was never told about is how AI-SDK aborts with
    /// "reasoning part `rs_0:0` not found".
    reasoning: Option<(usize, String)>,
    /// Text accumulated in the open reasoning item, for its `done` frames.
    reasoning_text: String,
    /// Open tool calls, in the order they were opened.
    tools: Vec<ToolCall>,
    usage: Usage,
    finished: bool,
}

/// An open `function_call` output item.
///
/// Named fields rather than a tuple: four of the five are strings, and the
/// positional form is what let the function name go missing — there was no
/// obvious slot for it, so the closing frame shipped a literal `""` instead.
#[derive(Debug, Clone)]
struct ToolCall {
    call_id: String,
    /// This item's index in the output list.
    index: usize,
    item_id: String,
    /// Kept from the opening event because this dialect repeats the function
    /// name on the item's `done` frame, and that frame is where clients read it
    /// from — an empty one there leaves them with a call they cannot dispatch.
    name: String,
    /// Arguments accumulated across this call's deltas.
    args: String,
}

impl RenderState {
    #[must_use]
    pub fn new(request_id: &str, model: &str) -> Self {
        Self {
            id: format!("resp_{request_id}"),
            model: model.to_owned(),
            ..Self::default()
        }
    }

    fn frame(name: &str, body: &Value) -> String {
        // Named events: this dialect's clients dispatch on the `event:` line.
        format!("event: {name}\ndata: {body}\n\n")
    }

    /// Item id unique across responses, not just within one.
    ///
    /// `self.id` is `resp_{request_id}`. A client that keys transcript
    /// messages by item id otherwise treats every new reply as an edit to
    /// `msg_1` from the first turn.
    fn item_id(&self, kind: &str, index: usize) -> String {
        let rest = self.id.strip_prefix("resp_").unwrap_or(&self.id);
        format!("{kind}_{rest}_{index}")
    }

    fn ensure_created(&mut self, out: &mut String) {
        if self.created {
            return;
        }
        self.created = true;
        out.push_str(&Self::frame(
            "response.created",
            &json!({
                "type": "response.created",
                "response": {
                    "id": self.id,
                    "object": "response",
                    "created_at": 0,
                    "status": "in_progress",
                    "model": self.model,
                    "output": [],
                }
            }),
        ));
    }

    /// Open a reasoning item and its first summary part.
    fn open_reasoning(&mut self, out: &mut String) -> (usize, String) {
        if let Some(open) = self.reasoning.clone() {
            return open;
        }
        self.close_message(out);
        let index = self.next_index;
        self.next_index += 1;
        let item_id = self.item_id("rs", index);

        out.push_str(&Self::frame(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {
                    "id": item_id,
                    "type": "reasoning",
                    "summary": [],
                }
            }),
        ));
        out.push_str(&Self::frame(
            "response.reasoning_summary_part.added",
            &json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": "" },
            }),
        ));

        self.reasoning = Some((index, item_id.clone()));
        (index, item_id)
    }

    /// Close the open reasoning item, if there is one.
    fn close_reasoning(&mut self, out: &mut String) {
        let Some((index, item_id)) = self.reasoning.take() else {
            return;
        };
        let text = std::mem::take(&mut self.reasoning_text);

        out.push_str(&Self::frame(
            "response.reasoning_summary_text.done",
            &json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "text": text,
            }),
        ));
        out.push_str(&Self::frame(
            "response.reasoning_summary_part.done",
            &json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": text },
            }),
        ));
        out.push_str(&Self::frame(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": {
                    "id": item_id,
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": text }],
                }
            }),
        ));
    }

    /// Open a message item and its first content part.
    fn open_message(&mut self, out: &mut String) -> (usize, String) {
        if let Some(open) = self.message.clone() {
            return open;
        }
        // One item at a time: reasoning must close before the message opens.
        self.close_reasoning(out);
        let index = self.next_index;
        self.next_index += 1;
        let item_id = self.item_id("msg", index);

        out.push_str(&Self::frame(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {
                    "id": item_id, "type": "message", "status": "in_progress",
                    "role": "assistant", "content": [],
                }
            }),
        ));
        out.push_str(&Self::frame(
            "response.content_part.added",
            &json!({
                "type": "response.content_part.added",
                "item_id": item_id, "output_index": index, "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] }
            }),
        ));

        self.message = Some((index, item_id.clone()));
        (index, item_id)
    }

    /// Close the open message item, if there is one.
    fn close_message(&mut self, out: &mut String) {
        let Some((index, item_id)) = self.message.take() else {
            return;
        };
        let text = std::mem::take(&mut self.text);

        out.push_str(&Self::frame(
            "response.output_text.done",
            &json!({
                "type": "response.output_text.done",
                "item_id": item_id, "output_index": index,
                "content_index": 0, "text": text,
            }),
        ));
        out.push_str(&Self::frame(
            "response.content_part.done",
            &json!({
                "type": "response.content_part.done",
                "item_id": item_id, "output_index": index, "content_index": 0,
                "part": { "type": "output_text", "text": text, "annotations": [] }
            }),
        ));
        out.push_str(&Self::frame(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": {
                    "id": item_id, "type": "message", "status": "completed",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                }
            }),
        ));
    }
}

/// One canonical event → Responses SSE frames, if it produces any.
pub fn render_event(event: &StreamEvent, st: &mut RenderState) -> Option<String> {
    let mut out = String::new();

    match event {
        StreamEvent::Start { model, usage } => {
            crate::stream::adopt_model(&mut st.model, model);
            st.usage.merge(usage);
            st.ensure_created(&mut out);
        }

        StreamEvent::TextDelta { text } => {
            st.ensure_created(&mut out);
            let (index, item_id) = st.open_message(&mut out);
            st.text.push_str(text);
            out.push_str(&RenderState::frame(
                "response.output_text.delta",
                &json!({
                    "type": "response.output_text.delta",
                    "item_id": item_id, "output_index": index,
                    "content_index": 0, "delta": text,
                }),
            ));
        }

        StreamEvent::ThinkingDelta { text } => {
            st.ensure_created(&mut out);
            let (index, item_id) = st.open_reasoning(&mut out);
            st.reasoning_text.push_str(text);
            out.push_str(&RenderState::frame(
                "response.reasoning_summary_text.delta",
                &json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": item_id, "output_index": index,
                    "summary_index": 0, "delta": text,
                }),
            ));
        }

        StreamEvent::ToolUseStart { id, name } => render_tool_start(st, &mut out, id, name),
        StreamEvent::ToolUseDelta { id, partial_json } => {
            render_tool_delta(st, &mut out, id, partial_json)?;
        }
        StreamEvent::ToolUseEnd { id } => render_tool_end(st, &mut out, id)?,

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
            st.ensure_created(&mut out);
            st.close_reasoning(&mut out);
            st.close_message(&mut out);

            let incomplete = matches!(reason, StopReason::MaxTokens | StopReason::Refusal);
            let name = if incomplete {
                "response.incomplete"
            } else {
                "response.completed"
            };
            let mut response = json!({
                "id": st.id,
                "object": "response",
                "created_at": 0,
                "status": if incomplete { "incomplete" } else { "completed" },
                "model": st.model,
                "usage": usage_json(&st.usage),
            });
            if incomplete {
                response["incomplete_details"] = json!({
                    "reason": match reason {
                        StopReason::MaxTokens => "max_output_tokens",
                        _ => "content_filter",
                    }
                });
            }
            out.push_str(&RenderState::frame(
                name,
                &json!({ "type": name, "response": response }),
            ));
        }

        StreamEvent::Error { message } => render_failure(st, &mut out, message)?,
    }

    (!out.is_empty()).then_some(out)
}

/// Terminate the response as failed.
///
/// `response.failed`, not a bare `error` event: a client in this dialect waits
/// for a terminal `response.*` frame and an `error` event is not one, so an SDK
/// that dispatches on the response lifecycle keeps waiting after it — which is
/// the stall this is meant to end.
fn render_failure(st: &mut RenderState, out: &mut String, message: &str) -> Option<()> {
    if st.finished {
        return None;
    }
    st.finished = true;
    st.ensure_created(out);
    st.close_reasoning(out);
    st.close_message(out);
    out.push_str(&RenderState::frame(
        "response.failed",
        &json!({
            "type": "response.failed",
            "response": {
                "id": st.id,
                "object": "response",
                "created_at": 0,
                "status": "failed",
                "model": st.model,
                "error": { "code": "server_error", "message": message },
                "usage": usage_json(&st.usage),
            }
        }),
    ));
    Some(())
}

/// Open a `function_call` item, closing any open message item first.
fn render_tool_start(st: &mut RenderState, out: &mut String, id: &str, name: &str) {
    st.ensure_created(out);
    // One item at a time: an open reasoning or message item must close
    // before the next opens.
    st.close_reasoning(out);
    st.close_message(out);

    let index = st.next_index;
    st.next_index += 1;
    let item_id = st.item_id("fc", index);
    st.tools.push(ToolCall {
        call_id: id.to_owned(),
        index,
        item_id: item_id.clone(),
        name: name.to_owned(),
        args: String::new(),
    });

    out.push_str(&RenderState::frame(
        "response.output_item.added",
        &json!({
            "type": "response.output_item.added",
            "output_index": index,
            "item": {
                "id": item_id, "type": "function_call", "status": "in_progress",
                "call_id": id, "name": name, "arguments": "",
            }
        }),
    ));
}

/// Emit an argument fragment for an open tool call.
fn render_tool_delta(
    st: &mut RenderState,
    out: &mut String,
    id: &str,
    partial_json: &str,
) -> Option<()> {
    let call = st.tools.iter_mut().find(|t| t.call_id == id)?;
    call.args.push_str(partial_json);
    let (index, item_id) = (call.index, call.item_id.clone());
    out.push_str(&RenderState::frame(
        "response.function_call_arguments.delta",
        &json!({
            "type": "response.function_call_arguments.delta",
            "item_id": item_id, "output_index": index, "delta": partial_json,
        }),
    ));
    Some(())
}

/// Close a tool call, emitting its assembled arguments.
///
/// Takes the call out of the open list rather than reading it in place, so a
/// second end for the same id finds nothing and renders nothing. Upstreams do
/// send one: Anthropic closes every content block with the same event, and a
/// text block that follows a tool block reparses as an end for the tool. The
/// frames this emits carry the function name and the complete arguments —
/// everything a client needs to dispatch — so emitting them twice dispatches a
/// side-effecting tool twice.
fn render_tool_end(st: &mut RenderState, out: &mut String, id: &str) -> Option<()> {
    let pos = st.tools.iter().position(|t| t.call_id == id)?;
    let call = st.tools.remove(pos);
    out.push_str(&RenderState::frame(
        "response.function_call_arguments.done",
        &json!({
            "type": "response.function_call_arguments.done",
            "item_id": call.item_id, "output_index": call.index, "arguments": call.args,
        }),
    ));
    out.push_str(&RenderState::frame(
        "response.output_item.done",
        &json!({
            "type": "response.output_item.done",
            "output_index": call.index,
            "item": {
                "id": call.item_id, "type": "function_call", "status": "completed",
                "call_id": call.call_id, "name": call.name, "arguments": call.args,
            }
        }),
    ));
    Some(())
}

/// An Anthropic Messages response → a Responses response.
#[must_use]
pub fn render_response(anthropic: &Value, request_id: &str) -> Value {
    let mut output = Vec::new();
    let mut text = String::new();

    for block in anthropic["content"].as_array().unwrap_or(&Vec::new()) {
        match block["type"].as_str().unwrap_or_default() {
            "text" => text.push_str(block["text"].as_str().unwrap_or_default()),
            "tool_use" => output.push(json!({
                "id": format!("fc_{request_id}_{}", output.len()),
                "type": "function_call",
                "status": "completed",
                "call_id": block["id"],
                "name": block["name"],
                "arguments": block["input"].to_string(),
            })),
            _ => {}
        }
    }

    // The message item comes first when there is text, matching the order a
    // streamed response would have produced.
    if !text.is_empty() {
        output.insert(
            0,
            json!({
                "id": format!("msg_{request_id}_0"),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            }),
        );
    }

    let u = &anthropic["usage"];
    let cached = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let written = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    let input = u["input_tokens"].as_u64().unwrap_or(0) + cached + written;
    let out_tokens = u["output_tokens"].as_u64().unwrap_or(0);

    let stop = anthropic["stop_reason"].as_str().unwrap_or("end_turn");
    let incomplete = matches!(stop, "max_tokens" | "refusal");

    let mut response = json!({
        "id": format!("resp_{request_id}"),
        "object": "response",
        "created_at": 0,
        "status": if incomplete { "incomplete" } else { "completed" },
        "model": anthropic["model"],
        "output": output,
        "usage": {
            "input_tokens": input,
            "input_tokens_details": { "cached_tokens": cached },
            "output_tokens": out_tokens,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": input + out_tokens,
        }
    });
    if incomplete {
        response["incomplete_details"] = json!({
            "reason": if stop == "max_tokens" { "max_output_tokens" } else { "content_filter" }
        });
    }
    response
}

fn usage_json(u: &Usage) -> Value {
    // Every prompt-side token in `input_tokens`, so the totals add up the way
    // clients in this dialect check them.
    let input = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
    json!({
        "input_tokens": input,
        "input_tokens_details": { "cached_tokens": u.cache_read_tokens },
        "output_tokens": u.output_tokens,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": input + u.output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic streamed response, with the full item and content-part
    /// lifecycle this dialect requires.
    const STREAM: &[&str] = &[
        r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5","usage":{"input_tokens":19200,"input_tokens_details":{"cached_tokens":18000}}}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"msg_0","type":"message","role":"assistant"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"content_index":0,"delta":"Let me "}"#,
        r#"{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"content_index":0,"delta":"check."}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read_file","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{\"path\""}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":": \"a.rs\"}"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read_file"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","usage":{"input_tokens":19200,"input_tokens_details":{"cached_tokens":18000},"output_tokens":142}}}"#,
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
    fn text_reassembles_across_deltas() {
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
    fn the_cached_prefix_is_not_counted_twice() {
        // `input_tokens` includes the cached prefix here, as in Chat
        // Completions. Adding them would bill 37200 for a 19200-token prompt.
        let (_, acc) = drive(STREAM);
        let u = acc.usage();
        assert_eq!(u.cache_read_tokens, 18_000);
        assert_eq!(u.input_tokens, 1_200);
        assert_eq!(u.output_tokens, 142);
    }

    #[test]
    fn tool_arguments_reassemble_despite_the_id_switch() {
        // The trap: the opening event carries `call_id`, every delta after it
        // carries only `item_id`. Keying deltas on the item id leaves the
        // buffer empty, so the arguments never reassemble and every tool call
        // looks malformed.
        let (events, acc) = drive(STREAM);
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseDelta { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["call_1", "call_1"],
            "deltas must carry the call id"
        );
        assert_eq!(
            acc.quality_gate(),
            None,
            "and the arguments must be valid JSON"
        );
    }

    #[test]
    fn lifecycle_events_that_carry_nothing_produce_nothing() {
        // This dialect emits many; treating an unknown one as fatal would break
        // every request the day OpenAI adds another.
        let mut acc = StreamAccumulator::new();
        for noise in [
            r#"{"type":"response.in_progress"}"#,
            r#"{"type":"response.content_part.added","part":{"type":"output_text"}}"#,
            r#"{"type":"response.output_text.done","text":"x"}"#,
            r#"{"type":"something.new.in.2027"}"#,
        ] {
            assert!(
                parse_event(noise, &mut acc)
                    .expect("must not error")
                    .is_empty()
            );
        }
    }

    #[test]
    fn truncation_is_reported_as_a_distinct_stop_reason() {
        // It drives escalation, so it must not collapse into end-of-turn.
        let (events, _) = drive(&[
            r#"{"type":"response.incomplete","response":{"usage":{},"incomplete_details":{"reason":"max_output_tokens"}}}"#,
        ]);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Stop {
                reason: StopReason::MaxTokens,
                ..
            }
        )));
    }

    #[test]
    fn a_request_round_trips_through_the_canonical_form() {
        let wire = json!({
            "model": "gpt-5",
            "instructions": "You are helpful.",
            "max_output_tokens": 2048,
            "stream": true,
            "input": [
                { "type": "message", "role": "user",
                  "content": [{ "type": "input_text", "text": "read a.rs" }] },
                { "type": "function_call", "call_id": "call_1",
                  "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" },
                { "type": "function_call_output", "call_id": "call_1",
                  "output": "fn main() {}" }
            ],
            "tools": [{ "type": "function", "name": "read_file",
                        "description": "reads", "parameters": { "type": "object" } }]
        });

        let c = parse_request(&wire).expect("parses");
        assert_eq!(c.max_tokens, 2048, "max_output_tokens, not max_tokens");
        assert_eq!(c.system.len(), 1, "instructions becomes the system prompt");
        assert_eq!(c.tools.len(), 1, "tools are flat in this dialect");
        assert!(c.stream);

        let call = c
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                ContentBlock::ToolUse { input, .. } => Some(input.clone()),
                _ => None,
            });
        assert_eq!(call.expect("tool call")["path"], "a.rs");
        assert!(
            c.messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        );

        let back = render_request(&c, "gpt-5").expect("renders");
        assert_eq!(back["instructions"], "You are helpful.");
        assert_eq!(back["max_output_tokens"], 2048);
        assert!(back.get("max_tokens").is_none());
        assert_eq!(back["tools"][0]["name"], "read_file", "flat, not nested");
    }

    #[test]
    fn content_parts_are_typed_by_direction() {
        // input_text going up, output_text coming back. Getting this backwards
        // is rejected by the API.
        let c = CanonicalRequest {
            model: "m".to_owned(),
            system: vec![],
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "q".into(),
                        cache_control: None,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "a".into(),
                        cache_control: None,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            stream: false,
            temperature: None,
            thinking_budget: None,
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
        };
        let back = render_request(&c, "m").expect("renders");
        assert_eq!(back["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(back["input"][1]["content"][0]["type"], "output_text");
    }

    #[test]
    fn a_bare_string_input_is_accepted() {
        let c = parse_request(&json!({ "model": "m", "input": "hello" })).expect("parses");
        assert_eq!(c.messages.len(), 1);
        assert!(matches!(
            c.messages[0].content.first(),
            Some(ContentBlock::Text { text, .. }) if text == "hello"
        ));
    }

    #[test]
    fn previous_response_id_survives_a_round_trip() {
        // The only dialect that has it, and the field that makes a follow-up
        // turn make sense at all.
        let c = parse_request(&json!({
            "model": "gpt-5", "input": "and the second one?",
            "previous_response_id": "resp_abc",
        }))
        .expect("parses");
        assert_eq!(c.previous_response_id.as_deref(), Some("resp_abc"));

        let back = render_request(&c, "gpt-5").expect("renders");
        assert_eq!(back["previous_response_id"], "resp_abc");
    }

    #[test]
    fn a_json_schema_round_trips_through_text_format() {
        // Nested under `text` and flat inside it, where Chat Completions nests
        // the same fields one level deeper under `json_schema`.
        let schema = json!({"type": "object", "properties": {"n": {"type": "number"}}});
        let c = parse_request(&json!({
            "model": "gpt-5", "input": "hi",
            "text": {"format": {
                "type": "json_schema", "name": "answer",
                "schema": schema.clone(), "strict": true,
            }},
            "tool_choice": "required",
        }))
        .expect("parses");
        assert!(matches!(
            c.response_format,
            Some(ResponseFormat::JsonSchema { ref name, strict: true, .. }) if name == "answer"
        ));
        assert_eq!(c.tool_choice, Some(ToolChoice::Required));

        let back = render_request(&c, "gpt-5").expect("renders");
        assert_eq!(back["text"]["format"]["type"], "json_schema");
        assert_eq!(back["text"]["format"]["name"], "answer");
        assert_eq!(back["text"]["format"]["schema"], schema);
        assert!(
            back["text"]["format"].get("json_schema").is_none(),
            "that wrapper belongs to Chat Completions"
        );
        assert_eq!(back["tool_choice"], "required");
    }

    #[test]
    fn stop_sequences_are_refused_rather_than_dropped() {
        // This dialect has no `stop` at all. Dropping it lets generation run
        // straight past the point the client said to stop, and the client has
        // no way to tell that from a model that ignored a stop sequence.
        let c = crate::openai::parse_request(&json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}],
            "stop": ["\n\n"],
        }))
        .expect("parses");

        let err = render_request(&c, "gpt-5").expect_err("must not be dropped");
        assert!(matches!(
            err,
            Error::UnsupportedField {
                field: "stop",
                dialect: Dialect::OpenAIResponses,
            }
        ));
    }

    #[test]
    fn a_named_tool_choice_renders_flat() {
        let c = crate::openai::parse_request(&json!({
            "model": "m", "messages": [],
            "tool_choice": {"type": "function", "function": {"name": "read_file"}}
        }))
        .expect("parses");
        let back = render_request(&c, "gpt-5").expect("renders");
        assert_eq!(back["tool_choice"]["name"], "read_file");
        assert!(back["tool_choice"].get("function").is_none(), "flat here");
    }

    // ── rendering ────────────────────────────────────────────────────────────

    fn render_all(events: &[StreamEvent]) -> (Vec<String>, String) {
        let mut st = RenderState::new("req1", "claude-opus-5");
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
                    if v["type"] == "response.output_text.delta" {
                        text.push_str(v["delta"].as_str().unwrap_or_default());
                    }
                }
            }
        }
        (names, text)
    }

    #[test]
    fn rendering_emits_the_full_item_lifecycle_in_order() {
        // A client here drops a delta for an item it was never told about, so
        // added/done must bracket every item and part.
        let (names, text) = render_all(&[
            StreamEvent::Start {
                model: "m".to_owned(),
                usage: Usage {
                    input_tokens: 900,
                    ..Usage::default()
                },
            },
            StreamEvent::TextDelta {
                text: "Hello".to_owned(),
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
                "response.created",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(text, "Hello");
    }

    #[test]
    fn a_reasoning_item_is_opened_before_its_deltas() {
        // AI-SDK maps reasoning_summary_part.added to reasoning-start. A
        // summary delta for an item that was never added aborts the run with
        // "reasoning part rs_0:0 not found".
        let (names, _) = render_all(&[
            StreamEvent::ThinkingDelta {
                text: "hmm".to_owned(),
            },
            StreamEvent::TextDelta {
                text: "hi".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ]);
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
    }

    #[test]
    fn a_reasoning_item_identifies_itself_as_reasoning() {
        let mut st = RenderState::new("r", "m");
        let raw = render_event(
            &StreamEvent::ThinkingDelta {
                text: "hmm".to_owned(),
            },
            &mut st,
        )
        .expect("frames");
        let frames: Vec<Value> = raw
            .split("\n\n")
            .filter(|f| !f.trim().is_empty())
            .filter_map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
            .map(|d| serde_json::from_str(d).expect("valid json"))
            .collect();

        let added = frames
            .iter()
            .find(|v| v["type"] == "response.output_item.added")
            .expect("opened");
        assert_eq!(added["item"]["type"], "reasoning");
        assert_eq!(added["item"]["id"], "rs_r_0");
        assert_eq!(added["output_index"], 0);

        let part = frames
            .iter()
            .find(|v| v["type"] == "response.reasoning_summary_part.added")
            .expect("part");
        assert_eq!(part["item_id"], "rs_r_0");
        assert_eq!(part["summary_index"], 0);

        let delta = frames
            .iter()
            .find(|v| v["type"] == "response.reasoning_summary_text.delta")
            .expect("delta");
        assert_eq!(delta["item_id"], "rs_r_0");
        assert_eq!(delta["delta"], "hmm");
    }

    #[test]
    fn item_ids_do_not_repeat_across_responses() {
        // CopilotKit keys transcript messages by the item id on the wire.
        // `msg_1` every turn merges reply N into reply 1.
        let mut a = RenderState::new("aaa", "m");
        let mut b = RenderState::new("bbb", "m");
        let ea = render_event(
            &StreamEvent::TextDelta {
                text: "hi".to_owned(),
            },
            &mut a,
        )
        .expect("a");
        let eb = render_event(
            &StreamEvent::TextDelta {
                text: "hi".to_owned(),
            },
            &mut b,
        )
        .expect("b");
        assert!(ea.contains("\"id\":\"msg_aaa_0\""), "{ea}");
        assert!(eb.contains("\"id\":\"msg_bbb_0\""), "{eb}");
    }

    #[test]
    fn a_tool_call_closes_the_message_item_before_opening_its_own() {
        let (names, _) = render_all(&[
            StreamEvent::TextDelta {
                text: "thinking".to_owned(),
            },
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "f".into(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_1".into(),
                partial_json: "{}".into(),
            },
            StreamEvent::ToolUseEnd {
                id: "call_1".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]);
        let first_done = names.iter().position(|n| n == "response.output_item.done");
        let second_added = names
            .iter()
            .enumerate()
            .filter(|(_, n)| *n == "response.output_item.added")
            .nth(1)
            .map(|(i, _)| i);
        assert!(
            first_done < second_added,
            "the message item must close before the tool item opens: {names:?}"
        );
    }

    #[test]
    fn responses_output_item_done_carries_function_name() {
        // Clients in this dialect read the function name off the item's `done`
        // frame — it is the one that reports the item complete — so dropping it
        // there hands them a finished call they cannot dispatch, even though
        // the `added` frame minutes earlier had the name.
        let mut st = RenderState::new("r", "m");
        let raw: String = [
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "read_file".into(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_1".into(),
                partial_json: r#"{"path":"a.rs"}"#.into(),
            },
            StreamEvent::ToolUseEnd {
                id: "call_1".into(),
            },
        ]
        .iter()
        .filter_map(|e| render_event(e, &mut st))
        .collect();

        let items: Vec<Value> = raw
            .split("\n\n")
            .filter(|f| !f.trim().is_empty())
            .filter_map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
            .map(|d| serde_json::from_str(d).expect("valid json"))
            .filter(|v: &Value| v["type"] == "response.output_item.done")
            .collect();

        assert_eq!(items.len(), 1, "the tool item closes exactly once");
        let item = &items[0]["item"];
        assert_eq!(item["name"], "read_file");
        assert_eq!(item["call_id"], "call_1");
        assert_eq!(item["arguments"], r#"{"path":"a.rs"}"#);
        assert_eq!(item["status"], "completed");
    }

    /// An Anthropic stream that uses a tool and then keeps talking.
    ///
    /// Anthropic closes every content block with the same `content_block_stop`,
    /// and the accumulator keeps naming the last tool id after that tool has
    /// ended — so the *text* block's stop parses as a second `ToolUseEnd` for a
    /// call that is already closed.
    const TOOL_THEN_TEXT: &[&str] = &[
        r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":10}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_abc","name":"read_file","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.rs\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Done."}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
    ];

    /// Anthropic upstream events, parsed and rendered into this dialect, as the
    /// `data:` payload of each frame.
    fn anthropic_into_responses(lines: &[&str]) -> Vec<Value> {
        let mut acc = StreamAccumulator::new();
        let mut st = RenderState::new("r", "claude-opus-5");
        let mut raw = String::new();
        for line in lines {
            for e in crate::anthropic::parse_event(line, &mut acc).expect("parses") {
                acc.observe(&e);
                if let Some(frames) = render_event(&e, &mut st) {
                    raw.push_str(&frames);
                }
            }
        }
        raw.split("\n\n")
            .filter(|f| !f.trim().is_empty())
            .filter_map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
            .map(|d| serde_json::from_str(d).expect("valid json"))
            .collect()
    }

    #[test]
    fn a_closed_tool_call_is_not_closed_again_by_a_later_block_stop() {
        // A duplicate `output_item.done` is not cosmetic here: it carries the
        // function name and complete arguments, which is everything a client
        // needs to dispatch — so a second one is a second dispatch, and a tool
        // that writes something writes it twice.
        let frames = anthropic_into_responses(TOOL_THEN_TEXT);

        let closes: Vec<&Value> = frames
            .iter()
            .filter(|v| v["type"] == "response.output_item.done")
            .filter(|v| v["item"]["type"] == "function_call")
            .collect();

        assert_eq!(
            closes.len(),
            1,
            "the function-call item closes exactly once: {frames:#?}"
        );
        let item = &closes[0]["item"];
        assert_eq!(item["call_id"], "toolu_abc");
        assert_eq!(item["name"], "read_file", "and it names the function");
        assert_eq!(item["arguments"], r#"{"path":"a.rs"}"#);

        let arg_dones = frames
            .iter()
            .filter(|v| v["type"] == "response.function_call_arguments.done")
            .count();
        assert_eq!(
            arg_dones, 1,
            "the arguments are reported final exactly once: {frames:#?}"
        );

        // The text that followed must land in a message item of its own, not
        // reopen the closed call.
        assert!(
            frames
                .iter()
                .any(|v| v["type"] == "response.output_text.delta" && v["delta"] == "Done.")
        );
    }

    #[test]
    fn rendering_round_trips_through_our_own_parser() {
        let mut st = RenderState::new("r", "m");
        let raw: String = [
            StreamEvent::Start {
                model: "m".to_owned(),
                usage: Usage {
                    input_tokens: 1200,
                    cache_read_tokens: 18_000,
                    ..Usage::default()
                },
            },
            StreamEvent::TextDelta {
                text: "Hi".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                usage: Usage {
                    output_tokens: 142,
                    ..Usage::default()
                },
            },
        ]
        .iter()
        .filter_map(|e| render_event(e, &mut st))
        .collect();

        let mut acc = StreamAccumulator::new();
        let mut text = String::new();
        for frame in raw.split("\n\n").filter(|f| !f.trim().is_empty()) {
            let payload = frame
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .expect("data line");
            for e in parse_event(payload, &mut acc).expect("parses") {
                acc.observe(&e);
                if let StreamEvent::TextDelta { text: t } = &e {
                    text.push_str(t);
                }
            }
        }
        assert_eq!(text, "Hi");
        assert_eq!(acc.usage().input_tokens, 1_200);
        assert_eq!(acc.usage().cache_read_tokens, 18_000);
        assert_eq!(acc.usage().output_tokens, 142);
    }

    #[test]
    fn a_duplicate_stop_does_not_terminate_twice() {
        let mut st = RenderState::new("r", "m");
        let stop = StreamEvent::Stop {
            reason: StopReason::EndTurn,
            usage: Usage::default(),
        };
        assert!(render_event(&stop, &mut st).is_some());
        assert!(render_event(&stop, &mut st).is_none());
    }

    #[test]
    fn an_anthropic_response_renders_into_this_dialect() {
        let anthropic = json!({
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": "An answer."}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1200, "output_tokens": 42,
                      "cache_read_input_tokens": 18000}
        });
        let out = render_response(&anthropic, "req1");
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["output"][0]["content"][0]["text"], "An answer.");
        assert_eq!(out["usage"]["input_tokens"], 19_200);
        assert_eq!(
            out["usage"]["input_tokens_details"]["cached_tokens"],
            18_000
        );
        assert_eq!(
            out["usage"]["total_tokens"].as_u64(),
            Some(19_200 + 42),
            "total must equal input + output"
        );
    }

    #[test]
    fn truncation_renders_as_incomplete_with_a_reason() {
        let anthropic = json!({
            "model": "m",
            "content": [{"type": "text", "text": "partial"}],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let out = render_response(&anthropic, "r");
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");
    }
}
