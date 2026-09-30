//! The summary tools: what still needs summarising, what has been written, and how it comes back.
//!
//! # The order these tools are meant to be used in
//!
//! `windrecorder_summaries_pending` first. It is the only one that answers "what is left", and it is the
//! only one that can: an agent that cannot see the work queue cannot decide to do the work, because it
//! would have to list a day's segments, group them by `video_file`, guess which it had already covered,
//! and re-derive a rule the bridge owns. So the queue carries each stretch, its span, and **its full
//! frame text** — one call from reading to writing.
//!
//! Then `windrecorder_period_summary_write` per stretch, and `windrecorder_day_summary_write` once every
//! stretch of the day stands. `windrecorder_summaries_read` looks at what exists;
//! `windrecorder_prompts_read` hands out the same prompt words this machine's own generator would use, so
//! a paragraph written by an outside AI and one written by `windai` are the same shape of thing.
//!
//! # What this module writes, and what it never touches
//!
//! Two directories under `userdata/` that this feature owns —
//! `result_ai_period_summary/{product-day}.json` and `result_ai_daily_summary/{product-day}.json` — and
//! the prompt overrides. The index is read through `wind-summary`, which has no database dependency at
//! all, so a summary call cannot write a row; no tool here creates, deletes or rewrites a month file.
//!
//! # Nothing is rationed, and that is a stated property
//!
//! Frame text comes back whole and a written summary is stored byte for byte. `max_text_chars` exists on
//! the queue as an *opt-in* narrowing for a caller that wants it, and its default is 0, meaning "the
//! whole thing". There is no length limit on a write and no word list applied to one. The size that *is*
//! reported is `ocr_chars`, so a caller can see what a request will cost without being capped into it.
//!
//! # Every "empty" answer says which kind of empty it is
//!
//! The rule the AI caches taught this binary: a `state` of `not_generated` (nobody wrote about this day),
//! `generated_empty` (something was written and the file holds nothing), `unreadable` (a file is there and
//! is not a summary file) and `stale` (it was written and its premise has since moved) are four different
//! facts, and an agent offered one word for all four will retry, widen its range, or conclude the bridge
//! is broken.

use serde_json::{json, Value};
use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::prompts::{self, Prompts};
use wind_summary::{self as summary, Kind, PromptDigests, Reader as IndexReader};

use crate::axis::Axis;
use crate::runtime::Runtime;
use crate::tools::{day_window, note_skipped, optional_day, optional_usize, resolve_range, text_arg, Rejected, Window, TEXT_CHARS_LIMIT};

type Called = Result<Value, Rejected>;

/// How many missing segment keys a refusal names before it says how many more there are.
pub const MISSING_LISTED: usize = 20;

/// The prompt digests in force, so "written under which words" is comparable across producers.
fn digests(config: &Config) -> PromptDigests {
    let resolved = Prompts::read(config);
    PromptDigests::of(&resolved.period_system, &resolved.period_user, &resolved.daily_system, &resolved.daily_user)
}

/// The product days a window touches, oldest first.
///
/// Both ends go through `wind-summary`'s own day arithmetic — the same function that decided which file
/// a stretch's summary is stored in — so a range starting at 02:00 asks about *yesterday*, once, and not
/// about both days.
fn days_covered(config: &Config, window: &Window) -> Vec<String> {
    let shift = config.day_begin_minutes();
    let from = summary::day_of(window.from, shift);
    let to = summary::day_of(window.to, shift);
    summary::days_between(&from, &to)
}

/// The install's own agreement about how much one summarising run may take, as the queue reports it.
///
/// Three reads of [`wind_base::config`], and no numbers of this module's own: the same three accessors
/// are what `windmaint` builds its `windai summarize` command line from and what the Recording and AI
/// pages write. That is the whole reason the ceiling is reported rather than left inside the pass — an
/// agent that cannot see the budget has to guess one, and a guess about how much work to do on somebody
/// else's API bill is the thing this binary refuses to make for them.
fn one_run_may_take(config: &Config) -> Value {
    json!({
        "days": config.summary_pending_days_in_idle(),
        "stretches": config.summary_stretch_limit_in_idle(),
        "switched_on": config.ai_summary_allowed_in_idle(),
        "note": "What `windmaint` asks of `windai summarize` on an idle run, from the Recording page's two \
                 budget rows and the AI page's switch. This queue is not capped to it: the ceiling is the \
                 install's own agreement about how long a pass may take, and it is reported so a caller \
                 doing the same work voluntarily can match it.",
    })
}

