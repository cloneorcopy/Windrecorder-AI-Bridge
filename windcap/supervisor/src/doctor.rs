//! `windsvc doctor` — the report for a tray that "isn't working".
//!
//! Everything here is read-only, and everything is phrased as an answer rather than a dump: which
//! binary a click would run, from where, whether a lock's owner is a live process, and which of the
//! two log files to open. A diagnostic that only prints paths is a diagnostic the user has to
//! interpret themselves, which is the thing that went wrong often enough for this command to exist.
//!
//! It never fails on a missing directory and never panics, because a broken install is exactly when
//! the command gets run.

use std::path::Path;

use wind_base::config::Config;
use wind_base::fslock::{lock_state, LockState};

use wind_base::i18n::Catalog;
use crate::layout::Layout;
use crate::menu::{self, Snapshot};
use crate::native::{self, Missing, Spawn};
use crate::options::Options;
use crate::update;

/// The report. `Ok` only means the config could be read; a line saying "missing" is a result, not an
/// error, and the exit code stays 0 for it.
pub fn report(options: &Options) -> Result<(), String> {
    // `Config::load` treats a missing file as "no keys", which is right for a recorder that can fall
    // back to its own defaults and wrong for a diagnostic: every line below would then describe an
    // install that is empty rather than say that this is not an install directory at all.
    let root = crate::options::absolute(options.root());
    if !wind_base::install::is_install_root(&root) {
        return Err(format!(
            "{} is not a Windrecorder install — it carries no {} (looked in {} and {})",
            root.display(),
            wind_base::install::DEFAULTS_BASENAME,
            root.join(wind_base::install::CONFIG_SRC).display(),
            root.join(wind_base::install::LEGACY_CONFIG_SRC).display(),
        ));
    }
    let config = Config::load(&root).map_err(|error| error.to_string())?;
    let layout = Layout::from_config(&config);
    let catalog = Catalog::load(&layout.root, &config.str_or("lang", "en"));

    line("root", &path(&layout.root));
    line("config", &format!("{} (merged with userdata/config_user.json)", install_marker(&root).display()));
    line("log dir", &format!("{}{}", path(&layout.log_dir), exists_note(layout.log_dir.is_dir())));
    line("record log", &format!("{}{}", path(&layout.recording.out), exists_note(layout.recording.out.exists())));
    line("record err", &path(&layout.recording.err));
    // The pair the interface process writes into. The file names are the historical `webui.*` ones
    // a user may already have from an older install; the label says what is in them now.
    line("interface log", &path(&layout.interface.out));
    line("interface err", &path(&layout.interface.err));
    line("mcp log", &format!("{}{}", path(&layout.bridge.out), exists_note(layout.bridge.out.exists())));
    line("mcp err", &path(&layout.bridge.err));
    // Two separate lines, because they are two separate things and the old single "icons" line got
    // both wrong: it named `__assets__` as the tray's icon source (the tray's icons are compiled into
    // this .exe — see `src\icon.rs`), and it printed `[absent]` against a standalone install, which is
    // exactly where that was never true.
    line(
        "icons",
        &format!(
            "compiled into {} — resource #{}",
            path(&this_exe()),
            crate::icon::ICON_RESOURCES.iter().map(u16::to_string).collect::<Vec<_>>().join(" and #")
        ),
    );
    line("ocr fixtures", &format!("{}{}", path(&layout.assets), exists_note(layout.assets.is_dir())));
    line(
        "languages",
        &format!(
            "{}{}",
            path(&layout.languages),
            if catalog.loaded() { "" } else { "  [UNREADABLE — every label falls back to its key]" }
        ),
    );
    // The tray's own version, from the binary, and then the honest state of "can this tray update
    // itself" — which is the same answer for every install: no, it cannot, and the lines below say
    // what the upgrade path actually is and which changelog describes the build being run.
    line("version", &update::local_version(&layout.root));
    for (label, value) in update_state_lines(&layout) {
        line(label, &value);
    }
    println!();

    println!("binaries — searched, in order");
    for dir in native::candidate_dirs(&layout.root) {
        println!("  {}", describe(&dir));
    }
    println!("  {:<12} {:<10} {}", "binary", "build", "path");
    for name in native::BINARIES {
        let found = native::find_binary(name, &layout.root);
        match &found {
            Some(path) => println!("  {:<12} {:<10} {}", name, native::describe_build(Some(path)), path.display()),
            None => println!("  {:<12} {:<10} {}", name, "missing", "-"),
        }
    }
    println!();

    // What each menu item would actually run. There is no opt-in above this any more: the binaries
    // named here are the only implementations that exist, so an absent one is an error, and the two
    // lines below have to be able to say so in words a user can act on.
    println!("launches — the commands the tray would run");
    let recorder = native::recorder_argv(&layout.root);
    let interface = native::ui_argv(&layout.root);
    line("recording via", &describe_launch(&recorder));
    line("interface via", &describe_launch(&interface));
    let mut problems = Vec::new();
    if let Err(missing) = &recorder {
        problems.push(format!("{} is not installed, so recording cannot start", missing.file()));
    }
    if let Err(missing) = &interface {
        problems.push(format!("{} is not installed, so the search and settings window cannot open", missing.file()));
    }
    if problems.is_empty() {
        line("launch check", "every binary the tray launches is present");
    } else {
        for (index, problem) in problems.iter().enumerate() {
            line(if index == 0 { "MISSING" } else { "" }, problem);
        }
        for (label, outcome) in [("recording", &recorder), ("interface", &interface)] {
            if let Err(missing) = outcome {
                println!("  {label} searched, in order:");
                for dir in &missing.searched {
                    println!("    {}", describe(dir));
                }
            }
        }
    }
    println!();

    println!("mcp bridge — the HTTP service `windmcp serve` is, which the tray starts and stops");
    for (label, value) in bridge_report(&config, &layout) {
        line(label, &value);
    }
    println!();

    println!("locks — a lock is only live if the pid inside it is running");
    print_file_lock("tray", &layout.tray_lock);
    print_file_lock("record", &layout.record_lock);
    print_file_lock("mcp", &layout.bridge_lock);
    print_maintain_lock(&layout.maintain_lock);
    println!();

    println!("the menu, right now");
    // The snapshot is the same one `tray` builds; a recording read out of the lock is what a second
    // copy of the tray would see too, which is the whole point of the pid-in-file protocol.
    let recording = matches!(lock_state(&layout.record_lock), LockState::HeldBy { alive: true, .. });
    // A window leaves no lock, so no other process can see one: `interface_running` is false
    // here not because nothing is up but because this process cannot know. The running tray does
    // know, because it owns the child handle — which is why its menu is the other half of this report.
    let mut snapshot = Snapshot::idle(update::local_version(&layout.root));
    snapshot.recording = recording;
    snapshot.changelog_present = layout.changelog_target().is_some();
    snapshot.bridge_enabled = native::bridge_enabled(&config);
    snapshot.bridge_running = bridge_is_running(&layout);
    // The same file the running tray reads, so this report and that menu cannot disagree about whether
    // frames are being written. Absent is its own answer: a recorder from before this file existed
    // proves only the lock.
    snapshot.capture = wind_base::fslock::read_capture(&config.record_state_path());
    line("hover text", &menu::tooltip(&snapshot, &catalog));
    for row in menu::rows(&snapshot, &catalog) {
        if let Some(item) = row.as_item() {
            let state = if item.enabled { "" } else { "  (disabled)" };
            println!("{}", format!("  {:<46}{state}", item.label).trim_end());
        }
    }
    // The window is named from `native::INTERFACE` rather than spelled here: since the egui front end
    // was retired from the shipped set this line would otherwise have kept telling a user that a
    // binary their install does not carry is the one they cannot see the state of.
    println!(
        "  interface state is not visible to this process — {} takes no lock; run `windsvc \
         doctor` while the tray is up and read its menu for that. The bridge is visible: it is the \
         pid in {}",
        native::INTERFACE,
        path(&layout.bridge_lock)
    );
    Ok(())
}

