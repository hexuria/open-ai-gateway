use super::*;
use crate::admin::tests::ENDPOINT_ROWS;
use crate::admin::{AdminCommand, CatalogCommand};
use clap::Parser;
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Parser, Debug)]
#[command(name = "admin")]
struct Cli {
    #[command(subcommand)]
    cmd: AdminCommand,
}

fn parsed(args: &[&str]) -> AdminCommand {
    Cli::try_parse_from(std::iter::once("admin").chain(args.iter().copied()))
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
        .cmd
}

fn endpoint(args: &[&str]) -> EndpointCommand {
    let mut words = vec!["endpoint"];
    words.extend_from_slice(args);
    match parsed(&words) {
        AdminCommand::Endpoint(cmd) => cmd,
        other => panic!("expected an endpoint command, got {other:?}"),
    }
}

fn add_args(args: &[&str]) -> EndpointAddArgs {
    let mut words = vec!["add"];
    words.extend_from_slice(args);
    match endpoint(&words) {
        EndpointCommand::Add { args } => args,
        other => panic!("expected endpoint add, got {other:?}"),
    }
}

fn set_args(args: &[&str]) -> EndpointSetArgs {
    let mut words = vec!["set", "merge"];
    words.extend_from_slice(args);
    match endpoint(&words) {
        EndpointCommand::Set { args } => args,
        other => panic!("expected endpoint set, got {other:?}"),
    }
}

