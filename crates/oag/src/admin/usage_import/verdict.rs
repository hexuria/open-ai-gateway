//! Judging each message: already in the ledger, foreign, or to import.

use super::{Fingerprint, LEDGER_SLACK, Session, Source};
use std::collections::HashMap;
use time::OffsetDateTime;

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
pub(super) struct LedgerIndex {
    at: HashMap<Fingerprint, Vec<OffsetDateTime>>,
    /// When this gateway served the source's own provider, sorted.
    served: Vec<OffsetDateTime>,
}

impl LedgerIndex {
    pub(super) fn build(rows: Vec<(OffsetDateTime, i64, i64, i64, i64)>) -> Self {
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

    pub(super) fn activity(mut served: Vec<OffsetDateTime>) -> Self {
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
pub(super) enum Skip {
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
pub(super) enum Verdict {
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
///
/// [`DISTINCTIVE_TOKENS`]: super::DISTINCTIVE_TOKENS
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
pub(super) fn is_native_model(slug: &str) -> bool {
    let name = slug.rsplit('/').next().unwrap_or(slug);
    name.starts_with("claude")
}

pub(super) fn judge(
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
