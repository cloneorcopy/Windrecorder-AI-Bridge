//! The tools. Every one of them is a pure function from a parsed request to a JSON value.
//!
//! Nothing here knows about HTTP, JSON-RPC or the command line, which is what makes the whole
//! protocol surface testable from `cargo test` and lets `windmcp search ffmpeg` and the MCP tool
//! `windrecorder_search` be the *same code path* rather than two implementations that can drift.
//! The transport lives in `jsonrpc` and `http`; arguments arrive as already-parsed JSON and the
//! result goes out as one.
//!
//! Two rules every handler obeys:
//!
//!   * **a title is a normalised title.** [`row_view`] and [`crate::stream::titled_stream`] both run
//!     the string through [`title::normalize`], so one window reads identically in a search hit, a
//!     day event and a usage table, and matches what the recorder's own index shows.
//!   * **an empty answer is not an error.** A range with nothing in it, a keyword nobody typed, a
//!     page past the end: each returns a well-formed empty result, because an agent that cannot tell
//!     "no data" from "broken tool" retries with a wider range and then restarts the service. Only a
//!     genuinely unusable *argument* is an error, and its message names the accepted shape.
//!
//! The second rule has a corollary the AI caches forced us to write down: an empty answer must be
//! *empty for a stated reason*. [`attach_ai`] used to obey "absence is normal" by omitting its two
//! fields, which made five different facts — nothing generated yet, generated and found nothing,
//! generation failed, cache unreadable, cache keys this day at a width this build never writes —
//! indistinguishable, and indistinguishable from the tool being broken. They are now always present
//! and each names itself.

use base64::Engine as _;
use serde_json::{json, Map, Value};
use wind_base::clock::{self, LocalParts};
use wind_store::aggregate;
use wind_store::read::Row;
use wind_store::search::Query;

use crate::axis::Axis;
use crate::library;
use crate::runtime::{ai_cache_years, AiCache, Runtime};
use crate::stream;
use crate::title;

/// Characters of recognized text a search hit carries.
pub const MAX_RESULT_TEXT_CHARS: usize = 400;
/// Frames `around` returns when not told otherwise.
pub const AROUND_DEFAULT_LIMIT: usize = 10;
/// Characters of text `around` keeps per frame by default: a real screen full of OCR runs to
/// thousands, so twenty whole frames in one call is a fifteen-thousand-token answer.
pub const MAX_AROUND_TEXT_CHARS: usize = 800;
/// The ceiling an agent may ask for on `max_text_chars`, which is where "the whole screen" and "the
/// whole afternoon" are still distinguishable.
pub const TEXT_CHARS_LIMIT: usize = 100_000;
/// Events a day summary returns by default. A day with ninety title switches is noise, not a
/// summary, and the count left out is reported rather than dropped quietly.
pub const SUMMARY_DEFAULT_LIMIT: usize = 30;
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;
/// Half a day, either side of a moment. Beyond this the caller does not want frames *around* a
/// moment, they want a range query, and an unbounded window is an unbounded response.
pub const MAX_WINDOW_SECONDS: i64 = 43_200;

/// Tag strings one AI answer returns.
///
/// This ceiling used to be invisible and never bite, because it sat above a day's tag budget. It is
/// now reached in practice: the month entry that answers a day query is generated 1.5x longer than a
/// day's was — 22 strings for the shipped `ai_extract_max_tag_num` of 15 — so a full month's tags
/// *are* truncated here, and `omitted_tags` counts what was dropped. A limit that quietly shrank an
/// answer is the same class of defect as a lookup that quietly returned nothing.
pub const AI_TAGS_LIMIT: usize = 20;

/// Where the app's own AI caches live: the settings key that relocates each, and its shipped
/// directory. Both are read-only here, and neither is ever written or billed from this binary.
///
/// `tags` is what `windai tags --month` fills during idle maintenance. `summary` is upstream's
/// day-poem cache, which the install layout still creates and nothing in this build writes.
const TAGS_CACHE: (&str, &str) = ("ai_extract_tag_result_dir", "result_ai_extract_tag");
const SUMMARY_CACHE: (&str, &str) = ("ai_day_poem_result_dir", "result_ai_day_poem");

/// A rejected argument. Renders as an MCP tool error carrying `isError`, not as a transport
/// failure: the call was understood, the input was not.
#[derive(Debug)]
pub struct Rejected(pub String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Rejected {
    fn from(message: String) -> Rejected {
        Rejected(message)
    }
}

type Called = Result<Value, Rejected>;

/// Which rule turned an argument into a window. Printed beside every window this service returns.
///
/// The word "day" is the one that must never be ambiguous here. `windcapctl query --day 2026-09-22`
/// answered with the product day 03:00 → 02:59, and `windmcp app-usage --day 2026-09-22` answered
/// with the calendar day 00:00 → 23:59: the same word, two intervals, and the disagreement was
/// exactly the 00:00-to-03:00 band a user is most likely to be searching. Naming the rule next to
/// the window is what makes that impossible to read past, in either direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// A whole product day, from [`day_window`] and so from `wind_base::clock::day_bounds`.
    Day,
    /// `start`/`end` exactly as the caller typed them.
    Explicit,
    /// A moment plus `window_seconds` either side of it.
    Centered,
}

impl Rule {
    pub fn label(self) -> &'static str {
        match self {
            Rule::Day => "day",
            Rule::Explicit => "explicit",
            Rule::Centered => "centered",
        }
    }
}

/// How wide an AI answer reaches, and therefore how far it can be from the day it was asked about.
///
/// The same trap [`Rule`] was introduced for, one layer up: `windai` generates activity tags once a
/// **month**, and a client asking about one **day** can legitimately be answered with them — but
/// only if the payload says so. "What was this user doing on 2026-09-22?" answered with September's
/// tags is a correct answer to a slightly different question; the same bytes with no label are a
/// wrong answer, and the reader supplies the label it expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// A `YYYY-MM-DD` entry: this day, and no other.
    Day,
    /// A `YYYY-MM` entry: every day in the month this one is inside.
    Month,
}

impl Granularity {
    pub fn label(self) -> &'static str {
        match self {
            Granularity::Day => "day",
            Granularity::Month => "month",
        }
    }
}

/// Whether an AI cache answered, and if not, which kind of not.
///
/// Five states because the cache really does have five things to say. Before this existed all five
/// were "the field is not in the response", which is also what a malformed file, a feature that was
/// never generated, and a feature that generated nothing look like from outside — so an agent could
/// not tell a month it should ask again about from a day that will never have an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiState {
    /// An entry was found and it says something.
    Answered,
    /// An entry exists and is empty: the period was processed and produced nothing to say. Final,
    /// not a failure, and not worth retrying.
    GeneratedEmpty,
    /// An entry exists and holds upstream's `retry_times:N` marker — generation failed, and what is
    /// on disk is a to-do item that the old web UI used to render as a visible tag pill.
    GenerationFailed,
    /// Nothing has been generated for the period this day belongs to.
    NotGenerated,
    /// An entry is there but is not the shape the cache format promises.
    MalformedEntry,
    /// The cache file exists and could not be read or parsed at all.
    Unreadable,
}

