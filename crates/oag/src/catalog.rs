//! Seeding the model catalog.
//!
//! The authoritative source is LiteLLM's `model_prices_and_context_window.json`
//! — the most complete public pricing table, and the one sub2api uses too.
//! Point `--from` at a downloaded copy or straight at the published URL.
//!
//! The built-in set exists so a fresh install works offline, and is deliberately
//! small. **Verify the prices before trusting a savings figure**: they are a
//! starting point, they go stale, and the whole value of the counterfactual
//! column is that the numbers in it are real.
//!
//! A provider that publishes its own price list beats LiteLLM on money and
//! knows nothing about context windows; `plan_price_sync` is where those two
//! facts are reconciled.

use oag_core::{Error, Provider, Result};
use oag_store::ModelRow;
use oag_upstream::pricing::ModelPrice;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashSet;
use std::str::FromStr;

/// One built-in entry, before it becomes a row.
///
/// A named struct rather than a tuple: eleven positional fields is exactly how
/// a price ends up in the context-window column.
struct Builtin {
    id: &'static str,
    provider: &'static str,
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
            provider: "anthropic",
            upstream: "claude-opus-5",
            input: "15",
            output: "75",
            context: 400_000,
            max_output: 64_000,
            reasoning: true,
        },
        Builtin {
            id: "anthropic/claude-sonnet-4.5",
            provider: "anthropic",
            upstream: "claude-sonnet-4-5",
            input: "3",
            output: "15",
            context: 200_000,
            max_output: 64_000,
            reasoning: true,
        },
        Builtin {
            id: "anthropic/claude-haiku-4.5",
            provider: "anthropic",
            upstream: "claude-haiku-4-5",
            input: "1",
            output: "5",
            context: 200_000,
            max_output: 32_000,
            reasoning: false,
        },
        // A ChatGPT-linked Codex seat can call this; gpt-5.6-sol is rejected
        // on that path ("not supported when using Codex with a ChatGPT
        // account"). API list prices are the counterfactual for a flat-rate
        // OAuth seat.
        Builtin {
            id: "openai/gpt-5.5",
            provider: "openai",
            upstream: "gpt-5.5",
            input: "4",
            output: "20",
            context: 272_000,
            max_output: 128_000,
            reasoning: true,
        },
    ];

    MODELS
        .iter()
        .filter_map(|m| {
            let input = Decimal::from_str(m.input).ok()?;
            Some(ModelRow {
                id: m.id.to_owned(),
                provider: m.provider.to_owned(),
                upstream_name: m.upstream.to_owned(),
                input_per_mtok: input,
                output_per_mtok: Decimal::from_str(m.output).ok()?,
                // Anthropic and OpenAI GPT-5.6 publish the same ratios: a cache
                // read is a tenth of a fresh input token, a cache write is a
                // quarter more.
                cache_read_per_mtok: Some(input / Decimal::from(10)),
                cache_write_per_mtok: Some(input * Decimal::from_str("1.25").ok()?),
                context_window: m.context,
                max_output_tokens: m.max_output,
                supports_vision: true,
                supports_tools: true,
                supports_reasoning: m.reasoning,
                supports_prompt_cache: true,
                // Nothing built in is named by hand: a starter row shows the
                // derived label until an operator decides otherwise.
                display_label: None,
            })
        })
        .collect()
}

/// Load a LiteLLM pricing table from wherever `--from` points.
///
/// A path and a URL are the same table; the only difference is who fetches it,
/// and asking an operator to curl the file into place first was never the
/// interesting part of the job.
pub async fn from_litellm(source: &str) -> Result<Vec<ModelRow>> {
    if is_url(source) {
        let raw = fetch_text(source).await?;
        from_litellm_str(&raw, source)
    } else {
        from_litellm_file(source)
    }
}

/// Whether `--from` names a URL rather than a file.
///
/// A scheme prefix and nothing cleverer: `--from` was a path for as long as it
/// has existed, and a heuristic that sniffs for dots or slashes would turn a
/// file called `https` — or a relative path on a machine whose files are not
/// where the operator thought — into an outbound request.
fn is_url(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}

