
use super::*;

/// A transcript line, spelled the way Claude Code spells one.
fn line(session: &str, msg_id: &str, ts: &str, model: &str, u: [u64; 4]) -> String {
    serde_json::json!({
        "type": "assistant",
        "uuid": Uuid::new_v4().to_string(),
        "sessionId": session,
        "timestamp": ts,
        "message": {
            "id": msg_id,
            "model": model,
            "role": "assistant",
            "usage": {
                "input_tokens": u[0],
                "output_tokens": u[1],
                "cache_read_input_tokens": u[2],
                "cache_creation_input_tokens": u[3],
            },
        },
    })
    .to_string()
}

/// The same line with no `sessionId` at all — the shape finding C11 is
/// about. Real transcripts contain them: a resumed session's continuation
/// file, and some tool versions, simply omit the field.
fn line_without_session(msg_id: &str, ts: &str, model: &str, u: [u64; 4]) -> String {
    let mut v: serde_json::Value =
        serde_json::from_str(&line("ignored", msg_id, ts, model, u)).expect("fixture json");
    v.as_object_mut().expect("object").remove("sessionId");
    v.to_string()
}

fn at(ts: &str) -> OffsetDateTime {
    OffsetDateTime::parse(ts, &Rfc3339).expect("fixture timestamp")
}

/// Write fixture transcripts into a directory of this test's own.
fn fixture_dir(name: &str, files: &[(&str, String)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oag-import-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("project")).expect("fixture dir");
    for (file, body) in files {
        std::fs::write(dir.join("project").join(file), body).expect("fixture file");
    }
    dir
}

/// A catalogue row, for tests that care only about its identity and price.
fn model_row(
    id: &str,
    upstream: &str,
    input: Decimal,
    output: Decimal,
) -> oag_store::rows::ModelRow {
    oag_store::rows::ModelRow {
        id: id.to_owned(),
        provider: id.split('/').next().unwrap_or("openai").to_owned(),
        upstream_name: upstream.to_owned(),
        input_per_mtok: input,
        output_per_mtok: output,
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
        context_window: 200_000,
        max_output_tokens: 64_000,
        supports_vision: true,
        supports_tools: true,
        supports_reasoning: true,
        supports_prompt_cache: true,
        display_label: None,
    }
}

fn catalog() -> Prices {
    Prices::index(
        &[oag_store::rows::ModelRow {
            id: "anthropic/claude-opus-5".to_owned(),
            provider: "anthropic".to_owned(),
            upstream_name: "claude-opus-5".to_owned(),
            input_per_mtok: Decimal::from(15),
            output_per_mtok: Decimal::from(75),
            cache_read_per_mtok: Some(Decimal::from(1)),
            cache_write_per_mtok: Some(Decimal::from(18)),
            context_window: 200_000,
            max_output_tokens: 64_000,
            supports_vision: true,
            supports_tools: true,
            supports_reasoning: true,
            supports_prompt_cache: true,
            display_label: None,
        }],
        "anthropic",
    )
}

