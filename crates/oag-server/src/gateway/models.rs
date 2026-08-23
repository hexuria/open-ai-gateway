//! Model discovery: what this caller may actually ask for.
//!
//! Clients call `/v1/models` on startup to populate a picker and to validate a
//! configured model name. Without it they either fail closed or fall back to a
//! hardcoded list that has nothing to do with this deployment.
//!
//! The answer is per-caller, not a catalog dump. It is the intersection of the
//! route's ladder, the key's floor, and the providers this route actually holds
//! credentials for — because a model listed but unreachable turns into a
//! failure much later and much further from its cause. That also makes this an
//! entitlement surface, which is why it authenticates.

use super::{Caller, error_response, policy_for};
use crate::AppState;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use oag_core::{Provider, TierName, tier::RoutingMode};
use oag_router::Entitlement;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

/// `GET /v1/models` and `/models`.
pub async fn list(State(state): State<Arc<AppState>>, Caller(auth): Caller) -> Response {
    let Resolved {
        policy,
        providers,
        mode,
    } = match resolve(&state, &auth).await {
        Ok(r) => r,
        Err(response) => return response,
    };

    let catalog = state.catalog().await;
    let concrete = policy.entitled(&mode, &catalog, &providers);

    // Virtual names first: they are the cost-routing entry point, and a client
    // rendering a picker shows the top of the list.
    let mut data: Vec<Value> = virtual_rungs(&policy)
        .map(|rung| virtual_entry(rung.as_ref()))
        .collect();
    data.extend(concrete.iter().map(concrete_entry));
    envelope(data, &mode).into_response()
}

/// `GET /v1beta/models`, which uses a different envelope and different field
/// names. Deliberately does not share a renderer with [`list`].
pub async fn list_gemini(State(state): State<Arc<AppState>>, Caller(auth): Caller) -> Response {
    let Resolved {
        policy,
        providers,
        mode,
    } = match resolve(&state, &auth).await {
        Ok(r) => r,
        Err(response) => return response,
    };

    let catalog = state.catalog().await;
    let concrete = policy.entitled(&mode, &catalog, &providers);

    // A virtual name has no single window of its own, so it advertises the
    // widest one it could resolve to.
    let window = concrete
        .iter()
        .map(|e| e.spec.context_window)
        .max()
        .unwrap_or(0);
    let max_output = concrete
        .iter()
        .map(|e| e.spec.max_output_tokens)
        .max()
        .unwrap_or(0);

    let mut models: Vec<Value> = virtual_rungs(&policy)
        .map(|rung| gemini_virtual_entry(rung.as_ref(), window, max_output))
        .collect();
    models.extend(concrete.iter().map(gemini_entry));
    axum::Json(json!({ "models": models })).into_response()
}

struct Resolved {
    policy: oag_router::RoutingPolicy,
    providers: BTreeSet<Provider>,
    mode: RoutingMode,
}

/// Who is asking, and what the route lets them reach.
async fn resolve(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
) -> Result<Resolved, Response> {
    let (route, policy) = policy_for(state, auth)
        .await
        .map_err(|e| error_response(&e))?;
    let providers = providers_for(state, route.id, auth.principal_id)
        .await
        .map_err(|e| error_response(&e))?;

    Ok(Resolved {
        policy,
        providers,
        mode: if route.default_mode == "managed" {
            RoutingMode::Managed
        } else {
            RoutingMode::Passthrough
        },
    })
}

/// `oag/auto`, then one per advertised rung. `None` is `auto`.
fn virtual_rungs(policy: &oag_router::RoutingPolicy) -> impl Iterator<Item = Option<TierName>> {
    std::iter::once(None).chain(policy.virtual_names().into_iter().map(Some))
}

/// Providers the route holds usable credentials for.
///
/// `account.provider` is free text with no CHECK constraint, so one row nobody
/// can parse must narrow the answer rather than 500 the whole listing.
async fn providers_for(
    state: &Arc<AppState>,
    route_id: uuid::Uuid,
    principal_id: uuid::Uuid,
) -> oag_core::Result<BTreeSet<Provider>> {
    let raw = oag_store::repo::route_providers(&state.db, route_id, principal_id).await?;
    Ok(raw
        .into_iter()
        .filter_map(|name| {
            let parsed = name.parse::<Provider>().ok();
            if parsed.is_none() {
                tracing::warn!(
                    provider = %name,
                    "account.provider is not a known provider; not listing its models"
                );
            }
            parsed
        })
        .collect())
}

