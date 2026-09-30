//! Tests for the monthly tag feature, including the content-keyed cache.
//!
//! Every fixture month is written through `wind_store::write::Store::open_month`, and every request is
//! answered by the loopback listener in `crate::test_support::Canned`, so the whole path — index read,
//! title table, CSV, prompt, socket, parse, cache write — runs with no network and no key.
//!
//! One thing the fixtures have to respect, and it is worth knowing because it changes what the model is
//! shown: `aggregate::title_totals` charges time to a *run* of the same title, so a title that appears
//! on exactly one row contributes zero seconds and is dropped by upstream's one-second floor. Upstream's
//! own per-row accumulation would have given it the gap to the next row instead. Both groups of titles
//! below therefore get at least two rows, and `zero_second_titles_are_not_shown` pins the difference
//! deliberately.

use super::*;
use crate::settings::Settings;
use crate::test_support::{self, Canned};
use serde_json::json;
use std::path::{Path, PathBuf};

/// One title held for `rows` consecutive captures, 60 s apart — inside the 100-second session clip, so
/// the run stays one interval of `(rows - 1) * 60` seconds.
fn run_of(title: &str, from_index: usize, rows: usize) -> Vec<(String, String, Option<String>)> {
    (0..rows)
        .map(|i| {
            let total = (from_index + i) * 60;
            (
                format!("2026-09-10_{:02}-{:02}-00", 9 + total / 3600, (total % 3600) / 60),
                String::new(),
                Some(title.to_string()),
            )
        })
        .collect()
}

/// The month every table test uses: reading (300 s), a Chrome search (180 s), a spreadsheet (120 s),
/// and a password manager (60 s) that `exclude_words` must remove before the model ever sees it.
fn busy_month() -> Vec<(String, String, Option<String>)> {
    let mut rows = run_of("Obsidian - 读书笔记", 0, 6);
    rows.extend(run_of("Chrome - 季度营收 预测 - Google Chrome", 6, 4));
    rows.extend(run_of("Excel - budget_2026.xlsx", 10, 3));
    rows.extend(run_of("KeePass", 13, 2));
    rows
}

fn install(tag: &str, config: serde_json::Value) -> PathBuf {
    let root = test_support::install(&format!("tags-{tag}"), &config);
    test_support::fixture_month(&test_support::db_dir(&root), "default", 2026, 9, &busy_month());
    root
}

fn empty_install(tag: &str, config: serde_json::Value) -> PathBuf {
    test_support::install(&format!("tags-{tag}"), &config)
}

/// An install with a given month file, so a cross-month fingerprint can be built.
fn install_month(tag: &str, rows: &[(String, String, Option<String>)], year: i64, month: u32) -> PathBuf {
    let root = test_support::install(tag, &json!({}));
    test_support::fixture_month(&test_support::db_dir(&root), "default", year, month, rows);
    root
}

fn index(root: &Path) -> Index {
    Index::open(root).expect("the fixture install opens")
}

/// A live month plus a client pointed at a canned server.
struct Live {
    root: PathBuf,
    server: Canned,
}

impl Live {
    fn new(tag: &str, answers: Vec<(u16, String)>) -> Live {
        let server = Canned::start(answers);
        let root = test_support::install(
            &format!("tags-live-{tag}"),
            &json!({"open_ai_base_url": server.base_url(), "open_ai_api_key": "sk-somebody-elses-key"}),
        );
        test_support::fixture_month(&test_support::db_dir(&root), "default", 2026, 9, &busy_month());
        Live { root, server }
    }
}

