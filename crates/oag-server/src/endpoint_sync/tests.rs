//! The catalog sync: the plan it makes from a list, and, gated on Postgres,
//! what it writes, read from a stand-in upstream serving the committed Merge
//! fixture.

use super::*;
use futures_util::FutureExt as _;
use oag_store::NewEndpoint;
use oag_upstream::listing::priced_entries;
use rust_decimal::{Decimal, dec};
use serde_json::{Value, json};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockGuard, MockServer, ResponseTemplate};

const PAGE_1: &str = include_str!("../../../oag-upstream/tests/fixtures/merge-models-page-1.json");
const PAGE_2: &str = include_str!("../../../oag-upstream/tests/fixtures/merge-models-page-2.json");
/// The fixture's first page's `next_cursor`.
const CURSOR: &str = "eyJhZnRlciI6Im1pc3RyYWwvbWlzdHJhbC1sYXJnZS0yNDA3In0";

const GLM: &str = "zai/glm-5.3-flash";
const SONNET: &str = "anthropic/claude-sonnet-4.5";
const DEEPSEEK: &str = "deepseek/deepseek-v3.2";

fn page(raw: &str) -> Value {
    serde_json::from_str(raw).expect("a fixture page")
}

/// A fixture page with the named entries taken out.
fn without(raw: &str, gone: &[&str]) -> Value {
    let mut page = page(raw);
    page["data"]
        .as_array_mut()
        .expect("a list")
        .retain(|m| !gone.contains(&m["model"].as_str().unwrap_or_default()));
    page
}

/// Every entry of the fixture, as the sync reads it.
fn listed() -> Vec<ListedModel> {
    [PAGE_1, PAGE_2]
        .iter()
        .flat_map(|raw| priced_entries(&page(raw)))
        .collect()
}

fn stored(model: ModelRow, is_override: bool) -> StoredModelRow {
    StoredModelRow { model, is_override }
}

/// A row under `endpoint` with terms no list states.
fn any_row(endpoint: &str, upstream: &str) -> ModelRow {
    ModelRow {
        id: format!("{endpoint}/{upstream}"),
        provider: endpoint.to_owned(),
        upstream_name: upstream.to_owned(),
        input_per_mtok: Decimal::ONE,
        output_per_mtok: Decimal::TWO,
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
        context_window: 1_000,
        max_output_tokens: 100,
        supports_vision: false,
        supports_tools: false,
        supports_reasoning: false,
        supports_prompt_cache: false,
        display_label: None,
    }
}

#[test]
fn a_glob_is_runs_and_single_characters() {
    for (pattern, text) in [
        ("zai/*", GLM),
        ("*flash*", GLM),
        ("t6/zai/*", "t6/zai/glm"),
        ("?ai/*", "zai/x"),
        ("a*b*c", "aXbYc"),
        ("**", "abc"),
        ("*", ""),
        ("", ""),
        (GLM, GLM),
    ] {
        assert!(glob(pattern, text), "{pattern} matches {text}");
    }
    for (pattern, text) in [
        ("zai/*", SONNET),
        ("a*b*c", "aXbY"),
        ("a?c", "ac"),
        ("", "a"),
        ("zai", GLM),
        ("*a", "ab"),
        ("a*", "ba"),
    ] {
        assert!(!glob(pattern, text), "{pattern} does not match {text}");
    }
}

/// A filter says which models this sync manages: those it leaves out are
/// neither written nor removed, whatever the list says of them.
#[test]
fn a_filter_scopes_both_what_a_sync_writes_and_what_it_may_remove() {
    let e = "t6-plan";
    let existing = [
        stored(any_row(e, "zai/retired"), true),
        stored(any_row(e, SONNET), true),
    ];
    let included = plan(
        e,
        "Merge",
        &listed(),
        &existing,
        &HashSet::new(),
        &SyncOptions {
            include: vec!["zai/*".to_owned()],
            ..SyncOptions::default()
        },
    )
    .expect("the list offers GLM");
    assert_eq!(included.report.added, [format!("{e}/{GLM}")]);
    assert_eq!(included.writes.len(), 1);
    assert_eq!(
        included.remove,
        [format!("{e}/zai/retired")],
        "no longer listed, and the filter's to manage"
    );
    assert_eq!(included.report.filtered.len(), 9);
    assert!(included.report.filtered.contains(&SONNET.to_owned()));
    assert!(
        included.report.skipped.is_empty(),
        "an entry filtered out is not judged at all"
    );

    // A catalog id is a pattern too, and an exclude beats an include.
    let excluded = plan(
        e,
        "Merge",
        &listed(),
        &existing,
        &HashSet::new(),
        &SyncOptions {
            include: vec![format!("{e}/zai/*")],
            exclude: vec!["*retired".to_owned()],
            ..SyncOptions::default()
        },
    )
    .expect("the list offers GLM");
    assert_eq!(excluded.report.added, [format!("{e}/{GLM}")]);
    assert!(
        excluded.remove.is_empty(),
        "excluded, so it stays whatever the list says"
    );
}

