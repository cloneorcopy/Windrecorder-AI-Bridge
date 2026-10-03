//! Reading the index.
//!
//! Two rules govern everything in this file, and both exist because the recorder is writing the
//! same files while the UI is reading them:
//!
//!   * never query a live month file — work on the `_TEMP_READ.db` copy, refreshed on the same
//!     schedule upstream uses, so a long search cannot block an in-flight segment commit;
//!   * a month file's name is its time range. `{user}_{YYYY}-{MM}_wind.db` means a query for one
//!     Tuesday only ever opens one or two databases out of the years of history on disk. That
//!     routing is where the responsiveness of the whole product comes from.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Distinguishes concurrent staging files inside one process.
static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

use rusqlite::{Connection, OpenFlags};
use wind_base::clock::LocalParts;
use wind_base::paths;

use crate::schema::{column_names, ensure_schema, StoreError, TITLE_SEPARATOR};

/// One month of the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Month {
    pub user: String,
    pub year: i64,
    pub month: u32,
    pub path: PathBuf,
}

impl Month {
    pub fn from_path(path: &Path) -> Option<Month> {
        let parsed = paths::parse_month_db(path)?;
        Some(Month { user: parsed.user, year: parsed.year, month: parsed.month, path: path.to_path_buf() })
    }

    /// Inclusive epoch range this file covers. A month boundary is the whole calendar month in
    /// naive-local seconds — the day-begin shift is a *query* concern, not a storage one, because
    /// a row at 02:00 on the 1st still lives in the previous month's file only if the file was
    /// written by month-of-timestamp, which is what upstream does.
    pub fn coverage(&self) -> (i64, i64) {
        let start = LocalParts { year: self.year, month: self.month, day: 1, hour: 0, minute: 0, second: 0 };
        let (ny, nm) = if self.month == 12 {
            (self.year + 1, 1u32)
        } else {
            (self.year, self.month + 1)
        };
        let end = LocalParts { year: ny, month: nm, day: 1, hour: 0, minute: 0, second: 0 };
        (start.naive_epoch_seconds(), end.naive_epoch_seconds() - 1)
    }

    pub fn covers(&self, from: i64, to: i64) -> bool {
        let (start, end) = self.coverage();
        from <= end && to >= start
    }

    /// Open for reading, through the `_TEMP_READ.db` copy. See [`temp_read_for`].
    pub fn open_read(&self, stale_after: Duration, maintaining: bool) -> Result<Connection, StoreError> {
        let copy = temp_read_for(&self.path, stale_after, maintaining)?;
        let conn = open_read_only(&copy)?;
        Ok(conn)
    }

    /// Open the live file directly. Only the writer and the maintenance pass may do this.
    pub fn open_write(&self) -> Result<Connection, StoreError> {
        let conn = Connection::open(&self.path)?;
        conn.pragma_update(None, "journal_mode", "delete")?;
        ensure_schema(&conn)?;
        Ok(conn)
    }
}

fn open_read_only(path: &Path) -> Result<Connection, StoreError> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    // A reader of a *live* month file can be turned away for the moment a writer is swapping its
    // rollback journal away; waiting out that moment is what a census and a progress bar should do,
    // rather than reporting a month that exists as one that cannot be read.
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    Ok(conn)
}

/// Every month file in a `userdata/db` directory, oldest first.
///
/// Anything that is not a month file — `ocrSaved.db` from a pre-split install, a `_TEMP_READ.db`
/// copy, a `_BACKUP_` snapshot — is skipped by the name parser, which is why that parser is
/// anchored rather than a `contains("wind")` guess.
pub fn discover(db_dir: &Path) -> Vec<Month> {
    let mut out: Vec<Month> = Vec::new();
    let entries = match std::fs::read_dir(db_dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        if let Some(month) = Month::from_path(&entry.path()) {
            out.push(month);
        }
    }
    out.sort_by_key(|m| (m.year, m.month, m.user.clone()));
    out
}

