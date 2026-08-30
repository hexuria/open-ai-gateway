//! The hub representation.

use oag_router::RequestSignal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Marks a prompt prefix as cacheable.
///
/// Load-bearing well beyond translation: the scheduler derives session affinity
/// from exactly the blocks carrying this, because they are the part of the
/// prompt that is stable across turns. See `oag_pool::sticky`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        cache_control: Option<CacheControl>,
    },
    Image {
        media_type: String,
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// Extended reasoning. Carries an opaque provider signature that must round
    /// trip byte-exact — re-serialising it differently makes the provider reject
    /// the next turn as tampered.
    Thinking {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cache_control: Option<CacheControl>,
}

/// How the client constrained the model's use of tools.
///
/// Every dialect can express all four, but each spells them differently —
/// `required` here is `any` in Anthropic and `ANY` in Gemini — which is exactly
/// why it needs a canonical form rather than being forwarded verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call a tool.
    Auto,
    /// The model must call at least one tool.
    Required,
    /// The model must not call a tool.
    None,
    /// The model must call this one.
    Tool { name: String },
}

/// The shape the client demanded of the answer.
///
/// Load-bearing in a way `temperature` is not: a client that asked for a JSON
/// object is going to run `JSON.parse` on whatever comes back, so quietly
/// downgrading this to free text does not degrade the answer, it breaks the
/// caller. Anthropic Messages has no field for it, so translation to that
/// dialect fails rather than dropping it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Free text. Every dialect's default, so asking for it explicitly costs
    /// nothing and constrains nobody.
    Text,
    /// Any valid JSON object.
    JsonObject,
    /// JSON conforming to a schema.
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        /// Whether the provider must reject output that leaves the schema
        /// rather than doing its best.
        #[serde(default)]
        strict: bool,
    },
}

impl ResponseFormat {
    /// Whether this actually narrows what the model may return.
    ///
    /// `Text` does not: it names the behaviour every dialect already has. So a
    /// dialect with no structured-output field can still honour it, and a
    /// client that sets it explicitly is not worth a 400.
    #[must_use]
    pub const fn constrains_output(&self) -> bool {
        !matches!(self, Self::Text)
    }
}

/// How hard a model should think, as the dialects that offer a level say it.
///
/// The names and the ladder are OpenAI's, because it is the vendor that models
/// this as a level at all; Anthropic and Gemini state a token budget instead.
///
/// SIX RUNGS, TAKEN FROM THE BACKEND RATHER THAN GUESSED. `gpt-5.6-luna`
/// refuses `minimal` and names what it does accept: "Supported values are:
/// 'none', 'low', 'medium', 'high', 'xhigh', and 'max'." An earlier version of
/// this enum stopped at `High` and folded `xhigh` and `max` into it, which
/// silently capped a client asking for the most reasoning available at the
/// middle of the range — the opposite of what it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Rendered as `none`: reasoning off, which is a request like any other.
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    /// The wire spelling, which is the same in every dialect that has the concept.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Read a level, or nothing for a word this does not know.
    ///
    /// Unknown is `None` rather than a default: a client that asked for
    /// something we cannot express should get the upstream's own default, not
    /// a level we invented on its behalf.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            // `minimal` is another vendor's spelling of the bottom rung, and
            // some models refuse it by that name. Read it, render `none`.
            "none" | "minimal" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// A token budget for a dialect that only speaks budgets.
    ///
    /// Nominal, and unavoidably so — there is no exchange rate between "high"
    /// and a number of tokens, and any figure here is this gateway's opinion
    /// rather than the vendor's. The ordering is the only part that carries
    /// meaning, and it is what a downstream classifier reads.
    #[must_use]
    pub const fn as_budget(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Low => 4096,
            Self::Medium => 8192,
            Self::High => 16384,
            Self::XHigh => 32768,
            Self::Max => 65536,
        }
    }

    /// The nearest level to a budget, for a dialect that only speaks levels.
    ///
    /// Any budget above zero means at least `Low`: a client that asked for a
    /// thousand tokens of thinking asked for thinking, and rounding that down
    /// to `none` would answer the opposite of the question.
    #[must_use]
    pub const fn from_budget(tokens: u32) -> Self {
        if tokens == 0 {
            Self::Off
        } else if tokens >= 65536 {
            Self::Max
        } else if tokens >= 32768 {
            Self::XHigh
        } else if tokens >= 16384 {
            Self::High
        } else if tokens >= 8192 {
            Self::Medium
        } else {
            Self::Low
        }
    }
}

