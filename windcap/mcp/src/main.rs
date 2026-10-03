//! `windmcp` — the terminal front door to the MCP bridge.
//!
//! `serve` and `doctor` are the service. The other eleven commands are the *tools*, callable from a
//! terminal, and they are not a second implementation: each one resolves a root, then calls the same
//! `wind_mcp::tools` function the JSON-RPC layer dispatches to. That identity is the reason they exist.
//! A tool whose logic can only be reached through an MCP client cannot be regression-tested in CI, and
//! "the bridge works" is not a claim worth making unless the exact bytes it returns are visible without
//! a client in the loop. It is also why `period-summary-write` and `day-summary-write` are here: a
//! writer that can only be reached over HTTP cannot be tried before a client is configured, and the
//! gate on the daily write is a rule worth seeing fail in a terminal.
//!
//! Reports are the house style: one labelled line per fact, and a measured millisecond figure next
//! to every answer, because this directory's whole argument is the same data, faster, and an
//! unmeasurable argument is not worth shipping.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{json, Value};
use wind_mcp::args::{self, Command, ParseError, Range};
use wind_mcp::axis::Axis;
use wind_mcp::runtime::Runtime;
use wind_mcp::{auth, library, runtime, server, tools};

/// The other way in, after `--root`: a *path*, which is not a secret and so is safe in an
/// environment. Nothing here will read a token from anywhere else but the config file.
const ROOT_ENV_VAR: &str = "WINDRECORDER_ROOT";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let command = match args::parse(&argv) {
        Ok(command) => command,
        Err(ParseError::Help) => {
            print!("{}", args::USAGE);
            return ExitCode::SUCCESS;
        }
        // Ahead of `open()`, which is where every other path resolves a root and loads
        // `userdata/config_user.json` -- the version is the answer that must not need either.
        Err(ParseError::Version) => {
            println!("{}", args::version_line());
            return ExitCode::SUCCESS;
        }
        Err(other) => {
            eprintln!("windmcp: {other}");
            return ExitCode::from(2);
        }
    };
    let report = match dispatch(command) {
        Ok(report) => report,
        Err(message) => {
            eprintln!("windmcp: {message}");
            return ExitCode::from(2);
        }
    };
    print!("{report}");
    ExitCode::SUCCESS
}

fn dispatch(command: Command) -> Result<String, String> {
    match command {
        Command::Serve { root, host, port } => {
            let runtime = open(root)?;
            // Blocks for the life of the service; `serve` writes its own banner to stderr before the
            // accept loop opens, so a supervisor sees what was bound without parsing stdout.
            server::serve(runtime, port, host).map_err(|e| format!("refused to start: {e}"))?;
            Ok(String::new())
        }
        Command::Doctor { root } => doctor(root),
        Command::Status { root, json } => {
            let (runtime, axis) = pair(root)?;
            render(json, tools::status(&runtime, &axis), "status", 0.0)
        }
        Command::Search { root, keywords, range, exclude, limit, offset, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let mut arguments = range_arguments(&range);
            arguments.insert("keywords".to_string(), json!(keywords));
            if let Some(exclude) = exclude {
                arguments.insert("exclude".to_string(), json!(exclude));
            }
            arguments.insert("limit".to_string(), limit.map_or(Value::Null, |limit| json!(limit)));
            arguments.insert("offset".to_string(), offset.map_or(Value::Null, |offset| json!(offset)));
            let value = call(&runtime, &axis, "windrecorder_search", &Value::Object(arguments))?;
            render(json, value, "search", library::elapsed_ms(started))
        }
        Command::Around { root, moment, window, limit, max_text, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let value = call(&runtime, &axis, "windrecorder_around", &json!({
                "timestamp": moment, "window_seconds": window, "limit": limit, "max_text_chars": max_text,
            }))?;
            render(json, value, "around", library::elapsed_ms(started))
        }
        Command::AppUsage { root, range, limit, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let mut arguments = range_arguments(&range);
            arguments.insert("limit".to_string(), limit.map_or(Value::Null, |limit| json!(limit)));
            let value = call(&runtime, &axis, "windrecorder_app_usage", &Value::Object(arguments))?;
            render(json, value, "app-usage", library::elapsed_ms(started))
        }
        Command::DaySummary { root, date, limit, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let value = call(&runtime, &axis, "windrecorder_day_summary", &json!({ "date": date, "limit": limit }))?;
            render(json, value, "day-summary", library::elapsed_ms(started))
        }
        Command::Frame { root, moment, window, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let value = call(&runtime, &axis, "windrecorder_frame", &json!({ "timestamp": moment, "window_seconds": window }))?;
            render(json, value, "frame", library::elapsed_ms(started))
        }
        Command::SummariesPending { root, range, include, max_text, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let mut arguments = range_arguments(&range);
            if let Some(include) = include {
                arguments.insert("include".to_string(), json!(include));
            }
            if let Some(max_text) = max_text {
                arguments.insert("max_text_chars".to_string(), json!(max_text));
            }
            let value = call(&runtime, &axis, "windrecorder_summaries_pending", &Value::Object(arguments))?;
            render(json, value, "summaries-pending", library::elapsed_ms(started))
        }
        Command::SummariesRead { root, range, kind, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let mut arguments = range_arguments(&range);
            if let Some(kind) = kind {
                arguments.insert("kind".to_string(), json!(kind));
            }
            let value = call(&runtime, &axis, "windrecorder_summaries_read", &Value::Object(arguments))?;
            render(json, value, "summaries-read", library::elapsed_ms(started))
        }
        Command::PromptsRead { root, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let value = call(&runtime, &axis, "windrecorder_prompts_read", &json!({}))?;
            render(json, value, "prompts-read", library::elapsed_ms(started))
        }
        Command::PeriodSummaryWrite { root, segment, day, body, written_by, model, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let mut arguments = serde_json::Map::new();
            arguments.insert("segment".to_string(), json!(segment));
            if let Some(day) = day {
                arguments.insert("day".to_string(), json!(day));
            }
            arguments.insert("text".to_string(), json!(body.read()?));
            insert_caller(&mut arguments, written_by, model);
            let value = call(&runtime, &axis, "windrecorder_period_summary_write", &Value::Object(arguments))?;
            render(json, value, "period-summary-write", library::elapsed_ms(started))
        }
        Command::DaySummaryWrite { root, date, body, allow_partial, written_by, model, json } => {
            let (runtime, axis) = pair(root)?;
            let started = std::time::Instant::now();
            let mut arguments = serde_json::Map::new();
            arguments.insert("date".to_string(), json!(date));
            arguments.insert("text".to_string(), json!(body.read()?));
            if allow_partial {
                arguments.insert("allow_partial".to_string(), json!(true));
            }
            insert_caller(&mut arguments, written_by, model);
            let value = call(&runtime, &axis, "windrecorder_day_summary_write", &Value::Object(arguments))?;
            render(json, value, "day-summary-write", library::elapsed_ms(started))
        }
    }
}

