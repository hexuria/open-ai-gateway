use super::catalog::UPSERT_MODEL_SQL;
use super::usage::{KEY_USAGE_SQL, PRINCIPAL_USAGE_SQL};
use super::*;
use crate::Db;
use crate::rows::{ModelRow, ServiceRow, UsageWrite};
use oag_core::{AccountId, Error};
use rust_decimal::Decimal;
use rust_decimal::dec;
use time::OffsetDateTime;
use uuid::Uuid;

#[test]
fn hashing_is_stable_and_hex() {
    let h = hash_key("oag_live_abc123");
    assert_eq!(h.len(), 64);
    assert_eq!(h, hash_key("oag_live_abc123"));
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn only_a_key_this_gateway_could_have_minted_has_the_issued_shape() {
    let minted = format!("{KEY_PREFIX}{}", "0123456789abcdef".repeat(4));
    assert_eq!(minted.len(), KEY_LEN);
    assert!(is_issued_key_shape(&minted));

    // Every way a string can fail to be one of ours, each refused
    // without a lookup: wrong prefix, wrong length either way, a
    // character outside lowercase hex, and the fake key errors.hurl
    // sends to prove a 401.
    assert!(!is_issued_key_shape("sk-ant-0123456789abcdef"));
    assert!(!is_issued_key_shape(&minted[..KEY_LEN - 1]));
    assert!(!is_issued_key_shape(&format!("{minted}0")));
    assert!(!is_issued_key_shape(&minted.to_uppercase()));
    assert!(!is_issued_key_shape(&format!(
        "{KEY_PREFIX}{}",
        "g".repeat(64)
    )));
    assert!(!is_issued_key_shape("oag_live_definitely_not_a_real_key"));
    assert!(!is_issued_key_shape(""));
}

#[test]
fn a_loggable_prefix_stops_before_the_entropy() {
    // The whole safety of writing this to a log is where it stops. A key
    // is `oag_live_` plus 64 hex; the prefix is the marker plus seven of
    // those characters — 28 bits of the 256, enough to name one key among
    // an operator's few and useless to anyone who wants to present one.
    // Widening it later would be silent, so the count is pinned here and
    // not left to a `take(n)` nobody reads twice.
    let minted = format!("{KEY_PREFIX}{}", "0123456789abcdef".repeat(4));
    let prefix = loggable_key_prefix(&minted);

    assert_eq!(prefix, "oag_live_0123456");
    assert_eq!(prefix.len(), 16);
    assert_eq!(&prefix[..KEY_PREFIX.len()], KEY_PREFIX);
    assert_eq!(prefix.len() - KEY_PREFIX.len(), 7, "seven hex, no more");
    assert!(minted.starts_with(&prefix));
    assert!(
        prefix.len() < minted.len(),
        "a prefix that is the whole key is the key"
    );
}

#[test]
fn different_keys_hash_differently() {
    assert_ne!(hash_key("oag_live_a"), hash_key("oag_live_b"));
}

#[test]
fn the_hash_does_not_contain_the_key() {
    assert!(!hash_key("oag_live_secret").contains("secret"));
}

/// The identity-integration round trip: bind a principal, mint a key on it,
/// and confirm the key authenticates, carries its cap, is never admin, and
/// stops working once revoked.
///
/// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
#[tokio::test]
async fn a_minted_key_authenticates_is_capped_and_is_never_admin() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let email = format!("org-{}@gateway.local", Uuid::new_v4());
    let route = format!("route-{}", Uuid::new_v4());
    sqlx::query(
            "INSERT INTO route (id, name, tiers, default_mode)
             VALUES (gen_random_uuid(), $1, '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb, 'passthrough')",
        )
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("insert route");

    // Upsert is idempotent on email: a second call must not create a twin.
    let first = upsert_principal(&db, &email, "member", Some(dec!(25.00)))
        .await
        .expect("upsert");
    let again = upsert_principal(&db, &email, "member", None)
        .await
        .expect("upsert again");
    assert_eq!(first, again, "upsert is idempotent on email");

    // ...and a budget already set is not erased by an upsert that omits one.
    let usage = principal_usage(&db, &email)
        .await
        .expect("usage")
        .expect("principal exists");
    assert_eq!(usage.monthly_budget_usd, Some(dec!(25.000000)));
    assert_eq!(usage.month_to_date_usd, dec!(0));

    let minted = mint_key(&db, &email, &route, "member-key", Some(dec!(5.00)))
        .await
        .expect("mint")
        .expect("principal and route exist");
    assert!(minted.key.starts_with("oag_live_"));
    assert_eq!(minted.prefix, minted.key[..16]);

    let context = authenticate(&db, &minted.key)
        .await
        .expect("authenticate")
        .expect("the minted key is live");
    assert_eq!(context.principal_id, first);
    assert!(
        !context.admin,
        "a key minted over HTTP must never carry admin authority"
    );

    // The cap landed, and can be cleared.
    let quota: Option<Decimal> = sqlx::query_scalar("SELECT quota_usd FROM api_key WHERE id = $1")
        .bind(minted.id)
        .fetch_one(db.pool())
        .await
        .expect("read quota");
    assert_eq!(quota, Some(dec!(5.000000)));
    set_key_quota(&db, minted.id, None)
        .await
        .expect("clear quota")
        .expect("key exists");

    // The org budget can be raised.
    set_principal_budget(&db, &email, Some(dec!(99.00)))
        .await
        .expect("set budget")
        .expect("principal exists");
    let raised = principal_usage(&db, &email)
        .await
        .expect("usage")
        .expect("principal exists");
    assert_eq!(raised.monthly_budget_usd, Some(dec!(99.000000)));

    // Revocation is what makes the key stop working.
    revoke_key(&db, minted.id).await.expect("revoke");
    assert!(
        authenticate(&db, &minted.key)
            .await
            .expect("authenticate")
            .is_none(),
        "a revoked key must not authenticate"
    );
}

/// An idempotent bind MUST NOT be able to remove authority: upserting against
/// an existing admin's email leaves their role alone. Getting this wrong locks
/// a human operator out of the admin API — the gate wants an admin key AND an
/// admin principal — without their key ever changing.
/// The plumbing a budget or quota write evicts through: every hash of a
/// principal's keys, and the hash of the one key a quota write touched.
///
/// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
#[tokio::test]
async fn a_principals_key_hashes_are_all_listed_and_a_quota_write_names_its_own() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let email = format!("org-{}@gateway.local", Uuid::new_v4());
    let route = format!("route-{}", Uuid::new_v4());
    sqlx::query(
            "INSERT INTO route (id, name, tiers, default_mode)
             VALUES (gen_random_uuid(), $1, '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb, 'passthrough')",
        )
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("insert route");
    let principal = upsert_principal(&db, &email, "member", None)
        .await
        .expect("upsert");

    let one = mint_key(&db, &email, &route, "one", None)
        .await
        .expect("mint")
        .expect("principal and route exist");
    let two = mint_key(&db, &email, &route, "two", None)
        .await
        .expect("mint")
        .expect("principal and route exist");

    let mut listed = key_hashes_for_principal(&db, principal)
        .await
        .expect("list");
    listed.sort();
    let mut minted = vec![hash_key(&one.key), hash_key(&two.key)];
    minted.sort();
    assert_eq!(
        listed, minted,
        "both keys, by the hash the caches are keyed on"
    );

    let (_, _, touched) = set_key_quota(&db, two.id, Some(dec!(1)))
        .await
        .expect("set quota")
        .expect("the key exists");
    assert_eq!(
        touched,
        hash_key(&two.key),
        "the hash of the key written, not another"
    );
}

#[tokio::test]
async fn upserting_a_principal_never_demotes_an_existing_admin() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let email = format!("operator-{}@example.com", Uuid::new_v4());
    sqlx::query("INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'admin')")
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("seed an admin principal");

    // The identity-integration path can only ever ask for `member`.
    upsert_principal(&db, &email, "member", Some(dec!(10.00)))
        .await
        .expect("upsert");

    let role: String = sqlx::query_scalar("SELECT role FROM principal WHERE email = $1")
        .bind(&email)
        .fetch_one(db.pool())
        .await
        .expect("read role");
    assert_eq!(
        role, "admin",
        "an upsert must not strip an existing principal's admin role"
    );
    // ...while still doing its actual job.
    let usage = principal_usage(&db, &email)
        .await
        .expect("usage")
        .expect("exists");
    assert_eq!(usage.monthly_budget_usd, Some(dec!(10.000000)));
}

/// Naming a principal or route that does not exist is reported, not silently
/// swallowed — otherwise a caller believes it minted a key that never was.
#[tokio::test]
async fn minting_on_a_missing_principal_or_route_is_none_not_a_phantom_key() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let nobody = format!("nobody-{}@gateway.local", Uuid::new_v4());
    assert!(
        mint_key(&db, &nobody, "default", "k", None)
            .await
            .expect("mint")
            .is_none()
    );
    assert!(
        set_principal_budget(&db, &nobody, Some(dec!(1.00)))
            .await
            .expect("budget")
            .is_none()
    );
    assert!(
        principal_usage(&db, &nobody)
            .await
            .expect("usage")
            .is_none()
    );
}

/// `route_by_id` against a real Postgres.
///
/// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it. This used to
/// pin that the route's month was summed from the ledger — on every
/// request, for any route with a budget. It now pins the opposite: the
/// spend is a column `record_usage` maintains, and the read is one
/// primary-key lookup gated on the month the column names.
#[tokio::test]
async fn route_spend_is_its_column_read_as_zero_once_its_month_has_passed() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let insert = |name: &str, month: &str| {
        let db = db.clone();
        let name = format!("{name}-{}", Uuid::new_v4());
        let month = month.to_owned();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                    "INSERT INTO route (id, name, tiers, default_mode, monthly_budget_usd,
                                        spent_usd, spent_month)
                     VALUES (gen_random_uuid(), $1,
                             '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb,
                             'managed', 500, 200.75,
                             CASE $2 WHEN 'this' THEN date_trunc('month', now())::date
                                     WHEN 'last' THEN (date_trunc('month', now()) - interval '1 month')::date
                                     ELSE NULL END)
                     RETURNING id",
                )
                .bind(name)
                .bind(month)
                .fetch_one(db.pool())
                .await
                .expect("insert route")
        }
    };
    let current = insert("current", "this").await;
    let stale = insert("stale", "last").await;
    let never = insert("never", "none").await;

    let spent = |id: Uuid| {
        let db = db.clone();
        async move {
            route_by_id(&db, id)
                .await
                .expect("load")
                .expect("exists")
                .spent_usd
        }
    };
    assert_eq!(
        spent(current).await,
        dec!(200.75),
        "this month's column, as is"
    );
    assert!(
        spent(stale).await.is_zero(),
        "a column naming last month reads as zero: the month rolled over"
    );
    assert!(spent(never).await.is_zero(), "never spent");
}

/// One ledger write moves all three counters, in one statement, and the
/// monthly two reset at the boundary without a job.
///
/// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
#[tokio::test]
async fn one_recorded_usage_debits_the_key_the_principal_and_the_route_together() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;
    let key = capped_key(&db, principal, route, format!("debit-{}", Uuid::new_v4())).await;

    let write = |cost: &str| UsageWrite {
        request_id: Uuid::new_v4(),
        attempt: 0,
        principal_id: Some(principal),
        api_key_id: Some(key),
        route_id: Some(route),
        account_id: Some(account.as_uuid()),
        model_id: "kimi-k2".to_owned(),
        tier: "cheap".to_owned(),
        selection_reason: "default".to_owned(),
        escalated_from_tier: None,
        escalation_gate: None,
        usage: oag_router::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        cost_usd: cost.parse().expect("decimal"),
        counterfactual_usd: Decimal::ZERO,
        counterfactual_model_id: None,
        counterfactual_api_usd: Decimal::ZERO,
        status: 200,
        latency_ms: Some(10),
        ttft_ms: None,
        streamed: false,
    };

    record_usage(&db, &write("1.25")).await.expect("record");
    record_usage(&db, &write("0.50")).await.expect("record");

    // THE FIX. This read is what the cap is enforced against, and it is
    // exactly as current as the ledger: not a five-minute-old snapshot,
    // not a SUM.
    let spend = spend_for(&db, key, principal).await.expect("spend");
    assert_eq!(spend.key_usd, dec!(1.75), "lifetime, on the key");
    assert_eq!(
        spend.principal_usd,
        dec!(1.75),
        "this month, on the principal"
    );
    let row = route_by_id(&db, route)
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(row.spent_usd, dec!(1.75), "this month, on the route");

    // The month rolls over. Nothing runs at midnight; the column simply
    // names a month that is not this one, and reads as zero — then the
    // first write of the new month resets it to that write alone, while
    // the key's lifetime counter carries on.
    sqlx::query(
        "UPDATE principal SET spent_month = (date_trunc('month', now()) - interval '1 month')::date
              WHERE id = $1",
    )
    .bind(principal)
    .execute(db.pool())
    .await
    .expect("age the principal's month");
    let rolled = spend_for(&db, key, principal).await.expect("spend");
    assert!(
        rolled.principal_usd.is_zero(),
        "last month's spend is not this month's"
    );
    assert_eq!(rolled.key_usd, dec!(1.75), "the key's cap is lifetime");

    record_usage(&db, &write("0.25")).await.expect("record");
    let fresh = spend_for(&db, key, principal).await.expect("spend");
    assert_eq!(
        fresh.principal_usd,
        dec!(0.25),
        "reset to this month's first write"
    );
    assert_eq!(fresh.key_usd, dec!(2.00));

    // The month never moves backwards. A write whose transaction began
    // before midnight, re-evaluated against a row the first write of
    // the new month has just committed, accumulates into that month
    // rather than overwriting it with its own cost and stamping the old
    // month back. Reproduced here from the row's side: a row already a
    // month ahead of `now()` is what such a write sees.
    sqlx::query(
            "UPDATE principal
                SET spent_usd = 5, spent_month = (date_trunc('month', now()) + interval '1 month')::date
              WHERE id = $1",
        )
        .bind(principal)
        .execute(db.pool())
        .await
        .expect("advance the principal's month");
    record_usage(&db, &write("0.25")).await.expect("record");
    let (ahead_usd, ahead_month): (Decimal, time::Date) =
        sqlx::query_as("SELECT spent_usd, spent_month FROM principal WHERE id = $1")
            .bind(principal)
            .fetch_one(db.pool())
            .await
            .expect("read the row");
    let next_month: time::Date =
        sqlx::query_scalar("SELECT (date_trunc('month', now()) + interval '1 month')::date")
            .fetch_one(db.pool())
            .await
            .expect("next month");
    assert_eq!(ahead_usd, dec!(5.25), "accumulated into the later month");
    assert_eq!(ahead_month, next_month, "and the month stayed where it was");

    // A key that no longer exists cannot spend as if uncapped.
    assert!(
        matches!(
            spend_for(&db, Uuid::new_v4(), principal).await,
            Err(Error::Unauthenticated)
        ),
        "a missing key is a refusal, not zeros"
    );
}

/// A ledger row as the previous release writes one: inserted, and no
/// counter touched. What every old replica does for the whole rolling
/// deploy after the counters exist.
async fn old_binary_row(db: &Db, principal: Uuid, key: Uuid, route: Uuid, cost: &str) {
    sqlx::query(
        "INSERT INTO usage_event (
                 request_id, attempt, principal_id, api_key_id, route_id,
                 model_id, tier, selection_reason,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 cost_usd, counterfactual_usd, counterfactual_api_usd,
                 status, latency_ms, streamed)
             VALUES (gen_random_uuid(), 0, $1, $2, $3, 'kimi-k2', 'cheap', 'classified',
                     10, 5, 0, 0, $4, 0, $4, 200, 10, false)",
    )
    .bind(principal)
    .bind(key)
    .bind(route)
    .bind(cost.parse::<Decimal>().expect("decimal"))
    .execute(db.pool())
    .await
    .expect("insert an old-binary row");
}

async fn set_budgets(db: &Db, principal: Uuid, route: Uuid) {
    sqlx::query("UPDATE principal SET monthly_budget_usd = 100 WHERE id = $1")
        .bind(principal)
        .execute(db.pool())
        .await
        .expect("budget the principal");
    sqlx::query("UPDATE route SET monthly_budget_usd = 100 WHERE id = $1")
        .bind(route)
        .execute(db.pool())
        .await
        .expect("budget the route");
}

