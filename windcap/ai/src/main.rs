//! `windai` — the AI features from a terminal.
//!
//! Three commands, and each one is a thin reporter over a function in the library: `search` prints the
//! query the model described and then the rows it returned, `tags` prints the table that was sent and
//! the tags that came back, `doctor` prints what the configuration says and — only if a key is actually
//! configured — spends one round trip proving it.
//!
//! `main` is the only place that prints; every command body returns its report as a `String`. That is
//! the same shape as `windcapctl`, and for the same reason: the output is then assertable without a
//! terminal, which is what lets the unconfigured-install message be a tested behaviour instead of a hope.

use std::path::PathBuf;
use std::time::Instant;

use wind_ai::args::{self, Command, DoctorArgs, PromptsArgs, SearchArgs, SummarizeArgs, TagsArgs};
use wind_ai::client::Client;
use wind_ai::error::AiError;
use wind_ai::library::Index;
use wind_ai::settings::Settings;
use wind_ai::tags::Tags;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let command = match args::parse(&argv) {
        Ok(command) => command,
        Err(args::ParseError::Help) => {
            print!("{}", args::usage());
            return;
        }
        // Ahead of `open_index`, which is where every command below resolves a root and loads
        // `userdata/config_user.json`. The version is the one answer this binary gives without an
        // index, a config, a key or a network — and it is one line, not the usage screen.
        Err(args::ParseError::Version) => {
            println!("{}", args::version_line());
            return;
        }
        Err(error) => {
            eprintln!("windai: {error}\n");
            eprint!("{}", args::usage());
            std::process::exit(2);
        }
    };

    // Errors leave as text from `Faults`, which has already taken the key out of them. There is no
    // `{err:?}` of an error type holding an unredacted remote body anywhere in this binary.
    let report = match command {
        Command::Search(arguments) => search(arguments),
        Command::Tags(arguments) => tags(arguments),
        Command::Summarize(arguments) => summarize(arguments),
        Command::Prompts(arguments) => prompts(arguments),
        Command::Doctor(arguments) => doctor(arguments),
    };
    match report {
        Ok(text) => println!("{}", text.trim_end()),
        Err(error) => {
            eprintln!("windai: {error}");
            std::process::exit(1);
        }
    }
}

fn open_index(root: Option<PathBuf>) -> Result<Index, AiError> {
    let root = wind_ai::library::resolve_root(root);
    Index::open(&root)
}

/// The `summarize` command's report.
///
/// The exit code is the part the idle pass reads. A run where *everything* failed achieved nothing, and
/// `windmaint` has to be able to say so and try again later; a run where some stretches succeeded and one
/// endpoint call timed out is progress, and the queue offers the rest on the next pass anyway. So:
/// non-zero only when the run wrote nothing and something failed.
fn summarize(arguments: SummarizeArgs) -> Result<String, AiError> {
    let index = open_index(arguments.root.clone())?;
    let options = wind_ai::summarize::Options {
        day: arguments.day,
        pending: arguments.pending,
        limit: arguments.limit,
        force: arguments.force,
        allow_partial: arguments.allow_partial,
        dry_run: arguments.dry_run,
    };
    let client = Client::new(index.settings.clone());
    let report = wind_ai::summarize::run(&index, &client, &options)?;
    let mut out = wind_ai::summarize::render_report(&report);
    if options.dry_run {
        out.push_str("nothing was sent and nothing was written; drop --dry-run to do it for real\n");
    } else if report.written > 0 {
        out.push_str(&format!(
            "summaries are in {} and {}, and `windrecorder_summaries_read` shows them\n",
            index.root.join("userdata/result_ai_period_summary").display(),
            index.root.join("userdata/result_ai_daily_summary").display()
        ));
    }
    if report.failed > 0 && report.written == 0 {
        out.push_str("nothing was written; the reasons are above, and the same work is offered again next run\n");
        println!("{}", out.trim_end());
        return Err(index.faults().model(format!("{} request(s) failed", report.failed)));
    }
    Ok(out)
}

