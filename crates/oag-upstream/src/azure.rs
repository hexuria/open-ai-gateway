//! Azure `OpenAI`: Chat Completions at an Azure resource.
//!
//! The wire is `OpenAI`'s, request and stream alike, so this adapter renders
//! and reads exactly what [`crate::OpenAICompatAdapter`] does, with the same
//! codec. Two things differ, and they are the whole of this module: where a
//! request is posted, and the header its key rides in.
//!
//! **Where.** An endpoint's base URL is its resource's,
//! `https://{resource}.openai.azure.com` or
//! `https://{resource}.services.ai.azure.com` and nothing more
//! (`oag_core::endpoint::endpoint_base_url` holds a row to that). One of two
//! URLs goes on it, chosen by whether the row names an API version:
//!
//! - none: Azure's v1 API, `{base}/openai/v1/chat/completions`, with the model
//!   named in the body as every `OpenAI`-compatible host takes it;
//! - one: the deployments API,
//!   `{base}/openai/deployments/{deployment}/chat/completions?api-version={version}`.
//!
//! On Azure the model a request names is a deployment, in both: the name an
//! operator gave a model when deploying it to the resource. So a catalog row's
//! upstream name is the deployment's name. In the deployments URL it is one
//! path segment, percent-encoded; the body is the one the v1 API is sent,
//! `model` and all, which Azure reads the deployment from the path in place
//! of, and which is what `OpenAI`'s own SDK sends Azure there too.
//!
//! The query is built here, from the stored version: a base URL never carries
//! one, because the loader refuses a `?` in it.
//!
//! **The key** goes in `api-key`, the one header Azure reads an API key from.
//! A Microsoft Entra ID token, which Azure reads as a bearer instead, is not
//! served.

use crate::adapter::{ProviderAdapter, UpstreamRequest};
use crate::custom::{ExtraHeaders, authenticate};
use async_trait::async_trait;
use oag_core::provider::{AuthStyle, Endpoint};
use oag_core::{Error, Provider, Result};
use oag_proto::{StreamAccumulator, StreamEvent, openai};

/// Talks Chat Completions to one `azure` endpoint's resource.
#[derive(Debug, Clone)]
pub struct AzureOpenAIAdapter {
    endpoint: Endpoint,
    /// The resource's URL, with no path and no trailing slash.
    base_url: String,
    /// `None` is the v1 API; a version is the deployments API, at that version.
    api_version: Option<String>,
    /// Added to every request.
    headers: ExtraHeaders,
}

impl AzureOpenAIAdapter {
    /// The adapter for `endpoint`, posting beneath `base_url`: to the v1 API
    /// if `api_version` is `None`, or to the deployments API at that version.
    ///
    /// Both are used as given. A stored row's have passed the loader's rules:
    /// an Azure resource's URL, and an API version that is a date and
    /// `-preview` or not.
    #[must_use]
    pub fn for_endpoint(
        endpoint: Endpoint,
        base_url: impl Into<String>,
        api_version: Option<String>,
        headers: ExtraHeaders,
    ) -> Self {
        Self {
            endpoint,
            base_url: base_url.into(),
            api_version,
            headers,
        }
    }

    /// Where a request for `deployment` is posted, before the query.
    fn url(&self, deployment: &str) -> Result<String> {
        if self.api_version.is_none() {
            return Ok(format!("{}/openai/v1/chat/completions", self.base_url));
        }
        // A URL parser resolves `.` and `..` before a request is sent, so a
        // deployment named either would be posted somewhere else entirely, and
        // an empty one leaves `//`. Azure has no deployment named any of them.
        if matches!(deployment, "" | "." | "..") {
            return Err(Error::Config(format!(
                "endpoint `{}`: a model whose upstream name is {deployment:?} cannot be \
                 sent to Azure, whose deployments API puts the name in the URL as one \
                 path segment; name the deployment in the catalog row",
                self.endpoint.name()
            )));
        }
        Ok(format!(
            "{}/openai/deployments/{}/chat/completions",
            self.base_url,
            path_segment(deployment)
        ))
    }
}

