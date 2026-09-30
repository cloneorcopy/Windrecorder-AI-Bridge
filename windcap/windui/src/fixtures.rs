//! Real month files on a real temp directory, for the tests that must not mock the store.
//!
//! Every fixture here writes an actual `default_YYYY-MM_wind.db` through `wind_store::write::Store`
//! under the OS temp directory, in the shape an install root has: `userdata/db`, `userdata/videos`,
//! `windrecorder/config_src`. That matters more than it looks. A test that hand-builds a `Vec<Row>`
//! proves the view paints; only a test that reads a file proves the UI is asking the store the right
//! question — the same question the recorder is answering on the other side of the same bytes.
//!
//! Nothing here is ever pointed at a user's install root, and no fixture writes a thumbnail, so no
//! test decodes a JPEG.


use std::path::{Path, PathBuf};

use wind_base::LocalParts;
use wind_store::write::{Record, Store};

/// A scratch directory number unique to *this test*, not merely to this process.
///
/// cargo runs the suite in parallel threads inside one process, so a temp path built from the pid
/// alone gives two same-tag fixtures the same directory — and the `remove_dir_all` that clears a
/// fixture wipes a sibling's tree mid-run. That is the intermittent failure that passed on re-run.
static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn next_scratch_id() -> u64 {
    SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Copy the shipped `config_src/languages.json` into a fixture's settings directory. Two levels up
/// from `windui` is the repository root that carries it, the same `Catalog` the tray and a real window
/// read.
fn seed_languages(config_src_dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the repository root")
        .join("config_src")
        .join("languages.json");
    std::fs::copy(&shipped, config_src_dir.join("languages.json"))
        .unwrap_or_else(|e| panic!("copy {}: {e}", shipped.display()));
}

/// A row as a test wants to state it: name, wall clock, recognised text, window title.
pub type Seed = (&'static str, &'static str, &'static str, &'static str);

pub struct Library {
    pub root: PathBuf,
}

impl Library {
    /// A fresh install root with no data. `tag` only keeps concurrent test processes apart.
    pub fn empty(tag: &str) -> Library {
        let root = std::env::temp_dir().join(format!("windui-{tag}-{}-{}", std::process::id(), next_scratch_id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("userdata").join("db")).unwrap();
        std::fs::create_dir_all(root.join("userdata").join("videos")).unwrap();
        std::fs::create_dir_all(root.join("windrecorder").join("config_src")).unwrap();
        // A real install ships `languages.json` beside its defaults, and the window now translates
        // itself from it at startup. A scratch root that omits it would make every painted label
        // resolve to a missing-key marker, so a fixture seeds the shipped copy — the same file a user
        // reads. The render tests that want a *different* catalog (a locale, a dropped key) install
        // one on the state directly rather than through this root.
        seed_languages(&root.join("windrecorder").join("config_src"));
        Library { root }
    }

    /// The shipped defaults, minus the 100 keys no UI test reads. Enough for `Config::load` to
    /// answer with the same values a real install would.
    pub fn with_config(self, defaults: &str) -> Library {
        std::fs::write(
            self.root.join("windrecorder/config_src/config_default.json"),
            defaults.as_bytes(),
        )
        .unwrap();
        self
    }

    /// Write one month file. `user`/`year`/`month` decide the filename, which is the routing the
    /// whole product depends on, so a fixture states them rather than inheriting them.
    pub fn month(&self, user: &str, year: i64, month: u32, rows: &[Seed]) -> &Library {
        self.append(user, year, month, &records(rows))
    }

    /// The same, with a real base64 JPEG on every row: the only way to exercise the decode pipeline.
    pub fn month_with_thumbnail(&self, rows: &[Seed], thumbnail: &str) -> &Library {
        let mut records = records(rows);
        for record in &mut records {
            record.thumbnail = Some(thumbnail.to_string());
        }
        self.append("default", 2026, 9, &records)
    }

    fn append(&self, user: &str, year: i64, month: u32, records: &[Record]) -> &Library {
        let dir = self.root.join("userdata").join("db");
        let mut store = Store::open_month(&dir, user, year, month).expect("open month");
        store.append(records).expect("append");
        drop(store);
        self
    }

    /// A segment file on disk, so a row's `Locate` action has something true to point at.
    pub fn with_segment(&self, segment: &str) -> &Library {
        let stamp = LocalParts::from_stamp(&segment[..19]).expect("stamped name");
        let dir = self
            .root
            .join("userdata")
            .join("videos")
            .join(format!("{:04}-{:02}", stamp.year, stamp.month));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(segment), b"not really an mp4").unwrap();
        self
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The config file is the contract here, so a test reads the written value back out of it
    /// through a fresh `Config` rather than trusting the in-memory one.
    pub fn reload(&self) -> wind_base::config::Config {
        wind_base::config::Config::load(&self.root).expect("the fixture root must always load")
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn records(seeds: &[Seed]) -> Vec<Record> {
    seeds
        .iter()
        .map(|(name, stamp, text, title)| Record {
            videofile_name: name.to_string(),
            picturefile_name: String::new(),
            videofile_time: at(stamp),
            ocr_text: text.to_string(),
            win_title: Some(title.to_string()),
            deep_linking: None,
            // No thumbnail: a test that decodes a JPEG is testing the codec, not this UI.
            thumbnail: None,
        })
        .collect()
}

/// `HH:MM:SS` on 2026-09-21 in the app's own epoch.
pub fn at(stamp: &str) -> i64 {
    LocalParts::from_stamp(stamp)
        .unwrap_or_else(|| panic!("not a %Y-%m-%d_%H-%M-%S stamp: {stamp}"))
        .naive_epoch_seconds()
}

pub fn date(year: i64, month: u32, day: u32) -> LocalParts {
    LocalParts { year, month, day, hour: 0, minute: 0, second: 0 }
}

/// A timestamp with a clock time on the fixture's single day.
pub fn clock(hour: u32, minute: u32, second: u32) -> i64 {
    LocalParts { year: 2026, month: 9, day: 21, hour, minute, second }.naive_epoch_seconds()
}

// ---------------------------------------------------------------------------------------------
// A canned HTTP listener, for the one control that has to reach the network to be tested at all
// ---------------------------------------------------------------------------------------------

/// A real TCP server on loopback answering one request at a time with a canned chat completion.
///
/// The same argument `wind-ai`'s own `test_support` makes, and it applies doubly to a settings page:
/// a mocked transport proves the code calls itself, while a listener proves the bytes leaving this
/// process are a well-formed HTTP/1.1 POST carrying the `Authorization` header the endpoint will
/// actually see — which is the only way "the test-connection button exercises the real transport" can
/// be a statement a test checks rather than a claim it repeats. It also makes the redaction honest:
/// the server reflects the header it received back in the body, so a report that survives it has been
/// scrubbed against a string that genuinely crossed a socket.
///
/// The port is ephemeral and never fixed, so two tests — or two crates' test runs — cannot collide.
/// Every reply is `HTTP/1.1 <status>` with a `Content-Length` in **bytes** and `Connection: close`,
/// because one handler serves exactly one request and must not leave the socket open for the next
/// test's handler to be answered by.
pub struct Canned {
    port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Canned {
    /// Answer every request with `status` and `body`.
    pub fn answering(status: u16, body: String) -> Canned {
        Self::start(status, body, false)
    }

    /// Answer every request with `status` and a body that echoes the request's `Authorization` header
    /// back verbatim. This is the shape `wind_ai::error::redact` exists for: a gateway that reflects
    /// its own request into an error page is the failure mode that turns a diagnostic into a leak.
    pub fn reflecting(status: u16) -> Canned {
        Self::start(status, String::new(), true)
    }

    fn start(status: u16, body: String, reflect: bool) -> Canned {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let port = listener.local_addr().expect("local addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = Arc::clone(&stop);
            let recorded = Arc::clone(&requests);
            std::thread::spawn(move || {
                // One connection per accept, and `incoming()` rather than a hand-rolled loop, so the
                // only way this thread ends is the listener being dropped with `stop` already set.
                for stream in listener.incoming() {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    let Ok(mut stream) = stream else { continue };
                    // Headers, then exactly `Content-Length` more bytes: reading to a newline would
                    // stop mid-body and the request text the test asserts on would be truncated.
                    let mut seen: Vec<u8> = Vec::new();
                    let mut header_end = None;
                    let mut byte = [0u8; 1];
                    while header_end.is_none() {
                        if stream.read(&mut byte).unwrap_or(0) == 0 {
                            break;
                        }
                        seen.push(byte[0]);
                        if seen.ends_with(b"\r\n\r\n") {
                            header_end = Some(seen.len() - 4);
                        }
                    }
                    let Some(header_end) = header_end else { continue };
                    let headers = String::from_utf8_lossy(&seen[..header_end]).to_string();
                    let length = headers
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|line| line.split_once(':'))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut body_bytes = vec![0u8; length];
                    let mut filled = 0usize;
                    while filled < length {
                        match stream.read(&mut body_bytes[filled..]) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => filled += read,
                        }
                    }
                    let text = format!("{}\n{}", headers, String::from_utf8_lossy(&body_bytes[..filled]));
                    let authorization = headers
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    recorded.lock().expect("request log").push(text);
                    let payload = if reflect {
                        format!(r#"{{"error":{{"message":"reflected {authorization}"}}}}"#)
                    } else {
                        body.clone()
                    };
                    let reply = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        if status == 200 { "OK" } else { "Error" },
                        payload.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            })
        };
        Canned { port, stop, thread: Some(thread), requests }
    }

    /// The `open_ai_base_url` this listener answers. Loopback and plain HTTP on purpose: it is the
    /// shape a local model server is actually spelled in, and a hosted-style address would need a TLS
    /// certificate this machine has not got.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// Every request the listener has fully read, headers and body, in arrival order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request log").clone()
    }

    /// Whether the endpoint asked for is this one — the assertion that a real socket was used rather
    /// than a stub that quietly answered.
    pub fn saw(&self, needle: &str) -> bool {
        self.requests().iter().any(|r| r.contains(needle))
    }
}

impl Drop for Canned {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // The thread is parked in `accept`, so it will not notice the flag until something
            // connects; poking it with a throwaway connection is the difference between a test that
            // ends and a test harness that hangs.
            let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
            let _ = thread.join();
        }
    }
}