#[test]
fn the_table_is_csv_with_upstreams_column_names_and_longest_focus_first() {
    let root = install("table", json!({}));
    let fixture = index(&root);
    let tags = Tags::new(&fixture);
    let table = tags.title_table(2026, 9).unwrap();

    assert_eq!(table.titles.len(), 3, "{:?}", table.titles);
    assert_eq!(table.titles[0], "Obsidian - 读书笔记", "the most focused window leads");
    assert_eq!(table.csv.lines().count(), 3);
    assert!(table.csv.starts_with("Obsidian - 读书笔记,"), "{}", table.csv);
    assert_eq!(table.excluded, 1, "KeePass must be dropped, not merely sent and hoped away");
    assert!(!table.csv.contains("KeePass"), "{}", table.csv);
    assert_eq!(table.total_seconds, 300 + 180 + 120);

    // The prompt adds the header, so the file and the payload cannot disagree about where it is.
    let prompt = prompt::tags_user(&fixture.settings.prompts.tags_user, &table.csv);
    assert!(prompt.starts_with("content_page_name,screen_time\n"), "{prompt}");
    assert!(prompt.contains("Chrome - 季度营收 预测 - Google Chrome"));
}

#[test]
fn durations_are_written_the_way_the_prompt_is_worded() {
    assert_eq!(compact_duration(3), "3s");
    assert_eq!(compact_duration(63), "1m3s");
    assert_eq!(compact_duration(3723), "1h2m3s");
    assert_eq!(compact_duration(3600), "1h0m0s", "upstream keeps the minutes term once hours appear");
    assert_eq!(compact_duration(0), "0s");
    assert_eq!(compact_duration(-5), "0s");

    let root = install("durations", json!({}));
    let table = Tags::new(&index(&root)).title_table(2026, 9).unwrap();
    assert!(table.csv.contains(",5m0s"), "300 s is 5m0s upstream's format: {}", table.csv);
}

#[test]
fn a_title_that_only_flashed_past_is_not_counted_as_activity() {
    // One row of "Word" between two long runs: `title_totals` charges a run, so a single capture of a
    // title contributes nothing. Upstream's per-row accumulation would have given it 60 seconds.
    let mut rows = run_of("Obsidian - 读书笔记", 0, 6);
    rows.extend(run_of("Word - 一次性弹窗", 6, 1));
    rows.extend(run_of("Excel - budget_2026.xlsx", 7, 3));
    let root = install_month("tags-flash", &rows, 2026, 9);
    let table = Tags::new(&index(&root)).title_table(2026, 9).unwrap();
    assert!(!table.titles.iter().any(|t| t.contains("一次性")), "{:?}", table.titles);
}

#[test]
fn the_month_row_cap_is_double_the_day_cap() {
    let root = install("cap2", json!({"ai_extract_tag_wintitle_limit": 2}));
    let table = Tags::new(&index(&root)).title_table(2026, 9).unwrap();
    // 2 * 2 = 4, and only three titles survive the one-second floor, so nothing is cut.
    assert_eq!(table.titles.len(), 3, "{:?}", table.titles);

    let tight = install("cap1", json!({"ai_extract_tag_wintitle_limit": 1}));
    let table = Tags::new(&index(&tight)).title_table(2026, 9).unwrap();
    assert_eq!(table.titles, vec!["Obsidian - 读书笔记", "Chrome - 季度营收 预测 - Google Chrome"]);
}

#[test]
fn a_zero_limit_is_a_refusal_and_not_an_infinite_table() {
    let root = install("cap0", json!({"ai_extract_tag_wintitle_limit": 0}));
    // `max(1)` in `title_table`: a configuration that would send an empty table is better reported than
    // silently widened, and better widened than sent empty.
    let table = Tags::new(&index(&root)).title_table(2026, 9).unwrap();
    assert_eq!(table.titles.len(), 1, "{:?}", table.titles);
}

