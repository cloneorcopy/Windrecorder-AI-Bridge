//! The table definition, as it exists on users' disks today.
//!
//! Reproduced verbatim from `windrecorder/db_manager.py:139`. There is nothing to tidy up here:
//! `INT` and `BOOLEAN` are SQLite type-name fictions that resolve to INTEGER affinity, and the
//! installed base is built out of exactly these nine columns in exactly this order. Renaming or
//! reordering would not be an improvement, it would be a data-loss event.

use rusqlite::Connection;

pub const DDL: &str = "CREATE TABLE video_text \
     (videofile_name VARCHAR(100), picturefile_name VARCHAR(100), videofile_time INT, \
      ocr_text TEXT, is_videofile_exist BOOLEAN, is_picturefile_exist BOOLEAN, \
      thumbnail TEXT, win_title TEXT, deep_linking TEXT)";

/// Exact table order; the Python indexer binds this positionally, so it is a wire format.
pub const COLUMNS: [&str; 9] = [
    "videofile_name",
    "picturefile_name",
    "videofile_time",
    "ocr_text",
    "is_videofile_exist",
    "is_picturefile_exist",
    "thumbnail",
    "win_title",
    "deep_linking",
];

/// The two columns that were ALTER-added in later versions and are therefore missing from older
/// month files. Adding them in place is the only migration a native reader ever performs.
pub const LATE_COLUMNS: [&str; 2] = ["win_title", "deep_linking"];

/// `" -||- "` in the middle of `ocr_text` is what the WebUI and the bridge split on: the video
/// indexing path stores `"<ocr text> -||- <window title>"` in one field.
pub const TITLE_SEPARATOR: &str = " -||- ";

/// Every failure out of this crate is either SQLite's or the filesystem's, and neither is worth
/// wrapping in a bespoke type beyond the context the caller already has.
#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    /// The file is a SQLite database that does not contain the table, i.e. it is not one of ours.
    NotAnIndex(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "sqlite: {e}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::NotAnIndex(p) => write!(f, "{p} has no video_text table"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// Create the table if missing, and add the two lazily-introduced columns if an older month file
/// lacks them — mirroring `db_update_table_product_routine`. Returns the column list as read back.
pub fn ensure_schema(conn: &Connection) -> Result<Vec<String>, StoreError> {
    // Ask sqlite_master rather than string-matching a "table exists" failure: rusqlite surfaces
    // that condition as SqlInputError, not SqliteFailure, and a guard that silently never fires
    // would make every existing month unopenable.
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='video_text'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)?;
    if !exists {
        conn.execute_batch(DDL)?;
    }

    let mut present = column_names(conn)?;
    for column in LATE_COLUMNS {
        if !present.iter().any(|c| c == column) {
            conn.execute(&format!("ALTER TABLE video_text ADD COLUMN {column} TEXT"), [])?;
            present.push(column.to_string());
        }
    }
    Ok(present)
}

/// The table's columns, in stored order. A month file that has been hand-edited or partially
/// migrated is a real condition in the wild, so readers resolve columns by name from this.
pub fn column_names(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare("PRAGMA table_info(video_text)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut out = Vec::with_capacity(COLUMNS.len());
    for name in rows {
        out.push(name?);
    }
    if out.is_empty() {
        return Err(StoreError::NotAnIndex("video_text".to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_database_gets_the_nine_column_table() {
        let conn = Connection::open_in_memory().unwrap();
        let cols = ensure_schema(&conn).unwrap();
        assert_eq!(cols, COLUMNS.to_vec());
    }

    #[test]
    fn legacy_seven_column_month_is_upgraded_in_place_without_reordering() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE video_text (videofile_name VARCHAR(100), picturefile_name VARCHAR(100),
             videofile_time INT, ocr_text TEXT, is_videofile_exist BOOLEAN,
             is_picturefile_exist BOOLEAN, thumbnail TEXT)",
            [],
        )
        .unwrap();
        let cols = ensure_schema(&conn).unwrap();
        assert_eq!(cols.len(), 9);
        assert_eq!(cols[6], "thumbnail", "pre-existing columns must keep their positions");
        assert_eq!(cols[7], "win_title");
        assert_eq!(cols[8], "deep_linking");
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(ensure_schema(&conn).unwrap(), ensure_schema(&conn).unwrap());
    }

    #[test]
    fn a_database_without_the_table_is_rejected_rather_than_read_as_empty() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE something_else (x INT)", []).unwrap();
        // ensure_schema would create the table, which is right for a writer; column_names is the
        // reader's guard and must not invent one.
        assert!(matches!(
            column_names(&conn),
            Err(StoreError::NotAnIndex(_))
        ));
        let _ = ensure_schema(&conn).unwrap();
        assert!(column_names(&conn).is_ok());
    }
}
