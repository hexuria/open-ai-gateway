use super::*;
use oag_core::endpoint::Reason;
use serde_json::json;
use wiremock::matchers::{any, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A pool that dials nothing until it is asked: a write that reaches it fails
/// with a connection error, which is how these tests see that every rule in
/// front of the store passed.
fn nowhere() -> Db {
    Db::connect("postgres://oag:oag@127.0.0.1:1/oag", 1).expect("a lazy pool")
}

/// A plain OpenAI-dialect draft that every rule accepts, on the operator's
/// own network so no lookup leaves the machine.
fn draft(name: &str) -> Draft {
    Draft {
        name: name.to_owned(),
        dialect: "openai".to_owned(),
        platform: "plain".to_owned(),
        base_url: Some("http://127.0.0.1:9/v1".to_owned()),
        auth: "bearer".to_owned(),
        region: None,
        project: None,
        api_version: None,
        path: None,
        extra_headers: json!({"X-Project-Id": "p-1"}),
        display_name: None,
        discover_models: false,
    }
}

/// The row `draft` would be once stored.
fn row(draft: &Draft) -> EndpointRow {
    EndpointRow {
        name: draft.name.clone(),
        dialect: draft.dialect.clone(),
        platform: draft.platform.clone(),
        base_url: draft.base_url.clone(),
        auth: draft.auth.clone(),
        region: draft.region.clone(),
        project: draft.project.clone(),
        api_version: draft.api_version.clone(),
        path: draft.path.clone(),
        extra_headers: draft.extra_headers.clone(),
        display_name: draft.display_name.clone(),
        discover_models: draft.discover_models,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

#[test]
fn each_platform_defaults_to_the_one_auth_style_it_takes() {
    for (platform, auth) in [
        (Platform::Plain, AuthStyle::Bearer),
        (Platform::Azure, AuthStyle::ApiKeyHeader),
        (Platform::Gcp, AuthStyle::Bearer),
        (Platform::Aws, AuthStyle::None),
    ] {
        assert_eq!(default_auth(platform), auth, "{}", platform.as_str());
        assert!(auth.suits(platform), "{}", platform.as_str());
    }
}

#[test]
fn a_draft_is_stored_tidy() {
    let (stored, platform) = checked(Draft {
        base_url: Some("  http://127.0.0.1:8000/v1//  ".to_owned()),
        region: Some("   ".to_owned()),
        project: Some(String::new()),
        api_version: Some(" ".to_owned()),
        display_name: Some("  Merge Gateway  ".to_owned()),
        ..draft("t5-tidy")
    })
    .expect("a good draft");
    assert_eq!(platform, Platform::Plain);
    assert_eq!(
        stored.base_url.as_deref(),
        Some("http://127.0.0.1:8000/v1"),
        "normalised as the reload would"
    );
    assert_eq!(
        (stored.region, stored.project, stored.api_version),
        (None, None, None),
        "blank is no value at all"
    );
    assert_eq!(stored.display_name.as_deref(), Some("Merge Gateway"));

    let (aws, platform) = checked(Draft {
        dialect: "anthropic".to_owned(),
        platform: "aws".to_owned(),
        base_url: None,
        auth: "none".to_owned(),
        region: Some(" us-east-1 ".to_owned()),
        ..draft("t5-tidy-aws")
    })
    .expect("an aws draft needs no base URL");
    assert_eq!(platform, Platform::Aws);
    assert_eq!(aws.region.as_deref(), Some("us-east-1"));
    assert_eq!(aws.base_url, None);
}

#[test]
fn each_rule_refuses_a_draft_before_anything_is_asked() {
    let long = "x".repeat(129);
    for (bad, says) in [
        (
            Draft {
                name: "Merge".to_owned(),
                ..draft("")
            },
            "endpoint name `Merge` must be",
        ),
        (
            Draft {
                name: "openai".to_owned(),
                ..draft("")
            },
            "is reserved",
        ),
        (
            Draft {
                base_url: Some("https://api.openai.com/v1".to_owned()),
                ..draft("t5-compliance")
            },
            "openai.com",
        ),
        (
            Draft {
                base_url: Some("http://169.254.169.254/latest".to_owned()),
                ..draft("t5-metadata")
            },
            "link-local or cloud-metadata",
        ),
        (
            Draft {
                base_url: None,
                ..draft("t5-no-url")
            },
            "needs a base URL",
        ),
        (
            Draft {
                platform: "azure".to_owned(),
                base_url: Some("https://res.openai.azure.com".to_owned()),
                ..draft("t5-azure-bearer")
            },
            "auth `bearer` is not how the azure platform takes a key",
        ),
        (
            Draft {
                extra_headers: json!({"Authorization": "Bearer t5-not-a-key"}),
                ..draft("t5-auth-header")
            },
            "extra header `authorization` is not allowed",
        ),
        (
            Draft {
                extra_headers: json!({"x-ok": "split\r\nx-api-key: stolen"}),
                ..draft("t5-split-header")
            },
            "has a value no header can carry",
        ),
        (
            Draft {
                display_name: Some("two\nlines".to_owned()),
                ..draft("t5-name-lines")
            },
            "control characters",
        ),
        (
            Draft {
                display_name: Some(long),
                ..draft("t5-name-long")
            },
            "128 characters or fewer",
        ),
    ] {
        let refused = checked(bad).expect_err(says);
        assert!(refused.contains(says), "{says}: {refused}");
        assert!(!refused.contains("t5-not-a-key"), "{refused}");
    }
    // The limit is characters, not bytes.
    checked(Draft {
        display_name: Some("é".repeat(128)),
        ..draft("t5-name-accents")
    })
    .expect("128 characters of any script");
}

#[tokio::test]
async fn a_registration_is_refused_before_the_store_unless_every_rule_passes() {
    let db = nowhere();
    match register(
        &db,
        Draft {
            base_url: Some("https://api.anthropic.com".to_owned()),
            ..draft("t5-reg-compliance")
        },
    )
    .await
    {
        Err(WriteError::Invalid(message)) => assert!(message.contains("anthropic.com")),
        other => panic!("the compliance guard answers first: {other:?}"),
    }
    // RFC 6761 reserves `.invalid`: the lookup, not the rules, refuses it.
    match register(
        &db,
        Draft {
            base_url: Some("https://no-such-host.invalid/v1".to_owned()),
            ..draft("t5-reg-dns")
        },
    )
    .await
    {
        Err(WriteError::Invalid(message)) => {
            assert!(
                message.starts_with("resolving no-such-host.invalid"),
                "{message}"
            );
        }
        other => panic!("the name is resolved before it is stored: {other:?}"),
    }
    match register(&db, draft("t5-reg-good")).await {
        Err(WriteError::Failed(e)) => assert!(e.to_string().contains("registering endpoint")),
        other => panic!("a good draft reaches the store: {other:?}"),
    }
}

#[tokio::test]
async fn a_change_keeps_what_the_endpoint_is_and_resolves_only_a_new_url() {
    let db = nowhere();
    let stored = row(&Draft {
        base_url: Some("https://no-such-host.invalid/v1".to_owned()),
        ..draft("t5-change")
    });
    for moved in [
        Draft {
            dialect: "anthropic".to_owned(),
            ..Draft::from_row(&stored)
        },
        Draft {
            platform: "azure".to_owned(),
            ..Draft::from_row(&stored)
        },
        Draft {
            name: "t5-renamed".to_owned(),
            ..Draft::from_row(&stored)
        },
    ] {
        match change(&db, &stored, moved).await {
            Err(WriteError::Invalid(message)) => assert_eq!(message, FIXED),
            other => panic!("a change of what the endpoint is: {other:?}"),
        }
    }

    // The URL did not change, so a lookup that fails today does not stop a
    // new display name: the write reaches the store.
    let renamed = Draft {
        display_name: Some("Renamed".to_owned()),
        ..Draft::from_row(&stored)
    };
    match change(&db, &stored, renamed).await {
        Err(WriteError::Failed(e)) => assert!(e.to_string().contains("updating endpoint")),
        other => panic!("an unchanged URL is not resolved again: {other:?}"),
    }

    // A new URL is.
    let moved = Draft {
        base_url: Some("https://another-host.invalid/v1".to_owned()),
        ..Draft::from_row(&stored)
    };
    match change(&db, &stored, moved).await {
        Err(WriteError::Invalid(message)) => {
            assert!(
                message.starts_with("resolving another-host.invalid"),
                "{message}"
            );
        }
        other => panic!("a new URL is resolved: {other:?}"),
    }
}

#[test]
fn a_refused_store_write_is_answered_as_the_callers_mistake_or_the_databases() {
    match write_error(oag_core::Error::Config(
        "an endpoint named 'x' already exists".to_owned(),
    )) {
        WriteError::Taken(message) => assert!(message.contains("already exists")),
        other => panic!("{other:?}"),
    }
    match write_error(oag_core::Error::Config(
        "endpoint 'x' fails the database check endpoint_base_url_check".to_owned(),
    )) {
        WriteError::Invalid(message) => assert!(message.contains("endpoint_base_url_check")),
        other => panic!("{other:?}"),
    }
    match write_error(oag_core::Error::Internal("connection refused".to_owned())) {
        WriteError::Failed(e) => assert!(e.to_string().contains("connection refused")),
        other => panic!("{other:?}"),
    }

    let missing = WriteError::NotFound.into_error("merge").to_string();
    assert!(missing.contains("no endpoint named 'merge'"), "{missing}");
    for said in [
        WriteError::Invalid("bad".to_owned()),
        WriteError::Taken("bad".to_owned()),
    ] {
        assert!(matches!(said.into_error("merge"), oag_core::Error::Config(m) if m == "bad"));
    }
    assert!(matches!(
        WriteError::Failed(oag_core::Error::Internal("down".to_owned())).into_error("merge"),
        oag_core::Error::Internal(m) if m == "down"
    ));
}

#[test]
fn the_reloads_answer_is_asked_of_one_row() {
    assert_eq!(refusal(&row(&draft("t5-served"))), None);
    let azure = row(&Draft {
        platform: "azure".to_owned(),
        base_url: Some("https://res.openai.azure.com".to_owned()),
        auth: "api_key_header".to_owned(),
        ..draft("t5-unserved-azure")
    });
    let unsupported = refusal(&azure).expect("a valid row this build has no adapter for");
    assert_eq!(unsupported.reason, Reason::Unsupported);
    assert!(
        unsupported
            .message
            .starts_with("endpoint `t5-unserved-azure` is on the azure platform"),
        "the words an operator reads, with no `configuration: ` before them: {unsupported}"
    );
    let compliance = row(&Draft {
        base_url: Some("https://api.x.ai/v1".to_owned()),
        ..draft("t5-unserved-xai")
    });
    assert_eq!(
        refusal(&compliance).map(|r| r.reason),
        Some(Reason::Compliance)
    );
}

#[test]
fn a_header_whose_name_suggests_a_secret_is_shown_redacted() {
    let shown = shown_headers(&json!({
        "X-Project-Id": "p-1",
        "X-Api-Key-Id": "t5-hidden-1",
        "X-Session-Token": "t5-hidden-2",
        "Client-SECRET": "t5-hidden-3",
        "X-Auth-Scheme": "t5-hidden-4",
        "X-Retries": 3,
    }));
    assert_eq!(
        shown,
        [
            ("X-Project-Id".to_owned(), "p-1".to_owned()),
            ("X-Api-Key-Id".to_owned(), REDACTED.to_owned()),
            ("X-Session-Token".to_owned(), REDACTED.to_owned()),
            ("Client-SECRET".to_owned(), REDACTED.to_owned()),
            ("X-Auth-Scheme".to_owned(), REDACTED.to_owned()),
            ("X-Retries".to_owned(), "3".to_owned()),
        ],
        "every name, in stored order; a value only where the name is harmless"
    );
    assert!(shown_headers(&json!(["x"])).is_empty());
}

#[test]
fn a_list_is_counted_in_each_vendors_shape() {
    for (body, counted) in [
        (
            json!({"object": "list", "data": [{"id": "a"}, {"id": "b"}]}),
            (2, false),
        ),
        (json!({"data": [{"id": "a"}], "has_more": true}), (1, true)),
        (json!({"data": [], "has_more": false}), (0, false)),
        (
            json!({"models": [{"name": "models/g"}], "nextPageToken": "next"}),
            (1, true),
        ),
        (
            json!({"models": [{"name": "m"}], "nextPageToken": ""}),
            (1, false),
        ),
        (json!([{"id": "a"}, {"id": "b"}, {"id": "c"}]), (3, false)),
    ] {
        let bytes = serde_json::to_vec(&body).expect("json");
        assert_eq!(count(&bytes), Ok(counted), "{body}");
    }
    let not_json = count(b"<html>").expect_err("not JSON");
    assert!(not_json.starts_with("the answer is not JSON"), "{not_json}");
    let not_a_list = count(br#"{"data": {"id": "a"}}"#).expect_err("no list");
    assert!(not_a_list.contains("not a model list"), "{not_a_list}");
}

/// A stored plain endpoint whose base URL is `base`.
fn at(name: &str, dialect: &str, auth: &str, base: String) -> EndpointRow {
    row(&Draft {
        dialect: dialect.to_owned(),
        auth: auth.to_owned(),
        base_url: Some(base),
        ..draft(name)
    })
}

#[tokio::test]
async fn a_check_asks_the_list_without_a_key_and_counts_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-project-id", "p-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object": "list", "data": [{"id": "a"}, {"id": "b"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let checked = check(
        &at(
            "t5-check",
            "openai",
            "bearer",
            format!("{}/v1", server.uri()),
        ),
        None,
        None,
    )
    .await;

    assert_eq!(
        checked,
        Checked {
            url: Some(format!("{}/v1/models", server.uri())),
            status: Some(200),
            models: Some(2),
            more: false,
            error: None,
        }
    );
    assert!(checked.ok());
    server.verify().await;
    let asked = &server.received_requests().await.expect("recorded")[0];
    for key_header in ["authorization", "x-api-key", "x-goog-api-key", "api-key"] {
        assert!(
            asked.headers.get(key_header).is_none(),
            "no key was given, so none went: {key_header}"
        );
    }
}

#[tokio::test]
async fn a_check_with_a_key_sends_it_where_the_endpoint_names() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "t5-check-key"))
        .and(header(
            "anthropic-version",
            oag_proto::anthropic::API_VERSION,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": [{"id": "a"}, {"id": "b"}, {"id": "c"}], "has_more": true}),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let checked = check(
        &at("t5-check-keyed", "anthropic", "x_api_key", server.uri()),
        Some("t5-check-key"),
        None,
    )
    .await;

    assert_eq!(
        (checked.models, checked.more),
        (Some(3), true),
        "{checked:?}"
    );
    server.verify().await;
}

