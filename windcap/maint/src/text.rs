//! The text the capture tick did not read.
//!
//! A recorder with a maintenance window named writes pixels, a window title and a timestamp — the three
//! things that cannot be recovered later — and leaves the recognising to this step. It is the only place
//! in the workspace allowed to read those frames' text, because it reads the **masked** copy: the copy
//! written at capture time, where the mask, the panel geometry and the pixels were all live. Going back
//! to the raw JPEG here would be recognising exactly the edges the user asked never to become searchable,
//! which is why this step skips a row whose `_cropped` sibling is missing rather than falling back to
//! the frame beside it.
//!
//! It runs first in the pipeline, ahead of `convert`: the slice directories are what this reads, and the
//! steps after it are the ones that rename, encode and eventually recycle them.
//!
//! Repeats are folded here too. The tick cannot fold them — with no text yet, every row would look
//! identical to its neighbour and an unguarded fold would collapse a whole segment into one line — so the
//! rows this pass finds unsearchably similar to the one before it lose their *row*, and keep their file:
//! the footage is what the video is cut from, and deleting a frame on a text judgement would be data loss
//! dressed as deduplication.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use wind_base::config::Config;
use wind_base::ocr::Engine;
use wind_base::paths;
use wind_store::maintain::text_similarity;
use wind_store::{discover, write::Store};

/// What one run of this step saw and did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Waiting rows looked at, oldest first.
    pub rows: usize,
    /// Rows that gained their text.
    pub filled: usize,
    /// Rows whose masked copy is not on disk — reported, never guessed at.
    pub missing_copy: usize,
    /// Rows the engine could not read this time. They stay waiting, so a next pass tries again.
    pub unreadable: usize,
    /// Rows folded as repeats of the row before them.
    pub repeats: usize,
    /// Month files that held waiting rows.
    pub months: usize,
}

impl Outcome {
    fn is_empty(&self) -> bool {
        self.rows == 0
    }
}

/// The cache's slice directories, from one listing, for the whole of a [`run`].
///
/// Keyed by the stamp a directory name opens with, which is the fold [`wind_base::paths`] already makes
/// for the other steps: a finished segment is renamed with a marker after its stamp (`-VIDEO`,
/// `-SCREENSHOTS-OCRED`), and a suffix past the first 19 characters is the same recording rather than a
/// different one ([`paths::segment_stamp_of`]). A name that does not open with a real stamp is not in here
/// at all, which is what keeps a corrupt or hand-edited row from pointing this step at a folder the user
/// never recorded.
///
/// One stamp can hold more than one directory — a slice and a marked sibling of the same opening stamp —
/// so the value is the list of them rather than the first one the listing happened to reach. What decides
/// a hit has not moved with the listing: a candidate counts only if it holds this row's frame file.
#[derive(Default)]
struct SliceIndex {
    by_stamp: BTreeMap<String, Vec<PathBuf>>,
}

impl SliceIndex {
    /// `cache_screenshot`, listed once.
    ///
    /// An absent or unreadable cache root is an empty index, which is the same answer a failed `read_dir`
    /// gave every row before: an install that has not finished a segment has nothing to look up, and each
    /// row reports its masked copy as missing instead of the step failing.
    fn scan(cache: &Path) -> SliceIndex {
        let mut index = SliceIndex::default();
        let Ok(entries) = std::fs::read_dir(cache) else {
            return index;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            if let Some(stamp) = paths::segment_stamp_of(&name) {
                index.by_stamp.entry(stamp).or_default().push(entry.path());
            }
        }
        // Tried in name order: a directory listing has no defined order, and a row's answer must not
        // depend on it. Sorted, the plain `{stamp}` is asked for the frame before its `{stamp}-…`
        // sibling, so an unconverted slice wins the way the prefix match used to win it.
        for dirs in index.by_stamp.values_mut() {
            dirs.sort();
        }
        index
    }

    /// The slice directory a row's frame sits in, resolved against the listing already taken.
    fn slice_dir(&self, video: &str, picture: &str) -> Option<PathBuf> {
        // The row's *video* names the directory, never its picture: a frame is named for the second it
        // was caught, so only the segment's first row shares the directory's stamp. The name's own
        // pipeline suffix is invisible here because both sides are folded to the same 19 characters.
        let stamp = paths::segment_stamp_of(video)?;
        self.by_stamp.get(&stamp)?.iter().find(|dir| dir.join(picture).is_file()).cloned()
    }
}

/// The slice directory a row's frame sits in, taking the listing itself — the shape this step used to run
/// on every row, kept whole so the rule can be pinned without a month file, a store and an engine.
///
/// Nothing in production takes it: a step that walks rows lists the cache once and resolves each row
/// against that index (see [`run`]), and the other steps that need this listing go through
/// [`wind_base::paths::slice_dirs`], which is the same fold. Test-only so it does not stand in the
/// shipped binary as an unused path.
#[cfg(test)]
fn slice_dir(cache: &Path, video: &str, picture: &str) -> Option<PathBuf> {
    SliceIndex::scan(cache).slice_dir(video, picture)
}

