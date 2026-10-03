//! Operator-registered endpoints, end to end: a real gateway on real ports,
//! three wiremock upstreams behind it, and the endpoints, their credentials,
//! their catalog rows and a ladder naming them all registered while it runs.
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
use oag_core::Provider;
use oag_core::credential::SecretMaterial;
use oag_core::provider::{Dialect, Endpoint, Platform};
use oag_server::AppState;
use oag_store::{Cache, Db, EndpointDeletion, EndpointUpdate, EndpointUpdated, NewEndpoint, repo};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockGuard, MockServer, Request, ResponseTemplate};

/// How long a change may take to show. The refresh runs every second; this
/// allows for a loaded machine.
const WAIT: Duration = Duration::from_secs(45);

/// Every rung names one endpoint, and each endpoint speaks another dialect.
const LADDER: &str = r#"[
    {"name": "cheap", "models": ["mockoai/m-oai"]},
    {"name": "mid", "models": ["mockanth/m-anth"]},
    {"name": "top", "models": ["mockgem/m-gem"]}
]"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_endpoints_are_served_end_to_end_and_unserved_when_removed() {
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
    let gw = Gateway::start(&db_url, &redis_url).await;
    let (oai, anth, gem) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );

    // Everything from here on is written while the gateway serves: nothing
    // restarts it, and only its refresh picks the rows up.
    gw.endpoint(
        "mockoai",
        "openai",
        &format!("{}/v1", oai.uri()),
        "bearer",
        &json!({"x-tenant": "t4"}),
    )
    .await;
    gw.endpoint("mockanth", "anthropic", &anth.uri(), "bearer", &json!({}))
        .await;
    gw.endpoint(
        "mockgem",
        "gemini",
        &format!("{}/v1beta", gem.uri()),
        "x_goog_api_key",
        &json!({}),
    )
    .await;
    let key_a = gw.account("mockoai", "oai-key-a", 0).await;
    let key_b = gw.account("mockoai", "oai-key-b", 1).await;
    gw.account("mockanth", "anth-key", 0).await;
    gw.account("mockgem", "gem-key", 0).await;
    for (id, provider, upstream) in [
        ("mockoai/m-oai", "mockoai", "m-oai"),
        ("mockanth/m-anth", "mockanth", "m-anth"),
        ("mockgem/m-gem", "mockgem", "m-gem"),
    ] {
        gw.model(id, provider, upstream).await;
    }
    sqlx::query("UPDATE route SET tiers = $1::jsonb WHERE id = $2")
        .bind(LADDER)
        .bind(gw.route)
        .execute(gw.db.pool())
        .await
        .expect("the ladder");

    gw.listing_until("the three endpoints' models are listed", |ids| {
        ["mockoai/m-oai", "mockanth/m-anth", "mockgem/m-gem"]
            .iter()
            .all(|id| ids.iter().any(|listed| listed == id))
    })
    .await;

    native_openai(&gw, &oai, key_a).await;
    native_anthropic(&gw, &anth).await;
    native_gemini(&gw, &gem).await;
    translated(&gw, &anth).await;
    pinned(&gw, &gem).await;
    matrix(&gw).await;
    failover(&gw, &oai, key_b).await;
    refused_by_the_compliance_guard(&gw, &anth).await;
    removed(&gw).await;
}

/// An OpenAI client, an OpenAI-dialect endpoint: the request lands in its
/// native shape with the endpoint's bearer key and the operator's header, and
/// the ledger prices it.
async fn native_openai(gw: &Gateway, oai: &MockServer, key_a: Uuid) {
    let mock = answering(
        oai,
        "/v1/chat/completions",
        ("authorization", "Bearer oai-key-a"),
        openai_answer("hello from mockoai"),
    )
    .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "mockoai/m-oai", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let request_id = request_id_of(&res);
    let body = ok_json(res, "native openai").await;
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from mockoai"
    );

    let sent = only_request(&mock).await;
    assert_eq!(
        header_of(&sent, "x-tenant"),
        Some("t4"),
        "the operator's header"
    );
    assert_one_key(&sent, "authorization");
    let wire = body_of(&sent);
    assert_eq!(
        wire["model"], "m-oai",
        "the upstream's own name for the model"
    );
    assert_eq!(wire["messages"][0]["role"], "user");

    let (model, cost, account) = gw.ledger(request_id).await;
    assert_eq!(model, "mockoai/m-oai");
    assert!(cost > Decimal::ZERO, "priced from the catalog row: {cost}");
    assert_eq!(account, Some(key_a));
}

