//! The `windmcp` argument grammar.
//!
//! Deliberately narrow, and the narrowness is the security model: `--root`, `--host`, `--port`, and
//! nothing else. There is no `--token` and no `--no-auth`, and an unknown flag is an error rather
//! than something to ignore. A command line on Windows is readable by every process on the machine,
//! and this app already runs a tray process the user did not start, so a token that arrived as an
//! argument would be a token disclosed to anything that enumerates processes. Refusing to accept one
//! at all is stronger than accepting one and warning about it.
//!
//! Parsing is total and side-effect free — no file is opened and no clock is read here — so the whole
//! grammar is exercised by the tests below without a database or a socket.

use std::path::PathBuf;

use wind_base::clock::LocalParts;

/// A span of history as the user asked for it, before anything is opened or any clock is read.
///
/// A `--day` deliberately carries the *word* the user typed and not a pair of instants. Turning it
/// into bounds needs the install's `day_begin_minutes`, which lives in a config file, and a grammar
/// that read one would no longer be total or side-effect free — and, more to the point, a grammar
/// that guessed at the bounds is exactly how `--day` came to mean a calendar day here while meaning a
/// product day in `windcapctl query`. So the day is resolved once, in `tools::day_window`, by every
/// command that takes one.
#[derive(Debug, PartialEq, Eq)]
pub enum Range {
    /// `--day YYYY-MM-DD`: one whole product day.
    Day(String),
    /// `--from`/`--to`, honoured exactly with no day shift applied.
    Between(String, String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Start the resident HTTP service.
    Serve { root: Option<PathBuf>, host: Option<String>, port: Option<i64> },
    /// Report what would happen, without binding anything.
    Doctor { root: Option<PathBuf> },
    /// The tools, callable from a terminal, so the tool logic is testable without an MCP client.
    Status { root: Option<PathBuf>, json: bool },
    Search {
        root: Option<PathBuf>,
        keywords: String,
        range: Range,
        exclude: Option<String>,
        limit: Option<usize>,
        offset: Option<usize>,
        json: bool,
    },
    Around { root: Option<PathBuf>, moment: String, window: i64, limit: Option<usize>, max_text: Option<usize>, json: bool },
    AppUsage { root: Option<PathBuf>, range: Range, limit: Option<usize>, json: bool },
    DaySummary { root: Option<PathBuf>, date: String, limit: Option<usize>, json: bool },
    Frame { root: Option<PathBuf>, moment: String, window: i64, json: bool },
    SummariesPending { root: Option<PathBuf>, range: Range, include: Option<String>, max_text: Option<usize>, json: bool },
    SummariesRead { root: Option<PathBuf>, range: Range, kind: Option<String>, json: bool },
    PromptsRead { root: Option<PathBuf>, json: bool },
    PeriodSummaryWrite { root: Option<PathBuf>, segment: String, day: Option<String>, body: Written, written_by: Option<String>, model: Option<String>, json: bool },
    DaySummaryWrite { root: Option<PathBuf>, date: String, body: Written, allow_partial: bool, written_by: Option<String>, model: Option<String>, json: bool },
}

/// A paragraph being filed, and how it arrived.
///
/// Two shapes because a summary is prose: `--text` answers for a one-line test, and `--text-file` is the
/// only sane way to hand a multi-paragraph CJK summary to a Windows `argv`, where a native executable
/// receives a command line the shell has already re-encoded. The grammar records the path and never opens
/// it — same rule that keeps `--day` unresolved here, so the whole of this file stays testable without a
/// filesystem.
#[derive(Debug, PartialEq, Eq)]
pub enum Written {
    Inline(String),
    FromFile(PathBuf),
}

impl Written {
    /// The paragraph, read now rather than at parse time.
    ///
    /// A file's bytes are taken exactly as they are, its final newline included: the storage layer's
    /// promise is that text is stored as sent, and a CLI that trimmed on the way through would be the
    /// one place that promise quietly stops holding.
    pub fn read(&self) -> Result<String, String> {
        match self {
            Written::Inline(text) => Ok(text.clone()),
            Written::FromFile(path) => std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display())),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// `-h`/`--help`/`help`: print the usage screen and stop, successfully.
    Help,
    /// `--version`/`-V`/`version`: name, package version, build profile, and stop. Its own variant
    /// rather than another spelling of `Help`, because the two answers are different: one is a
    /// screen about what this binary does, the other is a fact about which build of it you have.
    Version,
    /// Not a command we know: the caller prints the usage screen *and* fails.
    Unknown(String),
    /// A known command invoked badly. The caller prints the usage screen and this message.
    Bad(String),
}

impl ParseError {
    pub fn message(&self) -> String {
        match self {
            ParseError::Help => USAGE.to_string(),
            ParseError::Version => format!("{}\n", version_line()),
            ParseError::Unknown(word) => format!("unknown command {word}\n\n{USAGE}"),
            ParseError::Bad(why) => format!("{why}\n\n{USAGE}"),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ParseError {}

/// What one subcommand accepts.
struct Spec {
    /// Flags that consume the next argv entry (or use `--flag=value`).
    values: &'static [&'static str],
    /// Bare flags.
    switches: &'static [&'static str],
    /// What the positionals are, for the error message.
    positional: &'static str,
    max_positional: usize,
}

const SPECS: &[(&str, Spec)] = &[
    ("serve", Spec { values: &["--root", "--host", "--port"], switches: &[], positional: "", max_positional: 0 }),
    ("doctor", Spec { values: &["--root"], switches: &[], positional: "", max_positional: 0 }),
    ("status", Spec { values: &["--root"], switches: &["--json"], positional: "", max_positional: 0 }),
    (
        "search",
        Spec {
            values: &["--root", "--from", "--to", "--day", "--exclude", "--limit", "--offset"],
            switches: &["--json"],
            positional: "keywords",
            max_positional: usize::MAX,
        },
    ),
    ("around", Spec { values: &["--root", "--window", "--limit", "--max-text"], switches: &["--json"], positional: "<timestamp>", max_positional: 1 }),
    ("app-usage", Spec { values: &["--root", "--from", "--to", "--day", "--limit"], switches: &["--json"], positional: "", max_positional: 0 }),
    ("day-summary", Spec { values: &["--root", "--limit"], switches: &["--json"], positional: "<date>", max_positional: 1 }),
    ("frame", Spec { values: &["--root", "--window"], switches: &["--json"], positional: "<timestamp>", max_positional: 1 }),
    (
        "summaries-pending",
        Spec {
            values: &["--root", "--from", "--to", "--day", "--include", "--max-text"],
            switches: &["--json"],
            positional: "",
            max_positional: 0,
        },
    ),
    ("summaries-read", Spec { values: &["--root", "--from", "--to", "--day", "--kind"], switches: &["--json"], positional: "", max_positional: 0 }),
    ("prompts-read", Spec { values: &["--root"], switches: &["--json"], positional: "", max_positional: 0 }),
    (
        "period-summary-write",
        Spec {
            values: &["--root", "--day", "--text", "--text-file", "--written-by", "--model"],
            switches: &["--json"],
            positional: "<segment>",
            max_positional: 1,
        },
    ),
    (
        "day-summary-write",
        Spec {
            values: &["--root", "--text", "--text-file", "--written-by", "--model"],
            switches: &["--json", "--allow-partial"],
            positional: "<date>",
            max_positional: 1,
        },
    ),
];

pub const USAGE: &str = "\
windmcp — Windrecorder's MCP bridge: screen history over streamable HTTP, read and summarised

