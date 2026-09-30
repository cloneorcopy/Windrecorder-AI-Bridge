//! Every timestamp the bridge produces, and the axis it sits on.
//!
//! `videofile_time` is not a POSIX timestamp. It is a *naive-local* epoch — the wall clock read off
//! `GetLocalTime` and interpreted as if it were UTC — because that is how `utils.dtstr_to_seconds`
//! built the installed base (`wind_base::clock` documents it, and its test quotes the real value
//! `1790025372`). On this UTC+8 machine the stored number is therefore exactly `true_unix + 28800`.
//!
//! That distinction is the single easiest way to lose a user's whole afternoon. An AI client handed
//! a bare `1790025372` will feed it to a POSIX converter and get 13:16; a client handed a bare
//! `"2026-09-21 21:16:12"` has no way to know whether to treat it as local or as UTC, and one of
//! those two guesses is eight hours wrong. So nothing here emits a bare anything:
//!
//!   * a rendered instant is always full ISO-8601 **with an explicit numeric UTC offset**, never
//!     `Z` and never a naked local time, so the instant it denotes is unambiguous;
//!   * the integer that survives a round trip is the *stored* value, which a client is only ever
//!     meant to hand back untouched, and the tool descriptions say so in as many words;
//!   * a parsed input that arrives with its own offset (`...T13:16:12Z`) is converted, not
//!     truncated, so the eight hours is never silently dropped on the way in either.
//!
//! The offset is measured, not assumed: the difference between what the OS clock says and what the
//! wall clock says *is* the offset, which is how this module stays correct without a timezone
//! database and without the `chrono` that is not in the workspace lock.

use wind_base::clock::LocalParts;

/// Seconds to add to a POSIX timestamp to get the value the index stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Axis {
    pub utc_offset_seconds: i64,
}

impl Axis {
    /// Measure the offset from the two clocks the platform already gives us.
    ///
    /// Both reads are of the same instant, seconds apart at worst, and a whole-hour offset cannot
    /// be confused by a one-second skew. Deriving it beats embedding a zone name: the recorder
    /// writes naive-local seconds on the machine it runs on, so "whatever this machine's wall clock
    /// is ahead of UTC by" is not an approximation of the rule, it *is* the rule.
    pub fn measure() -> Axis {
        let posix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        Axis { utc_offset_seconds: wind_base::clock::now().naive_epoch_seconds() - posix }
    }

    /// The ISO-8601 rendering of a stored value, with its offset spelled out.
    pub fn render(&self, stored: i64) -> String {
        let when = LocalParts::from_naive_epoch(stored);
        format!("{}{}", when.display().replace(' ', "T"), self.offset_literal())
    }

    /// Just the calendar date, which is what a day is addressed by. No offset: `date` names a day,
    /// and `2026-09-21+08:00` is not a thing an agent can be asked to compare usefully.
    pub fn render_date(&self, stored: i64) -> String {
        LocalParts::from_naive_epoch(stored).date_stamp()
    }

    pub fn offset_literal(&self) -> String {
        if self.utc_offset_seconds == 0 {
            return "Z".to_string();
        }
        let sign = if self.utc_offset_seconds < 0 { '-' } else { '+' };
        let magnitude = self.utc_offset_seconds.abs();
        let (h, m, s) = (magnitude / 3600, (magnitude % 3600) / 60, magnitude % 60);
        if s == 0 {
            format!("{sign}{h:02}:{m:02}")
        } else {
            // A zone with a seconds component is rare and historical, but emitting a wrong offset
            // to keep the string tidy is the exact failure this module exists to prevent.
            format!("{sign}{h:02}:{m:02}:{s:02}")
        }
    }

    /// POSIX seconds for a stored value, for the callers that want a real timestamp.
    pub fn to_posix(&self, stored: i64) -> i64 {
        stored - self.utc_offset_seconds
    }