/// A request, dialect-independent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRequest {
    /// What the client asked for. May be a virtual name like `oag/auto`, which
    /// the router resolves; the adapter never sees the virtual form.
    pub model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<ContentBlock>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    pub max_tokens: u32,
    #[serde(default)]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub temperature: Option<f32>,
    /// Extended thinking budget, in tokens.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thinking_budget: Option<u32>,
    /// How hard to think, where the dialect says it as a level rather than a
    /// budget.
    ///
    /// SEPARATE FROM `thinking_budget`, and not derivable from it. The two are
    /// how different vendors express the same intent, and neither converts
    /// cleanly: a budget is a ceiling the model may not reach, a level is a
    /// dial with no stated cost. Collapsing them loses whichever the client
    /// actually said — and it is the client's own words that should reach an
    /// upstream speaking the same dialect.
    ///
    /// Both may be set. A dialect renders whichever it can express, and
    /// `Effort::as_budget`/`from_budget` bridge the gap when it can only
    /// express the other one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thinking_effort: Option<Effort>,
    /// A session identifier the client supplied, if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_session: Option<String>,
    /// How the client constrained tool use, if it said anything about it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_choice: Option<ToolChoice>,
    /// The shape the client demanded of the answer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_format: Option<ResponseFormat>,
    /// Sequences whose appearance ends generation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// The stored response this turn continues.
    ///
    /// Only OpenAI Responses has this, and it is the whole conversation history
    /// rather than a hint: sending the turn without it asks the model to answer
    /// a follow-up with no idea what it is following up on. So it is the one
    /// carried field where a silent drop changes the *prompt*, not just the
    /// constraints on the answer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub previous_response_id: Option<String>,
}

impl CanonicalRequest {
    /// Everything the router needs to classify this request.
    ///
    /// Deriving it here rather than in the router keeps the router free of any
    /// knowledge of message shapes, and means a new dialect gets classification
    /// for free the moment it can parse into canonical form.
    #[must_use]
    pub fn signal(&self) -> RequestSignal {
        RequestSignal {
            prompt_tokens: self.estimated_prompt_tokens(),
            tool_count: self.tools.len(),
            turn_count: self.messages.len(),
            has_images: self.has_images(),
            thinking_requested: self.thinking_budget.is_some_and(|b| b > 0),
            has_code: self.has_code(),
            explicit_tier: None,
        }
    }

    /// Rough prompt size.
    ///
    /// Four bytes per token is the usual English approximation. Deliberately an
    /// estimate: this feeds a routing threshold, not a bill, and running a real
    /// tokeniser over every prompt to decide which side of an 8000-token line
    /// it falls on is latency spent for no gain. Billing uses the provider's
    /// own reported counts.
    #[must_use]
    pub fn estimated_prompt_tokens(&self) -> u64 {
        let text: usize = self
            .system
            .iter()
            .chain(self.messages.iter().flat_map(|m| m.content.iter()))
            .map(block_len)
            .sum();
        let tools: usize = self
            .tools
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
            .sum();
        ((text + tools) / 4) as u64
    }

    #[must_use]
    pub fn has_images(&self) -> bool {
        self.messages
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    }

    /// Whether the prompt carries code or a diff.
    ///
    /// A fenced block or a unified-diff header. Both are structural markers
    /// rather than guesses about wording, which is what keeps this from
    /// drifting as prompt styles change.
    #[must_use]
    pub fn has_code(&self) -> bool {
        self.system
            .iter()
            .chain(self.messages.iter().flat_map(|m| m.content.iter()))
            .any(|b| match b {
                ContentBlock::Text { text, .. } => {
                    text.contains("```") || text.contains("\n@@ ") || text.contains("\n--- ")
                }
                _ => false,
            })
    }
}

