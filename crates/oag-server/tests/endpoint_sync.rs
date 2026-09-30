//! An endpoint's priced model list, end to end: a stand-in Merge Gateway on a
//! mock server, `endpoint_sync::sync` writing its catalog rows, and a real
//! gateway on real ports serving them without a restart: listing them, sending
//! each request with the model's own multi-slash name, pricing it by the vendor
//! the sync chose, and narrowing the listing once discovery is turned on.
//!
//! Gated like every test that needs the store: skipped unless
//! `OAG_TEST_DATABASE_URL` and `OAG_TEST_REDIS_URL` are set. It makes a
//! database of its own and drops it afterwards, and it is alone in its binary,
//! because the endpoint registry is process-wide and every reload replaces it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use futures_util::FutureExt as _;
use oag_core::credential::SecretMaterial;
use oag_server::AppState;
use oag_server::endpoint_sync::{SyncOptions, sync};
use oag_store::{Cache, Db, EndpointUpdate, NewEndpoint, repo};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockGuard, MockServer, Request, ResponseTemplate};

const PAGE_1: &str = include_str!("../../oag-upstream/tests/fixtures/merge-models-page-1.json");
const PAGE_2: &str = include_str!("../../oag-upstream/tests/fixtures/merge-models-page-2.json");
/// The fixture's first page's `next_cursor`.
const CURSOR: &str = "eyJhZnRlciI6Im1pc3RyYWwvbWlzdHJhbC1sYXJnZS0yNDA3In0";

const ENDPOINT: &str = "mergemock";
const GLM: &str = "mergemock/zai/glm-5.3-flash";
const SONNET: &str = "mergemock/anthropic/claude-sonnet-4.5";
const DEEPSEEK: &str = "mergemock/deepseek/deepseek-v3.2";
const KEY: &str = "t6-merge-key";

/// How long a change may take to show. The refresh and the poller run every
/// second; this allows for a loaded machine.
const WAIT: Duration = Duration::from_secs(45);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_synced_merge_catalog_is_served_by_its_multi_slash_names_and_narrowed_by_discovery() {
    let (Ok(admin_url), Ok(redis_url)) = (
        std::env::var("OAG_TEST_DATABASE_URL"),
        std::env::var("OAG_TEST_REDIS_URL"),
    ) else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
        return;
    };
    let (db_url, name) = scratch_database(&admin_url).await;
    let outcome = std::panic::AssertUnwindSafe(scenario(db_url, redis_url))
        .catch_unwind()
        .await;
    drop_database(&admin_url, &name).await;
    if let Err(failed) = outcome {
        std::panic::resume_unwind(failed);
    }
}

