//! Refresh: make the index's existence flags agree with the disk.
//!
//! `is_videofile_exist` and `is_picturefile_exist` are written as constants by the recorder — a row is
//! committed while its video does not exist yet — and corrected by nobody else, so this pass is the
//! only thing keeping the UI's "file is missing" badge honest. It is also the cheapest way to prove the
//! rest of the install is wired up correctly, which is why it never stats per row.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use wind_base::paths;

use rusqlite::{Connection, OpenFlags};
use wind_base::config::Config;
use wind_base::pool::{self, Duty};
use wind_store::maintain::{apply_existence, ensure_time_index, plan_existence};
use wind_store::read::{self, Month, Row};

use crate::layout;

/// The names present under `videos/`, gathered one `read_dir` per month folder.
///
/// Keyed by the 19-character stamp rather than the whole filename because a segment is renamed as it
/// is indexed and compressed (`-INDEX`, `-OCRED`, `-COMPRESS`); matching on the full name would flag
/// every indexed video as missing, which is the exact bug this pass exists to fix.
///
/// The value is a list: a segment legitimately has two files at the moment the retention pass
/// re-compresses it, the original and its `-COMPRESS` replacement.
#[derive(Debug, Default)]
pub struct DiskVideos {
    by_stamp: HashMap<String, Vec<PathBuf>>,
    /// How many directories were listed, so a report can show the scan is proportional to the folders
    /// on disk and not to the rows in the index.
    pub listed: usize,
}

impl DiskVideos {
    pub fn scan(videos_dir: &Path) -> DiskVideos {
        let mut out = DiskVideos::default();
        out.collect(videos_dir, true);
        out
    }

    /// The number of distinct segments found, which is what a report line quotes.
    pub fn segments(&self) -> usize {
        self.by_stamp.len()
    }

    /// Every file on disk for the segment a name refers to.
    pub fn locate(&self, videofile_name: &str) -> &[PathBuf] {
        match layout::segment_stamp_of(videofile_name) {
            Some(stamp) => self.by_stamp.get(&stamp).map(Vec::as_slice).unwrap_or_default(),
            None => &[],
        }
    }

    pub fn has(&self, videofile_name: &str) -> bool {
        !self.locate(videofile_name).is_empty()
    }

    /// A month folder, or the flat root of an install that predates the monthly split. `descend` stops
    /// the walk at one level, so a stray directory of thumbnails inside a month costs one listing.
    fn collect(&mut self, dir: &Path, descend: bool) {
        self.listed += 1;
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if descend {
                    self.collect(&path, false);
                }
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(stamp) = layout::segment_stamp_of(name) {
                    self.by_stamp.entry(stamp).or_default().push(path);
                }
            }
        }
    }
}

/// Resolve a row's `picturefile_name` to the paths it could mean, in the directories that hold its
/// segment.
///
/// The field has been written three ways across the installed base: the re-index pass names the ffmpeg
/// crop it made (`42_cropped.jpg`, under `cache/i_frames/{stamp}`), the screenshot recorder names the
/// frame it grabbed (`{stamp}.jpg`, under the slice's directory in `cache_screenshot`), and a legacy
/// month carries only a bare name whose directory has long since been swept. `wind_base::paths` answers
/// all three, marker renames included — and refuses a stored path that tries to leave those two roots,
/// which is the one shape that would let a corrupt index point this pass, or the deletion pass that
/// trusts its flags, somewhere the user never recorded.
pub fn picture_paths(
    crops: &BTreeMap<String, PathBuf>,
    slices: &BTreeMap<String, PathBuf>,
    segment: &str,
    stored: &str,
) -> Vec<PathBuf> {
    let dirs = paths::frame_dirs_in(crops, slices, segment, stored);
    [dirs.crop, dirs.slice].into_iter().flatten().collect()
}

