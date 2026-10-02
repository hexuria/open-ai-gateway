//! Endpoints: the upstreams an operator registers, as adapters.
//!
//! An endpoint needs no adapter of its own. It speaks a dialect one already
//! serves, so [`adapter`] hands that adapter the endpoint's name, base URL, the
//! header its key goes in, and whatever headers the operator added. The key is
//! not in an [`EndpointSpec`]: it stays in the endpoint's sealed `account` rows,
//! as every provider's does, and reaches the adapter per request.

use crate::adapter::ProviderAdapter;
use crate::listing::ModelSource;
use crate::{AnthropicAdapter, GeminiAdapter, OpenAICompatAdapter};
use oag_core::provider::{AuthStyle, Dialect, Endpoint, Platform};
use oag_core::{Error, Provider, Result};
use reqwest::RequestBuilder;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::sync::Arc;

/// Everything an endpoint's adapter is built from: the endpoint, where it
/// answers, how it takes its key, and the headers an operator added to every
/// request it is sent.
///
/// A plain value with no store behind it: whatever keeps endpoints turns its
/// rows into these. The fields are private so that [`EndpointSpec::new`] is the
/// only way to make one, and every spec's headers have been checked.
#[derive(Debug, Clone)]
pub struct EndpointSpec {
    endpoint: Endpoint,
    base_url: String,
    auth: AuthStyle,
    extra_headers: ExtraHeaders,
    /// Whether its adapter answers `served_models` by reading the endpoint's
    /// model list: the row's `discover_models`.
    discover: bool,
}

impl EndpointSpec {
    /// A spec, if every extra header is one an endpoint may add: a valid
    /// header name and value, and not a name [`ExtraHeaders`] refuses.
    ///
    /// Extra headers never carry a secret. They are stored in the clear and
    /// shown to anyone who can read the endpoint; a key belongs in the
    /// endpoint's `account` row, sealed.
    ///
    /// `base_url` is used as given. Each adapter appends its path to it, so it
    /// wants what a built-in's configured URL gets from the gateway before an
    /// adapter sees it: no trailing slash, no query and no fragment.
    pub fn new<K, V>(
        endpoint: Endpoint,
        base_url: impl Into<String>,
        auth: AuthStyle,
        extra_headers: impl IntoIterator<Item = (K, V)>,
    ) -> std::result::Result<Self, String>
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let extra_headers = ExtraHeaders::parse(extra_headers)
            .map_err(|e| format!("endpoint `{}`: {e}", endpoint.name()))?;
        Ok(Self {
            endpoint,
            base_url: base_url.into(),
            auth,
            extra_headers,
            discover: false,
        })
    }

    /// This spec, with its adapter asked for the models each key serves when
    /// `discover` is set: see [`crate::listing::served`]. Off by default, so an
    /// endpoint whose operator did not ask is never sent a request the gateway
    /// did not need.
    #[must_use]
    pub fn with_discovery(mut self, discover: bool) -> Self {
        self.discover = discover;
        self
    }

    /// Where this endpoint's model list is read from, and how: its dialect,
    /// base URL, auth style and extra headers.
    #[must_use]
    pub fn model_source(&self) -> ModelSource<'_> {
        ModelSource {
            dialect: self.endpoint.dialect(),
            base_url: &self.base_url,
            auth: self.auth,
            headers: &self.extra_headers,
        }
    }
}

/// Headers an operator adds to every request one endpoint is sent: what a
/// host wants beyond its dialect, such as an `HTTP-Referer`, a tenant id or a
/// beta flag. Never a secret; see [`EndpointSpec::new`].
///
/// Refused by name, whatever the case:
///
/// - `authorization`, `x-api-key`, `x-goog-api-key`, `api-key` and `cookie`,
///   the headers a key or a session rides in. That leaves a key in only the
///   header its [`AuthStyle`] names, and keeps a credential out of a setting
///   stored in the clear.
/// - `host`, `content-length` and every `proxy-*`: where the request goes,
///   where its body ends and what a proxy is told. The transport sets those,
///   and one set here would make a different request from the one built.
///
/// Any other header the adapter sets itself (`content-type`,
/// `anthropic-version`) is replaced by an extra one of the same name, so an
/// operator can pin what a host insists on.
///
/// Made only by [`EndpointSpec::new`], or empty by default, so an adapter is
/// never handed a header that did not pass.
#[derive(Debug, Clone, Default)]
pub struct ExtraHeaders(HeaderMap);

