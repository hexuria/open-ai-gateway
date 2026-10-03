//! System One hosts, end to end: a real gateway on real ports, with three
//! wiremock upstreams behind it — the built-in Jev, and two System One hosts
//! registered while it runs: `selfjev`, which answers in Jev's shape at Jev's
//! own path, and `merge-decisions`, which answers as Merge Gateway's Decisions
//! API does, at `/v1/decisions`.
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
use oag_router::ModelId;
use oag_server::AppState;
use oag_store::{Cache, Db, EndpointDeletion, NewEndpoint, repo};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockGuard, MockServer, Request, ResponseTemplate};

/// How long a change may take to show. The refresh runs every second; this
/// allows for a loaded machine.
const WAIT: Duration = Duration::from_secs(45);

/// Merge Gateway's model as an operator catalogs it: the endpoint's name, then
/// Merge's own id, slash and all.
const MERGE_MODEL: &str = "merge-decisions/typesafe/jev-1.13";
/// The Jev-shaped host's model.
const SELF_MODEL: &str = "selfjev/jev-latest";

/// Merge Gateway's answer as its documentation shows it: the concrete version
/// that answered, not the one asked for; `object` and `vendor` beside the
/// answers; and `total_tokens` and `cost` in the usage. Written out rather
/// than serialised, so `1.6926e-05` is spelt as Merge spells it.
const MERGE_ANSWER: &str = r#"{"object": "decision", "model": "jev-1.13.0", "vendor": "typesafe",
 "answers": {"billing": {"type": "noul", "noul": 0.95},
  "team": {"type": "choice", "choice": "billing", "confidence": 1.0,
   "probabilities": {"billing": 1.0, "support": 0.0}},
  "urgency": {"type": "score", "score": 0.84, "confidence": 0.84,
   "legend": {"0": "low", "1": "high"}, "probabilities": {"0": 0.16, "1": 0.84}}},
 "usage": {"input_tokens": 403, "output_tokens": 70, "total_tokens": 473, "cost": 1.6926e-05}}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn system_one_hosts_are_served_end_to_end_and_unserved_when_removed() {
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
    let hosts = Hosts {
        jev: MockServer::start().await,
        selfjev: MockServer::start().await,
        merge: MockServer::start().await,
    };
    let gw = Gateway::start(&db_url, &redis_url, &hosts.jev.uri()).await;

    // Everything from here on is written while the gateway serves: nothing
    // restarts it, and only its refresh picks the rows up.
    gw.endpoint(
        "selfjev",
        &hosts.selfjev.uri(),
        None,
        "x_api_key",
        &json!({"x-team": "t7"}),
    )
    .await;
    gw.endpoint(
        "merge-decisions",
        &hosts.merge.uri(),
        Some("/v1/decisions"),
        "bearer",
        &json!({}),
    )
    .await;
    let jev_key = gw.account("jev", "t7-jev-key", 0, gw.route).await;
    let self_key = gw.account("selfjev", "t7-selfjev-key", 0, gw.route).await;
    let merge_a = gw
        .account("merge-decisions", "t7-merge-key-a", 0, gw.route)
        .await;
    let merge_b = gw
        .account("merge-decisions", "t7-merge-key-b", 1, gw.route)
        .await;
    gw.model(SELF_MODEL, "selfjev", "jev-latest").await;
    gw.model(MERGE_MODEL, "merge-decisions", "typesafe/jev-1.13")
        .await;
    gw.served_until("both hosts and their models are served", true)
        .await;

    jev_shaped_host(&gw, &hosts, self_key).await;
    merge_decisions(&gw, &hosts, merge_a).await;
    the_sdk_asks_merge(&gw, &hosts).await;
    built_in_jev_unchanged(&gw, &hosts, jev_key).await;
    unknown_models(&gw, &hosts).await;
    listing(&gw, &hosts).await;
    failover(&gw, &hosts, merge_a, merge_b).await;
    a_host_whose_listing_fails(&gw, &hosts, [merge_a, merge_b]).await;
    removed(&gw, &hosts).await;
}

