//! Authenticating inbound keys, cheaply.
//!
//! Auth is on the hot path of every single request, so a naive implementation
//! puts a database round trip in front of every call. Three tiers:
//!
//! 1. **L1** — in-process, short TTL. Absorbs the repeat traffic of one client
//!    hammering one key, which is most traffic.
//! 2. **L2** — Redis, longer TTL. Absorbs a cold replica and a restart.
//! 3. **Postgres** — the truth.
//!
//! Two details that matter more than the tiers:
//!
//! **Negative caching.** A miss is cached too. Without it, a scan of random
//! keys reaches Postgres on every request and an attacker gets a free
//! amplification lever against the database.
//!
//! **Single-flight.** On an L1 miss, concurrent requests for the same key
//! coalesce into one load. Without it, a popular key expiring means every
//! in-flight request for it hits Postgres simultaneously — a stampede that is
//! worst exactly when traffic is highest.

use crate::rows::AuthContext;
use crate::{Cache, Db, repo};
use oag_core::Result;
use std::sync::Arc;
use std::time::Duration;

/// Short, because it bounds how long a revoked key keeps working on a replica
/// that already cached it. Fifteen seconds of staleness is the price of not
/// querying Postgres on every request; anything longer starts to matter when
/// someone revokes a leaked key.
const L1_TTL: Duration = Duration::from_secs(15);
/// Longer, because Redis is invalidated explicitly on revocation rather than
/// waiting for expiry.
const L2_TTL: Duration = Duration::from_mins(5);

/// The three-tier lookup.
#[derive(Clone)]
pub struct AuthCache {
    l1: moka::future::Cache<String, Option<Arc<AuthContext>>>,
    db: Db,
    cache: Cache,
}

impl std::fmt::Debug for AuthCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthCache")
            .field("l1_entries", &self.l1.entry_count())
            .finish_non_exhaustive()
    }
}

impl AuthCache {
    #[must_use]
    pub fn new(db: Db, cache: Cache, max_entries: u64) -> Self {
        Self {
            l1: moka::future::Cache::builder()
                .max_capacity(max_entries)
                .time_to_live(L1_TTL)
                .build(),
            db,
            cache,
        }
    }

    /// Resolve a raw inbound key.
    ///
    /// `Ok(None)` means the key is not valid — a cached fact, not an error.
    pub async fn authenticate(&self, raw_key: &str) -> Result<Option<Arc<AuthContext>>> {
        let hash = repo::hash_key(raw_key);

        // `try_get_with` is the single-flight: concurrent callers for the same
        // key await one load rather than each starting their own.
        let db = self.db.clone();
        let cache = self.cache.clone();
        let key = hash.clone();

        self.l1
            .try_get_with(hash, async move {
                if let Some(ctx) = cache.auth_get(&key).await {
                    return Ok::<_, oag_core::Error>(Some(Arc::new(ctx)));
                }
                let found = repo::authenticate(&db, raw_key).await?;
                if let Some(ctx) = &found {
                    cache.auth_set(&key, ctx, L2_TTL).await;
                }
                Ok(found.map(Arc::new))
            })
            .await
            .map_err(|e| oag_core::Error::Internal(format!("auth lookup: {e}")))
    }

    /// Drop a key from every tier on this replica, and from Redis.
    ///
    /// Called when a key is revoked or edited. Other replicas' L1 entries still
    /// expire on their own within [`L1_TTL`], which bounds the window.
    pub async fn invalidate(&self, raw_key: &str) {
        let hash = repo::hash_key(raw_key);
        self.l1.invalidate(&hash).await;
        self.cache.auth_invalidate(&hash).await;
    }

    /// Drop every L1 entry on this replica.
    ///
    /// Not async: moka's bulk invalidation is a flag flip, and the entries are
    /// reclaimed lazily on subsequent reads.
    pub fn invalidate_all(&self) {
        self.l1.invalidate_all();
    }

    #[must_use]
    pub fn l1_len(&self) -> u64 {
        self.l1.entry_count()
    }
}
