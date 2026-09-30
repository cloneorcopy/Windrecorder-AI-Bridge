//! Test fixtures: a throwaway install and a seeded month, shared by this crate's unit tests and the
//! integration tests of every crate that reads summaries.
//!
//! Public and not `#[cfg(test)]` for the mechanical reason `wind-mcp`'s fixture module states: an
//! integration test under `tests/` compiles this crate as a dependency, where `cfg(test)` is off, so a
//! test-only module would be invisible to it — and a seeded month file is the one thing every
//! summaries test needs first.
//!
//! Every path here is under the OS temporary directory. Nothing in this module may take a real
//! `userdata/`: these fixtures write into the same two directories the product does, so a mistake here
//! must cost a failing test rather than a week of somebody's screen record.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_store::write::{Record, Store};

use crate::entries::{Coverage, DaySummary, PeriodSummary};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The `written_at` every fixture entry carries, so a test can assert on it without a clock.
pub const STAMP: &str = "2026-09-27 18:04:11";

/// The day the fixtures are built around.
pub const DAY: &str = "2026-09-27";

/// A throwaway directory that looks like a Windrecorder install.
///
/// The defaults file carries the two keys this crate reads — `user_name`, which scopes which month
/// files are the user's, and `day_begin_minutes` at the shipped 180 — because "the day rolls at 03:00"
/// is the assumption every test below depends on, and it is worth asserting that it comes from the
/// config rather than from a constant in here.
pub fn install(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windcap-summary-{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["userdata/db", "userdata/videos", "cache/locks", "config_src"] {
        std::fs::create_dir_all(dir.join(sub)).expect("fixture directories");
    }
    std::fs::write(
        dir.join("config_src/config_default.json"),
        br#"{
  "user_name": "default",
  "db_path": "db",
  "record_videos_dir": "videos",
  "day_begin_minutes": 180,
  "exclude_words": ["1Password"]
}"#,
    )
    .expect("defaults file");
    std::fs::write(dir.join("userdata/config_user.json"), "{}\n").expect("user config");
    dir
}

/// The config of a fixture install. Cheap enough to rebuild per thread, which is what lets the
/// concurrency test in `files` have as many readers as it likes.
pub fn config_at(root: &Path) -> Config {
    Config::load(root).expect("fixture config loads")
}

pub fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

/// `%Y-%m-%d_%H-%M-%S` → the stored naive-local epoch, which is what `videofile_time` holds.
pub fn at(stamp: &str) -> i64 {
    LocalParts::from_stamp(stamp).unwrap_or_else(|| panic!("{stamp} is not a %Y-%m-%d_%H-%M-%S stamp")).naive_epoch_seconds()
}

