//! Which OCR engines this machine can actually run, and how do we know.
//!
//! This is the command a user reaches for when their index is empty and they do not know why. The
//! Python equivalent is `ocr_manager.ocr_benchmark`, which is a good idea with two defects that turn
//! directly into wrong answers here, so both are fixed and both are worth naming:
//!
//!   * **a missing language is scored, not reported.** `ocr_image_ms` shells out to
//!     `Windows.Media.Ocr.Cli.exe` and returns its stdout *without looking at the exit status*. When the
//!     requested language is not installed the tool prints `ERROR: Language ja-jp is not supported` on
//!     stdout **and exits 0**, so `compare_strings` measures how much that error message resembles a page
//!     of Japanese prose and reports `available_check: true` with a low accuracy. The benchmark tells the
//!     user the engine works and is merely imprecise. Verified on this machine: `-l ja-jp` prints that
//!     line with exit code 0.
//!   * **accuracy is a character-set overlap, not a string comparison.** `len(set(a) & set(b)) /
//!     len(set(a) | set(b))`, upstream's own `TODO: WTF is this?` comment attached. It is retained —
//!     changing it would change which engines look good on a machine where they are not — but it is
//!     reported under that name and never as "how much of the text was read correctly".
//!
//! Everything is driven by the real fixtures in `__assets__/`: `OCR_test_1080_<lang>.png` paired with
//! `OCR_test_1080_words_<lang>.txt`. The pairs are *discovered from the directory*, not from a hard-coded
//! map, so a language added to `__assets__/` is probed the next time this runs rather than after someone
//! remembers to edit `const.py`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use wind_base::config::Config;

use crate::hash;

/// The threshold `compare_strings` uses to call a match.
pub const ACCURACY_THRESHOLD: f64 = 70.0;

/// The prefix the shipped fixtures share, and the word-list suffix.
const IMAGE_PREFIX: &str = "OCR_test_1080_";
const WORDS_INFIX: &str = "OCR_test_1080_words_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Ran, produced text, and the text resembles the expected text.
    Available,
    /// Not installed at all: the executable, the language pack, or the model folder is not there.
    Missing,
    /// Installed and invoked, and it failed. The detail is the tool's own words.
    Failing,
    /// Present on disk but not something this binary can drive. Reported rather than guessed at.
    NotDriveable,
    /// Installed and reachable, but *nothing was run against it*: there were no `__assets__/`
    /// fixture pairs to read. This is deliberately NOT `Failing`. A `Failing` row means "we tested
    /// the engine and it did not work"; this row means "we could not test it at all", and the two
    /// must not share a word, a colour, or an exit code. Conflating them is the false negative this
    /// command shipped with: a standalone install with no fixtures reported a perfectly good engine
    /// as `installed but failing / no fixture pairs were found`, and then exited 1 as if the engine
    /// were broken.
    Untested,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Available => "available",
            Status::Missing => "missing",
            Status::Failing => "installed but failing",
            Status::NotDriveable => "present, not driveable from Rust",
            Status::Untested => "untested (no fixtures)",
        }
    }

    /// Non-ASCII-free, so a cp936 console cannot mangle the one column the user is reading to decide
    /// whether their machine is broken.
    pub fn ascii(self) -> &'static str {
        match self {
            Status::Available => "OK",
            Status::Missing => "MISSING",
            Status::Failing => "FAILING",
            Status::NotDriveable => "HOSTED-BY-PYTHON",
            // Not "FAILING", not "OK": the machine's OCR is neither proven good nor proven bad here.
            Status::Untested => "NO-FIXTURES",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone)]
pub struct Probe {
    pub engine: String,
    pub language: String,
    pub status: Status,
    /// The tool's own error text, or the reason something is missing, or a version line.
    pub detail: String,
    pub accuracy: Option<f64>,
    pub elapsed_ms: Option<u128>,
    /// `__assets__/OCR_test_1080_xx.png`, when a fixture was used.
    pub fixture: Option<PathBuf>,
    /// The first characters the engine produced, for a reader that can display them.
    ///
    /// `--json` is the channel for this: on a cp936 console a Chinese sample renders as garbage, and the
    /// one field that would prove the decode worked becomes the thing that misleads the engineer reading
    /// it. Redirect `--json` to a file and open it as UTF-8 instead of trusting the terminal.
    pub sample: Option<String>,
}

impl Probe {
    fn new(engine: &str, language: &str, status: Status, detail: impl Into<String>) -> Probe {
        Probe {
            engine: engine.to_string(),
            language: language.to_string(),
            status,
            detail: detail.into(),
            accuracy: None,
            elapsed_ms: None,
            fixture: None,
            sample: None,
        }
    }
}

/// Everything probed, in the order a reader should see it.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub probes: Vec<Probe>,
    /// The engine the config currently selects, so "available" and "in use" can be compared.
    pub configured_engine: String,
    pub configured_language: String,
}