/// The `prompts` command: what would be sent, where it lives, and how to undo an edit.
fn prompts(arguments: PromptsArgs) -> Result<String, AiError> {
    let index = open_index(arguments.root.clone())?;
    let config = &index.config;
    if let Some(given) = &arguments.show {
        let found = lookup(given).ok_or_else(|| unknown_prompt(&index, given))?;
        let prompt = wind_base::prompts::read(config, found);
        let shipped = if prompt.overridden() { format!("\n# the shipped copy reads:\n{}", indent(&prompt.shipped)) } else { String::new() };
        return Ok(format!("{}\n# {} — {}{shipped}", prompt.text, found.label(), origin_line(&prompt)));
    }

    if let Some(given) = &arguments.restore {
        let found = lookup(given).ok_or_else(|| unknown_prompt(&index, given))?;
        let restored = wind_base::prompts::restore(config, found).map_err(|e| index.faults().usage(e))?;
        return Ok(if restored {
            format!("{} is back to the shipped words; your override file is gone\n", found.label())
        } else {
            format!("{} was never overridden, so there was nothing to restore\n", found.label())
        });
    }

    let mut out = String::new();
    if arguments.path {
        out.push_str(&format!("shipped   {}\n", wind_base::prompts::shipped_dir(config).display()));
        out.push_str(&format!("overrides {}\n", wind_base::prompts::override_dir(config).display()));
        return Ok(out);
    }
    out.push_str("The text sent to the model, and where a different copy would live:\n\n");
    for prompt in wind_base::prompts::read_all(config) {
        out.push_str(&format!(
            "  {:<22} {:>6} chars  {}\n",
            prompt.name.label(),
            prompt.text.chars().count(),
            origin_line(&prompt)
        ));
    }
    out.push_str(&format!(
        "\n`{{language}}` in those templates is filled with: {}   (from `lang` = {:?})\n",
        wind_base::prompts::answer_language(config),
        config.str_or("lang", wind_base::prompts::DEFAULT_INTERFACE_LANG)
    ));
    out.push_str("A template that names its own language in prose instead of carrying the slot is sent\n");
    out.push_str("exactly as written.\n");
    out.push_str(&format!(
        "\nedit   {}\nfrom   {}\n\n`--show NAME` prints one in full; `--restore NAME` deletes that override. The\nsettings screen edits these same files, so what is listed here is what runs.\n",
        wind_base::prompts::override_dir(config).display(),
        wind_base::prompts::shipped_dir(config).display()
    ));
    Ok(out)
}

/// Resolve a template name the user typed.
fn lookup(given: &str) -> Option<wind_base::prompts::Name> {
    wind_base::prompts::Name::ALL.iter().copied().find(|name| name.label() == given)
}

/// The refusal, naming every template there is. A typo in `--restore` that quietly did nothing would
/// leave the user believing their prompt was back to the shipped words.
fn unknown_prompt(index: &Index, given: &str) -> AiError {
    let listed: Vec<&str> = wind_base::prompts::Name::ALL.iter().map(|name| name.label()).collect();
    index.faults().usage(format!("no prompt is called `{given}`. The templates are: {}", listed.join(", ")))
}

fn origin_line(prompt: &wind_base::prompts::Prompt) -> String {
    match prompt.origin {
        wind_base::prompts::Origin::UserOverride => format!("your file, at {}", prompt.path.display()),
        wind_base::prompts::Origin::ShippedFile => format!("shipped, at {}", prompt.path.display()),
        wind_base::prompts::Origin::Embedded => "the copy compiled into this binary; no file was found".to_string(),
    }
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("#   {line}")).collect::<Vec<_>>().join("\n")
}

