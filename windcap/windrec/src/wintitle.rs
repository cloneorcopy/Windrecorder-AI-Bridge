//! The window-title side channel.
//!
//! A frame records what was on the screen; the title records *what the user was in*. Upstream keeps
//! that in a per-day CSV (`cache/win_title/{date}.csv`) written by its own thread, and joins it back
//! onto rows by timestamp when indexing video — which is why a title change is logged even while
//! recording is paused, and why the file must be append-only: a rewrite loses the record of a
//! session the user may still be searching for.
//!
//! The recorder reads the same live value rather than calling `GetForegroundWindow` itself, because
//! the title that belongs with a frame is the one that was true when the frame was grabbed, and the
//! grab and the log are on different threads.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wind_base::clock::{self, LocalParts};
use wind_base::csv;

/// The header upstream's CSV starts with. A reader that assumes column order breaks the moment a
/// title contains a comma, so the parse is by header name.
pub const HEADER: [&str; 3] = ["datetime", "window_title", "deep_linking"];

/// How often the title is polled. Two seconds is what the Python thread sleeps; slower and a quick
/// alt-tab disappears from the day's statistics, faster and the call is pure overhead.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The change-detection state machine, separated from the thread so it can be tested without a
/// window to observe.
#[derive(Debug, Default, Clone)]
pub struct TitleLog {
    last: Option<String>,
    dir: PathBuf,
}

impl TitleLog {
    pub fn new(dir: PathBuf) -> TitleLog {
        TitleLog { last: None, dir }
    }

    /// Record a title. Returns the row that was written, or `None` when nothing changed — an
    /// unchanged title is not an event, and logging it would multiply a static screen's file size by
    /// the poll rate for no information.
    pub fn observe(&mut self, when: &LocalParts, title: Option<&str>) -> Option<Vec<String>> {
        let title = title.map(str::trim).filter(|t| !t.is_empty())?;
        if self.last.as_deref() == Some(title) {
            return None;
        }
        self.last = Some(title.to_string());
        let row = vec![when.display(), title.to_string(), String::new()];
        let path = self.dir.join(Self::path_for(when));
        // A write failure must not stop the recorder: the log is diagnostic, the frames are the
        // product. The error goes to stderr and the title is still considered "seen".
        if let Err(e) = csv::append_row(&path, &HEADER, &row) {
            eprintln!("window-title log {}: {e}", path.display());
        }
        Some(row)
    }

    pub fn path_for(when: &LocalParts) -> String {
        format!("{}.csv", when.date_stamp())
    }
}

