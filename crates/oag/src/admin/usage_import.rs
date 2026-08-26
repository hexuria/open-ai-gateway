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
//! The second difficulty is whose money it was. A transcript names no account,
//! no email and no organisation, so nothing on disk says which credential paid
//! for the tokens — and the answer changes the arithmetic completely. Usage that
//! ran on a subscription has already been paid for by the monthly fee, so its
//! marginal cost is zero and the list price is only the bill the fee displaced;
//! booking that list price as spend invents a bill nobody was ever sent and
//! inflates the one figure this product exists to state honestly. So attribution
//! is told to the importer (`--account`) rather than inferred from a file that
//! does not know it. See [`Seat`].
//!
//! Two CLIs are read, and they are not equally safe to read. Claude Code writes
//! one transcript entry per API response, so a session can be matched against
//! the ledger call by call, and a transcript naming a non-Anthropic model is
//! proof the session was proxied. The Grok CLI writes one aggregate per user
//! turn covering every model call the turn made, which no per-call ledger row
//! can ever equal, and it asks for a Grok model whether it is pointed at x.ai
//! or at this gateway — so neither of those defences exists for it. What is
//! left is `--before` and a deliberately blunt overlap test. See [`Source`],
//! which holds every place the two differ, and [`judge`], which decides.
//!
//! Codex is still not supported: it records no token counts of its own.

use crate::admin::{ORIGIN_CLAUDE_CODE, ORIGIN_GROK_CLI};
use oag_core::credential::CredentialKind;
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

// ── which CLI ────────────────────────────────────────────────────────────────

/// The CLI whose records an import is reading.
///
/// Every place the two sources differ is a method here rather than a branch at
/// the call site, and every one of them is a total `match`. The differences are
/// not cosmetic — one of them decides which double-count defences exist at all —
/// so a third source must be forced to answer each question rather than
/// inheriting whichever answer happened to be the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    #[default]
    ClaudeCode,
    GrokCli,
}

impl Source {
    /// The `origin` its rows carry, which is also what `revert` deletes by.
    const fn origin(self) -> &'static str {
        match self {
            Self::ClaudeCode => ORIGIN_CLAUDE_CODE,
            Self::GrokCli => ORIGIN_GROK_CLI,
        }
    }

    /// The catalog provider its models are priced from, and the prefix an
    /// unpriced row's model id is given.
    const fn provider(self) -> &'static str {
        match self {
            Self::ClaudeCode => "anthropic",
            Self::GrokCli => "xai",
        }
    }

    /// What its files are called in a sentence.
    const fn records(self) -> &'static str {
        match self {
            Self::ClaudeCode => "transcripts",
            Self::GrokCli => "session logs",
        }
    }

    fn default_root(self) -> Result<PathBuf> {
        let home = std::env::var("HOME").map_err(|_| {
            Error::Config(format!(
                "HOME is unset; pass --path to say where the {} are",
                self.records()
            ))
        })?;
        let home = PathBuf::from(home);
        Ok(match self {
            Self::ClaudeCode => home.join(".claude").join("projects"),
            Self::GrokCli => home.join(".grok").join("sessions"),
        })
    }

    fn scan(self, root: &Path) -> Result<Scan> {
        match self {
            Self::ClaudeCode => scan_claude_code(root),
            Self::GrokCli => scan_grok_cli(root),
        }
    }

    /// Whether a model this source's own provider does not serve is proof the
    /// session went through a gateway.
    ///
    /// True for Claude Code: talking to Anthropic, it can only be answered by
    /// an Anthropic model, so `grok-4.5` in a transcript means something
    /// rewrote the request. False for the Grok CLI, and not because the test is
    /// unwritten — the Grok CLI pointed at this gateway asks for a Grok model
    /// and gets one, so the name is identical either way and carries no
    /// information. Nothing else in its files does either: no base URL, no
    /// host, no upstream request id.
    const fn foreign_model_proves_proxying(self) -> bool {
        match self {
            Self::ClaudeCode => true,
            Self::GrokCli => false,
        }
    }

    /// Look one reported model slug up in the catalog.
    ///
    /// The Grok CLI books usage against `grok-4.6-build` while every other file
    /// it writes — `summary.json`, `signals.json`, `models_cache.json` — calls
    /// the same model `grok-4.6`, which is the spelling x.ai's own listing
    /// seeds the catalog with. Trying the stripped name second is what lets a
    /// seeded catalog price the traffic at all. The suffix is observed rather
    /// than documented, so it is a fallback and not the primary key: getting it
    /// wrong costs nothing worse than the model landing under `unpriced`, named
    /// in the report, with its rows imported at no cost rather than a wrong one.
    fn price<'p>(self, prices: &'p Prices, slug: &str) -> Option<&'p (String, Pricing)> {
        prices.get(slug).or_else(|| match self {
            Self::ClaudeCode => None,
            Self::GrokCli => slug
                .strip_suffix(GROK_AGENT_SUFFIX)
                .and_then(|base| prices.get(base)),
        })
    }
}

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
    /// What the CLI itself thought this call cost, in whatever unit it counts
    /// in. `None` for a source that reports no such figure — Claude Code — and
    /// for a Grok record whose own usage the CLI marked incomplete.
    ///
    /// Never money. See [`Plan::cross_check`] for what it is for and why it is
    /// not booked.
    vendor_ticks: Option<u64>,
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
    /// Records the CLI itself flagged as having usage it could not fully
    /// gather (`usageIsIncomplete`, Grok only). Imported anyway — the tokens
    /// were spent, and a partial count under-reports, which is the direction to
    /// err in — but counted so the report can say the figure has a known floor
    /// under it rather than a known value.
    incomplete: usize,
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
        // Claude Code publishes no cost estimate of its own, only tokens.
        vendor_ticks: None,
    };
    scan.sessions
        .entry(session)
        .or_default()
        .messages
        .insert(message.external_id.clone(), message);
}

