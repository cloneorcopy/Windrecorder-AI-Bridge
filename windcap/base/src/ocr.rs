//! Which OCR engine the user picked, and how it is invoked.
//!
//! Upstream's contract for an OCR engine — the one `extension/how_to_contribute_third_party_ocr_support.md`
//! asks contributors to satisfy and the one `ocr_manager.ocr_image` dispatched on — is a function whose
//! *input is an image file path* and whose *output is the recognized string*. Only the hosting changed:
//! the Python app dispatched to in-process libraries, and the Python application was deleted in
//! `3f37cbf`. What survives natively is therefore exactly the engines that can run as a program —
//! `Windows.Media.Ocr.Cli` (shipped in `ocr_lib/`), Tesseract (a subprocess, already invoked that way by
//! the old `ocr_image_tesseract`), and anything a user registers as an external command.
//!
//! `WeChatOCR` is the exception, and the recent one: it ran inside Python upstream only because the
//! package that drove it was Python, and [`crate::wxocr`] now speaks the same channel natively. The rest —
//! `PaddleOCR`/RapidOCR and `ChineseOCR_lite_onnx` — are still listed as registered-but-not-available
//! rather than offered as if they could index a frame, because the worst settings list is one that lets you
//! pick something that then silently does nothing.
//!
//! The config keys are upstream's own and unchanged, so a `config_user.json` the Python app wrote reads
//! identically here: `ocr_engine` names the engine, `support_ocr_lst` is the registry its install scripts
//! appended to, `third_party_engine_ocr_lang` is the language list a third-party engine is addressed by,
//! and `TesseractOCR_filepath` is where Tesseract was told to live.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::config::Config;

/// The engine shipped at `ocr_lib/Windows.Media.Ocr.Cli.exe`, and upstream's default.
pub const WINDOWS_ENGINE: &str = "Windows.Media.Ocr.Cli";
/// The name upstream stores for Tesseract, and the one a migrated config carries.
pub const TESSERACT_ENGINE: &str = "TesseractOCR";
/// WeChat's own OCR, driven through `mmmojo_64.dll` by [`crate::wxocr`] instead of a Python package.
pub const WECHAT_ENGINE: &str = "WeChatOCR";

/// The key naming the engine in effect. Written by the settings page, read by the recorder and indexer.
pub const ENGINE_KEY: &str = "ocr_engine";
/// The engine registry the Python install scripts appended to. Still what says which names a user
/// deliberately registered.
pub const SUPPORT_LIST_KEY: &str = "support_ocr_lst";
/// Languages handed to a third-party engine — upstream's key, and still a list, because some engines
/// read several languages in one pass.
pub const THIRD_PARTY_LANG_KEY: &str = "third_party_engine_ocr_lang";
/// Where Tesseract was pointed. The shipped default is upstream's own value.
pub const TESSERACT_PATH_KEY: &str = "TesseractOCR_filepath";
/// The argv of an engine that is neither built-in: strings, program first, with `{image}` and `{lang}`
/// substituted. This is the native shape of upstream's "add a branch to `ocr_image`" step.
pub const COMMAND_KEY: &str = "ocr_engine_command";

const DEFAULT_TESSERACT: &str = r"C:\Program Files\Tesseract-OCR\tesseract.exe";
const WINDOWS_EXE_NAME: &str = "Windows.Media.Ocr.Cli.exe";
/// The two names in the one sentence `describe()` prints for the service engine, so the wording lives with
/// the module that owns the protocol rather than being copied here.
const EXE_LABEL: &str = "WeChatOCR.exe";
const DLL_LABEL: &str = "mmmojo_64.dll";

