//! Finding and launching the binaries the tray supervises.
//!
//! The search order is the contract between a built tree and an installed one: `$WINDCAP_HOME`, then
//! `bin/`, then the install root, then `windcap/target/release`, then `windcap/target/debug`. Release
//! before debug is not cosmetic — a debug build is an order of magnitude slower, and a tray that
//! picked it silently would be running a user's background recorder from an artefact that cannot
//! carry the performance claim.
//!
//! There is exactly one implementation of everything the tray launches. The Python application was
//! deleted in 3f37cbf, so `windrec.exe` is the only recorder and `winduiweb.exe` is the only
//! interface: neither launch takes a `Config`, because there is no longer a key that could choose
//! between two of them, and a missing binary is an error rather than a downgrade. The egui
//! `windui.exe` left the shipped set on 2026-09-27 — see
//! `docs/adr/2026-09-27-winduiweb-is-the-only-interface.md` and [`BINARIES`].
//!
//! This belongs in `wind-base` next to `Config`, and every other supervisor-shaped binary should ask
//! it the same questions. It lives here because this crate owns no file outside its own directory.

use std::path::{Path, PathBuf};

use wind_base::config::Config;

/// Environment override that names a directory holding the built binaries.
pub const ENV_HOME: &str = "WINDCAP_HOME";
/// The profiles a cargo tree can hold a binary in, best first.
pub const PROFILES: [&str; 2] = ["release", "debug"];
/// Every native binary the tray knows the name of.
///
/// `windmcp` is in this list because the tray supervises it: a bridge the menu can talk about has to
/// be a binary `doctor` can report as found or missing, and the search order is the same one that
/// keeps a development checkout from picking up a stale `debug` build.
///
/// `windsetup` is here for the same reason it must be spawnable: the tray runs the upgrade migration
/// through it on the way in (see [`migrate_argv`] and `supervisor::boot`), so a `windsvc doctor` has to
/// be able to say whether the install can even perform that migration. It was previously in no
/// discovery list at all, which is how an upgrading install silently skipped its migration step.
///
/// `windui` — the egui window — is deliberately **not** in this list. It was removed on 2026-09-27 by
/// `docs/adr/2026-09-27-winduiweb-is-the-only-interface.md`, which makes `winduiweb` the only
/// interface this product ships and opens: `doctor` no longer counts a window that no released install
/// carries as part of the install's integrity, and `release.ps1` no longer stages it. The crate is
/// still built and still tested, because it is the only implementation of the frame door, the prompt
/// panel and the form field declarations — retiring the binary is not deleting the code. Nothing here
/// is softer for the six that remain: every one of them is still named, found or reported missing.
pub const BINARIES: [&str; 6] = ["windrec", "winduiweb", "windmaint", "windcapctl", "windmcp", "windsetup"];
/// The only thing that can record.
pub const RECORDER: &str = "windrec";
/// The window the tray opens: the HTML front end.
///
/// `windui` — the egui window — is no longer shipped: it left [`BINARIES`] and `release.ps1`'s payload
/// table on 2026-09-27, when `winduiweb` became the only interface the product carries. The crate is
/// still built and still tested, and `bin\windui.exe` is still double-clickable in a development
/// tree — that is the transition the ADR allows, not a promise about an install. The name is switched
/// here rather than looked up as a preference because two candidates would mean a fallback, and a tray
/// that quietly opens the older window when the newer one is not in the install inverts this file's own
/// rule: an absent binary is named, never masked. The user would get a window and never learn the one
/// they asked for was missing.
pub const INTERFACE: &str = "winduiweb";
/// The only thing that can migrate an existing install's data forward.
pub const SETUP: &str = "windsetup";

