//! The AI caches behind `windrecorder_day_summary`, and the five different things "no answer" means.
//!
//! These are the tests for the day-versus-month key mismatch. `windai tags --month` writes
//! `userdata/result_ai_extract_tag/{year}.json` keyed `YYYY-MM`; the bridge used to read it keyed
//! `YYYY-MM-DD`, which is a lookup that succeeds on a cache left behind by the old Python app and
//! returns nothing on every install that has ever run the native tagger — with no error anywhere in
//! sight, and indistinguishable from "nobody tagged that day".
//!
//! The pure-function level, then the same answers over a real socket further down in this file.
//! Nothing here opens a port: the tool functions are the service's code path, not a parallel one,
//! which is the design `lib.rs` documents and `bridge.rs` checks at the transport.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use wind_mcp::fixture;

const DAY: &str = "2026-09-21";

fn install(tag: &str) -> PathBuf {
    let root = fixture::install(tag, r#"{"user_name": "default"}"#);
    fixture::month(&root, "default", 2026, 9, &fixture::busy_day());
    root
}

/// Write a tags cache exactly where, and exactly in the shape, `windai tags --month` writes one.
fn write_tags_cache(root: &Path, year: i64, entries: Value) {
    let dir = root.join("userdata/result_ai_extract_tag");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{year}.json")), serde_json::to_string_pretty(&entries).unwrap()).unwrap();
}

/// Write a day-poem cache — the file the install layout creates the directory for and nothing in
/// this build fills.
fn write_poem_cache(root: &Path, year: i64, text: Value) {
    let dir = root.join("userdata/result_ai_day_poem");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{year}.json")), serde_json::to_string_pretty(&text).unwrap()).unwrap();
}

fn raw_dir(root: &Path, contents: &str) {
    let dir = root.join("userdata/result_ai_extract_tag");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("2026.json"), contents).unwrap();
}

fn day(root: &Path, date: &str) -> Value {
    let runtime = wind_mcp::Runtime::open(root).unwrap();
    let axis = wind_mcp::Axis::measure();
    wind_mcp::tools::day_summary(&runtime, &axis, &json!({"date": date})).unwrap_or_else(|e| panic!("day_summary({date}) rejected: {e}"))
}

fn tags(root: &Path, date: &str) -> Value {
    day(root, date)["ai_tags"].clone()
}

fn summary(root: &Path, date: &str) -> Value {
    day(root, date)["ai_summary"].clone()
}

/// The regression this file exists for. Any change that lets a day inside a tagged month come back
/// empty — by dropping the fallback, by keying the lookup on the day alone, or by swallowing the
/// month entry — fails here on the first assertion rather than in front of an assistant.
#[test]
fn a_day_inside_a_tagged_month_is_answered_from_the_month_and_says_so() {
    let root = install("ai-month-fallback");
    write_tags_cache(&root, 2026, json!({"2026-09": ["rust", "refactoring", "mcp"]}));

    let tags = tags(&root, DAY);
    assert!(tags["available"].as_bool() == Some(true), "a tagged month answered a day inside it with nothing: {tags}");
    assert_eq!(tags["state"], json!("answered"));
    assert_eq!(tags["tags"].as_array().map(Vec::len), Some(3), "{tags}");
    assert_eq!(tags["tags"][0], json!("rust"));
    // The substitution is only honest while the payload names it. A month's tags delivered under a
    // day's heading with no granularity is the same deception one layer up.
    assert_eq!(tags["granularity"], json!("month"), "a month answer must not read as a day answer");
    assert_eq!(tags["cache_key"], json!("2026-09"));
    assert!(tags["file"].as_str().unwrap().ends_with("2026.json"), "{tags}");
    let note = tags["note"].as_str().unwrap();
    assert!(note.contains("2026-09"), "the note must name the month it answered from: {note}");
    assert!(note.contains(DAY), "and the day it was asked about: {note}");
}

/// A day entry is the more specific answer and wins — including when it is the *empty* answer, which
/// is the one case a tool is most tempted to override with something that looks better.
#[test]
fn a_day_entry_wins_and_an_empty_day_entry_is_not_overridden_by_the_month() {
    let root = install("ai-day-wins");
    write_tags_cache(
        &root,
        2026,
        json!({"2026-09": ["rust", "refactoring"], "2026-09-21": ["contracts", "law"], "2026-09-22": []}),
    );

    let day = tags(&root, DAY);
    assert_eq!(day["granularity"], json!("day"), "a legacy day entry must be served as the day's own");
    assert_eq!(day["cache_key"], json!(DAY));
    assert_eq!(day["tags"], json!(["contracts", "law"]));

    // Upstream writes `[]` on purpose for a day whose every window title was filtered out. Answering
    // that with the month's livelier list would be picking the truer-looking reply over the true one.
    let empty = tags(&root, "2026-09-22");
    assert_eq!(empty["state"], json!("generated_empty"), "{empty}");
    assert_eq!(empty["granularity"], json!("day"), "the emptiness is the day's, not the month's");
    assert_eq!(empty["tags"], json!([]));
    assert_eq!(empty["available"], json!(false), "an empty answer is not an available one");
    assert!(empty["note"].as_str().unwrap().contains("nothing to report"), "{}", empty["note"]);
}

