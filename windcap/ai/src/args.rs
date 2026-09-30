//! The `windai` argument grammar, as data rather than as control flow.
//!
//! Same principle as `windcapctl`: an unknown or misspelled flag is refused loudly instead of being
//! ignored. The stakes here are a little different, though — a silent default in a *search* tool means
//! the user reads an unrelated set of hits as the answer to their question, and a flag that accepts the
//! API key would be a feature this tool must not have, so the grammar's rejection path is part of the
//! security story rather than just politeness.

use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Search(SearchArgs),
    Tags(TagsArgs),
    Summarize(SummarizeArgs),
    Prompts(PromptsArgs),
    Doctor(DoctorArgs),
}

/// `windai summarize`, as data.
///
/// `day` and `pending` are mutually exclusive, and that is checked here rather than inside `run`, because
/// a silent precedence rule between two things a user typed in the same command is how a tool ends up
/// summarising the wrong week while reporting success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizeArgs {
    /// One product day, `YYYY-MM-DD`.
    pub day: Option<String>,
    /// Up to this many days with outstanding work, scanned back from today.
    pub pending: Option<usize>,
    /// At most this many stretches asked for in the whole run.
    pub limit: Option<usize>,
    pub force: bool,
    pub allow_partial: bool,
    pub dry_run: bool,
    pub root: Option<PathBuf>,
}

/// `windai prompts`: the door onto the text that gets sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptsArgs {
    /// Print one template's effective text instead of the table.
    pub show: Option<String>,
    /// Delete one override so the shipped words answer again.
    pub restore: Option<String>,
    /// Print where the two sets of files live, and exit.
    pub path: bool,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchArgs {
    pub phrase: String,
    pub root: Option<PathBuf>,
    /// Print the derived query — keywords, exclusions, the date span, and every clamp the port applied
    /// — before the hits. This is the flag that makes the mapping inspectable rather than magical.
    pub explain: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagsArgs {
    pub month: Month,
    pub root: Option<PathBuf>,
    /// Build the title table and show what would be sent, without sending or writing. The only way to
    /// see what a month would cost, and the only path through this feature that needs no key.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorArgs {
    pub root: Option<PathBuf>,
}

/// A `YYYY-MM` month, validated at the boundary rather than deep in the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Month {
    pub year: i64,
    pub month: u32,
}

impl Month {
    pub fn stamp(self) -> String {
        format!("{:04}-{:02}", self.year, self.month)
    }
}

impl std::str::FromStr for Month {
    type Err = String;
    fn from_str(text: &str) -> Result<Month, String> {
        let error = || format!("`{text}` is not a month; expected YYYY-MM, for example 2026-09");
        let parts: Vec<&str> = text.trim().split('-').collect();
        if parts.len() != 2 || parts[0].len() != 4 || parts[1].is_empty() || parts[1].len() > 2 {
            return Err(error());
        }
        let year: i64 = parts[0].parse().map_err(|_| error())?;
        let month: u32 = parts[1].parse().map_err(|_| error())?;
        // The day count comes from the workspace's own calendar so that February 30 is refused in
        // exactly one place.
        if !(1971..=2999).contains(&year) || month > 12 {
            return Err(error());
        }
        Ok(Month { year, month })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Help,
    /// `--version`/`-V`: which binary, which version, which build. Its own answer rather than
    /// another spelling of `Help`, because the usage screen says what this tool can do and this
    /// says what it *is* — and because it is the one reply that needs no root, no config and no
    /// API key, which is the state a user asking about a broken install is actually in.
    Version,
    /// No command given.
    Empty,
    /// `--api-key` and friends: the grammar refuses a key on the command line rather than accepting it
    /// and telling the user not to use it.
    Forbidden(String),
    Unknown(String),
    ExpectedValue(String),
    BadArgument(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Help => write!(f, "usage"),
            ParseError::Version => write!(f, "{}", version_line()),
            ParseError::Empty => write!(f, "no command given"),
            ParseError::Forbidden(name) => {
                write!(f, "{name} is not accepted: the API key is read from userdata/config_user.json only")
            }
            ParseError::Unknown(name) => write!(f, "unknown option {name}"),
            ParseError::ExpectedValue(name) => write!(f, "{name} needs a value"),
            ParseError::BadArgument(what) => write!(f, "{what}"),
        }
    }
}

/// Flags that take a value, per command. Anything not listed is either a switch or a mistake.
fn takes_value(command: &str, name: &str) -> bool {
    match (command, name) {
        (_, "--root")
        | (_, "--limit")
        | ("tags", "--month")
        | ("summarize", "--day")
        | ("summarize", "--pending")
        | ("prompts", "--show")
        | ("prompts", "--restore") => true,
        _ => false,
    }
}

