//! An endpoint's catalog sync and model discovery, as the store sees them: the
//! sync's one-transaction write, the ladder check that keeps a stale row, and
//! the keys the usage poller asks.
//!
//! Gated on Postgres like every test here that needs a row. Every name is
//! fresh, and every test removes what it wrote before it asserts.

use super::*;
use crate::{Db, ModelRow, NewEndpoint};
use rust_decimal::Decimal;
use uuid::Uuid;

fn test_db() -> Option<Db> {
    let url = std::env::var("OAG_TEST_DATABASE_URL").ok()?;
    Some(Db::connect(&url, 2).expect("connect"))
}

/// A name nothing else in the database has.
fn fresh(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..12])
}

async fn endpoint(db: &Db, name: &str, discover: bool) {
    insert_endpoint(
        db,
        &NewEndpoint {
            name,
            dialect: "openai",
            platform: "plain",
            base_url: Some("http://127.0.0.1:9/v1"),
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
}

fn model(provider: &str, upstream: &str, input: i64, label: Option<&str>) -> ModelRow {
    ModelRow {
        id: format!("{provider}/{upstream}"),
        provider: provider.to_owned(),
        upstream_name: upstream.to_owned(),
        input_per_mtok: Decimal::from(input),
        output_per_mtok: Decimal::from(input * 4),
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
        context_window: 128_000,
        max_output_tokens: 8_192,
        supports_vision: false,
        supports_tools: true,
        supports_reasoning: false,
        supports_prompt_cache: false,
        display_label: label.map(str::to_owned),
    }
}

/// An `api_key` credential under `provider`, with nothing a test could open.
async fn account(db: &Db, provider: &str, priority: i16, schedulable: bool) -> (Uuid, String) {
    let name = fresh("t6-key");
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
         credentials_nonce, priority, schedulable) \
         VALUES (gen_random_uuid(), $1, $2, 'api_key', '\\x00', '\\x00', $3, $4) RETURNING id",
    )
    .bind(&name)
    .bind(provider)
    .bind(priority)
    .bind(schedulable)
    .fetch_one(db.pool())
    .await
    .expect("an account");
    (id, name)
}

/// Everything filed under these endpoints, then the endpoints.
async fn remove(db: &Db, endpoints: &[&str]) {
    for name in endpoints {
        for table in ["model_catalog", "account"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE provider = $1"
            )))
            .bind(name)
            .execute(db.pool())
            .await
            .expect("clean up");
        }
        delete_endpoint(db, name).await.expect("clean up");
    }
}

fn by_id(rows: &[crate::StoredModelRow], id: &str) -> crate::StoredModelRow {
    rows.iter()
        .find(|r| r.model.id == id)
        .unwrap_or_else(|| panic!("{id} is in the catalog"))
        .clone()
}

/// A sync writes overrides, rewrites them the next time the list says
/// otherwise, and gives a label only to a row nobody has named.
#[tokio::test]
async fn a_sync_writes_overrides_rewrites_its_own_and_fills_only_a_missing_label() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let e = fresh("t6-sync");
    endpoint(&db, &e, false).await;
    let (a, b) = (format!("{e}/zai/a"), format!("{e}/b"));

    let first = sync_endpoint_models(
        &db,
        &e,
        &[
            model(&e, "zai/a", 1, Some("A (e)")),
            model(&e, "b", 2, None),
        ],
        &[],
    )
    .await;
    let after_first = provider_models(&db, &e).await;
    // The operator names one model, and neither the next sync's label nor its
    // rewrite of the prices takes the name away.
    set_model_label(&db, &a, Some("Mine"))
        .await
        .expect("rename");
    let second = sync_endpoint_models(
        &db,
        &e,
        &[
            model(&e, "zai/a", 9, Some("A (e)")),
            model(&e, "b", 2, Some("B (e)")),
        ],
        &[],
    )
    .await;
    let after_second = provider_models(&db, &e).await;
    remove(&db, &[&e]).await;

    assert_eq!(first.expect("sync").written, [a.clone(), b.clone()]);
    let after_first = after_first.expect("rows");
    assert!(after_first.iter().all(|r| r.is_override), "{after_first:?}");
    assert_eq!(
        by_id(&after_first, &a).model.upstream_name,
        "zai/a",
        "the upstream name keeps every slash after the endpoint's"
    );
    assert_eq!(
        by_id(&after_first, &a).model.display_label.as_deref(),
        Some("A (e)")
    );
    assert_eq!(by_id(&after_first, &b).model.display_label, None);

    let second = second.expect("sync");
    assert_eq!(second.written, [a.clone(), b.clone()]);
    assert!(second.held.is_empty() && second.removed.is_empty());
    let after_second = after_second.expect("rows");
    let rewritten = by_id(&after_second, &a);
    assert_eq!(
        rewritten.model.input_per_mtok,
        Decimal::from(9),
        "an override the sync wrote is one it rewrites"
    );
    assert_eq!(rewritten.model.display_label.as_deref(), Some("Mine"));
    assert_eq!(
        by_id(&after_second, &b).model.display_label.as_deref(),
        Some("B (e)"),
        "a row nobody named is given the list's name"
    );
}

