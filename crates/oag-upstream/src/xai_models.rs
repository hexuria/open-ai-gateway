//! Which Grok models a credential will actually accept.
//!
//! Two surfaces, because a subscription seat and an API key are not allowed to
//! ask the same host. `api.x.ai` answers an API key and refuses a seat token
//! with a billing error; the seat's list lives on the CLI chat proxy, which is
//! the host that seat already uses for quota. A new model shows up here the
//! day that host lists it — the catalog does not have to be edited by hand.

use oag_core::credential::CredentialKind;
use oag_core::{Error, Result};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashSet;
use std::str::FromStr;

const PROXY_LISTS: [&str; 2] = [
    "https://cli-chat-proxy.grok.com/v1/models-v2",
    "https://cli-chat-proxy.grok.com/v1/models",
];

/// USD-per-million scale used by `api.x.ai`'s language-model prices: 20000 is
/// $2.00. Same factor as `pricing::xai`.
const PER_MTOK_SCALE: i64 = 10_000;

/// One model a credential's own list named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedModel {
    pub upstream_name: String,
    pub input_per_mtok: Option<Decimal>,
    pub output_per_mtok: Option<Decimal>,
    pub cache_read_per_mtok: Option<Decimal>,
    pub context_window: Option<i32>,
    pub max_output_tokens: Option<i32>,
    pub supports_vision: Option<bool>,
}

/// Enough of an existing catalog row to price a model the list did not price.
// The capability flags mirror the catalog columns; folding them into an enum
// would just mean unfolding them again when the row is written.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy)]
pub struct Donor<'a> {
    pub upstream_name: &'a str,
    pub input_per_mtok: Decimal,
    pub output_per_mtok: Decimal,
    pub cache_read_per_mtok: Option<Decimal>,
    pub context_window: i32,
    pub max_output_tokens: i32,
    pub supports_vision: bool,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
    pub supports_prompt_cache: bool,
}

/// A catalog row to insert. Never an update: a list must not overwrite a
/// window or a price an operator or a previous seed already recorded.
// Same flags as `Donor`, for the same reason.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogInsert {
    pub upstream_name: String,
    pub input_per_mtok: Decimal,
    pub output_per_mtok: Decimal,
    pub cache_read_per_mtok: Option<Decimal>,
    pub context_window: i32,
    pub max_output_tokens: i32,
    pub supports_vision: bool,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
    pub supports_prompt_cache: bool,
}

/// Ask this credential which chat models it serves.
///
/// An empty answer is an error, not a list. Callers record the names on the
/// credential, and an empty array means "serves nothing" — which would clear
/// the picker. A payload we could not read must leave the previous answer
/// where it was.
pub async fn list(
    kind: CredentialKind,
    access_token: &str,
    proxy: Option<&str>,
) -> Result<Vec<ListedModel>> {
    let listed = match kind {
        CredentialKind::ApiKey => api_key_list(access_token, proxy).await?,
        CredentialKind::OAuth => proxy_list(access_token, proxy).await?,
        // A seat token and an API key are the two xAI credentials that have a
        // model list. Anything else presented here is a programming error.
        other => {
            return Err(Error::Internal(format!(
                "xai model list is not defined for a {other:?} credential"
            )));
        }
    };
    if listed.is_empty() {
        return Err(Error::Internal(
            "xai model list named no chat models".to_owned(),
        ));
    }
    Ok(listed)
}

async fn api_key_list(access_token: &str, proxy: Option<&str>) -> Result<Vec<ListedModel>> {
    let prices = crate::pricing::xai::fetch(access_token, proxy).await?;
    Ok(prices
        .into_iter()
        .map(|p| ListedModel {
            upstream_name: p.upstream_name,
            input_per_mtok: Some(p.input_per_mtok),
            output_per_mtok: Some(p.output_per_mtok),
            cache_read_per_mtok: p.cache_read_per_mtok,
            context_window: None,
            max_output_tokens: None,
            supports_vision: Some(p.supports_vision),
        })
        .collect())
}