// ── the Grok CLI ─────────────────────────────────────────────────────────────

/// The one file in a Grok session directory that carries token counts.
///
/// Its siblings — `events.jsonl`, `chat_history.jsonl` — record the same
/// conversation with no usage in it at all, so reading them could only cost
/// time now and, if a future version started copying usage into one of them,
/// bill every turn twice.
const GROK_USAGE_LOG: &str = "updates.jsonl";

/// The only `sessionUpdate` a Grok usage record has ever carried.
const GROK_TURN_COMPLETED: &str = "turn_completed";

/// The suffix the Grok CLI adds to a model name when it reports usage against
/// it, and nowhere else. See [`Source::price`].
const GROK_AGENT_SUFFIX: &str = "-build";

/// What a Grok row's model is called when the record does not name one.
///
/// A real value written into the ledger rather than a guess at which model ran:
/// it resolves to nothing in the catalog, so the row imports with no cost, gets
/// named in the report's `unpriced` list, and is visibly a question rather than
/// invisibly attributed to whichever model was most likely.
const GROK_UNNAMED_MODEL: &str = "unnamed";

/// Walk `root` for Grok session logs and read every one.
///
/// Same shape as [`scan_claude_code`] — a file that will not open is counted
/// and stepped over — but narrowed to [`GROK_USAGE_LOG`], because a Grok
/// session directory holds three `*.jsonl` files and only one of them is about
/// money.
fn scan_grok_cli(root: &Path) -> Result<Scan> {
    let mut scan = Scan::default();
    // An explicit `--path` to a single file is taken at its word; a directory
    // walk is not, since it turns up the siblings too.
    let named_directly = root.is_file();
    for path in jsonl_files(root)? {
        if !named_directly && path.file_name().and_then(|n| n.to_str()) != Some(GROK_USAGE_LOG) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            scan.malformed += 1;
            continue;
        };
        scan.files += 1;
        // The directory is named for the session, the file inside it never is.
        // Only a fallback either way: every record carries the id.
        let fallback = path
            .parent()
            .and_then(|dir| dir.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_owned();
        for line in text.lines() {
            absorb_grok_line(&mut scan, &fallback, line);
        }
    }
    Ok(scan)
}

/// Fold one Grok session-log line into the scan.
///
/// **These records are per turn, not cumulative, and the evidence is that they
/// fall.** A cumulative counter cannot go down; across 601 records on this
/// machine the per-record `totalTokens` rises and drops freely (5,774,520 then
/// 1,491,613 in consecutive records of one session), every record carries a
/// distinct `prompt_id` — 601 records, 601 distinct `(sessionId, prompt_id)`
/// pairs, no repeats — and the record count per session matches the
/// `turnCount` its own `signals.json` reports. So summing the records in a file
/// gives the session total, and taking only the last would throw away almost
/// all of it.
///
/// The guard against the other reading being wrong is not this comment but the
/// key: a record is stored under `prompt_id` (per model), so a log that
/// replayed or duplicated a turn overwrites rather than accumulates. If a
/// future version did switch to a running total under a new `sessionUpdate`,
/// the discriminator test below ignores it rather than adding it to the turns
/// it is a running total of.
fn absorb_grok_line(scan: &mut Scan, fallback_session: &str, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        scan.malformed += 1;
        return;
    };
    let params = &v["params"];
    let update = &params["update"];
    // Most of the file is prompts, tool calls and stream deltas; a line that is
    // not a completed turn is the ordinary case, not an error.
    if update["sessionUpdate"].as_str() != Some(GROK_TURN_COMPLETED) {
        return;
    }
    let usage = &update["usage"];
    if !usage.is_object() {
        return;
    }

    let (Some(prompt), Some(occurred_at)) = (
        update["prompt_id"].as_str().filter(|s| !s.is_empty()),
        grok_instant(&v["timestamp"]),
    ) else {
        scan.unusable += 1;
        return;
    };
    if usage["usageIsIncomplete"].as_bool() == Some(true) {
        scan.incomplete += 1;
    }

    let session = params["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_session)
        .to_owned();
    let into = scan.sessions.entry(session).or_default();

    // Per model where the record splits it out, which it always has. The
    // sub-objects sum to the turn's own totals exactly in every record on this
    // machine, so preferring them costs nothing and buys a row the catalog can
    // price rather than one lump attributed to nothing.
    match usage["modelUsage"].as_object().filter(|m| !m.is_empty()) {
        Some(models) => {
            for (slug, per_model) in models {
                push_grok_turn(into, prompt, occurred_at, slug, per_model);
            }
        }
        None => push_grok_turn(into, prompt, occurred_at, GROK_UNNAMED_MODEL, usage),
    }
}

