//! Per-credential circuit breaking.
//!
//! The cooldowns in the scheduler handle a credential that returned a bad
//! status. This handles the slower failure: one that is *technically* up but
//! failing most requests. Without it, the least-loaded stage actively prefers
//! the broken credential — it fails fast, so it always looks idle.

use std::time::Duration;

/// Breaker position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Passing traffic.
    Closed,
    /// Rejecting, until `until_unix`.
    Open { until_unix: i64 },
    /// Allowing one probe through to see if the credential recovered.
    HalfOpen,
}

/// A rolling failure counter with a cooldown.
#[derive(Debug, Clone)]
pub struct Breaker {
    consecutive_failures: u32,
    trip_threshold: u32,
    cooldown: Duration,
    opened_until: Option<i64>,
    half_open_probe_sent: bool,
}

impl Breaker {
    #[must_use]
    pub fn new(trip_threshold: u32, cooldown: Duration) -> Self {
        Self {
            consecutive_failures: 0,
            trip_threshold: trip_threshold.max(1),
            cooldown,
            opened_until: None,
            half_open_probe_sent: false,
        }
    }

    #[must_use]
    pub fn state(&self, now: i64) -> BreakerState {
        match self.opened_until {
            Some(until) if until > now => BreakerState::Open { until_unix: until },
            Some(_) => BreakerState::HalfOpen,
            None => BreakerState::Closed,
        }
    }

    /// Whether a request may be sent.
    ///
    /// In half-open, exactly one probe is allowed through. Letting the whole
    /// fleet retry the moment a cooldown expires is how a recovering upstream
    /// gets knocked straight back over.
    pub fn allows(&mut self, now: i64) -> bool {
        match self.state(now) {
            BreakerState::Closed => true,
            BreakerState::Open { .. } => false,
            BreakerState::HalfOpen => {
                if self.half_open_probe_sent {
                    false
                } else {
                    self.half_open_probe_sent = true;
                    true
                }
            }
        }
    }

    /// A request succeeded. Fully resets: a success in half-open closes the
    /// breaker outright rather than decrementing towards it.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.opened_until = None;
        self.half_open_probe_sent = false;
    }

    /// A request failed. Trips once the threshold is reached.
    pub fn record_failure(&mut self, now: i64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= self.trip_threshold {
            let secs = i64::try_from(self.cooldown.as_secs()).unwrap_or(i64::MAX);
            self.opened_until = Some(now.saturating_add(secs));
            self.half_open_probe_sent = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000;

    fn breaker() -> Breaker {
        Breaker::new(3, Duration::from_mins(1))
    }

    #[test]
    fn closed_until_the_threshold_is_reached() {
        let mut b = breaker();
        b.record_failure(NOW);
        b.record_failure(NOW);
        assert_eq!(b.state(NOW), BreakerState::Closed);
        b.record_failure(NOW);
        assert!(matches!(b.state(NOW), BreakerState::Open { .. }));
    }

    #[test]
    fn a_success_resets_the_run() {
        let mut b = breaker();
        b.record_failure(NOW);
        b.record_failure(NOW);
        b.record_success();
        b.record_failure(NOW);
        assert_eq!(b.state(NOW), BreakerState::Closed, "the run was broken");
    }

    #[test]
    fn open_rejects_then_half_opens_after_the_cooldown() {
        let mut b = breaker();
        for _ in 0..3 {
            b.record_failure(NOW);
        }
        assert!(!b.allows(NOW));
        assert_eq!(b.state(NOW + 61), BreakerState::HalfOpen);
    }

    #[test]
    fn half_open_admits_exactly_one_probe() {
        // The whole fleet retrying at once is how a recovering upstream gets
        // knocked straight back over.
        let mut b = breaker();
        for _ in 0..3 {
            b.record_failure(NOW);
        }
        let after = NOW + 61;
        assert!(b.allows(after), "first probe goes through");
        assert!(!b.allows(after), "second does not");
        assert!(!b.allows(after), "nor any after that");
    }

    #[test]
    fn a_successful_probe_closes_the_breaker() {
        let mut b = breaker();
        for _ in 0..3 {
            b.record_failure(NOW);
        }
        let after = NOW + 61;
        assert!(b.allows(after));
        b.record_success();
        assert_eq!(b.state(after), BreakerState::Closed);
        assert!(b.allows(after));
    }

    #[test]
    fn a_failed_probe_reopens_immediately() {
        let mut b = breaker();
        for _ in 0..3 {
            b.record_failure(NOW);
        }
        let after = NOW + 61;
        assert!(b.allows(after));
        b.record_failure(after);
        assert!(matches!(b.state(after), BreakerState::Open { .. }));
    }

    #[test]
    fn a_zero_threshold_still_needs_one_failure() {
        let mut b = Breaker::new(0, Duration::from_secs(1));
        assert_eq!(b.state(NOW), BreakerState::Closed);
        b.record_failure(NOW);
        assert!(matches!(b.state(NOW), BreakerState::Open { .. }));
    }
}