/// The three answers `check-engines` can give, kept distinct so a human and a script cannot read one
/// as another. The bug this exists to kill collapsed the third into the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// At least one engine was run against a real fixture and produced text above the threshold.
    Usable,
    /// The self-check ran against real fixtures and no engine produced usable text. A genuine
    /// problem: exit like a failure, because it is one.
    Unusable,
    /// The self-check never ran: there were no `__assets__/` fixture pairs to read, so no engine
    /// was exercised. Not a statement that any engine is broken — only that this command could not
    /// tell. Neither exit 0 (silently "fine") nor exit 1 (loudly "broken") describes this; it gets a
    /// third code of its own.
    Untestable,
}

impl Verdict {
    /// The word that goes in `--json`'s `outcome` field, and that a script switches on. Chosen to be
    /// unambiguous even in isolation: `untestable-no-fixtures` names the *cause*, so it can never be
    /// misread as "the engine is bad".
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Usable => "usable",
            Verdict::Unusable => "unusable",
            Verdict::Untestable => "untestable-no-fixtures",
        }
    }

    /// The process exit code for this verdict. `0` verified-good, `1` verified-bad, `3` could-not-
    /// verify. `3` is chosen so it is neither `0`/`1` (which already mean something for this command)
    /// nor `2` (`main`'s "you passed bad arguments" code), and so a script cannot confuse "we never
    /// ran the check" with "the check ran and failed".
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Usable => 0,
            Verdict::Unusable => 1,
            Verdict::Untestable => 3,
        }
    }
}

impl Report {
    pub fn usable(&self) -> bool {
        self.probes.iter().any(|p| p.status == Status::Available)
    }

    /// The verdict. The order is the whole point: a passing run is a passing run, and only when
    /// nothing passed do we ask *why* — because an installed-but-never-tested engine (`Untested`)
    /// means "no fixtures", which is a different claim from "we tested and it failed".
    pub fn verdict(&self) -> Verdict {
        if self.usable() {
            return Verdict::Usable;
        }
        if self.probes.iter().any(|p| p.status == Status::Untested) {
            return Verdict::Untestable;
        }
        Verdict::Unusable
    }

    /// The exit code implied by [`Report::verdict`].
    pub fn exit_code(&self) -> i32 {
        self.verdict().exit_code()
    }
}

/// Probe every engine this install ships or can reach.
///
/// The root is anchored to this process's working directory first, so that every fixture path handed to
/// a child process is absolute and a `--root .` behaves exactly like `--root C:\Windrecorder`.
pub fn probe(config: &Config) -> Report {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = &crate::pathguard::anchor(&cwd, config.root());
    let mut report = Report {
        configured_engine: wind_base::ocr::configured_name(config),
        configured_language: config.str_or("ocr_lang", "zh-Hans-CN"),
        ..Default::default()
    };
    let fixtures = discover_fixtures(&root.join("__assets__"));

    report.probes.extend(probe_windows_ocr(config, &fixtures));
    report.probes.extend(probe_tesseract(config, &fixtures));
    report.probes.extend(probe_wechat(config, &fixtures));
    report.probes.extend(probe_hosted_engines(config));
    report
}

