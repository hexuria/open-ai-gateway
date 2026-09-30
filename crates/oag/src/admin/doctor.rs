//! `oag admin doctor` — why a request on this route would fail.

use oag_core::config::Config;
use oag_core::{Provider, Result};
use oag_store::Db;

/// Bumped with 0008. 0007 added `account.usage_reserve_pct`, which every
/// credential query now selects, so a binary on the old schema selects no
/// candidate at all — a failure that reads as an empty pool unless something
/// says the schema is behind, which is this. 0008 added `usage_event.origin`,
/// which the reporting queries group on and the importer writes. 0017 re-keys
/// the rows a Claude Code import already wrote, so a binary on the old schema
/// re-imports the whole corpus the first time anybody runs `usage import`.
/// 0020 adds the `endpoint` table that `repo::endpoints` reads and writes.
///
/// Raise this with every migration that existing queries depend on. The test
/// below counts the files rather than trusting this line, because the number
/// that matters is the one on disk and a constant is exactly the thing that
/// gets forgotten.
const EXPECTED_MIGRATIONS: usize = 20;

pub async fn run(db: &Db, config: &Config, route: &str) -> Result<()> {
    conclude(problems(db, config, route).await?)
}

/// How many problems `run` reports, for a test to count.
async fn problems(db: &Db, config: &Config, route: &str) -> Result<u32> {
    let mut failed = 0u32;

    failed += check_migrations(db).await?;
    failed += check_catalog(db).await?;
    let Some((_mode, rungs)) = check_route(db, route).await? else {
        return Ok(failed + 1);
    };
    let accounts = load_accounts(db, route).await?;
    failed += route_problems(&rungs, &accounts, route);
    // Named, never counted: several keys on one seat owner are fine when they
    // are all that person's, and only the operator knows.
    let _many_keys = check_seat_owner_keys(db).await?;
    failed += check_codex(config, &accounts);
    Ok(failed)
}

