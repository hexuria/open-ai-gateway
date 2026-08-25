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
//!
//! One client reads it through a filter rather than as-is: see [`super::alias`]
//! for why `gateway.claude_code_model_aliases` exists and why it adds a second
//! copy of each entry instead of renaming the first.

use super::{Caller, alias, error_response, policy_for};
use crate::AppState;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use oag_core::{Provider, TierName, tier::RoutingMode};
use oag_router::Entitlement;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Query parameters on the listing.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ListQuery {
    /// Force the Claude Code aliases on or off for this one call, overriding
    /// `gateway.claude_code_model_aliases`.
    ///
    /// Claude Code itself cannot send this — it appends `/v1/models` to a bare
    /// `ANTHROPIC_BASE_URL` — so it is not the feature switch. It exists so an
    /// operator can `curl` exactly what the CLI would cache before deciding to
    /// turn the config flag on for everyone.
    claude_code: Option<String>,
}

/// `GET /v1/models` and `/models`.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Caller(auth): Caller,
    Query(query): Query<ListQuery>,
) -> Response {
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
    let aliases = wants_aliases(
        query.claude_code.as_deref(),
        state.config.gateway.claude_code_model_aliases,
    );

    // Virtual names first: they are the cost-routing entry point, and a client
    // rendering a picker shows the top of the list.
    let mut data: Vec<Value> = Vec::new();
    for rung in virtual_rungs(&policy) {
        push(&mut data, virtual_entry(rung.as_ref()), aliases);
    }
    for e in &concrete {
        push(&mut data, concrete_entry(e), aliases);
    }
    envelope(data, &mode, aliases).into_response()
}

/// Whether to emit the aliased twins.
///
/// An unparseable value falls back to the configured default rather than 400ing:
/// this is the call a client makes on startup, and failing it takes the whole
/// session with it over a query string nobody looked at.
fn wants_aliases(raw: Option<&str>, configured: bool) -> bool {
    match raw {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => configured,
    }
}

/// Append an entry, followed by its discovery alias when one is wanted.
///
/// Interleaved rather than appended in a block, so both audiences see the same
/// order: an OpenAI client reads canonical-then-twin down the list, and Claude
/// Code — which keeps only the twins — still gets the virtual routing names
/// first, which is where a picker looks.
fn push(out: &mut Vec<Value>, entry: Value, aliases: bool) {
    if aliases && let Some(twin) = alias_twin(&entry) {
        out.push(entry);
        out.push(twin);
        return;
    }
    out.push(entry);
}

/// The same model under an id Claude Code will keep, or `None` when its
/// canonical id already passes the filter.
fn alias_twin(entry: &Value) -> Option<Value> {
    let id = entry["id"].as_str()?;
    let aliased = alias::discovery_alias(id)?;
    let mut twin = entry.clone();
    twin["id"] = Value::String(aliased);
    // Which of the two names is the real one. Without it a dashboard reading
    // this listing has no way to tell a duplicate from a second model, and
    // would double every count.
    twin["oag"]["alias_of"] = Value::String(id.to_owned());
    Some(twin)
}

