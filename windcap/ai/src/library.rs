//! The only module in this crate that opens the user's index.
//!
//! Keeping it single and boring buys the two properties the rest of the crate depends on: every
//! feature reads the same library bounds (so the date clamp in `plan` and the month table in `tags`
//! agree about what exists), and every read goes through `wind_store`'s `_TEMP_READ.db` copy, so
//! neither feature can hold a lock the recorder needs mid-segment.
//!
//! It is also the seam that makes the AI features testable: the index half is a directory, and the
//! model half is a [`crate::client::Transport`], so an end-to-end test needs a temp folder and a
//! loopback listener and nothing else.

use std::path::{Path, PathBuf};
use std::time::Duration;

use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_store::read::{self, Month, Row};
use wind_store::search::{self as store_search, Query, SearchResult};
use wind_store::similar::SimilarChars;

use crate::error::{AiError, Faults};
use crate::settings::Settings;

/// Five minutes, the same window `windcapctl` uses: refreshing the copy on every AI call would make a
/// tag batch slower than the maintenance pass it is supposed to run alongside.
pub const READ_STALE_AFTER: Duration = Duration::from_secs(300);

/// The whole install: its configuration, its month files, and the span they actually cover.
pub struct Index {
    pub root: PathBuf,
    pub config: Config,
    pub settings: Settings,
    pub months: Vec<Month>,
    /// `(earliest, latest)` in the stored naive-local epoch, from the rows themselves.
    bounds: Option<(i64, i64)>,
    faults: Faults,
}

impl Index {
    /// Load configuration from `root` and list its month files. Opens no database yet.
    pub fn open(root: &Path) -> Result<Index, AiError> {
        let loader = Faults::anonymous();
        let config = Config::load(root).map_err(|e| loader.io(root, e))?;
        let settings = Settings::read(&config);
        // From here on every message is scrubbed with the real key, because from here on every
        // message could have come from a request that carried it.
        let faults = Faults::new(&settings.api_key);
        let months = read::discover(&config.db_dir());
        Ok(Index { root: root.to_path_buf(), config, settings, months, bounds: None, faults })
    }

    pub fn faults(&self) -> &Faults {
        &self.faults
    }

    pub fn is_empty(&self) -> bool {
        self.months.is_empty()
    }

    pub fn month_count(&self) -> usize {
        self.months.len()
    }

    /// The first and last row in the library, which is the span a natural-language date range may be
    /// clamped into.
    ///
    /// Only the two edge month files are opened, because a month file's *name* is already its time
    /// range — scanning every file to learn what the filenames say would copy years of history to
    /// `_TEMP_READ.db` for a single number. `None` means the install is empty, which the callers turn
    /// into a message rather than a search over nothing.
    pub fn bounds(&mut self) -> Result<Option<(i64, i64)>, AiError> {
        if self.bounds.is_some() {
            return Ok(self.bounds);
        }
        let edges = match (self.months.first(), self.months.last()) {
            (Some(first), Some(last)) if std::ptr::eq(first, last) => vec![first.clone()],
            (Some(first), Some(last)) => vec![first.clone(), last.clone()],
            _ => return Ok(None),
        };
        let mut lowest = i64::MAX;
        let mut highest = i64::MIN;
        let mut seen = false;
        for month in &edges {
            let conn = month.open_read(READ_STALE_AFTER, self.maintaining()).map_err(|e| self.err().store(e))?;
            if let Some((from, to)) = read::time_bounds(&conn).map_err(|e| self.err().store(e))? {
                seen = true;
                lowest = lowest.min(from);
                highest = highest.max(to);
            }
        }
        self.bounds = if seen { Some((lowest, highest)) } else { None };
        Ok(self.bounds)
    }

    /// The same as [`Index::bounds`], but phrased as dates for a prompt, and refused when there is
    /// nothing to search.
    pub fn date_bounds(&mut self) -> Result<(String, String), AiError> {
        let (from, to) = self
            .bounds()?
            .ok_or_else(|| self.err().store("the index holds no recorded rows yet — nothing to search"))?;
        Ok((
            LocalParts::from_naive_epoch(from).date_stamp(),
            LocalParts::from_naive_epoch(to).date_stamp(),
        ))
    }

