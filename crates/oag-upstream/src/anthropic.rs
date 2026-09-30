//! The Anthropic adapter.

use crate::adapter::{ProviderAdapter, UpstreamRequest};
use crate::custom::ExtraHeaders;
use async_trait::async_trait;
use oag_core::provider::{AuthStyle, Endpoint};
use oag_core::{Provider, Result};
use oag_proto::{StreamAccumulator, StreamEvent, anthropic};

/// Talks to the Anthropic Messages API, or to an endpoint that speaks it.
#[derive(Debug, Clone)]
pub struct AnthropicAdapter {
    /// `Provider::Anthropic` from [`AnthropicAdapter::new`], and the endpoint
    /// from [`AnthropicAdapter::for_endpoint`]: nothing else.
    provider: Provider,
    base_url: String,
    /// `XApiKey` for Anthropic; an endpoint's own otherwise.
    auth: AuthStyle,
    /// Added to every request. None for Anthropic.
    headers: ExtraHeaders,
    /// Whether `served_models` reads `{base}/v1/models`. Only an endpoint
    /// whose operator asked for it; never Anthropic itself.
    discover: bool,
}

impl Default for AnthropicAdapter {
    fn default() -> Self {
        Self::new("https://api.anthropic.com")
    }
}

impl AnthropicAdapter {
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            provider: Provider::Anthropic,
            base_url: base_url.into(),
            auth: AuthStyle::XApiKey,
            headers: ExtraHeaders::default(),
            discover: false,
        }
    }

    /// For an endpoint that speaks the Messages API: its name, its base URL,
    /// the header its key goes in and the headers its operator added.
    ///
    /// Takes an [`Endpoint`] rather than any provider, so the adapter it
    /// makes is always `Provider::Custom` and the only one serving
    /// `Provider::Anthropic` is the one [`AnthropicAdapter::new`] makes, which
    /// sends `x-api-key` whatever it is given.
    #[must_use]
    pub fn for_endpoint(
        endpoint: Endpoint,
        base_url: impl Into<String>,
        auth: AuthStyle,
        headers: ExtraHeaders,
    ) -> Self {
        Self {
            provider: Provider::Custom(endpoint),
            base_url: base_url.into(),
            auth,
            headers,
            discover: false,
        }
    }

    /// Answer `served_models` from the endpoint's own `{base}/v1/models`.
    #[must_use]
    pub fn with_discovery(mut self, discover: bool) -> Self {
        self.discover = discover;
        self
    }
}

#[async_trait]
impl ProviderAdapter for AnthropicAdapter {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        let body = anthropic::render_request(req.canonical, &req.model.upstream_name)?;
        let url = format!("{}/v1/messages", self.base_url);

        let builder = crate::builder_client()?
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", anthropic::API_VERSION)
            .json(&body);

        // `x-api-key`, always, for Anthropic. Anthropic credentials in this
        // gateway are API keys and only API keys.
        //
        // There used to be a bearer-token branch here, taken when the
        // credential carried a refresh token — for a Claude.ai subscription
        // seat. Nothing can produce one: `account add` creates an API key and
        // the two seat importers read Grok and Codex sessions. Nor should
        // anything — `docs/03-providers.md` lists Anthropic OAuth as
        // Prohibited, quoting Anthropic's own terms in `docs/compliance.md`:
        // third parties may not "store, or intermediate Claude.ai credentials
        // or session tokens".
        // So the branch was unreachable, and it would not have worked if
        // reached: a Claude.ai token needs the `anthropic-beta: oauth` header
        // this request never sends, and Anthropic rejects a bearer token
        // without it rather than accepting either form.
        //
        // Dead code that looks like a supported path is worse than no code:
        // it is the first thing someone reads when asking whether seats work
        // here, and it answers yes.
        //
        // An endpoint that speaks this dialect is not Anthropic, and sends its
        // key in whichever header it was registered with, a bearer token
        // included. `new` sets `XApiKey` and nothing changes it; only
        // `for_endpoint` sets another, and its provider is always
        // `Provider::Custom`. Neither path reads the credential's refresh
        // token, and neither refreshes: no endpoint takes an `oauth` row
        // (`Provider::support`), so no seat reaches it. Keeping an endpoint off
        // Anthropic's own hosts has to happen where its base URL is accepted:
        // this adapter cannot tell one opaque key from another.
        let builder = crate::custom::authenticate(builder, self.auth, &req.credential.access_token);

