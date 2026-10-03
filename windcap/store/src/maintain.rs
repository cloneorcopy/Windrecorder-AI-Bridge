//! The statements the maintenance pass runs against a live index.
//!
//! Upstream performs this work with pandas: it reads an entire month into a DataFrame, recomputes
//! every `is_videofile_exist` flag in Python, then writes the flags back one row at a time. On a
//! month with 40 000 rows that is 40 000 statements plus a full-table materialisation, which is why
//! the idle sweep used to take minutes and why it was left switched off. Here the same corrections
//! are a handful of grouped `UPDATE`s and one transaction.
//!
//! These functions take a `&Connection` rather than a path: the caller owns the file lock, decides
//! when the write is allowed, and passes the result of its own filesystem scan into `exists`.

use std::path::Path;

use rusqlite::{params, Connection, Transaction};

use crate::read::Row;
use crate::schema::StoreError;

/// What a maintenance pass changed, so the run can be reported and a no-op stays visibly a no-op.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub rows_scanned: usize,
    pub existence_corrected: usize,
    pub duplicates_dropped: usize,
    pub rows_deleted: usize,
    /// Index names created by [`ensure_time_index`], which is a schema change and worth reporting.
    pub indexes_created: Vec<String>,
}

/// A plan for one month: which rows to touch and why. Built first, applied second, so a caller can
/// log or dry-run the whole thing before it writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenancePlan {
    /// `videofile_name` -> whether the file is still on disk.
    pub video_existence: Vec<(String, bool)>,
    /// `picturefile_name` -> whether the frame is still on disk.
    pub picture_existence: Vec<(String, bool)>,
    pub duplicate_rowids: Vec<i64>,
}

/// An index on `videofile_time`.
///
/// Upstream creates no indexes at all — every day view, timeline sample and search is a full scan of
/// the month. A plain index on the timestamp column turns the day-scoped queries into a seek, and
/// it is additive: a reader that does not know about it is unaffected, so this is safe to run
/// against a database the Python app still opens.
pub const TIME_INDEX: &str = "CREATE INDEX IF NOT EXISTS video_text_time ON video_text (videofile_time)";

/// Is the timestamp index present? Read-only, so a caller can report the state of an index it has
/// no business changing: a search screen must not write schema onto a live recording.
pub fn time_index_present(conn: &Connection) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='video_text_time'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// Create the timestamp index if it is missing. Returns true when it was just created.
pub fn ensure_time_index(conn: &Connection) -> Result<bool, StoreError> {
    let existed = time_index_present(conn)?;
    if !existed {
        conn.execute_batch(TIME_INDEX)?;
    }
    Ok(!existed)
}

/// Fold a scanned directory listing into the existence corrections for the rows at hand.
///
/// `exists` is the caller's answer to "is this file still there", deliberately a closure: the
/// recorder's own `-VIDEO`/`-OCRED` naming, the monthly folder layout and the recycle-bin setting all
/// belong to whoever owns the files, while this function owns only the "one decision per distinct
/// name, one statement per name that changed" part — which is where the speed comes from.
pub fn plan_existence(
    rows: &[Row],
    video_exists: &dyn Fn(&str) -> bool,
    picture_exists: &dyn Fn(&str) -> bool,
) -> MaintenancePlan {
    let mut plan = MaintenancePlan::default();
    let mut seen_video: Vec<(String, bool)> = Vec::new();
    let mut seen_picture: Vec<(String, bool)> = Vec::new();

    for row in rows {
        if let Some(slot) = seen_video.iter_mut().find(|(name, _)| *name == row.videofile_name) {
            let _ = slot;
        } else {
            let value = video_exists(&row.videofile_name);
            seen_video.push((row.videofile_name.clone(), value));
        }
        if !row.picturefile_name.is_empty() && !seen_picture.iter().any(|(name, _)| *name == row.picturefile_name) {
            let value = picture_exists(&row.picturefile_name);
            seen_picture.push((row.picturefile_name.clone(), value));
        }
    }

    // Only emit a change where the stored flag disagrees with reality; rewriting a correct flag is
    // what made the old pass take an exclusive lock for no reason.
    for row in rows {
        if let Some((_, value)) = seen_video.iter().find(|(name, _)| *name == row.videofile_name) {
            if *value != row.video_exists && !plan.video_existence.iter().any(|(n, _)| *n == row.videofile_name) {
                plan.video_existence.push((row.videofile_name.clone(), *value));
            }
        }
        if let Some((_, value)) = seen_picture.iter().find(|(name, _)| *name == row.picturefile_name) {
            if *value != row.picture_exists
                && !plan.picture_existence.iter().any(|(n, _)| *n == row.picturefile_name)
            {
                plan.picture_existence.push((row.picturefile_name.clone(), *value));
            }
        }
    }
    plan
}

