//! Planning a request: which policy, budget, ladder and models it gets.

use super::respond::{Blocked, Laddered, no_viable_message};
use crate::AppState;
use axum::http::{HeaderMap, header};
use oag_core::tier::RoutingMode;
use oag_core::{Error, Result, TierName};
use oag_router::{BudgetState, Budgets, RoutingDecision, RoutingPolicy, TierLadder};
use std::collections::HashSet;
use std::sync::Arc;

/// Everything routing decided, before a single byte goes upstream.
pub(super) struct Plan {
    pub(super) policy: RoutingPolicy,
    pub(super) decision: RoutingDecision,
    pub(super) signal: oag_router::RequestSignal,
    pub(super) catalog: Arc<oag_router::Catalog>,
    pub(super) pressure: oag_router::BudgetPressure,
    /// The credential kind the request pinned, from the `@api` / `@sub`
    /// qualifier on the model name.
    ///
    /// Decided at normalisation rather than by routing, and carried here
    /// anyway: its only consumer is credential selection, two calls further
    /// down, and a plan is what already makes that journey. Threading a tenth
    /// argument through the same two frames would be the same coupling written
    /// less visibly.
    pub(super) channel: Option<oag_core::credential::CredentialKind>,
    /// What the route's credentials serve, for the savings baseline. Read
    /// once for `decide` and carried so `escalate` prices its row against the
    /// same baseline rather than the ladder's top rung.
    pub(super) served: HashSet<String>,
}

/// Load the caller's route and build the policy it implies.
///
/// Split out of `plan_request` because `/v1/models` needs the route and the
/// policy, and [`budgets_for`] the spend state — none of the rate token or the
/// model decision.
pub(crate) async fn policy_for(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
) -> Result<(oag_store::RouteRow, RoutingPolicy, oag_store::Spend)> {
    let route = oag_store::repo::route_by_id(&state.db, auth.route_id)
        .await?
        .ok_or_else(|| Error::Internal("route vanished between auth and routing".to_owned()))?;
    // Fresh, per request, beside the route: the one read that must never be
    // behind the ledger. See `Spend` for why it is not on `auth`.
    let spend = oag_store::repo::spend_for(&state.db, auth.api_key_id, auth.principal_id).await?;

    let ladder = parse_ladder(&route.tiers)?;

    // A key's floor beats the route's: it is the narrower grant, and the point
    // of pinning one key to `frontier` is that it applies to that key alone.
    let named = auth
        .key_floor_tier
        .as_deref()
        .or(route.floor_tier.as_deref())
        .map(TierName::from);
    let floor = named.as_ref().and_then(|n| ladder.tier(n));
    if let Some(name) = &named
        && floor.is_none()
    {
        // Written by the CLI or by psql, neither of which validates against the
        // ladder. Silently ignoring it means a key pinned to `frontier` quietly
        // serving from `cheap`, with nothing anywhere saying why.
        tracing::warn!(
            route = %route.name,
            floor = %name.as_str(),
            "floor tier names no rung in this ladder; ignoring it"
        );
    }

    let policy = RoutingPolicy::new(ladder, Box::new(oag_router::HeuristicClassifier::default()))
        .with_floor(floor);
    Ok((route, policy, spend))
}

/// The same spend caps inference consults, so `/v1/models` can refuse to
/// advertise what the next turn would hard-stop.
///
/// A per-key quota is a wall at the number written on it: it still degrades
/// through the constrained band first, but it does not get the principal's
/// overshoot grace. An operator who writes `quota_usd = 50` means fifty. A
/// route budget is the same shape at team scope.
///
/// Limits come from `auth`, which is cached; spend comes from `spend`, which
/// is read per request. Keeping the two apart is the whole fix: a cap checked
/// against a cached spend figure was a cap N concurrent requests all passed.
pub(crate) fn budgets_for(
    auth: &oag_store::AuthContext,
    route: &oag_store::RouteRow,
    spend: &oag_store::Spend,
) -> Budgets {
    Budgets {
        key: BudgetState {
            spent_usd: spend.key_usd,
            limit_usd: auth.quota_usd,
            hard_stop_multiple: rust_decimal::Decimal::ONE,
        },
        route: BudgetState {
            spent_usd: route.spent_usd,
            limit_usd: route.monthly_budget_usd,
            hard_stop_multiple: rust_decimal::Decimal::ONE,
        },
        principal: BudgetState {
            spent_usd: spend.principal_usd,
            limit_usd: auth.principal_budget_usd,
            hard_stop_multiple: auth.principal_hard_stop_multiple,
        },
    }
}

