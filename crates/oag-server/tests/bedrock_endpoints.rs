//! Bedrock endpoints, end to end: a real gateway on real ports, and two
//! stand-in Bedrocks behind it, registered while it runs as endpoints on the
//! `aws` platform in two regions. `bedrock-paris` speaks Converse (a Llama
//! model) in `eu-west-3`; `bedrock-seoul` serves Claude through `InvokeModel`
//! in `ap-northeast-2`. The gateway's own `bedrock_region` is `sa-east-1`, the
//! built-in provider's, and no request here may be signed for it.
//!
//! Every key is an obviously fake `access_key:secret` pair: no request leaves
//! the machine, and the signature is checked here instead, recomputed over
//! what each stand-in actually received.
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
use oag_upstream::sigv4;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wiremock::matchers::{header_regex, method, path};
use wiremock::{Mock, MockGuard, MockServer, Request, ResponseTemplate};

/// How long a change may take to show. The refresh runs every second; this
/// allows for a loaded machine.
const WAIT: Duration = Duration::from_secs(45);

/// Bedrock's ids for the two models, colon and all.
const LLAMA: &str = "meta.llama3-1-70b-instruct-v1:0";
const CLAUDE: &str = "anthropic.claude-sonnet-4-v1:0";

/// The keys, packed as Bedrock's are: `access_key:secret`. Not keys at all.
const PARIS_A: (&str, &str) = ("TESTACCESSKEYPARISA", "TESTSECRETPARISA");
const PARIS_B: (&str, &str) = ("TESTACCESSKEYPARISB", "TESTSECRETPARISB");
const SEOUL: (&str, &str) = ("TESTACCESSKEYSEOUL", "TESTSECRETSEOUL");

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bedrock_endpoints_are_served_in_their_own_regions_end_to_end() {
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
    let (paris, seoul) = (MockServer::start().await, MockServer::start().await);

    // Everything from here on is written while the gateway serves: nothing
    // restarts it, and only its refresh picks the rows up.
    gw.endpoint(
        "bedrock-paris",
        "bedrock_converse",
        "eu-west-3",
        &paris.uri(),
        &json!({"x-team": "t9"}),
    )
    .await;
    gw.endpoint(
        "bedrock-seoul",
        "anthropic",
        "ap-northeast-2",
        &seoul.uri(),
        &json!({}),
    )
    .await;
    let key_a = gw.account("bedrock-paris", PARIS_A, 0).await;
    let key_b = gw.account("bedrock-paris", PARIS_B, 1).await;
    gw.account("bedrock-seoul", SEOUL, 0).await;
    gw.model("bedrock-paris/llama", "bedrock-paris", LLAMA)
        .await;
    gw.model("bedrock-seoul/claude", "bedrock-seoul", CLAUDE)
        .await;

    gw.listing_until("both endpoints' models are listed", |ids| {
        ["bedrock-paris/llama", "bedrock-seoul/claude"]
            .iter()
            .all(|id| ids.iter().any(|listed| listed == id))
    })
    .await;

    openai_client_over_converse(&gw, &paris, &seoul, key_a).await;
    anthropic_client_over_converse(&gw, &paris).await;
    claude_in_another_region(&gw, &paris, &seoul).await;
    streamed_over_converse(&gw, &paris).await;
    throttled_key_fails_over(&gw, &paris, key_b).await;
    exception_mid_stream(&gw, &paris).await;
}