/// One row of a fixture: when it was indexed, which stretch it came from, its title, its screen text.
///
/// Generic over the string type rather than `&'static str` so a test can hand it a table of literals or
/// a computed name without a `Box::leak` in sight.
pub type Seeded = (i64, &'static str, &'static str, &'static str);

/// Write the month files a set of rows belongs to, grouped by the month of each row's own timestamp.
///
/// The title is empty-string-for-none rather than `Option` because every call site is a table of
/// literals, and `("…", "file.mp4", "", "text")` reads better in one than `None` in that column.
pub fn seed_month<S: AsRef<str>>(root: &Path, user: &str, rows: &[(i64, S, S, S)]) {
    let mut months: Vec<(i64, u32)> = Vec::new();
    for (time, _, _, _) in rows {
        let parts = LocalParts::from_naive_epoch(*time);
        let key = (parts.year, parts.month);
        if !months.contains(&key) {
            months.push(key);
        }
    }
    for (year, month) in months {
        let records: Vec<Record> = rows
            .iter()
            .filter(|(time, _, _, _)| {
                let parts = LocalParts::from_naive_epoch(*time);
                (parts.year, parts.month) == (year, month)
            })
            .map(|(time, videofile, title, text)| Record {
                videofile_name: videofile.as_ref().to_string(),
                picturefile_name: format!("{}.jpg", LocalParts::from_naive_epoch(*time).stamp()),
                videofile_time: *time,
                ocr_text: text.as_ref().to_string(),
                win_title: if title.as_ref().is_empty() { None } else { Some(title.as_ref().to_string()) },
                deep_linking: None,
                thumbnail: None,
            })
            .collect();
        let mut store = Store::open_month(&root.join("userdata/db"), user, year, month).expect("open_month");
        store.append(&records).expect("append");
    }
}

/// The stretches [`morning`] is built from: a key, its file, and text distinct enough that two of them
/// can never digest alike.
const MORNING: [(&str, &str, &str); 4] = [
    ("2026-09-27_09-00-00", "2026-09-27_09-00-00.mp4", "spreadsheet with the Q3 numbers"),
    ("2026-09-27_09-05-00", "2026-09-27_09-05-00.mp4", "the same spreadsheet, now with comments"),
    ("2026-09-27_09-10-00", "2026-09-27_09-10-00.mp4", "a chat about the Q3 numbers"),
    ("2026-09-27_09-15-00", "2026-09-27_09-15-00.mp4", "the chart exported to PNG"),
];

/// A morning with up to four stretches on [`DAY`], each its own segment.
pub fn morning(count: usize) -> Vec<Seeded> {
    MORNING[..count.min(MORNING.len())].iter().copied().map(|(stamp, file, text)| (at(stamp), file, "Qoder", text)).collect()
}

/// The same, written.
pub fn seed_morning(root: &Path, user: &str, count: usize) -> Vec<Seeded> {
    let rows = morning(count);
    seed_month(root, user, &rows);
    rows
}

/// A stretch paragraph with the fixture's numbers, for a caller that needs an entry to exist and does
/// not care which model wrote it.
pub fn period_entry(start: i64) -> PeriodSummary {
    PeriodSummary {
        text: "一段话，中间换了三个窗口。".into(),
        start,
        end: start + 60,
        frames: 4,
        ocr_chars: 100,
        written_at: STAMP.into(),
        written_by: "test".into(),
        model: String::new(),
        source_fingerprint: "aaaa".into(),
        prompt_fingerprint: String::new(),
    }
}

/// A day paragraph written from `summarised` of that day's stretches.
pub fn day_summary(day: &str, summarised: usize) -> DaySummary {
    DaySummary {
        date: day.into(),
        text: "这一天在对账和拍照之间度过。".into(),
        coverage: Coverage { segments_total: summarised, segments_summarised: summarised, missing: vec![] },
        partial: false,
        written_at: STAMP.into(),
        written_by: "windai".into(),
        model: String::new(),
        source_fingerprint: "s".into(),
        stale: false,
        prompt_fingerprint: String::new(),
    }
}

/// One product day, summarised: a paragraph per key plus the day's own.
///
/// The erase and retention passes test against this rather than hand-building entries, because "what
/// `forget` leaves behind" is a question about the files a producer actually wrote.
pub fn seed_day(config: &Config, day: &str, keys: &[&str]) {
    for key in keys {
        crate::files::write_period(config, day, key, &period_entry(at(key))).expect("seed period entry");
    }
    crate::files::write_daily(config, &day_summary(day, keys.len())).expect("seed day summary");
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_store::read;

    #[test]
    fn a_fixture_is_a_real_install_the_store_can_read() {
        let root = install("support");
        seed_month(&root, "default", &[(at("2026-09-27_09-00-00"), "2026-09-27_09-00-00.mp4", "Qoder", "text")]);
        let config = config_at(&root);
        assert_eq!(config.day_begin_minutes(), 180, "the shipped rollover, from the file");
        assert_eq!(config.user_name(), "default");
        let months = read::discover(&config.db_dir());
        assert_eq!(months.len(), 1);
        let conn = months[0].open_with(read::ReadOptions::always_refresh()).expect("open");
        assert_eq!(read::count_rows(&conn).expect("count"), 1);
        cleanup(&root);
    }

    #[test]
    fn rows_from_two_months_land_in_two_files() {
        let root = install("support-two-months");
        seed_month(
            &root,
            "default",
            &[
                (at("2026-08-31_23-00-00"), "2026-08-31_23-00-00.mp4", "a", "old"),
                (at("2026-09-01_09-00-00"), "2026-09-01_09-00-00.mp4", "a", "new"),
            ],
        );
        assert_eq!(read::discover(&config_at(&root).db_dir()).len(), 2);
        cleanup(&root);
    }

    #[test]
    fn two_installs_never_share_a_directory() {
        let a = install("same-tag");
        let b = install("same-tag");
        assert_ne!(a, b, "a fixed name would race the moment cargo runs tests in threads");
        cleanup(&a);
        cleanup(&b);
    }

    #[test]
    fn the_morning_helper_separates_stretches_by_key() {
        let root = install("support-morning");
        let rows = seed_morning(&root, "default", 3);
        assert_eq!(rows.len(), 3);
        let files: Vec<&str> = rows.iter().map(|(_, file, _, _)| *file).collect();
        assert_eq!(files, vec!["2026-09-27_09-00-00.mp4", "2026-09-27_09-05-00.mp4", "2026-09-27_09-10-00.mp4"]);
        cleanup(&root);
    }
}