  windmcp serve      [--root PATH] [--host H] [--port N]      start the service
  windmcp doctor     [--root PATH]                            what would happen, without binding
  windmcp status     [--root PATH] [--json]                   the windrecorder_status tool
  windmcp search KW... [--day D | --from T --to T] [--exclude W]
                     [--limit N] [--offset N] [--json]        the windrecorder_search tool
  windmcp around  <ts> [--window S] [--limit N] [--max-text N] [--json]
  windmcp app-usage    [--day D | --from T --to T] [--limit N] [--json]
  windmcp day-summary <date> [--limit N] [--json]
  windmcp frame      <ts> [--window S] [--json]
  windmcp summaries-pending [--day D | --from T --to T] [--include all]
                     [--max-text N] [--json]                   the work queue, with the full text of
                                                               each stretch that has no summary
  windmcp summaries-read    [--day D | --from T --to T] [--kind period|daily|both] [--json]
  windmcp prompts-read      [--json]                    the prompt words this machine would send
  windmcp period-summary-write <segment> --text TEXT | --text-file PATH
                     [--day D] [--written-by S] [--model S] [--json]
  windmcp day-summary-write  <date>  --text TEXT | --text-file PATH
                     [--allow-partial] [--written-by S] [--model S] [--json]
  windmcp --version  |  -V      name, package version and build profile; reads no config

<ts> is a stored timestamp handed back unchanged, or '2026-09-20', '2026-09-20 14:30:00', or
an ISO-8601 datetime with an offset. A stored timestamp is the local wall clock counted as if
it were UTC — NOT a POSIX second.

The two write commands are the only ones here that change anything, and they are the same code the
service dispatches to: `period-summary-write` files a stretch's paragraph, `day-summary-write` files a
day's and refuses while any of that day's stretches has none (say --allow-partial to write over the gap
and have it recorded as written over a gap). A paragraph is prose, so --text-file is the shape that
survives a Windows command line; its bytes are stored exactly as they are, final newline included.
Neither a prompt nor a summary has a length limit, and nothing here scans a summary for words.

--day YYYY-MM-DD is one Windrecorder product day, not a calendar day: it runs from that date
at the install's day_begin_minutes (03:00 by default) to the next date at one second before
it, so a 1am frame belongs to the previous day. windcapctl query --day resolves it the same
way, and every report here prints the window and the rule it used.

Where the service listens, and the bearer token, come from userdata/config_user.json and
nowhere else. There is no --token and no --no-auth: a command line is readable by every
process on the machine. Set enable_mcp_server to true to switch the service on at all.";

/// What `windmcp --version` answers with: `<binary> <package version> (<debug|release>)`.
///
/// The format is `wind_base::version`'s so that the eleven binaries in the zip cannot describe
/// themselves eleven different ways; the name and the `env!` are this crate's, so the number is the
/// one these bytes were built from. Answered without a root, a config, a token or a socket.
pub fn version_line() -> String {
    wind_base::version::line("windmcp", env!("CARGO_PKG_VERSION"))
}

pub fn parse(argv: &[String]) -> Result<Command, ParseError> {
    let argv = argv.strip_prefix_bin();
    let Some(name) = argv.first() else { return Err(ParseError::Help) };
    // Read before the help words and before the command table: `serve` would otherwise go looking
    // for a root, a config and a bearer token in order to answer a question that has nothing to do
    // with any of them.
    if wind_base::version::is_flag(name) || name == "version" {
        return Err(ParseError::Version);
    }
    if matches!(name.as_str(), "-h" | "--help" | "help") {
        return Err(ParseError::Help);
    }
    let spec = &SPECS.iter().find(|(command, _)| command == name).ok_or_else(|| ParseError::Unknown(name.clone()))?.1;

    let mut values: Vec<(&str, String)> = Vec::new();
    let mut switches: Vec<&str> = Vec::new();
    let mut positional: Vec<String> = Vec::new();

    let mut rest = argv[1..].iter();
    while let Some(argument) = rest.next() {
        if let Some((flag, inline)) = argument.split_once('=') {
            if !spec.values.contains(&flag) {
                return Err(ParseError::Bad(unknown_flag(spec, flag)));
            }
            values.push((flag, inline.to_string()));
            continue;
        }
        if argument.starts_with('-') {
            if spec.switches.contains(&argument.as_str()) {
                switches.push(argument.as_str());
                continue;
            }
            if !spec.values.contains(&argument.as_str()) {
                return Err(ParseError::Bad(unknown_flag(spec, argument)));
            }
            let value = rest.next().ok_or_else(|| ParseError::Bad(format!("{argument} needs a value")))?;
            values.push((argument.as_str(), value.clone()));
            continue;
        }
        if positional.len() >= spec.max_positional {
            return Err(ParseError::Bad(format!("unexpected extra argument {argument}; this command takes {}", spec.positional)));
        }
        positional.push(argument.clone());
    }

    let root = flag(&values, "--root").map(PathBuf::from);
    let json = switches.contains(&"--json");
    let whole = |name: &str| -> Result<Option<i64>, ParseError> {
        match flag(&values, name) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<i64>()
                .map(Some)
                .map_err(|_| ParseError::Bad(format!("{name} must be a whole number, got {raw}"))),
        }
    };
    let count = |name: &str| whole(name).map(|v| v.map(|v| v.max(0) as usize));