/// A command the tray can spawn: the program, then its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Spawn {
    /// The whole line, for the messages that have to name what failed.
    pub fn describe(&self) -> String {
        let mut line = self.program.display().to_string();
        for arg in &self.args {
            line.push(' ');
            if arg.contains(' ') {
                let quoted = format!("\"{arg}\"");
                line.push_str(&quoted);
            } else {
                line.push_str(arg);
            }
        }
        line
    }
}

/// Every directory a native binary could legitimately live in, most installed first.
pub fn candidate_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
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
    dirs
}

/// Absolute path to a native binary, or `None`. The `.exe` suffix is added if it is missing.
pub fn find_binary(name: &str, root: &Path) -> Option<PathBuf> {
    let file =
        if name.to_ascii_lowercase().ends_with(".exe") { name.to_string() } else { format!("{name}.exe") };
    candidate_dirs(root).into_iter().map(|dir| dir.join(&file)).find(|candidate| candidate.is_file())
}

/// Which build a found binary came from: `release`, `debug`, or `installed` for anything the
/// release layout put somewhere else. Kept apart from the path on purpose, so "which file" and
/// "what kind of build is it" can be reported separately — `windsvc doctor` prints both columns, and
/// a user comparing two machines needs to see that one found `bin\` and the other found a stale
/// `target\debug`.
pub fn describe_build(path: Option<&Path>) -> &'static str {
    let Some(path) = path else { return "missing" };
    match path.parent().and_then(|dir| dir.file_name()).and_then(|name| name.to_str()) {
        Some("release") => "release",
        Some("debug") => "debug",
        _ => "installed",
    }
}

/// An install that holds no binary for a job the tray has been asked to do.
///
/// This used to be impossible to reach, because every absent binary fell back on an interpreter. It
/// is now the only failure the launch path has, and it carries the directories it searched for
/// exactly that reason: "the recorder is missing from your install" and "recording is switched off"
/// are one balloon apart from each other and indistinguishable from anything else, so the message
/// has to name files and paths rather than express surprise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// What could not be launched, in the user's terms and written lower-case so that it reads in the
    /// middle of a sentence as well as at the start of one: "recording", "the interface".
    pub role: &'static str,
    /// The binary that was looked for, spelled the way [`BINARIES`] spells it.
    pub name: &'static str,
    /// Every directory that was searched, in [`candidate_dirs`]' order.
    pub searched: Vec<PathBuf>,
}

impl Missing {
    fn new(role: &'static str, name: &'static str, root: &Path) -> Missing {
        Missing { role, name, searched: candidate_dirs(root) }
    }

    /// The file that is absent, spelled the way Explorer shows it.
    pub fn file(&self) -> String {
        format!("{}.exe", self.name)
    }

    /// The balloon's title. `role` is lower-case precisely so this reads correctly for both jobs the
    /// tray can be asked to do and cannot do.
    pub fn title(&self) -> String {
        format!("Cannot start {}", self.role)
    }

    /// The one sentence, for a report that prints one value per line. `explain` opens with it, so the
    /// short and long forms of the complaint cannot disagree about which file is missing.
    pub fn headline(&self) -> String {
        format!("{} cannot start: this install holds no {}", capitalize(self.role), self.file())
    }

    /// The sentence the tray puts in a balloon and `doctor` puts beside the label.
    pub fn explain(&self) -> String {
        let lines = self.search_lines();
        let searched = lines.join("\n");
        format!(
            "{headline}\n\
             That is a missing file, not {role} being switched off — there is no second \
             implementation left to fall back on.\n\
             Searched, in order:\n{searched}\n\
             Build it with windcap\\build.ps1, or unpack the release zip over this directory so bin\\ is populated.",
            headline = self.headline(),
            role = self.role,
            searched = searched,
        )
    }

    /// The searched directories as pre-indented lines, for a report that wants them as a list.
    pub fn search_lines(&self) -> Vec<String> {
        self.searched.iter().map(|dir| format!("  {}", dir.display())).collect()
    }
}

