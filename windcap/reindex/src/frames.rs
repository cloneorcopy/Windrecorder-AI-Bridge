//! Getting frames out of a video file.
//!
//! Upstream has two implementations and picks between them on the configured encoder
//! (`ocr_manager.extract_iframe`): OpenCV reads every frame and keeps one every ~4 s for normal
//! encoders, and an ffmpeg `select` over I-frames runs for AV1, because libav's keyframe placement is
//! the only thing that keeps decoding an AV1 file bounded. Both paths have to hand back frames whose
//! *filename is the source frame index*, because that number is what the row's timestamp is derived
//! from (`round(frame_index / record_framerate)`), so the naming is load-bearing and not incidental.
//!
//! The OpenCV path is reproduced with ffmpeg rather than ported: a pure-Rust H.264 decoder is not a
//! thing this workspace can vendor offline, and `select='not(mod(n,STEP))' -frame_pts 1` yields the
//! same frames under the same names. Which means every call in this module goes through an argv
//! builder a test asserts on, and no test ever needs ffmpeg present.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `iframe_interval` in `extract_iframe`, in milliseconds.
pub const IFRAME_INTERVAL_MS: i64 = 4000;

/// Which extraction upstream would take for a given `record_encoder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// One frame every ~`IFRAME_INTERVAL_MS`, by source frame index. The non-AV1 path.
    Stride,
    /// Only I-frames. The AV1 path — decoding an AV1 file frame-by-frame costs more than the OCR.
    KeyFrame,
}

/// `if "av1" not in config.record_encoder.lower()` — a substring test on the lowercased encoder name,
/// so `cpu_av1`, `libaom-av1` and `av1_nvenc` all take the second path while `cpu_h264` does not.
pub fn strategy_for_encoder(encoder: &str) -> Strategy {
    if encoder.to_lowercase().contains("av1") {
        Strategy::KeyFrame
    } else {
        Strategy::Stride
    }
}

/// `frame_step = int(fps * iframe_interval / 1000)`.
///
/// Truncating, as Python's `int()` does, and floored at 1: a sub-0.25 fps source would make the step
/// zero and the modulo undefined, and upstream simply crashes there.
pub fn frame_step_for(fps: f64, interval_ms: i64) -> i64 {
    if !fps.is_finite() || fps <= 0.0 {
        return 1;
    }
    // `as i64` truncates toward zero on a positive value, which is what Python's `int()` does here.
    (((fps * interval_ms as f64) / 1000.0) as i64).max(1)
}

/// The ffmpeg argv that reads a video's stream line, with no output file.
///
/// `ffmpeg -i x` exits non-zero precisely because nothing was asked of it, which makes it a cheap
/// probe: it parses the container header and prints `... 2 fps, 2 tbr ...` without decoding a frame.
pub fn probe_args(ffmpeg: &Path, video: &Path) -> Vec<String> {
    vec![
        path(ffmpeg),
        "-hide_banner".to_string(),
        "-i".to_string(),
        path(video),
    ]
}

/// The ffmpeg argv for [`Strategy::Stride`].
///
/// `select='not(mod(n\,STEP))'` keeps source frames 0, STEP, 2*STEP… — the same set OpenCV writes —
/// and `-vsync vfr -frame_pts 1` names each output file after the frame's own presentation index, so
/// `8.jpg` is source frame 8. Without `-frame_pts 1` the muxer numbers outputs 1,2,3… and every row in
/// the segment would collapse onto the first few seconds.
pub fn stride_args(ffmpeg: &Path, video: &Path, out_dir: &Path, frame_step: i64) -> Vec<String> {
    let step = frame_step.max(1).to_string();
    args_with_select(
        ffmpeg,
        video,
        out_dir,
        // The backslash is part of the expression, not shell quoting: an unescaped comma would end
        // the `select` filter and start a new one in the chain.
        format!(r"select='not(mod(n\,{step}))'"),
    )
}

