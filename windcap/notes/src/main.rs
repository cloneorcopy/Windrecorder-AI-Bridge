//! `windnotes` — the flag table from a terminal.
//!
//! Four commands, mirroring the four things a user does with a bookmark: look at them, make one,
//! delete one, and see where the day view would draw them. The last is here for a reason beyond
//! convenience: marker geometry is the part of this subsystem a log line cannot show, and being able
//! to print `ratio` and `x` for a known instant is how a proportionality bug gets caught before it
//! puts a bookmark on the wrong hour.
//!
//! `--dry-run` on anything that writes: the row is computed and printed, and the file is left alone.
//! A dry run of `add` still grabs the screen, because the grab *is* the plan.

use std::path::PathBuf;

use wind_base::clock::{self, LocalParts};
use wind_base::config::Config;
use wind_base::version;
use wind_notes::capture::{self, CreateOutcome, FlagSource};
use wind_notes::flag;
use wind_notes::markers::{self, DaySpan};
use wind_notes::store::{Entry, FlagStore, SaveOutcome};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Ahead of the help words below and ahead of `Command::parse`, and so ahead of `Config::load`:
    // `--version` is the one thing this binary answers without an install root and without opening
    // `userdata/flag_mark_note.csv`. The user whose flag table is the broken thing still needs to
    // be able to say which `windnotes` they are holding.
    if argv.first().map(String::as_str).is_some_and(version::is_flag) {
        println!("{}", version_line());
        return;
    }
    if argv.is_empty() || matches!(argv[0].as_str(), "-h" | "--help" | "help") {
        println!("{}", usage());
        std::process::exit(if argv.is_empty() { 2 } else { 0 });
    }
    let command = match Command::parse(&argv) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let config = match Config::load(&command.root()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = dispatch(&command, &config) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// `list [--root PATH] [--day YYYY-MM-DD]`
    List { root: PathBuf, day: Option<LocalParts> },
    /// `add [--root PATH] [--note TEXT] [--at 'YYYY-MM-DD HH:MM:SS'] [--dry-run]`
    Add { root: PathBuf, note: Option<String>, at: Option<LocalParts>, dry_run: bool },
    /// `remove <row|'YYYY-MM-DD HH:MM:SS'> [--root PATH] [--dry-run]`
    Remove { root: PathBuf, target: Target, dry_run: bool },
    /// `markers --day YYYY-MM-DD --width N [--height N] [--from T] [--to T]`
    Markers { root: PathBuf, day: LocalParts, width: u32, height: u32, span: Option<DaySpan> },
}

/// What `remove` was pointed at. A row position and an instant are never ambiguous: the first is a
/// bare integer, the second always longer than eight characters.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Target {
    Index(usize),
    At(LocalParts),
}

impl Command {
    fn root(&self) -> PathBuf {
        match self {
            Command::List { root, .. }
            | Command::Add { root, .. }
            | Command::Remove { root, .. }
            | Command::Markers { root, .. } => root.clone(),
        }
    }
}

fn dispatch(command: &Command, config: &Config) -> Result<(), String> {
    match command {
        Command::List { day, .. } => list(config, *day),
        Command::Add { note, at, dry_run, .. } => add(config, note.as_deref(), *at, *dry_run),
        Command::Remove { target, dry_run, .. } => remove(config, *target, *dry_run),
        Command::Markers { day, width, height, span, .. } => draw_markers(config, *day, *span, *width, *height),
    }
}

