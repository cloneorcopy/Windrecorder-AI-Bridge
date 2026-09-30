//! The MCP side of the wire: JSON-RPC 2.0, the tool schemas, and nothing else.
//!
//! Hand-rolled rather than pulled in as a crate, on purpose. The surface this bridge needs is five
//! methods, and the alternative is `rmcp`/`rmcp-actix` or the transitive half of the async
//! ecosystem into a workspace that today builds offline from a lock nobody has to regenerate. The
//! cost of the deal is written down in `docs` below: no SSE streaming, no server-initiated
//! notifications, no `prompts/*`. The Python bridge already made the first of those choices for the
//! same reason — one JSON response per request, so a LAN-reachable endpoint does not need a proxy
//! that survives chunked streaming.
//!
//! This module is transport-free: it takes a parsed request and returns a parsed response. `http`
//! owns sockets, `server` owns sessions and the bearer gate, and both are testable by handing this
//! one a JSON value.

use base64::Engine as _;
use serde_json::{json, Value};

use crate::axis::Axis;
use crate::runtime::Runtime;
use crate::tools;

/// The protocol revisions this server will answer. A client that asks for anything else is told
/// which one it got, which is the negotiation the specification describes.
pub const SUPPORTED_PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
pub const SERVER_NAME: &str = "windrecorder";

/// JSON-RPC's own codes, plus the two the MCP schema borrows from HTTP semantics.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

pub const INSTRUCTIONS: &str = "\
Windrecorder is a local screen-memory engine: it captures the screen, runs OCR over the frames, \
records the foreground window title and browser URL, and stores them in monthly SQLite files. Every \
tool here reads that store and nothing else. Three of the eleven do not read it at all:
`windrecorder_period_summary_write` and `windrecorder_day_summary_write` write only into the two summary
directories this feature owns under `userdata/`, and `windrecorder_prompts_read` reads the prompt files.
The recorded index is never written by this service, and the recorder keeps running unaffected.

Work coarse to fine, because screen text is expensive and most questions need one frame:
- 'What was I doing on <day>?' -> windrecorder_day_summary: that day as merged dated events, with no \
screen text at all. Each event's `timestamp` is the handle for the next rung.
- An event looks relevant -> windrecorder_around with that timestamp. Text is clipped per frame; pass \
max_text_chars=0 when one exact moment really needs the whole text.
- 'Find where X appeared' -> windrecorder_search, over as narrow a range as you can justify: `day` for \
one product day, `start`/`end` for anything shorter.
- 'How long did I spend in each app?' -> windrecorder_app_usage, on the same `day`.
- A picture is worth more than text -> windrecorder_frame, or fetch its thumbnail resource.
- Check windrecorder_status first, for which dates hold data and how far indexing lags.
- To turn screen history into summaries: `windrecorder_summaries_pending` for a day or a range says which
recorded stretches have no summary that stands, and carries each one's full captured text, so it is the
whole reading step. Write each back with `windrecorder_period_summary_write`; when every stretch of the
day stands, `windrecorder_day_summary_write` files its paragraph — and refuses, naming what is missing,
while any of them is unwritten. `windrecorder_summaries_read` shows what exists and in which state;
`windrecorder_prompts_read` gives the prompt text this machine would have used, so a paragraph written
outside reads like one written inside.

TIMESTAMPS. Every rendered time is ISO-8601 with an explicit numeric UTC offset. Every integer \
`timestamp` is the index's own stored value, which is the local wall clock counted as if it were UTC \
and is therefore POSIX seconds plus an offset - NOT a POSIX timestamp. Hand those integers back to \
this bridge unchanged; do not convert them with a POSIX library or you will be eight hours away from \
the row you meant.

DAYS. A 'day' in this service is Windrecorder's product day, not the calendar day: it begins at the \
install's configurable day start, 03:00 by default, and runs to one second before it the next date, \
so a 1am frame is yesterday's work. `day` on windrecorder_search and windrecorder_app_usage, and \
`date` on windrecorder_day_summary, all resolve to exactly that one window - pass the same date to \
any two of them and their `range` fields are equal, which is how a usage table and a screen search \
can be cross-checked at all. `start`/`end` are honoured literally and carry no shift. Every response \
prints the window it searched and the rule that produced it, in `range`.

Window titles are normalised the same way the recorder's own statistics normalise them, so the same \
window reads identically across every tool. Titles on the user's exclude_words list are withheld \
from the day and usage summaries and counted in `withheld_excluded_titles`.

AI ANSWERS. `windrecorder_day_summary` also carries `ai_tags` and `ai_summary`, and each states its \
own `state` and `granularity`. Tags are generated per MONTH, so a day inside a tagged month is \
answered from the month and labelled `granularity: month`: say so when you use them, because the \
month's theme is not what the user was doing that afternoon. A `not_generated` state means nobody has \
generated that answer yet - it does not mean the day was empty, and it is not the same as \
`generated_empty` (generated, nothing to report), `generation_failed` (the model call failed) or \
`unreadable` (the cache file will not parse). `ai_summary` is that day's own paragraph, written from its stretch \
summaries, and has no month-level form to stand in for it: `not_generated` there means nobody has \
summarised this day, which on an install that has never run the feature is every day of it.

