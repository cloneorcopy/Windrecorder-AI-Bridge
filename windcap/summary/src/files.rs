//! The two directories, the day files, and the only write paths into them.
//!
//! # Shape
//!
//! ```text
//!   userdata/result_ai_period_summary/2026-09-27.json   {"2026-09-27_15-47-17": {…PeriodSummary}}
//!   userdata/result_ai_daily_summary/2026-09-27.json    {…DaySummary}
//! ```
//!
//! A period file is a key map, so one stretch's paragraph is added without touching its neighbours';
//! a daily file is a single object, because a day has exactly one of them. Two shapes, one set of
//! rules: the filename is the product day, keys sort on the way out, and every write is a temp file
//! renamed into place.
//!
//! # Why a merge and not an overwrite
//!
//! `save_dict_as_json_to_path`'s habit is what an install may already hold in these directories, so
//! this product and whatever the user ran before it read each other's files. Read-merge-write is also
//! the only posture in which a day file's *untouched* neighbours survive: an overwrite would need the
//! caller to hand back the whole day, and a caller that forgot one key would delete a paragraph of
//! somebody's history.
//!
//! # Two locks, because there are two ways to collide
//!
//! The bridge is a resident HTTP service: two agents can summarise the same day at the same second
//! inside *one* process, where a pid-named lock file is no help — [`wind_base::fslock::PidLock`] reads
//! its own pid as `Owned` and hands it out again. So [`gate`] takes a process-local mutex for the
//! thread story and the pid lock for the cross-process story (`windai`, the idle pass, a second bridge)
//! together, every single write. A lost update here is not hypothetical: it is the normal outcome of two
//! producers both deciding today needs summarising.
//!
//! The lock file lives beside `LOCK_MAINTAIN`, never in it. `Config::maintain_lock_claimed` reads the pid
//! inside `LOCK_MAINTAIN` specifically — since c7a6154 an empty container there no longer reads as a
//! maintenance run — and a sibling of our own cannot be mistaken for it either way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use wind_base::clock::{self, LocalParts};
use wind_base::config::Config;
use wind_base::fslock::PidLock;

use crate::entries::{DaySummary, PeriodSummary};
use crate::error::SummaryError;
use crate::keys;

/// `userdata/result_ai_period_summary`. Fixed rather than a config key: a directory the product reads
/// back is not a preference, and a setting that can be pointed at the videos folder is a footgun.
pub const PERIOD_DIR: &str = "result_ai_period_summary";
/// `userdata/result_ai_daily_summary`.
pub const DAILY_DIR: &str = "result_ai_daily_summary";
/// `cache/locks/LOCK_AI_SUMMARY`.
pub const LOCK_NAME: &str = "LOCK_AI_SUMMARY";

/// Which of the two artefact families a call is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Period,
    Daily,
}

impl Kind {
    pub fn dir_name(self) -> &'static str {
        match self {
            Kind::Period => PERIOD_DIR,
            Kind::Daily => DAILY_DIR,
        }
    }

    /// What a payload calls it, so a reader can be told which of the two answered.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Period => "period",
            Kind::Daily => "daily",
        }
    }
}

static WRITE_GATE: Mutex<()> = Mutex::new(());

/// The directory a family lives in.
pub fn dir(config: &Config, kind: Kind) -> PathBuf {
    config.userdata_dir().join(kind.dir_name())
}

/// `<dir>/<day>.json`, the one file that holds one product day.
pub fn day_path(config: &Config, kind: Kind, day: &str) -> PathBuf {
    dir(config, kind).join(format!("{day}.json"))
}

/// What a read of one day's period file found, in the states a caller has to tell apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayMap {
    pub path: PathBuf,
    /// Something is at that path.
    pub exists: bool,
    /// ...and it parsed as a map of entries. A file that exists and will not parse is *not* "no
    /// summaries" — it is an unknown answer, and the difference is the whole reason this is a struct.
    pub readable: bool,
    pub entries: BTreeMap<String, PeriodSummary>,
    /// Why it is missing or unreadable, phrased for a payload.
    pub note: String,
}

