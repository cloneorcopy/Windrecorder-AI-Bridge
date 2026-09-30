//! The writable layout a Windrecorder install needs, and the refusal to create it in the wrong place.
//!
//! Python scatters this across `main.py:65-68`, `config.initialize_config`, `db_manager.__init__` and
//! `record.py`, which is why a fresh install half-creates its own directories depending on which entry
//! point was run first. Gathering them is not tidiness: `windsetup init` has to be able to say "this
//! install is complete" and `windsetup doctor` has to be able to say which piece is missing, and neither
//! is possible while creation is a side effect of whichever module was imported first.
//!
//! Every directory here is derived from the *config*, not hard-coded, because `userdata_dir`,
//! `db_path` and the rest are user-editable and a layout helper that ignores them would create
//! `userdata/db` next to the user's real `D:/records` index and then report an empty install.

use std::path::{Path, PathBuf};

use wind_base::config::Config;

use crate::pathguard;

/// One directory in the layout, with the reason it exists.
#[derive(Debug, Clone)]
pub struct Slot {
    pub name: &'static str,
    pub path: PathBuf,
    /// Why this is required rather than merely written by something someday.
    pub purpose: &'static str,
}

/// The complete writable layout for an install.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    pub slots: Vec<Slot>,
}

impl Layout {
    /// Every directory the app writes to, in creation order.
    ///
    /// Order matters only for the report: parents first so a nested failure reads as one problem rather
    /// than five.
    pub fn resolve(config: &Config) -> Layout {
        let root = config.root().to_path_buf();
        let userdata = config.userdata_dir();
        let mut slots: Vec<Slot> = Vec::new();

        let mut push = |name: &'static str, path: PathBuf, purpose: &'static str| {
            if !slots.iter().any(|s| s.path == path) {
                slots.push(Slot { name, path, purpose });
            }
        };

        push("userdata", userdata.clone(), "everything the user owns and nothing that can be regenerated");
        push("db", config.db_dir(), "the monthly index: the only part of the install that cannot be rebuilt from the videos");
        push("videos", config.videos_dir(), "the recorded segments, one folder per month");
        push("db_imgemb", userdata.join(config.str_or("vdb_img_path", "db_imgemb")), "the image-embedding vector index");

        // The result folders are generated output. They are created because the UI writes into them
        // without checking, and listing them here is what lets `doctor` distinguish "not generated yet"
        // from "the user moved this and the config never followed".
        for (key, default) in RESULT_DIRS {
            push("result", config.result_dir(key, default), "generated output the UI writes without checking");
        }

        push("cache", config.cache_dir(), "regenerable scratch: safe to empty, never safe to lose");
        push("cache/locks", config.lock_dir(), "pid-carrying lock files, one per running role");
        push("cache/logs", config.log_dir(), "the only diagnosis available after a crash");
        push("cache/win_title", config.win_title_dir(), "the daily window-title side channel");
        push("cache/i_frames", config.iframe_dir(), "frames extracted for seek and thumbnail work");
        push("cache/last_idle", config.last_idle_maintain_path().parent().unwrap_or(&root).to_path_buf(), "marker for the idle maintenance cadence");

        Layout { root, slots }
    }

    /// The install refuses to lay itself down on a filesystem root or an empty path: both are
    /// reachable by `--root ""` and `--root C:\`, and the result would be a `userdata/` in the wrong
    /// place plus a `cache/` at the drive root.
    ///
    /// A relative `--root .` is legitimate and common, so the test is "does this path name any
    /// directory of its own", not "is it absolute". `C:\` and `\` resolve to a drive or volume root and
    /// have no named component; `.` and `install` do.
    pub fn validate_root(root: &Path) -> Result<(), String> {
        if root.to_string_lossy().trim().is_empty() {
            return Err("--root is empty; refuse to lay an install out in an unnamed directory".to_string());
        }
        let names_a_directory = root.components().any(|c| {
            matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)
        });
        if !names_a_directory {
            return Err(format!(
                "--root {} is a filesystem root; refusing to create a layout directly on a drive",
                root.display()
            ));
        }
        Ok(())
    }

    /// Create what is missing. Returns the slots that did not exist before.
    ///
    /// `dry_run` is a genuine no-op: not one directory is created. That is why the plan is computed
    /// from `path.exists()` rather than from "did `create_dir_all` succeed", so a dry run reports the
    /// same list a real run would have acted on.
    pub fn create(&self, dry_run: bool) -> Result<Vec<String>, String> {
        let mut created = Vec::new();
        for slot in &self.slots {
            pathguard::confine(&self.root, &slot.path)?;
            if slot.path.is_dir() {
                continue;
            }
            created.push(format!("{}  ({})", slot.path.display(), slot.purpose));
            if dry_run {
                continue;
            }
            std::fs::create_dir_all(&slot.path)
                .map_err(|e| format!("{}: {e}", slot.path.display()))?;
            if !slot.path.is_dir() {
                return Err(format!("{} reported success and is still not a directory", slot.path.display()));
            }
        }
        Ok(created)
    }

    /// Which slots are absent, for `doctor`.
    pub fn missing(&self) -> Vec<&'static str> {
        self.slots.iter().filter(|s| !s.path.is_dir()).map(|s| s.name).collect()
    }
}