fn is_switch(command: &str, name: &str) -> bool {
    matches!(
        (command, name),
        (_, "--help")
            | ("search", "--explain")
            | ("tags", "--dry-run")
            | ("summarize", "--dry-run")
            | ("summarize", "--force")
            | ("summarize", "--allow-partial")
            | ("prompts", "--path")
    )
}

/// An option name that must never appear, whatever the command.
///
/// Checked before the unknown-option path, and with its own message, so that `--api-key sk-…` is
/// explained as a policy rather than reported as a typo the user should retry with a different spelling.
fn forbidden(name: &str) -> bool {
    const NEEDLES: [&str; 5] = ["key", "token", "secret", "apikey", "auth"];
    let lowered = name.to_ascii_lowercase();
    lowered.starts_with("--") && NEEDLES.iter().any(|needle| lowered.contains(needle))
}

pub fn parse(argv: &[String]) -> Result<Command, ParseError> {
    let Some(command) = argv.first().map(|s| s.as_str()) else { return Err(ParseError::Empty) };
    // Ahead of the command match below, so the answer needs no subcommand, no `--root`, no config
    // and no key. Note it is also ahead of `forbidden`, which `--version` would not trip anyway.
    if wind_base::version::is_flag(command) {
        return Err(ParseError::Version);
    }
    if command == "--help" || command == "help" {
        return Err(ParseError::Help);
    }
    let rest = &argv[1..];
    match command {
        "search" => parse_search(rest),
        "tags" => parse_tags(rest),
        "summarize" => parse_summarize(rest),
        "prompts" => parse_prompts(rest),
        "doctor" => parse_doctor(rest),
        other if other.starts_with('-') => Err(ParseError::Unknown(other.to_string())),
        other => Err(ParseError::Unknown(format!("`{other}`"))),
    }
}

fn collect(
    command: &str,
    rest: &[String],
) -> Result<(Vec<String>, std::collections::BTreeMap<String, String>, Vec<String>), ParseError> {
    let mut positionals = Vec::new();
    let mut flags: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut switches = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let token = &rest[index];
        if token == "--help" {
            return Err(ParseError::Help);
        }
        if let Some(name) = token.strip_prefix("--") {
            let name = format!("--{name}");
            if forbidden(&name) {
                return Err(ParseError::Forbidden(name));
            }
            if takes_value(command, &name) {
                let value = rest.get(index + 1).ok_or_else(|| ParseError::ExpectedValue(name.clone()))?;
                if value.starts_with("--") {
                    return Err(ParseError::ExpectedValue(name));
                }
                flags.insert(name, value.clone());
                index += 2;
                continue;
            }
            if is_switch(command, &name) {
                if switches.contains(&name) {
                    return Err(ParseError::BadArgument(format!("{name} given twice")));
                }
                switches.push(name);
                index += 1;
                continue;
            }
            return Err(ParseError::Unknown(name));
        }
        positionals.push(token.clone());
        index += 1;
    }
    Ok((positionals, flags, switches))
}

fn root_of(flags: &std::collections::BTreeMap<String, String>) -> Option<PathBuf> {
    flags.get("--root").map(PathBuf::from)
}

fn parse_search(rest: &[String]) -> Result<Command, ParseError> {
    let (positionals, flags, switches) = collect("search", rest)?;
    if positionals.len() != 1 {
        return Err(ParseError::BadArgument(format!(
            "search needs exactly one phrase in quotes, got {}",
            if positionals.is_empty() { "none".to_string() } else { positionals.join(" + ") }
        )));
    }
    let phrase = positionals[0].trim().to_string();
    if phrase.is_empty() {
        return Err(ParseError::BadArgument("the phrase is empty after trimming".to_string()));
    }
    let limit = match flags.get("--limit") {
        Some(text) => usize::from_str(text).map_err(|_| ParseError::BadArgument(format!("--limit {text} is not a count")))?.max(1),
        None => 20,
    };
    if limit > 1000 {
        return Err(ParseError::BadArgument("--limit above 1000 prints nothing useful".to_string()));
    }
    Ok(Command::Search(SearchArgs {
        phrase,
        root: root_of(&flags),
        explain: switches.contains(&"--explain".to_string()),
        limit,
    }))
}

