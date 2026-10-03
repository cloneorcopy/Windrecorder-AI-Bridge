//! The two config layers, and the reconciliation that upstream gets dangerously wrong.
//!
//! Windrecorder's settings are `config_src/config_default.json` overlaid by
//! `userdata/config_user.json`. `wind_base::config::Config` already implements the *read* side of that,
//! so nothing here re-declares a key. What this module owns is the write side, which is where the data
//! lives:
//!
//! `config.py:initialize_config()` creates `userdata/`, copies a legacy `config/config_user.json` in
//! ahead of the defaults, and only falls back to copying the defaults when there is still no user file.
//! `config.py:update_config_files_from_default_to_user()` then runs on **every single process start**
//! (it is called from `get_config_json`, which is evaluated at import time) and does two things: add
//! each default key the user file lacks, and *delete* every user key the default file lacks.
//!
//! The deletion is the dangerous one, and it is dangerous because it is quiet. `config_default.json` is
//! a file that ships with the app, so if an update lands a stale or partial copy of it — an interrupted
//! `install_update.bat`, a zip unpacked over a folder, a downgraded release — then on the next launch
//! every key the user has that the stale file does not mention is dropped from `config_user.json`. That
//! is not cosmetic: `user_name` disappearing renames every database the user owns out from under their
//! own search results, `open_ai_api_key` disappearing throws away a secret they pasted in and cannot
//! recover, and `ocr_engine` disappearing silently switches the indexer to an engine that may not be
//! installed, which is how "my index is empty and I do not know why" starts.
//!
//! So `reconcile` here does the additive half and refuses the subtractive half. Keys the defaults do not
//! know about are *kept in the user file* and reported. `doctor` and `--dry-run` both print what the
//! Python reconciler would have deleted, because "this is the thing we chose not to do" is information
//! an operator needs, and it is also the migration's own regression test.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use wind_base::config::Config;

use crate::backup;

/// `config_src/config_default.json`, relative to the install root — the spelling the payload ships.
///
/// Not the path in effect on any given install: an overlay install that has not moved its data up
/// still keeps the file under `windrecorder/`, and [`defaults_path`] is what decides which one a
/// real install is reading. This constant is the name that goes in a report.
pub const DEFAULTS_RELPATH: &str = "config_src/config_default.json";
/// `userdata/config_user.json`, relative to the install root.
pub const USER_RELPATH: &str = "userdata/config_user.json";
/// The pre-0.0.9 location, which the migration moves rather than deletes.
pub const LEGACY_USER_RELPATH: &str = "config/config_user.json";

/// The factory-settings file this install is actually reading.
///
/// `Config` has already resolved it — payload first, then the legacy overlay layout — and seeding
/// or reconciling against a *different* file than the one the recorder loaded would mean `init`
/// writes a user config derived from settings nothing else uses. So this asks the config rather
/// than re-deriving the path, and only falls back to naming the payload location when there is no
/// on-disk file at all, which is the case [`seed`] answers with compiled-in defaults.
pub fn defaults_path(config: &Config) -> PathBuf {
    config.defaults_path().map(Path::to_path_buf).unwrap_or_else(|| config.root().join(DEFAULTS_RELPATH))
}

/// Why a layer could not be read as settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Missing(PathBuf),
    Unreadable(PathBuf, String),
    /// Valid bytes, not a JSON object. A truncated file or someone's attempt at a comment.
    Malformed(PathBuf, String),
    NotAnObject(PathBuf),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Missing(p) => write!(f, "{} does not exist", p.display()),
            ReadError::Unreadable(p, e) => write!(f, "cannot read {}: {e}", p.display()),
            ReadError::Malformed(p, e) => write!(f, "{} is not valid JSON: {e}", p.display()),
            ReadError::NotAnObject(p) => write!(f, "{} is not a JSON object", p.display()),
        }
    }
}

/// Read one layer as an ordered-by-key object.
pub fn read_object(root: &Path, relative: &str) -> Result<Map<String, Value>, ReadError> {
    let path = root.join(relative);
    read_object_at(&path)
}

