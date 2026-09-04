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
use oag_core::provider::Dialect;
use oag_proto::{StreamAccumulator, StreamEvent};
use oag_upstream::{Framing, ProviderAdapter};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// The most bytes we hold while waiting for one event to finish arriving.
///
/// The tail of the buffer is by definition a single incomplete frame, and the
/// largest real one is a tool call whose arguments are a big JSON document —
/// megabytes at the very outside. Past this it is not a large frame, it is an
/// upstream that will never send the delimiter, and continuing to buffer is an
/// unbounded allocation whose size a third party chooses. Failing the stream
/// keeps the memory bounded and tells the client why it stopped.
const MAX_PENDING: usize = 8 * 1024 * 1024;

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
            Egress::Passthrough { .. } => Self::None,
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

    /// A renderer for a dialect known only at runtime.
    ///
    /// Passthrough forwards the upstream's bytes and so needs no renderer while
    /// the stream is healthy. A failure still has to be reported in a dialect,
    /// and the client's is the upstream's — that is what made passthrough
    /// applicable in the first place.
    fn for_dialect(dialect: Dialect, request_id: &str, model: &str) -> Option<Self> {
        Some(match dialect {
            Dialect::OpenAIChatCompletions => {
                Self::ChatCompletions(oag_proto::openai::RenderState::new(request_id, model))
            }
            Dialect::AnthropicMessages => {
                Self::Anthropic(oag_proto::anthropic::RenderState::new(request_id, model))
            }
            Dialect::GeminiGenerateContent => Self::Gemini(oag_proto::gemini::RenderState::new()),
            Dialect::OpenAIResponses => {
                Self::Responses(oag_proto::responses::RenderState::new(request_id, model))
            }
            // `Dialect` is non-exhaustive. A dialect we cannot render cannot be
            // told anything, and silence beats bytes in the wrong shape.
            _ => return None,
        })
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
    ///
    /// The dialect is carried anyway, because a stream that fails mid-flight
    /// has to be told so in *some* dialect, and verbatim bytes end the moment
    /// there are no more of them.
    Passthrough {
        dialect: Dialect,
        request_id: String,
        model: String,
    },
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

/// The clocks a pump runs against.
///
/// Four, and each measures a different party: `idle` the upstream's silence,
/// `client_write` the client's, `max` the whole stream, and `keepalive` how
/// often our own silence is broken for intermediaries. They arrived one at a
/// time as four positional `Duration`s, which is how a caller swaps two and
/// gets a watchdog that never fires with no compile error to show for it.
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    /// How long the upstream may send nothing before the stream is failed.
    pub idle: Duration,
    /// Ceiling on the whole stream, checked on both sides of every await.
    pub max: Duration,
    /// How long a send to the client may wait before the client is given up.
    pub client_write: Duration,
    /// How often a no-op frame goes downstream while the upstream is quiet.
    pub keepalive: Duration,
}

