//! Model discovery: what this caller may actually ask for.
//!
//! Clients call `/v1/models` on startup to populate a picker and to validate a
//! configured model name. Without it they either fail closed or fall back to a
//! hardcoded list that has nothing to do with this deployment.
//!
//! The answer is per-caller, not a catalog dump. It is the intersection of the
//! route's ladder, the key's floor, and the providers this route actually holds
//! *live* credentials for — because a model listed but unreachable turns into a
//! failure much later and much further from its cause. Spent subscription
//! allowances, reserved-out seats, and an exhausted caller quota (key, route,
//! or principal) all empty the list, including the `oag/*` names: advertising
//! auto for a key that cannot spend is a 402 on the first turn. The `oag`
//! envelope still names the cause — `budget.pressure` and `providers[]` — so
//! an empty picker is not indistinguishable from "you are out of money" and
//! "your seat resets Thursday". That also makes this an entitlement surface,
//! which is why it authenticates.
//!
//! One client reads it through a filter rather than as-is: see [`super::alias`]
//! for why `gateway.claude_code_model_aliases` exists and why it adds a second
//! copy of each entry instead of renaming the first.

use super::{Caller, alias, budgets_for, error_response, policy_for};
use crate::AppState;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use oag_core::credential::CredentialKind;
use oag_core::{Provider, TierName, tier::RoutingMode};
use oag_router::{BudgetPressure, Entitlement};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use time::OffsetDateTime;

/// Which credential kinds a route can reach each provider through, and what
/// each of those will actually accept.
///
/// The listing offers `<model>@sub` only where a subscription is actually
/// reachable. Advertising a channel nobody holds a credential for is the same
/// mistake as listing a model with no provider behind it: the failure moves
/// away from its cause and turns up as a 503 much later.
///
/// Keyed by kind rather than collapsed to a provider because the served set is
/// a property of the CREDENTIAL, not of the provider: a Codex seat on a free
/// `ChatGPT` plan and an ordinary OpenAI API key are both `Provider::OpenAI` and
/// accept different models, so one answer for the provider is wrong for one of
/// them. `None` is "never asked", which is emphatically not "serves nothing".
type Channels = BTreeMap<Provider, HashMap<CredentialKind, Option<HashSet<String>>>>;

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
        channels,
        mode,
        pressure,
    } = match resolve(&state, &auth).await {
        Ok(r) => r,
        Err(response) => return response,
    };

    let aliases = wants_aliases(
        query.claude_code.as_deref(),
        state.config.gateway.claude_code_model_aliases,
    );

    let providers: BTreeSet<Provider> = channels.keys().copied().collect();
    let catalog = state.catalog().await;
    let concrete = offered(
        policy.entitled(&mode, &catalog, &providers, pressure),
        &channels,
    );

    // Virtual names first when anything can serve: they are the cost-routing
    // entry point, and a client rendering a picker shows the top of the list.
    // Omitted when nothing can: `oag/auto` on an empty picker is a 503.
    // Constrained spend hides oag/<rung> pins decide() would then abandon.
    let mut data: Vec<Value> = Vec::new();
    if advertise(pressure, concrete.len()) {
        for rung in virtual_rungs(&policy, pressure, state.config.gateway.advertise_auto) {
            push(&mut data, virtual_entry(rung.as_ref()), aliases);
        }
        for e in &concrete {
            for entry in concrete_entries(e, &channels) {
                push(&mut data, entry, aliases);
            }
        }
    }

    // Always, including when `data` is empty: that is the case a status panel
    // cannot guess from the picker. Same principal/route scope as the channels
    // query, without the serving filters, so a reserved or rate-limited seat
    // still appears. Failure here refuses the listing rather than returning a
    // picker with no explanation — the explanation is the new contract.
    let status =
        match oag_store::repo::route_channel_status(&state.db, auth.route_id, auth.principal_id)
            .await
        {
            Ok(rows) => rows,
            Err(e) => return error_response(&e),
        };
    let presence: Vec<Value> = super::presence::diagnose(
        &status,
        pressure,
        &model_counts(&concrete),
        OffsetDateTime::now_utc(),
    )
    .iter()
    .map(super::presence::ProviderPresence::to_json)
    .collect();

    envelope(data, &mode, aliases, pressure, presence).into_response()
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
    // Which of the names is the real one. Without it a dashboard reading this
    // listing has no way to tell a duplicate from a second model, and would
    // double every count.
    //
    // An entry that already names one keeps it rather than being overwritten
    // with its own id: `anthropic/xai/grok-4.6@sub` is a third spelling of
    // `xai/grok-4.6`, and pointing it at `xai/grok-4.6@sub` would make the
    // dedupe take two hops for no gain — `oag.channel` already says which
    // credential kind it pins.
    let of = match entry["oag"]["alias_of"].as_str() {
        Some(canonical) => canonical.to_owned(),
        None => id.to_owned(),
    };
    twin["oag"]["alias_of"] = Value::String(of);
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
        channels,
        mode,
        pressure,
    } = match resolve(&state, &auth).await {
        Ok(r) => r,
        Err(response) => return response,
    };

    let providers: BTreeSet<Provider> = channels.keys().copied().collect();
    let catalog = state.catalog().await;
    let concrete = offered(
        policy.entitled(&mode, &catalog, &providers, pressure),
        &channels,
    );

    if !advertise(pressure, concrete.len()) {
        return axum::Json(json!({ "models": [] })).into_response();
    }

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

    let mut models: Vec<Value> =
        virtual_rungs(&policy, pressure, state.config.gateway.advertise_auto)
            .map(|rung| gemini_virtual_entry(rung.as_ref(), window, max_output))
            .collect();
    models.extend(concrete.iter().map(gemini_entry));
    axum::Json(json!({ "models": models })).into_response()
}