/// `OCR_test_1080_<lang>.png` → the matching expected-text file, for every pair actually present.
///
/// `const.py:OCR_BENCHMARK_TEST_SET` hard-codes three languages plus a fallback and has no entry for
/// `ja-jp`'s real tag spelling, so a fixture added without a const edit is never tested. Listing the
/// directory makes the fixture set and the test set the same thing.
pub fn discover_fixtures(assets_dir: &Path) -> Vec<(String, PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(assets_dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let Some(stem) = name.strip_suffix(".png").and_then(|s| s.strip_prefix(IMAGE_PREFIX)) else { continue };
        let stem = stem.to_string();
        // `ja-jp` vs `zh-Hans-CN`: keep the exact spelling the OCR tool's `-l` flag expects, which is the
        // spelling in the file name.
        let words = assets_dir.join(format!("{WORDS_INFIX}{stem}.txt"));
        if words.is_file() {
            out.push((stem, path, words));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// Windows.Media.Ocr.Cli
// ---------------------------------------------------------------------------

const WINDOWS_ENGINE: &str = wind_base::ocr::WINDOWS_ENGINE;

fn probe_windows_ocr(config: &Config, fixtures: &[(String, PathBuf, PathBuf)]) -> Vec<Probe> {
    let mut out = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = crate::pathguard::anchor(&cwd, config.root());
    let exe = config.ocr_exe();
    if !exe.is_file() {
        out.push(Probe::new(
            WINDOWS_ENGINE,
            "-",
            Status::Missing,
            format!("{} is not present in this install", exe.display()),
        ));
        return out;
    }

    // `-s` is the language list. Python slices it as `lines[1:-1]`, which silently keeps a trailing blank
    // line and drops a real language if the tool's output ever changes shape; parsing by "does this look
    // like a language tag" is stable against that.
    let supported = match run(&exe, &["-s".to_string()], Some(&root)) {
        Ok(output) => parse_language_list(&output.stdout),
        Err(e) => {
            out.push(Probe::new(WINDOWS_ENGINE, "-", Status::Failing, format!("{} exists but cannot be run: {e}", exe.display())));
            return out;
        }
    };
    if supported.is_empty() {
        out.push(Probe::new(
            WINDOWS_ENGINE,
            "-",
            Status::Failing,
            format!("{} ran and reported no installed OCR languages; Windows Language Pack installation is the thing to check", exe.display()),
        ));
        return out;
    }

    let mut language_probes: Vec<Probe> = Vec::new();
    for (language, image, words) in fixtures {
        if !supported.iter().any(|s| s.eq_ignore_ascii_case(language)) {
            language_probes.push(Probe::new(
                WINDOWS_ENGINE,
                language,
                Status::Missing,
                format!("no language pack: this machine offers {}", supported.join(", ")),
            ));
            continue;
        }
        language_probes.push(score_engine_run(
            WINDOWS_ENGINE,
            language,
            image,
            words,
            &exe,
            &["-l".to_string(), language.clone(), image.display().to_string()],
            &root,
        ));
    }
    if language_probes.is_empty() {
        // No per-language row was produced, and the only way that happens past the checks above is
        // that there were no fixture pairs at all. Report it as *untested*, never as failing: the
        // engine ran `-s` fine and offered language(s); we simply had nothing to hand it.
        out.push(Probe::new(
            WINDOWS_ENGINE,
            "-",
            Status::Untested,
            format!(
                "{} is installed and reports language(s) [{}], but __assets__/ holds no OCR_test_1080_*/OCR_test_1080_words_* fixture pairs to read, so this engine was never tested. That is not a verdict on the engine — it is the self-check having nothing to check against.",
                exe.display(),
                supported.join(", ")
            ),
        ));
    } else {
        out.extend(language_probes);
    }
    out
}

/// Run one fixture through one engine and turn the result into a probe.
fn score_engine_run(
    engine: &str,
    language: &str,
    image: &Path,
    words: &Path,
    program: &Path,
    args: &[String],
    cwd: &Path,
) -> Probe {
    let expected = match std::fs::read(words) {
        Ok(bytes) => wind_base::ansi::decode_console_bytes(&bytes),
        Err(e) => return Probe::new(engine, language, Status::Failing, format!("{} cannot be read: {e}", words.display())),
    };
    let started = Instant::now();
    let output = match run(program, args, Some(cwd)) {
        Ok(output) => output,
        Err(e) => return Probe::new(engine, language, Status::Failing, format!("cannot be started: {e}")),
    };
    let elapsed = started.elapsed().as_millis();

    // The tool's own failure channel is stdout, with a zero exit status. Checking for `ERROR:` is
    // therefore not optional, and doing it *before* scoring is the fix for the misreport upstream ships.
    // The tool's own failure channel is stdout, with a zero exit status. Checking for `ERROR:` is
    // therefore not optional, and doing it *before* scoring is the fix for the misreport upstream ships.
    if let Some(complaint) = output.stdout.strip_prefix("ERROR:").or_else(|| {
        output
            .stdout
            .lines()
            .find(|line| line.starts_with("ERROR:"))
            .and_then(|line| line.strip_prefix("ERROR:"))
    }) {
        let mut probe = Probe::new(engine, language, Status::Missing, format!("the engine refused it: {}", complaint.trim()));
        probe.elapsed_ms = Some(elapsed);
        probe.fixture = Some(image.to_path_buf());
        return probe;
    }
    if !output.status.success() {
        let mut probe = Probe::new(
            engine,
            language,
            Status::Failing,
            format!("exited with {} — {}", output.status, tail(&output.stderr, &output.stdout)),
        );
        probe.elapsed_ms = Some(elapsed);
        probe.fixture = Some(image.to_path_buf());
        return probe;
    }

    score_output(engine, language, image, &expected, &collapse(&output.stdout), elapsed)
}

/// The verdict every engine probe ends with: what the tool read, against what it should have read.
///
/// Lifted out of [`score_engine_run`] when WeChat OCR became drivable — that engine is not a spawn, so it
/// cannot share the runner, but it must not get to invent its own idea of accuracy either.
fn score_output(engine: &str, language: &str, image: &Path, expected: &str, produced: &str, elapsed: u128) -> Probe {
    let accuracy = character_overlap(produced, &collapse(expected));
    let sample: String = produced.trim().chars().take(60).collect();
    let mut probe = Probe {
        engine: engine.to_string(),
        language: language.to_string(),
        accuracy: Some(accuracy),
        elapsed_ms: Some(elapsed),
        fixture: Some(image.to_path_buf()),
        detail: if accuracy < ACCURACY_THRESHOLD {
            format!(
                "ran, and {:.1}% of its characters overlap the expected text — below the {ACCURACY_THRESHOLD}% threshold, so the language pack may be present but wrong for this script",
                accuracy
            )
        } else {
            format!("ran, {:.1}% character overlap", accuracy)
        },
        status: if accuracy >= ACCURACY_THRESHOLD { Status::Available } else { Status::Failing },
        sample: Some(sample),
    };
    probe.fixture = Some(image.to_path_buf());
    probe
}

fn parse_language_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| is_language_tag(line))
        .map(str::to_string)
        .collect()
}

/// `zh-Hans-CN`, `en-US`, `eng`: a tag is 2-8 alphanumerics, optionally hyphen-segmented.
fn is_language_tag(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 20
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && text.contains(|c: char| c.is_ascii_alphabetic())
        && !text.contains(' ')
}

// ---------------------------------------------------------------------------
// Tesseract
// ---------------------------------------------------------------------------

const TESSERACT_ENGINE: &str = wind_base::ocr::TESSERACT_ENGINE;

fn probe_tesseract(config: &Config, fixtures: &[(String, PathBuf, PathBuf)]) -> Vec<Probe> {
    let mut out = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = crate::pathguard::anchor(&cwd, config.root());
    let configured = config.str_or(wind_base::ocr::TESSERACT_PATH_KEY, "C:\\Program Files\\Tesseract-OCR\\tesseract.exe");
    let candidates = wind_base::ocr::tesseract_candidates(config.root(), &configured);
    // A bare `tesseract` on the list is only a candidate if the loader can actually start it: reporting
    // `installed but failing` for a program that is simply not on this machine's PATH sends the user off
    // to reinstall something that was never installed. `PATH` lookup is the filesystem's to do, so the
    // probe is the spawn itself.
    let program = candidates
        .iter()
        .find(|candidate| is_executable(candidate))
        .cloned();
    let Some(program) = program else {
        out.push(Probe::new(
            TESSERACT_ENGINE,
            "-",
            Status::Missing,
            format!("not found; looked at {}", candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")),
        ));
        return out;
    };

    let listing = match run(&program, &["--list-langs".to_string()], Some(&root)) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            out.push(Probe::new(
                TESSERACT_ENGINE,
                "-",
                Status::Failing,
                format!("{} exited with {}: {}", program.display(), output.status, tail(&output.stderr, &output.stdout)),
            ));
            return out;
        }
        Err(e) => {
            out.push(Probe::new(TESSERACT_ENGINE, "-", Status::Failing, format!("{} cannot be started: {e}", program.display())));
            return out;
        }
    };
    let langs: Vec<String> = listing
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| is_language_tag(line) && *line != "List")
        .map(str::to_string)
        .collect();

    for (language, image, words) in fixtures {
        let code = match wind_base::ocr::tesseract_code(language) {
            Some(code) => code,
            None => {
                out.push(Probe::new(
                    TESSERACT_ENGINE,
                    language,
                    Status::Missing,
                    format!("no translation from the Windrecorder tag \"{language}\" to a Tesseract language code"),
                ));
                continue;
            }
        };
        if !langs.iter().any(|l| l == &code) {
            out.push(Probe::new(
                TESSERACT_ENGINE,
                language,
                Status::Missing,
                format!("this build offers [{}]; {code} is not among them", langs.join(" ")),
            ));
            continue;
        }
        // Tesseract wants `<image> <output-base>`, and `-` sends the text to stdout.
        out.push(score_engine_run(
            TESSERACT_ENGINE,
            language,
            image,
            words,
            &program,
            &[image.display().to_string(), "-".to_string(), "-l".to_string(), code.clone()],
            &root,
        ));
    }
    if fixtures.is_empty() {
        out.push(Probe::new(
            TESSERACT_ENGINE,
            "-",
            Status::Untested,
            format!(
                "{} is installed, but __assets__/ holds no OCR_test_1080_*/OCR_test_1080_words_* fixture pairs to read, so this engine was never tested. That is not a verdict on the engine — it is the self-check having nothing to check against.",
                program.display()
            ),
        ));
    }
    out
}