impl AiState {
    pub fn label(self) -> &'static str {
        match self {
            AiState::Answered => "answered",
            AiState::GeneratedEmpty => "generated_empty",
            AiState::GenerationFailed => "generation_failed",
            AiState::NotGenerated => "not_generated",
            AiState::MalformedEntry => "malformed_entry",
            AiState::Unreadable => "unreadable",
        }
    }

    /// Whether this state means the response carries a real answer. The one place that decides it,
    /// so `available` and `tags`/`text` cannot disagree.
    pub fn answered(self) -> bool {
        self == AiState::Answered
    }
}

/// An inclusive window on the stored axis, and how it was arrived at.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub from: i64,
    pub to: i64,
    pub rule: Rule,
    /// The install's `day_begin_minutes`, carried on *every* window whether or not this rule used
    /// it. `windcapctl`'s `Span` does the same: a reader who sees a window without the shift has no
    /// way to tell a 03:00 product day from a calendar day, which is the whole bug.
    pub day_begin_minutes: i64,
}

impl Window {
    /// The payload form. `start`/`end`/`from`/`to` keep the shape the bridge has always emitted; the
    /// three fields after them are the ones that make two commands comparable.
    pub(crate) fn json(&self, axis: &Axis) -> Value {
        json!({
            "start": axis.render(self.from),
            "end": axis.render(self.to),
            "from": self.from,
            "to": self.to,
            "seconds": self.to - self.from + 1,
            "rule": self.rule.label(),
            "day_begin_minutes": self.day_begin_minutes,
            "day_begin": self.day_begin_label(),
        })
    }

    /// The one line a terminal report prints: the window *and* the rule that produced it, read back
    /// out of the payload so the report can never quote a window the tool did not search.
    pub fn label(&self, axis: &Axis) -> String {
        format!(
            "{} → {} ({}, day_begin {})",
            axis.render(self.from),
            axis.render(self.to),
            self.rule.label(),
            self.day_begin_label()
        )
    }

    /// `HH:MM` for the shift that produced a day window.
    pub fn day_begin_label(&self) -> String {
        day_begin_label(self.day_begin_minutes)
    }
}

/// `HH:MM` for a `day_begin_minutes` value — the shape every report quotes it in.
pub fn day_begin_label(minutes: i64) -> String {
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60)
}

/// A named day, as the app counts it.
///
/// **This is the only day arithmetic in this binary.** `wind_base::clock::day_bounds` is the shared
/// helper and `windcapctl`'s only route from a date to a window; calling the same function from here
/// is what makes "one word, two intervals" a structural impossibility instead of a convention to
/// remember. `windrecorder_search`, `windrecorder_app_usage` and `windrecorder_day_summary` each
/// reach a day through this one function, and `tests/bridge.rs` drives all three from real argv and
/// compares their windows, so a command that grows its own arithmetic fails a test rather than a
/// user's cross-check.
pub fn day_window(runtime: &Runtime, axis: &Axis, text: &str) -> Result<Window, Rejected> {
    let given = axis.parse(text)?;
    let when = LocalParts::from_naive_epoch(given).date_only();
    let day_begin_minutes = runtime.day_begin_minutes();
    let (from, to) = clock::day_bounds(when.year, when.month, when.day, day_begin_minutes);
    Ok(Window { from, to, rule: Rule::Day, day_begin_minutes })
}

/// The name of a tool as it appears on the wire, shared by the schema table and the dispatcher.
pub const NAMES: [&str; 11] = [
    "windrecorder_status",
    "windrecorder_search",
    "windrecorder_around",
    "windrecorder_app_usage",
    "windrecorder_day_summary",
    "windrecorder_frame",
    "windrecorder_summaries_pending",
    "windrecorder_summaries_read",
    "windrecorder_prompts_read",
    "windrecorder_period_summary_write",
    "windrecorder_day_summary_write",
];

/// Dispatch one tool call. The only place a tool name is matched, in this binary and in the CLI.
///
/// Matching a name is the same act as refusing an argument that never went with it, so both happen
/// here and nowhere else: [`crate::jsonrpc::unexpected_arguments`] reads the schema the client was
/// handed, and a caller that typed `window_second` is told so before a query runs. The command line
/// arrives through this function too, which is why `windmcp around --window-second 5` and the MCP
/// tool call cannot mean different things.
pub fn call(runtime: &Runtime, axis: &Axis, name: &str, args: &Value) -> Called {
    let empty = json!({});
    let args = if args.is_null() { &empty } else { args };
    if let Err(refused) = crate::jsonrpc::unexpected_arguments(axis, name, args) {
        return Err(Rejected(refused));
    }
    match name {
        "windrecorder_status" => Ok(status(runtime, axis)),
        "windrecorder_search" => search(runtime, axis, args),
        "windrecorder_around" => around(runtime, axis, args),
        "windrecorder_app_usage" => app_usage(runtime, axis, args),
        "windrecorder_day_summary" => day_summary(runtime, axis, args),
        "windrecorder_frame" => frame(runtime, axis, args),
        // The summary surface. Three reads and two writes; the writes land in `userdata/result_ai_*`,
        // which is the only place in this binary that a file is created or replaced, and never in the
        // index. See `summaries` for why the queue is the first call of the sequence.
        "windrecorder_summaries_pending" => crate::summaries::pending(runtime, axis, args),
        "windrecorder_summaries_read" => crate::summaries::read(runtime, axis, args),
        "windrecorder_prompts_read" => crate::summaries::prompts(runtime, axis, args),
        "windrecorder_period_summary_write" => crate::summaries::write_period(runtime, axis, args),
        "windrecorder_day_summary_write" => crate::summaries::write_day(runtime, axis, args),
        other => Err(Rejected(format!("unknown tool {other}; this bridge offers {}", NAMES.join(", ")))),
    }
}