/// GET the pricing table.
///
/// A minute is generous for ~2MB over a slow link and still bounded: an
/// operator watching a seed that has hung has no way to tell it apart from one
/// that is merely slow, so it must end on its own.
async fn fetch_text(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_mins(1))
        .build()
        .map_err(|e| Error::Config(format!("building http client: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Config(format!("fetching {url}: {e}")))?;

    // Named status, because the usual failure is a moved raw.githubusercontent
    // path answering 404 with a page of HTML, and "expected value at line 1"
    // sends the operator looking in the wrong place entirely.
    if !response.status().is_success() {
        return Err(Error::Config(format!(
            "fetching {url}: returned {}",
            response.status()
        )));
    }

    response
        .text()
        .await
        .map_err(|e| Error::Config(format!("reading {url}: {e}")))
}

/// Parse a LiteLLM pricing file.
///
/// Its prices are per *token* rather than per million, which is the one thing
/// worth getting right here: a factor of a million in the wrong direction makes
/// every routing decision and every savings figure nonsense.
pub fn from_litellm_file(path: &str) -> Result<Vec<ModelRow>> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| Error::Config(format!("reading {path}: {e}")))?;
    from_litellm_str(&raw, path)
}

/// The parser itself, over the bytes, whoever fetched them.
///
/// `origin` is only ever printed: an error that does not say which file or URL
/// was empty of usable models is an error an operator cannot act on.
fn from_litellm_str(raw: &str, origin: &str) -> Result<Vec<ModelRow>> {
    let doc: Value = serde_json::from_str(raw)?;
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

        // LiteLLM keys a model either bare (`claude-opus-5`) or under its own
        // provider (`xai/grok-4.6`, `moonshot/kimi-latest-8k`). That prefix is
        // LiteLLM's namespace, not the provider's model name, so it has to come
        // off before the name goes on the wire — xAI answers a request for
        // `xai/grok-4.6` with "Model not found". Only the provider's own prefix
        // is removed: anything else in the name belongs to the model, and
        // Bedrock's `anthropic.claude-…` must survive intact.
        let wire_name = name
            .strip_prefix(&format!("{provider}/"))
            .unwrap_or(name)
            .to_owned();

        out.push(ModelRow {
            id: format!("{known}/{}", name.rsplit('/').next().unwrap_or(name)),
            provider: known.as_str().to_owned(),
            upstream_name: wire_name,
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
            // LiteLLM has no opinion about what to call a model in a picker,
            // and a seed must never write over what an operator called it.
            display_label: None,
        });
    }

    if out.is_empty() {
        return Err(Error::Config(format!(
            "no models in {origin} matched a provider this gateway has an adapter for"
        )));
    }
    Ok(out)
}

/// What a model no catalog seed has ever described is assumed to hold.
///
/// A provider price API states no context window, and xAI's
/// `long_context_threshold` is a price tier rather than a window, so a new row
/// has to guess. Understating is the safe direction: `ModelSpec::satisfies`
/// only ever *rejects* a model whose window is too small, so a low guess costs
/// a routing opportunity while a high guess costs a 400 from upstream on the
/// one request that mattered. 131072 is the smallest window any current Grok
/// model has. A LiteLLM seed replaces it with the real number, and a later
/// price sync leaves that number alone.
const ASSUMED_CONTEXT_WINDOW: i32 = 131_072;

/// Likewise conservative. Only the `/v1/models` listing reads this field — the
/// router takes its output budget from the request — so an understatement is
/// visible to a client and dangerous to nothing.
const ASSUMED_MAX_OUTPUT_TOKENS: i32 = 8_192;

/// One catalog write a native price sync wants to make.
#[derive(Debug)]
pub enum PriceSync {
    /// The catalog already knows this model: change the prices and nothing
    /// else. Everything a price API cannot see — the context window above all —
    /// is already right in that row and must survive the sync.
    Reprice {
        id: String,
        input_per_mtok: Decimal,
        output_per_mtok: Decimal,
        cache_read_per_mtok: Option<Decimal>,
    },
    /// New to the catalog, so there is a whole row to invent around the prices.
    Insert(ModelRow),
}

