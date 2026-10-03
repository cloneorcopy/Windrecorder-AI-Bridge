//! The convert step: cached screenshot slices become the videos the index already names.
//!
//! `windrec run` writes JPEG frames into `cache_screenshot/{stamp}/` and commits rows whose
//! `videofile_name` is `{stamp}.mp4` — a file that does not exist yet. Everything downstream (the
//! `-VIDEO` rename, the cache sweep, the UI's "the video is missing" flag) keys off that rename, so
//! this module has to produce exactly the name the rows already reference, in exactly the month
//! folder their timestamp falls in. Nothing here rewrites an index row.
//!
//! The queue of slices runs on `wind_base::pool` lanes rather than one after another on the caller's
//! thread: a slice is a whole ffmpeg subprocess, so the step asks the pool for as many of those as the
//! machine can carry at once (`Duty::Subprocess`). What the step *means* is unchanged — the slices are
//! enumerated first, every result lands back in its own slot, and the counters and the stop request are
//! folded from those slots in queue order, exactly as the serial loop built them one slice at a time.

use std::path::{Path, PathBuf};

use wind_base::clock::{self, LocalParts};
use wind_base::config::Config;
use wind_base::paths;
use wind_base::pool;

use crate::encode::{self, PresetTable};
use crate::layout;

/// The concat list, written into the slice directory it describes.
///
/// A name with no stamp in it cannot be mistaken for a frame by [`read_frames`], and it sits beside
/// the frames so a failure mid-encode leaves evidence a human can feed to ffmpeg by hand.
const LIST_FILE: &str = "windmaint_concat.txt";

/// One unconverted capture: `cache_screenshot/{stamp}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    pub dir: PathBuf,
    pub stamp: String,
}

// `wind_base::paths::slice_dirs`, under the name this crate's callers and tests already use. The
// listing rule moved for the same reason the stamp rules did: the window that shows a row's picture
// looks a segment's frames up with it, and a second `read_dir` filter would be free to disagree with
// the one the deletion pass trusts.
pub use wind_base::paths::slice_dirs;

/// One retained screen, with the instant it was captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub path: PathBuf,
    pub time: i64,
}

impl Frame {
    pub fn stamp(&self) -> String {
        LocalParts::from_naive_epoch(self.time).stamp()
    }
}

/// Every slice directory waiting to be converted, oldest first.
///
/// Two filters, and the second one is the safety of the whole pipeline. Upstream's first is
/// `re.match` against the datetime pattern — anchored at the start — so a directory that already
/// carries a pipeline marker (`-VIDEO`, `-DISCARD`, `-OCRED`) is skipped as finished work. The
/// second is `paths::is_submitted`: only a slice the recorder *closed* by writing the nested
/// `-SUBMIT` marker may be converted. Without it, a maintenance pass started while the recorder is
/// still writing would encode a half slice, rename the directory out from under the next frame, and
/// leave committed rows pointing at a path that no longer exists. Upstream applies the same rule
/// (`record.py`: skip unless the `-SUBMIT` marker exists).
///
/// Oldest-first is what makes the cache drain as a queue instead of leaving a two-week-old slice at
/// the back of an alphabetical scan.
pub fn discover_slices(cache_root: &Path) -> Vec<Slice> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(cache_root) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if name.is_empty() || paths::has_segment_marker(&name) || LocalParts::from_stamp(&name).is_none() {
            continue;
        }
        if !paths::is_submitted(&path) {
            continue;
        }
        out.push(Slice { dir: path, stamp: name });
    }
    out.sort_by(|a, b| a.stamp.cmp(&b.stamp));
    out
}