/// `GET /v1beta/models`, which uses a different envelope and different field
/// names. Deliberately does not share a renderer with [`list`].
///
/// No discovery aliases here: they exist for one client that reads the
/// Anthropic-shaped listing, and adding them would be noise a Gemini SDK has to
/// filter. An aliased name sent *back* still routes, because normalisation is
/// on the inference path rather than in this listing.
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
        &label(
            e.spec.provider.support().display_name,
            &e.spec.upstream_name,
        ),
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
    let name = rung.map_or("auto", TierName::as_str);
    let id = format!("oag/{name}");
    entry(
        &id,
        "oag",
        &label("OAG", name),
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

/// What a human should read, as opposed to what the router needs.
///
/// The vendor's own spelling plus the name on the wire — derived, not invented:
/// a marketing name we made up would disagree with the provider's own console
/// the first time they rename something. It matters because an aliased id is
/// `anthropic/xai/grok-4.6`, which reads as an Anthropic model and is the only
/// thing a Claude Code picker has to go on otherwise.
fn label(vendor: &str, name: &str) -> String {
    format!("{vendor}: {name}")
}

fn entry(id: &str, owned_by: &str, display_name: &str, oag: Value) -> Value {
    let mut entry = json!({
        // OpenAI's shape.
        "id": id,
        "object": "model",
        "created": 0,
        "owned_by": owned_by,
        // Anthropic's shape, for the same object.
        "type": "model",
        "display_name": display_name,
        "created_at": "1970-01-01T00:00:00Z",
    });
    // Ours. No pricing: that is the organisation's cost data, and it stays on
    // the admin listener where it already lives.
    entry["oag"] = oag;
    // Set here rather than in each renderer so a canonical row and its twin
    // cannot disagree about the field's existence — the twin overwrites it.
    entry["oag"]["alias_of"] = Value::Null;
    entry
}

fn envelope(data: Vec<Value>, mode: &RoutingMode, aliases: bool) -> axum::Json<Value> {
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
            // So an operator setting this up can see the flag took effect
            // without diffing two listings.
            "claude_code_aliases": aliases,
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
        let body = envelope(Vec::new(), &RoutingMode::Passthrough, false);
        assert_eq!(body.0["data"], json!([]));
        assert_eq!(body.0["first_id"], Value::Null);
        assert_eq!(body.0["last_id"], Value::Null);
        assert_eq!(body.0["has_more"], false);
    }

    /// The filter as Claude Code spells it, written independently of
    /// [`alias::survives_discovery_filter`] so the two cannot drift together.
    fn claude_code_keeps(id: &str) -> bool {
        let lower = id.to_lowercase();
        lower.starts_with("claude") || lower.starts_with("anthropic")
    }

    fn listed(aliases: bool) -> Vec<Value> {
        let spec = spec();
        let grok = ModelSpec {
            id: ModelId::new("xai/grok-4.6"),
            provider: Provider::XAI,
            upstream_name: "grok-4.6".to_owned(),
            ..spec.clone()
        };
        let mut out = Vec::new();
        push(&mut out, virtual_entry(None), aliases);
        push(
            &mut out,
            virtual_entry(Some(&TierName::new("cheap"))),
            aliases,
        );
        for spec in [&spec, &grok] {
            push(
                &mut out,
                concrete_entry(&Entitlement {
                    spec,
                    tier: None,
                    honoured: true,
                }),
                aliases,
            );
        }
        out
    }

    fn ids(aliases: bool) -> Vec<String> {
        listed(aliases)
            .iter()
            .filter_map(|m| m["id"].as_str())
            .map(String::from)
            .collect()
    }

    #[test]
    fn with_aliases_on_every_model_is_reachable_through_claude_codes_filter() {
        // The whole feature. Claude Code drops every id the regex misses, so a
        // model with no surviving spelling is simply absent from the picker.
        let kept: BTreeSet<String> = ids(true)
            .into_iter()
            .filter(|id| claude_code_keeps(id))
            .collect();

        for canonical in ["oag/auto", "oag/cheap", "xai/grok-4.6"] {
            let aliased = format!("anthropic/{canonical}");
            assert!(kept.contains(&aliased), "{canonical} unreachable");
        }
        // Already passes the filter on its own name, so it needs no twin.
        assert!(kept.contains("anthropic/claude-opus-5"));
    }

    #[test]
    fn an_id_that_already_passes_the_filter_gets_no_twin() {
        // A twin would be `anthropic/anthropic/claude-opus-5`, which resolves
        // to nothing and which the picker would show next to the real one.
        let ids = ids(true);
        assert_eq!(
            ids.iter().filter(|id| id.contains("claude-opus-5")).count(),
            1,
            "{ids:?}"
        );
    }

    #[test]
    fn an_alias_says_which_id_is_the_canonical_one() {
        // Otherwise a dashboard counting this listing counts every aliased
        // model twice and cannot tell which name to report spend under.
        let data = listed(true);
        let twin = data
            .iter()
            .find(|m| m["id"] == "anthropic/xai/grok-4.6")
            .expect("aliased");
        assert_eq!(twin["oag"]["alias_of"], "xai/grok-4.6");

        let canonical = data
            .iter()
            .find(|m| m["id"] == "xai/grok-4.6")
            .expect("canonical");
        assert_eq!(canonical["oag"]["alias_of"], Value::Null);
    }

    #[test]
    fn an_existing_consumer_sees_the_canonical_ids_whether_or_not_aliasing_is_on() {
        // The aliases are additive. An OpenAI SDK reading this endpoint before
        // the flag existed must find exactly what it found then.
        let plain = ids(false);
        assert_eq!(
            plain,
            [
                "oag/auto",
                "oag/cheap",
                "anthropic/claude-opus-5",
                "xai/grok-4.6"
            ]
        );

        let aliased = ids(true);
        for id in &plain {
            assert!(
                aliased.contains(id),
                "{id} vanished when aliasing turned on"
            );
        }
    }

    #[test]
    fn a_display_name_names_the_real_model_not_the_aliased_id() {
        // `anthropic/xai/grok-4.6` reads as an Anthropic model. The display
        // name is the only thing that says otherwise in a picker.
        let twin = listed(true)
            .into_iter()
            .find(|m| m["id"] == "anthropic/xai/grok-4.6")
            .expect("aliased");
        assert_eq!(twin["display_name"], "xAI: grok-4.6");
        assert_eq!(virtual_entry(None)["display_name"], "OAG: auto");
    }

    #[test]
    fn the_query_parameter_overrides_the_configured_default_in_both_directions() {
        // An operator has to be able to see what the CLI would cache without
        // turning the flag on for every other client first.
        assert!(wants_aliases(Some("1"), false));
        assert!(!wants_aliases(Some("0"), true));
        assert!(wants_aliases(None, true));
        assert!(!wants_aliases(None, false));
        // A value nobody parsed is not worth failing a startup call over.
        assert!(wants_aliases(Some("maybe"), true));
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