fn list(config: &Config, day: Option<LocalParts>) -> Result<(), String> {
    let store = FlagStore::load_for(config).map_err(|e| e.to_string())?;
    println!("{}", store.path().display());
    if !store.path().exists() {
        println!("  no flag table yet: `windnotes add` makes one");
        return Ok(());
    }
    let rows: Vec<(usize, &Entry)> = match day {
        None => store.entries().iter().enumerate().collect(),
        Some(day) => {
            let span = DaySpan::product_day(day, config.day_begin_minutes());
            store.on_day(span).into_iter().filter_map(|(index, _)| store.get(index).map(|entry| (index, entry))).collect()
        }
    };
    println!("{:<5} {:<20} {:<11} note", "row", "datetime", "thumbnail");
    if rows.is_empty() {
        println!("  (no rows{})", if day.is_some() { " that day" } else { "" });
    }
    for (index, entry) in &rows {
        // An unrecognised row is listed as it stands, because the alternative — not showing a row
        // the user can see in a text editor — reads as data loss.
        let tail = if entry.flag().is_none() { "   <- not understood here, written back unchanged" } else { "" };
        println!("{index:<5} {:<20} {:<11} {}{tail}", entry.datetime_text(), thumbnail_label(entry), one_line(entry.note()));
    }
    println!("{} row(s), {} with a thumbnail", rows.len(), rows.iter().filter(|(_, entry)| !entry.thumbnail().is_empty()).count());
    Ok(())
}

/// What the picture in a row actually is, decided by reading its header rather than by trusting the
/// column: upstream's own history leaves PNG-in-base64 rows next to JPEG ones.
fn thumbnail_label(entry: &Entry) -> String {
    if entry.thumbnail().is_empty() {
        return "none".to_string();
    }
    match capture::thumbnail_size(entry.thumbnail()) {
        Some((width, height)) => format!("{width}x{height} jpeg"),
        None => "unreadable".to_string(),
    }
}

fn one_line(text: &str) -> String {
    text.replace('\n', "\\n")
}

fn add(config: &Config, note: Option<&str>, at: Option<LocalParts>, dry_run: bool) -> Result<(), String> {
    // An instant that has already passed cannot be grabbed, so naming one means "take the frame the
    // index already holds for it" — the day view's flag button, from the terminal.
    let outcome = match at {
        None => capture::create_now(config, note, clock::now(), dry_run)?,
        Some(at) => {
            let note = note.map(str::to_string).or_else(|| capture::prefill_note(config, at));
            capture::create_from_history(config, at, note.as_deref(), dry_run)?
        }
    };
    report_created(&outcome, dry_run);
    Ok(())
}

fn report_created(outcome: &CreateOutcome, dry_run: bool) {
    println!("{}", outcome.flag.displayed_datetime());
    println!("  note      : {}", one_line(&outcome.flag.note));
    println!("  source    : {}", outcome.source.label());
    println!(
        "  thumbnail : {}",
        match outcome.thumbnail_size {
            Some((width, height)) => format!("{width}x{height} jpeg, {} base64 chars", outcome.flag.thumbnail.len()),
            None if outcome.flag.thumbnail.is_empty() => "none".to_string(),
            None => "not a readable JPEG".to_string(),
        }
    );
    println!("  row       : {}", outcome.index + 1);
    println!("  {}{}", outcome.path.display(), if dry_run { "  (dry run: nothing written)" } else { "  (appended)" });
    if outcome.source == FlagSource::Nothing {
        println!("  nothing was recorded at that instant, so this flag carries no picture");
    }
}

fn remove(config: &Config, target: Target, dry_run: bool) -> Result<(), String> {
    let mut store = FlagStore::load_for(config).map_err(|e| e.to_string())?;
    let removed: Vec<Entry> = match target {
        Target::Index(index) => match store.remove_index(index) {
            Some(entry) => vec![entry],
            None => return Err(format!("there is no row {index}; the table holds {} row(s)", store.len())),
        },
        Target::At(when) => {
            let indices: Vec<usize> = store.flags().filter(|(_, flag)| flag.when == when).map(|(index, _)| index).collect();
            if indices.is_empty() {
                return Err(format!("no flag at {}; `windnotes list` shows the row numbers", when.display()));
            }
            let entries = indices.iter().filter_map(|index| store.get(*index)).cloned().collect();
            store.remove_at(when);
            entries
        }
    };
    for entry in &removed {
        println!("removing  {}  {}", entry.datetime_text(), one_line(entry.note()));
    }
    let outcome = store.save(dry_run).map_err(|e| e.to_string())?;
    let report = match (outcome, dry_run) {
        (SaveOutcome::Written, true) => format!("would rewrite {} to {} row(s)", store.path().display(), store.len()),
        (SaveOutcome::Written, false) => format!("rewrote {} to {} row(s)", store.path().display(), store.len()),
        (SaveOutcome::Unchanged, _) => format!("{} unchanged", store.path().display()),
    };
    // The sentence upstream's behaviour would deserve: it is possible to empty this table from
    // here, and it is not possible to delete it from here.
    println!("{report}{}", if store.is_empty() && !removed.is_empty() { ", header kept, file left in place" } else { "" });
    Ok(())
}

