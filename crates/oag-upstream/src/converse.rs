//! The Bedrock Converse adapter: every model Bedrock serves — Llama, Mistral,
//! Nova, Claude — in one request shape, at `POST /model/{modelId}/converse`
//! and `/converse-stream`.
//!
//! Only an endpoint on the `aws` platform with the `bedrock_converse` dialect
//! is served by it. The built-in `bedrock` provider speaks Anthropic's body
//! through `InvokeModel`, and is untouched.
//!
//! What the two APIs share — the host, the region, the credential and its
//! `SigV4` signature — is the Bedrock adapter's, and this one rides on it:
//! they are two operations of one service. What differs is the path, the body
//! (`oag_proto::converse`), and the stream, whose events are named by a header
//! instead of arriving base64-wrapped ([`Framing::AwsConverseStream`]).

use crate::adapter::{Framing, ProviderAdapter, UpstreamRequest};
use crate::bedrock::BedrockAdapter;
use async_trait::async_trait;
use oag_core::provider::Dialect;
use oag_core::{Provider, Result};
use oag_proto::{StreamAccumulator, StreamEvent};

/// Talks to Bedrock's `Converse` API.
#[derive(Debug, Clone)]
pub struct ConverseAdapter {
    /// Where requests go, whose they are, and how they are signed: the
    /// endpoint's region, host, provider and headers.
    runtime: BedrockAdapter,
}

