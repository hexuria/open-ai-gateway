//! Points: the reference price and every model's multipliers.
//!
//! One point is one token at the reference price R (USD per million tokens, the admin's).
//! A token of class c on model m costs `price(m, c) / R` points, so a request's points are
//! exactly its list-price cost over R: `counterfactual_api_usd × 1,000,000 / R`. The
//! multipliers here are derived from the catalog's prices at read time, never stored, so a
//! price refresh or a change of R moves them together. This gateway derives and reports; the
//! partner service that reads them (`OpenGrok`) is the one that enforces a limit in points.

use super::{AdminActor, failed, invalid, not_found};
use crate::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use oag_store::repo;
use oag_store::repo::UsageWindow;
use oag_store::rows::ModelRow;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use time::OffsetDateTime;

/// A model's multipliers over the reference price, per token class. `None` where the catalog
/// has no price for the class. `shown_x` is the input multiplier: the one figure a picker
/// shows after the id; the four are for hover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPoints {
    pub id: String,
    pub input_x: String,
    pub output_x: String,
    pub cache_read_x: Option<String>,
    pub cache_write_x: Option<String>,
    pub shown_x: String,
}

/// A multiplier as a short decimal string: `10`, `2.5`, `0.125` — six places at most,
/// trailing zeros dropped, so the picker prints `×10` and not `×10.000000`.
fn multiplier(price_per_mtok: Decimal, reference: Decimal) -> String {
    (price_per_mtok / reference)
        .round_dp(6)
        .normalize()
        .to_string()
}

/// The multipliers of one catalog row at reference `r`. Pure, so the table in the plan is a
/// unit test.
pub fn multipliers(model: &ModelRow, r: Decimal) -> ModelPoints {
    let input_x = multiplier(model.input_per_mtok, r);
    ModelPoints {
        id: model.id.clone(),
        shown_x: input_x.clone(),
        input_x,
        output_x: multiplier(model.output_per_mtok, r),
        cache_read_x: model.cache_read_per_mtok.map(|p| multiplier(p, r)),
        cache_write_x: model.cache_write_per_mtok.map(|p| multiplier(p, r)),
    }
}

/// `GET /admin/api/points/reference` → `{ "usd_per_mtok": "0.200000" }`, or null while unset.
pub async fn points_reference(State(state): State<Arc<AppState>>) -> Response {
    match repo::points_reference(&state.db).await {
        Ok(reference) => {
            Json(json!({ "usd_per_mtok": reference.map(|r| format!("{r:.6}")) })).into_response()
        }
        Err(e) => failed(&e),
    }
}

#[derive(Debug, Deserialize)]
pub struct ReferenceInput {
    pub usd_per_mtok: String,
}

/// `PUT /admin/api/points/reference` ← `{ "usd_per_mtok": "0.200000" }`. Money as a string;
/// refused unless positive and under the column's range. Audited: a change re-values every
/// pool a partner service holds in points.
pub async fn set_points_reference(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Json(body): Json<ReferenceInput>,
) -> Response {
    let raw = body.usd_per_mtok.trim();
    let Ok(value) = raw.parse::<Decimal>() else {
        return invalid(&format!("'{raw}' is not a price, e.g. \"0.20\""));
    };
    if value <= Decimal::ZERO {
        return invalid("the reference price must be positive: a point is one token at it");
    }
    if value >= Decimal::from(1_000_000u32) {
        return invalid("the reference price must be under 1000000 USD per million tokens");
    }
    match repo::set_points_reference(&state.db, value).await {
        Ok(()) => {
            let shown = format!("{value:.6}");
            audit(&actor, "points.reference", "reference", &shown);
            Json(json!({ "usd_per_mtok": shown })).into_response()
        }
        Err(e) => failed(&e),
    }
}