/// `windrecorder_summaries_pending` — the work queue, carrying the material to do the work on.
pub fn pending(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let window = resolve_range(runtime, axis, args)?;
    let include = text_arg(args, "include")?;
    if !matches!(include.as_str(), "" | "all" | "pending") {
        return Err(Rejected(format!("include must be \"pending\" or \"all\", got {include:?}")));
    }
    let want_current = include == "all";
    let clip = optional_usize(args, "max_text_chars", 0, TEXT_CHARS_LIMIT)?.unwrap_or(0);
    let config = runtime.config();
    let resolved = Prompts::read(config);
    let digests = PromptDigests::of(&resolved.period_system, &resolved.period_user, &resolved.daily_system, &resolved.daily_user);
    let reader = IndexReader::new(config);
    let days = days_covered(config, &window);

    let mut pending_items: Vec<Value> = Vec::new();
    let mut stale_items: Vec<Value> = Vec::new();
    let mut current_items: Vec<Value> = Vec::new();
    let mut days_pending: Vec<Value> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut totals = (0usize, 0usize, 0usize, 0usize);
    for day in &days {
        let queue = summary::for_day_with(&reader, day, &digests).map_err(|e| Rejected(e.to_string()))?;
        let periods = summary::read_period(config, day);
        skipped.extend(queue.skipped.iter().cloned());
        totals.0 += queue.segments_total;
        totals.1 += queue.summarised;
        totals.2 += queue.unattributed;
        for item in &queue.pending {
            totals.3 += 1;
            let view = json!({
                "segment": item.segment.key,
                "video_file": item.segment.video_file,
                "day": item.segment.day,
                "start": item.segment.start,
                "end": item.segment.end,
                "when": format!("{} → {}", axis.render(item.segment.start), axis.render(item.segment.end)),
                "duration_seconds": item.segment.duration_seconds(),
                "frames": item.segment.frames,
                "ocr_chars": item.segment.ocr_chars,
                "titles": item.segment.titles,
                "reason": item.reason.label(),
                "note": item.reason.note(),
                "frames_detail": frames(&item.segment, clip),
                "previous_text": item.previous.as_ref().map(|entry| entry.text.clone()),
            });
            if item.reason == summary::Reason::Missing {
                pending_items.push(view);
            } else {
                stale_items.push(view);
            }
        }
        if want_current {
            for segment in &queue.current {
                current_items.push(json!({
                    "segment": segment.key,
                    "day": segment.day,
                    "start": segment.start,
                    "end": segment.end,
                    "frames": segment.frames,
                    "ocr_chars": segment.ocr_chars,
                    "written_at": periods.get(&segment.key).map(|entry| entry.written_at.clone()),
                    "written_by": periods.get(&segment.key).map(|entry| entry.written_by.clone()),
                }));
            }
        }
        if let Some((reasons, previous)) = day_needs_redo(&queue) {
            days_pending.push(json!({
                "date": day,
                "reasons": reasons,
                "coverage": {
                    "segments_total": queue.segments_total,
                    "summarised": queue.summarised,
                    "missing": queue.coverage.missing,
                },
                "previous_text": previous,
                // Every paragraph that exists for the day, not only the ones still standing: a producer
                // rewriting a day works from what was written, and an entry whose stretch has since
                // changed is still the best starting point for saying what the day was.
                "period_summaries": periods.entries.iter().map(|(key, written)| json!({
                    "segment": key,
                    "start": written.start,
                    "end": written.end,
                    "when": format!("{} → {}", axis.render(written.start), axis.render(written.end)),
                    "duration_seconds": (written.end - written.start).max(0),
                    "text": written.text,
                    "stale": !queue.current.iter().any(|standing| &standing.key == key),
                })).collect::<Vec<Value>>(),
            }));
        }
    }

    let mut out = json!({
        "range": window.json(axis),
        "counted": {
            "days": days.len(),
            "segments_total": totals.0,
            "summarised": totals.1,
            "needing_work": totals.3,
            "unattributed_rows": totals.2,
        },
        // The budget this machine's own idle pass works to, read from the same two settings the pass
        // spawns `windai` with. It is here because the queue is not rationed and an agent that simply
        // worked through all of `pending` would take on more in one sitting than the user agreed to
        // while they were away; a caller doing the same job by hand should size it the same way.
        // `windmaint`'s `schedule::summary_budget` and this field have one source, which is the point of
        // putting the pair in `wind_base::config` rather than leaving them as constants in the pass.
        "one_run_may_take": one_run_may_take(config),
        "pending": pending_items,
        "stale": stale_items,
        "days_pending": days_pending,
        "prompt": {
            "period_summary": { "system": resolved.period_system, "user": resolved.period_user },
            "daily_summary": { "system": resolved.daily_system, "user": resolved.daily_user },
            // The sentence the two templates above are asked to fill in, resolved: an outside producer
            // working from these words answers in the same language this machine's own pass would,
            // instead of guessing one from the prose it happens to see.
            "language": resolved.language,
            "placeholders": {
                "{frames_table}": "every frame of the stretch, in order: its time, window title, URL and full recognized text",
                "{period_summaries}": "one line per summarised stretch of that day: span, duration, paragraph",
                "{language}": resolved.language,
            },
            "note": "Send back a summary produced from these words and it will read like the ones this \
                     machine writes itself. Using your own words is allowed; the summary then records a \
                     different prompt fingerprint, and the queue will offer it back for a redo when the \
                     stored prompt changes.",
        },
        "where_summaries_live": {
            "period": runtime.shown(&summary::dir(config, Kind::Period)),
            "daily": runtime.shown(&summary::dir(config, Kind::Daily)),
        },
        "note": "`pending` has never had a summary; `stale` had one whose screen text or whose prompt has \
                 since changed, and carries that previous paragraph in `previous_text` so you can improve \
                 on it rather than start from nothing. `frames_detail` is the whole captured text of each \
                 stretch unless you clipped it, so this call is all the reading you need before \
                 `windrecorder_period_summary_write`.",
    });
    if want_current {
        out["current"] = json!(current_items);
    }
    note_skipped(&mut out, skipped);
    if totals.2 > 0 {
        out["unattributed_note"] = json!(format!(
            "{unattributed} rows in this range carry a filename this build cannot read as a recording \
             stretch, so they can hold no summary and are not counted above",
            unattributed = totals.2
        ));
    }
    Ok(out)
}

