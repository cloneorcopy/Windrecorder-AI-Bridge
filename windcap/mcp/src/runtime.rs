//! The install this bridge serves: where its files are, and the five settings that decide whether
//! it listens at all.
//!
//! Everything comes from `userdata/config_user.json` through `wind_base::Config`, including the
//! bearer token. That placement is the security design, not a convenience: a command line is
//! readable by every process on the machine (`wmic process list`, Task Manager's command-line
//! column, a crash dump), and this app already runs a tray process the user did not start. A token
//! passed as an argument or an environment variable is a token disclosed. So `windmcp` accepts
//! `--root`, `--host` and `--port` and refuses any other flag rather than offering one.
//!
//! Nothing here imports from the Python package. `config_src/config_default.json` — or the
//! `windrecorder/config_src/` copy an overlay install still keeps, which [`wind_base::install`]
//! decides — overlaid by the user file is the whole contract, and `Config` already reads exactly
//! those two.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::auth;
use wind_base::config::Config;
use wind_base::fslock::LockState;
use wind_store::read::Month;

/// The loopback default. A fresh install that has never heard of this feature must not open a port.
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: i64 = 21120;

#[derive(Debug)]
pub enum Error {
    /// The chosen directory is not a Windrecorder install.
    NotAnInstall(PathBuf),
    /// A config file could not be read or parsed.
    Config(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotAnInstall(p) => write!(f, "{} holds no userdata directory: not a Windrecorder install", p.display()),
            Error::Config(m) => write!(f, "{m}"),
        }
    }
}

/// Where a token came from, reported to the user by name and never by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    Absent,
    /// Present but shorter than a generated secret should be.
    TooShort,
    Usable,
}

/// One Windrecorder install, read on demand.
pub struct Runtime {
    root: PathBuf,
    config: Config,
    /// `(mtime, size)` of the user config last time it was read.
    stamp: Option<(SystemTime, u64)>,
    /// The file changed and did not parse. A resident server cannot simply keep the last good
    /// secret, so the readers below switch to their most restrictive answer until it does.
    torn: bool,
}

impl Runtime {
    /// Resolve an install directory and refuse anything that is not one.
    pub fn open(root: &Path) -> Result<Runtime, Error> {
        if !root.join("userdata").is_dir() {
            return Err(Error::NotAnInstall(root.to_path_buf()));
        }
        let config = Config::load(root).map_err(|e| Error::Config(e.to_string()))?;
        let stamp = Self::config_stamp(&config);
        Ok(Runtime { root: root.to_path_buf(), config, stamp, torn: false })
    }