/// One concrete model.
///
/// A superset of the OpenAI and Anthropic shapes, because both dialects are
/// served from the same base URL and the caller's SDK is not knowable from the
/// request — `extract_key` accepts all three header spellings from any client,
/// so auth headers carry no dialect signal.
fn concrete_entry(e: &Entitlement) -> Value {
    entry(
        e.spec.id.as_str(),
        e.spec.provider.as_str(),
        json!({
            "tier": e.tier.as_ref().map(TierName::as_str),
            "provider": e.spec.provider.as_str(),
            "virtual": false,
            "honoured": e.honoured,
            "context_window": e.spec.context_window,
            "max_output_tokens": e.spec.max_output_tokens,
            "capabilities": {
                "vision": e.spec.capabilities.vision,
                "tools": e.spec.capabilities.tools,
                "reasoning": e.spec.capabilities.reasoning,
                "prompt_cache": e.spec.capabilities.prompt_cache,
            },
        }),
    )
}

/// One virtual name. `None` is `oag/auto`.
///
/// Must emit the identical field set to `concrete_entry`: virtual entries sort
/// first, so a thin one fails SDK validation on element 0 and breaks
/// `models.list()` for every model, not just this one.
fn virtual_entry(rung: Option<&TierName>) -> Value {
    let id = rung.map_or_else(|| "oag/auto".to_owned(), |r| format!("oag/{}", r.as_str()));
    entry(
        &id,
        "oag",
        json!({
            "tier": rung.map(TierName::as_str),
            "provider": "oag",
            "virtual": true,
            "honoured": true,
            "context_window": Value::Null,
            "max_output_tokens": Value::Null,
            "capabilities": Value::Null,
        }),
    )
}

fn entry(id: &str, owned_by: &str, oag: Value) -> Value {
    let mut entry = json!({
        // OpenAI's shape.
        "id": id,
        "object": "model",
        "created": 0,
        "owned_by": owned_by,
        // Anthropic's shape, for the same object.
        "type": "model",
        "display_name": id,
        "created_at": "1970-01-01T00:00:00Z",
    });
    // Ours. No pricing: that is the organisation's cost data, and it stays on
    // the admin listener where it already lives.
    entry["oag"] = oag;
    entry
}

fn envelope(data: Vec<Value>, mode: &RoutingMode) -> axum::Json<Value> {
    // Null when the list is empty, rather than indexing into it.
    let first = data
        .first()
        .and_then(|m| m["id"].as_str())
        .map(String::from);
    let last = data.last().and_then(|m| m["id"].as_str()).map(String::from);
    let mut body = json!({
        "object": "list",
        "has_more": false,
        "first_id": first,
        "last_id": last,
        "oag": {
            "mode": if *mode == RoutingMode::Managed { "managed" } else { "passthrough" },
        },
    });
    body["data"] = Value::Array(data);
    axum::Json(body)
}

fn gemini_entry(e: &Entitlement) -> Value {
    gemini(
        e.spec.id.as_str(),
        e.spec.context_window,
        e.spec.max_output_tokens,
    )
}

fn gemini_virtual_entry(rung: Option<&TierName>, window: u32, max_output: u32) -> Value {
    let id = rung.map_or_else(|| "oag/auto".to_owned(), |r| format!("oag/{}", r.as_str()));
    gemini(&id, window, max_output)
}