/// Whether a day's own summary needs redoing, with its reasons and whatever text is there now.
fn day_needs_redo(queue: &summary::DayQueue) -> Option<(Vec<String>, Option<String>)> {
    match &queue.daily {
        summary::DailyState::Absent if queue.gate_open() && !queue.current.is_empty() => Some((vec!["not_generated".to_string()], None)),
        summary::DailyState::Stale(entry, reasons) => Some((reasons.iter().map(|reason| reason.label().to_string()).collect(), Some(entry.text.clone()))),
        summary::DailyState::Unreadable(note) => Some((vec!["unreadable".to_string()], Some(note.clone()))),
        _ => None,
    }
}

/// One stretch's frames, in index order, verbatim unless the caller asked for a clip.
fn frames(segment: &summary::Segment, clip: usize) -> Vec<Value> {
    segment
        .detail
        .iter()
        .map(|frame| {
            let characters = frame.text.chars().count();
            let text = if clip > 0 && characters > clip { frame.text.chars().take(clip).collect::<String>() } else { frame.text.clone() };
            json!({
                "timestamp": frame.timestamp,
                "when": LocalParts::from_naive_epoch(frame.timestamp).display(),
                "title": frame.title,
                "url": frame.url,
                "text": text,
                "text_chars": characters,
                "clipped": text.chars().count() != characters,
            })
        })
        .collect()
}

/// `windrecorder_prompts_read` — the words this machine would send, exactly as they now stand.
pub fn prompts(runtime: &Runtime, _axis: &Axis, _args: &Value) -> Called {
    let resolved = prompts::read_all(runtime.config());
    Ok(json!({
        "prompts": resolved.iter().map(|entry| json!({
            "name": entry.name.label(),
            "text": entry.text,
            "origin": entry.origin.label(),
            "path": runtime.shown(&entry.path),
            "overridden": entry.overridden(),
            "shipped": entry.shipped,
            "placeholders": entry.placeholders(),
            "required": entry.name.required(),
        })).collect::<Vec<Value>>(),
        "language": prompts::answer_language(runtime.config()),
        "answer_language_note": "What `{language}` in the summary and tag templates is filled with, from this \
                                 install's `lang` — the interface language the user set. It is the value of a \
                                 slot, so a template whose own words name a language sends those words instead.",
        "edit_them": "Each name is a file: `userdata/ai_prompts/<name>.txt` overrides the shipped copy at \
                      `config_src/ai_prompts/<name>.txt`. Deleting the override restores the default, and \
                      what this tool returns is what the next request sends — the settings screen edits the \
                      same two files, not a copy of them.",
        "validation": "Saved text may use only the placeholders listed for that template, and must keep the \
                       ones that carry the material (`{frames_table}`, `{period_summaries}`, `{table}`). \
                       Nothing else is checked, and neither a prompt nor a summary has a length limit.",
    }))
}

