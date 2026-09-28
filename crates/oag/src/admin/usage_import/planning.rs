//! Pricing what survives judging and planning the rows to write.

use super::verdict::{LedgerIndex, Skip, Verdict, judge};
use super::{IMPORT_NAMESPACE, Scan, Source};
use oag_core::credential::CredentialKind;
use oag_core::{Error, Result};
use oag_router::{Pricing, Usage};
use oag_store::Db;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use time::OffsetDateTime;
use uuid::Uuid;

// ── pricing ──────────────────────────────────────────────────────────────────

/// The catalog, indexed by every name a transcript might use for a model.
///
/// A transcript writes the provider's own spelling (`claude-opus-5`), which is
/// `model_catalog.upstream_name`, while the ledger stores the canonical
/// `provider/name` id. Both spellings, and the tail of the id, are accepted so
/// that a catalog seeded from either direction resolves.
#[derive(Debug, Default)]
pub(super) struct Prices {
    /// `None` marks a spelling that two different catalogue rows both answer
    /// to — see the note in `index`. Absent and ambiguous are different
    /// answers, and both mean "cannot price this".
    by_name: HashMap<String, Option<(String, Pricing)>>,
}

impl Prices {
    pub(super) fn index(rows: &[oag_store::rows::ModelRow], provider: &str) -> Self {
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
                // A key two different models both answer to prices neither.
                //
                // `insert` returned the displaced entry and it was thrown away,
                // so the last row in catalogue order won a coin toss the caller
                // could not see — and a transcript slug that matched it was
                // priced against the wrong model, silently, on every row of the
                // import. The tail-of-the-id key is the one that collides:
                // `index` filters the catalogue to a single provider two lines
                // above, so the collision is never between providers — it is
                // one row's `upstream_name` against another's id-tail *within*
                // one, which is the pair the test fixtures.
                //
                // Marked rather than dropped, because the *unambiguous* keys of
                // both models still work: only the spelling that cannot
                // identify one model is refused, and the import reports it as
                // unpriced, which it already knows how to do.
                match by_name.entry(key.to_owned()) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(Some(entry.clone()));
                    }
                    std::collections::hash_map::Entry::Occupied(mut slot) => {
                        let collides = slot.get().as_ref().is_some_and(|(id, _)| *id != row.id);
                        if collides {
                            slot.insert(None);
                        }
                    }
                }
            }
        }
        Self { by_name }
    }

    pub(super) fn get(&self, slug: &str) -> Option<&(String, Pricing)> {
        // `None` is a key two models answered to. The caller treats an
        // unpriceable model exactly as it treats an unknown one — it counts it
        // and reports it — which is the honest outcome for a slug that does not
        // identify a model.
        self.by_name.get(slug).and_then(Option::as_ref)
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
pub(super) struct Seat {
    pub(super) id: Uuid,
    pub(super) name: String,
    pub(super) provider: String,
    pub(super) kind: CredentialKind,
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
    pub(super) fn flat_rate(&self) -> bool {
        self.kind.flat_rate()
    }

    /// How the report names it in a sentence.
    pub(super) fn describe(&self) -> String {
        format!("{} {}", self.provider, self.kind.channel_label())
    }
}

/// Find the credential the operator named.
///
/// `account.name` carries no unique constraint, so two rows may answer to one
/// name. That is a state the schema permits and this command cannot resolve:
/// attributing a month of financial history to whichever row happened to sort
/// first is a wrong answer that looks exactly like a right one.
pub(super) async fn account_by_name(db: &Db, name: &str) -> Result<Seat> {
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
pub(super) struct Pending {
    pub(super) request_id: Uuid,
    pub(super) source_ref: String,
    pub(super) occurred_at: OffsetDateTime,
    pub(super) model_id: String,
    pub(super) usage: Usage,
    /// What these tokens list for at the model's own API price. `None` when the
    /// catalog has no such model — deliberately not `Some(ZERO)`, because a
    /// model nobody priced is an unanswered question, and the ledger has a
    /// `cost_known` column so that it does not have to be filed as a gift.
    ///
    /// Not the same thing as what the row cost: see [`Pending::booked`].
    pub(super) listed: Option<Decimal>,
    /// The CLI's own estimate for this row, carried through for the report's
    /// cross-check and never written to the ledger. See [`Plan::cross_check`].
    pub(super) vendor_ticks: Option<u64>,
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
    pub(super) fn booked(&self, seat: Option<&Seat>) -> (Decimal, Decimal) {
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
    pub(super) rows: Vec<Pending>,
    pub(super) skipped: Vec<(String, Skip)>,
    /// Slugs the catalog could not price, and how many messages each cost us.
    pub(super) unpriced: BTreeMap<String, usize>,
    /// The credential these rows are attributed to, if the operator named one.
    /// Not part of a row's identity: re-running with a different `--account`
    /// derives the same `source_ref`, loses to the unique index and changes
    /// nothing, so re-attributing is a `revert` and an import, not a re-run.
    pub(super) seat: Option<Seat>,
    /// Which CLI these rows were read from. Part of every `source_ref` and of
    /// the `origin` column, so it decides what a later `revert` can undo.
    pub(super) source: Source,
    pub(super) scan: Scan,
}

impl Plan {
    pub(super) fn tokens(&self) -> Usage {
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
    pub(super) fn listed(&self) -> Decimal {
        self.rows.iter().filter_map(|r| r.listed).sum()
    }

    pub(super) fn skipped_as_foreign(&self) -> usize {
        self.skipped
            .iter()
            .filter(|(_, s)| matches!(s, Skip::ForeignModel { .. }))
            .count()
    }

    pub(super) fn skipped_as_proxied(&self) -> usize {
        self.skipped
            .iter()
            .filter(|(_, s)| matches!(s, Skip::AlreadyInLedger { .. }))
            .count()
    }

    pub(super) fn skipped_as_overlapping(&self) -> usize {
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
    pub(super) fn cross_check(&self) -> Option<(u64, Decimal, usize)> {
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
pub(super) fn plan(
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
                    // The session is part of the identity only where the
                    // message id cannot stand alone — see
                    // `Source::message_ids_are_global`, which is where the
                    // reasoning for each source lives.
                    //
                    // Nothing is lost by dropping it: `origin` still says which
                    // CLI the rows came from, and that is what `revert` matches
                    // on. It never matched on this shape.
                    // Changing this shape changes the identity every re-import
                    // recognises, so a change here needs a migration to carry
                    // the rows already written across — see
                    // `migrations/0017_rekey_claude_code_imports.sql`, which
                    // exists because this one shipped without it.
                    let source_ref = if source.message_ids_are_global() {
                        format!("{origin}:{}", message.external_id)
                    } else {
                        format!("{origin}:{session_id}:{}", message.external_id)
                    };
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