/// A host in Jev's own shape, at Jev's own path, with its key in `x-api-key`:
/// it is sent its own name for the model and the rest of the caller's bytes,
/// and its answer comes back byte for byte, priced by its catalog row.
async fn jev_shaped_host(gw: &Gateway, hosts: &Hosts, key: Uuid) {
    let answer = jev_answer("jev-latest");
    let mock = answering(
        &hosts.selfjev,
        "/v1/systemone",
        ("x-api-key", "t7-selfjev-key"),
        answer.clone(),
    )
    .await;
    let before = hosts.seen().await;
    let body = questions(SELF_MODEL, r#","trace":"rides along""#);
    let res = gw.ask(&gw.key, &body).await;
    assert_eq!(res.status(), 200, "{:?}", res.headers());
    assert_eq!(header_of(&res, "x-oag-model"), Some(SELF_MODEL));
    assert_eq!(header_of(&res, "x-typesafe-request-id"), Some("upstream-1"));
    let request_id = request_id_of(&res);
    assert_eq!(
        res.bytes().await.expect("body"),
        answer,
        "the host's bytes, unchanged"
    );
    assert_eq!(
        hosts.seen().await,
        before.plus([0, 1, 0]),
        "only its own host was asked"
    );

    let sent = only_request(&mock).await;
    assert_one_key(&sent, "x-api-key");
    assert_eq!(header_of_request(&sent, "x-team"), Some("t7"));
    assert_eq!(
        String::from_utf8(sent.body.clone()).expect("utf-8"),
        body.replace(SELF_MODEL, "jev-latest"),
        "its own name for the model, and every other byte as the caller sent it"
    );

    let row = gw.ledger(request_id).await;
    assert_eq!(row.model_id, SELF_MODEL);
    assert_eq!((row.input_tokens, row.output_tokens), (42, 3));
    assert_eq!(row.account_id, Some(key));
    // 42 input tokens at $2/Mtok, and output free.
    assert_eq!(row.cost_usd, Decimal::new(84, 6));
}

/// Merge Gateway's Decisions API: posted at `/v1/decisions` with the bearer
/// key, sent `typesafe/jev-1.13`, answered as `jev-1.13.0` with Merge's own
/// extras, all returned byte for byte. The ledger prices the tokens by the
/// catalog row the request named, not by Merge's `usage.cost`.
async fn merge_decisions(gw: &Gateway, hosts: &Hosts, key_a: Uuid) {
    let mock = answering(
        &hosts.merge,
        "/v1/decisions",
        ("authorization", "Bearer t7-merge-key-a"),
        MERGE_ANSWER.as_bytes().to_vec(),
    )
    .await;
    let before = hosts.seen().await;
    let body = questions(MERGE_MODEL, r#","vendor":"typesafe""#);
    let res = gw.ask(&gw.key, &body).await;
    assert_eq!(res.status(), 200);
    assert_eq!(
        header_of(&res, "x-oag-model"),
        Some(MERGE_MODEL),
        "priced as, and so named as, the row it was asked by"
    );
    let request_id = request_id_of(&res);
    assert_eq!(res.bytes().await.expect("body"), MERGE_ANSWER.as_bytes());
    assert_eq!(hosts.seen().await, before.plus([0, 0, 1]));

    let sent = only_request(&mock).await;
    assert_one_key(&sent, "authorization");
    let wire = body_of(&sent);
    assert_eq!(wire["model"], "typesafe/jev-1.13");
    assert_eq!(
        wire["vendor"], "typesafe",
        "a field Merge takes rides along"
    );
    assert_eq!(
        String::from_utf8(sent.body.clone()).expect("utf-8"),
        body.replace(MERGE_MODEL, "typesafe/jev-1.13")
    );

    let row = gw.ledger(request_id).await;
    assert_eq!(row.model_id, MERGE_MODEL);
    assert_eq!((row.input_tokens, row.output_tokens), (403, 70));
    assert_eq!(row.account_id, Some(key_a));
    assert_eq!(
        row.cost_usd,
        Decimal::new(806, 6),
        "403 input tokens at the catalog's $2/Mtok; Merge's own 0.000016926 is not the ledger's"
    );
    assert_eq!(row.attempt, 0);
}

/// The owner's SDK, unmodified, asks Merge through the gateway by the model's
/// catalog id and decodes Merge's answer, extras and all.
async fn the_sdk_asks_merge(gw: &Gateway, hosts: &Hosts) {
    let _mock = answering(
        &hosts.merge,
        "/v1/decisions",
        ("authorization", "Bearer t7-merge-key-a"),
        MERGE_ANSWER.as_bytes().to_vec(),
    )
    .await;
    let answer = gw
        .sdk()
        .system_one_opts(
            json!({"ticket": "I was charged twice."}),
            [
                (
                    "billing",
                    typesafe_sdk::Question::noul("Is this about billing?"),
                ),
                (
                    "team",
                    typesafe_sdk::Question::choice(
                        "Which team?",
                        [("billing", None), ("support", None)],
                    ),
                ),
            ],
            typesafe_sdk::SystemOneOpts {
                model: Some(MERGE_MODEL.to_owned()),
                ..typesafe_sdk::SystemOneOpts::default()
            },
        )
        .await
        .expect("the SDK reads Merge's answer through the gateway");
    assert_eq!(answer.model, "jev-1.13.0");
    assert!((answer.noul("billing").expect("noul").noul - 0.95).abs() < f64::EPSILON);
    assert_eq!(answer.choice("team").expect("choice").choice, "billing");
    assert_eq!(answer.usage.input_tokens, Some(403));
    assert_eq!(answer.raw_body(), MERGE_ANSWER.as_bytes());
}

/// The built-in Jev, beside the hosts: a bare model is Jev's, sent with the
/// Jev key as a bearer and the caller's bytes exactly, and only Jev is asked.
async fn built_in_jev_unchanged(gw: &Gateway, hosts: &Hosts, key: Uuid) {
    let answer = jev_answer("jev-latest");
    let mock = answering(
        &hosts.jev,
        "/v1/systemone",
        ("authorization", "Bearer t7-jev-key"),
        answer.clone(),
    )
    .await;
    let before = hosts.seen().await;
    let body = questions("jev-latest", r#","trace":"rides along""#);
    let res = gw.ask(&gw.key, &body).await;
    assert_eq!(res.status(), 200);
    assert_eq!(header_of(&res, "x-oag-model"), Some("jev/jev-latest"));
    let request_id = request_id_of(&res);
    assert_eq!(res.bytes().await.expect("body"), answer);
    assert_eq!(hosts.seen().await, before.plus([1, 0, 0]));
    let sent = only_request(&mock).await;
    assert_one_key(&sent, "authorization");
    assert_eq!(sent.body, body.as_bytes(), "the caller's bytes, unchanged");
    let row = gw.ledger(request_id).await;
    assert_eq!(row.model_id, "jev/jev-latest");
    assert_eq!(row.account_id, Some(key));
    assert_eq!(row.cost_usd, Decimal::ZERO, "no row prices it here");
}

/// A model that names no System One provider this gateway serves is refused
/// before anything is sent anywhere, and a route holding no credential for the
/// provider a request names gets the refusal that says whose key to add — the
/// one a route with no Jev key has always had.
async fn unknown_models(gw: &Gateway, hosts: &Hosts) {
    let before = hosts.seen().await;
    for model in ["no-such-host/jev-latest", "anthropic/claude-haiku-4.5"] {
        let res = gw.ask(&gw.key, &questions(model, "")).await;
        assert_eq!(res.status(), 400, "{model}");
        let body = json_of(res).await;
        assert_eq!(body["error"]["type"], "no_viable_model", "{model}");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("is not a System One model")),
            "{body}"
        );
    }
    // A host's model nobody has priced (C4): what the host would charge for
    // its answer could not be metered, so it is not asked.
    for model in ["merge-decisions/typesafe/jev-9", "selfjev/jev-1.13.0"] {
        let res = gw.ask(&gw.key, &questions(model, "")).await;
        assert_eq!(res.status(), 400, "{model}");
        let body = json_of(res).await;
        assert_eq!(body["error"]["type"], "no_viable_model", "{model}");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains(&format!("'{model}' is not in the catalog"))),
            "{body}"
        );
    }
    for (model, whose) in [("jev-latest", "Jev"), (MERGE_MODEL, "merge-decisions")] {
        let res = gw.ask(&gw.bare_key, &questions(model, "")).await;
        assert_eq!(res.status(), 503, "{model}");
        let body = json_of(res).await;
        assert_eq!(
            body["error"]["type"], "system_one_not_configured",
            "{model}"
        );
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(&format!("holds no {whose} credential")),
            "{message}"
        );
    }
    let listing = gw.get(&gw.bare_key, "/jev/v1/models").await;
    assert_eq!(listing.status(), 503, "nothing to list from");

    // Nor can a chat request reach a host: its model is System One's, which
    // no chat route routes over, and the refusal says where it is served.
    let chat = gw
        .client
        .post(format!("{}/v1/chat/completions", gw.public))
        .bearer_auth(&gw.key)
        .json(&json!({"model": MERGE_MODEL, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("the gateway answers");
    assert_eq!(chat.status(), 400);
    let body = json_of(chat).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("/jev/v1/systemone")),
        "{body}"
    );
    assert_eq!(hosts.seen().await, before, "nothing reached any host");
}