/// An Anthropic client, an Anthropic-dialect endpoint registered with bearer
/// auth: the key rides as a bearer, not in `x-api-key`.
async fn native_anthropic(gw: &Gateway, anth: &MockServer) {
    let mock = answering(
        anth,
        "/v1/messages",
        ("authorization", "Bearer anth-key"),
        anthropic_answer("hello from mockanth"),
    )
    .await;
    let res = gw
        .post(
            "/v1/messages",
            &json!({
                "model": "mockanth/m-anth",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
    let body = ok_json(res, "native anthropic").await;
    assert_eq!(body["content"][0]["text"], "hello from mockanth");

    let sent = only_request(&mock).await;
    assert_one_key(&sent, "authorization");
    assert!(header_of(&sent, "anthropic-version").is_some());
    let wire = body_of(&sent);
    assert_eq!(wire["model"], "m-anth");
    assert_eq!(wire["max_tokens"], 64);
}

/// A Gemini client, a Gemini-dialect endpoint: the model goes in the path and
/// the key in `x-goog-api-key`.
async fn native_gemini(gw: &Gateway, gem: &MockServer) {
    let mock = answering(
        gem,
        "/v1beta/models/m-gem:generateContent",
        ("x-goog-api-key", "gem-key"),
        gemini_answer("hello from mockgem"),
    )
    .await;
    let res = gw
        .post(
            "/v1beta/models/mockgem/m-gem:generateContent",
            &json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}),
        )
        .await;
    let body = ok_json(res, "native gemini").await;
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "hello from mockgem"
    );

    let sent = only_request(&mock).await;
    assert_one_key(&sent, "x-goog-api-key");
    assert_eq!(body_of(&sent)["contents"][0]["parts"][0]["text"], "hi");
}

/// An OpenAI client, an Anthropic-dialect endpoint: translated on the way out
/// and back.
async fn translated(gw: &Gateway, anth: &MockServer) {
    let mock = answering(
        anth,
        "/v1/messages",
        ("authorization", "Bearer anth-key"),
        anthropic_answer("translated by mockanth"),
    )
    .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "mockanth/m-anth", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let body = ok_json(res, "openai to anthropic").await;
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "translated by mockanth"
    );

    let wire = body_of(&only_request(&mock).await);
    assert_eq!(wire["model"], "m-anth");
    assert!(
        wire["max_tokens"].is_u64(),
        "the Messages shape, which requires it: {wire}"
    );
    assert!(wire.get("choices").is_none() && wire["messages"].is_array());
}

/// `oag/top` pins the rung whose one model is the Gemini endpoint's.
async fn pinned(gw: &Gateway, gem: &MockServer) {
    let mock = answering(
        gem,
        "/v1beta/models/m-gem:generateContent",
        ("x-goog-api-key", "gem-key"),
        gemini_answer("pinned to mockgem"),
    )
    .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "oag/top", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let body = ok_json(res, "a rung pin").await;
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "pinned to mockgem"
    );
    assert_eq!(body["model"], "mockgem/m-gem");
    only_request(&mock).await;
}

/// The admin matrix lists each endpoint after the built-ins, with an adapter
/// and its credentials counted.
async fn matrix(gw: &Gateway) {
    let res = gw
        .client
        .get(format!("{}/admin/api/providers", gw.admin))
        .bearer_auth(&gw.admin_key)
        .send()
        .await
        .expect("the admin listener answers");
    let rows = ok_json(res, "the provider matrix").await;
    let rows = rows.as_array().expect("a list");
    let row = |name: &str| {
        rows.iter()
            .find(|r| r["provider"] == name)
            .unwrap_or_else(|| panic!("{name} is not in the matrix: {rows:?}"))
    };
    for (name, dialect, accounts) in [
        ("mockoai", "OpenAI Chat Completions", 2),
        ("mockanth", "Anthropic Messages", 1),
        ("mockgem", "Gemini generateContent", 1),
    ] {
        let endpoint = row(name);
        assert_eq!(endpoint["adapter"], true, "{name}");
        assert_eq!(endpoint["dialect"], dialect, "{name}");
        assert_eq!(endpoint["accounts"], accounts, "{name}");
        assert_eq!(endpoint["credential_kinds"], json!(["api_key"]), "{name}");
    }
    assert_eq!(row("anthropic")["adapter"], true, "the built-ins stay");
}