/// A list that offers nothing to write is a list the sync cannot use, not an
/// endpoint that stopped serving: it is refused before anything is removed.
#[test]
fn a_list_that_offers_nothing_plans_nothing() {
    let e = "t6-plan";
    let existing = [stored(any_row(e, GLM), true)];
    let err = plan(
        e,
        "Merge",
        &listed(),
        &existing,
        &HashSet::new(),
        &SyncOptions {
            include: vec!["nothing/*".to_owned()],
            ..SyncOptions::default()
        },
    )
    .expect_err("nothing matched");
    assert_eq!(err, "0 skipped, 10 filtered out");

    let unservable: Vec<ListedModel> = listed()
        .into_iter()
        .filter(|m| listing::choose(m, PriceChoice::Cheapest).is_err())
        .collect();
    let err = plan(
        e,
        "Merge",
        &unservable,
        &existing,
        &HashSet::new(),
        &SyncOptions::default(),
    )
    .expect_err("nothing to price");
    assert_eq!(err, "7 skipped, 0 filtered out");
}

/// A row is unchanged only when it is already an override with the list's
/// terms and a label; an id another provider's row holds is left to it.
#[test]
fn a_row_is_unchanged_only_as_an_override_with_the_same_terms_and_a_label() {
    let e = "t6-plan";
    let glm: Vec<ListedModel> = listed()
        .into_iter()
        .filter(|m| m.upstream.as_deref() == Some(GLM))
        .collect();
    let offer = listing::choose(&glm[0], PriceChoice::Cheapest).expect("priced");
    let same = catalog_row(e, "Merge", &offer);
    assert_eq!(
        same.id.split_once('/'),
        Some((e, GLM)),
        "split at the first slash"
    );
    assert_eq!(same.display_label.as_deref(), Some("GLM 5.3 Flash (Merge)"));

    let decide = |row: StoredModelRow| {
        let p = plan(
            e,
            "Merge",
            &glm,
            &[row],
            &HashSet::new(),
            &SyncOptions::default(),
        )
        .expect("GLM is offered");
        let r = p.report;
        (
            r.added.len(),
            r.updated.len(),
            r.unchanged.len(),
            r.held.len(),
            p.writes.len(),
        )
    };
    assert_eq!(decide(stored(same.clone(), true)), (0, 0, 1, 0, 0));
    assert_eq!(
        decide(stored(same.clone(), false)),
        (0, 1, 0, 0, 1),
        "not an override yet"
    );
    let mut nameless = same.clone();
    nameless.display_label = None;
    assert_eq!(
        decide(stored(nameless, true)),
        (0, 1, 0, 0, 1),
        "a label to give"
    );
    let mut named = same.clone();
    named.display_label = Some("Mine".to_owned());
    assert_eq!(
        decide(stored(named, true)),
        (0, 0, 1, 0, 0),
        "the operator's label is not a difference"
    );
    let mut repriced = same.clone();
    repriced.input_per_mtok = dec!(0.07);
    assert_eq!(decide(stored(repriced, true)), (0, 1, 0, 0, 1));
    let mut theirs = same;
    theirs.provider = "anthropic".to_owned();
    assert_eq!(decide(stored(theirs, true)), (0, 0, 0, 1, 0));
}

