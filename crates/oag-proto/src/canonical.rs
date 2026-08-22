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
    /// A session identifier the client supplied, if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_session: Option<String>,
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
            client_session: None,
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
