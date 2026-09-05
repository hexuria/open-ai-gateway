//! `oag admin doctor` — why a request on this route would fail.

use oag_core::config::Config;
use oag_core::{Provider, Result};
use oag_store::Db;

/// Bumped with 0008. 0007 added `account.usage_reserve_pct`, which every
/// credential query now selects, so a binary on the old schema selects no
/// candidate at all — a failure that reads as an empty pool unless something
/// says the schema is behind, which is this. 0008 added `usage_event.origin`,
/// which the reporting queries group on and the importer writes.
///
/// Raise this with every migration that existing queries depend on. The test
/// below counts the files rather than trusting this line, because the number
/// that matters is the one on disk and a constant is exactly the thing that
/// gets forgotten.
const EXPECTED_MIGRATIONS: usize = 15;

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
    // Named, never counted: an unpriced seat is a reporting gap and the gateway
    // serves perfectly well without the figure. Exiting non-zero over it would
    // make `doctor` unusable in CI.
    let _unpriced = check_seat_prices(&accounts);
    failed += check_reserves(&accounts);
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

/// Every migration `1..=EXPECTED_MIGRATIONS`, and every one of them successful.
///
/// Counting rows answered a weaker question than the one being asked. A schema
/// with a gap — 1..13 and 15, which is what a hand-patched `_sqlx_migrations`
/// leaves behind — counted fourteen and passed while the table 14 was supposed
/// to alter did not exist. So did a schema whose last migration is recorded
/// `success = false`, which is the state sqlx leaves after a migration that
/// failed halfway: the row is there, the DDL is not, and doctor called it
/// healthy.
///
/// Both are the same mistake, and it is the one this whole review is about:
/// asserting a proxy for the property instead of the property.
async fn check_migrations(db: &Db) -> Result<u32> {
    let applied: Vec<(i64, bool)> =
        sqlx::query_as("SELECT version, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(db.pool())
            .await
            .map_err(|e| oag_core::Error::Internal(format!("reading migrations: {e}")))?;

    let expected = i64::try_from(EXPECTED_MIGRATIONS).unwrap_or(i64::MAX);
    let succeeded: std::collections::BTreeSet<i64> = applied
        .iter()
        .filter(|(_, ok)| *ok)
        .map(|(v, _)| *v)
        .collect();
    let missing: Vec<i64> = (1..=expected).filter(|v| !succeeded.contains(v)).collect();
    let failed: Vec<i64> = applied
        .iter()
        .filter(|(_, ok)| !*ok)
        .map(|(v, _)| *v)
        .collect();

    if missing.is_empty() && failed.is_empty() {
        println!("ok   migrations  {} applied", applied.len());
        return Ok(0);
    }
    if !failed.is_empty() {
        // Distinguished from simply absent, because the fix differs: `migrate`
        // will not retry a version already recorded, so a failed row has to be
        // dealt with by hand before anything else can move.
        println!(
            "FAIL migrations  {} recorded as failed",
            failed
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "     fix: the DDL did not complete; resolve it and clear the row before migrating"
        );
    }
    if !missing.is_empty() {
        println!(
            "FAIL migrations  {} not applied, of 1..={EXPECTED_MIGRATIONS}",
            missing
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("     fix: oag migrate");
    }
    Ok(1)
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
    /// The provider's last reading of the allowance left, and the floor an
    /// operator put under it. Together they decide whether the seat is being
    /// held back right now, which is a live reason a request would fail.
    usage_remaining_pct: Option<rust_decimal::Decimal>,
    usage_reserve_pct: Option<i16>,
    /// The principal this credential is bound to, if it is bound to one.
    ///
    /// The scheduler filters on it: a credential with an owner serves that
    /// principal's requests and nobody else's. Doctor never selected it, so a
    /// personally bound seat counted as a live credential for its rung and the
    /// route reported `ok` — for every principal, including the ones that
    /// cannot reach it. The first symptom is `no_viable_model` on a route the
    /// CLI has just called healthy.
    owner_principal_id: Option<uuid::Uuid>,
}

impl Seat {
    fn live(&self, now: time::OffsetDateTime) -> bool {
        self.schedulable
            && self.cooldown_until.is_none_or(|t| t <= now)
            && self.rate_limited_until.is_none_or(|t| t <= now)
            && !self.reserved_out()
    }

    /// Whether this credential can serve an arbitrary caller on the route.
    ///
    /// A rung is only covered if something on it will answer *anyone* who is
    /// entitled to the route. An owner-bound credential answers exactly one
    /// principal, so counting it as coverage told every other principal their
    /// route was healthy right up until `no_viable_model`.
    const fn shared(&self) -> bool {
        self.owner_principal_id.is_none()
    }

    /// Whether the reserve is holding this seat out of the pool right now.
    ///
    /// The scheduler's own rule, borrowed rather than restated: a doctor that
    /// disagreed with the scheduler about which credentials are live would be
    /// worse than no doctor at all.
    fn reserved_out(&self) -> bool {
        oag_pool::held_by_reserve(
            self.usage_remaining_pct,
            self.usage_reserve_pct.map(rust_decimal::Decimal::from),
        )
    }

    fn state(&self, now: time::OffsetDateTime) -> &'static str {
        if !self.schedulable {
            "disabled"
        } else if self.cooldown_until.is_some_and(|t| t > now) {
            "cooling down"
        } else if self.rate_limited_until.is_some_and(|t| t > now) {
            "rate limited"
        } else if self.reserved_out() {
            "held back"
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
            Option<rust_decimal::Decimal>,
            Option<i16>,
            Option<uuid::Uuid>,
        ),
    >(
        r"
        SELECT a.name, a.provider, a.kind, a.schedulable, a.cooldown_until,
               a.rate_limited_until, a.monthly_cost_usd,
               a.usage_remaining_pct, a.usage_reserve_pct, a.owner_principal_id
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
                    usage_remaining_pct,
                    usage_reserve_pct,
                    owner_principal_id,
                )| Seat {
                    name,
                    provider,
                    kind,
                    schedulable,
                    cooldown_until,
                    rate_limited_until,
                    monthly_cost_usd,
                    usage_remaining_pct,
                    usage_reserve_pct,
                    owner_principal_id,
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
            .filter(|p| {
                !accounts
                    .iter()
                    .any(|a| a.provider == *p && a.live(now) && a.shared())
            })
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
            // The one failure whose cause is invisible in the listing above.
            // A credential that is live in every other respect but bound to a
            // principal serves that principal and nobody else, so a rung whose
            // only candidate is bound reads as "no credential" here while
            // `account list` shows it ready — and the operator goes looking for
            // an outage that is not there.
            for p in &missing {
                let bound: Vec<&str> = accounts
                    .iter()
                    .filter(|a| a.provider == **p && a.live(now) && !a.shared())
                    .map(|a| a.name.as_str())
                    .collect();
                if !bound.is_empty() {
                    println!(
                        "     note: {} is live but bound to one principal, so it cannot \
                         serve this rung for anyone else",
                        bound.join(", ")
                    );
                }
            }
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
/// Returns the seats it warned about, in the order it named them.
///
/// It used to return `0` unconditionally — correctly, because an unpriced seat
/// is a warning and not a failure — and every test of it asserted that zero.
/// Four tests, none of which could fail: they passed against a function that
/// found the right seats, one that found none, and one that found all of them,
/// because none of that reaches the return value.
///
/// Returning the names makes the thing under test observable without changing
/// what `doctor` does with it: the caller still adds nothing to its failure
/// count, which is what keeps `doctor` exiting zero over a reporting gap.
fn check_seat_prices(accounts: &[Seat]) -> Vec<&str> {
    let unpriced: Vec<&str> = accounts
        .iter()
        .filter(|a| a.kind == "oauth" && a.monthly_cost_usd.is_none())
        .map(|a| a.name.as_str())
        .collect();
    let Some(first) = unpriced.first() else {
        return unpriced;
    };
    println!(
        "WARN seats       no monthly price on {}; saving reads as unknown",
        unpriced.join(", ")
    );
    println!("     fix: oag admin account set-cost {first} --monthly-cost <your plan price>");
    unpriced
}

/// A seat sitting at or below its reserve, which is a request that will fail
/// today rather than a reporting gap.
///
/// Reported as a failure, unlike the unpriced-seat warning: the seat is out of
/// the pool for as long as its window lasts, and `check_ladder` above will
/// already have failed any rung that has nothing else to fall back on. Silence
/// here would leave an operator reading "no live xai credential" while
/// `account list` shows a healthy, enabled, un-cooled seat — the exact
/// confusion the reserve's own error message exists to prevent.
fn check_reserves(accounts: &[Seat]) -> u32 {
    let held: Vec<&Seat> = accounts.iter().filter(|a| a.reserved_out()).collect();
    if held.is_empty() {
        return 0;
    }
    for seat in &held {
        let reserve = seat.usage_reserve_pct.unwrap_or_default();
        let remaining = seat
            .usage_remaining_pct
            .map_or_else(|| "unknown".to_owned(), |r| format!("{r:.0}%"));
        println!(
            "FAIL reserve     {} is at {remaining} of its allowance, at or below its {reserve}% reserve",
            seat.name
        );
    }
    println!(
        "     fix: oag admin account set-reserve {} --pct <lower> (or wait for the window to reset)",
        held[0].name
    );
    u32::try_from(held.len()).unwrap_or(u32::MAX)
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

    #[test]
    fn the_expected_migration_count_matches_the_migrations_on_disk() {
        // This constant is the thing that gets forgotten: a migration lands,
        // the queries start depending on its column, and the check still passes
        // a schema that is one behind — it uses `>=`, so being stale never
        // fails loudly, it just stops catching the case it exists for.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations");
        let on_disk = std::fs::read_dir(dir)
            .expect("migrations directory")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
            .count();
        assert_eq!(
            EXPECTED_MIGRATIONS, on_disk,
            "a migration was added without raising EXPECTED_MIGRATIONS, so doctor \
             would call a one-behind schema healthy"
        );
    }

    fn seat(name: &str, kind: &str, cost: Option<i64>) -> Seat {
        Seat {
            name: name.to_owned(),
            provider: "xai".to_owned(),
            kind: kind.to_owned(),
            schedulable: true,
            cooldown_until: None,
            rate_limited_until: None,
            monthly_cost_usd: cost.map(rust_decimal::Decimal::from),
            usage_remaining_pct: None,
            usage_reserve_pct: None,
            owner_principal_id: None,
        }
    }

    /// The same seat, bound to one principal — the shape the scheduler will
    /// only ever hand to that principal's requests.
    fn owner_bound(name: &str) -> Seat {
        let mut s = seat(name, "oauth", Some(300));
        s.owner_principal_id = Some(uuid::Uuid::new_v4());
        s
    }

    /// A seat with a reading and a floor under it, both in whole percent.
    fn reserved(name: &str, remaining: i64, reserve: i16) -> Seat {
        let mut s = seat(name, "oauth", Some(300));
        s.usage_remaining_pct = Some(rust_decimal::Decimal::from(remaining));
        s.usage_reserve_pct = Some(reserve);
        s
    }

    #[test]
    fn a_seat_nobody_priced_is_named_along_with_the_command_that_prices_it() {
        // The saving column can only show a dash without this figure, and a
        // dash weeks later reads as a broken report rather than an unanswered
        // question. Nothing can infer it, so the check exists to ask.
        assert_eq!(
            check_seat_prices(&[seat("grok", "oauth", None)]),
            vec!["grok"]
        );
    }

    #[test]
    fn a_priced_seat_is_not_nagged_about() {
        assert!(
            check_seat_prices(&[seat("grok", "oauth", Some(300))]).is_empty(),
            "a seat with a price is not an unanswered question"
        );
    }

    #[test]
    fn a_metered_key_is_never_asked_for_a_monthly_price() {
        // An API key is billed per token; there is no plan behind it to price,
        // so asking would be noise on every run of a perfectly healthy gateway.
        assert!(check_seat_prices(&[seat("openai-key", "api_key", None)]).is_empty());
    }

    #[test]
    fn every_unpriced_seat_is_named_and_none_of_them_is_a_failure() {
        // Serving is unaffected by not knowing what a seat cost. Exiting
        // non-zero over a reporting gap would make `doctor` unusable in CI —
        // so the check names them all and the caller counts none of them.
        let seats = [seat("a", "oauth", None), seat("b", "oauth", None)];
        assert_eq!(
            check_seat_prices(&seats),
            vec!["a", "b"],
            "an operator fixing this needs every name, not the first one"
        );
    }

    #[test]
    fn a_seat_held_back_by_its_reserve_is_reported_as_a_live_failure() {
        // Doctor exists to answer "why would a request fail right now". A seat
        // parked by its own reserve is enabled, un-cooled and not rate limited,
        // so every other line in this report calls it healthy.
        assert_eq!(check_reserves(&[reserved("grok", 4, 10)]), 1);
        assert_eq!(
            check_reserves(&[reserved("grok", 10, 10)]),
            1,
            "at the line"
        );
    }

    #[test]
    fn a_seat_with_headroom_above_its_reserve_is_not_reported() {
        assert_eq!(check_reserves(&[reserved("grok", 45, 10)]), 0);
    }

    #[test]
    fn a_seat_nobody_has_polled_is_never_reported_as_held_back() {
        // NULL is unknown, not empty, and the scheduler will happily use this
        // seat — so calling it a failure would send an operator to lower a
        // reserve that is not stopping anything.
        let mut unread = seat("grok", "oauth", Some(300));
        unread.usage_reserve_pct = Some(10);
        assert_eq!(check_reserves(&[unread]), 0);
    }

    #[test]
    fn a_seat_with_no_reserve_is_never_reported_however_little_is_left() {
        // Today's behaviour, unchanged: without a reserve the seat is drained
        // to empty and the provider's 429 is what stops it.
        let mut spent = seat("grok", "oauth", Some(300));
        spent.usage_remaining_pct = Some(rust_decimal::Decimal::ZERO);
        assert_eq!(check_reserves(&[spent]), 0);
    }

    #[test]
    fn a_reserved_out_seat_is_not_counted_as_a_live_credential_for_a_rung() {
        // The ladder check asks each rung for a live credential. A seat the
        // scheduler will refuse must not answer that question, or doctor
        // reports a healthy ladder for requests that all fail.
        let rungs = vec![oag_router::ladder::Rung {
            name: oag_core::TierName::new("cheap"),
            models: vec![oag_router::ModelId::new("xai/grok-4.6")],
        }];
        assert_eq!(
            check_ladder(&rungs, &[reserved("grok", 2, 10)], "default"),
            1
        );
        assert_eq!(
            check_ladder(&rungs, &[reserved("grok", 80, 10)], "default"),
            0
        );
    }
    /// C10. An owner-bound seat does not cover a rung for everyone else.
    ///
    /// The scheduler filters candidates on `owner_principal_id`: a credential
    /// with an owner serves that principal's requests and nobody else's. Doctor
    /// never selected the column, so a personally bound seat answered "is there
    /// a live credential for this rung" on behalf of every principal on the
    /// route. The route reported `ok`, and the first contradiction anyone saw
    /// was `no_viable_model` on a route the CLI had just called healthy.
    #[test]
    fn an_owner_bound_seat_does_not_cover_a_rung() {
        let rungs = vec![oag_router::ladder::Rung {
            name: oag_core::TierName::new("cheap"),
            models: vec![oag_router::ModelId::new("xai/grok-4.6")],
        }];

        // Live by every other measure — schedulable, no cooldown, no rate
        // limit, no reserve — and reachable by exactly one principal.
        let bound = owner_bound("grok-personal");
        assert!(
            bound.live(time::OffsetDateTime::now_utc()),
            "the fixture has to be live, or this would pass for the wrong reason"
        );
        assert_eq!(
            check_ladder(&rungs, &[bound], "default"),
            1,
            "a rung whose only credential answers one principal is not covered"
        );

        // A shared credential beside it covers the rung for everyone.
        assert_eq!(
            check_ladder(
                &rungs,
                &[
                    owner_bound("grok-personal"),
                    seat("grok-team", "oauth", Some(300))
                ],
                "default"
            ),
            0
        );
    }
    /// C16. A gap or a failed row is not a healthy schema.
    ///
    /// `check_migrations` compared `applied.len()` against
    /// `EXPECTED_MIGRATIONS`, which answers a weaker question than the one
    /// being asked. A schema holding 1..13 and 15 — what a hand-patched
    /// `_sqlx_migrations` leaves behind — counted fourteen and passed while the
    /// table migration 14 was supposed to alter did not exist. So did one whose
    /// last row is `success = false`, which is the state sqlx leaves after a
    /// migration that failed halfway: the row is there and the DDL is not.
    #[tokio::test]
    async fn a_gap_or_a_failed_migration_is_not_a_healthy_schema() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        // The healthy case, which is what the test database is in.
        assert_eq!(check_migrations(&db).await.expect("check"), 0);

        // A gap: hide the newest version, then put it back. The count is
        // restored to `EXPECTED_MIGRATIONS` by adding a version that does not
        // belong, which is what makes counting insufficient.
        let newest = i64::try_from(EXPECTED_MIGRATIONS).expect("fits");
        let row: (Vec<u8>, i64, bool) = sqlx::query_as(
            "DELETE FROM _sqlx_migrations WHERE version = $1
             RETURNING checksum, execution_time, success",
        )
        .bind(newest)
        .fetch_one(db.pool())
        .await
        .expect("take the newest row out");
        sqlx::query(
            "INSERT INTO _sqlx_migrations
                 (version, description, installed_on, success, checksum, execution_time)
             VALUES ($1, 'not a real migration', now(), true, $2, $3)",
        )
        .bind(newest + 1)
        .bind(&row.0)
        .bind(row.1)
        .execute(db.pool())
        .await
        .expect("put a row back that does not close the gap");

        let with_gap = check_migrations(&db).await.expect("check");

        // Restore before asserting, so a failure cannot leave the database
        // one migration short for every later test.
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
            .bind(newest + 1)
            .execute(db.pool())
            .await
            .expect("cleanup");
        sqlx::query(
            "INSERT INTO _sqlx_migrations
                 (version, description, installed_on, success, checksum, execution_time)
             VALUES ($1, 'restored', now(), $4, $2, $3)",
        )
        .bind(newest)
        .bind(&row.0)
        .bind(row.1)
        .bind(row.2)
        .execute(db.pool())
        .await
        .expect("restore");

        assert_eq!(
            with_gap, 1,
            "the row count was right and the schema was not: counting rows \
             cannot see which versions they are"
        );
        assert_eq!(
            check_migrations(&db).await.expect("check"),
            0,
            "and the restore really restored it"
        );
    }
}