/// Which of these frame names are on disk, listing each containing directory at most once.
///
/// Returns the answer per distinct name plus the number of directories listed. `plan_existence` already
/// probes once per distinct name, but the frames of one slice share a parent directory and a month's
/// rows are one directory per recorded segment, so a stat per name would be tens of thousands of
/// syscalls where a `read_dir` each answers the same question.
///
/// `crops` and `slices` are the two cache roots as [`paths::slice_dirs`] listed them, taken once for the
/// whole step by [`run`] and handed down by reference. Two reasons, one of them honest arithmetic and one
/// of them correctness: a year of footage is twelve months all pointing at the *same* two roots, so
/// listing them per month was twelve whole `read_dir`s of a folder nobody had changed between them; and
/// one listing means the pass judges every month against the disk as it stood when the step began,
/// instead of letting a late month answer against a cache the recorder or the sweep moved on from while
/// the month beside it was being judged against where that cache had been.
///
/// The per-slice listings below stay per month on purpose: the count this returns is how many directories
/// *this month's* rows pointed at, which is the figure the report line argues with.
pub fn probe_pictures(
    rows: &[Row],
    crops: &BTreeMap<String, PathBuf>,
    slices: &BTreeMap<String, PathBuf>,
) -> (HashMap<String, bool>, usize) {
    // Keyed by name, because that is what a flag is stored against — `plan_existence` answers one
    // question per distinct `picturefile_name`, and the `UPDATE` that follows is name-keyed too. Two
    // segments that share a frame name therefore share a flag; the sweep works a segment at a time, so
    // this is a reporting coarseness rather than a wrong answer about a file.
    let mut distinct: Vec<(&str, &str)> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for row in rows {
        if !row.picturefile_name.is_empty() && seen.insert(row.picturefile_name.as_str()) {
            distinct.push((row.picturefile_name.as_str(), row.videofile_name.as_str()));
        }
    }

    let mut listings: HashMap<PathBuf, Option<HashSet<String>>> = HashMap::new();
    let mut answers = HashMap::new();
    for (name, segment) in distinct {
        let hit = picture_paths(crops, slices, segment, name).into_iter().any(|path| {
            let parent = path.parent().unwrap_or(path.as_path()).to_path_buf();
            let file = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if !listings.contains_key(&parent) {
                listings.insert(parent.clone(), read_listing(&parent));
            }
            listings.get(&parent).and_then(Option::as_ref).is_some_and(|names| names.contains(&file))
        });
        answers.insert(name.to_string(), hit);
    }
    // The two root listings are not counted here, and are no longer taken here either: the number this
    // returns is how many *slice* directories the month's rows pointed at.
    let listed = listings.len();
    (answers, listed)
}

/// The names in a directory, or `None` when it cannot be listed at all — which is how a slice the
/// cache sweep already deleted answers "no" for every one of its frames with a single failed call.
fn read_listing(dir: &Path) -> Option<HashSet<String>> {
    let read = std::fs::read_dir(dir).ok()?;
    let mut names = HashSet::new();
    for entry in read.flatten() {
        if let Some(name) = entry.path().file_name().and_then(|n| n.to_str()) {
            names.insert(name.to_string());
        }
    }
    Some(names)
}

/// What a refresh pass did, summed over the months it visited.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub months: usize,
    pub rows_scanned: usize,
    pub video_names: usize,
    pub picture_names: usize,
    pub directories_listed: usize,
    /// Distinct names whose stored flag disagreed with the disk.
    pub flags_corrected: usize,
    /// Rows the UPDATEs actually touched, which is larger: one name covers every row of a segment.
    pub rows_written: usize,
    /// Month files whose timestamp index this pass created — or, under `--dry-run`, month files that
    /// are missing it, which is the work the next real run will do.
    pub indexes: usize,
}

/// What one month's lane came back with, before the fold sums it into the step's `Outcome`.
///
/// Owns everything in it: the connection it was read from belongs to that lane and is closed by the time
/// the fold looks, so nothing here may borrow it.
#[derive(Debug)]
struct MonthRefresh {
    label: String,
    rows: usize,
    video_names: usize,
    picture_names: usize,
    directories_listed: usize,
    flags_corrected: usize,
    rows_written: usize,
    indexes: usize,
}

