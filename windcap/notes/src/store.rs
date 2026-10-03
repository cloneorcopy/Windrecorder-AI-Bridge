//! The table: read it, edit it in memory, write it back without ever losing a row.
//!
//! Three write paths exist because the three things a user does are different shapes of work:
//!
//!   * [`FlagStore::append_persisted`] — one new row, written by appending a single line. The tray
//!     action's path, and the reason it is not "rewrite the table with one more row" is that a
//!     rewrite can lose a row another writer added in between, while an append cannot.
//!   * [`FlagStore::save`] — the editor's whole-table write: stage a unique sibling, rename over the
//!     target, refuse if the file moved since it was read, and do nothing at all when nothing
//!     changed.
//!   * deletion — a rewrite of the remaining rows. **Never** the removal of the file.
//!
//! The last point is a deliberate behaviour change from upstream. `st_save_flag_mark_note_from_editor`
//! begins with `if (df_editor["delete"] == 1).all(): send2trash(config.flag_mark_note_filepath)`, so a
//! user who selects every bookmark to clear the list instead loses the file itself, along with every
//! note in it, to the recycle bin — where it does no good, because the app reads a missing file as an
//! empty table and shows "you have never used this feature". Selecting all rows is an ordinary way to
//! say "clear them", and the answer is an empty table with its header intact, not a deleted contract.
//! (`component_flag_mark` has the same instinct in reverse: it trashes a file that parses as empty.
//! An empty file is a valid state here, and it is left alone.)

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::csv;

use crate::flag::{self, Flag, HEADER};
use crate::markers::DaySpan;

/// Distinguishes concurrent staging files, so two writers never share one temp name.
static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// One line of the table, whether or not this crate understands it.
///
/// A row whose datetime is in a shape nothing recognises stays [`Entry::Raw`] and is written back
/// byte-identically. Dropping unparseable rows on save is the quiet data loss that a
/// read-modify-write table like this is always one bad line away from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Flag(Flag),
    Raw(csv::Record),
}

impl Entry {
    pub fn flag(&self) -> Option<&Flag> {
        match self {
            Entry::Flag(flag) => Some(flag),
            Entry::Raw(_) => None,
        }
    }

    /// The `datetime` cell as it will be written: the parsed instant in the ISO spelling for a
    /// understood row, the untouched field for a raw one.
    pub fn datetime_text(&self) -> String {
        match self {
            Entry::Flag(flag) => flag.stored_datetime(),
            Entry::Raw(record) => record.get(1).cloned().unwrap_or_default(),
        }
    }

    pub fn note(&self) -> &str {
        match self {
            Entry::Flag(flag) => &flag.note,
            Entry::Raw(record) => record.get(2).map(String::as_str).unwrap_or(""),
        }
    }

    /// The `thumbnail` cell, without any data-URL prefix.
    pub fn thumbnail(&self) -> &str {
        match self {
            Entry::Flag(flag) => &flag.thumbnail,
            Entry::Raw(record) => record.first().map(String::as_str).unwrap_or(""),
        }
    }

    pub fn fields(&self) -> csv::Record {
        match self {
            Entry::Flag(flag) => flag.fields(),
            // A raw row is re-emitted from the fields it arrived with, so its datetime keeps whatever
            // spelling was on disk rather than being normalised by side effect.
            Entry::Raw(record) => record.clone(),
        }
    }

    /// Only understood rows are editable: an unrecognised line is preserved, not guessed at.
    fn set_note(&mut self, note: &str) -> bool {
        match self {
            Entry::Flag(flag) => {
                // `update_note_to_csv_by_datetime` writes the placeholder for an empty note, so the
                // column is never blank on disk.
                let note = flag::normalise_note(note);
                if flag.note == note {
                    false
                } else {
                    flag.note = note;
                    true
                }
            }
            Entry::Raw(_) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// The file already held exactly these bytes: it was not opened for writing, so its mtime is
    /// untouched. The UI diffs on that, and so does every staleness test in the workspace.
    Unchanged,
    Written,
}

/// The result of one row-targeted write — the editor's edit and delete buttons, and nothing else.
///
/// A row is addressed by a [`RowRef`] describing what the caller was *shown*, not by a bare position,
/// because a position is a claim about the past and the file may have moved. The write is carried out
/// against a fresh read that re-finds the row by content, so the answers a caller has to distinguish
/// are: it did the thing, the row it meant is no longer uniquely findable, or the file moved under it
/// mid-write. Only the first wrote anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowEdit {
    /// The row was found and the table was written (or, for a dry run, would be).
    Done(SaveOutcome),
    /// No unique row matched: the position ran past the end, or the row it named is gone or is now
    /// indistinguishable from an identical one. Nothing was written — the caller should reload.
    NotFound,
    /// Another writer changed the file between this operation's read and its write, so writing now
    /// would drop a row it appended. Nothing was written; the store is resynchronised to the truth.
    Conflict,
}

/// How the editor names the row it wants to change: the datetime and note it displayed, plus the
/// position it displayed them at.
///
/// The text is the authority and the index is only a fast path. Under an append at the end — the
/// concurrent write that actually happens, the tray flagging while a window is open — the index still
/// points at the same row, so it is trusted first. If a concurrent delete has shifted it, the row is
/// re-found by its `(datetime, note)`; if that now matches zero rows or more than one, the operation
/// refuses rather than edit or drop a row the user never pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowRef {
    /// The stored `datetime` text, as `Entry::datetime_text` re-emits it.
    pub datetime: String,
    /// The note text as displayed.
    pub note: String,
    /// The absolute position this row held when the caller read it.
    pub index: usize,
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    /// The table was edited from a snapshot that is no longer what is on disk. Writing now would
    /// replace rows added since — usually by the tray, while an editor window was open.
    ChangedOnDisk(PathBuf),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "{e}"),
            StoreError::ChangedOnDisk(path) => {
                write!(f, "{} changed since it was read; reload before saving", path.display())
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> StoreError {
        StoreError::Io(e)
    }
}