/// `windrecorder_status` — what data exists, how fresh it is, and what the bridge can see.
///
/// The per-database list is keyed `databases`, not `months`: an entry is one *file*, and a month can
/// hold one file per user. There is deliberately no `search_semantics` field — the matching rules
/// belong in the tool description, where an agent reads them before it calls, not in a payload it
/// has to fetch first. That was decided once already and is not an oversight.
///
/// `clock` and `day_begin` are both here for the same reason: they are the two conventions that make
/// a number in this service mean what it says, and a client that cannot see them cannot tell a
/// correct answer from an eight-hour- or three-hour-shifted one.
pub fn status(runtime: &Runtime, axis: &Axis) -> Value {
    let (facts, unreadable, ms) = runtime.facts();
    let total_rows: i64 = facts.iter().map(|f| f.rows).sum();
    let earliest = facts.iter().filter_map(|f| f.bounds.map(|b| b.0)).min();
    let latest = facts.iter().filter_map(|f| f.bounds.map(|b| b.1)).max();
    let (lock_present, lock_pid, lock_age) = runtime.recorder_lock();
    // Both sides of this subtraction are on the stored axis, so the offset cancels and the result is
    // a real age even though neither input is a POSIX timestamp.
    let minutes_since = latest.map(|last| ((clock::now().naive_epoch_seconds() - last).max(0) as f64 / 60.0).round() as i64);

    json!({
        "root": runtime.root().display().to_string(),
        "user_name": runtime.config().user_name(),
        "has_data": total_rows > 0,
        "total_rows": total_rows,
        "total_segments": facts.iter().map(|f| f.segments).sum::<i64>(),
        "first_record": earliest.map(|at| axis.render(at)),
        "last_record": latest.map(|at| axis.render(at)),
        "minutes_since_last_record": minutes_since,
        "databases": facts.iter().map(|fact| json!({
            "database": library::name(&fact.month),
            "month": format!("{:04}-{:02}", fact.month.year, fact.month.month),
            "rows": fact.rows,
            "segments": fact.segments,
            "first_record": fact.bounds.map(|b| axis.render(b.0)),
            "last_record": fact.bounds.map(|b| axis.render(b.1)),
            // A month predating the win_title column genuinely has no titles; saying so is the
            // difference between "that day was untitled" and "I cannot tell".
            "missing_columns": fact.missing_columns,
        })).collect::<Vec<Value>>(),
        "videos_dir_present": runtime.videos_dir().is_dir(),
        "recorder_lock": {
            "present": lock_present,
            "pid": lock_pid.map(|p| p as i64),
            "lock_age_seconds": lock_age,
        },
        "excluded_title_words": runtime.exclude_words().len(),
        // Which of the two AI caches exist, and in which of the two key shapes. `day_summary` can only
        // ever answer from these, and what it answers with depends on the answer: a cache of month
        // keys serves a day at month width, a cache of day keys serves the day itself, and an absent
        // one serves nothing. A client that can see that here does not have to discover it by
        // receiving an empty `ai_tags` and guessing which of the three happened.
        "ai_caches": ai_caches(runtime),
        // The two directories this feature owns. Named here because a client deciding whether to
        // summarise a day first has to know whether any day has ever been summarised at all, and
        // because these are the only files in the install this service writes.
        "summary_caches": summary_caches(runtime),
        // Which axis every integer here lives on, stated where a client sees it before handing one
        // back to `around` or `frame`.
        "clock": {
            "epoch": "naive-local",
            "note": "a timestamp is videofile_time: the local wall clock counted as if it were UTC, \
                     i.e. POSIX seconds plus utc_offset_seconds. Pass it back unchanged; do not feed \
                     it to a POSIX converter.",
            "utc_offset_seconds": axis.utc_offset_seconds,
            "utc_offset": axis.offset_literal(),
        },
        // The other convention a number in this service silently depends on, and the one that used to
        // be applied inconsistently: a "day" is the product day, not the calendar day.
        "day_begin": {
            "at": day_begin_label(runtime.day_begin_minutes()),
            "minutes": runtime.day_begin_minutes(),
            "note": "a `day` argument is [day HH:MM:00, next day HH:MM:59] as Windrecorder counts it, \
                     so a frame at 01:00 belongs to the previous day. `start`/`end` are honoured \
                     exactly and carry no shift.",
        },
        "read_ms": round_ms(ms),
        "unreadable_databases": unreadable,
    })
}

/// The state of the two AI caches `windrecorder_day_summary` draws on, year by year.
///
/// Reads a directory listing and up to a handful of small JSON files, and issues no model call: these
/// are caches the app's own idle maintenance writes, so looking at them is free. The point of naming
/// the counts is that the interesting fact about this install's tags is *which key shape* they are
/// in, not how many there are — a year of month keys is a working native tagger, a year of day keys
/// is a legacy Python cache, and an empty directory is a feature that has never run. `day_summary`
/// answers differently for each, and this is where a client finds out before it asks.
fn ai_caches(runtime: &Runtime) -> Value {
    let mut out = Map::new();
    for (name, cache, generated) in [
        ("tags", TAGS_CACHE, "month"),
        ("summary", SUMMARY_CACHE, "none"),
    ] {
        let dir = runtime.config().result_dir(cache.0, cache.1);
        let years: Vec<Value> = ai_cache_years(&dir)
            .into_iter()
            .map(|(year, path)| {
                let file = runtime.ai_cache_at(cache.0, cache.1, year);
                let (months, days) = file.key_shapes();
                json!({
                    "file": runtime.shown(&path),
                    "readable": file.readable,
                    "month_keys": months,
                    "day_keys": days,
                })
            })
            .collect();
        out.insert(
            name.to_string(),
            json!({
                "dir": runtime.shown(&dir),
                "dir_present": dir.is_dir(),
                "years": years,
                // Which granularity this binary ever writes. `month` is `windai tags --month`;
                // `none` is the day poem, whose generator was never ported, so a summary cache on a
                // native-only install is legacy data or empty.
                "granularity_generated_by_this_build": generated,
            }),
        );
    }
    out.insert(
        "note".to_string(),
        json!(
            "`windai tags --month` writes one `YYYY-MM` entry per month and no `YYYY-MM-DD` entry \
             per day. `windrecorder_day_summary` therefore answers a day from the month that \
             contains it and labels that answer `ai_tags.granularity: \"month\"`. Day-level \
             summaries (`ai_summary`) have no generator in this build and no month-level equivalent \
             to fall back on; a `not_generated` there is the normal answer, not a fault."
        ),
    );
    Value::Object(out)
}

/// How much of the library has been summarised, per family, and which days.
///
/// A directory listing and nothing else: no index read, no model call. What it answers is the question
/// a client cannot ask a per-day tool — "has this feature ever run here" — which on an install with no
/// summaries at all is the difference between planning ninety writes and planning none.
fn summary_caches(runtime: &Runtime) -> Value {
    let mut out = Map::new();
    for (name, kind) in [("period", wind_summary::Kind::Period), ("daily", wind_summary::Kind::Daily)] {
        let dir = wind_summary::dir(runtime.config(), kind);
        let days = wind_summary::days_present(runtime.config(), kind);
        out.insert(
            name.to_string(),
            json!({
                "dir": runtime.shown(&dir),
                "dir_present": dir.is_dir(),
                "days": days.len(),
                "first_day": days.first(),
                "last_day": days.last(),
            }),
        );
    }
    out.insert(
        "entries".to_string(),
        json!({
            "period_summaries": wind_summary::days_present(runtime.config(), wind_summary::Kind::Period)
                .iter()
                .map(|day| wind_summary::read_period(runtime.config(), day).len())
                .sum::<usize>(),
            "daily_summaries": wind_summary::days_present(runtime.config(), wind_summary::Kind::Daily).len(),
        }),
    );
    out.insert(
        "note".to_string(),
        json!(
            "`windrecorder_summaries_pending` reports what is left; these two directories are the only \
             place this service writes, and `windmaint forget` prunes them with the text they were \
             derived from."
        ),
    );
    Value::Object(out)
}

