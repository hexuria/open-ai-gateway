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
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
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
/// This used to be a read (`ZCOUNT` of scores inside the window) so a count
/// could never race an acquire over who removes what. The race was imaginary
/// for members both sides already treat as dead — they only trim scores at or
/// before `now - ttl`, which acquire would drop on the next take anyway — and
/// the read left expired members sitting in the key until something acquired.
/// A full credential never acquired, so nothing swept it; operators staring
/// at `ZCARD` saw ghosts the scheduler had already stopped counting.
///
/// Trim first, then `ZCARD`. The acquire's trim is inclusive at `now - ttl`,
/// so this uses the same command rather than an exclusive `ZCOUNT` that had
/// to explain a boundary the two sides already agree on.
const SLOTS_IN_USE: &str = r"
local key = KEYS[1]
local ttl = tonumber(ARGV[1])

local now = redis.call('TIME')[1]
redis.call('ZREMRANGEBYSCORE', key, '-inf', now - ttl)
return redis.call('ZCARD', key)
";

/// Refresh a live slot's score so a short TTL can outlive a long stream.
///
/// No-ops if the member is gone: an admin clear, a release, or a trim must
/// not be undone by a heartbeat that lost the race.
const REFRESH_SLOT: &str = r"
local key    = KEYS[1]
local member = ARGV[1]
local ttl    = tonumber(ARGV[2])

if redis.call('ZSCORE', key, member) == false then
  return 0
end

local now = redis.call('TIME')[1]
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

/// The scripts, prepared once. `redis::Script::new` copies the source and
/// hashes it for `EVALSHA`; doing that per call was a SHA-1 over a few hundred
/// bytes on every slot acquire, every count and every rate token.
static ACQUIRE_SLOT_SCRIPT: std::sync::LazyLock<redis::Script> =
    std::sync::LazyLock::new(|| redis::Script::new(ACQUIRE_SLOT));
static SLOTS_IN_USE_SCRIPT: std::sync::LazyLock<redis::Script> =
    std::sync::LazyLock::new(|| redis::Script::new(SLOTS_IN_USE));
static REFRESH_SLOT_SCRIPT: std::sync::LazyLock<redis::Script> =
    std::sync::LazyLock::new(|| redis::Script::new(REFRESH_SLOT));
static TAKE_TOKEN_SCRIPT: std::sync::LazyLock<redis::Script> =
    std::sync::LazyLock::new(|| redis::Script::new(TAKE_TOKEN));