struct Resolved {
    policy: oag_router::RoutingPolicy,
    channels: Channels,
    mode: RoutingMode,
    pressure: BudgetPressure,
}

/// Who is asking, and what the route lets them reach.
async fn resolve(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
) -> Result<Resolved, Response> {
    let (route, policy) = policy_for(state, auth)
        .await
        .map_err(|e| error_response(&e))?;
    let mode = if route.default_mode == "managed" {
        RoutingMode::Managed
    } else {
        RoutingMode::Passthrough
    };
    let pressure = budgets_for(auth, &route).pressure();
    // An exhausted caller cannot spend: skip the credential scan, because
    // nothing it would return can be advertised.
    let channels = if pressure == BudgetPressure::Exhausted {
        Channels::new()
    } else {
        channels_for(state, route.id, auth.principal_id)
            .await
            .map_err(|e| error_response(&e))?
    };

    Ok(Resolved {
        policy,
        channels,
        mode,
        pressure,
    })
}

/// Whether this caller gets a picker.
///
/// Exhausted spend is a hard stop on inference. No reachable model is a 503
/// on `oag/auto`. Both belong as an empty list, not as virtual names a client
/// would then send.
fn advertise(pressure: BudgetPressure, concrete: usize) -> bool {
    pressure != BudgetPressure::Exhausted && concrete > 0
}

/// `oag/auto` when it is advertised, then one per advertised rung. `None` is
/// `auto`.
///
/// `auto` is behind a flag and the rungs are not, because only `auto` claims a
/// judgement the classifier makes from the payload's shape rather than from the
/// task; see `gateway.advertise_auto`. Hiding it does not unhook it — `decide`
/// still honours the name, so an existing pin keeps resolving.
fn virtual_rungs(
    policy: &oag_router::RoutingPolicy,
    pressure: BudgetPressure,
    auto: bool,
) -> impl Iterator<Item = Option<TierName>> {
    auto.then_some(None)
        .into_iter()
        .chain(policy.virtual_names(pressure).into_iter().map(Some))
}

/// Providers the route holds usable credentials for, and through which
/// credential kinds.
///
/// `account.provider` and `account.kind` are both free text with no CHECK
/// constraint, so a row nobody can parse must narrow the answer rather than 500
/// the whole listing. An unparseable kind drops that one channel and keeps the
/// provider: the model is still reachable, it just cannot be addressed by a
/// qualifier nobody can name.
async fn channels_for(
    state: &Arc<AppState>,
    route_id: uuid::Uuid,
    principal_id: uuid::Uuid,
) -> oag_core::Result<Channels> {
    let raw = oag_store::repo::route_channels(&state.db, route_id, principal_id).await?;
    let mut out = Channels::new();
    for (name, kind, served) in raw {
        let Ok(provider) = name.parse::<Provider>() else {
            tracing::warn!(
                provider = %name,
                "account.provider is not a known provider; not listing its models"
            );
            continue;
        };
        let Some(parsed) = CredentialKind::from_column(&kind) else {
            tracing::warn!(
                provider = %name,
                kind = %kind,
                "account.kind is not a known credential kind; not offering a qualified id for it"
            );
            continue;
        };
        let served: Option<HashSet<String>> = served.map(|m| m.into_iter().collect());
        match out.entry(provider).or_default().entry(parsed) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(served);
            }
            // Two credentials of the same kind at one provider merge. Both
            // known unions; either unknown makes the pair unknown, because a
            // credential we never asked might serve what the other refuses and
            // hiding a model on that guess is the bug being fixed.
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let unknown = match (slot.get_mut().as_mut(), served) {
                    (Some(acc), Some(more)) => {
                        acc.extend(more);
                        false
                    }
                    _ => true,
                };
                if unknown {
                    slot.insert(None);
                }
            }
        }
    }
    Ok(out)
}

