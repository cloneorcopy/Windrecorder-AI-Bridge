//! Retention: what the storage policy says has aged out, and the two fates it allows.
//!
//! Two config windows drive this pass, both counted in the product's own days rather than in calendar
//! days: `vid_store_day` deletes a segment outright (its file, the screenshot slice it was made from,
//! and its index rows), `vid_compress_day` keeps it but re-encodes it smaller. `recycle_deleted_files`
//! decides whether the first fate moves to `userdata/trash/` or is unrecoverable, so the decision table
//! below is the only place a file's fate is chosen, and every branch of it is tested.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use wind_base::clock::{self, LocalParts};
use wind_base::config::Config;
use wind_store::maintain::{delete_rows, expired_rows};
use wind_store::read;

use crate::convert;
use crate::encode::{
    self, CompressPreset, CompressTable, EncoderAvailability, CPU_FALLBACK_ACCELERATOR, CPU_FALLBACK_ENCODER, resolve_compress,
};
use crate::layout;
use crate::refresh::{self, DiskVideos};

/// What the retention policy wants done with one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    Keep,
    Delete,
    Compress,
}

/// The exclusive lower bound of what a retention window of `days` product-days still keeps.
///
/// Anchored on the *start of today's product day* rather than on the current clock reading, because
/// `day_begin_minutes` is how the app defines a day: with the shipped 03:00 boundary a capture at 01:00
/// belongs to the day before, and cutting at wall-clock midnight would expire a night's recordings a day
/// early. `days <= 0` disables the sweep entirely, which is upstream's meaning for
/// `vid_store_day = 0` / `vid_compress_day = 0` and is why this returns an `Option` — an install with no
/// config file at all must expire nothing rather than everything.
pub fn retention_cutoff(now: &LocalParts, days: i64, day_begin_minutes: i64) -> Option<i64> {
    if days <= 0 {
        return None;
    }
    let today = now.date_only();
    let (day_start, _) = clock::day_bounds(today.year, today.month, today.day, day_begin_minutes);
    Some(day_start - (days - 1) * 86_400)
}

/// How long a run's recycled files stay in `userdata/trash/` before retention removes them for good.
///
/// The folder is this product's substitute for the Windows Recycle Bin, and it is the one half of
/// `recycle_deleted_files` that had no end: the switch defaults to *on*, every expired video was moved
/// sideways instead of deleted, and nothing ever looked at the destination again — so the default
/// configuration reclaimed nothing and the disk kept shrinking. Seven days is the "you had a week to
/// notice" window a recycle bin is actually for; past it the file has survived the only retention
/// decision the user asked to revisit.
pub const TRASH_KEEP_DAYS: i64 = 7;

/// A trash folder's name is ours to judge only when it is exactly one run stamp.
const TRASH_STAMP_LEN: usize = 19;

/// How old one `userdata/trash/` entry is, in seconds, or `None` when it is not a run of ours.
///
/// The age comes from the *name*, not the filesystem: a folder copied in from another install carries
/// the mtime of the day it arrived, and judging that against this machine's retention clock would
/// delete somebody's backup on the day it was restored. Anything that is not a full run stamp is left
/// alone, which also means a user who drops a folder in there to keep it safe keeps it safe.
pub fn trash_run_age(name: &str, now: &LocalParts) -> Option<i64> {
    if name.len() != TRASH_STAMP_LEN {
        return None;
    }
    let stamp = LocalParts::from_stamp(name)?;
    Some(now.naive_epoch_seconds() - stamp.naive_epoch_seconds())
}

/// A file the indexer is still holding: mid-OCR, or OCR-failed and awaiting its retry.
///
/// Upstream renames a video to `-INDEX` while it is being indexed and to `-ERROR{n}` when that fails.
/// Deleting or re-encoding either would pull the file out from under a running pass and, since the rename
/// carries the same stamp, take the rows of the plainly named file with it.
pub fn in_flight(name: &str) -> bool {
    name.contains("-INDEX") || name.contains("-ERROR")
}

/// One row's fate, given the two cutoffs. Pure, and the table the whole pass is judged by.
///
/// Upstream restricts both branches to names carrying `-OCRED`, i.e. to files the OCR indexing pass has
/// finished with. That is not reproducible here: a native segment stays a plain `{stamp}.mp4` until an
/// indexing pass that is not part of this binary marks it, so requiring the marker would mean nothing on
/// a native install is ever deleted or compressed — an unbounded disk. The in-flight guard above
/// replaces it as the thing that protects work in progress.
pub fn decide(time: i64, name: &str, store_cutoff: Option<i64>, compress_cutoff: Option<i64>) -> Fate {
    if in_flight(name) {
        return Fate::Keep;
    }
    if store_cutoff.is_some_and(|cutoff| time < cutoff) {
        return Fate::Delete;
    }
    // Already compressed is already small: running the encoder over its own output would cost quality for
    // no bytes, which is why that marker is part of the rule and not merely of the name.
    if compress_cutoff.is_some_and(|cutoff| time < cutoff) && !name.contains("-COMPRESS") {
        return Fate::Compress;
    }
    Fate::Keep
}

/// The marker an *indexed video file* carries, which is shorter than the directory marker in
/// `paths::MARKER_OCRED`: the OCR pass renames `x.mp4` to `x-OCRED.mp4`, and every naming rule in the
/// retention code keys off that suffix.
const OCRED_ON_FILE: &str = "-OCRED";

/// The name a re-compressed segment takes: `-COMPRESS` is inserted before the indexing marker, so
/// `x-OCRED.mp4` becomes `x-COMPRESS-OCRED.mp4` exactly as `compress_video_resolution` names it, while a
/// native segment carrying no marker becomes `x-COMPRESS.mp4`. Either way the stamp prefix — which is how
/// every lookup matches a row to a file — is unchanged, so no index row has to be rewritten.
pub fn compressed_name(name: &str) -> String {
    if name.contains("-COMPRESS") {
        return name.to_string();
    }
    let insert_at = match name.find(OCRED_ON_FILE) {
        Some(at) => at,
        None => match name.rfind('.') {
            Some(dot) if dot > 0 => dot,
            _ => name.len(),
        },
    };
    format!("{}-COMPRESS{}", &name[..insert_at], &name[insert_at..])
}

