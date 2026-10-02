//! System One end to end: a real gateway, a mock Jev behind it, and the real
//! `typesafe_sdk::Client` in front of it.
//!
//! Gated like every test that needs the store: they skip unless
//! `OAG_TEST_DATABASE_URL` and `OAG_TEST_REDIS_URL` are set. What is left
//! without them is the pure half at the bottom of this file.
//!
//! The mock answers with the SDK's own wire types, so it is a server of the
//! same contract the client speaks — the SDK used in the direction a gateway
//! uses it. It records every call it is sent and every byte it answers with,
//! which is what lets these tests say which credential was used and that an
//! answer came back unchanged.

use super::*;
use axum::http::HeaderMap;
use std::collections::HashMap;
use std::sync::Mutex;
use typesafe_sdk::wire::{
    Answer, ChoiceAnswer, ModelMetadata, NoulAnswer, Question, ScoreAnswer, Usage,
};

/// What the mock Jev does with a request made with one key.
#[derive(Debug, Clone, Copy)]
enum Behaviour {
    /// Answer every question.
    Answer,
    /// Answer every question, naming this model as the one that answered.
    AnswerAs(&'static str),
    /// Refuse with this status and a JSON error body.
    Refuse(u16),
    /// Refuse with 429 and this `Retry-After`, in seconds.
    Throttle(u64),
    /// A 200 whose body is not a System One response.
    NotAnAnswer,
    /// Accept the connection and say nothing for longer than the gateway
    /// waits for headers.
    Silent,
}

#[derive(Debug, Clone)]
struct Call {
    key: String,
    path: String,
    body: bytes::Bytes,
}

#[derive(Default)]
struct Seen {
    behaviour: HashMap<String, Behaviour>,
    calls: Vec<Call>,
    /// Every body the mock answered with, byte for byte.
    sent: Vec<bytes::Bytes>,
}

#[derive(Clone, Default)]
struct Mock(Arc<Mutex<Seen>>);

impl Mock {
    fn behave(&self, key: &str, behaviour: Behaviour) {
        self.lock().behaviour.insert(key.to_owned(), behaviour);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.0
            .lock()
            .expect("no test thread panicked holding the mock")
    }

    fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// How many requests arrived with this key.
    fn calls_with(&self, key: &str) -> usize {
        self.lock().calls.iter().filter(|c| c.key == key).count()
    }

    fn sent(&self) -> Vec<bytes::Bytes> {
        self.lock().sent.clone()
    }

    fn record(&self, headers: &HeaderMap, path: &str, body: bytes::Bytes) -> Behaviour {
        let key = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_owned();
        let mut seen = self.lock();
        let behaviour = seen
            .behaviour
            .get(&key)
            .copied()
            .unwrap_or(Behaviour::Answer);
        seen.calls.push(Call {
            key,
            path: path.to_owned(),
            body,
        });
        behaviour
    }

    fn router(&self) -> axum::Router {
        axum::Router::new()
            .route("/v1/systemone", axum::routing::post(mock_system_one))
            .route("/v1/models", axum::routing::get(mock_models))
            .with_state(self.clone())
    }
}

/// One plausible answer per question, of the type the question asks for.
fn answer_for(question: &Question) -> Answer {
    match question {
        Question::Choice { criteria, .. } => {
            let labels: Vec<&String> = criteria.keys().collect();
            let others = u32::try_from(labels.len().max(2) - 1).expect("small");
            let rest = 0.2 / f64::from(others);
            Answer::Choice(ChoiceAnswer::new(
                labels.first().map_or("none", |l| l.as_str()),
                0.8,
                labels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| ((*l).clone(), if i == 0 { 0.8 } else { rest })),
            ))
        }
        Question::Score { criteria, .. } => {
            let last = u32::try_from(criteria.len().saturating_sub(1)).expect("small");
            Answer::Score(ScoreAnswer::new(
                f64::from(last),
                0.7,
                criteria
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (u32::try_from(i).expect("small"), c.clone())),
                (0..=last).map(|i| (i, if i == last { 0.7 } else { 0.1 })),
            ))
        }
        _ => Answer::Noul(NoulAnswer::new(0.97)),
    }
}