/// Refresh one month file on one lane: scan its rows, probe them against the step's listings, and — unless
/// this is a dry run — write the corrections in this month's own transaction.
///
/// Every listing it uses is a borrowed one it cannot change, and every handle it makes it keeps to itself,
/// which is what lets two months be worked at the same instant.
fn refresh_month(
    month: &Month,
    dry_run: bool,
    videos: &DiskVideos,
    crops: &BTreeMap<String, PathBuf>,
    slices: &BTreeMap<String, PathBuf>,
) -> Result<MonthRefresh, String> {
    let label = month.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
    // Opened here, used here, dropped here: a connection never crosses a lane.
    let mut conn = if dry_run {
        open_read_only(&month.path).map_err(|e| format!("{label}: {e}"))?
    } else {
        month.open_write().map_err(|e| format!("{label}: {e}"))?
    };
    let rows = read::rows_in_window(&conn, None, None).map_err(|e| format!("{label}: {e}"))?;
    let (pictures, directories_listed) = probe_pictures(&rows, crops, slices);
    let video_names: HashSet<&str> = rows.iter().map(|r| r.videofile_name.as_str()).collect();
    let plan = plan_existence(
        &rows,
        &|name| videos.has(name),
        &|name| pictures.get(name).copied().unwrap_or(false),
    );
    let flags_corrected = plan.video_existence.len() + plan.picture_existence.len();

    let (rows_written, indexes) = if dry_run {
        // The third, narrower door: nothing written at all, not even the timestamp index. A month that is
        // missing it is counted as work the next real run will do.
        (0, usize::from(!has_time_index(&conn).unwrap_or(true)))
    } else {
        // One transaction for this one month, never merged across months: a lane holding its own month's
        // lock while its neighbour holds another database's is the whole reason the lanes are allowed.
        let tx = conn.transaction().map_err(|e| format!("{label}: {e}"))?;
        let written = apply_existence(&tx, &plan).map_err(|e| format!("{label}: {e}"))?;
        tx.commit().map_err(|e| format!("{label}: {e}"))?;
        (written, usize::from(ensure_time_index(&conn).map_err(|e| format!("{label}: {e}"))?))
    };

    Ok(MonthRefresh {
        label,
        rows: rows.len(),
        video_names: video_names.len(),
        picture_names: pictures.len(),
        directories_listed,
        flags_corrected,
        rows_written,
        indexes,
    })
}

/// Refresh every month file, oldest first, up to `limit` of them.
pub fn run(config: &Config, dry_run: bool, limit: Option<usize>) -> Result<Outcome, String> {
    let months = read::discover(&config.db_dir());
    // The disk, listed once for the step: one walk of `videos/`, one listing of each of the two cache
    // roots. Every month the pass then works is judged against this same view of the same disk, which is
    // both the point of the step and the reason a month's row count cannot change the number of reads.
    let videos = DiskVideos::scan(&config.videos_dir());
    let crops = paths::slice_dirs(&config.iframe_dir());
    let slices = paths::slice_dirs(&config.cache_screenshot_dir());
    println!(
        "videos: {} folder(s) listed, {} distinct segment stamp(s) on disk",
        videos.listed,
        videos.segments()
    );

    // `--limit` bounds the number of MONTHS, so it clips the queue before the pool is asked for anything:
    // a limited run claims exactly the months it was told to and never spends a lane on the rest.
    let wanted = limit.unwrap_or(usize::MAX).min(months.len());
    // A month is a full row scan, existence probes against listings already in hand, one write transaction
    // and one `CREATE INDEX` — mostly disk, so [`Duty::Disk`] is the policy for it (`wind_base::pool`).
    // `rusqlite::Connection` is `!Sync`, so no connection is shared between workers: each one is opened
    // inside `refresh_month` by the lane that uses it and closed there, and that per-month isolation is
    // exactly what makes running the months at the same time safe.
    let slots = pool::run(
        &months[..wanted],
        pool::lanes(Duty::Disk),
        || wind_base::maintain::may_continue(config),
        |month| refresh_month(month, dry_run, &videos, &crops, &slices),
    );

    let mut outcome = Outcome::default();
    // Folded in month order, oldest first, so the counters and the report lines read the way they did when
    // one thread walked the queue: a lane that finished first does not get its line printed first.
    for slot in slots {
        // A slot left empty is a month no lane claimed because a stop was asked — the serial loop's
        // `break`, with the remaining months left for the next window.
        let Some(work) = slot else { break };
        let work = work?;
        println!(
            "{}: {} rows, {} video name(s) + {} frame name(s) probed ({} dir list(s)), {} flag correction(s){}",
            work.label,
            work.rows,
            work.video_names,
            work.picture_names,
            work.directories_listed,
            work.flags_corrected,
            if dry_run { " [dry-run]" } else { "" }
        );

        outcome.months += 1;
        outcome.rows_scanned += work.rows;
        outcome.video_names += work.video_names;
        outcome.picture_names += work.picture_names;
        outcome.directories_listed += work.directories_listed;
        outcome.flags_corrected += work.flags_corrected;
        outcome.rows_written += work.rows_written;
        outcome.indexes += work.indexes;
        if work.rows_written > 0 {
            println!("  wrote {} row(s) of corrections", work.rows_written);
        }
    }
    Ok(outcome)
}