        self.headers
            .apply(builder)
            .build()
            .map_err(|e| oag_core::Error::Internal(format!("building anthropic request: {e}")))
    }

    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
        anthropic::parse_event(raw, acc)
    }

    async fn served_models(
        &self,
        credential: &oag_core::credential::SecretMaterial,
        proxy: Option<&str>,
    ) -> Result<Option<Vec<String>>> {
        if !self.discover {
            return Ok(None);
        }
        let source = crate::listing::ModelSource {
            dialect: oag_core::provider::Dialect::AnthropicMessages,
            base_url: &self.base_url,
            auth: self.auth,
            headers: &self.headers,
        };
        let served = crate::listing::served(&source, &credential.access_token, proxy).await?;
        Ok(Some(served.models))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_core::credential::SecretMaterial;
    use oag_proto::{CanonicalRequest, ContentBlock, Message, Role};
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::dec;

    fn model() -> ModelSpec {
        ModelSpec {
            id: ModelId::new("anthropic/claude-opus-5"),
            provider: Provider::Anthropic,
            upstream_name: "claude-opus-5".to_owned(),
            pricing: Pricing {
                input_per_mtok: dec!(15),
                output_per_mtok: dec!(75),
                cache_read_per_mtok: Some(dec!(1.5)),
                cache_write_per_mtok: Some(dec!(18.75)),
            },
            context_window: 400_000,
            max_output_tokens: 64_000,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                reasoning: true,
                prompt_cache: true,
            },
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
                    text: "hello".to_owned(),
                    cache_control: None,
                }],
            }],
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
            passthrough: None,
        }
    }

    fn api_key() -> SecretMaterial {
        SecretMaterial {
            access_token: "FAKE-CREDENTIAL-FOR-TESTS".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        }
    }

    fn oauth() -> SecretMaterial {
        SecretMaterial {
            access_token: "oauth-access-token".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            expires_at: Some(9_999_999_999),
            version: 1,
            client_id: None,
            account_id: None,
        }
    }

    #[test]
    fn an_api_key_authenticates_with_x_api_key() {
        let a = AnthropicAdapter::default();
        let canonical = request();
        let m = model();
        let cred = api_key();
        let req = a
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &m,
                credential: &cred,
                session: None,
            })
            .expect("builds");

        assert_eq!(req.url().as_str(), "https://api.anthropic.com/v1/messages");
        assert!(req.headers().contains_key("x-api-key"));
        assert!(!req.headers().contains_key("authorization"));
        assert_eq!(req.headers()["anthropic-version"], anthropic::API_VERSION);
    }

    /// U13. Anthropic credentials authenticate with `x-api-key`, always.
    ///
    /// This test used to require the opposite for a credential carrying a
    /// refresh token — a Claude.ai subscription seat. That branch was
    /// unreachable (nothing can produce such a credential: `account add`
    /// creates an API key and the two importers read Grok and Codex sessions)
    /// and would not have worked if reached: a Claude.ai token needs an
    /// `anthropic-beta: oauth` header this request never sends.
    ///
    /// It should also never be reachable. `docs/03-providers.md` lists
    /// Anthropic OAuth as Prohibited, quoting Anthropic's own terms in
    /// `docs/compliance.md`: third parties may not "store, or intermediate
    /// Claude.ai credentials or session tokens". So the test asserts the rule
    /// rather than the dead branch — including for a credential that happens to
    /// carry a refresh token, which is the shape that used to divert.
    #[test]
    fn every_anthropic_credential_authenticates_with_the_api_key_header() {
        let a = AnthropicAdapter::default();
        let canonical = request();
        let m = model();

        for cred in [api_key(), oauth()] {
            let req = a
                .build(&UpstreamRequest {
                    canonical: &canonical,
                    model: &m,
                    credential: &cred,
                    session: None,
                })
                .expect("builds");

            assert!(
                req.headers().contains_key("x-api-key"),
                "an Anthropic credential is an API key, whatever else it carries"
            );
            assert!(
                !req.headers().contains_key("authorization"),
                "a bearer token here would be intermediating a Claude.ai session, \
                 which Anthropic's terms forbid"
            );
        }
    }

    #[test]
    fn the_upstream_name_is_sent_not_the_canonical_id() {
        // The client asked for `oag/auto`; the wire must carry the provider's
        // own name for the model the router chose.
        let a = AnthropicAdapter::default();
        let canonical = request();
        let m = model();
        let cred = api_key();
        let req = a
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &m,
                credential: &cred,
                session: None,
            })
            .expect("builds");

        let body = req
            .body()
            .and_then(reqwest::Body::as_bytes)
            .expect("has a body");
        let parsed: serde_json::Value = serde_json::from_slice(body).expect("valid json");
        assert_eq!(parsed["model"], "claude-opus-5");
        assert_ne!(parsed["model"], "oag/auto");
    }

    #[test]
    fn a_custom_base_url_is_honoured() {
        // Needed to point at a mock server in tests, and at a gateway-in-front
        // -of-a-gateway in some deployments.
        let a = AnthropicAdapter::new("http://127.0.0.1:9999");
        let canonical = request();
        let m = model();
        let cred = api_key();
        let req = a
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &m,
                credential: &cred,
                session: None,
            })
            .expect("builds");
        assert_eq!(req.url().as_str(), "http://127.0.0.1:9999/v1/messages");
    }
}
