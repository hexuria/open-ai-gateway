//! `oag admin route`: routes, their mode and their ladder of rungs.

use super::RouteCommand;
use oag_core::Result;
use oag_store::Db;
use rust_decimal::Decimal;
use uuid::Uuid;

pub(super) async fn route_cmd(db: &Db, cmd: RouteCommand) -> Result<()> {
    match cmd {
        RouteCommand::Mode { mode, route } => set_mode(db, &route, mode.as_str()).await,
        RouteCommand::Tiers { route, rungs } => {
            let parsed = parse_ladder_rungs(&rungs)?;
            set_rungs(db, &route, parsed).await
        }
        RouteCommand::Show { route } => show_route(db, &route).await,
    }
}

pub(super) fn parse_ladder_rungs(specs: &[String]) -> Result<Vec<oag_router::ladder::Rung>> {
    if specs.is_empty() {
        return Err(oag_core::Error::Config(
            "pass rungs as name=model[,model] cheapest first, e.g. cheap=xai/grok-4.3".to_owned(),
        ));
    }
    let mut rungs = Vec::with_capacity(specs.len());
    for spec in specs {
        let Some((name, models)) = spec.split_once('=') else {
            return Err(oag_core::Error::Config(format!(
                "expected name=model[,model], got '{spec}'"
            )));
        };
        let models: Vec<oag_router::ModelId> = models
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(oag_router::ModelId::new)
            .collect();
        if models.is_empty() {
            return Err(oag_core::Error::Config(format!(
                "rung '{name}' has no models"
            )));
        }
        rungs.push(oag_router::ladder::Rung {
            name: oag_core::TierName::from(name),
            models,
        });
    }
    Ok(rungs)
}

async fn set_rungs(db: &Db, route: &str, rungs: Vec<oag_router::ladder::Rung>) -> Result<()> {
    if oag_router::TierLadder::new(rungs.clone()).is_none() {
        return Err(oag_core::Error::Config(
            "a ladder needs at least one rung".to_owned(),
        ));
    }
    let value = serde_json::to_value(&rungs).map_err(oag_core::Error::Serde)?;
    let n = sqlx::query("UPDATE route SET tiers = $2, updated_at = now() WHERE name = $1")
        .bind(route)
        .bind(&value)
        .execute(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("setting tiers: {e}")))?;
    if n.rows_affected() == 0 {
        return Err(oag_core::Error::Config(format!("no route named {route}")));
    }
    println!("route '{route}' ladder set: {} rungs", rungs.len());
    for (i, r) in rungs.iter().enumerate() {
        println!("  {i}. {} -> {}", r.name, r.models.len());
    }
    Ok(())
}

async fn show_route(db: &Db, route: &str) -> Result<()> {
    let row: Option<(String, serde_json::Value, Option<String>, Option<Decimal>)> = sqlx::query_as(
        "SELECT default_mode, tiers, floor_tier, monthly_budget_usd FROM route WHERE name = $1",
    )
    .bind(route)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("loading route: {e}")))?;
    let Some((mode, tiers, floor, budget)) = row else {
        return Err(oag_core::Error::Config(format!(
            "no route named {route}; `oag admin init` creates 'default'"
        )));
    };
    let rungs: Vec<oag_router::ladder::Rung> =
        serde_json::from_value(tiers).map_err(oag_core::Error::Serde)?;
    println!("route {route}");
    println!("  mode    {mode}");
    println!("  floor   {}", floor.as_deref().unwrap_or("(none)"));
    println!(
        "  budget  {}",
        budget.map_or_else(|| "uncapped".to_owned(), |b| format!("${b}/mo"))
    );
    println!("  ladder");
    for (i, r) in rungs.iter().enumerate() {
        let models: Vec<&str> = r.models.iter().map(oag_router::ModelId::as_str).collect();
        println!("    {i}. {} = {}", r.name, models.join(","));
    }
    Ok(())
}

/// A starter ladder. Deliberately three rungs with one model each: it is the
/// smallest thing that demonstrates classification, escalation, and budget
/// downgrade all doing something.
const DEFAULT_TIERS: &str = r#"[
  {"name": "cheap",    "models": ["anthropic/claude-haiku-4.5"]},
  {"name": "balanced", "models": ["anthropic/claude-sonnet-4.5"]},
  {"name": "frontier", "models": ["anthropic/claude-opus-5"]}
]"#;

pub(super) async fn upsert_route(db: &Db, name: &str) -> Result<Uuid> {
    let tiers: serde_json::Value =
        serde_json::from_str(DEFAULT_TIERS).map_err(oag_core::Error::Serde)?;
    let id: (Uuid,) = sqlx::query_as(
        r"
        INSERT INTO route (id, name, tiers, default_mode)
        VALUES ($1, $2, $3, 'passthrough')
        ON CONFLICT (name) DO UPDATE SET updated_at = now()
        RETURNING id
        ",
    )
    .bind(Uuid::now_v7())
    .bind(name)
    .bind(tiers)
    .fetch_one(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("creating route: {e}")))?;
    Ok(id.0)
}

pub(super) async fn set_mode(db: &Db, route: &str, mode: &str) -> Result<()> {
    if !matches!(mode, "passthrough" | "managed") {
        return Err(oag_core::Error::Config(format!(
            "mode must be 'passthrough' or 'managed', not '{mode}'"
        )));
    }
    let n = sqlx::query("UPDATE route SET default_mode = $2, updated_at = now() WHERE name = $1")
        .bind(route)
        .bind(mode)
        .execute(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("setting mode: {e}")))?;
    if n.rows_affected() == 0 {
        return Err(oag_core::Error::Config(format!("no route named {route}")));
    }
    println!("route '{route}' mode: {mode}");
    if mode == "managed" {
        println!("  concrete model names will now be overridden by policy");
    } else {
        println!("  concrete model names will be honoured; oag/* stays managed");
    }
    Ok(())
}

pub(super) async fn set_tiers_json(db: &Db, route: &str, tiers: &str) -> Result<()> {
    // Parse through the real type, so a malformed ladder is rejected here and
    // not on the first request that route serves.
    let rungs: Vec<oag_router::ladder::Rung> =
        serde_json::from_str(tiers).map_err(oag_core::Error::Serde)?;
    set_rungs(db, route, rungs).await
}
