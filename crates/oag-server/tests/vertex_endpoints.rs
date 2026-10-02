//! Vertex endpoints, end to end: a real gateway on real ports, a stand-in
//! Google token endpoint at `gateway.gcp_token_url`, and a stand-in Vertex
//! behind two endpoints on the `gcp` platform, registered while the gateway
//! runs. `vertex-gem` serves Gemini in `us-central1` with one key;
//! `vertex-claude` serves Claude in the `global` region with two, and Google
//! refuses to mint for the first.
//!
//! Every key is a service-account key around the committed test-only RSA key
//! (`crates/oag-upstream/tests/fixtures`), which has never been attached to a
//! Google account. No request leaves the machine.
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

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::FutureExt as _;
use oag_core::credential::SecretMaterial;
use oag_server::AppState;
use oag_store::{Cache, Db, NewEndpoint, repo};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Match, Mock, MockGuard, MockServer, Request, ResponseTemplate};

/// How long a change may take to show. The refresh runs every second; this
/// allows for a loaded machine.
const WAIT: Duration = Duration::from_secs(45);

/// Vertex's names for the two models: Claude's carries its version after an
/// `@`, which goes in the path as `%40`.
const GEMINI: &str = "gemini-2.5-flash";
const CLAUDE: &str = "claude-sonnet-4-5@20250929";

/// Where each is sent, beneath the stand-in Vertex.
const GEMINI_AT: &str =
    "/v1/projects/oag-test/locations/us-central1/publishers/google/models/gemini-2.5-flash";
const CLAUDE_AT: &str = "/v1/projects/oag-test/locations/global/publishers/anthropic/models/claude-sonnet-4-5%4020250929";

/// The service accounts behind the three keys, and the tokens Google grants
/// the two it honours.
const GEM_ACCOUNT: &str = "t10-gem@oag-test.invalid";
const CLAUDE_ACCOUNT: &str = "t10-claude@oag-test.invalid";
const REVOKED_ACCOUNT: &str = "t10-revoked@oag-test.invalid";
const GEM_TOKEN: &str = "ya29.t10-gem";
const CLAUDE_TOKEN: &str = "ya29.t10-claude";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vertex_endpoints_are_served_with_tokens_minted_from_their_keys() {
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
    let (google, vertex) = (MockServer::start().await, MockServer::start().await);
    // Each account's grant is answered on its own: one token each for the two
    // Google honours, minted once for the whole scenario, and a refusal for
    // the third, as Google refuses a key it no longer knows.
    for (account, token) in [(GEM_ACCOUNT, GEM_TOKEN), (CLAUDE_ACCOUNT, CLAUDE_TOKEN)] {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(IssuedBy(account))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": token,
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .expect(1)
            .mount(&google)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(IssuedBy(REVOKED_ACCOUNT))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "Invalid JWT Signature.",
        })))
        .mount(&google)
        .await;

    let gw = Gateway::start(&db_url, &redis_url, &format!("{}/token", google.uri())).await;

    // Everything from here on is written while the gateway serves: nothing
    // restarts it, and only its refresh picks the rows up.
    gw.endpoint("vertex-gem", "gemini", "us-central1", &vertex.uri())
        .await;
    gw.endpoint("vertex-claude", "anthropic", "global", &vertex.uri())
        .await;
    let gem_key = gw.account("vertex-gem", GEM_ACCOUNT, 0).await;
    let revoked_key = gw.account("vertex-claude", REVOKED_ACCOUNT, 0).await;
    let claude_key = gw.account("vertex-claude", CLAUDE_ACCOUNT, 1).await;
    gw.model("vertex-gem/flash", "vertex-gem", GEMINI).await;
    gw.model("vertex-claude/sonnet", "vertex-claude", CLAUDE)
        .await;

    gw.listing_until("both endpoints' models are listed", |ids| {
        ["vertex-gem/flash", "vertex-claude/sonnet"]
            .iter()
            .all(|id| ids.iter().any(|listed| listed == id))
    })
    .await;

    gemini_for_an_openai_client(&gw, &google, &vertex, gem_key).await;
    gemini_streamed_on_the_same_token(&gw, &google, &vertex).await;
    claude_past_a_key_that_cannot_mint(&gw, &google, &vertex, claude_key, revoked_key).await;
    claude_streamed(&gw, &google, &vertex).await;
    no_key_and_no_token_reaches_a_client(&gw, &google, claude_key).await;

    google.verify().await;
}

