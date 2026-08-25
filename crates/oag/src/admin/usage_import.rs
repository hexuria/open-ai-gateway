//! `oag admin usage import` — folding a CLI's own session transcripts into the
//! ledger.
//!
//! Traffic that never touched the gateway is invisible to it. Run Claude Code
//! against Anthropic directly and the tokens are spent, the money is gone, and
//! the ledger says nothing happened — which makes every figure built on the
//! ledger a statement about a subset nobody named. The transcripts on disk
//! record exactly what the ledger stores, so the gap is closable.
//!
//! The whole difficulty is the other direction. A session pointed *at* the
//! gateway writes a transcript entry **and** produces a `usage_event`. Import
//! that naively and every figure inflates, including `SUM(counterfactual -
//! cost)` — the one number this product exists to state honestly, and the one
//! whose inflation looks exactly like success. So the importer's job is less
//! "read the files" than "decide which sessions the ledger already knows", and
//! the bias throughout is towards skipping: an under-reported month is a
//! question someone can answer later, a double-counted one is a wrong answer
//! nobody thinks to ask about.
//!
//! See [`judge`] for the decision and what it gets wrong in each direction.

use crate::admin::ORIGIN_CLAUDE_CODE;
use oag_core::{Error, Result};
use oag_router::{Pricing, Usage};
use oag_store::{Db, repo};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Namespace for the `UUIDv5` an imported row is keyed by.
///
/// Fixed forever: change it and every previously imported row loses its
/// identity, so the next import writes all of them again. Its only requirement
/// is that it is not a namespace anything else derives ids in.
const IMPORT_NAMESPACE: Uuid = Uuid::from_u128(0x6f61_6725_7573_6167_655f_696d_706f_7274);

/// A ledger row reduced to the only thing a transcript can be compared against.
///
/// Four exact token counts, in the ledger's own column order. Both sides derive
/// them from the same upstream usage object, so a proxied call's transcript
/// entry and its ledger row agree digit for digit or the mapping is broken.
type Fingerprint = (i64, i64, i64, i64);

/// Below this, a fingerprint is not evidence of anything.
///
/// Agentic turns run tens of thousands of cache-read tokens and collide by
/// accident about as often as two random large integers do. Tiny ones do not:
/// `(2, 1, 0, 0)` is a shape many unrelated calls land on, and treating one of
/// those as proof would let a single coincidence delete a whole session from
/// the import. The floor costs nothing in recall — a real session has hundreds
/// of turns and essentially all of them clear it.
const DISTINCTIVE_TOKENS: u64 = 1_000;

/// How far a ledger row's `occurred_at` may sit from the same call's transcript
/// timestamp.
///
/// The two clocks measure different moments: the transcript stamps the message
/// as the client writes it, the ledger stamps it as metering completes after
/// the response finishes. A long streamed answer separates them by minutes, and
/// the two machines' clocks need not agree either. Generous rather than tight,
/// because widening it can only cause a skip and narrowing it can cause a
/// double count.
const LEDGER_SLACK: time::Duration = time::Duration::minutes(10);

/// What an imported row records where the gateway would record a routing
/// decision. Not one of `reason_label`'s values, deliberately: this row is not
/// the outcome of a routing decision, and borrowing "passthrough" would make it
/// indistinguishable from one in the by-tier report.
const IMPORTED_LABEL: &str = "imported";

// ── the shape of a transcript ────────────────────────────────────────────────

/// One billable call, as a transcript records it.
#[derive(Debug, Clone)]
struct Message {
    /// Stable within the session. One API response is written as several
    /// transcript lines — one per content block — each with its own `uuid` but
    /// all carrying the same `message.id` and a byte-identical usage object.
    /// Keying on `message.id` is what stops the importer billing a reply with
    /// four content blocks four times.
    external_id: String,
    occurred_at: OffsetDateTime,
    /// The provider's own spelling, e.g. `claude-opus-5`. Resolved against the
    /// catalog later; kept raw here so an unpriced row can still say what ran.
    model_slug: String,
    usage: Usage,
}