/// Cache-key stability, which is the whole reason the fingerprint exists.
#[test]
fn the_fingerprint_ignores_row_order_and_breaks_on_content() {
    let first = Tags::new(&index(&install("hash-a", json!({})))).title_table(2026, 9).unwrap();
    let again = Tags::new(&index(&install("hash-a", json!({})))).title_table(2026, 9).unwrap();
    assert_eq!(first.fingerprint, again.fingerprint, "the same titles must hash the same");
    assert_eq!(first.fingerprint.len(), 16);

    // The same three titles, in a different order of focus. A re-index or a tie in the durations must
    // not cost a second API call, because the *set* the model was shown has not changed. The row order
    // cannot change the set at all: `Index::month_rows` sorts by (time, rowid) before `title_totals` ever
    // sees the rows, so a reversed fixture is the same month — asserting inequality here asserted that
    // the cache key was order-*sensitive*, which is the one thing this fingerprint promises not to be.
    let mut reordered = busy_month();
    reordered.reverse();
    let root = install_month("tags-reordered", &reordered, 2026, 9);
    let reordered = Tags::new(&index(&root)).title_table(2026, 9).unwrap();
    assert_eq!(
        first.fingerprint, reordered.fingerprint,
        "the rows arrived in the opposite order, which is exactly what the set hash must absorb"
    );
    assert_eq!(first.titles, reordered.titles, "same titles, same order of focus");

    // The other half of the name: a title that was not in the month before *must* move the fingerprint,
    // or the equality above is just a constant.
    let mut wider = busy_month();
    wider.extend(run_of("Blender - 建模练习", 15, 3));
    let wider = Tags::new(&index(&install_month("tags-wider", &wider, 2026, 9))).title_table(2026, 9).unwrap();
    assert_ne!(first.fingerprint, wider.fingerprint, "a new title is new information");

    let mut extra = busy_month();
    extra.extend(run_of("Obsidian - 读书笔记", 15, 2));
    let grown = Tags::new(&index(&install_month("tags-grown", &extra, 2026, 9)))
        .title_table(2026, 9)
        .unwrap();
    assert_eq!(
        first.fingerprint, grown.fingerprint,
        "more seconds of the same titles is the same set, which is the point of hashing the set"
    );

    // The same titles in a different month. The stamps have to move with the file: a month file's *name*
    // is its calendar span (`read::Month::coverage`), and `Index::month_rows` clips a read to that span,
    // so September rows written into `default_2026-10_wind.db` are not an October month — they are an
    // empty one, and the assertion below would be comparing a fingerprint against a failure.
    let october_rows: Vec<(String, String, Option<String>)> = busy_month()
        .into_iter()
        .map(|(stamp, text, title)| (stamp.replacen("2026-09", "2026-10", 1), text, title))
        .collect();
    let other_month = install_month("tags-october", &october_rows, 2026, 10);
    let october = Tags::new(&index(&other_month)).title_table(2026, 10).unwrap();
    assert_eq!(first.fingerprint, october.fingerprint, "the fingerprint is about the titles, not the date");
}

#[test]
fn filter_words_are_cut_from_the_field_not_from_the_assembled_csv() {
    let root = empty_install("filter", json!({"ai_extract_tag_filter_words": ["季度营收", ""]}));
    let rows = [
        run_of("Chrome - 季度营收 - Google", 0, 3).as_slice(),
        run_of("Excel - ok", 3, 2).as_slice(),
    ]
    .concat();
    test_support::fixture_month(&test_support::db_dir(&root), "default", 2026, 9, &rows);

    let table = Tags::new(&index(&root)).title_table(2026, 9).unwrap();
    assert!(!table.csv.contains("季度营收"), "{}", table.csv);
    assert!(table.csv.contains("Chrome -  - Google"), "{}", table.csv);
    assert_eq!(table.titles.len(), 2, "filtering changes the payload, not which titles qualify");
}

#[test]
fn a_title_that_is_nothing_but_a_filter_word_says_so_instead_of_vanishing() {
    let root = empty_install("filter-all", json!({"ai_extract_tag_filter_words": ["秘密"]}));
    let rows = run_of("秘密文件 - Notepad", 0, 3);
    test_support::fixture_month(&test_support::db_dir(&root), "default", 2026, 9, &rows);
    let table = Tags::new(&index(&root)).title_table(2026, 9).unwrap();
    // " - Notepad" survives, so the row is not empty; this checks the empty-collapse branch instead.
    assert!(table.csv.contains("Notepad"), "{}", table.csv);

    let gone = empty_install("filter-gone", json!({"ai_extract_tag_filter_words": ["秘密文件 - Notepad"]}));
    let rows = run_of("秘密文件 - Notepad", 0, 3);
    test_support::fixture_month(&test_support::db_dir(&gone), "default", 2026, 9, &rows);
    let table = Tags::new(&index(&gone)).title_table(2026, 9).unwrap();
    assert!(table.csv.contains("‹redacted›"), "{}", table.csv);
}