fn draw_markers(config: &Config, day: LocalParts, explicit: Option<DaySpan>, width: u32, height: u32) -> Result<(), String> {
    let store = FlagStore::load_for(config).map_err(|e| e.to_string())?;
    let span = explicit.unwrap_or_else(|| DaySpan::product_day(day, config.day_begin_minutes()));
    let on_day = store.on_day(span);
    let times: Vec<i64> = on_day.iter().map(|(_, flag)| flag.epoch()).collect();
    let plan = markers::layout(span, &times, width, height);

    println!(
        "day {} | span {} .. {} ({} s, {}) | strip {}x{} | {} flag(s)",
        day.date_stamp(),
        LocalParts::from_naive_epoch(span.from).display(),
        LocalParts::from_naive_epoch(span.to).display(),
        span.seconds(),
        if span.is_measurable() { "measurable" } else { "degenerate: nothing can be proportional to it" },
        width,
        height,
        on_day.len(),
    );
    if on_day.is_empty() {
        println!("  no flag that day");
        return Ok(());
    }
    println!("{:<5} {:<20} {:>7} {:>5}  {:<13} {}", "row", "datetime", "ratio", "x", "bar", "banner: pole-top, apex, pole-middle");
    for marker in &plan.markers {
        let (index, flag) = on_day[marker.index];
        println!(
            "{index:<5} {:<20} {:>7.4} {:>5}  {:>5}..{:<5} ({:>4},{}) ({:>4},{}) ({:>4},{}){}",
            flag.stored_datetime(),
            marker.ratio,
            marker.x,
            marker.bar.x,
            marker.bar.right(),
            marker.triangle[0].x,
            marker.triangle[0].y,
            marker.triangle[1].x,
            marker.triangle[1].y,
            marker.triangle[2].x,
            marker.triangle[2].y,
            if marker.clipped { "  clipped at the right edge" } else { "" },
        );
    }
    for (position, time) in &plan.outside {
        let (index, flag) = on_day[*position];
        debug_assert_eq!(flag.epoch(), *time);
        println!("{index:<5} {:<20} {:>7} {:>5}  not drawn: outside the span", flag.stored_datetime(), "-", "-");
    }
    Ok(())
}

fn usage() -> String {
    "\
usage: windnotes <command> [--root PATH] [...]

  list     [--root PATH] [--day YYYY-MM-DD]
  add      [--root PATH] [--note TEXT] [--at 'YYYY-MM-DD HH:MM:SS'] [--dry-run]
  remove   <row|'YYYY-MM-DD HH:MM:SS'> [--root PATH] [--dry-run]
  markers  --day YYYY-MM-DD --width N [--height N] [--from T] [--to T] [--root PATH]
  --version | -V
             print this binary's name, its package version and its build profile; reads no
             config, no table and no install root, so it answers when nothing else can

--root defaults to the install directory containing this executable; the table is
`<root>/userdata/flag_mark_note.csv` either way.

add grabs the screen. With --at it flags an instant that has already passed instead, copying the
frame the index captured then and using its window title as the note.

markers prints where the day view draws each bookmark: `ratio` is the instant's place along the
span, `x` the pixel column of its bar. --from/--to replace the span with the day's recorded range,
which is what the strip is built from upstream.

remove rewrites the table with the surviving rows. It never removes the file, not even when the
last row goes: upstream sent the whole CSV to the trash when every row was selected."
        .to_string()
}

/// What `windnotes --version` prints. `wind_base::version` holds the format so that the eleven
/// binaries in one zip cannot describe themselves eleven ways; the name and the `env!` are this one's.
fn version_line() -> String {
    version::line("windnotes", env!("CARGO_PKG_VERSION"))
}