/// A Chat Completions client asks for the Gemini model. Google is sent a
/// grant signed by the endpoint's key, and Vertex is sent the token it
/// granted, as a bearer, at the model's exact path; the client gets Vertex's
/// answer as a Chat Completions one, and the ledger prices it.
async fn gemini_for_an_openai_client(
    gw: &Gateway,
    google: &MockServer,
    vertex: &MockServer,
    gem_key: Uuid,
) {
    let mock = answering(
        vertex,
        &format!("{GEMINI_AT}:generateContent"),
        GEM_TOKEN,
        ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hello from Vertex."}]},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 5, "totalTokenCount": 14},
            "modelVersion": GEMINI
        })),
    )
    .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "vertex-gem/flash",
                    "messages": [{"role": "user", "content": "Say hello."}]}),
        )
        .await;
    let request_id = request_id_of(&res);
    let (body, raw) = ok_json(res, "Gemini on Vertex for an OpenAI client").await;
    assert_eq!(body["object"], "chat.completion", "{body}");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from Vertex."
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 9);
    assert_eq!(body["usage"]["completion_tokens"], 5);
    assert_holds_no_secret(&raw);

    let grants = grants(google).await;
    let [grant] = grants.as_slice() else {
        panic!("one grant so far, not {grants:?}");
    };
    assert_eq!(
        grant["grant_type"],
        "urn:ietf:params:oauth:grant-type:jwt-bearer"
    );
    let claims = claims(&grant["assertion"]);
    assert_eq!(claims["iss"], GEM_ACCOUNT);
    assert_eq!(
        claims["aud"], "https://oauth2.googleapis.com/token",
        "addressed to Google's token endpoint, as Google requires, though it was posted to \
         the configured stand-in and not to the key's token_uri"
    );

    let sent = only_request(&mock).await;
    assert_one_bearer(&sent, GEM_TOKEN);
    let wire = body_of(&sent);
    assert_eq!(wire["contents"][0]["parts"][0]["text"], "Say hello.");
    assert!(
        wire.get("model").is_none(),
        "the model is in the path: {wire}"
    );

    let (model, cost, account) = gw.ledger(request_id).await;
    assert_eq!(model, "vertex-gem/flash");
    assert!(cost > Decimal::ZERO, "priced from the catalog row: {cost}");
    assert_eq!(account, Some(gem_key));
}

