//! The migration marker: what a second process can see about what the first one finished.
//!
//! `upgrade_migration_routine.py` has no record of having run. It is a script that runs unconditionally
//! at the top of `onboard_setting.py` and relies on every one of its steps being harmless when repeated
//! — which mostly works, and mostly fails: `shutil.move("videos", "userdata")` is idempotent only until
//! `userdata/videos` already exists, at which point it produces `userdata/videos/videos`, and the
//! `-ERROR.` rename is idempotent only because the new name no longer matches. The cost of the missing
//! record is that a crash halfway through leaves no way to tell which of six steps the user is on.
//!
//! So the marker is not an optimisation, it is the re-entrancy mechanism, and it carries two things:
//!
//!   * a **plan hash** over the steps and the state they were computed against, so a marker written by
//!     different code, or against a tree that has since changed, cannot be mistaken for "already done"
//!     — a stale claim of completion is the worst possible value for a migration record to have; and
//!   * a **per-step record**, written after each step rather than once at the end, so an interrupted run
//!     resumes from the step it did not finish instead of either redoing everything or nothing.
//!
//! Version numbers alone cannot do this job. `--from-version` describes what the *installer* claims to
//! have reached; only a content hash describes what this machine's files actually looked like.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use wind_base::config::Config;

/// The highest Windrecorder release whose migration this build understands.
///
/// A copy of the string in `windrecorder/__init__.py`, kept here because the alternative — parsing a
/// Python file at runtime — makes a data migration depend on a source tree that a packaged install does
/// not have. `doctor` reports both numbers side by side so the drift is visible instead of silent.
pub const LATEST_KNOWN_RELEASE: &str = "0.0.31";