/// Apply an existence plan in one transaction. Returns the number of rows the database changed.
pub fn apply_existence(tx: &Transaction<'_>, plan: &MaintenancePlan) -> Result<usize, StoreError> {
    let mut changed = 0usize;
    // `prepare_cached` outside the loop: this is the same two statements hundreds of times, and
    // compiling the SQL per row is a cost upstream pays on every bind.
    {
        let mut stmt = tx.prepare_cached("UPDATE video_text SET is_videofile_exist = ? WHERE videofile_name = ?")?;
        for (name, exists) in &plan.video_existence {
            changed += stmt.execute(params![*exists, name])?;
        }
    }
    {
        let mut stmt =
            tx.prepare_cached("UPDATE video_text SET is_picturefile_exist = ? WHERE picturefile_name = ?")?;
        for (name, exists) in &plan.picture_existence {
            changed += stmt.execute(params![*exists, name])?;
        }
    }
    Ok(changed)
}

/// One row's redrawn preview, in the form the caller collected and this module writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbWrite {
    pub rowid: i64,
    pub thumbnail: String,
}

/// Rewrite `video_text.thumbnail` for the rows a preview pass drew, in the caller's transaction.
///
/// The column is the whole picture a card, a lightbox tile and a tray surface have, and after a row is
/// committed nothing else writes it: the recorder set it once, at the size the settings happened to say
/// that day. Keyed by `rowid` rather than by a name, because every row of a segment shares
/// `videofile_name` and — worse — a name-keyed UPDATE would redraw a whole segment from one frame.
pub fn apply_thumbnails(tx: &Transaction<'_>, writes: &[ThumbWrite]) -> Result<usize, StoreError> {
    let mut changed = 0usize;
    let mut stmt = tx.prepare_cached("UPDATE video_text SET thumbnail = ?1 WHERE rowid = ?2")?;
    for write in writes {
        changed += stmt.execute(params![write.thumbnail, write.rowid])?;
    }
    Ok(changed)
}

/// Character-set Jaccard similarity, matching `ocr_manager.compare_strings`.
///
/// The metric stays exactly as upstream defined it even though it is weak — two screens sharing 94 %
/// of their *distinct characters* in any order are called duplicates — because the history on users'
/// disks was deduplicated under it. Swapping in a real text-similarity measure would silently change
/// which rows a search from last year returns.
pub fn text_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let left: std::collections::HashSet<char> = a.chars().collect();
    let right: std::collections::HashSet<char> = b.chars().collect();
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(&right).count();
    let union = left.union(&right).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Which rows of a batch are repeats of an earlier row.
///
/// The rule reproduced from `remove_duplicates_in_df`: a row is dropped if *any* earlier row in the
/// batch — dropped or kept — is at or above `threshold`. Earlier rows are not skipped when they were
/// themselves dropped, because that is what upstream does and the retained row set has to match.
///
/// `threshold` is a fraction (0.94), not upstream's `* 100` scaled form; callers convert once.
pub fn duplicate_indices(rows: &[Row], threshold: f64) -> Vec<i64> {
    let mut ordered: Vec<&Row> = rows.iter().collect();
    ordered.sort_by_key(|r| (r.time, r.rowid));
    let bodies: Vec<&str> = ordered.iter().map(|r| r.body()).collect();
    duplicate_flags(&bodies, threshold)
        .into_iter()
        .map(|at| ordered[at].rowid)
        .collect()
}

