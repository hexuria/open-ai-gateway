//! Pumping a streamed response from an upstream to a client.
//!
//! The shape that matters: the upstream is read by its **own task**, which
//! pushes to a bounded channel that the client's response body drains. Three
//! things fall out of that separation, and none of them work if you read the
//! upstream directly from the response body:
//!
//! - **Backpressure.** A slow client fills the channel and the reader parks.
//!   Without the bound, a slow client is an unbounded memory leak.
//! - **A slow client cannot stall the upstream read**, so the idle watchdog
//!   measures the upstream and not the client.
//! - **A client that disappears does not stop the accounting.** The provider
//!   has already committed to generating those tokens and will bill for them,
//!   so we keep draining and record what it cost. Stopping early would make
//!   every cancelled request free in our ledger and paid on the invoice.

use futures_util::StreamExt;
use oag_core::Error;
use oag_proto::{StreamAccumulator, StreamEvent};
use oag_upstream::{Framing, ProviderAdapter};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How the stream ended.
#[derive(Debug, Clone)]
pub struct StreamOutcome {
    pub accumulator: StreamAccumulator,
    /// Time to the first token of *content*.
    ///
    /// Measured from the first text or reasoning delta, not the first byte.
    /// `message_start` arrives immediately and carries no content, so timing to
    /// it reports ~0ms for a response the user waited a second to see begin —
    /// which makes the one latency number users actually feel useless.
    pub ttft: Option<Duration>,
    pub total: Duration,
    /// True when the client hung up before the upstream finished.
    pub client_gone: bool,
    /// Set when the upstream stalled or errored mid-stream.
    pub error: Option<String>,
}

/// What the client's response body receives.
pub type Chunk = std::result::Result<bytes::Bytes, std::io::Error>;

/// Renders canonical events into whichever dialect the client speaks.
///
/// One enum rather than a trait object: there are exactly as many variants as
/// there are dialects, and a match keeps the compiler responsible for noticing
/// when a new one is added.
enum Renderer {
    None,
    ChatCompletions(oag_proto::openai::RenderState),
    Anthropic(oag_proto::anthropic::RenderState),
    Gemini(oag_proto::gemini::RenderState),
    Responses(oag_proto::responses::RenderState),
}

impl Renderer {
    fn new(egress: &Egress) -> Self {
        match egress {
            Egress::Passthrough => Self::None,
            Egress::ChatCompletions { request_id, model } => {
                Self::ChatCompletions(oag_proto::openai::RenderState::new(request_id, model))
            }
            Egress::AnthropicMessages { request_id, model } => {
                Self::Anthropic(oag_proto::anthropic::RenderState::new(request_id, model))
            }
            Egress::Gemini => Self::Gemini(oag_proto::gemini::RenderState::new()),
            Egress::Responses { request_id, model } => {
                Self::Responses(oag_proto::responses::RenderState::new(request_id, model))
            }
        }
    }

    fn render(&mut self, event: &oag_proto::StreamEvent) -> Option<String> {
        match self {
            Self::None => None,
            Self::ChatCompletions(st) => oag_proto::openai::render_event(event, st),
            Self::Anthropic(st) => oag_proto::anthropic::render_event(event, st),
            Self::Gemini(st) => oag_proto::gemini::render_event(event, st),
            Self::Responses(st) => oag_proto::responses::render_event(event, st),
        }
    }
}

/// How the client's bytes are produced.
#[derive(Debug, Clone)]
pub enum Egress {
    /// Client and upstream speak the same dialect: forward bytes verbatim.
    ///
    /// Always preferred when it applies. We already hold bytes the upstream
    /// considered correct; re-serialising them can only introduce differences,
    /// and every difference is a client bug waiting to be blamed on us.
    Passthrough,
    /// They differ: render each canonical event into the client's dialect.
    ChatCompletions { request_id: String, model: String },
    /// The other direction: an Anthropic-shaped client over a Chat Completions
    /// upstream. The headline case, since it is what a Claude-shaped agent
    /// routed to a cheap model looks like.
    AnthropicMessages { request_id: String, model: String },
    /// A Gemini-shaped client over some other upstream.
    Gemini,
    /// A Responses-shaped client over some other upstream.
    Responses { request_id: String, model: String },
}