async fn proxy_list(access_token: &str, proxy: Option<&str>) -> Result<Vec<ListedModel>> {
    let client = crate::side_channel_client(proxy, std::time::Duration::from_secs(10))?;
    let mut last = Error::Internal("xai model list was not attempted".to_owned());
    for url in PROXY_LISTS {
        match fetch_body(&client, url, access_token).await {
            Ok(body) => match parse_listing(&body) {
                Ok(models) if !models.is_empty() => return Ok(models),
                Ok(_) => {
                    last = Error::Internal(format!("{url} named no chat models"));
                }
                Err(e) => last = e,
            },
            Err(e) => last = e,
        }
    }
    Err(last)
}

async fn fetch_body(client: &reqwest::Client, url: &str, access_token: &str) -> Result<String> {
    let response = client
        .get(url)
        .header("authorization", format!("Bearer {}", access_token.trim()))
        // The same CLI marker the billing call sends. The proxy is what a
        // seat is allowed to ask; the public API is not.
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Internal(format!("xai model list request {url}: {e}")))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::Internal(format!(
            "xai model list {url} returned {status}: {}",
            truncate(&body)
        )));
    }
    Ok(body)
}

/// Pull chat models out of either listing shape.
///
/// The proxy has answered both an OpenAI `{data:[{id}]}` list and a
/// `{models:[{id, context_window}]}` document, and the field that carries the
/// id is not stable across the two. An unrecognised body is an error: turning
/// it into an empty list would clear the picker.
pub fn parse_listing(body: &str) -> Result<Vec<ListedModel>> {
    let json: Value = serde_json::from_str(body)
        .map_err(|e| Error::Internal(format!("xai model list is not JSON: {e}")))?;
    let rows = match &json {
        Value::Object(o) => o
            .get("models")
            .or_else(|| o.get("data"))
            .and_then(Value::as_array),
        Value::Array(rows) => Some(rows),
        _ => None,
    };
    let listed: Vec<ListedModel> = rows.into_iter().flatten().filter_map(parse_one).collect();
    if listed.is_empty() {
        return Err(Error::Internal(format!(
            "xai model list: no ids in a payload shaped {}",
            truncate(body)
        )));
    }
    Ok(listed)
}

fn parse_one(row: &Value) -> Option<ListedModel> {
    let name = match row {
        Value::String(s) => s.clone(),
        other => model_name(other)?.to_owned(),
    };
    if name.is_empty() || !is_chat_name(&name, row) {
        return None;
    }
    let (input, output, cache) = prices_of(row);
    Some(ListedModel {
        upstream_name: name,
        input_per_mtok: input,
        output_per_mtok: output,
        cache_read_per_mtok: cache,
        context_window: i32_field(row, &["context_window", "contextWindow"]),
        max_output_tokens: i32_field(row, &["max_output_tokens", "maxOutputTokens"]),
        supports_vision: vision_of(row),
    })
}

fn model_name(row: &Value) -> Option<&str> {
    row["id"]
        .as_str()
        .or_else(|| row["slug"].as_str())
        .or_else(|| row["model"].as_str())
        .or_else(|| {
            let nested = &row["model"];
            nested["id"]
                .as_str()
                .or_else(|| nested["slug"].as_str())
                .or_else(|| nested["model"].as_str())
        })
        .filter(|s| !s.is_empty())
}

fn is_chat_name(name: &str, row: &Value) -> bool {
    let modalities = modalities(row, "output_modalities")
        .or_else(|| modalities(&row["model"], "output_modalities"));
    if let Some(mods) = modalities
        && !mods.is_empty()
        && !mods.contains(&"text")
    {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    !(lower.starts_with("grok-imagine")
        || lower.starts_with("grok-voice")
        || lower.starts_with("grok-tts")
        || lower.starts_with("grok-stt"))
}

fn modalities<'a>(row: &'a Value, key: &str) -> Option<Vec<&'a str>> {
    row.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
}

fn vision_of(row: &Value) -> Option<bool> {
    if let Some(flag) = row["supports_vision"].as_bool() {
        return Some(flag);
    }
    let mods = modalities(row, "input_modalities")
        .or_else(|| modalities(&row["model"], "input_modalities"))?;
    if mods.is_empty() {
        return None;
    }
    Some(mods.contains(&"image"))
}

