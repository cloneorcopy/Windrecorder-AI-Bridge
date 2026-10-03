//! The OCR step of the reindex path, which is the shared engine over a file that already exists.
//!
//! Which engine that is — and what argv it wants — is [`wind_base::ocr`]'s answer, resolved once into
//! [`crate::index::Settings`] so a back-index cannot disagree with the live recorder about who read the
//! screen. The reindex path is the one place in the pipeline that already has image files on disk (ffmpeg
//! wrote them), so this is *less* awkward than the recorder's version of the same call: hand the path over,
//! read stdout.
//!
//! Two behaviours carried over from `ocr_manager.ocr_image_ms` because both are load-bearing: the working
//! directory stays the install root, since an engine resolves its own language data relative to it; and
//! stdout is decoded through [`wind_base::decode_console_bytes`], because an engine writes the machine's
//! console code page and a lossy UTF-8 read turns the product into mojibake that looks like plausible
//! garbage in a log.

use std::path::{Path, PathBuf};

pub use wind_base::ocr::EngineError;

/// The error type the pipeline reports. Named as it was before the dispatch moved into `wind-base`, so the
/// frame loop reads the same.
pub type OcrError = EngineError;

/// One engine plus the scratch naming this pass writes frames under.
pub struct Engine {
    ocr: wind_base::ocr::Engine,
    /// The base name scratch inputs are derived from. One per engine, reused: the engine is invoked
    /// serially, so there is never more than one input alive at a time.
    input: PathBuf,
}

impl Engine {
    /// Wrap the engine a `Settings` resolved, writing any scratch input under `scratch_dir`.
    pub fn new(ocr: wind_base::ocr::Engine, scratch_dir: &Path) -> Engine {
        Engine { ocr, input: scratch_dir.join("wind_reindex_ocr_input.jpg") }
    }

    pub fn is_installed(&self) -> bool {
        self.ocr.is_installed()
    }

    /// Whether this engine is a resident process rather than a child started per frame — the difference
    /// between a failure that costs milliseconds and one that costs a whole
    /// [`wind_base::wxocr::TASK_TIMEOUT`].
    pub fn is_resident(&self) -> bool {
        self.ocr.is_service()
    }

    /// The invocation, in the engine's own words.
    pub fn describe(&self) -> String {
        self.ocr.describe()
    }

    /// The engine running, which is the config's `ocr_engine` unless [`Self::note`] explains otherwise.
    pub fn name(&self) -> &str {
        self.ocr.name()
    }

    /// Why this is not the engine the config names. A reindex that says nothing here has used what the
    /// user asked for.
    pub fn note(&self) -> Option<&str> {
        self.ocr.note()
    }

    /// Recognise an image file already on disk.
    ///
    /// Returns `Ok(String::new())` for a frame with nothing recognisable in it. That is a result and not an
    /// error: the caller drops it with the same "shorter than three characters" rule the Python loop
    /// applied.
    pub fn recognize_file(&self, image: &Path) -> Result<String, OcrError> {
        self.ocr.recognize(image)
    }

    /// Recognise the scratch copy of a frame, writing `bytes` to a unique name first.
    ///
    /// Reusing one fixed scratch path would make two engines' outputs race if this is ever driven from more
    /// than one thread, and the write is cheap next to the recognition.
    pub fn recognize_bytes(&self, seq: u64, bytes: &[u8]) -> Result<String, OcrError> {
        let path = self.input.with_extension(format!("jpg.{seq}"));
        std::fs::write(&path, bytes).map_err(|e| OcrError::Failed(format!("write {}: {e}", path.display())))?;
        let result = self.recognize_file(&path);
        let _ = std::fs::remove_file(&path);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reindex engine is the shared engine: whatever argv the Windows shape has always had, and nothing
    /// this crate re-declares.
    #[test]
    fn the_engine_hands_the_image_to_what_the_settings_resolved() {
        let program = PathBuf::from("D:/Windrecorder/ocr_lib/Windows.Media.Ocr.Cli.exe");
        let engine = Engine::new(
            wind_base::ocr::Engine::builtin_at(program.clone(), PathBuf::from("D:/Windrecorder"), "zh-Hans-CN"),
            Path::new("D:/cache/i_frames/segment"),
        );
        assert_eq!(engine.describe(), "D:/Windrecorder/ocr_lib/Windows.Media.Ocr.Cli.exe -l zh-Hans-CN <image>");
        assert_eq!(engine.name(), wind_base::ocr::WINDOWS_ENGINE);
        assert!(engine.note().is_none());
    }

    /// A missing engine is the single most common reason a reindex run cannot start, and it has to be an
    /// error the caller can report rather than an `os error 2` from deep inside a frame loop.
    #[test]
    fn a_missing_engine_is_reported_not_panicked() {
        let engine = Engine::new(
            wind_base::ocr::Engine::builtin_at(
                PathBuf::from("Z:/definitely-not-here/Windows.Media.Ocr.Cli.exe"),
                PathBuf::from("."),
                "zh-Hans-CN",
            ),
            &std::env::temp_dir(),
        );
        assert!(!engine.is_installed());
        let err = engine.recognize_file(Path::new("x.jpg")).expect_err("must fail");
        assert!(matches!(err, OcrError::Missing(_)), "{err}");
    }
}
