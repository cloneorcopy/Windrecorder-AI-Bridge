//! The state of an install, from a command that is forbidden from changing it.
//!
//! `windsetup doctor` is the read-only twin of `migrate`: same discovery, same path validation, none of
//! the writes. Three of its jobs cannot be done by any other command in the workspace:
//!
//!   * **name the layer in effect.** Windrecorder has two config files and a legacy third location, and
//!     which one a user is editing is not obvious from an install that has been upgraded four times.
//!   * **show a half-migrated month file.** A seven-column file and a nine-column file sitting in the same
//!     directory is a real state, reachable by a crash mid-`migrate`, and it is invisible to every other
//!     tool because all of them resolve columns by name and carry on. This is where it becomes one line of
//!     output.
//!   * **say what `migrate` would change, without changing it.** That question has to be answerable at
//!     2 a.m. by someone who is not about to run a write against data they cannot recreate.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use wind_base::config::Config;
use wind_base::fslock::{lock_state, LockState};
use wind_store::schema;

use crate::configfile;
use crate::engines;
use crate::hash;
use crate::layout::Layout;
use crate::marker::{self, Marker};
use crate::migrate;
use crate::pathguard;

#[derive(Debug, Clone)]
pub struct MonthFacts {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub columns: Vec<String>,
    pub rows: Option<i64>,
    /// Why the row count or column set could not be read, when it could not.
    pub problem: Option<String>,
    /// The name parses as a month file but the owner component is not a safe path element.
    pub unsafe_name: bool,
}