/// A failed recognition. `Display` is what reaches the recorder's log and the settings page's note.
#[derive(Debug)]
pub enum EngineError {
    /// The engine's program is not where this install says it is.
    Missing(PathBuf),
    /// The program exists and could not be started.
    Spawn(std::io::Error),
    /// It ran and failed. The detail is the tool's own words.
    Failed(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Missing(p) => write!(f, "OCR engine not found at {}", p.display()),
            EngineError::Spawn(e) => write!(f, "could not start the OCR engine: {e}"),
            EngineError::Failed(msg) => write!(f, "OCR engine failed: {msg}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// One selectable row of the engine picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The value written to `ocr_engine`.
    pub name: String,
    /// Can this binary actually drive it right now?
    pub available: bool,
    /// Why — the program found, or the reason it is not offered. Shown to the user verbatim.
    pub detail: String,
}

/// The engine in effect: an argv to run over an image file, plus the language to ask it for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    /// The engine that will actually run — not always [`Engine::requested`], because [`Engine::select`]
    /// degrades rather than refusing to start.
    name: String,
    /// The configured name, whatever it was, so a report can say "you asked for X and got Y".
    requested: String,
    program: PathBuf,
    /// The arguments, in the engine's own shape, with the empty string marking the image slot.
    args: Vec<String>,
    /// Engines resolve their own language data relative to their working directory, so every invocation
    /// keeps the install root as its cwd — the way the Python subprocess ran.
    cwd: PathBuf,
    /// Set for the one engine that is not a command line. `program` is then the child's own exe, and
    /// `args` is empty: a process that loads 21 MB of models per start is reused across frames, so the
    /// argv shape every other engine has would describe something that never happens.
    service: Option<crate::wxocr::Install>,
    lang: String,
    /// Set when the configured engine could not be used and this one is a substitute. The recorder prints
    /// it before the first frame: a silent substitution is how a user's whole index ends up in the wrong
    /// engine.
    note: Option<String>,
}

impl Engine {
    /// Resolve `ocr_engine` against this install. Never fails: an engine that cannot be run degrades to
    /// the Windows one and says why in [`Engine::note`], which is upstream's own behaviour
    /// (`ocr_image`'s `try`/`except` and `reset_ocr_engine_config_to_windows`).
    pub fn select(config: &Config) -> Engine {
        let requested = configured_name(config);
        // `kind` is what the match borrows, so `requested` stays free to move into the engine it selects.
        let kind = requested.clone();
        match kind.as_str() {
            WINDOWS_ENGINE => Engine::windows(config, &requested, None),
            TESSERACT_ENGINE => match tesseract_program(config) {
                None => Engine::windows(
                    config,
                    &requested,
                    Some(format!(
                        "{TESSERACT_ENGINE} is selected in {ENGINE_KEY}, but no tesseract was found at {} \
                         or on PATH; this run uses {WINDOWS_ENGINE} instead",
                        configured_path(config).display()
                    )),
                ),
                Some(program) => {
                    let lang = engine_language(config, &requested);
                    let Some(code) = tesseract_code(&lang) else {
                        return Engine::windows(
                            config,
                            &requested,
                            Some(format!(
                                "{TESSERACT_ENGINE} has no language code for \"{lang}\" and nothing was \
                                 guessed; this run uses {WINDOWS_ENGINE}. Set {THIRD_PARTY_LANG_KEY} to a \
                                 code `tesseract --list-langs` reports"
                            )),
                        );
                    };
                    // `<image> -` writes the text to stdout instead of an `<image>.txt` beside the user's
                    // screenshot cache.
                    Engine {
                        name: requested.clone(),
                        requested,
                        program,
                        args: vec![String::new(), "-".to_string(), "-l".to_string(), code],
                        cwd: config.root().to_path_buf(),
                        service: None,
                        lang,
                        note: None,
                    }
                }
            },
            WECHAT_ENGINE => {
                let install = crate::wxocr::Install::probe(config.root());
                match install.missing {
                    None => Engine::service(config, install, None),
                    Some(why) => Engine::windows(
                        config,
                        &requested,
                        Some(format!(
                            "{WECHAT_ENGINE} is selected in {ENGINE_KEY}, but this install cannot run it: {why}. \
                             This run uses {WINDOWS_ENGINE} instead"
                        )),
                    ),
                }
            }
            other => match custom_command(config) {
                Some((program, args)) => Engine {
                    name: other.to_string(),
                    requested,
                    program,
                    args,
                    cwd: config.root().to_path_buf(),
                    service: None,
                    lang: engine_language(config, other),
                    note: None,
                },
                None => Engine::windows(
                    config,
                    &requested,
                    Some(format!(
                        "{ENGINE_KEY} is \"{other}\", which this binary cannot run: it is neither a \
                         built-in engine nor named by a {COMMAND_KEY} list. This run uses \
                         {WINDOWS_ENGINE} instead"
                    )),
                ),
            },
        }
    }

    /// The built-in engine, at a path and language the caller already decided on.
    ///
    /// Production code resolves through [`Engine::select`], which is what reads `ocr_engine`. This exists
    /// for a caller that reads its settings once and hands them down as a plain struct — `wind-reindex` —
    /// whose tests must be able to fill that struct on a machine with no install directory in it.
    pub fn builtin_at(program: PathBuf, cwd: PathBuf, lang: &str) -> Engine {
        let lang = lang.to_string();
        Engine {
            name: WINDOWS_ENGINE.to_string(),
            requested: WINDOWS_ENGINE.to_string(),
            program,
            args: vec!["-l".to_string(), lang.clone(), String::new()],
            cwd,
            service: None,
            lang,
            note: None,
        }
    }

