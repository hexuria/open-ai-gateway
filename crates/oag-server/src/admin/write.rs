//! The four admin mutations.
//!
//! Deliberately four. This project dropped sub2api's admin sprawl — a
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
//!
//! No request bodies. Every one of these is a verb against an id, so there is
//! nothing to send, nothing to validate, and nothing for serde to turn from an
//! absent field into a null one.

use super::auth::AdminActor;
use super::{failed, not_found};
use crate::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use oag_core::AccountId;
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
