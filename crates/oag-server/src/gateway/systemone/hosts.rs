//! Which System One provider a request names, and what it is sent: the pure
//! half of serving System One endpoints. End to end, against wiremock hosts,
//! it is `tests/systemone_endpoints.rs`.

use super::*;
use oag_core::provider::{Dialect, Endpoint, Platform};
use rust_decimal::Decimal;

fn host(name: &str) -> Provider {
    Provider::Custom(Endpoint::new(name, Dialect::SystemOne, Platform::Plain).expect("a name"))
}

fn row(id: &str, provider: Provider, upstream: &str, input: i64) -> ModelSpec {
    ModelSpec {
        id: ModelId::new(id),
        provider,
        upstream_name: upstream.to_owned(),
        pricing: Pricing {
            input_per_mtok: Decimal::from(input),
            output_per_mtok: Decimal::ZERO,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        },
        context_window: 0,
        max_output_tokens: 0,
        capabilities: Capabilities::default(),
        display_label: None,
    }
}

/// Merge Gateway's model as an operator catalogs it, a Jev row, and a second
/// host's row whose id is not `<host>/<upstream name>`.
fn catalog() -> Catalog {
    Catalog::from_entries([
        row(
            "t7-merge/typesafe/jev-1.13",
            host("t7-merge"),
            "typesafe/jev-1.13",
            1,
        ),
        row("jev/jev-latest", Provider::Jev, "jev-latest", 2),
        row("t7-selfhost/fast", host("t7-selfhost"), "jev-latest", 3),
    ])
}

fn hosts() -> [Provider; 3] {
    [Provider::Jev, host("t7-merge"), host("t7-selfhost")]
}

fn target(provider: Provider, upstream_model: Option<&str>, spec: Option<&str>) -> Target {
    let catalog = catalog();
    Target {
        provider,
        upstream_model: upstream_model.map(str::to_owned),
        spec: spec.map(|id| catalog.get(&ModelId::new(id)).cloned().expect("a row")),
    }
}

/// Everything the built-in Jev was sent before endpoints existed still goes to
/// Jev, byte for byte, and a catalog id reaches it by Jev's own name.
#[test]
fn what_jev_always_answered_still_goes_to_jev() {
    let (catalog, hosts) = (catalog(), hosts());
    let resolve = |model| resolve(model, &catalog, &hosts).expect("resolves");
    let verbatim = target(Provider::Jev, None, None);
    assert_eq!(resolve(None), verbatim, "no model: Jev picks");
    assert_eq!(resolve(Some("jev-latest")), verbatim, "a bare name");
    assert_eq!(
        resolve(Some("jev-9000")),
        verbatim,
        "a bare name no row has"
    );
    assert_eq!(resolve(Some("jev/jev-9000")), verbatim, "Jev's own prefix");
    assert_eq!(
        resolve(Some("typesafe/jev-1.13")),
        verbatim,
        "and its alias, which is how Merge Gateway spells Jev's models"
    );
    assert_eq!(
        resolve(Some("jev/jev-latest")),
        target(Provider::Jev, Some("jev-latest"), Some("jev/jev-latest")),
        "a catalog id is sent the row's upstream name"
    );
}

/// A host's model by its catalog id, by `<host>/<name>` with or without a row,
/// and never a bare name, which stays Jev's.
#[test]
fn a_host_s_model_goes_to_that_host_by_its_own_name() {
    let (catalog, hosts) = (catalog(), hosts());
    let resolve = |model| resolve(Some(model), &catalog, &hosts).expect("resolves");
    assert_eq!(
        resolve("t7-merge/typesafe/jev-1.13"),
        target(
            host("t7-merge"),
            Some("typesafe/jev-1.13"),
            Some("t7-merge/typesafe/jev-1.13")
        ),
        "an id with several slashes is still one catalog id"
    );
    assert_eq!(
        resolve("t7-merge/typesafe/jev-2"),
        target(host("t7-merge"), Some("typesafe/jev-2"), None),
        "a model its host lists and nobody has priced"
    );
    assert_eq!(
        resolve("t7-selfhost/jev-latest"),
        target(
            host("t7-selfhost"),
            Some("jev-latest"),
            Some("t7-selfhost/fast")
        ),
        "priced by the host's row for that upstream name, whatever its id"
    );
    assert_eq!(
        resolve("t7-selfhost/fast"),
        target(
            host("t7-selfhost"),
            Some("jev-latest"),
            Some("t7-selfhost/fast")
        ),
        "and reached by that id too"
    );
}

/// A name that says it is some other provider's is refused before any
/// credential is leased, rather than sent to Jev.
#[test]
fn a_model_no_system_one_provider_serves_is_refused_not_sent_to_jev() {
    let catalog = catalog();
    for (model, hosts) in [
        ("anthropic/claude-haiku-4.5", &hosts()[..]),
        ("oag/auto", &hosts()[..]),
        ("t7-nobody/jev-latest", &hosts()[..]),
        // A host this gateway no longer serves: its requests do not fall to
        // Jev, who was never asked to see them.
        ("t7-merge/typesafe/jev-2", &[Provider::Jev][..]),
    ] {
        let refused = resolve(Some(model), &catalog, hosts).expect_err(model);
        assert!(matches!(refused, Error::NoViableModel(_)), "{model}");
        assert!(
            refused
                .to_string()
                .contains(&format!("'{model}' is not a System One model")),
            "{refused}"
        );
    }
}