/// A Chat Completions client asks for the Llama model: the request reaches
/// only the Paris stand-in, at `/model/{id}/converse`, in Converse's shape and
/// signed for `eu-west-3` with the first key; Converse's answer reaches the
/// client as a Chat Completions one; and the ledger prices it.
async fn openai_client_over_converse(
    gw: &Gateway,
    paris: &MockServer,
    seoul: &MockServer,
    key_a: Uuid,
) {
    let mock = answering(
        paris,
        &format!("/model/{LLAMA}/converse"),
        PARIS_A.0,
        converse_answer(),
    )
    .await;
    let seoul_before = seen(seoul).await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({
                "model": "bedrock-paris/llama",
                "max_tokens": 300,
                "temperature": 0.5,
                "stop": ["END"],
                "messages": [
                    {"role": "system", "content": "Answer briefly."},
                    {"role": "user", "content": "What is the top song on WZPZ?"}
                ],
                "tools": [{"type": "function", "function": {
                    "name": "top_song",
                    "description": "The most popular song on a station.",
                    "parameters": {"type": "object",
                                   "properties": {"sign": {"type": "string"}}}
                }}]
            }),
        )
        .await;
    let request_id = request_id_of(&res);
    let body = ok_json(res, "an OpenAI client over Converse").await;
    assert_eq!(body["object"], "chat.completion", "{body}");
    let message = &body["choices"][0]["message"];
    assert_eq!(message["content"], "Let me look that up.", "{body}");
    assert_eq!(
        message["tool_calls"][0]["id"],
        "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q"
    );
    assert_eq!(message["tool_calls"][0]["function"]["name"], "top_song");
    assert_eq!(
        serde_json::from_str::<Value>(
            message["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .expect("arguments are a string")
        )
        .expect("JSON arguments"),
        json!({"sign": "WZPZ"})
    );
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["usage"]["prompt_tokens"], 30);
    assert_eq!(body["usage"]["completion_tokens"], 62);

    let sent = only_request(&mock).await;
    assert_signed_for(&sent, PARIS_A, "eu-west-3");
    assert_eq!(
        header_of(&sent, "x-team"),
        Some("t9"),
        "the operator's header"
    );
    assert_eq!(
        body_of(&sent),
        json!({
            "messages": [
                {"role": "user", "content": [{"text": "What is the top song on WZPZ?"}]}
            ],
            "system": [{"text": "Answer briefly."}],
            "inferenceConfig": {"maxTokens": 300, "temperature": 0.5, "stopSequences": ["END"]},
            "toolConfig": {"tools": [{"toolSpec": {
                "name": "top_song",
                "description": "The most popular song on a station.",
                "inputSchema": {"json": {"type": "object",
                                         "properties": {"sign": {"type": "string"}}}}
            }}]}
        }),
        "Converse's shape, with the model in the path and not the body"
    );
    assert_eq!(
        seen(seoul).await,
        seoul_before,
        "the other region's endpoint is sent nothing"
    );

    let (model, cost, account) = gw.ledger(request_id).await;
    assert_eq!(model, "bedrock-paris/llama");
    assert!(cost > Decimal::ZERO, "priced from the catalog row: {cost}");
    assert_eq!(account, Some(key_a));
}