    Ok(match name.as_str() {
        "serve" => Command::Serve { root, host: flag(&values, "--host").map(String::from), port: whole("--port")?.map(|p| p.clamp(0, 65_535)) },
        "doctor" => Command::Doctor { root },
        "status" => Command::Status { root, json },
        "search" => {
            let range = window(&values)?;
            Command::Search {
                root,
                keywords: positional.join(" "),
                range,
                exclude: flag(&values, "--exclude").map(String::from),
                limit: count("--limit")?,
                offset: count("--offset")?,
                json,
            }
        }
        "around" => Command::Around {
            root,
            moment: one(&positional, spec.positional)?,
            window: whole("--window")?.unwrap_or(120),
            limit: count("--limit")?,
            max_text: count("--max-text")?,
            json,
        },
        "app-usage" => Command::AppUsage { root, range: window(&values)?, limit: count("--limit")?, json },
        "day-summary" => Command::DaySummary { root, date: one(&positional, spec.positional)?, limit: count("--limit")?, json },
        "frame" => Command::Frame { root, moment: one(&positional, spec.positional)?, window: whole("--window")?.unwrap_or(900), json },
        "summaries-pending" => Command::SummariesPending {
            root,
            range: window(&values)?,
            include: flag(&values, "--include").map(String::from),
            max_text: count("--max-text")?,
            json,
        },
        "summaries-read" => Command::SummariesRead { root, range: window(&values)?, kind: flag(&values, "--kind").map(String::from), json },
        "prompts-read" => Command::PromptsRead { root, json },
        "period-summary-write" => Command::PeriodSummaryWrite {
            root,
            segment: one(&positional, spec.positional)?,
            day: flag(&values, "--day").map(|day| checked_day(day)).transpose()?,
            body: body(&values, name)?,
            written_by: flag(&values, "--written-by").map(String::from),
            model: flag(&values, "--model").map(String::from),
            json,
        },
        "day-summary-write" => Command::DaySummaryWrite {
            root,
            date: one(&positional, spec.positional)?,
            body: body(&values, name)?,
            allow_partial: switches.contains(&"--allow-partial"),
            written_by: flag(&values, "--written-by").map(String::from),
            model: flag(&values, "--model").map(String::from),
            json,
        },
        other => return Err(ParseError::Unknown(other.to_string())),
    })
}

/// A paragraph to file, from exactly one of the two flags that carry one.
///
/// An empty `--text ""` is accepted and means what it says: the tool stores an empty paragraph, which is
/// a different answer from having written none. Refusing it here would be the CLI inventing a rule the
/// storage layer deliberately does not have.
fn body(values: &[(&str, String)], command: &str) -> Result<Written, ParseError> {
    match (flag(values, "--text"), flag(values, "--text-file")) {
        (Some(text), None) => Ok(Written::Inline(text.to_string())),
        (None, Some(path)) => Ok(Written::FromFile(PathBuf::from(path))),
        (None, None) => Err(ParseError::Bad(format!(
            "{command} writes a paragraph and was not given one: --text \"...\" or --text-file PATH. \
             An intentionally empty summary is --text \"\""
        ))),
        (Some(_), Some(_)) => Err(ParseError::Bad("give --text or --text-file, not both; one of them would be silently ignored".to_string())),
    }
}

trait StripBin {
    fn strip_prefix_bin(&self) -> &[String];
}

impl StripBin for [String] {
    /// `main` hands this the raw `std::env::args()`, whose first entry is the executable path.
    /// Stripping it here rather than at the call site is what lets the tests below read like a
    /// terminal session instead of like an argv index.
    fn strip_prefix_bin(&self) -> &[String] {
        match self.first() {
            Some(program) if program.ends_with("windmcp.exe") || program.ends_with("windmcp") => &self[1..],
            _ => self,
        }
    }
}

fn flag<'a>(values: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    values.iter().find(|(have, _)| *have == name).map(|(_, value)| value.as_str())
}