/// Can this candidate actually be run?
fn is_executable(candidate: &Path) -> bool {
    if candidate.components().count() > 1 {
        return candidate.is_file();
    }
    // A bare program name: ask the loader by running it. `--version` is cheap and universal.
    run(candidate, &["--version".to_string()], None).is_ok()
}

// ---------------------------------------------------------------------------
// The Python-hosted engines
// ---------------------------------------------------------------------------

/// The optional external engines, reported as installed-or-not.
///
/// These are not probeable from a native binary: `ChineseOCR_lite_onnx` is `ocr_lib/chineseocr_lite_onnx`
/// driven through Python and onnxruntime, `PaddleOCR` is the `rapidocr_onnxruntime` package in the venv,
/// and `WeChatOCR` needs the `wechat_ocr` extension plus a WeChat installation with a live IPC callback.
/// Saying "present" is the honest ceiling from here, and the detail names what would have to be true for
/// it to actually index a frame.
fn probe_hosted_engines(config: &Config) -> Vec<Probe> {
    let root = config.root();
    let mut out = Vec::new();

    let col = root.join("ocr_lib").join("chineseocr_lite_onnx");
    let models = col.join("models");
    out.push(match (col.join("model.py").is_file(), models.is_dir()) {
        (true, true) => Probe::new(
            "ChineseOCR_lite_onnx",
            "en-US, zh-Hans, zh-Hant",
            Status::NotDriveable,
            format!("{} and {} are present; recognition runs inside Python's onnxruntime, so the native indexer cannot call it", col.display(), models.display()),
        ),
        (true, false) => Probe::new(
            "ChineseOCR_lite_onnx",
            "en-US, zh-Hans, zh-Hant",
            Status::Failing,
            format!("{} is there but its model folder is not; the engine cannot load", models.display()),
        ),
        (false, _) => Probe::new("ChineseOCR_lite_onnx", "en-US, zh-Hans, zh-Hant", Status::Missing, format!("{} is not in this install", col.display())),
    });

    let paddle = root.join(".venv").join("Lib").join("site-packages").join("rapidocr_onnxruntime");
    out.push(if paddle.is_dir() {
        Probe::new("PaddleOCR", "en-US, zh-Hans, zh-Hant", Status::NotDriveable, format!("{} is installed; recognition runs inside Python", paddle.display()))
    } else {
        Probe::new("PaddleOCR", "en-US, zh-Hans, zh-Hant", Status::Missing, format!("{} is not installed", paddle.display()))
    });

    out
}

