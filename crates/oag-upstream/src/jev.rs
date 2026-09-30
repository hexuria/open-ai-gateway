//! Jev, the upstream behind System One, and every host that serves its shape.
//!
//! Not a [`ProviderAdapter`](crate::ProviderAdapter). That trait is the chat
//! contract — build from a canonical request, parse a stream of events — and
//! System One has neither: one question set goes out as JSON, one answer set
//! comes back. So this is only the two requests, built for the same transport
//! every other upstream call goes through, which is what puts the credential's
//! proxy, connection pool and breaker in front of Jev too.
//!
//! The built-in Jev is posted at the SDK's own paths rather than a second
//! spelling of them, so the gateway calls Jev exactly where
//! `typesafe_sdk::Client` would. An operator's System One endpoint
//! ([`crate::custom::system_one`]) is the same two requests at its own base
//! URL, with its key in the header it was registered with, its extra headers,
//! and, where it names one, its own path for the question set: Merge Gateway
//! takes the same request and answer at `/v1/decisions`.

use crate::custom::{ExtraHeaders, authenticate};
use oag_core::credential::SecretMaterial;
use oag_core::provider::AuthStyle;
use oag_core::{Error, Result};
use reqwest::RequestBuilder;
use serde_json::{Map, Value};
use typesafe_sdk::wire::{MODELS_PATH, ModelMetadata, SYSTEM_ONE_PATH};

/// Where Jev answers when `gateway.provider_base_urls.jev` is unset: the
/// SDK's own default, so the two cannot disagree about it.
pub const DEFAULT_BASE_URL: &str = typesafe_sdk::DEFAULT_BASE_URL;

/// How many pages of one host's model listing a listing reads. Eight pages of
/// [`PAGE_LIMIT`] is four thousand models; Merge Gateway lists about three
/// hundred.
pub const MAX_LISTING_PAGES: usize = 8;

/// The page size asked of an endpoint's listing: the most Merge Gateway
/// allows, which answers a larger `limit` with 422. A host that does not page
/// ignores it.
const PAGE_LIMIT: &str = "500";

/// Builds the requests a System One credential is used for.
#[derive(Debug, Clone)]
pub struct JevUpstream {
    /// Already normalised by whoever configured it: no trailing slash, because
    /// the paths are appended to it.
    base_url: String,
    /// Where a question set is posted, beneath `base_url`.
    path: String,
    auth: AuthStyle,
    extra_headers: ExtraHeaders,
}

impl JevUpstream {
    /// The built-in Jev at `base_url`: the SDK's paths, the key as a bearer
    /// token, and no other header.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            path: SYSTEM_ONE_PATH.to_owned(),
            auth: AuthStyle::Bearer,
            extra_headers: ExtraHeaders::default(),
        }
    }

    /// An endpoint's: see [`crate::custom::system_one`], the one caller.
    pub(crate) fn for_endpoint(
        base_url: String,
        path: String,
        auth: AuthStyle,
        extra_headers: ExtraHeaders,
    ) -> Self {
        Self {
            base_url,
            path,
            auth,
            extra_headers,
        }
    }

    /// `POST {base}{path}`, carrying the caller's body as it arrived. The path
    /// is `/v1/systemone` unless an endpoint names its own.
    ///
    /// The body is the client's own bytes rather than a re-serialisation of
    /// what the gateway parsed: the gateway only checks it, and anything it
    /// wrote instead could only differ from what the client meant.
    pub fn system_one(
        &self,
        credential: &SecretMaterial,
        body: impl Into<reqwest::Body>,
    ) -> Result<reqwest::Request> {
        let builder = self
            .authorised(
                crate::builder_client()?.post(format!("{}{}", self.base_url, self.path)),
                credential,
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body);
        self.extra_headers
            .apply(builder)
            .build()
            .map_err(|e| Error::Internal(format!("building a System One request: {e}")))
    }

    /// `GET {base}/v1/models`: what this credential can ask.
    pub fn models(&self, credential: &SecretMaterial) -> Result<reqwest::Request> {
        self.listing(credential, &[])
    }

    /// One page of an endpoint's listing: `GET {base}/v1/models` with the most
    /// a page may hold, and the cursor its previous page named.
    ///
    /// The built-in Jev's listing is not paged, and is asked with
    /// [`JevUpstream::models`]; this is for a host that pages, as Merge Gateway
    /// does, fifty to a page unless asked for more.
    pub fn models_page(
        &self,
        credential: &SecretMaterial,
        cursor: Option<&str>,
    ) -> Result<reqwest::Request> {
        match cursor {
            Some(cursor) => self.listing(credential, &[("limit", PAGE_LIMIT), ("cursor", cursor)]),
            None => self.listing(credential, &[("limit", PAGE_LIMIT)]),
        }
    }

    fn listing(
        &self,
        credential: &SecretMaterial,
        query: &[(&str, &str)],
    ) -> Result<reqwest::Request> {
        let mut builder = crate::builder_client()?.get(format!("{}{MODELS_PATH}", self.base_url));
        if !query.is_empty() {
            builder = builder.query(query);
        }
        let builder = self
            .authorised(builder, credential)
            .header(reqwest::header::ACCEPT, "application/json");
        self.extra_headers
            .apply(builder)
            .build()
            .map_err(|e| Error::Internal(format!("building a System One models request: {e}")))
    }

    /// `builder` with the key where this upstream takes it.
    ///
    /// A bearer goes through `bearer_auth`, as the built-in Jev's always has,
    /// which also marks the header sensitive; every other style through the
    /// one function every endpoint's adapter uses.
    fn authorised(&self, builder: RequestBuilder, credential: &SecretMaterial) -> RequestBuilder {
        match self.auth {
            AuthStyle::Bearer => builder.bearer_auth(&credential.access_token),
            other => authenticate(builder, other, &credential.access_token),
        }
    }
}

