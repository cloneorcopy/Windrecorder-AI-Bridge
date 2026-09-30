//! Test-only scaffolding: a canned HTTP listener, a synthetic install, a fixture month of the index.
//!
//! Kept in `src/` rather than `tests/` so that the whole crate's test surface is one compilation
//! unit and every helper is reachable from the module it exists to test.
//!
//! The listener is a real TCP server on loopback rather than a mocked transport on purpose. A mock
//! proves that the code calls *itself*; a listener proves that the request bytes leaving the process
//! are a well-formed HTTP/1.1 POST with the headers and JSON body the endpoint will actually see, and
//! that the parsing survives a real socket's read boundaries. The one thing it cannot prove is the TLS
//! handshake and the hosted endpoint's own behaviour, which is stated as unverified in the report.
//!
//! The framing convention, stated because both sides have to agree on it and a failure to agree arrives
//! as a protocol error three modules away from the cause: every reply here is `HTTP/1.1 <status>` with a
//! `Content-Length` measured in **bytes** — not `Transfer-Encoding: chunked`, and not a character count,
//! which is the difference between a tag answer arriving whole and arriving truncated, since every tag
//! answer is Chinese and therefore three bytes a character — plus `Connection: close`, because one
//! handler serves exactly one request and must not leave a connection open to be answered by the next
//! test's handler. The one exception is a scripted status of `0`, which is not a reply: the connection
//! closes with nothing written, standing in for the endpoint that accepts and then never answers.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};
use wind_base::config::Config;

/// One fixture row: `(stamp, ocr_text, win_title)`.
pub type Row = (String, String, Option<String>);

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap()
}

/// A chat-completions reply carrying `text` as the assistant message.
pub fn completion_body(text: &str) -> String {
    json!({"id":"chatcmpl-test","object":"chat.completion","created":0,
           "model":"gpt-4o",
           "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
           "usage":{"prompt_tokens":40,"completion_tokens":12,"total_tokens":52}})
        .to_string()
}

/// The canned server.
///
/// One thread, one pending connection at a time. Callers are no longer all blocking — `summarize` keeps
/// four requests in flight — and this still serves them one by one, which is deliberate: the lane that
/// arrives nth takes reply nth and is logged at index n under one lock, so a test can pair an answer with
/// the request that got it however the threads interleaved. The other lanes wait in the listen backlog,
/// exactly as they would against a hosted gateway that queues. How many were in flight at once is not
/// something a socket can show without timing, so the pass reports its own wave widths instead — see
/// [`crate::summarize::Pacing`].
///
/// The port is ephemeral (`:0`) and never fixed, so two tests, or two `cargo test` invocations of
/// different crates, cannot collide on it; the URL a test hands to configuration is read back from the
/// listener it just bound. `requests` is the single record of what arrived — the count *and* the reply
/// index both come from it, so a test can never see a count that its own `request(i)` cannot satisfy.
pub struct Canned {
    port: u16,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The full request bytes, in arrival order, for a test to assert on.
    pub requests: Arc<Mutex<Vec<String>>>,
    /// What the server does *besides* answering, per arrival index — see [`Canned::add_effect`].
    effects: Arc<Mutex<BTreeMap<usize, Effect>>>,
}

/// Something the fake server does while a request is in flight, before it answers.
pub type Effect = Arc<dyn Fn() + Send + Sync>;

impl Canned {
    /// Serve `replies` in order; any further request gets a plain "ok" completion.
    pub fn start(replies: Vec<(u16, String)>) -> Canned {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let port = listener.local_addr().expect("local addr").port();
        let running = Arc::new(AtomicBool::new(true));
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(replies));
        let effects: Arc<Mutex<BTreeMap<usize, Effect>>> = Arc::new(Mutex::new(BTreeMap::new()));

        let thread_running = Arc::clone(&running);
        let thread_requests = Arc::clone(&requests);
        let thread_replies = Arc::clone(&replies);
        let thread_effects = Arc::clone(&effects);
        let thread = std::thread::spawn(move || {
            listener.set_nonblocking(true).expect("nonblocking");
            while thread_running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle(stream, &thread_requests, &thread_replies, &thread_effects),
                    // The boring case: nothing is pending, so wait a moment instead of spinning.
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    // Anything else is not a reason to stop serving. The listener is still bound, the next
                    // request may be perfectly ordinary, and a server thread that quits on one unusual
                    // accept turns one bad connection into every later request in the same test failing at
                    // connect time — which is how a 4 %-rate flake becomes a wall of red that says "the
                    // network is broken" instead of "this one accept was odd".
                    Err(_) => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        });

