//! The Bedrock adapter.
//!
//! Bedrock serves Anthropic's models in Anthropic's own body format, with three
//! differences that all bite on the first attempt:
//!
//! 1. **The model is in the URL**, not the body — and the body must not carry a
//!    `model` field at all, or the request is rejected.
//! 2. **`anthropic_version` replaces the version header**, and its value is a
//!    Bedrock-specific string rather than the API date.
//! 3. **SigV4 signing**, over the exact bytes being sent.
//!
//! Credentials are either long-lived IAM keys or temporary STS ones; the
//! session token, when present, is signed rather than merely attached.

use crate::adapter::{Framing, ProviderAdapter, UpstreamRequest};
use crate::sigv4::{self, Credentials};
use async_trait::async_trait;
use oag_core::{Provider, Result};
use oag_proto::{StreamAccumulator, StreamEvent, anthropic};

/// The value Bedrock expects in place of the `anthropic-version` header.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Talks to Bedrock's `InvokeModel` API.
#[derive(Debug, Clone)]
pub struct BedrockAdapter {
    region: String,
    /// Overrides the derived AWS endpoint — a VPC endpoint, a proxy, or a mock.
    ///
    /// The signed `host` follows it, because SigV4 signs the host header and a
    /// signature over the wrong host is rejected with an error that names
    /// neither.
    endpoint: Option<String>,
}

impl Default for BedrockAdapter {
    fn default() -> Self {
        Self::new("us-east-1")
    }
}

impl BedrockAdapter {
    #[must_use]
    pub fn new(region: impl Into<String>) -> Self {
        Self {
            region: region.into(),
            endpoint: None,
        }
    }

    /// Point at a specific endpoint instead of the regional AWS one.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: Option<String>) -> Self {
        self.endpoint = endpoint.filter(|e| !e.is_empty());
        self
    }

    /// The scheme and authority requests go to.
    fn origin(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| format!("https://bedrock-runtime.{}.amazonaws.com", self.region))
    }

    /// The value of the `host` header, which is also what gets signed.
    fn host(&self) -> String {
        self.origin()
            .split("://")
            .nth(1)
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_owned()
    }

    /// Split the stored credential into its SigV4 parts.
    ///
    /// Packed as `access_key:secret[:session_token]` in the sealed material, so
    /// Bedrock needs no separate credential shape from every other provider.
    fn credentials(raw: &str) -> Result<Credentials> {
        let mut parts = raw.splitn(3, ':');
        let access_key_id = parts.next().unwrap_or_default().to_owned();
        let secret_access_key = parts
            .next()
            .ok_or_else(|| {
                oag_core::Error::Config(
                    "a bedrock credential is 'access_key:secret[:session_token]'".to_owned(),
                )
            })?
            .to_owned();

        if access_key_id.is_empty() || secret_access_key.is_empty() {
            return Err(oag_core::Error::Config(
                "a bedrock credential is 'access_key:secret[:session_token]'".to_owned(),
            ));
        }

        Ok(Credentials {
            access_key_id,
            secret_access_key,
            session_token: parts.next().map(std::borrow::ToOwned::to_owned),
        })
    }
}

#[async_trait]
impl ProviderAdapter for BedrockAdapter {
    fn provider(&self) -> Provider {
        Provider::Bedrock
    }

    fn framing(&self) -> Framing {
        Framing::AwsEventStream
    }

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        let creds = Self::credentials(&req.credential.access_token)?;

        // Anthropic's body, minus the model, plus Bedrock's version marker.
        let mut body = anthropic::render_request(req.canonical, &req.model.upstream_name)?;
        if let Some(obj) = body.as_object_mut() {
            obj.remove("model");
            obj.remove("stream");
            obj.insert(
                "anthropic_version".to_owned(),
                serde_json::Value::String(BEDROCK_ANTHROPIC_VERSION.to_owned()),
            );
        }
        let bytes = serde_json::to_vec(&body)?;

        let action = if req.canonical.stream {
            "invoke-with-response-stream"
        } else {
            "invoke"
        };
        // The model id contains characters that must survive verbatim in the
        // path (`anthropic.claude-sonnet-4-v1:0`), including the colon.
        let path = format!("/model/{}/{action}", req.model.upstream_name);
        let host = self.host();
        let origin = self.origin();

        let signed = sigv4::sign(
            &creds,
            &self.region,
            "bedrock",
            sigv4::SigningRequest {
                method: "POST",
                path: &path,
                host: &host,
                body: &bytes,
            },
            time::OffsetDateTime::now_utc(),
        );

