//! The OCR step, which stays an external process.
//!
//! Which program that is, and what argv it wants, is [`wind_base::ocr`]'s answer — the same one the
//! settings page lists and `windsetup check-engines` probes. This module is the part that is specific to
//! recording: the frame is in memory here rather than on disk, so it has to be encoded and written before
//! the engine can be handed it, and what gets written must be the *masked* copy.
//!
//! That file interface is the one place the pipeline cannot be allocation-free, so the shape is "encode
//! once, hand it over, read stdout". Measured cost on this machine is 0.22-1.2 s per frame, which makes it
//! the dominant term once the session probe was fixed.
//!
//! Because the input is a *file*, it is also the one place a privacy boundary can be missed: whatever bytes
//! land in the scratch path are readable by the engine, and by anyone who finds that path. So the scratch
//! file is written from a masked copy and nothing else, and the only entry point a recording path may call
//! is [`OcrEngine::recognize_masked`].

use std::path::{Path, PathBuf};

use windcap::crop::MaskPlan;

/// JPEG quality of the frame handed to the recogniser. High enough that character shapes survive;
/// `windcap::crop::MASKED_JPEG_QUALITY` is the same number on the reindex side, deliberately, so the two
/// paths read the same image off the same encoder.
pub const OCR_JPEG_QUALITY: u8 = 92;

/// Consecutive frames a resident engine may lose before the recorder stops asking it about every one.
///
/// The number is [`wind_base::wxocr`]'s, because it is a property of that engine's timeout rather than of
/// recording; the reindex pass stops on the same count.
pub const ENGINE_GIVE_UP_AFTER: u32 = wind_base::wxocr::GIVE_UP_AFTER;

/// Frames skipped between retries once that threshold is reached. See [`ENGINE_GIVE_UP_AFTER`].
pub const ENGINE_RETRY_AFTER_FRAMES: u64 = wind_base::wxocr::RETRY_AFTER_FRAMES;

/// Whether to ask the engine about this frame.
///
/// Pure, because the only way to test a backoff is to hand it the numbers it reacts to: a unit test cannot
/// arrange for WeChat's child process to stop answering.
fn asks_engine(consecutive_failures: u32, frames_skipped: u64) -> bool {
    consecutive_failures < ENGINE_GIVE_UP_AFTER || frames_skipped >= ENGINE_RETRY_AFTER_FRAMES
}

#[derive(Debug)]
pub enum OcrError {
    /// The engine program is not where this install says it is.
    EngineMissing(PathBuf),
    Encode(String),
    /// The engine could not be started, or ran and failed. Its own words.
    Engine(String),
}

impl std::fmt::Display for OcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrError::EngineMissing(p) => write!(f, "OCR engine not found at {}", p.display()),
            OcrError::Encode(e) => write!(f, "could not encode input image: {e}"),
            OcrError::Engine(msg) => write!(f, "{msg}"),
        }
    }
}

pub struct OcrEngine {
    engine: wind_base::ocr::Engine,
    input: PathBuf,
    /// Frames skipped since the resident engine was last asked. Only ever non-zero for such an engine,
    /// which is why it needs no explanation in the other engines' behaviour.
    skipped: std::cell::Cell<u64>,
}

impl OcrEngine {
    /// The engine `ocr_engine` selects, with the frame's scratch copy written under `scratch_dir`.
    pub fn from_config(config: &wind_base::Config, scratch_dir: PathBuf) -> OcrEngine {
        OcrEngine::new(wind_base::ocr::Engine::select(config), scratch_dir)
    }

    pub fn new(engine: wind_base::ocr::Engine, scratch_dir: PathBuf) -> OcrEngine {
        OcrEngine { engine, input: scratch_dir.join("windrec_ocr_input.jpg"), skipped: std::cell::Cell::new(0) }
    }

    pub fn is_installed(&self) -> bool {
        self.engine.is_installed()
    }

    /// The invocation, in the engine's own words.
    pub fn describe(&self) -> String {
        self.engine.describe()
    }

    /// The engine that will run, and — when it is not the one configured — why [`Self::note`] says so.
    pub fn name(&self) -> &str {
        self.engine.name()
    }

    pub fn requested(&self) -> &str {
        self.engine.requested()
    }

    /// The configured engine could not be driven, so another one is standing in. A recorder that started
    /// without saying this indexes a user's screen into an engine they never chose.
    pub fn note(&self) -> Option<&str> {
        self.engine.note()
    }

    pub fn program(&self) -> &Path {
        self.engine.program()
    }

