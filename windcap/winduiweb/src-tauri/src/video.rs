//! The playback door: how a recorded segment reaches a `<video>` inside this window.
//!
//! Until now the honest answer to "can I watch this" was *no — here is the file in Explorer*, and both
//! front ends said so out loud (`commands::locate`'s own comment). This module is the reason that sentence
//! had to be rewritten, because the product already holds everything a player needs: `windmaint` encodes
//! each segment with `-r 1` on the way in *and* out (`maint/src/encode.rs:399`), so a player that seeks to
//! second S lands on frame S — the property every `videofile_time` → offset lookup in this product already
//! rests on; and it writes `-an` and `-movflags +faststart`, so a segment carries no audio to desynchronise
//! and its index sits at the head of the file rather than the tail. What was missing was one channel from
//! those bytes to a media element.
//!
//! ## Why a protocol rather than Tauri's built-in `asset:` one
//!
//! Tauri's asset protocol does exactly this job, and it is not available here: it sits behind the
//! `protocol-asset` cargo feature, which pulls `http-range`, and that crate is in neither this workspace's
//! `Cargo.lock` nor the offline registry this tree builds from. The config knob is closed too, not merely
//! unused — `tauri-build` hard-errors when `tauri.conf.json` enables `assetProtocol` without the matching
//! cargo feature. What is left is the framework's own `register_asynchronous_uri_scheme_protocol`, whose
//! `http` types `tauri` re-exports (`tauri-2.11.6/src/lib.rs:117`): no new dependency, and the range rule
//! written down once, here, where it is testable without a webview.
//!
//! ## What the webview is allowed to ask for
//!
//! A *segment name*, exactly as the index stores it — `2026-09-21_10-00-00.mp4`, or the same stamp with a
//! pipeline marker on it. Never a path. This is the same rule `frame` already answers by row key: the
//! caller says which row it means and the Rust side decides which file that is ([`bare_name`],
//! [`resolve`]). Two things follow. The month folder comes out of the name's own parsed stamp, and the
//! final component out of a directory listing, so no string a webview sends is ever joined onto a root.
//! And a row whose segment `vid_store_day` already swept cannot be conjured back: the door answers *there
//! is nothing to play*, which is the sentence this window already knows how to say about a missing frame.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use tauri::http::{header, StatusCode};
use tauri::{Manager as _, UriSchemeContext, UriSchemeResponder};
use wind_base::config::Config;
use wind_base::LocalParts;
use wind_ui::segments;

/// The scheme this crate registers, and the only one whose requests it answers.
///
/// On Windows a webview reaches a custom scheme at `http://{scheme}.localhost/…`, elsewhere at
/// `{scheme}://localhost/…`. [`source_url`] is the one place that spelling is decided, and it lives in the
/// crate that registers the protocol so the two halves cannot drift apart.
pub const SCHEME: &str = "windvideo";

/// The largest run of bytes one response carries.
///
/// A player asks for the tail of a file and then jumps, so the answer is bounded rather than "the rest of
/// the segment": 8 MiB is what lets a seek start immediately, and the media element asks for the next run
/// as it consumes this one. Tauri's own asset handler caps at 1 MiB for the same reason; the number here is
/// larger because these files are read off the same disk, and one round trip per megabyte of a
/// multi-monitor segment is a lot of round trips.
pub const MAX_CHUNK: u64 = 8 * 1024 * 1024;

/// The URL this platform's webview has to ask `name` through.
pub fn source_url(name: &str) -> String {
    if cfg!(windows) {
        format!("http://{SCHEME}.localhost/{name}")
    } else {
        format!("{SCHEME}://localhost/{name}")
    }
}

/// The one form of request path this door answers: a bare stored segment name.
///
/// Refused: an empty path, any separator, any `..`, anything that is not a `.mp4`, any character outside
/// the set a segment name is built from (`%Y-%m-%d_%H-%M-%S`, a `-MARKER`, and the extension), and any
/// name whose leading stamp is not a real moment — the stamp is what places the file in a month folder, so
/// a name without one is not a segment at all.
///
/// The accepted characters are also the ones that need no escaping in a URL, which is why there is no
/// percent-encoder on either side of this function, and no segment name this product has ever written
/// needed one.
pub fn bare_name(path: &str) -> Option<&str> {
    let name = path.strip_prefix('/').unwrap_or(path);
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    if !name.ends_with(".mp4") {
        return None;
    }
    if !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')) {
        return None;
    }
    LocalParts::from_stamp(name).map(|_| name)
}

