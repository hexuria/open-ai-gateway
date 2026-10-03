//! Endpoints: the upstreams an operator registers, as adapters.
//!
//! An endpoint needs no adapter of its own. It speaks a dialect one already
//! serves, so [`adapter`] hands that adapter the endpoint's name, base URL, the
//! header its key goes in, and whatever headers the operator added. The key is
//! not in an [`EndpointSpec`]: it stays in the endpoint's sealed `account` rows,
//! as every provider's does, and reaches the adapter per request. A System One
//! endpoint is not a chat upstream, so it gets the upstream the System One
//! route calls instead, from [`system_one`].
//!
//! An endpoint on the `aws` platform is Bedrock in a region of the operator's
//! choosing, and its adapter is a Bedrock one: Claude through `InvokeModel`,
//! or every other model Bedrock serves through `Converse`.
//!
//! An endpoint on the `azure` platform speaks `OpenAI`'s dialect at an Azure
//! resource, whose URLs and key header are Azure's own, so it gets an adapter
//! of its own: [`crate::azure::AzureOpenAIAdapter`].
//!
//! An endpoint on the `gcp` platform is Vertex AI in a project and region of
//! the operator's choosing: Gemini, or Claude, with a token minted from the
//! service account each of its credentials holds (`crate::vertex`).

