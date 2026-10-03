//! Resolving `--day`, `--from`/`--to`, or nothing at all into the inclusive epoch window a search
//! runs over.
//!
//! Precedence is fixed and stated once here: an explicit `--day` beats `--from`/`--to`, which beats
//! the default of "today". A half-given explicit window is an error rather than an open-ended scan,
//! because an unbounded range is the one way this tool could quietly read every month file the user
//! owns — and when the command is `windmaint forget`, an unbounded window is one typo away from
//! erasing the library.
//!
//! All bounds are in the app's own naive-local epoch (`wind_base::clock`), never POSIX seconds and
//! never a SQLite date — the values on disk are that shape, and a conversion here would shift every
//! window by the machine's UTC offset.
//!
//! This lives in the base crate rather than beside its first caller because `windcapctl query` and
//! `windmaint forget` must accept the *same* spellings of a day: two parsers of that pair is how one
//! tool ends up erasing a different period than the other just reported.

use crate::clock::{self, LocalParts};

/// The raw window flags as the user typed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RangeArg {
    pub day: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Which rule produced a [`Span`]. Printed in every report, because "why does this say 3 rows"
/// always starts with "which window did you search".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `--day` or the `day` positional.
    Day,
    /// `--from`/`--to`.
    Explicit,
    /// Neither: the product day containing the wall clock.
    Today,
    /// `--month`.
    Month,
}

/// An inclusive window in the stored epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub from: i64,
    pub to: i64,
    pub origin: Origin,
    /// The day-begin shift in effect, so a reader can tell a 03:00 product day from a calendar day.
    pub day_begin_minutes: i64,
}

impl Span {
    /// `2026-09-22 03:00:00 .. 2026-09-23 02:59:59`, the both-ends form the reports quote.
    pub fn label(&self) -> String {
        format!(
            "{} .. {}",
            LocalParts::from_naive_epoch(self.from).display(),
            LocalParts::from_naive_epoch(self.to).display()
        )
    }

    pub fn seconds(&self) -> i64 {
        (self.to - self.from).max(0) + 1
    }

    /// `HH:MM` for the shift that produced these bounds. Quoted in every report because a "day" that
    /// silently starts at 03:00 is the single most confusing thing about this product's data.
    pub fn day_begin_label(&self) -> String {
        let minutes = self.day_begin_minutes;
        format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60)
    }
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Day => "day",
            Origin::Explicit => "explicit",
            Origin::Today => "today",
            Origin::Month => "month",
        }
    }
}

/// `YYYY-MM-DD` -> `(year, month, day)`, rejecting anything the calendar does not have.
///
/// The calendar check comes for free by parsing the date as a full stamp: `LocalParts::from_stamp`
/// already refuses month 13 and 29 February in a non-leap year, and re-implementing that here is how
/// the two rules drift apart.
pub fn parse_date(text: &str) -> Option<(i64, u32, u32)> {
    if text.len() != 10 {
        return None;
    }
    let parts = LocalParts::from_stamp(&format!("{text}_00-00-00"))?;
    Some((parts.year, parts.month, parts.day))
}

/// A `YYYY-MM-DD_HH-MM-SS` stamp, or a bare date at either end of that date.
///
/// `date_at_end` decides which way a date-only value is rounded out. Left unrounded, `--to
/// 2026-09-23` would silently mean "up to midnight" and drop the whole of the 23rd, which is the
/// opposite of what a user typing a date means by it.
pub fn parse_instant(text: &str, date_at_end: bool) -> Option<i64> {
    if let Some((y, m, d)) = parse_date(text) {
        let seconds = LocalParts { year: y, month: m, day: d, hour: 0, minute: 0, second: 0 }.naive_epoch_seconds();
        return Some(if date_at_end { seconds + 86_400 - 1 } else { seconds });
    }
    LocalParts::from_stamp(text).map(|p| p.naive_epoch_seconds())
}

/// One product day: `[day 03:00:00, next day 02:59:59]` for the shipped `day_begin_minutes`.
pub fn product_day(year: i64, month: u32, day: u32, day_begin_minutes: i64, origin: Origin) -> Span {
    let (from, to) = clock::day_bounds(year, month, day, day_begin_minutes);
    Span { from, to, origin, day_begin_minutes }
}

/// A whole calendar month, used by `stats --month` and the library-wide reports.
pub fn parse_month(text: &str) -> Result<Span, String> {
    let bad = || format!("invalid --month '{text}': expected YYYY-MM");
    if text.len() != 7 || text.as_bytes().get(4) != Some(&b'-') {
        return Err(bad());
    }
    let (y, m) = text.split_once('-').ok_or_else(bad)?;
    let year: i64 = y.parse().map_err(|_| bad())?;
    let month: u32 = m.parse().map_err(|_| bad())?;
    if !(1970..2200).contains(&year) || !(1..=12).contains(&month) {
        return Err(bad());
    }
    let from = LocalParts { year, month, day: 1, hour: 0, minute: 0, second: 0 }.naive_epoch_seconds();
    let last = clock::days_in_month(year, month);
    let to = LocalParts { year, month, day: last, hour: 23, minute: 59, second: 59 }.naive_epoch_seconds();
    Ok(Span { from, to, origin: Origin::Month, day_begin_minutes: 0 })
}