/// `GET /admin/api/points/models` → every catalog model's multipliers at the current
/// reference. 404 with the reason while no reference is set: an empty list would read as
/// "no models", which is a different fact.
pub async fn points_models(State(state): State<Arc<AppState>>) -> Response {
    let reference = match repo::points_reference(&state.db).await {
        Ok(Some(reference)) => reference,
        Ok(None) => {
            return not_found("no reference price set; PUT /admin/api/points/reference first");
        }
        Err(e) => return failed(&e),
    };
    match repo::catalog(&state.db).await {
        Ok(models) => {
            let rows: Vec<serde_json::Value> = models
                .iter()
                .map(|model| {
                    let points = multipliers(model, reference);
                    json!({
                        "id": points.id,
                        "input_x": points.input_x,
                        "output_x": points.output_x,
                        "cache_read_x": points.cache_read_x,
                        "cache_write_x": points.cache_write_x,
                        "shown_x": points.shown_x,
                    })
                })
                .collect();
            Json(rows).into_response()
        }
        Err(e) => failed(&e),
    }
}

#[derive(Debug, Deserialize)]
pub struct WindowQuery {
    pub window: Option<String>,
}

/// The window a query names, month by default; the sentence for a 400 otherwise.
fn window_of(raw: Option<&str>) -> std::result::Result<UsageWindow, String> {
    let raw = raw.unwrap_or("month");
    UsageWindow::parse(raw)
        .ok_or_else(|| format!("'{raw}' is not a window; one of 5h, 24h, 7d, month"))
}

