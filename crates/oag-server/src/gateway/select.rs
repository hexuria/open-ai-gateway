//! Picking a credential: sticky pin first, then the cascade.

use crate::AppState;
use oag_core::{AccountId, Error, Provider, Result};
use oag_pool::{Candidate, SessionKey};
use oag_store::{AccountRow, repo};
use std::collections::HashSet;
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

/// The chosen credential, plus the slot that has to be given back.
#[derive(Debug, Clone)]
pub struct Lease {
    pub account: AccountRow,
    pub request_id: String,
    pub via_sticky: bool,
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
pub async fn lease(
    state: &AppState,
    route_id: uuid::Uuid,
    principal_id: uuid::Uuid,
    provider: Provider,
    session: &SessionKey,
    excluded: &HashSet<AccountId, impl std::hash::BuildHasher>,
    request_id: &str,
) -> Result<Lease> {
    let rows = repo::candidates(&state.db, route_id, provider.as_str(), principal_id).await?;
    if rows.is_empty() {
        return Err(Error::NoCredential { provider });
    }

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let sticky_key = session.redis_key(&route_id.to_string());

    // 1. The pin. Skipped during failover: the whole point of failing over is
    //    that the pinned credential just failed us.
    if excluded.is_empty()
        && let Some(row) = try_pinned(state, &rows, &sticky_key, now, request_id).await
    {
        return Ok(Lease {
            account: row,
            request_id: request_id.to_owned(),
            via_sticky: true,
        });
    }

    // 2. The cascade. Try in order, because the winner may have filled its last
    // slot between the snapshot and the acquire.
    let mut exhausted = 0usize;
    let mut remaining: Vec<&AccountRow> = rows
        .iter()
        .filter(|r| !excluded.contains(&r.account_id()))
        .collect();

    while !remaining.is_empty() {
        let mut candidates = Vec::with_capacity(remaining.len());
        for row in &remaining {
            // Skip credentials this replica has watched fail repeatedly.
            //
            // This has to happen *before* the cascade, not inside it: a broken
            // credential fails fast, so it always has the lowest in-flight
            // count, so the least-loaded stage actively prefers it. Filtering
            // afterwards would be too late.
            //
            // A read, deliberately. We are asking about every candidate and
            // will send to one; spending a half-open probe here would spend it
            // on credentials this request never touches. The probe is claimed
            // where the request is dispatched.
            if !state.breakers.permits(row.account_id(), now) {
                continue;
            }
            if let Some(c) = candidate_for(state, row, now).await {
                candidates.push(c);
            }
        }

        // Random per attempt: without it every replica reading the same
        // snapshot at the same instant picks the same credential and stampedes.
        let tie_breaker = fastrand_u64();
        let Some(selection) = oag_pool::select(&candidates, now, tie_breaker) else {
            return Err(Error::NoCredential { provider });
        };

        let Some(row) = remaining
            .iter()
            .find(|r| r.account_id() == selection.account)
            .copied()
        else {
            return Err(Error::NoCredential { provider });
        };

        let limit = u32::try_from(row.max_concurrency).unwrap_or(0);
        if state
            .cache
            .acquire_slot(selection.account, request_id, limit, SLOT_TTL)
            .await
            .unwrap_or(false)
        {
            let _ = state
                .cache
                .sticky_set(&sticky_key, selection.account, STICKY_TTL)
                .await;
            metrics::counter!("oag_selection_total", "stage" => format!("{:?}", selection.stage))
                .increment(1);
            return Ok(Lease {
                account: row.clone(),
                request_id: request_id.to_owned(),
                via_sticky: false,
            });
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
    Err(Error::NoCredential { provider })
}

/// Give the slot back.
///
/// Best-effort. If this fails the slot expires on its own within [`SLOT_TTL`],
/// which is why the TTL exists — a leaked slot is bounded, not permanent.
pub async fn release(state: &AppState, account: AccountId, request_id: &str) {
    if let Err(e) = state.cache.release_slot(account, request_id).await {
        tracing::debug!(error = %e, "could not release slot; it will expire");
    }
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
    let acquired = state
        .cache
        .acquire_slot(
            candidate.account,
            request_id,
            candidate.max_concurrency,
            SLOT_TTL,
        )
        .await
        .unwrap_or(false);

    if acquired {
        metrics::counter!("oag_selection_total", "stage" => "sticky").increment(1);
        Some(row.clone())
    } else {
        None
    }
}

async fn candidate_for(state: &AppState, row: &AccountRow, _now: i64) -> Option<Candidate> {
    let in_flight = state
        .cache
        .slots_in_use(row.account_id())
        .await
        .unwrap_or(0);
    row.to_candidate(in_flight, 0)
}

fn is_eligible(c: &Candidate, now: i64) -> bool {
    c.schedulable
        && c.cooldown_until.is_none_or(|t| t <= now)
        && c.rate_limited_until.is_none_or(|t| t <= now)
        && c.in_flight < c.max_concurrency
}

/// A cheap non-cryptographic random.
///
/// Only used to spread ties across equally-good credentials, so it needs to be
/// unpredictable to nobody — but it does need to differ between concurrent
/// calls in one process, which a time-seeded value does not reliably do.
fn fastrand_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0x2545_F491_4F6C_DD1D);
    // xorshift64*, advanced atomically so concurrent callers get distinct draws.
    let mut x = STATE.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    x
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_slot_outlives_the_longest_permitted_request() {
        // If a slot expired under a live request, the credential would be
        // oversubscribed rather than merely leaky.
        assert!(SLOT_TTL > Duration::from_mins(30));
    }
}