/// Store one turn's usage on one model.
///
/// Keyed by `prompt_id` and model together: a turn that used two models is two
/// rows, and the same turn read twice is the same two rows overwritten rather
/// than four rows summed.
fn push_grok_turn(
    session: &mut Session,
    prompt: &str,
    occurred_at: OffsetDateTime,
    slug: &str,
    usage: &serde_json::Value,
) {
    let message = Message {
        external_id: format!("{prompt}:{slug}"),
        occurred_at,
        model_slug: slug.to_owned(),
        usage: grok_usage(usage),
        vendor_ticks: usage["costUsdTicks"].as_u64(),
    };
    session
        .messages
        .insert(message.external_id.clone(), message);
}

/// Map one Grok usage object onto the ledger's four token columns.
///
/// Two of the five counts Grok reports are subsets of the others, and adding
/// them would bill the same tokens twice.
///
/// `cachedReadTokens` is part of `inputTokens`, so the uncached input this
/// ledger stores is the difference. That is measured, not assumed: pricing all
/// 601 records on this machine both ways against x.ai's published rate ratios
/// (input : cached read : output = 2 : 0.5 : 6) makes the subset reading land
/// on exactly one of two constant ticks-per-dollar figures for 463 of them —
/// the two are a context-length tier, one twice the other — while the disjoint
/// reading lands on noise with no repeated value at all. `cachedReadTokens` is
/// also never greater than `inputTokens` in any record, and `totalTokens` is
/// `inputTokens + outputTokens` in every one, with no room for it on the side.
///
/// `reasoningTokens` is likewise part of `outputTokens` — it never exceeds it —
/// and so is not a column here at all.
fn grok_usage(u: &serde_json::Value) -> Usage {
    let count = |key: &str| u[key].as_u64().unwrap_or(0);
    let cache_read = count("cachedReadTokens");
    Usage {
        // Saturating rather than trusting the arithmetic in the file: a
        // negative uncached count is not a thing, and a record disagreeing with
        // itself should cost a few tokens of under-reporting rather than take
        // the whole import down.
        input_tokens: count("inputTokens").saturating_sub(cache_read),
        output_tokens: count("outputTokens"),
        cache_read_tokens: cache_read,
        cache_write_tokens: count("cacheCreationTokens"),
    }
}

/// The instant a Grok record carries, in either spelling it might use.
///
/// Every record observed writes Unix seconds as a number. The string branch is
/// not speculation for its own sake: the `summary.json` sitting beside these
/// files writes RFC 3339, so both spellings already coexist in this CLI's own
/// output, and a version that switched would otherwise make every session
/// unusable at once with nothing in the report explaining why.
fn grok_instant(v: &serde_json::Value) -> Option<OffsetDateTime> {
    if let Some(seconds) = v.as_i64() {
        return OffsetDateTime::from_unix_timestamp(seconds).ok();
    }
    v.as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
}

// ── the decision ─────────────────────────────────────────────────────────────