    /// Whether a maintenance run is holding the files open with a live process behind its lock, which
    /// is the one case where reading through a stale copy beats refreshing it. An empty
    /// `LOCK_MAINTAIN` directory is not that case: a finished pass leaves it standing.
    fn maintaining(&self) -> bool {
        self.config.maintain_lock_claimed()
    }

    fn err(&self) -> &Faults {
        &self.faults
    }

    /// The months a range touches, which is what keeps a query for one Tuesday from opening years of
    /// history.
    pub fn months_in(&self, from: i64, to: i64) -> Vec<Month> {
        read::months_in_range(&self.months, from, to).into_iter().cloned().collect()
    }

    /// The shape-similar Chinese glyph table, when the install has one and the user has not switched
    /// fuzzy matching off. Honoured because a mistyped OCR glyph is the commonest reason a keyword
    /// search of this data comes back empty.
    fn similar(&self) -> Option<SimilarChars> {
        if !self.config.bool_or("use_similar_ch_char_to_search", true) {
            return None;
        }
        SimilarChars::load(&self.config_src_file("similar_CN_characters.txt")).ok()
    }

    /// Run a query across the months it spans, adding the shape-similar glyph expansion if the install
    /// has one. `total` on the result counts every match regardless of the query's own `limit`, so a
    /// caller that scanned with a cap can still report the true number.
    pub fn run(&self, query: &Query) -> Result<SearchResult, AiError> {
        let months = self.months_in(query.from, query.to);
        if months.is_empty() {
            return Ok(SearchResult::default());
        }
        let query = match self.similar() {
            Some(table) => query.clone().with_similar(table),
            None => query.clone(),
        };
        store_search::search_months(&months, &query, READ_STALE_AFTER, self.maintaining())
            .map_err(|e| self.err().store(e))
    }

    /// A file inside the settings directory.
    ///
    /// [`Config::config_src_dir`] decides which of the two layouts that is — the payload's
    /// `config_src/`, or the `windrecorder/config_src/` an overlay install still keeps — and it is
    /// the same answer the config layer was read from, so the Chinese fuzzy-glyph table cannot come
    /// from a different install than the keys that say to use it.
    fn config_src_file(&self, name: &str) -> PathBuf {
        self.config.config_src_file(name)
    }

    /// Every row of one calendar month, oldest first, for the tag feature.
    pub fn month_rows(&self, year: i64, month: u32) -> Result<Vec<Row>, AiError> {
        let months: Vec<Month> = self
            .months
            .iter()
            .filter(|m| m.year == year && m.month == month)
            .cloned()
            .collect();
        if months.is_empty() {
            return Ok(Vec::new());
        }
        // The month's own calendar span, which is deliberately *not* shifted by
        // `day_begin_minutes`: that shift is a query-time grouping for the day views (see
        // `aggregate::histogram`), while the rows of a month file are split by calendar month, so
        // clipping the read to the calendar span is what keeps a 02:00 row on the 1st in its month.
        let (from, to) = months.iter().fold((i64::MAX, i64::MIN), |acc, m| {
            let (low, high) = m.coverage();
            (acc.0.min(low), acc.1.max(high))
        });
        let mut out = Vec::new();
        for month in &months {
            let conn = month.open_read(READ_STALE_AFTER, self.maintaining()).map_err(|e| self.err().store(e))?;
            let rows = read::rows_in_window(&conn, Some(from), Some(to)).map_err(|e| self.err().store(e))?;
            out.extend(rows);
        }
        out.sort_by_key(|r| (r.time, r.rowid));
        Ok(out)
    }

    /// Build the query a validated plan describes. Public so a caller (and the CLI's `--explain`) can
    /// see the exact statement the plan turns into.
    pub fn query_for(&self, plan: &crate::plan::SearchPlan, limit: usize) -> Query {
        let base = Query::new(plan.from, plan.to)
            .with_keywords(&plan.keywords_joined())
            .with_exclude(&plan.exclude_joined());
        Query { limit: Some(limit.max(1)), ..base }
    }
}

/// `--root`, or the install this binary was launched from.
///
/// [`wind_base::install`] owns the rule and every binary asks it the same question, because "which
/// install am I?" is one fact and this workspace used to compute it eight ways. Development runs
/// from `windcap/target/debug` need the walk up; a payload staged at `C:\Windrecorder` does not.
pub fn resolve_root(explicit: Option<PathBuf>) -> PathBuf {
    wind_base::install::resolve_root_from_exe(explicit)
}
