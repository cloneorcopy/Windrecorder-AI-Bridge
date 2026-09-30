//! Making a flag: the grab behind the tray action, and the index row behind a flag placed on a
//! moment that has already passed.
//!
//! Neither of these captures or stores a *new* kind of pixel. A flag's `thumbnail` column is the same
//! base64 JPEG the index keeps for its rows ([`wind_base::image::thumbnail_base64`], read with
//! the same `thumbnail_generation_size_width` / `thumbnail_generation_jpg_quality` pair the recorder
//! uses), and the grab itself is the recorder's [`windcap::capture::Grabber`]. A second GDI path or a
//! second encoder here would be a second thing to keep in agreement with the index.
//!
//! The two entry points differ only in where the frame comes from:
//!
//!   * [`create_now`] — "flag what is on my screen", from the tray. It grabs.
//!   * [`create_from_history`] — `st_create_timestamp_flag_mark_note_from_oneday_timeselect`: the day
//!     view picked an instant, so the frame that *was* captured then is copied out of the index,
//!     along with that row's window title as the starting note.

use std::path::PathBuf;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::image;
use windcap::capture::{monitor_rect, virtual_desktop, Grabber, VirtualDesktop};
use wind_store::aggregate::nearest;
use wind_store::read::{self as store_read, Row};

use crate::flag::{self, Flag, NOTE_PLACEHOLDER};
use crate::store::FlagStore;

/// Working resolution of the one-off grab.
///
/// The recorder's own number, reused. A preview is far narrower than the frame it stands for, but it is box-averaged out of
/// the grabbed pixels, and stretching straight to a few hundred would make the bookmark as blocky as
/// the thing it is meant to stand for. One 1920 px grab costs the tray action tens of milliseconds,
/// which is not what a bookmark has to be optimised for.
pub const GRAB_WIDTH: u32 = 1920;

/// Upstream's `time_threshold=60` in `db_get_closest_row_around_by_datetime`: how far from the
/// flagged instant a captured frame may be and still stand in for it.
pub const ANCHOR_WINDOW_SECONDS: i64 = 60;

/// Where a new flag's pixels came from. Reported by the tray action and the CLI, because "the flag has
/// no picture in it" needs an explanation attached to it rather than discovered a week later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagSource {
    /// Grabbed the live screen from this rectangle.
    Screen(VirtualDesktop),
    /// Copied the thumbnail of an indexed row captured at that moment.
    Index { rowid: i64, videofile_name: String },
    /// Nothing was available, so the flag is filed with an empty thumbnail — which is what upstream
    /// does when its lookup returns no row (`"" if row.empty else …`).
    Nothing,
}

impl FlagSource {
    pub fn label(&self) -> String {
        match self {
            FlagSource::Screen(rect) => {
                format!("screen grab {}x{} at +{}+{}", rect.width, rect.height, rect.x, rect.y)
            }
            FlagSource::Index { rowid, videofile_name } => format!("index row {rowid} ({videofile_name})"),
            FlagSource::Nothing => "no frame within the window".to_string(),
        }
    }
}

/// What a create call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOutcome {
    pub flag: Flag,
    /// Position of the new row in the table, so a caller can point at the row it just made.
    pub index: usize,
    /// Where the frame came from, for the message that follows the action.
    pub source: FlagSource,
    pub path: PathBuf,
    /// False for a dry run, which grabs and computes but writes nothing.
    pub written: bool,
    /// The thumbnail's pixel size, decoded back out of the base64 — the difference between "a string
    /// was produced" and "a preview JPEG was produced".
    pub thumbnail_size: Option<(u32, u32)>,
}

/// The rectangle a flag's grab should cover, following the display strategy the recorder honours.
///
/// Upstream grabs `sct.monitors[display_index]` when `multi_display_record_strategy` is `single` and
/// the whole desktop otherwise. A named display that is not attached falls back to the desktop instead
/// of failing the action, because the user asked for a bookmark, not for a monitor audit — the same
/// trade the recorder makes in `capture_source`.
pub fn grab_source(config: &Config) -> VirtualDesktop {
    if config.str_or("multi_display_record_strategy", "all") == "single" {
        let index = config.i64_or("record_single_display_index", 1) as i32;
        if let Some(rect) = monitor_rect(index) {
            return rect;
        }
        eprintln!("display {index} is not attached; grabbing the whole virtual desktop instead");
    }
    virtual_desktop()
}