/// The file that makes a directory a Windrecorder install rather than a folder.
///
/// Both `doctor` and the tray's own startup ask this before creating anything, because the answer is
/// the difference between "this install is empty" and "you pointed me at the wrong directory".
///
/// Which of the two layouts the file is in is [`wind_base::install`]'s decision, not this function's:
/// the payload manages `config_src/` and a legacy overlay install still keeps `windrecorder/`. The
/// tray reports whichever one it actually found, and falls back to naming the payload location so the
/// "not an install" message points at a path the user can act on.
pub fn install_marker(root: &Path) -> std::path::PathBuf {
    wind_base::install::defaults_file(root)
        .unwrap_or_else(|| root.join(wind_base::install::CONFIG_SRC).join(wind_base::install::DEFAULTS_BASENAME))
}

fn print_file_lock(what: &str, path: &Path) {
    match lock_state(path) {
        LockState::Free => println!("  {what:<8} free        {}", path.display()),
        LockState::Owned => println!("  {what:<8} held by this very process ({}), which is not a {what} owner", std::process::id()),
        LockState::HeldBy { alive, .. } => {
            // The body is echoed back because it *is* the protocol: seeing the pid proves the file was
            // read rather than guessed at, and it is the number a user can check in Task Manager.
            let body = std::fs::read_to_string(path).unwrap_or_default();
            let verdict = if alive { "LIVE" } else { "stale — the owner process is gone" };
            println!("  {what:<8} pid {body:<10} {verdict}  {}", path.display());
        }
        LockState::Unreadable => {
            println!("  {what:<8} UNREADABLE  {} — a lock this cannot parse is refused, never deleted", path.display())
        }
    }
}

