//! Google access tokens, minted from a service-account key.
//!
//! Vertex AI takes a bearer token, and the credential an operator holds for it
//! is a service-account JSON key. The key never goes to Vertex. It signs a
//! short-lived JWT, and Google's token endpoint trades that JWT for an access
//! token: the JWT-bearer grant of RFC 7523, as Google documents it for
//! server-to-server OAuth. [`GcpTokenCache`] does the trade and keeps each
//! account's token until five minutes before it expires.
//!
//! Nothing calls this yet. PR 10b wires it in through the adapter's
//! `prepare_credential` hook, which runs on the failover path just before the
//! request is built, so the Vertex adapter only ever sees the minted token.
//!
//! Each replica mints its own tokens, and that is safe. A mint consumes
//! nothing, unlike an OAuth refresh, so two replicas minting for one account
//! both end up holding working tokens. None of the fleet lock or the
//! compare-and-swap in the server's `gateway::refresh` applies.
//!
//! # Errors
//!
//! A failed mint is a failure of this credential, and the caller moves on to
//! another one. The failover path already does that for any error from
//! `ensure_fresh`, and PR 10b calls this from the same place. `oag_core::Error`
//! has no variant for a bad credential, so the variants follow the existing
//! refresh code. A key that cannot be read is [`Error::Config`], as a malformed
//! Bedrock credential is. A token endpoint that refuses, redirects or answers
//! with something that is not a token is [`Error::Internal`], as a failed Codex
//! or xAI refresh is.
//!
//! No error message and no `Debug` output contains the private key, the signed
//! assertion or an access token. Error messages are logged, and an assertion is
//! as good as the key for the hour it is valid: anyone holding it can trade it
//! for a token.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use oag_core::{AccountId, Error, Result};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

/// Google's OAuth 2.0 token endpoint.
///
/// Production mints here. Tests pass a stand-in, and PR 10b lets config
/// override it. A key's own `token_uri` never does; `ServiceAccountKey` says
/// why.
pub const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The one scope asked for. What the token may actually do is decided by the
/// service account's IAM roles, not by this string, and Vertex AI accepts it.
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The JWT-bearer grant (RFC 7523 §2.1).
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// How long a signed assertion is valid. Google refuses anything longer.
const ASSERTION_LIFETIME_SECS: i64 = 3600;

/// Mint again this long before the cached token expires, so no request sets
/// out with a token that dies on the way. The same margin `gateway::refresh`
/// gives an OAuth token.
const REFRESH_SKEW_SECS: i64 = 5 * 60;

/// How long one mint may take. Every caller for the account waits behind it,
/// so this bounds their wait as well as its own.
const MINT_TIMEOUT: Duration = Duration::from_secs(10);

/// The parts of a service-account JSON key that a mint needs.
///
/// `token_uri` is left out on purpose, and must stay out. The key file names a
/// token endpoint of its own, and following it would let whoever wrote the file
/// choose where this process posts a signed assertion and whose answer it
/// trusts as a token. Pointed at a metadata service or an internal port, that
/// is a request forgery made from inside the gateway (SSRF). The token URL is
/// the one the cache was built with, and nothing in the key can change it.
#[derive(Deserialize)]
struct ServiceAccountKey {
    #[serde(rename = "type")]
    kind: String,
    client_email: String,
    private_key_id: String,
    /// PKCS#8 PEM, `-----BEGIN PRIVATE KEY-----`.
    private_key: String,
}

impl ServiceAccountKey {
    fn from_json(json: &str) -> Result<Self> {
        let key: Self = serde_json::from_str(json).map_err(|e| {
            // Where, never what: serde quotes the offending value back, and a
            // key pasted as one JSON string would be quoted into the log whole.
            Error::Config(format!(
                "the service account key is not the JSON Google issues \
                 ({:?} error at line {}, column {})",
                e.classify(),
                e.line(),
                e.column()
            ))
        })?;
        if key.kind != "service_account" {
            return Err(Error::Config(
                "the credential's `type` is not `service_account`: only a service \
                 account key can mint a Google access token here"
                    .to_owned(),
            ));
        }
        Ok(key)
    }