        Canned { port, running, thread: Some(thread), requests, effects }
    }

    /// Run `effect` immediately before answering the `index`th request that arrives, counting from zero.
    ///
    /// This is how a test puts a paragraph on the disk *while a summarise pass is waiting on its
    /// requests*, which is what the MCP bridge does to the same day file from another process — and it is
    /// the only honest way to prove "look at the disk before calling something failed", because an action
    /// taken on the handler side cannot land after the read that is supposed to find it. It is attached
    /// after the server starts rather than passed to [`Canned::start`] because the install, and with it the
    /// segment digests the paragraph has to carry, does not exist until the endpoint URL does.
    ///
    /// A reply scripted with status `0` is the partner of this: the connection closes with nothing written,
    /// which is the shape a real timeout leaves for the client (`{host} accepted the connection but sent no
    /// reply`) produced here on a schedule rather than after fifteen minutes.
    pub fn add_effect(&mut self, index: usize, effect: Effect) {
        self.effects.lock().unwrap().insert(index, effect);
    }

    /// A server that answers every request with one completion of `text`.
    pub fn with_completion(text: &str) -> Canned {
        Canned::start(vec![(200, completion_body(text))])
    }

    /// The `open_ai_base_url` to point configuration at this server.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// The nth raw request, exactly as the socket received it.
    pub fn request(&self, index: usize) -> String {
        let guard = self.requests.lock().unwrap();
        guard.get(index).cloned().unwrap_or_else(|| panic!("only {} request(s) arrived", guard.len()))
    }

    pub fn last_request(&self) -> String {
        let guard = self.requests.lock().unwrap();
        guard.last().cloned().expect("no request arrived at the canned server")
    }

    /// The JSON body of the nth request, parsed.
    pub fn request_json(&self, index: usize) -> Value {
        let raw = self.request(index);
        let body = raw.split_once("\r\n\r\n").unwrap_or(("", "")).1;
        serde_json::from_str(body).unwrap_or_else(|e| panic!("request {index} body is not JSON ({e}): {body}"))
    }
}

impl Drop for Canned {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Bounded so a stuck handler cannot hang the whole test binary: the thread is a daemon of
            // sorts, and if it will not stop, the suite's result is already decided.
            let _ = thread.join();
        }
    }
}

fn handle(
    stream: TcpStream,
    requests: &Arc<Mutex<Vec<String>>>,
    replies: &Arc<Mutex<Vec<(u16, String)>>>,
    effects: &Arc<Mutex<BTreeMap<usize, Effect>>>,
) {
    // The accepted socket goes back to *blocking* mode, and this is the line the whole harness turned on.
    // On Windows an accepted socket inherits the listening socket's nonblocking flag, so without this the
    // first `read_line` answers WSAEWOULDBLOCK whenever the request bytes have not landed at the instant we
    // look — and `SO_RCVTIMEO` is silently ignored on a nonblocking socket, so the deadline below cannot
    // cover for it. The handler then saw an "error", closed the connection without answering, and WinHTTP
    // reported a server that sends no reply. The accept loop's 1 ms poll had been hiding it: by the time
    // `accept` returned, the request had usually arrived, so roughly one request in twenty-five did not.
    // A symptom that reads as "the fake server's HTTP framing is wrong" and is really socket mode.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    // One handle for the whole exchange. `try_clone` used to sit here so the reply could be written while a
    // `BufReader` owned the socket; `into_inner` below does the same job with one fewer Winsock object per
    // request, which is worth having in a suite that opens and closes a connection per assertion.
    let mut reader = BufReader::new(stream);
    let raw = match read_request(&mut reader) {
        Ok(raw) => raw,
        Err(_) => return,
    };
    // Logged and indexed under one lock, so the nth reply can only ever go to the nth request and a
    // test's `request_count()` is exactly the range its `request(i)` can ask for.
    let index = {
        let mut log = requests.lock().unwrap();
        let index = log.len();
        log.push(raw);
        index
    };
    // Whatever this arrival was told to cause in the outside world, caused *before* the answer lands. That
    // order is the whole point: a test asserting "the pass found the file already written when it went to
    // look at the disk" is only honest if the write cannot possibly have happened after the read.
    if let Some(effect) = effects.lock().unwrap().get(&index).cloned() {
        effect();
    }
    let (status, body) = replies
        .lock()
        .unwrap()
        .get(index)
        .cloned()
        .unwrap_or_else(|| (200, completion_body("ok")));
    let mut stream = reader.into_inner();
    if status == 0 {
        // Nothing is written and the socket closes: the client's own report of that is the deadline-class
        // failure this crate cannot otherwise produce on a schedule.
        let _ = stream.shutdown(std::net::Shutdown::Write);
        return;
    }
    // `Content-Length` in *bytes*, not chars: a Chinese tag answer is valid UTF-8 whose byte length is
    // larger than its character count, and a length that lies is a truncated body on the client side —
    // which WinHTTP would report as a short read rather than as a broken header.
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// Read headers, then exactly `Content-Length` bytes of body, or `Err` if no request arrived.
///
/// WinHTTP pipelines nothing here, but a handler that returned after the headers would leave the body in
/// the socket, so the body is read here rather than left for the client. A connection that carried no
/// request line at all is an error and not an empty request, because logging it would consume one of the
/// scripted replies and shift every later answer onto the wrong request.
///
/// The header name is matched case-insensitively because this is the one piece of HTTP framing the
/// harness depends on: a spelling it does not recognise reads as "no body", and a body it does not read
/// is a byte count in the log that does not match the JSON the test then parses.
fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<String> {
    let mut head = String::new();
    let mut declared = 0usize;
    let mut first_line = true;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            // EOF before a request line: a connection that was opened and dropped, not a request.
            if head.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no request arrived"));
            }
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if first_line {
            first_line = false;
        } else if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                declared = value.trim().parse().unwrap_or(0);
            }
        }
        head.push_str(trimmed);
        head.push_str("\r\n");
    }
    let mut body = vec![0u8; declared];
    reader.read_exact(&mut body)?;
    let mut out = head;
    out.push_str("\r\n");
    out.push_str(&String::from_utf8_lossy(&body));
    Ok(out)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