#[derive(Debug, Clone)]
pub struct FlagStore {
    path: PathBuf,
    entries: Vec<Entry>,
    /// FNV-1a of the bytes this snapshot was read from, `None` when the file did not exist.
    fingerprint: Option<u64>,
}

impl FlagStore {
    /// Read the table. A missing file is an empty table, exactly as the Python app treats it: not an
    /// error, and nothing is created.
    pub fn load(path: &Path) -> Result<FlagStore, StoreError> {
        let bytes = read_if_present(path)?;
        let entries = match &bytes {
            Some(bytes) => parse_body(bytes)?,
            None => Vec::new(),
        };
        Ok(FlagStore { path: path.to_path_buf(), entries, fingerprint: bytes.as_deref().map(fingerprint) })
    }

    /// The table at `config.flag_note_path()` — `userdata/flag_mark_note.csv`.
    pub fn load_for(config: &Config) -> Result<FlagStore, StoreError> {
        FlagStore::load(&config.flag_note_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// An empty table is a normal state, not a reason to remove the file (see the module note).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    /// The understood rows, each with its position in the file so a caller can map a click in a list
    /// back to the row to edit or delete.
    pub fn flags(&self) -> impl Iterator<Item = (usize, &Flag)> {
        self.entries.iter().enumerate().filter_map(|(index, entry)| entry.flag().map(|flag| (index, flag)))
    }

    /// The exact bytes [`FlagStore::save`] writes: the header, then one row per entry.
    pub fn body(&self) -> String {
        let mut out = csv::format_row(&HEADER) + "\n";
        for entry in &self.entries {
            out.push_str(&csv::format_row(&entry.fields()));
            out.push('\n');
        }
        out
    }

    /// Stage a row in memory only — the editor's model, where a save is an explicit act.
    pub fn add(&mut self, flag: Flag) -> usize {
        let index = self.entries.len();
        self.entries.push(Entry::Flag(flag));
        index
    }

    /// Append one row to the file without rewriting it, then resynchronise from disk.
    ///
    /// The resynchronise is not wasted work: an append cannot claim success while staying ignorant of
    /// what the file now holds, and reading the table back is the only way to know the row landed
    /// where we think it did. It also means a second flag taken while this one was being written
    /// shows up in the returned index instead of being silently overwritten.
    pub fn append_persisted(&mut self, flag: Flag, dry_run: bool) -> Result<usize, StoreError> {
        if dry_run {
            return Ok(self.entries.len());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        csv::append_row(&self.path, &HEADER, &flag.fields())?;
        *self = FlagStore::load(&self.path)?;
        let index = self
            .entries
            .iter()
            .rposition(|entry| entry.flag() == Some(&flag))
            .unwrap_or_else(|| self.entries.len().saturating_sub(1));
        Ok(index)
    }

    /// Rewrite the table atomically — or not at all when it already says this.
    ///
    /// `dry_run` answers "what *would* change" without creating, truncating or renaming anything,
    /// including the staging file.
    pub fn save(&mut self, dry_run: bool) -> Result<SaveOutcome, StoreError> {
        let rendered = self.body();
        let on_disk = read_if_present(&self.path)?;
        if on_disk.as_deref() == Some(rendered.as_bytes()) {
            return Ok(SaveOutcome::Unchanged);
        }
        if self.moved_since_read(on_disk.as_deref()) {
            return Err(StoreError::ChangedOnDisk(self.path.clone()));
        }
        if dry_run {
            return Ok(SaveOutcome::Written);
        }
        write_atomic(&self.path, rendered.as_bytes())?;
        self.fingerprint = Some(fingerprint(rendered.as_bytes()));
        Ok(SaveOutcome::Written)
    }

    /// Create a header-only file when there is none, which is all
    /// `ensure_flag_mark_note_csv_exist` does. An existing table, empty or not, is never touched.
    pub fn ensure_exists(&mut self, dry_run: bool) -> Result<bool, StoreError> {
        if self.path.exists() {
            return Ok(false);
        }
        if !dry_run {
            let header = format!("{}\n", csv::format_row(&HEADER));
            write_atomic(&self.path, header.as_bytes())?;
            self.fingerprint = Some(fingerprint(header.as_bytes()));
        }
        Ok(true)
    }

    /// `update_note_to_csv_by_datetime`: the note of every flag at this exact second. Returns how
    /// many rows changed, so a caller can tell "saved" from "that flag is not in the table".
    ///
    /// Upstream matches the formatted datetime string, which means two flags taken in the same second
    /// share a note. That is kept: the second is the column's resolution, so there is no finer address
    /// to aim at, and inventing one would move the other flag's bookmark.
    pub fn set_note(&mut self, when: LocalParts, note: &str) -> usize {
        self.entries
            .iter_mut()
            .filter(|entry| entry.flag().map(|flag| flag.when) == Some(when))
            .map(|entry| entry.set_note(note))
            .filter(|changed| *changed)
            .count()
    }

    /// Drop one row by its position in the table — the editor's checkbox column and the CLI's
    /// `remove <index>`.
    pub fn remove_index(&mut self, index: usize) -> Option<Entry> {
        if index < self.entries.len() {
            return Some(self.entries.remove(index));
        }
        None
    }

    /// Drop every row at one instant — the tray window's "❌ remove mark".
    pub fn remove_at(&mut self, when: LocalParts) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.flag().map(|flag| flag.when) != Some(when));
        before - self.entries.len()
    }

    /// Drop the selected rows in one go, by position.
    ///
    /// Descending or duplicated indices are the caller's problem to not have, so this sorts and
    /// dedups, and removes from the back so a position cannot shift under a later removal.
    pub fn remove_rows(&mut self, indices: &[usize]) -> usize {
        let mut drop = indices.to_vec();
        drop.sort_unstable();
        drop.dedup();
        let before = self.entries.len();
        for index in drop.into_iter().rev() {
            if index < self.entries.len() {
                self.entries.remove(index);
            }
        }
        before - self.entries.len()
    }

    /// The flags whose instant falls inside a day's span, in file order.
    pub fn on_day(&self, span: DaySpan) -> Vec<(usize, &Flag)> {
        self.flags().filter(|(_, flag)| span.contains(flag.epoch())).collect()
    }

    /// Re-read the file into this snapshot.
    ///
    /// The whole-table write paths below call it first so they act on what is on disk *now* — a row
    /// the tray appended while the caller was looking is part of the table they rewrite, not a row
    /// they overwrite. Without the reload, "edit this note" would save the list the editor loaded
    /// minutes ago and quietly delete whatever arrived since.
    pub fn reload(&mut self) -> Result<(), StoreError> {
        *self = FlagStore::load(&self.path)?;
        Ok(())
    }

    /// Rewrite the note of exactly one [`RowRef`], and write the survivors. This — not a CSV-rewriting
    /// call in the editor — is one of only two whole-table write paths, shared with
    /// [`FlagStore::remove_ref`] so `flag_mark_note.csv` has a single writer.
    ///
    /// Returns [`RowEdit`] rather than a bare count so the caller can tell "saved", "that row is gone"
    /// and "somebody else wrote first, so nothing happened" apart, and reload only on the last two.
    pub fn set_note_ref(&mut self, row: &RowRef, note: &str, dry_run: bool) -> Result<RowEdit, StoreError> {
        if dry_run {
            let plan = match self.find_ref(row) {
                None => RowEdit::NotFound,
                Some(at) => {
                    let mut probe = self.entries[at].clone();
                    RowEdit::Done(if probe.set_note(note) { SaveOutcome::Written } else { SaveOutcome::Unchanged })
                }
            };
            return Ok(plan);
        }
        self.reload()?;
        let Some(at) = self.find_ref(row) else { return Ok(RowEdit::NotFound) };
        self.entries[at].set_note(note);
        self.commit()
    }

    /// Delete exactly one [`RowRef`], guarded the same way as [`FlagStore::set_note_ref`].
    ///
    /// Deletion is the destructive half of the editor, so the guard is the point: it never writes a
    /// table it did not just read, and it removes the row the caller *saw* — re-found by its
    /// `(datetime, note)` against the current file, not by a position a concurrent writer may have
    /// moved — or nothing at all. A rewrite that drops the wrong row is unrecoverable; a `NotFound` the
    /// user can look again at is not.
    pub fn remove_ref(&mut self, row: &RowRef, dry_run: bool) -> Result<RowEdit, StoreError> {
        if dry_run {
            return Ok(match self.find_ref(row) {
                Some(_) => RowEdit::Done(SaveOutcome::Written),
                None => RowEdit::NotFound,
            });
        }
        self.reload()?;
        let Some(at) = self.find_ref(row) else { return Ok(RowEdit::NotFound) };
        self.entries.remove(at);
        self.commit()
    }

    /// Write the in-memory table, turning the stale-file refusal into a [`RowEdit::Conflict`] and a
    /// resynchronised store rather than an error the caller has to unwind by hand.
    fn commit(&mut self) -> Result<RowEdit, StoreError> {
        match self.save(false) {
            Ok(outcome) => Ok(RowEdit::Done(outcome)),
            Err(StoreError::ChangedOnDisk(_)) => {
                let _ = self.reload();
                Ok(RowEdit::Conflict)
            }
            Err(e) => Err(e),
        }
    }

    /// Where `row` now lives, if it can be identified at all.
    ///
    /// The position it was shown at is trusted first, because an append lands at the end and leaves
    /// every earlier index pointing at the same `(datetime, note)`; the content search is the fallback
    /// for when a concurrent *delete* has shifted things. If that search finds the row nowhere (it was
    /// removed) or in two places (two indistinguishable rows and no way to know which the user meant),
    /// the answer is `None` and the caller writes nothing — guessing at a destructive edit is the one
    /// outcome that is always wrong.
    fn find_ref(&self, row: &RowRef) -> Option<usize> {
        let matches = |entry: &Entry| entry.datetime_text() == row.datetime && entry.note() == row.note;
        if self.entries.get(row.index).is_some_and(matches) {
            return Some(row.index);
        }
        let mut found: Option<usize> = None;
        for (at, entry) in self.entries.iter().enumerate() {
            if matches(entry) {
                if found.is_some() {
                    return None;
                }
                found = Some(at);
            }
        }
        found
    }

    fn moved_since_read(&self, on_disk: Option<&[u8]>) -> bool {
        match (self.fingerprint, on_disk.map(fingerprint)) {
            (Some(loaded), Some(now)) => loaded != now,
            // Held an absent file that now exists: somebody created it while this snapshot was open.
            (None, Some(_)) => true,
            (Some(_), None) | (None, None) => false,
        }
    }
}

fn parse_body(bytes: &[u8]) -> Result<Vec<Entry>, StoreError> {
    let text = std::str::from_utf8(bytes).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("the flag table is not valid UTF-8: {e}"))
    })?;
    Ok(csv::parse_all(text, true)
        .into_iter()
        .map(|record| match Flag::from_record(&record) {
            Ok(flag) => Entry::Flag(flag),
            Err(_) => Entry::Raw(record),
        })
        .collect())
}

fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Io(e)),
    }
}

/// Replace the file through a unique sibling name.
///
/// Two things this is careful about, both learned elsewhere in the workspace:
///   * the staging name carries a pid and a counter, because the tray and the editor can save in the
///     same second and a shared `flag_mark_note.tmp` would have one writer rename the other's
///     half-written file into place. That is why this does not call [`wind_base::csv::write_rows`],
///     whose temp name is a fixed `with_extension("tmp")`.
///   * the rename is what makes the write atomic: a reader sees either the old complete table or the
///     new one. On Windows it *fails* while another process holds the target open (Excel, the old
///     webui) instead of sharing it, which is the right outcome — the caller's rows are still in
///     memory and the error says so.
fn write_atomic(path: &Path, body: &[u8]) -> Result<(), io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = staging_path(path);
    let outcome = (|| -> io::Result<()> {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(body)?;
        // This table is the user's only copy of their notes: get it to disk before the rename.
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staged, path)?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    outcome
}

fn staging_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("flag_mark_note.csv");
    path.with_file_name(format!(".{name}.tmp-{}-{}", std::process::id(), STAGE_SEQ.fetch_add(1, Ordering::Relaxed)))
}

/// FNV-1a. Only ever a "did the bytes move" test, never an integrity claim, so a
/// non-cryptographic hash that costs one pass is the right size of tool.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use wind_base::clock::LocalParts;

    use super::*;

    /// Every test gets its own directory: these write the file a real install would hold at
    /// `userdata/flag_mark_note.csv`, and two tests sharing one path would race.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-notes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn table(dir: &Path) -> PathBuf {
        dir.join("flag_mark_note.csv")
    }

    fn write(dir: &Path, body: &str) -> PathBuf {
        let path = table(dir);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn flag_at(stamp: &str, note: &str) -> Flag {
        Flag { thumbnail: "AAA".into(), when: LocalParts::from_stamp(stamp).unwrap(), note: note.into() }
    }

    /// The `RowRef` the editor would build from the row it is showing at `index` — the same
    /// `(datetime, note, position)` triple the day panel hands back when a row is edited or deleted.
    fn seen(store: &FlagStore, index: usize) -> RowRef {
        let entry = store.get(index).unwrap();
        RowRef { datetime: entry.datetime_text(), note: entry.note().to_string(), index }
    }

    const HEADER_ONLY: &str = "thumbnail,datetime,note\n";

    /// The compatibility contract, asserted the only way it can be: a file in the exact shape
    /// `to_csv(index=False)` emits — minimal quoting, doubled quotes, a note spanning two physical
    /// lines — is read, and written back as the same bytes.
    #[test]
    fn a_pandas_quoted_table_round_trips_byte_for_byte() {
        let dir = scratch("pandas");
        let body = "thumbnail,datetime,note\n\
ABCD,2026-09-21 21:16:12,\"keep, this\"\n\
ABCD,2026-09-21 21:20:00,\"say \"\"hi\"\"\"\n\
ABCD,2026-09-21 21:30:00,\"line\nbreak\"\n\
,2026-09-21 21:40:00,_\n";
        let path = write(&dir, body);
        let mut store = FlagStore::load(&path).unwrap();

        let notes: Vec<&str> = store.flags().map(|(_, f)| f.note.as_str()).collect();
        assert_eq!(notes, vec!["keep, this", "say \"hi\"", "line\nbreak", "_"]);
        // The embedded newline is one row, not two: four rows, not five.
        assert_eq!(store.len(), 4);
        assert_eq!(store.save(false).unwrap(), SaveOutcome::Unchanged, "the writer must emit what the reader read");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A no-op save is the case the UI performs on every focus change, and a rewritten mtime there
    /// would make every staleness check downstream fire.
    #[test]
    fn a_no_op_save_does_not_touch_the_file() {
        let dir = scratch("noop");
        let path = write(&dir, &format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,note\n"));
        // An mtime in the past, so a write would be obvious.
        let quiet = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(quiet).unwrap();

        let mut store = FlagStore::load(&path).unwrap();
        assert_eq!(store.save(false).unwrap(), SaveOutcome::Unchanged);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), quiet);

        // Changing a note is not a no-op, and the same mtime check proves the write happened.
        let when = store.flags().next().unwrap().1.when;
        assert_eq!(store.set_note(when, "typed it in"), 1);
        assert_eq!(store.save(false).unwrap(), SaveOutcome::Written);
        assert_ne!(std::fs::metadata(&path).unwrap().modified().unwrap(), quiet);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_datetime_spellings_are_read_and_written_in_the_stored_form() {
        let dir = scratch("spellings");
        let body = "thumbnail,datetime,note\n\
AAA,2026-09-21 21:16:12,iso\n\
AAA,2026/09/21   21:16:12,editor\n";
        let path = write(&dir, body);
        let mut store = FlagStore::load(&path).unwrap();
        let times: Vec<String> = store.flags().map(|(_, f)| f.stored_datetime()).collect();
        assert_eq!(times, vec!["2026-09-21 21:16:12", "2026-09-21 21:16:12"]);
        assert_eq!(store.len(), 2, "a row in the editor's own spelling is not a row we cannot read");

        store.save(false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,iso\nAAA,2026-09-21 21:16:12,editor\n"));
        // And the file that comes back is readable again, with the same instants.
        assert_eq!(FlagStore::load(&path).unwrap().flags().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The upstream bug, stated as the behaviour that replaces it.
    #[test]
    fn deleting_every_row_empties_the_table_and_keeps_the_file() {
        let dir = scratch("delete-all");
        let path = write(&dir, &format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,one\nAAA,2026-09-21 21:17:00,two\n"));
        let mut store = FlagStore::load(&path).unwrap();
        let indices: Vec<usize> = (0..store.len()).collect();
        assert_eq!(store.remove_rows(&indices), 2, "select-all is two removals, not one file removal");
        assert_eq!(store.save(false).unwrap(), SaveOutcome::Written);

        assert!(path.exists(), "the file is never sent to the trash");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HEADER_ONLY);
        let again = FlagStore::load(&path).unwrap();
        assert!(again.is_empty());
        assert_eq!(again.len(), 0);
        // The Python app reads this file and gets its three columns back.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_one_row_leaves_the_others_exactly_as_they_were() {
        let dir = scratch("remove-one");
        let path = write(
            &dir,
            "thumbnail,datetime,note\nAAA,2026-09-21 21:16:12,\"keep, this\"\nBBB,2026-09-21 21:17:00,drop\nCCC,2026-09-21 21:18:00,\"say \"\"hi\"\"\"\n",
        );
        let mut store = FlagStore::load(&path).unwrap();
        let removed = store.remove_index(1).unwrap();
        assert_eq!(removed.note(), "drop");
        store.save(false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "thumbnail,datetime,note\nAAA,2026-09-21 21:16:12,\"keep, this\"\nCCC,2026-09-21 21:18:00,\"say \"\"hi\"\"\"\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_row_at_one_instant_is_removed_together() {
        let dir = scratch("remove-at");
        let path = write(&dir, &format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,one\nBBB,2026-09-21 21:16:12,two\nCCC,2026-09-21 21:17:00,three\n"));
        let mut store = FlagStore::load(&path).unwrap();
        let when = store.flags().next().unwrap().1.when;
        assert_eq!(store.remove_at(when), 2);
        assert_eq!(store.remove_at(LocalParts::from_stamp("2020-01-01_00-00-00").unwrap()), 0);
        store.save(false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{HEADER_ONLY}CCC,2026-09-21 21:17:00,three\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two windows write this file: the tray appends while an editor holds a snapshot. An append must
    /// not rewrite the rows beside it, and a whole-table write must notice it is about to.
    #[test]
    fn an_append_keeps_a_row_another_writer_added() {
        let dir = scratch("concurrent");
        let path = table(&dir);
        let mut tray = FlagStore::load(&path).unwrap();
        let mut editor = FlagStore::load(&path).unwrap();

        tray.append_persisted(flag_at("2026-09-21_21-16-12", "first"), false).unwrap();
        editor.append_persisted(flag_at("2026-09-21_21-17-00", "second"), false).unwrap();

        let both = FlagStore::load(&path).unwrap();
        assert_eq!(both.len(), 2, "neither append lost the other");
        let notes: Vec<&str> = both.flags().map(|(_, f)| f.note.as_str()).collect();
        assert_eq!(notes, vec!["first", "second"]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,first\nAAA,2026-09-21 21:17:00,second\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_whole_table_write_refuses_a_file_that_moved_under_it() {
        let dir = scratch("stale");
        let path = write(&dir, &format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,mine\n"));
        let mut editor = FlagStore::load(&path).unwrap();
        // The tray appends while the editor window is open.
        FlagStore::load(&path).unwrap().append_persisted(flag_at("2026-09-21_21-20-00", "theirs"), false).unwrap();

        editor.add(flag_at("2026-09-21_21-30-00", "also mine"));
        let error = editor.save(false).unwrap_err();
        assert!(matches!(error, StoreError::ChangedOnDisk(_)), "{error}");
        // Refusing means the file is untouched, both rows still there, and nothing staged behind.
        assert_eq!(FlagStore::load(&path).unwrap().len(), 2);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no staging file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_reports_the_change_and_writes_nothing() {
        let dir = scratch("dry");
        let path = write(&dir, &format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,mine\n"));
        let before = std::fs::read_to_string(&path).unwrap();
        let mut store = FlagStore::load(&path).unwrap();
        store.remove_index(0);
        assert_eq!(store.save(true).unwrap(), SaveOutcome::Written, "the plan says a write is needed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "and nothing happened");

        // The row it would take is numbered against the in-memory snapshot, which is empty: the
        // removal above is staged, not written.
        assert_eq!(store.append_persisted(flag_at("2026-09-21_22-00-00", "would be"), true).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // An absent file stays absent.
        let fresh = table(&scratch("dry-missing"));
        let dir2 = fresh.parent().unwrap().to_path_buf();
        let mut store = FlagStore::load(&fresh).unwrap();
        assert!(store.ensure_exists(true).unwrap());
        assert!(!fresh.exists());
        store.append_persisted(flag_at("2026-09-21_22-00-00", "_"), true).unwrap();
        assert!(!fresh.exists());
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// A row this crate cannot understand is preserved exactly, because "the app did not recognise my
    /// line" must never become "the app deleted my line".
    #[test]
    fn an_unreadable_row_survives_a_save_unchanged() {
        let dir = scratch("raw");
        let body = "thumbnail,datetime,note\n\
AAA,yesterday,hand edited\n\
AAA,2026-09-21 21:16:12,spare column,fourth field\n\
AAA,2026-09-21 21:17:00,fine\n";
        let path = write(&dir, body);
        let mut store = FlagStore::load(&path).unwrap();
        assert_eq!(store.flags().count(), 1);
        assert_eq!(store.get(0).unwrap().datetime_text(), "yesterday");
        assert_eq!(store.get(1).unwrap().fields().len(), 4);

        let when = store.flags().next().unwrap().1.when;
        assert_eq!(store.set_note(when, "edited"), 1);
        store.save(false).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("AAA,yesterday,hand edited\n"), "{after}");
        assert!(after.contains("AAA,2026-09-21 21:16:12,spare column,fourth field\n"), "{after}");
        assert!(after.contains("AAA,2026-09-21 21:17:00,edited\n"), "{after}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_empty_table_that_can_be_created() {
        let dir = scratch("missing");
        let path = table(&dir);
        let mut store = FlagStore::load(&path).unwrap();
        assert!(store.is_empty() && store.len() == 0);
        assert!(store.ensure_exists(false).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HEADER_ONLY);
        assert!(!store.ensure_exists(false).unwrap(), "an existing table is not rewritten");
        // `ensure_exists` put those bytes there, so the identical save that follows is the no-op it
        // should be rather than a second write of the same header.
        assert_eq!(store.save(false).unwrap(), SaveOutcome::Unchanged);
        store = FlagStore::load(&path).unwrap();
        assert_eq!(store.save(false).unwrap(), SaveOutcome::Unchanged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_append_creates_the_file_with_the_header_the_python_app_expects() {
        let dir = scratch("append-creates");
        let path = table(&dir);
        let mut store = FlagStore::load(&path).unwrap();
        let index = store.append_persisted(flag_at("2026-09-21_21-16-12", "_"), false).unwrap();
        assert_eq!(index, 0);
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,_\n"));
        // Readable by pandas' own header, which is the whole point of this test.
        assert_eq!(&first.lines().next().unwrap().split(',').collect::<Vec<_>>(), &vec!["thumbnail", "datetime", "note"]);
        assert_eq!(store.append_persisted(flag_at("2026-09-21_21-17-00", "_"), false).unwrap(), 1);
        assert_eq!(FlagStore::load(&path).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_are_selected_by_day_not_by_string_prefix() {
        let dir = scratch("on-day");
        let path = write(
            &dir,
            &format!(
                "{HEADER_ONLY}AAA,2026-09-21 21:16:12,monday\nAAA,2026-09-22 09:00:00,tuesday\nAAA,2026-09-23 02:00:00,late tuesday in the product day\n"
            ),
        );
        let store = FlagStore::load(&path).unwrap();
        let tuesday = LocalParts::from_date("2026-09-22").unwrap();
        let span = DaySpan::product_day(tuesday, 180);
        let notes: Vec<&str> = store.on_day(span).into_iter().map(|(_, f)| f.note.as_str()).collect();
        assert_eq!(notes, vec!["tuesday", "late tuesday in the product day"], "02:00 on the 23rd is the 22nd's work");
        assert_eq!(store.on_day(DaySpan::product_day(LocalParts::from_date("2026-09-21").unwrap(), 180)).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_note_is_never_written() {
        let dir = scratch("empty-note");
        let path = table(&dir);
        let mut store = FlagStore::load(&path).unwrap();
        store.add(Flag { thumbnail: String::new(), when: LocalParts::from_stamp("2026-09-21_21-16-12").unwrap(), note: String::new() });
        let when = store.flags().next().unwrap().1.when;
        store.set_note(when, "");
        store.save(false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{HEADER_ONLY},2026-09-21 21:16:12,_\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Editing a note the way the editor does: rewrite exactly the one row the caller pointed at, and
    /// leave every other row's bytes — quotes, embedded commas, everything — exactly as they were.
    #[test]
    fn editing_one_note_changes_only_that_row() {
        let dir = scratch("edit-one");
        let path = write(
            &dir,
            "thumbnail,datetime,note\nAAA,2026-09-21 21:16:12,\"keep, this\"\nBBB,2026-09-21 21:17:00,drop me\nCCC,2026-09-21 21:18:00,\"say \"\"hi\"\"\"\n",
        );
        let mut editor = FlagStore::load(&path).unwrap();
        let row = seen(&editor, 1);
        assert_eq!((row.datetime.as_str(), row.note.as_str()), ("2026-09-21 21:17:00", "drop me"));
        assert_eq!(editor.set_note_ref(&row, "rewritten", false).unwrap(), RowEdit::Done(SaveOutcome::Written));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "thumbnail,datetime,note\nAAA,2026-09-21 21:16:12,\"keep, this\"\nBBB,2026-09-21 21:17:00,rewritten\nCCC,2026-09-21 21:18:00,\"say \"\"hi\"\"\"\n"
        );
        // An edit that says what is already there is a no-op that does not rewrite the file.
        let again = seen(&editor, 1);
        assert_eq!(again.note, "rewritten", "the editor's own snapshot advanced with the write");
        assert_eq!(editor.set_note_ref(&again, "rewritten", false).unwrap(), RowEdit::Done(SaveOutcome::Unchanged));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole reason the guarded delete reloads before it writes: the tray can append while an
    /// editor window is open, and a rewrite of a stale list would drop that append. The delete acts on
    /// the row the caller *saw* and keeps whatever arrived since.
    #[test]
    fn deleting_a_row_keeps_a_flag_another_writer_appended_after_it_was_read() {
        let dir = scratch("delete-concurrent");
        let path = write(&dir, &format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,first\nBBB,2026-09-21 21:17:00,second\nCCC,2026-09-21 21:18:00,third\n"));
        let mut editor = FlagStore::load(&path).unwrap();
        // While the editor is open, the tray appends a fourth flag.
        FlagStore::load(&path).unwrap().append_persisted(flag_at("2026-09-21_21-19-00", "theirs"), false).unwrap();

        // The user deletes the second row from the (now stale) view they were shown.
        let row = seen(&editor, 1);
        assert_eq!(row.note, "second");
        assert_eq!(editor.remove_ref(&row, false).unwrap(), RowEdit::Done(SaveOutcome::Written));
        let after = FlagStore::load(&path).unwrap();
        let notes: Vec<&str> = after.flags().map(|(_, f)| f.note.as_str()).collect();
        assert_eq!(notes, vec!["first", "third", "theirs"], "the appended flag survives and only `second` is gone");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,first\nCCC,2026-09-21 21:18:00,third\nAAA,2026-09-21 21:19:00,theirs\n"),
            "survivors are byte-for-byte what they were, including the late append"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An append at the end never moves an earlier row, so a stale index still lands correctly; but a
    /// concurrent *delete* shifts everything after it. The content fallback means the editor still
    /// removes the row it was shown, not the one that slid into its position.
    #[test]
    fn a_delete_takes_the_row_it_was_shown_even_after_a_concurrent_delete_shifted_the_index() {
        let dir = scratch("delete-shift");
        let path = write(&dir, &format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,one\nBBB,2026-09-21 21:17:00,two\nCCC,2026-09-21 21:18:00,three\n"));
        let mut editor = FlagStore::load(&path).unwrap();
        let row = seen(&editor, 2);
        assert_eq!(row.note, "three", "the row the editor is about to delete");
        // Somebody else deletes the first row, so `three` moves from index 2 to index 1.
        let mut other = FlagStore::load(&path).unwrap();
        assert_eq!(other.remove_ref(&seen(&other, 0), false).unwrap(), RowEdit::Done(SaveOutcome::Written));

        assert_eq!(editor.remove_ref(&row, false).unwrap(), RowEdit::Done(SaveOutcome::Written));
        let after = FlagStore::load(&path).unwrap();
        let notes: Vec<&str> = after.flags().map(|(_, f)| f.note.as_str()).collect();
        assert_eq!(notes, vec!["two"], "only the row the editor meant is gone, not the one that moved up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wrong-row guarantee, stated as its refusal: when a stale index no longer points at a row
    /// that can be told apart from its neighbours, a delete writes nothing rather than guessing.
    #[test]
    fn a_delete_refuses_to_guess_between_two_identical_rows() {
        let dir = scratch("delete-ambiguous");
        let path = write(&dir, &format!("{HEADER_ONLY}XXX,2026-09-21 21:00:00,gone\nAAA,2026-09-21 21:16:12,dupe\nAAA,2026-09-21 21:16:12,dupe\n"));
        let mut editor = FlagStore::load(&path).unwrap();
        let row = seen(&editor, 2);
        let mut other = FlagStore::load(&path).unwrap();
        // Drop the leading row so both `dupe`s shift left and the editor's index 2 runs off the end.
        assert_eq!(other.remove_ref(&seen(&other, 0), false).unwrap(), RowEdit::Done(SaveOutcome::Written));

        assert_eq!(editor.remove_ref(&row, false).unwrap(), RowEdit::NotFound);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,dupe\nAAA,2026-09-21 21:16:12,dupe\n"),
            "a refused delete left the file untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dry run answers "what would happen" and touches nothing: no bytes, no mtime, no staging file.
    #[test]
    fn a_row_dry_run_reports_and_writes_nothing() {
        let dir = scratch("row-dry");
        let path = write(&dir, &format!("{HEADER_ONLY}AAA,2026-09-21 21:16:12,one\nBBB,2026-09-21 21:17:00,two\n"));
        let before = std::fs::read_to_string(&path).unwrap();
        let quiet = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(quiet).unwrap();

        let mut editor = FlagStore::load(&path).unwrap();
        assert_eq!(editor.remove_ref(&seen(&editor, 0), true).unwrap(), RowEdit::Done(SaveOutcome::Written));
        assert_eq!(editor.set_note_ref(&seen(&editor, 1), "changed", true).unwrap(), RowEdit::Done(SaveOutcome::Written));
        assert_eq!(editor.set_note_ref(&seen(&editor, 1), "two", true).unwrap(), RowEdit::Done(SaveOutcome::Unchanged));
        // A row the panel never saw cannot be deleted by a made-up reference.
        let ghost = RowRef { datetime: "2020-01-01 00:00:00".into(), note: "nothing".into(), index: 99 };
        assert_eq!(editor.remove_ref(&ghost, true).unwrap(), RowEdit::NotFound);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "and nothing happened");
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), quiet, "a dry run does not touch the file");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no staging file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
