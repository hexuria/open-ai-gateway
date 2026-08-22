//! Redis: what the replicas need to agree on.
//!
//! Four things live here — concurrency slots, session pins, the auth cache, and
//! route rate limiting — and all four are expendable. Losing Redis costs a
//! burst of database reads, a moment of sloppy concurrency accounting, and an
//! unthrottled minute. It never loses money or credentials, because those are
//! in Postgres.

use oag_core::{AccountId, Error, Result};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Acquire a concurrency slot if the credential is under its limit.
///
/// One sorted set per credential; members are request ids scored by acquisition
/// time. The script trims members older than the TTL, then adds only if the
/// resulting size is under the limit. Atomic, so two replicas cannot both see
/// the last free slot.
///
/// `TIME` comes from Redis rather than the caller so replicas with skewed
/// clocks still agree on what "expired" means.
const ACQUIRE_SLOT: &str = r"
local key    = KEYS[1]
local member = ARGV[1]
local limit  = tonumber(ARGV[2])
local ttl    = tonumber(ARGV[3])

local now = redis.call('TIME')[1]
redis.call('ZREMRANGEBYSCORE', key, '-inf', now - ttl)

if redis.call('ZCARD', key) >= limit then
  return 0
end

redis.call('ZADD', key, now, member)
redis.call('EXPIRE', key, ttl * 2)
return 1
";

/// Take one token from a route's bucket, returning the seconds to wait.
///
/// A token bucket rather than a fixed window. A fixed window lets a caller
/// spend a whole minute's allowance in the last second of one window and the
/// whole next allowance in the first second of the next, so a route limited to
/// 60/min serves 120 in about two seconds — right when a burst is least
/// welcome. Tokens here accrue continuously at `rate` per second, capped at
/// `burst`.
///
/// Returned as a string because Lua numbers cross the Redis protocol as
/// integers, and the whole point of the return value is its fraction.
///
/// `TIME` comes from Redis rather than the caller for the same reason the slot
/// script uses it: replicas disagree about the clock, and Redis is the one
/// thing they all agree on.
const TAKE_TOKEN: &str = r"
local key   = KEYS[1]
local rate  = tonumber(ARGV[1])
local burst = tonumber(ARGV[2])

local t   = redis.call('TIME')
local now = tonumber(t[1]) + tonumber(t[2]) / 1000000

local b      = redis.call('HMGET', key, 'tokens', 'ts')
local tokens = tonumber(b[1])
local ts     = tonumber(b[2])
if tokens == nil or ts == nil then
  tokens = burst
  ts     = now
end

tokens = math.min(burst, tokens + (now - ts) * rate)

local wait = 0
if tokens >= 1 then
  tokens = tokens - 1
else
  wait = (1 - tokens) / rate
end

redis.call('HSET', key, 'tokens', tokens, 'ts', now)
-- A bucket that has sat idle long enough to refill completely is
-- indistinguishable from one that never existed, so let it expire.
redis.call('EXPIRE', key, math.ceil(burst / rate) + 1)
return tostring(wait)
";

/// Redis, for cross-replica coordination.
///
/// Connects lazily. A gateway that refuses to boot because Redis is not up yet
/// will crash-loop every replica during a Redis restart, and will lose a race
/// with its own dependencies on a cold start. Booting and reporting
/// `ready: false` lets the load balancer route around this replica while it
/// waits, which is the behaviour the readiness probe exists to express.
#[derive(Clone)]
pub struct Cache {
    client: redis::Client,
    conn: Arc<RwLock<Option<ConnectionManager>>>,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cache")
    }
}

impl Cache {
    /// Validate the URL. Does not dial — see the type-level note.
    pub fn connect(url: &str) -> Result<Self> {
        let client =
            redis::Client::open(url).map_err(|e| Error::Internal(format!("redis url: {e}")))?;
        Ok(Self {
            client,
            conn: Arc::new(RwLock::new(None)),
        })
    }

