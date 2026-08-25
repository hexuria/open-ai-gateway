//! Resolving a reporting period to the two instants a query can use.
//!
//! The summary endpoint began with one rolling `?days=N` look-back, which
//! cannot express the question an operator actually has — "did we spend more
//! this month than last month". A rolling 30 days is not a calendar month, and
//! two rolling windows are not comparable with each other at all. So the
//! grammar grew named calendar periods and an explicit range, and everything
//! here turns one of those into an unambiguous half-open `[start, end)` pair.
//!
//! ## Boundaries
//!
//! Half-open, always: `start` is inclusive, `end` exclusive. A row stamped
//! exactly `2026-09-01T00:00:00` belongs to September, and belongs to it once —
//! closed-closed bounds would count that row in both months and make two
//! adjacent periods sum to more than the year.
//!
//! ## Timezone
//!
//! Calendar boundaries are computed in a **fixed offset**, defaulting to UTC
//! and overridable per request with `?tz=<minutes east of UTC>` (the dashboard
//! sends the browser's). The ledger stores `timestamptz`, so the instant is
//! never in doubt; what a zone decides is only which side of midnight a request
//! falls on. An operator in UTC+8 reading UTC months sees eight hours of every
//! month attributed to the previous one, which is exactly the silent
//! misattribution the `tz` parameter exists to prevent.
//!
//! A fixed offset rather than an IANA zone is deliberate: naming zones needs a
//! bundled tz database, and the only thing that buys is a correct boundary for
//! a period that straddles a DST change — an hour, once or twice a year, on a
//! monthly total. The offset in effect when the request was made is used for
//! the whole window; that is the one approximation here, and it is stated
//! rather than hidden.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// The rolling look-back a caller gets when they ask for nothing. Unchanged
/// from before calendar periods existed, so an existing dashboard, script or
/// bookmark keeps seeing the same numbers.
const DEFAULT_DAYS: i32 = 30;

/// What a caller may write on the query string.
///
/// Four spellings of "which window", deliberately not merged into one: a named
/// calendar period, an explicit range, the legacy rolling look-back, and the
/// zone the first two are measured in.
#[derive(Debug, Default, Deserialize)]
pub struct Window {
    /// A named calendar period: `month`, `last-month`, `year`, `last-year`,
    /// `all`.
    pub period: Option<String>,
    /// Start of a custom range, `YYYY-MM-DD`, inclusive.
    pub from: Option<String>,
    /// End of a custom range, `YYYY-MM-DD`, **inclusive** — an operator asking
    /// for `from=2026-03-01&to=2026-03-15` means through the end of the 15th,
    /// not up to its midnight.
    pub to: Option<String>,
    /// Days to look back from now. The original rolling window, still exactly
    /// itself: clamped to 1..=3650, no upper bound on the range, and what an
    /// unqualified request resolves to.
    pub days: Option<i32>,
    /// Minutes east of UTC to compute calendar boundaries in. Defaults to 0.
    pub tz: Option<i32>,
}

/// A resolved window: two instants, and enough labelling that a caller cannot
/// misread which one they are looking at.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Inclusive lower bound. `None` only for `all`, which has no start — the
    /// ledger keeps full history and there is no purge, so "everything" really
    /// does mean everything.
    pub start: Option<OffsetDateTime>,
    /// Exclusive upper bound. `None` for a window that runs to now: leaving it
    /// unbounded rather than pinning it to `now` keeps a `days=N` query
    /// byte-for-byte the predicate it always was.
    pub end: Option<OffsetDateTime>,
    /// The instant this resolved at. The effective end of an open window, and
    /// the anchor every relative boundary was computed from, so the reported
    /// window and the query agree even if the clock moves mid-request.
    pub now: OffsetDateTime,
    /// The zone the calendar boundaries were computed in.
    pub offset: UtcOffset,
    /// Canonical name of the grammar that matched.
    pub period: &'static str,
    /// A label with no locale and no ambiguity in it.
    pub label: String,
}

