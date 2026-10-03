//! Azure `OpenAI` endpoints, end to end: a real gateway on real ports, with
//! two wiremock upstreams standing in for two Azure resources, one served
//! through Azure's v1 API and one through its deployments API, both
//! registered while the gateway runs.
//!
//! An azure endpoint's base URL must be an Azure resource's, which no test can
//! stand up, so each mock is put in a resource's place with
//! `oag_core::endpoint::stand_in_for_azure`. That exists only in test builds
//! (`oag-core`'s `test-fixtures` feature, which this crate turns on as a
//! dev-dependency), and it admits the one loopback origin it is given.
//!
//! Gated like every test that needs the store: skipped unless
//! `OAG_TEST_DATABASE_URL` and `OAG_TEST_REDIS_URL` are set. It makes a
//! database of its own on that server and drops it afterwards, so its
//! endpoints are the only ones any reload here sees.
//!
//! One test, in steps, alone in its binary. The endpoint registry is
//! process-wide and every reload replaces it whole, so a second test here
//! reloading a database of its own would unregister this one's endpoints
//! between two of its steps.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use futures_util::FutureExt as _;
use oag_core::credential::SecretMaterial;
use oag_server::AppState;
use oag_store::{Cache, Db, NewEndpoint, repo};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockGuard, MockServer, Request, ResponseTemplate};

/// How long a change may take to show. The refresh runs every second; this
/// allows for a loaded machine.
const WAIT: Duration = Duration::from_secs(45);

/// The deployments API version the second endpoint asks for.
const VERSION: &str = "2024-10-21";

/// The second resource's deployment, a name that is not a path segment as it
/// stands: a space and a slash.
const DEPLOYMENT: &str = "prod gpt-4o/eu";
/// Where the deployments API takes it: one segment, percent-encoded.
const DEPLOYMENT_PATH: &str = "/openai/deployments/prod%20gpt-4o%2Feu/chat/completions";

const LADDER: &str = r#"[
    {"name": "cheap", "models": ["azv1/gpt-4o-mini"]},
    {"name": "top", "models": ["azdep/gpt-4o"]}
]"#;

/// Azure's stream as it sends one: the prompt's filter results in a first
/// frame with no choices, each delta with its own, the finish, the usage
/// `stream_options` asked for, and `[DONE]`.
const AZURE_STREAM: &str = concat!(
    r#"data: {"choices":[],"created":0,"id":"","model":"","object":"","prompt_filter_results":[{"prompt_index":0,"content_filter_results":{"hate":{"filtered":false,"severity":"safe"}}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"content_filter_results":{},"delta":{"content":"","refusal":null,"role":"assistant"},"finish_reason":null,"index":0}],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-mini-2024-07-18","object":"chat.completion.chunk"}"#,
    "\n\n",
    r#"data: {"choices":[{"content_filter_results":{"hate":{"filtered":false,"severity":"safe"}},"delta":{"content":"streamed from"},"finish_reason":null,"index":0}],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-mini-2024-07-18","object":"chat.completion.chunk"}"#,
    "\n\n",
    r#"data: {"choices":[{"content_filter_results":{"hate":{"filtered":false,"severity":"safe"}},"delta":{"content":" azure"},"finish_reason":null,"index":0}],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-mini-2024-07-18","object":"chat.completion.chunk"}"#,
    "\n\n",
    r#"data: {"choices":[{"content_filter_results":{},"delta":{},"finish_reason":"stop","index":0}],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-mini-2024-07-18","object":"chat.completion.chunk"}"#,
    "\n\n",
    r#"data: {"choices":[],"created":1,"id":"chatcmpl-t8","model":"gpt-4o-mini-2024-07-18","object":"chat.completion.chunk","usage":{"completion_tokens":5,"prompt_tokens":21,"total_tokens":26}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn azure_endpoints_are_served_end_to_end_in_both_apis() {
    let (Ok(admin_url), Ok(redis_url)) = (
        std::env::var("OAG_TEST_DATABASE_URL"),
        std::env::var("OAG_TEST_REDIS_URL"),
    ) else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
        return;
    };
    let (db_url, name) = scratch_database(&admin_url).await;
    // Caught, so a failed assertion still reaches the drop below instead of
    // leaving the database behind.
    let outcome = std::panic::AssertUnwindSafe(scenario(db_url, redis_url))
        .catch_unwind()
        .await;
    drop_database(&admin_url, &name).await;
    if let Err(failed) = outcome {
        std::panic::resume_unwind(failed);
    }
}

