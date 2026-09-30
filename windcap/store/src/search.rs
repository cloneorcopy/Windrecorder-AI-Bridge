//! The search the product is named after.
//!
//! The shape of the statement is dictated by what users have already been shown: whitespace-split
//! tokens, ANDed; each token ORed across `ocr_text` and `win_title`; each token optionally expanded
//! into its shape-similar Chinese variants; a NOT-AND chain of exclude terms; and an inclusive
//! `BETWEEN` on the stored naive-local epoch. That is compatibility.
//!
//! The one thing deliberately *not* copied is how upstream builds this SQL — by pasting the user's
//! own text into the string, with `'` doubled on the keyword path and on nothing else. A window
//! title containing a quote is ordinary data, so upstream's search breaks on real history. Here
//! every value is a bound parameter and the only interpolated fragments are structural.

use std::path::PathBuf;
use std::time::Duration;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, ToSql};

use crate::read::{self, Month, Row};
use crate::schema::StoreError;
use crate::similar::SimilarChars;

/// A parsed search. Construct one with [`Query::new`] and turn the fields on as needed.
#[derive(Debug, Clone)]
pub struct Query {
    /// Whitespace-separated terms, all of which must appear.
    pub tokens: Vec<String>,
    /// Terms that must not appear in the body text.
    pub exclude: Vec<String>,
    /// Inclusive range in the stored epoch.
    pub from: i64,
    pub to: i64,
    /// Fuzzy glyph expansion; `None` disables it, as `use_similar_ch_char_to_search = false` does.
    pub similar: Option<SimilarChars>,
    /// Trim the result to a page. Applied after the per-month rows are merged and re-sorted, so
    /// paging is global rather than "the first page of each file".
    pub limit: Option<usize>,
    pub offset: usize,
}

impl Query {
    pub fn new(from: i64, to: i64) -> Query {
        Query {
            tokens: Vec::new(),
            exclude: Vec::new(),
            from,
            to,
            similar: None,
            limit: None,
            offset: 0,
        }
    }

    /// Split on whitespace the way `keyword_input.split()` does: runs of any whitespace collapse.
    pub fn with_keywords(mut self, keywords: &str) -> Query {
        self.tokens = keywords.split_whitespace().map(|s| s.trim().to_string()).collect();
        self
    }

    pub fn with_exclude(mut self, keywords: &str) -> Query {
        self.exclude = keywords.split_whitespace().map(|s| s.trim().to_string()).collect();
        self
    }

    pub fn with_similar(mut self, table: SimilarChars) -> Query {
        self.similar = Some(table);
        self
    }

    pub fn page(mut self, page_size: usize, page_index: usize) -> Query {
        // `page_index` is 1-based, because that is how the WebUI's number input has always counted.
        if page_size == 0 {
            return self;
        }
        self.limit = Some(page_size);
        self.offset = page_size.saturating_mul(page_index.saturating_sub(1));
        self
    }

    /// Upstream's `re.sub(r"(?<=\w)-(?=\w)", " ", keyword)`: an inner hyphen between word
    /// characters becomes a space, so `self-help` searches for both words.
    fn split_inner_hyphens(term: &str) -> Vec<String> {
        let chars: Vec<char> = term.chars().collect();
        let mut out = vec![String::new()];
        for (i, ch) in chars.iter().enumerate() {
            let is_word_sep = *ch == '-'
                && i > 0
                && i + 1 < chars.len()
                && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')
                && (chars[i + 1].is_alphanumeric() || chars[i + 1] == '_');
            if is_word_sep {
                out.push(String::new());
            } else {
                out.last_mut().unwrap().push(*ch);
            }
        }
        out.into_iter().filter(|s| !s.is_empty()).collect()
    }

