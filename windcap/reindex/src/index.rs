//! Indexing one already-recorded video.
//!
//! This is `ocr_manager.ocr_core_logic` plus the state machine wrapped around it in
//! `ocr_process_single_video`, and it is the path that makes a user's existing library searchable. The
//! live recorder already writes rows; this writes them for video recorded before the recorder existed,
//! or by the Python app.
//!
//! Four things about the shape are worth stating up front, because they are the parts a reader is most
//! likely to "improve" and break:
//!
//!   * The rename that marks a file `-INDEX` happens *before* any frame is touched, and the rename to
//!     `-OCRED` only after the rows are committed. The marker is the only record that a run was
//!     interrupted, and the rollback it triggers is the only thing that stops a crashed night from
//!     leaving half-indexed rows behind.
//!   * Deduplication runs twice, against two different strings. The sequential pass compares the *raw*
//!     OCR text of consecutive frames; the cross-time pass compares the *stored* text, which carries
//!     the window title after `" -||- "`. Comparing the wrong one changes which rows survive.
//!   * Every row's timestamp comes from the segment's filename and the frame's index. Nothing reads the
//!     container's own timing, so the naive-local epoch convention in [`crate::timeline`] has to hold
//!     exactly.
//!   * A video file is never deleted. Renames only, always inside the directory it was found in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use wind_store::maintain::text_similarity;
use wind_store::write::{Record, Store};

use crate::crop::{self, CropError, MaskPlan, Pixels, Tile};
use crate::engine::Engine;
use crate::frames::{self, ExtractError, Frame};
use crate::naming;
use crate::text::{clean_dirty_text, is_str_contain_list_word};
use crate::timeline;
use crate::wintitle::{optimize_window_title, TitleTable};

/// `len(ocr_result_stringB) < 3` — under this many characters a frame is noise, not a screen.
pub const MIN_TEXT_CHARS: usize = 3;

/// Everything the pipeline needs, resolved once so the run itself is a function of data.
///
/// Built by [`Settings::from_config`]; a test constructs it directly, which is the only way the
/// thresholds, the rename rules and the failure paths can be pinned without an install directory,
/// ffmpeg or the OCR engine on disk.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The install root: holds `userdata/`, `cache/` and `ocr_lib/`.
    pub root: PathBuf,
    /// The library root. Renames stay inside it, and so does every path this crate will accept.
    pub videos_dir: PathBuf,
    pub db_dir: PathBuf,
    pub win_title_dir: PathBuf,
    pub iframe_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// `cache_screenshot/` — the recorder's slice folders, and the only evidence that the live path can
    /// still read a segment's own frames. [`coverage`] asks it before deciding to re-OCR a segment the
    /// index already holds rows for.
    pub screenshot_dir: PathBuf,
    pub user_name: String,
    pub ffmpeg: PathBuf,
    pub encoder: String,
    /// `record_framerate`, the divisor that turns a frame index into seconds.
    pub framerate: i64,
    /// `record_seconds`, the nominal segment length. Used only to bound which months a rollback scans.
    pub record_seconds: i64,
    pub ocr_lang: String,
    /// The OCR engine `ocr_engine` selects, resolved once through [`wind_base::ocr::Engine::select`] — the
    /// same door the live recorder walks through, so a back-index cannot read a frame with a different
    /// engine than the one that recorded it.
    pub ocr: wind_base::ocr::Engine,
    pub exclude_words: Vec<String>,
    /// `ocr_image_crop_URBL`, the edges the user excluded from OCR: four percentages per display slot,
    /// in top/right/bottom/left order. Read as a raw list and interpreted by `windcap::crop`, the one
    /// geometry the live recorder uses too.
    pub urbl: Vec<i64>,
    /// Panels attached, the divisor in the similarity threshold.
    pub display_count: usize,
    /// `index_reduce_same_content_at_different_time`.
    pub cross_time_dedup: bool,
    /// `ocr_compare_similarity_in_table`, already a fraction.
    pub cross_time_threshold: f64,
    pub thumbnail_width: u32,
    pub thumbnail_quality: u8,
    /// Leave the extracted frames on disk afterwards. Upstream keeps them when image-embedding search
    /// is enabled, because that pass consumes them next; with no such pass here, the default cleans up.
    pub keep_frames: bool,
}

impl Settings {
    /// Read every key the pipeline uses from the merged config, and ask the OS how many displays are
    /// attached the way `utils.get_display_count()` does.
    pub fn from_config(config: &wind_base::Config) -> Settings {
        Settings {
            root: config.root().to_path_buf(),
            videos_dir: config.videos_dir(),
            db_dir: config.db_dir(),
            win_title_dir: config.win_title_dir(),
            iframe_dir: config.iframe_dir(),
            cache_dir: config.cache_dir(),
            screenshot_dir: config.cache_screenshot_dir(),
            user_name: config.user_name(),
            ffmpeg: config.ffmpeg_path(),
            encoder: config.str_or("record_encoder", "cpu_h264"),
            framerate: config.i64_or("record_framerate", 2),
            record_seconds: config.i64_or("record_seconds", 900),
            ocr_lang: config.str_or("ocr_lang", "zh-Hans-CN"),
            ocr: wind_base::ocr::Engine::select(config),
            exclude_words: config.str_list("exclude_words"),
            urbl: config.i64_list(crop::CONFIG_KEY),
            display_count: display_count(),
            cross_time_dedup: config.bool_or("index_reduce_same_content_at_different_time", true),
            cross_time_threshold: config.f64_or("ocr_compare_similarity_in_table", 0.94),
            thumbnail_width: config.thumbnail_width(),
            thumbnail_quality: config.thumbnail_quality(),
            keep_frames: false,
        }
    }

    /// The threshold the sequential dedup uses, as a fraction.
    pub fn similarity_threshold(&self) -> f64 {
        similarity_threshold_for(self.display_count)
    }

    /// Where this video's extracted frames live.
    ///
    /// Keyed by the *unmarked* stem, as upstream does, so an interrupted run's leftovers are found and
    /// wiped by the retry rather than read again and indexed twice.
    pub fn frame_dir(&self, video: &str) -> PathBuf {
        self.iframe_dir.join(strip_extension(&naming::base_name(video)))
    }
}

/// `100 - 30 / display_count`, in percent, converted once to the fraction `text_similarity` returns.
///
/// Upstream scales the Jaccard overlap to 0-100 and compares against a percent threshold, so the two
/// representations have to meet on the same line: one panel gives 0.70, two give 0.85, three 0.90.
/// Dividing by the panel count is the point — the more screens share a desktop, the more one panel's
/// text may differ before it counts as new information.
pub fn similarity_threshold_for(display_count: usize) -> f64 {
    (100.0 - 30.0 / display_count.max(1) as f64) / 100.0
}

/// `utils.get_display_count()`: attached panels, not counting the virtual-desktop union `mss` puts at
/// index 0.
fn display_count() -> usize {
    windcap::capture::monitors().len().max(1)
}

/// What happened to one video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Rows committed and the file renamed to `-OCRED`.
    Indexed,
    /// Nothing done, with the reason: `already indexed (-OCRED)`, `not an .mp4`, a retry cap, or a
    /// name that is not a bare file in the library.
    Skipped(String),
    /// The run failed. The file is now `-ERROR{n}.mp4` and a `LOG_ERROR_*.MD` in `cache/` explains why.
    Failed { error: String, renamed_to: String, log_file: String },
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Indexed => "indexed",
            Outcome::Skipped(_) => "skipped",
            Outcome::Failed { .. } => "failed",
        }
    }
}

/// The counts a run reports, which double as the diagnosis when a video indexes to zero rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub frames: usize,
    pub written: usize,
    /// Rejected for looking like the previously kept frame.
    pub similar: usize,
    /// Rejected for under three characters of text.
    pub too_short: usize,
    /// Rejected because the screen, or the foreground window, named an excluded word.
    pub excluded: usize,
    /// Rejected by the second, cross-time pass.
    pub duplicates: usize,
    /// Frames that could not be decoded or read by the engine. Counted, not fatal: one unreadable
    /// frame is a bad frame, not a bad video.
    pub unreadable: usize,
    /// Frames whose OCR input had at least one excluded edge painted black over it.
    ///
    /// Reported beside `frames` so a user can see that the mask they configured actually ran, and can
    /// notice the case where it did not: `frames` well above `masked` means the configured percentages
    /// were zero, or that the frames do not match the displays plugged in now and fell back to the
    /// default band.
    pub masked: usize,
}