async fn mock_system_one(
    axum::extract::State(mock): axum::extract::State<Mock>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let refusal = |status: u16, extra: Option<(&'static str, String)>| {
        let mut response = (
            StatusCode::from_u16(status).expect("a status"),
            [(header::CONTENT_TYPE, "application/json")],
            format!(r#"{{"detail":"mock Jev refused with {status}"}}"#),
        )
            .into_response();
        if let Some((name, value)) = extra {
            response
                .headers_mut()
                .insert(name, value.parse().expect("a header value"));
        }
        response
    };
    match mock.record(&headers, "/v1/systemone", body.clone()) {
        Behaviour::Refuse(status) => refusal(status, None),
        Behaviour::Throttle(secs) => refusal(429, Some(("retry-after", secs.to_string()))),
        Behaviour::NotAnAnswer => (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"not":"an answer"}"#,
        )
            .into_response(),
        Behaviour::Silent => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            StatusCode::OK.into_response()
        }
        behaviour @ (Behaviour::Answer | Behaviour::AnswerAs(_)) => {
            // The server side of the contract, with the SDK's types: parse the
            // question set, answer it, serialise the response.
            let request: SystemOneRequest =
                serde_json::from_slice(&body).expect("the gateway forwards only valid requests");
            let answers: Vec<(String, Answer)> = request
                .questions
                .iter()
                .map(|(name, question)| (name.clone(), answer_for(question)))
                .collect();
            let model = match behaviour {
                Behaviour::AnswerAs(model) => model,
                _ => request.model.as_deref().unwrap_or("jev-latest"),
            };
            let response = SystemOneResponse::new(model, Usage::new(Some(42), Some(3)), answers);
            let bytes = pretty(&response);
            let n = {
                let mut seen = mock.lock();
                seen.sent.push(bytes.clone());
                seen.sent.len()
            };
            (
                [
                    (header::CONTENT_TYPE, "application/json".to_owned()),
                    (
                        header::HeaderName::from_static(REQUEST_ID_HEADER),
                        format!("req-{n}"),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
    }
}

async fn mock_models(
    axum::extract::State(mock): axum::extract::State<Mock>,
    headers: HeaderMap,
) -> Response {
    if let Behaviour::Refuse(status) = mock.record(&headers, "/v1/models", bytes::Bytes::new()) {
        return StatusCode::from_u16(status)
            .expect("a status")
            .into_response();
    }
    let listing =
        ListModelsResponse::new([ModelMetadata::new("jev-latest", "Fast model", "2026-08-01")]);
    let bytes = pretty(&listing);
    mock.lock().sent.push(bytes.clone());
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::HeaderName::from_static(REQUEST_ID_HEADER),
                "models-1",
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Valid JSON that no serialiser would write: indented, with a trailing
/// newline. A gateway that parsed the answer and wrote it out again would send
/// different bytes, which is how the pass-through assertions can fail.
fn pretty(value: &impl serde::Serialize) -> bytes::Bytes {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialises");
    bytes.push(b'\n');
    bytes::Bytes::from(bytes)
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

/// A gateway on a real port, in front of the mock, with one route, one
/// principal and one key of its own.
struct Gateway {
    state: Arc<AppState>,
    db: oag_store::Db,
    url: String,
    key: String,
    key_id: uuid::Uuid,
    route: uuid::Uuid,
    route_name: String,
    principal: uuid::Uuid,
    mock: Mock,
}

/// A ladder chat requests can route over, with nothing of Jev's on it.
const CHAT_LADDER: &str = r#"[{"name":"cheap","models":["anthropic/claude-haiku-4.5"]}]"#;

impl Gateway {
    /// `None` when the backends are not configured; the caller skips.
    async fn start(ladder: &str, quota: Option<rust_decimal::Decimal>) -> Option<Self> {
        let (Ok(db_url), Ok(redis_url)) = (
            std::env::var("OAG_TEST_DATABASE_URL"),
            std::env::var("OAG_TEST_REDIS_URL"),
        ) else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
            return None;
        };
        let mock = Mock::default();
        let jev = serve(mock.router()).await;
        // One same-credential retry and a one-second headers deadline, so the
        // retry and the silence below are measured in fractions of the
        // defaults rather than in minutes.
        let config = oag_core::config::Config::from_yaml(&crate::testing::config_yaml(
            &db_url,
            &redis_url,
            &format!(
                "gateway:\n  same_account_retries: 1\n  upstream_response_timeout: 1\n  \
                 provider_base_urls:\n    jev: \"{jev}/\"\n"
            ),
        ))
        .expect("test config");
        let db = oag_store::Db::connect(&config.database.url, 4).expect("pool");
        db.migrate().await.expect("migrate");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("client");
        let state = Arc::new(AppState::new(config, db.clone(), cache).expect("state"));

        let tag = uuid::Uuid::new_v4();
        let email = format!("system-one-{tag}@example.invalid");
        let route_name = format!("system-one-{tag}");
        let principal: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member') \
             RETURNING id",
        )
        .bind(&email)
        .fetch_one(db.pool())
        .await
        .expect("principal");
        let route: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, $2::jsonb) \
             RETURNING id",
        )
        .bind(&route_name)
        .bind(ladder)
        .fetch_one(db.pool())
        .await
        .expect("route");
        let minted = oag_store::repo::mint_key(&db, &email, &route_name, "system-one", quota)
            .await
            .expect("mint")
            .expect("the principal and route exist");

        let url = serve(crate::public_router(Arc::clone(&state))).await;
        Some(Self {
            state,
            db,
            url,
            key: minted.key,
            key_id: minted.id,
            route,
            route_name,
            principal,
            mock,
        })
    }

    /// A Jev key on this gateway's route, sealed the way `account add` seals
    /// one. Lower `priority` is tried first, which is how a test knows which
    /// credential a request reaches before the other.
    async fn add_jev_key(&self, secret: &str, priority: i16) -> uuid::Uuid {
        self.add_account(secret, priority, true, None).await
    }

    async fn add_account(
        &self,
        secret: &str,
        priority: i16,
        schedulable: bool,
        proxy: Option<&str>,
    ) -> uuid::Uuid {
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
        let account: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
             credentials_nonce, priority, schedulable, proxy_url) \
             VALUES (gen_random_uuid(), $1, 'jev', 'api_key', $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(format!("{secret}-{}", uuid::Uuid::new_v4()))
        .bind(&sealed.ciphertext)
        .bind(&sealed.nonce)
        .bind(priority)
        .bind(schedulable)
        .bind(proxy)
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

    async fn post(&self, path: &str, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}{path}", self.url))
            .bearer_auth(&self.key)
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .expect("the gateway answers")
    }

    async fn ask(&self, body: &str) -> reqwest::Response {
        self.post("/jev/v1/systemone", body).await
    }

    fn sdk(&self) -> typesafe_sdk::Client {
        typesafe_sdk::Client::builder()
            .api_key(&self.key)
            .base_url(format!("{}/jev", self.url))
            // The gateway does the retrying; a client retrying on top of it
            // would hide which of the two answered.
            .retry(typesafe_sdk::RetryPolicy::disabled())
            .build()
            .expect("client")
    }

    /// The ledger row for one request, once the detached write has landed.
    async fn ledger(&self, request_id: uuid::Uuid) -> Option<LedgerRow> {
        for _ in 0..100 {
            let row: Option<LedgerRow> = sqlx::query_as(
                "SELECT model_id, tier, selection_reason, input_tokens, output_tokens, \
                 cost_usd, counterfactual_usd, account_id, attempt, status, streamed, \
                 api_key_id, route_id, principal_id \
                 FROM usage_event WHERE request_id = $1",
            )
            .bind(request_id)
            .fetch_optional(self.db.pool())
            .await
            .expect("read the ledger");
            if row.is_some() {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    /// Every ledger row this gateway's key has, for when the caller holds no
    /// request id — the SDK does not surface `x-oag-request-id`.
    async fn rows_for_key(&self) -> Vec<LedgerRow> {
        sqlx::query_as(
            "SELECT model_id, tier, selection_reason, input_tokens, output_tokens, \
             cost_usd, counterfactual_usd, account_id, attempt, status, streamed, \
             api_key_id, route_id, principal_id \
             FROM usage_event WHERE api_key_id = $1",
        )
        .bind(self.key_id)
        .fetch_all(self.db.pool())
        .await
        .expect("read the ledger")
    }

    async fn benched(
        &self,
        account: uuid::Uuid,
    ) -> (Option<time::OffsetDateTime>, Option<time::OffsetDateTime>) {
        sqlx::query_as("SELECT cooldown_until, rate_limited_until FROM account WHERE id = $1")
            .bind(account)
            .fetch_one(self.db.pool())
            .await
            .expect("the account")
    }
}

#[derive(Debug, sqlx::FromRow)]
struct LedgerRow {
    model_id: String,
    tier: String,
    selection_reason: String,
    input_tokens: i64,
    output_tokens: i64,
    cost_usd: rust_decimal::Decimal,
    counterfactual_usd: rust_decimal::Decimal,
    account_id: Option<uuid::Uuid>,
    attempt: i16,
    status: i16,
    streamed: bool,
    api_key_id: Option<uuid::Uuid>,
    route_id: Option<uuid::Uuid>,
    principal_id: Option<uuid::Uuid>,
}

const QUESTIONS: &str = r#"{"state":{"document":"I was charged twice. Please fix this ASAP."},"model":"jev-latest","questions":{"billing":{"type":"noul","instructions":"Is this ticket about billing?"},"tone":{"type":"choice","instructions":"What is the customer's tone?","criteria":{"calm":null,"frustrated":null,"angry":null}},"urgency":{"type":"score","instructions":"How urgent is this ticket?","criteria":["can wait","this week","today"]}},"trace":"extra keys ride along"}"#;

fn header_of<'a>(response: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    response.headers().get(name).and_then(|v| v.to_str().ok())
}

fn request_id_of(response: &reqwest::Response) -> uuid::Uuid {
    header_of(response, "x-oag-request-id")
        .and_then(|id| id.parse().ok())
        .expect("every answer names its request")
}

async fn json_of(response: reqwest::Response) -> serde_json::Value {
    response.json().await.expect("a JSON body")
}

/// The strongest check there is: the owner's SDK, unmodified, pointed at
/// `<gateway>/jev`, gets typed answers with their confidence — and the ledger
/// has the row.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_sdk_gets_typed_answers_through_the_gateway() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    let account = gw.add_jev_key("jev-key-a", 0).await;
    let client = gw.sdk();

    let response = client
        .system_one(
            serde_json::json!({"document": "I was charged twice. Please fix this ASAP."}),
            [
                (
                    "billing",
                    typesafe_sdk::Question::noul("Is this ticket about billing?"),
                ),
                (
                    "tone",
                    typesafe_sdk::Question::choice(
                        "What is the customer's tone?",
                        [("calm", None), ("frustrated", None), ("angry", None)],
                    ),
                ),
                (
                    "urgency",
                    typesafe_sdk::Question::score(
                        "How urgent is this ticket?",
                        ["can wait", "this week", "today"],
                    ),
                ),
            ],
        )
        .await
        .expect("the gateway answers the SDK");

    assert!((response.noul("billing").expect("noul").noul - 0.97).abs() < f64::EPSILON);
    let tone = response.choice("tone").expect("choice");
    assert_eq!(tone.choice, "calm");
    assert!((tone.confidence - 0.8).abs() < f64::EPSILON);
    assert_eq!(tone.probabilities.len(), 3);
    let urgency = response.score("urgency").expect("score");
    assert!((urgency.score - 2.0).abs() < f64::EPSILON);
    assert!((urgency.confidence - 0.7).abs() < f64::EPSILON);
    assert_eq!(response.usage.input_tokens, Some(42));
    assert_eq!(
        response
            .request_id()
            .expect("Jev's request id reaches the caller"),
        "req-1"
    );
    assert_eq!(
        response.raw_body(),
        &gw.mock.sent()[0][..],
        "the SDK decoded Jev's own bytes, not a re-serialisation"
    );

    let listing = client.models().await.expect("the listing too");
    assert_eq!(listing.models.len(), 1);
    assert_eq!(listing.models[0].name, "jev-latest");

    // The ledger: provider jev by id, the model that answered, the tokens Jev
    // reported, the credential that served. Written off the request's future,
    // so waited for.
    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = gw.rows_for_key().await;
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        rows.len(),
        1,
        "one answer, one row — and none for the listing"
    );
    let row = &rows[0];
    assert_eq!(row.model_id, "jev/jev-latest");
    assert_eq!((row.input_tokens, row.output_tokens), (42, 3));
    assert_eq!(row.account_id, Some(account));
    assert_eq!(row.api_key_id, Some(gw.key_id));
    assert_eq!(row.route_id, Some(gw.route));
    assert_eq!(row.principal_id, Some(gw.principal));
    assert_eq!(row.status, 200);
    assert_eq!(row.tier, "", "a System One model sits on no rung");
    assert_eq!(row.selection_reason, "passthrough");
    assert!(!row.streamed);
    assert_eq!(row.attempt, 0);
}

/// Jev's bytes come back as Jev sent them, beside this gateway's identity; the
/// caller's bytes go up as the caller sent them, with Jev's key and not the
/// caller's.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_passes_through_byte_for_byte_with_the_gateway_s_headers() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header_of(&response, "content-type"),
        Some("application/json")
    );
    assert_eq!(header_of(&response, REQUEST_ID_HEADER), Some("req-1"));
    assert_eq!(header_of(&response, "x-oag-model"), Some("jev/jev-latest"));
    assert_eq!(
        header_of(&response, crate::BUILD_HEADER),
        crate::build_id().to_str().ok()
    );
    let request_id = request_id_of(&response);
    let body = response.bytes().await.expect("body");
    assert_eq!(body, gw.mock.sent()[0], "the answer is Jev's, unchanged");

    let calls = gw.mock.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/v1/systemone");
    assert_eq!(calls[0].key, "jev-key-a", "Jev's key, never the caller's");
    assert_eq!(
        calls[0].body,
        QUESTIONS.as_bytes(),
        "the question set is the caller's, unchanged — extra keys included"
    );

    let row = gw.ledger(request_id).await.expect("the answer is metered");
    assert_eq!(row.model_id, "jev/jev-latest");
    // Unpriced: nothing in this gateway's catalog prices it, so the row costs
    // nothing and claims no saving.
    assert_eq!(row.cost_usd, rust_decimal::Decimal::ZERO);
    assert_eq!(row.counterfactual_usd, row.cost_usd);
}