/// The window, as the response reports it back.
///
/// Returned on every summary so a caller — and the dashboard — can state what
/// is on screen. Without it a page showing "$412" cannot say $412 of *what*,
/// and an operator comparing two screenshots has no way to know they were taken
/// over the same days.
#[derive(Debug, Serialize)]
pub struct WindowView {
    /// `month`, `last-month`, `year`, `last-year`, `all`, `custom`, `days`.
    pub period: &'static str,
    pub label: String,
    /// Inclusive start, RFC3339 in the reporting zone. `null` only for `all`.
    pub start: Option<String>,
    /// Exclusive end, RFC3339 in the reporting zone. Always present: a window
    /// that runs to now ends at the instant the query ran, and saying so is
    /// more use than a `null` the reader has to interpret.
    pub end: String,
    /// The zone the boundaries were computed in, echoed so a caller can tell a
    /// month boundary they disagree with from a total they disagree with.
    pub tz_offset_minutes: i32,
}

impl Resolved {
    /// The effective end: the explicit bound, or the moment this resolved.
    #[must_use]
    pub fn effective_end(&self) -> OffsetDateTime {
        self.end.unwrap_or(self.now)
    }

    #[must_use]
    pub fn view(&self) -> WindowView {
        WindowView {
            period: self.period,
            label: self.label.clone(),
            start: self.start.map(|s| rfc3339(s, self.offset)),
            end: rfc3339(self.effective_end(), self.offset),
            tz_offset_minutes: self.offset.whole_minutes().into(),
        }
    }

    /// How many calendar months of a flat monthly fee this window covers for a
    /// seat that has existed since `created_at`.
    ///
    /// The old code multiplied the monthly fee by `days / 30`, which charges
    /// 31/30 of a fee for a 31-day month and 28/30 for February — so the two
    /// months an operator is comparing are quoted at different prices for the
    /// same subscription. Here a *calendar* month is the unit, so any full
    /// calendar month is exactly `1`, a full year exactly `12`, and a partial
    /// month the fraction of that particular month's own length.
    ///
    /// Clamped to the seat's `created_at`, which also gives `all` a start it
    /// would otherwise not have. A seat OAG has known for a week is charged a
    /// week of fee, not a month of it — the alternative bills a window in which
    /// the gateway could not have routed a single request to that seat, and
    /// reports the resulting hole as a loss the subscription made.
    ///
    /// `None` when the arithmetic cannot be done at all (a `created_at` outside
    /// the representable calendar), which the caller renders as an unknown
    /// plan cost rather than a free one.
    #[must_use]
    pub fn seat_months(&self, created_at: OffsetDateTime) -> Option<Decimal> {
        let start = match self.start {
            Some(s) => s.max(created_at),
            None => created_at,
        };
        calendar_months(start, self.effective_end(), self.offset)
    }
}

/// Resolve a query string into one window, or the message a caller needs to fix
/// it.
///
/// Precedence, when a caller sends more than one: `period` beats `from`/`to`
/// beats `days`. Each is narrower than the next and `days` is what everyone
/// gets by default, so a stale `days=30` left in a bookmarked URL must not
/// quietly override the calendar month the operator just clicked. Nothing is
/// rejected for redundancy — a picker that appends a parameter rather than
/// rewriting the query string is a normal thing to build against.
pub fn resolve(window: &Window, now: OffsetDateTime) -> Result<Resolved, String> {
    let offset = offset_from(window.tz)?;
    let local = now.to_offset(offset);

    if let Some(period) = window.period.as_deref().map(str::trim)
        && !period.is_empty()
    {
        return named(period, local, now, offset);
    }

    match (window.from.as_deref(), window.to.as_deref()) {
        (Some(from), Some(to)) => custom(from, to, now, offset),
        // Half a range is far more likely a typo than an intent, and guessing
        // the other half would answer a question nobody asked.
        (Some(_), None) | (None, Some(_)) => {
            Err("a custom range needs both from and to, each as YYYY-MM-DD".to_owned())
        }
        (None, None) => Ok(rolling(window.days.unwrap_or(DEFAULT_DAYS), now, offset)),
    }
}