/// Decide what a provider's stated prices should do to the catalog.
///
/// Pure, and separate from the writes, because the interesting property here is
/// not that it talks to Postgres: it is that a model the catalog already has
/// comes back as `Reprice`, which cannot touch a context window, no matter what
/// the price payload did or did not say.
#[must_use]
pub fn plan_price_sync(
    provider: Provider,
    prices: &[ModelPrice],
    known: &HashSet<String>,
) -> Vec<PriceSync> {
    prices
        .iter()
        .map(|p| {
            let id = format!("{provider}/{}", p.upstream_name);
            if known.contains(&id) {
                return PriceSync::Reprice {
                    id,
                    input_per_mtok: p.input_per_mtok,
                    output_per_mtok: p.output_per_mtok,
                    cache_read_per_mtok: p.cache_read_per_mtok,
                };
            }
            PriceSync::Insert(ModelRow {
                id,
                provider: provider.as_str().to_owned(),
                upstream_name: p.upstream_name.clone(),
                input_per_mtok: p.input_per_mtok,
                output_per_mtok: p.output_per_mtok,
                cache_read_per_mtok: p.cache_read_per_mtok,
                // Nobody quotes a cache *write* price here; a derived one would
                // be a number the ledger cannot defend.
                cache_write_per_mtok: None,
                context_window: ASSUMED_CONTEXT_WINDOW,
                max_output_tokens: ASSUMED_MAX_OUTPUT_TOKENS,
                supports_vision: p.supports_vision,
                // Only what the payload proves. A price list does not mention
                // tools or reasoning, and claiming them makes the router pick a
                // model that then refuses the request; claiming a prompt cache
                // the model lacks makes the ledger bill cache reads that never
                // happened. A LiteLLM seed fills these in for real.
                supports_tools: false,
                supports_reasoning: false,
                supports_prompt_cache: p.cache_read_per_mtok.is_some(),
                // A price API states no name for a picker either, and this is
                // an INSERT: a model the catalog already knows takes the
                // `Reprice` arm above, which names no columns but the prices.
                display_label: None,
            })
        })
        .collect()
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
    fn the_builtin_catalog_includes_the_codex_subscription_model() {
        let models = builtin();
        let sol = models
            .iter()
            .find(|m| m.id == "openai/gpt-5.5")
            .expect("codex model");
        assert_eq!(sol.provider, "openai");
        assert_eq!(sol.upstream_name, "gpt-5.5");
        assert!(sol.supports_reasoning);
        assert!(sol.input_per_mtok > Decimal::ZERO);
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
    fn litellms_own_prefix_does_not_travel_to_the_provider() {
        // LiteLLM keys some models under its own provider namespace. That
        // prefix is not part of the model's name, and sending it upstream is
        // not a cosmetic slip: xAI answers a request for `xai/grok-4.6` with
        // "Model not found", so every model imported this way is unreachable
        // while looking perfectly correct in the catalog. The id keeps the
        // prefix because that is OAG's own namespace; only the wire name loses
        // it. A name that carries no provider prefix must be left alone, and a
        // Bedrock-style name has to survive whole.
        let file = serde_json::json!({
            "xai/grok-4.6":       { "litellm_provider": "xai", "input_cost_per_token": 0.000_002,
                                    "output_cost_per_token": 0.000_006 },
            "moonshot/kimi-k2":   { "litellm_provider": "moonshot", "input_cost_per_token": 0.000_000_6,
                                    "output_cost_per_token": 0.000_002_5 },
            "claude-opus-5":      { "litellm_provider": "anthropic", "input_cost_per_token": 0.000_015,
                                    "output_cost_per_token": 0.000_075 },
            "bedrock/anthropic.claude-sonnet-4-v1:0":
                                  { "litellm_provider": "bedrock", "input_cost_per_token": 0.000_003,
                                    "output_cost_per_token": 0.000_015 },
        });
        let path = std::env::temp_dir().join("oag-litellm-prefix.json");
        std::fs::write(&path, file.to_string()).expect("write");
        let rows = from_litellm_file(path.to_str().expect("path")).expect("parses");

        let wire = |id: &str| {
            rows.iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
                .upstream_name
                .clone()
        };
        assert_eq!(wire("xai/grok-4.6"), "grok-4.6");
        assert_eq!(wire("kimi/kimi-k2"), "kimi-k2");
        // Never carried a prefix; must be untouched.
        assert_eq!(wire("anthropic/claude-opus-5"), "claude-opus-5");
        // Only the provider's own prefix comes off — the rest is the model.
        assert_eq!(
            wire("bedrock/anthropic.claude-sonnet-4-v1:0"),
            "anthropic.claude-sonnet-4-v1:0"
        );
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
    fn a_from_value_with_a_scheme_is_fetched_and_anything_else_is_opened() {
        // The two failures this guards: a path that turns into a network call
        // (and a confusing DNS error), and a URL handed to the filesystem (and
        // a "no such file" naming something that was never a file).
        assert!(is_url(
            "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
        ));
        assert!(is_url("http://localhost:8080/prices.json"));
        assert!(!is_url("./model_prices.json"));
        assert!(!is_url("/etc/oag/model_prices.json"));
        assert!(!is_url("https"));
        // Not a scheme, just a file whose name starts with one.
        assert!(!is_url("https-prices.json"));
    }

    #[test]
    fn a_url_and_a_file_parse_to_the_same_rows() {
        // The split into a fetcher and a parser must not have changed what the
        // parser does; the file path is the behaviour that already shipped.
        let file = serde_json::json!({
            "claude-opus-5": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000_015,
                "output_cost_per_token": 0.000_075,
                "max_input_tokens": 400_000
            }
        })
        .to_string();
        let path = std::env::temp_dir().join("oag-litellm-origin.json");
        std::fs::write(&path, &file).expect("write");

        let from_disk = from_litellm_file(path.to_str().expect("path")).expect("parses");
        let from_wire = from_litellm_str(&file, "https://example.invalid/x.json").expect("parses");

        assert_eq!(from_disk.len(), from_wire.len());
        assert_eq!(from_disk[0].id, from_wire[0].id);
        assert_eq!(from_disk[0].input_per_mtok, from_wire[0].input_per_mtok);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_empty_url_import_names_the_url_it_came_from() {
        // "no models in ." would send the operator hunting through their disk
        // for a table that came off the network.
        let body = serde_json::json!({
            "some-model": { "litellm_provider": "a-provider-we-have-no-adapter-for" }
        })
        .to_string();
        let err = from_litellm_str(&body, "https://example.invalid/x.json")
            .expect_err("no importable models");
        assert!(err.to_string().contains("https://example.invalid/x.json"));
    }

    #[test]
    fn a_native_price_sync_never_touches_a_known_model_s_context_window() {
        // The crux of the whole feature: xAI states no context window, so the
        // only safe thing to do to a model the catalog already holds is change
        // its prices. A `Reprice` structurally cannot carry a window; an
        // `Insert` here would silently replace 500k with a guess.
        let prices = vec![ModelPrice {
            upstream_name: "grok-4.6".to_owned(),
            input_per_mtok: rust_decimal::dec!(2),
            output_per_mtok: rust_decimal::dec!(6),
            cache_read_per_mtok: Some(rust_decimal::dec!(0.5)),
            supports_vision: true,
        }];
        let known: HashSet<String> = ["xai/grok-4.6".to_owned()].into_iter().collect();

        let plan = plan_price_sync(Provider::XAI, &prices, &known);
        assert_eq!(plan.len(), 1);
        match &plan[0] {
            PriceSync::Reprice {
                id,
                input_per_mtok,
                cache_read_per_mtok,
                ..
            } => {
                assert_eq!(id, "xai/grok-4.6");
                assert_eq!(*input_per_mtok, rust_decimal::dec!(2));
                assert_eq!(*cache_read_per_mtok, Some(rust_decimal::dec!(0.5)));
            }
            PriceSync::Insert(m) => {
                panic!("a known model must be repriced, not rewritten: {}", m.id)
            }
        }
    }

    #[test]
    fn a_model_the_catalog_has_never_seen_gets_a_window_that_understates() {
        // A new row has to guess, and the guess must be low: a small window
        // only ever costs a routing opportunity, while a large one costs a 400
        // from upstream on the request that overflowed it.
        let prices = vec![ModelPrice {
            upstream_name: "grok-9-unheard-of".to_owned(),
            input_per_mtok: rust_decimal::dec!(3),
            output_per_mtok: rust_decimal::dec!(15),
            cache_read_per_mtok: None,
            supports_vision: false,
        }];

        let plan = plan_price_sync(Provider::XAI, &prices, &HashSet::new());
        let PriceSync::Insert(row) = &plan[0] else {
            panic!("an unknown model has no prices to update in place");
        };
        assert_eq!(row.id, "xai/grok-9-unheard-of");
        assert_eq!(row.provider, "xai");
        assert_eq!(row.context_window, ASSUMED_CONTEXT_WINDOW);
        // 200000 is xAI's long-context *price* threshold; a window inferred
        // from it would be a made-up number wearing an authoritative one's
        // clothes.
        assert!(row.context_window < 200_000);
        // Nothing in a price list proves a model can call tools.
        assert!(!row.supports_tools);
        assert!(!row.supports_prompt_cache, "no cache price, no cache");
    }

    #[test]
    fn prices_do_not_round_trip_through_binary_float() {
        // 0.0000001 has no exact f64 representation; via the string form it is
        // exact, and prices are multiplied by token counts and summed.
        let v = serde_json::json!(0.000_000_1);
        assert_eq!(decimal(&v), Some(rust_decimal::dec!(0.000_000_1)));
    }
}
