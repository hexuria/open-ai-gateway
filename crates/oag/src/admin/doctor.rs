//! `oag admin doctor` — why a request on this route would fail.

use oag_core::config::Config;
use oag_core::{Provider, Result};
use oag_store::Db;

const EXPECTED_MIGRATIONS: usize = 5;

pub async fn run(db: &Db, config: &Config, route: &str) -> Result<()> {
    let mut failed = 0u32;

    failed += check_migrations(db).await?;
    failed += check_catalog(db).await?;
    let Some((_mode, rungs)) = check_route(db, route).await? else {
        failed += 1;
        return conclude(failed);
    };
    let accounts = load_accounts(db, route).await?;
    failed += report_accounts(&accounts);
    failed += check_ladder(&rungs, &accounts, route);
    failed += check_seat_prices(&accounts);
    failed += check_codex(config, &accounts);
    conclude(failed)
}

fn conclude(failed: u32) -> Result<()> {
    if failed == 0 {
        println!("ok");
        Ok(())
    } else {
        Err(oag_core::Error::Config(format!(
            "doctor found {failed} problem(s); commands that fix them are printed above"
        )))
    }
}

async fn check_migrations(db: &Db) -> Result<u32> {
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(db.pool())
            .await
            .map_err(|e| oag_core::Error::Internal(format!("reading migrations: {e}")))?;
    if applied.len() >= EXPECTED_MIGRATIONS {
        println!("ok   migrations  {} applied", applied.len());
        Ok(0)
    } else {
        println!(
            "FAIL migrations  {} applied, expected at least {EXPECTED_MIGRATIONS}",
            applied.len()
        );
        println!("     fix: oag migrate");
        Ok(1)
    }
}

async fn check_catalog(db: &Db) -> Result<u32> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_catalog")
        .fetch_one(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("counting catalog: {e}")))?;
    if n == 0 {
        println!("FAIL catalog     empty; every request will fail to route");
        println!("     fix: oag admin catalog seed");
        Ok(1)
    } else {
        println!("ok   catalog     {n} models");
        Ok(0)
    }
}

async fn check_route(
    db: &Db,
    route: &str,
) -> Result<Option<(String, Vec<oag_router::ladder::Rung>)>> {
    let row: Option<(String, serde_json::Value)> =
        sqlx::query_as("SELECT default_mode, tiers FROM route WHERE name = $1")
            .bind(route)
            .fetch_optional(db.pool())
            .await
            .map_err(|e| oag_core::Error::Internal(format!("loading route: {e}")))?;
    let Some((mode, tiers)) = row else {
        println!("FAIL route       no route named '{route}'");
        println!("     fix: oag admin init --route {route}");
        return Ok(None);
    };
    let rungs: Vec<oag_router::ladder::Rung> =
        serde_json::from_value(tiers).map_err(oag_core::Error::Serde)?;
    println!("ok   route       {route}  mode={mode}");
    let ladder: Vec<String> = rungs
        .iter()
        .map(|r| {
            let models: Vec<&str> = r.models.iter().map(oag_router::ModelId::as_str).collect();
            format!("{}={}", r.name, models.join(","))
        })
        .collect();
    println!("ok   ladder      {}", ladder.join(" "));
    Ok(Some((mode, rungs)))
}

struct Seat {
    name: String,
    provider: String,
    kind: String,
    schedulable: bool,
    cooldown_until: Option<time::OffsetDateTime>,
    rate_limited_until: Option<time::OffsetDateTime>,
    /// The seat's flat monthly price. `None` is not a free seat — it is a seat
    /// nobody has told the gateway the price of, which is why it is checked.
    monthly_cost_usd: Option<rust_decimal::Decimal>,
}

impl Seat {
    fn live(&self, now: time::OffsetDateTime) -> bool {
        self.schedulable
            && self.cooldown_until.is_none_or(|t| t <= now)
            && self.rate_limited_until.is_none_or(|t| t <= now)
    }

    fn state(&self, now: time::OffsetDateTime) -> &'static str {
        if !self.schedulable {
            "disabled"
        } else if self.cooldown_until.is_some_and(|t| t > now) {
            "cooling down"
        } else if self.rate_limited_until.is_some_and(|t| t > now) {
            "rate limited"
        } else {
            "ready"
        }
    }
}

async fn load_accounts(db: &Db, route: &str) -> Result<Vec<Seat>> {
    sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            bool,
            Option<time::OffsetDateTime>,
            Option<time::OffsetDateTime>,
            Option<rust_decimal::Decimal>,
        ),
    >(
        r"
        SELECT a.name, a.provider, a.kind, a.schedulable, a.cooldown_until,
               a.rate_limited_until, a.monthly_cost_usd
        FROM account a
        JOIN account_route ar ON ar.account_id = a.id
        JOIN route r ON r.id = ar.route_id
        WHERE r.name = $1
        ORDER BY a.provider, a.name
        ",
    )
    .bind(route)
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing accounts: {e}")))
    .map(|rows| {
        rows.into_iter()
            .map(
                |(
                    name,
                    provider,
                    kind,
                    schedulable,
                    cooldown_until,
                    rate_limited_until,
                    monthly_cost_usd,
                )| Seat {
                    name,
                    provider,
                    kind,
                    schedulable,
                    cooldown_until,
                    rate_limited_until,
                    monthly_cost_usd,
                },
            )
            .collect()
    })
}