/// Everything doctor can say about a route from its ladder and credentials
/// alone, as a count of problems. Split from `run` so the arithmetic is
/// checkable without a database.
fn route_problems(rungs: &[oag_router::ladder::Rung], accounts: &[Seat], route: &str) -> u32 {
    let mut failed = report_accounts(accounts);
    failed += check_ladder(rungs, accounts, route);
    // Named, never counted: an unpriced seat is a reporting gap and the gateway
    // serves perfectly well without the figure. Exiting non-zero over it would
    // make `doctor` unusable in CI.
    let _unpriced = check_seat_prices(accounts);
    failed += check_orphaned_seats(accounts);
    failed += check_reserves(accounts);
    failed
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
    ///
    /// An owner-less subscription seat answers no one at all — the scheduler
    /// matches it for nobody until `account set-owner` binds it — so only an
    /// owner-less key that is not a seat is shared.
    fn shared(&self) -> bool {
        self.owner_principal_id.is_none() && self.kind != "oauth"
    }

    /// A subscription seat with no owner: inert, and reported as such.
    fn orphaned_seat(&self) -> bool {
        self.owner_principal_id.is_none() && self.kind == "oauth"
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

/// Live credentials for `provider` that serve exactly one principal: the
/// reason a rung can read "no credential" while `account list` shows one
/// ready. An owner-less seat is not one of them — it serves no one, and
/// [`check_orphaned_seats`] says so.
fn bound_elsewhere<'a>(
    accounts: &'a [Seat],
    provider: &str,
    now: time::OffsetDateTime,
) -> Vec<&'a str> {
    accounts
        .iter()
        .filter(|a| a.provider == provider && a.live(now) && !a.shared() && !a.orphaned_seat())
        .map(|a| a.name.as_str())
        .collect()
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
                let bound = bound_elsewhere(accounts, p, now);
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

/// A subscription seat nobody owns, which therefore serves nobody.
///
/// Reported as a failure: an older version let `--shared` pool a personal
/// plan, and the request path now matches such a seat for no one, so it is
/// silently out of the pool until an owner is named.
fn check_orphaned_seats(accounts: &[Seat]) -> u32 {
    let orphaned: Vec<&str> = accounts
        .iter()
        .filter(|a| a.orphaned_seat())
        .map(|a| a.name.as_str())
        .collect();
    let Some(first) = orphaned.first() else {
        return 0;
    };
    println!(
        "FAIL seats       {} has no owner, so serves no one: a subscription seat \
         belongs to one person",
        orphaned.join(", ")
    );
    println!("     fix: oag admin account set-owner {first} --owner-email <its owner>");
    u32::try_from(orphaned.len()).unwrap_or(u32::MAX)
}

/// Seat owners holding more than one live inference key.
pub(super) async fn check_seat_owner_keys(
    db: &Db,
) -> Result<Vec<oag_store::repo::SeatWithManyKeys>> {
    let many = oag_store::repo::seats_with_many_keys(db, None).await?;
    for s in &many {
        println!("{}", many_keys_warning(s));
    }
    Ok(many)
}

/// The one wording for "this seat's owner has several keys", shared with
/// `key create`, so the operator reads the same sentence wherever it fires.
pub(super) fn many_keys_warning(s: &oag_store::repo::SeatWithManyKeys) -> String {
    format!(
        "WARN seats       {} belongs to {}, who holds {} live keys. Fine if they are \
         all theirs; a key given to anyone else shares the seat, which its terms \
         forbid.",
        s.seat, s.owner_email, s.keys
    )
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

    /// Held by every test here that reads `_sqlx_migrations` while another
    /// writes it: the gap test plants a bogus row for a moment, and a check
    /// that ran in that moment would count it.
    static SCHEMA_ROWS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        // A pooled credential carrying the reading, because only an
        // owner-less non-seat covers a rung for everyone; the reserve rule
        // itself does not look at the kind.
        let pooled = |remaining| {
            let mut s = reserved("grok", remaining, 10);
            s.kind = "api_key".to_owned();
            s
        };
        assert_eq!(check_ladder(&rungs, &[pooled(2)], "default"), 1);
        assert_eq!(check_ladder(&rungs, &[pooled(80)], "default"), 0);
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

        // A shared credential beside it covers the rung for everyone. It is
        // an API key: a seat is never shared, and an owner-less one covers
        // nothing (below).
        assert_eq!(
            check_ladder(
                &rungs,
                &[
                    owner_bound("grok-personal"),
                    seat("grok-team", "api_key", None)
                ],
                "default"
            ),
            0
        );
    }

    /// A route's problems add up: an owner-less seat beside a live shared key
    /// is one problem and nothing else, and binding it clears the count.
    #[test]
    fn an_ownerless_seat_is_one_route_problem_and_binding_it_clears_it() {
        let rungs = vec![oag_router::ladder::Rung {
            name: oag_core::TierName::new("cheap"),
            models: vec![oag_router::ModelId::new("xai/grok-4.6")],
        }];
        let key = || seat("pooled-key", "api_key", None);
        assert_eq!(route_problems(&rungs, &[key()], "default"), 0);
        assert_eq!(
            route_problems(
                &rungs,
                &[key(), seat("grok-pooled", "oauth", Some(300))],
                "default"
            ),
            1
        );
        assert_eq!(
            route_problems(&rungs, &[key(), owner_bound("grok-personal")], "default"),
            0
        );
        assert_eq!(check_orphaned_seats(&[key(), owner_bound("mine")]), 0);

        // The other checks add too: a rung only a personal seat covers is one
        // problem, and a pooled key held out by its reserve is two (the rung,
        // and the reserve).
        assert_eq!(route_problems(&rungs, &[owner_bound("mine")], "default"), 1);
        let mut held = key();
        held.usage_remaining_pct = Some(rust_decimal::Decimal::from(2));
        held.usage_reserve_pct = Some(10);
        assert_eq!(route_problems(&rungs, &[held], "default"), 2);
    }

    /// `run`'s total is the schema checks plus the route's own problems: a
    /// route with a live shared key adds nothing, and the same ladder with no
    /// credential adds two (no accounts, and an uncovered rung).
    #[tokio::test]
    async fn the_route_problems_add_to_the_total() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");
        let config = oag_core::config::Config::from_yaml(&format!(
            "database:\n  url: \"{url}\"\nredis:\n  url: \"redis://127.0.0.1:1\"\n\
             security:\n  signing_secret: \"Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0=\"\n  \
             credential_kek: \"MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=\"\n"
        ))
        .expect("config");
        let route = |name: String| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, uuid::Uuid>(
                    "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, \
                     '[{\"name\":\"cheap\",\"models\":[\"xai/grok-4.6\"]}]'::jsonb) RETURNING id",
                )
                .bind(&name)
                .fetch_one(db.pool())
                .await
                .expect("route");
                name
            }
        };
        let good = route(format!("good-{}", uuid::Uuid::new_v4())).await;
        let bad = route(format!("bad-{}", uuid::Uuid::new_v4())).await;
        let key: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
             VALUES (gen_random_uuid(), $1, 'xai', 'api_key', '\\x00', '\\x00') RETURNING id",
        )
        .bind(format!("key-{}", uuid::Uuid::new_v4()))
        .fetch_one(db.pool())
        .await
        .expect("key");
        sqlx::query(
            "INSERT INTO account_route (account_id, route_id) SELECT $1, id FROM route WHERE name = $2",
        )
        .bind(key)
        .bind(&good)
        .execute(db.pool())
        .await
        .expect("attach");

        let _rows = SCHEMA_ROWS.lock().await;
        let schema = check_migrations(&db).await.expect("m") + check_catalog(&db).await.expect("c");
        assert_eq!(problems(&db, &config, &good).await.expect("good"), schema);
        assert_eq!(problems(&db, &config, &bad).await.expect("bad"), schema + 2);
    }

    /// The note on a failed rung names only credentials that serve one
    /// principal: not a shared key, not an owner-less seat, not another
    /// provider's, and not one that is out of rotation.
    #[test]
    fn the_bound_note_names_only_live_personal_credentials_of_that_provider() {
        let now = time::OffsetDateTime::now_utc();
        let mut elsewhere = owner_bound("other-provider");
        elsewhere.provider = "openai".to_owned();
        let mut off = owner_bound("disabled");
        off.schedulable = false;
        let accounts = [
            owner_bound("mine"),
            seat("pooled-key", "api_key", None),
            seat("grok-pooled", "oauth", Some(300)),
            elsewhere,
            off,
        ];
        assert_eq!(bound_elsewhere(&accounts, "xai", now), vec!["mine"]);
    }

    /// Migration 0019: a seat with no owner, pooled by an older version's
    /// `--shared`, serves no one. It covers no rung, is not described as
    /// "bound to one principal", and is reported with the command that binds
    /// it.
    #[test]
    fn an_ownerless_seat_covers_nothing_and_is_reported() {
        let rungs = vec![oag_router::ladder::Rung {
            name: oag_core::TierName::new("cheap"),
            models: vec![oag_router::ModelId::new("xai/grok-4.6")],
        }];
        let orphan = seat("grok-pooled", "oauth", Some(300));
        assert!(orphan.live(time::OffsetDateTime::now_utc()));
        assert!(orphan.orphaned_seat() && !orphan.shared());
        assert_eq!(check_ladder(&rungs, &[orphan], "default"), 1);
        assert_eq!(
            check_orphaned_seats(&[
                seat("grok-pooled", "oauth", Some(300)),
                seat("pooled-key", "api_key", None),
                owner_bound("grok-personal"),
            ]),
            1,
            "only the owner-less seat is a problem; an owner-less key is the pool"
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
        let _rows = SCHEMA_ROWS.lock().await;

        // The healthy case, which is what the test database is in.
        assert_eq!(check_migrations(&db).await.expect("check"), 0);

        // Everything below leaves `_sqlx_migrations` describing a schema that
        // is not the one present. `cargo test --workspace` runs several
        // crates' gated tests against ONE database, and every one of them
        // calls `db.migrate()`; a migrate landing inside this window sees the
        // newest version as unapplied, re-applies it, and then this test's
        // restore collides on the primary key — or sees `success = false` and
        // aborts with "partially applied". Both were observed on CI, on `main`,
        // as two different-looking failures with this single cause.
        //
        // So hold the lock `Db::migrate` holds, for the width of the window.
        // A dedicated connection rather than a pooled one: an advisory lock
        // lives on its session, and a pooled connection handed back to the
        // pool while still holding it would leak the lock into whatever query
        // ran next. Dropping this closes the session, so a panic mid-window
        // releases it too.
        let mut guard = <sqlx::PgConnection as sqlx::Connection>::connect(&url)
            .await
            .expect("a connection of our own to hold the migration lock on");
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(oag_store::MIGRATION_LOCK_ID)
            .execute(&mut guard)
            .await
            .expect("take the migration lock");

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

        // A migration that ran and FAILED. C16 asks for versions `1..=N` and
        // every one of them successful; the gap above only exercises the first
        // half, so reverting the `success` filter left this test green. A
        // half-applied schema is the worse of the two states: the rows are all
        // present and the tables are not.
        sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = $1")
            .bind(newest)
            .execute(db.pool())
            .await
            .expect("mark the newest as failed");
        let with_failure = check_migrations(&db).await.expect("check");
        sqlx::query("UPDATE _sqlx_migrations SET success = true WHERE version = $1")
            .bind(newest)
            .execute(db.pool())
            .await
            .expect("restore");

        // Window closed: the table describes the schema again, so a
        // concurrent migrate is safe. Released before the assertions, so a
        // failure here cannot hold the lock while the rest of the suite waits.
        drop(guard);

        assert_eq!(
            with_failure, 1,
            "every version was present and one of them had not succeeded: \
             counting versions cannot see that either"
        );
        assert_eq!(
            check_migrations(&db).await.expect("check"),
            0,
            "and this restore restored it too"
        );
    }
}