#[test]
fn tag_parsing_trims_dedupes_and_respects_the_configured_cap() {
    assert_eq!(parse_tags("读书, 预算 , 读书 ,", 10), vec!["读书", "预算"]);
    assert_eq!(parse_tags("a, b, c, d", 2), vec!["a", "b"]);
    assert_eq!(parse_tags("no tags here", 5), vec!["no tags here"]);
    assert!(parse_tags(" , , . . ! ", 5).is_empty(), "punctuation runs are not tags");
    // Upstream deletes newlines, which welds two lines into one tag. A space keeps them separate words
    // of one tag rather than silently inventing a compound.
    assert_eq!(parse_tags("one\ntwo,three\r\n", 5), vec!["one two", "three"]);
    assert_eq!(parse_tags("", 5), Vec::<String>::new());
    assert_eq!(parse_tags("  ", 5), Vec::<String>::new());
}

#[test]
fn the_month_tag_allowance_follows_upstreams_truncation() {
    assert_eq!(month_tag_limit(15), 22, "int(15 * 1.5), as upstream truncates it");
    assert_eq!(month_tag_limit(4), 6);
    assert_eq!(month_tag_limit(1), 1);
    assert_eq!(month_tag_limit(0), 0);
    assert_eq!(parse_tags("a,b,c", month_tag_limit(2)), vec!["a", "b", "c"], "2 * 1.5 = 3");
}

#[test]
fn an_empty_month_is_reported_rather_than_sent_as_an_empty_table() {
    let root = empty_install("empty-month", json!({}));
    let error = Tags::new(&index(&root)).title_table(2026, 9).expect_err("no rows");
    assert_eq!(error.kind(), crate::error::ErrorKind::Store);
    assert!(error.to_string().contains("2026-09"), "{error}");
}

#[test]
fn a_month_with_rows_but_no_attributable_focus_says_so_and_does_not_blame_the_settings() {
    // Six single-capture titles, none of which forms a run: not an `exclude_words` problem.
    let rows: Vec<(String, String, Option<String>)> = (0..6)
        .map(|i| {
            (
                format!("2026-09-11_09-{:02}-00", i * 7),
                String::new(),
                Some(format!("only once {i}")),
            )
        })
        .collect();
    let root = install_month("tags-no-focus", &rows, 2026, 9);
    let error = Tags::new(&index(&root)).title_table(2026, 9).expect_err("nothing held focus");
    assert_eq!(error.kind(), crate::error::ErrorKind::Store, "{error}");
    assert!(error.to_string().contains("held focus"), "{error}");
    assert!(!error.to_string().contains("exclude_words"), "{error}");
}

#[test]
fn a_month_entirely_swallowed_by_exclude_words_blames_the_list_that_did_it() {
    // `exclude_words` in a `config_user.json` *replaces* the shipped list rather than adding to it, so
    // naming three titles leaves `install()`'s KeePass row untouched and still taggable — a month with
    // something left to tag is not a month that was swallowed. Drop that row: the three titles here are
    // the three the list removes, which is also the count the message has to carry.
    let mut rows = busy_month();
    rows.retain(|(_, _, title)| title.as_deref() != Some("KeePass"));
    let root = test_support::install(
        "tags-all-excluded",
        &json!({"exclude_words": ["Obsidian", "Chrome", "Excel"]}),
    );
    test_support::fixture_month(&test_support::db_dir(&root), "default", 2026, 9, &rows);
    let error = Tags::new(&index(&root)).title_table(2026, 9).expect_err("everything excluded");
    assert_eq!(error.kind(), crate::error::ErrorKind::Disabled, "{error}");
    assert!(error.to_string().contains("exclude_words"), "{error}");
    assert!(error.to_string().contains("3"), "the count removed is the useful part: {error}");
    assert!(!error.to_string().contains("held focus"), "{error}");
}