/// A model name is Jev's to choose, and not every string is a header value.
/// One that is not loses `x-oag-model`, never the answer: it was given and
/// billed either way, and the ledger still names it.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_naming_a_model_no_header_can_carry_still_arrives() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;
    gw.mock
        .behave("jev-key-a", Behaviour::AnswerAs("jev\nlatest"));

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    assert_eq!(header_of(&response, "x-oag-model"), None);
    assert!(header_of(&response, REQUEST_ID_HEADER).is_some());
    let request_id = request_id_of(&response);
    assert_eq!(response.bytes().await.expect("body"), gw.mock.sent()[0]);
    let row = gw.ledger(request_id).await.expect("metered");
    assert_eq!(row.model_id, "jev/jev\nlatest");
}

/// Priced, the row costs what the catalog says — against System One's own
/// catalog, which the chat catalog never sees.
#[tokio::test(flavor = "multi_thread")]
async fn a_priced_jev_model_costs_what_its_catalog_row_says_and_claims_no_saving() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;
    gw.state
        .set_catalog([ModelSpec {
            id: ModelId::new("jev/jev-latest"),
            provider: Provider::Jev,
            upstream_name: "jev-latest".to_owned(),
            pricing: Pricing {
                input_per_mtok: rust_decimal::Decimal::ONE,
                output_per_mtok: rust_decimal::Decimal::TWO,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 0,
            max_output_tokens: 0,
            capabilities: Capabilities::default(),
            display_label: None,
        }])
        .await;

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    let row = gw.ledger(request_id_of(&response)).await.expect("metered");
    // 42 input at $1/Mtok and 3 output at $2/Mtok.
    assert_eq!(row.cost_usd, rust_decimal::Decimal::new(48, 6));
    assert_eq!(
        row.counterfactual_usd, row.cost_usd,
        "no chat model is a baseline for an answer only Jev can give"
    );
}