/// Read `response`, forward it to `tx`, and account for usage.
///
/// Long, deliberately: this is one state machine over a handful of locals
/// that every branch reads, and slicing it into helpers would thread those
/// locals through five signatures to save the lint.
#[allow(clippy::too_many_lines)]
pub async fn pump(
    response: reqwest::Response,
    adapter: Arc<dyn ProviderAdapter>,
    tx: mpsc::Sender<Chunk>,
    deadlines: Deadlines,
    egress: Egress,
) -> StreamOutcome {
    let Deadlines {
        idle: idle_timeout,
        max: max_duration,
        client_write: client_write_timeout,
        keepalive: keepalive_interval,
    } = deadlines;
    let started = Instant::now();
    let mut acc = StreamAccumulator::new();
    let mut ttft = None;
    let mut client_gone = false;
    let mut error = None;

    // A no-op frame downstream while the upstream is quiet, so an intermediary
    // with its own idle timeout does not sever a stream the model is still
    // thinking on. Three Terraform modules, a Cloudflare precondition and
    // docs/04-cloud.md all described this as what keeps quiet streams alive;
    // until now no code emitted one. An SSE comment (`: keepalive`) is
    // discarded by every conforming parser and needs no per-dialect renderer.
    // Reset on every real chunk, so it only fires into silence.
    let mut keepalive = tokio::time::interval(keepalive_interval.max(Duration::from_millis(1)));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick of an interval is immediate; a keepalive before the first
    // byte would be noise, so it is consumed here.
    keepalive.tick().await;

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

        // The idle deadline is fixed before the wait, not re-armed per
        // keepalive tick: a keepalive is our silence, not the upstream's, and
        // must not keep resetting the watchdog that measures the upstream.
        // `StreamExt::next` is cancellation-safe, so dropping it to service a
        // tick loses nothing.
        let idle_deadline = tokio::time::Instant::now() + idle_timeout;
        let next = loop {
            tokio::select! {
                n = tokio::time::timeout_at(idle_deadline, body.next()) => break n,
                _ = keepalive.tick(), if !client_gone => {
                    // Not `mark_committed` on success: nothing of the answer
                    // went out. A refused or stalled send is a gone client,
                    // exactly as for a real chunk.
                    let frame = bytes::Bytes::from_static(b": keepalive\n\n");
                    let sent = tokio::time::timeout(client_write_timeout, tx.send(Ok(frame))).await;
                    if !matches!(sent, Ok(Ok(()))) {
                        client_gone = true;
                        tracing::debug!("client gone during keepalive; draining upstream for accounting");
                    }
                }
            }
        };
        keepalive.reset();

        let chunk = match next {
            // The watchdog measures the *upstream*, so a slow client can never
            // trip it. A model thinking quietly still sends periodic events.
            Err(_) => {
                error = Some(format!("upstream idle for {}s", idle_timeout.as_secs()));
                break;
            }
            // The body ended. Whether that was the end of the *answer* is not
            // knowable here — the last frame may still be in `pending` — so it
            // is judged after the loop, from the terminal event.
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
            Egress::Passthrough { .. } => chunk,
            _ => bytes::Bytes::from(translated),
        };

        if !outbound.is_empty() && !client_gone {
            // The channel is bounded, which bounds memory and not time: a
            // client that stops reading fills it and the send parks here —
            // past the idle watchdog, which measures the upstream, and past
            // the ceiling, which is checked at the top of a loop a parked
            // task never returns to. So the send gets its own deadline, and
            // a client that has read nothing for that long is treated as one
            // that hung up. Either way we keep going — the provider is
            // generating (and billing for) these tokens regardless, and
            // stopping here would make every abandoned request free in our
            // ledger and paid on the invoice.
            match tokio::time::timeout(client_write_timeout, tx.send(Ok(outbound))).await {
                Ok(Ok(())) => acc.mark_committed(),
                Ok(Err(_)) => {
                    client_gone = true;
                    tracing::debug!("client disconnected; draining upstream for accounting");
                }
                Err(_) => {
                    client_gone = true;
                    tracing::debug!(
                        timeout_s = client_write_timeout.as_secs(),
                        "client stopped reading; draining upstream for accounting"
                    );
                }
            }
            // The send may have been the long part. Hold the ceiling on this
            // side of the await too, or a slow reader stretches a stream past
            // it one chunk at a time.
            if started.elapsed() >= max_duration {
                error = Some(format!("stream exceeded {}s", max_duration.as_secs()));
                break;
            }
        }

        // Checked here rather than before framing, so a single large read full
        // of *complete* frames is not mistaken for one runaway frame: by this
        // point `pending` holds only the incomplete tail.
        if pending.len() > MAX_PENDING {
            error = Some(format!(
                "upstream sent {} bytes without completing an event",
                pending.len()
            ));
            pending.clear();
            break;
        }
    }

    // Whatever is left is a partial frame; try once more in case it completed
    // exactly at EOF.
    if !pending.is_empty() {
        let payloads = take_payloads(&mut pending, framing);
        let _ = fold_payloads(&payloads, &adapter, &mut acc, &mut render);
    }

    // Whether the response reached its own end. Every dialect marks that with a
    // terminal event, so this — and not how the read loop happened to exit — is
    // what decides whether the client is owed a sentinel or an error.
    //
    // Read after the tail fold, because the frame carrying the terminal event
    // can be the one that completes exactly at EOF.
    let complete = acc.stop_reason().is_some();

    // An upstream that closes cleanly before the terminal event has truncated
    // the answer, and nothing in the loop can tell: the read simply ends, the
    // same way it ends on success. Unnamed here, that is the whole bug — a
    // partial answer handed to the client stamped complete.
    if error.is_none() && !complete {
        error = Some("upstream closed before the response was complete".to_owned());
    }

    // A stream that died mid-flight has to say so on the wire. Otherwise the
    // client sees a truncated answer it has no way to distinguish from a short
    // one, or — for every dialect whose stream simply stops — waits for a
    // terminal event that is never coming and hangs until its own timeout,
    // while the ledger records the 502 nobody was told about.
    //
    // Only when the response never completed. A connection that drops *after*
    // the terminal event cost the client nothing, and an error appended to an
    // answer it has already seen end is a contradiction it has to resolve
    // alone.
    if !complete
        && !client_gone
        && let Some(message) = &error
        && let Some(frame) = error_frame(&egress, &mut render, message)
    {
        // Bounded like every other send: a client that stopped reading must
        // not hold the task on its own error frame either.
        let _ = tokio::time::timeout(client_write_timeout, tx.send(Ok(bytes::Bytes::from(frame))))
            .await;
    }

    // A translated stream has to synthesise the sentinel the client's dialect
    // expects. Anthropic has no equivalent, so without this a Chat Completions
    // client waits for a [DONE] that never arrives and hangs until its own
    // timeout — which looks exactly like a slow model.
    //
    // Gated on the response being complete rather than on the read having gone
    // well: `[DONE]` asserts that this answer is whole, which stays true after
    // a terminal event however the connection then behaved, and is false before
    // one however tidily the upstream hung up.
    if !client_gone && complete && matches!(egress, Egress::ChatCompletions { .. }) {
        let _ = tokio::time::timeout(
            client_write_timeout,
            tx.send(Ok(bytes::Bytes::from(oag_proto::openai::done_frame()))),
        )
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

/// The client-dialect frame that reports `message` as a stream failure.
///
/// `None` when there is no way to say it: a dialect with no renderer. Silence
/// is the honest outcome there — bytes in the wrong shape would be worse than
/// none, and the outcome still carries the error for the ledger.
fn error_frame(egress: &Egress, render: &mut Renderer, message: &str) -> Option<Vec<u8>> {
    let event = StreamEvent::Error {
        message: message.to_owned(),
    };
    match egress {
        Egress::Passthrough {
            dialect,
            request_id,
            model,
        } => Renderer::for_dialect(*dialect, request_id, model)?.render(&event),
        // Through the live renderer, not around it: it holds the state the
        // frame depends on — which items are open, whether the stream has
        // already been terminated.
        _ => render.render(&event),
    }
    .map(String::into_bytes)
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
    let Some(idx) = last_blank_line(buf) else {
        return Vec::new();
    };
    let complete = buf[..=idx].to_vec();
    buf.drain(..=idx);

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

/// The index of the final newline of the last blank line in `buf`.
///
/// The line break is LF or CRLF, per provider — the SSE grammar permits either
/// and real providers ship both. Matching only `\n\n` leaves a CRLF stream
/// buffered forever: no content, no usage, and no error to explain it, because
/// from the reader's point of view a frame simply never completed.
fn last_blank_line(buf: &[u8]) -> Option<usize> {
    // A blank line is one break immediately followed by another, so the byte
    // before this `\n` is either the previous break's `\n` or the `\r` of this
    // one's `\r\n` — in which case the `\n` before *that* is the break.
    (1..buf.len()).rev().find(|&i| {
        buf[i] == b'\n'
            && (buf[i - 1] == b'\n' || (i >= 2 && buf[i - 1] == b'\r' && buf[i - 2] == b'\n'))
    })
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
///
/// Which is why `dialect` is a parameter rather than an assumption. This read
/// Anthropic's field names whatever the upstream was, so a Chat Completions or
/// Gemini body yielded nothing at all: no tokens in the ledger, and an
/// empty-response gate that escalated a perfectly good answer onto the
/// expensive rung. It is the upstream's dialect, not the client's.
pub async fn collect(
    response: reqwest::Response,
    dialect: Dialect,
) -> std::result::Result<(bytes::Bytes, Vec<StreamEvent>, StreamAccumulator), Error> {
    let bytes = response
        .bytes()
        .await
        .map_err(|e| Error::Internal(format!("reading upstream response: {e}")))?;

    // A 200 whose body is not JSON is not an answer in any dialect. This used
    // to be waved through as "unparsed" and handed to the client verbatim —
    // which for a translated pair meant bytes in the wrong shape, and for a
    // same-dialect pair meant whatever the provider's front door emitted. An
    // error switches credentials, and says what happened.
    let v = serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| {
        Error::Internal(format!("a successful upstream response was not JSON: {e}"))
    })?;

    let events = match dialect {
        Dialect::AnthropicMessages => oag_proto::anthropic::parse_response(&v),
        Dialect::OpenAIChatCompletions => oag_proto::openai::parse_response(&v),
        Dialect::GeminiGenerateContent => oag_proto::gemini::parse_response(&v),
        Dialect::OpenAIResponses => oag_proto::responses::parse_response(&v),
        // `Dialect` is non-exhaustive. A dialect with no reader cannot be
        // rendered for the client either, so this is an error rather than an
        // unjudged pass: the comment that used to sit here — "no provider
        // declares Responses as its native dialect, so nothing reaches this"
        // — was false the day the Codex adapter existed, and the silent arm
        // beneath it is how a Codex seat's answers reached clients as raw
        // SSE under application/json.
        other => {
            return Err(Error::Internal(format!(
                "no non-streaming reader for the {other:?} dialect"
            )));
        }
    };

    let mut acc = StreamAccumulator::new();
    for e in &events {
        acc.observe(e);
    }

    Ok((bytes, events, acc))
}

/// Read a streamed upstream response to completion and fold it into events.
///
/// For an adapter that streams regardless of what the client asked for (see
/// `ProviderAdapter::always_streams`): the client wants one body, the upstream
/// only sends a stream, and the body has to be rendered from the events. This
/// is `pump` without a client to write to — the same framing, the same parser,
/// the same watchdogs — and it returns what `collect` returns for a JSON body,
/// so the caller need not care which it got.
pub async fn collect_stream(
    response: reqwest::Response,
    adapter: Arc<dyn ProviderAdapter>,
    idle_timeout: Duration,
    max_duration: Duration,
) -> std::result::Result<(Vec<StreamEvent>, StreamAccumulator), Error> {
    let started = Instant::now();
    let framing = adapter.framing();
    let mut body = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    let mut acc = StreamAccumulator::new();
    let mut events = Vec::new();

    loop {
        if started.elapsed() >= max_duration {
            return Err(Error::Internal(format!(
                "upstream stream exceeded {}s",
                max_duration.as_secs()
            )));
        }
        let chunk = match tokio::time::timeout(idle_timeout, body.next()).await {
            Err(_) => {
                return Err(Error::Internal(format!(
                    "upstream idle for {}s",
                    idle_timeout.as_secs()
                )));
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                return Err(Error::Internal(format!("upstream read failed: {e}")));
            }
            Ok(Some(Ok(bytes))) => bytes,
        };
        pending.extend_from_slice(&chunk);
        for payload in take_payloads(&mut pending, framing) {
            match adapter.parse_event(&payload, &mut acc) {
                Ok(parsed) => {
                    for e in &parsed {
                        acc.observe(e);
                    }
                    events.extend(parsed);
                }
                Err(e) => tracing::debug!(error = %e, "skipping unparseable stream frame"),
            }
        }
    }

    // An error the upstream put inside a 200 stream is an error, not an
    // answer: with no client holding the bytes yet, another credential can
    // still be tried.
    if let Some(message) = events.iter().find_map(|e| match e {
        StreamEvent::Error { message } => Some(message.clone()),
        _ => None,
    }) {
        return Err(Error::Internal(format!("upstream stream error: {message}")));
    }

    Ok((events, acc))
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
    fn crlf_blank_line_completes_a_frame() {
        // The SSE grammar allows either line break and providers ship both.
        // Matching only "\n\n" held a CRLF stream in `pending` forever: no
        // content, no usage, and no error to explain it, because from the
        // reader's side a frame simply never completed.
        let mut buf = b"data: {\"a\":1}\r\n\r\ndata: {\"b\"".to_vec();
        assert_eq!(take_payloads(&mut buf, Framing::Sse), vec![r#"{"a":1}"#]);
        assert_eq!(buf, b"data: {\"b\"", "the partial tail still waits");

        // Named events, which are CRLF's usual company, and a mixed pair.
        let mut buf =
            b"event: x\r\ndata: {\"a\":1}\r\n\r\nevent: y\ndata: {\"b\":2}\r\n\n".to_vec();
        assert_eq!(
            take_payloads(&mut buf, Framing::Sse),
            vec![r#"{"a":1}"#, r#"{"b":2}"#]
        );
        assert!(buf.is_empty());
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

    // ── how a stream ends ────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_quiet_upstream_gets_keepalives_downstream_until_the_watchdog_fires() {
        // The upstream sends one frame and then nothing. The client must see
        // comment frames at the keepalive interval — the thing every deploy
        // artefact promised and nothing emitted — and the idle watchdog must
        // still fire on the UPSTREAM's silence, undisturbed by our own.
        let (tx, mut rx) = mpsc::channel(64);
        let outcome = pump(
            stalling(vec![sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            )]),
            anthropic(),
            tx,
            Deadlines {
                idle: Duration::from_millis(250),
                max: Duration::from_secs(30),
                client_write: Duration::from_secs(5),
                keepalive: Duration::from_millis(40),
            },
            Egress::Passthrough {
                dialect: Dialect::AnthropicMessages,
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(
            outcome.error.is_some(),
            "the idle watchdog fired on the upstream"
        );
        let sent = drain(&mut rx).await;
        let keepalives = sent.matches(": keepalive\n\n").count();
        assert!(
            keepalives >= 3,
            "expected several keepalives across 250ms of silence at 40ms; got {keepalives}: {sent:?}"
        );
        assert!(sent.contains("hi"), "the real frame went out too");
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_does_not_park_the_pump() {
        // A receiver that is alive but never polls. With a channel of one and
        // several chunks to send, the second send would have blocked for as
        // long as the client kept the connection open — past every watchdog,
        // holding the lease, the socket and the shutdown guard. Now it waits
        // the write deadline, gives the client up, and drains the upstream
        // so the usage still reaches the ledger.
        let (tx, _rx_never_read) = mpsc::channel(1);
        let frames = vec![
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"one "}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"two "}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"three"}}"#,
            ),
            sse(
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            ),
        ];
        let started = std::time::Instant::now();
        let outcome = pump(
            streamed(frames),
            anthropic(),
            tx,
            Deadlines {
                idle: Duration::from_secs(5),
                max: Duration::from_secs(30),
                client_write: Duration::from_millis(50),
                keepalive: Duration::from_secs(10),
            },
            Egress::AnthropicMessages {
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(outcome.client_gone, "a reader that never reads is gone");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "returned in {:?}: bounded by the write deadline, not the client",
            started.elapsed()
        );
        assert_eq!(
            outcome.accumulator.usage().output_tokens,
            3,
            "the upstream was drained to the end for accounting"
        );
        assert!(
            outcome.error.is_none(),
            "the upstream itself was fine: {:?}",
            outcome.error
        );
    }

    #[tokio::test]
    async fn idle_timeout_emits_dialect_error_not_done() {
        // The bug: an abnormal exit was a bare `break`, so it fell through to
        // the unconditional `[DONE]`. A stream that died mid-answer told the
        // client it had finished normally, while the ledger recorded the 502
        // nobody was informed of.
        let (tx, mut rx) = mpsc::channel(16);
        let outcome = pump(
            stalling(vec![sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an "}}"#,
            )]),
            anthropic(),
            tx,
            Deadlines {
                idle: Duration::from_millis(50),
                max: Duration::from_secs(30),
                client_write: Duration::from_secs(5),
                keepalive: Duration::from_secs(10),
            },
            Egress::ChatCompletions {
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(outcome.error.is_some(), "the idle watchdog fired");
        let sent = drain(&mut rx).await;
        assert!(sent.contains("half an "), "the partial answer went out");
        assert!(
            !sent.contains("[DONE]"),
            "a failed stream must not report success: {sent}"
        );

        let last = sent.trim_end().rsplit("data: ").next().expect("a frame");
        let v: serde_json::Value = serde_json::from_str(last).expect("the last frame is JSON");
        assert!(
            v["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("idle")),
            "the frame names the failure: {last}"
        );
    }

    #[tokio::test]
    async fn clean_eof_without_a_terminal_event_is_a_failure() {
        // The quiet half of the same bug: the upstream closes the connection
        // tidily, mid-answer. The read loop cannot tell that from success — it
        // just ends — so it fell through to [DONE] and stamped a truncated
        // answer complete, which is the one failure a client cannot detect.
        let (tx, mut rx) = mpsc::channel(16);
        let outcome = pump(
            streamed(vec![sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an "}}"#,
            )]),
            anthropic(),
            tx,
            Deadlines {
                idle: Duration::from_secs(5),
                max: Duration::from_secs(30),
                client_write: Duration::from_secs(5),
                keepalive: Duration::from_secs(10),
            },
            Egress::ChatCompletions {
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(
            outcome.error.is_some(),
            "a stream with no terminal event did not succeed"
        );
        let sent = drain(&mut rx).await;
        assert!(
            !sent.contains("[DONE]"),
            "a truncated answer must not claim to be whole: {sent}"
        );
        assert!(
            sent.contains("upstream_error"),
            "the client is told: {sent}"
        );
    }

    #[tokio::test]
    async fn a_truncated_stream_ends_with_an_error_in_anthropic_s_dialect() {
        // Same cause, and the dialect with no sentinel to get wrong: here the
        // stream simply stopped, so the client waited for message_stop until its
        // own timeout.
        let (tx, mut rx) = mpsc::channel(16);
        let outcome = pump(
            streamed(vec![sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an "}}"#,
            )]),
            anthropic(),
            tx,
            Deadlines {
                idle: Duration::from_secs(5),
                max: Duration::from_secs(30),
                client_write: Duration::from_secs(5),
                keepalive: Duration::from_secs(10),
            },
            Egress::AnthropicMessages {
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(outcome.error.is_some());
        let sent = drain(&mut rx).await;
        assert!(sent.contains("event: error"), "silence is the bug: {sent}");
        assert!(
            !sent.contains("message_stop"),
            "and this answer did not end normally: {sent}"
        );
    }

    #[tokio::test]
    async fn a_read_error_after_the_terminal_event_still_completes_the_stream() {
        // The inverse, and a regression the error frame introduced: the answer
        // was whole and the connection dropped on the way out. Treating that as
        // a stream failure appends an error to a response the client has
        // already seen end, and withholds the sentinel it is waiting for.
        let (tx, mut rx) = mpsc::channel(16);
        let outcome = pump(
            failing(vec![sse(
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            )]),
            anthropic(),
            tx,
            Deadlines {
                idle: Duration::from_secs(5),
                max: Duration::from_secs(30),
                client_write: Duration::from_secs(5),
                keepalive: Duration::from_secs(10),
            },
            Egress::ChatCompletions {
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(
            outcome.error.is_some(),
            "the transport failure is still recorded"
        );
        let sent = drain(&mut rx).await;
        assert!(
            sent.contains("[DONE]"),
            "a complete answer keeps its sentinel: {sent}"
        );
        assert!(
            !sent.contains("upstream_error"),
            "and gains no frame contradicting it: {sent}"
        );
    }

    #[tokio::test]
    async fn a_clean_stream_still_ends_with_done() {
        // The other half of the gate: withholding [DONE] on failure must not
        // withhold it on success, or every healthy translated stream hangs.
        let (tx, mut rx) = mpsc::channel(16);
        let outcome = pump(
            streamed(vec![sse(
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            )]),
            anthropic(),
            tx,
            Deadlines {
                idle: Duration::from_secs(5),
                max: Duration::from_secs(30),
                client_write: Duration::from_secs(5),
                keepalive: Duration::from_secs(10),
            },
            Egress::ChatCompletions {
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(outcome.error.is_none());
        assert!(drain(&mut rx).await.contains("[DONE]"));
    }

    #[tokio::test]
    async fn a_passthrough_client_is_told_the_stream_failed() {
        // Passthrough forwards bytes and so has no renderer, which used to mean
        // a failure was reported to the client as nothing at all: the stream
        // just stopped, and the client waited for message_stop until its own
        // timeout.
        let (tx, mut rx) = mpsc::channel(16);
        let outcome = pump(
            stalling(vec![sse(r#"{"type":"ping"}"#)]),
            anthropic(),
            tx,
            Deadlines {
                idle: Duration::from_millis(50),
                max: Duration::from_secs(30),
                client_write: Duration::from_secs(5),
                keepalive: Duration::from_secs(10),
            },
            Egress::Passthrough {
                dialect: Dialect::AnthropicMessages,
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(outcome.error.is_some());
        let sent = drain(&mut rx).await;
        assert!(
            sent.contains("event: error") && sent.contains(r#""type":"error""#),
            "the client's own dialect says so: {sent}"
        );
    }

    #[tokio::test]
    async fn sse_pending_is_capped() {
        // An upstream that never sends the delimiter used to be an unbounded
        // allocation: `pending` grew for as long as bytes kept arriving, to
        // whatever size the upstream chose.
        let mib = bytes::Bytes::from(vec![b'x'; 1024 * 1024]);
        let chunks = vec![mib; MAX_PENDING / (1024 * 1024) + 1];

        let (tx, mut rx) = mpsc::channel(16);
        let outcome = pump(
            stalling(chunks),
            anthropic(),
            tx,
            // Long enough that neither watchdog can be what stops this: the cap
            // trips within milliseconds of the bytes arriving.
            Deadlines {
                idle: Duration::from_secs(5),
                max: Duration::from_secs(5),
                client_write: Duration::from_secs(5),
                keepalive: Duration::from_secs(10),
            },
            Egress::ChatCompletions {
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
            },
        )
        .await;

        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains("without completing an event")),
            "the overflow, not the watchdog, ended it: {:?}",
            outcome.error
        );
        let sent = drain(&mut rx).await;
        assert!(!sent.contains("[DONE]"));
        assert!(
            sent.contains("without completing an event"),
            "the client is told why it stopped: {sent}"
        );
    }

    /// One SSE frame carrying `payload`.
    fn sse(payload: &str) -> bytes::Bytes {
        bytes::Bytes::from(format!("data: {payload}\n\n"))
    }

    /// A streaming response body over `chunks`, ending cleanly after them.
    fn streamed(chunks: Vec<bytes::Bytes>) -> reqwest::Response {
        into_response(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::io::Error>),
        ))
    }

    /// The same, but the body fails after the last chunk — what a connection
    /// reset partway through looks like from here.
    fn failing(chunks: Vec<bytes::Bytes>) -> reqwest::Response {
        into_response(
            futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)).chain(
                futures_util::stream::once(async {
                    Err(std::io::Error::other("connection reset by peer"))
                }),
            ),
        )
    }

    /// The same, but the body hangs after the last chunk rather than ending —
    /// which is what an upstream that stops sending looks like from here.
    fn stalling(chunks: Vec<bytes::Bytes>) -> reqwest::Response {
        into_response(
            futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)).chain(
                futures_util::stream::once(async {
                    std::future::pending::<()>().await;
                    Ok(bytes::Bytes::new())
                }),
            ),
        )
    }

    fn into_response<S>(stream: S) -> reqwest::Response
    where
        S: futures_util::stream::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static,
    {
        reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(stream)))
    }

    /// Everything the client's response body received, as text.
    async fn drain(rx: &mut mpsc::Receiver<Chunk>) -> String {
        let mut out = Vec::new();
        while let Some(chunk) = rx.recv().await {
            out.extend_from_slice(&chunk.expect("no io error on this path"));
        }
        String::from_utf8(out).expect("utf8")
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

    // ── non-streamed bodies ──────────────────────────────────────────────────
    //
    // Every one of these used to be read with Anthropic's field names, whatever
    // the upstream was. A Chat Completions or Gemini body therefore yielded
    // nothing at all: no tokens for the ledger, and an accumulator that looked
    // like an empty response — so a perfectly good answer tripped the gate and
    // escalated onto the expensive rung.

    /// A complete upstream response, as `collect` receives one.
    fn body(json: &str) -> reqwest::Response {
        reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(json.to_owned())
                .expect("response"),
        )
    }

    #[tokio::test]
    async fn collect_parses_openai_usage_and_text() {
        let (_, _, acc) = collect(
            body(
                r#"{
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "the answer is 4" },
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens": 1200,
                        "completion_tokens": 142,
                        "prompt_tokens_details": { "cached_tokens": 200 }
                    }
                }"#,
            ),
            Dialect::OpenAIChatCompletions,
        )
        .await
        .expect("collects");

        // 1200 total prompt less the 200 served from cache, which is priced
        // separately rather than counted twice.
        assert_eq!(acc.usage().input_tokens, 1_000);
        assert_eq!(acc.usage().cache_read_tokens, 200);
        assert_eq!(acc.usage().output_tokens, 142);
        assert_eq!(
            acc.quality_gate(),
            None,
            "an answer with text in it must not escalate"
        );
    }

    #[tokio::test]
    async fn collect_parses_gemini_usage_and_text() {
        let (_, _, acc) = collect(
            body(
                r#"{
                    "candidates": [{
                        "content": { "role": "model", "parts": [{ "text": "the answer is 4" }] },
                        "finishReason": "STOP"
                    }],
                    "usageMetadata": {
                        "promptTokenCount": 1200,
                        "cachedContentTokenCount": 200,
                        "candidatesTokenCount": 142
                    }
                }"#,
            ),
            Dialect::GeminiGenerateContent,
        )
        .await
        .expect("collects");

        assert_eq!(acc.usage().input_tokens, 1_000);
        assert_eq!(acc.usage().cache_read_tokens, 200);
        assert_eq!(acc.usage().output_tokens, 142);
        assert_eq!(acc.quality_gate(), None);
    }

    #[tokio::test]
    async fn collect_still_parses_anthropic_usage_and_text() {
        let (_, _, acc) = collect(
            body(
                r#"{
                    "content": [{ "type": "text", "text": "the answer is 4" }],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1000,
                        "output_tokens": 142,
                        "cache_read_input_tokens": 200,
                        "cache_creation_input_tokens": 300
                    }
                }"#,
            ),
            Dialect::AnthropicMessages,
        )
        .await
        .expect("collects");

        assert_eq!(acc.usage().input_tokens, 1_000);
        assert_eq!(acc.usage().cache_read_tokens, 200);
        assert_eq!(acc.usage().cache_write_tokens, 300);
        assert_eq!(acc.usage().output_tokens, 142);
        assert_eq!(acc.quality_gate(), None);
    }

    #[tokio::test]
    async fn a_tool_only_openai_answer_is_neither_empty_nor_malformed() {
        // The agentic case: no text at all, and arguments that arrive whole
        // rather than in fragments. Read as Anthropic this was an empty
        // response; read as fragments it would look like a truncated tool call.
        let (_, _, acc) = collect(
            body(
                r#"{
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": "{\"path\": \"src/main.rs\"}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }],
                    "usage": { "prompt_tokens": 50, "completion_tokens": 12 }
                }"#,
            ),
            Dialect::OpenAIChatCompletions,
        )
        .await
        .expect("collects");

        assert_eq!(acc.usage().output_tokens, 12);
        assert_eq!(acc.quality_gate(), None);
    }

    #[tokio::test]
    async fn a_responses_body_is_read_like_every_other() {
        // This test used to assert the opposite: that a Responses body had no
        // reader and was passed on unjudged, on the grounds that "nothing
        // serves Responses as an upstream today". The Codex adapter does, and
        // an unjudged pass is exactly how its answers reached clients as raw
        // bytes with zero tokens in the ledger.
        let (_, events, acc) = collect(
            body(
                r#"{
                    "status": "completed",
                    "output": [{
                        "type": "message", "role": "assistant",
                        "content": [{ "type": "output_text", "text": "the answer is 4" }]
                    }],
                    "usage": { "input_tokens": 1200, "output_tokens": 142,
                               "input_tokens_details": { "cached_tokens": 200 } }
                }"#,
            ),
            Dialect::OpenAIResponses,
        )
        .await
        .expect("collects");

        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { text } if text == "the answer is 4"))
        );
        assert_eq!(acc.usage().input_tokens, 1_000);
        assert_eq!(acc.usage().cache_read_tokens, 200);
        assert_eq!(acc.usage().output_tokens, 142);
        assert_eq!(acc.quality_gate(), None);
    }

    #[tokio::test]
    async fn a_successful_response_that_is_not_json_is_an_error_not_an_answer() {
        let err = collect(
            body("<html>rate limited</html>"),
            Dialect::AnthropicMessages,
        )
        .await
        .expect_err("not an answer");
        assert!(err.to_string().contains("not JSON"), "{err}");
    }

    #[tokio::test]
    async fn an_always_streaming_adapter_is_read_to_completion_into_events() {
        // THE CODEX CASE. The client asked for one body; the adapter forced a
        // stream. Reading that stream as a JSON body found nothing and handed
        // the raw `data:` lines back under application/json, with zero tokens
        // metered. Read as the stream it is, the events carry the text, the
        // whole tool call, and the usage the seat will be charged for.
        let frames = [
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
        let sse = frames.iter().fold(String::new(), |mut out, f| {
            use std::fmt::Write as _;
            let _ = write!(out, "data: {f}\n\n");
            out
        });
        let codex: Arc<dyn ProviderAdapter> = Arc::new(oag_upstream::codex::CodexAdapter::new());
        assert!(codex.always_streams());

        let (events, acc) = collect_stream(
            body(&sse),
            codex,
            Duration::from_secs(5),
            Duration::from_secs(30),
        )
        .await
        .expect("collects the stream");

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Let me check.");
        assert_eq!(acc.usage().input_tokens, 1_200);
        assert_eq!(acc.usage().cache_read_tokens, 18_000);
        assert_eq!(
            acc.usage().output_tokens,
            142,
            "the seat is charged for these"
        );
        assert_eq!(acc.quality_gate(), None, "the tool call reassembled");
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