/// Grab one frame and produce the thumbnail the column stores, touching no files.
pub fn capture_frame(config: &Config) -> Result<(String, FlagSource), String> {
    windcap::capture::make_thread_dpi_aware();
    let source = grab_source(config);
    if source.width <= 0 || source.height <= 0 {
        return Err(format!("nothing to grab: the desktop is {}x{}", source.width, source.height));
    }
    let frame = grab_once(config, source)?;
    let thumbnail = image::thumbnail_base64(
        &frame.rgb,
        frame.width as usize,
        frame.height as usize,
        config.thumbnail_width(),
        config.thumbnail_quality(),
    )
    .map_err(|e| format!("thumbnail: {e}"))?;
    Ok((thumbnail, FlagSource::Screen(source)))
}

/// The pixels of one grab, owned outright so the caller never holds a borrow of the `Grabber`.
struct Grabbed {
    rgb: Vec<u8>,
    width: u32,
    height: u32,
}

/// One grab, with the rebuild the recorder does when the monitors moved underneath it.
///
/// `Ok(None)` from [`Grabber::grab`] means the live desktop no longer matches the rectangle the
/// grabber was built for — a monitor being hotplugged while the user reaches for the tray. Retrying
/// once against the desktop as it is now is better than failing a bookmark.
fn grab_once(config: &Config, source: VirtualDesktop) -> Result<Grabbed, String> {
    let mut grabber = Grabber::with_source(GRAB_WIDTH, source, false).map_err(|e| e.to_string())?;
    let mut frame = grabber.grab().map_err(|e| e.to_string())?;
    if frame.is_none() && grab_source(config) != source {
        let rebuilt = Grabber::with_source(GRAB_WIDTH, grab_source(config), false).map_err(|e| e.to_string())?;
        drop(grabber);
        grabber = rebuilt;
        frame = grabber.grab().map_err(|e| e.to_string())?;
    }
    let frame = frame.ok_or_else(|| "the desktop changed while it was being grabbed".to_string())?;
    Ok(Grabbed { rgb: frame.rgb, width: frame.width, height: frame.height })
}

/// The row whose frame stands for an instant that has passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub row: Row,
    /// Seconds between the flagged instant and that frame — never more than the window asked for.
    pub distance: i64,
    /// Where the captured frame is on disk, if it still exists.
    ///
    /// Resolved by the store, not here: a slice directory is renamed with a `-SUBMIT`/`-VIDEO` marker
    /// once the segment closes, so only [`wind_store::read::resolve_frame`] can find it again from a
    /// row whose `picturefile_name` is a bare basename.
    pub frame: Option<PathBuf>,
}

/// The indexed frame nearest this instant, within `window` seconds, searched across the month files
/// the window touches.
pub fn anchor(config: &Config, when: LocalParts, window: i64) -> Result<Option<Anchor>, String> {
    let target = when.naive_epoch_seconds();
    let window = window.max(0);
    let months = store_read::discover(&config.db_dir());
    let mut candidates: Vec<Row> = Vec::new();
    for month in store_read::months_in_range(&months, target - window, target + window) {
        let connection = month.open_with(store_read::ReadOptions::default()).map_err(|e| e.to_string())?;
        candidates
            .extend(store_read::rows_in_window(&connection, Some(target - window), Some(target + window)).map_err(|e| e.to_string())?);
    }
    // `nearest` rather than upstream's "the latest row inside the threshold": a frame captured a
    // minute after a flag should not beat one captured a second before it. `nearest`'s tie-break is
    // the earlier row, which is the app's documented "look backwards first".
    let Some(row) = nearest(&candidates, target, window) else {
        return Ok(None);
    };
    let distance = (row.time - target).abs();
    let frame = store_read::resolve_frame(&config.cache_screenshot_dir(), &row);
    Ok(Some(Anchor { row, distance, frame }))
}

/// What to put in the editor's note box for a flag: the title of the window that was up then.
///
/// `Flag_mark_window` prefills with the *current* window title; the same value is what the index
/// recorded at the flagged instant, and a flag on the past should be annotated with the past.
pub fn prefill_note(config: &Config, when: LocalParts) -> Option<String> {
    let anchor = anchor(config, when, ANCHOR_WINDOW_SECONDS).ok()??;
    anchor.row.title().map(str::trim).filter(|title| !title.is_empty()).map(str::to_string)
}

/// The tray action: grab the screen, thumbnail it, append one row.
///
/// A failed grab is an error rather than a thumbnail-less flag: the user asked for a picture of what
/// was on the screen, and quietly filing a bookmark with an empty thumbnail is how the column fills
/// with rows that mean nothing a month later. Use [`create_from_history`] for an instant that has
/// already passed, which expects to find a frame rather than make one.
pub fn create_now(config: &Config, note: Option<&str>, when: LocalParts, dry_run: bool) -> Result<CreateOutcome, String> {
    let (thumbnail, source) = capture_frame(config)?;
    let flag = Flag { thumbnail, when, note: note_or_placeholder(note) };
    persist(config, flag, source, dry_run)
}

