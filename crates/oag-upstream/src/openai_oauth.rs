//! OpenAI/Codex subscription OAuth: importing the Codex CLI session, and the
//! refresh grant.
//!
//! A `ChatGPT` subscription authenticates the Codex CLI through OpenAI's OAuth
//! server and leaves the session in `~/.codex/auth.json`. This parses that file
//! into an importable credential and keeps it alive with the refresh grant. It
//! never writes the file — the CLI owns it, and rotated tokens are persisted in
//! the `account` row instead, version-guarded, by `ensure_fresh`.
//!
//! Contracts (endpoint, client id, error codes, refresh window) are ported from
//! openusage's `Sources/OpenUsage/Providers/Codex/{CodexAuthStore,CodexUsageClient}.swift`
//! (MIT). This module obtains and refreshes the credential; the inference path
//! that actually spends it lives behind its own adapter.

use oag_core::credential::SecretMaterial;
use oag_core::{Error, Result};
use serde::Deserialize;

/// The Codex CLI's public OAuth client id. The token endpoint binds a refresh
/// token to the client it was issued under, so the same id must present it.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OpenAI's OAuth token endpoint. Fixed rather than discovered — OpenAI
/// publishes no third-party OIDC discovery document for this flow.
pub const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Refresh this far ahead of the access token's JWT `exp`, matching the Codex
/// CLI's own 5-minute slack so we rotate on its schedule rather than guessing —
/// refreshing early tripped `refresh_token_reused` in openusage's issue #516.
pub const REFRESH_SKEW_SECS: i64 = 5 * 60;

/// One signed-in Codex CLI session.
#[derive(Debug, Clone)]
pub struct CodexSession {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// The account this seat belongs to, sent as the `ChatGPT-Account-Id` header.
    pub account_id: Option<String>,
    /// Access-token expiry from its JWT `exp`, unix seconds; `None` if the
    /// token is not a decodable JWT.
    pub expires_at: Option<i64>,
}

impl CodexSession {
    /// The sealed shape an `account` row stores.
    #[must_use]
    pub fn into_material(self) -> SecretMaterial {
        SecretMaterial {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
            version: 0,
            client_id: Some(CODEX_CLIENT_ID.to_owned()),
            account_id: self.account_id,
        }
    }
}

/// `~/.codex/auth.json`, in the CLI's own vocabulary.
#[derive(Deserialize)]
struct RawAuth {
    tokens: Option<RawTokens>,
}