/// `--from`/`--to`, or `--day`, which is the thing people get wrong at 1am.
///
/// Both ends of an explicit range are demanded because a history tool with no lower bound is a
/// full-table scan of somebody's whole screen record, and an agent cannot be trusted to know that a
/// month of OCR text does not fit in its context. A `--day` needs no bounds from the user precisely
/// because it is bounded: it is one product day, and which one is the install's config's business,
/// not this file's — see [`Range::Day`].
fn window(values: &[(&str, String)]) -> Result<Range, ParseError> {
    match (flag(values, "--day"), (flag(values, "--from"), flag(values, "--to"))) {
        (Some(day), (None, None)) => Ok(Range::Day(checked_day(day)?)),
        (Some(_), _) => Err(ParseError::Bad("--day cannot be combined with --from/--to".to_string())),
        (None, (Some(from), Some(to))) => Ok(Range::Between(from.to_string(), to.to_string())),
        (None, (None, None)) => Err(ParseError::Bad(
            "--from and --to are required (unbounded scans are not offered); --day 2026-09-21 is the shortcut".to_string(),
        )),
        (None, (Some(_), None)) | (None, (None, Some(_))) => {
            Err(ParseError::Bad("both --from and --to are required; one of them is not a range".to_string()))
        }
    }
}

