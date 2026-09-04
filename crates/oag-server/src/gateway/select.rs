//! Picking a credential: sticky pin first, then the cascade.

use crate::AppState;
use oag_core::credential::CredentialKind;
use oag_core::{AccountId, Error, Provider, Result};
use oag_pool::{Candidate, SessionKey};
use oag_store::{AccountRow, repo};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long a conversation stays pinned to a credential without being used.
///
/// Comfortably longer than a think-and-reply cycle, short enough that an
/// abandoned conversation releases its pin rather than skewing the pool.
const STICKY_TTL: Duration = Duration::from_mins(30);

/// How long a concurrency slot survives without being released.
///
/// The backstop for a replica that dies holding slots. Must exceed the longest
/// request, or a live request's slot expires under it and the credential is
/// oversubscribed.
const SLOT_TTL: Duration = Duration::from_mins(35);

/// Where a concurrency slot goes back to.
///
/// A trait only so that [`SlotGuard`]'s `Drop` can be exercised without a
/// Redis; the one production implementor is [`oag_store::Cache`].
#[async_trait::async_trait]
pub trait SlotStore: Send + Sync + 'static {
    async fn release(&self, account: AccountId, request_id: &str);
}

#[async_trait::async_trait]
impl SlotStore for oag_store::Cache {
    async fn release(&self, account: AccountId, request_id: &str) {
        // Best-effort. A slot that never gets handed back expires on its own
        // within [`SLOT_TTL`], which is why the TTL exists — a leaked slot is
        // bounded, not permanent.
        if let Err(e) = self.release_slot(account, request_id).await {
            tracing::debug!(error = %e, "could not release slot; it will expire");
        }
    }
}

/// The concurrency slot a lease holds, given back when the lease is dropped.
///
/// The same shape as `InFlightGuard`, for the same reason. Both are counts a
/// request borrows and has to return, and returning them by an explicit call at
/// every exit only ever covers the exits somebody remembered — so the streaming
/// path, which resolves its adapter and its renderer *after* a credential is
/// already leased, returned past every release. A stranded slot sits there for
/// the full [`SLOT_TTL`]; eight of them on one credential and it reports itself
/// `AtCapacity` with nothing in flight.
///
/// Held behind an `Arc` by [`Lease`], so a cloned lease releases once, when the
/// last clone goes. A clone dropped while the original is still streaming would
/// hand back a slot that is genuinely in use, oversubscribing the credential —
/// which is worse than leaking one.
pub struct SlotGuard {
    store: Arc<dyn SlotStore>,
    account: AccountId,
    request_id: String,
    released: AtomicBool,
}

impl std::fmt::Debug for SlotGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotGuard")
            .field("account", &self.account)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

impl SlotGuard {
    async fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        self.store.release(self.account, &self.request_id).await;
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if *self.released.get_mut() {
            return;
        }
        // Redis is async and `Drop` is not, so the release becomes a task.
        // Nothing awaits it, and nothing can — the whole job of this drop is to
        // cover the paths nobody wrote a release on, and a release that never
        // runs is the same bounded leak the TTL already covers.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("no runtime to release a slot on; it will expire");
            return;
        };
        let store = Arc::clone(&self.store);
        let account = self.account;
        let request_id = std::mem::take(&mut self.request_id);
        runtime.spawn(async move { store.release(account, &request_id).await });
    }
}

/// The chosen credential, plus the slot that has to be given back.
#[derive(Debug, Clone)]
pub struct Lease {
    pub account: AccountRow,
    pub request_id: String,
    pub via_sticky: bool,
    /// Never read: its `Drop` is the whole of it.
    slot: Arc<SlotGuard>,
}

impl Lease {
    /// Give the slot back now, rather than whenever this lease is dropped.
    ///
    /// Worth the explicit call wherever the ordering matters, because the drop
    /// spawns its release and so cannot be ordered against what happens next.
    /// Escalation needs that ordering: the rung above may pick this very
    /// credential, and its `acquire_slot` racing our release would either find
    /// a credential with no room or — both attempts carry the same request id —
    /// have the slot it just took removed underneath it.
    pub async fn release(&self) {
        self.slot.release().await;
    }
}

