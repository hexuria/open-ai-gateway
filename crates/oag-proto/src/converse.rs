//! The AWS Bedrock Converse dialect: `POST /model/{modelId}/converse`, and
//! `/converse-stream` for the same body streamed.
//!
//! Upstream only. No client speaks Converse, so this is the half of a codec an
//! upstream needs — canonical → a Converse request, and a Converse response or
//! stream event → canonical events — with no request parser and no renderer
//! for a Converse client behind it.
//!
//! Not wired to anything yet. PR 9b adds `Dialect::BedrockConverse`, points
//! this module's `DIALECT` at it, and builds the adapter that calls it.
//!
//! Where the dialect departs from the hub, each a place translation can lose
//! something:
//!
//! - A content block is an object with exactly one key — `{"text": …}`,
//!   `{"toolUse": …}` — rather than one tagged with `type`.
//! - The system prompt is a list of blocks; generation settings live under
//!   `inferenceConfig`, tools under `toolConfig`, structured output under
//!   `outputConfig`. The model, and whether to stream, are in the URL.
//! - Roles must alternate, so turns the canonical form keeps apart — a tool
//!   result and the user's next words, say — are merged into one.
//! - Tool names are held to the OpenAI function-name pattern, and sanitised
//!   the same way.
//! - Usage is Anthropic-shaped: `inputTokens` excludes the cached prefix,
//!   which is reported beside it rather than inside it.
//! - A stream announces its stop reason before its usage.
//!
//! The shapes are the documented ones:
//!
//! - <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html>
//! - <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html>
//! - <https://docs.aws.amazon.com/bedrock/latest/userguide/conversation-inference.html>,
//!   for the order a stream's events arrive in.

use crate::canonical::{
    CanonicalRequest, ContentBlock, Message, ResponseFormat, Role, Tool, ToolChoice,
    ToolResultContent,
};
use crate::function_names::FunctionNameMap;
use crate::stream::{StopReason, StreamAccumulator, StreamEvent};
use oag_core::provider::Dialect;
use oag_core::{Error, Result};
use oag_router::Usage;
use serde_json::{Value, json};

/// The dialect a refusal names.
///
/// A PLACEHOLDER until PR 9b. [`Error::UnsupportedField`] has to name a
/// dialect, and `Dialect::BedrockConverse` does not exist yet: adding it
/// belongs to the PR that wires this module in, which is also the first one
/// through which any request can reach a refusal here. Anthropic Messages
/// because it is what the Bedrock provider speaks today. 9b replaces this line.
const DIALECT: Dialect = Dialect::AnthropicMessages;

fn refused(field: &'static str) -> Error {
    Error::UnsupportedField {
        field,
        dialect: DIALECT,
    }
}

/// Canonical → a Converse request body.
///
/// Takes no model: Converse carries it in the URL, and streaming too, so the
/// body for `/converse` and `/converse-stream` is the same.
///
/// Three things canonical can say are refused rather than dropped, because
/// dropping them changes what the caller was promised:
///
/// - `previous_response_id` — see below.
/// - `response_format` as any JSON object: Converse's structured output takes
///   a schema, and has no mode for "an object, any object".
/// - `tool_choice: none` with tools defined: Converse's choices are `auto`,
///   `any` and `tool`, and none of them forbids a call.
///
/// Dropped, each for the reason given where it happens: a thinking budget or
/// level, thinking blocks, cache breakpoints, a schema's `strict` flag, empty
/// system blocks, and a passthrough residue.
pub fn render_request(req: &CanonicalRequest) -> Result<Value> {
    // A stored-response id is the conversation itself in the dialect that
    // issued it, and there is nothing here to hang it on: Converse replays the
    // whole conversation in `messages` every turn. Continuing without it
    // would answer a follow-up with the follow-up alone.
    if req.previous_response_id.is_some() {
        return Err(refused("previous_response_id"));
    }

    let names = FunctionNameMap::from_request(req);

    let mut inference = json!({ "maxTokens": req.max_tokens });
    if let Some(t) = req.temperature {
        inference["temperature"] = json!(t);
    }
    if !req.stop.is_empty() {
        inference["stopSequences"] = json!(req.stop);
    }

    let mut body = json!({
        "messages": render_messages(&req.messages, &names),
        "inferenceConfig": inference,
    });

    // Text only, one block each: a `SystemContentBlock` holds nothing else
    // canonical could put here. An empty one is left out rather than sent,
    // because the field refuses an empty string and the block says nothing.
    let system: Vec<Value> = req
        .system
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } if !text.is_empty() => Some(json!({ "text": text })),
            _ => None,
        })
        .collect();
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }

    if let Some(config) = render_tool_config(req, &names)? {
        body["toolConfig"] = config;
    }
    if let Some(config) = render_output_config(req.response_format.as_ref())? {
        body["outputConfig"] = config;
    }

    // Thinking is not rendered, in either spelling. Converse has no base field
    // for it: each model family takes its own knob under
    // `additionalModelRequestFields`, and a knob the model does not know is a
    // 400 where no knob is an answer without extended thinking. The same call
    // `anthropic::render_request` makes for a model whose generation it cannot
    // read.
    //
    // No passthrough either. A residue is re-emitted only into the dialect it
    // was written in, and no client writes Converse.
    Ok(body)
}

/// The canonical turns as Converse messages.
///
/// Turns whose role would repeat are merged, because Converse refuses a
/// conversation that does not alternate between `user` and `assistant`, and
/// the canonical form does not promise that it does: a Chat Completions
/// client's tool results and its next words arrive as two user turns. A turn
/// left with no block this dialect carries is dropped, since an empty
/// `content` is refused too — and dropping it can leave its neighbours
/// adjacent, which the merge then settles.
fn render_messages(messages: &[Message], names: &FunctionNameMap) -> Vec<Value> {
    let mut turns: Vec<(&str, Vec<Value>)> = Vec::new();
    for m in messages {
        // No `system` or `tool` role on the wire: the system prompt is its own
        // field, and a tool result is user-turn content, as in Anthropic.
        let role = if m.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let blocks: Vec<Value> = m
            .content
            .iter()
            .filter_map(|b| render_block(b, names))
            .collect();
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last, content)) if *last == role => content.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }
    turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect()
}