    /// The signed JWT that is traded for an access token at `audience`.
    fn sign_assertion(&self, audience: &str, now: i64) -> Result<String> {
        let der = pkcs8_der(&self.private_key).ok_or_else(|| {
            Error::Config(
                "the service account's `private_key` is not a PKCS#8 PEM \
                 (`-----BEGIN PRIVATE KEY-----`)"
                    .to_owned(),
            )
        })?;
        // `KeyRejected` names the defect with a fixed description, never bytes
        // of the key, so it is safe to pass on.
        let key_pair = RsaKeyPair::from_pkcs8(&der).map_err(|e| {
            Error::Config(format!(
                "the service account's `private_key` is not a usable RSA key: {e}"
            ))
        })?;

        let header = serde_json::json!({
            "alg": "RS256",
            "typ": "JWT",
            "kid": &self.private_key_id,
        });
        let claims = serde_json::json!({
            "iss": &self.client_email,
            "scope": SCOPE,
            "aud": audience,
            "iat": now,
            "exp": now + ASSERTION_LIFETIME_SECS,
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );

        let mut signature = vec![0; key_pair.public().modulus_len()];
        key_pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| {
                Error::Internal("signing a service account assertion failed".to_owned())
            })?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

/// Never the key, and never a field that could hold it.
impl std::fmt::Debug for ServiceAccountKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAccountKey")
            .field("type", &self.kind)
            .field("client_email", &self.client_email)
            .field("private_key_id", &self.private_key_id)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

/// The DER inside a PKCS#8 PEM, or `None` if the text is not one.
fn pkcs8_der(pem: &str) -> Option<Vec<u8>> {
    let body = pem
        .trim()
        .strip_prefix("-----BEGIN PRIVATE KEY-----")?
        .strip_suffix("-----END PRIVATE KEY-----")?;
    let base64: String = body.split_ascii_whitespace().collect();
    STANDARD.decode(base64).ok()
}

/// A token endpoint's answer to a grant that succeeded (RFC 6749 §5.1).
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    token_type: String,
}

/// Why a token endpoint's answer was not a token, for the error message.
fn refusal(status: reqwest::StatusCode, body: &[u8]) -> String {
    if status.is_redirection() {
        return format!(
            "the Google token endpoint answered {status}, and the redirect was not \
             followed: a signed assertion goes to the configured token URL or nowhere"
        );
    }
    match oauth_error(body) {
        Some(code) => format!("the Google token endpoint answered {status} ({code})"),
        None => format!("the Google token endpoint answered {status}"),
    }
}

/// The error code in a token endpoint's refusal (RFC 6749 §5.2), if it is one
/// of the registered ones.
///
/// Only the code, and only a known one. The rest of the body is whatever the
/// endpoint chose to write, and this message is logged. A stand-in, a proxy or
/// a mistyped token URL can put anything there, the assertion it was just sent
/// included. `invalid_grant` is the one an operator sees most: the key or its
/// service account was deleted or disabled, or this host's clock is off by
/// enough that the assertion's `iat` looks wrong to Google.
fn oauth_error(body: &[u8]) -> Option<&'static str> {
    const CODES: [&str; 6] = [
        "invalid_request",
        "invalid_client",
        "invalid_grant",
        "unauthorized_client",
        "unsupported_grant_type",
        "invalid_scope",
    ];
    let body: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = body.get("error")?.as_str()?;
    CODES.into_iter().find(|known| *known == code)
}

/// One account's minted token.
struct Minted {
    access_token: String,
    /// Unix seconds.
    expires_at: i64,
}

/// A slot per account. At most one mint is in flight per account, because the
/// mint happens while the slot is locked.
type Slot = Arc<tokio::sync::Mutex<Option<Minted>>>;

/// Mints Google access tokens from service-account keys, and keeps them until
/// five minutes before they expire.
///
/// Keyed by account. An account's secret is not rewritten in place: a new key
/// is a new account row, with a new id. So a token cached under an id always
/// came from the key that id holds. A slot stays in the map after its account
/// is removed. That costs one small entry per account ever used, and there are
/// only as many of those as there are rows.
pub struct GcpTokenCache {
    token_url: String,
    client: reqwest::Client,
    /// Unix seconds. Injected so a test can move time instead of sleeping.
    clock: fn() -> i64,
    /// Held only to find or create a slot, never across an await.
    slots: std::sync::Mutex<HashMap<AccountId, Slot>>,
}

