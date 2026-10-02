//! Google access tokens, minted from a service-account key.
//!
//! Vertex AI takes a bearer token, and the credential an operator holds for it
//! is a service-account JSON key. The key never goes to Vertex. It signs a
//! short-lived JWT, and Google's token endpoint trades that JWT for an access
//! token: the JWT-bearer grant of RFC 7523, as Google documents it for
//! server-to-server OAuth. [`GcpTokenCache`] does the trade and keeps each
//! account's token until five minutes before it expires.
//!
//! The Vertex adapter (`crate::vertex`) calls it from the adapter's
//! `prepare_credential` hook, which runs on the failover path just before the
//! request is built, so the request is only ever built with the minted token.
//! The gateway holds one cache for every `gcp` endpoint, and it outlives the
//! reloads that rebuild their adapters.
//!
//! Each replica mints its own tokens, and that is safe. A mint consumes
//! nothing, unlike an OAuth refresh, so two replicas minting for one account
//! both end up holding working tokens. None of the fleet lock or the
//! compare-and-swap in the server's `gateway::refresh` applies.
//!
//! # Errors
//!
//! A failed mint is a failure of this credential, and the caller moves on to
//! another one. The failover path does that for any error from `ensure_fresh`
//! or `prepare_credential`, and this is called from the second. Every failure
//! is [`Error::UpstreamUnavailable`], which the client is told as 503, not the
//! 500 of a gateway that broke. Its `lasting` says whether the credential
//! itself is at fault (a key that cannot be read or signed with, or one the
//! token endpoint refused with a 4xx) or only the way to Google (a transport
//! error, a 5xx, a 429, a redirect, an answer that is not a token), and the
//! failover path cools the credential down for as long as that says: ten
//! minutes, or thirty seconds.
//!
//! A failure is remembered per account for fifteen seconds, behind the same
//! lock as the token. The callers that waited on a mint that failed, and those
//! that arrive just after it, are handed its error rather than each sent to
//! the token endpoint to be told the same thing.
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
/// Production mints here. Tests pass a stand-in, and `gateway.gcp_token_url`
/// overrides it, whose default this is. A key's own `token_uri` never does;
/// `ServiceAccountKey` says why.
pub const DEFAULT_TOKEN_URL: &str = oag_core::config::DEFAULT_GCP_TOKEN_URL;

/// The committed test-only RSA key (`tests/fixtures/README.md`), PKCS#8 PEM,
/// for a test to build a service-account key around, in this crate or, with
/// `test-fixtures`, in another. No release build carries it. It guards
/// nothing.
#[cfg(any(test, feature = "test-fixtures"))]
pub const TEST_KEY_PEM: &str = include_str!("../tests/fixtures/gcp-test-key.pem");

/// The one scope asked for. What the token may actually do is decided by the
/// service account's IAM roles, not by this string, and Vertex AI accepts it.
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The JWT-bearer grant (RFC 7523 §2.1).
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// Who every assertion is addressed to, its `aud`, wherever it is posted.
///
/// Google's documentation fixes it: "When making an access token request
/// this value is always `https://oauth2.googleapis.com/token`"
/// (<https://developers.google.com/identity/protocols/oauth2/service-account>).
/// The token URL can be another, a stand-in or a proxy, and an assertion
/// addressed to that is one Google refuses.
const AUDIENCE: &str = "https://oauth2.googleapis.com/token";

/// How long a signed assertion is valid. Google refuses anything longer.
const ASSERTION_LIFETIME_SECS: i64 = 3600;

/// How far back an assertion is dated. A host whose clock runs ahead of
/// Google's would otherwise sign an `iat` Google reads as the future, and be
/// refused `invalid_grant` ("Token must be a short-lived token (60 minutes)
/// and in a reasonable timeframe"). Google's own Go library dates its
/// assertions back for the same reason.
const CLOCK_SKEW_SECS: i64 = 30;

/// Mint again this long before the cached token expires, so no request sets
/// out with a token that dies on the way. The same margin `gateway::refresh`
/// gives an OAuth token.
const REFRESH_SKEW_SECS: i64 = 5 * 60;

/// How long one mint may take. Every caller for the account waits behind it,
/// so this bounds their wait as well as its own.
const MINT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a failed mint is remembered: its error is handed to every caller
/// for the account in that time, with no request to the token endpoint.
const FAILURE_TTL_SECS: i64 = 15;