/// The same rule over bare strings, returning the positions to drop.
///
/// The recorder needs this before a row has an id — it collapses the batch it is about to commit —
/// so the comparison is factored away from `Row` and both callers share one implementation of the
/// threshold semantics.
pub fn duplicate_flags(texts: &[&str], threshold: f64) -> Vec<usize> {
    let sets: Vec<std::collections::HashSet<char>> = texts.iter().map(|t| t.chars().collect()).collect();
    let mut dropped = Vec::new();
    for j in 1..texts.len() {
        // A length ratio outside [t, 1/t] cannot reach the threshold; this prunes most comparisons
        // without changing the answer.
        let min_len = (sets[j].len() as f64 * threshold) as usize;
        let max_len = if threshold > 0.0 { (sets[j].len() as f64 / threshold) as usize + 1 } else { usize::MAX };
        for i in 0..j {
            if sets[i].len() < min_len || sets[i].len() > max_len {
                continue;
            }
            if text_similarity(texts[i], texts[j]) >= threshold {
                dropped.push(j);
                break;
            }
        }
    }
    dropped
}

/// Delete rows by rowid, chunked so the statement stays inside SQLite's variable limit.
pub fn delete_rows(tx: &Transaction<'_>, rowids: &[i64]) -> Result<usize, StoreError> {
    const CHUNK: usize = 400;
    let mut deleted = 0usize;
    for chunk in rowids.chunks(CHUNK) {
        let holders = (1..=chunk.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
        let sql = format!("DELETE FROM video_text WHERE rowid IN ({holders})");
        let mut stmt = tx.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        deleted += stmt.execute(refs.as_slice())?;
    }
    Ok(deleted)
}

/// Every rowid belonging to one segment, for the rollback that a failed re-index performs.
///
/// Matching on the 19-character stamp prefix — not the whole name — is upstream's own convention, so
/// that a re-run with `-INDEX` appended to the filename still finds the rows written before it.
pub fn segment_rowids(conn: &Connection, videofile_name: &str) -> Result<Vec<i64>, StoreError> {
    let stamp: String = videofile_name.chars().take(19).collect();
    let mut stmt = conn.prepare("SELECT rowid FROM video_text WHERE videofile_name LIKE ? ORDER BY rowid")?;
    let rows = stmt.query_map([format!("%{stamp}%")], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// What the index says about one segment: how many rows name it, and how many of them still have no
/// text at all.
///
/// This is the one door for the question "is this footage already searchable", and it is here rather
/// than in either caller because two of them ask it: the back-index decides whether to read a video's
/// frames again, and the pass's census has to promise the window the same number the step will then do.
/// Written twice it would be two answers, which is the failure this workspace keeps paying for.
///
/// The same 19-character stamp match [`segment_rowids`] uses, so a row written under a name that has
/// since gained a pipeline marker still counts as belonging to the segment.
pub fn segment_text_state(conn: &Connection, videofile_name: &str) -> Result<(usize, usize), StoreError> {
    let stamp: String = videofile_name.chars().take(19).collect();
    let counted = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(CASE WHEN ocr_text IS NULL OR ocr_text = '' THEN 1 ELSE 0 END), 0) \
         FROM video_text WHERE videofile_name LIKE ?1",
        [format!("%{stamp}%")],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let (rows, waiting) = counted;
    Ok((rows.max(0) as usize, waiting.max(0) as usize))
}

/// What the index says about one segment, and what a pass should therefore do with its video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentCoverage {
    /// No row names this segment — legacy footage, an imported library, or a capture whose rows never
    /// committed. The back-index's whole reason to exist.
    Uncovered,
    /// Rows exist and every one of them carries text: searching this hour already works, whoever read it.
    Read { rows: usize },
    /// Rows exist and some are still empty. `slice_on_disk` is the difference between waiting and
    /// stranded: while the recorder's slice folder is there, the pass's text step can still read those
    /// frames on a later pass; once it has been swept, the video is the only copy of the words left.
    Waiting { rows: usize, waiting: usize, slice_on_disk: bool },
}

/// Ask how much of one segment the index already holds.
///
/// One function for two callers, because the two must not disagree: the back-index decides from this
/// whether to spend an hour of OCR on footage somebody can already search, and the pass's census
/// promises the window the number of videos the step will then actually work. A census that counts by a
/// different rule than the step is a button that lies.
///
/// `months` is which month files the caller's segment could have rows in — the caller knows the segment
/// length and the product's day boundary, and this file does not. Read-only, and it never creates a
/// month file: a coverage check that made a database to ask a question would be a side effect worse
/// than the work it saves.
///
/// Anything this cannot read falls through to [`SegmentCoverage::Uncovered`], which is the behaviour
/// before the question was ever asked: an index it cannot read is a reason to do the work, not a reason
/// to mark footage searchable and hide the gap from the user.
pub fn segment_coverage(
    db_dir: &Path,
    user: &str,
    months: &[(i64, u32)],
    screenshot_dir: &Path,
    videofile_name: &str,
) -> SegmentCoverage {
    let mut rows = 0usize;
    let mut waiting = 0usize;
    for (year, month) in months {
        let path = db_dir.join(wind_base::paths::month_filename(user, *year, *month));
        if !path.exists() {
            continue;
        }
        let Ok(conn) = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return SegmentCoverage::Uncovered;
        };
        // A lane that is committing right now is not a broken index. Wait for it rather than answering
        // "uncovered" and re-OCR ing footage somebody else has already read.
        let _ = conn.busy_timeout(std::time::Duration::from_secs(30));
        let Ok((month_rows, month_waiting)) = segment_text_state(&conn, videofile_name) else {
            return SegmentCoverage::Uncovered;
        };
        rows += month_rows;
        waiting += month_waiting;
    }
    if rows == 0 {
        return SegmentCoverage::Uncovered;
    }
    if waiting == 0 {
        return SegmentCoverage::Read { rows };
    }
    SegmentCoverage::Waiting { rows, waiting, slice_on_disk: slice_held(screenshot_dir, videofile_name) }
}

