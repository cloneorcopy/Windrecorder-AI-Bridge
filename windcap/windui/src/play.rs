//! The player: how a segment's seconds reach the egui window without a decoder in this workspace.
//!
//! The HTML window has a media element and needed only a byte door (`winduiweb/src-tauri/src/video.rs`).
//! This window has none: `egui` paints textures, and there is no video or bitstream crate in this
//! workspace's lock — the only codec here is `image` with its jpeg feature, which is what the stored
//! thumbnails need and nothing more. Adding one is not an option the build allows (`cargo build --offline`
//! is the gate), so the pixels come from the same external program the rest of the product already trusts
//! for footage work: `ffmpeg`, which `windmaint` runs to *make* these files and `backend::frame_from_video`
//! already runs to pull one frame out of one.
//!
//! What that leaves is a slideshow with a clock. These segments are one frame per second (`-r 1` in and
//! out, `maint/src/encode.rs:399`), silent (`-an`), and their index is at the head of the file
//! (`+faststart`). So the player does not need frame-accurate timing, audio sync, or a buffer: it needs one
//! decoded picture per second, the second it belongs to, and a way to stop that does not leave an ffmpeg
//! running behind a paused window.
//!
//! Frames travel as JPEG through a pipe rather than raw RGB for one measurable reason: a 1920-wide grab is
//! ~6 MB as `rawvideo` and ~200 KB as a JPEG, at one per second, and the picture arrives at the same cost
//! as the still door that already decodes JPEGs off this very pipeline (`thumbs::decode_jpeg` is reused,
//! so there is one decoder path in the window, not two).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// What a stream run produced, in the words the window reports.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Frames handed to the caller. Zero with no `failure` means the segment ran out, which is an ending,
    /// not an error.
    pub frames: i64,
    /// A sentence for a run that could not start or died mid-way. The window shows it rather than painting
    /// black, because "this machine cannot decode h265" and "this row's footage was deleted" look the same
    /// from inside a black rectangle.
    pub failure: Option<String>,
}

/// One request to show a segment from a given second.
#[derive(Debug, Clone)]
pub struct Source {
    pub ffmpeg: PathBuf,
    pub segment: PathBuf,
    /// Where to start. `-ss` before `-i` is the accurate form for these files: the encoder wrote one
    /// frame per second, so the frame at second S is the only candidate and there is nothing to round.
    pub at: i64,
}

impl Source {
    pub fn new(ffmpeg: PathBuf, segment: PathBuf, at: i64) -> Source {
        Source { ffmpeg, segment, at: at.max(0) }
    }
}

/// The argument vector that streams JPEGs, without the program itself.
///
/// `-loglevel error` because stderr is not read while the pipe is running, and ffmpeg stops writing to a
/// full stderr pipe the same way it stops writing to a full stdout one. The duration this window needs is
/// asked for separately, by [`probe_args`], which does read it.
pub fn stream_args(source: &Source) -> Vec<String> {
    let segment = source.segment.to_string_lossy().into_owned();
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-ss".into(),
        source.at.to_string(),
        "-i".into(),
        segment,
        "-f".into(),
        "image2pipe".into(),
        "-c:v".into(),
        "mjpeg".into(),
        // The still door uses ffmpeg's own default quality for the frame it extracts; `-q:v 5` is the
        // visually-lossless end of that scale and costs about a third of what the default costs to read.
        "-q:v".into(),
        "5".into(),
        "-".into(),
    ]
}

/// The argument vector that asks one file how long it is.
///
/// `ffmpeg -i file` with no output fails on purpose: it prints its banner, the stream table and the
/// `Duration:` line to stderr, then exits. That is the cheapest thing on this machine that reports a
/// duration, and it costs one short process rather than a second binary (`ffprobe`) the payload does not
/// ship and `windsetup doctor` does not look for.
pub fn probe_args(segment: &Path) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-i".into(),
        segment.to_string_lossy().into_owned(),
    ]
}