/// `/jev/v1/models`: Jev's models by their own names, then each host's as
/// `<host>/<name>`, in the SDK's shape. Merge's listing is its paged list of
/// every model it routes, from which only its decision models are kept.
async fn listing(gw: &Gateway, hosts: &Hosts) {
    let mut mocks = vec![
        listing_mock(
            &hosts.jev,
            Page::Unpaged,
            None,
            json!({"models": [
                {"name": "jev-latest", "description": "Fast", "release_date": "2026-08-01"}
            ]}),
        )
        .await,
        listing_mock(
            &hosts.selfjev,
            Page::First,
            None,
            json!({"models": [
                {"name": "jev-latest", "description": "Fast", "release_date": "2026-08-01"},
                {"name": "jev-1.13.0", "description": "Pinned", "release_date": "2026-07-01"}
            ]}),
        )
        .await,
    ];
    mocks.push(
        listing_mock(
            &hosts.merge,
            Page::First,
            None,
            json!({"object": "list", "has_more": true, "next_cursor": "t7-page-2", "data": [
                {"model": "openai/gpt-5.4", "display_name": "GPT-5.4",
                 "vendors": {"openai": {"capabilities": {"output": ["text", "tool_use"]}}}},
                {"model": "typesafe/jev-1.13", "display_name": "Jev 1.13",
                 "vendors": {"typesafe": {"launch_date": "2026-07-01",
                    "capabilities": {"output": ["decision"]}}}}
            ]}),
        )
        .await,
    );
    mocks.push(
        listing_mock(
            &hosts.merge,
            Page::After("t7-page-2"),
            None,
            json!({"object": "list", "has_more": false, "next_cursor": null, "data": [
                {"model": "typesafe/jev-1.14", "display_name": "Jev 1.14",
                 "vendors": {"typesafe": {"capabilities": {"output": ["decision"]}}}}
            ]}),
        )
        .await,
    );

    let listing = gw.sdk().models().await.expect("the SDK reads the listing");
    let names: Vec<&str> = listing.models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "jev-latest",
            "merge-decisions/typesafe/jev-1.13",
            "merge-decisions/typesafe/jev-1.14",
            "selfjev/jev-latest",
            "selfjev/jev-1.13.0",
        ]
    );
    let merged = &listing.models[1];
    assert_eq!(
        (merged.description.as_str(), merged.release_date.as_str()),
        ("Jev 1.13", "2026-07-01")
    );
    for mock in &mocks {
        only_request(mock).await;
    }
    assert_eq!(
        only_request(&mocks[0]).await.url.query(),
        None,
        "Jev's own listing is asked as it always was"
    );
    // Each host's listing is asked with its own key, where it takes one.
    assert_one_key(&only_request(&mocks[1]).await, "x-api-key");
    for page in &mocks[2..] {
        assert_one_key(&only_request(page).await, "authorization");
    }
    drop(mocks);

    // A route whose only System One key is a host's lists that host alone.
    let _page = listing_mock(
        &hosts.merge,
        Page::First,
        Some(("authorization", "Bearer t7-merge-key-a")),
        json!({"data": [{"model": "typesafe/jev-1.13"}]}),
    )
    .await;
    gw.account("merge-decisions", "t7-merge-key-a", 0, gw.merge_route)
        .await;
    let res = gw.get(&gw.merge_key, "/jev/v1/models").await;
    let body = json_of(res).await;
    assert_eq!(
        body,
        json!({"models": [{"name": MERGE_MODEL, "description": "", "release_date": ""}]})
    );
}

