//! The date axis, and why every conversion on it has to go through one type.
//!
//! `videofile_time` is **not** a POSIX timestamp. Upstream builds it by taking a naive local
//! `datetime`, formatting it, and subtracting 1970-01-01 *as if that string were UTC*
//! (`windrecorder/utils.py:103-110`), so on this UTC+8 machine a stored value is
//! `true_unix + 28800`. `wind_base::clock::LocalParts` is the only place that offset is allowed to
//! live, and this module is the only place this crate is allowed to cross between it and the
//! `%Y-%m-%d` text a language model thinks in.
//!
//! The failure mode if that discipline slips is specific and bad: a user asks for "yesterday
//! afternoon", the model answers `2026-09-22`, and the eight hours between the two epoch conventions
//! decide whether the search covers that day or starts at 08:00 and silently misses the morning. The
//! user does not conclude that there are two calendars in the product — they conclude that search is
//! broken. So there is exactly one pair of functions here, and no `SystemTime`/`UNIX_EPOCH` arithmetic
//! anywhere in the crate to drift into.
//!
//! The other thing this module owns is *refusing to believe the model*. A date range is the one part
//! of a model's answer that can make a search return nothing at all, so it is clamped into the
//! library's real bounds rather than passed through, and every clamp is recorded for `--explain`.

use wind_base::clock::LocalParts;

/// A day, in seconds, on the stored axis.
pub const DAY_SECONDS: i64 = 86_400;
/// The last second of a day that starts at midnight.
pub const DAY_END_OFFSET: i64 = DAY_SECONDS - 1;
/// Dates outside this band are not a misunderstanding, they are a hallucination, and no arithmetic
/// downstream is specced for them.
const SANE_YEAR: std::ops::RangeInclusive<i64> = 1971..=2999;

/// What a model's two date strings became after validation, and what had to change to get there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Inclusive, in the stored naive-local epoch — the unit `wind_store::search::Query` takes.
    pub from: i64,
    pub to: i64,
    /// Human-readable, in `%Y-%m-%d`, exactly as the model wrote them (before clamping).
    pub requested: (String, String),
    /// One entry per thing this module overrode. Non-empty means the answer is not the model's.
    pub notes: Vec<String>,
}

impl Resolved {
    pub fn was_adjusted(&self) -> bool {
        !self.notes.is_empty()
    }

    /// The pair `Query::new` takes.
    pub fn span(&self) -> (i64, i64) {
        (self.from, self.to)
    }

    /// A one-line summary for `--explain`: `2026-09-21 → 2026-09-22`, in the dates the *query* used.
    pub fn display(&self) -> String {
        format!("{} → {}", LocalParts::from_naive_epoch(self.from).date_stamp(), LocalParts::from_naive_epoch(self.to).date_stamp())
    }
}

/// Midnight at the start of a `%Y-%m-%d` day, on the stored axis.
///
/// `LocalParts::from_date` is the only parser used, which means `2026-02-30` and `2026-13-01` are
/// rejected here for the same reason they are rejected everywhere else in the workspace: two
/// calendars agreeing on whether a date exists is not negotiable.
pub fn day_start(text: &str) -> Option<LocalParts> {
    let parts = LocalParts::from_date(text)?;
    if !SANE_YEAR.contains(&parts.year) {
        return None;
    }
    Some(parts)
}

/// Inclusive `[00:00:00, 23:59:59]` of one day.
pub fn day_window(parts: &LocalParts) -> (i64, i64) {
    let start = parts.naive_epoch_seconds();
    (start, start + DAY_END_OFFSET)
}

/// The whole library's span, rendered as the dates a prompt can show the model.
pub fn bound_dates(bounds: (i64, i64)) -> (String, String) {
    (LocalParts::from_naive_epoch(bounds.0).date_stamp(), LocalParts::from_naive_epoch(bounds.1).date_stamp())
}