/// Is this segment's recorder slice still on disk?
///
/// `cache_screenshot/{stamp}[-MARKER]` is where the masked copies live, and while that folder is there
/// the text step can finish the rows a video's own OCR would only duplicate. The listing comes from
/// [`wind_base::paths::slice_dirs`] rather than from one guessed path, because the marker suffix is part
/// of the name and a path built from index text is never trusted to point at a file in this workspace.
fn slice_held(screenshot_dir: &Path, videofile_name: &str) -> bool {
    let stamp: String = videofile_name.chars().take(19).collect();
    wind_base::paths::slice_dirs(screenshot_dir).contains_key(stamp.as_str())
}

/// Rows older than `cutoff`, oldest first — the input to the retention sweep.
pub fn expired_rows(conn: &Connection, cutoff: i64) -> Result<Vec<Row>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT rowid, videofile_name, picturefile_name, videofile_time, ocr_text, \
             is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking \
             FROM video_text WHERE videofile_time < ? ORDER BY videofile_time, rowid")?;
    let rows = stmt.query_map([cutoff], crate::read::row_from)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// How many rows one time window covers — the count of [`erase_window`]'s own predicate.
///
/// The bound is normalized the same way for the same reason: the dry run and the write have to agree,
/// and a `COUNT` that counted NULL text as a row the eraser would skip (or the reverse) turns
/// `--dry-run` into a lie about how many rows are about to change.
pub fn count_window(conn: &Connection, from: i64, to: i64) -> Result<i64, StoreError> {
    let (lo, hi) = if from == to { (from, to + 1) } else { (from.min(to), to.max(from)) };
    let mut stmt = conn.prepare("SELECT COUNT(*) FROM video_text WHERE videofile_time BETWEEN ?1 AND ?2")?;
    Ok(stmt.query_row(rusqlite::params![lo, hi], |r| r.get::<_, i64>(0))?)
}

/// Rows that still hold something a search could return, which is what "this file is now empty" means.
///
/// An empty string counts as nothing: the recorder writes `''` rather than NULL where a column had no
/// value, so a check for NULL alone would call a freshly erased file un-erased.
pub fn count_with_text(conn: &Connection) -> Result<i64, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT COUNT(*) FROM video_text WHERE COALESCE(ocr_text, '') <> '' OR COALESCE(win_title, '') <> '' \
         OR COALESCE(deep_linking, '') <> '' OR COALESCE(thumbnail, '') <> ''",
    )?;
    Ok(stmt.query_row([], |r| r.get::<_, i64>(0))?)
}

