//! Previews: redraw `video_text.thumbnail` at the size the window paints a card at.
//!
//! The index's thumbnail *is* the picture a card has, in both windows, in the lightbox and in the tray.
//! Upstream sized it for a 70 px grid tile, because that is what its own WebUI drew; this fork draws a
//! card several times that wide, so every row on a user's disk stores a preview the size of a stamp and
//! shows it stretched. Raising `thumbnail_generation_size_width` alone fixes the rows recorded from
//! today forward and leaves the user's entire history blurry — which is the dead-control shape this
//! product has now been corrected for twice: a setting that changes only the future, sitting in a page
//! that says nothing about the past.
//!
//! So this pass exists. It is a *redraw*, not a re-index: no row's text is touched, so there is no OCR
//! engine to have installed, no minutes of recognition to pay for, and no chance that a different engine
//! rewrites what a year-old search finds. What changes is one JPEG per row.
//!
//! ## Where the pixels come from
//!
//! Two doors, and the order is the design:
//!
//! 1. **The retained screenshot.** `cache_screenshot/<slice>/` holds one JPEG per kept frame, named for
//!    the instant it was captured, and it survives until the retention sweep takes the segment. The
//!    video the slice became is a slideshow of exactly those files, so the frame a row was made of *is*
//!    the newest screenshot at or before the row's instant — not an approximation of it. This door costs
//!    one directory listing per segment and one decode per row.
//! 2. **The video.** Once the sweep has been through, ffmpeg is asked for the row's offset into the
//!    segment. Slower, and needing a binary an install may not carry, which is why it is second and why
//!    a row that neither door can serve keeps the thumbnail it already has instead of going blank.
//!
//! The stored name in `picturefile_name` is deliberately *not* the key. Which pass committed a row
//! decides what that name is — `windrec` writes the screenshot's own stamp, the re-index pass writes the
//! ffmpeg crop (`42_cropped.jpg`) that is swept minutes later — so a pass that looked the stored name up
//! would find the frame for one kind of row and miss it for the other. The row's instant is common to
//! both.
//!
//! ## Where the work runs
//!
//! The deciding stays serial and the doing goes on [`wind_base::pool`] lanes. A month's rows are planned
//! one after another exactly as they always were — the plan is cheap (a header peek per row) and its
//! answers depend on one shared listing — and then the jobs it produced are handed to the pool with
//! `Duty::Decode` lanes, because a job is a JPEG decode, a Lanczos resize and a JPEG encode with nothing
//! in common with its neighbour. What comes back is one answer per job in the order the jobs were handed
//! out, and those answers are folded and written on this thread in the one transaction per month file the
//! pass has always made: a month of previews is still committed all at once or not at all, and the
//! database still never sees two writers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use rusqlite::Connection;
use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::paths;
use wind_base::pool;
use wind_store::maintain::{apply_thumbnails, ThumbWrite};
use wind_store::read::{self, Row};

use crate::refresh::DiskVideos;

/// The picture a preview is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The recorder's own frame, at the resolution it was captured.
    Screenshot(PathBuf),
    /// The segment, and the row's offset into it in seconds.
    Video(PathBuf, i64),
}

/// One row's work: which door, and into which rowid the answer goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub rowid: i64,
    pub source: Source,
}

/// What a plan decided about one month file's rows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    pub rows: usize,
    /// Rows whose stored picture is already at least as wide as the target.
    pub already_wide_enough: usize,
    /// Rows with a picture on disk behind them, in the order the index holds them.
    pub jobs: Vec<Job>,
    /// Rows neither door can serve.
    pub missing_source: usize,
}

/// What `run` did, summed over the months it visited.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub months: usize,
    pub rows: usize,
    pub already_wide_enough: usize,
    pub missing_source: usize,
    pub planned: usize,
    pub from_screenshot: usize,
    pub from_video: usize,
    /// Rows whose new picture was written.
    pub written: usize,
    /// Rows where a picture was found and could not be read, decoded, encoded or saved.
    pub failed: usize,
}

/// The screenshots of one segment cache, listed once for the whole run.
///
/// A month's rows are a handful of segments, and a `read_dir` per row is the mistake this whole file is
/// here to stop making.
///
/// The listing is taken up front, in [`Slices::scan`], and nothing below it fills anything in. That is
/// what lets the redraws share it: a memo that caches itself behind `&mut self` cannot be read by a pool
/// lane at all, and listing per job instead would walk the same slice directory once per row of it — the
/// very cost the serial loop was written to avoid. So: the same directories as ever (whatever
/// `paths::slice_dirs` finds under the cache root), the same frames out of each, sorted the same way, all
/// of it built once and then read by reference from every worker.
pub struct Slices {
    root: PathBuf,
    /// Stamp → that slice's frames, oldest first. Absent means "no such slice directory"; an empty list
    /// means "there is a slice directory and the sweep has been through it" — which is an answer, not a
    /// failure.
    frames: BTreeMap<String, Vec<(i64, PathBuf)>>,
}