fn parse_tags(rest: &[String]) -> Result<Command, ParseError> {
    let (positionals, flags, switches) = collect("tags", rest)?;
    if !positionals.is_empty() {
        return Err(ParseError::BadArgument(format!("tags takes no free argument, got {}", positionals.join(" "))));
    }
    let text = flags.get("--month").ok_or(ParseError::BadArgument("tags needs --month YYYY-MM".to_string()))?;
    let month = Month::from_str(text).map_err(ParseError::BadArgument)?;
    Ok(Command::Tags(TagsArgs { month, root: root_of(&flags), dry_run: switches.contains(&"--dry-run".to_string()) }))
}

fn parse_summarize(rest: &[String]) -> Result<Command, ParseError> {
    let (positionals, flags, switches) = collect("summarize", rest)?;
    if !positionals.is_empty() {
        return Err(ParseError::BadArgument(format!("summarize takes no free argument, got {}", positionals.join(" "))));
    }
    let day = flags.get("--day").cloned();
    let pending = match flags.get("--pending") {
        Some(text) => Some(usize::from_str(text).map_err(|_| ParseError::BadArgument(format!("--pending {text} is not a count")))?),
        None => None,
    };
    if day.is_some() && pending.is_some() {
        return Err(ParseError::BadArgument(
            "`--day` names one day and `--pending` asks for whichever days still have work; give one or the other".to_string(),
        ));
    }
    Ok(Command::Summarize(SummarizeArgs {
        day,
        pending,
        limit: count(&flags, "--limit")?,
        force: switches.contains(&"--force".to_string()),
        allow_partial: switches.contains(&"--allow-partial".to_string()),
        dry_run: switches.contains(&"--dry-run".to_string()),
        root: root_of(&flags),
    }))
}

fn parse_prompts(rest: &[String]) -> Result<Command, ParseError> {
    let (positionals, flags, switches) = collect("prompts", rest)?;
    if !positionals.is_empty() {
        return Err(ParseError::BadArgument(format!("prompts takes no free argument, got {}", positionals.join(" "))));
    }
    Ok(Command::Prompts(PromptsArgs {
        show: flags.get("--show").cloned(),
        restore: flags.get("--restore").cloned(),
        path: switches.contains(&"--path".to_string()),
        root: root_of(&flags),
    }))
}

/// An optional positive-integer flag, with the same refusal as every other misspelled value here.
fn count(flags: &std::collections::BTreeMap<String, String>, name: &str) -> Result<Option<usize>, ParseError> {
    match flags.get(name) {
        None => Ok(None),
        Some(text) => match text.parse::<usize>() {
            Ok(value) if value > 0 => Ok(Some(value)),
            _ => Err(ParseError::BadArgument(format!("{name} {text} is not a count of one or more"))),
        },
    }
}

fn parse_doctor(rest: &[String]) -> Result<Command, ParseError> {
    let (positionals, flags, _) = collect("doctor", rest)?;
    if !positionals.is_empty() {
        return Err(ParseError::BadArgument(format!("doctor takes no free argument, got {}", positionals.join(" "))));
    }
    Ok(Command::Doctor(DoctorArgs { root: root_of(&flags) }))
}

/// The help text, inline rather than in a sibling file: this crate owns exactly `Cargo.toml` and
/// `src/*.rs`, and a text asset would be a fourth thing to keep in sync with the grammar above.
pub fn usage() -> String {
    USAGE.to_string()
}

/// What `windai --version` answers with: `<binary> <package version> (<debug|release>)`.
///
/// The format is `wind_base::version`'s so the eleven binaries in one zip cannot each invent a way of
/// describing themselves; the name and the `env!` are this crate's, so the number is the version
/// these bytes were built from. Nothing here reaches a config file or an API key.
pub fn version_line() -> String {
    wind_base::version::line("windai", env!("CARGO_PKG_VERSION"))
}

const USAGE: &str = "\
windai — Windrecorder's AI features, from a terminal

USAGE
    windai search \"<phrase>\"  [--root PATH] [--explain] [--limit N]
    windai tags --month YYYY-MM [--root PATH] [--dry-run]
    windai summarize [--day YYYY-MM-DD | --pending N] [--limit N]
                     [--force] [--allow-partial] [--dry-run] [--root PATH]
    windai prompts [--show NAME | --restore NAME | --path]  [--root PATH]
    windai doctor               [--root PATH]
    windai --version | -V       which binary, which version, which build

