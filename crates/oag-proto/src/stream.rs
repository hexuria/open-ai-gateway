//! Streaming translation state.
//!
//! Translating a stream is not a map over events. The dialects disagree about
//! where a content block starts, whether tool arguments arrive whole or in
//! fragments, and when usage is reported — so translation needs state that
//! lives for the duration of the response. That state is here, in one place,
//! rather than smeared across each codec.

use oag_router::Usage;
use serde::{Deserialize, Serialize};

/// A dialect-independent stream event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// The response has begun. Carries whatever usage is known up front —
    /// which, for Anthropic, is the input and cache counts.
    Start {
        model: String,
        usage: Usage,
    },
    /// A run of assistant text.
    TextDelta {
        text: String,
    },
    /// A run of reasoning text.
    ThinkingDelta {
        text: String,
    },
    /// A tool call is starting.
    ToolUseStart {
        id: String,
        name: String,
    },
    /// A fragment of a tool call's JSON arguments.
    ///
    /// Fragments, not a parsed value: providers stream partial JSON, and
    /// deferring the parse to `ToolUseEnd` is the only way to avoid trying to
    /// deserialise `{"path": "sr` as an object.
    ToolUseDelta {
        id: String,
        partial_json: String,
    },
    ToolUseEnd {
        id: String,
    },
    /// Updated usage. Arrives repeatedly; always merged, never assigned.
    UsageUpdate {
        usage: Usage,
    },
    /// The response is complete.
    Stop {
        reason: StopReason,
        usage: Usage,
    },
    /// An error arrived inside a 200 response body.
    ///
    /// Its own variant because it is not an HTTP error and the distinction
    /// matters for failover: if nothing has reached the client yet we can
    /// still retry elsewhere, and once bytes are out we cannot.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    Refusal,
}

/// Take the model name a `Start` event announced, keeping the current one when
/// it announced none.
///
/// The guard belongs on the incoming name. A render state is built with the
/// model the client routed on, so its own field is never empty, and testing
/// that one instead assigns unconditionally — blanking the model for every
/// upstream that opens a stream without naming one, which the Anthropic and
/// Responses parsers both default to empty. Shared rather than written out per
/// dialect because it was written out per dialect, and one of the three copies
/// had the test the wrong way round.
pub(crate) fn adopt_model(current: &mut String, announced: &str) {
    if !announced.is_empty() {
        current.clear();
        current.push_str(announced);
    }
}

/// State carried across the events of one response.
#[derive(Debug, Clone, Default)]
pub struct StreamAccumulator {
    usage: Usage,
    /// Whether any byte has reached the client. Once true, failover is off the
    /// table: a half-written stream cannot be restarted on another credential
    /// without the client seeing two beginnings.
    committed: bool,
    text_len: usize,
    tool_calls: usize,
    stop_reason: Option<StopReason>,
    /// Partial tool arguments, keyed by call id.
    tool_buffers: Vec<(String, String)>,
    /// The tool call opened and not yet ended, if any.
    ///
    /// Anthropic closes every content block with the same `content_block_stop`
    /// and says nothing about which kind it was closing. Without this, a text
    /// block's stop found the most recent tool id still sitting in
    /// `tool_buffers` and closed a call that had already ended.
    open_tool: Option<String>,
    /// Set when nothing here was read from the response at all.
    unparsed: bool,
}

