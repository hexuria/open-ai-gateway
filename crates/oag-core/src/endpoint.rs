//! What an endpoint row may say.
//!
//! One set of rules for everything that reads the `endpoint` table: the
//! gateway's reload, which serves each row that passes and skips each one that
//! does not, and the CLI, which registers the same rows so `--provider <name>`
//! parses. Two readers with a rule each would disagree, and the disagreement
//! would be a key the CLI accepts for an endpoint the gateway never serves.
//!
//! The schema's CHECKs (migration 0020) are the second line behind these, and
//! they hold less: the platform matrix and the base URL's scheme, but not what
//! a region may contain, which auth style a platform takes, which hosts a
//! plain endpoint may not name, which hosts an azure one may, or what an API
//! version looks like.
//!
//! Like the rest of this crate, nothing here does I/O. A base URL is judged by
//! what it says and never by what its name resolves to: a reload that asked DNS
//! would let a resolver decide which endpoints serve. The resolved address is
//! checked where an endpoint is written instead.

use crate::provider::{AuthStyle, Dialect, Endpoint, Platform};
use crate::service::catalog_url;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use url::{Host, Url};

/// A configured base URL, in the one shape every adapter's concatenation expects.
///
/// Trailing slashes go, because every adapter builds its request URL by
/// appending a path: `https://host/` became `https://host//v1/messages`, which
/// most upstreams tolerate and some do not — a configuration bug that works in
/// the deployment where it was typed and fails in the next one.
///
/// A query or a fragment is refused rather than trimmed. Appending a path after
/// either produces a URL that means something different — `https://host/?x=1`
/// plus `/v1/messages` is a query string containing a path, not a path — and
/// guessing which half the operator meant is worse than saying so. Refused at
/// startup, where it is a config error, rather than surfacing later as a 404
/// from an upstream that never received the request.
pub fn normalise_base_url(provider: &str, raw: &str) -> crate::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(crate::Error::Config(format!(
            "the base URL for {provider} is empty"
        )));
    }
    if let Some(bad) = ['?', '#'].into_iter().find(|c| trimmed.contains(*c)) {
        return Err(crate::Error::Config(format!(
            "the base URL for {provider} contains '{bad}': {trimmed}. Every request path is \
             appended to it, so a query or fragment here would silently change what the \
             resulting URL means."
        )));
    }
    if !trimmed.contains("://") {
        return Err(crate::Error::Config(format!(
            "the base URL for {provider} has no scheme: {trimmed}"
        )));
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

/// Hosts a `plain` endpoint may not point at, each with every name under it.
///
/// The first seven are providers this gateway serves itself, and whose
/// subscriptions it either refuses outright (Claude) or holds for their one
/// owner only (`ChatGPT`, Grok). A plain endpoint is a second way to reach a
/// host with none of that: its credentials are pooled `api_key` rows, and no
/// rule reads what one carries. Pointed at `api.anthropic.com`, an endpoint
/// would carry a Claude subscription token filed as an ordinary key past the
/// refusal migration 0018 put on the `anthropic` provider.
///
/// The last three are the clouds. Each serves models through a platform of its
/// own (`aws`, `gcp`, `azure`) that signs a request the way that cloud expects,
/// or through the built-in Gemini and Bedrock providers. A plain request sent
/// there is a key sent somewhere it cannot work.
pub const PLAIN_REFUSED_HOSTS: [&str; 10] = [
    "anthropic.com",
    "claude.ai",
    "claude.com",
    "chatgpt.com",
    "openai.com",
    "x.ai",
    "grok.com",
    "googleapis.com",
    "amazonaws.com",
    "azure.com",
];

/// The entry of [`PLAIN_REFUSED_HOSTS`] that `url`'s host is or is under,
/// when `platform` is plain.
///
/// Read from the host the `url` crate parsed, which for http and https is
/// already lowercase, percent-decoded and IDNA-mapped: `API.OpenAI.com`,
/// `api%2Eopenai.com` and a full-width dot all arrive as `api.openai.com`. A
/// trailing dot names the same host to DNS and is ignored, and a port is not
/// part of the host. An IP literal has no name to compare, so it passes: whose
/// address it is, is a question only DNS could answer.
#[must_use]
pub fn plain_refused_host(url: &Url, platform: Platform) -> Option<&'static str> {
    if platform != Platform::Plain {
        return None;
    }
    let Some(Host::Domain(host)) = url.host() else {
        return None;
    };
    let host = host.trim_end_matches('.');
    PLAIN_REFUSED_HOSTS.into_iter().find(|&refused| {
        host == refused
            || host
                .strip_suffix(refused)
                .is_some_and(|under| under.ends_with('.'))
    })
}

