//! End-to-end tests for the natural-language mapping, run with **no network and no API key**.
//!
//! The double is a real loopback HTTP listener (`crate::test_support::Canned`) and the index is a real
//! month database written by `wind_store::write::Store::open_month` in a temp directory. So the request
//! asserted here is the byte stream WinHTTP actually put on a socket, and the rows asserted back are the
//! ones SQLite actually returned for the query the plan described.
//!
//! The fixture month is four rows of a plausible Chinese user's September:
//!
//! | stamp | ocr_text | win_title |
//! |---|---|---|
//! | 2026-09-18 00:00:00 | 续约合同 终稿 draft | Word - 续约合同.docx |
//! | 2026-09-18 16:30:00 | renewal thread in wechat | 微信 - 张三 (WeChat) |
//! | 2026-09-19 10:00:00 | 无关的午餐菜单 lunch menu | Chrome |
//! | 2026-09-25 23:59:59 | 续约 renewal 第二次 | Word - 续约合同.docx |
//!
//! The first and last stamps are exactly midnight and the last second of a day on purpose: the library's
//! own bounds then fall on day boundaries, so a range inside September can be checked for *not* being
//! clamped, which is the half of the clamp contract that a widening test cannot show.

use super::*;
use crate::test_support::{self, Canned};
use serde_json::json;
use std::path::PathBuf;
use wind_base::clock::LocalParts;

fn epoch(stamp: &str) -> i64 {
    LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
}

const ROWS: &[(&str, &str, &str)] = &[
    ("2026-09-18_00-00-00", "续约合同 终稿 draft", "Word - 续约合同.docx"),
    ("2026-09-18_16-30-00", "renewal thread in wechat", "微信 - 张三 (WeChat)"),
    ("2026-09-19_10-00-00", "无关的午餐菜单 lunch menu", "Chrome"),
    ("2026-09-25_23-59-59", "续约 renewal 第二次", "Word - 续约合同.docx"),
];

/// A synthetic install, its month of index, and the client its configuration describes.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// `tag` must differ per test: two tests sharing a directory would share `_TEMP_READ.db` copies, and
    /// the staleness rule would then serve one test the other's database.
    fn build(tag: &str, server: &Canned, rows: &[(&str, &str, &str)]) -> Fixture {
        Fixture::build_at(tag, server, rows, 2026, 9)
    }

    fn build_at(tag: &str, server: &Canned, rows: &[(&str, &str, &str)], year: i64, month: u32) -> Fixture {
        let root = test_support::install(
            &format!("search-{tag}"),
            &json!({
                "open_ai_base_url": server.base_url(),
                "open_ai_api_key": "sk-somebody-elses-key",
                // Off so that a glyph table in the repo cannot widen a match and make an assertion here
                // depend on which characters the file happens to declare similar.
                "use_similar_ch_char_to_search": false
            }),
        );
        let owned: Vec<(String, String, Option<String>)> = rows
            .iter()
            .map(|(stamp, text, title)| (stamp.to_string(), text.to_string(), Some(title.to_string())))
            .collect();
        test_support::fixture_month(&test_support::db_dir(&root), "default", year, month, &owned);
        Fixture { root }
    }

    /// The pair a real caller has: one read of one configuration file, feeding both halves.
    fn pair(&self) -> (Index, Client<crate::client::WinHttp>) {
        let index = Index::open(&self.root).expect("the fixture install opens");
        let client = Client::new(index.settings.clone());
        (index, client)
    }
}