/// A row the list no longer offers is removed, unless a ladder names it.
#[test]
fn a_stale_row_a_ladder_names_is_planned_to_stay() {
    let e = "t6-plan";
    let existing = [
        stored(any_row(e, "gone/a"), true),
        stored(any_row(e, "gone/b"), true),
    ];
    let laddered = HashSet::from([format!("{e}/gone/a")]);
    let p = plan(
        e,
        "Merge",
        &listed(),
        &existing,
        &laddered,
        &SyncOptions {
            price: PriceChoice::First,
            dry_run: true,
            ..SyncOptions::default()
        },
    )
    .expect("the list offers three");
    assert_eq!(p.report.kept_on_ladder, [format!("{e}/gone/a")]);
    assert_eq!(p.remove, [format!("{e}/gone/b")]);
    assert_eq!(p.report.removed, p.remove);
    // And the report says whose sync it is, and how it was asked for.
    assert_eq!(p.report.endpoint, e);
    assert_eq!(p.report.price, PriceChoice::First);
    assert!(p.report.dry_run);
}

/// What the write found overrides what the plan saw: an id another
/// provider's row came to hold is held, neither added nor updated, and a
/// stale row a ladder came to name is kept rather than removed.
#[test]
fn the_write_has_the_last_word_on_what_was_written_and_removed() {
    let mut report = SyncReport {
        added: vec!["e/a".to_owned(), "e/b".to_owned()],
        updated: vec!["e/c".to_owned(), "e/d".to_owned()],
        kept_on_ladder: vec!["e/k".to_owned()],
        ..SyncReport::default()
    };
    settle(
        &mut report,
        EndpointSync {
            written: vec!["e/a".to_owned(), "e/d".to_owned()],
            held: vec!["e/b".to_owned(), "e/c".to_owned()],
            removed: vec!["e/x".to_owned()],
        },
        vec!["e/x".to_owned(), "e/y".to_owned()],
    );
    assert_eq!(report.added, ["e/a"]);
    assert_eq!(report.updated, ["e/d"]);
    assert_eq!(report.held, ["e/b", "e/c"]);
    assert_eq!(report.removed, ["e/x"]);
    assert_eq!(report.kept_on_ladder, ["e/k", "e/y"]);
}

/// A stand-in Merge on a mock server, an endpoint registered for it, and one
/// key filed under the endpoint, on the test database.
struct Merge {
    db: Db,
    kek: Kek,
    server: MockServer,
    endpoint: String,
    /// The credential filed under the endpoint, by name, if one is.
    key: Option<String>,
}

impl Merge {
    /// `None` when `OAG_TEST_DATABASE_URL` is unset. The endpoint takes its key
    /// as `auth` says, and has one filed under it only when `keyed`.
    async fn start(base_path: &str, auth: &str, keyed: bool) -> Option<Self> {
        let url = std::env::var("OAG_TEST_DATABASE_URL").ok()?;
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");
        let kek = Kek::from_base64(&crate::testing::config("").security.credential_kek)
            .expect("the test key");
        let server = MockServer::start().await;
        let endpoint = format!("t6-m{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        repo::insert_endpoint(
            &db,
            &NewEndpoint {
                name: &endpoint,
                dialect: "openai",
                platform: "plain",
                base_url: Some(&format!("{}{base_path}", server.uri())),
                auth,
                region: None,
                project: None,
                api_version: None,
                extra_headers: &json!({}),
                display_name: Some("Merge Mock"),
                discover_models: false,
            },
        )
        .await
        .expect("an endpoint");
        if !keyed {
            return Some(Self {
                db,
                kek,
                server,
                endpoint,
                key: None,
            });
        }
        let key = format!("{endpoint}-key");
        let sealed = kek
            .seal_json(&SecretMaterial {
                access_token: "t6-sync-key".to_owned(),
                refresh_token: None,
                expires_at: None,
                version: 0,
                client_id: None,
                account_id: None,
            })
            .expect("seal");
        sqlx::query(
            "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
             credentials_nonce) VALUES (gen_random_uuid(), $1, $2, 'api_key', $3, $4)",
        )
        .bind(&key)
        .bind(&endpoint)
        .bind(&sealed.ciphertext)
        .bind(&sealed.nonce)
        .execute(db.pool())
        .await
        .expect("a key");
        Some(Self {
            db,
            kek,
            server,
            endpoint,
            key: Some(key),
        })
    }

    /// Serve `first` and `second` as the two pages of the priced list at the
    /// origin's `/v1/models`, read with the endpoint's key, and nothing at the
    /// dialect's own `{base}/models`: where Merge keeps them.
    async fn serve(&self, first: Value, second: Value) -> [MockGuard; 3] {
        [
            Mock::given(method("GET"))
                .and(path("/v1/openai/models"))
                .respond_with(ResponseTemplate::new(404))
                .mount_as_scoped(&self.server)
                .await,
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .and(query_param("limit", "500"))
                .and(query_param_is_missing("cursor"))
                .and(header("authorization", "Bearer t6-sync-key"))
                .respond_with(ResponseTemplate::new(200).set_body_json(first))
                .mount_as_scoped(&self.server)
                .await,
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .and(query_param("limit", "500"))
                .and(query_param("cursor", CURSOR))
                .and(header("authorization", "Bearer t6-sync-key"))
                .respond_with(ResponseTemplate::new(200).set_body_json(second))
                .mount_as_scoped(&self.server)
                .await,
        ]
    }

    async fn sync(&self, options: SyncOptions) -> Result<SyncReport> {
        super::sync(&self.db, &self.kek, &self.endpoint, &options).await
    }

    fn id(&self, upstream: &str) -> String {
        format!("{}/{upstream}", self.endpoint)
    }

    async fn rows(&self) -> Vec<Terms> {
        repo::provider_models(&self.db, &self.endpoint)
            .await
            .expect("rows")
            .iter()
            .map(terms)
            .collect()
    }

    async fn remove(&self) {
        for table in ["model_catalog", "account"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE provider = $1"
            )))
            .bind(&self.endpoint)
            .execute(self.db.pool())
            .await
            .expect("clean up");
        }
        repo::delete_endpoint(&self.db, &self.endpoint)
            .await
            .expect("clean up");
    }
}