The contents are a record of everything that crossed this user's screen. Treat it as private, quote \
only what the task needs, and do not paste large stretches somewhere external.";

/// The read-only annotation every tool carries, so a host can tell an agent it may call these
/// without asking first.
fn read_only() -> Value {
    json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false })
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn integer(description: &str, low: i64, high: i64) -> Value {
    json!({ "type": "integer", "minimum": low, "maximum": high, "description": description })
}

/// `tools/list`'s payload. Written here, not derived from the Rust signatures, because the schema is
/// what an agent reads *before* it calls, and that text is the tool's interface.
pub fn tool_list(axis: &Axis) -> Value {
    let timestamp = |what: &str| {
        // Declared as both spellings because every payload hands back an *integer* and this text tells
        // the agent to pass it back unchanged: a host that validates an argument against the schema
        // before sending it would otherwise refuse the one value shape this service produced.
        json!({
            "type": ["string", "integer"],
            "description": format!(
                "{what}: the `timestamp` integer from an earlier result, passed back unchanged, or an \
                 ISO-8601 datetime such as '2026-09-20 14:30:00' or '{}'.",
                axis.render(1_790_025_372)
            ),
        })
    };
    json!({ "tools": [
        {
            "name": "windrecorder_status",
            "description": "Report which month files hold data, how many records, and how fresh they are. \
                Read this before concluding that an empty search meant an empty period. The per-file list \
                is under `databases`; `clock` says which axis every timestamp in this service lives on; \
                `ai_caches` says which of the caches `windrecorder_day_summary` draws its `ai_tags` and \
                `ai_summary` from exist, and whether their entries are keyed by month or by day - which is \
                how to tell 'nobody has generated tags yet' from 'they exist at a coarser width'.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_search",
            "description": "Search recognized screen text and foreground window titles within a time range, \
                newest first. Give either `day` (one whole Windrecorder product day, which begins at the \
                install's configurable day start and defaults to 03:00) or both `start` and `end`; a `day` \
                and explicit bounds are mutually exclusive, and an unbounded search is not offered. Each \
                keyword is an exact case-insensitive substring and keywords are ANDed; an inner hyphen is \
                treated as a space, as upstream does, so 'read-me' finds 'read me' but not the literal \
                'read-me'. Visually-similar Chinese character expansion is not applied, so a Chinese query \
                can return fewer hits than the same query in the web UI. Empty keywords simply lists the \
                range. Text is clipped to 400 characters, with `text_chars` giving the real length. Every \
                result carries the `range` actually searched and the `rule` that produced it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "day": string("One whole product day, YYYY-MM-DD: [day 03:00:00, next day 02:59:59] \
                        under the shipped day start. Use this for 'what happened on <date>'."),
                    "start": string("Start of an explicit time range: '2026-09-20' or '2026-09-20 14:30:00'. \
                        Required unless `day` is given; never shifted by the day start."),
                    "end": string("End of an explicit range, same shapes. Required unless `day` is given."),
                    "keywords": string("Space-separated words that must all match. Empty lists the whole range."),
                    "exclude": string("Space-separated words that must not appear."),
                    "limit": integer("Results to return.", 1, tools::MAX_LIMIT as i64),
                    "offset": integer("Skip this many newest matches to page further back.", 0, 1_000_000),
                },
                "anyOf": [{ "required": ["day"] }, { "required": ["start", "end"] }],
                "additionalProperties": false,
            },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_around",
            "description": "Read the frames surrounding one moment, oldest first, with title, URL and \
                recognized text. A full screen of text costs thousands of tokens, so text is clipped per \
                frame by default and `text_chars` gives the real length; set max_text_chars=0 when the whole \
                text of a small window is actually needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timestamp": timestamp("Central moment"),
                    "window_seconds": integer("Seconds of slack on each side of the moment.", 1, tools::MAX_WINDOW_SECONDS),
                    "limit": integer("Frames to return.", 1, tools::MAX_LIMIT as i64),
                    "max_text_chars": integer("Recognized text kept per frame; 0 returns each frame in full.", 0, tools::TEXT_CHARS_LIMIT as i64),
                },
                "required": ["timestamp"],
                "additionalProperties": false,
            },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_app_usage",
            "description": "Time spent under each foreground window title, ranked, with each title's share \
                of counted time. Give either `day` (one whole Windrecorder product day, beginning at the \
                install's configurable day start and defaulting to 03:00) or both `start` and `end`; the \
                same `day` gives the same window this tool's siblings search, so a usage table and a \
                search of one date are comparable. Screen time is credited from each recorded frame to \
                the next, capped at 100 seconds so a locked or overnight period is not one long session, \
                after the same title normalisation the web UI applies. `share_of_counted_time` is the \
                share of the seconds this tool counted, not of the whole clock range, so idle and \
                unrecorded stretches are not in the denominator. Excluded titles are withheld and counted \
                in `withheld_excluded_titles`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "day": string("One whole product day, YYYY-MM-DD: [day 03:00:00, next day 02:59:59] \
                        under the shipped day start. Use this for 'how long did I spend on <date>'."),
                    "start": string("Start of an explicit period: '2026-09-20' or '2026-09-20 14:30:00'. \
                        Required unless `day` is given; never shifted by the day start."),
                    "end": string("End of an explicit period, same shapes. Required unless `day` is given."),
                    "limit": integer("Window titles to return.", 1, tools::MAX_LIMIT as i64),
                },
                "anyOf": [{ "required": ["day"] }, { "required": ["start", "end"] }],
                "additionalProperties": false,
            },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_day_summary",
            "description": "One day as merged dated events, carrying no screen text. Consecutive frames under \
                the same window title become one event with a start, a duration and a `timestamp` you can pass \
                straight to windrecorder_around. Prefer this over searching blind - it is the cheap coarse rung. \
                `total_counted_seconds` covers the whole day rather than only the events shown. Days follow \
                Windrecorder's own configurable day start, which defaults to 03:00, so a 1am frame belongs to \
                the previous day - and that is the same window `windrecorder_search` and `windrecorder_app_usage` \
                answer for when they are given the same `day`. The payload prints it in `range`. \
                AI CONTEXT: `ai_tags` and `ai_summary` are always present, each with a `state` and a \
                `granularity`, and both must be read together with them. Activity tags are generated once per \
                MONTH, so a day inside a tagged month is answered from that month's entry and says \
                `granularity: month` - those are the month's theme, not what this day was doing, and must not be \
                presented as the day's own. `ai_tags.state` is one of `answered`, `generated_empty` (tagged, and \
                there was nothing to say), `generation_failed` (the model call failed; asking again is the fix), \
                `not_generated` (nobody has ever tagged this period) or `unreadable` (the cache file will not \
                parse); only `answered` carries tags. `ai_summary` is that day's own paragraph, written from \
                its stretch summaries, and it has no month equivalent to fall back on: `not_generated` \
                there means nobody has summarised this day yet, which is a different claim from \
                `generated_empty`. Read `partial` and `stale` beside it - a day written over a gap says \
                so, and so does one whose stretches or whose prompt have since changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "date": string("Which day, e.g. '2026-09-03'. A datetime is accepted too and its date part is used."),
                    "limit": integer("Events to return, oldest first.", 1, tools::MAX_LIMIT as i64),
                },
                "required": ["date"],
                "additionalProperties": false,
            },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_frame",
            "description": "The preview image stored alongside the frame nearest a moment, as image content, \
                plus where the full-resolution capture and the video segment are on disk and the offset within \
                it. The image is the preview kept in the index — `thumbnail_generation_size_width` pixels wide, \
                labelled with the format actually \
                stored. The same bytes are addressable as the windrecorder://thumbnail/{timestamp} resource.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timestamp": timestamp("Central moment"),
                    "window_seconds": integer("How far to look for a stored frame.", 1, tools::MAX_WINDOW_SECONDS),
                },
                "required": ["timestamp"],
                "additionalProperties": false,
            },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_summaries_pending",
            "description": "The work queue for AI summaries: which recorded stretches have no summary that \
                stands, and what each one's screen actually says. Give either `day` (one whole Windrecorder \
                product day, which begins at the install's configurable day start and defaults to 03:00) or \
                both `start` and `end`; an unbounded queue is not offered. Every entry carries its segment \
                key, span, frame count, character count, window titles and the FULL recognized text of each \
                frame, so this one call is all the reading needed before \
                `windrecorder_period_summary_write`; set `max_text_chars` only if you want less than the \
                whole text, and `text_chars`/`clipped` per frame tell you what happened. `pending` entries \
                never had a summary; `stale` ones had one whose screen text or whose prompt has since \
                changed, and carry that old paragraph in `previous_text`. `days_pending` names the days \
                whose daily summary no longer stands and carries the stretch paragraphs to rewrite it \
                from. `prompt` is the current editable prompt text and its placeholders, which is how a \
                paragraph written here ends up reading like one this machine writes itself. `counted` is the \
                width of the range and `skipped_databases` means coverage there is a floor, not a total.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "day": string("One whole product day, YYYY-MM-DD. Use this for 'what is left of <date>'."),
                    "start": string("Start of an explicit range: '2026-09-20' or '2026-09-20 14:30:00'. \
                        Required unless `day` is given."),
                    "end": string("End of an explicit range, same shapes. Required unless `day` is given."),
                    "include": string("'pending' (default) lists only the work; 'all' also lists the \
                        stretches that are already done, as `current`."),
                    "max_text_chars": integer("Characters of recognized text to keep per frame. 0, the \
                        default, returns each frame in full.", 0, tools::TEXT_CHARS_LIMIT as i64),
                },
                "anyOf": [{ "required": ["day"] }, { "required": ["start", "end"] }],
                "additionalProperties": false,
            },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_summaries_read",
            "description": "Read back the summaries that exist for a range of days: the per-stretch \
                paragraphs, the per-day paragraph, or both. Give `day` or `start`+`end` as elsewhere, and \
                `kind` of 'period', 'daily' or 'both'. Nothing is clipped: stored text comes back byte for \
                byte with its `written_at`, `written_by`, `model` and both fingerprints. Each family states \
                its own `state`, because 'nobody wrote about this day' (`not_generated`), 'a file is there \
                and holds nothing' (`generated_empty`), 'a file is there and is not readable' (`unreadable`), \
                'it was written over a gap' (`partial`) and 'it was written and its premise has moved' \
                (`stale`) are five different answers; days with no file at all are also listed in \
                `absent_days`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "day": string("One whole product day, YYYY-MM-DD."),
                    "start": string("Start of an explicit range. Required unless `day` is given."),
                    "end": string("End of an explicit range. Required unless `day` is given."),
                    "kind": string("'period', 'daily' or 'both' (the default)."),
                },
                "anyOf": [{ "required": ["day"] }, { "required": ["start", "end"] }],
                "additionalProperties": false,
            },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_prompts_read",
            "description": "The prompt text this machine would send a model right now, all seven templates, \
                each with where it came from ('user' means the file at \
                userdata/ai_prompts/<name>.txt overrides the shipped one at config_src/ai_prompts/<name>.txt), \
                what it would be without the override, and which placeholders it accepts. This is the same \
                text the settings screen edits, so what you read here is what the next request sends — \
                nothing chooses a different prompt behind your back.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": read_only(),
        },
        {
            "name": "windrecorder_period_summary_write",
            "description": "File one summary of one recorded stretch. Name the stretch three ways and this \
                picks one key: its filename ('2026-09-27_15-47-17.mp4'), its start stamp \
                ('2026-09-27_15-47-17'), or a `timestamp` inside it (which needs `day`, so one month file is \
                read rather than every one). A stretch that was never recorded is refused, so no summary can \
                describe a window that does not exist. `text` is stored byte for byte — no length limit, no \
                truncation, no word list applied to it — and the span, frame count and character count come \
                from the index rather than from anything you send, so the numbers beside a paragraph are the \
                numbers the database holds. Writing the same stretch twice replaces its paragraph and says \
                `replaced: true`; the reply ends with how much of that day is now covered, and whether the \
                daily gate has opened.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "segment": string("The stretch: filename, start stamp, or a timestamp inside it."),
                    "day": string("YYYY-MM-DD. Only needed when `segment` is a bare timestamp."),
                    "text": string("The summary. Required; send \"\" for an intentionally empty one."),
                    "written_by": string("Who you are, recorded verbatim and never used to decide \
                        anything — 'windai', 'qoder', the name your users will want to see later."),
                    "model": string("Which model produced it, as you report it. Recorded, not verified."),
                },
                "required": ["segment", "text"],
                "additionalProperties": false,
            },
            "annotations": writes(),
        },
        {
            "name": "windrecorder_day_summary_write",
            "description": "File one day's summary, written from that day's stretch summaries. This is \
                gated, and the gate is the feature: before anything is stored the bridge recounts the \
                day's recorded stretches, and if any of them has no summary that stands, the call is \
                refused and names the missing stretches (first 20, with the total) — so a day cannot be \
                summarised from the half that happened to be looked at. Send `allow_partial: true` to \
                summarise what exists so far; that records `partial: true` together with the gap, and every \
                later read says the day was written over a hole rather than pretending it was whole. Text \
                is stored as sent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "date": string("Which day, YYYY-MM-DD, as the product counts it."),
                    "text": string("The day's summary. Required."),
                    "allow_partial": { "type": "boolean", "description": "Write despite gaps in the day, and \
                        record that it was done that way." },
                    "written_by": string("Who you are."),
                    "model": string("Which model produced it, as you report it."),
                },
                "required": ["date", "text"],
                "additionalProperties": false,
            },
            "annotations": writes(),
        },
    ] })
}

