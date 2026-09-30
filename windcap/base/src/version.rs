//! `--version`, answered the same way by every native binary.
//!
//! The line is `<name> <semver> (<profile>)`, and each of the three parts has its own reason to be
//! there. The name is the *binary*'s, supplied by the caller, because a user holding
//! `windmaint.exe` and a user holding the `wind-maint` package are describing one artefact and
//! should read the word they typed. The semver is `env!("CARGO_PKG_VERSION")` expanded *in the
//! calling crate* — passed in rather than read here, because read here it would be this helper's
//! version, which is only accidentally the same number.
//!
//! The profile is the part that turns "which build" into an answer someone can act on:
//! `release.ps1` warns that a debug build is "roughly an order of magnitude slower", and
//! `native_runtime.describe_build()` labels a debug build wherever it finds one. Both use the bare
//! words `debug` and `release`, so this does too, and the three places a build is named cannot
//! disagree.
//!
//! Why this is a separate module rather than a line inside each `main`: a support conversation
//! needs one string out of eleven executables, and eleven copies of a `format!` are eleven ways to write
//! `(dbg)` in one build and `(debug)` in another.

/// The build profile this binary was compiled with, in the words the rest of the project uses.
///
/// `cfg!(debug_assertions)` is the same switch `windui`'s `usage()` and `--exit-after` are gated
/// on, and the same one `release.ps1` reads off cargo's own output directory.
#[cfg(debug_assertions)]
pub fn profile() -> &'static str {
    "debug"
}

/// The release half of [`profile`], kept beside it for the same reason `windui` keeps two `usage()`.
#[cfg(not(debug_assertions))]
pub fn profile() -> &'static str {
    "release"
}

/// The one line a user quotes when they report a problem: `<name> <semver> (<profile>)`.
pub fn line(name: &str, semver: &str) -> String {
    format!("{name} {semver} ({})", profile())
}

/// Is this argument a request for the version line?
///
/// `--version` and `-V` only, and case-sensitively: `-v` is left alone because several of these
/// binaries read a positional argument that could one day be a flag-shaped word, and a tool that
/// guesses is a tool that misroutes.
pub fn is_flag(argument: &str) -> bool {
    matches!(argument, "--version" | "-V")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_is_name_then_version_then_profile() {
        let line = line("windrec", "1.2.3");
        assert!(line.starts_with("windrec 1.2.3 "), "{line}");
        assert!(line.ends_with(" (debug)") || line.ends_with(" (release)"), "{line}");
        assert_eq!(line.lines().count(), 1, "one line, whatever it names");
    }

    /// The profile word is not free-form: it has to be the one `native_runtime.describe_build()`
    /// returns and `release.ps1` writes, or the two halves of a bug report disagree.
    #[test]
    fn the_profile_is_one_of_the_two_words_the_python_runtime_recognises() {
        assert!(matches!(profile(), "debug" | "release"), "{}", profile());
        assert_eq!(line("x", "0"), format!("x 0 ({})", profile()));
    }

    #[test]
    fn both_version_spellings_are_flags_and_other_things_are_not() {
        assert!(is_flag("--version"));
        assert!(is_flag("-V"));
        for not in ["-v", "--Version", "version", "-h", "--help", "", "doctor"] {
            assert!(!is_flag(not), "{not:?} is not a version flag");
        }
    }
}