/// A lease over `account`, holding its slot until the last clone is dropped.
fn leased(state: &AppState, account: AccountRow, request_id: &str, via_sticky: bool) -> Lease {
    let slot = SlotGuard {
        store: Arc::new(state.cache.clone()),
        account: account.account_id(),
        request_id: request_id.to_owned(),
        released: AtomicBool::new(false),
    };
    Lease {
        account,
        request_id: request_id.to_owned(),
        via_sticky,
        slot: Arc::new(slot),
    }
}

/// Choose a credential for this request.
///
/// Order:
/// 1. **Sticky pin.** If this conversation already has one and it is still
///    healthy and has room, reuse it — that is what makes the provider's prompt
///    cache hit, and on agentic traffic the cache is most of the bill.
/// 2. **The cascade.** Otherwise run the filter cascade over everything the
///    route can reach.
///
/// `excluded` carries credentials that already failed this request, so failover
/// does not hand back the one that just broke.
///
/// `channel` is the `@api` / `@sub` pin off the model name. It narrows the pool
/// before anything else looks at it — including the sticky pin, which would
/// otherwise hand a conversation that started unqualified straight back to an
/// API key on the turn the caller asked for a seat.
#[allow(clippy::too_many_arguments)]
pub async fn lease(
    state: &AppState,
    route_id: uuid::Uuid,
    principal_id: uuid::Uuid,
    provider: Provider,
    session: &SessionKey,
    excluded: &HashSet<AccountId, impl std::hash::BuildHasher>,
    request_id: &str,
    channel: Option<CredentialKind>,
) -> Result<Lease> {
    // The pin narrows the pool *before* the sticky branch and the cascade both
    // read it, which is the whole of honouring it: filtering only inside the
    // cascade would leave a conversation already pinned to an API key served
    // from that key on the turn the caller asked for a seat.
    //
    // Filtered here rather than in `repo::candidates`, where the predicate
    // would sit comfortably beside the owner check, because an empty result
    // from SQL cannot tell "this route has no xai credentials" from "it has
    // three and none is a subscription" — and those need opposite answers.
    let rows = of_kind(
        repo::candidates(&state.db, route_id, provider.as_str(), principal_id).await?,
        channel,
    );

    // What "nothing left to try" means, given the pin and the reserves. A seat
    // that exists and is cooling down is a wait; an operator told only "no
    // credential for xai" goes and stares at three API keys that are working
    // perfectly. A reserve is worse still, because the pool it sends them to
    // stare at is healthy and deliberately parked.
    //
    // Computed from `rows` rather than from the candidates, because by the time
    // there is nothing to select the candidates are gone — and this is the one
    // moment the reason matters.
    let reserved = reserve_holding_back(&rows);
    let none_left = || match (reserved, channel) {
        (Some(reserve_pct), _) => Error::ReserveHeld {
            provider,
            reserve_pct,
        },
        (None, Some(kind)) => Error::NoCredentialOfKind { provider, kind },
        (None, None) => Error::NoCredential { provider },
    };

    if rows.is_empty() {
        if let Some(kind) = channel {
            metrics::counter!(
                "oag_channel_unavailable_total",
                "provider" => provider.as_str(),
                "kind" => kind.to_string(),
            )
            .increment(1);
        }
        return Err(none_left());
    }

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let sticky_key = session.redis_key(&route_id.to_string());

    // 1. The pin. Skipped during failover: the whole point of failing over is
    //    that the pinned credential just failed us.
    if excluded.is_empty()
        && let Some(row) = try_pinned(state, &rows, &sticky_key, now, request_id).await
    {
        return Ok(leased(state, row, request_id, true));
    }

    // 2. The cascade. Try in order, because the winner may have filled its last
    // slot between the snapshot and the acquire.
    let mut exhausted = 0usize;
    let mut remaining: Vec<&AccountRow> = rows
        .iter()
        .filter(|r| !excluded.contains(&r.account_id()))
        .collect();

    while !remaining.is_empty() {
        let candidates = candidates_for(state, &remaining, now).await;

        // Random per attempt: without it every replica reading the same
        // snapshot at the same instant picks the same credential and stampedes.
        let tie_breaker = fastrand_u64();
        let Some(selection) = oag_pool::select(&candidates, now, tie_breaker) else {
            if let Some(full) = every_candidate_is_full(&candidates, now) {
                metrics::counter!("oag_at_capacity_total", "provider" => provider.as_str())
                    .increment(1);
                return Err(Error::AtCapacity {
                    provider,
                    candidates: full,
                });
            }
            return Err(none_left());
        };

        let Some(row) = remaining
            .iter()
            .find(|r| r.account_id() == selection.account)
            .copied()
        else {
            return Err(none_left());
        };

        let limit = u32::try_from(row.max_concurrency).unwrap_or(0);
        let acquired = match state
            .cache
            .acquire_slot(selection.account, request_id, limit, SLOT_TTL)
            .await
        {
            Ok(acquired) => acquired,
            // Fail OPEN. `unwrap_or(false)` here turned a Redis outage into
            // "lost the race" for every candidate in turn, and the request
            // into `AtCapacity` — a full product outage reported as a sizing
            // problem. Admit, and count the admission.
            Err(e) => {
                slot_accounting_degraded("acquire", &e);
                true
            }
        };
        if acquired {
            let _ = state
                .cache
                .sticky_set(&sticky_key, selection.account, STICKY_TTL)
                .await;
            metrics::counter!("oag_selection_total", "stage" => format!("{:?}", selection.stage))
                .increment(1);
            return Ok(leased(state, row.clone(), request_id, false));
        }

        // Lost the race for the last slot. Drop it and re-run rather than
        // failing: another credential is very likely free.
        remaining.retain(|r| r.account_id() != selection.account);
        exhausted += 1;
    }

    // Distinguish "nothing to pick from" from "everything is busy". The first
    // means somebody has to add a credential; the second means waiting, or
    // raising max_concurrency, and resolves without anyone doing anything.
    if exhausted > 0 {
        metrics::counter!("oag_at_capacity_total", "provider" => provider.as_str()).increment(1);
        return Err(Error::AtCapacity {
            provider,
            candidates: exhausted,
        });
    }
    Err(none_left())
}

