//! What the tray says about its own version — and the deliberate absence of anything else.
//!
//! This module used to answer two questions: what version am I, and has somebody published a newer
//! one. The second question outlived its own product. It fetched the file upstream used to declare
//! the *Python* release in and walked the two version strings against each other — coherent while
//! the tray was `main.py`, nonsense once the application it versioned was deleted (`3f37cbf`),
//! because a native binary's `0.1.0` and a Python package's `0.0.31` are two unrelated numbering
//! schemes, and no comparison between them means anything a user could act on.
//!
//! The verdict drove exactly one visible thing: the badge that re-labelled the "Update" row from a
//! version to an offer. The offer ran `install_update.bat`, which did `git pull` + `pip` + `poetry`
//! into the Python tree — steps with no subject left: no Python, no poetry, no checkout to pull,
//! and a user who unpacked a release zip has none of the three. There was nothing to restore, so
//! the remote check and the update offer are removed rather than re-pointed; the tray cannot
//! install anything, and no menu row or report line claims it can.
//!
//! What survives is the first question, which is the only one with a true answer available: since
//! `044f170` the version is the binary's own, read from its embedded resources, identical to what
//! `windsvc --version` prints. It has no dependence on any file on disk, and keeping it that way is
//! what the tests below pin down.

use std::path::Path;

use wind_base::version;

/// The version this binary carries, in the same words `windsvc --version` uses.
///
/// Upstream read it out of the Python package's declaration file, and for a while that was right:
/// the tray *was* `main.py`, and the number in that file was the number it was running. It is not
/// any more. Since 407ff8e every binary here carries its own version — `--version` answers from
/// `CARGO_PKG_VERSION` and the PE's `VS_VERSION_INFO` block carries the same one. Measured on a
/// standalone root before this change, the menu rendered `🚀 Version unknown`, because the file it
/// asked for is not in the payload; on this development tree it rendered the Python app's `0.0.31`,
/// which is a real number belonging to a different product than the one the user is holding.
///
/// The root argument survives only because its callers hold one anyway; nothing here reads the disk.
pub fn local_version(_root: &Path) -> String {
    native_version()
}

/// The version, from this process's own compiled-in resources and nothing else.
///
/// Deliberately the same string as `--version` minus the leading binary name, so the tray cannot
/// report a version the command line contradicts. `options::version_line` and the test below pin
/// that relationship.
pub fn native_version() -> String {
    format!("{} ({})", env!("CARGO_PKG_VERSION"), version::profile())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options;
    use std::io::Read;
    use std::path::PathBuf;

    /// The tray reports its own version, and says so in the same words as its own `--version`.
    #[test]
    fn the_tray_versions_itself_from_the_binary_rather_than_from_a_file_beside_it() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(PathBuf::from).unwrap();
        let shown = local_version(&root);
        assert!(shown.contains(env!("CARGO_PKG_VERSION")), "{shown} does not carry this crate's version");
        assert!(shown.ends_with(&format!("({})", version::profile())), "{shown}");
        assert!(!shown.contains("unknown"), "the old answer: {shown}");
        // The exact relationship to `--version`: same tail, so a support ticket quoting one cannot
        // contradict the other. Written as a suffix check rather than equality because the menu line
        // already says "Version" and does not need to say "windsvc" again.
        assert!(options::version_line().ends_with(&shown), "{} does not end with the tray's {shown}", options::version_line());
    }

    /// The mutation guard for the above: a version that comes from the binary cannot depend on what
    /// happens to be on disk beside it. Deleting every file the Python release process ever wrote
    /// must change nothing about what the tray says, and neither can adding them back.
    #[test]
    fn no_root_on_disk_changes_what_the_tray_says_it_is() {
        let with = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(PathBuf::from).unwrap();
        let dir = std::env::temp_dir().join(format!("windsvc-version-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        assert!(!dir.join("windrecorder").exists(), "this scratch root has no Python tree at all");
        assert_eq!(local_version(&dir), local_version(&with), "the tray's version cannot depend on the tree beside it");
        // A root that is not merely empty but gone: the answer is still compiled in.
        assert_eq!(local_version(&dir.join("nowhere")), native_version(), "even a root nobody created reports the same version");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The deletion, asserted so that a re-introduction has to delete a failing test first: the
    /// tray ships no updater and no remote version check, so no *line of code* in this crate may
    /// name the fetch it used to make, the script it used to run, or the verdict that drove the
    /// badge. Comments are stripped before the scan — prose is allowed to remember what the code
    /// must not do again, which is how `layout.rs` can carry one historical mention of the deleted
    /// script while this test still means it. Every forbidden string is assembled from halves
    /// because this file is itself under scan.
    #[test]
    fn nothing_in_the_tray_still_fetches_a_version_or_launches_an_updater() {
        let forbidden: Vec<String> = ["curl", "githubuser", "REMOTE", "update", "UPDATE", "install", "__", "avail"]
            .into_iter()
            .map(|head| match head {
                "curl" => format!("{head}.exe"),
                "githubuser" => format!("{head}content.com"),
                "REMOTE" => format!("{head}_VERSION_URL"),
                "update" => format!("{head}_script"),
                "UPDATE" => format!("{head}_SCRIPT"),
                "install" => format!("{head}_update.bat"),
                "__" => format!("{head}version{head}"),
                _ => format!("{head}able_version"),
            })
            .collect();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut scanned = 0;
        for entry in std::fs::read_dir(&src).expect("supervisor\\src must exist to be scanned") {
            let path = entry.expect("readable entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let mut text = String::new();
            std::fs::File::open(&path).expect("readable source").read_to_string(&mut text).expect("utf-8 source");
            scanned += 1;
            for line in text.lines() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for needle in &forbidden {
                    assert!(!line.contains(needle), "{} still codes for {needle:?}: {line}", path.display());
                }
            }
        }
        assert!(scanned >= 10, "the scan covered {scanned} source files — it must cover the whole crate");
    }
}
