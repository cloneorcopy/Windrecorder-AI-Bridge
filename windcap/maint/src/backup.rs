//! The index backup: a copy of every month file, and a bounded history of those copies.
//!
//! Upstream backs up inside the idle OCR routine, which means a user whose indexing never completes
//! never gets a backup at all. Here it is a command of its own, because the thing it protects — the
//! index that turns years of screens into a search — is the only part of the install that cannot be
//! regenerated from the videos.
//!
//! The two AI summary folders ride along for the same reason, and are the stronger case of it: a
//! paragraph an outside AI wrote over MCP has no generator at all, so `result_ai_period_summary/` and
//! `result_ai_daily_summary/` are text that leaves the product the moment its file is lost. Their
//! rotation is per day rather than per run — see [`prune_per_source`] for why the two rules cannot be
//! one.

use std::path::{Path, PathBuf};

use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::paths;
use wind_base::pool::{self, Duty};
use wind_summary as summary;

use crate::layout;
use crate::refresh;

/// How many copies survive of the things one pass backs up. Upstream's `keep_items_num`.
///
/// For the month files this is a ceiling on the *whole* folder, because every month is copied by every
/// run and they all carry the same stamp: [`prune`] keeps the newest `KEEP` names in the directory, so a
/// twelve-month install keeps one full generation and three strays of the previous one. The summaries
/// are rotated per day instead, in their own folders, because copying three hundred day files into a
/// fifteen-file ceiling would keep nothing worth restoring.
pub const KEEP: usize = 15;

/// `cache/db_backup`, beside the other regenerable caches rather than under `userdata/`.
pub fn backup_dir(config: &Config) -> PathBuf {
    config.cache_dir().join("db_backup")
}

/// `{base}_BACKUP_{stamp}.{ext}`, the name `backup_dbfile` writes.
///
/// The shape matters beyond tidiness: pruning reads the age back out of the name, and upstream's own
/// `_TEMP_READ` guard is what stops a disposable read copy being promoted into a backup of record.
/// The extension is whatever the source carries, because the same naming serves a `.db` month file and
/// a `.json` day summary — and a restore is a rename back, not a rename plus a guess.
pub fn backup_name(db: &Path, stamp: &str) -> Option<String> {
    let stem = db.file_stem()?.to_str()?;
    if stem.contains(paths::TEMP_READ_SUFFIX) {
        return None;
    }
    match db.extension().and_then(|e| e.to_str()) {
        Some(ext) => Some(format!("{stem}_BACKUP_{stamp}.{ext}")),
        None => Some(format!("{stem}_BACKUP_{stamp}")),
    }
}

/// The `{base}` half of a copy name — everything before the `_BACKUP_` that dates it.
pub fn backup_base_of(name: &str) -> Option<&str> {
    name.split_once("_BACKUP_").map(|(base, _)| base)
}

/// The instant a backup was taken, read back out of its name.
///
/// A name with no parseable stamp is not a backup this pass made, and must never be a candidate for
/// deletion.
pub fn backup_stamp_of(name: &str) -> Option<LocalParts> {
    let after = name.split_once("_BACKUP_")?.1;
    LocalParts::from_stamp(after)
}

/// Which of these backup names fall outside the newest `keep`.
///
/// Newest-first by stamp, ties broken by name so a run is reproducible; anything with no stamp in it is
/// left alone entirely rather than sorted to one end and deleted there.
///
/// One ceiling over the whole list, which is what the month folder wants (see [`KEEP`]): every month is
/// copied by every run, so the newest `keep` names are the newest generation of them. Applying this to
/// the summaries would keep fifteen copies out of three hundred day files, which is not a backup of
/// anything — hence [`prune_per_source`].
pub fn prune(names: &[String], keep: usize) -> Vec<String> {
    let mut dated: Vec<(LocalParts, &String)> = names.iter().filter_map(|n| backup_stamp_of(n).map(|s| (s, n))).collect();
    dated.sort_by(|a, b| b.0.naive_epoch_seconds().cmp(&a.0.naive_epoch_seconds()).then_with(|| a.1.cmp(b.1)));
    dated.into_iter().skip(keep).map(|(_, name)| name.clone()).collect()
}