/// Token estimate for a client that asked "how big is this prompt".
///
/// Deliberately separate from [`CanonicalRequest::estimated_prompt_tokens`],
/// which feeds a routing threshold. That one is biased low in ways that do not
/// matter there and do matter here: a client that under-counts never compacts
/// and then takes a hard context-overflow error from the provider. Changing it
/// would also silently re-route every deployment, because the classifier's
/// thresholds are calibrated against its current behaviour.
///
/// Still an estimate. No tokeniser is linked, and the divisors below are
/// reasoned rather than measured — which is why every response built on this
/// is marked as an estimate rather than presented as a count.
#[must_use]
pub fn count_input_tokens(req: &CanonicalRequest) -> u64 {
    let content: usize = req
        .system
        .iter()
        .chain(req.messages.iter().flat_map(|m| m.content.iter()))
        .map(count_block)
        .sum();

    // Tool schemas are JSON: punctuation-dense, so closer to three bytes per
    // token than four. The name and prose description are ordinary English.
    let tools: usize = req
        .tools
        .iter()
        .map(|t| (t.name.len() + t.description.len()) / 4 + t.input_schema.to_string().len() / 3)
        .sum();

    // Per-message and per-tool framing the provider adds around the content
    // itself — role markers, delimiters, the tool-definition envelope.
    let framing = 4 * req.messages.len() + 8 * req.tools.len();

    (content + tools + framing) as u64
}

fn count_block(b: &ContentBlock) -> usize {
    match b {
        ContentBlock::Text { text, .. } | ContentBlock::Thinking { text, .. } => text.len() / 4,
        ContentBlock::ToolResult { content, .. } => content.len() / 4,
        ContentBlock::ToolUse { name, input, .. } => (name.len() + input.to_string().len()) / 3,
        // Anthropic bills an image at roughly (width x height) / 750, which puts
        // a typical screenshot between 1,000 and 1,600 tokens. `block_len`'s
        // 1,000 *bytes* becomes 250 tokens after its divisor — low by about 6x,
        // and low is the direction that breaks a client's compaction trigger.
        ContentBlock::Image { .. } => 1_500,
    }
}

fn block_len(b: &ContentBlock) -> usize {
    match b {
        ContentBlock::Text { text, .. } | ContentBlock::Thinking { text, .. } => text.len(),
        ContentBlock::ToolResult { content, .. } => content.len(),
        ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
        // Images are billed by dimension, not bytes; the base64 length would
        // wildly overstate the token cost.
        ContentBlock::Image { .. } => 1_000,
    }
}