async fn scenario(db_url: String, redis_url: String) {
    let (v1, deployments) = (MockServer::start().await, MockServer::start().await);
    for resource in [&v1, &deployments] {
        oag_core::endpoint::stand_in_for_azure(&resource.uri()).expect("a mock on loopback");
    }
    let gw = Gateway::start(&db_url, &redis_url).await;

    // Everything from here on is written while the gateway serves: nothing
    // restarts it, and only its refresh picks the rows up.
    gw.azure("azv1", &v1.uri(), None).await;
    gw.azure("azdep", &deployments.uri(), Some(VERSION)).await;
    let key_a = gw.account("azv1", "azure-key-a", 0).await;
    let key_b = gw.account("azv1", "azure-key-b", 1).await;
    let key_d = gw.account("azdep", "azure-key-d", 0).await;
    gw.model("azv1/gpt-4o-mini", "azv1", "gpt-4o-mini-prod")
        .await;
    gw.model("azdep/gpt-4o", "azdep", DEPLOYMENT).await;
    sqlx::query("UPDATE route SET tiers = $1::jsonb WHERE id = $2")
        .bind(LADDER)
        .bind(gw.route)
        .execute(gw.db.pool())
        .await
        .expect("the ladder");

    gw.listing_until("both resources' models are listed", |ids| {
        ["azv1/gpt-4o-mini", "azdep/gpt-4o"]
            .iter()
            .all(|id| ids.iter().any(|listed| listed == id))
    })
    .await;

    the_v1_api(&gw, &v1, key_a).await;
    the_deployments_api(&gw, &deployments, key_d).await;
    streamed(&gw, &v1, key_a).await;
    filtered(&gw, &deployments).await;
    prompt_filtered(&gw, &v1).await;
    failover(&gw, &v1, key_b).await;
}

/// No API version: Azure's v1 API, the deployment named in the body, the key
/// in `api-key` and nowhere else.
async fn the_v1_api(gw: &Gateway, v1: &MockServer, key_a: Uuid) {
    let mock = answering(
        v1,
        "/openai/v1/chat/completions",
        "azure-key-a",
        openai_answer("hello from azure v1"),
    )
    .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "azv1/gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let (body, request_id) = answered(res, "the v1 API").await;
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from azure v1"
    );

    let sent = only_request(&mock).await;
    assert_eq!(sent.url.path(), "/openai/v1/chat/completions");
    assert_eq!(sent.url.query(), None, "the v1 API takes no version");
    assert_one_key(&sent);
    assert_eq!(
        header_of(&sent, "x-tenant"),
        Some("t8"),
        "the operator's header"
    );
    let wire = body_of(&sent);
    assert_eq!(wire["model"], "gpt-4o-mini-prod", "the deployment's name");
    assert_eq!(wire["messages"][0]["content"], "hi");

    let (model, cost, account) = gw.ledger(request_id).await;
    assert_eq!(model, "azv1/gpt-4o-mini");
    assert!(cost > Decimal::ZERO, "priced from the catalog row: {cost}");
    assert_eq!(account, Some(key_a));
}