/// Which of these names fall outside the newest `keep` *for the file each one came from*.
///
/// The summaries' rotation. A day's paragraph changes when somebody summarises that stretch again, which
/// is a different event from "a pass ran", and the two must not be priced the same way: under a global
/// ceiling, a January day file is deleted because March got summarised twice. Grouping by the base name
/// makes each day's history its own, bounded by the same `KEEP`, and leaves a name that cannot be dated
/// exactly where [`prune`] leaves one — untouched.
///
/// Output is sorted, so a report of what was pruned is the same list in the same order every run.
pub fn prune_per_source(names: &[String], keep: usize) -> Vec<String> {
    let mut groups: std::collections::BTreeMap<&str, Vec<(LocalParts, &String)>> = std::collections::BTreeMap::new();
    for name in names {
        let (Some(base), Some(stamp)) = (backup_base_of(name), backup_stamp_of(name)) else { continue };
        groups.entry(base).or_default().push((stamp, name));
    }
    let mut stale: Vec<String> = groups
        .into_values()
        .flat_map(|mut group| {
            group.sort_by(|a, b| b.0.naive_epoch_seconds().cmp(&a.0.naive_epoch_seconds()).then_with(|| a.1.cmp(b.1)));
            group.into_iter().skip(keep).map(|(_, name)| name.clone()).collect::<Vec<String>>()
        })
        .collect();
    stale.sort();
    stale
}

/// What a backup run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub months: usize,
    pub written: usize,
    pub pruned: usize,
    /// The two AI summary folders, which are the part no other command can rebuild.
    pub summaries: SummaryOutcome,
}

/// What one pass did to the AI summary folders.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SummaryOutcome {
    /// Day files it looked at, across both families.
    pub days: usize,
    /// Copies taken.
    pub copied: usize,
    /// Days whose newest copy already holds the same bytes, so nothing was written for them.
    pub unchanged: usize,
    /// Copies beyond the newest `KEEP` for the day they came from.
    pub pruned: usize,
}

/// One month file, copied — or planned, for a dry run — by a pool lane.
struct Copy {
    /// Rows read off the live file, which the report line leads with whether or not anything was written.
    rows: i64,
    source: PathBuf,
    target: PathBuf,
}

/// Read one month's row count and copy it into the backup folder.
///
/// Nothing here prints, and nothing here prunes. The report lines are emitted by [`run`] in item order
/// once the pool has come back, which is what keeps a parallel pass writing the same lines in the same
/// order as the serial one did; the prune reads the whole folder, so it has to wait for the last copy to
/// land. `Ok(None)` is a file this pass does not back up at all — a `_TEMP_READ` copy, whose name is not
/// a name for a backup — and it is not an error and not a month counted.
fn copy_one(month: &wind_store::read::Month, destination: &Path, stamp: &str, dry_run: bool) -> Result<Option<Copy>, String> {
    let Some(name) = backup_name(&month.path, stamp) else { return Ok(None) };
    let target = destination.join(&name);
    let rows = count_rows(&month.path)?;
    // A dry run plans and writes nothing — not even the folder the copies would have gone into.
    if dry_run {
        return Ok(Some(Copy { rows, source: month.path.clone(), target }));
    }
    std::fs::create_dir_all(destination).map_err(|e| format!("{}: {e}", destination.display()))?;
    // Distinct targets, since every month file's copy name carries that month's own stem; the shared
    // `create_dir_all` is idempotent, so lanes racing to make the folder is not a race.
    std::fs::copy(&month.path, &target).map_err(|e| format!("{}: {e}", month.path.display()))?;
    Ok(Some(Copy { rows, source: month.path.clone(), target }))
}

