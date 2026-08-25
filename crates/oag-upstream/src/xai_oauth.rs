//! xAI subscription OAuth: Grok CLI sessions, and the refresh grant.
//!
//! A Grok subscription authenticates the Grok CLI through xAI's OIDC
//! server and leaves the session in `~/.grok/auth.json`. Two things live here:
//! parsing that file into importable sessions, and the refresh-token grant
//! that keeps an imported session alive. Neither ever writes the file — the
//! CLI owns it, and a gateway that rotated the token underneath it would break
//! the login it was imported from. Rotated tokens are persisted in the
//! `account` row instead, version-guarded, by `ensure_fresh`.

use oag_core::credential::SecretMaterial;
use oag_core::{Error, Result};
use serde::Deserialize;

/// The OIDC issuer for Grok CLI sessions. Tests point this at a local mock.
pub const DEFAULT_AUTH_BASE: &str = "https://auth.x.ai";

/// The prefix that marks an entry of `auth.json` as an xAI session. Everything
/// after the `::` is the OIDC client id, which older files do not repeat
/// inside the entry.
const SESSION_KEY_PREFIX: &str = "https://auth.x.ai::";

/// One signed-in Grok CLI session. `expires_at` is unix seconds; `None` means
/// unknown — try the token and refresh on the first 401 rather than guessing.
#[derive(Debug, Clone)]
pub struct GrokSession {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub client_id: Option<String>,
}

impl GrokSession {
    /// The sealed shape an `account` row stores.
    #[must_use]
    pub fn into_material(self) -> SecretMaterial {
        SecretMaterial {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
            version: 0,
            client_id: self.client_id,
            account_id: None,
        }
    }
}

/// What one `auth.json` entry actually carries. The access token is under
/// `key`, which is the file's vocabulary, not ours.
#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    key: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    oidc_client_id: Option<String>,
}

/// Parse the sessions out of one `auth.json`.
///
/// The file is a flat object; only keys with the xAI issuer prefix are ours —
/// the CLI stores other providers' sessions in the same file. An entry with an
/// empty `key` is a logged-out remnant and is skipped. One file can hold
/// several subscriptions side by side, so this returns all of them.
pub fn sessions_from_json(json: &str) -> Result<Vec<GrokSession>> {
    let map: std::collections::BTreeMap<String, serde_json::Value> = serde_json::from_str(json)
        .map_err(|e| Error::Config(format!("auth.json is not a JSON object: {e}")))?;

    let mut sessions = Vec::new();
    for (key, value) in map {
        let Some(suffix) = key.strip_prefix(SESSION_KEY_PREFIX) else {
            continue;
        };
        let entry: RawEntry = serde_json::from_value(value)
            .map_err(|e| Error::Config(format!("auth.json entry {key}: {e}")))?;
        if entry.key.is_empty() {
            continue;
        }
        sessions.push(GrokSession {
            access_token: entry.key,
            refresh_token: entry.refresh_token,
            expires_at: entry.expires_at.as_deref().and_then(parse_rfc3339),
            // Older files carry the client id only in the map key.
            client_id: entry
                .oidc_client_id
                .or_else(|| (!suffix.is_empty()).then(|| suffix.to_owned())),
        });
    }
    Ok(sessions)
}

/// Union sessions from several files, first occurrence of a token winning.
///
/// The same session shows up twice when a file was copied to a second machine
/// and both paths are handed to the importer; two *rows* for one token would
/// double its concurrency and split its ledger.
#[must_use]
pub fn union_sessions(batches: impl IntoIterator<Item = Vec<GrokSession>>) -> Vec<GrokSession> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for batch in batches {
        for session in batch {
            if seen.insert(session.access_token.clone()) {
                out.push(session);
            }
        }
    }
    out
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(time::OffsetDateTime::unix_timestamp)
        .ok()
}

/// What the token endpoint returns on a successful refresh grant.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct Discovery {
    token_endpoint: String,
}