    /// The resident-service engine: the same `Engine` a caller selects, dispatches and reports, with the
    /// spawn replaced by one channel to a child this process keeps.
    ///
    /// Public because a probe has to be able to hold the engine it means to test. `windsetup
    /// check-engines` scores every engine on the machine, and requiring the user to have selected WeChat
    /// first would turn the report into a restatement of the config file.
    pub fn service(config: &Config, install: crate::wxocr::Install, note: Option<String>) -> Engine {
        let requested = WECHAT_ENGINE.to_string();
        let lang = engine_language(config, WECHAT_ENGINE);
        Engine {
            name: WECHAT_ENGINE.to_string(),
            requested,
            program: install.exe.clone(),
            args: Vec::new(),
            cwd: config.root().to_path_buf(),
            service: Some(install),
            lang,
            note,
        }
    }

    /// The Windows engine, at the path this install ships, asked in `ocr_lang`.
    fn windows(config: &Config, requested: &str, note: Option<String>) -> Engine {
        let mut engine = Engine::builtin_at(
            windows_exe(config.root()),
            config.root().to_path_buf(),
            &config.str_or("ocr_lang", "zh-Hans-CN"),
        );
        engine.requested = requested.to_string();
        engine.note = note;
        engine
    }

    /// What `ocr_engine` says, whether or not it can be honoured.
    pub fn requested(&self) -> &str {
        &self.requested
    }

    /// The engine actually going to run.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The language this engine is asked in, in the tag *that engine* expects — a Windows BCP-47 tag for
    /// the built-in, a Tesseract code for Tesseract.
    pub fn language(&self) -> &str {
        &self.lang
    }

    /// Why this is not the configured engine. `None` when it is.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Is the program there? A bare name is the loader's to resolve, so it is assumed reachable and a
    /// spawn failure reports the rest; anything with a directory in it is checkable here, and checked.
    pub fn is_installed(&self) -> bool {
        match &self.service {
            Some(install) => install.is_usable(),
            None => resolve_program(&self.program).is_some(),
        }
    }

    /// Is this the resident service rather than a program run over one image?
    pub fn is_service(&self) -> bool {
        self.service.is_some()
    }

    /// The engine's own path, for a report that has to name a file.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The invocation, program first, for the image at `image`. Pure, so the argv is pinned by a test on
    /// a machine with no engines installed.
    pub fn argv(&self, image: &Path) -> Vec<String> {
        if self.service.is_some() {
            // There is no argv. Returning a invented one is how a log line starts describing a command
            // nobody ran; [`describe`] says what actually happens instead.
            return Vec::new();
        }
        let rendered = image.display().to_string();
        let mut out = Vec::with_capacity(self.args.len() + 1);
        out.push(self.program.to_string_lossy().into_owned());
        for arg in &self.args {
            out.push(if arg.is_empty() {
                rendered.clone()
            } else {
                arg.replace("{image}", &rendered).replace("{lang}", &self.lang)
            });
        }
        out
    }

    /// `argv` with the image as a placeholder, so a log can name the engine's shape without inventing a
    /// path for it.
    pub fn describe(&self) -> String {
        if let Some(install) = &self.service {
            return format!("{} (resident service over {}, models in {})", EXE_LABEL, DLL_LABEL, install.dir.display());
        }
        let mut line = self.program.to_string_lossy().into_owned();
        for arg in &self.args {
            line.push(' ');
            line.push_str(if arg.is_empty() || arg.contains("{image}") {
                "<image>"
            } else {
                arg.as_str()
            });
        }
        line
    }

    /// Run the engine over an image file already on disk and return its text.
    ///
    /// `Ok("")` is a result, not an error: a frame with nothing readable in it. Callers drop it with
    /// their own "too short to index" rule.
    ///
    /// stdout goes through [`crate::decode_console_bytes`], because a console child writes the machine's
    /// code page rather than UTF-8 and a lossy decode stores rows that look like plausible garbage.
    pub fn recognize(&self, image: &Path) -> Result<String, EngineError> {
        if let Some(install) = &self.service {
            return crate::wxocr::recognize(install, image, crate::wxocr::TASK_TIMEOUT).map_err(EngineError::Failed);
        }
        let Some(program) = resolve_program(&self.program) else {
            return Err(EngineError::Missing(self.program.clone()));
        };
        let mut argv = self.argv(image);
        argv[0] = program.to_string_lossy().into_owned();
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.cwd)
            .output()
            .map_err(EngineError::Spawn)?;
        if !output.status.success() {
            let mut detail = crate::decode_console_bytes(&output.stderr);
            if detail.trim().is_empty() {
                detail = crate::decode_console_bytes(&output.stdout);
            }
            return Err(EngineError::Failed(format!(
                "exit {:?}: {}",
                output.status.code(),
                detail.trim()
            )));
        }
        Ok(crate::decode_console_bytes(&output.stdout))
    }
}

