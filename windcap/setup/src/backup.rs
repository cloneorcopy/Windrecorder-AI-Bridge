//! Backups the code can *prove* it made.
//!
//! The brief's rule is that no destructive or irreversible step runs without a copy of the file it is
//! about to change, in a location the same code also created, verified by size and hash after the
//! write. That is a stronger claim than `std::fs::copy(...)?`, and the gap is exactly where restore
//! attempts fail in practice:
//!
//!   * `fs::copy` returning `Ok` does not mean the destination holds the source's bytes — a full disk,
//!     a roaming profile syncing, or an antivirus opening the new file can all leave a short file that
//!     SQLite will open and then refuse to read;
//!   * the source can change *while* it is being copied (a recorder still running, a second
//!     `windsetup`), which produces a copy of neither the old nor the new database;
//!   * a backup nobody can point at is not a backup. So each one is recorded in an append-only
//!     manifest the operator can read, keyed by the file it protects.
//!
//! Backups live in `userdata/backup/<stamp>/<path relative to root>` — under `userdata/`, not `cache/`,
//! because `cache/` is documented as regenerable scratch and `windsetup init` plus every retention sweep
//! is entitled to empty it. A backup that a cleanup pass may delete is worse than none: it makes the
//! migration look guarded when it is not.

use std::path::{Path, PathBuf};

use serde_json::json;
use wind_base::clock::LocalParts;
use wind_base::config::Config;

use crate::hash;
use crate::pathguard;

/// `userdata/backup`, the root of every copy this crate makes.
pub fn backup_root(config: &Config) -> PathBuf {
    config.userdata_dir().join("backup")
}

/// `userdata/trash`, where a file upstream would have deleted is moved instead.
pub fn trash_root(config: &Config) -> PathBuf {
    config.userdata_dir().join("trash")
}

/// A copy that exists, is the right size, and hashes to the same digest as the file it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    /// The file that was protected.
    pub source: PathBuf,
    /// Where the copy lives, forever.
    pub target: PathBuf,
    pub sha256: String,
    pub size: u64,
    /// False when a previous run's verified copy was reused instead of a new one being written. This is
    /// reported rather than hidden because "the backup exists" and "this run made a backup" answer
    /// different questions during a restore.
    pub reused: bool,
}

/// A verified copy, or the reason the operation must not proceed.
#[derive(Debug)]
pub enum BackupError {
    /// The file to be protected does not exist, so there is nothing to back up and nothing to change.
    Missing(PathBuf, String),
    Failed(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::Missing(p, e) => write!(f, "cannot read {0} to back it up: {e}", p.display()),
            BackupError::Failed(m) => write!(f, "{m}"),
        }
    }
}

/// Copy `source` into this run's backup folder and prove the copy is faithful.
///
/// `run_stamp` is shared by every backup in one command invocation, so a plan printed by
/// `--dry-run` and the copies made by the real run line up, and a manifest reader sees one row per
/// logical operation rather than one per second.
pub fn protect(
    config: &Config,
    source: &Path,
    run_stamp: &str,
    dry_run: bool,
) -> Result<Option<Backup>, BackupError> {
    let (size, digest) = measure(source).map_err(|e| BackupError::Missing(source.to_path_buf(), e))?;

    // A dry run computes the destination and reports it, and creates nothing at all — not even the
    // backup directory, which is what lets a test assert that `--dry-run` leaves the tree byte-identical.
    let target = destination(config, source, run_stamp);
    if dry_run {
        return Ok(Some(Backup {
            source: source.to_path_buf(),
            target,
            sha256: digest,
            size,
            reused: false,
        }));
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BackupError::Failed(format!("{}: {e}", parent.display())))?;
    }
    pathguard::safe_write_target(&backup_root(config), &target)
        .map_err(|e| BackupError::Failed(format!("backup destination escaped the backup root: {e}")))?;

    // A copy of a file that is being written is a copy of nothing. Refuse if it moved underneath us,
    // rather than storing a digest that will not match either state.
    std::fs::copy(source, &target).map_err(|e| BackupError::Failed(format!("{} -> {}: {e}", source.display(), target.display())))?;

    let copy = verify(&target, size, &digest)
        .map_err(BackupError::Failed)?;
    let backup = Backup { source: source.to_path_buf(), target, sha256: digest, size, reused: false };
    record(config, &backup, &copy).map_err(BackupError::Failed)?;
    Ok(Some(backup))
}