impl Slices {
    pub fn scan(cache_root: &Path) -> Slices {
        let frames = paths::slice_dirs(cache_root)
            .into_iter()
            .map(|(stamp, dir)| (stamp, list_frames(&dir)))
            .collect();
        Slices { root: cache_root.to_path_buf(), frames }
    }

    /// The frame a row's instant falls on: the newest screenshot captured at or before it.
    ///
    /// Falls back to the slice's earliest frame when the row predates every file listed. A segment's
    /// first row is indexed at the instant of its first screenshot, and a one-second difference between
    /// the two clocks must not cost the user that row's picture.
    pub fn frame_at(&self, segment: &str, at: i64) -> Option<PathBuf> {
        let stamp = paths::segment_stamp_of(segment)?;
        let frames = self.frames.get(&stamp)?;
        frames
            .iter()
            .filter(|(when, _)| *when <= at)
            .next_back()
            .or_else(|| frames.first())
            .map(|(_, path)| path.clone())
            .filter(|path| path.is_file())
    }

    /// How many slice directories the cache root holds, for the report line.
    pub fn segments(&self) -> usize {
        self.frames.len()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// The JPEGs in a slice directory, keyed by the instant carried in their own names.
///
/// A name that does not parse as a stamp is skipped rather than guessed at: a slice directory can hold a
/// `-SUBMIT` marker directory and the odd stray file, and letting either stand in for a frame would put
/// somebody's unrelated picture into their index.
fn list_frames(dir: &Path) -> Vec<(i64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(i64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let is_jpeg = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("jpg"));
            if !is_jpeg {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            Some((LocalParts::from_stamp(stem)?.naive_epoch_seconds(), path))
        })
        .collect();
    out.sort();
    out
}

/// The row's offset into its own segment, in seconds, never negative.
///
/// The segment's start is in its name and the row's instant is in its own column, both written by the
/// same clock, so this is the subtraction the player already seeks with — asked the other way round.
pub fn offset_in_segment(row: &Row) -> Option<i64> {
    let stamp = paths::segment_stamp_of(&row.videofile_name)?;
    let start = LocalParts::from_stamp(&stamp)?.naive_epoch_seconds();
    Some((row.time - start).max(0))
}

/// The picture behind a row, at the resolution it was captured, if a door is open.
pub fn source_for(row: &Row, slices: &Slices, videos: &DiskVideos) -> Option<Source> {
    if let Some(frame) = slices.frame_at(&row.videofile_name, row.time) {
        return Some(Source::Screenshot(frame));
    }
    let offset = offset_in_segment(row)?;
    let video = videos.locate(&row.videofile_name).first()?;
    Some(Source::Video(video.clone(), offset))
}

/// The width a row can be redrawn *to*: the target, or the width of the picture it is made from when
/// that is the smaller.
///
/// The video door is excluded by calculation rather than by probing: ffmpeg's `scale` enlarges, so a
/// segment narrower than the target really does produce a preview at the target width, and the plan
/// should keep asking until it appears. A file source cannot be enlarged at all — [`preview_from_bytes`]
/// refuses to — so a row whose frame is 120 px wide is finished the moment it stores 120 px, and
/// re-decoding it on every idle pass would be a job that never converges.
fn reachable_width(source: &Source, target: u32) -> u32 {
    match source {
        Source::Screenshot(path) => match image::image_dimensions(path) {
            Ok((width, _)) => target.min(width.max(1)),
            // Unreadable header: keep the row a candidate, so the pass tries it and reports the failure
            // out loud rather than quietly deciding it was never work to do.
            Err(_) => target,
        },
        Source::Video(..) => target,
    }
}

/// Decide a month's work without reading a single pixel.
///
/// Split from [`apply`] for the same reason every other pass here is split: the deciding is testable on
/// a machine with no ffmpeg and no user footage, and the doing is not.
pub fn plan(rows: &[Row], slices: &Slices, videos: &DiskVideos, width: u32) -> Plan {
    let mut out = Plan { rows: rows.len(), ..Default::default() };
    for row in rows {
        let stored = row.thumbnail.as_deref().unwrap_or_default();
        let source = match source_for(row, slices, videos) {
            Some(source) => source,
            None => {
                out.missing_source += 1;
                continue;
            }
        };
        // The header peek below is the whole cost of the smarter rule: one read of a few bytes, against
        // the full decode that asking the question any other way would cost.
        if !stored.is_empty() && stored_width(stored).is_some_and(|w| w >= reachable_width(&source, width)) {
            out.already_wide_enough += 1;
            continue;
        }
        out.jobs.push(Job { rowid: row.rowid, source });
    }
    out
}

/// One lane's answer for one job: which row it was, which door it drew from, and the picture or the
/// reason there is none.
///
/// The bytes come back instead of being written where they were made, because the write belongs to the
/// month's one transaction and the transaction belongs to the calling thread. The two door counters ride
/// along so [`Outcome`]'s can be summed from the answers in job order rather than from a shared counter
/// the workers would have to fight over.
struct Drawn {
    rowid: i64,
    /// One of these two is 1 and the other 0: which door this row's picture actually came out of.
    from_screenshot: usize,
    from_video: usize,
    /// `Ok` is the base64 JPEG the row should now store; `Err` is why it keeps the one it has.
    picture: Result<String, String>,
}

/// Turn a plan into bytes and write them. One transaction, so a pass that dies halfway leaves every
/// row either redrawn or untouched — never a month of rows whose preview is a screenshot of the wrong
/// moment.
///
/// The drawing is the slow half and it is per row: a JPEG decode, a Lanczos resize, an encode, and for
/// the rows the sweep has been through, a whole ffmpeg seek. None of it shares anything with the next
/// row, so it goes out on [`pool`] lanes sized for [`pool::Duty::Decode`] — which is the difference
/// between a night of 3 800 rows and 3 800 nights. SQLite is not on a lane: the answers are folded here,
/// in job order, and written here, in the same single transaction there always was.
pub fn apply(config: &Config, conn: &mut Connection, plan: &Plan, width: u32, quality: u8, ffmpeg: &Path) -> Result<Outcome, String> {
    let mut outcome = Outcome {
        months: 1,
        rows: plan.rows,
        already_wide_enough: plan.already_wide_enough,
        missing_source: plan.missing_source,
        planned: plan.jobs.len(),
        ..Default::default()
    };
    let slots = pool::run(
        &plan.jobs,
        pool::lanes(pool::Duty::Decode),
        // Each job is a seek into a screenshot or a video, which is the slowest thing this pass does per
        // row; a month's worth is not something to make somebody wait out because they pressed stop. The
        // question is asked before a lane takes a job, so a stop costs the pass at most the redraws
        // already in flight — and nothing has been written by then anyway, because the transaction opens
        // after every lane has come back.
        || wind_base::maintain::may_continue(config),
        |job| {
            // A job a lane claimed is one item of 其他整理 — a row the census counted as `planned`, and it
            // is reported as taken the moment it is, whether or not a picture comes out of it: the
            // progress says what was attempted.
            wind_base::maintain::add_items(wind_base::maintain::Leg::Other, 1);
            match &job.source {
                Source::Screenshot(path) => Drawn {
                    rowid: job.rowid,
                    from_screenshot: 1,
                    from_video: 0,
                    picture: preview_from_file(path, width, quality),
                },
                Source::Video(video, offset) => Drawn {
                    rowid: job.rowid,
                    from_screenshot: 0,
                    from_video: 1,
                    picture: preview_from_video(ffmpeg, video, *offset, width, quality),
                },
            }
        },
    );

    // Folded in job order — the order the index holds the rows in — so the report reads the same way it
    // did when one thread did the work: a lane that finished first does not get its failure named first.
    let mut writes: Vec<ThumbWrite> = Vec::with_capacity(plan.jobs.len());
    for drawn in slots.into_iter().flatten() {
        outcome.from_screenshot += drawn.from_screenshot;
        outcome.from_video += drawn.from_video;
        match drawn.picture {
            // A row whose picture could not be drawn is simply absent from the write list, so the UPDATE
            // never touches it and the stamp-sized preview it already has stays where it is.
            Ok(thumbnail) => writes.push(ThumbWrite { rowid: drawn.rowid, thumbnail }),
            Err(error) => {
                outcome.failed += 1;
                eprintln!("preview: row {} could not be redrawn: {error}", drawn.rowid);
            }
        }
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    outcome.written = apply_thumbnails(&tx, &writes).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(outcome)
}

/// Redraw every month file's previews, oldest first, up to `limit` of them.
pub fn run(config: &Config, dry_run: bool, limit: Option<usize>) -> Result<Outcome, String> {
    let width = config.thumbnail_width();
    let quality = config.thumbnail_quality();
    let ffmpeg = config.ffmpeg_path();
    let months = read::discover(&config.db_dir());
    let videos = DiskVideos::scan(&config.videos_dir());
    let slices = Slices::scan(&config.cache_screenshot_dir());
    println!(
        "previews: target {} px at quality {}; {} slice directory(s) under {}, {} segment(s) of video on disk",
        width,
        quality,
        slices.segments(),
        slices.root().display(),
        videos.segments()
    );

    let mut total = Outcome::default();
    for month in months.iter().take(limit.unwrap_or(usize::MAX)) {
        if !wind_base::maintain::may_continue(config) {
            break;
        }
        let label = month.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let mut conn = if dry_run {
            Connection::open_with_flags(
                &month.path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .map_err(|e| format!("{label}: {e}"))?
        } else {
            month.open_write().map_err(|e| format!("{label}: {e}"))?
        };
        let rows = read::rows_in_window(&conn, None, None).map_err(|e| format!("{label}: {e}"))?;
        let plan = plan(&rows, &slices, &videos, width);
        println!(
            "{label}: {} row(s), {} already wide enough, {} to redraw ({} from a screenshot, {} needing the video), {} with neither on disk{}",
            plan.rows,
            plan.already_wide_enough,
            plan.jobs.len(),
            plan.jobs.iter().filter(|job| matches!(job.source, Source::Screenshot(_))).count(),
            plan.jobs.iter().filter(|job| matches!(job.source, Source::Video(..))).count(),
            plan.missing_source,
            if dry_run { " [dry-run]" } else { "" }
        );
        if dry_run {
            total.months += 1;
            total.rows += plan.rows;
            total.already_wide_enough += plan.already_wide_enough;
            total.missing_source += plan.missing_source;
            total.planned += plan.jobs.len();
            continue;
        }
        let outcome = apply(config, &mut conn, &plan, width, quality, &ffmpeg).map_err(|e| format!("{label}: {e}"))?;
        println!(
            "  wrote {} preview(s){}; {} row(s) failed",
            outcome.written,
            if outcome.failed > 0 { ", some rows keep the picture they had" } else { "" },
            outcome.failed
        );
        total.months += 1;
        total.rows += outcome.rows;
        total.already_wide_enough += outcome.already_wide_enough;
        total.missing_source += outcome.missing_source;
        total.planned += outcome.planned;
        total.from_screenshot += outcome.from_screenshot;
        total.from_video += outcome.from_video;
        total.written += outcome.written;
        total.failed += outcome.failed;
    }
    Ok(total)
}

/// The width a stored base64 JPEG is, read out of its start-of-frame marker.
///
/// Header-only on purpose: this is what decides whether a row needs redrawing, and decoding a whole
/// frame to ask "is it wider than the target?" would make the planning slower than the work it is
/// measuring. `None` — not zero — for anything unreadable, so a corrupt blob is redrawn rather than
/// judged.
pub fn stored_width(stored: &str) -> Option<u32> {
    let body = match stored.split_once(',') {
        Some((head, rest)) if head.trim().starts_with("data:") => rest,
        _ => stored,
    };
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD.decode(&cleaned).ok()?;
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut at = 2;
    while at + 9 < bytes.len() {
        if bytes[at] != 0xFF {
            // Resynchronise rather than give up: encoders are entitled to pad between segments.
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        // Standalone markers carry no length field; everything else is `FF <len-hi> <len-lo> <body>`.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            at += 2;
            continue;
        }
        let length = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
        if length < 2 {
            return None;
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            // After the length and the sample precision comes height, then width — the order JPEG fixes.
            return Some(u16::from_be_bytes([bytes[at + 7], bytes[at + 8]]) as u32);
        }
        at += 2 + length;
    }
    None
}

/// The preview for one row, from the bytes of its frame.
fn preview_from_bytes(bytes: &[u8], width: u32, quality: u8) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("empty picture".into());
    }
    let image = image::load_from_memory(bytes).map_err(|e| format!("decode: {e}"))?;
    // The width is decided in integers, and it is exactly the width `reachable_width` will later accept:
    // `image::thumbnail` recomputes its own scale and came back one pixel short on a 1920 px grab asked
    // for at 240, which left a fifth of a real library looking unfinished to the next pass forever. The
    // height keeps the aspect; the clamp is what stops a small frame being stretched into a blurry claim
    // about resolution.
    let (sw, sh) = (image.width().max(1), image.height().max(1));
    let wide = width.min(sw).max(1);
    // Rounded half-up, and never zero: a portrait grab downscaled to 240 px is still hundreds tall.
    let tall = (((u64::from(sh) * u64::from(wide) + u64::from(sw) / 2) / u64::from(sw)) as u32).max(1);
    let small = image.resize_exact(wide, tall, image::imageops::FilterType::Lanczos3);
    let rgb = small.to_rgb8();
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let jpeg = wind_base::image::encode_jpeg(rgb.as_raw(), w, h, quality)?;
    Ok(STANDARD.encode(&jpeg))
}

fn preview_from_file(path: &Path, width: u32, quality: u8) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    preview_from_bytes(&bytes, width, quality)
}