/// Who is writing, as the caller says it. Recorded verbatim and used to decide nothing.
fn insert_caller(arguments: &mut serde_json::Map<String, Value>, written_by: Option<String>, model: Option<String>) {
    if let Some(written_by) = written_by {
        arguments.insert("written_by".to_string(), json!(written_by));
    }
    if let Some(model) = model {
        arguments.insert("model".to_string(), json!(model));
    }
}

/// One tool, through the dispatcher the service uses.
fn call(runtime: &Runtime, axis: &Axis, name: &str, arguments: &serde_json::Value) -> Result<serde_json::Value, String> {
    tools::call(runtime, axis, name, arguments).map_err(|rejected| rejected.to_string())
}

/// The arguments a range-taking command contributes to its tool.
///
/// Exactly one place, for the same reason the tools have exactly one day helper: if each command
/// turned `--day` into bounds on its way past, this file would be where the two of them drifted
/// apart again. A day is handed over as a `day` and resolved by `tools::day_window`, which is the
/// same function `day-summary` uses and the only caller of `wind_base::clock::day_bounds` here.
fn range_arguments(range: &Range) -> serde_json::Map<String, Value> {
    let mut arguments = serde_json::Map::new();
    match range {
        Range::Day(day) => {
            arguments.insert("day".to_string(), json!(day));
        }
        Range::Between(from, to) => {
            arguments.insert("start".to_string(), json!(from));
            arguments.insert("end".to_string(), json!(to));
        }
    }
    arguments
}

fn pair(root: Option<PathBuf>) -> Result<(Runtime, Axis), String> {
    let runtime = open(root)?;
    Ok((runtime, Axis::measure()))
}

fn open(root: Option<PathBuf>) -> Result<Runtime, String> {
    let chosen = root.or_else(|| std::env::var_os(ROOT_ENV_VAR).map(PathBuf::from)).unwrap_or_else(default_root);
    Runtime::open(&chosen).map_err(|e| e.to_string())
}

/// `--root`, or `WINDRECORDER_ROOT`, or the install this binary was launched from.
///
/// The rule is [`wind_base::install`]'s, shared with every other binary here: a directory that
/// carries the shipped settings is an install, and the walk-up from `windcap/target/debug` finds
/// the checkout exactly as the walk-up from `C:\Windrecorder\bin` finds the install. One
/// implementation, because two of them agreeing is a coincidence rather than a design — and when
/// they stop agreeing, `windmcp` answers questions about a different install than `windcapctl`.
fn default_root() -> PathBuf {
    wind_base::install::resolve_root_from_exe(None)
}