// ---------------------------------------------------------------------------
// WeChat OCR
// ---------------------------------------------------------------------------

const WECHAT_ENGINE: &str = wind_base::ocr::WECHAT_ENGINE;

/// Read the fixtures with WeChat's own engine, through the same channel the recorder would use.
///
/// One child serves every fixture, which is the point of the engine being a service — and the reason this
/// probe is written against `Engine` rather than against a spawn helper: the thing under test is the same
/// object the indexer will hold.
fn probe_wechat(config: &Config, fixtures: &[(String, PathBuf, PathBuf)]) -> Vec<Probe> {
    let install = wind_base::wxocr::Install::probe(config.root());
    if !install.is_usable() {
        let why = install.missing.unwrap_or_else(|| "the engine's folder is incomplete".to_string());
        let status = if install.dir.is_dir() { Status::Failing } else { Status::Missing };
        return vec![Probe::new(WECHAT_ENGINE, "en-US, zh-Hans, zh-Hant", status, why)];
    }
    if fixtures.is_empty() {
        return vec![Probe::new(
            WECHAT_ENGINE,
            "-",
            Status::Untested,
            format!("{} is present, but __assets__/ holds no fixture pair to read it with", install.dir.display()),
        )];
    }
    // The engine under test is built from what the probe just found on disk, not from `ocr_engine`: this
    // report scores every engine the machine can run, and reading the user's selection first would make a
    // row appear only after they had already committed to it.
    let engine = wind_base::ocr::Engine::service(config, install.clone(), None);
    let mut out = Vec::new();
    for (language, image, words) in fixtures {
        let expected = match std::fs::read(words) {
            Ok(bytes) => wind_base::ansi::decode_console_bytes(&bytes),
            Err(e) => {
                out.push(Probe::new(WECHAT_ENGINE, language, Status::Failing, format!("{} cannot be read: {e}", words.display())));
                continue;
            }
        };
        let started = Instant::now();
        match engine.recognize(image) {
            Ok(produced) => out.push(score_output(
                WECHAT_ENGINE,
                language,
                image,
                &expected,
                &collapse(&produced),
                started.elapsed().as_millis(),
            )),
            Err(e) => out.push(Probe::new(
                WECHAT_ENGINE,
                language,
                Status::Failing,
                format!("{}: {e}", EXE_HINT),
            )),
        }
    }
    out
}

/// What to say beside a failure, naming the child rather than the protocol.
const EXE_HINT: &str = "WeChatOCR.exe could not read the fixture";

// ---------------------------------------------------------------------------
// The text metric, ported
// ---------------------------------------------------------------------------

/// `utils.wrap_text_by_remove_break`: newlines become spaces, then spaces *between CJK characters* go.
///
/// The second half is what makes the metric usable on Chinese: an OCR tool emits one space between every
/// pair of ideographs, and the reference text has none, so without this a perfect recognition scores as a
/// near miss on exactly the language the app is built for.
pub fn collapse(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    let chars: Vec<char> = flat.chars().collect();
    let mut out = String::with_capacity(flat.len());
    for index in 0..chars.len() {
        if chars[index] == ' ' {
            let before = index.checked_sub(1).map(|i| chars[i]);
            let after = chars.get(index + 1).copied();
            // The decision has to be made *before* emitting, or the space is in the string already and
            // the correction does nothing — which is what the first version of this function did.
            if matches!((before, after), (Some(b), Some(a)) if is_cjk(b) && is_cjk(a)) {
                continue;
            }
        }
        out.push(chars[index]);
    }
    out
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FA5   // the range upstream's own regex `[\u4e00-\u9fa5]` covers
        | 0x3400..=0x4DBF // extension A, which upstream's regex misses and which appears in real names
        | 0xF900..=0xFAFF
        | 0x3040..=0x30FF // kana, so the Japanese fixture is measured the same way
    )
}