/// Is this the same screen as the row before it?
///
/// Compared against the *previous kept* row rather than the segment's last one, which is the rule
/// `collapse_repeats` follows in the live path: two rows a minute apart with the same text are the same
/// screen, and the second adds nothing a search could not already find.
fn is_repeat(previous: &Option<String>, content: &str, threshold: f64) -> bool {
    match previous {
        Some(before) if !before.is_empty() && !content.is_empty() => {
            text_similarity(before, content) * 100.0 >= threshold
        }
        _ => false,
    }
}

/// Fill in the text of every row that is waiting for it, oldest month first.
pub fn run(config: &Config, dry_run: bool, limit: Option<usize>) -> Result<Outcome, String> {
    let cache = config.cache_screenshot_dir();
    // The cache listed once for the step instead of once per row. A night's work here is ~3 800 rows over
    // an install whose cache holds ~230 entries, and a fresh `read_dir` per row meant the whole pass spent
    // its window re-finding folders that had not moved — the rows themselves, and the frames they name,
    // are still asked the same way. The snapshot is safe to hold for the whole run because this step never
    // creates, renames or deletes a slice directory: it folds rows and leaves their files on disk, and
    // `convert`, which is the step that renames them, runs after it.
    let slices = SliceIndex::scan(&cache);
    // The same selection the recorder makes, so a window that fills rows in with a different engine than
    // the one that would have read them live is not silently rewriting the user's index.
    let engine = Engine::select(config);
    let threshold = config.f64_or("ocr_compare_similarity_in_table", 94.0);
    let mut total = Outcome::default();

    for month in discover(&config.db_dir()) {
        let mut store = Store::open(&month.path).map_err(|e| format!("{}: {e}", month.path.display()))?;
        let waiting = store.pending_text().map_err(|e| format!("{}: {e}", month.path.display()))?;
        if waiting.is_empty() {
            continue;
        }
        total.months += 1;
        let mut previous: Option<String> = None;
        let mut folded: Vec<i64> = Vec::new();
        let mut done = 0usize;

        // Rows in order, on this thread. The fold compares each row with the *previous kept* one, so the
        // chain is what decides a repeat and it has to be walked once, oldest first: only the directory
        // listing came out of this loop, and putting the rows on pool lanes would change which of them
        // turn out to repeat.
        for (rowid, video, picture, title) in &waiting {
            if limit.is_some_and(|cap| done >= cap) {
                break;
            }
            // One row of OCR is the unit a stop request is worth answering inside: this loop reads a
            // frame per row, and a night's rows are minutes.
            if !wind_base::maintain::may_continue(config) {
                break;
            }
            done += 1;
            total.rows += 1;
            // One row read is one picture for the 文字识别 counter, published at most once a second
            // (`wind_base::maintain`).
            wind_base::maintain::add_items(wind_base::maintain::Leg::Text, 1);
            let Some(dir) = slices.slice_dir(video, picture) else {
                total.missing_copy += 1;
                continue;
            };
            let cropped = dir.join(picture.replace(".jpg", "_cropped.jpg"));
            if !cropped.is_file() {
                // No masked copy means the frame predates this step, or the write failed and said so in
                // the recorder's log. Reading the raw JPEG instead would index what the mask exists to
                // keep out, so the row is reported and left waiting.
                total.missing_copy += 1;
                continue;
            }
            if dry_run {
                total.filled += 1;
                continue;
            }
            match engine.recognize(&cropped) {
                Ok(body) => {
                    // Composed the way the live path composes it: the title belongs in the searchable
                    // body, and history was indexed that way.
                    let content = wind_store::Record {
                        videofile_name: String::new(),
                        picturefile_name: String::new(),
                        videofile_time: 0,
                        ocr_text: body.clone(),
                        win_title: title.clone(),
                        deep_linking: None,
                        thumbnail: None,
                    }
                    .indexed_text();
                    if is_repeat(&previous, &content, threshold) {
                        folded.push(*rowid);
                        total.repeats += 1;
                        continue;
                    }
                    store.fill_text(*rowid, &body, title.as_deref()).map_err(|e| e.to_string())?;
                    previous = Some(content);
                    total.filled += 1;
                }
                Err(_) => total.unreadable += 1,
            }
        }

        if !folded.is_empty() {
            store.delete_rows(&folded).map_err(|e| e.to_string())?;
        }
    }

    Ok(total)
}

