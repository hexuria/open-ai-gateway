//! Naming models, as opposed to addressing them.
//!
//! A catalog id is an address: clients send it, a route's ladder names it, and
//! the ledger records spend under it, so renaming one rewrites all three at
//! once and breaks every historical join. A label is a name, and this is the
//! endpoint that changes one. Splitting them is what makes renaming safe enough
//! to do from a web page.
//!
//! Only two verbs live here, both against the catalog rather than against an
//! incident: the reads in [`super`] are per-request state, and these are
//! configuration.

use super::{AdminActor, failed, invalid, not_found};
use crate::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use oag_store::repo;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// The rename body. One field, deliberately: everything else about a catalog
/// row is either derived from the provider or a price, and prices already have
/// their own guarded path.
#[derive(Debug, Deserialize)]
pub struct ModelLabelInput {
    /// The new name. `null` — or a string that is empty once trimmed — clears
    /// it, which restores the derived default rather than storing a copy of
    /// today's derivation.
    #[serde(default)]
    pub display_label: Option<String>,
}

/// `GET /admin/api/models`.
///
/// The catalog as an operator needs to see it: what it is called, what it would
/// be called if nobody had named it, and enough of the row to recognise it.
/// Prices are already on the summary; repeating them here would make this the
/// second place they can look wrong.
pub async fn list_models(State(state): State<Arc<AppState>>) -> Response {
    let mut rows = match repo::catalog(&state.db).await {
        Ok(rows) => rows,
        Err(e) => return failed(&e),
    };
    // Sorted here rather than in the query: the router reads the same rows a
    // few times a minute and does not care about their order, and a table that
    // reshuffles between renders is one an operator cannot rename from.
    rows.sort_by(|a, b| a.id.cmp(&b.id));

    let models: Vec<_> = rows
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "provider": m.provider,
                "upstream_name": m.upstream_name,
                // What is stored, which is null far more often than not.
                "display_label": m.display_label,
                // What a picker shows when the above is null. The page uses it
                // as the placeholder, so an operator can see the default
                // without having to type it in to find out.
                "derived_label": m.derived_label(),
                "context_window": m.context_window,
            })
        })
        .collect();
    Json(json!({ "models": models })).into_response()
}

/// `PATCH /admin/api/models/{id}`.
///
/// The id is a wildcard path capture because a catalog id contains a slash —
/// `xai/grok-4.6` is two path segments, not one — and matching it as a single
/// segment would 404 every model in the catalog.
pub async fn update_model(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<String>,
    Json(input): Json<ModelLabelInput>,
) -> Response {
    let label = match clean_label(input.display_label.as_deref()) {
        Ok(label) => label,
        Err(message) => return invalid(&message),
    };

    match repo::set_model_label(&state.db, &id, label.as_deref()).await {
        Ok(Some(id)) => {
            audit(&actor, "model.label", &id, label.as_deref().unwrap_or(""));
            // The listing renders from the in-memory catalog, so without this
            // the rename does not show up until the refresh interval elapses —
            // and an operator who cannot see their own edit assumes it failed
            // and does it again. This replica only; the others pick it up on
            // their own refresh, exactly as `catalog/reload` says.
            if let Err(e) = state.reload_catalog().await {
                tracing::warn!(error = %e, "renamed a model but could not reload the catalog");
            }
            Json(json!({ "id": id, "display_label": label })).into_response()
        }
        Ok(None) => not_found("no model with that id"),
        Err(e) => failed(&e),
    }
}

/// Trim a submitted label, or turn it into "clear this".
///
/// Empty and absent mean the same thing on purpose: an operator who selects the
/// text in the box and deletes it means "go back to the default", and storing
/// an empty string instead would render a nameless row in every picker.
fn clean_label(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(label) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if label.chars().count() > 128 {
        return Err("display_label must be 128 characters or fewer".to_owned());
    }
    // The dashboard escapes what it renders, but this string also reaches
    // logs and other people's clients; a name with a newline in it is a name
    // that breaks whatever renders it line by line.
    if label.chars().any(char::is_control) {
        return Err("display_label must not contain control characters".to_owned());
    }
    Ok(Some(label.to_owned()))
}

/// The same audit line the account and service writes emit, with a text
/// subject: a catalog id is `provider/model`, not a uuid.
///
/// `warn!` rather than `info!` for the reason [`super::write`] gives — so
/// tightening the filter does not erase the trail of who renamed what.
fn audit(actor: &AdminActor, action: &str, subject: &str, name: &str) {
    tracing::warn!(
        target: "oag::audit",
        actor = %actor.email,
        actor_id = %actor.principal_id,
        action,
        subject,
        name,
        "admin write"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_a_label_is_spelled_the_two_ways_an_operator_would_spell_it() {
        // Deleting the text in the box sends `""`; a client with a real data
        // model sends `null`. Both mean "go back to the derived default", and
        // storing an empty string for the first would render a nameless row.
        assert_eq!(clean_label(None), Ok(None));
        assert_eq!(clean_label(Some("")), Ok(None));
        assert_eq!(clean_label(Some("   ")), Ok(None));
    }

    #[test]
    fn a_label_is_stored_trimmed_because_a_stray_space_is_invisible() {
        assert_eq!(
            clean_label(Some("  Grok, the fast one  ")),
            Ok(Some("Grok, the fast one".to_owned()))
        );
    }

    #[test]
    fn a_label_that_would_break_whatever_renders_it_is_refused() {
        assert!(clean_label(Some("two\nlines")).is_err());
        assert!(clean_label(Some(&"x".repeat(129))).is_err());
        // Counted in characters, not bytes: a name in a non-Latin script is a
        // name, and a byte limit would refuse it at a third of the length.
        assert!(clean_label(Some(&"é".repeat(128))).is_ok());
    }
}
