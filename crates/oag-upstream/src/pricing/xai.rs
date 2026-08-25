//! xAI's own model and price list.
//!
//! `GET /v1/language-models`, with the same bearer an inference call uses. Two
//! things about this endpoint are worth knowing before touching it:
//!
//! It is silent about context windows. `long_context_threshold` looks like one
//! and is not — it is the token count above which the long-context prices
//! apply (200k on a model LiteLLM reports as holding 500k), so anything that
//! passes it off as a window shrinks the catalog's idea of what fits and the
//! router quietly stops sending long requests to the model that could serve
//! them.
//!
//! And it is `/v1/language-models`, not `/v1/models`: the latter also lists the
//! image and video models, which are not chat models and must never reach the
//! catalog the router picks from.

use super::ModelPrice;
use oag_core::{Error, Result};
use rust_decimal::Decimal;
use serde::Deserialize;

const MODELS_URL: &str = "https://api.x.ai/v1/language-models";

/// A price field is USD per million tokens, scaled by ten thousand: 20000 is
/// $2.00/Mtok.
///
/// Verified against LiteLLM across three models and three fields — grok-4.6
/// prompt 20000 → $2.00, grok-4.3 prompt 12500 → $1.25, grok-4.6 cached 5000 →
/// $0.50. This is the same class of trap as LiteLLM's per-token prices: a
/// factor in the wrong direction here makes every routing decision and every
/// savings figure nonsense, and nothing downstream would flag it.
const PER_MTOK_SCALE: i64 = 10_000;

pub async fn fetch(access_token: &str) -> Result<Vec<ModelPrice>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| Error::Internal(format!("building price client: {e}")))?;

    let response = client
        .get(MODELS_URL)
        .header("authorization", format!("Bearer {}", access_token.trim()))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Internal(format!("xai model list request: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::Internal(format!(
            "xai model list returned {}",
            response.status()
        )));
    }

    let body: Response = response
        .json()
        .await
        .map_err(|e| Error::Internal(format!("xai model list body: {e}")))?;
    Ok(parse(&body))
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    models: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
    #[serde(default)]
    prompt_text_token_price: i64,
    #[serde(default)]
    completion_text_token_price: i64,
    /// Absent on a model with no prompt cache, which is not a cache that costs
    /// nothing.
    #[serde(default)]
    cached_prompt_text_token_price: Option<i64>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
}

fn parse(body: &Response) -> Vec<ModelPrice> {
    body.models
        .iter()
        .filter_map(|m| {
            // The endpoint should already have excluded grok-imagine-*, but a
            // model that cannot emit text is not a chat model whatever list it
            // came from, and one in the catalog is a model the router can pick
            // and no chat request can use. An *absent* modality list is not
            // evidence of an image model, so only a stated one disqualifies.
            if !m.output_modalities.is_empty() && !m.output_modalities.iter().any(|s| s == "text") {
                return None;
            }

            let input = per_mtok(m.prompt_text_token_price);
            let output = per_mtok(m.completion_text_token_price);
            // Same guard as the LiteLLM importer: a zero price wins every cost
            // comparison outright, so an unpriced or placeholder entry is worse
            // than a missing one.
            if input.is_zero() && output.is_zero() {
                return None;
            }

            Some(ModelPrice {
                upstream_name: m.id.clone(),
                input_per_mtok: input,
                output_per_mtok: output,
                cache_read_per_mtok: m.cached_prompt_text_token_price.map(per_mtok),
                supports_vision: m.input_modalities.iter().any(|s| s == "image"),
            })
        })
        .collect()
}

