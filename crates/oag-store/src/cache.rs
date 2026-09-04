//! Redis: what the replicas need to agree on.
//!
//! Four things live here — concurrency slots, session pins, the auth cache, and
//! route rate limiting — and all four are expendable. Losing Redis costs a
//! burst of database reads, a moment of sloppy concurrency accounting, and an
//! unthrottled minute. It never loses money or credentials, because those are
//! in Postgres.
//!
//! Expendable cuts both ways: nothing read back out of Redis is trusted on its
//! own authority. The auth cache is the one entry here that names an identity,
//! so it is authenticated with [`AuthMac`] — see that type for why a plain
//! JSON value was a privilege-escalation primitive.

use hmac::{Hmac, Mac};
use oag_core::{AccountId, Error, Result};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use sha2::Sha256;
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
return 1";

/// Count the live slots on a credential: the members `ACQUIRE_SLOT` would
/// keep, by the same clock and the same expiry.
///
/// A read, deliberately — it trims nothing. Writes stay in the acquire path
/// so a count can never race an acquire over who removes what; this simply
/// declines to count what the next acquire will remove anyway.
const SLOTS_IN_USE: &str = r"
local key = KEYS[1]
local ttl = tonumber(ARGV[1])

local now = redis.call('TIME')[1]
return redis.call('ZCOUNT', key, now - ttl, '+inf')
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
    ///
    /// Counts only members younger than `ttl` — the same expiry `acquire_slot`
    /// trims by — read against Redis's clock so replicas agree on it. A plain
    /// `ZCARD` counted expired members too, and only an *acquire* ever swept
    /// them: a credential that leaked `max_concurrency` slots (a replica that
    /// died holding them, a pump that never returned) read as full to every
    /// candidate pass, nothing tried to acquire on a full credential, and so
    /// nothing swept it — a lockout lasting until the key's own expiry, twice
    /// the TTL. Seventy minutes of a healthy credential reporting itself busy.
    pub async fn slots_in_use(&self, account: AccountId, ttl: Duration) -> Result<u32> {
        let mut conn = self.conn().await?;
        let n: u32 = redis::Script::new(SLOTS_IN_USE)
            .key(slot_key(account))
            .arg(ttl.as_secs())
            .invoke_async(&mut conn)
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

/// Domain separation, so a tag minted for an auth entry cannot be replayed as
/// anything else that might one day be signed with the same secret.
const AUTH_MAC_DOMAIN: &[u8] = b"oag:auth-cache:v1";
/// Envelope prefix. Versioned so a future format change is a miss on the old
/// entries rather than a garbled parse of them.
const AUTH_ENVELOPE_PREFIX: &str = "v1.";

/// Authenticates auth-cache entries with `security.signing_secret`.
///
/// The L2 auth cache used to hold a bare JSON `AuthContext` keyed by the hash
/// of the inbound key, and a hit was taken as proof of identity. Redis is not
/// proof of anything: anyone who can `SET` — a shared or unauthenticated Redis,
/// a compromised sidecar, another tenant of the same instance — could write
/// `oag:auth:{sha256(their own key)}` and choose which principal, route and
/// budget it named, `admin: true` included. Nothing downstream re-checked,
/// because the tiers exist precisely so that a hit skips Postgres.
///
/// So every entry carries an HMAC-SHA256 tag, and the key hash is part of the
/// signed message. Signing only the JSON would still let someone copy a
/// legitimately-signed admin entry sideways onto their own key's slot, which is
/// the same attack with an extra step.
///
/// A tag that does not verify is a **miss**, not an error: the caller falls
/// through to Postgres and gets the right answer. That is also what makes this
/// deployable — entries written by an older binary are unsigned, so they are
/// simply ignored until they expire.
#[derive(Clone)]
pub struct AuthMac {
    key: Box<[u8]>,
}

impl std::fmt::Debug for AuthMac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthMac")
    }
}

impl AuthMac {
    /// Keyed with `security.signing_secret`, which is already required at boot
    /// and already required to be identical on every replica — which is exactly
    /// the property a fleet-wide cache MAC needs.
    #[must_use]
    pub fn new(signing_secret: &str) -> Self {
        Self {
            key: signing_secret.as_bytes().into(),
        }
    }