/// The prompt text the client marked cacheable, in order.
///
/// This is what session affinity is keyed on. Hashing the whole conversation
/// instead would produce a fresh key every turn and pin nothing — which is the
/// subtle failure mode: affinity appears to be configured, and never hits.
#[must_use]
pub fn extract_cache_blocks(req: &CanonicalRequest) -> Vec<&str> {
    let mut out: Vec<&str> = req
        .system
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text {
                text,
                cache_control: Some(CacheControl::Ephemeral),
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    // Tool definitions are usually the largest stable prefix in an agentic
    // prompt, and clients cache-mark them for exactly that reason.
    out.extend(
        req.tools
            .iter()
            .filter(|t| t.cache_control.is_some())
            .map(|t| t.name.as_str()),
    );

    out.extend(req.messages.iter().flat_map(|m| {
        m.content.iter().filter_map(|b| match b {
            ContentBlock::Text {
                text,
                cache_control: Some(CacheControl::Ephemeral),
            } => Some(text.as_str()),
            _ => None,
        })
    }));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> ContentBlock {
        ContentBlock::Text {
            text: s.to_owned(),
            cache_control: None,
        }
    }

    fn cached(s: &str) -> ContentBlock {
        ContentBlock::Text {
            text: s.to_owned(),
            cache_control: Some(CacheControl::Ephemeral),
        }
    }

    fn request(messages: Vec<Message>) -> CanonicalRequest {
        CanonicalRequest {
            model: "oag/auto".to_owned(),
            system: vec![],
            messages,
            tools: vec![],
            max_tokens: 1024,
            stream: true,
            temperature: None,
            thinking_budget: None,
            thinking_effort: None,
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
        }
    }

    #[test]
    fn only_cache_marked_content_is_extracted() {
        let mut req = request(vec![Message {
            role: Role::User,
            content: vec![text("volatile question")],
        }]);
        req.system = vec![cached("stable system prompt"), text("volatile note")];
        assert_eq!(extract_cache_blocks(&req), vec!["stable system prompt"]);
    }

    #[test]
    fn cache_blocks_are_stable_as_the_conversation_grows() {
        // The failure this guards against: keying affinity on the whole
        // conversation gives a new key every turn, so nothing is ever pinned
        // and the prompt cache never hits — while appearing configured.
        let mut req = request(vec![Message {
            role: Role::User,
            content: vec![text("turn one")],
        }]);
        req.system = vec![cached("stable system prompt")];
        let early: Vec<String> = extract_cache_blocks(&req)
            .into_iter()
            .map(str::to_owned)
            .collect();

        for i in 0..10 {
            req.messages.push(Message {
                role: Role::Assistant,
                content: vec![text(&format!("reply {i}"))],
            });
        }
        assert_eq!(
            extract_cache_blocks(&req),
            early.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn signal_reflects_the_shape_of_the_request() {
        let mut req = request(vec![Message {
            role: Role::User,
            content: vec![text("fix this:\n```rust\nfn main() {}\n```")],
        }]);
        req.thinking_budget = Some(4096);
        req.tools = vec![Tool {
            name: "read_file".to_owned(),
            description: "reads a file".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,
        }];
        let s = req.signal();
        assert_eq!(s.tool_count, 1);
        assert_eq!(s.turn_count, 1);
        assert!(s.has_code, "a fenced block is code");
        assert!(s.thinking_requested);
        assert!(!s.has_images);
    }

    #[test]
    fn a_diff_counts_as_code() {
        let req = request(vec![Message {
            role: Role::User,
            content: vec![text("review:\n--- a/x.rs\n@@ -1 +1 @@\n-a\n+b")],
        }]);
        assert!(req.signal().has_code);
    }

    #[test]
    fn images_are_detected_and_not_sized_by_base64_length() {
        let req = request(vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".to_owned(),
                data: "A".repeat(500_000),
            }],
        }]);
        assert!(req.signal().has_images);
        // Half a megabyte of base64 must not read as 125k tokens.
        assert!(req.estimated_prompt_tokens() < 1_000);
    }

    #[test]
    fn thinking_signatures_round_trip_byte_exact() {
        // Providers reject a replayed thinking block whose signature changed,
        // so serialisation must not normalise it.
        let block = ContentBlock::Thinking {
            text: "reasoning".to_owned(),
            signature: Some("EqQBCkYIBRgCKkC+abc/123==".to_owned()),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn an_empty_request_has_no_cache_blocks() {
        assert!(extract_cache_blocks(&request(vec![])).is_empty());
    }
}

#[cfg(test)]
mod count_tests {
    use super::*;

    fn blank() -> CanonicalRequest {
        CanonicalRequest {
            model: "anthropic/opus".to_owned(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
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
        }
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text {
            text: t.to_owned(),
            cache_control: None,
        }
    }

    fn user(blocks: Vec<ContentBlock>) -> CanonicalRequest {
        CanonicalRequest {
            messages: vec![Message {
                role: Role::User,
                content: blocks,
            }],
            ..blank()
        }
    }

    #[test]
    fn an_image_is_priced_near_what_a_provider_bills_not_at_a_quarter_of_it() {
        // `estimated_prompt_tokens` values an image at 1_000 *bytes*, which its
        // divisor turns into 250 tokens — roughly 6x low against a real
        // screenshot. Low is the direction that breaks a compaction trigger,
        // which is why this endpoint does not reuse it.
        let req = user(vec![ContentBlock::Image {
            media_type: "image/png".to_owned(),
            data: "AAAA".to_owned(),
        }]);
        assert!(count_input_tokens(&req) > 1_000);
        assert!(req.estimated_prompt_tokens() < 500);
    }

    #[test]
    fn a_tool_schema_counts_denser_than_the_same_bytes_of_prose() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" }, "recursive": { "type": "boolean" } },
        });
        let with_tool = CanonicalRequest {
            tools: vec![Tool {
                name: "list_files".to_owned(),
                description: String::new(),
                input_schema: schema.clone(),
                cache_control: None,
            }],
            ..blank()
        };
        let as_prose = user(vec![text(&schema.to_string())]);
        assert!(count_input_tokens(&with_tool) > count_input_tokens(&as_prose));
    }

    #[test]
    fn the_routing_estimate_is_pinned_so_classifier_thresholds_cannot_drift() {
        // `estimated_prompt_tokens` feeds the classifier's 8000/100000 lines.
        // Changing it silently re-routes every deployment, so it gets a
        // regression guard rather than an opinion.
        let req = user(vec![text(&"a".repeat(4_000))]);
        assert_eq!(req.estimated_prompt_tokens(), 1_000);
    }

    #[test]
    fn framing_is_charged_per_message_so_many_short_turns_are_not_free() {
        let one = user(vec![text("hi")]);
        let many = CanonicalRequest {
            messages: (0..10)
                .map(|_| Message {
                    role: Role::User,
                    content: vec![text("hi")],
                })
                .collect(),
            ..blank()
        };
        assert!(count_input_tokens(&many) > count_input_tokens(&one) * 5);
    }
}