    /// The `WHERE` fragment and its parameters, as positional `?` binds in order.
    pub fn where_clause(&self) -> (String, Vec<SqlValue>) {
        let mut sql = String::from(" WHERE ");
        let mut binds: Vec<SqlValue> = Vec::new();
        let mut parts: Vec<String> = Vec::new();

        if self.tokens.is_empty() {
            parts.push("ocr_text LIKE ?".to_string());
            binds.push(SqlValue::Text("%".to_string()));
        } else if let Some(table) = &self.similar {
            for token in &self.tokens {
                let variants = table.variants_for_token(token, crate::similar::MAX_VARIANTS);
                let group = variants
                    .iter()
                    .map(|_| "(ocr_text LIKE ? OR win_title LIKE ?)")
                    .collect::<Vec<_>>()
                    .join(" OR ");
                for variant in &variants {
                    let pattern = format!("%{variant}%");
                    binds.push(SqlValue::Text(pattern.clone()));
                    binds.push(SqlValue::Text(pattern));
                }
                parts.push(format!("({group})"));
            }
        } else {
            for token in &self.tokens {
                let mut pieces = Vec::new();
                for part in Self::split_inner_hyphens(token) {
                    let pattern = format!("%{part}%");
                    binds.push(SqlValue::Text(pattern.clone()));
                    binds.push(SqlValue::Text(pattern));
                    pieces.push("(ocr_text LIKE ? OR win_title LIKE ?)".to_string());
                }
                if pieces.is_empty() {
                    continue;
                }
                parts.push(format!("({})", pieces.join(" AND ")));
            }
        }

        for term in &self.exclude {
            for part in Self::split_inner_hyphens(term) {
                parts.push("ocr_text NOT LIKE ?".to_string());
                binds.push(SqlValue::Text(format!("%{part}%")));
            }
        }

        // `BETWEEN a AND b` on both ends of the range, with upstream's degenerate-range fix: a
        // search whose bounds are identical would otherwise match nothing but an exact second.
        let (from, to) = if self.from == self.to {
            (self.from, self.to + 1)
        } else {
            (self.from.min(self.to), self.to.max(self.from))
        };
        parts.push("(videofile_time BETWEEN ? AND ?)".to_string());
        binds.push(SqlValue::Integer(from));
        binds.push(SqlValue::Integer(to));

        sql.push_str(&parts.join(" AND "));
        (sql, binds)
    }

    /// How many rows match, ignoring paging. The UI needs this before it can draw page controls.
    pub fn count_sql(&self) -> (String, Vec<SqlValue>) {
        let (where_clause, binds) = self.where_clause();
        (format!("SELECT COUNT(*) FROM video_text{where_clause}"), binds)
    }

    fn rows_sql(&self) -> (String, Vec<SqlValue>) {
        let (where_clause, mut binds) = self.where_clause();
        let mut sql = format!(
            "SELECT {} FROM video_text{where_clause} ORDER BY videofile_time, rowid",
            read::SELECT_COLUMNS
        );
        if let Some(limit) = self.limit {
            sql.push_str(" LIMIT ? OFFSET ?");
            binds.push(SqlValue::Integer(limit as i64));
            binds.push(SqlValue::Integer(self.offset as i64));
        }
        (sql, binds)
    }
}

/// Everything a search produced, across every month it touched.
#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub rows: Vec<Row>,
    /// Rows matched before paging.
    pub total: i64,
    /// Rows per page, as configured, so the caller can render page controls without recomputing.
    pub page_size: usize,
}

impl SearchResult {
    pub fn page_count(&self) -> usize {
        if self.page_size == 0 || self.total == 0 {
            return if self.total > 0 { 1 } else { 0 };
        }
        ((self.total as usize) + self.page_size - 1) / self.page_size
    }
}

/// Search one open database.
pub fn search(conn: &Connection, query: &Query) -> Result<Vec<Row>, StoreError> {
    let (sql, binds) = query.rows_sql();
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn ToSql> = binds.iter().map(|v| v as &dyn ToSql).collect();
    let mut rows = Vec::new();
    let mapped = stmt.query_map(refs.as_slice(), read::row_from)?;
    for row in mapped {
        rows.push(row?);
    }
    Ok(rows)
}

pub fn count(conn: &Connection, query: &Query) -> Result<i64, StoreError> {
    let (sql, binds) = query.count_sql();
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn ToSql> = binds.iter().map(|v| v as &dyn ToSql).collect();
    let total: i64 = stmt.query_row(refs.as_slice(), |r| r.get(0))?;
    Ok(total)
}

/// Search a set of months and merge the results in time order.
///
/// Paging is applied to the merge, not to each file, which is what makes a page boundary land in
/// the same place regardless of how many months a range spans.
pub fn search_months(
    months: &[Month],
    query: &Query,
    stale_after: Duration,
    maintaining: bool,
) -> Result<SearchResult, StoreError> {
    let per_month_page = Query { limit: None, offset: 0, ..query.clone() };
    let mut rows: Vec<Row> = Vec::new();
    let mut total = 0i64;

    for month in months {
        let conn = month.open_read(stale_after, maintaining)?;
        total += count(&conn, &per_month_page)?;
        let mut found = search(&conn, &per_month_page)?;
        for row in &mut found {
            row.month_path = Some(month.path.clone());
        }
        rows.append(&mut found);
    }

    // SQLite sorts each file's rows correctly, but the merge across files is ours to do.
    rows.sort_by_key(|r| (r.time, r.rowid));
    if let Some(limit) = query.limit {
        rows = rows.into_iter().skip(query.offset).take(limit).collect();
    }
    Ok(SearchResult { rows, total, page_size: query.limit.unwrap_or(0) })
}