/// `Duration: 00:03:03.00` → `183`.
///
/// Rounded down to whole seconds, which is the only granularity a one-frame-per-second slideshow has.
pub fn duration_from_log(text: &str) -> Option<i64> {
    let line = text.lines().find_map(|line| line.trim().strip_prefix("Duration:"))?;
    let mut parts = line.trim().split(':');
    let hours: i64 = parts.next()?.trim().parse().ok()?;
    let minutes: i64 = parts.next()?.trim().parse().ok()?;
    // The seconds field carries the fraction and then whatever ffmpeg printed after the time — `, start:
    // 0.000000` — so it is parsed up to the first non-numeric character rather than as a whole token.
    let seconds: f64 = parts.next()?.trim().split(',').next()?.parse().ok()?;
    Some(hours * 3600 + minutes * 60 + seconds.floor() as i64)
}

/// The frames in a JPEG stream, found by their end marker.
///
/// Baseline JPEG entropy coding stuffs every `0xFF` with a `0x00`, so the pair `FF D9` cannot appear
/// inside scan data — it is the marker, and the marker is where a frame ends. Chunk boundaries fall
/// anywhere, so the remainder is kept between calls rather than assumed to line up.
#[derive(Debug, Default)]
pub struct JpegRuns {
    held: Vec<u8>,
}

impl JpegRuns {
    /// Every complete frame in `bytes` appended so far, each one ending at its `FF D9`.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.held.extend_from_slice(bytes);
        let mut found = Vec::new();
        let mut from = 0usize;
        while let Some(offset) = find_end(&self.held[from..]) {
            let end = from + offset + 2;
            found.push(self.held[from..end].to_vec());
            from = end;
        }
        self.held.drain(..from);
        found
    }

    /// Bytes left over after the stream closed. `Some` means the encoder stopped inside a frame, which the
    /// caller reports rather than decoding half a picture.
    pub fn trailing(&self) -> Option<usize> {
        (!self.held.is_empty()).then_some(self.held.len())
    }
}

fn find_end(hay: &[u8]) -> Option<usize> {
    hay.windows(2).position(|pair| pair == [0xFF, 0xD9])
}

/// One ffmpeg process, streaming frames to `on_frame` until the segment ends, the caller stops it, or it
/// fails.
///
/// The pipe is read on a detached thread and handed over as chunks, which is what keeps [`stop`] honest:
/// a blocking `read` would not notice a stop request until ffmpeg produced the next second of pictures, so
/// pausing would leave the process alive for up to a second. With the channel, the loop wakes every 100 ms
/// whether or not data arrived, and the child is killed before this returns — so a paused window holds no
/// ffmpeg, which is the difference between a player and a leak.
pub fn stream(source: &Source, stop: &AtomicBool, mut on_frame: impl FnMut(i64, Vec<u8>)) -> Outcome {
    let program = source.ffmpeg.to_string_lossy().into_owned();
    let mut command = std::process::Command::new(&program);
    command.args(stream_args(source)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // The same `CREATE_NO_WINDOW` `backend::frame_from_video` and `locate` carry. Without it every
        // play gives ffmpeg a console of its own, which takes the foreground from the window that asked.
        command.creation_flags(0x0800_0000);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Outcome { failure: Some(format!("could not start {program}: {e}")), ..Default::default() },
    };
    let Some(stdout) = child.stdout.take() else {
        return Outcome { failure: Some(format!("{program} gave us no output pipe")), ..Default::default() };
    };
    let (sender, receiver) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut reader = stdout;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if sender.send(chunk[..read].to_vec()).is_err() {
                        // The player went away mid-read. Dropping the sender ends `stream`, which kills the
                        // child; ffmpeg's own stdout closing makes it exit on its own too.
                        break;
                    }
                }
            }
        }
    });

    let mut runs = JpegRuns::default();
    let mut shown = 0i64;
    let started = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                for frame in runs.push(&bytes) {
                    on_frame(source.at + shown, frame);
                    shown += 1;
                    // Pace at one picture a second against wall clock. ffmpeg decodes a
                    // one-frame-per-second source far faster than that, and without this the window would
                    // show a whole segment in a burst and then sit on its last frame.
                    let due = started + Duration::from_secs(shown as u64);
                    while Instant::now() < due {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(40));
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let failure = match runs.trailing() {
        Some(leftover) if !stop.load(Ordering::Relaxed) => {
            Some(format!("the segment stopped mid-picture, with {leftover} bytes left over — a truncated or re-compressed file"))
        }
        _ => None,
    };
    Outcome { frames: shown, failure }
}