/// The day view's "flag the time I selected": copy the frame and the title from the index.
pub fn create_from_history(
    config: &Config,
    when: LocalParts,
    note: Option<&str>,
    dry_run: bool,
) -> Result<CreateOutcome, String> {
    let (thumbnail, title, source) = match anchor(config, when, ANCHOR_WINDOW_SECONDS)? {
        Some(anchor) => (
            anchor.row.thumbnail.clone().unwrap_or_default(),
            anchor.row.title().map(str::to_string),
            FlagSource::Index { rowid: anchor.row.rowid, videofile_name: anchor.row.videofile_name.clone() },
        ),
        // Upstream still files the row with an empty thumbnail in this case; a flag is a bookmark
        // first and a picture second.
        None => (String::new(), None, FlagSource::Nothing),
    };
    let note = match note {
        Some(text) => flag::normalise_note(text),
        None => title.map(|title| flag::normalise_note(&title)).unwrap_or_else(|| NOTE_PLACEHOLDER.to_string()),
    };
    let flag = Flag { thumbnail, when, note };
    persist(config, flag, source, dry_run)
}

/// Append the row and report what happened.
///
/// One line is appended rather than the table rewritten, so a flag taken while an editor window is
/// open cannot lose the rows that window loaded.
fn persist(config: &Config, flag: Flag, source: FlagSource, dry_run: bool) -> Result<CreateOutcome, String> {
    let mut store = FlagStore::load_for(config).map_err(|e| e.to_string())?;
    let thumbnail_size = thumbnail_size(&flag.thumbnail);
    let path = store.path().to_path_buf();
    let index = store.append_persisted(flag.clone(), dry_run).map_err(|e| e.to_string())?;
    Ok(CreateOutcome { flag, index, source, path, written: !dry_run, thumbnail_size })
}

/// The note a new flag starts with: the caller's text, or the placeholder the editor replaces.
fn note_or_placeholder(note: Option<&str>) -> String {
    flag::normalise_note(note.unwrap_or(""))
}

/// The dimensions the encoder produced, read back out of the JPEG inside the base64.
pub fn thumbnail_size(thumbnail: &str) -> Option<(u32, u32)> {
    if thumbnail.is_empty() {
        return None;
    }
    jpeg_dimensions(&STANDARD.decode(thumbnail).ok()?)
}

/// Walk a JPEG's marker chain to its start-of-frame.
///
/// Only the frame header is needed to check a stored thumbnail is what it claims to be, and doing it
/// here keeps the check free of an image decoder: the workspace has a JPEG *encoder*
/// (`wind_base::image`) and nothing that reads one back.
pub fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 1 < bytes.len() {
        if bytes[i] != 0xFF {
            // Inside the entropy stream: the frame header we wanted was not there.
            return None;
        }
        let marker = bytes[i + 1];
        i += 2;
        // Standalone markers carry no length; `0xFF` fill bytes are padding before a real marker.
        if marker == 0xFF || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            continue;
        }
        // SOF0..SOF3 and SOF5..SOF7, SOF9..; DHT(C4), DAC(C8) and DNG(C9) are not frame headers.
        if (0xC0..=0xC3).contains(&marker) || (0xC5..=0xC7).contains(&marker) || (0xC9..=0xCF).contains(&marker) {
            return read_frame_size(&bytes.get(i..)?);
        }
        let length = u16::from_be_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]) as usize;
        if length < 2 {
            return None;
        }
        i += length;
    }
    None
}

