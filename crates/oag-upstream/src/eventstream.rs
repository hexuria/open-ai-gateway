//! AWS `vnd.amazon.eventstream` framing.
//!
//! Bedrock does not stream server-sent events. It streams a binary framing
//! format, and a reader that splits on blank lines finds nothing in it — which
//! is exactly what a Bedrock stream did here before this file existed: zero
//! frames, an empty response, and zero recorded usage.
//!
//! One message:
//!
//! ```text
//!   0  total_length    u32 be
//!   4  headers_length  u32 be
//!   8  prelude_crc     u32 be
//!  12  headers         headers_length bytes
//!      payload         total_length - headers_length - 16 bytes
//!      message_crc     u32 be
//! ```
//!
//! Bedrock's payload is JSON of the form `{"bytes": "<base64>"}`, and the
//! base64 decodes to the provider's own event — Anthropic's, for a Claude
//! model. So the useful output of this module is that inner JSON.

use base64::Engine as _;

/// Bytes before the headers begin: three `u32` fields.
const PRELUDE_LEN: usize = 12;
/// The prelude plus the trailing message CRC.
const OVERHEAD: usize = PRELUDE_LEN + 4;
/// The largest message the format allows.
///
/// `total_length` is a `u32` read straight off the wire, so a corrupt or
/// hostile prelude can claim four gigabytes and the caller will keep buffering
/// until it arrives. The spec caps a message at 16 MiB, so anything larger is
/// not a big message — it is a bad length, and the same corruption the
/// undersized case already refuses to resynchronise on.
const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// A message's headers, as far as we care about them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Headers {
    /// `:event-type` — `chunk` for content, or an exception name.
    pub event_type: Option<String>,
    /// `:exception-type`, when the frame reports a failure rather than content.
    pub exception_type: Option<String>,
}

/// One decoded message.
#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub headers: Headers,
    pub payload: Vec<u8>,
}

/// Take every complete message from `buf`, leaving any partial tail behind.
///
/// A partial tail is the normal case, not an error: a message can and does
/// straddle a TCP read.
pub fn take_messages(buf: &mut Vec<u8>) -> Vec<Message> {
    let mut out = Vec::new();
    let mut consumed = 0usize;

    loop {
        let rest = &buf[consumed..];
        if rest.len() < PRELUDE_LEN {
            break;
        }

        let total = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let headers_len = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;

        // A frame claiming to be smaller than its own overhead, larger than the
        // format permits, or with more headers than it has bytes, is corrupt.
        // Stopping is the only safe move: advancing by a bogus length would
        // resynchronise on garbage, and waiting for a length no real message
        // has would buffer until the process runs out of memory.
        if !(OVERHEAD..=MAX_MESSAGE_LEN).contains(&total) || headers_len > total - OVERHEAD {
            tracing::warn!(
                total,
                headers_len,
                "malformed event-stream prelude; stopping"
            );
            break;
        }
        if rest.len() < total {
            // The rest of this message has not arrived yet.
            break;
        }

        let headers = parse_headers(&rest[PRELUDE_LEN..PRELUDE_LEN + headers_len]);
        let payload = rest[PRELUDE_LEN + headers_len..total - 4].to_vec();
        out.push(Message { headers, payload });
        consumed += total;
    }

    buf.drain(..consumed);
    out
}

/// Parse the header block, keeping only the two headers that matter.
///
/// Header values come in nine types; only string (7) carries anything we read,
/// but every type has to be *skipped* correctly or the parse desynchronises and
/// the rest of the block is garbage.
fn parse_headers(mut b: &[u8]) -> Headers {
    let mut headers = Headers::default();

    while b.len() >= 2 {
        let name_len = b[0] as usize;
        if b.len() < 1 + name_len + 1 {
            break;
        }
        let name = String::from_utf8_lossy(&b[1..=name_len]).into_owned();
        let value_type = b[1 + name_len];
        b = &b[1 + name_len + 1..];

        let value: Option<String> = match value_type {
            // bool true / bool false: no value bytes.
            0..=1 => None,
            // byte
            2 => {
                if b.is_empty() {
                    break;
                }
                b = &b[1..];
                None
            }
            // short, integer, long
            3..=5 => {
                let n = match value_type {
                    3 => 2,
                    4 => 4,
                    _ => 8,
                };
                if b.len() < n {
                    break;
                }
                b = &b[n..];
                None
            }
            // byte array (6) and string (7): u16 length prefix.
            6..=7 => {
                if b.len() < 2 {
                    break;
                }
                let len = u16::from_be_bytes([b[0], b[1]]) as usize;
                if b.len() < 2 + len {
                    break;
                }
                let v =
                    (value_type == 7).then(|| String::from_utf8_lossy(&b[2..2 + len]).into_owned());
                b = &b[2 + len..];
                v
            }
            // timestamp
            8 => {
                if b.len() < 8 {
                    break;
                }
                b = &b[8..];
                None
            }
            // uuid
            9 => {
                if b.len() < 16 {
                    break;
                }
                b = &b[16..];
                None
            }
            _ => break,
        };

        match name.as_str() {
            ":event-type" => headers.event_type = value,
            ":exception-type" => headers.exception_type = value,
            _ => {}
        }
    }

    headers
}