/// The rung an `oag/...` model name pins, if any. `oag/auto` pins nothing.
pub(crate) fn virtual_tier(model: &str) -> Option<TierName> {
    model
        .strip_prefix("oag/")
        .filter(|s| *s != "auto")
        .map(TierName::from)
}

/// Every upstream model name this route's credentials say they serve.
///
/// Empty means nothing has been discovered yet, and the caller must read that
/// as "unknown" rather than "nothing" — the same distinction
/// `account.served_models` draws between NULL and an empty array. A failure
/// here degrades the savings baseline and must never fail the request: the
/// answer has been paid for either way.
async fn served_models_for(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
) -> HashSet<String> {
    match oag_store::repo::route_channels(&state.db, auth.route_id, auth.principal_id).await {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|(_, _, served)| served)
            .flatten()
            .collect(),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "served models unavailable; savings baseline falls back to the ladder"
            );
            HashSet::new()
        }
    }
}

/// Resolve the route, build its policy, and choose a model.
///
/// Separated from `handle` because it is the part with no I/O side effects
/// beyond two reads — which makes it the part worth reasoning about on its own.
pub(super) async fn plan_request(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    canonical: &oag_proto::CanonicalRequest,
    headers: &HeaderMap,
    catalog: Arc<oag_router::Catalog>,
    channel: Option<oag_core::credential::CredentialKind>,
) -> Result<Plan> {
    let (route, policy, spend) = policy_for(state, auth).await?;

    // Throttle before doing any of the expensive work below — classification,
    // model selection, credential selection. A request that is going to be
    // refused should be refused cheaply.
    if let Some(rpm) = route.rpm_limit
        && let Ok(rpm) = u32::try_from(rpm)
        && let Some(retry_after) = state.cache.take_rate_token(route.id, rpm).await?
    {
        return Err(Error::RateLimited { retry_after });
    }

    let mut signal = canonical.signal();

    // `x-oag-tier` outranks the body's model name: the header is what a caller
    // adds deliberately, often when the body is generated by a tool they do not
    // control. Both resolve through the ladder, and an unrecognised rung stays
    // `None` on purpose — `decide` maps an unknown tier to `ladder.floor()`, so
    // a typo that reached it would silently pin the *cheapest* rung.
    let header_tier = headers
        .get("x-oag-tier")
        .and_then(|v| v.to_str().ok())
        .map(TierName::from);
    if let Some(name) = &header_tier
        && policy.rung(name).is_none()
    {
        tracing::warn!(
            tier = %name.as_str(),
            route = %route.name,
            "x-oag-tier names no rung in this ladder; ignoring it"
        );
    }
    // The header is filtered for validity BEFORE the body is consulted.
    //
    // `.map(...).or_else(...)` never reached the body once the header was
    // present, valid or not — so a typo in `x-oag-tier` silently discarded an
    // `oag/frontier` in the request body and the request fell back to
    // classification. Two pins, one of them correct, and the wrong one won by
    // being in the wrong place.
    //
    // The header still outranks the body when it names a real rung: it is what
    // a caller adds deliberately, often because the body is generated by a tool
    // they do not control.
    let requested_tier = header_tier
        .filter(|n| policy.rung(n).is_some())
        .or_else(|| virtual_tier(&canonical.model));
    if let Some(name) = &requested_tier
        && policy.rung(name).is_none()
    {
        tracing::warn!(
            tier = %name.as_str(),
            route = %route.name,
            "requested tier names no rung in this ladder; falling back to classification"
        );
    }
    signal.explicit_tier = requested_tier.filter(|n| policy.rung(n).is_some());

    // An explicit tier is only ever consulted by the classifier, and `decide`
    // only classifies outside its passthrough branch. Without the third arm,
    // `x-oag-tier` and `oag/<rung>` are both no-ops on a stock route, whose
    // `default_mode` is `passthrough`.
    let mode = if canonical.model.starts_with("oag/")
        || route.default_mode == "managed"
        || signal.explicit_tier.is_some()
    {
        RoutingMode::Managed
    } else {
        RoutingMode::Passthrough
    };

    let budget = budgets_for(auth, &route, &spend);

    // Logged at debug because "why did this route the way it did" is the
    // question every routing complaint turns into, and reconstructing it from
    // the ledger afterwards is slower than reading one line.
    tracing::debug!(
        mode = ?mode,
        pressure = ?budget.pressure(),
        binding = %budget.binding(),
        key_spent = %budget.key.spent_usd,
        key_quota = ?budget.key.limit_usd,
        route_spent = %budget.route.spent_usd,
        route_budget = ?budget.route.limit_usd,
        spent = %budget.principal.spent_usd,
        limit = ?budget.principal.limit_usd,
        floor = ?policy.floor_name(),
        "budget and mode"
    );

    // The savings baseline. Taken from what this route's credentials actually
    // serve rather than from the ladder's top rung: see `RoutingPolicy::decide`
    // for why the ladder cannot supply it. Empty means nothing has been asked
    // yet, and the ladder's ceiling stands — the pre-discovery behaviour.
    let served = served_models_for(state, auth).await;
    let decision = match policy.decide(
        &mode,
        Some(&canonical.model),
        &signal,
        &budget,
        &catalog,
        canonical.max_tokens,
        &served,
    ) {
        Ok(d) => d,
        Err(Error::NoViableModel(_)) => {
            let laddered = explain(state, auth, &canonical.model, policy.ladder(), &catalog).await;
            return Err(Error::NoViableModel(no_viable_message(
                &route.name,
                &canonical.model,
                policy.ladder(),
                laddered.as_ref(),
            )));
        }
        Err(e) => return Err(e),
    };

    Ok(Plan {
        policy,
        decision,
        signal,
        catalog,
        pressure: budget.pressure(),
        channel,
        served,
    })
}

