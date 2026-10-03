//! `Windrecorder.exe` — the file in this payload that carries the product's name, and the one a
//! stranger double-clicks.
//!
//! ## Why a twelfth executable is the answer to eleven
//!
//! `bin\` holds the tray, the recorder, two windows, the bridge, the maintenance pass, the reindexer,
//! the migration tool, the notes store, the AI command line and the terminal query tool, and until
//! this crate existed not one of those files is called Windrecorder. Every one of them says
//! `ProductName = Windrecorder` in its version resource, which is a fact you read *after* picking a
//! file. So the delivery promise that `release.ps1` and both READMEs make — unzip, double-click, done —
//! has always quietly required reading a document to learn that the word for "double-click" is
//! `windsvc.exe`. A launcher named after the product removes the lookup. That is the entire argument
//! for this file, and it is a naming argument, not a process-model one.
//!
//! ## What it does, and what it deliberately does not
//!
//! It finds `windsvc.exe` and starts it, handing argv over untouched. It takes no lock, draws no menu,
//! lays out no directory, opens no config, and knows nothing about recording. Every one of those
//! stays the tray's, for the reason `supervisor/src/main.rs` gives for itself: a component that starts
//! doing another binary's job is a second implementation of a product that already has one. What is
//! added here is a name, an icon, and one `CreateProcess`.
//!
//! Concretely, three answers to argv and nothing else:
//!   * **no arguments** — start the tray and exit. Not wait: the tray is the long-lived process, and a
//!     launcher that sits in front of it adds a second handle for the same icon and a second thing for
//!     Task Manager to be confused by. Clicking twice is the tray's own single-instance check to answer,
//!     and its balloon (`layout::ALREADY_RUNNING_CAPTION`) is the feedback.
//!   * **`--version` / `-V`** — answer about *this* file, in `wind_base::version`'s shared format. The
//!     twelve files come out of one build and one zip, so the number is the same one `windsvc
//!     --version` prints; what differs is that this is the line a person quotes when they clicked an
//!     icon and something did not happen.
//!   * **anything else** — attach the launching console, run the tray with those arguments, and exit
//!     with its code. `Windrecorder.exe doctor --root .` is therefore `windsvc.exe doctor --root .`,
//!     byte for byte in the report, and there is still exactly one thing that knows what a healthy
//!     install looks like.
//!
//! Autostart still registers `bin\windsvc.exe` and not this file. `base/src/autostart.rs::target_exe`
//! names the staged tray outright, and the right thing to name is the process that owns the lock and the
//! icon: a Run entry pointing at a launcher that exits a second after starting the tray would put a
//! second tray through the single-instance check at every logon to announce nothing. The same reasoning
//! is why `supervisor/src/native.rs::BINARIES` does not list `Windrecorder` either — `doctor` reports on
//! the binaries that *do work*, and this one does none.
//!
//! ## Shape of the crate
//!
//! `plan` turns argv into one of the three answers; `candidates` is where the tray could be, in the
//! order `maint/src/schedule.rs` uses for its own scheduled children. Both are pure, because `cargo
//! test` does not build binaries — the same constraint every other crate in this workspace writes
//! down — and the decision is what is worth asserting. Whether the tray actually starts is proven once,
//! on a real payload, by `smoke.ps1`. `ffi` is every Win32 declaration in one place, which is two calls.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod ffi;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What `--version` prints, what the version resource is generated for, and the file name without its
/// extension. Deliberately not `windsvc`'s name: the point of this crate is that the string a person
/// typed and the string that answers are the same word.
pub const NAME: &str = "Windrecorder";

/// The tray, sought by name. Never `current_exe` and never "myself": this file and the tray are two
/// processes, and the tray is the one that owns the lock, the menu, the recorder and the report.
pub const TRAY: &str = "windsvc";

/// The override every binary's search honours, spelled the same way as `supervisor/src/native.rs`.
pub const ENV_HOME: &str = "WINDCAP_HOME";

/// The profiles a cargo tree can hold a binary in, best first, so a development checkout cannot run a
/// `debug` artefact behind a `release` one that is sitting in the same tree.
pub const PROFILES: [&str; 2] = ["release", "debug"];

/// The one thing argv can ask for.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Answer about this file and stop.
    Version,
    /// Start the tray, do not wait for it.
    Start,
    /// Run the tray with these arguments, wait, and leave on its code.
    Forward(Vec<String>),
}