/// Read `response`, forward it to `tx`, and account for usage.
pub async fn pump(
    response: reqwest::Response,
    adapter: Arc<dyn ProviderAdapter>,
    tx: mpsc::Sender<Chunk>,
    idle_timeout: Duration,
    max_duration: Duration,
    egress: Egress,
) -> StreamOutcome {
    let started = Instant::now();
    let mut acc = StreamAccumulator::new();
    let mut ttft = None;
    let mut client_gone = false;
    let mut error = None;

    // Ask the adapter how this provider delimits events, once, up front.
    let framing = adapter.framing();
    let mut body = response.bytes_stream();
    // SSE frames can split across TCP reads; hold the tail until it completes.
    let mut pending = Vec::<u8>::new();

    let mut render = Renderer::new(&egress);

    loop {
        if started.elapsed() >= max_duration {
            error = Some(format!("stream exceeded {}s", max_duration.as_secs()));
            break;
        }

        let next = tokio::time::timeout(idle_timeout, body.next()).await;

        let chunk = match next {
            // The watchdog measures the *upstream*, so a slow client can never
            // trip it. A model thinking quietly still sends periodic events.
            Err(_) => {
                error = Some(format!("upstream idle for {}s", idle_timeout.as_secs()));
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                error = Some(format!("upstream read failed: {e}"));
                break;
            }
            Ok(Some(Ok(bytes))) => bytes,
        };

        // Account first, then forward. If the client is gone we still want the
        // usage, and this ordering means a send failure cannot skip it.
        pending.extend_from_slice(&chunk);
        let payloads = take_payloads(&mut pending, framing);
        let (saw_content, translated) = fold_payloads(&payloads, &adapter, &mut acc, &mut render);

        if ttft.is_none() && saw_content {
            ttft = Some(started.elapsed());
        }

        // Verbatim when the dialects match, translated bytes when they do not.
        //
        // A non-SSE upstream can never be passed through verbatim: the client
        // asked for SSE and Bedrock's binary envelope is not it. `egress_for`
        // guarantees a renderer for that case.
        let outbound = match &egress {
            Egress::Passthrough => chunk,
            _ => bytes::Bytes::from(translated),
        };

        if outbound.is_empty() {
            continue;
        }

        if !client_gone {
            if tx.send(Ok(outbound)).await.is_ok() {
                acc.mark_committed();
            } else {
                // Receiver dropped: the client hung up. Keep going — the
                // provider is generating (and billing for) these tokens either
                // way, and stopping here would make every cancelled request
                // free in our ledger and paid on the invoice.
                client_gone = true;
                tracing::debug!("client disconnected; draining upstream for accounting");
            }
        }
    }

    // Whatever is left is a partial frame; try once more in case it completed
    // exactly at EOF.
    if !pending.is_empty() {
        let payloads = take_payloads(&mut pending, framing);
        let _ = fold_payloads(&payloads, &adapter, &mut acc, &mut render);
    }

    // A translated stream has to synthesise the sentinel the client's dialect
    // expects. Anthropic has no equivalent, so without this a Chat Completions
    // client waits for a [DONE] that never arrives and hangs until its own
    // timeout — which looks exactly like a slow model.
    if !client_gone && matches!(egress, Egress::ChatCompletions { .. }) {
        let _ = tx
            .send(Ok(bytes::Bytes::from(oag_proto::openai::done_frame())))
            .await;
    }
    // Anthropic needs no sentinel: its stream ends with message_stop, which the
    // renderer already emitted.

    StreamOutcome {
        accumulator: acc,
        ttft,
        total: started.elapsed(),
        client_gone,
        error,
    }
}

/// Take every complete event payload from `buf`, leaving any partial tail.
///
/// The framing is the provider's, not a constant: Bedrock streams length-
/// prefixed binary messages rather than SSE, and a reader that assumes blank
/// lines finds nothing in one — an empty response and zero usage, with no error
/// anywhere.
fn take_payloads(buf: &mut Vec<u8>, framing: Framing) -> Vec<String> {
    match framing {
        Framing::Sse => take_sse_payloads(buf),
        Framing::AwsEventStream => oag_upstream::eventstream::take_messages(buf)
            .iter()
            .filter_map(oag_upstream::eventstream::inner_event)
            .collect(),
    }
}

/// The `data:` payloads of every complete SSE frame in `buf`.
fn take_sse_payloads(buf: &mut Vec<u8>) -> Vec<String> {
    // Frames are separated by a blank line. Anything after the last one is
    // incomplete and must wait for more bytes.
    let Some(idx) = buf.windows(2).rposition(|w| w == b"\n\n") else {
        return Vec::new();
    };
    let complete = buf[..=idx + 1].to_vec();
    buf.drain(..=idx + 1);

    let Ok(text) = std::str::from_utf8(&complete) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        // Some dialects terminate with a sentinel that is not JSON.
        .filter(|p| !p.is_empty() && *p != "[DONE]")
        .map(std::borrow::ToOwned::to_owned)
        .collect()
}