impl StreamAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold an event in.
    pub fn observe(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Start { usage, .. }
            | StreamEvent::UsageUpdate { usage }
            | StreamEvent::Stop { usage, .. } => {
                // Merge, never assign: Anthropic reports input and cache counts
                // in `message_start` and output counts later without repeating
                // them, so assignment silently zeroes the input side of every
                // streamed request's bill.
                self.usage.merge(usage);
                if let StreamEvent::Stop { reason, .. } = event {
                    self.stop_reason = Some(*reason);
                }
            }
            StreamEvent::TextDelta { text } | StreamEvent::ThinkingDelta { text } => {
                self.text_len = self.text_len.saturating_add(text.len());
            }
            StreamEvent::ToolUseStart { id, .. } => {
                self.tool_calls = self.tool_calls.saturating_add(1);
                self.tool_buffers.push((id.clone(), String::new()));
                self.open_tool = Some(id.clone());
            }
            StreamEvent::ToolUseDelta { id, partial_json } => {
                if let Some((_, buf)) = self.tool_buffers.iter_mut().find(|(k, _)| k == id) {
                    buf.push_str(partial_json);
                }
            }
            StreamEvent::ToolUseEnd { .. } => self.open_tool = None,
            StreamEvent::Error { .. } => {}
        }
    }

    /// Note that bytes have gone to the client.
    pub fn mark_committed(&mut self) {
        self.committed = true;
    }

    /// Note that the response body could not be read at all.
    ///
    /// Not the same thing as an empty answer, and the difference is money. An
    /// empty answer is the model's failure and worth escalating; a body we have
    /// no reader for is ours, and gating on it escalates every request to the
    /// expensive rung while recording zero tokens for either attempt.
    pub fn mark_unparsed(&mut self) {
        self.unparsed = true;
    }

    /// Whether we can still fail this request over to another credential.
    #[must_use]
    pub const fn can_failover(&self) -> bool {
        !self.committed
    }

    /// The id of the tool call currently being streamed, if any.
    ///
    /// Anthropic identifies a tool block once, at `content_block_start`, and
    /// then addresses its deltas by block index. Canonical events carry the id
    /// on every delta, so the mapping has to live somewhere — here, because it
    /// is per-response state and this is where per-response state goes.
    #[must_use]
    pub fn current_tool_id(&self) -> Option<String> {
        self.tool_buffers.last().map(|(id, _)| id.clone())
    }

    /// The id of the `index`-th tool call opened in this response.
    ///
    /// Chat Completions addresses argument fragments by `index` after the
    /// first, and streams parallel calls interleaved by it. `current_tool_id`
    /// alone would hand every fragment to whichever call opened last, which is
    /// right for one call and wrong for two.
    #[must_use]
    pub fn tool_id_at(&self, index: usize) -> Option<String> {
        self.tool_buffers.get(index).map(|(id, _)| id.clone())
    }

    /// The tool call that is open right now: started and not yet ended.
    ///
    /// For a dialect whose block-close event does not say what kind of block it
    /// closed, this is the difference between "close the call" and "close
    /// whatever was last a call".
    #[must_use]
    pub fn open_tool_id(&self) -> Option<String> {
        self.open_tool.clone()
    }

    /// Every tool call opened in this response, in the order they opened.
    ///
    /// For a dialect that never signals the end of a call, the terminal chunk
    /// is where they all end; this is the list it needs.
    #[must_use]
    pub fn tool_ids(&self) -> Vec<String> {
        self.tool_buffers.iter().map(|(id, _)| id.clone()).collect()
    }

    /// Whether any tool call has opened in this response so far.
    ///
    /// For a parser whose dialect ends a tool-calling turn with the same
    /// event as a plain one: the stop reason it emits has to say `tool_use`
    /// when a call was made, and the call was made in an earlier frame.
    #[must_use]
    pub const fn saw_tool_call(&self) -> bool {
        self.tool_calls > 0
    }

    /// How many tool calls have opened in this response so far.
    ///
    /// For a dialect with no wire id for a call: the ordinal is what makes
    /// the synthesised id unique across the chunks of one stream.
    #[must_use]
    pub const fn tool_call_count(&self) -> usize {
        self.tool_calls
    }

    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    #[must_use]
    pub const fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
    }

    /// Whether the response is bad enough to be worth retrying on a better
    /// model, and which gate it tripped.
    ///
    /// Only shapes a stronger model would plausibly fix. Transport failures are
    /// absent on purpose: escalating on those would quietly migrate the fleet
    /// onto expensive models every time a provider had a bad afternoon.
    #[must_use]
    pub fn quality_gate(&self) -> Option<oag_router::QualityGate> {
        use oag_router::QualityGate;

        // Nothing observed because nothing was readable. Judging that would
        // blame the model for our own blind spot.
        if self.unparsed {
            return None;
        }

        if self.text_len == 0 && self.tool_calls == 0 {
            return Some(QualityGate::EmptyResponse);
        }
        match self.stop_reason {
            Some(StopReason::Refusal) => Some(QualityGate::Refusal),
            Some(StopReason::MaxTokens) => Some(QualityGate::Truncated),
            _ => {
                // A tool call whose accumulated arguments are not valid JSON is
                // the classic small-model failure on an agentic prompt, and the
                // single most valuable thing to escalate on.
                // An empty buffer is not malformed. A zero-parameter tool
                // streams `arguments: ""`, which `from_str` rejects — so a
                // perfectly valid call condemned the answer, and the gateway
                // paid for a second attempt that produced the same call again.
                // Empty means "no arguments", which is `{}`.
                let malformed = self.tool_buffers.iter().any(|(_, buf)| {
                    !buf.trim().is_empty()
                        && serde_json::from_str::<serde_json::Value>(buf).is_err()
                });
                malformed.then_some(QualityGate::MalformedToolCall)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_router::QualityGate;

    /// P8. A tool that takes no arguments is not a malformed tool call.
    #[test]
    fn a_zero_parameter_tool_call_is_not_condemned_as_malformed() {
        // A tool with no parameters streams `arguments: ""`, which `from_str`
        // rejects — so the gate condemned a perfectly valid call, and the
        // gateway paid for a second attempt that produced the same call again.
        // Empty means "no arguments", which is `{}`.
        let mut acc = StreamAccumulator::new();
        for e in [
            StreamEvent::ToolUseStart {
                id: "call_1".to_owned(),
                name: "get_time".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_1".to_owned(),
                partial_json: String::new(),
            },
            StreamEvent::ToolUseEnd {
                id: "call_1".to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ] {
            acc.observe(&e);
        }
        assert_eq!(acc.quality_gate(), None, "a valid call with no arguments");

        // Arguments that are genuinely broken still are.
        let mut acc = StreamAccumulator::new();
        for e in [
            StreamEvent::ToolUseStart {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
            },
            StreamEvent::ToolUseDelta {
                id: "call_1".to_owned(),
                partial_json: r#"{"path": "#.to_owned(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ] {
            acc.observe(&e);
        }
        assert_eq!(acc.quality_gate(), Some(QualityGate::MalformedToolCall));
    }

    fn usage(input: u64, output: u64, cache_read: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
        }
    }

    #[test]
    fn usage_accumulates_instead_of_being_overwritten() {
        // The exact shape of an Anthropic stream. Assignment here would zero
        // the input and cache counts and under-bill every streamed request.
        let mut acc = StreamAccumulator::new();
        acc.observe(&StreamEvent::Start {
            model: "claude-opus-5".to_owned(),
            usage: usage(1_000, 0, 20_000),
        });
        acc.observe(&StreamEvent::UsageUpdate {
            usage: usage(0, 50, 0),
        });
        acc.observe(&StreamEvent::Stop {
            reason: StopReason::EndTurn,
            usage: usage(0, 250, 0),
        });

        assert_eq!(acc.usage().input_tokens, 1_000);
        assert_eq!(acc.usage().cache_read_tokens, 20_000);
        assert_eq!(acc.usage().output_tokens, 250);
    }

    #[test]
    fn failover_is_available_until_the_first_byte_ships() {
        let mut acc = StreamAccumulator::new();
        assert!(acc.can_failover());
        acc.mark_committed();
        assert!(
            !acc.can_failover(),
            "a half-written stream cannot restart elsewhere"
        );
    }

    #[test]
    fn an_empty_response_trips_the_gate() {
        let mut acc = StreamAccumulator::new();
        acc.observe(&StreamEvent::Start {
            model: "m".to_owned(),
            usage: Usage::default(),
        });
        assert_eq!(acc.quality_gate(), Some(QualityGate::EmptyResponse));
    }

    #[test]
    fn a_body_nothing_could_read_is_not_an_empty_response() {
        // The two look identical from here — no text, no tool calls — and they
        // must not be treated the same. An empty answer is the model's failure
        // and worth paying a better model to retry; a body we have no reader for
        // is ours, and escalating on it means paying twice for every request
        // through that upstream and recording tokens for neither attempt.
        let mut acc = StreamAccumulator::new();
        acc.mark_unparsed();
        assert_eq!(acc.quality_gate(), None);
    }

    #[test]
    fn a_good_response_trips_nothing() {
        let mut acc = StreamAccumulator::new();
        acc.observe(&StreamEvent::TextDelta {
            text: "hello".to_owned(),
        });
        acc.observe(&StreamEvent::Stop {
            reason: StopReason::EndTurn,
            usage: usage(10, 5, 0),
        });
        assert_eq!(acc.quality_gate(), None);
    }

    #[test]
    fn truncation_and_refusal_are_distinguished() {
        let mut truncated = StreamAccumulator::new();
        truncated.observe(&StreamEvent::TextDelta {
            text: "partial".to_owned(),
        });
        truncated.observe(&StreamEvent::Stop {
            reason: StopReason::MaxTokens,
            usage: Usage::default(),
        });
        assert_eq!(truncated.quality_gate(), Some(QualityGate::Truncated));

        let mut refused = StreamAccumulator::new();
        refused.observe(&StreamEvent::TextDelta {
            text: "I cannot".to_owned(),
        });
        refused.observe(&StreamEvent::Stop {
            reason: StopReason::Refusal,
            usage: Usage::default(),
        });
        assert_eq!(refused.quality_gate(), Some(QualityGate::Refusal));
    }

    #[test]
    fn malformed_tool_arguments_trip_the_gate() {
        // The classic small-model failure on an agentic prompt, and the single
        // most valuable thing to escalate on.
        let mut acc = StreamAccumulator::new();
        acc.observe(&StreamEvent::ToolUseStart {
            id: "call_1".to_owned(),
            name: "read_file".to_owned(),
        });
        acc.observe(&StreamEvent::ToolUseDelta {
            id: "call_1".to_owned(),
            partial_json: "{\"path\": \"src/".to_owned(),
        });
        acc.observe(&StreamEvent::ToolUseEnd {
            id: "call_1".to_owned(),
        });
        acc.observe(&StreamEvent::Stop {
            reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        assert_eq!(acc.quality_gate(), Some(QualityGate::MalformedToolCall));
    }

    #[test]
    fn well_formed_tool_arguments_assembled_from_fragments_are_fine() {
        let mut acc = StreamAccumulator::new();
        acc.observe(&StreamEvent::ToolUseStart {
            id: "call_1".to_owned(),
            name: "read_file".to_owned(),
        });
        for fragment in ["{\"path\"", ": \"src/", "main.rs\"}"] {
            acc.observe(&StreamEvent::ToolUseDelta {
                id: "call_1".to_owned(),
                partial_json: fragment.to_owned(),
            });
        }
        acc.observe(&StreamEvent::Stop {
            reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        assert_eq!(
            acc.quality_gate(),
            None,
            "fragments reassemble into valid JSON"
        );
    }

    #[test]
    fn a_mid_stream_error_does_not_escalate_by_itself() {
        // Transport trouble is a credential problem for failover to handle, not
        // a reason to start paying for a bigger model.
        let mut acc = StreamAccumulator::new();
        acc.observe(&StreamEvent::TextDelta {
            text: "partial".to_owned(),
        });
        acc.observe(&StreamEvent::Error {
            message: "upstream reset".to_owned(),
        });
        assert_eq!(acc.quality_gate(), None);
    }
}