/// The ffmpeg argv for [`Strategy::KeyFrame`].
///
/// Upstream's version of this command ends in `-r 1 -f image2`, which re-times the surviving I-frames
/// to one per second *and* numbers the output files 1,2,3…. That numbering is what it then divides by
/// `record_framerate` to get a timestamp, so on an AV1 recording every stored row lands within a few
/// seconds of the segment's start. Kept `-vsync vfr -frame_pts 1` instead, which is what the stride
/// path uses and what makes the timestamp formula honest; the I-frame *selection* itself is unchanged.
pub fn iframe_args(ffmpeg: &Path, video: &Path, out_dir: &Path) -> Vec<String> {
    args_with_select(ffmpeg, video, out_dir, r"select='eq(pict_type\,I)'".to_string())
}

/// The shared shape of both extraction commands.
fn args_with_select(ffmpeg: &Path, video: &Path, out_dir: &Path, select: String) -> Vec<String> {
    vec![
        path(ffmpeg),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        // The working directory is cleared first, but `-y` also removes ffmpeg's interactive prompt,
        // which would otherwise hang a run launched from a service with no console.
        "-y".to_string(),
        "-i".to_string(),
        path(video),
        "-vf".to_string(),
        select,
        "-vsync".to_string(),
        "vfr".to_string(),
        "-frame_pts".to_string(),
        "1".to_string(),
        out_dir.join("%d.jpg").to_string_lossy().replace('\\', "/"),
    ]
}

/// One extracted frame: what it is called, and which source frame it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub index: i64,
    pub original: PathBuf,
}

impl Frame {
    /// The file the OCR step reads. Upstream writes the masked copy beside the original and keeps the
    /// original for the thumbnail, and it stores *this* name in `picturefile_name`, so both the
    /// layout and the column value have to stay in that shape.
    pub fn cropped_path(&self) -> PathBuf {
        self.original.with_file_name(format!("{}_cropped.jpg", self.index))
    }

    pub fn cropped_name(&self) -> String {
        self.cropped_path().file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    }
}

/// The frame index encoded in an extraction output name.
///
/// Digits only, ignoring everything else, which is upstream's `int("".join(filter(isdigit, x)))`. A
/// name with no digits is not a frame and is skipped rather than treated as frame zero.
pub fn frame_index_of(name: &str) -> Option<i64> {
    let digits: String = name.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// The frames present in `out_dir`, oldest first.
pub fn list_frames(out_dir: &Path) -> Result<Vec<Frame>, std::io::Error> {
    let mut frames = Vec::new();
    for entry in std::fs::read_dir(out_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".jpg") || name.contains("_cropped") {
            continue;
        }
        if let Some(index) = frame_index_of(&name) {
            frames.push(Frame { index, original: entry.path() });
        }
    }
    frames.sort_by_key(|f| f.index);
    Ok(frames)
}