/// `SOF` payload: length, precision, then height and width as big-endian counts.
fn read_frame_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let _precision = *bytes.get(2)?;
    let height = u16::from_be_bytes([*bytes.get(3)?, *bytes.get(4)?]) as u32;
    let width = u16::from_be_bytes([*bytes.get(5)?, *bytes.get(6)?]) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wind_store::write::{Record, Store};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-notes-capture-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `Config` whose whole install root is a scratch directory: every derived path — the index,
    /// the cache, the flag table — lands under `dir`, and the shipped defaults still answer.
    fn test_config(dir: &Path) -> Config {
        Config::load(dir).expect("an empty directory loads as an install with only defaults")
    }

    fn stamp(text: &str) -> LocalParts {
        LocalParts::from_stamp(text).unwrap()
    }

    /// The recorder files a preview at the configured width; so must this, or the day view's thumbnails and
    /// the bookmarks stop looking like the same thing. The number itself is not the point — the encoder
    /// keeps whatever aspect the grab had, which is what the row's picture has to do.
    #[test]
    fn a_thumbnail_is_a_real_jpeg_at_the_width_it_was_asked_for() {
        let (w, h) = (640usize, 360usize);
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i % 256) as u8).collect();
        let b64 = image::thumbnail_base64(&rgb, w, h, 70, 30).unwrap();
        assert_eq!(thumbnail_size(&b64), Some((70, 39)), "aspect kept, width exactly the config value");
        assert!(!b64.starts_with("data:"), "the column stores bare base64");
        assert_eq!(thumbnail_size(""), None, "a flag with no frame has no size to report");
    }

    #[test]
    fn jpeg_dimensions_reads_the_header_and_rejects_everything_else() {
        let rgb = vec![128u8; 100 * 50 * 3];
        let jpeg = image::encode_jpeg(&rgb, 100, 50, 80).unwrap();
        assert_eq!(jpeg_dimensions(&jpeg), Some((100, 50)));
        for not_a_jpeg in [vec![], vec![0xFF, 0xD8], b"PNG,\x1a\n".to_vec(), vec![0xFF, 0xD8, 0x00, 0x00]] {
            assert_eq!(jpeg_dimensions(&not_a_jpeg), None, "{not_a_jpeg:?}");
        }
        // A PNG's base64 must not pass for a JPEG: that is how a thumbnail column silently changes
        // container format.
        assert_eq!(thumbnail_size(&STANDARD.encode(b"\x89PNG\r\n\x1a\n")), None);
    }

    #[test]
    fn the_display_strategy_decides_the_grab_rectangle() {
        let dir = scratch("rect");
        let mut config = test_config(&dir);
        let desktop = virtual_desktop();

        assert_eq!(config.str_or("multi_display_record_strategy", "all"), "all");
        assert_eq!(grab_source(&config), desktop);

        // A named display that is not attached falls back to the whole desktop, and the default index
        // of 1 is the first real monitor.
        config.set("multi_display_record_strategy", serde_json::Value::from("single"));
        config.set("record_single_display_index", serde_json::Value::from(9_999));
        assert_eq!(grab_source(&config), desktop);
        config.set("record_single_display_index", serde_json::Value::from(1));
        match monitor_rect(1) {
            Some(rect) => {
                assert!(rect.width > 0 && rect.height > 0);
                assert_eq!(grab_source(&config), rect);
            }
            None => assert_eq!(grab_source(&config), desktop),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dry run grabs for real and writes nothing at all.
    #[test]
    fn a_dry_run_grabs_but_files_nothing() {
        let dir = scratch("dry");
        let config = test_config(&dir);
        if virtual_desktop().width <= 0 {
            // No desktop (a service session, CI): the honest answer is an error, not an empty flag.
            let error = create_now(&config, Some("note"), stamp("2026-09-21_21-16-12"), true).unwrap_err();
            assert!(error.contains("nothing to grab"), "{error}");
        } else {
            let outcome = create_now(&config, Some("typed note"), stamp("2026-09-21_21-16-12"), true).expect("grab");
            assert!(!outcome.written);
            assert_eq!(outcome.flag.note, "typed note");
            assert_eq!(outcome.flag.stored_datetime(), "2026-09-21 21:16:12");
            // The configured width, not a literal one: the flag's preview is drawn at whatever
            // `thumbnail_generation_size_width` says, and the shipped default is a product decision
            // that has already moved once. Pinning 70 here would make this test fail on a Tuesday in
            // September over something that has nothing to do with a dry run writing nothing.
            assert_eq!(
                outcome.thumbnail_size.map(|(w, _)| w),
                Some(config.thumbnail_width()),
                "the grab was filed at the width the settings asked for"
            );
            assert!(matches!(outcome.source, FlagSource::Screen(_)));
            assert!(!config.flag_note_path().exists(), "a dry run creates no table");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The store's job, called: a flag on an instant resolves to the row that was captured then.
    #[test]
    fn an_instant_resolves_to_the_frame_that_was_actually_captured() {
        let dir = scratch("anchor");
        let config = test_config(&dir);
        let at = stamp("2026-09-21_21-16-12").naive_epoch_seconds();
        seed_index(&config, at);

        // The frame on disk lives in a directory that has since gained its pipeline marker, which is
        // why resolution goes through the store.
        let slice = config.cache_screenshot_dir().join("2026-09-21_21-16-10-SUBMIT");
        std::fs::create_dir_all(&slice).unwrap();
        let frame = slice.join("2026-09-21_21-16-12.jpg");
        std::fs::write(&frame, b"jpeg").unwrap();

        let found = anchor(&config, LocalParts::from_naive_epoch(at + 2), 60).unwrap().expect("an anchor");
        assert_eq!(found.row.time, at, "the closest row wins, not the latest inside the window");
        assert_eq!(found.distance, 2);
        assert_eq!(found.row.title(), Some("Notepad"));
        assert_eq!(found.frame.as_deref(), Some(frame.as_path()));
        assert_eq!(prefill_note(&config, LocalParts::from_naive_epoch(at)).as_deref(), Some("Notepad"));

        // Beyond the window there is no anchor, and the flag is filed without a picture rather than
        // borrowing one from an hour away.
        assert!(anchor(&config, LocalParts::from_naive_epoch(at + 3_600), 60).unwrap().is_none());
        assert_eq!(anchor(&config, stamp("2020-01-01_00-00-00"), 60).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flag_on_a_past_instant_copies_the_row_and_its_title() {
        let dir = scratch("from-history");
        let config = test_config(&dir);
        let at = stamp("2026-09-21_21-16-12").naive_epoch_seconds();
        seed_index(&config, at);

        let outcome = create_from_history(&config, LocalParts::from_naive_epoch(at), None, false).unwrap();
        let FlagSource::Index { videofile_name, .. } = &outcome.source else {
            panic!("expected the flag to be sourced from the index, got {:?}", outcome.source);
        };
        assert_eq!(videofile_name, "2026-09-21_21-16-10.mp4");
        assert_eq!(outcome.flag.note, "Notepad", "upstream files the window title as the note");
        assert_eq!(outcome.flag.thumbnail, "AAEDTg==");
        assert_eq!(outcome.thumbnail_size, None, "the copied bytes are whatever the index held");
        assert_eq!(std::fs::read_to_string(&config.flag_note_path()).unwrap(), "thumbnail,datetime,note\nAAEDTg==,2026-09-21 21:16:12,Notepad\n");
        assert_eq!(outcome.index, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_past_instant_with_nothing_recorded_is_still_a_flag() {
        let dir = scratch("from-history-empty");
        let config = test_config(&dir);
        let when = stamp("2026-09-21_21-16-12");
        let outcome = create_from_history(&config, when, None, false).unwrap();
        assert_eq!(outcome.source, FlagSource::Nothing);
        assert_eq!(outcome.flag.note, NOTE_PLACEHOLDER);
        assert!(!outcome.flag.has_thumbnail());
        assert!(outcome.written);
        assert_eq!(std::fs::read_to_string(&config.flag_note_path()).unwrap(), "thumbnail,datetime,note\n,2026-09-21 21:16:12,_\n");

        // An explicit note beats the title and the placeholder, empty included.
        let second = create_from_history(&config, when, Some("nothing"), false).unwrap();
        assert_eq!(second.flag.note, "nothing");
        assert_eq!(second.index, 1);
        assert_eq!(create_from_history(&config, when, Some(""), false).unwrap().flag.note, NOTE_PLACEHOLDER);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One month file with two rows: the test's own, and one a minute earlier to be nearest to.
    fn seed_index(config: &Config, at: i64) {
        let mut store = Store::open_month(&config.db_dir(), &config.user_name(), 2026, 9).unwrap();
        store
            .append(&[
                Record {
                    videofile_name: "2026-09-21_21-16-10.mp4".into(),
                    picturefile_name: "2026-09-21_21-16-12.jpg".into(),
                    videofile_time: at,
                    ocr_text: "screen text".into(),
                    win_title: Some("Notepad".into()),
                    deep_linking: None,
                    thumbnail: Some("AAEDTg==".into()),
                },
                Record {
                    videofile_name: "2026-09-21_21-16-10.mp4".into(),
                    picturefile_name: "2026-09-21_21-15-00.jpg".into(),
                    videofile_time: at - 60,
                    ocr_text: "older".into(),
                    win_title: Some("Chrome".into()),
                    deep_linking: None,
                    thumbnail: Some("AAA=".into()),
                },
            ])
            .unwrap();
    }

    #[test]
    fn a_note_is_the_placeholder_or_what_the_user_typed() {
        assert_eq!(note_or_placeholder(None), NOTE_PLACEHOLDER);
        assert_eq!(note_or_placeholder(Some("")), NOTE_PLACEHOLDER);
        assert_eq!(note_or_placeholder(Some("  ")), "  ", "only truly empty becomes the placeholder, as upstream");
        assert_eq!(note_or_placeholder(Some("a,b")), "a,b");
    }
}
