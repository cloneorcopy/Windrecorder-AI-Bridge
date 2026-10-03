//! The window-title side channel, read back.
//!
//! The recorder's titles arrive in `cache/win_title/{YYYY-MM-DD}.csv`, written by their own thread
//! whenever the foreground window *changes*. Indexing an existing video therefore has to replay that
//! join: the frame's computed second selects the title that was in force at that instant. Getting the
//! selection rule wrong mislabels which application a screen belonged to, and the title is also part
//! of the searchable text.
//!
//! The rule reproduced here is `record_wintitle.get_wintitle_or_deep_linking_by_timestamp`, including
//! the two things about it that look like bugs and are load-bearing: a timestamp *before* the first
//! logged change returns that first change, and a timestamp *after* the last one returns nothing at
//! all.

use std::path::{Path, PathBuf};

use wind_base::clock::LocalParts;

use crate::csvread;

/// The columns of the day file, by name rather than position — a title can contain a comma, so the
/// parse has to be the quoted one `wind_base::csv` implements.
const COL_DATETIME: usize = 0;
const COL_TITLE: usize = 1;
const COL_DEEP_LINKING: usize = 2;

/// The header the writer emits.
pub const HEADER: [&str; 3] = ["datetime", "window_title", "deep_linking"];

/// One logged foreground-window change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// In the app's naive-local epoch, the same scale as `videofile_time`.
    pub at: i64,
    pub window_title: String,
    pub deep_linking: String,
}

/// A day's titles, in the order they were written.
#[derive(Debug, Default, Clone)]
pub struct TitleTable {
    entries: Vec<Entry>,
}

impl TitleTable {
    pub fn empty() -> TitleTable {
        TitleTable::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read `{dir}/{YYYY-MM-DD}.csv` for the day `at` falls in.
    ///
    /// A missing file is normal and means "no titles were logged that day", which yields `None` from
    /// every lookup rather than an error — upstream returns `None` for exactly this case.
    pub fn load_day(dir: &Path, at: i64) -> TitleTable {
        let day = LocalParts::from_naive_epoch(at);
        let path = dir.join(format!("{}.csv", day.date_stamp()));
        TitleTable::load_csv(&path)
    }

    /// Read one day's file. The parse is [`crate::csvread`]'s, not `wind_base::csv`'s, because a Chinese
    /// window title read byte-wise arrives as Latin-1 and would be stored wrong forever.
    pub fn load_csv(path: &Path) -> TitleTable {
        TitleTable::from_rows(&csvread::read(path, true))
    }

    /// Parse CSV records, skipping any row whose timestamp is not readable.
    ///
    /// Upstream hands the whole column to `pd.to_datetime`, which fails the *file* on one bad row.
    /// Dropping only the bad row keeps a day's history available; the interval structure is then
    /// built from what is left, which is the more useful of the two behaviours and no more wrong.
    pub fn from_rows(rows: &[Vec<String>]) -> TitleTable {
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() < 2 {
                continue;
            }
            let Some(at) = parse_display_timestamp(&row[COL_DATETIME]) else { continue };
            entries.push(Entry {
                at,
                window_title: row.get(COL_TITLE).cloned().unwrap_or_default(),
                deep_linking: row.get(COL_DEEP_LINKING).cloned().unwrap_or_default(),
            });
        }
        // The writer appends in real time, so the file is already ordered; sorting makes a restored or
        // hand-edited day behave the same as a fresh one instead of inverting its intervals.
        entries.sort_by_key(|e| e.at);
        TitleTable { entries }
    }