/// The rolling-deploy window, reproduced: rows the old binary wrote after
/// the backfill are invisible to the cap until something reconciles.
///
/// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
#[tokio::test]
async fn reconcile_catches_up_the_spend_an_old_binary_recorded() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;
    let key = capped_key(
        &db,
        principal,
        route,
        format!("reconcile-{}", Uuid::new_v4()),
    )
    .await;
    set_budgets(&db, principal, route).await;

    // The new binary's write debits; the old binary's does not.
    record_usage(
        &db,
        &UsageWrite {
            request_id: Uuid::new_v4(),
            attempt: 0,
            principal_id: Some(principal),
            api_key_id: Some(key),
            route_id: Some(route),
            account_id: Some(account.as_uuid()),
            model_id: "kimi-k2".to_owned(),
            tier: "cheap".to_owned(),
            selection_reason: "default".to_owned(),
            escalated_from_tier: None,
            escalation_gate: None,
            usage: oag_router::Usage::default(),
            cost_usd: dec!(1.00),
            counterfactual_usd: Decimal::ZERO,
            counterfactual_model_id: None,
            counterfactual_api_usd: Decimal::ZERO,
            status: 200,
            latency_ms: Some(10),
            ttft_ms: None,
            streamed: false,
        },
    )
    .await
    .expect("record");
    old_binary_row(&db, principal, key, route, "0.50").await;

    let before = spend_for(&db, key, principal).await.expect("spend");
    assert_eq!(
        before.principal_usd,
        dec!(1.00),
        "the old row is invisible to the cap"
    );

    let done = reconcile_monthly_spend(&db).await.expect("reconcile");
    assert!(done.principals >= 1 && done.routes >= 1, "{done:?}");

    let after = spend_for(&db, key, principal).await.expect("spend");
    assert_eq!(
        after.principal_usd,
        dec!(1.50),
        "the cap now sees the ledger"
    );
    let row = route_by_id(&db, route)
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(row.spent_usd, dec!(1.50));

    // A second pass is a no-op in effect: same number, not doubled.
    reconcile_monthly_spend(&db).await.expect("reconcile again");
    let again = spend_for(&db, key, principal).await.expect("spend");
    assert_eq!(again.principal_usd, dec!(1.50));

    // The month only moves forward, here as in the debit. A row already
    // stamped with a later month is left exactly as it is.
    sqlx::query(
            "UPDATE principal
                SET spent_usd = 5, spent_month = (date_trunc('month', now()) + interval '1 month')::date
              WHERE id = $1",
        )
        .bind(principal)
        .execute(db.pool())
        .await
        .expect("advance the month");
    reconcile_monthly_spend(&db).await.expect("reconcile");
    let (usd, ahead): (Decimal, bool) = sqlx::query_as(
        "SELECT spent_usd, spent_month > date_trunc('month', now())::date
               FROM principal WHERE id = $1",
    )
    .bind(principal)
    .fetch_one(db.pool())
    .await
    .expect("read");
    assert_eq!(usd, dec!(5));
    assert!(ahead, "a later month is not pulled back");
}

/// The race the row lock exists for: a debit landing while a pass runs is
/// never lost from the counter.
///
/// The interleaving is forced, not hoped for. This used to spawn twenty-four
/// debits against six reconciles and assert the totals agreed, which passes
/// whenever no pass happens to overlap a debit's lock window — so it passed
/// with the fix reverted about as often as it caught it, and it was the
/// scheduler rather than the code that decided which. It also asked thirty
/// tasks to share a pool of eight, so its most common failure was a pool
/// timeout that had nothing to do with the property.
///
/// The construction below is the race written down. A debit holds the
/// principal's row lock in an uncommitted transaction; the reconcile pass
/// then runs and is *observed* to be blocked on that lock before the debit
/// commits. That is exactly the window the bug lives in:
///
///   * With the lock taken first, the pass waits before computing anything,
///     so its `SUM` runs after the debit committed and includes it.
///   * With the obvious single statement, the pass blocks *inside* its
///     UPDATE with the subquery already evaluated. Postgres re-checks the
///     WHERE clause against the new row version but does not re-run the SET
///     subquery, so a stale sum overwrites the debit and the money is gone.
///
/// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it.
#[tokio::test]
async fn reconcile_does_not_lose_a_debit_that_lands_while_it_runs() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 4).expect("connect");
    db.migrate().await.expect("migrate");
    let (principal, route, key, write) = budgeted_principal(&db).await;

    // A debit in flight: its ledger row and its counter increment, holding
    // the principal's row lock until it commits. This is what
    // `record_usage` does; done by hand only because the test has to stop
    // in the middle of it.
    let mut debit = db.pool().begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO usage_event (request_id, attempt, principal_id, api_key_id, route_id,
                                      account_id, model_id, tier, selection_reason,
                                      input_tokens, output_tokens, cost_usd, status)
             VALUES ($1, 0, $2, $3, $4, $5, 'kimi-k2', 'cheap', 'default', 0, 0, 0.10, 200)",
    )
    .bind(Uuid::new_v4())
    .bind(principal)
    .bind(key)
    .bind(route)
    .bind(write().account_id)
    .execute(&mut *debit)
    .await
    .expect("ledger row");
    sqlx::query(
        "UPDATE principal SET spent_usd = spent_usd + 0.10,
                                  spent_month = date_trunc('month', now())::date
              WHERE id = $1",
    )
    .bind(principal)
    .execute(&mut *debit)
    .await
    .expect("take the row lock");

    // The pass, which must now be waiting on that lock.
    let pass = tokio::spawn({
        let db = db.clone();
        async move { reconcile_monthly_spend(&db).await }
    });

    // Observed, not assumed. Without this the debit could commit before the
    // pass had reached the row at all, and the test would go back to
    // asserting whatever the scheduler felt like.
    let mut blocked = false;
    for _ in 0..200 {
        // `pg_stat_activity`, not `pg_locks` filtered by relation: a
        // transaction waiting for another's row lock waits on that
        // transaction's id, so nothing ungranted is recorded against the
        // table itself.
        //
        // Narrowed to the reconcile's own statements, and to somebody else's
        // backend. "Any backend waiting on a lock in this database" is
        // satisfied by any sibling test's `db.migrate()`, which takes an
        // advisory lock — so under parallel load the probe returned true
        // for a stranger, the debit committed before this pass reached the
        // row, and the reverted code passed. The test was interleaving
        // nothing and asserting it had.
        //
        // Both statements, because the pass blocks on whichever comes
        // first: it takes the row lock with `SELECT ... FOR UPDATE` before
        // it runs the `SET spent_usd` that needs it.
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
                  WHERE datname = current_database()
                    AND pid <> pg_backend_pid()
                    AND wait_event_type = 'Lock'
                    AND (query LIKE '%FOR UPDATE%' OR query LIKE '%SET spent_usd%')",
        )
        .fetch_one(db.pool())
        .await
        .expect("read the lock table");
        if waiting > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        blocked,
        "the pass never reached the locked row, so this run proved nothing"
    );

    debit.commit().await.expect("commit the debit");
    pass.await.expect("task").expect("reconcile");

    // And a second debit after the pass, so the counter is exercised in
    // both orders relative to it.
    record_usage(&db, &write()).await.expect("record");

    let ledger: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_event
              WHERE principal_id = $1 AND occurred_at >= date_trunc('month', now())",
    )
    .bind(principal)
    .fetch_one(db.pool())
    .await
    .expect("sum");
    assert_eq!(ledger, dec!(0.45), "the old row and two debits");

    let spend = spend_for(&db, key, principal).await.expect("spend");
    assert_eq!(
        spend.principal_usd, ledger,
        "the debit that landed mid-pass is still in the counter"
    );
    let row = route_by_id(&db, route)
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(row.spent_usd, ledger, "and in the route's");
}

/// Fixture: a budgeted principal and route with one debit already counted,
/// plus a builder for further debits of ten cents.
///
/// Its own function because the race test above is long enough without it,
/// and because the pieces only mean something together: reconcile touches
/// budgeted rows only, and the pre-existing row is what proves a pass
/// rewrites rather than merely adds.
async fn budgeted_principal(
    db: &Db,
) -> (Uuid, Uuid, Uuid, impl Fn() -> UsageWrite + Send + 'static) {
    let (principal, route, account) = seed(db).await;
    let key = capped_key(db, principal, route, format!("race-{}", Uuid::new_v4())).await;
    set_budgets(db, principal, route).await;
    old_binary_row(db, principal, key, route, "0.25").await;

    let write = move || UsageWrite {
        request_id: Uuid::new_v4(),
        attempt: 0,
        principal_id: Some(principal),
        api_key_id: Some(key),
        route_id: Some(route),
        account_id: Some(account.as_uuid()),
        model_id: "kimi-k2".to_owned(),
        tier: "cheap".to_owned(),
        selection_reason: "default".to_owned(),
        escalated_from_tier: None,
        escalation_gate: None,
        usage: oag_router::Usage::default(),
        cost_usd: dec!(0.10),
        counterfactual_usd: Decimal::ZERO,
        counterfactual_model_id: None,
        counterfactual_api_usd: Decimal::ZERO,
        status: 200,
        latency_ms: Some(10),
        ttft_ms: None,
        streamed: false,
    };
    (principal, route, key, write)
}

/// Fixture: one principal, one route, one shared credential joined to it.
async fn seed(db: &Db) -> (Uuid, Uuid, AccountId) {
    let tag = Uuid::new_v4();
    let principal: Uuid = sqlx::query_scalar(
        "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')
             RETURNING id",
    )
    .bind(format!("{tag}@example.invalid"))
    .fetch_one(db.pool())
    .await
    .expect("principal");

    let route: Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers, default_mode)
             VALUES (gen_random_uuid(), $1, '[{\"name\":\"cheap\",\"models\":[\"kimi-k2\"]}]'::jsonb, 'managed')
             RETURNING id",
        )
        .bind(format!("route-{tag}"))
        .fetch_one(db.pool())
        .await
        .expect("route");

    let account: Uuid = sqlx::query_scalar(
        "INSERT INTO account
                 (id, name, provider, kind, credentials_sealed, credentials_nonce)
             VALUES (gen_random_uuid(), $1, 'anthropic', 'api_key', '\\x00', '\\x00')
             RETURNING id",
    )
    .bind(format!("acct-{tag}"))
    .fetch_one(db.pool())
    .await
    .expect("account");

    sqlx::query("INSERT INTO account_route (account_id, route_id) VALUES ($1, $2)")
        .bind(account)
        .bind(route)
        .execute(db.pool())
        .await
        .expect("join");

    (principal, route, AccountId::from_uuid(account))
}

/// All four windows after one spend six hours ago and one just now: the five-hour window
/// holds only the recent one and frees up when it ages out; the day and the week hold both,
/// and each frees up when the older one leaves it; the month resets on the first.
///
/// The day was the one window with no assertion here, which left the only figure whose
/// bound sits *between* the two spends untested — the case that tells a correct window
/// from one that is merely wide enough.
fn assert_windows(usage: &KeyUsage) {
    assert_eq!(
        usage.five_hour_usd,
        dec!(0.500000),
        "the six-hour-old spend is outside"
    );
    assert_eq!(
        usage.day_usd,
        dec!(1.750000),
        "and inside the day, which is the window the six-hour-old spend \
             distinguishes: a five-hour bound excludes it and a day includes it"
    );
    assert_eq!(
        usage.seven_day_usd,
        dec!(2.000000),
        "the week holds the three-day-old spend the day excludes — the pair that \
             makes the two windows distinguishable at all"
    );
    let now = OffsetDateTime::now_utc();
    let frees = usage
        .five_hour_frees_at
        .expect("a non-empty window frees up");
    let minutes = (frees - now).whole_minutes();
    assert!(
        (4 * 60 + 58..=5 * 60).contains(&minutes),
        "the five-hour window frees up when its oldest (just-now) spend ages out: {frees}"
    );
    let frees = usage
        .seven_day_frees_at
        .expect("a non-empty window frees up");
    let hours = (frees - now).whole_hours();
    assert!(
        (4 * 24 - 1..=4 * 24).contains(&hours),
        "the seven-day window frees up when its OLDEST spend ages out, and that \
             is the three-day-old one — four days from now, not seven: {frees}"
    );
    assert!(
        usage.month_resets_at > now,
        "the month resets on the first of next month"
    );
}

/// A capped key on `principal`, straight into the table — `mint_key` would
/// do, but a test about the ledger should not depend on the mint path.
async fn capped_key(db: &Db, principal: Uuid, route: Uuid, name: String) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id, quota_usd)
             VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4, $5)
             RETURNING id",
    )
    .bind(hash_key(&name))
    .bind(&name)
    .bind(principal)
    .bind(route)
    .bind(Some(dec!(5.00)))
    .fetch_one(db.pool())
    .await
    .expect("mint")
}

fn test_db() -> Option<Db> {
    let url = std::env::var("OAG_TEST_DATABASE_URL").ok()?;
    Some(Db::connect(&url, 2).expect("connect"))
}