/// What argv means to the launcher.
///
/// `--version` is only a version request on its own. `Windrecorder.exe --version --root .` is not a
/// thing anybody means, and reading it as one would silently drop the `--root` they typed as thought it
/// was a request for a different answer; forwarding it lets the tray produce the usage error, which is
/// the one usage error in this product.
pub fn plan(argv: &[String]) -> Plan {
    match argv {
        [] => Plan::Start,
        [only] if wind_base::version::is_flag(only) => Plan::Version,
        arguments => Plan::Forward(arguments.to_vec()),
    }
}

/// Every path the tray could be at, most installed first, in the order they are tried.
///
/// The directory beside this one comes first, which is the payload's own layout: `bin\Windrecorder.exe`
/// and `bin\windsvc.exe` are staged into the same folder by the same script. After that the shared
/// `WINDCAP_HOME`, the install's `bin\`, the install root, and a cargo tree's `release` then `debug` —
/// the same list `maint/src/schedule.rs::candidate_dirs` builds for the children it runs, for the same
/// reason: a scheduled child and a launched tray are both "a binary I did not compile myself and must
/// therefore locate by rule, not by argument".
///
/// Pure, and returning paths rather than testing them, so a test can assert the *order* — which is the
/// part that decides whether a development checkout picks up a stale build.
pub fn candidates(exe_dir: &Path, root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![exe_dir.to_path_buf()];
    if let Some(home) = std::env::var_os(ENV_HOME) {
        if !home.is_empty() {
            dirs.push(PathBuf::from(home));
        }
    }
    dirs.push(root.join("bin"));
    dirs.push(root.to_path_buf());
    for profile in PROFILES {
        dirs.push(root.join("windcap").join("target").join(profile));
    }
    let file = format!("{TRAY}.exe");
    // Deduplicated because the launcher normally lives *in* `bin\`, so its own directory and
    // `<root>\bin` are the same path — and the one place this list is shown to a human is the
    // "Looked in:" paragraph of [`missing_message`], where the same directory twice reads as a bug in
    // the report rather than as a no-op.
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let candidate = dir.join(&file);
        if !seen.contains(&candidate) {
            seen.push(candidate);
        }
    }
    seen
}

/// The install root, by the rule every other binary uses: walk up from the running executable until a
/// directory carrying `config_src` or `userdata` answers. No `--root` is passed to the tray, because
/// the tray sits in the same folder as this file and resolves the same walk to the same answer — and a
/// launcher that guessed a root would be the one place in the product where the root came from a
/// different process's opinion.
fn install_root() -> PathBuf {
    wind_base::install::resolve_root_from_exe(None)
}

/// The tray's path, or the whole list of places it was looked for, plus whether an install root was
/// found at all.
///
/// Returning the candidates alongside the failure is what makes [`missing_message`] name directories
/// rather than complain vaguely: an install missing one file and an install with the file in a place
/// this binary cannot see are different repairs, and the reader has to be able to tell them apart.
///
/// The second half of the error is measured, not theoretical. Run against a folder that is nothing but
/// `bin\Windrecorder.exe`, `resolve_root_from_exe` falls back to the directory it started in — which is
/// `install.rs`'s documented answer to a root it cannot find — and the list then reads
/// `bin\bin\windsvc.exe` and `bin\windcap\target\release\windsvc.exe`. Correct, and unreadable without
/// the sentence saying that no root was found.
fn find_tray() -> Result<PathBuf, (Vec<PathBuf>, bool)> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let Some(exe_dir) = exe_dir else {
        // No path to myself: a process started with an unopenable image, which is not a state this
        // binary can be in and still be running. Treated as "nothing was found" rather than as a
        // panic, because the message below is still the useful thing to say.
        return Err((Vec::new(), false));
    };
    let root = install_root();
    let looked = candidates(&exe_dir, &root);
    match looked.iter().find(|candidate| candidate.is_file()) {
        Some(found) => Ok(found.clone()),
        None => Err((looked, wind_base::install::is_install_root(&root))),
    }
}