impl ExtraHeaders {
    /// Names refused outright. Lowercase, which is how a parsed [`HeaderName`]
    /// spells every name; `proxy-*` is checked beside them.
    const REFUSED: [&'static str; 7] = [
        "authorization",
        "x-api-key",
        "x-goog-api-key",
        "api-key",
        "host",
        "cookie",
        "content-length",
    ];

    pub(crate) fn parse<K, V>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> std::result::Result<Self, String>
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            let raw = name.as_ref();
            let name = HeaderName::from_bytes(raw.as_bytes())
                .map_err(|_| format!("extra header {raw:?} is not a valid header name"))?;
            if Self::refused(&name) {
                return Err(format!(
                    "extra header `{name}` is not allowed: a key or a session goes in the \
                     endpoint's account, never in a header, and host, content-length and \
                     proxy-* are the transport's to set"
                ));
            }
            // The value is left out of the message: it is refused for holding a
            // character a header cannot, and it is not ours to echo.
            let value = HeaderValue::from_str(value.as_ref())
                .map_err(|_| format!("extra header `{name}` has a value no header can carry"))?;
            headers.append(name, value);
        }
        Ok(Self(headers))
    }

    fn refused(name: &HeaderName) -> bool {
        let name = name.as_str();
        Self::REFUSED.contains(&name) || name.starts_with("proxy-")
    }

    /// `builder` with these headers on it, each replacing any header of its
    /// name the adapter set.
    pub(crate) fn apply(&self, builder: RequestBuilder) -> RequestBuilder {
        builder.headers(self.0.clone())
    }
}

/// `builder` carrying `secret` the way `auth` says: in exactly one header, or
/// in none for [`AuthStyle::None`].
///
/// The Chat Completions, Messages and `generateContent` adapters all attach
/// their key with this: a built-in with the one style its provider takes, an
/// endpoint with the one it was registered with. So one function decides which
/// header a key goes in.
pub(crate) fn authenticate(
    builder: RequestBuilder,
    auth: AuthStyle,
    secret: &str,
) -> RequestBuilder {
    match auth {
        AuthStyle::Bearer => builder.header("authorization", format!("Bearer {secret}")),
        AuthStyle::XApiKey => builder.header("x-api-key", secret),
        AuthStyle::XGoogApiKey => builder.header("x-goog-api-key", secret),
        AuthStyle::ApiKeyHeader => builder.header("api-key", secret),
        AuthStyle::None => builder,
    }
}