/// Where this install keeps its videos, read now rather than resolved once at startup.
///
/// A fresh `Config::load` per request is the rule every command in this crate already follows
/// (`commands::State::env`), and the reason it matters here is that the folder moves: the Recording page
/// edits `record_videos_dir`, and a protocol that settled on a root at startup would go on serving — or
/// refusing — the old one.
fn videos_dir(root: &Path) -> Option<PathBuf> {
    Config::load(root).ok().map(|config| config.videos_dir())
}

/// The segment a stored name refers to on this machine, if the file is still there.
///
/// [`bare_name`] first, then the same resolver the result cards use for their Locate button, so the two
/// doors cannot disagree about whether a segment exists.
pub fn resolve(root: &Path, name: &str) -> Option<PathBuf> {
    let dir = videos_dir(root)?;
    segments::Index::new().resolve(&dir, name)
}

/// The sample entry a segment was recorded with, read out of its own bytes.
///
/// The fourcc lives in the `moov` box, and every segment this product writes carries `-movflags
/// +faststart` (`maint/src/encode.rs`), so the index is at the head of the file and the first couple of
/// hundred kilobytes are enough to name the codec. Asking `ffmpeg` instead would mean a process per row
/// for something a byte scan answers in microseconds, and `ffprobe` is not in the payload.
///
/// `"unknown"` is a real answer, not a failure: it is what a file this product did not write, or one whose
/// `moov` sits past the scanned head, returns — and the caller must then hand the file to the player
/// unchanged rather than guess that it needs work done to it.
pub fn codec_of(path: &Path) -> String {
    const SCAN: u64 = 512 * 1024;
    let Ok(file) = std::fs::File::open(path) else { return "unknown".to_string() };
    let Ok(len) = file.metadata().map(|m| m.len()) else { return "unknown".to_string() };
    let mut head = std::io::BufReader::new(file);
    let mut buffer = vec![0u8; std::cmp::min(len, SCAN) as usize];
    if head.read_exact(&mut buffer).is_err() {
        return "unknown".to_string();
    }
    // Longest marker first where they could overlap in meaning: `av01` is an AV1 sample entry, and a
    // file's own handler box spells it out.
    for (fourcc, name) in [
        (b"hvc1".as_slice(), "hevc"),
        (b"hev1".as_slice(), "hevc"),
        (b"av01".as_slice(), "av1"),
        (b"vp09".as_slice(), "vp9"),
        (b"avc1".as_slice(), "h264"),
    ] {
        if buffer.windows(fourcc.len()).any(|window| window == fourcc) {
            return name.to_string();
        }
    }
    "unknown".to_string()
}

/// The RFC 6381 codec string a media element is asked about, or `None` for a codec no player names.
pub fn codec_mime(codec: &str) -> Option<&'static str> {
    match codec {
        "hevc" => Some("video/mp4; codecs=\"hvc1.1.6.L93.B0\""),
        "av1" => Some("video/mp4; codecs=\"av01.0.05M.08\""),
        "vp9" => Some("video/mp4; codecs=\"vp09.00.10.08\""),
        "h264" => Some("video/mp4; codecs=\"avc1.42E01E\""),
        _ => None,
    }
}

/// Where a decoded-elsewhere copy of a segment lives: `cache\playback`.
pub fn playback_dir(root: &Path) -> PathBuf {
    root.join("cache").join("playback")
}

/// The name a copy of `stem` is kept under, with the source's own stamp in it.
///
/// The stamp is what makes the cache self-correcting: a segment that is re-encoded (or renamed by the
/// retention pipeline) has a different mtime, so its old copy is simply never asked for again, and the
/// prune below can drop it with the rest of the stale ones.
pub fn copy_name(stem: &str, source_mtime_nanos: i128) -> String {
    format!("{stem}-{source_mtime_nanos:x}-h264.mp4")
}