#[tokio::test]
async fn a_refusal_is_reported_with_its_status() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let row = at("t5-check-401", "openai", "bearer", server.uri());

    let bare = check(&row, None, None).await;
    assert_eq!((bare.status, bare.models), (Some(401), None));
    assert_eq!(
        bare.error.as_deref(),
        Some("answered 401 Unauthorized without a key")
    );
    assert!(!bare.ok());

    let keyed = check(&row, Some("t5-check-key"), None).await;
    assert_eq!(keyed.error.as_deref(), Some("answered 401 Unauthorized"));
}

#[tokio::test]
async fn a_redirect_is_reported_and_never_followed() {
    let elsewhere = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&elsewhere)
        .await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/models?stolen=1", elsewhere.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;

    let checked = check(
        &at("t5-check-307", "openai", "bearer", server.uri()),
        Some("t5-check-key"),
        None,
    )
    .await;

    elsewhere.verify().await;
    server.verify().await;
    assert_eq!(checked.status, Some(307));
    let error = checked.error.expect("a redirect is not a list");
    assert!(
        error.contains(&format!("a redirect to {}", elsewhere.uri())),
        "the origin it pointed at: {error}"
    );
    assert!(!error.contains("stolen"), "and no more of it: {error}");
    assert!(error.contains("never follows"), "{error}");
}