#[derive(Debug)]
pub enum ExtractError {
    /// ffmpeg could not be started at all.
    Spawn(std::io::Error),
    /// ffmpeg ran and refused the file — a truncated or non-video `.mp4` reaches here.
    Failed { code: Option<i32>, stderr: String },
    /// The command succeeded but produced nothing, which for a video means "nothing to index".
    Empty,
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::Spawn(e) => write!(f, "could not start ffmpeg: {e}"),
            ExtractError::Failed { code, stderr } => write!(f, "ffmpeg exited {code:?}: {stderr}"),
            ExtractError::Empty => f.write_str("ffmpeg produced no frames"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Pull a video's frames into `out_dir`, which must already exist and be empty.
pub fn extract(
    ffmpeg: &Path,
    video: &Path,
    out_dir: &Path,
    strategy: Strategy,
    frame_step: i64,
) -> Result<Vec<Frame>, ExtractError> {
    let args = match strategy {
        Strategy::Stride => stride_args(ffmpeg, video, out_dir, frame_step),
        Strategy::KeyFrame => iframe_args(ffmpeg, video, out_dir),
    };
    // argv[0] is the program, the rest are its arguments.
    let (program, rest) = args.split_first().expect("argv builders never return an empty vector");
    let output = Command::new(program).args(rest).output().map_err(ExtractError::Spawn)?;
    if !output.status.success() {
        return Err(ExtractError::Failed {
            code: output.status.code(),
            stderr: wind_base::decode_console_bytes(&output.stderr).trim().to_string(),
        });
    }
    let frames = list_frames(out_dir).unwrap_or_default();
    if frames.is_empty() {
        return Err(ExtractError::Empty);
    }
    Ok(frames)
}

/// The video's frame rate as ffmpeg reports it, or `None` when the line is absent or unreadable.
///
/// This is `cv2.VideoCapture(path).get(cv2.CAP_PROP_FPS)` — it drives only the sampling stride, so a
/// caller that cannot probe falls back to the configured recording rate, which is the true value for
/// anything Windrecorder itself recorded.
pub fn probe_framerate(ffmpeg: &Path, video: &Path) -> Option<f64> {
    let args = probe_args(ffmpeg, video);
    let (program, rest) = args.split_first()?;
    let output = Command::new(program).args(rest).output().ok()?;
    // Deliberately not checking the exit status: this invocation always fails, and its stderr is the
    // whole point.
    let text = wind_base::decode_console_bytes(&output.stderr);
    parse_stream_framerate(&text)
}

/// Pull the `fps` figure out of an ffmpeg stream description.
///
/// Reads the first `Stream #…` line, since that is the video stream for every file this product
/// records (audio is never written). The value is `2` for `1920x1080 …, 2 fps, 2 tbr, 16384 tbn`.
pub fn parse_stream_framerate(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.trim_start().starts_with("Stream #"))?;
    // The token has to be there: without it, `1920x1080` would be read back as a frame rate.
    let (before, _) = line.split_once(" fps")?;
    let number: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    number.parse().ok().filter(|v: &f64| *v > 0.0 && v.is_finite())
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffmpeg() -> PathBuf {
        PathBuf::from("C:/ffmpeg/ffmpeg.exe")
    }

    #[test]
    fn only_the_encoder_name_decides_the_strategy() {
        assert_eq!(strategy_for_encoder("cpu_h264"), Strategy::Stride);
        assert_eq!(strategy_for_encoder("gpu_h264_nvenc"), Strategy::Stride);
        assert_eq!(strategy_for_encoder("cpu_av1"), Strategy::KeyFrame);
        assert_eq!(strategy_for_encoder("libaom-av1"), Strategy::KeyFrame);
        // Case-insensitive, exactly as `.lower()` makes it.
        assert_eq!(strategy_for_encoder("AV1_NVENC"), Strategy::KeyFrame);
        // A substring, not a whole-word match: `notav1really` takes the I-frame path upstream-side too.
        assert_eq!(strategy_for_encoder("xav1y"), Strategy::KeyFrame);
    }

    #[test]
    fn the_stride_is_truncated_not_rounded_and_never_zero() {
        assert_eq!(frame_step_for(2.0, IFRAME_INTERVAL_MS), 8);
        assert_eq!(frame_step_for(30.0, IFRAME_INTERVAL_MS), 120, "30 fps for 4 s is 120 frames");
        assert_eq!(frame_step_for(2.9, IFRAME_INTERVAL_MS), 11, "int(11.6) is 11");
        assert_eq!(frame_step_for(0.1, IFRAME_INTERVAL_MS), 1, "a 400ms step would divide by zero");
        assert_eq!(frame_step_for(f64::NAN, IFRAME_INTERVAL_MS), 1);
        assert_eq!(frame_step_for(0.0, IFRAME_INTERVAL_MS), 1);
    }

    /// The argv is the contract with ffmpeg, so it is asserted literally: an accidental reordering or
    /// a lost backslash changes which frames come out and nothing else would notice.
    #[test]
    fn the_stride_command_is_exact() {
        let args = stride_args(
            &ffmpeg(),
            Path::new("D:/lib/2026-09-21_21-16-12.mp4"),
            Path::new("D:/cache/i_frames/2026-09-21_21-16-12"),
            8,
        );
        assert_eq!(
            args,
            vec![
                "C:/ffmpeg/ffmpeg.exe",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                "D:/lib/2026-09-21_21-16-12.mp4",
                "-vf",
                r"select='not(mod(n\,8))'",
                "-vsync",
                "vfr",
                "-frame_pts",
                "1",
                "D:/cache/i_frames/2026-09-21_21-16-12/%d.jpg",
            ]
        );
    }

    #[test]
    fn the_key_frame_command_is_exact() {
        let args = iframe_args(
            &ffmpeg(),
            Path::new("v.mp4"),
            Path::new("out"),
        );
        assert_eq!(
            args,
            vec![
                "C:/ffmpeg/ffmpeg.exe",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                "v.mp4",
                "-vf",
                r"select='eq(pict_type\,I)'",
                "-vsync",
                "vfr",
                "-frame_pts",
                "1",
                "out/%d.jpg",
            ]
        );
    }

    #[test]
    fn the_probe_command_asks_for_nothing_but_the_header() {
        let args = probe_args(&ffmpeg(), Path::new("v.mp4"));
        assert_eq!(args, vec!["C:/ffmpeg/ffmpeg.exe", "-hide_banner", "-i", "v.mp4"]);
        // No output file means no decode, and it is the stderr of the resulting failure that is read.
        assert!(!args.iter().any(|a| a.ends_with(".jpg")));
    }

    #[test]
    fn framerate_is_read_from_the_stream_line_not_from_a_guess() {
        let text = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'gop.mp4'\n  Duration: 00:00:20.00, start: 0.000000, bitrate: 54 kb/s\n  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, unknown/bt706-2-1, progressive), 1920x1080 [SAR 1:1 DAR 16:9], 51 kb/s, 2 fps, 2 tbr, 16384 tbn (default)\n";
        assert_eq!(parse_stream_framerate(text), Some(2.0));
        assert_eq!(
            parse_stream_framerate("  Stream #0:0: Video: vp9, 3840x2160, 29.97 fps, 30 tbr"),
            Some(29.97)
        );
        assert_eq!(parse_stream_framerate("no streams here"), None);
        assert_eq!(parse_stream_framerate("Stream #0:0: Video: h264, 1920x1080"), None, "no fps field");
        assert_eq!(parse_stream_framerate("Stream #0:0: 0 fps"), None, "a zero rate would divide by it");
    }

