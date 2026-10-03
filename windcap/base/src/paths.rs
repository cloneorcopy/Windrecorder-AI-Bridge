//! On-disk naming rules.
//!
//! These are the strings that the whole installed base of user data is keyed by: a recorder that
//! invents a tidier name breaks every existing row's `LIKE '%…%'` lookup, and a reader that parses
//! loosely silently skips a user's history. Everything here is therefore pinned to what upstream
//! already wrote, and the tests quote values out of a live `userdata/` directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A month's index file is `{user}_{YYYY}-{MM}_wind.db` inside `userdata/db`.
pub const DB_SUFFIX: &str = "_wind.db";

/// Readers never open the live file: they work on a copy with this suffix, exactly as
/// `db_manager._get_db_temp_read` does, so the recorder's writes are never blocked by a query.
pub const TEMP_READ_SUFFIX: &str = "_TEMP_READ";

/// Directory markers the screenshot-array pipeline stamps on a finished segment.
pub const MARKER_VIDEO: &str = "-VIDEO";
pub const MARKER_SUBMIT: &str = "-SUBMIT";
pub const MARKER_DISCARD: &str = "-DISCARD";
pub const MARKER_OCRED: &str = "-SCREENSHOTS-OCRED";

/// The marker directory a slice gains once its rows are in the index.
///
/// Nested *inside* the slice rather than a rename of it, and that placement is load-bearing twice
/// over: the maintenance pass discovers work by ignoring any directory whose name already carries a
/// pipeline marker, so renaming the slice would hide it forever; and the presence of this directory
/// is what tells the maintenance pass the recorder has finished writing and closed the segment.
/// Converting a slice that is still open means ffmpeg reads frames as they are appended and then
/// renames the directory out from under the writer, whose next frame fails and whose committed rows
/// point at a path that no longer exists.
pub const SUBMIT_MARKER_DIR: &str = "-SUBMIT";

/// Has this slice been committed to the index and closed by the recorder?
pub fn is_submitted(slice_dir: &Path) -> bool {
    slice_dir.join(SUBMIT_MARKER_DIR).is_dir()
}

/// The first 19 characters of any recording name are its start timestamp in
/// `%Y-%m-%d_%H-%M-%S`. Upstream depends on this in three separate places
/// (`utils.get_video_timestamp_by_filename_and_time`, the `LIKE '%name[:19]%'` filters and the
/// filename regex scan in `oneday.find_closest_video_by_filesys`), so a name that does not start
/// with a stamp is unreadable to the rest of the system.
pub const STAMP_LEN: usize = 19;

pub fn month_filename(user: &str, year: i64, month: u32) -> String {
    format!("{user}_{year:04}-{month:02}{}", DB_SUFFIX)
}

/// A month file's coordinates, parsed back out of its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonthDb {
    pub user: String,
    pub year: i64,
    pub month: u32,
    pub path: PathBuf,
}

/// `{user}_{YYYY}-{MM}_wind.db` -> `Some((user, year, month))`.
///
/// The user name may itself contain underscores and digits, so the tail is anchored and the split
/// point is found by position from the end rather than by searching for the first `_`.
pub fn parse_month_db(path: &Path) -> Option<MonthDb> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(DB_SUFFIX)?;
    let (user, date) = stem.rsplit_once('_')?;
    if user.is_empty() || date.len() != 7 || date.as_bytes()[4] != b'-' {
        return None;
    }
    let (y, m) = date.split_once('-')?;
    let year: i64 = y.parse().ok()?;
    let month: u32 = m.parse().ok()?;
    if !(1970..2200).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    Some(MonthDb { user: user.to_string(), year, month, path: path.to_path_buf() })
}

/// The read-only working copy for a live index file.
pub fn temp_read_of(db: &Path) -> PathBuf {
    let name = db.file_name().and_then(|s| s.to_str()).unwrap_or("wind.db");
    db.with_file_name(format!("{name}{TEMP_READ_SUFFIX}.db"))
}