    /// The entry in force at `at`, projected through `column`.
    fn lookup(&self, at: i64, column: Column) -> Option<&str> {
        let first = self.entries.first()?;
        // `i == 0 and target_time <= df.loc[i]`: a timestamp before or on the first change takes it.
        if at <= first.at {
            return Some(field(first, column));
        }
        for i in 0..self.entries.len() {
            let current = &self.entries[i];
            let Some(next) = self.entries.get(i + 1) else {
                // The last row: upstream evaluates `target < df.loc[i + 1]` here, takes a KeyError,
                // aborts the loop and returns nothing. A title after the final logged change is a
                // title the thread never saw, so "unknown" is the honest answer.
                return None;
            };
            if at >= current.at && at < next.at {
                // "如果离后边记录的时间超过1s，则取上一个的记录" — within one second of the *next*
                // change, the next change wins, because the frame was almost certainly captured
                // after the user already switched.
                return Some(field(if next.at - at < 1 { next } else { current }, column));
            }
        }
        None
    }

    /// The window title in force at `at`.
    pub fn title_at(&self, at: i64) -> Option<&str> {
        self.lookup(at, Column::Title).filter(|t| !t.trim().is_empty() && *t != "None")
    }

    /// The deep link in force at `at`, which the browser extension writes beside the title.
    pub fn deep_linking_at(&self, at: i64) -> Option<&str> {
        self.lookup(at, Column::DeepLinking).filter(|d| !d.trim().is_empty() && *d != "None")
    }
}

#[derive(Clone, Copy)]
enum Column {
    Title,
    DeepLinking,
}

fn field<'a>(entry: &'a Entry, column: Column) -> &'a str {
    match column {
        Column::Title => &entry.window_title,
        Column::DeepLinking => &entry.deep_linking,
    }
}

/// `%Y-%m-%d %H:%M:%S`, the CSV's own format.
///
/// `LocalParts::from_stamp` parses the *filename* format, which separates the date from the time with
/// `_` and the time fields with `-`; this is the same instant spelled with a space and colons, because
/// one string was a filename and the other is a column pandas wrote. Rewriting the four positions that
/// differ lets both spellings share one validator rather than one second calendar.
pub fn parse_display_timestamp(text: &str) -> Option<i64> {
    let chars: Vec<char> = text.trim().chars().collect();
    if chars.len() < 19 || chars[10] != ' ' || chars[13] != ':' || chars[16] != ':' {
        return None;
    }
    let mut stamp: String = chars[..19].iter().collect();
    stamp.replace_range(10..11, "_");
    stamp.replace_range(13..14, "-");
    stamp.replace_range(16..17, "-");
    LocalParts::from_stamp(&stamp).map(|p| p.naive_epoch_seconds())
}

/// `record_wintitle.optimize_wintitle_name`: strip the decoration browsers and chat clients hang on
/// their titles, so "(12) Home / X.com" indexes as "Home / X.com".
///
/// The order of the substitutions is upstream's and it is not commutative — the leading `(3)` is only
/// removable by the anchored rule *before* the bare `(\d+)` rule runs, and the bare rule runs after the
/// asterisk rules, so a reordered pipeline yields a different string in a user's index.
pub fn optimize_window_title(text: &str) -> String {
    let mut text = text.to_string();
    // Telegram: "(1) 大懒趴俱乐部 – (283859)" -> the conversation name only.
    text = drop_numbered_paren_after(&text, " – ");
    text = drop_numbered_paren_after(&text, " - ");
    text = strip_leading_badge(&text);

    // Microsoft Edge: "XXXX and 64 more pages - Personal - Microsoft Edge".
    text = drop_more_pages(&text);
    text = text.replace(" - Personal", "");

    // A document's unsaved-state asterisk, in any of the three positions it can sit.
    text = deasterisk(&text);
    text = drop_bare_numbered_paren(&text);
    text = text.trim().to_string();
    deasterisk(&text)
}