/// The candidates a channel pin leaves standing. No pin leaves all of them.
///
/// A row whose `kind` column parses to nothing is dropped by a pin and kept
/// without one — the safe direction both times. `account.kind` is free text, so
/// a misspelling must not silently satisfy `@sub`: a request that asked for a
/// seat and got something unrecognised is billed as though it were metered, and
/// the caller is never told.
fn of_kind(rows: Vec<AccountRow>, channel: Option<CredentialKind>) -> Vec<AccountRow> {
    let Some(kind) = channel else {
        return rows;
    };
    rows.into_iter()
        .filter(|r| CredentialKind::from_column(&r.kind) == Some(kind))
        .collect()
}

/// Reuse this conversation's pinned credential, if it is still usable.
///
/// "Still usable" is the load-bearing part. A pin that is honoured even when
/// the credential is cooling down or saturated stops being affinity and starts
/// being a way to keep hammering a sick credential, so an unusable pin falls
/// through to the cascade instead.
async fn try_pinned(
    state: &AppState,
    rows: &[AccountRow],
    sticky_key: &str,
    now: i64,
    request_id: &str,
) -> Option<AccountRow> {
    let pinned = state
        .cache
        .sticky_get(sticky_key, STICKY_TTL)
        .await
        .ok()??;
    let row = rows.iter().find(|r| r.account_id() == pinned)?;
    let candidate = candidate_for(state, row, now).await?;

    if !is_eligible(&candidate, now) || !state.breakers.permits(pinned, now) {
        return None;
    }
    let acquired = match state
        .cache
        .acquire_slot(
            candidate.account,
            request_id,
            candidate.max_concurrency,
            SLOT_TTL,
        )
        .await
    {
        Ok(acquired) => acquired,
        // Same policy as the cascade: an unanswerable Redis admits. The pin
        // was the right credential a moment ago; a blink does not change that.
        Err(e) => {
            slot_accounting_degraded("acquire", &e);
            true
        }
    };

    if acquired {
        metrics::counter!("oag_selection_total", "stage" => "sticky").increment(1);
        Some(row.clone())
    } else {
        None
    }
}