/// An Anthropic client asks the same model: the same Converse request, and
/// the answer comes back as an Anthropic message.
async fn anthropic_client_over_converse(gw: &Gateway, paris: &MockServer) {
    let mock = answering(
        paris,
        &format!("/model/{LLAMA}/converse"),
        PARIS_A.0,
        converse_answer(),
    )
    .await;
    let res = gw
        .post(
            "/v1/messages",
            &json!({
                "model": "bedrock-paris/llama",
                "max_tokens": 200,
                "system": "Answer briefly.",
                "messages": [{"role": "user", "content": "What is the top song on WZPZ?"}],
                "tools": [{"name": "top_song", "input_schema": {"type": "object"}}]
            }),
        )
        .await;
    let body = ok_json(res, "an Anthropic client over Converse").await;
    assert_eq!(body["type"], "message", "{body}");
    assert_eq!(
        body["content"][0],
        json!({"type": "text", "text": "Let me look that up."})
    );
    assert_eq!(
        body["content"][1],
        json!({"type": "tool_use", "id": "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q",
               "name": "top_song", "input": {"sign": "WZPZ"}})
    );
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(body["usage"]["input_tokens"], 30);
    assert_eq!(body["usage"]["output_tokens"], 62);

    let sent = only_request(&mock).await;
    assert_signed_for(&sent, PARIS_A, "eu-west-3");
    let wire = body_of(&sent);
    assert_eq!(wire["system"], json!([{"text": "Answer briefly."}]));
    assert_eq!(wire["inferenceConfig"], json!({"maxTokens": 200}));
    assert_eq!(
        wire["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "top_song"
    );
}

/// Claude on the Seoul endpoint, through `InvokeModel` as the built-in does:
/// Anthropic's body less its model, Bedrock's version marker, signed for
/// `ap-northeast-2` — and Paris is sent nothing.
async fn claude_in_another_region(gw: &Gateway, paris: &MockServer, seoul: &MockServer) {
    let mock = answering(
        seoul,
        &format!("/model/{CLAUDE}/invoke"),
        SEOUL.0,
        json!({
            "id": "msg_t9",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4",
            "content": [{"type": "text", "text": "Annyeong from Seoul."}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 12, "output_tokens": 8}
        }),
    )
    .await;
    let paris_before = seen(paris).await;
    let res = gw
        .post(
            "/v1/messages",
            &json!({
                "model": "bedrock-seoul/claude",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
    let body = ok_json(res, "Claude through InvokeModel").await;
    assert_eq!(body["content"][0]["text"], "Annyeong from Seoul.");

    let sent = only_request(&mock).await;
    assert_signed_for(&sent, SEOUL, "ap-northeast-2");
    let wire = body_of(&sent);
    assert_eq!(wire["anthropic_version"], "bedrock-2023-05-31");
    assert!(
        wire.get("model").is_none(),
        "the model is in the path: {wire}"
    );
    assert_eq!(wire["max_tokens"], 64);
    assert_eq!(seen(paris).await, paris_before, "Paris is sent nothing");
}

/// A Chat Completions client streams: the stand-in answers
/// `/converse-stream` with AWS event-stream bytes, and the client is sent its
/// own chunks — the text, the tool call opened and then its arguments a
/// fragment at a time, and the bill on the chunk that ends the answer, before
/// `[DONE]`.
async fn streamed_over_converse(gw: &Gateway, paris: &MockServer) {
    let mock = Mock::given(method("POST"))
        .and(path(format!("/model/{LLAMA}/converse-stream")))
        .and(signed_by(PARIS_A.0))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(converse_stream(), "application/vnd.amazon.eventstream"),
        )
        .expect(1)
        .mount_as_scoped(paris)
        .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({
                "model": "bedrock-paris/llama",
                "stream": true,
                "messages": [{"role": "user", "content": "What is the top song on WZPZ?"}],
                "tools": [{"type": "function", "function": {
                    "name": "top_song", "parameters": {"type": "object"}}}]
            }),
        )
        .await;
    let status = res.status();
    let sent = res.text().await.expect("a body");
    assert_eq!(status, 200, "{sent}");

    let chunks = chat_chunks(&sent);
    let delta = |c: &Value| c["choices"][0]["delta"].clone();
    let text: String = chunks
        .iter()
        .filter_map(|c| delta(c)["content"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(text, "Let me look that up.", "{sent}");
    let opened = chunks
        .iter()
        .find(|c| delta(c)["tool_calls"][0]["id"] == "tooluse_1")
        .unwrap_or_else(|| panic!("the call opens with its id: {sent}"));
    assert_eq!(
        delta(opened)["tool_calls"][0]["function"]["name"],
        "top_song"
    );
    let fragments: Vec<String> = chunks
        .iter()
        .filter_map(|c| {
            delta(c)["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .filter(|a| !a.is_empty())
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(fragments, ["{\"si", "gn\": \"WZ", "PZ\"}"], "{sent}");

    let last = chunks.last().expect("chunks");
    assert_eq!(last["choices"][0]["finish_reason"], "tool_calls", "{sent}");
    assert_eq!(last["usage"]["prompt_tokens"], 412, "{sent}");
    assert_eq!(last["usage"]["completion_tokens"], 58, "{sent}");
    assert!(sent.trim_end().ends_with("data: [DONE]"), "{sent}");

    let request = only_request(&mock).await;
    assert_signed_for(&request, PARIS_A, "eu-west-3");
    assert!(
        body_of(&request).get("stream").is_none(),
        "streaming is the path's to say"
    );
}

/// The first key is throttled — 429 and `ThrottlingException`, as Bedrock
/// answers — so the request moves to the second key, and the ledger names the
/// key that answered.
async fn throttled_key_fails_over(gw: &Gateway, paris: &MockServer, key_b: Uuid) {
    let throttled = Mock::given(method("POST"))
        .and(path(format!("/model/{LLAMA}/converse")))
        .and(signed_by(PARIS_A.0))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header(
                    "x-amzn-errortype",
                    "ThrottlingException:http://internal.amazon.com/coral/com.amazon.bedrock/",
                )
                .set_body_json(
                    json!({"message": "Too many requests, please wait before trying again."}),
                ),
        )
        .expect(1)
        .mount_as_scoped(paris)
        .await;
    let answered = answering(
        paris,
        &format!("/model/{LLAMA}/converse"),
        PARIS_B.0,
        converse_answer(),
    )
    .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "bedrock-paris/llama",
                    "messages": [{"role": "user", "content": "again"}]}),
        )
        .await;
    let request_id = request_id_of(&res);
    let body = ok_json(res, "failover past a throttled key").await;
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Let me look that up."
    );
    assert_eq!(throttled.received_requests().await.len(), 1, "key A, once");
    let sent = only_request(&answered).await;
    assert_signed_for(&sent, PARIS_B, "eu-west-3");
    let (_, _, account) = gw.ledger(request_id).await;
    assert_eq!(
        account,
        Some(key_b),
        "the ledger names the key that answered"
    );
}

/// An exception inside a 200 stream: the client has its text and then an
/// error that names the exception, and no `[DONE]` that would call the answer
/// whole.
async fn exception_mid_stream(gw: &Gateway, paris: &MockServer) {
    let mut stream = event("messageStart", &json!({"role": "assistant"}));
    stream.extend(event(
        "contentBlockDelta",
        &json!({"contentBlockIndex": 0, "delta": {"text": "Half an "}}),
    ));
    stream.extend(
        oag_upstream::eventstream::encode(
            &[
                (":exception-type", "throttlingException"),
                (":content-type", "application/json"),
                (":message-type", "exception"),
            ],
            br#"{"message":"Too many tokens, please wait before trying again."}"#,
        )
        .expect("a short message"),
    );
    // Whichever key the throttling left in rotation.
    let mock = Mock::given(method("POST"))
        .and(path(format!("/model/{LLAMA}/converse-stream")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(stream, "application/vnd.amazon.eventstream"),
        )
        .expect(1)
        .mount_as_scoped(paris)
        .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "bedrock-paris/llama", "stream": true,
                    "messages": [{"role": "user", "content": "go"}]}),
        )
        .await;
    let sent = res.text().await.expect("a body");
    only_request(&mock).await;

    let chunks = chat_chunks(&sent);
    let text: String = chunks
        .iter()
        .filter_map(|c| {
            c["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(text, "Half an ", "{sent}");
    let errors: Vec<&Value> = chunks.iter().filter(|c| c.get("error").is_some()).collect();
    assert_eq!(errors.len(), 1, "one failure, one frame: {sent}");
    assert!(
        errors[0]["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("throttlingException: Too many tokens")),
        "the exception's own name and words: {sent}"
    );
    assert!(!sent.contains("[DONE]"), "{sent}");
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
        // Every built-in on a closed port, so nothing this gateway could send
        // leaves the machine. The built-in Bedrock's region is a third one
        // that no endpoint here names: a request signed for it was signed for
        // the gateway's region rather than its endpoint's.
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
  bedrock_region: "sa-east-1"
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

        let email = "t9-owner@example.invalid";
        sqlx::query(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')",
        )
        .bind(email)
        .execute(db.pool())
        .await
        .expect("principal");
        // A route needs a rung to parse; the models here are pinned by name.
        let route: Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), 't9', \
             '[{\"name\": \"cheap\", \"models\": [\"bedrock-paris/llama\"]}, \
               {\"name\": \"top\", \"models\": [\"bedrock-seoul/claude\"]}]'::jsonb) \
             RETURNING id",
        )
        .fetch_one(db.pool())
        .await
        .expect("route");
        let key = repo::mint_key(&db, email, "t9", "t9-inference", None)
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

    /// An endpoint on the aws platform, as `oag admin endpoint add` would
    /// write one: no key header, since `SigV4` signs instead, and a base URL
    /// that stands in for the region's own host.
    async fn endpoint(
        &self,
        name: &str,
        dialect: &str,
        region: &str,
        base_url: &str,
        extra_headers: &Value,
    ) {
        repo::insert_endpoint(
            &self.db,
            &NewEndpoint {
                name,
                dialect,
                platform: "aws",
                base_url: Some(base_url),
                auth: "none",
                region: Some(region),
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

    /// A Bedrock key filed under `provider`, sealed as `account add` seals
    /// one — kind `bedrock`, packed `access_key:secret` — and joined to the
    /// route. Lower `priority` is tried first.
    async fn account(&self, provider: &str, (access, secret): (&str, &str), priority: i16) -> Uuid {
        let sealed = self
            .state
            .kek
            .seal_json(&SecretMaterial {
                access_token: format!("{access}:{secret}"),
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
             VALUES (gen_random_uuid(), $1, $2, 'bedrock', $3, $4, $5) RETURNING id",
        )
        .bind(access)
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
            supports_tools: true,
            supports_reasoning: false,
            supports_prompt_cache: false,
            display_label: None,
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

/// Matches a request signed with the access key `access`.
fn signed_by(access: &str) -> wiremock::matchers::HeaderRegexMatcher {
    header_regex(
        "authorization",
        &format!("^AWS4-HMAC-SHA256 Credential={access}/"),
    )
}

/// A mock on `server` that answers `at` with `answer` for the one request
/// signed with `access`, and expects exactly one.
async fn answering(server: &MockServer, at: &str, access: &str, answer: Value) -> MockGuard {
    Mock::given(method("POST"))
        .and(path(at))
        .and(signed_by(access))
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

/// How many requests `server` has received so far.
async fn seen(server: &MockServer) -> usize {
    server.received_requests().await.expect("recording").len()
}

/// `sent` carries a `SigV4` signature with `key`, scoped to `region` and
/// Bedrock, that holds over what actually arrived — and no key header.
///
/// Recomputed here from the path, host, date and body the stand-in received,
/// with the key's secret: what AWS does with a request before it answers one.
fn assert_signed_for(sent: &Request, (access, secret): (&str, &str), region: &str) {
    let header = |name: &str| {
        header_of(sent, name)
            .unwrap_or_else(|| panic!("no {name} header"))
            .to_owned()
    };
    let authorization = header("authorization");
    let date = header("x-amz-date");
    assert!(
        authorization.starts_with(&format!(
            "AWS4-HMAC-SHA256 Credential={access}/{}/{region}/bedrock/aws4_request,",
            &date[..8]
        )),
        "scoped to the endpoint's region and Bedrock: {authorization}"
    );
    let at = time::PrimitiveDateTime::parse(
        &date,
        &time::macros::format_description!("[year][month][day]T[hour][minute][second]Z"),
    )
    .expect("an AWS date")
    .assume_utc();
    let recomputed = sigv4::sign(
        &sigv4::Credentials {
            access_key_id: access.to_owned(),
            secret_access_key: secret.to_owned(),
            session_token: None,
        },
        region,
        "bedrock",
        sigv4::SigningRequest {
            method: "POST",
            path: sent.url.path(),
            host: &header("host"),
            body: &sent.body,
        },
        at,
    );
    assert_eq!(
        authorization, recomputed.authorization,
        "the signature holds"
    );
    for key in ["x-api-key", "x-goog-api-key", "api-key"] {
        assert!(header_of(sent, key).is_none(), "{key}");
    }
}

/// What Converse answers: a few words, a tool call, its stop and its bill.
/// The documented sample, as the codec's own tests have it.
fn converse_answer() -> Value {
    json!({
        "output": {"message": {"role": "assistant", "content": [
            {"text": "Let me look that up."},
            {"toolUse": {"toolUseId": "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q",
                         "name": "top_song", "input": {"sign": "WZPZ"}}}
        ]}},
        "stopReason": "tool_use",
        "usage": {"inputTokens": 30, "outputTokens": 62, "totalTokens": 92},
        "metrics": {"latencyMs": 1275}
    })
}

/// One `ConverseStream` event, framed as Bedrock frames it.
fn event(event_type: &str, payload: &Value) -> Vec<u8> {
    oag_upstream::eventstream::encode(
        &[
            (":event-type", event_type),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ],
        payload.to_string().as_bytes(),
    )
    .expect("a short message")
}

/// The same answer streamed, in the order the user guide draws: the text, a
/// tool call whose input arrives in three fragments, the stop, and last the
/// bill. `p` is the padding AWS puts in events.
fn converse_stream() -> Vec<u8> {
    [
        event(
            "messageStart",
            &json!({"role": "assistant", "p": "abcdefgh"}),
        ),
        event(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"text": "Let me look "}, "p": "ab"}),
        ),
        event(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"text": "that up."}}),
        ),
        event("contentBlockStop", &json!({"contentBlockIndex": 0})),
        event(
            "contentBlockStart",
            &json!({"contentBlockIndex": 1,
                    "start": {"toolUse": {"toolUseId": "tooluse_1", "name": "top_song"}}}),
        ),
        event(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"si"}}}),
        ),
        event(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "gn\": \"WZ"}}}),
        ),
        event(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "PZ\"}"}}}),
        ),
        event("contentBlockStop", &json!({"contentBlockIndex": 1})),
        event("messageStop", &json!({"stopReason": "tool_use"})),
        event(
            "metadata",
            &json!({"usage": {"inputTokens": 412, "outputTokens": 58, "totalTokens": 470},
                    "metrics": {"latencyMs": 930}}),
        ),
    ]
    .concat()
}

/// The JSON chunks of a Chat Completions stream, `[DONE]` and keepalives left
/// out.
fn chat_chunks(sent: &str) -> Vec<Value> {
    sent.split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .filter(|payload| *payload != "[DONE]")
        .map(|payload| serde_json::from_str(payload).unwrap_or_else(|e| panic!("{e}: {payload}")))
        .collect()
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

fn free_port() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").expect("a free port")
}

fn port_of(listener: &std::net::TcpListener) -> u16 {
    listener.local_addr().expect("bound").port()
}

/// A database of this run's own on the test server: `(url, name)`.
async fn scratch_database(admin_url: &str) -> (String, String) {
    let (server, _) = admin_url.rsplit_once('/').expect("a database URL");
    let name = format!("oag_t9_{}", Uuid::new_v4().simple());
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