impl DayMap {
    /// Nothing has ever been written for this day.
    pub fn absent(&self) -> bool {
        !self.exists
    }

    pub fn get(&self, key: &str) -> Option<&PeriodSummary> {
        self.entries.get(key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One day's daily summary, in the same states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyFile {
    pub path: PathBuf,
    pub exists: bool,
    pub readable: bool,
    pub summary: Option<DaySummary>,
    pub note: String,
}

impl DailyFile {
    pub fn absent(&self) -> bool {
        !self.exists
    }
}

/// Read one day's period entries. Never fails: absence and corruption are states to report, not errors
/// to raise, because every caller here has to answer "is this day done?" and "I could not tell" is one
/// of the answers.
pub fn read_period(config: &Config, day: &str) -> DayMap {
    read_map_at(&day_path(config, Kind::Period, day))
}

/// Read one day's daily summary.
///
/// Parsed as a [`DaySummary`] and not as a key map: a day's file *is* the object, so running it through
/// the period reader first would report every valid daily summary as "not a summary file" — which is the
/// bug this comment exists to keep somebody from reintroducing.
pub fn read_daily(config: &Config, day: &str) -> DailyFile {
    let path = day_path(config, Kind::Daily, day);
    let failed = |note: String, exists: bool| DailyFile { path: path.clone(), exists, readable: false, summary: None, note };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return failed(format!("{} does not exist", shown(&path)), false),
        Err(e) => return failed(format!("{} cannot be read: {e}", shown(&path)), true),
    };
    if text.trim().is_empty() {
        return failed(format!("{} is empty", shown(&path)), true);
    }
    match serde_json::from_str::<DaySummary>(&text) {
        Ok(summary) => DailyFile { path, exists: true, readable: true, summary: Some(summary), note: String::new() },
        Err(e) => failed(format!("{} is not a day summary: {e}", shown(&path)), true),
    }
}

fn read_map_at(path: &Path) -> DayMap {
    let absent = || DayMap { path: path.to_path_buf(), exists: false, readable: false, entries: BTreeMap::new(), note: format!("{} does not exist", shown(path)) };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return absent(),
        Err(e) => {
            return DayMap { path: path.to_path_buf(), exists: true, readable: false, entries: BTreeMap::new(), note: format!("{} cannot be read: {e}", shown(path)) }
        }
    };
    if text.trim().is_empty() {
        // Blank is an empty day, not a corrupt one — the reading `Runtime::ai_cache_at` already gives.
        return DayMap { path: path.to_path_buf(), exists: true, readable: true, entries: BTreeMap::new(), note: format!("{} is empty", shown(path)) };
    }
    match serde_json::from_str::<BTreeMap<String, PeriodSummary>>(&text) {
        Ok(entries) => DayMap { path: path.to_path_buf(), exists: true, readable: true, entries, note: String::new() },
        Err(e) => DayMap { path: path.to_path_buf(), exists: true, readable: false, entries: BTreeMap::new(), note: format!("{} is not a summary file: {e}", shown(path)) },
    }
}

/// A path as this service prints it: `result_ai_period_summary/2026-09-27.json` rather than a
/// ninety-character absolute path an agent will then quote back at a user who cannot use it.
fn shown(path: &Path) -> String {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    let folder = path.parent().and_then(|parent| parent.file_name()).and_then(|s| s.to_str()).unwrap_or("");
    if folder.is_empty() {
        name.to_string()
    } else {
        format!("{folder}/{name}")
    }
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    pub path: PathBuf,
    /// Something was already there, so this write replaced a paragraph.
    pub replaced: bool,
    /// How many entries the day holds now (a daily file is always one).
    pub entries_after: usize,
}