/// How many rows a [`Query`] selects. The dry run of the erase command, and nothing else.
///
/// Deliberately the query's own `count_sql` rather than a count written beside the `UPDATE` below: a
/// `--dry-run` that reported a different number of rows than the real run then touched would be worse
/// than no dry run, because it is the step the user trusts before they hand over the write.
pub fn count_matching(conn: &Connection, query: &crate::search::Query) -> Result<i64, StoreError> {
    let (sql, binds) = query.count_sql();
    let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt.query_row(refs.as_slice(), |r| r.get::<_, i64>(0))?)
}

/// Blank every column that carries what the screen showed, on the rows inside one time window.
///
/// The keywordless form of the erase, and the reason it is not [`erase_matching`] handed an empty
/// [`crate::search::Query`]: a query with no tokens adds `ocr_text LIKE '%'`, which does not match a
/// NULL, so a row indexed from its window title alone — text never read, title kept — would survive the
/// one command whose whole job is that nothing about a period stays searchable. The bound is normalized
/// exactly as `Query::where_clause` normalizes it, so this window and the same window reported by
/// `windcapctl query` cover the same rows.
pub fn erase_window(conn: &Connection, from: i64, to: i64) -> Result<usize, StoreError> {
    let (lo, hi) = if from == to { (from, to + 1) } else { (from.min(to), to.max(from)) };
    let mut stmt = conn.prepare(
        "UPDATE video_text SET ocr_text = NULL, win_title = NULL, deep_linking = NULL, thumbnail = NULL \
         WHERE videofile_time BETWEEN ?1 AND ?2",
    )?;
    Ok(stmt.execute(rusqlite::params![lo, hi])?)
}