pub fn read_object_at(path: &Path) -> Result<Map<String, Value>, ReadError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::Missing(path.to_path_buf())),
        Err(e) => return Err(ReadError::Unreadable(path.to_path_buf(), e.to_string())),
    };
    let value: Value = serde_json::from_str(&text).map_err(|e| ReadError::Malformed(path.to_path_buf(), e.to_string()))?;
    value.as_object().cloned().ok_or(ReadError::NotAnObject(path.to_path_buf()))
}

/// Which layer is actually in effect, for `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layers {
    pub defaults: PathBuf,
    pub user: PathBuf,
    pub defaults_present: bool,
    pub user_present: bool,
    /// A config the app can read but that is not the file the user thinks they are editing.
    pub legacy_user_present: bool,
    pub effective: String,
}

pub fn layers(config: &Config) -> Layers {
    let root = config.root();
    let defaults = defaults_path(config);
    let user = root.join(USER_RELPATH);
    let legacy = root.join(LEGACY_USER_RELPATH);
    let (defaults_present, user_present, legacy_present) = (defaults.is_file(), user.is_file(), legacy.is_file());
    // The same three-way the Python loader performs: `Config::load` skips a missing layer, so an install
    // with no user file runs on pure defaults and any change the UI saves will create one.
    let effective = match (defaults_present, user_present, legacy_present) {
        (true, true, false) => "defaults + userdata/config_user.json".to_string(),
        (true, false, true) => "defaults only — a legacy config/config_user.json is being ignored; run `migrate`".to_string(),
        (true, false, false) => "defaults only — no user config yet; `init` will seed one".to_string(),
        (false, true, _) => "userdata/config_user.json ALONE — the shipped defaults file is missing, so no key added after your config was written exists".to_string(),
        // Both files present and a legacy copy still on disk: the live file wins, and the legacy one is
        // the thing `migrate`'s comparison step exists to adjudicate.
        (true, true, true) => "defaults + userdata/config_user.json, with an unresolved config/config_user.json still on disk; run `migrate`".to_string(),
        // Nothing on disk at all. Every binary carries the compiled-in copy, so this is degraded and
        // installable rather than the dead install it was before the payload could stand alone.
        (false, false, _) => "NOTHING on disk — running on the defaults compiled into the binary; `init` will seed a user config".to_string(),
    };
    Layers { defaults, user, defaults_present, user_present, legacy_user_present: legacy_present, effective }
}

/// What seeding did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seeded {
    /// The user's file was already there and was not opened for writing.
    AlreadyPresent,
    /// Created from the shipped defaults.
    FromDefaults { size: u64, sha256: String },
    /// Created by adopting a pre-0.0.9 `config/config_user.json`, which is what
    /// `initialize_config()` does and what protects an upgrading user's settings from being replaced by
    /// factory values.
    FromLegacyFile { size: u64, sha256: String },
    /// Created from the defaults **compiled into this binary**, because no `config_default.json` was on
    /// disk anywhere. Kept distinct from `FromDefaults` because it is a condition worth reading in the
    /// output: the payload's data did not arrive, and the install came up on the copy its own executable
    /// carries. That is degraded, not fatal — which is the whole point of embedding it.
    FromEmbeddedDefaults { size: u64, sha256: String },
}