/// An organisation's points and its leak, in one pass, without naming a key.
///
/// The whole point of the principal scope: every coworker key an org mints
/// sits on one principal, so this answers "the org's spend" with no key
/// list and therefore no ceiling on how many coworkers an org may have.
///
/// The leak half uses `selection_reason`, never `counterfactual_api_usd =
/// 0`. A *served* row can be zero too — no tokens, or a model with no
/// price — so the fixture seeds one of those deliberately: if the
/// discriminator ever becomes the money, that row joins the leak and the
/// count goes up.
#[tokio::test]
async fn an_orgs_points_and_its_leak_come_from_one_pass_over_the_ledger() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;
    // Two keys on ONE principal: the shape an org has, and the thing a
    // key-list sum would have to enumerate.
    let a = capped_key(&db, principal, route, format!("org-a-{}", Uuid::new_v4())).await;
    let b = capped_key(&db, principal, route, format!("org-b-{}", Uuid::new_v4())).await;

    let write = |key: Uuid, reason: &str, cost: &str, api: &str| UsageWrite {
        request_id: Uuid::new_v4(),
        attempt: 0,
        principal_id: Some(principal),
        api_key_id: Some(key),
        route_id: Some(route),
        account_id: Some(account.as_uuid()),
        model_id: "kimi-k2".to_owned(),
        tier: "cheap".to_owned(),
        selection_reason: reason.to_owned(),
        escalated_from_tier: None,
        escalation_gate: None,
        usage: oag_router::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        cost_usd: cost.parse().expect("decimal"),
        counterfactual_usd: Decimal::ZERO,
        counterfactual_model_id: None,
        counterfactual_api_usd: api.parse().expect("decimal"),
        status: 200,
        latency_ms: Some(10),
        ttft_ms: None,
        streamed: false,
    };

    // Served, on both keys, so the aggregate has to span them.
    record_usage(&db, &write(a, "default", "1.00", "2.00"))
        .await
        .expect("record");
    record_usage(&db, &write(b, "default", "0.50", "1.00"))
        .await
        .expect("record");
    // A served row worth nothing: zero list price, real cost. This is the
    // trap — it looks exactly like a leak if you discriminate on money.
    record_usage(&db, &write(a, "default", "0.25", "0"))
        .await
        .expect("record");
    // The actual leak: paid for, served to nobody.
    record_usage(&db, &write(b, "abandoned", "0.20", "0"))
        .await
        .expect("record");
    record_usage(&db, &write(a, "lost", "0.05", "0"))
        .await
        .expect("record");

    let got = points_for_principal(
        &db,
        principal,
        UsageWindow::Month,
        Some(dec!(0.20)),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("principal points");

    // 3.00 of list price over R=0.20. The two unserved rows and the
    // zero-priced served row contribute nothing, for different reasons.
    assert_eq!(got.points, Some(15_000_000));
    // 0.20 + 0.05, and NOT the 0.25 served row that also priced at zero.
    assert_eq!(
        got.unbilled_cost_usd,
        dec!(0.250000),
        "the zero-priced SERVED row must not count as a leak"
    );
    assert_eq!(got.total_cost_usd, dec!(2.000000));
    assert_eq!(got.unserved_attempts, 2);

    // And it agrees with the per-key sum it exists to replace, which is
    // what makes it a scope change rather than a different number.
    let by_key = points_for_keys(
        &db,
        &[a, b],
        UsageWindow::Month,
        dec!(0.20),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("by key");
    let summed: i64 = by_key.iter().map(|(_, points)| *points).sum();
    assert_eq!(
        got.points,
        Some(summed),
        "the principal scope must be the same money as the key list, only \
             without the list"
    );

    // No reference is None, never zero: zero is a spend.
    let unpriced = points_for_principal(
        &db,
        principal,
        UsageWindow::Month,
        None,
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("no reference");
    assert_eq!(unpriced.points, None);
    assert_eq!(
        unpriced.unbilled_cost_usd,
        dec!(0.250000),
        "the leak is money and does not need a reference price"
    );
}

/// A rung is a property of a request, so one model used two ways is two
/// rows — and every attempt is counted beside the served ones.
///
/// Three things at once because they share a fixture and each would be
/// vacuous without the others: grouping by `(model_id, tier)` means nothing
/// unless a model actually spans two rungs, `attempts` means nothing unless
/// something went unserved, and the `'' -> None` mapping means nothing
/// unless a row was pinned directly.
#[tokio::test]
async fn one_model_reached_two_ways_is_two_rows_and_counts_what_it_did_not_serve() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;
    // Unique per run: `capped_key` hashes the name into `key_hash`, which is
    // unique, and `oag_g0` keeps every row a previous run left behind. A
    // fixed name here passes once and collides on the second run.
    let key = capped_key(
        &db,
        principal,
        route,
        format!("tier-split-{}", Uuid::new_v4()),
    )
    .await;

    let write = |tier: &str, reason: &str, api: &str| UsageWrite {
        request_id: Uuid::new_v4(),
        attempt: 0,
        principal_id: Some(principal),
        api_key_id: Some(key),
        route_id: Some(route),
        account_id: Some(account.as_uuid()),
        model_id: "kimi-k2".to_owned(),
        tier: tier.to_owned(),
        selection_reason: reason.to_owned(),
        escalated_from_tier: None,
        escalation_gate: None,
        usage: oag_router::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        cost_usd: dec!(0.10),
        counterfactual_usd: Decimal::ZERO,
        counterfactual_model_id: None,
        counterfactual_api_usd: api.parse().expect("decimal"),
        status: 200,
        latency_ms: Some(10),
        ttft_ms: None,
        streamed: false,
    };

    // Two served through the `cheap` rung, one abandoned on the same rung,
    // and two the caller pinned by name. An abandoned attempt records a
    // real `cost_usd` and a zero `counterfactual_api_usd` — the gateway
    // paid, the member is charged no points — which is exactly why the
    // count cannot be recovered from the money.
    for api in ["2.00", "1.00"] {
        record_usage(&db, &write("cheap", "default", api))
            .await
            .expect("record");
    }
    record_usage(&db, &write("cheap", "abandoned", "0"))
        .await
        .expect("record");
    for api in ["4.00", "1.00"] {
        record_usage(&db, &write("", "pinned", api))
            .await
            .expect("record");
    }

    let rows = key_usage_by_model(
        &db,
        key,
        UsageWindow::Month,
        Some(dec!(0.20)),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("by model");

    assert_eq!(
        rows.len(),
        2,
        "one model, two rungs, two rows — grouping by model_id alone gives one: {rows:?}"
    );

    let pinned = rows
        .iter()
        .find(|r| r.tier.is_none())
        .expect("the directly-pinned row, whose tier is null and not \"\"");
    let cheap = rows
        .iter()
        .find(|r| r.tier.as_deref() == Some("cheap"))
        .expect("the rung row");

    assert_eq!(cheap.model_id, pinned.model_id, "the same model, both ways");

    // The rung row: three tried, two served.
    assert_eq!(cheap.attempts, 3);
    assert_eq!(cheap.requests, 2);
    assert!(
        cheap.attempts > cheap.requests,
        "the abandoned one is counted"
    );
    // And the difference is invisible in the money: the abandoned attempt
    // added to `cost_usd` and nothing to `list_usd`.
    assert_eq!(cheap.list_usd, dec!(3.000000));
    assert_eq!(cheap.cost_usd, dec!(0.300000));

    // The pinned row: nothing unserved, so the two agree.
    assert_eq!((pinned.attempts, pinned.requests), (2, 2));
    assert_eq!(pinned.list_usd, dec!(5.000000));

    // Points follow list price, per row, so the rows do not share a total.
    assert_eq!(cheap.points, Some(15_000_000));
    assert_eq!(pinned.points, Some(25_000_000));
}

/// A key's usage is its OWN ledger rows: another key on the same principal
/// does not count, the month figure is the ledger's sum, and the lifetime
/// counter is what the cap is enforced against. An id that is not a key is
/// `None`, never a zeroed row.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one ledger, three windows, two keys: the setup is the test
async fn key_usage_reads_one_keys_ledger_and_its_cap() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;
    let email: String = sqlx::query_scalar("SELECT email FROM principal WHERE id = $1")
        .bind(principal)
        .fetch_one(db.pool())
        .await
        .expect("principal email");

    let own = capped_key(
        &db,
        principal,
        route,
        format!("usage-own-{}", Uuid::new_v4()),
    )
    .await;
    let theirs = capped_key(
        &db,
        principal,
        route,
        format!("usage-theirs-{}", Uuid::new_v4()),
    )
    .await;

    let write = |key: Uuid, cost: &str, api: &str| UsageWrite {
        request_id: Uuid::new_v4(),
        attempt: 0,
        principal_id: Some(principal),
        api_key_id: Some(key),
        route_id: Some(route),
        account_id: Some(account.as_uuid()),
        model_id: "kimi-k2".to_owned(),
        tier: "cheap".to_owned(),
        selection_reason: "default".to_owned(),
        escalated_from_tier: None,
        escalation_gate: None,
        usage: oag_router::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        cost_usd: cost.parse().expect("decimal"),
        counterfactual_usd: Decimal::ZERO,
        counterfactual_model_id: None,
        // What the same tokens would cost at the model's list API price — for a seat, the
        // bill it displaced while `cost_usd` stays truthfully what it is.
        counterfactual_api_usd: api.parse().expect("decimal"),
        status: 200,
        latency_ms: Some(10),
        ttft_ms: None,
        streamed: false,
    };
    let early = write(own, "1.25", "2.00");
    record_usage(&db, &early).await.expect("record");
    // Three days old: inside the week and the month, outside the day. Without a spend
    // between the two bounds, `day_usd` and `seven_day_usd` hold the same figure and a
    // day window widened to seven days passes every assertion here — which it did.
    let older = write(own, "0.25", "0.40");
    record_usage(&db, &older).await.expect("record");
    record_usage(&db, &write(own, "0.50", "0.80"))
        .await
        .expect("record");
    record_usage(&db, &write(theirs, "9.00", "9.00"))
        .await
        .expect("record");
    // The first spend happened six hours ago: inside the week and the month, outside the
    // five-hour window.
    sqlx::query(
        "UPDATE usage_event SET occurred_at = now() - interval '6 hours' WHERE request_id = $1",
    )
    .bind(early.request_id)
    .execute(db.pool())
    .await
    .expect("backdate");
    sqlx::query(
        "UPDATE usage_event SET occurred_at = now() - interval '3 days' WHERE request_id = $1",
    )
    .bind(older.request_id)
    .execute(db.pool())
    .await
    .expect("backdate");

    // Which backdated spends this calendar month holds, asked of the rows by
    // the bound `key_usage` uses. In a month's first three days the
    // three-day-old spend is last month's, and in its first six hours so is
    // the six-hour-old one. Fixed at "all three", this failed on the 1st to
    // the 3rd of every month. The rolling windows never move; the month's
    // figures are this month's rows and no others.
    let this_month = |request_id: Uuid| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT occurred_at >= date_trunc('month', now()) FROM usage_event \
                 WHERE request_id = $1",
            )
            .bind(request_id)
            .fetch_one(db.pool())
            .await
            .expect("a backdated row")
        }
    };
    let (early_this_month, older_this_month) = (
        this_month(early.request_id).await,
        this_month(older.request_id).await,
    );
    // Cost, list price and points at R = 0.20 of each spend the month holds.
    // The one written last was never backdated, so it is always among them.
    let month: Vec<(Decimal, Decimal, i64)> = [
        (true, dec!(0.50), dec!(0.80), 4_000_000),
        (early_this_month, dec!(1.25), dec!(2.00), 10_000_000),
        (older_this_month, dec!(0.25), dec!(0.40), 2_000_000),
    ]
    .into_iter()
    .filter_map(|(held, cost, list, points)| held.then_some((cost, list, points)))
    .collect();
    let month_cost: Decimal = month.iter().map(|m| m.0).sum();
    let month_list: Decimal = month.iter().map(|m| m.1).sum();
    let month_points: i64 = month.iter().map(|m| m.2).sum();
    let month_requests = i64::try_from(month.len()).expect("three at most");

    let usage = key_usage(&db, own, Some(dec!(0.20)))
        .await
        .expect("usage")
        .expect("the key exists");
    assert_eq!(usage.key_id, own);
    assert_eq!(usage.principal_email, email);
    assert!(usage.active);
    assert_eq!(usage.quota_usd, Some(dec!(5.000000)));
    assert_eq!(
        usage.spent_usd,
        dec!(2.000000),
        "the counter the cap is enforced against"
    );
    assert_eq!(usage.month_to_date_usd, month_cost, "this key's rows only");
    assert_eq!(usage.requests, month_requests);
    assert_windows(&usage);
    assert_eq!(
        usage.five_hour_requests, 1,
        "only the recent spend is inside five hours"
    );
    assert_eq!(usage.seven_day_requests, 3);
    assert_eq!(
        usage.month_counterfactual_usd, month_list,
        "the list-price bill the same tokens would have carried"
    );
    assert_eq!(usage.five_hour_counterfactual_usd, dec!(0.800000));
    assert_eq!(usage.seven_day_counterfactual_usd, dec!(3.200000));
    // The rolling day holds the six-hour-old spend and not the three-day-old one, which
    // is the only thing that tells this window from the week.
    assert_eq!(usage.day_usd, dec!(1.750000));
    assert_eq!(usage.day_requests, 2);
    assert_eq!(usage.day_counterfactual_usd, dec!(2.800000));
    assert!(usage.day_frees_at.is_some());
    // Points at R = 0.20: list price × 1e6 / 0.20, per request, summed.
    assert_eq!(
        usage.month_points,
        Some(month_points),
        "2.00, 0.80 and 0.40 at list price, as far as the month holds them"
    );
    assert_eq!(usage.five_hour_points, Some(4_000_000));
    assert_eq!(usage.day_points, Some(14_000_000));
    assert_eq!(usage.seven_day_points, Some(16_000_000));
    // Per model, inside the month and inside five hours.
    let by_model = key_usage_by_model(
        &db,
        own,
        UsageWindow::Month,
        Some(dec!(0.20)),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("by model");
    assert_eq!(by_model.len(), 1);
    assert_eq!(by_model[0].model_id, "kimi-k2");
    // The month, so every one of this key's spends the month holds.
    assert_eq!(by_model[0].requests, month_requests);
    assert_eq!(
        (by_model[0].input_tokens, by_model[0].output_tokens),
        (10 * month_requests, 5 * month_requests)
    );
    assert_eq!(by_model[0].cost_usd, month_cost);
    assert_eq!(by_model[0].list_usd, month_list);
    assert_eq!(by_model[0].points, Some(month_points));
    let recent = key_usage_by_model(
        &db,
        own,
        UsageWindow::FiveHours,
        None,
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("by model");
    assert_eq!(recent[0].requests, 1);
    assert_eq!(
        recent[0].points, None,
        "no reference, no points — never zero"
    );
    // The batch: three keys, one query; the empty one is absent.
    let pool = points_for_keys(
        &db,
        &[own, theirs],
        UsageWindow::Month,
        dec!(0.20),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("points");
    let of = |key: Uuid| pool.iter().find(|(k, _)| *k == key).map(|(_, p)| *p);
    assert_eq!(
        of(own),
        Some(month_points),
        "2.00, 0.80 and 0.40 over 0.20, as far as the month holds them"
    );
    assert_eq!(of(theirs), Some(45_000_000), "9.00 at list price over 0.20");

    let other = key_usage(&db, theirs, None)
        .await
        .expect("usage")
        .expect("the key exists");
    assert_eq!(other.month_to_date_usd, dec!(9.000000));
    assert_eq!(other.requests, 1);
    assert_eq!(other.month_points, None, "read without a reference");
    let empty_key = capped_key(
        &db,
        principal,
        route,
        format!("usage-empty-{}", Uuid::new_v4()),
    )
    .await;
    let empty = key_usage(&db, empty_key, Some(dec!(0.20)))
        .await
        .expect("usage")
        .expect("the key exists");
    assert_eq!(empty.five_hour_usd, dec!(0));
    assert_eq!(
        (
            empty.five_hour_requests,
            empty.seven_day_requests,
            empty.requests
        ),
        (0, 0, 0)
    );
    assert_eq!(empty.month_counterfactual_usd, dec!(0));
    assert_eq!(
        empty.month_points,
        Some(0),
        "a reference and no rows is zero points"
    );
    assert!(empty.day_frees_at.is_none());
    assert!(
        key_usage_by_model(
            &db,
            empty_key,
            UsageWindow::Day,
            Some(dec!(0.20)),
            OffsetDateTime::now_utc()
        )
        .await
        .expect("by model")
        .is_empty()
    );
    assert!(
        empty.five_hour_frees_at.is_none() && empty.seven_day_frees_at.is_none(),
        "an empty window has nothing to free up"
    );

    assert!(
        key_usage(&db, Uuid::new_v4(), None)
            .await
            .expect("usage")
            .is_none(),
        "an unknown id is None, not a zeroed row"
    );
}

#[tokio::test]
async fn the_points_reference_is_one_row_the_admin_replaces() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    // Another test may have set it; what this proves is replace-in-place and the read-back.
    set_points_reference(&db, dec!(0.20)).await.expect("set");
    assert_eq!(points_reference(&db).await.expect("read"), Some(dec!(0.20)));
    set_points_reference(&db, dec!(0.25))
        .await
        .expect("replace");
    assert_eq!(points_reference(&db).await.expect("read"), Some(dec!(0.25)));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM points_reference")
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(rows, 1, "one row, replaced, never a second");
    assert!(
        set_points_reference(&db, dec!(0)).await.is_err(),
        "the table refuses a price that is not positive even if a caller forgot to"
    );
}

#[test]
fn a_window_starts_where_it_says_and_frees_when_its_oldest_spend_ages_out() {
    use time::macros::datetime;
    let now = datetime!(2026-09-03 10:30:00 UTC);
    assert_eq!(UsageWindow::parse("5h"), Some(UsageWindow::FiveHours));
    assert_eq!(UsageWindow::parse("24h"), Some(UsageWindow::Day));
    assert_eq!(UsageWindow::parse("7d"), Some(UsageWindow::SevenDays));
    assert_eq!(UsageWindow::parse(" month "), Some(UsageWindow::Month));
    assert_eq!(UsageWindow::parse("1d"), None);
    assert_eq!(
        UsageWindow::Day.since(now),
        datetime!(2026-09-02 10:30:00 UTC)
    );
    assert_eq!(
        UsageWindow::Month.since(now),
        datetime!(2026-09-01 00:00:00 UTC)
    );
    let oldest = datetime!(2026-09-03 08:00:00 UTC);
    assert_eq!(
        UsageWindow::FiveHours.frees_at(Some(oldest), now),
        Some(datetime!(2026-09-03 13:00:00 UTC))
    );
    assert_eq!(
        UsageWindow::FiveHours.frees_at(None, now),
        None,
        "empty: nothing to free"
    );
    assert_eq!(
        UsageWindow::Month.frees_at(None, now),
        Some(datetime!(2026-10-01 00:00:00 UTC))
    );
    assert_eq!(
        UsageWindow::Month.frees_at(None, datetime!(2026-12-15 00:00:00 UTC)),
        Some(datetime!(2027-01-01 00:00:00 UTC))
    );
}

/// The queries whose SELECT lists name every column by hand.
///
/// `rows.rs` justifies hand-written `FromRow` structs on the grounds that a
/// column mistake "surfaces as a runtime error on first query, which the
/// integration tests catch" — but nothing exercised either query, so that
/// claim was unbacked until now. Both SELECT lists were edited in this
/// change, which is exactly when it needed to be true.
#[tokio::test]
async fn the_hand_written_account_selects_match_the_schema() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;

    let found = candidates(&db, route, "anthropic", principal)
        .await
        .expect("candidates must not fail on a column name");
    assert_eq!(
        found.len(),
        1,
        "the seeded credential should be a candidate"
    );

    let row = account_by_id(&db, account)
        .await
        .expect("account_by_id must not fail on a column name")
        .expect("exists");
    assert_eq!(row.provider, "anthropic");
    assert_eq!(row.max_concurrency, 8);
}

#[tokio::test]
async fn route_channels_hides_what_the_caller_cannot_use() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;

    assert_eq!(
        route_channels(&db, route, principal).await.expect("list"),
        vec![("anthropic".to_owned(), "api_key".to_owned(), None)],
        "the kind rides along, so the listing knows which qualifiers to offer, \
             and the served set so it knows which models each will take"
    );

    // Disabled is an operator decision, not a transient state: advertising
    // a model nobody can reach moves the failure away from its cause.
    set_schedulable(&db, account, false).await.expect("disable");
    assert!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .is_empty()
    );
    set_schedulable(&db, account, true).await.expect("enable");

    // A credential bound to someone else must not show up here either.
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')
             RETURNING id",
    )
    .bind(format!("other-{}@example.invalid", Uuid::new_v4()))
    .fetch_one(db.pool())
    .await
    .expect("other principal");
    sqlx::query("UPDATE account SET owner_principal_id = $2 WHERE id = $1")
        .bind(account.as_uuid())
        .bind(other)
        .execute(db.pool())
        .await
        .expect("bind");
    assert!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .is_empty(),
        "another principal's personal credential is not this caller's to see"
    );
}