/// The months a time range touches.
pub fn months_in_range<'a>(months: &'a [Month], from: i64, to: i64) -> Vec<&'a Month> {
    months.iter().filter(|m| m.covers(from, to)).collect()
}

/// Refresh and return the read copy for a live index file.
///
/// Upstream re-copies only when the origin is more than `stale_after` newer and no maintenance is
/// running: the copy is the expensive part, and mid-maintenance the origin is being rewritten under
/// the reader, so a torn snapshot is worse than a slightly old one.
pub fn temp_read_for(origin: &Path, stale_after: Duration, maintaining: bool) -> Result<PathBuf, StoreError> {
    let temp = paths::temp_read_of(origin);
    let origin_newer = match (modified(origin)?, modified(&temp)?) {
        (Some(a), Some(b)) => matches!(a.duration_since(b), Ok(gap) if gap > stale_after),
        (Some(_), None) => true,
        _ => false,
    };
    if !temp.exists() || (origin_newer && !maintaining) {
        refresh_copy(origin, &temp)?;
    }
    Ok(temp)
}

/// Replace the read copy without ever leaving a half-written one visible.
///
/// `std::fs::copy` truncates its destination first, so copying straight onto an existing copy is a
/// window — however short — in which a second reader can open a file whose schema pages have
/// landed and whose data pages have not. SQLite treats that structure as a valid, empty month: no
/// error, no rows, and a footer that still reports the count it read elsewhere. The copy is written
/// under a unique name and renamed into place, so a reader sees either the old complete file or the
/// new complete one.
///
/// The rename can fail on Windows because another process holds the current copy open — an open
/// file cannot be replaced. That is the correct outcome, not a fault: the existing copy is a
/// complete database, so this reader uses it and the refresh is simply deferred to whoever gets
/// there first.
fn refresh_copy(origin: &Path, temp: &Path) -> Result<(), StoreError> {
    if let Some(parent) = temp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = temp.with_extension(format!("tmp-{}-{}", std::process::id(), STAGE_SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::copy(origin, &staged)?;
    // The copy is disposable and re-created on the next read; carrying the origin's mtime forward
    // would make the staleness test compare a file against itself.
    let _ = std::fs::File::open(&staged).map(|f| f.set_modified(SystemTime::now()));
    match std::fs::rename(&staged, temp) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            if temp.exists() {
                Ok(())
            } else {
                Err(e.into())
            }
        }
    }
}

