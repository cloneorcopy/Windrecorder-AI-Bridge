//! `windsetup` — first-run setup, OCR engine discovery, the upgrade migration, and install health.
//!
//! Four commands, all taking `--root PATH`, all defaulting the root to the directory holding this
//! executable or — during development, running out of `target/debug` — the ancestor that actually has
//! `windrecorder/` in it. That resolution is shared with `windmaint` and `windcapctl` for a reason: a
//! user with the binaries staged at the install root and a developer running them from `target/debug`
//! must get the same answers from the same arguments, or the smoke test proves nothing about the payload.
//!
//! `--dry-run` is honoured by `migrate` and is a genuine no-op: the plan is computed and printed, and no
//! file is created, renamed or deleted anywhere. It does not need a lock and must not take one, because a
//! command that cannot run while the recorder is busy is a command nobody runs before a migration they
//! are already nervous about.
//!
//! # Console encoding
//!
//! Every string this binary prints goes through `engines::escape_non_ascii` unless it is pure ASCII. The
//! console on a zh-CN install is cp936, where a UTF-8 Chinese window title renders as plausible garbage
//! rather than as an error, and a report about which languages your OCR can read is exactly the document
//! that must not lie. `--json` writes raw UTF-8 so the real bytes can be redirected and read elsewhere.

use std::path::PathBuf;

use wind_base::config::Config;
use wind_base::version;

use wind_base::autostart;
use wind_setup::{configfile, doctor, engines, layout, migrate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Init,
    CheckEngines,
    Migrate,
    Doctor,
    Autostart,
}

impl Command {
    fn parse(text: &str) -> Option<Command> {
        match text {
            "init" => Some(Command::Init),
            "check-engines" => Some(Command::CheckEngines),
            "migrate" => Some(Command::Migrate),
            "doctor" => Some(Command::Doctor),
            "autostart" => Some(Command::Autostart),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Command::Init => "init",
            Command::CheckEngines => "check-engines",
            Command::Migrate => "migrate",
            Command::Doctor => "doctor",
            Command::Autostart => "autostart",
        }
    }

    /// Only `migrate` mutates the install's own files, and only `init` may create the tree.
    /// `autostart` mutates *elsewhere* — one value under HKCU, and only when asked to with
    /// `--enable`/`--disable` — which is why it can take `--dry-run` and still report honestly.
    fn mutates(self) -> bool {
        matches!(self, Command::Init | Command::Migrate | Command::Autostart)
    }
}

