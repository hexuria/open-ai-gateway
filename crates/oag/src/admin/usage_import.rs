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
//! does not know it. See [`Seat`](planning::Seat).
//!
//! Two CLIs are read, and they are not equally safe to read. Claude Code writes
//! one transcript entry per API response, so a session can be matched against
//! the ledger call by call, and a transcript naming a non-Anthropic model is
//! proof the session was proxied. The Grok CLI writes one aggregate per user
//! turn covering every model call the turn made, which no per-call ledger row
//! can ever equal, and it asks for a Grok model whether it is pointed at x.ai
//! or at this gateway — so neither of those defences exists for it. What is
//! left is `--before` and a deliberately blunt overlap test. See [`Source`],
//! which holds every place the two differ, and [`judge`](verdict::judge), which decides.
//!
//! Codex is still not supported: it records no token counts of its own.

use crate::admin::{ORIGIN_CLAUDE_CODE, ORIGIN_GROK_CLI};
use claude_code::scan_claude_code;
use grok_cli::{GROK_AGENT_SUFFIX, scan_grok_cli};
use oag_core::{Error, Result};
use oag_router::{Pricing, Usage};
use planning::Prices;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use uuid::Uuid;

pub use apply::{import, revert};

mod apply;
mod claude_code;
mod grok_cli;
mod planning;
mod reporting;
mod verdict;

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
    /// Whether this source's message ids identify a call on their own.
    ///
    /// Decides whether the session is part of a row's identity, and the two
    /// sources genuinely differ.
    ///
    /// Claude Code's is the provider's own message id — `msg_01...`, unique
    /// across every session and every machine — so including the session in the
    /// key adds nothing and costs correctness. That is finding C11: a line with
    /// no `sessionId` falls back to the *filename*, so the same message in a
    /// resumed session's continuation file derived a second `source_ref`,
    /// cleared the unique index that exists to stop exactly this, and booked
    /// one API call's tokens twice.
    ///
    /// Grok CLI's is `prompt_id`, and whether that is unique outside its own
    /// session is not something this repository can demonstrate. Dropping the
    /// session there would merge two genuinely different turns and *lose*
    /// money, which is the same defect with the sign reversed and the worse of
    /// the two. So it keeps the session until someone can show otherwise —
    /// a fixture with one `prompt_id` in two Grok sessions would settle it.
    const fn message_ids_are_global(self) -> bool {
        match self {
            Self::ClaudeCode => true,
            Self::GrokCli => false,
        }
    }

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
    ///
    /// [`Plan::cross_check`]: planning::Plan::cross_check
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

#[cfg(test)]
mod tests;