async fn candidate_for(state: &AppState, row: &AccountRow, _now: i64) -> Option<Candidate> {
    // Counted by the same expiry the acquire trims by, so a leaked slot stops
    // counting when it would have been swept — rather than until the key
    // itself expires, twice the TTL later, with the credential reading as
    // full the whole time and nothing acquiring on it to sweep it.
    let in_flight = match state.cache.slots_in_use(row.account_id(), SLOT_TTL).await {
        Ok(n) => n,
        // The count is what the scheduler ranks by; without it every candidate
        // ranks as idle, which is the right degraded answer. Say so, rather
        // than pass it off as an idle credential.
        Err(e) => {
            slot_accounting_degraded("count", &e);
            0
        }
    };
    // The gauge `metrics::describe` has declared since the beginning and
    // nothing ever set. Set here, from the number the scheduler is about to
    // rank by, because this is the one place the answer is already in hand.
    metrics::gauge!("oag_slots_in_use", "account" => row.name.clone()).set(f64::from(in_flight));
    row.to_candidate(in_flight, 0)
}

/// Redis could not answer a slot question.
///
/// Counted and logged rather than folded into `false`/`0`, which is what it
/// used to be: an unreachable Redis read as "lost the race for the last slot"
/// on every candidate, every candidate was dropped, and the request failed
/// `AtCapacity` — sending the operator to raise `max_concurrency` on a pool
/// with nothing in flight. Selection now admits the request instead and
/// counts the admission, because the rate limiter beside it has always failed
/// open for the same reason: coordination is a courtesy, and refusing all
/// traffic because the coordination store blinked trades a real outage for a
/// theoretical oversubscription.
fn slot_accounting_degraded(op: &'static str, e: &Error) {
    metrics::counter!("oag_slot_accounting_degraded_total", "op" => op).increment(1);
    tracing::warn!(error = %e, op, "slot accounting unavailable; admitting without it");
}

fn is_eligible(c: &Candidate, now: i64) -> bool {
    c.schedulable
        && c.cooldown_until.is_none_or(|t| t <= now)
        && c.rate_limited_until.is_none_or(|t| t <= now)
        // Beside the cooldown for a reason: a conversation pinned to a seat
        // that has since crossed its reserve has to fall through to the
        // cascade, exactly as it does when the seat starts cooling down.
        // Honouring the pin regardless would leave the reserve protecting
        // everyone except the traffic already drinking from the seat, which is
        // all of the traffic that matters.
        && !oag_pool::held_by_reserve(c.usage_remaining_pct, c.usage_reserve_pct)
        && c.in_flight < c.max_concurrency
}

/// The reserve to name when a request finds nothing to run on, if a reserve is
/// what is holding the pool back.
///
/// `None` unless *every* candidate is reserved out, so the message can say
/// "every credential" and be telling the truth. A pool where one seat is parked
/// and another is merely cooling down is not a reserve problem: it resolves on
/// its own, and pointing an operator at a policy they set would send them to
/// change a setting that was not the cause.
///
/// The largest of the reserves when they differ, because every credential is at
/// or below its own — and therefore below the largest — while the smallest
/// would make the sentence false for the seat with the roomiest line.
fn reserve_holding_back(rows: &[AccountRow]) -> Option<i16> {
    if rows.is_empty() || !rows.iter().all(AccountRow::held_by_reserve) {
        return None;
    }
    rows.iter().filter_map(|r| r.usage_reserve_pct).max()
}

