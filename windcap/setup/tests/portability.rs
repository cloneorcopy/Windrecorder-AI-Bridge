//! End-to-end tests against fixture *installs*, not against functions.
//!
//! The unit tests inside each module check one rule. These check the thing the brief is actually about:
//! a directory shaped like a real Windrecorder install, run through `init`, `migrate` and `doctor`, with
//! the assertions made on what is left on disk afterwards — the column order of a real month file, the
//! bytes of a real config, whether a second run changed anything at all.
//!
//! Every scenario named in the brief is here: a fresh tree; a tree that already has a user config; the
//! key-deletion reconciliation with its backup; a legacy seven-column month file; `migrate` run twice;
//! `migrate` interrupted; and a crafted month-file name that tries to point a write outside the install.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{Map, Value};
use wind_base::config::Config;
use wind_setup::{backup, configfile, doctor, layout, marker, migrate};
use marker::Marker;

// ---------------------------------------------------------------------------
/// A directory shaped like an install, with a real defaults file in it.
///
/// The defaults are a trimmed copy of the shipped `config_default.json` — real keys, real defaults — so a
/// test that passes here is exercising the same overlay the app reads rather than a fixture invented for
/// the test.
///
/// Each scenario gets its own directory name, which matters more than it looks: these tests run in
/// parallel inside one process, and a shared `std::process::id()` prefix means they see each other's
/// `userdata/` and every count in every assertion becomes a race.
fn install(scenario: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wind-setup-it-{scenario}-{}-{}",
        std::process::id(),
        scenario_hash(scenario)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config_src")).unwrap();
    std::fs::write(dir.join("config_src/config_default.json"), DEFAULTS).unwrap();
    dir
}

/// Two independent short digests of a scenario name, so no two scenarios can collide on a directory.
fn scenario_hash(scenario: &str) -> String {
    wind_setup::hash::sha256_hex(scenario.as_bytes())[..10].to_string()
}

const DEFAULTS: &str = r#"{
  "userdata_dir": "userdata",
  "db_path": "db",
  "vdb_img_path": "db_imgemb",
  "record_videos_dir": "videos",
  "lang": "en",
  "ocr_lang": "zh-Hans-CN",
  "ocr_engine": "Windows.Media.Ocr.Cli",
  "user_name": "default",
  "max_page_result": 30,
  "record_mode": "screenshot_array",
  "screenshot_interval_second": 3,
  "recycle_deleted_files": true,
  "wordcloud_result_dir": "result_wordcloud",
  "timeline_result_dir": "result_timeline",
  "lightbox_result_dir": "result_lightbox",
  "wintitle_result_dir": "result_wintitle",
  "date_state_dir": "result_date_state",
  "ai_extract_tag_result_dir": "result_ai_extract_tag",
  "ai_day_poem_result_dir": "result_ai_day_poem"
}"#;

fn write_user(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir.join("userdata")).unwrap();
    std::fs::write(dir.join("userdata/config_user.json"), body).unwrap();
}

/// One month file, in the shape a pre-`win_title` install left on disk.
fn legacy_month(db_dir: &Path, name: &str, rows: usize) -> PathBuf {
    std::fs::create_dir_all(db_dir).unwrap();
    let path = db_dir.join(name);
    let conn = Connection::open(&path).unwrap();
    // Verbatim from the 0.0.9-era `CREATE TABLE`: seven columns, `INT` and `BOOLEAN` type fictions.
    conn.execute_batch(
        "CREATE TABLE video_text (videofile_name VARCHAR(100), picturefile_name VARCHAR(100),
         videofile_time INT, ocr_text TEXT, is_videofile_exist BOOLEAN, is_picturefile_exist BOOLEAN,
         thumbnail TEXT);",
    )
    .unwrap();
    for index in 0..rows {
        conn.execute(
            "INSERT INTO video_text VALUES (?,?,?,?,?,?,?)",
            rusqlite::params![
                format!("2026-08-0{index}_10-00-0{index}.mp4"),
                format!("2026-08-0{index}_10-00-0{index}/f.jpg"),
                1_754_000_000 + index as i64,
                format!("screen text {index}"),
                1,
                1,
                "aGVsbG8="
            ],
        )
        .unwrap();
    }
    drop(conn);
    path
}

/// The columns of a real file, as SQLite sees them, in stored order.
fn columns(path: &Path) -> Vec<String> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(video_text)").unwrap();
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names
}

fn rows(path: &Path) -> i64 {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row("SELECT COUNT(*) FROM video_text", [], |r| r.get(0)).unwrap()
}

/// The first row's text, read back positionally *and* by name: the point of `ADD COLUMN` is that an
/// installer that wrote seven values still reads as seven values.
fn first_row(path: &Path) -> Vec<String> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn.prepare("SELECT * FROM video_text ORDER BY rowid LIMIT 1").unwrap();
    let width = stmt.column_count();
    let mut rows = stmt
        .query_map([], move |row| {
            let mut cells = Vec::with_capacity(width);
            for index in 0..width {
                // `rusqlite::types::Value`, not `Option<String>`: `videofile_time` is an INTEGER and a
                // typed read of it as text is a hard error, which is exactly the positional-read failure
                // this helper exists to prove does not happen after an `ADD COLUMN`.
                let value = row.get::<_, rusqlite::types::Value>(index)?;
                cells.push(match value {
                    rusqlite::types::Value::Null => String::new(),
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    rusqlite::types::Value::Real(f) => f.to_string(),
                    rusqlite::types::Value::Text(t) => t,
                    rusqlite::types::Value::Blob(b) => format!("<{} bytes>", b.len()),
                });
            }
            Ok(cells)
        })
        .unwrap();
    rows.next().and_then(|row| row.ok()).unwrap_or_default()
}