/// `re.sub(" – \\(\\d+\\)", "", text)` and its `" - "` twin: a separator immediately followed by a
/// parenthesised integer, removed whole, at every non-overlapping position.
fn drop_numbered_paren_after(text: &str, separator: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let sep: Vec<char> = separator.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i..].starts_with(sep.as_slice()) {
            if let Some(end) = paren_digits(&chars, i + sep.len()) {
                i = end; // neither the separator nor the badge is emitted
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `re.sub("\\(\\d+\\)", "", text)` — the same badge with no separator to anchor it.
fn drop_bare_numbered_paren(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' {
            if let Some(end) = paren_digits(&chars, i) {
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// If `(digits)` starts at `from`, the index just past its closing parenthesis.
fn paren_digits(chars: &[char], from: usize) -> Option<usize> {
    if chars.get(from) != Some(&'(') {
        return None;
    }
    let mut j = from + 1;
    while j < chars.len() && chars[j].is_ascii_digit() {
        j += 1;
    }
    (j > from + 1 && chars.get(j) == Some(&')')).then_some(j + 1)
}

/// `re.sub("^\\(\\d+\\) ", "", text)` — anchored, and it requires the trailing space.
fn strip_leading_badge(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    match paren_digits(&chars, 0) {
        Some(end) if chars.get(end) == Some(&' ') => chars[end + 1..].iter().collect(),
        _ => text.to_string(),
    }
}

/// `re.sub(" and \\d+ more pages", "", text)`.
fn drop_more_pages(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let head: Vec<char> = " and ".chars().collect();
    let tail: Vec<char> = " more pages".chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i..].starts_with(head.as_slice()) {
            let mut j = i + head.len();
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + head.len() && chars[j..].starts_with(tail.as_slice()) {
                i = j + tail.len();
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The three asterisk rules, in upstream's order. `str::replace` is non-overlapping and left-to-right,
/// which is what `re.sub` is too.
fn deasterisk(text: &str) -> String {
    text.replace(" * ", " ").replace(" *", " ").replace("* ", " ")
}

/// The day file the writer would use for this instant.
pub fn path_for(dir: &Path, at: i64) -> PathBuf {
    dir.join(format!("{}.csv", LocalParts::from_naive_epoch(at).date_stamp()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Either spelling of an instant: the CSV's `%Y-%m-%d %H:%M:%S` or the filename's `%Y-%m-%d_%H-%M-%S`.
    /// Accepting both keeps the tests readable in whichever format the assertion is about.
    fn epoch(text: &str) -> i64 {
        parse_display_timestamp(text)
            .or_else(|| LocalParts::from_stamp(text).map(|p| p.naive_epoch_seconds()))
            .unwrap_or_else(|| panic!("{text} is not an instant in either format"))
    }

    fn table(spec: &[(&str, &str)]) -> TitleTable {
        TitleTable::from_rows(
            &spec
                .iter()
                .map(|(t, title)| vec![(*t).to_string(), (*title).to_string(), String::new()])
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn the_csv_timestamp_format_is_the_filename_format_with_a_space() {
        assert_eq!(parse_display_timestamp("2026-09-21 21:16:12"), Some(epoch("2026-09-21_21-16-12")));
        assert_eq!(parse_display_timestamp("2026-09-21_21-16-12"), None, "the wrong separator is not tolerated");
        assert_eq!(parse_display_timestamp("2026-09-21 21:16"), None, "too short to be a timestamp");
        assert_eq!(parse_display_timestamp("nonsense"), None);
    }

    #[test]
    fn a_timestamp_before_the_first_change_takes_it() {
        let t = table(&[("2026-09-21 10:00:00", "Excel")]);
        assert_eq!(t.title_at(epoch("2026-09-21 09:00:00")), Some("Excel"));
        assert_eq!(t.title_at(epoch("2026-09-21 10:00:00")), Some("Excel"), "on the second, inclusive");
    }

    #[test]
    fn between_two_changes_the_earlier_one_wins() {
        let t = table(&[("2026-09-21 10:00:00", "Excel"), ("2026-09-21 10:05:00", "Chrome")]);
        assert_eq!(t.title_at(epoch("2026-09-21 10:02:00")), Some("Excel"));
        assert_eq!(t.title_at(epoch("2026-09-21 10:04:59")), Some("Excel"), "a full second before the next change");
    }

    /// Upstream's lookahead — "if the timestamp is less than a second from the *next* record, take that
    /// one" — exists because a frame is grabbed on a different clock from the title poll and can land
    /// just after an alt-tab the log recorded a moment later.
    ///
    /// With whole-second timestamps the difference is either 0 or at least 1, and a difference of 0 means
    /// the target sits *on* the next entry, which the interval test already excludes — so the branch only
    /// fires when the CSV carries sub-second values, which pandas will write and the native writer does
    /// not. It is kept because dropping it would change the answer for a day logged by the Python app,
    /// and the test below pins the boundary it does control: a full second away is the earlier title.
    #[test]
    fn a_second_away_from_the_next_change_still_belongs_to_the_earlier_one() {
        let t = table(&[("2026-09-21 10:00:00", "Excel"), ("2026-09-21 10:00:02", "Chrome")]);
        assert_eq!(t.title_at(epoch("2026-09-21 10:00:01")), Some("Excel"), "exactly 1s before: the rule needs *less than* 1s");
        // On the entry itself the first branch of upstream's loop is what answers, and it answers with
        // the entry it is standing on.
        assert_eq!(t.title_at(epoch("2026-09-21 10:00:00")), Some("Excel"));
        // Landing *on* the final entry is the dead-third-branch case, not this one: see
        // `after_the_last_logged_change_there_is_no_title_to_report`.
        assert_eq!(t.title_at(epoch("2026-09-21 10:00:02")), None);
    }

    /// Upstream's loop reads `df.loc[i + 1]` on the final row, takes a `KeyError`, and the whole
    /// lookup returns nothing. A title after the last logged change was never observed, so an empty
    /// result is not a bug to fix — it is the difference between "no title" and "the wrong title".
    #[test]
    fn after_the_last_logged_change_there_is_no_title_to_report() {
        let t = table(&[("2026-09-21 10:00:00", "Excel"), ("2026-09-21 10:05:00", "Chrome")]);
        assert_eq!(t.title_at(epoch("2026-09-21 11:00:00")), None);
        assert_eq!(t.title_at(epoch("2026-09-21 10:05:00")), None);
    }

    #[test]
    fn an_empty_title_column_reads_as_no_title() {
        let t = table(&[("2026-09-21 10:00:00", ""), ("2026-09-21 10:05:00", "Chrome")]);
        assert_eq!(t.title_at(epoch("2026-09-21 10:04:59")), None, "never the empty string");
        assert_eq!(t.title_at(epoch("2026-09-21 10:02:00")), None);
        assert_eq!(t.title_at(epoch("2026-09-21 09:00:00")), None, "the first entry is blank");
    }

    #[test]
    fn an_empty_table_has_no_answers() {
        assert_eq!(TitleTable::empty().title_at(0), None);
        assert_eq!(TitleTable::empty().deep_linking_at(0), None);
        assert!(TitleTable::empty().is_empty());
    }

    #[test]
    fn deep_linking_is_a_separate_projection_of_the_same_row() {
        let rows = vec![
            vec!["2026-09-21 10:00:00".to_string(), "Chrome".to_string(), "https://example.com/x".to_string()],
        ];
        let t = TitleTable::from_rows(&rows);
        assert_eq!(t.title_at(epoch("2026-09-21 10:00:00")), Some("Chrome"));
        assert_eq!(t.deep_linking_at(epoch("2026-09-21 10:00:00")), Some("https://example.com/x"));
    }

    #[test]
    fn rows_out_of_order_are_read_as_a_timeline_not_as_a_scramble() {
        let t = table(&[("2026-09-21 10:05:00", "Chrome"), ("2026-09-21 10:00:00", "Excel")]);
        assert_eq!(t.len(), 2);
        assert_eq!(t.title_at(epoch("2026-09-21 10:02:00")), Some("Excel"));
    }

    #[test]
    fn a_stray_unparseable_row_does_not_lose_the_whole_day() {
        let t = TitleTable::from_rows(&[
            vec!["2026-09-21 10:00:00".into(), "Excel".into(), String::new()],
            vec!["not a time".into(), "Ghost".into(), String::new()],
            vec!["2026-09-21 10:10:00".into(), "Chrome".into(), String::new()],
        ]);
        assert_eq!(t.len(), 2);
        assert_eq!(t.title_at(epoch("2026-09-21 10:05:00")), Some("Excel"));
    }

    #[test]
    fn titles_are_read_from_the_day_file_named_for_the_instant() {
        let dir = std::env::temp_dir().join(format!("windcap-reindex-title-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Writing through `wind_base::csv` is correct — it emits the string's UTF-8 bytes — so this
        // doubles as a check that the two crates agree on the file format and only disagree on reading.
        wind_base::csv::append_row(&dir.join("2026-09-21.csv"), &HEADER, &["2026-09-21 21:16:12", "Notepad", ""]).unwrap();
        wind_base::csv::append_row(
            &dir.join("2026-09-21.csv"),
            &HEADER,
            &["2026-09-21 21:20:00", "(3) 项目讨论 – (283859)", ""],
        )
        .unwrap();
        // A third change is needed for the second to be *answerable*: a timestamp after the final
        // logged change returns nothing, which is upstream's behaviour and is tested elsewhere.
        wind_base::csv::append_row(&dir.join("2026-09-21.csv"), &HEADER, &["2026-09-21 21:30:00", "End", ""]).unwrap();
        let table = TitleTable::load_day(&dir, epoch("2026-09-21_21-16-12"));
        assert_eq!(table.title_at(epoch("2026-09-21_21-16-12")).map(str::to_string), Some("Notepad".into()));
        // The regression: a Chinese title read through a byte-wise parser arrives as Latin-1.
        assert_eq!(
            table.title_at(epoch("2026-09-21_21-20-30")).map(str::to_string),
            Some("(3) 项目讨论 – (283859)".into()),
            "a CJK title must arrive as characters, not as its own UTF-8 bytes"
        );
        assert_eq!(optimize_window_title(table.title_at(epoch("2026-09-21_21-20-30")).unwrap()), "项目讨论");
        // A day with no file is empty, not an error, and the path really is the day's own.
        assert!(TitleTable::load_day(&dir, epoch("2026-09-25_00-00-00")).is_empty());
        assert_eq!(path_for(&dir, epoch("2026-09-21_21-16-12")), dir.join("2026-09-21.csv"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unread_badges_are_stripped_in_upstreams_order() {
        assert_eq!(optimize_window_title("(1) 大懒趴俱乐部 – (283859)"), "大懒趴俱乐部");
        assert_eq!(optimize_window_title("(3) General - (12)"), "General");
        assert_eq!(optimize_window_title("Home / X.com"), "Home / X.com");
    }

    #[test]
    fn browser_and_editor_decoration_is_removed() {
        assert_eq!(
            optimize_window_title("Windrecorder and 64 more pages - Personal - Microsoft Edge"),
            "Windrecorder - Microsoft Edge"
        );
        assert_eq!(optimize_window_title("Blender* a.blend"), "Blender a.blend");
        assert_eq!(optimize_window_title("notes * - Notepad"), "notes - Notepad");
        assert_eq!(optimize_window_title("  padded  "), "padded");
        // A number that is not a badge survives.
        assert_eq!(optimize_window_title("Chapter 12 - Book"), "Chapter 12 - Book");
    }

    #[test]
    fn the_string_none_is_not_a_title() {
        // Upstream feeds a missing lookup through `str(None)`, which stores the literal word "None".
        let t = table(&[("2026-09-21 10:00:00", "None")]);
        assert_eq!(t.title_at(epoch("2026-09-21 10:00:00")), None);
    }
}