#[test]
fn titles_are_trimmed_and_a_blank_title_is_not_a_window() {
    let rows = vec![
        run_of("  padded title  ", 0, 3),
        vec![("2026-09-11_09-30-00".to_string(), "text".to_string(), Some("   ".to_string()))],
        vec![("2026-09-11_09-31-00".to_string(), "text".to_string(), None)],
        vec![("2026-09-11_09-32-00".to_string(), "text".to_string(), Some("  ".to_string()))],
    ]
    .concat();
    let root = install_month("tags-trim", &rows, 2026, 9);
    let table = Tags::new(&index(&root)).title_table(2026, 9).unwrap();
    assert_eq!(table.titles.len(), 1, "{:?}", table.titles);
    assert!(!table.csv.contains("  padded"), "{:?}", table.csv);
}

#[test]
fn the_cache_round_trips_in_upstreams_file_and_key_shape() {
    let root = install("cache", json!({}));
    let fixture = index(&root);
    let tags = Tags::new(&fixture);
    assert_eq!(Tags::month_key(2026, 9), "2026-09");
    assert_eq!(Tags::month_key(2026, 12), "2026-12");
    assert_eq!(Tags::month_key(2026, 1), "2026-01");
    assert!(tags.cache_path(2026).ends_with(Path::new("result_ai_extract_tag/2026.json")));
    assert!(tags.hash_path(2026).ends_with(Path::new("result_ai_extract_tag/2026.hash.json")));
    assert!(tags.cached(2026, 9).unwrap().is_none(), "nothing written yet");

    write_map_entry(&tags.cache_path(2026), "2026-09", json!(["读书", "预算"])).unwrap();
    write_map_entry(&tags.hash_path(2026), "2026-09", json!("deadbeefdeadbeef")).unwrap();
    // A sibling month written by hand (or by the Streamlit page) must survive our rewrite.
    write_map_entry(&tags.cache_path(2026), "2026-01", json!(["旧标签"])).unwrap();

    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tags.cache_path(2026)).unwrap()).unwrap();
    assert_eq!(on_disk["2026-09"], json!(["读书", "预算"]), "a list of strings, as Python writes it");
    assert_eq!(on_disk["2026-01"], json!(["旧标签"]), "the write is a merge, not a replacement");
    let (list, hash) = tags.cached(2026, 9).unwrap().expect("now cached");
    assert_eq!(list, vec!["读书", "预算"]);
    assert_eq!(hash, "deadbeefdeadbeef");

    // Keys stay sorted, so a byte comparison against a Python-written file is meaningful.
    let text = std::fs::read_to_string(tags.cache_path(2026)).unwrap();
    assert!(text.find("2026-01").unwrap() < text.find("2026-09").unwrap(), "{text}");
    assert!(text.contains("\n  \""), "two-space indent, as `to_string_pretty` writes it");
}

/// The upstream trap this design avoids: retry markers and any other metadata written into the tag array
/// are rendered as tags by the Streamlit page, because it maps every element to a `<span>`.
#[test]
fn a_legacy_entry_with_no_fingerprint_is_a_miss_not_a_hit() {
    let root = install("legacy", json!({}));
    let fixture = index(&root);
    let tags = Tags::new(&fixture);
    write_map_entry(&tags.cache_path(2026), "2026-09", json!(["retry_times:2"])).unwrap();
    assert!(tags.cached(2026, 9).unwrap().is_none(), "no hash file means the content is unknown");
    // Even a well-formed list is a miss without a fingerprint to compare.
    write_map_entry(&tags.cache_path(2026), "2026-03", json!(["读书"])).unwrap();
    assert!(tags.cached(2026, 3).unwrap().is_none());
}