    #[test]
    fn frame_indices_come_from_the_name_and_only_from_digits() {
        assert_eq!(frame_index_of("8.jpg"), Some(8));
        assert_eq!(frame_index_of("240.jpg"), Some(240));
        assert_eq!(frame_index_of("8_cropped.jpg"), Some(8));
        assert_eq!(frame_index_of("thumb.jpg"), None);
        assert_eq!(frame_index_of(".jpg"), None);
    }

    #[test]
    fn a_cropped_frame_sits_beside_its_original() {
        let frame = Frame { index: 8, original: PathBuf::from("D:/cache/i_frames/seg/8.jpg") };
        assert_eq!(frame.cropped_path(), PathBuf::from("D:/cache/i_frames/seg/8_cropped.jpg"));
        assert_eq!(frame.cropped_name(), "8_cropped.jpg");
    }

    #[test]
    fn listing_frames_skips_the_cropped_copies_and_sorts_numerically() {
        let dir = std::env::temp_dir().join(format!("windcap-reindex-frames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["16.jpg", "8.jpg", "0.jpg", "8_cropped.jpg", "notes.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let frames = list_frames(&dir).unwrap();
        assert_eq!(
            frames.iter().map(|f| f.index).collect::<Vec<_>>(),
            vec![0, 8, 16],
            "string order would put 16 before 8 and put every row in the wrong place in the segment"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