/// Turn a model's `start_date`/`end_date` into a range the index can be asked for.
///
/// `Err` is reserved for answers that are not dates at all — there is no honest way to clamp
/// `"sometime last year"`, and inventing a range would produce a confident wrong answer, which is
/// worse than a refusal. Everything else is a clamp plus a note:
///
///   * reversed bounds are swapped (a model that writes `end_date` first is describing the same two
///     days);
///   * a range partly outside the library is cut to the library;
///   * a range *entirely* outside the library is collapsed onto the nearer end rather than returned
///     as an empty search, because "the model said 1833" is a fact about the model, not about the
///     user's history, and the user's next question is the same one.
pub fn resolve(start: &str, end: &str, bounds: (i64, i64)) -> Result<Resolved, String> {
    let requested = (start.trim().to_string(), end.trim().to_string());
    let first = day_start(start).ok_or_else(|| format!("`{start}` is not a valid YYYY-MM-DD date"))?;
    let last = day_start(end).ok_or_else(|| format!("`{end}` is not a valid YYYY-MM-DD date"))?;
    let mut notes = Vec::new();

    // A day contributes its whole span, so the two numbers that matter are the earlier day's midnight
    // and the later day's 23:59:59 — never a "less than tomorrow midnight", which is off by one second
    // the moment a row is recorded at exactly midnight.
    let (first_from, first_to) = day_window(&first);
    let (last_from, last_to) = day_window(&last);
    let mut from = first_from.min(last_from);
    let mut to = first_to.max(last_to);
    if first_from > last_from {
        // The model put the later day first. Same two days, so honour them the other way round rather
        // than returning an empty range that the user will read as a broken search.
        notes.push(format!("`{start}` is after `{end}`; the two were treated as the same pair of days"));
    }

    let (lowest, highest) = bounds;
    if to < lowest || from > highest {
        // Wholly outside. Collapse onto the nearer edge, expressed as a *day* rather than as a
        // timestamp, so the clamped range still covers something a user could plausibly have meant
        // instead of a one-second sliver at an arbitrary offset into the first row on disk.
        let at_start = to < lowest;
        let anchor = LocalParts::from_naive_epoch(if at_start { lowest } else { highest }).date_only();
        let (clamped_from, clamped_to) = day_window(&anchor);
        notes.push(format!(
            "the requested range {start}…{end} lies entirely outside the recorded history ({}…{}); \
             it was clamped to {}",
            LocalParts::from_naive_epoch(lowest).date_stamp(),
            LocalParts::from_naive_epoch(highest).date_stamp(),
            anchor.date_stamp()
        ));
        return Ok(Resolved { from: clamped_from, to: clamped_to, requested, notes });
    }

    let before = (from, to);
    from = from.max(lowest);
    to = to.min(highest);
    if (from, to) != before {
        let (shown_from, shown_to) = bound_dates((from, to));
        notes.push(format!(
            "the requested range {start}…{end} extends past the recorded history; it was clamped to \
             {shown_from}…{shown_to}"
        ));
    }
    debug_assert!(from <= to, "the branches above cannot produce an inverted range");
    Ok(Resolved { from, to, requested, notes })
}