/// How many copies the folder keeps before the oldest are dropped.
///
/// Twenty, because a person who watches more than twenty segments in a sitting is watching them from a
/// media player rather than from this window, and an unbounded cache of re-encoded footage beside a
/// 1 fps library is how a viewer becomes the largest thing on the disk.
pub const KEEP_COPIES: usize = 20;

/// Drop all but the newest [`KEEP_COPIES`] copies, by modification time.
pub fn prune_copies(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            name.ends_with("-h264.mp4").then_some(entry).and_then(|entry| entry.metadata().ok())
                .and_then(|m| Some((m.modified().ok()?, path)))
                .or(None)
        })
        .collect();
    if files.len() <= KEEP_COPIES {
        return;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.iter().skip(KEEP_COPIES) {
        let _ = std::fs::remove_file(path);
    }
}

/// Make — or reuse — a copy of `source` that this machine's media element can decode.
///
/// HEVC and AV1 are what the recorder writes when the user picks `NVIDIA_h265` or `SVT-AV1`, and a
/// `<video>` element on Windows cannot show either without an optional store extension: the row was
/// playable in the sense that the file existed, and unplayable in every way the user could tell. The
/// product already ships the decoder that can — the same `ffmpeg` that turns screenshot slices into these
/// very files — so the answer is to ask it for one copy, once, and keep it in `cache\playback`.
///
/// `-an` because a segment has no audio track to lose (`encode.rs` writes its files that way), and
/// `+faststart` because the copy is served over the same byte door as the original, which seeks.
pub fn playable_copy(config: &Config, source: &Path) -> Result<PathBuf, String> {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "the segment has no name a copy could be made under".to_string())?;
    let stamp = source
        .metadata()
        .and_then(|m| m.modified())
        .map(|t| match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        })
        .unwrap_or(0);
    let dir = playback_dir(&config.root());
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let target = dir.join(copy_name(stem, stamp));
    if target.is_file() {
        return Ok(target);
    }
    let ffmpeg = config.ffmpeg_path();
    // Written under a `.part.mp4` name and renamed, so a window that is closed mid-encode cannot leave a
    // file the next one believes is a finished copy. The `.mp4` has to stay on the end: `ffmpeg` picks the
    // container from the output extension, and a bare `.part` is answered with `Invalid argument` — a
    // status number that told nobody anything when it was all this door reported.
    let part_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.replacen(".mp4", ".part.mp4", 1))
        .unwrap_or_else(|| "copy.part.mp4".to_string());
    let temporary = dir.join(part_name);
    let output = std::process::Command::new(&ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel").arg("error")
        .arg("-y")
        .arg("-i").arg(source)
        .arg("-map").arg("0:v:0")
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "26"])
        .arg("-an")
        .args(["-movflags", "+faststart"])
        .arg(&temporary)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("cannot start {}: {e} — this install has no ffmpeg beside it, so a codec this window cannot decode has nowhere to be turned into one it can", ffmpeg.display()))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&temporary);
        // The encoder's own words, not a status number: `-22` tells nobody that their ffmpeg build has no
        // `libx264`, and the sentence on the row has to be the one that says what to do next.
        let said = String::from_utf8_lossy(&output.stderr).lines().filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join(" ");
        return Err(format!(
            "{} could not re-encode this segment ({})",
            ffmpeg.display(),
            if said.is_empty() { output.status.to_string() } else { said }
        ));
    }
    std::fs::rename(&temporary, &target).map_err(|e| format!("{}: {e}", target.display()))?;
    prune_copies(&dir);
    Ok(target)
}

/// The URL for a re-encoded copy rather than the original segment.
///
/// A separate route, not a name that happens to live somewhere else: [`bare_name`] refuses anything that
/// is not shaped like a recorded segment, and a copy is not one — it is a derived file in the cache
/// folder, and the door that serves it says so in its own path.
pub fn copy_url(name: &str) -> String {
    if cfg!(windows) {
        format!("http://{SCHEME}.localhost/copy/{name}")
    } else {
        format!("{SCHEME}://localhost/copy/{name}")
    }
}