/// The annotations a write tool carries.
///
/// `readOnlyHint: false` is the machine-readable half of "this bridge can now write"; `destructiveHint:
/// false` is true in the specific sense that both writers are idempotent upserts into this feature's own
/// files, and a repeat call replaces one paragraph rather than erasing a history. `openWorldHint` stays
/// false: no tool here reaches a network.
fn writes() -> Value {
    json!({ "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false })
}

/// Refuse an argument the tool never published, and name the one the caller probably meant.
///
/// Every `inputSchema` above carries `"additionalProperties": false`. That is a promise to whoever
/// read it, and until this function nothing kept it: `windrecorder_around` given `window_second` —
/// one letter short of `window_seconds` — answered with the 120-second default window and `isError:
/// false`, so an agent that asked to see ten minutes around a moment was shown two, and only a reader
/// who went looking in `range` would notice the answer was not the question. A mistyped dial is the
/// one mistake this bridge can catch for free, and the one a caller cannot see in the reply.
///
/// It reads the published schema rather than keeping a second list of names, because a copy here
/// would be the next thing to drift from the thing clients are told. It lives on this side of the
/// line for the same reason, and runs from [`tools::call`], which the service and the command line
/// both go through — so `windmcp search --window-second 5` is refused in the same words as the
/// JSON-RPC call, and the two surfaces cannot disagree about what an argument is.
pub fn unexpected_arguments(axis: &Axis, name: &str, arguments: &Value) -> Result<(), String> {
    let Some(given) = arguments.as_object() else { return Ok(()) };
    if given.is_empty() {
        return Ok(());
    }
    let published = tool_list(axis);
    let tools = published["tools"].as_array().cloned().unwrap_or_default();
    let Some(tool) = tools.iter().find(|tool| tool["name"] == json!(name)) else {
        // An unknown tool is the dispatcher's message to give, because only it knows the list.
        return Ok(());
    };
    let known: Vec<String> = tool["inputSchema"]["properties"]
        .as_object()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    let unexpected: Vec<&String> = given.keys().filter(|key| !known.iter().any(|name| name == *key)).collect();
    if unexpected.is_empty() {
        return Ok(());
    }
    let carried = unexpected.iter().map(|key| format!("`{key}`")).collect::<Vec<_>>().join(" and ");
    if known.is_empty() {
        return Err(format!("{name} takes no arguments; this call carried {carried}."));
    }
    let mut message = format!(
        "{name} does not take {carried}. Its arguments are {}.",
        known.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    if let Some(some) = unexpected.iter().find_map(|key| nearest(&known, key)) {
        message.push_str(&format!(" Did you mean `{some}`?"));
    }
    Err(message)
}

/// The closest published name to a word a caller misspelled, within two edits.
///
/// Two is the whole of it. Past that a suggestion is a guess, and a guess in an error message is a
/// new bug wearing the old bug's clothes; a caller that is further away than two letters wants the
/// list, which the same message already carries.
fn nearest<'a>(known: &'a [String], given: &str) -> Option<&'a String> {
    let mut best: Option<(usize, &String)> = None;
    for candidate in known {
        let distance = edits(candidate.as_str(), given);
        let closer = match best {
            None => true,
            Some((closest, _)) => distance < closest,
        };
        if distance <= 2 && closer {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// Levenshtein distance, on two rows, because the longest word here is `max_text_chars`.
fn edits(a: &str, b: &str) -> usize {
    let target: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=target.len()).collect();
    let mut current = vec![0usize; target.len() + 1];
    for (row, source) in a.chars().enumerate() {
        current[0] = row + 1;
        for (column, want) in target.iter().enumerate() {
            let same = usize::from(source == *want);
            current[column + 1] = (previous[column] + 1 - same).min(current[column] + 1).min(previous[column + 1] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[target.len()]
}

/// One JSON-RPC exchange. `None` means the request was a notification and gets no reply at all,
/// which is what the transport must translate into a 202 with an empty body.
pub fn handle(runtime: &Runtime, axis: &Axis, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let responds = request.get("id").is_some();
    let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));

    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return responds.then(|| failure(id, INVALID_REQUEST, "jsonrpc must be the string \"2.0\""));
    }
    let result = match method {
        "initialize" => succeed(initialize(&params)),
        "notifications/initialized" | "notifications/cancelled" => {
            if responds {
                return Some(failure(id, INVALID_REQUEST, "notifications must not carry an id, so there is no reply to send"));
            }
            return None;
        }
        "ping" => succeed(json!({})),
        "tools/list" => succeed(tool_list(axis)),
        "tools/call" => call_tool(runtime, axis, &params),
        "resources/list" => succeed(json!({ "resources": [] })),
        "resources/templates/list" => succeed(resource_templates()),
        "resources/read" => read_resource(runtime, axis, &params),
        _ => Err((METHOD_NOT_FOUND, format!("unknown method {method:?}"))),
    };
    // A notification never gets a response, including an error one: that is how a client that
    // announces itself to a server it does not speak to fails silently instead of noisily, and the
    // specification asks for the silence.
    if !responds {
        return None;
    }
    match result {
        Ok(value) => Some(success(id, value)),
        Err((code, message)) => Some(failure(id, code, &message)),
    }
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
    let agreed = if SUPPORTED_PROTOCOLS.contains(&asked) { asked } else { SUPPORTED_PROTOCOLS[0] };
    json!({
        "protocolVersion": agreed,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
        },
        "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION"), "title": "Windrecorder screen memory" },
        "instructions": INSTRUCTIONS,
    })
}