/// The same client streams: `streamGenerateContent?alt=sse` is sent the token
/// already minted, with no second grant, and the client is sent Chat
/// Completions chunks.
async fn gemini_streamed_on_the_same_token(gw: &Gateway, google: &MockServer, vertex: &MockServer) {
    let stream = [
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Streamed "}]},
                               "index": 0}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "hello."}]},
                               "finishReason": "STOP", "index": 0}],
               "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 4,
                                 "totalTokenCount": 13}}),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\r\n\r\n"))
    .collect::<Vec<_>>()
    .concat();
    let mock = Mock::given(method("POST"))
        .and(path(format!("{GEMINI_AT}:streamGenerateContent")))
        .and(query_param("alt", "sse"))
        .and(header(
            "authorization",
            format!("Bearer {GEM_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .expect(1)
        .mount_as_scoped(vertex)
        .await;
    let res = gw
        .post(
            "/v1/chat/completions",
            &json!({"model": "vertex-gem/flash", "stream": true,
                    "messages": [{"role": "user", "content": "Stream hello."}]}),
        )
        .await;
    let status = res.status();
    let sent = res.text().await.expect("a body");
    assert_eq!(status, 200, "{sent}");
    let text: String = data_frames(&sent)
        .iter()
        .filter_map(|c| {
            c["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(text, "Streamed hello.", "{sent}");
    assert!(sent.trim_end().ends_with("data: [DONE]"), "{sent}");
    assert_holds_no_secret(&sent);

    assert_one_bearer(&only_request(&mock).await, GEM_TOKEN);
    assert_eq!(
        issuers(google).await,
        [GEM_ACCOUNT],
        "the token minted for the first request is the one sent with the second"
    );
}

/// An Anthropic client asks for Claude. The first key's grant is refused, so
/// the request moves to the second key, whose token reaches `rawPredict` at a
/// path whose model holds `@` as `%40`; the body names no model and carries
/// Vertex's version, and Vertex's answer reaches the client as it was sent.
async fn claude_past_a_key_that_cannot_mint(
    gw: &Gateway,
    google: &MockServer,
    vertex: &MockServer,
    claude_key: Uuid,
    revoked_key: Uuid,
) {
    let mock = answering(
        vertex,
        &format!("{CLAUDE_AT}:rawPredict"),
        CLAUDE_TOKEN,
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_vrtx_t10",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5-20250929",
            "content": [{"type": "text", "text": "Hello from Claude on Vertex."}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 12, "output_tokens": 8}
        })),
    )
    .await;
    let res = gw
        .post(
            "/v1/messages",
            &json!({
                "model": "vertex-claude/sonnet",
                "max_tokens": 64,
                "system": "Answer briefly.",
                "messages": [{"role": "user", "content": "Say hello."}]
            }),
        )
        .await;
    let request_id = request_id_of(&res);
    let (body, raw) = ok_json(res, "Claude on Vertex for an Anthropic client").await;
    assert_eq!(body["content"][0]["text"], "Hello from Claude on Vertex.");
    assert_eq!(body["id"], "msg_vrtx_t10", "Vertex's own answer");
    assert_holds_no_secret(&raw);

    assert_eq!(
        issuers(google).await,
        [GEM_ACCOUNT, REVOKED_ACCOUNT, CLAUDE_ACCOUNT],
        "the first key's grant was refused, and the second key's was granted"
    );
    let sent = only_request(&mock).await;
    assert_one_bearer(&sent, CLAUDE_TOKEN);
    assert!(
        header_of(&sent, "anthropic-version").is_none(),
        "the body names the version"
    );
    let wire = body_of(&sent);
    assert_eq!(wire["anthropic_version"], "vertex-2023-10-16");
    assert!(
        wire.get("model").is_none(),
        "the model is in the path: {wire}"
    );
    assert_eq!(wire["max_tokens"], 64);
    assert_eq!(wire["stream"], false);
    assert_eq!(wire["system"][0]["text"], "Answer briefly.");
    assert_eq!(wire["messages"][0]["content"][0]["text"], "Say hello.");

    let (model, cost, account) = gw.ledger(request_id).await;
    assert_eq!(model, "vertex-claude/sonnet");
    assert!(cost > Decimal::ZERO, "{cost}");
    assert_eq!(
        account,
        Some(claude_key),
        "the ledger names the key that answered, not {revoked_key}"
    );
}

/// The Anthropic client streams: `streamRawPredict` is sent the second key's
/// token, minted already, and its server-sent events reach the client.
async fn claude_streamed(gw: &Gateway, google: &MockServer, vertex: &MockServer) {
    let events = [
        (
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_vrtx_t10s", "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-5-20250929", "content": [],
                "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 12, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Streamed "}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "from Vertex."}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta",
                   "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                   "usage": {"output_tokens": 6}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ];
    let stream = events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect::<Vec<_>>()
        .concat();
    let grants_before = issuers(google).await.len();
    let mock = Mock::given(method("POST"))
        .and(path(format!("{CLAUDE_AT}:streamRawPredict")))
        .and(header(
            "authorization",
            format!("Bearer {CLAUDE_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .expect(1)
        .mount_as_scoped(vertex)
        .await;
    let res = gw
        .post(
            "/v1/messages",
            &json!({"model": "vertex-claude/sonnet", "max_tokens": 64, "stream": true,
                    "messages": [{"role": "user", "content": "Stream hello."}]}),
        )
        .await;
    let status = res.status();
    let sent = res.text().await.expect("a body");
    assert_eq!(status, 200, "{sent}");
    let frames = data_frames(&sent);
    let text: String = frames
        .iter()
        .filter(|f| f["type"] == "content_block_delta")
        .filter_map(|f| f["delta"]["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(text, "Streamed from Vertex.", "{sent}");
    assert_eq!(
        frames.first().map(|f| f["type"].clone()),
        Some(json!("message_start"))
    );
    assert_eq!(
        frames.last().map(|f| f["type"].clone()),
        Some(json!("message_stop"))
    );
    assert_holds_no_secret(&sent);

    let request = only_request(&mock).await;
    assert_one_bearer(&request, CLAUDE_TOKEN);
    assert_eq!(body_of(&request)["stream"], true);
    assert!(
        !issuers(google).await[grants_before..]
            .iter()
            .any(|issuer| issuer == CLAUDE_ACCOUNT),
        "the second key's token was reused, not minted again"
    );
}

/// With the key that can mint taken out of service, the request has only the
/// one Google refuses. The client is told 500 `internal_error`, as for any
/// credential that fails to refresh, and nothing it is told holds a key, an
/// assertion or a token.
async fn no_key_and_no_token_reaches_a_client(gw: &Gateway, google: &MockServer, claude_key: Uuid) {
    sqlx::query("UPDATE account SET schedulable = false WHERE id = $1")
        .bind(claude_key)
        .execute(gw.db.pool())
        .await
        .expect("disable the key that mints");
    let before = issuers(google).await;
    let res = gw
        .post(
            "/v1/messages",
            &json!({"model": "vertex-claude/sonnet", "max_tokens": 64,
                    "messages": [{"role": "user", "content": "Anyone there?"}]}),
        )
        .await;
    let status = res.status();
    let headers = format!("{:?}", res.headers());
    let raw = res.text().await.expect("a body");
    assert_eq!(status, 500, "{raw}");
    let body: Value = serde_json::from_str(&raw).expect("a JSON error");
    assert_eq!(body["error"]["type"], "internal_error", "{raw}");
    assert_holds_no_secret(&raw);
    assert_holds_no_secret(&headers);

    let after = issuers(google).await;
    assert_eq!(
        &after[before.len()..],
        [REVOKED_ACCOUNT],
        "the one key left was tried, once"
    );
}

/// Matches a token grant whose assertion `email` issued.
struct IssuedBy(&'static str);

impl Match for IssuedBy {
    fn matches(&self, request: &Request) -> bool {
        form(request)
            .get("assertion")
            .map(|assertion| claims(assertion))
            .is_some_and(|claims| claims["iss"] == self.0)
    }
}

/// The form a token grant posts.
fn form(request: &Request) -> HashMap<String, String> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

/// The claims of a JWT, or `null` if it has none to read.
fn claims(jwt: &str) -> Value {
    jwt.split('.')
        .nth(1)
        .and_then(|claims| URL_SAFE_NO_PAD.decode(claims).ok())
        .and_then(|claims| serde_json::from_slice(&claims).ok())
        .unwrap_or(Value::Null)
}

/// Every grant the stand-in Google has received, as forms.
async fn grants(google: &MockServer) -> Vec<HashMap<String, String>> {
    google
        .received_requests()
        .await
        .expect("recording is on")
        .iter()
        .map(form)
        .collect()
}

/// Who issued each grant received so far, in order.
async fn issuers(google: &MockServer) -> Vec<String> {
    grants(google)
        .await
        .iter()
        .map(|grant| {
            claims(&grant["assertion"])["iss"]
                .as_str()
                .expect("an issuer")
                .to_owned()
        })
        .collect()
}

/// `shown` holds no part of a service-account key, no signed assertion and
/// no access token.
fn assert_holds_no_secret(shown: &str) {
    for secret in ["ya29.", "BEGIN PRIVATE KEY", "private_key", "eyJhbGci"] {
        assert!(!shown.contains(secret), "{secret} in: {shown}");
    }
    for line in oag_upstream::gcp_token::TEST_KEY_PEM
        .lines()
        .filter(|line| !line.starts_with("-----"))
    {
        assert!(!shown.contains(line), "a line of a key in: {shown}");
    }
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
    async fn start(db_url: &str, redis_url: &str, token_url: &str) -> Self {
        // Both held until both are known, so they cannot be the same port.
        let (public, admin) = (free_port(), free_port());
        let (public_port, admin_port) = (port_of(&public), port_of(&admin));
        drop((public, admin));
        // Every built-in on a closed port, so nothing this gateway could send
        // leaves the machine, and Google's token endpoint is the stand-in's.
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
  gcp_token_url: "{token_url}"
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

        let email = "t10-owner@example.invalid";
        sqlx::query(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')",
        )
        .bind(email)
        .execute(db.pool())
        .await
        .expect("principal");
        // A route needs a rung to parse; the models here are pinned by name.
        let route: Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), 't10', \
             '[{\"name\": \"cheap\", \"models\": [\"vertex-gem/flash\"]}, \
               {\"name\": \"top\", \"models\": [\"vertex-claude/sonnet\"]}]'::jsonb) \
             RETURNING id",
        )
        .fetch_one(db.pool())
        .await
        .expect("route");
        let key = repo::mint_key(&db, email, "t10", "t10-inference", None)
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

    /// An endpoint on the gcp platform, as `oag admin endpoint add` would
    /// write one: a bearer token, project `oag-test`, and a base URL that
    /// stands in for the region's own host.
    async fn endpoint(&self, name: &str, dialect: &str, region: &str, base_url: &str) {
        repo::insert_endpoint(
            &self.db,
            &NewEndpoint {
                name,
                dialect,
                platform: "gcp",
                base_url: Some(base_url),
                auth: "bearer",
                region: Some(region),
                project: Some("oag-test"),
                api_version: None,
                path: None,
                extra_headers: &json!({}),
                display_name: None,
                discover_models: false,
            },
        )
        .await
        .expect("an endpoint");
    }

    /// A service account's key filed under `provider`, sealed as `account
    /// add` seals one, as kind `service_account`, and joined to the route.
    /// Lower `priority` is tried first.
    async fn account(&self, provider: &str, email: &str, priority: i16) -> Uuid {
        let key = json!({
            "type": "service_account",
            "project_id": "oag-test",
            "private_key_id": "0123456789abcdef0123456789abcdef01234567",
            "private_key": oag_upstream::gcp_token::TEST_KEY_PEM,
            "client_email": email,
            "client_id": "100000000000000000001",
            // Never followed: the configured token URL is where grants go.
            "token_uri": "http://127.0.0.1:1/token",
        });
        let sealed = self
            .state
            .kek
            .seal_json(&SecretMaterial {
                access_token: serde_json::to_string_pretty(&key).expect("JSON"),
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
             VALUES (gen_random_uuid(), $1, $2, 'service_account', $3, $4, $5) RETURNING id",
        )
        .bind(email)
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
        let (body, _) = ok_json(res, "/v1/models").await;
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
/// carrying `token` as its bearer, and expects exactly one.
async fn answering(
    server: &MockServer,
    at: &str,
    token: &str,
    answer: ResponseTemplate,
) -> MockGuard {
    Mock::given(method("POST"))
        .and(path(at))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(answer)
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

/// `sent` carries `token` as its one bearer, and no key rides anywhere else.
fn assert_one_bearer(sent: &Request, token: &str) {
    let bearers: Vec<_> = sent.headers.get_all("authorization").iter().collect();
    assert_eq!(bearers, [format!("Bearer {token}").as_str()]);
    for key in ["x-api-key", "x-goog-api-key", "api-key"] {
        assert!(header_of(sent, key).is_none(), "{key}");
    }
    let seen = format!("{:?}{}", sent.headers, String::from_utf8_lossy(&sent.body));
    for line in oag_upstream::gcp_token::TEST_KEY_PEM
        .lines()
        .filter(|line| !line.starts_with("-----"))
    {
        assert!(!seen.contains(line), "a key reached Vertex");
    }
}

/// The JSON payloads of a server-sent event stream, `[DONE]` and keepalives
/// left out.
fn data_frames(sent: &str) -> Vec<Value> {
    sent.lines()
        .filter_map(|line| line.strip_prefix("data: "))
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

/// A 200's body, parsed and as it arrived.
async fn ok_json(res: reqwest::Response, what: &str) -> (Value, String) {
    let status = res.status();
    let text = res.text().await.expect("a body");
    assert_eq!(status, 200, "{what}: {text}");
    let body = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{what}: {e}: {text}"));
    (body, text)
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
    let name = format!("oag_t10_{}", Uuid::new_v4().simple());
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