/// The value of `ocr_engine`, defaulted the way upstream defaults it.
pub fn configured_name(config: &Config) -> String {
    let name = config.str_or(ENGINE_KEY, WINDOWS_ENGINE).trim().to_string();
    if name.is_empty() {
        WINDOWS_ENGINE.to_string()
    } else {
        name
    }
}

/// Where the Windows engine lives. One definition, which [`Config::ocr_exe`] answers through too, so the
/// picker, the recorder's spawn and `windsetup check-engines` cannot name three different files.
pub fn windows_exe(root: &Path) -> PathBuf {
    root.join("ocr_lib").join(WINDOWS_EXE_NAME)
}

/// The path `TesseractOCR_filepath` asks for, including upstream's shipped default.
pub fn configured_path(config: &Config) -> PathBuf {
    PathBuf::from(config.str_or(TESSERACT_PATH_KEY, DEFAULT_TESSERACT))
}

/// Where `tesseract.exe` could be, in the order they should be tried: the configured path first, because
/// that is the file the user edited on purpose; then the well-known installs; then the folder this
/// project's own installer uses; then whatever the loader resolves a bare `tesseract` to.
///
/// De-duplicated case-insensitively, since the shipped default and the first well-known path are the same
/// string on a normal machine — and a report that names one file twice reads as a tool that looked
/// nowhere.
pub fn tesseract_candidates(root: &Path, configured: &str) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from(configured), PathBuf::from("tesseract")];
    for known in [
        DEFAULT_TESSERACT,
        r"C:\Program Files (x86)\Tesseract-OCR\tesseract.exe",
        r"C:\Program Files (x86)\Tesseract\tesseract.exe",
    ] {
        out.push(PathBuf::from(known));
    }
    out.push(root.join("ocr_lib").join("tesseract").join("tesseract.exe"));
    let mut seen: Vec<PathBuf> = Vec::new();
    out.retain(|candidate| {
        let fresh = !seen
            .iter()
            .any(|seen: &PathBuf| seen.as_os_str().eq_ignore_ascii_case(candidate.as_os_str()));
        if fresh {
            seen.push(candidate.clone());
        }
        fresh
    });
    out
}

/// Where `tesseract_candidates` should look for the engine this config points at.
pub fn tesseract_candidates_for(config: &Config) -> Vec<PathBuf> {
    let configured = config.str_or(TESSERACT_PATH_KEY, DEFAULT_TESSERACT);
    tesseract_candidates(config.root(), &configured)
}

/// The Tesseract this install can reach, found without starting a process — the settings page asks on
/// every render, and a probe that spawns `--version` per keystroke is not a settings page.
pub fn tesseract_program(config: &Config) -> Option<PathBuf> {
    tesseract_candidates_for(config).into_iter().find_map(|candidate| resolve_program(&candidate))
}

/// Where a program name actually is. A path is itself, if the file exists; a bare name is looked up in
/// `PATH` for the file the loader would run, which is the same answer without spawning anything.
fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string_lossy().into_owned());
    if name.is_empty() {
        return None;
    }
    which(&name).or_else(|| which(&format!("{name}.exe")))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