/// The frames of one slice, in capture order.
///
/// Ordered by the timestamp in the filename rather than by mtime, because the database rows the video
/// has to line up with were written from those same names; a copied or restored directory whose
/// mtimes were flattened still rebuilds the original timeline. A file whose name is not a stamp is
/// not a frame — that is how upstream's `_cropped`/`_error` side products and the slice's own json
/// manifest stay out of the video.
pub fn read_frames(slice_dir: &Path) -> Vec<Frame> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(slice_dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !is_image_name(name) {
            continue;
        }
        // The whole stem has to be a stamp: upstream's `_cropped` and `_error` side products start
        // with a valid stamp and would otherwise be read as frames of the slice.
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
        if stem.len() != paths::STAMP_LEN {
            continue;
        }
        if let Some(at) = LocalParts::from_stamp(stem).map(|p| p.naive_epoch_seconds()) {
            out.push(Frame { path, time: at });
        }
    }
    out.sort_by(|a, b| (a.time, &a.path).cmp(&(b.time, &b.path)));
    out
}

fn is_image_name(name: &str) -> bool {
    matches!(name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png")
}

/// Where a slice's video belongs: `videos/{YYYY-MM}/{stamp}.mp4`.
///
/// The month folder is derived from the segment's own stamp, which is the same rule
/// `make_screenshots_into_video_via_dir_path` applies to `vid_file_name` — and the reason a segment
/// that crosses midnight into a new month still lands beside its siblings.
pub fn output_video(config: &Config, stamp: &str) -> Option<(PathBuf, String)> {
    let parts = LocalParts::from_stamp(stamp)?;
    Some((
        config.month_videos_dir(parts.year, parts.month).join(format!("{stamp}.mp4")),
        format!("{stamp}.mp4"),
    ))
}

/// What one convert run did, in the terms the report line uses.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub converted: usize,
    pub discarded: usize,
    pub already_present: usize,
    pub failed: usize,
    /// Encoders put down by 停止整理. Kept apart from `failed` because the pass was asked to stop, and a
    /// report that turned somebody's button into "N slice(s) could not be encoded" would be a sentence the
    /// next reader goes looking for a broken encoder over.
    pub called_off: usize,
    pub frames: usize,
    pub seconds: i64,
}

/// Convert up to `limit` slices. `dry_run` touches nothing at all: no list file, no ffmpeg, no rename.
pub fn run(root: &Path, config: &Config, dry_run: bool, limit: Option<usize>) -> Result<Outcome, String> {
    run_followed(root, config, dry_run, limit, None)
}