#[tokio::test]
async fn route_channels_hides_a_spent_subscription_even_without_a_reserve() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;
    let id = account.as_uuid();

    // Unread remaining is unknown, not empty: a provider with no usage
    // endpoint must not vanish from the picker.
    assert_eq!(
        route_channels(&db, route, principal).await.expect("list"),
        vec![("anthropic".to_owned(), "api_key".to_owned(), None)]
    );

    sqlx::query("UPDATE account SET usage_remaining_pct = 50 WHERE id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("half remaining");
    assert_eq!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .len(),
        1,
        "headroom and no reserve is still a live credential"
    );

    sqlx::query("UPDATE account SET usage_remaining_pct = 0 WHERE id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("spent");
    assert!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .is_empty(),
        "a spent seat cannot serve today, reserve or not"
    );

    sqlx::query(
        "UPDATE account SET usage_remaining_pct = 20, usage_reserve_pct = 10 WHERE id = $1",
    )
    .bind(id)
    .execute(db.pool())
    .await
    .expect("above reserve");
    assert_eq!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .len(),
        1,
        "above the reserve is listed"
    );

    sqlx::query("UPDATE account SET usage_remaining_pct = 10 WHERE id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("at reserve");
    assert!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .is_empty(),
        "at the reserve line is held back"
    );
}

#[tokio::test]
async fn route_channel_status_includes_what_the_listing_hides() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;
    let id = account.as_uuid();

    assert_eq!(
        route_channel_status(&db, route, principal)
            .await
            .expect("status")
            .len(),
        1,
        "a live credential is visible to both the picker and the panel"
    );

    set_schedulable(&db, account, false).await.expect("disable");
    assert!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .is_empty(),
        "disabled is hidden from the picker"
    );
    let disabled = route_channel_status(&db, route, principal)
        .await
        .expect("status");
    assert_eq!(disabled.len(), 1);
    assert!(!disabled[0].schedulable);
    set_schedulable(&db, account, true).await.expect("enable");

    sqlx::query("UPDATE account SET usage_remaining_pct = 8, usage_reserve_pct = 15 WHERE id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("reserve");
    assert!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .is_empty(),
        "reserved is hidden from the picker"
    );
    let reserved = &route_channel_status(&db, route, principal)
        .await
        .expect("status")[0];
    assert_eq!(reserved.usage_remaining_pct, Some(dec!(8)));
    assert_eq!(reserved.usage_reserve_pct, Some(15));

    let until = OffsetDateTime::now_utc() + time::Duration::days(15);
    sqlx::query(
        "UPDATE account
                SET usage_remaining_pct = NULL, usage_reserve_pct = NULL,
                    rate_limited_until = $2
              WHERE id = $1",
    )
    .bind(id)
    .bind(until)
    .execute(db.pool())
    .await
    .expect("rate limit");
    assert!(
        route_channels(&db, route, principal)
            .await
            .expect("list")
            .is_empty(),
        "rate-limited is hidden from the picker"
    );
    let limited = &route_channel_status(&db, route, principal)
        .await
        .expect("status")[0];
    assert!(
        limited
            .rate_limited_until
            .is_some_and(|t| t > OffsetDateTime::now_utc())
    );

    // A credential bound to someone else is not this caller's to diagnose,
    // the same way it is not theirs to pick.
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')
             RETURNING id",
    )
    .bind(format!("other-{}@example.invalid", Uuid::new_v4()))
    .fetch_one(db.pool())
    .await
    .expect("other principal");
    sqlx::query("UPDATE account SET owner_principal_id = $2 WHERE id = $1")
        .bind(id)
        .bind(other)
        .execute(db.pool())
        .await
        .expect("bind");
    assert!(
        route_channel_status(&db, route, principal)
            .await
            .expect("status")
            .is_empty(),
        "another principal's personal credential is not this caller's to see"
    );
}

#[tokio::test]
async fn clearing_a_cooldown_leaves_the_providers_own_backoff_alone() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (_, _, account) = seed(&db).await;

    sqlx::query(
        "UPDATE account
                SET cooldown_until = now() + interval '1 hour',
                    cooldown_reason = 'test',
                    rate_limited_until = now() + interval '1 hour',
                    window_resets_at = now() + interval '1 hour'
              WHERE id = $1",
    )
    .bind(account.as_uuid())
    .execute(db.pool())
    .await
    .expect("cool down");

    clear_cooldown(&db, account).await.expect("clear");

    let row = account_by_id(&db, account)
        .await
        .expect("load")
        .expect("row");
    assert!(row.cooldown_until.is_none());
    assert!(
        row.rate_limited_until.is_some(),
        "rate_limited_until is the provider's own Retry-After; discarding it \
             fleet-wide turns a throttle into an account action"
    );
    assert!(row.window_resets_at.is_some());
}

#[tokio::test]
async fn admin_authority_is_carried_by_the_key_not_the_principal() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, _) = seed(&db).await;

    let mint = |key: &'static str, admin: bool| {
        let db = &db;
        async move {
            sqlx::query(
                "INSERT INTO api_key
                         (id, key_hash, key_prefix, name, principal_id, route_id, admin)
                     VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4, $5)",
            )
            .bind(hash_key(key))
            .bind(key)
            .bind(principal)
            .bind(route)
            .bind(admin)
            .execute(db.pool())
            .await
            .expect("mint");
        }
    };

    let plain = format!("plain-{}", Uuid::new_v4());
    let elevated = format!("admin-{}", Uuid::new_v4());
    let plain: &'static str = Box::leak(plain.into_boxed_str());
    let elevated: &'static str = Box::leak(elevated.into_boxed_str());
    mint(plain, false).await;
    mint(elevated, true).await;

    assert!(
        !authenticate(&db, plain)
            .await
            .expect("auth")
            .expect("found")
            .admin,
        "an inference key must not carry admin authority just because its \
             principal is an admin"
    );
    assert!(
        authenticate(&db, elevated)
            .await
            .expect("auth")
            .expect("found")
            .admin
    );
}

async fn insert_named_service(db: &Db, name: &str) -> ServiceRow {
    insert_service(
        db,
        &NewService {
            id: Uuid::now_v7(),
            name,
            kind: "sandbox",
            base_url: "http://127.0.0.1:9",
            health_path: "/health",
            dashboard_url: Some("http://127.0.0.1:9/ui"),
            auth_ref: None,
        },
    )
    .await
    .expect("insert service")
}

#[tokio::test]
async fn the_service_catalog_round_trips_and_records_health() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let tag = Uuid::new_v4();
    let name = format!("orgo-{tag}");

    let row = insert_named_service(&db, &name).await;
    assert!(row.enabled);
    assert!(row.last_ok.is_none());
    assert!(row.last_error.is_none());

    let listed = list_services(&db).await.expect("list");
    assert!(
        listed.iter().any(|s| s.id == row.id),
        "inserted service must appear in the catalog"
    );

    let ok = record_service_health(&db, row.id, true, None)
        .await
        .expect("record ok")
        .expect("exists");
    assert!(ok.last_ok.is_some());
    assert!(ok.last_error.is_none());

    let bad = record_service_health(&db, row.id, false, Some("health returned HTTP 503"))
        .await
        .expect("record err")
        .expect("exists");
    assert_eq!(bad.last_ok, ok.last_ok, "a failed probe must keep last_ok");
    assert_eq!(bad.last_error.as_deref(), Some("health returned HTTP 503"));

    set_service_enabled(&db, row.id, false)
        .await
        .expect("disable")
        .expect("exists");
    let disabled = service_by_id(&db, row.id)
        .await
        .expect("load")
        .expect("exists");
    assert!(!disabled.enabled);

    let updated = update_service(
        &db,
        row.id,
        &ServiceUpdate {
            name: &name,
            kind: "browser",
            base_url: "http://127.0.0.1:19",
            health_path: "/ready",
            dashboard_url: None,
            auth_ref: None,
            enabled: true,
        },
    )
    .await
    .expect("update")
    .expect("exists");
    assert_eq!(updated.kind, "browser");
    assert!(updated.enabled);
    assert!(updated.dashboard_url.is_none());
}

#[tokio::test]
async fn a_duplicate_service_name_is_a_config_error() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let name = format!("dup-{}", Uuid::new_v4());
    insert_named_service(&db, &name).await;
    let err = insert_service(
        &db,
        &NewService {
            id: Uuid::now_v7(),
            name: &name,
            kind: "tool",
            base_url: "http://127.0.0.1:9",
            health_path: "/health",
            dashboard_url: None,
            auth_ref: None,
        },
    )
    .await
    .expect_err("duplicate name");
    assert!(
        matches!(err, Error::Config(_)),
        "unique name must fail as Config, not Internal: {err}"
    );
}

#[tokio::test]
async fn auth_ref_must_point_at_an_existing_credential() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let err = insert_service(
        &db,
        &NewService {
            id: Uuid::now_v7(),
            name: &format!("ref-{}", Uuid::new_v4()),
            kind: "tool",
            base_url: "http://127.0.0.1:9",
            health_path: "/health",
            dashboard_url: None,
            auth_ref: Some(Uuid::now_v7()),
        },
    )
    .await
    .expect_err("missing credential");
    assert!(
        matches!(err, Error::Config(_)),
        "a dangling auth_ref is a config error: {err}"
    );
}

/// One client request can pay for several attempts, and every one of them
/// reaches the ledger.
///
/// 0014 contracted the key onto `(request_id, attempt)`, which is what the
/// expand half in 0003 was built for. Before it, the primary key on
/// `request_id` alone admitted the first row and `ON CONFLICT DO NOTHING`
/// silently dropped the rest — so an abandoned attempt never once landed,
/// and a lost one landed only by displacing the answer the client got.
///
/// What has to keep holding across that change is idempotence: a replayed
/// write of the same `(request_id, attempt)` is still a no-op, because that
/// is what makes a retried ledger write safe.
#[tokio::test]
async fn both_attempts_of_one_request_reach_the_ledger() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, account) = seed(&db).await;

    // The key this release contracted onto. Asserted rather than assumed,
    // because every row below lands or is dropped according to it.
    let pk: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
             WHERE conname = 'usage_event_pkey'",
    )
    .fetch_one(db.pool())
    .await
    .expect("the ledger has a primary key");
    assert_eq!(pk, "PRIMARY KEY (request_id, attempt)");

    let raw = format!("meter-{}", Uuid::new_v4());
    let key: Uuid = sqlx::query_scalar(
        "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id)
             VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4)
             RETURNING id",
    )
    .bind(hash_key(&raw))
    .bind(&raw)
    .bind(principal)
    .bind(route)
    .fetch_one(db.pool())
    .await
    .expect("mint");

    // One client request, two attempts: a cheap answer that tripped a
    // quality gate, and the retry a rung up that was actually served.
    let request_id = Uuid::new_v4();
    let write = |attempt: i16, reason: &str, cost: &str| UsageWrite {
        request_id,
        attempt,
        principal_id: Some(principal),
        api_key_id: Some(key),
        route_id: Some(route),
        account_id: Some(account.as_uuid()),
        model_id: "kimi-k2".to_owned(),
        tier: "cheap".to_owned(),
        selection_reason: reason.to_owned(),
        escalated_from_tier: None,
        escalation_gate: Some("Refusal".to_owned()),
        usage: oag_router::Usage {
            input_tokens: 1_000,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        cost_usd: cost.parse().expect("decimal"),
        counterfactual_usd: Decimal::ZERO,
        counterfactual_model_id: None,
        counterfactual_api_usd: Decimal::ZERO,
        status: 200,
        latency_ms: Some(10),
        ttft_ms: None,
        streamed: false,
    };

    // The served row still goes first, which is no longer load-bearing for
    // survival but is the order the request path writes in.
    record_usage(&db, &write(1, "escalated", "1.75"))
        .await
        .expect("served attempt");
    record_usage(&db, &write(0, "abandoned", "0.25"))
        .await
        .expect("abandoned attempt");

    let reasons: Vec<String> = sqlx::query_scalar(
        "SELECT selection_reason FROM usage_event
              WHERE request_id = $1 ORDER BY attempt",
    )
    .bind(request_id)
    .fetch_all(db.pool())
    .await
    .expect("read back");

    assert_eq!(
        reasons,
        vec!["abandoned".to_owned(), "escalated".to_owned()],
        "both attempts are in the ledger, each under its own dispatch number"
    );

    // Idempotence is the reason the conflict clause is there in the first
    // place, and it had to outlive the key change.
    record_usage(&db, &write(1, "escalated", "1.75"))
        .await
        .expect("replay");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE request_id = $1")
        .bind(request_id)
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(rows, 2, "a replayed write is still a no-op");
}

/// A key with no spend yet, and a way to build ledger writes against it.
async fn metered_key(db: &Db) -> (Uuid, impl Fn(Uuid, i16, &str, &str) -> UsageWrite) {
    let (principal, route, account) = seed(db).await;
    let raw = format!("meter-{}", Uuid::new_v4());
    let key: Uuid = sqlx::query_scalar(
        "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id)
             VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4)
             RETURNING id",
    )
    .bind(hash_key(&raw))
    .bind(&raw)
    .bind(principal)
    .bind(route)
    .fetch_one(db.pool())
    .await
    .expect("mint");

    let build = move |request_id: Uuid, attempt: i16, reason: &str, cost: &str| UsageWrite {
        request_id,
        attempt,
        principal_id: Some(principal),
        api_key_id: Some(key),
        route_id: Some(route),
        account_id: Some(account.as_uuid()),
        model_id: "kimi-k2".to_owned(),
        tier: "cheap".to_owned(),
        selection_reason: reason.to_owned(),
        escalated_from_tier: None,
        escalation_gate: None,
        usage: oag_router::Usage {
            input_tokens: 1_000,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        cost_usd: cost.parse().expect("decimal"),
        counterfactual_usd: Decimal::ZERO,
        counterfactual_model_id: None,
        counterfactual_api_usd: Decimal::ZERO,
        status: 200,
        latency_ms: Some(10),
        ttft_ms: None,
        streamed: false,
    };

    (key, build)
}

/// The denormalised counter the quota check reads.
async fn key_spend(db: &Db, key: Uuid) -> Decimal {
    sqlx::query_scalar("SELECT spent_usd FROM api_key WHERE id = $1")
        .bind(key)
        .fetch_one(db.pool())
        .await
        .expect("read spend")
}

/// Idempotence has to cover the debit, not just the row.
///
/// Metering retries: the write can fail after the row lands, and the caller
/// replays it. `ON CONFLICT DO NOTHING` makes the second insert a no-op, so
/// the spend it carries has already been counted — charging for it again
/// walks a key toward its quota on nothing but a retry.
#[tokio::test]
async fn a_replayed_write_does_not_debit_the_key_twice() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (key, write) = metered_key(&db).await;

    let request_id = Uuid::new_v4();
    record_usage(&db, &write(request_id, 0, "classified", "1.50"))
        .await
        .expect("first write");
    assert_eq!(key_spend(&db, key).await, dec!(1.50));

    record_usage(&db, &write(request_id, 0, "classified", "1.50"))
        .await
        .expect("replay");
    assert_eq!(
        key_spend(&db, key).await,
        dec!(1.50),
        "the replay added no ledger row, so it must add no spend either"
    );
}

/// The debit follows the row, and now every attempt has one.
///
/// `SUM(cost_usd)` over the ledger and `api_key.spent_usd` are two views of
/// the same money. Before 0014 the second attempt's row was dropped and its
/// debit had to be dropped with it, or the two would disagree with nothing
/// to reconcile against. Now both rows land, so both debits must.
#[tokio::test]
async fn every_attempt_that_lands_debits_the_key() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");

    let (key, write) = metered_key(&db).await;
    let request_id = Uuid::new_v4();

    record_usage(&db, &write(request_id, 1, "escalated", "1.75"))
        .await
        .expect("served attempt");
    record_usage(&db, &write(request_id, 0, "abandoned", "0.25"))
        .await
        .expect("abandoned attempt");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE request_id = $1")
        .bind(request_id)
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(rows, 2, "both attempts are rows");
    assert_eq!(
        key_spend(&db, key).await,
        dec!(2.00),
        "the provider generated and invoiced both, so the key pays for both"
    );

    // And a replay of either still moves nothing.
    record_usage(&db, &write(request_id, 0, "abandoned", "0.25"))
        .await
        .expect("replay");
    assert_eq!(key_spend(&db, key).await, dec!(2.00), "replay is a no-op");
}