/// The model is renamed and nothing else is touched: every other member is
/// the bytes it arrived as, in the order it arrived in.
#[test]
fn a_rename_changes_the_model_and_keeps_every_other_byte() {
    // `\/` is JSON's escape for `/`: a serialiser writes `/`, so a value that
    // still holds `\/` was copied, not rewritten.
    let body = br#"{ "state" : {"n": 123456789012345678901234567890, "s": "a\/b \"q\""},
        "model":"t7-merge/typesafe/jev-1.13",
        "questions": {"q": {"type": "noul", "instructions": "?"}} ,
        "vendor" : "typesafe", "customer": null }"#;
    let renamed = renamed(body, "typesafe/jev-1.13").expect("renames");
    assert_eq!(
        std::str::from_utf8(&renamed).expect("utf-8"),
        r#"{"state":{"n": 123456789012345678901234567890, "s": "a\/b \"q\""},"model":"typesafe/jev-1.13","questions":{"q": {"type": "noul", "instructions": "?"}},"vendor":"typesafe","customer":null}"#
    );
    let request: SystemOneRequest = serde_json::from_slice(&renamed).expect("still a request");
    assert_eq!(request.model.as_deref(), Some("typesafe/jev-1.13"));

    // A name that needs escaping is escaped, and a key spelt with an escape
    // is the key it spells: `model`, with its `d` as a JSON unicode escape.
    let escaped_key = concat!(r#"{"mo"#, "\\", r#"u0064el":"x","state":"s"}"#);
    assert_eq!(escaped_key.len(), 30, "the escape is six characters");
    let quoted = renamed_str(escaped_key.as_bytes(), "a\"b");
    assert_eq!(quoted, r#"{"model":"a\"b","state":"s"}"#);
}

fn renamed_str(body: &[u8], model: &str) -> String {
    String::from_utf8(renamed(body, model).expect("renames").to_vec()).expect("utf-8")
}

#[test]
fn a_body_with_two_models_never_reaches_a_rename() {
    let twice = br#"{"state":"s","model":"t7-merge/a","model":"jev-latest","questions":{"q":{"type":"noul"}}}"#;
    assert!(
        serde_json::from_slice::<SystemOneRequest>(twice).is_err(),
        "the SDK's decoder refuses it before a provider is chosen"
    );
    // And were one to arrive, every model is renamed.
    assert_eq!(
        renamed_str(twice, "a"),
        r#"{"state":"s","model":"a","model":"a","questions":{"q":{"type":"noul"}}}"#
    );
}

/// A body that is not a JSON object is not renamed, and the refusal says what
/// was expected instead: the exact 400 `invalid_request` the route would
/// answer with, rendered by the same `error_response` every refusal goes
/// through.
///
/// No client reaches this today, because the SDK's decoder refuses such a
/// body before a model is resolved, so it is held here rather than end to
/// end. The text is the visitor's `expecting`, which serde puts after
/// "expected"; without it, the operator reads "expected" followed by nothing.
#[tokio::test]
async fn a_body_that_is_not_an_object_is_refused_naming_what_was_expected() {
    for (body, says) in [
        (
            &b"[]"[..],
            "serialisation: invalid type: sequence, expected a JSON object at line 1 column 0",
        ),
        (
            &b"7"[..],
            "serialisation: invalid type: integer `7`, expected a JSON object at line 1 column 1",
        ),
    ] {
        let refused = renamed(body, "typesafe/jev-1.13").expect_err("not an object");
        assert_eq!(refused.to_string(), says);

        let response = error_response(&refused);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let rendered: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .expect("a body"),
        )
        .expect("a JSON envelope");
        assert_eq!(rendered["error"]["type"], "invalid_request");
        assert_eq!(rendered["error"]["message"], says);
    }
}

/// What answered is priced first; a name no row spells falls back to the row
/// the request was resolved by, and then to an unpriced stand-in under the
/// provider that answered.
#[test]
fn an_answer_is_priced_by_what_ran_then_by_what_was_asked() {
    let catalog = catalog();
    let merge = host("t7-merge");
    let asked = catalog
        .get(&ModelId::new("t7-merge/typesafe/jev-1.13"))
        .cloned()
        .expect("a row");

    // Merge answers with the concrete version, which no row spells.
    let concrete = priced(&catalog, merge, "jev-1.13.0", Some(&asked));
    assert_eq!(concrete, asked);

    // A row for what ran wins over the one asked for.
    let ran = priced(&catalog, Provider::Jev, "jev-latest", Some(&asked));
    assert_eq!(ran.id.as_str(), "jev/jev-latest");

    let unpriced = priced(&catalog, merge, "jev-1.13.0", None);
    assert_eq!(unpriced.id.as_str(), "t7-merge/jev-1.13.0");
    assert_eq!(unpriced.provider, merge);
    assert!(unpriced.pricing.input_per_mtok.is_zero());
}
