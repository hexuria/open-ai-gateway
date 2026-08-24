//! Upstream credential kinds and their sealed material.
//!
//! See `docs/compliance.md` for which kinds are sanctioned by which provider.
//! The gateway treats them all identically; the distinction is an operator's
//! to make, and the schema records it so the choice is visible.

use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// How an upstream credential authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CredentialKind {
    /// A static provider API key. The default, and the one every provider
    /// explicitly permits pooling across an organisation's own users.
    ApiKey,
    /// A subscription OAuth token pair. Sanctioned for the seat holder's own
    /// use; see `docs/compliance.md` before pooling one across people.
    OAuth,
    /// AWS `SigV4` credentials for Bedrock.
    Bedrock,
    /// GCP service-account credentials for Vertex.
    Vertex,
    /// A generic service account (provider-specific JSON blob).
    ServiceAccount,
}

impl CredentialKind {
    /// Whether this kind carries an expiring token that needs refreshing.
    #[must_use]
    pub const fn refreshable(self) -> bool {
        matches!(self, Self::OAuth | Self::Vertex | Self::ServiceAccount)
    }
}

impl fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ApiKey => "api_key",
            Self::OAuth => "oauth",
            Self::Bedrock => "bedrock",
            Self::Vertex => "vertex",
            Self::ServiceAccount => "service_account",
        })
    }
}

/// Decrypted credential material.
///
/// Only ever exists in memory, between an AEAD open and the request that uses
/// it. `ZeroizeOnDrop` so it does not linger in a freed allocation, and the
/// `Debug` impl is hand-written so it cannot be logged by accident — a derived
/// one would print the secret into any `tracing` field that captured it.
#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct SecretMaterial {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub refresh_token: Option<String>,
    /// Unix seconds. `None` for credentials that do not expire.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[zeroize(skip)]
    pub expires_at: Option<i64>,
    /// Monotonic counter guarding against a concurrent refresh clobbering a
    /// newer token. Compared before persisting; see `oag-upstream::refresh`.
    #[serde(default)]
    #[zeroize(skip)]
    pub version: u64,
    /// The OAuth client id the refresh grant must be presented under. Only
    /// OAuth credentials carry one; the token endpoint rejects a refresh
    /// token presented by a different client, so it travels with the pair.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_id: Option<String>,
}

impl fmt::Debug for SecretMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretMaterial")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("version", &self.version)
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl SecretMaterial {
    /// Whether the token is expired, or close enough that we should refresh
    /// before spending a request on it.
    #[must_use]
    pub fn expires_within(&self, now_unix: i64, skew_secs: i64) -> bool {
        self.expires_at
            .is_some_and(|exp| exp - skew_secs <= now_unix)
    }
}