#[test]
fn one_reply_written_as_several_lines_is_billed_once() {
    // An API response is split across the transcript one line per content
    // block, each with its own uuid and a byte-identical usage object.
    // Summing lines rather than messages roughly doubles every figure.
    let body = [
        line(
            "s1",
            "msg_a",
            "2026-01-01T00:00:00Z",
            "claude-opus-5",
            [10, 20, 5000, 0],
        ),
        line(
            "s1",
            "msg_a",
            "2026-01-01T00:00:01Z",
            "claude-opus-5",
            [10, 20, 5000, 0],
        ),
        line(
            "s1",
            "msg_b",
            "2026-01-01T00:00:05Z",
            "claude-opus-5",
            [10, 30, 5000, 0],
        ),
    ]
    .join("\n");
    let dir = fixture_dir("dedupe", &[("a.jsonl", body)]);
    let scan = scan_claude_code(&dir).expect("scan");
    assert_eq!(scan.sessions["s1"].messages.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_the_ledger_already_has_is_skipped_whole() {
    let body = [
        line(
            "s1",
            "msg_a",
            "2026-01-01T00:00:00Z",
            "claude-opus-5",
            [10, 20, 5000, 0],
        ),
        line(
            "s1",
            "msg_b",
            "2026-01-01T00:05:00Z",
            "claude-opus-5",
            [11, 21, 6000, 0],
        ),
    ]
    .join("\n");
    let dir = fixture_dir("proxied", &[("a.jsonl", body)]);
    let scan = scan_claude_code(&dir).expect("scan");
    // One gateway row, matching one of the two messages. The base URL is a
    // per-process setting, so one match condemns the session entire.
    let ledger = LedgerIndex::build(vec![(at("2026-01-01T00:00:30Z"), 10, 20, 5000, 0)]);
    let plan = plan(scan, &ledger, &catalog(), Source::ClaudeCode, None);
    assert!(plan.rows.is_empty(), "nothing may be imported");
    assert_eq!(plan.skipped_as_proxied(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_naming_a_model_its_own_provider_cannot_serve_went_through_a_gateway() {
    // Proof, not inference: Claude Code talking to Anthropic can only be
    // answered by an Anthropic model, so a transcript naming grok went
    // through something that rewrote the request. This catches the case
    // fingerprinting structurally cannot — a session proxied by a gateway
    // whose rows live in some other database, which would otherwise be
    // imported into these books a second time.
    let body = [
        line(
            "s1",
            "m1",
            "2026-01-01T00:00:00Z",
            "claude-opus-5",
            [1, 2, 3, 0],
        ),
        line("s1", "m2", "2026-01-01T00:01:00Z", "grok-4.5", [4, 5, 6, 0]),
    ]
    .join("\n");
    let dir = fixture_dir("foreign", &[("a.jsonl", body)]);
    let scan = scan_claude_code(&dir).expect("scan");
    // An empty ledger: nothing here could have matched a fingerprint.
    let plan = plan(
        scan,
        &LedgerIndex::build(vec![]),
        &catalog(),
        Source::ClaudeCode,
        None,
    );
    assert!(plan.rows.is_empty(), "the whole session is skipped");
    assert_eq!(plan.skipped_as_foreign(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_model_the_catalog_has_never_heard_of_is_not_treated_as_foreign() {
    // The test is the vendor's family name, never catalog membership. A
    // catalog is whatever has been seeded, so keying on it would condemn an
    // honest session the day Anthropic ships a model nobody has seeded yet.
    assert!(is_native_model("claude-opus-9-not-yet-released"));
    assert!(is_native_model("anthropic/claude-haiku-4.5"));
    assert!(!is_native_model("grok-4.6"));
    assert!(!is_native_model("stealth/ox-alpha"));
}

#[test]
fn a_session_the_ledger_has_never_seen_is_imported() {
    let body = [
        line(
            "s1",
            "msg_a",
            "2026-01-01T00:00:00Z",
            "claude-opus-5",
            [10, 20, 5000, 0],
        ),
        line(
            "s1",
            "msg_b",
            "2026-01-01T00:05:00Z",
            "claude-opus-5",
            [11, 21, 6000, 0],
        ),
    ]
    .join("\n");
    let dir = fixture_dir("direct", &[("a.jsonl", body)]);
    let scan = scan_claude_code(&dir).expect("scan");
    // A gateway row of a different shape, and one of the right shape but
    // hours away: neither is this session.
    let ledger = LedgerIndex::build(vec![
        (at("2026-01-01T00:00:30Z"), 99, 99, 9999, 0),
        (at("2026-01-01T09:00:00Z"), 10, 20, 5000, 0),
    ]);
    let plan = plan(scan, &ledger, &catalog(), Source::ClaudeCode, None);
    assert_eq!(plan.rows.len(), 2);
    assert!(plan.skipped.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_tiny_token_shape_is_not_treated_as_proof_of_anything() {
    // (2, 1, 0, 0) is a shape unrelated calls land on constantly. Letting
    // one of those condemn a session would drop real history on a
    // coincidence, so only a distinctive fingerprint counts as evidence.
    let body = line(
        "s1",
        "msg_a",
        "2026-01-01T00:00:00Z",
        "claude-opus-5",
        [2, 1, 0, 0],
    );
    let dir = fixture_dir("tiny", &[("a.jsonl", body)]);
    let scan = scan_claude_code(&dir).expect("scan");
    let ledger = LedgerIndex::build(vec![(at("2026-01-01T00:00:01Z"), 2, 1, 0, 0)]);
    let plan = plan(scan, &ledger, &catalog(), Source::ClaudeCode, None);
    assert_eq!(plan.rows.len(), 1, "a coincidence is not a match");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_model_the_catalog_lacks_is_imported_with_no_cost_rather_than_a_cost_of_zero() {
    let body = [
        line(
            "s1",
            "msg_a",
            "2026-01-01T00:00:00Z",
            "claude-opus-5",
            [1000, 1000, 0, 0],
        ),
        // A model of the right family that nobody has seeded: unpriceable,
        // but not evidence the session went through a gateway. A foreign
        // family name would be skipped outright and never reach pricing.
        line(
            "s1",
            "msg_b",
            "2026-01-01T00:01:00Z",
            "claude-opus-9-unseeded",
            [1000, 1000, 0, 0],
        ),
    ]
    .join("\n");
    let dir = fixture_dir("unpriced", &[("a.jsonl", body)]);
    let scan = scan_claude_code(&dir).expect("scan");
    let plan = plan(
        scan,
        &LedgerIndex::default(),
        &catalog(),
        Source::ClaudeCode,
        None,
    );
    assert_eq!(plan.rows.len(), 2);
    assert_eq!(plan.unpriced.get("claude-opus-9-unseeded"), Some(&1));
    let unpriced = plan
        .rows
        .iter()
        .find(|r| r.model_id.ends_with("claude-opus-9-unseeded"))
        .expect("the unpriced row");
    assert_eq!(unpriced.listed, None, "no cost, not a zero cost");
    let priced = plan
        .rows
        .iter()
        .find(|r| r.model_id == "anthropic/claude-opus-5")
        .expect("the priced row");
    assert!(priced.listed.is_some_and(|c| c > Decimal::ZERO));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_torn_line_is_stepped_over_rather_than_ending_the_run() {
    // A transcript is appended to live, so a crash leaves a half-written
    // last line. Aborting there would make the importer unusable against
    // exactly the machine it runs on.
    let body = [
        line(
            "s1",
            "msg_a",
            "2026-01-01T00:00:00Z",
            "claude-opus-5",
            [10, 20, 5000, 0],
        ),
        r#"{"type":"assistant","message":{"usa"#.to_owned(),
        String::new(),
        line(
            "s1",
            "msg_b",
            "2026-01-01T00:01:00Z",
            "claude-opus-5",
            [11, 21, 6000, 0],
        ),
    ]
    .join("\n");
    let dir = fixture_dir("torn", &[("a.jsonl", body)]);
    let scan = scan_claude_code(&dir).expect("scan");
    assert_eq!(scan.malformed, 1);
    assert_eq!(scan.sessions["s1"].messages.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resumed_session_copied_into_a_second_file_is_still_one_session() {
    // A resumed session writes its predecessor's entries into a new file,
    // so the same call appears under two filenames. Merging on the id in
    // the entry rather than the filename is what keeps it one call.
    let first = line(
        "s1",
        "msg_a",
        "2026-01-01T00:00:00Z",
        "claude-opus-5",
        [10, 20, 5000, 0],
    );
    let second = [
        first.clone(),
        line(
            "s1",
            "msg_b",
            "2026-01-01T00:01:00Z",
            "claude-opus-5",
            [11, 21, 6000, 0],
        ),
    ]
    .join("\n");
    let dir = fixture_dir("resumed", &[("a.jsonl", first), ("b.jsonl", second)]);
    let scan = scan_claude_code(&dir).expect("scan");
    assert_eq!(scan.sessions.len(), 1);
    assert_eq!(scan.sessions["s1"].messages.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_cutoff_excludes_a_session_that_runs_past_it() {
    let body = [
        line(
            "s1",
            "msg_a",
            "2026-01-01T00:00:00Z",
            "claude-opus-5",
            [10, 20, 5000, 0],
        ),
        line(
            "s2",
            "msg_b",
            "2026-03-01T00:00:00Z",
            "claude-opus-5",
            [11, 21, 6000, 0],
        ),
    ]
    .join("\n");
    let dir = fixture_dir("cutoff", &[("a.jsonl", body)]);
    let scan = scan_claude_code(&dir).expect("scan");
    let plan = plan(
        scan,
        &LedgerIndex::default(),
        &catalog(),
        Source::ClaudeCode,
        Some(at("2026-02-01T00:00:00Z")),
    );
    assert_eq!(plan.rows.len(), 1);
    assert_eq!(plan.skipped, vec![("s2".to_owned(), Skip::AfterCutoff)]);
    let _ = std::fs::remove_dir_all(&dir);
}

fn seat(kind: CredentialKind) -> Seat {
    Seat {
        id: Uuid::new_v4(),
        name: "claude-personal".to_owned(),
        provider: "anthropic".to_owned(),
        kind,
    }
}

/// One priced row, the shape every booking test asks its question about.
fn a_priced_row(name: &str) -> Pending {
    let body = line(
        "s1",
        "msg_a",
        "2026-01-01T00:00:00Z",
        "claude-opus-5",
        [10_000, 2_000, 50_000, 0],
    );
    let dir = fixture_dir(name, &[("a.jsonl", body)]);
    let plan = plan(
        scan_claude_code(&dir).expect("scan"),
        &LedgerIndex::default(),
        &catalog(),
        Source::ClaudeCode,
        None,
    );
    let _ = std::fs::remove_dir_all(&dir);
    plan.rows.into_iter().next().expect("one planned row")
}

#[test]
fn an_import_attributed_to_a_subscription_costs_nothing_and_books_the_bill_it_displaced() {
    // The whole point of --account. This usage ran on a flat rate that was
    // already paid, so its marginal cost is zero; recording the list price
    // as spend would put a bill in the ledger that no invoice matches, and
    // inflate every saving figure derived from it.
    let row = a_priced_row("seat-booking");
    let (cost, api) = row.booked(Some(&seat(CredentialKind::OAuth)));
    assert_eq!(cost, Decimal::ZERO, "a paid-for token costs nothing more");
    assert!(
        api > Decimal::ZERO,
        "what the fee displaced is still worth knowing"
    );
    assert_eq!(api, row.listed.expect("priced"));
}

#[test]
fn an_import_attributed_to_a_metered_key_books_the_list_price_as_real_spend() {
    // On a metered credential the list price *is* the cost — the invoice
    // exists — so attribution changes whose row it is and not what it cost.
    let row = a_priced_row("metered-booking");
    let listed = row.listed.expect("priced");
    assert_eq!(
        row.booked(Some(&seat(CredentialKind::ApiKey))),
        (listed, listed)
    );
}

#[test]
fn an_unattributed_import_is_booked_exactly_as_it_was_before_account_existed() {
    // No flag, no change: the rows are metered spend at list. Silently
    // zeroing them because a subscription is the likelier explanation would
    // guess at money, and the guess is invisible once written.
    let row = a_priced_row("unattributed-booking");
    let listed = row.listed.expect("priced");
    assert_eq!(row.booked(None), (listed, listed));
}

#[test]
fn only_a_subscription_row_matches_the_predicate_the_headline_excludes() {
    // `cost_usd = 0 AND counterfactual_api_usd > 0` is what "subscription
    // seat" means across this tree: the headline totals exclude it and the
    // per-seat table selects it. So this assertion is the double-count
    // check — a seat import is counted once, on its seat's line, and an
    // unattributed or metered import is counted once, in the headline.
    let row = a_priced_row("headline-predicate");
    let is_seat_row = |(cost, api): (Decimal, Decimal)| cost.is_zero() && api > Decimal::ZERO;
    assert!(is_seat_row(row.booked(Some(&seat(CredentialKind::OAuth)))));
    assert!(!is_seat_row(
        row.booked(Some(&seat(CredentialKind::ApiKey)))
    ));
    assert!(!is_seat_row(row.booked(None)));
}

#[test]
fn an_unpriced_model_on_a_seat_is_not_mistaken_for_a_seat_row() {
    // A row nobody could price has a displaced bill of zero, so it fails
    // the seat predicate and stays in the headline contributing nothing.
    // Better than the alternative: a seat line whose API value is a
    // silently missing model rather than a small one.
    let unpriced = Pending {
        listed: None,
        ..a_priced_row("unpriced-seat")
    };
    assert_eq!(
        unpriced.booked(Some(&seat(CredentialKind::OAuth))),
        (Decimal::ZERO, Decimal::ZERO)
    );
}

#[test]
fn the_same_message_always_derives_the_same_row_identity() {
    // The idempotency key. A re-run must produce the same request_id and
    // the same source_ref for a message, or the ledger's unique index has
    // nothing to reject the second copy with.
    let body = line(
        "s1",
        "msg_a",
        "2026-01-01T00:00:00Z",
        "claude-opus-5",
        [10, 20, 5000, 0],
    );
    let dir = fixture_dir("stable", &[("a.jsonl", body)]);
    let first = plan(
        scan_claude_code(&dir).expect("scan"),
        &LedgerIndex::default(),
        &catalog(),
        Source::ClaudeCode,
        None,
    );
    let second = plan(
        scan_claude_code(&dir).expect("scan"),
        &LedgerIndex::default(),
        &catalog(),
        Source::ClaudeCode,
        None,
    );
    assert_eq!(first.rows[0].source_ref, "claude-code:msg_a");
    assert_eq!(first.rows[0].request_id, second.rows[0].request_id);
    let _ = std::fs::remove_dir_all(&dir);
}

/// C11's other half: the rows a previous binary already wrote.
///
/// Changing the derived `source_ref` changes the identity every re-import
/// recognises — the unique index on `source_ref` and the `uuid_v5` that
/// becomes `request_id`. Without 0017 the first import after this upgrade
/// matches nothing and books the whole corpus a second time, and reports it
/// as a clean first import because every row really was written.
///
/// Driven against the database rather than asserted about the format,
/// because the fix is a migration and what has to be true is a property of
/// the rows: an old-shaped row is re-keyed to exactly what `plan()` now
/// derives, and two old rows for one message become one.
#[tokio::test]
async fn the_backfill_rekeys_old_imports_onto_what_the_importer_now_derives() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    // Two sessions quoting one message, plus a message seen once. The
    // shape a pre-C11 binary wrote.
    let tag = Uuid::new_v4().simple().to_string();
    let shared = format!("msg_shared_{tag}");
    let only = format!("msg_only_{tag}");
    let old_rows = [
        (
            format!("claude-code:sess_a_{tag}:{shared}"),
            "2026-09-01T10:00:00Z",
        ),
        (
            format!("claude-code:sess_b_{tag}:{shared}"),
            "2026-09-02T10:00:00Z",
        ),
        (
            format!("claude-code:sess_a_{tag}:{only}"),
            "2026-09-01T11:00:00Z",
        ),
    ];
    for (source_ref, at) in &old_rows {
        sqlx::query(
            "INSERT INTO usage_event (request_id, attempt, origin, source_ref, model_id, \
                 tier, selection_reason, input_tokens, output_tokens, cost_usd, \
                 counterfactual_usd, counterfactual_api_usd, status, occurred_at) \
                 VALUES ($1, 0, 'claude-code', $2, 'anthropic/claude-opus-5', 'frontier', \
                         'imported', 100, 20, 0, 0, 1.00, 200, $3::timestamptz)",
        )
        .bind(Uuid::new_v5(&IMPORT_NAMESPACE, source_ref.as_bytes()))
        .bind(source_ref)
        .bind(at)
        .execute(db.pool())
        .await
        .expect("seed a pre-C11 row");
    }

    // The migration has already run, so re-key these by hand exactly as it
    // does — the statement is the same one, against rows seeded after it.
    // What is asserted is the outcome the migration defines, not the run.
    rekey_claude_code_imports(&db).await;

    let refs: Vec<String> = sqlx::query_scalar(
        "SELECT source_ref FROM usage_event WHERE source_ref LIKE $1 ORDER BY source_ref",
    )
    .bind(format!("%{tag}%"))
    .fetch_all(db.pool())
    .await
    .expect("read back");

    assert_eq!(
        refs,
        vec![
            format!("claude-code:{only}"),
            format!("claude-code:{shared}")
        ],
        "the session is gone from the key, and the message quoted in two \
             sessions is one row rather than two"
    );

    // And the identity a re-import derives finds them. This is the whole
    // point: `plan()` computes both of these, and both must match.
    for message in [&shared, &only] {
        let source_ref = format!("claude-code:{message}");
        let found: Option<Uuid> =
            sqlx::query_scalar("SELECT request_id FROM usage_event WHERE source_ref = $1")
                .bind(&source_ref)
                .fetch_optional(db.pool())
                .await
                .expect("read back");
        assert_eq!(
            found,
            Some(Uuid::new_v5(&IMPORT_NAMESPACE, source_ref.as_bytes())),
            "a re-import derives this request_id for {source_ref}; if the row \
                 carries another one the primary key does not collide and the \
                 import writes it again"
        );
    }

    sqlx::query("DELETE FROM usage_event WHERE source_ref LIKE $1")
        .bind(format!("%{tag}%"))
        .execute(db.pool())
        .await
        .expect("clean up");
}

/// 0017 itself, run against rows this test seeded.
///
/// The migration text, not a copy of it: a migration runs once and long
/// before a test can plant a row in the old shape, so the alternative is
/// either a test that asserts nothing about those rows or a second copy of
/// the statements that agrees with the original only until somebody edits
/// one of them. `include_str!` removes the choice.
///
/// Safe to run twice: it creates its helper, uses it, drops it, and its
/// `WHERE` matches only rows still in the old shape.
async fn rekey_claude_code_imports(db: &Db) {
    const MIGRATION: &str =
        include_str!("../../../../../migrations/0017_rekey_claude_code_imports.sql");
    sqlx::raw_sql(MIGRATION)
        .execute(db.pool())
        .await
        .expect("0017 runs against the rows seeded above");
}

/// C15. A spelling two models answer to prices neither.
///
/// The index accepts three keys per row — the provider's own spelling, the
/// canonical id, and the tail of the id — and the tail is the one that
/// collides: `openai/gpt-5` and `azure/gpt-5` both end `gpt-5`. `insert`
/// returned the displaced entry and it was thrown away, so the last row in
/// catalogue order won a coin toss the caller could not see, and every
/// transcript row matching that slug was priced against whichever model
/// that happened to be.
#[test]
fn a_slug_two_models_answer_to_is_not_priced_by_a_coin_toss() {
    let rows = vec![
        model_row(
            "openai/gpt-5",
            "gpt-5-2026",
            Decimal::from(10),
            Decimal::from(30),
        ),
        model_row(
            "openai/gpt-5-mini",
            "gpt-5",
            Decimal::from(1),
            Decimal::from(3),
        ),
    ];
    let prices = Prices::index(&rows, "openai");

    // `gpt-5` is the tail of the first row's id AND the second row's
    // upstream name. Two models, one spelling, no answer.
    assert!(
        prices.get("gpt-5").is_none(),
        "pricing this against either model is a guess, and a guess that \
             lands in the ledger as money"
    );

    // Every unambiguous spelling still resolves, which is the reason for
    // marking the key rather than dropping the rows.
    assert_eq!(
        prices.get("openai/gpt-5").map(|(id, _)| id.as_str()),
        Some("openai/gpt-5")
    );
    assert_eq!(
        prices.get("gpt-5-2026").map(|(id, _)| id.as_str()),
        Some("openai/gpt-5")
    );
    assert_eq!(
        prices.get("openai/gpt-5-mini").map(|(id, _)| id.as_str()),
        Some("openai/gpt-5-mini")
    );
}

/// C11. The same message found in two files is still one API call.
///
/// A transcript line carries the provider's own message id, which is
/// globally unique. The session is the file it happened to be found in —
/// and when a line has no `sessionId`, which real transcripts contain,
/// the scanner falls back to the *filename*. Keyed on that, a resumed
/// session's continuation file derived a second `source_ref` for the same
/// message, sailed past the unique index that exists to stop exactly this,
/// and booked one API call's tokens twice.
///
/// Every fixture in this module wrote `sessionId` into every line, so
/// nothing here could reach the fallback.
#[test]
fn a_message_with_no_session_id_derives_one_identity_wherever_it_is_found() {
    let body =
        |ts: &str| line_without_session("msg_shared", ts, "claude-opus-5", [10, 20, 5000, 0]);

    // The same message in two differently named files — a resumed session,
    // which is the shape `a_resumed_session_copied_into_a_second_file_is_still_one_session`
    // already tests for lines that DO carry a session id.
    let dir = fixture_dir(
        "no-session",
        &[
            ("first.jsonl", body("2026-01-01T00:00:00Z")),
            ("second.jsonl", body("2026-01-01T00:00:00Z")),
        ],
    );
    let planned = plan(
        scan_claude_code(&dir).expect("scan"),
        &LedgerIndex::default(),
        &catalog(),
        Source::ClaudeCode,
        None,
    );

    let refs: std::collections::BTreeSet<&str> =
        planned.rows.iter().map(|r| r.source_ref.as_str()).collect();
    assert_eq!(
        refs,
        ["claude-code:msg_shared"].into_iter().collect(),
        "two files, one message id, one identity — keyed on the filename \
             this produced two, and the ledger booked the tokens twice"
    );
    let ids: std::collections::BTreeSet<Uuid> = planned.rows.iter().map(|r| r.request_id).collect();
    assert_eq!(ids.len(), 1, "and one request id, which is the primary key");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The import against a real Postgres.
///
/// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it. Idempotence
/// is enforced by a unique index, and an index is not a thing that can be
/// tested without the database that holds it.
#[tokio::test]
async fn a_second_run_writes_nothing_and_a_dry_run_writes_nothing_at_all() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    // Message ids unique per run, because `source_ref` is now keyed on
    // them alone — which is what a provider's message id already is.
    let session = format!("s-{}", Uuid::new_v4());
    let (msg_a, msg_b) = (format!("msg_a-{session}"), format!("msg_b-{session}"));
    let body = [
        line(
            &session,
            &msg_a,
            "2026-01-01T00:00:00Z",
            "claude-opus-5",
            [10, 20, 5000, 0],
        ),
        line(
            &session,
            &msg_b,
            "2026-01-01T00:01:00Z",
            "claude-opus-5",
            [11, 21, 6000, 0],
        ),
    ]
    .join("\n");
    let dir = fixture_dir("apply", &[("a.jsonl", body)]);
    let path = dir.to_string_lossy().into_owned();

    let count = |db: Db, session: String| async move {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_event WHERE source_ref LIKE $1")
            .bind(format!("claude-code:%-{session}"))
            .fetch_one(db.pool())
            .await
            .expect("count")
    };

    // The dry run is the default, and it must leave the ledger untouched.
    import(&db, Source::ClaudeCode, Some(&path), None, None, false)
        .await
        .expect("dry run");
    assert_eq!(count(db.clone(), session.clone()).await, 0);

    import(&db, Source::ClaudeCode, Some(&path), None, None, true)
        .await
        .expect("apply");
    assert_eq!(count(db.clone(), session.clone()).await, 2);

    // The second run re-derives the same keys and loses to the index.
    import(&db, Source::ClaudeCode, Some(&path), None, None, true)
        .await
        .expect("re-apply");
    assert_eq!(
        count(db.clone(), session.clone()).await,
        2,
        "a re-run must not append a second copy of the same money"
    );

    // And the marking is what makes the import removable on its own.
    sqlx::query("DELETE FROM usage_event WHERE source_ref LIKE $1")
        .bind(format!("claude-code:%-{session}"))
        .execute(db.pool())
        .await
        .expect("cleanup");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A ledger holding one priced model, a subscription, a metered key, and
/// one session's transcript on disk.
///
/// The setup both attribution tests need, and none of what either is
/// asserting — kept together so the two can ask their own question in a
/// dozen lines each rather than sharing one test that asks both.
struct Attributed {
    db: Db,
    sub: String,
    key: String,
    source_ref: String,
    path: String,
    dir: PathBuf,
    model: String,
}

impl Attributed {
    /// `None` when `OAG_TEST_DATABASE_URL` is unset — how these skip on a
    /// machine with no Postgres rather than failing on one.
    async fn seed(name: &str) -> Option<Self> {
        let url = std::env::var("OAG_TEST_DATABASE_URL").ok()?;
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        // A price, so the row has a list value to book or to displace.
        // Without one, a seat row and an unpriceable row both read zero and
        // the assertion could not tell the two apart.
        // The upstream name is unique per run as well as the id, and the
        // transcript below names that one. Every run of this fixture leaves
        // its catalogue row behind, so a fixed `claude-opus-5` accumulated
        // — and since C15 a slug several rows answer to is deliberately
        // unpriceable, which is the correct behaviour finding the fixture
        // rather than the other way round.
        let slug = format!("claude-opus-5-{}", Uuid::new_v4());
        let model = format!("anthropic/{slug}");
        sqlx::query(
            "INSERT INTO model_catalog (id, provider, upstream_name, input_per_mtok, \
                 output_per_mtok, cache_read_per_mtok, cache_write_per_mtok, context_window, \
                 max_output_tokens) VALUES ($1, 'anthropic', $2, 15, 75, 1, 18, \
                 200000, 64000)",
        )
        .bind(&model)
        .bind(&slug)
        .execute(db.pool())
        .await
        .expect("seed catalog");

        // Two credentials, deliberately of the two kinds that book apart.
        let sub = format!("sub-{}", Uuid::new_v4());
        let key = format!("key-{}", Uuid::new_v4());
        for (account, kind) in [(&sub, "oauth"), (&key, "api_key")] {
            sqlx::query(
                "INSERT INTO account (id, name, provider, kind, credentials_sealed, \
                     credentials_nonce) VALUES ($1, $2, 'anthropic', $3, '\\x00', '\\x00')",
            )
            .bind(Uuid::new_v4())
            .bind(account)
            .bind(kind)
            .execute(db.pool())
            .await
            .expect("seed account");
        }

        // A message id unique per run: `source_ref` is keyed on it alone
        // for this source now, so a fixed one would collide across runs of
        // this same test — which is the property under test, seen from the
        // other side.
        let session = format!("s-{}", Uuid::new_v4());
        let msg = format!("msg_a-{session}");
        let body = line(
            &session,
            &msg,
            "2026-01-01T00:00:00Z",
            &slug,
            [10_000, 2_000, 0, 0],
        );
        let dir = fixture_dir(name, &[("a.jsonl", body)]);
        Some(Self {
            db,
            sub,
            key,
            source_ref: format!("claude-code:{msg}"),
            path: dir.to_string_lossy().into_owned(),
            dir,
            model,
        })
    }

    async fn rows(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_event WHERE source_ref = $1")
            .bind(&self.source_ref)
            .fetch_one(self.db.pool())
            .await
            .expect("count")
    }

    /// Rows first: the account is a foreign key of the usage it paid for.
    async fn cleanup(self) {
        let run = async |sql: &'static str, arg: &String| {
            sqlx::query(sql)
                .bind(arg)
                .execute(self.db.pool())
                .await
                .expect("cleanup");
        };
        run(
            "DELETE FROM usage_event WHERE source_ref = $1",
            &self.source_ref,
        )
        .await;
        for account in [&self.sub, &self.key] {
            run("DELETE FROM account WHERE name = $1", account).await;
        }
        run("DELETE FROM model_catalog WHERE id = $1", &self.model).await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Attribution reaching the columns the reports actually read.
///
/// The arithmetic is unit-tested above; what needs a database is that it
/// lands where `seat_summaries` looks for it and where the headline does
/// not.
#[tokio::test]
async fn an_import_attributed_to_a_seat_is_booked_as_that_seats_row_and_left_out_of_the_headline() {
    let Some(fx) = Attributed::seed("attributed").await else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    import(
        &fx.db,
        Source::ClaudeCode,
        Some(&fx.path),
        None,
        Some(&fx.sub),
        true,
    )
    .await
    .expect("apply");

    let booked: (Decimal, Decimal, Option<Uuid>) = sqlx::query_as(
        "SELECT cost_usd, counterfactual_api_usd, account_id FROM usage_event \
             WHERE source_ref = $1",
    )
    .bind(&fx.source_ref)
    .fetch_one(fx.db.pool())
    .await
    .expect("the imported row");
    assert_eq!(
        booked.0,
        Decimal::ZERO,
        "a subscription's tokens are already paid for"
    );
    assert!(booked.1 > Decimal::ZERO, "the displaced bill is recorded");
    assert!(booked.2.is_some(), "and it belongs to the seat that paid");

    // The headline's own predicate, spelled as `summary` spells it. Copied
    // rather than shared because the point is that the two agree: if that
    // query changes shape, this test should stop passing and say so.
    let in_headline: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM usage_event WHERE source_ref = $1 \
             AND NOT (cost_usd = 0 AND counterfactual_api_usd > 0)",
    )
    .bind(&fx.source_ref)
    .fetch_one(fx.db.pool())
    .await
    .expect("headline count");
    assert_eq!(
        in_headline, 0,
        "the seat's line already states this money, so the headline stating \
             it again would state it twice"
    );
    fx.cleanup().await;
}

#[tokio::test]
async fn a_revert_scoped_to_one_credential_removes_that_import_and_no_other() {
    // Attributing an import to the wrong subscription is the mistake
    // `--account` newly makes possible, so undoing it must be possible
    // without taking the other subscriptions' history along with it.
    let Some(fx) = Attributed::seed("reverted").await else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    import(
        &fx.db,
        Source::ClaudeCode,
        Some(&fx.path),
        None,
        Some(&fx.sub),
        true,
    )
    .await
    .expect("apply");
    assert_eq!(fx.rows().await, 1);

    revert(&fx.db, ORIGIN_CLAUDE_CODE, Some(&fx.key), true)
        .await
        .expect("revert the other credential");
    assert_eq!(
        fx.rows().await,
        1,
        "another seat's revert is not this one's"
    );

    revert(&fx.db, ORIGIN_CLAUDE_CODE, Some(&fx.sub), true)
        .await
        .expect("revert");
    assert_eq!(
        fx.rows().await,
        0,
        "what the import added, the revert removes"
    );
    fx.cleanup().await;
}

// ── the Grok CLI ─────────────────────────────────────────────────────────

/// One model's slice of a turn, as `modelUsage` spells it:
/// `(slug, [input, output, cached read, cache creation, reasoning], ticks)`.
///
/// `input` is the gross figure the file carries, cached reads included —
/// the fixture speaks the format's own dialect so that the subtraction the
/// importer does is a thing under test rather than a thing baked in here.
type GrokModel<'a> = (&'a str, [u64; 5], u64);

fn grok_usage_json(slice: &GrokModel<'_>) -> serde_json::Value {
    let [input, output, cached, created, reasoning] = slice.1;
    serde_json::json!({
        "inputTokens": input,
        "outputTokens": output,
        "totalTokens": input + output,
        "cachedReadTokens": cached,
        "cacheCreationTokens": created,
        "reasoningTokens": reasoning,
        "modelCalls": 3,
        "apiDurationMs": 1234,
        "costUsdTicks": slice.2,
    })
}

/// One `updates.jsonl` line, spelled the way the Grok CLI spells one.
fn grok_turn(session: &str, prompt: &str, ts: i64, models: &[GrokModel<'_>]) -> String {
    let mut totals = [0u64; 5];
    let mut ticks = 0u64;
    let mut per_model = serde_json::Map::new();
    for slice in models {
        for (t, v) in totals.iter_mut().zip(slice.1) {
            *t += v;
        }
        ticks += slice.2;
        per_model.insert(slice.0.to_owned(), grok_usage_json(slice));
    }
    let mut usage = grok_usage_json(&("", totals, ticks));
    usage["modelUsage"] = serde_json::Value::Object(per_model);
    usage["numTurns"] = serde_json::json!(models.len());
    serde_json::json!({
        "timestamp": ts,
        "method": "_x.ai/session/update",
        "params": {
            "sessionId": session,
            "update": {
                "sessionUpdate": GROK_TURN_COMPLETED,
                "prompt_id": prompt,
                "stop_reason": "end_turn",
                "usage": usage,
            },
        },
    })
    .to_string()
}

/// Grok session logs live at `<cwd>/<session-uuid>/updates.jsonl`, and the
/// directory name is the importer's fallback session id — so a fixture that
/// flattened the layout would not exercise the walk that finds them.
fn grok_fixture_dir(name: &str, sessions: &[(&str, String)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oag-grok-{name}-{}", Uuid::new_v4()));
    for (session, body) in sessions {
        let leaf = dir.join("%2Fsome%2Fproject").join(session);
        std::fs::create_dir_all(&leaf).expect("fixture dir");
        std::fs::write(leaf.join(GROK_USAGE_LOG), body).expect("fixture file");
    }
    dir
}

fn grok_catalog() -> Prices {
    Prices::index(
        &[oag_store::rows::ModelRow {
            id: "xai/grok-4.6".to_owned(),
            provider: "xai".to_owned(),
            upstream_name: "grok-4.6".to_owned(),
            input_per_mtok: Decimal::from(2),
            output_per_mtok: Decimal::from(6),
            cache_read_per_mtok: Some(Decimal::new(5, 1)),
            cache_write_per_mtok: Some(Decimal::from(2)),
            context_window: 500_000,
            max_output_tokens: 64_000,
            supports_vision: true,
            supports_tools: true,
            supports_reasoning: true,
            supports_prompt_cache: true,
            display_label: None,
        }],
        "xai",
    )
}

fn unix(ts: &str) -> i64 {
    at(ts).unix_timestamp()
}

fn grok_plan(dir: &Path, ledger: &LedgerIndex) -> Plan {
    plan(
        scan_grok_cli(dir).expect("scan"),
        ledger,
        &grok_catalog(),
        Source::GrokCli,
        None,
    )
}

#[test]
fn a_turn_logged_twice_is_billed_once_rather_than_summed_into_a_multiple_of_itself() {
    // The hazard a per-turn log carries that a per-message one does not: if
    // these records were a running total, or if a crash replayed one, then
    // summing the file multiplies the session by however many records it
    // has. Keying on the turn's own `prompt_id` is what makes a second copy
    // of a turn overwrite the first instead of adding to it.
    let body = [
        grok_turn(
            "sess-1",
            "p1",
            unix("2026-01-01T00:00:00Z"),
            &[("grok-4.6-build", [100_000, 2_000, 80_000, 0, 1_500], 700)],
        ),
        grok_turn(
            "sess-1",
            "p1",
            unix("2026-01-01T00:00:00Z"),
            &[("grok-4.6-build", [100_000, 2_000, 80_000, 0, 1_500], 700)],
        ),
    ]
    .join("\n");
    let dir = grok_fixture_dir("replayed", &[("sess-1", body)]);
    let plan = grok_plan(&dir, &LedgerIndex::default());
    assert_eq!(
        plan.rows.len(),
        1,
        "one turn is one row however often logged"
    );
    assert_eq!(plan.tokens().input_tokens, 20_000);
    assert_eq!(plan.cross_check().map(|(ticks, _, _)| ticks), Some(700));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_turns_of_a_session_are_summed_because_the_records_are_per_turn_not_a_running_total() {
    // The verdict this importer stakes its arithmetic on, written as a
    // shape only a per-turn log can have: the second turn is *smaller* than
    // the first, which a cumulative counter cannot be. Read as a running
    // total, this session would be the 60,000 tokens of its last record;
    // read per turn it is the 90,000 both records actually cost, and the
    // whole file is evidence for the second reading.
    let body = [
        grok_turn(
            "sess-1",
            "p1",
            unix("2026-01-01T00:00:00Z"),
            &[("grok-4.6-build", [60_000, 0, 0, 0, 0], 100)],
        ),
        grok_turn(
            "sess-1",
            "p2",
            unix("2026-01-01T00:10:00Z"),
            &[("grok-4.6-build", [30_000, 0, 0, 0, 0], 50)],
        ),
    ]
    .join("\n");
    let dir = grok_fixture_dir("per-turn", &[("sess-1", body)]);
    let plan = grok_plan(&dir, &LedgerIndex::default());
    assert_eq!(plan.rows.len(), 2);
    assert_eq!(plan.tokens().input_tokens, 90_000);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_turn_split_across_two_models_becomes_a_row_each_at_that_models_own_price() {
    // The reason `modelUsage` is read at all rather than the turn total: a
    // lump attributed to one model prices the whole turn at that model's
    // rate, and there is no way to notice afterwards.
    let body = grok_turn(
        "sess-1",
        "p1",
        unix("2026-01-01T00:00:00Z"),
        &[
            ("grok-4.6-build", [10_000, 1_000, 0, 0, 0], 400),
            ("grok-4.5-build", [20_000, 2_000, 0, 0, 0], 300),
        ],
    );
    let dir = grok_fixture_dir("per-model", &[("sess-1", body)]);
    let plan = grok_plan(&dir, &LedgerIndex::default());
    assert_eq!(plan.rows.len(), 2, "one row per model, not one per turn");

    // The seeded model is priced under its catalog id, reached by stripping
    // the `-build` suffix the usage record adds and nothing else does.
    let priced = plan
        .rows
        .iter()
        .find(|r| r.model_id == "xai/grok-4.6")
        .expect("the seeded model");
    // 10k input at $2/Mtok plus 1k output at $6/Mtok.
    assert_eq!(priced.listed, Some(Decimal::new(26, 3)));

    // The unseeded one keeps its own name rather than borrowing the other's
    // price, and is named in the report so a catalog entry can fix it.
    let unseeded = plan
        .rows
        .iter()
        .find(|r| r.model_id == "xai/grok-4.5-build")
        .expect("the unseeded model");
    assert_eq!(unseeded.listed, None, "no cost, not a cost of zero");
    assert_eq!(plan.unpriced.get("grok-4.5-build"), Some(&1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cached_read_is_not_billed_a_second_time_as_uncached_input() {
    // `inputTokens` includes `cachedReadTokens`, so storing both as written
    // would bill the cached prefix twice — once at the input rate it never
    // paid. At these prices that is the difference between $0.05 and $0.20
    // on one turn, and it compounds over every turn of an agentic session.
    let body = grok_turn(
        "sess-1",
        "p1",
        unix("2026-01-01T00:00:00Z"),
        &[("grok-4.6-build", [100_000, 0, 90_000, 0, 0], 1)],
    );
    let dir = grok_fixture_dir("cached", &[("sess-1", body)]);
    let plan = grok_plan(&dir, &LedgerIndex::default());
    let row = &plan.rows[0];
    assert_eq!(row.usage.input_tokens, 10_000, "the uncached remainder");
    assert_eq!(row.usage.cache_read_tokens, 90_000);
    // 10k at $2/Mtok plus 90k at $0.50/Mtok.
    assert_eq!(row.listed, Some(Decimal::new(65, 3)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_this_gateway_was_serving_xai_during_is_skipped() {
    // The only ledger signal this source has. It cannot be a fingerprint
    // match: a Grok turn aggregates every model call it made and the ledger
    // holds one row per call, so the two sides count different things and
    // no comparison of token counts could ever agree, not even for a
    // session that certainly was proxied.
    let body = grok_turn(
        "sess-1",
        "p1",
        unix("2026-01-01T00:00:00Z"),
        &[("grok-4.6-build", [100_000, 2_000, 0, 0, 0], 700)],
    );
    let dir = grok_fixture_dir("overlap", &[("sess-1", body)]);

    // A gateway row inside the session's window condemns it whole.
    let overlapping = grok_plan(
        &dir,
        &LedgerIndex::activity(vec![at("2026-01-01T00:02:00Z")]),
    );
    assert!(overlapping.rows.is_empty());
    assert_eq!(overlapping.skipped_as_overlapping(), 1);

    // One well outside it does not. Otherwise a gateway that has ever
    // served xai would block every import this source could ever make.
    let elsewhere = grok_plan(
        &dir,
        &LedgerIndex::activity(vec![at("2026-01-01T09:00:00Z")]),
    );
    assert_eq!(elsewhere.rows.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_grok_model_name_is_never_read_as_proof_the_session_was_proxied() {
    // The Claude importer skips a session naming a model its provider does
    // not serve. Applying that here would be a category error: this CLI
    // asks for a Grok model and gets one whether it is pointed at x.ai or
    // at this gateway, so the name carries no information either way and a
    // model missing from the catalog would silently delete real history.
    let body = grok_turn(
        "sess-1",
        "p1",
        unix("2026-01-01T00:00:00Z"),
        &[("grok-4.9-unreleased", [50_000, 1_000, 0, 0, 0], 300)],
    );
    let dir = grok_fixture_dir("no-foreign", &[("sess-1", body)]);
    let plan = grok_plan(&dir, &LedgerIndex::default());
    assert_eq!(plan.skipped_as_foreign(), 0);
    assert_eq!(plan.rows.len(), 1, "unpriceable is not the same as proxied");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_torn_grok_log_is_stepped_over_rather_than_ending_the_run() {
    // These files are appended to live, and an operator importing a year of
    // sessions should not lose the run to the one that was open when their
    // laptop slept. A whole unreadable file costs its own sessions and
    // nothing else.
    let good = grok_turn(
        "sess-1",
        "p1",
        unix("2026-01-01T00:00:00Z"),
        &[("grok-4.6-build", [10_000, 1_000, 0, 0, 0], 400)],
    );
    let torn = [
        grok_turn(
            "sess-2",
            "p1",
            unix("2026-01-01T01:00:00Z"),
            &[("grok-4.6-build", [20_000, 1_000, 0, 0, 0], 400)],
        ),
        r#"{"params":{"update":{"sessionUpdate":"turn_comp"#.to_owned(),
    ]
    .join("\n");
    let dir = grok_fixture_dir(
        "torn",
        &[
            ("sess-1", good),
            ("sess-2", torn),
            ("sess-3", "not json at all\nnor is this".to_owned()),
        ],
    );
    let scan = scan_grok_cli(&dir).expect("scan");
    assert_eq!(scan.files, 3);
    assert_eq!(scan.malformed, 3, "one torn line and two junk ones");
    assert_eq!(scan.sessions.len(), 2, "the readable turns survive");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_siblings_of_the_usage_log_are_not_read_as_a_second_copy_of_the_turn() {
    // A Grok session directory holds three `*.jsonl` files. Only
    // `updates.jsonl` carries token counts today — but a walk that took
    // every `*.jsonl` would bill each turn twice the day one of the others
    // started carrying them too.
    let turn = grok_turn(
        "sess-1",
        "p1",
        unix("2026-01-01T00:00:00Z"),
        &[("grok-4.6-build", [10_000, 1_000, 0, 0, 0], 400)],
    );
    let dir = grok_fixture_dir("siblings", &[("sess-1", turn.clone())]);
    std::fs::write(
        dir.join("%2Fsome%2Fproject")
            .join("sess-1")
            .join("events.jsonl"),
        turn,
    )
    .expect("fixture sibling");
    let scan = scan_grok_cli(&dir).expect("scan");
    assert_eq!(scan.files, 1);
    assert_eq!(scan.sessions["sess-1"].messages.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_cli_s_own_cost_estimate_is_reported_beside_ours_and_never_booked_as_money() {
    // `costUsdTicks` has no documented scale, so booking it would be
    // picking one on aesthetics. It stays a ratio against our own figure,
    // which is a constant for a given model whatever a tick turns out to be
    // — so a ratio that drifts says our catalog price is stale.
    let body = grok_turn(
        "sess-1",
        "p1",
        unix("2026-01-01T00:00:00Z"),
        &[("grok-4.6-build", [1_000_000, 0, 0, 0, 0], 3_400_000_000)],
    );
    let dir = grok_fixture_dir("ticks", &[("sess-1", body)]);
    let plan = grok_plan(&dir, &LedgerIndex::default());
    let row = &plan.rows[0];
    // 1M tokens at $2/Mtok. The ticks are an order of magnitude away from
    // that number in every reading, and none of them reached the ledger.
    assert_eq!(row.listed, Some(Decimal::from(2)));
    assert_eq!(row.booked(None), (Decimal::from(2), Decimal::from(2)));
    assert_eq!(
        plan.cross_check(),
        Some((3_400_000_000, Decimal::from(2), 1))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_same_grok_turn_always_derives_the_same_row_identity() {
    // The idempotency key for this source. A turn is identified by the
    // prompt it answered and the model that answered it — both of which the
    // file states, neither of which the importer invents — so a re-run
    // re-derives the same `source_ref` and loses to the unique index.
    let body = grok_turn(
        "sess-1",
        "p1",
        unix("2026-01-01T00:00:00Z"),
        &[("grok-4.6-build", [10_000, 1_000, 0, 0, 0], 400)],
    );
    let dir = grok_fixture_dir("stable", &[("sess-1", body)]);
    let first = grok_plan(&dir, &LedgerIndex::default());
    let second = grok_plan(&dir, &LedgerIndex::default());
    assert_eq!(
        first.rows[0].source_ref,
        "grok-cli:sess-1:p1:grok-4.6-build"
    );
    assert_eq!(first.rows[0].request_id, second.rows[0].request_id);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_grok_import_is_reverted_without_touching_the_claude_code_one() {
    // Two origins rather than one shared `imported`, because the two
    // sources are not equally well defended: an operator who decides the
    // Grok figures are unsafe must be able to drop them and keep the Claude
    // Code history, which is defended by an exact per-call ledger match.
    assert_ne!(Source::ClaudeCode.origin(), Source::GrokCli.origin());
    assert_eq!(Source::GrokCli.origin(), ORIGIN_GROK_CLI);
}

/// The Grok import against a real Postgres.
///
/// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it. Idempotence
/// is enforced by a unique index, and an index is not a thing that can be
/// tested without the database that holds it.
#[tokio::test]
async fn a_second_grok_run_writes_nothing_and_a_dry_run_writes_nothing_at_all() {
    let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
        eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
        return;
    };
    let db = Db::connect(&url, 2).expect("connect");
    db.migrate().await.expect("migrate");

    let session = format!("s-{}", Uuid::new_v4());
    let body = [
        grok_turn(
            &session,
            "p1",
            unix("2026-01-01T00:00:00Z"),
            &[("grok-4.6-build", [10_000, 1_000, 0, 0, 0], 400)],
        ),
        grok_turn(
            &session,
            "p2",
            unix("2026-01-01T00:10:00Z"),
            &[("grok-4.6-build", [20_000, 2_000, 0, 0, 0], 800)],
        ),
    ]
    .join("\n");
    let dir = grok_fixture_dir("apply", &[(&session, body)]);
    let path = dir.to_string_lossy().into_owned();

    let count = |db: Db, session: String| async move {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_event WHERE source_ref LIKE $1")
            .bind(format!("{ORIGIN_GROK_CLI}:{session}:%"))
            .fetch_one(db.pool())
            .await
            .expect("count")
    };

    import(&db, Source::GrokCli, Some(&path), None, None, false)
        .await
        .expect("dry run");
    assert_eq!(count(db.clone(), session.clone()).await, 0);

    import(&db, Source::GrokCli, Some(&path), None, None, true)
        .await
        .expect("apply");
    assert_eq!(count(db.clone(), session.clone()).await, 2);

    import(&db, Source::GrokCli, Some(&path), None, None, true)
        .await
        .expect("re-apply");
    assert_eq!(
        count(db.clone(), session.clone()).await,
        2,
        "a re-run must not append a second copy of the same money"
    );

    sqlx::query("DELETE FROM usage_event WHERE source_ref LIKE $1")
        .bind(format!("{ORIGIN_GROK_CLI}:{session}:%"))
        .execute(db.pool())
        .await
        .expect("cleanup");
    let _ = std::fs::remove_dir_all(&dir);
}
