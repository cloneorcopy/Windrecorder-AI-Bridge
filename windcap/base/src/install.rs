//! What makes a directory a Windrecorder install, and where its shipped settings live.
//!
//! # The problem this file exists to kill
//!
//! Every native binary has to answer "where am I installed?" before it can read a config, open an
//! index or seed a first run. For the whole life of the Python app the answer was free: the tree
//! carried a directory named `windrecorder/`, so "does a `windrecorder/` exist" identified an
//! install, and it was checked in eight places with eight hand-written variants of the same two
//! lines. Eight variants is how a rule drifts.
//!
//! The rule then became load-bearing in a way it never was before. The native payload wants to be a
//! product rather than an overlay dropped onto a Python checkout, and the payload carries no
//! `windrecorder/` directory. So the sentinel and the data it named had to be separated, and the
//! separation had to be done in exactly one place, because an install that resolves its root
//! differently in `windrec` than in `windsetup` is an install whose recorder writes into a
//! directory its indexer then reports as empty.
//!
//! # The rule
//!
//! The install root is the directory that carries the shipped factory settings. That is the
//! *payload-managed* `config_src/config_default.json`; the `userdata/` directory is accepted as a
//! second string so a root that has been created but not yet seeded is still a root.
//!
//! `config_default.json` is chosen over `userdata/` as the primary marker because it is the file
//! the payload owns and the recorder cannot run without: it is the only seed source for a first
//! run, and it is the layer every other key is reconciled against. A directory with a `userdata/`
//! and no settings is an install with a problem; a directory with the settings file is an install.
//!
//! # The three layouts, and which wins
//!
//! Thousands of running installs are *overlays*: the native binaries were unzipped on top of a
//! Python checkout, so they have `windrecorder/config_src/` and no top-level `config_src/` until
//! they upgrade. A new standalone install has the reverse. An install caught mid-upgrade has
//! **both**, and there the top-level one must win, because that is the copy the payload unpacks
//! its newer defaults over — reading the stale `windrecorder/` copy after an upgrade is precisely
//! the "defaults file is behind the binary" bug `setup::migrate` exists to reconcile.
//!
//! So the order is fixed and tested: `config_src/`, then `windrecorder/config_src/`, then nothing.
//! The full list of layouts and the tests pinning them are in this module's `tests`.

use std::path::{Component, Path, PathBuf};

/// The settings directory the native payload ships and manages.
pub const CONFIG_SRC: &str = "config_src";
/// Where the same files lived in every install built before the payload could stand alone. Kept
/// working forever, not until a convenient release: an overlay install that has not upgraded yet
/// has nothing else.
pub const LEGACY_CONFIG_SRC: &str = "windrecorder/config_src";
/// The file inside either candidate that both seeds a first run and answers "what are the factory
/// settings". This is the sentinel: a directory holding it is an install.
pub const DEFAULTS_BASENAME: &str = "config_default.json";
/// The second-string marker. A root may legitimately have this and not the settings file — a fresh
/// `init` creates the layout before it seeds, and pointing a binary at a user's data directory is
/// never a mistake worth refusing over.
pub const USERDATA_DIR: &str = "userdata";
/// The name of the config key that lets a user point the settings directory somewhere else.
pub const CONFIG_SRC_KEY: &str = "config_src_dir";

/// Is `value` one of the two spellings this project has shipped as the *default* for
/// [`CONFIG_SRC_KEY`]?
///
/// The distinction is the whole reason this function exists. `Config::save` writes a complete
/// merged snapshot, so every `userdata/config_user.json` on an install that has ever been used
/// carries whatever literal was in the defaults file when it was written — including thousands of
/// files saying `windrecorder\config_src`, which nobody chose. Treating that inherited value as a
/// hand-written override would mean an upgraded install keeps reading the stale settings layer its
/// own upgrade just replaced, which is exactly the case (c) bug this rule exists to prevent.
///
/// Anything else — `D:/shared/wr-config`, `settings`, a genuinely hand-edited path — is a choice
/// and is honoured. Both slash spellings of each default are recognised, because the value has
/// been written out of Python with backslashes and read back by tools that normalise them.
pub fn is_default_src_literal(value: &str) -> bool {
    let normalised = value.trim().replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    let normalised = normalised.strip_prefix("./").unwrap_or(&normalised);
    normalised == CONFIG_SRC || normalised == LEGACY_CONFIG_SRC
}