/// [`Missing::role`] is stored lower-case so it reads in a sentence's middle; this is for the place it
/// has to open one.
fn capitalize(word: &str) -> String {
    let (first, rest) = word.split_at(1);
    format!("{}{}", first.to_ascii_uppercase(), rest)
}

/// Start recording: `windrec loop --root <root>`.
///
/// The contract is unchanged from the Python loop it replaced — a long-lived foreground process that
/// ends its current segment on `CTRL_BREAK_EVENT` and exits, which is all the supervisor knows about
/// what it supervises. What is new is that nothing else can honour that contract, so an install
/// without the binary reports [`Missing`] rather than quietly running an interpreter with no script.
pub fn recorder_argv(root: &Path) -> Result<Spawn, Missing> {
    let binary = find_binary(RECORDER, root).ok_or_else(|| Missing::new("recording", RECORDER, root))?;
    Ok(Spawn { program: binary, args: vec!["loop".into(), "--root".into(), root.display().to_string()] })
}

/// Open the interface: `winduiweb --root <root>`.
///
/// A window serves nothing, so there is no port to choose, no address to wait for and no log line to
/// scrape — all of which existed only while the interface was a Streamlit server.
pub fn ui_argv(root: &Path) -> Result<Spawn, Missing> {
    let binary = find_binary(INTERFACE, root).ok_or_else(|| Missing::new("the interface", INTERFACE, root))?;
    Ok(Spawn { program: binary, args: vec!["--root".into(), root.display().to_string()] })
}

/// The MCP bridge's own switch. This is the *same* key `windmcp` reads before it binds anything
/// (`windcap/mcp/src/runtime.rs`), which is the whole point: one key, read by the process that
/// starts the service and by the process that is the service, with no second copy of the decision
/// that can drift out of agreement with the first.
pub fn bridge_enabled(config: &Config) -> bool {
    config.bool_or("enable_mcp_server", false)
}

/// `windmcp serve --root <root>`, or `None` when the install holds no bridge to start.
///
/// The argument list is exactly two flags and it must stay that way. `windmcp` reads its host, its
/// port, its token and whether authentication is required out of `userdata/config_user.json`, and
/// accepts nothing else on the command line — a command line is readable by every process on the
/// machine, so a secret or a bind address passed here would be a secret disclosed. The tray's job is
/// to start the service the config describes, never to override it.
///
/// `None` is the whole answer, and it is a different shape from [`recorder_argv`]'s and
/// [`ui_argv`]'s failure on purpose: the bridge is a service the user asks for by name with
/// `enable_mcp_server`, so an install that predates it is a normal state and `doctor` reports it
/// beside the key that asked for it. The recorder and the interface are what the tray exists to
/// launch whether or not anyone switched them on, so their absence is an error with a message, not a
/// row in a table.
pub fn bridge_argv(root: &Path) -> Option<Spawn> {
    let binary = find_binary("windmcp", root)?;
    Some(Spawn { program: binary, args: vec!["serve".into(), "--root".into(), root.display().to_string()] })
}

/// `windsetup migrate --root <root>` — the upgrade migration the tray runs before it may record.
///
/// This is the fix for the install that would otherwise upgrade silently and stay half-migrated: the
/// seven migration steps in `windsetup` were complete and re-entrant, but nothing spawned the binary,
/// so an existing user who copied this build over their old one never got the `userdata/` split, the
/// 0.0.12 retry tag, or the index's `win_title`/`deep_linking` columns. The tray *owns the trigger*
/// (it is the one process every desktop launch passes through, and it holds the tray lock so exactly
/// one tray can migrate), and `windsetup` owns the implementation (it is the one binary permitted to
/// rewrite the index — the supervisor itself opens no database, per this crate's own contract).
///
/// The command line is two arguments and must stay that way: `windsetup` reads the install root from
/// `--root` and nothing else, and a `migrate` invocation must not carry an override that would make the
/// tray's automatic run differ from the `windsetup migrate` a human is told to run when it fails.
pub fn migrate_argv(root: &Path) -> Result<Spawn, Missing> {
    let binary = find_binary(SETUP, root).ok_or_else(|| Missing::new("the upgrade migration", SETUP, root))?;
    Ok(Spawn { program: binary, args: vec!["migrate".into(), "--root".into(), root.display().to_string()] })
}