/// `--json` emits exactly what the tool returned, byte for byte, because that is what a test asserts
/// on and what a script pipes. Without it, a readable report.
fn render(json: bool, value: serde_json::Value, what: &str, ms: f64) -> Result<String, String> {
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?));
    }
    let mut out = String::new();
    match what {
        "status" => {
            push(&mut out, "root", &text(&value["root"]));
            push(&mut out, "user", &text(&value["user_name"]));
            push(&mut out, "databases", &format!("{} file(s)", value["databases"].as_array().map_or(0, Vec::len)));
            push(&mut out, "rows", &format!("{}", value["total_rows"].as_i64().unwrap_or(0)));
            push(&mut out, "segments", &format!("{}", value["total_segments"].as_i64().unwrap_or(0)));
            push(&mut out, "first", value["first_record"].as_str().unwrap_or("-").to_string());
            push(&mut out, "last", value["last_record"].as_str().unwrap_or("-").to_string());
            push(&mut out, "age", match value["minutes_since_last_record"].as_i64() {
                Some(minutes) => format!("{minutes} min since the last record"),
                None => "nothing recorded yet".to_string(),
            });
            push(&mut out, "clock", format!("{} ({}s past POSIX)", value["clock"]["epoch"].as_str().unwrap_or("?"), value["clock"]["utc_offset_seconds"].as_i64().unwrap_or(0)));
            push(&mut out, "product day", format!("begins at {} (day_begin_minutes {})", value["day_begin"]["at"].as_str().unwrap_or("?"), value["day_begin"]["minutes"].as_i64().unwrap_or(0)));
            // Which AI caches the day summary can draw on, and in which key shape. Printed here
            // because it is the one thing a user cannot learn any other way from a terminal: that the
            // tags on this install are month-wide, and were always going to be.
            push(&mut out, "ai caches", &ai_caches_line(&value));
            // The two directories this service writes into, which is the fact a reader of `status` most
            // needs before asking why a day has no summary: whether anything was ever written at all.
            push(&mut out, "summaries", &summary_caches_line(&value));
            for database in value["databases"].as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "  {:<32} {:>7} rows  {:>4} seg  {}{}\n",
                    text(&database["database"]),
                    database["rows"].as_i64().unwrap_or(0),
                    database["segments"].as_i64().unwrap_or(0),
                    database["first_record"].as_str().unwrap_or("-"),
                    match database["missing_columns"].as_array().filter(|l| !l.is_empty()) {
                        Some(missing) => format!("  missing {}", missing.iter().map(text).collect::<Vec<_>>().join("+")),
                        None => String::new(),
                    },
                ));
            }
        }
        "search" => {
            push(&mut out, "window", &window_line(&value));
            push(&mut out, "keywords", &value["keywords"].as_array().map(|l| l.iter().map(text).collect::<Vec<_>>().join(" ")).unwrap_or_default());
            push(&mut out, "matches", &format!("{} total, {} shown (offset {})", value["total_matches"].as_i64().unwrap_or(0), value["returned"].as_i64().unwrap_or(0), value["offset"].as_i64().unwrap_or(0)));
            for row in value["results"].as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "  {}  {:<34}  {}\n",
                    row["time"].as_str().unwrap_or("?"),
                    truncate(&text(&row["window_title"]), 34),
                    truncate(&row["text"].as_str().unwrap_or("").replace('\n', " "), 60),
                ));
            }
        }
        "around" => {
            push(&mut out, "center", &format!("{} (stored {})", value["center"].as_str().unwrap_or("?"), value["center_timestamp"].as_i64().unwrap_or(0)));
            push(&mut out, "window", &window_line(&value));
            push(&mut out, "frames", &format!("{} of {} in range", value["frames"].as_array().map_or(0, Vec::len), value["in_range"].as_i64().unwrap_or(0)));
            for row in value["frames"].as_array().into_iter().flatten() {
                out.push_str(&format!("  {}  {:<34}  {}\n", row["time"].as_str().unwrap_or("?"), truncate(&text(&row["window_title"]), 34), truncate(&row["text"].as_str().unwrap_or("").replace('\n', " "), 50)));
            }
        }
        "app-usage" => {
            push(&mut out, "window", &window_line(&value));
            push(&mut out, "counted", &format!("{} s across {} title(s)", value["total_counted_seconds"].as_i64().unwrap_or(0), value["distinct_titles"].as_i64().unwrap_or(0)));
            if value["withheld_excluded_titles"].as_i64().unwrap_or(0) > 0 {
                push(&mut out, "withheld", &format!("{} frame(s) under an excluded title", value["withheld_excluded_titles"].as_i64().unwrap_or(0)));
            }
            for row in value["usage"].as_array().into_iter().flatten() {
                out.push_str(&format!("  {:>5} s  {:>6.2}%  {}\n", row["seconds"].as_i64().unwrap_or(0), row["share_of_counted_time"].as_f64().unwrap_or(0.0) * 100.0, text(&row["window_title"])));
            }
        }
        "day-summary" => {
            push(&mut out, "date", &text(&value["date"]));
            push(&mut out, "window", &window_line(&value));
            push(&mut out, "counted", &format!("{} s across {} event(s)", value["total_counted_seconds"].as_i64().unwrap_or(0), value["total_events"].as_i64().unwrap_or(0)));
            for row in value["events"].as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "  {}  {:>5} s  {:>3} f  {}\n",
                    row["from"].as_str().unwrap_or("?").get(11..).unwrap_or("?"),
                    row["seconds"].as_i64().unwrap_or(0),
                    row["frames"].as_i64().unwrap_or(0),
                    text(&row["window_title"]),
                ));
            }
            // Both AI answers get a line even when they carry nothing, and the line names the reason:
            // a report that printed nothing for `not_generated` and nothing for `unreadable` would
            // reproduce in the terminal the exact ambiguity the payload was changed to remove.
            push(&mut out, "ai tags", &ai_line(&value["ai_tags"], "tags"));
            push(&mut out, "ai summary", &ai_line(&value["ai_summary"], "text"));
            // The one case where reading the width wrong costs the user a true statement: a month's
            // tags printed under a day's date look like an answer about that day.
            if value["ai_tags"]["granularity"] == json!("month") {
                push(&mut out, "ai caveat", "the tags above summarise the whole month, not this day (see `ai_tags.note` in --json)");
            }
        }
        "frame" => {
            push(&mut out, "time", &format!("{} ({} s from the moment)", value["time"].as_str().unwrap_or("?"), value["distance_seconds"].as_i64().unwrap_or(0)));
            push(&mut out, "window", &window_line(&value));
            push(&mut out, "title", &text(&value["window_title"]));
            push(&mut out, "video", &format!("{} +{} s", text(&value["video_file"]), value["offset_in_segment"].as_i64().unwrap_or(0)));
            push(&mut out, "thumbnail", match value["thumbnail"].as_object() {
                Some(_) => format!("{} {} bytes, resource {}", value["thumbnail"]["mime_type"].as_str().unwrap_or("?"), value["thumbnail"]["bytes"].as_i64().unwrap_or(0), value["thumbnail"]["resource"].as_str().unwrap_or("?")),
                None => "none stored for this frame".to_string(),
            });
            push(&mut out, "frame", value["frame_path"].as_str().unwrap_or("not on disk"));
            push(&mut out, "segment", value["video_path"].as_str().unwrap_or("not on disk"));
        }
        "summaries-pending" => {
            push(&mut out, "window", &window_line(&value));
            let counted = &value["counted"];
            push(
                &mut out,
                "counted",
                &format!(
                    "{} day(s), {} stretch(es), {} summarised, {} needing work",
                    counted["days"].as_i64().unwrap_or(0),
                    counted["segments_total"].as_i64().unwrap_or(0),
                    counted["summarised"].as_i64().unwrap_or(0),
                    counted["needing_work"].as_i64().unwrap_or(0),
                ),
            );
            for (label, key) in [("pending", "pending"), ("stale", "stale")] {
                let items = value[key].as_array().cloned().unwrap_or_default();
                push(&mut out, label, &format!("{} stretch(es)", items.len()));
                for item in items {
                    // The day is already in the segment key and every row is the same day, so the span
                    // prints as two clock times: the payload's own `when` is 53 characters of repeated
                    // date, and a queue is read for "how long was this" rather than "in which month".
                    let when = item["when"].as_str().unwrap_or("?");
                    let (begins, ends) = when.split_once(" → ").unwrap_or((when, "?"));
                    out.push_str(&format!(
                        "  {}  {}-{}  {:>5} s  {:>4} f  {:>6} c  {}\n",
                        item["segment"].as_str().unwrap_or("?"),
                        time_part(begins),
                        time_part(ends),
                        item["duration_seconds"].as_i64().unwrap_or(0),
                        item["frames"].as_i64().unwrap_or(0),
                        item["ocr_chars"].as_i64().unwrap_or(0),
                        item["reason"].as_str().unwrap_or("?"),
                    ));
                }
            }
            let days = value["days_pending"].as_array().cloned().unwrap_or_default();
            if !days.is_empty() {
                push(&mut out, "days to redo", &format!("{} day(s)", days.len()));
                for day in days {
                    out.push_str(&format!(
                        "  {}  {} of {} summarised  {}\n",
                        day["date"].as_str().unwrap_or("?"),
                        day["coverage"]["summarised"].as_i64().unwrap_or(0),
                        day["coverage"]["segments_total"].as_i64().unwrap_or(0),
                        day["reasons"].as_array().map(|l| l.iter().map(text).collect::<Vec<_>>().join("+")).unwrap_or_default(),
                    ));
                }
            }
            push(&mut out, "prompt", &prompt_line(&value["prompt"]));
            push(&mut out, "where", &format!("{} | {}", text(&value["where_summaries_live"]["period"]), text(&value["where_summaries_live"]["daily"])));
            if let Some(note) = value["unattributed_note"].as_str() {
                push(&mut out, "unattributed", note);
            }
            push(&mut out, "reading", "each entry's `frames_detail` in --json carries the whole captured text of that stretch");
        }
        "summaries-read" => {
            push(&mut out, "window", &window_line(&value));
            push(&mut out, "kind", &text(&value["kind"]));
            push(
                &mut out,
                "counts",
                &format!(
                    "{} period summar(y/ies), {} daily",
                    value["counts"]["period_summaries"].as_i64().unwrap_or(0),
                    value["counts"]["daily_summaries"].as_i64().unwrap_or(0)
                ),
            );
            for day in value["days"].as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "  {}  period {:<13}  daily {}\n",
                    day["date"].as_str().unwrap_or("?"),
                    format!(
                        "{} ({})",
                        day["period"]["state"].as_str().unwrap_or("-"),
                        day["period"]["entries"].as_array().map_or(0, Vec::len)
                    ),
                    daily_state_line(&day["daily"]),
                ));
                for entry in day["period"]["entries"].as_array().into_iter().flatten() {
                    out.push_str(&format!(
                        "      {}  {:>5} char(s)  by {:<14}  at {}\n",
                        entry["segment"].as_str().unwrap_or("?"),
                        entry["text_chars"].as_i64().unwrap_or(0),
                        truncate(&nonempty(text(&entry["written_by"]), "unattributed"), 14),
                        entry["written_at"].as_str().unwrap_or("?"),
                    ));
                }
            }
            let absent = value["absent_days"].as_array().cloned().unwrap_or_default();
            if !absent.is_empty() {
                push(&mut out, "absent", &format!("{} day(s) with nothing written: {}", absent.len(), absent.iter().map(text).collect::<Vec<_>>().join(", ")));
            }
        }
        "prompts-read" => {
            push(&mut out, "language", &text(&value["language"]));
            for prompt in value["prompts"].as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "  {:<22} {:<9} {:>6} char(s)  from {}{}\n",
                    text(&prompt["name"]),
                    text(&prompt["origin"]),
                    prompt["text"].as_str().map(|body| body.chars().count()).unwrap_or(0),
                    text(&prompt["path"]),
                    match prompt["required"].as_array().filter(|l| !l.is_empty()) {
                        Some(list) => format!(", keeps {}", list.iter().map(text).collect::<Vec<_>>().join("+")),
                        None => String::new(),
                    },
                ));
            }
            push(&mut out, "override", "userdata/ai_prompts/<name>.txt wins over config_src/ai_prompts/<name>.txt; deleting the first restores the shipped words");
            push(&mut out, "limits", "no length limit on a prompt or on a summary, and no word list either");
        }
        "period-summary-write" => {
            push(&mut out, "segment", &text(&value["segment"]));
            push(&mut out, "when", &text(&value["when"]));
            push(&mut out, "stored", &format!("{} char(s) into {} ({})", value["text_chars"].as_i64().unwrap_or(0), text(&value["written_to"]), if value["replaced"] == json!(true) { "replaced" } else { "new" }));
            push(
                &mut out,
                "day",
                &format!(
                    "{}: {} of {} stretches summarised — {}",
                    text(&value["day"]),
                    value["day_coverage"]["summarised"].as_i64().unwrap_or(0),
                    value["day_coverage"]["segments_total"].as_i64().unwrap_or(0),
                    if value["day_coverage"]["complete"] == json!(true) { "the daily write will accept this day" } else { "still short, so `windmcp summaries-pending --day` names what is left" },
                ),
            );
        }
        "day-summary-write" => {
            push(&mut out, "date", &text(&value["date"]));
            push(&mut out, "stored", &format!("{} char(s) into {} ({})", value["text_chars"].as_i64().unwrap_or(0), text(&value["written_to"]), if value["replaced"] == json!(true) { "replaced" } else { "new" }));
            push(
                &mut out,
                "coverage",
                &format!(
                    "{} of {}{}, written {}",
                    value["coverage"]["segments_summarised"].as_i64().unwrap_or(0),
                    value["coverage"]["segments_total"].as_i64().unwrap_or(0),
                    match value["coverage"]["missing"].as_array() {
                        Some(missing) if !missing.is_empty() => format!(", {} still missing", missing.len()),
                        _ => String::new(),
                    },
                    if value["partial"] == json!(true) { "over a gap, and recorded that way" } else { "as a whole day" },
                ),
            );
            push(&mut out, "at", &text(&value["written_at"]));
        }
        other => return Err(format!("no report renderer for {other}")),
    }
    if let Some(skipped) = value["skipped_databases"].as_array() {
        push(&mut out, "skipped", &format!("{} database(s)", skipped.len()));
        for note in skipped.iter().map(text) {
            out.push_str(&format!("  ! {note}\n"));
        }
    }
    out.push_str(&format!("{}: {ms:.3} ms\n", what));
    Ok(out)
}