fn prices_of(row: &Value) -> (Option<Decimal>, Option<Decimal>, Option<Decimal>) {
    let source = if row.get("prompt_text_token_price").is_some() {
        row
    } else if row["model"].get("prompt_text_token_price").is_some() {
        &row["model"]
    } else {
        row
    };
    if let Some(raw) = source["prompt_text_token_price"].as_i64() {
        return (
            Some(per_mtok(raw)),
            Some(per_mtok(
                source["completion_text_token_price"].as_i64().unwrap_or(0),
            )),
            source["cached_prompt_text_token_price"]
                .as_i64()
                .map(per_mtok),
        );
    }
    (None, None, None)
}

fn per_mtok(raw: i64) -> Decimal {
    Decimal::from(raw) / Decimal::from(PER_MTOK_SCALE)
}

fn i32_field(row: &Value, keys: &[&str]) -> Option<i32> {
    for key in keys {
        if let Some(n) = row[key].as_i64().and_then(|n| i32::try_from(n).ok()) {
            return Some(n);
        }
        if let Some(n) = row["model"][key]
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
        {
            return Some(n);
        }
    }
    None
}

fn truncate(body: &str) -> String {
    const MAX: usize = 300;
    let mut end = MAX.min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    if body.len() <= MAX {
        body.to_owned()
    } else {
        format!("{}…", &body[..end])
    }
}

/// Published list prices for models whose own list states no money.
///
/// A seat's proxy names models and context windows. It does not price them.
/// These are the API list prices — the counterfactual a flat-rate seat is
/// measured against — for the current flagship. A later model with no entry
/// here borrows the dearest Grok row already in the catalog rather than being
/// dropped, and rather than being inserted at zero, which would win every
/// cost comparison.
struct Published {
    upstream: &'static str,
    input: &'static str,
    output: &'static str,
    cached: &'static str,
    context: i32,
    max_output: i32,
    vision: bool,
    reasoning: bool,
}

const PUBLISHED: &[Published] = &[
    // Grok 4.7, September 2026. $2 / $0.50 cached / $6 per million under
    // 200k, 500k context, text and image in, text out. The proxy also serves
    // the same model as `grok-4.7-build`. Fast is the same model at twice the
    // token rates and is not on the public API.
    Published {
        upstream: "grok-4.7",
        input: "2",
        output: "6",
        cached: "0.50",
        context: 500_000,
        max_output: 128_000,
        vision: true,
        reasoning: true,
    },
    Published {
        upstream: "grok-4.7-build",
        input: "2",
        output: "6",
        cached: "0.50",
        context: 500_000,
        max_output: 128_000,
        vision: true,
        reasoning: true,
    },
    Published {
        upstream: "grok-4.7-build-fast",
        input: "4",
        output: "12",
        cached: "1",
        context: 500_000,
        max_output: 128_000,
        vision: true,
        reasoning: true,
    },
];

fn published(name: &str) -> Option<&'static Published> {
    PUBLISHED.iter().find(|p| p.upstream == name)
}

fn money(raw: &str) -> Option<Decimal> {
    Decimal::from_str(raw).ok()
}

/// Understating a window is the safe direction: the router only rejects a
/// model whose window is too small. Same numbers the price-sync path uses
/// when a provider states no window at all.
const ASSUMED_CONTEXT: i32 = 131_072;
const ASSUMED_MAX_OUTPUT: i32 = 8_192;