/// What `userdata/config_user.json` is about to be built from.
enum SeedFrom {
    /// An existing file, adopted verbatim: the legacy user config, or the shipped defaults.
    File(PathBuf),
    /// The `config_default.json` compiled into this binary.
    Embedded(&'static str),
}

impl SeedFrom {
    fn label(&self) -> String {
        match self {
            SeedFrom::File(path) => path.display().to_string(),
            SeedFrom::Embedded(_) => "the defaults compiled into this binary".to_string(),
        }
    }
}

/// Pick the seed source: a legacy user config first, then the install's defaults file, then the
/// compiled-in copy.
///
/// The order is `config.py:initialize_config()`'s order, with one addition at the end. The last
/// resort is what makes `windsetup init` work on a directory that arrived with no
/// `config_default.json` — which, until the payload carried its own data, was not an edge case but
/// the only case a brand-new standalone install could be created in. It has the same shape as the
/// `ocr_lib` defect where a missing file downstream of the binary silently disables a feature: the
/// file was treated as guaranteed to exist, and it was not.
fn seed_from(config: &Config) -> SeedFrom {
    let root = config.root();
    let legacy = root.join(LEGACY_USER_RELPATH);
    if legacy.is_file() {
        return SeedFrom::File(legacy);
    }
    match config.defaults_path() {
        Some(path) => SeedFrom::File(path.to_path_buf()),
        None => SeedFrom::Embedded(wind_base::install::embedded_defaults()),
    }
}

/// Create `userdata/config_user.json` — but only when it does not exist.
///
/// The "only when" is the entire function. A first run has nothing to lose; a second run over an install
/// that has been used for two years would replace every setting the user made with factory values, and
/// `initialize_config` is reachable from an import, so "did we get here by accident" is not a question
/// that can be answered later. The pre-existing file is therefore never opened for writing on this path.
///
/// It cannot fail for want of a defaults file. Before the payload carried its own data there was exactly
/// one way to be missing here and it was fatal; now the compiled-in copy is always behind it.
pub fn seed(config: &Config, dry_run: bool) -> Result<Seeded, String> {
    let root = config.root();
    let user = root.join(USER_RELPATH);
    if user.exists() {
        return Ok(Seeded::AlreadyPresent);
    }
    let source = seed_from(config);
    let embedded = matches!(source, SeedFrom::Embedded(_));
    let from_legacy = root.join(LEGACY_USER_RELPATH).is_file();

    let (size, sha256) = match &source {
        SeedFrom::File(path) => {
            let size = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
            let sha256 = crate::hash::digest_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
            (size, sha256)
        }
        SeedFrom::Embedded(text) => (text.len() as u64, crate::hash::sha256_hex(text.as_bytes())),
    };
    if dry_run {
        return Ok(report(from_legacy, embedded, size, sha256));
    }

    if let Some(parent) = user.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    // Written through a staging file so a torn copy can never become the user's only config.
    let stage = user.with_extension("json.stage");
    let written = match &source {
        SeedFrom::File(path) => std::fs::copy(path, &stage).map(|_| ()).map_err(|e| format!("{}: {e}", stage.display())),
        SeedFrom::Embedded(text) => std::fs::write(&stage, text).map_err(|e| format!("{}: {e}", stage.display())),
    };
    written?;
    if std::fs::metadata(&stage).map(|m| m.len()).unwrap_or(0) != size
        || crate::hash::digest_file(&stage).ok().as_deref() != Some(sha256.as_str())
    {
        let _ = std::fs::remove_file(&stage);
        return Err(format!("the staged copy of {} did not verify; nothing was seeded", source.label()));
    }
    std::fs::rename(&stage, &user).map_err(|e| {
        let _ = std::fs::remove_file(&stage);
        format!("{}: {e}", user.display())
    })?;
    Ok(report(from_legacy, embedded, size, sha256))
}

fn report(from_legacy: bool, embedded: bool, size: u64, sha256: String) -> Seeded {
    if from_legacy {
        Seeded::FromLegacyFile { size, sha256 }
    } else if embedded {
        Seeded::FromEmbeddedDefaults { size, sha256 }
    } else {
        Seeded::FromDefaults { size, sha256 }
    }
}

/// The difference between the two key sets, in both directions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drift {
    /// In the defaults, absent from the user file: reconcile will add these.
    pub missing_from_user: Vec<String>,
    /// In the user file, absent from the defaults: the ones Python would delete and this crate keeps.
    pub extra_in_user: Vec<String>,
}

impl Drift {
    pub fn is_clean(&self) -> bool {
        self.missing_from_user.is_empty() && self.extra_in_user.is_empty()
    }
}

/// The defaults layer as an object, from whichever source is in effect.
///
/// `seed` treats a missing on-disk file as "use the compiled-in copy", and reconciliation has to
/// agree with it: a `migrate` that reconciled against nothing would report every key the user has
/// as one the defaults do not know about, which is the subtractive bug this module exists to
/// refuse. Parsing the embedded text cannot fail — it is compile-time checked to be valid JSON by
/// the test on it — so the only errors here come from a real file that is really unreadable.
fn defaults_object(config: &Config) -> Result<Map<String, Value>, String> {
    match config.defaults_path() {
        Some(path) => read_object_at(path).map_err(|e| e.to_string()),
        None => serde_json::from_str::<Value>(wind_base::install::embedded_defaults())
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| "the defaults compiled into this binary are not a JSON object".to_string()),
    }
}