/// `len(set(a) & set(b)) / len(set(a) | set(b)) * 100`, upstream's `compare_strings`.
///
/// Reproduced exactly, including the property the author flagged: it compares the *sets of distinct
/// characters*, so `"ababababab"` and `"aaaaabbbbb"` score 100%. Retaining it is what makes this report
/// comparable with every benchmark result the Python app ever printed; replacing it would quietly change
/// which engines look good without changing which engines are good.
pub fn character_overlap(produced: &str, expected: &str) -> f64 {
    if produced.is_empty() && expected.is_empty() {
        return 0.0;
    }
    if produced.chars().all(char::is_whitespace) && expected.chars().all(char::is_whitespace) {
        return 0.0;
    }
    let left: std::collections::BTreeSet<char> = produced.chars().filter(|c| !c.is_whitespace()).collect();
    let right: std::collections::BTreeSet<char> = expected.chars().filter(|c| !c.is_whitespace()).collect();
    let union = left.union(&right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(&right).count() as f64 / union as f64 * 100.0
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Output {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

fn run(program: &Path, args: &[String], cwd: Option<&Path>) -> Result<Output, String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    // The child runs with the *install root* as its working directory, which is the contract the Python
    // app had: `ocr_image_ms` invoked `ocr_lib\Windows.Media.Ocr.Cli.exe` and a bare `__assets__/...`
    // fixture from a process whose cwd was the install. Running from the exe's own folder instead — the
    // obvious guess — resolves every relative fixture against `ocr_lib/`, and the tool then answers
    // "the system cannot find the file specified" for a file that is plainly there, which reads to the
    // user as a broken engine rather than a broken path.
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let output = command
        .output()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    Ok(Output {
        status: output.status,
        // Evidence-based decoding, because this tool writes its result in the console code page: on a
        // zh-CN machine the Chinese fixture comes back as GBK bytes, verified on this install.
        stdout: wind_base::ansi::decode_console_bytes(&output.stdout),
        stderr: wind_base::ansi::decode_console_bytes(&output.stderr),
    })
}

fn tail(stderr: &str, stdout: &str) -> String {
    let source = if stderr.trim().is_empty() { stdout } else { stderr };
    let lines: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect();
    if lines.is_empty() { "(no output)".to_string() } else { lines.join(" | ") }
}

/// Escape every non-ASCII character.
///
/// The console on a zh-CN Windows install is cp936, and `println!` of a UTF-8 Chinese string there does
/// not crash — it prints plausible-looking garbage. For a report whose whole purpose is telling someone
/// whether their OCR is working, that is the worst possible failure mode, so `--ascii` renders the
/// language names and detail text as `\uXXXX`, which is unreadable but never wrong.
pub fn escape_non_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            out.push_str(&format!("\\u{:04X}", c as u32));
        }
    }
    out
}

/// The report as JSON, in the shape `--json` prints.
pub fn to_json(report: &Report) -> serde_json::Value {
    let verdict = report.verdict();
    serde_json::json!({
        "configured_engine": report.configured_engine,
        "configured_language": report.configured_language,
        "usable": report.usable(),
        // The three outcomes, as words and as the code the process exits with, so a script never has
        // to re-derive them from the rows and cannot mistake "not tested" for "failed".
        "outcome": verdict.label(),
        "testable": verdict != Verdict::Untestable,
        "exit_code": report.exit_code(),
        "probes": report.probes.iter().map(|p| serde_json::json!({
            "engine": p.engine,
            "language": p.language,
            "status": p.status.label(),
            "code": p.status.ascii(),
            "detail": p.detail,
            "accuracy_percent": p.accuracy.map(|a| (a * 10.0).round() / 10.0),
            "elapsed_ms": p.elapsed_ms.map(|e| e as u64),
            "fixture": p.fixture.as_ref().map(|f| f.to_string_lossy().to_string()),
            "text_sample": p.sample,
        })).collect::<Vec<_>>(),
    })
}

