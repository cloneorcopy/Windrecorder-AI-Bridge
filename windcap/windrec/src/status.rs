//! `windrec status` — is it working, what is indexed, and what is stuck?
//!
//! The tray can say whether it holds the record lock and the UI can count rows, but neither can
//! answer the question a user actually asks on a machine that has been running for a month: is
//! recording happening, how current is the index, and are slices piling up unconverted. That last
//! one is the failure mode this rewrite has to make visible, because the maintenance pass only runs
//! while the screen is idle — a user who leaves their workstation locked every evening converts
//! nothing, and the frames sit there invisible to search.

use std::path::Path;
use std::time::Duration;

use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::fslock::{lock_state, LockState};
use wind_base::paths;
use wind_store::read;

use wind_base::paths::SUBMIT_MARKER_DIR as SUBMIT_MARKER;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SliceCounts {
    /// Directories the recorder filled but nothing has committed or converted yet.
    pub awaiting_maintenance: usize,
    /// Rows are in the index; the video does not exist yet.
    pub awaiting_conversion: usize,
    pub finished: usize,
    pub discarded: usize,
}

/// Tell the slice directories apart by their marker rather than by guesswork.
///
/// `submitted` is the directory's own name only: a committed slice is marked by a nested `-SUBMIT`
/// directory (see `Segment::submit_marker`), so that one needs a filesystem check and is passed in
/// by the caller rather than inferred from the name.
///
/// Three counts instead of one because each means something different: an unmarked directory is
/// frames the recorder wrote, a `-SUBMIT` directory is a segment whose rows are already searchable
/// but whose video is not — the state in which a search result points at a file that cannot be
/// played — and `-VIDEO` is finished. Anything that does not parse as a timestamped directory is not
/// one of ours and is ignored rather than counted.
pub fn classify_slices(
    entries: impl Iterator<Item = (String, bool)>,
) -> SliceCounts {
    let mut out = SliceCounts::default();
    for (name, submitted) in entries {
        // Name markers win over the nested one, because the nested marker survives the conversion
        // rename: `windmaint` renames the directory to `{stamp}-VIDEO` and the `-SUBMIT` directory it
        // read is still inside it. Checking the marker first counted every converted slice as work
        // still to do, which is the one thing this report must not lie about.
        if name.contains(paths::MARKER_VIDEO) || name.contains(paths::MARKER_OCRED) {
            out.finished += 1;
        } else if name.contains(paths::MARKER_DISCARD) {
            out.discarded += 1;
        } else if submitted {
            out.awaiting_conversion += 1;
        } else if LocalParts::from_stamp(&name).is_some() {
            out.awaiting_maintenance += 1;
        }
    }
    out
}

pub fn report(config: &Config) -> Result<(), String> {
    report_recorder(config);
    report_index(config);
    report_slices(config);
    Ok(())
}

fn report_recorder(config: &Config) {
    let lock = config.record_lock_path();
    match lock_state(&lock) {
        LockState::HeldBy { pid, alive: true } => {
            println!("recorder        RUNNING (pid {pid})  lock {lock}", lock = lock.display());
        }
        LockState::HeldBy { pid, alive: false } => {
            println!(
                "recorder        not running; stale lock from dead pid {pid} at {lock}",
                lock = lock.display()
            );
        }
        LockState::Owned => println!("recorder        RUNNING in this process"),
        LockState::Unreadable => {
            println!("recorder        lock exists but names no pid: {lock}", lock = lock.display());
        }
        LockState::Free => println!("recorder        not running (no lock)"),
    }
}

fn report_index(config: &Config) {
    let months = read::discover(&config.db_dir());
    println!(
        "index           {} month file(s) in {dir}",
        months.len(),
        dir = config.db_dir().display()
    );
    let mut total = 0i64;
    let mut newest: Option<i64> = None;
    for month in &months {
        // Read through the temp copy even for a status line: `status` is run while the recorder is
        // running, and a read lock held on the live file is how a segment commit gets delayed.
        let conn = match month.open_read(Duration::from_secs(300), false) {
            Ok(conn) => conn,
            Err(e) => {
                println!("  {file} unreadable: {e}", file = month.path.display());
                continue;
            }
        };
        let rows = read::count_rows(&conn).unwrap_or(0);
        let bounds = read::time_bounds(&conn).unwrap_or(None);
        total += rows;
        if let Some((_, last)) = bounds {
            newest = Some(newest.map_or(last, |current: i64| current.max(last)));
        }
        println!(
            "  {year:04}-{month:02}  {rows:>6} rows  {span}",
            year = month.year,
            month = month.month,
            span = bounds.map_or_else(
                || "empty".to_string(),
                |(first, last)| format!(
                    "{first} .. {last}",
                    first = LocalParts::from_naive_epoch(first).display(),
                    last = LocalParts::from_naive_epoch(last).display()
                )
            )
        );
    }
    match newest {
        Some(at) => println!("indexed through   {when}", when = LocalParts::from_naive_epoch(at).display()),
        None => println!("indexed through   nothing yet"),
    }
    println!("rows total      {total}");
}