fn resource_templates() -> Value {
    json!({ "resourceTemplates": [{
        "uriTemplate": tools::THUMBNAIL_TEMPLATE,
        "name": "Frame thumbnail",
        "title": "The stored preview image for one frame",
        "description": format!(
            "The base64 JPEG or PNG thumbnail the index keeps for the frame stored at {{timestamp}}, \
             which is the `timestamp` field of a search hit, a day event or a frame result. {}",
            tools::thumbnail_uri(1_790_025_372)
        ),
        "mimeType": "image/jpeg",
    } ] })
}

fn call_tool(runtime: &Runtime, axis: &Axis, params: &Value) -> Result<Value, (i32, String)> {
    let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "tools/call needs a `name`".to_string()))?;
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err((INVALID_PARAMS, "tools/call `arguments` must be an object".to_string()));
    }
    match tools::call(runtime, axis, name, &arguments) {
        Ok(value) => Ok(presentation(name, value)),
        // A rejected argument is a *tool* error, not a protocol error: the agent needs to see the
        // message as tool output so it corrects the input, and a JSON-RPC error would be swallowed
        // by clients that treat transport failures as retry-the-same-thing.
        Err(rejected) => Ok(json!({
            "content": [{ "type": "text", "text": rejected.to_string() }],
            "isError": true,
        })),
    }
}