/// How many levels `resolve_root` walks up from the executable before giving up.
///
/// Six covers every real layout: an install that runs its binaries out of `bin/` needs one, and a
/// development run out of `windcap/target/debug` needs three. It is a cap on a fruitless search,
/// not a scope limit — nothing above six levels could be this program's install without the
/// walk having already found the directory that is.
pub const MAX_WALK_UP: usize = 6;

/// The settings directories, in the order the rule consults them.
pub fn candidates() -> [PathBuf; 2] {
    [PathBuf::from(CONFIG_SRC), PathBuf::from(LEGACY_CONFIG_SRC)]
}

/// Join `relative` onto `root` keeping only ordinary components.
///
/// A config value of `..\\..\\windows` or `C:\\somewhere` is not something these binaries should
/// follow while looking for a lookup table, so anything that is not a plain directory name is
/// dropped rather than sanitised.
pub fn confined_join(root: &Path, relative: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in Path::new(relative).components() {
        if let Component::Normal(part) = part {
            path.push(part);
        }
    }
    path
}

/// The settings directory this root actually carries, top-level first.
pub fn config_src_dir(root: &Path) -> Option<PathBuf> {
    candidates().into_iter().map(|rel| root.join(rel)).find(|dir| dir.join(DEFAULTS_BASENAME).is_file())
}

/// A named file inside whichever settings directory this root carries.
///
/// Falls back to the payload location when the root has neither, so callers report a plausible
/// path in their error instead of having to invent one.
pub fn config_src_file(root: &Path, name: &str) -> PathBuf {
    config_src_dir(root).unwrap_or_else(|| root.join(CONFIG_SRC)).join(name)
}

/// The factory-settings file this root actually carries, top-level first.
///
/// `None` is not "this is not an install" — see [`defaults_source`], which also reports the copy
/// compiled into the binary.
pub fn defaults_file(root: &Path) -> Option<PathBuf> {
    candidates().into_iter().map(|rel| root.join(rel).join(DEFAULTS_BASENAME)).find(|file| file.is_file())
}

/// Where the factory settings in effect for `root` came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultsSource {
    /// `config_src/config_default.json` — the payload-managed copy.
    Payload(PathBuf),
    /// `windrecorder/config_default.json` — an overlay install that has not moved its data up.
    Legacy(PathBuf),
    /// Nothing on disk; the copy compiled into this binary is being used. Every binary in the
    /// workspace carries it, so a missing file can no longer mean "cannot install".
    Embedded,
}

impl DefaultsSource {
    pub fn describe(&self) -> String {
        match self {
            DefaultsSource::Payload(p) | DefaultsSource::Legacy(p) => p.display().to_string(),
            DefaultsSource::Embedded => "the defaults compiled into this binary".to_string(),
        }
    }

    /// The settings directory this source implies, when it is an on-disk one.
    pub fn config_src(&self) -> Option<&Path> {
        match self {
            DefaultsSource::Payload(p) | DefaultsSource::Legacy(p) => p.parent(),
            DefaultsSource::Embedded => None,
        }
    }
}

/// The settings in effect for `root`: the on-disk file, top-level first, or the compiled-in copy.
///
/// This is the function `Config::load` and `windsetup`' seeding both go through, which is the
/// entire point of the module — the read side and the seed side of a missing defaults file have to
/// agree or an install seeds from one place and reads from another.
pub fn defaults_source(root: &Path) -> DefaultsSource {
    match defaults_file(root) {
        // `unwrap` is safe: `defaults_file` only returns paths built by `candidates()`, every one of
        // which has a parent.
        Some(path) if path.starts_with(root.join(CONFIG_SRC)) => DefaultsSource::Payload(path),
        Some(path) => DefaultsSource::Legacy(path),
        None => DefaultsSource::Embedded,
    }
}

/// The factory settings compiled into every binary.
///
/// `config_src/config_default.json` from the top of the repository — the same file the payload
/// ships, read at compile time. The on-disk copy always wins when it exists, so an upgrade that
/// ships newer defaults is never held back by an older binary; this is the floor that makes
/// "no settings file on disk" a warning rather than a dead install.
pub fn embedded_defaults() -> &'static str {
    // Three levels up from `base/src`: base, windcap, and the install root that carries `config_src`.
    include_str!("../../../config_src/config_default.json")
}