/// Copy every month file into the backup directory, then trim each month's history to [`KEEP`], and do
/// the same for the two AI summary folders — which is where [`backup_summaries`] lives, because one
/// command has to leave the install's whole unregenerable state backed up, not half of it.
///
/// The copies go out on pool lanes and come back in the order their months came, so the counters and the
/// report lines read exactly as the serial step's did; the prune waits for the last of them because it
/// reads the whole folder, and a copy still in flight would be a name the ceiling has not seen yet.
pub fn run(config: &Config, now: &LocalParts, dry_run: bool, limit: Option<usize>) -> Result<Outcome, String> {
    let months = wind_store::read::discover(&config.db_dir());
    let destination = backup_dir(config);
    let stamp = now.stamp();
    let mut outcome = Outcome::default();

    // `--limit` bounds the queue before the pool is asked for anything, so a limited run claims exactly the
    // months it was told to and the cap is never spent on work nobody asked for.
    let wanted = limit.unwrap_or(usize::MAX).min(months.len());
    // A month's unit is one read of its row count and one whole-file copy: disk, not CPU, and the drive is
    // the bottleneck past a few lanes (`wind_base::pool`).
    let copies = pool::run(&months[..wanted], pool::lanes(Duty::Disk), || wind_base::maintain::may_continue(config), |month| {
        copy_one(month, &destination, &stamp, dry_run)
    });

    // Folded in item order, and the first failure ends the run the way the serial loop's `?` did — which
    // costs the one thing the serial loop could not: a month whose neighbour failed may already have been
    // copied on another lane. The extra copy is additive and harmless (the next pass counts it as history
    // and prunes it); the alternative, refusing to copy ahead of a month nobody has looked at yet, would
    // put the whole step's wall clock back.
    for slot in copies {
        // A slot left empty is a month the pool never claimed because a stop was asked: the serial loop's
        // `break`, with the same suffix of work left undone.
        let Some(result) = slot else { break };
        let Some(copy) = result? else { continue };
        outcome.months += 1;
        if dry_run {
            println!("{} rows: would copy {} -> {}", copy.rows, copy.source.display(), copy.target.display());
            continue;
        }
        println!("{} rows: {} -> {}", copy.rows, copy.source.display(), copy.target.display());
        outcome.written += 1;
        // One month copied is one item of this step's own count, published where the serial loop
        // published it — after the bytes landed, so a planned copy in a dry run still costs nothing. Not a
        // leg's counter: no census counts month files, and a bar with no denominator is not drawn.
        wind_base::maintain::add_step_items(1);
    }

    let existing: Vec<String> = match std::fs::read_dir(&destination) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let name = path.file_name()?.to_str()?.to_string();
                layout::inside(&destination, &path).then_some(name)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    for stale in prune(&existing, KEEP) {
        let path = destination.join(&stale);
        // Re-derived from a directory listing and re-checked against the same directory: a backup name
        // is a name, and names are how the retention rules get subverted elsewhere.
        if !layout::inside(&destination, &path) {
            continue;
        }
        println!("prune {stale}");
        if !dry_run {
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        outcome.pruned += 1;
    }
    outcome.summaries = backup_summaries(config, now, dry_run)?;
    Ok(outcome)
}

/// `cache/db_backup/result_ai_period_summary` (or `…_daily_summary`) — the copy folder for one family,
/// named exactly like the folder it is copied from so a restore is a copy back to a path that already
/// has a name. Sharing `cache/db_backup` with the month files is the point: one place holds everything
/// this install cannot afford to lose.
pub fn summary_backup_dir(config: &Config, kind: summary::Kind) -> PathBuf {
    backup_dir(config).join(kind.dir_name())
}

/// Copy each day summary file that has changed since its own newest copy, then trim every day's history
/// to [`KEEP`] copies of *that day*.
///
/// The unchanged check is what makes a per-day rotation affordable. Without it a pass would write a copy
/// of every day file in the install every time it ran — three hundred near-identical files a night, most
/// of them a paragraph that has not moved in a month — and the ceiling would be spent on duplicates
/// rather than on history. Byte equality is the check, not a timestamp: a summary rewritten to the same
/// text has not changed, and a file whose mtime moved for a reason nobody can name is not a reason to
/// spend a copy.
pub fn backup_summaries(config: &Config, now: &LocalParts, dry_run: bool) -> Result<SummaryOutcome, String> {
    let stamp = now.stamp();
    let mut outcome = SummaryOutcome::default();
    for kind in [summary::Kind::Period, summary::Kind::Daily] {
        let destination = summary_backup_dir(config, kind);
        // This family's copy folder listed once instead of once per day file: a year of history was ~300
        // listings of a folder holding up to fifteen copies of each of those days. Hoisting is safe
        // because the only writes this loop makes into `destination` are `{day}_BACKUP_{stamp}.json`, and a
        // day's newest copy is found by keeping the names whose base is *that* day — a copy this loop wrote
        // for an earlier day can never be the answer for the day now being asked, and each day is asked
        // once (`days_present` lists distinct file names). An absent folder is the same empty listing the
        // first day's own `names_in` returned. The prune below still re-lists: it has to see these copies.
        let listing = names_in(&destination);
        for day in summary::days_present(config, kind) {
            outcome.days += 1;
            let source = summary::files::day_path(config, kind, &day);
            let Some(name) = backup_name(&source, &stamp) else { continue };
            let target = destination.join(&name);
            if newest_copy(&destination, &listing, &day).is_some_and(|previous| same_bytes(&previous, &source)) {
                outcome.unchanged += 1;
                continue;
            }
            if dry_run {
                println!("would copy {} -> {}", source.display(), target.display());
                continue;
            }
            std::fs::create_dir_all(&destination).map_err(|e| format!("{}: {e}", destination.display()))?;
            std::fs::copy(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
            println!("{} -> {}", source.display(), target.display());
            outcome.copied += 1;
        }
        for stale in prune_per_source(&names_in(&destination), KEEP) {
            let path = destination.join(&stale);
            if !layout::inside(&destination, &path) {
                continue;
            }
            println!("prune {stale}");
            if !dry_run {
                std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            outcome.pruned += 1;
        }
    }
    Ok(outcome)
}

/// The newest copy of one day's file in a copy folder, read out of a listing taken for the whole folder —
/// see [`backup_summaries`] for why one listing serves every day of a family.
fn newest_copy(dir: &Path, names: &[String], base: &str) -> Option<PathBuf> {
    let mut dated: Vec<(LocalParts, String)> = names
        .iter()
        .filter(|name| backup_base_of(name) == Some(base))
        .filter_map(|name| backup_stamp_of(name).map(|stamp| (stamp, name.clone())))
        .collect();
    dated.sort_by(|a, b| b.0.naive_epoch_seconds().cmp(&a.0.naive_epoch_seconds()).then_with(|| a.1.cmp(&b.1)));
    dated.into_iter().next().map(|(_, name)| dir.join(name))
}

fn names_in(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_name()?.to_str()?.to_string();
                layout::inside(dir, &path).then_some(name)
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(one), Ok(other)) => one == other,
        _ => false,
    }
}

fn count_rows(db: &Path) -> Result<i64, String> {
    let conn = refresh::open_read_only(db).map_err(|e| format!("{}: {e}", db.display()))?;
    wind_store::read::count_rows(&conn).map_err(|e| format!("{}: {e}", db.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wind_store::write::{Record, Store};

    fn temp_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-backup-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(time: i64) -> Record {
        Record {
            videofile_name: "2026-09-21_21-16-12.mp4".into(),
            picturefile_name: "f.jpg".into(),
            videofile_time: time,
            ocr_text: "screen text".into(),
            win_title: None,
            deep_linking: None,
            thumbnail: Some("AAA".into()),
        }
    }

    #[test]
    fn a_backup_name_keeps_the_month_it_came_from_and_its_own_instant() {
        assert_eq!(
            backup_name(Path::new("db/default_2026-09_wind.db"), "2026-09-22_03-00-00").as_deref(),
            Some("default_2026-09_wind_BACKUP_2026-09-22_03-00-00.db")
        );
        // The disposable read copy must never become a backup of record.
        assert_eq!(backup_name(Path::new("db/default_2026-09_wind.db_TEMP_READ.db"), "s"), None);
        // And the same naming serves a day summary, extension and all, because a copy that loses which
        // kind of file it was cannot be copied back.
        assert_eq!(
            backup_name(Path::new("userdata/result_ai_period_summary/2026-09-27.json"), "2026-09-28_03-00-00").as_deref(),
            Some("2026-09-27_BACKUP_2026-09-28_03-00-00.json")
        );
        assert_eq!(backup_base_of("2026-09-27_BACKUP_2026-09-28_03-00-00.json"), Some("2026-09-27"));
        assert_eq!(backup_base_of("2026-09-27.json"), None);
    }

    #[test]
    fn a_backup_is_dated_by_the_stamp_in_its_name() {
        let parts = backup_stamp_of("default_2026-09_wind_BACKUP_2026-09-22_03-00-00.db").unwrap();
        assert_eq!(parts.stamp(), "2026-09-22_03-00-00");
        assert!(backup_stamp_of("default_2026-09_wind.db").is_none());
        assert!(backup_stamp_of("default_2026-09_wind_BACKUP_nonsense.db").is_none());
    }

    #[test]
    fn pruning_keeps_the_newest_and_leaves_unparsable_files_alone() {
        let names: Vec<String> = (0..20)
            .map(|day| format!("m_BACKUP_2026-09-{:02}_10-00-00.db", day + 1))
            .chain(std::iter::once("honest-mistake.db".to_string()))
            .collect();
        let stale = prune(&names, KEEP);
        assert_eq!(
            stale,
            vec![
                "m_BACKUP_2026-09-05_10-00-00.db",
                "m_BACKUP_2026-09-04_10-00-00.db",
                "m_BACKUP_2026-09-03_10-00-00.db",
                "m_BACKUP_2026-09-02_10-00-00.db",
                "m_BACKUP_2026-09-01_10-00-00.db",
            ],
            "20 dated backups, the 15 newest kept, oldest reported first"
        );
        assert!(!stale.contains(&"honest-mistake.db".to_string()), "a file we cannot date is not ours to delete");
        assert_eq!(prune(&names, 0).len(), 20);
        assert_eq!(prune(&names, 99).len(), 0);
        assert_eq!(prune(&[], KEEP).len(), 0);
    }

    #[test]
    fn a_run_copies_every_month_and_reads_back_the_same_rows() {
        let root = temp_tree("copy");
        let db = root.join("userdata/db");
        for month in [8u32, 9] {
            let mut store = Store::open_month(&db, "default", 2026, month).unwrap();
            store.append(&[record(1), record(2)]).unwrap();
            drop(store);
        }
        let config = Config::load(&root).unwrap();
        let now = LocalParts::from_stamp("2026-10-01_04-00-00").unwrap();
        let outcome = run(&config, &now, false, None).unwrap();
        assert_eq!((outcome.months, outcome.written, outcome.pruned), (2, 2, 0), "{outcome:?}");

        let copies: Vec<String> = fs::read_dir(backup_dir(&config))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(copies.len(), 2, "{copies:?}");
        assert!(copies.iter().all(|n| n.ends_with("_BACKUP_2026-10-01_04-00-00.db")), "{copies:?}");
        let copy = backup_dir(&config).join("default_2026-09_wind_BACKUP_2026-10-01_04-00-00.db");
        assert_eq!(count_rows(&copy).unwrap(), 2, "the copy is a readable index, not a truncated file");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_creates_no_directory_and_no_copy() {
        let root = temp_tree("dry");
        let mut store = Store::open_month(&root.join("userdata/db"), "default", 2026, 9).unwrap();
        store.append(&[record(1)]).unwrap();
        drop(store);

        let config = Config::load(&root).unwrap();
        let now = LocalParts::from_stamp("2026-10-01_04-00-00").unwrap();
        let outcome = run(&config, &now, true, None).unwrap();
        assert_eq!((outcome.months, outcome.written), (1, 0));
        assert!(!backup_dir(&config).exists(), "the backup directory is not created by a dry run");
        assert!(!root.join("cache").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_old_history_is_trimmed_to_the_newest_fifteen() {
        let root = temp_tree("prune");
        let config = Config::load(&root).unwrap();
        let destination = backup_dir(&config);
        fs::create_dir_all(&destination).unwrap();
        for day in 1..=18 {
            fs::write(destination.join(format!("default_2026-09_wind_BACKUP_2026-05-{day:02}_10-00-00.db")), b"x").unwrap();
        }
        let mut store = Store::open_month(&root.join("userdata/db"), "default", 2026, 9).unwrap();
        store.append(&[record(1)]).unwrap();
        drop(store);

        let now = LocalParts::from_stamp("2026-06-01_04-00-00").unwrap();
        let outcome = run(&config, &now, false, None).unwrap();
        let left = fs::read_dir(&destination).unwrap().count();
        assert_eq!(outcome.pruned, 4, "18 old + 1 new, 15 kept: {outcome:?}");
        assert_eq!(left, KEEP);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn limit_bounds_the_months_copied() {
        let root = temp_tree("limit");
        let db = root.join("userdata/db");
        for month in [7u32, 8, 9] {
            Store::open_month(&db, "default", 2026, month).unwrap();
        }
        let config = Config::load(&root).unwrap();
        let now = LocalParts::from_stamp("2026-10-01_04-00-00").unwrap();
        let outcome = run(&config, &now, false, Some(2)).unwrap();
        assert_eq!((outcome.months, outcome.written), (2, 2));
        assert_eq!(fs::read_dir(backup_dir(&config)).unwrap().count(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_install_with_no_index_backs_up_nothing() {
        let root = temp_tree("empty");
        let config = Config::load(&root).unwrap();
        let now = LocalParts::from_stamp("2026-10-01_04-00-00").unwrap();
        assert_eq!(run(&config, &now, false, None).unwrap(), Outcome::default());
        assert!(!root.join("userdata/db").exists(), "a dry install stays dry");
        let _ = fs::remove_dir_all(&root);
    }

    /// A backup pass reads these as text and copies them as text, so the fixture is the bytes rather
    /// than a parsed entry: what is being tested is which files get copied, not the summary format.
    fn write_day(config: &Config, kind: summary::Kind, day: &str, body: &str) -> PathBuf {
        let path = summary::files::day_path(config, kind, day);
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(&path, body).expect("day file");
        path
    }

    fn copies(dir: &Path) -> Vec<String> {
        match fs::read_dir(dir) {
            Ok(entries) => {
                let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
                names.sort();
                names
            }
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn prune_per_source_ceilings_each_file_its_own_history() {
        let names: Vec<String> = [
            "2026-09-01_BACKUP_2026-10-01_10-00-00.json",
            "2026-09-01_BACKUP_2026-10-02_10-00-00.json",
            "2026-09-02_BACKUP_2026-10-01_10-00-00.json",
            "2026-09-02_BACKUP_2026-10-02_10-00-00.json",
            "2026-09-02_BACKUP_2026-10-03_10-00-00.json",
            "a-stray-note.json",
        ]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
        let stale = prune_per_source(&names, 2);
        assert_eq!(stale, vec!["2026-09-02_BACKUP_2026-10-01_10-00-00.json"], "only the day that overflows loses a copy, and it loses its oldest");
        assert_eq!(prune_per_source(&names, 3).len(), 0);
        assert_eq!(prune_per_source(&names, 0).len(), 5, "a ceiling of zero prunes every dated copy and no other");
    }

    #[test]
    fn summaries_are_copied_beside_the_index_and_an_unchanged_day_is_not_copied_twice() {
        let root = temp_tree("summaries");
        let config = Config::load(&root).unwrap();
        write_day(&config, summary::Kind::Period, "2026-09-26", "{\"a\":\"morning\"}");
        write_day(&config, summary::Kind::Period, "2026-09-27", "{\"a\":\"afternoon\"}");
        write_day(&config, summary::Kind::Daily, "2026-09-27", "{\"date\":\"2026-09-27\"}");
        // A stray the folder may already hold — not a day, not this pass's business.
        let stray = summary::dir(&config, summary::Kind::Period).join("notes.json");
        fs::write(&stray, b"{}").expect("stray");

        let first = backup_summaries(&config, &LocalParts::from_stamp("2026-09-28_03-00-00").unwrap(), false).unwrap();
        assert_eq!((first.days, first.copied, first.unchanged, first.pruned), (3, 3, 0, 0), "{first:?}");
        assert_eq!(
            copies(&summary_backup_dir(&config, summary::Kind::Period)),
            vec![
                "2026-09-26_BACKUP_2026-09-28_03-00-00.json",
                "2026-09-27_BACKUP_2026-09-28_03-00-00.json"
            ]
        );
        assert_eq!(copies(&summary_backup_dir(&config, summary::Kind::Daily)).len(), 1);

        let second = backup_summaries(&config, &LocalParts::from_stamp("2026-09-29_03-00-00").unwrap(), false).unwrap();
        assert_eq!((second.days, second.copied, second.unchanged), (3, 0, 3), "a paragraph that has not moved costs no copy");
        assert_eq!(copies(&summary_backup_dir(&config, summary::Kind::Period)).len(), 2);

        write_day(&config, summary::Kind::Period, "2026-09-27", "{\"a\":\"rewritten\"}");
        let third = backup_summaries(&config, &LocalParts::from_stamp("2026-09-30_03-00-00").unwrap(), false).unwrap();
        assert_eq!((third.days, third.copied, third.unchanged), (3, 1, 2), "{third:?}");
        let day = fs::read_to_string(summary_backup_dir(&config, summary::Kind::Period).join("2026-09-27_BACKUP_2026-09-30_03-00-00.json")).unwrap();
        assert_eq!(day, "{\"a\":\"rewritten\"}", "the copy is the source, byte for byte");
        assert_eq!(
            copies(&summary_backup_dir(&config, summary::Kind::Period)).len(),
            3,
            "and the earlier generation of that day is still there to be gone back to"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The whole reason the summaries do not ride the month files' ceiling: under a newest-15-over-the-
    /// folder rule, a January day is deleted because March got re-summarised, and after two passes on an
    /// install with a year of history there is no history left in there at all.
    #[test]
    fn a_quiet_day_is_not_pruned_because_a_busy_one_was_summarised_again() {
        let root = temp_tree("per-day-history");
        let config = Config::load(&root).unwrap();
        write_day(&config, summary::Kind::Period, "2026-09-01", "old");
        write_day(&config, summary::Kind::Period, "2026-09-02", "new");
        let destination = summary_backup_dir(&config, summary::Kind::Period);
        fs::create_dir_all(&destination).expect("dir");
        for day in 1..=18 {
            fs::write(destination.join(format!("2026-09-01_BACKUP_2026-05-{day:02}_10-00-00.json")), format!("old {day}")).expect("history");
        }
        for day in 1..=3 {
            fs::write(destination.join(format!("2026-09-02_BACKUP_2026-05-{day:02}_10-00-00.json")), b"older").expect("history");
        }

        let outcome = backup_summaries(&config, &LocalParts::from_stamp("2026-06-01_04-00-00").unwrap(), false).unwrap();
        assert_eq!((outcome.days, outcome.copied), (2, 2), "{outcome:?}");
        assert_eq!(outcome.pruned, 4, "only the first day overflows: 18 old + 1 new, 15 kept");
        let left = copies(&destination);
        assert_eq!(left.iter().filter(|n| n.starts_with("2026-09-01_")).count(), KEEP);
        assert_eq!(left.iter().filter(|n| n.starts_with("2026-09-02_")).count(), 4);
        assert_eq!(left.iter().filter(|n| n.starts_with("2026-09-01_")).count() + left.iter().filter(|n| n.starts_with("2026-09-02_")).count(), left.len());
        let _ = fs::remove_dir_all(&root);
    }

    /// `--dry-run` is a genuine no-op on every subcommand, and the summary copies are the part a reader
    /// would least expect a folder for.
    #[test]
    fn a_dry_run_plans_summary_copies_and_writes_nothing() {
        let root = temp_tree("summaries-dry");
        let config = Config::load(&root).unwrap();
        write_day(&config, summary::Kind::Period, "2026-09-27", "{}");
        let outcome = backup_summaries(&config, &LocalParts::from_stamp("2026-09-28_03-00-00").unwrap(), true).unwrap();
        assert_eq!((outcome.days, outcome.copied), (1, 0), "the plan is in `days`; `copied` counts bytes written");
        assert!(!summary_backup_dir(&config, summary::Kind::Period).exists(), "and nothing was written for it");
        assert!(!backup_dir(&config).exists(), "not even the folder it would have gone into");
        let _ = fs::remove_dir_all(&root);
    }

    /// One command, the whole unregenerable state: a `windmaint backup` that copied the index and left
    /// the summaries behind would be the half of a backup that reads as the whole of one.
    #[test]
    fn a_backup_run_copies_the_index_and_the_summaries_together() {
        let root = temp_tree("both");
        let mut store = Store::open_month(&root.join("userdata/db"), "default", 2026, 9).unwrap();
        store.append(&[record(1)]).unwrap();
        drop(store);
        let config = Config::load(&root).unwrap();
        write_day(&config, summary::Kind::Daily, "2026-09-27", "{\"date\":\"2026-09-27\"}");

        let outcome = run(&config, &LocalParts::from_stamp("2026-10-01_04-00-00").unwrap(), false, None).unwrap();
        assert_eq!((outcome.months, outcome.written), (1, 1));
        assert_eq!((outcome.summaries.days, outcome.summaries.copied), (1, 1), "{outcome:?}");
        assert_eq!(copies(&summary_backup_dir(&config, summary::Kind::Daily)).len(), 1);
        let top = copies(&backup_dir(&config));
        assert!(top.iter().any(|n| n.ends_with(".db")), "the month copy is in the folder itself: {top:?}");
        assert_eq!(top.iter().filter(|n| n.ends_with(".json")).count(), 0, "a summary copy is never filed among the month files: {top:?}");
        let _ = fs::remove_dir_all(&root);
    }
}