/// `name` as one URL path segment: every byte but an unreserved one (RFC
/// 3986's letters, digits, `-`, `.`, `_` and `~`) percent-encoded, so no
/// character in it can end the segment, begin a query or a fragment, or be
/// read as an escape it was not.
///
/// `sigv4`'s path encoder leaves the same bytes alone but keeps `/`, which
/// separates a path's segments; here it is part of a name and is escaped.
fn path_segment(name: &str) -> String {
    use std::fmt::Write as _;
    let mut segment = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            segment.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail.
            let _ = write!(segment, "%{byte:02X}");
        }
    }
    segment
}

#[async_trait]
impl ProviderAdapter for AzureOpenAIAdapter {
    fn provider(&self) -> Provider {
        Provider::Custom(self.endpoint)
    }

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        let deployment = &req.model.upstream_name;
        let url = self.url(deployment)?;
        let body = openai::render_request(req.canonical, deployment)?;

        let mut builder = crate::builder_client()?
            .post(url)
            .header("content-type", "application/json");
        if let Some(version) = &self.api_version {
            builder = builder.query(&[("api-version", version)]);
        }
        let builder = authenticate(
            builder,
            AuthStyle::ApiKeyHeader,
            &req.credential.access_token,
        )
        .json(&body);
        self.headers
            .apply(builder)
            .build()
            .map_err(|e| Error::Internal(format!("building request: {e}")))
    }

    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
        openai::parse_event(raw, acc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom::{EndpointSpec, adapter};
    use crate::{Framing, HttpTransport, Transport as _};
    use oag_core::credential::SecretMaterial;
    use oag_core::provider::{Dialect, Platform};
    use oag_proto::stream::StopReason;
    use oag_proto::{CanonicalRequest, ContentBlock, Message, Role};
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::dec;
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The key every test sends. Never a real one: there are none to send.
    const KEY: &str = "azure-test-key";

    /// The deployments API version these tests ask for.
    const VERSION: &str = "2024-10-21";

    /// What an operator might add: a header Azure logs a caller's requests by.
    const EXTRA: [(&str, &str); 2] = [("x-ms-useragent", "oag-test"), ("x-tenant", "t8")];

    fn endpoint(name: &str) -> Endpoint {
        Endpoint::new(name, Dialect::OpenAIChatCompletions, Platform::Azure).unwrap()
    }

    fn spec(name: &str, base_url: &str, api_version: Option<&str>) -> EndpointSpec {
        EndpointSpec::new(endpoint(name), base_url, AuthStyle::ApiKeyHeader, EXTRA)
            .unwrap()
            .with_api_version(api_version.map(str::to_owned))
    }

    fn model(endpoint: Endpoint, deployment: &str) -> ModelSpec {
        ModelSpec {
            id: ModelId::new(format!("{}/chat", endpoint.name())),
            provider: Provider::Custom(endpoint),
            upstream_name: deployment.to_owned(),
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
            thinking_effort: None,
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
            passthrough: None,
        }
    }

    fn credential() -> SecretMaterial {
        SecretMaterial {
            access_token: KEY.to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        }
    }

    /// What `spec`'s adapter builds for one request to `deployment`.
    fn build(spec: &EndpointSpec, deployment: &str, stream: bool) -> Result<reqwest::Request> {
        let adapter = adapter(spec).expect("an azure endpoint has an adapter");
        let (canonical, model, credential) = (
            request(stream),
            model(adapter_endpoint(&adapter), deployment),
            credential(),
        );
        adapter.build(&UpstreamRequest {
            canonical: &canonical,
            model: &model,
            credential: &credential,
            session: None,
        })
    }

    fn adapter_endpoint(adapter: &Arc<dyn ProviderAdapter>) -> Endpoint {
        match adapter.provider() {
            Provider::Custom(endpoint) => endpoint,
            other => panic!("an endpoint's adapter serves {other}"),
        }
    }

    /// One request through the real transport to a mock that answers only
    /// `at`: what arrived there.
    async fn send(
        server: &MockServer,
        spec: &EndpointSpec,
        deployment: &str,
        at: &str,
    ) -> wiremock::Request {
        Mock::given(method("POST"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(server)
            .await;
        let built = build(spec, deployment, false).expect("builds");
        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let response = transport.execute(built).await.expect("the mock answers");
        assert_eq!(response.status(), 200, "the request missed {at}");
        server.verify().await;
        let mut received = server.received_requests().await.expect("recording is on");
        assert_eq!(received.len(), 1);
        received.remove(0)
    }

    /// The key in `api-key` alone, the operator's headers, and JSON.
    fn assert_headers(received: &wiremock::Request) {
        let headers = &received.headers;
        let values = |name: &str| headers.get_all(name).iter().collect::<Vec<_>>();
        assert_eq!(
            values("api-key"),
            [KEY],
            "the key, once, where Azure reads it"
        );
        for elsewhere in ["authorization", "x-api-key", "x-goog-api-key"] {
            assert_eq!(values(elsewhere), Vec::<&str>::new(), "{elsewhere}");
        }
        for (extra, value) in EXTRA {
            assert_eq!(values(extra), [value], "the operator's {extra}");
        }
        assert_eq!(headers["content-type"], "application/json");
    }

    fn body(received: &wiremock::Request) -> serde_json::Value {
        serde_json::from_slice(&received.body).expect("a JSON body")
    }

    #[tokio::test]
    async fn the_v1_api_is_posted_at_the_resource_with_the_deployment_in_the_body() {
        let server = MockServer::start().await;
        let spec = spec("t8-azure-v1", &server.uri(), None);
        let received = send(
            &server,
            &spec,
            "gpt-4o-mini-prod",
            "/openai/v1/chat/completions",
        )
        .await;
        assert_eq!(received.url.path(), "/openai/v1/chat/completions");
        assert_eq!(received.url.query(), None, "the v1 API takes no version");
        assert_headers(&received);
        let body = body(&received);
        assert_eq!(body["model"], "gpt-4o-mini-prod");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn the_deployments_api_names_the_deployment_in_the_path_and_the_version_in_the_query() {
        let server = MockServer::start().await;
        let spec = spec("t8-azure-deployments", &server.uri(), Some(VERSION));
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt-4o-prod/chat/completions"))
            .and(query_param("api-version", VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let built = build(&spec, "gpt-4o-prod", false).expect("builds");
        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let answered = transport.execute(built).await.expect("the mock answers");
        assert_eq!(answered.status(), 200, "the request missed the deployment");
        server.verify().await;
        let received = server.received_requests().await.expect("recording is on");
        let [received] = received.as_slice() else {
            panic!("{received:?}");
        };

        assert_eq!(
            received.url.query(),
            Some("api-version=2024-10-21"),
            "the version, and nothing else"
        );
        assert_headers(received);
        assert_eq!(
            body(received)["model"],
            "gpt-4o-prod",
            "the body is the one the v1 API is sent"
        );
    }

    /// A preview version is a version like any other.
    #[test]
    fn a_preview_version_is_asked_for_as_it_is_stored() {
        let spec = spec(
            "t8-azure-preview",
            "https://res.openai.azure.com",
            Some("2025-04-01-preview"),
        );
        let built = build(&spec, "gpt-4o", false).expect("builds");
        assert_eq!(
            built.url().as_str(),
            "https://res.openai.azure.com/openai/deployments/gpt-4o/chat/completions\
             ?api-version=2025-04-01-preview"
        );
    }

    /// Whatever a deployment's name holds, it is one path segment, and the
    /// query is still the version alone.
    #[tokio::test]
    async fn a_deployment_name_is_one_path_segment_whatever_it_holds() {
        for (deployment, segment) in [
            ("gpt 4o", "gpt%204o"),
            ("team/gpt-4o", "team%2Fgpt-4o"),
            (
                "x?api-version=1999-01-01#y",
                "x%3Fapi-version%3D1999-01-01%23y",
            ),
            ("50%25", "50%2525"),
            ("ünï", "%C3%BCn%C3%AF"),
            ("a\\b", "a%5Cb"),
            ("a..b", "a..b"),
            ("..a", "..a"),
            ("~_.-", "~_.-"),
        ] {
            let server = MockServer::start().await;
            let spec = spec("t8-azure-encoded", &server.uri(), Some(VERSION));
            let at = format!("/openai/deployments/{segment}/chat/completions");
            let received = send(&server, &spec, deployment, &at).await;
            assert_eq!(received.url.path(), at, "{deployment:?}");
            assert_eq!(
                received.url.query(),
                Some("api-version=2024-10-21"),
                "{deployment:?}"
            );
            assert_eq!(body(&received)["model"], deployment);
        }
    }

    #[test]
    fn a_deployment_name_that_is_no_path_segment_is_refused() {
        let deployments = spec(
            "t8-azure-dots",
            "https://res.openai.azure.com",
            Some(VERSION),
        );
        for deployment in ["", ".", ".."] {
            let err = build(&deployments, deployment, false)
                .expect_err(deployment)
                .to_string();
            assert!(
                err.contains(&format!(
                    "endpoint `t8-azure-dots`: a model whose upstream name is {deployment:?} \
                     cannot be sent to Azure"
                )),
                "{err}"
            );
        }
        // The v1 API names the deployment in the body, where any name is one.
        let v1 = spec("t8-azure-dots-v1", "https://res.openai.azure.com", None);
        let built = build(&v1, "..", false).expect("builds");
        assert_eq!(
            built.url().as_str(),
            "https://res.openai.azure.com/openai/v1/chat/completions"
        );
    }

    /// RFC 3986's unreserved bytes stay as they are, and every other byte is
    /// escaped, so no name can end the segment or begin a query.
    #[test]
    fn a_path_segment_escapes_every_byte_but_an_unreserved_one() {
        for byte in 1u8..=127 {
            let one = char::from(byte).to_string();
            let unreserved = byte.is_ascii_alphanumeric() || b"-._~".contains(&byte);
            let expected = if unreserved {
                one.clone()
            } else {
                format!("%{byte:02X}")
            };
            assert_eq!(path_segment(&one), expected, "byte {byte}");
        }
        assert_eq!(path_segment("é"), "%C3%A9", "each byte of a character");
        assert_eq!(path_segment("Gpt-4o_mini.2~"), "Gpt-4o_mini.2~");
        assert_eq!(path_segment("a/b?c#d"), "a%2Fb%3Fc%23d");
    }

    /// Streamed, the request asks for the stream's usage as every Chat
    /// Completions upstream is asked, and Azure's stream reads as `OpenAI`'s:
    /// the prompt-filter frame with no choices first, content filter results
    /// on each delta, and the usage last.
    #[test]
    fn a_stream_is_asked_for_and_read_as_openai_s() {
        let spec = spec("t8-azure-stream", "https://res.openai.azure.com", None);
        let built = build(&spec, "gpt-4o", true).expect("builds");
        let body: serde_json::Value = serde_json::from_slice(
            built
                .body()
                .and_then(reqwest::Body::as_bytes)
                .expect("body"),
        )
        .expect("json");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);

        let adapter = adapter(&spec).expect("an adapter");
        assert_eq!(adapter.framing(), Framing::Sse);
        let mut acc = StreamAccumulator::default();
        let mut events = Vec::new();
        for frame in AZURE_STREAM {
            let parsed = adapter.parse_event(frame, &mut acc).expect(frame);
            for event in &parsed {
                acc.observe(event);
            }
            events.extend(parsed);
        }
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello from Azure");
        assert_eq!(acc.stop_reason(), Some(StopReason::EndTurn));
        let usage = acc.usage();
        assert_eq!((usage.input_tokens, usage.output_tokens), (9, 4));
    }

    /// A content filter that stops an answer stops it as a refusal: what the
    /// `OpenAI` dialect says `content_filter` means, and what every client
    /// dialect is told.
    #[test]
    fn a_content_filter_stops_an_answer_as_a_refusal() {
        let spec = spec("t8-azure-filtered", "https://res.openai.azure.com", None);
        let adapter = adapter(&spec).expect("an adapter");
        let mut acc = StreamAccumulator::default();
        let filtered = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"content_filter","content_filter_results":{"hate":{"filtered":true,"severity":"high"}}}],"id":"chatcmpl-t8","model":"gpt-4o","object":"chat.completion.chunk"}"#;
        let events = adapter.parse_event(filtered, &mut acc).expect("parses");
        assert!(
            events.iter().any(|e| matches!(
                e,
                StreamEvent::Stop {
                    reason: StopReason::Refusal,
                    ..
                }
            )),
            "{events:?}"
        );
    }

    /// Azure's stream, as it sends one: a first frame with no choices and the
    /// prompt's filter results, deltas each with their own, the finish, then
    /// the usage `stream_options` asked for, then `[DONE]`.
    const AZURE_STREAM: [&str; 6] = [
        r#"{"choices":[],"created":0,"id":"","model":"","object":"","prompt_filter_results":[{"prompt_index":0,"content_filter_results":{"hate":{"filtered":false,"severity":"safe"}}}]}"#,
        r#"{"choices":[{"content_filter_results":{},"delta":{"content":"","refusal":null,"role":"assistant"},"finish_reason":null,"index":0}],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-2024-11-20","object":"chat.completion.chunk","system_fingerprint":"fp_t8"}"#,
        r#"{"choices":[{"content_filter_results":{"hate":{"filtered":false,"severity":"safe"}},"delta":{"content":"Hello from"},"finish_reason":null,"index":0}],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-2024-11-20","object":"chat.completion.chunk"}"#,
        r#"{"choices":[{"content_filter_results":{"hate":{"filtered":false,"severity":"safe"}},"delta":{"content":" Azure"},"finish_reason":null,"index":0}],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-2024-11-20","object":"chat.completion.chunk"}"#,
        r#"{"choices":[{"content_filter_results":{},"delta":{},"finish_reason":"stop","index":0}],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-2024-11-20","object":"chat.completion.chunk"}"#,
        r#"{"choices":[],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-2024-11-20","object":"chat.completion.chunk","usage":{"completion_tokens":4,"prompt_tokens":9,"total_tokens":13}}"#,
    ];

    #[test]
    fn an_azure_endpoint_gets_this_adapter_under_its_own_name() {
        for api_version in [None, Some(VERSION)] {
            let spec = spec("t8-azure-kind", "https://res.openai.azure.com", api_version);
            let adapter = adapter(&spec).expect("the azure platform is served");
            assert_eq!(
                adapter.provider(),
                Provider::Custom(endpoint("t8-azure-kind"))
            );
            assert_eq!(adapter.dialect(), Dialect::OpenAIChatCompletions);
            assert_eq!(adapter.framing(), Framing::Sse);
            assert!(!adapter.always_streams());
        }
    }

    #[test]
    fn an_azure_endpoint_takes_its_key_in_api_key_and_speaks_openai_or_is_refused() {
        // The loader holds a row to both, and the factory does not take its
        // word for either.
        for auth in [
            AuthStyle::Bearer,
            AuthStyle::XApiKey,
            AuthStyle::XGoogApiKey,
            AuthStyle::None,
        ] {
            let spec = EndpointSpec::new(
                endpoint("t8-azure-auth"),
                "https://res.openai.azure.com",
                auth,
                EXTRA,
            )
            .unwrap();
            let err = adapter(&spec).expect_err(auth.as_str()).to_string();
            assert!(
                err.contains(&format!(
                    "endpoint `t8-azure-auth` is on the azure platform, which takes its key \
                     in `api-key` (auth api_key_header), not {}",
                    auth.as_str()
                )),
                "{err}"
            );
        }
        for dialect in [
            Dialect::AnthropicMessages,
            Dialect::GeminiGenerateContent,
            Dialect::SystemOne,
            Dialect::OpenAIResponses,
        ] {
            let spec = EndpointSpec::new(
                Endpoint::new("t8-azure-dialect", dialect, Platform::Azure).unwrap(),
                "https://res.openai.azure.com",
                AuthStyle::ApiKeyHeader,
                EXTRA,
            )
            .unwrap();
            let err = adapter(&spec).expect_err("not Azure's").to_string();
            assert!(
                err.contains(&format!(
                    "endpoint `t8-azure-dialect` speaks {dialect}, which the azure platform \
                     does not serve"
                )),
                "{err}"
            );
        }
    }
}