/// One concrete model: its own id, then a qualified id per credential kind
/// the route can actually reach.
///
/// A qualifier is how a caller pins a subscription or an API key instead of
/// letting the router pick. That pin is worth advertising even when the route
/// holds only one kind — otherwise a lone Grok seat lists as `xai/grok-4.6`
/// with `channel: null`, and a picker cannot tell it is a subscription. A kind
/// nobody holds is still omitted: that id would 503, which is the failure this
/// listing exists to move earlier.
fn concrete_entries(e: &Entitlement, channels: &Channels) -> Vec<Value> {
    let mut out = vec![concrete_entry(e, None)];
    let Some(held) = channels.get(&e.spec.provider) else {
        return out;
    };
    out.extend(
        CredentialKind::QUALIFIED
            .iter()
            .copied()
            .filter(|k| kind_serves(held, *k, &e.spec.upstream_name))
            .map(|k| concrete_entry(e, Some(k))),
    );
    out
}

/// One concrete model, optionally pinned to a credential kind.
///
/// A superset of the OpenAI and Anthropic shapes, because both dialects are
/// served from the same base URL and the caller's SDK is not knowable from the
/// request — `extract_key` accepts all three header spellings from any client,
/// so auth headers carry no dialect signal.
fn concrete_entry(e: &Entitlement, channel: Option<CredentialKind>) -> Value {
    let canonical = e.spec.id.as_str();
    let id = channel
        .and_then(|k| alias::qualified_id(canonical, k))
        .unwrap_or_else(|| canonical.to_owned());
    // The operator's name for the model, or the derived one. The suffix says
    // which channel, because two rows reading `xAI: Grok 4.6` in a picker are
    // a coin toss.
    let display = match channel {
        Some(kind) => format!("{} · {}", e.spec.label(), kind.channel_label()),
        None => e.spec.label(),
    };
    let mut entry = entry(
        &id,
        e.spec.provider.as_str(),
        &display,
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
            "channel": channel.and_then(CredentialKind::qualifier),
        }),
    );
    if channel.is_some() {
        // Same model, different address. Said here rather than left to the
        // reader, so a dashboard counting this listing does not report two
        // models where the organisation has one.
        entry["oag"]["alias_of"] = Value::String(canonical.to_owned());
    }
    entry
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
        &oag_router::derive_label("OAG", name),
        json!({
            "tier": rung.map(TierName::as_str),
            "provider": "oag",
            "virtual": true,
            "honoured": true,
            "context_window": Value::Null,
            "max_output_tokens": Value::Null,
            "capabilities": Value::Null,
            // A virtual name pins no credential kind: choosing among them is
            // most of what it is for.
            "channel": Value::Null,
        }),
    )
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

/// Whether this entitlement belongs on the picker for the credentials we hold.
///
/// Ladder models always do. Off-ladder catalog names are an API menu: a
/// metered key can call whatever the provider sells, so dumping the catalog
/// is honest. A subscription seat is not an API key — listing every
/// historical `xai/grok-*` behind `grok-seat` is how two Grok models read as
/// forty-six. `decide` still honours a name someone typed; this only decides
/// what we advertise.
fn on_offer(e: &Entitlement<'_>, channels: &Channels) -> bool {
    let Some(held) = channels.get(&e.spec.provider) else {
        return false;
    };
    match serves(held, &e.spec.upstream_name) {
        // We asked the credentials and they answered. That is the whole
        // answer: a rung cannot make a refused model servable — a free Codex
        // plan refuses `gpt-5` however prominently a ladder names it — and a
        // model the seat serves is reachable whether or not a rung names it.
        Some(answer) => answer,
        // Never asked. The old rule, which is the best guess available: rung
        // membership, or a metered key that can call the whole catalogue.
        None => e.tier.is_some() || held.keys().any(|k| !k.flat_rate()),
    }
}