    fn config_stamp(config: &Config) -> Option<(SystemTime, u64)> {
        let path = config.userdata_dir().join("config_user.json");
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    /// Pick up a settings page that has just been saved.
    ///
    /// The service is resident now — that is the migration away from stdio — so a token rotated in
    /// the web UI has to take effect on the next request, not at the next start. The stamp is the
    /// cheap guard; the reload is the whole point.
    pub fn refresh(&mut self) {
        let current = Self::config_stamp(&self.config);
        if current == self.stamp {
            return;
        }
        match Config::load(&self.root) {
            Ok(reloaded) => {
                self.stamp = Self::config_stamp(&reloaded);
                self.config = reloaded;
                self.torn = false;
            }
            // Fail closed on a file that changed and then could not be read. Keeping the last good
            // token would look friendlier, but it makes the file's contents non-authoritative: a
            // rotation or a revocation written as a corrupt-looking edit would silently not take
            // effect. `enabled` deliberately keeps its last value — see `token()`.
            Err(_) => {
                self.stamp = current;
                self.torn = true;
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The one switch. Absent means off, which is what a config written before this feature
    /// existed says, and what a default-on remote-read of someone's screen history must never be.
    pub fn enabled(&self) -> bool {
        self.config.bool_or("enable_mcp_server", false)
    }

    pub fn host(&self) -> String {
        let host = self.config.str_or("mcp_server_host", DEFAULT_HOST).trim().to_string();
        if host.is_empty() {
            DEFAULT_HOST.to_string()
        } else {
            host
        }
    }

    /// The port, deliberately *not* sanitized.
    ///
    /// `i64_or` would read a typo like `"21l20"` as the default, and a resident endpoint is
    /// referenced by URL from other people's configuration: quietly moving it would leave every
    /// existing client pointing at nothing while the service looked healthy. An unparseable value
    /// reads as `0`, which `auth::startup_guard` refuses to bind.
    pub fn port(&self) -> i64 {
        match self.config.str_or("mcp_server_port", "").trim() {
            "" => DEFAULT_PORT,
            raw => raw.parse::<i64>().unwrap_or(0),
        }
    }

    /// The bearer token. Empty when the config is unreadable, which is a denial rather than a
    /// default: `auth::authenticates` refuses an empty expectation outright.
    pub fn token(&self) -> String {
        if self.torn {
            return String::new();
        }
        self.config.str_or("mcp_server_token", "").trim().to_string()
    }

    /// Whether a request must carry the token.
    ///
    /// Defaults to *on*. A config predating this key must stay protected, and "the file did not
    /// say" is not the same as "the user turned it off" — the user file is rewritten by the app
    /// without atomicity, so a torn read can briefly look like anything.
    pub fn auth_required(&self) -> bool {
        self.torn || self.config.bool_or("mcp_server_auth_required", true)
    }

    pub fn token_state(token: &str, minimum: usize) -> TokenState {
        if token.is_empty() {
            TokenState::Absent
        } else if token.chars().count() < minimum {
            TokenState::TooShort
        } else {
            TokenState::Usable
        }
    }

    /// The month files belonging to the configured user, oldest first.
    ///
    /// `read::discover` returns every month file in the directory; an install can hold more than
    /// one user's history and a bridge that served all of them would answer to one user's AI
    /// assistant with another's screen.
    pub fn months(&self) -> Vec<Month> {
        let user = self.config.user_name();
        wind_store::read::discover(&self.config.db_dir())
            .into_iter()
            .filter(|m| m.user == user)
            .collect()
    }

    /// Which month files hold data for the range, so a one-day question opens one file.
    pub fn months_covering(&self, from: i64, to: i64) -> Vec<Month> {
        wind_store::read::months_in_range(&self.months(), from, to).into_iter().cloned().collect()
    }

    /// Windrecorder's own do-not-index list, which is where a password manager goes.
    pub fn exclude_words(&self) -> Vec<String> {
        self.config
            .str_list("exclude_words")
            .into_iter()
            .map(|word| word.trim().to_lowercase())
            .filter(|word| !word.is_empty())
            .collect()
    }

    pub fn day_begin_minutes(&self) -> i64 {
        self.config.day_begin_minutes()
    }

    /// What the recorder's lock file says, without claiming the process behind it is alive: a
    /// stale lock can outlive its owner, so the age is reported and the inference left to the
    /// reader, exactly as the Python bridge did.
    pub fn recorder_lock(&self) -> (bool, Option<u32>, Option<i64>) {
        let path = self.config.record_lock_path();
        let (present, pid) = match wind_base::fslock::lock_state(&path) {
            LockState::Free => (false, None),
            LockState::HeldBy { pid, alive: _ } => (true, Some(pid)),
            LockState::Owned => (true, Some(std::process::id())),
            LockState::Unreadable => (path.is_file(), None),
        };
        let age = if present {
            std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|d| d.as_secs() as i64)
        } else {
            None
        };
        (present, pid, age)
    }

    /// The URL a client should be pointed at, with the wildcard bind spelled as the address a
    /// LAN peer would actually type.
    pub fn client_url(&self) -> String {
        client_url_for(&self.host(), self.port())
    }

    /// The directories a frame's full-resolution image and its video live in.
    pub fn cache_screenshot_dir(&self) -> PathBuf {
        self.config.cache_screenshot_dir()
    }

    pub fn videos_dir(&self) -> PathBuf {
        self.config.videos_dir()
    }

    /// One of the app's AI result caches, e.g. `userdata/result_ai_extract_tag/2026.json`, with the
    /// reason it is empty attached.
    ///
    /// These are written during the app's own idle maintenance, so reading one costs no API key and
    /// starts no model call, and absence is ordinary: a day nobody tagged has no entry.
    ///
    /// The return is a struct rather than an `Option` because "empty" is three different facts and a
    /// reader that cannot tell them apart has to guess. *No file*, *a file this cannot read*, and *a
    /// file that simply does not hold the key* mean different things to a client — the first two are
    /// a broken or untouched feature, the third is a normal answer to "has anyone asked the model
    /// about this period yet". A cache that once read as `None` for all three let a tool report
    /// "nothing here" while its own cache file sat unparseable next door.
    pub fn ai_cache_at(&self, setting: &str, default_dir: &str, year: i64) -> AiCache {
        let path = self.config.result_dir(setting, default_dir).join(format!("{year}.json"));
        let shown = self.shown(&path);
        let nothing = serde_json::Map::new();
        let read = |exists: bool, readable: bool, entries: serde_json::Map<String, serde_json::Value>| AiCache { path: path.clone(), shown: shown.clone(), exists, readable, entries };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return read(false, false, nothing),
            // A directory where the file should be, or a permission refusal, is a file that is there
            // and cannot be used — which is not the same message to give a client as one that was
            // never written.
            Err(_) => return read(true, false, nothing),
        };
        // Blank is not corrupt. `windai`'s own reader treats a whitespace-only file as an empty map,
        // and a cache that exists and holds nothing is a real third state.
        if text.trim().is_empty() {
            return read(true, true, nothing);
        }
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(serde_json::Value::Object(entries)) => read(true, true, entries),
            // Valid JSON that is not a map, and invalid JSON, give the tool the same thing to say.
            _ => read(true, false, nothing),
        }
    }

    /// A path the way this service prints them: relative to the install root where it is under one,
    /// so an answer about a single day does not repeat the machine's directory layout in every field
    /// of it. One helper, because `status` and `day_summary` describing the same cache file two
    /// different ways is a bug waiting to be cross-checked and lost.
    pub fn shown(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).display().to_string()
    }
}

