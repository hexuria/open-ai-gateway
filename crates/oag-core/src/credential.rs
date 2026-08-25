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

    /// Whether usage on this kind is billed at a flat rate rather than per
    /// token. A subscription seat is paid for by a monthly fee, so the marginal
    /// cost of a request is zero and the metered price becomes a counterfactual
    /// — see `usage_event.counterfactual_api_usd`.
    #[must_use]
    pub const fn flat_rate(self) -> bool {
        matches!(self, Self::OAuth)
    }

    /// The `@` qualifier that pins a model to this kind, for the kinds a client
    /// may ask for by name.
    ///
    /// Two, and deliberately only two. `@api` and `@sub` are the choice a
    /// caller actually has — the same upstream reached through a metered key or
    /// through a seat — while Bedrock, Vertex and a service account are
    /// different upstreams, and a different upstream is a different provider
    /// with an id of its own. A qualifier for those would be a second way to
    /// spell something that already has a name.
    #[must_use]
    pub const fn qualifier(self) -> Option<&'static str> {
        match self {
            Self::ApiKey => Some("api"),
            Self::OAuth => Some("sub"),
            Self::Bedrock | Self::Vertex | Self::ServiceAccount => None,
        }
    }

    /// Every qualifier a client may write, for an error that names them.
    pub const QUALIFIED: &'static [Self] = &[Self::ApiKey, Self::OAuth];

    /// Parse the text after the `@` in a model id.
    ///
    /// `None` is a client error rather than "unqualified": a caller who wrote
    /// `@subscription` meant to exclude their API keys, and ignoring the pin
    /// would send the request to exactly the credential they excluded.
    #[must_use]
    pub fn from_qualifier(s: &str) -> Option<Self> {
        Self::QUALIFIED
            .iter()
            .copied()
            .find(|k| k.qualifier() == Some(s))
    }

    /// How to name this kind in a sentence a caller reads.
    ///
    /// `Display` writes the column value, which is what the schema and the CLI
    /// speak. "no oauth credential for xai on this route" is that string in a
    /// place it does not belong: the person reading it bought a subscription,
    /// not an oauth.
    #[must_use]
    pub const fn channel_label(self) -> &'static str {
        match self {
            Self::ApiKey => "API key",
            Self::OAuth => "subscription",
            Self::Bedrock => "Bedrock",
            Self::Vertex => "Vertex",
            Self::ServiceAccount => "service account",
        }
    }

    /// Parse the discriminator as stored in `account.kind`. Unknown strings map
    /// to `None` rather than erroring: the caller decides what an unrecognised
    /// kind means, and for flat-rate classification the safe answer is "metered".
    #[must_use]
    pub fn from_column(s: &str) -> Option<Self> {
        match s {
            "api_key" => Some(Self::ApiKey),
            "oauth" => Some(Self::OAuth),
            "bedrock" => Some(Self::Bedrock),
            "vertex" => Some(Self::Vertex),
            "service_account" => Some(Self::ServiceAccount),
            _ => None,
        }
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
    /// The provider's account identifier, sent as a request header on some
    /// subscription APIs (Codex's `ChatGPT-Account-Id`). Not a secret, but it
    /// is bound to this credential, so it lives with it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[zeroize(skip)]
    pub account_id: Option<String>,
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
            .field("account_id", &self.account_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_qualifier_round_trips_to_the_kind_it_names() {
        // The pin is only worth anything if the two directions agree: the
        // listing advertises `@sub` from the kind, and the inference path reads
        // `@sub` back into it.
        for kind in CredentialKind::QUALIFIED.iter().copied() {
            let q = kind.qualifier().expect("a qualified kind names one");
            assert_eq!(CredentialKind::from_qualifier(q), Some(kind));
        }
        assert_eq!(
            CredentialKind::from_qualifier("api"),
            Some(CredentialKind::ApiKey)
        );
        assert_eq!(
            CredentialKind::from_qualifier("sub"),
            Some(CredentialKind::OAuth)
        );
    }

    #[test]
    fn a_kind_that_is_a_provider_of_its_own_has_no_qualifier() {
        // Bedrock is a different base URL, adapter, auth and bill, so it is a
        // different provider with an id of its own. A qualifier for it would be
        // a second spelling of something already named.
        for kind in [
            CredentialKind::Bedrock,
            CredentialKind::Vertex,
            CredentialKind::ServiceAccount,
        ] {
            assert_eq!(kind.qualifier(), None, "{kind} should not be addressable");
            assert_eq!(CredentialKind::from_qualifier(&kind.to_string()), None);
        }
    }

    #[test]
    fn an_unknown_qualifier_parses_to_nothing_rather_than_a_default() {
        // Falling back to "unqualified" would send a request to the very
        // credential the caller wrote the pin to exclude.
        for bogus in ["bogus", "subscription", "apikey", "oauth", ""] {
            assert_eq!(CredentialKind::from_qualifier(bogus), None, "{bogus}");
        }
    }
}
