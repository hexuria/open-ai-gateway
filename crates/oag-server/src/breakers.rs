//! Per-credential circuit breakers, held in memory.
//!
//! The database cooldowns in `apply_disposition` handle a credential that
//! returned a bad status. This handles the slower failure: one that is
//! *technically* up and failing most requests.
//!
//! Without it, the least-loaded stage actively prefers the broken credential —
//! it fails fast, so it always looks idle. That is the counter-intuitive part
//! and the reason this exists separately from cooldowns.
//!
//! Deliberately per-replica and not shared. A breaker is a local observation
//! ("my requests to this credential are failing"), it needs no coordination to
//! be useful, and putting it in Redis would add a round trip to the hot path
//! for something each replica can decide alone.

use oag_core::AccountId;
use oag_pool::Breaker;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Consecutive failures before a credential is taken out of rotation.
const TRIP_THRESHOLD: u32 = 5;
/// How long it stays out before one probe is allowed through.
const COOLDOWN: Duration = Duration::from_mins(1);

/// Breakers for every credential this replica has talked to.
#[derive(Debug, Default)]
pub struct Breakers {
    inner: Mutex<HashMap<AccountId, Breaker>>,
}

impl Breakers {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a request may be sent to this credential.
    ///
    /// A poisoned lock fails open rather than propagating: refusing every
    /// credential because a mutex broke would turn a small bug into a total
    /// outage, and the database cooldowns still protect us.
    pub fn allows(&self, account: AccountId, now: i64) -> bool {
        self.inner.lock().map_or(true, |mut m| {
            m.entry(account)
                .or_insert_with(|| Breaker::new(TRIP_THRESHOLD, COOLDOWN))
                .allows(now)
        })
    }

    pub fn record_success(&self, account: AccountId) {
        if let Ok(mut m) = self.inner.lock() {
            m.entry(account)
                .or_insert_with(|| Breaker::new(TRIP_THRESHOLD, COOLDOWN))
                .record_success();
        }
    }

    pub fn record_failure(&self, account: AccountId) {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if let Ok(mut m) = self.inner.lock() {
            m.entry(account)
                .or_insert_with(|| Breaker::new(TRIP_THRESHOLD, COOLDOWN))
                .record_failure(now);
        }
    }

    /// How many credentials are currently tripped. For `/metrics`.
    pub fn open_count(&self, now: i64) -> usize {
        self.inner.lock().map_or(0, |m| {
            m.values()
                .filter(|b| matches!(b.state(now), oag_pool::BreakerState::Open { .. }))
                .count()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unseen_credential_is_allowed() {
        let b = Breakers::new();
        assert!(b.allows(AccountId::new(), 0));
    }

    #[test]
    fn a_run_of_failures_trips_it_and_a_success_clears_it() {
        let b = Breakers::new();
        let a = AccountId::new();
        for _ in 0..TRIP_THRESHOLD {
            b.record_failure(a);
        }
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        assert!(!b.allows(a, now), "should be open");
        assert_eq!(b.open_count(now), 1);

        b.record_success(a);
        assert!(b.allows(a, now));
        assert_eq!(b.open_count(now), 0);
    }

    #[test]
    fn credentials_are_tracked_independently() {
        // A broken credential must not take its healthy neighbours down.
        let b = Breakers::new();
        let broken = AccountId::new();
        let healthy = AccountId::new();
        for _ in 0..TRIP_THRESHOLD {
            b.record_failure(broken);
        }
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        assert!(!b.allows(broken, now));
        assert!(b.allows(healthy, now));
    }
}