fn gemini(id: &str, window: u32, max_output: u32) -> Value {
    json!({
        // `models/<id>` round-trips through the existing
        // `POST /v1beta/models/{*model_action}` route, which splits on the
        // final colon rather than on a slash.
        "name": format!("models/{id}"),
        "displayName": id,
        "inputTokenLimit": window,
        "outputTokenLimit": max_output,
        "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_router::catalog::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::Decimal;

    fn spec() -> ModelSpec {
        ModelSpec {
            id: ModelId::new("anthropic/claude-opus-5"),
            provider: Provider::Anthropic,
            upstream_name: "claude-opus-5".to_owned(),
            pricing: Pricing {
                input_per_mtok: Decimal::from(15),
                output_per_mtok: Decimal::from(75),
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 200_000,
            max_output_tokens: 64_000,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                reasoning: true,
                prompt_cache: true,
            },
        }
    }

    fn keys_of(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v
            .as_object()
            .expect("object")
            .keys()
            .map(String::clone)
            .collect();
        k.sort();
        k
    }

    #[test]
    fn a_virtual_entry_has_the_same_shape_as_a_concrete_one() {
        // Virtual entries sort first, so a thin one fails SDK validation on
        // element 0 and breaks models.list() for every model, not just itself.
        let spec = spec();
        let concrete = concrete_entry(&Entitlement {
            spec: &spec,
            tier: Some(TierName::new("frontier")),
            honoured: true,
        });
        let virtual_auto = virtual_entry(None);
        let virtual_rung = virtual_entry(Some(&TierName::new("cheap")));

        assert_eq!(keys_of(&concrete), keys_of(&virtual_auto));
        assert_eq!(keys_of(&concrete), keys_of(&virtual_rung));
        assert_eq!(keys_of(&concrete["oag"]), keys_of(&virtual_auto["oag"]));

        // Both dialects' required fields, on the same object, because the
        // caller's SDK is not knowable from the request.
        for field in ["id", "object", "created", "owned_by"] {
            assert!(
                concrete.get(field).is_some(),
                "missing OpenAI field {field}"
            );
        }
        for field in ["type", "display_name", "created_at"] {
            assert!(
                concrete.get(field).is_some(),
                "missing Anthropic field {field}"
            );
        }
    }

    #[test]
    fn virtual_names_are_prefixed_and_auto_is_the_unpinned_one() {
        assert_eq!(virtual_entry(None)["id"], "oag/auto");
        assert_eq!(virtual_entry(None)["oag"]["tier"], Value::Null);
        assert_eq!(
            virtual_entry(Some(&TierName::new("cheap")))["id"],
            "oag/cheap"
        );
        assert_eq!(
            virtual_entry(Some(&TierName::new("cheap")))["oag"]["tier"],
            "cheap"
        );
    }

    #[test]
    fn no_entry_carries_pricing() {
        // Cost data belongs on the admin listener, which is where it already
        // lives. A client asking what it may call does not need the rate card.
        let spec = spec();
        let rendered = concrete_entry(&Entitlement {
            spec: &spec,
            tier: None,
            honoured: true,
        })
        .to_string();
        assert!(!rendered.contains("per_mtok"), "{rendered}");
        assert!(!rendered.contains("pricing"), "{rendered}");
    }

    #[test]
    fn an_empty_list_renders_without_indexing_into_it() {
        let body = envelope(Vec::new(), &RoutingMode::Passthrough);
        assert_eq!(body.0["data"], json!([]));
        assert_eq!(body.0["first_id"], Value::Null);
        assert_eq!(body.0["last_id"], Value::Null);
        assert_eq!(body.0["has_more"], false);
    }

    #[test]
    fn the_gemini_envelope_is_not_the_openai_one() {
        // Gemini clients read `models`; they do not know `object`/`data`.
        let spec = spec();
        let entry = gemini_entry(&Entitlement {
            spec: &spec,
            tier: None,
            honoured: true,
        });
        assert_eq!(entry["name"], "models/anthropic/claude-opus-5");
        assert_eq!(entry["inputTokenLimit"], 200_000);
        assert_eq!(entry["outputTokenLimit"], 64_000);

        // The name has to survive the generate route, which splits on the final
        // colon rather than on a slash — so a slashed id round-trips.
        let path = "anthropic/claude-opus-5:generateContent";
        let (model, action) = path.rsplit_once(':').expect("has an action");
        assert_eq!(model, "anthropic/claude-opus-5");
        assert_eq!(action, "generateContent");
    }
}