/// No Jev credential on the route: a refusal that says so, before any
/// upstream call, in the envelope the SDK reads its message from.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_without_jev_refuses_plainly_and_the_sdk_reads_why() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 503);
    let body = json_of(response).await;
    assert_eq!(body["error"]["type"], "system_one_not_configured");
    let message = body["error"]["message"].as_str().expect("a message");
    assert!(
        message.contains("System One is not configured on this route")
            && message.contains(&gw.route_name),
        "{message}"
    );

    let listing = gw.post_get("/jev/v1/models").await;
    assert_eq!(
        listing.status(),
        503,
        "the listing has nothing to list from"
    );

    let err = gw
        .sdk()
        .system_one("x", [("q", typesafe_sdk::Question::noul("?"))])
        .await
        .expect_err("refused");
    let api = err.api().expect("an API error, not a transport one");
    assert_eq!(api.status, 503);
    assert!(
        err.to_string()
            .contains("System One is not configured on this route"),
        "the SDK reads error.message from this gateway's envelope: {err}"
    );
    assert!(gw.mock.calls().is_empty(), "nothing reached Jev");
}

/// A Jev key that exists and cannot serve is not "not configured": the route
/// has one, and the refusal must not send the operator to add another.
#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_jev_key_is_no_credential_not_unconfigured() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_account("jev-key-off", 0, false, None).await;

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 503);
    assert_eq!(json_of(response).await["error"]["type"], "no_credential");
    assert!(gw.mock.calls().is_empty());
}

