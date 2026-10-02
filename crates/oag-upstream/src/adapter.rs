//! The per-provider contract.
//!
//! One implementation per provider, and deliberately narrow: build a request,
//! interpret a response. Everything a provider does *not* need to know about —
//! which credential to use, whether to retry, what it cost — is decided before
//! the adapter is called. The trait is the shape: adding a provider means
//! implementing it.

use async_trait::async_trait;
use oag_core::provider::Dialect;
use oag_core::{AccountId, Provider, Result, credential::SecretMaterial};
use oag_proto::{CanonicalRequest, StreamAccumulator, StreamEvent};
use oag_router::ModelSpec;
use std::borrow::Cow;

/// Everything needed to call an upstream, once routing has decided.
#[derive(Debug, Clone)]
pub struct UpstreamRequest<'a> {
    pub canonical: &'a CanonicalRequest,
    pub model: &'a ModelSpec,
    pub credential: &'a SecretMaterial,
    /// A stable id for the conversation this request continues, for an
    /// upstream that wants one: a Codex seat sends it as `session_id`, and a
    /// person's own CLI keeps one id for a whole conversation. `None` where the
    /// caller has no conversation to name; the adapter then makes one up.
    pub session: Option<uuid::Uuid>,
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
    /// The same binary messages as Bedrock's `ConverseStream` sends them: the
    /// payload is one Converse event's JSON as it is, and which event it is
    /// is said only by the message's `:event-type` header.
    AwsConverseStream,
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
    ///
    /// `proxy` is the credential's own `proxy_url`. It was applied only to
    /// inference, so a deployment whose egress must go through a proxy had its
    /// refresh traffic leave by another route — sometimes failing, sometimes
    /// succeeding and bypassing the control the proxy existed to enforce.
    async fn refresh(
        &self,
        _credential: &SecretMaterial,
        _proxy: Option<&str>,
    ) -> Result<Option<SecretMaterial>> {
        Ok(None)
    }

    /// The credential a request to this adapter is built with, from the one
    /// stored for `account`.
    ///
    /// For almost every adapter that is the stored one, unchanged, which the
    /// default hands back without a copy. It differs where what is stored is
    /// not what goes on the wire: a Google service account is a JSON key, and
    /// a request carries a short-lived token minted from it. `account` names
    /// whose credential this is, for an adapter that keeps what it minted.
    ///
    /// Called on the request path once per credential tried, after `refresh`
    /// and before [`ProviderAdapter::build`], which receives what this
    /// returns. An error here is the credential's: the request moves on to the
    /// next one, as it does when a refresh fails.
    async fn prepare_credential<'a>(
        &'a self,
        _account: AccountId,
        stored: &'a SecretMaterial,
    ) -> Result<Cow<'a, SecretMaterial>> {
        Ok(Cow::Borrowed(stored))
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
    async fn served_models(
        &self,
        _credential: &SecretMaterial,
        _proxy: Option<&str>,
    ) -> Result<Option<Vec<String>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An adapter that overrides nothing it does not have to.
    #[derive(Debug)]
    struct Defaults;

    #[async_trait]
    impl ProviderAdapter for Defaults {
        fn provider(&self) -> Provider {
            Provider::OpenAI
        }

        fn build(&self, _req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
            Err(oag_core::Error::Internal("never built".to_owned()))
        }

        fn parse_event(
            &self,
            _raw: &str,
            _acc: &mut StreamAccumulator,
        ) -> Result<Vec<StreamEvent>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn by_default_a_request_is_built_with_the_stored_credential_itself() {
        let stored = SecretMaterial {
            access_token: "stored-key".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 3,
            client_id: None,
            account_id: None,
        };
        let prepared = Defaults
            .prepare_credential(AccountId::from_uuid(uuid::Uuid::nil()), &stored)
            .await
            .expect("the default never fails");
        let Cow::Borrowed(same) = prepared else {
            panic!("the default copied the credential it was handed");
        };
        assert!(
            std::ptr::eq(same, &raw const stored),
            "the default hands back the very credential it was given"
        );
    }
}