COMMANDS
    search    Ask the model to turn one sentence into a structured query, run that query
              against the monthly index, and print the hits. The phrase is one argument:
              quote it. Nothing from the index is sent to the model — only the phrase.
    tags      Ask the model what a month of foreground window titles adds up to, and cache
              the tags in userdata/result_ai_extract_tag/<year>.json, keyed by the month and
              by a fingerprint of the titles, so re-running an unchanged month costs nothing.
    summarize Summarise what was on screen, stretch by stretch and then day by day, and cache
              the paragraphs in userdata/result_ai_period_summary/<day>.json and
              userdata/result_ai_daily_summary/<day>.json. THIS ONE SENDS YOUR SCREEN TEXT to
              the endpoint in open_ai_base_url: the tagger sends window titles only, this sends
              the captured words, because a summary of a stretch that was never read is not a
              summary. Re-running costs nothing where nothing changed — a stretch whose text and
              whose prompt are both unchanged is not asked about again, whoever wrote it last. A
              day's own summary is refused until every stretch of that day has one that stands,
              unless --allow-partial says otherwise, and then it is stored as partial.
    prompts   The text this binary would send: seven templates, which copy is in force, where
              both live. Show one, or restore the shipped words. These are files, not settings —
              the settings screen edits the same two paths, so what you read here is what runs.
    doctor    Report the endpoint, the model, and whether a key is configured — never the
              key itself — and make one round trip if one is.

OPTIONS
    --root PATH   The Windrecorder install to work on. Defaults to the install this binary
                  was launched from.
    --explain     Print the derived query (keywords, exclusions, date span, window-title
                  filter) and every correction applied to the model's answer, before the hits.
    --dry-run     tags only: build the title table and show what would be sent, without
                  sending it or writing anything.
    --day DATE    summarize only: exactly one product day, YYYY-MM-DD, as the install counts it
                  (03:00 by default). A date the calendar does not have is refused.
    --pending N   summarize only: the N days with outstanding work, scanning back from today as
                  far as 60 days. With neither --day nor --pending: one day, today's product day.
    --force       summarize only: re-ask stretches that already have a summary that stands — the
                  way to regenerate a day after rewriting its prompt, without deleting files.
    --allow-partial
                  summarize only: write a day's summary over gaps and record that it was done that
                  way. The daily gate refuses without it, which is the point of the gate.
    --show NAME   prompts only: print one template's effective text, verbatim.
    --restore NAME
                  prompts only: delete that override so the shipped text answers again. Nothing
                  else is written, and the shipped file is never touched.
    --path        prompts only: print the two directories and exit.
    --limit N     search: how many hits to print, default 20 (`max_page_result`). summarize: at
                  most N stretches asked for in this run, which is how a big day is tried cheaply.
    --version     This binary's name, its package version and its build profile, then exit. Reads
                  no config and no index, so it answers even with no key and no library. -V works.
    --help        This text.