fn print_maintain_lock(dir: &Path) {
    if !dir.exists() {
        println!("  maintain free        {}", dir.display());
        return;
    }
    // A directory, not a file: `windrecorder.lock.FileLock` uses it as a container for one marker per
    // video, and `windmaint` adds a `PID` child of its own. Both shapes have to be reported, because
    // they mean different things — an empty container is an abandoned claim, a `PID` inside it is a
    // live one.
    let pid_file = dir.join("PID");
    let markers = std::fs::read_dir(dir).map(|entries| entries.flatten().count()).unwrap_or(0);
    match std::fs::read_to_string(&pid_file) {
        Ok(body) => match body.trim().parse::<u32>() {
            Ok(pid) => {
                let alive = wind_base::fslock::is_process_running(pid);
                println!(
                    "  maintain pid {pid:<9} {}({markers} entr(y|ies) inside) {}",
                    if alive { "LIVE" } else { "stale — the owner process is gone " },
                    dir.display()
                );
            }
            Err(_) => println!("  maintain UNREADABLE {} ({markers} entries)", pid_file.display()),
        },
        Err(_) => println!(
            "  maintain claimed by a directory lock only, with {markers} entr(y|ies) — a Python pass or an abandoned one"
        ),
    }
}

/// The updater question as two lines: what can update this install, and which file on disk
/// describes this build.
///
/// The first is the answer a user of a tray that once offered an updater is owed — plainly, and with
/// the thing that actually replaces a running install named. The second resolves exactly like the
/// menu row resolves, from the same [`Layout::changelog_target`], so the report and the menu can
/// never disagree about whether there is a changelog and which file it is.
fn update_state_lines(layout: &Layout) -> Vec<(&'static str, String)> {
    let updates = "none — this build ships no updater; a release zip unpacked over this install is the only upgrade path";
    let changelog = match layout.changelog_target() {
        Some(target) => format!("{}  (opened by \"See what's new\")", path(&target)),
        None => format!("none — this install carries neither {} nor {}", path(&layout.release_notes), path(&layout.changelog)),
    };
    vec![("updates", updates.to_string()), ("changelog", changelog)]
}

/// Is a bridge up? Read from its pid file, and so honest about a `windmcp serve` somebody started
/// by hand — which is the case the tray must refuse to double-start onto.
fn bridge_is_running(layout: &Layout) -> bool {
    matches!(lock_state(&layout.bridge_lock), LockState::HeldBy { alive: true, .. })
}

/// The bridge's answers, as labelled lines. Its own section because it is the fork's namesake
/// feature and the one switch a user is told about in `windmcp --help` — before this existed the
/// key was in no shipped file, so following the help text changed nothing and `doctor` said so
/// nowhere. The three questions this has to answer unprompted are the three the help text implies:
/// is it on, is it running, and where do I look when it is not.
///
/// It reports the bind as the raw settings the bridge will itself read, never as a verdict. `windmcp`
/// owns that decision — including the port it deliberately does not sanitize and the minimum length
/// a secret has to clear — and a second copy of either in the tray is a second answer to drift.
fn bridge_report(config: &Config, layout: &Layout) -> Vec<(&'static str, String)> {
    let enabled = native::bridge_enabled(config);
    // The same three-way `Supervisor::start_bridge` works through, in the same order, from the same
    // two predicates — so the report describes the real decision instead of paraphrasing it.
    let would_start = match (enabled, native::bridge_argv(&layout.root)) {
        (false, _) => "nothing — enable_mcp_server is false".to_string(),
        (true, None) => "nothing — on, but no windmcp binary was found in the directories above".to_string(),
        (true, Some(spawn)) => spawn.describe(),
    };
    let token = if config.str_or("mcp_server_token", "").trim().is_empty() {
        "not set (the value is never printed; while auth is required the bridge refuses to bind without one)"
            .to_string()
    } else {
        "set (value not printed)".to_string()
    };
    vec![
        ("enable_mcp_server", yes_no(enabled).to_string()),
        ("would start", would_start),
        ("running", yes_no(bridge_is_running(layout)).to_string()),
        ("mcp_server_host", config.str_or("mcp_server_host", "(unset — the bridge defaults to 127.0.0.1)")),
        ("mcp_server_port", config.str_or("mcp_server_port", "(unset — the bridge defaults to 21120)")),
        ("auth required", yes_no(config.bool_or("mcp_server_auth_required", true)).to_string()),
        ("mcp_server_token", token),
        ("logs", format!("{}, {}", path(&layout.bridge.out), path(&layout.bridge.err))),
        ("verdict", format!("run `windmcp doctor --root {}` for the client url and the bind verdict", path(&layout.root))),
    ]
}