// --- A synthetic install ---------------------------------------------------------------------

/// `userdata/config_user.json` for a throwaway install, carrying the real shipped defaults so that a
/// test overriding one key still sees the other fourteen as the product ships them.
///
/// The AI keys default to *working* values (`sk-test-key-not-a-real-credential`, this module's base
/// URL points at [`Canned`]) because most tests are about behaviour once configured; a test that
/// needs the unconfigured case overrides the key back to the placeholder.
pub fn install(tag: &str, overrides: &Value) -> PathBuf {
    let digest = crate::hashing::fnv1a64(tag.as_bytes());
    let merged = {
        let defaults: Value = serde_json::from_str(
            &std::fs::read_to_string(repo_root().join("config_src/config_default.json")).unwrap(),
        )
        .unwrap();
        let mut object = defaults.as_object().unwrap().clone();
        object.insert(
            "open_ai_api_key".to_string(),
            json!("sk-test-key-not-a-real-credential"),
        );
        for (key, value) in overrides.as_object().expect("overrides must be a JSON object") {
            object.insert(key.clone(), value.clone());
        }
        Value::Object(object)
    };

    let dir = std::env::temp_dir().join(format!("windai-test-{tag}-{:x}-{}", std::process::id(), digest));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config_src")).unwrap();
    std::fs::copy(
        repo_root().join("config_src/config_default.json"),
        dir.join("config_src/config_default.json"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("userdata/db")).unwrap();
    std::fs::write(dir.join("userdata/config_user.json"), serde_json::to_string_pretty(&merged).unwrap()).unwrap();
    dir
}

pub fn config_with(tag: &str, overrides: &Value) -> Config {
    Config::load(&install(tag, overrides)).expect("the synthetic install must parse")
}

/// A [`Settings`](crate::settings::Settings) for a synthetic install, with a working key unless the
/// test overrides it back to the placeholder.
pub fn settings(tag: &str, overrides: &Value) -> crate::settings::Settings {
    crate::settings::Settings::read(&config_with(tag, overrides))
}

pub fn repo_config() -> Config {
    Config::load(&repo_root()).expect("the shipped config_default.json must parse")
}

/// A month of the index written into `dir` — a `userdata/db` directory, because month files sit flat
/// in it named `{user}_{YYYY}-{MM}_wind.db` — containing `rows` of `(stamp, ocr_text, win_title)`.
///
/// Built through `wind_store::write::Store::open_month` rather than raw SQL, so the fixture is
/// byte-identical to what the recorder produces and the reader's routing by filename works on it.
pub fn fixture_month(dir: &Path, user: &str, year: i64, month: u32, rows: &[Row]) {
    use wind_store::write::{Record, Store};
    let mut store = Store::open_month(dir, user, year, month).expect("fixture month database");
    let records: Vec<Record> = rows
        .iter()
        .map(|(stamp, text, title)| Record {
            videofile_name: format!("{stamp}-VIDEO-SCREENSHOTS-OCRED.mp4"),
            picturefile_name: String::new(),
            videofile_time: wind_base::clock::LocalParts::from_stamp(stamp)
                .unwrap_or_else(|| panic!("fixture stamp {stamp:?} is not %Y-%m-%d_%H-%M-%S"))
                .naive_epoch_seconds(),
            ocr_text: text.clone(),
            win_title: title.clone(),
            deep_linking: None,
            thumbnail: Some("/9j/AA==".to_string()),
        })
        .collect();
    store.append(&records).unwrap();
}

/// The `userdata/db` directory of an install, created.
pub fn db_dir(root: &Path) -> PathBuf {
    let dir = root.join("userdata/db");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