/// C7. A host whose listing fails is left out of it, and the others are
/// listed: Jev's models and `selfjev`'s. Merge refuses the listing with a
/// Retry-After, and neither of its keys is parked for it. This runs after the
/// failover step because a listing leases a key as a question set does, so it
/// moves the caller's pin, which that step starts from.
async fn a_host_whose_listing_fails(gw: &Gateway, hosts: &Hosts, merge_keys: [Uuid; 2]) {
    let before = [
        parked(gw, merge_keys[0]).await,
        parked(gw, merge_keys[1]).await,
    ];
    let mocks = [
        listing_mock(
            &hosts.jev,
            Page::Unpaged,
            None,
            json!({"models": [
                {"name": "jev-latest", "description": "Fast", "release_date": "2026-08-01"}
            ]}),
        )
        .await,
        listing_mock(
            &hosts.selfjev,
            Page::First,
            None,
            json!({"models": [
                {"name": "jev-latest", "description": "Fast", "release_date": "2026-08-01"}
            ]}),
        )
        .await,
    ];
    let failing = Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("retry-after", "600")
                .set_body_string("t7: listing is down"),
        )
        .mount_as_scoped(&hosts.merge)
        .await;

    let listing = gw
        .sdk()
        .models()
        .await
        .expect("listed without the host that failed");
    let names: Vec<&str> = listing.models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["jev-latest", "selfjev/jev-latest"]);
    for mock in &mocks {
        only_request(mock).await;
    }
    assert!(
        !failing.received_requests().await.is_empty(),
        "the failing host was asked"
    );
    let after = [
        parked(gw, merge_keys[0]).await,
        parked(gw, merge_keys[1]).await,
    ];
    assert_eq!(after, before, "a listing parks no key");
}

