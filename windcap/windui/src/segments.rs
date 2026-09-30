//! Which segments are still on disk, and where.
//!
//! The index stores a segment's *name*, not a path, because the files move into a monthly folder
//! and later get renamed with a pipeline marker (`-VIDEO`, `-SUBMIT`, `-SCREENSHOTS-OCRED`).
//! Resolution is therefore a name lookup inside `userdata/videos/{YYYY-MM}/`, exactly as
//! `file_utils.check_video_exist_in_videos_dir` does it — including the substring match, which is
//! what lets a row written against `2026-09-21_12-00-00.mp4` find the same segment once it is
//! `2026-09-21_12-00-00-VIDEO-SCREENSHOTS-OCRED.mp4`.
//!
//! `wind_store::search::locate_segment` takes a flat name list plus one root and joins them, which
//! cannot produce a correct path for a monthly folder unless the caller smuggles the `YYYY-MM`
//! prefix into the names. This type reads the month out of the segment stamp instead, and caches one
//! directory listing per month so a page of 100 results costs a handful of `read_dir` calls rather
//! than a hundred — the mistake that made the old search screen crawl.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use wind_base::paths;
use std::sync::{Arc, Mutex};

use wind_base::LocalParts;

/// Shareable, refreshable cache of monthly video listings.
#[derive(Default, Clone)]
pub struct Index {
    months: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl Index {
    pub fn new() -> Index {
        Index::default()
    }

    /// Forget every listing. Called when the user asks for a refresh, because a segment that has
    /// just finished converting is precisely the case where a stale answer looks like data loss.
    pub fn clear(&self) {
        if let Ok(mut cache) = self.months.lock() {
            cache.clear();
        }
    }

    fn listing(&self, dir: &Path) -> Vec<String> {
        if let Ok(cache) = self.months.lock() {
            if let Some(found) = cache.get(&dir.to_string_lossy().to_string()) {
                return found.clone();
            }
        }
        let mut names: Vec<String> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect(),
            // A missing month folder is normal: it means nothing was recorded that month, and the
            // doctor-style rule for this whole codebase is that an absent directory is an answer,
            // not a failure.
            Err(_) => Vec::new(),
        };
        names.sort();
        if let Ok(mut cache) = self.months.lock() {
            cache.insert(dir.to_string_lossy().to_string(), names.clone());
        }
        names
    }

    /// The month folder a stored segment name belongs to, from the name's own stamp.
    fn month_dir(videos_dir: &Path, segment: &str) -> Option<(PathBuf, String)> {
        let stem = segment.rsplit(['/', '\\']).next().unwrap_or(segment);
        let stamp = LocalParts::from_stamp(stem)?;
        let name = format!("{:04}-{:02}", stamp.year, stamp.month);
        Some((videos_dir.join(&name), name))
    }

    /// Full path of the file a row's segment became, if it exists.
    pub fn resolve(&self, videos_dir: &Path, segment: &str) -> Option<PathBuf> {
        let (dir, _) = Self::month_dir(videos_dir, segment)?;
        let stem = segment.rsplit(['/', '\\']).next().unwrap_or(segment);
        let needle = stem.split('.').next().unwrap_or(stem);
        if needle.is_empty() {
            return None;
        }
        self.listing(&dir)
            .into_iter()
            .find(|name| name.contains(needle))
            .map(|name| dir.join(name))
            .filter(|path| path.is_file())
    }

    /// Does any segment for this product-day exist on disk at all?
    ///
    /// This is what separates the two "nothing here" messages OneDay has to tell apart: a day with
    /// recordings that were never indexed is a problem the user can fix, a day with no recordings
    /// is not.
    ///
    /// Both calendar dates are probed because a product-day with `day_begin_minutes = 180` runs to
    /// 02:59 the next morning, and a 00:30 segment is filed under tomorrow's month folder.
    pub fn has_any_segment_on_date(&self, videos_dir: &Path, day: LocalParts) -> bool {
        let tomorrow = crate::model::shift_date(day, 1);
        [day, tomorrow].iter().any(|probe| {
            let prefix = probe.date_stamp();
            let dir = videos_dir.join(format!("{:04}-{:02}", probe.year, probe.month));
            self.listing(&dir).into_iter().any(|name| name.starts_with(&prefix))
        })
    }
}

/// Which of a segment's two frame directories are on disk, and what a row's name means inside them.
///
/// `wind_base::paths::frame_file` is the rule; this is the listing that makes it affordable to ask once
/// per card. A search page resolves a hundred rows, and each rule-application would otherwise list both
/// roots — the same mistake that made the old search screen crawl.
#[derive(Default, Clone)]
pub struct Pictures {
    listed: Arc<Mutex<Option<(BTreeMap<String, PathBuf>, BTreeMap<String, PathBuf>)>>>,
}

impl Pictures {
    pub fn new() -> Pictures {
        Pictures::default()
    }

    /// Forget both listings. A slice that has just been converted — the moment its directory gains its
    /// `-VIDEO` marker — is exactly the case where a stale answer looks like lost footage.
    pub fn clear(&self) {
        if let Ok(mut cache) = self.listed.lock() {
            *cache = None;
        }
    }