/// An AWS exception frame, rewritten as the dialect's own error event.
///
/// The old code returned the body verbatim, and the doc claimed that surfaced
/// the message. It did not: the caller passes this to `anthropic::parse_event`,
/// whose dispatch is on `v["type"]`, and an AWS exception payload is
/// `{"message":"…"}` with no `type` at all. It fell to the `_ => vec![]` arm and
/// was erased — so a stream that emitted content deltas and then a
/// `throttlingException` reached the client as HTTP 200 with a half-finished
/// answer and no error. The credential was never cooled down, the breaker
/// recorded nothing, and the ledger charged for the partial generation.
///
/// Rewritten here because `exception_type` is in hand here and nowhere
/// afterwards: the kind is a *header* on the envelope rather than a field in the
/// body, so anything downstream would be guessing at what sort of failure it was
/// even if it noticed there had been one.
fn exception_event(kind: &str, payload: &[u8]) -> String {
    // Lossy, not strict. A frame whose bytes are not valid UTF-8 is still an
    // exception, and refusing it puts us back in the silent stall — with the
    // added insult that the provider had said what was wrong. Latin-1 through
    // `char::from`, which is what this used to do, mangles every multi-byte
    // character in a message an operator is meant to read.
    let body = String::from_utf8_lossy(payload);
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.into_owned());

    serde_json::json!({
        "type": "error",
        "error": { "type": kind, "message": message },
    })
    .to_string()
}