    /// Parse one agent-supplied time into the stored axis.
    ///
    /// Three shapes are accepted, because all three arrive in practice:
    ///   * an integer, or a string of digits — taken as an already-stored value, passed through
    ///     untouched. This is the hand-off path: `around` and `frame` are given the `timestamp` a
    ///     search returned, and re-basing it would move the moment by the whole offset;
    ///   * a naive date or datetime — taken as the *wall clock*, which is what a stored value reads
    ///     as, so `2026-09-21 21:16:12` selects the row whose stored value renders as itself;
    ///   * an ISO-8601 datetime carrying a `Z` or a numeric offset — converted through POSIX, so a
    ///     client that sends UTC gets the instant it meant rather than one eight hours adrift.
    pub fn parse(&self, text: &str) -> Result<i64, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err(self.usage());
        }
        if let Ok(digits) = text.parse::<i64>() {
            return Ok(digits);
        }
        let normalized = text.replace('/', "-");
        let body = normalized.trim_end();

        if let Some((clock, given)) = split_offset(body) {
            let naive = parse_naive(clock).ok_or_else(|| self.usage())?;
            // An explicit offset says what wall clock it is relative to, so the absolute instant is
            // `naive - given`; adding the machine's own offset puts it back on the stored axis.
            return Ok(naive - given + self.utc_offset_seconds);
        }
        if body.len() == 10 {
            let day = LocalParts::from_date(body).ok_or_else(|| self.usage())?;
            return Ok(day.naive_epoch_seconds());
        }
        parse_naive(body).ok_or_else(|| self.usage())
    }

    /// The error an agent gets, which has to teach it the format rather than reject it.
    pub fn usage(&self) -> String {
        format!(
            "unrecognized time; use '2026-09-20', '2026-09-20 14:30:00', or an ISO-8601 datetime \
             with an offset such as '2026-09-20T14:30:00{}'. A bare number is taken as a stored \
             timestamp, which is the `timestamp` field of an earlier result passed back unchanged.",
            self.offset_literal()
        )
    }
}

/// `%Y-%m-%d`, optionally followed by `%H:%M:%S` or `%H:%M`, in either the `T` or the space form,
/// plus the `%Y-%m-%d_%H-%M-%S` spelling every filename on this disk uses.
fn parse_naive(text: &str) -> Option<i64> {
    // The segment-name form is length-exact on purpose: `LocalParts::from_stamp` reads the first 19
    // bytes and ignores the rest, so handing it a longer string would accept `..._21-16-12.mp4`.
    if text.len() == 19 && text.as_bytes()[10] == b'_' {
        return LocalParts::from_stamp(text).map(|p| p.naive_epoch_seconds());
    }
    let mut stamp = String::with_capacity(19);
    let date = text.get(..10)?;
    stamp.push_str(date);
    match text.len() {
        10 => stamp.push_str("_00-00-00"),
        16 | 19 => {
            let sep = text.as_bytes()[10] as char;
            if !matches!(sep, 'T' | ' ') {
                return None;
            }
            let tail = &text[11..];
            let mut fields = tail.split(':');
            let hour = fields.next()?.parse::<u32>().ok()?;
            let minute = fields.next()?.parse::<u32>().ok()?;
            let second = match fields.next() {
                Some(s) => s.parse::<u32>().ok()?,
                None => 0,
            };
            stamp.push('_');
            stamp.push_str(&format!("{hour:02}-{minute:02}-{second:02}"));
        }
        _ => return None,
    }
    LocalParts::from_stamp(&stamp).map(|p| p.naive_epoch_seconds())
}