/// The digest of a fixture, so a report can be tied to the bytes it was produced against.
pub fn fixture_digest(path: &Path) -> String {
    hash::digest_file(path).unwrap_or_else(|_| "unreadable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_pairs_are_discovered_from_disk() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(Path::to_path_buf).unwrap();
        let fixtures = discover_fixtures(&repo.join("__assets__"));
        let languages: Vec<&str> = fixtures.iter().map(|(l, _, _)| l.as_str()).collect();
        assert!(languages.contains(&"en-US"), "{languages:?}");
        assert!(languages.contains(&"zh-Hans-CN"), "{languages:?}");
        assert!(languages.contains(&"ja-jp"), "{languages:?}");
        for (_, image, words) in &fixtures {
            assert!(image.is_file() && words.is_file(), "{image:?} / {words:?}");
            assert!(words.file_name().unwrap().to_string_lossy().starts_with(WORDS_INFIX));
        }
    }

    #[test]
    fn an_image_without_its_word_list_is_not_a_test_set() {
        let dir = std::env::temp_dir().join(format!("wind-setup-fixtures-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("OCR_test_1080_ko-KR.png"), b"png").unwrap();
        assert!(discover_fixtures(&dir).is_empty(), "a picture with nothing to compare against proves nothing");
        std::fs::write(dir.join("OCR_test_1080_words_ko-KR.txt"), b"hangul").unwrap();
        assert_eq!(discover_fixtures(&dir).len(), 1);
        // The words file is never mistaken for an image, and a stray asset is ignored.
        std::fs::write(dir.join("OCR_test_1080_words_en-US.txt"), b"x").unwrap();
        assert_eq!(discover_fixtures(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn language_tags_are_told_apart_from_prose() {
        assert_eq!(
            parse_language_list("All of supported language\nen-US\nzh-Hans-CN\n\n"),
            vec!["en-US".to_string(), "zh-Hans-CN".to_string()],
            "the header and the trailing blank must not become languages"
        );
        assert!(parse_language_list("").is_empty());
        assert!(parse_language_list("ERROR: something is wrong").is_empty());
    }

    #[test]
    fn the_metric_is_the_one_upstream_prints() {
        // Distinct-character sets, so an anagram scores perfectly. This is the property `compare_strings`
        // carries and this report must not pretend otherwise.
        assert!((character_overlap("ababababab", "aaaaabbbbb") - 100.0).abs() < 1e-9);
        assert!((character_overlap("abc", "xyz")).abs() < 1e-9);
        assert!((character_overlap("", "")).abs() < 1e-9);
        assert!((character_overlap("  \n ", "\r")).abs() < 1e-9);
        assert!((character_overlap("hello", "hello") - 100.0).abs() < 1e-9, "identical text scores a perfect overlap");
    }

    #[test]
    fn cjk_line_wrapping_is_removed_before_scoring() {
        assert_eq!(collapse("文 字\n测 试"), "文字测试");
        assert_eq!(collapse("a b c"), "a b c", "latin spacing is not the wrapping being corrected");
        assert_eq!(collapse("中文 英文"), "中文英文");
    }

    /// The probe asks the shared table rather than keeping its own, so a language the settings page offers
    /// is a language this command can actually test — and the table's own rows are pinned in
    /// `wind_base::ocr`'s tests.
    #[test]
    fn tesseract_codes_come_from_the_dispatch_tables_own_rows() {
        assert_eq!(wind_base::ocr::tesseract_code("zh-Hans-CN").as_deref(), Some("chi_sim"));
        assert_eq!(wind_base::ocr::tesseract_code("en-US").as_deref(), Some("eng"));
        assert_eq!(wind_base::ocr::tesseract_code("ja-jp").as_deref(), Some("jpn"));
        assert_eq!(wind_base::ocr::tesseract_code("xx-YY"), None, "an unmapped tag must be reported, not guessed");
    }

    #[test]
    fn escaping_keeps_ascii_and_names_everything_else() {
        // The Japanese and Chinese fixture text is what a cp936 console mangles; the escape must be a
        // faithful rendering, not a transliteration.
        assert_eq!(escape_non_ascii("文字"), "\\u6587\\u5B57");
        assert_eq!(escape_non_ascii("en-US"), "en-US");
        assert_eq!(escape_non_ascii("ja-jp"), "ja-jp");
    }

    // ---- the three outcomes, kept three -------------------------------------
    //
    // `check-engines` used to fold "no fixtures to test against" into "installed but failing" and
    // then exit 1 as if the engine were broken. These tests pin the fix: an engine that was installed
    // but never run gets its own status, its own verdict, its own exit code, and wording that does
    // not say "fail".

    #[test]
    fn no_fixtures_is_a_third_outcome_not_an_engine_failure() {
        let mut report = Report::default();
        report.probes.push(Probe::new(WINDOWS_ENGINE, "-", Status::Untested, "no fixtures to read"));
        assert_eq!(report.verdict(), Verdict::Untestable, "{report:?}");
        assert_eq!(report.exit_code(), 3, "inconclusive is neither success (0) nor failure (1)");
        assert!(!report.usable(), "nothing was proven usable");
        // The word a human reads and the code a script switches on both refuse to say "fail".
        assert!(!Status::Untested.label().contains("fail"), "{}", Status::Untested.label());
        assert_eq!(Status::Untested.ascii(), "NO-FIXTURES", "{}", Status::Untested.ascii());
    }

    #[test]
    fn tested_and_failed_is_a_real_failure() {
        let mut report = Report::default();
        let mut p = Probe::new(WINDOWS_ENGINE, "en-US", Status::Failing, "below the threshold");
        p.fixture = Some(PathBuf::from("__assets__/OCR_test_1080_en-US.png"));
        report.probes.push(p);
        assert_eq!(report.verdict(), Verdict::Unusable, "{report:?}");
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn tested_and_passed_is_exit_zero() {
        let mut report = Report::default();
        let mut p = Probe::new(WINDOWS_ENGINE, "en-US", Status::Available, "ran, 92.7% overlap");
        p.fixture = Some(PathBuf::from("__assets__/OCR_test_1080_en-US.png"));
        report.probes.push(p);
        assert_eq!(report.verdict(), Verdict::Usable, "{report:?}");
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn no_engine_at_all_is_a_real_gap_and_not_the_inconclusive_code() {
        // No `Untested` row: the check found no engine to talk about, which is a genuine "nothing
        // usable" (exit 1) and must not be softened into the "could not verify" code.
        let mut report = Report::default();
        report.probes.push(Probe::new(WINDOWS_ENGINE, "-", Status::Missing, "exe not present"));
        report.probes.push(Probe::new(TESSERACT_ENGINE, "-", Status::Missing, "not found"));
        assert_eq!(report.verdict(), Verdict::Unusable, "{report:?}");
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn the_three_verdicts_have_three_different_exit_codes() {
        let codes = [Verdict::Usable.exit_code(), Verdict::Unusable.exit_code(), Verdict::Untestable.exit_code()];
        assert_eq!(codes, [0, 1, 3], "each outcome needs its own code");
        // `main` reserves 2 for bad arguments, so the inconclusive code must not be that.
        assert_ne!(Verdict::Untestable.exit_code(), 2);
    }

    #[test]
    fn json_names_the_outcome_and_carries_its_exit_code() {
        let mut report = Report::default();
        report.probes.push(Probe::new(WINDOWS_ENGINE, "-", Status::Untested, "no fixtures"));
        let value = to_json(&report);
        assert_eq!(value["outcome"], serde_json::json!("untestable-no-fixtures"), "{value}");
        assert_eq!(value["exit_code"], serde_json::json!(3), "{value}");
        assert_eq!(value["testable"], serde_json::json!(false), "{value}");
        assert_eq!(value["usable"], serde_json::json!(false), "{value}");
    }

    #[test]
    fn a_configured_engine_that_is_not_installed_shows_as_missing_not_empty() {
        let dir = std::env::temp_dir().join(format!("wind-setup-engines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), r#"{"ocr_engine": "Windows.Media.Ocr.Cli"}"#).unwrap();
        let config = Config::load(&dir).unwrap();
        let report = probe(&config);
        let windows = report.probes.iter().find(|p| p.engine == WINDOWS_ENGINE).expect("a row for the shipped engine");
        assert_eq!(windows.status, Status::Missing, "{windows:?}");
        assert!(windows.detail.contains("Windows.Media.Ocr.Cli.exe"), "{:?}", windows.detail);
        assert!(!report.usable(), "an install with no OCR must say so");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The regression this exists for: the probe must run the child with the *install root* as its
    /// working directory, because the fixture paths are relative to it. Pointing the child at
    /// `ocr_lib/` instead — the intuitive choice, since that is where the exe is — makes
    /// `__assets__/OCR_test_1080_en-US.png` unresolvable and the tool reports a missing file for a file
    /// that is there. The report then says the engine is broken. It is not, and the user cannot tell.
    ///
    /// Run with a relative `--root`, because that is what `check-engines --root .` is and what a user
    /// standing in their install folder types.
    #[test]
    fn a_relative_root_still_finds_the_fixtures_and_scores_the_engine() {
        let relative = PathBuf::from("../..");
        if !relative.join("ocr_lib/Windows.Media.Ocr.Cli.exe").is_file() {
            return;
        }
        let config = Config::load(&relative).unwrap();
        let report = probe(&config);
        let english = report
            .probes
            .iter()
            .find(|p| p.engine == WINDOWS_ENGINE && p.language == "en-US")
            .expect("an en-US row");
        assert_eq!(english.status, Status::Available, "{english:?}");
        assert!(
            !english.detail.contains("cannot find") && !english.detail.contains("找不到"),
            "the fixture path was resolved against the wrong directory: {:?}",
            english.detail
        );
        assert!(report.usable());
    }

    #[test]
    fn the_real_engine_on_this_machine_is_probed_end_to_end() {
        // Gated on the shipped binary being present, because this test means something on a recorder
        // workstation and nothing on a CI box with no `ocr_lib`.
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(Path::to_path_buf).unwrap();
        if !repo.join("ocr_lib/Windows.Media.Ocr.Cli.exe").is_file() {
            return;
        }
        let config = Config::load(&repo).unwrap();
        let report = probe(&config);
        let rows: Vec<(&str, &str, Status)> = report
            .probes
            .iter()
            .filter(|p| p.engine == WINDOWS_ENGINE)
            .map(|p| (p.language.as_str(), p.status.label(), p.status))
            .collect();
        assert!(!rows.is_empty(), "the shipped engine must produce at least one per-language row");
        let english = report.probes.iter().find(|p| p.engine == WINDOWS_ENGINE && p.language == "en-US").expect("an en-US row");
        assert_eq!(english.status, Status::Available, "{english:?}");
        // 92.7% on this machine. The bar is the threshold the report itself calls usable, not a
        // hand-picked number: a fixture or an engine that degrades below *that* is the thing a user needs
        // to be told about, and anything above it is a passing run whose exact score varies by build.
        assert!(english.accuracy.unwrap() >= ACCURACY_THRESHOLD, "{english:?}");
        let unsupported = report.probes.iter().find(|p| p.engine == WINDOWS_ENGINE && p.language == "ja-jp");
        if let Some(japanese) = unsupported {
            // A machine with the Japanese pack would legitimately score it; one without must call it
            // missing, which is precisely the misreport the Python benchmark makes.
            assert!(
                matches!(japanese.status, Status::Available | Status::Missing),
                "ja-jp must never be scored-as-working-but-failing: {japanese:?}"
            );
        }
    }
}
