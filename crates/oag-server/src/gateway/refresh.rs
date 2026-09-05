//! Keeping OAuth credentials fresh.
//!
//! An expiring token that N replicas all notice at once is a small distributed
//! systems problem, and every part of this file exists because one of the
//! obvious implementations is wrong:
//!
//! - **Refresh without a lock** and every replica refreshes simultaneously. For
//!   providers whose refresh tokens are single-use, all but one of those calls
//!   invalidates the token the others just received.
//! - **Lock only within the process** and you have solved it for one replica.
//! - **Lock, then refresh what you already read**, and the loser of the race
//!   refreshes using a refresh token the winner has already consumed.
//! - **Write without a version check** and a slow refresh can overwrite a newer
//!   token with an older one.
//!
//! So: a process-local gate, then a fleet-wide lock, then a *fresh* read, then
//! a compare-and-swap write.

use crate::AppState;
use oag_core::credential::SecretMaterial;
use oag_core::{AccountId, Error, Result};
use oag_store::AccountRow;
use std::time::Duration;

/// Refresh this far ahead of expiry.
///
/// Long enough that a request never starts with a token about to expire
/// mid-flight, short enough not to churn tokens needlessly.
const REFRESH_SKEW: i64 = 300;

/// Lock lifetime. Must exceed a slow refresh round trip; the TTL is what stops
/// a dead replica from wedging a credential.
const LOCK_TTL: Duration = Duration::from_secs(30);

/// How long a loser waits for the winner before giving up and using what it has.
const LOSER_WAIT: Duration = Duration::from_secs(5);

/// Return usable credential material, refreshing first if it is close to expiry.
pub async fn ensure_fresh(state: &AppState, row: &AccountRow) -> Result<SecretMaterial> {
    // Trimmed on the way out of the seal, not at each adapter's header call.
    //
    // A credential arrives from a shell, a `--secret` flag, a pasted
    // `auth.json` or an environment variable, and every one of those can carry
    // a trailing newline. Sent verbatim it becomes `Authorization: Bearer
    // sk-abc\n`, which providers answer with 401 — so a perfectly valid
    // credential reads as revoked, the breaker cools it down, and the operator
    // goes looking for a billing problem. Trimming here covers credentials
    // already stored as well as new ones.
    let material: SecretMaterial = state
        .kek
        .open_json::<SecretMaterial>(&row.sealed())?
        .trimmed();
    let account = row.account_id();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    if !material.expires_within(now, REFRESH_SKEW) {
        return Ok(material);
    }

    // The adapter this credential actually gets, not the provider's default.
    //
    // A Codex seat is `Provider::OpenAI` and refreshes against ChatGPT's token
    // endpoint, not OpenAI's — `adapter_for` is what knows that, and asking
    // `state.adapter` by provider alone handed the seat's refresh token to the
    // wrong grant. The same mistake, in the same shape, as the dialect bug this
    // helper's siblings already carry a comment about.
    let adapter = crate::gateway::adapter_for(state, row.provider.parse()?, row)?;

    // 1. Process-local gate. Cheap, and it means one replica makes at most one
    //    attempt at the distributed lock per SUCCESSFUL refresh, however many
    //    requests are waiting: each waiter re-reads on entry and steps aside if
    //    someone got there first. A refresh that failed is not persisted, so
    //    after one the next waiter finds the credential still due and tries
    //    again — serially, not concurrently, but tried.
    let gate = state.refresh_gate(account);
    let _local = gate.lock().await;

    // Another task on this replica may have refreshed while we waited.
    if let Some(current) = reread(state, account).await?
        && !current.expires_within(now, REFRESH_SKEW)
    {
        return Ok(current);
    }

    // 2. Fleet-wide lock.
    if !state
        .cache
        .acquire_refresh_lock(account, LOCK_TTL)
        .await
        .unwrap_or(false)
    {
        // Someone else is doing it. Wait briefly, then take whatever they
        // wrote. Racing them would consume a refresh token they are using.
        tokio::time::sleep(LOSER_WAIT).await;
        return reread(state, account)
            .await?
            .ok_or_else(|| Error::Internal("credential vanished during refresh".to_owned()));
    }

    let result = refresh_locked(state, account, &adapter, now).await;
    state.cache.release_refresh_lock(account).await;
    result
}

/// Do the refresh, holding the lock.
async fn refresh_locked(
    state: &AppState,
    account: AccountId,
    adapter: &std::sync::Arc<dyn oag_upstream::ProviderAdapter>,
    now: i64,
) -> Result<SecretMaterial> {
    // 3. Re-read *after* taking the lock. The value we read before waiting may
    //    already be stale, and refreshing with a consumed refresh token is
    //    exactly the failure the lock exists to prevent.
    let row = oag_store::repo::account_by_id(&state.db, account)
        .await?
        .ok_or_else(|| Error::Internal("credential vanished during refresh".to_owned()))?;
    let current: SecretMaterial = state
        .kek
        .open_json::<SecretMaterial>(&row.sealed())?
        .trimmed();

    if !current.expires_within(now, REFRESH_SKEW) {
        return Ok(current);
    }

    // The credential's own proxy. It applied to inference only, so a
    // deployment whose egress must go through a proxy had its refresh traffic
    // leave by another route — sometimes failing, sometimes succeeding and
    // bypassing the control the proxy existed to enforce.
    match adapter.refresh(&current, row.proxy_url.as_deref()).await {
        Ok(Some(mut fresh)) => {
            fresh.version = current.version.saturating_add(1);
            let sealed = state.kek.seal_json(&fresh)?;
            let expires = fresh
                .expires_at
                .and_then(|e| time::OffsetDateTime::from_unix_timestamp(e).ok());

            // 4. Compare-and-swap. If the version moved under us, another
            //    writer won and theirs is at least as new as ours.
            let won = oag_store::repo::store_credentials(
                &state.db,
                account,
                &sealed,
                row.token_version,
                expires,
            )
            .await?;

            if won {
                metrics::counter!("oag_token_refreshes_total", "outcome" => "ok").increment(1);
                tracing::info!(%account, "refreshed credential");
                Ok(fresh)
            } else {
                tracing::debug!(%account, "lost the credential write race; using the winner's");
                reread(state, account)
                    .await?
                    .ok_or_else(|| Error::Internal("credential vanished".to_owned()))
            }
        }

        // The adapter does not refresh this kind. Nothing to do.
        Ok(None) => Ok(current),

        Err(e) => {
            // A rejected refresh token usually means somebody else already
            // rotated it. Re-read before concluding the credential is dead:
            // treating a won race as a failure would disable a healthy
            // credential every time two replicas refreshed together.
            if let Ok(Some(after)) = reread(state, account).await
                && after.version > current.version
            {
                tracing::debug!(%account, "refresh raced; another writer had already rotated it");
                return Ok(after);
            }
            metrics::counter!("oag_token_refreshes_total", "outcome" => "failed").increment(1);
            tracing::warn!(%account, error = %e, "credential refresh failed");
            Err(e)
        }
    }
}

async fn reread(state: &AppState, account: AccountId) -> Result<Option<SecretMaterial>> {
    let Some(row) = oag_store::repo::account_by_id(&state.db, account).await? else {
        return Ok(None);
    };
    Ok(Some(
        state
            .kek
            .open_json::<SecretMaterial>(&row.sealed())?
            .trimmed(),
    ))
}
