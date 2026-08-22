//! The per-provider contract.
//!
//! One implementation per provider, and deliberately narrow: build a request,
//! interpret a response. Everything a provider does *not* need to know about —
//! which credential to use, whether to retry, what it cost — is decided before
//! the adapter is called. sub2api's equivalent is duck-typed across four
//! concrete services with no interface at all, which is why adding a provider
//! there means reading four of them to work out the shape.

use async_trait::async_trait;
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

#[async_trait]
pub trait ProviderAdapter: Send + Sync + std::fmt::Debug {
    fn provider(&self) -> Provider;

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
}