#[tokio::test]
async fn an_answer_that_is_not_a_list_or_no_answer_is_a_failed_check() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>welcome</html>"))
        .mount(&server)
        .await;
    let html = check(
        &at("t5-check-html", "gemini", "bearer", server.uri()),
        None,
        None,
    )
    .await;
    assert_eq!(html.status, Some(200));
    assert!(
        html.error
            .as_deref()
            .is_some_and(|e| e.starts_with("the answer is not JSON")),
        "{html:?}"
    );

    // Nothing listens on port 1.
    let refused = check(
        &at(
            "t5-check-down",
            "openai",
            "bearer",
            "http://127.0.0.1:1".to_owned(),
        ),
        None,
        None,
    )
    .await;
    assert_eq!(refused.status, None);
    assert_eq!(refused.url.as_deref(), Some("http://127.0.0.1:1/models"));
    let why = refused.error.as_deref().unwrap_or_default();
    assert!(why.starts_with("no answer: "), "{refused:?}");
    assert!(
        why.contains("Connection refused"),
        "the cause under reqwest's own words, which name only the URL: {why}"
    );

    let proxied = check(
        &at("t5-check-proxy", "openai", "bearer", server.uri()),
        None,
        Some("::not a proxy"),
    )
    .await;
    assert!(
        proxied
            .error
            .as_deref()
            .is_some_and(|e| e.contains("proxy_url is unusable")),
        "{proxied:?}"
    );
    // A blank proxy is no proxy, as a blank column is.
    let unproxied = check(
        &at("t5-check-proxy", "openai", "bearer", server.uri()),
        None,
        Some("  "),
    )
    .await;
    assert_eq!(unproxied.status, Some(200), "{unproxied:?}");
}

