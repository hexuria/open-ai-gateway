//! Registering an endpoint: the rules a write must pass, how a stored one is
//! shown, and asking one which models it lists.
//!
//! Shared by `oag admin endpoint` and `/admin/api/endpoints`, so the two refuse
//! the same rows for the same reasons. A write passes every rule the gateway's
//! reload applies to a row ([`EndpointConfig::from_columns`], then the header
//! names [`EndpointSpec::new`] checks) and one the reload cannot: what the base
//! URL's name resolves to now ([`crate::egress`]). So a row either writer
//! stores is one the next reload serves, unless this build has no adapter for
//! its platform or dialect. That is no reason to refuse the write, because the
//! row is right and a later build serves it, so it is said instead: [`refusal`]
//! asks one row the reload's own question.

use crate::egress::{deny_resolved_target, validate_endpoint_base_url};
use oag_core::endpoint::{Columns, EndpointConfig, Reason, Refusal};
use oag_core::provider::{AuthStyle, Platform};
use oag_store::repo::{self, EndpointUpdate, NewEndpoint};
use oag_store::{Db, EndpointRow};
use oag_upstream::custom::EndpointSpec;
use oag_upstream::gcp_token::{DEFAULT_TOKEN_URL, GcpTokenCache};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Why a change that names another dialect or platform is refused.
pub const FIXED: &str = "an endpoint's name, dialect and platform are what it is, and never \
     change: its credentials and models were registered for them. Remove the endpoint and \
     register it again";

/// What a header value is shown as when its name suggests a secret.
pub const REDACTED: &str = "<redacted>";

/// How long a check waits for the whole answer.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of an answer a check reads. A model list carrying every field a
/// vendor adds runs to a few hundred KiB.
const MOST_READ: usize = 8 << 20;

/// The auth style an endpoint on `platform` gets when none is named: the one
/// style its platform takes, and on a plain host the bearer token that most
/// hosts copying `OpenAI`'s API expect.
#[must_use]
pub const fn default_auth(platform: Platform) -> AuthStyle {
    match platform {
        Platform::Plain | Platform::Gcp => AuthStyle::Bearer,
        Platform::Azure => AuthStyle::ApiKeyHeader,
        Platform::Aws => AuthStyle::None,
    }
}

/// An endpoint row as a writer means to store it: every column but the
/// timestamps, each spelt as the column spells it.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub name: String,
    pub dialect: String,
    pub platform: String,
    pub base_url: Option<String>,
    pub auth: String,
    pub region: Option<String>,
    pub project: Option<String>,
    pub api_version: Option<String>,
    /// Where a `system_one` endpoint takes a question set, beneath its base
    /// URL (migration 0021). `None` is `/v1/systemone`, Jev's own; no other
    /// dialect has one.
    pub path: Option<String>,
    /// A JSON object of header names to values. Never a key.
    pub extra_headers: Value,
    pub display_name: Option<String>,
    pub discover_models: bool,
}

impl Draft {
    /// The stored row, as the starting point of a change.
    #[must_use]
    pub fn from_row(row: &EndpointRow) -> Self {
        Self {
            name: row.name.clone(),
            dialect: row.dialect.clone(),
            platform: row.platform.clone(),
            base_url: row.base_url.clone(),
            auth: row.auth.clone(),
            region: row.region.clone(),
            project: row.project.clone(),
            api_version: row.api_version.clone(),
            path: row.path.clone(),
            extra_headers: row.extra_headers.clone(),
            display_name: row.display_name.clone(),
            discover_models: row.discover_models,
        }
    }