/// `windmcp doctor` — everything the user needs in order to decide whether to turn this on, and
/// nothing that opens a socket or prints a secret.
fn doctor(root: Option<PathBuf>) -> Result<String, String> {
    let runtime = open(root)?;
    let axis = Axis::measure();
    let host = runtime.host();
    let port = runtime.port();
    let token = runtime.token();
    let auth_required = runtime.auth_required();
    let state = Runtime::token_state(&token, auth::TOKEN_MIN_CHARS);

    let mut out = String::new();
    push(&mut out, "root", &runtime.root().display().to_string());
    push(&mut out, "enabled", match runtime.enabled() {
        true => "yes (enable_mcp_server)",
        false => "no (enable_mcp_server is off; nothing will listen until it is set)",
    });
    push(&mut out, "bind", &format!("{host}:{}", if port == 0 { "unset-or-invalid".to_string() } else { port.to_string() }));
    push(&mut out, "scope", match (auth::is_loopback(&host), auth::is_wildcard(&host)) {
        (true, _) => "loopback only — this machine",
        (_, true) => "EVERY INTERFACE — reachable from the local network",
        (false, false) => "one address — reachable as that address",
    });
    push(&mut out, "auth", if auth_required { "required (Authorization: Bearer ...)" } else { "NOT REQUIRED" });
    // The value is never printed, at any length, in either direction. Only whether one exists, how
    // long it is, and whether that is long enough to be a secret.
    push(&mut out, "token", match state {
        runtime::TokenState::Absent if auth_required => "not configured (in userdata/config_user.json, never in argv or the environment)",
        runtime::TokenState::Absent => "not configured, and authentication is off",
        runtime::TokenState::TooShort => "configured but shorter than 24 characters — too short to be a secret",
        runtime::TokenState::Usable => "configured",
    });
    if state != runtime::TokenState::Absent {
        push(&mut out, "token length", &format!("{} characters", token.chars().count()));
    }

    let guard = server::validate_bind(&host, port, &token, auth_required);
    match &guard {
        Ok(_) => push(&mut out, "would bind", "ok"),
        Err(why) => push(&mut out, "would refuse", why),
    }

    let (facts, unreadable, ms) = runtime.facts();
    let rows: i64 = facts.iter().map(|f| f.rows).sum();
    push(&mut out, "visible", &format!("{} month file(s), {rows} row(s), {} segment(s)", facts.len(), facts.iter().map(|f| f.segments).sum::<i64>()));
    for fact in &facts {
        out.push_str(&format!(
            "  {:<32} {:>7} rows  {} → {}\n",
            library::name(&fact.month),
            fact.rows,
            fact.bounds.map(|b| axis.render(b.0)).unwrap_or_else(|| "-".to_string()),
            fact.bounds.map(|b| axis.render(b.1)).unwrap_or_else(|| "-".to_string()),
        ));
    }
    for note in &unreadable {
        out.push_str(&format!("  ! unreadable: {note}\n"));
    }
    push(&mut out, "user", &runtime.config().user_name());
    push(&mut out, "exclude_words", &format!("{} word(s) withheld from summaries", runtime.exclude_words().len()));
    // The bridge stopped being read-only when it gained the two summary writers, so the screen a user
    // reads *before* turning it on has to say where it writes and how much is already there. Naming the
    // two directories is also the reassurance: they are not the index, not the videos, and not anything
    // this install cannot rebuild — and `windmaint forget` reaches them when it erases.
    push(&mut out, "writes", &writes_line(&runtime));
    push(&mut out, "clock", &format!("timestamps are naive-local: POSIX + {}s ({})", axis.utc_offset_seconds, axis.offset_literal()));
    // The other convention every `--day` in this binary depends on, printed next to the first one
    // because each is the difference between an answer and an answer three or eight hours away. The
    // window is today's, resolved by the same helper every day-taking command uses, so what a user
    // reads here is the rule in force rather than a description of it.
    push(&mut out, "product day", &format!("begins at {} (day_begin_minutes {})", tools::day_begin_label(runtime.day_begin_minutes()), runtime.day_begin_minutes()));
    let today = wind_base::clock::now().date_stamp();
    push(&mut out, "day window", &format!("--day {today} means {}", tools::day_window(&runtime, &axis, &today).map(|window| window.label(&axis)).unwrap_or_else(|why| why.to_string())));

    let (lock_present, lock_pid, lock_age) = runtime.recorder_lock();
    push(&mut out, "recorder lock", match (lock_present, lock_pid) {
        (true, Some(pid)) => format!("present, owner pid {pid}, {} s old", lock_age.unwrap_or(0)),
        (true, None) => format!("present, owner unreadable ({} s old)", lock_age.unwrap_or(0)),
        (false, _) => "absent - the recorder is not running".to_string(),
    });

    push(&mut out, "client url", &runtime.client_url());
    out.push('\n');
    if let Ok(refused) = guard {
        let _ = refused;
    }
    let example = if auth_required {
        format!(
            "  {{\"url\": \"{}\", \"headers\": {{\"Authorization\": \"Bearer <mcp_server_token>\"}}}}",
            runtime.client_url()
        )
    } else {
        format!("  {{\"url\": \"{}\"}}  (no token — reachable without one)", runtime.client_url())
    };
    out.push_str(&format!("what a client config looks like:\n{example}\n"));
    out.push_str(&format!("doctor: {ms:.3} ms\n"));
    Ok(out)
}

