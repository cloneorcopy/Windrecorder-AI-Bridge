//! Where everything the tray touches lives on disk, and the two strings it never invents.
//!
//! Every path here is derived from the same config keys `windrecorder/config.py` derives its own
//! from, so a user who moved `log_dir` gets a tray that writes where they told the app to write.
//!
//! The tray's icons are deliberately *not* among them. They used to be — `Layout::icon()` handed
//! `__assets__/icon-tray.png` to a GDI+ decode that treated failure as fatal, which meant a standalone
//! install with no `__assets__` could not start at all. They are compiled into the `.exe` now; see
//! `src\icon.rs`. What `__assets__` still holds, and still has to be on disk for, is the OCR fixtures
//! `windsetup check-engines` scores engines against.

use std::path::{Path, PathBuf};

use wind_base::config::Config;

/// `__assets__`, spelled the way `windrecorder/const.py` spells it.
pub const ASSET_DIR: &str = "__assets__";
/// The release notes the payload ships at its root — what "See what's new" opens on a standalone
/// install, because it is the one changelog that describes *this* product and it travels with the
/// zip. `release.ps1` generates it per release, hashes included.
pub const RELEASE_NOTES_NAME: &str = "RELEASE-NOTES.txt";
/// The checkout-side changelog. A development tree is an install root too, and this is the file it
/// carries; the standalone payload does not, and needs no substitute.
pub const CHANGELOG_NAME: &str = "CHANGELOG.md";
/// The caption of the "already running" dialog, which `main.py` hardcodes rather than translates.
pub const ALREADY_RUNNING_CAPTION: &str = "Windrecorder is already running.";
/// The pid file naming the live `windmcp` child. It lives beside the record and tray locks because
/// it means the same thing: a process is doing work, and its pid decides whether that is true now.
///
/// It is deliberately *not* a `windmaint`-style claim, and deliberately not cleaned up by anything
/// that only means to start over. `clear_maintain_markers` above is the one place in this module
/// that deletes, and it deletes only markers whose owner has no claim on the living; a lock naming
/// a live pid is left alone by every path here, because deleting it would hide the very thing the
/// tray needs to refuse to start a second bridge onto a port already taken.
pub const BRIDGE_LOCK_NAME: &str = "LOCK_FILE_MCP.MD";

/// The two files one supervised process writes: `out` is its stdout, `err` its stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPair {
    pub out: PathBuf,
    pub err: PathBuf,
}

/// Everything derived, in one place, so a message can name a path instead of assembling one.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    pub log_dir: PathBuf,
    /// The directory the OCR test fixtures live in. The tray reads nothing from it any more — its
    /// icons are compiled in — but `windsetup check-engines` still cannot score an engine without
    /// them, which is why the release payload now ships this one directory's fixtures.
    pub assets: PathBuf,
    pub languages: PathBuf,
    /// The release notes the zip carried in. First choice of [`Layout::changelog_target`].
    pub release_notes: PathBuf,
    /// The checkout-side changelog, second choice: a development root has no release notes.
    pub changelog: PathBuf,
    pub recording: LogPair,
    pub interface: LogPair,
    /// The MCP bridge's own pair. It is a network listener, so its output belongs nowhere the
    /// recorder's does: "why did the port not open" must not mean digging through a recording log.
    pub bridge: LogPair,
    pub tray_lock: PathBuf,
    pub record_lock: PathBuf,
    pub maintain_lock: PathBuf,
    /// Holds the pid of the supervised `windmcp serve`, so a separate process — `windsvc doctor` —
    /// can say whether the bridge is up without owning it.
    pub bridge_lock: PathBuf,
    pub flag_note: PathBuf,
}

