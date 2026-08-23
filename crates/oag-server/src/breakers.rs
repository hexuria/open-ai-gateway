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
use oag_pool::{Admission, Breaker};
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

    /// Whether a request *may* be sent to this credential.
    ///
    /// For filtering candidates, and therefore free of side effects: selection
    /// asks about every credential it can reach and sends to one of them. Use
    /// [`Dispatch::claim`] at the point a request actually goes out.
    ///
    /// A poisoned lock fails open rather than propagating: refusing every
    /// credential because a mutex broke would turn a small bug into a total
    /// outage, and the database cooldowns still protect us.
    pub fn permits(&self, account: AccountId, now: i64) -> bool {
        self.inner
            .lock()
            .map_or(true, |m| m.get(&account).is_none_or(|b| b.permits(now)))
    }

    /// Claim the right to send one request. See [`Dispatch`].
    fn begin_request(&self, account: AccountId, now: i64) -> Admission {
        self.inner.lock().map_or(Admission::Admitted, |mut m| {
            m.entry(account)
                .or_insert_with(|| Breaker::new(TRIP_THRESHOLD, COOLDOWN))
                .begin_request(now)
        })
    }

    fn release_probe(&self, account: AccountId) {
        if let Ok(mut m) = self.inner.lock()
            && let Some(b) = m.get_mut(&account)
        {
            b.release_probe();
        }
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

/// The right to send one request to a credential, held for as long as sending
/// it might still be called off.
///
/// Selection only *reads* the breakers, so a credential that was considered and
/// passed over keeps its half-open probe. This is what spends the probe — and
/// what gives it back if the request never reaches the wire, because a probe
/// that was claimed and abandoned observed nothing about the credential and
/// must not cost it another cooldown.
#[derive(Debug)]
pub struct Dispatch<'a> {
    breakers: &'a Breakers,
    account: AccountId,
    /// Set only while this guard holds an unspent half-open probe.
    probe: bool,
}

impl<'a> Dispatch<'a> {
    /// `None` when the breaker is open, or when another request beat us to the
    /// half-open probe between selection and here.
    pub fn claim(breakers: &'a Breakers, account: AccountId, now: i64) -> Option<Self> {
        let probe = match breakers.begin_request(account, now) {
            Admission::Denied => return None,
            Admission::Admitted => false,
            Admission::Probe => true,
        };
        Some(Self {
            breakers,
            account,
            probe,
        })
    }

    /// The request has gone to the wire. Whatever comes back — including a
    /// connection failure — is an observation, so the outcome recorded against
    /// the breaker now owns its state and there is nothing left to give back.
    pub fn sent(&mut self) {
        self.probe = false;
    }
}

impl Drop for Dispatch<'_> {
    fn drop(&mut self) {
        if self.probe {
            self.breakers.release_probe(self.account);
        }
    }
}

impl Breakers {
    /// Put a credential back in rotation after an operator cleared its cooldown.
    ///
    /// Replica-local, consistent with the rest of this module: other replicas
    /// heal on their own half-open probe. Expressed as a success rather than a
    /// separate reset path so there is one definition of "healthy".
    pub fn clear(&self, account: AccountId) {
        self.record_success(account);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp()
    }

    /// A tripped credential, and the moment its cooldown has expired.
    fn tripped(breakers: &Breakers) -> (AccountId, i64) {
        let account = AccountId::new();
        for _ in 0..TRIP_THRESHOLD {
            breakers.record_failure(account);
        }
        let recovered = now() + i64::try_from(COOLDOWN.as_secs()).unwrap_or(i64::MAX) + 1;
        (account, recovered)
    }

    #[test]
    fn an_unseen_credential_is_allowed() {
        let b = Breakers::new();
        assert!(b.permits(AccountId::new(), 0));
    }

    #[test]
    fn a_run_of_failures_trips_it_and_a_success_clears_it() {
        let b = Breakers::new();
        let a = AccountId::new();
        for _ in 0..TRIP_THRESHOLD {
            b.record_failure(a);
        }
        let now = now();
        assert!(!b.permits(a, now), "should be open");
        assert_eq!(b.open_count(now), 1);

        b.record_success(a);
        assert!(b.permits(a, now));
        assert_eq!(b.open_count(now), 0);
    }

    #[test]
    fn credentials_are_tracked_independently() {
        // A broken credential must not take its healthy neighbours down.
        let b = Breakers::new();
        let healthy = AccountId::new();
        let (broken, _) = tripped(&b);
        let now = now();
        assert!(!b.permits(broken, now));
        assert!(b.permits(healthy, now));
    }

    #[test]
    fn clearing_a_breaker_puts_the_credential_back_in_rotation() {
        // What the admin clear-cooldown button reaches. The database cooldown
        // is fleet-wide; this is the replica-local half of the same action.
        let breakers = Breakers::new();
        let (account, _) = tripped(&breakers);
        let now = now();
        assert!(
            !breakers.permits(account, now),
            "a run of failures trips it"
        );

        breakers.clear(account);
        assert!(breakers.permits(account, now));
        assert_eq!(breakers.open_count(now), 0);
    }

    #[test]
    fn filtering_candidates_leaves_the_probe_for_whoever_dispatches() {
        // The bug this replaced: `select` asked the breaker about every
        // candidate while filtering, which spent the probe on a credential the
        // request then did not use. That credential never saw another request,
        // so it never recorded the success that would close its breaker.
        let breakers = Breakers::new();
        let (account, recovered) = tripped(&breakers);

        for _ in 0..16 {
            assert!(breakers.permits(account, recovered), "considered, not used");
        }

        let mut dispatch =
            Dispatch::claim(&breakers, account, recovered).expect("the probe is still there");
        assert!(
            Dispatch::claim(&breakers, account, recovered).is_none(),
            "and only one request gets it"
        );
        dispatch.sent();
        drop(dispatch);

        breakers.record_success(account);
        assert!(breakers.permits(account, recovered));
        assert_eq!(breakers.open_count(recovered), 0);
    }

    #[test]
    fn a_dispatch_that_never_happened_returns_the_probe() {
        let breakers = Breakers::new();
        let (account, recovered) = tripped(&breakers);

        drop(Dispatch::claim(&breakers, account, recovered).expect("probe"));

        assert!(
            Dispatch::claim(&breakers, account, recovered).is_some(),
            "the next request may still probe"
        );
    }
}