fn push(out: &mut String, label: &str, value: impl std::fmt::Display) {
    out.push_str(&format!("{:<16}{value}\n", format!("{label}: ")));
}

/// The window a tool actually searched, with the rule that produced it, read straight out of the
/// payload.
///
/// `windcapctl query` has always printed `(day, day_begin 03:00)` next to its window; this is the
/// same sentence in this binary's voice. It is assembled from the response rather than from the
/// command's own arguments so that a report can never quote a window the tool did not use, and it is
/// printed by every command that takes a day or a range so that two of them disagreeing about
/// `--day` is something a reader trips over rather than something they have to notice.
fn window_line(value: &serde_json::Value) -> String {
    let range = &value["range"];
    format!(
        "{} → {} ({}, day_begin {})",
        range["start"].as_str().unwrap_or("?"),
        range["end"].as_str().unwrap_or("?"),
        range["rule"].as_str().unwrap_or("?"),
        range["day_begin"].as_str().unwrap_or("?"),
    )
}

fn text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One AI answer, as a report line: whether it answered, at what width, out of which cache key.
///
/// `state` and `granularity` print even when there is nothing to print, because they are the two
/// facts that stop a reader using a month's tags as though they described one afternoon — and a
/// report that showed a blank line for `not_generated` and the same blank line for `unreadable`
/// would rebuild in the terminal the exact ambiguity the payload was changed to remove. The long
/// `note` stays out of the table; `--json` carries the whole object, and a two-hundred-character
/// sentence per field is how reports stop being read.
fn ai_line(field: &serde_json::Value, answer: &str) -> String {
    let state = field["state"].as_str().unwrap_or("?");
    let granularity = field["granularity"].as_str().unwrap_or("nothing");
    let key = field["cache_key"].as_str().unwrap_or("-");
    let reached = match &field[answer] {
        serde_json::Value::Array(list) if list.is_empty() => "nothing".to_string(),
        serde_json::Value::Array(list) => format!("{} tag(s)", list.len()),
        serde_json::Value::String(text) => format!("{} char(s)", text.chars().count()),
        serde_json::Value::Null => "nothing".to_string(),
        _ => "?".to_string(),
    };
    let omitted = field["omitted_tags"].as_i64().map_or(String::new(), |left| format!(", {left} more not shown"));
    // A day summary written over a gap, or whose footage has since aged out, is `answered` in the sense
    // that there is text — and saying only `answered` here is how a reader takes a third of a day for the
    // whole of it. The two flags and the fraction come along, from the payload's own fields.
    let mut qualifier = String::new();
    if field["partial"] == json!(true) {
        qualifier.push_str(" +partial");
    }
    if field["stale"] == json!(true) {
        qualifier.push_str(" +stale");
    }
    match (field["coverage"]["segments_summarised"].as_i64(), field["coverage"]["segments_total"].as_i64()) {
        (Some(done), Some(total)) if total > 0 => qualifier.push_str(&format!(" ({done}/{total} stretches)")),
        _ => {}
    }
    format!("{reached}{omitted}  [{state}{qualifier} at {granularity} width, key `{key}`]")
}

