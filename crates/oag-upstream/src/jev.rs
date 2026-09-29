//! Jev, the upstream behind System One.
//!
//! Not a [`ProviderAdapter`](crate::ProviderAdapter). That trait is the chat
//! contract — build from a canonical request, parse a stream of events — and
//! System One has neither: one question set goes out as JSON, one answer set
//! comes back. So this is only the two requests, built for the same transport
//! every other upstream call goes through, which is what puts the credential's
//! proxy, connection pool and breaker in front of Jev too.
//!
//! The paths are the SDK's constants rather than a second spelling of them, so
//! the gateway calls Jev exactly where `typesafe_sdk::Client` would.

use oag_core::Result;
use oag_core::credential::SecretMaterial;
use typesafe_sdk::wire::{MODELS_PATH, SYSTEM_ONE_PATH};

/// Where Jev answers when `gateway.provider_base_urls.jev` is unset: the
/// SDK's own default, so the two cannot disagree about it.
pub const DEFAULT_BASE_URL: &str = typesafe_sdk::DEFAULT_BASE_URL;

/// Builds the requests a Jev credential is used for.
#[derive(Debug, Clone)]
pub struct JevUpstream {
    /// Already normalised by whoever configured it: no trailing slash, because
    /// the paths are appended to it.
    base_url: String,
}

impl JevUpstream {
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    /// `POST {base}/v1/systemone`, carrying the caller's body as it arrived.
    ///
    /// The body is the client's own bytes rather than a re-serialisation of
    /// what the gateway parsed: the gateway only checks it, and anything it
    /// wrote instead could only differ from what the client meant.
    pub fn system_one(
        &self,
        credential: &SecretMaterial,
        body: impl Into<reqwest::Body>,
    ) -> Result<reqwest::Request> {
        crate::builder_client()?
            .post(format!("{}{SYSTEM_ONE_PATH}", self.base_url))
            .bearer_auth(&credential.access_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .build()
            .map_err(|e| oag_core::Error::Internal(format!("building a System One request: {e}")))
    }

    /// `GET {base}/v1/models`: what this credential can ask.
    pub fn models(&self, credential: &SecretMaterial) -> Result<reqwest::Request> {
        crate::builder_client()?
            .get(format!("{}{MODELS_PATH}", self.base_url))
            .bearer_auth(&credential.access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|e| oag_core::Error::Internal(format!("building a Jev models request: {e}")))
    }
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
}