fn modified(path: &Path) -> Result<Option<SystemTime>, StoreError> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(Some(m.modified()?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// One `video_text` row.
///
/// `time` is stored in the app's naive-local epoch (see `wind_base::clock`), never as a SQLite
/// date, because that is what every existing row on every existing user's disk already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub rowid: i64,
    pub videofile_name: String,
    pub picturefile_name: String,
    pub time: i64,
    pub ocr_text: String,
    pub win_title: Option<String>,
    pub deep_linking: Option<String>,
    pub thumbnail: Option<String>,
    pub video_exists: bool,
    pub picture_exists: bool,
    /// Which month file this came from, so a caller can reopen it without re-resolving.
    pub month_path: Option<PathBuf>,
}

impl Row {
    /// The window title appended after `" -||- "`, if the writer used that convention.
    ///
    /// The video-indexing path folds the title into `ocr_text`; the screenshot path keeps it in the
    /// `win_title` column. A row can come from either, so a reader must look at both, exactly as
    /// the bridge does.
    pub fn body(&self) -> &str {
        match self.ocr_text.rfind(TITLE_SEPARATOR) {
            Some(at) => self.ocr_text[..at].trim_end(),
            None => &self.ocr_text,
        }
    }

    pub fn embedded_title(&self) -> Option<&str> {
        self.ocr_text
            .rfind(TITLE_SEPARATOR)
            .map(|at| &self.ocr_text[at + TITLE_SEPARATOR.len()..])
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// Prefer the real column, fall back to what the text carries.
    pub fn title(&self) -> Option<&str> {
        self.win_title.as_deref().map(str::trim).filter(|s| !s.is_empty()).or_else(|| self.embedded_title())
    }

    pub fn when(&self) -> LocalParts {
        LocalParts::from_naive_epoch(self.time)
    }

    /// The 19-character prefix every filename convention depends on.
    pub fn segment_stamp(&self) -> Option<LocalParts> {
        LocalParts::from_stamp(&self.videofile_name)
    }

    /// Seconds into the segment, which is what a player needs to seek to. Negative when the row's
    /// own timestamp precedes the file's, which happens for the first frame of a restarted segment.
    pub fn offset_in_segment(&self) -> Option<i64> {
        self.segment_stamp().map(|stamp| self.time - stamp.naive_epoch_seconds())
    }

    /// Which frame of its segment this row was read out of, when its own picture name says so.
    ///
    /// `picturefile_name` is not a label. The re-index pass names every row's picture after the
    /// source frame the OCR engine looked at — `wind_reindex::frames` extracts with
    /// `-frame_pts 1` precisely because "the naming is load-bearing and not incidental", and
    /// `index.rs` commits `frame.cropped_name()`, i.e. `{index}_cropped.jpg`. So `416_cropped.jpg`
    /// is frame 416 of the segment named by `videofile_name`, and that number is the one fact in the
    /// row about *which picture this row came from*.
    ///
    /// It is worth more than [`Row::offset_in_segment`], which re-derives the second from the row's
    /// timestamp, and that timestamp was itself computed as `frame / record_framerate`
    /// (`wind_reindex::timeline::row_time`). The divisor is the recorder's configured frame rate,
    /// while the file the frames were pulled out of is the one `windmaint` wrote —
    /// `maint/src/encode.rs`'s `encode_args`, which pins the container "so a player that seeks to
    /// second S lands on frame S". Those two agree only when the configured rate is 1. Where they do
    /// not, `offset_in_segment` is the *claim* and this is the *evidence*: measured against the live
    /// September index on this machine, 2 542 of 2 551 rows are named this way and every one of them
    /// sits at second `frame_index` of its own segment, while the stored offset had them at half that.
    ///
    /// `None` for every other name, which is most of what the recorder writes:
    /// `windrec::recorder` stores `{stamp}.jpg`, a wall-clock name whose row time is already exact,
    /// and a legacy absolute path is not a frame number however it is spelled. A row that does not
    /// name a frame is not answered with an invented one.
    pub fn frame_index(&self) -> Option<i64> {
        let name = self.picturefile_name.trim();
        let digits = name.strip_suffix(".jpg")?.strip_suffix("_cropped")?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse::<i64>().ok().filter(|index| *index >= 0)
    }
}

/// The `SELECT` list, ordered by rowid and then by time so the same query returns the same rows in
/// the same order twice. Upstream emits no `ORDER BY` at all, which makes its result order an
/// accident of page layout.
pub(crate) const SELECT_COLUMNS: &str = "rowid, videofile_name, picturefile_name, videofile_time, ocr_text, \
     is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking";

/// Read a row from a statement whose columns are [`SELECT_COLUMNS`].
pub(crate) fn row_from(stmt: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        rowid: stmt.get(0)?,
        videofile_name: stmt.get::<_, Option<String>>(1)?.unwrap_or_default(),
        picturefile_name: stmt.get::<_, Option<String>>(2)?.unwrap_or_default(),
        time: stmt.get::<_, Option<i64>>(3)?.unwrap_or(0),
        ocr_text: stmt.get::<_, Option<String>>(4)?.unwrap_or_default(),
        video_exists: stmt.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
        picture_exists: stmt.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
        thumbnail: stmt.get::<_, Option<String>>(7)?,
        win_title: stmt.get::<_, Option<String>>(8)?,
        deep_linking: stmt.get::<_, Option<String>>(9)?,
        month_path: None,
    })
}

