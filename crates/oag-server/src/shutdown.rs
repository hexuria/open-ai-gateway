//! Shutting down without cutting anyone off.
//!
//! A rolling deploy of an ordinary web service is uneventful: requests take
//! milliseconds, so "stop accepting, finish what you have, exit" completes
//! instantly. A streamed completion runs for minutes, and the same sequence
//! with a short deadline severs every one of them.
//!
//! sub2api gives in-flight work a hardcoded five seconds. Every deploy drops
//! every active stream, and to each client it looks like a random upstream
//! failure rather than a deploy.
//!
//! The sequence here:
//!
//! 1. `SIGTERM` arrives.
//! 2. Readiness starts failing **immediately**, so the load balancer's health
//!    check ejects this replica within a few seconds and sends no new work.
//! 3. In-flight streams keep going, for up to `max_stream_duration`.
//! 4. Exit once they finish, or the budget expires — whichever comes first.
//!
//! Step 2 is what makes it work, and it only works if the orchestrator's own
//! grace period is longer than the drain budget. `deploy/compose/stack.yml`
//! sets `stop_grace_period` accordingly; on Kubernetes it is
//! `terminationGracePeriodSeconds`.

use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Process-wide lifecycle state.
#[derive(Debug, Default)]
pub struct Lifecycle {
    draining: AtomicBool,
    in_flight: AtomicU64,
    metrics: OnceLock<PrometheusHandle>,
}

impl Lifecycle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the metrics handle. Once only; later calls are ignored.
    pub fn set_metrics(&self, handle: PrometheusHandle) {
        let _ = self.metrics.set(handle);
    }

    #[must_use]
    pub fn metrics(&self) -> Option<&PrometheusHandle> {
        self.metrics.get()
    }

    /// Whether this replica is shutting down. Readiness reports this.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    fn begin_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
        metrics::gauge!("oag_draining").set(1.0);
    }

    /// Register a request as in flight. Returns a guard that decrements on drop
    /// — including on panic, so a failed request cannot leak the count and
    /// stall shutdown forever.
    #[must_use]
    pub fn track(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlightGuard {
            lifecycle: Arc::clone(self),
        }
    }

    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }
}

/// Decrements the in-flight count when dropped.
#[derive(Debug)]
pub struct InFlightGuard {
    lifecycle: Arc<Lifecycle>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.lifecycle.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Resolves when it is time to stop accepting connections.
///
/// Flips readiness the instant a signal arrives, then waits for in-flight work.
pub async fn signal(lifecycle: Arc<Lifecycle>, drain_budget: Duration) {
    wait_for_signal().await;

    lifecycle.begin_draining();
    tracing::info!(
        in_flight = lifecycle.in_flight(),
        drain_budget_secs = drain_budget.as_secs(),
        "draining: readiness now failing, finishing in-flight requests"
    );

    let deadline = tokio::time::Instant::now() + drain_budget;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    loop {
        ticker.tick().await;
        let remaining = lifecycle.in_flight();
        if remaining == 0 {
            tracing::info!("drain complete");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                remaining,
                "drain budget exhausted; closing with requests still in flight"
            );
            return;
        }
    }
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal as unix_signal};

    // SIGTERM is what an orchestrator sends; SIGINT is what a person sends.
    // Both mean the same thing here.
    let mut term = match unix_signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot listen for SIGTERM");
            std::future::pending::<()>().await;
            return;
        }
    };
    let mut int = match unix_signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot listen for SIGINT");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = int.recv()  => tracing::info!("SIGINT"),
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_lifecycle_is_serving() {
        let l = Lifecycle::new();
        assert!(!l.is_draining());
        assert_eq!(l.in_flight(), 0);
    }

    #[test]
    fn guards_track_in_flight_work() {
        let l = Arc::new(Lifecycle::new());
        let a = l.track();
        let b = l.track();
        assert_eq!(l.in_flight(), 2);
        drop(a);
        assert_eq!(l.in_flight(), 1);
        drop(b);
        assert_eq!(l.in_flight(), 0);
    }

    #[test]
    fn a_panicking_request_does_not_leak_its_slot() {
        // Otherwise one panic stalls every future shutdown for the whole drain
        // budget, waiting on a request that will never finish.
        let l = Arc::new(Lifecycle::new());
        let result = std::panic::catch_unwind({
            let l = Arc::clone(&l);
            move || {
                let _guard = l.track();
                panic!("boom");
            }
        });
        assert!(result.is_err());
        assert_eq!(l.in_flight(), 0, "the guard's Drop still ran");
    }

    #[tokio::test]
    async fn a_guard_moved_into_a_task_keeps_the_request_in_flight() {
        // The bug this guards against: the handler returns as soon as the
        // response *headers* are decided, but a streamed body runs for minutes
        // afterwards. A guard dropped when the handler returns tells shutdown
        // the request is over, and the next rolling deploy severs the stream.
        let l = Arc::new(Lifecycle::new());
        let guard = l.track();
        assert_eq!(l.in_flight(), 1);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            // Stands in for the stream pump: the guard is owned here.
            let _guard = guard;
            let _ = rx.await;
        });

        // The handler has "returned"; the stream has not finished.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            l.in_flight(),
            1,
            "the request must still count while its body is streaming"
        );

        let _ = tx.send(());
        task.await.expect("task");
        assert_eq!(
            l.in_flight(),
            0,
            "and stop counting once it really finishes"
        );
    }

    #[tokio::test]
    async fn draining_completes_once_in_flight_work_finishes() {
        let l = Arc::new(Lifecycle::new());
        let guard = l.track();
        l.begin_draining();
        assert!(l.is_draining());

        let waiter = tokio::spawn({
            let l = Arc::clone(&l);
            async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                let mut ticker = tokio::time::interval(Duration::from_millis(10));
                loop {
                    ticker.tick().await;
                    if l.in_flight() == 0 || tokio::time::Instant::now() >= deadline {
                        return l.in_flight();
                    }
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(guard);
        assert_eq!(waiter.await.expect("waiter"), 0);
    }
}