/// Parse the `data:` payloads out of complete frames and fold them in.
///
/// Returns whether any carried actual content — which is what times the first
/// token — and, when a renderer is supplied, the client-dialect bytes.
fn fold_payloads(
    payloads: &[String],
    adapter: &Arc<dyn ProviderAdapter>,
    acc: &mut StreamAccumulator,
    render: &mut Renderer,
) -> (bool, Vec<u8>) {
    let mut out = Vec::new();
    let mut saw_content = false;
    for payload in payloads {
        match adapter.parse_event(payload, acc) {
            Ok(events) => {
                for e in &events {
                    if matches!(
                        e,
                        StreamEvent::TextDelta { .. }
                            | StreamEvent::ThinkingDelta { .. }
                            | StreamEvent::ToolUseStart { .. }
                    ) {
                        saw_content = true;
                    }
                    acc.observe(e);
                    if let Some(frame) = render.render(e) {
                        out.extend_from_slice(frame.as_bytes());
                    }
                }
            }
            // A frame we cannot parse must not kill a stream that is otherwise
            // fine: at worst the usage is slightly under-counted, and the
            // client still gets a complete answer.
            Err(e) => tracing::debug!(error = %e, "skipping unparseable stream frame"),
        }
    }
    (saw_content, out)
}

/// Read a non-streaming response and fold it into an accumulator.
///
/// The accumulator must end up in the *same state* a streamed response would
/// have reached, because that is what `quality_gate` reads. Extracting only
/// usage would leave `text_len` at zero, and every non-streaming response would
/// then look like an empty one — so every single one would escalate.
pub async fn collect(
    response: reqwest::Response,
) -> std::result::Result<(bytes::Bytes, StreamAccumulator), Error> {
    let bytes = response
        .bytes()
        .await
        .map_err(|e| Error::Internal(format!("reading upstream response: {e}")))?;

    let mut acc = StreamAccumulator::new();
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok((bytes, acc));
    };

    acc.observe(&StreamEvent::UsageUpdate {
        usage: oag_router::Usage {
            input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: v["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0),
            cache_write_tokens: v["usage"]["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(0),
        },
    });

    // Replay the content as the events a stream would have produced.
    if let Some(blocks) = v["content"].as_array() {
        for block in blocks {
            match block["type"].as_str().unwrap_or_default() {
                "text" => acc.observe(&StreamEvent::TextDelta {
                    text: block["text"].as_str().unwrap_or_default().to_owned(),
                }),
                "thinking" => acc.observe(&StreamEvent::ThinkingDelta {
                    text: block["thinking"].as_str().unwrap_or_default().to_owned(),
                }),
                "tool_use" => {
                    let id = block["id"].as_str().unwrap_or_default().to_owned();
                    acc.observe(&StreamEvent::ToolUseStart {
                        id: id.clone(),
                        name: block["name"].as_str().unwrap_or_default().to_owned(),
                    });
                    // Whole, not fragmented — a non-streamed tool call is
                    // already complete JSON, so the malformed-arguments gate
                    // should never fire on one.
                    acc.observe(&StreamEvent::ToolUseDelta {
                        partial_json: block["input"].to_string(),
                        id: id.clone(),
                    });
                    acc.observe(&StreamEvent::ToolUseEnd { id });
                }
                _ => {}
            }
        }
    }

    if let Some(reason) = v["stop_reason"].as_str() {
        acc.observe(&StreamEvent::Stop {
            reason: match reason {
                "max_tokens" => oag_proto::StopReason::MaxTokens,
                "stop_sequence" => oag_proto::StopReason::StopSequence,
                "tool_use" => oag_proto::StopReason::ToolUse,
                "refusal" => oag_proto::StopReason::Refusal,
                _ => oag_proto::StopReason::EndTurn,
            },
            usage: *acc.usage(),
        });
    }

    Ok((bytes, acc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic() -> Arc<dyn ProviderAdapter> {
        Arc::new(oag_upstream::AnthropicAdapter::default())
    }

    #[test]
    fn only_complete_sse_frames_are_taken() {
        let mut buf = b"data: {\"a\":1}\n\ndata: {\"b\"".to_vec();
        let payloads = take_payloads(&mut buf, Framing::Sse);
        assert_eq!(payloads, vec![r#"{"a":1}"#]);
        assert_eq!(
            buf, b"data: {\"b\"",
            "the partial tail waits for more bytes"
        );
    }

    #[test]
    fn a_buffer_with_no_complete_frame_yields_nothing() {
        // The failure this prevents: parsing half a JSON object and discarding
        // the event, which loses the usage it carried.
        let mut buf = b"data: {\"partial".to_vec();
        assert!(take_payloads(&mut buf, Framing::Sse).is_empty());
        assert_eq!(buf, b"data: {\"partial");
    }

    #[test]
    fn several_frames_arriving_together_are_all_taken() {
        let mut buf = b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n".to_vec();
        assert_eq!(take_payloads(&mut buf, Framing::Sse).len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn a_done_sentinel_is_not_offered_as_a_payload() {
        let mut buf = b"data: [DONE]\n\n".to_vec();
        assert!(take_payloads(&mut buf, Framing::Sse).is_empty());
    }

    #[test]
    fn usage_accumulates_across_frames_split_mid_json() {
        // The realistic case: a TCP read boundary lands inside an event.
        let adapter = anthropic();
        let mut acc = StreamAccumulator::new();
        let mut render = Renderer::None;
        let mut buf = Vec::new();

        let whole = b"data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":50}}}\n\n";
        let (head, tail) = whole.split_at(40);

        buf.extend_from_slice(head);
        let p = take_payloads(&mut buf, Framing::Sse);
        let _ = fold_payloads(&p, &adapter, &mut acc, &mut render);
        assert_eq!(acc.usage().input_tokens, 0, "nothing complete yet");

        buf.extend_from_slice(tail);
        let p = take_payloads(&mut buf, Framing::Sse);
        let _ = fold_payloads(&p, &adapter, &mut acc, &mut render);
        assert_eq!(acc.usage().input_tokens, 50, "reassembled and counted");
    }

    #[test]
    fn ttft_is_timed_from_content_not_from_the_opening_event() {
        // message_start arrives immediately and carries no content. Timing to
        // it reports ~0ms for a response the user waited a second to see start.
        let adapter = anthropic();
        let mut acc = StreamAccumulator::new();
        let mut render = Renderer::None;

        let opening = vec![
            r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":5}}}"#
                .to_owned(),
        ];
        assert!(
            !fold_payloads(&opening, &adapter, &mut acc, &mut render).0,
            "an opening event is not content"
        );

        let content = vec![
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#
                .to_owned(),
        ];
        assert!(
            fold_payloads(&content, &adapter, &mut acc, &mut render).0,
            "a text delta is"
        );
    }

    #[test]
    fn a_tool_call_counts_as_content_for_ttft() {
        // A response that is only a tool call has no text, but the user is
        // still waiting for it and it is still the first thing to arrive.
        let adapter = anthropic();
        let mut acc = StreamAccumulator::new();
        let mut render = Renderer::None;
        let frame = vec![r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"f"}}"#.to_owned()];
        assert!(fold_payloads(&frame, &adapter, &mut acc, &mut render).0);
    }

    // ── framing ──────────────────────────────────────────────────────────────

    #[test]
    fn an_event_stream_upstream_is_decoded_not_split_on_blank_lines() {
        // The bug: a Bedrock stream read as SSE yields zero frames — an empty
        // response and zero recorded usage, with no error anywhere.
        let inner =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let framed = aws_frame(inner);

        let mut as_sse = framed.clone();
        assert!(
            take_payloads(&mut as_sse, Framing::Sse).is_empty(),
            "read as SSE, a binary frame yields nothing — which is the failure"
        );

        let mut as_stream = framed;
        let payloads = take_payloads(&mut as_stream, Framing::AwsEventStream);
        assert_eq!(payloads, vec![inner.to_owned()]);
    }

    #[test]
    fn an_event_stream_message_split_across_reads_waits_for_the_rest() {
        let whole = aws_frame(r#"{"type":"message_stop"}"#);
        let split = whole.len() / 2;

        let mut buf = whole[..split].to_vec();
        assert!(take_payloads(&mut buf, Framing::AwsEventStream).is_empty());
        buf.extend_from_slice(&whole[split..]);
        assert_eq!(take_payloads(&mut buf, Framing::AwsEventStream).len(), 1);
    }

    /// Build one AWS event-stream message carrying `inner`.
    fn aws_frame(inner: &str) -> Vec<u8> {
        use base64::Engine as _;
        let payload = serde_json::json!({
            "bytes": base64::engine::general_purpose::STANDARD.encode(inner)
        })
        .to_string()
        .into_bytes();

        let name = b":event-type";
        let mut headers = vec![u8::try_from(name.len()).expect("short")];
        headers.extend_from_slice(name);
        headers.push(7);
        headers.extend_from_slice(&5u16.to_be_bytes());
        headers.extend_from_slice(b"chunk");

        let total = 16 + headers.len() + payload.len();
        let mut out = Vec::new();
        out.extend_from_slice(&u32::try_from(total).expect("fits").to_be_bytes());
        out.extend_from_slice(&u32::try_from(headers.len()).expect("fits").to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(&payload);
        out.extend_from_slice(&0u32.to_be_bytes());
        out
    }
}
