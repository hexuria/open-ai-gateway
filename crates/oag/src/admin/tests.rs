use super::accounts::{
    add_account, add_account_from_args, clear_account_slots, clear_slots_report,
    default_concurrency, kind_is_offered, rename_account, reserve_holds, set_account_owner,
    validated_reserve,
};
use super::catalog::{catalog_lines, empty_catalog_lines};
use super::keys::{mint_key, revoke_key_lines, seat_owner_warnings};
use super::overview::{MONTH_HEADLINE_SQL, init};
use super::principals::{principal_role, promote_principal, upsert_principal};
use super::routes::{parse_ladder_rungs, upsert_route};
use super::*;
use clap::Parser;
use oag_store::repo;
use uuid::Uuid;

/// Held by every test in this binary that registers an endpoint, and by the
/// doctor tests that count them: `doctor` asks every endpoint in the database,
/// so one registered mid-count would be counted by a test that never made it.
pub(crate) static ENDPOINT_ROWS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// C13. "No xai models" and "no models" are different answers.
///
/// `catalog list --provider xai` against a catalog full of Anthropic models
/// printed "catalog is empty; seed it with `oag admin catalog seed`" — so
/// the operator seeded a catalog that was already seeded, got the same
/// message, and concluded the seed was broken.
#[test]
fn an_empty_filtered_catalog_is_not_an_empty_catalog() {
    let seeded = |lines: &[String]| lines.iter().any(|l| l.contains("catalog seed"));

    assert!(
        seeded(&empty_catalog_lines(0, None)),
        "a genuinely empty catalog is the one to offer seeding for"
    );
    assert!(
        seeded(&empty_catalog_lines(0, Some("xai"))),
        "and so is one that is empty before any filter"
    );

    let filtered = empty_catalog_lines(17, Some("xai"));
    assert!(
        !seeded(&filtered),
        "but a catalog holding 17 models of other providers is not empty, and \
             telling the operator to seed it sends them in a circle: {filtered:?}"
    );
    assert!(
        filtered[0].contains("xai") && filtered[0].contains("17"),
        "the filter and what it excluded are both the answer: {filtered:?}"
    );
}

/// C9 at the call site: a revocation that could not evict says so.
///
/// `oag_store`'s `evicting_against_an_unreachable_cache_is_an_error` proves
/// `auth_invalidate` returns an error when it cannot reach Redis. Nothing
/// proved the CLI reads it. Drop the result on the floor — which is what
/// this code did — and the command prints "shared cache evicted" over an
/// eviction that never happened, which during a leaked-key incident is the
/// sentence the operator acts on.
///
/// Gated on Postgres, because a key has to be revoked before there is
/// anything to evict. Redis is a closed port, so the eviction genuinely
/// fails; the connection manager's backoff is why this test is not quick.
#[tokio::test]
async fn a_revocation_that_could_not_evict_warns_instead_of_reassuring() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let tag = Uuid::new_v4();
    let principal: Uuid = sqlx::query_scalar(
        "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member') \
             RETURNING id",
    )
    .bind(format!("c9-{tag}@example.invalid"))
    .fetch_one(db.pool())
    .await
    .expect("principal");
    let route: Uuid = sqlx::query_scalar(
        "INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]') RETURNING id",
    )
    .bind(format!("c9-{tag}"))
    .fetch_one(db.pool())
    .await
    .expect("route");
    let prefix = format!("oag_live_c9{}", &tag.simple().to_string()[..8]);
    sqlx::query(
        "INSERT INTO api_key (id, key_hash, key_prefix, name, principal_id, route_id) \
             VALUES (gen_random_uuid(), $1, $2, $3, $4, $5)",
    )
    .bind(format!("hash-{tag}"))
    .bind(&prefix)
    .bind(format!("c9-{tag}"))
    .bind(principal)
    .bind(route)
    .execute(db.pool())
    .await
    .expect("mint");

    let lines = revoke_key_lines(&db, "redis://127.0.0.1:1", &prefix)
        .await
        .expect("the database half succeeds; the cache half is what fails");

    assert!(
        lines.iter().any(|l| l.contains("revoked")),
        "the row was deactivated, and the command has to say so: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("NOT evicted")),
        "the cache was unreachable, so the key keeps working for five more \
             minutes and the operator has to be told: {lines:?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("shared cache evicted; each replica")),
        "and must not be told the opposite in the same breath: {lines:?}"
    );
}

/// C13 at the call site: the count handed to the message is the one from
/// before the filter.
///
/// `an_empty_filtered_catalog_is_not_an_empty_catalog` above proves
/// `empty_catalog_lines` decides correctly given the right number. Nothing
/// proved the command passes it. `catalog_lines` is the code that does, and
/// passing `rows.len()` after the filter instead — the obvious mistake, and
/// the one the original bug was — leaves every assertion on the helper
/// green while the command again tells the operator to seed a catalog that
/// is already full.
#[test]
fn a_filtered_listing_reports_what_the_catalog_holds_not_what_survived() {
    let model = |id: &str, provider: &str| oag_store::ModelRow {
        id: id.to_owned(),
        provider: provider.to_owned(),
        upstream_name: id.to_owned(),
        input_per_mtok: Decimal::ONE,
        output_per_mtok: Decimal::ONE,
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
        context_window: 200_000,
        max_output_tokens: 8_192,
        supports_vision: false,
        supports_tools: true,
        supports_reasoning: false,
        supports_prompt_cache: false,
        display_label: None,
    };
    let catalog = vec![
        model("anthropic/claude-opus-5", "anthropic"),
        model("anthropic/claude-sonnet-5", "anthropic"),
        model("kimi/k2", "kimi"),
    ];

    let filtered =
        catalog_lines(catalog.clone(), Some("xai"), None).expect("xai is a real provider");
    assert!(
        filtered[0].contains("xai") && filtered[0].contains('3'),
        "the filter and the size of what it excluded are the answer: {filtered:?}"
    );
    assert!(
        !filtered.iter().any(|l| l.contains("catalog seed")),
        "a catalog of three models is not empty, and sending the operator \
             to seed it sends them in a circle: {filtered:?}"
    );

    // A genuinely empty catalog still gets the seeding advice, filter or no.
    for provider in [None, Some("xai")] {
        let empty = catalog_lines(Vec::new(), provider, None).expect("provider");
        assert!(
            empty.iter().any(|l| l.contains("catalog seed")),
            "{provider:?}: nothing is in there to list: {empty:?}"
        );
    }

    // And a filter that matches lists only its own provider.
    let kimi = catalog_lines(catalog, Some("kimi"), None).expect("kimi is a real provider");
    assert_eq!(kimi.len(), 2, "a header and one model: {kimi:?}");
    assert!(kimi[1].contains("kimi/k2"), "{kimi:?}");
}

/// C14. A duplicate credential name is refused, and renaming is the way out.
///
/// `account.name` carries no unique constraint, and every command that
/// addresses a credential does so by name — `disable`, `enable`,
/// `set-cost`, `set-reserve`. A second credential with an existing name was
/// creatable and then unaddressable: `disable` updated both or neither, and
/// nothing could tell them apart or rename one.
///
/// Refused at the command rather than by an index, because an index would
/// fail to build on any deployment that already holds a pair — which is
/// exactly the deployment that needs the tool.
#[tokio::test]
async fn a_duplicate_credential_name_is_refused_and_renaming_is_the_way_out() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    let kek =
        oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");

    let route = format!("c14-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]')")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("route");

    let name = format!("c14-{}", Uuid::new_v4());
    let add = |n: String, r: String| {
        let (db, kek) = (db.clone(), kek.clone());
        async move {
            add_account(
                &db,
                &kek,
                &n,
                "anthropic",
                "not-a-real-secret",
                &r,
                4,
                0,
                None,
                None,
            )
            .await
        }
    };
    add(name.clone(), route.clone()).await.expect("first");

    let err = add(name.clone(), route.clone())
        .await
        .expect_err("the name is taken");
    assert!(
        err.to_string().contains("account rename"),
        "the refusal has to name the way out: {err}"
    );

    // Exactly one credential holds the name, so every by-name command still
    // addresses one thing.
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE name = $1")
        .bind(&name)
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(held, 1);

    // And renaming frees it.
    let freed = format!("{name}-old");
    rename_account(&db, &name, &freed).await.expect("rename");
    add(name.clone(), route.clone())
        .await
        .expect("the name is free again");
    assert!(
        rename_account(&db, &freed, &name).await.is_err(),
        "renaming onto a name in use would recreate the pair"
    );
}

