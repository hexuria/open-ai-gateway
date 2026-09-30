//! What an endpoint's model list says: which models a key may use, and, where
//! the list prices them, what each one costs.
//!
//! Two readers of one kind of document:
//!
//! - **Discovery.** [`served`] reads the list where the endpoint's dialect
//!   keeps it (`{base}/models`, or `{base}/v1/models` for the Messages
//!   dialect) for the usage poller, which records the ids in
//!   `account.served_models`. Ids only.
//! - **The catalog sync.** A catalog row is priced or it is not written, and an
//!   id-only list prices nothing. [`priced`] reads a list that prices each
//!   model per vendor, in the shape Merge Gateway serves, and [`choose`] turns
//!   one of its entries into a priced [`Offer`], or into the [`Skip`] that says
//!   why it has none.
//!
//! Both follow a list across its pages — `has_more` with `next_cursor` (Merge)
//! or `last_id` (Anthropic), or `nextPageToken` (Gemini) — for at most
//! [`MAX_PAGES`]. A list neither can read to its end is an error, never a
//! shorter list: discovery would record the short list as everything the key
//! serves and hide the models on the pages it never read, and a sync would
//! remove them from the catalog.
//!
//! Every request carries the endpoint's key in the header its auth style names,
//! and the operator's extra headers, through a client that follows no redirect:
//! a followed redirect takes the key to wherever `Location` points.

use crate::custom::{ExtraHeaders, authenticate};
use oag_core::provider::{AuthStyle, Dialect};
use oag_core::{Error, Result};
use reqwest::Url;
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// The most pages one list may take. A list still naming a next page after
/// this many is unreadable, for the reason the module gives.
pub const MAX_PAGES: usize = 20;

/// The largest page read, in bytes. A model list is tens or hundreds of
/// kilobytes; the poller that reads one runs inside the gateway, and a base URL
/// pointed at something else must not be read into its memory whole.
const MAX_PAGE_BYTES: usize = 8 << 20;

/// Per request. These run on a poller or from the CLI, never on the request
/// path.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The page size a priced list is asked for: Merge's maximum, so its whole
/// catalog is one or two pages rather than a dozen.
const PRICED_PAGE_SIZE: &str = "500";

/// What a model's chosen vendor is assumed to hold when the list states no
/// window. Understating is the safe direction: the router only ever rejects a
/// model whose window is too small. The same numbers the xAI inserts and the
/// native price sync assume.
const ASSUMED_CONTEXT_WINDOW: i32 = 131_072;
const ASSUMED_MAX_OUTPUT_TOKENS: i32 = 8_192;

/// Availability statuses that say a model, or one vendor of it, cannot be
/// called now. Anything else, and no status at all, is callable.
const UNAVAILABLE: [&str; 6] = [
    "deprecated",
    "unavailable",
    "retired",
    "disabled",
    "discontinued",
    "sunset",
];

/// Where an endpoint answers and how it takes its key: what reading its model
/// list needs.
#[derive(Debug, Clone, Copy)]
pub struct ModelSource<'a> {
    pub dialect: Dialect,
    /// As the adapters get it: no trailing slash, no query, no fragment.
    pub base_url: &'a str,
    pub auth: AuthStyle,
    pub headers: &'a ExtraHeaders,
}

impl ModelSource<'_> {
    /// Where this endpoint's dialect keeps its model list, asking for the
    /// largest page the dialect's own API allows.
    ///
    /// Chat Completions gets no page size: its `/models` is one page wherever
    /// it is served, and a parameter one host does not know is one some host
    /// refuses.
    pub fn list_url(&self) -> Result<Url> {
        let mut url = parse_url(&format!("{}{}", self.base_url, list_path(self.dialect)?))?;
        let size = match self.dialect {
            Dialect::AnthropicMessages => Some(("limit", "1000")),
            Dialect::GeminiGenerateContent => Some(("pageSize", "1000")),
            _ => None,
        };
        if let Some((name, value)) = size {
            url.query_pairs_mut().append_pair(name, value);
        }
        Ok(url)
    }
}

/// The path, under the base URL, that a dialect's model list is at.
fn list_path(dialect: Dialect) -> Result<&'static str> {
    match dialect {
        Dialect::OpenAIChatCompletions | Dialect::GeminiGenerateContent => Ok("/models"),
        Dialect::AnthropicMessages => Ok("/v1/models"),
        other => Err(Error::Config(format!(
            "an endpoint speaking {other} has no model list to read"
        ))),
    }
}

/// The ids one endpoint's model list names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The first page's URL.
    pub url: String,
    pub pages: usize,
    /// In the list's order, each once. Empty when the list is: the endpoint
    /// was asked and serves nothing, which is not the same as not knowing.
    pub models: Vec<String>,
}

/// Ask an endpoint which models `key` may use.
///
/// An entry the list marks as needing access the key has not been granted, or
/// as deprecated or unavailable, is left out: it is listed and not callable.
/// An entry is named by its `id`, by `model` in a list that prices vendors
/// (whose `id`, if it had one, would not be the name the endpoint takes), or
/// by a Gemini `name` without its `models/` prefix.
///
/// A list whose entries name nothing is an error rather than an empty answer,
/// and so is anything that is not a list: an empty answer hides every model the
/// key has, and it must only ever mean the endpoint said so.
pub async fn served(source: &ModelSource<'_>, key: &str, proxy: Option<&str>) -> Result<Served> {
    let url = source.list_url()?;
    let client = crate::side_channel_client(proxy, TIMEOUT)?;
    let pages = read_pages(&client, source, &url, key, None).await?;
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for page in &pages {
        for id in page_ids(page)? {
            if seen.insert(id.clone()) {
                models.push(id);
            }
        }
    }
    Ok(Served {
        url: url.to_string(),
        pages: pages.len(),
        models,
    })
}