#[derive(Deserialize)]
struct RawTokens {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

/// Parse the session out of one `auth.json`.
///
/// Returns `None` when the file carries no OAuth access token — an
/// API-key-only `auth.json` (`OPENAI_API_KEY` set, no `tokens`) is a valid
/// Codex login but not a subscription seat, and importing it as one would
/// produce a credential that 401s on the subscription backend.
pub fn session_from_json(json: &str) -> Result<Option<CodexSession>> {
    let auth: RawAuth = serde_json::from_str(json)
        .map_err(|e| Error::Config(format!("codex auth.json is not valid JSON: {e}")))?;
    let Some(tokens) = auth.tokens else {
        return Ok(None);
    };
    let Some(access_token) = tokens.access_token.filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let expires_at = jwt_exp(&access_token);
    Ok(Some(CodexSession {
        access_token,
        refresh_token: tokens.refresh_token.filter(|t| !t.is_empty()),
        account_id: tokens.account_id.filter(|t| !t.is_empty()),
        expires_at,
    }))
}

/// The `exp` claim (unix seconds) from a JWT access token, or `None` if the
/// token is not a three-part JWT with a numeric `exp`. Signature is not
/// verified: this is a refresh-timing hint, not an authorization decision.
fn jwt_exp(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_i64()
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Refresh a Codex OAuth credential. `Ok(None)` means "not refreshable" — an
/// account with no refresh token takes this path rather than erroring.
pub async fn refresh(
    credential: &SecretMaterial,
    token_url: &str,
) -> Result<Option<SecretMaterial>> {
    let Some(refresh_token) = credential.refresh_token.as_deref() else {
        return Ok(None);
    };

    // Bounded below the fleet refresh lock's 30s TTL so a hung endpoint surfaces
    // as this credential's failure rather than a wedged lock.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| Error::Internal(format!("building refresh client: {e}")))?;

    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CODEX_CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|e| Error::Internal(format!("codex token endpoint: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        // These three are the codes openusage's CodexUsageClient recognises. A
        // reused token usually means another replica won the refresh race, which
        // `ensure_fresh` recovers from by re-reading; expired/invalidated need a
        // re-login and only a re-import fixes them.
        let hint = [
            "refresh_token_reused",
            "refresh_token_expired",
            "refresh_token_invalidated",
        ]
        .into_iter()
        .find(|code| body.contains(code))
        .map_or(String::new(), |code| format!(" ({code})"));
        return Err(Error::Internal(format!(
            "codex token endpoint returned {status}{hint}"
        )));
    }

    let token: TokenResponse = response
        .json()
        .await
        .map_err(|e| Error::Internal(format!("codex token response: {e}")))?;

    let new_expiry = jwt_exp(&token.access_token);
    Ok(Some(SecretMaterial {
        access_token: token.access_token,
        // OpenAI rotates the refresh token on use; keep the one we presented
        // only if the response omitted a replacement.
        refresh_token: token
            .refresh_token
            .or_else(|| credential.refresh_token.clone()),
        expires_at: new_expiry.or(credential.expires_at),
        version: credential.version,
        client_id: credential.client_id.clone(),
        account_id: credential.account_id.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JWT with the given `exp` and an unverified signature — enough for the
    /// parser, which never checks the signature.
    fn jwt_with_exp(exp: i64) -> String {
        use base64::Engine;
        let enc = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        let header = enc(&serde_json::json!({ "alg": "none", "typ": "JWT" }));
        let payload = enc(&serde_json::json!({ "exp": exp, "sub": "user" }));
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn parses_a_codex_oauth_session() {
        let token = jwt_with_exp(1_800_000_000);
        let json = format!(
            r#"{{"tokens":{{"access_token":"{token}","refresh_token":"r","account_id":"acct-1","id_token":"i"}},"last_refresh":"2026-08-24T00:00:00Z"}}"#
        );
        let session = session_from_json(&json).expect("parses").expect("present");
        assert_eq!(session.refresh_token.as_deref(), Some("r"));
        assert_eq!(session.account_id.as_deref(), Some("acct-1"));
        assert_eq!(session.expires_at, Some(1_800_000_000));
    }

    #[test]
    fn an_api_key_only_auth_is_not_a_subscription_seat() {
        // A valid Codex login, but no OAuth token to spend on the subscription.
        let json = r#"{"OPENAI_API_KEY":"sk-abc","tokens":null}"#;
        assert!(session_from_json(json).expect("parses").is_none());
        let json2 = r#"{"tokens":{"access_token":""}}"#;
        assert!(session_from_json(json2).expect("parses").is_none());
    }

    #[test]
    fn into_material_stamps_the_codex_client_id() {
        let token = jwt_with_exp(1_800_000_000);
        let json = format!(r#"{{"tokens":{{"access_token":"{token}","refresh_token":"r"}}}}"#);
        let material = session_from_json(&json)
            .expect("parses")
            .expect("present")
            .into_material();
        assert_eq!(material.client_id.as_deref(), Some(CODEX_CLIENT_ID));
    }

    #[tokio::test]
    async fn a_credential_without_a_refresh_token_is_not_refreshable() {
        let material = SecretMaterial {
            access_token: "t".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: Some(CODEX_CLIENT_ID.to_owned()),
            account_id: None,
        };
        assert!(
            refresh(&material, "http://127.0.0.1:9")
                .await
                .expect("ok")
                .is_none()
        );
    }

    async fn mock_token_endpoint(status: u16, body: &'static str) -> String {
        use axum::routing::post;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let app = axum::Router::new().route(
            "/oauth/token",
            post(move || async move {
                axum::response::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .expect("response")
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("{base}/oauth/token")
    }

    fn oauth_material() -> SecretMaterial {
        SecretMaterial {
            access_token: "old".to_owned(),
            refresh_token: Some("old-refresh".to_owned()),
            expires_at: Some(0),
            version: 4,
            client_id: Some(CODEX_CLIENT_ID.to_owned()),
            account_id: Some("acct-1".to_owned()),
        }
    }

    #[tokio::test]
    async fn a_successful_refresh_rotates_the_pair_and_keeps_the_account() {
        let new = jwt_with_exp(1_900_000_000);
        let body: &'static str = Box::leak(
            format!(r#"{{"access_token":"{new}","refresh_token":"new-refresh"}}"#).into_boxed_str(),
        );
        let url = mock_token_endpoint(200, body).await;
        let fresh = refresh(&oauth_material(), &url)
            .await
            .expect("ok")
            .expect("some");
        assert!(fresh.access_token.starts_with(&new[..8]));
        assert_eq!(fresh.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(fresh.account_id.as_deref(), Some("acct-1"));
        assert_eq!(fresh.version, 4, "the version bump belongs to ensure_fresh");
        assert_eq!(fresh.expires_at, Some(1_900_000_000));
    }

    #[tokio::test]
    async fn a_reused_refresh_token_is_named_in_the_error() {
        let url = mock_token_endpoint(400, r#"{"error":{"code":"refresh_token_reused"}}"#).await;
        let err = refresh(&oauth_material(), &url).await.unwrap_err();
        assert!(err.to_string().contains("refresh_token_reused"), "{err}");
    }
}