/// Threads to hand the encoder: `-threads` is a CPU-side knob, and passing it to a hardware encoder
/// changes how it fills its own queue, so upstream only sets it for `compress_accelerator = cpu`.
pub fn threads_for(accelerator: &str, threads: i64) -> Option<i64> {
    (accelerator == "cpu" && threads > 0).then_some(threads)
}

/// The encoder the retention pass will really re-encode with, and the note it owes the user.
///
/// Split out of [`Sweep::new`] because the availability question needs an `ffmpeg` path the struct
/// literal cannot borrow twice, and because the pure part — "is the configured row usable, and what
/// is the CPU row?" — is the half worth testing without a machine attached.
fn sweep_encoder(root: &Path, config: &Config, ffmpeg: &Path, dry_run: bool) -> (CompressPreset, Option<String>) {
    let encoder = config.str_or("compress_encoder", "x264");
    let accelerator = config.str_or("compress_accelerator", "cpu");
    let asked = format!("compress_encoder '{encoder}' with compress_accelerator '{accelerator}'");
    let preset = compress_preset(root, config);
    // The CPU row of the *table*, not a constant in this file: the settings page offers exactly that
    // table's `x264`/`cpu` cell, and a hand-edited table that spells the fallback differently should
    // be honoured rather than overruled from here.
    let cpu_fallback = CompressTable::load(root).ok().and_then(|table| table.get(CPU_FALLBACK_ENCODER, CPU_FALLBACK_ACCELERATOR).cloned());
    let availability = |codec: &str| {
        if dry_run {
            EncoderAvailability::Unknown
        } else {
            layout::probe_encoder(ffmpeg, codec)
        }
    };
    let choice = resolve_compress(&asked, preset, cpu_fallback, &availability);
    (choice.preset, choice.note)
}

/// Path of the compress preset table, for the note a misconfigured install produces.
///
/// `encode`'s constant rather than a second copy of it: this one had already drifted into naming
/// the legacy location while the loader had moved on, which is the exact shape of the bug every
/// other hand-rolled path literal in this workspace is now folded away for.
use crate::encode::COMPRESS_PRESET_RELPATH;

/// The encoder the compress window asks for, with upstream's fallback when the tables disagree.
pub fn compress_preset(root: &Path, config: &Config) -> CompressPreset {
    let name = config.str_or("compress_encoder", "x264");
    let accelerator = config.str_or("compress_accelerator", "cpu");
    let fallback = CompressPreset { encoder: "libx264".to_string(), crf_flag: vec!["-crf".to_string()] };
    match CompressTable::load(root) {
        Ok(table) => table.get(&name, &accelerator).cloned().unwrap_or_else(|| {
            eprintln!("note: {name}/{accelerator} is not a preset in {COMPRESS_PRESET_RELPATH}; using libx264");
            fallback
        }),
        Err(e) => {
            eprintln!("note: {e}; using libx264");
            fallback
        }
    }
}

/// What one retention run did. Under `--dry-run` these are the counts it *would* produce.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub months: usize,
    pub rows_examined: usize,
    pub segments_deleted: usize,
    pub files_removed: usize,
    pub slices_removed: usize,
    pub rows_deleted: usize,
    pub segments_compressed: usize,
    pub compress_failures: usize,
    /// Recycled-file run folders removed from `userdata/trash/`, and the bytes they held.
    pub trash_runs_pruned: usize,
    pub trash_bytes_freed: u64,
    /// Stretch summaries dropped with the segments they were written from, and what that left the days
    /// they belonged to. `expire` never deletes a day's own paragraph — see `summaries::Aftermath`.
    pub summaries: crate::summaries::Pruned,
}

/// Everything the sweep touches the filesystem with: the resolved directories, the encoder settings and
/// the trash policy. One place, so no deletion path can forget which tree it is confined to.
#[derive(Debug)]
pub struct Sweep {
    pub root: PathBuf,
    pub videos_dir: PathBuf,
    pub cache_root: PathBuf,
    pub trash_dir: PathBuf,
    pub videos: DiskVideos,
    pub slices: BTreeMap<String, PathBuf>,
    pub ffmpeg: PathBuf,
    pub preset: CompressPreset,
    pub crf: i64,
    pub scale: f64,
    pub threads: Option<i64>,
    /// The run's stamp, which names the trash folder everything recycled lands under.
    pub stamp: String,
    pub recycle: bool,
    pub dry_run: bool,
}

impl Sweep {
    pub fn new(root: &Path, config: &Config, now: &LocalParts, dry_run: bool) -> Sweep {
        let videos_dir = config.videos_dir();
        let cache_root = config.cache_screenshot_dir();
        let ffmpeg = config.ffmpeg_path();
        let (preset, note) = sweep_encoder(root, config, &ffmpeg, dry_run);
        if let Some(note) = note {
            eprintln!("note: {note}");
        }
        Sweep {
            videos: DiskVideos::scan(&videos_dir),
            slices: convert::slice_dirs(&cache_root),
            root: root.to_path_buf(),
            trash_dir: trash_dir(config),
            videos_dir,
            cache_root,
            ffmpeg,
            preset,
            crf: config.i64_or("compress_quality", 39),
            scale: config.f64_or("video_compress_rate", 0.5),
            threads: threads_for(&config.str_or("compress_accelerator", "cpu"), config.i64_or("compress_cpu_threads", 2)),
            stamp: now.stamp(),
            recycle: config.bool_or("recycle_deleted_files", true),
            dry_run,
        }
    }

    /// Move one path out of the way, honouring `recycle_deleted_files`.
    ///
    /// `confined_to` is the tree the path was listed from, and the guard is not decoration: this is the
    /// only route to `remove_dir_all` in the binary, and a name that escaped its folder is reported and
    /// skipped rather than followed.
    pub fn remove(&self, path: &Path, confined_to: &Path) -> Result<bool, String> {
        if !layout::inside(confined_to, path) {
            eprintln!("  refusing to touch {}: outside {}", path.display(), confined_to.display());
            return Ok(false);
        }
        match layout::discard(path, &self.trash_dir, &self.root, &self.stamp, self.recycle) {
            Ok(Some(moved)) => {
                println!("  -> {}", moved.display());
                Ok(true)
            }
            Ok(None) => Ok(true),
            Err(e) => {
                eprintln!("  could not remove: {e}");
                Ok(false)
            }
        }
    }