/// Run `test` against a fresh [`Merge`] that takes a bearer key and has one,
/// and remove what it wrote whether or not the test passed.
async fn with_merge<F, Fut>(base_path: &str, test: F)
where
    F: FnOnce(Arc<Merge>) -> Fut,
    Fut: Future<Output = ()>,
{
    with_merge_as(base_path, "bearer", true, test).await;
}

/// [`with_merge`], for an endpoint that takes its key as `auth` says, with one
/// filed under it only when `keyed`.
async fn with_merge_as<F, Fut>(base_path: &str, auth: &str, keyed: bool, test: F)
where
    F: FnOnce(Arc<Merge>) -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(merge) = Merge::start(base_path, auth, keyed).await else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let merge = Arc::new(merge);
    let outcome = AssertUnwindSafe(test(Arc::clone(&merge)))
        .catch_unwind()
        .await;
    merge.remove().await;
    if let Err(failed) = outcome {
        std::panic::resume_unwind(failed);
    }
}

/// A catalog row's every column a sync writes, comparable in one assertion.
// The flags are the catalog's columns, one for one.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, PartialEq)]
struct Terms {
    id: String,
    provider: String,
    upstream: String,
    input: Decimal,
    output: Decimal,
    cache_read: Option<Decimal>,
    cache_write: Option<Decimal>,
    context: i32,
    max_output: i32,
    vision: bool,
    tools: bool,
    reasoning: bool,
    prompt_cache: bool,
    label: Option<String>,
    is_override: bool,
}

fn terms(row: &StoredModelRow) -> Terms {
    let m = &row.model;
    Terms {
        id: m.id.clone(),
        provider: m.provider.clone(),
        upstream: m.upstream_name.clone(),
        input: m.input_per_mtok,
        output: m.output_per_mtok,
        cache_read: m.cache_read_per_mtok,
        cache_write: m.cache_write_per_mtok,
        context: m.context_window,
        max_output: m.max_output_tokens,
        vision: m.supports_vision,
        tools: m.supports_tools,
        reasoning: m.supports_reasoning,
        prompt_cache: m.supports_prompt_cache,
        label: m.display_label.clone(),
        is_override: row.is_override,
    }
}