/// Where this service writes, and what is in those two directories already.
fn writes_line(runtime: &Runtime) -> String {
    let config = runtime.config();
    let period = wind_summary::dir(config, wind_summary::Kind::Period);
    let daily = wind_summary::dir(config, wind_summary::Kind::Daily);
    let held = |kind: wind_summary::Kind| wind_summary::days_present(config, kind).len();
    format!(
        "only {} ({} day file(s)) and {} ({} day file(s)); never the index, never the footage",
        runtime.shown(&period),
        held(wind_summary::Kind::Period),
        runtime.shown(&daily),
        held(wind_summary::Kind::Daily),
    )
}

/// What this install's two summary directories hold, as one `status` line — including the fact that
/// most installs hold nothing at all, which is a different claim from "the feature is broken".
fn summary_caches_line(value: &serde_json::Value) -> String {
    let caches = &value["summary_caches"];
    let side = |kind: &str, entry: &str| -> String {
        let cache = &caches[kind];
        if cache["dir_present"].as_bool() == Some(false) {
            return format!("{kind}: nothing written yet ({} does not exist)", text(&cache["dir"]));
        }
        let paragraphs = caches["entries"][entry].as_i64().unwrap_or(0);
        let days = cache["days"].as_i64().unwrap_or(0);
        let span = match (cache["first_day"].as_str(), cache["last_day"].as_str()) {
            (Some(first), Some(last)) if first != last => format!(", {first} → {last}"),
            (Some(first), _) => format!(", from {first}"),
            _ => String::new(),
        };
        format!("{kind}: {paragraphs} paragraph(s) over {days} day(s){span}")
    };
    format!("{} | {}", side("period", "period_summaries"), side("daily", "daily_summaries"))
}