    /// Re-encode one aged segment smaller, and retire the source once the smaller file is proven.
    ///
    /// `run` already decided the fate; this only carries it out, so a `Keep` never reaches it.
    ///
    /// `may_work` is handed all the way down to the encoder, because a compression is an ffmpeg re-encode of
    /// a whole segment — the slowest single file this step owns — and 停止整理 means put that down now, not
    /// when it finishes. Whatever the encoder had written is removed; the source is never traded for it.
    pub fn compress(&self, source: &Path, may_work: &dyn Fn() -> bool) -> Result<CompressResult, String> {
        let name = source.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let output = source.with_file_name(compressed_name(&name));
        let target = output.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        if output.exists() {
            println!("  {name}: {target} is already there, nothing to do");
            return Ok(CompressResult::Skipped);
        }
        println!("  {name}: re-encode to {target} with {}", self.preset.encoder);
        if self.dry_run {
            return Ok(CompressResult::Planned);
        }
        let args = encode::compress_args(source, &output, &self.preset, self.crf, self.scale, self.threads);
        match layout::run_ffmpeg(&self.ffmpeg, &args, may_work) {
            Ok(()) => {
                // Upstream's own proof that the encoder worked: a sub-kilobyte mp4 is a failure that
                // looked like a success, and the source must never be traded for one.
                if !std::fs::metadata(&output).map(|m| m.len() > 1024).unwrap_or(false) {
                    let _ = std::fs::remove_file(&output);
                    eprintln!("  {name}: the encoder produced nothing usable, keeping the source");
                    return Ok(CompressResult::Failed);
                }
                self.remove(source, &self.videos_dir)?;
                Ok(CompressResult::Written)
            }
            // 停止整理: the half-written smaller file goes and the source stays exactly where it was, so
            // the next window re-encodes it. Not a failure — nobody asked the encoder whether it could.
            Err(layout::Called::Off) => {
                let _ = std::fs::remove_file(&output);
                Ok(CompressResult::CalledOff)
            }
            Err(layout::Called::Failed(e)) => {
                let _ = std::fs::remove_file(&output);
                eprintln!("  {name}: {e}");
                Ok(CompressResult::Failed)
            }
        }
    }
}

/// What happened to one segment's file when the compress window caught up with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressResult {
    /// A re-encode ran and the source was retired.
    Written,
    /// `--dry-run` reached this branch, so the encode is claimed but not done.
    Planned,
    /// Nothing to do: the compressed sibling is already on disk.
    Skipped,
    /// ffmpeg ran and lost; the half-written output was removed and the source left alone.
    Failed,
    /// The encoder was put down because 停止整理 was pressed. The source is untouched, so the next window
    /// re-encodes it, and this is not counted among the failures: the step was told to stop, not shown to
    /// be unable.
    CalledOff,
}

/// Where a recycled file lands: one folder per maintenance run, under `userdata/trash/`.
///
/// Written here rather than spelled at both uses so the pass that fills the folder and the pass that
/// empties it cannot disagree about which tree is theirs.
pub fn trash_dir(config: &Config) -> PathBuf {
    config.userdata_dir().join("trash")
}

/// The size of one run's folder, followed through its subdirectories.
///
/// Only used for the number on the report line, so an unreadable entry counts as zero rather than
/// failing the prune: the deletion already happened, and a byte total is not worth losing that over.
fn dir_bytes(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Err(_) => 0,
        Ok(meta) if !meta.is_dir() => meta.len(),
        Ok(_) => std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|e| dir_bytes(&e.path())).sum())
            .unwrap_or(0),
    }
}

/// Remove the run folders in `trash_dir` older than [`TRASH_KEEP_DAYS`].
///
/// Returns how many were removed and how many bytes they held. Every path is checked [`layout::inside`]
/// its own trash root before anything is unlinked: this is a `remove_dir_all` over a tree whose names
/// came from a directory listing, which is the case the confinement rule in this binary exists for.
fn prune_trash(trash_dir: &Path, now: &LocalParts, dry_run: bool) -> (usize, u64) {
    let entries = match std::fs::read_dir(trash_dir) {
        Ok(entries) => entries,
        // No folder yet: nothing has ever been recycled on this install.
        Err(_) => return (0, 0),
    };
    let keep_for = TRASH_KEEP_DAYS * 86_400;
    let mut removed = 0;
    let mut freed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let aged_out = trash_run_age(&name, now).is_some_and(|age| age > keep_for);
        if !aged_out || !layout::inside(trash_dir, &path) {
            continue;
        }
        let size = dir_bytes(&path);
        if dry_run {
            println!("  trash: would remove {} ({} byte(s))", path.display(), size);
        } else if let Err(e) = std::fs::remove_dir_all(&path) {
            eprintln!("  trash: could not remove {}: {e}", path.display());
            continue;
        } else {
            println!("  trash: removed {} ({} byte(s))", path.display(), size);
        }
        removed += 1;
        freed += size;
    }
    (removed, freed)
}

/// What `userdata/trash/` is holding right now: run folders and bytes.
///
/// Reported by `doctor` beside the retention windows, because the one question a disk-space report
/// cannot leave out is how much of the "freed" space is only parked one directory over.
pub fn trash_holdings(trash_dir: &Path) -> (usize, u64) {
    match std::fs::read_dir(trash_dir) {
        Err(_) => (0, 0),
        Ok(entries) => {
            let runs: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            let bytes = runs.iter().map(|p| dir_bytes(p)).sum();
            (runs.len(), bytes)
        }
    }
}

