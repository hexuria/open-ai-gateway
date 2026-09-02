//! The Gemini `generateContent` dialect.
//!
//! Structurally the furthest from the canonical form of the three, and each
//! difference is somewhere translation can lose something:
//!
//! - Messages are `contents` with `parts`, and the assistant role is spelled
//!   `model` rather than `assistant`.
//! - The system prompt is `systemInstruction`, a `Content` rather than a string.
//! - Tools are `functionDeclarations` nested inside a single `tools` entry, not
//!   a list of tools.
//! - A tool *call* is a `functionCall` part; a tool *result* is a
//!   `functionResponse` part, addressed by function **name** rather than by a
//!   call id — so a conversation with two concurrent calls to the same function
//!   cannot be represented faithfully. We key on name and accept the limit.
//! - Generation settings live under `generationConfig`, not at the top level.
//! - Usage is `usageMetadata`, and its prompt count *includes* the cached
//!   prefix, like Chat Completions and unlike Anthropic.

use crate::canonical::{
    CanonicalRequest, ContentBlock, Effort, Message, ResponseFormat, Role, Tool, ToolChoice,
};
use crate::stream::{StopReason, StreamAccumulator, StreamEvent};
use oag_core::provider::Dialect;
use oag_core::{Error, Result};
use oag_router::Usage;
use serde_json::{Value, json};

const DIALECT: Dialect = Dialect::GeminiGenerateContent;

/// Canonical → Gemini wire JSON.
///
/// The model name is not in the body here: it is in the URL path, which is why
/// this takes no model argument.
pub fn render_request(req: &CanonicalRequest) -> Result<Value> {
    let contents: Vec<Value> = req.messages.iter().map(render_message).collect();

    let mut body = json!({
        "contents": contents,
        "generationConfig": { "maxOutputTokens": req.max_tokens },
    });

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
        body["systemInstruction"] = json!({ "parts": [{ "text": text }] });
    }

    if !req.tools.is_empty() {
        // One `tools` entry holding every declaration, not one entry per tool.
        body["tools"] = json!([{
            "functionDeclarations": req.tools.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })).collect::<Vec<_>>()
        }]);
    }

    if let Some(t) = req.temperature {
        body["generationConfig"]["temperature"] = json!(t);
    }
    if let Some(budget) = req.thinking_budget {
        body["generationConfig"]["thinkingConfig"] = json!({ "thinkingBudget": budget });
    }

    // Nothing here corresponds to a stored response: this dialect replays the
    // whole conversation in `contents` every turn.
    if req.previous_response_id.is_some() {
        return Err(Error::UnsupportedField {
            field: "previous_response_id",
            dialect: DIALECT,
        });
    }

    if let Some(choice) = &req.tool_choice {
        // Its own top-level `toolConfig`, not part of `generationConfig`, and a
        // named tool is expressed as "call something, from this list of one".
        body["toolConfig"] = json!({ "functionCallingConfig": match choice {
            ToolChoice::Auto => json!({ "mode": "AUTO" }),
            ToolChoice::Required => json!({ "mode": "ANY" }),
            ToolChoice::None => json!({ "mode": "NONE" }),
            ToolChoice::Tool { name } => json!({
                "mode": "ANY", "allowedFunctionNames": [name],
            }),
        }});
    }
    if let Some(format) = &req.response_format {
        // A MIME type rather than a format object, with the schema alongside it.
        let cfg = &mut body["generationConfig"];
        match format {
            ResponseFormat::Text => cfg["responseMimeType"] = json!("text/plain"),
            ResponseFormat::JsonObject => cfg["responseMimeType"] = json!("application/json"),
            // The schema's label and `strict` flag have no field here, and
            // neither loses the constraint: this dialect always enforces a
            // `responseSchema`, which is at worst stricter than was asked.
            ResponseFormat::JsonSchema { schema, .. } => {
                cfg["responseMimeType"] = json!("application/json");
                cfg["responseSchema"] = schema.clone();
            }
        }
    }
    if !req.stop.is_empty() {
        body["generationConfig"]["stopSequences"] = json!(req.stop);
    }

    Ok(body)
}