/// `raw` as the base URL of an endpoint on `platform`, normalised as a
/// built-in's is, or why it cannot be one.
///
/// No control character, first: a URL parser drops a tab or a newline and
/// escapes the rest, so the URL every rule below judges would not be the text
/// a writer stored, and that text would reach a terminal as written. Then
/// [`catalog_url`]: http or https, no credentials in it, and no link-local or
/// metadata literal. Loopback and private addresses pass, so a model server
/// on the operator's own network can be registered. Then the compliance
/// guard, [`plain_refused_host`], and then [`normalise_base_url`]: no query,
/// no fragment. An azure endpoint's must then be an Azure resource's and
/// nothing more ([`AZURE_HOSTS`]), so no address of any kind passes there.
/// No DNS; see the module.
///
/// What comes back is the URL the parser read, which is the one every rule
/// judged: scheme and host in lowercase, no default port, the path
/// percent-encoded, and no trailing slash. So what a writer stores is what was
/// checked, and every spelling of one base URL is stored as one.
pub fn endpoint_base_url(raw: &str, platform: Platform) -> Result<String, Refusal> {
    if raw.trim().chars().any(char::is_control) {
        // Not repeated: the value is what holds the character.
        return Err(Refusal::new(
            Reason::BaseUrl,
            "the base URL holds a control character, such as a tab, a newline or an \
             escape: a URL parser would drop or escape it, so the URL checked would not \
             be the one stored. Type it again without one",
        ));
    }
    let url = catalog_url(raw).map_err(|e| Refusal::new(Reason::BaseUrl, words(e)))?;
    if let Some(refused) = plain_refused_host(&url, platform) {
        return Err(Refusal::new(
            Reason::Compliance,
            format!(
                "the base URL's host is {refused} or under it, where a plain endpoint may \
                 not point: that provider is served by its built-in adapter or by the aws, \
                 gcp or azure platform. See docs/compliance.md"
            ),
        ));
    }
    normalise_base_url("this endpoint", raw)
        .map_err(|e| Refusal::new(Reason::BaseUrl, words(e)))?;
    if platform == Platform::Azure {
        return azure_base_url(&url);
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// The hosts an `azure` endpoint's base URL may name, each with one resource's
/// name in front: Azure `OpenAI`'s `{resource}.openai.azure.com`, and Azure AI
/// Foundry's `{resource}.services.ai.azure.com`.
pub const AZURE_HOSTS: [&str; 2] = ["openai.azure.com", "services.ai.azure.com"];

/// `url`, an `azure` endpoint's base URL, as the one URL an Azure resource
/// answers at, `https://{resource}.{host}` for a host in [`AZURE_HOSTS`], or
/// why it is not one.
///
/// An allow-list, where a plain endpoint has a deny-list, because Azure needs
/// no more: every request an azure endpoint is sent goes to its resource, and
/// the adapter writes the rest of the URL itself (`/openai/v1/…`, or
/// `/openai/deployments/…?api-version=…`). So the base URL is the resource and
/// nothing else: https, Azure's own port, no path. Anything wider, a host on
/// the operator's network or an address, is somewhere a request built for
/// Azure, with an Azure key in it, has no business going, and the narrowest
/// rule keeps that surface as small as the platform allows.
///
/// `{resource}` is 2 to 63 of `a-z`, `0-9` and `-`, starting with a letter or
/// a digit, so it is one DNS label and cannot reach past the host it sits in
/// front of. The host is the one the `url` crate parsed: lowercase,
/// percent-decoded and IDNA-mapped, as [`plain_refused_host`] reads it. A
/// trailing dot is not ignored here, though: a deny-list has to catch every
/// spelling of a host, and an allow-list need accept only one.
///
/// What comes back is rebuilt from the host, `https://{host}`, so the URL a
/// request is built on is the one that was checked. A stand-in a test put in
/// Azure's place comes back as its origin; see `stand_in_for_azure`, which a
/// release build does not have.
fn azure_base_url(url: &Url) -> Result<String, Refusal> {
    let refused = |problem: String| {
        Refusal::new(
            Reason::BaseUrl,
            format!(
                "{problem}: an azure endpoint's base URL is its resource's, \
                 https://{{resource}}.openai.azure.com or \
                 https://{{resource}}.services.ai.azure.com, and nothing more; the gateway \
                 adds each request's path itself"
            ),
        )
    };
    // Before the stand-in, which is held to it too: the adapter appends
    // `/openai/…` to whatever is here. A path of slashes alone is none, and
    // `normalise_base_url` keeps none of it.
    if !url.path().bytes().all(|b| b == b'/') {
        return Err(refused(format!("the base URL has a path, {}", url.path())));
    }
    if stood_in(url) {
        return Ok(url.origin().ascii_serialization());
    }
    if url.scheme() != "https" {
        return Err(refused(format!(
            "the base URL is {}, not https",
            url.scheme()
        )));
    }
    let host = url.host_str().unwrap_or_default();
    let resource = match url.host() {
        Some(Host::Domain(domain)) => AZURE_HOSTS
            .into_iter()
            .find_map(|azure| domain.strip_suffix(azure)?.strip_suffix('.')),
        // An address names no resource.
        _ => None,
    };
    if !resource.is_some_and(is_azure_resource) {
        return Err(refused(format!("{host} is not an Azure resource's host")));
    }
    // The parser drops 443, https's own, so a port left here is another.
    if let Some(port) = url.port() {
        return Err(refused(format!("the base URL names port {port}")));
    }
    Ok(format!("https://{host}"))
}

/// Whether `label` may be an Azure resource's name in front of one of
/// [`AZURE_HOSTS`]: `^[a-z0-9][a-z0-9-]{1,62}$`, one DNS label of 2 to 63.
fn is_azure_resource(label: &str) -> bool {
    let mut bytes = label.bytes();
    (2..=63).contains(&label.len())
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The origins tests have put in an Azure resource's place.
///
/// Only `stand_in_for_azure` adds one, and a build without `test-fixtures`
/// does not have it, so in a release build this is empty for the life of the
/// process and [`azure_base_url`] accepts an Azure resource and nothing else.
static AZURE_STAND_INS: Mutex<Vec<url::Origin>> = Mutex::new(Vec::new());

/// Whether a test put `url`'s origin in an Azure resource's place.
fn stood_in(url: &Url) -> bool {
    AZURE_STAND_INS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&url.origin())
}

/// TEST-ONLY: lets `base_url`, where a test's mock server listens on this
/// machine, be an `azure` endpoint's base URL for the rest of the process.
///
/// No test can stand up a host under `azure.com`, so without this no test
/// could load an azure row, and none could send one a request through the
/// gateway. It admits the origin given (scheme, address and port) and no
/// other, only on a loopback address, and it waives only the scheme and host
/// [`endpoint_base_url`] asks of an azure endpoint: the stand-in still has no
/// path, query, fragment or credentials.
///
/// Compiled for this crate's own tests and under the `test-fixtures` feature,
/// which only test builds turn on: `oag-server` asks for it among its
/// dev-dependencies, and a dev-dependency's features stay out of every build
/// that is not a test's. A release binary has no such function, so nothing in
/// it can add a stand-in. That is why this is a feature and not a setting: a
/// setting ships in every binary, and one line of YAML or one environment
/// variable would reopen the surface the azure rule exists to close.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn stand_in_for_azure(base_url: &str) -> Result<(), String> {
    let url = Url::parse(base_url).map_err(|e| format!("{base_url} is not a URL: {e}"))?;
    let loopback = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if !loopback {
        return Err(format!(
            "{base_url} is not on a loopback address: only a mock on this machine may stand \
             in for Azure"
        ));
    }
    AZURE_STAND_INS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(url.origin());
    Ok(())
}

/// Whether `value` may be a region or a project: 1 to 63 of `a-z`, `0-9` and
/// `-`.
///
/// A platform puts both into a hostname or a URL path
/// (`bedrock-runtime.{region}.amazonaws.com`, `projects/{project}/…`), so
/// they may hold what a DNS label may and nothing that could end the label,
/// the host or the path segment they land in.
#[must_use]
pub fn is_location(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `value` is shaped like an AWS region, as `^[a-z]{2}(-[a-z]+)+-\d$`
/// says: `us-east-1`, `eu-central-2`, `us-gov-west-1`.
///
/// Stricter than [`is_location`], which an aws endpoint's region passes too.
/// The region is the host a request goes to
/// (`bedrock-runtime.{region}.amazonaws.com`) and the scope its signature is
/// made for, and AWS answers a signature scoped to something that is not a
/// region as it answers a bad key. A Google-shaped `us-central1` would be
/// loaded, sent, and fail on every credential the endpoint has, with an error
/// that points at the credentials.
#[must_use]
pub fn is_aws_region(value: &str) -> bool {
    let lower = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase());
    let parts: Vec<&str> = value.split('-').collect();
    let [area, words @ .., number] = parts.as_slice() else {
        return false;
    };
    area.len() == 2
        && lower(area)
        && !words.is_empty()
        && words.iter().copied().all(lower)
        && number.len() == 1
        && number.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `value` may be a System One endpoint's path: `/`, then at most 127
/// of letters, digits, `/`, `.`, `_` and `-`, in segments that are neither
/// empty, `.` nor `..`.
///
/// The characters are migration 0021's `endpoint_path_check`. The segments are
/// this function's own. A path is appended to the base URL as written, and a
/// URL parser resolves `.` and `..` before the request is sent, so a path
/// holding one would post somewhere other than where it reads. An empty
/// segment is a `//` or a trailing `/`, which most hosts tolerate and some do
/// not: the reason a base URL loses its trailing slash.
#[must_use]
pub fn is_path(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('/') else {
        return false;
    };
    rest.len() <= 127
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
        && rest
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

/// Whether `value` may be an azure endpoint's API version: a date, then
/// `-preview` or nothing, as `^\d{4}-\d{2}-\d{2}(-preview)?$` says
/// (`2024-10-21`, `2025-04-01-preview`), which is how Azure names the versions
/// of its deployments API.
///
/// It becomes the `api-version` of every request the endpoint is sent, so it
/// holds nothing that could end that parameter or begin another. Whether the
/// date is a version Azure has published is Azure's to say.
#[must_use]
pub fn is_api_version(value: &str) -> bool {
    let date = value.strip_suffix("-preview").unwrap_or(value);
    date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// An endpoint row's columns, as stored: the store's `EndpointRow` lends
/// them, and a writer can pass what it is about to insert.
#[derive(Debug, Clone, Copy)]
pub struct Columns<'a> {
    pub name: &'a str,
    pub dialect: &'a str,
    pub platform: &'a str,
    pub base_url: Option<&'a str>,
    pub auth: &'a str,
    pub region: Option<&'a str>,
    pub project: Option<&'a str>,
    pub api_version: Option<&'a str>,
    /// Where a `system_one` endpoint takes a question set; see [`is_path`].
    /// No other dialect takes one.
    pub path: Option<&'a str>,
    /// Must be a JSON object whose values are strings.
    pub extra_headers: &'a serde_json::Value,
}

/// An endpoint row that passed every rule here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConfig {
    pub endpoint: Endpoint,
    /// Normalised by [`endpoint_base_url`]. `None` only on aws and gcp, whose
    /// host a platform builds from the region.
    pub base_url: Option<String>,
    pub auth: AuthStyle,
    pub region: Option<String>,
    pub project: Option<String>,
    /// On an azure endpoint, the version of Azure's deployments API its
    /// requests ask for, checked by [`is_api_version`]; `None` there is
    /// Azure's v1 API. Passed through as stored on every other platform,
    /// where nothing reads it yet.
    pub api_version: Option<String>,
    /// Checked by [`is_path`], and only ever set on a System One endpoint.
    /// `None` there is Jev's own path, which whoever builds the upstream
    /// supplies: this crate does not know it.
    pub path: Option<String>,
    /// Header names and values, in the stored order. Only their being strings
    /// is checked here; whether each can be a header, and is one an endpoint
    /// may add, is checked where it becomes one
    /// (`oag_upstream::custom::EndpointSpec::new`).
    pub extra_headers: Vec<(String, String)>,
}

impl EndpointConfig {
    /// The endpoint `columns` describe, or the first rule they break.
    ///
    /// A refusal's message never quotes an extra header's value: headers are
    /// not for secrets, which is no reason to print one that was put there.
    ///
    /// The [`Endpoint`] is made last, once every rule has passed: making one
    /// keeps its name for the life of the process (see [`Endpoint::new`]), so
    /// a row refused for any rule must not have made one on the way.
    pub fn from_columns(columns: &Columns<'_>) -> Result<Self, Refusal> {
        let checked = CheckedColumns::from_columns(columns)?;
        let endpoint = Endpoint::new(columns.name, checked.dialect, checked.platform)
            .map_err(|m| Refusal::new(Reason::Name, m))?;
        Ok(Self {
            endpoint,
            base_url: checked.base_url,
            auth: checked.auth,
            region: checked.region,
            project: checked.project,
            api_version: checked.api_version,
            path: checked.path,
            extra_headers: checked.extra_headers,
        })
    }
}

/// An endpoint row's columns that passed every rule here: an
/// [`EndpointConfig`] but for its [`Endpoint`], whose name is still the row's
/// own text.
///
/// What a writer checks a row with. Making an `Endpoint` interns its name for
/// the life of the process, and a row a writer is about to store can still be
/// refused after these rules, by its headers, by what its base URL resolves
/// to, or by the database. Each refusal would leave a name behind, and a
/// caller sending a fresh name with every refused write could grow the
/// process without bound. Only a row the gateway serves needs an `Endpoint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedColumns {
    pub dialect: Dialect,
    pub platform: Platform,
    /// As [`EndpointConfig::base_url`].
    pub base_url: Option<String>,
    pub auth: AuthStyle,
    pub region: Option<String>,
    pub project: Option<String>,
    pub api_version: Option<String>,
    pub path: Option<String>,
    pub extra_headers: Vec<(String, String)>,
}

impl CheckedColumns {
    /// `columns`, if they pass every rule [`EndpointConfig::from_columns`]
    /// applies, which is every one but the interning; or the first they
    /// break.
    pub fn from_columns(columns: &Columns<'_>) -> Result<Self, Refusal> {
        let dialect = Dialect::from_endpoint_column(columns.dialect)
            .map_err(|m| Refusal::new(Reason::Dialect, m))?;
        let platform: Platform = columns
            .platform
            .parse()
            .map_err(|m| Refusal::new(Reason::Platform, m))?;
        if !platform.speaks(dialect) {
            return Err(Refusal::new(
                Reason::Platform,
                format!(
                    "the {} platform does not serve the {} dialect",
                    platform.as_str(),
                    columns.dialect
                ),
            ));
        }
        Endpoint::validate_name(columns.name).map_err(|m| Refusal::new(Reason::Name, m))?;
        let auth: AuthStyle = columns
            .auth
            .parse()
            .map_err(|m| Refusal::new(Reason::Auth, m))?;
        if !auth.suits(platform) {
            return Err(Refusal::new(
                Reason::Auth,
                format!(
                    "auth `{}` is not how the {} platform takes a key",
                    auth.as_str(),
                    platform.as_str()
                ),
            ));
        }
        // The platforms that build their host from the region.
        let regional = matches!(platform, Platform::Aws | Platform::Gcp);
        let base_url = match columns.base_url {
            Some(raw) => Some(endpoint_base_url(raw, platform)?),
            None if regional => None,
            None => {
                return Err(Refusal::new(
                    Reason::BaseUrl,
                    format!(
                        "an endpoint on the {} platform needs a base URL",
                        platform.as_str()
                    ),
                ));
            }
        };
        let region = location(Reason::Region, columns.region, regional, platform)?;
        if platform == Platform::Aws
            && let Some(region) = region.as_deref()
            && !is_aws_region(region)
        {
            return Err(Refusal::new(
                Reason::Region,
                format!(
                    "region {region:?} is not an AWS region: one is shaped like us-east-1, \
                     two letters, then words, then one digit, each after a `-`"
                ),
            ));
        }
        let project = location(
            Reason::Project,
            columns.project,
            platform == Platform::Gcp,
            platform,
        )?;
        let path = path(columns.path, dialect, columns.dialect)?;
        let api_version = api_version(columns.api_version, platform)?;
        Ok(Self {
            dialect,
            platform,
            base_url,
            auth,
            region,
            project,
            api_version,
            path,
            extra_headers: header_pairs(columns.extra_headers)?,
        })
    }
}

/// A path, if the row may have the one it names: a System One endpoint's, and
/// one [`is_path`] accepts. `column` is the dialect as stored, for the message.
fn path(value: Option<&str>, dialect: Dialect, column: &str) -> Result<Option<String>, Refusal> {
    let Some(value) = value else {
        return Ok(None);
    };
    if dialect != Dialect::SystemOne {
        return Err(Refusal::new(
            Reason::Path,
            format!(
                "only a system_one endpoint takes a path; the {column} dialect builds its own \
                 from each request"
            ),
        ));
    }
    if !is_path(value) {
        return Err(Refusal::new(
            Reason::Path,
            format!(
                "path {value:?} must be `/` and then at most 127 of letters, digits, `/`, `.`, \
                 `_` and `-`, in segments that are neither empty, `.` nor `..`"
            ),
        ));
    }
    Ok(Some(value.to_owned()))
}

/// An API version, if the row may have the one it names: on an azure
/// endpoint, one [`is_api_version`] accepts, because there it picks Azure's
/// deployments API and becomes every request's `api-version`. Any other
/// platform's is passed through as stored: nothing there reads it yet.
fn api_version(value: Option<&str>, platform: Platform) -> Result<Option<String>, Refusal> {
    match value {
        Some(value) if platform == Platform::Azure && !is_api_version(value) => Err(Refusal::new(
            Reason::ApiVersion,
            format!(
                "api_version {value:?} must be a date, YYYY-MM-DD, with -preview after it \
                     or not, as Azure names its API versions; leave it unset for Azure's v1 API"
            ),
        )),
        value => Ok(value.map(str::to_owned)),
    }
}

/// A region or a project, checked by [`is_location`] wherever it is given and
/// required where `required` says the platform builds a URL from it.
fn location(
    reason: Reason,
    value: Option<&str>,
    required: bool,
    platform: Platform,
) -> Result<Option<String>, Refusal> {
    let what = reason.as_str();
    match value {
        Some(value) if is_location(value) => Ok(Some(value.to_owned())),
        Some(value) => Err(Refusal::new(
            reason,
            format!("{what} {value:?} must be 1 to 63 characters from a-z, 0-9 and `-`"),
        )),
        None if required => Err(Refusal::new(
            reason,
            format!(
                "an endpoint on the {} platform needs a {what}",
                platform.as_str()
            ),
        )),
        None => Ok(None),
    }
}

/// `extra_headers` as name and value pairs, if it is an object of strings.
fn header_pairs(value: &serde_json::Value) -> Result<Vec<(String, String)>, Refusal> {
    let Some(object) = value.as_object() else {
        return Err(Refusal::new(
            Reason::Headers,
            "extra_headers is not a JSON object",
        ));
    };
    object
        .iter()
        .map(|(name, value)| match value.as_str() {
            Some(value) => Ok((name.clone(), value.to_owned())),
            None => Err(Refusal::new(
                Reason::Headers,
                format!("extra header `{name}` is not a string"),
            )),
        })
        .collect()
}

/// The words of a `Config` error, without the `configuration: ` its `Display`
/// puts in front. Every error [`endpoint_base_url`] relays is one.
fn words(e: crate::Error) -> String {
    match e {
        crate::Error::Config(message) => message,
        other => other.to_string(),
    }
}

/// Why an endpoint row is not served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: Reason,
    /// For the operator: which rule, and the value that broke it, unless the
    /// value is an extra header's.
    pub message: String,
}

