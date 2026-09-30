//! Command line. Same shape as `windrec`'s and `windui`'s: a subcommand, then `--flag=value` or
//! `--flag value`, and a missing value is a message rather than a panic.

use std::path::{Path, PathBuf};

use wind_base::version;

/// What `main` should do with the parsed arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// The tray loop. Needs a desktop and takes over the thread it runs on.
    Run(Options),
    /// Read-only report. The command a user reaches for when the tray "isn't working".
    Doctor(Options),
    /// `--version`/`-V`: name, package version and build profile, and nothing else. Before any
    /// other arm because it must answer on an install whose config is broken.
    Version,
    /// No subcommand at all, or one this binary does not have.
    Usage(Unknown),
    /// A subcommand was there but its arguments were not usable.
    Bad(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Unknown {
    /// No subcommand at all: the user typed `windsvc`.
    Empty,
    /// A word this binary does not have.
    Command(String),
}

impl Unknown {
    /// The offending word, for the usage line that names it.
    pub fn command(&self) -> Option<&str> {
        match self {
            Unknown::Empty => None,
            Unknown::Command(name) => Some(name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub root: PathBuf,
}

/// The install root made absolute, without touching the filesystem.
///
/// Two things need this. A spawned recorder is given `--root <root>` *and* has that same directory as
/// its working directory, so a relative root would be resolved twice and point one level too high —
/// the recorder would read a different install than the tray, or none at all. And `doctor`'s paths have
/// to be ones a user can open. `std::path::absolute` rather than `canonicalize` because the root need
/// not exist yet (that is what `doctor` is for) and because it does not add the `\\?\` volume prefix
/// that `GdipCreateBitmapFromFile` refuses to open.
pub fn absolute(root: &Path) -> PathBuf {
    std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf())
}

/// The install directory: `--root`, or the directory carrying this install's shipped settings,
/// found by walking up from the executable.
///
/// A development build runs out of `windcap/target/debug` and a shipped one out of `bin\`, so
/// "the folder holding me" is never the answer and the tray has to walk up to find the root. It
/// asks [`wind_base::install`] to do that walking, because the rule is one fact shared by all eleven
/// binaries and this path is what the tray hands `windrec` and `winduiweb` as their `--root`.
///
/// The walk this function replaced was not merely a slightly wrong default. It returned the
/// executable's own directory when it found no `windrecorder/` above it, and every consumer of the
/// answer then checked it against the shared rule — so on a standalone payload `windsvc doctor`
/// answered *"…\bin is not a Windrecorder install — it carries no config_default.json"* and the tray
/// refused to boot at all, on an install that is otherwise complete. Measured, not reasoned about.
pub fn default_root() -> PathBuf {
    wind_base::install::resolve_root_from_exe(None)
}

pub fn classify(argv: &[String]) -> Invocation {
    // No arguments at all is the double-click, and the double-click means `run`. Windows hands a
    // program launched from Explorer an argv with nothing in it, so any other reading made the shipped
    // `bin\windsvc.exe` print a usage block into a console it does not have and exit — the gap
    // `release.ps1` described as "starting the tray is still a command somebody types" went one level
    // deeper than the missing `windsetup init`. This is not a binary guessing between two jobs: `run`
    // is the only thing a desktop start can mean, `doctor` is a report a person asks for by name, and
    // `parse(&[])` is the same default-root walk-up every other binary in the payload does — so this
    // is exactly the `windsvc run` the instructions used to require, with the words left off.
    let Some(command) = argv.first() else {
        return match parse(&[]) {
            Ok(options) => Invocation::Run(options),
            Err(message) => Invocation::Bad(message),
        };
    };
    // Answered ahead of the subcommand match, and ahead of `parse`: the one question a user with an
    // unreadable `userdata/config_user.json` must always be able to ask is which binary they have.
    if version::is_flag(command) {
        return Invocation::Version;
    }
    let rest = &argv[1..];
    match command.as_str() {
        "run" => match parse(rest) {
            Ok(options) => Invocation::Run(options),
            Err(message) => Invocation::Bad(message),
        },
        "doctor" => match parse(rest) {
            Ok(options) => Invocation::Doctor(options),
            Err(message) => Invocation::Bad(message),
        },
        "help" | "--help" | "-h" => Invocation::Usage(Unknown::Empty),
        other => Invocation::Usage(Unknown::Command(other.to_string())),
    }
}

/// The line `--version` prints. `env!` expands in this crate, so the number is windsvc's own.
pub fn version_line() -> String {
    version::line("windsvc", env!("CARGO_PKG_VERSION"))
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut root = None;
    let mut index = 0;
    while index < args.len() {
        let (key, inline) = match args[index].split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (args[index].as_str(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            if let Some(found) = inline.clone() {
                return Ok(found);
            }
            index += 1;
            args.get(index).cloned().ok_or_else(|| format!("{what} needs a value"))
        };
        match key {
            "--root" => root = Some(PathBuf::from(value("--root")?)),
            other => return Err(format!("unexpected argument '{other}'")),
        }
        index += 1;
    }
    Ok(Options { root: root.unwrap_or_else(default_root) })
}

pub fn usage(command: Option<&str>) -> String {
    let header = match command {
        Some(name) => format!("unknown command '{name}'\n"),
        None => String::new(),
    };
    format!(
        "{header}\
         usage:\n\
         \x20 windsvc run     [--root PATH]\n\
         \x20                   take the tray lock, show the notification-area icon and supervise\n\
         \x20                   windrec/winduiweb until Exit is chosen\n\
         \x20 windsvc doctor  [--root PATH]\n\
         \x20                   report which binaries were found, the state of every lock, where the\n\
         \x20                   logs go, and what each menu item would do right now\n\
         \x20 windsvc --version\n\
         \x20                   print this binary's name, package version and build profile, and\n\
         \x20                   read nothing at all -- it answers on a broken install too\n\
         \n\
         --root defaults to the install directory: the folder carrying config_src/ (or, on an\n\
         install that has not moved its data up, windrecorder/config_src/). The walk starts at this\n\
         executable, so a tray launched from bin\\ settles on the install and not on bin\\."
    )
}

impl Options {
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The delivery contract, and the reversal of an older one. This asserted `Usage` on the reasoning
    /// that "a stray double-click spawning a tray" was the failure to avoid — written when starting the
    /// app was something a person typed. Once the payload is the product, that reasoning inverts:
    /// Explorer passes an empty argv, and a tray that answers an empty argv with a usage block printed
    /// into a console it does not have is a double-click that appears to do nothing. `--help` and an
    /// unknown command still reach `Usage`, because those *are* somebody asking which commands exist.
    #[test]
    fn a_double_click_is_run_and_not_a_usage_message() {
        assert!(matches!(classify(&args(&[])), Invocation::Run(_)), "empty argv must reach the tray, not the exit");
        assert!(matches!(classify(&args(&["--help"])), Invocation::Usage(Unknown::Empty)), "asking for the command list still answers");
        assert!(matches!(classify(&args(&["tray"])), Invocation::Usage(Unknown::Command(_))), "an unknown command still names itself");
    }

    /// The `run` a double-click gets resolves its root the same way `windsvc run` does — by walking up
    /// from the executable to the install — rather than defaulting to the working directory, which for
    /// a launch from Explorer is wherever the shell happened to stand.
    #[test]
    fn the_implicit_run_takes_the_default_root_not_an_empty_one() {
        let Invocation::Run(options) = classify(&args(&[])) else { panic!("an empty argv is a run") };
        assert!(!options.root().as_os_str().is_empty(), "the root is resolved, not left blank");
    }

    #[test]
    fn an_unknown_subcommand_names_itself_in_the_usage() {
        let text = usage(Some("tray"));
        assert!(text.starts_with("unknown command 'tray'"), "{text}");
        assert!(text.contains("windsvc run") && text.contains("windsvc doctor"), "{text}");
    }

    #[test]
    fn both_subcommands_are_recognised() {
        assert!(matches!(classify(&args(&["run", "--root", "X"])), Invocation::Run(_)));
        assert!(matches!(classify(&args(&["doctor", "--root", "X"])), Invocation::Doctor(_)));
    }

    /// The version line is the anchor a support conversation is built on, so it has to name the
    /// binary, carry this crate's version, and say which profile produced the bytes.
    #[test]
    fn version_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windsvc "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        assert_eq!(line.matches(' ').count(), 2, "{line}");
    }

    /// `-V` is the short form every other binary in the toolset answers too, and the version is
    /// taken for the command word it replaces — never passed on to `run` or `doctor`.
    #[test]
    fn both_version_spellings_short_circuit_the_subcommand_match() {
        for spelling in ["--version", "-V"] {
            assert_eq!(classify(&args(&[spelling])), Invocation::Version, "{spelling}");
            // Even beside a subcommand-shaped word, because the flag is read first.
            assert_eq!(classify(&args(&[spelling, "--root", "Z:/nope"])), Invocation::Version, "{spelling}");
        }
        assert!(!usage(None).contains("unknown command"));
        assert!(usage(None).contains("--version"), "{}", usage(None));
    }

    #[test]
    fn root_accepts_both_spellings() {
        let spaced = classify(&args(&["doctor", "--root", "/tmp/install"]));
        let inline = classify(&args(&["doctor", "--root=/tmp/install"]));
        for invocation in [spaced, inline] {
            let Invocation::Doctor(options) = invocation else { panic!("expected a parsed doctor run") };
            assert_eq!(options.root, PathBuf::from("/tmp/install"));
        }
    }

    #[test]
    fn a_flag_without_its_value_is_an_error_message() {
        let Invocation::Bad(message) = classify(&args(&["doctor", "--root"])) else {
            panic!("must not silently fall back to the default root");
        };
        assert_eq!(message, "--root needs a value");
    }

    #[test]
    fn an_unexpected_flag_is_rejected_loudly() {
        let Invocation::Bad(message) = classify(&args(&["run", "--no-tray"])) else {
            panic!("unknown flags must not be ignored");
        };
        assert!(message.contains("--no-tray"), "{message}");
    }

    #[test]
    fn the_default_root_walks_up_to_an_install() {
        // The test binary lives under the real install tree, so the walk must land on its root — and
        // it must land there by the shared rule, not by spotting a `windrecorder/` directory, which a
        // standalone payload does not carry.
        let root = default_root();
        assert!(wind_base::install::is_install_root(&root), "default_root({root:?}) found no install");
        assert!(
            wind_base::install::defaults_file(&root).is_some(),
            "default_root({root:?}) cannot reach a shipped settings layer"
        );
    }

    /// The regression, stated as the answer rather than as the algorithm: the old hand-rolled walk
    /// gave up and returned the directory holding the executable, so on a standalone install the tray
    /// settled on `bin\` and handed `windrec --root <install>\bin` — a recorder that then built
    /// `bin\userdata\db\`, `bin\cache\` and `bin\cache_screenshot\` inside the program folder.
    #[test]
    fn a_standalone_layout_resolves_above_bin_and_never_to_the_directory_holding_the_exe() {
        let scratch = std::env::temp_dir().join(format!("windsvc-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let root = scratch.join("payload");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(root.join("config_src").join("config_default.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        assert!(!root.join("windrecorder").exists(), "the whole point of the case: no Python package");

        // Asked about the development tree this test actually runs in, the walk must go above the
        // folder holding the exe rather than settle on it.
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let walked = default_root();
        assert_ne!(walked, exe_dir, "the root came back as the directory holding the binary");
        assert!(exe_dir.starts_with(&walked), "default_root({walked:?}) is not an ancestor of {exe_dir:?}");

        // Asked about a `bin/` one level under a root that carries only the payload's own settings,
        // it must find that root. This is `wind_base::install`'s rule being exercised through the
        // tray's entry point, which is the pairing that used to be missing.
        assert_eq!(wind_base::install::resolve_root(None, &root.join("bin")), root);
        assert_eq!(
            wind_base::install::resolve_root(Some(PathBuf::from("somewhere/else")), &root.join("bin")),
            PathBuf::from("somewhere/else"),
            "--root is never second-guessed, not even by a correct walk"
        );
        std::fs::remove_dir_all(scratch).unwrap();
    }

    /// A relative root is the one input that can silently point the recorder at the wrong install:
    /// it is passed as `--root` *and* used as the child's cwd, so `..` would be walked twice.
    #[test]
    fn a_relative_root_is_made_absolute_without_inventing_a_volume_prefix() {
        let relative = absolute(Path::new(".."));
        assert!(relative.is_absolute(), "{relative:?}");
        assert!(
            !relative.starts_with("\\\\?\\"),
            "a verbatim prefix is rejected by the image loader: {relative:?}"
        );
        // A path that is already absolute survives unchanged, which is what a released install gives.
        assert_eq!(absolute(Path::new("D:/Windrecorder")), PathBuf::from("D:/Windrecorder"));
        // And a root that does not exist is still answerable, because `doctor` runs on broken installs.
        assert!(absolute(Path::new("Z:/nope/nothing")).is_absolute());
    }
}