/// A model list that prices its models: every entry, as listed.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    /// The first page's URL, which is where the list was found.
    pub url: String,
    pub pages: usize,
    pub models: Vec<ListedModel>,
}

/// One entry of a priced list, before anything is chosen from it.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedModel {
    /// The name the endpoint takes on the wire: Merge's `model`. `None` for an
    /// entry with no name, which [`choose`] skips.
    pub upstream: Option<String>,
    pub display_name: Option<String>,
    pub status: Option<String>,
    pub access_required: bool,
    /// In the list's order, which is what [`PriceChoice::First`] reads.
    pub vendors: Vec<Vendor>,
}

/// One vendor an entry can be served by, with its own terms.
#[derive(Debug, Clone, PartialEq)]
pub struct Vendor {
    pub name: String,
    pub status: Option<String>,
    pub access_required: bool,
    pub context_window: Option<i32>,
    pub max_output_tokens: Option<i32>,
    /// Modalities, lowercased: `text`, `image`, `audio`, `video`.
    pub input: Vec<String>,
    pub output: Vec<String>,
    pub tools: bool,
    pub reasoning: bool,
    /// `None` unless both per-token prices are stated.
    pub price: Option<Price>,
}

impl Vendor {
    /// Text in and text out: what a chat request needs.
    fn is_chat(&self) -> bool {
        self.input.iter().any(|m| m == "text") && self.output.iter().any(|m| m == "text")
    }

    fn callable(&self) -> bool {
        !self.access_required && !unavailable(self.status.as_deref())
    }
}

/// USD per million tokens, rounded to the catalog's six decimal places, so a
/// price compares equal to what the catalog stored for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub input: Decimal,
    pub output: Decimal,
    pub cache_read: Option<Decimal>,
    pub cache_write: Option<Decimal>,
}

impl Price {
    fn is_free(&self) -> bool {
        self.input.is_zero() && self.output.is_zero()
    }
}

/// Which vendor's terms a model is priced by, when several serve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PriceChoice {
    /// The lowest input plus output price among the vendors that can serve it,
    /// the first listed on a tie. Merge routes a request to its cheapest vendor
    /// (a GLM 5.3 Flash call went to `particle`, the lowest-priced), so this is
    /// the price a request is actually charged.
    #[default]
    Cheapest,
    /// The first vendor listed that can serve it.
    First,
}

impl PriceChoice {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cheapest => "cheapest",
            Self::First => "first",
        }
    }
}

/// A model a priced list offers for chat, on the terms of one vendor.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub upstream: String,
    pub display_name: Option<String>,
    /// The vendor whose price, window and capabilities these are.
    pub vendor: String,
    pub price: Price,
    /// The vendor's own, or an understatement when it states none.
    pub context_window: i32,
    pub max_output_tokens: i32,
    /// Whether the vendor takes an image in.
    pub vision: bool,
    pub tools: bool,
    pub reasoning: bool,
}

/// Why a priced list's entry is not offered for chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// No name, or one no request could carry.
    Unnamed,
    /// Listed for accounts granted access to it.
    AccessRequired,
    /// The entry's own status: deprecated, unavailable and the like.
    Unavailable(String),
    /// No vendor serves it.
    NoVendor,
    /// No vendor takes text in and gives text out. Carries what the first
    /// vendor gives instead.
    NotChat(String),
    /// Every vendor that could serve it for chat is unavailable or needs
    /// access.
    NoCallableVendor,
    /// No callable chat vendor states both per-token prices.
    NoPrice,
    /// Every priced vendor lists it at zero, which would win every cost
    /// comparison the router makes.
    Free,
}

impl Skip {
    /// A short name for the reason, the same for every entry skipped for it:
    /// what a summary counts by.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Unnamed => "unnamed",
            Self::AccessRequired => "access required",
            Self::Unavailable(_) => "deprecated or unavailable",
            Self::NoVendor => "no vendor",
            Self::NotChat(_) => "not a chat model",
            Self::NoCallableVendor => "no available vendor",
            Self::NoPrice => "no per-token price",
            Self::Free => "listed free",
        }
    }
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unnamed => f.write_str("the entry names no model"),
            Self::AccessRequired => f.write_str("the list says it needs access this key lacks"),
            Self::Unavailable(status) => write!(f, "the list marks it {status}"),
            Self::NoVendor => f.write_str("no vendor serves it"),
            Self::NotChat(output) => write!(f, "not a chat model: it gives {output}"),
            Self::NoCallableVendor => f.write_str("every vendor that serves it is unavailable"),
            Self::NoPrice => f.write_str("no vendor states a per-token price for it"),
            Self::Free => f.write_str(
                "listed at zero, which would win every cost comparison; add it with \
                 `oag admin catalog add --free` if that is meant",
            ),
        }
    }
}