/// The name every month file is expected to carry once the maintenance pass has been round it.
pub const TIME_INDEX_NAME: &str = "video_text_time";

/// Open a month file without writing to it.
///
/// `Month::open_read` is not usable here: it works through the `_TEMP_READ.db` copy, and creating that
/// copy is itself a write into the user's `userdata/db`, which `--dry-run` promises not to make.
/// `Month::open_write` runs the schema migration. This is the third, narrower door.
pub(crate) fn open_read_only(path: &Path) -> Result<Connection, rusqlite::Error> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
}

fn has_time_index(conn: &Connection) -> Result<bool, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
        [TIME_INDEX_NAME],
        |r| r.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wind_base::clock::LocalParts;
    use wind_store::write::{Record, Store};

    fn temp_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-refresh-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn at(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    fn record(video: &str, frame: &str, stamp: &str) -> Record {
        Record {
            videofile_name: video.into(),
            picturefile_name: frame.into(),
            videofile_time: at(stamp),
            ocr_text: "screen text".into(),
            win_title: None,
            deep_linking: None,
            thumbnail: Some("AAA".into()),
        }
    }

    /// A month file plus the disk layout it points at: one video that survived and was renamed by the
    /// indexing pass, one that was never converted, two frames that exist and one that does not.
    fn fixture(tag: &str) -> PathBuf {
        let root = temp_tree(tag);
        let month_dir = root.join("userdata/videos/2026-09");
        fs::create_dir_all(&month_dir).unwrap();
        fs::write(month_dir.join("2026-09-21_21-16-12-OCRED.mp4"), b"v").unwrap();

        // Named and placed exactly as the pipeline leaves them: the slice directory gains its marker
        // when the frames become a video, and what a row stores is the bare frame name.
        let slice = root.join("cache_screenshot/2026-09-21_21-16-12-VIDEO");
        fs::create_dir_all(&slice).unwrap();
        for frame in ["2026-09-21_21-16-12", "2026-09-21_21-16-20"] {
            fs::write(slice.join(format!("{frame}.jpg")), b"j").unwrap();
        }

        let mut store = Store::open_month(&root.join("userdata/db"), "default", 2026, 9).unwrap();
        let kept = "2026-09-21_21-16-12.jpg".to_string();
        let kept_too = "2026-09-21_21-16-20.jpg".to_string();
        let gone = "2026-09-21_21-16-32.jpg".to_string();
        store
            .append(&[
                record("2026-09-21_21-16-12.mp4", &kept, "2026-09-21_21-16-12"),
                record("2026-09-21_21-16-12.mp4", &kept_too, "2026-09-21_21-16-20"),
                record("2026-09-21_21-16-12.mp4", &gone, "2026-09-21_21-16-32"),
                record("2026-09-22_10-00-00.mp4", "", "2026-09-22_10-00-00"),
            ])
            .unwrap();
        drop(store);
        root
    }

    #[test]
    fn a_video_is_found_by_its_stamp_however_the_file_was_renamed() {
        let root = temp_tree("match");
        let month_dir = root.join("userdata/videos/2026-09");
        fs::create_dir_all(&month_dir).unwrap();
        fs::write(month_dir.join("2026-09-21_21-16-12-COMPRESS-OCRED.mp4"), b"v").unwrap();
        fs::write(root.join("userdata/videos/2026-10-01_00-00-00.mp4"), b"flat").unwrap();
        fs::write(month_dir.join("notes.txt"), b"not a video").unwrap();

        let disk = DiskVideos::scan(&root.join("userdata/videos"));
        assert!(disk.has("2026-09-21_21-16-12.mp4"), "the row name must match the renamed file");
        assert!(disk.has("2026-10-01_00-00-00.mp4"), "a flat videos/ layout still counts");
        assert!(!disk.has("notes.txt"), "a name carrying no stamp can never match");
        assert!(!disk.has("2026-09-25_00-00-00.mp4"));
        assert_eq!(disk.listed, 2, "the root plus one month folder, whatever the row count");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_videos_dir_scans_as_empty_instead_of_failing() {
        let disk = DiskVideos::scan(Path::new("Z:/definitely/not/here"));
        assert!(!disk.has("2026-09-21_21-16-12.mp4"));
        assert_eq!(disk.listed, 1);
        assert_eq!(disk.segments(), 0);
    }

    #[test]
    fn a_frame_name_is_resolved_inside_its_segment_and_a_path_is_refused() {
        let root = temp_tree("resolve");
        let (crops, slices) = (paths::slice_dirs(&root.join("cache/i_frames")), paths::slice_dirs(&root.join("cache_screenshot")));
        assert!(
            picture_paths(&crops, &slices, "2026-09-21_21-16-12.mp4", "8.jpg").is_empty(),
            "a segment with no directory on disk has no frame to find"
        );

        fs::create_dir_all(root.join("cache_screenshot/2026-09-21_21-16-12-VIDEO")).unwrap();
        fs::create_dir_all(root.join("cache/i_frames/2026-09-21_21-16-12")).unwrap();
        let (crops, slices) = (paths::slice_dirs(&root.join("cache/i_frames")), paths::slice_dirs(&root.join("cache_screenshot")));
        assert_eq!(
            picture_paths(&crops, &slices, "2026-09-21_21-16-12.mp4", "8.jpg"),
            vec![
                root.join("cache/i_frames/2026-09-21_21-16-12/8.jpg"),
                root.join("cache_screenshot/2026-09-21_21-16-12-VIDEO/8.jpg"),
            ],
            "both roots the pipeline writes are answered, the marker rename included, and the .mp4              suffix never becomes a directory name"
        );
        for escaped in ["D:/x/y.jpg", "../outside.jpg", "a/b.jpg", ""] {
            assert!(
                picture_paths(&crops, &slices, "2026-09-21_21-16-12.mp4", escaped).is_empty(),
                "{escaped:?} was resolved outside the directories this segment owns"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn probing_frames_lists_each_slice_directory_once() {
        let root = fixture("probe");
        let conn = Connection::open(root.join("userdata/db/default_2026-09_wind.db")).unwrap();
        let rows = read::rows_in_window(&conn, None, None).unwrap();
        // The two roots listed once, exactly as `run` lists them once per step and hands them down.
        let crops = paths::slice_dirs(&root.join("cache/i_frames"));
        let slices = paths::slice_dirs(&root.join("cache_screenshot"));
        let (answers, listed) = probe_pictures(&rows, &crops, &slices);
        assert_eq!(answers.len(), 3, "one answer per distinct frame name, not per row");
        assert_eq!(listed, 1, "all three frames live in one slice directory");
        assert_eq!(answers.values().filter(|v| **v).count(), 2, "the third frame was cleaned up");
        assert_eq!(probe_pictures(&[], &crops, &slices).0.len(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refresh_corrects_flags_and_adds_the_index() {
        let root = fixture("apply");
        let config = Config::load(&root).unwrap();
        let outcome = run(&config, false, None).unwrap();

        assert_eq!(outcome.months, 1);
        assert_eq!(outcome.rows_scanned, 4);
        assert_eq!(outcome.flags_corrected, 3, "two frames appear, one video does not");
        assert_eq!(outcome.rows_written, 3);
        assert_eq!(outcome.indexes, 1, "this pass added the timestamp index");

        let conn = Connection::open(root.join("userdata/db/default_2026-09_wind.db")).unwrap();
        let flags: (i64, i64) = conn
            .query_row(
                "SELECT sum(is_videofile_exist), sum(is_picturefile_exist) FROM video_text \
                 WHERE videofile_name = '2026-09-21_21-16-12.mp4'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(flags, (3, 2), "the renamed video counts, the deleted frame does not");
        let missing: i64 = conn
            .query_row(
                "SELECT is_videofile_exist FROM video_text WHERE videofile_name = '2026-09-22_10-00-00.mp4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(missing, 0, "a video that was never converted is flagged off");
        assert!(has_time_index(&conn).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    /// A month of its own, carrying one segment: a video the disk has not got and a frame it still has.
    /// Two of these is a year's index in miniature, and the shape a lane has to handle on its own.
    fn one_month_of(root: &Path, year: i64, month: u32, stamp: &str) {
        let slice = root.join(format!("cache_screenshot/{stamp}-VIDEO"));
        fs::create_dir_all(&slice).unwrap();
        fs::write(slice.join(format!("{stamp}.jpg")), b"j").unwrap();
        let mut store = Store::open_month(&root.join("userdata/db"), "default", year, month).unwrap();
        store.append(&[record(&format!("{stamp}.mp4"), &format!("{stamp}.jpg"), stamp)]).unwrap();
        drop(store);
    }

    #[test]
    fn two_months_are_both_refreshed_and_their_counters_are_the_sum() {
        // The per-month independence the pool is built on: two month files, each opened, scanned, planned
        // and written by a lane of its own, both corrected by one run — with every counter the sum of the
        // two months rather than the last one to finish.
        let root = temp_tree("two-months");
        one_month_of(&root, 2026, 8, "2026-08-05_08-00-00");
        one_month_of(&root, 2026, 9, "2026-09-21_21-16-12");
        let config = Config::load(&root).unwrap();
        let outcome = run(&config, false, None).unwrap();

        assert_eq!(outcome.months, 2);
        assert_eq!(outcome.rows_scanned, 2, "one row apiece, both read");
        assert_eq!(outcome.flags_corrected, 4, "each month: one video gone, one frame found");
        assert_eq!(outcome.rows_written, 4);
        assert_eq!(outcome.picture_names, 2, "two distinct frame names, one per segment");
        assert_eq!(outcome.directories_listed, 2, "each month points at its own slice directory");
        assert_eq!(outcome.indexes, 2, "one timestamp index per month file, not one per step");

        for (year, month, stamp) in [(2026, 8, "2026-08-05_08-00-00"), (2026, 9, "2026-09-21_21-16-12")] {
            let conn = Connection::open(root.join(format!("userdata/db/default_{year}-{month:02}_wind.db"))).unwrap();
            let flags: (i64, i64) = conn
                .query_row(
                    "SELECT is_videofile_exist, is_picturefile_exist FROM video_text WHERE videofile_name = ?1",
                    [format!("{stamp}.mp4")],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(flags, (0, 1), "{stamp}: the missing video is off, the surviving frame is on");
            assert!(has_time_index(&conn).unwrap(), "{year}-{month:02} got its own index");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_refresh_is_a_no_op() {
        let root = fixture("twice");
        let config = Config::load(&root).unwrap();
        run(&config, false, None).unwrap();
        let second = run(&config, false, None).unwrap();
        assert_eq!(second.flags_corrected, 0, "{second:?}");
        assert_eq!(second.rows_written, 0);
        assert_eq!(second.indexes, 0, "the index is not recreated");
        assert_eq!(second.rows_scanned, 4, "the rows are still scanned, they just already agree");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_computes_the_same_plan_and_writes_nothing() {
        let root = fixture("dry");
        let db = root.join("userdata/db/default_2026-09_wind.db");
        let before = fs::read(&db).unwrap();
        let config = Config::load(&root).unwrap();
        let outcome = run(&config, true, None).unwrap();

        assert_eq!(outcome.flags_corrected, 3, "the plan is still reported");
        assert_eq!(outcome.rows_written, 0);
        assert_eq!(outcome.indexes, 1, "the missing index is reported as work to do");
        assert_eq!(fs::read(&db).unwrap(), before, "the index file is byte-identical");
        let leftovers: Vec<String> = fs::read_dir(root.join("userdata/db"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("TEMP_READ"))
            .collect();
        assert!(leftovers.is_empty(), "a dry run must not create the read copy either: {leftovers:?}");
        assert!(!has_time_index(&Connection::open(&db).unwrap()).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn limit_bounds_the_months_touched() {
        let root = temp_tree("limit");
        let db = root.join("userdata/db");
        for (year, month) in [(2026, 8), (2026, 9), (2026, 10)] {
            Store::open_month(&db, "default", year, month).unwrap();
        }
        let config = Config::load(&root).unwrap();
        let outcome = run(&config, true, Some(2)).unwrap();
        assert_eq!(outcome.months, 2);
        assert_eq!(outcome.rows_scanned, 0, "empty months are still visited and reported");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_install_with_no_index_files_reports_no_months() {
        let root = temp_tree("empty");
        let config = Config::load(&root).unwrap();
        let outcome = run(&config, true, None).unwrap();
        assert_eq!(outcome, Outcome::default());
        let _ = fs::remove_dir_all(&root);
    }
}
