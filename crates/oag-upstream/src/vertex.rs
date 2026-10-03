//! Google Vertex AI: Gemini and Claude, for an endpoint on the `gcp` platform.
//!
//! Vertex serves Google's models in the Gemini API's body and Anthropic's in
//! the Messages API's, so this adapter reuses both codecs. Three things differ
//! from either vendor's own API, and each would fail the first request:
//!
//! 1. **The URL names the project, the region, the publisher and the model**:
//!    `{host}/v1/projects/{project}/locations/{region}/publishers/{google|anthropic}/models/{model}:{method}`.
//!    The host is the region's own, `https://{region}-aiplatform.googleapis.com`,
//!    or `https://aiplatform.googleapis.com` for the `global` region, unless
//!    the endpoint names a base URL, which replaces it.
//! 2. **The credential is a token minted from a service account's key**, sent
//!    as `Authorization: Bearer`. What is stored is the key, and
//!    [`ProviderAdapter::prepare_credential`] trades it for a token through the
//!    gateway's one [`GcpTokenCache`], so `build` is handed the token and never
//!    the key.
//! 3. **Claude's body names no model and carries `anthropic_version`**, with
//!    the value Vertex requires, [`VERTEX_ANTHROPIC_VERSION`]. There is no
//!    `anthropic-version` header: Google's documented requests send none.

use crate::adapter::{ProviderAdapter, UpstreamRequest};
use crate::custom::ExtraHeaders;
use crate::gcp_token::GcpTokenCache;
use async_trait::async_trait;
use oag_core::credential::SecretMaterial;
use oag_core::provider::{AuthStyle, Dialect, Endpoint};
use oag_core::{AccountId, Error, Provider, Result};
use oag_proto::{StreamAccumulator, StreamEvent, anthropic, gemini};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::Arc;

/// What Vertex requires in a Claude request's body, where Anthropic's own API
/// takes the `anthropic-version` header.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Whose model a request is for, which decides its path and its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Publisher {
    /// Gemini, in the Gemini API's body: `generateContent`.
    Google,
    /// Claude, in Anthropic's Messages body: `rawPredict`.
    Anthropic,
}

/// Talks to Vertex AI for one endpoint on the `gcp` platform.
#[derive(Debug)]
pub struct VertexAdapter {
    endpoint: Endpoint,
    publisher: Publisher,
    /// Where every request goes, before the path: the region's host, the
    /// global one, or the endpoint's base URL.
    host: String,
    project: String,
    region: String,
    /// The operator's headers, on every request.
    headers: ExtraHeaders,
    /// The gateway's, shared by every `gcp` endpoint and kept across reloads.
    tokens: Arc<GcpTokenCache>,
}

impl VertexAdapter {
    /// The adapter for `endpoint`, in `project` and `region`, minting its
    /// credentials' tokens through `tokens`.
    ///
    /// `base_url`, when there is one, replaces the host (a Private Service
    /// Connect endpoint, a proxy, a stand-in), and the path beneath it still
    /// names the project and the region. `gemini` and `anthropic` are the
    /// dialects served; any other is refused.
    pub fn for_endpoint(
        endpoint: Endpoint,
        base_url: Option<&str>,
        region: &str,
        project: &str,
        headers: ExtraHeaders,
        tokens: Arc<GcpTokenCache>,
    ) -> Result<Self> {
        let publisher = match endpoint.dialect() {
            Dialect::GeminiGenerateContent => Publisher::Google,
            Dialect::AnthropicMessages => Publisher::Anthropic,
            other => {
                return Err(Error::Config(format!(
                    "endpoint `{}` speaks {other}, which the gcp platform does not serve",
                    endpoint.name()
                )));
            }
        };
        Ok(Self {
            endpoint,
            publisher,
            host: host(base_url, region),
            project: project.to_owned(),
            region: region.to_owned(),
            headers,
            tokens,
        })
    }