/// A key's `cooldown_until` and `rate_limited_until`.
async fn parked(
    gw: &Gateway,
    key: Uuid,
) -> (Option<time::OffsetDateTime>, Option<time::OffsetDateTime>) {
    sqlx::query_as("SELECT cooldown_until, rate_limited_until FROM account WHERE id = $1")
        .bind(key)
        .fetch_one(gw.db.pool())
        .await
        .expect("the account")
}

/// A 429 on the host's first key is served by its second: one request each,
/// the first parked for as long as Merge said, and the ledger names the key
/// that answered.
async fn failover(gw: &Gateway, hosts: &Hosts, key_a: Uuid, key_b: Uuid) {
    let limited = Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .and(header("authorization", "Bearer t7-merge-key-a"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "120")
                .set_body_json(json!({"error": {"message": "slow down", "type": "rate_limit"}})),
        )
        .expect(1)
        .mount_as_scoped(&hosts.merge)
        .await;
    let answered = answering(
        &hosts.merge,
        "/v1/decisions",
        ("authorization", "Bearer t7-merge-key-b"),
        MERGE_ANSWER.as_bytes().to_vec(),
    )
    .await;
    let res = gw.ask(&gw.key, &questions(MERGE_MODEL, "")).await;
    assert_eq!(res.status(), 200);
    let request_id = request_id_of(&res);
    assert_eq!(res.bytes().await.expect("body"), MERGE_ANSWER.as_bytes());
    assert_eq!(limited.received_requests().await.len(), 1, "key a, once");
    only_request(&answered).await;
    let row = gw.ledger(request_id).await;
    assert_eq!(
        row.account_id,
        Some(key_b),
        "billed to the key that answered"
    );
    assert_eq!(row.attempt, 1);
    let parked: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT rate_limited_until FROM account WHERE id = $1")
            .bind(key_a)
            .fetch_one(gw.db.pool())
            .await
            .expect("the account");
    assert!(
        parked.is_some_and(|t| t > time::OffsetDateTime::now_utc() + time::Duration::seconds(90)),
        "parked for Merge's own Retry-After: {parked:?}"
    );
}