impl Layout {
    pub fn from_config(config: &Config) -> Layout {
        let root = config.root().to_path_buf();
        let log_dir = config.log_dir();
        Layout {
            root: root.clone(),
            log_dir: log_dir.clone(),
            assets: root.join(ASSET_DIR),
            languages: wind_base::i18n::languages_path(&root),
            release_notes: root.join(RELEASE_NOTES_NAME),
            changelog: root.join(CHANGELOG_NAME),
            recording: LogPair { out: log_dir.join("recording.log"), err: log_dir.join("recording.err") },
            interface: LogPair { out: log_dir.join("webui.log"), err: log_dir.join("webui.err") },
            bridge: LogPair { out: log_dir.join("mcp.log"), err: log_dir.join("mcp.err") },
            tray_lock: config.tray_lock_path(),
            record_lock: config.record_lock_path(),
            maintain_lock: config.maintain_lock_dir(),
            bridge_lock: config.lock_dir().join(BRIDGE_LOCK_NAME),
            flag_note: config.flag_note_path(),
        }
    }

    /// The file "See what's new" opens, resolved against what this install actually carries.
    ///
    /// The payload's own `RELEASE-NOTES.txt` wins wherever it exists: it describes *this* product and
    /// it travelled with the binaries, which makes it the only changelog that can be true about the
    /// bytes the tray is supervising. A development checkout has no release notes and does have
    /// `CHANGELOG.md`, so that is the fallback. When neither is on disk the answer is `None`, and the
    /// menu drops the row rather than pointing it at a file that is not there — the mistake
    /// `install_update.bat` was left behind as.
    pub fn changelog_target(&self) -> Option<PathBuf> {
        [self.release_notes.as_path(), self.changelog.as_path()].into_iter().find(|path| path.is_file()).map(Path::to_path_buf)
    }

