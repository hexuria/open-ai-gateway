//! Periodic slot hygiene: trim Redis, publish the gauge including zero.
//!
//! Selection only writes `oag_slots_in_use` when it successfully counts. After
//! Redis drops a key — TTL, `DEL`, an admin clear — a replica that then 503s
//! or hangs never writes, and Prometheus keeps the last full observation.
//! Operators chase ghosts. This sweep is the thing that writes when nothing
//! selects.

use crate::AppState;
use crate::gateway::select::{
    SLOT_TTL, fastrand_u64, publish_slots_in_use, slot_accounting_degraded,
};
use std::sync::Arc;
use std::time::Duration;

/// How often every replica re-reads Redis and publishes the gauge.
///
/// Short enough that a clear or a TTL trim shows up on the dashboard before
/// the next remasure burst; long enough that a fleet of seats is one pipeline,
/// not a hot path.
const SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// Accounts per Redis pipeline.
///
/// One pipeline for every account ran N scripts under the request path's 2s
/// slot deadline, and a timeout drops the connection live requests share: a
/// large account table made a background task wedge the request path every
/// 15 seconds. A chunk this size is milliseconds on a healthy Redis, so a
/// chunk that times out means Redis really is in trouble.
const SWEEP_CHUNK: usize = 200;

/// Start the slot sweep, for as long as the process runs.
///
/// The first tick lands at a random point in the first interval, so replicas
/// that start together do not sweep together.
pub fn spawn_slot_sweep(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(sweep_forever(state))
}