/// Turn a handler's JSON value into a `tools/call` result.
///
/// `windrecorder_frame` carries image bytes, and MCP wants those as an `image` content block rather
/// than as a base64 field of a JSON object. The handler still returns one pure value; this function
/// only moves the bytes to where a client will render them and leaves the metadata behind, so the
/// response never pays for the same picture twice.
fn presentation(name: &str, mut value: Value) -> Value {
    let lifted = if name == "windrecorder_frame" {
        match value.pointer_mut("/thumbnail") {
            Some(serde_json::Value::Object(_)) => {
                let thumbnail = std::mem::replace(&mut value["thumbnail"], Value::Null);
                let data = thumbnail["data"].as_str().unwrap_or_default().to_string();
                let mime = thumbnail["mime_type"].as_str().unwrap_or_default().to_string();
                value["thumbnail"] = json!({
                    "resource": thumbnail["resource"],
                    "mime_type": thumbnail["mime_type"],
                    "bytes": thumbnail["bytes"],
                });
                Some((data, mime))
            }
            _ => None,
        }
    } else {
        None
    };
    let mut content = vec![json!({ "type": "text", "text": summarise(name, &value) })];
    if let Some((data, mime)) = lifted {
        content.push(json!({ "type": "image", "data": data, "mimeType": mime }));
    }
    json!({ "content": content, "structuredContent": value, "isError": false })
}

