//! The summary surface over the tool functions: the queue, the two writers, the gate, and the prompts.
//!
//! These go through `wind_mcp::tools::call`, which is the same function the JSON-RPC dispatcher and the
//! command line both reach — so a tool that works only over a socket, or only from argv, cannot happen.
//! `bridge.rs` covers the transport; this file covers what the five new tools promise, including the two
//! things the design is actually about: **the queue carries the whole text** and **a day cannot be
//! summarised over a gap it does not admit to**.
//!
//! Every fixture is a synthetic install under the OS temp directory, built with the bridge's own writer,
//! so the month files are byte-identical to what the recorder produces and none of this touches a real
//! `userdata/`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use wind_mcp::fixture;
use wind_mcp::tools::call;
use wind_mcp::{Axis, Runtime};

const DAY: &str = "2026-09-27";

/// A day with two stretches, one of them long enough to prove nothing is clipped.
fn two_stretches() -> Vec<fixture::Row> {
    // `Row` holds `&'static str` fields, so a fixture text built at runtime has to be leaked. Leaking a
    // few kilobytes per test process is the cheaper of the two bad options here; the alternative is a
    // `String`-typed fixture API that every existing call site would then have to clone through.
    let long: &'static str = Box::leak("x".repeat(5_000).into_boxed_str());
    vec![
        fixture::Row::new("2026-09-27_09-00-10", "quarterly forecast sheet", Some("Excel — Q3"))
            .segment("2026-09-27_09-00-00"),
        fixture::Row::new("2026-09-27_09-01-10", &long, Some("Excel — Q3")).segment("2026-09-27_09-00-00"),
        fixture::Row::new("2026-09-27_10-00-05", "chat with 张伟 about the deadline", Some("WeChat"))
            .segment("2026-09-27_10-00-00"),
    ]
}

fn install(tag: &str) -> PathBuf {
    let root = fixture::install(tag, r#"{"user_name": "default"}"#);
    fixture::month(&root, "default", 2026, 9, &two_stretches());
    root
}

fn run(root: &Path, name: &str, args: Value) -> Value {
    let runtime = Runtime::open(root).expect("runtime");
    let axis = Axis::measure();
    call(&runtime, &axis, name, &args).unwrap_or_else(|e| panic!("{name} rejected: {e}"))
}

fn refused(root: &Path, name: &str, args: Value) -> String {
    let runtime = Runtime::open(root).expect("runtime");
    let axis = Axis::measure();
    match call(&runtime, &axis, name, &args) {
        Ok(value) => panic!("{name} was expected to refuse, answered: {value}"),
        Err(rejected) => rejected.to_string(),
    }
}

// ---------------------------------------------------------------- the queue

#[test]
fn a_day_nobody_has_written_about_comes_back_needing_two_stretches_with_their_text() {
    let root = install("pending-empty");
    let queue = run(&root, "windrecorder_summaries_pending", json!({ "day": DAY }));
    assert_eq!(queue["counted"]["segments_total"], json!(2), "{queue}");
    assert_eq!(queue["counted"]["summarised"], json!(0));
    assert_eq!(queue["pending"].as_array().expect("list").len(), 2, "two stretches, oldest first");
    assert!(queue["stale"].as_array().expect("list").is_empty());
    assert!(queue["current"].is_null(), "the default queue is the work, not the whole day");

    let first = &queue["pending"][0];
    assert_eq!(first["segment"], json!("2026-09-27_09-00-00"));
    assert_eq!(first["reason"], json!("missing"));
    assert_eq!(first["frames"], json!(2));
    assert_eq!(first["video_file"], json!("2026-09-27_09-00-00.mp4"));
    assert_eq!(first["titles"], json!(["Excel — Q3"]));

    // The promise the whole tool exists for: the text is here, in full, no second call.
    let frames = first["frames_detail"].as_array().expect("frames");
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1]["text_chars"], json!(5_000), "the long frame reports its real length");
    assert_eq!(frames[1]["text"].as_str().expect("text").chars().count(), 5_000, "and returns all of it");
    assert_eq!(frames[1]["clipped"], json!(false));
    assert_eq!(queue["pending"][1]["frames_detail"][0]["text"], json!("chat with 张伟 about the deadline"));
    assert_eq!(queue["pending"][1]["frames_detail"][0]["title"], json!("WeChat"));

    // The prompt travels with the queue, so an outside AI writes the same shape of paragraph.
    assert!(queue["prompt"]["period_summary"]["user"]
        .as_str()
        .expect("template")
        .contains("{frames_table}"));
    assert!(queue["prompt"]["placeholders"]["{frames_table}"].as_str().expect("gloss").contains("frame"));
    assert!(queue["where_summaries_live"]["period"].as_str().expect("dir").contains("result_ai_period_summary"));
    fixture::cleanup(&root);
}