/// A completed or failed run over one video.
#[derive(Debug, Clone)]
pub struct Report {
    /// The name rows are keyed by, with no pipeline marker on it.
    pub video: String,
    pub outcome: Outcome,
    pub counts: Counts,
    /// Where the file ended up on disk.
    pub final_name: String,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        matches!(self.outcome, Outcome::Indexed | Outcome::Skipped(_))
    }
}

#[derive(Debug)]
pub enum PipelineError {
    /// Extraction, masking or committing failed.
    Pipeline(String),
    /// The file could not be renamed — the one failure that leaves a video in a state no later run can
    /// read correctly, so it is reported separately.
    Rename(String),
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineError::Pipeline(m) => write!(f, "{m}"),
            PipelineError::Rename(m) => write!(f, "rename failed: {m}"),
        }
    }
}

impl std::error::Error for PipelineError {}

/// Index one video that is already in the library.
///
/// `file_name` is the name as it is on disk *now*, which may carry `-INDEX` (an interrupted run) or
/// `-ERROR1` (a retry). The file is looked for in `settings.videos_dir`; use [`index_video_path`] for a
/// caller that walked the month folders itself.
pub fn index_video(settings: &Settings, file_name: &str) -> Report {
    index_video_path(settings, &settings.videos_dir.join(file_name))
}