/// Upsert one stretch's summary into its day file.
///
/// The caller names both the day and the key, and a mismatch between them is refused rather than
/// resolved by preferring one: silently filing a paragraph under a different day than its key says is
/// how a gate passes over a hole nobody can find afterwards.
pub fn write_period(config: &Config, day: &str, key: &str, summary: &PeriodSummary) -> Result<WriteOutcome, SummaryError> {
    keys::parse_day(day).ok_or_else(|| SummaryError::BadDate(day.to_string()))?;
    let owner = keys::day_of(summary.start, config.day_begin_minutes());
    if summary.start > 0 && owner != day {
        return Err(SummaryError::BadReference(format!(
            "day {day} does not own this segment's start: {} falls in {owner}",
            LocalParts::from_naive_epoch(summary.start).display()
        )));
    }
    let path = day_path(config, Kind::Period, day);
    let _guards = gate(config)?;
    let current = read_map_at(&path);
    if current.exists && !current.readable {
        return Err(SummaryError::Unreadable { path: path.clone(), why: current.note });
    }
    let replaced = current.entries.contains_key(key);
    let mut entries = current.entries;
    entries.insert(key.to_string(), summary.clone());
    let entries_after = entries.len();
    write_map(&path, &entries)?;
    Ok(WriteOutcome { path, replaced, entries_after })
}

/// Replace one day's daily summary.
pub fn write_daily(config: &Config, summary: &DaySummary) -> Result<WriteOutcome, SummaryError> {
    keys::parse_day(&summary.date).ok_or_else(|| SummaryError::BadDate(summary.date.clone()))?;
    let path = day_path(config, Kind::Daily, &summary.date);
    let _guards = gate(config)?;
    let current = read_daily(config, &summary.date);
    if current.exists && !current.readable && current.summary.is_none() {
        // A file we cannot read as a day summary is not ours to overwrite — unless it is the blank the
        // layout pass creates, which is an empty answer rather than a foreign one.
        let blank = std::fs::read_to_string(&path).map(|text| text.trim().is_empty()).unwrap_or(false);
        if !blank {
            return Err(SummaryError::Unreadable { path: path.clone(), why: current.note });
        }
    }
    let replaced = current.exists;
    write_json(&path, summary)?;
    Ok(WriteOutcome { path, replaced, entries_after: 1 })
}

/// Drop the named stretches from a day, taking the file with them once it holds nothing.
///
/// This is the door `windmaint forget` and `expire` walk through: text derived from rows that are being
/// erased has to go with them, or the erasure is a rename.
pub fn prune_period(config: &Config, day: &str, keys: &[String]) -> Result<usize, SummaryError> {
    let path = day_path(config, Kind::Period, day);
    let _guards = gate(config)?;
    let current = read_map_at(&path);
    if !current.exists {
        return Ok(0);
    }
    if !current.readable {
        return Err(SummaryError::Unreadable { path: path.clone(), why: current.note });
    }
    let mut entries = current.entries;
    let before = entries.len();
    for key in keys {
        entries.remove(key);
    }
    let removed = before - entries.len();
    if entries.is_empty() {
        // An empty day is no file: `{}` on disk reads as "somebody looked at this day and wrote
        // nothing", which is a claim nobody made.
        if removed > 0 {
            std::fs::remove_file(&path)?;
        }
    } else {
        write_map(&path, &entries)?;
    }
    Ok(removed)
}

/// Remove one day's daily summary. Returns whether a file was there to remove.
pub fn prune_daily(config: &Config, day: &str) -> Result<bool, SummaryError> {
    let path = day_path(config, Kind::Daily, day);
    let _guards = gate(config)?;
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

/// Flag one day's summary as describing inputs that have since left the library.
///
/// It writes the `stale` field and touches nothing else. A paragraph that stopped matching its sources
/// must not be quietly regenerated, reworded, or deleted here: the user reads it, decides, asks again.
pub fn mark_daily_stale(config: &Config, day: &str) -> Result<bool, SummaryError> {
    let _guards = gate(config)?;
    let file = read_daily(config, day);
    let Some(mut summary) = file.summary else { return Ok(false) };
    if summary.stale {
        return Ok(false);
    }
    summary.stale = true;
    let path = file.path.clone();
    write_json(&path, &summary)?;
    Ok(true)
}

/// Every day a family holds a file for, oldest first. Files whose names are not real days are ignored
/// rather than treated as data.
pub fn days_present(config: &Config, kind: Kind) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir(config, kind)) else { return Vec::new() };
    let mut out: Vec<String> = read
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".json").map(str::to_string))
        .filter(|day| keys::parse_day(day).is_some())
        .collect();
    out.sort();
    out
}