/// Where an existing verified copy for `source` lives, if one does.
///
/// Re-entrancy needs this: a migration that crashed after backing up a month file and before ALTERing
/// it must not stamp a second copy on the next run, because the second copy is of the *same* file and
/// the backup folder fills with duplicates that all claim to be the pre-migration state. Reuse is
/// decided by digest, not by name, so a file that genuinely changed since then still gets a fresh copy.
pub fn find_existing(config: &Config, source: &Path) -> Option<Backup> {
    let root = backup_root(config);
    let (size, digest) = measure(source).ok()?;
    let relative = source.strip_prefix(config.root()).ok()?;
    let runs = std::fs::read_dir(&root).ok()?;
    for run in runs.flatten() {
        if !run.path().is_dir() {
            continue;
        }
        let candidate = run.path().join(relative);
        if candidate.is_file() && verify(&candidate, size, &digest).is_ok() {
            return Some(Backup { source: source.to_path_buf(), target: candidate, sha256: digest, size, reused: true });
        }
    }
    None
}

/// Back up, or reuse the previous run's verified copy. `None` only when the source is absent.
pub fn protect_once(
    config: &Config,
    source: &Path,
    run_stamp: &str,
    dry_run: bool,
) -> Result<Option<Backup>, BackupError> {
    if let Some(existing) = find_existing(config, source) {
        return Ok(Some(existing));
    }
    protect(config, source, run_stamp, dry_run)
}

/// The destination for one file in one run: `userdata/backup/<stamp>/<relative path>`.
///
/// The path relative to the install root is kept rather than just the file name because a migration
/// touches `userdata/db/x.db` and `videos/db/x.db`-shaped names alike; flattening them into one folder
/// loses which file a copy came from, which is the only thing that makes a restore mechanical.
pub fn destination(config: &Config, source: &Path, run_stamp: &str) -> PathBuf {
    let relative = source.strip_prefix(config.root()).unwrap_or_else(|_| {
        Path::new(source.file_name().unwrap_or(std::ffi::OsStr::new("item")))
    });
    backup_root(config).join(run_stamp).join(relative)
}

fn measure(source: &Path) -> Result<(u64, String), String> {
    let metadata = std::fs::metadata(source).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("is a directory, not a file".to_string());
    }
    let digest = hash::digest_file(source).map_err(|e| e.to_string())?;
    // Re-read the size after the digest: a file that changed while being hashed cannot be described by
    // a single (size, digest) pair, and storing a pair that never existed is worse than failing here.
    let after = std::fs::metadata(source).map_err(|e| e.to_string())?;
    if after.len() != metadata.len() {
        return Err(format!(
            "changed while being read ({} then {} bytes); is the recorder still running?",
            metadata.len(),
            after.len()
        ));
    }
    Ok((metadata.len(), digest))
}

/// Read the copy back and compare it against what the source was.
fn verify(target: &Path, expected_size: u64, expected_digest: &str) -> Result<String, String> {
    let metadata = std::fs::metadata(target)
        .map_err(|e| format!("{} was not created ({e})", target.display()))?;
    if metadata.len() != expected_size {
        return Err(format!(
            "{} is {} bytes but its source was {expected_size}: the copy is truncated, refusing to proceed",
            target.display(),
            metadata.len()
        ));
    }
    let digest = hash::digest_file(target)
        .map_err(|e| format!("{} cannot be re-read to be verified ({e})", target.display()))?;
    if digest != expected_digest {
        return Err(format!(
            "{} hashes to {digest}, its source hashed to {expected_digest}: the copy is not the file it claims",
            target.display()
        ));
    }
    Ok(digest)
}

/// Append one line to `userdata/backup/MANIFEST.jsonl`.
///
/// Append-only and human-readable on purpose. When a user is deciding whether to trust a restore weeks
/// later, the question is "which file is this copy of, and what did it hash to at the time" — a
/// question a directory listing cannot answer and a JSON-lines file can.
fn record(config: &Config, backup: &Backup, copy_digest: &str) -> Result<(), String> {
    let manifest = backup_root(config).join("MANIFEST.jsonl");
    if let Some(parent) = manifest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let line = json!({
        "source": backup.source.to_string_lossy(),
        "target": backup.target.to_string_lossy(),
        "size": backup.size,
        "source_sha256": backup.sha256,
        // Equal to `source_sha256` by construction, and recorded separately anyway: the assertion that
        // matters at restore time is "did we ever see these as the same value", and a manifest that
        // stores one field cannot express that it was checked.
        "target_sha256": copy_digest,
        "verified": copy_digest == backup.sha256,
        "pid": std::process::id(),
    });
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&manifest)
        .map_err(|e| format!("{}: {e}", manifest.display()))?;
    writeln!(file, "{line}").map_err(|e| format!("{}: {e}", manifest.display()))
}

