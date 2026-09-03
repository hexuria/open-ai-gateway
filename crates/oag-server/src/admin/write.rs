//! The admin mutations: four incident verbs, and the identity-integration set.
//!
//! **The four incident verbs.** This project dropped sub2api's admin sprawl — a
//! 2466-line settings handler over a generic key/value table — and the way back
//! to that is one reasonable-looking endpoint at a time. Each of these answers a
//! question an operator has *during an incident*, when reaching for psql is
//! slower and the CLI may not be to hand:
//!
//! - a credential is misbehaving          → disable
//! - it has recovered                     → enable
//! - it was cooled down and is fine now   → clear the cooldown
//! - a key leaked                         → revoke
//!
//! Anything an operator can do calmly, at a prompt, with a schema in front of
//! them, stays in the CLI. Nothing here touches sealed credentials, the KEK, or
//! any signing secret: those are not browser-reachable at any authority level.
//! Those four take no request body — each is a verb against an id.
//!
//! **The identity-integration set** (principals, keys, budgets) is the second
//! group, and it is a deliberate exception to "calm work stays in the CLI" — not
//! the first step back to sprawl. A partner service (`OpenGrok`) binds
//! each of its orgs to a principal and each of its members to a key on that
//! principal, so that an org admin can hand a teammate a working credential from
//! a web console. That flow is a *program* driving us, not an operator at a
//! prompt, and a program cannot shell out to the CLI. It stays inside the same
//! rule that governs the four above: minting is an OS RNG, a SHA-256 and an
//! INSERT — it reads no sealed credential, no KEK, and no signing secret. A key
//! minted here is never `admin`, so this surface cannot widen its own authority,
//! and an upsert never rewrites an existing principal's role, so it cannot remove
//! anyone else's either.
//!
//! BE CLEAR ABOUT WHAT IS *NOT* BOUNDED. `require_admin_layer` is all-or-nothing:
//! any key with `admin = true` reaches every route under `/admin/api`, these
//! included. So the admin key handed to a partner service is a FULL admin
//! credential for this gateway, not a scoped one — "bounded" describes this set of
//! endpoints, never the authority of the key that calls them. Per-key admin scopes
//! are the honest fix and do not exist yet; until they do, that key should be
//! treated as an operator credential and rotated like one.

use super::auth::AdminActor;
use super::{failed, invalid, not_found};
use crate::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use oag_core::AccountId;
use rust_decimal::Decimal;
use serde_json::json;
use std::sync::Arc;

pub async fn disable_account(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    match oag_store::repo::set_schedulable(&state.db, AccountId::from_uuid(id), false).await {
        Ok(Some(name)) => {
            audit(&actor, "account.disable", id, &name);
            Json(json!({
                "id": id,
                "name": name,
                "schedulable": false,
                "note": "requests already dispatched to this credential run to completion; \
                         selection is consulted at dispatch time only",
            }))
            .into_response()
        }
        Ok(None) => not_found("no credential with that id"),
        Err(e) => failed(&e),
    }
}

pub async fn enable_account(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    match oag_store::repo::set_schedulable(&state.db, AccountId::from_uuid(id), true).await {
        Ok(Some(name)) => {
            audit(&actor, "account.enable", id, &name);
            Json(json!({ "id": id, "name": name, "schedulable": true })).into_response()
        }
        Ok(None) => not_found("no credential with that id"),
        Err(e) => failed(&e),
    }
}

pub async fn clear_cooldown(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let account = AccountId::from_uuid(id);
    match oag_store::repo::clear_cooldown(&state.db, account).await {
        Ok(Some(name)) => {
            state.breakers.clear(account);
            audit(&actor, "account.clear-cooldown", id, &name);
            Json(json!({
                "id": id,
                "name": name,
                "cleared": true,
                "note": "the database cooldown is fleet-wide; the in-memory breaker was reset on \
                         this replica only, and others heal on their own probe",
            }))
            .into_response()
        }
        Ok(None) => not_found("no credential with that id"),
        Err(e) => failed(&e),
    }
}

pub async fn revoke_key(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    match oag_store::repo::revoke_key(&state.db, id).await {
        Ok(Some((key_hash, name, prefix))) => {
            // The hash is what the auth cache is keyed by, and the plaintext is
            // not recoverable — this is the only handle a revocation has.
            state.auth.invalidate_hash(&key_hash).await;
            audit(&actor, "key.revoke", id, &name);
            Json(json!({
                "id": id,
                "name": name,
                "key_prefix": prefix,
                "active": false,
                "note": "this replica stops honouring it now; others within their L1 TTL",
            }))
            .into_response()
        }
        Ok(None) => not_found("no key with that id"),
        Err(e) => failed(&e),
    }
}