/// The tags this project's config and `__assets__/` use are Windows BCP-47 names; Tesseract's are its own.
/// `const.py:OCR_SUPPORT_CONFIG["TesseractOCR"]` held ~100 rows of that table; what is kept here is the
/// set the product's own locales and fixtures name, and an unknown tag is refused rather than guessed at —
/// a wrong code reads to the user as "this engine is bad at Japanese", not as "we asked wrongly".
pub fn tesseract_code(language: &str) -> Option<String> {
    const KNOWN: &[(&str, &str)] = &[
        ("zh-hans-cn", "chi_sim"),
        ("zh-hans", "chi_sim"),
        ("zh-cn", "chi_sim"),
        ("sc", "chi_sim"),
        ("zh-hant", "chi_tra"),
        ("zh-hant-cn", "chi_tra"),
        ("tc", "chi_tra"),
        ("en-us", "eng"),
        ("en", "eng"),
        ("ja-jp", "jpn"),
        ("ja-jp-japan", "jpn"),
        ("ja", "jpn"),
        ("ko-kr", "kor"),
        ("ko", "kor"),
    ];
    let key = language.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }
    if let Some((_, code)) = KNOWN.iter().find(|(tag, _)| *tag == key) {
        return Some((*code).to_string());
    }
    // Already a Tesseract code — `eng`, `chi_sim`, `old_swe` — so there is nothing to translate.
    let shaped = key.len() == 3 || key.contains('_');
    let ascii_only = key.chars().all(|c| c.is_ascii_alphabetic() || c == '_');
    if shaped && ascii_only {
        return Some(key);
    }
    None
}

/// The language an engine is addressed by. Upstream's rule: the built-in Windows engine takes `ocr_lang`,
/// a third-party one takes the first entry of `third_party_engine_ocr_lang`, and a third-party engine with
/// no language list falls back to `ocr_lang` rather than running with none.
pub fn engine_language(config: &Config, name: &str) -> String {
    if name == WINDOWS_ENGINE {
        return config.str_or("ocr_lang", "zh-Hans-CN");
    }
    match config
        .str_list(THIRD_PARTY_LANG_KEY)
        .into_iter()
        .find(|entry| !entry.trim().is_empty())
    {
        Some(entry) => entry.trim().to_string(),
        None => config.str_or("ocr_lang", "zh-Hans-CN"),
    }
}

/// A contributed engine: `ocr_engine_command` as a list of strings, program first, `{image}` and `{lang}`
/// substituted in. A bare string is read as "this program takes `-l <lang> <image>`", which is the shape
/// of every engine this project has ever shipped.
fn custom_command(config: &Config) -> Option<(PathBuf, Vec<String>)> {
    let raw = config.raw(COMMAND_KEY)?;
    let argv: Vec<String> = match raw {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect(),
        Value::String(text) if !text.trim().is_empty() => {
            vec![text.trim().to_string(), "-l".to_string(), "{lang}".to_string(), "{image}".to_string()]
        }
        _ => return None,
    };
    let (program, args) = argv.split_first()?;
    if program.trim().is_empty() {
        return None;
    }
    Some((PathBuf::from(program.trim()), args.to_vec()))
}

