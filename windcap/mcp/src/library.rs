//! The only place in this binary that opens the user's files.
//!
//! One module owns every filesystem and database touch, for the same reason `windcapctl` has a
//! `library.rs`: the command and tool bodies stay pure functions over data, and the staleness and
//! locking policy exists in exactly one place. A reader that guesses five minutes where the writer
//! expects five seconds re-copies the whole index on every keystroke.
//!
//! There is no SQL here, and none is reachable from here. Every statement the bridge causes is
//! `wind-store`'s own, issued through `Month::open_with`, `read::rows_in_window`,
//! `search::search_months` and the `aggregate` functions. That is the boundary the crate is built
//! against: a bridge that could write a `SELECT` is a bridge that will eventually write a `WHERE`
//! nobody reviewed.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use wind_store::read::{self, Month, Row};
use wind_store::search::{Query, SearchResult};

use crate::runtime::Runtime;

/// Five minutes, which is `wind-store`'s own default and upstream's window in
/// `db_manager.get_temp_dbfilepath`.
pub const READ_STALE_AFTER: Duration = Duration::from_secs(300);

/// A month file and the totals that are cheaper to ask it for than to compute here.
#[derive(Debug, Clone)]
pub struct MonthFacts {
    pub month: Month,
    pub rows: i64,
    /// Distinct `videofile_name` values, i.e. segments with at least one indexed row.
    pub segments: i64,
    pub bounds: Option<(i64, i64)>,
    /// Which of the two ALTER-added columns this file predates, so a caller can say *why* a
    /// historical month has no titles rather than reporting an empty answer.
    pub missing_columns: Vec<String>,
}

/// The searchable columns a month file actually offers.
pub const SEARCHABLE: [&str; 2] = ["ocr_text", "win_title"];

impl Runtime {
    /// How this reader is allowed to read: the staleness window plus whether the idle maintenance
    /// pass holds its directory lock with a live process behind it, in which case the read copies are
    /// deliberately left alone because the origin is being rewritten underneath them. The directory
    /// outlives the pass that used it, so its existence is not the answer.
    fn read_options(&self) -> read::ReadOptions {
        read::ReadOptions {
            stale_after: READ_STALE_AFTER,
            maintaining: self.config().maintain_lock_claimed(),
        }
    }

    /// Per-file totals across the whole library, oldest first, and how long that took.
    ///
    /// `COUNT(DISTINCT videofile_name)` comes from `read::count_segments` rather than from counting
    /// filenames here: pulling a year of `ocr_text` into memory to count strings is the mistake this
    /// rewrite exists to stop making.
    pub fn facts(&self) -> (Vec<MonthFacts>, Vec<String>, f64) {
        let started = Instant::now();
        let options = self.read_options();
        let mut out = Vec::new();
        let mut unreadable = Vec::new();
        for month in self.months() {
            let conn = match month.open_with(options) {
                Ok(conn) => conn,
                Err(e) => {
                    unreadable.push(format!("{}: {e}", name(&month)));
                    continue;
                }
            };
            let rows = match read::count_rows(&conn) {
                Ok(rows) => rows,
                Err(e) => {
                    unreadable.push(format!("{}: {e}", name(&month)));
                    continue;
                }
            };
            let columns = read::available_columns(&conn).unwrap_or_default();
            out.push(MonthFacts {
                month: month.clone(),
                rows,
                segments: read::count_segments(&conn).unwrap_or(0),
                bounds: read::time_bounds(&conn).unwrap_or(None),
                missing_columns: SEARCHABLE
                    .iter()
                    .filter(|column| !columns.iter().any(|have| have == *column))
                    .map(|column| (*column).to_string())
                    .collect(),
            });
        }
        (out, unreadable, elapsed_ms(started))
    }

    /// Every row in a window, oldest first, merged across the months it touches.
    ///
    /// A month that cannot be opened degrades to a note rather than failing the answer: the
    /// recorder runs without WAL and without writer retry, so the month currently being indexed can
    /// raise instead of blocking, and a locked file must not hide the historical ones.
    pub fn rows_in(&self, from: i64, to: i64) -> (Vec<Row>, Vec<String>) {
        let options = self.read_options();
        let mut rows = Vec::new();
        let mut skipped = Vec::new();
        for month in self.months_covering(from, to) {
            let conn = match month.open_with(options) {
                Ok(conn) => conn,
                Err(e) => {
                    skipped.push(format!("{}: {e}", name(&month)));
                    continue;
                }
            };
            match read::rows_in_window(&conn, Some(from), Some(to)) {
                Ok(found) => rows.extend(found),
                Err(e) => skipped.push(format!("{}: {e}", name(&month))),
            }
        }
        rows.sort_by_key(|row| (row.time, row.rowid));
        (rows, skipped)
    }

    /// A keyword search across the months the range touches, with the wall-clock cost attached.
    pub fn run_search(&self, query: &Query) -> Result<(SearchResult, Vec<String>, f64), String> {
        let started = Instant::now();
        let options = self.read_options();
        let months = self.months_covering(query.from, query.to);
        let mut skipped = Vec::new();
        // `search_months` opens every file itself and fails the whole call if one is locked, so the
        // merge is done one month at a time here to keep that degradation local. Paging is applied
        // afterwards, which is what makes a page boundary land in the same place no matter how many
        // months the range spans.
        let unpaged = Query { limit: None, offset: 0, ..query.clone() };
        let mut rows: Vec<Row> = Vec::new();
        for month in &months {
            let conn = match month.open_with(options) {
                Ok(conn) => conn,
                Err(e) => {
                    skipped.push(format!("{}: {e}", name(month)));
                    continue;
                }
            };
            match wind_store::search::search(&conn, &unpaged) {
                Ok(mut found) => {
                    for row in &mut found {
                        row.month_path = Some(month.path.clone());
                    }
                    rows.append(&mut found);
                }
                Err(e) => skipped.push(format!("{}: {e}", name(month))),
            }
        }
        // The unpaged merge *is* the match set, so the total needs no second statement.
        let total = rows.len() as i64;
        rows.sort_by_key(|row| (row.time, row.rowid));
        if let Some(limit) = query.limit {
            rows = rows.into_iter().skip(query.offset).take(limit).collect();
        }
        Ok((
            SearchResult { rows, total, page_size: query.limit.unwrap_or(0) },
            skipped,
            elapsed_ms(started),
        ))
    }

    /// The month files a range touches, for a caller that only needs to know how many.
    pub fn months_for(&self, from: i64, to: i64) -> Vec<Month> {
        self.months_covering(from, to)
    }
}

/// How long a read took, in milliseconds — printed next to every answer by the CLI, because a
/// claim nobody can measure in thirty seconds is not a claim.
pub fn elapsed_ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// A month file's name as reports quote it: the file itself, not the parsed coordinates.
pub fn name(month: &Month) -> String {
    month.path.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string()
}

/// Row counts keyed by month file name, which is how `status` prints a table without holding
/// connections open.
pub type PerMonth = BTreeMap<String, i64>;