/// Price one entry for chat, by `choice`, or say why not.
///
/// Only a vendor that takes text in and gives text out, is not marked
/// unavailable and needs no access is a candidate, and only one stating both
/// per-token prices, of which not both are zero, can price the entry. The
/// window and the capabilities come from the vendor the price does, because
/// that is the vendor the request is served by.
pub fn choose(model: &ListedModel, choice: PriceChoice) -> std::result::Result<Offer, Skip> {
    let upstream = model
        .upstream
        .as_deref()
        .filter(|name| is_wire_name(name))
        .ok_or(Skip::Unnamed)?;
    if model.access_required {
        return Err(Skip::AccessRequired);
    }
    if let Some(status) = model.status.as_deref().filter(|s| unavailable(Some(s))) {
        return Err(Skip::Unavailable(status.to_owned()));
    }
    let first = model.vendors.first().ok_or(Skip::NoVendor)?;
    let chat: Vec<&Vendor> = model.vendors.iter().filter(|v| v.is_chat()).collect();
    if chat.is_empty() {
        let output = if first.output.is_empty() {
            "no stated output".to_owned()
        } else {
            first.output.join(" and ")
        };
        return Err(Skip::NotChat(output));
    }
    let callable: Vec<&Vendor> = chat.into_iter().filter(|v| v.callable()).collect();
    if callable.is_empty() {
        return Err(Skip::NoCallableVendor);
    }
    let priced: Vec<(&Vendor, Price)> = callable
        .into_iter()
        .filter_map(|v| v.price.map(|p| (v, p)))
        .collect();
    if priced.is_empty() {
        return Err(Skip::NoPrice);
    }
    let mut paid = priced.into_iter().filter(|(_, p)| !p.is_free());
    let chosen = match choice {
        // `min_by_key` keeps the first of equal minimums: a tie goes to the
        // vendor listed first.
        PriceChoice::Cheapest => paid.min_by_key(|(_, p)| p.input + p.output),
        PriceChoice::First => paid.next(),
    };
    let (vendor, price) = chosen.ok_or(Skip::Free)?;
    Ok(Offer {
        upstream: upstream.to_owned(),
        display_name: model.display_name.clone(),
        vendor: vendor.name.clone(),
        price,
        context_window: vendor.context_window.unwrap_or(ASSUMED_CONTEXT_WINDOW),
        max_output_tokens: vendor
            .max_output_tokens
            .unwrap_or(ASSUMED_MAX_OUTPUT_TOKENS),
        vision: vendor.input.iter().any(|m| m == "image"),
        tools: vendor.tools,
        reasoning: vendor.reasoning,
    })
}

/// Read an endpoint's priced model list.
///
/// At `listing_url` when one is given, which must be on the endpoint's own
/// origin: the endpoint's key goes with the request, and only ever to where the
/// endpoint answers. Otherwise at the first of these that prices its models:
///
/// 1. the list URL of the endpoint's dialect, `{base}/models` (or
///    `{base}/v1/models`);
/// 2. `/v1/models` at the base URL's origin, which is where Merge keeps its
///    priced list for every surface it serves (`/v1/openai`, `/v1/anthropic`).
///
/// A list is priced when its entries carry a `vendors` object. A list that
/// answers with ids alone is refused, whichever URL it came from: a catalog row
/// needs a price, and none is ever invented.
///
/// Each request asks for the largest page Merge serves (`limit=500`) unless the
/// URL already names a limit.
pub async fn priced(
    source: &ModelSource<'_>,
    listing_url: Option<&str>,
    key: &str,
    proxy: Option<&str>,
) -> Result<Listing> {
    let candidates = match listing_url {
        Some(raw) => vec![on_origin(source, raw)?],
        None => derived(source)?,
    };
    let client = crate::side_channel_client(proxy, TIMEOUT)?;
    let mut unpriced = None;
    let mut empty = None;
    let mut failures = Vec::new();
    for url in candidates {
        let url = with_default(&url, "limit", PRICED_PAGE_SIZE);
        let first = match get_json(&client, source, &url, key).await {
            Ok(page) => page,
            Err(e) => {
                failures.push(e.to_string());
                continue;
            }
        };
        match shape(&first) {
            Shape::Priced => {
                let pages = read_pages(&client, source, &url, key, Some(first)).await?;
                let models = pages.iter().flat_map(priced_entries).collect();
                return Ok(Listing {
                    url: url.to_string(),
                    pages: pages.len(),
                    models,
                });
            }
            Shape::Unpriced => {
                unpriced.get_or_insert(url);
            }
            Shape::Empty => {
                empty.get_or_insert(url);
            }
            Shape::NotAList => failures.push(format!("{url} did not answer with a model list")),
        }
    }
    if let Some(url) = unpriced {
        return Err(Error::Config(format!(
            "the model list at {url} names models without prices, and a catalog row needs \
             one, so nothing was written. Add its models one at a time with `oag admin \
             catalog add --id <endpoint>/<model> --upstream <model> --input-per-mtok <usd> \
             --output-per-mtok <usd> --context <tokens> --max-output <tokens>`, or point \
             --listing-url at a list that prices them (per vendor, as Merge Gateway's does)"
        )));
    }
    if let Some(url) = empty {
        return Err(Error::Config(format!(
            "the model list at {url} is empty, so nothing was written"
        )));
    }
    Err(Error::Internal(format!(
        "no priced model list could be read: {}",
        failures.join("; ")
    )))
}

/// The URLs [`priced`] tries when it is not given one, in order.
fn derived(source: &ModelSource<'_>) -> Result<Vec<Url>> {
    let own = parse_url(&format!(
        "{}{}",
        source.base_url,
        list_path(source.dialect)?
    ))?;
    let mut origin = own.clone();
    origin.set_path("/v1/models");
    origin.set_query(None);
    Ok(if origin == own {
        vec![own]
    } else {
        vec![own, origin]
    })
}