pub fn drift(config: &Config) -> Result<Drift, String> {
    let root = config.root();
    let defaults = defaults_object(config)?;
    let user_path = root.join(USER_RELPATH);
    let user = match read_object_at(&user_path) {
        Ok(map) => map,
        // No user file yet is not drift: `seed` handles it, and reconcile has nothing to protect.
        Err(ReadError::Missing(_)) => return Ok(Drift { missing_from_user: defaults.keys().cloned().collect(), extra_in_user: Vec::new() }),
        Err(e) => return Err(e.to_string()),
    };
    let default_keys: BTreeSet<&String> = defaults.keys().collect();
    let user_keys: BTreeSet<&String> = user.keys().collect();
    Ok(Drift {
        missing_from_user: default_keys.difference(&user_keys).map(|k| (*k).clone()).collect(),
        extra_in_user: user_keys.difference(&default_keys).map(|k| (*k).clone()).collect(),
    })
}

/// What a reconcile run changed, and what it deliberately did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub added: Vec<String>,
    /// Keys kept because deleting them is not this program's call. Python's reconciler drops them.
    pub preserved: Vec<String>,
    /// Keys where the default and the user disagree. Not a change: the user's value wins by design, and
    /// the list is printed so a user can see which settings are not at their shipped value.
    pub overridden: Vec<String>,
    pub backup: Option<backup::Backup>,
    pub written: bool,
    /// The user file did not exist, so there was nothing to reconcile.
    pub skipped: bool,
}