/// The `search` command's whole report, returned rather than printed.
fn search(arguments: SearchArgs) -> Result<String, AiError> {
    let mut index = open_index(arguments.root.clone())?;
    let client = Client::new(index.settings.clone());
    let started = Instant::now();
    let outcome = wind_ai::search::run(&mut index, &client, &arguments.phrase, arguments.limit)?;

    let mut out = String::new();
    if arguments.explain {
        out.push_str(&explain(&outcome, &index, started.elapsed().as_secs_f64()));
    }
    let hits = outcome.rows.len();
    out.push_str(&format!(
        "\n{} of {} matching record{} for {:?}\n",
        hits,
        outcome.total,
        if outcome.total == 1 { "" } else { "s" },
        outcome.phrase
    ));
    if hits == 0 {
        out.push_str(
            "Nothing in the index matches that. A zero-row answer is the search working, not failing — \
             try `--explain` to see what it was actually asked for.\n",
        );
        return Ok(out);
    }
    out.push_str(&render_rows(&outcome.rows));
    if outcome.capped {
        out.push_str(&format!(
            "(the scan stopped at {} rows; narrow the date range to see the rest)\n",
            wind_ai::search::SCAN_CAP
        ));
    }
    Ok(out)
}

/// What the model decided, and every correction this crate applied to it.
fn explain(outcome: &wind_ai::search::Outcome, index: &Index, elapsed_seconds: f64) -> String {
    let plan = &outcome.plan;
    let mut out = String::from("\nderived query (--explain)\n");
    out.push_str(&format!("  keywords    : {}\n", or_none(&plan.keywords)));
    out.push_str(&format!("  exclude     : {}\n", or_none(&plan.exclude)));
    out.push_str(&format!("  window title: {}\n", or_none(&plan.applications)));
    out.push_str(&format!("  occurrence  : {:?}\n", plan.occurrence));
    out.push_str(&format!(
        "  range       : {}  (asked for {})\n",
        plan.range().display(),
        if plan.requested_dates.0.is_empty() { "the whole library".to_string() }
        else { format!("{}…{}", plan.requested_dates.0, plan.requested_dates.1) }
    ));
    out.push_str(&format!("  months open : {}\n", outcome.months_searched));
    out.push_str(&format!("  elapsed     : {elapsed_seconds:.2}s\n"));
    if plan.is_time_only() {
        out.push_str("  note        : no keywords, so every record in the range is returned\n");
    }
    if plan.notes.is_empty() {
        out.push_str("  corrections : none — the query is exactly what the model described\n");
    } else {
        out.push_str("  corrections :\n");
        for note in &plan.notes {
            out.push_str(&format!("    - {note}\n"));
        }
    }
    if let Some(usage) = outcome.usage {
        out.push_str(&format!(
            "  endpoint    : {}  model {}\n  tokens      : {} prompt + {} completion = {total}\n",
            index.settings.base_url, index.settings.model, usage.prompt_tokens, usage.completion_tokens,
            total = usage.total_tokens.max(usage.prompt_tokens + usage.completion_tokens)
        ));
    }
    out
}

fn or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.iter().map(|item| format!("{item:?}")).collect::<Vec<_>>().join(", ")
    }
}

/// One line per hit: time, then the title or the first line of the body, the two things that identify a
/// moment. The body is never printed in full — it is the user's screen text, and a terminal is not where
/// a page of OCR belongs.
fn render_rows(rows: &[wind_store::read::Row]) -> String {
    let mut out = String::new();
    for row in rows {
        let when = wind_base::clock::LocalParts::from_naive_epoch(row.time);
        let detail = match (row.title(), row.body().lines().next()) {
            (Some(title), _) => title.trim().to_string(),
            (None, Some(line)) => line.trim().to_string(),
            (None, None) => "(no text)".to_string(),
        };
        out.push_str(&format!("  {}  {}  {}\n", when.date_stamp(), when.time_display(), clip_one_line(&detail, 88)));
    }
    out
}

