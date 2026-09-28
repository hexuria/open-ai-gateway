//! Reading Claude Code's session logs into sessions of messages.

use super::{Message, Scan};
use oag_core::{Error, Result};
use oag_router::Usage;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Walk `root` for `*.jsonl` and read every one.
///
/// A file that cannot be opened is counted and stepped over rather than
/// aborting: an operator importing thirty projects' worth of history should not
/// lose the run to one file with the wrong permissions.
pub(super) fn scan_claude_code(root: &Path) -> Result<Scan> {
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
pub(super) fn jsonl_files(root: &Path) -> Result<Vec<PathBuf>> {
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