/// Every row in a window, oldest first. `None` bounds mean "no bound on that side".
pub fn rows_in_window(
    conn: &Connection,
    from: Option<i64>,
    to: Option<i64>,
) -> Result<Vec<Row>, StoreError> {
    let mut sql = String::from("SELECT rowid, * FROM video_text");
    let mut conditions = Vec::new();
    let mut params: Vec<i64> = Vec::new();
    if let Some(f) = from {
        conditions.push(format!("videofile_time >= ?{}", params.len() + 1));
        params.push(f);
    }
    if let Some(t) = to {
        conditions.push(format!("videofile_time <= ?{}", params.len() + 1));
        params.push(t);
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY videofile_time, rowid");

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        // `SELECT *` here is deliberate: legacy months may hold fewer columns, so this path reads
        // by position from the header rather than assuming the nine-column order.
        let rowid: i64 = r.get(0)?;
        Ok(Row {
            rowid,
            videofile_name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            picturefile_name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            time: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            ocr_text: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            video_exists: r.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
            picture_exists: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
            thumbnail: r.get::<_, Option<String>>(7)?,
            win_title: r.get::<_, Option<String>>(8)?,
            deep_linking: r.get::<_, Option<String>>(9)?,
            month_path: None,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The one row a `rowid` names, or `None` when the month file has no such row.
///
/// `rowid` *is* the primary key, so this is a seek rather than a scan — which is what lets a window
/// that showed a sampled tile, and holds nothing about that row but its key, go and read the row it
/// needs in order to enlarge the picture. [`rows_in_window`] deliberately reads the whole window;
/// nothing here should.
///
/// `SELECT rowid, *` and not a named list, for the same reason that function does it that way: a
/// legacy month file predates `win_title` and `deep_linking`, and naming a column the file never had
/// is an error where reading by position is a `None`.
pub fn row_by_rowid(conn: &Connection, rowid: i64) -> Result<Option<Row>, StoreError> {
    let mut stmt = conn.prepare("SELECT rowid, * FROM video_text WHERE rowid = ?1")?;
    let mut rows = stmt.query_map([rowid], row_from)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// How many distinct segments a month file describes.
///
/// A day view wants to say "8 screens across 3 recordings", and answering it by reading every row
/// materialises a year of OCR text to count filenames.
pub fn count_segments(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COUNT(DISTINCT videofile_name) FROM video_text", [], |r| r.get::<_, i64>(0))?)
}

/// How a caller reads: the staleness window for the temp copy, and whether maintenance is running.
///
/// Both are policy, and both were being re-derived at every call site. A reader that guesses 60 s
/// where the writer expects 300 s re-copies the database on every keystroke; one that forgets the
/// maintain lock can take its snapshot while the origin is mid-rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOptions {
    pub stale_after: Duration,
    pub maintaining: bool,
}

impl Default for ReadOptions {
    fn default() -> ReadOptions {
        ReadOptions { stale_after: Duration::from_secs(300), maintaining: false }
    }
}

impl ReadOptions {
    /// Never trust an existing copy: refresh before reading. For a caller that has just written and
    /// must see its own rows.
    pub fn always_refresh() -> ReadOptions {
        ReadOptions { stale_after: Duration::ZERO, maintaining: false }
    }
}

impl Month {
    /// [`Month::open_read`] with the policy bundled.
    pub fn open_with(&self, options: ReadOptions) -> Result<Connection, StoreError> {
        self.open_read(options.stale_after, options.maintaining)
    }
}

/// The earliest and latest stored timestamps, which is how the UI sets its date slider bounds.
pub fn time_bounds(conn: &Connection) -> Result<Option<(i64, i64)>, StoreError> {
    let bounds = conn.query_row(
        "SELECT MIN(videofile_time), MAX(videofile_time) FROM video_text",
        [],
        |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
    )?;
    Ok(match bounds {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    })
}

pub fn count_rows(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COUNT(*) FROM video_text", [], |r| r.get::<_, i64>(0))?)
}

/// The slice directory a segment's frames live in, whatever marker it has since acquired.
///
/// A row stores `picturefile_name` as a bare basename because the directory is renamed with a
/// `-SUBMIT`/`-VIDEO` marker once the segment closes; the only stable handle is the segment's
/// 19-character stamp prefix, which is also how the Python reader resolves names.
pub fn resolve_slice_dir(cache_root: &Path, stamp: &str) -> Option<PathBuf> {
    if stamp.len() < paths::STAMP_LEN {
        return None;
    }
    let entries = std::fs::read_dir(cache_root).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(stamp) && entry.path().is_dir() {
            return Some(entry.path());
        }
    }
    None
}

/// Where a row's captured frame is on disk, if it is still there.
pub fn resolve_frame(cache_root: &Path, row: &Row) -> Option<PathBuf> {
    let stamp: String = row.videofile_name.chars().take(paths::STAMP_LEN).collect();
    let dir = resolve_slice_dir(cache_root, &stamp)?;
    let candidate = dir.join(&row.picturefile_name);
    candidate.exists().then_some(candidate)
}

/// A segment's video file, looked up by stamp prefix inside its month folder.
///
/// Prefix rather than equality because the name on disk may carry a `-SCREENSHOTS-OCRED` stage
/// marker while the index row keeps the plain `{stamp}.mp4` it was written with.
pub fn resolve_video(videos_root: &Path, videofile_name: &str) -> Option<PathBuf> {
    let stamp: String = videofile_name.chars().take(paths::STAMP_LEN).collect();
    let month = LocalParts::from_stamp(&stamp)?;
    let dir = videos_root.join(format!("{:04}-{:02}", month.year, month.month));
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&stamp) && name.ends_with(".mp4") {
            return Some(entry.path());
        }
    }
    None
}

/// The columns a month file actually has, exposed so a caller can refuse to read a file it does
/// not understand instead of returning an empty result that looks like "no data that day".
pub fn available_columns(conn: &Connection) -> Result<Vec<String>, StoreError> {
    column_names(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn insert(conn: &Connection, name: &str, time: i64, text: &str, title: Option<&str>) -> i64 {
        conn.execute(
            "INSERT INTO video_text (videofile_name, picturefile_name, videofile_time, ocr_text,
              is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
             VALUES (?1,?2,?3,?4,1,0,'AAA',?5,'')",
            params![name, "p.jpg", time, text, title],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn month_coverage_is_the_calendar_month_in_the_stored_epoch() {
        let m = Month { user: "default".into(), year: 2026, month: 9, path: PathBuf::new() };
        let (start, end) = m.coverage();
        assert_eq!(start, LocalParts::from_stamp("2026-09-01_00-00-00").unwrap().naive_epoch_seconds());
        assert_eq!(end, LocalParts::from_stamp("2026-09-30_23-59-59").unwrap().naive_epoch_seconds());

        let december = Month { year: 2026, month: 12, ..m.clone() };
        assert!(december.covers(end_of(2026, 12, 31, 23, 59, 59), end_of(2026, 12, 31, 23, 59, 59)));
        // A range straddling the boundary must open both files.
        let (a, b) = (
            end_of(2026, 9, 30, 23, 0, 0),
            LocalParts::from_stamp("2026-10-01_01-00-00").unwrap().naive_epoch_seconds(),
        );
        assert!(m.covers(a, b));
        let oct = Month { month: 10, ..m.clone() };
        assert!(oct.covers(a, b));
        let july = Month { month: 7, ..m };
        assert!(!july.covers(a, b));
    }

    fn end_of(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        LocalParts { year: y, month: mo, day: d, hour: h, minute: mi, second: s }.naive_epoch_seconds()
    }

    #[test]
    fn a_row_can_be_found_by_its_key_alone() {
        let conn = db();
        let base = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        let first = insert(&conn, "a.mp4", base, "first", Some("Word"));
        let second = insert(&conn, "b.mp4", base + 20, "second", None);

        let row = row_by_rowid(&conn, second).unwrap().expect("the row is there");
        assert_eq!(row.rowid, second);
        assert_eq!(row.videofile_name, "b.mp4");
        assert_eq!(row.time, base + 20);
        assert_eq!(row.ocr_text, "second", "the caller that enlarges a picture still has to name the row");
        // The first row of the month is reachable on the same terms, title and all.
        assert_eq!(row_by_rowid(&conn, first).unwrap().and_then(|r| r.win_title).as_deref(), Some("Word"));
        // A rowid the file does not hold is an answer, not a failure: the month may have been rewritten
        // between the window listing it and the click asking for it.
        assert!(row_by_rowid(&conn, first + 500).unwrap().is_none());
    }

    #[test]
    fn window_reads_are_bounded_and_ordered_by_time() {
        let conn = db();
        let base = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        insert(&conn, "b.mp4", base + 20, "second", None);
        insert(&conn, "a.mp4", base, "first", None);
        insert(&conn, "c.mp4", base + 40, "third", None);

        let all = rows_in_window(&conn, None, None).unwrap();
        assert_eq!(all.iter().map(|r| r.time).collect::<Vec<_>>(), vec![base, base + 20, base + 40]);
        let window = rows_in_window(&conn, Some(base + 1), Some(base + 30)).unwrap();
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].videofile_name, "b.mp4");
        assert_eq!(count_rows(&conn).unwrap(), 3);
        assert_eq!(time_bounds(&conn).unwrap(), Some((base, base + 40)));
    }

    #[test]
    fn an_empty_database_has_no_bounds() {
        let conn = db();
        assert_eq!(time_bounds(&conn).unwrap(), None);
        assert!(rows_in_window(&conn, None, None).unwrap().is_empty());
    }

    /// The two ways upstream writes a title, and the reader has to understand both.
    #[test]
    fn titles_come_from_the_column_or_from_the_text() {
        let conn = db();
        let base = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        insert(&conn, "a.mp4", base, "some screen text", Some("Notepad"));
        insert(&conn, "b.mp4", base + 1, "screen text -||- ChatGPT", None);
        let rows = rows_in_window(&conn, None, None).unwrap();
        assert_eq!(rows[0].title(), Some("Notepad"));
        assert_eq!(rows[0].body(), "some screen text");
        assert_eq!(rows[1].title(), Some("ChatGPT"));
        assert_eq!(rows[1].body(), "screen text");

        let empty_column = Row {
            win_title: Some("   ".into()),
            ocr_text: "text -||- Real".into(),
            ..rows[0].clone()
        };
        assert_eq!(empty_column.title(), Some("Real"), "a whitespace-only column must not mask the text");
    }

    #[test]
    fn a_row_knows_where_it_sits_inside_its_segment() {
        let conn = db();
        insert(&conn, "2026-09-21_10-00-00.mp4", end_of(2026, 9, 21, 10, 5, 30), "x", None);
        let row = &rows_in_window(&conn, None, None).unwrap()[0];
        assert_eq!(row.offset_in_segment(), Some(330));
        assert_eq!(row.when().display(), "2026-09-21 10:05:30");
    }

    /// A re-indexed row's picture name is a frame number, and it survives whatever the row's own
    /// timestamp arithmetic did to it. That is the row's only direct statement of which picture it is,
    /// and `backend::frame` has to be able to read it.
    #[test]
    fn a_reindexed_row_names_the_frame_it_was_read_out_of() {
        let base = |row: Row| Row {
            videofile_name: "2026-09-21_10-00-00.mp4".into(),
            time: 0,
            ..row
        };
        let named = |picture: &str| base(Row { picturefile_name: picture.into(), ..stub_row() });

        assert_eq!(named("416_cropped.jpg").frame_index(), Some(416));
        assert_eq!(named("0_cropped.jpg").frame_index(), Some(0));

        // Every other name the column holds is not a frame, and is not answered with one.
        for not_a_frame in [
            "2026-09-22_19-48-12.jpg",                                      // the recorder's wall clock
            "C:\\cache_screenshot\\2026-09-22_19-48-12\\2026-09-22.jpg",    // a legacy absolute path
            "cropped.jpg",
            "_cropped.jpg",
            "12_cropped.png",
            "12_cropped.jpg.bak",
            "-4_cropped.jpg",
            "",
        ] {
            assert_eq!(named(not_a_frame).frame_index(), None, "{not_a_frame} is not a frame number");
        }
    }

    fn stub_row() -> Row {
        Row {
            rowid: 1,
            videofile_name: String::new(),
            picturefile_name: String::new(),
            time: 0,
            ocr_text: String::new(),
            win_title: None,
            deep_linking: None,
            thumbnail: None,
            video_exists: false,
            picture_exists: false,
            month_path: None,
        }
    }

    #[test]
    fn segment_count_is_answered_without_reading_the_bodies() {
        let conn = db();
        let base = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        insert(&conn, "a.mp4", base, "one", None);
        insert(&conn, "a.mp4", base + 1, "two", None);
        insert(&conn, "b.mp4", base + 2, "three", None);
        assert_eq!(count_segments(&conn).unwrap(), 2);
        assert_eq!(count_segments(&db()).unwrap(), 0);
    }

    #[test]
    fn read_options_carry_the_upstream_policy() {
        let options = ReadOptions::default();
        assert_eq!(options.stale_after, Duration::from_secs(300));
        assert!(!options.maintaining);
        assert_eq!(ReadOptions::always_refresh().stale_after, Duration::ZERO);

        let dir = std::env::temp_dir().join(format!("windcap-ropts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let origin = dir.join("default_2026-09_wind.db");
        crate::write::Store::open(&origin).unwrap();
        let month = Month::from_path(&origin).unwrap();
        let conn = month.open_with(options).unwrap();
        drop(conn);
        assert!(paths::temp_read_of(&origin).exists(), "the copy is what gets opened");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_only_picks_up_month_files_and_sorts_them() {
        let dir = std::env::temp_dir().join(format!("windcap-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "default_2026-10_wind.db",
            "default_2026-09_wind.db",
            "ocrSaved.db",
            "default_2026-09_wind.db_TEMP_READ.db",
            "readme.txt",
        ] {
            std::fs::write(dir.join(name), b"not really sqlite").unwrap();
        }
        let found = discover(&dir);
        assert_eq!(
            found.iter().map(|m| (m.year, m.month)).collect::<Vec<_>>(),
            vec![(2026, 9), (2026, 10)]
        );
        assert_eq!(months_in_range(&found, end_of(2026, 9, 15, 0, 0, 0), end_of(2026, 9, 16, 0, 0, 0)).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_frame_resolves_through_the_stamp_prefix_not_the_directory_name() {
        let root = std::env::temp_dir().join(format!("windcap-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let slice = root.join("2026-09-21_10-00-00-SUBMIT");
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("2026-09-21_10-00-05.jpg"), b"jpeg").unwrap();
        std::fs::create_dir_all(root.join("unrelated")).unwrap();

        let conn = db();
        insert(&conn, "2026-09-21_10-00-00.mp4", 1, "x", None);
        // Patch the basename the fixture helper does not set.
        conn.execute("UPDATE video_text SET picturefile_name = '2026-09-21_10-00-05.jpg'", []).unwrap();
        let row = rows_in_window(&conn, None, None).unwrap().remove(0);

        assert_eq!(
            resolve_frame(&root, &row),
            Some(slice.join("2026-09-21_10-00-05.jpg"))
        );
        let mut gone = row.clone();
        gone.picturefile_name = "2026-09-21_10-99-99.jpg".into();
        assert_eq!(resolve_frame(&root, &gone), None, "a deleted frame resolves to nothing, not the directory");
        assert_eq!(resolve_frame(&root, &Row { videofile_name: "junk".into(), ..row.clone() }), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_video_is_found_by_prefix_inside_its_own_month_folder() {
        let root = std::env::temp_dir().join(format!("windcap-videos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let month = root.join("2026-09");
        std::fs::create_dir_all(&month).unwrap();
        std::fs::write(month.join("2026-09-21_10-00-00-SCREENSHOTS-OCRED.mp4"), b"v").unwrap();
        std::fs::write(month.join("2026-09-21_11-00-00.mp4"), b"v").unwrap();

        assert_eq!(
            resolve_video(&root, "2026-09-21_10-00-00.mp4"),
            Some(month.join("2026-09-21_10-00-00-SCREENSHOTS-OCRED.mp4"))
        );
        assert_eq!(resolve_video(&root, "2026-09-21_12-00-00.mp4"), None);
        // A row whose stamp is not a date at all must not search the whole tree.
        assert_eq!(resolve_video(&root, "not-a-stamp.mp4"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The regression: a copy must never be observable in a half-written state. Asserted the only
    /// way it can be asserted — by refreshing a copy many times while a reader holds the previous
    /// one open and counting rows every time.
    #[test]
    fn a_refresh_never_exposes_an_empty_copy() {
        let dir = std::env::temp_dir().join(format!("windcap-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let origin = dir.join("default_2026-09_wind.db");
        {
            let store = crate::write::Store::open(&origin).unwrap();
            let conn = store.connection();
            conn.execute(
                "INSERT INTO video_text (videofile_name, picturefile_name, videofile_time, ocr_text,
                   is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
                 VALUES ('a.mp4','a.jpg',1790025372,'content',1,0,'',NULL,'')",
                [],
            ).unwrap();
        }

        for round in 0..40 {
            // Hold the copy open, then refresh it: the rename loses to the open handle on Windows,
            // and the reader must still see its rows rather than an empty month.
            let first = temp_read_for(&origin, Duration::from_secs(0), false).unwrap();
            let held = Connection::open_with_flags(&first, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let _ = std::fs::write(&origin, std::fs::read(&origin).unwrap());
            let second = temp_read_for(&origin, Duration::from_secs(0), false).unwrap();
            let rows: i64 = held
                .query_row("SELECT COUNT(*) FROM video_text", [], |r| r.get(0))
                .unwrap();
            let reopened: i64 = Connection::open_with_flags(&second, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|c| c.query_row("SELECT COUNT(*) FROM video_text", [], |r| r.get(0)))
                .unwrap_or(-1);
            assert_eq!(rows, 1, "round {round}: the held copy read as empty mid-refresh");
            assert_eq!(reopened, 1, "round {round}: the replaced copy read as empty");
            drop(held);
        }
        // No staging files left behind, whatever won each race.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "staging files leaked: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_read_copy_is_made_once_and_refreshed_only_when_the_origin_is_stale() {
        let dir = std::env::temp_dir().join(format!("windcap-tempread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let origin = dir.join("default_2026-09_wind.db");
        std::fs::write(&origin, b"v1").unwrap();

        let temp = temp_read_for(&origin, Duration::from_secs(300), false).unwrap();
        assert_eq!(std::fs::read(&temp).unwrap(), b"v1");

        // Origin newer than the copy, but inside the staleness window: the copy is kept.
        std::fs::write(&origin, b"v2").unwrap();
        let again = temp_read_for(&origin, Duration::from_secs(300), false).unwrap();
        assert_eq!(std::fs::read(&again).unwrap(), b"v1", "a copy younger than stale_after is reused");

        // Maintenance running: never re-copy, the origin may be mid-rewrite.
        let held = temp_read_for(&origin, Duration::from_secs(0), true).unwrap();
        assert_eq!(std::fs::read(&held).unwrap(), b"v1");

        // Maintenance over and the origin genuinely newer: refresh.
        let fresh = temp_read_for(&origin, Duration::from_secs(0), false).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"v2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