impl Command {
    fn parse(argv: &[String]) -> Result<Command, String> {
        let first = argv.first().ok_or_else(usage)?;
        match first.as_str() {
            "list" => {
                let args = Args::parse(&argv[1..], &["--root", "--day"], &[])?;
                args.no_positionals()?;
                Ok(Command::List { root: args.root()?, day: args.day()? })
            }
            "add" => {
                let args = Args::parse(&argv[1..], &["--root", "--note", "--at"], &["--dry-run"])?;
                args.no_positionals()?;
                Ok(Command::Add {
                    root: args.root()?,
                    note: args.value("--note").map(str::to_string),
                    at: args.instant("--at")?,
                    dry_run: args.has("--dry-run"),
                })
            }
            "remove" => {
                let args = Args::parse(&argv[1..], &["--root"], &["--dry-run"])?;
                let target = match args.positionals.len() {
                    0 => return Err(format!("remove needs a row number or a datetime\n\n{}", usage())),
                    1 => Target::parse(&args.positionals[0])?,
                    n => return Err(format!("remove takes one target, given {n}")),
                };
                Ok(Command::Remove { root: args.root()?, target, dry_run: args.has("--dry-run") })
            }
            "markers" => {
                let args = Args::parse(&argv[1..], &["--root", "--day", "--width", "--height", "--from", "--to"], &[])?;
                args.no_positionals()?;
                let day = args.day()?.ok_or_else(|| "markers needs --day YYYY-MM-DD".to_string())?;
                let width: u32 = args
                    .value("--width")
                    .ok_or_else(|| "markers needs --width N".to_string())?
                    .parse()
                    .map_err(|e| format!("--width: {e}"))?;
                if width == 0 {
                    return Err("--width must be at least 1".to_string());
                }
                // The strip's height sets the banner's size, so it is the caller's to name; the
                // default is the thumbnail scale the index uses, as good a guess as any for a
                // terminal check.
                let height: u32 = match args.value("--height") {
                    Some(text) => text.parse().map_err(|e| format!("--height: {e}"))?,
                    None => 70,
                };
                if height == 0 {
                    return Err("--height must be at least 1".to_string());
                }
                let span = match (args.instant("--from")?, args.instant("--to")?) {
                    (Some(from), Some(to)) => Some(DaySpan::new(from.naive_epoch_seconds(), to.naive_epoch_seconds())),
                    (None, None) => None,
                    _ => return Err("--from and --to are given together, as one span".to_string()),
                };
                Ok(Command::Markers { root: args.root()?, day, width, height, span })
            }
            other => Err(format!("unknown command '{other}'\n\n{}", usage())),
        }
    }
}

impl Target {
    fn parse(text: &str) -> Result<Target, String> {
        if let Ok(index) = text.trim().parse::<usize>() {
            return Ok(Target::Index(index));
        }
        flag::parse_datetime(text)
            .map(Target::At)
            .ok_or_else(|| format!("'{text}' is neither a row number nor a datetime ('YYYY-MM-DD HH:MM:SS')"))
    }
}