/// A range covering everything the library holds, with no note: the answer when a query says nothing
/// about time at all, which upstream reaches for via `db_first_earliest_record_time()` and `now()`.
pub fn everything(bounds: (i64, i64)) -> Resolved {
    Resolved { from: bounds.0, to: bounds.1, requested: (String::new(), String::new()), notes: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_base::clock::LocalParts;

    fn epoch(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    /// The library spans 2026-09-01 .. 2026-09-30, all naive-local.
    fn bounds() -> (i64, i64) {
        (epoch("2026-09-01_00-00-00"), epoch("2026-09-30_23-59-59"))
    }

    /// The whole point of the module, stated as a number: `2026-09-22` means the 22nd on the *stored*
    /// axis, which is not the 22nd in UTC. 1_790_025_372 is the value the production database holds
    /// for 2026-09-21_21-16-12, so this pins the same anchor `wind-store` pins.
    #[test]
    fn a_date_lands_on_the_axis_the_database_uses() {
        let parts = day_start("2026-09-21").unwrap();
        assert_eq!(parts.naive_epoch_seconds(), 1_789_948_800);
        // True UTC midnight for the same calendar day is eight hours later in epoch terms; mixing the
        // two up is the bug this function exists to prevent.
        assert_eq!(epoch("2026-09-21_21-16-12"), 1_790_025_372);
        // 21:16:12 into the day, which is the row the production database actually holds.
        assert_eq!(1_790_025_372 - parts.naive_epoch_seconds(), 76_572);
    }

    #[test]
    fn a_day_is_midnight_to_the_last_second_of_it() {
        let (from, to) = day_window(&day_start("2026-09-22").unwrap());
        assert_eq!((from, to), (epoch("2026-09-22_00-00-00"), epoch("2026-09-22_23-59-59")));
        assert_eq!(to - from, DAY_END_OFFSET);
    }

    #[test]
    fn a_range_inside_the_library_passes_through_untouched() {
        let resolved = resolve("2026-09-10", "2026-09-12", bounds()).unwrap();
        assert_eq!(resolved.span(), (epoch("2026-09-10_00-00-00"), epoch("2026-09-12_23-59-59")));
        assert!(!resolved.was_adjusted(), "{:?}", resolved.notes);
        assert_eq!(resolved.display(), "2026-09-10 → 2026-09-12");
    }

    #[test]
    fn a_partly_outside_range_is_cut_not_refused() {
        let resolved = resolve("2026-08-20", "2026-09-05", bounds()).unwrap();
        assert_eq!(resolved.from, epoch("2026-09-01_00-00-00"), "the cut is at the library's start");
        assert_eq!(resolved.to, epoch("2026-09-05_23-59-59"));
        assert!(resolved.was_adjusted());
        assert!(resolved.notes[0].contains("clamped"), "{}", resolved.notes[0]);
    }

    #[test]
    fn a_range_entirely_outside_is_collapsed_onto_the_nearest_day() {
        let before = resolve("2024-01-01", "2024-01-31", bounds()).unwrap();
        assert_eq!(before.from, epoch("2026-09-01_00-00-00"));
        assert_eq!(before.to, epoch("2026-09-01_23-59-59"));
        assert!(before.notes[0].contains("entirely outside"), "{}", before.notes[0]);

        let after = resolve("2031-03-01", "2031-03-31", bounds()).unwrap();
        assert_eq!(after.from, epoch("2026-09-30_00-00-00"));
        assert_eq!(after.to, epoch("2026-09-30_23-59-59"));

        // An empty search would be the "correct" answer and the useless one; both branches stay a
        // whole day so there is something to look at.
        assert_eq!(after.to - after.from, DAY_END_OFFSET);
    }

    #[test]
    fn reversed_dates_are_treated_as_the_same_pair_of_days() {
        let resolved = resolve("2026-09-12", "2026-09-10", bounds()).unwrap();
        assert_eq!((resolved.from, resolved.to), (epoch("2026-09-10_00-00-00"), epoch("2026-09-12_23-59-59")));
        assert!(resolved.notes.iter().any(|n| n.contains("is after")), "{:?}", resolved.notes);
    }

    #[test]
    fn a_single_day_query_covers_that_day() {
        let resolved = resolve("2026-09-15", "2026-09-15", bounds()).unwrap();
        assert_eq!(resolved.to - resolved.from, DAY_END_OFFSET);
    }

    #[test]
    fn prose_instead_of_a_date_is_an_error_not_a_guess() {
        for bad in ["last tuesday", "2026-9-5", "2026-02-30", "2026-13-01", "", "yesterday afternoon"] {
            let error = resolve(bad, "2026-09-10", bounds()).expect_err("{bad} must not parse");
            assert!(error.contains("YYYY-MM-DD"), "{error}");
        }
    }

    #[test]
    fn a_year_that_cannot_be_a_recording_is_rejected() {
        assert!(resolve("1900-01-01", "2026-09-10", bounds()).is_err(), "no screen recorder is that old");
        assert!(resolve("2026-09-10", "3099-01-01", bounds()).is_err());
        assert!(day_start("1970-01-01").is_none(), "below the band on purpose: 1970 is the epoch itself");
        assert!(day_start("2999-12-31").is_some());
    }

    #[test]
    fn bound_dates_round_trips_through_the_axis() {
        let (first, last) = bound_dates(bounds());
        assert_eq!((first.as_str(), last.as_str()), ("2026-09-01", "2026-09-30"));
        let resolved = resolve(&first, &last, bounds()).unwrap();
        assert_eq!(resolved.span(), everything(bounds()).span());
    }

    #[test]
    fn everything_spans_the_library_invisibly() {
        let resolved = everything(bounds());
        assert!(!resolved.was_adjusted());
        assert_eq!(resolved.display(), "2026-09-01 → 2026-09-30");
    }
}