/// One year of one AI result cache: the file, whether it could be used, and what it holds.
#[derive(Debug)]
pub struct AiCache {
    /// The path that was read, reported so a tool can name the file it looked in.
    pub path: PathBuf,
    /// ...and the same path as this service prints it, under [`Runtime::shown`].
    pub shown: String,
    /// Something exists at that path.
    pub exists: bool,
    /// ...and it parsed as a JSON object.
    pub readable: bool,
    pub entries: serde_json::Map<String, serde_json::Value>,
}

impl AiCache {
    /// How many keys the file holds in each of the two shapes upstream used: `YYYY-MM` for a month
    /// and `YYYY-MM-DD` for a day. `windai` writes only the first; the second is what a legacy
    /// Python-generated cache is full of, and the count is how a client can see which it has.
    pub fn key_shapes(&self) -> (usize, usize) {
        let mut months = 0usize;
        let mut days = 0usize;
        for key in self.entries.keys() {
            if is_day_key(key) {
                days += 1;
            } else if is_month_key(key) {
                months += 1;
            }
        }
        (months, days)
    }
}

/// `YYYY-MM`, the key `windai tags --month` writes.
///
/// Tested a byte at a time rather than by slicing the string. These keys come out of a JSON
/// file on disk that any writer can put anything in, and the shapes are checked by position;
/// reading it as bytes is what makes "not this shape" the only answer a strange key can
/// produce, with no character-boundary case to reason about at all.
pub fn is_month_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() == 7 && bytes[4] == b'-' && digits(&bytes[..4]) && digits(&bytes[5..])
}

/// `YYYY-MM-DD`, the key upstream's day pass wrote and this build does not.
pub fn is_day_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() == 10 && bytes[4] == b'-' && bytes[7] == b'-' && digits(&bytes[..4]) && digits(&bytes[5..7]) && digits(&bytes[8..])
}

/// ASCII digits only, and on bytes for the same reason: a cache key is whatever the
/// file says it is.
fn digits(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit)
}

/// Every year file in one AI result directory, oldest first.
///
/// A file is a year file when its name is `{integer}.json`. That test, not `*.json`, because
/// `windai` also writes `{year}.hash.json` beside each one — the content fingerprints that keep an
/// unchanged month from costing another request — and a listing that counted those as a second copy
/// of the year would report tags that do not exist.
pub fn ai_cache_years(dir: &Path) -> Vec<(i64, PathBuf)> {
    let mut years: Vec<(i64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            let year = name.strip_suffix(".json")?.parse().ok()?;
            path.is_file().then_some((year, path))
        })
        .collect();
    years.sort();
    years
}