/// `windrecorder_search` — keyword and time-range search over recognized text and window titles.
pub fn search(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let window = resolve_range(runtime, axis, args)?;
    let query = Query {
        limit: Some(limit_arg(args, DEFAULT_LIMIT)?),
        offset: optional_usize(args, "offset", 0, usize::MAX / 2)?.unwrap_or(0),
        ..Query::new(window.from, window.to).with_keywords(&text_arg(args, "keywords")?).with_exclude(&text_arg(args, "exclude")?)
    };
    let (found, skipped, ms) = runtime.run_search(&query)?;

    // The store merges oldest-first because that is what the UI's table wants; the bridge promises
    // newest-first because an agent paging back through history wants the most recent hit first, and
    // `offset` is documented as "skip this many newest matches".
    let mut rows = found.rows;
    rows.reverse();
    let shown = rows.len();
    let results: Vec<Value> = rows.iter().map(|row| row_view(row, axis, MAX_RESULT_TEXT_CHARS)).collect();

    let mut out = json!({
        "range": window.json(axis),
        "keywords": query.tokens,
        "exclude": query.exclude,
        "total_matches": found.total,
        "returned": shown,
        "offset": query.offset,
        "has_more": found.total > query.offset as i64 + shown as i64,
        "results": results,
        "read_ms": round_ms(ms),
    });
    note_skipped(&mut out, skipped);
    Ok(out)
}

/// `windrecorder_around` — the frames nearest one moment, in time order.
pub fn around(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let center = axis.parse(&time_arg(args, "timestamp", axis)?)?;
    let window = bounded_i64(args, "window_seconds", 120, 1, MAX_WINDOW_SECONDS)?;
    let limit = limit_arg(args, AROUND_DEFAULT_LIMIT)?;
    // `max_text_chars = 0` means "the whole thing", which is the documented escape hatch for the one
    // moment that really needs it.
    let max_text = optional_usize(args, "max_text_chars", 0, TEXT_CHARS_LIMIT)?.unwrap_or(MAX_AROUND_TEXT_CHARS);

    let span = Window { from: center - window, to: center + window, rule: Rule::Centered, day_begin_minutes: runtime.day_begin_minutes() };
    let (mut rows, skipped) = runtime.rows_in(span.from, span.to);
    // Rank by distance *before* taking the limit, so a truncated read keeps the frames nearest the
    // moment rather than whichever month happened to open first.
    let available = rows.len();
    rows.sort_by_key(|row| ((row.time - center).abs(), row.time, row.rowid));
    let mut chosen: Vec<Row> = rows.into_iter().take(limit).collect();
    chosen.sort_by_key(|row| (row.time, row.rowid));

    let frames: Vec<Value> = chosen.iter().map(|row| row_view(row, axis, max_text)).collect();
    let mut out = json!({
        "center": axis.render(center),
        "center_timestamp": center,
        "range": span.json(axis),
        "frames": frames,
        "in_range": available,
        "text_chars_per_frame": max_text,
    });
    // Its own field, not `skipped_databases`: that one means "a file could not be read", and filing
    // "you asked for more than this returns" under it would send an agent off to look for index
    // corruption when the answer is simply to narrow the window.
    if available > chosen.len() {
        out["warning"] = json!(format!(
            "{available} frames fall in this window and {} were returned; narrow window_seconds",
            chosen.len()
        ));
    }
    note_skipped(&mut out, skipped);
    Ok(out)
}

/// `windrecorder_app_usage` — time spent under each foreground window title, ranked.
pub fn app_usage(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let window = resolve_range(runtime, axis, args)?;
    let limit = limit_arg(args, DEFAULT_LIMIT)?;
    app_usage_in(runtime, axis, &window, limit)
}

/// The measurement, with the window already resolved.
///
/// `day_summary` reaches the numbers through the same [`stream::credit`] call, and the test that
/// keeps the two honest calls these two functions with identical bounds: if a refactor ever gives
/// either one its own idea of what a second is worth, that test fails instead of a user noticing
/// that the day adds up to less than the week. The window travels as one [`Window`] rather than as
/// two integers so that the rule that produced it cannot be lost on the way.
pub fn app_usage_in(runtime: &Runtime, axis: &Axis, window: &Window, limit: usize) -> Called {
    let mut skipped = Vec::new();
    let titled = stream::titled_stream(runtime, window.from, window.to, &mut skipped);
    let credits = stream::credit(&titled, stream::successor(runtime, window.to).as_ref());
    let ranked = stream::by_title(&titled, &credits);

    let mut out = json!({
        "range": window.json(axis),
        "titled_frames": titled.titled,
        "distinct_titles": ranked.len(),
        "total_counted_seconds": credits.total,
        "withheld_excluded_titles": titled.withheld,
        "usage": ranked.iter().take(limit).map(|(title, seconds)| json!({
            "window_title": title,
            "seconds": seconds,
            "share_of_counted_time": share(*seconds, credits.total),
        })).collect::<Vec<Value>>(),
    });
    if ranked.len() > limit {
        out["omitted_titles"] = json!(ranked.len() - limit);
    }
    note_skipped(&mut out, skipped);
    Ok(out)
}

/// `windrecorder_day_summary` — one day as merged dated events, carrying no screen text at all.
///
/// The `date` this tool answers with is the first day of the window it searched, so the label can
/// only ever name a day whose events are actually in the payload; and the window itself is printed
/// alongside it, because a summary titled 2026-09-22 that counts 03:00 → 02:59 is only honest while
/// it says so.
pub fn day_summary(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let limit = limit_arg(args, SUMMARY_DEFAULT_LIMIT)?;
    let window = day_window(runtime, axis, &date_arg_required(args, "date", axis)?)?;
    let (from, to) = (window.from, window.to);

    let mut skipped = Vec::new();
    let titled = stream::titled_stream(runtime, from, to, &mut skipped);
    let credits = stream::credit(&titled, stream::successor(runtime, to).as_ref());
    let kept: Vec<_> =
        stream::runs(&titled, &credits).into_iter().filter(|run| run.seconds > stream::MIN_CREDITED_SECONDS).collect();

    let date = LocalParts::from_naive_epoch(from).date_stamp();
    let mut out = json!({
        "date": date,
        "range": window.json(axis),
        "frames": titled.titled,
        "withheld_excluded_titles": titled.withheld,
        "total_counted_seconds": credits.total,
        "total_events": kept.len(),
        "events": kept.iter().take(limit).map(|run| event_json(&axis, run)).collect::<Vec<Value>>(),
    });
    if kept.len() > limit {
        out["omitted_events"] = json!(kept.len() - limit);
    }
    attach_ai(runtime, &mut out, &date);
    note_skipped(&mut out, skipped);
    Ok(out)
}