/// Whether any of a provider's credentials serves this model, or `None` when
/// at least one of them has never been asked.
///
/// Unknown beats a negative deliberately. A credential we have not
/// interrogated might serve the model, and hiding it on that guess is the
/// same class of mistake as advertising one that refuses — it just fails in
/// the other direction, where nobody can see it.
fn serves(held: &HashMap<CredentialKind, Option<HashSet<String>>>, upstream: &str) -> Option<bool> {
    let mut unknown = false;
    for set in held.values() {
        match set {
            Some(s) if s.contains(upstream) => return Some(true),
            Some(_) => {}
            None => unknown = true,
        }
    }
    (!unknown).then_some(false)
}

/// Whether this exact credential kind is held, and serves this model.
///
/// The `@sub` twin used to be emitted for any model whose provider had a
/// subscription anywhere on the route, with nothing checking the seat would
/// take it. That is how a free `ChatGPT` plan came to advertise `openai/gpt-5@sub`
/// and refuse it on the first turn.
fn kind_serves(
    held: &HashMap<CredentialKind, Option<HashSet<String>>>,
    kind: CredentialKind,
    upstream: &str,
) -> bool {
    match held.get(&kind) {
        Some(Some(served)) => served.contains(upstream),
        // Held, never asked: the old behaviour, which is to offer it.
        Some(None) => true,
        None => false,
    }
}

fn offered<'c>(concrete: Vec<Entitlement<'c>>, channels: &Channels) -> Vec<Entitlement<'c>> {
    concrete
        .into_iter()
        .filter(|e| on_offer(e, channels))
        .collect()
}

