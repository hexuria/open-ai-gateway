//! OpenRouter's public model list, read for one thing: the reasoning-effort
//! levels it states for a few hundred models, and each one's default.
//!
//! `GET https://openrouter.ai/api/v1/models` needs no key and is sent none.
//! It is read when the catalog is written — `oag admin endpoint sync`, `oag
//! admin catalog sync-efforts` — and never per request: what it says is
//! stored with each row, beside the committed overrides that win over it (see
//! [`oag_router::efforts`]).
//!
//! An entry's `reasoning.supported_efforts` lists its levels highest first,
//! unlabelled, beside `reasoning.default_effort`. An entry without them
//! (`reasoning` null, or only `mandatory`) states no levels, and nor does one
//! naming a level [`oag_router::efforts::LEVELS`] does not hold or a default
//! it does not list: those cannot be ordered, and none are guessed.

use oag_core::{Error, Result};
use oag_router::efforts::{ReasoningEfforts, Snapshot};
use serde_json::Value;
use std::time::Duration;

/// Where OpenRouter lists its models.
pub const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// The list is under a megabyte, and an operator is waiting on it.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Read the list at `url`, and the levels its entries state.
///
/// An unreadable list is an error, and so is one that states levels for no
/// model at all: that is a list in a shape this release does not read, and
/// stored, it would clear every row's levels.
pub async fn reasoning_efforts(url: &str) -> Result<Snapshot> {
    // The list is public, so there is no credential and no credential's
    // proxy. Still the client every side channel uses: one that follows no
    // redirect and gives up.
    let client = crate::side_channel_client(None, TIMEOUT)?;
    let response = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Internal(format!("OpenRouter's model list {url}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Internal(format!(
            "OpenRouter's model list {url} returned {status}"
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| Error::Internal(format!("OpenRouter's model list {url} is not JSON: {e}")))?;
    parse(&body, url)
}

/// The levels each entry of one list states.
fn parse(body: &Value, url: &str) -> Result<Snapshot> {
    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Internal(format!("{url} did not answer with a model list")))?;
    let snapshot = Snapshot::new(entries.iter().filter_map(stated));
    if snapshot.is_empty() {
        return Err(Error::Internal(format!(
            "OpenRouter's model list at {url} states reasoning-effort levels for no model, so \
             it is not in a shape this release reads, and nothing was written"
        )));
    }
    Ok(snapshot)
}