/// That day's AI tags and one-line summary, both from caches the app's idle maintenance fills.
///
/// Both fields are always present, as objects that carry their own [`AiState`] and [`Granularity`],
/// rather than being omitted when there is nothing to put in them. Omission was the defect: see
/// [`ai_tags`] for the day-versus-month key mismatch that made a working feature return nothing on
/// every native install, and [`AiState`] for why four different kinds of "nothing" had to stop
/// sharing one shape with a fifth that meant "broken".
fn attach_ai(runtime: &Runtime, out: &mut Value, date: &str) {
    out["ai_tags"] = ai_tags(runtime, date);
    out["ai_summary"] = ai_summary(runtime, date);
}

/// The tags entry for one day, resolved out of a cache that is keyed by month.
///
/// `userdata/result_ai_extract_tag/{year}.json` is one file shared by two granularities. Upstream
/// wrote both a `YYYY-MM-DD` entry (its day pass) and a `YYYY-MM` entry (its month pass) into it;
/// the native `windai tags --month`, which is what the idle maintenance pass actually schedules,
/// writes only the month key. Reading the day key alone — which is what this function did — is
/// therefore a lookup that succeeds on a legacy Python cache and fails on every install that has run
/// the native tagger, with no error and no trace, and the failure looks exactly like a day nobody
/// tagged. So: take the day entry when there is one, otherwise resolve to the month that day is
/// inside and *say which of the two answered*.
///
/// A day entry always wins, including when it holds `[]`. Upstream writes `[]` on purpose for a
/// period whose every window title was filtered out, and silently preferring the month's longer,
/// better-looking list to that day's emptiness would be the tool picking the more flattering reply
/// over the true one — the same substitution this payload exists to make visible.
fn ai_tags(runtime: &Runtime, date: &str) -> Value {
    let year = year_of(date);
    let cache = runtime.ai_cache_at(TAGS_CACHE.0, TAGS_CACHE.1, year);
    let file = cache.shown.as_str();
    // A file that is simply not there is not an unreadable one: that answer is `not_generated`, and
    // it comes out of the empty `entries` below. Only a file that exists and will not parse is a
    // different fact — and it must not be allowed to borrow that answer.
    if cache.exists && !cache.readable {
        return ai_field(
            AiState::Unreadable,
            None,
            None,
            &cache,
            &format!(
                "no tags are reported for {date}: {file} exists and cannot be read as a tag cache. \
                 This is not \"there are no tags\" — the answer for this day is unknown."
            ),
            json!([]),
        );
    }
    let month = month_of(date);
    let (granularity, key) = match cache.entries.contains_key(date) {
        true => (Granularity::Day, date.to_string()),
        false => (Granularity::Month, month.to_string()),
    };
    let Some(entry) = cache.entries.get(&key) else {
        // Neither key. The two reasons a client will care about — this file tags other months but
        // not this one, and this file has never been written — get different sentences, because
        // only one of them is fixed by running `windai tags --month {month}`.
        let (months, days) = cache.key_shapes();
        let note = match (cache.exists, months, days) {
            (false, _, _) => format!(
                "no tags have been generated for {date}: {file} does not exist. `windai tags --month \
                 {month}` generates them, and the idle maintenance pass does the same when \
                 enable_ai_extract_tag is on."
            ),
            _ => format!(
                "no tags have been generated for {month}, the month {date} falls in, so none can be \
                 given for the day either. {file} holds {months} month and {days} day entries; none \
                 of them cover this date. `windai tags --month {month}` would produce one."
            ),
        };
        return ai_field(AiState::NotGenerated, None, None, &cache, &note, json!([]));
    };

    let Some(list) = entry.as_array() else {
        return ai_field(
            AiState::MalformedEntry,
            None,
            Some(&key),
            &cache,
            &format!("the `{key}` entry in {file} is not a list of tags, so it is not reported as one"),
            json!([]),
        );
    };
    if list.is_empty() {
        return ai_field(
            AiState::GeneratedEmpty,
            Some(granularity),
            Some(&key),
            &cache,
            &match granularity {
                Granularity::Day => format!(
                    "{key} was tagged and the model returned no tags for it. This day has an answer \
                     and it is this: nothing to report."
                ),
                Granularity::Month => format!(
                    "{key} was tagged and produced no tags, so the month has no attributable window \
                     titles. There is nothing to report for any day inside it, {date} included."
                ),
            },
            json!([]),
        );
    }
    let tags: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
    if tags.len() == 1 && tags[0].starts_with("retry_times") {
        return ai_field(
            AiState::GenerationFailed,
            None,
            Some(&key),
            &cache,
            &format!(
                "tagging {key} failed and the cache holds upstream's retry marker `{}` in place of \
                 tags. Nothing is claimed about this day; asking again is the way to find out.",
                tags[0]
            ),
            json!([]),
        );
    }
    if tags.is_empty() {
        return ai_field(
            AiState::MalformedEntry,
            None,
            Some(&key),
            &cache,
            &format!("the `{key}` entry in {file} holds {} values and not one of them is a string", list.len()),
            json!([]),
        );
    }

    let kept: Vec<&str> = tags.iter().copied().take(AI_TAGS_LIMIT).collect();
    let note = match granularity {
        // The sentence that stops a month's answer being read as a day's. Everything else in this
        // object is machine-checkable; this one is for the reader who skims.
        Granularity::Month => format!(
            "These tags are {key}'s: they summarise the whole of {key} and not {date} on its own. \
             Windrecorder generates activity tags per month, and no day-level entry exists for this \
             date, so the month the day falls in is the widest true answer. Treat them as the \
             month's theme, not as what this day was doing."
        ),
        Granularity::Day => format!("These tags were generated for {date} itself, from this day's window titles."),
    };
    let mut field = ai_field(AiState::Answered, Some(granularity), Some(&key), &cache, &note, json!(kept));
    // Reported rather than dropped, exactly as `omitted_events` does for a busy day.
    if tags.len() > kept.len() {
        field["omitted_tags"] = json!(tags.len() - kept.len());
    }
    field
}

