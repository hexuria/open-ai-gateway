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

/// The provider's own event JSON, unwrapped from Bedrock's envelope.
///
/// Returns `None` for a frame that carries no inner event — a heartbeat, or a
/// payload shaped differently from what we expect. An exception frame yields
/// its body so the caller can surface the message rather than a silent stall.
#[must_use]
pub fn inner_event(msg: &Message) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(&msg.payload).ok()?;

    if let Some(encoded) = v["bytes"].as_str() {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        return String::from_utf8(decoded).ok();
    }

    // An exception frame is JSON already, and saying so beats stalling until
    // the idle watchdog fires with no explanation.
    if msg.headers.exception_type.is_some() {
        return Some(msg.payload.clone().into_iter().map(char::from).collect());
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
    fn an_exception_frame_surfaces_its_body() {
        // Better than stalling until the idle watchdog fires with no reason.
        let msg = Message {
            headers: Headers {
                event_type: None,
                exception_type: Some("throttlingException".to_owned()),
            },
            payload: br#"{"message":"Too many requests"}"#.to_vec(),
        };
        assert!(
            inner_event(&msg)
                .expect("body")
                .contains("Too many requests")
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