    /// The tag is over `domain ‖ hash ‖ json`, NUL-separated. `serde_json`
    /// escapes control characters, and the hash is hex, so no NUL can appear
    /// inside a field and the framing stays unambiguous.
    fn primed(&self, hash: &str, json: &str) -> Option<Hmac<Sha256>> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).ok()?;
        mac.update(AUTH_MAC_DOMAIN);
        mac.update(&[0]);
        mac.update(hash.as_bytes());
        mac.update(&[0]);
        mac.update(json.as_bytes());
        Some(mac)
    }

    /// Serialise and tag a context for storage under `hash`.
    #[must_use]
    pub fn seal(&self, hash: &str, ctx: &crate::rows::AuthContext) -> Option<String> {
        let json = serde_json::to_string(ctx).ok()?;
        let tag = self.primed(hash, &json)?.finalize().into_bytes();
        Some(format!("{AUTH_ENVELOPE_PREFIX}{}.{json}", hex::encode(tag)))
    }

    /// Verify and parse an entry stored under `hash`.
    ///
    /// `None` for an unsigned, forged, tampered, misfiled or truncated entry —
    /// every one of them indistinguishable from a cache miss to the caller.
    #[must_use]
    pub fn open(&self, hash: &str, sealed: &str) -> Option<crate::rows::AuthContext> {
        // The tag is hex, so the first `.` after it is the separator; the JSON
        // may well contain more of them inside decimal amounts.
        let (tag, json) = sealed.strip_prefix(AUTH_ENVELOPE_PREFIX)?.split_once('.')?;
        let tag = hex::decode(tag).ok()?;
        // `verify_slice` compares in constant time.
        self.primed(hash, json)?.verify_slice(&tag).ok()?;
        serde_json::from_str(json).ok()
    }
}

impl Cache {
    /// Read a cached auth context, if one is there and it is ours.
    ///
    /// Returns `None` on any failure, including Redis being down and including
    /// a bad MAC. A cache is an optimisation: if it cannot answer, the caller
    /// falls through to Postgres. Propagating an error here would turn a Redis
    /// blip into an outage — and, for the MAC case, would turn a forged entry
    /// into a way to make requests fail rather than a way to make them pass.
    pub async fn auth_get(&self, hash: &str, mac: &AuthMac) -> Option<crate::rows::AuthContext> {
        let mut conn = self.conn().await.ok()?;
        let raw: Option<String> = conn.get(auth_key(hash)).await.ok()?;
        let raw = raw?;
        let ctx = mac.open(hash, &raw);
        if ctx.is_none() {
            // Worth a line: the only innocent explanation is an entry written
            // before this binary, or a `signing_secret` that has just changed.
            // Otherwise someone is writing to our key space. The hash is a
            // digest of a live credential, so it is not logged.
            tracing::warn!("discarding an auth cache entry that failed authentication");
        }
        ctx
    }

