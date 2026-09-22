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
use oag_store::ModelRow;
use oag_upstream::xai_models::{CatalogInsert, Donor};
use std::collections::HashSet;
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
async fn poll_once(state: &Arc<AppState>) {
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

        match oag_upstream::usage::fetch(provider, kind, &material, row.proxy_url.as_deref()).await
        {
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

        discover_served(state, &row, provider, account, &material).await;
    }

    // An API key has no quota, so the loop above never sees it. It does have
    // a model list, and that list is how a newly released model reaches the
    // picker without anyone editing the catalog by hand.
    let api_keys = match oag_store::repo::schedulable_accounts(&state.db, "xai", "api_key").await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "usage poll: could not load xai api keys");
            return;
        }
    };
    for row in api_keys {
        let account = row.account_id();
        let Ok(provider) = row.provider.parse() else {
            continue;
        };
        let material = match ensure_fresh(state, &row).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%account, error = %e, "usage poll: skipping unreadable api key");
                continue;
            }
        };
        discover_served(state, &row, provider, account, &material).await;
    }
}

/// Ask one credential which models it will actually accept, and record it.
///
/// Rides the usage sweep rather than getting a task of its own: it wants the
/// same set of credentials, the same freshened token, and the same failure
/// posture, and a second interval task polling the same seats would only be a
/// second thing to reason about.
///
/// The answer cannot come from anywhere else. The catalogue is priced from
/// LiteLLM and knows nothing about entitlement; a local proxy's model list is
/// that proxy's opinion; and the served set is a property of the PLAN behind
/// one credential, so a free `ChatGPT` seat and a paid one at the same provider
/// disagree. Only the credential can answer for itself.
///
/// Failure leaves the column exactly as it was, which is the same reasoning as
/// the usage columns above: NULL means "never asked" and falls back to ladder
/// visibility, while writing an empty array would claim the seat serves
/// nothing and empty the caller's picker.
async fn discover_served(
    state: &Arc<AppState>,
    row: &oag_store::AccountRow,
    provider: oag_core::Provider,
    account: oag_core::AccountId,
    material: &oag_core::credential::SecretMaterial,
) {
    // xAI's list carries prices and context the name-only adapter answer
    // throws away, and a model that is not inserted into the catalog can
    // never appear in `/v1/models` no matter what `served_models` says.
    if provider == oag_core::Provider::XAI {
        let Some(kind) = oag_core::credential::CredentialKind::from_column(&row.kind) else {
            return;
        };
        match oag_upstream::xai_models::list(kind, &material.access_token, row.proxy_url.as_deref())
            .await
        {
            Ok(listed) => record_xai_models(state, account, &listed).await,
            Err(e) => tracing::debug!(%account, error = %e, "served models: provider read failed"),
        }
        return;
    }

    let Ok(adapter) = crate::gateway::adapter_for(state, provider, row) else {
        return;
    };
    match adapter
        .served_models(material, row.proxy_url.as_deref())
        .await
    {
        Ok(Some(models)) => {
            if let Err(e) =
                oag_store::repo::set_served_models(&state.db, account.as_uuid(), &models).await
            {
                tracing::warn!(%account, error = %e, "served models: could not record");
                return;
            }
            tracing::debug!(%account, count = models.len(), "served models discovered");
        }
        // This adapter cannot be asked. Not a failure, and not evidence of
        // anything about the credential.
        Ok(None) => {}
        Err(e) => tracing::debug!(%account, error = %e, "served models: provider read failed"),
    }
}

/// Record what this xAI credential serves, and insert catalog rows for any
/// name it listed that the catalog has never seen.
///
/// The served set is written even when the insert fails: a model already in
/// the catalog becomes visible on the next listing, and the insert is retried
/// on the next poll. An insert that succeeded is reloaded into memory here,
/// rather than waiting out `catalog_refresh_interval`, because that interval
/// is what a picker would otherwise sit behind.
async fn record_xai_models(
    state: &Arc<AppState>,
    account: oag_core::AccountId,
    listed: &[oag_upstream::xai_models::ListedModel],
) {
    if let Err(e) = insert_new_xai_models(state, listed).await {
        tracing::warn!(%account, error = %e, "served models: could not add the new ones to the catalog");
    }
    let names: Vec<String> = listed.iter().map(|m| m.upstream_name.clone()).collect();
    if let Err(e) = oag_store::repo::set_served_models(&state.db, account.as_uuid(), &names).await {
        tracing::warn!(%account, error = %e, "served models: could not record");
        return;
    }
    tracing::debug!(%account, count = names.len(), "served models discovered");
}

async fn insert_new_xai_models(
    state: &Arc<AppState>,
    listed: &[oag_upstream::xai_models::ListedModel],
) -> oag_core::Result<()> {
    let catalog = oag_store::repo::catalog(&state.db).await?;
    let already: HashSet<String> = catalog
        .iter()
        .filter(|m| m.provider == "xai")
        .map(|m| m.upstream_name.clone())
        .collect();
    let donors: Vec<Donor<'_>> = catalog
        .iter()
        .filter(|m| m.provider == "xai")
        .map(donor_of)
        .collect();
    let inserts = oag_upstream::xai_models::inserts_for(listed, &already, &donors);
    if inserts.is_empty() {
        return Ok(());
    }
    for insert in &inserts {
        oag_store::repo::upsert_model(&state.db, &row_from(insert), false).await?;
        tracing::info!(
            model = %insert.upstream_name,
            "catalog: added a model the provider listed"
        );
    }
    state.reload_catalog().await?;
    Ok(())
}

fn donor_of(row: &ModelRow) -> Donor<'_> {
    Donor {
        upstream_name: row.upstream_name.as_str(),
        input_per_mtok: row.input_per_mtok,
        output_per_mtok: row.output_per_mtok,
        cache_read_per_mtok: row.cache_read_per_mtok,
        context_window: row.context_window,
        max_output_tokens: row.max_output_tokens,
        supports_vision: row.supports_vision,
        supports_tools: row.supports_tools,
        supports_reasoning: row.supports_reasoning,
        supports_prompt_cache: row.supports_prompt_cache,
    }
}

fn row_from(insert: &CatalogInsert) -> ModelRow {
    ModelRow {
        id: format!("xai/{}", insert.upstream_name),
        provider: "xai".to_owned(),
        upstream_name: insert.upstream_name.clone(),
        input_per_mtok: insert.input_per_mtok,
        output_per_mtok: insert.output_per_mtok,
        cache_read_per_mtok: insert.cache_read_per_mtok,
        cache_write_per_mtok: None,
        context_window: insert.context_window,
        max_output_tokens: insert.max_output_tokens,
        supports_vision: insert.supports_vision,
        supports_tools: insert.supports_tools,
        supports_reasoning: insert.supports_reasoning,
        supports_prompt_cache: insert.supports_prompt_cache,
        display_label: None,
    }
}