/// An API version: the deployments API, the deployment one percent-encoded
/// path segment, the version the query's one parameter.
async fn the_deployments_api(gw: &Gateway, deployments: &MockServer, key_d: Uuid) {
    let mock = Mock::given(method("POST"))
        .and(path(DEPLOYMENT_PATH))
        .and(query_param("api-version", VERSION))
        .and(header("api-key", "azure-key-d"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_answer("hello from a deployment")),
        )
        .expect(1)
        .mount_as_scoped(deployments)
        .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "azdep/gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let (body, request_id) = answered(res, "the deployments API").await;
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from a deployment"
    );

    let sent = only_request(&mock).await;
    assert_eq!(sent.url.path(), DEPLOYMENT_PATH);
    assert_eq!(
        sent.url.query(),
        Some("api-version=2024-10-21"),
        "the version, and nothing else"
    );
    assert_one_key(&sent);
    assert_eq!(header_of(&sent, "x-tenant"), Some("t8"));
    assert_eq!(body_of(&sent)["model"], DEPLOYMENT, "the body is v1's");

    let (model, _, account) = gw.ledger(request_id).await;
    assert_eq!(model, "azdep/gpt-4o");
    assert_eq!(account, Some(key_d));
}

/// A streamed request to an `OpenAI` client: Azure's stream is relayed byte
/// for byte, and its usage frame is what the ledger bills.
async fn streamed(gw: &Gateway, v1: &MockServer, key_a: Uuid) {
    let mock = Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header("api-key", "azure-key-a"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(AZURE_STREAM, "text/event-stream"))
        .expect(1)
        .mount_as_scoped(v1)
        .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({
                "model": "azv1/gpt-4o-mini",
                "stream": true,
                "messages": [{"role": "user", "content": "stream it"}]
            }),
        )
        .await;
    let (status, request_id) = (res.status(), request_id_of(&res));
    let content_type = header_of_response(&res, "content-type").map(str::to_owned);
    let relayed = res.text().await.expect("the stream");
    assert_eq!(status, 200, "{relayed}");
    let request_id = request_id.expect("every answer names its request");
    assert!(
        content_type.is_some_and(|t| t.starts_with("text/event-stream")),
        "an SSE answer"
    );
    assert_eq!(relayed, AZURE_STREAM, "the same dialect is passed through");

    let wire = body_of(&only_request(&mock).await);
    assert_eq!(wire["stream"], true);
    assert_eq!(
        wire["stream_options"]["include_usage"], true,
        "asked for the usage frame"
    );

    let (model, cost, account) = gw.ledger(request_id).await;
    assert_eq!(model, "azv1/gpt-4o-mini");
    assert!(cost > Decimal::ZERO, "{cost}");
    assert_eq!(account, Some(key_a));
    let (input, output): (i64, i64) = sqlx::query_as(
        "SELECT input_tokens, output_tokens FROM usage_event \
         WHERE request_id = $1 AND status = 200",
    )
    .bind(request_id)
    .fetch_one(gw.db.pool())
    .await
    .expect("the ledger row");
    assert_eq!(
        (input, output),
        (21, 5),
        "the usage Azure's stream reported"
    );
}

/// Azure's content filter stops an answer with `content_filter`, which an
/// Anthropic client, translated, is told as a refusal.
async fn filtered(gw: &Gateway, deployments: &MockServer) {
    let mut answer = openai_answer("I can");
    answer["choices"][0]["finish_reason"] = json!("content_filter");
    answer["choices"][0]["content_filter_results"] =
        json!({"violence": {"filtered": true, "severity": "high"}});
    answer["prompt_filter_results"] = json!([{"prompt_index": 0, "content_filter_results": {}}]);
    let mock = Mock::given(method("POST"))
        .and(path(DEPLOYMENT_PATH))
        .and(query_param("api-version", VERSION))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer))
        .expect(1)
        .mount_as_scoped(deployments)
        .await;
    let res = gw
        .post(
            "/v1/messages",
            &json!({
                "model": "azdep/gpt-4o",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
    let body = ok_json(res, "a filtered answer, translated").await;
    assert_eq!(body["type"], "message");
    assert_eq!(body["stop_reason"], "refusal", "{body}");
    assert_eq!(body["content"][0]["text"], "I can");
    only_request(&mock).await;
}

/// A prompt Azure's content filter rejects is Azure's 400, relayed under
/// `error.upstream`, and is not tried on the endpoint's other key: the same
/// prompt would be filtered there too.
async fn prompt_filtered(gw: &Gateway, v1: &MockServer) {
    let refused = Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "The response was filtered due to the prompt triggering Azure \
                        OpenAI's content management policy.",
            "type": null,
            "param": "prompt",
            "code": "content_filter",
            "status": 400,
            "innererror": {
                "code": "ResponsibleAIPolicyViolation",
                "content_filter_result": {"violence": {"filtered": true, "severity": "high"}}
            }
        }})))
        .expect(1)
        .mount_as_scoped(v1)
        .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "azv1/gpt-4o-mini", "messages": [{"role": "user", "content": "no"}]}),
        )
        .await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.expect("an error body");
    assert_eq!(body["error"]["type"], "upstream_error", "{body}");
    assert_eq!(body["error"]["upstream_status"], 400, "{body}");
    assert_eq!(
        body["error"]["upstream"]["error"]["code"], "content_filter",
        "Azure's own words reach the caller: {body}"
    );
    assert_eq!(
        refused.received_requests().await.len(),
        1,
        "one key, and not the other"
    );
}