fn render_block(b: &ContentBlock, names: &FunctionNameMap) -> Option<Value> {
    match b {
        // A cache breakpoint is dropped. Converse spells one as a `cachePoint`
        // block, but only the models the prompt-caching guide lists take it,
        // and for any other it is at best ignored. Losing the breakpoint costs
        // money, not the answer — as it does on the Chat Completions and
        // Gemini wires, which have nowhere to put one.
        ContentBlock::Text { text, .. } => Some(json!({ "text": text })),
        ContentBlock::Image { media_type, data } => Some(image(media_type, data)),
        ContentBlock::ToolUse { id, name, input } => Some(json!({
            "toolUse": { "toolUseId": id, "name": names.wire(name), "input": input }
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut result = json!({
                "toolUseId": tool_use_id,
                "content": tool_result_content(content),
            });
            // Only a failure says so. `status` is documented for Nova and
            // Claude alone and success is what its absence means, so a result
            // that worked carries nothing another model family could refuse,
            // and one that failed keeps the one word that says it failed.
            if *is_error {
                result["status"] = json!("error");
            }
            Some(json!({ "toolResult": result }))
        }
        // Reasoning is not replayed. Converse has `reasoningContent` for it,
        // but only the model that produced the reasoning can take it back,
        // signature and all, and a canonical block may have come from any
        // upstream. Dropped, as `gemini.rs` drops it.
        ContentBlock::Thinking { .. } => None,
    }
}

/// A tool result's content, as Converse's list of blocks.
///
/// A string stays one text block, verbatim even when it holds JSON: canonical
/// does not say a result *is* JSON, and re-typing text as a `json` block
/// changes what the model is shown. Blocks keep their shape where Converse has
/// one — text, and the image a screenshot tool hands back, which the
/// string-only dialects have to flatten away.
fn tool_result_content(content: &ToolResultContent) -> Vec<Value> {
    match content {
        ToolResultContent::Text(text) => vec![json!({ "text": text })],
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(json!({ "text": text })),
                ContentBlock::Image { media_type, data } => Some(image(media_type, data)),
                // A result cannot hold a call, another result, or reasoning.
                _ => None,
            })
            .collect(),
    }
}

/// An image block.
///
/// Converse names the format rather than the MIME type, and takes `png`,
/// `jpeg`, `gif` or `webp`. Any other type is passed on as its subtype for the
/// upstream to refuse, as the other renderers pass theirs on.
fn image(media_type: &str, data: &str) -> Value {
    json!({ "image": {
        "format": media_type.strip_prefix("image/").unwrap_or(media_type),
        // Canonical holds base64 already, which is what the JSON wire takes.
        "source": { "bytes": data },
    }})
}

/// `toolConfig`, or `None` when the request defines no tools.
///
/// `toolChoice` lives inside `toolConfig`, and `toolConfig` needs at least one
/// tool — so with none there is nothing for a choice to constrain, and `none`
/// in particular is already true.
fn render_tool_config(req: &CanonicalRequest, names: &FunctionNameMap) -> Result<Option<Value>> {
    if req.tools.is_empty() {
        return Ok(None);
    }
    let mut config = json!({
        "tools": req.tools.iter().map(|t| render_tool(t, names)).collect::<Vec<_>>(),
    });
    if let Some(choice) = &req.tool_choice {
        config["toolChoice"] = match choice {
            ToolChoice::Auto => json!({ "auto": {} }),
            // `any` here, as in Anthropic; `required` everywhere else.
            ToolChoice::Required => json!({ "any": {} }),
            ToolChoice::Tool { name } => json!({ "tool": { "name": names.wire(name) } }),
            // No spelling for it, and leaving the choice out means `auto`,
            // which lets the model call a tool the client has just forbidden.
            ToolChoice::None => return Err(refused("tool_choice")),
        };
    }
    Ok(Some(config))
}

fn render_tool(t: &Tool, names: &FunctionNameMap) -> Value {
    let mut spec = json!({
        "name": names.wire(&t.name),
        "inputSchema": { "json": t.input_schema },
    });
    // At least one character when present, so the empty string canonical
    // defaults a missing description to is left out rather than refused. A
    // cache breakpoint on the tool is dropped, as on a text block.
    if !t.description.is_empty() {
        spec["description"] = json!(t.description);
    }
    json!({ "toolSpec": spec })
}

/// `outputConfig`, or `None` when the answer is free text.
///
/// Structured output, from
/// <https://docs.aws.amazon.com/bedrock/latest/userguide/structured-output.html>.
fn render_output_config(format: Option<&ResponseFormat>) -> Result<Option<Value>> {
    match format {
        // Free text is what Converse does anyway, so asking for it is no
        // reason to refuse the request.
        None | Some(ResponseFormat::Text) => Ok(None),
        Some(ResponseFormat::JsonObject) => Err(refused("response_format")),
        // The schema travels as a JSON *string*, not an object. `strict` has
        // no field and loses nothing: the schema is always enforced, which is
        // at worst stricter than was asked.
        Some(ResponseFormat::JsonSchema { name, schema, .. }) => Ok(Some(json!({
            "textFormat": {
                "type": "json_schema",
                "structure": { "jsonSchema": { "schema": schema.to_string(), "name": name } },
            }
        }))),
    }
}

/// A complete `/converse` response → the events its stream would have carried.
///
/// The counterpart to [`parse_event`], and it has to reach the same
/// accumulator state: the quality gate reads the text and tool-call counts, so
/// a reader that takes only usage leaves every non-streamed answer looking
/// empty.
///
/// Tool names come back as the wire spelled them. The caller restores the
/// client's own with `FunctionNameMap::restore_in_events`, as it does for a
/// Chat Completions body.
#[must_use]
pub fn parse_response(body: &Value) -> Vec<StreamEvent> {
    let usage = parse_usage(&body["usage"]);
    let mut events = vec![StreamEvent::UsageUpdate { usage }];

    for block in body["output"]["message"]["content"]
        .as_array()
        .unwrap_or(&Vec::new())
    {
        if let Some(text) = block["text"].as_str() {
            events.push(StreamEvent::TextDelta {
                text: text.to_owned(),
            });
        } else if let Some(text) = block["reasoningContent"]["reasoningText"]["text"].as_str() {
            events.push(StreamEvent::ThinkingDelta {
                text: text.to_owned(),
            });
        } else if let Some(call) = block.get("toolUse") {
            let id = call["toolUseId"].as_str().unwrap_or_default().to_owned();
            events.push(StreamEvent::ToolUseStart {
                id: id.clone(),
                name: call["name"].as_str().unwrap_or_default().to_owned(),
            });
            // Whole, not fragmented: a non-streamed call is complete JSON, so
            // the malformed-arguments gate should never fire on one.
            events.push(StreamEvent::ToolUseDelta {
                id: id.clone(),
                partial_json: call["input"].to_string(),
            });
            events.push(StreamEvent::ToolUseEnd { id });
        }
    }

    if let Some(reason) = body["stopReason"].as_str() {
        events.push(StreamEvent::Stop {
            reason: parse_stop_reason(reason),
            usage,
        });
    }

    events
}