    /// Where a request for `model` goes, streamed or not.
    fn url(&self, model: &str, stream: bool) -> String {
        let (publisher, method) = match (self.publisher, stream) {
            (Publisher::Google, false) => ("google", "generateContent"),
            (Publisher::Google, true) => ("google", "streamGenerateContent?alt=sse"),
            (Publisher::Anthropic, false) => ("anthropic", "rawPredict"),
            (Publisher::Anthropic, true) => ("anthropic", "streamRawPredict"),
        };
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/{publisher}/models/{}:{method}",
            self.host,
            segment(&self.project),
            segment(&self.region),
            segment(model),
        )
    }

    /// The body: the Gemini API's for Google's models, and Anthropic's for
    /// Claude, less its model and plus Vertex's version marker.
    fn body(&self, req: &UpstreamRequest<'_>) -> Result<serde_json::Value> {
        match self.publisher {
            Publisher::Google => gemini::render_request(req.canonical),
            Publisher::Anthropic => {
                let model = req.model.upstream_name.as_str();
                // Vertex pins a Claude model's version after an `@`
                // (`claude-sonnet-4-5@20250929`). The codec reads the model's
                // generation from its name to choose the form of thinking it
                // takes, and with the pin attached it reads no minor version:
                // 4.7 as 4.0, which takes the form 4.7 refuses. The name has
                // no other use in the body, where it is dropped: it is in the
                // path.
                let name = model.split_once('@').map_or(model, |(name, _)| name);
                let mut body = anthropic::render_request(req.canonical, name)?;
                if let Some(object) = body.as_object_mut() {
                    object.remove("model");
                    object.insert(
                        "anthropic_version".to_owned(),
                        VERTEX_ANTHROPIC_VERSION.into(),
                    );
                }
                Ok(body)
            }
        }
    }
}

/// The host a request goes to: `base_url` when the endpoint names one, and
/// otherwise the region's own, which for `global` has no region in it.
fn host(base_url: Option<&str>, region: &str) -> String {
    match base_url {
        Some(base_url) => base_url.to_owned(),
        None if region == "global" => "https://aiplatform.googleapis.com".to_owned(),
        None => format!("https://{region}-aiplatform.googleapis.com"),
    }
}

