//! Codex (`ChatGPT` subscription) remaining-quota reading.
//!
//! Contract ported from openusage's
//! `Sources/OpenUsage/Providers/Codex/{CodexUsageClient,CodexUsageMapper}.swift`
//! (MIT), which reads the same endpoint the Codex clients do and carries its
//! shape in tests taken from live responses.
//!
//! ## What is verified, and what is not
//!
//! **Observed live** (a `free` plan at its limit, 2026-08-25, through this very
//! poller): the URL and that a bearer token plus `chatgpt-account-id` is the
//! whole of the auth; `rate_limit` carrying `allowed`, `limit_reached`,
//! `primary_window` and a null `secondary_window`; and a window carrying
//! `used_percent`, `limit_window_seconds`, `reset_at` (unix seconds) and
//! `reset_after_seconds`. That response's window was 2592000 seconds — a
//! thirty-day pool — which is why a monthly period is one of the named ones.
//!
//! **Not observed here, taken from the openusage port**: the two-pool shape an
//! entitled plan reports, and with it the 18000-second (five-hour session) and
//! 604800-second (weekly) durations that name those pools. Nobody has run this
//! against a Plus or Pro seat, so that half is a faithful port rather than a
//! measurement, and it is flagged as such rather than presented as fact.
//!
//! Because of that split, everything is optional and every absent field means
//! *unknown* rather than zero: a body carrying no percentage and no exhaustion
//! flag yields `None`, and the poller then leaves the account's usage columns
//! NULL. The asymmetry is deliberate — a fabricated 0% would bench a working
//! seat until its imagined window reset, and a fabricated 100% would hide an
//! exhausted one behind a full-looking bar. A blank is the only honest answer
//! to a body we did not understand.
//!
//! Fields present in the response that this module ignores on purpose:
//! `additional_rate_limits` (per-model limits such as Spark — exhausting one
//! does not exhaust the seat, so it must not bench it), `credits`,
//! `rate_limit_reset_credits`, `rate_limit_upsell`, `plan_type`, and the
//! identifying `user_id` / `email` / `account_id`. None of them answers "how
//! much of this seat's allowance is left", which is the only question
//! [`UsageSnapshot`] exists to answer — and the last three are personal data
//! with no reason to enter the gateway's memory.

use super::UsageSnapshot;
use oag_core::credential::SecretMaterial;
use oag_core::{Error, Result};
use serde::Deserialize;

/// The endpoint the Codex clients read their own rate-limit meters from.
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// The five-hour session pool, in seconds. From the openusage port.
const SESSION_WINDOW_SECS: f64 = 18_000.0;
/// The weekly pool. From the openusage port.
const WEEKLY_WINDOW_SECS: f64 = 604_800.0;
/// The thirty-day pool a free plan is metered on. Observed live.
const MONTHLY_WINDOW_SECS: f64 = 2_592_000.0;

pub async fn fetch(credential: &SecretMaterial) -> Result<Option<UsageSnapshot>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Internal(format!("building usage client: {e}")))?;

    let mut request = client
        .get(USAGE_URL)
        .header(
            "authorization",
            format!("Bearer {}", credential.access_token.trim()),
        )
        .header("accept", "application/json")
        // The same self-identification the inference path sends. Not required
        // by this endpoint as far as the port shows — a bearer token and the
        // account header are — but a gateway that names itself one way when it
        // spends the quota and another way when it reads it is asking to be
        // told apart for no benefit.
        .header("originator", crate::codex::DEFAULT_ORIGINATOR)
        .header(
            "user-agent",
            concat!("codex_cli_rs/oag-", env!("CARGO_PKG_VERSION")),
        );

    // Account-scoped, exactly as on the inference path: the header binds the
    // read to the seat the token belongs to. Absent rather than empty when the
    // credential carries no account id.
    if let Some(account_id) = &credential.account_id {
        request = request.header("chatgpt-account-id", account_id.as_str());
    }

    let response = request
        .send()
        .await
        .map_err(|e| Error::Internal(format!("codex usage request: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::Internal(format!(
            "codex usage returned {}",
            response.status()
        )));
    }

    let body: Response = response
        .json()
        .await
        .map_err(|e| Error::Internal(format!("codex usage body: {e}")))?;
    Ok(parse(body, now_unix()))
}

#[derive(Debug, Deserialize)]
struct Response {
    rate_limit: Option<RateLimit>,
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    /// The backend's own verdict on whether the seat may serve right now.
    /// Observed live as `false` on a spent plan.
    allowed: Option<bool>,
    /// Its companion, observed live as `true` on the same response.
    limit_reached: Option<bool>,
    /// Normally the five-hour session pool — but the backend is known to move a
    /// temporarily sole weekly limit into this slot, and a free plan puts its
    /// thirty-day pool here, so the slot is a fallback for naming a window and
    /// never the authority. `limit_window_seconds` is.
    primary_window: Option<LimitWindow>,
    secondary_window: Option<LimitWindow>,
}

