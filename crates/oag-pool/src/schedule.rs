//! The filter cascade: given every credential that could serve a request,
//! pick one.
//!
//! Ported from sub2api's scheduler, which arrived at these stages one incident
//! at a time. Each is a separate, independently justified filter rather than a
//! single scoring function, because a weighted score makes it impossible to
//! answer "why did this request go there" — and that is the question you have
//! at 3am.
//!
//! The whole thing is a pure function of a snapshot plus a clock reading, so
//! every stage is testable without Redis, Postgres, or a real credential.

use oag_core::{AccountId, Provider};
use rust_decimal::Decimal;

/// Whether a credential has fallen to or below the reserve an operator set on
/// it — the slice of a subscription's allowance the gateway is told to leave
/// alone rather than spend down to a 429.
///
/// NULL on either side is "no", and the two NULLs mean different things. An
/// unset reserve is every fleet that predates the column, so it has to schedule
/// exactly as it did. An unread `remaining` is a seat whose provider has no
/// usage endpoint, or one the poller has not reached yet: unknown, not empty.
/// Benching a working credential because nobody has measured it would take out
/// the whole pool the first time polling broke — a far worse failure than
/// draining one seat, which is all this exists to prevent.
///
/// A free function rather than a method because both liveness filters need it
/// and so does the message the caller builds when nothing is left, and a rule
/// with this much reasoning behind it must not be written down three times.
#[must_use]
pub fn held_by_reserve(remaining_pct: Option<Decimal>, reserve_pct: Option<Decimal>) -> bool {
    matches!((remaining_pct, reserve_pct), (Some(left), Some(floor)) if left <= floor)
}

/// One credential, as the scheduler sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub account: AccountId,
    pub provider: Provider,
    /// Lower is preferred. Credentials in the same tier compete on load.
    pub priority: u8,
    pub max_concurrency: u32,
    /// Requests currently in flight against this credential.
    pub in_flight: u32,
    /// Requests queued waiting for a slot.
    pub waiting: u32,
    /// Operator switch. A disabled credential is never chosen.
    pub schedulable: bool,
    /// Unix seconds until which this credential is cooling down after errors.
    pub cooldown_until: Option<i64>,
    /// Unix seconds until which the provider says we are rate limited.
    pub rate_limited_until: Option<i64>,
    /// Unix seconds when this credential's usage window resets. Drives the
    /// "use it or lose it" stage.
    pub window_resets_at: Option<i64>,
    /// How much of the subscription's allowance the provider says is left,
    /// 0..100. `None` is unknown — no usage endpoint, or not polled yet.
    pub usage_remaining_pct: Option<Decimal>,
    /// The floor an operator set under that allowance, 0..100. `None` is no
    /// reserve, which is how every credential behaved before the column existed.
    pub usage_reserve_pct: Option<Decimal>,
    /// Unix seconds of last use. Drives LRU.
    pub last_used_at: i64,
}

impl Candidate {
    /// Whether this credential can take a request right now.
    ///
    /// The reserve sits here beside the cooldown and the rate limit rather than
    /// anywhere upstream of them, so it composes with them for nothing: a seat
    /// held back by its reserve simply is not eligible, and every path that
    /// already copes with an ineligible credential — the cascade, the sticky
    /// pin falling through, failover to the next credential — copes with this
    /// one too without knowing the reserve exists.
    fn eligible(&self, now: i64) -> bool {
        self.schedulable
            && self.cooldown_until.is_none_or(|t| t <= now)
            && self.rate_limited_until.is_none_or(|t| t <= now)
            && !held_by_reserve(self.usage_remaining_pct, self.usage_reserve_pct)
            && self.in_flight < self.max_concurrency
    }

    /// Occupancy as a percentage, including queued requests.
    ///
    /// Integer basis points rather than a float so ordering is exact and two
    /// equally-loaded credentials compare equal instead of nearly-equal.
    fn load_bp(&self) -> u32 {
        if self.max_concurrency == 0 {
            return u32::MAX;
        }
        let busy = self.in_flight.saturating_add(self.waiting);
        busy.saturating_mul(10_000) / self.max_concurrency
    }
}

/// Which credential won, and which stage decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub account: AccountId,
    pub stage: Stage,
}

/// The cascade stage that produced the winner. Recorded for observability:
/// a fleet where every choice is decided at `SoonestReset` is a fleet whose
/// credentials are all saturated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// One eligible credential; no contest.
    OnlyCandidate,
    /// Decided on load.
    LeastLoaded,
    /// Decided on which usage window expires soonest.
    SoonestReset,
    /// Decided on least-recently-used.
    LeastRecentlyUsed,
}