    fn columns(&self) -> Columns<'_> {
        Columns {
            name: &self.name,
            dialect: &self.dialect,
            platform: &self.platform,
            base_url: self.base_url.as_deref(),
            auth: &self.auth,
            region: self.region.as_deref(),
            project: self.project.as_deref(),
            api_version: self.api_version.as_deref(),
            path: self.path.as_deref(),
            extra_headers: &self.extra_headers,
        }
    }

    fn new_endpoint(&self) -> NewEndpoint<'_> {
        NewEndpoint {
            name: &self.name,
            dialect: &self.dialect,
            platform: &self.platform,
            base_url: self.base_url.as_deref(),
            auth: &self.auth,
            region: self.region.as_deref(),
            project: self.project.as_deref(),
            api_version: self.api_version.as_deref(),
            path: self.path.as_deref(),
            extra_headers: &self.extra_headers,
            display_name: self.display_name.as_deref(),
            discover_models: self.discover_models,
        }
    }

    fn update(&self) -> EndpointUpdate<'_> {
        EndpointUpdate {
            base_url: self.base_url.as_deref(),
            auth: &self.auth,
            region: self.region.as_deref(),
            project: self.project.as_deref(),
            api_version: self.api_version.as_deref(),
            path: self.path.as_deref(),
            extra_headers: &self.extra_headers,
            display_name: self.display_name.as_deref(),
            discover_models: self.discover_models,
        }
    }
}

/// Why an endpoint write was not made.
#[derive(Debug)]
pub enum WriteError {
    /// A rule the draft breaks, in words for the operator: the caller's 400.
    Invalid(String),
    /// Another endpoint has the name: the caller's 409.
    Taken(String),
    /// No endpoint has the name: the caller's 404.
    NotFound,
    /// The database failed: the caller's 500.
    Failed(oag_core::Error),
}

impl WriteError {
    /// As the error a CLI command exits with.
    #[must_use]
    pub fn into_error(self, name: &str) -> oag_core::Error {
        match self {
            Self::Invalid(message) | Self::Taken(message) => oag_core::Error::Config(message),
            Self::NotFound => oag_core::Error::Config(format!(
                "no endpoint named '{name}'; see `oag admin endpoint list`"
            )),
            Self::Failed(e) => e,
        }
    }
}

/// Register `draft` as a new endpoint, if it passes every rule.
pub async fn register(db: &Db, draft: Draft) -> Result<EndpointRow, WriteError> {
    let (draft, platform) = checked(draft).map_err(WriteError::Invalid)?;
    resolves(&draft, platform).await?;
    repo::insert_endpoint(db, &draft.new_endpoint())
        .await
        .map_err(write_error)
}

/// Replace `stored`'s settings with `draft`'s, if the draft passes every rule
/// and names the same endpoint.
///
/// The base URL's name is resolved only when the URL changed: a lookup that
/// fails today is no reason to refuse a new display name for an endpoint that
/// was checked when its URL was written.
pub async fn change(
    db: &Db,
    stored: &EndpointRow,
    draft: Draft,
) -> Result<EndpointRow, WriteError> {
    let same = (
        draft.name.as_str(),
        draft.dialect.as_str(),
        draft.platform.as_str(),
    ) == (
        stored.name.as_str(),
        stored.dialect.as_str(),
        stored.platform.as_str(),
    );
    if !same {
        return Err(WriteError::Invalid(FIXED.to_owned()));
    }
    let (draft, platform) = checked(draft).map_err(WriteError::Invalid)?;
    if draft.base_url != stored.base_url {
        resolves(&draft, platform).await?;
    }
    match repo::update_endpoint(db, &stored.name, &draft.update()).await {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(WriteError::NotFound),
        Err(e) => Err(write_error(e)),
    }
}

/// `draft` as it is stored, and its platform, or the first rule it breaks.
///
/// Blank optional text is no value at all, the base URL is stored in the
/// normalised form the reload would give it, and the headers must be ones
/// the adapter will send.
fn checked(mut draft: Draft) -> Result<(Draft, Platform), String> {
    for field in [
        &mut draft.base_url,
        &mut draft.region,
        &mut draft.project,
        &mut draft.api_version,
        &mut draft.path,
    ] {
        *field = present(field.as_deref());
    }
    draft.display_name = display_name(draft.display_name.as_deref())?;
    let config =
        EndpointConfig::from_columns(&draft.columns()).map_err(|refusal| refusal.message)?;
    EndpointSpec::new(
        config.endpoint,
        config.base_url.clone().unwrap_or_default(),
        config.auth,
        config
            .extra_headers
            .iter()
            .map(|(name, value)| (name, value)),
    )?;
    draft.base_url = config.base_url;
    Ok((draft, config.endpoint.platform()))
}