    /// A live connection, dialling on first use and after a failed attempt.
    ///
    /// `ConnectionManager` reconnects internally once it exists, so this only
    /// has to handle "we have never successfully connected".
    async fn conn(&self) -> Result<ConnectionManager> {
        if let Some(c) = self.conn.read().await.clone() {
            return Ok(c);
        }
        let mut guard = self.conn.write().await;
        // Another task may have connected while we waited for the write lock.
        if let Some(c) = guard.clone() {
            return Ok(c);
        }
        let c = ConnectionManager::new(self.client.clone())
            .await
            .map_err(|e| Error::Internal(format!("connecting to redis: {e}")))?;
        *guard = Some(c.clone());
        Ok(c)
    }

    pub async fn ping(&self) -> bool {
        let Ok(mut conn) = self.conn().await else {
            return false;
        };
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .is_ok()
    }

    /// Try to take a concurrency slot on a credential.
    pub async fn acquire_slot(
        &self,
        account: AccountId,
        request: &str,
        limit: u32,
        ttl: Duration,
    ) -> Result<bool> {
        let mut conn = self.conn().await?;
        let taken: i64 = redis::Script::new(ACQUIRE_SLOT)
            .key(slot_key(account))
            .arg(request)
            .arg(limit)
            .arg(ttl.as_secs())
            .invoke_async(&mut conn)
            .await
            .map_err(|e| Error::Internal(format!("acquiring slot: {e}")))?;
        Ok(taken == 1)
    }

    /// Take one request's worth of rate-limit allowance for a route.
    ///
    /// `Ok(None)` means proceed. `Ok(Some(d))` means the route is over its
    /// limit and `d` is how long until a token is available.
    ///
    /// Fails **open**: if Redis is unreachable the request is allowed and a
    /// warning is logged. Throttling is a courtesy to upstream providers, not a
    /// correctness invariant — and unlike the spend caps, exceeding it cannot
    /// cost money that the ledger will not see. Refusing traffic because the
    /// coordination store blinked would trade a real outage for a theoretical
    /// one.
    pub async fn take_rate_token(&self, route: Uuid, rpm: u32) -> Result<Option<Duration>> {
        if rpm == 0 {
            return Ok(None);
        }
        let (rate, burst) = rate_and_burst(rpm);

        let mut conn = match self.conn().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, %route, "rate limiting unavailable; allowing");
                return Ok(None);
            }
        };
        let wait: String = match redis::Script::new(TAKE_TOKEN)
            .key(format!("oag:rate:{route}"))
            .arg(rate)
            .arg(burst)
            .invoke_async(&mut conn)
            .await
        {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, %route, "rate limiting unavailable; allowing");
                return Ok(None);
            }
        };

        let wait: f64 = wait.parse().unwrap_or(0.0);
        Ok((wait > 0.0).then(|| Duration::from_secs_f64(wait)))
    }

    /// Give a slot back.
    pub async fn release_slot(&self, account: AccountId, request: &str) -> Result<()> {
        let mut conn = self.conn().await?;
        let _: i64 = conn
            .zrem(slot_key(account), request)
            .await
            .map_err(|e| Error::Internal(format!("releasing slot: {e}")))?;
        Ok(())
    }

    /// How many slots a credential is currently holding.
    pub async fn slots_in_use(&self, account: AccountId) -> Result<u32> {
        let mut conn = self.conn().await?;
        let n: u32 = conn
            .zcard(slot_key(account))
            .await
            .map_err(|e| Error::Internal(format!("counting slots: {e}")))?;
        Ok(n)
    }

    /// Which credential a session is pinned to, refreshing the pin's lifetime.
    pub async fn sticky_get(&self, key: &str, ttl: Duration) -> Result<Option<AccountId>> {
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn
            .get(key)
            .await
            .map_err(|e| Error::Internal(format!("reading sticky pin: {e}")))?;
        let Some(raw) = raw else { return Ok(None) };
        // Refresh on read: an active conversation should keep its pin, and an
        // abandoned one should let go of it.
        let _: bool = conn
            .expire(key, i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX))
            .await
            .map_err(|e| Error::Internal(format!("refreshing sticky pin: {e}")))?;
        Ok(uuid::Uuid::parse_str(&raw).ok().map(AccountId::from_uuid))
    }

    pub async fn sticky_set(&self, key: &str, account: AccountId, ttl: Duration) -> Result<()> {
        let mut conn = self.conn().await?;
        let _: () = conn
            .set_ex(key, account.to_string(), ttl.as_secs())
            .await
            .map_err(|e| Error::Internal(format!("writing sticky pin: {e}")))?;
        Ok(())
    }
}