    /// Whether this engine is a process the recorder keeps rather than a child it starts per frame. The
    /// difference is what a timeout costs: one frame here, the whole library there.
    ///
    /// Test-only, because the recording path never asks it: [`Self::gate_resident_engine`] reads
    /// `is_service` off the engine itself, and only the two tests that state which kind of engine the
    /// backoff exists for need the answer spelled out.
    #[cfg(test)]
    pub fn is_resident(&self) -> bool {
        self.engine.is_service()
    }

    /// Encode the frame as JPEG, run the engine, return its text.
    ///
    /// Returns Ok("") for a frame with no recognisable text, which is a result, not an error: the caller
    /// drops it via the same "too short" rule the Python loop applies (`len(ocr) < 5`).
    ///
    /// **This is the raw entry point and nothing on a recording path may use it.** It hands the engine every
    /// pixel it is given, including whatever the user asked to keep out of the index. It stays public for
    /// `windrec doctor`, which times the encode and the spawn on a machine where the mask is the thing being
    /// reported on rather than the thing being tested; [`OcrEngine::recognize_masked`] is the one the loop
    /// runs.
    pub fn recognize(&self, rgb: &[u8], width: usize, height: usize) -> Result<String, OcrError> {
        if !self.is_installed() {
            return Err(OcrError::EngineMissing(self.program().to_path_buf()));
        }
        self.gate_resident_engine(wind_base::wxocr::consecutive_failures())?;
        let jpeg = wind_base::image::encode_jpeg(rgb, width, height, OCR_JPEG_QUALITY).map_err(OcrError::Encode)?;
        std::fs::write(&self.input, jpeg).map_err(|e| OcrError::Engine(format!("write {}: {e}", self.input.display())))?;
        self.engine.recognize(&self.input).map_err(|e| match e {
            wind_base::ocr::EngineError::Missing(p) => OcrError::EngineMissing(p),
            other => OcrError::Engine(other.to_string()),
        })
    }

    /// Refuse to ask a resident engine that has stopped answering, until enough frames have gone by.
    ///
    /// The frame is not lost by this: the caller keeps it, indexes its window title, and counts the
    /// unanswered frame in the closing report. What is refused is the wait, not the recording.
    fn gate_resident_engine(&self, consecutive: u32) -> Result<(), OcrError> {
        if !self.engine.is_service() {
            return Ok(());
        }
        if asks_engine(consecutive, self.skipped.get()) {
            self.skipped.set(0);
            return Ok(());
        }
        let waited = self.skipped.get() + 1;
        self.skipped.set(waited);
        Err(OcrError::Engine(format!(
            "{} has failed {consecutive} frames in a row, so frame {waited} of the next \
             {ENGINE_RETRY_AFTER_FRAMES} is not being offered to it; it is recorded and indexed by window title",
            self.engine.name()
        )))
    }

    /// The live path's OCR: paint the user's excluded edges black on a private copy of the frame, and
    /// recognise that.
    ///
    /// The copy is the whole design. `rgb` — the grab the recorder will also write to disk and thumbnail —
    /// is not modified and not resized, so masking costs the user no footage; only the bytes that reach the
    /// recogniser lose the excluded regions. That is upstream's shape: `_crop_ocr_image` writes a separate
    /// `_cropped.png` and hands that to `ocr_image`, while the row, the video list and the thumbnail all
    /// keep naming the untouched screenshot.
    ///
    /// The geometry is `windcap::crop`'s, shared with `wind-reindex`, so a frame cannot be masked one way
    /// live and another way when the same footage is re-indexed from video.
    ///
    /// Returns the text and how many bands were painted, which is what lets the closing report say how many
    /// frames were masked. A zero there means this configuration excludes nothing, and the report must not
    /// imply otherwise.
    pub fn recognize_masked(
        &self,
        rgb: &[u8],
        width: usize,
        height: usize,
        plan: &MaskPlan,
    ) -> Result<(String, usize), OcrError> {
        if !self.is_installed() {
            return Err(OcrError::EngineMissing(self.program().to_path_buf()));
        }
        let (masked, painted) = ocr_input(rgb, width, height, plan);
        let text = self.recognize(&masked, width, height)?;
        Ok((text, painted))
    }
}