    /// Cache an auth context. Best-effort for the same reason.
    pub async fn auth_set(
        &self,
        hash: &str,
        ctx: &crate::rows::AuthContext,
        ttl: Duration,
        mac: &AuthMac,
    ) {
        let Ok(mut conn) = self.conn().await else {
            return;
        };
        let Some(sealed) = mac.seal(hash, ctx) else {
            return;
        };
        let _: std::result::Result<(), _> =
            conn.set_ex(auth_key(hash), sealed, ttl.as_secs()).await;
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
    use crate::rows::AuthContext;
    use rust_decimal::Decimal;

    const SECRET: &str = "an-adequately-long-test-signing-secret-000000";

    fn ctx(admin: bool) -> AuthContext {
        AuthContext {
            api_key_id: Uuid::new_v4(),
            principal_id: Uuid::new_v4(),
            route_id: Uuid::new_v4(),
            key_floor_tier: None,
            admin,
            quota_usd: None,
            spent_usd: Decimal::ZERO,
            principal_budget_usd: None,
            principal_hard_stop_multiple: Decimal::ONE,
            principal_spent_usd: Decimal::ZERO,
        }
    }

    /// The MAC, without Redis in the way. Everything the Redis test asserts
    /// about a hit reduces to these, and these run on a bare `cargo test`.
    #[test]
    fn only_an_entry_we_signed_for_this_key_opens() {
        let mac = AuthMac::new(SECRET);
        let hash = crate::repo::hash_key("sk-victim");
        let sealed = mac.seal(&hash, &ctx(true)).expect("seal");

        assert!(
            mac.open(&hash, &sealed).expect("round trip").admin,
            "our own entry must open, admin flag intact"
        );

        // The original bug: a bare JSON value was accepted as an identity, so
        // anyone able to SET could mint one.
        let bare = serde_json::to_string(&ctx(true)).expect("json");
        assert!(
            mac.open(&hash, &bare).is_none(),
            "an unsigned entry must not open"
        );

        // Tampering: keep our tag, swap the payload for a more generous one.
        let (tag, _) = sealed
            .strip_prefix(AUTH_ENVELOPE_PREFIX)
            .expect("prefix")
            .split_once('.')
            .expect("tag");
        let tampered = format!("{AUTH_ENVELOPE_PREFIX}{tag}.{bare}");
        assert!(
            mac.open(&hash, &tampered).is_none(),
            "a payload swapped under a valid-looking tag must not open"
        );

        // Sideways replay: our own signed admin entry, refiled under the
        // attacker's key hash. This is why the hash is inside the MAC and not
        // just the Redis key name.
        assert!(
            mac.open(&crate::repo::hash_key("sk-attacker"), &sealed)
                .is_none(),
            "an entry signed for another key hash must not open"
        );

        // A replica configured with a different secret is not us.
        assert!(
            AuthMac::new("a-completely-different-but-long-enough-secret")
                .open(&hash, &sealed)
                .is_none(),
            "another secret must not open our entry"
        );

        // Garbage in the envelope is a miss, never a panic.
        for junk in ["", "v1.", "v1.zz.{}", "v2.00.{}", "not-an-envelope"] {
            assert!(mac.open(&hash, junk).is_none(), "{junk:?} must not open");
        }
    }

    /// The same forgery against a real Redis, through the accessor the auth
    /// path actually calls. Skipped without `OAG_TEST_REDIS_URL`; the unit test
    /// above covers the logic when it is unset.
    #[tokio::test]
    async fn forged_unsigned_auth_cache_entry_is_ignored() {
        let Ok(url) = std::env::var("OAG_TEST_REDIS_URL") else {
            eprintln!("skipped: OAG_TEST_REDIS_URL unset");
            return;
        };
        let cache = Cache::connect(&url).expect("cache");
        let mac = AuthMac::new(SECRET);
        let hash = crate::repo::hash_key(&format!("sk-attacker-{}", Uuid::new_v4()));

        // Exactly what an attacker with SET access would write: the format this
        // cache used to accept, naming an admin identity of their choosing.
        let forged = serde_json::to_string(&ctx(true)).expect("json");
        let mut conn = cache.conn().await.expect("conn");
        let _: () = conn
            .set_ex(auth_key(&hash), forged, 60)
            .await
            .expect("plant");

        assert!(
            cache.auth_get(&hash, &mac).await.is_none(),
            "an unsigned Redis entry must read as a cache miss, not as an identity"
        );

        // And the honest path still works, so the check is not simply refusing
        // everything.
        let real = ctx(false);
        cache
            .auth_set(&hash, &real, Duration::from_mins(1), &mac)
            .await;
        let got = cache
            .auth_get(&hash, &mac)
            .await
            .expect("our own entry must come back");
        assert_eq!(got.api_key_id, real.api_key_id);
        assert!(!got.admin);

        cache.auth_invalidate(&hash).await;
    }

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
    /// Skipped when `OAG_TEST_REDIS_URL` is unset, like the other Redis tests.
    #[tokio::test]
    async fn expired_slot_members_do_not_count_as_in_use() {
        // The lockout. Eight members older than the TTL — a replica that died
        // holding them — and a `ZCARD` reported eight in flight on a
        // credential with `max_concurrency: 8`. Nothing acquired on a full
        // credential, so nothing ever ran the sweep that lives in the acquire
        // script, and the credential stayed "full" until the key's own expiry
        // at twice the TTL. The count now applies the acquire's own expiry.
        let Ok(url) = std::env::var("OAG_TEST_REDIS_URL") else {
            eprintln!("skipped: OAG_TEST_REDIS_URL unset");
            return;
        };
        let cache = Cache::connect(&url).expect("cache");
        let account = AccountId::new();
        let ttl = Duration::from_mins(1);

        let mut conn = cache.conn().await.expect("conn");
        let (now, _): (i64, i64) = redis::cmd("TIME")
            .query_async(&mut conn)
            .await
            .expect("time");
        for i in 0..8 {
            let _: () = conn
                .zadd(slot_key(account), format!("dead-{i}"), now - 3600)
                .await
                .expect("plant a stale member");
        }
        let _: () = conn
            .zadd(slot_key(account), "live", now)
            .await
            .expect("plant a live member");

        assert_eq!(
            cache.slots_in_use(account, ttl).await.expect("count"),
            1,
            "only the member inside the TTL is in use"
        );

        // And the credential is still acquirable: the stale members are not
        // standing in the way of the slot they used to hold.
        assert!(
            cache
                .acquire_slot(account, "fresh", 2, ttl)
                .await
                .expect("acquire"),
            "one live member and a limit of two leaves room"
        );
        let _: () = conn.del(slot_key(account)).await.expect("cleanup");
    }

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