/// Everything local first, then one round trip for whoever survives.
///
/// The breaker filter has to happen *before* the cascade, not inside it: a
/// broken credential fails fast, so it always has the lowest in-flight count,
/// so the least-loaded stage actively prefers it. Filtering afterwards would
/// be too late. A read, deliberately — we are asking about every candidate
/// and will send to one; spending a half-open probe here would spend it on
/// credentials this request never touches. The probe is claimed where the
/// request is dispatched.
///
/// The row-local eligibility check (schedulable, cooling, rate-limited,
/// reserved) is on the same struct and costs nothing, so it goes ahead of the
/// Redis count too. Before, one `ZCARD` per row was awaited in sequence and
/// then most of the answers were discarded by a predicate the row could have
/// answered itself.
async fn candidates_for(state: &AppState, remaining: &[&AccountRow], now: i64) -> Vec<Candidate> {
    let probe: Vec<&AccountRow> = remaining
        .iter()
        .copied()
        .filter(|r| state.breakers.permits(r.account_id(), now))
        .filter(|r| r.to_candidate(0, 0).is_some_and(|c| is_eligible(&c, now)))
        .collect();
    let ids: Vec<AccountId> = probe.iter().map(|r| r.account_id()).collect();
    let counts = match state.cache.slots_in_use_many(&ids, SLOT_TTL).await {
        Ok(counts) if counts.len() == ids.len() => counts,
        Ok(_) => {
            slot_accounting_degraded("count", &Error::Internal("short pipeline reply".to_owned()));
            vec![0; ids.len()]
        }
        Err(e) => {
            slot_accounting_degraded("count", &e);
            vec![0; ids.len()]
        }
    };
    let mut candidates = Vec::with_capacity(probe.len());
    for (row, in_flight) in probe.iter().zip(counts) {
        metrics::gauge!("oag_slots_in_use", "account" => row.name.clone())
            .set(f64::from(in_flight));
        if let Some(c) = row.to_candidate(in_flight, 0) {
            candidates.push(c);
        }
    }
    candidates
}

/// How many candidates there were, when nothing was selectable because every
/// one of them was eligible and simply full. `None` for any other nothing.
///
/// Says WHICH nothing. A pool at its concurrency limit is a wait, not a
/// configuration problem, and it used to exit selection as `no_credential`
/// with `oag_at_capacity_total` flat — the same signal as a route with no
/// credentials at all, pointing the operator at the wrong fix. Only
/// credentials that were built as candidates are classified: a breaker-skipped
/// one never reached the list, and `breaker-verify.sh` pins that an open
/// breaker still answers `no_credential`.
fn every_candidate_is_full(candidates: &[Candidate], now: i64) -> Option<usize> {
    let full = candidates
        .iter()
        .filter(|c| is_eligible(c, now) && c.in_flight >= c.max_concurrency)
        .count();
    (full > 0 && full == candidates.len()).then_some(full)
}

/// A cheap non-cryptographic random.
///
/// Only used to spread ties across equally-good credentials, so it needs to be
/// unpredictable to nobody — but it does need to differ between concurrent
/// calls in one process, which a time-seeded value does not reliably do.
fn fastrand_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0x2545_F491_4F6C_DD1D);
    // xorshift64, advanced with one atomic read-modify-write so concurrent
    // callers get distinct draws. The comment above used to say "atomically"
    // over a separate load and store, which is two callers reading the same
    // state and both storing the same successor — the exact same draw, on the
    // exact code path whose only job is to make two replicas differ.
    let mut next = 0u64;
    let _ = STATE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |mut x| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        next = x;
        Some(x)
    });
    next
}

/// A slot store that counts releases instead of dialling Redis, and a lease
/// wired to one.
///
/// Not inside `tests` because the gateway's own tests need to build a lease and
/// a lease's guard is private to this module.
#[cfg(test)]
pub(crate) mod testing {
    use super::{AccountId, AtomicBool, Lease, SlotGuard, SlotStore};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    pub(crate) struct CountingSlots {
        released: Arc<AtomicUsize>,
    }

