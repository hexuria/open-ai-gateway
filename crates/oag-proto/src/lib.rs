#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Wire-format translation.
//!
//! Four dialects are in play — Anthropic Messages, OpenAI Chat Completions,
//! OpenAI Responses, Gemini `generateContent` — and any client dialect may need
//! to reach any upstream one. Pairwise converters would be twelve of them, each
//! with its own streaming state machine; sub2api learned this and settled on a
//! hub, and so do we. Everything converts to and from one canonical
//! representation, so adding a fifth dialect is two converters rather than
//! eight.
//!
//! Anthropic Messages is the hub's shape because it is the most expressive of
//! the four: it has explicit content blocks, tool results as first-class
//! content, and cache breakpoints. Lowering from it loses less than raising to
//! it would gain.
//!
//! The crate is pure. No network, no clock, no I/O — which is what makes a
//! recorded-fixture test corpus possible, and translation is precisely the
//! thing you want that many tests on.

pub mod anthropic;
pub mod canonical;
pub mod gemini;
pub mod openai;
pub mod responses;
pub mod stream;

pub use canonical::{
    CacheControl, CanonicalRequest, ContentBlock, Message, ResponseFormat, Role, Tool, ToolChoice,
    count_input_tokens, extract_cache_blocks,
};
pub use stream::{StopReason, StreamAccumulator, StreamEvent};

use oag_core::provider::Dialect;

/// Translates between a dialect and the canonical hub.
///
/// Implemented once per dialect rather than once per pair. Each half is
/// independently testable: parse a recorded request, assert the canonical form;
/// render a canonical form, assert the wire bytes.
pub trait DialectCodec: Send + Sync {
    fn dialect(&self) -> Dialect;

    /// Wire bytes from a client → canonical.
    fn parse_request(&self, body: &serde_json::Value) -> oag_core::Result<CanonicalRequest>;

    /// Canonical → wire bytes for an upstream.
    fn render_request(&self, req: &CanonicalRequest) -> oag_core::Result<serde_json::Value>;

    /// One upstream SSE event → zero or more canonical events.
    ///
    /// Zero-or-more rather than one-to-one because the dialects genuinely do
    /// not line up: one Anthropic `content_block_start` plus its deltas is a
    /// single OpenAI chunk, and one OpenAI chunk carrying both content and a
    /// tool call is two canonical events.
    fn parse_event(
        &self,
        raw: &str,
        acc: &mut StreamAccumulator,
    ) -> oag_core::Result<Vec<StreamEvent>>;

    /// Canonical event → wire bytes for the client's dialect.
    fn render_event(
        &self,
        event: &StreamEvent,
        acc: &mut StreamAccumulator,
    ) -> oag_core::Result<Option<String>>;
}