/// The smallest argument reader that takes both `--flag value` and `--flag=value`, the shape the
/// other native binaries use, so one syntax covers the whole toolset.
struct Args {
    positionals: Vec<String>,
    values: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Args {
    fn parse(argv: &[String], valued: &[&str], switches: &[&str]) -> Result<Args, String> {
        let mut args = Args { positionals: Vec::new(), values: Vec::new(), switches: Vec::new() };
        let mut i = 0;
        while i < argv.len() {
            let token = &argv[i];
            let name = match token.strip_prefix("--") {
                Some(rest) => format!("--{}", rest.split_once('=').map(|(key, _)| key).unwrap_or(rest)),
                None => {
                    args.positionals.push(token.clone());
                    i += 1;
                    continue;
                }
            };
            let inline = token.split_once('=').map(|(_, value)| value.to_string());
            if valued.contains(&name.as_str()) {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        i += 1;
                        argv.get(i).ok_or_else(|| format!("{name} needs a value"))?.clone()
                    }
                };
                args.values.push((name, value));
            } else if switches.contains(&name.as_str()) {
                if inline.is_some() {
                    return Err(format!("{name} takes no value"));
                }
                args.switches.push(name);
            } else {
                return Err(format!("unknown flag '{name}'"));
            }
            i += 1;
        }
        Ok(args)
    }

    /// The last spelling wins: a repeated flag is a correction, not a merge.
    fn value(&self, name: &str) -> Option<&str> {
        self.values.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }

    fn no_positionals(&self) -> Result<(), String> {
        match self.positionals.first() {
            Some(extra) => Err(format!("unexpected argument '{extra}'")),
            None => Ok(()),
        }
    }

    fn root(&self) -> Result<PathBuf, String> {
        let root = match self.value("--root") {
            Some(text) => PathBuf::from(text),
            None => default_root(),
        };
        if !root.exists() {
            return Err(format!("--root {} does not exist", root.display()));
        }
        Ok(root)
    }

    fn day(&self) -> Result<Option<LocalParts>, String> {
        match self.value("--day") {
            None => Ok(None),
            Some(text) => LocalParts::from_date(text).map(Some).ok_or_else(|| format!("--day must be YYYY-MM-DD, given '{text}'")),
        }
    }

    fn instant(&self, name: &str) -> Result<Option<LocalParts>, String> {
        match self.value(name) {
            None => Ok(None),
            Some(text) => flag::parse_datetime(text)
                .map(Some)
                .ok_or_else(|| format!("{name} must be 'YYYY-MM-DD HH:MM:SS', given '{text}'")),
        }
    }
}