/// The sentence the feature exists for, mapped, clamped, run, and answered.
#[test]
fn a_phrase_becomes_a_query_and_finds_the_right_rows() {
    let canned = Canned::with_completion(
        &json!({
            "keywords": ["续约"],
            "exclude_keywords": ["菜单"],
            "applications": [],
            "start_date": "2026-09-18",
            "end_date": "2026-09-19",
            "occurrence": "any"
        })
        .to_string(),
    );
    let fixture = Fixture::build("phrase", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let outcome = run(&mut index, &client, "那封关于续约的邮件，上周下午", 20).expect("the search runs");

    assert_eq!(outcome.plan.keywords, vec!["续约"]);
    assert_eq!(outcome.plan.exclude, vec!["菜单"]);
    assert_eq!(
        (outcome.plan.from, outcome.plan.to),
        (epoch("2026-09-18_00-00-00"), epoch("2026-09-19_23-59-59"))
    );
    assert!(outcome.plan.notes.is_empty(), "a range inside the library must be untouched: {:?}", outcome.plan.notes);
    assert_eq!(outcome.months_searched, 1, "one month's dates open one file");
    assert_eq!(outcome.total, 1, "the 18th's contract row; the 25th is outside the range");
    assert_eq!(outcome.rows.len(), 1);
    assert!(outcome.rows[0].body().contains("续约"), "{:?}", outcome.rows[0].ocr_text);
    assert_eq!(outcome.rows[0].time, epoch("2026-09-18_00-00-00"));
    assert_eq!(outcome.usage, Some(Usage { prompt_tokens: 40, completion_tokens: 12, total_tokens: 52 }));
}

/// The assertion that the request really left this process and reached a socket: the listener saw a
/// well-formed POST whose system turn carries the library's own bounds and today's date.
#[test]
fn the_request_that_actually_left_the_process_is_the_documented_shape() {
    let canned =
        Canned::with_completion(r#"{"keywords":["x"],"start_date":"2026-09-18","end_date":"2026-09-19"}"#);
    let fixture = Fixture::build("wire", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    run(&mut index, &client, "anything at all", 5).unwrap();

    assert_eq!(canned.request_count(), 1, "one sentence, one request");
    let raw = canned.last_request();
    assert!(raw.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"), "{raw}");
    assert!(raw.contains("Content-Type: application/json"), "{raw}");
    assert!(raw.contains("Authorization: Bearer sk-somebody-elses-key"), "the header is sent: {raw}");
    assert!(raw.contains("User-Agent: windai/0.1"), "{raw}");

    let body = canned.request_json(0);
    assert_eq!(body["model"], "gpt-4o", "`open_ai_modelname` is what is called");
    assert_eq!(body["response_format"]["type"], "json_object");
    assert_eq!(body["temperature"], PARSE_TEMPERATURE);
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], "anything at all", "the phrase is the user turn");
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("2026-09-18") && system.contains("2026-09-25"), "the library's real span: {system}");
    assert!(system.contains("Today is "), "relative dates need an anchor");
}

#[test]
fn an_out_of_range_date_from_the_model_is_clamped_before_the_index_is_asked() {
    let canned = Canned::with_completion(
        &json!({"keywords": ["续约"], "start_date": "2020-01-01", "end_date": "2020-12-31"}).to_string(),
    );
    let fixture = Fixture::build("clamp", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let outcome = run(&mut index, &client, "续约", 20).unwrap();
    assert_eq!(
        (outcome.plan.from, outcome.plan.to),
        (epoch("2026-09-18_00-00-00"), epoch("2026-09-18_23-59-59")),
        "collapsed onto the first recorded day"
    );
    assert!(outcome.plan.notes.iter().any(|n| n.contains("entirely outside")), "{:?}", outcome.plan.notes);
    assert_eq!(outcome.total, 1, "and that day does hold a renewal, so the answer is still useful");
}

#[test]
fn a_partly_outside_range_is_cut_at_the_edge_it_crossed() {
    let canned = Canned::with_completion(
        &json!({"keywords": ["续约"], "start_date": "2026-01-01", "end_date": "2026-12-31"}).to_string(),
    );
    let fixture = Fixture::build("widen", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let outcome = run(&mut index, &client, "续约", 20).unwrap();
    assert_eq!((outcome.plan.from, outcome.plan.to), (epoch("2026-09-18_00-00-00"), epoch("2026-09-25_23-59-59")));
    assert_eq!(outcome.total, 2, "both renewal rows");
    assert!(outcome.plan.notes.iter().any(|n| n.contains("clamped")), "{:?}", outcome.plan.notes);
}

#[test]
fn a_garbage_answer_is_refused_without_opening_the_index() {
    let canned = Canned::start(vec![(200, test_support::completion_body("I cannot help with that."))]);
    let fixture = Fixture::build("garbage", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let error = run(&mut index, &client, "续约", 20).expect_err("prose is not a plan");
    assert_eq!(error.kind(), crate::error::ErrorKind::Model);
    assert_eq!(canned.request_count(), 1, "the refusal is about the answer, not about the call");
}

#[test]
fn a_server_side_failure_says_so_rather_than_looking_like_no_hits() {
    let canned = Canned::start(vec![(503, "upstream busy".to_string())]);
    let fixture = Fixture::build("503", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let error = run(&mut index, &client, "续约", 20).expect_err("a 503 is not an empty result set");
    assert_eq!(error.kind(), crate::error::ErrorKind::HttpStatus);
    assert!(error.to_string().contains("503"), "{error}");
    assert!(error.to_string().contains("upstream busy"), "{error}");
}

/// The redaction rule, exercised over a socket rather than in a unit test: an endpoint that echoes the
/// authorization header back inside its own 400 must not get the key into the text the user reads.
#[test]
fn an_endpoint_that_echoes_the_key_back_out_of_a_failing_request_has_it_removed() {
    let echo = r#"{"error":{"message":"bad Authorization: Bearer sk-somebody-elses-key"}}"#;
    let canned = Canned::start(vec![(400, echo.to_string())]);
    let fixture = Fixture::build("echo", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let error = run(&mut index, &client, "续约", 20).expect_err("a 400 is a failure");
    let rendered = error.to_string();
    assert!(!rendered.contains("sk-somebody-elses-key"), "the key leaked into {rendered}");
    assert!(rendered.contains(crate::error::REDACTED), "{rendered}");
    assert_eq!(error.kind(), crate::error::ErrorKind::HttpStatus);
    // The outbound request still had to carry it, of course.
    assert!(canned.last_request().contains("sk-somebody-elses-key"));
}

#[test]
fn the_title_filter_narrows_rows_and_reports_how_many_it_removed() {
    let canned = Canned::with_completion(
        &json!({"keywords": ["renewal"], "applications": ["Word"],
                "start_date": "2026-09-01", "end_date": "2026-09-30", "occurrence": "last"})
            .to_string(),
    );
    let fixture = Fixture::build("title", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let outcome = run(&mut index, &client, "在 Word 里改续约的时候", 20).unwrap();
    assert_eq!(outcome.total, 2, "two rows say renewal, in body or title");
    assert_eq!(outcome.rows.len(), 1, "the WeChat row is removed by title, not by the query");
    assert_eq!(outcome.dropped_by_title_filter, 1);
    assert!(outcome.rows[0].title().unwrap().contains("Word"));
    assert!(outcome.plan.notes.iter().any(|n| n.contains("clamped")), "{:?}", outcome.plan.notes);
}

#[test]
fn first_and_last_pick_opposite_ends_of_the_same_match_set() {
    for (occurrence, want_stamp) in [("first", "2026-09-18_00-00-00"), ("last", "2026-09-25_23-59-59")] {
        let canned = Canned::with_completion(
            &json!({"keywords": ["续约"], "start_date": "2026-09-18", "end_date": "2026-09-25",
                    "occurrence": occurrence})
                .to_string(),
        );
        let fixture = Fixture::build(&format!("order-{occurrence}"), &canned, ROWS);
        let (mut index, client) = fixture.pair();
        let outcome = run(&mut index, &client, "续约", 20).unwrap();
        assert_eq!(outcome.rows.len(), 2, "{occurrence}");
        assert_eq!(
            LocalParts::from_naive_epoch(outcome.rows[0].time).stamp(),
            want_stamp,
            "{occurrence} must lead with {want_stamp}"
        );
    }
}

#[test]
fn a_broad_question_without_keywords_searches_the_window_instead_of_failing() {
    let canned = Canned::with_completion(
        &json!({"keywords": [], "start_date": "2026-09-19", "end_date": "2026-09-19"}).to_string(),
    );
    let fixture = Fixture::build("broad", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let outcome = run(&mut index, &client, "我周六干了什么", 20).unwrap();
    assert!(outcome.plan.is_time_only());
    assert!(outcome.plan.keywords.is_empty());
    assert_eq!(outcome.total, 1, "the only row on the 19th");
    assert!(outcome.rows[0].body().contains("午餐"));
}

#[test]
fn the_display_limit_leaves_the_true_match_count_alone() {
    let canned = Canned::with_completion(
        &json!({"keywords": ["续约"], "start_date": "2026-09-18", "end_date": "2026-09-25"}).to_string(),
    );
    let fixture = Fixture::build("limit", &canned, ROWS);
    let (mut index, client) = fixture.pair();
    let outcome = run(&mut index, &client, "续约", 1).unwrap();
    assert_eq!(outcome.rows.len(), 1);
    assert_eq!(outcome.total, 2, "two matched, one is shown");
    assert!(!outcome.capped, "the scan was not cut short, so the list is complete");
}

#[test]
fn a_query_spanning_two_months_opens_both_and_merges_in_time_order() {
    let canned = Canned::with_completion(
        &json!({"keywords": ["续约"], "start_date": "2026-08-01", "end_date": "2026-09-30"}).to_string(),
    );
    let root = test_support::install(
        "search-two-months",
        &json!({"open_ai_base_url": canned.base_url(),
                "open_ai_api_key": "sk-somebody-elses-key",
                "use_similar_ch_char_to_search": false}),
    );
    let db = test_support::db_dir(&root);
    let owned: Vec<(String, String, Option<String>)> = ROWS
        .iter()
        .map(|(s, t, w)| (s.to_string(), t.to_string(), Some(w.to_string())))
        .collect();
    test_support::fixture_month(&db, "default", 2026, 9, &owned);
    test_support::fixture_month(
        &db,
        "default",
        2026,
        8,
        &[("2026-08-20_09-00-00".to_string(), "续约 renewal 第一次".to_string(), Some("Word".to_string()))],
    );
    let mut index = Index::open(&root).unwrap();
    let client = Client::new(index.settings.clone());
    let outcome = run(&mut index, &client, "续约", 20).unwrap();
    assert_eq!(outcome.months_searched, 2);
    assert_eq!(outcome.total, 3);
    assert_eq!(
        LocalParts::from_naive_epoch(outcome.rows.last().unwrap().time).stamp(),
        "2026-08-20_09-00-00",
        "newest-first puts the August row last"
    );
}

#[test]
fn an_empty_index_is_reported_as_such_before_any_request_is_spent() {
    let canned = Canned::with_completion("{}");
    let root = test_support::install(
        "search-empty",
        &json!({"open_ai_base_url": canned.base_url(), "open_ai_api_key": "sk-somebody-elses-key"}),
    );
    // No month file at all: an install that has been recorded on but never indexed.
    let _ = std::fs::remove_dir_all(root.join("userdata/db"));
    let mut index = Index::open(&root).expect("an index with no files is still an index");
    assert!(index.is_empty());
    let client = Client::new(index.settings.clone());
    let error = run(&mut index, &client, "anything", 5).expect_err("nothing is recorded");
    assert_eq!(error.kind(), crate::error::ErrorKind::Store, "{error}");
    assert_eq!(canned.request_count(), 0, "an empty library must not cost an API call");
}

#[test]
fn an_unconfigured_key_stops_before_the_socket_and_says_which_key_to_set() {
    let canned = Canned::with_completion("{}");
    let root = test_support::install(
        "search-nokey",
        &json!({"open_ai_base_url": canned.base_url(), "open_ai_api_key": crate::settings::KEY_PLACEHOLDER}),
    );
    let db = test_support::db_dir(&root);
    let owned: Vec<(String, String, Option<String>)> = ROWS
        .iter()
        .map(|(s, t, w)| (s.to_string(), t.to_string(), Some(w.to_string())))
        .collect();
    test_support::fixture_month(&db, "default", 2026, 9, &owned);
    let mut index = Index::open(&root).unwrap();
    let client = Client::new(index.settings.clone());
    let error = run(&mut index, &client, "续约", 5).expect_err("a placeholder is not a credential");
    assert_eq!(error.kind(), crate::error::ErrorKind::Unconfigured, "{error}");
    assert!(error.to_string().contains("open_ai_api_key"), "{error}");
    assert_eq!(canned.request_count(), 0);
}