/// What to say when there is no tray to start.
pub fn missing_message(looked: &[PathBuf], root_found: bool) -> String {
    let mut text = format!("{TRAY}.exe was not found, and nothing else in this install can start the recorder for you.");
    if looked.is_empty() {
        text.push_str("\n\nThis launcher could not work out where it was run from.");
    } else {
        text.push_str("\n\nLooked in:");
        for path in looked {
            text.push_str(&format!("\n  {}", path.display()));
        }
        if !root_found {
            text.push_str("\n\nNone of the folders above this file looks like an install (no `config_src`, no `userdata`), so the search started here rather than at an install root.");
        }
    }
    text.push_str("\n\nRe-extract the payload over this folder, or run `windcap\\build.ps1 -Stage` in a cloned tree. `windsetup.exe init` and `windrec.exe loop` still work by hand, but the tray is what makes them start themselves.");
    text
}

/// Start the tray and leave. The exit code is this process's verdict on *starting* it, not the tray's
/// eventual one — which is the whole reason the two paths differ: nobody is watching a double-click.
fn start() -> i32 {
    let program = match find_tray() {
        Ok(program) => program,
        Err((looked, root_found)) => {
            ffi::error_box(NAME, &missing_message(&looked, root_found));
            return 1;
        }
    };
    match Command::new(&program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => 0,
        Err(error) => {
            ffi::error_box(NAME, &format!("{} could not be started: {error}", program.display()));
            1
        }
    }
}