/// A 429 on one of the endpoint's keys fails over to the other, once each.
async fn failover(gw: &Gateway, v1: &MockServer, key_b: Uuid) {
    let limited = Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header("api-key", "azure-key-a"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_json(json!({"error": {"code": "429", "message": "Requests have exceeded the call rate limit"}})),
        )
        .expect(1)
        .mount_as_scoped(v1)
        .await;
    let key_b_answers = answering(
        v1,
        "/openai/v1/chat/completions",
        "azure-key-b",
        openai_answer("hello from key b"),
    )
    .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "azv1/gpt-4o-mini", "messages": [{"role": "user", "content": "again"}]}),
        )
        .await;
    let (body, request_id) = answered(res, "failover").await;
    assert_eq!(body["choices"][0]["message"]["content"], "hello from key b");
    assert_eq!(limited.received_requests().await.len(), 1, "key a, once");
    assert_one_key(&only_request(&key_b_answers).await);
    let (_, _, account) = gw.ledger(request_id).await;
    assert_eq!(
        account,
        Some(key_b),
        "and the ledger names the key that answered"
    );
}

/// A gateway on real ports, with a principal, a route and a key of its own.
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
        // Both held until both are known, so they cannot be the same port.
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
  usage_poll_interval: 0
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
        // What `oag serve` does before it listens.
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

        let email = "t8-owner@example.invalid";
        sqlx::query(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'admin')",
        )
        .bind(email)
        .execute(db.pool())
        .await
        .expect("principal");
        // A route needs a rung to parse; this one names nothing in the
        // catalog, and the scenario replaces it.
        let route: Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), 't8', \
             '[{\"name\": \"cheap\", \"models\": [\"anthropic/placeholder\"]}]'::jsonb) \
             RETURNING id",
        )
        .fetch_one(db.pool())
        .await
        .expect("route");
        let key = repo::mint_key(&db, email, "t8", "t8-inference", None)
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

    /// An azure endpoint at `base_url`, in the deployments API at
    /// `api_version` or the v1 API without one, with one header of the
    /// operator's.
    async fn azure(&self, name: &str, base_url: &str, api_version: Option<&str>) {
        repo::insert_endpoint(
            &self.db,
            &NewEndpoint {
                name,
                dialect: "openai",
                platform: "azure",
                base_url: Some(base_url),
                auth: "api_key_header",
                region: None,
                project: None,
                api_version,
                path: None,
                extra_headers: &json!({"x-tenant": "t8"}),
                display_name: None,
                discover_models: false,
            },
        )
        .await
        .expect("an endpoint");
    }

    /// A key filed under `provider`, sealed as `account add` seals one and
    /// joined to the route. Lower `priority` is tried first.
    async fn account(&self, provider: &str, secret: &str, priority: i16) -> Uuid {
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
             credentials_nonce, priority) \
             VALUES (gen_random_uuid(), $1, $2, 'api_key', $3, $4, $5) RETURNING id",
        )
        .bind(secret)
        .bind(provider)
        .bind(&sealed.ciphertext)
        .bind(&sealed.nonce)
        .bind(priority)
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

    async fn model(&self, id: &str, provider: &str, upstream: &str) {
        let row = oag_store::ModelRow {
            id: id.to_owned(),
            provider: provider.to_owned(),
            upstream_name: upstream.to_owned(),
            input_per_mtok: Decimal::from(3),
            output_per_mtok: Decimal::from(15),
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
            context_window: 128_000,
            max_output_tokens: 8_192,
            supports_vision: false,
            supports_tools: false,
            supports_reasoning: false,
            supports_prompt_cache: false,
            display_label: None,
            reasoning_efforts: None,
            reasoning_effort: None,
        };
        repo::upsert_model(&self.db, &row, false)
            .await
            .expect("a catalog row");
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
    async fn listing_until(&self, what: &str, done: impl Fn(&[String]) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            let ids = self.model_ids().await;
            if done(&ids) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: /v1/models still lists {ids:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// The served ledger row for one request, once the detached write lands:
    /// its model, cost and credential.
    async fn ledger(&self, request_id: Uuid) -> (String, Decimal, Option<Uuid>) {
        let deadline = Instant::now() + WAIT;
        loop {
            let row: Option<(String, Decimal, Option<Uuid>)> = sqlx::query_as(
                "SELECT model_id, cost_usd, account_id FROM usage_event \
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

/// A mock on `server` that answers `at` with `answer` for the one request
/// carrying `key` in `api-key`, and expects exactly one.
async fn answering(server: &MockServer, at: &str, key: &str, answer: Value) -> MockGuard {
    Mock::given(method("POST"))
        .and(path(at))
        .and(header("api-key", key))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer))
        .expect(1)
        .mount_as_scoped(server)
        .await
}

/// The one request a mock received.
async fn only_request(mock: &MockGuard) -> Request {
    let mut received = mock.received_requests().await;
    assert_eq!(received.len(), 1, "{received:?}");
    received.remove(0)
}

/// The key rode in `api-key`, once, and in no other header a key can ride in.
fn assert_one_key(sent: &Request) {
    for name in ["authorization", "x-api-key", "x-goog-api-key", "api-key"] {
        assert_eq!(
            sent.headers.get_all(name).iter().count(),
            usize::from(name == "api-key"),
            "{name} on a request whose key belongs in api-key"
        );
    }
}

fn header_of<'a>(sent: &'a Request, name: &str) -> Option<&'a str> {
    sent.headers.get(name).and_then(|v| v.to_str().ok())
}

fn header_of_response<'a>(res: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    res.headers().get(name).and_then(|v| v.to_str().ok())
}

fn body_of(sent: &Request) -> Value {
    serde_json::from_slice(&sent.body).expect("a JSON body")
}

/// The request an answer names. Read before the status is judged, and
/// expected after, so a failure says what came back rather than what header
/// it lacked.
fn request_id_of(res: &reqwest::Response) -> Option<Uuid> {
    res.headers()
        .get("x-oag-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// A 200's JSON body and the request it names.
async fn answered(res: reqwest::Response, what: &str) -> (Value, Uuid) {
    let request_id = request_id_of(&res);
    let body = ok_json(res, what).await;
    (body, request_id.expect("every answer names its request"))
}

async fn ok_json(res: reqwest::Response, what: &str) -> Value {
    let status = res.status();
    let text = res.text().await.expect("a body");
    assert_eq!(status, 200, "{what}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{what}: {e}: {text}"))
}

/// A Chat Completions answer as Azure sends one: `OpenAI`'s, with the
/// filter results beside the choice.
fn openai_answer(text: &str) -> Value {
    json!({
        "id": "chatcmpl-t8",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-4o-2024-11-20",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
            "content_filter_results": {"hate": {"filtered": false, "severity": "safe"}}
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
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
    let name = format!("oag_t8_{}", Uuid::new_v4().simple());
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