/// The days a range covers that actually have files, in order.
pub fn days_in_range(config: &Config, kind: Kind, from_day: &str, to_day: &str) -> Vec<String> {
    let wanted = keys::days_in_range(from_day, to_day);
    days_present(config, kind).into_iter().filter(|day| wanted.iter().any(|want| want == day)).collect()
}

/// Read every period file a range touches, oldest day first.
pub fn read_period_range(config: &Config, from_day: &str, to_day: &str) -> Vec<(String, DayMap)> {
    days_in_range(config, Kind::Period, from_day, to_day).into_iter().map(|day| {
        let path = day_path(config, Kind::Period, &day);
        let map = read_map_at(&path);
        (day, DayMap { path, ..map })
    }).collect()
}

/// Read every daily summary a range touches, oldest day first.
pub fn read_daily_range(config: &Config, from_day: &str, to_day: &str) -> Vec<(String, DailyFile)> {
    days_in_range(config, Kind::Daily, from_day, to_day).into_iter().map(|day| (day.clone(), read_daily(config, &day))).collect()
}

/// The newest day file a family holds, for `status`, which has to say how fresh the caches are.
pub fn newest_file(config: &Config, kind: Kind) -> Option<PathBuf> {
    days_present(config, kind).last().map(|day| day_path(config, kind, &day))
}

/// Both gates for the duration of one read-merge-write. See the module header for why there are two.
fn gate(config: &Config) -> Result<Guards, SummaryError> {
    let thread = WRITE_GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // A panicked holder left the day files intact — the write is a rename — so recovering the mutex is
    // safe. What must not happen is the thread gate dropping before the process one is taken.
    let process = PidLock::acquire(&config.lock_dir().join(LOCK_NAME)).map_err(SummaryError::Locked)?;
    Ok(Guards { _process: process, _thread: thread })
}

/// Both gates, in the order they are given back.
///
/// Field order *is* the release order — Rust drops in declaration order — and the process gate has to go
/// first. Releasing the mutex first let the next writer in this process reach [`PidLock::acquire`] while
/// this one was still deleting the lock file, and it read a file that was being created or removed:
/// either "exists but names no pid", or a share violation on `create_new`. Both arrive as a refused
/// write, which is what made `two_threads_writing_one_day_keep_both_paragraphs` fail one run in eight.
/// Nothing was ever lost — the day file is only ever replaced by its own read-merge-write, under both
/// gates — but a writer that refuses the lock is a worse report than one that waits.
struct Guards {
    _process: PidLock,
    _thread: std::sync::MutexGuard<'static, ()>,
}

/// The cross-process gate on its own, for a caller that must hold it across many writes — the
/// `windmaint forget` pass prunes several days and must not be interleaved with a bridge write in any
/// of them. A single [`write_period`] or [`prune_period`] takes this itself; taking it twice in one
/// process is harmless (`PidLock` reads its own pid as ours), so callers need not reason about it.
pub struct FileLock {
    _lock: PidLock,
}

pub fn lock(config: &Config) -> Result<FileLock, SummaryError> {
    Ok(FileLock { _lock: PidLock::acquire(&config.lock_dir().join(LOCK_NAME)).map_err(SummaryError::Locked)? })
}

/// The wall clock as entries store it. A string, because a person opens these files.
pub fn now_stamp() -> String {
    clock::now().display()
}

fn write_map(path: &Path, entries: &BTreeMap<String, PeriodSummary>) -> Result<(), SummaryError> {
    write_json(path, entries)
}