/// S4. A debit reaches the counter at the ledger's own scale.
///
/// `usage_event.cost_usd` is `numeric(14,8)` and the three denormalised
/// counters were `numeric(14,6)`, so `SET spent_usd = spent_usd +
/// ins.cost_usd` rounded every debit on assignment. A cheap-rung request
/// costs on the order of $0.0001; one costing less than $0.0000005 debited
/// nothing at all, and traffic made of such requests spent real money
/// against a cap that never moved.
///
/// It also put the reconciler in an argument it could not win: 0012 exists
/// so a budget is enforced against a number that cannot be stale, and that
/// number was compared for equality against an eight-place ledger sum it
/// could not represent.
#[tokio::test]
async fn a_debit_keeps_every_place_the_ledger_recorded() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (key, build) = metered_key(&db).await;

    // Eight places, the last two of which the old column could not hold.
    let cost = "0.00000123";
    record_usage(&db, &build(Uuid::new_v4(), 0, "classified", cost))
        .await
        .expect("record");

    assert_eq!(
        key_spend(&db, key).await,
        dec!(0.00000123),
        "the counter rounded the debit away: at six places this is 0.000001"
    );

    // And the property the reconciler depends on: counter == ledger sum,
    // exactly, with no tolerance.
    let ledger: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_event WHERE api_key_id = $1",
    )
    .bind(key)
    .fetch_one(db.pool())
    .await
    .expect("sum");
    assert_eq!(
        key_spend(&db, key).await,
        ledger,
        "the counter and the ledger have to be comparable for equality"
    );

    // Repeated, because rounding on assignment compounds: a hundred of
    // these is where a six-place counter and the ledger visibly part.
    for _ in 0..99 {
        record_usage(&db, &build(Uuid::new_v4(), 0, "classified", cost))
            .await
            .expect("record");
    }
    assert_eq!(
        key_spend(&db, key).await,
        dec!(0.00012300),
        "a hundred debits of 0.00000123 are 0.000123, not 0.0001"
    );
}

/// S2. A prefix collision revokes several keys, and says so.
///
/// `key_prefix` is the displayed half of a key and carries no unique index,
/// so this UPDATE has always been able to match more than one row. The
/// caller took `fetch_optional`, which keeps the first and drops the rest —
/// so on a collision the other keys were deactivated in the database while
/// their hashes were never evicted from the shared cache, and they went on
/// authenticating from L2 for its full TTL. An operator revoking a leaked
/// key during an incident was told one key was dealt with, got one
/// eviction, and could have had the leaked one still working.
#[tokio::test]
async fn revoking_by_prefix_returns_every_key_that_shared_it() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (principal, route, _) = seed(&db).await;

    // Two keys, one displayed prefix. Nothing prevents this: the prefix is
    // a display string, not an identifier.
    let shared = format!("oag_live_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let mut hashes = Vec::new();
    for name in ["the-leaked-one", "someone-elses"] {
        let raw = format!("{name}-{}", Uuid::new_v4());
        let hash = hash_key(&raw);
        sqlx::query(
            "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id)
                 VALUES (gen_random_uuid(), $1, $2, $3, $4, $5)",
        )
        .bind(&hash)
        .bind(&shared)
        .bind(name)
        .bind(principal)
        .bind(route)
        .execute(db.pool())
        .await
        .expect("mint");
        hashes.push(hash);
    }

    let revoked = revoke_key_by_prefix(&db, &shared).await.expect("revoke");
    assert_eq!(
        revoked.len(),
        2,
        "both keys carried the prefix and both were deactivated, so the \
             caller has to be told about both — it is the hashes it evicts"
    );
    let returned: std::collections::HashSet<&str> =
        revoked.iter().map(|(h, _, _)| h.as_str()).collect();
    for hash in &hashes {
        assert!(
            returned.contains(hash.as_str()),
            "a key was switched off in the database and its hash never came \
                 back, so nothing evicts it and it authenticates until the TTL"
        );
    }

    // Both rows really are inactive, and a second call finds nothing left:
    // the UPDATE is scoped to `active`, so this is not idempotent by
    // accident but by the predicate.
    let still_active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM api_key WHERE key_prefix = $1 AND active")
            .bind(&shared)
            .fetch_one(db.pool())
            .await
            .expect("count");
    assert_eq!(still_active, 0);
    assert!(
        revoke_key_by_prefix(&db, &shared)
            .await
            .expect("revoke again")
            .is_empty(),
        "nothing active left to revoke"
    );
}

/// C4: the reason 0016 gives for dropping `account_schedulable_idx`.
///
/// Its first draft said the seat poller was the only query filtering
/// `schedulable`. `route_channels` does too, and the claim was
/// load-bearing — had a `schedulable` query led with `provider`, dropping
/// the index would have been a regression rather than a saving.
///
/// So this asks the planner rather than arguing. The index is recreated
/// exactly as 0013 defined it, both queries are explained, and neither may
/// choose it. Recreated and dropped inside the test because 0016 has
/// already removed it: asserting that a plan does not use an index that
/// does not exist would pass for the wrong reason, which is the shape of
/// check this whole review was about.
///
/// The first draft asked the wrong question, though: whether any plan
/// *named* the index. Ask the planner for an index plan — `SET LOCAL
/// enable_seqscan = off` — and it names it, every time, for any partial
/// index on `schedulable`: it bitmap-scans the whole index and rechecks on
/// the heap. `Recheck Cond: schedulable` with `Filter: (kind = ...)` is the
/// planner using the index's *predicate* and ignoring its columns, which is
/// available from any index with that `WHERE` and is not what 0013 was for.
/// So the first draft would have reported a regression that was not one the
/// moment anybody made it ask.
///
/// The question that means something is whether the index's columns did any
/// work, and the plan answers it: `Index Cond` under the scan node is the
/// planner saying they narrowed the search. That is asserted here, and the
/// control below proves the arrangement can say yes — the same queries,
/// the same rows, an index whose columns the poller genuinely uses
/// (`ON account (kind) WHERE schedulable`) produces exactly the `Index
/// Cond` the real one does not.
///
/// The rows are seeded inside the same rolled-back transaction so the plan
/// does not depend on what other tests happen to have left in `account`.
#[tokio::test]
async fn an_index_on_provider_cannot_serve_a_query_without_one() {
    // `route_channels`: filters `schedulable`, does not bound `provider`.
    const ROUTE_CHANNELS: &str = "EXPLAIN SELECT DISTINCT a.provider, a.kind, a.served_models \
             FROM account a JOIN account_route ar ON ar.account_id = a.id \
             WHERE ar.route_id = $1 AND a.schedulable \
               AND (a.owner_principal_id IS NULL OR a.owner_principal_id = $2)";
    // The seat poller: same predicate, same absence of a provider bound.
    const POLLER: &str = "EXPLAIN SELECT id FROM account WHERE kind = 'oauth' AND schedulable";

    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");

    // Every plan is taken inside a transaction with sequential scans
    // disabled, so a plan that does not use the index is the planner
    // refusing it rather than the planner never having looked.
    let explain = async |index: &'static str| -> [Vec<String>; 2] {
        let mut tx = db.pool().begin().await.expect("begin");
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .expect("ask for the index plan");
        // Private copies of the two tables, which shadow the real ones for
        // every unqualified name below (`pg_temp` is searched first), so
        // the queries under test are unchanged. On the shared table this
        // flaked: other tests update `account` concurrently, and an index
        // built over broken HOT chains is marked `indcheckxmin`, which
        // makes it invisible to the transaction that created it. The
        // planner then had no index to take, seq-scanned at the disabled
        // cost, and the control "could not say yes". A fresh table has no
        // HOT chains, and no other test's rows.
        for copy in [
            "CREATE TEMP TABLE account (LIKE public.account INCLUDING DEFAULTS) \
                 ON COMMIT DROP",
            "CREATE TEMP TABLE account_route (LIKE public.account_route INCLUDING DEFAULTS) \
                 ON COMMIT DROP",
        ] {
            sqlx::query(copy)
                .execute(&mut *tx)
                .await
                .expect("a private copy of the table");
        }
        // And the copies are what the queries will read. Were `pg_temp`
        // not first on the search path, every name below would resolve to
        // the shared tables again and this test would be back to flaking
        // -- or worse, passing on whatever other tests left there.
        let shadowed: Vec<String> = sqlx::query_scalar(
            "SELECT n.nspname::text FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.oid IN ('account'::regclass, 'account_route'::regclass)",
        )
        .fetch_all(&mut *tx)
        .await
        .expect("where the names resolve");
        assert!(
            shadowed.len() == 2 && shadowed.iter().all(|n| n.starts_with("pg_temp")),
            "the plans would be taken on the shared tables: {shadowed:?}"
        );
        sqlx::query(index)
            .execute(&mut *tx)
            .await
            .expect("create the index under test");
        // Enough rows for an index plan to be worth considering: below a
        // page or so the planner seq-scans whatever exists, and then this
        // test proves nothing. Seeded inside the transaction, so the table
        // is left exactly as it was found however this run ends.
        sqlx::query(
            "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
                 credentials_nonce, priority) \
                 SELECT gen_random_uuid(), 'c4-' || i, \
                        (ARRAY['anthropic','kimi','openai','xai'])[1 + (i % 4)], \
                        'oauth', '\\x00', '\\x00', (i % 8)::smallint \
                 FROM generate_series(1, 64) i",
        )
        .execute(&mut *tx)
        .await
        .expect("seed accounts");
        sqlx::query("ANALYZE account")
            .execute(&mut *tx)
            .await
            .expect("statistics, or the planner is guessing at row counts");

        let route_channels = sqlx::query_scalar::<_, String>(ROUTE_CHANNELS)
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .fetch_all(&mut *tx)
            .await
            .expect("explain route_channels");
        let poller = sqlx::query_scalar::<_, String>(POLLER)
            .fetch_all(&mut *tx)
            .await
            .expect("explain the poller");
        // Rolled back, so the index never outlives the plan it was made
        // for and the schema stays as 0016 left it.
        tx.rollback().await.expect("rollback");
        // Kept apart. A plan's root node carries no `->`, so a window that
        // scanned forward from the last node of one plan would run into
        // the next and could match its `Index Cond` — the cross-plan
        // coupling the first fix to this test removed on one side only.
        [route_channels, poller]
    };

    // Did the index supply a bound anywhere, or was it merely walked?
    // `Index Cond` under a scan node is the planner saying the index's own
    // columns narrowed the search; without one it read every entry and
    // filtered on the heap, which a partial index on `schedulable` allows
    // whatever it is keyed on.
    //
    // *Every* occurrence in a plan, not the first, and each plan on its
    // own. The index can appear in either query's plan, and which one is a
    // property of the table's statistics rather than of the index: with
    // `account` nearly empty the planner takes the partial index for
    // `route_channels` too, as a plain Index Scan with a `Filter` and no
    // `Index Cond` — and a check that stopped at the first occurrence
    // concluded the control could not say yes, failing on a fresh database
    // while passing on a developer's populated one.
    let bounded = |plan: &[String]| {
        plan.iter().enumerate().any(|(i, line)| {
            line.contains("account_schedulable_idx")
                && plan[i + 1..]
                    .iter()
                    .take_while(|l| !l.contains("->"))
                    .any(|l| l.contains("Index Cond:"))
        })
    };
    let bounded_anywhere = |plans: &[Vec<String>; 2]| plans.iter().any(|p| bounded(p));

    // The control. An index whose columns the poller can genuinely use, so
    // a "no" below is a real answer.
    let control =
        explain("CREATE INDEX account_schedulable_idx ON account (kind) WHERE schedulable").await;
    assert!(
        bounded_anywhere(&control),
        "the arrangement cannot say yes, so its no would mean nothing:\n{}",
        control.concat().join("\n")
    );

    // And 0013's index, on the same table, the same rows, the same settings.
    let plans = explain(
        "CREATE INDEX account_schedulable_idx ON account (provider, priority) \
             WHERE schedulable",
    )
    .await;
    assert!(
        !bounded_anywhere(&plans),
        "a query bounds its search with account_schedulable_idx, so 0016 \
             dropped an index something needed:\n{}",
        plans.concat().join("\n")
    );
}

/// C7: 0015 says "nothing rounds on the way out". It has to be true.
///
/// The migration widened the three spend counters to `numeric(16,8)`, the
/// scale the ledger already carries, so a debit is no longer rounded on the
/// way in. Every read path then cast its ledger sums back to
/// `numeric(14,6)` — while returning the widened counter beside them
/// untouched — so a panel could show a key's `spent_usd` and its own ledger
/// sum disagreeing in the last two digits, which is the exact symptom 0015
/// claims to have removed.
///
/// A single sub-cent debit is enough to see it: at six places
/// `0.00000001` reads as `0.00000000`.
#[tokio::test]
async fn a_sub_cent_debit_survives_the_read_path() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (key, build) = metered_key(&db).await;

    let tiny: Decimal = "0.00000001".parse().expect("decimal");
    record_usage(&db, &build(Uuid::now_v7(), 0, "classified", "0.00000001"))
        .await
        .expect("record");

    let usage = key_usage(&db, key, None)
        .await
        .expect("read the panel")
        .expect("the key exists");
    assert_eq!(
        usage.month_to_date_usd, tiny,
        "the ledger sum was rounded on the way out, so it no longer matches \
             the counter 0015 widened to hold it"
    );

    let counter: Decimal = sqlx::query_scalar("SELECT spent_usd FROM api_key WHERE id = $1")
        .bind(key)
        .fetch_one(db.pool())
        .await
        .expect("read the counter");
    assert_eq!(
        usage.month_to_date_usd, counter,
        "the panel and the counter must agree to the last place: they are \
             the same money, read two ways"
    );
}

/// S1. The usage panels read a window, not a key's whole history.
///
/// Both queries state their windows inside `FILTER` clauses, which decide
/// what each aggregate counts and nothing about what the join reads. With
/// no bound in the `ON`, the join walked every row the key or principal had
/// ever written in order to report a rolling five hours — and `key_usage`
/// is the query a partner service calls before each model call, per member.
/// The ledger-partitioning design doc classifies both as range-bounded;
/// that premise was wrong, which is why this asserts the plan and not the
/// numbers. The numbers were always right. They just cost a scan.
///
/// What is asserted is that the window bound reaches the *index condition*
/// rather than being applied after rows are read. That is the difference
/// between a range scan and a full walk, and it is a property of the query
/// rather than of how much data happens to be in the table — so the test
/// says nothing about which plan the planner prefers today, and turning
/// sequential scans off is how it asks the question it actually means.
#[tokio::test]
async fn the_usage_panels_bound_the_ledger_side_of_their_joins() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (key, build) = metered_key(&db).await;
    for _ in 0..32 {
        record_usage(&db, &build(Uuid::new_v4(), 0, "classified", "0.01"))
            .await
            .expect("seed");
    }
    // The same rows carry a `principal_id`, so one seed serves both panels.
    let principal_email: String = sqlx::query_scalar(
        "SELECT p.email FROM principal p JOIN api_key k ON k.principal_id = p.id \
             WHERE k.id = $1",
    )
    .bind(key)
    .fetch_one(db.pool())
    .await
    .expect("the principal that owns the key");

    let mut tx = db.pool().begin().await.expect("begin");
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .expect("ask for the index plan");

    // The statement `key_usage` runs, not a copy of its join. This test
    // used to inline the join "verbatim from its `FROM` onwards", so
    // deleting S1's window bound from the real query left it green — which
    // is the failure this whole group is about. `EXPLAIN` on the const is
    // the only form that cannot drift from what runs.
    // `Box::leak` because sqlx refuses a non-`'static` statement — the same
    // "static SQL only" rule the rest of this crate follows, enforced by
    // the driver. One leaked string per test run is the cost of planning
    // the real query instead of a copy, and it is a cost worth paying
    // exactly once.
    let explain: &'static str = Box::leak(format!("EXPLAIN {KEY_USAGE_SQL}").into_boxed_str());
    let plan: String = sqlx::query_scalar(explain)
        .bind(key)
        .bind(Option::<Decimal>::None)
        .fetch_all(&mut *tx)
        .await
        .map(|rows: Vec<String>| rows.join("\n"))
        .expect("explain");
    tx.rollback().await.expect("rollback");

    let ledger_cond = plan
        .lines()
        .skip_while(|l| !l.contains("usage_event"))
        .find(|l| l.contains("Index Cond:"))
        .unwrap_or_else(|| panic!("the ledger side of the join is not indexed:\n{plan}"));
    assert!(
        ledger_cond.contains("occurred_at"),
        "the window bound must be part of the index range, not a filter \
             applied to every row the key has ever written:\n{plan}"
    );
    assert!(
        ledger_cond.contains("api_key_id"),
        "and it rides on `usage_event_key_idx`, which leads with the key:\n{plan}"
    );

    // And `principal_usage`, whose join carries the same bound and whose
    // panel is read on every budget check. S1 moved the bound in both
    // statements; only one of them had an assertion, so deleting it from
    // this one left the group green.
    let mut tx = db.pool().begin().await.expect("begin");
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .expect("ask for the index plan");
    let explain: &'static str =
        Box::leak(format!("EXPLAIN {PRINCIPAL_USAGE_SQL}").into_boxed_str());
    let plan: String = sqlx::query_scalar(explain)
        .bind(principal_email)
        .fetch_all(&mut *tx)
        .await
        .map(|rows: Vec<String>| rows.join("\n"))
        .expect("explain");
    tx.rollback().await.expect("rollback");

    let ledger_cond = plan
        .lines()
        .skip_while(|l| !l.contains("usage_event"))
        .find(|l| l.contains("Index Cond:"))
        .unwrap_or_else(|| panic!("the ledger side of the join is not indexed:\n{plan}"));
    assert!(
        ledger_cond.contains("occurred_at"),
        "a month's figures must not be read out of a principal's whole \
             history:\n{plan}"
    );
    assert!(
        ledger_cond.contains("principal_id"),
        "and it rides on `usage_event_principal_idx`:\n{plan}"
    );
}

