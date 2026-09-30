//! The user's own bookmarks, which are a CSV file and not a database table.
//!
//! `userdata/flag_mark_note.csv` is written by the tray icon and edited by the day view, so read,
//! append, edit and delete all go through `wind-notes`' [`FlagStore`] — the crate that owns the file's
//! contract — and not through a second reader or writer here. That is the whole point of this module:
//! it is a thin projection of the store onto what the panel draws, and every mutation it offers is
//! one call into the single implementation that knows how to rewrite the table without losing a row.
//!
//! Four shapes of work reach the file:
//!
//!   * [`for_day`] reads a day's flags for the panel;
//!   * [`add_at`] appends the row behind the OneDay 🚩 button (an empty thumbnail is correct there —
//!     the strip already holds that frame, and the button flags a moment that has already passed);
//!   * [`edit_note`] rewrites exactly one row's note through [`FlagStore::set_note_persisted`];
//!   * [`remove`] deletes exactly one row through [`FlagStore::remove_persisted`].
//!
//! The last two are the destructive ones, so they resolve the target row against a *fresh* read (see
//! `FlagStore::locate`) rather than trusting the position the panel was shown: a flag the tray
//! appended in the meantime is kept, and a row that has shifted or gone is refused, never guessed at.

use serde::Serialize;
use std::path::Path;

use wind_base::LocalParts;
use wind_notes::flag::{Flag, NOTE_PLACEHOLDER};
use wind_notes::store::{FlagStore, RowEdit, RowRef, SaveOutcome, StoreError};

/// The outcome of an edit or a delete, in the words the panel needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagEdit {
    /// The row was written.
    Saved,
    /// The requested text was already what the row held, so the file was left alone.
    Unchanged,
    /// The row is no longer there to change — reload the list and look again.
    Gone,
    /// Somebody else wrote the file between our read and our write, so nothing was changed. Reload and
    /// retry; the concurrent append is still there.
    Conflict,
}

/// One flag of a day, as the panel draws it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagNote {
    /// The `%Y-%m-%d %H:%M:%S` string as stored.
    pub when: String,
    pub note: String,
    /// Parsed into the app's epoch, so a row the reader understood always has one.
    pub time: Option<i64>,
    /// This row's absolute position in the file, which is the address [`edit_note`] and [`remove`]
    /// hand to the store. It is resolved by content against a fresh read before it is trusted.
    pub index: usize,
    /// Whether the row carries a picture, so the panel can tell a frame-less bookmark from a thumbnail
    /// it simply has not decoded.
    pub has_thumbnail: bool,
}

/// Every flag whose timestamp falls inside `[from, to]`, oldest first.
///
/// A row whose date cannot be parsed is skipped rather than guessed at: without a timestamp it
/// belongs to no day, and showing it on every day the user opens would be a lie about the data. The
/// store keeps those rows verbatim and rewrites them untouched, so skipping them in the *view* never
/// loses them in the *file*.
pub fn for_day(path: &Path, from: i64, to: i64) -> Vec<FlagNote> {
    let store = match FlagStore::load(path) {
        Ok(store) => store,
        Err(StoreError::Io(_)) | Err(StoreError::ChangedOnDisk(_)) => return Vec::new(),
    };
    let mut out: Vec<FlagNote> = store
        .flags()
        .filter(|(_, flag)| flag.epoch() >= from && flag.epoch() <= to)
        .map(|(index, flag)| FlagNote {
            when: flag.stored_datetime(),
            note: flag.note.clone(),
            time: Some(flag.epoch()),
            index,
            has_thumbnail: flag.has_thumbnail(),
        })
        .collect();
    // Oldest first, and a row that sorts before an identical instant by its file position, so the two
    // flags taken in the same second keep a stable order between a read and the edit that names one.
    out.sort_by_key(|flag| (flag.time.unwrap_or(i64::MIN), flag.index));
    out
}