/// The day's own summary, as the AI summary feature writes it, or `None` when nothing has been.
///
/// `state` distinguishes the four things a reader can otherwise conflate: written and standing
/// (`answered`), written over a gap it admits to (`partial`), written and no longer matching the
/// stretches it was built from (`stale`), and a file that exists but is not a summary (`unreadable`).
/// An absent file is not this function's answer at all — it falls through to the legacy cache, because
/// a Python-era poem is still a real summary of that day.
fn native_day_summary(runtime: &Runtime, date: &str) -> Option<Value> {
    let stored = wind_summary::read_daily(runtime.config(), date);
    if !stored.exists {
        return None;
    }
    let cache = AiCache {
        path: stored.path.clone(),
        shown: runtime.shown(&stored.path),
        exists: true,
        readable: stored.readable,
        entries: serde_json::Map::new(),
    };
    if !stored.readable {
        let note = format!("no day summary is reported for {date}: {}. This is not \"there is no summary\" — the answer is unknown.", stored.note);
        return Some(ai_field(AiState::Unreadable, None, None, &cache, &note, Value::Null));
    }
    let written = stored.summary?;
    let whole = written.coverage.complete() && !written.partial && !written.stale;
    let note = if whole {
        format!(
            "every recorded stretch of {date} had a summary when this was written, so it reads as a whole \
             day; the per-stretch paragraphs it was built from are in the directory named in `period_dir`."
        )
    } else {
        format!(
            "this summary of {date} was written over {} of {} stretches, and the day says so in `partial`, \
             `stale` and `coverage.missing`. Present it as part of the day, not as the whole of it.",
            written.coverage.segments_summarised, written.coverage.segments_total
        )
    };
    let mut field = ai_field(AiState::Answered, Some(Granularity::Day), Some(date), &cache, &note, json!(written.text));
    field["written_at"] = json!(written.written_at);
    field["written_by"] = json!(written.written_by);
    field["model"] = json!(written.model);
    field["partial"] = json!(written.partial);
    field["stale"] = json!(written.stale);
    field["coverage"] = json!({
        "segments_total": written.coverage.segments_total,
        "segments_summarised": written.coverage.segments_summarised,
        "missing": written.coverage.missing,
    });
    field["period_dir"] = json!(runtime.shown(&wind_summary::dir(runtime.config(), wind_summary::Kind::Period)));
    Some(field)
}

/// The one-line summary upstream called the day poem, from `result_ai_day_poem/{year}.json`.
///
/// **There is no month fallback here, on purpose**, and the reason is the shape of the cache rather
/// than a missed case. Upstream generated this file one entry per *day* and built its month view by
/// listing the days of a month in order — there is no `YYYY-MM` entry anywhere to fall back to. A
/// month's *tags* are a coarser answer to the same question about the same window titles; a day's
/// poem rendered from a month's tags would be a different kind of thing altogether, written by a
/// model that was never asked, under a field named for the day. Substituting it would move the
/// deception one layer up rather than removing it, which is the one outcome this whole payload
/// exists to prevent.
///
/// What this build writes into the file is nothing: the day-poem generator was never ported, so the
/// honest default answer is `not_generated`, said out loud. A `YYYY-MM-DD` entry is still served
/// when one is present, because the install layout creates this directory and a cache left behind by
/// the old Python app holds real summaries.
fn ai_summary(runtime: &Runtime, date: &str) -> Value {
    // Three levels, most recent first: what this feature writes, then what the old Python app wrote,
    // then the honest absence. The native file is preferred because it is the one whose premise the
    // bridge can check — a poem cache says a day was summarised, a summary row says which stretches it
    // was written from and whether those still stand.
    if let Some(answer) = native_day_summary(runtime, date) {
        return answer;
    }
    let year = year_of(date);
    let cache = runtime.ai_cache_at(SUMMARY_CACHE.0, SUMMARY_CACHE.1, year);
    let file = cache.shown.as_str();
    if cache.exists && !cache.readable {
        return ai_field(
            AiState::Unreadable,
            None,
            None,
            &cache,
            &format!(
                "no day summary is reported for {date}: {file} is there and cannot be read as a \
                 summary cache. This is not \"there is no summary\" — the answer is unknown.",
            ),
            Value::Null,
        );
    }
    let Some(entry) = cache.entries.get(date) else {
        let where_things_stand = match cache.exists {
            true => format!("{file} exists and holds no entry for this date"),
            false => format!("{file} does not exist"),
        };
        return ai_field(
            AiState::NotGenerated,
            None,
            None,
            &cache,
            &format!(
                "no day summary exists for {date}, and there is no month-level summary that could \
                 stand in for one: upstream generated these per day only, and nothing in this build \
                 writes them — {where_things_stand}. This says nothing about the day, only that \
                 nobody has summarised it. It is also not the same question `ai_tags` answers: those \
                 are keywords for a period, this would be a sentence about one day."
            ),
            Value::Null,
        );
    };
    let Some(text) = entry.as_str() else {
        return ai_field(
            AiState::MalformedEntry,
            None,
            Some(date),
            &cache,
            &format!("the `{date}` entry in {file} is not a string, so it is not reported as a summary"),
            Value::Null,
        );
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ai_field(
            AiState::GeneratedEmpty,
            Some(Granularity::Day),
            Some(date),
            &cache,
            &format!("a summary was generated for {date} and it came back empty. This day has an \
                      answer and it is this: nothing to say."),
            Value::Null,
        );
    }
    if trimmed.starts_with("retry_times") {
        return ai_field(
            AiState::GenerationFailed,
            None,
            Some(date),
            &cache,
            &format!(
                "summarising {date} failed and the cache holds upstream's retry marker \
                 `{trimmed}` in place of prose. That marker is not a summary and is never returned \
                 as one."
            ),
            Value::Null,
        );
    }
    ai_field(
        AiState::Answered,
        Some(Granularity::Day),
        Some(date),
        &cache,
        &format!("This summary was written for {date} itself. Day-level summaries are generated per \
                  day; no month-level summary exists."),
        json!(trimmed),
    )
}

/// The shared half of an AI answer: its state, its width, the cache key it came from, the file that
/// holds it, and one sentence a reader can act on. The caller supplies the answer itself, under
/// `tags` or `text`.
///
/// `granularity` and `key` are `null` whenever there is no answer, so the payload cannot point at a
/// width it did not use. `available` is derived from the state rather than passed in, for the same
/// reason the CLI's window line is read back out of the payload: two fields a caller sets separately
/// are two fields that can disagree.
fn ai_field(state: AiState, granularity: Option<Granularity>, key: Option<&str>, cache: &AiCache, note: &str, answer: Value) -> Value {
    let (name, value): (&str, Value) = match answer {
        Value::Array(list) => ("tags", json!(list)),
        other => ("text", other),
    };
    let mut out = Map::new();
    out.insert("state".into(), json!(state.label()));
    out.insert("available".into(), json!(state.answered()));
    out.insert("granularity".into(), granularity.map_or(Value::Null, |g| json!(g.label())));
    out.insert("cache_key".into(), key.map_or(Value::Null, |key| json!(key)));
    out.insert(name.into(), value);
    out.insert("file".into(), json!(cache.shown));
    // The one discriminator `state` cannot carry on its own: `not_generated` because this period was
    // never asked about, and `not_generated` because nobody has ever run the tagger here, are the
    // same state and two different facts. Named as a boolean so a client can branch without reading
    // the sentence — which is exactly the distinction that used to be invisible from outside.
    out.insert("cache_file_present".into(), json!(cache.exists));
    out.insert("note".into(), json!(note));
    Value::Object(out)
}