/// Whatever the SDK's own decoder refuses, the gateway refuses — as a 400, in
/// its envelope, before a credential is leased or Jev is asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_a_system_one_request_is_refused_before_jev_is_asked() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;

    for (body, says) in [
        ("[]", "expected"),
        ("123", "expected"),
        ("not json", "expected"),
        (r#"{"questions":{"q":{"type":"noul"}}}"#, "state"),
        (r#"{"state":"x","questions":{}}"#, "At least one question"),
        (
            r#"{"state":"x","questions":{"q":{"type":"score","criteria":[]}}}"#,
            "",
        ),
        (r#"{"state":1,"questions":{"q":{"type":"noul"}}}"#, ""),
    ] {
        let response = gw.ask(body).await;
        assert_eq!(response.status(), 400, "{body}");
        let refusal = json_of(response).await;
        assert_eq!(refusal["error"]["type"], "invalid_request", "{body}");
        let message = refusal["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(says), "{body}: {message}");
    }

    // Through the SDK, whose own checks let an empty question set through
    // `extra_body`: the 400 arrives as an API error carrying our sentence.
    let mut extra = serde_json::Map::new();
    extra.insert("questions".to_owned(), serde_json::json!({}));
    let err = gw
        .sdk()
        .system_one_opts(
            "x",
            [("q", typesafe_sdk::Question::noul("?"))],
            typesafe_sdk::SystemOneOpts {
                extra_body: Some(extra),
                ..typesafe_sdk::SystemOneOpts::default()
            },
        )
        .await
        .expect_err("refused");
    assert_eq!(err.api().map(|a| a.status), Some(400));
    assert!(
        err.to_string()
            .contains("At least one question is required"),
        "{err}"
    );

    assert!(gw.mock.calls().is_empty(), "no bad body reached Jev");
}

/// A 5xx moves to the next Jev key, and benches the one that failed.
#[tokio::test(flavor = "multi_thread")]
async fn a_5xx_fails_over_to_the_next_jev_key_and_benches_the_first() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    let first = gw.add_jev_key("jev-key-a", 0).await;
    let second = gw.add_jev_key("jev-key-b", 1).await;
    gw.mock.behave("jev-key-a", Behaviour::Refuse(500));

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    let request_id = request_id_of(&response);
    assert_eq!(
        response.bytes().await.expect("body"),
        gw.mock.sent()[0],
        "the second key's answer, unchanged"
    );
    assert_eq!(gw.mock.calls_with("jev-key-a"), 1, "a 5xx is not retried");
    assert_eq!(gw.mock.calls_with("jev-key-b"), 1);

    let (cooldown, _) = gw.benched(first).await;
    assert!(
        cooldown.is_some_and(|t| t > time::OffsetDateTime::now_utc()),
        "the failing key sits out"
    );
    let row = gw.ledger(request_id).await.expect("metered");
    assert_eq!(
        row.account_id,
        Some(second),
        "billed to the key that answered"
    );
    assert_eq!(row.attempt, 1, "on the request's second credential");
}

/// A 408 is about the moment, not the key: retried on the same key, then
/// moved on.
#[tokio::test(flavor = "multi_thread")]
async fn a_408_is_retried_on_the_same_key_before_moving_on() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;
    gw.add_jev_key("jev-key-b", 1).await;
    gw.mock.behave("jev-key-a", Behaviour::Refuse(408));

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        gw.mock.calls_with("jev-key-a"),
        2,
        "the first try and `same_account_retries` (1) more"
    );
    assert_eq!(gw.mock.calls_with("jev-key-b"), 1);
}

