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
use oag_upstream::ProviderAdapter;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How the stream ended.
#[derive(Debug)]
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

/// Read `response`, forward bytes to `tx`, and account for usage.
///
/// Bytes go through **verbatim**. When the client and the upstream speak the
/// same dialect there is nothing to translate, and re-serialising a stream we
/// already have correct bytes for can only introduce differences. Events are
/// parsed in parallel purely to accumulate usage.
pub async fn pump(
    response: reqwest::Response,
    adapter: Arc<dyn ProviderAdapter>,
    tx: mpsc::Sender<Chunk>,
    idle_timeout: Duration,
    max_duration: Duration,
) -> StreamOutcome {
    let started = Instant::now();
    let mut acc = StreamAccumulator::new();
    let mut ttft = None;
    let mut client_gone = false;
    let mut error = None;

    let mut body = response.bytes_stream();
    // SSE frames can split across TCP reads; hold the tail until it completes.
    let mut pending = Vec::<u8>::new();

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
        let frames = pending_take_complete(&mut pending);
        let saw_content = drain_frames(&frames, &adapter, &mut acc);

        if ttft.is_none() && saw_content {
            ttft = Some(started.elapsed());
        }

        if !client_gone {
            if tx.send(Ok(chunk)).await.is_ok() {
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

    // Whatever is left is a partial frame; parse it in case it completed at EOF.
    if !pending.is_empty() {
        let _ = drain_frames(&pending, &adapter, &mut acc);
    }

    StreamOutcome {
        accumulator: acc,
        ttft,
        total: started.elapsed(),
        client_gone,
        error,
    }
}

/// Split off every complete SSE frame, leaving any partial tail in `buf`.
fn pending_take_complete(buf: &mut Vec<u8>) -> Vec<u8> {
    // Frames are separated by a blank line. Anything after the last one is
    // incomplete and must wait for more bytes.
    match buf.windows(2).rposition(|w| w == b"\n\n") {
        Some(idx) => {
            let complete = buf[..=idx + 1].to_vec();
            buf.drain(..=idx + 1);
            complete
        }
        None => Vec::new(),
    }
}

/// Parse the `data:` payloads out of complete frames and fold them in.
///
/// Returns whether any of them carried actual content, which is what times
/// the first token.
fn drain_frames(
    bytes: &[u8],
    adapter: &Arc<dyn ProviderAdapter>,
    acc: &mut StreamAccumulator,
) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut saw_content = false;
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        // Some dialects terminate with a sentinel that is not JSON.
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
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
                }
            }
            // A frame we cannot parse must not kill a stream that is otherwise
            // fine: at worst the usage is slightly under-counted, and the
            // client still gets a complete answer.
            Err(e) => tracing::debug!(error = %e, "skipping unparseable stream frame"),
        }
    }
    saw_content
}

/// Read a non-streaming response, accounting for usage.
pub async fn collect(
    response: reqwest::Response,
) -> std::result::Result<(bytes::Bytes, StreamAccumulator), Error> {
    let bytes = response
        .bytes()
        .await
        .map_err(|e| Error::Internal(format!("reading upstream response: {e}")))?;

    let mut acc = StreamAccumulator::new();
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        let usage = oag_router::Usage {
            input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: v["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0),
            cache_write_tokens: v["usage"]["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(0),
        };
        acc.observe(&StreamEvent::UsageUpdate { usage });
    }
    Ok((bytes, acc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_complete_frames_are_taken() {
        let mut buf = b"data: {\"a\":1}\n\ndata: {\"b\"".to_vec();
        let complete = pending_take_complete(&mut buf);
        assert_eq!(complete, b"data: {\"a\":1}\n\n");
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
        assert!(pending_take_complete(&mut buf).is_empty());
        assert_eq!(buf, b"data: {\"partial");
    }

    #[test]
    fn several_frames_arriving_together_are_all_taken() {
        let mut buf = b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n".to_vec();
        let complete = pending_take_complete(&mut buf);
        assert_eq!(
            String::from_utf8_lossy(&complete).matches("data:").count(),
            2,
            "both frames should be taken in one pass"
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn usage_accumulates_across_frames_split_mid_json() {
        // The realistic case: a TCP read boundary lands inside an event.
        let adapter: Arc<dyn ProviderAdapter> = Arc::new(oag_upstream::AnthropicAdapter::default());
        let mut acc = StreamAccumulator::new();
        let mut buf = Vec::new();

        let whole = b"data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":50}}}\n\n";
        let (head, tail) = whole.split_at(40);

        buf.extend_from_slice(head);
        let _ = drain_frames(&pending_take_complete(&mut buf), &adapter, &mut acc);
        assert_eq!(acc.usage().input_tokens, 0, "nothing complete yet");

        buf.extend_from_slice(tail);
        let _ = drain_frames(&pending_take_complete(&mut buf), &adapter, &mut acc);
        assert_eq!(acc.usage().input_tokens, 50, "reassembled and counted");
    }

    #[test]
    fn ttft_is_timed_from_content_not_from_the_opening_event() {
        // message_start arrives immediately and carries no content. Timing to
        // it reports ~0ms for a response the user waited a second to see start.
        let adapter: Arc<dyn ProviderAdapter> = Arc::new(oag_upstream::AnthropicAdapter::default());
        let mut acc = StreamAccumulator::new();

        let opening = b"data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":5}}}\n\n";
        assert!(
            !drain_frames(opening, &adapter, &mut acc),
            "an opening event is not content"
        );

        let content = b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        assert!(drain_frames(content, &adapter, &mut acc), "a text delta is");
    }

    #[test]
    fn a_tool_call_counts_as_content_for_ttft() {
        // A response that is only a tool call has no text, but the user is
        // still waiting for it and it is still the first thing to arrive.
        let adapter: Arc<dyn ProviderAdapter> = Arc::new(oag_upstream::AnthropicAdapter::default());
        let mut acc = StreamAccumulator::new();
        let frame = b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\"}}\n\n";
        assert!(drain_frames(frame, &adapter, &mut acc));
    }

    #[test]
    fn a_done_sentinel_is_not_parsed_as_json() {
        let adapter: Arc<dyn ProviderAdapter> = Arc::new(oag_upstream::AnthropicAdapter::default());
        let mut acc = StreamAccumulator::new();
        let _ = drain_frames(b"data: [DONE]\n\n", &adapter, &mut acc);
        assert_eq!(acc.usage().total(), 0);
    }
}
