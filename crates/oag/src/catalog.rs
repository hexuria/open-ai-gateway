//! Seeding the model catalog.
//!
//! The authoritative source is LiteLLM's `model_prices_and_context_window.json`
//! — the most complete public pricing table, and the one sub2api uses too.
//! Point `--from` at a downloaded copy.
//!
//! The built-in set exists so a fresh install works offline, and is deliberately
//! small. **Verify the prices before trusting a savings figure**: they are a
//! starting point, they go stale, and the whole value of the counterfactual
//! column is that the numbers in it are real.

use oag_core::{Error, Result};
use oag_store::ModelRow;
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;

/// One built-in entry, before it becomes a row.
///
/// A named struct rather than a tuple: eleven positional fields is exactly how
/// a price ends up in the context-window column.
struct Builtin {
    id: &'static str,
    upstream: &'static str,
    /// USD per million input tokens.
    input: &'static str,
    /// USD per million output tokens.
    output: &'static str,
    context: i32,
    max_output: i32,
    reasoning: bool,
}

/// A small starter catalog. Refresh from LiteLLM before relying on the numbers.
#[must_use]
pub fn builtin() -> Vec<ModelRow> {
    const MODELS: &[Builtin] = &[
        Builtin {
            id: "anthropic/claude-opus-5",
            upstream: "claude-opus-5",
            input: "15",
            output: "75",
            context: 400_000,
            max_output: 64_000,
            reasoning: true,
        },
        Builtin {
            id: "anthropic/claude-sonnet-4.5",
            upstream: "claude-sonnet-4-5",
            input: "3",
            output: "15",
            context: 200_000,
            max_output: 64_000,
            reasoning: true,
        },
        Builtin {
            id: "anthropic/claude-haiku-4.5",
            upstream: "claude-haiku-4-5",
            input: "1",
            output: "5",
            context: 200_000,
            max_output: 32_000,
            reasoning: false,
        },
    ];

    MODELS
        .iter()
        .filter_map(|m| {
            let input = Decimal::from_str(m.input).ok()?;
            Some(ModelRow {
                id: m.id.to_owned(),
                provider: "anthropic".to_owned(),
                upstream_name: m.upstream.to_owned(),
                input_per_mtok: input,
                output_per_mtok: Decimal::from_str(m.output).ok()?,
                // Anthropic's published ratios: a cache read is a tenth of a
                // fresh input token, a cache write is a quarter more.
                cache_read_per_mtok: Some(input / Decimal::from(10)),
                cache_write_per_mtok: Some(input * Decimal::from_str("1.25").ok()?),
                context_window: m.context,
                max_output_tokens: m.max_output,
                supports_vision: true,
                supports_tools: true,
                supports_reasoning: m.reasoning,
                supports_prompt_cache: true,
            })
        })
        .collect()
}

/// Parse a LiteLLM pricing file.
///
/// Its prices are per *token* rather than per million, which is the one thing
/// worth getting right here: a factor of a million in the wrong direction makes
/// every routing decision and every savings figure nonsense.
pub fn from_litellm_file(path: &str) -> Result<Vec<ModelRow>> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| Error::Config(format!("reading {path}: {e}")))?;
    let doc: Value = serde_json::from_str(&raw)?;
    let Some(map) = doc.as_object() else {
        return Err(Error::Config(
            "pricing file is not a JSON object".to_owned(),
        ));
    };

    let million = Decimal::from(1_000_000u32);
    let mut out = Vec::new();

    for (name, spec) in map {
        // A metadata key, not a model.
        if name == "sample_spec" {
            continue;
        }
        let Some(provider) = spec["litellm_provider"].as_str() else {
            continue;
        };
        // Only providers we have an adapter for. Importing the rest would put
        // models in the catalog that the router could pick and nothing could
        // serve.
        let Ok(known) = provider.parse::<oag_core::Provider>() else {
            continue;
        };

        let Some(input) = decimal(&spec["input_cost_per_token"]) else {
            continue;
        };
        let Some(output) = decimal(&spec["output_cost_per_token"]) else {
            continue;
        };

        // Skip free and placeholder entries: a zero price would make the model
        // win every cost comparison outright.
        if input.is_zero() && output.is_zero() {
            continue;
        }

        let modes = spec["supported_modalities"].as_array();
        let vision = spec["supports_vision"].as_bool().unwrap_or_else(|| {
            modes.is_some_and(|m| m.iter().any(|v| v.as_str() == Some("image")))
        });

        out.push(ModelRow {
            id: format!("{known}/{}", name.rsplit('/').next().unwrap_or(name)),
            provider: known.as_str().to_owned(),
            upstream_name: name.clone(),
            input_per_mtok: input * million,
            output_per_mtok: output * million,
            cache_read_per_mtok: decimal(&spec["cache_read_input_token_cost"]).map(|d| d * million),
            cache_write_per_mtok: decimal(&spec["cache_creation_input_token_cost"])
                .map(|d| d * million),
            // Context windows are millions at most; i32 is ample and a
            // saturating conversion beats a wrap.
            context_window: i32::try_from(spec["max_input_tokens"].as_i64().unwrap_or(0))
                .unwrap_or(i32::MAX),
            max_output_tokens: i32::try_from(spec["max_output_tokens"].as_i64().unwrap_or(4096))
                .unwrap_or(i32::MAX),
            supports_vision: vision,
            supports_tools: spec["supports_function_calling"].as_bool().unwrap_or(false),
            supports_reasoning: spec["supports_reasoning"].as_bool().unwrap_or(false),
            supports_prompt_cache: spec["supports_prompt_caching"].as_bool().unwrap_or(false),
        });
    }

    if out.is_empty() {
        return Err(Error::Config(format!(
            "no models in {path} matched a provider this gateway has an adapter for"
        )));
    }
    Ok(out)
}