CONFIGURATION
    Read from <root>/userdata/config_user.json, over <root>/config_src/
    config_default.json: open_ai_base_url, open_ai_api_key, open_ai_modelname,
    ai_api_endpoint_selected, enable_ai_extract_tag, ai_extract_tag_wintitle_limit,
    ai_extract_max_tag_num, ai_extract_tag_in_idle_batch_size, ai_extract_tag_filter_words,
    ai_extract_tag_result_dir, exclude_words.

    There is deliberately no --api-key, and no OPENAI_API_KEY: the key is read from
    config_user.json and from nowhere else, because a command-line argument is readable by
    every other process on the machine and an environment variable is inherited by every
    child process this app starts.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_search_phrase_is_one_quoted_argument() {
        let command = parse(&argv(&["search", "那封关于续约的邮件", "--root", "C:\\x"])).unwrap();
        let Command::Search(args) = command else { panic!("wrong command {command:?}") };
        assert_eq!(args.phrase, "那封关于续约的邮件");
        assert_eq!(args.root, Some(PathBuf::from("C:\\x")));
        assert!(!args.explain);
        assert_eq!(args.limit, 20);
    }

    #[test]
    fn explain_and_limit_are_per_command() {
        let Command::Search(args) = parse(&argv(&["search", "x", "--explain", "--limit", "5"])).unwrap() else {
            panic!()
        };
        assert!(args.explain);
        assert_eq!(args.limit, 5);
        assert!(matches!(parse(&argv(&["search", "--explain"])), Err(ParseError::BadArgument(_))));
        assert!(matches!(parse(&argv(&["tags", "--explain", "--month", "2026-09"])), Err(ParseError::Unknown(_))));
    }

    #[test]
    fn a_key_on_the_command_line_is_refused_as_policy_not_as_a_typo() {
        for name in ["--api-key", "--key", "--openai-key", "--token", "--secret"] {
            let error = parse(&argv(&["search", "x", name, "sk-literal"])).expect_err("{name} must be refused");
            assert!(matches!(error, ParseError::Forbidden(_)), "{name} -> {error:?}");
            assert!(error.to_string().contains("config_user.json"), "{error}");
        }
        // The value of a refused flag must not be echoed either.
        let error = parse(&argv(&["search", "x", "--api-key", "sk-literal"])).unwrap_err();
        assert!(!error.to_string().contains("sk-literal"), "{error}");
    }

    #[test]
    fn a_month_is_validated_at_the_boundary() {
        assert_eq!(Month::from_str("2026-09").unwrap(), Month { year: 2026, month: 9 });
        assert_eq!(Month::from_str("2026-1").unwrap().month, 1);
        assert_eq!(Month { year: 2026, month: 12 }.stamp(), "2026-12");
        for bad in ["2026-13", "26-09", "2026-09-01", "notamonth", "", "2026-", "-2026-09", "1900-01"] {
            assert!(Month::from_str(bad).is_err(), "{bad} parsed");
        }
    }

    #[test]
    fn tags_requires_a_month_and_rejects_free_arguments() {
        let Command::Tags(args) = parse(&argv(&["tags", "--month", "2026-08", "--dry-run"])).unwrap() else {
            panic!()
        };
        assert_eq!(args.month, Month { year: 2026, month: 8 });
        assert!(args.dry_run);
        assert!(matches!(parse(&argv(&["tags"])), Err(ParseError::BadArgument(_))));
        assert!(matches!(parse(&argv(&["tags", "--month", "2026-08", "extra"])), Err(ParseError::BadArgument(_))));
        assert!(matches!(parse(&argv(&["tags", "--month"])), Err(ParseError::ExpectedValue(_))));
        assert!(matches!(parse(&argv(&["tags", "--month", "--dry-run"])), Err(ParseError::ExpectedValue(_))));
    }

    #[test]
    fn doctor_takes_only_a_root() {
        let Command::Doctor(args) = parse(&argv(&["doctor", "--root", "."])).unwrap() else { panic!() };
        assert_eq!(args.root, Some(PathBuf::from(".")));
        assert!(matches!(parse(&argv(&["doctor"])), Ok(Command::Doctor(_))));
        assert!(matches!(parse(&argv(&["doctor", "junk"])), Err(ParseError::BadArgument(_))));
    }

    #[test]
    fn mistyped_flags_and_commands_are_refused_rather_than_defaulted() {
        // A misspelled --root falling back to "this directory" is how a tool searches the wrong history.
        assert!(matches!(parse(&argv(&["search", "x", "--rot", "y"])), Err(ParseError::Unknown(_))));
        assert!(matches!(parse(&argv(&["searh", "x"])), Err(ParseError::Unknown(_))));
        assert!(matches!(parse(&argv(&[])), Err(ParseError::Empty)));
        assert!(matches!(parse(&argv(&["--help"])), Err(ParseError::Help)));
        assert!(matches!(parse(&argv(&["search", "x", "--help"])), Err(ParseError::Help)));
        assert!(matches!(parse(&argv(&["search", "x", "--explain", "--explain"])), Err(ParseError::BadArgument(_))));
        assert!(matches!(parse(&argv(&["search", "x", "--limit", "many"])), Err(ParseError::BadArgument(_))));
        assert!(matches!(parse(&argv(&["search", "x", "--limit", "0"])), Ok(Command::Search(_))));
        assert!(matches!(parse(&argv(&["search", "   "])), Err(ParseError::BadArgument(_))));
    }

    #[test]
    fn the_usage_text_names_every_command_and_the_key_policy() {
        let text = usage();
        for needle in ["search", "tags", "doctor", "--explain", "--dry-run", "config_user.json"] {
            assert!(text.contains(needle), "usage.txt lost {needle}");
        }
    }

    /// `windai --version` is the answer a user gets when their install has no key and no index, so
    /// the grammar has to reach it before either. It is also the only `ParseError` that is not a
    /// complaint, and its `Display` is the line itself.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windai "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        assert_eq!(line.to_string(), ParseError::Version.to_string(), "Display is the line, not a complaint");
        for spelling in ["--version", "-V"] {
            assert_eq!(parse(&argv(&[spelling])), Err(ParseError::Version), "{spelling}");
        }
        // Refused as a *key* is refused: never by falling through to a default command.
        assert!(matches!(parse(&argv(&["--version", "x"])), Err(ParseError::Version)));
        for needle in ["--version", "-V"] {
            assert!(usage().contains(needle), "{needle} documented nowhere:\n{}", usage());
        }
    }
}