fn named(
    period: &str,
    local: OffsetDateTime,
    now: OffsetDateTime,
    offset: UtcOffset,
) -> Result<Resolved, String> {
    let (year, month) = (local.year(), local.month());
    match period {
        "month" => {
            let start = month_start(year, month, offset).ok_or_else(unrepresentable)?;
            Ok(Resolved {
                start: Some(start),
                end: None,
                now,
                offset,
                period: "month",
                label: format!("{} to date", year_month(year, month)),
            })
        }
        "last-month" => {
            let (py, pm) = previous_month(year, month);
            let start = month_start(py, pm, offset).ok_or_else(unrepresentable)?;
            let end = month_start(year, month, offset).ok_or_else(unrepresentable)?;
            Ok(Resolved {
                start: Some(start),
                end: Some(end),
                now,
                offset,
                period: "last-month",
                label: year_month(py, pm),
            })
        }
        "year" => {
            let start = year_start(year, offset).ok_or_else(unrepresentable)?;
            Ok(Resolved {
                start: Some(start),
                end: None,
                now,
                offset,
                period: "year",
                label: format!("{year} to date"),
            })
        }
        "last-year" => {
            let start = year_start(year - 1, offset).ok_or_else(unrepresentable)?;
            let end = year_start(year, offset).ok_or_else(unrepresentable)?;
            Ok(Resolved {
                start: Some(start),
                end: Some(end),
                now,
                offset,
                period: "last-year",
                label: format!("{}", year - 1),
            })
        }
        "all" => Ok(Resolved {
            start: None,
            end: None,
            now,
            offset,
            period: "all",
            label: "all time".to_owned(),
        }),
        other => Err(format!(
            "unknown period {other:?}: valid values are month, last-month, year, last-year, all \
             — or pass from=YYYY-MM-DD&to=YYYY-MM-DD for a custom range, or days=N for a rolling \
             look-back"
        )),
    }
}

fn custom(
    from: &str,
    to: &str,
    now: OffsetDateTime,
    offset: UtcOffset,
) -> Result<Resolved, String> {
    let from_date = parse_date(from)?;
    let to_date = parse_date(to)?;
    if from_date > to_date {
        return Err(format!(
            "from ({from_date}) is after to ({to_date}); a range runs forwards"
        ));
    }
    // `to` is inclusive, so the exclusive end is the midnight that opens the
    // following day. Anything else silently drops the last day's spend.
    let end_date = to_date.next_day().ok_or_else(unrepresentable)?;
    Ok(Resolved {
        start: Some(midnight(from_date, offset)),
        end: Some(midnight(end_date, offset)),
        now,
        offset,
        period: "custom",
        label: format!("{from_date} to {to_date}"),
    })
}

/// The original rolling look-back, preserved exactly: clamped to 1..=3650 days
/// back from now, with no upper bound.
fn rolling(days: i32, now: OffsetDateTime, offset: UtcOffset) -> Resolved {
    let days = days.clamp(1, 3650);
    Resolved {
        start: Some(now - Duration::days(days.into())),
        end: None,
        now,
        offset,
        period: "days",
        label: format!("last {days} days"),
    }
}

/// Real UTC offsets run from -12:00 to +14:00; anything outside that is a
/// caller sending seconds, or a sign error, and either way the month boundary
/// it would produce is not one anybody meant.
fn offset_from(tz: Option<i32>) -> Result<UtcOffset, String> {
    let minutes = tz.unwrap_or(0);
    if !(-720..=840).contains(&minutes) {
        return Err(format!(
            "tz must be minutes east of UTC, between -720 and 840; got {minutes}"
        ));
    }
    UtcOffset::from_whole_seconds(minutes * 60)
        .map_err(|_| format!("tz {minutes} is not a usable UTC offset"))
}

fn parse_date(s: &str) -> Result<Date, String> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    Date::parse(s.trim(), format).map_err(|_| format!("{s:?} is not a date of the form YYYY-MM-DD"))
}

fn unrepresentable() -> String {
    "that period falls outside the representable calendar".to_owned()
}

fn midnight(date: Date, offset: UtcOffset) -> OffsetDateTime {
    PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_offset(offset)
}

fn month_start(year: i32, month: Month, offset: UtcOffset) -> Option<OffsetDateTime> {
    Date::from_calendar_date(year, month, 1)
        .ok()
        .map(|d| midnight(d, offset))
}

fn year_start(year: i32, offset: UtcOffset) -> Option<OffsetDateTime> {
    month_start(year, Month::January, offset)
}

fn previous_month(year: i32, month: Month) -> (i32, Month) {
    match month {
        Month::January => (year - 1, Month::December),
        other => (year, other.previous()),
    }
}

