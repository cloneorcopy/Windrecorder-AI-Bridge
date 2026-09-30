//! Wall-clock helpers.
//!
//! `videofile_time` in the existing databases is *not* a POSIX timestamp. The Python app builds it
//! with `dtstr_to_seconds` (`utils.py:103-110`), which takes a naive local timestamp and subtracts
//! 1970-01-01 as if it were UTC. On a UTC+8 machine the stored value is therefore
//! `true_unix + 28800`. Writing true POSIX seconds into the same file would put every new row eight
//! hours away from the existing ones, so the offset is applied deliberately.

use serde::{Deserialize, Serialize};

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct SystemTimeRaw {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLocalTime(out: *mut SystemTimeRaw);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalParts {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn now() -> LocalParts {
    let mut raw = SystemTimeRaw::default();
    unsafe { GetLocalTime(&mut raw) };
    LocalParts {
        year: i64::from(raw.year),
        month: u32::from(raw.month),
        day: u32::from(raw.day),
        hour: u32::from(raw.hour),
        minute: u32::from(raw.minute),
        second: u32::from(raw.second),
    }
}

impl LocalParts {
    /// Minutes after midnight, which is the unit a maintenance window is written in.
    ///
    /// A clock time and an epoch second are not the same kind of fact: `22:00` means the same minute
    /// every day, so comparing a window against `naive_epoch_seconds` would have to divide out whole
    /// days and would then get midnight wrong on the one boundary that matters — a pass that is told
    /// to stop at `06:00` has to stop there today, not at the same offset from some epoch.
    pub fn minute_of_day(&self) -> u32 {
        self.hour * 60 + self.minute
    }

    /// Naive-local seconds since 1970, matching the value the Python writer stores.
    pub fn naive_epoch_seconds(&self) -> i64 {
        (days_from_civil(self.year, i64::from(self.month), i64::from(self.day)) * 86_400)
            + i64::from(self.hour) * 3_600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
    }

    /// `%Y-%m-%d_%H-%M-%S`, the filename/timestamp format in `const.py` `DATETIME_FORMAT`.
    pub fn stamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `%Y-%m-%d`, the filename format of the window-title side channel.
    pub fn date_stamp(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `HH:MM:SS` only, which is what a result list shows: the date repeats on every row of a day
    /// view, and printing it costs a column of the only space that matters.
    pub fn time_display(&self) -> String {
        let full = self.display();
        full.get(11..).unwrap_or(&full).to_string()
    }

    /// `%Y-%m-%d %H:%M:%S`, what the UI shows for a row.
    pub fn display(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// Parse a bare `%Y-%m-%d` as midnight of that calendar day.
    ///
    /// Every date the UI and the CLI accept is this shape, and re-validating it separately would
    /// mean two calendars disagreeing about whether 2026-02-30 exists.
    pub fn from_date(text: &str) -> Option<LocalParts> {
        let mut stamp = String::with_capacity(19);
        stamp.push_str(text.trim());
        stamp.push_str("_00-00-00");
        LocalParts::from_stamp(&stamp)
    }

    /// Parse `%Y-%m-%d_%H-%M-%S`, the segment-name format every recording file on disk uses.
    ///
    /// This is how the maintenance pass learns when a `.mp4` covers without opening it, and how
    /// the UI resolves a `videofile_name` back to the wall-clock origin of its timeline.
    pub fn from_stamp(text: &str) -> Option<LocalParts> {
        let digits: Vec<u8> = text.as_bytes().iter().copied().take(19).collect();
        if digits.len() < 19 {
            return None;
        }
        let at = |i: usize| digits[i];
        if !(at(4) == b'-' && at(7) == b'-' && at(10) == b'_' && at(13) == b'-' && at(16) == b'-') {
            return None;
        }
        let num = |a: usize, b: usize| -> Option<u32> {
            std::str::from_utf8(&digits[a..b]).ok()?.parse().ok()
        };
        let parts = LocalParts {
            year: i64::from(num(0, 4)?),
            month: num(5, 7)?,
            day: num(8, 10)?,
            hour: num(11, 13)?,
            minute: num(14, 16)?,
            second: num(17, 19)?,
        };
        if parts.month < 1
            || parts.month > 12
            || parts.day < 1
            || parts.day > days_in_month(parts.year, parts.month)
            || parts.hour > 23
            || parts.minute > 59
            || parts.second > 59
        {
            return None;
        }
        Some(parts)
    }
}

/// Inverse of [`days_from_civil`], so a stored `videofile_time` can be rendered as a date.
fn civil_from_days(z: i64) -> (i64, i32, i32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as i32, d as i32)
}

/// Days in a Gregorian month; `month` is 1-based.
pub fn days_in_month(year: i64, month: u32) -> u32 {
    const LEN: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let feb = matches!(month, 2) && (year % 4 == 0 && (year % 100 != 0 || year % 400 == 0));
    if feb {
        29
    } else {
        LEN[(month.saturating_sub(1) % 12) as usize]
    }
}

impl LocalParts {
    /// Rebuild calendar fields from a value produced by [`LocalParts::naive_epoch_seconds`].
    pub fn from_naive_epoch(secs: i64) -> LocalParts {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        LocalParts {
            year,
            month: month as u32,
            day: day as u32,
            hour: (rem / 3_600) as u32,
            minute: ((rem % 3_600) / 60) as u32,
            second: (rem % 60) as u32,
        }
    }

    /// The calendar date this instant falls on, ignoring the time of day.
    pub fn date_only(&self) -> LocalParts {
        LocalParts { hour: 0, minute: 0, second: 0, ..*self }
    }
}

/// One "day" as the product counts it: `[day 03:00:00, next day 02:59:59]` when
/// `day_begin_minutes` is 180, and it rolls over a month boundary the way
/// `utils.get_datetime_in_day_range_pole_by_config_day_begin` does.
///
/// Both ends are inclusive, in the naive-local epoch the database stores.
pub fn day_bounds(year: i64, month: u32, day: u32, day_begin_minutes: i64) -> (i64, i64) {
    let midnight = |y: i64, m: u32, d: u32| {
        LocalParts { year: y, month: m, day: d, hour: 0, minute: 0, second: 0 }.naive_epoch_seconds()
    };
    let day_start = midnight(year, month, day);
    let (ny, nm, nd) = next_day(year, month, day);
    (
        day_start + day_begin_minutes * 60,
        midnight(ny, nm, nd) + day_begin_minutes * 60 - 1,
    )
}

/// The calendar day after a given date, crossing month and year boundaries.
pub fn next_day(year: i64, month: u32, day: u32) -> (i64, u32, u32) {
    if day < days_in_month(year, month) {
        return (year, month, day + 1);
    }
    if month < 12 {
        (year, month + 1, 1)
    } else {
        (year + 1, 1, 1)
    }
}

/// Render a duration of seconds as `H:MM:SS`, the format the search result's "locate" column uses.
pub fn seconds_to_hhmmss(total: i64) -> String {
    let sign = if total < 0 { "-" } else { "" };
    let t = total.abs();
    format!("{sign}{}:{:02}:{:02}", t / 3_600, (t % 3_600) / 60, t % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values computed independently with Python's `datetime.date` arithmetic.
    #[test]
    fn days_from_civil_matches_known_epoch_anchors() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2024, 3, 1), 19_783);
        assert_eq!(days_from_civil(2023, 3, 1), 19_417);
        assert_eq!(days_from_civil(2026, 9, 22), 20_718);
    }

    #[test]
    fn leap_day_is_counted() {
        assert_eq!(days_from_civil(2024, 3, 1) - days_from_civil(2024, 2, 28), 2);
        assert_eq!(days_from_civil(2023, 3, 1) - days_from_civil(2023, 2, 28), 1);
    }

    #[test]
    fn naive_epoch_seconds_is_the_stamp_interpreted_as_utc() {
        let p = LocalParts { year: 2026, month: 9, day: 22, hour: 12, minute: 0, second: 0 };
        assert_eq!(p.naive_epoch_seconds(), days_from_civil(2026, 9, 22) * 86_400 + 43_200);
    }

    #[test]
    fn stamps_use_the_upstream_format() {
        let p = LocalParts { year: 2026, month: 9, day: 22, hour: 1, minute: 2, second: 3 };
        assert_eq!(p.stamp(), "2026-09-22_01-02-03");
    }

    #[test]
    fn now_is_plausible_on_this_machine() {
        let p = now();
        assert!((2024..2100).contains(&p.year), "year read as {}", p.year);
        assert!((1..=12).contains(&p.month));
        assert!(p.naive_epoch_seconds() > 1_700_000_000);
    }

    /// The round trip is what lets the UI render a stored row without a timezone library.
    #[test]
    fn naive_epoch_round_trips() {
        for stamp in [
            "2026-09-21_21-16-12",
            "2024-02-29_23-59-59",
            "2026-01-01_00-00-00",
            "2026-12-31_03-04-05",
        ] {
            let parsed = LocalParts::from_stamp(stamp).expect("parses");
            assert_eq!(parsed.stamp(), stamp);
            let back = LocalParts::from_naive_epoch(parsed.naive_epoch_seconds());
            assert_eq!(back, parsed, "round trip failed for {stamp}");
        }
    }

    /// 1790025372 is the value the live database holds for the row named
    /// 2026-09-21_21-16-12.mp4; matching it byte for byte is the compatibility contract.
    #[test]
    fn matches_the_epoch_the_production_database_stores() {
        let p = LocalParts::from_stamp("2026-09-21_21-16-12").unwrap();
        assert_eq!(p.naive_epoch_seconds(), 1_790_025_372);
    }

    #[test]
    fn stamps_outside_the_calendar_are_rejected() {
        assert!(LocalParts::from_stamp("2026-13-01_00-00-00").is_none());
        assert!(LocalParts::from_stamp("2025-02-29_00-00-00").is_none());
        assert!(LocalParts::from_stamp("2024-02-29_00-00-00").is_some());
        assert!(LocalParts::from_stamp("2026-09-21 21:16:12").is_none());
        assert!(LocalParts::from_stamp("short").is_none());
    }

    #[test]
    fn next_day_crosses_month_and_year() {
        assert_eq!(next_day(2026, 9, 21), (2026, 9, 22));
        assert_eq!(next_day(2026, 9, 30), (2026, 10, 1));
        assert_eq!(next_day(2026, 12, 31), (2027, 1, 1));
        assert_eq!(next_day(2024, 2, 28), (2024, 2, 29));
        assert_eq!(next_day(2026, 2, 28), (2026, 3, 1));
    }

    /// With the shipped default of 03:00 the day runs 03:00:00 .. 02:59:59 next-day, inclusive,
    /// and a 00:00 setting degenerates to the calendar day exactly as Python does.
    #[test]
    fn day_bounds_follow_day_begin_minutes() {
        let (s, e) = day_bounds(2026, 9, 21, 180);
        assert_eq!(s, LocalParts::from_stamp("2026-09-21_03-00-00").unwrap().naive_epoch_seconds());
        assert_eq!(e, LocalParts::from_stamp("2026-09-22_02-59-59").unwrap().naive_epoch_seconds());
        let (s0, e0) = day_bounds(2026, 9, 21, 0);
        assert_eq!(s0, LocalParts::from_stamp("2026-09-21_00-00-00").unwrap().naive_epoch_seconds());
        assert_eq!(e0, LocalParts::from_stamp("2026-09-21_23-59-59").unwrap().naive_epoch_seconds());
        let (ms, me) = day_bounds(2026, 12, 31, 180);
        assert_eq!(me, LocalParts::from_stamp("2027-01-01_02-59-59").unwrap().naive_epoch_seconds());
        assert!(me > ms);
    }

    #[test]
    fn durations_render_for_the_locate_column() {
        assert_eq!(seconds_to_hhmmss(0), "0:00:00");
        assert_eq!(seconds_to_hhmmss(3_723), "1:02:03");
        assert_eq!(seconds_to_hhmmss(-61), "-0:01:01");
    }

    #[test]
    fn a_bare_date_is_midnight_of_that_day() {
        assert_eq!(
            LocalParts::from_date("2026-09-21"),
            Some(LocalParts { year: 2026, month: 9, day: 21, hour: 0, minute: 0, second: 0 })
        );
        assert_eq!(LocalParts::from_date("2026-02-30"), None, "the calendar must not be negotiated with");
        assert_eq!(LocalParts::from_date("2024-02-29").map(|p| p.day), Some(29));
        assert_eq!(LocalParts::from_date(" 2026-12-31 ").map(|p| p.month), Some(12));
        assert_eq!(LocalParts::from_date("2026-9-1"), None, "unpadded months are not our format");
    }

    #[test]
    fn a_time_only_label_is_the_tail_of_the_full_display() {
        let p = LocalParts { year: 2026, month: 9, day: 21, hour: 9, minute: 5, second: 3 };
        assert_eq!(p.time_display(), "09:05:03");
        assert_eq!(p.display(), "2026-09-21 09:05:03");
    }
}
