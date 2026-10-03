//! Erasing what the index already holds, for a period the user names.
//!
//! The privacy mask is the door on the way in: it decides what never becomes searchable. This is the
//! door on the other side, for the text that already *is* searchable. Until now there was no such door:
//! the only `DELETE FROM video_text` statements in the workspace belonged to `wind-reindex` rebuilding
//! one segment and to [`wind_store::maintain`] moving a row, so "erase the nine days I was job-hunting"
//! had no answer at any level of the product — not the window, not the tray, not the CLI.
//!
//! Two ways to name the period, and it has to be named: `--day 2026-09-22`, or `--from` with `--to`,
//! every spelling resolved by [`wind_base::range`] so this command and `windcapctl query` cannot mean
//! different things by the same argument. A `forget` with no window would be one forgotten flag away
//! from erasing the library, so it is refused rather than defaulted — which is also why this module
//! calls [`wind_base::range::parse_date`] and friends directly instead of [`wind_base::range::resolve`],
//! whose last resort is "today".
//!
//! What one row loses: `ocr_text`, `win_title`, `deep_linking`, `thumbnail`. What this command cannot
//! reach, printed in its own report because a privacy command that overstates itself is worse than none:
//!
//!   * **the footage.** The video and any screenshot still in `cache_screenshot/` keep every pixel, and
//!     no `--and-delete-the-video` flag is offered: [`crate::expire`] owns media deletion, and one
//!     command that erased text *and* silently removed files is how a user loses the recording they
//!     meant to keep.
//!   * **the month backups.** `cache/db_backup/` holds the last few copies of each month file, taken
//!     before this erase, so a `windmaint backup` pass can put the text back. The report names how many
//!     copies the install keeps.
//!   * **the two regenerable AI caches.** `result_ai_extract_tag/{year}.json` and
//!     `result_ai_day_poem/{year}.json` were written *from* text that may now be gone; this command
//!     cannot tell which entries drew on the erased rows, so it leaves them and names `windai` as the
//!     way to regenerate them.
//!   * **the AI summaries, which it does erase.** `result_ai_period_summary/` and
//!     `result_ai_daily_summary/` hold paragraphs no binary here can produce again — some were written
//!     by an outside AI over MCP, from screen text this pass is blanking. Leaving them behind would mean
//!     the erased content surviving in prose, which is an erase in name only, so the affected stretches
//!     are dropped and a day left with no stretch standing loses its own summary too. A *partial*
//!     erasure drops the whole stretch's paragraph: a summary is not made safe by a note saying part of
//!     its source was removed, and no reader can tell which part.

use wind_base::config::Config;
use wind_base::range::{self, Origin};
use wind_store::{maintain, read, search::Query};

/// What one erase pass did, across the month files it opened.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub months: usize,
    /// Rows matched, whether or not they were written: under `--dry-run` this is the promise.
    pub rows_matched: i64,
    pub rows_erased: usize,
    /// Month files left with no searchable text at all.
    pub emptied: usize,
    /// Stretch summaries dropped because the text they were written from is being erased with them, and
    /// what that leaves for the days they belonged to.
    pub summaries: crate::summaries::Pruned,
}

/// The period to erase, after the arguments have been checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: i64,
    pub to: i64,
    /// Which rule produced it, so the report can say "the product day that begins 03:00" rather than
    /// leaving a user to work out why the bounds are not midnight.
    pub rule: &'static str,
}

impl Window {
    /// `--day YYYY-MM-DD`, widened to that product day by the same rule `windcapctl query --day` uses.
    pub fn from_day(text: &str, day_begin_minutes: i64) -> Result<Window, String> {
        let (y, m, d) = range::parse_date(text).ok_or_else(|| format!("invalid --day '{text}': expected YYYY-MM-DD"))?;
        let span = range::product_day(y, m, d, day_begin_minutes, Origin::Day);
        Ok(Window { from: span.from, to: span.to, rule: span.origin.label() })
    }