/// The row a plain Merge-style endpoint is stored as.
fn stored() -> EndpointRow {
    EndpointRow {
        name: "merge".to_owned(),
        dialect: "openai".to_owned(),
        platform: "plain".to_owned(),
        base_url: Some("https://api-gateway.merge.example/v1/openai".to_owned()),
        auth: "bearer".to_owned(),
        region: None,
        project: None,
        api_version: None,
        path: None,
        extra_headers: json!({"X-Project-Id": "p-1", "X-Session-Token": "t5-hidden"}),
        display_name: Some("Merge".to_owned()),
        discover_models: false,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

#[test]
fn an_added_endpoint_takes_its_platforms_auth_unless_it_names_one() {
    for (platform, auth) in [
        ("plain", "bearer"),
        ("azure", "api_key_header"),
        ("gcp", "bearer"),
        ("aws", "none"),
    ] {
        let draft = draft(add_args(&[
            "--name",
            "t5",
            "--dialect",
            "openai",
            "--platform",
            platform,
        ]))
        .expect("a draft");
        assert_eq!(
            (draft.platform.as_str(), draft.auth.as_str()),
            (platform, auth)
        );
        assert_eq!(draft.extra_headers, json!({}));
        assert!(!draft.discover_models);
    }
    let draft = draft(add_args(&[
        "--name",
        "merge",
        "--dialect",
        "system_one",
        "--platform",
        "plain",
        "--auth",
        "x_api_key",
        "--base-url",
        "http://127.0.0.1:9",
        "--header",
        "X-Project-Id=p-1",
        "--header",
        "X-Title=oag",
        "--region",
        "r",
        "--project",
        "p",
        "--api-version",
        "v",
        "--display-name",
        "Merge",
        "--discover",
    ]))
    .expect("a draft");
    assert_eq!(
        draft,
        Draft {
            name: "merge".to_owned(),
            dialect: "system_one".to_owned(),
            platform: "plain".to_owned(),
            base_url: Some("http://127.0.0.1:9".to_owned()),
            auth: "x_api_key".to_owned(),
            region: Some("r".to_owned()),
            project: Some("p".to_owned()),
            api_version: Some("v".to_owned()),
            path: None,
            extra_headers: json!({"X-Project-Id": "p-1", "X-Title": "oag"}),
            display_name: Some("Merge".to_owned()),
            discover_models: true,
        }
    );
}

#[test]
fn a_header_is_its_name_and_everything_after_the_first_equals_sign() {
    let pairs =
        |raw: &[&str]| parse_headers(&raw.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
    assert_eq!(
        pairs(&[
            "X-Project-Id=abc",
            "X-Sig=a=b",
            " X-Space =  v ",
            "X-Empty="
        ])
        .expect("headers"),
        [
            ("X-Project-Id".to_owned(), json!("abc")),
            ("X-Sig".to_owned(), json!("a=b")),
            ("X-Space".to_owned(), json!("v")),
            ("X-Empty".to_owned(), json!("")),
        ]
    );
    for (raw, says) in [
        (&["t5-no-equals-sign"][..], "has no `=`"),
        (&["=t5-value"][..], "no name before its `=`"),
        (
            &["X-A=1", "x-a=t5-value"][..],
            "header `x-a` is given twice",
        ),
    ] {
        let err = pairs(raw).expect_err(says).to_string();
        assert!(err.contains(says), "{raw:?}: {err}");
        assert!(!err.contains("t5-"), "a value is never echoed: {err}");
    }
}

#[test]
fn a_set_changes_only_what_it_names() {
    let mut draft = Draft::from_row(&stored());
    let err = apply(&mut draft, set_args(&[])).expect_err("nothing named");
    assert!(err.to_string().contains("nothing to change"), "{err}");
    assert_eq!(draft, Draft::from_row(&stored()), "and nothing changed");

    let mut changed = Draft::from_row(&stored());
    apply(
        &mut changed,
        set_args(&[
            "--base-url",
            "http://127.0.0.1:10/v1",
            "--auth",
            "none",
            "--header",
            "x-project-id=p-2",
            "--header",
            "X-New=1",
            "--unset-header",
            "x-session-token",
            "--region",
            "eu-west-1",
            "--project",
            "acme",
            "--api-version",
            "2024-10-21",
            "--display-name",
            "",
            "--discover",
        ]),
    )
    .expect("a change");
    assert_eq!(
        changed,
        Draft {
            base_url: Some("http://127.0.0.1:10/v1".to_owned()),
            auth: "none".to_owned(),
            region: Some("eu-west-1".to_owned()),
            project: Some("acme".to_owned()),
            api_version: Some("2024-10-21".to_owned()),
            // Blank, which the shared rules store as none.
            display_name: Some(String::new()),
            extra_headers: json!({"x-project-id": "p-2", "X-New": "1"}),
            discover_models: true,
            ..Draft::from_row(&stored())
        },
        "a header of any case replaces the one stored, and an unset one goes"
    );

    let mut off = Draft {
        discover_models: true,
        ..Draft::from_row(&stored())
    };
    apply(&mut off, set_args(&["--discover", "false"])).expect("a change");
    assert!(!off.discover_models);
    assert_eq!(
        off.extra_headers,
        stored().extra_headers,
        "headers not named stay"
    );

    let err = apply(
        &mut Draft::from_row(&stored()),
        set_args(&["--unset-header", "X-Missing"]),
    )
    .expect_err("nothing to unset");
    assert!(
        err.to_string().contains("sends no header `X-Missing`"),
        "{err}"
    );
}

#[test]
fn a_listing_names_every_setting_and_counts_what_names_each_endpoint() {
    assert_eq!(
        list_lines(&[], &HashMap::new()),
        ["no endpoints; register one with `oag admin endpoint add`"]
    );

    let gcp = EndpointRow {
        name: "t5-gcp".to_owned(),
        dialect: "gemini".to_owned(),
        platform: "gcp".to_owned(),
        base_url: None,
        region: Some("us-central1".to_owned()),
        project: Some("acme".to_owned()),
        extra_headers: json!({}),
        ..stored()
    };
    let aws = EndpointRow {
        name: "t5-aws".to_owned(),
        dialect: "anthropic".to_owned(),
        platform: "aws".to_owned(),
        base_url: None,
        auth: "none".to_owned(),
        region: Some("us-east-1".to_owned()),
        extra_headers: json!({}),
        ..stored()
    };
    let refs = HashMap::from([(
        "merge".to_owned(),
        EndpointReferences {
            accounts: 2,
            schedulable: 1,
            models: 7,
            on_ladder: 3,
        },
    )]);
    // At an address, which no Azure resource is.
    let azure = EndpointRow {
        name: "t5-azure".to_owned(),
        platform: "azure".to_owned(),
        base_url: Some("https://10.0.0.7".to_owned()),
        auth: "api_key_header".to_owned(),
        extra_headers: json!({}),
        ..stored()
    };
    let lines = list_lines(&[stored(), gcp, azure, aws], &refs);
    assert!(lines[0].starts_with("NAME"), "{lines:?}");
    assert!(
        lines[1].starts_with("merge ")
            && lines[1].contains("openai")
            && lines[1].contains("bearer")
            && lines[1]
                .ends_with("  2      7         3  https://api-gateway.merge.example/v1/openai"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with("header X-Project-Id: p-1"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with("header X-Session-Token: <redacted>")),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("t5-hidden")), "{lines:?}");
    let unserved: Vec<_> = lines.iter().filter(|l| l.contains("not served")).collect();
    assert!(
        unserved.len() == 1 && unserved[0].contains("10.0.0.7 is not an Azure resource's host"),
        "the azure row alone is not served: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("t5-aws") && l.ends_with("region us-east-1")),
        "{lines:?}"
    );
}

#[test]
fn show_prints_every_setting_and_whether_it_is_served() {
    let refs = EndpointReferences {
        accounts: 2,
        schedulable: 1,
        models: 7,
        on_ladder: 3,
    };
    let lines = show_lines(&stored(), refs);
    for line in [
        "name          merge",
        "display name  Merge",
        "dialect       openai",
        "platform      plain",
        "base url      https://api-gateway.merge.example/v1/openai",
        "auth          bearer",
        "region        -",
        "discover      no",
        "headers       X-Project-Id: p-1",
        "              X-Session-Token: <redacted>",
        "accounts      2 (1 in rotation)",
        "models        7",
        "on a ladder   3",
        "served        yes",
    ] {
        assert!(lines.iter().any(|l| l == line), "{line:?} in {lines:?}");
    }
    let bare = show_lines(
        &EndpointRow {
            extra_headers: json!({}),
            base_url: Some("https://api.openai.com/v1".to_owned()),
            discover_models: true,
            ..stored()
        },
        EndpointReferences::default(),
    );
    assert!(bare.iter().any(|l| l == "headers       -"), "{bare:?}");
    assert!(bare.iter().any(|l| l == "discover      yes"), "{bare:?}");
    assert!(
        bare.iter()
            .any(|l| l.starts_with("served        no: ") && l.contains("openai.com")),
        "{bare:?}"
    );
}

#[test]
fn an_endpoint_in_use_names_what_holds_it_and_how_to_clear_each() {
    let both = in_use_lines("merge", 2, 3).join("\n");
    for says in [
        "still named by 2 credential(s) and 3 catalog model(s)",
        "DELETE FROM account WHERE provider = 'merge'",
        "DELETE FROM model_catalog WHERE provider = 'merge'",
        "take them off every ladder",
        "then: oag admin endpoint remove merge",
    ] {
        assert!(both.contains(says), "{says}: {both}");
    }
    let keys = in_use_lines("merge", 1, 0).join("\n");
    assert!(keys.contains("DELETE FROM account"), "{keys}");
    assert!(!keys.contains("DELETE FROM model_catalog"), "{keys}");
    let models = in_use_lines("merge", 0, 1).join("\n");
    assert!(!models.contains("DELETE FROM account"), "{models}");
    assert!(models.contains("DELETE FROM model_catalog"), "{models}");
}

#[test]
fn a_check_says_what_was_asked_with_which_key_and_what_came_back() {
    let listed = Checked {
        url: Some("http://h/v1/models".to_owned()),
        status: Some(200),
        models: Some(57),
        more: false,
        error: None,
    };
    assert_eq!(
        check_lines(&listed, None),
        [
            "GET http://h/v1/models (no key)",
            "  answered 200: 57 model(s) listed"
        ]
    );
    assert_eq!(
        check_lines(
            &Checked {
                more: true,
                ..listed.clone()
            },
            Some("merge-1")
        ),
        [
            "GET http://h/v1/models (the key of merge-1)",
            "  answered 200: 57 model(s) listed, and more on pages not read"
        ]
    );

    let refused = Checked {
        models: None,
        status: Some(401),
        error: Some("answered 401 Unauthorized without a key".to_owned()),
        ..listed.clone()
    };
    let lines = check_lines(&refused, None);
    assert_eq!(
        lines[1],
        "  failed: answered 401 Unauthorized without a key"
    );
    assert!(lines[2].contains("--account <name>"), "{lines:?}");
    assert_eq!(
        check_lines(&refused, Some("merge-1")).len(),
        2,
        "no hint to use a key when one was used"
    );

    let unasked = Checked {
        url: None,
        status: None,
        models: None,
        more: false,
        error: Some("only plain endpoints are".to_owned()),
    };
    assert_eq!(
        check_lines(&unasked, None),
        ["nothing was asked", "  failed: only plain endpoints are"]
    );
}

#[test]
fn adding_says_what_was_registered_and_what_is_left_to_do() {
    let lines = added_lines(&stored()).join("\n");
    for says in [
        "endpoint merge: openai on plain at https://api-gateway.merge.example/v1/openai",
        "auth bearer; headers X-Project-Id, X-Session-Token",
        "oag admin account add --name merge-1 --provider merge --secret <key>",
        "oag admin catalog add --id merge/<model>",
        "next catalog refresh",
    ] {
        assert!(lines.contains(says), "{says}: {lines}");
    }
    assert!(
        !lines.contains("p-1"),
        "header values stay out of it: {lines}"
    );
    assert!(!lines.contains("not served"), "{lines}");

    let vertex = added_lines(&EndpointRow {
        platform: "gcp".to_owned(),
        dialect: "gemini".to_owned(),
        base_url: None,
        region: Some("us-central1".to_owned()),
        project: Some("acme".to_owned()),
        extra_headers: json!({}),
        ..stored()
    })
    .join("\n");
    assert!(
        vertex.contains("at project acme in us-central1"),
        "{vertex}"
    );
    assert!(vertex.contains("auth bearer; no extra headers"), "{vertex}");
    assert!(!vertex.contains("not served"), "{vertex}");

    // At an address, which no Azure resource is.
    let unserved = added_lines(&EndpointRow {
        platform: "azure".to_owned(),
        base_url: Some("https://10.0.0.7".to_owned()),
        auth: "api_key_header".to_owned(),
        extra_headers: json!({}),
        ..stored()
    })
    .join("\n");
    assert!(
        unserved.contains("not served by this build") && unserved.contains("kept for a build"),
        "{unserved}"
    );
}

/// A pool on the test database, migrated, or `None` when there is none.
async fn test_db() -> Option<Db> {
    let url = std::env::var("OAG_TEST_DATABASE_URL").ok()?;
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

fn kek() -> Kek {
    Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek")
}

/// The least config `endpoint_cmd` takes; of its verbs only `sync` reads it.
fn config() -> Config {
    Config::from_yaml(
        "database:\n  url: \"postgres://oag:oag@127.0.0.1:1/oag_g0\"\nredis:\n  url: \
         \"redis://127.0.0.1:1\"\nsecurity:\n  signing_secret: \
         \"Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0=\"\n  credential_kek: \
         \"MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=\"\n",
    )
    .expect("a minimal config")
}

async fn run(db: &Db, args: &[&str]) -> Result<()> {
    match parsed(args) {
        AdminCommand::Endpoint(cmd) => endpoint_cmd(db, &kek(), &config(), cmd).await,
        AdminCommand::Catalog(cmd) => super::super::catalog::catalog_cmd(db, &kek(), cmd).await,
        other => panic!("not an endpoint or catalog command: {other:?}"),
    }
}

fn fresh() -> String {
    format!("t5c-{}", &Uuid::new_v4().simple().to_string()[..20])
}

/// Each endpoint command, and `catalog add`, hands back what the database
/// said: against one nothing answers, every one fails. A command that printed
/// and answered `Ok` without asking would pass for one that works.
#[tokio::test]
async fn every_endpoint_command_surfaces_the_databases_failure() {
    let db = Db::connect("postgres://oag:oag@127.0.0.1:1/oag_g0", 1).expect("lazy pool");
    let (list, show, set, remove, check, add, model) = tokio::join!(
        run(&db, &["endpoint", "list"]),
        run(&db, &["endpoint", "show", "merge"]),
        run(&db, &["endpoint", "set", "merge", "--discover"]),
        run(&db, &["endpoint", "remove", "merge"]),
        run(&db, &["endpoint", "check", "merge"]),
        run(
            &db,
            &[
                "endpoint",
                "add",
                "--name",
                "merge",
                "--dialect",
                "openai",
                "--platform",
                "plain",
                "--base-url",
                "http://127.0.0.1:9/v1",
            ],
        ),
        run(
            &db,
            &[
                "catalog",
                "add",
                "--id",
                "xai/grok-t5",
                "--upstream",
                "grok-t5",
                "--input-per-mtok",
                "1",
                "--output-per-mtok",
                "2",
                "--context",
                "1000",
                "--max-output",
                "100",
            ],
        ),
    );
    for (verb, outcome) in [
        ("list", list),
        ("show", show),
        ("set", set),
        ("remove", remove),
        ("check", check),
        ("add", add),
        ("catalog add", model),
    ] {
        outcome.expect_err(verb);
    }
}

/// Add, refuse, change, give it a model, refuse to remove it while the model
/// names it, remove it: an endpoint's life through the CLI, against a real
/// database.
// Long because it is one endpoint's whole life, each step needing the last.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn an_endpoint_is_added_changed_and_removed_from_the_cli() {
    let Some(db) = test_db().await else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let _endpoints = ENDPOINT_ROWS.lock().await;
    let (name, refused) = (fresh(), fresh());

    run(
        &db,
        &[
            "endpoint",
            "add",
            "--name",
            &name,
            "--dialect",
            "openai",
            "--platform",
            "plain",
            "--base-url",
            "http://127.0.0.1:9/v1/",
            "--header",
            "X-Project-Id=p-1",
            "--display-name",
            " Scratch ",
        ],
    )
    .await
    .expect("added");
    let row = repo::get_endpoint(&db, &name)
        .await
        .expect("read")
        .expect("stored");
    assert_eq!(row.base_url.as_deref(), Some("http://127.0.0.1:9/v1"));
    assert_eq!(row.auth, "bearer");
    assert_eq!(row.extra_headers, json!({"X-Project-Id": "p-1"}));
    assert_eq!(row.display_name.as_deref(), Some("Scratch"));

    let again = run(
        &db,
        &[
            "endpoint",
            "add",
            "--name",
            &name,
            "--dialect",
            "openai",
            "--platform",
            "plain",
            "--base-url",
            "http://127.0.0.1:9/v1",
        ],
    )
    .await
    .expect_err("the name is taken");
    assert!(again.to_string().contains("already exists"), "{again}");

    let compliance = run(
        &db,
        &[
            "endpoint",
            "add",
            "--name",
            &refused,
            "--dialect",
            "anthropic",
            "--platform",
            "plain",
            "--base-url",
            "https://api.anthropic.com",
        ],
    )
    .await
    .expect_err("a compliance host");
    assert!(
        compliance.to_string().contains("anthropic.com"),
        "{compliance}"
    );
    assert!(
        repo::get_endpoint(&db, &refused)
            .await
            .expect("read")
            .is_none(),
        "nothing was stored"
    );

    run(
        &db,
        &[
            "endpoint",
            "set",
            &name,
            "--base-url",
            "http://127.0.0.1:10/v1",
            "--unset-header",
            "x-project-id",
            "--discover",
        ],
    )
    .await
    .expect("changed");
    let row = repo::get_endpoint(&db, &name)
        .await
        .expect("read")
        .expect("stored");
    assert_eq!(row.base_url.as_deref(), Some("http://127.0.0.1:10/v1"));
    assert_eq!(row.extra_headers, json!({}));
    assert!(row.discover_models);
    assert_eq!(
        row.display_name.as_deref(),
        Some("Scratch"),
        "not named, kept"
    );

    let missing = run(&db, &["endpoint", "set", &fresh(), "--discover"])
        .await
        .expect_err("no such endpoint");
    assert!(
        missing.to_string().contains("no endpoint named"),
        "{missing}"
    );

    // A model whose id holds more slashes than the one after the provider.
    let model = format!("{name}/zai/glm-5.3-flash");
    run(
        &db,
        &[
            "catalog",
            "add",
            "--id",
            &model,
            "--upstream",
            "zai/glm-5.3-flash",
            "--input-per-mtok",
            "0.1",
            "--output-per-mtok",
            "0.4",
            "--context",
            "128000",
            "--max-output",
            "8192",
            "--tools",
        ],
    )
    .await
    .expect("a model under the endpoint");
    let (provider, upstream, tools, is_override): (String, String, bool, bool) = sqlx::query_as(
        "SELECT provider, upstream_name, supports_tools, is_override FROM model_catalog \
         WHERE id = $1",
    )
    .bind(&model)
    .fetch_one(db.pool())
    .await
    .expect("the model");
    assert_eq!(
        (provider.as_str(), upstream.as_str(), tools, is_override),
        (name.as_str(), "zai/glm-5.3-flash", true, true)
    );

    // The same id again rewrites the row, override and all: the operator
    // correcting their own row is not skipped as somebody else's override.
    run(
        &db,
        &[
            "catalog",
            "add",
            "--id",
            &model,
            "--upstream",
            "zai/glm-5.3-flash-0925",
            "--input-per-mtok",
            "0.2",
            "--output-per-mtok",
            "0.8",
            "--context",
            "64000",
            "--max-output",
            "4096",
            "--display-label",
            "GLM Flash",
        ],
    )
    .await
    .expect("the model restated");
    let restated: (
        String,
        rust_decimal::Decimal,
        i32,
        bool,
        Option<String>,
        bool,
    ) = sqlx::query_as(
        "SELECT upstream_name, input_per_mtok, context_window, supports_tools, \
             display_label, is_override FROM model_catalog WHERE id = $1",
    )
    .bind(&model)
    .fetch_one(db.pool())
    .await
    .expect("the model");
    assert_eq!(
        restated,
        (
            "zai/glm-5.3-flash-0925".to_owned(),
            rust_decimal::Decimal::new(2, 1),
            64_000,
            false,
            Some("GLM Flash".to_owned()),
            true
        ),
        "every column is the second write's"
    );

    let in_use = run(&db, &["endpoint", "remove", &name])
        .await
        .expect_err("a model names it");
    assert!(in_use.to_string().contains("in use"), "{in_use}");
    assert!(
        repo::get_endpoint(&db, &name)
            .await
            .expect("read")
            .is_some(),
        "and it is still there"
    );

    sqlx::query("DELETE FROM model_catalog WHERE id = $1")
        .bind(&model)
        .execute(db.pool())
        .await
        .expect("remove the model");
    run(&db, &["endpoint", "remove", &name])
        .await
        .expect("removed");
    assert!(
        repo::get_endpoint(&db, &name)
            .await
            .expect("read")
            .is_none()
    );
    let gone = run(&db, &["endpoint", "remove", &name])
        .await
        .expect_err("already gone");
    assert!(gone.to_string().contains("no endpoint named"), "{gone}");
}

/// `check --account` sends that credential's key, opened from its sealed row,
/// in the header the endpoint names; without it, no key goes and the upstream's
/// 401 fails the command.
// Long for its fixture: an endpoint, a route and a credential, all removed
// before asserting.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn a_check_with_an_account_sends_that_accounts_key() {
    let Some(db) = test_db().await else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let _endpoints = ENDPOINT_ROWS.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "t5-cli-check-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "a"}]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let (name, route) = (fresh(), fresh());
    let credential = format!("{name}-1");
    sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]')")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("a route");
    let outcome = async {
        run(
            &db,
            &[
                "endpoint",
                "add",
                "--name",
                &name,
                "--dialect",
                "anthropic",
                "--platform",
                "plain",
                "--auth",
                "x_api_key",
                "--base-url",
                &server.uri(),
            ],
        )
        .await?;
        super::super::accounts::add_account(
            &db,
            &kek(),
            &credential,
            &name,
            "t5-cli-check-key",
            &route,
            4,
            0,
            None,
            None,
        )
        .await?;
        let keyed = run(&db, &["endpoint", "check", &name, "--account", &credential]).await;
        let bare = run(&db, &["endpoint", "check", &name]).await;
        let stranger = run(&db, &["endpoint", "check", &name, "--account", "nobody"]).await;
        Ok::<_, oag_core::Error>((keyed, bare, stranger))
    }
    .await;

    sqlx::query("DELETE FROM account WHERE provider = $1")
        .bind(&name)
        .execute(db.pool())
        .await
        .expect("remove the credential");
    sqlx::query("DELETE FROM route WHERE name = $1")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("remove the route");
    repo::delete_endpoint(&db, &name)
        .await
        .expect("remove the endpoint");

    let (keyed, bare, stranger) = outcome.expect("the fixture");
    keyed.expect("the key listed its models");
    server.verify().await;
    let bare = bare.expect_err("no key, and the endpoint wants one");
    assert!(
        bare.to_string()
            .contains("did not answer with a model list"),
        "{bare}"
    );
    let stranger = stranger.expect_err("no such credential");
    assert!(stranger.to_string().contains("named nobody"), "{stranger}");
}

/// `catalog seed --from` takes a LiteLLM provider's models when an endpoint is
/// registered under that provider's name, and still skips one nothing serves.
#[tokio::test]
async fn a_seed_takes_the_models_of_a_registered_endpoint() {
    let Some(db) = test_db().await else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let _endpoints = ENDPOINT_ROWS.lock().await;
    let (name, stranger) = (fresh(), fresh());
    repo::insert_endpoint(
        &db,
        &oag_store::NewEndpoint {
            name: &name,
            dialect: "openai",
            platform: "plain",
            base_url: Some("http://127.0.0.1:9/v1"),
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
    let file = std::env::temp_dir().join(format!("pr5-litellm-{}.json", Uuid::new_v4()));
    let spec = |provider: &str| {
        json!({
            "litellm_provider": provider,
            "input_cost_per_token": 0.000_001,
            "output_cost_per_token": 0.000_002,
            "max_input_tokens": 128_000,
            "max_output_tokens": 8_192,
        })
    };
    std::fs::write(
        &file,
        serde_json::to_vec(&json!({
            format!("{name}/llama-9"): spec(&name),
            format!("{stranger}/llama-9"): spec(&stranger),
        }))
        .expect("json"),
    )
    .expect("a pricing file");

    let seeded = run(
        &db,
        &["catalog", "seed", "--from", file.to_str().expect("utf-8")],
    )
    .await;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, provider FROM model_catalog WHERE provider = ANY($1)")
            .bind(vec![name.clone(), stranger.clone()])
            .fetch_all(db.pool())
            .await
            .expect("read");

    std::fs::remove_file(&file).expect("remove the file");
    sqlx::query("DELETE FROM model_catalog WHERE provider = $1")
        .bind(&name)
        .execute(db.pool())
        .await
        .expect("remove the model");
    repo::delete_endpoint(&db, &name)
        .await
        .expect("remove the endpoint");

    seeded.expect("seeded");
    assert_eq!(rows, [(format!("{name}/llama-9"), name.clone())]);
}

#[test]
fn check_is_parsed_with_or_without_an_account() {
    match endpoint(&["check", "merge", "--account", "merge-1"]) {
        EndpointCommand::Check { name, account } => {
            assert_eq!(
                (name.as_str(), account.as_deref()),
                ("merge", Some("merge-1"))
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        endpoint(&["check", "merge"]),
        EndpointCommand::Check { account: None, .. }
    ));
    assert!(matches!(
        parsed(&["catalog", "list"]),
        AdminCommand::Catalog(CatalogCommand::List { .. })
    ));
}