#[test]
fn a_corrupt_cache_file_is_named_and_not_silently_replaced() {
    let root = install("corrupt", json!({}));
    let fixture = index(&root);
    let tags = Tags::new(&fixture);
    std::fs::create_dir_all(tags.cache_path(2026).parent().unwrap()).unwrap();
    std::fs::write(tags.cache_path(2026), b"{ not json").unwrap();
    let error = tags.cached(2026, 9).expect_err("garbage must not become a quiet miss");
    assert_eq!(error.kind(), crate::error::ErrorKind::Io);
    assert!(error.to_string().contains("2026.json"), "{error}");
    // A file that exists but is not an object is the other shape of the same problem.
    std::fs::write(tags.cache_path(2026), b"[1,2]").unwrap();
    assert!(tags.cached(2026, 9).is_err());
}

#[test]
fn whitespace_only_cache_content_is_treated_as_absent_not_as_garbage() {
    let root = install("blank", json!({}));
    let fixture = index(&root);
    let tags = Tags::new(&fixture);
    std::fs::create_dir_all(tags.cache_path(2026).parent().unwrap()).unwrap();
    std::fs::write(tags.cache_path(2026), b"   \n ").unwrap();
    assert!(tags.cached(2026, 9).unwrap().is_none());
}

#[test]
fn an_unchanged_month_is_answered_from_disk_with_no_second_request() {
    let live = Live::new("hit", vec![(200, test_support::completion_body("阅读, 季度营收, 预算"))]);
    let fixture = index(&live.root);
    let client = Client::new(fixture.settings.clone());
    let tags = Tags::new(&fixture);

    let first = tags.run(&client, 2026, 9, false).unwrap();
    assert!(!first.cache_hit);
    assert!(!first.tags.from_cache);
    assert_eq!(live.server.request_count(), 1);
    assert_eq!(first.tags.tags, vec!["阅读", "季度营收", "预算"]);
    assert!(first.written_to.exists());
    assert_eq!(first.tags.usage, Some(Usage { prompt_tokens: 40, completion_tokens: 12, total_tokens: 52 }));

    let second = tags.run(&client, 2026, 9, false).unwrap();
    assert!(second.cache_hit, "the same titles must not cost a second call");
    assert!(second.tags.from_cache);
    assert_eq!(live.server.request_count(), 1, "still exactly one request for two runs");
    assert_eq!(second.tags.tags, first.tags.tags);
    assert_eq!(second.tags.table.fingerprint, first.tags.table.fingerprint);
    assert_eq!(second.written_to, first.written_to);
    assert_eq!(second.tags.usage, None, "a cached answer spent nothing");

    // Grow the month: the title set is the same, so the cache still holds — which is the documented
    // trade of hashing the set rather than the seconds.
    let grown = run_of("Obsidian - 读书笔记", 15, 3);
    let mut rows = busy_month();
    rows.extend(grown);
    let changed = install_month("tags-then-changed", &rows, 2026, 9);
    std::fs::create_dir_all(Tags::new(&index(&changed)).cache_path(2026).parent().unwrap()).unwrap();
    std::fs::copy(tags.cache_path(2026), Tags::new(&index(&changed)).cache_path(2026)).unwrap();
    std::fs::copy(tags.hash_path(2026), Tags::new(&index(&changed)).hash_path(2026)).unwrap();
    let after = Tags::new(&index(&changed)).run(&client, 2026, 9, false).unwrap();
    assert!(after.cache_hit, "the same three titles, so the same answer");
}