/// Strip a trailing `Z` or `+hh:mm` / `-hh:mm` (seconds optional), returning the bare clock
/// reading and the offset it was written against.
fn split_offset(text: &str) -> Option<(&str, i64)> {
    if text.ends_with('Z') || text.ends_with('z') {
        return Some((&text[..text.len() - 1], 0));
    }
    // Only a sign at a position the clock part cannot reach: `2026-09-20 14:30:00` has its first
    // sign at index 4 inside the date, so the search starts after the date and the space or `T`.
    let at = (11..text.len()).find(|&i| matches!(text.as_bytes()[i], b'+' | b'-'))?;
    let (sign, digits) = match text.as_bytes()[at] {
        b'-' => (-1i64, &text[at + 1..]),
        _ => (1i64, &text[at + 1..]),
    };
    let mut parts = digits.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next().unwrap_or("0").parse().ok()?;
    let second: i64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((&text[..at], sign * (hour * 3600 + minute * 60 + second)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UTC+8 the development and production machines both run on.
    fn plus_eight() -> Axis {
        Axis { utc_offset_seconds: 28_800 }
    }

    /// 1790025372 is the value a live database holds for the row named 2026-09-21_21-16-12.
    #[test]
    fn a_stored_value_renders_as_the_wall_clock_it_was_taken_from() {
        let axis = plus_eight();
        assert_eq!(axis.render(1_790_025_372), "2026-09-21T21:16:12+08:00");
        // Same instant on the POSIX axis, which is 28800 lower and 13:16 in UTC.
        assert_eq!(axis.to_posix(1_790_025_372), 1_789_996_572);
    }

    /// The failure the contract names: a client that sees no offset assumes UTC and shifts by eight.
    #[test]
    fn an_rendered_instant_always_names_its_offset() {
        let axis = plus_eight();
        let text = axis.render(1_790_025_372);
        assert!(text.ends_with("+08:00"), "{text} says nothing about which axis it is on");
        assert!(!text.ends_with('Z'));
        assert_eq!(axis.render_date(1_790_025_372), "2026-09-21");
        // A zone that is not a whole hour still says something true.
        assert_eq!(Axis { utc_offset_seconds: 19_800 }.offset_literal(), "+05:30");
        assert_eq!(Axis { utc_offset_seconds: -3600 }.offset_literal(), "-01:00");
        assert_eq!(Axis { utc_offset_seconds: 0 }.offset_literal(), "Z");
    }

    #[test]
    fn a_round_trip_through_parse_returns_the_same_instant() {
        let axis = plus_eight();
        for stored in [1_790_025_372i64, 1_789_996_572, 1_800_000_000] {
            let rendered = axis.render(stored);
            assert_eq!(axis.parse(&rendered).unwrap(), stored, "round trip broke on {rendered}");
            assert_eq!(axis.parse(&stored.to_string()).unwrap(), stored, "a passed-through integer");
        }
    }

    #[test]
    fn a_wall_clock_string_selects_the_row_that_displays_as_itself() {
        let axis = plus_eight();
        let expected = 1_790_025_372;
        for input in [
            "2026-09-21 21:16:12",
            "2026-09-21T21:16:12",
            "2026-09-21_21-16-12",
            "  2026-09-21 21:16:12  ",
        ] {
            assert_eq!(axis.parse(input).unwrap(), expected, "for input {input:?}");
        }
        // A time given to the minute means the whole minute, so the seconds are zero — and the
        // test says so explicitly rather than letting a copy-paste of `expected` hide it.
        assert_eq!(axis.parse("2026-09-21 21:16").unwrap(), expected - 12);
        assert_eq!(axis.parse("2026-09-21").unwrap(), expected - expected % 86_400);
        assert_eq!(axis.parse("2026/09/21 21:16:12").unwrap(), expected, "the upstream slash form");
    }

    /// A client that sends UTC means UTC. Honouring that is the difference between the row the
    /// agent asked for and a row eight hours away that may not exist.
    #[test]
    fn an_incoming_offset_is_converted_not_truncated() {
        let axis = plus_eight();
        let expected = 1_790_025_372;
        assert_eq!(axis.parse("2026-09-21T13:16:12Z").unwrap(), expected);
        assert_eq!(axis.parse("2026-09-21T13:16:12+00:00").unwrap(), expected);
        assert_eq!(axis.parse("2026-09-21T21:16:12+08:00").unwrap(), expected);
        assert_eq!(axis.parse("2026-09-21T06:16:12-07:00").unwrap(), expected);
    }

    #[test]
    fn an_unparsable_time_teaches_the_format_instead_of_just_failing() {
        let axis = plus_eight();
        for bad in ["yesterday", "today", "", "   ", "2026-13-01 00:00:00", "2026-02-30", "2026-9-1", "14:30"] {
            let message = axis.parse(bad).unwrap_err();
            assert!(message.contains("2026-09-20"), "{bad:?} produced a message with no example: {message}");
        }
    }

    #[test]
    fn the_measured_offset_agrees_with_the_wall_clock_that_writes_the_index() {
        let axis = Axis::measure();
        // Whatever zone this machine is in, the two clocks must disagree by the offset the stored
        // epoch is defined by — a mismatch here means the recorder and this module disagree about
        // what `videofile_time` means, which is the whole bug class.
        let now_stored = wind_base::clock::now().naive_epoch_seconds();
        let posix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert_eq!(axis.utc_offset_seconds, now_stored - posix);
        assert_eq!(axis.parse(&wind_base::clock::now().display().replace(' ', "T")).unwrap(), now_stored);
    }

    #[test]
    fn offset_splitting_does_not_mistake_a_date_hyphen_for_a_sign() {
        assert_eq!(split_offset("2026-09-21T21:16:12"), None);
        assert_eq!(split_offset("2026-09-21"), None);
        assert_eq!(split_offset("2026-09-21T21:16:12+08:00"), Some(("2026-09-21T21:16:12", 28_800)));
        assert_eq!(split_offset("2026-09-21T21:16:12Z"), Some(("2026-09-21T21:16:12", 0)));
    }
}