    /// `--from T --to T`, both ends required and neither defaulted.
    pub fn from_bounds(from: Option<&str>, to: Option<&str>) -> Result<Window, String> {
        let (Some(lo), Some(hi)) = (from, to) else {
            return Err(match (from, to) {
                (None, None) => "a period is required: give --day YYYY-MM-DD, or both --from and --to. \
                    A forget with no window would erase the whole library, and that is not a default to \
                    fall back to"
                    .to_string(),
                (None, _) => "--from is required when --to is given".to_string(),
                _ => "--to is required when --from is given".to_string(),
            });
        };
        let (Some(from), Some(to)) = (range::parse_instant(lo, false), range::parse_instant(hi, true)) else {
            return Err(format!("invalid --from '{lo}' / --to '{hi}': expected YYYY-MM-DD or YYYY-MM-DD_HH-MM-SS"));
        };
        if from > to {
            return Err(format!("--from '{lo}' is after --to '{hi}'"));
        }
        Ok(Window { from, to, rule: Origin::Explicit.label() })
    }

    /// The bounds in the wall time the rows are stored in, for the report line.
    fn label(&self) -> String {
        range::Span { from: self.from, to: self.to, origin: Origin::Explicit, day_begin_minutes: 0 }.label()
    }

    /// Does one month file's calendar coverage have any row that can be in this window?
    ///
    /// A file that cannot is skipped without being opened: `open_write` runs SQLite's schema ensure and
    /// takes a write lock, and two years of month files should not pay that for one afternoon in
    /// September.
    fn touches(&self, month: &read::Month) -> bool {
        let (from, to) = month.coverage();
        from <= self.to && to >= self.from
    }
}

/// The four period flags exactly as the user typed them, before the install's own day-start is applied.
///
/// Held as text rather than parsed at the CLI edge because `--day` cannot be widened without reading
/// `day_begin_minutes` from the config, and the config is not loaded until after the arguments check.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Args {
    pub day: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub keyword: Option<String>,
}

impl Args {
    /// The window to erase.
    pub fn window(&self, day_begin_minutes: i64) -> Result<Window, String> {
        if self.day.is_some() && (self.from.is_some() || self.to.is_some()) {
            return Err("--day names a whole product day and cannot be combined with --from/--to".to_string());
        }
        match &self.day {
            Some(day) => Window::from_day(day, day_begin_minutes),
            None => Window::from_bounds(self.from.as_deref(), self.to.as_deref()),
        }
    }

    /// The keyword, with a whitespace-only value treated as absent: `--keyword " "` would otherwise
    /// reach [`Query::with_keywords`] as no tokens, and no tokens means "every row in the window".
    pub fn keyword(&self) -> Option<&str> {
        self.keyword.as_deref().map(str::trim).filter(|word| !word.is_empty())
    }

    /// Whether nothing at all was given, which `--dry-run` users hit first.
    pub fn is_empty(&self) -> bool {
        self.day.is_none() && self.from.is_none() && self.to.is_none()
    }
}