/// Run the cascade.
///
/// Returns `None` when nothing is eligible — the caller's cue to report that
/// the pool is exhausted rather than to wait indefinitely.
///
/// `tie_breaker` is threaded in rather than drawn from an RNG so the function
/// stays pure and the tests stay deterministic. In production it is a random
/// per-request value: without it, every replica reading the same snapshot picks
/// the same credential at the same instant and stampedes it.
#[must_use]
pub fn select(candidates: &[Candidate], now: i64, tie_breaker: u64) -> Option<Selection> {
    // Stage 0 — eligibility.
    let eligible: Vec<&Candidate> = candidates.iter().filter(|c| c.eligible(now)).collect();
    let (first, rest) = eligible.split_first()?;
    if rest.is_empty() {
        return Some(Selection {
            account: first.account,
            stage: Stage::OnlyCandidate,
        });
    }

    // Stage 1 — priority tier. Lower wins outright; a tier-2 credential is
    // never chosen while any tier-1 credential can serve. This is what makes
    // "prefer our own keys, fall back to the shared pool" expressible.
    let best_priority = eligible.iter().map(|c| c.priority).min()?;
    let tier: Vec<&Candidate> = eligible
        .into_iter()
        .filter(|c| c.priority == best_priority)
        .collect();

    // Stage 2 — least loaded. LLM requests vary enormously in duration, so
    // occupancy is a far better signal than round-robin, for the same reason
    // the load balancer in front of us uses least-request.
    let min_load = tier.iter().map(|c| c.load_bp()).min()?;
    let least_loaded: Vec<&Candidate> = tier
        .into_iter()
        .filter(|c| c.load_bp() == min_load)
        .collect();
    if let [only] = least_loaded.as_slice() {
        return Some(Selection {
            account: only.account,
            stage: Stage::LeastLoaded,
        });
    }

    // Stage 3 — use it or lose it. Among equally-loaded credentials, prefer the
    // one whose quota window resets soonest: its unused capacity is about to
    // evaporate, while the others keep theirs. Credentials with no window are
    // never preferred here, so a metered subscription is drained before an
    // unmetered key.
    // Only a window still ahead of us counts. `window_resets_at` is written by
    // the usage poller and never cleared once the reset passes, so a seat that
    // reset an hour ago still carries the timestamp — and without this filter
    // it won this stage forever, as the "soonest" reset, while looking on the
    // dashboard like the stage doing exactly its job.
    let soonest = least_loaded
        .iter()
        .filter_map(|c| c.window_resets_at.filter(|t| *t > now))
        .min();
    let pool: Vec<&Candidate> = match soonest {
        Some(t) => {
            let expiring: Vec<&Candidate> = least_loaded
                .iter()
                .copied()
                .filter(|c| c.window_resets_at == Some(t))
                .collect();
            if let [only] = expiring.as_slice() {
                return Some(Selection {
                    account: only.account,
                    stage: Stage::SoonestReset,
                });
            }
            expiring
        }
        None => least_loaded,
    };

    // Stage 4 — least recently used, with a random tie-break so concurrent
    // requests reading an identical snapshot spread out instead of stampeding.
    let oldest = pool.iter().map(|c| c.last_used_at).min()?;
    let coldest: Vec<&Candidate> = pool
        .into_iter()
        .filter(|c| c.last_used_at == oldest)
        .collect();
    let idx = usize::try_from(tie_breaker % coldest.len() as u64).unwrap_or(0);
    coldest.get(idx).map(|c| Selection {
        account: c.account,
        stage: Stage::LeastRecentlyUsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn candidate(priority: u8) -> Candidate {
        Candidate {
            account: AccountId::new(),
            provider: Provider::Anthropic,
            priority,
            max_concurrency: 10,
            in_flight: 0,
            waiting: 0,
            schedulable: true,
            cooldown_until: None,
            rate_limited_until: None,
            window_resets_at: None,
            usage_remaining_pct: None,
            usage_reserve_pct: None,
            last_used_at: NOW - 100,
        }
    }

    /// A seat with a reading and a floor under it, both in whole percent.
    fn seat(remaining: i64, reserve: i64) -> Candidate {
        let mut c = candidate(0);
        c.usage_remaining_pct = Some(Decimal::from(remaining));
        c.usage_reserve_pct = Some(Decimal::from(reserve));
        c
    }

    #[test]
    fn an_empty_pool_selects_nothing() {
        assert!(select(&[], NOW, 0).is_none());
    }

    #[test]
    fn disabled_credentials_are_never_chosen() {
        let mut c = candidate(0);
        c.schedulable = false;
        assert!(select(&[c], NOW, 0).is_none());
    }

    #[test]
    fn cooldown_excludes_until_it_expires_then_recovers() {
        let mut c = candidate(0);
        c.cooldown_until = Some(NOW + 60);
        assert!(select(std::slice::from_ref(&c), NOW, 0).is_none());
        // Recovery is implicit: no sweeper, no state transition, just a clock
        // comparison. Nothing can leave a credential stuck out of the pool.
        assert!(select(&[c], NOW + 61, 0).is_some());
    }

    #[test]
    fn rate_limited_credentials_are_excluded_until_reset() {
        let mut c = candidate(0);
        c.rate_limited_until = Some(NOW + 30);
        assert!(select(std::slice::from_ref(&c), NOW, 0).is_none());
        assert!(select(&[c], NOW + 31, 0).is_some());
    }

    #[test]
    fn a_seat_still_above_its_reserve_is_a_candidate() {
        // The reserve is a floor, not a ceiling. A seat with headroom left
        // above it has to keep serving, or setting one would take the seat out
        // of the pool the moment it was set.
        assert!(select(&[seat(20, 10)], NOW, 0).is_some());
    }

    #[test]
    fn a_seat_at_or_below_its_reserve_is_not_scheduled() {
        // The whole feature: stop at the line rather than at the provider's
        // 429, so what is left is there when the window is nearly up and
        // somebody needs it.
        assert!(select(&[seat(10, 10)], NOW, 0).is_none(), "at the line");
        assert!(select(&[seat(4, 10)], NOW, 0).is_none(), "below it");
    }

    #[test]
    fn a_seat_nobody_has_measured_is_never_treated_as_exhausted() {
        // NULL is unknown, not empty. A provider with no usage endpoint, or a
        // poller that has been down since boot, leaves every reading NULL — and
        // benching the fleet over an absence of information would be a far
        // bigger outage than the one the reserve prevents.
        let mut unread = candidate(0);
        unread.usage_reserve_pct = Some(Decimal::from(10));
        assert!(select(&[unread], NOW, 0).is_some());
    }

    #[test]
    fn a_credential_with_no_reserve_schedules_exactly_as_it_always_did() {
        // Every fleet that predates the column has NULL here, down to a seat
        // reading zero percent — which the poller already benches by its own
        // route, and which this filter must not start second-guessing.
        let mut spent = candidate(0);
        spent.usage_remaining_pct = Some(Decimal::ZERO);
        assert!(select(&[spent], NOW, 0).is_some());
    }

    #[test]
    fn a_reserved_out_seat_loses_to_one_with_headroom_rather_than_failing_the_request() {
        // Failing over is not re-implemented for the reserve: an ineligible
        // candidate is one the cascade already skips, so the request lands on
        // the other seat with nothing extra written to make it.
        let live = seat(80, 10);
        let picked = select(&[seat(5, 10), live.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.account, live.account);
    }

    #[test]
    fn a_fractional_reading_is_compared_exactly() {
        // `usage_remaining_pct` is numeric(5,2). Rounding 10.01 down to the
        // reserve would park a seat that still has room, and rounding 9.99 up
        // would spend past the line — so the comparison stays in decimal.
        let mut just_above = candidate(0);
        just_above.usage_remaining_pct = Some(Decimal::new(1001, 2));
        just_above.usage_reserve_pct = Some(Decimal::from(10));
        assert!(select(&[just_above], NOW, 0).is_some());

        let mut just_below = candidate(0);
        just_below.usage_remaining_pct = Some(Decimal::new(999, 2));
        just_below.usage_reserve_pct = Some(Decimal::from(10));
        assert!(select(&[just_below], NOW, 0).is_none());
    }

    #[test]
    fn a_saturated_credential_is_skipped() {
        let mut full = candidate(0);
        full.in_flight = full.max_concurrency;
        let free = candidate(0);
        let picked = select(&[full, free.clone()], NOW, 0).expect("free one is eligible");
        assert_eq!(picked.account, free.account);
    }

    #[test]
    fn lower_priority_tier_wins_outright() {
        // This is what makes "prefer our own keys, fall back to the shared
        // pool" expressible: a tier-1 credential is never used while a tier-0
        // one can serve, no matter how much less loaded tier 1 is.
        let mut preferred = candidate(0);
        preferred.in_flight = 9;
        let fallback = candidate(1);
        let picked = select(&[fallback, preferred.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.account, preferred.account);
    }

    #[test]
    fn within_a_tier_the_least_loaded_wins() {
        let mut busy = candidate(0);
        busy.in_flight = 8;
        let mut idle = candidate(0);
        idle.in_flight = 1;
        let picked = select(&[busy, idle.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.account, idle.account);
        assert_eq!(picked.stage, Stage::LeastLoaded);
    }

    #[test]
    fn queued_requests_count_towards_load() {
        // Two credentials with one in flight each, but one has a queue. The
        // unqueued one is genuinely freer even though in_flight is equal.
        let mut queued = candidate(0);
        queued.in_flight = 1;
        queued.waiting = 5;
        let mut clear = candidate(0);
        clear.in_flight = 1;
        let picked = select(&[queued, clear.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.account, clear.account);
    }

    #[test]
    fn load_is_relative_to_capacity_not_absolute() {
        // 5/100 is less loaded than 2/10, despite the larger absolute count.
        let mut big = candidate(0);
        big.max_concurrency = 100;
        big.in_flight = 5;
        let mut small = candidate(0);
        small.max_concurrency = 10;
        small.in_flight = 2;
        let picked = select(&[small, big.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.account, big.account);
    }

    #[test]
    fn equally_loaded_prefers_the_window_about_to_reset() {
        // Use-it-or-lose-it: capacity on the credential resetting in an hour is
        // about to evaporate; the other keeps its quota either way.
        let mut expiring = candidate(0);
        expiring.window_resets_at = Some(NOW + 3_600);
        let mut later = candidate(0);
        later.window_resets_at = Some(NOW + 86_400);
        let picked = select(&[later, expiring.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.account, expiring.account);
        assert_eq!(picked.stage, Stage::SoonestReset);
    }

    #[test]
    fn a_window_that_already_reset_is_not_about_to() {
        // `window_resets_at` is written by the usage poller and never
        // cleared once the moment passes. A seat whose window reset an hour
        // ago therefore still carried the timestamp — and won this stage
        // forever as the "soonest" reset, the dashboard reporting
        // `stage="SoonestReset"` as if the feature were working. An elapsed
        // window is no window: the decision falls through to recency.
        let mut elapsed = candidate(0);
        elapsed.window_resets_at = Some(NOW - 3_600);
        elapsed.last_used_at = NOW - 1;
        let mut plain = candidate(0);
        plain.last_used_at = NOW - 10_000;
        let picked = select(&[elapsed, plain.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.stage, Stage::LeastRecentlyUsed, "not SoonestReset");
        assert_eq!(picked.account, plain.account, "recency decided it");
    }

    #[test]
    fn metered_windows_drain_before_unmetered_keys() {
        // A credential with no window has nothing to lose by waiting.
        let mut metered = candidate(0);
        metered.window_resets_at = Some(NOW + 3_600);
        let unmetered = candidate(0);
        let picked = select(&[unmetered, metered.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.account, metered.account);
    }

    #[test]
    fn otherwise_least_recently_used_wins() {
        let mut stale = candidate(0);
        stale.last_used_at = NOW - 10_000;
        let mut fresh = candidate(0);
        fresh.last_used_at = NOW - 1;
        let picked = select(&[fresh, stale.clone()], NOW, 0).expect("selects");
        assert_eq!(picked.account, stale.account);
        assert_eq!(picked.stage, Stage::LeastRecentlyUsed);
    }

    #[test]
    fn identical_candidates_spread_across_the_tie_breaker() {
        // Without this, every replica reading the same snapshot at the same
        // instant picks the same credential and stampedes it.
        let pool: Vec<Candidate> = (0..4).map(|_| candidate(0)).collect();
        let chosen: std::collections::BTreeSet<_> = (0..64)
            .filter_map(|i| select(&pool, NOW, i).map(|s| s.account))
            .collect();
        assert_eq!(chosen.len(), 4, "all four should be reachable");
    }

    #[test]
    fn selection_is_deterministic_for_a_fixed_tie_breaker() {
        let pool: Vec<Candidate> = (0..4).map(|_| candidate(0)).collect();
        let first = select(&pool, NOW, 7).expect("selects");
        for _ in 0..50 {
            assert_eq!(select(&pool, NOW, 7).expect("selects"), first);
        }
    }

    #[test]
    fn zero_capacity_credentials_are_never_chosen() {
        let mut c = candidate(0);
        c.max_concurrency = 0;
        assert!(select(&[c], NOW, 0).is_none());
    }
}