#[tokio::test]
async fn a_check_gives_up_after_its_timeout() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": []}))
                .set_delay(CHECK_TIMEOUT + Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    let slow = check(
        &at("t5-check-slow", "openai", "bearer", server.uri()),
        None,
        None,
    )
    .await;
    assert_eq!(
        (slow.status, slow.error.as_deref()),
        (None, Some("no answer within 10s"))
    );
}

#[tokio::test]
async fn an_endpoint_that_cannot_be_asked_says_why_and_asks_nothing() {
    let aws = row(&Draft {
        dialect: "anthropic".to_owned(),
        platform: "aws".to_owned(),
        base_url: None,
        auth: "none".to_owned(),
        region: Some("us-east-1".to_owned()),
        ..draft("t5-check-aws")
    });
    let checked = check(&aws, None, None).await;
    assert_eq!(checked.url, None);
    assert!(
        checked
            .error
            .as_deref()
            .is_some_and(|e| e.contains("only plain endpoints are")),
        "{checked:?}"
    );

    let broken = row(&Draft {
        base_url: Some("https://api.openai.com/v1".to_owned()),
        ..draft("t5-check-broken")
    });
    let checked = check(&broken, None, None).await;
    assert_eq!(checked.url, None);
    assert!(
        checked
            .error
            .as_deref()
            .is_some_and(|e| e.contains("openai.com")),
        "{checked:?}"
    );
}

#[tokio::test]
async fn a_check_reads_a_list_up_to_its_limit_and_no_further() {
    // `{"data":[],"p":"…"}` padded to exactly `size` bytes.
    let padded = |size: usize| {
        let frame = r#"{"data":[],"p":""}"#.len();
        format!(r#"{{"data":[],"p":"{}"}}"#, "x".repeat(size - frame))
    };
    for (size, fits) in [(MOST_READ, true), (MOST_READ + 1, false)] {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(padded(size)))
            .mount(&server)
            .await;
        let checked = check(
            &at("t5-check-big", "openai", "bearer", server.uri()),
            None,
            None,
        )
        .await;
        if fits {
            assert_eq!(checked.models, Some(0), "{:?}", checked.error);
        } else {
            assert!(
                checked
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("larger than 8 MiB")),
                "{:?}",
                checked.error
            );
        }
    }
}