/// Does `path` name a directory that is, or is on the way to being, an install?
pub fn is_install_root(path: &Path) -> bool {
    defaults_file(path).is_some() || path.join(CONFIG_SRC).is_dir() || path.join(LEGACY_CONFIG_SRC).is_dir() || path.join(USERDATA_DIR).is_dir()
}

/// The install root, given an explicit `--root` and the directory to start walking up from.
///
/// An explicit root is returned untouched: the user said where, and a tool that second-guesses
/// `--root` cannot be used to diagnose the very layout it is doubting. Otherwise the start
/// directory and each ancestor up to [`MAX_WALK_UP`] levels up is asked [`is_install_root`], and
/// the first one that says yes wins — nearest-first, so an install nested inside another install
/// resolves to its own copy.
///
/// The walk-up is what lets one binary serve both a user's install and a development tree:
/// `windcap/target/debug/windrec.exe` finds the checkout root, `C:\Windrecorder\bin\windrec.exe`
/// finds `C:\Windrecorder`, and neither needs to be told.
///
/// With nothing to find, `start` comes back. Every caller has an explicit-root escape hatch, and a
/// root that does not exist fails loudly downstream rather than silently resolving somewhere else.
pub fn resolve_root(explicit: Option<PathBuf>, start: &Path) -> PathBuf {
    if let Some(root) = explicit {
        return root;
    }
    let mut probe = start.to_path_buf();
    for _ in 0..=MAX_WALK_UP {
        if is_install_root(&probe) {
            return probe;
        }
        if !probe.pop() {
            break;
        }
    }
    start.to_path_buf()
}