/// The three chat models the fixture prices, as the cheapest vendor of each
/// terms them, in id order.
fn cheapest_rows(m: &Merge) -> Vec<Terms> {
    vec![
        Terms {
            id: m.id(SONNET),
            provider: m.endpoint.clone(),
            upstream: SONNET.to_owned(),
            input: dec!(3),
            output: dec!(15),
            cache_read: Some(dec!(0.3)),
            cache_write: Some(dec!(3.75)),
            context: 200_000,
            max_output: 64_000,
            vision: true,
            tools: true,
            reasoning: true,
            prompt_cache: true,
            label: Some("Claude Sonnet 4.5 (Merge Mock)".to_owned()),
            is_override: true,
        },
        Terms {
            id: m.id(DEEPSEEK),
            provider: m.endpoint.clone(),
            upstream: DEEPSEEK.to_owned(),
            input: dec!(0.28),
            output: dec!(0.42),
            cache_read: Some(dec!(0.028)),
            cache_write: None,
            context: 128_000,
            max_output: 8_192,
            vision: false,
            tools: true,
            reasoning: true,
            prompt_cache: true,
            label: Some("DeepSeek V3.2 (Merge Mock)".to_owned()),
            is_override: true,
        },
        // Priced by `particle`, the cheapest of its three vendors, which is
        // also where its window and capabilities come from.
        Terms {
            id: m.id(GLM),
            provider: m.endpoint.clone(),
            upstream: GLM.to_owned(),
            input: dec!(0.06),
            output: dec!(0.22),
            cache_read: None,
            cache_write: None,
            context: 131_072,
            max_output: 32_768,
            vision: false,
            tools: true,
            reasoning: false,
            prompt_cache: false,
            label: Some("GLM 5.3 Flash (Merge Mock)".to_owned()),
            is_override: true,
        },
    ]
}

/// The fixture, found at Merge's origin across two pages, becomes exactly
/// these rows; a second sync changes none of them; and pricing by the first
/// vendor instead changes the one model whose first vendor is not its
/// cheapest.
#[tokio::test]
async fn a_merge_list_becomes_exact_catalog_rows_and_a_second_sync_changes_nothing() {
    with_merge("/v1/openai", |m| async move {
        let _served = m.serve(page(PAGE_1), page(PAGE_2)).await;

        let first = m.sync(SyncOptions::default()).await.expect("synced");
        assert_eq!(
            first.url,
            format!("{}/v1/models?limit=500", m.server.uri()),
            "found at the origin, not at {{base}}/models"
        );
        assert_eq!(first.pages, 2);
        assert_eq!(first.account.as_deref(), m.key.as_deref());
        assert_eq!(first.price, PriceChoice::Cheapest);
        assert!(!first.dry_run);
        assert_eq!(first.added, [m.id(GLM), m.id(SONNET), m.id(DEEPSEEK)]);
        for quiet in [
            &first.updated,
            &first.unchanged,
            &first.removed,
            &first.kept_on_ladder,
            &first.held,
            &first.filtered,
        ] {
            assert!(quiet.is_empty(), "{first:?}");
        }
        let skipped: Vec<(&str, &str)> = first
            .skipped
            .iter()
            .map(|(name, why)| (name.as_str(), why.label()))
            .collect();
        assert_eq!(
            skipped,
            [
                ("openai/gpt-4o-mini-tts", "not a chat model"),
                ("openai/gpt-image-1", "not a chat model"),
                ("google/veo-3", "not a chat model"),
                ("mistral/mistral-large-2407", "deprecated or unavailable"),
                ("openai/o3-deep-research", "access required"),
                ("meta/llama-4-scout", "no per-token price"),
                ("moonshot/kimi-k2-free", "listed free"),
            ]
        );
        assert_eq!(m.rows().await, cheapest_rows(&m));

        let again = m.sync(SyncOptions::default()).await.expect("synced");
        assert_eq!(again.unchanged, [m.id(GLM), m.id(SONNET), m.id(DEEPSEEK)]);
        assert!(again.added.is_empty() && again.updated.is_empty() && again.removed.is_empty());
        assert_eq!(m.rows().await, cheapest_rows(&m), "idempotent");

        let first_vendor = m
            .sync(SyncOptions {
                price: PriceChoice::First,
                ..SyncOptions::default()
            })
            .await
            .expect("synced");
        assert_eq!(first_vendor.updated, [m.id(GLM)]);
        assert_eq!(first_vendor.unchanged, [m.id(SONNET), m.id(DEEPSEEK)]);
        let mut expected = cheapest_rows(&m);
        let glm = expected.last_mut().expect("GLM sorts last");
        (glm.input, glm.output, glm.cache_read) = (dec!(0.1), dec!(0.4), Some(dec!(0.02)));
        (glm.context, glm.max_output) = (200_000, 128_000);
        (glm.reasoning, glm.prompt_cache) = (true, true);
        assert_eq!(
            m.rows().await,
            expected,
            "priced by `zai`, listed first, and termed by it"
        );
    })
    .await;
}

