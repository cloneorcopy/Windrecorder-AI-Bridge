//! Which recorded stretch a summary belongs to, and which *product day* files it.
//!
//! Both questions look trivial and neither is. "Which segment" has three spellings an agent will
//! actually send (the `.mp4` name from a search hit, the bare stamp, or the `timestamp` integer a
//! previous tool handed back), and they must all land on one key or the same stretch gets three
//! summaries. "Which day" is the one the whole design hangs on: a 02:50 stretch belongs to *yesterday*
//! under the install's own day start, and if the summary is filed by calendar date while the coverage
//! gate counts by product day, a day can be simultaneously "complete" and "missing entries" — which is
//! not a bug a user sees, it is a bug they are slowly lied to by.
//!
//! So the rule is imported, not restated: [`day_of`] answers through `wind_base::clock::day_bounds`,
//! the same function `range::product_day` uses for `windrecorder_search --day`, so the two doors
//! cannot disagree about the boundary they are both printing in their payloads.

use wind_base::clock::{self, LocalParts};
use wind_base::range::{self, Origin, Span};

/// A day as every filename and payload field spells it.
pub const DAY_FORMAT_NOTE: &str = "YYYY-MM-DD";

/// The canonical key of a recorded segment: its 19-character start stamp, `2026-09-27_15-47-17`.
///
/// Accepts the name with or without an extension, and with either underscore or hyphen inside the
/// time part, because `videofile_name` carries the extension and a search hit's `timestamp` does not.
/// The parse is `LocalParts::from_stamp`, the same one `Row::segment_stamp` runs over the column, so a
/// key this returns is by construction a key the index can produce — and February 30 is refused in the
/// one place the workspace already refuses it.
pub fn canonical_key(reference: &str) -> Option<String> {
    let stamp = LocalParts::from_stamp(reference.trim())?;
    Some(stamp.stamp())
}

/// `YYYY-MM-DD` -> the coordinates, only if the calendar has that day.
pub fn parse_day(day: &str) -> Option<(i64, u32, u32)> {
    range::parse_date(day)
}

/// The product day that owns an instant, as `YYYY-MM-DD`.
///
/// `day_bounds` returns `[day 03:00:00, next day 02:59:59]` for the shipped shift, so an instant
/// outside that window is one day-step away from the answer. The name of the owning day is the day whose
/// 03:00 *begins* the window, not the calendar date the instant falls on: 27th 02:00 belongs to the day
/// that started 26th 03:00, and filing it under the 27th would put a paragraph in a file the day's
/// coverage never looks at.
pub fn day_of(instant: i64, day_begin_minutes: i64) -> String {
    let here = LocalParts::from_naive_epoch(instant);
    let (from, to) = clock::day_bounds(here.year, here.month, here.day, day_begin_minutes);
    let owner = if instant < from {
        LocalParts::from_naive_epoch(from - 86_400)
    } else if instant > to {
        LocalParts::from_naive_epoch(to + 1)
    } else {
        here
    };
    owner.date_stamp()
}

/// The window one product day covers.
pub fn day_span(day: &str, day_begin_minutes: i64) -> Option<Span> {
    let (year, month, day_of_month) = parse_day(day)?;
    Some(range::product_day(year, month, day_of_month, day_begin_minutes, Origin::Day))
}