/// The year a `YYYY-MM-DD` stamp names. `date` always arrives from [`LocalParts::date_stamp`], so
/// the slices are in bounds; the fallbacks exist so a malformed stamp reads as year 0 — a cache file
/// that will not exist — rather than as a panic in a read-only tool.
fn year_of(date: &str) -> i64 {
    date.get(..4).and_then(|head| head.parse().ok()).unwrap_or(0)
}

/// The month a `YYYY-MM-DD` stamp falls in, which is the key `windai` writes.
fn month_of(date: &str) -> &str {
    date.get(..7).unwrap_or(date)
}

/// `windrecorder_frame` — the stored preview nearest a moment, plus where its real image is.
pub fn frame(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let center = axis.parse(&time_arg(args, "timestamp", axis)?)?;
    let window = bounded_i64(args, "window_seconds", 900, 1, MAX_WINDOW_SECONDS)?;
    frame_at(runtime, axis, center, window).ok_or_else(|| Rejected(format!("no stored frame within {window}s of {}", axis.render(center))))
}

/// The row nearest `center`, with its thumbnail resolved, or `None` if the window holds nothing.
pub fn frame_at(runtime: &Runtime, axis: &Axis, center: i64, window: i64) -> Option<Value> {
    let span = Window { from: center - window, to: center + window, rule: Rule::Centered, day_begin_minutes: runtime.day_begin_minutes() };
    let (rows, _skipped) = runtime.rows_in(span.from, span.to);
    let row = aggregate::nearest(&rows, center, window)?;
    // The column is TEXT holding base64, and upstream leaves it empty rather than null when a row
    // was indexed without a preview, so both spellings mean the same thing.
    let bytes = row
        .thumbnail
        .as_deref()
        .map(|stored| base64::engine::general_purpose::STANDARD.decode(stored).unwrap_or_default())
        .filter(|bytes| !bytes.is_empty());
    let mime = bytes.as_deref().and_then(sniff_image_format);

    Some(json!({
        "timestamp": row.time,
        "time": axis.render(row.time),
        "range": span.json(axis),
        "distance_seconds": (row.time - center).abs(),
        "video_file": row.videofile_name,
        "picture_file": row.picturefile_name,
        "offset_in_segment": row.offset_in_segment(),
        "window_title": row.title().and_then(title::normalize),
        "url": row.deep_linking.clone().filter(|url| !url.trim().is_empty()),
        "text": row.body(),
        "text_chars": row.body().chars().count(),
        // Where the full-resolution capture and the segment are on disk, resolved by the same
        // stamp-prefix rules the UI uses: a slice directory gains a pipeline marker after the
        // segment closes, so the stored name is never the whole path.
        "frame_path": wind_store::read::resolve_frame(&runtime.cache_screenshot_dir(), &row).map(|p| p.display().to_string()),
        "video_path": wind_store::read::resolve_video(&runtime.videos_dir(), &row.videofile_name).map(|p| p.display().to_string()),
        "thumbnail": match (mime, &bytes) {
            (Some(mime), Some(bytes)) => json!({
                "resource": thumbnail_uri(row.time),
                "mime_type": mime,
                "bytes": bytes.len(),
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            }),
            _ => Value::Null,
        },
    }))
}

/// The resource URI a client fetches bytes from, which is also the handle `resources/read` accepts.
/// Keyed by the stored timestamp so it round-trips through `search` and `around`.
pub fn thumbnail_uri(stored: i64) -> String {
    format!("windrecorder://thumbnail/{stored}")
}

pub const THUMBNAIL_TEMPLATE: &str = "windrecorder://thumbnail/{timestamp}";

/// The bytes behind a `windrecorder://thumbnail/{timestamp}` URI, and the row they came from.
pub fn read_thumbnail(runtime: &Runtime, axis: &Axis, uri: &str) -> Result<(Vec<u8>, String, Value), Rejected> {
    let stored = uri
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|tail| tail.parse::<i64>().ok())
        .ok_or_else(|| Rejected(format!("not a thumbnail uri: {uri}; expected {THUMBNAIL_TEMPLATE}")))?;
    // The exact row first: a resource addressed at a moment must not answer with a picture from a
    // different minute, which is what a forgiving window would do.
    let value = frame_at(runtime, axis, stored, 0)
        .or_else(|| frame_at(runtime, axis, stored, 900))
        .ok_or_else(|| Rejected(format!("no stored thumbnail at or near {}", axis.render(stored))))?;
    if value["thumbnail"].is_null() {
        return Err(Rejected("that frame has no stored thumbnail".to_string()));
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(value["thumbnail"]["data"].as_str().unwrap_or_default())
        .map_err(|e| Rejected(format!("stored thumbnail is not valid base64: {e}")))?;
    let mime = value["thumbnail"]["mime_type"].as_str().unwrap_or("application/octet-stream").to_string();
    Ok((data, mime, value))
}

