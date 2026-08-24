//! The Gemini adapter.

use crate::adapter::{ProviderAdapter, UpstreamRequest};
use async_trait::async_trait;
use oag_core::{Provider, Result};
use oag_proto::{StreamAccumulator, StreamEvent, gemini};

/// Talks to Gemini's `generateContent` API.
#[derive(Debug, Clone)]
pub struct GeminiAdapter {
    base_url: String,
}

impl Default for GeminiAdapter {
    fn default() -> Self {
        Self::new("https://generativelanguage.googleapis.com/v1beta")
    }
}

impl GeminiAdapter {
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl ProviderAdapter for GeminiAdapter {
    fn provider(&self) -> Provider {
        Provider::Gemini
    }

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        let body = gemini::render_request(req.canonical)?;

        // The model and the streaming mode are both in the path here, not the
        // body — and the separator before the method is a colon, which is
        // unusual enough to be worth pointing at.
        let method = if req.canonical.stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        let url = format!(
            "{}/models/{}:{}",
            self.base_url, req.model.upstream_name, method
        );

        reqwest::Client::new()
            .post(&url)
            .header("content-type", "application/json")
            // Its own header, not Authorization and not x-api-key.
            .header("x-goog-api-key", &req.credential.access_token)
            .json(&body)
            .build()
            .map_err(|e| oag_core::Error::Internal(format!("building gemini request: {e}")))
    }

    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
        gemini::parse_event(raw, acc)
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
            id: ModelId::new("gemini/gemini-2.5-pro"),
            provider: Provider::Gemini,
            upstream_name: "gemini-2.5-pro".to_owned(),
            pricing: Pricing {
                input_per_mtok: dec!(1.25),
                output_per_mtok: dec!(10),
                cache_read_per_mtok: Some(dec!(0.31)),
                cache_write_per_mtok: None,
            },
            context_window: 1_000_000,
            max_output_tokens: 65_536,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                reasoning: true,
                prompt_cache: true,
            },
        }
    }

    fn request(stream: bool) -> CanonicalRequest {
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
            stream,
            temperature: None,
            thinking_budget: None,
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
        }
    }

    #[test]
    fn the_model_and_mode_are_in_the_path_not_the_body() {
        let a = GeminiAdapter::default();
        let c = request(true);
        let m = model();
        let cr = cred();
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");

        assert!(
            req.url()
                .as_str()
                .contains("/models/gemini-2.5-pro:streamGenerateContent"),
            "{}",
            req.url()
        );
        assert!(
            req.url().as_str().contains("alt=sse"),
            "SSE must be requested"
        );

        let body = req.body().and_then(reqwest::Body::as_bytes).expect("body");
        let v: serde_json::Value = serde_json::from_slice(body).expect("json");
        assert!(v.get("model").is_none());
        assert!(v.get("stream").is_none());
    }

    #[test]
    fn non_streaming_uses_the_other_method() {
        let a = GeminiAdapter::default();
        let c = request(false);
        let m = model();
        let cr = cred();
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");
        assert!(
            req.url().as_str().ends_with(":generateContent"),
            "{}",
            req.url()
        );
    }

    #[test]
    fn it_authenticates_with_its_own_header() {
        // Not Authorization, and not x-api-key.
        let a = GeminiAdapter::default();
        let c = request(false);
        let m = model();
        let cr = cred();
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");
        assert!(req.headers().contains_key("x-goog-api-key"));
        assert!(!req.headers().contains_key("authorization"));
        assert!(!req.headers().contains_key("x-api-key"));
    }
}