impl Refusal {
    #[must_use]
    pub fn new(reason: Reason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The rule a row broke, as the `reason` label of the gateway's
/// `oag_endpoint_invalid_total`. A closed set, so the metric's label values
/// are one too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// Malformed, or taken by a built-in provider, an alias, `oag` or `codex`.
    Name,
    /// Unknown, or known and not served by this build.
    Dialect,
    /// Unknown, or not one that speaks the row's dialect.
    Platform,
    /// Unknown, or not how the row's platform takes a key.
    Auth,
    /// Missing where the platform needs one, or not a base URL.
    BaseUrl,
    /// A host a plain endpoint may not point at: [`PLAIN_REFUSED_HOSTS`].
    Compliance,
    Region,
    Project,
    /// On a dialect that takes none, or not one [`is_path`] accepts.
    Path,
    /// On an azure endpoint, not one [`is_api_version`] accepts.
    ApiVersion,
    /// Not an object of strings, or a header an endpoint may not add.
    Headers,
    /// A valid row this build has no adapter for.
    Unsupported,
}

impl Reason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Dialect => "dialect",
            Self::Platform => "platform",
            Self::Auth => "auth",
            Self::BaseUrl => "base_url",
            Self::Compliance => "compliance",
            Self::Region => "region",
            Self::Project => "project",
            Self::Path => "path",
            Self::ApiVersion => "api_version",
            Self::Headers => "headers",
            Self::Unsupported => "unsupported",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// U5. A configured base URL is normalised once, or refused.
    ///
    /// Every adapter builds its request URL by concatenation, so a trailing
    /// slash produced `https://host//v1/messages` — which most upstreams
    /// tolerate and some do not. That is the worst kind of configuration bug:
    /// it works in the deployment where it was typed.
    #[test]
    fn a_base_url_is_trimmed_or_refused_at_startup() {
        for raw in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com///",
            "  https://api.anthropic.com/  ",
        ] {
            assert_eq!(
                normalise_base_url("anthropic", raw).expect("normalises"),
                "https://api.anthropic.com",
                "every spelling of the same endpoint has to reach the adapter \
                 identically: {raw}"
            );
        }

