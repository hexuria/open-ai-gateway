use super::*;
use axum::body::to_bytes;
use oag_core::Provider;
use oag_core::provider::EndpointRegistry;
use std::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn actor() -> AdminActor {
    AdminActor {
        principal_id: uuid::Uuid::nil(),
        email: "ops@example.invalid".to_owned(),
    }
}

/// The status and the JSON body of an answer.
async fn read(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    (
        status,
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned())),
    )
}

fn input(body: &Value) -> EndpointInput {
    serde_json::from_value(body.clone()).expect("a registration body")
}

fn patch(body: &Value) -> EndpointPatch {
    serde_json::from_value(body.clone()).expect("a change body")
}

/// A POST against a state whose database is a closed port. Every rule is
/// checked before the database is, so a refusal comes back without a store.
async fn create(body: Value) -> (StatusCode, Value) {
    let state = crate::testing::state("");
    read(create_endpoint(State(state), actor(), Json(input(&body))).await).await
}

#[tokio::test]
async fn a_metadata_or_link_local_url_is_a_400() {
    for base_url in [
        "http://169.254.169.254/latest",
        "http://[fe80::1]:8000/v1",
        "http://metadata.google.internal/v1",
    ] {
        let (status, body) = create(json!({
            "name": "t5-api-metadata",
            "dialect": "openai",
            "platform": "plain",
            "base_url": base_url,
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{base_url}: {body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.contains("link-local or cloud-metadata")),
            "{base_url}: {body}"
        );
    }
}

#[tokio::test]
async fn a_compliance_host_on_a_plain_endpoint_is_a_400() {
    for (base_url, host) in [
        ("https://api.anthropic.com", "anthropic.com"),
        ("https://chatgpt.com/backend-api/codex", "chatgpt.com"),
        ("https://res.openai.azure.com", "azure.com"),
    ] {
        let (status, body) = create(json!({
            "name": "t5-api-compliance",
            "dialect": "openai",
            "platform": "plain",
            "base_url": base_url,
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{base_url}: {body}");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(error.contains(host), "{base_url}: {error}");
        assert!(error.contains("docs/compliance.md"), "{base_url}: {error}");
    }
}

#[tokio::test]
async fn a_body_breaking_any_other_rule_is_a_400_naming_it() {
    for (body, says) in [
        (
            json!({"name": "t5-api-header", "dialect": "openai", "platform": "plain",
                   "base_url": "http://127.0.0.1:9/v1",
                   "extra_headers": {"X-Api-Key": "t5-not-a-key"}}),
            "extra header `x-api-key` is not allowed",
        ),
        (
            json!({"name": "t5-api-pair", "dialect": "anthropic", "platform": "azure",
                   "base_url": "https://res.openai.azure.com"}),
            "the azure platform does not serve the anthropic dialect",
        ),
        (
            json!({"name": "t5-api-region", "dialect": "anthropic", "platform": "aws"}),
            "the aws platform needs a region",
        ),
        (
            json!({"name": "gemini", "dialect": "gemini", "platform": "plain",
                   "base_url": "http://127.0.0.1:9"}),
            "is reserved",
        ),
    ] {
        let (status, answer) = create(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{says}: {answer}");
        let error = answer["error"].as_str().unwrap_or_default();
        assert!(error.contains(says), "{says}: {error}");
        assert!(!error.contains("t5-not-a-key"), "{error}");
    }
}

#[tokio::test]
async fn a_patch_naming_the_dialect_or_the_platform_is_a_400_before_anything_is_read() {
    // The database is a closed port: a handler that read the row first would
    // answer 500, not 400.
    for body in [
        json!({"dialect": "anthropic"}),
        json!({"platform": "azure"}),
        json!({"name": "t5-renamed", "display_name": "x"}),
        json!({"dialect": "openai", "base_url": "http://127.0.0.1:9/v1"}),
    ] {
        let state = crate::testing::state("");
        let (status, answer) = read(
            update_endpoint(
                State(state),
                actor(),
                Path("t5-api-fixed".to_owned()),
                Json(patch(&body)),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {answer}");
        assert_eq!(answer["error"], endpoints::FIXED, "{body}");
    }
}

#[test]
fn a_patch_tells_a_field_left_out_from_one_cleared() {
    let left = patch(&json!({}));
    assert!(!left.moves());
    assert_eq!(
        (left.base_url, left.display_name, left.auth),
        (None, None, None),
        "nothing sent, nothing changed"
    );

    let cleared = patch(&json!({"display_name": null, "api_version": null, "extra_headers": null}));
    assert_eq!(cleared.display_name, Some(None));
    assert_eq!(cleared.api_version, Some(None));
    assert_eq!(cleared.extra_headers, Some(None));

    let set = patch(&json!({"region": "eu-west-1", "discover_models": true}));
    assert_eq!(set.region, Some(Some("eu-west-1".to_owned())));
    assert_eq!(set.discover_models, Some(true));
    assert_eq!(set.path, None, "a path left out is kept");

    let moved = patch(&json!({"path": "/v1/decisions"}));
    assert_eq!(moved.path, Some(Some("/v1/decisions".to_owned())));
    assert_eq!(patch(&json!({"path": null})).path, Some(None));

    // A misspelt field is refused rather than silently ignored.
    assert!(
        serde_json::from_value::<EndpointPatch>(json!({"base_uri": "http://h"})).is_err(),
        "an unknown field is a mistake to report"
    );
    assert!(
        serde_json::from_value::<EndpointInput>(json!({
            "name": "t5", "dialect": "openai", "platform": "plain", "headers": {}
        }))
        .is_err()
    );
}

#[test]
fn a_patch_changes_exactly_the_fields_it_sends() {
    let stored = Draft {
        name: "t5-apply".to_owned(),
        dialect: "openai".to_owned(),
        platform: "plain".to_owned(),
        base_url: Some("http://127.0.0.1:9/v1".to_owned()),
        auth: "bearer".to_owned(),
        region: Some("eu-west-1".to_owned()),
        project: Some("acme".to_owned()),
        api_version: Some("v1".to_owned()),
        path: None,
        extra_headers: json!({"X-Old": "1"}),
        display_name: Some("Old".to_owned()),
        discover_models: false,
    };

    let mut untouched = stored.clone();
    patch(&json!({})).apply(&mut untouched);
    assert_eq!(untouched, stored);

    let mut changed = stored.clone();
    patch(&json!({
        "base_url": "http://127.0.0.1:10/v1",
        "auth": "x_api_key",
        "region": null,
        "project": "other",
        "api_version": null,
        "extra_headers": {"X-New": "2"},
        "display_name": "New",
        "discover_models": true,
    }))
    .apply(&mut changed);
    assert_eq!(
        changed,
        Draft {
            base_url: Some("http://127.0.0.1:10/v1".to_owned()),
            auth: "x_api_key".to_owned(),
            region: None,
            project: Some("other".to_owned()),
            api_version: None,
            path: None,
            extra_headers: json!({"X-New": "2"}),
            display_name: Some("New".to_owned()),
            discover_models: true,
            ..stored.clone()
        }
    );

    let mut moved = stored.clone();
    patch(&json!({"path": "/v1/decisions"})).apply(&mut moved);
    assert_eq!(moved.path.as_deref(), Some("/v1/decisions"));
    patch(&json!({"path": null})).apply(&mut moved);
    assert_eq!(moved.path, None, "null goes back to Jev's own path");

    let mut emptied = stored;
    patch(&json!({"extra_headers": null})).apply(&mut emptied);
    assert_eq!(
        emptied.extra_headers,
        json!({}),
        "null removes every header"
    );
}

#[test]
fn a_registration_takes_its_platforms_auth_unless_it_names_one() {
    for (platform, auth) in [
        ("plain", "bearer"),
        ("azure", "api_key_header"),
        ("gcp", "bearer"),
        ("aws", "none"),
        ("vertex", ""),
    ] {
        let draft =
            input(&json!({"name": "t5", "dialect": "openai", "platform": platform})).into_draft();
        assert_eq!(draft.auth, auth, "{platform}");
        assert_eq!(draft.extra_headers, json!({}), "{platform}");
    }
    let named = input(&json!({
        "name": "t5", "dialect": "anthropic", "platform": "plain", "auth": "x_api_key",
        "extra_headers": {"X-Project-Id": "p"}, "discover_models": true,
    }))
    .into_draft();
    assert_eq!(named.auth, "x_api_key");
    assert_eq!(named.extra_headers, json!({"X-Project-Id": "p"}));
    assert!(named.discover_models);
    assert_eq!(named.path, None);

    let decisions = input(&json!({
        "name": "t7", "dialect": "system_one", "platform": "plain",
        "base_url": "https://api-gateway.merge.example", "path": "/v1/decisions",
    }))
    .into_draft();
    assert_eq!(decisions.path.as_deref(), Some("/v1/decisions"));
}

#[test]
fn a_view_redacts_what_looks_secret_and_says_why_a_row_is_not_served() {
    let row = EndpointRow {
        name: "t5-view".to_owned(),
        dialect: "openai".to_owned(),
        // At an address, which no Azure resource is.
        platform: "azure".to_owned(),
        base_url: Some("https://10.0.0.7".to_owned()),
        auth: "api_key_header".to_owned(),
        region: None,
        project: None,
        api_version: None,
        path: None,
        extra_headers: json!({"X-Team": "core", "X-Session-Token": "t5-hidden"}),
        display_name: Some("Azure".to_owned()),
        discover_models: false,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    };
    let refs = EndpointReferences {
        accounts: 3,
        schedulable: 2,
        models: 5,
        on_ladder: 1,
    };
    let shown = serde_json::to_value(view(&row, refs)).expect("json");
    assert_eq!(
        shown["extra_headers"],
        json!({"X-Team": "core", "X-Session-Token": endpoints::REDACTED})
    );
    assert_eq!(
        (
            &shown["accounts"],
            &shown["schedulable_accounts"],
            &shown["models"],
            &shown["on_ladder"]
        ),
        (&json!(3), &json!(2), &json!(5), &json!(1))
    );
    assert_eq!(shown["served"], false);
    assert_eq!(shown["path"], Value::Null);
    assert!(
        shown["problem"]
            .as_str()
            .is_some_and(|p| p.contains("not an Azure resource's host")),
        "{shown}"
    );
    assert_eq!(shown["created_at"], "1970-01-01T00:00:00Z");
}

#[tokio::test]
async fn each_write_failure_has_its_status() {
    for (error, status) in [
        (
            WriteError::Invalid("bad".to_owned()),
            StatusCode::BAD_REQUEST,
        ),
        (WriteError::Taken("taken".to_owned()), StatusCode::CONFLICT),
        (WriteError::NotFound, StatusCode::NOT_FOUND),
        (WriteError::Changed, StatusCode::CONFLICT),
        (
            WriteError::Failed(oag_core::Error::Internal("down".to_owned())),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let (got, body) = read(write_failed(error)).await;
        assert_eq!(got, status, "{body}");
        assert!(body["error"].is_string(), "{body}");
    }
}

/// Somewhere for a log line to land, so a test can read it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn a_write_leaves_an_audit_line_naming_who_and_which_endpoint() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        audit(&actor(), "endpoint.create", "t5-audited");
    });
    let log = String::from_utf8_lossy(&captured.0.lock().expect("log")).into_owned();
    for says in [
        "oag::audit",
        "ops@example.invalid",
        "endpoint.create",
        "t5-audited",
    ] {
        assert!(log.contains(says), "{says}: {log}");
    }
}

/// A fresh endpoint name.
fn fresh() -> String {
    format!("t5a-{}", &uuid::Uuid::new_v4().simple().to_string()[..20])
}

/// Register, list, read, change, refuse to remove while in use, remove: the
/// whole life of one endpoint through the handlers, against a real database.
// Long because it is one endpoint's whole life; split, every part would need
// the registration the first one makes.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn an_endpoint_lives_its_whole_life_through_the_api() {
    let Some(state) = crate::testing::live_state().await else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
        return;
    };
    let name = fresh();
    let body = json!({
        "name": name,
        "dialect": "openai",
        "platform": "plain",
        "base_url": "http://127.0.0.1:9/v1/",
        "extra_headers": {"X-Project-Id": "p-1"},
        "display_name": "  Scratch  ",
    });

    let (status, created) =
        read(create_endpoint(State(Arc::clone(&state)), actor(), Json(input(&body))).await).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["base_url"], "http://127.0.0.1:9/v1",
        "stored normalised"
    );
    assert_eq!(created["display_name"], "Scratch");
    assert_eq!(created["auth"], "bearer", "the plain platform's default");
    assert_eq!(created["served"], true, "{created}");
    assert_eq!(created["accounts"], 0);
    let endpoint = EndpointRegistry::global()
        .get(&name)
        .expect("the write reloaded this replica, so the name resolves at once");
    assert!(
        state.adapter(Provider::Custom(endpoint)).is_ok(),
        "and it has an adapter"
    );

    let (status, again) =
        read(create_endpoint(State(Arc::clone(&state)), actor(), Json(input(&body))).await).await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    assert!(
        again["error"]
            .as_str()
            .is_some_and(|e| e.contains("already exists"))
    );

    let (status, listed) = read(list_endpoints(State(Arc::clone(&state))).await).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed
            .as_array()
            .is_some_and(|all| all.iter().any(|e| e["name"] == name.as_str())),
        "{listed}"
    );

    let (status, changed) = read(
        update_endpoint(
            State(Arc::clone(&state)),
            actor(),
            Path(name.clone()),
            Json(patch(&json!({
                "base_url": "http://127.0.0.1:10/v1",
                "display_name": null,
                "discover_models": true,
            }))),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["base_url"], "http://127.0.0.1:10/v1");
    assert_eq!(changed["display_name"], Value::Null);
    assert_eq!(changed["discover_models"], true);
    assert_eq!(
        changed["extra_headers"],
        json!({"X-Project-Id": "p-1"}),
        "a field not sent is kept"
    );
    assert_eq!(changed["dialect"], "openai");

    let (status, refused) = read(
        update_endpoint(
            State(Arc::clone(&state)),
            actor(),
            Path(name.clone()),
            Json(patch(&json!({"base_url": "https://api.x.ai/v1"}))),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    let (status, _) = read(
        update_endpoint(
            State(Arc::clone(&state)),
            actor(),
            Path(fresh()),
            Json(patch(&json!({"display_name": "x"}))),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A credential names it now, so it stays.
    sqlx::query(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce) \
         VALUES (gen_random_uuid(), $1, $1, 'api_key', '\\x00', '\\x00')",
    )
    .bind(&name)
    .execute(state.db.pool())
    .await
    .expect("a credential");
    let (status, read_back) =
        read(get_endpoint(State(Arc::clone(&state)), Path(name.clone())).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read_back["accounts"], 1, "{read_back}");

    let (status, in_use) =
        read(delete_endpoint(State(Arc::clone(&state)), actor(), Path(name.clone())).await).await;
    assert_eq!(status, StatusCode::CONFLICT, "{in_use}");
    assert_eq!(
        (&in_use["accounts"], &in_use["models"]),
        (&json!(1), &json!(0))
    );
    assert!(
        in_use["error"]
            .as_str()
            .is_some_and(|e| e.contains("nothing was removed")),
        "{in_use}"
    );

    sqlx::query("DELETE FROM account WHERE provider = $1")
        .bind(&name)
        .execute(state.db.pool())
        .await
        .expect("remove the credential");
    let (status, deleted) =
        read(delete_endpoint(State(Arc::clone(&state)), actor(), Path(name.clone())).await).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(
        EndpointRegistry::global().get(&name),
        None,
        "and the removal reloaded this replica too"
    );

    let (status, _) = read(get_endpoint(State(Arc::clone(&state)), Path(name.clone())).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) =
        read(delete_endpoint(State(Arc::clone(&state)), actor(), Path(name)).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_check_asks_the_stored_endpoint_and_reports_what_it_found() {
    let Some(state) = crate::testing::live_state().await else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "a"}, {"id": "b"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let name = fresh();
    let base_url = format!("{}/v1", server.uri());
    repo::insert_endpoint(
        &state.db,
        &oag_store::NewEndpoint {
            name: &name,
            dialect: "openai",
            platform: "plain",
            base_url: Some(&base_url),
            auth: "bearer",
            region: None,
            project: None,
            api_version: None,
            path: None,
            extra_headers: &json!({}),
            display_name: None,
            discover_models: false,
        },
    )
    .await
    .expect("an endpoint");

    let checked =
        read(check_endpoint(State(Arc::clone(&state)), actor(), Path(name.clone())).await).await;
    let missing =
        read(check_endpoint(State(Arc::clone(&state)), actor(), Path(fresh())).await).await;
    let removed = repo::delete_endpoint(&state.db, &name).await;

    let (status, body) = checked;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "name": name,
            "ok": true,
            "url": format!("{base_url}/models"),
            "status": 200,
            "models": 2,
            "more": false,
            "error": null,
        })
    );
    server.verify().await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    assert_eq!(removed.expect("delete"), EndpointDeletion::Deleted);
}