/// How long a slot Redis round trip may take before we drop the cached
/// connection and surface an error.
///
/// Completions that hang inside `ConnectionManager` look like an empty reply
/// to the client (`RemoteDisconnected`) while `/v1/models` and liveness still
/// answer. Two seconds is well above a healthy RTT and well below a client
/// timeout; the caller fail-opens, which is the same answer an unreachable
/// Redis already gets.
const SLOT_OP_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// Bumped every time a connection is established. A caller that timed
    /// out captures it first and may only drop the connection it timed out
    /// on; without this, hundreds of callers timing out on one wedged socket
    /// each destroyed whatever healthy connection had been built since.
    conn_gen: Arc<std::sync::atomic::AtomicU64>,
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
            conn_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
        // Bounded reconnects: the crate default is six exponential retries,
        // and every in-flight command waits on that future. Completions that
        // share this manager then look empty/`RemoteDisconnected` while
        // `/v1/models` still answers. Two tries, two-second caps; the caller
        // fail-opens.
        let cfg = ConnectionManagerConfig::new()
            .set_response_timeout(Some(SLOT_OP_TIMEOUT))
            .set_connection_timeout(Some(SLOT_OP_TIMEOUT))
            .set_number_of_retries(2);
        let c = ConnectionManager::new_with_config(self.client.clone(), cfg)
            .await
            .map_err(|e| Error::Internal(format!("connecting to redis: {e}")))?;
        *guard = Some(c.clone());
        self.conn_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(c)
    }

    /// Forget the cached connection so the next call redials.
    ///
    /// A timed-out command can leave `ConnectionManager` holding a socket that
    /// will never answer; keeping it would hang every subsequent slot op on
    /// this replica, which is the completions wedge.
    async fn drop_conn(&self, expected_gen: u64) {
        let mut guard = self.conn.write().await;
        if self.conn_gen.load(std::sync::atomic::Ordering::SeqCst) != expected_gen {
            // Already replaced by a newer connection; the one that timed out is gone.
            return;
        }
        *guard = None;
    }

    /// Run a slot Redis op with a deadline. Timeout drops the connection.
    async fn slot_timed<T>(
        &self,
        op: &'static str,
        fut: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let generation = self.conn_gen.load(std::sync::atomic::Ordering::SeqCst);
        match tokio::time::timeout(SLOT_OP_TIMEOUT, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                // redis-rs spells its own timeout "timed out"; the old check for
                // "timeout" never matched it, so this branch was dead.
                let text = e.to_string().to_ascii_lowercase();
                if text.contains("timeout") || text.contains("timed out") {
                    tracing::warn!(
                        op,
                        error = %e,
                        "redis slot op timed out; dropping the cached connection"
                    );
                    self.drop_conn(generation).await;
                }
                Err(e)
            }
            Err(_) => {
                tracing::warn!(
                    op,
                    "redis slot op timed out; dropping the cached connection"
                );
                self.drop_conn(generation).await;
                Err(Error::Internal(format!("{op}: redis timed out")))
            }
        }
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
        self.slot_timed("acquiring slot", async {
            let mut conn = self.conn().await?;
            let mut inv = ACQUIRE_SLOT_SCRIPT.key(slot_key(account));
            inv.arg(request).arg(limit).arg(ttl.as_secs());
            let taken: i64 = eval_slot_script(&mut conn, "acquiring slot", &inv).await?;
            Ok(taken == 1)
        })
        .await
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

        match self
            .slot_timed("taking rate token", async {
                let mut conn = self.conn().await?;
                let mut inv = TAKE_TOKEN_SCRIPT.key(format!("oag:rate:{route}"));
                inv.arg(rate).arg(burst);
                let wait: String = eval_slot_script(&mut conn, "taking rate token", &inv).await?;
                Ok(wait_from_redis(&wait))
            })
            .await
        {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(error = %e, %route, "rate limiting unavailable; allowing");
                Ok(None)
            }
        }
    }

    /// Give a slot back.
    pub async fn release_slot(&self, account: AccountId, request: &str) -> Result<()> {
        self.slot_timed("releasing slot", async {
            let mut conn = self.conn().await?;
            let _: i64 = conn
                .zrem(slot_key(account), request)
                .await
                .map_err(|e| Error::Internal(format!("releasing slot: {e}")))?;
            Ok(())
        })
        .await
    }

    /// Keep a held slot from ageing out of the TTL window.
    ///
    /// `Ok(true)` if the member was still there and its score moved. `Ok(false)`
    /// if it was already gone — release, trim, or an admin clear won.
    pub async fn refresh_slot(
        &self,
        account: AccountId,
        request: &str,
        ttl: Duration,
    ) -> Result<bool> {
        self.slot_timed("refreshing slot", async {
            let mut conn = self.conn().await?;
            let mut inv = REFRESH_SLOT_SCRIPT.key(slot_key(account));
            inv.arg(request).arg(ttl.as_secs());
            let kept: i64 = eval_slot_script(&mut conn, "refreshing slot", &inv).await?;
            Ok(kept == 1)
        })
        .await
    }

    /// Drop every slot member for a credential. Returns how many were there,
    /// expired included — the operator asked to clear the key, not to apply
    /// the scheduler's window to it.
    pub async fn clear_slots(&self, account: AccountId) -> Result<u32> {
        self.slot_timed("clearing slots", async {
            let mut conn = self.conn().await?;
            let key = slot_key(account);
            let n: i64 = conn
                .zcard(&key)
                .await
                .map_err(|e| Error::Internal(format!("counting slots to clear: {e}")))?;
            let _: i64 = conn
                .del(&key)
                .await
                .map_err(|e| Error::Internal(format!("clearing slots: {e}")))?;
            Ok(u32::try_from(n.max(0)).unwrap_or(u32::MAX))
        })
        .await
    }

    /// How many slots a credential is currently holding.
    ///
    /// Trims members older than `ttl` — the same expiry `acquire_slot` trims
    /// by — then `ZCARD`s what remains, against Redis's clock so replicas
    /// agree. A plain `ZCARD` counted expired members too, and only an
    /// *acquire* ever swept them: a credential that leaked `max_concurrency`
    /// slots (a replica that died holding them, a pump that never returned)
    /// read as full to every candidate pass, nothing tried to acquire on a
    /// full credential, and so nothing swept it — a lockout lasting until the
    /// key's own expiry, twice the TTL.
    ///
    /// The count itself now does the trim, so a background sweep (or a
    /// candidate pass that finds the credential idle) can clear ghosts
    /// without waiting for a successful acquire.
    pub async fn slots_in_use(&self, account: AccountId, ttl: Duration) -> Result<u32> {
        self.slot_timed("counting slots", async {
            let mut conn = self.conn().await?;
            let mut inv = SLOTS_IN_USE_SCRIPT.key(slot_key(account));
            inv.arg(ttl.as_secs());
            eval_slot_script(&mut conn, "counting slots", &inv).await
        })
        .await
    }

    /// `slots_in_use` for several credentials in one round trip, in order.
    ///
    /// Selection asks about every candidate before choosing one. Asked one at
    /// a time that was a sequential Redis round trip per credential per
    /// attempt — and re-run per failover and per lost race, so a pool of
    /// twenty credentials in a failover storm was hundreds of serial round
    /// trips on one request. One pipeline, one round trip, however many.
    pub async fn slots_in_use_many(
        &self,
        accounts: &[AccountId],
        ttl: Duration,
    ) -> Result<Vec<u32>> {
        if accounts.is_empty() {
            return Ok(Vec::new());
        }
        self.slot_timed("counting slots", async {
            let mut conn = self.conn().await?;
            let mut pipe = redis::pipe();
            for account in accounts {
                pipe.invoke_script(
                    SLOTS_IN_USE_SCRIPT
                        .key(slot_key(*account))
                        .arg(ttl.as_secs()),
                );
            }
            let mut last = None;
            for _ in 0..4 {
                match pipe.query_async::<Vec<u32>>(&mut conn).await {
                    Ok(counts) => return Ok(counts),
                    // A single invocation loads the script on NOSCRIPT and retries; a
                    // pipeline does not. Straight after a Redis restart, a SCRIPT
                    // FLUSH, or on a node that has never seen this script, every
                    // count would otherwise fail — and selection would run open (see
                    // `slot_accounting_degraded`) until some other path happened to
                    // load it. Load and go again; a flush between load and eval
                    // is why this is a loop, not a single retry.
                    Err(e) if is_noscript(&e) => {
                        SLOTS_IN_USE_SCRIPT
                            .load_async(&mut conn)
                            .await
                            .map_err(|e| Error::Internal(format!("loading slot script: {e}")))?;
                        last = Some(e);
                    }
                    Err(e) => {
                        return Err(Error::Internal(format!("counting slots: {e}")));
                    }
                }
            }
            Err(Error::Internal(format!(
                "counting slots: {}",
                last.map_or_else(|| "NoScript".into(), |e| e.to_string())
            )))
        })
        .await
    }

    /// Which credential a session is pinned to, refreshing the pin's lifetime.
    pub async fn sticky_get(&self, key: &str, ttl: Duration) -> Result<Option<AccountId>> {
        self.slot_timed("reading sticky pin", async {
            let mut conn = self.conn().await?;
            // Refresh on read: an active conversation should keep its pin, and an
            // abandoned one should let go of it. `GETEX` does both in one round
            // trip; this was a GET and an EXPIRE, on every request with a pin.
            let raw: Option<String> = conn
                .get_ex(key, redis::Expiry::EX(ttl.as_secs()))
                .await
                .map_err(|e| Error::Internal(format!("reading sticky pin: {e}")))?;
            let Some(raw) = raw else { return Ok(None) };
            Ok(uuid::Uuid::parse_str(&raw).ok().map(AccountId::from_uuid))
        })
        .await
    }

    pub async fn sticky_set(&self, key: &str, account: AccountId, ttl: Duration) -> Result<()> {
        self.slot_timed("writing sticky pin", async {
            let mut conn = self.conn().await?;
            let _: () = conn
                .set_ex(key, account.to_string(), ttl.as_secs())
                .await
                .map_err(|e| Error::Internal(format!("writing sticky pin: {e}")))?;
            Ok(())
        })
        .await
    }
}