/// A model the list stops offering is removed, unless a ladder names it: then
/// it stays, and the report says so.
#[tokio::test]
async fn a_model_dropped_from_the_list_is_removed_unless_a_ladder_names_it() {
    with_merge("/v1/openai", |m| async move {
        {
            let _served = m.serve(page(PAGE_1), page(PAGE_2)).await;
            m.sync(SyncOptions::default()).await.expect("first sync");
        }
        let route = format!(
            "t6-route-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, $2)")
            .bind(&route)
            .bind(json!([{"name": "cheap", "models": [m.id(DEEPSEEK)]}]))
            .execute(m.db.pool())
            .await
            .expect("a route");
        let _served = m
            .serve(without(PAGE_1, &[SONNET]), without(PAGE_2, &[DEEPSEEK]))
            .await;

        let report = m.sync(SyncOptions::default()).await;
        sqlx::query("DELETE FROM route WHERE name = $1")
            .bind(&route)
            .execute(m.db.pool())
            .await
            .expect("clean up");
        let report = report.expect("second sync");

        assert_eq!(report.removed, [m.id(SONNET)]);
        assert_eq!(report.kept_on_ladder, [m.id(DEEPSEEK)]);
        assert_eq!(report.unchanged, [m.id(GLM)]);
        let ids: Vec<String> = m.rows().await.into_iter().map(|t| t.id).collect();
        assert_eq!(ids, [m.id(DEEPSEEK), m.id(GLM)]);
    })
    .await;
}

/// An id-only `/models`, a key nobody filed, and a listing URL on another host
/// are each refused, and none of them writes a row.
#[tokio::test]
async fn an_id_only_list_is_refused_and_nothing_is_written() {
    with_merge("/v1", |m| async move {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer t6-sync-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"id": "m-1", "object": "model"}, {"id": "m-2", "object": "model"}]
            })))
            .expect(1)
            .mount(&m.server)
            .await;

        let refused = m
            .sync(SyncOptions::default())
            .await
            .expect_err("no prices")
            .to_string();
        assert!(refused.contains("without prices"), "{refused}");
        assert!(refused.contains("oag admin catalog add"), "{refused}");

        let unknown = m
            .sync(SyncOptions {
                account: Some("t6-nobody".to_owned()),
                ..SyncOptions::default()
            })
            .await
            .expect_err("no such key")
            .to_string();
        assert!(
            unknown.contains("no credential named 't6-nobody'"),
            "{unknown}"
        );

        let elsewhere = m
            .sync(SyncOptions {
                listing_url: Some("https://elsewhere.example/v1/models".to_owned()),
                ..SyncOptions::default()
            })
            .await
            .expect_err("another origin")
            .to_string();
        assert!(elsewhere.contains("origin"), "{elsewhere}");

        assert!(m.rows().await.is_empty());
    })
    .await;
}

/// A dry run reports what a sync would add and remove, and does neither.
#[tokio::test]
async fn a_dry_run_reports_and_writes_nothing() {
    with_merge("/v1/openai", |m| async move {
        let served = m.serve(page(PAGE_1), page(PAGE_2)).await;
        let planned = m
            .sync(SyncOptions {
                dry_run: true,
                ..SyncOptions::default()
            })
            .await
            .expect("planned");
        assert!(planned.dry_run);
        assert_eq!(planned.added, [m.id(GLM), m.id(SONNET), m.id(DEEPSEEK)]);
        assert!(m.rows().await.is_empty(), "nothing added");

        m.sync(SyncOptions::default()).await.expect("written");
        drop(served);
        let _served = m.serve(without(PAGE_1, &[SONNET]), page(PAGE_2)).await;
        let planned = m
            .sync(SyncOptions {
                dry_run: true,
                ..SyncOptions::default()
            })
            .await
            .expect("planned");
        assert_eq!(planned.removed, [m.id(SONNET)]);
        assert_eq!(m.rows().await, cheapest_rows(&m), "nothing removed");
    })
    .await;
}