    fn listings(&self, iframe_root: &Path, cache_root: &Path) -> (BTreeMap<String, PathBuf>, BTreeMap<String, PathBuf>) {
        if let Ok(cache) = self.listed.lock() {
            if let Some(found) = cache.as_ref() {
                return found.clone();
            }
        }
        let fresh = (paths::slice_dirs(iframe_root), paths::slice_dirs(cache_root));
        if let Ok(mut cache) = self.listed.lock() {
            *cache = Some(fresh.clone());
        }
        fresh
    }

    /// The file a row was indexed from, if either directory still holds it.
    ///
    /// The crop under `cache/i_frames` wins, because that is the picture the row's own text was read out
    /// of; the recorder's frame in the slice directory is the same screen a moment earlier or later.
    pub fn resolve(&self, iframe_root: &Path, cache_root: &Path, segment: &str, name: &str) -> Option<PathBuf> {
        let (crops, slices) = self.listings(iframe_root, cache_root);
        let dirs = paths::frame_dirs_in(&crops, &slices, segment, name);
        dirs.crop.filter(|path| path.is_file()).or_else(|| dirs.slice.filter(|path| path.is_file()))
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windui-segments-{tag}-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("2026-09")).unwrap();
        dir
    }

    fn stamp(text: &str) -> LocalParts {
        LocalParts::from_stamp(text).unwrap()
    }

    #[test]
    fn a_segment_is_found_under_its_month_folder_even_after_a_marker_rename() {
        let dir = root("resolve");
        std::fs::write(dir.join("2026-09").join("2026-09-21_12-00-00-VIDEO-SCREENSHOTS-OCRED.mp4"), b"x").unwrap();
        let index = Index::new();
        let found = index.resolve(&dir, "2026-09-21_12-00-00.mp4").expect("resolved");
        assert_eq!(found.file_name().unwrap(), "2026-09-21_12-00-00-VIDEO-SCREENSHOTS-OCRED.mp4");
        assert_eq!(found.parent().unwrap(), dir.join("2026-09"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_month_folder_is_not_an_error() {
        let dir = root("absent");
        let index = Index::new();
        assert!(index.resolve(&dir, "2026-09-21_12-00-00.mp4").is_none());
        assert!(!index.has_any_segment_on_date(&dir, stamp("2026-09-21_12-00-00")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_without_a_stamp_resolves_to_nothing_instead_of_guessing() {
        let dir = root("stamped");
        std::fs::write(dir.join("2026-09").join("loose.mp4"), b"x").unwrap();
        let index = Index::new();
        assert!(index.resolve(&dir, "not-a-timestamp").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_date_probe_reads_the_prefix_the_recorder_writes() {
        let dir = root("date");
        std::fs::write(dir.join("2026-09").join("2026-09-21_07-00-00-SUBMIT"), b"x").unwrap();
        std::fs::write(dir.join("2026-09").join("2026-09-22_07-00-00-SUBMIT"), b"x").unwrap();
        let index = Index::new();
        assert!(index.has_any_segment_on_date(&dir, stamp("2026-09-21_00-00-00")));
        assert!(!index.has_any_segment_on_date(&dir, stamp("2026-09-23_00-00-00")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two roots the pipeline writes, the marker rename in between, and the one name a row carries.
    #[test]
    fn a_cards_frame_is_resolved_in_either_root_and_the_crop_wins() {
        let dir = root("frames");
        let cache = dir.join("cache_screenshot");
        let iframes = dir.join("i_frames");
        let slice = cache.join("2026-09-21_12-00-00-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("2026-09-21_12-00-08.jpg"), b"frame").unwrap();

        let pictures = Pictures::new();
        assert_eq!(
            pictures.resolve(&iframes, &cache, "2026-09-21_12-00-00.mp4", "2026-09-21_12-00-08.jpg").as_deref(),
            Some(slice.join("2026-09-21_12-00-08.jpg").as_path())
        );
        assert_eq!(pictures.resolve(&iframes, &cache, "2026-09-21_12-00-00.mp4", "gone.jpg"), None);
        assert_eq!(pictures.resolve(&iframes, &cache, "2026-09-21_12-00-00.mp4", "../escape.jpg"), None);

        std::fs::create_dir_all(iframes.join("2026-09-21_12-00-00")).unwrap();
        std::fs::write(iframes.join("2026-09-21_12-00-00").join("8_cropped.jpg"), b"crop").unwrap();
        std::fs::write(slice.join("8_cropped.jpg"), b"slice copy").unwrap();
        pictures.clear();
        assert_eq!(
            pictures.resolve(&iframes, &cache, "2026-09-21_12-00-00.mp4", "8_cropped.jpg").as_deref(),
            Some(iframes.join("2026-09-21_12-00-00").join("8_cropped.jpg").as_path()),
            "the crop the index names is the row's own picture, and the listing had to be refreshed to              see the directory that now holds it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cleared_cache_sees_a_file_created_after_the_first_lookup() {
        let dir = root("cache");
        let index = Index::new();
        assert!(index.resolve(&dir, "2026-09-21_12-00-00.mp4").is_none());
        std::fs::write(dir.join("2026-09").join("2026-09-21_12-00-00.mp4"), b"x").unwrap();
        assert!(index.resolve(&dir, "2026-09-21_12-00-00.mp4").is_none(), "the listing is cached");
        index.clear();
        assert!(index.resolve(&dir, "2026-09-21_12-00-00.mp4").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