/// "This month has never been tagged", "this year never has", and "the tagger ran and wrote an empty
/// year" are three facts with three different fixes. They were one shape before this payload existed.
#[test]
fn an_untagged_month_and_a_cache_that_never_ran_say_different_things() {
    let tagged = install("ai-untagged-month");
    write_tags_cache(&tagged, 2026, json!({"2026-08": ["teaching"], "2026-09": ["rust"]}));
    let outside = tags(&tagged, "2026-11-05");
    assert_eq!(outside["state"], json!("not_generated"), "{outside}");
    assert_eq!(outside["granularity"], Value::Null, "no answer, so no width claimed");
    assert_eq!(outside["available"], json!(false));
    assert_eq!(outside["tags"], json!([]));
    // The machine-readable half of the difference, so a client can branch on it without reading the
    // sentence: the cache is there and this period is not in it, versus no cache at all.
    assert_eq!(outside["cache_file_present"], json!(true), "{outside}");
    let note = outside["note"].as_str().unwrap();
    assert!(note.contains("2026-11"), "the answer must name the month that is missing: {note}");
    assert!(note.contains("2 month"), "and what the file does hold, so the gap is locatable: {note}");

    let never = tags(&install("ai-no-cache-at-all"), DAY);
    assert_eq!(never["state"], json!("not_generated"), "{never}");
    assert_eq!(never["cache_file_present"], json!(false), "{never}");
    assert!(never["note"].as_str().unwrap().contains("does not exist"), "{}", never["note"]);
    assert_ne!(outside["note"], never["note"], "the two kinds of nothing must not read the same");

    // A file that exists and holds nothing is a third sentence again: the feature ran, and wrote an
    // empty year. That is not the same claim as the file being absent.
    let blank = install("ai-blank-cache");
    write_tags_cache(&blank, 2026, json!({}));
    let blank = tags(&blank, DAY);
    assert_eq!(blank["state"], json!("not_generated"), "{blank}");
    assert!(blank["note"].as_str().unwrap().contains("0 month"), "{}", blank["note"]);
}

/// An unreadable cache is a broken file, not an empty one. Letting it answer "there are no tags" is
/// the one failure mode a client can do nothing about unless it is named.
#[test]
fn an_unreadable_cache_is_never_reported_as_having_no_tags() {
    let root = install("ai-unreadable");
    raw_dir(&root, "{ this is not json");
    let broken = tags(&root, DAY);
    assert_eq!(broken["state"], json!("unreadable"), "{broken}");
    assert_eq!(broken["available"], json!(false));
    assert_eq!(broken["tags"], json!([]));
    assert!(broken["note"].as_str().unwrap().contains("not \"there are no tags\""), "{}", broken["note"]);

    // A valid file with one corrupt entry says that too, instead of falling back to a wider key and
    // arriving looking like a working answer.
    let mixed = install("ai-malformed-entry");
    write_tags_cache(&mixed, 2026, json!({"2026-09-21": "not a list", "2026-09": ["rust"]}));
    let entry = tags(&mixed, DAY);
    assert_eq!(entry["state"], json!("malformed_entry"), "{entry}");
    assert_eq!(entry["tags"], json!([]), "a month answer must not leak in behind a corrupt day entry");
}

/// Upstream writes a failed generation back into the same list as `["retry_times:2"]`, and the old
/// web UI rendered that string as a visible tag pill. Handing it back as a tag would be quoting the
/// user their own retry counter and calling it their month's theme.
#[test]
fn a_retry_marker_is_reported_as_a_failure_and_never_as_an_answer() {
    let root = install("ai-retry");
    write_tags_cache(&root, 2026, json!({"2026-09": ["retry_times:2"]}));
    let tags = tags(&root, DAY);
    assert_eq!(tags["state"], json!("generation_failed"), "{tags}");
    assert_eq!(tags["tags"], json!([]));
    assert_eq!(tags["granularity"], Value::Null, "a run that failed proved nothing about any width");
    assert!(tags["note"].as_str().unwrap().contains("retry_times:2"), "{}", tags["note"]);

    write_poem_cache(&root, 2026, json!({DAY: "retry_times:1"}));
    let summary = summary(&root, DAY);
    assert_eq!(summary["state"], json!("generation_failed"), "{summary}");
    assert_eq!(summary["text"], Value::Null, "the marker must not be served as prose");
}