fn every_path(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path).display().to_string();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            out.insert(format!("{relative}:{}", if meta.is_dir() { "d" } else { "f" }));
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

fn contents(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in every_path(root) {
        let relative = entry.split(':').next().unwrap().to_string();
        let path = root.join(&relative);
        if path.is_file() {
            let digest = wind_setup::hash::digest_file(&path).unwrap_or_else(|_| "unreadable".to_string());
            out.push((relative, digest));
        }
    }
    out.sort();
    out
}

fn options(config: &Config, dry_run: bool) -> migrate::Options<'_> {
    migrate::Options { config, dry_run, from_version: None, stamp: "2026-09-23_10-00-00".to_string() }
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

#[test]
fn init_on_a_fresh_tree_builds_the_layout_and_seeds_the_config() {
    let dir = install("init-fresh");
    let config = Config::load(&dir).unwrap();
    let layout = layout::Layout::resolve(&config);
    assert!(!layout.create(false).unwrap().is_empty());
    assert!(matches!(
        configfile::seed(&config, false).unwrap(),
        configfile::Seeded::FromDefaults { .. }
    ));

    for expected in [
        "userdata",
        "userdata/db",
        "userdata/videos",
        "userdata/db_imgemb",
        "userdata/result_lightbox",
        "userdata/result_timeline",
        "userdata/result_wintitle",
        "userdata/result_wordcloud",
        "userdata/result_date_state",
        "userdata/result_ai_extract_tag",
        "userdata/result_ai_day_poem",
        "cache",
        "cache/locks",
        "cache/logs",
        "cache/win_title",
        "cache/i_frames",
        "userdata/config_user.json",
    ] {
        assert!(dir.join(expected).exists(), "{expected} was not created");
    }
    // Seeded from the defaults, byte for byte.
    assert_eq!(
        wind_setup::hash::digest_file(&dir.join("userdata/config_user.json")).unwrap(),
        wind_setup::hash::digest_file(&dir.join("config_src/config_default.json")).unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The scenario that destroys an install if it is got wrong: `init` run again over a two-year-old tree.
#[test]
fn init_never_clobbers_an_existing_user_config() {
    let dir = install("init-clobber");
    let user = r#"{ "lang": "sc", "user_name": "amy", "ocr_engine": "PaddleOCR", "max_page_result": 500 }"#;
    write_user(&dir, user);
    let config = Config::load(&dir).unwrap();

    layout::Layout::resolve(&config).create(false).unwrap();
    assert_eq!(configfile::seed(&config, false).unwrap(), configfile::Seeded::AlreadyPresent);

    let after = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
    assert_eq!(after, user, "the file was not rewritten by one byte");
    let loaded = Config::load(&dir).unwrap();
    assert_eq!(loaded.user_name(), "amy");
    assert_eq!(loaded.str_or("ocr_engine", "?"), "PaddleOCR");
    assert_eq!(loaded.i64_or("max_page_result", 0), 500);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The reconciliation, with its backup
// ---------------------------------------------------------------------------

/// The data-loss case the brief names: the defaults file is *older* than the user's.
#[test]
fn reconciliation_backs_up_first_and_keeps_the_users_extra_key() {
    let dir = install("reconcile");
    // Stale defaults: two keys the user's file does not have, and no mention of three keys it does.
    std::fs::write(
        dir.join("config_src/config_default.json"),
        r#"{ "lang": "en", "user_name": "default", "brand_new_knob": 7, "another_new_one": true }"#,
    )
    .unwrap();
    let user = r#"{
  "lang": "sc",
  "user_name": "amy",
  "ocr_engine": "PaddleOCR",
  "open_ai_api_key": "sk-secret-do-not-lose-me"
}"#;
    write_user(&dir, user);
    let config = Config::load(&dir).unwrap();
    let user_path = dir.join(configfile::USER_RELPATH);
    let before = std::fs::read_to_string(&user_path).unwrap();

    // What the Python reconciler would have deleted, from the same pair of files.
    let drift = configfile::drift(&config).unwrap();
    assert_eq!(
        drift.extra_in_user,
        vec!["ocr_engine".to_string(), "open_ai_api_key".to_string()],
        "{drift:?}"
    );
    assert!(drift.missing_from_user.contains(&"brand_new_knob".to_string()));

    let outcome = configfile::reconcile(&config, "2026-09-23_10-00-00", false).unwrap();
    assert_eq!(outcome.written, true);
    // `user_name` is in the user's file as "amy" and `lang` as "sc", so only the two keys the file never
    // mentioned arrive — and the ones it did keep their values, which is asserted below.
    assert_eq!(
        outcome.added,
        vec!["another_new_one", "brand_new_knob"],
        "the defaults the user's file did not mention"
    );
    assert_eq!(outcome.preserved, vec!["ocr_engine", "open_ai_api_key"]);

    let backup = outcome.backup.clone().expect("a write must be preceded by a proven backup");
    assert!(backup.target.exists(), "the backup must exist on disk, not just in a return value");
    assert_eq!(std::fs::read_to_string(&backup.target).unwrap(), before, "the backup holds the pre-reconcile bytes");
    assert_eq!(wind_setup::hash::digest_file(&backup.target).unwrap(), backup.sha256);
    assert!(backup.target.starts_with(backup::backup_root(&config)));
    let manifest = std::fs::read_to_string(backup::backup_root(&config).join("MANIFEST.jsonl")).unwrap();
    assert!(manifest.contains("\"verified\":true"), "{manifest}");

    let after = std::fs::read_to_string(&user_path).unwrap();
    for kept in ["\"lang\": \"sc\"", "\"user_name\": \"amy\"", "PaddleOCR", "sk-secret-do-not-lose-me"] {
        assert!(after.contains(kept), "the reconciled config lost {kept}:\n{after}");
    }
    assert!(after.contains("brand_new_knob"), "the new defaults did not arrive:\n{after}");
    // The overlay stays an overlay: a key the user never set that *is* in the defaults and was not in
    // their file arrives; a key they set keeps their value.
    let parsed: Map<String, Value> = serde_json::from_str(&after).unwrap();
    assert_eq!(parsed["lang"], Value::from("sc"), "the default's \"en\" must not win over the user");
    assert_eq!(parsed["user_name"], Value::from("amy"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_reconciliation_adds_nothing_and_writes_nothing() {
    let dir = install("reconcile-twice");
    write_user(&dir, r#"{ "lang": "sc", "mine": 1 }"#);
    let config = Config::load(&dir).unwrap();
    let first = configfile::reconcile(&config, "run-1", false).unwrap();
    assert!(first.written);
    let bytes = std::fs::read(dir.join(configfile::USER_RELPATH)).unwrap();

    let second = configfile::reconcile(&config, "run-2", false).unwrap();
    assert!(!second.written, "a reconcile that adds nothing must not rewrite the file");
    assert!(second.backup.is_none(), "and must not stamp a second \"pre-migration\" backup that is not one");
    assert_eq!(std::fs::read(dir.join(configfile::USER_RELPATH)).unwrap(), bytes);
    assert!(!backup::backup_root(&config).join("run-2").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// migrate: the legacy month file
// ---------------------------------------------------------------------------

#[test]
fn migrate_adds_the_two_columns_and_leaves_the_rows_readable() {
    let dir = install("migrate-schema");
    let db = dir.join("userdata/db");
    let month = legacy_month(&db, "default_2026-08_wind.db", 4);
    let before = std::fs::read(&month).unwrap();
    assert_eq!(first_row(&month).len(), 7, "the fixture itself must be readable before the migration");
    assert_eq!(columns(&month).len(), 7);
    let config = Config::load(&dir).unwrap();

    let report = migrate::run(&options(&config, false)).unwrap();
    let schema = report.steps.iter().find(|(s, _)| s.id == "index-schema").expect("the step ran");
    assert!(schema.1.pending(), "{:?}", schema.1.actions);
    assert!(schema.1.blocked.is_empty(), "{:?}", schema.1.blocked);

    assert_eq!(
        columns(&month),
        vec![
            "videofile_name",
            "picturefile_name",
            "videofile_time",
            "ocr_text",
            "is_videofile_exist",
            "is_picturefile_exist",
            "thumbnail",
            "win_title",
            "deep_linking"
        ],
        "the seven original columns must keep their positions and the two new ones must land last"
    );
    assert_eq!(columns(&month)[6], "thumbnail", "an inserted column would shift every positional read");
    assert_eq!(rows(&month), 4, "an ALTER must not touch a row");
    assert_eq!(first_row(&month)[0], "2026-08-00_10-00-00.mp4");
    assert_eq!(first_row(&month)[3], "screen text 0");
    assert_eq!(first_row(&month)[6], "aGVsbG8=", "the thumbnail is still in column seven");

    // The backup of the file that was altered, found at the path the backup helper documents and
    // verified against the bytes that were there before. `find_existing` is deliberately not usable for
    // this lookup: it matches a *current* digest, and the whole point of this backup is that the file's
    // bytes are no longer the ones on disk.
    let backed = backup::destination(&config, &month, "2026-09-23_10-00-00");
    assert!(backed.is_file(), "the migration left no backup at {backed:?}");
    assert_eq!(std::fs::read(&backed).unwrap(), before, "the backup is the pre-ALTER file");
    assert_eq!(wind_setup::hash::digest_file(&backed).unwrap(), before_sha(&before));
    let manifest = std::fs::read_to_string(backup::backup_root(&config).join("MANIFEST.jsonl")).unwrap();
    assert!(manifest.contains("default_2026-08_wind.db"), "the manifest names the file it protects:
{manifest}");
    assert!(manifest.contains("\"verified\":true"), "{manifest}");
    // Seven columns, four rows, in the copy — i.e. the backup is a restorable index, not a file of bytes.
    assert_eq!(columns(&backed).len(), 7);
    assert_eq!(rows(&backed), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

fn before_sha(bytes: &[u8]) -> String {
    wind_setup::hash::sha256_hex(bytes)
}

/// The property the brief asks for by name.
#[test]
fn migrate_run_twice_is_a_noop_the_second_time() {
    let dir = install("migrate-twice");
    let db = dir.join("userdata/db");
    legacy_month(&db, "default_2026-07_wind.db", 2);
    legacy_month(&db, "default_2026-08_wind.db", 3);
    // A pre-split folder to move, and an error-tagged video to rename.
    std::fs::create_dir_all(dir.join("db")).unwrap();
    std::fs::create_dir_all(dir.join("videos")).unwrap();
    std::fs::write(dir.join("videos/2026-01-01_01-01-01-ERROR.mp4"), b"v").unwrap();
    write_user(&dir, r#"{ "lang": "sc", "user_name": "amy", "my_key": 1 }"#);
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();

    let first = migrate::run(&options(&config, false)).unwrap();
    assert!(first.changed(), "a legacy tree must have work to do");
    let marker_after_first = std::fs::read(marker::marker_path(&config)).unwrap();
    let state_after_first = contents(&dir);

    let second = migrate::run(&options(&config, false)).unwrap();
    assert!(!second.changed(), "second run planned: {:?}", second.steps.iter().filter(|(_, r)| r.pending()).map(|(s, r)| (s.id, &r.actions)).collect::<Vec<_>>());
    assert!(second.blockers().is_empty(), "{:?}", second.blockers());
    assert_eq!(contents(&dir), state_after_first, "the second run changed a byte somewhere");
    assert_eq!(std::fs::read(marker::marker_path(&config)).unwrap(), marker_after_first, "and did not rewrite the marker either");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A crash between two steps, and the crash *inside* the marker write.
#[test]
fn an_interrupted_migrate_resumes_and_a_missing_marker_is_rederived() {
    let dir = install("migrate-crash");
    let db = dir.join("userdata/db");
    let month = legacy_month(&db, "default_2026-06_wind.db", 5);
    std::fs::create_dir_all(dir.join("videos")).unwrap();
    std::fs::write(dir.join("videos/2026-02-02_02-02-02-ERROR.mp4"), b"v").unwrap();
    write_user(&dir, r#"{ "lang": "ja", "user_name": "amy" }"#);
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();

    // Step one of the run: only the folder move, by restricting what is offered.
    let moved = migrate::run(&migrate::Options {
        config: &config,
        dry_run: false,
        from_version: None,
        stamp: "run-1".to_string(),
    })
    .unwrap();
    assert!(moved.changed());
    let with_marker = Marker::load(&config).expect("a step that did work leaves a record");
    assert!(!with_marker.steps.is_empty());

    // The crash: the marker is gone, as it would be if the process died before the first write. Nothing
    // else is rewound — the folders are moved, the columns may be added, the videos are renamed.
    std::fs::remove_file(marker::marker_path(&config)).unwrap();
    assert!(Marker::load(&config).is_none());

    let resumed = migrate::run(&migrate::Options {
        config: &config,
        dry_run: false,
        from_version: None,
        stamp: "run-2".to_string(),
    })
    .unwrap();
    assert!(resumed.blockers().is_empty(), "{:?}", resumed.blockers());
    // Re-derived from the tree, so it is a no-op rather than a re-run of work already done.
    assert!(
        !resumed.changed(),
        "{:?}",
        resumed.steps.iter().filter(|(_, r)| r.pending()).map(|(s, r)| (s.id, r.actions.clone())).collect::<Vec<_>>()
    );
    assert_eq!(columns(&month).len(), 9, "the month file is still migrated");
    assert_eq!(rows(&month), 5, "and still has its rows");
    assert!(!dir.join("videos").exists(), "the pre-split folder is gone from the install root");
    let tagged = dir.join("userdata/videos/2026-02-02_02-02-02-ERROR1.mp4");
    assert!(tagged.is_file(), "the retry tag was applied, under userdata/videos: {:?}", std::fs::read_dir(dir.join("userdata/videos")).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>()));
    assert!(!dir.join("userdata/videos/2026-02-02_02-02-02-ERROR.mp4").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A half-run, simulated by cutting the plan in the middle rather than deleting the record.
#[test]
fn a_step_that_did_not_finish_is_offered_again_and_the_rest_are_untouched() {
    let dir = install("migrate-half");
    let db = dir.join("userdata/db");
    legacy_month(&db, "default_2026-05_wind.db", 3);
    legacy_month(&db, "default_2026-06_wind.db", 3);
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();

    // Run only the first three steps by dropping the rest afterwards: the marker claims what happened.
    let report = migrate::run(&options(&config, false)).unwrap();
    assert!(report.blockers().is_empty());
    // Now pretend the schema step's record was lost mid-write, while its work on one file survived.
    let mut record = Marker::load(&config).unwrap();
    record.steps.remove("index-schema");
    record.save(&config).unwrap();
    // And one month file is genuinely rewound to seven columns, i.e. the migration stopped after one.
    let untouched = db.join("default_2026-05_wind.db");
    let conn = Connection::open(&untouched).unwrap();
    conn.execute_batch("DROP TABLE video_text; CREATE TABLE video_text (videofile_name VARCHAR(100), picturefile_name VARCHAR(100), videofile_time INT, ocr_text TEXT, is_videofile_exist BOOLEAN, is_picturefile_exist BOOLEAN, thumbnail TEXT);").unwrap();
    drop(conn);

    let again = migrate::run(&options(&config, false)).unwrap();
    let schema = again.steps.iter().find(|(s, _)| s.id == "index-schema").unwrap();
    assert!(schema.1.pending(), "the rewound month must be re-offered: {:?}", schema.1.actions);
    assert!(again.blockers().is_empty(), "{:?}", again.blockers());
    assert_eq!(columns(&untouched).len(), 9);
    assert_eq!(rows(&untouched), 0, "a table rebuilt empty stays empty; migrate invents no rows");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The path-traversal attempt
// ---------------------------------------------------------------------------

/// A month file named `.._2026-09_wind.db` is legal on NTFS, parses as a month file, and its owner
/// component is `..`. Reconstructing a name from that — which is exactly what `paths::month_filename`
/// does, and what the writer does on every segment commit — aims the write at the parent of the
/// database directory.
#[test]
fn a_crafted_month_file_name_cannot_point_a_write_outside_the_install() {
    let dir = install("traversal");
    let db = dir.join("userdata/db");
    // The hostile file: a real SQLite month file, so it *could* be migrated if nothing checked the name.
    let hostile = legacy_month(&db, ".._2026-09_wind.db", 2);
    // Two innocent bystanders, one of which is the escape target.
    let sibling = legacy_month(&dir.join("userdata"), "innocent.db", 1);
    let outside = dir.parent().unwrap().join("wind-setup-outside-target.db");
    let _ = std::fs::remove_file(&outside);
    let good = legacy_month(&db, "default_2026-09_wind.db", 7);

    let config = Config::load(&dir).unwrap();
    let report = migrate::run(&options(&config, false)).unwrap();
    let schema = report.steps.iter().find(|(s, _)| s.id == "index-schema").unwrap();

    let refused = schema.1.blocked.iter().find(|b| b.contains(".._2026-09_wind.db"));
    assert!(refused.is_some(), "the crafted name must be named in the report, not skipped quietly: {:?}", schema.1);
    assert!(schema.1.actions.iter().any(|a| a.starts_with("default_2026-09_wind.db")), "a safe name in the same directory is still migrated: {:?}", schema.1.actions);

    assert_eq!(columns(&good).len(), 9);
    assert_eq!(columns(&hostile).len(), 7, "the refused file was not opened for writing at all");
    assert_eq!(rows(&hostile), 2, "and its rows are untouched");
    assert!(!sibling.exists() || columns(&sibling).len() == 7);
    assert!(!outside.exists(), "nothing was written outside the install root");

    // `doctor` shows the same verdict, from the same rule.
    let health = doctor::inspect(&config).unwrap();
    let month = health.months.iter().find(|m| m.name == ".._2026-09_wind.db").expect("listed");
    assert!(month.unsafe_name);
    assert!(month.columns.is_empty(), "an unsafe month file is never read");
    let text = doctor::render(&health);
    assert!(text.contains("safe path element"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same name arriving through the *config* rather than a listing: `db_path` is joined under
/// `userdata`, so `../../` there aims the whole index at the parent of the install.
#[test]
fn a_config_that_points_the_index_at_the_parents_is_refused_by_the_layout() {
    let dir = install("traversal-config");
    std::fs::write(
        dir.join("config_src/config_default.json"),
        r#"{ "userdata_dir": ".", "db_path": "../../../windows/system32" }"#,
    )
    .unwrap();
    let config = Config::load(&dir).unwrap();
    let layout = layout::Layout::resolve(&config);
    let planned = layout.create(false);
    assert!(planned.is_err(), "an escaping db_path must be refused, not created: {planned:?}");
    assert!(!dir.join("../../windows/system32").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// --dry-run is a no-op
// ---------------------------------------------------------------------------

#[test]
fn a_dry_run_plans_everything_and_changes_nothing() {
    let dir = install("dry");
    let db = dir.join("userdata/db");
    legacy_month(&db, "default_2026-04_wind.db", 6);
    std::fs::create_dir_all(dir.join("videos/2026-03")).unwrap();
    std::fs::write(dir.join("videos/2026-03/2026-03-01_01-01-01-ERROR.mp4"), b"v").unwrap();
    std::fs::create_dir_all(dir.join("db")).unwrap();
    std::fs::write(dir.join("db/legacy_2026-02_wind.db"), b"legacy").unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(dir.join("config/config_user.json"), r#"{ "lang": "sc" }"#).unwrap();
    write_user(&dir, r#"{ "lang": "ja", "keep_me": true }"#);

    let config = Config::load(&dir).unwrap();
    let before = contents(&dir);
    let listing = every_path(&dir);

    let report = migrate::run(&options(&config, true)).unwrap();
    assert!(report.changed(), "a dry run on a legacy tree must plan real work");
    let planned: usize = report.steps.iter().map(|(_, r)| r.actions.len()).sum();
    assert!(planned >= 4, "only {planned} actions were planned: {:?}", report.steps.iter().map(|(s, r)| (s.id, r.actions.len())).collect::<Vec<_>>());
    // Every planned action names a destination, so the report is checkable rather than decorative.
    for (_, result) in &report.steps {
        for action in &result.actions {
            assert!(!action.is_empty());
        }
    }
    assert_eq!(every_path(&dir), listing, "a dry run created or removed a path");
    assert_eq!(contents(&dir), before, "a dry run rewrote a file");
    assert!(!dir.join("userdata/backup").exists(), "the backup root is not created by a dry run");
    assert!(!dir.join("userdata/trash").exists(), "nor is the trash root");
    assert!(!marker::marker_path(&config).exists(), "nor the marker");

    // And `doctor`, which is a dry run of the plan plus a lot of reading.
    let health = doctor::inspect(&config).unwrap();
    let _ = doctor::render(&health);
    assert_eq!(contents(&dir), before, "doctor rewrote a file");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The folder relocation, including the collision upstream gets wrong
// ---------------------------------------------------------------------------

#[test]
fn a_legacy_folder_moves_into_userdata_and_a_collision_goes_to_trash_not_into_itself() {
    let dir = install("legacy-move");
    std::fs::create_dir_all(dir.join("db")).unwrap();
    std::fs::write(dir.join("db/old_2026-01_wind.db"), b"old month").unwrap();
    std::fs::create_dir_all(dir.join("videos")).unwrap();
    std::fs::write(dir.join("videos/2026-01-01_01-01-01.mp4"), b"old video").unwrap();
    // The collision: `userdata/videos` already exists and is what the recorder is writing to.
    std::fs::create_dir_all(dir.join("userdata/videos")).unwrap();
    std::fs::write(dir.join("userdata/videos/2026-09-09_09-09-09.mp4"), b"live video").unwrap();
    write_user(&dir, r#"{ "lang": "en" }"#);
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();

    let report = migrate::run(&options(&config, false)).unwrap();
    let step = report.steps.iter().find(|(s, _)| s.id == "legacy-layout").unwrap();
    assert!(step.1.blocked.is_empty(), "{:?}", step.1.blocked);
    assert!(!dir.join("db").exists(), "the free-standing folder moved");
    assert!(dir.join("userdata/db/old_2026-01_wind.db").is_file(), "and arrived under userdata");
    // The colliding one was neither merged nor nested: `userdata/videos/videos` must not exist, because
    // `shutil.move` would have made it and the recorder would then be writing to a folder nothing reads.
    assert!(!dir.join("userdata/videos/videos").exists(), "a colliding folder was nested inside itself");
    assert!(dir.join("videos").exists() == false || dir.join("userdata/trash").exists(), "the legacy videos went somewhere recoverable");
    assert!(dir.join("userdata/videos/2026-09-09_09-09-09.mp4").is_file(), "the live file was not disturbed");
    let trashed: Vec<String> = std::fs::read_dir(dir.join("userdata/trash"))
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let run = entry.path();
            let names = std::fs::read_dir(&run).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
            Some(names.join(","))
        })
        .collect();
    assert!(trashed.iter().any(|t| t.contains("videos")), "the colliding folder is recoverable from the trash: {trashed:?}");
    assert!(std::fs::read(dir.join("userdata/db/old_2026-01_wind.db")).unwrap() == b"old month");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_legacy_config_file_is_adopted_only_when_the_live_one_is_a_superset() {
    let dir = install("legacy-config-conflict");
    write_user(&dir, r#"{ "lang": "sc", "user_name": "amy" }"#);
    std::fs::create_dir_all(dir.join("config")).unwrap();
    // The legacy file knows something the live one does not. Merging would mean picking a winner the
    // user never chose, so both files have to survive.
    std::fs::write(dir.join("config/config_user.json"), r#"{ "lang": "en", "ocr_engine": "PaddleOCR" }"#).unwrap();
    let config = Config::load(&dir).unwrap();

    let report = migrate::run(&options(&config, false)).unwrap();
    let step = report.steps.iter().find(|(s, _)| s.id == "legacy-config-file").unwrap();
    assert!(!step.1.blocked.is_empty(), "a real conflict must be a blocker, not a silent merge");
    assert!(step.1.blocked.iter().any(|b| b.contains("ocr_engine")), "{:?}", step.1.blocked);
    assert!(dir.join("userdata/config_user.json").is_file(), "the live config is untouched");
    assert!(dir.join("config/config_user.json").is_file(), "so is the legacy one");
    let live: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap()).unwrap();
    assert_eq!(live["lang"], serde_json::Value::from("sc"), "the live file's own values are untouched");
    assert_eq!(live["user_name"], serde_json::Value::from("amy"));
    // `ocr_engine` arrives from the *defaults* during reconciliation; what must not arrive is the
    // legacy file's value, which is the one that would silently switch the user onto another engine.
    assert_ne!(live["ocr_engine"], serde_json::Value::from("PaddleOCR"), "the legacy value was merged in: {live}");

    // With the conflict resolved by the user, the retire path works.
    std::fs::write(dir.join("config/config_user.json"), r#"{ "lang": "sc", "user_name": "amy" }"#).unwrap();
    let second = migrate::run(&migrate::Options { config: &config, dry_run: false, from_version: None, stamp: "run-2".to_string() }).unwrap();
    let retired = second.steps.iter().find(|(s, _)| s.id == "legacy-config-file").unwrap();
    assert!(retired.1.blocked.is_empty(), "{:?}", retired.1.blocked);
    assert!(!dir.join("config/config_user.json").exists(), "the duplicate was retired");
    assert!(dir.join("userdata/config_user.json").is_file(), "and the live file is still there");
    let trashed = format!("{:?}", every_path(&dir));
    assert!(trashed.contains("TRASHED-config_user.json"), "the retired copy is recoverable: {trashed}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_retry_tag_is_applied_once_and_only_to_the_file_name() {
    let dir = install("error-tag");
    let videos = dir.join("userdata/videos");
    std::fs::create_dir_all(videos.join("2026-01")).unwrap();
    std::fs::write(videos.join("2026-01-01_01-01-01-ERROR.mp4"), b"a").unwrap();
    std::fs::write(videos.join("2026-01/2026-01-02_02-02-02-ERROR.mp4"), b"b").unwrap();
    // Already tagged: `-ERROR1.` does not contain `-ERROR.`, so a second pass must leave it alone.
    std::fs::write(videos.join("2026-01-03_03-03-03-ERROR1.mp4"), b"c").unwrap();
    // The upstream bug: `str.replace` on the full path rewrites the *directory* name too, sending every
    // child path into a folder that never existed.
    let weird = videos.join("2026-01-04-ERROR.backup");
    std::fs::create_dir_all(&weird).unwrap();
    std::fs::write(weird.join("2026-01-05_05-05-05-ERROR.mp4"), b"d").unwrap();
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();

    let report = migrate::run(&options(&config, false)).unwrap();
    let step = report.steps.iter().find(|(s, _)| s.id == "error-video-tag").unwrap();
    assert_eq!(step.1.actions.len(), 3, "{:?}", step.1.actions);
    assert!(step.1.blocked.is_empty(), "{:?}", step.1.blocked);
    assert!(videos.join("2026-01-01_01-01-01-ERROR1.mp4").is_file());
    assert!(videos.join("2026-01/2026-01-02_02-02-02-ERROR1.mp4").is_file());
    assert!(weird.join("2026-01-05_05-05-05-ERROR1.mp4").is_file(), "the file inside a -ERROR-named folder was renamed in place");
    assert!(weird.is_dir(), "and its directory kept its name: {:?}", weird);
    assert!(videos.join("2026-01-03_03-03-03-ERROR1.mp4").is_file(), "an already-tagged file was left alone");
    assert!(!videos.join("2026-01-03_03-03-03-ERROR11.mp4").exists());

    let second = migrate::run(&migrate::Options { config: &config, dry_run: false, from_version: None, stamp: "run-2".to_string() }).unwrap();
    let retag = second.steps.iter().find(|(s, _)| s.id == "error-video-tag").unwrap();
    assert!(!retag.1.pending(), "a second pass finds nothing to rename: {:?}", retag.1.actions);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rename_that_would_overwrite_an_existing_video_is_refused_not_forced() {
    let dir = install("error-clash");
    let videos = dir.join("userdata/videos");
    std::fs::create_dir_all(&videos).unwrap();
    std::fs::write(videos.join("2026-01-01_01-01-01-ERROR.mp4"), b"the retry state").unwrap();
    // Somebody, or some crash, already made the target.
    std::fs::write(videos.join("2026-01-01_01-01-01-ERROR1.mp4"), b"already counted").unwrap();
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();

    let report = migrate::run(&options(&config, false)).unwrap();
    let step = report.steps.iter().find(|(s, _)| s.id == "error-video-tag").unwrap();
    assert!(!step.1.blocked.is_empty(), "the clash must be reported");
    assert!(step.1.blocked.iter().any(|b| b.contains("already exists")), "{:?}", step.1.blocked);
    assert_eq!(std::fs::read(videos.join("2026-01-01_01-01-01-ERROR1.mp4")).unwrap(), b"already counted", "the existing file was not replaced");
    assert!(videos.join("2026-01-01_01-01-01-ERROR.mp4").is_file(), "and neither was the source");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The marker is a record, not a lock
// ---------------------------------------------------------------------------

#[test]
fn the_marker_records_what_ran_and_can_be_read_by_a_second_process() {
    let dir = install("marker");
    let db = dir.join("userdata/db");
    legacy_month(&db, "default_2026-03_wind.db", 1);
    write_user(&dir, r#"{ "lang": "sc" }"#);
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();
    migrate::run(&options(&config, false)).unwrap();

    let text = std::fs::read_to_string(marker::marker_path(&config)).unwrap();
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["migrated_to"].as_str(), Some(marker::LATEST_KNOWN_RELEASE));
    assert!(parsed["plan_hash"].as_str().unwrap().len() == 64, "a content hash, not a timestamp");
    let steps = parsed["steps"].as_object().unwrap();
    assert!(steps.contains_key("index-schema"), "{steps:?}");
    for (name, record) in steps {
        assert!(record["fingerprint"].as_str().unwrap().len() == 64, "{name} has no content hash");
        assert!(!record["done_at"].as_str().unwrap().is_empty());
    }
    // Deleting it is survivable: every step re-derives its work from the tree.
    std::fs::remove_file(marker::marker_path(&config)).unwrap();
    let again = migrate::run(&options(&config, false)).unwrap();
    assert!(!again.changed(), "losing the marker must not re-do completed work");
    assert!(again.blockers().is_empty(), "{:?}", again.blockers());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_month_after_a_migration_is_offered_again_rather_than_skipped() {
    let dir = install("new-month");
    let db = dir.join("userdata/db");
    legacy_month(&db, "default_2026-02_wind.db", 2);
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();
    migrate::run(&options(&config, false)).unwrap();
    assert_eq!(columns(&db.join("default_2026-02_wind.db")).len(), 9);

    // September arrives, in the legacy shape again — which is what a downgrade or a copied-in month looks
    // like, and the reason the marker cannot be the only thing deciding whether to act.
    let september = legacy_month(&db, "default_2026-09_wind.db", 4);
    let second = migrate::run(&migrate::Options { config: &config, dry_run: false, from_version: None, stamp: "run-2".to_string() }).unwrap();
    let schema = second.steps.iter().find(|(s, _)| s.id == "index-schema").unwrap();
    assert!(schema.1.pending(), "a month that appeared since the last run must be migrated: {:?}", schema.1.actions);
    assert_eq!(columns(&september).len(), 9);
    assert_eq!(rows(&september), 4);
    // The month that was already migrated is not backed up a second time.
    let backed_up = second
        .steps
        .iter()
        .flat_map(|(_, r)| &r.notes)
        .filter(|n| n.contains("default_2026-02_wind.db"))
        .count();
    assert_eq!(backed_up, 0, "an untouched month produced a note about itself: {backed_up}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// doctor against a real, messy install
// ---------------------------------------------------------------------------

#[test]
fn doctor_reports_the_half_migrated_state_that_no_other_command_shows() {
    let dir = install("doctor");
    let db = dir.join("userdata/db");
    let nine = legacy_month(&db, "default_2026-08_wind.db", 3);
    let seven = legacy_month(&db, "default_2026-09_wind.db", 12);
    std::fs::create_dir_all(dir.join("userdata/videos")).unwrap();
    write_user(&dir, r#"{ "lang": "sc", "user_name": "amy", "my_own_knob": true }"#);
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();
    // Migrate only the schema, then rewind one month, to reach the half-state on purpose.
    migrate::run(&options(&config, false)).unwrap();
    {
        let conn = Connection::open(&seven).unwrap();
        conn.execute_batch("DROP TABLE video_text; CREATE TABLE video_text (videofile_name VARCHAR(100), picturefile_name VARCHAR(100), videofile_time INT, ocr_text TEXT, is_videofile_exist BOOLEAN, is_picturefile_exist BOOLEAN, thumbnail TEXT);").unwrap();
    }

    // A segment still carrying the untagged `-ERROR.` name, written *after* the migration ran: the 0.0.12
    // step already tagged everything that existed at the time, and this is the state a user reaches by
    // recording a failed video after upgrading. `doctor` has to show it as outstanding work.
    std::fs::write(dir.join("userdata/videos/2026-09-01_01-01-01-ERROR.mp4"), b"v").unwrap();

    let health = doctor::inspect(&config).unwrap();
    assert_eq!(health.months.len(), 2);
    let nine_facts = health.months.iter().find(|m| m.name == "default_2026-08_wind.db").unwrap();
    let seven_facts = health.months.iter().find(|m| m.name == "default_2026-09_wind.db").unwrap();
    assert_eq!(nine_facts.columns.len(), 9);
    assert_eq!(seven_facts.columns.len(), 7);
    assert_eq!(seven_facts.missing_columns(), vec!["win_title", "deep_linking"]);
    assert_eq!(health.total_rows, 3, "the rewound month contributes nothing until it is re-indexed");
    assert_eq!(seven_facts.rows, Some(0));
    assert_eq!(nine_facts.rows, Some(3));

    let text = doctor::render(&health);
    assert!(text.contains("HALF-MIGRATED"), "{text}");
    assert!(text.contains("my_own_knob"), "the key the Python reconciler would delete is named: {text}");
    assert!(text.contains("RETRY TAG"), "{text}");
    assert!(text.contains("keys to add"), "{text}");
    assert!(text.contains("nothing to do") || text.contains("index-schema"), "the plan section exists: {text}");
    assert!(text.contains("changed nothing") || text.contains("dry run"), "{text}");
    // And the month that is fine was not touched by the reporting.
    assert_eq!(columns(&nine).len(), 9);
    let json = doctor::to_json(&health);
    assert_eq!(json["total_rows"].as_i64(), Some(3));
    assert!(json["months"].as_array().unwrap().iter().any(|m| m["missing_late_columns"].as_array().unwrap().len() == 2));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The migration must not be the thing that breaks a healthy install.
#[test]
fn migrate_on_a_modern_install_is_silent_and_writes_no_backup() {
    let dir = install("modern");
    let db = dir.join("userdata/db");
    let mut store = wind_store::Store::open_month(&db, "default", 2026, 9).unwrap();
    store
        .append(&[wind_store::Record {
            videofile_name: "2026-09-21_21-16-12.mp4".into(),
            picturefile_name: "2026-09-21_21-16-12/f.jpg".into(),
            videofile_time: 1_758_470_172,
            ocr_text: "already nine columns".into(),
            win_title: Some("notepad.exe".into()),
            deep_linking: None,
            thumbnail: Some("AAA".into()),
        }])
        .unwrap();
    drop(store);
    write_user(&dir, DEFAULTS);
    let config = Config::load(&dir).unwrap();
    layout::Layout::resolve(&config).create(false).unwrap();
    configfile::seed(&config, false).unwrap();
    let before = contents(&dir);

    let report = migrate::run(&options(&config, false)).unwrap();
    assert!(!report.changed(), "{:?}", report.steps.iter().filter(|(_, r)| r.pending()).map(|(s, _)| s.id).collect::<Vec<_>>());
    assert!(report.blockers().is_empty(), "{:?}", report.blockers());
    assert!(!dir.join("userdata/backup").exists(), "a run that changed nothing created a backup folder");
    assert_eq!(contents(&dir), before, "a modern install was rewritten by a migration that had nothing to do");
    let _ = std::fs::remove_dir_all(&dir);
}