#[test]
fn a_changed_title_set_invalidates_the_cache_and_asks_again() {
    let live = Live::new(
        "invalidate",
        vec![
            (200, test_support::completion_body("阅读, 预算")),
            (200, test_support::completion_body("阅读, 预算, 密码管理")),
        ],
    );
    let fixture = index(&live.root);
    let client = Client::new(fixture.settings.clone());
    let tags = Tags::new(&fixture);
    tags.run(&client, 2026, 9, false).unwrap();
    assert_eq!(live.server.request_count(), 1);

    // A new title appears in the month; the fingerprint must move and the cache must not be trusted.
    let mut rows = busy_month();
    rows.extend(run_of("Blender - 建模练习", 15, 3));
    let root2 = install_month("tags-included", &rows, 2026, 9);
    std::fs::create_dir_all(Tags::new(&index(&root2)).cache_path(2026).parent().unwrap()).unwrap();
    std::fs::copy(tags.cache_path(2026), Tags::new(&index(&root2)).cache_path(2026)).unwrap();
    std::fs::copy(tags.hash_path(2026), Tags::new(&index(&root2)).hash_path(2026)).unwrap();
    let second = Tags::new(&index(&root2)).run(&client, 2026, 9, false).unwrap();
    assert!(!second.cache_hit, "a new title is new information");
    assert_eq!(second.tags.tags, vec!["阅读", "预算", "密码管理"]);
}

#[test]
fn dry_run_writes_nothing_and_still_asks_because_that_is_what_shows_the_cost() {
    let live = Live::new("dry", vec![(200, test_support::completion_body("阅读"))]);
    let fixture = index(&live.root);
    let client = Client::new(fixture.settings.clone());
    let tags = Tags::new(&fixture);
    let run = tags.run(&client, 2026, 9, true).unwrap();
    assert!(run.dry_run);
    assert!(!run.written_to.exists(), "--dry-run writes no tags file");
    assert!(!tags.hash_path(2026).exists(), "nor a hash file");
    assert_eq!(live.server.request_count(), 1);
    assert_eq!(run.tags.tags, vec!["阅读"]);
    assert_eq!(run.written_to, tags.cache_path(2026));
}

#[test]
fn the_tag_request_carries_titles_only_and_never_the_body_text() {
    let live = Live::new("egress", vec![(200, test_support::completion_body("预算"))]);
    let root = test_support::install(
        "tags-egress",
        &json!({"open_ai_base_url": live.server.base_url(), "open_ai_api_key": "sk-somebody-elses-key"}),
    );
    let mut rows: Vec<(String, String, Option<String>)> = (0..6)
        .map(|i| {
            (
                format!("2026-09-12_11-{:02}-00", i),
                "SECRET OCR BODY must never be sent".to_string(),
                Some("Excel - budget.xlsx".to_string()),
            )
        })
        .collect();
    rows.extend((0..3).map(|i| {
        (
            format!("2026-09-12_12-{:02}-00", i),
            "另一段绝密的屏幕文字".to_string(),
            Some("Obsidian - 私密日记".to_string()),
        )
    }));
    test_support::fixture_month(&test_support::db_dir(&root), "default", 2026, 9, &rows);

    let fixture = index(&root);
    let client = Client::new(fixture.settings.clone());
    Tags::new(&fixture).run(&client, 2026, 9, false).unwrap();

    let body = live.server.request_json(0);
    let user = body["messages"][1]["content"].as_str().unwrap();
    // What must be there: the two window titles, with their focus durations. What must never be there:
    // the `ocr_text` of the rows those titles belong to. The two are not the same string — asserting the
    // absence of `私密日记` would assert the absence of a *title*, which is exactly what this feature is
    // allowed to send, and the assertion would only be satisfiable by sending nothing at all.
    assert!(user.contains("budget.xlsx"), "{user}");
    assert!(user.contains("Obsidian - 私密日记"), "the title is what is sent: {user}");
    assert!(!user.contains("SECRET OCR BODY"), "titles only, never the captured text: {user}");
    assert!(!user.contains("另一段绝密的屏幕文字"), "titles only, never the captured text: {user}");
    assert_eq!(body["temperature"], TAG_TEMPERATURE);
    assert_eq!(body["messages"][0]["role"], "system");
    assert!(body.get("response_format").is_none(), "the tag answer is a comma line, not JSON");
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains(&format!("at most {}", month_tag_limit(fixture.settings.max_tag_num))), "{system}");
}

