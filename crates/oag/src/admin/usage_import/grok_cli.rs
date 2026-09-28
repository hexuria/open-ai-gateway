//! Reading the Grok CLI's usage log into sessions of turns.

use super::claude_code::jsonl_files;
use super::{Message, Scan, Session};
use oag_core::Result;
use oag_router::Usage;
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The one file in a Grok session directory that carries token counts.
///
/// Its siblings — `events.jsonl`, `chat_history.jsonl` — record the same
/// conversation with no usage in it at all, so reading them could only cost
/// time now and, if a future version started copying usage into one of them,
/// bill every turn twice.
pub(super) const GROK_USAGE_LOG: &str = "updates.jsonl";

/// The only `sessionUpdate` a Grok usage record has ever carried.
pub(super) const GROK_TURN_COMPLETED: &str = "turn_completed";

/// The suffix the Grok CLI adds to a model name when it reports usage against
/// it, and nowhere else. See [`Source::price`](super::Source::price).
pub(super) const GROK_AGENT_SUFFIX: &str = "-build";

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
///
/// [`scan_claude_code`]: super::claude_code::scan_claude_code
pub(super) fn scan_grok_cli(root: &Path) -> Result<Scan> {
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