/// `raw` as a listing URL, if it is on the endpoint's origin.
fn on_origin(source: &ModelSource<'_>, raw: &str) -> Result<Url> {
    let url = parse_url(raw.trim())?;
    let base = parse_url(source.base_url)?;
    if url.origin() != base.origin() {
        return Err(Error::Config(format!(
            "the listing URL {url} is not on the endpoint's origin ({}): the endpoint's key \
             is sent with the request, so it only goes where the endpoint answers",
            base.origin().ascii_serialization()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::Config(
            "the listing URL may not carry credentials or a fragment".to_owned(),
        ));
    }
    Ok(url)
}

fn parse_url(raw: &str) -> Result<Url> {
    Url::parse(raw).map_err(|e| Error::Config(format!("{raw} is not a URL: {e}")))
}

/// `url` with `name=value` added, unless it already names `name`.
fn with_default(url: &Url, name: &str, value: &str) -> Url {
    if url.query_pairs().any(|(k, _)| k == name) {
        return url.clone();
    }
    let mut url = url.clone();
    url.query_pairs_mut().append_pair(name, value);
    url
}

/// `url` with `name` set to `value`, replacing any value it had.
fn with_param(url: &Url, name: &str, value: &str) -> Url {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != name)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut next = url.clone();
    next.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair(name, value);
    next
}

/// Every page of the list starting at `first`, the first already read when
/// `fetched` holds it.
async fn read_pages(
    client: &reqwest::Client,
    source: &ModelSource<'_>,
    first: &Url,
    key: &str,
    fetched: Option<Value>,
) -> Result<Vec<Value>> {
    let mut page = match fetched {
        Some(page) => page,
        None => get_json(client, source, first, key).await?,
    };
    let mut pages = Vec::new();
    let mut cursors = HashSet::new();
    loop {
        let next = next_page(&page, first)?;
        pages.push(page);
        let Some((param, cursor)) = next else {
            return Ok(pages);
        };
        if pages.len() >= MAX_PAGES {
            return Err(Error::Internal(format!(
                "the model list at {first} runs past {MAX_PAGES} pages; a list read only in \
                 part is not used"
            )));
        }
        if !cursors.insert(cursor.clone()) {
            return Err(Error::Internal(format!(
                "the model list at {first} named the same next page twice"
            )));
        }
        page = get_json(client, source, &with_param(first, param, &cursor), key).await?;
    }
}

/// The query parameter and value that ask for the page after `page`, or `None`
/// when it is the last.
fn next_page(page: &Value, url: &Url) -> Result<Option<(&'static str, String)>> {
    let text = |key: &str| {
        page.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match page.get("has_more").and_then(Value::as_bool) {
        Some(false) => Ok(None),
        Some(true) => text("next_cursor")
            .map(|cursor| ("cursor", cursor))
            .or_else(|| text("last_id").map(|last| ("after_id", last)))
            .map(Some)
            .ok_or_else(|| {
                Error::Internal(format!(
                    "the model list at {url} says it has more pages and names no cursor \
                     for the next one"
                ))
            }),
        None => Ok(text("nextPageToken")
            .map(|token| ("pageToken", token))
            .or_else(|| text("next_cursor").map(|cursor| ("cursor", cursor)))),
    }
}

/// One page, as JSON, read with the endpoint's key and headers.
async fn get_json(
    client: &reqwest::Client,
    source: &ModelSource<'_>,
    url: &Url,
    key: &str,
) -> Result<Value> {
    let mut builder = client.get(url.clone()).header("accept", "application/json");
    if source.dialect == Dialect::AnthropicMessages {
        builder = builder.header("anthropic-version", oag_proto::anthropic::API_VERSION);
    }
    let builder = source
        .headers
        .apply(authenticate(builder, source.auth, key));
    let mut response = builder
        .send()
        .await
        .map_err(|e| Error::Internal(format!("model list {url}: {e}")))?;
    let status = response.status();
    let body = bounded_body(&mut response, url).await?;
    if !status.is_success() {
        return Err(Error::Internal(format!(
            "model list {url} returned {status}: {}",
            snippet(&String::from_utf8_lossy(&body))
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|e| Error::Internal(format!("model list {url} is not JSON: {e}")))
}

async fn bounded_body(response: &mut reqwest::Response, url: &Url) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Internal(format!("reading model list {url}: {e}")))?
    {
        if body.len() + chunk.len() > MAX_PAGE_BYTES {
            return Err(Error::Internal(format!(
                "model list {url} sent a page over {} MiB; not read",
                MAX_PAGE_BYTES >> 20
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn snippet(body: &str) -> String {
    const MAX: usize = 300;
    if body.len() <= MAX {
        return body.to_owned();
    }
    let mut end = MAX;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

/// A page's entries: `data` (`OpenAI`, Anthropic, Merge), `models` (Gemini),
/// or the page itself when it is a bare array.
fn items(page: &Value) -> Option<&Vec<Value>> {
    match page {
        Value::Array(items) => Some(items),
        Value::Object(o) => o
            .get("data")
            .or_else(|| o.get("models"))
            .and_then(Value::as_array),
        _ => None,
    }
}

/// The callable model ids on one page. See [`served`].
fn page_ids(page: &Value) -> Result<Vec<String>> {
    let items = items(page).ok_or_else(|| {
        Error::Internal(format!("not a model list: {}", snippet(&page.to_string())))
    })?;
    let named: Vec<(&Value, String)> = items
        .iter()
        .filter_map(|item| model_id(item).map(|id| (item, id)))
        .collect();
    if named.is_empty() && !items.is_empty() {
        return Err(Error::Internal(format!(
            "the model list names no model: {}",
            snippet(&page.to_string())
        )));
    }
    Ok(named
        .into_iter()
        .filter(|(item, _)| callable(item))
        .map(|(_, id)| id)
        .collect())
}

/// The name an entry gives its model. See [`served`].
fn model_id(item: &Value) -> Option<String> {
    if let Value::String(id) = item {
        return Some(id.clone()).filter(|id| is_wire_name(id));
    }
    let text = |key: &str| item.get(key).and_then(Value::as_str);
    let named = if item.get("vendors").is_some() {
        text("model").or_else(|| text("id"))
    } else {
        text("id").or_else(|| text("model"))
    };
    named
        .or_else(|| text("name").map(|name| name.strip_prefix("models/").unwrap_or(name)))
        .filter(|id| is_wire_name(id))
        .map(str::to_owned)
}

/// Whether an entry is marked callable: no access requirement, and no status
/// in [`UNAVAILABLE`].
fn callable(item: &Value) -> bool {
    !item
        .get("access_required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !unavailable(item.get("availability_status").and_then(Value::as_str))
}

fn unavailable(status: Option<&str>) -> bool {
    status.is_some_and(|s| UNAVAILABLE.iter().any(|u| s.eq_ignore_ascii_case(u)))
}

/// A name a request can carry: something, with no whitespace or control
/// character in it.
fn is_wire_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// What the first page of a list says about the rest.
enum Shape {
    /// Its entries carry `vendors`.
    Priced,
    /// It names models and prices none of them.
    Unpriced,
    /// It is a list, with nothing in it.
    Empty,
    NotAList,
}

fn shape(page: &Value) -> Shape {
    match items(page) {
        None => Shape::NotAList,
        Some(items) if items.is_empty() => Shape::Empty,
        Some(items)
            if items
                .iter()
                .any(|i| i.get("vendors").is_some_and(Value::is_object)) =>
        {
            Shape::Priced
        }
        Some(_) => Shape::Unpriced,
    }
}

/// The entries on one page of a priced list, as [`priced`] reads them. Nothing
/// for a page that is not a list.
#[must_use]
pub fn priced_entries(page: &Value) -> Vec<ListedModel> {
    items(page)
        .into_iter()
        .flatten()
        .map(listed_model)
        .collect()
}

/// One entry of a priced list, read leniently: a field that is missing or of
/// the wrong type reads as absent, and [`choose`] decides what its absence
/// means.
fn listed_model(item: &Value) -> ListedModel {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let vendors = item
        .get("vendors")
        .and_then(Value::as_object)
        .map(|vendors| {
            vendors
                .iter()
                .map(|(name, v)| {
                    let capabilities = &v["capabilities"];
                    Vendor {
                        name: name.clone(),
                        status: text(v, "availability_status"),
                        access_required: v["access_required"].as_bool().unwrap_or(false),
                        context_window: positive(&v["context_window"]),
                        max_output_tokens: positive(&v["max_output_tokens"]),
                        input: modalities(&capabilities["input"]),
                        output: modalities(&capabilities["output"]),
                        tools: capabilities["supports_tool_calling"]
                            .as_bool()
                            .unwrap_or(false),
                        reasoning: capabilities["supports_reasoning"]
                            .as_bool()
                            .unwrap_or(false),
                        price: price(&v["pricing"]),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    ListedModel {
        upstream: text(item, "model").or_else(|| text(item, "id")),
        display_name: text(item, "display_name").filter(|n| !n.trim().is_empty()),
        status: text(item, "availability_status"),
        access_required: item["access_required"].as_bool().unwrap_or(false),
        vendors,
    }
}

fn modalities(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn positive(value: &Value) -> Option<i32> {
    let n = match value {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    i32::try_from(n).ok().filter(|n| *n > 0)
}

fn price(pricing: &Value) -> Option<Price> {
    Some(Price {
        input: money(&pricing["input_per_million"])?,
        output: money(&pricing["output_per_million"])?,
        cache_read: money(&pricing["cache_read_per_million"]),
        cache_write: money(&pricing["cache_write_per_million"]),
    })
}

/// A price as an exact decimal, through its written form rather than a binary
/// float, rounded half away from zero to six places as the catalog's
/// `numeric(12,6)` columns round it. Negative, or too large for those columns,
/// reads as no price.
fn money(value: &Value) -> Option<Decimal> {
    let written = match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_owned(),
        _ => return None,
    };
    let exact = Decimal::from_str(&written)
        .or_else(|_| Decimal::from_scientific(&written))
        .ok()?;
    let stored = exact.round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero);
    (stored >= Decimal::ZERO && stored < Decimal::from(1_000_000)).then_some(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PAGE_1: &str = include_str!("../tests/fixtures/merge-models-page-1.json");
    const PAGE_2: &str = include_str!("../tests/fixtures/merge-models-page-2.json");

    fn page(raw: &str) -> Value {
        serde_json::from_str(raw).expect("a fixture page")
    }

    fn fixture() -> Vec<ListedModel> {
        [PAGE_1, PAGE_2]
            .iter()
            .flat_map(|raw| priced_entries(&page(raw)))
            .collect()
    }

    fn listed<'a>(models: &'a [ListedModel], name: &str) -> &'a ListedModel {
        models
            .iter()
            .find(|m| m.upstream.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name} is in the fixture"))
    }

    fn source<'a>(dialect: Dialect, base: &'a str, headers: &'a ExtraHeaders) -> ModelSource<'a> {
        ModelSource {
            dialect,
            base_url: base,
            auth: AuthStyle::Bearer,
            headers,
        }
    }

    #[test]
    fn the_cheapest_callable_vendor_prices_a_model_and_brings_its_own_terms() {
        let models = fixture();
        let glm = choose(listed(&models, "zai/glm-5.3-flash"), PriceChoice::Cheapest)
            .expect("a chat model with three priced vendors");
        assert_eq!(glm.vendor, "particle", "0.06 + 0.22 is the lowest total");
        assert_eq!(
            glm.price,
            Price {
                input: dec!(0.06),
                output: dec!(0.22),
                cache_read: None,
                cache_write: None
            }
        );
        assert_eq!(
            (glm.context_window, glm.max_output_tokens),
            (131_072, 32_768)
        );
        assert!(glm.tools && !glm.reasoning && !glm.vision, "{glm:?}");
        assert_eq!(glm.display_name.as_deref(), Some("GLM 5.3 Flash"));

        let first = choose(listed(&models, "zai/glm-5.3-flash"), PriceChoice::First)
            .expect("the same model");
        assert_eq!(first.vendor, "zai", "listed first");
        assert_eq!(
            (first.price.input, first.price.output),
            (dec!(0.1), dec!(0.4))
        );
        assert_eq!(first.price.cache_read, Some(dec!(0.02)));
        assert_eq!(first.context_window, 200_000);
        assert!(first.reasoning);
    }

    #[test]
    fn a_vendor_marked_unavailable_never_prices_a_model_however_cheap() {
        let models = fixture();
        for choice in [PriceChoice::Cheapest, PriceChoice::First] {
            let deepseek = choose(listed(&models, "deepseek/deepseek-v3.2"), choice)
                .expect("one callable vendor");
            assert_eq!(deepseek.vendor, "deepseek", "{choice:?}");
            assert_eq!(deepseek.price.cache_read, Some(dec!(0.028)));
        }
    }

    #[test]
    fn cache_prices_and_image_input_come_through() {
        let models = fixture();
        let sonnet = choose(
            listed(&models, "anthropic/claude-sonnet-4.5"),
            PriceChoice::Cheapest,
        )
        .expect("priced");
        assert_eq!(
            sonnet.price,
            Price {
                input: dec!(3),
                output: dec!(15),
                cache_read: Some(dec!(0.3)),
                cache_write: Some(dec!(3.75))
            }
        );
        assert!(sonnet.vision && sonnet.tools && sonnet.reasoning);
    }

    #[test]
    fn every_entry_that_cannot_serve_chat_at_a_price_says_why() {
        let models = fixture();
        for (name, why) in [
            ("openai/gpt-4o-mini-tts", Skip::NotChat("audio".to_owned())),
            ("openai/gpt-image-1", Skip::NotChat("image".to_owned())),
            ("google/veo-3", Skip::NotChat("video".to_owned())),
            (
                "mistral/mistral-large-2407",
                Skip::Unavailable("deprecated".to_owned()),
            ),
            ("openai/o3-deep-research", Skip::AccessRequired),
            ("meta/llama-4-scout", Skip::NoPrice),
            ("moonshot/kimi-k2-free", Skip::Free),
        ] {
            for choice in [PriceChoice::Cheapest, PriceChoice::First] {
                assert_eq!(
                    choose(listed(&models, name), choice),
                    Err(why.clone()),
                    "{name}"
                );
            }
        }
        assert_eq!(models.len(), 10, "every fixture entry was read");
    }

    #[test]
    fn a_nameless_or_vendorless_entry_is_skipped() {
        let nameless = listed_model(&serde_json::json!({"display_name": "x", "vendors": {}}));
        assert_eq!(choose(&nameless, PriceChoice::Cheapest), Err(Skip::Unnamed));
        let spaced = listed_model(&serde_json::json!({"model": "two words", "vendors": {}}));
        assert_eq!(choose(&spaced, PriceChoice::Cheapest), Err(Skip::Unnamed));
        let bare = listed_model(&serde_json::json!({"model": "m", "vendors": {}}));
        assert_eq!(choose(&bare, PriceChoice::Cheapest), Err(Skip::NoVendor));
    }

    #[test]
    fn a_tie_goes_to_the_vendor_listed_first() {
        let tied = listed_model(&serde_json::json!({
            "model": "t/m",
            "vendors": {
                "b": {"capabilities": {"input": ["text"], "output": ["text"]},
                      "pricing": {"input_per_million": 1, "output_per_million": 2}},
                "a": {"capabilities": {"input": ["text"], "output": ["text"]},
                      "pricing": {"input_per_million": 2, "output_per_million": 1}}
            }
        }));
        assert_eq!(
            choose(&tied, PriceChoice::Cheapest).expect("priced").vendor,
            "b"
        );
    }

    #[test]
    fn a_price_is_exact_rounded_as_stored_and_never_negative() {
        assert_eq!(money(&serde_json::json!(0.075)), Some(dec!(0.075)));
        assert_eq!(money(&serde_json::json!("0.0375")), Some(dec!(0.0375)));
        assert_eq!(money(&serde_json::json!(0.000_000_1)), Some(dec!(0)));
        assert_eq!(
            money(&serde_json::json!(0.000_000_5)),
            Some(dec!(0.000001)),
            "half away from zero, as numeric(12,6) rounds it"
        );
        assert_eq!(money(&serde_json::json!(-1)), None);
        assert_eq!(money(&serde_json::json!(1_000_000)), None);
        assert_eq!(money(&serde_json::json!(null)), None);
        assert_eq!(money(&serde_json::json!("free")), None);
    }

    #[test]
    fn ids_come_from_each_dialects_list_and_uncallable_entries_are_left_out() {
        let openai = serde_json::json!({"object": "list", "data": [
            {"id": "gpt-5", "object": "model"}, {"id": "o3", "object": "model"}
        ]});
        assert_eq!(page_ids(&openai).unwrap(), ["gpt-5", "o3"]);

        let gemini = serde_json::json!({"models": [
            {"name": "models/gemini-2.5-pro"}, {"name": "models/gemini-2.5-flash"}
        ]});
        assert_eq!(
            page_ids(&gemini).unwrap(),
            ["gemini-2.5-pro", "gemini-2.5-flash"]
        );

        let merge = page(PAGE_1);
        assert_eq!(
            page_ids(&merge).unwrap(),
            [
                "zai/glm-5.3-flash",
                "anthropic/claude-sonnet-4.5",
                "openai/gpt-4o-mini-tts",
                "openai/gpt-image-1",
                "google/veo-3",
            ],
            "named by `model`, and the deprecated entry is not callable"
        );
        assert_eq!(
            page_ids(&page(PAGE_2)).unwrap(),
            [
                "deepseek/deepseek-v3.2",
                "meta/llama-4-scout",
                "moonshot/kimi-k2-free"
            ],
            "an entry that needs access is not callable either"
        );

        assert_eq!(
            page_ids(&serde_json::json!({"data": []})).unwrap(),
            Vec::<String>::new()
        );
        assert!(page_ids(&serde_json::json!({"data": [{"object": "model"}]})).is_err());
        assert!(page_ids(&serde_json::json!({"error": "nope"})).is_err());
    }

    #[test]
    fn each_dialect_keeps_its_list_where_its_api_does() {
        let headers = ExtraHeaders::default();
        let at = |dialect, base| {
            source(dialect, base, &headers)
                .list_url()
                .map(|u| u.to_string())
        };
        assert_eq!(
            at(Dialect::OpenAIChatCompletions, "https://h.example/v1").unwrap(),
            "https://h.example/v1/models"
        );
        assert_eq!(
            at(Dialect::AnthropicMessages, "https://h.example").unwrap(),
            "https://h.example/v1/models?limit=1000"
        );
        assert_eq!(
            at(Dialect::GeminiGenerateContent, "https://h.example/v1beta").unwrap(),
            "https://h.example/v1beta/models?pageSize=1000"
        );
        assert!(at(Dialect::SystemOne, "https://h.example").is_err());
    }

    #[test]
    fn a_priced_list_is_looked_for_on_the_dialects_path_then_at_the_origin() {
        let headers = ExtraHeaders::default();
        let derived_at = |dialect, base| -> Vec<String> {
            derived(&source(dialect, base, &headers))
                .unwrap()
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert_eq!(
            derived_at(
                Dialect::OpenAIChatCompletions,
                "https://api-gateway.merge.dev/v1/openai"
            ),
            [
                "https://api-gateway.merge.dev/v1/openai/models",
                "https://api-gateway.merge.dev/v1/models"
            ]
        );
        assert_eq!(
            derived_at(
                Dialect::AnthropicMessages,
                "https://api-gateway.merge.dev/v1/anthropic"
            ),
            [
                "https://api-gateway.merge.dev/v1/anthropic/v1/models",
                "https://api-gateway.merge.dev/v1/models"
            ]
        );
        assert_eq!(
            derived_at(Dialect::OpenAIChatCompletions, "https://h.example/v1"),
            ["https://h.example/v1/models"],
            "tried once when the two are the same"
        );
    }

    #[test]
    fn a_listing_url_must_be_where_the_endpoint_answers() {
        let headers = ExtraHeaders::default();
        let merge = source(
            Dialect::OpenAIChatCompletions,
            "https://api-gateway.merge.dev/v1/openai",
            &headers,
        );
        assert_eq!(
            on_origin(
                &merge,
                " https://api-gateway.merge.dev/v1/models?limit=100 "
            )
            .unwrap()
            .as_str(),
            "https://api-gateway.merge.dev/v1/models?limit=100"
        );
        for elsewhere in [
            "https://evil.example/v1/models",
            "http://api-gateway.merge.dev/v1/models",
            "https://api-gateway.merge.dev:8443/v1/models",
        ] {
            let err = on_origin(&merge, elsewhere).expect_err(elsewhere);
            assert!(err.to_string().contains("origin"), "{elsewhere}: {err}");
        }
        assert!(on_origin(&merge, "https://u:p@api-gateway.merge.dev/v1/models").is_err());
    }

    #[test]
    fn a_page_says_how_to_ask_for_the_next_one() {
        let url = Url::parse("https://h.example/v1/models").unwrap();
        let next = |page: Value| next_page(&page, &url).unwrap();
        assert_eq!(
            next(serde_json::json!({"has_more": true, "next_cursor": "c2"})),
            Some(("cursor", "c2".to_owned()))
        );
        assert_eq!(
            next(serde_json::json!({"has_more": true, "last_id": "m9", "first_id": "m0"})),
            Some(("after_id", "m9".to_owned()))
        );
        assert_eq!(
            next(serde_json::json!({"nextPageToken": "t2"})),
            Some(("pageToken", "t2".to_owned()))
        );
        assert_eq!(
            next(serde_json::json!({"has_more": false, "next_cursor": "c2"})),
            None
        );
        assert_eq!(next(serde_json::json!({"data": []})), None);
        assert_eq!(next(serde_json::json!({"nextPageToken": ""})), None);
        assert!(next_page(&serde_json::json!({"has_more": true}), &url).is_err());

        let paged = with_param(
            &Url::parse("https://h.example/m?limit=500&cursor=old").unwrap(),
            "cursor",
            "new/+",
        );
        assert_eq!(
            paged.as_str(),
            "https://h.example/m?limit=500&cursor=new%2F%2B"
        );
    }

    /// A key `served` reads the list with: never a real one.
    const KEY: &str = "listing-test-key";

    #[tokio::test]
    async fn discovery_reads_every_page_with_the_endpoints_key_and_headers() {
        let server = MockServer::start().await;
        let headers = ExtraHeaders::parse([("x-team", "t6")]).unwrap();
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param_is_missing("after_id"))
            .and(query_param("limit", "1000"))
            .and(header("authorization", "Bearer listing-test-key"))
            .and(header(
                "anthropic-version",
                oag_proto::anthropic::API_VERSION,
            ))
            .and(header("x-team", "t6"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"type": "model", "id": "claude-a"}, {"type": "model", "id": "claude-b"}],
                "has_more": true, "first_id": "claude-a", "last_id": "claude-b"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param("after_id", "claude-b"))
            .and(query_param("limit", "1000"))
            .and(header("authorization", "Bearer listing-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"type": "model", "id": "claude-c"}, {"type": "model", "id": "claude-a"}],
                "has_more": false, "last_id": "claude-a"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base = server.uri();
        let answer = served(
            &source(Dialect::AnthropicMessages, &base, &headers),
            KEY,
            None,
        )
        .await
        .expect("two pages");
        assert_eq!(answer.models, ["claude-a", "claude-b", "claude-c"]);
        assert_eq!(answer.pages, 2);
        server.verify().await;
    }

    #[tokio::test]
    async fn a_list_that_never_ends_or_repeats_itself_is_not_used() {
        let server = MockServer::start().await;
        let headers = ExtraHeaders::default();
        // Every page names a fresh cursor, forever.
        Mock::given(method("GET"))
            .and(path("/v1beta/models"))
            .respond_with(|req: &wiremock::Request| {
                let n = req
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "pageToken")
                    .map_or(0, |(_, v)| v.parse::<u32>().unwrap_or(0));
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "models": [{"name": format!("models/m-{n}")}],
                    "nextPageToken": (n + 1).to_string()
                }))
            })
            .expect(u64::try_from(MAX_PAGES).unwrap())
            .mount(&server)
            .await;
        let base = format!("{}/v1beta", server.uri());
        let err = served(
            &source(Dialect::GeminiGenerateContent, &base, &headers),
            KEY,
            None,
        )
        .await
        .expect_err("past the bound");
        assert!(err.to_string().contains("runs past 20 pages"), "{err}");
        server.verify().await;

        let looping = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "m"}], "has_more": true, "next_cursor": "same"
            })))
            .expect(2)
            .mount(&looping)
            .await;
        let base = looping.uri();
        let err = served(
            &source(Dialect::OpenAIChatCompletions, &base, &headers),
            KEY,
            None,
        )
        .await
        .expect_err("a cursor that does not move");
        assert!(err.to_string().contains("same next page twice"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_or_unreadable_list_is_an_error_and_an_empty_one_is_an_answer() {
        let headers = ExtraHeaders::default();
        for (status, body, ok) in [
            (500, serde_json::json!({"error": "boom"}), None),
            (200, serde_json::json!({"detail": "not a list"}), None),
            (
                200,
                serde_json::json!({"data": []}),
                Some(Vec::<String>::new()),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&server)
                .await;
            let base = format!("{}/v1", server.uri());
            let got = served(
                &source(Dialect::OpenAIChatCompletions, &base, &headers),
                KEY,
                None,
            )
            .await
            .map(|s| s.models);
            if let Some(models) = ok {
                assert_eq!(got.expect("an answer"), models);
            } else {
                let err = got.expect_err("no answer");
                if status == 500 {
                    assert!(err.to_string().contains("500"), "{err}");
                    assert!(!err.to_string().contains(KEY), "{err}");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_priced_list_is_found_at_the_origin_and_read_across_its_pages() {
        let server = MockServer::start().await;
        let headers = ExtraHeaders::default();
        Mock::given(method("GET"))
            .and(path("/v1/openai/models"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param("limit", "500"))
            .and(query_param_is_missing("cursor"))
            .and(header("authorization", "Bearer listing-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE_1, "application/json"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param("limit", "500"))
            .and(query_param(
                "cursor",
                "eyJhZnRlciI6Im1pc3RyYWwvbWlzdHJhbC1sYXJnZS0yNDA3In0",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE_2, "application/json"))
            .expect(1)
            .mount(&server)
            .await;

        let base = format!("{}/v1/openai", server.uri());
        let listing = priced(
            &source(Dialect::OpenAIChatCompletions, &base, &headers),
            None,
            KEY,
            None,
        )
        .await
        .expect("found at the origin");
        assert_eq!(listing.url, format!("{}/v1/models?limit=500", server.uri()));
        assert_eq!(listing.pages, 2);
        assert_eq!(listing.models, fixture());
        server.verify().await;
    }

    #[tokio::test]
    async fn an_id_only_list_is_refused_and_names_the_way_to_add_a_model() {
        let server = MockServer::start().await;
        let headers = ExtraHeaders::default();
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list", "data": [{"id": "m-1", "object": "model"}]
            })))
            .mount(&server)
            .await;
        let base = format!("{}/v1", server.uri());
        let err = priced(
            &source(Dialect::OpenAIChatCompletions, &base, &headers),
            None,
            KEY,
            None,
        )
        .await
        .expect_err("no prices");
        let message = err.to_string();
        assert!(message.contains("without prices"), "{message}");
        assert!(message.contains("oag admin catalog add"), "{message}");
    }

    #[tokio::test]
    async fn a_given_listing_url_is_the_only_one_read() {
        let server = MockServer::start().await;
        let headers = ExtraHeaders::default();
        Mock::given(method("GET"))
            .and(path("/catalog"))
            .and(query_param("limit", "7"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE_2, "application/json"))
            .expect(1)
            .mount(&server)
            .await;
        let base = format!("{}/v1/openai", server.uri());
        let listing = priced(
            &source(Dialect::OpenAIChatCompletions, &base, &headers),
            Some(&format!("{}/catalog?limit=7", server.uri())),
            KEY,
            None,
        )
        .await
        .expect("read where it was pointed");
        assert_eq!(listing.pages, 1);
        assert_eq!(listing.models.len(), 4);
        server.verify().await;
    }
}