/// A 429 on one of the endpoint's keys fails over to the other, once each.
async fn failover(gw: &Gateway, oai: &MockServer, key_b: Uuid) {
    let limited = Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer oai-key-a"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_json(json!({"error": {"message": "slow down", "type": "rate_limit"}})),
        )
        .expect(1)
        .mount_as_scoped(oai)
        .await;
    let answered = answering(
        oai,
        "/v1/chat/completions",
        ("authorization", "Bearer oai-key-b"),
        openai_answer("hello from key b"),
    )
    .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "mockoai/m-oai", "messages": [{"role": "user", "content": "again"}]}),
        )
        .await;
    let request_id = request_id_of(&res);
    let body = ok_json(res, "failover").await;
    assert_eq!(body["choices"][0]["message"]["content"], "hello from key b");
    assert_eq!(limited.received_requests().await.len(), 1, "key a, once");
    only_request(&answered).await;
    let (_, _, account) = gw.ledger(request_id).await;
    assert_eq!(
        account,
        Some(key_b),
        "and the ledger names the key that answered"
    );
}

/// A row that stops passing is dropped on the next reload with its account
/// and catalog row still in place: nothing is sent to it, the listing loses
/// its model, and the metric says why.
async fn refused_by_the_compliance_guard(gw: &Gateway, anth: &MockServer) {
    let seen = repo::get_endpoint(&gw.db, "mockanth")
        .await
        .expect("read")
        .expect("registered")
        .updated_at;
    let updated = repo::update_endpoint(
        &gw.db,
        "mockanth",
        &EndpointUpdate {
            base_url: Some("https://api.anthropic.com"),
            auth: "bearer",
            region: None,
            project: None,
            api_version: None,
            path: None,
            extra_headers: &json!({}),
            display_name: None,
            discover_models: false,
            seen,
        },
    )
    .await
    .expect("the schema takes it: only the gateway's guard refuses it");
    assert!(
        matches!(updated, EndpointUpdated::Updated(_)),
        "{updated:?}"
    );

    gw.listing_until("mockanth's model is withdrawn", |ids| {
        !ids.iter().any(|id| id == "mockanth/m-anth")
    })
    .await;
    assert!(
        "mockanth".parse::<Provider>().is_err(),
        "no longer resolves"
    );

    let before = anth.received_requests().await.expect("recording").len();
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "mockanth/m-anth", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    // The refusal for a model the gateway does not serve, and no other: any
    // status but 200 would let a 500 from a half-served endpoint pass as one.
    let status = res.status();
    let body: Value = res.json().await.expect("a JSON refusal");
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["type"], "no_viable_model", "{body}");
    // The model is still on the ladder, so the refusal names what is in its
    // way, the endpoint nobody serves, and not the ladder.
    assert!(
        body["error"]["message"].as_str().is_some_and(|m| {
            m.starts_with("'mockanth/m-anth' is on the ladder of route")
                && m.contains("serves no provider named 'mockanth'")
        }),
        "{body}"
    );
    assert_eq!(
        anth.received_requests().await.expect("recording").len(),
        before,
        "and nothing reached the host it now names"
    );

    let metrics = gw
        .client
        .get(format!("{}/metrics", gw.admin))
        .send()
        .await
        .expect("metrics")
        .text()
        .await
        .expect("text");
    assert!(
        metrics.contains(r#"oag_endpoint_invalid_total{reason="compliance"}"#),
        "{metrics}"
    );
}