/// A lost stream and the retry that replaced it: two rows, one request.
///
/// The credential generated an answer and the connection died before it was
/// whole; another credential served the retry. The provider will invoice
/// both generations, so both are rows and both are money — but the client
/// made one request, and every count over the ledger has to keep saying so.
///
/// That split is the whole reason the request counts filter on
/// `selection_reason` and the spend sums do not. Before 0014 the question
/// could not arise: the lost row displaced the served one, so the ledger
/// held a single row that was 502, cost the retry nothing, and carried no
/// counterfactual — the answer the client actually got was unbillable.
#[tokio::test]
async fn a_lost_attempt_and_its_retry_are_two_rows_but_one_request() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");

    let (key, build) = metered_key(&db).await;
    let request_id = Uuid::new_v4();

    // Attempt 0: generated, then the stream died. 502, and no
    // counterfactual — there is one baseline per client request and the
    // served row below is the row that carries it.
    let mut lost = build(request_id, 0, "lost", "0.40");
    lost.status = 502;

    // Attempt 1: what the client was served, priced against the ladder's
    // ceiling like any other served answer.
    let mut served = build(request_id, 1, "classified", "1.10");
    served.counterfactual_usd = dec!(9.00);
    served.counterfactual_model_id = Some("anthropic/claude-opus-5".to_owned());

    record_usage(&db, &served).await.expect("served attempt");
    record_usage(&db, &lost).await.expect("lost attempt");

    let rows: Vec<(i16, String, i16, Decimal, Decimal)> = sqlx::query_as(
        "SELECT attempt, selection_reason, status, cost_usd, counterfactual_usd
               FROM usage_event WHERE request_id = $1 ORDER BY attempt",
    )
    .bind(request_id)
    .fetch_all(db.pool())
    .await
    .expect("read back");

    assert_eq!(
        rows,
        vec![
            (0, "lost".to_owned(), 502, dec!(0.40), Decimal::ZERO),
            (1, "classified".to_owned(), 200, dec!(1.10), dec!(9.00)),
        ],
        "the lost attempt sits beside the served one rather than in place of it"
    );

    // Both were generated, so both are debited.
    assert_eq!(
        key_spend(&db, key).await,
        dec!(1.50),
        "the key pays for the generation that was lost as well as the one served"
    );

    let usage = key_usage(&db, key, None)
        .await
        .expect("read the key's usage")
        .expect("the key exists");
    assert_eq!(
        usage.month_to_date_usd,
        dec!(1.50),
        "spend counts every attempt: leaving the lost one out is what made \
             failover look free"
    );
    assert_eq!(
        usage.requests, 1,
        "and the request count does not: one client request was made, however \
             many generations it took to answer it"
    );
}

/// A catalog row as a seed builds one: no label, because a seed has no
/// opinion about what to call anything.
fn seed_model(id: &str, input: Decimal) -> ModelRow {
    ModelRow {
        id: id.to_owned(),
        provider: "xai".to_owned(),
        upstream_name: "grok-4.6".to_owned(),
        input_per_mtok: input,
        output_per_mtok: input * Decimal::from(4),
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
        context_window: 131_072,
        max_output_tokens: 8_192,
        supports_vision: false,
        supports_tools: false,
        supports_reasoning: false,
        supports_prompt_cache: false,
        display_label: None,
    }
}

#[test]
fn the_upsert_refreshes_prices_without_naming_the_label_column() {
    // The guard against the whole failure, readable without a database:
    // `display_label` may appear in the INSERT, never in the conflict
    // branch. The moment it joins that list, every re-seed writes NULL over
    // whatever an operator called the model — and a nightly price sync
    // makes renaming look like it silently stopped working.
    let conflict = UPSERT_MODEL_SQL
        .split_once("DO UPDATE SET")
        .expect("the upsert has a conflict branch")
        .1;
    // The assignments alone: comments stripped, because the one above the
    // WHERE clause explains this very rule and names the column while doing
    // it, and cut at the WHERE, which reads `is_override` on purpose.
    let assignments: String = conflict
        .lines()
        .take_while(|l| !l.trim_start().starts_with("WHERE"))
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !assignments.contains("display_label"),
        "a refresh must not carry a label: {assignments}"
    );
    assert!(
        !assignments.contains("is_override"),
        "nor the override flag it is modelled on: {assignments}"
    );
    assert!(
        assignments.contains("input_per_mtok"),
        "the prices really are refreshed: {assignments}"
    );
}

#[tokio::test]
async fn an_operators_label_outlives_a_reseed_and_a_price_sync() {
    // The reason the column is not in the conflict branch, end to end. An
    // operator renames a model once; a nightly LiteLLM seed and a provider
    // price sync both run over it afterwards, and neither knows the name
    // exists. If either carried the column, the rename would last until the
    // next tick and nobody would connect the two.
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");

    let id = format!("xai/grok-{}", Uuid::new_v4());
    upsert_model(&db, &seed_model(&id, dec!(3)), false)
        .await
        .expect("seed");

    let labelled = set_model_label(&db, &id, Some("Grok, the fast one"))
        .await
        .expect("label");
    assert_eq!(labelled.as_deref(), Some(id.as_str()));

    // A second seed, exactly as `oag admin seed-catalog` runs it.
    upsert_model(&db, &seed_model(&id, dec!(5)), false)
        .await
        .expect("reseed");
    // And a native price sync, which takes the other write path.
    assert!(
        update_model_prices(&db, &id, dec!(7), dec!(28), Some(dec!(0.7)))
            .await
            .expect("reprice")
    );

    let row = catalog(&db)
        .await
        .expect("catalog")
        .into_iter()
        .find(|m| m.id == id)
        .expect("the row is still there");
    assert_eq!(
        row.display_label.as_deref(),
        Some("Grok, the fast one"),
        "a seed and a sync both know nothing about names"
    );
    // The prices did move, so this is not a row nothing touched.
    assert_eq!(row.input_per_mtok, dec!(7));

    // And clearing it is a distinct state from naming it the derived
    // default: the row goes back to following the provider's spelling.
    set_model_label(&db, &id, None).await.expect("clear");
    let row = catalog(&db)
        .await
        .expect("catalog")
        .into_iter()
        .find(|m| m.id == id)
        .expect("row");
    assert_eq!(row.display_label, None);
    assert_eq!(row.derived_label(), "xAI: grok-4.6");

    assert_eq!(
        set_model_label(&db, "xai/not-a-model", Some("x"))
            .await
            .expect("query"),
        None,
        "renaming a model that does not exist is the caller's 404"
    );
}

#[tokio::test]
async fn the_catalog_select_matches_the_schema() {
    // `rows.rs` takes hand-written `FromRow` structs on the grounds that a
    // column mistake shows up as a runtime error on the first query. That
    // is only true if something runs the query, and this SELECT grew a
    // column in this change.
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let id = format!("xai/grok-{}", Uuid::new_v4());
    upsert_model(&db, &seed_model(&id, dec!(3)), false)
        .await
        .expect("seed");

    let rows = catalog(&db)
        .await
        .expect("catalog must not fail on a column name");
    assert!(rows.iter().any(|m| m.id == id));
}

/// 0018: the database refuses a Claude subscription that could serve, and the
/// kinds no adapter ever served, whatever wrote the row. Each refusal names the
/// constraint it trips, so a constraint dropped by a later migration fails here
/// by name rather than by a count. 0020 took `service_account` back for Vertex,
/// which leaves `vertex` as the dead kind. Every kind the column admits is
/// written once at the end, so a later migration that drops one fails here.
#[tokio::test]
async fn the_schema_refuses_a_claude_subscription_that_can_serve_and_a_dead_kind() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let insert = |provider: &'static str, kind: &'static str, schedulable: bool| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                // Every row owned, so each insert trips only the constraint it
                // is about and not 0019's "a seat has an owner".
                "WITH owner AS (INSERT INTO principal (id, email) \
                 VALUES (gen_random_uuid(), 'owner-' || gen_random_uuid() || '@test') \
                 RETURNING id) \
                 INSERT INTO account (id, name, provider, kind, credentials_sealed, \
                 credentials_nonce, schedulable, owner_principal_id) \
                 SELECT gen_random_uuid(), $1, $2, $3, '\\x00', '\\x00', $4, owner.id \
                 FROM owner RETURNING id",
            )
            .bind(format!("guard-{}", Uuid::new_v4()))
            .bind(provider)
            .bind(kind)
            .bind(schedulable)
            .fetch_one(db.pool())
            .await
        }
    };

    for (provider, kind, constraint) in [
        (
            "anthropic",
            "oauth",
            "account_claude_subscription_never_serves",
        ),
        ("gemini", "vertex", "account_kind_check"),
    ] {
        let refused = insert(provider, kind, true)
            .await
            .expect_err(&format!("{provider}/{kind} must not be insertable"));
        assert!(
            refused.to_string().contains(constraint),
            "{provider}/{kind} tripped the wrong thing: {refused}"
        );
    }

    // A Claude plan that only books imported Claude Code usage is allowed —
    // it can never be leased — and switching it on is what stays refused.
    let plan = insert("anthropic", "oauth", false)
        .await
        .expect("an accounting-only Claude plan");
    let switched_on = sqlx::query("UPDATE account SET schedulable = true WHERE id = $1")
        .bind(plan)
        .execute(db.pool())
        .await
        .expect_err("enabling a Claude subscription must be refused");
    assert!(
        switched_on
            .to_string()
            .contains("account_claude_subscription_never_serves"),
        "{switched_on}"
    );

    for (provider, kind) in [
        ("anthropic", "api_key"),
        ("xai", "oauth"),
        ("bedrock", "bedrock"),
        ("vertex-eu", "service_account"),
    ] {
        insert(provider, kind, true)
            .await
            .unwrap_or_else(|e| panic!("{provider}/{kind}: {e}"));
    }
}

/// A seat on `route` for `provider`, owned by `owner`, straight into the table.
async fn seat_on(db: &Db, route: Uuid, provider: &str, kind: &str, owner: Option<Uuid>) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce, \
         owner_principal_id) VALUES (gen_random_uuid(), $1, $2, $3, '\\x00', '\\x00', $4) \
         RETURNING id",
    )
    .bind(format!("{kind}-{}", Uuid::new_v4()))
    .bind(provider)
    .bind(kind)
    .bind(owner)
    .fetch_one(db.pool())
    .await
    .expect("account");
    sqlx::query("INSERT INTO account_route (account_id, route_id) VALUES ($1, $2)")
        .bind(id)
        .bind(route)
        .execute(db.pool())
        .await
        .expect("join");
    id
}

async fn another_principal(db: &Db) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO principal (id, email) VALUES (gen_random_uuid(), $1) RETURNING id",
    )
    .bind(format!("{}@example.invalid", Uuid::new_v4()))
    .fetch_one(db.pool())
    .await
    .expect("principal")
}

/// 0019 and the request path: a subscription seat serves its owner and no one
/// else, while an owner-less API key stays the route's shared pool. The lease
/// path has exactly one query (`candidates`), so this is the whole of "can B
/// be served by A's seat".
#[tokio::test]
async fn a_seat_serves_its_owner_and_no_one_else() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (owner, route, _) = seed(&db).await;
    let other = another_principal(&db).await;
    let seat = seat_on(&db, route, "xai", "oauth", Some(owner)).await;
    let pooled = seat_on(&db, route, "xai", "api_key", None).await;

    let ids =
        |rows: Vec<crate::rows::AccountRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    let mine = ids(candidates(&db, route, "xai", owner).await.expect("owner"));
    let theirs = ids(candidates(&db, route, "xai", other).await.expect("other"));
    assert!(mine.contains(&seat) && mine.contains(&pooled), "{mine:?}");
    assert!(
        theirs.contains(&pooled),
        "the shared key serves everyone: {theirs:?}"
    );
    assert!(
        !theirs.contains(&seat),
        "another principal was offered the owner's seat: {theirs:?}"
    );

    // And the schema refuses to make a seat that would belong to nobody.
    let refused = sqlx::query(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
         VALUES (gen_random_uuid(), $1, 'xai', 'oauth', '\\x00', '\\x00')",
    )
    .bind(format!("orphan-{}", Uuid::new_v4()))
    .execute(db.pool())
    .await
    .expect_err("an owner-less seat must not be insertable");
    assert!(
        refused.to_string().contains("account_seat_has_one_owner"),
        "{refused}"
    );
}

/// A seat an older version pooled (`--shared`) is still in some databases:
/// 0019 does not stop a boot over it. It must serve no one. Built inside a
/// transaction that disables the trigger, because the schema no longer lets
/// anything create one; the rollback turns it back on.
#[tokio::test]
async fn an_ownerless_seat_from_an_older_version_serves_no_one() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (owner, route, _) = seed(&db).await;
    let pooled = seat_on(&db, route, "xai", "api_key", None).await;

    let mut tx = db.pool().begin().await.expect("begin");
    sqlx::query("ALTER TABLE account DISABLE TRIGGER account_seat_has_one_owner")
        .execute(&mut *tx)
        .await
        .expect("disable, inside the transaction only");
    let orphan: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
         VALUES (gen_random_uuid(), $1, 'xai', 'oauth', '\\x00', '\\x00') RETURNING id",
    )
    .bind(format!("legacy-{}", Uuid::new_v4()))
    .fetch_one(&mut *tx)
    .await
    .expect("a legacy owner-less seat");
    sqlx::query("INSERT INTO account_route (account_id, route_id) VALUES ($1, $2)")
        .bind(orphan)
        .bind(route)
        .execute(&mut *tx)
        .await
        .expect("join");

    for principal in [owner, Uuid::new_v4()] {
        let rows: Vec<crate::rows::AccountRow> = sqlx::query_as(super::routes::CANDIDATES_SQL)
            .bind(route)
            .bind("xai")
            .bind(principal)
            .fetch_all(&mut *tx)
            .await
            .expect("candidates");
        let ids: Vec<Uuid> = rows.into_iter().map(|r| r.id).collect();
        assert!(ids.contains(&pooled), "{ids:?}");
        assert!(
            !ids.contains(&orphan),
            "an owner-less seat was offered to {principal}: {ids:?}"
        );
    }
    tx.rollback().await.expect("rollback");
}

/// The warning `doctor` and `key create` print: a seat owner with several
/// live inference keys. Admin keys and revoked keys do not count, and an
/// owner with one key is not reported.
#[tokio::test]
async fn a_seat_owner_with_several_live_keys_is_reported() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (owner, route, _) = seed(&db).await;
    let seat = seat_on(&db, route, "xai", "oauth", Some(owner)).await;
    let seat_name: String = sqlx::query_scalar("SELECT name FROM account WHERE id = $1")
        .bind(seat)
        .fetch_one(db.pool())
        .await
        .expect("name");
    let key = |admin: bool, active: bool| {
        let db = db.clone();
        async move {
            let name = format!("k-{}", Uuid::new_v4());
            sqlx::query(
                "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id, \
                 admin, active) VALUES (gen_random_uuid(), $1, 'oag_live_test', $2, $3, $4, $5, $6)",
            )
            .bind(hash_key(&name))
            .bind(&name)
            .bind(owner)
            .bind(route)
            .bind(admin)
            .bind(active)
            .execute(db.pool())
            .await
            .expect("key");
        }
    };

    key(false, true).await;
    key(true, true).await; // an admin key: not counted
    key(false, false).await; // revoked: not counted
    let one = seats_with_many_keys(&db, Some(owner)).await.expect("query");
    assert!(
        one.is_empty(),
        "one live inference key is not a warning: {one:?}"
    );

    key(false, true).await;
    let two = seats_with_many_keys(&db, Some(owner)).await.expect("query");
    assert_eq!(two.len(), 1, "{two:?}");
    assert_eq!((two[0].seat.as_str(), two[0].keys), (seat_name.as_str(), 2));
    assert!(
        seats_with_many_keys(&db, None)
            .await
            .expect("all")
            .iter()
            .any(|s| s.seat == seat_name),
        "and the unfiltered form, which doctor uses, includes it"
    );
}