/// `account add --provider <endpoint>` files the key under the endpoint, as
/// the one kind its platform takes, and refuses an endpoint the gateway would
/// not serve with the reason it would not.
///
/// Gated on Postgres: the endpoints are rows, read through the same mapping
/// the gateway's reload uses. The process-wide registry is only ever
/// installed from the table here, and every row this test writes is in it.
// Long for its setup: three endpoint rows, a route, and cleaning all of it up
// before asserting.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_key_is_filed_under_an_endpoint_as_the_kind_its_platform_takes() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    let kek =
        oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");
    let _endpoints = ENDPOINT_ROWS.lock().await;

    let tag = Uuid::new_v4().simple().to_string()[..10].to_owned();
    let route = format!("t4-{tag}");
    sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]')")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("route");
    let (plain, aws, refused) = (
        format!("t4p-{tag}"),
        format!("t4a-{tag}"),
        format!("t4r-{tag}"),
    );
    let none = serde_json::json!({});
    for (name, dialect, platform, base_url, auth, region) in [
        (
            &plain,
            "openai",
            "plain",
            Some("http://127.0.0.1:9/v1"),
            "bearer",
            None,
        ),
        (&aws, "anthropic", "aws", None, "none", Some("us-east-1")),
        // The schema takes it; the compliance guard does not.
        (
            &refused,
            "openai",
            "plain",
            Some("https://api.openai.com/v1"),
            "bearer",
            None,
        ),
    ] {
        repo::insert_endpoint(
            &db,
            &oag_store::NewEndpoint {
                name,
                dialect,
                platform,
                base_url,
                auth,
                region,
                project: None,
                api_version: None,
                path: None,
                extra_headers: &none,
                display_name: None,
                discover_models: false,
            },
        )
        .await
        .expect("the schema admits every one of these");
    }

    let add = |endpoint: &str, secret: &str| {
        let (db, kek, route) = (db.clone(), kek.clone(), route.clone());
        let (name, endpoint, secret) = (
            format!("{endpoint}-key"),
            endpoint.to_owned(),
            secret.to_owned(),
        );
        async move {
            add_account(
                &db, &kek, &name, &endpoint, &secret, &route, 4, 0, None, None,
            )
            .await
        }
    };
    let outcome = async {
        add(&plain, "t4-not-a-real-key").await?;
        add(&aws, "AKIDEXAMPLE:not-a-real-secret").await?;
        let err = add(&refused, "t4-not-a-real-key")
            .await
            .expect_err("the gateway would never serve it");
        let kinds: Vec<(String, String)> = sqlx::query_as(
            "SELECT provider, kind FROM account WHERE provider = ANY($1) ORDER BY provider",
        )
        .bind(vec![plain.clone(), aws.clone(), refused.clone()])
        .fetch_all(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(e.to_string()))?;
        Ok::<_, oag_core::Error>((err, kinds))
    }
    .await;

    // Cleaned up before asserting, so a failure leaves nothing behind.
    sqlx::query("DELETE FROM account WHERE provider = ANY($1)")
        .bind(vec![plain.clone(), aws.clone(), refused.clone()])
        .execute(db.pool())
        .await
        .expect("remove the keys");
    for name in [&plain, &aws, &refused] {
        assert_eq!(
            repo::delete_endpoint(&db, name).await.expect("delete"),
            oag_store::EndpointDeletion::Deleted,
            "{name}"
        );
    }
    sqlx::query("DELETE FROM route WHERE name = $1")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("remove the route");

    let (err, kinds) = outcome.expect("both served endpoints take a key");
    assert_eq!(
        kinds,
        [
            (aws.clone(), "bedrock".to_owned()),
            (plain, "api_key".to_owned())
        ],
        "each key is the kind its endpoint's platform signs with, and none was \
         filed under the refused one"
    );
    let err = err.to_string();
    assert!(
        err.contains(&format!(
            "endpoint '{refused}' is registered but not served"
        )),
        "{err}"
    );
    assert!(err.contains("openai.com"), "and it says which rule: {err}");
}

/// A gcp endpoint's key is a service account's JSON, read whole from a file,
/// checked as the gateway's mint reads it, and filed as a `service_account`.
/// A secret that could never mint is refused before anything is stored, and
/// the refusal quotes none of it.
///
/// Gated on Postgres, as the test above is: the endpoint is a row.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_service_account_key_is_read_from_a_file_and_checked_before_it_is_filed() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    let kek =
        oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");
    let _endpoints = ENDPOINT_ROWS.lock().await;

    let tag = Uuid::new_v4().simple().to_string()[..10].to_owned();
    let (route, vertex) = (format!("t10-{tag}"), format!("t10v-{tag}"));
    sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]')")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("route");
    repo::insert_endpoint(
        &db,
        &oag_store::NewEndpoint {
            name: &vertex,
            dialect: "gemini",
            platform: "gcp",
            base_url: None,
            auth: "bearer",
            region: Some("us-central1"),
            project: Some("oag-test"),
            api_version: None,
            path: None,
            extra_headers: &serde_json::json!({}),
            display_name: None,
            discover_models: false,
        },
    )
    .await
    .expect("a gcp endpoint");

    let key = serde_json::json!({
        "type": "service_account",
        "project_id": "oag-test",
        "private_key_id": "t10",
        "private_key": oag_upstream::gcp_token::TEST_KEY_PEM,
        "client_email": "t10-cli@oag-test.invalid",
    });
    // Over many lines, as Google's download is.
    let pretty = serde_json::to_string_pretty(&key).expect("JSON");
    let file = std::env::temp_dir().join(format!("oag-t10-{tag}.json"));
    std::fs::write(&file, &pretty).expect("a key file");
    let mut wrong_type = key.clone();
    wrong_type["type"] = serde_json::json!("authorized_user");
    let refused = [
        ("an API key", "AIza-t10-not-a-service-account".to_owned()),
        ("another kind of Google key", wrong_type.to_string()),
    ];

    let filed = format!("{vertex}-sa");
    let outcome = async {
        let cli = AdminCli::try_parse_from([
            "admin",
            "account",
            "add",
            "--name",
            &filed,
            "--provider",
            &vertex,
            "--secret-file",
            file.to_str().expect("a UTF-8 path"),
            "--route",
            &route,
        ])
        .expect("parses");
        let AdminCommand::Account(AccountCommand::Add { args }) = cli.cmd else {
            panic!("expected an account add");
        };
        add_account_from_args(&db, &kek, args).await?;

        let mut refusals = Vec::new();
        for (case, secret) in &refused {
            let name = format!("{vertex}-refused");
            let err = add_account(&db, &kek, &name, &vertex, secret, &route, 4, 0, None, None)
                .await
                .expect_err(case);
            refusals.push((*case, err));
        }
        let rows: Vec<(Uuid, String, String)> =
            sqlx::query_as("SELECT id, name, kind FROM account WHERE provider = $1")
                .bind(&vertex)
                .fetch_all(db.pool())
                .await
                .map_err(|e| oag_core::Error::Internal(e.to_string()))?;
        let stored = match rows.first() {
            Some((id, _, _)) => repo::account_by_id(&db, oag_core::AccountId::from_uuid(*id))
                .await?
                .map(|row| kek.open_json::<oag_core::credential::SecretMaterial>(&row.sealed()))
                .transpose()?
                .map(|material| material.access_token.clone()),
            None => None,
        };
        Ok::<_, oag_core::Error>((refusals, rows, stored))
    }
    .await;

    // Cleaned up before asserting, so a failure leaves nothing behind.
    let _ = std::fs::remove_file(&file);
    sqlx::query("DELETE FROM account WHERE provider = $1")
        .bind(&vertex)
        .execute(db.pool())
        .await
        .expect("remove the keys");
    assert_eq!(
        repo::delete_endpoint(&db, &vertex).await.expect("delete"),
        oag_store::EndpointDeletion::Deleted
    );
    sqlx::query("DELETE FROM route WHERE name = $1")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("remove the route");

    let (refusals, rows, stored) = outcome.expect("the key in the file is filed");
    assert_eq!(
        rows.iter()
            .map(|(_, name, kind)| (name.as_str(), kind.as_str()))
            .collect::<Vec<_>>(),
        [(filed.as_str(), "service_account")],
        "the file's key, as the kind a gcp endpoint takes, and no refused one"
    );
    assert_eq!(
        stored.as_deref(),
        Some(pretty.as_str()),
        "sealed as the file holds it"
    );
    for (case, err) in refusals {
        assert!(matches!(err, oag_core::Error::Config(_)), "{case}: {err:?}");
        let message = err.to_string();
        assert!(
            !message.contains("AIza-t10") && !message.contains("BEGIN PRIVATE KEY"),
            "{case}: {message}"
        );
        for line in oag_upstream::gcp_token::TEST_KEY_PEM.lines() {
            assert!(!message.contains(line), "{case}: {message}");
        }
    }
}

/// `--secret-file` is a third way to give a secret, and never beside another
/// or beside an import, which takes its credential from a session file.
#[test]
fn a_secret_file_is_one_way_to_give_the_secret_and_never_beside_another() {
    let parse = |extra: &[&str]| {
        let mut argv = vec!["admin", "account", "add", "--name", "vertex-sa"];
        argv.extend_from_slice(extra);
        AdminCli::try_parse_from(argv)
    };
    let cli = parse(&["--provider", "vertex", "--secret-file", "/keys/sa.json"])
        .expect("a provider and a key file");
    let AdminCommand::Account(AccountCommand::Add { args }) = cli.cmd else {
        panic!("expected an account add");
    };
    assert_eq!(args.secret_file.as_deref(), Some("/keys/sa.json"));
    assert!(args.secret.is_none());

    for clash in [
        &[
            "--provider",
            "vertex",
            "--secret-file",
            "/keys/sa.json",
            "--secret",
            "typed",
        ][..],
        &["--from", "codex", "--secret-file", "/keys/sa.json"],
    ] {
        let err = parse(clash).expect_err("two sources for one secret");
        assert!(
            err.to_string().contains("--secret-file"),
            "{clash:?}: {err}"
        );
    }
}