/// Gateway-served rows, arranged for the question the importer can ask of them.
///
/// Which question that is depends on the source, and only one of the two fields
/// is ever populated. Claude Code gets the strong one — "did this exact token
/// shape occur near this time?", so a hash on the shape with a sorted list of
/// instants under it. The Grok CLI cannot ask it at all: its records are turn
/// aggregates over many model calls and the ledger's are per call, so the two
/// sides count different things and no fingerprint could match even for a
/// session that certainly was proxied. All it gets is a sorted list of when
/// this gateway served its provider.
#[derive(Debug, Default)]
struct LedgerIndex {
    at: HashMap<Fingerprint, Vec<OffsetDateTime>>,
    /// When this gateway served the source's own provider, sorted.
    served: Vec<OffsetDateTime>,
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
        Self {
            at,
            served: Vec::new(),
        }
    }

    fn activity(mut served: Vec<OffsetDateTime>) -> Self {
        served.sort_unstable();
        Self {
            at: HashMap::new(),
            served,
        }
    }

    fn seen_between(&self, fp: Fingerprint, from: OffsetDateTime, to: OffsetDateTime) -> bool {
        self.at.get(&fp).is_some_and(|times| {
            let start = times.partition_point(|t| *t < from);
            times.get(start).is_some_and(|t| *t <= to)
        })
    }

    /// How many gateway rows for this provider fall inside the window.
    fn served_between(&self, from: OffsetDateTime, to: OffsetDateTime) -> usize {
        let start = self.served.partition_point(|t| *t < from);
        let end = self.served.partition_point(|t| *t <= to);
        end - start
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
    /// This gateway was serving the source's own provider while the session
    /// ran. Not evidence that it served *this* session — only that it could
    /// have. The blunt instrument a source whose records cannot be
    /// fingerprinted is left with.
    GatewayActive { rows: usize },
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
    source: Source,
    before: Option<OffsetDateTime>,
) -> Option<Verdict> {
    let (start, end) = session.window()?;
    if before.is_some_and(|cutoff| end >= cutoff) {
        return Some(Verdict::Skipped(Skip::AfterCutoff));
    }
    // Checked before the fingerprints because it is the stronger statement: a
    // fingerprint says this ledger has the session, a foreign model says no
    // direct session could have produced it at all.
    if source.foreign_model_proves_proxying()
        && let Some(model) = foreign_model(session)
    {
        return Some(Verdict::Skipped(Skip::ForeignModel {
            model: model.to_owned(),
        }));
    }
    let (from, to) = (start - LEDGER_SLACK, end + LEDGER_SLACK);
    match source {
        Source::ClaudeCode => {
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
        }
        // Deliberately blunt, because nothing sharper is available: a turn
        // aggregate cannot be matched against per-call rows, so the only
        // remaining question is whether this gateway was serving x.ai at all
        // while the session ran. On a deployment that serves that provider all
        // day this skips every Grok session, which is exactly the trade the
        // rest of this module makes — a month left out is a question someone
        // can answer, a month counted twice is a wrong answer nobody asks
        // about. Every skip is named in the report, so the operator can see
        // what it cost them and reach for `--before` instead.
        Source::GrokCli => {
            let rows = ledger.served_between(from, to);
            if rows > 0 {
                return Some(Verdict::Skipped(Skip::GatewayActive { rows }));
            }
        }
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

// ── attribution ──────────────────────────────────────────────────────────────

/// The credential an import is attributed to.
///
/// Resolved once, before anything is planned, because it decides how every row
/// in the run is booked rather than anything about which rows there are. A
/// transcript cannot supply it: Claude Code records `userType` and nothing else
/// about who is paying, so the only honest source is the operator saying so.
#[derive(Debug, Clone)]
struct Seat {
    id: Uuid,
    name: String,
    provider: String,
    kind: CredentialKind,
}

impl Seat {
    /// Whether this credential is paid for by a flat fee rather than per token.
    ///
    /// The gateway's own test, reached through the same function
    /// (`CredentialKind::flat_rate`), so an imported row and a served one on the
    /// same seat agree on what the tokens cost. An unrecognised kind reads as
    /// metered for the same reason it does in the gateway: recording a real cost
    /// that turns out to be zero is a correction, recording a zero that turns
    /// out to be real is a hole.
    fn flat_rate(&self) -> bool {
        self.kind.flat_rate()
    }

    /// How the report names it in a sentence.
    fn describe(&self) -> String {
        format!("{} {}", self.provider, self.kind.channel_label())
    }
}

/// Find the credential the operator named.
///
/// `account.name` carries no unique constraint, so two rows may answer to one
/// name. That is a state the schema permits and this command cannot resolve:
/// attributing a month of financial history to whichever row happened to sort
/// first is a wrong answer that looks exactly like a right one.
async fn account_by_name(db: &Db, name: &str) -> Result<Seat> {
    let rows: Vec<(Uuid, String, String, String)> =
        sqlx::query_as("SELECT id, name, provider, kind FROM account WHERE name = $1")
            .bind(name)
            .fetch_all(db.pool())
            .await
            .map_err(|e| Error::Internal(format!("looking up credential: {e}")))?;

    match rows.as_slice() {
        [] => Err(Error::Config(format!(
            "no credential named {name}; see `oag admin account list`"
        ))),
        [(id, name, provider, kind)] => Ok(Seat {
            id: *id,
            name: name.clone(),
            provider: provider.clone(),
            // An unknown discriminator is not an error here — the row exists and
            // its usage is real. It falls through to metered, which is the
            // conservative reading.
            kind: CredentialKind::from_column(kind).unwrap_or(CredentialKind::ApiKey),
        }),
        many => Err(Error::Config(format!(
            "{} credentials are named {name}; rename one, or the import cannot say \
             which subscription paid",
            many.len()
        ))),
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
    /// What these tokens list for at the model's own API price. `None` when the
    /// catalog has no such model — deliberately not `Some(ZERO)`, because a
    /// model nobody priced is an unanswered question, and the ledger has a
    /// `cost_known` column so that it does not have to be filed as a gift.
    ///
    /// Not the same thing as what the row cost: see [`Pending::booked`].
    listed: Option<Decimal>,
    /// The CLI's own estimate for this row, carried through for the report's
    /// cross-check and never written to the ledger. See [`Plan::cross_check`].
    vendor_ticks: Option<u64>,
}

impl Pending {
    /// What this row books: `(cost_usd, counterfactual_api_usd)`.
    ///
    /// Real money spent, and the pay-per-token bill these tokens stand for. On a
    /// metered credential — and on an unattributed import, which is assumed
    /// metered — the two are one number: the list price is what was actually
    /// billed, nothing was avoided, and `SUM(counterfactual - cost)` correctly
    /// reads zero. On a flat-rate seat they diverge, because the monthly fee has
    /// already bought these tokens: the marginal cost of one more message is
    /// zero and the list price is only the bill that fee displaced. Charging
    /// both would count the same work twice in two different currencies.
    ///
    /// The divergent shape — `cost_usd = 0 AND counterfactual_api_usd > 0` — is
    /// the predicate that means "subscription seat" everywhere else in this
    /// tree, and matching it is the point rather than an accident: an attributed
    /// import belongs on its seat's line and out of the metered headline, which
    /// is exactly how the gateway books a seat it serves itself.
    fn booked(&self, seat: Option<&Seat>) -> (Decimal, Decimal) {
        let listed = self.listed.unwrap_or(Decimal::ZERO);
        if seat.is_some_and(Seat::flat_rate) {
            (Decimal::ZERO, listed)
        } else {
            (listed, listed)
        }
    }
}

/// Everything an import would do, decided before anything is written.
#[derive(Debug, Default)]
pub struct Plan {
    rows: Vec<Pending>,
    skipped: Vec<(String, Skip)>,
    /// Slugs the catalog could not price, and how many messages each cost us.
    unpriced: BTreeMap<String, usize>,
    /// The credential these rows are attributed to, if the operator named one.
    /// Not part of a row's identity: re-running with a different `--account`
    /// derives the same `source_ref`, loses to the unique index and changes
    /// nothing, so re-attributing is a `revert` and an import, not a re-run.
    seat: Option<Seat>,
    /// Which CLI these rows were read from. Part of every `source_ref` and of
    /// the `origin` column, so it decides what a later `revert` can undo.
    source: Source,
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

    /// The list value of everything that would be imported. Whether that is a
    /// bill or a bill avoided depends on the seat: see [`Pending::booked`].
    fn listed(&self) -> Decimal {
        self.rows.iter().filter_map(|r| r.listed).sum()
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

    fn skipped_as_overlapping(&self) -> usize {
        self.skipped
            .iter()
            .filter(|(_, s)| matches!(s, Skip::GatewayActive { .. }))
            .count()
    }

    /// The CLI's own cost estimate and ours, over exactly the rows carrying
    /// both: `(ticks, our dollars, rows)`.
    ///
    /// Reported, never booked. The Grok CLI counts in `costUsdTicks`, and
    /// nothing on disk says how large a tick is: against x.ai's published rate
    /// ratios the figure lands on exactly 1.7e9 or 3.4e9 ticks per dollar, and
    /// both a nano-dollar tick with rates 1.7x the published ones and a
    /// 1.7e9-per-dollar tick with the published ones fit that identically.
    /// Booking either as money would be picking one on aesthetics, and a wrong
    /// scale is a wrong ledger to the same number of decimal places as a right
    /// one.
    ///
    /// It is worth printing anyway, as a ratio. For a given model the ratio is
    /// a constant whatever the tick turns out to be, so one that moves between
    /// imports says our catalog price and x.ai's have diverged — and the
    /// catalog is the side we can fix. It moves for an honest reason too: Grok
    /// charges double above roughly 200k tokens of context and the catalog
    /// holds one price per model, so a long-context month reads high.
    ///
    /// The two sums are taken over the same rows deliberately. Summing ticks
    /// over every row and dollars over only the priced ones would make the
    /// ratio a statement about how much of the catalog is seeded.
    fn cross_check(&self) -> Option<(u64, Decimal, usize)> {
        let mut ticks = 0u64;
        let mut listed = Decimal::ZERO;
        let mut rows = 0usize;
        for row in &self.rows {
            if let (Some(t), Some(l)) = (row.vendor_ticks, row.listed) {
                ticks = ticks.saturating_add(t);
                listed += l;
                rows += 1;
            }
        }
        (rows > 0).then_some((ticks, listed, rows))
    }
}

/// Turn a scan into a plan. Pure: no database, no clock, no filesystem.
fn plan(
    scan: Scan,
    ledger: &LedgerIndex,
    prices: &Prices,
    source: Source,
    before: Option<OffsetDateTime>,
) -> Plan {
    let mut out = Plan {
        source,
        ..Plan::default()
    };
    let origin = source.origin();
    for (session_id, session) in &scan.sessions {
        match judge(session, ledger, source, before) {
            // A session with nothing in it: no window, so nothing to decide.
            None => {}
            Some(Verdict::Skipped(reason)) => {
                out.skipped.push((session_id.clone(), reason));
            }
            Some(Verdict::Import) => {
                for message in session.messages.values() {
                    let source_ref = format!("{origin}:{session_id}:{}", message.external_id);
                    let listed = source.price(prices, &message.model_slug);
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
                            || format!("{}/{}", source.provider(), message.model_slug),
                            |(id, _)| id.clone(),
                        ),
                        usage: message.usage,
                        listed: listed.map(|(_, p)| p.cost(&message.usage)),
                        vendor_ticks: message.vendor_ticks,
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
    let seat = plan.seat.as_ref();
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
        let mut api = Vec::with_capacity(batch.len());
        let mut refs = Vec::with_capacity(batch.len());
        let mut known = Vec::with_capacity(batch.len());
        for row in batch {
            let (booked, listed) = row.booked(seat);
            ids.push(row.request_id);
            at.push(row.occurred_at);
            models.push(row.model_id.clone());
            input.push(clamp(row.usage.input_tokens));
            output.push(clamp(row.usage.output_tokens));
            cache_read.push(clamp(row.usage.cache_read_tokens));
            cache_write.push(clamp(row.usage.cache_write_tokens));
            cost.push(booked);
            api.push(listed);
            refs.push(row.source_ref.clone());
            // Still "was the list price known", which on a seat row is the
            // question that carries the money: its cost of zero is exact, and
            // the figure that can be a guess is the bill it displaced.
            known.push(row.listed.is_some());
        }

        let result = sqlx::query(
            r"
            INSERT INTO usage_event (
                request_id, attempt, occurred_at, account_id, model_id, tier,
                selection_reason,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                cost_usd, counterfactual_usd, counterfactual_api_usd,
                status, streamed, origin, source_ref, cost_known
            )
            -- What it cost and what it lists for are one number on an imported
            -- metered row: nothing was routed, so nothing was avoided, and the
            -- row contributes its real spend to the headline and exactly zero
            -- to SUM(counterfactual - cost). They are two numbers on a row
            -- attributed to a subscription, whose monthly fee already bought
            -- these tokens -- and that shape, `cost_usd = 0 AND
            -- counterfactual_api_usd > 0`, is the predicate meaning
            -- 'subscription seat' everywhere else in this tree. Matching it is
            -- deliberate: the row then lands on its seat's line and out of the
            -- metered headline, which is the same treatment a seat the gateway
            -- serves itself gets. account_id is what carries it there; without
            -- it the predicate would be true of a row belonging to no seat.
            SELECT r.id, 0, r.at, $14, r.model, $12, $12,
                   r.input, r.output, r.cache_read, r.cache_write,
                   r.cost, r.api, r.api,
                   200, false, $13, r.source_ref, r.cost_known
            FROM unnest(
                     $1::uuid[], $2::timestamptz[], $3::text[],
                     $4::bigint[], $5::bigint[], $6::bigint[], $7::bigint[],
                     $8::numeric[], $9::numeric[], $10::text[], $11::bool[]
                 ) AS r(id, at, model, input, output, cache_read, cache_write,
                        cost, api, source_ref, cost_known)
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
        .bind(&api)
        .bind(&refs)
        .bind(&known)
        .bind(IMPORTED_LABEL)
        .bind(plan.source.origin())
        .bind(seat.map(|s| s.id))
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
    source: Source,
    path: Option<&str>,
    before: Option<&str>,
    account: Option<&str>,
    apply: bool,
) -> Result<Plan> {
    let before = before
        .map(|s| {
            OffsetDateTime::parse(s, &Rfc3339)
                .map_err(|e| Error::Config(format!("--before is not an RFC 3339 instant: {e}")))
        })
        .transpose()?;

    // Resolved before a single file is read: a misspelled credential name should
    // cost a second, not a scan of a year of transcripts followed by a refusal.
    let seat = match account {
        Some(name) => Some(account_by_name(db, name).await?),
        None => None,
    };

    let root = path.map_or_else(|| source.default_root(), |p| Ok(PathBuf::from(p)))?;
    println!("source       {}  {}", source.origin(), root.display());
    let scan = source.scan(&root)?;

    // The ledger is read only over the span the records actually cover. Widened
    // by the same slack the per-session windows use, so a row sitting just
    // outside a session's edge is still available to match it.
    let ledger = match span(&scan) {
        Some((from, to)) => {
            let (from, to) = (from - LEDGER_SLACK, to + LEDGER_SLACK);
            match source {
                Source::ClaudeCode => {
                    LedgerIndex::build(repo::gateway_fingerprints(db, from, to).await?)
                }
                Source::GrokCli => LedgerIndex::activity(
                    repo::gateway_activity(db, source.provider(), from, to).await?,
                ),
            }
        }
        None => LedgerIndex::default(),
    };
    let prices = Prices::index(&repo::catalog(db).await?, source.provider());

    let mut plan = plan(scan, &ledger, &prices, source, before);
    plan.seat = seat;
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

/// Delete everything one importer wrote, optionally only what it wrote against
/// one credential.
///
/// The reason `origin` exists as a column rather than as a convention: an
/// import that turns out to have double counted has to be removable without
/// touching a single row the gateway earned, and without an operator writing
/// DELETE against the ledger by hand at the moment they are least calm. The
/// account filter is the same argument one level down — attributing an import
/// to the wrong subscription is the mistake `--account` newly makes possible,
/// and undoing it must not take the other subscriptions' history with it.
pub async fn revert(db: &Db, origin: &str, account: Option<&str>, apply: bool) -> Result<()> {
    if origin == "gateway" {
        return Err(Error::Config(
            "refusing to delete gateway-served rows; this command only removes imports".to_owned(),
        ));
    }
    let seat = match account {
        Some(name) => Some(account_by_name(db, name).await?),
        None => None,
    };
    let id = seat.as_ref().map(|s| s.id);
    let scope = seat
        .as_ref()
        .map_or_else(String::new, |s| format!(" attributed to {}", s.name));

    // One predicate, written once, so the count an operator reads and the
    // delete they then authorise cannot describe different rows. A macro rather
    // than a `format!` because sqlx accepts only a literal — which is the same
    // reason writing it once is worth the macro: the alternative is the clause
    // typed out twice. A NULL account matches the whole origin rather than the
    // rows that have no account, which is what omitting the flag means.
    macro_rules! scoped {
        ($head:literal) => {
            concat!(
                $head,
                " FROM usage_event WHERE origin = $1 AND ($2::uuid IS NULL OR account_id = $2)"
            )
        };
    }
    let n: i64 = sqlx::query_scalar(scoped!("SELECT COUNT(*)"))
        .bind(origin)
        .bind(id)
        .fetch_one(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("counting imported usage: {e}")))?;
    if !apply {
        println!("would delete {n} rows with origin '{origin}'{scope}");
        println!("dry run: nothing was written. re-run with --apply to delete them.");
        return Ok(());
    }
    let deleted = sqlx::query(scoped!("DELETE"))
        .bind(origin)
        .bind(id)
        .execute(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("deleting imported usage: {e}")))?;
    println!(
        "deleted {} rows with origin '{origin}'{scope}",
        deleted.rows_affected()
    );
    Ok(())
}

/// The instant range every scanned message falls in.
fn span(scan: &Scan) -> Option<(OffsetDateTime, OffsetDateTime)> {
    scan.sessions
        .values()
        .filter_map(Session::window)
        .reduce(|(lo, hi), (a, b)| (lo.min(a), hi.max(b)))
}

/// The CLI's own cost estimate beside ours, as a ratio and never as money.
///
/// See [`Plan::cross_check`] for why the ratio is the useful part and why the
/// absolute figure is not one this importer is entitled to book. Silent for a
/// source that publishes no estimate, which is the whole of Claude Code.
fn report_cross_check(plan: &Plan) {
    let Some((ticks, ours, rows)) = plan.cross_check() else {
        return;
    };
    println!("cross-check  {ticks} ticks, the CLI's own estimate over {rows} messages");
    if ours > Decimal::ZERO {
        println!(
            "             {:.0} ticks per catalog dollar",
            Decimal::from(ticks) / ours
        );
    }
    println!("             not booked: nothing on disk says how large a tick is.");
    println!("             the ratio is the check — one that moves between imports");
    println!("             means our catalog price and the vendor's have diverged");
}

/// How these rows land in the books, said out loud in every case.
///
/// Priced at list is the right answer for a metered key and the wrong one for a
/// subscription, and nothing in a session record can tell them apart — so an
/// operator running this blind is told which they just chose.
fn report_booking(seat: Option<&Seat>) {
    match seat {
        Some(seat) if seat.flat_rate() => {
            println!("account      {} ({})", seat.name, seat.describe());
            println!("             booked at $0: the monthly fee already bought these");
            println!("             tokens, and the list price above is the bill it displaced");
        }
        Some(seat) => {
            println!("account      {} ({})", seat.name, seat.describe());
            println!("             booked at list price as real spend, which is what a");
            println!("             metered credential is actually billed");
        }
        None => {
            println!("account      none: booked at list price as real spend");
            println!("             if this ran on a subscription, that is a bill nobody was");
            println!("             sent. re-run with --account <name> to attribute it, and");
            println!("             see `oag admin account list` for the names");
        }
    }
}

/// Which double-count defences this run actually applied.
///
/// Printed rather than left in the docs, and printed for both sources rather
/// than only for the weak one: an operator can only tell that a Grok figure
/// needs spot-checking if they can see what the other source gets that it does
/// not. The gaps named here are gaps in the evidence on disk, not in the code,
/// and a run that quietly omitted them would let a number be trusted for
/// reasons that do not hold.
fn report_protections(source: Source) {
    let provider = source.provider();
    match source {
        Source::ClaudeCode => {
            println!("protection   ledger match, exact and per call: a session with one");
            println!("             distinctive token shape already metered here is skipped");
            println!("             foreign model: a non-Anthropic model in the transcript");
            println!("             proves the session was proxied, wherever its rows landed");
            println!("             --before: exact, and infers nothing");
        }
        Source::GrokCli => {
            println!("protection   weaker here, and the gaps are not oversights:");
            println!("             - foreign model: no signal at all. This CLI asks the");
            println!("               gateway for a Grok model and gets one, so the name is");
            println!("               identical whether it was proxied or not");
            println!("             - ledger match: none possible. Grok logs one aggregate");
            println!("               per turn and the ledger one row per call, so no two");
            println!("               token counts can ever line up. In its place a session");
            println!("               is skipped whenever this gateway served {provider} at all");
            println!("               while it ran, which over-skips rather than double-counts");
            println!("             - --before: exact, infers nothing, and is the only");
            println!("               protection this source really has. Set it to when you");
            println!("               pointed this CLI at the gateway");
        }
    }
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
    if s.incomplete > 0 {
        println!(
            "             {} records the CLI could not fully account for, imported \
             as the floor they are",
            s.incomplete
        );
    }
    let proxied = plan.skipped_as_proxied();
    let foreign = plan.skipped_as_foreign();
    let overlapping = plan.skipped_as_overlapping();
    let cutoff = plan.skipped.len() - proxied - foreign - overlapping;
    println!("sessions     {} seen", s.sessions.len());
    println!("             {proxied} skipped: already in the ledger");
    if foreign > 0 {
        println!("             {foreign} skipped: went through a gateway (foreign model)");
    }
    if overlapping > 0 {
        println!(
            "             {overlapping} skipped: this gateway was serving {} at the time",
            plan.source.provider()
        );
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
    let priced = plan.rows.len() - unpriced_rows;
    // The same number means two different things, so it is labelled by which.
    // "cost $34,372" against a subscription is a bill nobody was sent.
    match &plan.seat {
        Some(seat) if seat.flat_rate() => println!(
            "displaced    ${:.4} over {priced} priced messages",
            plan.listed()
        ),
        _ => println!(
            "cost         ${:.4} over {priced} priced messages",
            plan.listed()
        ),
    }
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

    report_cross_check(plan);
    report_booking(plan.seat.as_ref());
    report_protections(plan.source);

    let provider = plan.source.provider();
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
            Skip::GatewayActive { rows } => {
                println!("skip {id}  {rows} gateway {provider} rows fall in its window");
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
            .bind(format!("claude-code:{session}:%"))
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
            let model = format!("anthropic/claude-opus-5-{}", Uuid::new_v4());
            sqlx::query(
                "INSERT INTO model_catalog (id, provider, upstream_name, input_per_mtok, \
                 output_per_mtok, cache_read_per_mtok, cache_write_per_mtok, context_window, \
                 max_output_tokens) VALUES ($1, 'anthropic', 'claude-opus-5', 15, 75, 1, 18, \
                 200000, 64000)",
            )
            .bind(&model)
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

            let session = format!("s-{}", Uuid::new_v4());
            let body = line(
                &session,
                "msg_a",
                "2026-01-01T00:00:00Z",
                "claude-opus-5",
                [10_000, 2_000, 0, 0],
            );
            let dir = fixture_dir(name, &[("a.jsonl", body)]);
            Some(Self {
                db,
                sub,
                key,
                source_ref: format!("claude-code:{session}:msg_a"),
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
    async fn an_import_attributed_to_a_seat_is_booked_as_that_seats_row_and_left_out_of_the_headline()
     {
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
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM usage_event WHERE source_ref LIKE $1",
            )
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
}