/// The adapter that serves `spec`'s endpoint: the one for its dialect, built
/// for its name, base URL, auth style and headers.
///
/// The adapter's provider is `Provider::Custom`, never a built-in, so nothing
/// that keys on a built-in applies to it: no OAuth refresh, no xAI model
/// discovery, and not the rule that Anthropic's key goes in `x-api-key`. It
/// discovers models only when the spec says to, from the endpoint's own list.
///
/// Only the plain platform is served so far. An endpoint on AWS, GCP or Azure
/// is refused rather than sent a request built for a plain host, which its
/// platform would reject for want of a signature, a minted token or a
/// deployment path.
pub fn adapter(spec: &EndpointSpec) -> Result<Arc<dyn ProviderAdapter>> {
    let endpoint = spec.endpoint;
    let name = endpoint.name();
    match endpoint.platform() {
        Platform::Plain => {}
        platform @ (Platform::Aws | Platform::Gcp | Platform::Azure) => {
            return Err(Error::Config(format!(
                "endpoint `{name}` is on the {} platform, which is not supported yet; \
                 only plain endpoints are served",
                platform.as_str()
            )));
        }
    }

    let base = spec.base_url.clone();
    let (auth, headers, discover) = (spec.auth, spec.extra_headers.clone(), spec.discover);
    let adapter: Arc<dyn ProviderAdapter> = match endpoint.dialect() {
        Dialect::OpenAIChatCompletions => Arc::new(
            OpenAICompatAdapter::new(Provider::Custom(endpoint), base)
                .with_auth(auth)
                .with_headers(headers)
                .with_discovery(discover),
        ),
        Dialect::AnthropicMessages => Arc::new(
            AnthropicAdapter::for_endpoint(endpoint, base, auth, headers).with_discovery(discover),
        ),
        Dialect::GeminiGenerateContent => Arc::new(
            GeminiAdapter::for_endpoint(endpoint, base, auth, headers).with_discovery(discover),
        ),
        // Not a chat upstream, so no `ProviderAdapter` serves it: that trait
        // builds from a canonical conversation, and a System One question set
        // is not one. The System One route calls Jev through a `JevUpstream`
        // instead, which already takes its base URL, so serving an endpoint
        // there means one `JevUpstream` per endpoint. `JevUpstream` sends its
        // key as a bearer token, so an endpoint registered with any other auth
        // style needs it to go through `authenticate` first.
        Dialect::SystemOne => {
            return Err(Error::Config(format!(
                "endpoint `{name}`: System One endpoints are served by the System One \
                 route, not by a chat adapter"
            )));
        }
        other => {
            return Err(Error::Config(format!(
                "endpoint `{name}` speaks {other}, which no adapter serves"
            )));
        }
    };
    Ok(adapter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Framing, HttpTransport, Transport as _, UpstreamRequest};
    use oag_core::credential::SecretMaterial;
    use oag_proto::{CanonicalRequest, ContentBlock, Message, Role};
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::dec;
    use std::time::Duration;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The key every test sends. Never a real one: there are none to send.
    const KEY: &str = "endpoint-test-key";

    /// Every header a key can ride in.
    const AUTH_HEADERS: [&str; 4] = ["authorization", "x-api-key", "x-goog-api-key", "api-key"];

    /// Each auth style, and the one header it must put the key in.
    const STYLES: [(AuthStyle, Option<(&str, &str)>); 5] = [
        (
            AuthStyle::Bearer,
            Some(("authorization", "Bearer endpoint-test-key")),
        ),
        (AuthStyle::XApiKey, Some(("x-api-key", KEY))),
        (AuthStyle::XGoogApiKey, Some(("x-goog-api-key", KEY))),
        (AuthStyle::ApiKeyHeader, Some(("api-key", KEY))),
        (AuthStyle::None, None),
    ];

    /// Each dialect an endpoint is served in, the path its base URL has on the
    /// mock, and the path its adapter posts to beneath that.
    const DIALECTS: [(Dialect, &str, &str); 3] = [
        (
            Dialect::OpenAIChatCompletions,
            "/v1",
            "/v1/chat/completions",
        ),
        (Dialect::AnthropicMessages, "", "/v1/messages"),
        (
            Dialect::GeminiGenerateContent,
            "/v1beta",
            "/v1beta/models/some-model:generateContent",
        ),
    ];

    /// What an operator might add for a host that ranks its callers.
    const EXTRA: [(&str, &str); 2] = [
        ("HTTP-Referer", "https://oag.example"),
        ("X-Title", "open-ai-gateway"),
    ];

    fn endpoint(name: &str, dialect: Dialect, platform: Platform) -> Endpoint {
        Endpoint::new(name, dialect, platform).unwrap()
    }

    fn plain_spec(endpoint: Endpoint, base_url: &str, auth: AuthStyle) -> EndpointSpec {
        EndpointSpec::new(endpoint, base_url, auth, EXTRA).unwrap()
    }

    fn model(endpoint: Endpoint) -> ModelSpec {
        ModelSpec {
            id: ModelId::new(format!("{}/some-model", endpoint.name())),
            provider: Provider::Custom(endpoint),
            upstream_name: "some-model".to_owned(),
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
            // Not streamed, so a Gemini endpoint's path is the plain method.
            stream: false,
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

    /// One request through `spec`'s adapter and the real transport, to a mock
    /// that answers only `expected_path`; what arrived there.
    async fn send(
        spec: &EndpointSpec,
        server: &MockServer,
        expected_path: &str,
    ) -> wiremock::Request {
        Mock::given(method("POST"))
            .and(path(expected_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(server)
            .await;

        let adapter = adapter(spec).expect("a plain endpoint has an adapter");
        let (canonical, model, credential) = (request(), model(spec.endpoint), credential());
        let built = adapter
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .expect("builds");
        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let response = transport.execute(built).await.expect("the mock answers");
        assert_eq!(
            response.status(),
            200,
            "{}: the request missed {expected_path}",
            spec.endpoint.name()
        );

        server.verify().await;
        let mut received = server.received_requests().await.expect("recording is on");
        assert_eq!(received.len(), 1);
        received.remove(0)
    }

    #[tokio::test]
    async fn every_dialect_sends_the_key_in_the_one_header_its_endpoint_names() {
        for (dialect, base_path, expected_path) in DIALECTS {
            for (auth, expected) in STYLES {
                let server = MockServer::start().await;
                let name = format!("t3-{}-{}", base_name(dialect), auth.as_str());
                let spec = plain_spec(
                    endpoint(&name, dialect, Platform::Plain),
                    &format!("{}{base_path}", server.uri()),
                    auth,
                );
                let received = send(&spec, &server, expected_path).await;
                let headers = &received.headers;

                // The named header once, with the key; every other one never.
                for header in AUTH_HEADERS {
                    let values: Vec<_> = headers.get_all(header).iter().collect();
                    let wanted: Vec<&str> = expected
                        .filter(|&(named, _)| named == header)
                        .map(|(_, value)| value)
                        .into_iter()
                        .collect();
                    assert_eq!(values, wanted, "{name}: {header}");
                }
                for (extra, value) in EXTRA {
                    assert_eq!(
                        headers.get(extra).map(|v| v.to_str().unwrap()),
                        Some(value),
                        "{name}: the operator's {extra} is sent"
                    );
                }
                if dialect == Dialect::AnthropicMessages {
                    assert_eq!(
                        headers["anthropic-version"],
                        oag_proto::anthropic::API_VERSION,
                        "{name}: an Anthropic-dialect endpoint still names the API version"
                    );
                }
            }
        }
    }

    /// A short word for each dialect, for an endpoint name.
    fn base_name(dialect: Dialect) -> &'static str {
        match dialect {
            Dialect::OpenAIChatCompletions => "openai",
            Dialect::AnthropicMessages => "anthropic",
            _ => "gemini",
        }
    }

    #[tokio::test]
    async fn an_extra_header_replaces_the_one_the_adapter_would_send() {
        // A host that insists on another API version gets it, and gets one:
        // appended rather than replaced, the request would carry two.
        let server = MockServer::start().await;
        let spec = EndpointSpec::new(
            endpoint(
                "t3-pinned-version",
                Dialect::AnthropicMessages,
                Platform::Plain,
            ),
            server.uri(),
            AuthStyle::XApiKey,
            [("Anthropic-Version", "2024-01-01")],
        )
        .unwrap();
        let received = send(&spec, &server, "/v1/messages").await;
        let versions: Vec<_> = received
            .headers
            .get_all("anthropic-version")
            .iter()
            .collect();
        assert_eq!(versions, ["2024-01-01"]);
    }

    #[test]
    fn an_extra_header_may_not_carry_a_key_or_redirect_the_connection() {
        let refused = [
            "authorization",
            "Authorization",
            "X-API-Key",
            "x-goog-api-key",
            "API-KEY",
            "Host",
            "cookie",
            "Content-Length",
            "proxy-authorization",
            "Proxy-Connection",
            "proxy-anything",
        ];
        for name in refused {
            let endpoint = endpoint(
                "t3-refused",
                Dialect::OpenAIChatCompletions,
                Platform::Plain,
            );
            let err = EndpointSpec::new(endpoint, "http://h", AuthStyle::Bearer, [(name, "v")])
                .expect_err(name);
            assert!(
                err.contains(&format!("`{}`", name.to_ascii_lowercase())),
                "{name}: {err}"
            );
            assert!(err.contains("not allowed"), "{name}: {err}");
            assert!(err.starts_with("endpoint `t3-refused`"), "{name}: {err}");
        }
    }

    #[test]
    fn an_extra_header_must_be_a_header_at_all() {
        let endpoint = endpoint(
            "t3-malformed",
            Dialect::OpenAIChatCompletions,
            Platform::Plain,
        );
        for name in ["", "two words", "x:y", "new\nline", "naïve"] {
            let err = EndpointSpec::new(endpoint, "http://h", AuthStyle::Bearer, [(name, "v")])
                .expect_err(name);
            assert!(err.contains("not a valid header name"), "{name:?}: {err}");
        }
        for value in ["split\r\nx-api-key: stolen", "nul\0", "bell\u{7}"] {
            let err = EndpointSpec::new(endpoint, "http://h", AuthStyle::Bearer, [("x-ok", value)])
                .expect_err(value);
            assert!(err.contains("`x-ok` has a value"), "{value:?}: {err}");
            assert!(!err.contains(value), "the value is not echoed: {err}");
        }
    }

    #[test]
    fn a_header_that_only_resembles_a_refused_one_is_kept() {
        // The refusal is by exact name, and by prefix only for `proxy-`.
        let endpoint = endpoint(
            "t3-lookalike",
            Dialect::OpenAIChatCompletions,
            Platform::Plain,
        );
        let lookalikes = [
            ("x-api-key-id", "1"),
            ("api-keys", "1"),
            ("x-proxy-region", "eu"),
            ("authorization-hint", "none"),
            ("x-host", "a"),
            ("anthropic-beta", "prompt-caching-2024-07-31"),
        ];
        let spec = EndpointSpec::new(endpoint, "http://h", AuthStyle::Bearer, lookalikes).unwrap();
        for (name, value) in lookalikes {
            assert_eq!(spec.extra_headers.0[name], value, "{name}");
        }
    }

    #[test]
    fn a_plain_endpoint_gets_the_adapter_for_its_dialect_under_its_own_name() {
        for (dialect, _, _) in DIALECTS {
            let name = format!("t3-kind-{}", base_name(dialect));
            let endpoint = endpoint(&name, dialect, Platform::Plain);
            let adapter = adapter(&plain_spec(endpoint, "http://h", AuthStyle::Bearer)).unwrap();
            assert_eq!(adapter.provider(), Provider::Custom(endpoint), "{name}");
            assert_eq!(adapter.dialect(), dialect, "{name}");
            assert_eq!(adapter.framing(), Framing::Sse, "{name}");
            assert!(!adapter.always_streams(), "{name}");
        }
    }

    /// With discovery on, each dialect's adapter answers `served_models` from
    /// the endpoint's own list, read with the key where the endpoint takes it,
    /// or with no key at all where it takes none, and with the operator's
    /// headers. With it off, the adapter answers that it cannot be asked, and
    /// sends nothing.
    #[tokio::test]
    async fn an_endpoint_that_discovers_reads_its_own_list_and_one_that_does_not_sends_nothing() {
        for (dialect, base_path, list_path, listed) in [
            (
                Dialect::OpenAIChatCompletions,
                "/v1",
                "/v1/models",
                serde_json::json!({"object": "list", "data": [{"id": "some-model"}]}),
            ),
            (
                Dialect::AnthropicMessages,
                "",
                "/v1/models",
                serde_json::json!({"data": [{"id": "some-model"}], "has_more": false}),
            ),
            (
                Dialect::GeminiGenerateContent,
                "/v1beta",
                "/v1beta/models",
                serde_json::json!({"models": [{"name": "models/some-model"}]}),
            ),
        ] {
            for (auth, carrier) in [
                (AuthStyle::XGoogApiKey, Some("x-goog-api-key")),
                (AuthStyle::None, None),
            ] {
                let server = MockServer::start().await;
                Mock::given(method("GET"))
                    .and(path(list_path))
                    .and(header("x-title", "open-ai-gateway"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(listed.clone()))
                    .expect(1)
                    .mount(&server)
                    .await;
                let name = format!("t6-d-{}-{}", base_name(dialect), auth.as_str());
                let spec = plain_spec(
                    endpoint(&name, dialect, Platform::Plain),
                    &format!("{}{base_path}", server.uri()),
                    auth,
                );

                let off = adapter(&spec.clone()).unwrap();
                assert_eq!(
                    off.served_models(&credential(), None).await.unwrap(),
                    None,
                    "{name}: not asked to discover"
                );
                let on = adapter(&spec.with_discovery(true)).unwrap();
                assert_eq!(
                    on.served_models(&credential(), None).await.unwrap(),
                    Some(vec!["some-model".to_owned()]),
                    "{name}"
                );
                server.verify().await;
                let sent = server.received_requests().await.expect("recording is on");
                for key_header in AUTH_HEADERS {
                    let values: Vec<_> = sent[0].headers.get_all(key_header).iter().collect();
                    let wanted: Vec<&str> = carrier
                        .filter(|&c| c == key_header)
                        .map(|_| KEY)
                        .into_iter()
                        .collect();
                    assert_eq!(values, wanted, "{name}: {key_header}");
                }
            }
        }
    }

    #[test]
    fn an_endpoint_on_a_cloud_platform_is_refused_until_one_serves_it() {
        for (platform, dialect) in [
            (Platform::Aws, Dialect::AnthropicMessages),
            (Platform::Gcp, Dialect::GeminiGenerateContent),
            (Platform::Azure, Dialect::OpenAIChatCompletions),
        ] {
            let name = format!("t3-cloud-{}", platform.as_str());
            let spec = plain_spec(
                endpoint(&name, dialect, platform),
                "http://h",
                AuthStyle::Bearer,
            );
            let err = adapter(&spec).expect_err(&name).to_string();
            assert!(err.contains("not supported yet"), "{err}");
            assert!(
                err.contains(&format!("the {} platform", platform.as_str())),
                "{err}"
            );
            assert!(err.contains(&name), "{err}");
        }
    }

    #[test]
    fn a_system_one_endpoint_is_left_to_the_system_one_route() {
        let spec = plain_spec(
            endpoint("t3-jev", Dialect::SystemOne, Platform::Plain),
            "http://h",
            AuthStyle::Bearer,
        );
        let err = adapter(&spec).expect_err("no chat adapter").to_string();
        assert!(
            err.contains("System One endpoints are served by the System One route"),
            "{err}"
        );
        assert!(err.contains("t3-jev"), "{err}");
    }

    #[test]
    fn a_dialect_no_adapter_speaks_is_refused() {
        let spec = plain_spec(
            endpoint("t3-responses", Dialect::OpenAIResponses, Platform::Plain),
            "http://h",
            AuthStyle::Bearer,
        );
        let err = adapter(&spec).expect_err("no adapter").to_string();
        assert!(err.contains("OpenAI Responses"), "{err}");
        assert!(err.contains("no adapter serves"), "{err}");
    }
}