/// The tag request is built from the same value the two summary requests are, so one install cannot answer
/// itself in two languages — which is the shape of the "half Chinese half English" complaint: a Chinese
/// day whose paragraph is Chinese and whose tag row is English.
#[test]
fn the_tag_request_asks_for_the_language_the_interface_is_set_to() {
    for (lang, phrase) in [("sc", "Chinese (Simplified Han)"), ("ja", "Japanese"), ("en", "English")] {
        let live = Live::new(&format!("lang-{lang}"), vec![(200, test_support::completion_body("预算"))]);
        let root = test_support::install(
            &format!("tags-lang-{lang}"),
            &json!({
                "open_ai_base_url": live.server.base_url(),
                "open_ai_api_key": "sk-somebody-elses-key",
                "lang": lang,
            }),
        );
        test_support::fixture_month(&test_support::db_dir(&root), "default", 2026, 9, &busy_month());
        let fixture = index(&root);
        assert_eq!(fixture.settings.prompts.language, phrase, "`lang` {lang} must name {phrase:?}");
        let client = Client::new(fixture.settings.clone());
        Tags::new(&fixture).run(&client, 2026, 9, false).unwrap();

        let system = live.server.request_json(0)["messages"][0]["content"].as_str().unwrap().to_string();
        assert!(system.contains(&format!("on one line, in {phrase}, and nothing else")), "{system}");
        assert!(!system.contains("{language}"), "the slot was filled: {system}");
        assert!(!system.contains("in the language of the table"), "no second rule still in force: {system}");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn an_answer_of_nothing_is_a_failure_that_caches_nothing() {
    let live = Live::new("none", vec![(200, test_support::completion_body("  ,  , . "))]);
    let fixture = index(&live.root);
    let client = Client::new(fixture.settings.clone());
    let tags = Tags::new(&fixture);
    let error = tags.run(&client, 2026, 9, false).expect_err("empty tags must not be cached");
    assert_eq!(error.kind(), crate::error::ErrorKind::Model, "{error}");
    assert!(error.to_string().contains("nothing was cached"), "{error}");
    assert!(!tags.cache_path(2026).exists(), "{:?}", tags.cache_path(2026));
    assert!(!tags.hash_path(2026).exists());
}

#[test]
fn a_network_failure_leaves_the_cache_untouched_so_the_next_run_can_retry() {
    let live = Live::new("503", vec![(503, "gateway down".to_string())]);
    let fixture = index(&live.root);
    let client = Client::new(fixture.settings.clone());
    let tags = Tags::new(&fixture);
    let error = tags.run(&client, 2026, 9, false).expect_err("a 503 is not a tag list");
    assert_eq!(error.kind(), crate::error::ErrorKind::HttpStatus);
    assert!(!tags.cache_path(2026).exists(), "a failed month must not be remembered as empty");
    // And the cap on tags is applied to a successful answer, not to the model's good manners.
    let many = (0..40).map(|i| format!("tag{i}")).collect::<Vec<_>>().join(",");
    let live2 = Live::new("cap", vec![(200, test_support::completion_body(&many))]);
    let fixture2 = index(&live2.root);
    let client2 = Client::new(fixture2.settings.clone());
    let run = Tags::new(&fixture2).run(&client2, 2026, 9, false).unwrap();
    assert_eq!(run.tags.tags.len(), month_tag_limit(fixture2.settings.max_tag_num));
}

#[test]
fn the_result_directory_follows_the_config_key_so_the_page_finds_it() {
    let root = empty_install("dir", json!({"ai_extract_tag_result_dir": "my_tags"}));
    let settings = Settings::read(&wind_base::config::Config::load(&root).unwrap());
    assert!(settings.tags_dir.ends_with(Path::new("my_tags")), "{:?}", settings.tags_dir);
    let fixture = index(&root);
    assert!(Tags::new(&fixture).cache_path(2026).ends_with("my_tags/2026.json"));
}