impl GcpTokenCache {
    /// A cache that mints at `token_url`, which is [`DEFAULT_TOKEN_URL`]
    /// outside tests.
    ///
    /// Redirects are not followed. A token endpoint that answers 3xx has
    /// failed the mint, because following the redirect would post the signed
    /// assertion to a host nobody configured.
    pub fn new(token_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(MINT_TIMEOUT)
            .build()
            .map_err(|e| Error::Internal(format!("building the Google token client: {e}")))?;
        Ok(Self {
            token_url: token_url.into(),
            client,
            clock: system_now,
            slots: std::sync::Mutex::default(),
        })
    }

    /// Read the time from `clock` (unix seconds) instead of the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    /// An access token for `account`, minted from `sa_json` (its
    /// service-account JSON key) unless a cached one has more than five
    /// minutes left.
    ///
    /// Concurrent callers for one account wait for the single mint in flight
    /// and then share its token. A mint that fails caches nothing, so each
    /// caller that was waiting behind it makes its own attempt in turn:
    /// serially, never concurrently, each bounded by the mint timeout. A caller
    /// that is cancelled mid-mint abandons it, and the next one starts over.
    pub async fn token(&self, account: AccountId, sa_json: &str) -> Result<String> {
        let slot = self.slot(account);
        let mut cached = slot.lock().await;
        // Read after the lock is taken: a caller that waited has to judge the
        // token the one before it minted against the time now, not the time
        // it started waiting.
        let now = (self.clock)();
        if let Some(minted) = cached
            .as_ref()
            .filter(|m| now < m.expires_at - REFRESH_SKEW_SECS)
        {
            return Ok(minted.access_token.clone());
        }

        let key = ServiceAccountKey::from_json(sa_json)?;
        let assertion = key.sign_assertion(&self.token_url, now)?;
        let (access_token, expires_in) = self.exchange(&assertion).await?;
        // Counted from before the request went out, so the recorded expiry is
        // never later than the real one.
        *cached = Some(Minted {
            access_token: access_token.clone(),
            expires_at: now.saturating_add_unsigned(expires_in),
        });
        Ok(access_token)
    }

    fn slot(&self, account: AccountId) -> Slot {
        // A panic cannot leave the map half-written: it is only ever read and
        // inserted into. So a poisoned lock is recovered, not given up on.
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(slots.entry(account).or_default())
    }

    /// Trade a signed assertion for an access token and its lifetime in seconds.
    async fn exchange(&self, assertion: &str) -> Result<(String, u64)> {
        let response = self
            .client
            .post(&self.token_url)
            .form(&[("grant_type", GRANT_TYPE), ("assertion", assertion)])
            .send()
            .await
            .map_err(|e| Error::Internal(format!("the Google token endpoint: {e}")))?;
        let status = response.status();
        let body = response.bytes().await;
        if !status.is_success() {
            // The status is the finding; a body that failed to arrive only
            // costs the error code that might have come with it.
            return Err(Error::Internal(refusal(status, &body.unwrap_or_default())));
        }
        let body = body.map_err(|e| Error::Internal(format!("the Google token endpoint: {e}")))?;

        // Where, never what: the body holds the access token, and serde quotes
        // the value it choked on.
        let token: TokenResponse = serde_json::from_slice(&body).map_err(|e| {
            Error::Internal(format!(
                "the Google token endpoint answered {status} with something other \
                 than a token ({:?} error at line {}, column {})",
                e.classify(),
                e.line(),
                e.column()
            ))
        })?;
        // The caller sends it as `Authorization: Bearer`, which is only right
        // for a bearer token. The type is case-insensitive (RFC 6749 §5.1).
        if !token.token_type.eq_ignore_ascii_case("bearer") {
            return Err(Error::Internal(
                "the Google token endpoint issued a token that is not a bearer token".to_owned(),
            ));
        }
        Ok((token.access_token, token.expires_in))
    }
}

/// Never a token: the tokens are the one thing here worth stealing.
impl std::fmt::Debug for GcpTokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpTokenCache")
            .field("token_url", &self.token_url)
            .finish_non_exhaustive()
    }
}

fn system_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests;