impl Message {
    fn fingerprint(&self) -> Fingerprint {
        let u = &self.usage;
        (
            clamp(u.input_tokens),
            clamp(u.output_tokens),
            clamp(u.cache_read_tokens),
            clamp(u.cache_write_tokens),
        )
    }

    fn distinctive(&self) -> bool {
        self.usage.total() >= DISTINCTIVE_TOKENS
    }
}

fn clamp(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Every call from one CLI session.
///
/// The unit of the whole feature. `ANTHROPIC_BASE_URL` is read once per
/// process, so a session is entirely proxied or entirely direct — there is no
/// such thing as half a session in the ledger, and deciding per message would
/// invent a state that cannot occur.
#[derive(Debug, Default)]
struct Session {
    /// Keyed by `external_id`, which both de-duplicates the multi-line replies
    /// and gives the report a deterministic order.
    messages: BTreeMap<String, Message>,
}

impl Session {
    fn window(&self) -> Option<(OffsetDateTime, OffsetDateTime)> {
        let mut it = self.messages.values().map(|m| m.occurred_at);
        let first = it.next()?;
        Some(it.fold((first, first), |(lo, hi), t| (lo.min(t), hi.max(t))))
    }
}

/// What one pass over the transcript directory found.
#[derive(Debug, Default)]
pub struct Scan {
    /// By session id, merged across files. A resumed session copies its
    /// predecessor's history into a new file, so the same call appears under
    /// two filenames; merging on the id rather than the path is what keeps it
    /// one call.
    sessions: BTreeMap<String, Session>,
    files: usize,
    /// Lines that were not JSON at all. A transcript is appended to live and
    /// can be truncated mid-write by a crash, so a torn last line is normal and
    /// must not take the run down with it.
    malformed: usize,
    /// Lines that were JSON, carried usage, and still could not be imported —
    /// no `message.id` to key on, or no readable timestamp. Counted apart from
    /// malformed because they mean something different: the file is fine and
    /// the importer's assumptions are not.
    unusable: usize,
}

// ── parsing ──────────────────────────────────────────────────────────────────

/// Walk `root` for `*.jsonl` and read every one.
///
/// A file that cannot be opened is counted and stepped over rather than
/// aborting: an operator importing thirty projects' worth of history should not
/// lose the run to one file with the wrong permissions.
fn scan_claude_code(root: &Path) -> Result<Scan> {
    let mut scan = Scan::default();
    for path in jsonl_files(root)? {
        let Ok(text) = std::fs::read_to_string(&path) else {
            scan.malformed += 1;
            continue;
        };
        scan.files += 1;
        // The filename stem is only a fallback. It usually equals the session
        // id and sometimes does not, because a resumed session writes its
        // forebear's entries into a file named after the new session.
        let fallback = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_owned();
        for line in text.lines() {
            absorb_claude_line(&mut scan, &fallback, line);
        }
    }
    Ok(scan)
}

/// Depth-first, iterative. No `walkdir`: one directory tree is not worth a
/// dependency, and the recursion depth here is a project layout, not a graph.
fn jsonl_files(root: &Path) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    if !root.is_dir() {
        return Err(Error::Config(format!(
            "no transcripts at {}; pass --path to say where they are",
            root.display()
        )));
    }
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Fold one transcript line into the scan.
///
/// Every rejection is silent-but-counted rather than fatal. The interesting
/// lines are a minority of the file — user turns, tool results and summaries
/// all live here too — so "this is not an assistant reply with usage" is the
/// ordinary case, not an error.
fn absorb_claude_line(scan: &mut Scan, fallback_session: &str, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        scan.malformed += 1;
        return;
    };
    if v["type"].as_str() != Some("assistant") {
        return;
    }
    // A synthetic entry stands for an error the client rendered, not a call the
    // provider billed; its usage is zeroes wearing a model name.
    if v["isApiErrorMessage"].as_bool() == Some(true) {
        return;
    }
    let msg = &v["message"];
    let usage = &msg["usage"];
    if !usage.is_object() {
        return;
    }
    let model = msg["model"].as_str().unwrap_or_default();
    if model.is_empty() || model == "<synthetic>" {
        return;
    }

    let (Some(id), Some(ts)) = (msg["id"].as_str(), v["timestamp"].as_str()) else {
        scan.unusable += 1;
        return;
    };
    let Ok(occurred_at) = OffsetDateTime::parse(ts, &Rfc3339) else {
        scan.unusable += 1;
        return;
    };

    let session = v["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_session)
        .to_owned();

    let message = Message {
        external_id: id.to_owned(),
        occurred_at,
        model_slug: model.to_owned(),
        usage: Usage {
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
            // The gateway's own Anthropic decoder maps these two the same way
            // (`oag_proto::anthropic`), which is what makes a fingerprint
            // comparable across the two paths at all.
            cache_read_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
            cache_write_tokens: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        },
    };
    scan.sessions
        .entry(session)
        .or_default()
        .messages
        .insert(message.external_id.clone(), message);
}

// ── the decision ─────────────────────────────────────────────────────────────

/// Gateway-served rows, arranged for the one question the importer asks.
///
/// "Did this exact token shape occur near this time?" — so a hash on the shape
/// and a sorted list of instants under it, rather than a scan per message.
#[derive(Debug, Default)]
struct LedgerIndex {
    at: HashMap<Fingerprint, Vec<OffsetDateTime>>,
}

impl LedgerIndex {
    fn build(rows: Vec<(OffsetDateTime, i64, i64, i64, i64)>) -> Self {
        let mut at: HashMap<Fingerprint, Vec<OffsetDateTime>> = HashMap::new();
        for (t, i, o, r, w) in rows {
            at.entry((i, o, r, w)).or_default().push(t);
        }
        for times in at.values_mut() {
            times.sort_unstable();
        }
        Self { at }
    }

    fn seen_between(&self, fp: Fingerprint, from: OffsetDateTime, to: OffsetDateTime) -> bool {
        self.at.get(&fp).is_some_and(|times| {
            let start = times.partition_point(|t| *t < from);
            times.get(start).is_some_and(|t| *t <= to)
        })
    }
}

/// Why a session is not being imported.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Skip {
    /// The ledger already has it: `matched` of the session's messages appear as
    /// gateway rows inside its own time window.
    AlreadyInLedger { matched: usize, of: usize },
    /// Excluded by `--before`.
    AfterCutoff,
    /// The transcript names a model this CLI's own provider does not serve, so
    /// the session cannot have talked to that provider directly.
    ForeignModel { model: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Import,
    Skipped(Skip),
}

/// Decide whether the ledger already contains this session.
///
/// The rule: take the session's own time window, widened by [`LEDGER_SLACK`],
/// and ask how many of its messages have a gateway row in there with the same
/// four token counts. One distinctive match condemns the whole session, because
/// the base URL is a per-process setting and a session cannot be half proxied.
///
/// **False positives** (a direct session skipped, so under-reported). Two
/// sessions running at once — one through the gateway, one not — where the
/// direct one happens to produce a token shape the proxied one also produced.
/// [`DISTINCTIVE_TOKENS`] removes the cheap version of this; what is left needs
/// two unrelated calls to agree on four large integers within ten minutes. The
/// cost is a session's worth of history missing, and the report names the
/// session and the reason, so it is visible and re-importable by hand.
///
/// **False negatives** (a proxied session imported, so double counted). This is
/// the direction that corrupts the savings figure, and there are three ways in:
/// the ledger being a different or a pruned database from the one that served
/// the session, so its rows are simply not there to match; the gateway having
/// dropped every metering write for that session, which it does silently by
/// design (`oag_usage_write_failures_total`); and the two sides disagreeing on
/// token counts, which would break every fingerprint at once rather than one.
/// Nothing in a transcript can detect any of them — the files record no base
/// URL, no endpoint and no upstream request id — so the defence is `--before`,
/// which an operator sets to the moment they started routing through the
/// gateway and which is correct by construction rather than by inference.
/// A model the CLI's own provider cannot serve, if the session names one.
///
/// Proof rather than inference, and the only such proof a transcript offers:
/// Claude Code talking to Anthropic can only be answered by an Anthropic model,
/// so a transcript naming `grok-4.5` went through something that rewrote the
/// request. Fingerprint matching can only recognise a session this ledger
/// already holds; this recognises one that went through a gateway whose rows
/// are somewhere else entirely — another deployment, or this one before its
/// database was reset — which is exactly the case that would otherwise be
/// imported twice into the same set of books.
///
/// Skipping is the conservative half of the trade: a session proxied through
/// somebody else's gateway is usage this ledger never saw and arguably should
/// import, and it will be left out. That under-reports, visibly and
/// recoverably, which is the direction to err in.
fn foreign_model(session: &Session) -> Option<&str> {
    session
        .messages
        .values()
        .map(|m| m.model_slug.as_str())
        .find(|slug| !is_native_model(slug))
}

/// Whether a slug is one the transcript's own provider could have returned.
///
/// Deliberately a prefix test on the vendor's own family name rather than a
/// catalog lookup: the catalog holds whatever has been seeded, so a model
/// missing from it would read as foreign and condemn an honest session.
fn is_native_model(slug: &str) -> bool {
    let name = slug.rsplit('/').next().unwrap_or(slug);
    name.starts_with("claude")
}

fn judge(
    session: &Session,
    ledger: &LedgerIndex,
    before: Option<OffsetDateTime>,
) -> Option<Verdict> {
    let (start, end) = session.window()?;
    if before.is_some_and(|cutoff| end >= cutoff) {
        return Some(Verdict::Skipped(Skip::AfterCutoff));
    }
    // Checked before the fingerprints because it is the stronger statement: a
    // fingerprint says this ledger has the session, a foreign model says no
    // direct session could have produced it at all.
    if let Some(model) = foreign_model(session) {
        return Some(Verdict::Skipped(Skip::ForeignModel {
            model: model.to_owned(),
        }));
    }
    let (from, to) = (start - LEDGER_SLACK, end + LEDGER_SLACK);
    let matched = session
        .messages
        .values()
        .filter(|m| m.distinctive() && ledger.seen_between(m.fingerprint(), from, to))
        .count();
    if matched > 0 {
        return Some(Verdict::Skipped(Skip::AlreadyInLedger {
            matched,
            of: session.messages.len(),
        }));
    }
    Some(Verdict::Import)
}

// ── pricing ──────────────────────────────────────────────────────────────────

/// The catalog, indexed by every name a transcript might use for a model.
///
/// A transcript writes the provider's own spelling (`claude-opus-5`), which is
/// `model_catalog.upstream_name`, while the ledger stores the canonical
/// `provider/name` id. Both spellings, and the tail of the id, are accepted so
/// that a catalog seeded from either direction resolves.
#[derive(Debug, Default)]
struct Prices {
    by_name: HashMap<String, (String, Pricing)>,
}

impl Prices {
    fn index(rows: &[oag_store::rows::ModelRow], provider: &str) -> Self {
        let mut by_name = HashMap::new();
        for row in rows.iter().filter(|r| r.provider == provider) {
            let entry = (
                row.id.clone(),
                Pricing {
                    input_per_mtok: row.input_per_mtok,
                    output_per_mtok: row.output_per_mtok,
                    cache_read_per_mtok: row.cache_read_per_mtok,
                    cache_write_per_mtok: row.cache_write_per_mtok,
                },
            );
            for key in [
                row.upstream_name.as_str(),
                row.id.as_str(),
                row.id.rsplit('/').next().unwrap_or(&row.id),
            ] {
                by_name.insert(key.to_owned(), entry.clone());
            }
        }
        Self { by_name }
    }

    fn get(&self, slug: &str) -> Option<&(String, Pricing)> {
        self.by_name.get(slug)
    }
}

// ── the plan ─────────────────────────────────────────────────────────────────

/// One row the importer would write.
#[derive(Debug, Clone)]
struct Pending {
    request_id: Uuid,
    source_ref: String,
    occurred_at: OffsetDateTime,
    model_id: String,
    usage: Usage,
    /// `None` when the catalog has no such model. Deliberately not `Some(ZERO)`:
    /// a model nobody priced is an unanswered question, and the ledger has a
    /// `cost_known` column so that it does not have to be filed as a gift.
    cost: Option<Decimal>,
}

/// Everything an import would do, decided before anything is written.
#[derive(Debug, Default)]
pub struct Plan {
    rows: Vec<Pending>,
    skipped: Vec<(String, Skip)>,
    /// Slugs the catalog could not price, and how many messages each cost us.
    unpriced: BTreeMap<String, usize>,
    scan: Scan,
}

impl Plan {
    fn tokens(&self) -> Usage {
        self.rows.iter().fold(Usage::default(), |mut acc, r| {
            acc.input_tokens += r.usage.input_tokens;
            acc.output_tokens += r.usage.output_tokens;
            acc.cache_read_tokens += r.usage.cache_read_tokens;
            acc.cache_write_tokens += r.usage.cache_write_tokens;
            acc
        })
    }

    fn cost(&self) -> Decimal {
        self.rows.iter().filter_map(|r| r.cost).sum()
    }

    fn skipped_as_foreign(&self) -> usize {
        self.skipped
            .iter()
            .filter(|(_, s)| matches!(s, Skip::ForeignModel { .. }))
            .count()
    }

    fn skipped_as_proxied(&self) -> usize {
        self.skipped
            .iter()
            .filter(|(_, s)| matches!(s, Skip::AlreadyInLedger { .. }))
            .count()
    }
}

/// Turn a scan into a plan. Pure: no database, no clock, no filesystem.
fn plan(scan: Scan, ledger: &LedgerIndex, prices: &Prices, before: Option<OffsetDateTime>) -> Plan {
    let mut out = Plan::default();
    for (session_id, session) in &scan.sessions {
        match judge(session, ledger, before) {
            // A session with nothing in it: no window, so nothing to decide.
            None => {}
            Some(Verdict::Skipped(reason)) => {
                out.skipped.push((session_id.clone(), reason));
            }
            Some(Verdict::Import) => {
                for message in session.messages.values() {
                    let source_ref =
                        format!("{ORIGIN_CLAUDE_CODE}:{session_id}:{}", message.external_id);
                    let listed = prices.get(&message.model_slug);
                    if listed.is_none() {
                        *out.unpriced.entry(message.model_slug.clone()).or_default() += 1;
                    }
                    out.rows.push(Pending {
                        request_id: Uuid::new_v5(&IMPORT_NAMESPACE, source_ref.as_bytes()),
                        source_ref,
                        occurred_at: message.occurred_at,
                        // An unpriced model still gets a canonical-looking id so
                        // the row says what ran; pricing it later is then a
                        // catalog edit, not an archaeology exercise.
                        model_id: listed.map_or_else(
                            || format!("anthropic/{}", message.model_slug),
                            |(id, _)| id.clone(),
                        ),
                        usage: message.usage,
                        cost: listed.map(|(_, p)| p.cost(&message.usage)),
                    });
                }
            }
        }
    }
    out.scan = scan;
    out
}

// ── writing ──────────────────────────────────────────────────────────────────

/// Rows per INSERT.
///
/// A month of agentic history is tens of thousands of messages, and a row per
/// round trip made a re-run cost two minutes of doing nothing — every one of
/// those rows is already present and is going to lose to the unique index, and
/// the wire time is paid before the database can say so. Batched, the same
/// re-run is a couple of seconds. Deliberately not one statement for the whole
/// import: the parameter arrays are held in memory twice over, and a failure
/// then discards the entire run rather than the last thousand rows of it.
const WRITE_BATCH: usize = 1_000;

/// Append the planned rows.
///
/// Untargeted `ON CONFLICT DO NOTHING`, for the same reason `record_usage` uses
/// it: both the primary key and the partial unique index on `source_ref` are
/// arbiters, and naming one would tie this statement to a schema that is still
/// being reshaped. A second run therefore inserts nothing and reports so, which
/// is the idempotency requirement met by the database rather than by a
/// pre-flight SELECT that a concurrent run could race.
///
/// No enclosing transaction. A partial import is not a corrupt one — every row
/// carries its own identity, so re-running finishes the job and re-inserts
/// nothing — whereas holding one transaction open across tens of thousands of
/// rows makes a failure at the end throw away work that was entirely correct.
async fn write(db: &Db, plan: &Plan) -> Result<u64> {
    let mut written = 0u64;
    for batch in plan.rows.chunks(WRITE_BATCH) {
        let mut ids = Vec::with_capacity(batch.len());
        let mut at = Vec::with_capacity(batch.len());
        let mut models = Vec::with_capacity(batch.len());
        let mut input = Vec::with_capacity(batch.len());
        let mut output = Vec::with_capacity(batch.len());
        let mut cache_read = Vec::with_capacity(batch.len());
        let mut cache_write = Vec::with_capacity(batch.len());
        let mut cost = Vec::with_capacity(batch.len());
        let mut refs = Vec::with_capacity(batch.len());
        let mut known = Vec::with_capacity(batch.len());
        for row in batch {
            ids.push(row.request_id);
            at.push(row.occurred_at);
            models.push(row.model_id.clone());
            input.push(clamp(row.usage.input_tokens));
            output.push(clamp(row.usage.output_tokens));
            cache_read.push(clamp(row.usage.cache_read_tokens));
            cache_write.push(clamp(row.usage.cache_write_tokens));
            cost.push(row.cost.unwrap_or(Decimal::ZERO));
            refs.push(row.source_ref.clone());
            known.push(row.cost.is_some());
        }

        let result = sqlx::query(
            r"
            INSERT INTO usage_event (
                request_id, attempt, occurred_at, model_id, tier, selection_reason,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                cost_usd, counterfactual_usd, counterfactual_api_usd,
                status, streamed, origin, source_ref, cost_known
            )
            -- cost, counterfactual and the API-equivalent are one value on an
            -- imported row. Nothing was routed, so nothing was avoided: this
            -- traffic contributes its real spend to the headline and exactly
            -- zero to SUM(counterfactual - cost), which is the truth about
            -- money spent outside the gateway. Keeping the API-equivalent equal
            -- to the cost is also what stops `cost_usd = 0 AND
            -- counterfactual_api_usd > 0` -- the predicate that means
            -- 'subscription seat' -- from ever matching one.
            SELECT r.id, 0, r.at, r.model, $11, $11,
                   r.input, r.output, r.cache_read, r.cache_write,
                   r.cost, r.cost, r.cost,
                   200, false, $12, r.source_ref, r.cost_known
            FROM unnest(
                     $1::uuid[], $2::timestamptz[], $3::text[],
                     $4::bigint[], $5::bigint[], $6::bigint[], $7::bigint[],
                     $8::numeric[], $9::text[], $10::bool[]
                 ) AS r(id, at, model, input, output, cache_read, cache_write,
                        cost, source_ref, cost_known)
            ON CONFLICT DO NOTHING
            ",
        )
        .bind(&ids)
        .bind(&at)
        .bind(&models)
        .bind(&input)
        .bind(&output)
        .bind(&cache_read)
        .bind(&cache_write)
        .bind(&cost)
        .bind(&refs)
        .bind(&known)
        .bind(IMPORTED_LABEL)
        .bind(ORIGIN_CLAUDE_CODE)
        .execute(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("importing usage: {e}")))?;
        written += result.rows_affected();
    }
    Ok(written)
}