/// `windrecorder_summaries_read` — what has been written, across a range of days.
pub fn read(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let window = resolve_range(runtime, axis, args)?;
    let kind = match text_arg(args, "kind")?.as_str() {
        "" | "both" => "both",
        "period" => "period",
        "daily" => "daily",
        other => return Err(Rejected(format!("kind must be \"period\", \"daily\" or \"both\", got {other:?}"))),
    };
    let config = runtime.config();
    let days = days_covered(config, &window);
    let mut out_days: Vec<Value> = Vec::new();
    let mut absent: Vec<String> = Vec::new();
    let mut counts = (0usize, 0usize);
    for day in &days {
        let periods = summary::read_period(config, day);
        let daily = summary::read_daily(config, day);
        counts.0 += periods.len();
        counts.1 += usize::from(daily.summary.is_some());
        let mut entry = json!({
            "date": day,
            "range": { "from": axis.render(day_window(runtime, axis, day)?.from), "to": axis.render(day_window(runtime, axis, day)?.to) },
        });
        if kind != "daily" {
            entry["period"] = json!({
                "state": period_state(&periods),
                "file": runtime.shown(&periods.path),
                "note": periods.note,
                "entries": periods.entries.iter().map(|(key, written)| json!({
                    "segment": key,
                    "start": written.start,
                    "end": written.end,
                    "when": format!("{} → {}", axis.render(written.start), axis.render(written.end)),
                    "frames": written.frames,
                    "ocr_chars": written.ocr_chars,
                    "text": written.text,
                    "text_chars": written.text.chars().count(),
                    "written_at": written.written_at,
                    "written_by": written.written_by,
                    "model": written.model,
                    "source_fingerprint": written.source_fingerprint,
                    "prompt_fingerprint": written.prompt_fingerprint,
                })).collect::<Vec<Value>>(),
            });
        }
        if kind != "period" {
            entry["daily"] = json!({
                "state": daily_state(&daily),
                "file": runtime.shown(&daily.path),
                "note": daily.note,
                "text": daily.summary.as_ref().map(|written| written.text.clone()),
                "written_at": daily.summary.as_ref().map(|written| written.written_at.clone()),
                "written_by": daily.summary.as_ref().map(|written| written.written_by.clone()),
                "model": daily.summary.as_ref().map(|written| written.model.clone()),
                "partial": daily.summary.as_ref().map(|written| written.partial),
                "stale": daily.summary.as_ref().map(|written| written.stale),
                "coverage": daily.summary.as_ref().map(|written| json!({
                    "segments_total": written.coverage.segments_total,
                    "segments_summarised": written.coverage.segments_summarised,
                    "missing": written.coverage.missing,
                })),
            });
        }
        if periods.absent() && daily.absent() {
            absent.push(day.clone());
            continue;
        }
        out_days.push(entry);
    }
    Ok(json!({
        "range": window.json(axis),
        "kind": kind,
        "days": out_days,
        "absent_days": absent,
        "counts": { "period_summaries": counts.0, "daily_summaries": counts.1 },
        "note": "A day in `absent_days` has had nothing written about it. That is a different claim from a \
                 day whose file exists and holds nothing, and from one whose file cannot be parsed, and \
                 `state` per family is where those three are told apart.",
    }))
}

fn period_state(periods: &summary::DayMap) -> &'static str {
    match (periods.exists, periods.readable, periods.entries.is_empty()) {
        (false, _, _) => "not_generated",
        (true, false, _) => "unreadable",
        (true, true, true) => "generated_empty",
        (true, true, false) => "answered",
    }
}

fn daily_state(daily: &summary::DailyFile) -> &'static str {
    match (daily.exists, daily.readable, daily.summary.as_ref()) {
        (false, _, _) => "not_generated",
        (true, false, _) => "unreadable",
        (true, true, Some(written)) if written.stale => "stale",
        (true, true, Some(written)) if !written.coverage.complete() || written.partial => "partial",
        (true, true, Some(_)) => "answered",
        (true, true, None) => "unreadable",
    }
}