/// The `%Y-%m-%d_%H-%M-%S` prefix that identifies a segment.
///
/// Upstream compares segments by this prefix rather than by full filename
/// (`utils.get_video_timestamp_by_filename_and_time`, `segment_rowids`' `LIKE '%name[:19]%'`),
/// because the same recording changes name as it moves through the pipeline:
/// `x.mp4` → `x-INDEX.mp4` → `x-OCRED.mp4` → `x-COMPRESS-OCRED.mp4`.
pub fn stamp_prefix(name: &str) -> String {
    name.chars().take(STAMP_LEN).collect()
}

/// The stamp a name starts with, when it genuinely starts with one.
///
/// [`crate::clock::LocalParts::from_stamp`] is what makes this more than a substring: `notes.txt` and a
/// row whose name was truncated both yield a short prefix, and treating either as a segment would let
/// an unrelated file keep an index row flagged as present.
pub fn segment_stamp_of(name: &str) -> Option<String> {
    let prefix = stamp_prefix(name);
    (prefix.chars().count() == STAMP_LEN && crate::clock::LocalParts::from_stamp(&prefix).is_some())
        .then_some(prefix)
}

/// Do two names describe the same recording?
///
/// Short names (a hand-edited row, a truncated path) must not match each other just because they are
/// both too short to carry a stamp, so both sides have to parse as a stamp first.
pub fn same_segment(a: &str, b: &str) -> bool {
    match (segment_stamp_of(a), segment_stamp_of(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Every screenshot slice directory on disk, keyed by its segment stamp.
///
/// Markers included, because a converted slice keeps its frames until the cache sweep deletes the
/// folder: "find the pictures this row was made of" and "remove this video and everything it was made
/// of" both have to see `{stamp}-VIDEO` as well as `{stamp}`. Paths come out of a directory listing and
/// never out of a name from the index, which is what keeps a corrupt or hand-edited row from pointing a
/// reader — or a deletion — somewhere the user did not record.
///
/// An absent cache root is an empty map rather than an error: an install that has never finished a
/// segment has nothing to look up, and every caller here reads `None` as "try the next door".
pub fn slice_dirs(cache_root: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(cache_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(stamp) = segment_stamp_of(name) {
            out.insert(stamp, path);
        }
    }
    out
}

/// A segment's two frame directories, as they stand on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameDirs {
    /// `cache/i_frames/{stamp}` — where the re-index pass writes the crop that a row's
    /// `picturefile_name` names, and which is swept as soon as the segment has been indexed.
    pub crop: Option<PathBuf>,
    /// `cache_screenshot/{stamp}[-MARKER]` — where the recorder wrote the frame itself, and which
    /// survives until the retention sweep takes the segment.
    pub slice: Option<PathBuf>,
}

impl FrameDirs {
    /// Nothing on disk holds this segment's pictures, so no name can be resolved against it.
    pub fn is_empty(&self) -> bool {
        self.crop.is_none() && self.slice.is_none()
    }
}

/// Resolve one row's frame name against directories that have already been listed — see
/// [`frame_dirs`], which is this with the listings taken.
///
/// The name is treated as untrusted, because it comes out of a file that a year of bugs (or an editor)
/// may have touched: it has to be a bare file name, with no separator, no `..` and no drive. An
/// absolute name is refused for the same reason even though upstream's Python wrote some — a path in the
/// index is not permission for a reader, or for the pass that deletes, to go wherever the index says.
pub fn frame_dirs_in(
    crops: &BTreeMap<String, PathBuf>,
    slices: &BTreeMap<String, PathBuf>,
    segment: &str,
    name: &str,
) -> FrameDirs {
    let mut out = FrameDirs::default();
    // `.` and `..` carry no separator and no `..` of their own, and `join` takes them — so the empty
    // name, the two relative entries and anything absolute are refused by name, not by luck.
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(std::path::is_separator)
        || name.contains("..")
        || Path::new(name).is_absolute()
    {
        return out;
    }
    let Some(stamp) = segment_stamp_of(segment) else {
        return out;
    };
    out.crop = crops.get(&stamp).map(|dir| dir.join(name));
    out.slice = slices.get(&stamp).map(|dir| dir.join(name));
    out
}

/// Where a row's own frame picture can still be, given the two roots the pipeline writes into.
///
/// Both come from [`slice_dirs`], so the pipeline's rename of a slice directory is invisible here — and
/// both are `None` when nothing carries the segment's stamp, which is the ordinary answer for footage
/// the retention sweep has already taken.
pub fn frame_dirs(iframe_root: &Path, cache_root: &Path, segment: &str, name: &str) -> FrameDirs {
    frame_dirs_in(&slice_dirs(iframe_root), &slice_dirs(cache_root), segment, name)
}

/// The first of a row's candidate frames that is really a file, preferring the crop the index names.
///
/// The crop wins because it is the picture that row's own text was read out of. The slice's frame is the
/// same screen a moment earlier or later, which makes a good preview and a slightly different original.
pub fn frame_file(iframe_root: &Path, cache_root: &Path, segment: &str, name: &str) -> Option<PathBuf> {
    let dirs = frame_dirs(iframe_root, cache_root, segment, name);
    dirs.crop.filter(|path| path.is_file()).or_else(|| dirs.slice.filter(|path| path.is_file()))
}

/// Does this directory name already carry a pipeline marker?
pub fn has_segment_marker(dir_name: &str) -> bool {
    [MARKER_VIDEO, MARKER_SUBMIT, MARKER_DISCARD, MARKER_OCRED]
        .iter()
        .any(|m| dir_name.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_directory_is_found_whatever_marker_it_carries() {
        assert_eq!(segment_stamp_of("2026-09-21_12-00-00.mp4").as_deref(), Some("2026-09-21_12-00-00"));
        assert_eq!(
            segment_stamp_of("2026-09-21_12-00-00-VIDEO-SCREENSHOTS-OCRED").as_deref(),
            Some("2026-09-21_12-00-00")
        );
        assert_eq!(segment_stamp_of("2026-13-45_99-99-99.mp4"), None, "an impossible date is not a stamp");
        assert_eq!(segment_stamp_of("notes.txt"), None);
        assert!(same_segment("2026-09-21_21-16-12.mp4", "2026-09-21_21-16-12-OCRED.mp4"));
        assert!(!same_segment("short", "short"), "two names too short to carry a stamp do not match");
    }

    #[test]
    fn slice_dirs_lists_directories_that_are_slices_and_nothing_else() {
        let root = std::env::temp_dir().join(format!("windbase-slices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("2026-09-21_21-16-12-VIDEO")).unwrap();
        std::fs::create_dir_all(root.join("2026-09-22_09-00-00")).unwrap();
        std::fs::create_dir_all(root.join("notes")).unwrap();
        std::fs::write(root.join("2026-09-23_09-00-00.jpg"), b"x").unwrap();

        let found = slice_dirs(&root);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            found.get("2026-09-21_21-16-12").map(|p| p.file_name().unwrap().to_str().unwrap()),
            Some("2026-09-21_21-16-12-VIDEO")
        );
        assert!(slice_dirs(&root.join("absent")).is_empty(), "an absent root answers instead of failing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rows_frame_is_found_in_either_root_and_under_a_renamed_slice() {
        let root = std::env::temp_dir().join(format!("windbase-frames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cache = root.join("cache_screenshot");
        let iframes = root.join("i_frames");
        let slice = cache.join("2026-09-21_21-16-12-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("2026-09-21_21-16-20.jpg"), b"frame").unwrap();

        assert_eq!(
            frame_file(&iframes, &cache, "2026-09-21_21-16-12.mp4", "2026-09-21_21-16-20.jpg").as_deref(),
            Some(slice.join("2026-09-21_21-16-20.jpg").as_path()),
            "the .mp4 suffix must not become a directory name, and the marker must not hide the slice"
        );

        let crop = iframes.join("2026-09-21_21-16-12");
        std::fs::create_dir_all(&crop).unwrap();
        std::fs::write(crop.join("8_cropped.jpg"), b"crop").unwrap();
        std::fs::write(slice.join("8_cropped.jpg"), b"copy in the slice").unwrap();
        assert_eq!(
            frame_file(&iframes, &cache, "2026-09-21_21-16-12.mp4", "8_cropped.jpg").as_deref(),
            Some(crop.join("8_cropped.jpg").as_path()),
            "the crop the index actually names is the row's own picture"
        );

        assert_eq!(frame_file(&iframes, &cache, "2026-09-21_21-16-12.mp4", "gone.jpg"), None);
        assert_eq!(frame_file(&iframes, &cache, "not-a-stamp.mp4", "8_cropped.jpg"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stored_frame_name_cannot_point_outside_its_segment() {
        let root = std::env::temp_dir().join(format!("windbase-frames-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cache_screenshot/2026-09-21_21-16-12")).unwrap();
        std::fs::create_dir_all(root.join("i_frames/2026-09-21_21-16-12")).unwrap();
        let crops = slice_dirs(&root.join("i_frames"));
        let slices = slice_dirs(&root.join("cache_screenshot"));
        let sep = std::path::MAIN_SEPARATOR;
        let rejected = vec![
            String::new(),
            "../secrets.jpg".into(),
            format!("..{sep}secrets.jpg"),
            "a/b.jpg".into(),
            format!("a{sep}b.jpg"),
            "C:/absolute.jpg".into(),
            "..".into(),
            ".".into(),
        ];
        for name in rejected.iter().map(String::as_str) {
            let dirs = frame_dirs_in(&crops, &slices, "2026-09-21_21-16-12.mp4", name);
            assert!(dirs.is_empty(), "{name:?} was accepted as a frame in the segment's directory: {dirs:?}");
        }
        assert!(!frame_dirs_in(&crops, &slices, "2026-09-21_21-16-12.mp4", "0.jpg").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn month_filename_matches_the_upstream_convention() {
        assert_eq!(month_filename("default", 2026, 9), "default_2026-09_wind.db");
        assert_eq!(month_filename("default", 2026, 12), "default_2026-12_wind.db");
        assert_eq!(month_filename("amy", 2024, 1), "amy_2024-01_wind.db");
    }

    #[test]
    fn month_files_round_trip_through_their_names() {
        let p = Path::new("userdata/db/default_2026-09_wind.db");
        let m = parse_month_db(p).expect("parses");
        assert_eq!((m.user.as_str(), m.year, m.month), ("default", 2026, 9));

        // A user name holding underscores and digits must not shift the date split.
        let p = Path::new("db/2024_snow_white_2025-03_wind.db");
        let m = parse_month_db(p).expect("parses");
        assert_eq!((m.user.as_str(), m.year, m.month), ("2024_snow_white", 2025, 3));
    }

    #[test]
    fn anything_that_is_not_a_month_file_is_rejected() {
        for name in [
            "ocrSaved.db",
            "default_2026-09_wind.db_TEMP_READ.db",
            "default_2026-13_wind.db",
            "default_20_wind.db",
            "_2026-09_wind.db",
            "backup_2026-09_wind_BACKUP_2026-09-21.db",
        ] {
            assert_eq!(parse_month_db(Path::new(name)), None, "{name} should not parse");
        }
    }

    #[test]
    fn temp_read_copy_keeps_the_original_name_as_a_prefix() {
        let p = temp_read_of(Path::new("db/default_2026-09_wind.db"));
        assert_eq!(p.file_name().unwrap(), "default_2026-09_wind.db_TEMP_READ.db");
        // The copy must not itself look like a month file, or discovery would read it twice.
        assert!(parse_month_db(&p).is_none());
    }

    #[test]
    fn pipeline_markers_are_recognised_so_maintenance_skips_finished_work() {
        assert!(has_segment_marker("2026-09-21_21-16-12-VIDEO"));
        assert!(has_segment_marker("2026-09-21_21-16-12-SUBMIT"));
        assert!(has_segment_marker("2026-09-21_21-16-12-SCREENSHOTS-OCRED"));
        assert!(!has_segment_marker("2026-09-21_21-16-12"));
    }
}
