//! Periodic slot hygiene: trim Redis, publish the gauge including zero.
//!
//! Selection only writes `oag_slots_in_use` when it successfully counts. After
//! Redis drops a key — TTL, `DEL`, an admin clear — a replica that then 503s
//! or hangs never writes, and Prometheus keeps the last full observation.
//! Operators chase ghosts. This sweep is the thing that writes when nothing
//! selects.

use crate::AppState;
use crate::gateway::select::{SLOT_TTL, publish_slots_in_use};
use std::sync::Arc;
use std::time::Duration;

/// How often every replica re-reads Redis and publishes the gauge.
///
/// Short enough that a clear or a TTL trim shows up on the dashboard before
/// the next remasure burst; long enough that a fleet of seats is one pipeline,
/// not a hot path.
const SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// Start the slot sweep, for as long as the process runs.
pub fn spawn_slot_sweep(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            ticker.tick().await;
            sweep_once(&state).await;
        }
    });
}

/// One pass: trim every credential's slot key and publish the live count,
/// zero included.
async fn sweep_once(state: &AppState) {
    let labels = match oag_store::repo::account_slot_labels(&state.db).await {
        Ok(labels) => labels,
        Err(e) => {
            tracing::debug!(error = %e, "slot sweep: could not list accounts");
            return;
        }
    };
    if labels.is_empty() {
        return;
    }
    let ids: Vec<_> = labels.iter().map(|(id, _)| *id).collect();
    let counts = match state.cache.slots_in_use_many(&ids, SLOT_TTL).await {
        Ok(counts) if counts.len() == ids.len() => counts,
        Ok(_) | Err(_) => {
            // A failed count is not idle. Publishing zero here would wipe a
            // real reading during the one outage in which nothing knows.
            return;
        }
    };
    for ((_, name), n) in labels.iter().zip(counts) {
        publish_slots_in_use(name, n);
    }
}

#[cfg(test)]
mod tests {
    use super::SWEEP_INTERVAL;
    use crate::gateway::select::SLOT_TTL;

    #[test]
    fn the_sweep_runs_inside_a_slot_lease() {
        assert!(
            SWEEP_INTERVAL < SLOT_TTL,
            "a ghost left after TTL would sit until the next acquire without this"
        );
    }
}