/// Refresh an xAI OAuth credential. `Ok(None)` means "not refreshable" — a
/// static API key on the same provider takes this path.
///
/// The endpoint is discovered per call rather than hardcoded: xAI moved its
/// auth infrastructure once already, and the discovery document is the one URL
/// they are committed to keeping stable. The call is cheap next to how rarely
/// a refresh happens.
pub async fn refresh(
    credential: &SecretMaterial,
    auth_base: &str,
) -> Result<Option<SecretMaterial>> {
    let Some(refresh_token) = credential.refresh_token.as_deref() else {
        return Ok(None);
    };
    let Some(client_id) = credential.client_id.as_deref() else {
        // Without the client id the grant is rejected upstream with a message
        // that does not say why. Fail with one that does.
        return Err(Error::Config(
            "xai oauth credential has no client_id; re-import it with \
             `oag admin add-account --from-grok`"
                .to_owned(),
        ));
    };

    // Bounded below the fleet refresh lock's 30s TTL, so a hung endpoint
    // surfaces as this credential's failure rather than a wedged lock.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| Error::Internal(format!("building refresh client: {e}")))?;

    let discovery: Discovery = client
        .get(format!("{auth_base}/.well-known/openid-configuration"))
        .send()
        .await
        .map_err(|e| Error::Internal(format!("xai oauth discovery: {e}")))?
        .error_for_status()
        .map_err(|e| Error::Internal(format!("xai oauth discovery: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Internal(format!("xai oauth discovery body: {e}")))?;

    let response = client
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|e| Error::Internal(format!("xai token endpoint: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        // `invalid_grant` is worth naming: it usually means another replica
        // won the refresh race and this token is already consumed, which
        // `ensure_fresh` recovers from by re-reading — or the seat was
        // signed out, which only a re-import fixes.
        let hint = if body.contains("invalid_grant") {
            " (invalid_grant: consumed by a concurrent refresh, or the \
             session was revoked — re-import if this repeats)"
        } else {
            ""
        };
        return Err(Error::Internal(format!(
            "xai token endpoint returned {status}{hint}"
        )));
    }

    let token: TokenResponse = response
        .json()
        .await
        .map_err(|e| Error::Internal(format!("xai token response: {e}")))?;

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    Ok(Some(SecretMaterial {
        access_token: token.access_token,
        // A provider that does not rotate the refresh token omits it; the one
        // we presented is then still live.
        refresh_token: token
            .refresh_token
            .or_else(|| credential.refresh_token.clone()),
        // True expiry, no skew: `ensure_fresh` already refreshes ahead of it.
        expires_at: token.expires_in.map(|s| now + s),
        // The caller owns the version bump; see `refresh_locked`.
        version: credential.version,
        client_id: credential.client_id.clone(),
        account_id: credential.account_id.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_SEATS: &str = r#"{
        "https://auth.x.ai::client-abc": {
            "key": "token-one",
            "refresh_token": "refresh-one",
            "expires_at": "2026-08-24T12:00:00Z",
            "oidc_client_id": "client-abc"
        },
        "https://auth.x.ai::client-def": {
            "key": "token-two",
            "refresh_token": "refresh-two"
        },
        "https://auth.example.com::other": { "key": "not-ours" },
        "https://auth.x.ai::stale": { "key": "" }
    }"#;

    #[test]
    fn parses_every_xai_session_and_nothing_else() {
        let sessions = sessions_from_json(TWO_SEATS).expect("parses");
        assert_eq!(
            sessions.len(),
            2,
            "two live xAI seats; foreign and logged-out entries skipped"
        );
        assert!(
            sessions
                .iter()
                .all(|s| s.access_token.starts_with("token-"))
        );
    }

    #[test]
    fn client_id_falls_back_to_the_map_key_suffix() {
        let sessions = sessions_from_json(TWO_SEATS).expect("parses");
        let two = sessions
            .iter()
            .find(|s| s.access_token == "token-two")
            .expect("present");
        // The entry has no oidc_client_id; the key suffix is the only copy.
        assert_eq!(two.client_id.as_deref(), Some("client-def"));
    }

    #[test]
    fn expiry_is_unix_seconds_and_absence_is_unknown() {
        let sessions = sessions_from_json(TWO_SEATS).expect("parses");
        let one = sessions
            .iter()
            .find(|s| s.access_token == "token-one")
            .expect("present");
        assert_eq!(one.expires_at, Some(1_787_572_800));
        let two = sessions
            .iter()
            .find(|s| s.access_token == "token-two")
            .expect("present");
        assert_eq!(
            two.expires_at, None,
            "no expiry claim means unknown, not fresh"
        );
    }

    #[test]
    fn union_dedupes_by_token_and_first_path_wins() {
        let a = sessions_from_json(TWO_SEATS).expect("parses");
        let b = sessions_from_json(TWO_SEATS).expect("parses");
        let merged = union_sessions([a, b]);
        assert_eq!(merged.len(), 2, "the same file twice is still two seats");
    }

    #[test]
    fn a_file_that_is_not_an_object_is_a_config_error() {
        assert!(sessions_from_json("[]").is_err());
        assert!(sessions_from_json("not json").is_err());
    }

    #[tokio::test]
    async fn a_credential_without_a_refresh_token_is_not_refreshable() {
        let material = SecretMaterial {
            access_token: "static-key".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        };
        let refreshed = refresh(&material, "http://127.0.0.1:9").await.expect("ok");
        assert!(
            refreshed.is_none(),
            "a static key must not attempt the grant"
        );
    }

    #[tokio::test]
    async fn a_refresh_token_without_a_client_id_names_the_fix() {
        let material = SecretMaterial {
            access_token: "t".to_owned(),
            refresh_token: Some("r".to_owned()),
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        };
        let err = refresh(&material, "http://127.0.0.1:9").await.unwrap_err();
        assert!(err.to_string().contains("--from-grok"), "{err}");
    }

    /// A minimal OIDC server: a discovery document pointing at itself, and a
    /// token endpoint that answers with `grant` and records the form it saw.
    async fn mock_oidc(
        status: u16,
        grant: &'static str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    ) {
        use axum::routing::{get, post};

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));

        let discovery =
            axum::Json(serde_json::json!({ "token_endpoint": format!("{base}/oauth/token") }));
        let recorded = seen.clone();
        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let d = discovery.clone();
                    async move { d }
                }),
            )
            .route(
                "/oauth/token",
                post(move |axum::Form(form): axum::Form<Vec<(String, String)>>| {
                    *recorded.lock().expect("lock") = form;
                    async move {
                        axum::response::Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(grant))
                            .expect("response")
                    }
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (base, seen)
    }

    fn oauth_material() -> SecretMaterial {
        SecretMaterial {
            access_token: "old-access".to_owned(),
            refresh_token: Some("old-refresh".to_owned()),
            expires_at: Some(0),
            version: 3,
            client_id: Some("client-abc".to_owned()),
            account_id: None,
        }
    }

    #[tokio::test]
    async fn the_grant_goes_through_discovery_and_carries_the_session_identity() {
        let (base, seen) = mock_oidc(
            200,
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
        )
        .await;

        let before = time::OffsetDateTime::now_utc().unix_timestamp();
        let fresh = refresh(&oauth_material(), &base)
            .await
            .expect("refresh ok")
            .expect("refreshable");

        let form = seen.lock().expect("lock").clone();
        let field = |k: &str| {
            form.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(field("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(field("client_id").as_deref(), Some("client-abc"));
        assert_eq!(field("refresh_token").as_deref(), Some("old-refresh"));

        assert_eq!(fresh.access_token, "new-access");
        assert_eq!(fresh.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(fresh.client_id.as_deref(), Some("client-abc"));
        assert_eq!(fresh.version, 3, "the version bump belongs to ensure_fresh");
        let expires = fresh.expires_at.expect("expiry set");
        assert!(
            (expires - before - 3600).abs() <= 2,
            "true expiry, no home-made skew: {expires} vs {before}+3600"
        );
    }

    #[tokio::test]
    async fn an_unrotated_refresh_token_is_kept() {
        // Some providers return only a new access token; dropping the refresh
        // token we still hold would make the *next* refresh impossible.
        let (base, _) = mock_oidc(200, r#"{"access_token":"new-access"}"#).await;

        let fresh = refresh(&oauth_material(), &base)
            .await
            .expect("refresh ok")
            .expect("refreshable");
        assert_eq!(fresh.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(fresh.expires_at, None, "no claim means unknown, not zero");
    }

    #[tokio::test]
    async fn invalid_grant_is_named_in_the_error() {
        let (base, _) = mock_oidc(400, r#"{"error":"invalid_grant"}"#).await;

        let err = refresh(&oauth_material(), &base).await.unwrap_err();
        assert!(err.to_string().contains("invalid_grant"), "{err}");
    }
}