    impl CountingSlots {
        /// The release count once the task that `Drop` spawned has had a chance
        /// to run. A drop cannot await, so a test has to yield to it — and
        /// keeping the yields going past the first release is what catches a
        /// second one.
        pub(crate) async fn settled(&self) -> usize {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            self.released.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SlotStore for CountingSlots {
        async fn release(&self, _account: AccountId, _request_id: &str) {
            self.released.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A credential row with nothing but its identity and its kind set.
    pub(crate) fn account(name: &str, kind: &str) -> oag_store::AccountRow {
        oag_store::AccountRow {
            id: uuid::Uuid::new_v4(),
            name: name.to_owned(),
            provider: "xai".to_owned(),
            kind: kind.to_owned(),
            credentials_sealed: Vec::new(),
            credentials_nonce: Vec::new(),
            token_version: 0,
            token_expires_at: None,
            owner_principal_id: None,
            proxy_url: None,
            priority: 0,
            max_concurrency: 1,
            schedulable: true,
            cooldown_until: None,
            rate_limited_until: None,
            window_resets_at: None,
            usage_remaining_pct: None,
            usage_reserve_pct: None,
            last_used_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// A lease with no database row behind it.
    pub(crate) fn lease(store: &Arc<CountingSlots>) -> Lease {
        let account = oag_store::AccountRow {
            id: uuid::Uuid::nil(),
            name: "test".to_owned(),
            provider: "anthropic".to_owned(),
            kind: "api_key".to_owned(),
            credentials_sealed: Vec::new(),
            credentials_nonce: Vec::new(),
            token_version: 0,
            token_expires_at: None,
            owner_principal_id: None,
            proxy_url: None,
            priority: 0,
            max_concurrency: 1,
            schedulable: true,
            cooldown_until: None,
            rate_limited_until: None,
            window_resets_at: None,
            usage_remaining_pct: None,
            usage_reserve_pct: None,
            last_used_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        Lease {
            request_id: "req-1".to_owned(),
            via_sticky: false,
            slot: Arc::new(SlotGuard {
                store: Arc::clone(store) as Arc<dyn SlotStore>,
                account: account.account_id(),
                request_id: "req-1".to_owned(),
                released: AtomicBool::new(false),
            }),
            account,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{CountingSlots, lease as test_lease};
    use super::*;

    /// A state whose Redis is a port nothing listens on: every slot question
    /// fails at connect, immediately. `Db::connect` is lazy and never dialled.
    fn dead_redis_state() -> Arc<AppState> {
        let src = r#"
database:
  url: "postgres://oag:oag@127.0.0.1:1/oag"
redis:
  url: "redis://127.0.0.1:1"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
"#;
        let config = oag_core::config::Config::from_yaml(src).expect("test config");
        let db = oag_store::Db::connect(&config.database.url, 1).expect("lazy pool");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
        Arc::new(AppState::new(config, db, cache).expect("state"))
    }

    #[tokio::test]
    async fn an_unanswerable_redis_reads_as_idle_not_as_full() {
        // THE OUTAGE. `slots_in_use(..).unwrap_or(0)` was already the right
        // degraded answer for the *count*; the acquire beside it read
        // `unwrap_or(false)` — "lost the race" — and every candidate lost, so
        // every request failed `AtCapacity` while Redis was down. The count
        // path is the half this harness can reach without Postgres: a dead
        // Redis must yield a candidate, ranked idle, and not an error.
        let state = dead_redis_state();
        let row = super::testing::account("seat", "api_key");

        let candidate = candidate_for(&state, &row, 0)
            .await
            .expect("a candidate, not a refusal");
        assert_eq!(candidate.in_flight, 0, "unknown is idle, not full");
        assert!(is_eligible(&candidate, 0));

        // And the acquire reports the failure as an error the caller can
        // choose to admit on — not as `false`, which is what turned a Redis
        // blink into a refusal of every credential in turn.
        let err = state
            .cache
            .acquire_slot(row.account_id(), "req", 1, SLOT_TTL)
            .await
            .expect_err("a dead Redis is an error, not a lost race");
        assert!(err.to_string().contains("redis"), "{err}");
    }

    #[tokio::test]
    async fn a_dropped_lease_hands_its_slot_back() {
        // The whole point of the guard: an early return anywhere between
        // `lease` and the end of the request gives the slot back, whether or
        // not whoever wrote that return thought about it.
        let slots = Arc::new(CountingSlots::default());
        drop(test_lease(&slots));
        assert_eq!(slots.settled().await, 1);
    }

    #[tokio::test]
    async fn a_cloned_lease_hands_its_slot_back_once_the_last_clone_goes() {
        // Both clones name one slot in Redis. Releasing on the first drop would
        // hand back a slot the other clone is still streaming through, which
        // oversubscribes the credential rather than merely leaking from it.
        let slots = Arc::new(CountingSlots::default());
        let lease = test_lease(&slots);
        let clone = lease.clone();

        drop(lease);
        assert_eq!(slots.settled().await, 0, "the clone still holds the slot");

        drop(clone);
        assert_eq!(slots.settled().await, 1);
    }

    #[tokio::test]
    async fn an_explicit_release_is_not_repeated_when_the_lease_drops() {
        // Escalation releases explicitly so the next rung can re-lease the same
        // credential. A second release on drop would be aimed at the *new*
        // slot, since both attempts carry the same request id.
        let slots = Arc::new(CountingSlots::default());
        let lease = test_lease(&slots);
        lease.release().await;
        assert_eq!(slots.settled().await, 1);

        drop(lease);
        assert_eq!(slots.settled().await, 1);
    }

    #[test]
    fn eligibility_matches_the_schedulers_view() {
        let base = Candidate {
            account: AccountId::new(),
            provider: Provider::Anthropic,
            priority: 0,
            max_concurrency: 4,
            in_flight: 0,
            waiting: 0,
            schedulable: true,
            cooldown_until: None,
            rate_limited_until: None,
            window_resets_at: None,
            usage_remaining_pct: None,
            usage_reserve_pct: None,
            last_used_at: 0,
        };
        assert!(is_eligible(&base, 100));

        let mut cooling = base.clone();
        cooling.cooldown_until = Some(200);
        assert!(!is_eligible(&cooling, 100), "still cooling");
        assert!(is_eligible(&cooling, 300), "cooldown passed");

        let mut full = base.clone();
        full.in_flight = 4;
        assert!(!is_eligible(&full, 100), "no free slot");

        let mut off = base;
        off.schedulable = false;
        assert!(!is_eligible(&off, 100));
    }

    #[test]
    fn the_tie_breaker_varies_between_calls() {
        // A constant here would make every replica choose identically and
        // stampede one credential.
        let draws: std::collections::HashSet<u64> = (0..32).map(|_| fastrand_u64()).collect();
        assert!(draws.len() > 24, "expected mostly distinct draws");
    }

    #[test]
    fn a_sub_pinned_request_never_sees_an_api_key_credential() {
        // The pin is worthless unless it reaches selection. Asserting on the
        // surviving rows' kinds rather than on a count: a filter that kept the
        // right *number* of credentials and the wrong ones would bill a
        // subscription request to a metered key and say nothing.
        let rows = vec![
            super::testing::account("key-a", "api_key"),
            super::testing::account("seat", "oauth"),
            super::testing::account("key-b", "api_key"),
        ];

        let subs = of_kind(rows.clone(), Some(CredentialKind::OAuth));
        assert_eq!(
            subs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["seat"]
        );

        let keys = of_kind(rows.clone(), Some(CredentialKind::ApiKey));
        assert_eq!(
            keys.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["key-a", "key-b"]
        );

        // And with no pin the pool is untouched — the cheapest live credential
        // is the default, and that is what makes the router worth having.
        assert_eq!(of_kind(rows.clone(), None).len(), rows.len());
    }

    #[test]
    fn a_credential_kind_nobody_can_parse_does_not_satisfy_a_pin() {
        // `account.kind` is free text with no CHECK constraint. A typo that
        // counted as a subscription would serve a seat-pinned request from
        // something else and meter it as though it had not.
        let rows = vec![super::testing::account("typo", "0auth")];
        assert!(of_kind(rows.clone(), Some(CredentialKind::OAuth)).is_empty());
        assert_eq!(
            of_kind(rows, None).len(),
            1,
            "unpinned, it is still a candidate"
        );
    }

    #[test]
    fn a_pin_with_no_matching_credential_names_the_kind_it_wanted() {
        // "no credential available for xai" sends an operator to look at a pool
        // holding three healthy keys. The missing thing is the channel, and the
        // message has to be the thing that says so.
        let missing = Error::NoCredentialOfKind {
            provider: Provider::XAI,
            kind: CredentialKind::OAuth,
        };
        let message = missing.to_string();
        assert!(message.contains("subscription"), "{message}");
        assert!(message.contains("xai"), "{message}");
        // Not the wire spelling of the column: the person reading this bought a
        // subscription, not an oauth.
        assert!(!message.contains("oauth"), "{message}");

        assert!(
            Error::NoCredentialOfKind {
                provider: Provider::XAI,
                kind: CredentialKind::ApiKey,
            }
            .to_string()
            .contains("API key")
        );
    }

    /// A credential row with a usage reading and a floor under it.
    fn reserved(name: &str, remaining: i64, reserve: i16) -> AccountRow {
        let mut row = super::testing::account(name, "oauth");
        row.usage_remaining_pct = Some(rust_decimal::Decimal::from(remaining));
        row.usage_reserve_pct = Some(reserve);
        row
    }

    #[test]
    fn a_pinned_conversation_lets_go_of_a_seat_that_has_crossed_its_reserve() {
        // The sticky pin is the traffic doing the draining. A reserve that held
        // for new conversations and not for the ones already on the seat would
        // protect nothing.
        let mut candidate = Candidate {
            account: AccountId::new(),
            provider: Provider::XAI,
            priority: 0,
            max_concurrency: 4,
            in_flight: 0,
            waiting: 0,
            schedulable: true,
            cooldown_until: None,
            rate_limited_until: None,
            window_resets_at: None,
            usage_remaining_pct: Some(rust_decimal::Decimal::from(40)),
            usage_reserve_pct: Some(rust_decimal::Decimal::from(10)),
            last_used_at: 0,
        };
        assert!(is_eligible(&candidate, 100), "still has headroom");

        candidate.usage_remaining_pct = Some(rust_decimal::Decimal::from(10));
        assert!(!is_eligible(&candidate, 100), "at the line");

        // And an unread percentage is unknown, never exhausted: the pin holds.
        candidate.usage_remaining_pct = None;
        assert!(is_eligible(&candidate, 100));
    }

    #[test]
    fn a_pool_parked_by_its_reserves_names_the_reserve_rather_than_the_pool() {
        // "no credential available for xai" sends an operator to look at a pool
        // of enabled, un-cooled, perfectly healthy seats. The reserve is the
        // whole content of this failure, and its three fixes are nothing like
        // the fix for an empty pool.
        assert_eq!(reserve_holding_back(&[reserved("seat", 5, 10)]), Some(10));

        let message = Error::ReserveHeld {
            provider: Provider::XAI,
            reserve_pct: 10,
        }
        .to_string();
        assert!(message.contains("10%"), "{message}");
        assert!(message.contains("reserve"), "{message}");
        assert!(message.contains("set-reserve"), "{message}");
    }

    #[test]
    fn several_reserves_are_reported_by_the_one_every_seat_is_under() {
        // The message says "every credential is at or below its N%". Naming the
        // smallest would make that sentence false for the seat with the
        // roomiest line, which is the seat an operator would go and look at.
        let rows = [reserved("a", 5, 10), reserved("b", 20, 50)];
        assert_eq!(reserve_holding_back(&rows), Some(50));
    }

    #[test]
    fn an_unpolled_seat_is_never_reported_as_reserved_out() {
        // NULL is unknown, not empty. This row is schedulable, so blaming the
        // reserve for a failed request would be a lie with a fix attached.
        let mut unread = super::testing::account("seat", "oauth");
        unread.usage_reserve_pct = Some(10);
        assert_eq!(reserve_holding_back(&[unread]), None);
    }

    #[test]
    fn a_seat_with_no_reserve_never_produces_the_reserve_error() {
        // Today's behaviour, unchanged: an empty seat with no reserve set is
        // stopped by the provider's 429, and that is not this error.
        let mut spent = super::testing::account("seat", "oauth");
        spent.usage_remaining_pct = Some(rust_decimal::Decimal::ZERO);
        assert_eq!(reserve_holding_back(&[spent]), None);
        assert_eq!(
            reserve_holding_back(&[]),
            None,
            "an empty pool is not a reserve"
        );
    }

    #[test]
    fn a_pool_that_is_only_partly_reserved_out_is_not_blamed_on_the_reserve() {
        // One seat parked and another merely cooling down is a wait, not a
        // policy problem — and it resolves without anybody changing a setting.
        let rows = [
            reserved("parked", 5, 10),
            super::testing::account("key", "api_key"),
        ];
        assert_eq!(reserve_holding_back(&rows), None);
    }

    #[test]
    fn a_slot_outlives_the_longest_permitted_request() {
        // If a slot expired under a live request, the credential would be
        // oversubscribed rather than merely leaky.
        assert!(SLOT_TTL > Duration::from_mins(30));
    }
}