/// The requested model, when the route's ladder names it, as routing alone can
/// see it: whether its provider is served, and whether the catalog holds it.
///
/// Named by the id the request used or, where the catalog knows the name, by
/// the catalog's id for it. `None` for a model the ladder does not name, which
/// is the ladder's to explain, and for one whose id names no provider.
pub(super) fn laddered(
    requested: &str,
    ladder: &TierLadder,
    catalog: &oag_router::Catalog,
) -> Option<Laddered> {
    let requested = requested.trim();
    let names = |id: &str| {
        ladder
            .rungs()
            .iter()
            .any(|rung| rung.models.iter().any(|m| m.as_str() == id))
    };
    let spec = catalog.resolve(requested);
    if !names(requested) && !spec.is_some_and(|spec| names(spec.id.as_str())) {
        return None;
    }
    if spec.is_some() {
        return Some(Laddered::Catalogued);
    }
    let (prefix, _) = requested.split_once('/')?;
    Some(match prefix.parse::<oag_core::Provider>() {
        // By its own name, which is what a credential is filed under, rather
        // than the alias the ladder may have spelled it with.
        Ok(p) if p.native_dialect().is_chat() => Laddered::Uncatalogued {
            provider: p.as_str().to_owned(),
            blocked: None,
        },
        _ => Laddered::Unserved {
            provider: prefix.to_owned(),
        },
    })
}

/// [`laddered`], and what the route's credentials for the model's provider
/// are to this caller when the catalog does not hold the model.
///
/// The one read a refusal adds, made only for a request that has already
/// failed: nothing on the way to a served answer pays for it. A read that
/// fails leaves the credentials unsaid, and the refusal says what routing
/// alone can.
async fn explain(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    requested: &str,
    ladder: &TierLadder,
    catalog: &oag_router::Catalog,
) -> Option<Laddered> {
    let mut found = laddered(requested, ladder, catalog)?;
    if let Laddered::Uncatalogued { provider, blocked } = &mut found {
        match oag_store::repo::provider_standing(
            &state.db,
            auth.route_id,
            provider,
            auth.principal_id,
        )
        .await
        {
            Ok(standing) => {
                // The counts go to the log, for the operator: the refusal says
                // what is in the way, and this says how much of the pool is.
                tracing::info!(
                    %provider,
                    principal_id = %auth.principal_id,
                    route_id = %auth.route_id,
                    ?standing,
                    "a model on the ladder could not be routed"
                );
                *blocked = Blocked::of(&standing);
            }
            Err(e) => tracing::warn!(
                error = %e,
                "could not read the route's credentials to explain a refusal"
            ),
        }
    }
    Some(found)
}

/// The inbound key, from any header a client might use.
///
/// Three spellings because three ecosystems: `Authorization` for OpenAI-shaped
/// clients, `x-api-key` for Anthropic's, `x-goog-api-key` for Gemini's. A
/// gateway that accepts only one makes the others' SDKs unusable.
pub(crate) fn extract_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(token) = v.strip_prefix("Bearer ")
    {
        return Some(token);
    }
    headers
        .get("x-api-key")
        .or_else(|| headers.get("x-goog-api-key"))
        .and_then(|v| v.to_str().ok())
}

pub(super) fn parse_ladder(tiers: &serde_json::Value) -> Result<TierLadder> {
    let rungs: Vec<oag_router::ladder::Rung> = serde_json::from_value(tiers.clone())
        .map_err(|e| Error::Config(format!("route.tiers is not a ladder: {e}")))?;
    TierLadder::new(rungs)
        .ok_or_else(|| Error::Config("route.tiers is empty; a route must have a rung".to_owned()))
}