/// Bind an org (or any external subject) to a principal. Idempotent on email.
pub async fn upsert_principal(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Json(body): Json<PrincipalInput>,
) -> Response {
    // 'member' unless explicitly asked otherwise, and 'admin' is refused: admin
    // authority is the CLI's to grant, or this endpoint could promote itself.
    let role = body.role.as_deref().unwrap_or("member");
    if role != "member" {
        return invalid("role must be 'member'; admin principals are minted at the CLI");
    }
    let budget = match parse_money(body.monthly_budget_usd.as_deref()) {
        Ok(budget) => budget,
        Err(message) => return invalid(&message),
    };
    match oag_store::repo::upsert_principal(&state.db, &body.email, role, budget).await {
        Ok(id) => {
            audit(&actor, "principal.upsert", id, &body.email);
            Json(json!({ "id": id, "email": body.email })).into_response()
        }
        Err(e) => failed(&e),
    }
}

/// Mint an inbound key on a principal. The plaintext is in this reply and
/// nowhere else, ever.
pub async fn mint_key(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Json(body): Json<KeyInput>,
) -> Response {
    let quota = match parse_money(body.quota_usd.as_deref()) {
        Ok(quota) => quota,
        Err(message) => return invalid(&message),
    };
    let route = body.route.as_deref().unwrap_or("default");
    match oag_store::repo::mint_key(&state.db, &body.principal_email, route, &body.name, quota)
        .await
    {
        Ok(Some(minted)) => {
            audit(&actor, "key.mint", minted.id, &body.name);
            Json(json!({
                "id": minted.id,
                "key_prefix": minted.prefix,
                // Shown once. Not stored, not recoverable, not logged.
                "key": minted.key,
                "note": "this is the only time the key is returned",
            }))
            .into_response()
        }
        Ok(None) => not_found("no principal with that email, or no route with that name"),
        Err(e) => failed(&e),
    }
}

/// Set or clear a principal's monthly budget — the org-level cap.
pub async fn set_principal_budget(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(email): Path<String>,
    Json(body): Json<BudgetInput>,
) -> Response {
    let budget = match parse_money(body.monthly_budget_usd.as_deref()) {
        Ok(budget) => budget,
        Err(message) => return invalid(&message),
    };
    match oag_store::repo::set_principal_budget(&state.db, &email, budget).await {
        Ok(Some(id)) => {
            audit(&actor, "principal.budget", id, &email);
            Json(json!({ "id": id, "email": email, "monthly_budget_usd": body.monthly_budget_usd }))
                .into_response()
        }
        Ok(None) => not_found("no principal with that email"),
        Err(e) => failed(&e),
    }
}

/// Set or clear one key's spend cap — the per-member cap.
pub async fn set_key_quota(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<QuotaInput>,
) -> Response {
    let quota = match parse_money(body.quota_usd.as_deref()) {
        Ok(quota) => quota,
        Err(message) => return invalid(&message),
    };
    match oag_store::repo::set_key_quota(&state.db, id, quota).await {
        Ok(Some((name, prefix))) => {
            audit(&actor, "key.quota", id, &name);
            Json(json!({
                "id": id,
                "name": name,
                "key_prefix": prefix,
                "quota_usd": body.quota_usd,
            }))
            .into_response()
        }
        Ok(None) => not_found("no key with that id"),
        Err(e) => failed(&e),
    }
}

/// A principal's budget and month-to-date spend — the org rollup.
pub async fn principal_usage(
    State(state): State<Arc<AppState>>,
    Path(email): Path<String>,
) -> Response {
    match oag_store::repo::principal_usage(&state.db, &email).await {
        Ok(Some(usage)) => Json(json!({
            "id": usage.principal_id,
            "email": usage.email,
            "monthly_budget_usd": usage.monthly_budget_usd.map(|b| format!("{b:.6}")),
            "month_to_date_usd": format!("{:.6}", usage.month_to_date_usd),
            "requests": usage.requests,
        }))
        .into_response(),
        Ok(None) => not_found("no principal with that email"),
        Err(e) => failed(&e),
    }
}