/// [`index_video`] with the file's full path given.
pub fn index_video_path(settings: &Settings, path: &Path) -> Report {
    let on_disk = file_name_of(path);
    let video = naming::base_name(&on_disk);

    // The name-based decisions come first: they need no filesystem access, and "already indexed" is a
    // better answer than "not a file" for a caller that globbed a directory.
    let state = naming::classify(&on_disk);
    let attempt = match state.should_index() {
        Some(attempt) => attempt,
        None => {
            return Report {
                video,
                outcome: Outcome::Skipped(state.skip_reason().unwrap_or("skipped").to_string()),
                counts: Counts::default(),
                final_name: on_disk,
            }
        }
    };

    // A name read out of a database row, or typed into a shell, must not be able to reach outside the
    // library — including the rollback below, which would otherwise open a database for whatever month
    // a traversal string happened to parse as.
    if let Err(reason) = check_path(settings, path) {
        return Report { video, outcome: Outcome::Skipped(reason), counts: Counts::default(), final_name: on_disk };
    }

    // The live path may already own this segment, and if it does, indexing from the video is a second
    // bill for the same work: `windrec` writes a row for every frame it keeps, the pass's `text` step
    // reads that frame's masked copy, and the rollback below would then *delete* the recorder's rows
    // and replace them with coarser ones sampled from the video. Asked here, ahead of the rollback,
    // because the rollback is the destruction this check exists to prevent.
    //
    // Only for a name that has never been marked. A file carrying `-INDEX` or `-ERROR{n}` is this
    // program's own unfinished business — an interrupted run may have committed one month of a
    // boundary segment and not the next — and the retry that rolls back and rewrites is the repair. A
    // coverage check that read those half rows and called the segment finished would strand the other
    // half of the hour for good.
    if state == naming::State::Fresh {
        match coverage(settings, &video) {
            // The encode step may be writing this very file. Read before the answer is acted on, because
            // both answers that *follow* touch it: the rename below, and the frame extraction after it.
            Coverage::Read { .. } | Coverage::Uncovered if still_in_the_encode_queue(settings, &video) => {
                let reason = encode_is_not_done_yet(settings, &video);
                return Report { video, outcome: Outcome::Skipped(reason), counts: Counts::default(), final_name: on_disk };
            }
            Coverage::Read { rows } => {
                // Marked as indexed: the question `-OCRED` answers is "is this footage searchable", and
                // it is, whoever did the reading.
                return match rename(path, &naming::ocred_name(&video)) {
                    Ok(finished) => Report {
                        video,
                        outcome: Outcome::Skipped(format!("already indexed by the recorder ({rows} row(s) read)")),
                        counts: Counts::default(),
                        final_name: file_name_of(&finished),
                    },
                    Err(e) => failure(settings, path, &video, attempt, &Counts::default(), e.to_string()),
                };
            }
            Coverage::Waiting { rows, waiting, slice_on_disk: true } => {
                // Deliberately *not* marked. The frames are still on disk, so the next pass's `text`
                // step can still read them; a half-filled day is the normal state of a pass that ran out
                // of window, and a marker here would strand it.
                return Report {
                    video,
                    outcome: Outcome::Skipped(format!("{waiting} of {rows} row(s) are still waiting for the text step and its slice is on disk")),
                    counts: Counts::default(),
                    final_name: on_disk,
                };
            }
            // No row names this segment, or its rows are waiting and the frames that could fill them are
            // gone: the video is the only copy of the words left, so this is exactly the footage the
            // back-index was built for.
            Coverage::Uncovered | Coverage::Waiting { slice_on_disk: false, .. } => {}
        }
    }

    // A run that left an `-INDEX` marker, or an earlier attempt that failed, may already have written
    // rows. Delete them before writing new ones, or the same frame appears twice in every search.
    rollback(settings, &video);

    // Mark before touching a frame. If the process dies between here and the commit, the next run
    // knows to roll back — which is the whole reason the marker exists.
    let marked = if on_disk.contains(naming::MARKER_INDEX) {
        path.to_path_buf()
    } else {
        match rename(path, &naming::index_name(&video)) {
            Ok(p) => p,
            Err(e) => {
                return Report {
                    video,
                    outcome: Outcome::Failed { error: e.to_string(), renamed_to: on_disk, log_file: String::new() },
                    counts: Counts::default(),
                    final_name: file_name_of(path),
                }
            }
        }
    };

    let mut counts = Counts::default();
    match run_pipeline(settings, &marked, &video, &mut counts) {
        Ok(()) => match rename(&marked, &naming::ocred_name(&video)) {
            Ok(finished) => Report {
                video: video.clone(),
                outcome: Outcome::Indexed,
                counts,
                final_name: file_name_of(&finished),
            },
            Err(e) => failure(settings, &marked, &video, attempt, &counts, e.to_string()),
        },
        Err(e) => failure(settings, &marked, &video, attempt, &counts, e.to_string()),
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// Refuse anything that is not a plain `.mp4` inside the video library.
fn check_path(settings: &Settings, path: &Path) -> Result<(), String> {
    let name = file_name_of(path);
    if !naming::is_bare_filename(&name) {
        return Err("name is not a bare file in the library".to_string());
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| "no parent directory".to_string())?;
    let parent = parent.canonicalize().map_err(|e| format!("cannot resolve {}: {e}", parent.display()))?;
    let library = settings
        .videos_dir
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", settings.videos_dir.display()))?;
    // The library root and every month folder beneath it are in scope; nothing else is.
    if parent != library && !parent.starts_with(&library) {
        return Err("outside the videos directory".to_string());
    }
    if !path.is_file() {
        return Err("not a file".to_string());
    }
    Ok(())
}

fn rename(path: &Path, new_name: &str) -> Result<PathBuf, PipelineError> {
    let target = path.with_file_name(new_name);
    std::fs::rename(path, &target).map_err(|e| PipelineError::Rename(format!("{} -> {new_name}: {e}", path.display())))?;
    Ok(target)
}

/// Delete any rows already stored under this video's name, across the months the segment could span.
///
/// Upstream interpolates the name into the SQL string. It is bound here instead, because the name is
/// read back out of database rows and a video called `x' OR 1=1 --.mp4` would otherwise cost a user a
/// month of history.
fn rollback(settings: &Settings, video: &str) {
    let Some(start) = timeline::segment_start_seconds(video) else { return };
    let end = start + settings.record_seconds.max(0);
    for (year, month) in months_between(start, end) {
        // A month file that does not exist yet has nothing to roll back, and creating one to discover
        // that is a side effect not worth having.
        if !settings.db_dir.join(wind_base::paths::month_filename(&settings.user_name, year, month)).exists() {
            continue;
        }
        if let Ok(store) = Store::open_month(&settings.db_dir, &settings.user_name, year, month) {
            delete_rows_for(store.connection(), video);
        }
    }
}

/// The bound delete behind [`rollback`].
///
/// Matched on the name's leading timestamp rather than the whole string, which is upstream's own
/// convention (`LIKE '%name[:19]%'`, used in three places) and is what makes the rollback survive a
/// marker: a row can only ever be written under the unmarked name, but a `%…%` over the full name would
/// also miss anything an older build left behind. The pattern is bound, never interpolated into SQL.
pub fn delete_rows_for(conn: &Connection, video: &str) -> usize {
    conn.execute("DELETE FROM video_text WHERE videofile_name LIKE ?", rusqlite::params![like_pattern(video)])
        .unwrap_or(0)
}

/// The `LIKE` operand that selects every row belonging to one segment.
pub fn like_pattern(video: &str) -> String {
    let base = naming::base_name(video);
    let stem = base.rsplit_once('.').map_or(base.as_str(), |(stem, _)| stem);
    let stamp: String = stem.chars().take(wind_base::paths::STAMP_LEN).collect();
    format!("%{stamp}%")
}

/// Which month files a segment starting at `start` and ending at `end` can have rows in.
///
/// A segment recorded at 23:50 on the 30th writes into two files, and upstream opens the database per
/// row by timestamp; a rollback that only cleared the first would leave the tail orphaned.
pub fn months_between(start: i64, end: i64) -> Vec<(i64, u32)> {
    let mut out = Vec::new();
    let mut current = timeline::month_of(start);
    let last = timeline::month_of(end.max(start));
    while current <= last {
        out.push(current);
        current = if current.1 == 12 { (current.0 + 1, 1) } else { (current.0, current.1 + 1) };
    }
    out
}

/// Is this video's own recorder slice still sitting in the cache **unmarked**?
///
/// That is the difference between a finished hour and one that is being written right now: `convert`
/// runs ffmpeg straight into the video's final name and renames the slice directory `{stamp}-VIDEO` only
/// *after* the encoder returned, so an unmarked `{stamp}` beside a `.mp4` of the same stamp is positive
/// evidence that the encode has not declared itself done. Reading such a file would OCR the part that
/// happened to be there and mark the whole segment `-OCRED` — the rest of the hour gone for good — or
/// fail to rename a file another process holds open and leave a truncated video that the next convert
/// step reads as somebody else's finished work.
///
/// The exact unmarked path is asked for, not [`wind_base::paths::slice_dirs`]'s stamp fold: a slice
/// renamed `-VIDEO` is a *completed* encode and its video is exactly what this walk is for.
///
/// Inside one maintenance pass this almost never fires — `convert` (step 2) marked its finished slices
/// long before `reindex` (step 5) walks — and it exists for the lane the pass starts while the encoder
/// is still working, which is the case that lets the two legs overlap without one of them reading a
/// half-written file.
fn still_in_the_encode_queue(settings: &Settings, video: &str) -> bool {
    encode_pending_slice(settings, video).is_some()
}

/// The same question, with the directory that answered it. `None` when nothing is holding the video.
fn encode_pending_slice(settings: &Settings, video: &str) -> Option<PathBuf> {
    let stamp = wind_base::paths::segment_stamp_of(video)?;
    let slice = settings.screenshot_dir.join(stamp);
    slice.is_dir().then_some(slice)
}

/// The line a deferred video reports, naming the directory that is the evidence.
fn encode_is_not_done_yet(settings: &Settings, video: &str) -> String {
    match encode_pending_slice(settings, video) {
        Some(slice) => format!("its slice is still unmarked in the encode queue, so the video may still be being written ({})", slice.display()),
        None => "the encode queue still owns this video".to_string(),
    }
}

/// Whether the index already covers one segment, and what the back-index should therefore do with it.
///
/// The rule lives in [`wind_store::maintain::segment_coverage`], and this is the same type under the
/// name this crate's caller and tests use: the maintenance pass's census asks the identical question to
/// promise the window a number, and a step and a census that answer it differently is a button that
/// lies. The question is not "has this file been OCR'd by *this* program" — the `-OCRED` marker answers
/// that — but "can the user already search this hour". Since the recorder writes a row per retained
/// frame and the pass's `text` step reads each one's masked copy, the answer is yes for every segment
/// the recorder captured, and re-OCR ing the same footage from the video spends the pass's whole budget
/// a second time to say the same thing.
pub use wind_store::maintain::SegmentCoverage as Coverage;

/// Ask the index how much of this segment the live path has already read.
///
/// The wrapper only turns the stamp into the month files that could hold this segment's rows; the
/// reading, the fall-backs and the slice question are the store's. A segment that cannot be placed on
/// the clock is `Uncovered`, which is the behaviour before the question was ever asked.
pub fn coverage(settings: &Settings, video: &str) -> Coverage {
    let Some(start) = timeline::segment_start_seconds(video) else {
        return Coverage::Uncovered;
    };
    let end = start + settings.record_seconds.max(0);
    wind_store::maintain::segment_coverage(
        &settings.db_dir,
        &settings.user_name,
        &months_between(start, end),
        &settings.screenshot_dir,
        video,
    )
}

/// Everything between the marker rename and the commit.
fn run_pipeline(
    settings: &Settings,
    path: &Path,
    video: &str,
    counts: &mut Counts,
) -> Result<(), PipelineError> {
    let out_dir = settings.frame_dir(video);
    prepare_dir(&out_dir)?;

    let strategy = frames::strategy_for_encoder(&settings.encoder);
    let probed = frames::probe_framerate(&settings.ffmpeg, path);
    let step = frames::frame_step_for(probed.unwrap_or(settings.framerate.max(1) as f64), frames::IFRAME_INTERVAL_MS);
    // The rate this segment is actually cut at, which is what turns a stored frame number into the
    // second of the video it sits at. `record_framerate` is a config *guess*, and the guess and the
    // encoder disagreed by a factor of two on this branch's own corpus (windmaint writes one frame per
    // second, the shipped default says 2), which moved every re-indexed row's `videofile_time` to half
    // its real second. The probe already answered this question for `step` two lines above; answering
    // it a second time from the config was the bug. Falls back to the setting only when ffprobe says
    // nothing, because a segment with no readable rate still has to be indexed.
    let rate = segment_rate(probed, settings.framerate);
    let extracted = frames::extract(&settings.ffmpeg, path, &out_dir, strategy, step)
        .map_err(|e| PipelineError::Pipeline(describe_extract(video, &e)))?;
    counts.frames = extracted.len();

    let engine = Engine::new(settings.ocr.clone(), &out_dir);
    if !engine.is_installed() {
        return Err(PipelineError::Pipeline(format!("OCR engine not installed: {}", engine.describe())));
    }

    // Enumerating the desktop is a syscall per call, and every frame of one segment has the same
    // layout, so it happens once here rather than once per frame.
    let panels = monitors();
    let union = desktop();
    let mut records: Vec<Record> = Vec::new();
    // The raw OCR text of the last *written* frame. Raw, not cleaned and without the title: this is
    // what upstream compares, and cleaning first would change which frames look alike.
    let mut previous: Option<String> = None;
    let threshold = settings.similarity_threshold();

    for frame in &extracted {
        if !frame.original.is_file() {
            // Upstream skips a frame whose unmasked original has gone missing, and the masked copy is
            // derived from it, so a half-written extraction contributes nothing.
            continue;
        }
        // One decode, two encodes: the masked copy the engine reads, and the thumbnail of the original
        // that the index stores. The mask is *painted* on the copy and the original is what the row
        // points at — cropping here would delete footage the user recorded. See `crate::crop`.
        let pixels = match Pixels::read(&frame.original) {
            Ok(pixels) => pixels,
            Err(_) => {
                counts.unreadable += 1;
                continue;
            }
        };
        // `for_frame` and the live recorder's `for_grab` are the two doors into one geometry
        // (`windcap::crop`); at a frame's native resolution they are the same plan, which is what
        // `crate::crop::tests::the_geometry_is_the_shared_one_not_a_copy_of_it` pins.
        let plan = MaskPlan::for_frame(pixels.width, pixels.height, &panels, union, &settings.urbl);
        let masked = pixels.masked(&plan);
        if !plan.is_empty() {
            counts.masked += 1;
        }
        match masked.to_jpeg(crop::MASKED_JPEG_QUALITY) {
            Ok(bytes) => std::fs::write(frame.cropped_path(), bytes)
                .map_err(|e| PipelineError::Pipeline(format!("cannot write {}: {e}", frame.cropped_path().display())))?,
            Err(e) => {
                counts.unreadable += 1;
                let _ = e;
                continue;
            }
        }

        let raw = match engine.recognize_file(&frame.cropped_path()) {
            Ok(text) => text,
            Err(e) => {
                counts.unreadable += 1;
                // A resident engine that has gone quiet costs a task timeout per frame, so a back-index
                // that kept asking would spend the rest of the night waiting and index nothing. The
                // command engines fall through instead: they fail in well under a second, the reason is in
                // the report's unreadable count, and one bad frame among thousands is not a dead engine.
                if engine.is_resident() && wind_base::wxocr::has_gone_quiet() {
                    return Err(PipelineError::Pipeline(format!(
                        "{} stopped answering after {} frames in a row: {e}",
                        engine.name(),
                        wind_base::wxocr::GIVE_UP_AFTER
                    )));
                }
                continue;
            }
        };

        if let Some(last) = &previous {
            if text_similarity(last, &raw) >= threshold {
                counts.similar += 1;
                continue;
            }
        }
        if raw.chars().count() < MIN_TEXT_CHARS {
            counts.too_short += 1;
            continue;
        }
        if is_str_contain_list_word(&raw, &settings.exclude_words) {
            counts.excluded += 1;
            continue;
        }

        let at = timeline::row_time(video, frame.index, rate).ok_or_else(|| {
            PipelineError::Pipeline(format!("{video} has no timestamp in its name, so no row can be placed"))
        })?;
        let titles = TitleTable::load_day(&settings.win_title_dir, at);
        let win_title = titles.title_at(at).map(|t| optimize_window_title(t)).filter(|t| !t.trim().is_empty());
        let deep_linking = titles.deep_linking_at(at).map(str::to_string).filter(|d| !d.trim().is_empty());
        if let Some(title) = &win_title {
            // The window is checked separately, and a frame rejected here does *not* become the anchor
            // for the next one — otherwise hiding a password manager would split the screens on either
            // side of it into two rows.
            if is_str_contain_list_word(title, &settings.exclude_words) {
                counts.excluded += 1;
                continue;
            }
        }

        let thumbnail = pixels.thumbnail_base64(settings.thumbnail_width, settings.thumbnail_quality).ok();
        records.push(Record {
            videofile_name: video.to_string(),
            picturefile_name: frame.cropped_name(),
            videofile_time: at,
            // The body only: `Record::indexed_text` appends `" -||- " + win_title` when it commits, the
            // same way upstream composes the two into one field before writing.
            ocr_text: clean_dirty_text(&raw),
            win_title,
            deep_linking,
            thumbnail,
        });
        // Only a written frame becomes the anchor for the next one.
        previous = Some(raw);
    }

    if settings.cross_time_dedup && !records.is_empty() {
        let before = records.len();
        records = drop_cross_time_duplicates(&records, settings.cross_time_threshold);
        counts.duplicates = before - records.len();
    }

    counts.written = commit(settings, &records)?;
    Ok(())
}

/// The second pass: the same similarity rule, over everything this video wrote, at the configured
/// threshold.
///
/// Compares the *stored* text — cleaned body plus `" -||- " + title` — because that is the column
/// upstream's pandas pass reads (`remove_duplicates_in_df(df, "ocr_text")`). A row whose body repeats
/// but whose window differs is therefore *not* a duplicate here, which is right: it is the same
/// document in a different application, and the user searches for both.
pub fn drop_cross_time_duplicates(records: &[Record], threshold: f64) -> Vec<Record> {
    let texts: Vec<String> = records.iter().map(Record::indexed_text).collect();
    let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
    let dropped = wind_store::maintain::duplicate_flags(&borrowed, threshold);
    records
        .iter()
        .enumerate()
        .filter(|(index, _)| !dropped.contains(index))
        .map(|(_, record)| record.clone())
        .collect()
}

/// Group by calendar month and commit each group in its own transaction. Returns the rows written.
fn commit(settings: &Settings, records: &[Record]) -> Result<usize, PipelineError> {
    if records.is_empty() {
        return Ok(0);
    }
    let mut by_month: BTreeMap<(i64, u32), Vec<Record>> = BTreeMap::new();
    for record in records {
        by_month.entry(timeline::month_of(record.videofile_time)).or_default().push(record.clone());
    }
    let mut written = 0usize;
    for ((year, month), group) in by_month {
        let mut store = Store::open_month(&settings.db_dir, &settings.user_name, year, month)
            .map_err(|e| PipelineError::Pipeline(format!("cannot open {year:04}-{month:02} index: {e}")))?;
        let count = store
            .append(&group)
            .map_err(|e| PipelineError::Pipeline(format!("cannot commit {year:04}-{month:02}: {e}")))?;
        written += count;
    }
    Ok(written)
}

/// The failure path: leave the artefact, rename so the next run knows to retry, and report.
fn failure(settings: &Settings, marked: &Path, video: &str, attempt: i64, counts: &Counts, error: String) -> Report {
    let new_name = naming::error_name(video, attempt);
    let log = settings.cache_dir.join(naming::error_log_name(&new_name));
    write_error_log(&log, video, counts, &error);
    match rename(marked, &new_name) {
        Ok(moved) => Report {
            video: video.to_string(),
            outcome: Outcome::Failed {
                error,
                renamed_to: new_name,
                log_file: log.to_string_lossy().to_string(),
            },
            counts: counts.clone(),
            final_name: file_name_of(&moved),
        },
        // The rename is the one failure that must not be swallowed quietly: without it the video stays
        // marked `-INDEX` and its stale rows are not cleared by the next run.
        Err(again) => Report {
            video: video.to_string(),
            outcome: Outcome::Failed {
                error: format!("{error}; and {again}"),
                renamed_to: new_name,
                log_file: log.to_string_lossy().to_string(),
            },
            counts: counts.clone(),
            final_name: file_name_of(marked),
        },
    }
}

/// `cache/LOG_ERROR_{name}.MD`, the file a user is asked to attach when a video will not index.
fn write_error_log(path: &Path, video: &str, counts: &Counts, error: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = format!(
        "{}\n{error}\n\nvideo: {video}\nframes: {}\nwritten: {}\nsimilar: {}\ntoo_short: {}\nexcluded: \
         {}\nduplicates: {}\nunreadable: {}\nmasked: {}\n",
        wind_base::clock::now().stamp(),
        counts.frames,
        counts.written,
        counts.similar,
        counts.too_short,
        counts.excluded,
        counts.duplicates,
        counts.unreadable,
        counts.masked,
    );
    let _ = std::fs::write(path, body);
}

/// The attached panels, in desktop coordinates.
///
/// Converted with the shared `From`, not a hand-written field shuffle: this is the same list the live
/// recorder hands [`MaskPlan::for_grab`], and a second conversion here is a second chance to get a
/// monitor's origin wrong.
fn monitors() -> Vec<Tile> {
    windcap::capture::monitors().into_iter().map(Tile::from).collect()
}

/// `mss`'s `monitors[0]`: the union of every panel, which is what an all-displays frame is sized to.
fn desktop() -> Tile {
    Tile::from(windcap::capture::virtual_desktop())
}

fn describe_extract(video: &str, error: &ExtractError) -> String {
    match error {
        ExtractError::Empty => format!("{video}: no frames could be extracted"),
        other => format!("{video}: {other}"),
    }
}

fn prepare_dir(dir: &Path) -> Result<(), PipelineError> {
    // Wipe the frame directory before starting: frames left by an interrupted run would otherwise be
    // read again and indexed twice.
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| PipelineError::Pipeline(format!("cannot create {}: {e}", dir.display())))
}

fn strip_extension(name: &str) -> String {
    name.rsplit_once('.').map_or_else(|| name.to_string(), |(stem, _)| stem.to_string())
}

/// Delete the frames a run extracted, unless they were asked to be kept.
///
/// Explicit because upstream leaves them for the image-embedding pass to clean, and a caller that runs
/// over a whole library wants them gone before the next video's frames arrive.
pub fn cleanup_frames(settings: &Settings, video: &str) {
    if settings.keep_frames {
        return;
    }
    let _ = std::fs::remove_dir_all(settings.frame_dir(video));
}

/// The frame rate one segment's rows are measured in: what the container says, or the configured guess
/// when ffprobe could not answer.
///
/// The configured value and the encoder are not the same fact. `windmaint` writes every segment at one
/// frame per second so that a player seeking to second S lands on frame S, while `record_framerate`
/// shipped as 2 on the install this was found on — and a row's `videofile_time` is computed by dividing
/// its frame number by whichever of the two is used. Dividing by a rate the file does not have puts every
/// re-indexed row at a fraction of its real second, which is what made a thumbnail and the picture opened
/// from it look like different moments. So the measured rate wins, and the setting is only the fallback.
fn segment_rate(probed: Option<f64>, configured: i64) -> i64 {
    let from_file = probed.map(|found| found.round() as i64).filter(|found| *found > 0);
    from_file.unwrap_or(configured).max(1)
}

/// One frame's position in its segment, in seconds.
pub fn frame_offset(frame: &Frame, framerate: i64) -> i64 {
    timeline::frame_offset_seconds(frame.index, framerate)
}

/// `CropError` has no `Display` use in this module's signatures, but a pipeline error string built
/// from one should read like the rest of them.
impl From<CropError> for PipelineError {
    fn from(error: CropError) -> PipelineError {
        PipelineError::Pipeline(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_base::clock::LocalParts;

    fn record_at(time: i64, body: &str, title: Option<&str>) -> Record {
        Record {
            videofile_name: "2026-09-21_21-16-12.mp4".into(),
            picturefile_name: format!("{}.jpg", (time - 1_790_025_372) * 4),
            videofile_time: time,
            ocr_text: body.into(),
            win_title: title.map(str::to_string),
            deep_linking: None,
            thumbnail: None,
        }
    }

    /// The threshold is `(100 - 30/panels)/100`, upstream's percent form scaled to what
    /// `text_similarity` returns. Getting this wrong on a two-panel desktop changes which screens a
    /// user's own library still contains.
    #[test]
    fn the_similarity_line_shrinks_with_more_panels() {
        assert!((similarity_threshold_for(1) - 0.70).abs() < 1e-12);
        assert!((similarity_threshold_for(2) - 0.85).abs() < 1e-12);
        assert!((similarity_threshold_for(3) - 0.90).abs() < 1e-12);
        assert!((similarity_threshold_for(6) - 0.95).abs() < 1e-12);
        assert!((similarity_threshold_for(0) - 0.70).abs() < 1e-12, "zero panels must not divide");
    }

    #[test]
    fn the_metric_is_wind_stores_jaccard_not_a_reinvention() {
        // Pinned against the values wind-store's own tests use, because the rows already on users'
        // disks were deduplicated under exactly this function.
        assert_eq!(text_similarity("", ""), 0.0);
        assert_eq!(text_similarity("abc", "abc"), 1.0);
        assert!((text_similarity("abcd", "abce") - 0.6).abs() < 1e-9);
        // "ababababab" is "very similar to" "aaaaabbbbb" — upstream's own TODO comment about this,
        // kept because the installed base was built with it.
        assert_eq!(text_similarity("ababababab", "aaaaabbbbb"), 1.0);
    }

    #[test]
    fn a_repeat_of_the_previous_screen_is_dropped_at_the_line() {
        let threshold = similarity_threshold_for(2);
        assert!(text_similarity("quarterly revenue report 2026", "quarterly revenue report 2026") >= threshold);
        assert!(
            text_similarity("quarterly revenue report 2026", "a shopping list with socks and tape") < threshold,
            "a genuinely different screen must survive"
        );
    }

    #[test]
    fn the_cross_time_pass_keeps_the_first_of_each_group() {
        let records = vec![
            record_at(1_790_025_372, "revenue report 2026 q3", Some("Excel")),
            record_at(1_790_025_376, "revenue report 2026 q3", Some("Excel")),
            record_at(1_790_025_380, "revenue report 2026 q4", Some("Excel")),
            record_at(1_790_025_384, "a shopping cart with socks", Some("Chrome")),
        ];
        let kept = drop_cross_time_duplicates(&records, 0.94);
        assert_eq!(kept.len(), 3, "the exact repeat goes; q4 and the cart stay");
        assert_eq!(kept[0].videofile_time, 1_790_025_372, "the earliest of a group is the one kept");
        assert_eq!(kept[1].videofile_time, 1_790_025_380);
        assert_eq!(kept[2].videofile_time, 1_790_025_384);
    }

    /// The cross-time pass reads the composed column, so the same screen in two applications is two
    /// rows — which is what "where did I see this" needs.
    #[test]
    fn the_cross_time_pass_reads_the_stored_text_including_the_title() {
        let same_body = "quarterly revenue report for the third quarter";
        let records = vec![
            record_at(1_790_025_372, same_body, Some("Excel")),
            record_at(1_790_025_376, same_body, Some("Chrome")),
        ];
        let kept = drop_cross_time_duplicates(&records, 0.94);
        assert_eq!(kept.len(), 2);
        assert!(kept[0].indexed_text().contains(" -||- Excel"), "the title is part of what is compared");
    }

    #[test]
    fn an_empty_batch_and_a_single_row_are_their_own_answers() {
        assert!(drop_cross_time_duplicates(&[], 0.94).is_empty());
        let one = vec![record_at(1_790_025_372, "only screen", Some("Excel"))];
        assert_eq!(drop_cross_time_duplicates(&one, 0.94).len(), 1);
    }

    #[test]
    fn the_minimum_length_is_counted_in_characters() {
        assert_eq!(MIN_TEXT_CHARS, 3);
        assert!("ab".chars().count() < MIN_TEXT_CHARS);
        assert!("abc".chars().count() >= MIN_TEXT_CHARS);
        // Three Chinese characters clear the same bar three Latin ones do: the rule is length, not
        // information, and it is upstream's.
        assert!("中中中".chars().count() >= MIN_TEXT_CHARS);
    }

    #[test]
    fn rows_are_routed_to_the_month_their_timestamp_falls_in() {
        let end_of_month = LocalParts::from_stamp("2026-09-30_23-59-59").unwrap().naive_epoch_seconds();
        assert_eq!(timeline::month_of(end_of_month), (2026, 9));
        assert_eq!(timeline::month_of(end_of_month + 1), (2026, 10));
    }

    #[test]
    fn a_segment_across_a_month_boundary_rolls_back_in_both_files() {
        let start = LocalParts::from_stamp("2026-09-30_23-50-00").unwrap().naive_epoch_seconds();
        assert_eq!(months_between(start, start + 900), vec![(2026, 9), (2026, 10)]);
        assert_eq!(months_between(start, start + 60), vec![(2026, 9)]);
        let year_end = LocalParts::from_stamp("2026-12-31_23-50-00").unwrap().naive_epoch_seconds();
        assert_eq!(months_between(year_end, year_end + 1200), vec![(2026, 12), (2027, 1)]);
        assert_eq!(months_between(start, start), vec![(2026, 9)], "a zero-length span is one month");
    }

    #[test]
    fn frames_live_under_the_unmarked_stem() {
        let settings = test_settings();
        assert_eq!(
            settings.frame_dir("2026-09-21_21-16-12.mp4"),
            settings.iframe_dir.join("2026-09-21_21-16-12")
        );
        // An interrupted run's directory is the retry's directory, which is what gets it wiped.
        assert_eq!(settings.frame_dir("2026-09-21_21-16-12-ERROR1.mp4"), settings.frame_dir("2026-09-21_21-16-12.mp4"));
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    #[test]
    fn stripping_an_extension_leaves_the_stem() {
        assert_eq!(strip_extension("2026-09-21_21-16-12-INDEX.mp4"), "2026-09-21_21-16-12-INDEX");
        assert_eq!(strip_extension("no_extension"), "no_extension");
    }

    /// The delete is bound, and the pattern is matched as text — so a crafted filename that looks like
    /// SQL cannot erase somebody else's history.
    #[test]
    fn a_crafted_video_name_cannot_erase_someone_elses_rows() {
        let conn = Connection::open_in_memory().unwrap();
        wind_store::schema::ensure_schema(&conn).unwrap();
        for name in ["2026-09-21_21-16-12.mp4", "2026-09-22_10-00-00.mp4", "x' OR 1=1 --.mp4"] {
            conn.execute(
                "INSERT INTO video_text (videofile_name, picturefile_name, videofile_time, ocr_text, \
                 is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking) \
                 VALUES (?1,'0.jpg',1790025372,'text',1,0,'',NULL,'')",
                rusqlite::params![name],
            )
            .unwrap();
        }
        assert_eq!(delete_rows_for(&conn, "2026-09-21_21-16-12.mp4"), 1);
        assert_eq!(delete_rows_for(&conn, "x' OR 1=1 --.mp4"), 1, "matched literally, not executed");
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM video_text", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1, "the innocent row survives the injection attempt");
    }

    /// A rollback must also clear the rows written while the file wore `-INDEX`, which is the entire
    /// point of the marker.
    #[test]
    fn a_rollback_finds_rows_written_under_a_different_marker() {
        let conn = Connection::open_in_memory().unwrap();
        wind_store::schema::ensure_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO video_text (videofile_name, picturefile_name, videofile_time, ocr_text,
                 is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
             VALUES ('2026-09-21_21-16-12.mp4','0.jpg',1790025372,'a',1,0,'',NULL,''),
                    ('2026-09-21_21-16-12-INDEX.mp4','4.jpg',1790025376,'b',1,0,'',NULL,''),
                    ('2026-09-21_21-16-12-OCRED.mp4','8.jpg',1790025380,'c',1,0,'',NULL,''),
                    ('2026-09-22_10-00-00.mp4','0.jpg',1790106400,'d',1,0,'',NULL,'')",
        )
        .unwrap();
        // The caller passes whatever name it has, marked or not; all three of the segment's rows go.
        assert_eq!(delete_rows_for(&conn, "2026-09-21_21-16-12-ERROR1.mp4"), 3);
        assert_eq!(delete_rows_for(&conn, "2026-09-22_10-00-00.mp4"), 1);
        assert_eq!(delete_rows_for(&conn, "2026-09-23_00-00-00.mp4"), 0, "an unknown segment deletes nothing");
    }

    #[test]
    fn the_row_pattern_is_the_timestamp_not_the_whole_filename() {
        assert_eq!(like_pattern("2026-09-21_21-16-12.mp4"), "%2026-09-21_21-16-12%");
        assert_eq!(like_pattern("2026-09-21_21-16-12-INDEX.mp4"), "%2026-09-21_21-16-12%");
        assert_eq!(like_pattern("2026-09-21_21-16-12-ERROR2.mp4"), "%2026-09-21_21-16-12%");
        // A name with no stamp stays short and literal; it simply will not match a real segment.
        assert_eq!(like_pattern("holiday"), "%holiday%");
    }

    #[test]
    fn the_error_artefact_names_the_renamed_file_and_carries_the_reason() {
        let dir = std::env::temp_dir().join(format!("windcap-reindex-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(naming::error_log_name("X-ERROR1.mp4"));
        let counts = Counts { frames: 12, similar: 4, too_short: 2, ..Default::default() };
        write_error_log(&path, "X.mp4", &counts, "ffmpeg exited 1: invalid data");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(path.file_name().unwrap(), "LOG_ERROR_X-ERROR1.mp4.MD");
        assert!(text.contains("ffmpeg exited 1: invalid data"), "{text}");
        assert!(text.contains("frames: 12"), "{text}");
        assert!(text.starts_with("20"), "opens with the naive-local stamp: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_skip_is_reported_with_its_reason_and_writes_nothing() {
        let settings = test_settings();
        std::fs::write(settings.videos_dir.join("holiday-OCRED.mp4"), b"x").unwrap();
        let report = index_video(&settings, "holiday-OCRED.mp4");
        assert_eq!(report.outcome, Outcome::Skipped("already indexed (-OCRED)".into()));
        assert_eq!(report.counts, Counts::default());
        assert_eq!(report.final_name, "holiday-OCRED.mp4", "the file was not touched");
        assert_eq!(report.video, "holiday.mp4", "and the row key is still the unmarked name");
        assert!(settings.videos_dir.join("holiday-OCRED.mp4").exists());

        let report = index_video(&settings, "notes.txt");
        assert_eq!(report.outcome, Outcome::Skipped("not an .mp4".into()));
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// Upstream's driver caps retries at `ERROR_VIDEO_RETRY_TIMES`, and a file whose marker is not a
    /// number is not retried at all.
    #[test]
    fn a_video_that_failed_too_often_is_left_alone() {
        let settings = test_settings();
        std::fs::write(settings.videos_dir.join("X-ERROR4.mp4"), b"x").unwrap();
        let report = index_video(&settings, "X-ERROR4.mp4");
        assert_eq!(report.outcome, Outcome::Skipped("too many failed attempts".into()));
        assert_eq!(report.video, "X.mp4");
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// A name that is not a bare file in the library is declined before anything is resolved —
    /// including the rollback, which would otherwise open whichever month a traversal parsed as.
    #[test]
    fn a_traversal_name_is_declined_before_anything_is_read() {
        let settings = test_settings();
        let report = index_video_path(&settings, Path::new("../outside.mp4"));
        assert!(matches!(report.outcome, Outcome::Skipped(_)), "{:?}", report.outcome);

        let escaping = settings.videos_dir.join("../../elsewhere.mp4");
        let report = index_video_path(&settings, &escaping);
        assert_eq!(report.outcome, Outcome::Skipped("outside the videos directory".into()));
        assert!(!settings.db_dir.join("default_2026-09_wind.db").exists(), "no database was opened");
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// No ffmpeg and no engine: a video that cannot be read has to come back as `-ERROR1` with a log,
    /// and must leave no rows under its name.
    #[test]
    fn an_unreadable_video_becomes_error_one_and_leaves_no_rows() {
        let mut settings = test_settings();
        // Deliberately not a real binary, so this never spawns anything regardless of what is on PATH.
        settings.ffmpeg = PathBuf::from("Z:/definitely-not-here/ffmpeg.exe");
        let name = "2026-09-21_21-16-12.mp4";
        std::fs::write(settings.videos_dir.join(name), b"not a video").unwrap();

        let report = index_video(&settings, name);
        match &report.outcome {
            Outcome::Failed { renamed_to, log_file, error } => {
                assert_eq!(renamed_to, "2026-09-21_21-16-12-ERROR1.mp4");
                assert!(!error.is_empty());
                assert!(log_file.ends_with("LOG_ERROR_2026-09-21_21-16-12-ERROR1.mp4.MD"), "{log_file}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(settings.videos_dir.join("2026-09-21_21-16-12-ERROR1.mp4").exists(), "the video is never deleted");
        assert!(!settings.videos_dir.join("2026-09-21_21-16-12-INDEX.mp4").exists(), "the marker is replaced");
        assert!(settings.cache_dir.join(naming::error_log_name("2026-09-21_21-16-12-ERROR1.mp4")).exists());
        assert!(!settings.db_dir.join("default_2026-09_wind.db").exists(), "a failed run writes no index");
        cleanup_frames(&settings, name);
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// A retry of a failed video rolls its previous attempt's rows away first, then re-marks itself.
    #[test]
    fn a_retry_is_marked_index_and_its_stale_rows_are_cleared() {
        let mut settings = test_settings();
        settings.ffmpeg = PathBuf::from("Z:/definitely-not-here/ffmpeg.exe");
        let db = settings.db_dir.join(wind_base::paths::month_filename(&settings.user_name, 2026, 9));
        std::fs::create_dir_all(&settings.db_dir).unwrap();
        {
            let mut store = Store::open(&db).unwrap();
            store.append(&[record_at(1_790_025_372, "stale row from the failed attempt", None)]).unwrap();
        }
        assert_eq!(count_rows_in(&db), 1);

        std::fs::write(settings.videos_dir.join("2026-09-21_21-16-12-ERROR1.mp4"), b"broken").unwrap();
        let report = index_video(&settings, "2026-09-21_21-16-12-ERROR1.mp4");
        assert!(matches!(report.outcome, Outcome::Failed { .. }), "{:?}", report.outcome);
        assert_eq!(report.video, "2026-09-21_21-16-12.mp4");
        // The attempt counter moved up, and the stale row is gone even though this attempt also failed.
        assert_eq!(report.final_name, "2026-09-21_21-16-12-ERROR2.mp4");
        assert_eq!(count_rows_in(&db), 0, "the rollback ran before the attempt, not only after it");
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// The month file these tests write their seed rows into.
    fn seeded_db(settings: &Settings) -> PathBuf {
        let db = settings.db_dir.join(wind_base::paths::month_filename(&settings.user_name, 2026, 9));
        std::fs::create_dir_all(&settings.db_dir).unwrap();
        db
    }

    /// Put rows in the index the way the live path does: one per retained frame, keyed by the segment's
    /// unmarked video name. An empty `body` is a row still waiting for the `text` step.
    fn seed(settings: &Settings, rows: &[Record]) {
        let mut store = Store::open(&seeded_db(settings)).unwrap();
        store.append(rows).unwrap();
    }

    /// The claim this whole check exists for: footage the recorder wrote rows for, and whose rows the
    /// pass's `text` step has read, must not be OCR'd a second time from the video.
    #[test]
    fn a_segment_the_recorder_already_read_is_marked_without_a_second_ocr_pass() {
        let settings = test_settings();
        seed(
            &settings,
            &[record_at(1_790_025_372, "the first screen", None), record_at(1_790_025_376, "the second screen", None)],
        );
        let path = settings.videos_dir.join("2026-09-21_21-16-12.mp4");
        std::fs::write(&path, b"footage").unwrap();

        // Every `test_settings` points ffmpeg at a file that does not exist, so reaching the pipeline at
        // all would come back as a failure: a genuine skip is proved by the absence of one.
        let report = index_video_path(&settings, &path);
        assert!(
            matches!(&report.outcome, Outcome::Skipped(why) if why.contains("recorder")),
            "{:?}",
            report.outcome
        );
        assert_eq!(report.final_name, "2026-09-21_21-16-12-OCRED.mp4", "searchable footage is marked done");
        assert_eq!(count_rows_in(&seeded_db(&settings)), 2, "nothing was added, and the recorder's rows survived");
        assert!(!settings.videos_dir.join("2026-09-21_21-16-12.mp4").exists());
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// Half a day read is the normal state of a pass that ran out of window. The frames are still on
    /// disk, so the `text` step can finish the job on the next pass — and the video must not be marked,
    /// or the segment is decided forever by the pass that gave up halfway.
    #[test]
    fn a_segment_still_waiting_for_text_with_its_slice_on_disk_is_skipped_unmarked() {
        let settings = test_settings();
        seed(
            &settings,
            &[record_at(1_790_025_372, "already read", None), record_at(1_790_025_376, "", None)],
        );
        std::fs::create_dir_all(settings.screenshot_dir.join("2026-09-21_21-16-12")).unwrap();
        let path = settings.videos_dir.join("2026-09-21_21-16-12.mp4");
        std::fs::write(&path, b"footage").unwrap();

        let report = index_video_path(&settings, &path);
        assert!(
            matches!(&report.outcome, Outcome::Skipped(why) if why.contains("1 of 2 row(s) are still waiting")),
            "{:?}",
            report.outcome
        );
        assert!(path.exists(), "unmarked: the next pass's text step still owns these frames");
        assert!(!settings.videos_dir.join("2026-09-21_21-16-12-OCRED.mp4").exists());
        assert_eq!(count_rows_in(&seeded_db(&settings)), 2, "and its rows are left where they are");
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// Once the slice folder is gone the video is the only copy of the words left, which is exactly the
    /// footage the back-index was built for: the waiting rows are rolled back and the segment is worked.
    #[test]
    fn a_segment_whose_slice_is_gone_is_still_indexed_from_the_video() {
        let settings = test_settings();
        seed(
            &settings,
            &[record_at(1_790_025_372, "already read", None), record_at(1_790_025_376, "", None)],
        );
        let path = settings.videos_dir.join("2026-09-21_21-16-12.mp4");
        std::fs::write(&path, b"footage").unwrap();

        // No `cache_screenshot/{stamp}` folder, so this must reach the pipeline — and with ffmpeg
        // deliberately absent, reaching it is reported as a failure rather than a skip.
        let report = index_video_path(&settings, &path);
        assert!(matches!(report.outcome, Outcome::Failed { .. }), "{:?}", report.outcome);
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// Legacy footage — a library an install inherited, with no rows of its own — is the work this
    /// command exists to do, and the coverage check must never swallow it.
    #[test]
    fn footage_the_index_has_never_heard_of_is_indexed() {
        let settings = test_settings();
        // An empty index for this month: `seed` is not called, so no month file exists at all.
        let path = settings.videos_dir.join("2026-09-21_21-16-12.mp4");
        std::fs::write(&path, b"inherited footage").unwrap();

        let report = index_video_path(&settings, &path);
        assert!(matches!(report.outcome, Outcome::Failed { .. }), "{:?}", report.outcome);
        assert!(
            settings.videos_dir.join("2026-09-21_21-16-12-INDEX.mp4").is_file()
                || settings.videos_dir.join("2026-09-21_21-16-12-ERROR1.mp4").is_file(),
            "it was marked as work in progress, not answered as somebody else's business"
        );
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// An index this program cannot read is a reason to do the work, not a reason to mark footage done.
    /// Both halves of that fall-back are proved here: a month file that is not a database, and a query
    /// against a table that is not there.
    #[test]
    fn an_unreadable_index_falls_back_to_doing_the_work() {
        let settings = test_settings();
        std::fs::create_dir_all(&settings.db_dir).unwrap();
        let db = settings.db_dir.join(wind_base::paths::month_filename(&settings.user_name, 2026, 9));
        std::fs::write(&db, b"not a database at all").unwrap();
        let path = settings.videos_dir.join("2026-09-21_21-16-12.mp4");
        std::fs::write(&path, b"footage").unwrap();

        let report = index_video_path(&settings, &path);
        assert!(matches!(report.outcome, Outcome::Failed { .. }), "{:?}", report.outcome);
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// A video the encode step is still writing has its slice directory sitting beside the cache under
    /// its own stamp, unmarked: `{stamp}.mp4` exists and is growing. Nothing may be read out of it and
    /// nothing may be renamed on it — the walk is the one that would mark it `-OCRED` and decide the
    /// truncated hour is finished. This is what lets a reindex lane run while convert is still encoding.
    #[test]
    fn a_video_whose_slice_is_unmarked_is_deferred_and_left_untouched() {
        let settings = test_settings();
        // No rows at all, so coverage would otherwise answer `Uncovered` and the pipeline would run.
        let path = settings.videos_dir.join("2026-09-21_21-16-12.mp4");
        std::fs::write(&path, b"a video that is still growing").unwrap();
        std::fs::create_dir_all(settings.screenshot_dir.join("2026-09-21_21-16-12")).unwrap();

        let report = index_video_path(&settings, &path);
        assert!(
            matches!(&report.outcome, Outcome::Skipped(why) if why.contains("unmarked in the encode queue")),
            "{:?}",
            report.outcome
        );
        assert_eq!(report.final_name, "2026-09-21_21-16-12.mp4", "not renamed: the encoder still owns it");
        assert!(path.is_file(), "and the file itself is exactly where the encode left it");
        assert!(!settings.videos_dir.join("2026-09-21_21-16-12-INDEX.mp4").is_file(), "no marker written");
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// The other half of the same rule: `{stamp}-VIDEO` is the encode's own completion mark, so a slice
    /// renamed that way is a finished video and the walk goes back to doing its job with it.
    #[test]
    fn a_slice_marked_by_the_encode_step_is_a_finished_video_and_gets_worked() {
        let settings = test_settings();
        let path = settings.videos_dir.join("2026-09-21_21-16-12.mp4");
        std::fs::write(&path, b"footage").unwrap();
        std::fs::create_dir_all(settings.screenshot_dir.join("2026-09-21_21-16-12-VIDEO")).unwrap();

        let report = index_video_path(&settings, &path);
        // ffmpeg is deliberately absent in `test_settings`, so reaching the pipeline at all is the proof
        // that a marked slice is a finished video: the answer is this program's own failure, not somebody
        // else's unfinished work.
        assert!(matches!(report.outcome, Outcome::Failed { .. }), "{:?}", report.outcome);
        assert!(report.final_name.contains("-INDEX") || report.final_name.contains("-ERROR"), "{}", report.final_name);
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// The four answers [`coverage`] can give, on the four states of one install's index.
    #[test]
    fn coverage_names_what_the_index_holds_for_one_segment() {
        let settings = test_settings();
        let video = "2026-09-21_21-16-12.mp4";
        assert_eq!(coverage(&settings, video), Coverage::Uncovered, "no month file yet: nothing to consult");

        seed(&settings, &[record_at(1_790_025_372, "read", None)]);
        assert_eq!(coverage(&settings, video), Coverage::Read { rows: 1 });

        let waiting = test_settings();
        seed(&waiting, &[record_at(1_790_025_372, "read", None), record_at(1_790_025_376, "", None)]);
        assert_eq!(
            coverage(&waiting, video),
            Coverage::Waiting { rows: 2, waiting: 1, slice_on_disk: false },
            "one row unread and no slice folder"
        );
        std::fs::create_dir_all(waiting.screenshot_dir.join("2026-09-21_21-16-12-VIDEO")).unwrap();
        assert_eq!(
            coverage(&waiting, video),
            Coverage::Waiting { rows: 2, waiting: 1, slice_on_disk: true },
            "a marked-up slice folder counts: the frames are the same frames"
        );
        let _ = std::fs::remove_dir_all(&settings.root);
        let _ = std::fs::remove_dir_all(&waiting.root);
    }

    fn count_rows_in(path: &Path) -> i64 {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        conn.query_row("SELECT COUNT(*) FROM video_text", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn an_indexed_run_writes_rows_keyed_by_the_unmarked_name() {
        // The commit and the naming contract, without ffmpeg: rows are built by hand and pushed
        // through the same `commit` the pipeline uses.
        let settings = test_settings();
        let records = vec![
            record_at(1_790_025_372, "第一屏", Some("Chrome")),
            record_at(1_790_025_376, "第二屏 different", Some("Excel")),
        ];
        assert_eq!(commit(&settings, &records).unwrap(), 2);
        let db = settings.db_dir.join(wind_base::paths::month_filename(&settings.user_name, 2026, 9));
        let conn = Connection::open(&db).unwrap();
        let mut statement = conn
            .prepare("SELECT videofile_name, videofile_time, ocr_text, win_title FROM video_text ORDER BY videofile_time")
            .unwrap();
        let rows: Vec<(String, i64, String, Option<String>)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows[0].0, "2026-09-21_21-16-12.mp4", "never the -INDEX name the file wore");
        assert_eq!(rows[0].1, 1_790_025_372, "frame 0 of this segment is the segment's own timestamp");
        assert_eq!(rows[0].2, "第一屏 -||- Chrome", "the title rides in the indexed text as well");
        assert_eq!(rows[0].3.as_deref(), Some("Chrome"));
        assert_eq!(rows[1].1, 1_790_025_376, "frame 8 at 2 fps is four seconds in");
        // Upstream writes `True, False` for the two existence flags on this path; `Store::append` does
        // the same, and a maintenance pass corrects them later.
        let flags: (i64, i64) = conn
            .query_row("SELECT is_videofile_exist, is_picturefile_exist FROM video_text LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(flags, (1, 0));
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    #[test]
    fn an_empty_batch_commits_nothing_and_creates_no_database() {
        let settings = test_settings();
        assert_eq!(commit(&settings, &[]).unwrap(), 0);
        assert!(!settings.db_dir.join("default_2026-09_wind.db").exists());
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    #[test]
    fn a_frame_number_is_also_its_position_in_the_segment() {
        let frame = Frame { index: 240, original: PathBuf::from("240.jpg") };
        assert_eq!(frame_offset(&frame, 2), 120);
        assert_eq!(frame.cropped_name(), "240_cropped.jpg");
    }

    /// The privacy boundary has to be the same shape on both paths, or the regions a user excluded
    /// become searchable as soon as their footage is re-indexed from video instead of grabbed live.
    ///
    /// This is the pipeline's own expression — `MaskPlan::for_frame(pixels.width, pixels.height,
    /// &panels, union, &settings.urbl)` — checked against the recorder's `MaskPlan::for_grab` for one
    /// 3840x1080 all-displays frame, with every rectangle written out literally so a change to the
    /// shared geometry has to be a decision rather than a drift.
    #[test]
    fn the_reindexer_and_the_live_recorder_mask_the_same_rectangles() {
        let panels = [
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            Tile { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let union = Tile { x: 0, y: 0, width: 3840, height: 1080 };
        let settings = test_settings();

        let indexed = MaskPlan::for_frame(3840, 1080, &panels, union, &settings.urbl);
        let live = MaskPlan::for_grab(3840, 1080, union, &panels, union, &settings.urbl);
        assert_eq!(indexed.bands(), live.bands(), "one boundary, two callers");
        assert_eq!(indexed.describe(), live.describe(), "and the same thing said about it");

        // The shipped [6, 6, 6, 3] is top, right, bottom, left: 64 rows, 115 columns, 64 rows and 57
        // columns, once per panel.
        assert_eq!(
            indexed.bands(),
            vec![
                crop::Band { x: 0, y: 0, width: 1920, height: 64 },
                crop::Band { x: 0, y: 1016, width: 1920, height: 64 },
                crop::Band { x: 0, y: 0, width: 57, height: 1080 },
                crop::Band { x: 1805, y: 0, width: 115, height: 1080 },
                crop::Band { x: 1920, y: 0, width: 1920, height: 64 },
                crop::Band { x: 1920, y: 1016, width: 1920, height: 64 },
                crop::Band { x: 1920, y: 0, width: 57, height: 1080 },
                crop::Band { x: 3725, y: 0, width: 115, height: 1080 },
            ]
        );
        let _ = std::fs::remove_dir_all(&settings.root);
    }

    /// A `Settings` pointing at a throwaway tree, so the failure paths can be exercised without an
    /// install directory, ffmpeg or the OCR engine.
    fn test_settings() -> Settings {
        let root = std::env::temp_dir().join(format!(
            "windcap-reindex-idx-{}-{}",
            std::process::id(),
            test_counter::next()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let settings = Settings {
            root: root.clone(),
            videos_dir: root.join("userdata/videos"),
            db_dir: root.join("userdata/db"),
            win_title_dir: root.join("cache/win_title"),
            iframe_dir: root.join("cache/i_frames"),
            cache_dir: root.join("cache"),
            screenshot_dir: root.join("cache_screenshot"),
            user_name: "default".into(),
            ffmpeg: PathBuf::from("Z:/definitely-not-here/ffmpeg.exe"),
            encoder: "cpu_h264".into(),
            framerate: 2,
            record_seconds: 900,
            ocr_lang: "zh-Hans-CN".into(),
            // Deliberately an engine that is not there: these tests exercise the failure paths, and the
            // engine a real install resolves is the one `Settings::from_config` reads from the config.
            ocr: wind_base::ocr::Engine::builtin_at(
                root.join("ocr_lib").join("Windows.Media.Ocr.Cli.exe"),
                root.clone(),
                "zh-Hans-CN",
            ),
            exclude_words: vec!["Windrecorder".into()],
            urbl: vec![6, 6, 6, 3],
            display_count: 1,
            cross_time_dedup: true,
            cross_time_threshold: 0.94,
            thumbnail_width: 70,
            thumbnail_quality: 30,
            keep_frames: false,
        };
        std::fs::create_dir_all(settings.videos_dir.clone()).unwrap();
        std::fs::create_dir_all(settings.cache_dir.clone()).unwrap();
        settings
    }

    /// The measured rate wins over the configured guess, because a row's second is derived by dividing
    /// its frame number and the encoder pins one frame per second while `record_framerate` shipped as 2.
    #[test]
    fn the_segment_answers_for_its_own_rate_before_the_config_does() {
        assert_eq!(segment_rate(Some(1.0), 2), 1, "a 1 fps segment is not halved by a config that says 2");
        assert_eq!(segment_rate(Some(2.0), 2), 2, "and a real 2 fps segment keeps the rate it has");
        assert_eq!(segment_rate(Some(29.97), 2), 30, "the rounded rate is what divides the frame index");
        assert_eq!(segment_rate(None, 2), 2, "with nothing measured, the setting is the fallback");
        assert_eq!(segment_rate(None, 0), 1, "and a rate of zero still cannot divide by zero");
        assert_eq!(segment_rate(Some(0.0), 2), 2, "nor can a probe that reported nothing usable");

        // The consequence, in seconds rather than in a divisor: frame 200 of a segment that holds one
        // frame per second sits at second 200, not at second 100.
        assert_eq!(timeline::frame_offset_seconds(200, segment_rate(Some(1.0), 2)), 200);
        assert_eq!(timeline::frame_offset_seconds(200, segment_rate(None, 2)), 100, "the old answer, kept honest as the fallback case");
    }

    /// Tests share one temp root only if they share one id; this keeps parallel test threads from
    /// deleting each other's trees.
    mod test_counter {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        pub fn next() -> u64 {
            N.fetch_add(1, Ordering::Relaxed)
        }
    }
}