        // A path is legitimate and kept: Gemini's own default carries one.
        assert_eq!(
            normalise_base_url(
                "gemini",
                "https://generativelanguage.googleapis.com/v1beta/"
            )
            .expect("normalises"),
            "https://generativelanguage.googleapis.com/v1beta"
        );

        // A query or fragment cannot be normalised away. Appending a path after
        // either means something else entirely, and guessing which half the
        // operator meant is worse than saying so.
        for raw in ["https://host/?apikey=secret", "https://host/#anchor"] {
            let err = normalise_base_url("openai", raw).expect_err("refused");
            assert!(
                err.to_string().contains(raw.trim()),
                "the operator has to see which value was rejected: {err}"
            );
        }

        // And nonsense is refused at startup rather than becoming a 404 from an
        // upstream that never received the request.
        assert!(normalise_base_url("openai", "api.openai.com").is_err());
        assert!(normalise_base_url("openai", "   ").is_err());
    }

    const NO_HEADERS: &serde_json::Value = &serde_json::Value::Null;

    /// A row that passes: a plain OpenAI-dialect host on the operator's own
    /// network, with one header.
    fn plain(headers: &serde_json::Value) -> Columns<'_> {
        Columns {
            name: "t4-core-plain",
            dialect: "openai",
            platform: "plain",
            base_url: Some("http://10.0.0.7:8000/v1/"),
            auth: "bearer",
            region: None,
            project: None,
            api_version: None,
            path: None,
            extra_headers: headers,
        }
    }

    #[test]
    fn a_good_row_on_each_platform_is_read_whole() {
        let headers = json!({"HTTP-Referer": "https://oag.example", "X-Title": "oag"});
        let config = EndpointConfig::from_columns(&plain(&headers)).expect("a good row");
        assert_eq!(config.endpoint.name(), "t4-core-plain");
        assert_eq!(config.endpoint.dialect(), Dialect::OpenAIChatCompletions);
        assert_eq!(config.endpoint.platform(), Platform::Plain);
        assert_eq!(
            config.base_url.as_deref(),
            Some("http://10.0.0.7:8000/v1"),
            "normalised as a built-in's is"
        );
        assert_eq!(config.auth, AuthStyle::Bearer);
        assert_eq!(config.path, None);
        assert_eq!(
            config.extra_headers,
            [
                ("HTTP-Referer".to_owned(), "https://oag.example".to_owned()),
                ("X-Title".to_owned(), "oag".to_owned()),
            ],
            "every header, in the stored order"
        );

        let aws = EndpointConfig::from_columns(&Columns {
            name: "t4-core-aws",
            dialect: "anthropic",
            platform: "aws",
            base_url: None,
            auth: "none",
            region: Some("eu-west-1"),
            ..plain(&json!({}))
        })
        .expect("an aws row needs no base URL");
        assert_eq!(aws.base_url, None);
        assert_eq!(aws.region.as_deref(), Some("eu-west-1"));
        assert_eq!(aws.extra_headers, []);

        // Converse, for the models Bedrock serves that are not Claude, at a
        // VPC endpoint rather than the regional host.
        let converse = EndpointConfig::from_columns(&Columns {
            name: "t9-core-converse",
            dialect: "bedrock_converse",
            platform: "aws",
            base_url: Some("https://vpce-0a1b.bedrock-runtime.us-gov-west-1.vpce.amazonaws.com/"),
            auth: "none",
            region: Some("us-gov-west-1"),
            ..plain(&json!({}))
        })
        .expect("a converse row");
        assert_eq!(converse.endpoint.dialect(), Dialect::BedrockConverse);
        assert_eq!(converse.endpoint.platform(), Platform::Aws);
        assert_eq!(converse.region.as_deref(), Some("us-gov-west-1"));
        assert_eq!(
            converse.base_url.as_deref(),
            Some("https://vpce-0a1b.bedrock-runtime.us-gov-west-1.vpce.amazonaws.com"),
            "an aws row may name amazonaws.com, which a plain one may not"
        );

        let gcp = EndpointConfig::from_columns(&Columns {
            name: "t4-core-gcp",
            dialect: "gemini",
            platform: "gcp",
            base_url: None,
            auth: "bearer",
            region: Some("global"),
            project: Some("my-project-1"),
            ..plain(&json!({}))
        })
        .expect("a gcp row");
        assert_eq!(
            (gcp.region.as_deref(), gcp.project.as_deref()),
            (Some("global"), Some("my-project-1"))
        );

        let azure = EndpointConfig::from_columns(&Columns {
            name: "t4-core-azure",
            platform: "azure",
            base_url: Some("https://res.openai.azure.com"),
            auth: "api_key_header",
            api_version: Some("2024-10-21"),
            ..plain(&json!({}))
        })
        .expect("an azure row may name azure.com, which a plain one may not");
        assert_eq!(azure.api_version.as_deref(), Some("2024-10-21"));
        assert_eq!(
            azure.base_url.as_deref(),
            Some("https://res.openai.azure.com")
        );

        // Merge Gateway's Decisions API: System One's wire shape at a path of
        // its own.
        let decisions = EndpointConfig::from_columns(&Columns {
            name: "t7-core-decisions",
            dialect: "system_one",
            base_url: Some("https://api-gateway.merge.dev"),
            path: Some("/v1/decisions"),
            ..plain(&json!({}))
        })
        .expect("a System One row with a path");
        assert_eq!(decisions.endpoint.dialect(), Dialect::SystemOne);
        assert_eq!(decisions.path.as_deref(), Some("/v1/decisions"));
        let jev_shaped = EndpointConfig::from_columns(&Columns {
            name: "t7-core-jev",
            dialect: "system_one",
            ..plain(&json!({}))
        })
        .expect("a System One row without one");
        assert_eq!(
            jev_shaped.path, None,
            "the default is the upstream's to supply"
        );
    }

    // Long because it is a table: one row per rule, each a whole set of
    // columns, and split across functions it would stop being one list.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn each_rule_refuses_a_row_with_its_own_reason() {
        let object = json!({});
        let base = plain(&object);
        let headers_not_object = json!(["x"]);
        let header_not_string = json!({"x-team": 7, "x-ok": "fine"});
        let cases: [(Columns<'_>, Reason, &str); 21] = [
            (
                Columns {
                    name: "openai",
                    ..base
                },
                Reason::Name,
                "reserved",
            ),
            (
                Columns {
                    name: "Bad Name",
                    ..base
                },
                Reason::Name,
                "must be",
            ),
            (
                Columns {
                    dialect: "klingon",
                    ..base
                },
                Reason::Dialect,
                "unknown dialect `klingon`",
            ),
            (
                Columns {
                    dialect: "bedrock_converse",
                    platform: "aws",
                    base_url: None,
                    auth: "none",
                    region: Some("us-central1"),
                    ..base
                },
                Reason::Region,
                "region \"us-central1\" is not an AWS region",
            ),
            (
                Columns {
                    platform: "vertex",
                    ..base
                },
                Reason::Platform,
                "unknown platform `vertex`",
            ),
            (
                Columns {
                    dialect: "anthropic",
                    platform: "azure",
                    auth: "api_key_header",
                    ..base
                },
                Reason::Platform,
                "the azure platform does not serve the anthropic dialect",
            ),
            (
                Columns {
                    auth: "basic",
                    ..base
                },
                Reason::Auth,
                "unknown auth style `basic`",
            ),
            (
                Columns {
                    platform: "azure",
                    base_url: Some("https://res.openai.azure.com"),
                    auth: "bearer",
                    ..base
                },
                Reason::Auth,
                "auth `bearer` is not how the azure platform takes a key",
            ),
            (
                Columns {
                    base_url: None,
                    ..base
                },
                Reason::BaseUrl,
                "the plain platform needs a base URL",
            ),
            (
                Columns {
                    base_url: Some("http://169.254.169.254/latest"),
                    ..base
                },
                Reason::BaseUrl,
                "link-local or cloud-metadata",
            ),
            (
                Columns {
                    base_url: Some("https://h.example/v1?key=1"),
                    ..base
                },
                Reason::BaseUrl,
                "contains '?'",
            ),
            (
                Columns {
                    base_url: Some("https://API.Anthropic.com./v1"),
                    ..base
                },
                Reason::Compliance,
                "anthropic.com",
            ),
            (
                Columns {
                    region: Some("US_EAST_1"),
                    ..base
                },
                Reason::Region,
                "region \"US_EAST_1\" must be",
            ),
            (
                Columns {
                    dialect: "anthropic",
                    platform: "aws",
                    base_url: None,
                    auth: "none",
                    ..base
                },
                Reason::Region,
                "the aws platform needs a region",
            ),
            (
                Columns {
                    dialect: "gemini",
                    platform: "gcp",
                    base_url: None,
                    region: Some("us-central1"),
                    ..base
                },
                Reason::Project,
                "the gcp platform needs a project",
            ),
            (
                Columns {
                    path: Some("/v1/chat/completions"),
                    ..base
                },
                Reason::Path,
                "only a system_one endpoint takes a path; the openai dialect",
            ),
            (
                Columns {
                    dialect: "system_one",
                    path: Some("/v1/../admin"),
                    ..base
                },
                Reason::Path,
                "path \"/v1/../admin\" must be",
            ),
            (
                Columns {
                    extra_headers: &headers_not_object,
                    ..base
                },
                Reason::Headers,
                "not a JSON object",
            ),
            (
                Columns {
                    extra_headers: &header_not_string,
                    ..base
                },
                Reason::Headers,
                "extra header `x-team` is not a string",
            ),
            (
                Columns {
                    platform: "azure",
                    base_url: Some("https://res.openai.azure.com/openai/v1"),
                    auth: "api_key_header",
                    ..base
                },
                Reason::BaseUrl,
                "the base URL has a path, /openai/v1",
            ),
            (
                Columns {
                    platform: "azure",
                    base_url: Some("https://res.openai.azure.com"),
                    auth: "api_key_header",
                    api_version: Some("v1"),
                    ..base
                },
                Reason::ApiVersion,
                "api_version \"v1\" must be a date",
            ),
        ];
        for (columns, reason, says) in cases {
            let refused = EndpointConfig::from_columns(&columns).expect_err(says);
            assert_eq!(refused.reason, reason, "{says}: {refused}");
            assert!(refused.message.contains(says), "{says}: {refused}");
            assert!(!refused.message.starts_with("configuration: "), "{refused}");
            assert_eq!(refused.to_string(), refused.message);
        }
    }

    #[test]
    fn a_refusal_never_prints_a_header_value() {
        let headers = json!({"x-team": ["secret-looking-value"]});
        let refused = EndpointConfig::from_columns(&plain(&headers)).expect_err("not a string");
        assert!(
            !refused.message.contains("secret-looking-value"),
            "{refused}"
        );
        assert!(
            EndpointConfig::from_columns(&plain(NO_HEADERS)).is_err(),
            "null is not an object either"
        );
    }

    /// Every spelling of a refused host that names the same host.
    #[test]
    fn the_compliance_guard_refuses_every_spelling_of_a_refused_host() {
        for (raw, refused) in [
            ("https://api.anthropic.com", "anthropic.com"),
            ("https://anthropic.com/v1", "anthropic.com"),
            ("https://console.anthropic.com", "anthropic.com"),
            ("https://claude.ai", "claude.ai"),
            ("https://api.claude.com/", "claude.com"),
            ("https://chatgpt.com/backend-api/codex", "chatgpt.com"),
            ("https://api.openai.com/v1", "openai.com"),
            ("https://auth.openai.com", "openai.com"),
            ("https://api.x.ai/v1", "x.ai"),
            ("https://grok.com", "grok.com"),
            (
                "https://generativelanguage.googleapis.com/v1beta",
                "googleapis.com",
            ),
            (
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                "amazonaws.com",
            ),
            ("https://res.openai.azure.com", "azure.com"),
            // Case, a trailing dot or several, a port, percent-encoding and a
            // full-width dot all name the same host.
            ("https://API.OpenAI.COM/v1", "openai.com"),
            ("https://api.openai.com./v1", "openai.com"),
            ("https://api.openai.com../v1", "openai.com"),
            ("https://api.openai.com:8443/v1", "openai.com"),
            ("https://api%2Eopenai%2Ecom/v1", "openai.com"),
            ("https://api.openai\u{3002}com/v1", "openai.com"),
            ("http://API.X.AI:80", "x.ai"),
        ] {
            let url = catalog_url(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(
                plain_refused_host(&url, Platform::Plain),
                Some(refused),
                "{raw}"
            );
            let err = endpoint_base_url(raw, Platform::Plain).expect_err(raw);
            assert_eq!(err.reason, Reason::Compliance, "{raw}: {err}");
            assert!(err.message.contains(refused), "{raw}: {err}");
        }
    }

    #[test]
    fn the_compliance_guard_passes_lookalikes_addresses_and_other_platforms() {
        // A suffix only counts at a label boundary.
        for raw in [
            "https://notopenai.com/v1",
            "https://openai.com.proxy.example/v1",
            "https://max.ai",
            "https://anthropic.co",
            "https://grok.com-mirror.example",
            "https://api.groq.com/openai/v1",
            // Addresses have no name to compare. Loopback and private ones are
            // where a local model server runs.
            "http://127.0.0.1:8000/v1",
            "http://[::1]:8000",
            "http://10.0.0.5",
            "http://localhost:11434/v1",
        ] {
            let url = catalog_url(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(plain_refused_host(&url, Platform::Plain), None, "{raw}");
            endpoint_base_url(raw, Platform::Plain).unwrap_or_else(|e| panic!("{raw}: {e}"));
        }
        // The clouds' hosts are refused to plain endpoints only.
        for (raw, platform) in [
            ("https://res.openai.azure.com", Platform::Azure),
            (
                "https://bedrock-runtime.eu-west-1.amazonaws.com",
                Platform::Aws,
            ),
            (
                "https://us-central1-aiplatform.googleapis.com",
                Platform::Gcp,
            ),
        ] {
            let url = catalog_url(raw).expect("a URL");
            assert_eq!(plain_refused_host(&url, platform), None, "{raw}");
            assert_eq!(
                endpoint_base_url(raw, platform).as_deref(),
                Ok(raw),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_base_url_is_refused_before_it_is_normalised() {
        for (raw, says) in [
            ("ftp://h.example", "http or https"),
            ("https://user:pw@h.example", "credentials"),
            ("http://metadata.google.internal", "metadata"),
            ("", "empty"),
            ("https://h.example/#frag", "contains '#'"),
        ] {
            let err = endpoint_base_url(raw, Platform::Plain).expect_err(raw);
            assert_eq!(err.reason, Reason::BaseUrl, "{raw}: {err}");
            assert!(err.message.contains(says), "{raw}: {err}");
        }
        assert_eq!(
            endpoint_base_url(" http://127.0.0.1:8000/v1// ", Platform::Plain).as_deref(),
            Ok("http://127.0.0.1:8000/v1")
        );
    }

    /// An azure endpoint's base URL is its resource's, spelt however the URL
    /// parser reads as that one URL, and it comes back as that URL.
    #[test]
    fn an_azure_base_url_is_a_resource_and_nothing_more() {
        let longest = format!("https://{}.openai.azure.com", "a".repeat(63));
        for (raw, normalised) in [
            (
                "https://res.openai.azure.com",
                "https://res.openai.azure.com",
            ),
            (
                "https://res.openai.azure.com/",
                "https://res.openai.azure.com",
            ),
            (
                "  https://RES.OpenAI.Azure.COM///  ",
                "https://res.openai.azure.com",
            ),
            (
                "https://res.openai.azure.com:443",
                "https://res.openai.azure.com",
            ),
            (
                "https://my-res-01.services.ai.azure.com",
                "https://my-res-01.services.ai.azure.com",
            ),
            ("https://ab.openai.azure.com", "https://ab.openai.azure.com"),
            ("https://0a.openai.azure.com", "https://0a.openai.azure.com"),
            ("https://a-.openai.azure.com", "https://a-.openai.azure.com"),
            (longest.as_str(), longest.as_str()),
        ] {
            assert_eq!(
                endpoint_base_url(raw, Platform::Azure).as_deref(),
                Ok(normalised),
                "{raw}"
            );
        }
    }

    /// Everything else, each refused as a base URL and for what it says: a
    /// host that is not a resource's, an address, plain http, a path, a port,
    /// and whatever any endpoint is refused for.
    #[test]
    fn an_azure_base_url_that_is_not_a_resource_is_refused() {
        let too_long = format!("https://{}.openai.azure.com", "a".repeat(64));
        let not_a_resource = "is not an Azure resource's host";
        for (raw, says) in [
            (
                "http://res.openai.azure.com",
                "the base URL is http, not https",
            ),
            ("http://127.0.0.1:8000", "the base URL is http, not https"),
            ("https://res.openai.azure.com/openai", "has a path, /openai"),
            (
                "https://res.openai.azure.com/openai/v1/",
                "has a path, /openai/v1/",
            ),
            (
                "https://res.openai.azure.com/openai/deployments/gpt",
                "has a path, /openai/deployments/gpt",
            ),
            ("https://res.openai.azure.com:8443", "names port 8443"),
            ("https://res.openai.azure.com:80", "names port 80"),
            ("https://openai.azure.com", not_a_resource),
            ("https://services.ai.azure.com", not_a_resource),
            ("https://.openai.azure.com", not_a_resource),
            ("https://a.openai.azure.com", not_a_resource),
            (too_long.as_str(), not_a_resource),
            ("https://-res.openai.azure.com", not_a_resource),
            ("https://res_1.openai.azure.com", not_a_resource),
            ("https://a.b.openai.azure.com", not_a_resource),
            ("https://resopenai.azure.com", not_a_resource),
            ("https://res.openai.azure.com.", not_a_resource),
            ("https://res.openai.azure.com.evil.example", not_a_resource),
            ("https://res.cognitiveservices.azure.com", not_a_resource),
            ("https://res.ai.azure.com", not_a_resource),
            ("https://res.azure.com", not_a_resource),
            ("https://evil.example", not_a_resource),
            ("https://localhost", not_a_resource),
            ("https://127.0.0.1", not_a_resource),
            ("https://10.0.0.5", not_a_resource),
            ("https://[::1]", not_a_resource),
            ("https://[2001:db8::1]:443", not_a_resource),
            // The rules every endpoint is held to come first.
            ("https://169.254.169.254", "link-local or cloud-metadata"),
            ("https://user:pw@res.openai.azure.com", "credentials"),
            (
                "https://res.openai.azure.com/?api-version=2024-10-21",
                "contains '?'",
            ),
            ("https://res.openai.azure.com#x", "contains '#'"),
            ("ftp://res.openai.azure.com", "http or https"),
        ] {
            let err = endpoint_base_url(raw, Platform::Azure).expect_err(raw);
            assert_eq!(err.reason, Reason::BaseUrl, "{raw}: {err}");
            assert!(err.message.contains(says), "{raw}: {err}");
        }
        // The message names the host it read, and what an azure base URL is.
        let err = endpoint_base_url("https://10.0.0.5", Platform::Azure).expect_err("an address");
        assert!(
            err.message
                .starts_with("10.0.0.5 is not an Azure resource's host: "),
            "{err}"
        );
        assert!(
            err.message
                .contains("https://{resource}.openai.azure.com or https://{resource}.services"),
            "{err}"
        );
    }

    #[test]
    fn a_resource_name_is_one_dns_label() {
        let longest = "a".repeat(63);
        for good in ["ab", "a1", "0a", "my-res-01", "a-", longest.as_str()] {
            assert!(is_azure_resource(good), "{good}");
        }
        let too_long = "a".repeat(64);
        for bad in [
            "",
            "a",
            "-a",
            "Res",
            "res_1",
            "res.x",
            "rés",
            "a b",
            too_long.as_str(),
        ] {
            assert!(!is_azure_resource(bad), "{bad:?}");
        }
    }

    /// A stand-in is the origin a test named and no other, and it is held to
    /// the rest of the rule. This is the only test in this crate that adds
    /// one, on ports no other test uses: stand-ins last for the process.
    #[test]
    fn a_stand_in_takes_azure_s_place_only_where_a_test_put_it() {
        let refused = |raw: &str| endpoint_base_url(raw, Platform::Azure).expect_err(raw);
        assert!(refused("http://127.0.0.1:1").message.contains("not https"));

        stand_in_for_azure("http://127.0.0.1:1").expect("a loopback address");
        stand_in_for_azure("http://[::1]:2/").expect("a loopback address");
        for (raw, normalised) in [
            ("http://127.0.0.1:1", "http://127.0.0.1:1"),
            ("http://127.0.0.1:1/", "http://127.0.0.1:1"),
            ("http://[::1]:2", "http://[::1]:2"),
        ] {
            assert_eq!(
                endpoint_base_url(raw, Platform::Azure).as_deref(),
                Ok(normalised),
                "{raw}"
            );
        }
        // The origin, exactly: another port, address or scheme is not it.
        for (raw, says) in [
            ("http://127.0.0.1:3", "not https"),
            ("http://[::1]:1", "not https"),
            ("https://127.0.0.1:1", "is not an Azure resource's host"),
            ("http://localhost:1", "not https"),
            // And a stand-in is a resource's URL in every other way.
            ("http://127.0.0.1:1/openai/v1", "has a path"),
            ("http://127.0.0.1:1/?x=1", "contains '?'"),
            ("http://user:pw@127.0.0.1:1", "credentials"),
        ] {
            let err = refused(raw);
            assert_eq!(err.reason, Reason::BaseUrl, "{raw}: {err}");
            assert!(err.message.contains(says), "{raw}: {err}");
        }

        // Only a loopback address stands in, so a test cannot point an azure
        // endpoint anywhere else either.
        for raw in [
            "http://10.0.0.5:80",
            "http://192.168.1.2:8080",
            "http://localhost:4",
            "https://res.openai.azure.com.evil.example",
            "not a URL",
        ] {
            let err = stand_in_for_azure(raw).expect_err(raw);
            assert!(err.starts_with(raw), "{raw}: {err}");
        }
        assert!(refused("http://10.0.0.5:80").message.contains("not https"));
    }

    #[test]
    fn an_api_version_is_a_date_and_preview_or_not() {
        for good in ["2024-10-21", "2025-04-01-preview", "0000-00-00"] {
            assert!(is_api_version(good), "{good}");
        }
        for bad in [
            "",
            "v1",
            "preview",
            "latest",
            "-preview",
            "2024-10-21-Preview",
            "2024-10-21-preview-preview",
            "2024-10-21-beta",
            "2024-10-21preview",
            "2024-1-021",
            "24-10-21",
            "2024/10/21",
            "20241021",
            "2024-10-210",
            "2024-10-2",
            " 2024-10-21",
            "2024-10-21 ",
            "2024-10-21&x=1",
            "2024-10-21#",
            "2024-10-2\u{FF11}",
            "\u{FF12}024-10-21",
        ] {
            assert!(!is_api_version(bad), "{bad:?}");
        }

        let empty = json!({});
        let azure = |api_version: Option<&'static str>| Columns {
            name: "t8-core-azure",
            platform: "azure",
            base_url: Some("https://res.openai.azure.com"),
            auth: "api_key_header",
            api_version,
            ..plain(&empty)
        };
        let v1 = EndpointConfig::from_columns(&azure(None)).expect("Azure's v1 API");
        assert_eq!(v1.api_version, None);
        let preview = EndpointConfig::from_columns(&azure(Some("2025-04-01-preview")))
            .expect("a preview version");
        assert_eq!(preview.api_version.as_deref(), Some("2025-04-01-preview"));
        let refused = EndpointConfig::from_columns(&azure(Some("2024-10-21&x=1")))
            .expect_err("a second parameter");
        assert_eq!(refused.reason, Reason::ApiVersion);
        assert!(
            refused
                .message
                .contains("leave it unset for Azure's v1 API"),
            "{refused}"
        );

        // Only azure holds an API version to it: nothing else reads one yet.
        let elsewhere = EndpointConfig::from_columns(&Columns {
            api_version: Some("v1"),
            ..plain(&empty)
        })
        .expect("passed through");
        assert_eq!(elsewhere.api_version.as_deref(), Some("v1"));
    }

    #[test]
    fn a_location_is_a_dns_label_and_nothing_more() {
        let longest = "a".repeat(63);
        for good in [
            "us-east-1",
            "global",
            "my-project-123",
            "a",
            longest.as_str(),
        ] {
            assert!(is_location(good), "{good}");
        }
        let too_long = "a".repeat(64);
        for bad in [
            "",
            "US-EAST-1",
            "us_east_1",
            "us.east",
            "us-east-1/../x",
            "us east",
            "eu-west-1\n",
            "ü",
            too_long.as_str(),
        ] {
            assert!(!is_location(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_aws_region_is_shaped_like_one() {
        for good in [
            "us-east-1",
            "eu-central-2",
            "ap-southeast-4",
            "us-gov-west-1",
            "us-isob-east-1",
            "il-central-1",
        ] {
            assert!(is_aws_region(good), "{good}");
        }
        for bad in [
            "",
            "us-central1",
            "europe-west4",
            "global",
            "useast-1",
            "u-east-1",
            "usa-east-1",
            "us-1",
            "us--1",
            "us-east-",
            "us-east-12",
            "us-east-x",
            "US-EAST-1",
            "us-east1-1",
            "us-east-1-",
            "-us-east-1",
            "us-east-\u{661}",
        ] {
            assert!(!is_aws_region(bad), "{bad:?}");
        }

        // Only aws holds a region to it. Google's regions are not shaped so.
        let gcp = EndpointConfig::from_columns(&Columns {
            name: "t9-core-gcp",
            dialect: "gemini",
            platform: "gcp",
            base_url: None,
            auth: "bearer",
            region: Some("us-central1"),
            project: Some("p"),
            ..plain(&json!({}))
        });
        assert!(gcp.is_ok(), "{gcp:?}");
    }

    /// Migration 0021's characters, and segments a URL parser would leave as
    /// written.
    #[test]
    fn a_path_is_segments_a_url_keeps_as_written() {
        let longest = format!("/{}", "a".repeat(127));
        for good in [
            "/v1/systemone",
            "/v1/decisions",
            "/api/v2.1/System_One-x",
            "/a",
            "/..a/b..",
            longest.as_str(),
        ] {
            assert!(is_path(good), "{good}");
        }
        let too_long = format!("/{}", "a".repeat(128));
        for bad in [
            "",
            "/",
            "v1/decisions",
            "/v1/decisions/",
            "/v1//decisions",
            "/v1/./decisions",
            "/v1/../admin",
            "/..",
            "/v1/decisions?x=1",
            "/v1/decisions#top",
            "/v1/déc",
            "/v1/a b",
            "/@evil.example",
            "/v1:8080",
            "/v1\\decisions",
            too_long.as_str(),
        ] {
            assert!(!is_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn every_reason_has_its_own_label() {
        let reasons = [
            Reason::Name,
            Reason::Dialect,
            Reason::Platform,
            Reason::Auth,
            Reason::BaseUrl,
            Reason::Compliance,
            Reason::Region,
            Reason::Project,
            Reason::Path,
            Reason::ApiVersion,
            Reason::Headers,
            Reason::Unsupported,
        ];
        let labels: std::collections::HashSet<&str> = reasons.iter().map(|r| r.as_str()).collect();
        assert_eq!(labels.len(), reasons.len(), "{labels:?}");
        for label in labels {
            assert!(
                label.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{label}"
            );
        }
        assert_eq!(Reason::BaseUrl.as_str(), "base_url");
        assert_eq!(Reason::ApiVersion.as_str(), "api_version");
        assert_eq!(Reason::Unsupported.as_str(), "unsupported");
    }

    /// C14. A row refused for any rule leaves its name out of the interner,
    /// which keeps every name it is given for the life of the process: a
    /// caller sending a fresh name with each refused row would otherwise grow
    /// the process by a name a refusal, for as long as it ran. Only a row that
    /// passes every rule is interned, and checking one interns nothing.
    #[test]
    fn a_refused_row_leaves_no_name_behind() {
        let headers = json!({"X-Title": "t14"});
        let not_strings = json!({"X-Title": 7});
        let good = |name: &'static str| Columns {
            name,
            ..plain(&headers)
        };
        let refused = [
            Columns {
                auth: "nope",
                ..good("t14-refused-auth")
            },
            Columns {
                base_url: Some("http://169.254.169.254/latest"),
                ..good("t14-refused-metadata")
            },
            Columns {
                base_url: Some("https://api.openai.com/v1"),
                ..good("t14-refused-compliance")
            },
            Columns {
                base_url: None,
                ..good("t14-refused-no-base-url")
            },
            Columns {
                region: Some("Not A Region"),
                ..good("t14-refused-region")
            },
            Columns {
                path: Some("/v1/decisions"),
                ..good("t14-refused-path")
            },
            Columns {
                extra_headers: &not_strings,
                ..good("t14-refused-headers")
            },
        ];
        for columns in &refused {
            let refusal = EndpointConfig::from_columns(columns).expect_err(columns.name);
            assert_ne!(refusal.reason, Reason::Name, "{}: {refusal}", columns.name);
            assert!(
                !crate::provider::is_interned(columns.name),
                "{} was refused for its {}, and its name was kept anyway",
                columns.name,
                refusal.reason.as_str()
            );
        }

        let checked = CheckedColumns::from_columns(&good("t14-checked-only")).expect("a good row");
        assert_eq!(checked.platform, Platform::Plain);
        assert!(
            !crate::provider::is_interned("t14-checked-only"),
            "checking a row interns nothing"
        );

        let served = good("t14-served");
        assert!(!crate::provider::is_interned(served.name));
        let config = EndpointConfig::from_columns(&served).expect("a good row");
        assert_eq!(config.endpoint.name(), "t14-served");
        assert!(
            crate::provider::is_interned("t14-served"),
            "a served row is"
        );
    }

    /// C18. A base URL holding a control character is refused, and the
    /// refusal does not repeat it. The parser would drop the tab or newline,
    /// and escape the rest, so the URL checked and the text stored would be
    /// two things, and the stored one would reach a terminal as written. At
    /// either end it is whitespace, and trimmed as it always was.
    #[test]
    fn a_base_url_with_a_control_character_is_refused_without_repeating_it() {
        for raw in [
            "http://10.0.0.7:8000/v1\u{1b}[2J",
            "http://10.0.0.\t7:8000/v1",
            "http://10.0.0.7:8000/v\n1",
            "http://10.0.0.7:8000/\u{7f}v1",
            "http://10.0.0.7:8000/\u{9b}31m",
        ] {
            let err = endpoint_base_url(raw, Platform::Plain).expect_err(raw);
            assert_eq!(err.reason, Reason::BaseUrl, "{raw:?}: {err}");
            assert!(err.message.contains("control character"), "{raw:?}: {err}");
            assert!(
                !err.message.chars().any(char::is_control),
                "{raw:?}: {err:?}"
            );
        }
        assert_eq!(
            endpoint_base_url("\thttp://10.0.0.7:8000/v1\n", Platform::Plain).as_deref(),
            Ok("http://10.0.0.7:8000/v1")
        );
    }

    /// C18. The base URL that comes back is the one the URL parser read,
    /// which is the one every rule judged: what is stored is what was
    /// checked, and every spelling of one URL is stored as one.
    #[test]
    fn a_base_url_comes_back_as_the_url_that_was_checked() {
        for (raw, normalised) in [
            (
                "HTTP://Models.Example.COM:80/V1/",
                "http://models.example.com/V1",
            ),
            (
                "https://models.example.com:443",
                "https://models.example.com",
            ),
            (
                "http://10.0.0.7:8000/my models/",
                "http://10.0.0.7:8000/my%20models",
            ),
            ("http://10.0.0.7:8000/a/../v1", "http://10.0.0.7:8000/v1"),
            ("http://[::1]:11434/", "http://[::1]:11434"),
            (
                "http://bücher.example/v1",
                "http://xn--bcher-kva.example/v1",
            ),
        ] {
            assert_eq!(
                endpoint_base_url(raw, Platform::Plain).as_deref(),
                Ok(normalised),
                "{raw}"
            );
        }
    }
}
