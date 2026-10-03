//! The upgrade path over a user's years of monthly databases.
//!
//! Ported step for step from `windrecorder/upgrade_migration_routine.py`, which runs unconditionally at
//! the top of `onboard_setting.py` and keeps no record of having run. Three things are different here,
//! and all three exist because the alternative loses data:
//!
//!   * **nothing is deleted.** Every `send2trash`, `os.remove` and `shutil.rmtree` upstream becomes a
//!     verified move into `userdata/trash/`, and one upstream deletion is refused outright (§0.0.9).
//!   * **every write is preceded by a backup the code checked.** A month file is copied and its digest
//!     re-read before its table is altered; the config file likewise.
//!   * **the run is re-entrant.** Each step re-derives its own work from the current state, so a second
//!     run finds nothing to do, and a crash between two steps resumes at the step that did not finish.
//!     The marker records what happened; it is not what makes the run safe, because a marker that lies
//!     would have to be the only thing standing between the migration and the user's data.
//!
//! # Ordering constraints, which are load-bearing
//!
//! The steps below are not in a nice-to-have order. `config-db-path` must run before `index-schema`:
//! the pre-split installs stored `db_path` as a path that already included `userdata/`, so the join
//! `userdata_dir + db_path` points at `userdata/userdata/db` until the key is corrected, and a schema
//! pass that runs first would discover no month files at all, then — with a marker saying "schema done"
//! — never look again. `legacy-layout` must also run before `index-schema`, for the mirror-image
//! reason: ALTERing the files in the old root-level `db/` and then moving that directory means the
//! migration passed over the index and then relocated it, and a user whose move was interrupted has
//! half of each.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};
use wind_base::config::Config;
use wind_store::schema;

use crate::backup;
use crate::configfile;
use crate::hash;
use crate::marker::{self, Marker, StepRecord};
use crate::pathguard;

/// The stamp one invocation files its backups and trash under.
///
/// Resolved once and threaded through every step, so the plan a `--dry-run` prints and the folder names
/// the real run creates are the same strings and a restore can be read off either one.
pub fn stamp_now() -> String {
    wind_base::clock::now().stamp()
}

impl std::fmt::Display for Release {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c] = self.parts;
        write!(f, "{a}.{b}.{c}")
    }
}

/// Where a step first appeared, so `--from-version` can say "this install is already past it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub parts: [u32; 3],
}

impl Release {
    pub fn parse(text: &str) -> Option<Release> {
        // `0.0.31`, ` 0.1 `, and a bare major are all shapes upstream's own version string has appeared
        // in; anything that is not three dot-separated numbers is not a release and must be refused by
        // the caller rather than read as zero, which would silently skip every step.
        let pieces: Vec<&str> = text.trim().split('.').filter(|p| !p.is_empty()).collect();
        if pieces.is_empty() || pieces.len() > 3 {
            return None;
        }
        let mut parts = [0u32; 3];
        for (index, piece) in pieces.iter().enumerate() {
            parts[index] = piece.parse().ok()?;
        }
        // A piece that is not a number at all still has to fail rather than become zero.
        pieces.iter().all(|p| p.parse::<u32>().is_ok()).then(|| Release { parts })
    }

    fn at_or_before(&self, other: &Release) -> bool {
        self.parts <= other.parts
    }
}

/// One migration step: what it is, which release introduced it, and what it guarantees.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub id: &'static str,
    /// The release whose routine this step ports. `None` means "a standing invariant, always checked".
    pub since: Option<Release>,
    pub title: &'static str,
}

/// Every step, in the only order they are correct in.
pub const STEPS: [Step; 7] = [
    Step { id: "startup-shortcut", since: Some(Release { parts: [0, 0, 5] }), title: "retire the pre-0.0.5 boot shortcut" },
    Step { id: "config-db-path", since: Some(Release { parts: [0, 0, 9] }), title: "correct db_path and vdb_img_path for the userdata/ split" },
    Step { id: "legacy-layout", since: Some(Release { parts: [0, 0, 9] }), title: "move the pre-split data folders under userdata/" },
    Step { id: "legacy-config-file", since: Some(Release { parts: [0, 0, 9] }), title: "move config/config_user.json into userdata/" },
    Step { id: "error-video-tag", since: Some(Release { parts: [0, 0, 12] }), title: "give -ERROR videos their retry counter" },
    Step { id: "index-schema", since: Some(Release { parts: [0, 0, 20] }), title: "add win_title and deep_linking to every month file" },
    Step { id: "config-reconcile", since: None, title: "add keys the defaults gained, keeping every key the user has" },
];

/// What one step found, and what it did about it.
#[derive(Debug, Clone, Default)]
pub struct StepResult {
    /// Work performed, or — under `--dry-run` — work that would be performed. Empty means the step is
    /// already satisfied, which is what makes a second run a no-op.
    pub actions: Vec<String>,
    /// Something this step could not resolve safely. A step with a blocker is *not* recorded as done, so
    /// the next run offers it again; a blocker is never quietly promoted into success.
    pub blocked: Vec<String>,
    /// Information the operator needs that is not work: what was deliberately not done, and why.
    pub notes: Vec<String>,
    /// A digest of the state this step saw, stored in the marker so `doctor` can tell "migrated" from
    /// "migrated, and then something changed again".
    pub fingerprint: String,
}

impl StepResult {
    pub fn pending(&self) -> bool {
        !self.actions.is_empty()
    }
    pub fn clean(&self) -> bool {
        self.actions.is_empty() && self.blocked.is_empty()
    }
}

#[derive(Debug)]
pub struct Options<'a> {
    pub config: &'a Config,
    pub dry_run: bool,
    /// The release the install's data has already been migrated *through*. Every step introduced at or
    /// before it is treated as applied, which is what lets a user who upgraded from 0.0.11 to 0.0.31
    /// tell `migrate` not to re-offer the 0.0.12 rename without also telling it to skip 0.0.20's schema.
    pub from_version: Option<Release>,
    pub stamp: String,
}