async fn sweep_forever(state: Arc<AppState>) {
    let start = first_tick_at(tokio::time::Instant::now());
    let mut ticker = tokio::time::interval_at(start, SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut labels = Labels::default();
    loop {
        ticker.tick().await;
        let now = tokio::time::Instant::now();
        sweep_once(&state, labels.current(&state.db, now).await).await;
    }
}

/// How long the sweep keeps its list of accounts before reading it again.
///
/// Accounts are added and renamed by an operator, not per request. Reading
/// the whole table on every 15s sweep, on every replica, was a full scan four
/// times a minute to learn nothing new. A new credential's gauge can take up
/// to this plus one [`SWEEP_INTERVAL`] to appear (the read happens on the
/// first sweep after the window closes); selection counts its slots from the
/// first request regardless.
const LABEL_REFRESH: Duration = Duration::from_mins(5);

/// The accounts the sweep publishes a gauge for, read at most every
/// [`LABEL_REFRESH`].
#[derive(Default)]
struct Labels {
    list: Vec<(oag_core::AccountId, String)>,
    read_at: Option<tokio::time::Instant>,
}

impl Labels {
    /// The list, read again if it is older than [`LABEL_REFRESH`].
    ///
    /// A failed read keeps the list it has and tries again on the next sweep:
    /// an old list still publishes every gauge it knows, where no list
    /// publishes none.
    async fn current(
        &mut self,
        db: &oag_store::Db,
        now: tokio::time::Instant,
    ) -> &[(oag_core::AccountId, String)] {
        if self.due(now) {
            let read = oag_store::repo::account_slot_labels(db).await;
            self.apply(read, now);
        }
        &self.list
    }

    fn due(&self, now: tokio::time::Instant) -> bool {
        self.read_at
            .is_none_or(|at| now.duration_since(at) >= LABEL_REFRESH)
    }

    /// Take a read's result. A successful read replaces the list, empty
    /// included, and starts the window; a failed one changes nothing, so the
    /// next sweep is still due to read.
    fn apply(
        &mut self,
        read: oag_core::Result<Vec<(oag_core::AccountId, String)>>,
        now: tokio::time::Instant,
    ) {
        match read {
            Ok(list) => {
                self.list = list;
                self.read_at = Some(now);
            }
            Err(e) => tracing::warn!(error = %e, "slot sweep: could not list accounts"),
        }
    }
}

/// When the first sweep runs: a random point in the interval after `now`.
fn first_tick_at(now: tokio::time::Instant) -> tokio::time::Instant {
    now + first_tick_offset()
}

/// A random point in `[0, SWEEP_INTERVAL)`.
fn first_tick_offset() -> Duration {
    // The top 53 bits as a fraction in [0, 1): exact in an f64.
    #[allow(clippy::cast_precision_loss)]
    let fraction = (fastrand_u64() >> 11) as f64 / (1u64 << 53) as f64;
    SWEEP_INTERVAL.mul_f64(fraction)
}

/// One pass: trim every credential's slot key and publish the live count,
/// zero included. Returns how many counts it published.
///
/// A failed count publishes nothing: it is not idle, and zero would wipe a
/// real reading during the one outage in which nothing knows. But it is said:
/// this is the only thing that republishes the gauge when nothing selects, and
/// a sweep failing in silence looked exactly like the bug it exists to fix.
async fn sweep_once(state: &AppState, labels: &[(oag_core::AccountId, String)]) -> usize {
    let mut published = 0;
    for chunk in labels.chunks(SWEEP_CHUNK) {
        let ids: Vec<_> = chunk.iter().map(|(id, _)| *id).collect();
        match state.cache.slots_in_use_many(&ids, SLOT_TTL).await {
            Ok(counts) => {
                for ((_, name), n) in chunk.iter().zip(counts) {
                    publish_slots_in_use(name, n);
                    published += 1;
                }
            }
            Err(e) => {
                // The rest would fail the same way, two seconds each.
                slot_accounting_degraded("sweep", &e);
                break;
            }
        }
    }
    published
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_sweep_lands_inside_the_first_interval() {
        for _ in 0..1000 {
            assert!(first_tick_offset() < SWEEP_INTERVAL);
        }
    }

    #[test]
    fn the_first_sweep_is_in_the_future() {
        let now = tokio::time::Instant::now();
        for _ in 0..100 {
            let at = first_tick_at(now);
            assert!(at >= now && at < now + SWEEP_INTERVAL);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_sweep_runs_as_its_own_task() {
        // A dead backend: each pass fails and returns, and the task goes on.
        let task = spawn_slot_sweep(crate::testing::state(""));
        tokio::time::sleep(SWEEP_INTERVAL * 2).await;
        assert!(
            !task.is_finished(),
            "the sweep loop must outlive its passes"
        );
        task.abort();
    }

    #[test]
    fn replicas_do_not_all_sweep_at_the_same_offset() {
        let offsets: std::collections::HashSet<_> = (0..16).map(|_| first_tick_offset()).collect();
        assert!(offsets.len() > 1, "every draw was the same offset");
    }

    /// The same labels the sweep would read, plus `extra` accounts of our own,
    /// enough to need two chunks. Cleaned up by the returned tag.
    async fn with_accounts(db: &oag_store::Db, extra: usize) -> String {
        let tag = uuid::Uuid::new_v4().to_string();
        for i in 0..extra {
            sqlx::query(
                "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
                 credentials_nonce) VALUES (gen_random_uuid(), $1, 'anthropic', 'api_key', \
                 '\\x00', '\\x00')",
            )
            .bind(format!("sweep-{tag}-{i}"))
            .execute(db.pool())
            .await
            .expect("account");
        }
        tag
    }

    async fn forget(db: &oag_store::Db, tag: &str) {
        sqlx::query("DELETE FROM account WHERE name LIKE $1")
            .bind(format!("sweep-{tag}-%"))
            .execute(db.pool())
            .await
            .expect("cleanup");
    }

    /// The list is read once, reused inside the refresh window, and read again
    /// after it.
    #[tokio::test]
    async fn the_account_list_is_reread_only_after_the_refresh_window() {
        let Ok(db_url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = oag_store::Db::connect(&db_url, 4).expect("pool");
        db.migrate().await.expect("migrate");
        let start = tokio::time::Instant::now();
        let mut labels = Labels::default();
        // The first read.
        labels.current(&db, start).await;

        let tag = with_accounts(&db, 1).await;
        let within = labels
            .current(&db, start + LABEL_REFRESH - Duration::from_secs(1))
            .await
            .iter()
            .any(|(_, name)| name.starts_with(&format!("sweep-{tag}")));
        let after = labels
            .current(&db, start + LABEL_REFRESH)
            .await
            .iter()
            .any(|(_, name)| name.starts_with(&format!("sweep-{tag}")));
        forget(&db, &tag).await;

        assert!(!within, "read again inside the window");
        assert!(after, "not read again once the window had passed");
    }

    fn label(name: &str) -> (oag_core::AccountId, String) {
        (
            oag_core::AccountId::from_uuid(uuid::Uuid::new_v4()),
            name.to_owned(),
        )
    }

    #[test]
    fn a_failed_reread_keeps_the_list_and_stays_due() {
        let start = tokio::time::Instant::now();
        let mut labels = Labels::default();
        labels.apply(Ok(vec![label("seat")]), start);
        let later = start + LABEL_REFRESH;
        assert!(labels.due(later));

        labels.apply(
            Err(oag_core::Error::Internal("postgres down".to_owned())),
            later,
        );
        assert_eq!(
            labels.list.len(),
            1,
            "an unreadable table is not an empty one"
        );
        assert!(
            labels.due(later + Duration::from_secs(1)),
            "a failed read must not start a new window: the next sweep reads again"
        );
    }

    #[test]
    fn a_successful_empty_read_empties_the_list() {
        // Every account deleted is a real answer, unlike a failed read.
        let start = tokio::time::Instant::now();
        let mut labels = Labels::default();
        labels.apply(Ok(vec![label("seat")]), start);
        labels.apply(Ok(Vec::new()), start + LABEL_REFRESH);
        assert!(labels.list.is_empty());
        assert!(!labels.due(start + LABEL_REFRESH + Duration::from_secs(1)));
    }

    #[test]
    fn the_list_is_due_again_exactly_at_the_end_of_its_window() {
        let start = tokio::time::Instant::now();
        let mut labels = Labels::default();
        assert!(labels.due(start), "never read");
        labels.apply(Ok(Vec::new()), start);
        assert!(!labels.due(start + LABEL_REFRESH - Duration::from_secs(1)));
        assert!(labels.due(start + LABEL_REFRESH));
    }

    /// Every account is counted, across more than one chunk.
    #[tokio::test]
    async fn a_sweep_counts_every_account_across_chunks() {
        let (Ok(db_url), Ok(redis_url)) = (
            std::env::var("OAG_TEST_DATABASE_URL"),
            std::env::var("OAG_TEST_REDIS_URL"),
        ) else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
            return;
        };
        let config = oag_core::config::Config::from_yaml(&crate::testing::config_yaml(
            &db_url, &redis_url, "",
        ))
        .expect("config");
        let db = oag_store::Db::connect(&config.database.url, 4).expect("pool");
        db.migrate().await.expect("migrate");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("cache");
        let state = AppState::new(config, db.clone(), cache).expect("state");
        let tag = with_accounts(&db, SWEEP_CHUNK + 1).await;

        let labels = oag_store::repo::account_slot_labels(&db)
            .await
            .expect("list");
        let published = sweep_once(&state, &labels).await;
        forget(&db, &tag).await;
        // Not an exact count: other tests add accounts concurrently. Our own
        // rows alone need two chunks, so a sweep that stopped after the first
        // one, or published nothing, falls short of this.
        assert!(published > SWEEP_CHUNK, "published {published}");
    }

    /// Redis down: nothing is published, and the sweep stops at the first
    /// chunk rather than paying the deadline once per chunk.
    #[tokio::test]
    async fn a_sweep_against_a_dead_redis_publishes_nothing() {
        let Ok(db_url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let config = oag_core::config::Config::from_yaml(&crate::testing::config_yaml(
            &db_url,
            "redis://127.0.0.1:1",
            "",
        ))
        .expect("config");
        let db = oag_store::Db::connect(&config.database.url, 4).expect("pool");
        db.migrate().await.expect("migrate");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("cache");
        let state = AppState::new(config, db.clone(), cache).expect("state");
        let tag = with_accounts(&db, SWEEP_CHUNK + 1).await;

        let labels = oag_store::repo::account_slot_labels(&db)
            .await
            .expect("list");
        let published = sweep_once(&state, &labels).await;
        forget(&db, &tag).await;
        assert_eq!(published, 0);
    }

    #[test]
    fn the_sweep_runs_inside_a_slot_lease() {
        assert!(
            SWEEP_INTERVAL < SLOT_TTL,
            "a ghost left after TTL would sit until the next acquire without this"
        );
    }
}