/// A key file that cannot be read is refused, naming the path, before the
/// database is asked anything: the pool here points at a closed port.
#[tokio::test]
async fn an_unreadable_secret_file_is_refused_before_any_query() {
    let missing =
        std::env::temp_dir().join(format!("oag-t10-missing-{}.json", Uuid::new_v4().simple()));
    let missing = missing.to_str().expect("a UTF-8 path");
    let cli = AdminCli::try_parse_from([
        "admin",
        "account",
        "add",
        "--name",
        "vertex-sa",
        "--provider",
        "vertex",
        "--secret-file",
        missing,
    ])
    .expect("parses");
    let AdminCommand::Account(AccountCommand::Add { args }) = cli.cmd else {
        panic!("expected an account add");
    };
    let db = Db::connect("postgres://oag:oag@127.0.0.1:1/oag_g0", 1).expect("lazy pool");
    let kek =
        oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");
    let err = add_account_from_args(&db, &kek, args)
        .await
        .expect_err("no such file");
    let message = err.to_string();
    assert!(
        message.contains("reading --secret-file") && message.contains(missing),
        "{message}"
    );
}

/// C8. clap does not read `OAG_ACCOUNT_SECRET`, so it cannot conflict on it.
///
/// clap treats an env-supplied value as explicitly present when it
/// evaluates conflicts. With `env` on `--secret` and a `conflicts_with_all`
/// against the importers, exporting the variable — the way this command's
/// own help recommends keeping a key out of shell history — made every
/// `oag admin account add --from codex` fail with "the argument '--secret'
/// cannot be used with '--from'", naming a flag that was not on the command
/// line. Only the operator who followed the advice could hit it.
///
/// Asserted by introspecting the parser rather than by setting the variable,
/// because the environment is process-global and mutating it from a test is
/// `unsafe` — which this crate does not permit. Two facts make the bug
/// unreachable, and both are checked: clap has no env binding for this
/// argument, and no conflict declared against the importers.
#[test]
fn clap_neither_reads_the_secret_env_var_nor_conflicts_on_it() {
    use clap::CommandFactory as _;

    let cmd = AdminCli::command();
    let add = cmd
        .get_subcommands()
        .find(|c| c.get_name() == "account")
        .expect("account")
        .get_subcommands()
        .find(|c| c.get_name() == "add")
        .expect("add")
        .clone();
    let secret = add
        .get_arguments()
        .find(|a| a.get_id() == "secret")
        .expect("--secret");

    assert!(
        secret.get_env().is_none(),
        "an env binding here is what made the conflict fire on a variable; \
             the fallback is read in `add_account_from_args` instead"
    );
    // And the observable half: clap no longer refuses the combination at
    // all. The exclusion moved into `add_account_from_args`, where a typed
    // flag and an inherited variable can still be told apart — clap has no
    // public accessor for an argument's conflicts, so this is asserted by
    // parsing rather than by introspection.
    AdminCli::try_parse_from([
        "admin",
        "account",
        "add",
        "--name",
        "seat",
        "--from",
        "codex",
        "--secret",
        "typed-on-the-command-line",
    ])
    .expect("clap accepts it; the command is what refuses it");
}

/// And an importer parses without a secret, which is the invocation that broke.
#[test]
fn a_seat_import_parses_with_no_secret_flag() {
    let cli = AdminCli::try_parse_from([
        "admin",
        "account",
        "add",
        "--name",
        "codex-seat",
        "--from",
        "codex",
    ])
    .expect("an importer needs no secret");
    let AdminCommand::Account(AccountCommand::Add { args }) = cli.cmd else {
        panic!("expected an account add");
    };
    assert_eq!(args.from, Some(AccountSource::Codex));
    assert!(
        args.secret.is_none(),
        "the flag was not given, so the struct must not claim it was — the \
             environment is read later, after the conflict check"
    );
}

/// C8 at the call site: the exclusion, at the place that now owns it.
///
/// The two tests above pin clap: it reads no environment variable, and it
/// refuses nothing. Neither reaches `add_account_from_args`, which is where
/// the refusal moved to — so with that check deleted both still pass, and a
/// `--secret` typed beside `--from` would be silently discarded. This one
/// parses the same command line and hands the parsed args to the command.
///
/// The database is never reached: the check precedes every query, so a pool
/// pointed at a closed port is enough, and that it returns at all is part of
/// the assertion — a refusal made after the first query would hang here.
#[tokio::test]
async fn a_typed_secret_beside_an_importer_is_refused_by_the_command() {
    let cli = AdminCli::try_parse_from([
        "admin",
        "account",
        "add",
        "--name",
        "seat",
        "--from",
        "codex",
        "--secret",
        "typed-on-the-command-line",
    ])
    .expect("clap accepts it; the command is what refuses it");
    let AdminCommand::Account(AccountCommand::Add { args }) = cli.cmd else {
        panic!("expected an account add");
    };

    let db = Db::connect("postgres://oag:oag@127.0.0.1:1/oag_g0", 1).expect("lazy pool");
    let kek =
        oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");
    let err = add_account_from_args(&db, &kek, args)
        .await
        .expect_err("a typed --secret beside --from is refused");
    assert!(
        err.to_string()
            .contains("--secret cannot be combined with --from"),
        "the error names the exclusion, so the operator knows which flag to \
             drop and that the environment fallback is not the problem: {err}"
    );
}

/// A subscription seat belongs to one person: importing one without
/// `--owner-email` is refused, and `--shared` — removed, kept hidden so old
/// scripts learn why — is refused in every form. Each refusal comes before the
/// first query, so a pool pointed at a closed port is enough.
#[tokio::test]
async fn a_seat_is_never_shared_and_never_imported_without_an_owner() {
    let db = Db::connect("postgres://oag:oag@127.0.0.1:1/oag_g0", 1).expect("lazy pool");
    let kek =
        oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");
    for argv in [
        &["--from", "codex"][..],
        &["--from", "grok"],
        &[
            "--from",
            "codex",
            "--owner-email",
            "me@example.com",
            "--shared",
        ],
        &["--provider", "xai", "--secret", "s", "--shared"],
    ] {
        let mut full = vec!["admin", "account", "add", "--name", "seat"];
        full.extend_from_slice(argv);
        let cli = AdminCli::try_parse_from(&full).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let AdminCommand::Account(AccountCommand::Add { args }) = cli.cmd else {
            panic!("expected an account add");
        };
        let err = add_account_from_args(&db, &kek, args)
            .await
            .expect_err(&format!("{argv:?} must be refused"));
        assert!(
            err.to_string().contains("belongs to one person"),
            "{argv:?}: {err}"
        );
    }
}

/// `account set-owner` binds a seat, and refuses a name it cannot resolve to
/// exactly one credential rather than moving the wrong one.
#[tokio::test]
async fn set_owner_binds_exactly_one_credential() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    let email = format!("{}@example.invalid", Uuid::new_v4());
    let owner: Uuid =
        sqlx::query_scalar("INSERT INTO principal (id, email) VALUES ($1, $2) RETURNING id")
            .bind(Uuid::new_v4())
            .bind(&email)
            .fetch_one(db.pool())
            .await
            .expect("principal");
    let key = |name: String| {
        let db = db.clone();
        async move {
            sqlx::query(
                "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
                 credentials_nonce) VALUES ($1, $2, 'xai', 'api_key', '\\x00', '\\x00')",
            )
            .bind(Uuid::new_v4())
            .bind(name)
            .execute(db.pool())
            .await
            .expect("account");
        }
    };

    let name = format!("bind-{}", Uuid::new_v4());
    key(name.clone()).await;
    let said = set_account_owner(&db, &name, &email).await.expect("bind");
    assert!(
        said.contains(&email) && said.contains("personal (serves its owner only)"),
        "{said}"
    );
    let bound: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_principal_id FROM account WHERE name = $1")
            .bind(&name)
            .fetch_one(db.pool())
            .await
            .expect("row");
    assert_eq!(bound, Some(owner));

    let missing = set_account_owner(&db, &format!("nope-{}", Uuid::new_v4()), &email)
        .await
        .expect_err("no such credential");
    assert!(
        missing.to_string().contains("no credential named"),
        "{missing}"
    );
    let nobody = set_account_owner(&db, &name, "nobody@example.invalid")
        .await
        .expect_err("no such principal");
    assert!(nobody.to_string().contains("no principal"), "{nobody}");

    let twin = format!("twin-{}", Uuid::new_v4());
    key(twin.clone()).await;
    key(twin.clone()).await;
    let ambiguous = set_account_owner(&db, &twin, &email)
        .await
        .expect_err("two credentials share the name");
    assert!(
        ambiguous.to_string().contains("rename one first"),
        "{ambiguous}"
    );
    let moved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM account WHERE name = $1 AND owner_principal_id IS NOT NULL",
    )
    .bind(&twin)
    .fetch_one(db.pool())
    .await
    .expect("count");
    assert_eq!(
        moved, 0,
        "the refusal rolled back, so neither twin was bound"
    );
}