/// The install root: the directory carrying this install's shipped settings, found by walking up
/// from this executable. The same [`wind_base::install`] rule every other binary uses — see the
/// module's doc for why one rule rather than one per tool.
fn default_root() -> PathBuf {
    wind_base::install::resolve_root_from_exe(None)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use wind_notes::flag::Flag;

    use super::*;

    /// A scratch install: `Config::load` of an empty directory answers with the shipped defaults, so
    /// every derived path — the table, the index, the cache — lands under the test's own directory.
    fn scratch(name: &str) -> (PathBuf, Config) {
        let dir = std::env::temp_dir().join(format!("windcap-notes-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config::load(&dir).unwrap();
        (dir, config)
    }

    fn seed(config: &Config) -> PathBuf {
        let path = config.flag_note_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "thumbnail,datetime,note\nAAA,2026-09-21 21:16:12,one\nBBB,2026-09-21 21:17:00,two\nCCC,2026-09-21 21:18:00,\"three, with a comma\"\n",
        )
        .unwrap();
        path
    }

    fn body(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn the_commands_read_their_own_arguments() {
        assert!(matches!(Command::parse(&["list".into()]).unwrap(), Command::List { day: None, .. }));
        assert_eq!(
            Command::parse(&["list".into(), "--day".into(), "2026-09-21".into()]).unwrap(),
            Command::List { root: default_root(), day: LocalParts::from_date("2026-09-21") }
        );
        match Command::parse(&["add".into(), "--note".into(), "keep this".into(), "--dry-run".into()]).unwrap() {
            Command::Add { note, at, dry_run, .. } => {
                assert_eq!(note.as_deref(), Some("keep this"));
                assert_eq!(at, None);
                assert!(dry_run);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            Command::parse(&["remove".into(), "3".into()]).unwrap(),
            Command::Remove { target: Target::Index(3), dry_run: false, .. }
        ));
        assert!(matches!(
            Command::parse(&["remove".into(), "2026-09-21 21:16:12".into(), "--dry-run".into()]).unwrap(),
            Command::Remove { target: Target::At(_), dry_run: true, .. }
        ));
        assert_eq!(
            Command::parse(&["markers".into(), "--day=2026-09-21".into(), "--width=1000".into()]).unwrap(),
            Command::Markers { root: default_root(), day: LocalParts::from_date("2026-09-21").unwrap(), width: 1000, height: 70, span: None }
        );
        let marked = Command::parse(&[
            "markers".into(),
            "--day".into(),
            "2026-09-21".into(),
            "--width".into(),
            "400".into(),
            "--height".into(),
            "40".into(),
            "--from".into(),
            "2026-09-21 08:00:00".into(),
            "--to".into(),
            "2026-09-21 16:00:00".into(),
        ])
        .unwrap();
        assert!(matches!(marked, Command::Markers { width: 400, height: 40, span: Some(_), .. }));
    }

    /// The version is answered by `main` ahead of `Command::parse`, which is where the config and
    /// the CSV would otherwise be reached -- so the assertion worth making is that the flag is
    /// *not* a command, and that the line itself names the binary and carries this crate's version.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windnotes "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        for spelling in ["--version", "-V"] {
            assert!(version::is_flag(spelling), "{spelling}");
            assert!(Command::parse(&[spelling.to_string()]).is_err(), "{spelling} is not a command");
        }
        assert!(usage().contains("--version"), "answered but undocumented:\n{}", usage());
        assert!(usage().contains("-V"), "{}", usage());
    }

    #[test]
    fn a_bad_command_line_is_refused_with_a_reason() {
        let bad: Vec<Vec<&str>> = vec![
            vec!["markers"],
            vec!["markers", "--day", "2026-09-21"],
            vec!["markers", "--day", "2026-09-21", "--width", "0"],
            vec!["markers", "--day", "2026-09-21", "--width", "9", "--from", "2026-09-21 08:00:00"],
            vec!["markers", "--day", "2026-09-21", "--width", "9", "--height", "0"],
            vec!["list", "--day", "21-09-2026"],
            vec!["list", "--unknown", "x"],
            vec!["list", "--dry-run"],
            vec!["list", "surprise"],
            vec!["remove"],
            vec!["remove", "1", "2"],
            vec!["remove", "nonsense"],
            vec!["remove", "--root"],
            vec!["add", "surprise"],
            vec!["add", "--dry-run=yes"],
            vec!["frobnicate"],
        ];
        for argv in bad {
            let error = Command::parse(&argv.iter().map(|a| a.to_string()).collect::<Vec<_>>()).unwrap_err();
            assert!(!error.is_empty(), "{argv:?} was refused without saying why");
        }
    }

    #[test]
    fn a_remove_target_is_a_row_or_an_instant_and_never_ambiguous() {
        assert_eq!(Target::parse(" 12 ").unwrap(), Target::Index(12));
        assert_eq!(Target::parse("2026-09-21 21:16:12").unwrap(), Target::At(LocalParts::from_stamp("2026-09-21_21-16-12").unwrap()));
        // The editor's own spelling addresses the same row.
        assert_eq!(Target::parse("2026/09/21   21:16:12").unwrap(), Target::parse("2026-09-21 21:16:12").unwrap());
        assert!(Target::parse("not a row").is_err());
        assert!(Target::parse("-3").is_err(), "a negative row is a typo, not the last row");
    }

    #[test]
    fn repeated_flags_are_a_correction_not_a_merge() {
        let args = Args::parse(&["--day".into(), "2026-01-01".into(), "--day".into(), "2026-09-21".into()], &["--day"], &[]).unwrap();
        assert_eq!(args.value("--day"), Some("2026-09-21"));
        assert_eq!(args.day().unwrap(), LocalParts::from_date("2026-09-21"));
    }

    #[test]
    fn listing_reports_the_table_it_read() {
        let (dir, config) = scratch("list");
        // No file yet is a report, not an error.
        list(&config, None).unwrap();
        seed(&config);
        list(&config, None).unwrap();
        // A day filter uses the product day, so the 21st's rows do not answer for the 22nd.
        list(&config, LocalParts::from_date("2026-09-22")).unwrap();
        list(&config, LocalParts::from_date("2026-09-21")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_thumbnail_is_labelled_by_what_its_own_header_says() {
        let when = LocalParts::from_stamp("2026-09-21_21-16-12").unwrap();
        assert_eq!(thumbnail_label(&Entry::Flag(Flag { thumbnail: "AAA".into(), when, note: "n".into() })), "unreadable", "AAA is base64 but not a JPEG");
        assert_eq!(thumbnail_label(&Entry::Flag(Flag { thumbnail: String::new(), when, note: "n".into() })), "none");
        assert_eq!(thumbnail_label(&Entry::Raw(vec!["x".into(), "y".into(), "z".into()])), "unreadable");
    }

    #[test]
    fn removing_a_row_leaves_the_other_rows_byte_for_byte() {
        let (dir, config) = scratch("remove");
        let path = seed(&config);
        remove(&config, Target::Index(1), false).unwrap();
        assert_eq!(
            body(&path),
            "thumbnail,datetime,note\nAAA,2026-09-21 21:16:12,one\nCCC,2026-09-21 21:18:00,\"three, with a comma\"\n"
        );
        remove(&config, Target::parse("2026-09-21 21:18:00").unwrap(), false).unwrap();
        assert_eq!(body(&path), "thumbnail,datetime,note\nAAA,2026-09-21 21:16:12,one\n");
        // The row is gone, so naming it again is an error rather than a silent no-op.
        assert!(remove(&config, Target::parse("2026-09-21 21:18:00").unwrap(), false).is_err());
        assert!(remove(&config, Target::Index(9), false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_removes_nothing() {
        let (dir, config) = scratch("remove-dry");
        let path = seed(&config);
        let quiet = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(quiet).unwrap();
        remove(&config, Target::Index(0), true).unwrap();
        assert_eq!(FlagStore::load(&path).unwrap().len(), 3);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), quiet, "a dry run does not touch the file");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "and leaves no staging file behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug, reported the way the fixed code behaves: selecting everything empties the table, and
    /// the file is still there for the Python app to open.
    #[test]
    fn removing_every_row_keeps_the_file_and_its_header() {
        let (dir, config) = scratch("remove-all");
        let path = seed(&config);
        for _ in 0..3 {
            remove(&config, Target::Index(0), false).unwrap();
        }
        assert!(path.exists());
        assert_eq!(body(&path), "thumbnail,datetime,note\n");
        assert!(FlagStore::load(&path).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adding_a_flag_for_a_past_instant_needs_no_screen() {
        let (dir, config) = scratch("add-history");
        // No index in this scratch install, so the flag is filed picture-less, exactly as upstream
        // files one when its lookup returns nothing.
        let when = LocalParts::from_stamp("2026-09-21_21-16-12").unwrap();
        add(&config, Some("typed"), Some(when), false).unwrap();
        assert_eq!(body(&config.flag_note_path()), "thumbnail,datetime,note\n,2026-09-21 21:16:12,typed\n");
        add(&config, None, Some(when), true).unwrap();
        assert_eq!(FlagStore::load(&config.flag_note_path()).unwrap().len(), 1, "the dry run added nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markers_prints_a_position_for_every_flag_on_the_day() {
        let (dir, config) = scratch("markers");
        let path = seed(&config);
        let day = LocalParts::from_date("2026-09-21").unwrap();
        // The product day, which is always measurable.
        draw_markers(&config, day, None, 1000, 70).unwrap();
        // The recorded range, named explicitly: this is the basis upstream proportions against, and
        // the two endpoints are the first and last rows of the fixture.
        draw_markers(
            &config,
            day,
            Some(DaySpan::new(
                LocalParts::from_stamp("2026-09-21_21-16-12").unwrap().naive_epoch_seconds(),
                LocalParts::from_stamp("2026-09-21_21-18-00").unwrap().naive_epoch_seconds(),
            )),
            1000,
            70,
        )
        .unwrap();
        // A day with nothing on it, and a degenerate span, are both reports.
        draw_markers(&config, LocalParts::from_date("2026-01-01").unwrap(), None, 800, 40).unwrap();
        draw_markers(&config, day, Some(DaySpan::new(0, 0)), 800, 40).unwrap();
        assert_eq!(
            body(&path),
            "thumbnail,datetime,note\nAAA,2026-09-21 21:16:12,one\nBBB,2026-09-21 21:17:00,two\nCCC,2026-09-21 21:18:00,\"three, with a comma\"\n",
            "the report is read-only"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_reaches_every_command() {
        let (dir, config) = scratch("dispatch");
        seed(&config);
        for command in [
            Command::List { root: dir.clone(), day: None },
            Command::Remove { root: dir.clone(), target: Target::Index(0), dry_run: true },
            Command::Markers { root: dir.clone(), day: LocalParts::from_date("2026-09-21").unwrap(), width: 640, height: 40, span: None },
        ] {
            dispatch(&command, &config).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
