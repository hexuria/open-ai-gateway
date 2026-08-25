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
pub async fn fetch(
    provider: Provider,
    credential: &SecretMaterial,
) -> oag_core::Result<Option<Vec<ModelPrice>>> {
    match provider {
        Provider::XAI => xai::fetch(&credential.access_token).await.map(Some),
        // Anthropic, OpenAI and Gemini publish prices on a web page, not an
        // API; there is nothing to call, so LiteLLM stays the source for them.
        _ => Ok(None),
    }
}