/// C5. Changing a budget at the CLI evicts the identities that cache it.
///
/// A budget lives in the cached auth context, not only in the row. The HTTP
/// path for this write has always evicted explicitly; `init` did not, so a
/// lowered cap was unenforced on every replica for the cache's full five
/// minutes, with nothing in the output hinting that a flush was needed. An
/// operator who has just capped a runaway principal has every reason to
/// believe they have capped them.
#[tokio::test]
async fn lowering_a_budget_at_the_cli_evicts_the_cached_identities() {
    let (Ok(url), Ok(redis_url)) = (
        std::env::var("OAG_TEST_DATABASE_URL"),
        std::env::var("OAG_TEST_REDIS_URL"),
    ) else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL or OAG_TEST_REDIS_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let email = format!("c5-{}@example.invalid", Uuid::new_v4());
    let route = format!("c5-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]')")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("route");
    // An admin, because the eviction is reached through `init` and `init`
    // mints an admin key — which B5 now refuses for a member. The role is
    // incidental to what this test is about; the eviction is not.
    let principal: Uuid = sqlx::query_scalar(
        "INSERT INTO principal (id, email, role, monthly_budget_usd)
             VALUES (gen_random_uuid(), $1, 'admin', 100) RETURNING id",
    )
    .bind(&email)
    .fetch_one(db.pool())
    .await
    .expect("principal");
    let key = mint_key(&db, &email, &route, "c5", None, false)
        .await
        .expect("mint");
    let hash = repo::hash_key(&key);

    // An identity in the shared cache, as a live request would leave.
    let cache = oag_store::Cache::connect(&redis_url).expect("cache");
    let mac = oag_store::AuthMac::new("test-signing-secret-for-c5-eviction-0001");
    let ctx = oag_store::AuthContext {
        api_key_id: Uuid::new_v4(),
        principal_id: principal,
        route_id: Uuid::new_v4(),
        key_floor_tier: None,
        admin: false,
        quota_usd: None,
        principal_budget_usd: Some(Decimal::from(100)),
        principal_hard_stop_multiple: Decimal::from(2),
        expires_at: None,
    };
    cache
        .auth_set(&hash, &ctx, std::time::Duration::from_mins(5), &mac)
        .await;
    assert!(
        cache.auth_get(&hash, &mac).await.is_some(),
        "the fixture has to be cached for the eviction to mean anything"
    );

    // Through `init`, which is what C5 changed. Calling
    // `evict_principal_keys` directly tested the helper: delete the call
    // from `init` and this stayed green, which is the state the finding
    // describes — a new cap set at the CLI and not enforced for five
    // minutes.
    init(&db, &redis_url, &email, &route, Some(Decimal::from(50)))
        .await
        .expect("init lowers the budget");

    assert!(
        cache.auth_get(&hash, &mac).await.is_none(),
        "the new cap is not enforced until this entry is gone, and five \
             minutes of an uncapped principal is the whole finding"
    );
}

/// C3. A credential attached to nothing is an error, not a success line.
///
/// The `account_route` insert selects from `route`, so a name that does not
/// match yields no rows and the INSERT writes nothing — which `execute`
/// reports as success. The command then printed "attached to route 'prod'"
/// over a credential joined to nothing: schedulable, listed as ready,
/// unreachable from any route, and every request through the gateway
/// failing `no_viable_model` while the CLI insisted the credential was fine.
#[tokio::test]
async fn adding_a_credential_to_a_missing_route_is_an_error() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    let kek =
        oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");

    let name = format!("c3-{}", Uuid::new_v4());
    let err = add_account(
        &db,
        &kek,
        &name,
        "anthropic",
        "not-a-real-secret-for-tests",
        "no-such-route",
        4,
        0,
        None,
        None,
    )
    .await
    .expect_err("no route, so nothing to attach to");
    let message = err.to_string();
    assert!(
        message.contains("no-such-route") && message.contains(&name),
        "the operator needs both halves to act on it: {message}"
    );

    // The credential itself survives: it holds a secret the operator has
    // just supplied and may not have kept, and destroying that to tidy up a
    // typo is the worse failure.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE name = $1")
        .bind(&name)
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(stored, 1, "the sealed secret is not thrown away");

    // And it is joined to nothing, which is what the error said.
    let joins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM account_route ar
               JOIN account a ON a.id = ar.account_id WHERE a.name = $1",
    )
    .bind(&name)
    .fetch_one(db.pool())
    .await
    .expect("count");
    assert_eq!(joins, 0);
}

/// C6. Adding a route does not hand out the admin role.
///
/// `init`'s upsert set `role = EXCLUDED.role` with the role hard-coded to
/// `admin`, so `oag admin init --email someone@corp.com --route staging` —
/// a command whose stated job is adding a route — silently promoted whoever
/// that email named and then minted them an admin key. Nothing in the
/// output mentioned a role, because from the command's point of view
/// nothing had changed: it asked for an admin and got one.
///
/// The store's own `upsert_principal` has always omitted `role` here, for
/// the mirror-image reason: an idempotent bind must not be able to *remove*
/// authority either.
#[tokio::test]
async fn init_against_an_existing_principal_leaves_their_role_alone() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let email = format!("c6-{}@example.invalid", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO principal (id, email, role, monthly_budget_usd)
             VALUES (gen_random_uuid(), $1, 'member', 50)",
    )
    .bind(&email)
    .execute(db.pool())
    .await
    .expect("seed a member");

    // The call `init` makes, asking for admin as it always has.
    upsert_principal(&db, &email, "admin", None)
        .await
        .expect("upsert");
    assert_eq!(
        principal_role(&db, &email).await.expect("role").as_deref(),
        Some("member"),
        "adding a route is not a grant of authority"
    );

    // The budget is still protected from an init that omits it.
    let budget: Option<Decimal> =
        sqlx::query_scalar("SELECT monthly_budget_usd FROM principal WHERE email = $1")
            .bind(&email)
            .fetch_one(db.pool())
            .await
            .expect("budget");
    assert_eq!(budget, Some(Decimal::from(50)), "COALESCE still guards it");

    // And a principal that does not exist yet is still created as asked —
    // promoting the first admin is what `init` is for.
    let fresh = format!("c6-first-{}@example.invalid", Uuid::new_v4());
    upsert_principal(&db, &fresh, "admin", None)
        .await
        .expect("upsert");
    assert_eq!(
        principal_role(&db, &fresh).await.expect("role").as_deref(),
        Some("admin")
    );

    // Granting is its own command, and it is idempotent.
    promote_principal(&db, &email).await.expect("promote");
    assert_eq!(
        principal_role(&db, &email).await.expect("role").as_deref(),
        Some("admin")
    );
    promote_principal(&db, &email).await.expect("promote again");
}

/// C7. An `--admin` key is refused for a principal who is not an admin.
///
/// The admin gate is an AND of two facts — the key's flag and the
/// principal's role — and a key can only carry one of them. Minted against
/// a member, an admin key authenticates fine and is then refused by every
/// admin endpoint, which reads as the admin API being broken rather than as
/// the key being half-privileged. Nothing checked it, nothing warned, and
/// the CLI had no command that could set a role.
#[tokio::test]
async fn an_admin_key_is_refused_for_a_principal_who_is_not_one() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let email = format!("c7-{}@example.invalid", Uuid::new_v4());
    sqlx::query("INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')")
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("seed a member");

    let route = format!("c7-{}", Uuid::new_v4());
    upsert_route(&db, &route).await.expect("route");

    // Through `mint_key`, not `require_admin_principal`. Calling the helper
    // proved only that the helper works: it passed with both call-site
    // checks deleted, which is precisely the state `init` was already in.
    let err = mint_key(&db, &email, &route, "k", None, true)
        .await
        .expect_err("a member cannot hold an admin key");
    let message = err.to_string();
    assert!(
        message.contains("oag admin principal promote"),
        "the operator needs the command that fixes it, not just the refusal: {message}"
    );

    let keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM api_key k JOIN principal p ON p.id = k.principal_id \
             WHERE p.email = $1",
    )
    .bind(&email)
    .fetch_one(db.pool())
    .await
    .expect("count");
    assert_eq!(
        keys, 0,
        "the refusal must happen before anything is written"
    );

    // An inference key for the same principal is unaffected: only the
    // combination is refused.
    mint_key(&db, &email, &route, "inference", None, false)
        .await
        .expect("a member may hold an inference key");

    promote_principal(&db, &email).await.expect("promote");
    mint_key(&db, &email, &route, "admin", None, true)
        .await
        .expect("an admin may hold an admin key");
}

/// C7's failure, reached through the door C7 did not close.
#[tokio::test]
async fn init_refuses_an_admin_key_for_an_existing_member() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let email = format!("b5-{}@example.invalid", Uuid::new_v4());
    sqlx::query("INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')")
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("seed a member");

    // `init` mints with `admin = true` unconditionally and prints "This is
    // an ADMIN key". Once C6 stopped it promoting an existing principal,
    // that key authenticated and was then refused by every admin endpoint —
    // the exact scenario C7 refuses one command over, with nothing checked
    // and nothing warned. The redis URL is unused because no budget is
    // passed, so no eviction runs.
    let route = format!("b5-{}", Uuid::new_v4());
    let err = init(&db, "redis://127.0.0.1:1", &email, &route, None)
        .await
        .expect_err("init must not mint an admin key for a member");
    assert!(
        err.to_string().contains("oag admin principal promote"),
        "the refusal names the command that fixes it: {err}"
    );

    let keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM api_key k JOIN principal p ON p.id = k.principal_id \
             WHERE p.email = $1",
    )
    .bind(&email)
    .fetch_one(db.pool())
    .await
    .expect("count");
    assert_eq!(
        keys, 0,
        "a key the operator was told is an admin key must not exist"
    );

    // Promoted, `init` completes: the refusal is about the role, not about
    // `init` itself, and the documented first-run path still works.
    promote_principal(&db, &email).await.expect("promote");
    init(&db, "redis://127.0.0.1:1", &email, &route, None)
        .await
        .expect("init succeeds for an admin principal");
}