/// The one-line text a client shows alongside the structured payload.
///
/// Every MCP client renders `content`; only some surface `structuredContent`. An agent that reads the
/// text and never gets to the JSON must still not be lost, so the summary names what came back and
/// where the next rung is — and, for the tools that answered a period, which rule turned the caller's
/// `day` into that period, because "the 22nd" means a product day here and an agent that guesses will
/// cross-check it against the wrong three hours.
fn summarise(name: &str, value: &Value) -> String {
    let rule = |value: &Value| match (value["range"]["rule"].as_str(), value["range"]["day_begin"].as_str()) {
        (Some(rule), Some(begin)) => format!(", {rule} day beginning {begin}"),
        (Some(rule), _) => format!(", {rule}"),
        _ => String::new(),
    };
    match name {
        "windrecorder_status" => format!(
            "{} rows across {} database(s); last record {}.",
            value["total_rows"].as_i64().unwrap_or(0),
            value["databases"].as_array().map_or(0, Vec::len),
            value["last_record"].as_str().unwrap_or("none")
        ),
        "windrecorder_search" => format!(
            "{} of {} match(es) in {} - {}{}; pass a result's `timestamp` to windrecorder_around.",
            value["returned"].as_i64().unwrap_or(0),
            value["total_matches"].as_i64().unwrap_or(0),
            value["range"]["start"].as_str().unwrap_or("?"),
            value["range"]["end"].as_str().unwrap_or("?"),
            rule(value),
        ),
        "windrecorder_around" => format!(
            "{} frame(s) around {}.",
            value["frames"].as_array().map_or(0, Vec::len),
            value["center"].as_str().unwrap_or("?")
        ),
        "windrecorder_app_usage" => format!(
            "{} second(s) counted across {} title(s) in {} - {}{}.",
            value["total_counted_seconds"].as_i64().unwrap_or(0),
            value["usage"].as_array().map_or(0, Vec::len),
            value["range"]["start"].as_str().unwrap_or("?"),
            value["range"]["end"].as_str().unwrap_or("?"),
            rule(value),
        ),
        "windrecorder_day_summary" => format!(
            "{} on {} across {} event(s) of {}.",
            value["total_counted_seconds"].as_i64().unwrap_or(0),
            value["date"].as_str().unwrap_or("?"),
            value["events"].as_array().map_or(0, Vec::len),
            value["total_events"].as_i64().unwrap_or(0),
        ),
        "windrecorder_frame" => format!(
            "frame at {} ({}s from the moment); thumbnail resource {}.",
            value["time"].as_str().unwrap_or("?"),
            value["distance_seconds"].as_i64().unwrap_or(0),
            value["thumbnail"]["resource"].as_str().unwrap_or("none"),
        ),
        _ => format!("{name} returned a result."),
    }
}