/// Blank every column that carries what the screen showed, on exactly the rows a [`Query`] selects.
///
/// The keyword form of the erase, so the words that let somebody find a row are the words that erase it.
/// Every seen-content column goes together — recognised text, foreground window title, browser link,
/// stored preview — because erasing the words while keeping the picture would leave the same row visible
/// in the day view and readable in the lightbox: one secret, two doors. The row itself survives with its
/// `videofile_name`, because what is erased here is the index, not the footage.
///
/// The predicate comes from [`crate::search::Query::where_clause`], the one the UI and the bridge also
/// use, so "the rows this period shows me" and "the rows this command erases" cannot drift apart.
pub fn erase_matching(conn: &Connection, query: &crate::search::Query) -> Result<usize, StoreError> {
    let (wheres, binds) = query.where_clause();
    let sql = format!(
        "UPDATE video_text SET ocr_text = NULL, win_title = NULL, deep_linking = NULL, thumbnail = NULL{wheres}"
    );
    let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt.execute(refs.as_slice())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ensure_schema;
    use rusqlite::params;
    use wind_base::clock::LocalParts;

    fn ts(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    fn row(id: i64, stamp: &str, video: &str, picture: &str, text: &str) -> Row {
        Row {
            rowid: id,
            videofile_name: video.into(),
            picturefile_name: picture.into(),
            time: ts(stamp),
            ocr_text: text.into(),
            win_title: None,
            deep_linking: None,
            thumbnail: None,
            video_exists: true,
            picture_exists: false,
            month_path: None,
        }
    }

    /// Previews are addressed by row, never by segment: the two statements differ by the whole history
    /// of a user's month, and a name-keyed UPDATE would paint every row of a segment with one frame.
    #[test]
    fn thumbnails_are_written_row_by_row_and_counted() {
        let mut conn = seeded(&[
            row(1, "2026-09-21_10-00-00", "2026-09-21_10-00-00.mp4", "0.jpg", "first"),
            row(2, "2026-09-21_10-00-02", "2026-09-21_10-00-00.mp4", "1.jpg", "second"),
        ]);
        let tx = conn.transaction().unwrap();
        let written = apply_thumbnails(
            &tx,
            &[
                ThumbWrite { rowid: 1, thumbnail: "AAA".into() },
                ThumbWrite { rowid: 2, thumbnail: "BBB".into() },
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(written, 2);
        let stored: Vec<String> = conn
            .prepare("SELECT thumbnail FROM video_text ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(stored, vec!["AAA".to_string(), "BBB".to_string()], "each row got its own picture");
        let tx = conn.transaction().unwrap();
        assert_eq!(apply_thumbnails(&tx, &[ThumbWrite { rowid: 99, thumbnail: "no".into() }]).unwrap(), 0, "a row that is gone changes nothing");
        tx.commit().unwrap();
    }

    fn seeded(rows: &[Row]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        for r in rows {
            conn.execute(
                "INSERT INTO video_text (rowid, videofile_name, picturefile_name, videofile_time, ocr_text,
                   is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'',NULL,'')",
                params![r.rowid, r.videofile_name, r.picturefile_name, r.time, r.ocr_text,
                        r.video_exists as i64, r.picture_exists as i64],
            )
            .unwrap();
        }
        conn
    }

    fn flag_of(conn: &Connection, video: &str) -> i64 {
        conn.query_row(
            "SELECT is_videofile_exist FROM video_text WHERE videofile_name = ? LIMIT 1",
            [video],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    }

    #[test]
    fn the_timestamp_index_is_created_once() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        assert!(ensure_time_index(&conn).unwrap(), "first call creates it");
        assert!(!ensure_time_index(&conn).unwrap(), "second call is a no-op");
        assert!(!ensure_time_index(&conn).unwrap());
    }

    #[test]
    fn existence_is_decided_once_per_name_and_only_writes_what_changed() {
        let rows = vec![
            row(1, "2026-09-21_10-00-00", "a.mp4", "a.jpg", "x"),
            row(2, "2026-09-21_10-01-00", "a.mp4", "b.jpg", "y"),
            row(3, "2026-09-21_10-02-00", "gone.mp4", "c.jpg", "z"),
        ];
        let conn = seeded(&rows);
        let video_probes = std::cell::Cell::new(0);
        let picture_probes = std::cell::Cell::new(0);
        let plan = plan_existence(
            &rows,
            &|name| {
                video_probes.set(video_probes.get() + 1);
                name == "a.mp4"
            },
            &|_| {
                picture_probes.set(picture_probes.get() + 1);
                false
            },
        );
        assert_eq!(video_probes.get(), 2, "one probe per distinct video name, not per row");
        assert_eq!(picture_probes.get(), 3);
        assert_eq!(plan.video_existence, vec![("gone.mp4".to_string(), false)], "a.mp4 was already correct");
        assert_eq!(plan.picture_existence.len(), 0, "pictures were already recorded as absent");

        let tx = conn.unchecked_transaction().unwrap();
        assert_eq!(apply_existence(&tx, &plan).unwrap(), 1);
        tx.commit().unwrap();
        assert_eq!(flag_of(&conn, "gone.mp4"), 0);
        assert_eq!(flag_of(&conn, "a.mp4"), 1);
    }

    #[test]
    fn a_flipped_picture_flag_lands_on_every_row_of_that_frame() {
        let rows = vec![row(1, "2026-09-21_10-00-00", "a.mp4", "f.jpg", "x")];
        let conn = seeded(&rows);
        let plan = plan_existence(&rows, &|_| true, &|_| true);
        assert_eq!(plan.picture_existence, vec![("f.jpg".to_string(), true)]);
        let tx = conn.unchecked_transaction().unwrap();
        apply_existence(&tx, &plan).unwrap();
        tx.commit().unwrap();
        let flag: i64 = conn
            .query_row("SELECT is_picturefile_exist FROM video_text", [], |r| r.get(0))
            .unwrap();
        assert_eq!(flag, 1);
    }

    #[test]
    fn similarity_is_the_shipped_metric() {
        assert_eq!(text_similarity("", ""), 0.0);
        assert_eq!(text_similarity("abc", "abc"), 1.0);
        assert!((text_similarity("abcd", "abce") - 0.6).abs() < 1e-9, "3 shared of 5 union");
        assert_eq!(text_similarity("a", ""), 0.0);
    }

    /// The exact retention rule upstream applies, including the quirk that a dropped row still
    /// serves as the anchor for the rows after it.
    #[test]
    fn a_batch_of_identical_screens_keeps_only_the_first() {
        let rows = vec![
            row(1, "2026-09-21_10-00-00", "a.mp4", "", "revenue report 2026 q3"),
            row(2, "2026-09-21_10-00-03", "a.mp4", "", "revenue report 2026 q3"),
            row(3, "2026-09-21_10-00-06", "a.mp4", "", "revenue report 2026 q4"),
            row(4, "2026-09-21_10-00-09", "a.mp4", "", "a shopping cart with socks"),
        ];
        // q3 vs q4 differ by one character out of fifteen: 0.867 Jaccard, below the 0.94 line.
        assert_eq!(duplicate_indices(&rows, 0.94), vec![2]);
        // Rows out of order still compare oldest-first.
        let mut shuffled = rows.clone();
        shuffled.reverse();
        assert_eq!(duplicate_indices(&shuffled, 0.94), vec![2]);
        assert!(duplicate_indices(&[], 0.94).is_empty());
        assert!(duplicate_indices(&rows[..1], 0.94).is_empty());
    }

    /// The one behaviour that must be copied exactly, quirks and all: similarity here is not
    /// transitive, and upstream lets an already-dropped row anchor the rows after it. So A~B and
    /// B~C with A!~C drops *both* B and C — a chain of screens that drifted one glyph at a time is
    /// collapsed even though its ends look different.
    #[test]
    fn a_drift_chain_collapses_further_than_a_single_comparison_against_the_kept_row() {
        // 10 characters, then 12, then 14: each step is 0.83 and 0.86 Jaccard against its
        // predecessor, but the ends only share 10 of 14, which is 0.71.
        let rows = vec![
            row(1, "2026-09-21_10-00-00", "a.mp4", "", "abcdefghij"),
            row(2, "2026-09-21_10-00-03", "a.mp4", "", "abcdefghijuv"),
            row(3, "2026-09-21_10-00-06", "a.mp4", "", "abcdefghijuvwx"),
        ];
        assert!(text_similarity("abcdefghij", "abcdefghijuvwx") < 0.8, "the ends must not match directly");
        assert_eq!(duplicate_indices(&rows, 0.8), vec![2, 3]);
    }

    #[test]
    fn a_looser_threshold_collapses_more_rows() {
        let rows = vec![
            row(1, "2026-09-21_10-00-00", "a.mp4", "", "revenue report 2026 q3"),
            row(2, "2026-09-21_10-00-03", "a.mp4", "", "revenue report 2026 q4"),
        ];
        assert_eq!(duplicate_indices(&rows, 0.94), Vec::<i64>::new());
        assert_eq!(duplicate_indices(&rows, 0.5), vec![2]);
    }

    #[test]
    fn deletion_is_chunked_and_counts() {
        let rows: Vec<Row> = (1..=900)
            .map(|i| row(i, "2026-09-21_10-00-00", "a.mp4", "", &format!("text {i}")))
            .collect();
        let conn = seeded(&rows);
        let ids: Vec<i64> = (1..=900).collect();
        let tx = conn.unchecked_transaction().unwrap();
        assert_eq!(delete_rows(&tx, &ids).unwrap(), 900);
        tx.commit().unwrap();
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM video_text", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
        assert_eq!(delete_rows(&conn.unchecked_transaction().unwrap(), &[] as &[i64]).unwrap(), 0);
    }

    #[test]
    fn a_segment_rollback_finds_rows_whatever_suffix_the_file_wore() {
        let rows = vec![
            row(1, "2026-09-21_10-00-00", "2026-09-21_10-00-00.mp4", "", "a"),
            row(2, "2026-09-21_10-01-00", "2026-09-21_10-00-00-INDEX.mp4", "", "b"),
            row(3, "2026-09-21_11-00-00", "2026-09-21_11-00-00.mp4", "", "c"),
        ];
        let conn = seeded(&rows);
        let ids = segment_rowids(&conn, "2026-09-21_10-00-00.mp4").unwrap();
        assert_eq!(ids, vec![1, 2]);
        assert!(segment_rowids(&conn, "2026-09-20_00-00-00.mp4").unwrap().is_empty());
    }

    #[test]
    fn retention_selects_only_what_is_past_its_expiry() {
        let rows = vec![
            row(1, "2025-01-01_10-00-00", "old.mp4", "", "old"),
            row(2, "2026-09-21_10-00-00", "new.mp4", "", "new"),
        ];
        let conn = seeded(&rows);
        let cutoff = ts("2026-01-01_00-00-00");
        let expired = expired_rows(&conn, cutoff).unwrap();
        assert_eq!(expired.iter().map(|r| r.videofile_name.as_str()).collect::<Vec<_>>(), vec!["old.mp4"]);
    }

    /// The four columns that hold what the screen showed, blanked together or not at all.
    #[test]
    fn an_erase_blanks_text_title_link_and_preview_and_leaves_the_row_and_its_video() {
        let conn = seeded(&[
            row(1, "2026-09-21_10-00-00", "2026-09-21_10-00-00.mp4", "a.jpg", "revenue report"),
            row(2, "2026-09-21_11-00-00", "2026-09-21_11-00-00.mp4", "b.jpg", "holiday photos"),
        ]);
        conn.execute(
            "UPDATE video_text SET win_title = 'Browser — rewards', deep_linking = 'https://x/1', thumbnail = 'AAAA' WHERE rowid = 1",
            [],
        )
        .unwrap();

        let erased = erase_window(&conn, ts("2026-09-21_09-00-00"), ts("2026-09-21_10-30-00")).unwrap();
        assert_eq!(erased, 1, "the window stops before 11:00");
        let blanked: (Option<String>, Option<String>, Option<String>, Option<String>) = conn
            .query_row("SELECT ocr_text, win_title, deep_linking, thumbnail FROM video_text WHERE rowid = 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap();
        assert_eq!(blanked, (None, None, None, None), "NULL, not an empty string that reads as 'nothing was there'");
        let untouched: String = conn
            .query_row("SELECT ocr_text FROM video_text WHERE rowid = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(untouched, "holiday photos", "the next hour is not in this window");
        let still_pointing: i64 = conn
            .query_row("SELECT count(*) FROM video_text WHERE videofile_name = '2026-09-21_10-00-00.mp4'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(still_pointing, 1, "the row and its footage pointer survive: this erases an index, not a video");
    }

    /// Why the keywordless erase is its own statement and not a [`crate::search::Query`] with no tokens.
    #[test]
    fn a_row_indexed_from_its_title_alone_is_still_inside_the_erasable_window() {
        let conn = seeded(&[]);
        conn.execute(
            "INSERT INTO video_text (rowid, videofile_name, picturefile_name, videofile_time, ocr_text,
               is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
             VALUES (1, '2026-09-21_10-00-00.mp4', 'a.jpg', ?1, NULL, 1, 0, '', 'Browser — rewards', '')",
            params![ts("2026-09-21_10-00-00")],
        )
        .unwrap();
        let day = crate::search::Query::new(ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"));
        assert_eq!(
            count_matching(&conn, &day).unwrap(),
            0,
            "a keywordless search sees only rows with text, so it cannot be the eraser's predicate"
        );
        assert_eq!(erase_window(&conn, day.from, day.to).unwrap(), 1, "and the title-only row is erased all the same");
        let title: Option<String> = conn
            .query_row("SELECT win_title FROM video_text WHERE rowid = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, None);
    }

    #[test]
    fn the_dry_run_count_and_the_keyword_erase_agree_on_the_same_rows() {
        let conn = seeded(&[
            row(1, "2026-09-21_10-00-00", "2026-09-21_10-00-00.mp4", "a.jpg", "revenue report"),
            row(2, "2026-09-21_11-00-00", "2026-09-21_11-00-00.mp4", "b.jpg", "holiday photos"),
            row(3, "2026-09-21_12-00-00", "2026-09-21_12-00-00.mp4", "c.jpg", "revenue forecast"),
        ]);
        let query = crate::search::Query::new(ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59")).with_keywords("revenue");
        assert_eq!(count_matching(&conn, &query).unwrap(), 2);
        assert_eq!(erase_matching(&conn, &query).unwrap(), 2, "the dry run promised the same two rows");
        assert_eq!(count_matching(&conn, &query).unwrap(), 0, "nothing matches afterwards, so a second run is a no-op");
        let bystander: String = conn
            .query_row("SELECT ocr_text FROM video_text WHERE rowid = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bystander, "holiday photos");
    }
}
