//! Writing a plan to the ledger in batches, and reverting an import.

use super::planning::{Plan, Prices, account_by_name, plan};
use super::reporting::{report, span};
use super::verdict::LedgerIndex;
use super::{IMPORTED_LABEL, LEDGER_SLACK, Source, clamp};
use oag_core::{Error, Result};
use oag_store::{Db, repo};
use std::path::PathBuf;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

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
pub(super) async fn write(db: &Db, plan: &Plan) -> Result<u64> {
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