/// The line this step adds to the pass's report, or nothing at all when there was nothing waiting.
pub fn report(outcome: &Outcome, dry_run: bool) -> String {
    if outcome.is_empty() {
        return "text: no row is waiting".to_string();
    }
    format!(
        "text: {} waiting row(s) in {} month file(s), {} filled, {} folded as repeats, \
         {} with no masked copy, {} unreadable{}",
        outcome.rows,
        outcome.months,
        outcome.filled,
        outcome.repeats,
        outcome.missing_copy,
        outcome.unreadable,
        if dry_run { ", dry run: nothing written" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_base::clock::LocalParts;

    fn install(tag: &str, settings: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-text-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), settings).unwrap();
        std::fs::create_dir_all(dir.join("userdata/db")).unwrap();
        std::fs::create_dir_all(dir.join("cache_screenshot")).unwrap();
        dir
    }

    /// A waiting row is one whose text is empty, and `run` has to find it and only it. The engine cannot
    /// be asked in a test — no install is required to have one — so the row is given no masked copy,
    /// which is the branch that proves it was selected and then honestly reported.
    #[test]
    fn a_waiting_row_without_its_masked_copy_is_reported_and_left_waiting() {
        let dir = install("missing", r#"{"maintain_window_start": "03:30", "maintain_window_end": "05:00"}"#);
        let db = dir.join("userdata/db");
        let mut store = Store::open_month(&db, "default", 2026, 9).unwrap();
        let waiting = wind_store::Record {
            videofile_name: "2026-09-27_03-30-00.mp4".into(),
            picturefile_name: "2026-09-27_03-30-00.jpg".into(),
            videofile_time: LocalParts { year: 2026, month: 9, day: 27, hour: 3, minute: 30, second: 0 }
                .naive_epoch_seconds(),
            ocr_text: String::new(),
            win_title: Some("- Explorer".into()),
            deep_linking: None,
            thumbnail: None,
        };
        let read = wind_store::Record { ocr_text: "already read".into(), ..waiting.clone() };
        store.append(&[waiting, read]).unwrap();
        drop(store);

        let config = Config::load(&dir).unwrap();
        let outcome = run(&config, true, None).expect("a dry run reads and reports");
        assert_eq!(outcome.rows, 1, "only the empty-text row is waiting");
        assert_eq!(outcome.missing_copy, 1, "and it has no frame on disk at all");
        assert_eq!(outcome.filled, 0, "nothing was recognised, so nothing was written");

        let store = Store::open_month(&db, "default", 2026, 9).unwrap();
        assert_eq!(store.pending_text().unwrap().len(), 1, "the row is still waiting for the next pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the branch above: the slice is on disk, so the listing the step took once answers
    /// the row, and the masked copy is found beside its frame. A dry run stops exactly there, which is what
    /// makes a fill testable on a machine with no OCR engine installed.
    #[test]
    fn a_row_is_resolved_through_the_listing_the_step_took_once() {
        let dir = install("filled", "{}");
        let db = dir.join("userdata/db");
        let mut store = Store::open_month(&db, "default", 2026, 9).unwrap();
        store
            .append(&[wind_store::Record {
                videofile_name: "2026-09-27_03-30-00.mp4".into(),
                picturefile_name: "2026-09-27_03-30-05.jpg".into(),
                videofile_time: LocalParts { year: 2026, month: 9, day: 27, hour: 3, minute: 30, second: 5 }
                    .naive_epoch_seconds(),
                ocr_text: String::new(),
                win_title: Some("- Notepad".into()),
                deep_linking: None,
                thumbnail: None,
            }])
            .unwrap();
        drop(store);
        // The segment's own folder under `cache_screenshot`, marked the way the pipeline marks a closed
        // one, holding the frame and the masked copy the tick wrote next to it.
        let slice = dir.join("cache_screenshot/2026-09-27_03-30-00-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("2026-09-27_03-30-05.jpg"), b"jpeg").unwrap();
        std::fs::write(slice.join("2026-09-27_03-30-05_cropped.jpg"), b"masked").unwrap();

        let config = Config::load(&dir).unwrap();
        let outcome = run(&config, true, None).expect("a dry run reads and reports");
        assert_eq!((outcome.rows, outcome.missing_copy, outcome.filled), (1, 0, 1), "{outcome:?}");
        assert_eq!(outcome.months, 1, "the month that held it is counted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The prefix match is the only way a row finds its slice once the directory has been marked.
    #[test]
    fn a_frame_is_found_in_the_directory_named_by_its_own_stamp() {
        let dir = install("slice", "{}");
        let slice = dir.join("2026-09-27_03-30-00");
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("2026-09-27_03-30-05.jpg"), b"jpeg").unwrap();
        // A marker directory beside it, which is what the recorder leaves when a segment closes.
        std::fs::create_dir_all(dir.join("2026-09-27_03-30-00-SUBMIT")).unwrap();

        assert_eq!(
            slice_dir(&dir, "2026-09-27_03-30-00.mp4", "2026-09-27_03-30-05.jpg").as_deref(),
            Some(slice.as_path()),
            "the row's video names the directory, the row's picture names the file"
        );
        assert!(
            slice_dir(&dir, "2026-09-27_09-00-00.mp4", "2026-09-27_09-00-05.jpg").is_none(),
            "a frame that is not there is not found"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Folding is a judgement about text, and empty text is not a match for anything.
    #[test]
    fn nothing_is_folded_away_from_a_blank_screen() {
        assert!(!is_repeat(&None, "", 94.0));
        assert!(!is_repeat(&Some("".into()), "words", 94.0));
        assert!(!is_repeat(&Some("words".into()), "", 94.0));
        assert!(is_repeat(&Some("the same page".into()), "the same page", 94.0));
    }
}