/// H9. A key that was not stored is not printed.
///
/// The INSERT selects from `principal` and `route`, so it inserts nothing
/// when either lookup misses — and `execute` reports that as a successful
/// statement affecting zero rows. The plaintext was printed anyway, under
/// "This is shown once", which was true in the worst possible way: never
/// stored, so unrecoverable and unable to ever authenticate.
///
/// The developer holding it gets 401 on every request, `key list` shows
/// nothing, and the incident reads as broken auth rather than as a mistyped
/// route name.
#[tokio::test]
async fn minting_against_a_missing_route_or_principal_is_an_error_not_a_key() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    // A route that exists, so only the principal is missing.
    let route = format!("h9-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]')")
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("seed route");

    let missing_principal = format!("nobody-{}@example.invalid", Uuid::new_v4());
    let err = mint_key(&db, &missing_principal, &route, "k", None, false)
        .await
        .expect_err("no principal, so no key");
    let message = err.to_string();
    assert!(
        message.contains(&missing_principal) && message.contains(&route),
        "the operator cannot see which lookup missed, so both are named: {message}"
    );

    // And nothing was written under either name.
    let keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM api_key k JOIN route r ON r.id = k.route_id WHERE r.name = $1",
    )
    .bind(&route)
    .fetch_one(db.pool())
    .await
    .expect("count");
    assert_eq!(keys, 0, "a failed mint leaves no row");

    // A principal that exists and a route that does not: the same answer,
    // because the SELECT is a cross join and either side empties it.
    let email = format!("h9-{}@example.invalid", Uuid::new_v4());
    sqlx::query("INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')")
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("seed principal");
    mint_key(&db, &email, "no-such-route-here", "k", None, false)
        .await
        .expect_err("no route, so no key");

    // Both present: a key, and it is really there.
    let key = mint_key(&db, &email, &route, "k", None, false)
        .await
        .expect("both exist");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM api_key WHERE key_hash = $1")
        .bind(repo::hash_key(&key))
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(
        stored, 1,
        "the key that was printed is the key that was stored"
    );
}

/// C4. The CLI headline counts per-token traffic only, as the API does.
///
/// A seat row has `cost_usd` of zero and a real API-equivalent price. The
/// admin API filters those out of the headline deliberately — a flat-rate
/// credential's zero marginal cost would otherwise inflate the frontier
/// saving, so the more a subscription was used the better the gateway would
/// claim to be doing. The CLI did not filter, so on any deployment holding
/// a seat the two surfaces differed by an order of magnitude and an
/// operator comparing them had no way to tell which was lying.
///
/// The statement is run directly rather than through `status`, which
/// prints: the number is the finding, and a wrong number looks exactly like
/// a right one on a terminal.
#[tokio::test]
async fn the_month_headline_leaves_seat_rows_out() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    // Inside one repeatable-read transaction, rolled back. The statement is
    // a whole-month aggregate with nothing to key on, so a `before` and an
    // `after` taken against the pool would move under any other test that
    // wrote a ledger row in between — and two tests doing this at once
    // would each spoil the other's arithmetic.
    let mut tx = db.pool().begin().await.expect("begin");
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .expect("a stable snapshot to count against");

    let before: (Decimal, Decimal, i64) = sqlx::query_as(MONTH_HEADLINE_SQL)
        .fetch_one(&mut *tx)
        .await
        .expect("headline");

    // One metered row and one seat row, this month, for the same tokens.
    // The seat's cost is truthfully zero and its displaced API bill is
    // large — which is exactly what makes it poison for a saving figure.
    for (cost, api) in [("2.50", "2.50"), ("0", "40.00")] {
        sqlx::query(
            "INSERT INTO usage_event (request_id, attempt, model_id, tier, \
                 selection_reason, input_tokens, output_tokens, cost_usd, \
                 counterfactual_usd, counterfactual_api_usd, status) \
                 VALUES ($1, 0, 'anthropic/claude-opus-5', 'frontier', 'classified', \
                         100, 20, $2::numeric, 9.00, $3::numeric, 200)",
        )
        .bind(Uuid::new_v4())
        .bind(cost)
        .bind(api)
        .execute(&mut *tx)
        .await
        .expect("seed");
    }

    let after: (Decimal, Decimal, i64) = sqlx::query_as(MONTH_HEADLINE_SQL)
        .fetch_one(&mut *tx)
        .await
        .expect("headline");
    tx.rollback().await.expect("rollback");

    assert_eq!(
        after.2 - before.2,
        1,
        "two rows landed and exactly one of them is per-token traffic"
    );
    assert_eq!(
        after.0 - before.0,
        Decimal::from_str_exact("2.50").expect("decimal"),
        "the seat contributed no spend"
    );
    assert_eq!(
        after.1 - before.1,
        Decimal::from_str_exact("9.00").expect("decimal"),
        "and no counterfactual — its zero cost against a frontier baseline \
             is the free saving that made this figure a lie"
    );
}

/// The `64fc95b` filter in the CLI headline: attempts are not requests.
///
/// `the_month_headline_leaves_seat_rows_out` above seeds only `classified`
/// rows, so `COUNT(*) FILTER (WHERE selection_reason NOT IN ('abandoned',
/// 'lost'))` could be deleted and it would stay green. Since 0014
/// contracted the ledger key onto `(request_id, attempt)`, one client
/// request leaves a row per attempt — every one generated and invoiced, so
/// every one is money, but only one of them is a request. Counting them all
/// makes the headline claim the gateway served more requests the more often
/// escalation saved a bad answer.
#[tokio::test]
async fn the_month_headline_counts_requests_and_sums_attempts() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    // One repeatable-read transaction, rolled back: see the note in
    // `the_month_headline_leaves_seat_rows_out`.
    let mut tx = db.pool().begin().await.expect("begin");
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .expect("a stable snapshot to count against");

    let before: (Decimal, Decimal, i64) = sqlx::query_as(MONTH_HEADLINE_SQL)
        .fetch_one(&mut *tx)
        .await
        .expect("headline");

    // One client request, three ledger rows: a gate abandoned the first
    // answer, a stream lost the second, the third was served. All three
    // were generated upstream and all three are billed.
    let request_id = Uuid::new_v4();
    for (attempt, reason) in [(0i16, "abandoned"), (1, "lost"), (2, "classified")] {
        sqlx::query(
            "INSERT INTO usage_event (request_id, attempt, model_id, tier, \
                 selection_reason, input_tokens, output_tokens, cost_usd, \
                 counterfactual_usd, counterfactual_api_usd, status) \
                 VALUES ($1, $2, 'anthropic/claude-opus-5', 'frontier', $3, \
                         100, 20, 1.00, 3.00, 1.00, 200)",
        )
        .bind(request_id)
        .bind(attempt)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .expect("seed");
    }

    let after: (Decimal, Decimal, i64) = sqlx::query_as(MONTH_HEADLINE_SQL)
        .fetch_one(&mut *tx)
        .await
        .expect("headline");
    tx.rollback().await.expect("rollback");

    assert_eq!(
        after.2 - before.2,
        1,
        "three rows landed and the client made one request"
    );
    assert_eq!(
        after.0 - before.0,
        Decimal::from_str_exact("3.00").expect("decimal"),
        "and every one of them was generated, so every one is paid for"
    );
}

#[derive(Parser, Debug)]
#[command(name = "admin")]
struct AdminCli {
    #[command(subcommand)]
    cmd: AdminCommand,
}

fn parse(args: &[&str]) -> std::result::Result<AdminCommand, clap::Error> {
    Ok(AdminCli::try_parse_from(std::iter::once("admin").chain(args.iter().copied()))?.cmd)
}

#[test]
fn set_reserve_parses_a_percentage_and_a_clear() {
    match parse(&["account", "set-reserve", "grok", "--pct", "15"])
        .unwrap_or_else(|e| panic!("{e}"))
    {
        AdminCommand::Account(AccountCommand::SetReserve { name, pct }) => {
            assert_eq!(name, "grok");
            assert_eq!(pct, Some(15));
        }
        other => panic!("expected account set-reserve, got {other:?}"),
    }
    // Omitting `--pct` is how a reserve is removed, exactly as omitting
    // `--monthly-cost` clears a price.
    match parse(&["account", "set-reserve", "grok"]).unwrap_or_else(|e| panic!("{e}")) {
        AdminCommand::Account(AccountCommand::SetReserve { pct, .. }) => assert_eq!(pct, None),
        other => panic!("expected account set-reserve, got {other:?}"),
    }
}

#[test]
fn clearing_an_empty_seat_says_nothing_about_live_requests() {
    let report = clear_slots_report("grok-seat", Uuid::nil(), 0);
    assert!(report.starts_with("cleared 0 slot(s) on grok-seat"));
    assert!(!report.contains("live requests"), "{report}");
}

#[test]
fn clearing_held_slots_warns_that_a_refused_retake_lasts_the_request() {
    // Not "within one heartbeat": the retake honours the limit, so a seat
    // that filled meanwhile refuses it, and the over-admission lasts as
    // long as the request. Nor is `dropped` a bound: it counts ghosts too.
    let report = clear_slots_report("grok-seat", Uuid::nil(), 3);
    for says in [
        "ghosts and live requests alike",
        "is refused",
        "until that request finishes",
    ] {
        assert!(report.contains(says), "should say {says:?}: {report}");
    }
    assert!(
        !report.contains("up to 3"),
        "dropped is not a ceiling: {report}"
    );
}