// ── the command ──────────────────────────────────────────────────────────────

/// Run `oag admin usage import`.
pub async fn import(
    db: &Db,
    path: Option<&str>,
    before: Option<&str>,
    apply: bool,
) -> Result<Plan> {
    let before = before
        .map(|s| {
            OffsetDateTime::parse(s, &Rfc3339)
                .map_err(|e| Error::Config(format!("--before is not an RFC 3339 instant: {e}")))
        })
        .transpose()?;

    let root = path.map_or_else(default_transcript_root, |p| Ok(PathBuf::from(p)))?;
    println!("source       claude-code  {}", root.display());
    let scan = scan_claude_code(&root)?;

    // The ledger is read only over the span the transcripts actually cover.
    // Widened by the same slack the per-session windows use, so a row sitting
    // just outside a session's edge is still available to match it.
    let ledger = match span(&scan) {
        Some((from, to)) => LedgerIndex::build(
            repo::gateway_fingerprints(db, from - LEDGER_SLACK, to + LEDGER_SLACK).await?,
        ),
        None => LedgerIndex::default(),
    };
    let prices = Prices::index(&repo::catalog(db).await?, "anthropic");

    let plan = plan(scan, &ledger, &prices, before);
    report(&plan);

    if !apply {
        println!();
        println!("dry run: nothing was written. re-run with --apply to write these rows.");
        return Ok(plan);
    }
    let written = write(db, &plan).await?;
    println!();
    println!("imported     {written} rows");
    if written < plan.rows.len() as u64 {
        let already = plan.rows.len() as u64 - written;
        println!("             {already} were already imported and were left alone");
    }
    Ok(plan)
}