/// An id a row of another provider holds is not taken over.
#[tokio::test]
async fn an_id_another_providers_row_holds_is_left_as_it_was() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let e = fresh("t6-held");
    endpoint(&db, &e, false).await;
    let id = format!("{e}/x");
    let mut theirs = model(&e, "x", 5, None);
    theirs.provider = "anthropic".to_owned();
    upsert_model(&db, &theirs, false).await.expect("a row");

    let synced = sync_endpoint_models(&db, &e, &[model(&e, "x", 1, None)], &[]).await;
    let seen = provider_models(&db, &e).await;
    let held: (String, Decimal) =
        sqlx::query_as("SELECT provider, input_per_mtok FROM model_catalog WHERE id = $1")
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .expect("still there");
    sqlx::query("DELETE FROM model_catalog WHERE id = $1")
        .bind(&id)
        .execute(db.pool())
        .await
        .expect("clean up");
    remove(&db, &[&e]).await;

    let synced = synced.expect("sync");
    assert_eq!(synced.held, std::slice::from_ref(&id));
    assert!(synced.written.is_empty());
    assert_eq!(held, ("anthropic".to_owned(), Decimal::from(5)));
    let seen: Vec<(String, String)> = seen
        .expect("rows")
        .into_iter()
        .map(|r| (r.model.id, r.model.provider))
        .collect();
    assert_eq!(
        seen,
        [(id, "anthropic".to_owned())],
        "a row holding the endpoint's prefix is shown to the sync, so it can say so"
    );
}

/// A row the list no longer names is removed unless a ladder names it, and a
/// ladder the router could not read keeps nothing.
#[tokio::test]
async fn a_stale_row_a_ladder_names_is_kept_and_the_rest_are_removed() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let e = fresh("t6-stale");
    endpoint(&db, &e, false).await;
    let (kept, gone, stays) = (format!("{e}/k"), format!("{e}/zai/r"), format!("{e}/s"));
    sync_endpoint_models(
        &db,
        &e,
        &[
            model(&e, "k", 1, None),
            model(&e, "zai/r", 1, None),
            model(&e, "s", 1, None),
        ],
        &[],
    )
    .await
    .expect("seed");
    let ladder = serde_json::json!([
        {"name": "cheap", "models": [kept.clone()]},
        {"name": "odd", "models": gone.clone()},
        {"name": "holes", "models": [null, 7]},
    ]);
    let routes = [fresh("t6-route"), fresh("t6-route")];
    for (route, tiers) in routes
        .iter()
        .zip([ladder, serde_json::json!({"not": [gone.clone()]})])
    {
        sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, $2)")
            .bind(route)
            .bind(tiers)
            .execute(db.pool())
            .await
            .expect("a route");
    }

    let laddered = laddered_models(&db).await;
    let synced = sync_endpoint_models(
        &db,
        &e,
        &[model(&e, "s", 1, None)],
        &[kept.clone(), gone.clone()],
    )
    .await;
    let left: Vec<String> = provider_models(&db, &e)
        .await
        .expect("rows")
        .into_iter()
        .map(|r| r.model.id)
        .collect();
    for route in &routes {
        sqlx::query("DELETE FROM route WHERE name = $1")
            .bind(route)
            .execute(db.pool())
            .await
            .expect("clean up");
    }
    remove(&db, &[&e]).await;

    let laddered = laddered.expect("ladders");
    assert!(laddered.contains(&kept), "{laddered:?}");
    assert!(
        !laddered.contains(&gone),
        "a rung whose models are not a list, and a ladder that is not one, name nothing"
    );
    let synced = synced.expect("sync");
    assert_eq!(synced.removed, [gone]);
    assert_eq!(synced.written, std::slice::from_ref(&stays));
    assert_eq!(left, [kept, stays]);
}

