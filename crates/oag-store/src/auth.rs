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

/// How long this identity may be cached, or `None` if it must not be.
///
/// `authenticate` filters expired keys at read time, so a key that has already
/// expired never reaches here. But an entry cached a minute before expiry kept
/// authenticating for the TTL's full five minutes afterwards — on every
/// replica, with the row in the database already saying no. A key with an
/// expiry is one somebody chose to time-box, and five minutes past the deadline
/// is a promise quietly broken.
///
/// Capped rather than refused, so a short-lived key still gets whatever caching
/// its remaining life allows. `None` only for a key whose expiry has already
/// passed between the query and here, which is a race the caller should not
/// paper over by caching the answer.
fn cacheable_for(ctx: &crate::rows::AuthContext, ttl: Duration) -> Option<Duration> {
    let Some(expires_at) = ctx.expires_at else {
        return Some(ttl);
    };
    let now = time::OffsetDateTime::now_utc();
    if expires_at <= now {
        return None;
    }
    let remaining = expires_at - now;
    let remaining = remaining.try_into().unwrap_or(ttl);
    Some(ttl.min(remaining))
}

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
                if let Some(ctx) = &found
                    && let Some(ttl) = cacheable_for(ctx, L2_TTL)
                {
                    cache.auth_set(&key, ctx, ttl, &mac).await;
                }
                Ok(found.map(Arc::new))
            })
            .await
            // The shed has to come back out as itself. Folding every load
            // error into `Internal` turned a refused permit into a 500 with no
            // `Retry-After` and an ERROR log per shed — a non-retryable answer
            // to the one condition that exists to be retried elsewhere.
            .map_err(|e| match &*e {
                oag_core::Error::Overloaded => oag_core::Error::Overloaded,
                other => oag_core::Error::Internal(format!("auth lookup: {other}")),
            })
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
        // Best-effort at this layer, and deliberately: the callers here are
        // keeping the cache tidy after a write that has already happened. The
        // one caller for whom the outcome is a statement to a human — the CLI's
        // revoke, which prints "shared cache evicted" — goes to
        // `Cache::auth_invalidate` directly and reads the result.
        if let Err(e) = self.cache.auth_invalidate(hash).await {
            tracing::warn!(error = %e, "an identity could not be evicted from the shared cache");
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache over backends nothing listens on: Redis misses fast, and the
    /// Postgres lookup behind the permit is never reached.
    fn dead_backends(lookup_permits: usize) -> AuthCache {
        let db = Db::connect("postgres://oag:oag@127.0.0.1:1/oag", 1).expect("lazy pool");
        let cache = Cache::connect("redis://127.0.0.1:1").expect("lazy client");
        AuthCache::new(
            db,
            cache,
            16,
            "a-signing-secret-long-enough-for-the-mac",
            lookup_permits,
        )
    }

    #[tokio::test]
    async fn a_shed_lookup_is_overloaded_not_internal() {
        // Every error out of the single-flight load used to be rewritten as
        // `Internal`, the refused permit included. That answered a shed with
        // a 500 and no `Retry-After`, logged it as an error, and counted it
        // as one — a non-retryable answer to the one condition whose whole
        // point is to be retried on a replica with room.
        let auth = dead_backends(1);
        let held = auth.lookups.try_acquire().expect("the only permit");

        let err = auth
            .authenticate("oag_sk_deadbeefdeadbeefdeadbeefdeadbeef")
            .await
            .expect_err("no permit, no lookup");
        assert!(matches!(err, oag_core::Error::Overloaded), "{err:?}");
        drop(held);
    }
    /// S7. A cached identity cannot outlive the key it belongs to.
    ///
    /// `authenticate` filters expired keys at read time, so an expired key
    /// never reaches the cache. But one cached a minute before expiry kept
    /// authenticating for the L2 TTL's full five minutes afterwards — on every
    /// replica, with the row in the database already saying no. A key with an
    /// expiry is one somebody chose to time-box.
    #[test]
    fn a_cached_identity_expires_no_later_than_its_key() {
        let ttl = Duration::from_mins(5);
        let now = time::OffsetDateTime::now_utc();
        let ctx = |expires_at| crate::rows::AuthContext {
            api_key_id: uuid::Uuid::nil(),
            principal_id: uuid::Uuid::nil(),
            route_id: uuid::Uuid::nil(),
            key_floor_tier: None,
            admin: false,
            quota_usd: None,
            principal_budget_usd: None,
            principal_hard_stop_multiple: rust_decimal::Decimal::ONE,
            expires_at,
        };

        // No expiry: the full TTL, exactly as before.
        assert_eq!(cacheable_for(&ctx(None), ttl), Some(ttl));

        // Expiring inside the window: capped to what is left, so the entry and
        // the key stop working at the same moment.
        let soon = cacheable_for(&ctx(Some(now + time::Duration::minutes(1))), ttl)
            .expect("a live key is cacheable");
        assert!(
            soon <= Duration::from_mins(1) && soon > Duration::from_secs(50),
            "about a minute, not five: {soon:?}"
        );

        // Expiring well beyond it: the TTL still bounds the cache.
        assert_eq!(
            cacheable_for(&ctx(Some(now + time::Duration::hours(2))), ttl),
            Some(ttl)
        );

        // Already gone — a race between the query and here. Not cached at all,
        // rather than cached for a negative duration or clamped to zero.
        assert_eq!(
            cacheable_for(&ctx(Some(now - time::Duration::seconds(1))), ttl),
            None
        );
    }
}
