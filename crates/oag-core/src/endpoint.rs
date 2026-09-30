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
//! a region may contain, which auth style a platform takes, or which hosts a
//! plain endpoint may not name.
//!
//! Like the rest of this crate, nothing here does I/O. A base URL is judged by
//! what it says and never by what its name resolves to: a reload that asked DNS
//! would let a resolver decide which endpoints serve. The resolved address is
//! checked where an endpoint is written instead.

use crate::provider::{AuthStyle, Dialect, Endpoint, Platform};
use crate::service::catalog_url;
use std::fmt;
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
/// [`catalog_url`] first: http or https, no credentials in it, and no
/// link-local or metadata literal. Loopback and private addresses pass, so a
/// model server on the operator's own network can be registered. Then the
/// compliance guard, [`plain_refused_host`], and then [`normalise_base_url`]:
/// no query, no fragment, no trailing slash. No DNS; see the module.
pub fn endpoint_base_url(raw: &str, platform: Platform) -> Result<String, Refusal> {
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
    normalise_base_url("this endpoint", raw).map_err(|e| Refusal::new(Reason::BaseUrl, words(e)))
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
    /// Passed through as stored. What it may contain is the Azure platform's
    /// to say, and it has not said yet.
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
        let endpoint = Endpoint::new(columns.name, dialect, platform)
            .map_err(|m| Refusal::new(Reason::Name, m))?;
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
        let project = location(
            Reason::Project,
            columns.project,
            platform == Platform::Gcp,
            platform,
        )?;
        let path = path(columns.path, dialect, columns.dialect)?;
        Ok(Self {
            endpoint,
            base_url,
            auth,
            region,
            project,
            api_version: columns.api_version.map(str::to_owned),
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
        let cases: [(Columns<'_>, Reason, &str); 19] = [
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
                    region: Some("us-east-1"),
                    ..base
                },
                Reason::Dialect,
                "not served by this build",
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
        assert_eq!(Reason::Unsupported.as_str(), "unsupported");
    }
}