/// The command empties the credential's key, by name.
#[tokio::test]
async fn clear_slots_empties_the_named_credential() {
    let (Ok(db_url), Ok(redis_url)) = (
        std::env::var("OAG_TEST_DATABASE_URL"),
        std::env::var("OAG_TEST_REDIS_URL"),
    ) else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
        return;
    };
    let db = Db::connect(&db_url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    let name = format!("clear-{}", Uuid::new_v4());
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
             credentials_nonce) VALUES (gen_random_uuid(), $1, 'anthropic', 'api_key', \
             '\\x00', '\\x00') RETURNING id",
    )
    .bind(&name)
    .fetch_one(db.pool())
    .await
    .expect("account");
    let account = oag_core::AccountId::from_uuid(id);
    let cache = oag_store::Cache::connect(&redis_url).expect("cache");
    let ttl = std::time::Duration::from_mins(1);
    for member in ["a", "b"] {
        assert!(
            cache
                .acquire_slot(account, member, 8, ttl)
                .await
                .expect("acquire")
        );
    }

    clear_account_slots(&db, &redis_url, &name)
        .await
        .expect("clear");
    assert_eq!(cache.slots_in_use(account, ttl).await.expect("count"), 0);
    sqlx::query("DELETE FROM account WHERE id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("cleanup");
}

#[test]
fn clear_slots_parses_a_credential_name() {
    match parse(&["account", "clear-slots", "grok-seat"]).unwrap_or_else(|e| panic!("{e}")) {
        AdminCommand::Account(AccountCommand::ClearSlots { name }) => {
            assert_eq!(name, "grok-seat");
        }
        other => panic!("expected account clear-slots, got {other:?}"),
    }
}

#[test]
fn a_reserve_outside_the_percentage_range_is_refused_by_naming_the_range() {
    // `--pct 150` and `--pct -1` are typos, and a database CHECK violation
    // is not an answer anybody can act on. The range has to be in the text
    // or the next guess is as blind as the first.
    for bad in [-1, 101, 1_000] {
        let e = validated_reserve(Some(bad)).expect_err("out of range");
        let message = e.to_string();
        assert!(message.contains("0-100"), "{message}");
        assert!(message.contains(&bad.to_string()), "{message}");
    }
}

#[test]
fn the_ends_of_the_range_and_a_cleared_reserve_are_accepted() {
    // 100 is "never schedule this seat", which is a legitimate, if blunt,
    // way to park one; 0 is a reserve that only fires on a truly empty
    // pool; None is no reserve at all.
    assert_eq!(
        validated_reserve(Some(0)).unwrap_or_else(|e| panic!("{e}")),
        Some(0)
    );
    assert_eq!(
        validated_reserve(Some(100)).unwrap_or_else(|e| panic!("{e}")),
        Some(100)
    );
    assert_eq!(
        validated_reserve(None).unwrap_or_else(|e| panic!("{e}")),
        None
    );
}

#[test]
fn a_listing_calls_a_seat_held_back_only_when_the_scheduler_would_refuse_it() {
    use rust_decimal::Decimal;
    assert!(reserve_holds(Some(Decimal::from(5)), Some(10)));
    assert!(
        reserve_holds(Some(Decimal::from(10)), Some(10)),
        "at the line"
    );
    assert!(!reserve_holds(Some(Decimal::from(45)), Some(10)));
    // Unknown is not empty, and no reserve is no policy.
    assert!(!reserve_holds(None, Some(10)));
    assert!(!reserve_holds(Some(Decimal::ZERO), None));
}

#[test]
fn account_add_accepts_from_space_and_equals() {
    for args in [
        &["account", "add", "--name", "n", "--from", "grok"][..],
        &["account", "add", "--name", "n", "--from=codex"][..],
    ] {
        match parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}")) {
            AdminCommand::Account(AccountCommand::Add { args }) => {
                assert!(args.from.is_some(), "{args:?}");
            }
            other => panic!("expected account add, got {other:?}"),
        }
    }
}

#[test]
fn hidden_add_account_and_from_bools_still_parse() {
    match parse(&["add-account", "--name", "n", "--from-grok"]).unwrap_or_else(|e| panic!("{e}")) {
        AdminCommand::AddAccount { args } => assert!(args.from_grok),
        other => panic!("expected hidden add-account, got {other:?}"),
    }
}

#[test]
fn key_create_and_legacy_key_flags_parse() {
    match parse(&["key", "create", "--email", "a@b.c"]).unwrap_or_else(|e| panic!("{e}")) {
        AdminCommand::Key(cli) => {
            assert!(matches!(cli.action, Some(KeyAction::Create { .. })));
        }
        other => panic!("expected key create, got {other:?}"),
    }
    match parse(&["key", "--email", "a@b.c"]).unwrap_or_else(|e| panic!("{e}")) {
        AdminCommand::Key(cli) => {
            assert!(cli.action.is_none());
            assert_eq!(cli.email.as_deref(), Some("a@b.c"));
        }
        other => panic!("expected legacy key, got {other:?}"),
    }
}

#[test]
fn key_revoke_is_positional() {
    match parse(&["key", "revoke", "oag_live_abc"]).unwrap_or_else(|e| panic!("{e}")) {
        AdminCommand::Key(cli) => {
            assert!(matches!(
                cli.action,
                Some(KeyAction::Revoke { ref prefix }) if prefix == "oag_live_abc"
            ));
        }
        other => panic!("expected key revoke, got {other:?}"),
    }
}

#[test]
fn route_tiers_parses_name_equals_models() {
    match parse(&[
        "route",
        "tiers",
        "cheap=xai/grok-4.3",
        "balanced=xai/grok-4.5",
    ])
    .unwrap_or_else(|e| panic!("{e}"))
    {
        AdminCommand::Route(RouteCommand::Tiers { rungs, .. }) => {
            assert_eq!(rungs.len(), 2);
            assert_eq!(rungs[0], "cheap=xai/grok-4.3");
        }
        other => panic!("expected route tiers, got {other:?}"),
    }
    let parsed = parse_ladder_rungs(&[
        "cheap=xai/grok-4.3,xai/grok-4".to_owned(),
        "balanced=xai/grok-4.5".to_owned(),
    ])
    .expect("rungs");
    assert_eq!(parsed[0].models.len(), 2);
    assert_eq!(parsed[1].name.as_str(), "balanced");
}

#[test]
fn hidden_flat_spellings_still_parse() {
    assert!(matches!(
        parse(&["seed-catalog"]).unwrap_or_else(|e| panic!("{e}")),
        AdminCommand::SeedCatalog { .. }
    ));
    assert!(matches!(
        parse(&["flush-cache"]).unwrap_or_else(|e| panic!("{e}")),
        AdminCommand::FlushCache
    ));
    assert!(matches!(
        parse(&["revoke-key", "--prefix", "oag_live_x"]).unwrap_or_else(|e| panic!("{e}")),
        AdminCommand::RevokeKey { .. }
    ));
    assert!(matches!(
        parse(&["set-mode", "--mode", "managed"]).unwrap_or_else(|e| panic!("{e}")),
        AdminCommand::SetMode { .. }
    ));
}

/// A Claude subscription is never a credential this gateway serves with, and
/// the schema cannot say so on its own: its CHECK knows kinds, not pairs.
#[test]
fn a_credential_kind_the_provider_does_not_offer_is_refused() {
    use oag_core::Provider;
    for (provider, kind) in [
        (Provider::Anthropic, "oauth"),
        (Provider::Kimi, "bedrock"),
        (Provider::Gemini, "oauth"),
        (Provider::OpenAI, "vertex"),
        (Provider::OpenAI, "no_such_kind"),
    ] {
        let refused = kind_is_offered(provider, kind).expect_err(&format!("{provider} {kind}"));
        assert!(
            refused.to_string().contains("does not take"),
            "{provider} {kind}: {refused}"
        );
    }
    for (provider, kind) in [
        (Provider::Anthropic, "api_key"),
        (Provider::OpenAI, "oauth"),
        (Provider::XAI, "oauth"),
        (Provider::Bedrock, "api_key"),
        (Provider::Bedrock, "bedrock"),
    ] {
        kind_is_offered(provider, kind).unwrap_or_else(|e| panic!("{provider} {kind}: {e}"));
    }
}

/// `key create` and `doctor` both say it when a seat's owner holds several
/// live inference keys, naming the seat, the owner and the count; one key says
/// nothing.
#[tokio::test]
async fn a_seat_owner_minting_a_second_key_is_warned() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    let email = format!("{}@example.invalid", Uuid::new_v4());
    let route = format!("r-{}", Uuid::new_v4());
    let seat = format!("seat-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO principal (id, email) VALUES ($1, $2)")
        .bind(Uuid::new_v4())
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("principal");
    sqlx::query("INSERT INTO route (id, name, tiers) VALUES ($1, $2, '[]'::jsonb)")
        .bind(Uuid::new_v4())
        .bind(&route)
        .execute(db.pool())
        .await
        .expect("route");
    sqlx::query(
        "INSERT INTO account (id, name, provider, kind, credentials_sealed, credentials_nonce, \
         owner_principal_id) SELECT $1, $2, 'xai', 'oauth', '\\x00', '\\x00', p.id \
         FROM principal p WHERE p.email = $3",
    )
    .bind(Uuid::new_v4())
    .bind(&seat)
    .bind(&email)
    .execute(db.pool())
    .await
    .expect("seat");

    mint_key(&db, &email, &route, "laptop", None, false)
        .await
        .expect("first key");
    assert!(seat_owner_warnings(&db, &email).await.is_empty());

    mint_key(&db, &email, &route, "someone-else", None, false)
        .await
        .expect("second key");
    let warned = seat_owner_warnings(&db, &email).await;
    assert_eq!(warned.len(), 1, "{warned:?}");
    for part in [
        seat.as_str(),
        email.as_str(),
        "2 live keys",
        "shares the seat",
    ] {
        assert!(
            warned[0].contains(part),
            "{part} missing from {}",
            warned[0]
        );
    }
    let from_doctor = super::doctor::check_seat_owner_keys(&db)
        .await
        .expect("doctor's check");
    assert!(
        from_doctor.iter().any(|s| s.seat == seat && s.keys == 2),
        "doctor sees the same seat: {from_doctor:?}"
    );
}

