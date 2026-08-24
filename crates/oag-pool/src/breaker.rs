//! Per-credential circuit breaking.
//!
//! The cooldowns in the scheduler handle a credential that returned a bad
//! status. This handles the slower failure: one that is *technically* up but
//! failing most requests. Without it, the least-loaded stage actively prefers
//! the broken credential — it fails fast, so it always looks idle.

use std::time::Duration;

/// What claiming a dispatch got you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The breaker is open, or another request is already holding the probe.
    Denied,
    /// Ordinary traffic through a closed breaker.
    Admitted,
    /// The single half-open probe. It is spent now, so a caller that does not
    /// actually send the request owes a [`Breaker::release_probe`].
    Probe,
}

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

    /// Whether a request *may* be sent — asked without sending one.
    ///
    /// Pure, and that is the whole point. Selection asks this about every
    /// candidate and then picks one of them. If merely asking spent the
    /// half-open probe, a recovering credential that was considered and passed
    /// over would have burnt its one chance on a request that went elsewhere,
    /// and nothing would ever record the success that closes it again.
    #[must_use]
    pub fn permits(&self, now: i64) -> bool {
        match self.state(now) {
            BreakerState::Closed => true,
            BreakerState::Open { .. } => false,
            BreakerState::HalfOpen => !self.half_open_probe_sent,
        }
    }

    /// Claim the right to dispatch, at the point the request is about to go
    /// out.
    ///
    /// In half-open exactly one caller gets [`Admission::Probe`]. Letting the
    /// whole fleet retry the moment a cooldown expires is how a recovering
    /// upstream gets knocked straight back over.
    pub fn begin_request(&mut self, now: i64) -> Admission {
        match self.state(now) {
            BreakerState::Closed => Admission::Admitted,
            BreakerState::Open { .. } => Admission::Denied,
            BreakerState::HalfOpen => {
                if self.half_open_probe_sent {
                    Admission::Denied
                } else {
                    self.half_open_probe_sent = true;
                    Admission::Probe
                }
            }
        }
    }

    /// Hand back a probe whose request never reached the wire.
    ///
    /// An abandoned probe taught us nothing about the credential, so keeping
    /// the token would hold the breaker shut for a whole further cooldown over
    /// a request that never happened. Leaves the failure count alone: this is
    /// not an outcome, it is the absence of one.
    pub fn release_probe(&mut self) {
        self.half_open_probe_sent = false;
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

    /// A breaker that has just tripped, and the moment its cooldown expires.
    fn half_open() -> (Breaker, i64) {
        let mut b = breaker();
        for _ in 0..3 {
            b.record_failure(NOW);
        }
        (b, NOW + 61)
    }

    #[test]
    fn open_rejects_then_half_opens_after_the_cooldown() {
        let (mut b, after) = half_open();
        assert_eq!(b.begin_request(NOW), Admission::Denied);
        assert_eq!(b.state(after), BreakerState::HalfOpen);
    }

    #[test]
    fn half_open_admits_exactly_one_probe() {
        // The whole fleet retrying at once is how a recovering upstream gets
        // knocked straight back over.
        let (mut b, after) = half_open();
        assert_eq!(b.begin_request(after), Admission::Probe, "one goes through");
        assert_eq!(
            b.begin_request(after),
            Admission::Denied,
            "the second does not"
        );
        assert_eq!(b.begin_request(after), Admission::Denied, "nor any after");
    }

    #[test]
    fn permits_does_not_consume_the_half_open_probe() {
        // Selection filters every candidate through `permits` and then picks
        // one. If asking spent the probe, a credential that was considered and
        // passed over would never get a request again: no request means no
        // success, and no success means the breaker never closes.
        let (mut b, after) = half_open();
        for _ in 0..16 {
            assert!(b.permits(after), "reading must not spend the probe");
        }
        assert_eq!(b.begin_request(after), Admission::Probe);
        assert!(!b.permits(after), "now somebody is holding it");
    }

    #[test]
    fn an_abandoned_probe_goes_back() {
        // The request never reached the wire, so it observed nothing. Keeping
        // the token would shut the credential out for another whole cooldown.
        let (mut b, after) = half_open();
        assert_eq!(b.begin_request(after), Admission::Probe);
        b.release_probe();

        assert!(b.permits(after), "the next caller may probe");
        assert_eq!(
            b.state(after),
            BreakerState::HalfOpen,
            "still half-open: releasing is not a success"
        );
        assert_eq!(b.begin_request(after), Admission::Probe);
    }

    #[test]
    fn a_successful_probe_closes_the_breaker() {
        let (mut b, after) = half_open();
        assert_eq!(b.begin_request(after), Admission::Probe);
        b.record_success();
        assert_eq!(b.state(after), BreakerState::Closed);
        assert_eq!(b.begin_request(after), Admission::Admitted);
    }

    #[test]
    fn a_failed_probe_reopens_immediately() {
        let (mut b, after) = half_open();
        assert_eq!(b.begin_request(after), Admission::Probe);
        b.record_failure(after);
        assert!(matches!(b.state(after), BreakerState::Open { .. }));
        assert!(!b.permits(after));
    }

    #[test]
    fn a_zero_threshold_still_needs_one_failure() {
        let mut b = Breaker::new(0, Duration::from_secs(1));
        assert_eq!(b.state(NOW), BreakerState::Closed);
        b.record_failure(NOW);
        assert!(matches!(b.state(NOW), BreakerState::Open { .. }));
    }

    #[test]
    fn releasing_without_a_probe_is_harmless() {
        // Callers that were merely admitted through a closed breaker may still
        // give back, and must not thereby hand out anything.
        let mut b = breaker();
        assert_eq!(b.begin_request(NOW), Admission::Admitted);
        b.release_probe();
        assert_eq!(b.state(NOW), BreakerState::Closed);
        assert!(b.permits(NOW));
    }
}