/// `value` as one path segment: every byte but an unreserved one (RFC 3986
/// §2.3) percent-encoded, which is how Google's HTTP transcoding encodes a
/// path variable and what its servers decode.
///
/// A Claude model id's `@` (`claude-sonnet-4-5@20250929`) is sent as `%40`,
/// and nothing a catalog row names as a model can end its segment, begin a
/// query or a fragment, or climb out of the path: `/`, `?`, `#` and `%` are
/// encoded with the rest.
fn segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail.
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[async_trait]
impl ProviderAdapter for VertexAdapter {
    fn provider(&self) -> Provider {
        Provider::Custom(self.endpoint)
    }

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        let token = &req.credential.access_token;
        // The stored credential is the service account's JSON key, and a
        // request built without `prepare_credential` would carry it whole,
        // private key and all, to the host. A Google access token is never
        // JSON, so this refuses that one mistake and nothing else.
        if token.trim_start().starts_with('{') {
            return Err(Error::Internal(format!(
                "endpoint `{}`: a request was built with a service account's key \
                 instead of a token minted from it, and was not sent",
                self.endpoint.name()
            )));
        }
        let body = self.body(req)?;
        let url = self.url(&req.model.upstream_name, req.canonical.stream);
        let builder = crate::builder_client()?
            .post(&url)
            .header("content-type", "application/json");
        let builder = crate::custom::authenticate(builder, AuthStyle::Bearer, token).json(&body);
        self.headers
            .apply(builder)
            .build()
            .map_err(|e| Error::Internal(format!("building vertex request: {e}")))
    }

    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
        match self.publisher {
            Publisher::Google => gemini::parse_event(raw, acc),
            Publisher::Anthropic => anthropic::parse_event(raw, acc),
        }
    }

    /// A token Vertex refused (revoked, or its service account's key
    /// disabled under it) is not handed out again: it goes from the cache, if
    /// it is still the one there, and the next request mints a new one.
    async fn credential_refused(&self, account: AccountId, refused: &SecretMaterial) {
        self.tokens.forget(account, &refused.access_token).await;
    }

    /// The token minted from the stored key: the cached one while it has more
    /// than five minutes left, a new one otherwise. A new copy of the
    /// credential, holding the token and nothing of the key.
    async fn prepare_credential<'a>(
        &'a self,
        account: AccountId,
        stored: &'a SecretMaterial,
        proxy: Option<&str>,
    ) -> Result<Cow<'a, SecretMaterial>> {
        let access_token = self
            .tokens
            .token(account, &stored.access_token, proxy)
            .await?;
        Ok(Cow::Owned(SecretMaterial {
            access_token,
            refresh_token: None,
            expires_at: None,
            version: stored.version,
            client_id: None,
            account_id: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp_token::TEST_KEY_PEM;
    use crate::{HttpTransport, Transport as _};
    use oag_core::provider::Platform;
    use oag_proto::canonical::Effort;
    use oag_proto::{CanonicalRequest, ContentBlock, Message, Role};
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::dec;
    use serde_json::{Value, json};
    use std::time::Duration;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const EMAIL: &str = "vertex-caller@oag-test.invalid";

    /// A service-account key around the committed test key, as Google issues
    /// one, `token_uri` and all.
    fn sa_json() -> String {
        json!({
            "type": "service_account",
            "project_id": "oag-test",
            "private_key_id": "0123456789abcdef0123456789abcdef01234567",
            "private_key": TEST_KEY_PEM,
            "client_email": EMAIL,
            "token_uri": "https://oauth2.googleapis.com/token",
        })
        .to_string()
    }

    fn stored() -> SecretMaterial {
        SecretMaterial {
            access_token: sa_json(),
            refresh_token: None,
            expires_at: None,
            version: 7,
            client_id: None,
            account_id: None,
        }
    }

    fn minted(token: &str) -> SecretMaterial {
        SecretMaterial {
            access_token: token.to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 7,
            client_id: None,
            account_id: None,
        }
    }

    fn tokens(token_url: &str) -> Arc<GcpTokenCache> {
        Arc::new(GcpTokenCache::new(token_url).expect("a token URL"))
    }

    fn adapter(dialect: Dialect, region: &str, base_url: Option<&str>) -> VertexAdapter {
        let endpoint = Endpoint::new("t10-vertex", dialect, Platform::Gcp).expect("a name");
        VertexAdapter::for_endpoint(
            endpoint,
            base_url,
            region,
            "oag-test",
            ExtraHeaders::default(),
            tokens("http://127.0.0.1:1/token"),
        )
        .expect("a dialect Vertex serves")
    }

    fn model(upstream: &str) -> ModelSpec {
        ModelSpec {
            id: ModelId::new(format!("t10-vertex/{upstream}")),
            provider: Provider::Custom(
                Endpoint::new("t10-vertex", Dialect::AnthropicMessages, Platform::Gcp)
                    .expect("a name"),
            ),
            upstream_name: upstream.to_owned(),
            pricing: Pricing {
                input_per_mtok: dec!(3),
                output_per_mtok: dec!(15),
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 200_000,
            max_output_tokens: 64_000,
            capabilities: Capabilities::default(),
            display_label: None,
            reasoning_efforts: None,
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

    fn build(
        adapter: &VertexAdapter,
        canonical: &CanonicalRequest,
        upstream: &str,
        credential: &SecretMaterial,
    ) -> Result<reqwest::Request> {
        adapter.build(&UpstreamRequest {
            canonical,
            model: &model(upstream),
            credential,
            session: None,
        })
    }

    fn body_of(built: &reqwest::Request) -> Value {
        serde_json::from_slice(
            built
                .body()
                .and_then(reqwest::Body::as_bytes)
                .expect("a body"),
        )
        .expect("JSON")
    }

    /// Each publisher and mode at its own method, on the region's host, and
    /// on the one host with no region in it for `global`.
    #[test]
    fn each_publisher_and_mode_is_sent_to_its_own_method() {
        let regional = "https://us-east5-aiplatform.googleapis.com/v1/projects/oag-test/\
                        locations/us-east5/publishers";
        let global = "https://aiplatform.googleapis.com/v1/projects/oag-test/\
                      locations/global/publishers";
        let cases = [
            (
                Dialect::GeminiGenerateContent,
                false,
                "google/models/m:generateContent",
            ),
            (
                Dialect::GeminiGenerateContent,
                true,
                "google/models/m:streamGenerateContent?alt=sse",
            ),
            (
                Dialect::AnthropicMessages,
                false,
                "anthropic/models/m:rawPredict",
            ),
            (
                Dialect::AnthropicMessages,
                true,
                "anthropic/models/m:streamRawPredict",
            ),
        ];
        for (dialect, stream, tail) in cases {
            for (region, prefix) in [("us-east5", regional), ("global", global)] {
                let built = build(
                    &adapter(dialect, region, None),
                    &request(stream),
                    "m",
                    &minted("ya29.t"),
                )
                .expect("builds");
                assert_eq!(built.url().as_str(), format!("{prefix}/{tail}"));
            }
        }
    }

    /// A base URL replaces the host and nothing beneath it.
    #[test]
    fn a_base_url_replaces_the_host_and_keeps_the_path() {
        let built = build(
            &adapter(
                Dialect::GeminiGenerateContent,
                "europe-west4",
                Some("http://127.0.0.1:9/psc"),
            ),
            &request(false),
            "gemini-2.5-pro",
            &minted("ya29.t"),
        )
        .expect("builds");
        assert_eq!(
            built.url().as_str(),
            "http://127.0.0.1:9/psc/v1/projects/oag-test/locations/europe-west4/\
             publishers/google/models/gemini-2.5-pro:generateContent"
        );
    }

    /// A model id is one path segment whatever it holds: Vertex's `@` is
    /// encoded, and so is anything that could end the segment or the path.
    #[test]
    fn a_model_id_is_one_path_segment() {
        let adapter = adapter(Dialect::AnthropicMessages, "us-east5", None);
        let models = "https://us-east5-aiplatform.googleapis.com/v1/projects/oag-test/\
                      locations/us-east5/publishers/anthropic/models";
        for (upstream, sent) in [
            ("claude-sonnet-4-5@20250929", "claude-sonnet-4-5%4020250929"),
            ("a/../b?c#d e%", "a%2F..%2Fb%3Fc%23d%20e%25"),
            ("claude-3.5_x~y", "claude-3.5_x~y"),
        ] {
            let built =
                build(&adapter, &request(false), upstream, &minted("ya29.t")).expect("builds");
            assert_eq!(
                built.url().as_str(),
                format!("{models}/{sent}:rawPredict"),
                "{upstream}"
            );
            assert_eq!(built.url().query(), None, "{upstream}");
            assert_eq!(built.url().fragment(), None, "{upstream}");
        }
    }

    /// Claude's body is Anthropic's without its model and with Vertex's
    /// version marker; `stream` stays, as Google's own requests send it.
    #[test]
    fn claudes_body_names_vertexs_version_and_not_its_model() {
        for stream in [false, true] {
            let built = build(
                &adapter(Dialect::AnthropicMessages, "us-east5", None),
                &request(stream),
                "claude-sonnet-4-5@20250929",
                &minted("ya29.t"),
            )
            .expect("builds");
            let body = body_of(&built);
            assert!(body.get("model").is_none(), "{body}");
            assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
            assert_eq!(body["stream"], stream);
            assert_eq!(body["max_tokens"], 256);
            assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
            assert!(
                built.headers().get("anthropic-version").is_none(),
                "the body carries the version"
            );
            assert_eq!(built.headers()["content-type"], "application/json");
        }
    }

    /// The version pin after `@` does not hide a model's generation from the
    /// codec: 4.7 is sent the adaptive thinking it takes, and 4.5 a budget.
    #[test]
    fn a_pinned_claude_version_keeps_its_generation() {
        let mut thinking = request(false);
        thinking.max_tokens = 32_000;
        thinking.thinking_effort = Some(Effort::High);
        let adapter = adapter(Dialect::AnthropicMessages, "global", None);
        for (upstream, form) in [
            ("claude-opus-4-7@20260101", "adaptive"),
            ("claude-sonnet-4-5@20250929", "enabled"),
        ] {
            let body =
                body_of(&build(&adapter, &thinking, upstream, &minted("ya29.t")).expect("builds"));
            assert_eq!(body["thinking"]["type"], form, "{upstream}: {body}");
        }
    }

    /// Gemini's body is the Gemini API's, as the codec renders it: the model
    /// and the mode are in the path.
    #[test]
    fn geminis_body_is_the_gemini_apis() {
        let built = build(
            &adapter(Dialect::GeminiGenerateContent, "us-central1", None),
            &request(true),
            "gemini-2.5-flash",
            &minted("ya29.t"),
        )
        .expect("builds");
        let body = body_of(&built);
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 256);
        assert!(body.get("model").is_none() && body.get("stream").is_none());
        assert!(body.get("anthropic_version").is_none());
    }

    /// The minted token rides as a bearer, and no other header carries a key.
    /// (The operator's headers are the factory's to hand over; `custom`'s
    /// tests send them.)
    #[test]
    fn the_token_rides_as_a_bearer_and_no_other_header_carries_a_key() {
        for dialect in [Dialect::GeminiGenerateContent, Dialect::AnthropicMessages] {
            let built = build(
                &adapter(dialect, "us-east5", None),
                &request(false),
                "m",
                &minted("ya29.minted"),
            )
            .expect("builds");
            let headers = built.headers();
            let bearers: Vec<_> = headers.get_all("authorization").iter().collect();
            assert_eq!(bearers, ["Bearer ya29.minted"], "{dialect}");
            for key in ["x-goog-api-key", "x-api-key", "api-key"] {
                assert!(headers.get(key).is_none(), "{dialect}: {key}");
            }
        }
    }

    /// A request built with the stored key instead of a minted token is
    /// refused, and the refusal quotes none of the key.
    #[test]
    fn a_service_account_key_is_never_sent_as_a_token() {
        for dialect in [Dialect::GeminiGenerateContent, Dialect::AnthropicMessages] {
            let err = build(
                &adapter(dialect, "global", None),
                &request(false),
                "m",
                &stored(),
            )
            .expect_err("an unminted key");
            assert!(matches!(err, Error::Internal(_)), "{err:?}");
            let message = err.to_string();
            assert!(message.contains("t10-vertex"), "{message}");
            for line in TEST_KEY_PEM.lines() {
                assert!(!message.contains(line), "{message}");
            }
            assert!(!message.contains(EMAIL), "{message}");
        }
    }

    #[test]
    fn a_dialect_vertex_does_not_serve_here_is_refused() {
        for dialect in [
            Dialect::OpenAIChatCompletions,
            Dialect::SystemOne,
            Dialect::BedrockConverse,
        ] {
            let endpoint = Endpoint::new("t10-vertex-x", dialect, Platform::Gcp).expect("a name");
            let err = VertexAdapter::for_endpoint(
                endpoint,
                None,
                "global",
                "p",
                ExtraHeaders::default(),
                tokens("http://127.0.0.1:1/token"),
            )
            .expect_err("not Vertex's");
            assert!(
                err.to_string().contains(&format!(
                    "endpoint `t10-vertex-x` speaks {dialect}, which the gcp platform does not serve"
                )),
                "{err}"
            );
        }
    }

    /// Each line of a stream is read by its own publisher's codec.
    #[test]
    fn each_publisher_reads_its_own_stream() {
        // An event's payload, as the transport hands it over once the frame
        // is split off.
        let gemini_line =
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"from gemini"}]}}]}"#;
        let claude_line = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"from claude"}}"#;
        for (dialect, line, text) in [
            (Dialect::GeminiGenerateContent, gemini_line, "from gemini"),
            (Dialect::AnthropicMessages, claude_line, "from claude"),
        ] {
            let mut acc = StreamAccumulator::default();
            let events = adapter(dialect, "global", None)
                .parse_event(line, &mut acc)
                .expect("parses");
            assert!(
                format!("{events:?}").contains(text),
                "{dialect}: {events:?}"
            );
        }
    }

    /// One request after another, through the real transport, to a stand-in
    /// token endpoint and a stand-in Vertex: one mint, whose token reaches
    /// the model's exact path as a bearer both times, and the key never
    /// reaches either host's model call. A second adapter for the same
    /// endpoint, as a reload builds, shares the cache and does not mint again.
    #[tokio::test]
    async fn a_request_carries_the_token_minted_once_from_the_stored_key() {
        let (google, vertex) = (MockServer::start().await, MockServer::start().await);
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "ya29.vertex-minted",
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .expect(1)
            .mount(&google)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/projects/oag-test/locations/us-central1/publishers/google/models/\
                 gemini-2.5-flash:streamGenerateContent",
            ))
            .and(query_param("alt", "sse"))
            .and(header("authorization", "Bearer ya29.vertex-minted"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
                "text/event-stream",
            ))
            .expect(3)
            .mount(&vertex)
            .await;

        let cache = tokens(&format!("{}/token", google.uri()));
        let endpoint = Endpoint::new("t10-vertex", Dialect::GeminiGenerateContent, Platform::Gcp)
            .expect("a name");
        let for_each_reload = || {
            VertexAdapter::for_endpoint(
                endpoint,
                Some(&vertex.uri()),
                "us-central1",
                "oag-test",
                ExtraHeaders::default(),
                Arc::clone(&cache),
            )
            .expect("a gcp adapter")
        };
        let account = AccountId::new();
        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let canonical = request(true);
        for adapter in [for_each_reload(), for_each_reload(), for_each_reload()] {
            let stored = stored();
            let prepared = adapter
                .prepare_credential(account, &stored, None)
                .await
                .expect("minted");
            assert_eq!(prepared.access_token, "ya29.vertex-minted");
            assert_eq!(prepared.refresh_token, None);
            assert_eq!(prepared.version, 7);
            let built = build(&adapter, &canonical, "gemini-2.5-flash", &prepared).expect("builds");
            let answer = transport.execute(built).await.expect("answers");
            assert_eq!(answer.status(), 200);
        }

        google.verify().await;
        vertex.verify().await;
        for sent in vertex.received_requests().await.expect("recording is on") {
            let seen = format!("{:?}{}", sent.headers, String::from_utf8_lossy(&sent.body));
            assert!(!seen.contains(EMAIL), "the key reached the model call");
            for line in TEST_KEY_PEM.lines().filter(|l| !l.starts_with("-----")) {
                assert!(!seen.contains(line), "the key reached the model call");
            }
        }
    }

    /// A token Vertex refused is dropped from the cache by the adapter that
    /// sent it, so the next `prepare_credential` mints again. A refusal of a
    /// token the cache no longer holds changes nothing.
    #[tokio::test]
    async fn a_refused_token_is_minted_again_by_the_next_prepare() {
        let google = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "ya29.vertex-minted",
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .expect(2)
            .mount(&google)
            .await;
        let endpoint = Endpoint::new("t10-vertex", Dialect::GeminiGenerateContent, Platform::Gcp)
            .expect("a name");
        let adapter = VertexAdapter::for_endpoint(
            endpoint,
            None,
            "us-central1",
            "oag-test",
            ExtraHeaders::default(),
            tokens(&format!("{}/token", google.uri())),
        )
        .expect("a gcp adapter");
        let (account, stored) = (AccountId::new(), stored());

        let first = adapter
            .prepare_credential(account, &stored, None)
            .await
            .expect("minted");
        let first = first.into_owned();
        // A token the cache does not hold: ignored, the cached one stays.
        adapter
            .credential_refused(account, &minted("ya29.someone-else"))
            .await;
        adapter
            .prepare_credential(account, &stored, None)
            .await
            .expect("still cached");
        assert_eq!(
            google.received_requests().await.expect("recording").len(),
            1
        );
        // The token it holds: dropped, so the next prepare mints again.
        adapter.credential_refused(account, &first).await;
        adapter
            .prepare_credential(account, &stored, None)
            .await
            .expect("minted again");
        google.verify().await;
    }

    /// The credential's proxy carries its mint. The token URL is a closed
    /// port, so a token at all came through the proxy.
    #[tokio::test]
    async fn a_credentials_proxy_carries_its_mint() {
        const CLOSED: &str = "http://127.0.0.1:1/token";
        let proxy = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "ya29.proxied",
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .expect(1)
            .mount(&proxy)
            .await;
        let endpoint = Endpoint::new("t10-vertex", Dialect::GeminiGenerateContent, Platform::Gcp)
            .expect("a name");
        let adapter = VertexAdapter::for_endpoint(
            endpoint,
            None,
            "global",
            "oag-test",
            ExtraHeaders::default(),
            tokens(CLOSED),
        )
        .expect("a gcp adapter");
        let stored = stored();
        let prepared = adapter
            .prepare_credential(AccountId::new(), &stored, Some(&proxy.uri()))
            .await
            .expect("minted through the proxy");
        assert_eq!(prepared.access_token, "ya29.proxied");
        let received = proxy.received_requests().await.expect("recording is on");
        assert_eq!(received[0].url.as_str(), CLOSED);
        proxy.verify().await;
    }

    /// A key that cannot mint is the credential's failure, and says nothing
    /// of the key.
    #[tokio::test]
    async fn a_key_that_cannot_mint_fails_its_credential_and_quotes_nothing() {
        let google = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": "invalid_grant", "error_description": "no"})),
            )
            .expect(1)
            .mount(&google)
            .await;
        let endpoint =
            Endpoint::new("t10-vertex", Dialect::AnthropicMessages, Platform::Gcp).expect("a name");
        let adapter = VertexAdapter::for_endpoint(
            endpoint,
            None,
            "global",
            "oag-test",
            ExtraHeaders::default(),
            tokens(&format!("{}/token", google.uri())),
        )
        .expect("a gcp adapter");
        let stored = stored();
        let err = adapter
            .prepare_credential(AccountId::new(), &stored, None)
            .await
            .expect_err("refused");
        let message = err.to_string();
        assert!(message.contains("invalid_grant"), "{message}");
        for line in TEST_KEY_PEM.lines().filter(|l| !l.starts_with("-----")) {
            assert!(!message.contains(line), "{message}");
        }
        google.verify().await;
    }
}