/// Removed as the store allows it — its keys and model first — the host
/// stops being served, and a request still naming its model is refused rather
/// than sent anywhere else.
async fn removed(gw: &Gateway, hosts: &Hosts) {
    for table in ["model_catalog", "account"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE provider = 'merge-decisions'"
        )))
        .execute(gw.db.pool())
        .await
        .expect("remove what names it");
    }
    assert_eq!(
        repo::delete_endpoint(&gw.db, "merge-decisions")
            .await
            .expect("delete"),
        EndpointDeletion::Deleted
    );
    gw.served_until("merge-decisions is no longer served", false)
        .await;

    let before = hosts.seen().await;
    let res = gw.ask(&gw.key, &questions(MERGE_MODEL, "")).await;
    assert_eq!(res.status(), 400);
    assert_eq!(json_of(res).await["error"]["type"], "no_viable_model");
    assert_eq!(hosts.seen().await, before, "not sent to Jev, nor anywhere");
}

/// The three upstreams.
struct Hosts {
    jev: MockServer,
    selfjev: MockServer,
    merge: MockServer,
}

/// How many requests each upstream has been sent: Jev, selfjev, merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen([usize; 3]);

impl Seen {
    fn plus(self, more: [usize; 3]) -> Self {
        Self([
            self.0[0] + more[0],
            self.0[1] + more[1],
            self.0[2] + more[2],
        ])
    }
}

impl Hosts {
    async fn seen(&self) -> Seen {
        let mut seen = [0; 3];
        for (count, server) in seen.iter_mut().zip([&self.jev, &self.selfjev, &self.merge]) {
            *count = server.received_requests().await.expect("recording").len();
        }
        Seen(seen)
    }
}

/// A gateway on real ports, with a principal and three routes of its own,
/// each with an inference key: the main one, one with no System One key at
/// all, and one whose only one is Merge's.
struct Gateway {
    state: Arc<AppState>,
    db: Db,
    client: reqwest::Client,
    public: String,
    key: String,
    bare_key: String,
    merge_key: String,
    route: Uuid,
    merge_route: Uuid,
}

#[derive(Debug, sqlx::FromRow)]
struct LedgerRow {
    model_id: String,
    input_tokens: i64,
    output_tokens: i64,
    cost_usd: Decimal,
    account_id: Option<Uuid>,
    attempt: i16,
}

impl Gateway {
    async fn start(db_url: &str, redis_url: &str, jev: &str) -> Self {
        // Both held until both are known, so they cannot be the same port.
        let (public, admin) = (free_port(), free_port());
        let (public_port, admin_port) = (port_of(&public), port_of(&admin));
        drop((public, admin));
        // Every built-in but Jev on a closed port: nothing this gateway could
        // send leaves the machine.
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
    jev: "{jev}"
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

        let email = "t7-owner@example.invalid";
        sqlx::query(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')",
        )
        .bind(email)
        .execute(db.pool())
        .await
        .expect("principal");
        let mut routes = Vec::new();
        let mut keys = Vec::new();
        for name in ["t7", "t7-bare", "t7-merge"] {
            // A route needs a rung to parse; this one names nothing.
            let route: Uuid = sqlx::query_scalar(
                "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, \
                 '[{\"name\": \"cheap\", \"models\": [\"anthropic/placeholder\"]}]'::jsonb) \
                 RETURNING id",
            )
            .bind(name)
            .fetch_one(db.pool())
            .await
            .expect("route");
            let key = repo::mint_key(&db, email, name, &format!("{name}-key"), None)
                .await
                .expect("mint")
                .expect("the principal and route exist");
            routes.push(route);
            keys.push(key.key);
        }
        let [key, bare_key, merge_key] = <[String; 3]>::try_from(keys).expect("three keys");

        Self {
            state,
            db,
            client,
            public,
            key,
            bare_key,
            merge_key,
            route: routes[0],
            merge_route: routes[2],
        }
    }