/// The copy a `copy/…` request asks for, if that name is one this door ever wrote.
///
/// The name has to be a bare component ending in the copy suffix [`playable_copy`] produces, and it is
/// joined onto the playback folder and nowhere else. A copy name is built here from a segment stem and a
/// source mtime, so nothing a webview can send is ever treated as a path — the same rule the segment door
/// runs on, applied to the second folder it now serves.
pub fn copy_file(root: &Path, request_path: &str) -> Option<PathBuf> {
    let name = request_path.strip_prefix("/copy/")?;
    if !name.ends_with("-h264.mp4") || !bare_name(name).is_some() {
        return None;
    }
    let dir = playback_dir(root);
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

/// What one request resolves to: `(status, first byte, last byte, bytes to send)`.
///
/// Inclusive at both ends, because that is how `Content-Range` words it and how the read below is bounded.
/// Three decisions live here and nowhere else:
///
///   * a file that fits, with no `Range` header, is `200` with the whole thing;
///   * a file that does not, with no `Range` header, is `206` with the head. No player asks this way — a
///     media element always sends `bytes=0-` — so the branch exists for the tool that does, and handing it
///     the first chunk beats allocating a whole segment for a request nobody made;
///   * a `Range` that cannot be understood or satisfied is `416`, and `plan` is where the refusal is
///     decided so the response can say the file's real length.
///
/// An empty file is `200` with zero bytes: there is no byte to name, so no range can be right either.
pub fn plan(header: Option<&str>, len: u64) -> (StatusCode, u64, u64, u64) {
    if len == 0 {
        return (StatusCode::OK, 0, 0, 0);
    }
    let whole = |start: u64, end: u64| (StatusCode::OK, start, end, end + 1 - start);
    let part = |start: u64, end: u64| (StatusCode::PARTIAL_CONTENT, start, end, end + 1 - start);
    let refused = (StatusCode::RANGE_NOT_SATISFIABLE, 0, 0, 0);
    let Some(spec) = header else {
        let end = len.min(MAX_CHUNK) - 1;
        return if end + 1 == len { whole(0, end) } else { part(0, end) };
    };
    match parse_range(spec, len) {
        // Clamped to one chunk rather than refused: the client asked for more than this door will carry,
        // and a short 206 is the answer a player continues from.
        Some((start, end)) => part(start, end.min(start + MAX_CHUNK - 1)),
        None => refused,
    }
}

/// A single byte run, in the three shapes a player sends: `bytes=START-`, `bytes=START-END`,
/// `bytes=-SUFFIX`.
///
/// `None` means "not one satisfiable run", covering a foreign unit, a multi-range list (a client asking for
/// several runs is downloading, not seeking), a non-numeric bound, a zero-length suffix, a reversed pair,
/// and any start at or past the end of the file. `len` must be non-zero, which is [`plan`]'s job to ensure.
pub fn parse_range(spec: &str, len: u64) -> Option<(u64, u64)> {
    let (unit, list) = spec.trim().split_once('=')?;
    if unit.trim().to_ascii_lowercase() != "bytes" || list.contains(',') {
        return None;
    }
    let last = len - 1;
    let (start, end) = match list.trim().split_once('-')? {
        (start, "") => (start.parse::<u64>().ok()?, last),
        ("", suffix) => {
            let suffix = suffix.parse::<u64>().ok()?;
            if suffix == 0 {
                return None;
            }
            (len.saturating_sub(suffix), last)
        }
        (start, end) => (start.parse::<u64>().ok()?, end.parse::<u64>().ok()?),
    };
    if start > end || start > last {
        return None;
    }
    Some((start, end.min(last)))
}

/// The answer to one request for one segment, decided without a webview in the way.
///
/// Split out from [`serve`] so that the byte-for-byte promise — the body *is* the run the `Content-Range`
/// names — is a unit test rather than something a manual click has to confirm.
pub fn respond_for(file: &Path, header: Option<&str>) -> tauri::http::Response<Vec<u8>> {
    let len = match std::fs::metadata(file) {
        Ok(metadata) => metadata.len(),
        // A name the index still holds and a file the sweep has already taken is the ordinary case for a
        // row older than `vid_store_day`, not a fault in the request.
        Err(_) => return empty(StatusCode::NOT_FOUND),
    };
    let (status, start, end, count) = plan(header, len);
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return empty_with(status, [(header::CONTENT_RANGE, format!("bytes */{len}"))]);
    }
    let body = match read_run(file, start, count) {
        Ok(bytes) => bytes,
        Err(_) => return empty(StatusCode::NOT_FOUND),
    };
    let mut builder = tauri::http::Response::builder()
        .status(status)
        // The two headers a media element wants before it will draw a frame: what these bytes are, and
        // that asking for another run is allowed.
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CONTENT_LENGTH, body.len().to_string());
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
    }
    // A custom scheme is a different origin from the page hosting the window. `*` because the bytes are a
    // recording the user already owns and this door serves nothing outside the install's own videos
    // folder — see [`bare_name`] for why a request cannot name anywhere else.
    builder = builder.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    // …and the header that makes the door inspectable from the page. `Content-Range` is not one of the
    // response headers a cross-origin `fetch` may read by default, so without this the answer is
    // correct and invisible at the same time: the media engine seeks happily (it is not filtered),
    // while a probe — or the window's own diagnostics — sees `null`. Tauri's asset handler exposes the
    // same one header for the same reason (`tauri-2.11.6/src/protocol/asset.rs:97`).
    builder = builder.header(header::ACCESS_CONTROL_EXPOSE_HEADERS, "content-range");
    builder.body(body).unwrap_or_else(|_| empty(StatusCode::INTERNAL_SERVER_ERROR))
}