/// One key's cap and spend — the per-member (or per-coworker) figure a partner
/// service shows next to the holder, and evaluates its own per-key limits
/// against. `spent_usd` is what the gateway's cap is enforced against (lifetime);
/// the three windows are the ledger since an instant, each with the moment it
/// next frees up (a rolling window: when its oldest spend ages out; the month:
/// the first of next month). All given so the caller cannot mistake one for
/// another. Instants are RFC 3339; money is a string.
pub async fn key_usage(State(state): State<Arc<AppState>>, Path(id): Path<uuid::Uuid>) -> Response {
    let rfc3339 = |t: time::OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    };
    // The points price first: without it the points fields are null, never zero.
    let reference = match oag_store::repo::points_reference(&state.db).await {
        Ok(reference) => reference,
        Err(e) => return failed(&e),
    };
    match oag_store::repo::key_usage(&state.db, id, reference).await {
        Ok(Some(usage)) => Json(json!({
            "id": usage.key_id,
            "name": usage.name,
            "key_prefix": usage.prefix,
            "principal": usage.principal_email,
            "active": usage.active,
            "quota_usd": usage.quota_usd.map(|q| format!("{q:.6}")),
            "spent_usd": format!("{:.6}", usage.spent_usd),
            "month_to_date_usd": format!("{:.6}", usage.month_to_date_usd),
            "month_resets_at": rfc3339(usage.month_resets_at),
            "requests": usage.requests,
            "five_hour_usd": format!("{:.6}", usage.five_hour_usd),
            "five_hour_frees_at": usage.five_hour_frees_at.map(rfc3339),
            "seven_day_usd": format!("{:.6}", usage.seven_day_usd),
            "seven_day_frees_at": usage.seven_day_frees_at.map(rfc3339),
            // Requests per rolling window, and what the same tokens would have cost at the
            // model's list API price: a subscription seat's usage shown against the bill it
            // displaced ("12 requests · would have cost $0.41 on API"), since its cost is
            // truthfully zero. The month's request count is `requests` above.
            "five_hour_requests": usage.five_hour_requests,
            "seven_day_requests": usage.seven_day_requests,
            "month_counterfactual_usd": format!("{:.6}", usage.month_counterfactual_usd),
            "five_hour_counterfactual_usd": format!("{:.6}", usage.five_hour_counterfactual_usd),
            "seven_day_counterfactual_usd": format!("{:.6}", usage.seven_day_counterfactual_usd),
            // The rolling day: the optional daily brake a coworker's owner may set.
            "day_usd": format!("{:.6}", usage.day_usd),
            "day_frees_at": usage.day_frees_at.map(rfc3339),
            "day_requests": usage.day_requests,
            "day_counterfactual_usd": format!("{:.6}", usage.day_counterfactual_usd),
            // Points per window; null while no reference price is set.
            "month_points": usage.month_points,
            "five_hour_points": usage.five_hour_points,
            "day_points": usage.day_points,
            "seven_day_points": usage.seven_day_points,
        }))
        .into_response(),
        Ok(None) => not_found("no key with that id"),
        Err(e) => failed(&e),
    }
}

/// Money arrives as a STRING, never a float: a budget is currency, and JSON
/// numbers are binary floating point. `None`/absent clears the value.
fn parse_money(raw: Option<&str>) -> std::result::Result<Option<Decimal>, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let value: Decimal = raw
        .parse()
        .map_err(|_| format!("'{raw}' is not an amount, e.g. \"25.00\""))?;
    if value.is_sign_negative() {
        return Err("an amount cannot be negative".to_string());
    }
    // The columns are numeric(14,6) — eight integer digits. Rejecting here makes an
    // out-of-range budget a 400 that says so, rather than a Postgres overflow
    // surfacing as a bare 500 with the real cause swallowed.
    if value >= Decimal::from(100_000_000u32) {
        return Err("an amount must be under 100000000".to_string());
    }
    Ok(Some(value))
}

#[derive(Debug, serde::Deserialize)]
pub struct PrincipalInput {
    pub email: String,
    pub role: Option<String>,
    pub monthly_budget_usd: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct KeyInput {
    pub principal_email: String,
    pub name: String,
    pub route: Option<String>,
    pub quota_usd: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct BudgetInput {
    pub monthly_budget_usd: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct QuotaInput {
    pub quota_usd: Option<String>,
}

/// The record of who changed what.
///
/// `warn!` rather than `info!` on purpose: the default log filter is `info`, but
/// it is a free-form string, and an operator tightening it to `warn` to quieten
/// a noisy deployment would otherwise erase the entire audit trail without
/// noticing. The `oag::audit` target keeps it greppable either way.
fn audit(actor: &AdminActor, action: &str, subject: uuid::Uuid, name: &str) {
    tracing::warn!(
        target: "oag::audit",
        actor = %actor.email,
        actor_id = %actor.principal_id,
        action,
        %subject,
        name,
        "admin write"
    );
}
