//! The per-provider contract.
//!
//! One implementation per provider, and deliberately narrow: build a request,
//! interpret a response. Everything a provider does *not* need to know about —
//! which credential to use, whether to retry, what it cost — is decided before
//! the adapter is called. sub2api's equivalent is duck-typed across four
//! concrete services with no interface at all, which is why adding a provider
//! there means reading four of them to work out the shape.

use async_trait::async_trait;
use oag_core::provider::Dialect;
use oag_core::{Provider, Result, credential::SecretMaterial};
use oag_proto::{CanonicalRequest, StreamAccumulator, StreamEvent};
use oag_router::ModelSpec;

/// Everything needed to call an upstream, once routing has decided.
#[derive(Debug, Clone)]
pub struct UpstreamRequest<'a> {
    pub canonical: &'a CanonicalRequest,
    pub model: &'a ModelSpec,
    pub credential: &'a SecretMaterial,
}

/// How an upstream delimits the events in a streamed response.
///
/// Not every provider streams server-sent events. Assuming they do is how a
/// Bedrock stream produces zero frames: its framing is binary, a reader
/// splitting on blank lines finds nothing, and the failure is silent — an empty
/// response and zero recorded usage rather than an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `data:` lines separated by a blank line. Everything except Bedrock.
    Sse,
    /// AWS `vnd.amazon.eventstream`: length-prefixed binary messages whose
    /// payload carries the provider's own event, base64-encoded.
    AwsEventStream,
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync + std::fmt::Debug {
    fn provider(&self) -> Provider;

    /// How this provider delimits streamed events.
    ///
    /// Defaults to SSE because all but one do; the one that does not overrides
    /// it, and the compiler is no help there — hence the loud note above.
    fn framing(&self) -> Framing {
        Framing::Sse
    }

    /// The dialect this adapter actually speaks.
    ///
    /// Defaults to the provider's, which is right for every adapter chosen by
    /// provider alone. It is NOT right for one chosen by credential: a Codex
    /// subscription is `Provider::OpenAI` and speaks Responses, so a caller
    /// asking the provider is told Chat Completions and forwards Responses
    /// bytes verbatim to a client that cannot read them — a 200 carrying a
    /// body the client sees as empty.
    ///
    /// The same shape as `framing` above, and for the same reason: both are
    /// facts about the ADAPTER that the provider cannot answer for. Framing
    /// learned it from Bedrock; this learned it from Codex.
    fn dialect(&self) -> Dialect {
        self.provider().native_dialect()
    }

    /// Whether this adapter streams the upstream regardless of what the client
    /// asked for.
    ///
    /// The third fact in the same family as `framing` and `dialect`, and for
    /// the same reason: a Codex seat's backend only streams, so the adapter
    /// forces `stream: true` on every request. Nothing downstream could tell.
    /// The collector branched on the *client's* `stream` flag, read an SSE
    /// transcript as a JSON body, found nothing it recognised, and handed the
    /// raw `data:` lines back under `application/json` — while the ledger
    /// recorded zero tokens for an answer the seat had fully generated. The
    /// response path asks this to know it must read the stream and render a
    /// body from it.
    fn always_streams(&self) -> bool {
        false
    }

    /// Build the HTTP request. Sets the URL, auth header, and body.
    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request>;

    /// Turn one raw SSE line into canonical events.
    ///
    /// Returns a `Vec` because the mapping is not one-to-one: an Anthropic
    /// `content_block_start` plus its deltas is a single OpenAI chunk, and one
    /// OpenAI chunk carrying both content and a tool call is two canonical
    /// events. Returning an empty vec is normal — most dialects emit heartbeat
    /// and bookkeeping lines that carry nothing.
    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>>;

    /// Refresh an expiring credential.
    ///
    /// Default is "no refresh needed", which is correct for every static API
    /// key — the majority — so only OAuth-style adapters implement it.
    async fn refresh(&self, _credential: &SecretMaterial) -> Result<Option<SecretMaterial>> {
        Ok(None)
    }

    /// Which models this *credential* can be used with, as the provider's own
    /// upstream names.
    ///
    /// A fact about the CREDENTIAL, not about the provider, which is the whole
    /// reason it lives on the adapter: a Codex subscription and an ordinary
    /// OpenAI API key are both `Provider::OpenAI` and serve different sets, so
    /// a caller asking the provider gets an answer that is wrong for one of
    /// them. The same shape as `framing` and `dialect` above, and the third
    /// time Codex has taught this lesson.
    ///
    /// `None` means this adapter cannot be asked, and a caller must conclude
    /// nothing from it — notably not that the credential serves nothing. An
    /// empty `Vec` is the different, stronger claim that it was asked and the
    /// answer was none.
    async fn served_models(&self, _credential: &SecretMaterial) -> Result<Option<Vec<String>>> {
        Ok(None)
    }
}
