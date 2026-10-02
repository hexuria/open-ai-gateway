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
//!
//! The same sweep asks the credentials that have a model list which models they
//! serve: xAI's, and the keys of every endpoint registered with
//! `discover_models`, each claimed the way a seat is.

use crate::AppState;
use crate::gateway::refresh::ensure_fresh;
use oag_store::ModelRow;
use oag_upstream::xai_models::{CatalogInsert, Donor};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

/// Start the usage poller, unless the interval is zero (disabled).
pub fn spawn_usage_poll(state: Arc<AppState>) {
    let interval = state.config.gateway.usage_poll_interval;
    if interval.is_zero() {
        return;
    }
    tokio::spawn(async move {
        // Poll shortly after boot, then about once an interval — a fresh
        // replica should not wait a whole period to know where its seats
        // stand. Each wait is jittered so replicas started together drift
        // apart; the per-seat claim in `poll_once` is what makes one of them
        // the reader.
        loop {
            poll_once(&state, interval).await;
            tokio::time::sleep(jittered(interval, rand::random::<f64>())).await;
        }
    });
}

/// `interval`, give or take a fifth, for a `unit` drawn from `[0, 1)`.
fn jittered(interval: Duration, unit: f64) -> Duration {
    interval.mul_f64(0.8 + 0.4 * unit.clamp(0.0, 1.0))
}

/// How long one replica's claim on a seat lasts: the shortest jittered wait,
/// so the next sweep on any replica always finds it expired.
fn claim_hold(interval: Duration) -> Duration {
    jittered(interval, 0.0)
}

/// One sweep over every subscription seat this replica wins the claim on.
async fn poll_once(state: &Arc<AppState>, interval: Duration) {
    let accounts = match oag_store::repo::schedulable_oauth_accounts(&state.db).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "usage poll: could not load seats");
            return;
        }
    };

    for row in accounts {
        let account = row.account_id();
        // One reader per seat per interval, fleet-wide. With Redis down the
        // seat is skipped rather than read by every replica: a stale quota is
        // a freshness problem, a crowd of readers on one person's plan is not.
        match state
            .cache
            .claim_usage_poll(account, claim_hold(interval))
            .await
        {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                tracing::debug!(%account, error = %e, "usage poll: no claim, skipping seat");
                continue;
            }
        }
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

        match oag_upstream::usage::fetch(
            provider,
            kind,
            &material,
            row.proxy_url.as_deref(),
            &state.config.gateway.codex.originator,
            &state.config.gateway.codex.user_agent,
        )
        .await
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

    sweep_endpoint_keys(state, interval).await;

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

/// Ask each key of an endpoint that discovers its models which models it
/// serves, and record the answer.
///
/// Claimed per key, as a seat is, so the fleet asks each key once an interval
/// however many replicas sweep. A replica that cannot serve the endpoint yet —
/// its reload has not loaded the row, or refused it — takes no claim, and
/// leaves the key to one that can: a claim taken by a replica that then asks
/// nothing would leave the key unasked for the interval.
///
/// First, every key whose endpoint no longer discovers has its served set
/// forgotten, so turning discovery off lists what the catalog holds again.
async fn sweep_endpoint_keys(state: &Arc<AppState>, interval: Duration) {
    if let Err(e) = oag_store::repo::forget_undiscovered_served_models(&state.db).await {
        tracing::warn!(error = %e, "usage poll: could not forget undiscovered served models");
    }
    let keys = match oag_store::repo::discovering_endpoint_accounts(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "usage poll: could not load endpoint keys");
            return;
        }
    };
    for row in keys {
        let account = row.account_id();
        let Ok(provider) = row.provider.parse::<oag_core::Provider>() else {
            continue;
        };
        if state.adapter(provider).is_err() {
            continue;
        }
        match state
            .cache
            .claim_usage_poll(account, claim_hold(interval))
            .await
        {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                tracing::debug!(%account, error = %e, "usage poll: no claim, skipping endpoint key");
                continue;
            }
        }
        let material = match ensure_fresh(state, &row).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%account, error = %e, "usage poll: skipping unreadable endpoint key");
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