/// Every engine the picker may offer: the ones this install can drive, and the ones a migrated config
/// registered but this binary cannot run — listed as unavailable rather than hidden, because a user whose
/// `ocr_engine` says `PaddleOCR` is entitled to know the install refused it instead of finding the box
/// quietly reset to something else.
///
/// The Python-hosted engines appear here only through `support_ocr_lst` — the folder the extension scripts
/// left behind is evidence of a registration, not of a capability.
pub fn choices(config: &Config) -> Vec<Choice> {
    let root = config.root();
    let windows = windows_exe(root);
    let mut out = vec![Choice {
        name: WINDOWS_ENGINE.to_string(),
        available: windows.is_file(),
        detail: match windows.is_file() {
            true => windows.display().to_string(),
            false => format!("{} is not in this install", windows.display()),
        },
    }];

    match tesseract_program(config) {
        Some(program) => out.push(Choice {
            name: TESSERACT_ENGINE.to_string(),
            available: true,
            detail: program.display().to_string(),
        }),
        None => out.push(Choice {
            name: TESSERACT_ENGINE.to_string(),
            available: false,
            detail: format!("no tesseract at {} or on PATH", configured_path(config).display()),
        }),
    };

    // WeChat's engine is a built-in door now, and it is offered or refused by the same rule the others
    // use: the files are there, or the sentence says which one is not.
    let wechat = crate::wxocr::Install::probe(root);
    out.push(Choice {
        name: WECHAT_ENGINE.to_string(),
        available: wechat.is_usable(),
        detail: match &wechat.missing {
            None => wechat.dir.display().to_string(),
            Some(why) => why.clone(),
        },
    });

    // Everything the config registers on top of the built-ins, plus what it currently selects, so no
    // name a user (or an upgrade) put there can vanish from the list.
    let mut registered = config.str_list(SUPPORT_LIST_KEY);
    registered.push(configured_name(config));
    registered.sort();
    registered.dedup();
    for name in registered {
        if name.is_empty() || name == WINDOWS_ENGINE || name == TESSERACT_ENGINE || name == WECHAT_ENGINE {
            continue;
        }
        let command = custom_command(config);
        let reachable = matches!(&command, Some((program, _)) if resolve_program(program).is_some());
        out.push(Choice {
            name,
            available: reachable,
            detail: match &command {
                Some((program, _)) if reachable => program.display().to_string(),
                Some((program, _)) => format!("{} is not reachable", program.display()),
                None => format!(
                    "registered by an extension install script, but its recognition ran inside Python; \
                     give it a {COMMAND_KEY} argv to make it drivable from here"
                ),
            },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A config read from a scratch install directory, so every test decides what is on disk. The name
    /// carries the tag and the process id because these run in parallel.
    struct Install {
        dir: PathBuf,
        config: Config,
    }

    impl Drop for Install {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn install(tag: &str, body: &str) -> Install {
        let dir = std::env::temp_dir().join(format!("windcap-ocr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), body).unwrap();
        let config = Config::load(&dir).unwrap();
        Install { dir, config }
    }

    /// A tesseract that exists, in this scratch install, so the path-shaped candidate is reachable
    /// without touching the machine's PATH.
    fn fake_tesseract(install: &mut Install) -> PathBuf {
        let program = install.dir.join("tesseract.exe");
        std::fs::write(&program, b"not really").unwrap();
        install.config.set(TESSERACT_PATH_KEY, Value::String(program.display().to_string()));
        program
    }

    #[test]
    fn the_default_selects_the_engine_the_shipped_config_names() {
        let install = install("default", r#"{"ocr_engine": "Windows.Media.Ocr.Cli", "ocr_lang": "zh-Hans-CN"}"#);
        let engine = Engine::select(&install.config);
        assert_eq!(engine.name(), WINDOWS_ENGINE);
        assert!(engine.note().is_none(), "{:?}", engine.note());
        assert_eq!(
            engine.argv(Path::new("C:/cache/8.jpg")),
            vec![
                windows_exe(install.config.root()).to_string_lossy().into_owned(),
                "-l".to_string(),
                "zh-Hans-CN".to_string(),
                "C:/cache/8.jpg".to_string()
            ]
        );
        assert!(engine.describe().ends_with("Windows.Media.Ocr.Cli.exe -l zh-Hans-CN <image>"), "{}", engine.describe());
    }

    /// Tesseract wants `<image> - -l <code>`: the `-` output base is what stops the engine writing a
    /// `.txt` into the user's screenshot cache.
    #[test]
    fn tesseract_is_invoked_with_the_image_first_and_a_mapped_language() {
        let mut install = install("tess", r#"{"ocr_engine": "TesseractOCR", "ocr_lang": "ja-jp"}"#);
        let program = fake_tesseract(&mut install);
        let engine = Engine::select(&install.config);
        assert_eq!(engine.name(), TESSERACT_ENGINE, "{:?}", engine.note());
        assert_eq!(
            engine.argv(Path::new("i.jpg")),
            vec![program.display().to_string(), "i.jpg".to_string(), "-".to_string(), "-l".to_string(), "jpn".to_string()]
        );
        assert_eq!(engine.describe(), format!("{} <image> - -l jpn", program.display()));
        assert_eq!(engine.language(), "ja-jp");
    }

    /// A third-party engine is addressed by upstream's own language key, not by `ocr_lang`.
    #[test]
    fn a_third_party_engine_is_asked_in_third_party_engine_ocr_lang() {
        let install = install(
            "lang",
            r#"{"ocr_engine": "TesseractOCR", "ocr_lang": "zh-Hans-CN", "third_party_engine_ocr_lang": ["eng", "chi_sim"]}"#,
        );
        assert_eq!(engine_language(&install.config, TESSERACT_ENGINE), "eng");
        assert_eq!(engine_language(&install.config, WINDOWS_ENGINE), "zh-Hans-CN");
    }

    /// Selecting an engine this binary cannot run must not stop the recorder — and must not be quiet
    /// about it either, because the note is the whole explanation of the text that gets indexed.
    #[test]
    fn an_engine_that_cannot_be_driven_degrades_to_the_builtin_and_says_so() {
        let install = install("degrade", r#"{"ocr_engine": "PaddleOCR", "ocr_lang": "zh-Hans-CN"}"#);
        let engine = Engine::select(&install.config);
        assert_eq!(engine.name(), WINDOWS_ENGINE);
        assert_eq!(engine.requested(), "PaddleOCR");
        let note = engine.note().expect("the substitution has to be reported");
        assert!(note.contains("PaddleOCR"), "{note}");
        assert!(note.contains(COMMAND_KEY), "{note}");
    }

    /// A configured engine whose language has no code is a wrong question, not an engine failure — so it
    /// is refused rather than answered with a guess.
    #[test]
    fn an_untranslatable_language_refuses_to_guess_a_tesseract_code() {
        let mut install = install("badlang", r#"{"ocr_engine": "TesseractOCR", "third_party_engine_ocr_lang": ["klingon"]}"#);
        fake_tesseract(&mut install);
        let engine = Engine::select(&install.config);
        assert_eq!(engine.name(), WINDOWS_ENGINE);
        assert!(engine.note().unwrap().contains("klingon"), "{:?}", engine.note());
    }

    /// The native equivalent of upstream's "add a branch to `ocr_image`": any program that takes an image
    /// path and prints text.
    #[test]
    fn a_contributed_engine_is_an_argv_template() {
        let install = install(
            "contrib",
            r#"{"ocr_engine": "MyOCR", "ocr_engine_command": ["C:/tools/myocr.exe", "--input", "{image}", "--lang", "{lang}"], "third_party_engine_ocr_lang": ["eng"]}"#,
        );
        let engine = Engine::select(&install.config);
        assert_eq!(engine.name(), "MyOCR", "{:?}", engine.note());
        assert_eq!(
            engine.argv(Path::new("C:/cache/8.jpg")),
            vec!["C:/tools/myocr.exe", "--input", "C:/cache/8.jpg", "--lang", "eng"]
        );
    }

    #[test]
    fn a_bare_command_string_is_read_as_the_shape_every_shipped_engine_uses() {
        let install = install("bare", r#"{"ocr_engine": "MyOCR", "ocr_engine_command": "C:/tools/myocr.exe", "ocr_lang": "en-US"}"#);
        let engine = Engine::select(&install.config);
        assert_eq!(engine.argv(Path::new("i.jpg")), vec!["C:/tools/myocr.exe", "-l", "en-US", "i.jpg"]);
    }

    /// An engine registered by the old install scripts stays in the list, marked unavailable: a picker
    /// that silently drops it would look like the install had forgotten the user's own choice.
    /// The engines whose recognition really did live in Python are still listed and still refused, and the
    /// sentence says what would make them drivable. `WeChatOCR` left this bucket when `wxocr` landed;
    /// `PaddleOCR` did not, and pretending otherwise would put a dead row back on the page.
    #[test]
    fn a_python_hosted_engine_is_listed_and_not_offered_as_runnable() {
        let install = install(
            "hosted",
            r#"{"ocr_engine": "PaddleOCR", "support_ocr_lst": ["Windows.Media.Ocr.Cli", "PaddleOCR"]}"#,
        );
        let rows = choices(&install.config);
        let paddle = rows.iter().find(|row| row.name == "PaddleOCR").expect("registered, so listed");
        assert!(!paddle.available, "{paddle:?}");
        assert!(paddle.detail.contains(COMMAND_KEY), "{paddle:?}");
        assert!(rows.iter().any(|row| row.name == WINDOWS_ENGINE), "{rows:?}");
        // Selecting it cannot strand the install: the recorder runs the built-in and says so.
        let engine = Engine::select(&install.config);
        assert_eq!(engine.name(), WINDOWS_ENGINE);
        assert!(engine.note().unwrap().contains("PaddleOCR"), "{:?}", engine.note());
    }

    /// WeChat's engine is offered by the files on disk, in one row, and refused with the name of whichever
    /// piece is missing — never by hiding the row, which is what left a user wondering where the feature
    /// upstream had was.
    #[test]
    fn wechat_ocr_is_offered_by_what_is_on_disk_and_refused_by_what_is_not() {
        let install = install("wechat", r#"{"ocr_engine": "WeChatOCR"}"#);
        let rows = choices(&install.config);
        let wechat = rows.iter().filter(|row| row.name == WECHAT_ENGINE).collect::<Vec<_>>();
        assert_eq!(wechat.len(), 1, "exactly one row, wherever it comes from: {rows:?}");
        assert!(!wechat[0].available, "{wechat:?}");
        assert!(wechat[0].detail.contains("wxocr-binary"), "{wechat:?}");

        let engine = Engine::select(&install.config);
        assert_eq!(engine.name(), WINDOWS_ENGINE, "an install with no binaries cannot run the service");
        assert!(engine.note().unwrap().contains(WECHAT_ENGINE), "{:?}", engine.note());

        let binary = install.dir.join(crate::wxocr::BINARY_DIR);
        std::fs::create_dir_all(binary.join("Model")).unwrap();
        std::fs::write(binary.join("WeChatOCR.exe"), b"MZ").unwrap();
        std::fs::write(binary.join("mmmojo_64.dll"), b"MZ").unwrap();
        let rows = choices(&install.config);
        let wechat = rows.iter().find(|row| row.name == WECHAT_ENGINE).expect("listed");
        assert!(wechat.available, "{wechat:?}");

        let engine = Engine::select(&install.config);
        assert_eq!(engine.name(), WECHAT_ENGINE, "with the files present, the selection is honoured");
        assert!(engine.note().is_none(), "{:?}", engine.note());
        assert!(engine.is_installed());
        assert!(engine.is_service(), "it is a child process, not a command line");
        assert!(engine.argv(Path::new("frame.jpg")).is_empty(), "an engine with no argv says so instead of inventing one");
        assert!(engine.describe().contains("resident service"), "{}", engine.describe());
        // And the refusal is still a refusal when the picture is asked for: no silent empty row.
        let _ = std::fs::remove_dir_all(binary.join("Model"));
        assert!(engine.recognize(Path::new("frame.jpg")).is_err(), "a service whose models vanished must say so");
    }

    #[test]
    fn the_language_table_covers_the_languages_the_product_ships_and_passes_codes_through() {
        assert_eq!(tesseract_code("zh-Hans-CN").as_deref(), Some("chi_sim"));
        assert_eq!(tesseract_code("en-US").as_deref(), Some("eng"));
        assert_eq!(tesseract_code("ja-jp").as_deref(), Some("jpn"));
        assert_eq!(tesseract_code("chi_tra").as_deref(), Some("chi_tra"));
        assert_eq!(tesseract_code("zh-Hans-CN-jp-jp-jp"), None);
        assert_eq!(tesseract_code(""), None);
    }

    /// The candidate list is shared with `windsetup check-engines`, so the two cannot disagree about where
    /// Tesseract was looked for.
    #[test]
    fn the_candidates_keep_the_configured_path_first_and_do_not_repeat_it() {
        let candidates = tesseract_candidates(Path::new("D:/Windrecorder"), DEFAULT_TESSERACT);
        assert_eq!(candidates[0], PathBuf::from(DEFAULT_TESSERACT));
        assert_eq!(
            candidates.iter().filter(|c| c.as_path() == Path::new(DEFAULT_TESSERACT)).count(),
            1,
            "{candidates:?}"
        );
        assert!(candidates.contains(&PathBuf::from("tesseract")), "{candidates:?}");
        assert!(candidates.contains(&PathBuf::from("D:/Windrecorder/ocr_lib/tesseract/tesseract.exe")), "{candidates:?}");
    }

    /// A missing engine must be an error the caller can report, not an `os error 2` from inside a frame
    /// loop — and it must be reached without spawning anything.
    #[test]
    fn a_missing_engine_is_reported_before_it_is_started() {
        let install = install("missing", r#"{"ocr_engine": "MyOCR", "ocr_engine_command": "Z:/definitely-not-here/myocr.exe"}"#);
        let engine = Engine::select(&install.config);
        assert!(!engine.is_installed(), "{:?}", engine.program());
        let err = engine.recognize(Path::new("x.jpg")).expect_err("must fail");
        assert!(matches!(err, EngineError::Missing(_)), "{err}");
        assert!(err.to_string().contains("definitely-not-here"), "{err}");
    }

    /// An empty `ocr_engine` is not an engine: it resolves to the default rather than to a program named
    /// `""`, which is how a hand-edited config becomes an `os error 2`.
    #[test]
    fn a_blank_engine_name_falls_back_to_the_default_instead_of_an_empty_program() {
        let install = install("blank", r#"{"ocr_engine": "   "}"#);
        assert_eq!(configured_name(&install.config), WINDOWS_ENGINE);
        assert_eq!(Engine::select(&install.config).name(), WINDOWS_ENGINE);
    }

    /// The one thing this module must never lose: the shipped defaults resolve to the engine the shipped
    /// `ocr_lib/` holds, in the repo layout as well as in a payload.
    #[test]
    fn the_repository_install_selects_the_engine_it_ships() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap();
        let config = Config::load(&root).expect("the shipped config must parse");
        let engine = Engine::select(&config);
        assert_eq!(engine.name(), WINDOWS_ENGINE, "{:?}", engine.note());
        assert_eq!(engine.program(), windows_exe(&root), "the path the config and the picker both name");
    }
}