/// Scale one stated price into USD per million tokens.
///
/// Integer arithmetic through `Decimal`, never `f64`: these numbers are
/// multiplied by token counts and summed into the ledger.
fn per_mtok(raw: i64) -> Decimal {
    Decimal::from(raw) / Decimal::from(PER_MTOK_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    /// Trimmed from a live response, field names and magnitudes intact.
    const LANGUAGE_MODELS: &str = r#"{ "models": [
        { "id": "grok-4.6",
          "aliases": ["grok-4.6-latest"],
          "prompt_text_token_price": 20000,
          "completion_text_token_price": 60000,
          "cached_prompt_text_token_price": 5000,
          "prompt_image_token_price": 20000,
          "search_price": 25000,
          "long_context_threshold": 200000,
          "prompt_text_token_price_long_context": 40000,
          "input_modalities": ["text", "image"],
          "output_modalities": ["text"],
          "version": "1.0.0",
          "owned_by": "xai",
          "created": 1762300000 },
        { "id": "grok-4.3",
          "prompt_text_token_price": 12500,
          "completion_text_token_price": 50000,
          "input_modalities": ["text"],
          "output_modalities": ["text"],
          "owned_by": "xai" },
        { "id": "grok-imagine-1",
          "prompt_text_token_price": 10000,
          "completion_text_token_price": 0,
          "input_modalities": ["text"],
          "output_modalities": ["image"],
          "owned_by": "xai" } ] }"#;

    #[test]
    fn a_stated_price_is_ten_thousand_times_the_dollars_per_million_tokens() {
        // The factor-error guard. 20000 is $2.00/Mtok, not $20000, not
        // $0.00002 — LiteLLM says grok-4.6 costs 2e-06 per input token.
        let body: Response = serde_json::from_str(LANGUAGE_MODELS).expect("json");
        let rows = parse(&body);
        let grok = rows
            .iter()
            .find(|m| m.upstream_name == "grok-4.6")
            .expect("grok-4.6");
        assert_eq!(grok.input_per_mtok, dec!(2));
        assert_eq!(grok.output_per_mtok, dec!(6));
        assert_eq!(grok.cache_read_per_mtok, Some(dec!(0.5)));
    }

    #[test]
    fn a_fractional_price_survives_the_scaling_exactly() {
        // 12500 is $1.25, and a price that lands between cents is exactly where
        // an f64 round trip would start drifting the ledger.
        let body: Response = serde_json::from_str(LANGUAGE_MODELS).expect("json");
        let grok = body
            .models
            .iter()
            .find(|m| m.id == "grok-4.3")
            .map(|m| per_mtok(m.prompt_text_token_price))
            .expect("grok-4.3");
        assert_eq!(grok, dec!(1.25));
    }

    #[test]
    fn a_model_that_cannot_emit_text_is_not_a_chat_model() {
        // grok-imagine-* has a prompt price and is still not something a chat
        // request can be routed to.
        let body: Response = serde_json::from_str(LANGUAGE_MODELS).expect("json");
        let rows = parse(&body);
        assert!(
            !rows
                .iter()
                .any(|m| m.upstream_name.starts_with("grok-imagine"))
        );
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn vision_comes_from_the_stated_modalities() {
        let body: Response = serde_json::from_str(LANGUAGE_MODELS).expect("json");
        let rows = parse(&body);
        assert!(
            rows.iter()
                .find(|m| m.upstream_name == "grok-4.6")
                .expect("grok-4.6")
                .supports_vision
        );
        assert!(
            !rows
                .iter()
                .find(|m| m.upstream_name == "grok-4.3")
                .expect("grok-4.3")
                .supports_vision
        );
    }

    #[test]
    fn an_absent_cache_price_is_not_a_free_cache() {
        // A missing field means "no separate cache price", and writing a zero
        // there would tell the ledger cache reads cost nothing.
        let body: Response = serde_json::from_str(LANGUAGE_MODELS).expect("json");
        let rows = parse(&body);
        let grok = rows
            .iter()
            .find(|m| m.upstream_name == "grok-4.3")
            .expect("grok-4.3");
        assert_eq!(grok.cache_read_per_mtok, None);
    }

    #[test]
    fn an_unpriced_entry_is_skipped_rather_than_priced_at_zero() {
        // Zero wins every cost comparison outright.
        let body: Response = serde_json::from_str(
            r#"{"models":[{"id":"grok-preview","prompt_text_token_price":0,"completion_text_token_price":0,"output_modalities":["text"]}]}"#,
        )
        .expect("json");
        assert!(parse(&body).is_empty());
    }

    #[test]
    fn an_empty_body_is_no_models_not_an_error() {
        assert!(parse(&serde_json::from_str("{}").expect("json")).is_empty());
    }
}