/// Every product day from `from` to `to` inclusive, as filenames in order.
///
/// Stepping by 86 400 seconds from the *calendar* midnight of each day rather than by "one day" on
/// the string, so a month with 31 days, a February, and a year boundary all come out of the same
/// arithmetic the clock already owns.
pub fn days_in_range(from: &str, to: &str) -> Vec<String> {
    let Some((y, m, d)) = parse_day(from) else { return Vec::new() };
    let Some((ey, em, ed)) = parse_day(to) else { return Vec::new() };
    let start = LocalParts { year: y, month: m, day: d, hour: 12, minute: 0, second: 0 }.naive_epoch_seconds();
    let end = LocalParts { year: ey, month: em, day: ed, hour: 12, minute: 0, second: 0 }.naive_epoch_seconds();
    if end < start {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cursor = start;
    // Noon on each side: a fixed 86 400 s step can never cross a daylight-saving boundary in a way
    // that lands on the wrong date, because no offset reaches four hours.
    while cursor <= end {
        out.push(LocalParts::from_naive_epoch(cursor).date_stamp());
        cursor += 86_400;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT: i64 = 180; // 03:00, as shipped.

    #[test]
    fn every_spelling_a_tool_hands_back_resolves_to_one_key() {
        for given in [
            "2026-09-27_15-47-17.mp4",
            "2026-09-27_15-47-17",
            "  2026-09-27_15-47-17.webm  ",
            "2026-09-27_15-47-17_frame.jpg",
        ] {
            assert_eq!(canonical_key(given).as_deref(), Some("2026-09-27_15-47-17"), "{given}");
        }
    }

    #[test]
    fn a_key_is_a_real_instant_and_nothing_else() {
        assert_eq!(canonical_key("2026-13-01_00-00-00"), None, "month 13 is not a segment");
        assert_eq!(canonical_key("2025-02-29_10-00-00"), None, "nor is a leap day in a common year");
        assert_eq!(canonical_key("yesterday"), None);
        assert_eq!(canonical_key(""), None);
        // A bare date is a *day*, not a stretch: accepting it as a segment key would let one write
        // silently claim a whole day's worth of segments.
        assert_eq!(canonical_key("2026-09-27"), None);
    }

    #[test]
    fn the_day_rolls_at_the_configured_hour_not_at_midnight() {
        let at = |stamp: &str| LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds();
        assert_eq!(day_of(at("2026-09-27_15-47-17"), SHIFT), "2026-09-27");
        assert_eq!(day_of(at("2026-09-27_03-00-00"), SHIFT), "2026-09-27", "the first second is inside");
        assert_eq!(day_of(at("2026-09-27_02-59-59"), SHIFT), "2026-09-26", "the last second of yesterday");
        assert_eq!(day_of(at("2026-09-27_00-00-30"), SHIFT), "2026-09-26", "and the small hours too");
    }

    #[test]
    fn a_day_that_rolls_back_midnight_rolls_back_too() {
        // `day_begin_minutes` is configurable; with a zero shift the product day is the calendar day,
        // and every assertion above must degrade to the boring answer rather than to an off-by-one.
        assert_eq!(day_of(LocalParts::from_stamp("2026-09-27_00-00-00").unwrap().naive_epoch_seconds(), 0), "2026-09-27");
    }

    #[test]
    fn the_rollover_crosses_month_and_year_ends() {
        let at = |stamp: &str| LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds();
        assert_eq!(day_of(at("2026-10-01_02-00-00"), SHIFT), "2026-09-30", "back over the month end");
        assert_eq!(day_of(at("2027-01-01_01-00-00"), SHIFT), "2026-12-31", "and over the year end");
    }

    #[test]
    fn a_days_window_is_the_same_window_the_bridge_prints() {
        let span = day_span("2026-09-27", SHIFT).expect("a real day");
        assert_eq!(
            span.label(),
            "2026-09-27 03:00:00 .. 2026-09-28 02:59:59",
            "the payload promise and the storage boundary must be one function's output"
        );
        assert_eq!(span.day_begin_label(), "03:00");
    }

    #[test]
    fn a_bad_day_names_the_shape_it_wanted() {
        assert!(day_span("2026-09-27", SHIFT).is_some(), "a real day parses");
        assert!(day_span("2026-9-7", SHIFT).is_none());
        assert!(day_span("", SHIFT).is_none());
        assert_eq!(canonical_key("2026-9-7"), None);
    }

    #[test]
    fn the_day_list_walks_a_month_and_a_year_boundary() {
        let days = days_in_range("2026-09-28", "2026-10-02");
        assert_eq!(days, vec!["2026-09-28", "2026-09-29", "2026-09-30", "2026-10-01", "2026-10-02"]);
        assert_eq!(days_in_range("2026-12-31", "2027-01-02").len(), 3);
        assert_eq!(days_in_range("2026-02-27", "2026-03-01").len(), 3, "and a 28-day February");
        assert_eq!(days_in_range("2024-02-27", "2024-03-01").len(), 4, "and a leap one");
        assert!(days_in_range("2026-09-28", "2026-09-27").is_empty(), "a reversed range is no days");
        assert!(days_in_range("nope", "2026-09-27").is_empty());
    }

    #[test]
    fn a_single_day_range_is_that_one_day() {
        assert_eq!(days_in_range("2026-09-27", "2026-09-27"), vec!["2026-09-27".to_string()]);
    }
}