/// `userdata/upgrade_marker.json`.
///
/// Deliberately not a hidden file and not inside `cache/`: it is the answer to "what has already been
/// done to my data", and it belongs where a user looking at their install will find it and where a
/// retention sweep of the regenerable cache will not delete it.
pub fn marker_path(config: &Config) -> PathBuf {
    config.userdata_dir().join("upgrade_marker.json")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRecord {
    pub done_at: String,
    /// A digest of the state this step saw when it completed. `plan_hash` for the whole run, this for
    /// one step, so a step whose inputs changed since it ran is re-offered rather than skipped.
    pub fingerprint: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Marker {
    /// The release this install was migrated to.
    pub migrated_to: String,
    /// `wind-setup`'s own version, so a marker written by an older binary is identifiable.
    pub writer: String,
    /// The instant the marker file was last written, not when the last step finished.
    pub updated_at: String,
    pub plan_hash: String,
    pub steps: BTreeMap<String, StepRecord>,
}

impl Marker {
    /// Load, or `None` when absent or unreadable.
    ///
    /// A corrupt marker is treated as no marker at all rather than as a fatal error, because every step
    /// it guards is idempotent. Refusing to migrate because a bookkeeping file got truncated would be
    /// the marker becoming the reason data is left unmigrated, which is backwards.
    pub fn load(config: &Config) -> Option<Marker> {
        let path = marker_path(config);
        let text = std::fs::read_to_string(&path).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let steps = value.get("steps")?.as_object()?;
        let mut parsed = BTreeMap::new();
        for (name, record) in steps {
            parsed.insert(
                name.clone(),
                StepRecord {
                    done_at: record.get("done_at")?.as_str()?.to_string(),
                    fingerprint: record.get("fingerprint")?.as_str()?.to_string(),
                    notes: record
                        .get("notes")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default(),
                },
            );
        }
        Some(Marker {
            migrated_to: value.get("migrated_to")?.as_str()?.to_string(),
            writer: value.get("writer")?.as_str()?.to_string(),
            updated_at: value.get("updated_at")?.as_str()?.to_string(),
            plan_hash: value.get("plan_hash")?.as_str()?.to_string(),
            steps: parsed,
        })
    }

    pub fn step(&self, name: &str) -> Option<&StepRecord> {
        self.steps.get(name)
    }

    /// Serialize. `serde_json` is already a workspace dependency through `wind-base`, so this is the
    /// format of record and not an extra tree to fetch.
    pub fn to_json(&self) -> String {
        let steps: BTreeMap<&String, Value> = self
            .steps
            .iter()
            .map(|(name, r)| (name, json!({ "done_at": r.done_at, "fingerprint": r.fingerprint, "notes": r.notes })))
            .collect();
        let body = json!({
            "migrated_to": self.migrated_to,
            "writer": self.writer,
            "updated_at": self.updated_at,
            "plan_hash": self.plan_hash,
            "steps": steps,
            "read_me": "Written by `windsetup migrate`. Records which upgrade steps this install has \
                        completed and the digest of the files each one saw. Safe to delete: every step \
                        is idempotent, so migrate will re-derive the state and re-run what is missing.",
        });
        // Two-space indent and unescaped non-ASCII, matching how the app writes every other JSON file.
        pretty(&body)
    }

    /// Write atomically.
    ///
    /// A marker torn by a power cut would read as "no steps recorded", which is survivable, or — worse —
    /// as valid JSON whose `steps` object is half a step short of the truth. Stage-then-rename removes
    /// the second case: a reader sees the old complete file or the new complete file, never a splice.
    pub fn save(&self, config: &Config) -> Result<PathBuf, String> {
        let path = marker_path(config);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let tmp = path.with_extension("json.stage");
        std::fs::write(&tmp, self.to_json()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            // Leave nothing half-written behind on failure.
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })?;
        Ok(path)
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
}

/// A digest over the plan: the ordered step names, the release each landed in, and the state each one
/// was computed against.
///
/// This is what makes "running it twice is a no-op the second time" a *checked* property rather than a
/// hope. The second run recomputes the same plan from the same tree and gets the same hash, so the
/// marker's claim is accepted. If a month file appears in between, the per-step fingerprints differ and
/// the affected step is offered again; if the migration code itself changes, `writer` and the step list
/// change and nothing in the old marker is trusted for the new step.
pub fn plan_hash(steps: &[(String, String, String)]) -> String {
    let mut blob = String::new();
    for (name, since, fingerprint) in steps {
        blob.push_str(name);
        blob.push('\n');
        blob.push_str(since);
        blob.push('\n');
        blob.push_str(fingerprint);
        blob.push('\n');
    }
    crate::hash::sha256_hex(blob.as_bytes())
}

/// Where the install's own Python version says it is, read-only and best-effort.
///
/// Returns `None` for a packaged install with no source tree. This is never a reason to stop: it exists
/// so `doctor` can show `0.0.31` next to `0.0.12` and let the user see that the marker and the code
/// disagree, which is the state where a migration is genuinely dangerous.
pub fn python_release(install_root: &Path) -> Option<String> {
    let file = install_root.join("windrecorder").join("__init__.py");
    let text = std::fs::read_to_string(&file).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("__version__") {
            if let Some(quote) = rest.find(['\'', '"']) {
                let marker = rest.as_bytes()[quote] as char;
                let start = quote + 1;
                if let Some(end) = rest[start..].find(marker) {
                    return Some(rest[start..start + end].to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> (PathBuf, Config) {
        let dir = std::env::temp_dir().join(format!("wind-setup-marker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        let config = Config::load(&dir).unwrap();
        (dir, config)
    }

    fn marker(plan: &str) -> Marker {
        let mut steps = BTreeMap::new();
        steps.insert(
            "legacy-layout-0.0.9".to_string(),
            StepRecord { done_at: "2026-09-23_01-02-03".into(), fingerprint: "aaa".into(), notes: vec!["moved videos".into()] },
        );
        Marker {
            migrated_to: LATEST_KNOWN_RELEASE.into(),
            writer: env!("CARGO_PKG_VERSION").into(),
            updated_at: "2026-09-23_01-02-03".into(),
            plan_hash: plan.into(),
            steps,
        }
    }

    #[test]
    fn a_marker_round_trips_through_disk() {
        let (root, config) = tree("round-trip");
        let original = marker(&plan_hash(&[("a".into(), "0.0.9".into(), "1".into())]));
        let path = original.save(&config).unwrap();
        assert_eq!(path, marker_path(&config));
        let loaded = Marker::load(&config).expect("reads back");
        assert_eq!(loaded, original);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The failure this guards: a marker that half-exists after a power cut must not be read as
    /// "everything is done", because that is the one misreport that lets a migration skip its work.
    #[test]
    fn an_unreadable_marker_is_no_marker_rather_than_a_false_claim() {
        let (root, config) = tree("corrupt");
        std::fs::create_dir_all(config.userdata_dir()).unwrap();
        for junk in ["", "{", "{\"steps\": null}", "not json at all", "{\"steps\": {\"x\": {}}"] {
            std::fs::write(marker_path(&config), junk).unwrap();
            assert!(Marker::load(&config).is_none(), "{junk:?} must not parse as a migration record");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saving_leaves_no_staging_file_behind() {
        let (root, config) = tree("atomic");
        marker("h").save(&config).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(config.userdata_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".stage"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_plan_hash_depends_on_everything_that_changes_the_work() {
        let base = vec![("a".to_string(), "0.0.9".to_string(), "1".to_string()), ("b".to_string(), "0.0.12".to_string(), "2".to_string())];
        let hash = plan_hash(&base);
        assert_eq!(hash, plan_hash(&base), "the same plan hashes the same, run after run");

        let mut longer = base.clone();
        longer.push(("c".to_string(), "0.0.20".to_string(), "3".to_string()));
        assert_ne!(hash, plan_hash(&longer), "one more step is a different plan");

        let mut reordered: Vec<(String, String, String)> = base.clone();
        reordered.reverse();
        assert_ne!(hash, plan_hash(&reordered), "migration order is part of the contract");

        let mut changed_input = base.clone();
        changed_input[0].2 = "1b".to_string();
        assert_ne!(hash, plan_hash(&changed_input), "a file that appeared since is a different install");
    }

    #[test]
    fn a_marker_explains_itself_on_disk() {
        let (root, config) = tree("documented");
        let text = marker("h").to_json();
        assert!(text.contains("read_me"), "the file must say what deleting it does");
        assert!(text.contains("legacy-layout-0.0.9"));
        assert!(text.contains("moved videos"));
        // A marker written for one tree is readable by that tree's own loader, which is the round trip
        // `doctor` depends on.
        assert_eq!(Marker::load(&config), None, "a marker that was never saved is not a migration record");
        let saved = marker("h");
        saved.save(&config).unwrap();
        assert_eq!(Marker::load(&config).unwrap(), saved);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The reader works, so `doctor`'s version line cannot silently become "unknown" forever.
    ///
    /// This used to read the real `windrecorder/__init__.py` beside this repo, which was a weak test
    /// for the stated purpose: it proved the file happened to be parseable today, and it broke the
    /// moment the Python tree was deleted -- which is the direction of travel, not an accident. It now
    /// writes the line it wants to see and reads it back, so the assertion is about the reader rather
    /// than about what is currently checked out.
    #[test]
    fn the_python_release_line_is_readable_when_a_tree_declares_one() {
        let dir = std::env::temp_dir().join(format!("windsetup-pythonrelease-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("windrecorder")).unwrap();
        std::fs::write(dir.join("windrecorder").join("__init__.py"), "__version__ = '0.0.31'\n").unwrap();
        assert_eq!(python_release(&dir).as_deref(), Some("0.0.31"));

        // The forms a real `__init__.py` actually comes in, since a reader that only handles one
        // spelling reports "unknown" for the others and that looks identical to having no tree at all.
        std::fs::write(dir.join("windrecorder").join("__init__.py"), "__version__ = \"9.9.9\"\n").unwrap();
        assert_eq!(python_release(&dir).as_deref(), Some("9.9.9"));
        std::fs::write(dir.join("windrecorder").join("__init__.py"), "  __version__ = '1.2.3'  # trailing\n").unwrap();
        assert_eq!(python_release(&dir).as_deref(), Some("1.2.3"));

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(python_release(&dir.join("nope")), None, "a gone tree reports nothing, not a guess");
    }
}