/// One entry's id and levels, if it states levels that can be used.
fn stated(entry: &Value) -> Option<(String, ReasoningEfforts)> {
    let id = entry.get("id")?.as_str()?;
    let reasoning = entry.get("reasoning")?;
    let levels: Vec<&str> = reasoning
        .get("supported_efforts")?
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<_>>()?;
    let default = reasoning.get("default_effort")?.as_str()?;
    Some((id.to_owned(), ReasoningEfforts::new(levels, default)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_router::efforts::{Overrides, plan};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Eight entries of OpenRouter's list as it was served on 2026-10-03,
    /// descriptions cut short.
    const LISTED: &str = include_str!("../tests/fixtures/openrouter-models.json");

    fn listed() -> Value {
        serde_json::from_str(LISTED).expect("the fixture is JSON")
    }

    /// The levels a snapshot holds for `vendor/model`, as an endpoint's row
    /// naming it would store them: `(values, default)`.
    fn levels_of(snapshot: &Snapshot, name: &str) -> Option<(Vec<&'static str>, &'static str)> {
        let id = format!("t7-endpoint/{name}");
        let (planned, _) = plan(
            [(id.as_str(), "t7-endpoint")],
            &Overrides::default(),
            snapshot,
        );
        planned
            .into_iter()
            .next()
            .and_then(|(_, efforts)| efforts)
            .map(|e| (e.values(), e.default_level().value))
    }

    #[test]
    fn a_recorded_list_yields_each_models_levels_lowest_first() {
        let snapshot = parse(&listed(), MODELS_URL).expect("levels");
        // Listed `xhigh, high, medium, low, none`.
        assert_eq!(
            levels_of(&snapshot, "openai/gpt-5.5"),
            Some((vec!["none", "low", "medium", "high", "xhigh"], "medium"))
        );
        assert_eq!(
            levels_of(&snapshot, "openai/gpt-5.6-terra"),
            Some((
                vec!["none", "low", "medium", "high", "xhigh", "max"],
                "medium"
            ))
        );
        // OpenRouter's `x-ai` is the built-in `xai`.
        assert_eq!(
            levels_of(&snapshot, "xai/grok-4.6"),
            Some((vec!["low", "medium", "high", "xhigh"], "high"))
        );
        assert_eq!(
            levels_of(&snapshot, "zhipu/glm-5.3-flash"),
            Some((vec!["low", "high", "max"], "max"))
        );
        assert_eq!(
            levels_of(&snapshot, "gemini/gemini-3.6-flash"),
            Some((vec!["minimal", "low", "medium", "high"], "medium"))
        );
        // `reasoning: {"mandatory": false}` and `reasoning: null` state none.
        assert_eq!(levels_of(&snapshot, "anthropic/claude-sonnet-4.5"), None);
        assert_eq!(levels_of(&snapshot, "kimi/kimi-k2"), None);
        assert_eq!(snapshot.len(), 6);
    }

    #[test]
    fn an_entry_whose_levels_cannot_be_ordered_states_none() {
        let snapshot = parse(
            &json!({"data": [
                {"id": "a/known", "reasoning": {"supported_efforts": ["high", "low"],
                                                "default_effort": "low"}},
                {"id": "a/unknown-level", "reasoning": {"supported_efforts": ["turbo", "low"],
                                                        "default_effort": "low"}},
                {"id": "a/unlisted-default", "reasoning": {"supported_efforts": ["high"],
                                                           "default_effort": "low"}},
                {"id": "a/no-default", "reasoning": {"supported_efforts": ["high"]}},
                {"id": "a/not-words", "reasoning": {"supported_efforts": ["high", 3],
                                                    "default_effort": "high"}},
                {"reasoning": {"supported_efforts": ["high"], "default_effort": "high"}}
            ]}),
            MODELS_URL,
        )
        .expect("one entry states levels");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            levels_of(&snapshot, "a/known"),
            Some((vec!["low", "high"], "low"))
        );
    }

    #[test]
    fn a_list_that_is_not_one_or_states_no_levels_is_an_error() {
        for (body, says) in [
            (json!({"error": "nope"}), "did not answer with a model list"),
            (json!({"data": {}}), "did not answer with a model list"),
            (
                json!({"data": [{"id": "a/m", "reasoning": null}]}),
                "states reasoning-effort levels for no model",
            ),
            (
                json!({"data": []}),
                "states reasoning-effort levels for no model",
            ),
        ] {
            let err = parse(&body, MODELS_URL).expect_err("unusable").to_string();
            assert!(err.contains(says), "{body}: {err}");
        }
    }

    #[tokio::test]
    async fn the_list_is_read_without_a_key_and_its_levels_returned() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(LISTED, "application/json"))
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/api/v1/models", server.uri());
        let snapshot = reasoning_efforts(&url).await.expect("read");
        assert_eq!(snapshot, parse(&listed(), &url).expect("levels"));
        let sent = server.received_requests().await.expect("recorded");
        assert_eq!(sent.len(), 1);
        assert!(
            !sent[0].headers.contains_key("authorization"),
            "a public list is sent no credential"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn a_list_that_fails_or_is_not_json_is_an_error_naming_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/down"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/html"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<html>", "text/html"))
            .mount(&server)
            .await;
        // A redirect is answered, not followed: it is not the list.
        Mock::given(method("GET"))
            .and(path("/moved"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/html", server.uri())),
            )
            .mount(&server)
            .await;
        for (at, says) in [
            ("/down", "returned 503 Service Unavailable"),
            ("/html", "is not JSON"),
            ("/moved", "returned 302 Found"),
        ] {
            let url = format!("{}{at}", server.uri());
            let err = reasoning_efforts(&url).await.expect_err(at).to_string();
            assert!(err.contains(says), "{at}: {err}");
            assert!(err.contains(&url), "{at}: {err}");
        }
        let unreachable = reasoning_efforts("http://127.0.0.1:1/api/v1/models")
            .await
            .expect_err("nothing listens there")
            .to_string();
        assert!(
            unreachable.starts_with("OpenRouter's model list http://127.0.0.1:1/"),
            "{unreachable}"
        );
    }
}