/// What `value` says once trimmed, if it says anything.
fn present(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// A display name, trimmed; blank is none. It reaches the console, logs and
/// listings, so it is a line of at most 128 characters.
fn display_name(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(name) = present(raw) else {
        return Ok(None);
    };
    if name.chars().count() > 128 {
        return Err("display_name must be 128 characters or fewer".to_owned());
    }
    if name.chars().any(char::is_control) {
        return Err("display_name must not contain control characters".to_owned());
    }
    Ok(Some(name))
}

/// The one rule the reload cannot apply: what the base URL's name resolves to.
async fn resolves(draft: &Draft, platform: Platform) -> Result<(), WriteError> {
    match &draft.base_url {
        Some(url) => validate_endpoint_base_url(url, platform)
            .await
            .map_err(WriteError::Invalid),
        None => Ok(()),
    }
}

/// A refused store write as the caller's answer: a name in use is a conflict,
/// a CHECK the draft fails is the draft's fault, and the rest is the
/// database's.
fn write_error(e: oag_core::Error) -> WriteError {
    match e {
        oag_core::Error::Config(message) if message.contains("already exists") => {
            WriteError::Taken(message)
        }
        oag_core::Error::Config(message) => WriteError::Invalid(message),
        other => WriteError::Failed(other),
    }
}

/// Why this build does not serve `row`, or `None` when it does: the reload's
/// own answer, asked of one row.
///
/// The reload hands every spec the gateway's token cache, and the factory
/// refuses a gcp endpoint without one, so this hands it one too. Nothing is
/// minted through it: an adapter is built here, and never sent anything.
#[must_use]
pub fn refusal(row: &EndpointRow) -> Option<Refusal> {
    match GcpTokenCache::new(DEFAULT_TOKEN_URL) {
        Ok(tokens) => crate::state::endpoint_upstream(row, &Arc::new(tokens)).err(),
        Err(e) => Some(Refusal::new(Reason::Unsupported, e.to_string())),
    }
}

/// The extra headers as a person may see them: every name and value, in the
/// stored order, except that a value whose header name mentions a key, a
/// token, a secret or auth is shown as [`REDACTED`].
///
/// Extra headers never hold a secret, and every writer says so. This is for
/// the one someone put there anyway: listing it would copy it into a terminal,
/// a screenshot or a ticket.
#[must_use]
pub fn shown_headers(headers: &Value) -> Vec<(String, String)> {
    let Some(object) = headers.as_object() else {
        return Vec::new();
    };
    object
        .iter()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            let shown = if ["key", "token", "secret", "auth"]
                .iter()
                .any(|word| lower.contains(word))
            {
                REDACTED.to_owned()
            } else {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned)
            };
            (name.clone(), shown)
        })
        .collect()
}

/// What asking an endpoint for its models found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Checked {
    /// What was asked, once a request could be made. Never holds a key: a key
    /// rides in a header.
    pub url: Option<String>,
    /// The status the endpoint answered with, when it answered.
    pub status: Option<u16>,
    /// How many models the answer lists.
    pub models: Option<usize>,
    /// Whether the answer said there were more pages than the one read.
    pub more: bool,
    /// What went wrong, if anything did.
    pub error: Option<String>,
}

impl Checked {
    /// Whether the endpoint answered with a model list.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    fn failed(url: Option<String>, status: Option<u16>, error: String) -> Self {
        Self {
            url,
            status,
            models: None,
            more: false,
            error: Some(error),
        }
    }
}