/// Delete everything one importer wrote.
///
/// The reason `origin` exists as a column rather than as a convention: an
/// import that turns out to have double counted has to be removable without
/// touching a single row the gateway earned, and without an operator writing
/// DELETE against the ledger by hand at the moment they are least calm.
pub async fn revert(db: &Db, origin: &str, apply: bool) -> Result<()> {
    if origin == "gateway" {
        return Err(Error::Config(
            "refusing to delete gateway-served rows; this command only removes imports".to_owned(),
        ));
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_event WHERE origin = $1")
        .bind(origin)
        .fetch_one(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("counting imported usage: {e}")))?;
    if !apply {
        println!("would delete {n} rows with origin '{origin}'");
        println!("dry run: nothing was written. re-run with --apply to delete them.");
        return Ok(());
    }
    let deleted = sqlx::query("DELETE FROM usage_event WHERE origin = $1")
        .bind(origin)
        .execute(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("deleting imported usage: {e}")))?;
    println!(
        "deleted {} rows with origin '{origin}'",
        deleted.rows_affected()
    );
    Ok(())
}

fn default_transcript_root() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".claude").join("projects"))
        .map_err(|_| {
            Error::Config("HOME is unset; pass --path to say where the transcripts are".to_owned())
        })
}

/// The instant range every scanned message falls in.
fn span(scan: &Scan) -> Option<(OffsetDateTime, OffsetDateTime)> {
    scan.sessions
        .values()
        .filter_map(Session::window)
        .reduce(|(lo, hi), (a, b)| (lo.min(a), hi.max(b)))
}

