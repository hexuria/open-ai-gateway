//! `oag admin init`, `status` and `cache`: standing a gateway up and reading its state.

use super::keys::{mint_key, print_key};
use super::principals::{evict_principal_keys, upsert_principal};
use super::routes::upsert_route;
use oag_core::Result;
use oag_store::Db;
use rust_decimal::Decimal;

pub(super) async fn init(
    db: &Db,
    redis_url: &str,
    email: &str,
    route: &str,
    budget: Option<Decimal>,
) -> Result<()> {
    let principal_id = upsert_principal(db, email, "admin", budget).await?;
    if budget.is_some() {
        evict_principal_keys(db, redis_url, principal_id, email).await;
    }
    let route_id = upsert_route(db, route).await?;
    println!("principal {email} -> {principal_id}");
    println!("route     {route} -> {route_id}");
    let key = mint_key(db, email, route, "initial", None, true).await?;
    print_key(&key);
    println!("\nNext:");
    println!("  This is an ADMIN key: it can disable credentials and revoke keys.");
    println!("  Do not paste it into a client. Mint a separate one for SDKs:");
    println!("      oag admin key create --email {email} --route {route} --name codex");
    println!();
    println!("  oag admin catalog seed");
    println!("  oag admin account add --name <n> --provider anthropic --secret <key>");
    println!();
    println!("  This route is in passthrough mode: a client that names a concrete");
    println!("  model gets that model. Clients asking for oag/auto are routed by");
    println!("  policy. To apply policy to every request, including ones that name");
    println!("  a model:");
    println!("      oag admin route mode managed --route {route}");
    Ok(())
}

/// What `oag admin status` prints as this month's headline.
///
/// A constant so the statement can be run on its own in a test. The predicate
/// below is the whole of finding C4 and it is invisible from the outside: the
/// command prints a number, and a wrong number looks exactly like a right one.
///
/// Per-token traffic only, exactly as the admin API's headline does. A seat row
/// has `cost_usd` of zero and a real API-equivalent price, so folding it in lets
/// a flat-rate credential's zero marginal cost inflate the frontier saving —
/// the more a subscription is used, the better this line claims the gateway is
/// doing. Without it the two surfaces differed by an order of magnitude on any
/// deployment holding a seat, and an operator comparing `oag admin status` with
/// the dashboard had no way to tell which of them was lying.
pub(super) const MONTH_HEADLINE_SQL: &str = r"
    SELECT COALESCE(SUM(cost_usd),0), COALESCE(SUM(counterfactual_usd),0),
           COUNT(*) FILTER (WHERE selection_reason NOT IN ('abandoned', 'lost'))
    FROM usage_event
    WHERE occurred_at >= date_trunc('month', now())
      AND NOT (cost_usd = 0 AND counterfactual_api_usd > 0)
";

pub(super) async fn flush_cache(redis_url: &str) -> Result<()> {
    let cache = oag_store::Cache::connect(redis_url)?;
    let n = cache.flush_auth_cache().await?;
    println!("dropped {n} cached auth entries");
    println!("  each replica's in-process cache expires within 15s");
    Ok(())
}

pub(super) async fn status(db: &Db) -> Result<()> {
    let routes: Vec<(String, i64, Option<Decimal>)> = sqlx::query_as(
        r"
        SELECT r.name, count(ar.account_id), r.monthly_budget_usd
        FROM route r LEFT JOIN account_route ar ON ar.route_id = r.id
        GROUP BY r.id ORDER BY r.name
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing routes: {e}")))?;

    println!("routes");
    for (name, accounts, budget) in routes {
        let b = budget.map_or_else(|| "uncapped".to_owned(), |b| format!("${b}/mo"));
        println!("  {name:<20} {accounts} credential(s)  {b}");
    }

    let accounts: Vec<(String, String, bool, Option<time::OffsetDateTime>)> = sqlx::query_as(
        "SELECT name, provider, schedulable, cooldown_until FROM account ORDER BY provider, name",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing accounts: {e}")))?;

    println!("\ncredentials");
    for (name, provider, schedulable, cooldown) in accounts {
        let state = if !schedulable {
            "disabled"
        } else if cooldown.is_some_and(|t| t > time::OffsetDateTime::now_utc()) {
            "cooling down"
        } else {
            "ready"
        };
        println!("  {name:<20} {provider:<12} {state}");
    }

    // The headline number: what the gateway saved this month.
    let spend: Option<(Decimal, Decimal, i64)> = sqlx::query_as(MONTH_HEADLINE_SQL)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("summing spend: {e}")))?;

    if let Some((cost, counterfactual, n)) = spend {
        println!("\nthis month  {n} requests");
        println!("  spent            ${cost:.4}");
        println!("  frontier-for-all ${counterfactual:.4}");
        println!("  saved            ${:.4}", counterfactual - cost);
    }
    Ok(())
}