/// The breaker trips on the failure a retry is about to follow, and the retry
/// does not happen: a key the breaker has just called unhealthy gets no more
/// of this request.
#[tokio::test(flavor = "multi_thread")]
async fn a_breaker_that_trips_mid_request_stops_the_retries() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    let only = gw.add_jev_key("jev-key-a", 0).await;
    gw.mock.behave("jev-key-a", Behaviour::Refuse(408));
    // One short of the threshold, so the next failure opens it.
    for _ in 0..4 {
        gw.state.breakers.record_failure(AccountId::from_uuid(only));
    }

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(
        gw.mock.calls_with("jev-key-a"),
        1,
        "no retry after the trip"
    );
    assert_eq!(
        response.status(),
        408,
        "the last upstream error, not 'none left'"
    );
    let body = json_of(response).await;
    assert_eq!(body["error"]["type"], "upstream_error");
    assert_eq!(body["error"]["upstream_status"], 408);
}

/// An answer counts for the key that gave it, as a chat answer does: it
/// clears the breaker's run of failures. A key one failure short of the
/// threshold before an answer is a whole threshold short after it, so the
/// next failure leaves it in rotation, where the same failures with no
/// answer between them take a key out.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_clears_the_breakers_run_of_failures() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    let answered = AccountId::from_uuid(gw.add_jev_key("jev-key-a", 0).await);
    let unanswered = AccountId::from_uuid(uuid::Uuid::new_v4());
    // One short of the threshold, so the next failure opens it.
    for _ in 0..4 {
        gw.state.breakers.record_failure(answered);
        gw.state.breakers.record_failure(unanswered);
    }

    assert_eq!(gw.ask(QUESTIONS).await.status(), 200);
    gw.state.breakers.record_failure(answered);
    gw.state.breakers.record_failure(unanswered);
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    assert!(
        gw.state.breakers.permits(answered, now),
        "one failure since the answer, not five in a row"
    );
    assert!(
        !gw.state.breakers.permits(unanswered, now),
        "the premise: five in a row open a breaker"
    );
}

/// A 429 benches the key for as long as Jev said, and the next key serves.
#[tokio::test(flavor = "multi_thread")]
async fn a_429_parks_the_key_for_its_retry_after_and_moves_on() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    let first = gw.add_jev_key("jev-key-a", 0).await;
    gw.add_jev_key("jev-key-b", 1).await;
    gw.mock.behave("jev-key-a", Behaviour::Throttle(120));

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        gw.mock.calls_with("jev-key-a"),
        1,
        "a throttle is not retried"
    );
    let (_, parked) = gw.benched(first).await;
    let now = time::OffsetDateTime::now_utc();
    assert!(
        parked
            .is_some_and(|t| t > now + time::Duration::seconds(100)
                && t < now + time::Duration::seconds(130)),
        "Jev's own Retry-After, not a guess: {parked:?}"
    );
}

/// A 2xx that is not a System One response is not an answer: the next key is
/// tried, and the caller never sees the bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_2xx_that_is_not_an_answer_moves_to_the_next_key() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;
    gw.add_jev_key("jev-key-b", 1).await;
    gw.mock.behave("jev-key-a", Behaviour::NotAnAnswer);

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.expect("body"), gw.mock.sent()[0]);
    assert_eq!(gw.mock.calls_with("jev-key-a"), 1);
    assert_eq!(gw.mock.calls_with("jev-key-b"), 1);
}

/// Unreachable: retried on the same key, then benched for the transport
/// cooldown, then the next key serves. The dead key's proxy is what fails,
/// which is also the proof that a Jev call goes through the key's own
/// transport.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_key_is_retried_then_benched() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    let dead = gw
        .add_account("jev-key-a", 0, true, Some("http://127.0.0.1:1"))
        .await;
    gw.add_jev_key("jev-key-b", 1).await;

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    assert_eq!(gw.mock.calls_with("jev-key-a"), 0, "its proxy refused");
    let (cooldown, _) = gw.benched(dead).await;
    assert!(
        cooldown.is_some_and(|t| t > time::OffsetDateTime::now_utc()),
        "benched once its retries were spent"
    );
}

/// Silent past the headers deadline: failed over at once, not retried.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_key_is_failed_over_without_a_retry() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    let silent = gw.add_jev_key("jev-key-a", 0).await;
    gw.add_jev_key("jev-key-b", 1).await;
    gw.mock.behave("jev-key-a", Behaviour::Silent);

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200);
    assert_eq!(gw.mock.calls_with("jev-key-a"), 1);
    let (cooldown, _) = gw.benched(silent).await;
    assert!(cooldown.is_some_and(|t| t > time::OffsetDateTime::now_utc()));
}