/// The prompt bundle a queue carried, as one line: the four widths, not the words.
///
/// The text itself is the payload's to carry and `--json`'s to print; a report that pasted two
/// paragraphs of template between its counts would push the numbers a reader came for off a screen.
fn prompt_line(prompt: &serde_json::Value) -> String {
    let width = |section: &str, field: &str| {
        prompt[section][field].as_str().map(|body| format!("{} char(s)", body.chars().count())).unwrap_or_else(|| "absent".to_string())
    };
    format!(
        "period: system {}, user {} | daily: system {}, user {}  (--json carries the text)",
        width("period_summary", "system"),
        width("period_summary", "user"),
        width("daily_summary", "system"),
        width("daily_summary", "user"),
    )
}

/// One day's daily answer: its state, the two flags that change how it should be read, and its width.
fn daily_state_line(daily: &serde_json::Value) -> String {
    let mut line = daily["state"].as_str().unwrap_or("-").to_string();
    if daily["partial"] == json!(true) {
        line.push_str(" +partial");
    }
    if daily["stale"] == json!(true) {
        line.push_str(" +stale");
    }
    match daily["text"].as_str() {
        Some(body) => format!("{line} ({} char(s))", body.chars().count()),
        None => line,
    }
}

/// The clock part of one end of a rendered span (`"…T12:15:48+08:00"` → `"12:15:48"`).
fn time_part(iso: &str) -> String {
    iso.get(11..19).unwrap_or(iso.trim()).to_string()
}