/// The window a command should search. Errors here are always the user's typo, never the data's.
pub fn resolve(spec: &RangeArg, day_begin_minutes: i64, now: LocalParts) -> Result<Span, String> {
    if let Some(day) = &spec.day {
        let (y, m, d) = parse_date(day).ok_or_else(|| format!("invalid --day '{day}': expected YYYY-MM-DD"))?;
        return Ok(product_day(y, m, d, day_begin_minutes, Origin::Day));
    }
    match (&spec.from, &spec.to) {
        (Some(from), Some(to)) => {
            let (Some(from), Some(to)) = (parse_instant(from, false), parse_instant(to, true)) else {
                return Err("invalid --from/--to stamp".to_string());
            };
            if from > to {
                return Err(format!("--from '{}' is after --to '{}'", stamp(from), stamp(to)));
            }
            Ok(Span { from, to, origin: Origin::Explicit, day_begin_minutes })
        }
        (Some(_), None) => Err("--to is required when --from is given".to_string()),
        (None, Some(_)) => Err("--from is required when --to is given".to_string()),
        (None, None) => Ok(product_day(now.year, now.month, now.day, day_begin_minutes, Origin::Today)),
    }
}

fn stamp(seconds: i64) -> String {
    LocalParts::from_naive_epoch(seconds).display()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> i64 {
        LocalParts::from_stamp(text).unwrap().naive_epoch_seconds()
    }

    fn spec(day: Option<&str>, from: Option<&str>, to: Option<&str>) -> RangeArg {
        RangeArg { day: day.map(str::to_string), from: from.map(str::to_string), to: to.map(str::to_string) }
    }

    #[test]
    fn a_date_parses_only_in_the_format_the_whole_product_uses() {
        assert_eq!(parse_date("2026-09-22"), Some((2026, 9, 22)));
        assert_eq!(parse_date("2026-2-9"), None, "no zero padding, no deal");
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("2025-02-29"), None);
        assert_eq!(parse_date("2024-02-29"), Some((2024, 2, 29)));
        assert_eq!(parse_date("2026-09-22_00-00-00"), None, "a stamp is not a date");
    }

    #[test]
    fn a_bare_date_is_widened_to_whichever_end_of_the_day_the_bound_sits_at() {
        assert_eq!(parse_instant("2026-09-22", false), Some(at("2026-09-22_00-00-00")));
        assert_eq!(parse_instant("2026-09-22", true), Some(at("2026-09-22_23-59-59")));
        assert_eq!(parse_instant("2026-09-22_07-30-00", true), Some(at("2026-09-22_07-30-00")));
        assert_eq!(parse_instant("nonsense", false), None);
    }

    #[test]
    fn an_explicit_month_span_covers_the_calendar_month() {
        let m = parse_month("2026-09").unwrap();
        assert_eq!((m.from, m.to), (at("2026-09-01_00-00-00"), at("2026-09-30_23-59-59")));
        let february = parse_month("2024-02").unwrap();
        assert_eq!(february.to, at("2024-02-29_23-59-59"));
        for bad in ["2026", "2026-13", "2026/09", "20269-", ""] {
            assert!(parse_month(bad).is_err(), "{bad} should not parse");
        }
    }

    /// The precedence the module doc promises.
    #[test]
    fn day_wins_over_an_explicit_window_which_wins_over_today() {
        let now = LocalParts::from_stamp("2026-09-22_12-00-00").unwrap();
        let both = spec(Some("2026-09-21"), Some("2026-09-22_00-00-00"), Some("2026-09-22_00-00-00"));
        let span = resolve(&both, 180, now).unwrap();
        assert_eq!(span.origin, Origin::Day);
        assert_eq!((span.from, span.to), (at("2026-09-21_03-00-00"), at("2026-09-22_02-59-59")));

        let explicit = spec(None, Some("2026-09-22_08-00-00"), Some("2026-09-22_09-00-00"));
        let span = resolve(&explicit, 180, now).unwrap();
        assert_eq!(span.origin, Origin::Explicit);
        assert_eq!((span.from, span.to), (at("2026-09-22_08-00-00"), at("2026-09-22_09-00-00")));

        let span = resolve(&spec(None, None, None), 180, now).unwrap();
        assert_eq!(span.origin, Origin::Today);
        assert_eq!((span.from, span.to), (at("2026-09-22_03-00-00"), at("2026-09-23_02-59-59")));
    }

    #[test]
    fn a_half_given_window_is_an_error_not_an_open_ended_scan() {
        let now = LocalParts::from_stamp("2026-09-22_12-00-00").unwrap();
        assert!(resolve(&spec(None, Some("2026-09-22_08-00-00"), None), 180, now).is_err());
        assert!(resolve(&spec(None, None, Some("2026-09-22_08-00-00")), 180, now).is_err());
        assert!(resolve(&spec(Some("yesterday"), None, None), 180, now).is_err());
    }

    #[test]
    fn bounds_the_wrong_way_round_are_reported_rather_than_answered() {
        let err = resolve(&spec(None, Some("2026-09-23_00-00-00"), Some("2026-09-22_00-00-00")), 180, clock::now())
            .unwrap_err();
        assert!(err.contains("is after --to"), "{err}");
    }

    #[test]
    fn a_day_at_midnight_is_exactly_the_calendar_day() {
        let span = product_day(2026, 9, 22, 0, Origin::Day);
        assert_eq!(span.seconds(), 86_400);
        assert_eq!(span.label(), "2026-09-22 00:00:00 .. 2026-09-22 23:59:59");
    }

    /// A product day that crosses a month boundary is the reason `stats` cannot group by substring.
    #[test]
    fn a_shifted_day_crosses_the_month_boundary() {
        let span = product_day(2026, 9, 1, 180, Origin::Day);
        assert_eq!(span.from, at("2026-09-01_03-00-00"));
        assert_eq!(span.to, at("2026-09-02_02-59-59"));
        let december = product_day(2026, 12, 31, 180, Origin::Day);
        assert_eq!(december.to, at("2027-01-01_02-59-59"));
    }
}