/// A request Jev will not take is the caller's to change. No other key would
/// take it either, and System One has no bigger model to climb to — so the
/// refusal comes straight back, Jev's own body nested in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_jev_rejects_comes_back_without_trying_another_key() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;
    gw.add_jev_key("jev-key-b", 1).await;

    for status in [400, 413, 422] {
        gw.mock.behave("jev-key-a", Behaviour::Refuse(status));
        let before = gw.mock.calls_with("jev-key-a");
        let response = gw.ask(QUESTIONS).await;
        assert_eq!(response.status(), status);
        let body = json_of(response).await;
        assert_eq!(body["error"]["type"], "upstream_error");
        assert_eq!(body["error"]["upstream_status"], status);
        assert_eq!(
            body["error"]["upstream"]["detail"],
            format!("mock Jev refused with {status}")
        );
        assert_eq!(gw.mock.calls_with("jev-key-a"), before + 1, "{status}");
    }
    assert_eq!(gw.mock.calls_with("jev-key-b"), 0, "never another key");
}

/// A budget in its last fifth is `Constrained`, which moves a chat request to
/// a cheaper rung. System One has no cheaper rung, so a constrained key is
/// served as normal — only the hard stop below refuses.
#[tokio::test(flavor = "multi_thread")]
async fn a_constrained_key_is_still_served() {
    let Some(gw) = Gateway::start(CHAT_LADDER, Some(rust_decimal::Decimal::TEN)).await else {
        return;
    };
    sqlx::query("UPDATE api_key SET spent_usd = 9 WHERE id = $1")
        .bind(gw.key_id)
        .execute(gw.db.pool())
        .await
        .expect("spend nine of ten");
    gw.add_jev_key("jev-key-a", 0).await;

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 200, "constrained is not a refusal here");
    assert_eq!(gw.mock.calls_with("jev-key-a"), 1);
}

/// The spend caps apply as they do to chat: an exhausted key is refused
/// before Jev is asked.
#[tokio::test(flavor = "multi_thread")]
async fn an_exhausted_key_is_refused_before_jev_is_asked() {
    let Some(gw) = Gateway::start(CHAT_LADDER, Some(rust_decimal::Decimal::ZERO)).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;

    let response = gw.ask(QUESTIONS).await;
    assert_eq!(response.status(), 402);
    assert_eq!(json_of(response).await["error"]["type"], "budget_exhausted");
    assert!(gw.mock.calls().is_empty());
}

/// And so does the route's rate limit: a System One request is a request on
/// the route, and it may not be a way around the route's throttle.
#[tokio::test(flavor = "multi_thread")]
async fn the_route_s_rate_limit_counts_system_one_requests() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;
    sqlx::query("UPDATE route SET rpm_limit = 1 WHERE id = $1")
        .bind(gw.route)
        .execute(gw.db.pool())
        .await
        .expect("limit the route");

    assert_eq!(gw.ask(QUESTIONS).await.status(), 200);
    let throttled = gw.ask(QUESTIONS).await;
    assert_eq!(throttled.status(), 429);
    assert!(header_of(&throttled, "retry-after").is_some());
    assert_eq!(gw.mock.calls().len(), 1, "the throttled one never left");
}

/// C7. A listing that fails benches no key and counts against no breaker:
/// past the breaker's threshold of failures, every listing still reaches the
/// key, nothing in the store parks it, and its breaker still admits it. A
/// host's model list failing says nothing about whether its keys answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_listing_benches_no_key_and_trips_no_breaker() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    let key = gw.add_jev_key("jev-key-a", 0).await;
    gw.mock.behave("jev-key-a", Behaviour::Refuse(500));

    // One more than the breaker's threshold of five. Each is Jev's own 500,
    // not "no credential": the key was there to ask every time.
    for listing in 0..6 {
        let response = gw.post_get("/jev/v1/models").await;
        assert_eq!(response.status(), 500, "listing {listing}");
        let body = json_of(response).await;
        assert_eq!(
            body["error"]["upstream_status"], 500,
            "listing {listing}: {body}"
        );
    }
    assert_eq!(
        gw.mock.calls_with("jev-key-a"),
        6,
        "every listing reached the key: none was benched by the one before"
    );
    assert_eq!(gw.benched(key).await, (None, None), "nothing parks the key");
    assert!(
        gw.state.breakers.permits(
            AccountId::from_uuid(key),
            time::OffsetDateTime::now_utc().unix_timestamp()
        ),
        "and its breaker never opened"
    );
}