/// Flag the moment the OneDay scrubber is on, the way the tray's 🚩 does but without a grab: this is a
/// moment that has already passed and is on the strip by virtue of being flagged.
pub fn add_at(path: &Path, time: i64, note: &str) -> Result<(), String> {
    let when = LocalParts::from_naive_epoch(time);
    let note = if note.trim().is_empty() { NOTE_PLACEHOLDER.to_string() } else { note.to_string() };
    let mut store = FlagStore::load(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // A single-line append, never a rewrite, so a flag taken here cannot drop a row another writer
    // added while this window was open.
    store.append_persisted(Flag { thumbnail: String::new(), when, note }, false).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// Rewrite the note of one flagged row, addressed by the `(datetime, note, position)` the panel showed.
/// Goes through the store's guarded whole-table write, which re-finds the row by content first.
pub fn edit_note(path: &Path, when: &str, note: &str, index: usize, new_note: &str) -> Result<FlagEdit, String> {
    let mut store = FlagStore::load(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let row = RowRef { datetime: when.to_string(), note: note.to_string(), index };
    let new_note = if new_note.trim().is_empty() { NOTE_PLACEHOLDER.to_string() } else { new_note.to_string() };
    let edit = store.set_note_ref(&row, &new_note, false).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(apply(edit))
}

/// Delete one flagged row, addressed the same guarded way.
pub fn remove(path: &Path, when: &str, note: &str, index: usize) -> Result<FlagEdit, String> {
    let mut store = FlagStore::load(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let row = RowRef { datetime: when.to_string(), note: note.to_string(), index };
    let edit = store.remove_ref(&row, false).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(apply(edit))
}

fn apply(edit: RowEdit) -> FlagEdit {
    match edit {
        RowEdit::Done(SaveOutcome::Written) => FlagEdit::Saved,
        RowEdit::Done(SaveOutcome::Unchanged) => FlagEdit::Unchanged,
        RowEdit::NotFound => FlagEdit::Gone,
        RowEdit::Conflict => FlagEdit::Conflict,
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("windui-flags-{tag}-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_missing_file_is_an_empty_list_not_an_error() {
        let path = dir("absent").join("flag_mark_note.csv");
        assert!(for_day(&path, 0, i64::MAX).is_empty());
    }

    #[test]
    fn only_the_requested_window_is_returned_and_it_is_ordered() {
        let d = dir("window");
        let path = d.join("flag_mark_note.csv");
        add_at(&path, stamp("2026-09-21 09:00:00"), "early").unwrap();
        add_at(&path, stamp("2026-09-22 09:00:00"), "other day").unwrap();
        add_at(&path, stamp("2026-09-21 23:00:00"), "late, same day").unwrap();
        let from = LocalParts::from_stamp("2026-09-21_03-00-00").unwrap().naive_epoch_seconds();
        let to = LocalParts::from_stamp("2026-09-22_02-59-59").unwrap().naive_epoch_seconds();
        let flags = for_day(&path, from, to);
        assert_eq!(flags.iter().map(|f| f.note.as_str()).collect::<Vec<_>>(), vec!["early", "late, same day"]);
        assert_eq!(flags[1].when, "2026-09-21 23:00:00");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_flag_written_by_the_ui_can_be_read_back_by_the_ui() {
        let d = dir("roundtrip");
        let path = d.join("flag_mark_note.csv");
        let time = stamp("2026-09-21 10:00:00");
        add_at(&path, time, "").unwrap();
        let flags = for_day(&path, time - 1, time + 1);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].note, "_", "an empty note is stored the way upstream stores it");
        assert!(!flags[0].has_thumbnail, "the OneDay button flags a past moment and grabs nothing");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The end-to-end edit: append several rows, rewrite one note through the UI's own save path, and
    /// read the file back — the new text is there, the row count is unchanged, and every other row is
    /// byte-for-byte what it was.
    #[test]
    fn editing_a_note_rewrites_only_that_row_on_a_real_file() {
        let d = dir("edit-e2e");
        let path = d.join("flag_mark_note.csv");
        std::fs::write(
            &path,
            "thumbnail,datetime,note\nAAA,2026-09-21 09:00:00,\"keep, this\"\nBBB,2026-09-21 10:00:00,typo\nCCC,2026-09-21 11:00:00,\"say \"\"hi\"\"\"\n",
        )
        .unwrap();
        let from = LocalParts::from_stamp("2026-09-21_03-00-00").unwrap().naive_epoch_seconds();
        let to = LocalParts::from_stamp("2026-09-22_02-59-59").unwrap().naive_epoch_seconds();
        let before = for_day(&path, from, to);
        assert_eq!(before.len(), 3);
        let target = before.iter().find(|f| f.note == "typo").expect("the typo row");

        assert_eq!(edit_note(&path, &target.when, &target.note, target.index, "corrected").unwrap(), FlagEdit::Saved);

        let after = for_day(&path, from, to);
        assert_eq!(after.len(), 3, "an edit changes a note, not the row count");
        assert_eq!(after[target.index].note, "corrected");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "thumbnail,datetime,note\nAAA,2026-09-21 09:00:00,\"keep, this\"\nBBB,2026-09-21 10:00:00,corrected\nCCC,2026-09-21 11:00:00,\"say \"\"hi\"\"\"\n",
            "only the target row's note changed; neighbours keep their quoting"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The end-to-end delete, and its confirm-gate at the model boundary: `remove` is the write, and
    /// the panel is what decides whether to call it (see the render tests for the confirm step).
    #[test]
    fn deleting_a_row_takes_only_that_row_from_a_real_file() {
        let d = dir("remove-e2e");
        let path = d.join("flag_mark_note.csv");
        std::fs::write(
            &path,
            "thumbnail,datetime,note\nAAA,2026-09-21 09:00:00,\"keep, this\"\nBBB,2026-09-21 10:00:00,drop me\nCCC,2026-09-21 11:00:00,\"say \"\"hi\"\"\"\n",
        )
        .unwrap();
        let from = LocalParts::from_stamp("2026-09-21_03-00-00").unwrap().naive_epoch_seconds();
        let to = LocalParts::from_stamp("2026-09-22_02-59-59").unwrap().naive_epoch_seconds();
        let before = for_day(&path, from, to);
        let target = before.iter().find(|f| f.note == "drop me").expect("the row to drop");

        assert_eq!(remove(&path, &target.when, &target.note, target.index).unwrap(), FlagEdit::Saved);

        let after = for_day(&path, from, to);
        assert_eq!(after.len(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "thumbnail,datetime,note\nAAA,2026-09-21 09:00:00,\"keep, this\"\nCCC,2026-09-21 11:00:00,\"say \"\"hi\"\"\"\n",
            "the survivors are byte-for-byte what they were"
        );
        // The row is gone, so naming it again (its `(datetime, note)` no longer in the file) is a
        // `Gone`, not a second deletion of whatever happens to sit at that position now.
        assert_eq!(remove(&path, &target.when, &target.note, target.index).unwrap(), FlagEdit::Gone);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The concurrency contract, from the UI's side: a flag the tray appended between the read that
    /// filled the panel and the delete that the panel issued must survive the delete untouched.
    #[test]
    fn a_delete_keeps_a_flag_the_tray_appended_after_the_panel_read() {
        let d = dir("remove-race");
        let path = d.join("flag_mark_note.csv");
        let from = LocalParts::from_stamp("2026-09-21_03-00-00").unwrap().naive_epoch_seconds();
        let to = LocalParts::from_stamp("2026-09-22_02-59-59").unwrap().naive_epoch_seconds();
        add_at(&path, stamp("2026-09-21 09:00:00"), "one").unwrap();
        add_at(&path, stamp("2026-09-21 10:00:00"), "two").unwrap();

        // The panel read the day, then the tray appends a third flag for a moment in the same day.
        let shown = for_day(&path, from, to);
        assert_eq!(shown.len(), 2);
        add_at(&path, stamp("2026-09-21 11:00:00"), "theirs").unwrap();

        // Delete the second row the panel was shown; the append at the end must not be clobbered.
        let gone = &shown[1];
        assert_eq!(remove(&path, &gone.when, &gone.note, gone.index).unwrap(), FlagEdit::Saved);
        let notes: Vec<String> = for_day(&path, from, to).into_iter().map(|f| f.note).collect();
        assert_eq!(notes, vec!["one", "theirs"], "the appended flag is still there and only `two` is gone");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn stamp(text: &str) -> i64 {
        wind_notes::flag::parse_datetime(text).unwrap().naive_epoch_seconds()
    }
}