/// Removal as the store allows it: the endpoint's model and key go first, and
/// then the endpoint, whose name the next reload stops serving.
async fn removed(gw: &Gateway) {
    assert!(
        matches!(
            repo::delete_endpoint(&gw.db, "mockgem").await,
            Ok(EndpointDeletion::InUse {
                accounts: 1,
                models: 1
            })
        ),
        "refused while its key and model name it"
    );
    for table in ["model_catalog", "account"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE provider = 'mockgem'"
        )))
        .execute(gw.db.pool())
        .await
        .expect("remove what names it");
    }
    assert_eq!(
        repo::delete_endpoint(&gw.db, "mockgem")
            .await
            .expect("delete"),
        EndpointDeletion::Deleted
    );

    gw.listing_until("mockgem's model is gone", |ids| {
        !ids.iter().any(|id| id == "mockgem/m-gem")
    })
    .await;
    let deadline = Instant::now() + WAIT;
    while "mockgem".parse::<Provider>().is_ok() {
        assert!(Instant::now() < deadline, "mockgem still resolves");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let gone = Provider::Custom(
        Endpoint::new("mockgem", Dialect::GeminiGenerateContent, Platform::Plain).expect("a name"),
    );
    assert!(gw.state.adapter(gone).is_err(), "and its adapter went too");
    assert!(!gw.state.providers().contains(&gone));
    assert!(
        gw.state
            .providers()
            .contains(&"mockoai".parse().expect("still registered")),
        "the others stay"
    );
}

/// A gateway on real ports, with a principal, a route and two keys of its
/// own: one for inference and one with admin authority.
struct Gateway {
    state: Arc<AppState>,
    db: Db,
    client: reqwest::Client,
    public: String,
    admin: String,
    key: String,
    admin_key: String,
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
        let recorder = oag_server::metrics::install().expect("the one recorder");
        oag_server::metrics::describe();
        state.lifecycle.set_metrics(recorder);
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

        // An admin, so one of its keys can read the admin API as well.
        let email = "t4-owner@example.invalid";
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
            "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), 't4', \
             '[{\"name\": \"cheap\", \"models\": [\"anthropic/placeholder\"]}]'::jsonb) \
             RETURNING id",
        )
        .fetch_one(db.pool())
        .await
        .expect("route");
        let key = repo::mint_key(&db, email, "t4", "t4-inference", None)
            .await
            .expect("mint")
            .expect("the principal and route exist");
        let admin_key = repo::mint_key(&db, email, "t4", "t4-admin", None)
            .await
            .expect("mint")
            .expect("the principal and route exist");
        sqlx::query("UPDATE api_key SET admin = true WHERE id = $1")
            .bind(admin_key.id)
            .execute(db.pool())
            .await
            .expect("admin authority");

        Self {
            state,
            db,
            client,
            public,
            admin: format!("http://127.0.0.1:{admin_port}"),
            key: key.key,
            admin_key: admin_key.key,
            route,
        }
    }

    async fn endpoint(
        &self,
        name: &str,
        dialect: &str,
        base_url: &str,
        auth: &str,
        extra_headers: &Value,
    ) {
        repo::insert_endpoint(
            &self.db,
            &NewEndpoint {
                name,
                dialect,
                platform: "plain",
                base_url: Some(base_url),
                auth,
                region: None,
                project: None,
                api_version: None,
                path: None,
                extra_headers,
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

/// A mock on `server` that answers `path` with `answer` for the one request
/// carrying `key`, and expects exactly one.
async fn answering(
    server: &MockServer,
    at: &str,
    (name, value): (&'static str, &'static str),
    answer: Value,
) -> MockGuard {
    Mock::given(method("POST"))
        .and(path(at))
        .and(header(name, value))
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

/// Exactly one header carried a key: `carrier`. No key rode anywhere else.
fn assert_one_key(sent: &Request, carrier: &str) {
    for name in ["authorization", "x-api-key", "x-goog-api-key", "api-key"] {
        assert_eq!(
            sent.headers.get_all(name).iter().count(),
            usize::from(name == carrier),
            "{name} on a request whose key belongs in {carrier}"
        );
    }
}

fn header_of<'a>(sent: &'a Request, name: &str) -> Option<&'a str> {
    sent.headers.get(name).and_then(|v| v.to_str().ok())
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
        "id": "chatcmpl-t4",
        "object": "chat.completion",
        "created": 1,
        "model": "m-oai",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    })
}

fn anthropic_answer(text: &str) -> Value {
    json!({
        "id": "msg_t4",
        "type": "message",
        "role": "assistant",
        "model": "m-anth",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 12, "output_tokens": 8}
    })
}

fn gemini_answer(text: &str) -> Value {
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": text}]},
            "finishReason": "STOP",
            "index": 0
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 6, "totalTokenCount": 16},
        "modelVersion": "m-gem"
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
    let name = format!("oag_t4_{}", Uuid::new_v4().simple());
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