fn next_month(year: i32, month: Month) -> (i32, Month) {
    match month {
        Month::December => (year + 1, Month::January),
        other => (year, other.next()),
    }
}

fn year_month(year: i32, month: Month) -> String {
    format!("{year:04}-{:02}", u8::from(month))
}

fn rfc3339(at: OffsetDateTime, offset: UtcOffset) -> String {
    at.to_offset(offset)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The length of `[start, end)` measured in calendar months.
///
/// Closed form rather than a walk, so no loop bound has to be invented for a
/// window that starts at a corrupt timestamp:
///
/// ```text
/// months = whole months between the two months' first days
///        - the part of the start's month that precedes `start`
///        + the part of the end's month that precedes `end`
/// ```
///
/// which makes a full calendar month exactly `1` whether it has 28 days or 31,
/// and a full year exactly `12`.
fn calendar_months(
    start: OffsetDateTime,
    end: OffsetDateTime,
    offset: UtcOffset,
) -> Option<Decimal> {
    if end <= start {
        // A seat created after the window closed owes nothing for it.
        return Some(Decimal::ZERO);
    }
    let s = start.to_offset(offset);
    let e = end.to_offset(offset);
    let whole = i64::from(e.year() - s.year()) * 12 + i64::from(u8::from(e.month()))
        - i64::from(u8::from(s.month()));
    Some(Decimal::from(whole) - elapsed_fraction(s, offset)? + elapsed_fraction(e, offset)?)
}

/// How much of its own calendar month has passed before `at`, as a fraction of
/// that month's length — so August divides by 31 and February by 28 or 29.
fn elapsed_fraction(at: OffsetDateTime, offset: UtcOffset) -> Option<Decimal> {
    let start = month_start(at.year(), at.month(), offset)?;
    let (ny, nm) = next_month(at.year(), at.month());
    let end = month_start(ny, nm, offset)?;
    let span = (end - start).whole_seconds();
    if span <= 0 {
        return None;
    }
    Some(Decimal::from((at - start).whole_seconds()) / Decimal::from(span))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use time::macros::datetime;

    fn at(window: &Window, now: OffsetDateTime) -> Resolved {
        resolve(window, now).expect("resolves")
    }

    fn period(name: &str) -> Window {
        Window {
            period: Some(name.to_owned()),
            ..Window::default()
        }
    }

    // ── the periods land on the boundaries they claim ────────────────────────

    #[test]
    fn month_runs_from_the_first_of_the_month_to_now() {
        let now = datetime!(2026-08-25 13:45:00 UTC);
        let r = at(&period("month"), now);
        assert_eq!(r.start, Some(datetime!(2026-08-01 00:00:00 UTC)));
        // Open-ended: the rows between the last event and now are not excluded
        // by a stale upper bound.
        assert_eq!(r.end, None);
        assert_eq!(r.effective_end(), now);
        assert_eq!(r.label, "2026-08 to date");
    }

    #[test]
    fn last_month_ends_where_this_month_begins_so_no_row_is_counted_twice() {
        let r = at(&period("last-month"), datetime!(2026-08-25 13:45:00 UTC));
        assert_eq!(r.start, Some(datetime!(2026-07-01 00:00:00 UTC)));
        assert_eq!(r.end, Some(datetime!(2026-08-01 00:00:00 UTC)));
        let this = at(&period("month"), datetime!(2026-08-25 13:45:00 UTC));
        assert_eq!(
            r.end, this.start,
            "the exclusive end of one month is the inclusive start of the next"
        );
    }

    #[test]
    fn last_month_in_january_walks_back_across_the_year() {
        let r = at(&period("last-month"), datetime!(2026-01-09 01:00:00 UTC));
        assert_eq!(r.start, Some(datetime!(2025-12-01 00:00:00 UTC)));
        assert_eq!(r.end, Some(datetime!(2026-01-01 00:00:00 UTC)));
        assert_eq!(r.label, "2025-12");
    }

    #[test]
    fn year_and_last_year_meet_at_new_year() {
        let now = datetime!(2026-08-25 13:45:00 UTC);
        assert_eq!(
            at(&period("year"), now).start,
            Some(datetime!(2026-01-01 00:00:00 UTC))
        );
        let previous = at(&period("last-year"), now);
        assert_eq!(previous.start, Some(datetime!(2025-01-01 00:00:00 UTC)));
        assert_eq!(previous.end, Some(datetime!(2026-01-01 00:00:00 UTC)));
        assert_eq!(previous.label, "2025");
    }

    #[test]
    fn all_is_unbounded_at_both_ends_because_the_ledger_is_never_purged() {
        let now = datetime!(2026-08-25 13:45:00 UTC);
        let r = at(&period("all"), now);
        assert_eq!(r.start, None);
        assert_eq!(r.end, None);
        assert_eq!(r.effective_end(), now);
    }

    #[test]
    fn a_custom_range_includes_the_whole_of_its_last_day() {
        let r = at(
            &Window {
                from: Some("2026-03-01".to_owned()),
                to: Some("2026-03-15".to_owned()),
                ..Window::default()
            },
            datetime!(2026-08-25 13:45:00 UTC),
        );
        assert_eq!(r.start, Some(datetime!(2026-03-01 00:00:00 UTC)));
        // The 16th's midnight, not the 15th's: an operator who wrote "to the
        // 15th" means through the end of it.
        assert_eq!(r.end, Some(datetime!(2026-03-16 00:00:00 UTC)));
        assert_eq!(r.label, "2026-03-01 to 2026-03-15");
    }

    #[test]
    fn a_single_day_range_is_that_day_and_only_that_day() {
        let r = at(
            &Window {
                from: Some("2024-02-29".to_owned()),
                to: Some("2024-02-29".to_owned()),
                ..Window::default()
            },
            datetime!(2026-08-25 13:45:00 UTC),
        );
        assert_eq!(r.start, Some(datetime!(2024-02-29 00:00:00 UTC)));
        assert_eq!(r.end, Some(datetime!(2024-03-01 00:00:00 UTC)));
    }

    #[test]
    fn a_month_boundary_is_computed_in_the_zone_the_caller_reads_it_in() {
        // 2026-08-01 03:00 in UTC+8 is still 2026-07-31 19:00 UTC. An operator
        // in Manila asking for "this month" must not be handed eight hours of
        // July, nor lose the first eight hours of August.
        let r = at(
            &Window {
                period: Some("month".to_owned()),
                tz: Some(480),
                ..Window::default()
            },
            datetime!(2026-08-01 03:00:00 +8),
        );
        assert_eq!(r.start, Some(datetime!(2026-08-01 00:00:00 +8)));
        assert_eq!(r.start, Some(datetime!(2026-07-31 16:00:00 UTC)));
        assert_eq!(r.view().tz_offset_minutes, 480);
    }

    // ── the rolling look-back is untouched ───────────────────────────────────

    #[test]
    fn days_still_means_exactly_what_it_meant_before_calendar_periods() {
        let now = datetime!(2026-08-25 13:45:00 UTC);
        let r = at(
            &Window {
                days: Some(7),
                ..Window::default()
            },
            now,
        );
        assert_eq!(r.start, Some(datetime!(2026-08-18 13:45:00 UTC)));
        assert_eq!(r.end, None, "no upper bound, as the old predicate had none");
        assert_eq!(r.period, "days");
    }

    #[test]
    fn an_unqualified_request_is_the_rolling_thirty_days_it_always_was() {
        let now = datetime!(2026-08-25 13:45:00 UTC);
        let r = at(&Window::default(), now);
        assert_eq!(r.start, Some(datetime!(2026-07-26 13:45:00 UTC)));
        assert_eq!(r.end, None);
        assert_eq!(r.label, "last 30 days");
    }

    #[test]
    fn days_is_still_clamped_to_one_and_ten_years() {
        let now = datetime!(2026-08-25 13:45:00 UTC);
        let zero = at(
            &Window {
                days: Some(0),
                ..Window::default()
            },
            now,
        );
        assert_eq!(zero.start, Some(now - Duration::days(1)));
        let huge = at(
            &Window {
                days: Some(999_999),
                ..Window::default()
            },
            now,
        );
        assert_eq!(huge.start, Some(now - Duration::days(3650)));
    }

    #[test]
    fn a_named_period_wins_over_a_days_left_behind_in_the_url() {
        let r = at(
            &Window {
                period: Some("last-month".to_owned()),
                days: Some(7),
                ..Window::default()
            },
            datetime!(2026-08-25 13:45:00 UTC),
        );
        assert_eq!(r.period, "last-month");
    }

    #[test]
    fn a_custom_range_wins_over_days_but_loses_to_a_named_period() {
        let now = datetime!(2026-08-25 13:45:00 UTC);
        let over_days = at(
            &Window {
                from: Some("2026-03-01".to_owned()),
                to: Some("2026-03-02".to_owned()),
                days: Some(7),
                ..Window::default()
            },
            now,
        );
        assert_eq!(over_days.period, "custom");
        let under_period = at(
            &Window {
                period: Some("year".to_owned()),
                from: Some("2026-03-01".to_owned()),
                to: Some("2026-03-02".to_owned()),
                ..Window::default()
            },
            now,
        );
        assert_eq!(under_period.period, "year");
    }

    // ── bad input is refused, in words that fix it ───────────────────────────

    #[test]
    fn an_unknown_period_is_refused_with_the_valid_values_named() {
        let err =
            resolve(&period("quarter"), datetime!(2026-08-25 13:45:00 UTC)).expect_err("refused");
        for valid in ["month", "last-month", "year", "last-year", "all", "days"] {
            assert!(err.contains(valid), "{err:?} should name {valid}");
        }
    }

    #[test]
    fn a_malformed_date_is_refused_with_the_shape_it_should_have_had() {
        let err = resolve(
            &Window {
                from: Some("01/03/2026".to_owned()),
                to: Some("2026-03-15".to_owned()),
                ..Window::default()
            },
            datetime!(2026-08-25 13:45:00 UTC),
        )
        .expect_err("refused");
        assert!(err.contains("YYYY-MM-DD"), "{err:?}");
    }

    #[test]
    fn a_date_that_does_not_exist_is_refused_rather_than_rounded() {
        let err = resolve(
            &Window {
                from: Some("2026-02-30".to_owned()),
                to: Some("2026-03-15".to_owned()),
                ..Window::default()
            },
            datetime!(2026-08-25 13:45:00 UTC),
        )
        .expect_err("refused");
        assert!(err.contains("YYYY-MM-DD"), "{err:?}");
    }

    #[test]
    fn from_after_to_is_refused_rather_than_silently_swapped() {
        let err = resolve(
            &Window {
                from: Some("2026-03-20".to_owned()),
                to: Some("2026-03-01".to_owned()),
                ..Window::default()
            },
            datetime!(2026-08-25 13:45:00 UTC),
        )
        .expect_err("refused");
        assert!(
            err.contains("2026-03-20") && err.contains("2026-03-01"),
            "{err:?}"
        );
    }

    #[test]
    fn half_a_range_is_refused_rather_than_guessed_at() {
        let err = resolve(
            &Window {
                from: Some("2026-03-01".to_owned()),
                ..Window::default()
            },
            datetime!(2026-08-25 13:45:00 UTC),
        )
        .expect_err("refused");
        assert!(err.contains("both from and to"), "{err:?}");
    }

    #[test]
    fn a_tz_outside_the_real_range_of_offsets_is_refused() {
        // A caller sending seconds instead of minutes lands here.
        let err = resolve(
            &Window {
                tz: Some(28_800),
                ..Window::default()
            },
            datetime!(2026-08-25 13:45:00 UTC),
        )
        .expect_err("refused");
        assert!(err.contains("minutes east of UTC"), "{err:?}");
    }

    // ── proration: the number the product's headline claim rests on ──────────

    /// A seat that has existed far longer than any window under test, so
    /// `created_at` clamping never interferes with the month arithmetic.
    const LONG_HELD: OffsetDateTime = datetime!(2020-01-01 00:00:00 UTC);

    #[test]
    fn a_full_thirty_one_day_month_charges_one_month_of_fee_not_thirty_one_thirtieths() {
        // August has 31 days. The old `days / 30` quoted 1.0333 of a fee for
        // it, so the same subscription cost more in August than in June.
        let r = at(&period("last-month"), datetime!(2026-09-10 00:00:00 UTC));
        assert_eq!(r.start, Some(datetime!(2026-08-01 00:00:00 UTC)));
        assert_eq!(r.seat_months(LONG_HELD), Some(dec!(1)));
    }

    #[test]
    fn february_charges_one_month_of_fee_and_not_twenty_eight_thirtieths() {
        // The mirror image: `days / 30` quoted 0.9333 of a fee for the whole of
        // February and reported the missing 6.7% as savings the seat earned.
        let r = at(&period("last-month"), datetime!(2026-03-10 00:00:00 UTC));
        assert_eq!(r.start, Some(datetime!(2026-02-01 00:00:00 UTC)));
        assert_eq!(r.seat_months(LONG_HELD), Some(dec!(1)));
    }

    #[test]
    fn a_leap_february_is_also_exactly_one_month() {
        let r = at(&period("last-month"), datetime!(2024-03-10 00:00:00 UTC));
        assert_eq!(r.start, Some(datetime!(2024-02-01 00:00:00 UTC)));
        assert_eq!(r.seat_months(LONG_HELD), Some(dec!(1)));
    }

    #[test]
    fn a_full_calendar_year_charges_twelve_months_of_fee() {
        let r = at(&period("last-year"), datetime!(2026-08-25 13:45:00 UTC));
        assert_eq!(r.seat_months(LONG_HELD), Some(dec!(12)));
    }

    #[test]
    fn a_part_month_charges_the_fraction_of_that_months_own_length() {
        // 2026-08-11 00:00 is ten of August's thirty-one days.
        let r = at(&period("month"), datetime!(2026-08-11 00:00:00 UTC));
        assert_eq!(r.seat_months(LONG_HELD), Some(dec!(10) / dec!(31)));
    }

    #[test]
    fn a_window_spanning_two_part_months_sums_both_at_their_own_lengths() {
        // 2026-07-15 → 2026-09-10: half of July at /31, all of August,
        // ten days of September at /30.
        let r = at(
            &Window {
                from: Some("2026-07-15".to_owned()),
                to: Some("2026-09-09".to_owned()),
                ..Window::default()
            },
            datetime!(2026-10-01 00:00:00 UTC),
        );
        let expected = dec!(17) / dec!(31) + dec!(1) + dec!(9) / dec!(30);
        let months = r.seat_months(LONG_HELD).expect("months");
        assert!(
            (months - expected).abs() < dec!(0.0000000001),
            "{months} vs {expected}"
        );
    }

    #[test]
    fn a_seat_is_only_charged_from_the_day_the_gateway_learned_about_it() {
        // Added on the 21st of a 31-day month: eleven days of fee, not a whole
        // month for a window in which no request could have reached it.
        let r = at(&period("last-month"), datetime!(2026-09-10 00:00:00 UTC));
        let created = datetime!(2026-08-21 00:00:00 UTC);
        assert_eq!(r.seat_months(created), Some(dec!(11) / dec!(31)));
    }

    #[test]
    fn a_seat_created_after_the_window_closed_owes_nothing_for_it() {
        let r = at(&period("last-month"), datetime!(2026-09-10 00:00:00 UTC));
        assert_eq!(
            r.seat_months(datetime!(2026-09-05 00:00:00 UTC)),
            Some(Decimal::ZERO)
        );
    }

    #[test]
    fn all_time_prorates_from_when_the_seat_was_added_since_it_has_no_start() {
        let r = at(&period("all"), datetime!(2026-08-01 00:00:00 UTC));
        assert_eq!(r.start, None);
        assert_eq!(
            r.seat_months(datetime!(2026-06-01 00:00:00 UTC)),
            Some(dec!(2)),
            "June and July in full"
        );
    }

    #[test]
    fn the_reported_window_names_both_ends_so_a_reader_cannot_misread_it() {
        let view = at(&period("last-month"), datetime!(2026-08-25 13:45:00 UTC)).view();
        assert_eq!(view.period, "last-month");
        assert_eq!(view.start.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(view.end, "2026-08-01T00:00:00Z");
    }

    #[test]
    fn an_open_window_reports_the_instant_it_was_answered_as_its_end() {
        let now = datetime!(2026-08-25 13:45:00 UTC);
        let view = at(&period("month"), now).view();
        assert_eq!(view.end, "2026-08-25T13:45:00Z");
    }
}