/// `host:port`, bracketed for the IPv6 forms that need it.
/// [`Runtime::client_url`] for a host and port that are not on disk yet — which is what the settings
/// page has while the person is still typing, and the reason it is one function rather than two
/// spellings of a URL that could disagree with the one the service prints.
pub fn client_url_for(host: &str, port: i64) -> String {
    let trimmed = host.trim();
    let shown = if auth::is_wildcard(trimmed) { DEFAULT_HOST.to_string() } else { trimmed.to_string() };
    format!("http://{}/mcp", format_authority(&shown, port))
}

/// How long [`listening`] waits for a connect before calling the address dead.
pub const LISTEN_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Is something answering on this address right now?
///
/// A connect, never a bind: binding would take the port away from the very service the question is
/// about, and a settings page that reports "not listening" because it just evicted the bridge is
/// worse than one that says nothing. Loopback with nothing behind it refuses in microseconds, so the
/// timeout only ever bites an address across a network — where no answer is the answer the row means
/// to give. An out-of-range or unresolvable host is `false`, not a panic and not a retry.
pub fn listening(host: &str, port: i64) -> bool {
    if !(1..=65_535).contains(&port) {
        return false;
    }
    // `[::1]` is how the address is written for the URL and the banner; `to_socket_addrs` wants it
    // bare, and asking it to resolve the bracketed form fails on the brackets.
    let bare = host.trim().trim_start_matches('[').trim_end_matches(']');
    let Ok(addresses) = (bare, port as u16).to_socket_addrs() else {
        return false;
    };
    addresses
        .take(2)
        .any(|address| TcpStream::connect_timeout(&address, LISTEN_PROBE_TIMEOUT).is_ok())
}

pub fn format_authority(host: &str, port: i64) -> String {
    // Re-bracketing an address a user already pasted from a URL would produce `[[::1]]:21120`, which
    // is not an authority — and it is printed in the one banner the user actually reads.
    let host = host.trim();
    let bare = host.strip_prefix('[').unwrap_or(host);
    let bare = bare.strip_suffix(']').unwrap_or(bare);
    if bare.contains(':') {
        format!("[{bare}]:{port}")
    } else {
        format!("{bare}:{port}")
    }
}