/// Sweep every month file for segments past their retention window.
pub fn run(root: &Path, config: &Config, dry_run: bool, limit: Option<usize>) -> Result<Outcome, String> {
    let now = clock::now();
    let day_begin = config.day_begin_minutes();
    let store_days = config.i64_or("vid_store_day", 0);
    let compress_days = config.i64_or("vid_compress_day", 0);
    let store_cutoff = retention_cutoff(&now, store_days, day_begin);
    let compress_cutoff = retention_cutoff(&now, compress_days, day_begin);
    println!(
        "retention: store {} day(s) before {}, compress {} day(s) before {}, recycle {}",
        store_days,
        quote_cutoff(store_cutoff),
        compress_days,
        quote_cutoff(compress_cutoff),
        config.bool_or("recycle_deleted_files", true)
    );
    // Ahead of the early return below, because this is the other half of what retention costs: a pass
    // with both windows off still has to age out what earlier passes parked in the trash. The deadline is
    // printed whether or not anything met it, so the policy is readable from the report.
    let (trash_runs_pruned, trash_bytes_freed) = prune_trash(&trash_dir(config), &now, dry_run);
    println!(
        "  trash: kept {} day(s), {} run folder(s) pruned, {} byte(s) freed{}",
        TRASH_KEEP_DAYS,
        trash_runs_pruned,
        trash_bytes_freed,
        if dry_run { " (dry run: nothing removed)" } else { "" }
    );
    let Some(probe_cutoff) = [store_cutoff, compress_cutoff].into_iter().flatten().max() else {
        println!("  both windows are 0, nothing is expired");
        return Ok(Outcome { trash_runs_pruned, trash_bytes_freed, ..Outcome::default() });
    };

    let sweep = Sweep::new(root, config, &now, dry_run);
    // The same line `convert` prints, for the same reason: the user should be able to see which
    // encoder a pass settled on without reading ffmpeg's output, and a note about a fallback that
    // scrolled past four lines earlier is the thing this branch keeps having to add.
    println!(
        "  encoder: {}/{} -> {}{}",
        config.str_or("compress_encoder", "x264"),
        config.str_or("compress_accelerator", "cpu"),
        sweep.preset.encoder,
        if dry_run { " (not probed: dry run)" } else { "" }
    );
    println!("  {} segment stamp(s), {} slice dir(s) on disk", sweep.videos.segments(), sweep.slices.len());
    let mut outcome = Outcome { trash_runs_pruned, trash_bytes_freed, ..Outcome::default() };
    let mut budget = limit.unwrap_or(usize::MAX);
    let mut stopped = false;

    for month in read::discover(&config.db_dir()) {
        if !wind_base::maintain::may_continue(config) {
            break;
        }
        let label = month.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let mut conn = if dry_run {
            refresh::open_read_only(&month.path).map_err(|e| format!("{label}: {e}"))?
        } else {
            month.open_write().map_err(|e| format!("{label}: {e}"))?
        };
        let rows = expired_rows(&conn, probe_cutoff).map_err(|e| format!("{label}: {e}"))?;
        outcome.months += 1;
        outcome.rows_examined += rows.len();

        let mut deletes: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        let mut compressions: BTreeMap<String, PathBuf> = BTreeMap::new();
        for row in rows {
            match decide(row.time, &row.videofile_name, store_cutoff, compress_cutoff) {
                Fate::Keep => {}
                Fate::Delete => {
                    deletes.entry(row.videofile_name.clone()).or_default().push(row.rowid);
                }
                Fate::Compress => {
                    // One file per segment: the un-compressed one, since a directory can legitimately
                    // hold both a source and the sibling a previous run wrote. The name is re-checked
                    // against the row's before the path is ever handed to ffmpeg, because the encode
                    // that follows replaces the file it names.
                    if compressions.contains_key(&row.videofile_name) {
                        continue;
                    }
                    let located = sweep.videos.locate(&row.videofile_name);
                    let file_of = |p: &PathBuf| p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
                    match located.iter().find(|p| layout::same_segment(&file_of(p), &row.videofile_name) && !file_of(p).contains("-COMPRESS")) {
                        Some(source) => {
                            compressions.insert(row.videofile_name.clone(), source.clone());
                        }
                        // The rows carry the name the recorder wrote, so a segment already shrunk by an
                        // earlier pass keeps a name that asks for more work; the disk says otherwise.
                        None if located.iter().any(|p| file_of(p).contains("-COMPRESS")) => {
                            println!("{label}: {} was compressed by an earlier pass", row.videofile_name);
                        }
                        None => {}
                    }
                }
            }
        }

        let mut doomed: Vec<i64> = Vec::new();
        let mut gone: Vec<String> = Vec::new();
        for (name, rowids) in &deletes {
            if budget == 0 {
                stopped = true;
                break;
            }
            budget -= 1;
            let files = sweep.videos.locate(name);
            let slice = layout::segment_stamp_of(name).and_then(|stamp| sweep.slices.get(&stamp));
            println!(
                "{label}: expire {name} — {} file(s), slice {}, {} row(s){}",
                files.len(),
                slice.map_or_else(|| "none".to_string(), |d| d.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string()),
                rowids.len(),
                if dry_run { " [dry-run]" } else { "" }
            );
            outcome.segments_deleted += 1;
            // Named before the dry run's `continue`, so the plan below can say what this segment's
            // summaries would cost — the one part of this pass whose result cannot be re-derived later.
            gone.push(name.clone());
            if dry_run {
                outcome.files_removed += files.len();
                outcome.slices_removed += usize::from(slice.is_some());
                outcome.rows_deleted += rowids.len();
                continue;
            }
            for file in files {
                outcome.files_removed += usize::from(sweep.remove(file, &sweep.videos_dir)?);
            }
            if let Some(slice) = slice {
                outcome.slices_removed += usize::from(sweep.remove(slice, &sweep.cache_root)?);
            }
            doomed.extend(rowids.iter().copied());
        }

        for source in compressions.values() {
            if budget == 0 {
                stopped = true;
                break;
            }
            // A compression is an ffmpeg re-encode of a whole segment, which is the slowest single thing
            // this step does. The same `stopped` path the budget uses takes it: the segments already
            // written stay written, and the rest wait for the next window.
            if !wind_base::maintain::may_continue(config) {
                stopped = true;
                break;
            }
            wind_base::maintain::add_items(wind_base::maintain::Leg::Other, 1);
            budget -= 1;
            match sweep.compress(source, &|| wind_base::maintain::may_continue(config))? {
                CompressResult::Written | CompressResult::Planned => outcome.segments_compressed += 1,
                CompressResult::Skipped => {}
                CompressResult::Failed => outcome.compress_failures += 1,
                // The same `stopped` path the budget and the between-items check use: the segments already
                // written stay written, the rest wait for the next window, and the encoder that was put down
                // mid-file left nothing behind to be uncertain about.
                CompressResult::CalledOff => {
                    stopped = true;
                    break;
                }
            }
        }

        // The rows of a segment whose files are already gone go in the same transaction, whatever the
        // budget said afterwards: leaving them behind would point the index at files that no longer
        // exist until the next refresh corrects the flag.
        if !dry_run && !doomed.is_empty() {
            let tx = conn.transaction().map_err(|e| format!("{label}: {e}"))?;
            outcome.rows_deleted += delete_rows(&tx, &doomed).map_err(|e| format!("{label}: {e}"))?;
            tx.commit().map_err(|e| format!("{label}: {e}"))?;
        }
        if !gone.is_empty() {
            // The summary of a stretch whose video, frames and rows are gone describes nothing. The
            // paragraph is dropped and the day it belonged to is flagged, through the same helper
            // `forget` uses, so the two passes cannot disagree about what happens to derived text — and
            // the dry run gets the same plan rather than a zero, because this is the one part of the
            // sweep no later pass can reconstruct.
            let reached = if dry_run {
                crate::summaries::plan(config, &gone, crate::summaries::Aftermath::Flag)
            } else {
                crate::summaries::prune_segments(config, &gone, crate::summaries::Aftermath::Flag)
            };
            outcome.summaries.entries += reached.entries;
            outcome.summaries.days_marked += reached.days_marked;
            outcome.summaries.days_touched += reached.days_touched;
        }
        if stopped {
            break;
        }
    }
    Ok(outcome)
}