/// Cursor's finding on #122: a CHECK — even NOT VALID — re-checks a legacy
/// owner-less seat on every UPDATE, so it could not be disabled, renamed or have
/// a rotated token stored. The trigger refuses only a write that would make an
/// owner-less seat: clearing an owner, or turning an owner-less key into a
/// seat. Everything else on a legacy seat still writes, and the poller, which
/// would refresh its token for no request, does not pick it up.
// Long because it walks one transaction through every write the finding
// named; split, each half would need its own legacy row and trigger dance.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn a_legacy_ownerless_seat_can_still_be_written_but_not_made() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (owner, _, _) = seed(&db).await;

    let mut tx = db.pool().begin().await.expect("begin");
    let exec = |sql: &'static str| sqlx::query(sql);
    exec("ALTER TABLE account DISABLE TRIGGER account_seat_has_one_owner")
        .execute(&mut *tx)
        .await
        .expect("disable");
    let legacy: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
         VALUES (gen_random_uuid(), $1, 'xai', 'oauth', '\\x00', '\\x00') RETURNING id",
    )
    .bind(format!("legacy-{}", Uuid::new_v4()))
    .fetch_one(&mut *tx)
    .await
    .expect("legacy");
    exec("ALTER TABLE account ENABLE TRIGGER account_seat_has_one_owner")
        .execute(&mut *tx)
        .await
        .expect("enable");

    // Every ordinary write to the legacy seat still lands.
    for (what, sql) in [
        (
            "disable",
            "UPDATE account SET schedulable = false WHERE id = $1",
        ),
        (
            "rename",
            "UPDATE account SET name = name || '-renamed' WHERE id = $1",
        ),
        (
            "store a rotated token",
            "UPDATE account SET credentials_sealed = '\\x01', \
          token_version = token_version + 1 WHERE id = $1",
        ),
        (
            "set its kind to what it is",
            "UPDATE account SET kind = 'oauth' WHERE id = $1",
        ),
    ] {
        sqlx::query("SAVEPOINT w")
            .execute(&mut *tx)
            .await
            .expect("savepoint");
        sqlx::query(sql)
            .bind(legacy)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("{what} on a legacy seat was refused: {e}"));
    }

    // Binding it works; and the poller's sweep never contained it.
    let polled: Vec<crate::rows::AccountRow> =
        sqlx::query_as(super::accounts::SCHEDULABLE_OAUTH_SQL)
            .fetch_all(&mut *tx)
            .await
            .expect("sweep");
    assert!(
        polled.iter().all(|r| r.id != legacy),
        "the poller swept an owner-less seat"
    );

    // What the trigger does refuse: making an owner-less seat out of a bound
    // one, or out of an owner-less key.
    let bound: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce, \
         owner_principal_id) VALUES (gen_random_uuid(), $1, 'xai', 'oauth', '\\x00', '\\x00', $2) \
         RETURNING id",
    )
    .bind(format!("bound-{}", Uuid::new_v4()))
    .bind(owner)
    .fetch_one(&mut *tx)
    .await
    .expect("bound seat");
    let key: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
         VALUES (gen_random_uuid(), $1, 'xai', 'api_key', '\\x00', '\\x00') RETURNING id",
    )
    .bind(format!("key-{}", Uuid::new_v4()))
    .fetch_one(&mut *tx)
    .await
    .expect("pooled key");
    for (what, sql, id) in [
        (
            "clearing a seat's owner",
            "UPDATE account SET owner_principal_id = NULL WHERE id = $1",
            bound,
        ),
        (
            "turning an owner-less key into a seat",
            "UPDATE account SET kind = 'oauth' WHERE id = $1",
            key,
        ),
    ] {
        sqlx::query("SAVEPOINT r")
            .execute(&mut *tx)
            .await
            .expect("savepoint");
        let refused = sqlx::query(sql)
            .bind(id)
            .execute(&mut *tx)
            .await
            .expect_err(what);
        assert!(
            refused.to_string().contains("account_seat_has_one_owner"),
            "{what}: {refused}"
        );
        sqlx::query("ROLLBACK TO SAVEPOINT r")
            .execute(&mut *tx)
            .await
            .expect("rollback to");
    }
    tx.rollback().await.expect("rollback");
}

/// The poller's sweep: an owned, enabled seat is in it; a disabled one is not.
#[tokio::test]
async fn the_usage_sweep_holds_owned_enabled_seats() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (owner, route, _) = seed(&db).await;
    let live = seat_on(&db, route, "xai", "oauth", Some(owner)).await;
    let off = seat_on(&db, route, "xai", "oauth", Some(owner)).await;
    sqlx::query("UPDATE account SET schedulable = false WHERE id = $1")
        .bind(off)
        .execute(db.pool())
        .await
        .expect("disable");
    let swept: Vec<Uuid> = schedulable_oauth_accounts(&db)
        .await
        .expect("sweep")
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert!(swept.contains(&live), "an owned, enabled seat is polled");
    assert!(!swept.contains(&off), "a disabled seat is not");
}

/// A fresh endpoint name that fits 0020's pattern.
fn endpoint_name() -> String {
    format!("t{}", &Uuid::new_v4().simple().to_string()[..20])
}

/// A `plain` OpenAI-shaped endpoint that every CHECK accepts. Each refusal case
/// starts from it and breaks one rule.
fn plain_endpoint<'a>(name: &'a str, headers: &'a serde_json::Value) -> NewEndpoint<'a> {
    NewEndpoint {
        name,
        dialect: "openai",
        platform: "plain",
        base_url: Some("https://llm.example.test/v1"),
        auth: "bearer",
        region: None,
        project: None,
        api_version: None,
        extra_headers: headers,
        display_name: None,
        discover_models: false,
    }
}

/// 0020: each CHECK on `endpoint` refuses what it exists to refuse, and the
/// refusal says which check it was. Each case breaks one rule and keeps the
/// rest, and the assertion is on the constraint's name, so a CHECK that stopped
/// refusing shows up as another name or as an insert that went through. Where
/// one bad value breaks two rules (an unknown dialect is outside the matrix
/// too), Postgres runs CHECKs in name order and reports the first failure,
/// which is the column's own check.
///
/// The second half inserts every pair the matrix serves. Without it, a CHECK
/// that refused everything would pass the first half.
// Long because it is a table of cases; split, each half would repeat the
// fixture the other needs.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn each_endpoint_check_refuses_what_the_matrix_does_not_serve() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let name = endpoint_name();
    let object = serde_json::json!({});
    let array = serde_json::json!(["x-org", "acme"]);
    let string = serde_json::json!("x-org: acme");
    let null = serde_json::Value::Null;
    let too_long = "a".repeat(33);
    let ok = plain_endpoint(&name, &object);
    let pair = "endpoint_platform_dialect_check";
    let base_url = "endpoint_base_url_check";
    let headers = "endpoint_extra_headers_check";

    let refused = [
        (
            "an upper-case name",
            NewEndpoint {
                name: "Groq",
                ..ok.clone()
            },
            "endpoint_name_check",
        ),
        (
            "a name led by a hyphen",
            NewEndpoint {
                name: "-groq",
                ..ok.clone()
            },
            "endpoint_name_check",
        ),
        (
            "a name with a slash in it",
            NewEndpoint {
                name: "groq/eu",
                ..ok.clone()
            },
            "endpoint_name_check",
        ),
        (
            "a 33-character name",
            NewEndpoint {
                name: &too_long,
                ..ok.clone()
            },
            "endpoint_name_check",
        ),
        (
            "an empty name",
            NewEndpoint {
                name: "",
                ..ok.clone()
            },
            "endpoint_name_check",
        ),
        (
            "an unknown dialect",
            NewEndpoint {
                dialect: "cohere",
                ..ok.clone()
            },
            "endpoint_dialect_check",
        ),
        (
            "an unknown platform",
            NewEndpoint {
                platform: "oracle",
                ..ok.clone()
            },
            "endpoint_platform_check",
        ),
        (
            "an unknown auth style",
            NewEndpoint {
                auth: "basic",
                ..ok.clone()
            },
            "endpoint_auth_check",
        ),
        (
            "azure serving gemini",
            NewEndpoint {
                platform: "azure",
                dialect: "gemini",
                ..ok.clone()
            },
            pair,
        ),
        (
            "azure serving anthropic",
            NewEndpoint {
                platform: "azure",
                dialect: "anthropic",
                ..ok.clone()
            },
            pair,
        ),
        (
            "plain serving bedrock_converse",
            NewEndpoint {
                dialect: "bedrock_converse",
                ..ok.clone()
            },
            pair,
        ),
        (
            "aws serving openai",
            NewEndpoint {
                platform: "aws",
                region: Some("us-east-1"),
                ..ok.clone()
            },
            pair,
        ),
        (
            "gcp serving system_one",
            NewEndpoint {
                platform: "gcp",
                dialect: "system_one",
                region: Some("us-central1"),
                project: Some("acme"),
                ..ok.clone()
            },
            pair,
        ),
        (
            "plain without a base_url",
            NewEndpoint {
                base_url: None,
                ..ok.clone()
            },
            base_url,
        ),
        (
            "azure without a base_url",
            NewEndpoint {
                platform: "azure",
                base_url: None,
                ..ok.clone()
            },
            base_url,
        ),
        (
            "a base_url with a query",
            NewEndpoint {
                base_url: Some("https://llm.example.test/v1?api-version=1"),
                ..ok.clone()
            },
            base_url,
        ),
        (
            "a base_url with a fragment",
            NewEndpoint {
                base_url: Some("https://llm.example.test/v1#top"),
                ..ok.clone()
            },
            base_url,
        ),
        (
            "a base_url that is not http",
            NewEndpoint {
                base_url: Some("ftp://llm.example.test/v1"),
                ..ok.clone()
            },
            base_url,
        ),
        (
            "a base_url with no scheme",
            NewEndpoint {
                base_url: Some("llm.example.test/v1"),
                ..ok.clone()
            },
            base_url,
        ),
        (
            "aws without a region",
            NewEndpoint {
                platform: "aws",
                dialect: "anthropic",
                base_url: None,
                ..ok.clone()
            },
            "endpoint_region_check",
        ),
        (
            "gcp without a region",
            NewEndpoint {
                platform: "gcp",
                dialect: "gemini",
                base_url: None,
                project: Some("acme"),
                ..ok.clone()
            },
            "endpoint_region_check",
        ),
        (
            "gcp without a project",
            NewEndpoint {
                platform: "gcp",
                dialect: "gemini",
                base_url: None,
                region: Some("us-central1"),
                ..ok.clone()
            },
            "endpoint_project_check",
        ),
        (
            "headers as an array",
            NewEndpoint {
                extra_headers: &array,
                ..ok.clone()
            },
            headers,
        ),
        (
            "headers as a string",
            NewEndpoint {
                extra_headers: &string,
                ..ok.clone()
            },
            headers,
        ),
        (
            "headers as JSON null",
            NewEndpoint {
                extra_headers: &null,
                ..ok.clone()
            },
            headers,
        ),
    ];
    for (what, row, constraint) in refused {
        match insert_endpoint(&db, &row).await {
            Err(Error::Config(message)) => assert!(
                message.contains(constraint),
                "{what} tripped the wrong thing: {message}"
            ),
            Err(other) => panic!("{what} must be a Config error: {other}"),
            Ok(_) => panic!("{what} was stored"),
        }
    }

    // A full UUID is 32 characters, the most a name may have.
    let longest = Uuid::new_v4().simple().to_string();
    let marked = format!("{}_a-b", &Uuid::new_v4().simple().to_string()[..20]);
    let names: Vec<String> = (0..7).map(|_| endpoint_name()).collect();
    let served = [
        NewEndpoint {
            name: &longest,
            ..ok.clone()
        },
        NewEndpoint {
            name: &marked,
            dialect: "anthropic",
            auth: "x_api_key",
            ..ok.clone()
        },
        NewEndpoint {
            name: &names[0],
            dialect: "gemini",
            auth: "x_goog_api_key",
            ..ok.clone()
        },
        NewEndpoint {
            name: &names[1],
            dialect: "system_one",
            base_url: Some("http://127.0.0.1:9"),
            auth: "none",
            ..ok.clone()
        },
        NewEndpoint {
            name: &names[2],
            platform: "azure",
            base_url: Some("https://acme.openai.azure.com"),
            auth: "api_key_header",
            api_version: Some("2024-10-21"),
            ..ok.clone()
        },
        NewEndpoint {
            name: &names[3],
            platform: "aws",
            dialect: "anthropic",
            base_url: None,
            region: Some("us-east-1"),
            ..ok.clone()
        },
        NewEndpoint {
            name: &names[4],
            platform: "aws",
            dialect: "bedrock_converse",
            region: Some("eu-west-1"),
            ..ok.clone()
        },
        NewEndpoint {
            name: &names[5],
            platform: "gcp",
            dialect: "gemini",
            base_url: None,
            region: Some("global"),
            project: Some("acme-prod"),
            ..ok.clone()
        },
        NewEndpoint {
            name: &names[6],
            platform: "gcp",
            dialect: "anthropic",
            base_url: None,
            region: Some("us-east5"),
            project: Some("acme-prod"),
            ..ok.clone()
        },
    ];
    for row in served {
        insert_endpoint(&db, &row).await.unwrap_or_else(|e| {
            panic!(
                "{} ({} on {}) must be stored: {e}",
                row.name, row.dialect, row.platform
            )
        });
    }
}

/// Insert, read back, list and update. Every column survives the trip. An
/// update replaces the settings and stamps `updated_at`, and the dialect and
/// platform stay what they were, because `EndpointUpdate` has no field for
/// either. A hand-written row gets the schema's defaults.
// Long because it walks one endpoint through its whole life; split, every
// part would need the insert that the first part makes.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn an_endpoint_round_trips_and_an_update_keeps_what_it_is() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let name = endpoint_name();
    let headers = serde_json::json!({ "x-org": "acme" });
    let row = insert_endpoint(
        &db,
        &NewEndpoint {
            name: &name,
            dialect: "anthropic",
            platform: "plain",
            base_url: Some("https://llm.example.test/v1"),
            auth: "x_api_key",
            region: Some("eu"),
            project: Some("acme"),
            api_version: Some("2023-06-01"),
            extra_headers: &headers,
            display_name: Some("Example"),
            discover_models: true,
        },
    )
    .await
    .expect("insert");
    assert_eq!(
        (
            row.name.as_str(),
            row.dialect.as_str(),
            row.platform.as_str(),
            row.base_url.as_deref(),
            row.auth.as_str(),
            row.region.as_deref(),
            row.project.as_deref(),
            row.api_version.as_deref(),
            &row.extra_headers,
            row.display_name.as_deref(),
            row.discover_models,
        ),
        (
            name.as_str(),
            "anthropic",
            "plain",
            Some("https://llm.example.test/v1"),
            "x_api_key",
            Some("eu"),
            Some("acme"),
            Some("2023-06-01"),
            &headers,
            Some("Example"),
            true,
        )
    );
    assert_eq!(
        get_endpoint(&db, &name).await.expect("get"),
        Some(row.clone())
    );
    assert!(
        list_endpoints(&db).await.expect("list").contains(&row),
        "a registered endpoint is listed"
    );

    let taken = insert_endpoint(&db, &plain_endpoint(&name, &headers))
        .await
        .expect_err("a second endpoint under a taken name");
    assert!(
        matches!(&taken, Error::Config(m) if m.contains("already exists")),
        "{taken}"
    );

    // Backdated, so the stamp the update writes shows whatever the clock's
    // resolution.
    sqlx::query("UPDATE endpoint SET updated_at = '2000-01-01T00:00:00Z' WHERE name = $1")
        .bind(&name)
        .execute(db.pool())
        .await
        .expect("backdate");
    let beta = serde_json::json!({ "anthropic-beta": "tools-2024-04-04" });
    let settings = EndpointUpdate {
        base_url: Some("https://eu.llm.example.test"),
        auth: "bearer",
        region: None,
        project: None,
        api_version: None,
        extra_headers: &beta,
        display_name: None,
        discover_models: false,
    };
    let updated = update_endpoint(&db, &name, &settings)
        .await
        .expect("update")
        .expect("exists");
    assert_eq!(
        (updated.dialect.as_str(), updated.platform.as_str()),
        ("anthropic", "plain"),
        "an update never changes what the endpoint is"
    );
    assert_eq!(
        (
            updated.base_url.as_deref(),
            updated.auth.as_str(),
            updated.region.as_deref(),
            updated.project.as_deref(),
            updated.api_version.as_deref(),
            &updated.extra_headers,
            updated.display_name.as_deref(),
            updated.discover_models,
        ),
        (
            Some("https://eu.llm.example.test"),
            "bearer",
            None,
            None,
            None,
            &beta,
            None,
            false,
        ),
        "every setting is replaced, the cleared ones included"
    );
    assert_eq!(updated.created_at, row.created_at);
    assert!(
        updated.updated_at >= row.created_at,
        "the update stamps updated_at: {}",
        updated.updated_at
    );
    assert_eq!(
        get_endpoint(&db, &name).await.expect("get"),
        Some(updated.clone())
    );

    let refused = update_endpoint(
        &db,
        &name,
        &EndpointUpdate {
            base_url: None,
            ..settings.clone()
        },
    )
    .await
    .expect_err("a plain endpoint without a base_url");
    assert!(
        matches!(&refused, Error::Config(m) if m.contains("endpoint_base_url_check")),
        "{refused}"
    );
    assert_eq!(
        get_endpoint(&db, &name).await.expect("get"),
        Some(updated),
        "a refused update changes nothing"
    );

    let nobody = endpoint_name();
    assert_eq!(
        update_endpoint(&db, &nobody, &settings)
            .await
            .expect("update"),
        None
    );
    assert_eq!(get_endpoint(&db, &nobody).await.expect("get"), None);

    let bare = endpoint_name();
    sqlx::query(
        "INSERT INTO endpoint (name, dialect, base_url) \
         VALUES ($1, 'openai', 'https://llm.example.test')",
    )
    .bind(&bare)
    .execute(db.pool())
    .await
    .expect("a row with only the columns that have no default");
    let defaults = get_endpoint(&db, &bare)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(
        (
            defaults.platform.as_str(),
            defaults.auth.as_str(),
            &defaults.extra_headers,
            defaults.discover_models,
        ),
        ("plain", "bearer", &serde_json::json!({}), false)
    );
}