// ── auth cache (L2) ───────────────────────────────────────────────────────────

impl Cache {
    /// Read a cached auth context.
    ///
    /// Returns `None` on any failure, including Redis being down. A cache is an
    /// optimisation: if it cannot answer, the caller falls through to Postgres.
    /// Propagating an error here would turn a Redis blip into an outage.
    pub async fn auth_get(&self, hash: &str) -> Option<crate::rows::AuthContext> {
        let mut conn = self.conn().await.ok()?;
        let raw: Option<String> = conn.get(auth_key(hash)).await.ok()?;
        serde_json::from_str(&raw?).ok()
    }

    /// Cache an auth context. Best-effort for the same reason.
    pub async fn auth_set(&self, hash: &str, ctx: &crate::rows::AuthContext, ttl: Duration) {
        let Ok(mut conn) = self.conn().await else {
            return;
        };
        let Ok(json) = serde_json::to_string(ctx) else {
            return;
        };
        let _: std::result::Result<(), _> = conn.set_ex(auth_key(hash), json, ttl.as_secs()).await;
    }

    /// Evict a cached auth context, fleet-wide.
    pub async fn auth_invalidate(&self, hash: &str) {
        let Ok(mut conn) = self.conn().await else {
            return;
        };
        let _: std::result::Result<i64, _> = conn.del(auth_key(hash)).await;
    }
}

impl Cache {
    /// Drop every cached auth entry. Returns how many were removed.
    ///
    /// Scans rather than `FLUSHDB`: this key space shares Redis with
    /// concurrency slots and session pins, and dropping those would void live
    /// concurrency accounting and scatter every in-flight conversation off its
    /// pinned credential.
    pub async fn flush_auth_cache(&self) -> Result<usize> {
        let mut conn = self.conn().await?;
        let mut cursor: u64 = 0;
        let mut removed = 0usize;

        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg("oag:auth:*")
                .arg("COUNT")
                .arg(500)
                .query_async(&mut conn)
                .await
                .map_err(|e| Error::Internal(format!("scanning auth cache: {e}")))?;

            if !keys.is_empty() {
                let n: usize = conn
                    .del(&keys)
                    .await
                    .map_err(|e| Error::Internal(format!("dropping auth cache: {e}")))?;
                removed += n;
            }

            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        Ok(removed)
    }
}

// ── refresh locks ─────────────────────────────────────────────────────────────

impl Cache {
    /// Take the fleet-wide right to refresh one credential.
    ///
    /// `SET NX EX`: the first replica to ask wins, and the TTL means a replica
    /// that dies mid-refresh releases the lock rather than wedging the
    /// credential forever. Losing the race is not an error — the loser waits
    /// and re-reads what the winner wrote.
    pub async fn acquire_refresh_lock(&self, account: AccountId, ttl: Duration) -> Result<bool> {
        let mut conn = self.conn().await?;
        let acquired: Option<String> = redis::cmd("SET")
            .arg(refresh_key(account))
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(ttl.as_secs())
            .query_async(&mut conn)
            .await
            .map_err(|e| Error::Internal(format!("acquiring refresh lock: {e}")))?;
        Ok(acquired.is_some())
    }

    pub async fn release_refresh_lock(&self, account: AccountId) {
        let Ok(mut conn) = self.conn().await else {
            return;
        };
        let _: std::result::Result<i64, _> = conn.del(refresh_key(account)).await;
    }
}

fn refresh_key(account: AccountId) -> String {
    format!("oag:refresh-lock:{account}")
}

fn auth_key(hash: &str) -> String {
    format!("oag:auth:{hash}")
}