fn quote_cutoff(cutoff: Option<i64>) -> String {
    match cutoff {
        Some(at) => LocalParts::from_naive_epoch(at).display(),
        None => "off".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wind_store::write::{Record, Store};

    fn at(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    fn temp_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-expire-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The retention policy under test, written where `Config::load` will find it.
    ///
    /// Tests state their policy instead of inheriting the shipped 1200/300 days, because an absent
    /// config file means *no* retention — the safe default — and a test that quietly relied on the
    /// shipped numbers would stop testing anything if those were ever changed.
    fn config_with_policy(root: &Path) -> Config {
        let src = root.join("windrecorder/config_src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("config_default.json"),
            r#"{"vid_store_day":1200,"vid_compress_day":300,"recycle_deleted_files":false,
                "compress_encoder":"x264","compress_accelerator":"cpu","compress_quality":39,
                "video_compress_rate":0.5,"compress_cpu_threads":2,"day_begin_minutes":180}"#,
        )
        .unwrap();
        Config::load(root).unwrap()
    }

    fn record(video: &str, frame: &str, time: i64) -> Record {
        Record {
            videofile_name: video.into(),
            picturefile_name: frame.into(),
            videofile_time: time,
            ocr_text: "screen text".into(),
            win_title: None,
            deep_linking: None,
            thumbnail: Some("AAA".into()),
        }
    }

    /// Every file under a tree, sorted, as a stand-in for "did anything move at all".
    fn walk(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path.to_string_lossy().into_owned());
            }
        }
        out.sort();
        out
    }

    /// A month of index holding one segment from 2020 (two files, one converted slice, two rows) and one
    /// captured an hour ago, which is the shape a real disk is in.
    fn fixture(tag: &str) -> (PathBuf, String) {
        let root = temp_tree(tag);
        let old = "2020-01-01_10-00-00";
        let old_dir = root.join("userdata/videos/2020-01");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join(format!("{old}.mp4")), vec![b'x'; 4096]).unwrap();
        fs::write(old_dir.join(format!("{old}-COMPRESS.mp4")), b"smaller".to_vec()).unwrap();

        let now = clock::now().naive_epoch_seconds();
        let recent = LocalParts::from_naive_epoch(now - 3_600);
        let recent_name = recent.stamp();
        let recent_dir = root.join("userdata/videos").join(format!("{:04}-{:02}", recent.year, recent.month));
        fs::create_dir_all(&recent_dir).unwrap();
        fs::write(recent_dir.join(format!("{recent_name}.mp4")), b"new").unwrap();

        let slice = root.join("cache_screenshot").join(format!("{old}-VIDEO"));
        fs::create_dir_all(&slice).unwrap();
        fs::write(slice.join(format!("{old}.jpg")), b"j").unwrap();

        let mut store = Store::open_month(&root.join("userdata/db"), "default", 2020, 1).unwrap();
        let frame = slice.join(format!("{old}.jpg")).to_string_lossy().into_owned();
        store.append(&[record(&format!("{old}.mp4"), &frame, at(old)), record(&format!("{old}.mp4"), &frame, at(old) + 5)]).unwrap();
        drop(store);
        let mut fresh = Store::open_month(&root.join("userdata/db"), "default", recent.year, recent.month).unwrap();
        fresh.append(&[record(&format!("{recent_name}.mp4"), "", now - 3_600)]).unwrap();
        drop(fresh);
        (root, recent_name)
    }

    /// One segment past the compress window but inside the store window: `aged` days before today.
    fn aged_segment(root: &Path, aged_days: i64) -> (String, PathBuf) {
        let aged = clock::now().naive_epoch_seconds() - aged_days * 86_400;
        let parts = LocalParts::from_naive_epoch(aged);
        let stamp = parts.stamp();
        let month_dir = root.join("userdata/videos").join(format!("{:04}-{:02}", parts.year, parts.month));
        fs::create_dir_all(&month_dir).unwrap();
        let source = month_dir.join(format!("{stamp}.mp4"));
        fs::write(&source, vec![b'x'; 4096]).unwrap();
        let mut store = Store::open_month(&root.join("userdata/db"), "default", parts.year, parts.month).unwrap();
        store.append(&[record(&format!("{stamp}.mp4"), "", aged)]).unwrap();
        drop(store);
        (stamp, source)
    }

    #[test]
    fn a_cutoff_is_the_start_of_the_oldest_day_kept() {
        let now = LocalParts::from_stamp("2026-09-22_19-48-12").unwrap();
        // Keeping 1 day means keeping only today's product day, which began at 03:00.
        assert_eq!(retention_cutoff(&now, 1, 180), Some(at("2026-09-22_03-00-00")));
        assert_eq!(retention_cutoff(&now, 2, 180), Some(at("2026-09-21_03-00-00")));
        assert_eq!(retention_cutoff(&now, 1200, 180), Some(at("2023-06-11_03-00-00")));
        // A midnight boundary degenerates to the calendar day, as `day_begin_minutes = 0` does.
        assert_eq!(retention_cutoff(&now, 1, 0), Some(at("2026-09-22_00-00-00")));
        // A window that crosses a year boundary still lands on a product-day start.
        let january = LocalParts::from_stamp("2026-01-03_01-00-00").unwrap();
        assert_eq!(retention_cutoff(&january, 3, 180), Some(at("2026-01-01_03-00-00")));
    }

    #[test]
    fn a_zero_window_disables_the_sweep_rather_than_expiring_everything() {
        let now = LocalParts::from_stamp("2026-09-22_19-48-12").unwrap();
        assert_eq!(retention_cutoff(&now, 0, 180), None);
        assert_eq!(retention_cutoff(&now, -5, 180), None);
        assert!(retention_cutoff(&now, 300, 180).is_some());
    }

    /// The whole retention policy in one table, including the ways it stays silent.
    #[test]
    fn the_decision_table_covers_every_branch() {
        let store = at("2026-09-01_03-00-00");
        let compress = at("2026-09-10_03-00-00");
        let recent = at("2026-09-20_12-00-00");

        assert_eq!(decide(store - 1, "old.mp4", Some(store), Some(compress)), Fate::Delete, "past both windows: delete wins");
        assert_eq!(decide(store, "old.mp4", Some(store), Some(compress)), Fate::Compress, "the cutoff itself is still kept");
        assert_eq!(decide(compress - 1, "mid.mp4", Some(store), Some(compress)), Fate::Compress);
        assert_eq!(decide(recent, "new.mp4", Some(store), Some(compress)), Fate::Keep);
        assert_eq!(decide(recent, "new.mp4", None, None), Fate::Keep, "retention off");
        assert_eq!(decide(store - 1, "x.mp4", None, Some(compress)), Fate::Compress, "only the compress window is on");
        assert_eq!(decide(store - 1, "x.mp4", Some(store), None), Fate::Delete, "only the delete window is on");
        assert_eq!(decide(compress - 1, "x-COMPRESS.mp4", Some(store), Some(compress)), Fate::Keep, "small enough already");
        // A file the indexer is still working on is nobody's to touch, however old its stamp.
        assert_eq!(decide(store - 10, "x-INDEX.mp4", Some(store), Some(compress)), Fate::Keep);
        assert_eq!(decide(store - 10, "x-ERROR2.mp4", Some(store), Some(compress)), Fate::Keep);
        assert!(in_flight("2026-09-21_21-16-12-ERROR3.mp4"));
        assert!(!in_flight("2026-09-21_21-16-12-OCRED.mp4"));
    }

    #[test]
    fn a_compressed_name_is_inserted_before_the_pipeline_marker() {
        assert_eq!(compressed_name("2026-09-21_21-16-12-OCRED.mp4"), "2026-09-21_21-16-12-COMPRESS-OCRED.mp4");
        assert_eq!(compressed_name("2026-09-21_21-16-12.mp4"), "2026-09-21_21-16-12-COMPRESS.mp4");
        assert_eq!(compressed_name("2026-09-21_21-16-12-COMPRESS-OCRED.mp4"), "2026-09-21_21-16-12-COMPRESS-OCRED.mp4");
        assert_eq!(compressed_name("no-extension"), "no-extension-COMPRESS");
        assert_eq!(
            wind_base::paths::stamp_prefix(&compressed_name("2026-09-21_21-16-12-OCRED.mp4")),
            wind_base::paths::stamp_prefix("2026-09-21_21-16-12.mp4"),
            "the rename must not break the row lookup that matches on the stamp"
        );
    }

    #[test]
    fn only_a_cpu_accelerator_gets_a_thread_count() {
        assert_eq!(threads_for("cpu", 2), Some(2));
        assert_eq!(threads_for("nvenc", 2), None);
        assert_eq!(threads_for("cpu", 0), None, "0 means leave it to ffmpeg");
    }

    #[test]
    fn the_shipped_compress_config_resolves_to_a_real_encoder() {
        let install = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).unwrap().to_path_buf();
        let preset = compress_preset(&install, &Config::load(&install).unwrap());
        assert_eq!(preset.encoder, "libx264", "the shipped default is x264 on cpu");
        assert_eq!(preset.crf_flag, ["-crf"]);

        // An install configured for something the table has never heard of still compresses.
        let dir = temp_tree("preset");
        fs::create_dir_all(dir.join("windrecorder/config_src")).unwrap();
        fs::write(
            dir.join("windrecorder/config_src/video_compress_preset.json"),
            r#"{"x264":{"cpu":{"encoder":"libx264","crf_flag":"-crf"}}}"#,
        )
        .unwrap();
        assert_eq!(compress_preset(&dir, &Config::load(&dir).unwrap()).encoder, "libx264");
        let missing = temp_tree("preset-missing");
        assert_eq!(compress_preset(&missing, &Config::load(&missing).unwrap()).encoder, "libx264");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&missing);
    }

    #[test]
    fn expired_slices_are_found_whatever_marker_they_carry() {
        let root = temp_tree("slices");
        let cache = root.join("cache_screenshot");
        fs::create_dir_all(cache.join("2026-09-21_21-16-12")).unwrap();
        fs::create_dir_all(cache.join("2026-09-20_10-00-00-VIDEO")).unwrap();
        fs::create_dir_all(cache.join("2026-09-19_10-00-00-DISCARD")).unwrap();
        fs::create_dir_all(cache.join("not-a-stamp")).unwrap();
        fs::write(cache.join("loose.jpg"), b"j").unwrap();

        let found = convert::slice_dirs(&cache);
        assert_eq!(found.len(), 3, "marked slices still hold the frames they were made of");
        assert!(found.contains_key("2026-09-21_21-16-12"));
        assert!(found.contains_key("2026-09-20_10-00-00"));
        assert!(convert::slice_dirs(&root.join("absent")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_expired_segment_takes_its_files_its_slice_and_its_rows() {
        let (root, recent_name) = fixture("delete");
        let config = config_with_policy(&root);
        let outcome = run(&root, &config, false, None).unwrap();

        assert_eq!(outcome.segments_deleted, 1, "{outcome:?}");
        assert_eq!(outcome.files_removed, 2, "the video and its -COMPRESS sibling");
        assert_eq!(outcome.slices_removed, 1);
        assert_eq!(outcome.rows_deleted, 2);
        assert_eq!(outcome.segments_compressed, 0, "a segment past both windows is deleted, not shrunk");

        let month_dir = root.join("userdata/videos/2020-01");
        assert!(!month_dir.join("2020-01-01_10-00-00.mp4").exists());
        assert!(!month_dir.join("2020-01-01_10-00-00-COMPRESS.mp4").exists());
        assert!(!root.join("cache_screenshot/2020-01-01_10-00-00-VIDEO").exists());
        let survivors = walk(&root.join("userdata/videos"));
        assert!(survivors.iter().any(|p| p.ends_with(&format!("{recent_name}.mp4"))), "a fresh recording is untouched: {survivors:?}");

        let conn = rusqlite::Connection::open(root.join("userdata/db/default_2020-01_wind.db")).unwrap();
        assert_eq!(conn.query_row("SELECT count(*) FROM video_text", [], |r| r.get::<_, i64>(0)).unwrap(), 0, "the rows go with the segment");
        let survivors: i64 = read::discover(&root.join("userdata/db"))
            .iter()
            .map(|m| {
                rusqlite::Connection::open(&m.path)
                    .and_then(|c| c.query_row("SELECT count(*) FROM video_text", [], |r| r.get::<_, i64>(0)))
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(survivors, 1, "only the fresh segment's row is left");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recycled_files_land_in_the_trash_and_rows_still_go() {
        let (root, _) = fixture("trash");
        let mut config = config_with_policy(&root);
        config.set("recycle_deleted_files", serde_json::Value::Bool(true));
        run(&root, &config, false, None).unwrap();

        let trashed = walk(&root.join("userdata/trash"));
        assert!(trashed.iter().any(|p| p.ends_with("2020-01-01_10-00-00.mp4")), "{trashed:?}");
        assert!(trashed.iter().any(|p| p.ends_with("2020-01-01_10-00-00.jpg")), "the slice rides along: {trashed:?}");
        assert!(!root.join("userdata/videos/2020-01/2020-01-01_10-00-00.mp4").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn with_recycling_off_the_bytes_are_really_gone() {
        let (root, _) = fixture("hard-delete");
        let config = config_with_policy(&root);
        run(&root, &config, false, None).unwrap();
        assert!(!root.join("userdata/trash").exists(), "nothing is staged for a trash that is switched off");
        assert!(walk(&root.join("userdata/videos/2020-01")).is_empty());
        assert!(!root.join("cache_screenshot/2020-01-01_10-00-00-VIDEO").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_reports_the_sweep_and_leaves_every_byte_in_place() {
        let (root, _) = fixture("dry");
        let config = config_with_policy(&root);
        let tree_before = walk(&root);
        let watched = vec![
            root.join("userdata/videos/2020-01/2020-01-01_10-00-00.mp4"),
            root.join("userdata/db/default_2020-01_wind.db"),
            root.join("cache_screenshot/2020-01-01_10-00-00-VIDEO/2020-01-01_10-00-00.jpg"),
        ];
        let before: Vec<Vec<u8>> = watched.iter().map(|p| fs::read(p).unwrap()).collect();

        let outcome = run(&root, &config, true, None).unwrap();
        assert_eq!(outcome.segments_deleted, 1, "the plan is still reported");
        assert_eq!(outcome.files_removed, 2);
        assert_eq!(outcome.rows_deleted, 2);
        assert_eq!(walk(&root), tree_before, "no file, directory or byte moved");
        assert_eq!(watched.iter().map(|p| fs::read(p).unwrap()).collect::<Vec<_>>(), before);
        let _ = fs::remove_dir_all(&root);
    }

    /// The paragraph an outside AI wrote about a segment is the one artefact this pass cannot redo, and
    /// it describes footage that is now gone — so it goes with the files, and the day it belonged to
    /// says so. Flagged rather than deleted: nothing here was aimed at that day's content.
    #[test]
    fn an_expired_segment_takes_its_summary_paragraph_and_flags_the_day_it_belonged_to() {
        let (root, _) = fixture("summaries");
        let config = config_with_policy(&root);
        let old = "2020-01-01_10-00-00";
        let day = wind_summary::day_of(at(old), config.day_begin_minutes());
        wind_summary::test_support::seed_day(&config, &day, &[old]);

        let planned = run(&root, &config, true, None).unwrap();
        assert_eq!(
            (planned.summaries.entries, planned.summaries.days_marked, planned.summaries.days_removed),
            (1, 1, 0),
            "{planned:?}: the dry run names the prose it is about to reach"
        );
        assert_eq!(wind_summary::read_period(&config, &day).len(), 1, "and reaches nothing while planning");

        let done = run(&root, &config, false, None).unwrap();
        assert_eq!(done.summaries, planned.summaries, "and the real run keeps the plan's number exactly");
        assert!(wind_summary::read_period(&config, &day).absent(), "the paragraph went with the video");
        assert!(wind_summary::read_daily(&config, &day).summary.expect("kept").stale, "the day is flagged, not deleted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_crafted_row_name_cannot_reach_outside_the_scanned_directories() {
        let root = temp_tree("escape");
        let documents = root.join("Documents");
        fs::create_dir_all(&documents).unwrap();
        fs::write(documents.join("life-work.mp4"), b"mine").unwrap();
        let cache = root.join("cache_screenshot");
        fs::create_dir_all(cache.join("2020-01-01_10-00-00")).unwrap();
        fs::write(cache.join("2020-01-01_10-00-00/f.jpg"), b"j").unwrap();

        // A row whose names point out of their trees, at a stamp a real slice also carries.
        let mut store = Store::open_month(&root.join("userdata/db"), "default", 2020, 1).unwrap();
        store.append(&[record("../../Documents/life-work.mp4", "../../../../Documents/life-work.mp4", at("2020-01-01_10-00-00"))]).unwrap();
        drop(store);

        let config = config_with_policy(&root);
        let outcome = run(&root, &config, false, None).unwrap();
        assert_eq!(outcome.rows_deleted, 1, "the row is past its window and is dropped");
        assert_eq!(outcome.files_removed, 0, "the name matched nothing the scan listed");
        assert!(documents.join("life-work.mp4").exists(), "a file outside videos/ is not ours to delete");
        assert!(cache.join("2020-01-01_10-00-00/f.jpg").exists(), "nor is a directory reached by ..");
        let _ = fs::remove_dir_all(&root);
    }

    /// The compress branch, driven end to end against an `ffmpeg.exe` that is not an executable, so the
    /// failure path is what gets tested and the suite never needs an encoder installed.
    #[test]
    fn a_segment_inside_the_compress_window_is_shrunk_not_deleted() {
        let root = temp_tree("compress");
        fs::write(root.join("ffmpeg.exe"), b"this is not a portable executable").unwrap();
        let (stamp, source) = aged_segment(&root, 400);
        let month_dir = source.parent().unwrap().to_path_buf();
        let config = config_with_policy(&root);

        let outcome = run(&root, &config, false, None).unwrap();
        assert_eq!(outcome.segments_compressed, 0, "{outcome:?}");
        assert_eq!(outcome.compress_failures, 1, "the encoder is a text file");
        assert_eq!(outcome.rows_deleted, 0, "a compress never deletes rows");
        assert_eq!(outcome.files_removed, 0);
        assert!(source.exists(), "the source survives a failed re-encode");
        assert!(!month_dir.join(format!("{stamp}-COMPRESS.mp4")).exists(), "no half-written output is left");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_existing_compressed_sibling_means_no_work_and_no_risk() {
        let root = temp_tree("compress-twice");
        let (stamp, source) = aged_segment(&root, 400);
        fs::write(source.with_file_name(format!("{stamp}-COMPRESS.mp4")), vec![b'y'; 2048]).unwrap();
        let config = config_with_policy(&root);

        let outcome = run(&root, &config, false, None).unwrap();
        assert_eq!((outcome.segments_compressed, outcome.compress_failures, outcome.files_removed), (0, 0, 0), "{outcome:?}");
        assert!(source.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_of_the_compress_branch_claims_the_plan_and_runs_nothing() {
        let root = temp_tree("compress-dry");
        fs::write(root.join("ffmpeg.exe"), b"this is not a portable executable").unwrap();
        aged_segment(&root, 400);
        let config = config_with_policy(&root);
        let tree_before = walk(&root);

        let outcome = run(&root, &config, true, None).unwrap();
        assert_eq!(outcome.segments_compressed, 1, "the plan is reported");
        assert_eq!(walk(&root), tree_before, "and nothing at all happened");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn limit_bounds_the_segments_acted_on() {
        let root = temp_tree("limit");
        let month_dir = root.join("userdata/videos/2020-01");
        fs::create_dir_all(&month_dir).unwrap();
        let mut store = Store::open_month(&root.join("userdata/db"), "default", 2020, 1).unwrap();
        for second in 0..3u32 {
            let stamp = format!("2020-01-01_10-0{second}-00");
            fs::write(month_dir.join(format!("{stamp}.mp4")), b"x").unwrap();
            store.append(&[record(&format!("{stamp}.mp4"), "", at(&stamp))]).unwrap();
        }
        drop(store);

        let config = config_with_policy(&root);
        let outcome = run(&root, &config, false, Some(1)).unwrap();
        assert_eq!(outcome.segments_deleted, 1, "{outcome:?}");
        assert_eq!(outcome.rows_deleted, 1);
        assert_eq!(month_dir.read_dir().unwrap().count(), 2, "the other two segments are for the next run");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn retention_switched_off_entirely_visits_nothing_and_deletes_nothing() {
        let root = temp_tree("off");
        let mut store = Store::open_month(&root.join("userdata/db"), "default", 2020, 1).unwrap();
        store.append(&[record("2020-01-01_10-00-00.mp4", "", at("2020-01-01_10-00-00"))]).unwrap();
        drop(store);
        let mut config = config_with_policy(&root);
        config.set("vid_store_day", serde_json::Value::from(0));
        config.set("vid_compress_day", serde_json::Value::from(0));

        let outcome = run(&root, &config, false, None).unwrap();
        assert_eq!(outcome, Outcome::default(), "both windows off means no month is even opened");
        let conn = rusqlite::Connection::open(root.join("userdata/db/default_2020-01_wind.db")).unwrap();
        assert_eq!(conn.query_row("SELECT count(*) FROM video_text", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    /// The stamp length the trash prune recognises has to be the shape this binary writes.
    #[test]
    fn a_trash_run_folder_is_named_by_one_full_run_stamp() {
        let stamp = LocalParts::from_stamp("2026-09-26_03-00-00").unwrap().stamp();
        assert_eq!(stamp.len(), TRASH_STAMP_LEN, "the recogniser and the writer drifted apart");
    }

    #[test]
    fn the_trash_window_judges_only_our_own_run_stamps() {
        let now = LocalParts::from_stamp("2026-09-27_12-00-00").unwrap();
        assert_eq!(trash_run_age("2026-09-26_03-00-00", &now), Some(33 * 3_600), "one day and nine hours");
        assert!(trash_run_age("not-a-stamp", &now).is_none(), "a folder nobody here created");
        assert!(trash_run_age("2026-09-26_03-00-00-backup", &now).is_none(), "a stamp with a tail is not one of our runs");
        assert!(trash_run_age("", &now).is_none());
    }

    /// The whole point of the prune: `recycle_deleted_files` defaults to on, so without this the
    /// retention pass moves an expired library sideways and the disk never gets it back.
    #[test]
    fn an_aged_trash_run_is_removed_and_a_fresh_one_and_a_stranger_are_left_alone() {
        let root = temp_tree("trash-prune");
        let trash = root.join("userdata/trash");
        let now = LocalParts::from_stamp("2026-09-27_12-00-00").unwrap();
        for (name, size) in [
            ("2026-09-01_03-00-00", 3000usize),
            ("2026-09-26_03-00-00", 200),
            ("holiday-videos", 200),
        ] {
            let inside = trash.join(name).join("userdata/videos");
            fs::create_dir_all(&inside).unwrap();
            fs::write(inside.join("x.mp4"), vec![b'x'; size]).unwrap();
        }
        let (runs, bytes) = trash_holdings(&trash);
        assert_eq!((runs, bytes), (3, 3400), "everything in the folder is counted, stamp or not");

        let (removed, freed) = prune_trash(&trash, &now, false);
        assert_eq!(removed, 1, "only the run of ours past the window");
        assert!(freed >= 3000, "{freed}");
        assert!(!trash.join("2026-09-01_03-00-00").exists());
        assert!(trash.join("2026-09-26_03-00-00").exists(), "inside the keep window");
        assert!(trash.join("holiday-videos").exists(), "not a run of ours, never touched");

        let (runs, _) = trash_holdings(&trash);
        assert_eq!(runs, 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_names_the_aged_trash_run_and_removes_nothing() {
        let root = temp_tree("trash-prune-dry");
        let trash = root.join("userdata/trash");
        let aged = trash.join("2026-09-01_03-00-00");
        fs::create_dir_all(&aged).unwrap();
        fs::write(aged.join("x.mp4"), vec![b'x'; 128]).unwrap();
        let now = LocalParts::from_stamp("2026-09-27_12-00-00").unwrap();

        let (removed, freed) = prune_trash(&trash, &now, true);
        assert_eq!((removed, freed), (1, 128), "the plan is reported, not just the deed");
        assert!(aged.exists(), "and a dry run deletes nothing");
        let _ = fs::remove_dir_all(&root);
    }
}