fn clip_one_line(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

/// The `tags` command.
fn tags(arguments: TagsArgs) -> Result<String, AiError> {
    let index = open_index(arguments.root.clone())?;
    // The spend gate, enforced at the one binary that actually spends. `enable_ai_extract_tag` is the
    // switch this crate already reads everywhere — `Settings::read` loads it, `doctor` prints it, the
    // idle scheduler honours it — and honouring it here too means no caller, automated or a person at a
    // terminal, can reach the endpoint before the user has turned the feature on. This is not a second
    // switch: it is the existing one, read the same way, at the place it most needs to bite. A
    // `--dry-run` is always allowed: it builds the exact table and prints the cost without sending a
    // byte, which is precisely how you decide whether to switch it on.
    if !arguments.dry_run && !index.settings.enable_extract_tag {
        let faults = wind_ai::error::Faults::new(&index.settings.api_key);
        return Err(faults.disabled(
            "AI tagging is switched off (`enable_ai_extract_tag` is false), so `windai tags` will not \
             spend API quota against your key. Turn it on in Settings, or re-run with `--dry-run` to see \
             what a month would send and cost without sending anything.",
        ));
    }
    let client = Client::new(index.settings.clone());
    let tags = Tags::new(&index);
    let MonthInput { year, month } = MonthInput { year: arguments.month.year, month: arguments.month.month };
    let run = tags.run(&client, year, month, arguments.dry_run)?;
    let settings = &index.settings;

    let mut out = String::new();
    out.push_str(&format!(
        "{} {}\n  titles sent : {} (limit {} from `ai_extract_tag_wintitle_limit`), {} excluded by \
         `exclude_words`, {} of screen time\n  fingerprint : {}  cached at {}\n",
        if arguments.dry_run { "would tag" } else { "tags for" },
        run.tags.month,
        run.tags.table.titles.len(),
        settings.wintitle_limit.saturating_mul(wind_ai::tags::MONTH_TITLE_MULTIPLIER),
        run.tags.table.excluded,
        wind_base::clock::seconds_to_hhmmss(run.tags.table.total_seconds),
        run.tags.table.fingerprint,
        tags.hash_path(year).display(),
    ));
    if arguments.dry_run {
        out.push_str("\nwould send this table:\n");
        for line in run.tags.table.csv.lines() {
            out.push_str(&format!("  {line}\n"));
        }
        out.push_str(&format!(
            "\nnothing was sent and nothing was written. Re-run without --dry-run to ask `{}`.\n",
            settings.model
        ));
        return Ok(out);
    }
    out.push_str(&format!(
        "  source      : {}\n",
        if run.cache_hit { "cache — no request was made" } else { "endpoint" }
    ));
    out.push_str("\n  ");
    out.push_str(&run.tags.tags.join("  "));
    out.push('\n');
    out.push_str(&format!("\nwritten to {}\n", run.written_to.display()));
    Ok(out)
}

struct MonthInput {
    year: i64,
    month: u32,
}

/// The `doctor` command: configuration as it stands, then one round trip if there is anything to test.
///
/// This prints the base URL and the model name, both of which are ordinary configuration, and reports the
/// key as a yes/no plus a fingerprint — never its value, and not even its length, which is enough to
/// narrow a brute force of a lost key.
fn doctor(arguments: DoctorArgs) -> Result<String, AiError> {
    let root = wind_ai::library::resolve_root(arguments.root);
    let index = Index::open(&root)?;
    let settings: Settings = index.settings.clone();
    let faults = wind_ai::error::Faults::new(&settings.api_key);
    settings.require_usable(&faults).ok();

    let mut out = String::new();
    out.push_str(&format!("windai doctor — {}\n", root.display()));
    out.push_str(&format!("  endpoint     : {}  ({})\n", or_unset(&settings.base_url), settings.endpoint_selected));
    out.push_str(&format!("  model        : {}\n", or_unset(&settings.model)));
    out.push_str(&format!("  api key      : {}\n", key_line(&settings)));
    out.push_str(&format!("  tags feature : {}\n", yes_no(settings.enable_extract_tag)));
    out.push_str(&format!("  idle batcher : {}\n", yes_no(settings.enable_extract_tag_in_idle)));
    out.push_str(&format!(
        "  tag limits   : {} titles, {} tags per day, {} per idle run\n",
        settings.wintitle_limit, settings.max_tag_num, settings.idle_batch_size
    ));
    out.push_str(&format!("  filter words : {}\n", if settings.filter_words.is_empty() { "(none)".into() } else { settings.filter_words.join(", ") }));
    out.push_str(&format!("  tags written : {}\n", settings.tags_dir.display()));

    let months = index.months.len();
    let mut index = index;
    match index.bounds()? {
        Some(bounds) => {
            let (first, last) = wind_ai::dates::bound_dates(bounds);
            out.push_str(&format!("  index        : {months} month file(s), {first} to {last}\n"));
        }
        None if months == 0 => out.push_str(&format!("  index        : no month files in {}\n", index.root.join("userdata/db").display())),
        None => out.push_str(&format!("  index        : {months} month file(s), all empty\n")),
    }

    if !settings.key_configured() {
        out.push_str(
            "\ncannot make a test request: there is no API key to send. Nothing was sent and nothing was \
             charged. Set `open_ai_base_url`, `open_ai_api_key` and `open_ai_modelname` in \
             userdata/config_user.json (or in the Settings page) and run this again.\n",
        );
        return Ok(out);
    }
    if let Some(reason) = unusable(&settings, &faults) {
        out.push_str(&format!("\nnot testing the endpoint: {reason}\n"));
        return Ok(out);
    }

    let client = Client::new(settings.clone());
    out.push_str("\nmaking one request…\n");
    let started = Instant::now();
    match client.ping() {
        Ok(completion) => {
            out.push_str(&format!(
                "  round trip   : {:.1}s, {} characters back\n",
                started.elapsed().as_secs_f64(),
                completion.text.chars().count()
            ));
            if let Some(usage) = completion.usage {
                out.push_str(&format!("  tokens       : {}\n", usage.total_tokens));
            }
            out.push_str(&format!("  reply          : {}\n", clip_one_line(completion.text.trim(), 72)));
            out.push_str("the endpoint answers, so `search` and `tags` are configured.\n");
        }
        Err(error) => {
            out.push_str(&format!("  round trip   : failed after {:.1}s\n", started.elapsed().as_secs_f64()));
            out.push_str(&format!("  {error}\n"));
            out.push_str(
                "that is the request itself failing, not this tool. The message above is what the \
                 endpoint said, with the key removed from it.\n",
            );
        }
    }
    Ok(out)
}

/// The answer to "is there a key, and where does it go?".
///
/// Factored out of `doctor` so that `report::the_doctor_wording_describes_a_key_without_printing_it`
/// asserts on *this* text: the version of the wording a test re-types is a copy that can drift, and when
/// it drifts the test stops describing the product. It drifted once — the copy dropped the
/// `userdata/config_user.json` path the real message names, and the assertion "the message says where to
/// fix it" then failed against the copy while the product was fine.
///
/// The configured branch prints a fingerprint and nothing else: not the value, not its length, because a
/// screenshot of a terminal is a plausible place for this line to end up.
fn key_line(settings: &Settings) -> String {
    if settings.key_configured() {
        format!("configured, fingerprint {}", settings.api_key.fingerprint())
    } else if settings.api_key.is_empty() {
        "NOT SET — put it in userdata/config_user.json as `open_ai_api_key`".to_string()
    } else {
        format!(
            "NOT SET — still holds the installer placeholder `{}`; replace it in \
             userdata/config_user.json",
            wind_ai::KEY_PLACEHOLDER
        )
    }
}

fn unusable(settings: &Settings, faults: &wind_ai::error::Faults) -> Option<String> {
    settings.require_usable(faults).err().map(|e| e.to_string())
}

fn or_unset(value: &str) -> String {
    if value.trim().is_empty() { "NOT SET".to_string() } else { value.to_string() }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

/// The pieces of the report that are worth asserting without a terminal, a key or an index.
#[cfg(test)]
mod report {
    use super::*;
    use std::path::Path;

    fn row(stamp: &str, text: &str, title: Option<&str>) -> wind_store::read::Row {
        wind_store::read::Row {
            rowid: 1,
            videofile_name: format!("{stamp}-VIDEO.mp4"),
            picturefile_name: String::new(),
            time: wind_base::clock::LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds(),
            ocr_text: text.to_string(),
            win_title: title.map(str::to_string),
            deep_linking: None,
            thumbnail: None,
            video_exists: true,
            picture_exists: false,
            month_path: None,
        }
    }

    #[test]
    fn a_hit_line_is_time_then_title_then_body_when_there_is_no_title() {
        let lines = render_rows(&[row("2026-09-18_14-02-00", "续约合同 终稿", Some("Word - x")), row("2026-09-19_10-00-00", "no title here", None)]);
        let got: Vec<&str> = lines.lines().collect();
        assert_eq!(got.len(), 2, "{lines}");
        assert!(got[0].starts_with("  2026-09-18  14:02:00  Word - x"), "{}", got[0]);
        assert!(got[1].contains("no title here"), "{}", got[1]);
        assert!(!lines.contains("续"), "the body is not printed when a title exists: {lines}");
    }

    #[test]
    fn a_long_title_is_clipped_and_said_so() {
        let long = "x".repeat(200);
        let lines = render_rows(&[row("2026-09-18_14-02-00", "", Some(&long))]);
        assert!(lines.contains('…'), "{lines}");
        assert!(lines.chars().count() < 140, "{lines}");
    }

    #[test]
    fn a_row_with_nothing_in_it_still_prints_a_line() {
        let lines = render_rows(&[row("2026-09-18_14-02-00", "", None)]);
        assert!(lines.contains("(no text)"), "{lines}");
    }

    #[test]
    fn an_empty_result_is_reported_as_an_empty_result() {
        assert_eq!(render_rows(&[]), "");
        assert_eq!(or_none(&[]), "(none)");
        assert_eq!(or_none(&["a".into(), "b".into()]), "\"a\", \"b\"");
    }

    /// The `doctor` report's key line, asserted on the two states this install can be in — and asserted
    /// against `key_line` itself, not against a re-typed copy of it. The value itself is never reachable
    /// from here: `SecretKey::expose` is `pub(crate)` to the *library*, and this is a different crate.
    #[test]
    fn the_doctor_wording_describes_a_key_without_printing_it() {
        let settings = Settings::read(&wind_base::config::Config::load(&repo_root()).unwrap());
        let described = key_line(&settings);
        assert!(!described.contains(wind_ai::KEY_PLACEHOLDER) || !settings.key_configured());
        if settings.key_configured() {
            assert!(described.starts_with("configured, fingerprint "), "{described}");
            assert!(described.chars().count() < 60, "a fingerprint is short: {described}");
            let hex: String = described.rsplit(' ').next().unwrap().chars().rev().collect();
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{described}");
        } else {
            assert!(described.contains("config_user.json"), "the message names where to fix it");
        }
        // The third state — a key that is simply absent — has to name the place too, and must not claim
        // the installer wrote a placeholder that is not there. Built by hand rather than from a fixture
        // install, because this binary cannot reach the library's test scaffolding; that is the right way
        // round anyway, since the line depends on the key and on nothing else.
        let mut absent = settings.clone();
        absent.api_key = wind_ai::SecretKey::new("");
        let absent_line = key_line(&absent);
        assert!(absent_line.contains("config_user.json"), "{absent_line}");
        assert!(!absent_line.contains(wind_ai::KEY_PLACEHOLDER), "{absent_line}");
        assert_eq!(or_unset(""), "NOT SET");
        assert_eq!(or_unset("https://x"), "https://x");
        assert_eq!(yes_no(true), "on");
        assert_eq!(yes_no(false), "off");
    }

    fn repo_root() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .map(Path::to_path_buf)
            .unwrap()
    }
}