/// [`resolve_root`] starting from this process's executable, which is what eleven binaries want and
/// the reason none of them should be writing the loop themselves.
pub fn resolve_root_from_exe(explicit: Option<PathBuf>) -> PathBuf {
    let start = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    resolve_root(explicit, &start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-install-{}-{tag}-{}", std::process::id(), std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// (a) The legacy overlay install: the binaries were unzipped onto a Python checkout, so the
    /// only settings are under `windrecorder/`. This is what thousands of running installs look
    /// like, and the new rule must not leave them behind.
    #[test]
    fn a_legacy_overlay_install_resolves_to_windrecorder_config_src() {
        let dir = scratch("legacy");
        write(&dir.join(LEGACY_CONFIG_SRC).join(DEFAULTS_BASENAME), r#"{"user_name": "legacy"}"#);
        write(&dir.join("windrecorder/__init__.py"), "__version__ = '0'\n");
        std::fs::create_dir_all(dir.join("userdata")).unwrap();

        assert!(is_install_root(&dir));
        assert_eq!(config_src_dir(&dir), Some(dir.join(LEGACY_CONFIG_SRC)));
        assert_eq!(defaults_source(&dir), DefaultsSource::Legacy(dir.join(LEGACY_CONFIG_SRC).join(DEFAULTS_BASENAME)));
        assert_eq!(config_src_file(&dir, "similar_CN_characters.txt"), dir.join(LEGACY_CONFIG_SRC).join("similar_CN_characters.txt"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// (b) The new standalone install: exactly what `release.ps1` unpacks, and nothing else.
    #[test]
    fn a_standalone_install_resolves_to_the_top_level_config_src() {
        let dir = scratch("standalone");
        write(&dir.join(CONFIG_SRC).join(DEFAULTS_BASENAME), r#"{"user_name": "standalone"}"#);
        std::fs::create_dir_all(dir.join("bin")).unwrap();

        assert!(is_install_root(&dir));
        assert_eq!(config_src_dir(&dir), Some(dir.join(CONFIG_SRC)));
        assert_eq!(defaults_source(&dir), DefaultsSource::Payload(dir.join(CONFIG_SRC).join(DEFAULTS_BASENAME)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// (c) The transition install — an upgrade unpacked `config_src/` over an install that still
    /// has its old `windrecorder/config_src/`. The top-level copy must win, because it is the one
    /// the payload just refreshed: reading the stale copy would hand the user the defaults their
    /// upgrade was written to replace, which is the most confusing field bug this rule could
    /// produce, so it is pinned here rather than left to "probably fine".
    #[test]
    fn when_both_settings_directories_exist_the_payload_one_wins() {
        let dir = scratch("transition");
        write(&dir.join(LEGACY_CONFIG_SRC).join(DEFAULTS_BASENAME), r#"{"user_name": "stale"}"#);
        write(&dir.join(CONFIG_SRC).join(DEFAULTS_BASENAME), r#"{"user_name": "current"}"#);
        std::fs::create_dir_all(dir.join("userdata")).unwrap();

        assert_eq!(config_src_dir(&dir), Some(dir.join(CONFIG_SRC)));
        assert_eq!(defaults_file(&dir), Some(dir.join(CONFIG_SRC).join(DEFAULTS_BASENAME)));
        assert_eq!(defaults_source(&dir), DefaultsSource::Payload(dir.join(CONFIG_SRC).join(DEFAULTS_BASENAME)));
        // And the config built on top of it agrees, rather than reading whichever it happened to
        // try first.
        let config = crate::config::Config::load(&dir).unwrap();
        assert_eq!(config.str_or("user_name", "?"), "current");
        assert_eq!(config.config_src_dir(), dir.join(CONFIG_SRC));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A root that exists but has not been seeded yet is still a root — `init` creates the layout
    /// before it writes any settings, and the walk-up has to agree with itself across that moment.
    #[test]
    fn a_root_with_only_userdata_is_an_install_root_without_being_a_settings_source() {
        let dir = scratch("userdata-only");
        std::fs::create_dir_all(dir.join("userdata")).unwrap();

        assert!(is_install_root(&dir));
        assert_eq!(config_src_dir(&dir), None, "nothing to read yet is not a settings directory");
        assert_eq!(defaults_source(&dir), DefaultsSource::Embedded);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The failure the whole change exists to remove: with no settings file anywhere the binaries
    /// used to have nothing to seed from and a first-run install could not be created at all.
    #[test]
    fn an_empty_directory_falls_back_to_the_compiled_in_defaults_rather_than_failing() {
        let dir = scratch("empty");
        assert!(!is_install_root(&dir), "an empty directory is not an install");
        assert_eq!(defaults_source(&dir), DefaultsSource::Embedded);
        let parsed: serde_json::Value = serde_json::from_str(embedded_defaults()).expect("the embedded defaults are valid JSON");
        assert!(parsed.get("user_name").is_some(), "the embedded copy must be the real settings file, not a stub");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The walk-up, which is what makes one binary serve an install and a development tree.
    #[test]
    fn resolution_walks_up_from_the_binary_to_the_nearest_install_root() {
        let dir = scratch("walkup");
        write(&dir.join(CONFIG_SRC).join(DEFAULTS_BASENAME), "{}");
        let exe_dir = dir.join("bin");
        std::fs::create_dir_all(exe_dir.join("nested")).unwrap();

        assert_eq!(resolve_root(None, &exe_dir), dir);
        assert_eq!(resolve_root(None, &exe_dir.join("nested")), dir, "the nearest root above wins, from any depth");
        assert_eq!(resolve_root(Some(PathBuf::from("elsewhere")), &exe_dir), PathBuf::from("elsewhere"), "--root is never second-guessed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A legacy install is found by the same code path as a standalone one — the point of routing
    /// all eight call sites through a single rule is that there is no second implementation left
    /// to forget to update.
    #[test]
    fn the_walk_up_finds_a_legacy_root_exactly_as_far_up_as_a_payload_root() {
        for rel in [CONFIG_SRC, LEGACY_CONFIG_SRC] {
            let dir = scratch("walkup-depth");
            write(&dir.join(&rel).join(DEFAULTS_BASENAME), "{}");
            let deep = dir.join("bin").join("x").join("y");
            std::fs::create_dir_all(&deep).unwrap();
            assert_eq!(resolve_root(None, &deep), dir, "{rel}");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_confined_join_refuses_to_escape_the_root() {
        let dir = scratch("confine");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(confined_join(&dir, r"..\\escape\\config_src"), dir.join("escape").join("config_src"));
        assert_eq!(confined_join(&dir, "a/./b"), dir.join("a").join("b"));
        assert_eq!(confined_join(&dir, ""), dir);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_candidates_are_ordered_payload_first() {
        assert_eq!(candidates(), [PathBuf::from("config_src"), PathBuf::from("windrecorder/config_src")]);
    }
}