/// Windrecorder writes JPEG thumbnails for recorded screens and a PNG row for its welcome screen, so
/// the label has to come from the bytes. Anything else is unexpected data, not a guess: an agent
/// handed a mislabelled image renders garbage and reports it as a corrupt database.
pub fn sniff_image_format(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// One `video_text` row as JSON, with recognized text clipped to `max_chars` (0 meaning whole).
fn row_view(row: &Row, axis: &Axis, max_chars: usize) -> Value {
    let body = row.body();
    let characters: Vec<char> = body.chars().collect();
    let clipped = if max_chars > 0 && characters.len() > max_chars {
        format!("{}\u{2026}", characters[..max_chars].iter().collect::<String>())
    } else {
        body.to_string()
    };
    let truncated = clipped.chars().count() != characters.len();
    let mut view = serde_json::Map::new();
    view.insert("timestamp".into(), json!(row.time));
    view.insert("time".into(), json!(axis.render(row.time)));
    view.insert("video_file".into(), json!(row.videofile_name));
    view.insert("text".into(), json!(clipped));
    // Absent rather than null, and twice for the same reason: a page of twenty frames is twenty rows,
    // so a `"url": null` on each is a line item the agent pays for and cannot use, and `text_chars`
    // only means something once something has been dropped. `window_title` keeps its explicit null,
    // because "this frame had no foreground window" is the answer and its absence would read as a bug.
    if truncated {
        view.insert("text_chars".into(), json!(characters.len()));
    }
    view.insert("window_title".into(), row.title().and_then(title::normalize).map_or(Value::Null, |clean| json!(clean)));
    if let Some(url) = row.deep_linking.as_deref().map(str::trim).filter(|url| !url.is_empty()) {
        view.insert("url".into(), json!(url));
    }
    view.insert("offset_in_segment".into(), row.offset_in_segment().map_or(Value::Null, |offset| json!(offset)));
    Value::Object(view)
}

/// One merged run of the day, as the coarse rung reports it.
///
/// No screen text by design, and the URL only when there is one: this is the call an agent makes to
/// read a whole day cheaply, and it is the text and the empty fields that would make it expensive.
fn event_json(axis: &Axis, run: &stream::Run) -> Value {
    let mut event = json!({
        // The hand-off point: a valid `timestamp` argument to `windrecorder_around`, which is how an
        // agent gets from a cheap day to the screen contents without guessing at a moment.
        "timestamp": run.from,
        "from": axis.render(run.from),
        "seconds": run.seconds,
        "frames": run.frames,
        "window_title": run.title,
    });
    if let Some(url) = &run.url {
        event["url"] = json!(url);
    }
    event
}

fn share(seconds: i64, counted: i64) -> f64 {
    if counted > 0 {
        ((seconds as f64 / counted as f64) * 10_000.0).round() / 10_000.0
    } else {
        0.0
    }
}

fn round_ms(ms: f64) -> f64 {
    (ms * 100.0).round() / 100.0
}

pub(crate) fn note_skipped(out: &mut Value, skipped: Vec<String>) {
    if !skipped.is_empty() {
        out["skipped_databases"] = json!(skipped);
    }
}

/// The window a range-taking tool should search, and the rule that produced it.
///
/// Two shapes, and the distinction is the whole point of this function. An explicit `start`/`end` is
/// honoured exactly: the day-begin shift is a convention the app applies to a *whole day*, and a
/// caller that typed bounds means those bounds. A `day` is the opposite case — a bare date standing
/// for a full day of this product's history — and it goes to [`day_window`] and nowhere else, which
/// is where `day_summary`'s own `date` also goes. Both tools that take a day and the tool that only
/// ever takes a day therefore cannot drift, because they do not each hold a copy of the rule.
pub(crate) fn resolve_range(runtime: &Runtime, axis: &Axis, args: &Value) -> Result<Window, Rejected> {
    if let Some(day) = optional_day(args)? {
        let bounded = ["start", "end"].iter().any(|key| args.get(*key).is_some_and(|value| !value.is_null()));
        if bounded {
            return Err(Rejected(
                "`day` names a whole product day and cannot be combined with `start`/`end`; give one or the other".to_string(),
            ));
        }
        return day_window(runtime, axis, &day);
    }
    let (mut from, mut to) = (axis.parse(&time_arg(args, "start", axis)?)?, axis.parse(&time_arg(args, "end", axis)?)?);
    if from > to {
        std::mem::swap(&mut from, &mut to);
    }
    // Widened by one second so a moment written twice still selects itself, exactly as before this
    // function learned about days.
    if from == to {
        to += 1;
    }
    Ok(Window { from, to, rule: Rule::Explicit, day_begin_minutes: runtime.day_begin_minutes() })
}

/// `day`, when one was given. Read through the same coercion as `day_summary`'s `date` — an absent,
/// null or blank value is "not a day", so the caller falls through to `start`/`end` rather than
/// reading an empty string as a date. Two spellings of the same idea must not parse two ways.
pub(crate) fn optional_day(args: &Value) -> Result<Option<String>, Rejected> {
    let given = date_arg(args, "day")?;
    Ok(if given.trim().is_empty() { None } else { Some(given) })
}

/// A calendar date, and specifically not a number.
///
/// `start`, `end` and `timestamp` take a bare integer, on purpose: that is how a stored timestamp
/// comes back and goes straight again, and [`Axis::usage`] says so. `day` and `date` promise the
/// opposite thing — one whole product day, named by its calendar date — and an agent that sent
/// `day: 20260921` as a number used to be answered about a Monday in 1970, with the wrong window
/// printed in `range` as though it were the right one. Refusing is the rule
/// `a_malformed_day_is_refused_rather_than_widened_to_a_guess` already holds for a mistyped string,
/// applied to the one shape a shell flag could not reach.
fn date_arg(args: &Value, key: &str) -> Result<String, Rejected> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => Err(Rejected(format!(
            "{key} must be a calendar date such as '2026-09-21', not {other}; a bare number is a stored \
             timestamp, and a timestamp belongs on `timestamp`."
        ))),
    }
}

/// [`date_arg`] where the tool cannot answer without it. The wording of the refusal stays the one
/// every required time argument gives, so a client that read one message has read all of them.
fn date_arg_required(args: &Value, key: &str, axis: &Axis) -> Result<String, Rejected> {
    let value = date_arg(args, key)?;
    if value.trim().is_empty() {
        return Err(Rejected(format!("{key} is required. {}", axis.usage())));
    }
    Ok(value)
}

pub(crate) fn text_arg(args: &Value, key: &str) -> Result<String, Rejected> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(other) => Err(Rejected(format!("{key} must be a string, got {other}"))),
    }
}

/// A time given as either an ISO string or a passed-back integer.
fn time_arg(args: &Value, key: &str, axis: &Axis) -> Result<String, Rejected> {
    match args.get(key) {
        None | Some(Value::Null) => Err(Rejected(format!("{key} is required. {}", axis.usage()))),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(other) => Err(Rejected(format!("{key} must be a datetime string or a stored timestamp, got {other}. {}", axis.usage()))),
    }
}

pub(crate) fn limit_arg(args: &Value, default: usize) -> Result<usize, Rejected> {
    Ok(optional_usize(args, "limit", 1, MAX_LIMIT)?.unwrap_or(default))
}

pub(crate) fn optional_usize(args: &Value, key: &str, low: usize, high: usize) -> Result<Option<usize>, Rejected> {
    let given = match args.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.as_i64().and_then(|v| usize::try_from(v).ok()),
        Some(other) => return Err(Rejected(format!("{key} must be a whole number, got {other}"))),
    };
    match given {
        Some(v) if (low..=high).contains(&v) => Ok(Some(v)),
        Some(v) => Err(Rejected(format!("{key} must be between {low} and {high}, got {v}"))),
        None => Err(Rejected(format!("{key} must be a whole number between {low} and {high}"))),
    }
}

/// Out-of-range bounds are clamped rather than refused: `window_seconds` is a dial, and an agent
/// that asks for more than the bridge will scan gets the widest honest answer instead of a retry.
pub(crate) fn bounded_i64(args: &Value, key: &str, default: i64, low: i64, high: i64) -> Result<i64, Rejected> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n.as_i64().map(|v| v.clamp(low, high)).ok_or_else(|| Rejected(format!("{key} must be a whole number"))),
        Some(other) => Err(Rejected(format!("{key} must be a number, got {other}"))),
    }
}