impl RateLimit {
    /// Whether the backend has said outright that this seat is spent.
    ///
    /// Only an explicit `false`/`true` counts. An absent flag is not a claim
    /// that the seat is fine — it just is not a claim that it is spent, and the
    /// percentages are left to answer on their own.
    fn exhausted(&self) -> bool {
        self.allowed == Some(false) || self.limit_reached == Some(true)
    }
}

#[derive(Debug, Deserialize)]
struct LimitWindow {
    /// 0..100 of the pool consumed. Absent means unknown — unlike Grok's
    /// proto-JSON, nothing here documents an omitted zero, so an absent
    /// percentage is not read as a full pool.
    used_percent: Option<f64>,
    /// The pool's period. What actually names the window.
    #[serde(rename = "limit_window_seconds")]
    period_seconds: Option<f64>,
    /// Unix seconds at which the pool refills.
    reset_at: Option<f64>,
    /// Seconds from now until it refills; the fallback when `reset_at` is
    /// absent, which is why parsing takes the current time as an argument.
    reset_after_seconds: Option<f64>,
}

impl LimitWindow {
    /// What to call this window on the dashboard, from its own stated period
    /// where there is one and from the slot it arrived in otherwise.
    fn label(&self, slot: &'static str) -> &'static str {
        match self.period_seconds {
            Some(secs) if (secs - SESSION_WINDOW_SECS).abs() < 1.0 => "5h",
            Some(secs) if (secs - WEEKLY_WINDOW_SECS).abs() < 1.0 => "weekly",
            Some(secs) if (secs - MONTHLY_WINDOW_SECS).abs() < 1.0 => "monthly",
            // A period none of the three names: say roughly how long it is
            // rather than asserting one of the pools we do know.
            Some(secs) if secs > 0.0 => {
                if secs < 86_400.0 {
                    "session"
                } else {
                    "rolling"
                }
            }
            _ => slot,
        }
    }

    fn resets_at(&self, now: i64) -> Option<i64> {
        if let Some(at) = self.reset_at {
            return whole_seconds(at);
        }
        Some(now + whole_seconds(self.reset_after_seconds?)?)
    }
}

/// The scarcer of the windows the backend reports, or `None` if it reported no
/// usable percentage at all.
///
/// Codex meters a seat on two pools at once — a five-hour session budget and a
/// weekly one — and a seat is unusable as soon as *either* is spent, so the
/// binding constraint is the smaller remaining figure. That is also what makes
/// the poller's benching correct: the reset it carries is the reset of the pool
/// that is actually blocking, so the seat comes back exactly when it can serve
/// again rather than at the far end of the weekly window.
///
/// This matches what migration 0005 already promises of the column: "the
/// scarcer of what the provider reports".
fn parse(body: Response, now: i64) -> Option<UsageSnapshot> {
    let rate_limit = body.rate_limit?;
    let windows = [
        (rate_limit.primary_window.as_ref(), "5h"),
        (rate_limit.secondary_window.as_ref(), "weekly"),
    ];

    let exhausted = rate_limit.exhausted();
    let mut scarcest: Option<UsageSnapshot> = None;
    for (window, slot) in windows {
        let Some(window) = window else { continue };
        // No percentage is no reading — unless the backend has already said the
        // seat is spent, in which case zero left is its statement, not our
        // guess. Otherwise an absent percentage would be read as an untouched
        // pool for a seat that may be empty.
        let Some(used) = window
            .used_percent
            .and_then(finite)
            .or_else(|| exhausted.then_some(100.0))
        else {
            continue;
        };
        let snapshot = UsageSnapshot {
            // `allowed: false` outranks any percentage beside it: the backend
            // is refusing the seat, so whatever the meter says, nothing can be
            // routed here until it resets.
            remaining_pct: if exhausted {
                0.0
            } else {
                100.0 - used.clamp(0.0, 100.0)
            },
            window_label: window.label(slot).to_owned(),
            resets_at: window.resets_at(now),
        };
        if scarcest
            .as_ref()
            .is_none_or(|s| snapshot.remaining_pct < s.remaining_pct)
        {
            scarcest = Some(snapshot);
        }
    }
    scarcest
}

/// JSON permits neither NaN nor infinity, but a `null` decodes to `None` and a
/// numeric field can still arrive as something unusable through a proxy that
/// rewrote the body. Anything not finite is dropped rather than propagated into
/// a percentage the scheduler acts on.
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

/// A JSON number of seconds as a whole count, or `None` if it is not one a
/// timestamp can hold.
///
/// The range check is the whole point of the function: the truncating cast
/// below is only defined once the value is known to fit, and a reset time that
/// wrapped would bench a seat until the year 292277026596 or un-bench an
/// exhausted one immediately.
#[allow(
    clippy::cast_possible_truncation,
    reason = "guarded by the range check on the line above"
)]
fn whole_seconds(value: f64) -> Option<i64> {
    let value = finite(value)?.trunc();
    // ±1e12 seconds is some thirty thousand years either side of the epoch:
    // wider than any reset a subscription will ever quote, and far enough from
    // `i64::MAX` that adding it to `now` cannot overflow.
    (-1e12..=1e12).contains(&value).then_some(value as i64)
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: &str) -> Response {
        serde_json::from_str(json).expect("json")
    }

    const NOW: i64 = 1_800_000_000;

    /// The response this poller actually received, 2026-08-25, from a `free`
    /// plan sitting at its limit. Trimmed of the identifying fields (`user_id`,
    /// `email`, `account_id`) — they are personal data and nothing reads them —
    /// but otherwise verbatim, including the keys this module ignores.
    const OBSERVED_FREE_PLAN_AT_LIMIT: &str = r#"{
      "plan_type": "free",
      "rate_limit": {
        "allowed": false,
        "limit_reached": true,
        "primary_window": {
          "used_percent": 100,
          "limit_window_seconds": 2592000,
          "reset_after_seconds": 1665902,
          "reset_at": 1789330460
        },
        "secondary_window": null
      },
      "code_review_rate_limit": null,
      "additional_rate_limits": null,
      "credits": { "has_credits": false, "unlimited": false, "balance": null },
      "spend_control": { "reached": false, "individual_limit": null },
      "rate_limit_reached_type": { "type": "rate_limit_reached", "details": "default" },
      "rate_limit_reset_credits": { "available_count": 0 }
    }"#;

    #[test]
    fn the_observed_response_reads_as_a_spent_monthly_pool() {
        let snap = parse(body(OBSERVED_FREE_PLAN_AT_LIMIT), NOW).expect("a reading");
        assert!((snap.remaining_pct - 0.0).abs() < 1e-9, "{snap:?}");
        // 2592000s is thirty days, not the five-hour or weekly pool an entitled
        // plan reports — naming it "5h" would tell an operator the seat frees
        // up this afternoon when it frees up in nineteen days.
        assert_eq!(snap.window_label, "monthly");
        assert_eq!(snap.resets_at, Some(1_789_330_460));
    }

    #[test]
    fn the_backends_own_refusal_outranks_a_percentage_beside_it() {
        // `allowed: false` means nothing can be routed here, whatever the meter
        // reads. Trusting the 40% would keep scheduling onto a refused seat.
        let snap = parse(
            body(
                r#"{"rate_limit":{"allowed":false,"primary_window":
                     {"used_percent":60,"limit_window_seconds":18000,"reset_at":1800009000}}}"#,
            ),
            NOW,
        )
        .expect("a reading");
        assert!((snap.remaining_pct - 0.0).abs() < 1e-9);
    }

    #[test]
    fn an_exhaustion_flag_gives_a_reading_even_with_no_percentage_to_read() {
        let snap = parse(
            body(
                r#"{"rate_limit":{"limit_reached":true,"primary_window":
                     {"limit_window_seconds":604800,"reset_at":1800400000}}}"#,
            ),
            NOW,
        )
        .expect("a reading");
        assert!((snap.remaining_pct - 0.0).abs() < 1e-9);
        assert_eq!(snap.window_label, "weekly");
    }

    #[test]
    fn an_absent_exhaustion_flag_is_not_a_claim_that_the_seat_is_fine() {
        // Only an explicit false/true counts; a body with neither flag nor
        // percentage is still no reading at all.
        assert!(
            parse(
                body(r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000}}}"#),
                NOW
            )
            .is_none()
        );
    }

    #[test]
    fn a_refusal_with_no_window_at_all_is_still_no_reading() {
        // Deliberate: with no window there is no reset, so a 0% would paint the
        // seat red on the dashboard without the poller being able to bench it
        // or say when it comes back. Better to leave the columns NULL.
        assert!(parse(body(r#"{"rate_limit":{"allowed":false}}"#), NOW).is_none());
    }

    /// The two-pool shape from the openusage port — not observed here.
    const BOTH_POOLS: &str = r#"{
        "plan_type": "pro",
        "rate_limit": {
          "primary_window":   { "used_percent": 12.5, "limit_window_seconds": 18000,
                                "reset_at": 1800009000, "reset_after_seconds": 9000 },
          "secondary_window": { "used_percent": 71.0,  "limit_window_seconds": 604800,
                                "reset_at": 1800400000 }
        },
        "credits": { "balance": 821 }
    }"#;

    #[test]
    fn the_scarcer_of_the_two_pools_is_the_one_reported() {
        // 71% of the weekly pool is gone against 12.5% of the session pool, so
        // the weekly one is what will stop this seat first.
        let snap = parse(body(BOTH_POOLS), NOW).expect("a reading");
        assert!((snap.remaining_pct - 29.0).abs() < 1e-9, "{snap:?}");
        assert_eq!(snap.window_label, "weekly");
        assert_eq!(
            snap.resets_at,
            Some(1_800_400_000),
            "and it carries the reset of the pool that is actually blocking"
        );
    }

    #[test]
    fn the_session_pool_wins_when_it_is_the_one_nearly_spent() {
        let snap = parse(
            body(
                r#"{"rate_limit":{
                     "primary_window":  {"used_percent": 96, "limit_window_seconds": 18000,
                                         "reset_at": 1800009000},
                     "secondary_window":{"used_percent": 30, "limit_window_seconds": 604800}}}"#,
            ),
            NOW,
        )
        .expect("a reading");
        assert!((snap.remaining_pct - 4.0).abs() < 1e-9);
        assert_eq!(snap.window_label, "5h");
        assert_eq!(snap.resets_at, Some(1_800_009_000));
    }

    #[test]
    fn a_window_is_named_by_its_stated_period_not_by_the_slot_it_arrived_in() {
        // The backend is known to move a sole weekly limit into the primary
        // slot; labelling that "5h" would tell an operator the seat frees up in
        // five hours when it frees up in a week.
        let snap = parse(
            body(
                r#"{"rate_limit":{"primary_window":
                     {"used_percent": 5, "limit_window_seconds": 604800}}}"#,
            ),
            NOW,
        )
        .expect("a reading");
        assert_eq!(snap.window_label, "weekly");
    }

    #[test]
    fn a_reset_offset_is_resolved_against_the_time_of_the_read() {
        let snap = parse(
            body(
                r#"{"rate_limit":{"primary_window":
                     {"used_percent": 5, "reset_after_seconds": 600}}}"#,
            ),
            NOW,
        )
        .expect("a reading");
        assert_eq!(snap.resets_at, Some(NOW + 600));
    }

    #[test]
    fn a_window_with_no_percentage_yields_no_reading_rather_than_a_full_pool() {
        // The bug this guards: reading an absent percentage as 0% used would
        // paint an exhausted seat as untouched and keep scheduling onto it.
        assert!(
            parse(
                body(r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000}}}"#),
                NOW,
            )
            .is_none()
        );
    }

    #[test]
    fn a_body_with_no_rate_limit_at_all_yields_no_reading() {
        assert!(parse(body(r#"{"plan_type":"pro"}"#), NOW).is_none());
        assert!(parse(body("{}"), NOW).is_none());
        assert!(parse(body(r#"{"rate_limit":null}"#), NOW).is_none());
        assert!(parse(body(r#"{"rate_limit":{}}"#), NOW).is_none());
    }

    #[test]
    fn an_unrecognised_body_is_no_reading_and_not_a_zero_one() {
        // A shape nobody here has seen. Unknown keys are ignored and the result
        // is a blank, which the poller leaves as NULL — the honest state.
        assert!(parse(body(r#"{"usage":{"weekly":{"percent":40}}}"#), NOW).is_none());
    }

    #[test]
    fn an_exhausted_pool_reads_as_zero_left_so_the_poller_can_bench_the_seat() {
        let snap = parse(
            body(
                r#"{"rate_limit":{"secondary_window":
                     {"used_percent": 100, "limit_window_seconds": 604800,
                      "reset_at": 1800400000}}}"#,
            ),
            NOW,
        )
        .expect("a reading");
        assert!((snap.remaining_pct - 0.0).abs() < 1e-9);
        assert_eq!(snap.resets_at, Some(1_800_400_000));
    }

    #[test]
    fn a_percentage_past_a_hundred_is_clamped_rather_than_going_negative() {
        let snap = parse(
            body(r#"{"rate_limit":{"primary_window":{"used_percent": 140}}}"#),
            NOW,
        )
        .expect("a reading");
        assert!((snap.remaining_pct - 0.0).abs() < 1e-9);
    }

    #[test]
    fn one_readable_window_beside_one_unreadable_still_gives_a_reading() {
        let snap = parse(
            body(
                r#"{"rate_limit":{
                     "primary_window":  {"limit_window_seconds": 18000},
                     "secondary_window":{"used_percent": 60, "limit_window_seconds": 604800}}}"#,
            ),
            NOW,
        )
        .expect("a reading");
        assert!((snap.remaining_pct - 40.0).abs() < 1e-9);
        assert_eq!(snap.window_label, "weekly");
    }
}