/// One page of a System One host's model listing, in the SDK's shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ListingPage {
    /// Each model the page lists that answers System One questions, by the
    /// host's own name for it.
    pub models: Vec<ModelMetadata>,
    /// The cursor for the next page, when the page says there is one.
    pub next: Option<String>,
}

/// A host's listing page, read in either of the two shapes a System One host
/// is known to list its models in, or why it is neither.
///
/// - Jev's own, the SDK's `ListModelsResponse`: `{"models": [{"name",
///   "description", "release_date"}]}`.
/// - Merge Gateway's, an `OpenAI`-style list: `{"object": "list", "data":
///   [...], "has_more", "next_cursor"}`, where each entry names its model in
///   `model`, itself in `display_name`, and what it can do per vendor, under
///   `vendors.<vendor>.capabilities`. That list holds every model the host
///   routes to, chat included, so an entry that says what it outputs is kept
///   only if one of the things it outputs is a `decision`. An entry that says
///   nothing about its output is kept, which is every entry of Jev's shape.
///
/// An entry with no name is left out rather than failing the page: a host's
/// listing holds what it holds, and one entry this gateway cannot name is no
/// reason to hide the rest.
pub fn listing_page(body: &[u8]) -> Result<ListingPage> {
    let page: Value = serde_json::from_slice(body)
        .map_err(|e| Error::Internal(format!("a System One listing that is not JSON: {e}")))?;
    let entries = ["models", "data"]
        .into_iter()
        .find_map(|key| page.get(key).and_then(Value::as_array))
        .ok_or_else(|| {
            Error::Internal(
                "a System One listing with neither a `models` nor a `data` list".to_owned(),
            )
        })?;
    let models = entries
        .iter()
        .filter_map(Value::as_object)
        .filter(|entry| decides(entry))
        .filter_map(listed)
        .collect();
    let next = if page.get("has_more") == Some(&Value::Bool(true)) {
        page.get("next_cursor")
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else {
        None
    };
    Ok(ListingPage { models, next })
}

/// Whether an entry answers System One questions: it says nothing about what
/// it outputs, or one of the things it says it outputs is a `decision`.
///
/// Read at the entry's top level and under each of its vendors, because Merge
/// Gateway reports capabilities per vendor.
fn decides(entry: &Map<String, Value>) -> bool {
    let vendors = entry
        .get("vendors")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(Map::values);
    let mut outputs = std::iter::once(entry)
        .map(|entry| entry.get("capabilities"))
        .chain(vendors.map(|vendor| vendor.get("capabilities")))
        .flatten()
        .filter_map(|capabilities| capabilities.get("output")?.as_array())
        .peekable();
    outputs.peek().is_none()
        || outputs.any(|output| output.iter().any(|kind| kind.as_str() == Some("decision")))
}

/// An entry as the SDK lists a model, if it names one. What either shape
/// lacks is the empty string, which is what the SDK's type holds for a field
/// it requires and a host did not send.
fn listed(entry: &Map<String, Value>) -> Option<ModelMetadata> {
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| entry.get(*key).and_then(Value::as_str))
    };
    let name = text(&["name", "model"])?;
    let released = text(&["release_date", "launch_date"]).or_else(|| {
        entry
            .get("vendors")
            .and_then(Value::as_object)?
            .values()
            .find_map(|vendor| vendor.get("launch_date").and_then(Value::as_str))
    });
    Some(ModelMetadata::new(
        name,
        text(&["description", "display_name"]).unwrap_or_default(),
        released.unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> SecretMaterial {
        SecretMaterial {
            access_token: "jev-key-1".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        }
    }

    fn header<'a>(request: &'a reqwest::Request, name: &str) -> &'a str {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_else(|| panic!("no {name} header"))
    }

    #[test]
    fn a_question_set_goes_to_the_system_one_path_with_the_jev_key() {
        let jev = JevUpstream::new("http://jev.internal/prefix");
        let body = br#"{"state":"x","questions":{"q":{"type":"noul"}}}"#;
        let request = jev
            .system_one(&credential(), body.to_vec())
            .expect("builds");

        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.url().as_str(),
            "http://jev.internal/prefix/v1/systemone",
            "the base URL's own path is kept: a proxy mounted under one works"
        );
        // Jev's key, as a bearer token. The caller's gateway key never gets
        // this far; that it is not what arrives upstream is asserted end to
        // end in the gateway's tests.
        assert_eq!(header(&request, "authorization"), "Bearer jev-key-1");
        assert_eq!(header(&request, "content-type"), "application/json");
        assert_eq!(header(&request, "accept"), "application/json");
        assert_eq!(
            request.body().and_then(reqwest::Body::as_bytes),
            Some(&body[..]),
            "the caller's bytes, not a re-serialisation of them"
        );
    }

    #[test]
    fn the_models_listing_is_a_get_with_the_same_key() {
        let request = JevUpstream::new("http://jev.internal")
            .models(&credential())
            .expect("builds");
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.url().as_str(), "http://jev.internal/v1/models");
        assert_eq!(header(&request, "authorization"), "Bearer jev-key-1");
        assert!(request.body().is_none());
    }

    #[test]
    fn unconfigured_jev_is_typesafe_s_own_api() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.typesafe.ai");
    }

    /// The built-in's requests carry these headers and no others, the key's
    /// marked sensitive as `bearer_auth` has always marked it. An endpoint's
    /// extra headers are the only way another one is added, and the built-in
    /// has none.
    #[test]
    fn the_built_in_sends_exactly_the_headers_it_always_has() {
        let jev = JevUpstream::new("http://jev.internal");
        let asked = jev.system_one(&credential(), "{}").expect("builds");
        let names: Vec<&str> = asked
            .headers()
            .keys()
            .map(reqwest::header::HeaderName::as_str)
            .collect();
        assert_eq!(names, ["authorization", "content-type", "accept"]);
        assert!(asked.headers()["authorization"].is_sensitive());

        let listed = jev.models(&credential()).expect("builds");
        let names: Vec<&str> = listed
            .headers()
            .keys()
            .map(reqwest::header::HeaderName::as_str)
            .collect();
        assert_eq!(names, ["authorization", "accept"]);
        assert_eq!(listed.url().query(), None, "and its listing is not paged");
    }

    /// An endpoint's page asks for the most a page may hold, and resumes from
    /// the cursor it was given, encoded as a query value.
    #[test]
    fn a_page_asks_for_the_most_a_page_holds_and_resumes_from_its_cursor() {
        let jev = JevUpstream::new("http://h.example");
        let first = jev.models_page(&credential(), None).expect("builds");
        assert_eq!(first.url().as_str(), "http://h.example/v1/models?limit=500");
        let next = jev
            .models_page(&credential(), Some("mistral/magistral medium"))
            .expect("builds");
        assert_eq!(
            next.url().as_str(),
            "http://h.example/v1/models?limit=500&cursor=mistral%2Fmagistral+medium"
        );
        assert_eq!(header(&next, "authorization"), "Bearer jev-key-1");
    }

    /// Jev's shape, entry for entry, and no next page.
    #[test]
    fn jev_s_own_listing_is_read_as_it_is() {
        let page = listing_page(
            br#"{"models":[
                {"name":"jev-latest","description":"Fast model","release_date":"2026-08-01"},
                {"name":"jev-1.13.0","description":"Pinned","release_date":"2026-07-01","extra":1}
            ]}"#,
        )
        .expect("a listing");
        assert_eq!(
            page,
            ListingPage {
                models: vec![
                    ModelMetadata::new("jev-latest", "Fast model", "2026-08-01"),
                    ModelMetadata::new("jev-1.13.0", "Pinned", "2026-07-01"),
                ],
                next: None,
            }
        );
    }

    /// Merge Gateway's shape: its decision models only, named by `model`,
    /// described by `display_name`, dated by a vendor's `launch_date`, with the
    /// cursor for the next page while it says there is one.
    #[test]
    fn a_merge_listing_keeps_its_decision_models_and_its_cursor() {
        let body = br#"{
            "object": "list",
            "data": [
                {"model": "openai/gpt-5.4", "provider": "openai", "display_name": "GPT-5.4",
                 "vendors": {"openai": {"launch_date": "2026-03-05",
                    "capabilities": {"input": ["text"], "output": ["text", "tool_use"]}}}},
                {"model": "typesafe/jev-1.13", "provider": "typesafe", "display_name": "Jev 1.13",
                 "vendors": {"typesafe": {"launch_date": "2026-07-01",
                    "capabilities": {"input": ["text"], "output": ["decision"]}}}},
                {"model": "acme/decider", "capabilities": {"output": ["decision"]}},
                {"display_name": "an entry with no model"}
            ],
            "has_more": true,
            "next_cursor": "typesafe/jev-1.13"
        }"#;
        let page = listing_page(body).expect("a listing");
        assert_eq!(
            page.models,
            [
                ModelMetadata::new("typesafe/jev-1.13", "Jev 1.13", "2026-07-01"),
                ModelMetadata::new("acme/decider", "", ""),
            ]
        );
        assert_eq!(page.next.as_deref(), Some("typesafe/jev-1.13"));

        let last =
            listing_page(br#"{"data":[],"has_more":false,"next_cursor":"x"}"#).expect("a listing");
        assert_eq!(
            last,
            ListingPage {
                models: vec![],
                next: None
            },
            "no more"
        );
        let unsaid = listing_page(br#"{"data":[],"next_cursor":"x"}"#).expect("a listing");
        assert_eq!(unsaid.next, None, "a cursor alone is not a promise of more");
    }

    /// Output is read at the top and under every vendor: one that decides is
    /// enough, and saying nothing is not saying no.
    #[test]
    fn an_entry_decides_unless_what_it_outputs_leaves_decisions_out() {
        let decides_json = |json: &str| {
            let entry: Map<String, Value> = serde_json::from_str(json).expect("an object");
            decides(&entry)
        };
        assert!(decides_json(r#"{"name":"jev-latest"}"#));
        assert!(decides_json(r#"{"capabilities":{"input":["text"]}}"#));
        assert!(decides_json(r#"{"capabilities":{"output":["decision"]}}"#));
        assert!(decides_json(
            r#"{"vendors":{"a":{"capabilities":{"output":["text"]}},
                           "b":{"capabilities":{"output":["text","decision"]}}}}"#
        ));
        assert!(!decides_json(r#"{"capabilities":{"output":["text"]}}"#));
        assert!(!decides_json(r#"{"capabilities":{"output":[]}}"#));
        assert!(!decides_json(
            r#"{"vendors":{"a":{"capabilities":{"output":["text","tool_use"]}}}}"#
        ));
    }

    #[test]
    fn a_body_that_lists_nothing_is_not_a_listing() {
        for body in [
            &b"not json"[..],
            b"[]",
            br#"{"models":{}}"#,
            br#"{"items":[]}"#,
        ] {
            assert!(
                listing_page(body).is_err(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }
}
