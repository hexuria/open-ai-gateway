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
    /// Unix seconds of last use. Drives LRU.
    pub last_used_at: i64,
}

impl Candidate {
    /// Whether this credential can take a request right now.
    fn eligible(&self, now: i64) -> bool {
        self.schedulable
            && self.cooldown_until.is_none_or(|t| t <= now)
            && self.rate_limited_until.is_none_or(|t| t <= now)
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
    let least_loaded: Vec<&Candidate> = tier.into_iter().filter(|c| c.load_bp() == min_load).collect();
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
    let soonest = least_loaded.iter().filter_map(|c| c.window_resets_at).min();
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
    let coldest: Vec<&Candidate> = pool.into_iter().filter(|c| c.last_used_at == oldest).collect();
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
            last_used_at: NOW - 100,
        }
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