/// `windrecorder_period_summary_write` — file one stretch's paragraph under the day that owns it.
pub fn write_period(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let reference = text_arg(args, "segment")?;
    if reference.trim().is_empty() {
        return Err(Rejected(
            "segment is required: the recording's filename (`2026-09-27_15-47-17.mp4`), its start stamp, \
             or a `timestamp` inside it (which needs `day`, so one month file is read rather than all of \
             them)".to_string(),
        ));
    }
    let text = require_text(args)?;
    let config = runtime.config();
    let reader = IndexReader::new(config);
    let window = match optional_day(args)? {
        Some(day) => day_window(runtime, axis, &day)?,
        None => match summary::canonical_key(&reference) {
            // A stamp names its own start, so the product day — and so the month file — is known.
            Some(key) => {
                let begins = LocalParts::from_stamp(&key).ok_or_else(|| Rejected(format!("`{key}` is not a recording stamp")))?;
                let day = summary::day_of(begins.naive_epoch_seconds(), config.day_begin_minutes());
                day_window(runtime, axis, &day)?
            }
            None => {
                return Err(Rejected(
                    "give `day` alongside a timestamp, so this call reads one month file rather than all \
                     of them".to_string(),
                ))
            }
        },
    };
    let segment = reader.resolve(&reference, window.from, window.to).map_err(|e| Rejected(e.to_string()))?;
    let digests = digests(config);
    let entry = summary::PeriodSummary {
        text: text.clone(),
        start: segment.start,
        end: segment.end,
        frames: segment.frames,
        ocr_chars: segment.ocr_chars,
        written_at: summary::now_stamp(),
        written_by: text_arg(args, "written_by")?,
        model: text_arg(args, "model")?,
        source_fingerprint: segment.fingerprint.clone(),
        prompt_fingerprint: digests.period.clone(),
    };
    let outcome = summary::write_period(config, &segment.day, &segment.key, &entry).map_err(|e| Rejected(e.to_string()))?;
    let queue = summary::for_day_with(&reader, &segment.day, &digests).map_err(|e| Rejected(e.to_string()))?;
    Ok(json!({
        "segment": segment.key,
        "video_file": segment.video_file,
        "day": segment.day,
        "start": segment.start,
        "end": segment.end,
        "when": format!("{} → {}", axis.render(segment.start), axis.render(segment.end)),
        "frames": segment.frames,
        "ocr_chars": segment.ocr_chars,
        "text_chars": text.chars().count(),
        "replaced": outcome.replaced,
        "written_to": runtime.shown(&outcome.path),
        "written_at": entry.written_at,
        "day_coverage": {
            "segments_total": queue.segments_total,
            "summarised": queue.summarised,
            "missing": queue.coverage.missing,
            "complete": queue.gate_open(),
        },
        "note": format!(
            "{} of {} of this day's stretches now have a summary that stands. {}",
            queue.summarised,
            queue.segments_total,
            if queue.gate_open() {
                "The day is complete, so `windrecorder_day_summary_write` will accept a daily summary."
            } else {
                "Ask `windrecorder_summaries_pending` for this day to see which stretches are left."
            }
        ),
    }))
}