/// The same step, handing each finished segment to the lane that re-indexes behind the encoder.
///
/// A slice renamed `-VIDEO` is a complete hour: that rename is the only evidence on disk that ffmpeg
/// returned, and it happens on the lane that encoded it. Saying so here, the moment it happens, is what
/// lets a `wind-reindex` process start on the hours that are already encoded while the encoder is still
/// working on later ones, instead of the two legs queueing behind each other for the whole step.
///
/// `None` — which is what a standalone `windmaint convert` and the backlog census pass — hands nothing to
/// anybody: a rehearsal cannot make work, and a step run by hand does not silently start a back-index.
pub fn run_followed(
    root: &Path,
    config: &Config,
    dry_run: bool,
    limit: Option<usize>,
    hand_off: Option<&crate::schedule::Handoff>,
) -> Result<Outcome, String> {
    let cache_root = config.cache_screenshot_dir();
    // An unreadable preset file is reported once, here, and then simply *is* the empty table: every
    // name the config asks for then fails to resolve inside `resolve_encoder`, which is where the
    // "and we are encoding with libx264 instead" sentence belongs. Printing a note about the file and
    // a second, vaguer one about the encoder is the shape the old code had, and it let a real
    // misconfiguration read like a fallback.
    let table = match encode::PresetTable::load(root) {
        Ok(table) => Some(table),
        Err(e) => {
            eprintln!("note: {e}");
            None
        }
    };
    let encoder_name = config.str_or("record_encoder", "cpu_h264");
    let bitrate = config.i64_or("record_bitrate", 200);
    let crf = config.i64_or("record_crf", 39);
    let background = config.str_or("foreground_window_video_background_color", "#000000");
    let ffmpeg = config.ffmpeg_path();
    // Asked once per pass, not once per slice: the answer is a property of the machine, and a cache
    // with two hundred slices would otherwise pay two hundred process spawns to learn it once.
    //
    // A dry run asks nothing at all, because a dry run must not run anything. That leaves the pass
    // reporting an encoder it has not proven usable, so it says so on the same line rather than
    // letting `--dry-run` look like a guarantee it cannot make.
    let availability = |codec: &str| {
        if dry_run {
            encode::EncoderAvailability::Unknown
        } else {
            layout::probe_encoder(&ffmpeg, codec)
        }
    };
    let empty = PresetTable::default();
    let choice = encode::resolve_encoder(&encoder_name, table.as_ref().unwrap_or(&empty), bitrate, crf, &availability);
    if let Some(note) = &choice.note {
        eprintln!("note: {note}");
    }
    let probed = if dry_run { " (not probed: dry run)" } else { "" };
    println!(
        "encoder: {encoder_name} -> {}{probed}",
        encode::codec_in_args(&choice.args).unwrap_or("the preset's own default")
    );
    let encoder = choice.args;

    let slices = discover_slices(&cache_root);
    // The queue is enumerated before any of it is worked on, which is what lets the pool hand each
    // result back into the slot its slice came from: `--limit` and oldest-first are decided here, and
    // the fold below reads the slots in this same order.
    let queue: Vec<&Slice> = slices.iter().take(limit.unwrap_or(usize::MAX)).collect();
    let slots = pool::run(
        &queue,
        pool::lanes(pool::Duty::Subprocess),
        // Asked before the count, so a slice never encoded is never reported as one done. Encoding is
        // ffmpeg's, and a library of slices is minutes: this is the other place a stop has to land, and
        // a worker that is told to stop leaves the rest of the queue in its empty slot.
        || wind_base::maintain::may_continue(config),
        |slice| {
            // One slice looked at is one segment for the 视频合成 counter, whoever decided its fate afterwards.
            wind_base::maintain::add_items(wind_base::maintain::Leg::Convert, 1);
            let unit = convert_one(config, &ffmpeg, &encoder, &background, dry_run, slice);
            // The rename happened on this thread a moment ago, so this is the segment's first moment of
            // being a complete video. A dry run hands over nothing — it renamed nothing.
            if !dry_run && matches!(unit.did, Did::Converted { .. } | Did::AlreadyPresent) {
                if let Some(hand_off) = hand_off {
                    if let Some((video, _)) = output_video(config, &slice.stamp) {
                        hand_off.mark_encoded(video);
                    }
                }
            }
            unit
        },
    );

    let mut outcome = Outcome::default();
    let mut broken: Option<String> = None;
    // `answered` is the slots in queue order, minus the never-claimed suffix a stop left behind — which
    // is exactly the serial loop, whose `break` never counted the slices it did not reach. The fold
    // below is the same `+=` sequence, applied after the waiting instead of between the slices.
    for unit in pool::answered(slots) {
        // The first slice whose scaffolding could not be written is the error the serial loop would
        // have returned through `?`, in queue order rather than in the order the workers finished.
        if broken.is_none() {
            if let Did::Broken { message } = &unit.did {
                broken = Some(message.clone());
            }
        }
        outcome.frames += unit.frames;
        unit.did.tally(&mut outcome);
    }
    match broken {
        Some(message) => Err(message),
        None => Ok(outcome),
    }
}

/// What became of one slice, in the terms [`Outcome`] counts in.
#[derive(Debug)]
enum Did {
    /// Encoded and renamed `-VIDEO`, worth this many seconds of video.
    Converted { seconds: i64 },
    /// A dry run's success: counted as converted, adds no seconds and moves nothing.
    Planned,
    /// Too few frames. `mark_failed` is the `-DISCARD` rename going wrong as well, which the serial
    /// loop counted twice: once as discarded, once as failed.
    Discarded { mark_failed: bool },
    /// The video was already on disk, so the slice was marked instead of re-encoded.
    AlreadyPresent,
    /// Nothing moved: the stamp named no video, or ffmpeg failed, or the `-VIDEO` rename did.
    Failed,
    /// The encoder was put down because 停止整理 was pressed. The slice is untouched and unmarked, the
    /// partial video is gone, and this is the pass ending rather than a segment going wrong.
    CalledOff,
    /// What the serial loop returned through `?`: the first of these ends the step.
    Broken { message: String },
}