fn report(plan: &Plan) {
    let s = &plan.scan;
    println!("scanned      {} files", s.files);
    if s.malformed > 0 || s.unusable > 0 {
        println!(
            "             {} unreadable lines, {} with usage but no stable id",
            s.malformed, s.unusable
        );
    }
    let proxied = plan.skipped_as_proxied();
    let foreign = plan.skipped_as_foreign();
    let cutoff = plan.skipped.len() - proxied - foreign;
    println!("sessions     {} seen", s.sessions.len());
    println!("             {proxied} skipped: already in the ledger");
    if foreign > 0 {
        println!("             {foreign} skipped: went through a gateway (foreign model)");
    }
    if cutoff > 0 {
        println!("             {cutoff} skipped: not before the --before cutoff");
    }
    println!(
        "             {} to import",
        s.sessions.len() - plan.skipped.len()
    );

    let t = plan.tokens();
    println!("messages     {}", plan.rows.len());
    println!(
        "tokens       in {} out {} cache-read {} cache-write {}",
        t.input_tokens, t.output_tokens, t.cache_read_tokens, t.cache_write_tokens
    );
    let unpriced_rows: usize = plan.unpriced.values().sum();
    println!(
        "cost         ${:.4} over {} priced messages",
        plan.cost(),
        plan.rows.len() - unpriced_rows
    );
    if plan.unpriced.is_empty() {
        println!("unpriced     none");
    } else {
        // Named, not just counted. The fix is a catalog entry, and an operator
        // cannot add one for a model the report would not name.
        let models: Vec<String> = plan
            .unpriced
            .iter()
            .map(|(m, n)| format!("{m} ({n})"))
            .collect();
        println!("unpriced     {unpriced_rows} messages on models the catalog does not have:");
        println!("             {}", models.join(", "));
        println!("             imported with no cost, not a cost of zero");
    }

    // Every skip, named. A session silently missing from a financial import is
    // the failure mode this whole command is trying to avoid producing.
    for (id, reason) in &plan.skipped {
        match reason {
            Skip::AlreadyInLedger { matched, of } => {
                println!("skip {id}  {matched}/{of} messages match gateway rows in its window");
            }
            Skip::AfterCutoff => println!("skip {id}  ends at or after the --before cutoff"),
            Skip::ForeignModel { model } => {
                println!("skip {id}  names {model}, which its own provider does not serve");
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        let plan = plan(scan, &ledger, &catalog(), None);
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
        let plan = plan(scan, &LedgerIndex::build(vec![]), &catalog(), None);
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
        let plan = plan(scan, &ledger, &catalog(), None);
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
        let plan = plan(scan, &ledger, &catalog(), None);
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
        let plan = plan(scan, &LedgerIndex::default(), &catalog(), None);
        assert_eq!(plan.rows.len(), 2);
        assert_eq!(plan.unpriced.get("claude-opus-9-unseeded"), Some(&1));
        let unpriced = plan
            .rows
            .iter()
            .find(|r| r.model_id.ends_with("claude-opus-9-unseeded"))
            .expect("the unpriced row");
        assert_eq!(unpriced.cost, None, "no cost, not a zero cost");
        let priced = plan
            .rows
            .iter()
            .find(|r| r.model_id == "anthropic/claude-opus-5")
            .expect("the priced row");
        assert!(priced.cost.is_some_and(|c| c > Decimal::ZERO));
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
            Some(at("2026-02-01T00:00:00Z")),
        );
        assert_eq!(plan.rows.len(), 1);
        assert_eq!(plan.skipped, vec![("s2".to_owned(), Skip::AfterCutoff)]);
        let _ = std::fs::remove_dir_all(&dir);
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
            None,
        );
        let second = plan(
            scan_claude_code(&dir).expect("scan"),
            &LedgerIndex::default(),
            &catalog(),
            None,
        );
        assert_eq!(first.rows[0].source_ref, "claude-code:s1:msg_a");
        assert_eq!(first.rows[0].request_id, second.rows[0].request_id);
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

        let session = format!("s-{}", Uuid::new_v4());
        let body = [
            line(
                &session,
                "msg_a",
                "2026-01-01T00:00:00Z",
                "claude-opus-5",
                [10, 20, 5000, 0],
            ),
            line(
                &session,
                "msg_b",
                "2026-01-01T00:01:00Z",
                "claude-opus-5",
                [11, 21, 6000, 0],
            ),
        ]
        .join("\n");
        let dir = fixture_dir("apply", &[("a.jsonl", body)]);
        let path = dir.to_string_lossy().into_owned();

        let count = |db: Db, session: String| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM usage_event WHERE source_ref LIKE $1",
            )
            .bind(format!("claude-code:{session}:%"))
            .fetch_one(db.pool())
            .await
            .expect("count")
        };

        // The dry run is the default, and it must leave the ledger untouched.
        import(&db, Some(&path), None, false)
            .await
            .expect("dry run");
        assert_eq!(count(db.clone(), session.clone()).await, 0);

        import(&db, Some(&path), None, true).await.expect("apply");
        assert_eq!(count(db.clone(), session.clone()).await, 2);

        // The second run re-derives the same keys and loses to the index.
        import(&db, Some(&path), None, true)
            .await
            .expect("re-apply");
        assert_eq!(
            count(db.clone(), session.clone()).await,
            2,
            "a re-run must not append a second copy of the same money"
        );

        // And the marking is what makes the import removable on its own.
        sqlx::query("DELETE FROM usage_event WHERE source_ref LIKE $1")
            .bind(format!("claude-code:{session}:%"))
            .execute(db.pool())
            .await
            .expect("cleanup");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