async fn scenario(db_url: String, redis_url: String) {
    let gw = Gateway::start(&db_url, &redis_url).await;
    let merge = MockServer::start().await;
    // The dialect's own `{base}/models` answers with ids alone, which is what
    // discovery reads; the priced list is at the origin, over two pages.
    Mock::given(method("GET"))
        .and(path("/v1/openai/models"))
        .and(header("authorization", "Bearer t6-merge-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "zai/glm-5.3-flash"}, {"id": "anthropic/claude-sonnet-4.5"}]
        })))
        .mount(&merge)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param_is_missing("cursor"))
        .and(header("authorization", "Bearer t6-merge-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE_1, "application/json"))
        .mount(&merge)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("cursor", CURSOR))
        .and(header("authorization", "Bearer t6-merge-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE_2, "application/json"))
        .mount(&merge)
        .await;
    let base = format!("{}/v1/openai", merge.uri());

    // Written while the gateway serves: only its refresh picks them up.
    repo::insert_endpoint(
        &gw.db,
        &NewEndpoint {
            name: ENDPOINT,
            dialect: "openai",
            platform: "plain",
            base_url: Some(&base),
            auth: "bearer",
            region: None,
            project: None,
            api_version: None,
            path: None,
            extra_headers: &json!({}),
            display_name: Some("Merge"),
            discover_models: false,
        },
    )
    .await
    .expect("the endpoint");
    let key = gw.account(ENDPOINT, KEY).await;
    let report = sync(&gw.db, &gw.state.kek, ENDPOINT, &SyncOptions::default())
        .await
        .expect("the sync");
    assert_eq!(report.added, [GLM, SONNET, DEEPSEEK]);
    assert_eq!(report.url, format!("{}/v1/models?limit=500", merge.uri()));
    sqlx::query("UPDATE route SET tiers = $1::jsonb WHERE id = $2")
        .bind(json!([{"name": "cheap", "models": [GLM]}]))
        .bind(gw.route)
        .execute(gw.db.pool())
        .await
        .expect("the ladder");

    let listed = gw
        .listing_until("the synced models are listed", |ids| {
            [GLM, SONNET, DEEPSEEK]
                .iter()
                .all(|id| ids.iter().any(|listed| listed == id))
        })
        .await;
    assert!(
        !listed
            .iter()
            .any(|id| id.contains("veo-3") || id.contains("tts")),
        "what the sync skipped is not served: {listed:?}"
    );

    chat_by_the_models_full_name(&gw, &merge).await;
    gemini_path_with_slashes_in_the_model(&gw, &merge).await;
    rung_pin(&gw, &merge).await;
    discovery_narrows_the_listing(&gw, &base, key).await;
}

/// The catalog id is the endpoint's name, a slash, and the model's own name,
/// slashes and all: the request carries the model's own name, and the ledger
/// prices it by the vendor the sync chose.
async fn chat_by_the_models_full_name(gw: &Gateway, merge: &MockServer) {
    let mock = answering(merge, openai_answer("hello from particle")).await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": GLM, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let request_id = request_id_of(&res);
    let body = ok_json(res, "chat").await;
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from particle"
    );
    assert_eq!(
        body_of(&only_request(&mock).await)["model"],
        "zai/glm-5.3-flash",
        "the upstream name: everything after the endpoint's first slash"
    );

    let (model, cost) = gw.ledger(request_id).await;
    assert_eq!(model, GLM);
    // 100k in at $0.06 and 50k out at $0.22 per million: `particle`'s terms,
    // the cheapest vendor's. `zai`'s would be $0.03.
    assert_eq!(cost, Decimal::from_str("0.017").unwrap());
}

/// The Gemini surface carries the model in its path, where the id's slashes
/// sit between `models/` and `:generateContent`.
async fn gemini_path_with_slashes_in_the_model(gw: &Gateway, merge: &MockServer) {
    let mock = answering(merge, openai_answer("hello through gemini")).await;
    let res = gw
        .post(
            &format!("/v1beta/models/{GLM}:generateContent"),
            &json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}),
        )
        .await;
    let body = ok_json(res, "gemini path").await;
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "hello through gemini"
    );
    assert_eq!(
        body_of(&only_request(&mock).await)["model"],
        "zai/glm-5.3-flash"
    );
}

/// A rung naming a multi-slash id serves it. The answer passes through in the
/// upstream's own bytes, so the ledger is what names the catalog id.
async fn rung_pin(gw: &Gateway, merge: &MockServer) {
    let mock = answering(merge, openai_answer("hello from the rung")).await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "oag/cheap", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let request_id = request_id_of(&res);
    let body = ok_json(res, "rung pin").await;
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from the rung"
    );
    assert_eq!(
        body_of(&only_request(&mock).await)["model"],
        "zai/glm-5.3-flash"
    );
    assert_eq!(gw.ledger(request_id).await.0, GLM);
}

/// Turned on, discovery reads `{base}/models` with the endpoint's key, records
/// what it names, and the listing stops offering the synced model it does not.
async fn discovery_narrows_the_listing(gw: &Gateway, base: &str, key: Uuid) {
    repo::update_endpoint(
        &gw.db,
        ENDPOINT,
        &EndpointUpdate {
            base_url: Some(base),
            auth: "bearer",
            region: None,
            project: None,
            api_version: None,
            path: None,
            extra_headers: &json!({}),
            display_name: Some("Merge"),
            discover_models: true,
        },
    )
    .await
    .expect("discovery on")
    .expect("the endpoint");

    let listed = gw
        .listing_until("the model the list does not name is withdrawn", |ids| {
            !ids.iter().any(|id| id == DEEPSEEK)
        })
        .await;
    assert!(listed.iter().any(|id| id == GLM), "{listed:?}");
    assert!(listed.iter().any(|id| id == SONNET), "{listed:?}");
    let served: Option<Vec<String>> =
        sqlx::query_scalar("SELECT served_models FROM account WHERE id = $1")
            .bind(key)
            .fetch_one(gw.db.pool())
            .await
            .expect("the key");
    assert_eq!(
        served,
        Some(vec![
            "zai/glm-5.3-flash".to_owned(),
            "anthropic/claude-sonnet-4.5".to_owned()
        ])
    );
}