/// A credential or a catalog model that names an endpoint keeps it. The delete
/// is refused, reports what holds the endpoint, and removes nothing. Once
/// both are gone the endpoint goes, and a second delete finds nothing.
#[tokio::test]
async fn an_endpoint_that_a_credential_or_a_model_names_is_not_deleted() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let name = endpoint_name();
    let headers = serde_json::json!({});
    insert_endpoint(&db, &plain_endpoint(&name, &headers))
        .await
        .expect("insert");

    let account: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
         VALUES (gen_random_uuid(), $1, $2, 'api_key', '\\x00', '\\x00') RETURNING id",
    )
    .bind(format!("key-{name}"))
    .bind(&name)
    .fetch_one(db.pool())
    .await
    .expect("a key for it");
    assert_eq!(
        delete_endpoint(&db, &name).await.expect("delete"),
        EndpointDeletion::InUse {
            accounts: 1,
            models: 0
        }
    );
    assert!(
        get_endpoint(&db, &name).await.expect("get").is_some(),
        "a refused delete removes nothing"
    );

    let model = format!("{name}/llama");
    upsert_model(
        &db,
        &ModelRow {
            provider: name.clone(),
            ..seed_model(&model, dec!(0.10))
        },
        true,
    )
    .await
    .expect("a model on it");
    assert_eq!(
        delete_endpoint(&db, &name).await.expect("delete"),
        EndpointDeletion::InUse {
            accounts: 1,
            models: 1
        }
    );

    sqlx::query("DELETE FROM account WHERE id = $1")
        .bind(account)
        .execute(db.pool())
        .await
        .expect("remove the key");
    assert_eq!(
        delete_endpoint(&db, &name).await.expect("delete"),
        EndpointDeletion::InUse {
            accounts: 0,
            models: 1
        },
        "a model alone holds it"
    );

    sqlx::query("DELETE FROM model_catalog WHERE id = $1")
        .bind(&model)
        .execute(db.pool())
        .await
        .expect("remove the model");
    assert_eq!(
        delete_endpoint(&db, &name).await.expect("delete"),
        EndpointDeletion::Deleted
    );
    assert_eq!(get_endpoint(&db, &name).await.expect("get"), None);
    assert_eq!(
        delete_endpoint(&db, &name).await.expect("delete"),
        EndpointDeletion::NotFound
    );
}

/// The two tables whose `provider` can name an endpoint, and a write to each.
const WRITES_NAMING_AN_ENDPOINT: [(&str, &str); 2] = [
    (
        "account",
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
         VALUES (gen_random_uuid(), 'race-' || gen_random_uuid(), $1, 'api_key', '\\x00', '\\x00')",
    ),
    (
        "model_catalog",
        "INSERT INTO model_catalog (id, provider, upstream_name, input_per_mtok, \
         output_per_mtok, context_window, max_output_tokens) \
         VALUES ($1 || '/race-' || gen_random_uuid(), $1, 'race', 0, 0, 1, 1)",
    ),
];

/// Wait until backend `waiter` is blocked by backend `holder`. Returns false if
/// it never is, and the caller turns that into a failure: a race test whose
/// interleaving never happened has proved nothing.
///
/// Blocked by that backend in particular, not merely waiting on some lock. A
/// sibling test that holds `account` locked (the legacy-seat test disables a
/// trigger inside its transaction) would otherwise count as the wait this is
/// looking for.
async fn blocked_by(db: &Db, waiter: i32, holder: i32) -> bool {
    for _ in 0..400 {
        let blocked: bool = sqlx::query_scalar("SELECT $2 = ANY (pg_blocking_pids($1))")
            .bind(waiter)
            .bind(holder)
            .fetch_one(db.pool())
            .await
            .expect("pg_blocking_pids");
        if blocked {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

/// 0020's `endpoint_reference_holds`, with the delete first. A credential or a
/// model written while `delete_endpoint` holds the endpoint's row waits for
/// the delete to finish, and is then refused, because the row it names is
/// gone. Without the trigger, the write would not wait, and would land naming
/// nothing.
///
/// The delete is done by hand and stopped between its lock and its commit,
/// because what is being tested is a write that arrives in that gap.
#[tokio::test]
async fn a_write_naming_an_endpoint_being_deleted_waits_and_is_refused() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 4).expect("connect");
    db.migrate().await.expect("migrate");
    for (table, write) in WRITES_NAMING_AN_ENDPOINT {
        let name = endpoint_name();
        let headers = serde_json::json!({});
        insert_endpoint(&db, &plain_endpoint(&name, &headers))
            .await
            .expect("insert");

        // What `delete_endpoint` takes first, held open.
        let mut deleter = db.pool().begin().await.expect("begin");
        let deleter_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *deleter)
            .await
            .expect("pid");
        sqlx::query("SELECT name FROM endpoint WHERE name = $1 FOR UPDATE")
            .bind(&name)
            .execute(&mut *deleter)
            .await
            .expect("lock");

        let mut writer = db.pool().acquire().await.expect("a connection");
        let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *writer)
            .await
            .expect("pid");
        let written = tokio::spawn({
            let name = name.clone();
            async move {
                sqlx::query(write)
                    .bind(&name)
                    .execute(&mut *writer)
                    .await
                    .map(|_| ())
            }
        });
        assert!(
            blocked_by(&db, writer_pid, deleter_pid).await,
            "the {table} write never waited for the delete, so this run proved nothing"
        );

        sqlx::query("DELETE FROM endpoint WHERE name = $1")
            .bind(&name)
            .execute(&mut *deleter)
            .await
            .expect("delete");
        deleter.commit().await.expect("commit");

        let refused = written
            .await
            .expect("task")
            .expect_err(&format!("a {table} row naming a deleted endpoint"));
        assert!(
            refused.to_string().contains("endpoint_reference_holds"),
            "{table}: {refused}"
        );
        let stranded: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM account WHERE provider = $1) \
                  + (SELECT count(*) FROM model_catalog WHERE provider = $1)",
        )
        .bind(&name)
        .fetch_one(db.pool())
        .await
        .expect("count");
        assert_eq!(stranded, 0, "{table}: nothing names the deleted endpoint");
    }
}

/// The same race with the write first. `delete_endpoint` waits for a write that
/// named the endpoint before the delete arrived, and then counts it and
/// refuses. If either the trigger's lock or the delete's `FOR UPDATE` were
/// missing, the delete would not wait. It would read the uncommitted row as
/// absent and remove an endpoint that a row committed a moment later still
/// names.
#[tokio::test]
async fn a_delete_waits_for_a_write_naming_the_endpoint_and_then_refuses() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 4).expect("connect");
    db.migrate().await.expect("migrate");
    for (table, write) in WRITES_NAMING_AN_ENDPOINT {
        let name = endpoint_name();
        let headers = serde_json::json!({});
        insert_endpoint(&db, &plain_endpoint(&name, &headers))
            .await
            .expect("insert");

        let mut writer = db.pool().begin().await.expect("begin");
        let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *writer)
            .await
            .expect("pid");
        sqlx::query(write)
            .bind(&name)
            .execute(&mut *writer)
            .await
            .expect("the write, not yet committed");

        // One connection, so the backend asked here is the one the delete
        // runs on.
        let deleter = Db::connect(&url, 1).expect("connect");
        let deleter_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(deleter.pool())
            .await
            .expect("pid");
        let deletion = tokio::spawn({
            let name = name.clone();
            async move { delete_endpoint(&deleter, &name).await }
        });
        assert!(
            blocked_by(&db, deleter_pid, writer_pid).await,
            "the delete never waited for the {table} write, so this run proved nothing"
        );

        writer.commit().await.expect("commit");
        let held = if table == "account" {
            EndpointDeletion::InUse {
                accounts: 1,
                models: 0,
            }
        } else {
            EndpointDeletion::InUse {
                accounts: 0,
                models: 1,
            }
        };
        assert_eq!(
            deletion.await.expect("task").expect("delete"),
            held,
            "{table}"
        );
        assert!(
            get_endpoint(&db, &name).await.expect("get").is_some(),
            "{table}: an endpoint a committed row names is still there"
        );
    }
}

/// 0019 and 0020 together: a `service_account` credential can be written again,
/// with no owner. It is an organisation's credential, pooled the way an API key
/// is, so 0019's seat trigger, which asks only `oauth` rows for an owner, does
/// not stop it.
#[tokio::test]
async fn a_service_account_credential_needs_no_owner() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
         VALUES (gen_random_uuid(), $1, 'vertex-eu', 'service_account', '\\x00', '\\x00') \
         RETURNING id",
    )
    .bind(format!("sa-{}", Uuid::new_v4()))
    .fetch_one(db.pool())
    .await
    .expect("a pooled service account");
    let row = account_by_id(&db, AccountId::from_uuid(id))
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(
        (row.kind.as_str(), row.owner_principal_id),
        ("service_account", None)
    );
}

/// One catalog row, read the way the gateway reads them all.
async fn catalog_row(db: &Db, id: &str) -> ModelRow {
    catalog(db)
        .await
        .expect("catalog")
        .into_iter()
        .find(|m| m.id == id)
        .expect("the row")
}

/// Whether the catalog row `id` is marked as an operator's.
async fn is_override(db: &Db, id: &str) -> bool {
    sqlx::query_scalar("SELECT is_override FROM model_catalog WHERE id = $1")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("the row")
}

/// `override_model` writes the operator's row whole and marks it theirs, over
/// a seeded row and over their own earlier write alike, and a seed after it
/// leaves it alone. `upsert_model(.., true)` did neither: it skipped a row
/// already overridden and never set the flag on a seeded one.
#[tokio::test]
async fn an_operators_model_is_written_whole_and_a_seed_leaves_it() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");

    // A seeded row, which the operator has named.
    let seeded = format!("xai/grok-{}", Uuid::new_v4());
    upsert_model(&db, &seed_model(&seeded, dec!(3)), false)
        .await
        .expect("seed");
    set_model_label(&db, &seeded, Some("Grok, as named"))
        .await
        .expect("label");

    // The operator reprices it, saying nothing about the label.
    let mut theirs = seed_model(&seeded, dec!(9));
    theirs.context_window = 64_000;
    theirs.supports_tools = true;
    override_model(&db, &theirs).await.expect("override");
    let written = catalog_row(&db, &seeded).await;
    assert_eq!(
        (
            written.input_per_mtok,
            written.context_window,
            written.supports_tools
        ),
        (dec!(9), 64_000, true),
        "every column is the operator's"
    );
    assert_eq!(written.display_label.as_deref(), Some("Grok, as named"));
    assert!(
        is_override(&db, &seeded).await,
        "and the row is marked theirs"
    );

    // A seed after that leaves it where the operator put it.
    upsert_model(&db, &seed_model(&seeded, dec!(5)), false)
        .await
        .expect("reseed");
    assert_eq!(catalog_row(&db, &seeded).await.input_per_mtok, dec!(9));

    // A second write of theirs is not skipped as somebody's override, and a
    // label they give replaces the one before.
    let mut again = seed_model(&seeded, dec!(11));
    again.display_label = Some("Grok, renamed".to_owned());
    override_model(&db, &again).await.expect("override again");
    let written = catalog_row(&db, &seeded).await;
    assert_eq!(written.input_per_mtok, dec!(11));
    assert_eq!(written.display_label.as_deref(), Some("Grok, renamed"));

    // A model nobody seeded is inserted as the operator's.
    let fresh = format!("xai/grok-{}", Uuid::new_v4());
    override_model(&db, &seed_model(&fresh, dec!(2)))
        .await
        .expect("insert");
    assert!(is_override(&db, &fresh).await);
    assert_eq!(catalog_row(&db, &fresh).await.output_per_mtok, dec!(8));

    sqlx::query("DELETE FROM model_catalog WHERE id = ANY($1)")
        .bind(vec![seeded, fresh])
        .execute(db.pool())
        .await
        .expect("clean up");
}

/// Each endpoint is counted by what names it: its credentials and the ones in
/// rotation, its catalog models and the ones an active route's ladder names. A
/// ladder shaped wrong by hand counts as naming nothing, and an endpoint
/// nothing names counts zero everywhere.
// Long for its fixture: two endpoints, three routes, two credentials and three
// models, all removed again before asserting.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn each_endpoint_is_counted_by_what_names_it() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let object = serde_json::json!({});
    let (named, idle) = (endpoint_name(), endpoint_name());
    for name in [&named, &idle] {
        insert_endpoint(&db, &plain_endpoint(name, &object))
            .await
            .expect("an endpoint");
    }

    let models: Vec<String> = ["laddered", "benched", "unlisted"]
        .iter()
        .map(|m| format!("{named}/{m}"))
        .collect();
    for id in &models {
        let mut model = seed_model(id, dec!(1));
        model.provider.clone_from(&named);
        upsert_model(&db, &model, false).await.expect("a model");
    }
    let routes: Vec<String> = (0..3)
        .map(|i| format!("t5-refs-{i}-{}", Uuid::new_v4()))
        .collect();
    for (route, tiers, active) in [
        (
            &routes[0],
            serde_json::json!([
                {"name": "cheap", "models": ["xai/grok-4.6", models[0]]},
                {"name": "odd", "models": "not a list"},
                "not a rung",
            ]),
            true,
        ),
        // An inactive route's ladder serves nothing.
        (
            &routes[1],
            serde_json::json!([{"name": "cheap", "models": [models[1]]}]),
            false,
        ),
        // Not a list of rungs at all.
        (&routes[2], serde_json::json!({"cheap": [models[1]]}), true),
    ] {
        sqlx::query(
            "INSERT INTO route (id, name, tiers, active) VALUES (gen_random_uuid(), $1, $2, $3)",
        )
        .bind(route)
        .bind(tiers)
        .bind(active)
        .execute(db.pool())
        .await
        .expect("a route");
    }
    for (key, schedulable) in [("on", true), ("off", false)] {
        sqlx::query(
            "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
             credentials_nonce, schedulable) \
             VALUES (gen_random_uuid(), $1, $2, 'api_key', '\\x00', '\\x00', $3)",
        )
        .bind(format!("{named}-{key}"))
        .bind(&named)
        .bind(schedulable)
        .execute(db.pool())
        .await
        .expect("a credential");
    }

    let counted = endpoint_references(&db).await;

    sqlx::query("DELETE FROM account WHERE provider = $1")
        .bind(&named)
        .execute(db.pool())
        .await
        .expect("clean up credentials");
    sqlx::query("DELETE FROM model_catalog WHERE provider = $1")
        .bind(&named)
        .execute(db.pool())
        .await
        .expect("clean up models");
    sqlx::query("DELETE FROM route WHERE name = ANY($1)")
        .bind(&routes)
        .execute(db.pool())
        .await
        .expect("clean up routes");
    for name in [&named, &idle] {
        assert_eq!(
            delete_endpoint(&db, name).await.expect("delete"),
            EndpointDeletion::Deleted
        );
    }

    let counted = counted.expect("the read");
    assert_eq!(
        counted.get(&named).copied(),
        Some(EndpointReferences {
            accounts: 2,
            schedulable: 1,
            models: 3,
            on_ladder: 1,
        })
    );
    assert_eq!(
        counted.get(&idle).copied(),
        Some(EndpointReferences::default()),
        "an endpoint nothing names is listed, at zero"
    );
}