fn is_noscript(err: &redis::RedisError) -> bool {
    err.kind() == redis::ErrorKind::Server(redis::ServerErrorKind::NoScript)
}

/// `EVALSHA`, loading the script if Redis has forgotten it.
///
/// `invoke_async` already loads and retries once. A `SCRIPT FLUSH` (restart,
/// failover, a parallel test) between that load and the retry still returns
/// `NoScript` to us. A few more attempts are cheaper than treating an empty
/// script cache as a fatal acquire.
async fn eval_slot_script<T>(
    conn: &mut ConnectionManager,
    op: &'static str,
    invocation: &redis::ScriptInvocation<'_>,
) -> Result<T>
where
    T: redis::FromRedisValue,
{
    let mut last = None;
    for _ in 0..4 {
        match invocation.invoke_async(conn).await {
            Ok(v) => return Ok(v),
            Err(e) if is_noscript(&e) => {
                if let Err(load_err) = invocation.load_async(conn).await {
                    return Err(Error::Internal(format!("{op}: loading script: {load_err}")));
                }
                last = Some(e);
            }
            Err(e) => return Err(Error::Internal(format!("{op}: {e}"))),
        }
    }
    Err(Error::Internal(format!(
        "{op}: {}",
        last.map_or_else(|| "NoScript".into(), |e| e.to_string())
    )))
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

    /// Evict a cached auth context, fleet-wide. `Err` when the cache could not
    /// be reached or the DEL failed.
    ///
    /// The result matters here in a way it does not for a cache write. This is
    /// called on the revocation path, where "the shared cache is clear" is
    /// something the CLI goes on to *tell an operator* during an incident. It
    /// returned `()` and swallowed both failures, so a Redis that was
    /// unreachable produced the same output as one that had dropped the key,
    /// and the operator was told the residue expires in fifteen seconds when it
    /// was five minutes.
    ///
    /// Callers that are merely keeping the cache tidy may still ignore this.
    pub async fn auth_invalidate(&self, hash: &str) -> Result<()> {
        let mut conn = self.conn().await?;
        let _: i64 = conn
            .del(auth_key(hash))
            .await
            .map_err(|e| Error::Internal(format!("evicting a cached identity: {e}")))?;
        Ok(())
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
// evicting by process identity is neither. "At most one TTL" holds because
// `slots_in_use` trims by the same expiry the acquire trims by — before it
// did, a leaked slot stood in the count until the key's own EXPIRE at twice
// the TTL, and nothing acquiring on a "full" credential ever ran the trim.
// An operator who cannot wait that long has `clear_slots`.

/// The wait a rate-limit script asked for, or `None` for no wait.
///
/// `try_from_secs_f64`, not `from_secs_f64`, which panics on a non-finite or
/// out-of-range value. This number comes back from a Lua script in Redis, so it
/// is data from another process — and a panic here is a 500 on a request the
/// rate limiter was only meant to delay. A value we cannot make a duration of
/// means "no wait", the same answer an unparseable one already gets.
///
/// A named function rather than three lines at the call site, because the call
/// site needs a live Redis and a script that returns the value in question.
/// The test used to reimplement these lines, so deleting `try_from_secs_f64`
/// from the real code left it green — a copy of the fix cannot fail with it.
fn wait_from_redis(raw: &str) -> Option<Duration> {
    let wait: f64 = raw.parse().unwrap_or(0.0);
    (wait > 0.0)
        .then(|| Duration::try_from_secs_f64(wait).ok())
        .flatten()
}

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
            principal_budget_usd: None,
            principal_hard_stop_multiple: Decimal::ONE,
            expires_at: None,
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

        let _ = cache.auth_invalidate(&hash).await;
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
    async fn a_pipelined_count_survives_a_redis_that_has_not_seen_the_script() {
        // SCRIPT FLUSH is what a restart, a failover to a fresh replica, or a
        // new cluster node looks like to EVALSHA: NOSCRIPT. The single-key
        // count loads and retries on its own; the pipeline had to be taught
        // to, or every count after a restart failed until a pinned request
        // happened to load the script through the other path.
        let Ok(url) = std::env::var("OAG_TEST_REDIS_URL") else {
            eprintln!("skipped: OAG_TEST_REDIS_URL unset");
            return;
        };
        let cache = Cache::connect(&url).expect("cache");
        let (a, b) = (AccountId::new(), AccountId::new());
        let ttl = Duration::from_mins(1);

        let mut conn = cache.conn().await.expect("conn");
        let _: () = redis::cmd("SCRIPT")
            .arg("FLUSH")
            .query_async(&mut conn)
            .await
            .expect("flush the script cache");
        assert!(
            cache
                .acquire_slot(a, "live", 4, ttl)
                .await
                .expect("acquire"),
            "one live member on a"
        );
        let _: () = redis::cmd("SCRIPT")
            .arg("FLUSH")
            .query_async(&mut conn)
            .await
            .expect("flush again, so the count itself meets NOSCRIPT");

        assert_eq!(
            cache.slots_in_use_many(&[a, b], ttl).await.expect("count"),
            vec![1, 0],
            "loaded on NOSCRIPT and answered in order"
        );
        let _: () = conn.del(slot_key(a)).await.expect("cleanup");
    }

    #[tokio::test]
    async fn acquire_and_count_survive_repeated_script_flush() {
        // Production: a failover. Tests: the pipelined NOSCRIPT case running
        // next to this crate's other Redis tests. Either way EVALSHA can lose
        // the script between load and retry; acquire used to surface that as
        // a fatal Internal and a seat stayed empty-and-full at once.
        let Ok(url) = std::env::var("OAG_TEST_REDIS_URL") else {
            eprintln!("skipped: OAG_TEST_REDIS_URL unset");
            return;
        };
        let cache = Cache::connect(&url).expect("cache");
        let account = AccountId::new();
        let ttl = Duration::from_mins(1);
        let mut conn = cache.conn().await.expect("conn");
        for i in 0..8 {
            let _: () = redis::cmd("SCRIPT")
                .arg("FLUSH")
                .query_async(&mut conn)
                .await
                .expect("flush");
            assert!(
                cache
                    .acquire_slot(account, &format!("m{i}"), 16, ttl)
                    .await
                    .unwrap_or_else(|e| panic!("acquire {i}: {e}")),
                "member {i}"
            );
        }
        let _: () = redis::cmd("SCRIPT")
            .arg("FLUSH")
            .query_async(&mut conn)
            .await
            .expect("flush before count");
        assert_eq!(
            cache.slots_in_use(account, ttl).await.expect("count"),
            8,
            "every flushed acquire still left a live member"
        );
        assert_eq!(cache.clear_slots(account).await.expect("clear"), 8);
        assert_eq!(cache.slots_in_use(account, ttl).await.expect("empty"), 0);
    }

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
        // The boundary itself. The acquire script sweeps members at or
        // before `now - ttl`, so the count has to exclude exactly that
        // score and include the one after it, or the two disagree about a
        // member on the line and a credential reads one slot fuller than
        // the acquire will find it.
        //
        // The two sides of that boundary are not equally safe to pin, and
        // treating them as if they were is what made this test flaky. `now` is
        // read once, here; the window's edge keeps moving while the test runs.
        // A member planted at exactly `now - ttl` only gets *older* relative to
        // that edge, so it stays excluded however slow the run — the exclusive
        // side can be pinned to the second. A member planted just inside falls
        // out of the window as soon as the run takes longer than its margin,
        // and at one second of margin, ordinary parallel load was enough: the
        // count came back 1 and the failure read as a regression in the Lua.
        //
        // Five seconds, which is far outside any scheduling delay this test can
        // suffer and far inside a one-minute TTL — it is still 55 seconds from
        // `now`, so it cannot pass by the count simply admitting recent members.
        let inside_margin_secs: i64 = 5;
        let ttl_secs = i64::try_from(ttl.as_secs()).expect("fits");
        let _: () = conn
            .zadd(slot_key(account), "on-the-line", now - ttl_secs)
            .await
            .expect("plant a member at exactly now - ttl");
        let _: () = conn
            .zadd(
                slot_key(account),
                "just-inside",
                now - ttl_secs + inside_margin_secs,
            )
            .await
            .expect("plant a member inside the window");

        assert_eq!(
            cache.slots_in_use(account, ttl).await.expect("count"),
            2,
            "the live member and the one just inside; not the eight, not the one on the line"
        );

        // And the credential is still acquirable: the stale members are not
        // standing in the way of the slot they used to hold.
        assert!(
            cache
                .acquire_slot(account, "fresh", 3, ttl)
                .await
                .expect("acquire"),
            "two live members and a limit of three leaves room"
        );
        let _: () = conn.del(slot_key(account)).await.expect("cleanup");
    }

    #[tokio::test]
    async fn an_absent_slot_key_counts_as_zero_in_flight() {
        // Redis empty is idle, not full. The production pain was a replica
        // whose gauge still showed max concurrency after the key was gone —
        // operators chased ghosts, and if selection trusted a stale view the
        // seat stayed at_capacity until restart. The count is Redis, and an
        // absent key is zero.
        let Ok(url) = std::env::var("OAG_TEST_REDIS_URL") else {
            eprintln!("skipped: OAG_TEST_REDIS_URL unset");
            return;
        };
        let cache = Cache::connect(&url).expect("cache");
        let account = AccountId::new();
        let ttl = Duration::from_mins(1);
        let mut conn = cache.conn().await.expect("conn");
        let _: () = conn.del(slot_key(account)).await.expect("absent");
        assert_eq!(
            cache.slots_in_use(account, ttl).await.expect("count"),
            0,
            "a missing key is zero in flight"
        );
        assert_eq!(
            cache
                .slots_in_use_many(&[account], ttl)
                .await
                .expect("pipelined"),
            vec![0]
        );
    }

    #[tokio::test]
    async fn clear_slots_drops_the_key_and_counts_as_zero() {
        let Ok(url) = std::env::var("OAG_TEST_REDIS_URL") else {
            eprintln!("skipped: OAG_TEST_REDIS_URL unset");
            return;
        };
        let cache = Cache::connect(&url).expect("cache");
        let account = AccountId::new();
        let ttl = Duration::from_mins(1);
        assert!(
            cache
                .acquire_slot(account, "live-a", 8, ttl)
                .await
                .expect("acquire a")
        );
        assert!(
            cache
                .acquire_slot(account, "live-b", 8, ttl)
                .await
                .expect("acquire b")
        );
        assert_eq!(cache.slots_in_use(account, ttl).await.expect("count"), 2);

        let dropped = cache.clear_slots(account).await.expect("clear");
        assert_eq!(dropped, 2, "both live members were in the key");
        assert_eq!(
            cache.slots_in_use(account, ttl).await.expect("count after"),
            0,
            "Redis empty ⇒ in_flight 0"
        );

        // A heartbeat after a clear must not recreate the member: the operator
        // asked the seat to be empty, including under a still-running request.
        assert!(
            !cache
                .refresh_slot(account, "live-a", ttl)
                .await
                .expect("refresh"),
            "refresh of a cleared member is a no-op"
        );
        assert_eq!(cache.slots_in_use(account, ttl).await.expect("still 0"), 0);
    }

    #[tokio::test]
    async fn refresh_keeps_a_live_member_and_ignores_a_released_one() {
        let Ok(url) = std::env::var("OAG_TEST_REDIS_URL") else {
            eprintln!("skipped: OAG_TEST_REDIS_URL unset");
            return;
        };
        let cache = Cache::connect(&url).expect("cache");
        let account = AccountId::new();
        let ttl = Duration::from_mins(1);
        assert!(
            cache
                .acquire_slot(account, "held", 2, ttl)
                .await
                .expect("acquire")
        );
        assert!(
            cache
                .refresh_slot(account, "held", ttl)
                .await
                .expect("refresh live"),
            "a held member is refreshed"
        );
        cache.release_slot(account, "held").await.expect("release");
        assert!(
            !cache
                .refresh_slot(account, "held", ttl)
                .await
                .expect("refresh gone"),
            "a released member is not revived"
        );
        let _: () = cache
            .conn()
            .await
            .expect("conn")
            .del(slot_key(account))
            .await
            .expect("cleanup");
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

        // Five per minute: a burst of five, then a refusal.
        //
        // Counted against what **Redis** recorded, not against what the API
        // returned. `take_rate_token` answers `Ok(None)` for two different
        // things — "here is your token" and "Redis is unreachable, so I am
        // failing open" — and that conflation is deliberate and right in
        // production: refusing traffic because the coordination store blinked
        // trades a real outage for a theoretical one. It is fatal to a test
        // that counts `None`s, though. On CI this handed out **seven tokens in
        // 7.7 milliseconds** — not a slow runner earning extras, which was my
        // first and wrong reading of it, but five real grants and two
        // fail-opens wearing the same return value.
        //
        // The bucket's own `tokens` field cannot be faked that way: it moves
        // only when the script actually ran.
        let started = std::time::Instant::now();
        let mut granted = 0_u64;
        let wait = loop {
            match cache.take_rate_token(route, 5).await.expect("take") {
                None => {
                    granted += 1;
                    assert!(
                        granted <= 60,
                        "a 5/min bucket never refused in {granted} calls"
                    );
                }
                Some(wait) => break wait,
            }
        };

        // Ground truth, read straight from the bucket.
        let mut raw = redis::Client::open(url.as_str())
            .expect("client")
            .get_multiplexed_async_connection()
            .await
            .expect(
                "a connection of our own: if Redis is unreachable here, \
                     every `None` above may have been a fail-open and this \
                     test proved nothing",
            );
        let tokens: Option<String> = redis::cmd("HGET")
            .arg(format!("oag:rate:{route}"))
            .arg("tokens")
            .query_async(&mut raw)
            .await
            .expect("read the bucket");
        let tokens: f64 = tokens
            .expect("the bucket exists, so the script really ran")
            .parse()
            .expect("a number");

        assert!(
            tokens < 1.0,
            "it refused while holding {tokens} tokens, which is not a refusal \
             at all — the limiter is not decrementing"
        );
        assert!(
            granted >= 5,
            "the burst is five; only {granted} were granted in {:?}",
            started.elapsed()
        );
        assert!(
            wait > Duration::ZERO && wait <= Duration::from_secs(12),
            "a refusal must name a wait inside the twelve-second accrual \
             interval, got {wait:?}"
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
    /// C9. An eviction that did not happen says so.
    ///
    /// `auth_invalidate` returned `()` and swallowed both an unreachable Redis
    /// and a failed DEL, so the CLI's revoke path printed "shared cache
    /// evicted" either way. During a leaked-key incident that is the sentence
    /// the operator acts on, and the difference between the two outcomes is
    /// fifteen seconds and five minutes of a key that still works.
    #[tokio::test]
    async fn evicting_against_an_unreachable_cache_is_an_error() {
        // A port nothing is listening on. `Cache::connect` is lazy — it must
        // be, so a replica whose Redis is down still boots — so the failure
        // surfaces here, on use, which is exactly where the CLI needs it.
        let cache = Cache::connect("redis://127.0.0.1:1").expect("lazy connect");
        assert!(
            cache.auth_invalidate("some-hash").await.is_err(),
            "an eviction that could not reach the cache is not an eviction"
        );
    }
    /// S8. A value from Redis cannot panic the process.
    ///
    /// `Duration::from_secs_f64` panics on a non-finite or out-of-range value,
    /// and this number comes back from a Lua script in another process. A panic
    /// here is a 500 on a request the rate limiter was only meant to delay —
    /// the limiter turning a slowdown into an outage.
    #[test]
    fn a_nonsense_wait_from_redis_is_no_wait_rather_than_a_panic() {
        // The real conversion, over the values a `parse::<f64>()` of arbitrary
        // bytes can actually produce. This used to be a copy of those three
        // lines, which meant the test passed with `try_from_secs_f64` deleted
        // from the code it was written to protect.
        let wait = super::wait_from_redis;

        assert_eq!(wait("1.5"), Some(Duration::from_millis(1500)));
        assert_eq!(wait("0"), None);
        assert_eq!(wait("-1"), None);
        assert_eq!(wait("not a number"), None, "already handled, and still is");
        assert_eq!(wait("inf"), None, "`from_secs_f64` panics on this one");
        assert_eq!(wait("NaN"), None);
        assert_eq!(
            wait("1e300"),
            None,
            "beyond what a Duration can hold, which is also a panic"
        );
    }
}