/// A field that is legitimately empty in the data, printed as what it means rather than as nothing.
fn nonempty(value: String, when_blank: &str) -> String {
    if value.is_empty() {
        when_blank.to_string()
    } else {
        value
    }
}

/// What the two AI caches hold, including the fact a user cannot otherwise see from a terminal: that
/// `windai` tags whole months and never single days, which is why a day answer reads as a month.
fn ai_caches_line(value: &serde_json::Value) -> String {
    let caches = &value["ai_caches"];
    let shape = |name: &str| -> String {
        let cache = &caches[name];
        let years = cache["years"].as_array().map_or(0, Vec::len);
        let mut months = 0i64;
        let mut days = 0i64;
        for year in cache["years"].as_array().into_iter().flatten() {
            months += year["month_keys"].as_i64().unwrap_or(0);
            days += year["day_keys"].as_i64().unwrap_or(0);
        }
        format!("{years} file(s), {months} month + {days} day key(s), written per {}", cache["granularity_generated_by_this_build"].as_str().unwrap_or("?"))
    };
    format!("tags: {} | summary: {}", shape("tags"), shape("summary"))
}

fn truncate(text: &str, cells: usize) -> String {
    let characters: Vec<char> = text.chars().collect();
    if characters.len() <= cells {
        let padded = text.to_string();
        return format!("{padded:<cells$}");
    }
    let mut cut: String = characters[..cells.saturating_sub(1)].iter().collect();
    cut.push('…');
    cut
}

