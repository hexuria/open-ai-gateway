//! What an import tells the operator: bookings, protections and cross-checks.

use super::planning::{Plan, Seat};
use super::verdict::Skip;
use super::{Scan, Session, Source};
use rust_decimal::Decimal;
use time::OffsetDateTime;

/// The instant range every scanned message falls in.
pub(super) fn span(scan: &Scan) -> Option<(OffsetDateTime, OffsetDateTime)> {
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

pub(super) fn report(plan: &Plan) {
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