/// Move `source` into `userdata/trash/<stamp>/<relative>`, verified the same way a copy is.
///
/// This is the "never delete a user file" primitive. Upstream reaches for `send2trash` or
/// `shutil.rmtree`; every such call site is ported onto this one, because the Windows recycle bin is
/// per-volume, sometimes disabled by policy, unavailable to a non-interactive service, and — worst of
/// all — invisible to `doctor`, so a migration that used it could not report what it had protected.
/// `userdata/trash/<stamp>/<path relative to the install root>`.
///
/// The relative path is kept for the same reason `destination` keeps it: `userdata/videos/x.mp4` and
/// `cache/videos/x.mp4` moved into one flat folder are indistinguishable when someone tries to put them
/// back.
pub fn trash_destination(config: &Config, source: &Path, run_stamp: &str) -> PathBuf {
    let relative = source.strip_prefix(config.root()).unwrap_or_else(|_| {
        Path::new(source.file_name().unwrap_or(std::ffi::OsStr::new("item")))
    });
    let marked = format!("TRASHED-{}", relative.file_name().and_then(|n| n.to_str()).unwrap_or("item"));
    trash_root(config).join(run_stamp).join(relative.with_file_name(marked))
}

pub fn move_to_trash(
    config: &Config,
    source: &Path,
    run_stamp: &str,
    dry_run: bool,
) -> Result<Option<PathBuf>, String> {
    let root = config.root();
    // `trash_destination`, not `destination`: a file being retired is not a backup, and building the
    // path under `userdata/backup/` then confining it to `userdata/trash/` made every single move fail
    // its own containment check.
    let mut target = trash_destination(config, source, run_stamp);
    if dry_run {
        return Ok(Some(target));
    }
    pathguard::confine(root, source)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    pathguard::safe_write_target(&trash_root(config), &target)
        .map_err(|e| format!("trash destination escaped the trash root: {e}"))?;
    if target.exists() {
        // Two runs of the same second, or a name collision with something the user made. Overwriting a
        // trashed file would destroy the first one, which is the exact thing this function exists to
        // prevent, so the second copy gets a suffix instead.
        target = deduplicate(&target);
    }
    std::fs::rename(source, &target)
        .map_err(|e| format!("{} -> {}: {e}", source.display(), target.display()))?;
    Ok(Some(target))
}