/// Catalog rows for models this list named and the catalog has never seen.
///
/// `already` is upstream names, not catalog ids. An existing row is left
/// completely alone — this function cannot see a context window it should
/// preserve, so it does not try to update one.
#[must_use]
pub fn inserts_for<S: std::hash::BuildHasher>(
    listed: &[ListedModel],
    already: &HashSet<String, S>,
    donors: &[Donor<'_>],
) -> Vec<CatalogInsert> {
    let donor = donors
        .iter()
        .max_by(|a, b| a.input_per_mtok.cmp(&b.input_per_mtok));
    let mut out = Vec::new();
    for model in listed {
        if already.contains(&model.upstream_name) {
            continue;
        }
        let Some(insert) = insert_one(model, donor.copied()) else {
            continue;
        };
        out.push(insert);
    }
    out
}

fn insert_one(model: &ListedModel, donor: Option<Donor<'_>>) -> Option<CatalogInsert> {
    let published = published(&model.upstream_name);
    let input = model
        .input_per_mtok
        .or_else(|| published.and_then(|p| money(p.input)))
        .or_else(|| donor.map(|d| d.input_per_mtok))?;
    let output = model
        .output_per_mtok
        .or_else(|| published.and_then(|p| money(p.output)))
        .or_else(|| donor.map(|d| d.output_per_mtok))?;
    if input.is_zero() && output.is_zero() {
        return None;
    }
    let cache = model.cache_read_per_mtok.or_else(|| {
        published
            .and_then(|p| money(p.cached))
            .or_else(|| donor.and_then(|d| d.cache_read_per_mtok))
    });
    let context = model
        .context_window
        .or_else(|| published.map(|p| p.context))
        .or_else(|| donor.map(|d| d.context_window))
        .unwrap_or(ASSUMED_CONTEXT);
    let max_output = model
        .max_output_tokens
        .or_else(|| published.map(|p| p.max_output))
        .or_else(|| donor.map(|d| d.max_output_tokens))
        .unwrap_or(ASSUMED_MAX_OUTPUT);
    Some(CatalogInsert {
        upstream_name: model.upstream_name.clone(),
        input_per_mtok: input,
        output_per_mtok: output,
        cache_read_per_mtok: cache,
        context_window: context,
        max_output_tokens: max_output,
        supports_vision: model
            .supports_vision
            .or_else(|| published.map(|p| p.vision))
            .or_else(|| donor.map(|d| d.supports_vision))
            .unwrap_or(false),
        // A published Grok chat model takes tools. A borrowed row keeps
        // whatever the donor was willing to claim.
        supports_tools: published.is_some() || donor.is_some_and(|d| d.supports_tools),
        supports_reasoning: published
            .map(|p| p.reasoning)
            .or_else(|| donor.map(|d| d.supports_reasoning))
            .unwrap_or(false),
        supports_prompt_cache: cache.is_some() || donor.is_some_and(|d| d.supports_prompt_cache),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn an_openai_listing_yields_ids_and_drops_image_models() {
        let body = r#"{ "data": [
            { "id": "grok-4.7" },
            { "id": "grok-4.6" },
            { "id": "grok-imagine-1" },
            { "id": "grok-voice-1" }
        ] }"#;
        let listed = parse_listing(body).expect("ids");
        let names: Vec<_> = listed.iter().map(|m| m.upstream_name.as_str()).collect();
        assert_eq!(names, ["grok-4.7", "grok-4.6"]);
    }

    #[test]
    fn a_models_document_keeps_the_window_and_the_scaled_price() {
        let body = r#"{ "models": [ {
            "id": "grok-4.7",
            "context_window": 500000,
            "prompt_text_token_price": 20000,
            "completion_text_token_price": 60000,
            "cached_prompt_text_token_price": 5000,
            "input_modalities": ["text", "image"],
            "output_modalities": ["text"]
        } ] }"#;
        let listed = parse_listing(body).expect("models");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].upstream_name, "grok-4.7");
        assert_eq!(listed[0].context_window, Some(500_000));
        assert_eq!(listed[0].input_per_mtok, Some(dec!(2)));
        assert_eq!(listed[0].output_per_mtok, Some(dec!(6)));
        assert_eq!(listed[0].cache_read_per_mtok, Some(dec!(0.5)));
        assert_eq!(listed[0].supports_vision, Some(true));
    }

    #[test]
    fn a_nested_model_object_still_names_the_model() {
        let body =
            r#"{ "models": [ { "model": { "id": "grok-4.7", "context_window": 500000 } } ] }"#;
        let listed = parse_listing(body).expect("nested");
        assert_eq!(listed[0].upstream_name, "grok-4.7");
        assert_eq!(listed[0].context_window, Some(500_000));
    }

    #[test]
    fn a_body_with_no_ids_is_an_error() {
        let err = parse_listing(r#"{ "models": [] }"#).expect_err("empty");
        assert!(err.to_string().contains("no ids"), "{err}");
    }

    #[test]
    fn grok_4_7_is_priced_from_the_published_list_when_the_payload_is_not() {
        let listed = vec![ListedModel {
            upstream_name: "grok-4.7".to_owned(),
            input_per_mtok: None,
            output_per_mtok: None,
            cache_read_per_mtok: None,
            context_window: None,
            max_output_tokens: None,
            supports_vision: None,
        }];
        let rows = inserts_for(&listed, &HashSet::new(), &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].upstream_name, "grok-4.7");
        assert_eq!(rows[0].input_per_mtok, dec!(2));
        assert_eq!(rows[0].output_per_mtok, dec!(6));
        assert_eq!(rows[0].cache_read_per_mtok, Some(dec!(0.5)));
        assert_eq!(rows[0].context_window, 500_000);
        assert!(rows[0].supports_vision);
        assert!(rows[0].supports_reasoning);
        assert!(rows[0].supports_tools);
    }

    #[test]
    fn a_model_the_catalog_already_has_is_not_rewritten() {
        let listed = vec![ListedModel {
            upstream_name: "grok-4.7".to_owned(),
            input_per_mtok: Some(dec!(99)),
            output_per_mtok: Some(dec!(99)),
            cache_read_per_mtok: None,
            context_window: Some(1),
            max_output_tokens: None,
            supports_vision: Some(false),
        }];
        let already = HashSet::from(["grok-4.7".to_owned()]);
        assert!(inserts_for(&listed, &already, &[]).is_empty());
    }

    #[test]
    fn an_unpriced_unknown_model_borrows_the_dearest_existing_grok() {
        let listed = vec![ListedModel {
            upstream_name: "grok-4.8".to_owned(),
            input_per_mtok: None,
            output_per_mtok: None,
            cache_read_per_mtok: None,
            context_window: Some(500_000),
            max_output_tokens: None,
            supports_vision: None,
        }];
        let cheap = Donor {
            upstream_name: "grok-4.3",
            input_per_mtok: dec!(1.25),
            output_per_mtok: dec!(2.5),
            cache_read_per_mtok: None,
            context_window: 1_000_000,
            max_output_tokens: 8_192,
            supports_vision: false,
            supports_tools: true,
            supports_reasoning: true,
            supports_prompt_cache: false,
        };
        let flagship = Donor {
            upstream_name: "grok-4.6",
            input_per_mtok: dec!(2),
            output_per_mtok: dec!(6),
            cache_read_per_mtok: Some(dec!(0.5)),
            context_window: 500_000,
            max_output_tokens: 128_000,
            supports_vision: true,
            supports_tools: true,
            supports_reasoning: true,
            supports_prompt_cache: true,
        };
        let rows = inserts_for(&listed, &HashSet::new(), &[cheap, flagship]);
        assert_eq!(rows[0].input_per_mtok, dec!(2));
        assert_eq!(rows[0].output_per_mtok, dec!(6));
        assert_eq!(rows[0].context_window, 500_000, "the list's window wins");
        assert!(rows[0].supports_vision);
    }

    #[test]
    fn a_stated_price_beats_the_published_table() {
        let listed = vec![ListedModel {
            upstream_name: "grok-4.7".to_owned(),
            input_per_mtok: Some(dec!(3)),
            output_per_mtok: Some(dec!(9)),
            cache_read_per_mtok: Some(dec!(1)),
            context_window: None,
            max_output_tokens: None,
            supports_vision: Some(true),
        }];
        let rows = inserts_for(&listed, &HashSet::new(), &[]);
        assert_eq!(rows[0].input_per_mtok, dec!(3));
        assert_eq!(rows[0].output_per_mtok, dec!(9));
        assert_eq!(rows[0].context_window, 500_000);
    }
}