#[derive(Debug, Default)]
pub struct Report {
    /// `(step, result)` for every step that was offered.
    pub steps: Vec<(Step, StepResult)>,
    pub marker: Option<PathBuf>,
}

impl Report {
    pub fn changed(&self) -> bool {
        self.steps.iter().any(|(_, r)| r.pending())
    }
    pub fn blockers(&self) -> Vec<(String, String)> {
        self.steps
            .iter()
            .flat_map(|(step, result)| result.blocked.iter().map(move |b| (step.id.to_string(), b.clone())))
            .collect()
    }
}

/// Run the migration.
///
/// The marker is saved after each step rather than once at the end. A crash inside a step is handled by
/// that step being idempotent; a crash *between* steps is handled by the record, and rewriting the
/// marker at the end would lose the whole audit trail to the second failure mode.
pub fn run(options: &Options) -> Result<Report, String> {
    let mut report = Report::default();
    let mut applied = false;

    for step in offered_steps(options) {
        let result = execute(step, options)?;
        let needs_record = result.pending() || !result.blocked.is_empty();
        if options.dry_run {
            report.steps.push((step, result));
            continue;
        }
        if needs_record && result.blocked.is_empty() {
            let mut current = Marker::load(options.config).unwrap_or_default();
            fill_identity(&mut current, options);
            current.steps.insert(
                step.id.to_string(),
                StepRecord { done_at: options.stamp.clone(), fingerprint: result.fingerprint.clone(), notes: result.notes.clone() },
            );
            current.plan_hash = plan_of(options);
            report.marker = Some(current.save(options.config)?);
            applied = true;
        }
        report.steps.push((step, result));
    }
    if !options.dry_run && applied {
        let mut current = Marker::load(options.config).unwrap_or_default();
        fill_identity(&mut current, options);
        current.plan_hash = plan_of(options);
        report.marker = Some(current.save(options.config)?);
    }
    Ok(report)
}

/// Stamp the marker with who wrote it and when, leaving the recorded steps alone.
fn fill_identity(current: &mut Marker, options: &Options) {
    current.migrated_to = marker::LATEST_KNOWN_RELEASE.to_string();
    current.writer = env!("CARGO_PKG_VERSION").to_string();
    current.updated_at = options.stamp.clone();
}

/// The steps this install still needs, honouring `--from-version`.
pub fn offered_steps(options: &Options) -> Vec<Step> {
    STEPS
        .iter()
        .copied()
        .filter(|step| match (step.since, options.from_version) {
            // Already reached by the installer's own claim: the step is skipped, not silently failed.
            (Some(since), Some(from)) => !since.at_or_before(&from),
            _ => true,
        })
        .collect()
}

/// The fingerprint of the whole plan, for the marker.
pub fn plan_of(options: &Options) -> String {
    let mut entries = Vec::new();
    for step in STEPS {
        entries.push((step.id.to_string(), step.since.map_or_else(|| "-".to_string(), |r| version_text(&r)), describe_state(options.config, step.id)));
    }
    marker::plan_hash(&entries)
}

fn version_text(release: &Release) -> String {
    let [a, b, c] = release.parts;
    format!("{a}.{b}.{c}")
}

/// A cheap, stable description of the state a step cares about, as a digest.
///
/// Public because `doctor` needs it: a marker entry whose fingerprint no longer matches the tree is the
/// difference between "this install was migrated" and "it was migrated and then something changed again",
/// and only the second one needs attention.
pub fn state_fingerprint(config: &Config, step_id: &str) -> String {
    describe_state(config, step_id)
}

/// A cheap, stable description of the state a step cares about.
///
/// Deliberately *names and counts*, not digests of databases: hashing a year of month files on every
/// `doctor` run would make the safe command the slow one, and users would stop running it. A month
/// file's identity here is its name and size, which is enough to notice that new months arrived.
fn describe_state(config: &Config, step_id: &str) -> String {
    let mut blob = String::new();
    match step_id {
        "startup-shortcut" => {
            blob.push_str(&startup_shortcut_path().map(|p| format!("probe:{}", p.display())).unwrap_or_else(|| "probe:no-appdata".into()));
            blob.push_str(&format!(":present:{}", startup_shortcut_path().map(|p| p.exists()).unwrap_or(false)));
        }
        "config-db-path" | "config-reconcile" => {
            let user = config.root().join(configfile::USER_RELPATH);
            blob.push_str(&format!("user:{}", digest_or_absent(&user)));
            blob.push_str(&format!("defaults:{}", digest_or_absent(&configfile::defaults_path(config))));
        }
        "legacy-layout" | "legacy-config-file" => {
            for name in legacy_candidates() {
                blob.push_str(&format!("{}:{}|", name, flag(&config.root().join(name))));
            }
            for name in result_dir_names(config) {
                blob.push_str(&format!("r{name}:{}|", flag(&config.userdata_dir().join(&name))));
            }
            blob.push_str(&format!("legacycfg:{}", flag(&config.root().join(configfile::LEGACY_USER_RELPATH))));
        }
        "error-video-tag" => {
            let mut hits = 0;
            let mut names = String::new();
            for path in walk_files(&config.videos_dir()) {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.contains("-ERROR.") {
                    hits += 1;
                    names.push_str(name);
                    names.push('\n');
                }
            }
            blob.push_str(&format!("hits:{hits}:{}", hash::sha256_hex(names.as_bytes())));
        }
        _ => {
            for month in wind_store::read::discover(&config.db_dir()) {
                let name = month.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
                blob.push_str(&format!("{name}:{}|", file_size(&month.path)));
            }
        }
    }
    hash::sha256_hex(blob.as_bytes())
}

fn digest_or_absent(path: &Path) -> String {
    hash::digest_file(path).unwrap_or_else(|_| "absent".to_string())
}