fn read_resource(runtime: &Runtime, axis: &Axis, params: &Value) -> Result<Value, (i32, String)> {
    let uri = params.get("uri").and_then(Value::as_str).ok_or((INVALID_PARAMS, "resources/read needs a `uri`".to_string()))?;
    let (data, mime, _) = tools::read_thumbnail(runtime, axis, uri)
        // A missing resource is the code the specification reserves for exactly that.
        .map_err(|rejected| (-32002, rejected.to_string()))?;
    Ok(json!({ "contents": [{
        "uri": uri,
        "mimeType": mime,
        "blob": base64::engine::general_purpose::STANDARD.encode(&data),
    } ] }))
}

/// A method that answered, in the shape the dispatcher's `match` expects.
fn succeed(result: Value) -> Result<Value, (i32, String)> {
    Ok(result)
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn failure(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// An error the transport produces before a handler ever ran, named apart from the ones above
/// because the code is chosen by the framing layer and there may be no id to answer.
pub fn failure_notification(id: &Value, code: i32, message: &str) -> Value {
    failure(id.clone(), code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_gets_no_reply_and_a_request_always_does() {
        assert!(handle_without_runtime(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        // ...but a client that sends one anyway with an id must not be left waiting for a reply.
        let with_id = handle_without_runtime(&json!({"jsonrpc":"2.0","id":1,"method":"notifications/initialized"})).unwrap();
        assert_eq!(with_id["error"]["code"], json!(INVALID_REQUEST));
    }

    fn handle_without_runtime(request: &Value) -> Option<Value> {
        // `notifications/*` never touches the install, so a stub runtime cannot be reached.
        let runtime = crate::runtime::tests_stub();
        let axis = Axis { utc_offset_seconds: 28_800 };
        handle(&runtime, &axis, request)
    }

    /// The published surface and the dispatcher must agree, and the read-only promise must be exactly as
    /// wide as it is true. Nine of these tools only read; two write, into the summary directories this
    /// feature owns, and they publish `readOnlyHint: false` so a host that gates writes sees them without
    /// reading a description. Nothing in the list is destructive: both writers are idempotent upserts, and
    /// neither can reach the index.
    #[test]
    fn every_listed_tool_is_dispatchable_and_states_its_own_write_posture() {
        let axis = Axis { utc_offset_seconds: 28_800 };
        let listed = tool_list(&axis)["tools"].as_array().unwrap().clone();
        assert_eq!(listed.len(), tools::NAMES.len(), "one schema per tool");
        let writers = ["windrecorder_period_summary_write", "windrecorder_day_summary_write"];
        for tool in &listed {
            assert!(tool["inputSchema"]["type"] == "object");
            let name = tool["name"].as_str().unwrap();
            assert_eq!(tool["annotations"]["readOnlyHint"], json!(!writers.contains(&name)), "{name}");
            assert_eq!(tool["annotations"]["destructiveHint"], json!(false), "{name}");
            assert!(tools::NAMES.contains(&name), "{name} is published but not dispatched");
        }
        assert_eq!(
            listed.iter().filter(|tool| tool["annotations"]["readOnlyHint"] == json!(false)).count(),
            2,
            "the write surface is exactly these two tools wide"
        );
        // The two tools an agent must bound explicitly are the two that can scan a year. A `day` is a
        // bound too — what must stay impossible is a call that names no period at all.
        for name in ["windrecorder_search", "windrecorder_app_usage"] {
            let tool = listed.iter().find(|t| t["name"] == name).unwrap();
            assert_eq!(
                tool["inputSchema"]["anyOf"],
                json!([{ "required": ["day"] }, { "required": ["start", "end"] }]),
                "{name} must demand either a day or both bounds"
            );
            assert!(tool["inputSchema"].get("required").is_none(), "{name} gained an unconditional required set");
        }
        assert!(listed.iter().any(|t| t["name"] == "windrecorder_day_summary" && t["inputSchema"]["required"] == json!(["date"])));
    }

    /// A `timestamp` argument has to advertise both spellings, because the value the tools tell an
    /// agent to hand back is a JSON *number*.
    ///
    /// `tools::time_arg` accepts either, so the server never rejected an integer; what the old
    /// `{"type":"string"}` did was let a host that validates arguments against the schema refuse the
    /// one call shape the description recommends ("passed back unchanged"), which looks to the user like
    /// a bridge that cannot follow up on its own search result.
    #[test]
    fn a_timestamp_argument_accepts_the_integer_the_payload_returns() {
        let axis = Axis { utc_offset_seconds: 28_800 };
        let listed = tool_list(&axis)["tools"].as_array().unwrap().clone();
        let mut named = 0;
        for tool in &listed {
            let property = &tool["inputSchema"]["properties"]["timestamp"];
            if property.is_null() {
                continue;
            }
            named += 1;
            let types = property["type"].as_array().unwrap_or_else(|| panic!("{} declares timestamp as a bare type", tool["name"]));
            assert_eq!(types, &vec![json!("string"), json!("integer")], "{}", tool["name"]);
        }
        assert_eq!(named, 2, "`around` and `frame` name a central moment, and nothing else does");
    }

    /// The granularity contract has to be in the text an agent reads *before* it calls.
    ///
    /// A payload that states its own width is only honest if somebody knows to look at the field that
    /// states it, and the schema description is the one place every MCP host puts in front of the
    /// model ahead of the first call. If this assertion ever fails, the fix is not to delete it: a
    /// `windrecorder_day_summary` whose description stopped mentioning `granularity` is a bridge that
    /// is quietly handing month-wide answers out as day-wide ones again.
    #[test]
    fn the_day_summary_description_tells_the_agent_about_granularity_before_it_calls() {
        let axis = Axis { utc_offset_seconds: 28_800 };
        let listed = tool_list(&axis)["tools"].as_array().unwrap().clone();
        let day = listed.iter().find(|t| t["name"] == "windrecorder_day_summary").unwrap();
        let described = day["description"].as_str().unwrap();
        for promise in ["ai_tags", "ai_summary", "granularity", "state", "not_generated", "MONTH"] {
            assert!(described.contains(promise), "the description no longer names {promise}: {described}");
        }
        let status = listed.iter().find(|t| t["name"] == "windrecorder_status").unwrap();
        assert!(status["description"].as_str().unwrap().contains("ai_caches"), "status stopped advertising the cache report");
        // And the same promises in the instructions every client receives at handshake.
        for promise in ["ai_tags", "granularity", "generated_empty", "unreadable"] {
            assert!(INSTRUCTIONS.contains(promise), "the instructions no longer name {promise}");
        }
    }

    #[test]
    fn protocol_negotiation_answers_an_unknown_version_with_its_own() {
        let runtime = crate::runtime::tests_stub();
        let axis = Axis { utc_offset_seconds: 28_800 };
        for asked in ["2025-06-18", "9999-01-01"] {
            let response = handle(&runtime, &axis, &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":asked,"capabilities":{},"clientInfo":{"name":"t","version":"0"}}})).unwrap();
            let agreed = response["result"]["protocolVersion"].as_str().unwrap();
            assert!(SUPPORTED_PROTOCOLS.contains(&agreed), "{asked} -> {agreed}");
        }
        let result = handle(&runtime, &axis, &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})).unwrap()["result"].clone();
        assert_eq!(result["serverInfo"]["name"], json!(SERVER_NAME));
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"]["resources"].is_object());
        assert!(result["instructions"].as_str().unwrap().contains("windrecorder_day_summary"));
    }

    #[test]
    fn a_malformed_envelope_is_rejected_before_the_method_is_considered() {
        let response = handle_without_runtime(&json!({"jsonrpc":"1.0","id":1,"method":"ping"})).unwrap();
        assert_eq!(response["error"]["code"], json!(INVALID_REQUEST));
        let response = handle_without_runtime(&json!({"id":1,"method":"ping"})).unwrap();
        assert_eq!(response["error"]["code"], json!(INVALID_REQUEST), "a missing version is the same fault");
        let response = handle_without_runtime(&json!({"jsonrpc":"2.0","id":2,"method":"tools/leak"})).unwrap();
        assert_eq!(response["error"]["code"], json!(METHOD_NOT_FOUND));
    }

    #[test]
    fn an_unknown_tool_is_a_tool_error_not_a_protocol_error() {
        let runtime = crate::runtime::tests_stub();
        let axis = Axis { utc_offset_seconds: 28_800 };
        let response = handle(&runtime, &axis, &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"windrecorder_write_something","arguments":{}}})).unwrap();
        assert_eq!(response["result"]["isError"], json!(true));
        assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("unknown tool"));
        assert!(response.get("error").is_none(), "the exchange itself succeeded");
    }

    #[test]
    fn a_resource_template_names_the_uri_a_frame_result_returns() {
        let templates = resource_templates()["resourceTemplates"].as_array().unwrap().clone();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0]["uriTemplate"], json!(tools::THUMBNAIL_TEMPLATE));
        assert!(templates[0]["description"].as_str().unwrap().contains("windrecorder://thumbnail/1790025372"));
    }

    #[test]
    fn a_rejected_argument_still_completes_the_jsonrpc_exchange() {
        let runtime = crate::runtime::tests_stub();
        let axis = Axis { utc_offset_seconds: 28_800 };
        let response = handle(&runtime, &axis, &json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"windrecorder_search","arguments":{"start":"yesterday","end":"today","keywords":"x"}}})).unwrap();
        assert!(response.get("error").is_none(), "an agent's typo must not look like a broken server");
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("2026-09-20"), "{text}");
    }

}
