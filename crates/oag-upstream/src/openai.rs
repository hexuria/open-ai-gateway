//! The OpenAI-compatible adapter.
//!
//! One adapter, five providers. OpenAI, Kimi, DeepSeek, Zhipu, and xAI all
//! speak Chat Completions, differing only in base URL and model names — so
//! adding one of them is a catalog entry and a base URL, not code.

use crate::adapter::{ProviderAdapter, UpstreamRequest};
use async_trait::async_trait;
use oag_core::{Provider, Result};
use oag_proto::{StreamAccumulator, StreamEvent, openai};

/// Talks Chat Completions to whichever provider it was built for.
#[derive(Debug, Clone)]
pub struct OpenAICompatAdapter {
    provider: Provider,
    base_url: String,
    /// Where xAI's OIDC server lives. Only the refresh path reads it, and only
    /// for `Provider::XAI`; tests point it at a local mock.
    auth_base: String,
}

impl OpenAICompatAdapter {
    #[must_use]
    pub fn new(provider: Provider, base_url: impl Into<String>) -> Self {
        Self {
            provider,
            base_url: base_url.into(),
            auth_base: crate::xai_oauth::DEFAULT_AUTH_BASE.to_owned(),
        }
    }

    /// Override the OIDC server, for tests against a local mock.
    #[must_use]
    pub fn with_auth_base(mut self, auth_base: impl Into<String>) -> Self {
        self.auth_base = auth_base.into();
        self
    }

    /// The public endpoint for a provider, where it has one.
    #[must_use]
    pub fn default_base_url(provider: Provider) -> &'static str {
        match provider {
            Provider::OpenAI => "https://api.openai.com/v1",
            Provider::Kimi => "https://api.moonshot.cn/v1",
            Provider::DeepSeek => "https://api.deepseek.com/v1",
            Provider::Zhipu => "https://open.bigmodel.cn/api/paas/v4",
            Provider::XAI => "https://api.x.ai/v1",
            // Not Chat Completions providers; this adapter should not be built
            // for them, and an obviously-wrong URL fails loudly if it is.
            _ => "",
        }
    }
}

#[async_trait]
impl ProviderAdapter for OpenAICompatAdapter {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        if self.base_url.is_empty() {
            return Err(oag_core::Error::Internal(format!(
                "{} does not speak Chat Completions; it needs its own adapter",
                self.provider
            )));
        }

        let body = openai::render_request(req.canonical, &req.model.upstream_name)?;

        reqwest::Client::new()
            .post(format!("{}/chat/completions", self.base_url))
            .header("content-type", "application/json")
            // Bearer for every one of them; the dialect's single convention.
            .header(
                "authorization",
                format!("Bearer {}", req.credential.access_token),
            )
            .json(&body)
            .build()
            .map_err(|e| oag_core::Error::Internal(format!("building request: {e}")))
    }

    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
        openai::parse_event(raw, acc)
    }

    async fn refresh(
        &self,
        credential: &oag_core::credential::SecretMaterial,
    ) -> Result<Option<oag_core::credential::SecretMaterial>> {
        // Two of these providers issue subscription OAuth tokens; the rest hold
        // static keys, for which "nothing to refresh" is the correct answer.
        // Both OAuth paths no-op on a credential with no refresh token, so a
        // plain OpenAI or xAI API key falls through them safely.
        match self.provider {
            Provider::XAI => crate::xai_oauth::refresh(credential, &self.auth_base).await,
            Provider::OpenAI => {
                crate::openai_oauth::refresh(credential, crate::openai_oauth::DEFAULT_TOKEN_URL)
                    .await
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_core::credential::SecretMaterial;
    use oag_proto::{CanonicalRequest, ContentBlock, Message, Role};
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::dec;

    fn model(provider: Provider, upstream: &str) -> ModelSpec {
        ModelSpec {
            id: ModelId::new(format!("{provider}/{upstream}")),
            provider,
            upstream_name: upstream.to_owned(),
            pricing: Pricing {
                input_per_mtok: dec!(1),
                output_per_mtok: dec!(2),
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 128_000,
            max_output_tokens: 8192,
            capabilities: Capabilities::default(),
            display_label: None,
        }
    }

    fn request() -> CanonicalRequest {
        CanonicalRequest {
            model: "oag/auto".to_owned(),
            system: vec![],
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_owned(),
                    cache_control: None,
                }],
            }],
            tools: vec![],
            max_tokens: 256,
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

    fn cred() -> SecretMaterial {
        SecretMaterial {
            access_token: "test-token".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        }
    }

    #[test]
    fn every_compatible_provider_builds_the_same_shape() {
        // The payoff: five providers, one code path.
        for provider in [
            Provider::OpenAI,
            Provider::Kimi,
            Provider::DeepSeek,
            Provider::Zhipu,
            Provider::XAI,
        ] {
            let base = OpenAICompatAdapter::default_base_url(provider);
            assert!(!base.is_empty(), "{provider} needs a base url");

            let a = OpenAICompatAdapter::new(provider, base);
            let c = request();
            let m = model(provider, "some-model");
            let cr = cred();
            let req = a
                .build(&UpstreamRequest {
                    canonical: &c,
                    model: &m,
                    credential: &cr,
                })
                .expect("builds");

            assert!(
                req.url().as_str().ends_with("/chat/completions"),
                "{provider}"
            );
            assert!(req.headers().contains_key("authorization"), "{provider}");
        }
    }

    #[test]
    fn a_provider_that_does_not_speak_this_dialect_fails_loudly() {
        // Better a clear error than a request to an empty URL.
        let a = OpenAICompatAdapter::new(Provider::Anthropic, "");
        let c = request();
        let m = model(Provider::Anthropic, "claude");
        let cr = cred();
        let err = a.build(&UpstreamRequest {
            canonical: &c,
            model: &m,
            credential: &cr,
        });
        assert!(err.is_err());
    }

    #[test]
    fn a_streamed_request_asks_for_usage() {
        // Without stream_options.include_usage this dialect omits usage
        // entirely, and every streamed request would bill as zero.
        let a = OpenAICompatAdapter::new(Provider::Kimi, "https://example.invalid/v1");
        let c = request();
        let m = model(Provider::Kimi, "kimi-k2");
        let cr = cred();
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");
        let body = req.body().and_then(reqwest::Body::as_bytes).expect("body");
        let v: serde_json::Value = serde_json::from_slice(body).expect("json");
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert_eq!(v["model"], "kimi-k2");
    }
}