/// Every row handed to a sync must be the endpoint's own.
#[tokio::test]
async fn a_sync_refuses_a_row_of_another_provider() {
    let db = Db::connect("postgres://oag:oag@127.0.0.1:1/oag", 1).expect("lazy pool");
    let err = sync_endpoint_models(&db, "t6-mine", &[model("t6-theirs", "m", 1, None)], &[])
        .await
        .expect_err("refused before any statement");
    assert!(err.to_string().contains("t6-theirs/m"), "{err}");
}

/// The model list is read with the key named, or else the endpoint's first
/// schedulable key, lowest priority first.
#[tokio::test]
async fn a_listing_is_read_with_the_named_key_or_the_first_schedulable_one() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (e, other) = (fresh("t6-keys"), fresh("t6-keyless"));
    endpoint(&db, &e, false).await;
    endpoint(&db, &other, false).await;
    let (_, disabled) = account(&db, &e, 0, false).await;
    let (_, first) = account(&db, &e, 1, true).await;
    let (_, second) = account(&db, &e, 2, true).await;

    let picked = endpoint_account(&db, &e, None).await;
    let named = endpoint_account(&db, &e, Some(&disabled)).await;
    let named_second = endpoint_account(&db, &e, Some(&second)).await;
    let missing = endpoint_account(&db, &e, Some("t6-no-such-key")).await;
    let elsewhere = endpoint_account(&db, &other, Some(&first)).await;
    let none = endpoint_account(&db, &other, None).await;
    remove(&db, &[&e, &other]).await;

    let name =
        |row: oag_core::Result<Option<crate::AccountRow>>| row.expect("query").map(|r| r.name);
    assert_eq!(name(picked).as_deref(), Some(first.as_str()));
    assert_eq!(
        name(named).as_deref(),
        Some(disabled.as_str()),
        "named, it is used disabled"
    );
    assert_eq!(name(named_second).as_deref(), Some(second.as_str()));
    assert_eq!(name(missing), None);
    assert_eq!(
        name(elsewhere),
        None,
        "a key of another endpoint is not this one's"
    );
    assert_eq!(name(none), None);
}

/// The poller asks the schedulable keys of an endpoint that discovers, and
/// what it recorded for an endpoint that stopped is forgotten.
#[tokio::test]
async fn the_sweep_reads_only_discovering_endpoints_keys_and_forgets_the_rest() {
    let Some(db) = test_db() else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    db.migrate().await.expect("migrate");
    let (on, off) = (fresh("t6-disc"), fresh("t6-quiet"));
    endpoint(&db, &on, true).await;
    endpoint(&db, &off, false).await;
    let (asked, _) = account(&db, &on, 0, true).await;
    let (disabled, _) = account(&db, &on, 0, false).await;
    let (quiet, _) = account(&db, &off, 0, true).await;
    let seen = ["m".to_owned()];
    set_served_models(&db, asked, &seen).await.expect("served");
    set_served_models(&db, quiet, &seen).await.expect("served");

    let swept = discovering_endpoint_accounts(&db).await;
    let forgot = forget_undiscovered_served_models(&db).await;
    let served = |id: Uuid| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (Option<Vec<String>>, Option<time::OffsetDateTime>)>(
                "SELECT served_models, served_models_at FROM account WHERE id = $1",
            )
            .bind(id)
            .fetch_one(db.pool())
            .await
            .expect("an account")
        }
    };
    let (asked_after, quiet_after) = (served(asked).await, served(quiet).await);
    remove(&db, &[&on, &off]).await;

    let swept: Vec<Uuid> = swept.expect("sweep").into_iter().map(|r| r.id).collect();
    assert!(swept.contains(&asked), "{swept:?}");
    assert!(!swept.contains(&disabled), "a disabled key is not asked");
    assert!(
        !swept.contains(&quiet),
        "nor a key of an endpoint that does not discover"
    );
    assert!(forgot.expect("forget") >= 1);
    assert_eq!(asked_after.0.as_deref(), Some(&seen[..]), "kept");
    assert!(asked_after.1.is_some());
    assert_eq!(
        quiet_after,
        (None, None),
        "forgotten: never asked, as far as the listing can tell"
    );
}
