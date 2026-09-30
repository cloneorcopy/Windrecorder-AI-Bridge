//! Fixture installs and seeded month files, shared by the unit tests and the integration tests.
//!
//! Public and not `#[cfg(test)]` for one mechanical reason: an integration test under `tests/`
//! compiles this crate as a dependency, where `cfg(test)` is off, so a test-only module would not be
//! visible to it. A binary-only crate could hide the builder, but then the tool tests could not
//! reach the tools either, and the tools being reachable without an MCP client is the whole design.
//!
//! Every path here is under the OS temporary directory. Nothing in this module may take a real
//! `userdata/` or an install with somebody's history in it: the fixture is written by the crate's own
//! writer, so a mistake here costs a failing test rather than a lost week of screen record.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use wind_store::write::{Record, Store};

/// Distinguishes concurrent fixtures inside one test process.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A throwaway directory that looks like a Windrecorder install.
///
/// `user_settings` is written verbatim into `userdata/config_user.json`. The defaults file carries
/// the keys a bridge reads in the same shape `config_src/config_default.json` has them, so a test
/// that omits a setting sees the same default a real install would — which is how "off by default"
/// gets tested rather than asserted.
pub fn install(tag: &str, user_settings: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windcap-mcp-{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["userdata/db", "userdata/videos", "cache_screenshot", "cache/locks", "config_src"] {
        std::fs::create_dir_all(dir.join(sub)).expect("fixture directories");
    }
    std::fs::write(
        dir.join("config_src/config_default.json"),
        br#"{
  "user_name": "default",
  "db_path": "db",
  "record_videos_dir": "videos",
  "exclude_words": ["1Password"],
  "day_begin_minutes": 180
}"#,
    )
    .expect("defaults file");
    std::fs::write(dir.join("userdata/config_user.json"), format!("{user_settings}\n")).expect("user config");
    dir
}

/// Remove a fixture install. Tests that finish without it leave a directory per case in the OS temp
/// folder, which is untidy but harmless; the ones that assert a file was *not* written depend on the
/// fixture being fresh, so they call this first.
pub fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

/// `%Y-%m-%d_%H-%M-%S` → the stored naive-local epoch, which is what a `videofile_time` holds.
pub fn at(stamp: &str) -> i64 {
    wind_base::clock::LocalParts::from_stamp(stamp)
        .unwrap_or_else(|| panic!("{stamp} is not a %Y-%m-%d_%H-%M-%S stamp"))
        .naive_epoch_seconds()
}

/// A real JPEG, encoded by the same helper the recorder uses, as the index stores it: base64 text.
///
/// It has to be a real image because `tools::sniff_image_format` decides the advertised MIME type by
/// looking at the magic bytes, and a fixture made of `'AAA'` would test the fallback path only.
pub fn thumbnail_base64() -> String {
    let (width, height) = (8usize, 5usize);
    let mut rgb = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            rgb.push((x * 30) as u8);
            rgb.push((y * 50) as u8);
            rgb.push(128);
        }
    }
    base64::engine::general_purpose::STANDARD.encode(wind_base::image::encode_jpeg(&rgb, width, height, 60).expect("jpeg"))
}

/// One row of the fixture. Construct with [`Row::new`] and adjust with the builder methods.
#[derive(Debug, Clone)]
pub struct Row {
    pub stamp: &'static str,
    pub text: &'static str,
    pub title: Option<&'static str>,
    pub url: Option<&'static str>,
    pub picture: Option<&'static str>,
    /// Three states, because the column really has three: a real preview, nothing at all, and the
    /// empty string the indexer writes when a row was created without one.
    pub thumbnail: Thumbnail,
    /// The segment this frame was indexed out of. Left unset it is its own segment, which is *not*
    /// how a recording works: one 900-second .mp4 yields hundreds of rows, and the reason
    /// offset_in_segment exists at all is that a row's own timestamp is rarely its segment's.
    pub segment: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thumbnail {
    Generated,
    /// The empty string, which is what upstream's writer leaves behind.
    Blank,
    None,
}

impl Row {
    pub fn new(stamp: &'static str, text: &'static str, title: Option<&'static str>) -> Row {
        Row { stamp, text, title, url: None, picture: None, thumbnail: Thumbnail::Generated, segment: None }
    }

    pub fn url(mut self, url: &'static str) -> Row {
        self.url = Some(url);
        self
    }

    pub fn picture(mut self, picture: &'static str) -> Row {
        self.picture = Some(picture);
        self
    }

    pub fn thumbnail(mut self, thumbnail: Thumbnail) -> Row {
        self.thumbnail = thumbnail;
        self
    }