#[cfg(test)]
mod tests {
    use super::{claim_hold, jittered, poll_once, spawn_usage_poll};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{any, header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn owned_seat(state: &crate::AppState) -> oag_core::AccountId {
        let id: uuid::Uuid = sqlx::query_scalar(
            "WITH owner AS (INSERT INTO principal (id, email) \
             VALUES (gen_random_uuid(), 'owner-' || gen_random_uuid() || '@test') RETURNING id) \
             INSERT INTO account (id, name, provider, kind, credentials_sealed, \
             credentials_nonce, owner_principal_id) \
             SELECT gen_random_uuid(), 'poll-' || gen_random_uuid(), 'xai', 'oauth', \
             '\\x00', '\\x00', owner.id FROM owner RETURNING id",
        )
        .fetch_one(state.db.pool())
        .await
        .expect("seat");
        oag_core::AccountId::from_uuid(id)
    }

    /// A sweep claims each owned seat for the interval, whether or not its
    /// read then succeeds (this one's credential cannot even be opened), and
    /// the spawned poller sweeps as soon as it starts.
    #[tokio::test]
    async fn a_sweep_claims_each_owned_seat_and_the_poller_sweeps_on_start() {
        let Some(state) = crate::testing::live_state().await else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
            return;
        };
        let hold = Duration::from_secs(60);

        let swept = owned_seat(&state).await;
        poll_once(&state, hold).await;
        assert!(
            !state
                .cache
                .claim_usage_poll(swept, hold)
                .await
                .expect("claim"),
            "the sweep took this seat's claim, so a second replica would not read it"
        );

        let later = owned_seat(&state).await;
        spawn_usage_poll(Arc::clone(&state));
        // Probe until the poller holds the seat. A probe that wins holds it for
        // only a millisecond, so it never keeps the poller out for longer than
        // one of its one-second sweeps. A fixed sleep here was flaky on a
        // loaded machine: the first sweep had not reached this seat yet.
        let mut swept_later = false;
        for _ in 0..100 {
            if !state
                .cache
                .claim_usage_poll(later, Duration::from_millis(1))
                .await
                .expect("claim")
            {
                swept_later = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(swept_later, "the spawned poller swept within ten seconds");
    }

    /// An endpoint with one sealed key filed under it: the endpoint's name and
    /// the key.
    async fn endpoint_key(
        state: &crate::AppState,
        base_url: &str,
        discover: bool,
    ) -> (String, oag_core::AccountId) {
        let name = format!(
            "t6-poll-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        oag_store::repo::insert_endpoint(
            &state.db,
            &oag_store::NewEndpoint {
                name: &name,
                dialect: "openai",
                platform: "plain",
                base_url: Some(base_url),
                auth: "bearer",
                region: None,
                project: None,
                api_version: None,
                path: None,
                extra_headers: &serde_json::json!({}),
                display_name: None,
                discover_models: discover,
            },
        )
        .await
        .expect("an endpoint");
        let sealed = state
            .kek
            .seal_json(&oag_core::credential::SecretMaterial {
                access_token: "t6-poll-key".to_owned(),
                refresh_token: None,
                expires_at: None,
                version: 0,
                client_id: None,
                account_id: None,
            })
            .expect("seal");
        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
             credentials_nonce) VALUES (gen_random_uuid(), $1, $2, 'api_key', $3, $4) \
             RETURNING id",
        )
        .bind(format!("{name}-key"))
        .bind(&name)
        .bind(&sealed.ciphertext)
        .bind(&sealed.nonce)
        .fetch_one(state.db.pool())
        .await
        .expect("a key");
        (name, oag_core::AccountId::from_uuid(id))
    }

    async fn served(state: &crate::AppState, key: oag_core::AccountId) -> Option<Vec<String>> {
        sqlx::query_scalar("SELECT served_models FROM account WHERE id = $1")
            .bind(key.as_uuid())
            .fetch_one(state.db.pool())
            .await
            .expect("the key")
    }

    async fn remove_endpoint(state: &crate::AppState, name: &str) {
        sqlx::query("DELETE FROM account WHERE provider = $1")
            .bind(name)
            .execute(state.db.pool())
            .await
            .expect("clean up");
        oag_store::repo::delete_endpoint(&state.db, name)
            .await
            .expect("clean up");
    }