impl ConverseAdapter {
    /// Converse on `runtime`'s host and region, signed as its requests are.
    #[must_use]
    pub const fn new(runtime: BedrockAdapter) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl ProviderAdapter for ConverseAdapter {
    fn provider(&self) -> Provider {
        self.runtime.provider()
    }

    /// Said here rather than read from the provider: the runtime this rides
    /// on may be one the provider says speaks Anthropic.
    fn dialect(&self) -> Dialect {
        Dialect::BedrockConverse
    }

    fn framing(&self) -> Framing {
        Framing::AwsConverseStream
    }

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        let creds = BedrockAdapter::credentials(&req.credential.access_token)?;
        // No model and no stream flag in the body: both are in the path.
        let body = oag_proto::converse::render_request(req.canonical)?;
        let action = if req.canonical.stream {
            "converse-stream"
        } else {
            "converse"
        };
        self.runtime.signed_post(
            &creds,
            &req.model.upstream_name,
            action,
            serde_json::to_vec(&body)?,
        )
    }

    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
        // The transport has taken the event out of its binary envelope and
        // named it by its `:event-type` (`eventstream::converse_event`).
        oag_proto::converse::parse_event(raw, acc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sigv4;
    use crate::{HttpTransport, Transport as _};
    use oag_core::credential::SecretMaterial;
    use oag_core::provider::{Endpoint, Platform};
    use oag_proto::CanonicalRequest;
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::dec;
    use serde_json::{Value, json};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Obviously not a key: there are none to test with.
    const KEY: &str = "TESTACCESSKEY:TESTSECRETKEY";

    /// A Bedrock model id as AWS spells one, colon and all.
    const MODEL: &str = "meta.llama3-1-70b-instruct-v1:0";

    fn endpoint(name: &str) -> Endpoint {
        Endpoint::new(name, Dialect::BedrockConverse, Platform::Aws).unwrap()
    }

    fn adapter(name: &str, region: &str, base: Option<String>) -> ConverseAdapter {
        ConverseAdapter::new(
            BedrockAdapter::for_endpoint(endpoint(name), region).with_endpoint(base),
        )
    }

    fn model(endpoint: Endpoint) -> ModelSpec {
        ModelSpec {
            id: ModelId::new(format!("{}/llama", endpoint.name())),
            provider: Provider::Custom(endpoint),
            upstream_name: MODEL.to_owned(),
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
            reasoning_efforts: None,
        }
    }

    /// What a Chat Completions client sends, as the gateway reads it.
    fn canonical(stream: bool) -> CanonicalRequest {
        oag_proto::openai::parse_request(&json!({
            "model": "t9/llama",
            "stream": stream,
            "max_tokens": 300,
            "messages": [
                { "role": "system", "content": "Answer briefly." },
                { "role": "user", "content": "What is on WZPZ?" },
            ],
            "tools": [{ "type": "function", "function": {
                "name": "top_song",
                "description": "The most popular song on a station.",
                "parameters": { "type": "object",
                                "properties": { "sign": { "type": "string" } } },
            }}],
        }))
        .unwrap()
    }

    fn credential(raw: &str) -> SecretMaterial {
        SecretMaterial {
            access_token: raw.to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        }
    }

    fn build(adapter: &ConverseAdapter, canonical: &CanonicalRequest) -> reqwest::Request {
        let (model, credential) = (model(endpoint("t9-conv-build")), credential(KEY));
        adapter
            .build(&UpstreamRequest {
                canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .unwrap()
    }

    /// The region and service a SigV4 `authorization` header is scoped to.
    fn scope(authorization: &str) -> (String, String) {
        let credential = authorization
            .split("Credential=")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_else(|| panic!("no credential in {authorization}"));
        let parts: Vec<&str> = credential.split('/').collect();
        assert_eq!(parts.len(), 5, "{credential}");
        assert_eq!(parts[0], "TESTACCESSKEY", "{credential}");
        assert_eq!(parts[4], "aws4_request", "{credential}");
        (parts[2].to_owned(), parts[3].to_owned())
    }

    /// The signature AWS would compute for what arrived: over the path, host,
    /// date and body the stand-in received, not over what the adapter meant.
    fn verify_signature(received: &wiremock::Request, region: &str) {
        let header = |name: &str| {
            received
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_else(|| panic!("no {name} header"))
                .to_owned()
        };
        let at = time::PrimitiveDateTime::parse(
            &header("x-amz-date"),
            &time::macros::format_description!("[year][month][day]T[hour][minute][second]Z"),
        )
        .unwrap()
        .assume_utc();
        let recomputed = sigv4::sign(
            &BedrockAdapter::credentials(KEY).unwrap(),
            region,
            "bedrock",
            sigv4::SigningRequest {
                method: "POST",
                path: received.url.path(),
                host: &header("host"),
                body: &received.body,
            },
            at,
        );
        assert_eq!(header("authorization"), recomputed.authorization);
    }

    #[tokio::test]
    async fn a_request_is_posted_at_converse_signed_for_the_endpoints_region() {
        let server = MockServer::start().await;
        let at = format!("/model/{MODEL}/converse");
        Mock::given(method("POST"))
            .and(path(at.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = adapter("t9-conv-post", "eu-west-3", Some(server.uri()));
        let canonical = canonical(false);
        let request = build(&adapter, &canonical);
        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let answered = transport.execute(request).await.unwrap();
        assert_eq!(answered.status(), 200, "the request missed {at}");
        server.verify().await;

        let received = server.received_requests().await.unwrap().remove(0);
        let authorization = received.headers["authorization"].to_str().unwrap();
        assert_eq!(
            scope(authorization),
            ("eu-west-3".to_owned(), "bedrock".to_owned()),
            "scoped to the endpoint's region and Bedrock: {authorization}"
        );
        verify_signature(&received, "eu-west-3");
        for key_header in [
            "x-api-key",
            "x-goog-api-key",
            "api-key",
            "x-amz-security-token",
        ] {
            assert!(received.headers.get(key_header).is_none(), "{key_header}");
        }

        // The body is the Converse shape, and says neither the model nor
        // whether to stream: both are in the path.
        let body: Value = serde_json::from_slice(&received.body).unwrap();
        assert_eq!(
            body,
            oag_proto::converse::render_request(&canonical).unwrap()
        );
        assert_eq!(body["system"], json!([{ "text": "Answer briefly." }]));
        assert_eq!(
            body["messages"],
            json!([{ "role": "user", "content": [{ "text": "What is on WZPZ?" }] }])
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 300);
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "top_song"
        );
        assert!(body.get("model").is_none() && body.get("modelId").is_none());
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn a_stream_is_asked_for_at_converse_stream() {
        let adapter = adapter("t9-conv-stream", "us-west-2", None);
        let request = build(&adapter, &canonical(true));
        assert_eq!(
            request.url().as_str(),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/meta.llama3-1-70b-instruct-v1:0/converse-stream"
        );
        let body: Value =
            serde_json::from_slice(request.body().and_then(reqwest::Body::as_bytes).unwrap())
                .unwrap();
        assert!(body.get("stream").is_none(), "{body}");
    }

    #[test]
    fn the_model_id_survives_in_the_path_colon_and_all() {
        // Every Bedrock model id has a colon, which the signer encodes once on
        // top of what is on the wire. The built-in's rule, by the same code.
        let adapter = adapter("t9-conv-colon", "ap-southeast-2", None);
        let request = build(&adapter, &canonical(false));
        assert_eq!(
            request.url().path(),
            "/model/meta.llama3-1-70b-instruct-v1:0/converse"
        );
        assert_eq!(
            request.headers()["host"],
            "bedrock-runtime.ap-southeast-2.amazonaws.com"
        );
    }

    #[test]
    fn an_arn_is_one_path_segment_on_converse_too() {
        // An application inference profile's ARN, whose resource follows a
        // `/`. Converse posts through the Bedrock adapter's `signed_post`, so
        // it is sent as one segment the same way.
        let adapter = adapter("t9-conv-arn", "us-east-1", None);
        let canonical = canonical(false);
        let mut model = model(endpoint("t9-conv-arn"));
        model.upstream_name =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2c3d4e5f6"
                .to_owned();
        let credential = credential(KEY);
        let request = adapter
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .unwrap();
        assert_eq!(
            request.url().path(),
            "/model/arn:aws:bedrock:us-east-1:123456789012:\
             application-inference-profile%2Fa1b2c3d4e5f6/converse"
        );
    }

    #[test]
    fn the_signature_is_computed_over_the_exact_wire_path() {
        let adapter = adapter("t9-conv-wire", "eu-central-1", None);
        let request = build(&adapter, &canonical(false));
        let header = |name: &str| request.headers()[name].to_str().unwrap().to_owned();
        let at = time::PrimitiveDateTime::parse(
            &header("x-amz-date"),
            &time::macros::format_description!("[year][month][day]T[hour][minute][second]Z"),
        )
        .unwrap()
        .assume_utc();
        let recomputed = sigv4::sign(
            &BedrockAdapter::credentials(KEY).unwrap(),
            "eu-central-1",
            "bedrock",
            sigv4::SigningRequest {
                method: "POST",
                path: request.url().path(),
                host: &header("host"),
                body: request.body().and_then(reqwest::Body::as_bytes).unwrap(),
            },
            at,
        );
        assert_eq!(header("authorization"), recomputed.authorization);
    }

    #[test]
    fn temporary_credentials_carry_their_session_token() {
        let adapter = adapter("t9-conv-sts", "us-east-2", None);
        let (canonical, model) = (canonical(false), model(endpoint("t9-conv-sts")));
        let credential = credential("TESTACCESSKEY:TESTSECRETKEY:TESTSESSIONTOKEN");
        let request = adapter
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .unwrap();
        assert_eq!(
            request.headers()["x-amz-security-token"],
            "TESTSESSIONTOKEN"
        );
        let authorization = request.headers()["authorization"].to_str().unwrap();
        assert!(
            authorization.contains("x-amz-security-token"),
            "signed, not merely attached: {authorization}"
        );
    }

    #[test]
    fn a_credential_that_is_not_packed_is_refused_before_anything_is_built() {
        let adapter = adapter("t9-conv-bad-key", "us-east-1", None);
        let (canonical, model) = (canonical(false), model(endpoint("t9-conv-bad-key")));
        let credential = credential("just-a-key");
        let err = adapter
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .expect_err("no secret in it");
        assert!(err.to_string().contains("access_key:secret"), "{err}");
    }

    #[test]
    fn what_converse_cannot_say_is_refused_in_its_own_name() {
        let adapter = adapter("t9-conv-refuse", "us-east-1", None);
        let canonical = oag_proto::openai::parse_request(&json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "response_format": { "type": "json_object" },
        }))
        .unwrap();
        let (model, credential) = (model(endpoint("t9-conv-refuse")), credential(KEY));
        let err = adapter
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .expect_err("Converse has no schemaless JSON mode");
        assert!(
            matches!(
                err,
                oag_core::Error::UnsupportedField {
                    field: "response_format",
                    dialect: Dialect::BedrockConverse,
                }
            ),
            "{err}"
        );
        assert!(
            err.to_string()
                .starts_with("Bedrock Converse cannot express"),
            "{err}"
        );
    }

    #[test]
    fn it_names_its_endpoint_its_dialect_and_its_framing() {
        let adapter = adapter("t9-conv-facts", "us-east-1", None);
        assert_eq!(
            adapter.provider(),
            Provider::Custom(endpoint("t9-conv-facts"))
        );
        // Not the provider's say: a runtime built for the built-in provider
        // would answer Anthropic.
        assert_eq!(adapter.dialect(), Dialect::BedrockConverse);
        assert_eq!(
            ConverseAdapter::new(BedrockAdapter::new("us-east-1")).dialect(),
            Dialect::BedrockConverse
        );
        assert_eq!(adapter.framing(), Framing::AwsConverseStream);
        assert!(!adapter.always_streams());
    }

    #[test]
    fn a_named_event_is_read_as_converse() {
        let adapter = adapter("t9-conv-event", "us-east-1", None);
        let mut acc = StreamAccumulator::new();
        let events = adapter
            .parse_event(
                r#"{"contentBlockDelta":{"delta":{"text":"Starman"},"contentBlockIndex":0}}"#,
                &mut acc,
            )
            .unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::TextDelta {
                text: "Starman".to_owned()
            }]
        );
    }
}