impl MonthFacts {
    pub fn missing_columns(&self) -> Vec<&'static str> {
        schema::LATE_COLUMNS
            .iter()
            .filter(|column| !self.columns.iter().any(|c| c == *column))
            .copied()
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct LockFacts {
    pub role: &'static str,
    pub path: PathBuf,
    pub state: String,
}

/// Where the video step's encoder comes from.
///
/// `asked` is what [`Config::ffmpeg_path`] answered: an absolute path when the install holds one, a bare
/// name when it is leaving the question to `PATH`. `found` is the file that will actually run, or `None`.
///
/// `None` has to be said out loud because nothing else reports it. A missing ffmpeg raises no error in the
/// recorder — screenshots keep being taken, keep piling up in `cache_screenshot\`, and no `.mp4` ever
/// appears, which reads to a user as "the program is not recording" when the program is recording fine and
/// cannot finish.
#[derive(Debug)]
pub struct FfmpegFacts {
    pub asked: PathBuf,
    pub found: Option<PathBuf>,
}

/// The first directory on `directories` that holds `name`.
///
/// `directories` is a parameter rather than the process environment on purpose: a test that asked the real
/// `PATH` would pass on a machine with ffmpeg installed and fail on one without, which is the opposite of
/// what a test is for. Windows resolves a bare `ffmpeg` to `ffmpeg.exe`, so both spellings are tried.
fn first_on_path(name: &str, directories: &[PathBuf]) -> Option<PathBuf> {
    let extensions = [name.to_string(), format!("{name}.exe")];
    for candidate in extensions.iter().flat_map(|spell| directories.iter().map(move |dir| dir.join(spell))) {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Ask the config where its encoder is, then answer with a file rather than a hope.
fn ffmpeg_facts(config: &Config, directories: &[PathBuf]) -> FfmpegFacts {
    let asked = config.ffmpeg_path();
    // `ffmpeg_path` only returns an absolute path for a file it has already seen, so that answer needs no
    // second look; a relative one is the loader's problem, and the loader's rules are `first_on_path`.
    let found = if asked.is_absolute() { Some(asked.clone()) } else { first_on_path("ffmpeg", directories) };
    FfmpegFacts { asked, found }
}

/// The VIDEO STEP lines, as a function of the facts alone. Split out of [`render`] so the sentence a person
/// reads when ffmpeg is missing is testable on a machine that has one.
fn ffmpeg_lines(facts: &FfmpegFacts) -> String {
    let mut out = String::new();
    match &facts.found {
        Some(path) if facts.asked.is_absolute() => {
            out.push_str(&format!("  ok       {}   (this install)\n", engines::escape_non_ascii(&path.display().to_string())));
        }
        Some(path) => out.push_str(&format!(
            "  ok       {}   (from PATH; the app folder has none, so an update can change it under you)\n",
            engines::escape_non_ascii(&path.display().to_string())
        )),
        None => {
            out.push_str(&format!(
                "  !!       no ffmpeg: asked {:?}, found nothing on PATH\n",
                engines::escape_non_ascii(&facts.asked.display().to_string())
            ));
            out.push_str("           screenshots are still being taken and will keep piling up in cache_screenshot\\,\n");
            out.push_str("           but no segment will ever become a video. Put ffmpeg.exe in the app folder (next\n");
            out.push_str("           to bin\\) or install it system-wide; `windmaint convert` then catches up.\n");
        }
    }
    out
}


#[derive(Debug)]
pub struct Report {
    pub root: PathBuf,
    pub layers: configfile::Layers,
    pub user_config_error: Option<String>,
    pub drift: Option<configfile::Drift>,
    pub layout: Layout,
    pub created_slots: Vec<String>,
    pub months: Vec<MonthFacts>,
    pub total_rows: i64,
    pub locks: Vec<LockFacts>,
    pub marker: Option<Marker>,
    pub marker_path: PathBuf,
    pub stale_steps: Vec<String>,
    pub python_release: Option<String>,
    pub error_videos: Vec<String>,
    pub legacy_present: Vec<String>,
    pub onboarding: String,
    pub plan: migrate::Report,
    pub startup_shortcut: Option<PathBuf>,
    pub ffmpeg: FfmpegFacts,
}

/// Read everything. `migrate`'s own dry run is computed at the end so the two commands cannot disagree
/// about what the next run would do — there is one plan function and both call it.
pub fn inspect(config: &Config) -> Result<Report, String> {
    let root = config.root().to_path_buf();
    let layers = configfile::layers(config);

    let user_config_error = match configfile::read_object(config.root(), configfile::USER_RELPATH) {
        Ok(_) => None,
        Err(configfile::ReadError::Missing(_)) => Some("absent — the install is running on the shipped defaults alone".to_string()),
        Err(e) => Some(e.to_string()),
    };
    let drift = configfile::drift(config).ok();

    let layout = Layout::resolve(config);
    let created_slots = layout.slots.iter().filter(|s| s.path.is_dir()).map(|s| s.name.to_string()).collect();

    let mut months = Vec::new();
    let db_dir = config.db_dir();
    for month in wind_store::read::discover(&db_dir) {
        let name = month.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let size = std::fs::metadata(&month.path).map(|m| m.len()).unwrap_or(0);
        let unsafe_name = pathguard::check_component(&month.user).is_err();
        let facts = if unsafe_name {
            MonthFacts {
                name,
                path: month.path.clone(),
                size,
                columns: Vec::new(),
                rows: None,
                problem: Some(format!("its owner component {:?} is not a safe path element; migrate will refuse it", month.user)),
                unsafe_name: true,
            }
        } else if pathguard::no_reparse_points(&month.path).is_err() {
            MonthFacts {
                name,
                path: month.path.clone(),
                size,
                columns: Vec::new(),
                rows: None,
                problem: Some("not a plain file inside the database directory".to_string()),
                unsafe_name: false,
            }
        } else {
            match read_facts(&month.path, &name) {
                Ok(facts) => facts,
                Err(problem) => MonthFacts {
                    name,
                    path: month.path.clone(),
                    size,
                    columns: Vec::new(),
                    rows: None,
                    problem: Some(problem),
                    unsafe_name: false,
                },
            }
        };
        months.push(facts);
    }
    let total_rows = months.iter().filter_map(|m| m.rows).sum();

    let locks = lock_facts(config);

    let marker = Marker::load(config);
    let marker_path = marker::marker_path(config);
    let stale_steps = match &marker {
        Some(record) => migrate::STEPS
            .iter()
            .filter_map(|step| {
                record.steps.get(step.id).map(|saved| (step, saved))
            })
            .filter(|(step, saved)| {
                let now = migrate::state_fingerprint(config, step.id);
                now != saved.fingerprint
            })
            .map(|(step, _)| step.id.to_string())
            .collect(),
        None => Vec::new(),
    };

    let error_videos: Vec<String> = walk_error_videos(&config.videos_dir());
    let legacy_present: Vec<String> = migrate::legacy_candidates()
        .iter()
        .filter(|name| config.root().join(name).exists())
        .map(|name| name.to_string())
        .collect();

    // `db_manager.check_is_onboarding` in Python: no index at all, or exactly one month with one row in
    // it, means the user has not recorded anything worth showing and the onboarding markdown is what they
    // should see.
    let onboarding = onboarding_state(config, months.len(), total_rows);

    let plan_options = migrate::Options {
        config,
        dry_run: true,
        from_version: None,
        stamp: "dry-run".to_string(),
    };
    let plan = migrate::run(&plan_options).map_err(|e| format!("cannot compute what migrate would do: {e}"))?;

    Ok(Report {
        root,
        layers,
        user_config_error,
        drift,
        layout,
        created_slots,
        months,
        total_rows,
        locks,
        marker,
        marker_path,
        stale_steps,
        python_release: marker::python_release(config.root()),
        error_videos,
        legacy_present,
        onboarding,
        plan,
        startup_shortcut: migrate::startup_shortcut_path(),
        ffmpeg: ffmpeg_facts(config, &system_path_directories()),
    })
}

/// The directories the OS would search for a bare program name. Empty when `PATH` is unset or unreadable,
/// which is a machine where ffmpeg can only ever be the file inside the install.
fn system_path_directories() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).filter(|entry| !entry.as_os_str().is_empty()).collect())
        .unwrap_or_default()
}