/// `windrecorder_day_summary_write` — one day's paragraph, over a day whose stretches all stand.
pub fn write_day(runtime: &Runtime, axis: &Axis, args: &Value) -> Called {
    let given = text_arg(args, "date")?;
    let window = day_window(runtime, axis, &given)?;
    let date = summary::day_of(window.from, runtime.config().day_begin_minutes());
    let text = require_text(args)?;
    let allow_partial = match args.get("allow_partial") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(other) => return Err(Rejected(format!("allow_partial must be true or false, got {other}"))),
    };
    let config = runtime.config();
    let reader = IndexReader::new(config);
    let digests = digests(config);
    let queue = summary::for_day_with(&reader, &date, &digests).map_err(|e| Rejected(e.to_string()))?;
    if !queue.gate_open() && !allow_partial {
        let listed = queue.coverage.missing.iter().take(MISSING_LISTED).cloned().collect::<Vec<_>>().join(", ");
        let rest = queue
            .coverage
            .missing
            .len()
            .checked_sub(MISSING_LISTED)
            .map(|more| format!(", and {more} more"))
            .unwrap_or_default();
        return Err(Rejected(format!(
            "{date} holds {total} recorded stretches and {done} of them have a summary that stands, so a \
             daily summary would be written over an incomplete day. Still unwritten: {listed}{rest}. Ask \
             `windrecorder_summaries_pending` for the material, or send `allow_partial: true` to summarise \
             what exists so far — that records the day as partial and it reads as partial afterwards.",
            total = queue.segments_total,
            done = queue.summarised,
        )));
    }
    let row = summary::DaySummary {
        date: date.clone(),
        text: text.clone(),
        coverage: queue.coverage.clone(),
        partial: !queue.gate_open(),
        written_at: summary::now_stamp(),
        written_by: text_arg(args, "written_by")?,
        model: text_arg(args, "model")?,
        source_fingerprint: summary::daily_inputs_for(config, &queue),
        stale: false,
        prompt_fingerprint: digests.daily.clone(),
    };
    let outcome = summary::write_daily(config, &row).map_err(|e| Rejected(e.to_string()))?;
    Ok(json!({
        "date": row.date,
        "text_chars": text.chars().count(),
        "partial": row.partial,
        "coverage": {
            "segments_total": row.coverage.segments_total,
            "segments_summarised": row.coverage.segments_summarised,
            "missing": row.coverage.missing,
        },
        "replaced": outcome.replaced,
        "written_to": runtime.shown(&outcome.path),
        "written_at": row.written_at,
        "range": window.json(axis),
        "note": if row.partial {
            "This day was written over a gap and says so: `partial` is true and `coverage.missing` names \
             the stretches it was not written from. Reading the day back carries that forward."
        } else {
            "Every recorded stretch of this day had a summary that stands, so this reads as a whole day."
        },
    }))
}

/// The one required body argument of the two writers, taken verbatim.
fn require_text(args: &Value) -> Result<String, Rejected> {
    match args.get("text") {
        Some(Value::String(body)) => Ok(body.clone()),
        Some(other) => Err(Rejected(format!("text must be a string, got {other}"))),
        None => Err(Rejected("text is required; send `\"\"` if an empty paragraph is what you mean".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(body: &str) -> Config {
        let root = crate::fixture::install("idle-budget", body);
        let config = Config::load(&root).expect("a fixture install loads");
        crate::fixture::cleanup(&root);
        config
    }

    /// The ceiling the queue reports is the ceiling the pass works to, and both of them come from the
    /// file rather than from a number either binary carries.
    ///
    /// This is the promise behind "how long may one run take": a caller that matched the bridge's figure
    /// would be doing exactly what the user's own idle pass does, on the same budget, with no third copy
    /// of the two numbers to fall out of step.
    #[test]
    fn the_queue_reports_the_same_run_budget_the_settings_file_writes() {
        let written = settings(r#"{"summary_pending_days_in_idle": 6, "summary_stretch_limit_in_idle": 15, "enable_ai_summary_in_idle": false}"#);
        let out = one_run_may_take(&written);
        assert_eq!(out["days"], json!(6), "the Recording page's first budget row");
        assert_eq!(out["stretches"], json!(15), "and its second");
        assert_eq!(out["switched_on"], json!(false), "the AI page's switch, off");

        // A file that predates all three still answers two, forty and on — the values the pass carried
        // as constants, now held once in `wind_base::config`.
        let shipped = settings("{}");
        let out = one_run_may_take(&shipped);
        assert_eq!((out["days"].as_i64(), out["stretches"].as_i64()), (Some(2), Some(40)), "{out}");
        assert_eq!(out["switched_on"], json!(true));
    }

    /// The language the queue and `windrecorder_prompts_read` hand out is this install's own, from the one
    /// table in `wind-base`: the bridge names no language phrase of its own, so an outside producer is told
    /// the same sentence this machine's own pass would be told.
    #[test]
    fn the_queue_hands_out_the_answer_language_this_install_would_use() {
        assert_eq!(Prompts::read(&settings(r#"{"lang": "sc"}"#)).language, "Chinese (Simplified Han)");
        assert_eq!(Prompts::read(&settings(r#"{"lang": "ja"}"#)).language, "Japanese");
        // An install whose file names no `lang` gets the shipped default, which is the English case.
        let shipped = settings("{}");
        assert_eq!(Prompts::read(&shipped).language, "English");
        assert_eq!(prompts::answer_language(&shipped), "English", "and the payload reads the same function");
    }
}