/// The listing is Jev's own, unchanged, with its request id.
#[tokio::test(flavor = "multi_thread")]
async fn the_jev_listing_passes_through_unchanged() {
    let Some(gw) = Gateway::start(CHAT_LADDER, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;

    let response = gw.post_get("/jev/v1/models").await;
    assert_eq!(response.status(), 200);
    assert_eq!(header_of(&response, REQUEST_ID_HEADER), Some("models-1"));
    assert!(
        header_of(&response, "x-oag-request-id").is_some(),
        "which request this was"
    );
    assert_eq!(
        header_of(&response, "x-oag-model"),
        None,
        "a listing is not an answer from a model"
    );
    assert_eq!(response.bytes().await.expect("body"), gw.mock.sent()[0]);
    let calls = gw.mock.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/v1/models");
    assert_eq!(calls[0].key, "jev-key-a");
}

/// No chat request can reach a Jev key: not by naming a Jev model, not by its
/// bare name, and not off a ladder someone wrote a Jev model onto.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_request_can_never_reach_a_jev_key() {
    // A ladder whose cheap rung names Jev's model, on a route whose only
    // credential is a Jev key. Everything that could send chat to it is here.
    let ladder = r#"[{"name":"cheap","models":["jev/jev-latest"]},
                    {"name":"balanced","models":["anthropic/claude-haiku-4.5"]}]"#;
    let Some(gw) = Gateway::start(ladder, None).await else {
        return;
    };
    gw.add_jev_key("jev-key-a", 0).await;
    gw.state
        .set_catalog(
            oag_store::repo::catalog(&gw.db)
                .await
                .expect("catalog")
                .iter()
                .filter_map(oag_store::ModelRow::to_spec)
                .chain([
                    priced(&Catalog::new(), Provider::Jev, "jev-latest", None),
                    ModelSpec {
                        id: ModelId::new("anthropic/claude-haiku-4.5"),
                        provider: Provider::Anthropic,
                        upstream_name: "claude-haiku-4-5".to_owned(),
                        pricing: Pricing {
                            input_per_mtok: rust_decimal::Decimal::ONE,
                            output_per_mtok: rust_decimal::Decimal::from(5),
                            cache_read_per_mtok: None,
                            cache_write_per_mtok: None,
                        },
                        context_window: 200_000,
                        max_output_tokens: 32_000,
                        capabilities: Capabilities::default(),
                        display_label: None,
                    },
                ]),
        )
        .await;

    let chat = |model: &str| {
        format!(
            r#"{{"model":"{model}","max_tokens":8,"messages":[{{"role":"user","content":"hi"}}]}}"#
        )
    };

    // Named, in full: refused, and told where System One is served.
    let named = gw
        .post("/v1/chat/completions", &chat("jev/jev-latest"))
        .await;
    assert_eq!(named.status(), 400);
    let body = json_of(named).await;
    assert_eq!(body["error"]["type"], "no_viable_model");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("/jev/v1/systemone")),
        "{body}"
    );

    // By its bare upstream name.
    let bare = gw.post("/v1/chat/completions", &chat("jev-latest")).await;
    assert_eq!(bare.status(), 400);

    // Managed, with Jev's model on the cheap rung: the rung has nothing chat
    // can pick, so the climb goes on to Anthropic — which has no credential
    // here. A 503 about Anthropic, and not one call to Jev.
    let managed = gw.post("/v1/messages", &chat("oag/cheap")).await;
    assert_eq!(managed.status(), 503);
    assert_eq!(json_of(managed).await["error"]["type"], "no_credential");

    assert!(
        gw.mock.calls().is_empty(),
        "no chat request reached Jev: {:?}",
        gw.mock.calls()
    );
}

impl Gateway {
    async fn post_get(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("{}{path}", self.url))
            .bearer_auth(&self.key)
            .send()
            .await
            .expect("the gateway answers")
    }
}

// ── the pure half ──────────────────────────────────────────────────────────

#[test]
fn a_reported_count_is_a_ledger_count() {
    assert_eq!(tokens(Some(42)), 42);
    assert_eq!(tokens(Some(0)), 0);
    assert_eq!(tokens(None), 0, "absent is nothing to bill");
    assert_eq!(tokens(Some(-5)), 0, "and so is nonsense");
}

#[test]
fn an_answer_is_priced_by_its_catalog_row_or_left_unpriced() {
    let priced_row = ModelSpec {
        id: ModelId::new("jev/jev-latest"),
        provider: Provider::Jev,
        upstream_name: "jev-latest".to_owned(),
        pricing: Pricing {
            input_per_mtok: rust_decimal::Decimal::ONE,
            output_per_mtok: rust_decimal::Decimal::TWO,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        },
        context_window: 0,
        max_output_tokens: 0,
        capabilities: Capabilities::default(),
        display_label: None,
    };
    let catalog = Catalog::from_entries([priced_row.clone()]);
    assert_eq!(
        priced(&catalog, Provider::Jev, "jev-latest", None),
        priced_row
    );

    // A model the catalog has never seen — Jev answering with a dated name,
    // say — is still metered, under its own id, at no cost.
    let stand_in = priced(&catalog, Provider::Jev, "jev-2026-08-01", None);
    assert_eq!(stand_in.id.as_str(), "jev/jev-2026-08-01");
    assert_eq!(stand_in.provider, Provider::Jev);
    assert_eq!(stand_in.upstream_name, "jev-2026-08-01");
    assert!(
        stand_in.pricing.input_per_mtok.is_zero() && stand_in.pricing.output_per_mtok.is_zero()
    );
}

#[test]
fn a_body_that_is_not_the_wire_type_is_an_error_naming_it() {
    let err = decoded::<SystemOneResponse>(br#"{"answers":{}}"#).expect_err("no model");
    assert!(err.to_string().contains("SystemOneResponse"), "{err}");
    let ok = decoded::<ListModelsResponse>(br#"{"models":[]}"#).expect("a listing");
    assert!(ok.models.is_empty());
}
