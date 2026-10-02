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
//! `InvokeModelWithResponseStream`'s payload is JSON of the form
//! `{"bytes": "<base64>"}`, and the base64 decodes to the provider's own event
//! — Anthropic's, for a Claude model. So the useful output of this module is
//! that inner JSON ([`inner_event`]).
//!
//! `ConverseStream` frames its events the same way and wraps them in nothing:
//! the payload is the event's JSON, and which event it is is the message's
//! `:event-type` header. [`converse_event`] puts the two back together.
//!
//! A message is one of three kinds, by its `:message-type`: an `event`; an
//! `exception`, a failure the API models, named by `:exception-type`; or an
//! `error`, one it does not, which says what failed in its `:error-code` and
//! `:error-message` headers and nothing in its payload
//! (<https://smithy.io/2.0/aws/amazon-eventstream.html>). Both readers turn
//! the last two into an error the client is told.

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
    /// `:message-type`: `event`, `exception`, or `error` for a failure the
    /// API does not model, which the two headers below describe.
    pub message_type: Option<String>,
    /// `:error-code`, on an `error` message: what failed, by name.
    pub error_code: Option<String>,
    /// `:error-message`, on an `error` message: what failed, in words.
    pub error_message: Option<String>,
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

/// Parse the header block, keeping only the headers that matter.
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
            ":message-type" => headers.message_type = value,
            ":error-code" => headers.error_code = value,
            ":error-message" => headers.error_message = value,
            _ => {}
        }
    }

    headers
}

/// One message framed as AWS frames it, both checksums included: `headers` as
/// string headers in the order given, then `payload`. `None` when a header or
/// the whole message is too long for the format to say.
///
/// For tests, here and in the crates that depend on this one (the
/// `test-fixtures` feature): a stand-in Bedrock has to send the bytes the real
/// one does, and a copy of this per test file would be one more encoder each
/// to get wrong.
#[cfg(any(test, feature = "test-fixtures"))]
#[must_use]
pub fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Option<Vec<u8>> {
    let mut block = Vec::new();
    for (name, value) in headers {
        block.push(u8::try_from(name.len()).ok()?);
        block.extend_from_slice(name.as_bytes());
        // A string, the one value type every header here has.
        block.push(7);
        block.extend_from_slice(&u16::try_from(value.len()).ok()?.to_be_bytes());
        block.extend_from_slice(value.as_bytes());
    }
    let total = OVERHEAD + block.len() + payload.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&u32::try_from(total).ok()?.to_be_bytes());
    out.extend_from_slice(&u32::try_from(block.len()).ok()?.to_be_bytes());
    let prelude = crc32(&out);
    out.extend_from_slice(&prelude.to_be_bytes());
    out.extend_from_slice(&block);
    out.extend_from_slice(payload);
    let message = crc32(&out);
    out.extend_from_slice(&message.to_be_bytes());
    Some(out)
}

/// CRC-32 as the format checksums with it: the IEEE polynomial, reflected,
/// the one zlib computes.
#[cfg(any(test, feature = "test-fixtures"))]
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
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