/// Ask `row`'s endpoint which models it lists, the way its dialect's vendor
/// lists them, and count them.
///
/// With `secret`, the key rides in the header the endpoint's auth style names;
/// without, the request carries none, which is enough to see that the host
/// answers at that path. `proxy` is the credential's egress proxy, when the key
/// is one that goes through one.
///
/// The request is refused if the base URL's name resolves to a link-local or
/// metadata address now, follows no redirect, and gives up after
/// [`CHECK_TIMEOUT`].
pub async fn check(row: &EndpointRow, secret: Option<&str>, proxy: Option<&str>) -> Checked {
    let request = match models_request(row, secret) {
        Ok(request) => request,
        Err(e) => return Checked::failed(None, None, e),
    };
    let asked = request.url().clone();
    let url = Some(asked.to_string());
    if let Err(e) = deny_resolved_target(&asked).await {
        return Checked::failed(url, None, e);
    }
    let client = match client(proxy) {
        Ok(client) => client,
        Err(e) => return Checked::failed(url, None, e),
    };
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(e) if e.is_timeout() => {
            return Checked::failed(
                url,
                None,
                format!("no answer within {}s", CHECK_TIMEOUT.as_secs()),
            );
        }
        Err(e) => return Checked::failed(url, None, format!("no answer: {}", chain(&e))),
    };
    let status = response.status();
    let code = Some(status.as_u16());
    if status.is_redirection() {
        let to = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|location| location.to_str().ok())
            .and_then(|location| asked.join(location).ok())
            .map(|target| format!(" to {}", target.origin().ascii_serialization()))
            .unwrap_or_default();
        return Checked::failed(
            url,
            code,
            format!(
                "answered {status} with a redirect{to}, which a check never follows: a key \
                 would go with it"
            ),
        );
    }
    if !status.is_success() {
        let without = if secret.is_none() {
            " without a key"
        } else {
            ""
        };
        return Checked::failed(url, code, format!("answered {status}{without}"));
    }
    match read(response).await.and_then(|body| count(&body)) {
        Ok((models, more)) => Checked {
            url,
            status: code,
            models: Some(models),
            more,
            error: None,
        },
        Err(e) => Checked::failed(url, code, e),
    }
}

/// The model-list request for `row`, or why none can be made.
fn models_request(row: &EndpointRow, secret: Option<&str>) -> Result<reqwest::Request, String> {
    let config = row.to_endpoint().map_err(|refusal| refusal.message)?;
    let spec = EndpointSpec::new(
        config.endpoint,
        config.base_url.unwrap_or_default(),
        config.auth,
        config.extra_headers,
    )?;
    spec.models_request(secret).map_err(|e| match e {
        oag_core::Error::Config(message) => message,
        other => other.to_string(),
    })
}

/// A client that follows no redirect and waits [`CHECK_TIMEOUT`] at most.
fn client(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        // Not one: the request may carry a key, and a hop to another host
        // would carry it there.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!(
            "open-ai-gateway/",
            env!("CARGO_PKG_VERSION"),
            " endpoint-check"
        ));
    if let Some(proxy) = proxy.map(str::trim).filter(|p| !p.is_empty()) {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|e| format!("the credential's proxy_url is unusable: {e}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| format!("building the check's client: {e}"))
}

/// An error and every cause under it, which is where reqwest keeps the reason
/// a connection failed.
fn chain(e: &dyn std::error::Error) -> String {
    let mut words = e.to_string();
    let mut cause = e.source();
    while let Some(inner) = cause {
        words.push_str(": ");
        words.push_str(&inner.to_string());
        cause = inner.source();
    }
    words
}

/// The answer's body, up to [`MOST_READ`].
async fn read(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("reading the answer: {}", chain(&e)))?
    {
        if body.len() + chunk.len() > MOST_READ {
            return Err(format!(
                "the answer is larger than {} MiB, more than a model list",
                MOST_READ >> 20
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// How many models a list names, and whether it said there were more pages.
///
/// `data` is the list in `OpenAI`'s and Anthropic's shapes and `models` in
/// Gemini's and System One's; a bare array is a list too. Anthropic says
/// `has_more` and Gemini gives a `nextPageToken` when the answer is one page
/// of several.
fn count(body: &[u8]) -> Result<(usize, bool), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| format!("the answer is not JSON: {e}"))?;
    let list = match &value {
        Value::Array(models) => models,
        other => other
            .get("data")
            .or_else(|| other.get("models"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                "the answer is JSON but not a model list: it has no `data` or `models` list"
                    .to_owned()
            })?,
    };
    let more = value.get("has_more").and_then(Value::as_bool) == Some(true)
        || value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .is_some_and(|token| !token.is_empty());
    Ok((list.len(), more))
}

#[cfg(test)]
mod tests;