/// The polling thread plus the slot the recorder reads.
pub struct Reader {
    shared: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Reader {
    /// Start logging into `dir`. The thread exits when [`Reader::shutdown`] is called or the
    /// process dies, whichever comes first.
    pub fn start(dir: PathBuf) -> Reader {
        let shared = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_shared, thread_stop) = (Arc::clone(&shared), Arc::clone(&stop));
        let handle = std::thread::spawn(move || {
            let mut log = TitleLog::new(dir);
            while !thread_stop.load(Ordering::Relaxed) {
                let title = foreground_title();
                if let Some(row) = log.observe(&clock::now(), title.as_deref()) {
                    if let Ok(mut slot) = thread_shared.lock() {
                        *slot = Some(row[1].clone());
                    }
                } else if let Ok(mut slot) = thread_shared.lock() {
                    *slot = title;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        });
        Reader { shared, stop, handle: Mutex::new(Some(handle)) }
    }

    /// The title as of the last poll. `None` before the first poll lands, which is a real answer:
    /// a frame captured in the first two seconds of the process's life has no logged title yet.
    pub fn current(&self) -> Option<String> {
        self.shared.lock().ok().and_then(|g| g.clone())
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(handle) = guard.take() {
                // Bounded by POLL_INTERVAL; joining is what makes a Ctrl-C exit deterministic rather
                // than leaving a thread mid-write to the day's CSV.
                let _ = handle.join();
            }
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Title of the focused window, as `get_current_wintitle` produces it.
pub fn foreground_title() -> Option<String> {
    extern "system" {
        fn GetForegroundWindow() -> *mut core::ffi::c_void;
        fn GetWindowTextLengthW(hwnd: *mut core::ffi::c_void) -> i32;
        fn GetWindowTextW(hwnd: *mut core::ffi::c_void, buf: *mut u16, max: i32) -> i32;
    }
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return None;
        }
        let mut buffer = vec![0u16; (length + 1) as usize];
        let written = GetWindowTextW(hwnd, buffer.as_mut_ptr(), length + 1);
        if written <= 0 {
            return None;
        }
        String::from_utf16(&buffer[..written as usize]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(stamp: &str) -> LocalParts {
        LocalParts::from_stamp(stamp).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-wintitle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_day_gets_its_own_file_named_by_its_date() {
        assert_eq!(TitleLog::path_for(&at("2026-09-21_21-16-12")), "2026-09-21.csv");
    }

    #[test]
    fn only_a_change_is_an_event() {
        let dir = temp_dir("changes");
        let mut log = TitleLog::new(dir.clone());
        assert!(log.observe(&at("2026-09-21_10-00-00"), Some("Notepad")).is_some());
        assert!(log.observe(&at("2026-09-21_10-00-02"), Some("Notepad")).is_none());
        assert!(log.observe(&at("2026-09-21_10-00-04"), Some("Chrome")).is_some());
        assert_eq!(log.observe(&at("2026-09-21_10-00-06"), Some("Chrome")), None, "already current");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_or_blank_title_writes_nothing_and_resets_nothing() {
        let dir = temp_dir("blank");
        let mut log = TitleLog::new(dir.clone());
        assert!(log.observe(&at("2026-09-21_10-00-00"), None).is_none());
        assert!(log.observe(&at("2026-09-21_10-00-02"), Some("   ")).is_none());
        assert!(log.observe(&at("2026-09-21_10-00-04"), Some("Notepad")).is_some());
        // Returning to a title already seen is a change of state, and so is logged again after the
        // intervening window: the log is a timeline, not a set.
        assert!(log.observe(&at("2026-09-21_10-00-06"), Some("Excel")).is_some());
        assert!(log.observe(&at("2026-09-21_10-00-08"), Some("Notepad")).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_written_file_is_the_shape_the_python_reader_expects() {
        let dir = temp_dir("shape");
        let mut log = TitleLog::new(dir.clone());
        log.observe(&at("2026-09-21_10-00-00"), Some("Fix \"this\", now")).unwrap();
        log.observe(&at("2026-09-21_10-01-00"), Some("Second")).unwrap();
        let path = dir.join("2026-09-21.csv");
        let rows = csv::read_rows(&path, true).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "2026-09-21 10:00:00");
        assert_eq!(rows[0][1], "Fix \"this\", now", "a quoted title must survive one round trip");
        assert_eq!(rows[0][2], "");
        assert_eq!(rows[1][1], "Second");
        let head = std::fs::read_to_string(&path).unwrap();
        assert!(head.starts_with("datetime,window_title,deep_linking\r\n") || head.starts_with("datetime,window_title,deep_linking\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_accumulate_across_days_instead_of_replacing_them() {
        let dir = temp_dir("days");
        let mut log = TitleLog::new(dir.clone());
        log.observe(&at("2026-09-21_23-59-00"), Some("Late")).unwrap();
        log.observe(&at("2026-09-22_00-01-00"), Some("Later")).unwrap();
        assert_eq!(csv::read_rows(&dir.join("2026-09-21.csv"), true).unwrap().len(), 1);
        assert_eq!(csv::read_rows(&dir.join("2026-09-22.csv"), true).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_is_reported_not_fatal() {
        // An unwritable path is a logging failure, not a recording failure.
        let mut log = TitleLog::new(PathBuf::from("Z:\\definitely-not-here\\windcap"));
        let row = log.observe(&at("2026-09-21_10-00-00"), Some("Notepad"));
        assert_eq!(row.map(|r| r[1].clone()), Some("Notepad".to_string()));
    }

    #[test]
    fn this_test_process_can_read_its_own_foreground_title() {
        // A console process usually has a window; when it does not, the function must return None
        // rather than an empty string, because the caller distinguishes the two.
        match foreground_title() {
            Some(title) => assert!(!title.trim().is_empty(), "a blank title must come back as None"),
            None => {}
        }
    }

    #[test]
    fn the_reader_starts_stops_and_never_leaks_a_thread() {
        let dir = temp_dir("reader");
        {
            let reader = Reader::start(dir.clone());
            // One poll interval is enough for the first sample to land or to decide there is none.
            std::thread::sleep(POLL_INTERVAL + Duration::from_millis(300));
            let _ = reader.current();
        }
        // Drop joins the worker; reaching here at all is the assertion.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