/// How long one segment is, in seconds, or the sentence explaining why that is unknown.
pub fn probe(source: &Source) -> Result<i64, String> {
    let program = source.ffmpeg.to_string_lossy().into_owned();
    let mut command = std::process::Command::new(&program);
    command.args(probe_args(&source.segment)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    // stderr is read to end rather than kept: `output()` does exactly that, and this run's whole purpose is
    // the text ffmpeg prints there.
    let run = command.output().map_err(|e| format!("could not start {program}: {e}"))?;
    let text = String::from_utf8_lossy(&run.stderr).into_owned();
    duration_from_log(&text).ok_or_else(|| match text.lines().find(|line| line.contains("Error") || line.contains("Invalid")) {
        Some(complaint) => format!("{complaint}").trim().to_string(),
        None => format!("{program} reported no duration for {}", source.segment.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Source {
        Source::new(PathBuf::from("ffmpeg"), PathBuf::from("C:/videos/2026-09-26_12-15-48.mp4"), 42)
    }

    /// The two things a wrong argument would break, checked in the vector itself: the seek belongs to the
    /// input (so it lands on the frame at that second), and the output is a JPEG stream on a pipe.
    #[test]
    fn the_stream_asks_for_jpegs_from_the_requested_second() {
        let args = stream_args(&source());
        assert_eq!(&args[..7], ["-hide_banner", "-nostdin", "-loglevel", "error", "-ss", "42", "-i"], "the seek must precede the input: {args:?}");
        assert!(args.windows(2).any(|pair| pair == ["image2pipe", "-c:v"]));
        assert_eq!(args.last().unwrap(), "-", "stdout, not a file the window has to clean up");
    }

    /// A negative offset is not an argument; the caller's row can hold one for a segment that started
    /// before the row was indexed.
    #[test]
    fn a_second_before_zero_is_clamped_rather_than_passed_on() {
        assert_eq!(Source::new(PathBuf::from("ffmpeg"), PathBuf::from("a.mp4"), -7).at, 0);
        assert_eq!(stream_args(&Source::new(PathBuf::from("f"), PathBuf::from("a.mp4"), -7))[5], "0");
    }

    #[test]
    fn a_duration_line_becomes_whole_seconds() {
        let log = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'a.mp4':\n  Duration: 00:03:03.00, start: 0.000000, bitrate: 309 kb/s\n    Stream #0:0: Video: h264 (High) (avc1 / 0x31637661), yuv420p, 1920x1166, 1 fps, 1 tbr\n";
        assert_eq!(duration_from_log(log), Some(183));
        assert_eq!(duration_from_log("  Duration: 01:00:00.50, start: 0.0"), Some(3600));
        assert_eq!(duration_from_log("Duration: 00:00:07.00"), Some(7));
        assert_eq!(duration_from_log("no duration anywhere"), None);
        assert_eq!(duration_from_log("Duration: nonsense"), None);
        assert_eq!(duration_from_log(""), None);
    }

    /// Frame boundaries in a byte stream do not line up with chunk boundaries, and a frame split across two
    /// reads must be reported once, complete, when the second arrives.
    #[test]
    fn a_frame_split_across_reads_is_still_one_frame() {
        let first = [0xFFu8, 0xD8, 0x01, 0x02];
        let second = [0x03u8, 0xFF, 0xD9];
        let mut runs = JpegRuns::default();
        assert!(runs.push(&first).is_empty(), "no end marker yet");
        let frames = runs.push(&second);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], [0xFF, 0xD8, 0x01, 0x02, 0x03, 0xFF, 0xD9]);
        assert_eq!(runs.trailing(), None, "nothing was left hanging");
    }

    #[test]
    fn two_frames_in_one_read_are_two_frames_in_order() {
        let mut bytes = vec![0xFF, 0xD8, 0x0A, 0xFF, 0xD9];
        bytes.extend([0xFF, 0xD8, 0x0B, 0x0C, 0xFF, 0xD9]);
        bytes.extend([0xFF, 0xD8, 0x0D]);
        let mut runs = JpegRuns::default();
        let frames = runs.push(&bytes);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1], [0xFF, 0xD8, 0x0B, 0x0C, 0xFF, 0xD9]);
        assert_eq!(runs.trailing(), Some(3), "the half frame is what says the stream was cut");
    }

    /// `FF D9` cannot occur inside baseline scan data (every literal `FF` is stuffed with `00`), which is
    /// the whole reason this framer is safe. The test pins the one input shape that would otherwise prove
    /// it by construction rather than by argument: a stuffed pair must not end a frame.
    #[test]
    fn a_stuffed_ff_does_not_end_a_frame() {
        let mut runs = JpegRuns::default();
        assert!(runs.push(&[0xFF, 0xD8, 0xFF, 0x00, 0xFF, 0x01, 0xFF, 0xD9]).len() == 1);
        assert_eq!(runs.trailing(), None);
    }

    /// The end-to-end promise, against the real binary this product runs: one second in, one picture out,
    /// and the second the caller asked for is where it starts.
    ///
    /// Skips out loud rather than passing quietly when the machine has no ffmpeg, because ffmpeg is not in
    /// the payload and this workspace ships without it by design.
    #[test]
    fn a_generated_segment_streams_one_picture_per_second_from_the_requested_second() {
        let ffmpeg = std::path::Path::new("C:/Windows/System32/ffmpeg.exe");
        let ffmpeg = if ffmpeg.is_file() { ffmpeg.to_path_buf() } else {
            eprintln!("skipped: no ffmpeg at C:\\Windows\\System32\\ffmpeg.exe, which is what this install resolves");
            return;
        };
        let dir = std::env::temp_dir().join(format!("windui-play-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("generated.mp4");
        // The product's own shape: 1 fps, no audio, index at the head.
        let made = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=1", "-t", "4", "-r", "1", "-an", "-movflags", "+faststart"])
            .arg(&clip)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        assert!(made.is_ok_and(|s| s.success()), "the fixture itself could not be encoded");
        let source = Source::new(ffmpeg.clone(), clip.clone(), 0);
        assert_eq!(probe(&source).expect("a duration for the clip we just made"), 4);

        let stop = AtomicBool::new(false);
        let mut seen = Vec::new();
        let outcome = stream(&source, &stop, |second, jpeg| seen.push((second, jpeg.len())));
        assert_eq!(outcome.failure, None, "{:?}", outcome.failure);
        assert_eq!(seen.len(), 4, "one picture per second of a four-second clip");
        assert_eq!(seen.iter().map(|(second, _)| *second).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert!(seen.iter().all(|(_, len)| *len > 1000), "and each is a real picture, not a stub: {seen:?}");

        // Starting at second 2 has to skip the first two, which is the row's own moment on screen.
        let from_two = Source::new(ffmpeg, clip.clone(), 2);
        let mut seconds = Vec::new();
        stream(&from_two, &stop, |second, _| seconds.push(second));
        assert_eq!(seconds, vec![2, 3], "a seek that opened at zero would pass every other check here and still be wrong");
        let _ = std::fs::remove_dir_all(dir);
    }
}