    /// Sweep until `done` holds for the key's served set, or ten seconds pass,
    /// and say what the set is then. More than one sweep only when something
    /// else in this binary sweeps the same database and took the key's claim
    /// first.
    async fn sweep_until(
        state: &Arc<crate::AppState>,
        key: oag_core::AccountId,
        hold: Duration,
        done: impl Fn(&Option<Vec<String>>) -> bool,
    ) -> Option<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            poll_once(state, hold).await;
            let now = served(state, key).await;
            if done(&now) || Instant::now() > deadline {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// An endpoint key is asked its models across every page of its list, the
    /// answer is recorded, and the claim keeps any sweep, on any replica, from
    /// asking again within the interval.
    #[tokio::test]
    async fn an_endpoint_key_is_asked_once_an_interval_and_its_answer_recorded() {
        let Some(state) = crate::testing::live_state().await else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
            return;
        };
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param_is_missing("cursor"))
            .and(header("authorization", "Bearer t6-poll-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "m-1"}], "has_more": true, "next_cursor": "p2"
            })))
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param("cursor", "p2"))
            .and(header("authorization", "Bearer t6-poll-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "zai/m-2"}], "has_more": false
            })))
            .mount(&upstream)
            .await;
        let (name, key) = endpoint_key(&state, &format!("{}/v1", upstream.uri()), true).await;
        state.reload_catalog().await.expect("reload");
        let hold = Duration::from_secs(60);

        let recorded = sweep_until(&state, key, hold, Option::is_some).await;
        let asked = upstream.received_requests().await.expect("recording").len();
        poll_once(&state, hold).await;
        let asked_again = upstream.received_requests().await.expect("recording").len();
        let claimed = state.cache.claim_usage_poll(key, hold).await;
        remove_endpoint(&state, &name).await;

        assert_eq!(
            recorded,
            Some(vec!["m-1".to_owned(), "zai/m-2".to_owned()]),
            "every page's models"
        );
        assert_eq!(asked, 2, "one read of the list: its two pages");
        assert_eq!(asked_again, asked, "and no read while the claim holds");
        assert!(
            !claimed.expect("claim"),
            "the sweep holds the key's claim for the interval"
        );
    }

    /// A list the endpoint failed to serve leaves what was recorded before; a
    /// list with nothing in it is recorded as nothing, which hides every model.
    #[tokio::test]
    async fn a_failed_list_leaves_the_served_set_and_an_empty_one_hides_every_model() {
        let Some(state) = crate::testing::live_state().await else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
            return;
        };
        let (failing, empty) = (MockServer::start().await, MockServer::start().await);
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream fell over"))
            .mount(&failing)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": []})),
            )
            .mount(&empty)
            .await;
        let (failing_name, failing_key) =
            endpoint_key(&state, &format!("{}/v1", failing.uri()), true).await;
        let (empty_name, empty_key) =
            endpoint_key(&state, &format!("{}/v1", empty.uri()), true).await;
        let before = vec!["kept".to_owned()];
        for key in [failing_key, empty_key] {
            oag_store::repo::set_served_models(&state.db, key.as_uuid(), &before)
                .await
                .expect("an earlier answer");
        }
        state.reload_catalog().await.expect("reload");
        let hold = Duration::from_secs(60);

        let emptied = sweep_until(&state, empty_key, hold, |s| {
            s.as_ref().is_some_and(Vec::is_empty)
        })
        .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while failing
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
            && Instant::now() < deadline
        {
            poll_once(&state, hold).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let asked = failing.received_requests().await.expect("recording").len();
        let failed = served(&state, failing_key).await;
        remove_endpoint(&state, &failing_name).await;
        remove_endpoint(&state, &empty_name).await;

        assert_eq!(emptied, Some(Vec::new()), "asked, and it serves nothing");
        assert!(asked > 0, "the failing endpoint was asked");
        assert_eq!(failed, Some(before), "a failed read changes nothing");
    }

    /// A key whose claim another replica holds is not asked, and neither is a
    /// key of an endpoint that does not discover, whose old answer is
    /// forgotten.
    #[tokio::test]
    async fn a_claimed_key_and_one_whose_endpoint_does_not_discover_are_not_asked() {
        let Some(state) = crate::testing::live_state().await else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
            return;
        };
        let upstream = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m"}]})))
            .mount(&upstream)
            .await;
        let base = format!("{}/v1", upstream.uri());
        let (claimed_name, claimed_key) = endpoint_key(&state, &base, true).await;
        let (quiet_name, quiet_key) = endpoint_key(&state, &base, false).await;
        oag_store::repo::set_served_models(&state.db, quiet_key.as_uuid(), &["stale".to_owned()])
            .await
            .expect("an old answer");
        state.reload_catalog().await.expect("reload");
        let hold = Duration::from_secs(60);
        assert!(
            state
                .cache
                .claim_usage_poll(claimed_key, hold)
                .await
                .expect("claim"),
            "another replica's claim, taken first"
        );

        poll_once(&state, hold).await;
        let claimed_served = served(&state, claimed_key).await;
        let quiet_served = served(&state, quiet_key).await;
        let asked = upstream.received_requests().await.expect("recording").len();
        remove_endpoint(&state, &claimed_name).await;
        remove_endpoint(&state, &quiet_name).await;

        assert_eq!(asked, 0, "neither key was asked");
        assert_eq!(claimed_served, None);
        assert_eq!(
            quiet_served, None,
            "an answer discovery no longer keeps is forgotten"
        );
    }

    /// Each wait is the interval give or take a fifth, and a claim lasts the
    /// shortest of them, so the next sweep on any replica finds it expired.
    #[test]
    fn a_wait_is_within_a_fifth_of_the_interval_and_a_claim_ends_before_it() {
        let five = Duration::from_secs(300);
        assert_eq!(jittered(five, 0.0), Duration::from_secs(240));
        assert_eq!(jittered(five, 0.5), five);
        assert_eq!(jittered(five, 1.0), Duration::from_secs(360));
        assert_eq!(jittered(five, 7.0), Duration::from_secs(360), "clamped");
        assert_eq!(claim_hold(five), Duration::from_secs(240));
        assert!(claim_hold(five) <= jittered(five, 0.0));
    }
}