fn flag(path: &Path) -> char {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => 'd',
        Ok(meta) if meta.is_file() => 'f',
        Ok(_) => 'o',
        Err(_) => '-',
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn result_dir_names(config: &Config) -> Vec<String> {
    crate::layout::result_dir_keys()
        .into_iter()
        .map(|(key, default)| config.str_or(key, default))
        .collect()
}

/// The directories `shutil.move` is asked to relocate in 0.0.9.
///
/// Note what is *not* here: `result_date_state`, `result_ai_extract_tag` and `result_ai_day_poem`
/// postdate the split, so upstream correctly leaves them alone and this list stays three items shorter
/// than the config's full set of result directories.
pub fn legacy_candidates() -> [&'static str; 6] {
    ["videos", "db", "db_imgemb", "result_lightbox", "result_timeline", "result_wintitle"]
}

fn execute(step: Step, options: &Options) -> Result<StepResult, String> {
    let mut result = match step.id {
        "startup-shortcut" => startup_shortcut(options),
        "config-db-path" => config_db_path(options),
        "legacy-layout" => legacy_layout(options),
        "legacy-config-file" => legacy_config_file(options),
        "error-video-tag" => error_video_tag(options),
        "index-schema" => index_schema(options),
        "config-reconcile" => config_reconcile(options),
        other => return Err(format!("no implementation for step '{other}'")),
    }?;
    result.fingerprint = describe_state(options.config, step.id);
    Ok(result)
}

// ---------------------------------------------------------------------------
// 0.0.5 — the retired boot shortcut
// ---------------------------------------------------------------------------

/// The stale pre-0.0.5 launcher shortcut, where the profile puts it.
///
/// Public because `doctor` reports it: it lives outside the install root, so a user reading only the
/// install cannot see that their boot entry still points at a file that no longer exists.
pub fn startup_shortcut_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from)?;
    Some(
        appdata
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("start_record.bat.lnk"),
    )
}