    /// The directories the tray creates before anything can be spawned: without `log_dir` the
    /// recorder's stdout redirect fails at `CreateProcess`, and without the lock directory the
    /// single-instance check cannot write its own file.
    pub fn ensure_directories(&self) -> Result<(), String> {
        for dir in [self.root.join("cache"), self.log_dir.clone(), self.tray_lock.parent().map(Path::to_path_buf).unwrap_or_else(|| self.root.clone())] {
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Ok(())
    }

    /// `cache/locks/LOCK_MAINTAIN` is a *directory* upstream uses as a container for one marker per
    /// video being indexed, and the tray empties it on the way in: a marker left by a killed run
    /// tells the maintenance pass its work is already claimed, forever. Clearing it is `main.py`'s
    /// `file_utils.empty_directory`, and it is the one thing here that deletes.
    ///
    /// Only the `.md` markers a Python run wrote are removed; a `PID` file belongs to the native
    /// `windmaint` protocol and clearing it would make a live pass look like a corpse.
    pub fn clear_maintain_markers(&self) -> Result<usize, String> {
        let dir = &self.maintain_lock;
        if !dir.is_dir() {
            return Ok(0);
        }
        let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let keep = matches!(path.file_name().and_then(|n| n.to_str()), Some("PID"));
            if keep {
                continue;
            }
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            removed += 1;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(Path::to_path_buf).unwrap()
    }

    fn shipped() -> Layout {
        Layout::from_config(&Config::load(&repo_root()).expect("the shipped config must parse"))
    }

    #[test]
    fn the_recording_logs_are_the_paths_main_py_advertises_in_its_own_error_text() {
        let layout = shipped();
        let root = repo_root();
        assert_eq!(layout.log_dir, root.join("cache\\logs"));
        assert_eq!(layout.recording.out, root.join("cache\\logs").join("recording.log"));
        assert_eq!(layout.recording.err, root.join("cache\\logs").join("recording.err"));
        assert_eq!(layout.interface.out, root.join("cache\\logs").join("webui.log"));
        assert_eq!(layout.interface.err, root.join("cache\\logs").join("webui.err"));
    }

    /// The bridge gets a pair of its own, in the same directory the user relocated with `log_dir`,
    /// and never shares a file with the recorder: the one question its log answers is "why did the
    /// port not open", and that answer must not be interleaved with a recording.
    #[test]
    fn the_bridge_logs_are_its_own_pair_and_follow_a_relocated_log_dir() {
        let layout = shipped();
        let root = repo_root();
        assert_eq!(layout.bridge.out, root.join("cache\\logs").join("mcp.log"));
        assert_eq!(layout.bridge.err, root.join("cache\\logs").join("mcp.err"));
        assert_ne!(layout.bridge.out, layout.recording.out);
        assert_ne!(layout.bridge.err, layout.interface.err);

        let dir = std::env::temp_dir().join(format!("windsvc-layout-bridge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"log_dir": "D:/elsewhere/logs", "lock_file_dir": "state/locks"}"#,
        )
        .unwrap();
        let moved = Layout::from_config(&Config::load(&dir).unwrap());
        assert_eq!(moved.bridge.out, PathBuf::from("D:/elsewhere/logs").join("mcp.log"));
        assert_eq!(moved.bridge_lock, dir.join("state").join("locks").join("LOCK_FILE_MCP.MD"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lock_paths_come_from_the_same_keys_the_recorder_uses() {
        let layout = shipped();
        let locks = repo_root().join("cache\\locks");
        assert_eq!(layout.tray_lock, locks.join("LOCK_FILE_TRAY.MD"));
        assert_eq!(layout.record_lock, locks.join("LOCK_FILE_RECORD.MD"));
        assert_eq!(layout.maintain_lock, locks.join("LOCK_MAINTAIN"));
        assert_eq!(layout.bridge_lock, locks.join(BRIDGE_LOCK_NAME));
    }

    #[test]
    fn a_relocated_log_directory_moves_the_logs_with_it() {
        let dir = std::env::temp_dir().join(format!("windsvc-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"log_dir": "D:/elsewhere/logs", "lock_file_dir": "state/locks"}"#,
        )
        .unwrap();
        let layout = Layout::from_config(&Config::load(&dir).unwrap());
        assert_eq!(layout.recording.out, PathBuf::from("D:/elsewhere/logs").join("recording.log"));
        assert_eq!(layout.tray_lock, dir.join("state").join("locks").join("LOCK_FILE_TRAY.MD"));
        // The assets directory moves with the root like everything else, and nothing else does: the
        // tray's own two icons are not on this list because they are not on disk.
        assert_eq!(layout.assets, dir.join("__assets__"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tray's icons are compiled in, so the two things that must be true are that the source of
    /// each one exists in the crate, and that it is still the same picture `__assets__` ships.
    ///
    /// The first is load-bearing for the release: a missing or misnamed file in `icons\` is silently
    /// skipped by `base\version_resource.rs` (that is what makes the directory optional for the other
    /// eleven binaries), and the tray would then start up on the system default and look fine to nobody.
    /// The second is the provenance: the `.ico`s are thin containers around the shipped PNG bytes, so
    /// re-encoding them would put a second copy of the art in the tree that could drift.
    #[test]
    fn the_icons_the_tray_asks_for_are_the_icons_the_crate_ships() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let icons = crate_dir.join("icons");
        let names: Vec<String> = std::fs::read_dir(&icons)
            .expect("supervisor\\icons must exist: it is the only thing that puts an icon in windsvc.exe")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".ico"))
            .collect();
        let mut expected = crate::icon::ICON_SOURCES.map(str::to_string).to_vec();
        expected.sort();
        let mut found = names.clone();
        found.sort();
        assert_eq!(found, expected, "supervisor\\icons holds {names:?}, not the two the tray loads");
        // The leading number in each file name is the resource id `icon.rs` asks the loader for, so a
        // rename that breaks the pairing has to break this too.
        for (id, source) in crate::icon::ICON_RESOURCES.iter().zip(crate::icon::ICON_SOURCES) {
            let prefix = source.split('-').next().unwrap();
            assert_eq!(
                prefix.parse::<u16>().unwrap_or_else(|e| panic!("{source} does not start with a resource id: {e}")),
                *id,
                "{source} is named for a different resource id than the tray loads"
            );
            assert!(icons.join(source).is_file(), "{source} is missing from supervisor\\icons");
        }
    }

    /// Each `.ico` is one directory entry wrapping a whole PNG, and that PNG must still be the
    /// artwork `__assets__` carries. Only checked where `__assets__` exists — the payload tree does
    /// not carry it, and a build there is not a build that lost the art.
    #[test]
    fn each_embedded_icon_is_the_shipped_tray_art_with_an_ico_header_on_the_front() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo = crate_dir.parent().and_then(Path::parent).unwrap();
        let pairs = [("1-tray-recording.ico", "icon-tray.png"), ("2-tray-paused.ico", "icon-tray-pause.png")];
        for (icon, png) in pairs {
            let source = repo.join("__assets__").join(png);
            if !source.is_file() {
                continue;
            }
            let wrapped = std::fs::read(crate_dir.join("icons").join(icon)).unwrap();
            let original = std::fs::read(&source).unwrap();
            assert_eq!(&wrapped[..2], &[0, 0], "{icon} is not an ICO");
            assert_eq!(&wrapped[2..4], &[1, 0], "{icon} is not an icon directory");
            assert_eq!(&wrapped[4..6], &[1, 0], "{icon} must carry exactly one image, like the PNG does");
            // The PNG payload is stored whole, so the file must end with the bytes it started with.
            assert_eq!(&wrapped[22..], &original[..], "{icon} is not {} wrapped in a directory — it was re-encoded or is stale", source.display());
            assert_eq!(&original[..8], b"\x89PNG\r\n\x1a\n", "{} is not a PNG", source.display());
        }
    }

    #[test]
    fn clearing_the_maintain_container_leaves_the_native_pid_behind() {
        let dir = std::env::temp_dir().join(format!("windsvc-maint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = {
            std::fs::create_dir_all(dir.join("config_src")).unwrap();
            std::fs::write(dir.join("config_src/config_default.json"), r#"{"lock_file_dir": "locks"}"#)
                .unwrap();
            Layout::from_config(&Config::load(&dir).unwrap())
        };
        std::fs::create_dir_all(&layout.maintain_lock).unwrap();
        std::fs::write(layout.maintain_lock.join("2026-09-01_VIDEO.MD"), b"x").unwrap();
        std::fs::write(layout.maintain_lock.join("PID"), b"1234").unwrap();
        assert_eq!(layout.clear_maintain_markers().unwrap(), 1);
        assert!(layout.maintain_lock.join("PID").exists(), "a native maint's own claim must survive");
        assert!(!layout.maintain_lock.join("2026-09-01_VIDEO.MD").exists());
        // An absent container is not an error: the tray runs before maintenance ever has.
        let _ = std::fs::remove_dir_all(&layout.maintain_lock);
        assert_eq!(layout.clear_maintain_markers().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(tag: &str) -> (PathBuf, Layout) {
        let dir = std::env::temp_dir().join(format!("windsvc-layout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        let layout = Layout::from_config(&Config::load(&dir).unwrap());
        (dir, layout)
    }

    /// The rule the Changelog row's honesty rests on: the file the menu opens is the file that is
    /// there. A standalone payload carries only the release notes; a checkout carries only
    /// `CHANGELOG.md`; the two together resolve to the notes, because they describe the shipped
    /// bytes. An install carrying neither resolves to `None` — and `menu::rows` then has no row to
    /// point at a missing file, which is the exact defect this closes.
    #[test]
    fn the_changelog_target_is_whichever_changelog_this_root_actually_carries() {
        let (dir, layout) = scratch("changelog");
        assert_eq!(layout.changelog_target(), None, "a bare install resolves the row to nothing, not to a path");
        std::fs::write(dir.join(CHANGELOG_NAME), b"# Changelog").unwrap();
        assert_eq!(layout.changelog_target().as_deref(), Some(dir.join(CHANGELOG_NAME).as_path()));
        std::fs::write(dir.join(RELEASE_NOTES_NAME), b"release notes").unwrap();
        assert_eq!(
            layout.changelog_target().as_deref(),
            Some(dir.join(RELEASE_NOTES_NAME).as_path()),
            "the release notes that shipped with these binaries outrank the checkout changelog"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both candidates hang off the same root as everything else in the layout, so an install moved
    /// anywhere resolves its changelog there and not under some baked-in path.
    #[test]
    fn the_changelog_paths_follow_the_install_root() {
        let (_dir, layout) = scratch("changelog-root");
        assert_eq!(layout.release_notes, layout.root.join(RELEASE_NOTES_NAME));
        assert_eq!(layout.changelog, layout.root.join(CHANGELOG_NAME));
    }
}