        let mut builder = reqwest::Client::new()
            .post(format!("{origin}{path}"))
            .header("content-type", "application/json")
            .header("host", &host)
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.content_sha256)
            .header("authorization", &signed.authorization);

        if let Some(token) = &signed.session_token {
            builder = builder.header("x-amz-security-token", token);
        }

        builder
            .body(bytes)
            .build()
            .map_err(|e| oag_core::Error::Internal(format!("building bedrock request: {e}")))
    }

    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
        // By this point the transport has already stripped Bedrock's binary
        // envelope (see `framing`), so `raw` is Anthropic's own event JSON.
        anthropic::parse_event(raw, acc)
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
            id: ModelId::new("bedrock/claude-sonnet-4"),
            provider: Provider::Bedrock,
            // The real shape, colon and all.
            upstream_name: "anthropic.claude-sonnet-4-v1:0".to_owned(),
            pricing: Pricing {
                input_per_mtok: dec!(3),
                output_per_mtok: dec!(15),
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 200_000,
            max_output_tokens: 8192,
            capabilities: Capabilities::default(),
            display_label: None,
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

    fn cred(raw: &str) -> SecretMaterial {
        SecretMaterial {
            access_token: raw.to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        }
    }

    #[test]
    fn the_body_carries_the_version_marker_and_no_model() {
        // Bedrock rejects a body containing `model`; the model is in the path.
        let a = BedrockAdapter::default();
        let c = request(false);
        let m = model();
        let cr = cred("AKIDEXAMPLE:secret");
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");

        let body = req.body().and_then(reqwest::Body::as_bytes).expect("body");
        let v: serde_json::Value = serde_json::from_slice(body).expect("json");
        assert!(v.get("model").is_none(), "model must not be in the body");
        assert!(v.get("stream").is_none());
        assert_eq!(v["anthropic_version"], BEDROCK_ANTHROPIC_VERSION);
        assert_eq!(v["max_tokens"], 256);
    }

    #[test]
    fn the_model_id_survives_in_the_path_colon_and_all() {
        let a = BedrockAdapter::new("eu-west-1");
        let c = request(false);
        let m = model();
        let cr = cred("AKIDEXAMPLE:secret");
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");
        let url = req.url().as_str();
        assert!(
            url.contains("bedrock-runtime.eu-west-1.amazonaws.com"),
            "{url}"
        );
        assert!(url.contains("anthropic.claude-sonnet-4-v1:0"), "{url}");
        assert!(url.ends_with("/invoke"), "{url}");
    }

    #[test]
    fn streaming_uses_the_other_action() {
        let a = BedrockAdapter::default();
        let c = request(true);
        let m = model();
        let cr = cred("AKIDEXAMPLE:secret");
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");
        assert!(req.url().as_str().ends_with("/invoke-with-response-stream"));
    }

    #[test]
    fn the_request_is_signed() {
        let a = BedrockAdapter::default();
        let c = request(false);
        let m = model();
        let cr = cred("AKIDEXAMPLE:secret");
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");

        let auth = req.headers()["authorization"].to_str().expect("header");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
            "{auth}"
        );
        assert!(auth.contains("/bedrock/aws4_request"), "{auth}");
        assert!(req.headers().contains_key("x-amz-date"));
        assert!(req.headers().contains_key("x-amz-content-sha256"));
    }

    #[test]
    fn temporary_credentials_carry_their_session_token() {
        let a = BedrockAdapter::default();
        let c = request(false);
        let m = model();
        let cr = cred("AKIDEXAMPLE:secret:SESSIONTOKEN");
        let req = a
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: &cr,
            })
            .expect("builds");
        assert_eq!(req.headers()["x-amz-security-token"], "SESSIONTOKEN");
    }

    #[test]
    fn an_endpoint_override_moves_the_signed_host_with_it() {
        // SigV4 signs the host header; signing the AWS hostname while sending
        // to another one is rejected with an error that names neither.
        let a = BedrockAdapter::new("us-east-1")
            .with_endpoint(Some("http://127.0.0.1:9012".to_owned()));
        let c = request(false);
        let m = model();
        let cr = cred("AKIDEXAMPLE:secret");
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
                .starts_with("http://127.0.0.1:9012/model/"),
            "{}",
            req.url()
        );
        assert_eq!(req.headers()["host"], "127.0.0.1:9012");
        let auth = req.headers()["authorization"].to_str().expect("header");
        assert!(
            auth.contains("SignedHeaders=host;"),
            "host must still be signed"
        );
    }

    #[test]
    fn without_an_override_it_derives_the_regional_endpoint() {
        let a = BedrockAdapter::new("ap-southeast-2");
        assert_eq!(a.host(), "bedrock-runtime.ap-southeast-2.amazonaws.com");
        assert!(a.origin().starts_with("https://"));
    }

    #[test]
    fn an_empty_override_is_treated_as_no_override() {
        let a = BedrockAdapter::new("us-east-1").with_endpoint(Some(String::new()));
        assert_eq!(a.host(), "bedrock-runtime.us-east-1.amazonaws.com");
    }

    #[test]
    fn it_declares_binary_framing() {
        // The default is SSE; getting this wrong yields an empty stream and
        // zero usage rather than an error.
        assert_eq!(BedrockAdapter::default().framing(), Framing::AwsEventStream);
    }

    #[test]
    fn a_malformed_credential_fails_with_the_expected_shape() {
        // Better than a signature error from AWS that names nothing.
        assert!(BedrockAdapter::credentials("just-a-key").is_err());
        assert!(BedrockAdapter::credentials("").is_err());
        assert!(BedrockAdapter::credentials("key:secret").is_ok());
        assert!(BedrockAdapter::credentials("key:secret:token").is_ok());
    }
}