/// Replace the shortcut that points at a launcher which no longer exists.
///
/// Upstream `os.remove`s the old `.lnk` and then calls `change_startup_shortcut(is_create=True)`. The
/// removal is ported as a move into `userdata/trash/`, because a shortcut in the Startup folder is a
/// user file outside the install root and this program does not delete those.
///
/// The *creation* half is deliberately not ported. A `.lnk` is a shell link object, which means COM and
/// `IShellLinkW`; reaching for it here would put a machine-wide autorun entry inside a data migration,
/// and an installer that starts the app on boot without being asked is a worse surprise than a stale
/// shortcut the user can see. `doctor` names the file and points at the Settings toggle that owns it.
fn startup_shortcut(options: &Options) -> Result<StepResult, String> {
    let mut result = StepResult::default();
    let Some(path) = startup_shortcut_path() else {
        result.notes.push("%APPDATA% is not set; the startup shortcut could not be located".to_string());
        return Ok(result);
    };
    if !path.exists() {
        return Ok(result);
    }
    result.actions.push(format!("move the stale boot shortcut {}", path.display()));
    result.notes.push(
        "the replacement start_app.bat.lnk is not created here: a shell link needs COM, and an autorun \
         entry does not belong in a data migration. Turn \"record on startup\" on in Settings instead."
            .to_string(),
    );
    if options.dry_run {
        return Ok(result);
    }
    // Not confined to the install root — it cannot be, it lives in the profile — so the destination is
    // checked instead: the copy must land inside `userdata/trash/`, and it is a *move*, so the original
    // is preserved rather than replaced.
    let config = options.config;
    let staged = backup::trash_root(config).join(&options.stamp);
    if let Err(e) = std::fs::create_dir_all(&staged) {
        result.blocked.push(format!("{}: {e}", staged.display()));
        return Ok(result);
    }
    let target = staged.join(format!("TRASHED-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("shortcut.lnk")));
    let target = if target.exists() { target.with_extension(format!("lnk.{}", std::process::id())) } else { target };
    match std::fs::rename(&path, &target) {
        Ok(()) => {
            if !target.exists() {
                result.blocked.push(format!("{} was moved and cannot be found at {}", path.display(), target.display()));
            }
        }
        Err(e) => {
            // Refusing and leaving the shortcut alone is a real outcome, not a failure of the run: the
            // data migration is not what keeps the app from booting.
            result.blocked.push(format!("{} could not be moved ({e}); it is left exactly where it was", path.display()));
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// 0.0.9 — the two path keys, corrected before anything reads them
// ---------------------------------------------------------------------------

/// Force `db_path` and `vdb_img_path` to the names the split expects.
///
/// Upstream does this with `set_and_save_config`, which writes the *whole merged* config back to disk —
/// i.e. it snapshots every default into the user file as a side effect of fixing one key. Here only the
/// two keys are written, so a default that changes in a later release still reaches the user.
fn config_db_path(options: &Options) -> Result<StepResult, String> {
    let config = options.config;
    let mut result = StepResult::default();
    let user_path = config.root().join(configfile::USER_RELPATH);
    let mut user = match configfile::read_object_at(&user_path) {
        Ok(map) => map,
        // Nothing to correct: an install with no user file is already reading `db` from the defaults.
        Err(configfile::ReadError::Missing(_)) => return Ok(result),
        Err(e) => {
            result.blocked.push(format!("{e}"));
            return Ok(result);
        }
    };

    let mut changes: Vec<(String, String, Value)> = Vec::new();
    for (key, canonical) in [("db_path", "db"), ("vdb_img_path", "db_imgemb")] {
        let current = user.get(key).cloned();
        let needs = match &current {
            Some(Value::String(text)) => text != canonical,
            Some(other) => *other != Value::String(canonical.to_string()),
            None => false,
        };
        if needs {
            changes.push((key.to_string(), describe(current), Value::String(canonical.to_string())));
        }
    }
    if changes.is_empty() {
        return Ok(result);
    }
    for (key, from, _) in &changes {
        let canonical = if *key == "db_path" { "db" } else { "db_imgemb" };
        result.actions.push(format!("set {key} to \"{canonical}\" (was {from})"));
    }
    if options.dry_run {
        return Ok(result);
    }

    // A config write is a destructive write: if this file is broken the user loses every setting, so it
    // is backed up and verified like a database is.
    let protected = match backup::protect_once(config, &user_path, &options.stamp, false) {
        Ok(Some(backup)) => backup,
        Ok(None) => {
            result.blocked.push("the user config vanished before it could be backed up".to_string());
            return Ok(result);
        }
        Err(e) => {
            result.blocked.push(format!("{e}"));
            return Ok(result);
        }
    };
    result.notes.push(format!("backed up {} to {}", user_path.display(), protected.target.display()));
    for (key, _, value) in changes {
        user.insert(key, value);
    }
    if let Err(e) = write_user_config(&user_path, &user) {
        result.blocked.push(format!("{e}; the verified backup is at {}", protected.target.display()));
    }
    Ok(result)
}

fn describe(value: Option<Value>) -> String {
    match value {
        None => "unset".to_string(),
        Some(Value::String(text)) => format!("\"{text}\""),
        Some(other) => other.to_string(),
    }
}

/// Write the user layer as an overlay, atomically, and refuse a file that does not read back.
fn write_user_config(path: &Path, object: &Map<String, Value>) -> Result<(), String> {
    let body = serde_json::to_string_pretty(&Value::Object(object.clone())).map_err(|e| e.to_string())?;
    let stage = path.with_extension("json.stage");
    std::fs::write(&stage, &body).map_err(|e| format!("{}: {e}", stage.display()))?;
    std::fs::rename(&stage, path).map_err(|e| {
        let _ = std::fs::remove_file(&stage);
        format!("{}: {e}", path.display())
    })?;
    let reread = configfile::read_object_at(path).map_err(|e| format!("the config written to {} does not read back: {e}", path.display()))?;
    if reread != *object {
        return Err(format!("{} does not match what was written to it", path.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 0.0.9 — the folder relocation
// ---------------------------------------------------------------------------

/// Move the pre-split folders under `userdata/`.
///
/// Upstream's `shutil.move(src, "userdata")` relies on `dst` being a *directory* and nests the source
/// inside it, which produces the right answer exactly once. If `userdata/videos` already exists — and it
/// will on any install that has recorded since the split — the same call produces `userdata/videos/videos`
/// and the recorder then writes to a folder nobody reads. That is the bug this step guards against: a
/// collision is not merged and not overwritten, it is moved aside whole and reported.
fn legacy_layout(options: &Options) -> Result<StepResult, String> {
    let config = options.config;
    let root = config.root();
    let mut result = StepResult::default();
    let userdata = config.userdata_dir();

    for name in legacy_candidates() {
        let source = root.join(name);
        let meta = match std::fs::symlink_metadata(&source) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                result.blocked.push(format!("{}: {e}", source.display()));
                continue;
            }
        };
        if pathguard::confine(root, &source).is_err() {
            result.blocked.push(format!("{} is not a plain child of the install root; refusing", source.display()));
            continue;
        }
        if meta.file_type().is_symlink() {
            result.blocked.push(format!("{} is a symlink; leaving it alone", source.display()));
            continue;
        }
        let target = userdata.join(name);
        if pathguard::confine(root, &target).is_err() {
            result.blocked.push(format!("{} would land outside the install root; refusing", source.display()));
            continue;
        }
        // Three cases, and the middle one is the trap. A target that does not exist is a plain move. A
        // target that exists and is *empty* is not a collision at all: `init` and `main.py` both create
        // `userdata/db` and `userdata/videos` on sight, so any install that ran the new layout helper
        // before this migration has an empty placeholder exactly where the user's real folder needs to
        // go — treating that as a collision would move years of index history to the trash and leave an
        // empty install. Only a target holding something is a real conflict.
        let collided = match std::fs::symlink_metadata(&target) {
            Err(_) => Collision::Free,
            Ok(meta) if !meta.is_dir() => Collision::Occupied,
            Ok(_) => match std::fs::read_dir(&target) {
                Ok(mut entries) => {
                    let occupied = entries.next().is_some();
                    if occupied {
                        Collision::Occupied
                    } else {
                        Collision::EmptyPlaceholder
                    }
                }
                // Unreadable is treated as occupied: "we could not look, so we assumed it was safe to
                // move the user's data on top of it" is not a sentence worth writing.
                Err(_) => Collision::Occupied,
            },
        };

        if matches!(collided, Collision::Occupied) {
            // Two folders with the same name and no way to know which file wins. The legacy one goes to
            // the trash *whole*, keeping its route, so a human can compare the two trees afterwards.
            result.actions.push(format!(
                "{} exists alongside the non-empty {}; moving the older one aside rather than merging",
                source.display(),
                target.display()
            ));
            result.notes.push(format!("compare {} with {} — nothing was merged", source.display(), target.display()));
            if options.dry_run {
                continue;
            }
            match move_aside(config, &source, &options.stamp) {
                Ok(moved) => result.notes.push(format!("moved to {moved:?}")),
                Err(e) => result.blocked.push(e),
            }
        } else {
            let clearing = matches!(collided, Collision::EmptyPlaceholder);
            result.actions.push(format!(
                "move {} -> {}{}",
                source.display(),
                target.display(),
                if clearing { " (replacing the empty placeholder there now)" } else { "" }
            ));
            if options.dry_run {
                continue;
            }
            if clearing {
                // `remove_dir`, never `remove_dir_all`: this branch is only reachable once the directory
                // has been *proven* empty, and a fallback that deletes recursively would not need that
                // proof to stay a lie.
                if let Err(e) = std::fs::remove_dir(&target) {
                    result.blocked.push(format!("{}: {e}", target.display()));
                    continue;
                }
            }
            if let Err(e) = move_directory(&source, &target) {
                result.blocked.push(e);
            }
        }
    }
    Ok(result)
}

/// Whether a move destination can receive the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Collision {
    Free,
    /// Present and holding something: the merge decision is the user's, not ours.
    Occupied,
    /// Present and empty: a placeholder the layout helper made, which the real folder replaces.
    EmptyPlaceholder,
}

/// `rename`, falling back to copy-then-trash when the two paths are on different volumes.
///
/// A cross-volume `MoveFileEx` fails with `os error 17`; upstream's `shutil.move` handles that by
/// copying, and a migration that refuses to relocate a user's `videos/` off `D:` would be worse than the
/// bug it is fixing. The source is only removed once the copy has verified.
fn move_directory(source: &Path, target: &Path) -> Result<(), String> {
    match std::fs::rename(source, target) {
        Ok(()) if target.is_dir() => Ok(()),
        Ok(()) => Err(format!("{} reported a move and is not a directory", target.display())),
        Err(rename_error) => {
            let staged = target.with_extension("moving");
            std::fs::create_dir_all(&staged)
                .map_err(|e| format!("{}: {e} (rename of {} failed: {rename_error})", staged.display(), source.display()))?;
            copy_tree(source, &staged).map_err(|e| {
                let _ = std::fs::remove_dir_all(&staged);
                e
            })?;
            std::fs::remove_dir_all(&staged).map_err(|e| format!("{}: {e}", staged.display()))?;
            std::fs::rename(&staged, target).map_err(|e| format!("{}: {e}", target.display()))
        }
    }
}

/// The whole-tree copy used by the cross-volume fallback, and the verification it needs.
///
/// This is the only place in this crate that reaches `remove_dir_all`, and it only ever removes the
/// staging directory it just created itself, after every one of its files has been compared by size.
/// The user's original is never passed to it.
fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(source).map_err(|e| format!("{}: {e}", source.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let destination = target.join(&name);
        // `symlink_metadata` on the path, not `DirEntry::metadata`: whether the latter traverses a
        // reparse point is platform detail, and "we assumed it did not and copied through a junction"
        // is the mistake this whole module exists to avoid.
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        // A symlink inside a user's data folder is copied as a symlink or refused, never followed:
        // following one during a "move" would leave the original pointing into a directory we then delete.
        if metadata.file_type().is_symlink() {
            return Err(format!("{} is a symlink inside a folder being copied across volumes; refusing", entry.path().display()));
        }
        if metadata.is_dir() {
            std::fs::create_dir_all(&destination).map_err(|e| format!("{}: {e}", destination.display()))?;
            copy_tree(&entry.path(), &destination)?;
        } else if metadata.is_file() {
            std::fs::copy(entry.path(), &destination).map_err(|e| format!("{}: {e}", destination.display()))?;
            let copied = std::fs::metadata(&destination).map_err(|e| format!("{}: {e}", destination.display()))?.len();
            if copied != metadata.len() {
                return Err(format!("{} copied as {copied} of {} bytes", destination.display(), metadata.len()));
            }
        }
    }
    Ok(())
}

/// Retire a file or directory into `userdata/trash/`, keeping the route it came from.
///
/// Built through `backup::trash_destination` so the folder-move path and the single-file path cannot
/// drift into two different layouts: a restore has to be able to find a trashed item by the path it
/// occupied, and two naming schemes for the same operation is how one of them ends up unwritten.
fn move_aside(config: &Config, source: &Path, stamp: &str) -> Result<PathBuf, String> {
    let staged = backup::trash_destination(config, source, stamp);
    let target = if staged.exists() { staged.with_extension(format!("moved.{}", std::process::id())) } else { staged };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    pathguard::safe_write_target(&backup::trash_root(config), &target)?;
    std::fs::rename(source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
    if !target.exists() {
        return Err(format!("{} was moved and cannot be found at {}", source.display(), target.display()));
    }
    Ok(target)
}

/// Move `config/config_user.json` into `userdata/`, or prove it has already happened.
///
/// Upstream `shutil.move`s it unconditionally — which is fine on a first run and destructive on a
/// re-run, because by then `userdata/config_user.json` exists and is the file the user has actually been
/// editing. So the two files are *compared* first: the legacy copy is only allowed to become the live
/// config when the live one carries nothing the legacy one does not.
fn legacy_config_file(options: &Options) -> Result<StepResult, String> {
    let config = options.config;
    let root = config.root();
    let mut result = StepResult::default();
    let legacy = root.join(configfile::LEGACY_USER_RELPATH);
    let meta = match std::fs::symlink_metadata(&legacy) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Also the signal that `migrate` has already done this job, which is what makes a second run
            // a no-op rather than a re-copy.
            return Ok(result);
        }
        Err(e) => {
            result.blocked.push(format!("{}: {e}", legacy.display()));
            return Ok(result);
        }
    };
    if !meta.is_file() {
        result.blocked.push(format!("{} is not a file; leaving it", legacy.display()));
        return Ok(result);
    }

    let user = root.join(configfile::USER_RELPATH);
    let live = configfile::read_object_at(&user).ok();
    let older = configfile::read_object_at(&legacy);
    let legacy_object = match older {
        Ok(object) => object,
        Err(e) => {
            result.blocked.push(format!("{e} — the legacy config is unparseable and is being kept"));
            return Ok(result);
        }
    };

    match live {
        None => {
            result.actions.push(format!("move {} -> {}", legacy.display(), user.display()));
            if options.dry_run {
                return Ok(result);
            }
            if let Some(parent) = user.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            // Verified copy first, then the move: the destination is the user's only settings file.
            let digest = hash::digest_file(&legacy).map_err(|e| format!("{}: {e}", legacy.display()))?;
            std::fs::copy(&legacy, &user).map_err(|e| format!("{}: {e}", user.display()))?;
            let same = hash::digest_file(&user).ok().as_deref() == Some(digest.as_str());
            if !same {
                let _ = std::fs::remove_file(&user);
                result.blocked.push(format!("the copy of {} did not verify; the original is untouched", legacy.display()));
                return Ok(result);
            }
            result.notes.push(format!("{} hashes to the bytes now in {}", legacy.display(), user.display()));
            match move_aside(config, &legacy, &options.stamp) {
                Ok(moved) => result.notes.push(format!("the legacy file was retired to {moved:?}")),
                Err(e) => result.blocked.push(e),
            }
        }
        Some(live_object) => {
            let missing: Vec<String> = legacy_object
                .keys()
                .filter(|k| !live_object.contains_key(*k))
                .cloned()
                .collect();
            let differs: Vec<String> = legacy_object
                .keys()
                .filter(|k| live_object.get(*k).is_some_and(|v| Some(&legacy_object[*k]) != Some(v)))
                .cloned()
                .collect();
            if missing.is_empty() && differs.is_empty() {
                result.actions.push(format!("retire {}, whose every key is already in {}", legacy.display(), user.display()));
                if options.dry_run {
                    return Ok(result);
                }
                match move_aside(config, &legacy, &options.stamp) {
                    Ok(moved) => result.notes.push(format!("moved to {moved:?}")),
                    Err(e) => result.blocked.push(e),
                }
            } else {
                // The live file is *not* a superset. Merging would mean choosing which value wins, and
                // the only thing we know about the choice is that the user did not make it.
                result.blocked.push(format!(
                    "{} holds keys the live config does not ({}) — both files are left in place; resolve by hand",
                    legacy.display(),
                    if missing.is_empty() { String::new() } else { missing.join(", ") }
                ));
                result.notes.push(format!("values that differ: {}", if differs.is_empty() { "none".to_string() } else { differs.join(", ") }));
            }
        }
    }

    // The `config/` directory itself: upstream `shutil.rmtree`s it, taking anything else the user had
    // put there with it.
    let legacy_dir = root.join("config");
    if legacy_dir.is_dir() {
        let leftovers: Vec<String> = std::fs::read_dir(&legacy_dir)
            .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        if leftovers.is_empty() {
            if !options.dry_run {
                let _ = std::fs::remove_dir(&legacy_dir);
            }
            result.actions.push(format!("remove the now-empty {}", legacy_dir.display()));
        } else {
            result.notes.push(format!("{} still holds {leftovers:?}; it is left in place", legacy_dir.display()));
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// 0.0.12 — the retry-count tag on failed videos
// ---------------------------------------------------------------------------

/// Rename `-ERROR.<ext>` to `-ERROR1.<ext>`.
///
/// The tag is how the indexer counts retries against `ERROR_VIDEO_RETRY_TIMES`, so a file that keeps the
/// untagged name is retried forever. Re-entrancy is structural: `-ERROR1.` does not contain the
/// substring `-ERROR.`, so a second pass finds nothing.
///
/// One difference from upstream, and it is a correctness fix rather than a stylistic one: Python applies
/// `str.replace` to the **full path**, so a folder named `2026-01-01-ERROR.backup` rewrites every path
/// beneath it and renames its files into a directory that does not exist. The replacement here is against
/// the file name only, and the first occurrence only.
fn error_video_tag(options: &Options) -> Result<StepResult, String> {
    let config = options.config;
    let root = config.root();
    let mut result = StepResult::default();
    let videos = config.videos_dir();
    if !videos.is_dir() {
        return Ok(result);
    }

    for path in walk_files(&videos) {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let Some(at) = name.find("-ERROR.") else { continue };
        let renamed = format!("{}-ERROR1.{}", &name[..at], &name[at + "-ERROR.".len()..]);
        let parent = match path.parent() {
            Some(parent) => parent,
            None => continue,
        };
        let target = parent.join(&renamed);

        // Both ends of the rename are confined, and the new name is re-validated rather than trusted:
        // replacing `-ERROR.` with `-ERROR1.` can push a name over the component limit or, given a file
        // named `nul-ERROR.txt`, into the device namespace.
        if let Err(e) = pathguard::check_component(&renamed) {
            result.blocked.push(format!("{} would become {renamed}, which is not a name I will write: {e}", path.display()));
            continue;
        }
        if pathguard::confine(root, &target).is_err() {
            result.blocked.push(format!("{} would land outside the install root; refusing", path.display()));
            continue;
        }
        if target.exists() {
            result.blocked.push(format!("{} already exists; {} is left as it is", target.display(), path.display()));
            continue;
        }

        result.actions.push(format!("rename {name} -> {renamed}"));
        if options.dry_run {
            continue;
        }
        match std::fs::rename(&path, &target) {
            Ok(()) if target.exists() && !path.exists() => {}
            Ok(()) => result.blocked.push(format!("{} renamed to {} and neither side of that is visible", path.display(), target.display())),
            Err(e) => result.blocked.push(format!("{}: {e}", path.display())),
        }
    }
    Ok(result)
}

/// Every file under `root`, no follow-through of symlinks, bounded depth.
///
/// `MAX_DEPTH` exists because a user-created junction loop inside `videos/` would otherwise make this
/// walk never return, and a migration that hangs is indistinguishable from one that is still working.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 12;
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match entry.file_type() {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push((path, depth + 1));
            } else if meta.is_file() {
                out.push(path);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The index schema — win_title and deep_linking
// ---------------------------------------------------------------------------

/// Add the two late-introduced columns to every month file that lacks them.
///
/// This is `db_manager.db_update_table_product_routine`, and it is the step the whole safety argument
/// turns on. It works because `ALTER TABLE ... ADD COLUMN` is *additive*: a month file with seven columns
/// and one with nine are both readable by both versions of the code, because every reader resolves
/// columns by name (`schema::column_names`) rather than by position. So an interrupted schema pass leaves
/// a readable database, which is the property the brief demands and the reason no step in this crate
/// rewrites a table.
///
/// Each file is backed up and verified *before* the ALTER anyway: "additive and safe" describes SQLite's
/// behaviour, not the behaviour of a disk that is full.
fn index_schema(options: &Options) -> Result<StepResult, String> {
    let config = options.config;
    let mut result = StepResult::default();
    let db_dir = config.db_dir();
    if !db_dir.is_dir() {
        return Ok(result);
    }

    for month in wind_store::read::discover(&db_dir) {
        let name = month.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        // The user half of the name came out of a directory listing. `parse_month_db` will happily return
        // `user = ".."` for the legal NTFS name `.._2026-09_wind.db`, and the next name built with
        // `paths::month_filename(user, ..)` escapes the database directory. Refuse the file, keep it, and
        // say so — do not silently skip it, because "your September index is missing" needs a cause.
        if let Err(e) = pathguard::check_component(&month.user) {
            result.blocked.push(format!("{name}: its owner component is not a safe name ({e}); the file is left untouched"));
            continue;
        }
        if pathguard::confine(&db_dir, &month.path).is_err() || pathguard::no_reparse_points(&month.path).is_err() {
            result.blocked.push(format!("{name}: not a plain file inside {} — refusing to open it", db_dir.display()));
            continue;
        }

        let before = match inspect(&month.path) {
            Ok(facts) => facts,
            Err(e) => {
                result.blocked.push(format!("{name}: {e}"));
                continue;
            }
        };
        let missing: Vec<&str> = schema::LATE_COLUMNS.iter().filter(|c| !before.columns.iter().any(|name| name == *c)).copied().collect();
        if missing.is_empty() {
            continue;
        }
        result.actions.push(format!(
            "{name}: add {} ({} columns now, {} rows)",
            missing.join(" and "),
            before.columns.len(),
            before.rows
        ));
        if options.dry_run {
            continue;
        }

        let protected = match backup::protect_once(config, &month.path, &options.stamp, false) {
            Ok(Some(backup)) => backup,
            Ok(None) => {
                result.blocked.push(format!("{name}: cannot be read to be backed up"));
                continue;
            }
            Err(e) => {
                result.blocked.push(format!("{name}: {e}"));
                continue;
            }
        };
        if protected.reused {
            result.notes.push(format!("{name}: reusing the verified backup at {}", protected.target.display()));
        }

        // One transaction, so the two columns arrive together or not at all. It is not *required* for
        // readability — a file with eight columns is as readable as one with seven or nine — but it keeps
        // `win_title` and `deep_linking` from being half-present, which is the state where a reader that
        // checks only the first of them starts writing NULLs into a column the writer never fills.
        match alter(&month.path, &missing) {
            Ok(after) => {
                if after.rows != before.rows {
                    result.blocked.push(format!(
                        "{name}: row count went from {} to {} across an ALTER that should not touch rows; restore from {}",
                        before.rows,
                        after.rows,
                        protected.target.display()
                    ));
                    continue;
                }
                result.notes.push(format!("{name}: {} -> {} columns, {} rows unchanged, backup verified at {}", before.columns.len(), after.columns.len(), after.rows, protected.target.display()));
            }
            Err(e) => result.blocked.push(format!("{name}: {e}; the verified backup is at {}", protected.target.display())),
        }
    }
    Ok(result)
}

#[derive(Debug, Clone)]
struct Facts {
    columns: Vec<String>,
    rows: i64,
}

/// Read a month file's shape without writing a byte to it.
///
/// `wind_store::read::Month::open_read` is not usable here: it works through the `_TEMP_READ.db` copy,
/// and *creating* that copy is a write, which a `--dry-run` and a `doctor` are both forbidden from doing.
/// `open_write` would migrate the schema on the spot, which is the thing being assessed rather than done.
/// This is the gap reported about `wind-store`: it exposes no "open this exact file, read-only, without
/// staging a copy".
fn inspect(path: &Path) -> Result<Facts, String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("cannot be opened read-only: {e}"))?;
    let columns = schema::column_names(&conn).map_err(|e| e.to_string())?;
    let rows = wind_store::read::count_rows(&conn).map_err(|e| e.to_string())?;
    Ok(Facts { columns, rows })
}

/// Add the missing columns inside one transaction, then report the shape afterwards.
fn alter(path: &Path, missing: &[&str]) -> Result<Facts, String> {
    let mut conn = Connection::open(path).map_err(|e| format!("cannot be opened for the schema pass: {e}"))?;
    // The rollback journal, not WAL: this is the mode every other writer in the install uses, and
    // switching a user's month file to WAL would leave `-wal`/`-shm` siblings that the Python reader and
    // the `_TEMP_READ` copy strategy both have to cope with.
    conn.pragma_update(None, "journal_mode", "delete")
        .map_err(|e| format!("{}: cannot set the rollback journal: {e}", path.display()))?;
    let before = conn.transaction().map_err(|e| e.to_string())?;
    // `ensure_schema` is `wind-store`'s own ALTER path, called through the transaction so both columns
    // land together. Handing it `&tx` works because `Transaction` derefs to `Connection`.
    let columns = schema::ensure_schema(&before).map_err(|e| e.to_string())?;
    for column in missing {
        if !columns.iter().any(|c| c == column) {
            // Rolling back by dropping the transaction rather than committing a half-state.
            return Err(format!("{column} is still absent after ensure_schema"));
        }
    }
    before.commit().map_err(|e| e.to_string())?;
    drop(conn);
    inspect(path)
}

// ---------------------------------------------------------------------------
// The standing reconciliation
// ---------------------------------------------------------------------------

fn config_reconcile(options: &Options) -> Result<StepResult, String> {
    let outcome = match configfile::reconcile(options.config, &options.stamp, options.dry_run) {
        Ok(outcome) => outcome,
        // A config that will not parse is a *blocker*, not an aborted run. Everything else the user
        // asked for — the folder moves, the index columns, the retry tags — is still offerable, and
        // `doctor` in particular must be able to finish its report on the one install where a report is
        // most needed. Returning `Err` here would make the command that diagnoses a broken config refuse
        // to run on a broken config.
        Err(e) => {
            let mut blocked = StepResult::default();
            blocked.blocked.push(format!("{e}"));
            return Ok(blocked);
        }
    };
    let mut result = StepResult::default();
    if outcome.skipped {
        result.notes.push("there is no userdata/config_user.json yet, so there is nothing to reconcile".to_string());
        return Ok(result);
    }
    for key in &outcome.added {
        result.actions.push(format!("{key}: added from the shipped defaults"));
    }
    if !outcome.preserved.is_empty() {
        // The line that documents the difference between this program and the one it replaces.
        result.notes.push(format!(
            "{} key(s) the Python reconciler would delete are kept: {}",
            outcome.preserved.len(),
            outcome.preserved.join(", ")
        ));
    }
    if let Some(backup) = &outcome.backup {
        result.notes.push(format!("user config backed up to {}", backup.target.display()));
    }
    if !outcome.written && !outcome.added.is_empty() {
        result.notes.push("dry run: nothing written".to_string());
    }
    Ok(result)
}

/// A machine-readable rendering of a report, for `--json` and for tests.
pub fn to_json(report: &Report) -> Value {
    let steps: Vec<Value> = report
        .steps
        .iter()
        .map(|(step, result)| {
            json!({
                "id": step.id,
                "title": step.title,
                "since": step.since.map(|s| version_text(&s)).unwrap_or_else(|| "-".to_string()),
                "actions": result.actions,
                "blocked": result.blocked,
                "notes": result.notes,
                "fingerprint": result.fingerprint,
            })
        })
        .collect();
    json!({ "steps": steps, "marker": report.marker.as_ref().map(|p| p.to_string_lossy().to_string()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_ordered_by_the_release_that_introduced_them() {
        let versions: Vec<Option<[u32; 3]>> = STEPS.iter().map(|s| s.since.map(|r| r.parts)).collect();
        let mut last = [0, 0, 0];
        for version in versions.iter().flatten() {
            assert!(last <= *version, "{STEPS:?} is not in release order");
            last = *version;
        }
        assert_eq!(
            STEPS.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["startup-shortcut", "config-db-path", "legacy-layout", "legacy-config-file", "error-video-tag", "index-schema", "config-reconcile"]
        );
    }

    #[test]
    fn a_release_reads_the_shape_upstream_writes() {
        assert_eq!(Release::parse("0.0.12").map(|r| r.parts), Some([0, 0, 12]));
        assert_eq!(Release::parse(" 0.0.9 ").map(|r| r.parts), Some([0, 0, 9]));
        assert_eq!(Release::parse("0.1").map(|r| r.parts), Some([0, 1, 0]));
        assert_eq!(Release::parse("nonsense"), None);
        assert_eq!(Release::parse(""), None);
        // 0.0.12 sorts after 0.0.9, which is the whole reason the comparison is not a string compare.
        assert!(Release { parts: [0, 0, 9] }.at_or_before(&Release { parts: [0, 0, 12] }));
        assert!(!Release { parts: [0, 0, 12] }.at_or_before(&Release { parts: [0, 0, 9] }));
        assert!(Release { parts: [0, 1, 0] }.at_or_before(&Release { parts: [0, 0, 31] }) == false);
    }

    #[test]
    fn from_version_skips_the_steps_the_installer_claims_are_past() {
        let root = std::env::temp_dir().join(format!("wind-setup-steps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let config = Config::load(&root).unwrap();
        let all = offered_steps(&Options { config: &config, dry_run: true, from_version: None, stamp: "s".into() });
        assert_eq!(all.len(), STEPS.len());
        let later = offered_steps(&Options { config: &config, dry_run: true, from_version: Release::parse("0.0.12"), stamp: "s".into() });
        assert_eq!(
            later.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["index-schema", "config-reconcile"],
            "an install migrated *through* 0.0.12 keeps the 0.0.12 rename and loses nothing else"
        );
        // One release earlier and the rename is offered again — the boundary is inclusive of `from`.
        let boundary = offered_steps(&Options { config: &config, dry_run: true, from_version: Release::parse("0.0.11"), stamp: "s".into() });
        assert_eq!(
            boundary.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["error-video-tag", "index-schema", "config-reconcile"]
        );
        let never = offered_steps(&Options { config: &config, dry_run: true, from_version: Release::parse("9.9.9"), stamp: "s".into() });
        assert_eq!(never.iter().map(|s| s.id).collect::<Vec<_>>(), vec!["config-reconcile"], "a standing invariant is never skipped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_fingerprint_of_a_step_describes_state_not_time() {
        let root = std::env::temp_dir().join(format!("wind-setup-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("userdata/db")).unwrap();
        let config = Config::load(&root).unwrap();
        let before = describe_state(&config, "index-schema");
        assert_eq!(before, describe_state(&config, "index-schema"), "two calls, same answer");
        std::fs::write(root.join("userdata/db/default_2026-10_wind.db"), b"new month").unwrap();
        assert_ne!(before, describe_state(&config, "index-schema"), "a month appearing must change the fingerprint");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walking_a_missing_directory_is_empty_rather_than_an_error() {
        assert!(walk_files(Path::new("Z:/no/such/tree")).is_empty());
    }

    #[test]
    fn the_legacy_list_is_the_one_upstream_moves() {
        assert_eq!(
            legacy_candidates(),
            ["videos", "db", "db_imgemb", "result_lightbox", "result_timeline", "result_wintitle"]
        );
    }
}