/// A gateway on real ports, with a principal, a route and an inference key of
/// its own.
struct Gateway {
    state: Arc<AppState>,
    db: Db,
    client: reqwest::Client,
    public: String,
    key: String,
    route: Uuid,
}

impl Gateway {
    async fn start(db_url: &str, redis_url: &str) -> Self {
        let (public, admin) = (free_port(), free_port());
        let (public_port, admin_port) = (port_of(&public), port_of(&admin));
        drop((public, admin));
        // Every built-in on a closed port: nothing this gateway could send
        // leaves the machine.
        let config = oag_core::config::Config::from_yaml(&format!(
            r#"
database:
  url: "{db_url}"
redis:
  url: "{redis_url}"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
server:
  public_addr: "127.0.0.1:{public_port}"
  admin_addr: "127.0.0.1:{admin_port}"
gateway:
  catalog_refresh_interval: 1
  usage_poll_interval: 1
  same_account_retries: 0
  provider_base_urls:
    anthropic: "http://127.0.0.1:1"
    openai: "http://127.0.0.1:1"
    gemini: "http://127.0.0.1:1"
    kimi: "http://127.0.0.1:1"
    deepseek: "http://127.0.0.1:1"
    zhipu: "http://127.0.0.1:1"
    xai: "http://127.0.0.1:1"
    bedrock: "http://127.0.0.1:1"
    jev: "http://127.0.0.1:1"
"#
        ))
        .expect("test config");
        let db = Db::connect(db_url, 8).expect("pool");
        db.migrate().await.expect("migrate");
        let cache = Cache::connect(redis_url).expect("redis");
        let state = Arc::new(AppState::new(config, db.clone(), cache).expect("state"));
        let recorder = oag_server::metrics::install().expect("the one recorder");
        state.lifecycle.set_metrics(recorder);
        assert_eq!(state.reload_catalog().await.expect("the boot load"), 0);
        tokio::spawn(oag_server::serve(Arc::clone(&state)));

        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("client");
        let public = format!("http://127.0.0.1:{public_port}");
        let deadline = Instant::now() + WAIT;
        while client
            .get(format!("{public}/health/live"))
            .send()
            .await
            .is_err()
        {
            assert!(Instant::now() < deadline, "the gateway never listened");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let email = "t6-owner@example.invalid";
        sqlx::query("INSERT INTO principal (id, email) VALUES (gen_random_uuid(), $1)")
            .bind(email)
            .execute(db.pool())
            .await
            .expect("principal");
        // A route needs a rung to parse; the scenario replaces this one.
        let route: Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), 't6', \
             '[{\"name\": \"cheap\", \"models\": [\"anthropic/placeholder\"]}]'::jsonb) \
             RETURNING id",
        )
        .fetch_one(db.pool())
        .await
        .expect("route");
        let key = repo::mint_key(&db, email, "t6", "t6-inference", None)
            .await
            .expect("mint")
            .expect("the principal and route exist");
        Self {
            state,
            db,
            client,
            public,
            key: key.key,
            route,
        }
    }

    /// An API key filed under `provider`, sealed as `account add` seals one
    /// and joined to the route.
    async fn account(&self, provider: &str, secret: &str) -> Uuid {
        let sealed = self
            .state
            .kek
            .seal_json(&SecretMaterial {
                access_token: secret.to_owned(),
                refresh_token: None,
                expires_at: None,
                version: 0,
                client_id: None,
                account_id: None,
            })
            .expect("seal");
        let account: Uuid = sqlx::query_scalar(
            "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
             credentials_nonce) VALUES (gen_random_uuid(), $1, $2, 'api_key', $3, $4) \
             RETURNING id",
        )
        .bind(secret)
        .bind(provider)
        .bind(&sealed.ciphertext)
        .bind(&sealed.nonce)
        .fetch_one(self.db.pool())
        .await
        .expect("account");
        sqlx::query("INSERT INTO account_route (account_id, route_id) VALUES ($1, $2)")
            .bind(account)
            .bind(self.route)
            .execute(self.db.pool())
            .await
            .expect("join");
        account
    }

    async fn post(&self, path: &str, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.public))
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await
            .expect("the gateway answers")
    }

    async fn model_ids(&self) -> Vec<String> {
        let res = self
            .client
            .get(format!("{}/v1/models", self.public))
            .bearer_auth(&self.key)
            .send()
            .await
            .expect("the gateway answers");
        let body = ok_json(res, "/v1/models").await;
        body["data"]
            .as_array()
            .expect("a data list")
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect()
    }

    /// The listing, once `done` holds for it.
    async fn listing_until(&self, what: &str, done: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let ids = self.model_ids().await;
            if done(&ids) {
                return ids;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: /v1/models still lists {ids:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// The served ledger row for one request, once the detached write lands:
    /// its model and cost.
    async fn ledger(&self, request_id: Uuid) -> (String, Decimal) {
        let deadline = Instant::now() + WAIT;
        loop {
            let row: Option<(String, Decimal)> = sqlx::query_as(
                "SELECT model_id, cost_usd FROM usage_event \
                 WHERE request_id = $1 AND status = 200 ORDER BY attempt DESC LIMIT 1",
            )
            .bind(request_id)
            .fetch_optional(self.db.pool())
            .await
            .expect("read the ledger");
            if let Some(row) = row {
                return row;
            }
            assert!(Instant::now() < deadline, "no ledger row for {request_id}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// A mock answering one chat request at the endpoint's own path, with its key.
async fn answering(merge: &MockServer, answer: Value) -> MockGuard {
    Mock::given(method("POST"))
        .and(path("/v1/openai/chat/completions"))
        .and(header("authorization", "Bearer t6-merge-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer))
        .expect(1)
        .mount_as_scoped(merge)
        .await
}

async fn only_request(mock: &MockGuard) -> Request {
    let mut received = mock.received_requests().await;
    assert_eq!(received.len(), 1, "{received:?}");
    received.remove(0)
}

fn body_of(sent: &Request) -> Value {
    serde_json::from_slice(&sent.body).expect("a JSON body")
}

fn request_id_of(res: &reqwest::Response) -> Uuid {
    res.headers()
        .get("x-oag-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("every answer names its request")
}

async fn ok_json(res: reqwest::Response, what: &str) -> Value {
    let status = res.status();
    let text = res.text().await.expect("a body");
    assert_eq!(status, 200, "{what}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{what}: {e}: {text}"))
}

fn openai_answer(text: &str) -> Value {
    json!({
        "id": "chatcmpl-t6",
        "object": "chat.completion",
        "created": 1,
        "model": "zai/glm-5.3-flash",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 100_000, "completion_tokens": 50_000, "total_tokens": 150_000}
    })
}

fn free_port() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").expect("a free port")
}

fn port_of(listener: &std::net::TcpListener) -> u16 {
    listener.local_addr().expect("bound").port()
}

/// A database of this run's own on the test server: `(url, name)`.
async fn scratch_database(admin_url: &str) -> (String, String) {
    let (server, _) = admin_url.rsplit_once('/').expect("a database URL");
    let name = format!("oag_t6_{}", Uuid::new_v4().simple());
    let admin = Db::connect(admin_url, 1).expect("connect");
    // The name is a fresh UUID, never input.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(admin.pool())
        .await
        .expect("a scratch database");
    (format!("{server}/{name}"), name)
}

async fn drop_database(admin_url: &str, name: &str) {
    let admin = Db::connect(admin_url, 1).expect("connect");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(admin.pool())
    .await
    .expect("drop the scratch database");
}