#[derive(Debug)]
struct Options {
    command: Command,
    root: PathBuf,
    dry_run: bool,
    json: bool,
    ascii: bool,
    from_version: Option<migrate::Release>,
    /// `--enable` / `--disable`, for `autostart`: neither asks a question, one of them answers it.
    autostart_enable: bool,
    autostart_disable: bool,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Ahead of `parse_options` and ahead of `Config::load`: `windsetup` is the binary a user runs
    // *because* their install is wrong, and asking it which build it is must not itself need a
    // root that exists, a config that parses or a migration marker that reads.
    if argv.first().map(String::as_str).is_some_and(version::is_flag) {
        println!("{}", version_line());
        return;
    }
    let options = match parse_options(&argv) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let config = match Config::load(&options.root) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let outcome = match options.command {
        Command::Init => init(&options, &config),
        // `check-engines` carries its own three-way verdict (0 usable / 1 unusable / 3 not testable),
        // so it exits the process from here rather than collapsing into the `Result` the other
        // commands share. The `!` from `process::exit` coerces to the arm's `Result` type.
        Command::CheckEngines => std::process::exit(check_engines(&options, &config)),
        Command::Migrate => migrate(&options, &config),
        Command::Doctor => doctor(&options, &config),
        Command::Autostart => report_autostart(&options),
    };
    if let Err(e) = outcome {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Create the writable layout and seed the user config — never the other way round.
fn init(options: &Options, config: &Config) -> Result<(), String> {
    layout::Layout::validate_root(&options.root)?;
    let tree = layout::Layout::resolve(config);
    let created = tree.create(options.dry_run)?;
    let suffix = if options.dry_run { " (dry run: nothing created)" } else { "" };
    println!("windsetup init{} — root {}", suffix, options.root.display());
    if created.is_empty() {
        println!("  layout already complete, {} director(ies) present", tree.slots.len());
    } else {
        for slot in &created {
            println!("  + {slot}");
        }
        println!("  {} of {} slot(s) created", created.len(), tree.slots.len());
    }

    // Seeding is deliberately after the layout: `Config::save` and `initialize_config` both create
    // `userdata/` themselves, and a seed that ran first would either fail or silently create a directory
    // the layout report then has to describe twice.
    let seeded = configfile::seed(config, options.dry_run)?;
    match seeded {
        configfile::Seeded::AlreadyPresent => println!(
            "  user config: kept as it is ({}); init never overwrites a file you may have spent years setting up",
            config.root().join(configfile::USER_RELPATH).display()
        ),
        other => {
            let (from, size, digest) = match other {
                configfile::Seeded::FromDefaults { size, sha256 } => ("the shipped defaults", size, sha256),
                configfile::Seeded::FromEmbeddedDefaults { size, sha256 } => ("the defaults compiled into this binary — no config_src/config_default.json was on disk", size, sha256),
                configfile::Seeded::FromLegacyFile { size, sha256 } => ("a pre-0.0.9 config/config_user.json", size, sha256),
                configfile::Seeded::AlreadyPresent => unreachable!("handled above"),
            };
            println!("  user config: seeded from {from}, {size} bytes, sha256 {digest}");
        }
    }

    let drift = configfile::drift(config).ok();
    if let Some(drift) = drift.filter(|d| !d.is_clean()) {
        println!(
            "  note: {} key(s) the defaults have that your config does not, and {} your config has that the defaults do not. `migrate` reconciles them, with a backup.",
            drift.missing_from_user.len(),
            drift.extra_in_user.len()
        );
    }
    Ok(())
}

/// The engine report. This is the command to run when the index is empty.
///
/// Returns the process exit code directly rather than a `Result`, because the report has three
/// outcomes and they need three different codes — see [`engines::Verdict`] and the exit-status block
/// in [`usage`]. `Ok`/`Err` could only ever say "fine" or "broken", and "we could not run the check"
/// is neither.
fn check_engines(options: &Options, config: &Config) -> i32 {
    let report = engines::probe(config);
    // A resident engine has to be put down before this process returns, or its channel thread is still
    // running while the C runtime tears the process down — which reads as a crash on exit in a tool whose
    // whole job is to report cleanly.
    wind_base::wxocr::shutdown();
    if options.json {
        // The verdict is in the JSON (`outcome`, `testable`, `exit_code`) *and* in the exit code, so
        // a script gets the same answer whichever channel it reads.
        println!("{}", engines::to_json(&report));
        return report.exit_code();
    }
    let render = |text: &str| {
        if options.ascii {
            engines::escape_non_ascii(text)
        } else {
            text.to_string()
        }
    };
    println!("windsetup check-engines — root {}", options.root.display());
    println!("configured: ocr_engine={} ocr_lang={}", render(&report.configured_engine), render(&report.configured_language));
    println!();
    println!("{:<24} {:<24} {:<28} {}", "ENGINE", "LANGUAGE", "STATUS", "ACCURACY  TIME");
    for probe in &report.probes {
        let score = match probe.accuracy {
            Some(value) => format!("{value:>6.1}%"),
            None => "      -".to_string(),
        };
        let took = match probe.elapsed_ms {
            Some(ms) => format!("{ms} ms"),
            None => "-".to_string(),
        };
        println!(
            "{:<24} {:<24} {:<28} {:<9} {}",
            render(&probe.engine),
            render(&probe.language),
            render(probe.status.label()),
            score,
            took
        );
        if !probe.detail.is_empty() {
            println!("{:>50} {}", "", render(&probe.detail));
        }
        // The text the engine actually read, in the same escaping mode as everything else. A user
        // deciding whether their OCR works needs to see output, not only a percentage -- and under
        // `--ascii` they see `文` rather than a cp936 console's guess at what the character was.
        if let Some(sample) = &probe.sample {
            println!("{:>50} {}", "", render(&format!("read: {sample}")));
        }
    }
    println!();
    match report.verdict() {
        engines::Verdict::Usable => {
            println!("RESULT: usable (exit 0). At least one engine read a fixture above the {:.0}% overlap threshold.", engines::ACCURACY_THRESHOLD);
            0
        }
        engines::Verdict::Unusable => {
            println!("RESULT: NO ENGINE PRODUCED USABLE TEXT (exit 1). The fixtures were present and an installed");
            println!("engine was run against them, and none produced text above the {:.0}% overlap threshold, so", engines::ACCURACY_THRESHOLD);
            println!("the index cannot fill. See the rows above: a language pack has to be installed in Windows");
            println!("itself, and `third_party_engine_ocr_lang` has to name a code the engine you selected can load.");
            1
        }
        engines::Verdict::Untestable => {
            // Phrased so it cannot be misread as an engine failure by a person scanning the last line
            // or by a script branching on the exit code: this is "the check did not run", which is
            // neither the exit-0 "it is fine" nor the exit-1 "it is broken".
            println!("RESULT: NOT TESTED — no OCR fixtures (exit 3). THIS IS NOT A VERDICT ON YOUR OCR ENGINE.");
            println!("This install has no __assets__/OCR_test_1080_* image/word-list pairs, so the check never");
            println!("ran any engine and cannot say whether one is usable. The rows above mark the engine '{}'", engines::Status::Untested.label());
            println!("— that is 'not tested', not 'failed'.");
            println!("To get a real answer: the fixtures ship under __assets__/ in the release payload; on a");
            println!("standalone install that was assembled by hand, copy them in and re-run. Exit 3 means");
            println!("inconclusive — a passing engine exits 0 and an engine that genuinely failed exits 1.");
            3
        }
    }
}

/// The migration.
fn migrate(options: &Options, config: &Config) -> Result<(), String> {
    let stamp = migrate::stamp_now();
    let plan_options = migrate::Options {
        config,
        dry_run: options.dry_run,
        from_version: options.from_version,
        stamp: stamp.clone(),
    };
    let report = migrate::run(&plan_options)?;
    if options.json {
        println!("{}", migrate::to_json(&report));
        return Ok(());
    }
    println!(
        "windsetup migrate{} — root {}",
        if options.dry_run { " --dry-run (nothing created, renamed or deleted)" } else { "" },
        options.root.display()
    );
    if let Some(from) = &options.from_version {
        println!("  --from-version {from}: steps introduced at or before it are treated as already applied");
    }
    println!("  run stamp {stamp} — every backup and moved file this run makes is filed under it");
    for (step, result) in &report.steps {
        let header = match (result.actions.is_empty(), result.blocked.is_empty()) {
            (true, true) => "nothing to do",
            _ => "work",
        };
        println!("\n== {} [{}] — {}", step.id, header, step.title);
        for action in &result.actions {
            println!("   - {}", action);
        }
        for note in &result.notes {
            println!("   # {note}");
        }
        for blocked in &result.blocked {
            println!("   ! BLOCKED: {blocked}");
        }
        if !result.blocked.is_empty() {
            println!("   (this step is not recorded as finished, so the next run offers it again)");
        }
    }
    let changed = report.changed();
    println!();
    match (options.dry_run, changed) {
        (true, _) => println!(
            "dry run: {} action(s) planned, {} blocker(s); nothing was written",
            report.steps.iter().map(|(_, r)| r.actions.len()).sum::<usize>(),
            report.blockers().len()
        ),
        (false, true) => println!(
            "applied {} action(s); marker {}",
            report.steps.iter().map(|(_, r)| r.actions.len()).sum::<usize>(),
            report.marker.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "not written".to_string())
        ),
        (false, false) => println!("nothing to do: every step is already satisfied, and the marker was not rewritten"),
    }
    let blockers = report.blockers();
    if !blockers.is_empty() {
        return Err(format!("{} step(s) are blocked and were left for a human: {}", blockers.len(), blockers.iter().map(|(s, b)| format!("{s}: {b}")).collect::<Vec<_>>().join(" | ")));
    }
    Ok(())
}

/// The health report.
fn doctor(options: &Options, config: &Config) -> Result<(), String> {
    let report = doctor::inspect(config)?;
    if options.json {
        println!("{}", doctor::to_json(&report));
        return Ok(());
    }
    let text = doctor::render(&report);
    if options.ascii {
        print!("{}", engines::escape_non_ascii(&text));
    } else {
        print!("{text}");
    }
    Ok(())
}

/// What starts this install when the user signs in — and, when told to, make it so or undo it.
///
/// The settings page has written this entry since the switch existed, through
/// `wind_base::autostart::apply`. This is the other half of a control being real: a way to ask the
/// machine what it currently says, in words, without opening a registry editor. The two share the one
/// function that reads HKCU, so this can never disagree with the checkbox it is describing.
fn report_autostart(options: &Options) -> Result<(), String> {
    let (enable, disable) = (options.autostart_enable, options.autostart_disable);
    if enable && disable {
        return Err("--enable and --disable are opposites; pick one".to_string());
    }
    let want = if enable {
        Some(true)
    } else if disable {
        Some(false)
    } else {
        None
    };
    println!("windsetup autostart — root {}", options.root.display());
    println!("  value: HKCU\\{}\\{}", RUN_KEY_DISPLAY, autostart::VALUE_NAME);

    let before = autostart::current();
    print_entry(&before);

    let Some(want) = want else {
        println!("  (reporting only: pass --enable or --disable to change it)");
        return Ok(());
    };

    if options.dry_run {
        println!(
            "  dry run: would {} the entry to {}",
            if want { "set" } else { "remove" },
            match autostart::target_exe(&options.root, &std::env::current_exe().unwrap_or_default()) {
                Ok(exe) => autostart::command_for(&exe),
                Err(why) => format!("(and could not: {why})"),
            }
        );
        return Ok(());
    }

    match autostart::apply(&options.root, want) {
        autostart::Outcome::Unchanged => println!("  unchanged: the registry already said what was asked"),
        autostart::Outcome::Changed(what) => println!("  applied: {what}"),
        autostart::Outcome::Failed(why) => return Err(why),
    }
    println!("  now:");
    print_entry(&autostart::current());
    Ok(())
}

/// The three answers [`autostart::current`] can give, in the words a user reads.
fn print_entry(entry: &autostart::Entry) {
    use wind_base::autostart::Entry;
    match entry {
        Entry::None => println!("    not registered: this install does not start at sign-in"),
        Entry::Registered(command) => println!("    registered: {command}"),
        Entry::Unreadable(why) => println!("    unreadable: {why}"),
    }
}

/// The registry key as a human reads it. `autostart::RUN_KEY` is private to the crate that owns the
/// FFI, and this is display text, not a path anybody opens — so it is spelled here rather than widened
/// into that module's interface for one println.
const RUN_KEY_DISPLAY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The run stamp, shared by every backup and trash folder one invocation creates.
fn run_stamp() -> String {
    migrate::stamp_now()
}

fn parse_options(argv: &[String]) -> Result<Options, String> {
    let first = argv.first().ok_or_else(|| usage(None))?;
    let command = Command::parse(first).ok_or_else(|| usage(Some(&format!("unknown command '{first}'"))))?;
    let mut root = default_root();
    let mut dry_run = false;
    let mut json = false;
    let mut ascii = false;
    let mut from_version = None;
    let mut autostart_enable = false;
    let mut autostart_disable = false;
    let mut i = 1;
    while i < argv.len() {
        let (key, inline) = match argv[i].split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (argv[i].as_str(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            argv.get(i).cloned().ok_or_else(|| format!("{what} needs a value"))
        };
        match key {
            "--root" => root = PathBuf::from(value("--root")?),
            "--dry-run" => {
                if !command.mutates() {
                    return Err(format!("--dry-run means nothing to {}; it never writes", command.name()));
                }
                dry_run = true;
            }
            "--json" => json = true,
            "--ascii" => ascii = true,
            "--from-version" => {
                let text = value("--from-version")?;
                from_version = Some(migrate::Release::parse(&text).ok_or_else(|| format!("--from-version {text:?} is not a version like 0.0.12"))?);
                if command != Command::Migrate {
                    return Err(format!("--from-version belongs to migrate, not {}", command.name()));
                }
            }
            "--enable" => {
                if command != Command::Autostart {
                    return Err(format!("--enable belongs to autostart, not {}", command.name()));
                }
                if inline.is_some() {
                    return Err("--enable takes no value; it is a switch".to_string());
                }
                autostart_enable = true;
            }
            "--disable" => {
                if command != Command::Autostart {
                    return Err(format!("--disable belongs to autostart, not {}", command.name()));
                }
                if inline.is_some() {
                    return Err("--disable takes no value; it is a switch".to_string());
                }
                autostart_disable = true;
            }
            "--help" | "-h" => return Err(usage(None)),
            other => return Err(format!("unexpected argument '{other}' for {}", command.name())),
        }
        i += 1;
    }
    // `init` is the command that may create the root; every other one is pointing at an install that has
    // to exist, so a typo reports "no such directory" instead of an empty, successful-looking report.
    if !root.exists() && command != Command::Init {
        return Err(format!("--root {} does not exist", root.display()));
    }
    // A relative `--root` names the same install as that path written out in full, and `doctor`,
    // `check-engines` and `migrate` all accepted `.` while `init` refused it: `pathguard::confine`
    // compares paths lexically and folds `.` away, so `.\userdata` never looked like it was inside `.`,
    // and the command whose whole job is to lay out a tree told the user their own directory was outside
    // the install. Making it absolute once, here, is what lets every consumer — the layout, the config
    // load, the confinement checks and the paths a report prints — agree on one root.
    let root = std::path::absolute(&root)
        .map_err(|e| format!("--root {} could not be made absolute: {e}", root.display()))?;
    if json && ascii {
        return Err("--json already emits UTF-8; --ascii would only make it unreadable".to_string());
    }
    Ok(Options {
        command,
        root,
        dry_run,
        json,
        ascii,
        from_version,
        autostart_enable,
        autostart_disable,
    })
}

/// The install root: the directory carrying this install's shipped settings, found by walking up
/// from this executable.
///
/// This is the resolution that matters most, because `init` is the command that decides where an
/// install *begins*: a rule here that disagreed with `windrec`'s would lay a layout down in one
/// directory and record into another. Both ask [`wind_base::install`], which is the only place the
/// rule is written.
fn default_root() -> PathBuf {
    wind_base::install::resolve_root_from_exe(None)
}

fn usage(note: Option<&str>) -> String {
    let mut text = match note {
        Some(note) => format!("error: {note}\n\n"),
        None => String::new(),
    };
    text.push_str(
        "usage: windsetup <command> [--root PATH] [--ascii] [--json]\n\
         \n\
         \x20 init           [--root PATH] [--dry-run]   create the writable layout and seed\n\
         \x20                                           userdata/config_user.json, only if absent\n\
         \x20 check-engines  [--root PATH] [--json]      probe every OCR engine and language this\n\
         \x20                                           machine can actually run, against the\n\
         \x20                                           __assets__/ fixtures (this is the command to\n\
         \x20                                           run when your index is empty)\n\
         \x20 migrate        [--root PATH] [--dry-run]   the upgrade steps over an existing install:\n\
         \x20                    [--from-version V]       the 0.0.9 userdata/ split, the 0.0.12 error\n\
         \x20                                           tag, the index columns, the config keys\n\
         \x20 doctor         [--root PATH] [--json]      the state of the install: config layer, month\n\
         \x20                                           files and their columns, row counts, locks,\n\
         \x20                                           and what migrate would change\n\
         \x20 autostart      [--root PATH]             what starts this install when you sign in,\n\
         \x20                    [--enable|--disable]   read as the HKCU Run entry in words, or\n\
         \x20                                           change it (--dry-run shows what would\n\
         \x20                                           happen). The settings page's box writes\n\
         \x20                                           the same single value.\n\
         \x20 --version | -V                            this binary's name, its package version and\n\
         \x20                                           its build profile; reads no config and no\n\
         \x20                                           install root, so it answers on a broken one\n\
         \n\
         Every file this program is about to change is copied to userdata/backup/<stamp>/ first and the\n\
         copy is verified by size and SHA-256 before the change is made; userdata/backup/MANIFEST.jsonl\n\
         records each one. Nothing is ever deleted: what upstream deletes is moved to userdata/trash/.\n\
         --dry-run creates, renames and deletes nothing at all.\n\
         \n\
         --root defaults to the directory containing this executable.\n\
         --ascii escapes non-ASCII text for a cp936 console.\n\
         \n\
         check-engines keeps three outcomes apart and gives each its own exit status, so a script and\n\
         a human cannot read one as another:\n\
         \x20 0  usable        an installed engine read a fixture above the accuracy threshold\n\
         \x20 1  unusable      fixtures were present, an engine was run, and none produced usable text\n\
         \x20 3  not testable  no __assets__/OCR_test_1080_* fixture pairs, so no engine was ever run --\n\
         \x20                  NOT a verdict on the engine, and deliberately neither 0 (fine) nor 1 (bad)\n\
         For every command 2 means a bad argument and 1 a hard failure.",
    );
    text
}

/// What `windsetup --version` prints. The format is `wind_base::version`'s, shared by all eleven
/// binaries; the name and the `env!` make it this crate's answer.
fn version_line() -> String {
    version::line("windsetup", env!("CARGO_PKG_VERSION"))
}

/// The `migrate` run stamp, resolved once so every path in one report shares it.
pub fn stamp_now() -> String {
    run_stamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests reach for these two: `marker` is the file `migrate` writes and `doctor` reads, and
    // `pathguard` is the check a run stamp has to survive.
    use std::path::Path;
    use wind_base::clock;
    use wind_setup::{marker, pathguard};

    fn parse(args: &str) -> Result<Options, String> {
        parse_options(&args.split(' ').filter(|a| !a.is_empty()).map(str::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn every_command_is_reachable() {
        for (name, command) in [("init", Command::Init), ("check-engines", Command::CheckEngines), ("migrate", Command::Migrate), ("doctor", Command::Doctor)] {
            let options = parse(&format!("{name} --root .")).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(options.command, command);
            assert_eq!(command.name(), name);
        }
        assert!(parse("frobnicate --root .").is_err());
        assert!(parse("").is_err(), "no arguments is a usage message, not a run");
    }

    #[test]
    fn only_the_two_writer_commands_accept_a_dry_run() {
        assert!(parse("migrate --root . --dry-run").is_ok());
        assert!(parse("init --root . --dry-run").is_ok());
        assert!(parse("doctor --root . --dry-run").is_err(), "a read-only command cannot be told not to read");
        assert!(parse("check-engines --root . --dry-run").is_err());
    }

    #[test]
    fn from_version_is_parsed_and_belonged_to_migrate() {
        let options = parse("migrate --root . --from-version=0.0.12").unwrap();
        assert_eq!(options.from_version.map(|r| r.parts), Some([0, 0, 12]));
        assert!(parse("migrate --root . --from-version nonsense").is_err());
        assert!(parse("migrate --root . --from-version").is_err(), "a flag with no value");
        assert!(parse("doctor --root . --from-version 0.0.12").is_err());
    }

    #[test]
    fn a_missing_root_is_an_error_except_when_the_job_is_to_create_it() {
        let missing = "Z:/no/such/install/anywhere";
        assert!(parse(&format!("doctor --root {missing}")).is_err());
        assert!(parse(&format!("init --root {missing}")).is_ok(), "init is how a tree comes into existence");
    }

    /// `--root .` is the first thing in the install notes, and `init` used to answer it with
    /// ".\userdata resolves outside the install root ." while its three siblings accepted the same
    /// argument. The root the whole command then acts on has to be one absolute path.
    #[test]
    fn a_relative_root_becomes_the_absolute_path_it_names() {
        let cwd = std::env::current_dir().expect("a test runs somewhere");
        let absolute = |p: &std::path::Path| std::path::absolute(p).expect("absolute on a test path");
        for args in ["init --root .", "doctor --root .", "migrate --root .", "check-engines --root ."] {
            let options = parse(args).unwrap_or_else(|e| panic!("{args}: {e}"));
            assert!(options.root.is_absolute(), "{args} left {:?} relative", options.root);
            assert_eq!(options.root, absolute(&cwd), "{args}");
        }
        // `init` may point at a directory that is not there yet, and a relative one is the same case.
        let options = parse("init --root not-yet-laid-out").expect("init creates the root");
        assert_eq!(options.root, absolute(&cwd.join("not-yet-laid-out")));
    }

    #[test]
    fn the_two_output_modes_do_not_contradict_each_other() {
        assert!(parse("check-engines --root . --json --ascii").is_err());
        assert!(parse("doctor --root . --json").is_ok());
    }

    #[test]
    fn usage_names_every_command_and_the_backup_contract() {
        let text = usage(Some("unknown command 'frobnicate'"));
        for needle in ["init", "check-engines", "migrate", "doctor", "autostart", "--enable", "--disable", "--dry-run", "--from-version", "--ascii", "userdata/backup", "MANIFEST.jsonl", "userdata/trash", "frobnicate"] {
            assert!(text.contains(needle), "{needle} missing from the help:\n{text}");
        }
        assert!(!usage(None).contains("error:"), "plain help must not look like a failure");
    }

    /// The version is answered in `main`, ahead of `parse_options` — which is the function that
    /// refuses a root that does not exist — so it is reachable from exactly the install this
    /// binary is otherwise unable to talk about.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windsetup "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        for spelling in ["--version", "-V"] {
            assert!(version::is_flag(spelling), "{spelling}");
            // Proof the answer cannot come from this path: the parser rejects the flag outright,
            // which is why `main` has to read it first.
            assert!(parse(&format!("{spelling}")).is_err(), "{spelling} must not reach a command");
        }
        assert!(usage(None).contains("--version"), "answered but undocumented:\n{}", usage(None));
    }

    #[test]
    fn the_default_root_walks_up_out_of_the_build_directory() {
        // This test binary lives in `target/debug`, so the walk has to find the install root above it.
        let root = default_root();
        assert!(root.join("windrecorder").is_dir() || root.join("windcap").is_dir(), "{root:?}");
    }

    #[test]
    fn a_run_stamp_is_a_path_component() {
        let stamp = stamp_now();
        assert_eq!(stamp.len(), 19, "{stamp}");
        assert!(pathguard::check_component(&format!("backup-{stamp}")).is_ok(), "{stamp}");
        let parsed = clock::LocalParts::from_stamp(&stamp).expect("the stamp round-trips");
        assert_eq!(parsed.stamp(), stamp);
    }

    #[test]
    fn marker_is_reachable_from_the_binary_surface() {
        // `migrate` writes the file `doctor` reads; both are named from here so a rename breaks a test.
        assert!(marker::marker_path(&Config::load(Path::new(".")).unwrap()).ends_with("upgrade_marker.json"));
    }
}