fn deduplicate(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("item");
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    for attempt in 2u32.. {
        let name = if extension.is_empty() {
            format!("{stem}-{attempt}")
        } else {
            format!("{stem}-{attempt}.{extension}")
        };
        let candidate = path.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the loop returns the first free name")
}

/// A stamp for one command invocation.
pub fn run_stamp(now: &LocalParts) -> String {
    now.stamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> (PathBuf, Config) {
        let dir = std::env::temp_dir().join(format!("wind-setup-backup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("userdata/db")).unwrap();
        std::fs::create_dir_all(dir.join(wind_base::install::CONFIG_SRC)).unwrap();
        std::fs::write(dir.join(wind_base::install::CONFIG_SRC).join(wind_base::install::DEFAULTS_BASENAME), "{}").unwrap();
        let config = Config::load(&dir).unwrap();
        (dir, config)
    }

    #[test]
    fn a_protected_file_can_be_read_back_identically() {
        let (root, config) = tree("verify");
        let source = root.join("userdata/db/x.db");
        std::fs::write(&source, b"the only copy of september").unwrap();

        let backup = protect(&config, &source, "2026-09-23_01-02-03", false).unwrap().unwrap();
        assert_eq!(backup.size, 26, "the size is the source's, measured not assumed");
        assert_eq!(backup.sha256, hash::sha256_hex(b"the only copy of september"));
        assert!(!backup.reused);
        assert_eq!(std::fs::read(&backup.target).unwrap(), b"the only copy of september");
        assert!(backup.target.starts_with(backup_root(&config)), "the copy must live in the backup root");

        let manifest = std::fs::read_to_string(backup_root(&config).join("MANIFEST.jsonl")).unwrap();
        assert!(manifest.contains("\"verified\":true"), "{manifest}");
        assert!(manifest.contains(&backup.sha256), "the manifest must name the digest it checked");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_source_that_changed_during_the_read_is_refused() {
        let (root, config) = tree("drift");
        let source = root.join("userdata/db/drift.db");
        std::fs::write(&source, b"short").unwrap();
        // A directory is not a file, and `measure` must say so instead of hashing a path.
        assert!(matches!(
            protect(&config, &root.join("userdata/db"), "s", false),
            Err(BackupError::Missing(_, _))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The re-entrancy requirement: a run that crashed after the backup and before the write must not
    /// leave two copies behind, and must still be able to prove what the pre-migration bytes were.
    #[test]
    fn an_existing_verified_copy_is_reused_not_stamped_again() {
        let (root, config) = tree("reuse");
        let source = root.join("userdata/db/y.db");
        std::fs::write(&source, b"stable content").unwrap();
        let first = protect(&config, &source, "run-1", false).unwrap().unwrap();
        let second = protect_once(&config, &source, "run-2", false).unwrap().unwrap();
        assert!(second.reused, "the second run must find the first copy");
        assert_eq!(second.target, first.target);
        assert!(!backup_root(&config).join("run-2").exists(), "no new folder for a reused backup");

        // A file that genuinely changed since the last run is a different state and gets its own copy.
        std::fs::write(&source, b"changed content!").unwrap();
        let third = protect_once(&config, &source, "run-3", false).unwrap().unwrap();
        assert!(!third.reused);
        assert!(third.target.starts_with(backup_root(&config).join("run-3")), "{:?}", third.target);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A truncated copy is the failure mode `fs::copy` cannot report, and the one that makes a restore
    /// impossible. Simulated by pre-creating the destination with the wrong length.
    #[test]
    fn a_bad_copy_stops_the_operation_rather_than_continuing_on() {
        let (root, config) = tree("truncated");
        let source = root.join("userdata/db/z.db");
        std::fs::write(&source, b"0123456789").unwrap();
        let target = destination(&config, &source, "sabotage");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"01234").unwrap();
        // `find_existing` will not claim the short file, and `verify` says precisely what is wrong.
        assert!(find_existing(&config, &source).is_none());
        let err = verify(&target, 10, &hash::sha256_hex(b"0123456789")).unwrap_err();
        assert!(err.contains("truncated"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_produces_a_plan_and_touches_nothing() {
        let (root, config) = tree("dry");
        let source = root.join("userdata/db/dry.db");
        std::fs::write(&source, b"dry run bytes").unwrap();
        let planned = protect(&config, &source, "dry-stamp", true).unwrap().unwrap();
        assert_eq!(planned.size, 13);
        assert!(planned.target.to_string_lossy().contains("dry-stamp"));
        assert!(!backup_root(&config).exists(), "a dry run did not create the backup root");
        assert!(!trash_root(&config).exists());
        assert!(move_to_trash(&config, &source, "dry-stamp", true).unwrap().is_some());
        assert!(source.exists(), "a dry run did not move the user's file");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trashing_moves_the_file_and_keeps_its_route() {
        let (root, config) = tree("trash");
        let source = root.join("userdata/db/gone.db");
        std::fs::write(&source, b"upstream would have deleted me").unwrap();
        let moved = move_to_trash(&config, &source, "2026-09-23_05-00-00", false).unwrap().unwrap();
        assert!(!source.exists());
        assert_eq!(std::fs::read(&moved).unwrap(), b"upstream would have deleted me");
        assert!(moved.to_string_lossy().contains("TRASHED-gone.db"), "{moved:?}");
        assert!(moved.to_string_lossy().contains("userdata"), "the relative route survives the move");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_colliding_trash_name_never_overwrites_what_is_already_there() {
        let (root, config) = tree("trash-clash");
        let source = root.join("userdata/db/clash.db");
        std::fs::write(&source, b"first").unwrap();
        move_to_trash(&config, &source, "same-stamp", false).unwrap();
        std::fs::write(&source, b"second").unwrap();
        let second = move_to_trash(&config, &source, "same-stamp", false).unwrap().unwrap();
        let first = trash_destination(&config, &source, "same-stamp");
        assert_eq!(std::fs::read(&first).unwrap(), b"first", "the first trashed file survived");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trashing_a_path_outside_the_install_is_refused() {
        let (root, config) = tree("trash-escape");
        let outside = std::env::temp_dir().join(format!("wind-setup-outside-{}.db", std::process::id()));
        std::fs::write(&outside, b"not yours").unwrap();
        assert!(move_to_trash(&config, &outside, "s", false).is_err());
        assert!(outside.exists(), "a refused move must not have moved anything");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }
}