impl Did {
    fn tally(self, outcome: &mut Outcome) {
        match self {
            Did::Converted { seconds } => {
                outcome.converted += 1;
                outcome.seconds += seconds;
            }
            Did::Planned => outcome.converted += 1,
            Did::Discarded { mark_failed } => {
                if mark_failed {
                    outcome.failed += 1;
                }
                outcome.discarded += 1;
            }
            Did::AlreadyPresent => outcome.already_present += 1,
            Did::Failed => outcome.failed += 1,
            Did::CalledOff => outcome.called_off += 1,
            // No counter: a broken step answers with the `Err` built from this message, and the serial
            // `?` dropped the half-built outcome exactly the same way.
            Did::Broken { .. } => {}
        }
    }
}

/// One slice's verdict plus the frames the report sums over — the whole of what a worker hands back.
#[derive(Debug)]
struct Unit {
    did: Did,
    frames: usize,
}

/// Do everything the convert step does to one slice, on a pool lane.
///
/// Two slices' lines can interleave here, and that is honest rather than a defect: each `println!`
/// holds the stdout lock for the whole macro call, so a line is never torn mid-sentence and every line
/// still says which slice it is about. The serial loop had its lines in queue order only because it had
/// one thread; nothing downstream reads this text as a sequence.
///
/// The two steps that used to abort the whole pass with `?` (make the month folder, write the concat
/// list) come back as [`Did::Broken`] instead: on a lane there is no loop to return out of, and the
/// other slices' work is worth keeping.
fn convert_one(config: &Config, ffmpeg: &Path, encoder: &[String], background: &str, dry_run: bool, slice: &Slice) -> Unit {
    let frames = read_frames(&slice.dir);
    let count = frames.len();
    if count < encode::MIN_FRAMES {
        println!("{}: {count} frames is below the {min} needed for a video -> DISCARD", slice.dir.display(), min = encode::MIN_FRAMES);
        // The rename is the only state a discarded slice carries, so a failure to mark it is a failure
        // of the step as well as a discard.
        let mark_failed = !dry_run && mark(&slice.dir, paths::MARKER_DISCARD).is_err();
        return Unit { did: Did::Discarded { mark_failed }, frames: count };
    }

    let Some((output, name)) = output_video(config, &slice.stamp) else {
        return Unit { did: Did::Failed, frames: count };
    };
    if output.exists() {
        // A previous pass encoded this slice and died before renaming it: the video is the
        // evidence, and re-encoding over a file the rows already point at is the worse option.
        println!("{}: {} already exists -> mark VIDEO", slice.dir.display(), name);
        if !dry_run {
            let _ = mark(&slice.dir, paths::MARKER_VIDEO);
        }
        return Unit { did: Did::AlreadyPresent, frames: count };
    }

    let times: Vec<i64> = frames.iter().map(|f| f.time).collect();
    let durations = encode::frame_durations(&times, encode::TAIL_SECONDS);
    let entries: Vec<(PathBuf, i64)> = frames.iter().zip(&durations).map(|(f, d)| (f.path.clone(), *d)).collect();
    let total: i64 = durations.iter().sum();
    let sizes: Vec<(u32, u32)> = frames.iter().filter_map(|f| encode::image_dimensions(&f.path)).collect();
    let canvas = encode::video_canvas(&sizes, background);
    println!(
        "{}: {} frames, {} first/last {}, -> {}",
        slice.dir.display(),
        frames.len(),
        clock::seconds_to_hhmmss(total),
        format_args!("{} .. {}", frames[0].stamp(), frames[frames.len() - 1].stamp()),
        output.display()
    );
    if let Some(canvas) = &canvas {
        println!("  canvas {}x{} ({} of {} frames reported a size)", canvas.width, canvas.height, sizes.len(), frames.len());
    }
    if dry_run {
        return Unit { did: Did::Planned, frames: count };
    }

    if let Some(parent) = output.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Unit { did: Did::Broken { message: format!("{}: {e}", parent.display()) }, frames: count };
        }
    }
    let list = slice.dir.join(LIST_FILE);
    if let Err(e) = std::fs::write(&list, encode::concat_list(&entries, &slice.dir)) {
        return Unit { did: Did::Broken { message: format!("{}: {e}", list.display()) }, frames: count };
    }
    let args = encode::encode_args(&list, &output, encoder, canvas.as_ref());
    // The encoder is the longest single thing this step does — a long segment is a minute of ffmpeg — so
    // it is the one place where 停止整理 has to reach *into* the work item instead of waiting beside it.
    let result = layout::run_ffmpeg(ffmpeg, &args, &|| wind_base::maintain::may_continue(config));
    // The list is scaffolding: it must not outlive the encode in either direction, and a failed
    // one keeps the slice unmarked so the next idle pass retries it from the same frames.
    let _ = std::fs::remove_file(&list);
    match result {
        Ok(()) => {
            if std::fs::rename(&slice.dir, marked_path(&slice.dir, paths::MARKER_VIDEO)).is_err() {
                let _ = std::fs::remove_file(&output);
                return Unit { did: Did::Failed, frames: count };
            }
            Unit { did: Did::Converted { seconds: total }, frames: count }
        }
        // Called off with the pass: the partial video goes, the slice stays unmarked for the next pass,
        // and the encoder did not fail — counting it as a failure would make somebody's stop button the
        // machine's mistake, and would end the step with "could not be encoded" on its way out.
        Err(layout::Called::Off) => {
            let _ = std::fs::remove_file(&output);
            Unit { did: Did::CalledOff, frames: count }
        }
        Err(layout::Called::Failed(e)) => {
            eprintln!("  encode failed: {e}");
            let _ = std::fs::remove_file(&output);
            Unit { did: Did::Failed, frames: count }
        }
    }
}