/// Serialize, write to `*.json.tmp`, rename into place.
///
/// `to_string_pretty` over a sorted map is deliberate: that is the byte shape Python's
/// `save_dict_as_json_to_path` produces, so a file this wrote and one the old app wrote are
/// interchangeable, and a diff of a day file shows the one paragraph that changed.
fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), SummaryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let staging = staging_path(path);
    std::fs::write(&staging, &text)?;
    // Renaming over an existing target is the atomic step; on Windows it needs the destination gone
    // first, so a crash between the two leaves *no* file rather than half of one — which every reader
    // here reports as "absent", an honest state, instead of as "unreadable".
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&staging, path).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        SummaryError::Io(e)
    })
}

fn staging_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::Coverage;
    use crate::test_support as support;

    const KEY: &str = "2026-09-27_15-47-17";

    fn summary(text: &str, start: i64) -> PeriodSummary {
        PeriodSummary {
            text: text.into(),
            start,
            end: start + 60,
            frames: 4,
            ocr_chars: 100,
            written_at: support::STAMP.into(),
            written_by: "test".into(),
            model: String::new(),
            source_fingerprint: "aaaa".into(),
            prompt_fingerprint: String::new(),
        }
    }

    fn day(text: &str) -> DaySummary {
        DaySummary {
            date: "2026-09-27".into(),
            text: text.into(),
            coverage: Coverage { segments_total: 3, segments_summarised: 3, missing: vec![] },
            partial: false,
            written_at: support::STAMP.into(),
            written_by: "windai".into(),
            model: "m".into(),
            source_fingerprint: "s".into(),
            stale: false,
            prompt_fingerprint: String::new(),
        }
    }

    #[test]
    fn an_absent_day_says_absent_and_not_empty() {
        let root = support::install("files-absent");
        let config = support::config_at(&root);
        let read = read_period(&config, "2026-09-27");
        assert!(read.absent());
        assert!(!read.readable);
        assert!(read.is_empty());
        assert!(read.note.contains("does not exist"), "{}", read.note);
        assert!(read.note.contains("result_ai_period_summary/"), "a short path a user can act on: {}", read.note);
        assert!(!dir(&config, Kind::Period).exists(), "reading must not create the directory");
        support::cleanup(&root);
    }

    #[test]
    fn a_write_creates_the_directory_and_names_the_file() {
        let root = support::install("files-write");
        let config = support::config_at(&root);
        let start = support::at("2026-09-27_15-47-17");
        let outcome = write_period(&config, "2026-09-27", KEY, &summary("第一段。", start)).expect("write");
        assert!(!outcome.replaced, "the first write replaces nothing");
        assert_eq!(outcome.entries_after, 1);
        assert!(outcome.path.ends_with(Path::new("result_ai_period_summary").join("2026-09-27.json")), "{}", outcome.path.display());
        let back = read_period(&config, "2026-09-27");
        assert_eq!(back.get(KEY).expect("entry").text, "第一段。", "the text comes back byte for byte");
        let raw = std::fs::read_to_string(&outcome.path).expect("readable");
        assert!(raw.contains('\n'), "pretty-printed, so a day file can be read in an editor");
        assert!(!back.get(KEY).expect("entry").written_by.is_empty());
        support::cleanup(&root);
    }

    /// The failure the merge exists to prevent.
    #[test]
    fn writing_one_stretch_leaves_its_neighbours_alone() {
        let root = support::install("files-merge");
        let config = support::config_at(&root);
        write_period(&config, "2026-09-27", "2026-09-27_10-00-00", &summary("morning", support::at("2026-09-27_10-00-00"))).expect("one");
        let second = write_period(&config, "2026-09-27", "2026-09-27_11-00-00", &summary("noon", support::at("2026-09-27_11-00-00"))).expect("two");
        assert_eq!(second.entries_after, 2);
        let day = read_period(&config, "2026-09-27");
        assert_eq!(day.get("2026-09-27_10-00-00").expect("kept").text, "morning");
        assert_eq!(day.get("2026-09-27_11-00-00").expect("kept").text, "noon");
        let names: Vec<&String> = day.entries.keys().collect();
        assert_eq!(names, vec!["2026-09-27_10-00-00", "2026-09-27_11-00-00"], "sorted on the way out");
        support::cleanup(&root);
    }

    #[test]
    fn a_second_write_of_one_stretch_replaces_it_and_says_so() {
        let root = support::install("files-replace");
        let config = support::config_at(&root);
        let start = support::at("2026-09-27_15-47-17");
        write_period(&config, "2026-09-27", KEY, &summary("first", start)).expect("first");
        let again = write_period(&config, "2026-09-27", KEY, &summary("second", start)).expect("second");
        assert!(again.replaced, "an overwrite reports itself as one");
        assert_eq!(again.entries_after, 1, "and does not add a copy");
        assert_eq!(read_period(&config, "2026-09-27").get(KEY).expect("entry").text, "second");
        support::cleanup(&root);
    }

    #[test]
    fn a_torn_day_file_is_an_unknown_and_not_an_empty_answer() {
        let root = support::install("files-torn");
        let config = support::config_at(&root);
        let path = day_path(&config, Kind::Period, "2026-09-27");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"{ not json").expect("torn");
        let day = read_period(&config, "2026-09-27");
        assert!(day.exists && !day.readable && day.is_empty());
        assert!(day.note.contains("is not a summary file"), "{}", day.note);
        let error = write_period(&config, "2026-09-27", KEY, &summary("x", support::at("2026-09-27_15-47-17"))).expect_err("never merge into a file we cannot read");
        assert!(matches!(error, SummaryError::Unreadable { .. }), "{error:?}");
        assert_eq!(std::fs::read_to_string(&path).expect("still there"), "{ not json", "a refused write leaves the bytes it could not parse");
        support::cleanup(&root);
    }

    #[test]
    fn a_blank_file_is_an_empty_day_rather_than_a_corrupt_one() {
        let root = support::install("files-blank");
        let config = support::config_at(&root);
        let path = day_path(&config, Kind::Period, "2026-09-27");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"   \n").expect("blank");
        let day = read_period(&config, "2026-09-27");
        assert!(day.exists && day.readable && day.is_empty());
        write_period(&config, "2026-09-27", KEY, &summary("x", support::at("2026-09-27_15-47-17"))).expect("a blank file is overwritable");
        assert_eq!(read_period(&config, "2026-09-27").len(), 1);
        support::cleanup(&root);
    }

    #[test]
    fn a_day_and_a_key_that_disagree_are_refused() {
        let root = support::install("files-mismatch");
        let config = support::config_at(&root);
        let october = support::at("2026-10-05_09-00-00");
        let error = write_period(&config, "2026-09-27", "2026-10-05_09-00-00", &summary("x", october)).expect_err("a day that matches the key");
        assert!(matches!(error, SummaryError::BadReference(_)), "{error:?}");
        assert!(error.to_string().contains("2026-10-05"), "{error}");
        assert!(!day_path(&config, Kind::Period, "2026-09-27").exists(), "and nothing was written");
        assert!(write_period(&config, "not-a-day", KEY, &summary("x", support::at("2026-09-27_15-47-17"))).is_err());
        support::cleanup(&root);
    }

    #[test]
    fn a_day_outside_the_rollover_lands_in_the_day_that_owns_it() {
        // 02:00 is yesterday's product day at the shipped 03:00 start. A caller that guessed the calendar
        // date has to be refused rather than silently corrected, or the hole just moves somewhere else.
        let root = support::install("files-rollover");
        let config = support::config_at(&root);
        let early = support::at("2026-09-27_02-00-00");
        assert!(write_period(&config, "2026-09-27", "2026-09-27_02-00-00", &summary("x", early)).is_err(), "02:00 belongs to the 26th");
        write_period(&config, "2026-09-26", "2026-09-27_02-00-00", &summary("x", early)).expect("the 26th takes it");
        assert_eq!(read_period(&config, "2026-09-26").len(), 1);
        support::cleanup(&root);
    }

    #[test]
    fn pruning_named_stretches_leaves_the_rest_and_empties_the_file() {
        let root = support::install("files-prune");
        let config = support::config_at(&root);
        write_period(&config, "2026-09-27", "2026-09-27_10-00-00", &summary("a", support::at("2026-09-27_10-00-00"))).expect("one");
        write_period(&config, "2026-09-27", "2026-09-27_11-00-00", &summary("b", support::at("2026-09-27_11-00-00"))).expect("two");
        assert_eq!(prune_period(&config, "2026-09-27", &["2026-09-27_10-30-00".to_string()]).expect("no-op"), 0, "a key nobody holds is not an error");
        assert_eq!(prune_period(&config, "2026-09-27", &["2026-09-27_10-00-00".to_string()]).expect("one"), 1);
        assert_eq!(read_period(&config, "2026-09-27").len(), 1, "the other paragraph survived");
        assert_eq!(prune_period(&config, "2026-09-27", &["2026-09-27_11-00-00".to_string()]).expect("last"), 1);
        assert!(!day_path(&config, Kind::Period, "2026-09-27").exists(), "an empty day is no file");
        assert_eq!(prune_period(&config, "2026-09-26", &["whatever".to_string()]).expect("absent"), 0);
        support::cleanup(&root);
    }

    #[test]
    fn a_prune_refuses_a_file_it_cannot_read() {
        let root = support::install("files-prune-torn");
        let config = support::config_at(&root);
        let path = day_path(&config, Kind::Period, "2026-09-27");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"[1,2]").expect("an array is not a map");
        assert!(prune_period(&config, "2026-09-27", &["x".to_string()]).is_err(), "erasing must not guess at a file");
        assert!(path.exists(), "and it leaves the bytes for a human to look at");
        support::cleanup(&root);
    }

    #[test]
    fn a_daily_summary_round_trips_and_can_be_marked_stale_without_losing_its_text() {
        let root = support::install("files-daily");
        let config = support::config_at(&root);
        let first = day("一天。\n两行。");
        let outcome = write_daily(&config, &first).expect("write");
        assert!(!outcome.replaced);
        assert_eq!(read_daily(&config, "2026-09-27").summary.expect("parsed"), first, "newline and CJK both survive");
        assert!(mark_daily_stale(&config, "2026-09-27").expect("marked"));
        let after = read_daily(&config, "2026-09-27").summary.expect("still parses");
        assert!(after.stale);
        assert_eq!(after.text, first.text, "staling touches the flag and nothing else");
        assert!(!mark_daily_stale(&config, "2026-09-27").expect("already marked"), "reporting it twice is not a second change");
        assert!(prune_daily(&config, "2026-09-27").expect("removed"));
        assert!(!prune_daily(&config, "2026-09-27").expect("nothing to remove"));
        assert!(read_daily(&config, "2026-09-27").absent());
        support::cleanup(&root);
    }

    #[test]
    fn a_daily_write_replaces_and_reports_it() {
        let root = support::install("files-daily-replace");
        let config = support::config_at(&root);
        write_daily(&config, &day("first")).expect("one");
        let second = write_daily(&config, &day("second")).expect("two");
        assert!(second.replaced, "the one summary a day has was there already");
        assert_eq!(read_daily(&config, "2026-09-27").summary.expect("parsed").text, "second");
        support::cleanup(&root);
    }

    #[test]
    fn a_torn_daily_file_is_unreadable_rather_than_absent() {
        let root = support::install("files-daily-torn");
        let config = support::config_at(&root);
        let path = day_path(&config, Kind::Daily, "2026-09-27");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, br#"{"date": }"#).expect("torn");
        let read = read_daily(&config, "2026-09-27");
        assert!(read.exists, "there is a file");
        assert!(!read.readable, "and it is not a summary");
        assert!(read.summary.is_none());
        support::cleanup(&root);
    }

    #[test]
    fn range_reads_walk_the_days_in_order() {
        let root = support::install("files-range");
        let config = support::config_at(&root);
        for day_name in ["2026-09-26", "2026-09-27", "2026-10-01"] {
            let key = format!("{day_name}_10-00-00");
            write_period(&config, day_name, &key, &summary(day_name, support::at(&key))).expect("seed");
        }
        assert_eq!(days_present(&config, Kind::Period), vec!["2026-09-26", "2026-09-27", "2026-10-01"]);
        let spanned: Vec<String> = read_period_range(&config, "2026-09-26", "2026-09-27").into_iter().map(|(day, _)| day).collect();
        assert_eq!(spanned, vec!["2026-09-26", "2026-09-27"], "a month boundary is not a reason to stop");
        assert!(read_period_range(&config, "2026-09-28", "2026-09-30").is_empty(), "a gap yields nothing, not an error");
        assert!(days_in_range(&config, Kind::Period, "2026-09-27", "2026-09-26").is_empty(), "a reversed range is empty");
        assert_eq!(newest_file(&config, Kind::Period).expect("newest").display().to_string(), day_path(&config, Kind::Period, "2026-10-01").display().to_string());
        support::cleanup(&root);
    }

    #[test]
    fn a_stray_file_named_like_a_day_but_not_a_day_is_ignored() {
        let root = support::install("files-stray");
        let config = support::config_at(&root);
        let folder = dir(&config, Kind::Period);
        std::fs::create_dir_all(&folder).expect("dir");
        for stray in ["notes.json", "2026-13-45.json", "2026-09-27.json.tmp", "2026-09-27"] {
            std::fs::write(folder.join(stray), b"{}").expect("stray");
        }
        assert!(days_present(&config, Kind::Period).is_empty(), "a calendar-impossible name is not a day, and a missing extension is neither");
        support::cleanup(&root);
    }

    #[test]
    fn the_lock_is_taken_released_and_names_its_own_file() {
        let root = support::install("files-lock");
        let config = support::config_at(&root);
        {
            let _held = lock(&config).expect("acquired");
            assert!(config.lock_dir().join(LOCK_NAME).exists(), "the lock file is where the install keeps them");
            assert_ne!(LOCK_NAME, "LOCK_MAINTAIN", "and it is not the maintenance pass's");
            assert!(write_period(&config, "2026-09-27", KEY, &summary("x", support::at("2026-09-27_15-47-17"))).is_ok(), "a nested take in one process is not a deadlock");
        }
        assert!(!config.lock_dir().join(LOCK_NAME).exists(), "dropping releases it");
        assert!(lock(&config).is_ok(), "so the next writer gets in");
        support::cleanup(&root);
    }

    #[test]
    fn two_threads_writing_one_day_keep_both_paragraphs() {
        // The in-process half of the gate: a pid lock alone reports `Owned` for our own process and lets
        // both threads read the same day, so the last writer would win and one paragraph would be gone.
        let root = support::install("files-threads");
        let config = support::config_at(&root);
        let names: Vec<String> = (0..8).map(|i| format!("2026-09-27_10-00-{i:02}")).collect();
        let handles: Vec<_> = names
            .iter()
            .cloned()
            .map(|key| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let config = support::config_at(&root);
                    let start = support::at(&key);
                    write_period(&config, "2026-09-27", &key, &summary(&key, start))
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("no panic").expect("write");
        }
        let day = read_period(&config, "2026-09-27");
        assert_eq!(day.len(), 8, "eight threads, eight entries, none lost");
        for name in &names {
            assert!(day.get(name).is_some(), "{name} vanished");
        }
        support::cleanup(&root);
    }

    #[test]
    fn no_temp_file_is_left_beside_a_day() {
        let root = support::install("files-staging");
        let config = support::config_at(&root);
        let start = support::at("2026-09-27_15-47-17");
        write_period(&config, "2026-09-27", KEY, &summary("kept", start)).expect("first");
        write_period(&config, "2026-09-27", KEY, &summary("replaced", start)).expect("second");
        let path = day_path(&config, Kind::Period, "2026-09-27");
        assert!(!staging_path(&path).exists(), "the temp file is renamed away, not left beside the day");
        assert_eq!(std::fs::read_dir(dir(&config, Kind::Period)).expect("dir").count(), 1, "and nothing else is in there");
        assert_eq!(read_period(&config, "2026-09-27").get(KEY).expect("entry").text, "replaced");
        support::cleanup(&root);
    }
}