/// One `ConverseStream` event → canonical events.
///
/// Takes the event in the union shape the API reference documents for the
/// stream: the event's type as the one key, its payload as the value —
/// `{"contentBlockDelta": {…}}`. On the wire the type is the `:event-type`
/// header of an AWS event-stream message rather than part of its payload, so
/// whoever decodes the framing (`oag_upstream::eventstream`) wraps each
/// payload in it. Doing that is 9b's.
///
/// An empty result is normal: a text block's `contentBlockStop`, and
/// `messageStop`, which is held until the usage arrives.
pub fn parse_event(payload: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
    let v: Value = serde_json::from_str(payload)?;
    let mut events = Vec::new();
    // A union has exactly one member. Every one is read all the same, so an
    // unexpected shape yields what it can rather than an error.
    for (kind, event) in v.as_object().into_iter().flatten() {
        events.extend(parse_stream_event(kind, event, acc));
    }
    Ok(events)
}

fn parse_stream_event(kind: &str, event: &Value, acc: &mut StreamAccumulator) -> Vec<StreamEvent> {
    match kind {
        // Announces the role and nothing else: no model, no usage yet.
        "messageStart" => vec![StreamEvent::Start {
            model: String::new(),
            usage: Usage::default(),
        }],

        // Only a tool block opens with an event. Text and reasoning are fully
        // described by their deltas.
        "contentBlockStart" => {
            let call = &event["start"]["toolUse"];
            call["toolUseId"]
                .as_str()
                .map(|id| {
                    vec![StreamEvent::ToolUseStart {
                        id: id.to_owned(),
                        name: acc.restore_function_name(call["name"].as_str().unwrap_or_default()),
                    }]
                })
                .unwrap_or_default()
        }

        "contentBlockDelta" => parse_delta(&event["delta"], acc),

        // Every block ends with this, whatever kind it was, and the event does
        // not say which. Only a call that is still open is ended; see
        // `anthropic::parse_event`, whose `content_block_stop` taught that.
        "contentBlockStop" => acc
            .open_tool_id()
            .map(|id| vec![StreamEvent::ToolUseEnd { id }])
            .unwrap_or_default(),

        // Held, not emitted: the usage is still to come, in `metadata`, and
        // every renderer writes its terminal frame on the stop. See
        // `StreamAccumulator::hold_stop`.
        "messageStop" => {
            acc.hold_stop(parse_stop_reason(
                event["stopReason"].as_str().unwrap_or_default(),
            ));
            vec![]
        }

        // Last in the stream, and where the bill is.
        "metadata" => {
            let usage = parse_usage(&event["usage"]);
            vec![
                acc.take_held_stop()
                    .map_or(StreamEvent::UsageUpdate { usage }, |reason| {
                        StreamEvent::Stop { reason, usage }
                    }),
            ]
        }

        // `throttlingException`, `modelStreamErrorException` and the rest: an
        // error inside a 200 stream. The kind goes into the message because
        // that is the only place it survives — on the wire it is the name of
        // the event, not a field in it.
        kind if kind.ends_with("Exception") => vec![StreamEvent::Error {
            message: event["message"]
                .as_str()
                .map_or_else(|| kind.to_owned(), |m| format!("{kind}: {m}")),
        }],

        // Anything AWS adds later.
        _ => vec![],
    }
}

fn parse_delta(delta: &Value, acc: &StreamAccumulator) -> Vec<StreamEvent> {
    if let Some(text) = delta["text"].as_str() {
        vec![StreamEvent::TextDelta {
            text: text.to_owned(),
        }]
    } else if let Some(fragment) = delta["toolUse"]["input"].as_str() {
        // Partial JSON, addressed by block index rather than by id, as in
        // Anthropic. The accumulator holds the id the opening event carried.
        vec![StreamEvent::ToolUseDelta {
            id: acc.current_tool_id().unwrap_or_default(),
            partial_json: fragment.to_owned(),
        }]
    } else if let Some(text) = delta["reasoningContent"]["text"].as_str() {
        vec![StreamEvent::ThinkingDelta {
            text: text.to_owned(),
        }]
    } else {
        // A reasoning signature, redacted reasoning, a citation: none of them
        // is part of the streamed answer.
        vec![]
    }
}

/// `TokenUsage` → canonical usage.
///
/// Anthropic's shape, not Chat Completions': `inputTokens` is only the
/// uncached part of the prompt, and the cache counts are reported beside it, so
/// nothing is subtracted. `totalTokens` is the four added up, and is left to
/// `Usage::total` to recompute.
fn parse_usage(v: &Value) -> Usage {
    Usage {
        input_tokens: v["inputTokens"].as_u64().unwrap_or(0),
        output_tokens: v["outputTokens"].as_u64().unwrap_or(0),
        cache_read_tokens: v["cacheReadInputTokens"].as_u64().unwrap_or(0),
        cache_write_tokens: v["cacheWriteInputTokens"].as_u64().unwrap_or(0),
    }
}

