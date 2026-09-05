//! Reading a provider's own price list.
//!
//! LiteLLM's community table is the broad source and it lags reality; a
//! provider that publishes its own prices is authoritative about what it
//! charges. It is authoritative about nothing else, which is the whole point of
//! this type: none of these endpoints report a context window, so a
//! `ModelPrice` deliberately carries no field that could overwrite a routing
//! fact the catalog already knows.

pub mod xai;

use oag_core::Provider;
use oag_core::credential::CredentialKind;
use oag_core::credential::SecretMaterial;
use rust_decimal::Decimal;

/// One model's prices, as the provider itself states them.
///
/// Prices are USD per million tokens, matching `model_catalog`, because the one
/// unit conversion in this path should happen next to the payload that needs it
/// rather than somewhere down in the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPrice {
    /// The provider's own model id, i.e. what goes on the wire.
    pub upstream_name: String,
    pub input_per_mtok: Decimal,
    pub output_per_mtok: Decimal,
    /// `None` when the provider does not price a cache read separately — which
    /// is not the same as pricing it at zero.
    pub cache_read_per_mtok: Option<Decimal>,
    /// Stated by the payload's modality list, not guessed.
    pub supports_vision: bool,
}

/// Fetch the provider's price list, or `None` for a provider with no price API
/// wired up (in which case the caller should stay on the LiteLLM table).
///
/// Dispatched on the credential's kind as well as its provider. xAI's price
/// endpoint is part of its management API and takes an API key; a subscription
/// seat's OAuth token is not one, and presenting it there gets a 401 that the
/// caller reads as "prices unavailable" — or, worse, logs as an auth failure
/// against a credential that is working perfectly for inference. A seat has no
/// price list to give and asking it is the error.
pub async fn fetch(
    provider: Provider,
    kind: CredentialKind,
    credential: &SecretMaterial,
) -> oag_core::Result<Option<Vec<ModelPrice>>> {
    match (provider, kind) {
        (Provider::XAI, CredentialKind::ApiKey) => {
            xai::fetch(&credential.access_token).await.map(Some)
        }
        // Anthropic, OpenAI and Gemini publish prices on a web page, not an
        // API; there is nothing to call, so LiteLLM stays the source for them.
        // And a seat token cannot ask, whoever its provider is.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::{CredentialKind, Provider, fetch};
    use oag_core::credential::SecretMaterial;

    fn material() -> SecretMaterial {
        SecretMaterial {
            access_token: "not-a-real-token".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        }
    }

    /// U3. A seat token is never presented to a management API.
    ///
    /// xAI's price endpoint is part of its management API and takes an API key.
    /// A subscription seat's OAuth token is not one, and dispatching on the
    /// provider alone sent it there — producing a 401 the caller reads as
    /// "prices unavailable", or logs as an auth failure against a credential
    /// that is working perfectly for inference.
    ///
    /// Asserted without a network call: `Ok(None)` is returned before any
    /// request is built, which is the whole of the fix.
    #[tokio::test]
    async fn a_seat_credential_is_not_asked_for_a_price_list() {
        for kind in [
            CredentialKind::OAuth,
            CredentialKind::Bedrock,
            CredentialKind::Vertex,
            CredentialKind::ServiceAccount,
        ] {
            let answered = fetch(Provider::XAI, kind, &material())
                .await
                .expect("no request is made, so nothing can fail");
            assert!(
                answered.is_none(),
                "{kind:?} has no price list to give, and asking is the error"
            );
        }

        // And a provider with no price API is still `None` whatever it holds.
        assert!(
            fetch(Provider::Anthropic, CredentialKind::ApiKey, &material())
                .await
                .expect("no request")
                .is_none()
        );
    }
}