fn report_slices(config: &Config) {
    let cache = config.cache_screenshot_dir();
    let entries = std::fs::read_dir(&cache)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    let name = entry.file_name().into_string().ok()?;
                    if !path.is_dir() {
                        return None;
                    }
                    let submitted = path.join(SUBMIT_MARKER).is_dir();
                    Some((name, submitted))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let counts = classify_slices(entries.into_iter());
    println!(
        "slices          {pending} awaiting maintenance, {convert} committed awaiting conversion, {done} converted, {gone} discarded  {dir}",
        pending = counts.awaiting_maintenance,
        convert = counts.awaiting_conversion,
        done = counts.finished,
        gone = counts.discarded,
        dir = cache.display()
    );
    // `awaiting maintenance` cannot say which of those directories a killed instance left and which a
    // live one is filling; a journal can, because it names its writer's pid. Printed separately for
    // that reason, and worded as a promise the next start keeps only if the writer really is gone.
    let stranded = stranded_slices(&cache);
    if !stranded.is_empty() {
        let mut shown = stranded.iter().take(3).cloned().collect::<Vec<_>>();
        let hidden = stranded.len() - shown.len();
        if hidden > 0 {
            shown.push(format!("+{hidden} more"));
        }
        println!(
            "journals        {n} segment(s) hold a write-ahead journal, which the next recorder start replays: {list}",
            n = stranded.len(),
            list = shown.join(", "),
        );
    }
    if counts.awaiting_conversion + counts.awaiting_maintenance > 0 {
        println!("                run `windmaint all` — it converts and prunes only while the screen is idle");
    }
}

/// Which slice directories still hold a write-ahead journal, oldest first.
///
/// The discovery rule is the sweep's own, not a second opinion on it: a directory whose name parses
/// as a segment stamp, carries no pipeline marker, and contains a `-JOURNAL*` file. A journal that
/// belongs to a still-running writer is listed too — this reports what is on disk, and the claim rule
/// in `recorder::sweep_stranded` is what decides whether it may be touched.
pub fn stranded_slices(cache: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(cache) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else { continue };
        if paths::has_segment_marker(&name) || LocalParts::from_stamp(&name).is_none() {
            continue;
        }
        if !crate::segment::journal_paths(&path).is_empty() {
            out.push(name);
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(names: &[&str]) -> SliceCounts {
        classify_slices(names.iter().map(|s| (s.to_string(), false)))
    }

    fn marked(name: &str) -> SliceCounts {
        classify_slices(std::iter::once((name.to_string(), true)))
    }

    #[test]
    fn slice_directories_are_told_apart_by_their_marker_not_by_guesswork() {
        let got = counts(&[
            "2026-09-21_10-00-00",
            "2026-09-21_10-15-00",
            "2026-09-21_10-30-00-VIDEO",
            "2026-09-21_10-45-00-SCREENSHOTS-OCRED",
            "2026-09-21_11-00-00-DISCARD",
            "not-a-slice",
            ".hidden",
        ]);
        assert_eq!(
            got,
            SliceCounts {
                awaiting_maintenance: 2,
                awaiting_conversion: 0,
                finished: 2,
                discarded: 1,
            }
        );
    }

    #[test]
    fn a_journal_is_what_tells_a_stranded_directory_apart_from_a_finished_one() {
        let cache = std::env::temp_dir().join(format!("windcap-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let journal = |name: &str| {
            let dir = cache.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(crate::segment::JOURNAL_FILE), b"junk, not ours to parse").unwrap();
        };
        journal("2026-09-21_10-00-00");
        journal("2026-09-21_09-00-00");
        // Already converted, or discarded: the sweep will not reopen either decision, so a status line
        // that counted them would be promising work nobody is going to do.
        journal("2026-09-20_09-00-00-VIDEO");
        journal("2026-09-20_08-00-00-DISCARD");
        // A journal renamed to a claimed name is still a journal, and still pending.
        journal("2026-09-19_09-00-00");
        std::fs::rename(
            cache.join("2026-09-19_09-00-00").join(crate::segment::JOURNAL_FILE),
            cache.join("2026-09-19_09-00-00").join(crate::segment::claimed_journal_name(4_000_000)),
        )
        .unwrap();
        let quiet = cache.join("2026-09-18_09-00-00");
        std::fs::create_dir_all(quiet.join(crate::segment::SUBMIT_MARKER)).unwrap();
        std::fs::write(quiet.join("2026-09-18_09-00-05.jpg"), b"j").unwrap();
        std::fs::create_dir_all(cache.join("not-a-slice")).unwrap();
        std::fs::write(cache.join("stray.txt"), b"x").unwrap();

        assert_eq!(
            stranded_slices(&cache),
            vec![
                "2026-09-19_09-00-00".to_string(),
                "2026-09-21_09-00-00".to_string(),
                "2026-09-21_10-00-00".to_string(),
            ],
            "oldest first, journals only, nothing already settled"
        );
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn an_empty_cache_reports_zeroes_rather_than_failing() {
        assert_eq!(counts(&[]), SliceCounts::default());
    }

    /// The whole point of the nested marker: a slice whose rows are committed must still be
    /// discoverable as work, and must be reported as the state where a search result has no video.
    #[test]
    fn a_submitted_slice_is_counted_as_work_not_as_finished() {
        let got = marked("2026-09-21_10-00-00");
        assert_eq!(got.awaiting_conversion, 1);
        assert_eq!(got.awaiting_maintenance, 0);
        assert_eq!(got.finished, 0);
    }

    /// The state a slice is really left in after conversion: renamed by the outside, still carrying
    /// the marker it was submitted with on the inside. Reported as work-to-do here and the report
    /// would be wrong forever — which is exactly what it was before the name markers were checked
    /// first.
    #[test]
    fn a_converted_slice_keeps_its_submit_marker_and_is_still_counted_as_finished() {
        let got = classify_slices(std::iter::once(("2026-09-22_21-30-00-VIDEO".to_string(), true)));
        assert_eq!(got.finished, 1);
        assert_eq!(got.awaiting_conversion, 0);
    }

    #[test]
    fn a_marker_in_the_middle_still_counts_as_finished() {
        let got = counts(&["2026-09-21_10-00-00-VIDEO-SCREENSHOTS-OCRED"]);
        assert_eq!(got.finished, 1, "a directory carries both markers once conversion runs");
    }
}