/// `windsetup init --root <root>` — the first-run layout the tray runs for itself.
///
/// [`migrate_argv`] brings an existing install's data forward; this is the other half, the tree an
/// install has before there is any data in it. `windsetup init` is the only code in the workspace
/// that creates `userdata/` and the `result_*` folders the interface writes into without checking,
/// and until now a person had to type that command before double-clicking the icon meant anything —
/// the gap `release.ps1`'s own header admitted to. The tray is the one process every desktop start
/// passes through, so it is the one place the step can be automatic.
///
/// It stays a spawn rather than becoming a dependency on `wind-setup`: the layout of an install is
/// `windsetup`'s knowledge alone, `supervisor/Cargo.toml` deliberately links neither `wind-store`
/// nor bundled SQLite, and this crate's header rules out the tray touching the index. The argument
/// list is two and must stay two, for the same reason [`migrate_argv`]'s is — the automatic run must
/// be byte-for-byte the command a human is told to type when it fails.
pub fn init_argv(root: &Path) -> Result<Spawn, Missing> {
    let binary = find_binary(SETUP, root).ok_or_else(|| Missing::new("the first-run layout", SETUP, root))?;
    Ok(Spawn { program: binary, args: vec!["init".into(), "--root".into(), root.display().to_string()] })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree that only exists to hold files with the right names in the right places.
    fn tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windsvc-native-{}-{}", std::process::id(), unique()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("windcap/target/release")).unwrap();
        std::fs::create_dir_all(dir.join("windcap/target/debug")).unwrap();
        dir
    }

    fn unique() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"MZ").unwrap();
        path
    }

    /// Every test in this module reads `$WINDCAP_HOME`, because `candidate_dirs` does, and the
    /// environment is process-global while cargo's test threads are not. Without this the override
    /// test publishes its own value for the length of its body and any other thread that happens to
    /// resolve a binary in that window finds a file in somebody else's temporary directory — a flake
    /// that reproduces about two runs in three, and one no assertion can distinguish from a real
    /// regression in the search order. So the setter and every reader take this lock, and each test
    /// holds it for its whole body: `let _serial = env_guard();`.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn the_installed_bin_directory_beats_every_cargo_profile() {
        let _serial = env_guard();
        let root = tree();
        touch(&root, "bin/windrec.exe");
        touch(&root, "windcap/target/release/windrec.exe");
        touch(&root, "windcap/target/debug/windrec.exe");
        assert_eq!(find_binary("windrec", &root).unwrap(), root.join("bin/windrec.exe"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn release_beats_debug_when_nothing_is_installed() {
        let _serial = env_guard();
        let root = tree();
        touch(&root, "windcap/target/debug/windrec.exe");
        touch(&root, "windcap/target/release/windrec.exe");
        assert_eq!(find_binary("windrec", &root).unwrap(), root.join("windcap/target/release/windrec.exe"));
        std::fs::remove_file(root.join("windcap/target/release/windrec.exe")).unwrap();
        assert_eq!(find_binary("windrec", &root).unwrap(), root.join("windcap/target/debug/windrec.exe"));
        assert_eq!(describe_build(find_binary("windrec", &root).as_deref()), "debug");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Retargeted at `winduiweb.exe` on 2026-09-27 rather than dropped: the case is that the install
    /// root outranks the cargo tree and that a binary sitting there reads as `installed`, and it was
    /// carried by the egui name only because that was the interface at the time.
    #[test]
    fn the_install_root_itself_ranks_above_the_cargo_tree() {
        let _serial = env_guard();
        let root = tree();
        touch(&root, "winduiweb.exe");
        touch(&root, "windcap/target/release/winduiweb.exe");
        assert_eq!(find_binary("winduiweb", &root).unwrap(), root.join("winduiweb.exe"));
        assert_eq!(describe_build(Some(&root.join("winduiweb.exe"))), "installed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn windcap_home_overrides_the_whole_layout() {
        let _serial = env_guard();
        let root = tree();
        let home = root.join("elsewhere");
        touch(&home, "windrec.exe");
        touch(&root, "bin/windrec.exe");
        std::env::set_var(ENV_HOME, &home);
        assert_eq!(find_binary("windrec", &root).unwrap(), home.join("windrec.exe"));
        std::env::remove_var(ENV_HOME);
        assert_eq!(find_binary("windrec", &root).unwrap(), root.join("bin/windrec.exe"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_search_order_is_bin_then_root_then_release_then_debug() {
        let _serial = env_guard();
        let root = PathBuf::from("D:/Windrecorder");
        let dirs = candidate_dirs(&root);
        let tail = &dirs[dirs.len() - 4..];
        assert_eq!(
            tail,
            [
                PathBuf::from("D:/Windrecorder/bin"),
                root.clone(),
                root.join("windcap").join("target").join("release"),
                root.join("windcap").join("target").join("debug"),
            ]
        );
    }

    #[test]
    fn a_missing_binary_is_reported_as_missing_not_as_a_default_path() {
        let _serial = env_guard();
        let root = tree();
        assert_eq!(find_binary("windrec", &root), None);
        assert_eq!(describe_build(None), "missing");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The first-run command, asserted as a vector for the same reason the recorder's is: an `init`
    /// spawned with one argument out of place is a `windsetup` usage error, and a tray that refuses
    /// to start because it mistyped its own installer is a worse experience than the command line it
    /// replaced. `bin\windsetup.exe` is the payload's shape, so that is the path pinned here.
    #[test]
    fn the_first_run_command_is_windsetup_init_on_this_install() {
        let _serial = env_guard();
        let root = tree();
        let binary = touch(&root, "bin/windsetup.exe");
        let spawn = init_argv(&root).expect("a present windsetup must yield a command");
        assert_eq!(spawn.program, binary);
        assert_eq!(spawn.args, vec!["init".to_string(), "--root".to_string(), root.display().to_string()]);
        assert!(spawn.describe().contains("init --root"), "{}", spawn.describe());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The absent-binary message has to name the job it blocks, because the role string is what
    /// reaches the balloon: "Cannot start the first-run layout" and "Cannot start the upgrade
    /// migration" are different things for a user to go and fix, and both are this one file.
    #[test]
    fn an_absent_windsetup_names_the_first_run_it_blocks() {
        let _serial = env_guard();
        let root = tree();
        let missing = init_argv(&root).expect_err("this tree has no windsetup.exe in it");
        assert_eq!(missing.role, "the first-run layout");
        assert_eq!(missing.file(), "windsetup.exe");
        assert!(missing.explain().contains("not the first-run layout being switched off"), "{}", missing.explain());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A config written before this change can still say `"use_native_core": false`. Nothing reads
    /// that key any more, and the launcher must not: a stock install with a stock config records
    /// natively, which is the exact defect this removes.
    fn config_holding(settings: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("windsvc-config-{}-{}", std::process::id(), unique()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), settings).unwrap();
        Config::load(&dir).unwrap()
    }

    /// The exact argv the tray spawns, asserted as a vector: a recorder started with one argument out
    /// of place exits with a usage error before it captures anything.
    #[test]
    fn the_recorder_command_is_binary_loop_root() {
        let _serial = env_guard();
        let root = tree();
        let binary = touch(&root, "bin/windrec.exe");
        let spawn = recorder_argv(&root).expect("a present windrec must yield a command");
        assert_eq!(spawn.program, binary);
        assert_eq!(spawn.args, vec!["loop".to_string(), "--root".to_string(), root.display().to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The defect this replaced: with `"use_native_core": false` — the shipped default, and the value
    /// in every config written before the Python app was deleted — the tray built
    /// `python.exe -u record_screen.py` and recorded nothing. No config now has any say in what the
    /// recorder command is, and no command line names an interpreter or a `.py`.
    #[test]
    fn the_recorder_command_is_the_same_whatever_the_config_says() {
        let _serial = env_guard();
        let root = tree();
        touch(&root, "bin/windrec.exe");
        let expected = recorder_argv(&root).expect("the binary is present");
        for settings in [
            "{}",
            r#"{"use_native_core": false}"#,
            r#"{"use_native_core": true}"#,
            r#"{"use_native_core": "false"}"#,
            r#"{"start_recording_on_startup": false, "record_mode": "ffmpeg"}"#,
        ] {
            let config = config_holding(settings);
            // Read the config at all only to prove the launcher does not: `recorder_argv` has no
            // parameter for it. This asserts the *shape* of the guarantee.
            let spawn = recorder_argv(&root).expect("an opt-out key must not remove the command");
            assert_eq!(spawn, expected, "config {settings} changed the recorder command");
            let line = spawn.describe();
            for forbidden in ["python", "record_screen", "streamlit", ".py"] {
                assert!(!line.contains(forbidden), "the recorder command names {forbidden}: {line}");
            }
            assert!(line.contains("loop --root"), "the command is still `windrec loop --root`: {line}");
            let _ = config;
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_interface_command_is_binary_root_only() {
        let _serial = env_guard();
        let root = tree();
        // Spelled from `INTERFACE` rather than hard-coded: the moment this name changes, a test that
        // pins the old spelling starts passing for the wrong reason.
        let binary = touch(&root, &format!("bin/{}.exe", INTERFACE));
        let spawn = ui_argv(&root).expect("a present interface binary must yield a command");
        assert_eq!(spawn.program, binary);
        assert_eq!(spawn.args, vec!["--root".to_string(), root.display().to_string()]);
        // A window serves nothing: no port, no address, and nothing to wait for.
        let line = spawn.describe();
        for forbidden in ["port", "8501", "http", "streamlit", "webui.py", "python"] {
            assert!(!line.contains(forbidden), "the interface command names {forbidden}: {line}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The mixed install the `windui`/`windrec` split used to exist for is now two independent
    /// answers: the recorder still runs, and the interface says which file is absent instead of
    /// reaching for a server that has nothing to serve.
    #[test]
    fn a_tree_with_the_recorder_but_no_interface_reports_the_interface_missing() {
        let _serial = env_guard();
        let root = tree();
        touch(&root, "bin/windrec.exe");
        assert!(recorder_argv(&root).is_ok(), "the recorder does not borrow the interface's answer");
        let missing = ui_argv(&root).expect_err("no interface binary means no window to start");
        assert_eq!(missing.name, INTERFACE);
        assert_eq!(missing.file(), format!("{}.exe", INTERFACE));
        assert!(recorder_argv(&root).is_ok(), "and recording is unaffected by it");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole point of the type. An absent binary used to be indistinguishable from an idle
    /// recorder, because the command that was produced ran an interpreter over a deleted script. So
    /// the error has to name the file, name every directory it looked in, and say out loud that this
    /// is not the switched-off state.
    #[test]
    fn a_missing_binary_is_an_error_that_names_the_file_and_every_directory_searched() {
        let _serial = env_guard();
        let root = tree();
        let missing = recorder_argv(&root).expect_err("nothing in this tree can record");
        let message = missing.explain();
        assert_eq!(missing.name, RECORDER);
        assert_eq!(missing.searched, candidate_dirs(&root), "the message lists what was searched");
        assert!(message.contains("windrec.exe"), "{message}");
        assert!(message.contains(&root.join("bin").display().to_string()), "{message}");
        assert!(
            message.contains("not recording being switched off"),
            "the two states a user cannot otherwise tell apart have to be named: {message}"
        );
        // And it is not a command line wearing an error: no interpreter, no script, no silent ok.
        assert!(!message.contains("python.exe"), "{message}");
        for dir in missing.searched.iter().skip(1) {
            assert!(dir.starts_with(&root), "every searched directory is inside this install: {dir:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `.venv` was the release installer's Python, and `python_executable` preferred it over anything
    /// on `PATH`. Both are gone: a tree that still carries one from an older install must find its
    /// recorder in `bin\` exactly as a tree that does not, and the tray must not be able to resolve a
    /// path to the interpreter that used to run there.
    #[test]
    fn a_venv_in_the_tree_changes_nothing_and_hides_no_binary() {
        let _serial = env_guard();
        let root = tree();
        touch(&root, ".venv/Scripts/python.exe");
        touch(&root, "bin/windrec.exe");
        let spawn = recorder_argv(&root).expect("windrec.exe is there");
        assert_eq!(spawn.program, root.join("bin/windrec.exe"), "the venv did not win");
        assert_eq!(find_binary("windrec", &root).unwrap(), root.join("bin/windrec.exe"));
        assert_eq!(find_binary("python", &root), None, "the tray does not look for an interpreter");
        assert!(!candidate_dirs(&root).iter().any(|dir| dir.join("python.exe").is_file()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_spawn_describes_itself_with_the_command_that_failed() {
        let spawn = Spawn { program: PathBuf::from("D:/Windrec/bin/windrec.exe"), args: vec!["loop".into(), "--root".into(), "D:/Wind rec".into()] };
        let line = spawn.describe();
        assert!(line.contains("windrec.exe loop --root"), "{line}");
        assert!(line.contains("\"D:/Wind rec\""), "paths with spaces must stay quoted: {line}");
    }

    fn bridge_config(enabled: bool) -> Config {
        let dir = std::env::temp_dir().join(format!("windsvc-bridge-config-{}-{}", std::process::id(), unique()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            format!(r#"{{"enable_mcp_server": {enabled}, "mcp_server_token": "a-token-that-is-long-enough-to-be-one"}}"#),
        )
        .unwrap();
        Config::load(&dir).unwrap()
    }

    /// The whole defect this command line answers: `windmcp` existed, was built, was shipped, and
    /// nothing ever started it. Pinned as an exact vector because everything beyond `serve --root`
    /// is either redundant (the bridge reads its own config) or a disclosure (a secret in argv is
    /// readable by every process on the machine, which is why `windmcp` refuses one).
    #[test]
    fn the_bridge_command_is_serve_and_root_and_nothing_else() {
        let _serial = env_guard();
        let root = tree();
        let binary = touch(&root, "bin/windmcp.exe");
        let spawn = bridge_argv(&root).expect("a present windmcp must yield a command");
        assert_eq!(spawn.program, binary);
        assert_eq!(spawn.args, vec!["serve".to_string(), "--root".to_string(), root.display().to_string()]);
        assert_eq!(spawn.args.len(), 3, "the bridge takes no third argument");
        let line = spawn.describe();
        for forbidden in ["--token", "--host", "--port", "--no-auth", "a-token-that-is-long-enough"] {
            assert!(!line.contains(forbidden), "the command line must not carry {forbidden}: {line}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two independent conditions, and the switch is the one the bridge itself obeys. Absent means
    /// off: an install written before this feature existed must not gain a network listener.
    #[test]
    fn the_bridge_starts_only_when_the_switch_and_the_binary_both_agree() {
        let _serial = env_guard();
        let root = tree();
        touch(&root, "bin/windmcp.exe");
        assert!(bridge_enabled(&bridge_config(true)), "the key is the switch");
        assert!(!bridge_enabled(&bridge_config(false)));
        assert!(bridge_argv(&root).is_some(), "a present binary is startable once the key is on");

        // Enabled, but this install never got the binary: no fallback exists for a protocol only
        // this one program speaks, so the honest answer is "nothing to start".
        let bare = tree();
        assert!(bridge_enabled(&bridge_config(true)));
        assert_eq!(bridge_argv(&bare), None);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }

    /// `doctor` reports one row per known binary and the bridge is the fork's namesake feature, so
    /// an install that lacks it has to say so in the same table as the rest.
    #[test]
    fn the_bridge_is_a_reported_binary() {
        let _serial = env_guard();
        assert!(BINARIES.contains(&"windmcp"), "{BINARIES:?}");
        let root = tree();
        assert_eq!(find_binary("windmcp", &root), None);
        assert_eq!(describe_build(find_binary("windmcp", &root).as_deref()), "missing");
        touch(&root, "bin/windmcp.exe");
        assert_eq!(find_binary("windmcp", &root).unwrap(), root.join("bin/windmcp.exe"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole defect (3): `windsetup` was built, shipped and staged, but lived in no discovery list
    /// and no spawn path, so an upgrading install never ran its migration. It is now a binary the tray
    /// finds (so `doctor` can report it) and a command the tray runs. Pinned as an exact vector because
    /// `migrate`'s only argument is the root — a stray `--from-version` here would silently skip steps.
    #[test]
    fn the_setup_binary_is_discovered_and_the_migration_command_is_migrate_root() {
        let _serial = env_guard();
        assert!(BINARIES.contains(&SETUP), "{BINARIES:?}");
        let root = tree();
        let missing = migrate_argv(&root).expect_err("an install with no windsetup cannot migrate");
        assert_eq!(missing.name, SETUP);
        assert!(missing.explain().contains("windsetup.exe"), "{}", missing.explain());
        let binary = touch(&root, "bin/windsetup.exe");
        let spawn = migrate_argv(&root).expect("a present windsetup must yield a migration command");
        assert_eq!(spawn.program, binary);
        assert_eq!(spawn.args, vec!["migrate".to_string(), "--root".to_string(), root.display().to_string()]);
        assert_eq!(spawn.args.len(), 3, "migrate takes exactly the root and nothing else");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Which window the tray opens is a decision, not an accident of a string. `windui` is the egui
    /// front end and `winduiweb` the HTML one; both are still built, but only this one is the product,
    /// and the tray opens exactly it. A failure here means somebody moved the product's front door, and
    /// every message that names "the interface" now points at a different binary than the payload's
    /// `bin\` was staged with.
    #[test]
    fn the_tray_opens_the_html_window() {
        let _serial = env_guard();
        assert_eq!(INTERFACE, "winduiweb");
        assert!(BINARIES.contains(&INTERFACE), "the one the tray opens is a known binary: {BINARIES:?}");
    }

    /// The retirement, stated so it cannot be undone by an edit that means something else.
    /// `docs/adr/2026-09-27-winduiweb-is-the-only-interface.md` takes the egui window out of the
    /// shipped set — it is still a crate, still built by `build.ps1`, still tested, and no longer a
    /// binary `doctor` counts against an install or `release.ps1` puts in a zip. Re-adding the name to
    /// [`BINARIES`] would make every released install that omits it report a missing piece it was never
    /// supposed to carry, which is the failure this pins shut.
    #[test]
    fn the_egui_window_is_not_a_shipped_binary() {
        let _serial = env_guard();
        assert!(!BINARIES.contains(&"windui"), "windui was retired from the shipped set on 2026-09-27: {BINARIES:?}");
        assert!(BINARIES.contains(&"winduiweb"), "the interface the tray opens is the only front end here: {BINARIES:?}");
        assert!(BINARIES.contains(&INTERFACE), "and INTERFACE names a binary this list actually knows: {BINARIES:?}");
    }
}