/// The generated-output folders, as `(config key, default)`.
///
/// Defaults taken from `config_default.json`, not invented: `result_date_state` is the one that is easy
/// to get wrong, because it is not in `upgrade_migration_routine.py`'s move list and looks redundant
/// beside `result_timeline`. It is a separate folder for a separate feature and must exist.
const RESULT_DIRS: [(&str, &str); 7] = [
    ("wordcloud_result_dir", "result_wordcloud"),
    ("timeline_result_dir", "result_timeline"),
    ("lightbox_result_dir", "result_lightbox"),
    ("wintitle_result_dir", "result_wintitle"),
    ("date_state_dir", "result_date_state"),
    ("ai_extract_tag_result_dir", "result_ai_extract_tag"),
    ("ai_day_poem_result_dir", "result_ai_day_poem"),
];

/// The keys whose values are directory names the migration may have to move.
pub fn result_dir_keys() -> [(&'static str, &'static str); RESULT_DIRS.len()] {
    RESULT_DIRS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wind-setup-layout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_layout_names_every_writable_directory_upstream_uses() {
        let root = tree("names");
        let config = Config::load(&root).unwrap();
        let layout = Layout::resolve(&config);
        for expected in [
            "userdata",
            "userdata/db",
            "userdata/videos",
            "userdata/result_wordcloud",
            "userdata/result_timeline",
            "userdata/result_lightbox",
            "userdata/result_wintitle",
            "userdata/result_date_state",
            "userdata/result_ai_extract_tag",
            "userdata/result_ai_day_poem",
            "cache",
            "cache/locks",
            "cache/logs",
            "cache/win_title",
            "cache/i_frames",
        ] {
            let relative = expected.replace('/', std::path::MAIN_SEPARATOR_STR);
            assert!(
                layout.slots.iter().any(|s| s.path == root.join(&relative)),
                "{expected} missing from {:?}",
                layout.slots.iter().map(|s| s.path.clone()).collect::<Vec<_>>()
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn creating_the_layout_makes_a_usable_tree() {
        let root = tree("create");
        let config = Config::load(&root).unwrap();
        let layout = Layout::resolve(&config);
        let created = layout.create(false).unwrap();
        assert!(!created.is_empty());
        assert!(root.join("userdata/db").is_dir());
        assert!(root.join("cache/locks").is_dir());
        // A second run creates nothing: `init` is expected to be re-runnable after a partial install.
        assert!(layout.create(false).unwrap().is_empty(), "the second pass must be a no-op");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_reports_the_same_list_and_touches_nothing() {
        let root = tree("dry");
        let config = Config::load(&root).unwrap();
        let layout = Layout::resolve(&config);
        let planned = layout.create(true).unwrap();
        assert!(!planned.is_empty());
        assert!(!root.join("cache").exists(), "a dry run did not create a directory");
        assert!(!root.join("userdata").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A relative `--root` is normal (`.`), so the check is for the *shape* of the path rather than
    /// for absoluteness — and it must catch the two cases that actually destroy data.
    #[test]
    fn a_root_that_is_a_drive_or_nothing_at_all_is_refused() {
        assert!(Layout::validate_root(Path::new("")).is_err());
        assert!(Layout::validate_root(Path::new("   ")).is_err());
        assert!(Layout::validate_root(Path::new("/")).is_err());
        assert!(Layout::validate_root(Path::new("C:/")).is_err());
        assert!(Layout::validate_root(Path::new(".")).is_ok());
        assert!(Layout::validate_root(Path::new("E:/install")).is_ok());
    }

    #[test]
    fn a_custom_userdata_dir_is_honoured_rather_than_hardcoded() {
        let root = tree("custom");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(
            root.join("config_src/config_default.json"),
            r#"{"userdata_dir": "D:/elsewhere"}"#,
        )
        .unwrap();
        let config = Config::load(&root).unwrap();
        let layout = Layout::resolve(&config);
        assert!(layout.slots.iter().any(|s| s.path == PathBuf::from("D:/elsewhere")));
        // The layout helper does not decide safety by itself: `create` confines every slot to the
        // install root first, so a config that points `userdata_dir` off-volume is refused rather than
        // quietly laid down somewhere the user never intended their data to live.
        assert!(layout.create(false).is_err(), "a userdata_dir outside the root must not be created");
        assert!(!PathBuf::from("D:/elsewhere").exists(), "and the refusal must happen before the write");
        let _ = std::fs::remove_dir_all(&root);
    }
}