/// A fresh number for every frame this process asks ffmpeg to write.
///
/// The name used to be unique per *pass*, which was enough while one thread did one row at a time. Now two
/// lanes can be pulling the same stem at the same offset out of the same video in the same second, and one
/// scratch file between them is one lane decoding the other's half-written JPEG — a preview of nothing,
/// written into the index as if it were the row's own frame.
static SCRATCH_SERIAL: AtomicU64 = AtomicU64::new(0);

/// Ask ffmpeg for the one frame at `offset` and make a preview out of it.
///
/// `-ss` before `-i` is the fast seek: it reads the container's index instead of decoding to the mark.
/// The scratch file is removed either way — a preview is derived data, and leaving bytes of somebody's
/// screen in a temporary directory because one row failed is not a trade worth making.
fn preview_from_video(ffmpeg: &Path, video: &Path, offset: i64, width: u32, quality: u8) -> Result<String, String> {
    let serial = SCRATCH_SERIAL.fetch_add(1, Ordering::SeqCst);
    let scratch = std::env::temp_dir().join(format!(
        "windmaint-preview-{}-{}-{offset}-{serial}.jpg",
        std::process::id(),
        video.file_stem().and_then(|s| s.to_str()).unwrap_or("segment")
    ));
    let argv: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-ss".into(),
        offset.to_string(),
        "-i".into(),
        video.to_string_lossy().into_owned(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        format!("scale={width}:-2"),
        "-q:v".into(),
        "2".into(),
        scratch.to_string_lossy().into_owned(),
    ];
    let output = std::process::Command::new(ffmpeg)
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", ffmpeg.display()));
    let outcome = match output {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = std::fs::remove_file(&scratch);
            return Err(error);
        }
    };
    if !outcome.status.success() {
        let _ = std::fs::remove_file(&scratch);
        return Err(format!(
            "{} exited with {}: {}",
            ffmpeg.display(),
            outcome.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&outcome.stderr).trim()
        ));
    }
    let result = preview_from_file(&scratch, width, quality);
    let _ = std::fs::remove_file(&scratch);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_base::image::thumbnail_base64;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-previews-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rgb(w: usize, h: usize, tone: u8) -> Vec<u8> {
        vec![tone; w * h * 3]
    }

    fn row_at(rowid: i64, videofile_name: &str, time: i64, thumbnail: Option<String>) -> Row {
        Row {
            rowid,
            videofile_name: videofile_name.into(),
            picturefile_name: "42_cropped.jpg".into(),
            time,
            ocr_text: "screen text".into(),
            win_title: None,
            deep_linking: None,
            thumbnail,
            video_exists: true,
            picture_exists: true,
            month_path: None,
        }
    }

    /// A picture is stored as base64 JPEG, so the only question this pass asks of an old row — "is it
    /// already big enough?" — has to be answered out of the compressed bytes' own header.
    #[test]
    fn a_stored_previews_width_comes_out_of_its_header() {
        for width in [70u32, 240, 512] {
            let stored = thumbnail_base64(&rgb(600, 300, 200), 600, 300, width, 60).expect("encode");
            assert_eq!(stored_width(&stored), Some(width), "a {width} px preview must read back as {width}");
        }
        let prefixed = format!("data:image/jpeg;base64,{}", thumbnail_base64(&rgb(200, 120, 90), 200, 120, 100, 60).unwrap());
        assert_eq!(stored_width(&prefixed), Some(100), "a data-URL prefix is tolerated, as everywhere else");
        assert_eq!(stored_width(""), None, "nothing stored is unknown, not zero wide");
        assert_eq!(stored_width("not base64 at all!!!!"), None);
        assert_eq!(stored_width(&STANDARD.encode(b"pretend this is a jpeg")), None);
    }

    #[test]
    fn a_row_is_left_alone_once_its_picture_is_wide_enough() {
        let stamp = thumbnail_base64(&rgb(600, 300, 200), 600, 300, 70, 40).expect("encode");
        let big = thumbnail_base64(&rgb(600, 300, 200), 600, 300, 240, 40).expect("encode");
        let start = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        let root = scratch("plan");
        let slice = root.join("2026-09-21_10-00-00-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("2026-09-21_10-00-00.jpg"), b"not a jpeg").unwrap();

        let videos = DiskVideos::scan(&root.join("no-videos-here"));
        let slices = Slices::scan(&root);
        let plan = plan(
            &[
                row_at(1, "2026-09-21_10-00-00.mp4", start, Some(stamp.clone())),
                row_at(2, "2026-09-21_10-00-00.mp4", start + 1, Some(big.clone())),
                row_at(3, "2026-09-21_10-00-00.mp4", start + 2, Some(stamp)),
                row_at(4, "2026-09-30_10-00-00.mp4", start + 3, None),
            ],
            &slices,
            &videos,
            240,
        );
        assert_eq!(plan.rows, 4);
        assert_eq!(plan.already_wide_enough, 1, "only the 240 px row is already a preview");
        assert_eq!(
            plan.jobs.iter().map(|job| job.rowid).collect::<Vec<_>>(),
            vec![1, 3],
            "an unreadable width is work to do, not a verdict of big enough"
        );
        assert_eq!(plan.missing_source, 1, "row 4 names a segment this fixture never wrote");
        assert_eq!(
            plan.jobs.iter().filter(|job| matches!(job.source, Source::Screenshot(_))).count(),
            2,
            "both rows of the recorded slice are served from disk, with no ffmpeg in sight"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The offset the video door seeks with, and the clamp that keeps `-ss` from meaning "from the end".
    #[test]
    fn a_rows_offset_is_measured_from_its_segment_names_own_instant() {
        let start = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        assert_eq!(offset_in_segment(&row_at(1, "2026-09-21_10-00-00.mp4", start + 42, None)), Some(42));
        assert_eq!(offset_in_segment(&row_at(2, "2026-09-21_10-00-00.mp4", start - 30, None)), Some(0));
        assert_eq!(offset_in_segment(&row_at(3, "not-a-timestamp.mp4", start, None)), None);
    }

    /// The screenshot door is why this pass is cheap: one listing answers every row of a segment, and the
    /// frame a row falls on is the newest one at or before its instant.
    #[test]
    fn a_row_falls_on_the_newest_retained_frame_at_or_before_its_instant() {
        let root = scratch("slices");
        let start = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        // Named as the pipeline names them, marker included: the row says `…_10-00-00.mp4`, and the
        // slice directory gained `-VIDEO` when it was converted.
        let slice = root.join("2026-09-21_10-00-00-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        for seconds in [0, 3, 7] {
            let stamp = LocalParts::from_naive_epoch(start + seconds).stamp();
            std::fs::write(slice.join(format!("{stamp}.jpg")), b"jpeg").unwrap();
        }
        std::fs::write(slice.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir_all(slice.join("-SUBMIT")).unwrap();

        let slices = Slices::scan(&root);
        assert_eq!(slices.frame_at("2026-09-21_10-00-00.mp4", start + 5).map(|p| p.file_stem().unwrap().to_string_lossy().into_owned()), Some("2026-09-21_10-00-03".into()));
        assert_eq!(slices.frame_at("2026-09-21_10-00-00.mp4", start + 99).map(|p| p.file_stem().unwrap().to_string_lossy().into_owned()), Some("2026-09-21_10-00-07".into()));
        assert_eq!(slices.frame_at("2026-09-21_10-00-00.mp4", start).map(|p| p.file_stem().unwrap().to_string_lossy().into_owned()), Some("2026-09-21_10-00-00".into()));
        assert_eq!(
            slices.frame_at("2026-09-21_10-00-00.mp4", start - 5).map(|p| p.file_stem().unwrap().to_string_lossy().into_owned()),
            Some("2026-09-21_10-00-00".into()),
            "a row a second before the first frame still gets that frame, not nothing"
        );
        assert_eq!(slices.frame_at("2026-09-20_10-00-00.mp4", start), None, "a segment nobody recorded has no frame");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A frame that is there but unreadable must not be chosen over the video door that would work.
    #[test]
    fn the_screenshot_is_planned_before_the_video_and_neither_beats_both() {
        let root = scratch("doors");
        let cache = root.join("cache_screenshot");
        let slice = cache.join("2026-09-21_10-00-00-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        let frame = slice.join("2026-09-21_10-00-00.jpg");
        std::fs::write(&frame, b"jpeg").unwrap();
        let start = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();

        let videos_dir = root.join("videos");
        std::fs::create_dir_all(videos_dir.join("2026-09")).unwrap();
        let video = videos_dir.join("2026-09").join("2026-09-21_10-00-00-OCRED.mp4");
        std::fs::write(&video, b"v").unwrap();
        let videos = DiskVideos::scan(&videos_dir);

        let slices = Slices::scan(&cache);
        assert_eq!(source_for(&row_at(1, "2026-09-21_10-00-00.mp4", start + 4, None), &slices, &videos), Some(Source::Screenshot(frame.clone())));
        let _ = std::fs::remove_dir_all(&slice);
        let swept = Slices::scan(&cache);
        assert_eq!(
            source_for(&row_at(2, "2026-09-21_10-00-00.mp4", start + 12, None), &swept, &videos),
            Some(Source::Video(video, 12)),
            "once the sweep has been, the video is the only door"
        );
        let neither = Slices::scan(&cache);
        assert_eq!(source_for(&row_at(3, "2026-09-25_10-00-00.mp4", start + 12, None), &neither, &videos), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The convergence test, and the reason it matters: 22 % of a real install's frames are narrower
    /// than the target, and a plan that kept listing them would redraw them on every idle pass forever.
    #[test]
    fn a_row_drawn_from_a_small_frame_is_finished_at_the_frames_own_width() {
        let root = scratch("converge");
        let cache = root.join("cache_screenshot");
        let slice = cache.join("2026-09-21_10-00-00-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        let start = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        let frame = slice.join("2026-09-21_10-00-00.jpg");
        std::fs::write(
            &frame,
            &STANDARD.decode(thumbnail_base64(&rgb(300, 200, 190), 300, 200, 150, 70).expect("encode")).expect("bytes"),
        )
        .unwrap();
        let videos = DiskVideos::scan(&root.join("no-videos"));
        let row = |thumb: Option<String>| row_at(1, "2026-09-21_10-00-00.mp4", start, thumb);

        let small = thumbnail_base64(&rgb(600, 300, 200), 600, 300, 70, 40).expect("encode");
        let drawn = preview_from_file(&frame, 240, 70).expect("preview");
        assert_eq!(stored_width(&drawn), Some(150), "a 150 px frame is drawn at 150 px, not stretched");

        let slices = Slices::scan(&cache);
        let pending = plan(&[row(Some(small.clone()))], &slices, &videos, 240);
        assert_eq!(pending.jobs.len(), 1, "a stamp-sized row is work while the frame can improve it");
        assert_eq!(reachable_width(&pending.jobs[0].source, 240), 150);

        let slices = Slices::scan(&cache);
        let done = plan(&[row(Some(drawn))], &slices, &videos, 240);
        assert!(done.jobs.is_empty(), "{done:?} — a row already at its frame's width is finished");
        assert_eq!(done.already_wide_enough, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_video_sourced_row_keeps_the_full_target_because_ffmpeg_will_enlarge() {
        let video = Path::new("E:/nowhere/2026-09-21_10-00-00.mp4");
        assert_eq!(reachable_width(&Source::Video(video.to_path_buf(), 4), 240), 240);
        assert_eq!(reachable_width(&Source::Screenshot(PathBuf::from("Z:/absent.jpg")), 240), 240, "an unreadable header keeps the row a candidate, so its failure is reported rather than hidden");
    }

    #[test]
    fn a_preview_is_never_upsampled_past_the_picture_it_was_made_from() {
        // 120 px of screen, asked for at 480: enlarging would hand the user a blurry lie and tell them it
        // is high resolution.
        let small = thumbnail_base64(&rgb(120, 90, 180), 120, 90, 120, 70).expect("encode");
        let bytes = STANDARD.decode(&small).expect("base64");
        let redrawn = preview_from_bytes(&bytes, 480, 70).expect("preview");
        assert_eq!(stored_width(&redrawn), Some(120), "a source smaller than the target keeps its size");
        let shrunk = preview_from_bytes(&bytes, 60, 70).expect("preview");
        assert_eq!(stored_width(&shrunk), Some(60), "and a bigger request than the source is answered by the source");
        let tall = thumbnail_base64(&rgb(400, 1200, 120), 400, 1200, 400, 70).expect("encode");
        let tall_bytes = STANDARD.decode(&tall).expect("base64");
        assert_eq!(stored_width(&preview_from_bytes(&tall_bytes, 240, 70).unwrap()), Some(240), "a portrait grab still lands on the target width");
    }

    /// The pixel the plan accepts and the pixel the worker draws have to be the same pixel. This is the
    /// regression the first live run found: 1920 px in, 239 px out, and every idle pass after that asked
    /// for the same 150 rows again.
    #[test]
    fn a_redraw_lands_exactly_where_the_plan_says_the_row_is_finished() {
        for (source, target) in [(1920u32, 240u32), (1912, 240), (1280, 240), (1080, 240), (247, 240), (5000, 240), (333, 480)] {
            let grab = thumbnail_base64(&rgb(source as usize, (source as usize) / 2, 170), source as usize, (source as usize) / 2, source, 80)
                .expect("encode");
            let bytes = STANDARD.decode(&grab).expect("base64");
            let drawn = preview_from_bytes(&bytes, target, 72).expect("preview");
            let got = stored_width(&drawn).unwrap_or(0);
            assert_eq!(got, target.min(source), "a {source} px grab at target {target} drew {got}");
            let source_path = PathBuf::from("in-memory");
            let reachable = match &Source::Screenshot(source_path) {
                Source::Screenshot(_) => target.min(source),
                Source::Video(..) => target,
            };
            assert_eq!(got, reachable, "the plan would accept this row as finished");
        }
    }

    #[test]
    fn a_picture_that_is_not_a_picture_fails_instead_of_panicking() {
        assert!(preview_from_bytes(b"", 240, 70).is_err());
        assert!(preview_from_bytes(b"definitely not a jpeg", 240, 70).is_err());
        assert!(preview_from_file(Path::new("Z:/definitely/not/here.jpg"), 240, 70).is_err());
    }

    #[test]
    fn ffmpeg_that_is_not_there_reports_which_program_it_asked_for() {
        let error = preview_from_video(Path::new("Z:/no/such/ffmpeg.exe"), Path::new("Z:/no/such/video.mp4"), 4, 240, 70).expect_err("must fail");
        assert!(error.contains("Z:/no/such/ffmpeg.exe"), "{error}");
    }

    /// A config rooted at a temporary directory, so `may_continue` — the only thing [`apply`] asks of it —
    /// finds no stop request and every job gets claimed.
    fn config_at(root: &Path) -> Config {
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(root.join("config_src").join("config_default.json"), r#"{"user_name":"default"}"#).unwrap();
        Config::load(root).unwrap()
    }

    /// The pool gets a month's rows in any order it likes and finishes them in whatever order the lanes
    /// happen to come back; the database may not. Every row here is drawn from its own frame, and every
    /// frame is a different width, so a stored preview's width names the row it belongs to: if answers were
    /// folded by completion instead of by job, the tall grabs (the slow ones, alternating with the short
    /// ones) would land on their neighbours. The row whose frame is not a picture is the other half of the
    /// proof — it keeps the stamp-sized preview it had rather than being written as nothing.
    #[test]
    fn redraws_from_several_lanes_land_on_their_own_rows_and_a_failed_row_keeps_its_picture() {
        let root = scratch("lanes");
        let cache = root.join("cache_screenshot");
        let slice = cache.join("2026-09-21_10-00-00-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        let start = LocalParts::from_stamp("2026-09-21_10-00-00").unwrap().naive_epoch_seconds();
        let stamp = thumbnail_base64(&rgb(600, 300, 200), 600, 300, 70, 40).expect("encode");

        // More rows than the widest `Duty::Decode` lane count can be, so the pooled path is the one under
        // test even on a machine with eight of them.
        let rows_expected = 14usize;
        let broken = 3usize;
        let mut rows = Vec::new();
        let mut widths = Vec::new();
        for index in 0..rows_expected {
            let when = start + index as i64 * 10;
            let name = LocalParts::from_naive_epoch(when).stamp();
            let width = 100 + index as u32 * 10;
            let height = if index % 2 == 0 { 1400 } else { 200 };
            let grab = thumbnail_base64(&rgb(width as usize, height, 180), width as usize, height, width, 85).expect("encode");
            let bytes = STANDARD.decode(&grab).expect("base64");
            std::fs::write(slice.join(format!("{name}.jpg")), &bytes).expect("frame");
            widths.push(width);
            rows.push(row_at(index as i64 + 1, "2026-09-21_10-00-00.mp4", when, Some(stamp.clone())));
        }
        std::fs::write(
            slice.join(format!("{}.jpg", LocalParts::from_naive_epoch(start + broken as i64 * 10).stamp())),
            b"these are not a jpeg",
        )
        .expect("junk");

        let videos = DiskVideos::scan(&root.join("no-videos"));
        let slices = Slices::scan(&cache);
        let plan = plan(&rows, &slices, &videos, 240);
        assert_eq!(plan.jobs.len(), rows_expected, "a 70 px stamp is work against a wider frame");

        let mut conn = Connection::open_in_memory().unwrap();
        wind_store::schema::ensure_schema(&conn).unwrap();
        for row in &rows {
            conn.execute(
                "INSERT INTO video_text (rowid, videofile_name, picturefile_name, videofile_time, ocr_text,
                   is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
                 VALUES (?1, ?2, '42_cropped.jpg', ?3, 'screen text', 1, 1, ?4, '', '')",
                rusqlite::params![row.rowid, row.videofile_name, row.time, row.thumbnail],
            )
            .unwrap();
        }

        // Nothing here reaches the video door, so ffmpeg is never run; the name is only there because
        // [`apply`] asks for one.
        let outcome = apply(&config_at(&root), &mut conn, &plan, 240, 70, Path::new("Z:/no/such/ffmpeg.exe")).expect("apply");
        assert_eq!(outcome.planned, rows_expected);
        assert_eq!(outcome.from_screenshot, rows_expected, "every row still on disk, so every row from a frame");
        assert_eq!(outcome.from_video, 0);
        assert_eq!(outcome.failed, 1, "the one row whose frame is not a picture");
        assert_eq!(outcome.written, rows_expected - 1, "one transaction, every other row in it");

        for (index, width) in widths.iter().enumerate() {
            let stored: Option<String> =
                conn.query_row("SELECT thumbnail FROM video_text WHERE rowid = ?1", [index as i64 + 1], |row| row.get(0))
                    .unwrap();
            let stored = stored.expect("a row was either redrawn or left exactly as it was");
            if index == broken {
                assert_eq!(stored, stamp, "a row that failed keeps the preview it had instead of going blank");
            } else {
                assert_eq!(stored_width(&stored), Some(*width), "row {} must hold the picture drawn from its own frame", index + 1);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