/// An empty-but-valid install, for the tests that exercise the protocol without any data.
///
/// A fresh directory per call: `Runtime` holds a `Config` that cannot be cloned cheaply, and these
/// tests assert on messages, not on the filesystem, so paying for one `mkdir -p` per assertion buys
/// the isolation that keeps two tests from seeing each other's config.
#[cfg(test)]
pub(crate) fn tests_stub() -> Runtime {
    Runtime::open(&crate::fixture::install("protocol-stub", "{}")).expect("the stub install must be openable")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root(tag: &str, settings: &str) -> PathBuf {
        crate::fixture::install(tag, settings)
    }

    fn settings(host: &str, port: i64, token: &str, auth_required: bool) -> String {
        format!(
            r#"{{"enable_mcp_server": true, "mcp_server_host": "{host}", "mcp_server_port": {port},
  "mcp_server_token": "{token}", "mcp_server_auth_required": {auth_required}}}"#
        )
    }

    /// The settings page prints this URL, and the tray's notice prints this authority. Either of them
    /// building its own string is how a window ends up telling a user to point an assistant somewhere
    /// the service is not.
    #[test]
    fn the_url_the_page_shows_is_the_url_the_service_prints() {
        let token = "t".repeat(crate::auth::TOKEN_MIN_CHARS);
        let dir = fixture_root("url-shared", &settings("127.0.0.1", 21121, &token, true));
        let runtime = Runtime::open(&dir).unwrap();
        assert_eq!(runtime.client_url(), client_url_for("127.0.0.1", 21121));
        assert_eq!(runtime.client_url(), "http://127.0.0.1:21121/mcp");
        assert_eq!(
            client_url_for("0.0.0.0", 21121),
            "http://127.0.0.1:21121/mcp",
            "a wildcard bind is spelled as an address a peer can actually type"
        );
        assert_eq!(format_authority("::1", 21121), "[::1]:21121", "an IPv6 authority keeps its brackets");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The row under the port field answers "is anything there", so the answer has to change when the
    /// service stops — and an impossible port has to read as "no", not as a panic in a settings page.
    #[test]
    fn listening_follows_the_port_rather_than_the_setting() {
        let bound = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port to bind");
        let port = bound.local_addr().unwrap().port() as i64;
        assert!(listening("127.0.0.1", port), "something is accepting here");
        drop(bound);
        assert!(!listening("127.0.0.1", port), "and the moment it stops, so does the row's claim");
        assert!(!listening("127.0.0.1", 70_000), "out of range is not listening, and not a panic");
        assert!(!listening("not a host", 21120), "an address that will not resolve is not listening");
    }

    #[test]
    fn a_directory_that_is_not_an_install_is_refused() {
        let missing = Path::new("Z:/definitely/not/here");
        let err = match Runtime::open(missing) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("a directory with no userdata is not an install"),
        };
        assert!(err.contains("userdata"), "{err}");
    }

    #[test]
    fn an_untouched_config_leaves_the_service_off_and_loopback() {
        let dir = fixture_root("off", "{}");
        let runtime = Runtime::open(&dir).unwrap();
        assert!(!runtime.enabled(), "the one switch must default to off");
        assert_eq!(runtime.host(), "127.0.0.1");
        assert_eq!(runtime.port(), DEFAULT_PORT);
        assert!(runtime.token().is_empty());
        assert!(runtime.auth_required(), "a config predating the key stays protected");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_five_settings_are_read_back() {
        let dir = fixture_root("read", &settings("0.0.0.0", 21999, "a-token-of-sufficient-length", false));
        let runtime = Runtime::open(&dir).unwrap();
        assert!(runtime.enabled());
        assert_eq!(runtime.host(), "0.0.0.0");
        assert_eq!(runtime.port(), 21999);
        assert_eq!(runtime.token(), "a-token-of-sufficient-length");
        assert!(!runtime.auth_required());
        assert_eq!(runtime.client_url(), "http://127.0.0.1:21999/mcp");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A resident endpoint is referenced by URL from somebody else's configuration. A typo must
    /// stop the service, not relocate it.
    #[test]
    fn an_unparseable_port_is_not_silently_defaulted() {
        let dir = fixture_root("port", r#"{"mcp_server_port": "21l20"}"#);
        assert_eq!(Runtime::open(&dir).unwrap().port(), 0, "garbage must reach the bind guard as 0");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_rotated_token_is_picked_up_without_a_restart() {
        let dir = fixture_root("rotate", &settings("127.0.0.1", 21999, "first-token-value-long-enough", true));
        let mut runtime = Runtime::open(&dir).unwrap();
        assert_eq!(runtime.token(), "first-token-value-long-enough");
        // Write a different token and a different size, which is what the settings page does.
        std::fs::write(dir.join("userdata/config_user.json"), settings("127.0.0.1", 21999, "second-token-value-long-enough", true)).unwrap();
        let before = runtime.token();
        runtime.refresh();
        assert_ne!(before, runtime.token(), "refresh() kept serving the superseded token");
        assert_eq!(runtime.token(), "second-token-value-long-enough");
        // And an unchanged file must not be re-read, which is what makes calling this per request
        // affordable: the stamp is the whole cost.
        runtime.refresh();
        assert_eq!(runtime.token(), "second-token-value-long-enough");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_the_configured_users_months_are_visible() {
        let dir = fixture_root("users", "{}");
        let db = dir.join("userdata/db");
        wind_store::write::Store::open_month(&db, "default", 2026, 8).unwrap();
        wind_store::write::Store::open_month(&db, "default", 2026, 9).unwrap();
        wind_store::write::Store::open_month(&db, "roommate", 2026, 9).unwrap();
        let runtime = Runtime::open(&dir).unwrap();
        let visible: Vec<String> = runtime.months().iter().map(|m| format!("{}/{:04}-{:02}", m.user, m.year, m.month)).collect();
        assert_eq!(visible, vec!["default/2026-08", "default/2026-09"], "the bridge leaked another user's index");
        let august = crate::fixture::at("2026-08-15_12-00-00");
        let september = crate::fixture::at("2026-09-15_12-00-00");
        assert_eq!(runtime.months_covering(august, august).len(), 1, "1970 selects nothing because the range is out of range");
        assert_eq!(runtime.months_covering(august, september).len(), 2);
        assert_eq!(runtime.months_covering(september, september).len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exclude_words_are_lowercased_and_trimmed_once() {
        let dir = fixture_root("exclude", r#"{"exclude_words": [" 1Password ", "", "  ", "Keychain"]}"#);
        assert_eq!(Runtime::open(&dir).unwrap().exclude_words(), vec!["1password", "keychain"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_recorder_lock_is_reported_without_claiming_a_process_is_alive() {
        let dir = fixture_root("lock", "{}");
        let (present, pid, age) = Runtime::open(&dir).unwrap().recorder_lock();
        assert!((present, pid, age).eq(&(false, None, None)), "no lock file should read as free");
        std::fs::write(dir.join("cache/locks/LOCK_FILE_RECORD.MD"), b"4242").unwrap();
        let (present, pid, age) = Runtime::open(&dir).unwrap().recorder_lock();
        assert!(present);
        assert_eq!(pid, Some(4242));
        assert_eq!(age, Some(0), "a lock written this instant is zero seconds old");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_ipv6_bind_gets_its_brackets() {
        assert_eq!(format_authority("::1", 21120), "[::1]:21120");
        assert_eq!(format_authority("127.0.0.1", 21120), "127.0.0.1:21120");
    }

    /// The three empty answers must not collapse into one, because only two of them mean "nobody
    /// has generated this yet". This is the reader `tools::attach_ai` stands on.
    #[test]
    fn an_ai_cache_names_which_kind_of_empty_it_is() {
        let dir = fixture_root("ai", "{}");
        let absent = Runtime::open(&dir).unwrap().ai_cache_at("ai_extract_tag_result_dir", "result_ai_extract_tag", 2026);
        assert!(!absent.exists && !absent.readable, "no file yet is not a broken file");

        let year = dir.join("userdata/result_ai_extract_tag");
        std::fs::create_dir_all(&year).unwrap();
        std::fs::write(year.join("2026.json"), b"{ not json").unwrap();
        let torn = runtime_clone(&dir).ai_cache_at("ai_extract_tag_result_dir", "result_ai_extract_tag", 2026);
        assert!(torn.exists && !torn.readable, "a file that will not parse has to be reported as unreadable, not as an empty cache");

        std::fs::write(year.join("2026.json"), b"   \n").unwrap();
        assert!(runtime_clone(&dir).ai_cache_at("ai_extract_tag_result_dir", "result_ai_extract_tag", 2026).readable, "blank is an empty cache, not a corrupt one");

        std::fs::write(year.join("2026.json"), br#"{"2026-09": ["work", "reading"], "2026-09-21": ["x"]}"#).unwrap();
        let loaded = runtime_clone(&dir).ai_cache_at("ai_extract_tag_result_dir", "result_ai_extract_tag", 2026);
        assert_eq!(loaded.entries["2026-09"].as_array().map(Vec::len), Some(2));
        assert_eq!(loaded.key_shapes(), (1, 1), "one month key and one day key, the two shapes upstream shared a file between");

        // Keys a hand-edited or foreign file can hold. None of them is a shape this tool
        // will quote, and the byte-wise test is what keeps a strange one from being read as
        // a month or a day by length alone.
        std::fs::write(year.join("2026.json"), br#"{"202\u00e9-09": [], "note": "x", "2026": [], "2026-9": [], "2026-09-1": [], "2026-09-01-1": []}"#).unwrap();
        assert_eq!(runtime_clone(&dir).ai_cache_at("ai_extract_tag_result_dir", "result_ai_extract_tag", 2026).key_shapes(), (0, 0), "a near-miss key must not be counted as a real one");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `windai` writes `{year}.hash.json` beside every cache it fills. Those are fingerprints, not
    /// tags, and a listing that took them for a second copy of the year would report data that is
    /// not there.
    #[test]
    fn a_year_listing_skips_the_hash_sibling_and_sorts() {
        let dir = fixture_root("years", "{}");
        let year = dir.join("userdata/result_ai_extract_tag");
        std::fs::create_dir_all(&year).unwrap();
        for name in ["2027.json", "2025.json", "2025.hash.json", "2026.json", "notes.txt"] {
            std::fs::write(year.join(name), "{}").unwrap();
        }
        let found = ai_cache_years(&year);
        let listed: Vec<String> = found.iter().map(|(year, _)| year.to_string()).collect();
        assert_eq!(listed, ["2025", "2026", "2027"], "{found:?}");
        assert!(ai_cache_years(&dir.join("userdata/no_such_dir")).is_empty(), "an absent cache dir is empty, not an error");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn runtime_clone(dir: &Path) -> Runtime {
        Runtime::open(dir).unwrap()
    }
}