/// `endpoint models` reads the list discovery reads, and sets it beside the
/// catalog: which listed ids it serves, and which of its rows the list does
/// not name.
#[tokio::test]
async fn the_list_discovery_reads_is_reported_beside_the_catalog() {
    with_merge("/v1/openai", |m| async move {
        {
            let _served = m.serve(page(PAGE_1), page(PAGE_2)).await;
            m.sync(SyncOptions::default()).await.expect("rows");
        }
        Mock::given(method("GET"))
            .and(path("/v1/openai/models"))
            .and(header("authorization", "Bearer t6-sync-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "data": [{"id": GLM}, {"id": "zai/new-model"}]
            })))
            .mount(&m.server)
            .await;

        let report = super::models(&m.db, &m.kek, &m.endpoint, None)
            .await
            .expect("read");
        assert_eq!(report.url, format!("{}/v1/openai/models", m.server.uri()));
        assert_eq!(report.pages, 1);
        assert!(!report.discover);
        assert_eq!(report.account.as_deref(), m.key.as_deref());
        assert_eq!(
            report.listed,
            [
                (GLM.to_owned(), Some(m.id(GLM))),
                ("zai/new-model".to_owned(), None)
            ]
        );
        assert_eq!(report.unlisted, [m.id(SONNET), m.id(DEEPSEEK)]);
    })
    .await;
}

/// Every header a key can ride in.
const KEY_HEADERS: [&str; 4] = ["authorization", "x-api-key", "x-goog-api-key", "api-key"];

/// An endpoint that takes no key is synced and listed with none filed: the
/// sync and the list discovery reads both reach it, and no request carries a
/// key header.
#[tokio::test]
async fn an_endpoint_that_takes_no_key_is_read_without_one() {
    with_merge_as("/v1/openai", "none", false, |m| async move {
        Mock::given(method("GET"))
            .and(path("/v1/openai/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "data": [{"id": GLM}]
            })))
            .mount(&m.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param_is_missing("cursor"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(PAGE_1)))
            .mount(&m.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param("cursor", CURSOR))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(PAGE_2)))
            .mount(&m.server)
            .await;

        let synced = m
            .sync(SyncOptions::default())
            .await
            .expect("synced without a key");
        assert_eq!(synced.account, None);
        assert_eq!(synced.added, [m.id(GLM), m.id(SONNET), m.id(DEEPSEEK)]);
        let listed = super::models(&m.db, &m.kek, &m.endpoint, None)
            .await
            .expect("listed without a key");
        assert_eq!(listed.account, None);
        assert_eq!(listed.listed, [(GLM.to_owned(), Some(m.id(GLM)))]);

        let sent = m.server.received_requests().await.expect("recording");
        assert_eq!(
            sent.len(),
            4,
            "the sync's look at {{base}}/models, its two pages, and the list discovery reads"
        );
        for request in &sent {
            for name in KEY_HEADERS {
                assert!(
                    request.headers.get(name).is_none(),
                    "{name} on {}",
                    request.url
                );
            }
        }
    })
    .await;
}

/// An endpoint that takes a key and has none filed is refused, by the sync
/// and by the list both, before anything is sent to it.
#[tokio::test]
async fn an_endpoint_that_takes_a_key_and_has_none_is_refused_before_anything_is_sent() {
    with_merge_as("/v1/openai", "bearer", false, |m| async move {
        let refusal = format!(
            "configuration: endpoint '{0}' has no schedulable credential to read its model list \
             with; add one with `oag admin account add --provider {0}`, or name one with \
             --account",
            m.endpoint
        );
        let synced = m
            .sync(SyncOptions::default())
            .await
            .expect_err("no key to read with");
        assert_eq!(synced.to_string(), refusal);
        let listed = super::models(&m.db, &m.kek, &m.endpoint, None)
            .await
            .expect_err("no key to read with");
        assert_eq!(listed.to_string(), refusal);
        assert!(
            m.server
                .received_requests()
                .await
                .expect("recording")
                .is_empty()
        );
    })
    .await;
}