/// The month fallback deliberately does *not* carry over to the summary: upstream built its month
/// view out of day poems, so there is no month entry to serve, and a poem composed from a month's
/// tags would be a different kind of object under the same field name.
#[test]
fn no_day_summary_is_said_plainly_and_no_month_summary_is_invented() {
    let root = install("ai-summary-absent");
    write_tags_cache(&root, 2026, json!({"2026-09": ["rust", "refactoring"]}));

    assert!(tags(&root, DAY)["available"].as_bool() == Some(true), "the tags half must still answer");
    let missing = summary(&root, DAY);
    assert_eq!(missing["state"], json!("not_generated"), "{missing}");
    assert_eq!(missing["text"], Value::Null);
    assert_eq!(missing["granularity"], Value::Null);
    assert_eq!(missing["available"], json!(false));
    let note = missing["note"].as_str().unwrap();
    assert!(note.contains("no month-level summary"), "the note must say why the tags' fallback does not carry over: {note}");
    assert!(note.contains("ai_tags"), "and point at the field that does answer, so the two are not confused: {note}");

    // A legacy day poem really is served, at day width, because it is the day's own answer.
    write_poem_cache(&root, 2026, json!({DAY: "  A month of refactors, one line at a time.  "}));
    let held = summary(&root, DAY);
    assert_eq!(held["state"], json!("answered"), "{held}");
    assert_eq!(held["granularity"], json!("day"));
    assert_eq!(held["cache_key"], json!(DAY));
    assert_eq!(held["text"], json!("A month of refactors, one line at a time."), "trimmed, not padded");
}

/// A month's tag list is generated 1.5x longer than a day's ever was, so the response cap is now a
/// cut that actually lands. It has to be a counted cut.
#[test]
fn a_truncated_month_list_reports_what_it_dropped() {
    let root = install("ai-truncated");
    let long: Vec<String> = (0..25).map(|n| format!("tag{n}")).collect();
    write_tags_cache(&root, 2026, json!({"2026-09": long}));

    let tags = tags(&root, DAY);
    assert_eq!(tags["tags"].as_array().map(Vec::len), Some(wind_mcp::tools::AI_TAGS_LIMIT));
    assert_eq!(tags["omitted_tags"], json!(25 - wind_mcp::tools::AI_TAGS_LIMIT), "{tags}");
    assert_eq!(tags["state"], json!("answered"), "the head of the list is still the month's answer");
}

/// Both AI fields are present on every day summary, always. A client that has to code for "the field
/// might simply not be here" cannot tell a broken bridge from an untagged month, which is the
/// original complaint wearing a different coat.
#[test]
fn every_day_summary_carries_both_ai_fields_whether_or_not_they_answer() {
    let root = install("ai-always-present");
    let day = day(&root, DAY);
    assert!(day.get("ai_tags").is_some(), "ai_tags vanished from the payload: {day}");
    assert!(day.get("ai_summary").is_some(), "ai_summary vanished from the payload: {day}");
    assert_eq!(day["ai_tags"]["state"], json!("not_generated"));
    assert_eq!(day["ai_summary"]["state"], json!("not_generated"));
    for field in ["state", "available", "granularity", "cache_key", "file", "cache_file_present", "note"] {
        assert!(day["ai_tags"].get(field).is_some(), "ai_tags.{field} is missing: {}", day["ai_tags"]);
        assert!(day["ai_summary"].get(field).is_some(), "ai_summary.{field} is missing: {}", day["ai_summary"]);
    }
}

/// The same facts from `windrecorder_status`, visible *before* a client asks a question: that this
/// install's tags are month-shaped, and that a day-shaped entry is legacy data. A client that can
/// see this never has to infer the width from an empty answer.
#[test]
fn status_shows_which_granularity_the_tags_cache_actually_holds() {
    let root = install("ai-status");
    write_tags_cache(&root, 2026, json!({"2026-08": ["teaching"], "2026-09": ["rust"], "2026-09-21": ["contracts"]}));
    write_tags_cache(&root, 2025, json!({}));
    // `windai`'s fingerprint sibling, which is not a second year of tags.
    std::fs::write(root.join("userdata/result_ai_extract_tag/2026.hash.json"), json!({"2026-09": "abc"}).to_string()).unwrap();

    let runtime = wind_mcp::Runtime::open(&root).unwrap();
    let status = wind_mcp::tools::status(&runtime, &wind_mcp::Axis::measure());
    let tags = &status["ai_caches"]["tags"];
    assert_eq!(tags["granularity_generated_by_this_build"], json!("month"));
    assert_eq!(tags["years"].as_array().map(Vec::len), Some(2), "one entry per year file, and the \
            .hash.json sibling is not a year: {tags}");
    assert_eq!(tags["years"][0]["month_keys"], json!(0), "2025 is listed first and holds nothing");
    assert_eq!(tags["years"][1]["month_keys"], json!(2));
    assert_eq!(tags["years"][1]["day_keys"], json!(1), "a legacy day entry is visible here, which is \
            how a client learns a day answer is possible on this install");
    assert_eq!(status["ai_caches"]["summary"]["granularity_generated_by_this_build"], json!("none"));
    assert_eq!(status["ai_caches"]["summary"]["dir_present"], json!(false));
    assert!(status["ai_caches"]["note"].as_str().unwrap().contains("granularity"));
}