/// One launch as a line: the command, or the file that is absent.
///
/// The absent case must not be rendered as an empty value or as "off", which is how it read before:
/// a launch that has nothing to run is a different state from a launch nobody asked for, and this is
/// the one place `doctor` says which install-level fact the user is out by.
fn describe_launch(outcome: &Result<Spawn, Missing>) -> String {
    match outcome {
        Ok(spawn) => spawn.describe(),
        Err(missing) => format!("{} — {}", missing.explain().lines().next().unwrap_or_default(), missing.title()),
    }
}

/// This running executable. Named in the report because it is now the answer to "where did that icon
/// come from" — the tray's two states are resources inside it, not files it goes looking for.
fn this_exe() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("this executable"))
}

fn describe(dir: &Path) -> String {
    format!("{}{}", dir.display(), exists_note(dir.is_dir()))
}

fn exists_note(present: bool) -> &'static str {
    if present { "" } else { "  [absent]" }
}

fn path(path: &Path) -> String {
    path.display().to_string()
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn line(label: &str, value: &str) {
    println!("{label:<18} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(std::path::PathBuf::from).unwrap()
    }

    /// The one thing a user does with this command is run it. A missing directory, an absent binary or
    /// a lock naming a dead pid are the states it exists to describe, so none of them may make it
    /// fail — and it must never touch anything.
    #[test]
    fn the_report_runs_against_the_real_install_and_changes_nothing() {
        let root = repo_root();
        let locks = root.join("cache").join("locks");
        let before = listing(&locks);
        report(&Options { root: root.clone() }).expect("a real install must always be diagnosable");
        assert_eq!(listing(&locks), before, "doctor must not create or delete a lock");
        assert!(!matches!(lock_state(&Layout::from_config(&Config::load(&root).unwrap()).record_lock), LockState::HeldBy { alive: true, .. }),
                "a test run must not be holding the record lock");
    }

    #[test]
    fn a_broken_root_is_reported_as_an_error_message_not_a_panic() {
        let options = Options { root: repo_root().join("no-such-directory") };
        let error = report(&options).expect_err("a root with no config cannot be diagnosed");
        assert!(error.contains("no-such-directory"), "{error}");
    }

    #[test]
    fn an_absent_binary_is_described_as_missing_with_the_search_order_visible() {
        let root = repo_root();
        let config = Config::load(&root).unwrap();
        let layout = Layout::from_config(&config);
        let found = native::find_binary("windrec", &layout.root);
        let label = native::describe_build(found.as_deref());
        assert!(["release", "debug", "installed", "missing"].contains(&label), "{label}");
        let command = describe_launch(&native::recorder_argv(&layout.root));
        assert!(command.contains("loop"), "{command}");
        assert!(
            !command.contains("python") && !command.contains("record_screen"),
            "the tray must not offer an interpreter for a file this branch deleted: {command}"
        );
        assert!(exists_note(false).contains("absent"));
        assert!(exists_note(true).is_empty());
    }

    /// A scratch install holding exactly these settings, so the bridge's report can be read for a
    /// configuration the shipped defaults are not.
    fn scratch(tag: &str, settings: &str) -> (Config, Layout) {
        let dir = std::env::temp_dir().join(format!("windsvc-doctor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), settings).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let config = Config::load(&dir).unwrap();
        let layout = Layout::from_config(&config);
        (config, layout)
    }

    /// What the report says about the updater question, for every install shape: this tray cannot
    /// update itself — say so, and name what does — and it points at a changelog only when one is
    /// actually on disk. The same predicate the menu builds its row from, so the two cannot diverge.
    #[test]
    fn the_report_answers_the_updater_question_honestly_and_names_a_real_changelog() {
        let (_config, layout) = scratch("updates", "{}");
        let value = |want: &str| update_state_lines(&layout).into_iter().find(|(label, _)| *label == want).map(|(_, v)| v).unwrap_or_default();
        assert!(value("updates").contains("no updater"), "{}", value("updates"));
        assert!(value("updates").contains("zip"), "the report must name the path that does replace the install: {}", value("updates"));
        assert!(value("changelog").starts_with("none"), "a scratch root has nothing to open: {}", value("changelog"));
        std::fs::write(&layout.changelog, b"# Changelog").unwrap();
        assert_eq!(value("changelog"), format!("{}  (opened by \"See what's new\")", path(&layout.changelog)));
        std::fs::write(&layout.release_notes, b"release notes").unwrap();
        assert_eq!(value("changelog"), format!("{}  (opened by \"See what's new\")", path(&layout.release_notes)), "the shipped notes outrank the checkout file here too");
        let _ = std::fs::remove_dir_all(&layout.root);
    }

    /// The three questions the help text implies and the tray previously answered nowhere.
    #[test]
    fn the_bridge_section_answers_enabled_running_and_where_to_look() {
        let (config, layout) = scratch("answers", r#"{"enable_mcp_server": true}"#);
        std::fs::write(layout.root.join("bin/windmcp.exe"), b"MZ").unwrap();
        let report = bridge_report(&config, &layout);
        let labels: Vec<&str> = report.iter().map(|(label, _)| *label).collect();
        for needed in ["enable_mcp_server", "would start", "running", "mcp_server_host", "logs"] {
            assert!(labels.contains(&needed), "{labels:?} is missing {needed}");
        }
        let value = |want: &str| report.iter().find(|(label, _)| *label == want).map(|(_, v)| v.clone()).unwrap_or_default();
        assert_eq!(value("enable_mcp_server"), "yes");
        assert_eq!(value("running"), "no", "nothing was started by this test");
        assert!(value("would start").contains("windmcp"), "{}", value("would start"));
        assert!(value("would start").contains("serve"), "{}", value("would start"));
        assert!(value("logs").contains("mcp.log") && value("logs").contains("mcp.err"), "{}", value("logs"));
        let _ = std::fs::remove_dir_all(&layout.root);
    }

    /// Off is the shipped answer, and the shipped defaults are the restrictive ones. A factory
    /// config that opened a port would open it for every install on the next upgrade sweep, which
    /// re-applies the defaults to the user's file.
    #[test]
    fn the_shipped_defaults_leave_the_bridge_off_loopback_protected_and_secretless() {
        let config = Config::load(&repo_root()).unwrap();
        assert!(!native::bridge_enabled(&config), "enable_mcp_server must ship false");
        assert_eq!(config.str_or("mcp_server_host", "?"), "127.0.0.1");
        assert_eq!(config.str_or("mcp_server_port", "?"), "21120");
        assert!(config.bool_or("mcp_server_auth_required", false), "auth must ship required");
        assert!(config.str_or("mcp_server_token", "unset").is_empty(), "no secret ships in a default file");
        let report = bridge_report(&config, &Layout::from_config(&config));
        let would_start = report.iter().find(|(label, _)| *label == "would start").unwrap();
        assert!(would_start.1.contains("nothing"), "{}", would_start.1);
    }

    /// Enabled with no binary is the state that used to be silent. It must say which key is on and
    /// that nothing was found, rather than print an empty command line.
    #[test]
    fn an_enabled_bridge_with_nothing_to_run_says_so_instead_of_going_quiet() {
        let (config, layout) = scratch("nobinary", r#"{"enable_mcp_server": true}"#);
        assert!(native::bridge_enabled(&config));
        assert!(native::bridge_argv(&layout.root).is_none());
        let report = bridge_report(&config, &layout);
        let would_start = report.iter().find(|(label, _)| *label == "would start").unwrap();
        assert!(would_start.1.contains("no windmcp binary"), "{}", would_start.1);
        let _ = std::fs::remove_dir_all(&layout.root);
    }

    /// The one thing a diagnostic may never do: print the secret. Asserted against the whole
    /// rendered section, not one line, because a future line is exactly how this would regress.
    #[test]
    fn the_report_names_the_token_key_and_never_its_value() {
        let secret = "a-token-that-is-long-enough-to-be-a-secret";
        let (config, layout) = scratch("secret", &format!(r#"{{"enable_mcp_server": true, "mcp_server_token": "{secret}"}}"#));
        let rendered = bridge_report(&config, &layout)
            .into_iter()
            .map(|(label, value)| format!("{label}: {value}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("mcp_server_token"), "{rendered}");
        assert!(!rendered.contains(secret), "the report leaked the token");
        assert!(rendered.contains("set (value not printed)"), "{rendered}");
        let _ = std::fs::remove_dir_all(&layout.root);
    }

    fn listing(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }
}
