//! Making already-recorded video searchable.
//!
//! Windrecorder gets text into its index two ways. The live one — grab a frame, OCR it, write a row —
//! is `windrec`. This crate is the other one: given an `.mp4` that is already on disk, pull its key
//! frames, read them, collapse the repeats, and write the rows. It is what turns a user's years of
//! recordings into something a search can reach, and it is what `OCR_index_strategy = 1` asks for after
//! every finished segment.
//!
//! The port is deliberate about matching the Python it replaces — the same deduplication metric, the
//! same `" -||- "` composition, the same naive-local epoch, the same `-INDEX`/`-OCRED`/`-ERROR1`
//! renaming — because users' libraries were built under those rules and a "better" one silently
//! changes what an old search returns. Where this crate departs from upstream, the reason is written at
//! the function that departs.
//!
//! Every ffmpeg and OCR-engine invocation sits behind a pure argument-vector builder, so the test suite
//! pins the commands without needing either binary.

pub mod crop;
pub mod csvread;
pub mod engine;
pub mod frames;
pub mod index;
pub mod naming;
pub mod text;
pub mod timeline;
pub mod wintitle;

pub use crop::{crop_for_ocr, MaskPlan, Tile};
pub use engine::Engine;
pub use frames::{strategy_for_encoder, Strategy, IFRAME_INTERVAL_MS};
pub use index::{index_video, index_video_path, Settings};
pub use naming::State;
pub use timeline::{frame_offset_seconds, row_time, segment_start_seconds};
pub use wintitle::{optimize_window_title, TitleTable};

/// The error type the one-call-per-video entry points report.
///
/// A failure here is always someone else's cause — a bad file, a missing engine, an unwritable
/// directory — and the useful part is the string, which ends up verbatim in `LOG_ERROR_*.MD`.
#[derive(Debug)]
pub struct ReindexError(pub String);

impl std::fmt::Display for ReindexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReindexError {}
