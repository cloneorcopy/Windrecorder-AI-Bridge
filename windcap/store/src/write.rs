//! The writer.
//!
//! One transaction per batch, positional binds in `COLUMNS` order, integers stored as integers.
//! The Python path reaches the same state through `DataFrame.to_sql`, which issues one autocommit
//! INSERT per row — correct, and roughly two orders of magnitude slower on a 300-frame segment.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use wind_base::paths;

use crate::schema::{ensure_schema, StoreError, COLUMNS, TITLE_SEPARATOR};
use crate::read::Month;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub videofile_name: String,
    pub picturefile_name: String,
    pub videofile_time: i64,
    pub ocr_text: String,
    pub win_title: Option<String>,
    pub deep_linking: Option<String>,
    pub thumbnail: Option<String>,
}

impl Record {
    /// Compose `ocr_text` the way the video-indexing path does: body, separator, window title.
    ///
    /// Appending rather than storing the title only in its column is not redundancy — the search
    /// that users rely on runs over `ocr_text`, and history already indexed that way has to keep
    /// matching.
    pub fn indexed_text(&self) -> String {
        match &self.win_title {
            Some(title) if !title.trim().is_empty() => {
                format!("{}{}{}", self.ocr_text, TITLE_SEPARATOR, title)
            }
            _ => self.ocr_text.clone(),
        }
    }
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open (creating if needed) the month file for `(year, month)` under `dir`.
    pub fn open_month(dir: &Path, user: &str, year: i64, month: u32) -> Result<Store, StoreError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(paths::month_filename(user, year, month));
        Store::open(&path)
    }

    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let conn = Connection::open(path)?;
        // The Python app runs with the default rollback journal and no WAL; matching it keeps the
        // locking behaviour that the read-copy strategy assumes.
        conn.pragma_update(None, "journal_mode", "delete")?;
        // Wait rather than fail when somebody else is committing. A sharded back-index runs several
        // `wind-reindex` processes over one library, and a segment that crosses a month boundary writes
        // two month files — so two lanes can legitimately reach for the same file at the same moment.
        // Without this the second one gets `SQLITE_BUSY` immediately and the segment is renamed
        // `-ERROR1` for a collision that is not a fault of anyone's.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        ensure_schema(&conn)?;
        Ok(Store { conn, path: path.to_path_buf() })
    }

    pub fn from_month(month: &Month) -> Result<Store, StoreError> {
        Store::open(&month.path)
    }

    /// Borrow the connection, for the maintenance statements in `crate::maintain`.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn row_count(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM video_text", [], |r| r.get(0))?)
    }

    /// Insert a batch atomically. Returns the number of rows written.
    ///
    /// Atomicity is the point: a segment that dies halfway must leave no half-indexed rows pointing
    /// at frames the next run will not re-OCR.
    pub fn append(&mut self, records: &[Record]) -> Result<usize, StoreError> {
        if records.is_empty() {
            return Ok(0);
        }
        let sql = insert_sql();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(&sql)?;
            for rec in records {
                stmt.execute(params![
                    rec.videofile_name,
                    rec.picturefile_name,
                    rec.videofile_time,
                    rec.indexed_text(),
                    1i64, // is_videofile_exist: upstream hardcodes true on write, a later pass corrects it
                    0i64, // is_picturefile_exist: same
                    rec.thumbnail,
                    rec.win_title,
                    rec.deep_linking,
                ])?;
            }
        }
        tx.commit()?;
        Ok(records.len())
    }

    /// The rows still waiting for text, oldest first: `(rowid, videofile_name, picturefile_name, title)`.
    ///
    /// "Waiting" is an empty body, and there are two shapes of that, because every writer composes the
    /// window title into `ocr_text`: a row stored with no text at all, and a row stored as nothing but
    /// its title — `" -||- - Notepad"`, which is what a deferred row and an engine that could not read
    /// the screen both look like. Both are worth another try, and neither is a row that was read.
    ///
    /// A `pending` column is not an option: the nine columns are a zero-drift contract with every file
    /// the Python app wrote.
    pub fn pending_text(&self) -> Result<Vec<(i64, String, String, Option<String>)>, StoreError> {
        let separator = format!("{TITLE_SEPARATOR}%");
        let mut stmt = self.conn.prepare(
            "SELECT rowid, videofile_name, picturefile_name, win_title FROM video_text \
             WHERE ocr_text IS NULL OR ocr_text = '' OR ocr_text LIKE ?1 \
             ORDER BY videofile_time, rowid",
        )?;
        let rows = stmt.query_map(params![separator], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
    }

    /// Give one waiting row its text, composed exactly the way [`Record::indexed_text`] composes a new one.
    ///
    /// The window title is part of the searchable body in this format — history is searched that way, and
    /// a fill that wrote only the recognised pixels would quietly drop every title-only match.
    pub fn fill_text(&mut self, rowid: i64, body: &str, title: Option<&str>) -> Result<(), StoreError> {
        let text = match title {
            Some(title) if !title.trim().is_empty() => format!("{body}{TITLE_SEPARATOR}{title}"),
            _ => body.to_string(),
        };
        self.conn.execute(
            "UPDATE video_text SET ocr_text = ?1 WHERE rowid = ?2",
            params![text, rowid],
        )?;
        Ok(())
    }

    /// Drop the rows a pass judged repeats of a neighbour.
    ///
    /// The caller deletes the files; this only keeps the index honest about what is no longer there, and
    /// does it in one transaction so a pass that dies halfway cannot leave rows pointing at frames it has
    /// already unlinked.
    pub fn delete_rows(&mut self, rowids: &[i64]) -> Result<usize, StoreError> {
        if rowids.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut removed = 0usize;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM video_text WHERE rowid = ?1")?;
            for rowid in rowids {
                removed += stmt.execute(params![rowid])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// The newest timestamp in the file, which is where the next segment should start looking.
    pub fn latest_time(&self) -> Result<Option<i64>, StoreError> {        Ok(self
            .conn
            .query_row("SELECT MAX(videofile_time) FROM video_text", [], |r| r.get::<_, Option<i64>>(0))?)
    }

    /// Which frames of one segment already have a row here.
    ///
    /// The recorder journals every accepted frame and replays a stranded journal when it starts after
    /// a hard kill, and the window between [`Store::append`] committing and that journal being
    /// deleted is a real one. `append` must stay a plain INSERT — the close path's row counts are a
    /// compatibility surface, not something recovery may quietly change — so dedup happens by
    /// looking, here, at the natural key: `videofile_name` selects the month file's slice and
    /// `picturefile_name` is unique within it, because `Segment::offer` refuses two frames in the
    /// same second and the frame name is that second.
    pub fn committed_pictures(&self, videofile_name: &str) -> Result<HashSet<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT picturefile_name FROM video_text WHERE videofile_name = ?1")?;
        let rows = stmt.query_map(params![videofile_name], |row| row.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }
}

/// The INSERT, generated from `COLUMNS` so that constant stays the single source of truth for the
/// positional order the rest of the ecosystem binds against.
fn insert_sql() -> String {
    let placeholders = (1..=COLUMNS.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
    format!("INSERT INTO video_text ({}) VALUES ({placeholders})", COLUMNS.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read;
    use std::time::Duration;

    fn record(t: i64) -> Record {
        Record {
            videofile_name: "2026-09-22_12-00-00.mp4".into(),
            picturefile_name: "2026-09-22_12-00-00.jpg".into(),
            videofile_time: t,
            ocr_text: "hello world".into(),
            win_title: Some("- Notepad".into()),
            deep_linking: None,
            thumbnail: Some("AAA".into()),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-write-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn title_is_appended_with_the_load_bearing_separator() {
        assert_eq!(record(1).indexed_text(), "hello world -||- - Notepad");
    }

    #[test]
    fn a_blank_title_adds_no_separator_debris() {
        let rec = Record { win_title: Some("   ".into()), ..record(1) };
        assert_eq!(rec.indexed_text(), "hello world");
    }

    #[test]
    fn column_order_is_the_wire_format() {
        assert_eq!(COLUMNS[2], "videofile_time");
        assert_eq!(COLUMNS[7], "win_title");
        assert_eq!(insert_sql(), "INSERT INTO video_text (videofile_name, picturefile_name, \
             videofile_time, ocr_text, is_videofile_exist, is_picturefile_exist, thumbnail, \
             win_title, deep_linking) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)");
    }

    #[test]
    fn videofile_time_is_stored_as_a_true_integer() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let mut store = Store { conn, path: PathBuf::from(":memory:") };
        store.append(&[record(1_762_310_400)]).unwrap();
        let kind: String = store
            .conn
            .query_row("SELECT typeof(videofile_time) FROM video_text", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "integer", "REAL here would break every BETWEEN slice the reader takes");
    }

    #[test]
    fn batch_append_is_atomic_and_counts_rows() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let mut store = Store { conn, path: PathBuf::from(":memory:") };
        let batch: Vec<Record> = (0..500).map(|i| record(1_762_310_400 + i)).collect();
        assert_eq!(store.append(&batch).unwrap(), 500);
        assert_eq!(store.row_count().unwrap(), 500);
        assert_eq!(store.append(&[]).unwrap(), 0);
        assert_eq!(store.latest_time().unwrap(), Some(1_762_310_899));
    }

    /// The natural key the recorder's replay dedups against: one segment's frames, by name.
    ///
    /// `append` stays a plain INSERT — a segment commit is a compatibility surface that must not gain
    /// a hidden read — so idempotency is a lookup the caller makes first. A frame name is unique
    /// inside a segment and a segment name is unique inside a month, which is what lets a replay that
    /// finds five of its own rows already present write the missing two and nothing more.
    #[test]
    fn one_segments_committed_frames_are_askable_by_name() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let mut store = Store { conn, path: PathBuf::from(":memory:") };
        assert!(store.committed_pictures("2026-09-22_12-00-00.mp4").unwrap().is_empty(), "nothing indexed yet");

        store.append(&[record(1_762_310_400)]).unwrap();
        let other = Record {
            videofile_name: "2026-09-22_12-15-00.mp4".into(),
            picturefile_name: "2026-09-22_12-15-00.jpg".into(),
            ..record(1_762_310_400)
        };
        store.append(&[other]).unwrap();

        assert_eq!(
            store.committed_pictures("2026-09-22_12-00-00.mp4").unwrap().into_iter().collect::<Vec<_>>(),
            vec!["2026-09-22_12-00-00.jpg".to_string()],
        );
        assert!(
            !store.committed_pictures("2026-09-22_12-15-00.mp4").unwrap().contains("2026-09-22_12-00-00.jpg"),
            "the lookup is scoped to one segment, so a sibling segment cannot answer for it"
        );
        assert!(store.committed_pictures("2026-09-22_13-00-00.mp4").unwrap().is_empty());
        assert_eq!(store.row_count().unwrap(), 2, "and asking changed nothing");
    }

    /// The contract the whole rewrite rests on: a file written here must be readable by the reader,
    /// carry the same month filename, and be openable by the Python app unchanged.
    #[test]
    fn a_written_month_file_is_discoverable_and_readable() {
        let dir = temp_dir("roundtrip");
        let mut store = Store::open_month(&dir, "default", 2026, 9).unwrap();
        assert_eq!(store.path().file_name().unwrap(), "default_2026-09_wind.db");
        store.append(&[record(1_790_025_372), record(1_790_025_400)]).unwrap();
        drop(store);

        let months = read::discover(&dir);
        assert_eq!(months.len(), 1);
        assert_eq!((months[0].year, months[0].month), (2026, 9));
        let conn = months[0].open_read(Duration::from_secs(300), false).unwrap();
        let rows = read::rows_in_window(&conn, None, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title(), Some("- Notepad"));
        assert_eq!(rows[0].body(), "hello world");
        assert_eq!(read::time_bounds(&conn).unwrap(), Some((1_790_025_372, 1_790_025_400)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The question the whole split exists to answer: does what the recorder commits come back out
    /// of the search? Written through `Store`, read through `discover` + `search_months`, with the
    /// temp-copy path in between — so a change to the writer's column order, its epoch convention or
    /// its title separator that breaks retrieval fails here rather than in a user's empty search box.
    #[test]
    fn a_recorded_segment_is_findable_by_the_search_that_reads_it_back() {
        use crate::read;
        use crate::search::{self, Query};
        use std::time::Duration;

        let dir = temp_dir("search-roundtrip");
        let base = wind_base::clock::LocalParts::from_stamp("2026-09-21_10-00-00")
            .unwrap()
            .naive_epoch_seconds();
        let rows = [
            record_at(base, "quarterly revenue report", "Excel"),
            record_at(base + 60, "shopping cart with socks", "Chrome"),
            record_at(base + 120, "季度收入报告 revenue", "Excel"),
        ];
        let mut store = Store::open_month(&dir, "default", 2026, 9).unwrap();
        store.append(&rows).unwrap();
        drop(store);

        let months = read::discover(&dir);
        let day_from = base - 3_600;
        let day_to = base + 3_600;

        let hits = search::search_months(
            &months,
            &Query::new(day_from, day_to).with_keywords("revenue"),
            Duration::from_secs(300),
            false,
        )
        .unwrap();
        assert_eq!(hits.total, 2, "both revenue screens, one in each language");
        assert_eq!(hits.rows[0].title(), Some("Excel"));

        // A Chinese token must survive the same round trip, which is the encoding contract.
        let cjk = search::search_months(
            &months,
            &Query::new(day_from, day_to).with_keywords("收入"),
            Duration::from_secs(300),
            false,
        )
        .unwrap();
        assert_eq!(cjk.total, 1);
        assert_eq!(cjk.rows[0].body(), "季度收入报告 revenue");

        // Paging across the merge, and an exclude term.
        let page = search::search_months(
            &months,
            &Query::new(day_from, day_to).page(2, 2),
            Duration::from_secs(300),
            false,
        )
        .unwrap();
        assert_eq!(page.rows.len(), 1, "three rows, page size two, second page has one");
        assert_eq!(page.total, 3);
        let filtered = search::search_months(
            &months,
            &Query::new(day_from, day_to).with_exclude("socks"),
            Duration::from_secs(300),
            false,
        )
        .unwrap();
        assert_eq!(filtered.total, 2);

        // A range that does not touch this month file routes to nothing, and says so cheaply.
        let autumn: Vec<_> = months
            .iter()
            .filter(|m| m.covers(base - 86_400 * 40, base - 86_400 * 30))
            .cloned()
            .collect();
        assert!(autumn.is_empty(), "a range a month outside must not route into this file");
        let away = search::search_months(
            &autumn,
            &Query::new(base - 86_400 * 40, base - 86_400 * 30),
            Duration::from_secs(300),
            false,
        )
        .unwrap();
        assert_eq!(away.total, 0);
        assert_eq!(away.rows.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn record_at(time: i64, text: &str, title: &str) -> Record {
        Record {
            videofile_name: format!("{}.mp4", wind_base::clock::LocalParts::from_naive_epoch(time).stamp()),
            picturefile_name: format!("{}.jpg", wind_base::clock::LocalParts::from_naive_epoch(time).stamp()),
            videofile_time: time,
            ocr_text: text.into(),
            win_title: Some(title.into()),
            deep_linking: None,
            thumbnail: Some("AAA".into()),
        }
    }

    /// A row written with no text is the state a named window puts the index in, and the three calls
    /// below are the whole of what the maintenance pass needs from the writer: find them, fill them in
    /// the same composed shape a live write uses, and drop the ones that turn out to repeat.
    #[test]
    fn a_row_written_without_text_is_found_then_filled_then_droppable() {
        let dir = temp_dir("pending");
        let mut store = Store::open_month(&dir, "default", 2026, 9).unwrap();
        store
            .append(&[
                record_at(1_790_000_001, "", "- Notepad"),
                record_at(1_790_000_002, "read at capture", "- Chrome"),
                record_at(1_790_000_000, "", "- Explorer"),
            ])
            .unwrap();

        let pending = store.pending_text().unwrap();
        assert_eq!(pending.len(), 2, "only the empty-text rows are waiting");
        assert!(
            pending.iter().all(|(_, video, picture, _)| video.ends_with(".mp4") && picture.ends_with(".jpg")),
            "a waiting row has to name both files the pass will look for"
        );
        assert_eq!(
            pending[0].3.as_deref(),
            Some("- Explorer"),
            "oldest first, so a pass that is cut short spent its work on the history closest to vanishing"
        );

        let (rowid, _, _, title) = &pending[0];
        let rowid = *rowid;
        store.fill_text(rowid, "the desktop", title.as_deref()).unwrap();
        let stored: String = store
            .connection()
            .query_row("SELECT ocr_text FROM video_text WHERE rowid = ?1", params![rowid], |row| row.get(0))
            .unwrap();
        assert_eq!(
            stored,
            format!("the desktop{TITLE_SEPARATOR}- Explorer"),
            "a filled row keeps the title in the body, exactly as a live-written row does"
        );
        assert_eq!(store.pending_text().unwrap().len(), 1, "and it stops waiting");

        assert_eq!(store.delete_rows(&[rowid]).unwrap(), 1);
        assert_eq!(store.row_count().unwrap(), 2);
        assert_eq!(store.delete_rows(&[]).unwrap(), 0, "nothing asked for, nothing removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