/// The bytes that go to the engine, and how many excluded bands were painted to make them.
///
/// Split out so "the copy is masked and the caller's buffer is not" is testable on a machine with no OCR
/// engine installed, which is the only kind of machine a unit test can rely on.
fn ocr_input(rgb: &[u8], width: usize, height: usize, plan: &MaskPlan) -> (Vec<u8>, usize) {
    windcap::crop::masked_copy(rgb, width, height, plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config read from a scratch install directory, so the test decides which engine is selected.
    struct Install {
        dir: PathBuf,
        config: wind_base::Config,
    }

    impl Drop for Install {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn install(tag: &str, body: &str) -> Install {
        let dir = std::env::temp_dir().join(format!("windrec-ocr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), body).unwrap();
        let config = wind_base::Config::load(&dir).unwrap();
        Install { dir, config }
    }

    /// The recorder's engine is the config's engine, with the argv the built-in shape has always had.
    #[test]
    fn the_engine_comes_from_the_config_and_describes_itself() {
        let install = install("describe", r#"{"ocr_engine": "Windows.Media.Ocr.Cli", "ocr_lang": "en-US"}"#);
        let engine = OcrEngine::from_config(&install.config, std::env::temp_dir());
        assert_eq!(engine.name(), wind_base::ocr::WINDOWS_ENGINE);
        assert_eq!(engine.requested(), wind_base::ocr::WINDOWS_ENGINE);
        assert!(engine.note().is_none(), "{:?}", engine.note());
        let described = engine.describe();
        assert!(described.ends_with("Windows.Media.Ocr.Cli.exe -l en-US <image>"), "{described}");
        assert!(described.starts_with(&install.dir.to_string_lossy().into_owned()), "{described}");
    }

    /// A user who selected Tesseract gets Tesseract's argv in the recorder too — the whole point of the key
    /// being honoured, which is what regressed.
    #[test]
    fn a_selected_third_party_engine_reaches_the_recorder() {
        let dir = std::env::temp_dir().join(format!("windrec-ocr-tesseract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        // Forward slashes, because the path goes into a JSON string literal and backslashes would have to
        // be escaped to survive it. Windows accepts either spelling.
        let program = dir.join("tesseract.exe");
        std::fs::write(&program, b"not really").unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            format!(
                r#"{{"ocr_engine": "TesseractOCR", "ocr_lang": "en-US", "TesseractOCR_filepath": "{}"}}"#,
                program.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        let config = wind_base::Config::load(&dir).unwrap();
        let engine = OcrEngine::from_config(&config, std::env::temp_dir());
        assert_eq!(engine.name(), wind_base::ocr::TESSERACT_ENGINE, "{:?}", engine.note());
        assert!(engine.describe().ends_with("<image> - -l eng"), "{}", engine.describe());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The substitution is not allowed to be silent: `note()` is what the startup report prints.
    #[test]
    fn a_config_that_names_an_undrivable_engine_says_so_instead_of_stopping() {
        let install = install("fallback", r#"{"ocr_engine": "PaddleOCR", "ocr_lang": "zh-Hans-CN"}"#);
        let engine = OcrEngine::from_config(&install.config, std::env::temp_dir());
        assert_eq!(engine.name(), wind_base::ocr::WINDOWS_ENGINE);
        assert_eq!(engine.requested(), "PaddleOCR");
        assert!(engine.note().unwrap().contains("PaddleOCR"), "{:?}", engine.note());
    }

    #[test]
    fn a_missing_engine_is_reported_not_panicked() {
        let install = install("missing", r#"{"ocr_engine": "MyOCR", "ocr_engine_command": "Z:\\definitely-not-here\\MyOCR.exe"}"#);
        let engine = OcrEngine::from_config(&install.config, std::env::temp_dir());
        assert!(!engine.is_installed());
        let err = engine.recognize(&[0u8; 12], 2, 2).expect_err("must fail");
        assert!(matches!(err, OcrError::EngineMissing(_)), "{err}");
    }

    /// The privacy control's whole contract, on the bytes the engine is handed: the excluded edges are
    /// black in the copy, the copy is the same size as the frame, and the frame the caller kept is exactly
    /// what it was. If this ever fails by *shrinking* the buffer, masking has become cropping and the user
    /// has lost footage they recorded on purpose.
    #[test]
    fn the_ocr_input_is_masked_and_the_recorders_buffer_is_not() {
        let (w, h) = (200usize, 100usize);
        // Mid-grey, with a bright block in the top-left corner (inside the excluded band) and one in the
        // middle (where text the user wants searchable would be).
        let mut frame = vec![170u8; w * h * 3];
        for (x, y) in [(5usize, 2usize), (100usize, 50usize)] {
            let o = (y * w + x) * 3;
            frame[o] = 250;
            frame[o + 1] = 250;
            frame[o + 2] = 250;
        }
        let before = frame.clone();
        // 10% of 100 rows is 10 and of 200 columns is 20, so (5,2) is inside and (100,50) is outside.
        let plan = MaskPlan::whole_frame(w as u32, h as u32, &[10, 10, 10, 10]);

        let (input, painted) = ocr_input(&frame, w, h, &plan);
        assert_eq!(painted, 4, "all four edges");
        assert_eq!(input.len(), frame.len(), "the OCR input keeps the frame's size — masking, not cropping");
        let corner = (2 * w + 5) * 3;
        assert_eq!(&input[corner..corner + 3], &[0, 0, 0], "the excluded corner is black");
        let centre = (50 * w + 100) * 3;
        assert_eq!(&input[centre..centre + 3], &[250, 250, 250], "the readable centre survived");
        assert_eq!(frame, before, "and the recorder's own pixels were never touched");
    }

    /// An engine that is not there is reported the same way through both doors, so a masked path cannot
    /// fail more quietly than the raw one it replaced.
    #[test]
    fn the_masked_door_reports_a_missing_engine_like_the_raw_one() {
        let install = install("both-doors", r#"{"ocr_engine": "MyOCR", "ocr_engine_command": "Z:\\definitely-not-here\\MyOCR.exe"}"#);
        let engine = OcrEngine::from_config(&install.config, std::env::temp_dir());
        let plan = MaskPlan::whole_frame(2, 2, &[6, 6, 6, 3]);
        let raw = engine.recognize(&[0u8; 12], 2, 2).expect_err("must fail");
        let masked = engine.recognize_masked(&[0u8; 12], 2, 2, &plan).expect_err("must fail the same way");
        assert!(matches!(raw, OcrError::EngineMissing(_)));
        assert!(matches!(masked, OcrError::EngineMissing(_)));
    }

    /// A resident engine built by hand, pointed at files that are not there. `missing: None` is the point:
    /// this is an engine the install claims it can drive and which then stops answering, which is the only
    /// state worth gating — and no test should depend on WeChat's binaries being on the machine.
    fn resident_engine(install: &Install) -> OcrEngine {
        let dir = install.dir.join("wxocr-nowhere");
        OcrEngine::new(
            wind_base::ocr::Engine::service(
                &install.config,
                wind_base::wxocr::Install {
                    exe: dir.join("WeChatOCR.exe"),
                    dll: dir.join("mmmojo_64.dll"),
                    dir,
                    missing: None,
                },
                None,
            ),
            std::env::temp_dir(),
        )
    }

    /// The rule itself, before any of the plumbing that has to agree with it.
    #[test]
    fn a_failing_resident_engine_is_asked_once_per_backoff_window() {
        assert!(asks_engine(0, 0), "nothing has failed: every frame is offered");
        assert!(asks_engine(ENGINE_GIVE_UP_AFTER - 1, u64::MAX), "one short of the limit is still retried at once");
        assert!(!asks_engine(ENGINE_GIVE_UP_AFTER, ENGINE_RETRY_AFTER_FRAMES - 1), "giving up costs a window of frames");
        assert!(asks_engine(ENGINE_GIVE_UP_AFTER, ENGINE_RETRY_AFTER_FRAMES), "and the window ends");
        assert!(
            asks_engine(0, u64::MAX),
            "an engine that answered has reset its counter, so a long skip must not keep it out"
        );
    }

    /// The same rule as it runs: 100 frames against a dead engine cost two waits, not a hundred.
    #[test]
    fn a_dead_resident_engine_does_not_hold_up_the_library_frame_by_frame() {
        let install = install("backoff", r#"{"ocr_engine": "WeChatOCR"}"#);
        let engine = resident_engine(&install);
        assert!(engine.is_resident(), "the gate only exists for this kind of engine");
        let asked = (0..100)
            .filter(|_| engine.gate_resident_engine(ENGINE_GIVE_UP_AFTER * 3).is_ok())
            .count();
        assert_eq!(asked, 2, "one attempt per {ENGINE_RETRY_AFTER_FRAMES} skipped frames, twice in 100");
    }

    /// An engine that gets out of the way is the recorder's normal case, and the backoff must never be what
    /// stops it: the built-in runs a child and returns, in well under a second.
    #[test]
    fn a_command_engine_is_never_gated() {
        let install = install("no-gate", r#"{"ocr_engine": "Windows.Media.Ocr.Cli"}"#);
        let engine = OcrEngine::from_config(&install.config, std::env::temp_dir());
        assert!(!engine.is_resident(), "the built-in is a child per frame, which is why it needs no backoff");
        for _ in 0..100 {
            engine
                .gate_resident_engine(u32::MAX)
                .expect("a failing command engine is still asked, because it fails fast and says why");
        }
    }
}