/// The dispatchers hand back what the command they ran said. With a pool that
/// cannot connect, every command fails; a dispatcher that answered `Ok` without
/// running anything would pass for a working one.
#[tokio::test]
async fn the_account_and_key_dispatchers_surface_the_commands_errors() {
    let db = Db::connect("postgres://oag:oag@127.0.0.1:1/oag_g0", 1).expect("lazy pool");
    let kek =
        oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");
    let redis = "redis://127.0.0.1:1";

    let AdminCommand::Account(cmd) = AdminCli::try_parse_from([
        "admin",
        "account",
        "set-owner",
        "seat",
        "--owner-email",
        "me@example.com",
    ])
    .expect("parses")
    .cmd
    else {
        panic!("expected an account command");
    };
    super::accounts::account_cmd(&db, &kek, cmd, redis)
        .await
        .expect_err("set-owner cannot succeed without a database");

    let AdminCommand::Key(cli) =
        AdminCli::try_parse_from(["admin", "key", "create", "--email", "me@example.com"])
            .expect("parses")
            .cmd
    else {
        panic!("expected a key command");
    };
    super::keys::key_cmd(&db, redis, cli)
        .await
        .expect_err("key create cannot succeed without a database");
}

/// `doctor` fails a route it cannot find, rather than printing `ok` for it.
#[tokio::test]
async fn doctor_fails_a_route_that_does_not_exist() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");
    let config = oag_core::config::Config::from_yaml(&format!(
        "database:\n  url: \"{url}\"\nredis:\n  url: \"redis://127.0.0.1:1\"\nsecurity:\n  \
         signing_secret: \"Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0=\"\n  \
         credential_kek: \"MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=\"\n"
    ))
    .expect("a minimal config parses");
    let route = format!("no-such-route-{}", Uuid::new_v4());
    super::doctor::run(&db, &config, &route)
        .await
        .expect_err("a missing route is a problem");
}

/// A seat defaults to two requests in flight, an API key to eight, and the
/// flag overrides either.
#[test]
fn a_seat_defaults_to_two_requests_in_flight_and_a_key_to_eight() {
    assert_eq!(default_concurrency(Some(AccountSource::Codex)), 2);
    assert_eq!(default_concurrency(Some(AccountSource::Grok)), 2);
    assert_eq!(default_concurrency(None), 8);
    let parsed = |extra: &[&str]| {
        let mut argv = vec!["admin", "account", "add", "--name", "n", "--from", "codex"];
        argv.extend_from_slice(extra);
        let AdminCommand::Account(AccountCommand::Add { args }) =
            AdminCli::try_parse_from(&argv).expect("parses").cmd
        else {
            panic!("expected an account add");
        };
        args.max_concurrency
    };
    assert_eq!(parsed(&[]), None, "no flag, so the default decides");
    assert_eq!(parsed(&["--max-concurrency", "5"]), Some(5));
}

/// `endpoint add` takes every setting as a flag; the dialect, platform and
/// auth are the column spellings, with the hyphenated ones as aliases.
#[test]
fn endpoint_add_parses_every_setting() {
    match parse(&[
        "endpoint",
        "add",
        "--name",
        "merge",
        "--dialect",
        "openai",
        "--platform",
        "plain",
        "--base-url",
        "https://api-gateway.merge.dev/v1/openai",
        "--header",
        "X-Project-Id=p-1",
        "--header",
        "X-Title=oag",
        "--display-name",
        "Merge Gateway",
    ])
    .unwrap_or_else(|e| panic!("{e}"))
    {
        AdminCommand::Endpoint(EndpointCommand::Add { args }) => {
            assert_eq!(args.name, "merge");
            assert_eq!(args.dialect, DialectArg::Openai);
            assert_eq!(args.platform, PlatformArg::Plain);
            assert_eq!(args.auth, None, "the platform's default decides");
            assert_eq!(args.headers, ["X-Project-Id=p-1", "X-Title=oag"]);
            assert!(!args.discover);
        }
        other => panic!("expected endpoint add, got {other:?}"),
    }
    for (spelt, dialect) in [
        ("system_one", DialectArg::SystemOne),
        ("system-one", DialectArg::SystemOne),
        ("anthropic", DialectArg::Anthropic),
        ("gemini", DialectArg::Gemini),
        ("bedrock_converse", DialectArg::BedrockConverse),
        ("bedrock-converse", DialectArg::BedrockConverse),
    ] {
        match parse(&[
            "endpoint",
            "add",
            "--name",
            "e",
            "--dialect",
            spelt,
            "--platform",
            "aws",
            "--auth",
            "x-api-key",
        ])
        .unwrap_or_else(|e| panic!("{spelt}: {e}"))
        {
            AdminCommand::Endpoint(EndpointCommand::Add { args }) => {
                assert_eq!(args.dialect, dialect, "{spelt}");
                assert_eq!(args.auth, Some(AuthArg::XApiKey));
            }
            other => panic!("expected endpoint add, got {other:?}"),
        }
    }
    // Each spelling names the column value the shared rules read.
    for dialect in [
        DialectArg::Openai,
        DialectArg::Anthropic,
        DialectArg::Gemini,
        DialectArg::SystemOne,
        DialectArg::BedrockConverse,
    ] {
        let parsed = oag_core::provider::Dialect::from_endpoint_column(dialect.column())
            .unwrap_or_else(|e| panic!("{dialect:?}: {e}"));
        assert_eq!(parsed.endpoint_column(), Some(dialect.column()));
    }
    for (auth, style) in [
        (AuthArg::Bearer, "bearer"),
        (AuthArg::XApiKey, "x_api_key"),
        (AuthArg::XGoogApiKey, "x_goog_api_key"),
        (AuthArg::ApiKeyHeader, "api_key_header"),
        (AuthArg::None, "none"),
    ] {
        assert_eq!(auth.style().as_str(), style);
    }
    for (platform, spelt) in [
        (PlatformArg::Plain, "plain"),
        (PlatformArg::Aws, "aws"),
        (PlatformArg::Gcp, "gcp"),
        (PlatformArg::Azure, "azure"),
    ] {
        assert_eq!(platform.platform().as_str(), spelt);
    }
    // The platform is not optional.
    assert!(parse(&["endpoint", "add", "--name", "e", "--dialect", "openai"]).is_err());
}

/// `endpoint set` has no `--dialect` and no `--platform`: clap refuses both,
/// because changing either makes it a different endpoint.
#[test]
fn endpoint_set_refuses_the_dialect_and_the_platform() {
    for flag in [["--dialect", "anthropic"], ["--platform", "azure"]] {
        let err = parse(&["endpoint", "set", "merge", flag[0], flag[1]])
            .expect_err("not a setting that changes");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::UnknownArgument,
            "{flag:?}: {err}"
        );
    }
    match parse(&[
        "endpoint",
        "set",
        "merge",
        "--base-url",
        "",
        "--unset-header",
        "X-Old",
        "--discover",
    ])
    .unwrap_or_else(|e| panic!("{e}"))
    {
        AdminCommand::Endpoint(EndpointCommand::Set { args }) => {
            assert_eq!(args.name, "merge");
            assert_eq!(args.base_url.as_deref(), Some(""), "an empty value clears");
            assert_eq!(args.unset_headers, ["X-Old"]);
            assert_eq!(args.discover, Some(true));
        }
        other => panic!("expected endpoint set, got {other:?}"),
    }
    for (given, discover) in [(&["--discover", "false"][..], Some(false)), (&[][..], None)] {
        let mut argv = vec!["endpoint", "set", "merge"];
        argv.extend_from_slice(given);
        match parse(&argv).unwrap_or_else(|e| panic!("{e}")) {
            AdminCommand::Endpoint(EndpointCommand::Set { args }) => {
                assert_eq!(args.discover, discover, "{given:?}");
            }
            other => panic!("expected endpoint set, got {other:?}"),
        }
    }
    for verb in ["show", "remove", "check"] {
        assert!(
            parse(&["endpoint", verb, "merge"]).is_ok(),
            "{verb} takes the name positionally"
        );
        assert!(parse(&["endpoint", verb]).is_err(), "{verb} needs a name");
    }
    assert!(matches!(
        parse(&["endpoint", "list"]).unwrap_or_else(|e| panic!("{e}")),
        AdminCommand::Endpoint(EndpointCommand::List)
    ));
}

fn catalog_add(extra: &[&str]) -> std::result::Result<CatalogAddArgs, clap::Error> {
    let mut argv = vec![
        "catalog",
        "add",
        "--id",
        "merge/zai/glm-5.3-flash",
        "--upstream",
        "zai/glm-5.3-flash",
        "--context",
        "128000",
        "--max-output",
        "8192",
    ];
    argv.extend_from_slice(extra);
    match parse(&argv)? {
        AdminCommand::Catalog(CatalogCommand::Add { args }) => Ok(args),
        other => panic!("expected catalog add, got {other:?}"),
    }
}