/// Add every default key the user file lacks. Keep every key it has that the defaults lack.
///
/// The user file is written as an **overlay**, not as a merged snapshot: a key the user never set stays
/// unset, so a later release changing that key's default still reaches them. `config.save_config()` in
/// Python writes the full merged view instead, which is how a user who never touched `record_crf` ends
/// up with the 0.0.9 value pinned into their file forever.
pub fn reconcile(config: &Config, run_stamp: &str, dry_run: bool) -> Result<Reconciled, String> {
    let root = config.root();
    let user_path = root.join(USER_RELPATH);
    let defaults = defaults_object(config)?;
    let original = match read_object_at(&user_path) {
        Ok(map) => map,
        Err(ReadError::Missing(_)) => return Ok(Reconciled { skipped: true, ..Default::default() }),
        Err(e) => return Err(format!("refusing to reconcile an unreadable config: {e}")),
    };
    // A malformed user file must never be "repaired" by overwriting it: the bytes may be recoverable by
    // hand and they are certainly not recoverable once we replace them.
    let before: BTreeSet<String> = original.keys().cloned().collect();

    let mut added = Vec::new();
    let mut overridden = Vec::new();
    let mut merged = original.clone();
    for (key, value) in &defaults {
        match merged.get(key) {
            None => {
                merged.insert(key.clone(), value.clone());
                added.push(key.clone());
            }
            Some(existing) if existing != value => overridden.push(key.clone()),
            Some(_) => {}
        }
    }
    let preserved: Vec<String> = {
        let default_keys: BTreeSet<&String> = defaults.keys().collect();
        let mut keys: Vec<String> = original
            .keys()
            .filter(|k| !default_keys.contains(k))
            .cloned()
            .collect();
        keys.sort();
        keys
    };

    if added.is_empty() {
        // Nothing to add means nothing to write. Reporting the preserved keys is still worth doing, but
        // rewriting an identical file would churn its mtime and make `doctor`'s "unchanged" check lie.
        return Ok(Reconciled { added, preserved, overridden, backup: None, written: false, skipped: false });
    }

    // Every key the user had, before we touch anything. `protect_once` reuses a verified copy from an
    // earlier run, so a migration that crashed between the backup and the write does not produce a second
    // "pre-migration" copy whose digest no longer matches what was actually lost.
    let protected = backup::protect_once(config, &user_path, run_stamp, dry_run)
        .map_err(|e| e.to_string())?;
    if dry_run {
        return Ok(Reconciled { added, preserved, overridden, backup: protected, written: false, skipped: false });
    }
    let backup = protected.ok_or_else(|| "the user config disappeared mid-reconcile".to_string())?;

    let body = serde_json::to_string_pretty(&Value::Object(merged))
        .map_err(|e| format!("cannot serialize the reconciled config: {e}"))?;
    let stage = user_path.with_extension("json.stage");
    std::fs::write(&stage, &body).map_err(|e| format!("{}: {e}", stage.display()))?;
    std::fs::rename(&stage, &user_path).map_err(|e| {
        let _ = std::fs::remove_file(&stage);
        format!("{}: {e}", user_path.display())
    })?;

    // Proof, not hope: re-read the file we just wrote and assert that the user kept every key they had,
    // kept every value they had set, and gained exactly the keys we meant to add.
    let after = read_object_at(&user_path).map_err(|e| format!("the reconciled config does not read back: {e}"))?;
    let after_keys: BTreeSet<String> = after.keys().cloned().collect();
    let lost: Vec<&String> = before.iter().filter(|k| !after_keys.contains(k.as_str())).collect();
    if !lost.is_empty() {
        return Err(format!(
            "the reconciled config LOST keys {lost:?}; the verified backup is at {}",
            backup.target.display()
        ));
    }
    for (key, value) in &original {
        if after.get(key) != Some(value) {
            return Err(format!(
                "the reconciled config changed your value for \"{key}\"; the verified backup is at {}",
                backup.target.display()
            ));
        }
    }
    Ok(Reconciled { added, preserved, overridden, backup: Some(backup), written: true, skipped: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str, defaults: &str, user: Option<&str>) -> (PathBuf, Config) {
        let dir = std::env::temp_dir().join(format!("wind-setup-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(wind_base::install::CONFIG_SRC)).unwrap();
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        // Written at the payload spelling, which is also what `defaults_path` resolves to once the
        // file exists — the two have to agree or this fixture seeds one file and reads another.
        std::fs::write(dir.join(DEFAULTS_RELPATH), defaults).unwrap();
        if let Some(body) = user {
            std::fs::write(dir.join(USER_RELPATH), body).unwrap();
        }
        let config = Config::load(&dir).unwrap();
        (dir, config)
    }

    const DEFAULTS: &str = r#"{"lang": "en", "user_name": "default", "max_page_result": 20, "record_crf": 25}"#;

    #[test]
    fn seeding_creates_a_verifiable_copy_of_the_defaults() {
        let (root, config) = tree("seed", DEFAULTS, None);
        let outcome = seed(&config, false).unwrap();
        let user = root.join(USER_RELPATH);
        assert!(user.is_file());
        let Seeded::FromDefaults { sha256, size } = outcome else { panic!("expected a default seed, got {outcome:?}") };
        assert_eq!(sha256, crate::hash::digest_file(&user).unwrap());
        assert_eq!(size as usize, DEFAULTS.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The one that matters most. A user has spent two years setting this file up; a re-run of `init`
    /// must not be able to put it back to factory values.
    #[test]
    fn seeding_never_touches_an_existing_user_config() {
        let (root, config) = tree("clobber", DEFAULTS, Some(r#"{"lang": "sc", "user_name": "amy", "max_page_result": 999}"#));
        let user = root.join(USER_RELPATH);
        let before = std::fs::read(&user).unwrap();
        assert_eq!(seed(&config, false).unwrap(), Seeded::AlreadyPresent);
        assert_eq!(std::fs::read(&user).unwrap(), before, "the file was not rewritten at all");
        assert_eq!(Config::load(&root).unwrap().user_name(), "amy");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_older_install_keeps_its_own_settings_instead_of_inheriting_the_factory_ones() {
        let (root, config) = tree("legacy", DEFAULTS, None);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join(LEGACY_USER_RELPATH), r#"{"lang": "ja", "user_name": "snow_white"}"#).unwrap();
        let outcome = seed(&config, false).unwrap();
        assert!(matches!(outcome, Seeded::FromLegacyFile { .. }), "{outcome:?}");
        assert_eq!(Config::load(&root).unwrap().user_name(), "snow_white");
        // Adopted, not copied-then-deleted: the legacy file is still there for `migrate` to move.
        assert!(root.join(LEGACY_USER_RELPATH).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn drift_is_reported_in_both_directions() {
        let (root, config) = tree(
            "drift",
            DEFAULTS,
            Some(r#"{"lang": "sc", "user_name": "amy", "max_page_result": 20, "my_own_knob": true}"#),
        );
        let drift = drift(&config).unwrap();
        assert_eq!(drift.missing_from_user, vec!["record_crf"]);
        assert_eq!(drift.extra_in_user, vec!["my_own_knob"]);
        assert!(!drift.is_clean());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The brief's core scenario: the defaults file is *older* than the user's. Python would delete
    /// `ocr_engine` here, which is what turns the indexer off; this adds what is missing and keeps the
    /// rest, with a verified copy of the file it rewrote.
    #[test]
    fn a_stale_defaults_file_cannot_delete_a_setting_the_user_depends_on() {
        let stale_defaults = r#"{"lang": "en", "user_name": "default"}"#;
        let (root, config) = tree(
            "stale",
            stale_defaults,
            Some(r#"{"lang": "sc", "user_name": "amy", "ocr_engine": "PaddleOCR", "open_ai_api_key": "sk-secret-123"}"#),
        );
        let user = root.join(USER_RELPATH);
        let before = std::fs::read_to_string(&user).unwrap();

        let outcome = reconcile(&config, "2026-09-23_01-02-03", false).unwrap();
        assert_eq!(outcome.added, Vec::<String>::new(), "the stale file mentions nothing new");
        assert_eq!(outcome.preserved, vec!["ocr_engine", "open_ai_api_key"], "{:?}", outcome.preserved);
        let after = std::fs::read_to_string(&user).unwrap();
        // Nothing added means nothing written at all, so the file is untouched byte for byte.
        assert_eq!(after, before);

        // Now with one genuinely new default, so a write does happen.
        let (root, config) = tree("stale2", r#"{"lang": "en", "user_name": "default", "new_knob": 7}"#, Some(&before));
        let outcome = reconcile(&config, "run-2", false).unwrap();
        assert_eq!(outcome.added, vec!["new_knob"]);
        assert_eq!(outcome.preserved, vec!["ocr_engine", "open_ai_api_key"]);
        let after = std::fs::read_to_string(root.join(USER_RELPATH)).unwrap();
        assert!(after.contains("PaddleOCR"), "the user's engine survived:\n{after}");
        assert!(after.contains("sk-secret-123"), "the user's API key survived:\n{after}");
        assert!(after.contains("new_knob"));
        assert!(after.contains("\"lang\": \"sc\""), "and their language is still theirs");

        let backup = outcome.backup.expect("a write must be preceded by a backup");
        assert_eq!(std::fs::read_to_string(&backup.target).unwrap(), before, "the backup is the pre-reconcile bytes");
        assert_eq!(crate::hash::digest_file(&backup.target).unwrap(), backup.sha256);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reconcile_reports_which_values_are_not_at_their_shipped_default() {
        let (root, config) = tree("override", DEFAULTS, Some(r#"{"lang": "sc", "user_name": "default", "max_page_result": 20}"#));
        let outcome = reconcile(&config, "run", false).unwrap();
        assert_eq!(outcome.added, vec!["record_crf"]);
        assert_eq!(outcome.overridden, vec!["lang"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_reconciles_nothing_and_creates_no_backup() {
        let (root, config) = tree("dry", DEFAULTS, Some(r#"{"lang": "sc", "custom": 1}"#));
        let before = std::fs::read_to_string(root.join(USER_RELPATH)).unwrap();
        let outcome = reconcile(&config, "dry-stamp", true).unwrap();
        assert!(!outcome.written);
        assert_eq!(outcome.added, vec!["max_page_result", "record_crf", "user_name"]);
        assert_eq!(outcome.preserved, vec!["custom"]);
        assert_eq!(std::fs::read_to_string(root.join(USER_RELPATH)).unwrap(), before);
        assert!(!backup::backup_root(&config).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_malformed_user_config_is_reported_not_repaired() {
        // The config handle is built while the file is still parseable, and only then is the file broken:
        // `wind_base::Config::load` refuses a malformed layer outright, so an install whose config got
        // truncated cannot be opened by any command in the workspace. `reconcile` must still say what is
        // wrong rather than "repair" it by overwriting the bytes a human could recover.
        let (root, config) = tree("malformed", DEFAULTS, Some(r#"{"lang": "en"}"#));
        std::fs::write(root.join(USER_RELPATH), r#"{"lang": "sc", "#).unwrap();
        assert!(reconcile(&config, "s", false).is_err());
        assert!(drift(&config).is_err());
        // The broken bytes are still there for a human to fix.
        assert!(root.join(USER_RELPATH).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_layer_report_describes_a_legacy_install() {
        let (root, config) = tree("layers", DEFAULTS, None);
        assert!(layers(&config).effective.contains("no user config"));
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join(LEGACY_USER_RELPATH), "{}").unwrap();
        assert!(layers(&config).effective.contains("legacy"));
        std::fs::write(root.join(USER_RELPATH), "{}").unwrap();
        assert!(layers(&config).effective.contains("+ userdata"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// This test used to assert that seeding fails when no `config_default.json` exists. That
    /// refusal is the bug this change removes: it was reachable by simply unpacking the payload
    /// into an empty directory, which is the one thing a standalone product has to be able to do,
    /// and the message — "there is nothing to seed from" — described a dead install rather than a
    /// degraded one. So the assertions are inverted, deliberately, and kept rather than deleted
    /// because the *rest* of the old behaviour still has to hold.
    #[test]
    fn a_missing_defaults_layer_degrades_to_the_compiled_in_copy_and_still_installs() {
        let dir = std::env::temp_dir().join(format!("wind-setup-config-nodefaults-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        let config = Config::load(&dir).unwrap();
        // The report still names the situation — it is not silently pretending to have a file —
        // and it now also says what is being used instead.
        assert!(layers(&config).effective.contains("NOTHING on disk"), "{}", layers(&config).effective);
        assert!(layers(&config).effective.contains("compiled into the binary"));
        assert_eq!(config.defaults_source(), &wind_base::install::DefaultsSource::Embedded);
        // And `init` succeeds, seeding a real, complete, verifiable config.
        let outcome = seed(&config, false).unwrap();
        let user = dir.join(USER_RELPATH);
        assert!(user.is_file(), "a first run in an empty directory must produce a config");
        let Seeded::FromEmbeddedDefaults { size, sha256 } = outcome else { panic!("expected an embedded seed, got {outcome:?}") };
        assert_eq!(size, wind_base::install::embedded_defaults().len() as u64);
        assert_eq!(sha256, crate::hash::digest_file(&user).unwrap(), "the seeded bytes are the compiled-in bytes");
        assert_eq!(Config::load(&dir).unwrap().str_or("user_name", "?"), "default", "and it reads back as the shipped settings");
        // The on-disk file, once present, is authoritative: an upgrade that ships newer defaults
        // must not be overridden by whatever this binary was compiled against.
        std::fs::remove_file(&user).unwrap();
        std::fs::create_dir_all(dir.join(wind_base::install::CONFIG_SRC)).unwrap();
        std::fs::write(dir.join(DEFAULTS_RELPATH), r#"{"user_name": "from_disk"}"#).unwrap();
        let upgraded = Config::load(&dir).unwrap();
        assert_eq!(upgraded.str_or("user_name", "?"), "from_disk");
        assert!(matches!(upgraded.defaults_source(), wind_base::install::DefaultsSource::Payload(_)));
        assert_eq!(seed(&upgraded, false).unwrap(), seed_from_file_report(&upgraded));
        // A user file with no defaults behind it is still the lonely, dangerous case `layers` says so about.
        std::fs::remove_file(dir.join(DEFAULTS_RELPATH)).unwrap();
        std::fs::write(dir.join(USER_RELPATH), r#"{"lang": "en"}"#).unwrap();
        assert!(layers(&Config::load(&dir).unwrap()).effective.contains("ALONE"));
        assert_eq!(seed(&Config::load(&dir).unwrap(), false).unwrap(), Seeded::AlreadyPresent, "an existing user file is never replaced, even an orphaned one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `seed` result the on-disk layer implies, so the assertion above reads as a claim about
    /// precedence rather than as a re-statement of the enum.
    fn seed_from_file_report(config: &Config) -> Seeded {
        let path = defaults_path(config);
        Seeded::FromDefaults {
            size: std::fs::metadata(&path).unwrap().len(),
            sha256: crate::hash::digest_file(&path).unwrap(),
        }
    }
}