/// Blank every matched row, month file by month file.
pub fn run(config: &Config, window: &Window, keyword: Option<&str>, dry_run: bool) -> Result<Outcome, String> {
    println!(
        "forget: erasing the index for {} ({} rule), {}",
        window.label(),
        window.rule,
        match keyword {
            Some(word) => format!("the rows matching '{word}'"),
            None => "every row in the window".to_string(),
        }
    );
    if dry_run {
        println!("        dry run: nothing is written");
    }

    let mut outcome = Outcome::default();
    let mut blanked: Vec<String> = Vec::new();
    for month in read::discover(&config.db_dir()) {
        if !window.touches(&month) {
            continue;
        }
        let label = month.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let conn = if dry_run {
            crate::refresh::open_read_only(&month.path).map_err(|e| format!("{label}: {e}"))?
        } else {
            month.open_write().map_err(|e| format!("{label}: {e}"))?
        };
        // One predicate for both the count and the write, so the dry run cannot promise a number the
        // real run then misses: the keyword form runs through the same `Query` the UI searches with, and
        // the keywordless form through the pair of statements that share one bound.
        let query = keyword.map(|word| Query::new(window.from, window.to).with_keywords(word));
        let matched = match &query {
            Some(query) => maintain::count_matching(&conn, query).map_err(|e| e.to_string())?,
            None => maintain::count_window(&conn, window.from, window.to).map_err(|e| e.to_string())?,
        };
        outcome.months += 1;
        outcome.rows_matched += matched;
        // Which stretches are about to lose their text, collected before the write rather than after:
        // once the rows are blanked there is nothing left to ask, and a summary whose source has gone
        // without being named would survive the erase. Read-only either way, so the dry run collects the
        // same list and reports what it *would* reach for rather than a zero.
        let touching: Vec<String> = match &query {
            Some(query) => wind_store::search::search(&conn, query)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|row| row.videofile_name)
                .collect(),
            None => read::rows_in_window(&conn, Some(window.from), Some(window.to))
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|row| row.videofile_name)
                .collect(),
        };
        blanked.extend(touching);
        if dry_run {
            println!("  {label}: would blank {matched} row(s)");
            continue;
        }
        let erased = match &query {
            Some(query) => maintain::erase_matching(&conn, query).map_err(|e| e.to_string())?,
            None => maintain::erase_window(&conn, window.from, window.to).map_err(|e| e.to_string())?,
        };
        outcome.rows_erased += erased;
        let left = maintain::count_with_text(&conn).map_err(|e| e.to_string())?;
        if left == 0 {
            outcome.emptied += 1;
        }
        println!("  {label}: blanked {erased} row(s), {left} row(s) in this file still carry text");
    }

    outcome.summaries = if blanked.is_empty() {
        crate::summaries::Pruned::default()
    } else if dry_run {
        crate::summaries::plan(config, &blanked, crate::summaries::Aftermath::Erase)
    } else {
        crate::summaries::prune_segments(config, &blanked, crate::summaries::Aftermath::Erase)
    };
    println!(
        "\nforget: {} month file(s), {} row(s) {}, {} file(s) left with nothing to search{}",
        outcome.months,
        outcome.rows_matched,
        if dry_run { "matched" } else { "blanked" },
        outcome.emptied,
        if outcome.months == 0 { " — no month file covers that window" } else { "" }
    );
    println!(
        "        what this did not erase: the video and screenshot pixels (`windmaint expire` deletes those), \
         the {} month-file copy(ies) under {} (a `windmaint backup` pass can restore text from them), and \
         the tag and day-poem caches in result_ai_extract_tag/ and result_ai_day_poem/ (`windai` \
         regenerates those). The AI summaries derived from the rows in this window go with them: {}",
        crate::backup::KEEP,
        crate::backup::backup_dir(config).display(),
        outcome.summaries.report(dry_run),
    );
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use rusqlite::Connection;
    use wind_base::clock::LocalParts;
    use wind_store::schema::ensure_schema;

    fn ts(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-forget-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One month file with rows in it, in the layout `read::discover` looks for.
    fn seeded_month(root: &Path, user: &str, year: i64, month: u32, rows: &[(i64, &str, &str)]) -> PathBuf {
        let db_dir = root.join("userdata/db");
        fs::create_dir_all(&db_dir).unwrap();
        let path = db_dir.join(format!("{user}_{year:04}-{month:02}_wind.db"));
        let conn = Connection::open(&path).unwrap();
        ensure_schema(&conn).unwrap();
        for (time, text, title) in rows {
            conn.execute(
                "INSERT INTO video_text (videofile_name, picturefile_name, videofile_time, ocr_text,
                   is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
                 VALUES ('2026-09-22_10-00-00.mp4','a.jpg',?1,?2,1,0,'AAAA',?3,'')",
                rusqlite::params![time, text, title],
            )
            .unwrap();
        }
        path
    }

    fn config_at(root: &Path) -> Config {
        let src = root.join("config_src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("config_default.json"), r#"{"user_name":"default","day_begin_minutes":180}"#).unwrap();
        Config::load(root).unwrap()
    }

    fn open(path: impl AsRef<Path>) -> Connection {
        Connection::open(path).unwrap()
    }

    #[test]
    fn a_period_is_required_and_a_half_given_one_is_named() {
        assert!(Window::from_bounds(None, None).unwrap_err().contains("a period is required"));
        assert!(Window::from_bounds(Some("2026-09-22"), None).unwrap_err().contains("--to is required"));
        assert!(Window::from_bounds(None, Some("2026-09-22")).unwrap_err().contains("--from is required"));
        assert!(Window::from_day("yesterday", 180).is_err());
        assert!(Window::from_bounds(Some("2026-09-23"), Some("2026-09-22")).is_err(), "the wrong way round");
        assert!(Window::from_bounds(Some("2026-9-22"), Some("2026-9-23")).is_err(), "unpadded dates are not the product's spelling");
    }

    #[test]
    fn a_day_names_the_product_day_and_not_the_calendar_one() {
        let window = Window::from_day("2026-09-22", 180).unwrap();
        assert_eq!((window.from, window.to), (ts("2026-09-22_03-00-00"), ts("2026-09-23_02-59-59")));
        assert_eq!(window.rule, "day");
    }

    #[test]
    fn an_erase_blanks_every_row_of_the_window_and_leaves_the_rest_of_the_month() {
        let root = temp_dir("day");
        seeded_month(&root, "default", 2026, 9, &[
            (ts("2026-09-22_04-00-00"), "revenue report", "Browser — rewards"),
            (ts("2026-09-22_10-00-00"), "holiday photos", "Photos"),
            (ts("2026-09-25_10-00-00"), "quarterly plan", "Edge"),
        ]);
        let config = config_at(&root);
        let window = Window::from_day("2026-09-22", 180).unwrap();

        let plan = run(&config, &window, None, true).unwrap();
        assert_eq!((plan.months, plan.rows_matched), (1, 2), "the dry run counts the two rows of that day");
        let before = open(root.join("userdata/db/default_2026-09_wind.db"));
        assert_eq!(
            before.query_row("SELECT count(*) FROM video_text WHERE ocr_text IS NOT NULL", [], |r| r.get::<_, i64>(0)).unwrap(),
            3,
            "and wrote nothing"
        );
        drop(before);

        let done = run(&config, &window, None, false).unwrap();
        assert_eq!((done.months, done.rows_matched, done.rows_erased), (1, 2, 2), "the real run kept the dry run's promise");
        assert_eq!(done.emptied, 0, "the 25th still has a row with text");

        let conn = open(root.join("userdata/db/default_2026-09_wind.db"));
        assert_eq!(
            conn.query_row("SELECT count(*) FROM video_text WHERE ocr_text IS NULL AND win_title IS NULL AND thumbnail IS NULL AND deep_linking IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT ocr_text FROM video_text WHERE videofile_time = ?1", [ts("2026-09-25_10-00-00")], |r| r.get::<_, String>(0)).unwrap(),
            "quarterly plan",
            "a day outside the window is not this command's business"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_keyword_narrows_the_erase_to_the_rows_that_word_finds() {
        let root = temp_dir("keyword");
        seeded_month(&root, "default", 2026, 9, &[
            (ts("2026-09-22_04-00-00"), "revenue report", "Browser — rewards"),
            (ts("2026-09-22_10-00-00"), "holiday photos", "Photos"),
        ]);
        let config = config_at(&root);
        let window = Window::from_day("2026-09-22", 180).unwrap();

        let outcome = run(&config, &window, Some("revenue"), false).unwrap();
        assert_eq!((outcome.rows_matched, outcome.rows_erased), (1, 1));
        assert_eq!(outcome.emptied, 0);

        let conn = open(root.join("userdata/db/default_2026-09_wind.db"));
        assert_eq!(
            conn.query_row("SELECT ocr_text FROM video_text WHERE ocr_text IS NOT NULL", [], |r| r.get::<_, String>(0)).unwrap(),
            "holiday photos"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A month that cannot hold the window is skipped before it is opened — proven by making the file
    /// unwritable, which a pass that opened it for writing could not survive.
    #[test]
    fn a_month_file_that_cannot_hold_the_window_is_never_opened() {
        let root = temp_dir("coverage");
        seeded_month(&root, "default", 2026, 9, &[(ts("2026-09-22_04-00-00"), "revenue", "Edge")]);
        let october = seeded_month(&root, "default", 2026, 10, &[(ts("2026-10-02_04-00-00"), "budget", "Edge")]);
        let permissions = fs::metadata(&october).unwrap().permissions();
        let mut readonly = permissions.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&october, readonly).unwrap();

        let config = config_at(&root);
        let window = Window::from_day("2026-09-22", 180).unwrap();
        let outcome = run(&config, &window, None, false).unwrap();
        assert_eq!(outcome.months, 1, "only September was in play");

        fs::set_permissions(&october, permissions).unwrap();
        let conn = open(&october);
        assert_eq!(
            conn.query_row("SELECT ocr_text FROM video_text", [], |r| r.get::<_, String>(0)).unwrap(),
            "budget",
            "and October keeps its text"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn emptying_a_month_is_reported_and_still_keeps_its_rows_and_pointers() {
        let root = temp_dir("emptied");
        let path = seeded_month(&root, "default", 2026, 9, &[
            (ts("2026-09-22_04-00-00"), "revenue", "Edge"),
            (ts("2026-09-22_10-00-00"), "forecast", "Edge"),
        ]);
        let config = config_at(&root);
        let window = Window::from_bounds(Some("2026-09-22"), Some("2026-09-22")).unwrap();

        let outcome = run(&config, &window, None, false).unwrap();
        assert_eq!((outcome.rows_erased, outcome.emptied), (2, 1), "nothing in the file is searchable now");
        let conn = open(&path);
        assert_eq!(conn.query_row("SELECT count(*) FROM video_text", [], |r| r.get::<_, i64>(0)).unwrap(), 2, "the rows stay");
        assert_eq!(
            conn.query_row("SELECT videofile_name FROM video_text LIMIT 1", [], |r| r.get::<_, String>(0)).unwrap(),
            "2026-09-22_10-00-00.mp4",
            "and so does the pointer to the footage, which this command never deletes"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_window_no_month_covers_is_reported_as_having_done_nothing() {
        let root = temp_dir("empty");
        seeded_month(&root, "default", 2026, 9, &[(ts("2026-09-22_04-00-00"), "revenue", "Edge")]);
        let config = config_at(&root);
        let window = Window::from_day("2030-01-01", 180).unwrap();

        let outcome = run(&config, &window, None, false).unwrap();
        assert_eq!(outcome, Outcome::default(), "no file opened, no row claimed");
        let _ = fs::remove_dir_all(&root);
    }

    use wind_summary::test_support as support;

    const FIRST: &str = "2026-09-22_04-00-00";
    const SECOND: &str = "2026-09-22_10-00-00";
    const DAY: &str = "2026-09-22";

    /// A day with two stretches, both summarised, and a paragraph written from the pair.
    fn summarised_day(root: &Path) -> Config {
        let (first, second) = (format!("{FIRST}.mp4"), format!("{SECOND}.mp4"));
        support::seed_month(
            root,
            "default",
            &[
                (ts(FIRST), first.as_str(), "Browser — rewards", "revenue report"),
                (ts(SECOND), second.as_str(), "Photos", "holiday photos"),
            ],
        );
        let config = config_at(root);
        support::seed_day(&config, DAY, &[FIRST, SECOND]);
        config
    }

    /// The claim this whole pass exists for: text derived from the rows being blanked does not outlive
    /// them, and the day built from that text does not either.
    #[test]
    fn an_erase_takes_the_summaries_of_the_stretches_it_blanks_and_the_day_built_from_them() {
        let root = temp_dir("summaries");
        let config = summarised_day(&root);
        let window = Window::from_day(DAY, 180).unwrap();

        let planned = run(&config, &window, None, true).unwrap();
        assert_eq!((planned.rows_matched, planned.summaries.entries, planned.summaries.days_removed), (2, 2, 1), "{planned:?}");
        assert_eq!(wind_summary::read_period(&config, DAY).len(), 2, "a dry run promises and does not perform");
        assert!(wind_summary::dir(&config, wind_summary::Kind::Daily).join(format!("{DAY}.json")).exists(), "nor does it flag the day");

        let done = run(&config, &window, None, false).unwrap();
        assert_eq!(done.summaries, planned.summaries, "and the real run keeps the dry run's number exactly");
        assert!(wind_summary::read_period(&config, DAY).absent(), "the paragraphs are gone with the rows");
        let daily = wind_summary::dir(&config, wind_summary::Kind::Daily).join(format!("{DAY}.json"));
        assert!(!daily.exists(), "so is the day's own summary, not merely flagged");
        let _ = fs::remove_dir_all(&root);
    }

    /// A keyword erase leaves half the day standing. The day's paragraph then survives — flagged, because
    /// it was written from a list that just got shorter — and the untouched stretch's prose stays too.
    #[test]
    fn a_keyword_erase_drops_one_paragraph_marks_the_day_and_deletes_nothing_else() {
        let root = temp_dir("summaries-keyword");
        let config = summarised_day(&root);
        let window = Window::from_day(DAY, 180).unwrap();

        let planned = run(&config, &window, Some("revenue"), true).unwrap();
        assert_eq!((planned.summaries.entries, planned.summaries.days_marked, planned.summaries.days_removed), (1, 1, 0), "{planned:?}");
        let done = run(&config, &window, Some("revenue"), false).unwrap();
        assert_eq!(done.summaries, planned.summaries);
        let left = wind_summary::read_period(&config, DAY);
        assert_eq!(left.len(), 1, "the other stretch's paragraph stands");
        assert!(left.get(SECOND).is_some());
        let daily = wind_summary::read_daily(&config, DAY).summary.expect("kept");
        assert!(daily.stale, "and says its premise moved");
        assert_eq!(daily.text, support::day_summary(DAY, 2).text, "its prose is not rewritten by a pass that reads it as incomplete");
        let _ = fs::remove_dir_all(&root);
    }
}