use crate::adapter::ProviderAdapter;
use crate::gcp_token::GcpTokenCache;
use crate::listing::ModelSource;
use crate::{
    AnthropicAdapter, BedrockAdapter, ConverseAdapter, GeminiAdapter, JevUpstream,
    OpenAICompatAdapter, VertexAdapter,
};
use oag_core::endpoint::{is_aws_region, is_location};
use oag_core::provider::{AuthStyle, Dialect, Endpoint, Platform};
use oag_core::{Error, Provider, Result};
use reqwest::RequestBuilder;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::sync::Arc;
use typesafe_sdk::wire::SYSTEM_ONE_PATH;

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
    /// The version of Azure's deployments API an `azure` endpoint asks for;
    /// see [`EndpointSpec::with_api_version`].
    api_version: Option<String>,
    extra_headers: ExtraHeaders,
    /// Whether its adapter answers `served_models` by reading the endpoint's
    /// model list: the row's `discover_models`.
    discover: bool,
    /// The region an `aws` endpoint's requests go to and are signed for; see
    /// [`EndpointSpec::with_region`].
    region: Option<String>,
    /// The project a `gcp` endpoint's requests name; see
    /// [`EndpointSpec::with_project`].
    project: Option<String>,
    /// What a `gcp` endpoint's adapter mints its tokens through; see
    /// [`EndpointSpec::with_gcp_tokens`].
    gcp_tokens: Option<Arc<GcpTokenCache>>,
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
        let extra_headers = ExtraHeaders::parse(extra_headers, endpoint.platform())
            .map_err(|e| format!("endpoint `{}`: {e}", endpoint.name()))?;
        Ok(Self {
            endpoint,
            base_url: base_url.into(),
            auth,
            api_version: None,
            extra_headers,
            discover: false,
            region: None,
            project: None,
            gcp_tokens: None,
        })
    }

    /// Whether `pairs` are headers an endpoint named `name` on `platform` may
    /// add: judged as [`EndpointSpec::new`] judges them, refused in the same
    /// words, and with no [`Endpoint`] to build a spec for.
    ///
    /// For a writer checking a row it has not stored yet. Making an `Endpoint`
    /// interns its name for the life of the process, and a write refused after
    /// this, by the address its base URL resolves to or by the database, must
    /// leave nothing behind (see `oag_core::endpoint::CheckedColumns`).
    pub fn check_extra_headers<K, V>(
        name: &str,
        platform: Platform,
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> std::result::Result<(), String>
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        ExtraHeaders::parse(pairs, platform)
            .map(|_| ())
            .map_err(|e| format!("endpoint `{name}`: {e}"))
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

    /// A `GET` for the models this endpoint lists, where its dialect's vendor
    /// lists them: `{base}/models` for Chat Completions and `generateContent`,
    /// `{base}/v1/models` for the Messages API and System One. Each sits
    /// beside the path the dialect's adapter posts to, so a base URL that
    /// serves requests is the one asked.
    ///
    /// With `secret`, the key rides in the one header the endpoint's auth
    /// style names, as it does on every request; without one, in no header at
    /// all. The operator's headers go either way, and an Anthropic-dialect
    /// endpoint is told the API version its adapter speaks.
    ///
    /// Only a plain endpoint is asked. The other platforms list models on
    /// hosts, and with signatures, of their own: Bedrock's control plane, a
    /// token minted for Vertex, an Azure resource's deployments.
    pub fn models_request(&self, secret: Option<&str>) -> Result<reqwest::Request> {
        let name = self.endpoint.name();
        let platform = self.endpoint.platform();
        if platform != Platform::Plain {
            return Err(Error::Config(format!(
                "endpoint `{name}` is on the {} platform, whose model list is not asked yet; \
                 only plain endpoints are",
                platform.as_str()
            )));
        }
        let dialect = self.endpoint.dialect();
        let path = match dialect {
            Dialect::OpenAIChatCompletions | Dialect::GeminiGenerateContent => "/models",
            Dialect::AnthropicMessages => "/v1/models",
            Dialect::SystemOne => typesafe_sdk::wire::MODELS_PATH,
            other => {
                return Err(Error::Config(format!(
                    "endpoint `{name}` speaks {other}, which lists no models"
                )));
            }
        };
        let mut builder = crate::builder_client()?
            .get(format!("{}{path}", self.base_url))
            .header(reqwest::header::ACCEPT, "application/json");
        if dialect == Dialect::AnthropicMessages {
            builder = builder.header("anthropic-version", oag_proto::anthropic::API_VERSION);
        }
        if let Some(secret) = secret {
            builder = authenticate(builder, self.auth, secret);
        }
        self.extra_headers.apply(builder).build().map_err(|e| {
            Error::Internal(format!("building the model list request for `{name}`: {e}"))
        })
    }

    /// This spec, with the region its platform builds a host from.
    ///
    /// An `aws` endpoint needs one: the region names its host,
    /// `bedrock-runtime.{region}.amazonaws.com`, and the scope its requests
    /// are signed for, which a base URL does not change. Its base URL may be
    /// empty, which means that regional host.
    #[must_use]
    pub fn with_region(mut self, region: Option<String>) -> Self {
        self.region = region;
        self
    }

    /// This spec, with the API version an `azure` endpoint names.
    ///
    /// On Azure it picks the URL: `None` is Azure's v1 API, and a version is
    /// its deployments API at that version, sent as each request's
    /// `api-version`. Used as given; a stored one has passed
    /// [`oag_core::endpoint::is_api_version`]. No other platform reads it.
    #[must_use]
    pub fn with_api_version(mut self, api_version: Option<String>) -> Self {
        self.api_version = api_version;
        self
    }

    /// This spec, with the project its platform puts in a request's path.
    ///
    /// A `gcp` endpoint needs one, and a region too: every Vertex request is
    /// for `projects/{project}/locations/{region}`, at the region's own host
    /// unless the base URL names another. Its base URL may be empty, which
    /// means that host.
    #[must_use]
    pub fn with_project(mut self, project: Option<String>) -> Self {
        self.project = project;
        self
    }

    /// This spec, with the cache a `gcp` endpoint's adapter mints its tokens
    /// through.
    ///
    /// The gateway's one cache, never one of the adapter's own: a reload
    /// rebuilds every endpoint's adapter, and a cache per adapter would mint
    /// every account's token again after each one.
    #[must_use]
    pub fn with_gcp_tokens(mut self, tokens: Arc<GcpTokenCache>) -> Self {
        self.gcp_tokens = Some(tokens);
        self
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
/// - `host`, `content-length`, `transfer-encoding`, `connection`, `te`,
///   `upgrade`, `expect` and every `proxy-*`: where the request goes, how its
///   body is framed and where it ends, what becomes of the connection, and
///   what a proxy is told. The transport sets those, and one set here would
///   make a different request from the one built, or a different connection.
/// - `metadata-flavor`, the header a cloud's metadata server takes as proof a
///   request was meant for it. No model host asks for it.
/// - on an `aws` endpoint, every `x-amz-*`: `SigV4` sets and signs those, and
///   one set here would replace a header the signature covers, or add one it
///   does not.
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
    const REFUSED: [&'static str; 13] = [
        "authorization",
        "x-api-key",
        "x-goog-api-key",
        "api-key",
        "host",
        "cookie",
        "content-length",
        "transfer-encoding",
        "connection",
        "te",
        "upgrade",
        "expect",
        "metadata-flavor",
    ];

    /// `pairs` as the headers an endpoint on `platform` adds, or the first
    /// one it may not.
    pub(crate) fn parse<K, V>(
        pairs: impl IntoIterator<Item = (K, V)>,
        platform: Platform,
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
                     endpoint's account, never in a header; host, content-length, \
                     transfer-encoding, connection, te, upgrade, expect and proxy-* are the \
                     transport's to set; and metadata-flavor is for a cloud's metadata server"
                ));
            }
            if platform == Platform::Aws && name.as_str().starts_with("x-amz-") {
                return Err(format!(
                    "extra header `{name}` is not allowed on an aws endpoint: SigV4 sets and \
                     signs the x-amz-* headers itself"
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
///
/// The header is marked sensitive, so a request printed with `{:?}`, in a log
/// line or a panic message, shows the header's name and never the key, and an
/// HTTP/2 connection keeps the value out of its compression table. A value no
/// header can carry is handed to the builder as it was, for the builder to
/// refuse as it always has.
pub(crate) fn authenticate(
    builder: RequestBuilder,
    auth: AuthStyle,
    secret: &str,
) -> RequestBuilder {
    let (name, value) = match auth {
        AuthStyle::Bearer => ("authorization", format!("Bearer {secret}")),
        AuthStyle::XApiKey => ("x-api-key", secret.to_owned()),
        AuthStyle::XGoogApiKey => ("x-goog-api-key", secret.to_owned()),
        AuthStyle::ApiKeyHeader => ("api-key", secret.to_owned()),
        AuthStyle::None => return builder,
    };
    match HeaderValue::from_str(&value) {
        Ok(mut sensitive) => {
            sensitive.set_sensitive(true);
            builder.header(name, sensitive)
        }
        Err(_) => builder.header(name, value),
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
/// Every platform is served: plain here, and `aws`, `azure` and `gcp` below,
/// each before the plain check.
pub fn adapter(spec: &EndpointSpec) -> Result<Arc<dyn ProviderAdapter>> {
    let endpoint = spec.endpoint;
    let name = endpoint.name();
    if endpoint.platform() == Platform::Aws {
        return aws(spec);
    }
    if endpoint.platform() == Platform::Azure {
        return azure(spec);
    }
    if endpoint.platform() == Platform::Gcp {
        return gcp(spec);
    }
    plain(endpoint)?;

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
        // is not one. `system_one` below builds what the System One route
        // calls instead.
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

/// The adapter for an endpoint on the aws platform: Bedrock's runtime API in
/// the endpoint's own region, every request signed with `SigV4` for that
/// region.
///
/// The `anthropic` dialect is Claude in Anthropic's body through `InvokeModel`,
/// as the built-in Bedrock provider sends it; `bedrock_converse` is every other
/// model Bedrock serves, through `Converse`. Either way the request goes to the
/// region's own host unless the endpoint names a base URL (a VPC endpoint, a
/// proxy, a stand-in), and the signature is scoped to the region regardless.
/// No header carries a key: the signature is the credential, made from the
/// `access_key:secret[:session_token]` the endpoint's account holds.
///
/// The region is the spec's and never the gateway's: `gateway.bedrock_region`
/// is the built-in provider's alone.
fn aws(spec: &EndpointSpec) -> Result<Arc<dyn ProviderAdapter>> {
    let endpoint = spec.endpoint;
    let name = endpoint.name();
    let region = spec
        .region
        .as_deref()
        .filter(|region| is_aws_region(region))
        .ok_or_else(|| {
            Error::Config(format!(
                "endpoint `{name}` is on the aws platform, and names no AWS region"
            ))
        })?;
    // An empty base URL is no override: the region's own host.
    let runtime = BedrockAdapter::for_endpoint(endpoint, region)
        .with_endpoint(Some(spec.base_url.clone()))
        .with_headers(spec.extra_headers.clone());
    match endpoint.dialect() {
        Dialect::AnthropicMessages => Ok(Arc::new(runtime)),
        Dialect::BedrockConverse => Ok(Arc::new(ConverseAdapter::new(runtime))),
        other => Err(Error::Config(format!(
            "endpoint `{name}` speaks {other}, which the aws platform does not serve"
        ))),
    }
}

/// The adapter for an endpoint on the gcp platform: Vertex AI in the
/// endpoint's own project and region, every request carrying a token minted
/// from the service account its credential holds.
///
/// The `gemini` dialect is Google's models through `generateContent`;
/// `anthropic` is Claude through `rawPredict`. Either way the request goes to
/// the region's own host unless the endpoint names a base URL (a Private
/// Service Connect endpoint, a proxy, a stand-in), and the path names the
/// project and the region regardless. The key header is `authorization`,
/// whatever the spec's auth says: a minted token is a bearer token.
///
/// Refused without a region and a project that [`is_location`] accepts, or
/// without the gateway's token cache: a request this adapter could build
/// without them would go nowhere Vertex answers, or carry no token.
fn gcp(spec: &EndpointSpec) -> Result<Arc<dyn ProviderAdapter>> {
    let endpoint = spec.endpoint;
    let name = endpoint.name();
    let region = spec.region.as_deref().filter(|region| is_location(region));
    let project = spec
        .project
        .as_deref()
        .filter(|project| is_location(project));
    let (Some(region), Some(project)) = (region, project) else {
        return Err(Error::Config(format!(
            "endpoint `{name}` is on the gcp platform, and names no region and project"
        )));
    };
    let tokens = spec.gcp_tokens.clone().ok_or_else(|| {
        Error::Config(format!(
            "endpoint `{name}` is on the gcp platform, and was given no token cache to \
             mint its credentials' tokens with"
        ))
    })?;
    // An empty base URL is no override: the region's own host.
    let base_url = Some(spec.base_url.as_str()).filter(|base| !base.is_empty());
    let adapter = VertexAdapter::for_endpoint(
        endpoint,
        base_url,
        region,
        project,
        spec.extra_headers.clone(),
        tokens,
    )?;
    Ok(Arc::new(adapter))
}

/// The upstream that serves `spec`'s System One endpoint: Jev's two requests,
/// at the endpoint's base URL, with its key where it was registered to go and
/// its extra headers on both.
///
/// A question set is posted at `path` beneath the base URL, or at
/// `/v1/systemone`, Jev's own, when the endpoint names none. `path` is used as
/// given; a stored one has passed [`oag_core::endpoint::is_path`]. The listing
/// is read from `{base}/v1/models` either way, which is where both Jev and
/// Merge Gateway serve it.
pub fn system_one(spec: &EndpointSpec, path: Option<&str>) -> Result<JevUpstream> {
    let endpoint = spec.endpoint;
    if endpoint.dialect() != Dialect::SystemOne {
        return Err(Error::Config(format!(
            "endpoint `{}` speaks {}, not System One",
            endpoint.name(),
            endpoint.dialect()
        )));
    }
    plain(endpoint)?;
    Ok(JevUpstream::for_endpoint(
        spec.base_url.clone(),
        path.unwrap_or(SYSTEM_ONE_PATH).to_owned(),
        spec.auth,
        spec.extra_headers.clone(),
    ))
}

/// Refuses an endpoint on any platform but plain, where only a plain one is
/// served: a System One endpoint on any platform but plain. A chat endpoint
/// on aws, azure or gcp never gets here.
///
/// Refused rather than sent a request built for a plain host, which another
/// platform would reject for want of a signature, a minted token or a
/// deployment path.
fn plain(endpoint: Endpoint) -> Result<()> {
    match endpoint.platform() {
        Platform::Plain => Ok(()),
        platform @ (Platform::Aws | Platform::Gcp | Platform::Azure) => {
            Err(Error::Config(format!(
                "endpoint `{}` is on the {} platform, which is not supported yet",
                endpoint.name(),
                platform.as_str()
            )))
        }
    }
}

/// The adapter for an endpoint on the azure platform: Chat Completions at its
/// Azure resource, with its key in `api-key`, posted to Azure's v1 API or, when
/// the spec names an API version, to its deployments API at that version. See
/// [`crate::azure::AzureOpenAIAdapter`].
///
/// The platform matrix gives Azure the `OpenAI` dialect alone, and `api-key`
/// as the one way it takes a key, and the loader holds a row to both. This
/// does not take the loader's word for either: a spec built some other way is
/// refused rather than sent a request Azure would not read.
fn azure(spec: &EndpointSpec) -> Result<Arc<dyn ProviderAdapter>> {
    let endpoint = spec.endpoint;
    let name = endpoint.name();
    if endpoint.dialect() != Dialect::OpenAIChatCompletions {
        return Err(Error::Config(format!(
            "endpoint `{name}` speaks {}, which the azure platform does not serve",
            endpoint.dialect()
        )));
    }
    if !spec.auth.suits(Platform::Azure) {
        return Err(Error::Config(format!(
            "endpoint `{name}` is on the azure platform, which takes its key in `api-key` \
             (auth api_key_header), not {}",
            spec.auth.as_str()
        )));
    }
    Ok(Arc::new(crate::azure::AzureOpenAIAdapter::for_endpoint(
        endpoint,
        spec.base_url.clone(),
        spec.api_version.clone(),
        spec.extra_headers.clone(),
    )))
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
            reasoning_efforts: None,
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

    /// The check a writer runs before storing a row refuses what the spec
    /// would refuse, naming the endpoint, and passes what it would keep.
    #[test]
    fn checking_extra_headers_refuses_what_the_spec_refuses() {
        let err = EndpointSpec::check_extra_headers(
            "t7-check",
            Platform::Plain,
            [("Authorization", "Bearer smuggled")],
        )
        .expect_err("a second auth header is refused");
        assert!(err.starts_with("endpoint `t7-check`: "), "{err}");
        assert!(
            EndpointSpec::check_extra_headers("t7-check", Platform::Plain, [("x-team", "core")])
                .is_ok()
        );
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

    /// The headers that steer the connection rather than the request, and the
    /// one a cloud metadata server answers to, are refused on every platform;
    /// `x-amz-*` on an aws endpoint, whose signature sets and signs them.
    #[test]
    fn an_extra_header_may_not_steer_the_transport_or_speak_for_the_platform() {
        let plain = endpoint(
            "t3-transport",
            Dialect::OpenAIChatCompletions,
            Platform::Plain,
        );
        for name in [
            "Transfer-Encoding",
            "connection",
            "TE",
            "Upgrade",
            "expect",
            "Metadata-Flavor",
        ] {
            let err = EndpointSpec::new(plain, "http://h", AuthStyle::Bearer, [(name, "v")])
                .expect_err(name);
            assert!(
                err.contains(&format!("`{}` is not allowed", name.to_ascii_lowercase())),
                "{name}: {err}"
            );
        }

        let aws = endpoint("t3-amz", Dialect::AnthropicMessages, Platform::Aws);
        for name in [
            "x-amz-security-token",
            "X-Amz-Date",
            "x-amz-content-sha256",
            "x-amz-target",
        ] {
            let err = EndpointSpec::new(aws, "", AuthStyle::None, [(name, "v")]).expect_err(name);
            assert!(
                err.contains(&format!(
                    "`{}` is not allowed on an aws endpoint",
                    name.to_ascii_lowercase()
                )),
                "{name}: {err}"
            );
        }
        // Anywhere else an `x-amz-` header is the operator's to send.
        let spec = EndpointSpec::new(
            plain,
            "http://h",
            AuthStyle::Bearer,
            [("x-amz-meta-team", "t3")],
        )
        .expect("not an aws endpoint");
        assert_eq!(spec.extra_headers.0["x-amz-meta-team"], "t3");
        EndpointSpec::new(aws, "", AuthStyle::None, [("x-team", "t3")])
            .expect("an aws endpoint keeps every other header");
    }

    /// The key's header is marked sensitive, whichever header it rides in: a
    /// request printed for a log names the header and never shows the key.
    #[test]
    fn the_header_a_key_rides_in_is_marked_sensitive() {
        for (auth, expected) in STYLES {
            let request = authenticate(reqwest::Client::new().get("http://h/v1/models"), auth, KEY)
                .build()
                .expect("builds");
            let printed = format!("{request:?}");
            assert!(!printed.contains(KEY), "{}: {printed}", auth.as_str());
            match expected {
                Some((name, value)) => {
                    let sent = &request.headers()[name];
                    assert!(sent.is_sensitive(), "{name}");
                    assert_eq!(sent, value, "{name}: still the key");
                }
                None => assert!(
                    AUTH_HEADERS
                        .iter()
                        .all(|name| request.headers().get(*name).is_none()),
                    "no key, no header"
                ),
            }
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

    // ── the aws platform ─────────────────────────────────────────────────────

    /// An aws endpoint's key, packed as Bedrock's are. Obviously not one.
    const AWS_KEY: &str = "TESTACCESSKEY:TESTSECRETKEY";

    fn aws_spec(name: &str, dialect: Dialect, region: &str, base_url: &str) -> EndpointSpec {
        EndpointSpec::new(
            endpoint(name, dialect, Platform::Aws),
            base_url,
            AuthStyle::None,
            EXTRA,
        )
        .unwrap()
        .with_region(Some(region.to_owned()))
    }

    /// What `spec`'s adapter builds for one request, not streamed.
    fn build_aws(spec: &EndpointSpec) -> reqwest::Request {
        let adapter = adapter(spec).expect("an aws endpoint has an adapter");
        let (canonical, model) = (request(), model(spec.endpoint));
        let mut credential = credential();
        AWS_KEY.clone_into(&mut credential.access_token);
        adapter
            .build(&UpstreamRequest {
                canonical: &canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .expect("builds")
    }

    /// The region and the service a `SigV4` `authorization` is scoped to.
    fn signed_for(authorization: &str) -> (String, String) {
        let scope = authorization
            .strip_prefix("AWS4-HMAC-SHA256 Credential=TESTACCESSKEY/")
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_else(|| panic!("not a SigV4 authorization: {authorization}"));
        // `{date}/{region}/{service}/aws4_request`
        let parts: Vec<&str> = scope.split('/').collect();
        assert_eq!(parts.len(), 4, "{scope}");
        assert_eq!(parts[3], "aws4_request", "{scope}");
        (parts[1].to_owned(), parts[2].to_owned())
    }

    /// An aws endpoint is Bedrock in the region it names, whichever model
    /// family it serves.
    ///
    /// Neither region is `us-east-1`, the built-in's default, and the regional
    /// host and the signature's scope are both asserted: an adapter built for
    /// any region but the endpoint's own, the gateway's included, fails here.
    #[test]
    fn an_aws_endpoint_is_bedrock_in_its_own_region_whichever_model_family() {
        for (dialect, region, framing, action) in [
            (
                Dialect::AnthropicMessages,
                "eu-west-3",
                Framing::AwsEventStream,
                "invoke",
            ),
            (
                Dialect::BedrockConverse,
                "ap-northeast-2",
                Framing::AwsConverseStream,
                "converse",
            ),
        ] {
            let name = format!("t9-aws-{region}");
            // No base URL: the region's own host.
            let spec = aws_spec(&name, dialect, region, "");
            let adapter = adapter(&spec).expect("the aws platform is served");
            assert_eq!(
                adapter.provider(),
                Provider::Custom(spec.endpoint),
                "{name}"
            );
            assert_eq!(adapter.dialect(), dialect, "{name}");
            assert_eq!(adapter.framing(), framing, "{name}");
            assert!(!adapter.always_streams(), "{name}");

            let built = build_aws(&spec);
            assert_eq!(
                built.url().as_str(),
                format!("https://bedrock-runtime.{region}.amazonaws.com/model/some-model/{action}")
            );
            let authorization = built.headers()["authorization"].to_str().unwrap();
            assert_eq!(
                signed_for(authorization),
                (region.to_owned(), "bedrock".to_owned()),
                "{name}"
            );
            for header in ["x-api-key", "x-goog-api-key", "api-key"] {
                assert!(built.headers().get(header).is_none(), "{name}: {header}");
            }
        }
    }

    /// Two aws endpoints in two regions, each with a stand-in of its own: each
    /// stand-in is sent only its endpoint's request, signed for its region,
    /// with a signature that holds over what actually arrived.
    #[tokio::test]
    async fn two_aws_endpoints_in_two_regions_are_each_signed_for_their_own() {
        let (paris, sydney) = (MockServer::start().await, MockServer::start().await);
        let endpoints = [
            (
                aws_spec(
                    "t9-aws-paris",
                    Dialect::BedrockConverse,
                    "eu-west-3",
                    &paris.uri(),
                ),
                &paris,
                "eu-west-3",
                "/model/some-model/converse",
            ),
            (
                aws_spec(
                    "t9-aws-sydney",
                    Dialect::AnthropicMessages,
                    "ap-southeast-2",
                    &sydney.uri(),
                ),
                &sydney,
                "ap-southeast-2",
                "/model/some-model/invoke",
            ),
        ];
        for (_, server, _, at) in &endpoints {
            Mock::given(method("POST"))
                .and(path(*at))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
                .expect(1)
                .mount(server)
                .await;
        }
        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        for (spec, _, _, at) in &endpoints {
            let answered = transport.execute(build_aws(spec)).await.expect("answers");
            assert_eq!(
                answered.status(),
                200,
                "{}: missed {at}",
                spec.endpoint.name()
            );
        }

        for (spec, server, region, _) in &endpoints {
            let name = spec.endpoint.name();
            server.verify().await;
            let received = server.received_requests().await.expect("recording is on");
            let [sent] = received.as_slice() else {
                panic!("{name}: {received:?}");
            };
            let header = |name: &str| sent.headers[name].to_str().unwrap().to_owned();
            let authorization = header("authorization");
            assert_eq!(
                signed_for(&authorization),
                ((*region).to_owned(), "bedrock".to_owned()),
                "{name}"
            );
            let at = time::PrimitiveDateTime::parse(
                &header("x-amz-date"),
                &time::macros::format_description!("[year][month][day]T[hour][minute][second]Z"),
            )
            .unwrap()
            .assume_utc();
            let recomputed = crate::sigv4::sign(
                &BedrockAdapter::credentials(AWS_KEY).unwrap(),
                region,
                "bedrock",
                crate::sigv4::SigningRequest {
                    method: "POST",
                    path: sent.url.path(),
                    host: &header("host"),
                    body: &sent.body,
                },
                at,
            );
            assert_eq!(authorization, recomputed.authorization, "{name}");
            for (extra, value) in EXTRA {
                assert_eq!(header(extra), value, "{name}: the operator's {extra}");
            }
            for key in ["x-api-key", "x-goog-api-key", "api-key"] {
                assert!(sent.headers.get(key).is_none(), "{name}: {key}");
            }
        }
    }

    #[test]
    fn an_aws_endpoint_without_an_aws_region_is_refused() {
        for region in [None, Some("us-central1"), Some("eu-west-3.evil.example")] {
            let spec = EndpointSpec::new(
                endpoint("t9-aws-nowhere", Dialect::BedrockConverse, Platform::Aws),
                "",
                AuthStyle::None,
                EXTRA,
            )
            .unwrap()
            .with_region(region.map(str::to_owned));
            let err = adapter(&spec).expect_err("no host to send to").to_string();
            assert!(
                err.contains(
                    "endpoint `t9-aws-nowhere` is on the aws platform, and names no AWS region"
                ),
                "{region:?}: {err}"
            );
        }
    }

    #[test]
    fn an_aws_endpoint_in_a_dialect_bedrock_does_not_speak_is_refused() {
        // The platform matrix never pairs these with aws, and this does not
        // take the matrix's word for it.
        for dialect in [
            Dialect::OpenAIChatCompletions,
            Dialect::GeminiGenerateContent,
            Dialect::SystemOne,
        ] {
            let spec = aws_spec("t9-aws-wrong", dialect, "us-west-2", "");
            let err = adapter(&spec).expect_err("not Bedrock's").to_string();
            assert!(
                err.contains(&format!(
                    "speaks {dialect}, which the aws platform does not serve"
                )),
                "{err}"
            );
        }
    }

    // ── the gcp platform ─────────────────────────────────────────────────────

    /// A token cache at a port nothing listens on: nothing here mints.
    fn idle_tokens() -> Arc<GcpTokenCache> {
        Arc::new(GcpTokenCache::new("http://127.0.0.1:1/token").unwrap())
    }

    fn gcp_spec(name: &str, dialect: Dialect, region: &str, base_url: &str) -> EndpointSpec {
        EndpointSpec::new(
            endpoint(name, dialect, Platform::Gcp),
            base_url,
            AuthStyle::Bearer,
            EXTRA,
        )
        .unwrap()
        .with_region(Some(region.to_owned()))
        .with_project(Some("oag-test".to_owned()))
        .with_gcp_tokens(idle_tokens())
    }

    /// A gcp endpoint is Vertex in its own project and region, Gemini or
    /// Claude: at the region's host, the global one, or its base URL, with a
    /// bearer token and the operator's headers.
    #[test]
    fn a_gcp_endpoint_is_vertex_in_its_own_project_and_region() {
        let cases = [
            (
                Dialect::GeminiGenerateContent,
                "us-central1",
                "",
                "https://us-central1-aiplatform.googleapis.com/v1/projects/oag-test/\
                 locations/us-central1/publishers/google/models/some-model:generateContent",
            ),
            (
                Dialect::AnthropicMessages,
                "global",
                "",
                "https://aiplatform.googleapis.com/v1/projects/oag-test/\
                 locations/global/publishers/anthropic/models/some-model:rawPredict",
            ),
            (
                Dialect::GeminiGenerateContent,
                "europe-west4",
                "http://127.0.0.1:9/psc",
                "http://127.0.0.1:9/psc/v1/projects/oag-test/\
                 locations/europe-west4/publishers/google/models/some-model:generateContent",
            ),
        ];
        for (dialect, region, base_url, url) in cases {
            let name = format!("t10-gcp-{region}");
            let spec = gcp_spec(&name, dialect, region, base_url);
            let adapter = adapter(&spec).expect("the gcp platform is served");
            assert_eq!(
                adapter.provider(),
                Provider::Custom(spec.endpoint),
                "{name}"
            );
            assert_eq!(adapter.dialect(), dialect, "{name}");
            assert_eq!(adapter.framing(), Framing::Sse, "{name}");
            assert!(!adapter.always_streams(), "{name}");

            let (canonical, model) = (request(), model(spec.endpoint));
            let mut credential = credential();
            "ya29.t10".clone_into(&mut credential.access_token);
            let built = adapter
                .build(&UpstreamRequest {
                    canonical: &canonical,
                    model: &model,
                    credential: &credential,
                    session: None,
                })
                .expect("builds");
            assert_eq!(built.url().as_str(), url, "{name}");
            let headers = built.headers();
            let bearers: Vec<_> = headers.get_all("authorization").iter().collect();
            assert_eq!(bearers, ["Bearer ya29.t10"], "{name}");
            for key in ["x-api-key", "x-goog-api-key", "api-key"] {
                assert!(headers.get(key).is_none(), "{name}: {key}");
            }
            for (extra, value) in EXTRA {
                assert_eq!(headers[extra], value, "{name}: the operator's {extra}");
            }
        }
    }

    /// Without a region and a project a URL can hold, or without the
    /// gateway's token cache, a gcp endpoint has no adapter.
    #[test]
    fn a_gcp_endpoint_without_a_location_or_a_token_cache_is_refused() {
        let spec = || {
            EndpointSpec::new(
                endpoint(
                    "t10-gcp-nowhere",
                    Dialect::GeminiGenerateContent,
                    Platform::Gcp,
                ),
                "",
                AuthStyle::Bearer,
                EXTRA,
            )
            .unwrap()
        };
        let located = |region: Option<&str>, project: Option<&str>| {
            spec()
                .with_region(region.map(str::to_owned))
                .with_project(project.map(str::to_owned))
                .with_gcp_tokens(idle_tokens())
        };
        for unlocated in [
            located(None, Some("oag-test")),
            located(Some("us-central1"), None),
            located(Some("us-central1.example.test"), Some("oag-test")),
            located(Some("us-central1"), Some("oag-test/../other")),
        ] {
            let err = adapter(&unlocated)
                .expect_err("nowhere to send it")
                .to_string();
            assert!(
                err.contains(
                    "endpoint `t10-gcp-nowhere` is on the gcp platform, and names no region \
                     and project"
                ),
                "{err}"
            );
        }
        let untokened = spec()
            .with_region(Some("global".to_owned()))
            .with_project(Some("oag-test".to_owned()));
        let err = adapter(&untokened)
            .expect_err("no token to send")
            .to_string();
        assert!(err.contains("was given no token cache"), "{err}");
    }

    #[test]
    fn a_gcp_endpoint_in_a_dialect_vertex_does_not_speak_is_refused() {
        // The platform matrix never pairs these with gcp, and this does not
        // take the matrix's word for it.
        for dialect in [
            Dialect::OpenAIChatCompletions,
            Dialect::SystemOne,
            Dialect::BedrockConverse,
        ] {
            let spec = gcp_spec("t10-gcp-wrong", dialect, "global", "");
            let err = adapter(&spec).expect_err("not Vertex's").to_string();
            assert!(
                err.contains(&format!(
                    "speaks {dialect}, which the gcp platform does not serve"
                )),
                "{err}"
            );
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

    /// Each dialect's list is asked where its vendor serves one, beside the
    /// path its adapter posts to, and a key goes with it only when one is
    /// given, in the one header the endpoint names.
    #[test]
    fn the_model_list_is_asked_where_each_dialect_lists_it() {
        for (name, dialect, base_path, list_path) in [
            (
                "t5-list-openai",
                Dialect::OpenAIChatCompletions,
                "/v1",
                "/v1/models",
            ),
            (
                "t5-list-anthropic",
                Dialect::AnthropicMessages,
                "",
                "/v1/models",
            ),
            (
                "t5-list-gemini",
                Dialect::GeminiGenerateContent,
                "/v1beta",
                "/v1beta/models",
            ),
            ("t5-list-jev", Dialect::SystemOne, "/jev", "/jev/v1/models"),
        ] {
            let spec = EndpointSpec::new(
                endpoint(name, dialect, Platform::Plain),
                format!("http://h.example{base_path}"),
                AuthStyle::XGoogApiKey,
                EXTRA,
            )
            .unwrap();

            let bare = spec.models_request(None).unwrap();
            assert_eq!(bare.method(), reqwest::Method::GET, "{name}");
            assert_eq!(bare.url().path(), list_path, "{name}");
            assert_eq!(bare.url().query(), None, "{name}");
            for header in AUTH_HEADERS {
                assert!(
                    bare.headers().get(header).is_none(),
                    "{name}: no key was given, so none rides in {header}"
                );
            }
            for (extra, value) in EXTRA {
                assert_eq!(bare.headers()[extra], value, "{name}: {extra}");
            }
            assert_eq!(
                bare.headers()
                    .get("anthropic-version")
                    .map(|v| v.to_str().unwrap()),
                (dialect == Dialect::AnthropicMessages)
                    .then_some(oag_proto::anthropic::API_VERSION),
                "{name}: only the Messages API is told its version"
            );

            let keyed = spec.models_request(Some(KEY)).unwrap();
            for header in AUTH_HEADERS {
                let wanted = (header == "x-goog-api-key").then_some(KEY);
                assert_eq!(
                    keyed.headers().get(header).map(|v| v.to_str().unwrap()),
                    wanted,
                    "{name}: {header}"
                );
            }
            assert_eq!(keyed.url().path(), list_path, "{name}");
        }
    }

    #[test]
    fn only_a_plain_endpoint_is_asked_for_its_models() {
        for (platform, dialect) in [
            (Platform::Aws, Dialect::AnthropicMessages),
            (Platform::Gcp, Dialect::GeminiGenerateContent),
            (Platform::Azure, Dialect::OpenAIChatCompletions),
        ] {
            let name = format!("t5-list-{}", platform.as_str());
            let spec = plain_spec(
                endpoint(&name, dialect, platform),
                "http://h.example",
                AuthStyle::Bearer,
            );
            let err = spec.models_request(None).expect_err(&name).to_string();
            assert!(
                err.contains(&format!("the {} platform", platform.as_str())),
                "{err}"
            );
            assert!(err.contains("only plain endpoints are"), "{err}");
        }
        let spec = plain_spec(
            endpoint(
                "t5-list-responses",
                Dialect::OpenAIResponses,
                Platform::Plain,
            ),
            "http://h.example",
            AuthStyle::Bearer,
        );
        let err = spec.models_request(None).expect_err("no list").to_string();
        assert!(
            err.contains("OpenAI Responses, which lists no models"),
            "{err}"
        );
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

    /// A System One endpoint's question set, through the real transport to a
    /// mock that answers only `at`: what arrived there.
    async fn ask(upstream: &JevUpstream, server: &MockServer, at: &str) -> wiremock::Request {
        Mock::given(method("POST"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(server)
            .await;
        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let built = upstream
            .system_one(&credential(), &br#"{"state":"x","questions":{}}"#[..])
            .expect("builds");
        let response = transport.execute(built).await.expect("the mock answers");
        assert_eq!(response.status(), 200, "the question set missed {at}");
        server.verify().await;
        let mut received = server.received_requests().await.expect("recording is on");
        assert_eq!(received.len(), 1);
        received.remove(0)
    }

    /// Each auth style puts the key in its one header on a System One
    /// endpoint too, and the operator's headers ride along, at the path the
    /// endpoint names or at Jev's own.
    #[tokio::test]
    async fn a_system_one_endpoint_posts_at_its_path_with_its_key_where_it_says() {
        for (auth, expected) in STYLES {
            for (named, at) in [
                (Some("/v1/decisions"), "/gw/v1/decisions"),
                (None, "/gw/v1/systemone"),
            ] {
                let server = MockServer::start().await;
                let name = format!("t7-jev-{}", auth.as_str());
                let spec = plain_spec(
                    endpoint(&name, Dialect::SystemOne, Platform::Plain),
                    &format!("{}/gw", server.uri()),
                    auth,
                );
                let upstream = system_one(&spec, named).expect("a System One upstream");
                let received = ask(&upstream, &server, at).await;
                for header in AUTH_HEADERS {
                    let values: Vec<_> = received.headers.get_all(header).iter().collect();
                    let wanted: Vec<&str> = expected
                        .filter(|&(named, _)| named == header)
                        .map(|(_, value)| value)
                        .into_iter()
                        .collect();
                    assert_eq!(values, wanted, "{name} at {at}: {header}");
                }
                for (extra, value) in EXTRA {
                    assert_eq!(
                        received.headers.get(extra).map(|v| v.to_str().unwrap()),
                        Some(value),
                        "{name}: the operator's {extra} is sent"
                    );
                }
                assert_eq!(received.body, br#"{"state":"x","questions":{}}"#);
            }
        }
    }

    /// The listing is read from `{base}/v1/models`, a page at a time, with the
    /// same key and headers as a question set.
    #[tokio::test]
    async fn a_system_one_endpoint_lists_its_models_beneath_its_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gw/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .expect(1)
            .mount(&server)
            .await;
        let spec = plain_spec(
            endpoint("t7-jev-listing", Dialect::SystemOne, Platform::Plain),
            &format!("{}/gw", server.uri()),
            AuthStyle::XApiKey,
        );
        let upstream = system_one(&spec, Some("/v1/decisions")).expect("an upstream");
        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let built = upstream
            .models_page(&credential(), Some("next-one"))
            .expect("builds");
        assert_eq!(
            transport.execute(built).await.expect("answers").status(),
            200
        );
        server.verify().await;
        let received = server.received_requests().await.expect("recording is on");
        assert_eq!(received[0].url.query(), Some("limit=500&cursor=next-one"));
        assert_eq!(received[0].headers["x-api-key"], KEY);
        assert!(received[0].headers.get("authorization").is_none());
        assert_eq!(received[0].headers["x-title"], "open-ai-gateway");
    }

    #[test]
    fn only_a_plain_system_one_endpoint_gets_a_system_one_upstream() {
        let chat = plain_spec(
            endpoint(
                "t7-not-jev",
                Dialect::OpenAIChatCompletions,
                Platform::Plain,
            ),
            "http://h",
            AuthStyle::Bearer,
        );
        let err = system_one(&chat, None)
            .expect_err("a chat endpoint")
            .to_string();
        assert!(
            err.contains("speaks OpenAI Chat Completions, not System One"),
            "{err}"
        );
        assert!(err.contains("t7-not-jev"), "{err}");

        // The platform matrix never pairs System One with a cloud, and this
        // does not take the matrix's word for it.
        let cloud = plain_spec(
            endpoint("t7-cloud-jev", Dialect::SystemOne, Platform::Aws),
            "http://h",
            AuthStyle::None,
        );
        let err = system_one(&cloud, None).expect_err("a cloud").to_string();
        assert!(
            err.contains("the aws platform, which is not supported yet"),
            "{err}"
        );
    }
}