/// How long a token must still have to be handed out when the mint that
/// should have replaced it failed: long enough for a request to set out with
/// it and be answered.
const LAST_RESORT_SECS: i64 = 30;

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
        // The assertion's issuer. Google refuses one with none, and saying so
        // here costs no round trip.
        if key.client_email.trim().is_empty() {
            return Err(Error::Config(
                "the service account key has an empty `client_email`".to_owned(),
            ));
        }
        Ok(key)
    }

    /// The RSA key the assertion is signed with, from `private_key`.
    fn key_pair(&self) -> Result<RsaKeyPair> {
        let der = pkcs8_der(&self.private_key).ok_or_else(|| {
            Error::Config(
                "the service account's `private_key` is not a PKCS#8 PEM \
                 (`-----BEGIN PRIVATE KEY-----`)"
                    .to_owned(),
            )
        })?;
        // `KeyRejected` names the defect with a fixed description, never bytes
        // of the key, so it is safe to pass on.
        RsaKeyPair::from_pkcs8(&der).map_err(|e| {
            Error::Config(format!(
                "the service account's `private_key` is not a usable RSA key: {e}"
            ))
        })
    }

    /// The signed JWT that is traded for an access token, addressed to
    /// [`AUDIENCE`], issued [`CLOCK_SKEW_SECS`] before `now` and valid for the
    /// hour Google allows from then.
    fn sign_assertion(&self, now: i64) -> Result<String> {
        let key_pair = self.key_pair()?;
        let issued = now - CLOCK_SKEW_SECS;

        let header = serde_json::json!({
            "alg": "RS256",
            "typ": "JWT",
            "kid": &self.private_key_id,
        });
        let claims = serde_json::json!({
            "iss": &self.client_email,
            "scope": SCOPE,
            "aud": AUDIENCE,
            "iat": issued,
            "exp": issued + ASSERTION_LIFETIME_SECS,
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

/// The email of the service account `sa_json` is a key for, if it is a key a
/// mint can sign with: Google's JSON for a service account, whose
/// `private_key` is an RSA key in PKCS#8 PEM.
///
/// What `oag admin account add` asks before it seals a key for a `gcp`
/// endpoint, so a key that could never mint is refused when it is filed
/// rather than on every request after. It reads the key exactly as a mint
/// does, so the two cannot disagree, and it sends nothing anywhere: whether
/// Google will take the key is the first mint's to find out. The email is no
/// secret. No error quotes the key.
pub fn check_key(sa_json: &str) -> Result<String> {
    let key = ServiceAccountKey::from_json(sa_json)?;
    key.key_pair()?;
    Ok(key.client_email)
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
///
/// `assertion` is the one just sent, which nothing quoted may hold.
fn refusal(status: reqwest::StatusCode, body: &[u8], assertion: &str) -> String {
    if status.is_redirection() {
        return format!(
            "the Google token endpoint answered {status}, and the redirect was not \
             followed: a signed assertion goes to the configured token URL or nowhere"
        );
    }
    match oauth_error(body, assertion) {
        Some((code, Some(description))) => {
            format!("the Google token endpoint answered {status} ({code}: {description})")
        }
        Some((code, None)) => format!("the Google token endpoint answered {status} ({code})"),
        None => format!("the Google token endpoint answered {status}"),
    }
}

/// The error code in a token endpoint's refusal (RFC 6749 §5.2), if it is one
/// of the registered ones, and its description, if that can be shown.
///
/// Only a known code. `invalid_grant` is the one an operator sees most: the
/// key or its service account was deleted or disabled, the signature is
/// wrong, or this host's clock is off by enough that the assertion's `iat`
/// looks wrong to Google, and Google's `error_description` is what tells
/// those apart ("Invalid JWT Signature."). So the description is kept, when
/// [`presentable`] says it can do no harm in a logged message: the rest of
/// the body is whatever the endpoint chose to write, and a stand-in, a proxy
/// or a mistyped token URL can write anything, the assertion it was just
/// sent included.
fn oauth_error(body: &[u8], assertion: &str) -> Option<(&'static str, Option<String>)> {
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
    let code = CODES.into_iter().find(|known| *known == code)?;
    let description = body
        .get("error_description")
        .and_then(serde_json::Value::as_str)
        .filter(|description| presentable(description, assertion))
        .map(str::to_owned);
    Some((code, description))
}

/// The longest description a message quotes: a line, as Google's are.
const DESCRIPTION_MAX: usize = 200;

/// Whether `description` can go in an error message: at most
/// [`DESCRIPTION_MAX`] bytes, all of them in the set RFC 6749 §5.2 allows a
/// description (printable ASCII but `"` and `\`), and none of `assertion`
/// in it — not the whole, and not any sixteen bytes of it, so an echo cut
/// short or a piece of the signature is caught as well as the rest.
fn presentable(description: &str, assertion: &str) -> bool {
    const WINDOW: usize = 16;
    let bytes = description.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= DESCRIPTION_MAX
        && bytes
            .iter()
            .all(|b| matches!(*b, 0x20..=0x21 | 0x23..=0x5B | 0x5D..=0x7E))
        && !assertion
            .as_bytes()
            .windows(WINDOW)
            .any(|piece| bytes.windows(WINDOW).any(|quoted| quoted == piece))
}

/// One account's minted token.
struct Minted {
    access_token: String,
    /// Unix seconds.
    expires_at: i64,
}

/// A mint that failed, kept so the callers behind it, and those that arrive
/// in the next [`FAILURE_TTL_SECS`], are told what it was told.
struct Failed {
    /// Unix seconds.
    at: i64,
    reason: String,
    lasting: bool,
}

impl Failed {
    /// `e`, as a mint that failed at `at`. A key that cannot be read or signed
    /// with (`Error::Config`) is lasting, and so is a refusal the exchange
    /// marked lasting; anything else is the way to Google, which may answer
    /// the next time.
    fn new(e: Error, at: i64) -> Self {
        match e {
            Error::UpstreamUnavailable { reason, lasting } => Self {
                at,
                reason,
                lasting,
            },
            Error::Config(reason) => Self {
                at,
                reason,
                lasting: true,
            },
            other => Self {
                at,
                reason: other.to_string(),
                lasting: false,
            },
        }
    }

    /// The error a caller is handed: a new one each time, as `Error` is not
    /// `Clone`.
    fn error(&self) -> Error {
        Error::UpstreamUnavailable {
            reason: self.reason.clone(),
            lasting: self.lasting,
        }
    }
}

/// What a slot holds for one account: its token, and the mint that last
/// failed.
#[derive(Default)]
struct Held {
    minted: Option<Minted>,
    failed: Option<Failed>,
}

impl Held {
    /// The token in hand, while it has more than [`LAST_RESORT_SECS`] left:
    /// past the point it is replaced, but still good to set out with.
    fn last_resort(&self, now: i64) -> Option<&Minted> {
        self.minted
            .as_ref()
            .filter(|m| now < m.expires_at - LAST_RESORT_SECS)
    }
}

/// A slot per account. At most one mint is in flight per account, because the
/// mint happens while the slot is locked.
type Slot = Arc<tokio::sync::Mutex<Held>>;

/// The way to Google failed, or Google did: not this credential's fault, and
/// the next attempt may well succeed.
fn unreachable(reason: String) -> Error {
    Error::UpstreamUnavailable {
        reason,
        lasting: false,
    }
}

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
    /// Unix seconds. Injected so a test can move time instead of sleeping.
    clock: fn() -> i64,
    /// Held only to find or create a slot, never across an await.
    slots: std::sync::Mutex<HashMap<AccountId, Slot>>,
}

impl GcpTokenCache {
    /// A cache that mints at `token_url`, which is [`DEFAULT_TOKEN_URL`]
    /// unless `gateway.gcp_token_url` says otherwise.
    ///
    /// Refused unless `token_url` is an https URL, or an http one to this
    /// machine's own loopback (a stand-in, a local proxy), so a mistyped one
    /// stops the gateway at startup instead of failing every Vertex request,
    /// and no signed assertion crosses a network in the clear: one is as good
    /// as the key for the hour it is valid.
    ///
    /// Redirects are not followed. A token endpoint that answers 3xx has
    /// failed the mint, because following the redirect would post the signed
    /// assertion to a host nobody configured.
    pub fn new(token_url: impl Into<String>) -> Result<Self> {
        let token_url = token_url.into();
        let url = reqwest::Url::parse(&token_url).map_err(|e| {
            Error::Config(format!(
                "the Google token URL {token_url:?} is not a URL: {e}"
            ))
        })?;
        if !oag_core::endpoint::is_https_or_loopback(&url) {
            return Err(Error::Config(format!(
                "the Google token URL {token_url:?} is neither https nor http to this \
                 machine's loopback: a signed assertion is as good as the key for an hour, \
                 and is not sent anywhere in the clear"
            )));
        }
        Ok(Self {
            token_url,
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
    /// A mint goes through `proxy`, the account's own `proxy_url`, when it has
    /// one: a credential's proxy applies to every call made with it, and this
    /// is one.
    ///
    /// Concurrent callers for one account wait for the single mint in flight
    /// and then share what it came to: its token, or its failure, which is
    /// kept for [`FAILURE_TTL_SECS`] and handed to every caller in that time
    /// without another request to the token endpoint. A caller that is
    /// cancelled mid-mint abandons it, and the next one starts over.
    ///
    /// A token past the point it is replaced is still a token. When the mint
    /// that should replace it fails, it is handed out while it has more than
    /// [`LAST_RESORT_SECS`] left, and the failure is the caller's only after
    /// that.
    pub async fn token(
        &self,
        account: AccountId,
        sa_json: &str,
        proxy: Option<&str>,
    ) -> Result<String> {
        let slot = self.slot(account);
        let mut held = slot.lock().await;
        // Read after the lock is taken: a caller that waited has to judge the
        // token the one before it minted against the time now, not the time
        // it started waiting.
        let now = (self.clock)();
        if let Some(minted) = held
            .minted
            .as_ref()
            .filter(|m| now < m.expires_at - REFRESH_SKEW_SECS)
        {
            return Ok(minted.access_token.clone());
        }
        if let Some(failed) = held
            .failed
            .as_ref()
            .filter(|f| now < f.at.saturating_add(FAILURE_TTL_SECS))
        {
            return held
                .last_resort(now)
                .map(|m| m.access_token.clone())
                .ok_or_else(|| failed.error());
        }

        match self.mint(sa_json, proxy, now).await {
            Ok(minted) => {
                let access_token = minted.access_token.clone();
                *held = Held {
                    minted: Some(minted),
                    failed: None,
                };
                Ok(access_token)
            }
            Err(failed) => {
                let error = failed.error();
                held.failed = Some(failed);
                match held.last_resort(now) {
                    Some(minted) => {
                        tracing::warn!(
                            %account,
                            seconds_left = minted.expires_at - now,
                            error = %error,
                            "a new Google token could not be minted; the one in hand is used while it lasts"
                        );
                        Ok(minted.access_token.clone())
                    }
                    None => Err(error),
                }
            }
        }
    }

    /// A token minted from `sa_json` at `now`, or the failure its callers
    /// will be told.
    async fn mint(
        &self,
        sa_json: &str,
        proxy: Option<&str>,
        now: i64,
    ) -> std::result::Result<Minted, Failed> {
        let exchanged = async {
            let key = ServiceAccountKey::from_json(sa_json)?;
            let assertion = key.sign_assertion(now)?;
            self.exchange(&assertion, proxy).await
        };
        let (access_token, expires_in) = exchanged.await.map_err(|e| Failed::new(e, now))?;
        // Counted from before the request went out, so the recorded expiry is
        // never later than the real one.
        Ok(Minted {
            access_token,
            expires_at: now.saturating_add_unsigned(expires_in),
        })
    }

    fn slot(&self, account: AccountId) -> Slot {
        // A panic cannot leave the map half-written: it is only ever read and
        // inserted into. So a poisoned lock is recovered, not given up on.
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(slots.entry(account).or_default())
    }

    /// Trade a signed assertion for an access token and its lifetime in seconds.
    ///
    /// Through a client built for this mint, as every call made with a
    /// credential is (`side_channel_client`): `proxy` applied, no redirect
    /// followed, and the whole exchange bounded by the mint timeout. A mint
    /// is due at most once an hour per account, so nothing is worth keeping
    /// between two.
    async fn exchange(&self, assertion: &str, proxy: Option<&str>) -> Result<(String, u64)> {
        let response = crate::side_channel_client(proxy, MINT_TIMEOUT)?
            .post(&self.token_url)
            .form(&[("grant_type", GRANT_TYPE), ("assertion", assertion)])
            .send()
            .await
            .map_err(|e| unreachable(format!("the Google token endpoint: {e}")))?;
        let status = response.status();
        let body = response.bytes().await;
        if !status.is_success() {
            // The status is the finding; a body that failed to arrive only
            // costs the error code that might have come with it.
            // A 4xx is the endpoint refusing this credential (`invalid_grant`,
            // `invalid_client`), which lasts until an operator acts; a timeout,
            // a throttle, a redirect or a 5xx is the way to Google, which may
            // clear on its own.
            return Err(Error::UpstreamUnavailable {
                reason: refusal(status, &body.unwrap_or_default(), assertion),
                lasting: status.is_client_error() && !matches!(status.as_u16(), 408 | 429),
            });
        }
        let body = body.map_err(|e| unreachable(format!("the Google token endpoint: {e}")))?;

        // Where, never what: the body holds the access token, and serde quotes
        // the value it choked on.
        let token: TokenResponse = serde_json::from_slice(&body).map_err(|e| {
            unreachable(format!(
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
            return Err(unreachable(
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
