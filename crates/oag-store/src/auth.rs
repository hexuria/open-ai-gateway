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
//! Three details that matter more than the tiers:
//!
//! **Negative caching, and what it cannot do.** A miss is cached in L1, so a
//! client retrying one bad key does not re-ask Postgres each time. It does
//! NOT stop a scan of random keys, and this comment used to say it did: a
//! fresh key per request misses a negative cache exactly as it misses the
//! positive one. What stops the scan is elsewhere — the inference layer
//! refuses anything not shaped like an issued key before it gets here
//! (`repo::is_issued_key_shape`), and the Postgres lookup itself sits behind
//! a fixed number of permits (`lookup_permits`), so a flood of well-shaped
//! unknown keys sheds `Overloaded` at the door rather than queueing the whole
//! replica at `PgPool::acquire`.
//!
//! **Single-flight.** On an L1 miss, concurrent requests for the same key
//! coalesce into one load. Without it, a popular key expiring means every
//! in-flight request for it hits Postgres simultaneously — a stampede that is
//! worst exactly when traffic is highest.
//!
//! **Only Postgres is authoritative.** L2 is a shared, network-reachable store
//! that a tier below the gateway can write to, so an entry from it is believed
//! only if it carries our own [`AuthMac`] tag. L1 is in-process and is
//! therefore only ever populated from a verified L2 entry or from Postgres.

use crate::cache::AuthMac;
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
    /// `Arc` so that the per-request clone into the single-flight closure does
    /// not copy the secret.
    mac: Arc<AuthMac>,
    /// How many Postgres lookups may be in flight at once. A cache hit never
    /// touches this; a miss takes a permit or is refused. Refused, not queued:
    /// the pool behind it has its own queue with a ten-second timeout, and a
    /// flood of unknown keys used to fill that queue with lookups no valid
    /// credential was needed to start, while every real request waited
    /// behind them.
    lookups: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for AuthCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthCache")
            .field("l1_entries", &self.l1.entry_count())
            .finish_non_exhaustive()
    }
}

impl AuthCache {
    /// `signing_secret` is `security.signing_secret`: it authenticates the L2
    /// entries, and every replica must pass the same one or they will ignore
    /// each other's cache writes.
    ///
    /// `lookup_permits` bounds concurrent Postgres lookups; size it against the
    /// pool, not the traffic — twice `database.max_connections` leaves the
    /// other half of the pool for the requests that authenticated.
    #[must_use]
    pub fn new(
        db: Db,
        cache: Cache,
        max_entries: u64,
        signing_secret: &str,
        lookup_permits: usize,
    ) -> Self {
        Self {
            l1: moka::future::Cache::builder()
                .max_capacity(max_entries)
                .time_to_live(L1_TTL)
                .build(),
            db,
            cache,
            mac: Arc::new(AuthMac::new(signing_secret)),
            lookups: Arc::new(tokio::sync::Semaphore::new(lookup_permits.max(1))),
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
        let mac = Arc::clone(&self.mac);
        let lookups = Arc::clone(&self.lookups);
        let key = hash.clone();

        self.l1
            .try_get_with(hash, async move {
                if let Some(ctx) = cache.auth_get(&key, &mac).await {
                    return Ok::<_, oag_core::Error>(Some(Arc::new(ctx)));
                }
                // A miss in both caches is the one step here that costs a
                // pooled connection. Take a permit or refuse — `try_acquire`,
                // never `acquire`: queueing here is queueing at the pool with
                // extra steps, and the whole point is not to.
                let Ok(_permit) = lookups.try_acquire() else {
                    return Err(oag_core::Error::Overloaded);
                };
                let found = repo::authenticate(&db, raw_key).await?;
                if let Some(ctx) = &found {
                    cache.auth_set(&key, ctx, L2_TTL, &mac).await;
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
        self.invalidate_hash(&repo::hash_key(raw_key)).await;
    }

    /// Same, for a caller that holds the hash and not the plaintext.
    ///
    /// The revoke path is exactly that: `api_key` stores only the hash, so a
    /// revocation can never reconstruct the key it is revoking. Callers use
    /// this *instead of* `invalidate` and the cache call, not as well as —
    /// doing both would issue two DELs for one key.
    pub async fn invalidate_hash(&self, hash: &str) {
        self.l1.invalidate(hash).await;
        self.cache.auth_invalidate(hash).await;
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