/// Run the tray with argv and become its exit code.
///
/// Stdio is inherited rather than piped: the tray writes its report to whatever handles it was given,
/// and after [`ffi::attach_parent_console`] those handles are the terminal the command was typed into.
fn forward(arguments: &[String]) -> i32 {
    let program = match find_tray() {
        Ok(program) => program,
        Err((looked, root_found)) => {
            eprintln!("{}: {}", NAME, missing_message(&looked, root_found));
            return 1;
        }
    };
    match Command::new(&program).args(arguments).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("{}: {} could not be started: {error}", NAME, program.display());
            1
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match plan(&argv) {
        // Not console-attached, for the reason `supervisor/src/main.rs` measures for its own version
        // arm: attaching replaces the inherited handles with the console's, and `windsvc --version >
        // build.txt` was found there to write an empty file in cmd, PowerShell and MSYS alike. This
        // binary keeps the same choice, and `smoke.ps1` reads this line out of a redirected file, which
        // is where a claim about redirection gets checked rather than asserted. One short line aimed at
        // a support ticket is worth more arriving intact than arriving in a console that may not be
        // there.
        Plan::Version => {
            println!("{}", wind_base::version::line(NAME, env!("CARGO_PKG_VERSION")));
        }
        Plan::Start => std::process::exit(start()),
        Plan::Forward(arguments) => {
            ffi::attach_parent_console();
            std::process::exit(forward(&arguments));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &[&str]) -> Vec<String> {
        text.iter().map(|word| word.to_string()).collect()
    }

    /// The double-click is the delivery promise, so it is the arm with no argument and no name in it:
    /// a launcher that needed `run` typed would be a command somebody has to remember.
    #[test]
    fn no_arguments_starts_the_tray() {
        assert_eq!(plan(&args(&[])), Plan::Start);
    }

    #[test]
    fn a_lone_version_flag_is_answered_here_and_not_forwarded() {
        assert_eq!(plan(&args(&["--version"])), Plan::Version);
        assert_eq!(plan(&args(&["-V"])), Plan::Version);
    }

    /// `-v` is left to the tray on purpose: `wind_base::version::is_flag` is case-sensitive for the
    /// reason it documents, which is that a flag-shaped positional argument belongs to the tool that
    /// would receive it, not to the guess made about it.
    #[test]
    fn a_lowercase_v_is_not_a_version_request() {
        assert_eq!(plan(&args(&["-v"])), Plan::Forward(args(&["-v"])));
    }

    /// Arguments are forwarded as one block and verbatim. A launcher that rewrote, reordered or dropped
    /// one would leave `Windrecorder.exe doctor` and `windsvc.exe doctor` able to disagree, which is
    /// the failure this file's whole existence is trying not to add.
    #[test]
    fn everything_else_is_handed_over_unchanged() {
        let typed = args(&["doctor", "--root", "E:\\Windrecorder"]);
        assert_eq!(plan(&typed), Plan::Forward(typed.clone()));
        let spaced = args(&["--root", "C:\\Program Files\\Windrecorder"]);
        assert_eq!(plan(&spaced), Plan::Forward(spaced));
    }

    /// A `--version` that arrives with company goes to the tray, which answers with the usage error.
    #[test]
    fn a_version_flag_with_an_argument_is_not_a_version_request() {
        assert_eq!(plan(&args(&["--version", "--root", "."])), Plan::Forward(args(&["--version", "--root", "."])));
    }

    /// The whole order, asserted as one list. `WINDCAP_HOME` is cleared first because it is a slot in
    /// the middle of that list and a machine that happens to have it set would otherwise make this test
    /// about somebody's environment; the override itself is `supervisor/src/native.rs`'s behaviour, not
    /// this crate's, and it is asserted there. Four entries from two arguments is the deduplication:
    /// `E:/install/bin` is both the launcher's own directory and `<root>\bin`, and it is one place.
    #[test]
    fn the_tray_is_sought_beside_this_file_first() {
        std::env::remove_var(ENV_HOME);
        let dirs = candidates(Path::new("E:/install/bin"), Path::new("E:/install"));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("E:/install/bin/windsvc.exe"),
                PathBuf::from("E:/install/windsvc.exe"),
                PathBuf::from("E:/install/windcap/target/release/windsvc.exe"),
                PathBuf::from("E:/install/windcap/target/debug/windsvc.exe"),
            ],
            "the payload stages both files into bin\\, so the neighbour wins, and release is tried \
             before debug so a development tree cannot run a stale debug artefact silently"
        );
    }

    #[test]
    fn every_candidate_is_the_tray_by_name_never_this_file() {
        for path in candidates(Path::new("/x"), Path::new("/y")) {
            assert_eq!(path.file_name().map(|name| name.to_string_lossy().into_owned()), Some("windsvc.exe".to_string()), "{path:?}");
        }
    }

    /// The message has to name the file and the places, because "no tray" and "a tray we cannot see"
    /// are different repairs. This repo's own rule for a missing component is that it is named, never
    /// worked around silently.
    #[test]
    fn a_missing_tray_names_the_file_and_every_place_it_looked() {
        let looked = candidates(Path::new("/install/bin"), Path::new("/install"));
        let message = missing_message(&looked, true);
        assert!(message.contains("windsvc.exe"), "{message}");
        // Asserted through `display()` rather than a hand-written path: `join` puts the platform
        // separator between an absolute-in-one-style argument and the file name, and a test that
        // guessed at the mixture would fail for a reason nobody in support would recognise.
        for path in &looked {
            assert!(message.contains(&path.display().to_string()), "{path:?} is missing from:\n{message}");
        }
        assert!(message.contains("build.ps1"), "and it says what to do about it: {message}");
        assert!(!message.contains("None of the folders above"), "a real install root needs no explanation of itself:\n{message}");
        let orphan = missing_message(&looked, false);
        assert!(orphan.contains("None of the folders above this file"), "{orphan}");
        assert!(missing_message(&[], true).contains("could not work out where it was run from"), "an empty list is its own answer, not a blank paragraph");
    }

    #[test]
    fn the_version_line_names_the_file_that_was_clicked() {
        let line = wind_base::version::line(NAME, env!("CARGO_PKG_VERSION"));
        assert!(line.starts_with("Windrecorder "), "{line}");
        assert!(line.ends_with(" (debug)") || line.ends_with(" (release)"), "{line}");
        assert!(!line.contains("windsvc"), "this line is about the icon that was clicked: {line}");
    }

    /// `icons\1-app.ico` is the product's own icon copied, not re-encoded, which is the same rule
    /// `supervisor/icons` follows and the same one it has to keep honest: a second copy of a picture is
    /// a second truth. Checked against `__assets__/icon-tray.ico` whenever this tree has one — a payload
    /// unpacked from the zip carries `bin\`, `config_src\` and no `__assets__`, and this file's own
    /// bytes are already in the `.exe` by then.
    #[test]
    fn the_file_icon_is_the_products_own_icon_byte_for_byte() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let shipped = crate_dir.join("icons").join("1-app.ico");
        let bytes = std::fs::read(&shipped).expect("the crate declares its icon by shipping icons/1-app.ico");
        assert_eq!(&bytes[0..4], &[0, 0, 1, 0], "an ICO header, not a PNG pretending to be one");
        let art = crate_dir.parent().and_then(Path::parent).map(|root| root.join("__assets__").join("icon-tray.ico"));
        let Some(art) = art.filter(|path| path.is_file()) else {
            eprintln!("no __assets__ above {}; the copy is the source of truth here", crate_dir.display());
            return;
        };
        assert_eq!(bytes, std::fs::read(&art).unwrap_or_else(|error| panic!("{}: {error}", art.display())), "{art:?} and {shipped:?} have drifted apart");
    }
}