fn report_accounts(accounts: &[Seat]) -> u32 {
    let now = time::OffsetDateTime::now_utc();
    if accounts.is_empty() {
        println!("FAIL accounts    none attached to this route");
        println!(
            "     fix: oag admin account add --name <n> --provider <p> --secret <key> --route <route>"
        );
        return 1;
    }
    println!("ok   accounts    {} attached", accounts.len());
    for a in accounts {
        println!(
            "       {:<20} {:<12} {:<10} {}",
            a.name,
            a.provider,
            a.kind,
            a.state(now)
        );
    }
    0
}

fn check_ladder(rungs: &[oag_router::ladder::Rung], accounts: &[Seat], route: &str) -> u32 {
    let now = time::OffsetDateTime::now_utc();
    let mut failed = 0;
    for r in rungs {
        let mut providers = Vec::new();
        for model in &r.models {
            let p = model.as_str().split('/').next().unwrap_or(model.as_str());
            if !providers.contains(&p) {
                providers.push(p);
            }
        }
        let missing: Vec<&str> = providers
            .iter()
            .copied()
            .filter(|p| !accounts.iter().any(|a| a.provider == *p && a.live(now)))
            .collect();
        if missing.is_empty() {
            println!(
                "ok   rung {:<10} live credential for {}",
                r.name,
                providers.join(", ")
            );
        } else {
            failed += 1;
            println!(
                "FAIL rung {:<10} no live {} credential on route '{route}'",
                r.name,
                missing.join("/")
            );
            println!(
                "     fix: oag admin account add --name {p}-1 --provider {p} --secret <key> --route {route}",
                p = missing[0]
            );
        }
    }
    failed
}

/// A flat-rate seat with no price cannot be netted off against what its traffic
/// would have cost, so its saving reads as a dash forever with nothing saying
/// why. Nothing can infer the figure — a provider reports how much of a plan is
/// left, never what the plan costs — so an unset price is a question only an
/// operator can answer, and this is where it gets asked.
///
/// A warning rather than a failure: the gateway serves traffic perfectly well
/// without knowing what the seat cost, and refusing to start over a reporting
/// gap would be out of proportion.
fn check_seat_prices(accounts: &[Seat]) -> u32 {
    let unpriced: Vec<&str> = accounts
        .iter()
        .filter(|a| a.kind == "oauth" && a.monthly_cost_usd.is_none())
        .map(|a| a.name.as_str())
        .collect();
    if unpriced.is_empty() {
        return 0;
    }
    println!(
        "WARN seats       no monthly price on {}; saving reads as unknown",
        unpriced.join(", ")
    );
    println!(
        "     fix: oag admin account set-cost {} --monthly-cost <your plan price>",
        unpriced[0]
    );
    // Deliberately not counted as a failure, so `doctor` still exits zero.
    0
}

fn check_codex(config: &Config, accounts: &[Seat]) -> u32 {
    let has_codex = accounts
        .iter()
        .any(|a| a.provider == Provider::OpenAI.as_str() && a.kind == "oauth");
    if !has_codex {
        return 0;
    }
    let cx = &config.gateway.codex;
    let set = match &cx.instructions_path {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => true,
            Ok(_) => {
                println!("FAIL codex       instructions file {path} is empty");
                println!(
                    "     fix: set gateway.codex.instructions_path to deploy/codex-instructions.txt"
                );
                return 1;
            }
            Err(e) => {
                println!("FAIL codex       cannot read instructions file {path}: {e}");
                println!(
                    "     fix: set gateway.codex.instructions_path to deploy/codex-instructions.txt"
                );
                return 1;
            }
        },
        None => cx
            .instructions
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty()),
    };
    if set {
        println!("ok   codex       gateway.codex.instructions is set");
        0
    } else {
        println!(
            "FAIL codex       an OpenAI OAuth seat is attached but gateway.codex.instructions is unset"
        );
        println!(
            "     fix: set gateway.codex.instructions_path: deploy/codex-instructions.txt (the backend refuses the request without it)"
        );
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(name: &str, kind: &str, cost: Option<i64>) -> Seat {
        Seat {
            name: name.to_owned(),
            provider: "xai".to_owned(),
            kind: kind.to_owned(),
            schedulable: true,
            cooldown_until: None,
            rate_limited_until: None,
            monthly_cost_usd: cost.map(rust_decimal::Decimal::from),
        }
    }

    #[test]
    fn a_seat_nobody_priced_is_named_along_with_the_command_that_prices_it() {
        // The saving column can only show a dash without this figure, and a
        // dash weeks later reads as a broken report rather than an unanswered
        // question. Nothing can infer it, so the check exists to ask.
        assert_eq!(check_seat_prices(&[seat("grok", "oauth", None)]), 0);
    }

    #[test]
    fn a_priced_seat_is_not_nagged_about() {
        assert_eq!(check_seat_prices(&[seat("grok", "oauth", Some(300))]), 0);
    }

    #[test]
    fn a_metered_key_is_never_asked_for_a_monthly_price() {
        // An API key is billed per token; there is no plan behind it to price,
        // so asking would be noise on every run of a perfectly healthy gateway.
        assert_eq!(check_seat_prices(&[seat("openai-key", "api_key", None)]), 0);
    }

    #[test]
    fn an_unpriced_seat_does_not_fail_the_check() {
        // Serving is unaffected by not knowing what a seat cost. Exiting
        // non-zero over a reporting gap would make `doctor` unusable in CI.
        let seats = [seat("a", "oauth", None), seat("b", "oauth", None)];
        assert_eq!(check_seat_prices(&seats), 0, "a warning, never a failure");
    }
}