/// Read a JSON number as an exact `Decimal`.
///
/// Via the string form, not `as_f64`: these are prices, and round-tripping
/// through binary floating point is exactly the drift the ledger avoids.
fn decimal(v: &Value) -> Option<Decimal> {
    match v {
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        Value::String(s) => Decimal::from_str(s).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_catalog_is_internally_consistent() {
        let models = builtin();
        assert!(!models.is_empty());
        for m in &models {
            assert!(m.output_per_mtok > m.input_per_mtok, "{}", m.id);
            assert!(m.context_window > 0, "{}", m.id);
            assert!(m.provider.parse::<oag_core::Provider>().is_ok(), "{}", m.id);
            let read = m.cache_read_per_mtok.expect("cache read price");
            assert!(
                read < m.input_per_mtok,
                "a cache read must beat a fresh token"
            );
        }
    }

    #[test]
    fn the_builtin_ladder_is_actually_ordered_by_cost() {
        // The default route's rungs reference these ids, so if the ordering is
        // wrong the "cheap" rung is not cheap.
        let models = builtin();
        let price = |id: &str| {
            models
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.input_per_mtok)
                .expect("model present")
        };
        assert!(price("anthropic/claude-haiku-4.5") < price("anthropic/claude-sonnet-4.5"));
        assert!(price("anthropic/claude-sonnet-4.5") < price("anthropic/claude-opus-5"));
    }

    #[test]
    fn litellm_per_token_prices_are_scaled_to_per_million() {
        // The factor-of-a-million bug: LiteLLM stores per-token, we store
        // per-million, and getting it backwards makes every routing decision
        // and every savings figure nonsense.
        let file = serde_json::json!({
            "claude-opus-5": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000_015,
                "output_cost_per_token": 0.000_075,
                "cache_read_input_token_cost": 0.000_001_5,
                "max_input_tokens": 400_000,
                "max_output_tokens": 64_000,
                "supports_function_calling": true,
                "supports_prompt_caching": true
            }
        });
        let path = std::env::temp_dir().join("oag-litellm-test.json");
        std::fs::write(&path, file.to_string()).expect("write");
        let rows = from_litellm_file(path.to_str().expect("path")).expect("parses");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_per_mtok, rust_decimal::dec!(15));
        assert_eq!(rows[0].output_per_mtok, rust_decimal::dec!(75));
        assert_eq!(rows[0].cache_read_per_mtok, Some(rust_decimal::dec!(1.5)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn models_from_providers_we_cannot_serve_are_skipped() {
        // Importing them would let the router pick a model nothing can serve.
        let file = serde_json::json!({
            "some-unsupported-model": {
                "litellm_provider": "a-provider-we-have-no-adapter-for",
                "input_cost_per_token": 0.000_001,
                "output_cost_per_token": 0.000_002
            }
        });
        let path = std::env::temp_dir().join("oag-litellm-skip.json");
        std::fs::write(&path, file.to_string()).expect("write");
        assert!(
            from_litellm_file(path.to_str().expect("path")).is_err(),
            "nothing importable should be an error, not a silent empty catalog"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn free_entries_are_skipped() {
        // A zero price wins every cost comparison outright.
        let file = serde_json::json!({
            "free-thing": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0,
                "output_cost_per_token": 0
            },
            "real-thing": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000_003,
                "output_cost_per_token": 0.000_015,
                "max_input_tokens": 200_000
            }
        });
        let path = std::env::temp_dir().join("oag-litellm-free.json");
        std::fs::write(&path, file.to_string()).expect("write");
        let rows = from_litellm_file(path.to_str().expect("path")).expect("parses");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].upstream_name, "real-thing");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prices_do_not_round_trip_through_binary_float() {
        // 0.0000001 has no exact f64 representation; via the string form it is
        // exact, and prices are multiplied by token counts and summed.
        let v = serde_json::json!(0.000_000_1);
        assert_eq!(decimal(&v), Some(rust_decimal::dec!(0.000_000_1)));
    }
}