/// `GET /admin/api/keys/{id}/usage/models?window=5h|24h|7d|month` → one row per model the key
/// used inside the window: requests, tokens by class, cost, list price, points. A key that
/// exists and used nothing is `[]`; an unknown id is 404.
pub async fn key_usage_models(
    State(state): State<Arc<AppState>>,
    Path(id): Path<uuid::Uuid>,
    Query(query): Query<WindowQuery>,
) -> Response {
    let window = match window_of(query.window.as_deref()) {
        Ok(window) => window,
        Err(message) => return invalid(&message),
    };
    match repo::key_exists(&state.db, id).await {
        Ok(true) => {}
        Ok(false) => return not_found("no key with that id"),
        Err(e) => return failed(&e),
    }
    let reference = match repo::points_reference(&state.db).await {
        Ok(reference) => reference,
        Err(e) => return failed(&e),
    };
    match repo::key_usage_by_model(&state.db, id, window, reference, OffsetDateTime::now_utc())
        .await
    {
        Ok(rows) => Json(
            rows.iter()
                .map(|row| {
                    json!({
                        "model_id": row.model_id,
                        "requests": row.requests,
                        "input_tokens": row.input_tokens,
                        "output_tokens": row.output_tokens,
                        "cache_read_tokens": row.cache_read_tokens,
                        "cache_write_tokens": row.cache_write_tokens,
                        "cost_usd": format!("{:.6}", row.cost_usd),
                        "list_usd": format!("{:.6}", row.list_usd),
                        "points": row.points,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => failed(&e),
    }
}

/// How many keys one pool read may name. A member with fifty coworkers is fifty; five hundred
/// is a bug or an attack.
const MAX_POOL_KEYS: usize = 500;

#[derive(Debug, Deserialize)]
pub struct PointsInput {
    pub keys: Vec<uuid::Uuid>,
    pub window: Option<String>,
}

/// `POST /admin/api/usage/points` ← `{ "keys": [...], "window": "month" }` → the points each
/// key spent inside the window and their total, in one query — the partner service's pool
/// read (a member's pool is the sum over that member's coworker keys). A key with no rows is
/// 0; 404 with the reason while no reference price is set.
pub async fn points_for_keys(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PointsInput>,
) -> Response {
    let window = match window_of(body.window.as_deref()) {
        Ok(window) => window,
        Err(message) => return invalid(&message),
    };
    if body.keys.len() > MAX_POOL_KEYS {
        return invalid(&format!("at most {MAX_POOL_KEYS} keys per read"));
    }
    let reference = match repo::points_reference(&state.db).await {
        Ok(Some(reference)) => reference,
        Ok(None) => {
            return not_found("no reference price set; PUT /admin/api/points/reference first");
        }
        Err(e) => return failed(&e),
    };
    let mut keys: Vec<uuid::Uuid> = body.keys;
    keys.sort_unstable();
    keys.dedup();
    match repo::points_for_keys(
        &state.db,
        &keys,
        window,
        reference,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(spent) => {
            let mut per_key = serde_json::Map::new();
            let mut total: i64 = 0;
            for key in &keys {
                let points = spent
                    .iter()
                    .find(|(id, _)| id == key)
                    .map_or(0, |(_, points)| *points);
                total = total.saturating_add(points);
                per_key.insert(key.to_string(), json!(points));
            }
            Json(json!({ "window": window.as_str(), "keys": per_key, "total": total }))
                .into_response()
        }
        Err(e) => failed(&e),
    }
}

/// The audit line every admin write emits; the subject is the word "reference", there being
/// one. `warn!` for the reason [`super::write`] gives.
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
    fn dec(text: &str) -> Decimal {
        text.parse().expect("a decimal literal")
    }

    fn model(id: &str, input: Decimal, output: Decimal, cache_read: Option<Decimal>) -> ModelRow {
        ModelRow {
            id: id.to_owned(),
            provider: id.split('/').next().unwrap_or_default().to_owned(),
            upstream_name: id.to_owned(),
            input_per_mtok: input,
            output_per_mtok: output,
            cache_read_per_mtok: cache_read,
            cache_write_per_mtok: None,
            context_window: 128_000,
            max_output_tokens: 8_192,
            supports_vision: false,
            supports_tools: true,
            supports_reasoning: false,
            supports_prompt_cache: cache_read.is_some(),
            display_label: None,
        }
    }

    /// The table in the plan, at R = $0.20: "$0.20 per million is 1×, $5 per million is 25×".
    #[test]
    fn multipliers_are_list_price_over_the_reference_per_token_class() {
        let r = dec("0.20");
        let grok = multipliers(
            &model("xai/grok-4.6", dec("2.00"), dec("6.00"), Some(dec("0.50"))),
            r,
        );
        assert_eq!(grok.input_x, "10");
        assert_eq!(grok.output_x, "30");
        assert_eq!(grok.cache_read_x.as_deref(), Some("2.5"));
        assert_eq!(grok.cache_write_x, None);
        assert_eq!(grok.shown_x, "10", "the picker shows the input multiplier");
        let gpt55 = multipliers(
            &model("openai/gpt-5.5", dec("5"), dec("30"), Some(dec("0.50"))),
            r,
        );
        assert_eq!(
            (gpt55.input_x.as_str(), gpt55.output_x.as_str()),
            ("25", "150")
        );
        let mini = multipliers(
            &model(
                "openai/gpt-5-mini",
                dec("0.25"),
                dec("2.00"),
                Some(dec("0.025")),
            ),
            r,
        );
        assert_eq!(mini.input_x, "1.25");
        assert_eq!(mini.cache_read_x.as_deref(), Some("0.125"));
        let luna = multipliers(
            &model(
                "openai/gpt-5.6-luna",
                dec("0.20"),
                dec("1.20"),
                Some(dec("0.02")),
            ),
            r,
        );
        assert_eq!(
            luna.input_x, "1",
            "the reference model is 1×, written without zeros"
        );
        assert_eq!(luna.cache_read_x.as_deref(), Some("0.1"));
    }

    #[test]
    fn a_change_of_reference_rescales_every_multiplier_together() {
        let m = model("xai/grok-4.6", dec("2.00"), dec("6.00"), Some(dec("0.50")));
        assert_eq!(multipliers(&m, dec("0.20")).input_x, "10");
        assert_eq!(multipliers(&m, dec("0.40")).input_x, "5");
        assert_eq!(multipliers(&m, dec("0.40")).output_x, "15");
    }
}