/// A documented `stopReason` → the canonical reason.
fn parse_stop_reason(raw: &str) -> StopReason {
    match raw {
        "tool_use" => StopReason::ToolUse,
        // Out of room either way: the ceiling the request set, or the model's
        // whole context window. The answer was cut off rather than finished,
        // which is what the quality gate escalates as truncated.
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        // A guardrail or a content filter stopped the answer: Converse's two
        // words for what Gemini calls `SAFETY`.
        "guardrail_intervened" | "content_filtered" => StopReason::Refusal,
        // `end_turn`, and the two `malformed_*` reasons, which canonical has
        // no word for: the gate judges what did arrive — an empty answer, or
        // arguments that do not parse — rather than the label. Anything AWS
        // adds later lands here too.
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{CacheControl, Effort, Passthrough};

    fn request(f: impl FnOnce(&mut CanonicalRequest)) -> CanonicalRequest {
        let mut req = CanonicalRequest {
            model: "m".to_owned(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            max_tokens: 1024,
            stream: false,
            temperature: None,
            thinking_budget: None,
            thinking_effort: None,
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
            passthrough: None,
        };
        f(&mut req);
        req
    }

    fn text(s: &str) -> ContentBlock {
        ContentBlock::Text {
            text: s.to_owned(),
            cache_control: None,
        }
    }

    fn turn(role: Role, content: Vec<ContentBlock>) -> Message {
        Message { role, content }
    }

    fn drive(lines: &[&str]) -> (Vec<StreamEvent>, StreamAccumulator) {
        drive_from(StreamAccumulator::new(), lines)
    }

    /// Parse each event and fold it in, the way the gateway's stream loop does.
    fn drive_from(
        mut acc: StreamAccumulator,
        lines: &[&str],
    ) -> (Vec<StreamEvent>, StreamAccumulator) {
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

    fn stop_of(events: &[StreamEvent]) -> Option<StopReason> {
        events.iter().find_map(|e| match e {
            StreamEvent::Stop { reason, .. } => Some(*reason),
            _ => None,
        })
    }

    /// The last event of every stream: the bill, after the stop reason.
    const METADATA: &str = r#"{"metadata":{"usage":{"inputTokens":47,"outputTokens":20,"totalTokens":67},"metrics":{"latencyMs":100}}}"#;

    /// The partial `ConverseStream` response the user guide prints, completed:
    /// an empty first delta, text in pieces, the block's stop, then
    /// `messageStop` and — last — `metadata`.
    /// <https://docs.aws.amazon.com/bedrock/latest/userguide/conversation-inference.html>
    const TEXT_STREAM: &[&str] = &[
        r#"{"messageStart":{"role":"assistant"}}"#,
        r#"{"contentBlockDelta":{"delta":{"text":""},"contentBlockIndex":0}}"#,
        r#"{"contentBlockDelta":{"delta":{"text":" Title"},"contentBlockIndex":0}}"#,
        r#"{"contentBlockDelta":{"delta":{"text":":"},"contentBlockIndex":0}}"#,
        r#"{"contentBlockDelta":{"delta":{"text":" The"},"contentBlockIndex":0}}"#,
        r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
        r#"{"messageStop":{"stopReason":"max_tokens"}}"#,
        METADATA,
    ];

    /// Text, then a tool call whose input arrives in three fragments that
    /// split a key and a value, in the order the user guide draws: the call's
    /// own `contentBlockStart`, its deltas, its stop, then the stop reason and
    /// the bill.
    const TOOL_STREAM: &[&str] = &[
        r#"{"messageStart":{"role":"assistant"}}"#,
        r#"{"contentBlockDelta":{"delta":{"text":"Let me look that up."},"contentBlockIndex":0}}"#,
        r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
        r#"{"contentBlockStart":{"start":{"toolUse":{"toolUseId":"tooluse_kZJMlvQmRJ6eAyJE5GIl7Q","name":"top_song"}},"contentBlockIndex":1}}"#,
        r#"{"contentBlockDelta":{"delta":{"toolUse":{"input":"{\"si"}},"contentBlockIndex":1}}"#,
        r#"{"contentBlockDelta":{"delta":{"toolUse":{"input":"gn\": \"WZ"}},"contentBlockIndex":1}}"#,
        r#"{"contentBlockDelta":{"delta":{"toolUse":{"input":"PZ\"}"}},"contentBlockIndex":1}}"#,
        r#"{"contentBlockStop":{"contentBlockIndex":1}}"#,
        r#"{"messageStop":{"stopReason":"tool_use"}}"#,
        r#"{"metadata":{"usage":{"inputTokens":412,"outputTokens":58,"totalTokens":470},"metrics":{"latencyMs":930}}}"#,
    ];

    /// The API reference's own sample request, reached from the canonical
    /// form: a system prompt, one user turn and the inference settings, with
    /// the stop sequences from its second sample.
    /// <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html>
    #[test]
    fn a_text_turn_renders_as_the_documented_request() {
        let req = request(|r| {
            r.system = vec![text("You are an economist with access to lots of data")];
            r.messages = vec![turn(
                Role::User,
                vec![text(
                    "Write an article about impact of high inflation to GDP of a country",
                )],
            )];
            r.max_tokens = 1000;
            r.temperature = Some(0.5);
            r.stop = vec!["SUCCESS".to_owned(), "FAILURE".to_owned()];
            // Neither of these is in the body. Both are in the URL.
            r.model = "anthropic.claude-3-sonnet-20240229-v1:0".to_owned();
            r.stream = true;
        });
        assert_eq!(
            render_request(&req).expect("renders"),
            json!({
                "messages": [{
                    "role": "user",
                    "content": [{
                        "text": "Write an article about impact of high inflation to GDP of a country"
                    }],
                }],
                "system": [{ "text": "You are an economist with access to lots of data" }],
                "inferenceConfig": {
                    "maxTokens": 1000,
                    "temperature": 0.5,
                    "stopSequences": ["SUCCESS", "FAILURE"],
                },
            })
        );
    }

    /// The tool the user guide defines, and each of the three choices
    /// Converse has.
    /// <https://docs.aws.amazon.com/bedrock/latest/userguide/tool-use-client-side.html>
    #[test]
    fn a_tool_renders_as_a_tool_spec_and_each_choice_in_this_dialects_spelling() {
        let schema = json!({
            "type": "object",
            "properties": { "sign": { "type": "string", "description": "The call sign." } },
            "required": ["sign"],
        });
        let with = |choice: Option<ToolChoice>| {
            request(|r| {
                r.messages = vec![turn(
                    Role::User,
                    vec![text("What is the most popular song on WZPZ?")],
                )];
                r.tools = vec![
                    Tool {
                        name: "top_song".to_owned(),
                        description: "Get the most popular song played on a radio station."
                            .to_owned(),
                        input_schema: schema.clone(),
                        cache_control: None,
                    },
                    // What canonical holds for a function declared without a
                    // description. Sent as it is, the empty string is a 400.
                    Tool {
                        name: "now".to_owned(),
                        description: String::new(),
                        input_schema: json!({ "type": "object" }),
                        cache_control: None,
                    },
                ];
                r.tool_choice = choice;
            })
        };

        assert_eq!(
            render_request(&with(None)).expect("renders")["toolConfig"],
            json!({ "tools": [
                { "toolSpec": {
                    "name": "top_song",
                    "description": "Get the most popular song played on a radio station.",
                    "inputSchema": { "json": schema },
                }},
                { "toolSpec": { "name": "now", "inputSchema": { "json": { "type": "object" } } } },
            ]}),
            "no choice is Converse's own default, `auto`, and is left out"
        );

        for (choice, wire) in [
            (ToolChoice::Auto, json!({ "auto": {} })),
            (ToolChoice::Required, json!({ "any": {} })),
            (
                ToolChoice::Tool {
                    name: "top_song".to_owned(),
                },
                json!({ "tool": { "name": "top_song" } }),
            ),
        ] {
            let body = render_request(&with(Some(choice.clone()))).expect("renders");
            assert_eq!(body["toolConfig"]["toolChoice"], wire, "{choice:?}");
        }
    }

    #[test]
    fn forbidding_tool_calls_is_refused_while_there_is_a_tool_to_forbid() {
        // Converse's choices are `auto`, `any` and `tool`. Leaving `none` out
        // means `auto`, and a client that forbade calls — often to get its
        // final answer as text — would be handed one it has no plan for.
        let c = crate::openai::parse_request(&json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "summarise what you found" }],
            "tools": [{ "type": "function",
                        "function": { "name": "search", "parameters": { "type": "object" } } }],
            "tool_choice": "none",
        }))
        .expect("parses");
        assert_eq!(c.tool_choice, Some(ToolChoice::None), "the premise");
        let err = render_request(&c).expect_err("must not be dropped");
        assert!(
            matches!(
                err,
                Error::UnsupportedField {
                    field: "tool_choice",
                    ..
                }
            ),
            "{err}"
        );

        // With no tools there is nothing to forbid, so `none` already holds.
        let c = crate::openai::parse_request(&json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "tool_choice": "none",
        }))
        .expect("parses");
        let body = render_request(&c).expect("nothing to refuse");
        assert!(body.get("toolConfig").is_none(), "{body}");
    }

    /// A turn that called a tool and the turn that answers it, in the shapes
    /// the tool-use guide shows: a result that worked, one whose text is JSON,
    /// and one that failed.
    /// <https://docs.aws.amazon.com/bedrock/latest/userguide/tool-use-client-side.html>
    #[test]
    fn a_tool_call_and_its_results_render_as_tool_use_and_tool_result() {
        let call = |id: &str, sign: &str| ContentBlock::ToolUse {
            id: id.to_owned(),
            name: "top_song".to_owned(),
            input: json!({ "sign": sign }),
        };
        let result = |id: &str, content: &str, is_error: bool| ContentBlock::ToolResult {
            tool_use_id: id.to_owned(),
            content: ToolResultContent::Text(content.to_owned()),
            is_error,
        };
        let req = request(|r| {
            r.messages = vec![
                turn(Role::User, vec![text("Top songs on WZPZ, WKRP and WXYZ?")]),
                turn(
                    Role::Assistant,
                    vec![
                        call("tooluse_kZJMlvQmRJ6eAyJE5GIl7Q", "WZPZ"),
                        call("tooluse_2", "WKRP"),
                        call("tooluse_3", "WXYZ"),
                    ],
                ),
                turn(
                    Role::User,
                    vec![
                        result("tooluse_kZJMlvQmRJ6eAyJE5GIl7Q", "Elemental Hotel", false),
                        result("tooluse_2", r#"{"song":"Starman","artist":"Bowie"}"#, false),
                        result("tooluse_3", "Station WXYZ not found.", true),
                    ],
                ),
            ];
        });
        let body = render_request(&req).expect("renders");

        assert_eq!(
            body["messages"][1],
            json!({ "role": "assistant", "content": [
                { "toolUse": { "toolUseId": "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q",
                               "name": "top_song", "input": { "sign": "WZPZ" } } },
                { "toolUse": { "toolUseId": "tooluse_2",
                               "name": "top_song", "input": { "sign": "WKRP" } } },
                { "toolUse": { "toolUseId": "tooluse_3",
                               "name": "top_song", "input": { "sign": "WXYZ" } } },
            ]})
        );
        assert_eq!(
            body["messages"][2],
            json!({ "role": "user", "content": [
                { "toolResult": { "toolUseId": "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q",
                                  "content": [{ "text": "Elemental Hotel" }] } },
                // JSON text stays text, byte for byte. Canonical does not say
                // it is JSON, and a `json` block would show the model
                // something other than what the tool returned.
                { "toolResult": { "toolUseId": "tooluse_2",
                                  "content": [{ "text": r#"{"song":"Starman","artist":"Bowie"}"# }] } },
                // Only a failure carries `status`.
                { "toolResult": { "toolUseId": "tooluse_3",
                                  "content": [{ "text": "Station WXYZ not found." }],
                                  "status": "error" } },
            ]})
        );
    }

    #[test]
    fn an_image_renders_with_its_format_and_bytes_in_a_turn_and_in_a_tool_result() {
        let req = request(|r| {
            r.messages = vec![
                turn(
                    Role::User,
                    vec![
                        text("what is this"),
                        ContentBlock::Image {
                            media_type: "image/png".to_owned(),
                            data: "iVBORw0KGgo=".to_owned(),
                        },
                    ],
                ),
                turn(
                    Role::Assistant,
                    vec![ContentBlock::ToolUse {
                        id: "tooluse_1".to_owned(),
                        name: "screenshot".to_owned(),
                        input: json!({}),
                    }],
                ),
                turn(
                    Role::User,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: "tooluse_1".to_owned(),
                        content: ToolResultContent::Blocks(vec![
                            text("the screen"),
                            ContentBlock::Image {
                                media_type: "image/jpeg".to_owned(),
                                data: "/9j/4AAQ".to_owned(),
                            },
                        ]),
                        is_error: false,
                    }],
                ),
            ];
        });
        let body = render_request(&req).expect("renders");

        assert_eq!(
            body["messages"][0]["content"],
            json!([
                { "text": "what is this" },
                { "image": { "format": "png", "source": { "bytes": "iVBORw0KGgo=" } } },
            ])
        );
        // A screenshot tool's image stays an image, where the string-only
        // dialects have to flatten it away.
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"]["content"],
            json!([
                { "text": "the screen" },
                { "image": { "format": "jpeg", "source": { "bytes": "/9j/4AAQ" } } },
            ])
        );
    }

    #[test]
    fn turns_of_one_role_in_a_row_are_merged_into_one() {
        // Converse refuses a conversation that does not alternate. A Chat
        // Completions client's tool result and its next words are two user
        // turns in canonical form, and sent as they are that is a 400.
        let c = crate::openai::parse_request(&json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "read a.rs" },
                { "role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" },
                }]},
                { "role": "tool", "tool_call_id": "call_1", "content": "fn main() {}" },
                { "role": "user", "content": "now explain it" },
            ],
        }))
        .expect("parses");
        assert_eq!(c.messages.len(), 4, "the premise: two user turns in a row");

        assert_eq!(
            render_request(&c).expect("renders")["messages"],
            json!([
                { "role": "user", "content": [{ "text": "read a.rs" }] },
                { "role": "assistant", "content": [{ "toolUse": {
                    "toolUseId": "call_1", "name": "read_file", "input": { "path": "a.rs" },
                }}]},
                { "role": "user", "content": [
                    { "toolResult": { "toolUseId": "call_1",
                                      "content": [{ "text": "fn main() {}" }] } },
                    { "text": "now explain it" },
                ]},
            ])
        );
    }

    #[test]
    fn a_turn_with_nothing_this_dialect_carries_is_left_out_and_its_neighbours_merge() {
        // A turn that was only reasoning renders no block, and an empty
        // `content` is refused. Leaving it out puts two user turns side by
        // side, which is the merge's to settle.
        let req = request(|r| {
            r.messages = vec![
                turn(Role::User, vec![text("first")]),
                turn(
                    Role::Assistant,
                    vec![ContentBlock::Thinking {
                        text: "hmm".to_owned(),
                        signature: Some("sig".to_owned()),
                    }],
                ),
                turn(Role::User, vec![text("second")]),
            ];
        });
        assert_eq!(
            render_request(&req).expect("renders")["messages"],
            json!([{ "role": "user", "content": [{ "text": "first" }, { "text": "second" }] }])
        );
    }

    #[test]
    fn what_this_dialect_has_no_general_field_for_is_left_out() {
        // Each for the reason at its site: a cache breakpoint is model-gated,
        // reasoning can only go back to the model that wrote it, a thinking
        // knob is per model family, an empty system block is refused, and no
        // client writes a Converse residue.
        let req = request(|r| {
            r.system = vec![
                ContentBlock::Text {
                    text: "stable".to_owned(),
                    cache_control: Some(CacheControl::Ephemeral),
                },
                text(""),
            ];
            r.messages = vec![
                turn(Role::User, vec![text("hi")]),
                turn(
                    Role::Assistant,
                    vec![
                        ContentBlock::Thinking {
                            text: "hmm".to_owned(),
                            signature: Some("sig".to_owned()),
                        },
                        text("hello"),
                    ],
                ),
            ];
            r.thinking_budget = Some(8192);
            r.thinking_effort = Some(Effort::High);
            r.client_session = Some("session".to_owned());
            // Written in the dialect `DIALECT` stands in for today, so a
            // `merge_into` here would take it.
            r.passthrough = Some(Passthrough {
                dialect: Dialect::AnthropicMessages,
                body: json!({ "top_k": 5 }),
            });
        });
        assert_eq!(
            render_request(&req).expect("renders"),
            json!({
                "messages": [
                    { "role": "user", "content": [{ "text": "hi" }] },
                    { "role": "assistant", "content": [{ "text": "hello" }] },
                ],
                "system": [{ "text": "stable" }],
                "inferenceConfig": { "maxTokens": 1024 },
            })
        );
    }

    /// Structured output, as the guide's Converse example spells it.
    /// <https://docs.aws.amazon.com/bedrock/latest/userguide/structured-output.html>
    #[test]
    fn a_json_schema_renders_as_output_config_with_the_schema_as_a_string() {
        let schema = json!({
            "type": "object",
            "properties": { "title": { "type": "string" } },
            "required": ["title"],
            "additionalProperties": false,
        });
        let req = request(|r| {
            r.response_format = Some(ResponseFormat::JsonSchema {
                name: "data_extraction".to_owned(),
                schema: schema.clone(),
                strict: true,
            });
        });
        let body = render_request(&req).expect("renders");
        assert_eq!(
            body["outputConfig"],
            json!({ "textFormat": {
                "type": "json_schema",
                "structure": { "jsonSchema": {
                    "schema": schema.to_string(),
                    "name": "data_extraction",
                }},
            }})
        );
        // A string on this wire, and the same schema inside it.
        let sent = body["outputConfig"]["textFormat"]["structure"]["jsonSchema"]["schema"]
            .as_str()
            .expect("a string, not an object");
        assert_eq!(serde_json::from_str::<Value>(sent).expect("JSON"), schema);
    }

    #[test]
    fn any_json_object_is_refused_and_plain_text_is_not() {
        // Converse's structured output needs a schema. Dropping the constraint
        // sends a client that is about to call `JSON.parse` a paragraph of
        // prose, and nothing in the answer says the constraint went.
        let c = crate::openai::parse_request(&json!({
            "model": "m", "messages": [{ "role": "user", "content": "hi" }],
            "response_format": { "type": "json_object" },
        }))
        .expect("parses");
        let err = render_request(&c).expect_err("must not be dropped");
        assert!(matches!(
            err,
            Error::UnsupportedField {
                field: "response_format",
                ..
            }
        ));

        let c = crate::openai::parse_request(&json!({
            "model": "m", "messages": [], "response_format": { "type": "text" },
        }))
        .expect("parses");
        let body = render_request(&c).expect("plain text is expressible");
        assert!(body.get("outputConfig").is_none(), "{body}");
    }

    #[test]
    fn a_stored_response_id_this_dialect_cannot_express_is_refused() {
        // The conversation lives in `messages` here; a follow-up sent without
        // what it follows would be answered on its own.
        let c = crate::responses::parse_request(&json!({
            "model": "gpt-5", "input": "go on", "previous_response_id": "resp_abc",
        }))
        .expect("parses");
        let err = render_request(&c).expect_err("must not be dropped");
        assert!(matches!(
            err,
            Error::UnsupportedField {
                field: "previous_response_id",
                ..
            }
        ));
    }

    #[test]
    fn a_tool_name_this_dialect_refuses_is_sanitised_and_restored() {
        // Converse holds tool names to `[a-zA-Z0-9_-]{1,64}`, the OpenAI
        // pattern, and a connector tool like `user-Github.get_file` fails it —
        // so the whole turn would be a 400. Sanitised on the way out, and put
        // back on the call the model makes, or the client cannot dispatch it.
        let original = "user-Github.get_file";
        let wire = "user-Github_get_file";
        let req = request(|r| {
            r.tools = vec![Tool {
                name: original.to_owned(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
                cache_control: None,
            }];
            r.tool_choice = Some(ToolChoice::Tool {
                name: original.to_owned(),
            });
            r.messages = vec![
                turn(Role::User, vec![text("read it")]),
                turn(
                    Role::Assistant,
                    vec![ContentBlock::ToolUse {
                        id: "tooluse_1".to_owned(),
                        name: original.to_owned(),
                        input: json!({}),
                    }],
                ),
            ];
        });
        let body = render_request(&req).expect("renders");
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], wire);
        assert_eq!(body["toolConfig"]["toolChoice"]["tool"]["name"], wire);
        assert_eq!(body["messages"][1]["content"][0]["toolUse"]["name"], wire);

        let acc = StreamAccumulator::new().with_function_names(FunctionNameMap::from_request(&req));
        let (events, _) = drive_from(
            acc,
            &[
                r#"{"contentBlockStart":{"start":{"toolUse":{"toolUseId":"tooluse_2","name":"user-Github_get_file"}},"contentBlockIndex":0}}"#,
            ],
        );
        assert_eq!(
            events,
            vec![StreamEvent::ToolUseStart {
                id: "tooluse_2".to_owned(),
                name: original.to_owned(),
            }]
        );
    }

    // ── responses and streams ────────────────────────────────────────────────

    /// Every value `stopReason` is documented to take, and one it is not,
    /// through both readers: a whole body, and a stream's `messageStop`.
    /// <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_MessageStopEvent.html>
    #[test]
    fn each_stop_reason_maps_to_its_canonical_reason() {
        for (wire, reason) in [
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("guardrail_intervened", StopReason::Refusal),
            ("content_filtered", StopReason::Refusal),
            ("model_context_window_exceeded", StopReason::MaxTokens),
            ("malformed_model_output", StopReason::EndTurn),
            ("malformed_tool_use", StopReason::EndTurn),
            ("a_reason_from_2027", StopReason::EndTurn),
        ] {
            let body = json!({
                "output": { "message": { "role": "assistant", "content": [{ "text": "x" }] } },
                "stopReason": wire,
                "usage": { "inputTokens": 3, "outputTokens": 1, "totalTokens": 4 },
            });
            assert_eq!(
                stop_of(&parse_response(&body)),
                Some(reason),
                "{wire}, whole"
            );

            let stop = json!({ "messageStop": { "stopReason": wire } }).to_string();
            let (events, acc) = drive(&[&stop, METADATA]);
            assert_eq!(stop_of(&events), Some(reason), "{wire}, streamed");
            assert_eq!(acc.stop_reason(), Some(reason), "{wire}, streamed");
        }
    }

    /// With prompt caching on, `inputTokens` is only the uncached part: the
    /// guide's own formula for the whole prompt is `inputTokens +
    /// cacheReadInputTokens + cacheWriteInputTokens`.
    /// <https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html>
    #[test]
    fn usage_with_the_cache_is_read_without_counting_the_prefix_twice() {
        let body = json!({
            "output": { "message": { "role": "assistant", "content": [{ "text": "An answer." }] } },
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 1200,
                "outputTokens": 142,
                "totalTokens": 19642,
                "cacheReadInputTokens": 18000,
                "cacheWriteInputTokens": 300,
            },
            "metrics": { "latencyMs": 1275 },
        });
        let mut acc = StreamAccumulator::new();
        for e in &parse_response(&body) {
            acc.observe(e);
        }
        assert_eq!(
            *acc.usage(),
            Usage {
                input_tokens: 1200,
                output_tokens: 142,
                cache_read_tokens: 18000,
                cache_write_tokens: 300,
            }
        );
        // Subtracting the cache from `inputTokens`, as Chat Completions needs,
        // would bill the uncached prompt as zero; adding it in would bill the
        // prefix twice. Either way the four would not add up to AWS's total.
        assert_eq!(acc.usage().total(), 19_642);

        // A model without caching reports neither cache count.
        let (_, acc) = drive(TEXT_STREAM);
        assert_eq!(
            *acc.usage(),
            Usage {
                input_tokens: 47,
                output_tokens: 20,
                ..Usage::default()
            }
        );
    }

    #[test]
    fn a_whole_response_becomes_the_events_its_stream_would_carry() {
        let body = json!({
            "output": { "message": { "role": "assistant", "content": [
                { "reasoningContent": { "reasoningText": {
                    "text": "WZPZ is a call sign.", "signature": "c2ln" } } },
                { "text": "Let me look that up." },
                { "toolUse": { "toolUseId": "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q",
                               "name": "top_song", "input": { "sign": "WZPZ" } } },
            ]}},
            "stopReason": "tool_use",
            "usage": { "inputTokens": 30, "outputTokens": 62, "totalTokens": 92 },
            "metrics": { "latencyMs": 1275 },
        });
        let usage = Usage {
            input_tokens: 30,
            output_tokens: 62,
            ..Usage::default()
        };
        let id = "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q".to_owned();
        assert_eq!(
            parse_response(&body),
            vec![
                StreamEvent::UsageUpdate { usage },
                // Reasoning is told apart from the answer, or a client is shown
                // the model's scratchpad as its reply.
                StreamEvent::ThinkingDelta {
                    text: "WZPZ is a call sign.".to_owned()
                },
                StreamEvent::TextDelta {
                    text: "Let me look that up.".to_owned()
                },
                StreamEvent::ToolUseStart {
                    id: id.clone(),
                    name: "top_song".to_owned()
                },
                StreamEvent::ToolUseDelta {
                    id: id.clone(),
                    partial_json: r#"{"sign":"WZPZ"}"#.to_owned()
                },
                StreamEvent::ToolUseEnd { id },
                StreamEvent::Stop {
                    reason: StopReason::ToolUse,
                    usage
                },
            ]
        );
    }

    #[test]
    fn a_text_stream_reassembles_and_ends_with_its_bill() {
        let (events, acc) = drive(TEXT_STREAM);
        assert!(matches!(events.first(), Some(StreamEvent::Start { .. })));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, " Title: The");
        // A text block's stop ends no call.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolUseEnd { .. })),
            "{events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Stop {
                reason: StopReason::MaxTokens,
                usage: Usage {
                    input_tokens: 47,
                    output_tokens: 20,
                    ..Usage::default()
                },
            })
        );
        assert_eq!(acc.stop_reason(), Some(StopReason::MaxTokens));
    }

    #[test]
    fn a_streamed_tool_call_reassembles_from_its_partial_json() {
        let (events, acc) = drive(TOOL_STREAM);
        let id = "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q";

        let starts: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseStart { id, name } => Some((id.as_str(), name.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [(id, "top_song")]);

        // Converse addresses a fragment by block index. Every one has to find
        // the id the opening carried, or the arguments never reassemble.
        let mut input = String::new();
        for e in &events {
            if let StreamEvent::ToolUseDelta {
                id: to,
                partial_json,
            } = e
            {
                assert_eq!(to, id, "{events:?}");
                input.push_str(partial_json);
            }
        }
        assert_eq!(
            serde_json::from_str::<Value>(&input).expect("the fragments are whole JSON"),
            json!({ "sign": "WZPZ" })
        );
        let ends = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUseEnd { .. }))
            .count();
        assert_eq!(ends, 1, "one call, so one end: {events:?}");
        assert_eq!(acc.stop_reason(), Some(StopReason::ToolUse));
        assert_eq!(acc.quality_gate(), None, "a whole, valid call");

        // And through the hub: the call a client is handed, arguments parsed.
        let message = crate::anthropic::render_from_events(&events, "req_1", "m");
        assert_eq!(
            message["content"][1],
            json!({ "type": "tool_use", "id": id, "name": "top_song",
                    "input": { "sign": "WZPZ" } })
        );
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["usage"]["input_tokens"], 412);
    }

    #[test]
    fn a_tool_call_cut_short_trips_the_quality_gate() {
        // The fragment that closes the JSON never arrives: the classic
        // small-model failure, and the most valuable thing to escalate on.
        let mut lines = TOOL_STREAM.to_vec();
        lines.remove(6);
        let (_, acc) = drive(&lines);
        assert_eq!(
            acc.quality_gate(),
            Some(oag_router::QualityGate::MalformedToolCall)
        );
    }

    #[test]
    fn a_text_block_closing_after_a_tool_call_does_not_end_it_again() {
        // Every block ends with the same `contentBlockStop`, and the event
        // does not say what kind of block it closed. Ending "the last call
        // opened" would end this one twice, and a renderer that acts on the
        // end acts twice.
        let (events, _) = drive(&[
            TOOL_STREAM[3],
            TOOL_STREAM[4],
            TOOL_STREAM[5],
            TOOL_STREAM[6],
            TOOL_STREAM[7],
            r#"{"contentBlockDelta":{"delta":{"text":"Done."},"contentBlockIndex":2}}"#,
            r#"{"contentBlockStop":{"contentBlockIndex":2}}"#,
        ]);
        let ends = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUseEnd { .. }))
            .count();
        assert_eq!(ends, 1, "{events:?}");
    }

    #[test]
    fn the_stop_waits_for_the_usage_so_a_client_is_shown_the_bill() {
        // `messageStop` comes first and `metadata` after it. Every renderer
        // writes its terminal frame on the stop, so a stop passed on as it
        // arrived told every client the answer cost nothing.
        let mut acc = StreamAccumulator::new();
        let held =
            parse_event(r#"{"messageStop":{"stopReason":"end_turn"}}"#, &mut acc).expect("parses");
        assert!(held.is_empty(), "held until the usage arrives: {held:?}");
        assert_eq!(acc.stop_reason(), None, "and not complete until then");

        let events = parse_event(METADATA, &mut acc).expect("parses");
        let mut st = crate::anthropic::RenderState::new("req_1", "m");
        let frames: String = events
            .iter()
            .filter_map(|e| crate::anthropic::render_event(e, &mut st))
            .collect();
        let delta = frames
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(|l| serde_json::from_str::<Value>(l).expect("a frame"))
            .find(|f| f["type"] == "message_delta")
            .expect("a message_delta frame");
        assert_eq!(delta["delta"]["stop_reason"], "end_turn", "{delta}");
        assert_eq!(delta["usage"]["input_tokens"], 47, "{delta}");
        assert_eq!(delta["usage"]["output_tokens"], 20, "{delta}");

        // Usage with no stop held is an update, not a second stop.
        let events = parse_event(METADATA, &mut acc).expect("parses");
        assert!(
            matches!(events.as_slice(), [StreamEvent::UsageUpdate { .. }]),
            "{events:?}"
        );
    }

    #[test]
    fn streamed_reasoning_is_told_apart_from_the_answer_and_its_signature_is_not_text() {
        let (events, _) = drive(&[
            r#"{"contentBlockDelta":{"delta":{"reasoningContent":{"text":"The user wants "}},"contentBlockIndex":0}}"#,
            r#"{"contentBlockDelta":{"delta":{"reasoningContent":{"text":"a song."}},"contentBlockIndex":0}}"#,
            r#"{"contentBlockDelta":{"delta":{"reasoningContent":{"signature":"c2ln"}},"contentBlockIndex":0}}"#,
            r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
            r#"{"contentBlockDelta":{"delta":{"text":"Starman."},"contentBlockIndex":1}}"#,
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ThinkingDelta {
                    text: "The user wants ".to_owned()
                },
                StreamEvent::ThinkingDelta {
                    text: "a song.".to_owned()
                },
                StreamEvent::TextDelta {
                    text: "Starman.".to_owned()
                },
            ]
        );
    }

    #[test]
    fn an_exception_inside_the_stream_becomes_an_error_event_that_names_its_kind() {
        // The kind is the event's name on this wire, not a field in it, so
        // the message is the only place left to keep it.
        let mut acc = StreamAccumulator::new();
        let events = parse_event(
            r#"{"throttlingException":{"message":"Too many tokens, please wait before trying again."}}"#,
            &mut acc,
        )
        .expect("parses");
        assert_eq!(
            events,
            vec![StreamEvent::Error {
                message: "throttlingException: Too many tokens, please wait before trying again."
                    .to_owned()
            }]
        );

        let events = parse_event(r#"{"modelStreamErrorException":{}}"#, &mut acc).expect("parses");
        assert_eq!(
            events,
            vec![StreamEvent::Error {
                message: "modelStreamErrorException".to_owned()
            }]
        );
    }

    #[test]
    fn unknown_event_types_are_ignored_rather_than_fatal() {
        // AWS adds event types without warning. Failing the stream on one
        // would break every request the day they ship it.
        let mut acc = StreamAccumulator::new();
        let events = parse_event(r#"{"somethingNewIn2027":{"x":1}}"#, &mut acc);
        assert!(events.expect("must not error").is_empty());
        assert!(parse_event("not json", &mut acc).is_err());
    }

    /// What a model said in this dialect goes back to it unchanged on the
    /// next turn.
    ///
    /// The round trip an agent loop makes: a Converse answer out through the
    /// hub to a client (Anthropic-shaped here), and the client's copy of that
    /// turn back through the hub to Converse. The call's id, name and input
    /// have to survive both halves, or the result that follows answers a call
    /// the model never made.
    #[test]
    fn an_answer_round_trips_through_the_hub_into_the_next_request() {
        let said = json!({ "role": "assistant", "content": [
            { "text": "Let me look that up." },
            { "toolUse": { "toolUseId": "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q",
                           "name": "top_song", "input": { "sign": "WZPZ" } } },
        ]});
        let answer = crate::anthropic::render_from_events(
            &parse_response(&json!({
                "output": { "message": said },
                "stopReason": "tool_use",
                "usage": { "inputTokens": 30, "outputTokens": 62, "totalTokens": 92 },
            })),
            "req_1",
            "m",
        );

        let next = crate::anthropic::parse_request(&json!({
            "model": "m",
            "max_tokens": 1024,
            "messages": [
                { "role": "user", "content": "What is the most popular song on WZPZ?" },
                { "role": "assistant", "content": answer["content"] },
                { "role": "user", "content": [{ "type": "tool_result",
                    "tool_use_id": "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q", "content": "Elemental Hotel" }] },
            ],
        }))
        .expect("parses");
        let wire = render_request(&next).expect("renders");
        assert_eq!(wire["messages"][1], said, "the turn as the model wrote it");
        assert_eq!(
            wire["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q"
        );
    }
}