fn render_message(m: &Message) -> Value {
    let parts: Vec<Value> = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(json!({ "text": text })),
            ContentBlock::Image { media_type, data } => Some(json!({
                "inlineData": { "mimeType": media_type, "data": data }
            })),
            ContentBlock::ToolUse { name, input, .. } => Some(json!({
                "functionCall": { "name": name, "args": input }
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some(json!({
                // Addressed by name in this dialect; the id is the closest
                // thing we have when the original name is not carried.
                "functionResponse": {
                    "name": tool_use_id,
                    "response": { "result": content },
                }
            })),
            // No wire representation; replaying it would be rejected.
            ContentBlock::Thinking { .. } => None,
        })
        .collect();

    json!({
        "role": if m.role == Role::Assistant { "model" } else { "user" },
        "parts": parts,
    })
}

/// Gemini wire JSON → canonical.
pub fn parse_request(body: &Value) -> Result<CanonicalRequest> {
    let system = body["systemInstruction"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .map(|t| ContentBlock::Text {
                    text: t.to_owned(),
                    cache_control: None,
                })
                .collect()
        })
        .unwrap_or_default();

    let messages = body["contents"]
        .as_array()
        .map(|arr| arr.iter().filter_map(parse_content).collect())
        .unwrap_or_default();

    let tools = body["tools"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e["functionDeclarations"].as_array())
                .flatten()
                .filter_map(|d| {
                    Some(Tool {
                        name: d["name"].as_str()?.to_owned(),
                        description: d["description"].as_str().unwrap_or_default().to_owned(),
                        input_schema: d["parameters"].clone(),
                        cache_control: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let cfg = &body["generationConfig"];

    Ok(CanonicalRequest {
        // Carried in the URL, not the body.
        model: String::new(),
        system,
        messages,
        tools,
        max_tokens: cfg["maxOutputTokens"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(4096),
        stream: false,
        #[allow(clippy::cast_possible_truncation)]
        temperature: cfg["temperature"].as_f64().map(|t| t as f32),
        thinking_budget: cfg["thinkingConfig"]["thinkingBudget"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok()),
        // This dialect speaks budgets. Carry the nearest level too, so a hop to
        // one that speaks levels does not silently drop the request to think.
        thinking_effort: cfg["thinkingConfig"]["thinkingBudget"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .map(Effort::from_budget),
        client_session: None,
        tool_choice: parse_tool_choice(&body["toolConfig"]["functionCallingConfig"]),
        response_format: parse_response_format(cfg),
        stop: cfg["stopSequences"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(std::borrow::ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        // This dialect has no stored responses; the conversation is `contents`.
        previous_response_id: None,
    })
}

fn parse_tool_choice(cfg: &Value) -> Option<ToolChoice> {
    let mode = cfg["mode"].as_str()?;
    // A single allowed name is how this dialect says "call this one", so it
    // parses back to the named form rather than to a bare `Required`.
    if let Some(name) = cfg["allowedFunctionNames"]
        .as_array()
        .filter(|names| names.len() == 1)
        .and_then(|names| names[0].as_str())
        && mode != "NONE"
    {
        return Some(ToolChoice::Tool {
            name: name.to_owned(),
        });
    }
    match mode {
        "AUTO" => Some(ToolChoice::Auto),
        "ANY" => Some(ToolChoice::Required),
        "NONE" => Some(ToolChoice::None),
        _ => None,
    }
}

fn parse_response_format(cfg: &Value) -> Option<ResponseFormat> {
    let json = cfg["responseMimeType"].as_str() == Some("application/json");
    match &cfg["responseSchema"] {
        Value::Null => {
            if json {
                Some(ResponseFormat::JsonObject)
            } else if cfg["responseMimeType"].is_string() {
                Some(ResponseFormat::Text)
            } else {
                None
            }
        }
        schema => Some(ResponseFormat::JsonSchema {
            // Unnamed on this wire; the label is an OpenAI-side detail.
            name: "response".to_owned(),
            schema: schema.clone(),
            strict: true,
        }),
    }
}

fn parse_content(v: &Value) -> Option<Message> {
    let role = if v["role"].as_str() == Some("model") {
        Role::Assistant
    } else {
        Role::User
    };

    let content: Vec<ContentBlock> = v["parts"]
        .as_array()?
        .iter()
        .filter_map(|p| {
            if let Some(text) = p["text"].as_str() {
                return Some(ContentBlock::Text {
                    text: text.to_owned(),
                    cache_control: None,
                });
            }
            if let Some(call) = p.get("functionCall") {
                return Some(ContentBlock::ToolUse {
                    // No call id in this dialect; the name is the identity.
                    id: call["name"].as_str().unwrap_or_default().to_owned(),
                    name: call["name"].as_str().unwrap_or_default().to_owned(),
                    input: call["args"].clone(),
                });
            }
            if let Some(resp) = p.get("functionResponse") {
                return Some(ContentBlock::ToolResult {
                    tool_use_id: resp["name"].as_str().unwrap_or_default().to_owned(),
                    content: resp["response"].to_string(),
                    is_error: false,
                });
            }
            if let Some(inline) = p.get("inlineData") {
                return Some(ContentBlock::Image {
                    media_type: inline["mimeType"].as_str()?.to_owned(),
                    data: inline["data"].as_str()?.to_owned(),
                });
            }
            None
        })
        .collect();

    (!content.is_empty()).then_some(Message { role, content })
}

/// One SSE `data:` payload → canonical events.
pub fn parse_event(payload: &str, _acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
    let v: Value = serde_json::from_str(payload)?;
    Ok(parse_response(&v))
}

/// A complete non-streamed `generateContent` body → canonical events.
///
/// The same reader as [`parse_event`], because in this dialect it is the same
/// shape: a whole response is one chunk's `candidates` and `usageMetadata`
/// rather than a different envelope.
#[must_use]
pub fn parse_response(v: &Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    if let Some(usage) = v.get("usageMetadata") {
        events.push(StreamEvent::UsageUpdate {
            usage: parse_usage(usage),
        });
    }

    let Some(candidate) = v["candidates"].as_array().and_then(|c| c.first()) else {
        return events;
    };

    for part in candidate["content"]["parts"]
        .as_array()
        .unwrap_or(&Vec::new())
    {
        if let Some(text) = part["text"].as_str().filter(|t| !t.is_empty()) {
            // Reasoning arrives as an ordinary text part flagged with
            // `thought`, rather than in its own channel.
            if part["thought"].as_bool().unwrap_or(false) {
                events.push(StreamEvent::ThinkingDelta {
                    text: text.to_owned(),
                });
            } else {
                events.push(StreamEvent::TextDelta {
                    text: text.to_owned(),
                });
            }
        }
        if let Some(call) = part.get("functionCall") {
            let name = call["name"].as_str().unwrap_or_default().to_owned();
            // Arrives whole, not streamed in fragments.
            events.push(StreamEvent::ToolUseStart {
                id: name.clone(),
                name: name.clone(),
            });
            events.push(StreamEvent::ToolUseDelta {
                id: name.clone(),
                partial_json: call["args"].to_string(),
            });
            events.push(StreamEvent::ToolUseEnd { id: name });
        }
    }

    if let Some(reason) = candidate["finishReason"].as_str().filter(|r| !r.is_empty()) {
        events.push(StreamEvent::Stop {
            reason: match reason {
                "MAX_TOKENS" => StopReason::MaxTokens,
                "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" => StopReason::Refusal,
                _ => StopReason::EndTurn,
            },
            usage: v.get("usageMetadata").map(parse_usage).unwrap_or_default(),
        });
    }

    events
}

// ── rendering canonical events back into this dialect ─────────────────────────

/// State carried while rendering canonical events as Gemini SSE.
///
/// Less bookkeeping than the other two: this dialect has no block lifecycle and
/// no opening frame, so a chunk is self-contained. What it does need is the
/// accumulated usage, since `usageMetadata` reports running totals rather than
/// deltas.
#[derive(Debug, Clone, Default)]
pub struct RenderState {
    usage: Usage,
    finished: bool,
}

impl RenderState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn chunk(&self, parts: &[Value], finish: Option<&str>) -> String {
        let mut candidate = json!({
            "content": { "role": "model", "parts": parts },
            "index": 0,
        });
        if let Some(f) = finish {
            candidate["finishReason"] = json!(f);
        }
        let body = json!({
            "candidates": [candidate],
            "usageMetadata": usage_json(&self.usage),
        });
        format!("data: {body}\n\n")
    }
}

/// One canonical event → a Gemini SSE frame, if it produces one.
pub fn render_event(event: &StreamEvent, st: &mut RenderState) -> Option<String> {
    match event {
        // Neither has a frame in this dialect: usage rides on every chunk as a
        // running total, so both just fold in and emit nothing.
        StreamEvent::Start { usage, .. } | StreamEvent::UsageUpdate { usage } => {
            st.usage.merge(usage);
            None
        }

        StreamEvent::TextDelta { text } => Some(st.chunk(&[json!({ "text": text })], None)),

        // Reasoning is an ordinary text part carrying `thought`.
        StreamEvent::ThinkingDelta { text } => {
            Some(st.chunk(&[json!({ "text": text, "thought": true })], None))
        }

        // This dialect delivers a call whole, so the opening event alone has
        // nothing to say; the arguments arrive with the delta.
        StreamEvent::ToolUseStart { .. } | StreamEvent::ToolUseEnd { .. } => None,

        StreamEvent::ToolUseDelta { id, partial_json } => {
            let args: Value = serde_json::from_str(partial_json).unwrap_or_else(|_| json!({}));
            Some(st.chunk(
                &[json!({ "functionCall": { "name": id, "args": args } })],
                None,
            ))
        }

        StreamEvent::Stop { reason, usage } => {
            if st.finished {
                return None;
            }
            st.finished = true;
            st.usage.merge(usage);
            Some(st.chunk(
                &[],
                Some(match reason {
                    StopReason::MaxTokens => "MAX_TOKENS",
                    StopReason::Refusal => "SAFETY",
                    _ => "STOP",
                }),
            ))
        }

        StreamEvent::Error { message } => Some(format!(
            "data: {}\n\n",
            json!({ "error": { "code": 500, "message": message, "status": "INTERNAL" } })
        )),
    }
}

/// An Anthropic Messages response → a Gemini `generateContent` response.
#[must_use]
pub fn render_message_response(anthropic: &Value) -> Value {
    let mut parts = Vec::new();
    for block in anthropic["content"].as_array().unwrap_or(&Vec::new()) {
        match block["type"].as_str().unwrap_or_default() {
            "text" => parts.push(json!({ "text": block["text"] })),
            "tool_use" => parts.push(json!({
                "functionCall": { "name": block["name"], "args": block["input"] }
            })),
            _ => {}
        }
    }

    let u = &anthropic["usage"];
    let input = u["input_tokens"].as_u64().unwrap_or(0);
    let cached = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let output = u["output_tokens"].as_u64().unwrap_or(0);

    json!({
        "candidates": [{
            "content": { "role": "model", "parts": parts },
            "finishReason": match anthropic["stop_reason"].as_str().unwrap_or("end_turn") {
                "max_tokens" => "MAX_TOKENS",
                "refusal" => "SAFETY",
                _ => "STOP",
            },
            "index": 0,
        }],
        "usageMetadata": {
            // Folded back together: this dialect reports the total prompt.
            "promptTokenCount": input + cached,
            "cachedContentTokenCount": cached,
            "candidatesTokenCount": output,
            "totalTokenCount": input + cached + output,
        }
    })
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "promptTokenCount": u.input_tokens + u.cache_read_tokens,
        "cachedContentTokenCount": u.cache_read_tokens,
        "candidatesTokenCount": u.output_tokens,
        "totalTokenCount": u.total(),
    })
}

fn parse_usage(v: &Value) -> Usage {
    let cached = v["cachedContentTokenCount"].as_u64().unwrap_or(0);
    Usage {
        // `promptTokenCount` includes the cached prefix, so subtract to avoid
        // counting it twice and pricing it at the full input rate.
        input_tokens: v["promptTokenCount"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(cached),
        output_tokens: v["candidatesTokenCount"].as_u64().unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic streamed response, including a whole-arrival tool call and
    /// the usage metadata that lands with the final chunk.
    const STREAM: &[&str] = &[
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Let me "}]}}],"usageMetadata":{"promptTokenCount":19200,"cachedContentTokenCount":18000}}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"check."}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"read_file","args":{"path":"a.rs"}}}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":19200,"cachedContentTokenCount":18000,"candidatesTokenCount":142}}"#,
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
    fn the_cached_prefix_is_not_counted_twice() {
        // promptTokenCount includes the cached prefix here, as in Chat
        // Completions and unlike Anthropic. Adding them would bill 37200 for a
        // 19200-token prompt, and price the cached part at the full rate.
        let (_, acc) = drive(STREAM);
        let u = acc.usage();
        assert_eq!(u.cache_read_tokens, 18_000);
        assert_eq!(u.input_tokens, 1_200);
        assert_eq!(u.output_tokens, 142);
    }

    #[test]
    fn a_whole_tool_call_does_not_trip_the_malformed_gate() {
        // Arguments arrive complete here rather than in fragments, so the
        // reassembled JSON must still be valid.
        let (_, acc) = drive(STREAM);
        assert_eq!(acc.quality_gate(), None);
    }

    #[test]
    fn a_safety_stop_is_a_refusal_not_an_ordinary_end() {
        // The distinction drives escalation: a refusal is worth retrying on a
        // stronger model, an ordinary end-of-turn is not.
        let (events, _) = drive(&[
            r#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"SAFETY"}]}"#,
        ]);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Stop {
                reason: StopReason::Refusal,
                ..
            }
        )));
    }

    #[test]
    fn reasoning_parts_are_distinguished_from_ordinary_text() {
        // Reasoning is an ordinary text part flagged `thought`; treating it as
        // content would put the model's scratchpad in the answer.
        let (events, _) = drive(&[
            r#"{"candidates":[{"content":{"parts":[{"text":"hmm","thought":true},{"text":"answer"}]}}]}"#,
        ]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ThinkingDelta { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { .. }))
        );
    }

    #[test]
    fn a_request_round_trips_through_the_canonical_form() {
        let wire = json!({
            "systemInstruction": { "parts": [{ "text": "You are helpful." }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "read a.rs" }] },
                { "role": "model", "parts": [
                    { "functionCall": { "name": "read_file", "args": { "path": "a.rs" } } }
                ]},
                { "role": "user", "parts": [
                    { "functionResponse": { "name": "read_file",
                                            "response": { "result": "fn main() {}" } } }
                ]}
            ],
            "tools": [{ "functionDeclarations": [
                { "name": "read_file", "description": "reads",
                  "parameters": { "type": "object" } }
            ]}],
            "generationConfig": { "maxOutputTokens": 2048, "temperature": 0.4 }
        });

        let c = parse_request(&wire).expect("parses");
        assert_eq!(c.system.len(), 1);
        assert_eq!(
            c.tools.len(),
            1,
            "declarations are nested one level deeper here"
        );
        assert_eq!(c.max_tokens, 2048);
        assert_eq!(c.messages.len(), 3);

        let back = render_request(&c).expect("renders");
        // `model`, not `assistant`.
        assert_eq!(back["contents"][1]["role"], "model");
        assert_eq!(
            back["contents"][1]["parts"][0]["functionCall"]["name"],
            "read_file"
        );
        assert_eq!(
            back["tools"][0]["functionDeclarations"][0]["name"],
            "read_file"
        );
        assert_eq!(back["generationConfig"]["maxOutputTokens"], 2048);
        // The model name lives in the URL, not the body.
        assert!(back.get("model").is_none());
    }

    #[test]
    fn an_image_survives_the_round_trip() {
        let wire = json!({
            "contents": [{ "role": "user", "parts": [
                { "text": "what is this" },
                { "inlineData": { "mimeType": "image/png", "data": "AAAA" } }
            ]}]
        });
        let c = parse_request(&wire).expect("parses");
        assert!(c.has_images());
        let back = render_request(&c).expect("renders");
        assert_eq!(
            back["contents"][0]["parts"][1]["inlineData"]["data"],
            "AAAA"
        );
    }

    #[test]
    fn tool_choice_response_format_and_stop_survive_a_round_trip() {
        // Every one of the three has a different home here: a top-level
        // `toolConfig`, a MIME type plus a schema, and a `generationConfig`
        // list. None of them is where any other dialect keeps it.
        let schema = json!({"type": "object", "properties": {"n": {"type": "number"}}});
        let wire = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "toolConfig": {"functionCallingConfig": {"mode": "ANY"}},
            "generationConfig": {
                "maxOutputTokens": 512,
                "responseMimeType": "application/json",
                "responseSchema": schema.clone(),
                "stopSequences": ["END"],
            },
        });

        let c = parse_request(&wire).expect("parses");
        assert_eq!(c.tool_choice, Some(ToolChoice::Required));
        assert!(matches!(
            c.response_format,
            Some(ResponseFormat::JsonSchema { .. })
        ));
        assert_eq!(c.stop, vec!["END".to_owned()]);

        let back = render_request(&c).expect("renders");
        assert_eq!(
            back["toolConfig"]["functionCallingConfig"]["mode"], "ANY",
            "`ANY`, not `required`"
        );
        assert_eq!(
            back["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(back["generationConfig"]["responseSchema"], schema);
        assert_eq!(back["generationConfig"]["stopSequences"], json!(["END"]));
    }

    #[test]
    fn a_named_tool_choice_becomes_a_one_entry_allow_list() {
        // The only way this dialect says "call this specific tool".
        let c = crate::openai::parse_request(&json!({
            "model": "m", "messages": [],
            "tool_choice": {"type": "function", "function": {"name": "read_file"}}
        }))
        .expect("parses");

        let back = render_request(&c).expect("renders");
        let cfg = &back["toolConfig"]["functionCallingConfig"];
        assert_eq!(cfg["mode"], "ANY");
        assert_eq!(cfg["allowedFunctionNames"], json!(["read_file"]));

        // And it comes back as the named form rather than a bare `Required`.
        assert_eq!(
            parse_request(&back).expect("parses").tool_choice,
            Some(ToolChoice::Tool {
                name: "read_file".to_owned()
            })
        );
    }

    #[test]
    fn a_stored_response_id_this_dialect_cannot_express_is_refused() {
        // This dialect replays the whole conversation in `contents`; there is
        // nothing for a stored-response id to attach to.
        let c = crate::responses::parse_request(&json!({
            "model": "gpt-5", "input": "go on", "previous_response_id": "resp_abc",
        }))
        .expect("parses");

        let err = render_request(&c).expect_err("must not be dropped");
        assert!(matches!(
            err,
            Error::UnsupportedField {
                field: "previous_response_id",
                dialect: Dialect::GeminiGenerateContent,
            }
        ));
    }

    #[test]
    fn rendering_produces_frames_this_dialect_can_parse_back() {
        // The round-trip property the hub rests on: whatever we emit, our own
        // parser must recover the same content and counts from it.
        let mut st = RenderState::new();
        let events = [
            StreamEvent::Start {
                model: "m".to_owned(),
                usage: Usage {
                    input_tokens: 1200,
                    cache_read_tokens: 18_000,
                    ..Usage::default()
                },
            },
            StreamEvent::TextDelta {
                text: "Hello".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                usage: Usage {
                    output_tokens: 142,
                    ..Usage::default()
                },
            },
        ];
        let raw: String = events
            .iter()
            .filter_map(|e| render_event(e, &mut st))
            .collect();

        let mut acc = StreamAccumulator::new();
        let mut text = String::new();
        for frame in raw.split("\n\n").filter(|f| !f.trim().is_empty()) {
            let payload = frame.strip_prefix("data: ").expect("data prefix");
            for e in parse_event(payload, &mut acc).expect("parses") {
                acc.observe(&e);
                if let StreamEvent::TextDelta { text: t } = &e {
                    text.push_str(t);
                }
            }
        }
        assert_eq!(text, "Hello");
        assert_eq!(acc.usage().input_tokens, 1_200);
        assert_eq!(acc.usage().cache_read_tokens, 18_000);
        assert_eq!(acc.usage().output_tokens, 142);
    }

    #[test]
    fn a_duplicate_stop_does_not_emit_two_terminal_chunks() {
        let mut st = RenderState::new();
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
        let out = render_message_response(&anthropic);
        assert_eq!(
            out["candidates"][0]["content"]["parts"][0]["text"],
            "An answer."
        );
        assert_eq!(out["candidates"][0]["finishReason"], "STOP");
        assert_eq!(out["usageMetadata"]["promptTokenCount"], 19_200);
        assert_eq!(out["usageMetadata"]["cachedContentTokenCount"], 18_000);
    }

    #[test]
    fn thinking_blocks_are_dropped_rather_than_replayed() {
        // There is no wire representation, and inventing one gets the request
        // rejected. Dropping is the honest lossy choice.
        let c = CanonicalRequest {
            model: String::new(),
            system: vec![],
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "reasoning".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                        cache_control: None,
                    },
                ],
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
        let back = render_request(&c).expect("renders");
        let parts = back["contents"][0]["parts"].as_array().expect("parts");
        assert_eq!(parts.len(), 1, "only the answer survives");
        assert_eq!(parts[0]["text"], "answer");
    }
}