/// One month file's shape, read strictly read-only and without staging a copy.
fn read_facts(path: &Path, name: &str) -> Result<MonthFacts, String> {
    let conn = Connection::open_with_flags(path, read_only_flags())
        .map_err(|e| format!("cannot be opened read-only: {e}"))?;
    let columns = schema::column_names(&conn).map_err(|e| e.to_string())?;
    let rows = wind_store::read::count_rows(&conn).ok();
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let _ = name;
    Ok(MonthFacts {
        name: path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
        path: path.to_path_buf(),
        size,
        columns,
        rows,
        problem: None,
        unsafe_name: false,
    })
}

/// The same flag set `wind-store`'s own read-only open uses, spelled once so `doctor` cannot silently
/// link a shared-cache connection into a different threading contract than the reader it is describing.
/// A function rather than a `const`: `bitflags`' `|` is a trait method and not available in a const.
fn read_only_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn lock_facts(config: &Config) -> Vec<LockFacts> {
    let mut out = vec![
        fact("tray", config.tray_lock_path()),
        fact("record", config.record_lock_path()),
        LockFacts {
            role: "image-embedding",
            path: config.lock_dir().join(config.str_or("img_emb_lock_name", "LOCK_FILE_IMG_EMB.MD")),
            state: describe(lock_state(&config.lock_dir().join(config.str_or("img_emb_lock_name", "LOCK_FILE_IMG_EMB.MD")))),
        },
    ];
    // The maintain lock is a *directory* upstream, so `lock_state` on it reads as `Unreadable` and would
    // be reported as a corrupt lock forever. Its PID child is the real answer.
    let maintain = config.maintain_lock_dir();
    let state = if maintain.is_dir() {
        match std::fs::read_to_string(maintain.join("PID")) {
            Ok(body) => match body.trim().parse::<u32>() {
                Ok(pid) if wind_base::fslock::is_process_running(pid) => format!("held by running process {pid}"),
                Ok(dead) => format!("stale directory naming dead process {dead}"),
                Err(_) => "directory present with no readable PID; something else owns it".to_string(),
            },
            Err(_) => "directory present and empty: upstream's own container, not a claim".to_string(),
        }
    } else {
        "free".to_string()
    };
    out.push(LockFacts { role: "maintain", path: maintain, state });
    out
}

fn fact(role: &'static str, path: PathBuf) -> LockFacts {
    let state = describe(lock_state(&path));
    LockFacts { role, path, state }
}

fn describe(state: LockState) -> String {
    match state {
        LockState::Free => "free".to_string(),
        LockState::Owned => "held by this process".to_string(),
        LockState::HeldBy { pid, alive: true } => format!("held by running process {pid}"),
        LockState::HeldBy { pid, alive: false } => format!("stale: names dead process {pid}"),
        LockState::Unreadable => "present but naming no process; a foreign tool's file".to_string(),
    }
}