    async fn endpoint(
        &self,
        name: &str,
        base_url: &str,
        path: Option<&str>,
        auth: &str,
        extra_headers: &Value,
    ) {
        repo::insert_endpoint(
            &self.db,
            &NewEndpoint {
                name,
                dialect: "system_one",
                platform: "plain",
                base_url: Some(base_url),
                auth,
                region: None,
                project: None,
                api_version: None,
                path,
                extra_headers,
                display_name: None,
                discover_models: false,
            },
        )
        .await
        .expect("an endpoint");
    }

    /// A key filed under `provider`, sealed as `account add` seals one and
    /// joined to `route`. Lower `priority` is tried first.
    async fn account(&self, provider: &str, secret: &str, priority: i16, route: Uuid) -> Uuid {
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
        .bind(format!("{secret}-{}", Uuid::new_v4()))
        .bind(provider)
        .bind(&sealed.ciphertext)
        .bind(&sealed.nonce)
        .bind(priority)
        .fetch_one(self.db.pool())
        .await
        .expect("account");
        sqlx::query("INSERT INTO account_route (account_id, route_id) VALUES ($1, $2)")
            .bind(account)
            .bind(route)
            .execute(self.db.pool())
            .await
            .expect("join");
        account
    }

    /// A System One catalog row: $2 per million input tokens, output free, as
    /// Merge prices `typesafe/jev-1.13` today.
    async fn model(&self, id: &str, provider: &str, upstream: &str) {
        let row = oag_store::ModelRow {
            id: id.to_owned(),
            provider: provider.to_owned(),
            upstream_name: upstream.to_owned(),
            input_per_mtok: Decimal::TWO,
            output_per_mtok: Decimal::ZERO,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
            context_window: 64_000,
            max_output_tokens: 0,
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

    /// Waits until the refresh has loaded both hosts and both their models
    /// (`served`), or has dropped merge-decisions and its model (`!served`).
    async fn served_until(&self, what: &str, served: bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            let catalog = self.state.system_one_catalog().await;
            let loaded = |name: &str, model: &str| {
                name.parse::<Provider>()
                    .is_ok_and(|p| self.state.system_one(p).is_ok())
                    && catalog.get(&ModelId::new(model)).is_some()
            };
            let done = if served {
                loaded("selfjev", SELF_MODEL) && loaded("merge-decisions", MERGE_MODEL)
            } else {
                "merge-decisions".parse::<Provider>().is_err()
                    && catalog.get(&ModelId::new(MERGE_MODEL)).is_none()
            };
            if done {
                return;
            }
            assert!(Instant::now() < deadline, "{what}: not after {WAIT:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn ask(&self, key: &str, body: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/jev/v1/systemone", self.public))
            .bearer_auth(key)
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .expect("the gateway answers")
    }

    async fn get(&self, key: &str, at: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{at}", self.public))
            .bearer_auth(key)
            .send()
            .await
            .expect("the gateway answers")
    }

    fn sdk(&self) -> typesafe_sdk::Client {
        typesafe_sdk::Client::builder()
            .api_key(&self.key)
            .base_url(format!("{}/jev", self.public))
            .retry(typesafe_sdk::RetryPolicy::disabled())
            .build()
            .expect("client")
    }

    /// The served ledger row for one request, once the detached write lands.
    async fn ledger(&self, request_id: Uuid) -> LedgerRow {
        let deadline = Instant::now() + WAIT;
        loop {
            let row: Option<LedgerRow> = sqlx::query_as(
                "SELECT model_id, input_tokens, output_tokens, cost_usd, account_id, attempt \
                 FROM usage_event WHERE request_id = $1 AND status = 200",
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

/// A question set of each type about a state holding a number no float can
/// hold, naming `model`, with `extra` members after the questions.
fn questions(model: &str, extra: &str) -> String {
    format!(
        r#"{{"state":{{"ticket":"I was charged twice.","account":123456789012345678901234567890}},"model":"{model}","questions":{{"billing":{{"type":"noul","instructions":"Is this about billing?"}},"team":{{"type":"choice","instructions":"Which team?","criteria":{{"billing":"Invoices","support":"Bugs"}}}},"urgency":{{"type":"score","instructions":"How urgent?","criteria":["low","high"]}}}}{extra}}}"#
    )
}

/// An answer in Jev's shape to [`questions`], indented and with a trailing
/// newline: bytes no serialiser would write, so a gateway that re-wrote the
/// answer would be caught.
fn jev_answer(model: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(&json!({
        "model": model,
        "usage": {"input_tokens": 42, "output_tokens": 3},
        "answers": {
            "billing": {"type": "noul", "noul": 0.97},
            "team": {"type": "choice", "choice": "billing", "confidence": 0.9,
                     "probabilities": {"billing": 0.9, "support": 0.1}},
            "urgency": {"type": "score", "score": 1.0, "confidence": 0.8,
                        "legend": {"0": "low", "1": "high"},
                        "probabilities": {"0": 0.2, "1": 0.8}}
        }
    }))
    .expect("serialises");
    bytes.push(b'\n');
    bytes
}

/// A mock on `server` that answers a question set at `at` with `answer`, for
/// the one request carrying `key`, and expects exactly one.
async fn answering(
    server: &MockServer,
    at: &str,
    (name, value): (&'static str, &'static str),
    answer: Vec<u8>,
) -> MockGuard {
    Mock::given(method("POST"))
        .and(path(at))
        .and(header(name, value))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(answer, "application/json")
                .insert_header("x-typesafe-request-id", "upstream-1"),
        )
        .expect(1)
        .mount_as_scoped(server)
        .await
}

/// Which listing request a mock answers.
#[derive(Debug, Clone, Copy)]
enum Page {
    /// Jev's own, which is not paged.
    Unpaged,
    /// A host's first page.
    First,
    /// A host's page after the one whose cursor this is.
    After(&'static str),
}

/// A listing mock on `server` for one `page`, expecting one request. `key`
/// narrows it to one credential.
async fn listing_mock(
    server: &MockServer,
    page: Page,
    key: Option<(&'static str, &'static str)>,
    listing: Value,
) -> MockGuard {
    let mut mock = Mock::given(method("GET")).and(path("/v1/models"));
    match page {
        Page::Unpaged => mock = mock.and(query_param_is_missing("limit")),
        Page::First => {
            mock = mock
                .and(query_param("limit", "500"))
                .and(query_param_is_missing("cursor"));
        }
        Page::After(cursor) => {
            mock = mock
                .and(query_param("limit", "500"))
                .and(query_param("cursor", cursor));
        }
    }
    if let Some((name, value)) = key {
        mock = mock.and(header(name, value));
    }
    mock.respond_with(ResponseTemplate::new(200).set_body_json(listing))
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

fn header_of<'a>(res: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    res.headers().get(name).and_then(|v| v.to_str().ok())
}

fn header_of_request<'a>(sent: &'a Request, name: &str) -> Option<&'a str> {
    sent.headers.get(name).and_then(|v| v.to_str().ok())
}

fn body_of(sent: &Request) -> Value {
    serde_json::from_slice(&sent.body).expect("a JSON body")
}

fn request_id_of(res: &reqwest::Response) -> Uuid {
    header_of(res, "x-oag-request-id")
        .and_then(|v| v.parse().ok())
        .expect("every answer names its request")
}

async fn json_of(res: reqwest::Response) -> Value {
    res.json().await.expect("a JSON body")
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
    let name = format!("oag_t7_{}", Uuid::new_v4().simple());
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