fn marked_path(dir: &Path, marker: &str) -> PathBuf {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    dir.with_file_name(format!("{name}{marker}"))
}

/// Append a pipeline marker to a slice directory, the only state a converted slice carries.
pub fn mark(dir: &Path, marker: &str) -> Result<PathBuf, String> {
    let target = marked_path(dir, marker);
    std::fs::rename(dir, &target).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A put-down encoder is not a failed one. The step's own report keeps them in separate columns, and the
    /// difference is not tidiness: `failed > 0` is what makes `convert` answer with "N slice(s) could not be
    /// encoded", which is a sentence about a broken machine, and 停止整理 is a person's decision.
    #[test]
    fn a_put_down_encode_is_counted_apart_from_one_that_failed() {
        let mut outcome = Outcome::default();
        for did in [Did::Converted { seconds: 12 }, Did::CalledOff, Did::CalledOff, Did::Failed] {
            did.tally(&mut outcome);
        }
        assert_eq!((outcome.converted, outcome.called_off, outcome.failed), (1, 2, 1), "{outcome:?}");
    }

    fn temp_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-convert-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A slice directory as `windrec run` leaves it once the segment is closed: one JPEG per
    /// retained screen plus the nested `-SUBMIT` marker written after the rows commit. Fixtures are
    /// closed by default because that is the only state conversion is ever meant to see;
    /// [`write_open_slice`] is the half-written case a pass must not touch.
    fn write_slice(root: &Path, stamp: &str, frame_stamps: &[&str]) -> PathBuf {
        let dir = root.join(stamp);
        std::fs::create_dir_all(&dir).unwrap();
        for frame in frame_stamps {
            std::fs::write(dir.join(format!("{frame}.jpg")), b"fake jpeg").unwrap();
        }
        std::fs::create_dir_all(dir.join(paths::SUBMIT_MARKER_DIR)).unwrap();
        dir
    }

    /// A slice the recorder is still writing: frames, no marker.
    fn write_open_slice(root: &Path, stamp: &str, frame_stamps: &[&str]) -> PathBuf {
        let dir = write_slice(root, stamp, frame_stamps);
        let _ = std::fs::remove_dir_all(dir.join(paths::SUBMIT_MARKER_DIR));
        dir
    }

    const FIVE: [&str; 5] = [
        "2026-09-21_21-16-12",
        "2026-09-21_21-16-20",
        "2026-09-21_21-16-32",
        "2026-09-21_21-16-45",
        "2026-09-21_21-16-49",
    ];

    #[test]
    fn discovery_takes_closed_stamp_dirs_oldest_first() {
        let root = temp_tree("discover");
        let cache = root.join("cache_screenshot");
        std::fs::create_dir_all(&cache).unwrap();
        write_slice(&cache, "2026-09-21_21-16-12", &FIVE);
        write_slice(&cache, "2026-09-20_10-00-00", &["2026-09-20_10-00-00"]);
        write_slice(&cache, "2026-09-19_09-00-00-VIDEO", &FIVE);
        write_slice(&cache, "2026-09-18_09-00-00-DISCARD", &FIVE);
        write_slice(&cache, "2026-09-17_09-00-00-SCREENSHOTS-OCRED", &FIVE);
        write_slice(&cache, "not-a-timestamp", &FIVE);
        std::fs::write(cache.join("loose.txt"), b"x").unwrap();
        let found: Vec<String> = discover_slices(&cache).into_iter().map(|s| s.stamp).collect();
        assert_eq!(found, vec!["2026-09-20_10-00-00", "2026-09-21_21-16-12"]);
        assert!(discover_slices(&root.join("nope")).is_empty(), "a missing cache is an empty queue");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A slice the recorder is still writing must never be encoded: ffmpeg would read frames as they
    /// land, and the rename that marks the slice converted would take the directory out from under
    /// the next frame the writer opens.
    #[test]
    fn an_open_slice_is_left_alone() {
        let root = temp_tree("open");
        let cache = root.join("cache_screenshot");
        std::fs::create_dir_all(&cache).unwrap();
        write_open_slice(&cache, "2026-09-21_21-16-12", &FIVE);
        assert!(discover_slices(&cache).is_empty(), "no marker means the recorder still owns it");

        write_slice(&cache, "2026-09-21_21-16-12", &FIVE);
        assert_eq!(discover_slices(&cache).len(), 1, "closing it makes it work");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frames_are_ordered_by_the_stamp_in_their_name() {
        let root = temp_tree("frames");
        let slice = write_slice(&root, "2026-09-21_21-16-12", &FIVE);
        std::fs::write(slice.join("2026-09-21_21-17-00.png"), b"png").unwrap();
        std::fs::write(slice.join("tmp_db_json_all_files.json"), b"{}").unwrap();
        std::fs::write(slice.join("2026-09-21_21-18-00_cropped.jpg"), b"crop").unwrap();
        std::fs::create_dir_all(slice.join("sub")).unwrap();

        let frames = read_frames(&slice);
        assert_eq!(frames.len(), 6, "5 jpg + 1 png, no json, no _cropped, no directory");
        let times: Vec<i64> = frames.iter().map(|f| f.time).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
        assert_eq!(frames.last().unwrap().stamp(), "2026-09-21_21-17-00");
        assert!(read_frames(&root.join("absent")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_frame_written_out_of_order_still_lands_in_time_order() {
        let root = temp_tree("order");
        let slice = root.join("2026-09-21_21-16-12");
        std::fs::create_dir_all(&slice).unwrap();
        for frame in ["2026-09-21_21-16-45", "2026-09-21_21-16-12", "2026-09-21_21-16-20"] {
            std::fs::write(slice.join(format!("{frame}.jpg")), b"j").unwrap();
        }
        let stamps: Vec<String> = read_frames(&slice).into_iter().map(|f| f.stamp()).collect();
        assert_eq!(stamps, ["2026-09-21_21-16-12", "2026-09-21_21-16-20", "2026-09-21_21-16-45"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_output_name_is_the_one_the_index_rows_already_carry() {
        let root = temp_tree("output");
        let config = Config::load(&root).unwrap();
        let (path, name) = output_video(&config, "2026-09-21_21-16-12").unwrap();
        assert_eq!(name, "2026-09-21_21-16-12.mp4");
        assert_eq!(path, root.join("userdata").join("videos").join("2026-09").join(name));
        // A segment that crosses into a new month follows its own timestamp, not today's folder.
        let (december, _) = output_video(&config, "2026-12-31_23-59-59").unwrap();
        assert!(december.starts_with(root.join("userdata/videos/2026-12")), "{december:?}");
        assert!(output_video(&config, "2026-13-45_99-99-99").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mark_appends_the_pipeline_marker_in_place() {
        let root = temp_tree("mark");
        let slice = write_slice(&root, "2026-09-21_21-16-12", &FIVE);
        let moved = mark(&slice, paths::MARKER_VIDEO).unwrap();
        assert_eq!(moved, root.join("2026-09-21_21-16-12-VIDEO"));
        assert!(!slice.exists());
        assert!(read_frames(&moved).len() >= FIVE.len(), "the frames ride along with the rename");
        assert!(mark(&slice, paths::MARKER_VIDEO).is_err(), "renaming what is not there is an error");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_slice_below_the_frame_minimum_is_discarded_and_nothing_else_moves() {
        let root = temp_tree("discard");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(
            root.join("config_src/record_preset.json"),
            r#"{"cpu_h264":{"ffmpeg_cmd":["-c:v","libx264","-b:v","BITRATE"]}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("cache_screenshot")).unwrap();
        let short = write_slice(&root.join("cache_screenshot"), "2026-09-21_21-16-12", &FIVE[..4]);
        let other = root.join("cache_screenshot/other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("keep.jpg"), b"j").unwrap();

        let config = Config::load(&root).unwrap();
        let outcome = run(&root, &config, false, None).unwrap();
        assert_eq!((outcome.discarded, outcome.converted, outcome.failed), (1, 0, 0), "{outcome:?}");
        assert!(!short.exists(), "the unmarked directory is gone");
        assert!(root.join("cache_screenshot/2026-09-21_21-16-12-DISCARD").exists(), "it was renamed, not erased");
        assert!(other.join("keep.jpg").exists(), "a discard renames its own slice and nothing else");
        assert!(!root.join("userdata/videos").exists(), "a discarded slice creates no month folder");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The dry run is what a user points at a live install, so it must not even write the list file.
    #[test]
    fn a_dry_run_names_the_work_and_touches_nothing() {
        let root = temp_tree("dry");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(
            root.join("config_src/record_preset.json"),
            r#"{"cpu_h264":{"ffmpeg_cmd":["-c:v","libx264","-b:v","BITRATE"]}}"#,
        )
        .unwrap();
        let cache = root.join("cache_screenshot");
        std::fs::create_dir_all(&cache).unwrap();
        let slice = write_slice(&cache, "2026-09-21_21-16-12", &FIVE);
        let short = write_slice(&cache, "2026-09-21_22-00-00", &FIVE[..2]);

        let config = Config::load(&root).unwrap();
        let outcome = run(&root, &config, true, None).unwrap();
        assert_eq!(outcome.converted, 1, "{outcome:?}");
        assert_eq!(outcome.discarded, 1, "{outcome:?}");
        assert!(slice.exists() && short.exists(), "no rename under --dry-run");
        assert!(!slice.join(LIST_FILE).exists(), "no list file under --dry-run");
        assert!(!root.join("userdata/videos").exists(), "no month folder created under --dry-run");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn limit_bounds_the_number_of_slices_worked_on() {
        let root = temp_tree("limit");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(
            root.join("config_src/record_preset.json"),
            r#"{"cpu_h264":{"ffmpeg_cmd":["-c:v","libx264"]}}"#,
        )
        .unwrap();
        let cache = root.join("cache_screenshot");
        std::fs::create_dir_all(&cache).unwrap();
        write_slice(&cache, "2026-09-20_21-16-12", &FIVE);
        let untouched = write_slice(&cache, "2026-09-21_21-16-12", &FIVE);

        let config = Config::load(&root).unwrap();
        let outcome = run(&root, &config, false, Some(1)).unwrap();
        assert_eq!(outcome.converted + outcome.failed, 1, "{outcome:?}");
        assert!(untouched.exists(), "the second slice must still be waiting");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_existing_video_marks_the_slice_instead_of_re_encoding_it() {
        let root = temp_tree("existing");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(
            root.join("config_src/record_preset.json"),
            r#"{"cpu_h264":{"ffmpeg_cmd":["-c:v","libx264"]}}"#,
        )
        .unwrap();
        let cache = root.join("cache_screenshot");
        std::fs::create_dir_all(&cache).unwrap();
        let slice = write_slice(&cache, "2026-09-21_21-16-12", &FIVE);
        let month = root.join("userdata/videos/2026-09");
        std::fs::create_dir_all(&month).unwrap();
        std::fs::write(month.join("2026-09-21_21-16-12.mp4"), b"already encoded").unwrap();

        let config = Config::load(&root).unwrap();
        let outcome = run(&root, &config, false, None).unwrap();
        assert_eq!(outcome.already_present, 1, "{outcome:?}");
        assert_eq!(outcome.converted, 0);
        assert!(!slice.exists());
        assert_eq!(std::fs::read(month.join("2026-09-21_21-16-12.mp4")).unwrap(), b"already encoded");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The convert step works its queue on pool lanes now. What it must not change is what a pass
    /// *means*: three slices decided at once — one too short to be a video, one whose video is already
    /// on disk, one plain work — still come back with the counters the serial loop built, the frames of
    /// all three summed, and the slice that only a plan was made for left exactly where it was.
    #[test]
    fn three_slices_decided_at_once_come_back_with_the_same_counts_as_one_at_a_time() {
        let root = temp_tree("pooled");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(
            root.join("config_src/record_preset.json"),
            r#"{"cpu_h264":{"ffmpeg_cmd":["-c:v","libx264"]}}"#,
        )
        .unwrap();
        let cache = root.join("cache_screenshot");
        std::fs::create_dir_all(&cache).unwrap();
        let short = write_slice(&cache, "2026-09-19_09-00-00", &FIVE[..2]);
        let already = write_slice(&cache, "2026-09-20_10-00-00", &FIVE);
        let planned = write_slice(&cache, "2026-09-21_21-16-12", &FIVE);
        // The second slice's video is the evidence a previous pass left: the step marks it, never
        // re-encodes over it.
        let month = root.join("userdata/videos/2026-09");
        std::fs::create_dir_all(&month).unwrap();
        std::fs::write(month.join("2026-09-20_10-00-00.mp4"), b"already encoded").unwrap();

        let config = Config::load(&root).unwrap();
        // A dry run, because the third slice's fate otherwise belongs to the machine's ffmpeg: what is
        // being proved here is the fold the workers hand back, not the encoder.
        let outcome = run(&root, &config, true, None).unwrap();
        assert_eq!(
            (outcome.converted, outcome.discarded, outcome.already_present, outcome.failed, outcome.seconds),
            (1, 1, 1, 0, 0),
            "{outcome:?}"
        );
        assert_eq!(outcome.frames, 2 + FIVE.len() + FIVE.len(), "every branch's frames are counted");
        for slice in [&short, &already, &planned] {
            assert!(slice.exists(), "a dry run left {slice:?} unmarked");
            assert!(!slice.join(LIST_FILE).exists(), "no scaffolding in {slice:?}");
        }
        assert!(!month.join("2026-09-21_21-16-12.mp4").exists(), "the planned slice wrote no video");
        assert_eq!(std::fs::read(month.join("2026-09-20_10-00-00.mp4")).unwrap(), b"already encoded");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_concat_list_file_is_scaffolding_and_never_outlives_the_encode() {
        let root = temp_tree("scaffold");
        let slice = write_slice(&root, "2026-09-21_21-16-12", &FIVE);
        std::fs::write(slice.join(LIST_FILE), b"stale").unwrap();
        assert_eq!(read_frames(&slice).len(), FIVE.len(), "the list file must not be read as a frame");
        let _ = std::fs::remove_dir_all(&root);
    }
}