    pub fn segment(mut self, segment: &'static str) -> Row {
        self.segment = Some(segment);
        self
    }
}

/// Write one month file. Rows are inserted in the order given, in one transaction each batch.
pub fn month(root: &Path, user: &str, year: i64, month: u32, rows: &[Row]) {
    let mut store = Store::open_month(&root.join("userdata/db"), user, year, month).expect("open_month");
    let generated = thumbnail_base64();
    let records: Vec<Record> = rows
        .iter()
        .map(|row| {
            let videofile_name = format!("{}.mp4", row.segment.unwrap_or(row.stamp));
            let picturefile_name = row.picture.map_or_else(|| format!("{}.jpg", row.stamp), String::from);
            Record {
                videofile_name,
                picturefile_name,
                videofile_time: at(row.stamp),
                ocr_text: row.text.to_string(),
                win_title: row.title.map(String::from),
                deep_linking: row.url.map(String::from),
                thumbnail: match row.thumbnail {
                    Thumbnail::Generated => Some(generated.clone()),
                    Thumbnail::Blank => Some(String::new()),
                    Thumbnail::None => None,
                },
            }
        })
        .collect();
    store.append(&records).expect("append");
}

/// A named slice directory with one frame file in it, so `read::resolve_frame` has something to find.
///
/// The `-SUBMIT` marker is part of the fixture and not decoration: a slice directory is marked *after*
/// its rows are indexed, which is exactly why resolution goes by stamp prefix rather than by name.
pub fn slice(root: &Path, stamp: &str, picture: &str) -> PathBuf {
    let dir = root.join("cache_screenshot").join(format!("{stamp}-SUBMIT"));
    std::fs::create_dir_all(&dir).expect("slice directory");
    let file = dir.join(picture);
    std::fs::write(&file, wind_base::image::encode_jpeg(&[200u8; 3 * 4], 4, 1, 40).expect("frame jpeg")).expect("frame file");
    file
}

/// A month folder holding a segment whose name carries a stage marker.
pub fn segment(root: &Path, year: i64, month: u32, stamp: &str) -> PathBuf {
    let dir = root.join("userdata/videos").join(format!("{year:04}-{month:02}"));
    std::fs::create_dir_all(&dir).expect("videos month directory");
    let file = dir.join(format!("{stamp}-SCREENSHOTS-OCRED.mp4"));
    std::fs::write(&file, b"ftypmp42notreallyavideo").expect("segment file");
    file
}

/// The one recording every frame below was indexed out of.
pub const SEGMENT: &str = "2026-09-21_09-00-00";

/// How many rows [`busy_day`] writes, so a test that counts them survives the fixture growing by one.
pub const BUSY_DAY_ROWS: usize = 10;

/// One seeded month: a working morning, a title that is nothing but a badge, a locked-away password
/// manager, and a silence wide enough to prove the 100-second clip. Each row is a different shape of
/// window title, so the normaliser is exercised across the set rather than on one example.
pub fn busy_day() -> Vec<Row> {
    const ROWS: &[(&str, &str, Option<&str>, Option<&str>)] = &[
        ("2026-09-21_09-00-00", "quarterly forecast sheet", Some("Q3 (2026) review - Excel"), None),
        ("2026-09-21_09-00-30", "quarterly forecast updated", Some("Q3 (2026) review - Excel"), None),
        ("2026-09-21_09-01-00", "ffmpeg -i input.mp4", Some("(13) Blender* render.blend"), None),
        ("2026-09-21_09-01-30", "所有权 transfer complete", Some("ChatGPT - Personal - Microsoft Edge"), Some("https://chat.example/thread")),
        // 420 silent seconds: everything past the cap must not be counted as work.
        ("2026-09-21_09-08-30", "still 所有权", Some("ChatGPT - Personal - Microsoft Edge"), None),
        ("2026-09-21_09-09-00", "vault unlocked", Some("1Password"), None),
        ("2026-09-21_09-09-30", "vault entry", Some("(42)"), None),
        ("2026-09-21_09-10-00", "quarterly summary", Some("Q3 (2026) review - Excel"), None),
        ("2026-09-21_09-10-30", "新聊天 message", Some("(7) 大懒趴俱乐部 – (283859)"), None),
        // The frame that closes the one above. A sample owns the wait until the *next* sample, so the
        // last row of a range owns nothing and drops out of both summaries — which is the rule, and
        // is why this row exists rather than the fixture ending on the telegram one.
        ("2026-09-21_09-11-30", "recording started", Some("OBS Studio"), None),
    ];
    ROWS.iter()
        .map(|(stamp, text, title, url)| {
            let row = Row::new(stamp, text, *title).segment(SEGMENT);
            match url {
                Some(url) => row.url(url),
                None => row,
            }
        })
        .collect()
}

/// The same month with one row that has no preview at all, for the resource's failure path.
pub fn busy_day_without_thumbnails() -> Vec<Row> {
    busy_day().into_iter().map(|row| row.thumbnail(Thumbnail::Blank)).collect()
}
