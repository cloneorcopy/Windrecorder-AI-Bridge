//! The `windcapctl` argument grammar, as data rather than as control flow.
//!
//! Every subcommand declares which flags take a value, which are bare switches, and whether it
//! accepts positionals. Anything else in argv is refused, so a typo (`--form 2026-09-22`) fails
//! loudly instead of silently falling back to "today" — which in a history tool is a wrong answer,
//! not a default.
//!
//! Parsing is total and side-effect free: no file is opened and no clock is read here, so the whole
//! grammar is exercised by the tests below without a database or a screen.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use wind_base::version;

use wind_base::range::{self, RangeArg};

/// A parsed subcommand. `main` matches on this and calls exactly one reporter.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Status,
    Bench { iters: u32 },
    Grab { iters: u32, width: u32, source: Option<(i32, i32)> },
    Query(Box<QueryArgs>),
    Day(DayArgs),
    Stats(StatsArgs),
    Index(IndexArgs),
    Inspect(InspectArgs),
    Snap(SnapArgs),
    BenchSearch(BenchSearchArgs),
}

#[derive(Debug, PartialEq, Eq)]
pub struct QueryArgs {
    pub root: Option<PathBuf>,
    /// Joined positionals; `Query::with_keywords` does the whitespace splitting.
    pub keywords: String,
    pub window: RangeArg,
    pub exclude: Option<String>,
    /// `None` means "not given", so the reporter can fall back to the user's own config rather
    /// than to a number baked into this binary.
    pub page: Option<usize>,
    pub size: Option<usize>,
    /// `--exact`: do not expand the query through the shape-similar Chinese table.
    pub exact: bool,
    pub json: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DayArgs {
    pub root: Option<PathBuf>,
    pub day: String,
    /// Samples in the rendered timeline strip.
    pub detail: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct StatsArgs {
    pub root: Option<PathBuf>,
    pub month: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct IndexArgs {
    pub root: Option<PathBuf>,
    /// `--status` reports which files carry the index and writes nothing.
    pub status_only: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InspectArgs {
    pub root: Option<PathBuf>,
    pub segment: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SnapArgs {
    pub path: PathBuf,
    pub width: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BenchSearchArgs {
    pub root: Option<PathBuf>,
    pub iterations: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// `-h`/`--help`/`help`: print the usage screen and stop, successfully.
    Help,
    /// `--version`/`-V`: print this binary's name, package version and build profile and stop,
    /// successfully. Read before the command word is looked at, so it is answered on an install
    /// with no index and no config as well as on a good one.
    Version,
    /// Not a command we know: the caller prints the usage screen *and* fails.
    Unknown(String),
    /// A known command invoked badly. The caller prints the usage screen and this message.
    Bad(String),
}

/// What one subcommand accepts.
struct Spec {
    /// Flags that consume the next argv entry (or use `--flag=value`).
    values: &'static [&'static str],
    /// Bare flags.
    switches: &'static [&'static str],
    positional: Positional,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Positional {
    None,
    /// Exactly one, and it is mandatory.
    One,
    /// Zero or more.
    Many,
}

const SPECS: &[(&str, Spec)] = &[
    (
        "query",
        Spec {
            values: &["--root", "--day", "--from", "--to", "--exclude", "--page", "--size"],
            switches: &["--exact", "--json"],
            positional: Positional::Many,
        },
    ),
    (
        "day",
        // The date is positional, but `--day` is spelled too often in scripts to reject it, so the
        // count is checked by hand below instead of by the spec.
        Spec { values: &["--root", "--day", "--detail"], switches: &[], positional: Positional::Many },
    ),
    ("stats", Spec { values: &["--root", "--month"], switches: &[], positional: Positional::None }),
    ("index", Spec { values: &["--root"], switches: &["--status"], positional: Positional::None }),
    ("inspect", Spec { values: &["--root"], switches: &[], positional: Positional::One }),
    ("snap", Spec { values: &["--width"], switches: &[], positional: Positional::One }),
    ("bench-search", Spec { values: &["--root", "--iterations"], switches: &[], positional: Positional::None }),
    ("status", Spec { values: &[], switches: &[], positional: Positional::None }),
    ("bench", Spec { values: &[], switches: &[], positional: Positional::Many }),
    ("grab", Spec { values: &[], switches: &[], positional: Positional::Many }),
];

/// Everything after the command word, sorted into positionals / valued flags / switches.
struct Flags {
    positionals: Vec<String>,
    values: BTreeMap<String, String>,
    switches: BTreeSet<String>,
}

fn spec_of(command: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|(name, _)| *name == command).map(|(_, s)| s)
}

impl Flags {
    /// `argv` excludes both the program name and the command word.
    fn parse(argv: &[String], spec: &Spec) -> Result<Flags, String> {
        let mut out = Flags { positionals: Vec::new(), values: BTreeMap::new(), switches: BTreeSet::new() };
        let mut i = 0;
        // After a bare `--`, everything is positional: `windcapctl query -- --weird-token`.
        let mut flags_closed = false;
        while i < argv.len() {
            let token = argv[i].as_str();
            if flags_closed {
                out.positionals.push(token.to_string());
                i += 1;
                continue;
            }
            if token == "--" {
                flags_closed = true;
                i += 1;
                continue;
            }
            if !token.starts_with("--") {
                out.positionals.push(token.to_string());
                i += 1;
                continue;
            }
            let (name, inline) = match token.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (token, None),
            };
            if spec.switches.contains(&name) {
                if inline.is_some() {
                    return Err(format!("flag '{name}' takes no value"));
                }
                out.switches.insert(name.to_string());
                i += 1;
                continue;
            }
            if !spec.values.contains(&name) {
                return Err(format!("unknown flag '{name}'"));
            }
            let value = match inline {
                Some(v) => v.to_string(),
                None => {
                    i += 1;
                    match argv.get(i) {
                        Some(v) if v != "--" => v.clone(),
                        _ => return Err(format!("flag '{name}' needs a value")),
                    }
                }
            };
            // A later spelling of the same flag wins: `--day a --day b` is a correction, not a merge.
            out.values.insert(name.to_string(), value);
            i += 1;
        }
        Ok(out)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn number<T: FromStr>(&self, name: &str) -> Result<Option<T>, String>
    where
        T::Err: std::fmt::Display,
    {
        match self.value(name) {
            None => Ok(None),
            Some(raw) => raw.parse::<T>().map(Some).map_err(|e| format!("{name}: {e}")),
        }
    }

    /// Hands over the positionals, rejecting a count the command's spec does not allow.
    fn take_positionals(&mut self, command: &str, spec: &Spec) -> Result<Vec<String>, String> {
        let taken = std::mem::take(&mut self.positionals);
        match spec.positional {
            Positional::None if !taken.is_empty() => {
                Err(format!("{command} takes no arguments, got '{}'", taken.join(" ")))
            }
            Positional::One if taken.len() != 1 => {
                Err(format!("{command} needs exactly one argument, got {}", taken.len()))
            }
            _ => Ok(taken),
        }
    }
}

/// The `--day`/`--from`/`--to` values, syntax-checked here so a bad date is a usage failure and
/// never a silent whole-library scan.
fn window(flags: &Flags) -> Result<RangeArg, String> {
    if let Some(day) = flags.value("--day") {
        range::parse_date(day).ok_or_else(|| format!("invalid --day '{day}': expected YYYY-MM-DD"))?;
    }
    for name in ["--from", "--to"] {
        if let Some(value) = flags.value(name) {
            range::parse_instant(value, name == "--to").ok_or_else(|| {
                format!("invalid {name} '{value}': expected YYYY-MM-DD or YYYY-MM-DD_HH-MM-SS")
            })?;
        }
    }
    Ok(RangeArg {
        day: flags.value("--day").map(str::to_string),
        from: flags.value("--from").map(str::to_string),
        to: flags.value("--to").map(str::to_string),
    })
}

pub fn parse(argv: &[String]) -> Result<Command, ParseError> {
    let Some(command) = argv.first().map(String::as_str) else { return Err(ParseError::Unknown(String::new())) };
    // Checked here, ahead of the subcommand table below and of `Flags::parse`, because a version
    // request must not need a library to resolve, a root to exist or a `--day` to parse.
    if version::is_flag(command) {
        return Err(ParseError::Version);
    }
    if matches!(command, "-h" | "--help" | "help") {
        return Err(ParseError::Help);
    }
    let spec = spec_of(command).ok_or_else(|| ParseError::Unknown(command.to_string()))?;
    let mut flags = Flags::parse(&argv[1..], spec).map_err(ParseError::Bad)?;
    build(command, spec, &mut flags).map_err(ParseError::Bad)
}

fn build(command: &str, spec: &Spec, flags: &mut Flags) -> Result<Command, String> {
    let root = flags.value("--root").map(PathBuf::from);
    match command {
        "status" => {
            flags.take_positionals("status", spec)?;
            Ok(Command::Status)
        }
        "bench" => {
            let taken = flags.take_positionals("bench", spec)?;
            Ok(Command::Bench { iters: count_arg("bench", &taken, 2000)? })
        }
        "grab" => {
            let taken = flags.take_positionals("grab", spec)?;
            if taken.len() > 4 {
                return Err(format!("grab takes at most 4 arguments, got {}", taken.len()));
            }
            Ok(Command::Grab {
                iters: taken.first().map(|s| s.parse::<u32>().ok()).flatten().unwrap_or(20),
                width: taken.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1920),
                source: taken
                    .get(2)
                    .and_then(|a| a.parse::<i32>().ok())
                    .zip(taken.get(3).and_then(|b| b.parse::<i32>().ok())),
            })
        }
        "query" => {
            let taken = flags.take_positionals("query", spec)?;
            Ok(Command::Query(Box::new(QueryArgs {
                root,
                keywords: taken.join(" "),
                window: window(flags)?,
                exclude: flags.value("--exclude").map(str::to_string),
                page: flags.number("--page")?,
                size: flags.number("--size")?,
                exact: flags.switches.contains("--exact"),
                json: flags.switches.contains("--json"),
            })))
        }
        "day" => {
            let taken = flags.take_positionals("day", spec)?;
            if taken.len() > 1 {
                return Err(format!("day takes one date, got '{}'", taken.join(" ")));
            }
            // The positional is the date; `--day` is accepted too so both spellings work.
            let day = flags.value("--day").map(str::to_string).or_else(|| taken.first().cloned());
            let day = day.ok_or_else(|| "day needs a YYYY-MM-DD argument".to_string())?;
            range::parse_date(&day).ok_or_else(|| format!("invalid day '{day}': expected YYYY-MM-DD"))?;
            Ok(Command::Day(DayArgs { root, day, detail: flags.number("--detail")?.unwrap_or(8) }))
        }
        "stats" => {
            flags.take_positionals("stats", spec)?;
            let month = match flags.value("--month") {
                Some(m) => {
                    range::parse_month(m)?;
                    Some(m.to_string())
                }
                None => None,
            };
            Ok(Command::Stats(StatsArgs { root, month }))
        }
        "index" => {
            flags.take_positionals("index", spec)?;
            Ok(Command::Index(IndexArgs { root, status_only: flags.switches.contains("--status") }))
        }
        "inspect" => {
            let taken = flags.take_positionals("inspect", spec)?;
            Ok(Command::Inspect(InspectArgs { root, segment: taken[0].clone() }))
        }
        "snap" => {
            let taken = flags.take_positionals("snap", spec)?;
            Ok(Command::Snap(SnapArgs {
                path: PathBuf::from(&taken[0]),
                width: flags.number("--width")?.unwrap_or(1920),
            }))
        }
        "bench-search" => {
            flags.take_positionals("bench-search", spec)?;
            Ok(Command::BenchSearch(BenchSearchArgs {
                root,
                iterations: flags.number("--iterations")?.unwrap_or(20),
            }))
        }
        other => Err(format!("{other} has no handler")),
    }
}

/// `bench [ITERS]`: one optional count, shared shape with the other probe commands.
fn count_arg(command: &str, taken: &[String], default: u32) -> Result<u32, String> {
    match taken.len() {
        0 => Ok(default),
        1 => taken[0].parse::<u32>().map_err(|e| format!("{command}: {e}")),
        n => Err(format!("{command} takes at most one argument, got {n}")),
    }
}

/// The usage screen. `main` prints it, and a test asserts every command appears in it, so a new
/// subcommand cannot ship undocumented by accident.
pub fn usage() -> String {
    String::from(
        "\
usage: windcapctl <command> [options]

  windcapctl query [KEYWORDS...] [--root PATH]
                          [--day YYYY-MM-DD | --from STAMP --to STAMP]
                          [--exclude WORDS] [--page N] [--size N] [--exact] [--json]
                        Search the recorded history. Default range is today's product day.
                        --exact turns off shape-similar Chinese expansion.
  windcapctl day YYYY-MM-DD [--root PATH] [--detail N]
                        The OneDay view: totals, an ASCII activity chart, where the time went,
                        and a sampled timeline strip.
  windcapctl stats [--root PATH] [--month YYYY-MM]
                        Per-day histogram for one month plus whole-library totals.
  windcapctl index [--root PATH] [--status]
                        Create the timestamp index on every month file, then time the same
                        query 50x with it absent and present. THE ONLY COMMAND THAT WRITES.
  windcapctl inspect SEGMENT [--root PATH]
                        Every row of one segment with its similarity to the row before it.
  windcapctl snap OUT.jpg [--width N]
                        Grab the desktop and write it out as a JPEG.
  windcapctl bench-search [--root PATH] [--iterations N]
                        Fixed query battery: best ms / mean ms / rows hit per query.
  windcapctl status     One session snapshot (recordable, idle, desktop).
  windcapctl bench [ITERS]
                        Per-call cost of each Win32 probe.
  windcapctl grab [ITERS] [WIDTH] [SRC_W SRC_H]
                        Grab -> luma -> change gate, timed.

  --root defaults to the install directory: the folder carrying config_src/ (or, on an
  install that has not moved its data up, windrecorder/config_src/) and userdata/.
  STAMP is YYYY-MM-DD_HH-MM-SS; a bare YYYY-MM-DD also works for --from and --to.

  windcapctl --version    print this binary's name, its package version and its build profile.
                        Reads no config and opens no database, so it answers on a broken install.",
    )
}

/// What `windcapctl --version` answers with. The format lives in `wind_base::version` and is
/// shared by all eleven binaries; the name and `env!` are this crate's, so the number is the one this
/// executable was compiled from.
pub fn version_line() -> String {
    version::line("windcapctl", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn command(items: &[&str]) -> Result<Command, ParseError> {
        parse(&argv(items))
    }

    #[test]
    fn the_usage_screen_names_every_subcommand() {
        let text = usage();
        for (name, _) in SPECS {
            assert!(text.contains(&format!("windcapctl {name}")), "usage omits {name}");
        }
        assert!(text.starts_with("usage: windcapctl"), "{text}");
    }

    #[test]
    fn keywords_are_positional_and_join_with_a_space() {
        let Command::Query(args) = command(&["query", "chat", "gpt", "notes"]).unwrap() else { panic!("query") };
        assert_eq!(args.keywords, "chat gpt notes");
        assert!(args.exclude.is_none());
        assert!(!args.exact);
        assert!(!args.json);
    }

    #[test]
    fn an_empty_keyword_list_is_a_valid_bare_range_search() {
        let Command::Query(args) = command(&["query", "--day", "2026-09-22"]).unwrap() else { panic!("query") };
        assert_eq!(args.keywords, "");
        assert_eq!(args.window.day.as_deref(), Some("2026-09-22"));
    }

    #[test]
    fn flags_take_a_value_either_way_and_the_last_wins() {
        let Command::Query(args) = command(&["query", "--size=5", "--page", "3", "--size", "9"]).unwrap()
        else {
            panic!("query")
        };
        assert_eq!(args.size, Some(9));
        assert_eq!(args.page, Some(3));
    }

    #[test]
    fn switches_and_value_flags_are_not_interchangeable() {
        assert!(command(&["query", "--exact"]).is_ok());
        assert!(matches!(command(&["query", "--exact=true"]), Err(ParseError::Bad(_))));
        // A switch does not swallow the token after it, so this is still a keyword search.
        let Command::Query(args) = command(&["query", "--json", "extra"]).unwrap() else { panic!("query") };
        assert_eq!(args.keywords, "extra");
        assert!(matches!(command(&["query", "--page"]), Err(ParseError::Bad(_))));
        assert!(matches!(command(&["query", "--page", "--"]), Err(ParseError::Bad(_))));
    }

    #[test]
    fn a_typo_in_a_flag_name_is_refused_not_ignored() {
        let err = command(&["query", "--form", "2026-09-22"]).unwrap_err();
        assert!(matches!(&err, ParseError::Bad(m) if m.contains("unknown flag '--form'")), "{err:?}");
    }

    #[test]
    fn a_malformed_day_is_a_usage_error_before_any_file_is_opened() {
        let err = command(&["query", "--day", "22/09/2026"]).unwrap_err();
        assert!(matches!(&err, ParseError::Bad(m) if m.contains("invalid --day")), "{err:?}");
        let err = command(&["day", "not-a-date"]).unwrap_err();
        assert!(matches!(&err, ParseError::Bad(m) if m.contains("invalid day")), "{err:?}");
        let err = command(&["query", "--from", "2026-09-22T00:00"]).unwrap_err();
        assert!(matches!(&err, ParseError::Bad(m) if m.contains("invalid --from")), "{err:?}");
        let err = command(&["stats", "--month", "2026-13"]).unwrap_err();
        assert!(matches!(&err, ParseError::Bad(m) if m.contains("--month")), "{err:?}");
    }

    #[test]
    fn every_command_that_needs_an_argument_says_so() {
        for items in [vec!["day"], vec!["inspect"], vec!["snap"]] {
            assert!(matches!(command(&items), Err(ParseError::Bad(_))), "{items:?}");
        }
        assert!(matches!(command(&["stats", "2026-09"]), Err(ParseError::Bad(_))));
    }

    #[test]
    fn the_double_dash_ends_flags() {
        let Command::Query(args) = command(&["query", "--", "--not-a-flag"]).unwrap() else { panic!("query") };
        assert_eq!(args.keywords, "--not-a-flag");
    }

    #[test]
    fn root_and_numeric_options_are_parsed() {
        let Command::Day(args) = command(&["day", "2026-09-22", "--root", "/tmp/x", "--detail", "12"]).unwrap()
        else {
            panic!("day")
        };
        assert_eq!(args.root, Some(PathBuf::from("/tmp/x")));
        assert_eq!(args.detail, 12);
        let Command::BenchSearch(args) = command(&["bench-search", "--iterations", "3"]).unwrap() else {
            panic!("bench-search")
        };
        assert_eq!(args.iterations, 3);
        assert!(matches!(command(&["bench-search", "--iterations", "zero"]), Err(ParseError::Bad(_))));
    }

    #[test]
    fn legacy_probe_commands_still_parse() {
        assert_eq!(command(&["status"]).unwrap(), Command::Status);
        assert_eq!(command(&["bench"]).unwrap(), Command::Bench { iters: 2000 });
        assert_eq!(command(&["bench", "7"]).unwrap(), Command::Bench { iters: 7 });
        assert_eq!(
            command(&["grab", "5", "1280", "800", "600"]).unwrap(),
            Command::Grab { iters: 5, width: 1280, source: Some((800, 600)) }
        );
        assert_eq!(command(&["grab"]).unwrap(), Command::Grab { iters: 20, width: 1920, source: None });
        assert!(matches!(command(&["bench", "a", "b"]), Err(ParseError::Bad(_))));
        assert!(matches!(command(&["grab", "1", "2", "3", "4", "5"]), Err(ParseError::Bad(_))));
    }

    #[test]
    fn unknown_and_missing_commands_ask_for_the_usage_screen() {
        assert!(matches!(command(&["frobnicate"]), Err(ParseError::Unknown(m)) if m == "frobnicate"));
        assert!(matches!(command(&[]), Err(ParseError::Unknown(_))));
        assert!(matches!(command(&["help"]), Err(ParseError::Help)));
    }

    #[test]
    fn snap_never_looks_for_a_root() {
        let Command::Snap(args) = command(&["snap", "out.jpg", "--width", "800"]).unwrap() else { panic!("snap") };
        assert_eq!(args.path, PathBuf::from("out.jpg"));
        assert_eq!(args.width, 800);
    }

    #[test]
    fn index_knows_whether_it_is_allowed_to_write() {
        assert_eq!(command(&["index"]).unwrap(), Command::Index(IndexArgs { root: None, status_only: false }));
        assert!(matches!(
            command(&["index", "--status"]),
            Ok(Command::Index(IndexArgs { status_only: true, .. }))
        ));
    }

    /// `--version` is answered by the grammar, not by a reporter, which is the point: there is no
    /// library, index or config behind it. Both spellings, and the line says which binary and
    /// which build produced it.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windcapctl "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        for spelling in ["--version", "-V"] {
            assert_eq!(command(&[spelling]), Err(ParseError::Version), "{spelling}");
            // A root that is not there does not stop it, because the flag never reaches a root.
            assert_eq!(command(&[spelling, "--root", "Z:/no/such/install"]), Err(ParseError::Version));
        }
        assert!(usage().contains("--version"), "answered but undocumented:\n{}", usage());
    }
}