/// The provider's own event JSON, unwrapped from Bedrock's envelope.
///
/// Returns `None` for a frame that carries no inner event — a heartbeat, or a
/// payload shaped differently from what we expect. An exception frame is
/// rewritten into the dialect's own error event so the caller can surface the
/// message rather than a silent stall.
#[must_use]
pub fn inner_event(msg: &Message) -> Option<String> {
    // The exception check comes first, before the payload is required to be
    // JSON. The envelope header is what says this is an exception, and a body
    // we cannot parse is still one — asking `serde_json` for permission first
    // put an unreadable exception back into the silent stall this exists to
    // prevent.
    if let Some(kind) = msg.headers.exception_type.as_deref() {
        return Some(exception_event(kind, &msg.payload));
    }

    let v: serde_json::Value = serde_json::from_slice(&msg.payload).ok()?;

    if let Some(encoded) = v["bytes"].as_str() {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        return String::from_utf8(decoded).ok();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a message the way Bedrock does, so the tests exercise the real
    /// layout rather than a convenient one.
    fn frame(event_type: &str, inner: &str) -> Vec<u8> {
        let payload = serde_json::json!({
            "bytes": base64::engine::general_purpose::STANDARD.encode(inner)
        })
        .to_string()
        .into_bytes();

        // `:event-type` as a string header.
        let name = b":event-type";
        let mut headers = Vec::new();
        headers.push(u8::try_from(name.len()).expect("short name"));
        headers.extend_from_slice(name);
        headers.push(7); // string
        headers.extend_from_slice(
            &u16::try_from(event_type.len())
                .expect("short")
                .to_be_bytes(),
        );
        headers.extend_from_slice(event_type.as_bytes());

        let total = OVERHEAD + headers.len() + payload.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&u32::try_from(total).expect("fits").to_be_bytes());
        out.extend_from_slice(&u32::try_from(headers.len()).expect("fits").to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // prelude crc, unchecked
        out.extend_from_slice(&headers);
        out.extend_from_slice(&payload);
        out.extend_from_slice(&0u32.to_be_bytes()); // message crc, unchecked
        out
    }

    #[test]
    fn one_message_decodes_to_its_inner_event() {
        let inner = r#"{"type":"content_block_delta","delta":{"text":"hi"}}"#;
        let mut buf = frame("chunk", inner);
        let msgs = take_messages(&mut buf);

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].headers.event_type.as_deref(), Some("chunk"));
        assert_eq!(inner_event(&msgs[0]).as_deref(), Some(inner));
        assert!(buf.is_empty(), "a complete message is consumed");
    }

    #[test]
    fn several_messages_arriving_together_all_decode() {
        let mut buf = frame("chunk", r#"{"a":1}"#);
        buf.extend(frame("chunk", r#"{"a":2}"#));
        buf.extend(frame("chunk", r#"{"a":3}"#));
        let msgs = take_messages(&mut buf);
        assert_eq!(msgs.len(), 3);
        assert_eq!(inner_event(&msgs[2]).as_deref(), Some(r#"{"a":3}"#));
    }

    #[test]
    fn a_message_split_across_reads_waits_for_the_rest() {
        // The realistic case, and the one a naive decoder gets wrong: a TCP
        // read boundary lands inside a frame.
        let whole = frame("chunk", r#"{"type":"message_stop"}"#);
        let split = whole.len() / 2;

        let mut buf = whole[..split].to_vec();
        assert!(take_messages(&mut buf).is_empty(), "nothing complete yet");
        assert_eq!(buf.len(), split, "the partial frame is kept");

        buf.extend_from_slice(&whole[split..]);
        let msgs = take_messages(&mut buf);
        assert_eq!(msgs.len(), 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn a_partial_prelude_is_kept_rather_than_misread() {
        let mut buf = vec![0u8, 0, 1];
        assert!(take_messages(&mut buf).is_empty());
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn a_corrupt_length_stops_rather_than_resynchronising_on_garbage() {
        // A frame claiming to be smaller than its own overhead. Advancing by a
        // bogus length would read the rest of the stream as noise.
        let mut buf = vec![0u8, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 9, 9, 9, 9];
        assert!(take_messages(&mut buf).is_empty());
        assert_eq!(buf.len(), 16, "nothing consumed");
    }

    #[test]
    fn headers_of_every_type_are_skipped_correctly() {
        // Only strings are read, but every type must be *skipped* by the right
        // width or the parse desynchronises and later headers become garbage.
        let mut headers = Vec::new();
        // A bool header before the one we want.
        headers.push(4u8);
        headers.extend_from_slice(b"flag");
        headers.push(0); // bool true, no value bytes
        // An integer header.
        headers.push(3u8);
        headers.extend_from_slice(b"num");
        headers.push(4); // integer
        headers.extend_from_slice(&7i32.to_be_bytes());
        // Then the one that matters.
        headers.push(11u8);
        headers.extend_from_slice(b":event-type");
        headers.push(7);
        headers.extend_from_slice(&5u16.to_be_bytes());
        headers.extend_from_slice(b"chunk");

        let parsed = parse_headers(&headers);
        assert_eq!(parsed.event_type.as_deref(), Some("chunk"));
    }

    #[test]
    fn an_exception_frame_becomes_an_error_event_the_parser_dispatches_on() {
        // H7. This used to return the body verbatim and assert only that the
        // message text was in it — which the old code satisfied while the
        // defect was live. The body goes to `anthropic::parse_event`, whose
        // dispatch is on `v["type"]`, and an AWS exception payload is
        // `{"message":"…"}` with no `type`: it fell to the `_ => vec![]` arm
        // and was erased. A stream that emitted deltas and then a
        // `throttlingException` reached the client as a 200 with a
        // half-finished answer, no error, no cooldown, and a ledger charge.
        //
        // So the assertion is the round trip, not the substring.
        let msg = Message {
            headers: Headers {
                event_type: None,
                exception_type: Some("throttlingException".to_owned()),
            },
            payload: br#"{"message":"Too many requests"}"#.to_vec(),
        };
        let raw = inner_event(&msg).expect("an exception yields an event");

        let mut acc = oag_proto::StreamAccumulator::new();
        let events = oag_proto::anthropic::parse_event(&raw, &mut acc).expect("parses");
        let message = events
            .iter()
            .find_map(|e| match e {
                oag_proto::StreamEvent::Error { message } => Some(message.as_str()),
                _ => None,
            })
            .expect("the parser has to see an error, which is the whole finding");
        assert!(message.contains("Too many requests"), "{message}");
        assert!(
            raw.contains("throttlingException"),
            "the kind is a header on the envelope and exists nowhere downstream, \
             so it has to be carried into the body here: {raw}"
        );
    }

    #[test]
    fn an_exception_body_that_is_not_utf8_still_becomes_an_error() {
        // U14. The body was decoded through `char::from`, which is Latin-1 —
        // every multi-byte character in a message an operator is meant to read
        // came out mangled. Decoding strictly instead would be worse: `None`
        // puts us back in the silent stall this exists to prevent, for a frame
        // where the provider had actually said what was wrong.
        let mut payload = br#"{"message":"rate limited "#.to_vec();
        payload.extend_from_slice(&[0xff, 0xfe]);
        payload.extend_from_slice(br#""}"#);

        let msg = Message {
            headers: Headers {
                event_type: None,
                exception_type: Some("modelStreamErrorException".to_owned()),
            },
            payload,
        };
        let raw = inner_event(&msg).expect("an unreadable body is still an exception");

        let mut acc = oag_proto::StreamAccumulator::new();
        let events = oag_proto::anthropic::parse_event(&raw, &mut acc).expect("parses");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, oag_proto::StreamEvent::Error { .. })),
            "{raw}"
        );
    }

    #[test]
    fn a_frame_with_no_inner_event_yields_nothing() {
        let msg = Message {
            headers: Headers::default(),
            payload: br#"{"something":"else"}"#.to_vec(),
        };
        assert!(inner_event(&msg).is_none());
    }
}