/// `catalog add` needs both prices unless the model is `--free`, and a window
/// and an output limit of at least one token.
#[test]
fn catalog_add_parses_prices_limits_and_capabilities() {
    let args = catalog_add(&[
        "--input-per-mtok",
        "0.10",
        "--output-per-mtok",
        "0.40",
        "--cache-read-per-mtok",
        "0.01",
        "--tools",
        "--reasoning",
        "--display-label",
        "GLM Flash",
    ])
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(args.id, "merge/zai/glm-5.3-flash");
    assert_eq!(args.upstream, "zai/glm-5.3-flash");
    assert_eq!(
        (args.input_per_mtok, args.output_per_mtok),
        (
            Some(Decimal::from_str_exact("0.10").expect("decimal")),
            Some(Decimal::from_str_exact("0.40").expect("decimal"))
        )
    );
    assert_eq!(args.cache_write_per_mtok, None);
    assert_eq!((args.context, args.max_output), (128_000, 8_192));
    assert!(args.tools && args.reasoning && !args.vision && !args.prompt_cache);
    assert!(!args.free);

    assert!(
        catalog_add(&["--input-per-mtok", "1"]).is_err(),
        "no output price"
    );
    assert!(catalog_add(&[]).is_err(), "no prices and not free");
    let free = catalog_add(&["--free"]).expect("free needs no prices");
    assert_eq!((free.input_per_mtok, free.output_per_mtok), (None, None));
    assert!(
        parse(&[
            "catalog",
            "add",
            "--id",
            "a/b",
            "--upstream",
            "b",
            "--free",
            "--context",
            "0",
            "--max-output",
            "1",
        ])
        .is_err(),
        "a window of no tokens fits no request"
    );
}

/// A model id's provider is everything before the FIRST slash: an endpoint
/// that fronts other vendors names its models `vendor/model`.
#[test]
fn a_model_id_splits_at_its_first_slash() {
    assert_eq!(
        super::catalog::split_model_id("merge/zai/glm-5.3-flash").expect("split"),
        ("merge", "zai/glm-5.3-flash")
    );
    assert_eq!(
        super::catalog::split_model_id("xai/grok-4.6").expect("split"),
        ("xai", "grok-4.6")
    );
    for bad in ["merge", "/glm", "merge/", "merge/glm 5", "", "merge/\tglm"] {
        let err = super::catalog::split_model_id(bad).expect_err(bad);
        assert!(
            err.to_string().contains("<provider>/<model>"),
            "{bad:?}: {err}"
        );
    }
}

/// `catalog add`'s row: a zero price needs `--free`, `--free` needs a zero
/// price, and the id names its provider as the provider spells itself.
#[test]
fn catalog_add_refuses_a_zero_price_unless_it_is_free() {
    use oag_core::Provider;
    use oag_core::provider::{Dialect, Endpoint, Platform};
    let merge = Provider::Custom(
        Endpoint::new("merge", Dialect::OpenAIChatCompletions, Platform::Plain).expect("merge"),
    );
    // `=`, so a negative price is a value rather than a flag.
    let priced = |input: &str, output: &str| {
        let (input, output) = (
            format!("--input-per-mtok={input}"),
            format!("--output-per-mtok={output}"),
        );
        catalog_add(&[&input, &output]).unwrap_or_else(|e| panic!("{e}"))
    };

    let row = super::catalog::model_row(&priced("0.1", "0.4"), merge).expect("a row");
    assert_eq!(row.id, "merge/zai/glm-5.3-flash");
    assert_eq!(row.provider, "merge");
    assert_eq!(row.upstream_name, "zai/glm-5.3-flash");
    assert_eq!(row.context_window, 128_000);

    let zero = super::catalog::model_row(&priced("0", "0.000"), merge)
        .expect_err("zero without --free")
        .to_string();
    assert!(zero.contains("wins every cost comparison"), "{zero}");
    assert!(zero.contains("--free"), "{zero}");

    let free = catalog_add(&["--free"]).expect("free");
    let row = super::catalog::model_row(&free, merge).expect("free on purpose");
    assert!(row.input_per_mtok.is_zero() && row.output_per_mtok.is_zero());
    let lines = super::catalog::added_model_lines(&row).join("\n");
    assert!(lines.contains("free on purpose"), "{lines}");
    assert!(lines.contains("an operator override"), "{lines}");
    let paid = super::catalog::model_row(&priced("0.1", "0.4"), merge).expect("a row");
    assert!(
        !super::catalog::added_model_lines(&paid)
            .join("\n")
            .contains("free on purpose")
    );

    let both = catalog_add(&["--free", "--input-per-mtok", "1", "--output-per-mtok", "0"])
        .expect("clap takes both");
    let err = super::catalog::model_row(&both, merge)
        .expect_err("free with a price")
        .to_string();
    assert!(err.contains("--free says the model costs nothing"), "{err}");

    // One side free is a price, not a free model, and is not printed as one.
    for (input, output) in [("0", "2"), ("2", "0")] {
        let one_side = super::catalog::model_row(&priced(input, output), merge)
            .unwrap_or_else(|e| panic!("free on one side only ({input}, {output}): {e}"));
        let lines = super::catalog::added_model_lines(&one_side).join("\n");
        assert!(!lines.contains("free on purpose"), "{lines}");
    }

    for (input, says) in [
        ("-1", "cannot be negative"),
        ("1000000", "must be under 1000000"),
    ] {
        let err = super::catalog::model_row(&priced(input, "1"), merge)
            .expect_err(input)
            .to_string();
        assert!(err.contains(says), "{input}: {err}");
    }
    super::catalog::model_row(&priced("999999.999999", "1"), merge).expect("the most it holds");
    let cached = catalog_add(&[
        "--input-per-mtok",
        "1",
        "--output-per-mtok",
        "1",
        "--cache-write-per-mtok=-0.5",
    ])
    .expect("clap takes it");
    let err = super::catalog::model_row(&cached, merge)
        .expect_err("a negative cache price")
        .to_string();
    assert!(
        err.contains("--cache-write-per-mtok cannot be negative"),
        "{err}"
    );

    let err = super::catalog::model_row(
        &CatalogAddArgs {
            id: "grok/grok-4.6".to_owned(),
            ..catalog_add(&["--free"]).expect("free")
        },
        Provider::XAI,
    )
    .expect_err("an alias in the id")
    .to_string();
    assert!(err.contains("use --id xai/grok-4.6"), "{err}");

    let blank = CatalogAddArgs {
        upstream: "  ".to_owned(),
        ..catalog_add(&["--free"]).expect("free")
    };
    assert!(
        super::catalog::model_row(&blank, merge).is_err(),
        "no upstream name"
    );
    let labelled = CatalogAddArgs {
        display_label: Some("two\nlines".to_owned()),
        ..catalog_add(&["--free"]).expect("free")
    };
    assert!(super::catalog::model_row(&labelled, merge).is_err());
}

/// C11. A price is rounded to the catalog's six places, half away from zero
/// as Postgres rounds a `numeric(12,6)`, before any rule judges it: what rounds
/// to zero is a zero, what rounds to a million is past the ceiling, and the row
/// holds the price as it will be stored.
#[test]
fn a_catalog_price_is_rounded_as_it_will_be_stored_before_it_is_judged() {
    use oag_core::Provider;
    use oag_core::provider::{Dialect, Endpoint, Platform};
    let merge = Provider::Custom(
        Endpoint::new("merge", Dialect::OpenAIChatCompletions, Platform::Plain).expect("merge"),
    );
    let priced = |input: &str, output: &str| {
        let (input, output) = (
            format!("--input-per-mtok={input}"),
            format!("--output-per-mtok={output}"),
        );
        catalog_add(&[&input, &output]).unwrap_or_else(|e| panic!("{e}"))
    };
    let exact = |s: &str| Decimal::from_str_exact(s).expect("decimal");

    let zero = super::catalog::model_row(&priced("0.0000004", "0.00000049"), merge)
        .expect_err("both round to zero, and nobody said --free")
        .to_string();
    assert!(zero.contains("--free"), "{zero}");
    let free = CatalogAddArgs {
        input_per_mtok: Some(exact("0.0000004")),
        ..catalog_add(&["--free"]).expect("free")
    };
    super::catalog::model_row(&free, merge).expect("--free, and a price that rounds to zero");

    let ceiling = super::catalog::model_row(&priced("999999.9999995", "1"), merge)
        .expect_err("rounds to a million")
        .to_string();
    assert!(ceiling.contains("must be under 1000000"), "{ceiling}");

    let row = super::catalog::model_row(&priced("0.0000005", "1.0000025"), merge)
        .expect("each rounds away from zero");
    assert_eq!(
        (row.input_per_mtok, row.output_per_mtok),
        (exact("0.000001"), exact("1.000003")),
        "half away from zero, as Postgres rounds, not half to even"
    );
    let cached = catalog_add(&[
        "--input-per-mtok",
        "1",
        "--output-per-mtok",
        "1",
        "--cache-read-per-mtok",
        "0.12345649",
        "--cache-write-per-mtok=-0.0000004",
    ])
    .expect("clap takes it");
    let row = super::catalog::model_row(&cached, merge).expect("a cache price rounds too");
    assert_eq!(row.cache_read_per_mtok, Some(exact("0.123456")));
    assert_eq!(
        row.cache_write_per_mtok,
        Some(Decimal::ZERO),
        "a negative that rounds to nothing is a zero"
    );
}