fn walk_error_videos(videos: &Path) -> Vec<String> {
    const MAX_DEPTH: usize = 6;
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(videos.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push((path, depth + 1));
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.contains("-ERROR.") {
                    out.push(name.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// Which onboarding text the interface would show, and why.
///
/// The markdown itself lives in `config_src/onboarding_{en,ja,sc}.md` — or in the
/// `windrecorder/config_src/` copy an overlay install still keeps, which is
/// [`Config::config_src_file`]'s decision rather than this function's — and is rendered by the UI;
/// what belongs here is the *predicate*, because "the app is showing me the welcome page" and "my
/// index is empty because OCR found no engine" look identical to a user and are different problems.
fn onboarding_state(config: &Config, month_count: usize, total_rows: i64) -> String {
    let markdown = config.config_src_file(&format!("onboarding_{}.md", config.str_or("lang", "en")));
    let exists = markdown.is_file();
    if month_count == 0 {
        return format!(
            "ONBOARDING: no index files exist yet — the UI shows the welcome text ({}{}). Start the recorder, or run `check-engines` if it never fills.",
            markdown.display(),
            if exists { "" } else { ", MISSING from config_src" }
        );
    }
    if month_count == 1 && total_rows <= 1 {
        return format!(
            "ONBOARDING: one month with {total_rows} row(s) — still inside the welcome state ({}{}).",
            markdown.display(),
            if exists { "" } else { ", MISSING from config_src" }
        );
    }
    format!("INDEXED: {month_count} month file(s), {total_rows} row(s); the welcome text is not shown")
}

/// Render the report as the text `windsetup doctor` prints.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let line = |out: &mut String, label: &str, value: &str| {
        out.push_str(&format!("{label:<22} {value}\n"));
    };

    out.push_str("windsetup doctor\n");
    out.push_str(&format!("{}\n", "-".repeat(72)));
    line(&mut out, "install root", &report.root.display().to_string());
    line(
        &mut out,
        "app versions",
        &format!(
            "python {} | windsetup {} | migrated-to {}",
            report.python_release.as_deref().unwrap_or("unknown (no windrecorder/__init__.py)"),
            env!("CARGO_PKG_VERSION"),
            report.marker.as_ref().map(|m| m.migrated_to.as_str()).unwrap_or("never")
        ),
    );

    out.push_str("\nCONFIG LAYERS\n");
    line(&mut out, "defaults", &layer_flag(&report.layers.defaults, report.layers.defaults_present));
    line(&mut out, "user", &layer_flag(&report.layers.user, report.layers.user_present));
    if report.layers.legacy_user_present {
        let legacy = report.root.join(configfile::LEGACY_USER_RELPATH);
        out.push_str(&format!("{}   ← legacy, pre-0.0.9; `migrate` moves this\n", legacy.display()));
    }
    line(&mut out, "in effect", &report.layers.effective);
    if let Some(error) = &report.user_config_error {
        out.push_str(&format!("!! user config: {error}\n"));
    }
    if let Some(drift) = &report.drift {
        line(&mut out, "keys to add", &format!("{}", drift.missing_from_user.len()));
        out.push_str(&format!(
            "keys kept that the defaults lack: {}  (the Python reconciler would DELETE these)\n",
            drift.extra_in_user.len()
        ));
        if !drift.extra_in_user.is_empty() {
            out.push_str(&format!("   {}\n", engines::escape_non_ascii(&drift.extra_in_user.join(", "))));
        }
    }

    out.push_str("\nLAYOUT\n");
    for slot in &report.layout.slots {
        let present = slot.path.is_dir();
        out.push_str(&format!(
            "  {} {}\n",
            if present { "ok    " } else { "absent" },
            engines::escape_non_ascii(&relative(&report.root, &slot.path))
        ));
    }
    let _ = &report.created_slots;

    out.push_str("\nVIDEO STEP\n");
    out.push_str(&ffmpeg_lines(&report.ffmpeg));

    out.push_str("\nINDEX\n");
    if report.months.is_empty() {
        out.push_str("  no month files in the database directory\n");
    }
    for month in &report.months {
        let missing = month.missing_columns();
        let shape = if month.columns.is_empty() {
            "unreadable".to_string()
        } else {
            format!("{} columns", month.columns.len())
        };
        out.push_str(&format!(
            "  {:<34} {:>9} B  {:<11} {:>5} row(s)",
            engines::escape_non_ascii(&month.name),
            month.size,
            shape,
            month.rows.map(|r| r.to_string()).unwrap_or_else(|| "?".to_string())
        ));
        if !missing.is_empty() {
            out.push_str(&format!("   HALF-MIGRATED: no {}", missing.join(", ")));
        }
        if let Some(problem) = &month.problem {
            out.push_str(&format!("   {problem}"));
        }
        out.push('\n');
        if !month.columns.is_empty() {
            out.push_str(&format!("      {}\n", month.columns.join(", ")));
        }
    }
    out.push_str(&format!("  total: {} row(s) across {} month file(s)\n", report.total_rows, report.months.len()));

    if !report.legacy_present.is_empty() {
        out.push_str(&format!("\nPRE-SPLIT FOLDERS STILL AT THE ROOT\n  {}\n", report.legacy_present.join(", ")));
    }
    if !report.error_videos.is_empty() {
        out.push_str(&format!(
            "\nVIDEOS AWAITING THE 0.0.12 RETRY TAG ({})\n",
            report.error_videos.len()
        ));
        for name in report.error_videos.iter().take(10) {
            out.push_str(&format!("  {}\n", engines::escape_non_ascii(name)));
        }
        if report.error_videos.len() > 10 {
            out.push_str(&format!("  ... {} more\n", report.error_videos.len() - 10));
        }
    }

    out.push_str("\nLOCKS\n");
    for lock in &report.locks {
        out.push_str(&format!("  {:<16} {:<34} {}\n", lock.role, engines::escape_non_ascii(&relative(&report.root, &lock.path)), lock.state));
    }
    if let Some(shortcut) = &report.startup_shortcut {
        out.push_str(&format!(
            "  {:<16} {:<34} {}\n",
            "boot shortcut",
            engines::escape_non_ascii(&shortcut.display().to_string()),
            if shortcut.exists() { "stale: points at a launcher that no longer exists" } else { "none" }
        ));
    }

    out.push_str("\nMIGRATION MARKER\n");
    match &report.marker {
        None => out.push_str(&format!("  no marker at {}\n  every step below is offered on its own merits, so this is not a claim that nothing was migrated\n", report.marker_path.display())),
        Some(record) => {
            out.push_str(&format!("  {}\n  migrated_to {} by windsetup {}, updated {}\n", report.marker_path.display(), record.migrated_to, record.writer, record.updated_at));
            for (id, saved) in &record.steps {
                out.push_str(&format!("  {:<20} {} at {}\n", id, hash::short_digest(&saved.fingerprint), saved.done_at));
                for note in &saved.notes {
                    out.push_str(&format!("      {}\n", engines::escape_non_ascii(note)));
                }
            }
            if !report.stale_steps.is_empty() {
                out.push_str(&format!(
                    "  state changed since these steps ran: {} — migrate will re-check them\n",
                    report.stale_steps.join(", ")
                ));
            }
        }
    }

    out.push_str("\nWHAT `migrate` WOULD CHANGE\n");
    let mut pending = 0usize;
    let mut blockers = 0usize;
    for (step, result) in &report.plan.steps {
        if result.actions.is_empty() && result.blocked.is_empty() {
            out.push_str(&format!("  nothing to do   {}\n", step.id));
            continue;
        }
        pending += result.actions.len();
        blockers += result.blocked.len();
        out.push_str(&format!("  {:<20} {}\n", step.id, step.title));
        for action in &result.actions {
            out.push_str(&format!("      - {}\n", engines::escape_non_ascii(action)));
        }
        for blocked in &result.blocked {
            out.push_str(&format!("      ! {}\n", engines::escape_non_ascii(blocked)));
        }
        for note in &result.notes {
            out.push_str(&format!("      # {}\n", engines::escape_non_ascii(note)));
        }
    }
    out.push_str(&format!("\n{pending} action(s), {blockers} blocker(s) — this run changed nothing\n"));
    out.push_str(&format!("{}\n", report.onboarding));
    out
}

fn layer_flag(path: &Path, present: bool) -> String {
    if !present {
        return format!("{}  (absent)", engines::escape_non_ascii(&path.display().to_string()));
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    format!("{}  ({size} B, {}…)", engines::escape_non_ascii(&path.display().to_string()), hash::short_digest(&hash::digest_file(path).unwrap_or_default()))
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// The JSON rendering, for anything that reads this instead of a person.
pub fn to_json(report: &Report) -> Value {
    serde_json::json!({
        "root": report.root.to_string_lossy(),
        "python_release": report.python_release,
        "windsetup": env!("CARGO_PKG_VERSION"),
        "latest_known_release": marker::LATEST_KNOWN_RELEASE,
        "layers": {
            "defaults": report.layers.defaults_present,
            "user": report.layers.user_present,
            "legacy_user": report.layers.legacy_user_present,
            "effective": report.layers.effective,
            "user_config_error": report.user_config_error,
        },
        "drift": report.drift.as_ref().map(|d| serde_json::json!({
            "missing_from_user": d.missing_from_user,
            "extra_in_user_python_would_delete": d.extra_in_user,
        })),
        "layout": report.layout.slots.iter().map(|s| serde_json::json!({
            "name": s.name, "path": s.path.to_string_lossy(), "present": s.path.is_dir(),
        })).collect::<Vec<_>>(),
        // The same answer the text gives, because the person most likely to be reading this with a script
        // is reading it precisely because no video ever appeared.
        "ffmpeg": {
            "asked": report.ffmpeg.asked.to_string_lossy(),
            "found": report.ffmpeg.found.as_ref().map(|p| p.to_string_lossy().to_string()),
            "sentence": ffmpeg_lines(&report.ffmpeg),
        },
        "months": report.months.iter().map(|m| serde_json::json!({
            "name": m.name, "size": m.size, "columns": m.columns, "rows": m.rows,
            "missing_late_columns": m.missing_columns(), "problem": m.problem,
            "unsafe_name": m.unsafe_name,
        })).collect::<Vec<_>>(),
        "total_rows": report.total_rows,
        "locks": report.locks.iter().map(|l| serde_json::json!({"role": l.role, "path": l.path.to_string_lossy(), "state": l.state})).collect::<Vec<_>>(),
        "marker": report.marker.as_ref().map(|m| serde_json::Value::Object(
            m.steps.iter().map(|(id, s)| (id.clone(), serde_json::json!({"done_at": s.done_at, "fingerprint": s.fingerprint, "notes": s.notes}))).collect()
        )),
        "stale_steps": report.stale_steps,
        "legacy_folders": report.legacy_present,
        "error_videos": report.error_videos,
        "onboarding": report.onboarding,
        "plan": migrate::to_json(&report.plan),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wind-setup-doctor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"lang": "en", "user_name": "default", "db_path": "db", "record_videos_dir": "videos"}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_bare_directory_reports_itself_as_empty_rather_than_failing() {
        let dir = temp("bare");
        let config = Config::load(&dir).unwrap();
        let report = inspect(&config).unwrap();
        assert!(report.months.is_empty());
        assert_eq!(report.total_rows, 0);
        assert!(report.marker.is_none());
        assert!(report.onboarding.contains("ONBOARDING"));
        // The default layer is present, so the plan's config steps have something to talk about but the
        // index step has nothing to touch.
        assert!(!report.plan.steps.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_half_migrated_month_is_visible_in_the_report() {
        let dir = temp("half");
        let db = dir.join("userdata/db");
        std::fs::create_dir_all(&db).unwrap();
        let legacy = db.join("default_2026-08_wind.db");
        {
            let conn = Connection::open(&legacy).unwrap();
            conn.execute_batch("CREATE TABLE video_text (videofile_name VARCHAR(100), picturefile_name VARCHAR(100), videofile_time INT, ocr_text TEXT, is_videofile_exist BOOLEAN, is_picturefile_exist BOOLEAN, thumbnail TEXT);
                                INSERT INTO video_text VALUES ('a.mp4','f.jpg',1,'text',1,1,'t');")
                .unwrap();
        }
        let config = Config::load(&dir).unwrap();
        let report = inspect(&config).unwrap();
        assert_eq!(report.months.len(), 1, "{:?}", report.months);
        let month = &report.months[0];
        assert_eq!(month.columns.len(), 7);
        assert_eq!(month.rows, Some(1));
        assert_eq!(month.missing_columns(), vec!["win_title", "deep_linking"]);
        let text = render(&report);
        assert!(text.contains("HALF-MIGRATED"), "{text}");
        // Reporting is not migrating: the file still has seven columns afterwards.
        let conn = Connection::open_with_flags(&legacy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(schema::column_names(&conn).unwrap().len(), 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_database_directory_listing_cannot_make_a_write() {
        let dir = temp("read-only");
        std::fs::create_dir_all(dir.join("userdata/db")).unwrap();
        std::fs::create_dir_all(dir.join("userdata/videos")).unwrap();
        let before = listing(&dir);
        let config = Config::load(&dir).unwrap();
        let report = inspect(&config).unwrap();
        let _ = render(&report);
        assert_eq!(before, listing(&dir), "doctor created, removed or renamed something");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn listing(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                out.push(path.strip_prefix(root).unwrap_or(&path).display().to_string());
                if path.is_dir() {
                    stack.push(path);
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn an_unsafe_month_name_is_reported_not_read() {
        let dir = temp("traversal");
        let db = dir.join("userdata/db");
        std::fs::create_dir_all(&db).unwrap();
        // A legal NTFS name that parses as a month file owned by `..`.
        std::fs::write(db.join(".._2026-09_wind.db"), b"not really a database").unwrap();
        let config = Config::load(&dir).unwrap();
        let report = inspect(&config).unwrap();
        let month = report.months.iter().find(|m| m.name == ".._2026-09_wind.db").expect("the file is listed");
        assert!(month.unsafe_name, "{month:?}");
        assert!(month.columns.is_empty(), "an unsafe file must not be opened at all");
        let text = render(&report);
        assert!(text.contains("safe path element"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_states_distinguish_a_live_owner_from_a_corpse() {
        let dir = temp("locks");
        let locks = dir.join("cache/locks");
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::write(locks.join("LOCK_FILE_TRAY.MD"), "4000000").unwrap();
        let config = Config::load(&dir).unwrap();
        let report = inspect(&config).unwrap();
        let tray = report.locks.iter().find(|l| l.role == "tray").unwrap();
        assert!(tray.state.contains("dead process"), "{}", tray.state);
        let maintain = report.locks.iter().find(|l| l.role == "maintain").unwrap();
        assert!(maintain.state.contains("free"), "{}", maintain.state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_user_config_is_named_with_its_parse_error() {
        let dir = temp("broken");
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        std::fs::write(dir.join("userdata/config_user.json"), "{\"lang\": \"en\"}").unwrap();
        let config = Config::load(&dir).unwrap();
        // Broken after the handle exists, because `Config::load` itself refuses a malformed layer.
        std::fs::write(dir.join("userdata/config_user.json"), "{\"lang\": \"en\",").unwrap();
        let report = inspect(&config).unwrap();
        let text = render(&report);
        assert!(text.contains("user config"), "{text}");
        assert!(text.contains("config_user.json"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_json_rendering_carries_the_same_facts_as_the_text() {
        let dir = temp("json");
        std::fs::create_dir_all(dir.join("userdata/db")).unwrap();
        let config = Config::load(&dir).unwrap();
        let report = inspect(&config).unwrap();
        let value = to_json(&report);
        assert_eq!(value["total_rows"].as_i64(), Some(0));
        assert!(value["months"].as_array().unwrap().is_empty());
        assert!(value["plan"]["steps"].as_array().unwrap().len() >= crate::migrate::STEPS.len() - 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_render_never_panics_on_a_path_it_cannot_describe() {
        let dir = temp("render");
        std::fs::create_dir_all(dir.join("userdata/videos")).unwrap();
        std::fs::create_dir_all(dir.join("videos")).unwrap();
        // The live location, and the pre-split one, in the same tree.
        std::fs::write(dir.join("userdata/videos/2026-01-01_01-01-01-ERROR.mp4"), b"x").unwrap();
        std::fs::write(dir.join("videos/2026-01-01_01-01-02-ERROR.mp4"), b"x").unwrap();
        let config = Config::load(&dir).unwrap();
        let report = inspect(&config).unwrap();
        assert_eq!(report.error_videos, vec!["2026-01-01_01-01-01-ERROR.mp4".to_string()]);
        assert!(render(&report).contains("RETRY TAG"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_onboarding_predicate_matches_the_python_side() {
        let dir = temp("onboarding");
        let db = dir.join("userdata/db");
        std::fs::create_dir_all(&db).unwrap();
        let config = Config::load(&dir).unwrap();
        assert!(onboarding_state(&config, 0, 0).contains("no index files"));
        assert!(onboarding_state(&config, 1, 1).contains("welcome state"));
        assert!(onboarding_state(&config, 1, 2).contains("INDEXED"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_steady_state_has_no_stale_marker_entries() {
        let dir = temp("stale");
        let config = Config::load(&dir).unwrap();
        let mut record = Marker::default();
        let step = &migrate::STEPS[migrate::STEPS.len() - 1];
        record.steps.insert(
            step.id.to_string(),
            marker::StepRecord { done_at: "x".into(), fingerprint: migrate::state_fingerprint(&config, step.id), notes: vec![] },
        );
        record.save(&config).unwrap();
        let report = inspect(&config).unwrap();
        assert!(report.stale_steps.is_empty(), "{:?}", report.stale_steps);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_read_only_mode_cannot_write() {
        assert!(read_only_flags().contains(OpenFlags::SQLITE_OPEN_READ_ONLY));
        assert!(!read_only_flags().intersects(OpenFlags::SQLITE_OPEN_READ_WRITE));
    }

    /// The case nobody notices, because nothing fails: with no encoder the recorder goes on taking
    /// screenshots forever, so the report has to name the consequence rather than the absence.
    #[test]
    fn a_missing_ffmpeg_is_said_with_what_it_costs() {
        let text = ffmpeg_lines(&FfmpegFacts { asked: PathBuf::from("ffmpeg"), found: None });
        assert!(text.contains("no ffmpeg"), "{text}");
        assert!(text.contains("cache_screenshot"), "{text}");
        assert!(text.contains("windmaint convert"), "{text}");
    }

    /// The install's own copy and somebody else's on `PATH` are different facts: the second one can be
    /// changed under the app by anything the user installs later.
    #[test]
    fn an_ffmpeg_is_reported_by_where_it_came_from() {
        let inside = PathBuf::from("E:/app/ffmpeg.exe");
        let text = ffmpeg_lines(&FfmpegFacts { asked: inside.clone(), found: Some(inside) });
        assert!(text.contains("this install"), "{text}");
        let borrowed = ffmpeg_lines(&FfmpegFacts { asked: PathBuf::from("ffmpeg"), found: Some(PathBuf::from("C:/tools/ffmpeg.exe")) });
        assert!(borrowed.contains("from PATH"), "{borrowed}");
        assert!(!borrowed.contains("no ffmpeg"), "{borrowed}");
    }

    /// The loader's rule, spelled both ways Windows spells it — and a directory that happens to carry the
    /// name is not a program.
    #[test]
    fn a_bare_name_is_found_by_either_spelling_and_a_directory_is_not_a_hit() {
        let with = temp("ffmpeg-found");
        std::fs::write(with.join("ffmpeg.exe"), b"not really").unwrap();
        assert_eq!(first_on_path("ffmpeg", std::slice::from_ref(&with)), Some(with.join("ffmpeg.exe")));
        assert_eq!(first_on_path("ffmpeg", &[]), None, "no directories, no program");

        let shadow = temp("ffmpeg-directory");
        std::fs::create_dir_all(shadow.join("ffmpeg")).unwrap();
        assert_eq!(first_on_path("ffmpeg", &[shadow.clone()]), None, "a directory named like the tool is not the tool");
        let _ = std::fs::remove_dir_all(&with);
        let _ = std::fs::remove_dir_all(&shadow);
    }

    /// A report that found nothing still renders, and the JSON says the same thing the text does — the
    /// person reading this with a script is reading it because no video ever appeared.
    #[test]
    fn the_video_step_is_in_both_renderings() {
        let dir = temp("ffmpeg-report");
        let config = Config::load(&dir).unwrap();
        let report = inspect(&config).unwrap();
        let text = render(&report);
        assert!(text.contains("VIDEO STEP"), "{text}");
        let value = to_json(&report);
        assert_eq!(value["ffmpeg"]["found"].is_null(), report.ffmpeg.found.is_none());
        assert!(!value["ffmpeg"]["sentence"].as_str().unwrap_or_default().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