#[test]
fn a_caller_that_asks_for_less_text_gets_less_text_and_is_told_it_was_cut() {
    let root = install("pending-clip");
    let queue = run(&root, "windrecorder_summaries_pending", json!({ "day": DAY, "max_text_chars": 10 }));
    let frames = queue["pending"][0]["frames_detail"].as_array().expect("frames");
    assert_eq!(frames[1]["text"].as_str().expect("text").chars().count(), 10);
    assert_eq!(frames[1]["text_chars"], json!(5_000), "the real length survives the clip");
    assert_eq!(frames[1]["clipped"], json!(true));
    assert_eq!(frames[0]["text"].as_str().expect("text").chars().count(), 10, "a 24-character frame clips too");
    assert_eq!(frames[0]["clipped"], json!(true));
    let whole = run(&root, "windrecorder_summaries_pending", json!({ "day": DAY, "max_text_chars": 0 }));
    assert_eq!(whole["pending"][0]["frames_detail"][0]["clipped"], json!(false), "0 means nothing is cut");
    fixture::cleanup(&root);
}

#[test]
fn include_all_shows_the_finished_stretches_beside_the_ones_left() {
    let root = install("pending-current");
    run(
        &root,
        "windrecorder_period_summary_write",
        json!({ "segment": "2026-09-27_09-00-00", "text": "the forecast sheet", "written_by": "qoder" }),
    );
    let queue = run(&root, "windrecorder_summaries_pending", json!({ "day": DAY, "include": "all" }));
    assert_eq!(queue["current"].as_array().expect("current").len(), 1);
    assert_eq!(queue["current"][0]["segment"], json!("2026-09-27_09-00-00"));
    assert_eq!(queue["current"][0]["written_by"], json!("qoder"), "who wrote it is visible in the queue too");
    assert_eq!(queue["pending"].as_array().expect("pending").len(), 1);
    assert_eq!(queue["counted"]["summarised"], json!(1));
    fixture::cleanup(&root);
}