fn slot_key(account: AccountId) -> String {
    format!("oag:slots:{account}")
}

// NOTE ON WHAT IS DELIBERATELY ABSENT
//
// There is no startup cleanup here, and that is the point.
//
// sub2api runs a cleanup at every boot that removes every slot whose id does
// not carry the *current* process's randomly-regenerated prefix. With more than
// one replica that removes every slot held by every other live replica, so any
// restart, rolling deploy, or scale-up silently voids concurrency accounting
// fleet-wide until the in-flight requests drain.
//
// Slots here expire by TTL and nothing else. A replica that dies leaves its
// slots behind for at most one TTL, which is a bounded and self-healing error;
// evicting by process identity is neither.

/// Requests-per-minute expressed as a continuous refill rate and a bucket size.
///
/// Burst is the full minute's allowance: "60 requests per minute" plainly reads
/// as permission to make 60 requests, and a caller who makes them in the first
/// second has not broken the promise — they have simply spent it. What the
/// bucket prevents is spending it twice inside one minute.
fn rate_and_burst(rpm: u32) -> (f64, f64) {
    let burst = f64::from(rpm.max(1));
    (burst / 60.0, burst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpm_becomes_a_per_second_rate_and_a_full_minute_of_burst() {
        let (rate, burst) = rate_and_burst(60);
        assert!((rate - 1.0).abs() < f64::EPSILON);
        assert!((burst - 60.0).abs() < f64::EPSILON);

        // A limit of zero would mean an infinite wait rather than "no limit",
        // so the floor is one. Callers pass `rpm == 0` only by mistake; the
        // "unlimited" case is `rpm_limit IS NULL`, handled before we get here.
        let (rate, burst) = rate_and_burst(0);
        assert!(rate > 0.0 && burst >= 1.0);
    }

    /// The bucket itself, against a real Redis.
    ///
    /// Skipped when `OAG_TEST_REDIS_URL` is unset so a plain `cargo test` still
    /// works; CI sets it, so this does run there. The Lua is the part worth
    /// testing for real — the arithmetic above is trivial and the interesting
    /// behaviour is entirely inside the script.
    #[tokio::test]
    async fn a_bucket_hands_out_exactly_its_burst_then_makes_you_wait() {
        let Ok(url) = std::env::var("OAG_TEST_REDIS_URL") else {
            eprintln!("skipped: OAG_TEST_REDIS_URL unset");
            return;
        };
        let cache = Cache::connect(&url).expect("cache");
        let route = Uuid::new_v4();

        // Five per minute: five immediate, the sixth refused.
        for i in 0..5 {
            assert!(
                cache
                    .take_rate_token(route, 5)
                    .await
                    .expect("take")
                    .is_none(),
                "token {i} should have been free"
            );
        }
        let wait = cache
            .take_rate_token(route, 5)
            .await
            .expect("take")
            .expect("sixth request in a 5/min bucket must be refused");

        // One token accrues every twelve seconds at 5/min. Allow slack for the
        // fractional token earned while the loop above was running.
        assert!(
            wait > Duration::from_secs(9) && wait <= Duration::from_secs(12),
            "expected roughly a twelve second wait, got {wait:?}"
        );

        // A different route has its own bucket.
        assert!(
            cache
                .take_rate_token(Uuid::new_v4(), 5)
                .await
                .expect("take")
                .is_none(),
            "buckets must not be shared between routes"
        );
    }

    /// Fail-open is a deliberate policy choice, so it gets a test rather than
    /// just a comment. Redis being down must not turn into a 429 storm.
    #[tokio::test]
    async fn an_unreachable_redis_allows_the_request() {
        // Port 1 is reserved and nothing listens there.
        let cache = Cache::connect("redis://127.0.0.1:1").expect("lazy connect");
        assert!(
            cache
                .take_rate_token(Uuid::new_v4(), 1)
                .await
                .expect("must not surface an error")
                .is_none(),
            "a rate limiter that cannot reach Redis must allow, not refuse"
        );
    }
}