fn model_counts(concrete: &[Entitlement<'_>]) -> BTreeMap<Provider, usize> {
    let mut counts = BTreeMap::new();
    for e in concrete {
        *counts.entry(e.spec.provider).or_insert(0) += 1;
    }
    counts
}

fn envelope(
    data: Vec<Value>,
    mode: &RoutingMode,
    aliases: bool,
    pressure: BudgetPressure,
    providers: Vec<Value>,
) -> axum::Json<Value> {
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
            "budget": { "pressure": pressure.as_str() },
        },
    });
    body["data"] = Value::Array(data);
    // Present even when `data` is empty: that is the case that needed
    // explaining. Assigned rather than interpolated so the Vec is moved,
    // not re-serialized. Additive, so an SDK that never looks here still
    // parses the list it parsed before.
    body["oag"]["providers"] = Value::Array(providers);
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
    use rust_decimal::{Decimal, dec};

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
            display_label: None,
        }
    }

    /// A route holding every kind named, for one provider, none of them yet
    /// asked what they serve — the pre-discovery state.
    fn channels(provider: Provider, kinds: &[CredentialKind]) -> Channels {
        Channels::from([(provider, kinds.iter().copied().map(|k| (k, None)).collect())])
    }

    /// A route where one kind has been asked and answered.
    fn channels_serving(provider: Provider, kind: CredentialKind, upstream: &[&str]) -> Channels {
        let served: HashSet<String> = upstream.iter().map(|s| (*s).to_owned()).collect();
        Channels::from([(provider, HashMap::from([(kind, Some(served))]))])
    }

    fn entitled(spec: &ModelSpec) -> Entitlement<'_> {
        Entitlement {
            spec,
            tier: None,
            honoured: true,
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
        let concrete = concrete_entry(
            &Entitlement {
                spec: &spec,
                tier: Some(TierName::new("frontier")),
                honoured: true,
            },
            None,
        );
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

    fn two_rung_policy() -> oag_router::RoutingPolicy {
        let ladder = oag_router::TierLadder::new(vec![
            oag_router::ladder::Rung {
                name: TierName::new("cheap"),
                models: vec![ModelId::new("xai/grok-4.6")],
            },
            oag_router::ladder::Rung {
                name: TierName::new("frontier"),
                models: vec![ModelId::new("anthropic/claude-opus-5")],
            },
        ])
        .expect("non-empty");
        oag_router::RoutingPolicy::new(ladder, Box::new(oag_router::HeuristicClassifier::default()))
    }

    #[test]
    fn auto_is_advertised_only_behind_its_flag() {
        // The classifier picks a rung from the payload's shape, never from the
        // task, so `oag/auto` in a picker promises a judgement it does not
        // make. Hidden by default; the operator opts back in.
        let policy = two_rung_policy();

        let hidden: Vec<_> = virtual_rungs(&policy, BudgetPressure::Normal, false).collect();
        assert!(
            !hidden.contains(&None),
            "oag/auto advertised with the flag off: {hidden:?}"
        );

        let shown: Vec<_> = virtual_rungs(&policy, BudgetPressure::Normal, true).collect();
        assert_eq!(
            shown.first(),
            Some(&None),
            "auto leads the list when advertised, where a picker looks"
        );
    }

    #[test]
    fn the_rung_pins_are_advertised_whichever_way_the_auto_flag_points() {
        // `oag/cheap` is a tier the caller named, not a judgement we made, so
        // it claims nothing `auto` claims. Hiding auto must not take the rung
        // pins with it.
        let policy = two_rung_policy();
        let names = |auto: bool| -> Vec<String> {
            virtual_rungs(&policy, BudgetPressure::Normal, auto)
                .flatten()
                .map(|t| t.as_str().to_owned())
                .collect()
        };
        assert_eq!(names(false), ["cheap", "frontier"]);
        assert_eq!(names(true), names(false));
    }

    #[test]
    fn a_discovered_refusal_beats_ladder_membership() {
        // The bug this whole change exists for. A free ChatGPT seat refuses
        // `gpt-5` however prominently a rung names it, so a rung must not be
        // able to advertise a model the credential has told us it will not take.
        let spec = spec();
        let on_a_rung = Entitlement {
            spec: &spec,
            tier: Some(TierName::new("balanced")),
            honoured: true,
        };
        let seat_refuses =
            channels_serving(spec.provider, CredentialKind::OAuth, &["something-else"]);
        assert!(
            !on_offer(&on_a_rung, &seat_refuses),
            "a ladder rung advertised a model the credential refuses"
        );
    }

    #[test]
    fn a_discovered_model_is_offered_without_any_rung() {
        // The other half: once a credential has answered, the ladder stops
        // being what makes a model visible. An off-ladder model the seat
        // serves is reachable, and on a subscription-only route it used to be
        // dropped for want of a metered key.
        let spec = spec();
        let off_ladder = entitled(&spec);
        assert_eq!(off_ladder.tier, None);
        let seat_serves =
            channels_serving(spec.provider, CredentialKind::OAuth, &[&spec.upstream_name]);
        assert!(
            on_offer(&off_ladder, &seat_serves),
            "a model the seat serves was hidden because no rung named it"
        );
    }

    #[test]
    fn an_undiscovered_credential_keeps_the_old_behaviour() {
        // Never asked must not read as "serves nothing", or a gateway that has
        // not yet run a discovery sweep would advertise an empty picker.
        let spec = spec();
        let on_a_rung = Entitlement {
            spec: &spec,
            tier: Some(TierName::new("balanced")),
            honoured: true,
        };
        let unasked = channels(spec.provider, &[CredentialKind::OAuth]);
        assert!(on_offer(&on_a_rung, &unasked), "rung visibility regressed");
        assert!(
            !on_offer(&entitled(&spec), &unasked),
            "a flat-rate credential should not offer the whole catalogue"
        );
    }

    #[test]
    fn one_unasked_credential_makes_the_pair_unknown() {
        // Two seats of a kind, one discovered and one not. The undiscovered one
        // might serve what the other refuses, and hiding the model on that
        // guess fails in the direction nobody can see.
        let spec = spec();
        let mut mixed = channels_serving(spec.provider, CredentialKind::OAuth, &["other"]);
        mixed
            .get_mut(&spec.provider)
            .expect("provider")
            .insert(CredentialKind::ApiKey, None);
        assert_eq!(
            serves(
                mixed.get(&spec.provider).expect("provider"),
                &spec.upstream_name
            ),
            None,
            "an unasked credential must leave the answer unknown, not false"
        );
    }

    #[test]
    fn a_sub_twin_is_offered_only_where_the_seat_serves_the_model() {
        // `openai/gpt-5@sub` on a free Codex plan: the provider had a
        // subscription, nothing checked the seat would take that model, and the
        // caller found out on the first turn.
        let spec = spec();
        let refuses = channels_serving(spec.provider, CredentialKind::OAuth, &["something-else"]);
        let ids: Vec<String> = concrete_entries(&entitled(&spec), &refuses)
            .iter()
            .filter_map(|e| e["id"].as_str().map(String::from))
            .collect();
        assert!(
            !ids.iter().any(|id| id.ends_with("@sub")),
            "advertised a @sub twin the seat refuses: {ids:?}"
        );
    }

    #[test]
    fn no_entry_carries_pricing() {
        // Cost data belongs on the admin listener, which is where it already
        // lives. A client asking what it may call does not need the rate card.
        let spec = spec();
        let rendered = concrete_entry(&entitled(&spec), None).to_string();
        assert!(!rendered.contains("per_mtok"), "{rendered}");
        assert!(!rendered.contains("pricing"), "{rendered}");
    }

    #[test]
    fn a_subscription_seat_does_not_offer_the_api_catalog() {
        // grok-seat is OAuth. Dumping LiteLLM's 46 xAI ids behind it is how
        // two Grok models read as forty-six. An API key is the catalog menu.
        let grok = grok();
        let off_ladder = entitled(&grok);
        let on_ladder = Entitlement {
            spec: &grok,
            tier: Some(TierName::new("frontier")),
            honoured: true,
        };

        let sub = channels(Provider::XAI, &[CredentialKind::OAuth]);
        assert!(!on_offer(&off_ladder, &sub), "a seat is not an API catalog");
        assert!(
            on_offer(&on_ladder, &sub),
            "a ladder model the seat can reach still lists"
        );

        let api = channels(Provider::XAI, &[CredentialKind::ApiKey]);
        assert!(on_offer(&off_ladder, &api), "an API key offers the catalog");

        let both = channels(
            Provider::XAI,
            &[CredentialKind::ApiKey, CredentialKind::OAuth],
        );
        assert!(
            on_offer(&off_ladder, &both),
            "holding an API key as well as a seat still offers the catalog"
        );
    }

    #[test]
    fn an_empty_list_renders_without_indexing_into_it() {
        let body = envelope(
            Vec::new(),
            &RoutingMode::Passthrough,
            false,
            BudgetPressure::Normal,
            Vec::new(),
        );
        assert_eq!(body.0["data"], json!([]));
        assert_eq!(body.0["first_id"], Value::Null);
        assert_eq!(body.0["last_id"], Value::Null);
        assert_eq!(body.0["has_more"], false);
        assert_eq!(body.0["oag"]["budget"]["pressure"], "normal");
        assert_eq!(body.0["oag"]["providers"], json!([]));
    }

    #[test]
    fn an_empty_list_still_explains_why() {
        // Exhausted spend and spent seats both empty `data`. The envelope is
        // how a client tells "raise the quota" from "wait for the window".
        let spent = json!([{
            "provider": "xai",
            "serving": false,
            "reason": "quota_spent",
            "until": Value::Null,
            "remaining_pct": 0.0,
            "reserve_pct": Value::Null,
            "kinds": ["sub"],
            "models": 0,
        }]);
        let seats = envelope(
            Vec::new(),
            &RoutingMode::Passthrough,
            false,
            BudgetPressure::Normal,
            spent.as_array().cloned().unwrap(),
        );
        let money = envelope(
            Vec::new(),
            &RoutingMode::Passthrough,
            false,
            BudgetPressure::Exhausted,
            vec![json!({
                "provider": "xai",
                "serving": false,
                "reason": "budget_exhausted",
                "until": Value::Null,
                "remaining_pct": Value::Null,
                "reserve_pct": Value::Null,
                "kinds": ["sub"],
                "models": 0,
            })],
        );

        assert_eq!(seats.0["data"], json!([]));
        assert_eq!(money.0["data"], json!([]));
        assert_eq!(seats.0["oag"]["budget"]["pressure"], "normal");
        assert_eq!(money.0["oag"]["budget"]["pressure"], "exhausted");
        assert_eq!(seats.0["oag"]["providers"][0]["reason"], "quota_spent");
        assert_eq!(money.0["oag"]["providers"][0]["reason"], "budget_exhausted");
        assert_ne!(
            seats.0["oag"]["providers"][0]["reason"],
            money.0["oag"]["providers"][0]["reason"]
        );
        // Additive: a client that never learned `oag.providers` still sees
        // the fields it parsed before this existed.
        assert_eq!(seats.0["oag"]["mode"], "passthrough");
        assert_eq!(seats.0["oag"]["claude_code_aliases"], false);
    }

    fn key(quota: rust_decimal::Decimal, spent: rust_decimal::Decimal) -> oag_store::AuthContext {
        oag_store::AuthContext {
            api_key_id: uuid::Uuid::nil(),
            principal_id: uuid::Uuid::nil(),
            route_id: uuid::Uuid::nil(),
            key_floor_tier: None,
            admin: false,
            quota_usd: Some(quota),
            spent_usd: spent,
            principal_budget_usd: None,
            principal_hard_stop_multiple: rust_decimal::Decimal::ONE,
            principal_spent_usd: rust_decimal::Decimal::ZERO,
        }
    }

    fn uncapped_route() -> oag_store::RouteRow {
        oag_store::RouteRow {
            id: uuid::Uuid::nil(),
            name: "default".to_owned(),
            tiers: json!([]),
            default_mode: "passthrough".to_owned(),
            floor_tier: None,
            rpm_limit: None,
            monthly_budget_usd: None,
            spent_usd: rust_decimal::Decimal::ZERO,
            active: true,
        }
    }

    #[test]
    fn a_key_with_budget_left_still_advertises() {
        // $30 spent $0.50 is the case a picker must not empty over.
        let pressure = budgets_for(&key(dec!(30), dec!(0.5)), &uncapped_route()).pressure();
        assert_ne!(pressure, BudgetPressure::Exhausted);
        assert!(advertise(pressure, 1));
    }

    #[test]
    fn an_exhausted_key_advertises_nothing_not_even_virtual_names() {
        let pressure = budgets_for(&key(dec!(30), dec!(30)), &uncapped_route()).pressure();
        assert_eq!(pressure, BudgetPressure::Exhausted);
        assert!(
            !advertise(pressure, 3),
            "oag/auto on a hard-stopped key is a refused first turn"
        );
    }

    #[test]
    fn a_constrained_key_still_advertises() {
        // The last fifth degrades to cheap; it does not refuse. Emptying the
        // picker here would hide models the gateway would still serve.
        // Which *rungs* stay on the list is RoutingPolicy::virtual_names, not
        // this gate — Constrained is not Exhausted.
        let pressure = budgets_for(&key(dec!(30), dec!(25)), &uncapped_route()).pressure();
        assert_eq!(pressure, BudgetPressure::Constrained);
        assert!(advertise(pressure, 1));
        assert!(advertise(BudgetPressure::Normal, 1));
        assert!(!advertise(BudgetPressure::Exhausted, 3));
    }

    #[test]
    fn no_reachable_model_omits_virtual_names() {
        assert!(!advertise(BudgetPressure::Normal, 0));
    }

    #[test]
    fn an_exhausted_route_budget_empties_the_listing_too() {
        let mut route = uncapped_route();
        route.monthly_budget_usd = Some(dec!(10));
        route.spent_usd = dec!(10);
        let uncapped_key = oag_store::AuthContext {
            quota_usd: None,
            spent_usd: rust_decimal::Decimal::ZERO,
            ..key(dec!(1), dec!(0))
        };
        let pressure = budgets_for(&uncapped_key, &route).pressure();
        assert_eq!(pressure, BudgetPressure::Exhausted);
        assert!(!advertise(pressure, 1));
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
            push(&mut out, concrete_entry(&entitled(spec), None), aliases);
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

    fn grok() -> ModelSpec {
        ModelSpec {
            id: ModelId::new("xai/grok-4.6"),
            provider: Provider::XAI,
            upstream_name: "grok-4.6".to_owned(),
            ..spec()
        }
    }

    fn qualified_ids(spec: &ModelSpec, channels: &Channels) -> Vec<String> {
        concrete_entries(&entitled(spec), channels)
            .iter()
            .filter_map(|m| m["id"].as_str())
            .map(String::from)
            .collect()
    }

    #[test]
    fn a_qualified_id_is_offered_for_each_kind_the_route_can_reach() {
        // A lone seat must still say it is a subscription: otherwise the picker
        // only has `xai/grok-4.6` and cannot tell a seat from an API key. A
        // kind nobody holds is still omitted — that id would 503.
        let grok = grok();

        let both = channels(
            Provider::XAI,
            &[CredentialKind::ApiKey, CredentialKind::OAuth],
        );
        assert_eq!(
            qualified_ids(&grok, &both),
            ["xai/grok-4.6", "xai/grok-4.6@api", "xai/grok-4.6@sub"]
        );

        assert_eq!(
            qualified_ids(&grok, &channels(Provider::XAI, &[CredentialKind::OAuth])),
            ["xai/grok-4.6", "xai/grok-4.6@sub"],
            "a lone seat is advertised as @sub"
        );
        assert_eq!(
            qualified_ids(&grok, &channels(Provider::XAI, &[CredentialKind::ApiKey])),
            ["xai/grok-4.6", "xai/grok-4.6@api"],
            "a lone API key is advertised as @api"
        );

        assert_eq!(
            qualified_ids(&grok, &Channels::new()),
            ["xai/grok-4.6"],
            "a model with no channels recorded still lists once"
        );
    }

    #[test]
    fn a_qualified_id_the_listing_offers_is_one_the_inference_path_takes_back() {
        // Two halves of one feature in two modules. A listing that advertised
        // `xai/grok-4.6:sub` would populate a picker with ids that 400 on the
        // first turn.
        let grok = grok();
        let both = channels(
            Provider::XAI,
            &[CredentialKind::ApiKey, CredentialKind::OAuth],
        );
        let catalog = oag_router::Catalog::from_entries([grok.clone()]);

        for id in qualified_ids(&grok, &both) {
            let n = alias::normalise(&id, &catalog).expect("the listing's own id parses");
            assert_eq!(
                n.model.unwrap_or_else(|| id.clone()),
                "xai/grok-4.6",
                "{id} named another model"
            );
        }
    }

    #[test]
    fn a_qualified_entry_says_which_model_it_is_and_which_channel_it_pins() {
        // Otherwise a dashboard counting this listing reports three models
        // where the organisation has one, and a picker shows two rows with the
        // same name and no way to tell them apart.
        let grok = grok();
        let both = channels(
            Provider::XAI,
            &[CredentialKind::ApiKey, CredentialKind::OAuth],
        );
        let entries = concrete_entries(&entitled(&grok), &both);

        let canonical = &entries[0];
        assert_eq!(canonical["oag"]["alias_of"], Value::Null);
        assert_eq!(canonical["oag"]["channel"], Value::Null);
        assert_eq!(canonical["display_name"], "xAI: grok-4.6");

        let sub = entries
            .iter()
            .find(|m| m["id"] == "xai/grok-4.6@sub")
            .expect("subscription variant");
        assert_eq!(sub["oag"]["alias_of"], "xai/grok-4.6");
        assert_eq!(sub["oag"]["channel"], "sub");
        assert_eq!(sub["display_name"], "xAI: grok-4.6 · subscription");

        let api = entries
            .iter()
            .find(|m| m["id"] == "xai/grok-4.6@api")
            .expect("api variant");
        assert_eq!(api["display_name"], "xAI: grok-4.6 · API key");

        // Every variant carries the field set of the canonical row: an SDK
        // validates the whole array, not the first element.
        for e in &entries {
            assert_eq!(keys_of(e), keys_of(canonical));
            assert_eq!(keys_of(&e["oag"]), keys_of(&canonical["oag"]));
        }
    }

    #[test]
    fn a_qualified_id_and_the_claude_code_prefix_compose_into_one_reachable_name() {
        // Claude Code keeps only ids matching /^(claude|anthropic)/i, so
        // without a twin the pinned variants are simply absent from its picker
        // — and with a twin that named itself, a dashboard would need two hops
        // to work out which model it is.
        let grok = grok();
        let both = channels(
            Provider::XAI,
            &[CredentialKind::ApiKey, CredentialKind::OAuth],
        );
        let mut out = Vec::new();
        for entry in concrete_entries(&entitled(&grok), &both) {
            push(&mut out, entry, true);
        }
        let ids: Vec<&str> = out.iter().filter_map(|m| m["id"].as_str()).collect();
        assert!(ids.contains(&"anthropic/xai/grok-4.6@sub"), "{ids:?}");
        assert!(
            ids.iter()
                .all(|id| claude_code_keeps(id) || !id.starts_with("anthropic"))
        );

        let twin = out
            .iter()
            .find(|m| m["id"] == "anthropic/xai/grok-4.6@sub")
            .expect("twinned");
        assert_eq!(twin["oag"]["alias_of"], "xai/grok-4.6");
        assert_eq!(twin["oag"]["channel"], "sub");
    }

    #[test]
    fn an_operators_label_replaces_the_derived_one_wherever_the_model_appears() {
        // The point of having a label at all: renaming is free because the id
        // never moves. Every spelling of the model shows the new name and every
        // one of them still routes to the same id.
        let named = ModelSpec {
            display_label: Some("Grok, the fast one".to_owned()),
            ..grok()
        };
        let both = channels(
            Provider::XAI,
            &[CredentialKind::ApiKey, CredentialKind::OAuth],
        );
        let entries = concrete_entries(&entitled(&named), &both);

        assert_eq!(entries[0]["display_name"], "Grok, the fast one");
        assert_eq!(entries[0]["id"], "xai/grok-4.6");
        let sub = entries
            .iter()
            .find(|m| m["id"] == "xai/grok-4.6@sub")
            .expect("subscription variant");
        assert_eq!(sub["display_name"], "Grok, the fast one · subscription");
    }

    #[test]
    fn a_model_nobody_has_named_still_reads_as_something_in_a_picker() {
        // NULL is not "no name": it means derive one. `anthropic/xai/grok-4.6`
        // reads as an Anthropic model, and the display name is the only thing
        // in a Claude Code picker that says otherwise.
        assert_eq!(grok().label(), "xAI: grok-4.6");
        assert_eq!(spec().label(), "Anthropic: claude-opus-5");
    }
}