#[test]
fn an_explicit_range_spans_two_days_and_a_stretch_is_asked_for_once() {
    let root = fixture::install("pending-range", r#"{"user_name": "default"}"#);
    fixture::month(
        &root,
        "default",
        2026,
        9,
        &[
            fixture::Row::new("2026-09-26_20-00-00", "friday", Some("notepad")).segment("2026-09-26_20-00-00"),
            // 02:00 is still Thursday's product day at the shipped 03:00 rollover.
            fixture::Row::new("2026-09-27_02-00-00", "late", Some("notepad")).segment("2026-09-27_02-00-00"),
            fixture::Row::new("2026-09-27_11-00-00", "noon", Some("notepad")).segment("2026-09-27_11-00-00"),
        ],
    );
    let queue = run(&root, "windrecorder_summaries_pending", json!({ "day": "2026-09-27" }));
    let keys: Vec<&str> = queue["pending"].as_array().expect("pending").iter().map(|item| item["segment"].as_str().expect("key")).collect();
    assert_eq!(keys, vec!["2026-09-27_11-00-00"], "the 02:00 stretch belongs to the 26th, once: {queue}");
    assert_eq!(queue["counted"]["days"], json!(1));

    // A bare `end` is that instant, as it is for every other range tool in this bridge — the whole
    // library's convention, so a caller that means "through the 27th" says the time.
    let across = run(&root, "windrecorder_summaries_pending", json!({ "start": "2026-09-26", "end": "2026-09-27 23:59:59" }));
    assert_eq!(across["counted"]["days"], json!(3), "00:00 on the 26th is still the 25th's product day");
    assert_eq!(across["counted"]["segments_total"], json!(3), "a whole range asks for each stretch once");
    let listed: Vec<&str> = across["pending"].as_array().expect("pending").iter().map(|item| item["segment"].as_str().expect("key")).collect();
    assert_eq!(listed, vec!["2026-09-26_20-00-00", "2026-09-27_02-00-00", "2026-09-27_11-00-00"]);
    fixture::cleanup(&root);
}

// ---------------------------------------------------------------- the writers

#[test]
fn a_written_stretch_answers_to_all_three_ways_of_naming_it() {
    let root = install("write-names");
    let by_file = run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_09-00-00.mp4", "text": "one" }));
    assert_eq!(by_file["segment"], json!("2026-09-27_09-00-00"));
    assert_eq!(by_file["replaced"], json!(false));
    assert_eq!(by_file["frames"], json!(2), "the frame count comes from the index, not the caller");
    assert_eq!(by_file["ocr_chars"], json!(5_024), "24 characters of the short frame plus the 5 000 of the long one");

    let by_stamp = run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_09-00-00", "text": "two" }));
    assert_eq!(by_stamp["replaced"], json!(true), "the same stretch, so the same paragraph slot");
    assert_eq!(run(&root, "windrecorder_summaries_read", json!({ "day": DAY }))["days"][0]["period"]["entries"]
        .as_array()
        .expect("entries")
        .len(), 1, "three spellings must not make three summaries");

    let inside = fixture::at("2026-09-27_09-01-10").to_string();
    let by_instant =
        run(&root, "windrecorder_period_summary_write", json!({ "segment": inside, "day": DAY, "text": "three" }));
    assert_eq!(by_instant["segment"], json!("2026-09-27_09-00-00"), "a moment inside the stretch resolves to it");
    fixture::cleanup(&root);
}

#[test]
fn a_summary_is_stored_byte_for_byte_and_a_made_up_stretch_is_refused() {
    let root = install("write-text");
    let body = "第一段。\n\n第二行 with an \"quote\" and a ‘ curly one ’, and 12345 symbols.";
    let written = run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_10-00-00", "text": body }));
    assert_eq!(written["text_chars"], json!(body.chars().count()));
    let read = run(&root, "windrecorder_summaries_read", json!({ "day": DAY, "kind": "period" }));
    let entries = read["days"][0]["period"]["entries"].as_array().expect("entries");
    let stored = entries.iter().find(|e| e["segment"] == json!("2026-09-27_10-00-00")).expect("the one written");
    assert_eq!(stored["text"], json!(body), "newlines, CJK and quotes survive the round trip");

    for reference in ["2026-09-27_23-59-59", "2026-09-27_09-30-00", "not-a-segment"] {
        let message = refused(&root, "windrecorder_period_summary_write", json!({ "segment": reference, "text": "x", "day": DAY }));
        assert!(message.contains("no recorded segment") || message.contains("is not a recording segment"), "{reference}: {message}");
    }
    assert!(refused(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_10-00-00" })).contains("text is required"));
    assert!(refused(&root, "windrecorder_period_summary_write", json!({ "text": "x" })).contains("segment is required"));
    let bare_instant = fixture::at("2026-09-27_10-00-05").to_string();
    assert!(refused(&root, "windrecorder_period_summary_write", json!({ "segment": bare_instant, "text": "x" })).contains("give `day`"),
        "a bare timestamp must not become an unbounded library scan");
    fixture::cleanup(&root);
}

#[test]
fn the_daily_gate_refuses_a_half_written_day_and_names_what_is_missing() {
    let root = install("gate-refuses");
    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_09-00-00", "text": "morning" }));
    let message = refused(&root, "windrecorder_day_summary_write", json!({ "date": DAY, "text": "a whole day" }));
    assert!(message.contains("holds 2 recorded stretches") && message.contains("1 of them"), "the refusal counts: {message}");
    assert!(message.contains("2026-09-27_10-00-00"), "and names the stretch: {message}");
    assert!(message.contains("allow_partial"), "and says how to proceed knowingly: {message}");
    assert!(summary_file(&root, "daily", DAY).is_none(), "a refused write leaves nothing behind");

    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_10-00-00", "text": "chat" }));
    let written = run(&root, "windrecorder_day_summary_write", json!({ "date": DAY, "text": "a whole day", "written_by": "qoder" }));
    assert_eq!(written["partial"], json!(false));
    assert_eq!(written["coverage"]["missing"], json!([]));
    assert_eq!(written["replaced"], json!(false));
    let again = run(&root, "windrecorder_day_summary_write", json!({ "date": DAY, "text": "edited" }));
    assert_eq!(again["replaced"], json!(true), "a day has one summary, and rewriting it says so");
    fixture::cleanup(&root);
}

#[test]
fn allow_partial_writes_a_day_that_admits_its_own_gap() {
    let root = install("gate-partial");
    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_09-00-00", "text": "morning" }));
    let written = run(&root, "windrecorder_day_summary_write", json!({ "date": DAY, "text": "half a day", "allow_partial": true }));
    assert_eq!(written["partial"], json!(true));
    assert_eq!(written["coverage"]["segments_summarised"], json!(1));
    assert_eq!(written["coverage"]["missing"], json!(["2026-09-27_10-00-00"]), "the gap is recorded with the text");

    let read = run(&root, "windrecorder_summaries_read", json!({ "day": DAY, "kind": "daily" }));
    assert_eq!(read["days"][0]["daily"]["state"], json!("partial"));
    assert_eq!(read["days"][0]["daily"]["partial"], json!(true));
    assert_eq!(read["days"][0]["daily"]["text"], json!("half a day"));

    // The queue then keeps offering the day, because its premise is a gap.
    let queue = run(&root, "windrecorder_summaries_pending", json!({ "day": DAY }));
    assert_eq!(queue["days_pending"].as_array().expect("days").len(), 1);
    let day = &queue["days_pending"][0];
    assert!(day["reasons"].as_array().expect("reasons").contains(&json!("coverage_incomplete")), "{day}");
    assert_eq!(day["previous_text"], json!("half a day"), "and the old paragraph comes back to rewrite from");
    fixture::cleanup(&root);
}

#[test]
fn a_day_with_nothing_recorded_has_nothing_to_wait_for() {
    let root = install("gate-empty");
    let queue = run(&root, "windrecorder_summaries_pending", json!({ "day": "2026-09-05" }));
    assert_eq!(queue["counted"]["segments_total"], json!(0));
    assert!(queue["pending"].as_array().expect("pending").is_empty());
    assert!(queue["days_pending"].as_array().expect("days").is_empty(), "an empty day is not a day awaiting a summary");
    fixture::cleanup(&root);
}

// ---------------------------------------------------------------- reading back

#[test]
fn an_undocumented_day_and_an_empty_documented_day_say_different_things() {
    let root = install("read-states");
    let read = run(&root, "windrecorder_summaries_read", json!({ "day": DAY }));
    assert!(read["days"].as_array().expect("days").is_empty(), "nothing exists yet: {read}");
    assert_eq!(read["absent_days"], json!([DAY]), "and that is stated as an absent day, not an empty list");
    assert_eq!(read["counts"]["period_summaries"], json!(0));

    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_09-00-00", "text": "one" }));
    let read = run(&root, "windrecorder_summaries_read", json!({ "day": DAY }));
    assert!(read["absent_days"].as_array().expect("absent").is_empty());
    assert_eq!(read["days"][0]["period"]["state"], json!("answered"));
    assert_eq!(read["days"][0]["daily"]["state"], json!("not_generated"), "the other family is still untouched");
    assert_eq!(read["counts"]["period_summaries"], json!(1));

    // A file that is there and is not a summary file is neither of the above.
    let daily_dir = root.join("userdata/result_ai_daily_summary");
    std::fs::create_dir_all(&daily_dir).expect("dir");
    std::fs::write(daily_dir.join(format!("{DAY}.json")), b"{ oops").expect("torn");
    let read = run(&root, "windrecorder_summaries_read", json!({ "day": DAY, "kind": "daily" }));
    assert_eq!(read["days"][0]["daily"]["state"], json!("unreadable"));
    assert!(read["days"][0]["daily"]["note"].as_str().expect("note").contains("is not a day summary"));
    fixture::cleanup(&root);
}

#[test]
fn a_range_read_walks_days_and_keeps_the_two_families_apart() {
    let root = fixture::install("read-range", r#"{"user_name": "default"}"#);
    fixture::month(
        &root,
        "default",
        2026,
        9,
        &[
            fixture::Row::new("2026-09-26_10-00-00", "thursday", Some("notepad")).segment("2026-09-26_10-00-00"),
            fixture::Row::new("2026-09-27_10-00-00", "friday", Some("notepad")).segment("2026-09-27_10-00-00"),
        ],
    );
    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-26_10-00-00", "text": "wrote thursday up" }));
    run(&root, "windrecorder_day_summary_write", json!({ "date": "2026-09-26", "text": "a thursday" }));
    let read = run(&root, "windrecorder_summaries_read", json!({ "start": "2026-09-26 03:00:00", "end": "2026-09-27 23:59:59" }));
    // A day nothing has ever been written about is named in `absent_days` rather than carried as an
    // entry of two `not_generated` families: same fact, less payload, and the queue says it the same way.
    assert_eq!(read["days"].as_array().expect("days").len(), 1, "{read}");
    assert_eq!(read["days"][0]["date"], json!("2026-09-26"));
    assert_eq!(read["days"][0]["period"]["state"], json!("answered"));
    assert_eq!(read["days"][0]["daily"]["state"], json!("answered"));
    let from = read["days"][0]["range"]["from"].as_str().expect("rendered");
    assert!(from.starts_with("2026-09-26T03:00:00"), "the product day, stated on the service's axis: {from}");
    assert!(from.ends_with("+08:00") || from.ends_with("Z") || from.contains('+') || from.contains('-'), "with an explicit offset: {from}");
    assert_eq!(read["absent_days"], json!(["2026-09-27"]), "the 27th has had nothing written");
    assert_eq!(read["counts"]["period_summaries"], json!(1));
    assert_eq!(read["counts"]["daily_summaries"], json!(1));
    fixture::cleanup(&root);
}

// ---------------------------------------------------------------- the prompts

#[test]
fn the_prompts_that_would_be_sent_are_the_prompts_the_files_hold() {
    let root = install("prompts-shipped");
    let read = run(&root, "windrecorder_prompts_read", json!({}));
    let list = read["prompts"].as_array().expect("prompts");
    assert_eq!(list.len(), 7, "every registered template: {list:?}");
    let names: Vec<&str> = list.iter().map(|entry| entry["name"].as_str().expect("name")).collect();
    assert_eq!(
        names,
        vec![
            "period_summary_system",
            "period_summary_user",
            "daily_summary_system",
            "daily_summary_user",
            "tags_system",
            "tags_user",
            "search_system"
        ]
    );
    for entry in list {
        assert!(!entry["text"].as_str().expect("text").trim().is_empty(), "{} is empty", entry["name"]);
        // A synthetic install ships no `ai_prompts` directory, so the compiled-in copy of the same file
        // answers — which is the fallback that keeps a moved `config_src` from sending empty requests.
        assert_eq!(entry["origin"], json!("embedded"), "{}", entry["name"]);
        assert_eq!(entry["overridden"], json!(false));
    }
    let tags = list.iter().find(|entry| entry["name"] == json!("tags_system")).expect("tags");
    assert!(tags["text"].as_str().expect("text").contains("at most {max_tags} tags"), "the slot is a slot: {tags}");
    assert!(tags["text"].as_str().expect("text").contains("screen_time"), "the pre-migration wording survived");
    fixture::cleanup(&root);
}

#[test]
fn editing_a_prompt_puts_every_finished_summary_back_in_the_queue_and_says_why() {
    let root = install("prompts-changed");
    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_09-00-00", "text": "morning" }));
    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_10-00-00", "text": "chat" }));
    run(&root, "windrecorder_day_summary_write", json!({ "date": DAY, "text": "a day" }));
    assert!(run(&root, "windrecorder_summaries_pending", json!({ "day": DAY }))["pending"].as_array().expect("p").is_empty());

    let dir = root.join("userdata/ai_prompts");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join("period_summary_user.txt"),
        "Here is what the screen held:\n{frames_table}\nAnswer in three sentences.",
    )
    .expect("override");

    let read = run(&root, "windrecorder_prompts_read", json!({}));
    let period_user = read["prompts"].as_array().expect("list").iter().find(|e| e["name"] == json!("period_summary_user")).expect("entry");
    assert_eq!(period_user["origin"], json!("user"));
    assert_eq!(period_user["overridden"], json!(true), "the settings screen and this tool read the same file");
    assert!(period_user["shipped"].as_str().expect("shipped").contains("{segment}"), "the default stays visible beside it");

    let queue = run(&root, "windrecorder_summaries_pending", json!({ "day": DAY }));
    let reasons: Vec<&str> = queue["stale"].as_array().expect("stale").iter().map(|item| item["reason"].as_str().expect("r")).collect();
    assert_eq!(reasons, vec!["prompt_changed", "prompt_changed"], "{queue}");
    assert!(queue["stale"][0]["note"].as_str().expect("note").contains("prompt"));
    assert_eq!(queue["stale"][0]["previous_text"], json!("morning"), "the old paragraph is kept to improve on");
    assert_eq!(queue["counted"]["summarised"], json!(0), "a summary written under other words is not this prompt's coverage");
    assert!(queue["prompt"]["period_summary"]["user"].as_str().expect("text").contains("three sentences"), "the queue carries the new words");

    // The day's summary is flagged for a redo too, and the stretch it was written from is listed.
    let day = &queue["days_pending"][0];
    assert!(day["reasons"].as_array().expect("reasons").contains(&json!("summaries_changed")), "{day}");
    assert_eq!(day["period_summaries"].as_array().expect("paragraphs").len(), 2);

    // Restoring the default means deleting the override; the queue then agrees with what was written.
    std::fs::remove_dir_all(&dir).expect("removed");
    let after = run(&root, "windrecorder_summaries_pending", json!({ "day": DAY }));
    assert!(after["stale"].as_array().expect("stale").is_empty(), "{after}");
    assert!(after["days_pending"].as_array().expect("days").is_empty());
    fixture::cleanup(&root);
}

// ---------------------------------------------------------------- the argument gate

#[test]
fn the_new_tools_refuse_an_argument_they_never_published() {
    let root = install("args");
    let message = refused(&root, "windrecorder_summaries_pending", json!({ "day": DAY, "day_s": "2026-09-27" }));
    assert!(message.contains("does not take `day_s`"), "{message}");
    assert!(message.contains("Did you mean `day`"), "the new schemas feed the suggestion table: {message}");
    assert!(refused(&root, "windrecorder_period_summary_write", json!({ "segment": "x", "text": "y", "typo": 1 })).contains("does not take `typo`"));
    assert!(refused(&root, "windrecorder_day_summary_write", json!({ "date": DAY, "text": "y", "allow_partial": "yes" })).contains("allow_partial must be true or false"));
    assert!(refused(&root, "windrecorder_summaries_read", json!({ "day": DAY, "kind": "everything" })).contains("kind must be"));
    assert!(refused(&root, "windrecorder_summaries_pending", json!({})).contains("start"), "an unbounded queue is not offered");
    fixture::cleanup(&root);
}

#[test]
fn tools_list_announces_the_two_writers_as_writers() {
    let listed = wind_mcp::jsonrpc::tool_list(&Axis::measure());
    let tools = listed["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 11, "six reads, three more reads, two writes");
    for (name, read_only) in [
        ("windrecorder_summaries_pending", true),
        ("windrecorder_summaries_read", true),
        ("windrecorder_prompts_read", true),
        ("windrecorder_period_summary_write", false),
        ("windrecorder_day_summary_write", false),
    ] {
        let tool = tools.iter().find(|t| t["name"] == json!(name)).unwrap_or_else(|| panic!("{name} is not published"));
        assert_eq!(tool["annotations"]["readOnlyHint"], json!(read_only), "{name}");
        assert_eq!(tool["annotations"]["destructiveHint"], json!(false), "{name} overwrites one paragraph, not a history");
        assert_eq!(tool["inputSchema"]["additionalProperties"], json!(false), "{name} keeps its promise about extra arguments");
    }
    let pending = tools.iter().find(|t| t["name"] == json!("windrecorder_summaries_pending")).expect("pending");
    let properties = pending["inputSchema"]["properties"].as_object().expect("properties");
    assert_eq!(properties["max_text_chars"]["minimum"], json!(0), "the clip defaults to whole, and says so");
    assert_eq!(properties["max_text_chars"]["maximum"], json!(100_000));
}

/// The stored JSON of one family's day file, or `None` when the day was never written.
fn summary_file(root: &Path, family: &str, day: &str) -> Option<Value> {
    let dir = match family {
        "period" => root.join("userdata/result_ai_period_summary"),
        "daily" => root.join("userdata/result_ai_daily_summary"),
        other => panic!("{other}"),
    };
    let text = std::fs::read_to_string(dir.join(format!("{day}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

#[test]
fn a_written_day_lands_in_the_file_the_day_is_named_by() {
    let root = install("files-layout");
    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_09-00-00", "text": "morning" }));
    run(&root, "windrecorder_period_summary_write", json!({ "segment": "2026-09-27_10-00-00", "text": "chat" }));
    let period = summary_file(&root, "period", DAY).expect("period file");
    assert_eq!(period.as_object().expect("map").len(), 2, "one file, two stretches: {period}");
    assert!(period["2026-09-27_09-00-00"]["source_fingerprint"].as_str().expect("fp").len() == 16);
    assert!(summary_file(&root, "daily", DAY).is_none(), "no daily file until a daily is written");
    run(&root, "windrecorder_day_summary_write", json!({ "date": DAY, "text": "a day" }));
    let daily = summary_file(&root, "daily", DAY).expect("daily file");
    assert_eq!(daily["date"], json!(DAY), "the file names its own day, so a moved file cannot lie");
    assert_eq!(daily["partial"], json!(false));
    assert!(daily["source_fingerprint"].as_str().expect("fp").len() == 16);
    fixture::cleanup(&root);
}