/// What an `error` message says failed, as one line, `{code}: {words}`
/// (`InternalError: An internal server error occurred.`), or `None` for a
/// message of any other kind.
///
/// Both headers are required of an `error` message. One that arrives without
/// them is still a stream failing, so what is missing is taken from the
/// payload's text, or left out, rather than the message dropped.
fn stream_error(msg: &Message) -> Option<String> {
    if msg.headers.message_type.as_deref() != Some("error") {
        return None;
    }
    let code = msg
        .headers
        .error_code
        .as_deref()
        .filter(|code| !code.is_empty());
    let words = msg
        .headers
        .error_message
        .clone()
        .filter(|words| !words.is_empty())
        .or_else(|| {
            Some(String::from_utf8_lossy(&msg.payload).trim().to_owned())
                .filter(|words| !words.is_empty())
        });
    Some(match (code, words) {
        (Some(code), Some(words)) => format!("{code}: {words}"),
        (Some(code), None) => code.to_owned(),
        (None, Some(words)) => words,
        (None, None) => "the upstream's event stream reported an error and named none".to_owned(),
    })
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
    // An unmodeled error has no payload to parse at all: the headers say it.
    if let Some(error) = stream_error(msg) {
        return Some(
            serde_json::json!({
                "type": "error",
                "error": { "type": "error", "message": error },
            })
            .to_string(),
        );
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

/// A `ConverseStream` message as the one-member union the API reference
/// documents: `{"<event type>": payload}`, which is what
/// `oag_proto::converse::parse_event` reads.
///
/// Converse sends each event's JSON as the payload itself, with no `bytes`
/// envelope, and says which event it is only in the `:event-type` header, so
/// the name is put back around the payload here, where it is still in hand.
///
/// An exception frame is named by its `:exception-type` the same way —
/// `{"throttlingException": {"message": …}}` — which the parser turns into an
/// error in Converse's own terms, the kind kept in the message. As in
/// [`inner_event`], the header is what makes a frame an exception, so a body
/// that is not JSON, or not UTF-8, is still one: its text becomes the message.
///
/// `None` for a frame that names no event, or whose event payload is not JSON.
#[must_use]
pub fn converse_event(msg: &Message) -> Option<String> {
    // Not a member of the union, so it goes under a name no member can have,
    // which `oag_proto::converse` reads as an error.
    if let Some(error) = stream_error(msg) {
        let mut union = serde_json::Map::new();
        union.insert(
            oag_proto::converse::STREAM_ERROR.to_owned(),
            serde_json::json!({ "message": error }),
        );
        return Some(serde_json::Value::Object(union).to_string());
    }
    let (kind, payload) = if let Some(kind) = msg.headers.exception_type.as_deref() {
        let body = String::from_utf8_lossy(&msg.payload);
        let payload = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({ "message": body }));
        (kind, payload)
    } else {
        let kind = msg.headers.event_type.as_deref()?;
        (kind, serde_json::from_slice(&msg.payload).ok()?)
    };
    let mut union = serde_json::Map::new();
    union.insert(kind.to_owned(), payload);
    Some(serde_json::Value::Object(union).to_string())
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
                exception_type: Some("throttlingException".to_owned()),
                ..Headers::default()
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
        let exception = |payload: Vec<u8>| {
            inner_event(&Message {
                headers: Headers {
                    exception_type: Some("modelStreamErrorException".to_owned()),
                    ..Headers::default()
                },
                payload,
            })
            .expect("an exception frame is an error however its body decodes")
        };

        // A VALID multi-byte sequence, which is the case that tells the two
        // decodings apart. `C3 A9` is `é` in UTF-8 and `Ã©` in Latin-1 — and
        // the previous fixture used `FF FE`, which is invalid UTF-8, so it came
        // out mangled either way and the test passed with the bug restored.
        let mut valid = br#"{"message":"quota d"#.to_vec();
        valid.extend_from_slice(&[0xc3, 0xa9]);
        valid.extend_from_slice(br#"pass\u00e9"}"#);
        let raw = exception(valid);
        assert!(
            raw.contains("dépassé"),
            "a message an operator is meant to read came through mangled: {raw}"
        );
        assert!(!raw.contains("Ã©"), "that is Latin-1 output: {raw}");

        // And a body that is not valid UTF-8 at all is still an error rather
        // than a silent stall. Decoding strictly would put us back in the
        // failure this exists to prevent, for a frame where the provider had
        // actually said what was wrong.
        let mut invalid = br#"{"message":"rate limited "#.to_vec();
        invalid.extend_from_slice(&[0xff, 0xfe]);
        invalid.extend_from_slice(br#""}"#);
        let raw = exception(invalid);

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

    // ── ConverseStream ───────────────────────────────────────────────────────

    /// A `ConverseStream` event as Bedrock frames it: three string headers,
    /// and the event's JSON as the payload, padding field `p` and all.
    fn converse(event_type: &str, payload: &str) -> Vec<u8> {
        encode(
            &[
                (":event-type", event_type),
                (":content-type", "application/json"),
                (":message-type", "event"),
            ],
            payload.as_bytes(),
        )
        .expect("a short message")
    }

    fn exception(kind: &str, payload: &[u8]) -> Vec<u8> {
        encode(
            &[
                (":exception-type", kind),
                (":content-type", "application/json"),
                (":message-type", "exception"),
            ],
            payload,
        )
        .expect("a short message")
    }

    /// The one decoded message in `bytes`.
    fn only(bytes: &[u8]) -> Message {
        let mut buf = bytes.to_vec();
        let mut messages = take_messages(&mut buf);
        assert!(buf.is_empty(), "a whole message is consumed");
        assert_eq!(messages.len(), 1);
        messages.remove(0)
    }

    fn parsed(named: &str) -> Vec<oag_proto::StreamEvent> {
        oag_proto::converse::parse_event(named, &mut oag_proto::StreamAccumulator::new())
            .expect("parses")
    }

    #[test]
    fn a_converse_event_is_its_payload_named_by_its_header() {
        let payload = r#"{"contentBlockIndex":0,"delta":{"text":"Starman"},"p":"abcdefghij"}"#;
        let msg = only(&converse("contentBlockDelta", payload));
        assert_eq!(msg.headers.event_type.as_deref(), Some("contentBlockDelta"));

        let named = converse_event(&msg).expect("an event");
        let event: serde_json::Value = serde_json::from_str(payload).expect("JSON");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&named).expect("JSON"),
            serde_json::json!({ "contentBlockDelta": event })
        );
        // The union the parser reads, and the text in it.
        assert_eq!(
            parsed(&named),
            vec![oag_proto::StreamEvent::TextDelta {
                text: "Starman".to_owned()
            }]
        );
        // The reader for the other API finds nothing in it: there is no
        // `bytes` envelope to open.
        assert!(inner_event(&msg).is_none());
    }

    #[test]
    fn a_converse_exception_is_an_error_in_converses_own_terms() {
        let msg = only(&exception(
            "throttlingException",
            br#"{"message":"Too many tokens, please wait before trying again."}"#,
        ));
        let named = converse_event(&msg).expect("an exception is never dropped");
        assert_eq!(
            named,
            r#"{"throttlingException":{"message":"Too many tokens, please wait before trying again."}}"#
        );
        assert!(
            !named.contains(r#""type":"error""#),
            "not Anthropic's: {named}"
        );
        assert_eq!(
            parsed(&named),
            vec![oag_proto::StreamEvent::Error {
                message: "throttlingException: Too many tokens, please wait before trying again."
                    .to_owned()
            }]
        );
    }

    #[test]
    fn a_converse_exception_whose_body_cannot_be_read_is_still_an_error() {
        // Not JSON, not an object, not UTF-8: the header says it is an
        // exception, and the provider's words are kept as far as they go.
        let mut not_utf8 = b"quota d".to_vec();
        not_utf8.extend_from_slice(&[0xff, 0xfe]);
        for (body, says) in [
            (b"service unavailable".to_vec(), "service unavailable"),
            (br#""a JSON string""#.to_vec(), "a JSON string"),
            (not_utf8, "quota d"),
        ] {
            let msg = only(&exception("serviceUnavailableException", &body));
            let named = converse_event(&msg).expect("an exception is never dropped");
            let events = parsed(&named);
            let [oag_proto::StreamEvent::Error { message }] = events.as_slice() else {
                panic!("one error, from {named}: {events:?}");
            };
            assert!(
                message.starts_with("serviceUnavailableException: ") && message.contains(says),
                "{message}"
            );
        }
    }

    /// The third kind of message, beside events and exceptions: a failure the
    /// API does not model, `:message-type: error`, which says what failed in
    /// its `:error-code` and `:error-message` headers and carries nothing in
    /// its payload. Neither reader knew it, so each dropped it, and a stream
    /// that failed this way ended as though the answer had. Both now tell the
    /// client the code and the words.
    #[test]
    fn an_error_message_is_an_error_naming_its_code_and_words() {
        let msg = only(
            &encode(
                &[
                    (":message-type", "error"),
                    (":error-code", "InternalError"),
                    (":error-message", "An internal server error occurred."),
                ],
                b"",
            )
            .expect("a short message"),
        );
        let said = vec![oag_proto::StreamEvent::Error {
            message: "InternalError: An internal server error occurred.".to_owned(),
        }];

        let named = converse_event(&msg).expect("an error is never dropped");
        assert_eq!(parsed(&named), said, "Converse: {named}");

        let raw = inner_event(&msg).expect("an error is never dropped");
        let events =
            oag_proto::anthropic::parse_event(&raw, &mut oag_proto::StreamAccumulator::new())
                .expect("parses");
        assert_eq!(events, said, "InvokeModel: {raw}");

        // Its headers are required. Without them it is still an error, in
        // whatever words its payload has.
        let bare = only(&encode(&[(":message-type", "error")], b"stream reset").expect("short"));
        let named = converse_event(&bare).expect("an error is never dropped");
        assert_eq!(
            parsed(&named),
            vec![oag_proto::StreamEvent::Error {
                message: "stream reset".to_owned()
            }]
        );
    }

    #[test]
    fn a_converse_frame_that_names_no_event_or_is_not_json_yields_nothing() {
        let unnamed = only(&encode(&[(":message-type", "event")], b"{}").expect("short"));
        assert!(converse_event(&unnamed).is_none());
        let garbled = only(&converse("contentBlockDelta", "{not json"));
        assert!(converse_event(&garbled).is_none());
    }

    #[test]
    fn the_checksum_is_the_one_zlib_computes() {
        // The standard check value for CRC-32/ISO-HDLC.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn an_encoded_message_is_framed_and_checksummed_as_aws_frames_one() {
        let bytes = converse("messageStop", r#"{"stopReason":"end_turn"}"#);
        let word = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        assert_eq!(
            word(0),
            u32::try_from(bytes.len()).expect("short"),
            "the total length"
        );
        assert_eq!(word(8), crc32(&bytes[..8]), "the prelude's checksum");
        assert_eq!(
            word(bytes.len() - 4),
            crc32(&bytes[..bytes.len() - 4]),
            "the message's checksum covers everything before it"
        );
        let msg = only(&bytes);
        assert_eq!(msg.headers.event_type.as_deref(), Some("messageStop"));
        assert_eq!(msg.payload, br#"{"stopReason":"end_turn"}"#);
        assert!(
            encode(&[("n".repeat(256).as_str(), "v")], b"").is_none(),
            "a header name is at most 255 bytes"
        );
    }
}
