//! Polling each subscription seat's remaining quota.
//!
//! A flat-rate seat has an allowance the gateway cannot see from its own
//! ledger — only the provider knows how much of the weekly Grok pool or the
//! Codex window is left. This background task reads it on an interval and lands
//! it where it is useful: the dashboard shows it, and an exhausted seat is
//! benched from the scheduler until its window resets.
//!
//! Modelled on `spawn_catalog_refresh`: one interval task, failing soft so a
//! provider's bad afternoon degrades the freshness of a number, never the
//! request path.

use crate::AppState;
use crate::gateway::refresh::ensure_fresh;
use std::sync::Arc;
use time::OffsetDateTime;

/// Start the usage poller, unless the interval is zero (disabled).
pub fn spawn_usage_poll(state: Arc<AppState>) {
    let interval = state.config.gateway.usage_poll_interval;
    if interval.is_zero() {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Poll shortly after boot (the first immediate tick), then on the
        // interval — a fresh replica should not wait a whole period to know
        // where its seats stand.
        loop {
            ticker.tick().await;
            poll_once(&state).await;
        }
    });
}

/// One sweep over every subscription seat.
async fn poll_once(state: &AppState) {
    let accounts = match oag_store::repo::schedulable_oauth_accounts(&state.db).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "usage poll: could not load seats");
            return;
        }
    };

    for row in accounts {
        let account = row.account_id();
        let Ok(provider) = row.provider.parse() else {
            continue;
        };
        // The kind, not just the provider, decides whether there is a quota to
        // read: a Codex seat and an ordinary OpenAI API key are both
        // `Provider::OpenAI`, and only the first has an allowance.
        let Some(kind) = oag_core::credential::CredentialKind::from_column(&row.kind) else {
            continue;
        };
        // Reuse the same fleet-safe refresh the request path uses, so polling a
        // seat with a near-expiry token refreshes it once rather than 401ing.
        let material = match ensure_fresh(state, &row).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%account, error = %e, "usage poll: skipping unrefreshable seat");
                continue;
            }
        };

        match oag_upstream::usage::fetch(provider, kind, &material).await {
            Ok(Some(snap)) => {
                let resets = snap
                    .resets_at
                    .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok());
                if let Err(e) = oag_store::repo::record_usage_poll(
                    &state.db,
                    account,
                    snap.remaining_pct,
                    &snap.window_label,
                    resets,
                )
                .await
                {
                    tracing::warn!(%account, error = %e, "usage poll: could not record reading");
                    continue;
                }
                // An exhausted seat is benched until its window resets — the
                // scheduler already excludes a credential whose
                // `rate_limited_until` is in the future. Only when we know the
                // reset time, so the bench has an end.
                if snap.remaining_pct <= 0.0
                    && let Some(until) = resets
                {
                    let _ = oag_store::repo::rate_limit(&state.db, account, until).await;
                    tracing::info!(%account, "usage poll: seat exhausted, benched until window reset");
                }
                tracing::debug!(%account, remaining = snap.remaining_pct, "usage polled");
            }
            // Either no usage API for this credential, or a body we could not
            // read a percentage out of. Both leave the account's usage columns
            // exactly as they were: NULL means "unknown", and inventing a 0%
            // would bench a working seat while a 100% would hide a spent one.
            Ok(None) => {}
            Err(e) => tracing::debug!(%account, error = %e, "usage poll: provider read failed"),
        }
    }
}