/// `count` bytes from `file`, starting at `start`.
fn read_run(file: &Path, start: u64, count: u64) -> std::io::Result<Vec<u8>> {
    let mut handle = std::fs::File::open(file)?;
    handle.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::with_capacity(count as usize);
    handle.take(count).read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn empty(status: StatusCode) -> tauri::http::Response<Vec<u8>> {
    empty_with(status, [])
}

fn empty_with<const N: usize>(status: StatusCode, headers: [(header::HeaderName, String); N]) -> tauri::http::Response<Vec<u8>> {
    let mut builder = tauri::http::Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Vec::new())
        .unwrap_or_else(|_| tauri::http::Response::builder().status(status).body(Vec::new()).expect("an empty body with no headers is always constructible"))
}

/// Answer one `windvideo` request, off the thread the window paints on.
///
/// Async for the reason the crate already gives its disk-touching commands (`commands::off_main`): a chunk
/// read from a spinning drive is milliseconds to seconds, and a protocol handler that blocks is a window
/// that stops painting — the same "卡住主程序" complaint, arriving through a seek instead of a search.
///
/// The failure modes are silent by design: a 404 or a 403 with an empty body, because the only reader of
/// this answer is a media element, and the sentence the user needs is the one the window already has for a
/// row whose footage is gone (`windui_frame_missing`).
pub fn serve<R: tauri::Runtime>(context: UriSchemeContext<'_, R>, request: tauri::http::Request<Vec<u8>>, responder: UriSchemeResponder) {
    let path = request.uri().path().to_string();
    let range = request.headers().get(header::RANGE).and_then(|value| value.to_str().ok()).map(str::to_string);
    let root = context.app_handle().state::<crate::commands::State>().root.clone();
    std::thread::spawn(move || {
        // The copy route first: its path is `/copy/<name>`, which `bare_name` would refuse anyway, and a
        // segment request never starts with that prefix.
        let answer = match copy_file(&root, &path) {
            Some(file) => respond_for(&file, range.as_deref()),
            None => match bare_name(&path) {
                None => empty(StatusCode::FORBIDDEN),
                Some(name) => match resolve(&root, name) {
                    Some(file) => respond_for(&file, range.as_deref()),
                    None => empty(StatusCode::NOT_FOUND),
                },
            },
        };
        responder.respond(answer);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_scratch() -> u64 {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("windui-web-video-{tag}-{}-{}", std::process::id(), next_scratch()))
    }

    /// A videos folder in the shape `Config::videos_dir` hands to `segments::Index`: month directories, and
    /// the segment files this test cares about inside them.
    fn videos(tag: &str, files: &[&str]) -> PathBuf {
        let dir = scratch(tag);
        std::fs::create_dir_all(dir.join("2026-09")).expect("month folder");
        for file in files {
            std::fs::write(dir.join("2026-09").join(file), b"segment").expect("segment file");
        }
        dir
    }

    /// A fixture whose bytes are a pattern, not zeros: a response that is off by one has to be caught here
    /// rather than served as "close enough".
    fn file_of_len(tag: &str, len: usize) -> PathBuf {
        let path = scratch(tag).with_extension("mp4");
        let bytes: Vec<u8> = (0..len).map(|index| (index % 251) as u8).collect();
        std::fs::write(&path, bytes).expect("fixture");
        path
    }

    fn byte_at(index: usize) -> u8 {
        (index % 251) as u8
    }

    /// The accepted spellings are the ones the index and the encode pipeline produce, including a name that
    /// picked up a stage marker after the row was written.
    #[test]
    fn a_segment_name_is_accepted_in_every_shape_the_pipeline_writes() {
        assert_eq!(bare_name("/2026-09-21_10-00-00.mp4"), Some("2026-09-21_10-00-00.mp4"));
        assert_eq!(bare_name("2026-09-21_10-00-00-VIDEO-SCREENSHOTS-OCRED.mp4"), Some("2026-09-21_10-00-00-VIDEO-SCREENSHOTS-OCRED.mp4"));
    }

    /// Everything that could turn "play this segment" into "read this file" is refused on the name alone,
    /// before anything touches the disk.
    #[test]
    fn a_name_that_reaches_outside_a_month_folder_is_refused() {
        for refused in [
            "",
            "/",
            "..",
            "../secrets.mp4",
            "2026-09-21_10-00-00.mp4/../../windows/win.ini",
            "C:/install/userdata/videos/2026-09/2026-09-21_10-00-00.mp4",
            "\\\\server\\share\\2026-09-21_10-00-00.mp4",
            "2026-09-21_10-00-00.txt",
            "not-a-stamp.mp4",
            "2026-99-99_99-99-99.mp4",
            "2026 09 21 10 00 00.mp4",
            "2026-09-21_10-00-00.mp4?x=1",
        ] {
            assert_eq!(bare_name(refused), None, "{refused} must not name a segment");
        }
    }

    /// The month folder is chosen by the stamp inside the name and never by a caller's path, and a segment
    /// the maintenance pass renamed is still the row's own segment.
    #[test]
    fn a_stored_name_finds_the_file_even_after_the_pipeline_renamed_it() {
        let dir = videos("resolve", &["2026-09-21_10-00-00-VIDEO-SCREENSHOTS-OCRED.mp4"]);
        let found = segments::Index::new().resolve(&dir, "2026-09-21_10-00-00.mp4").expect("the renamed segment resolves");
        assert_eq!(found.file_name().unwrap().to_string_lossy(), "2026-09-21_10-00-00-VIDEO-SCREENSHOTS-OCRED.mp4");
        assert!(found.starts_with(&dir), "{found:?} escaped the videos folder");
        assert_eq!(segments::Index::new().resolve(&dir, "2026-09-22_10-00-00.mp4"), None, "a segment nobody recorded answers as nothing to play");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_url_follows_the_platforms_own_custom_scheme_spelling() {
        let url = source_url("2026-09-21_10-00-00.mp4");
        if cfg!(windows) {
            assert_eq!(url, "http://windvideo.localhost/2026-09-21_10-00-00.mp4");
        } else {
            assert_eq!(url, "windvideo://localhost/2026-09-21_10-00-00.mp4");
        }
        assert!(!url.contains('\\'), "a URL with a backslash in it is a path, not a URL: {url}");
    }

    /// The three shapes a player sends, and the refusals. `bytes=0-` is the request a media element makes
    /// before it will draw anything, so its inclusive end is what playback opens on.
    #[test]
    fn a_range_spec_becomes_the_inclusive_run_it_names() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-", 100), Some((10, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=-20", 100), Some((80, 99)));
        assert_eq!(parse_range("BYTES=1-2", 100), Some((1, 2)), "the unit name is case-insensitive");
        assert_eq!(parse_range("bytes=0-999", 100), Some((0, 99)), "an end past the file is the file's end");
        for refused in ["", "bytes=", "bytes=abc-def", "items=0-1", "bytes=0-1,4-5", "bytes=-0", "bytes=50-40", "bytes=100-", "bytes=x"] {
            assert_eq!(parse_range(refused, 100), None, "{refused} is not one satisfiable run");
        }
    }

    /// A file that fits is served whole; a file that does not is served in runs, and the status has to say
    /// which — a `200` carrying half a segment would be played as if it were a segment.
    #[test]
    fn the_plan_names_the_run_and_the_status_that_go_with_it() {
        assert_eq!(plan(None, 500), (StatusCode::OK, 0, 499, 500));
        assert_eq!(plan(Some("bytes=0-"), 500), (StatusCode::PARTIAL_CONTENT, 0, 499, 500));
        assert_eq!(plan(None, MAX_CHUNK * 3), (StatusCode::PARTIAL_CONTENT, 0, MAX_CHUNK - 1, MAX_CHUNK));
        assert_eq!(plan(Some("bytes=0-"), MAX_CHUNK * 2 + 7), (StatusCode::PARTIAL_CONTENT, 0, MAX_CHUNK - 1, MAX_CHUNK), "one response is never longer than one chunk");
        assert_eq!(plan(Some("bytes=99999999-"), 500).0, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(plan(Some("garbage"), 500).0, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(plan(Some("bytes=0-"), 0), (StatusCode::OK, 0, 0, 0), "an empty file has no byte to promise");
    }

    /// The body is the bytes the headers promise, from the right place in the file.
    #[test]
    fn a_partial_answer_carries_exactly_the_window_it_promises() {
        let path = file_of_len("partial", 1000);
        let response = respond_for(&path, Some("bytes=100-199"));
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 100-199/1000");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "100");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        let expected: Vec<u8> = (100..200).map(byte_at).collect();
        assert_eq!(response.body(), &expected);
        let _ = std::fs::remove_file(path);
    }

    /// What the page is allowed to see about a run it asked for. `Content-Range` is not on the cross-origin
    /// safelist, so a door that sends it correctly and does not expose it answers a probe with `null` —
    /// right, and indistinguishable from wrong.
    #[test]
    fn a_partial_answer_exposes_the_header_the_page_can_read_back() {
        let path = file_of_len("exposed", 4000);
        let response = respond_for(&path, Some("bytes=0-999"));
        assert_eq!(response.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS], "content-range");
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-999/4000");
        let whole = respond_for(&path, None);
        assert_eq!(whole.status(), StatusCode::OK, "a file that fits in one chunk is served whole");
        assert!(!whole.headers().contains_key(header::CONTENT_RANGE), "a 200 has no range to name");
        let _ = std::fs::remove_file(path);
    }

    /// A seek near the end of a segment asks for a run that ends at the file's last byte, which is where a
    /// hand-rolled range parser is off by one.
    #[test]
    fn the_last_run_of_a_segment_ends_at_the_last_byte() {
        let path = file_of_len("tail", 1000);
        let response = respond_for(&path, Some("bytes=990-"));
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 990-999/1000");
        assert_eq!(response.body().len(), 10);
        assert_eq!(*response.body().last().unwrap(), byte_at(999));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_request_past_the_end_of_a_file_is_refused_with_the_files_length() {
        let path = file_of_len("unsatisfiable", 500);
        let response = respond_for(&path, Some("bytes=500-"));
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */500");
        assert!(response.body().is_empty());
        let _ = std::fs::remove_file(path);
    }

    /// A row can outlive its segment by weeks, and the answer must be "no file" rather than "no permission"
    /// — the two send the user to different places.
    #[test]
    fn a_segment_that_is_simply_gone_answers_404() {
        let response = respond_for(&scratch("gone").with_extension("mp4"), None);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.body().is_empty());
    }

    /// What keeps a seek from allocating a whole multi-monitor segment into the window.
    #[test]
    fn one_response_never_carries_more_than_a_chunk() {
        let len = MAX_CHUNK as usize + 4096;
        let path = file_of_len("chunked", len);
        let response = respond_for(&path, Some("bytes=0-"));
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.body().len() as u64, MAX_CHUNK);
        assert_eq!(response.headers()[header::CONTENT_RANGE], format!("bytes 0-{}/{}", MAX_CHUNK - 1, len));
        assert_eq!(response.body().first(), Some(&byte_at(0)));
        assert_eq!(response.body().last(), Some(&byte_at(MAX_CHUNK as usize - 1)));
        let _ = std::fs::remove_file(path);
    }

    /// Write a file whose head carries one sample-entry fourcc, the way `+faststart` puts the `moov` there.
    fn fake_segment(tag: &str, fourcc: &[u8]) -> PathBuf {
        let path = scratch(tag).with_extension("mp4");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut body = b"\0\0\0 ftypisom".to_vec();
        body.extend_from_slice(fourcc);
        body.extend_from_slice(b"\0\0\0\0moov");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// The codec is read out of the bytes, because the config that wrote them describes what this machine
    /// records *now* — and a library holds `cpu_h264` footage from before the day the user picked
    /// `NVIDIA_h265`, and the reverse after they switched back.
    #[test]
    fn a_segments_codec_is_named_from_its_own_sample_entry() {
        for (fourcc, expected) in [
            (b"hvc1".as_slice(), "hevc"),
            (b"hev1".as_slice(), "hevc"),
            (b"av01".as_slice(), "av1"),
            (b"vp09".as_slice(), "vp9"),
            (b"avc1".as_slice(), "h264"),
        ] {
            let path = fake_segment(&format!("codec-{expected}"), fourcc);
            assert_eq!(codec_of(&path), expected, "{}", String::from_utf8_lossy(fourcc));
            let _ = std::fs::remove_file(path);
        }
        // A file with nothing recognisable in its head is `unknown`, which the caller must read as
        // "hand it to the player unchanged" rather than as "this one needs a copy made".
        let path = scratch("codec-mystery").with_extension("mp4");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a movie at all").unwrap();
        assert_eq!(codec_of(&path), "unknown");
        assert_eq!(codec_mime("unknown"), None, "no player is asked about a codec nobody names");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn only_a_copy_this_door_wrote_is_served_from_the_cache_folder() {
        let root = scratch("copy-door");
        let dir = playback_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        let name = copy_name("2026-09-21_10-00-00-VIDEO", 1_790_000_000_000_000_000);
        assert!(name.ends_with("-h264.mp4"), "{name}");
        std::fs::write(dir.join(&name), b"copy").unwrap();

        let served = copy_file(&root, &format!("/copy/{name}")).and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
        assert_eq!(served.as_deref(), Some(name.as_str()));
        // A segment request is not a copy request, and a name that is not a copy is refused before the
        // folder is even looked at — including the shapes that would walk out of it.
        for path in ["/2026-09-21_10-00-00.mp4", "/copy/2026-09-21_10-00-00.mp4", "/copy/../../userdata/videos/x-h264.mp4", "/copy/", "/copy/nope.mp4"] {
            assert!(copy_file(&root, path).is_none(), "{path} must not be served");
        }
        // A name that is shaped right but was never written is a 404, not a 403.
        assert!(copy_file(&root, "/copy/2030-01-01_00-00-00-1-h264.mp4").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The copy is keyed on the source's own mtime, so a segment that is re-encoded is not served from a
    /// stale copy — and the prune keeps the folder from becoming the largest thing on the disk.
    #[test]
    fn a_copy_is_named_after_the_file_it_came_from_and_the_folder_stays_bounded() {
        assert_ne!(copy_name("a", 111), copy_name("a", 222), "a new mtime is a new copy");
        assert_eq!(copy_name("a", 111), copy_name("a", 111), "and the same file is the same copy");

        let root = scratch("prune");
        let dir = playback_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..(KEEP_COPIES + 6) {
            std::fs::write(dir.join(format!("seg-{index}-h264.mp4")), b"x").unwrap();
        }
        // An unrelated file in the folder is nobody's to delete.
        std::fs::write(dir.join("keep-me.mp4"), b"x").unwrap();
        prune_copies(&dir);
        let kept: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        assert_eq!(kept.iter().filter(|n| n.ends_with("-h264.mp4")).count(), KEEP_COPIES, "{kept:?}");
        assert!(kept.contains(&"keep-me.mp4".to_string()), "the prune touches only what this door wrote");
        let _ = std::fs::remove_dir_all(&root);
    }
}