/// Convenience: the file a row's segment actually lives in, if it is still there.
///
/// The index stores the name, not a stable path, so resolution is a search over the monthly video
/// folders. `names_on_disk` is expected to be the caller's cached directory listing — upstream
/// does the same (`config.record_videos_dir_ud` scanned once per session) because re-listing per
/// row is what made the old search screen crawl.
pub fn locate_segment(names_on_disk: &[String], row: &Row, videos_root: &str) -> Option<PathBuf> {
    let stem = row.videofile_name.rsplit(['/', '\\']).next().unwrap_or(&row.videofile_name);
    let stamp: String = stem.chars().take(wind_base::paths::STAMP_LEN).collect();
    let found = names_on_disk.iter().find(|name| {
        let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
        name.starts_with(&stamp)
    })?;
    Some(PathBuf::from(videos_root).join(found))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn ts(stamp: &str) -> i64 {
        wind_base::clock::LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::ensure_schema(&conn).unwrap();
        let rows = [
            ("2026-09-21_10-00-00.mp4", ts("2026-09-21_10-00-00"), "季度复盘 Q3 revenue", "Excel", "chrome"),
            ("2026-09-21_11-00-00.mp4", ts("2026-09-21_11-00-00"), "revenue forecast 收入", "Notepad", ""),
            ("2026-09-21_12-00-00.mp4", ts("2026-09-21_12-00-00"), "private note", "Obsidian", "vault"),
            ("2026-09-21_13-00-00.mp4", ts("2026-09-21_13-00-00"), "收入 收入 收入", "", ""),
        ];
        for (name, time, text, title, deep) in rows {
            conn.execute(
                "INSERT INTO video_text (videofile_name, picturefile_name, videofile_time, ocr_text,
                   is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
                 VALUES (?1,?2,?3,?4,1,0,'AAA',?5,?6)",
                params![name, "p.jpg", time, text, title, deep],
            )
            .unwrap();
        }
        conn
    }

    fn all_range() -> (i64, i64) {
        (ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"))
    }

    #[test]
    fn a_bare_range_returns_everything_in_it_oldest_first() {
        let conn = db();
        let (from, to) = all_range();
        let hits = search(&conn, &Query::new(from, to)).unwrap();
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].videofile_name, "2026-09-21_10-00-00.mp4");
        assert_eq!(hits.last().unwrap().videofile_name, "2026-09-21_13-00-00.mp4");
    }

    #[test]
    fn an_out_of_range_query_returns_nothing() {
        let conn = db();
        let hits = search(&conn, &Query::new(ts("2026-01-01_00-00-00"), ts("2026-01-02_00-00-00"))).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn tokens_are_anded_and_matched_across_body_and_title() {
        let conn = db();
        let (from, to) = all_range();
        let one = search(&conn, &Query::new(from, to).with_keywords("revenue")).unwrap();
        assert_eq!(one.len(), 2, "two bodies carry revenue");
        // Two tokens must both appear, in the same row, in either field.
        let both = search(&conn, &Query::new(from, to).with_keywords("revenue 收入")).unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].videofile_name, "2026-09-21_11-00-00.mp4");
        let body_and_title = search(&conn, &Query::new(from, to).with_keywords("revenue Excel")).unwrap();
        assert_eq!(body_and_title.len(), 1, "one term may come from the title, the other from the body");
        let by_title = search(&conn, &Query::new(from, to).with_keywords("Excel")).unwrap();
        assert_eq!(by_title.len(), 1, "window titles are searched too");
    }

    #[test]
    fn exclude_terms_remove_rows_without_touching_the_rest() {
        let conn = db();
        let (from, to) = all_range();
        let hits = search(&conn, &Query::new(from, to).with_keywords("收入").with_exclude("private"))
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|r| !r.body().contains("private")));
    }

    #[test]
    fn paging_is_global_and_count_ignores_it() {
        let conn = db();
        let (from, to) = all_range();
        let page1 = search(&conn, &Query::new(from, to).page(2, 1)).unwrap();
        let page2 = search(&conn, &Query::new(from, to).page(2, 2)).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        assert_ne!(page1[0].videofile_name, page2[0].videofile_name);
        assert_eq!(count(&conn, &Query::new(from, to)).unwrap(), 4);
        // A page past the end is empty, not an error.
        assert!(search(&conn, &Query::new(from, to).page(2, 9)).unwrap().is_empty());
    }

    #[test]
    fn page_count_rounds_up() {
        let result = SearchResult { rows: vec![], total: 21, page_size: 20 };
        assert_eq!(result.page_count(), 2);
        assert_eq!(SearchResult { rows: vec![], total: 0, page_size: 20 }.page_count(), 0);
    }

    /// The whole reason this module exists: the user's data is not SQL.
    #[test]
    fn a_quote_in_the_data_or_the_query_cannot_break_the_statement() {
        let conn = db();
        // The nastiest thing a window title can contain, inserted the way upstream inserts it.
        conn.execute(
            "INSERT INTO video_text (videofile_name, picturefile_name, videofile_time, ocr_text,
               is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
             VALUES ('x.mp4','p',?1,?2,0,0,'',NULL,'')",
            params![ts("2026-09-21_14-00-00"), "Robert 'Bobby' tables'); DROP TABLE video_text;--"],
        )
        .unwrap();
        let (from, to) = all_range();
        let hits = search(&conn, &Query::new(from, to).with_keywords("'Bobby'")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(count(&conn, &Query::new(from, to)).unwrap(), 5, "the table must still be there");
        // An attacker-shaped keyword is just a string that happens to match nothing.
        assert!(search(&conn, &Query::new(from, to).with_keywords("1=1; --")).unwrap().is_empty());
    }

    #[test]
    fn inner_hyphens_split_like_the_upstream_regex() {
        assert_eq!(Query::split_inner_hyphens("self-help"), vec!["self", "help"]);
        assert_eq!(Query::split_inner_hyphens("-leading"), vec!["-leading"]);
        assert_eq!(Query::split_inner_hyphens("trailing-"), vec!["trailing-"]);
        assert_eq!(Query::split_inner_hyphens("a-b-c"), vec!["a", "b", "c"]);
        assert_eq!(Query::split_inner_hyphens("收入-支出"), vec!["收入", "支出"]);
    }

    #[test]
    fn hyphen_split_terms_are_anded_within_one_token() {
        let conn = db();
        let (from, to) = all_range();
        let hits = search(&conn, &Query::new(from, to).with_keywords("Q3-revenue")).unwrap();
        assert_eq!(hits.len(), 1, "a hyphenated query finds the row where the words are apart");
        assert_eq!(
            search(&conn, &Query::new(from, to).with_keywords("revenue-forecast")).unwrap()[0]
                .videofile_name,
            "2026-09-21_11-00-00.mp4"
        );
        assert!(search(&conn, &Query::new(from, to).with_keywords("revenue-forecasting")).unwrap().is_empty());
    }

    #[test]
    fn similar_glyph_expansion_widens_recall() {
        let conn = db();
        let (from, to) = all_range();
        // A table that confuses 收 with 改 must find the 收入 rows when asked for 改入.
        let table = SimilarChars::parse("收，改\n");
        let plain = search(&conn, &Query::new(from, to).with_keywords("改入")).unwrap();
        assert!(plain.is_empty());
        let fuzzy = search(&conn, &Query::new(from, to).with_keywords("改入").with_similar(table)).unwrap();
        assert_eq!(fuzzy.len(), 2);
    }

    #[test]
    fn a_degenerate_range_still_matches_its_own_second() {
        let conn = db();
        let at = ts("2026-09-21_11-00-00");
        assert_eq!(search(&conn, &Query::new(at, at)).unwrap().len(), 1);
    }

    #[test]
    fn bounds_the_wrong_way_round_instead_of_matching_nothing() {
        let (from, to) = all_range();
        let (sql, binds) = Query::new(to, from).where_clause();
        assert!(sql.contains("BETWEEN ? AND ?"));
        assert_eq!(binds[binds.len() - 2], SqlValue::Integer(from));
        assert_eq!(binds[binds.len() - 1], SqlValue::Integer(to));
    }

    #[test]
    fn locate_segment_resolves_a_name_through_the_stamped_prefix() {
        let conn = db();
        let (from, to) = all_range();
        let row = search(&conn, &Query::new(from, to).with_keywords("private")).unwrap().remove(0);
        let names = vec![
            "2026-09-21_12-00-00-VIDEO-SCREENSHOTS-OCRED.mp4".to_string(),
            "2026-09-21_09-00-00.mp4".to_string(),
        ];
        let found = locate_segment(&names, &row, "userdata/videos/2026-09").expect("found");
        assert_eq!(found.file_name().unwrap(), "2026-09-21_12-00-00-VIDEO-SCREENSHOTS-OCRED.mp4");
        assert!(locate_segment(&names[..1], &Row { videofile_name: "missing".into(), ..row.clone() }, "x").is_none());
    }
}