/// The one `--day` spelling check, used by every command that takes the flag — including
/// `period-summary-write`, where a malformed day would otherwise decide *which day's file a paragraph
/// is filed under*.
///
/// Checked against the same parser every other date in the workspace uses, and with the same
/// `YYYY-MM-DD`-only shape `windcapctl`'s `--day` demands, so one typo is refused the one way by both
/// binaries instead of being answered two ways.
fn checked_day(day: &str) -> Result<String, ParseError> {
    let day = day.trim();
    let shaped = day.len() == 10 && LocalParts::from_date(day).is_some();
    if !shaped {
        return Err(ParseError::Bad(format!("invalid --day '{day}': expected YYYY-MM-DD")));
    }
    Ok(day.to_string())
}

fn one(positional: &[String], what: &str) -> Result<String, ParseError> {
    match positional.len() {
        1 => Ok(positional[0].clone()),
        0 => Err(ParseError::Bad(format!("{what} is required"))),
        _ => Err(ParseError::Bad(format!("exactly one {what} is expected"))),
    }
}

/// The near-miss case worth a specific sentence: somebody will try to pass the token on the command
/// line exactly once, and "unknown flag; accepts --root, --host, --port" is a reply that invites them
/// to try a spelling. The answer has to be *why not*.
fn unknown_flag(spec: &Spec, given: &str) -> String {
    if matches!(
        given.to_ascii_lowercase().as_str(),
        "--token" | "--bearer" | "--auth-token" | "--api-key" | "--key" | "--no-auth" | "--insecure" | "--no-token"
    ) {
        return format!(
            "{given} is not accepted, on purpose: a command line is readable by every process on \
             this machine. Put the value in userdata/config_user.json as mcp_server_token instead."
        );
    }
    let _ = spec;
    format!("unknown flag {given} for this command")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn every_documented_command_parses() {
        assert_eq!(parse(&argv(&["serve"])), Ok(Command::Serve { root: None, host: None, port: None }));
        assert!(parse(&argv(&["doctor", "--root", "X:/install"])).is_ok());
        assert!(parse(&argv(&["status"])).is_ok());
        assert!(parse(&argv(&["search", "ffmpeg", "install", "--from", "2026-09-01", "--to", "2026-09-30"])).is_ok());
        assert!(parse(&argv(&["around", "1790025372"])).is_ok());
        assert!(parse(&argv(&["app-usage", "--from", "2026-09-01", "--to", "2026-09-02"])).is_ok());
        assert!(parse(&argv(&["day-summary", "2026-09-21"])).is_ok());
        assert!(parse(&argv(&["frame", "2026-09-21 21:16:12"])).is_ok());
        assert!(parse(&argv(&["summaries-pending", "--day", "2026-09-21", "--include", "all", "--max-text", "400"])).is_ok());
        assert!(parse(&argv(&["summaries-pending", "--from", "2026-09-01", "--to", "2026-09-02"])).is_ok());
        assert!(parse(&argv(&["summaries-read", "--day", "2026-09-21", "--kind", "daily"])).is_ok());
        assert!(parse(&argv(&["prompts-read"])).is_ok());
        assert!(parse(&argv(&["period-summary-write", "2026-09-21_09-00-00", "--text", "a paragraph"])).is_ok());
        assert!(parse(&argv(&["period-summary-write", "1790025372", "--day", "2026-09-21", "--text-file", "X:/notes/one.md"])).is_ok());
        assert!(parse(&argv(&["day-summary-write", "2026-09-21", "--text", "the day", "--allow-partial", "--written-by", "codex", "--model", "gpt-x"])).is_ok());
    }

    /// The writers take a paragraph and nothing else: no cap, no shape, no guessing at an empty one.
    /// An omitted body is the one mistake this grammar refuses, because a writer that ran with no text
    /// would file an empty summary the user never meant.
    #[test]
    fn a_writer_without_a_paragraph_is_refused_and_an_empty_one_is_not() {
        let error = parse(&argv(&["period-summary-write", "2026-09-21_09-00-00"])).unwrap_err();
        assert!(matches!(&error, ParseError::Bad(m) if m.contains("--text")), "{error:?}");
        assert!(parse(&argv(&["day-summary-write", "2026-09-21", "--text", ""])).is_ok(), "an intentionally empty paragraph is a real answer");
        let Command::DaySummaryWrite { body, .. } = parse(&argv(&["day-summary-write", "2026-09-21", "--text", ""])).unwrap() else {
            panic!("wrong command")
        };
        assert_eq!(body.read().as_deref(), Ok(""), "and it reaches the tool as the empty string");
        assert!(parse(&argv(&["day-summary-write", "2026-09-21", "--text", "a", "--text-file", "b"])).is_err(), "one of the two, not both");
    }

    /// `--text-file` is a path in the grammar and only a path: parsing opens nothing, which is what
    /// keeps the whole of this file exercisable without an install, and what lets `body.read()` be the
    /// single place a paragraph is ever read.
    #[test]
    fn the_grammar_reads_no_file_and_no_config_even_for_a_write() {
        let Command::PeriodSummaryWrite { body, segment, day, .. } =
            parse(&argv(&["period-summary-write", "2026-09-21_09-00-00", "--text-file", "Z:/nowhere/at/all.md"])).unwrap()
        else {
            panic!("wrong command")
        };
        assert_eq!((segment.as_str(), day.as_deref()), ("2026-09-21_09-00-00", None));
        let error = body.read().unwrap_err();
        assert!(error.contains("nowhere/at/all.md"), "the refusal names the file it could not open: {error}");
    }

    /// The flag belongs to the one command whose gate it can downgrade. On the stretch writer it would
    /// be a user believing they had narrowed something that was never there.
    #[test]
    fn allow_partial_is_the_daily_writers_flag_alone() {
        assert!(parse(&argv(&["period-summary-write", "2026-09-21_09-00-00", "--text", "x", "--allow-partial"])).is_err());
        let Command::DaySummaryWrite { allow_partial, .. } =
            parse(&argv(&["day-summary-write", "2026-09-21", "--text", "x", "--allow-partial"])).unwrap() else {
            panic!("wrong command")
        };
        assert!(allow_partial);
        assert!(!matches!(
            parse(&argv(&["day-summary-write", "2026-09-21", "--text", "x"])).unwrap(),
            Command::DaySummaryWrite { allow_partial: true, .. }
        ), "and it is off unless typed");
    }

    /// A `--day` decides which product day's file a paragraph is filed under, so a typo in it is
    /// refused the same way here as in `search` and in `windcapctl query`.
    #[test]
    fn a_writers_day_is_the_one_spelling_every_command_accepts() {
        assert!(parse(&argv(&["period-summary-write", "1790025372", "--day", "2026-9-21", "--text", "x"])).is_err());
        assert!(parse(&argv(&["period-summary-write", "1790025372", "--day", "2026-09-21", "--text", "x"])).is_ok());
        assert!(parse(&argv(&["summaries-read", "--kind", "daily"])).is_err(), "a range needs both ends here as elsewhere");
    }

    #[test]
    fn every_summary_command_is_on_the_usage_screen() {
        for line in [
            "summaries-pending",
            "summaries-read",
            "prompts-read",
            "period-summary-write",
            "day-summary-write",
            "--text-file",
            "--allow-partial",
        ] {
            assert!(USAGE.contains(line), "{line} missing from the usage screen");
        }
        assert!(!USAGE.contains("read-only"), "the bridge writes summaries now; the screen must not promise otherwise:\n{USAGE}");
    }

    /// The rule this whole file is written around.
    #[test]
    fn a_token_or_an_auth_bypass_on_the_command_line_is_refused_with_a_reason() {
        for flag in ["--token", "--bearer", "--auth-token", "--api-key", "--no-auth", "--no-token", "--insecure"] {
            let error = parse(&argv(&["serve", flag, "s3cret-value-not-in-any-log"])).unwrap_err();
            assert!(matches!(error, ParseError::Bad(_)), "{flag} produced {error:?}, expected a refusal");
            let message = error.message();
            assert!(message.contains("config_user.json"), "{flag}: {message}");
            assert!(!message.contains("s3cret"), "the refusal must not echo the value back");
            assert!(!message.contains("Bearer"), "nor describe the header, which is in the usage screen");
        }
    }

    #[test]
    fn an_unknown_flag_is_an_error_not_a_shrug() {
        // `--form` for `--from` in a history tool would otherwise silently mean "no bounds".
        let error = parse(&argv(&["search", "kw", "--form", "2026-09-22", "--to", "2026-09-23"])).unwrap_err();
        assert!(matches!(error, ParseError::Bad(_)), "{error:?}");
        assert!(error.message().contains("--form"), "{}", error.message());
    }

    #[test]
    fn an_unknown_command_says_so() {
        assert_eq!(parse(&argv(&["nope"])), Err(ParseError::Unknown("nope".to_string())));
    }

    #[test]
    fn help_and_nothing_at_all_are_the_usage_screen() {
        for word in ["-h", "--help", "help"] {
            assert_eq!(parse(&argv(&[word])), Err(ParseError::Help));
        }
        assert_eq!(parse(&argv(&[])), Err(ParseError::Help));
        // The program name is stripped, so an argv straight from `main` parses the same as the words.
        assert_eq!(parse(&argv(&["C:\\bin\\windmcp.exe", "status"])), Ok(Command::Status { root: None, json: false }));
    }

    /// `--version` used to be answered with the usage screen, which meant a user asking "which
    /// build is this" got a page about what it can do. It is its own answer now, and it is given
    /// without a root, a config, a token or a bound port.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windmcp "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        for spelling in ["--version", "-V", "version"] {
            assert_eq!(parse(&argv(&[spelling])), Err(ParseError::Version), "{spelling}");
            assert_eq!(parse(&argv(&["C:\\bin\\windmcp.exe", spelling])), Err(ParseError::Version), "{spelling}");
        }
        // The message a `Display` of the error produces is the line itself, not the usage screen.
        assert_eq!(ParseError::Version.message(), format!("{}\n", line));
        assert!(USAGE.contains("--version"), "answered but undocumented:\n{USAGE}");
    }

    #[test]
    fn a_range_needs_both_ends() {
        assert!(matches!(parse(&argv(&["search", "kw"])), Err(ParseError::Bad(m)) if m.contains("--from and --to")));
        assert!(matches!(parse(&argv(&["search", "kw", "--from", "2026-09-01"])), Err(ParseError::Bad(m)) if m.contains("both")));
        assert!(matches!(parse(&argv(&["search", "kw", "--day", "2026-09-01", "--from", "x"])), Err(ParseError::Bad(m)) if m.contains("--day")));
        assert!(matches!(parse(&argv(&["app-usage"])), Err(ParseError::Bad(m)) if m.contains("--from and --to")));
    }

    /// The grammar hands a day over unresolved, on purpose: deciding what a day *is* needs the
    /// install's `day_begin_minutes`, and a calendar day computed in this file is the bug that made
    /// `--day` mean one thing to `windmcp` and another to `windcapctl`.
    #[test]
    fn one_day_is_carried_as_the_word_the_user_typed_not_as_a_calendar_day() {
        let Command::Search { keywords, range, .. } = parse(&argv(&["search", "kw", "--day", "2026-09-21"])).unwrap() else {
            panic!("wrong command")
        };
        assert_eq!(keywords, "kw");
        assert_eq!(range, Range::Day("2026-09-21".to_string()));
        // Neither endpoint of a calendar day appears anywhere in what the parser produced.
        let rendered = format!("{range:?}");
        assert!(!rendered.contains("00:00:00") && !rendered.contains("23:59:59"), "{rendered} resolved a day in the grammar");

        let Command::AppUsage { range, .. } = parse(&argv(&["app-usage", "--day", "2026-09-21"])).unwrap() else {
            panic!("wrong command")
        };
        assert_eq!(range, Range::Day("2026-09-21".to_string()));

        let Command::Search { range, .. } = parse(&argv(&["search", "kw", "--from", "2026-09-21 08:00", "--to", "2026-09-21 09:00"])).unwrap()
        else {
            panic!("wrong command")
        };
        assert_eq!(range, Range::Between("2026-09-21 08:00".to_string(), "2026-09-21 09:00".to_string()));
    }

    /// `--day` is spelled the same way in both binaries and refused the same way, or the two tools
    /// disagree about what a typo means as well as about what a day means.
    #[test]
    fn a_malformed_day_is_a_usage_error_before_any_file_is_opened() {
        for bad in ["2026-9-1", "2026-13-01", "2025-02-29", "yesterday", "2026/09/21", "", "2026-09-21_00-00-00"] {
            let error = parse(&argv(&["app-usage", "--day", bad])).unwrap_err();
            assert!(matches!(&error, ParseError::Bad(m) if m.contains("invalid --day")), "{bad:?} produced {error:?}");
        }
        assert!(parse(&argv(&["app-usage", "--day", "2024-02-29"])).is_ok(), "a leap day is a real day");
    }

    #[test]
    fn positionals_are_joined_into_the_keyword_string_and_bounded_elsewhere() {
        let Command::Search { keywords, .. } = parse(&argv(&["search", "read", "me", "--from", "a", "--to", "b"])).unwrap() else {
            panic!("expected search")
        };
        assert_eq!(keywords, "read me");
        let error = parse(&argv(&["around", "1", "2"])).unwrap_err().message();
        assert!(error.contains("unexpected extra argument"), "{error}");
        assert!(error.contains("<timestamp>"), "{error}");
        assert!(matches!(parse(&argv(&["around"])), Err(ParseError::Bad(m)) if m.contains("required")));
        assert!(matches!(parse(&argv(&["status", "extra"])), Err(ParseError::Bad(_))));
    }

    #[test]
    fn equals_and_separate_forms_agree() {
        assert_eq!(parse(&argv(&["serve", "--port", "21999"])), parse(&argv(&["serve", "--port=21999"])));
        assert!(matches!(parse(&argv(&["serve", "--port", "abc"])), Err(ParseError::Bad(m)) if m.contains("--port")));
        assert_eq!(parse(&argv(&["doctor", "--root=X:/a"])), parse(&argv(&["doctor", "--root", "X:/a"])));
    }

    #[test]
    fn flags_that_take_a_value_are_not_swallowed_by_a_positional() {
        let Command::Around { moment, window, limit, max_text, json, .. } =
            parse(&argv(&["around", "1790025372", "--window", "30", "--limit", "5", "--json"])).unwrap() else {
            panic!("expected around")
        };
        assert_eq!((moment.as_str(), window, limit, max_text, json), ("1790025372", 30, Some(5), None, true));
    }

    #[test]
    fn a_trailing_flag_with_no_value_is_reported() {
        assert!(matches!(parse(&argv(&["serve", "--host"])), Err(ParseError::Bad(m)) if m.contains("needs a value")));
    }

    #[test]
    fn the_usage_screen_names_the_config_file_that_holds_the_secret() {
        assert!(USAGE.contains("config_user.json"));
        assert!(USAGE.contains("enable_mcp_server"));
    }
}
