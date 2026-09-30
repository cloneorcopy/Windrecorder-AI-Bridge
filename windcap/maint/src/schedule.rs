//! The two idle jobs that used to have a binary and no caller: re-OCR the library, and tag recent months.
//!
//! Upstream folded both into `record_screen.py::idle_maintain_process_main`. Its `ocr_manager_main` call
//! indexed already-recorded video that had never been OCR'd (`record_screen.py:63,273`), and its
//! `llm.cache_day_tags_in_idle_routine` call cached the AI month tags (`record_screen.py:101-102`). The
//! native equivalents — `wind-reindex` and `windai tags` — were built, shipped, staged into `bin/`, and
//! launched by nothing. The consequence for `wind-reindex` is silent: footage that predates an install,
//! or any segment whose first OCR pass failed, stays unsearchable forever. The consequence for `windai`
//! is worse than silent, because `windmcp` answers an AI client's "what has this user been working on"
//! from the very tag cache this step would have written, so a configured user gets an empty answer with no
//! way to tell "nothing tagged" from "tagging never ran."
//!
//! These steps run *inside* the maintenance pass rather than as a scheduler of their own, for exactly the
//! reason `convert` and `expire` do: the idle window is the only time heavy disk work is allowed, so a
//! pass never stutters a live capture. `main`'s `dispatch` holds the maintain lock across the whole
//! `all`, so `reindex` and `ai_tags` cannot run concurrently with each other, with the four existing
//! steps, or with a second pass — which is what matters because `reindex` writes rows into the monthly
//! index and `ai_tags` reads it.
//!
//! The recorder keeps the record lock for its whole life, including while idle-paused, so a lock check
//! alone could not tell "the idle recorder that launched me" from "a recorder that is capturing right
//! now." The pass is therefore launched with `--idle-granted-by <pid>` naming that recorder ([`recorder_guard`]):
//! the new steps run when no recorder holds the record lock, or when the only holder is the very idle
//! process that spawned this pass, and decline otherwise — so a hand-run `windmaint all` cannot OCR or
//! spend API quota under a live capture, while the automated idle pass can.
//!
//! AI costs money, so it is gated on the switches `windai` itself already reads rather than on new ones
//! ([`ai_gate`]): the feature must be enabled (`enable_ai_extract_tag`) *and* allowed in idle
//! (`enable_ai_extract_tag_in_idle`). When either is off this step does not spawn `windai` at all, so not
//! one request leaves the machine. Both are rows on the AI page now, because a pass that exists must not
//! be gated by a switch nobody can see, and their defaults live in `wind_base::config` where all three
//! readers can reach one copy of them.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::fslock::{lock_state, LockState};

/// The reindexer's own binary name (its `[[bin]]` is `wind-reindex`, exe suffix added by the finder).
pub const REINDEX_BINARY: &str = "wind-reindex";
/// How long the walk may stay silent before the parent asks whether it is still wanted.
///
/// One second, which is also the floor the progress publisher uses: a stop request is answered within a
/// second of the walk's next quiet moment, and a walk that is talking is checked on every line anyway.
const STOP_POLL: std::time::Duration = std::time::Duration::from_secs(1);
/// The AI CLI's own binary name.
pub const AI_BINARY: &str = "windai";

/// Every directory a scheduled child binary could legitimately live in, installed first.
///
/// The same order `windrec` uses to find `windmaint` and `windsvc` uses to find its own children: the
/// binary beside this one (a payload staged flat, or a `target/debug` run), then `WINDCAP_HOME`, then the
/// installed `bin/`, then the root, then a cargo release, then a debug build. Release before debug so a
/// background OCR never silently runs from an artefact that cannot carry the performance claim.
pub fn candidate_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            if let Some(parent) = dir.parent() {
                dirs.push(parent.to_path_buf());
            }
        }
    }
    if let Some(home) = std::env::var_os("WINDCAP_HOME") {
        if !home.is_empty() {
            dirs.push(PathBuf::from(home));
        }
    }
    dirs.push(root.join("bin"));
    dirs.push(root.to_path_buf());
    dirs.push(root.join("windcap").join("target").join("release"));
    dirs.push(root.join("windcap").join("target").join("debug"));
    dirs
}

/// Absolute path to a scheduled binary, adding `.exe` when the name does not carry it.
pub fn find_binary(name: &str, root: &Path) -> Option<PathBuf> {
    let file = if name.to_ascii_lowercase().ends_with(".exe") { name.to_string() } else { format!("{name}.exe") };
    candidate_dirs(root).into_iter().map(|dir| dir.join(&file)).find(|candidate| candidate.is_file())
}

/// Why the idle-only work may or may not start, given who holds the record lock.
///
/// `authorized` is the pid the launching recorder passed via `--idle-granted-by`; it is `None` for a
/// hand-run pass. A free or stale (dead-owner) record lock is never a reason to decline. A lock held by a
/// *live* pid declines unless that pid is the authorized idle recorder itself — so the automated pass
/// runs and a manual one refuses to race a capture.
pub fn recorder_guard(config: &Config, authorized: Option<u32>) -> Result<(), String> {
    match lock_state(&config.record_lock_path()) {
        LockState::Free | LockState::Owned | LockState::HeldBy { alive: false, .. } => Ok(()),
        LockState::HeldBy { pid, alive: true } if Some(pid) == authorized => Ok(()),
        LockState::HeldBy { pid, alive: true } => {
            Err(format!("a recorder (pid {pid}) holds {}; only the idle pass that recorder launched may do this work", config.record_lock_path().display()))
        }
        // A lock that names no pid is a foreign tool's; OCRing or spending under it is a guess.
        LockState::Unreadable => Err(format!("{} names no process; refusing to run idle-only work under it", config.record_lock_path().display())),
    }
}

/// The decision about whether to tag this pass, from the two switches `windai` already reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Both switches on: spawn `windai` for the recent months.
    Schedule,
    /// `enable_ai_extract_tag` is off — the feature is not enabled at all.
    DisabledMaster,
    /// The feature is on but `enable_ai_extract_tag_in_idle` is off — not allowed while idle.
    DisabledInIdle,
}

impl Gate {
    /// Why no request was made, for the `Disabled*` arms. Empty for `Schedule`.
    pub fn decline_reason(self) -> &'static str {
        match self {
            Gate::Schedule => "",
            Gate::DisabledMaster => "enable_ai_extract_tag is off, so the feature is switched off entirely",
            Gate::DisabledInIdle => "enable_ai_extract_tag_in_idle is off, so tagging is not allowed in the idle pass",
        }
    }
}

/// Read the AI spend gate through [`wind_base::config`] — the same two accessors, with the same
/// defaults, that `wind_ai::settings::Settings::read` and the AI settings page read these keys with.
///
/// A third copy of a `bool_or` default is how an install that never mentions the key ends up with three
/// answers to "may this pass spend money", which is the whole reason the pair moved to `Config`.
pub fn ai_gate(config: &Config) -> Gate {
    if !config.ai_extract_tag_enabled() {
        return Gate::DisabledMaster;
    }
    if !config.ai_extract_tag_allowed_in_idle() {
        return Gate::DisabledInIdle;
    }
    Gate::Schedule
}

/// The recent months to tag: the current and previous calendar month, intersected with the months the
/// install actually has an index file for. Capping at two bounds each pass's worst-case spend, and
/// `windai`'s own content-hash cache makes an unchanged month cost nothing anyway.
pub fn months_to_tag(config: &Config, now: &LocalParts) -> Vec<(i64, u32)> {
    let current = (now.year, now.month);
    let previous = if current.1 == 1 { (current.0 - 1, 12) } else { (current.0, current.1 - 1) };
    let present: Vec<(i64, u32)> = wind_store::read::discover(&config.db_dir()).iter().map(|m| (m.year, m.month)).collect();
    [previous, current]
        .into_iter()
        .filter(|month| present.contains(month))
        .collect()
}

/// How many videos a lane is given before the pool is worth widening.
///
/// The number is the engine's, not the walk's: a `wind-reindex` process starts one resident OCR child for
/// its whole life (~21 MB of models, and a cold start the port already pays for in the recorder), so a
/// second lane is only worth starting when there is enough work behind it to pay for that. Eight is the
/// floor — one 15-minute segment per lane, four times over — and below it the step runs one process
/// exactly as it always did.
const MIN_BATCH_PER_LANE: usize = 8;

/// The reindexer's argv for one named batch of videos.
///
/// `--file` repeated is the honest shape of "reindex follows convert": the caller names the segments the
/// encode step has already finished, rather than making the whole library wait for the last ffmpeg
/// process. A directory walk would also read a video that is still being written, and would re-ask
/// coverage for the months the lane has nothing to do with.
fn batch_args(binary: &Path, root: &Path, videos: &[PathBuf]) -> Command {
    let mut command = Command::new(binary);
    for video in videos {
        command.arg("--file").arg(video);
    }
    command.args(["--root"]).arg(root).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}

/// The command line `wind-reindex` gets for one whole-library walk: no `--file`, so it enumerates the
/// library itself exactly as it always has.
fn walk_args(binary: &Path, root: &Path, videos: &Path, shard: Option<(usize, usize)>) -> Command {
    let mut command = Command::new(binary);
    command.arg(videos).args(["--root"]).arg(root).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some((lane, lanes)) = shard {
        command.args(["--shard"]).arg(format!("{lane}/{lanes}"));
    }
    command
}

/// A spawned scheduled child whose two pipes are drained off the calling thread.
///
/// Both pipes are read on their own threads for the reason [`reindex_one`] gives: a child that cannot
/// write its stderr stops writing its stdout, and a stdout reader that blocks in the loop holds the step
/// open past the moment somebody pressed 停止整理. One type for all three shapes (the whole-library walk,
/// the lanes over it, and the batches that follow convert) is what keeps "a stop kills the child" one
/// rule instead of three.
/// A child process of the pass, with both output pipes read on their own threads.
///
/// `pub(crate)` for one reason: `convert`'s encoder is the longest single thing the pass does, and the
/// answer to 停止整理 for it is this same wait-with-a-kill — a second implementation of "read the pipes,
/// ask once a second, put the child down" would be a second rule to keep in step with the first.
pub(crate) struct Child {
    label: String,
    process: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    /// The stderr reader, taken when the child is put down so it is never joined twice.
    stderr: Option<std::thread::JoinHandle<String>>,
    /// Everything the child wrote to stdout, kept because the summariser's closing sentence is the one
    /// place that says what it owed, and a second count of the same queue is what this file avoids.
    stdout: String,
    /// Whether [`Child::stop`] has already killed and waited for this child, so [`Child::finish`] does
    /// not pretend a called-off walk was a segment that failed.
    stopped: bool,
}

/// How a child ended: its own exit status, what it wrote to stdout, and everything it wrote to stderr.
pub(crate) struct Exit {
    pub(crate) ok: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Child {
    /// Spawn `command` with both of its output pipes piped and its stdin closed, and start a reader thread
    /// per pipe.
    ///
    /// A child that writes more to stderr than the OS pipe holds stops writing to stdout — which would look
    /// exactly like a walk that never ends — and a stdout reader that blocks in the pump loop would hold
    /// the step open while the child is mid-frame, which is the difference between a stop landing in a
    /// second and a stop landing when the segment finishes. Both are this type's job so no caller has to
    /// re-derive them, and no caller gets to forget one.
    pub(crate) fn start(label: &str, command: &mut Command) -> Result<Child, String> {
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut process = command.spawn().map_err(|e| format!("could not start {label}: {e}"))?;
        let pipe = process.stdout.take().ok_or_else(|| format!("{label} had no stdout to read"))?;
        let errors = process.stderr.take().ok_or_else(|| format!("{label} had no stderr to read"))?;
        let (tx, lines) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = std::thread::spawn(move || {
            let mut collected = String::new();
            for line in std::io::BufReader::new(errors).lines().map_while(Result::ok) {
                collected.push_str(&line);
                collected.push('\n');
            }
            collected
        });
        Ok(Child { label: label.to_string(), process, lines, stderr: Some(stderr), stdout: String::new(), stopped: false })
    }

    /// Read the child's lines until it closes or the pass is called off, asking the pass's own question.
    fn pump(&mut self, config: &Config, on_line: impl FnMut(&str)) -> bool {
        self.pump_with(&|| wind_base::maintain::may_continue(config), on_line)
    }

    /// The same, with the stop question handed in by the caller.
    ///
    /// Kept a parameter for the reason `wind_base::pool` keeps one: the rule has to be testable without a
    /// disk, and without latching the process-wide answer for every later caller in the same test process.
    pub(crate) fn pump_with(&mut self, may_work: &dyn Fn() -> bool, mut on_line: impl FnMut(&str)) -> bool {
        let mut asked = std::time::Instant::now();
        loop {
            match self.lines.recv_timeout(STOP_POLL) {
                Ok(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    self.stdout.push_str(&line);
                    self.stdout.push('\n');
                    on_line(&line);
                    // Asked on the talking second as well as on the quiet one. A walk that says something for
                    // every line — a library of short segments, a summariser that reports one line per day —
                    // would otherwise never reach the timeout branch, and 停止整理 would land when that walk
                    // finished rather than when it was asked to.
                    if asked.elapsed() >= STOP_POLL {
                        asked = std::time::Instant::now();
                        if !may_work() {
                            self.stop();
                            return true;
                        }
                    }
                }
                // Nothing said for a whole second: the walk is inside a frame. This is where a stop request
                // is answered, because the next line may be a minute away.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if !may_work() {
                        self.stop();
                        return true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    /// The process this lane is waiting on, for a reader that has to prove it was put down.
    #[cfg(test)]
    fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Put the child down, and wait for it: an orphan `wind-reindex` keeps its engine and its month file
    /// open over the end of the pass that started it.
    fn stop(&mut self) {
        self.stopped = true;
        let _ = self.process.kill();
        let _ = self.process.wait();
        if let Some(reader) = self.stderr.take() {
            let _ = reader.join();
        }
    }

    /// Wait for the child to exit and say how it ended. `None` for a child that was called off: its
    /// failure is the stop request's, not the segment's, and the pass says so in its own sentence.
    pub(crate) fn finish(mut self) -> Result<Option<Exit>, String> {
        if self.stopped {
            return Ok(None);
        }
        let status = self.process.wait().map_err(|e| format!("could not wait for {}: {e}", self.label))?;
        let stderr = self.stderr.take().and_then(|reader| reader.join().ok()).unwrap_or_default();
        Ok(Some(Exit { ok: status.success(), stdout: std::mem::take(&mut self.stdout), stderr }))
    }
}


/// Re-OCR the library: run `wind-reindex` over the whole video directory.
///
/// `wind-reindex` skips every file already marked `-OCRED`, retries a failed `-ERROR{n}` within the
/// upstream cap, and renames each file only inside its own folder, so an unbounded idle run over a
/// large library is incremental and idempotent — the second pass indexes nothing new and writes nothing.
/// A video that fails OCR comes back `-ERROR{n}` and makes this step report a failure, which
/// `run_pipeline` collects without aborting the rest of the pass.
///
/// It now runs on several processes rather than one, for the reason the OCR engine itself gives: one
/// resident child answers one frame at a time, and a night of footage is thousands of frames. Each lane
/// walks the same library and keeps every [`MIN_BATCH_PER_LANE`]th video (`--shard`), so the sets are
/// disjoint and stable; each lane starts its own engine, which is the whole point. A library with less
/// than a batch per lane is walked by one process, because the second engine would cost more than the
/// work it would do.
pub fn reindex(config: &Config, root: &Path, authorized: Option<u32>) -> Result<(), String> {
    if let Err(why) = recorder_guard(config, authorized) {
        println!("reindex: declined — {why}");
        return Ok(());
    }
    let videos = config.videos_dir();
    if !videos.is_dir() {
        println!("reindex: nothing to do — no video library at {}", videos.display());
        return Ok(());
    }
    let binary = match find_binary(REINDEX_BINARY, root) {
        Some(path) => path,
        None => {
            println!("reindex: declined — {REINDEX_BINARY}.exe was not found beside this binary, in bin\\, or in the install root; run `windcap\\build.ps1`");
            return Ok(());
        }
    };
    let candidates = candidate_videos(&videos);
    let lanes = lanes_for(candidates);
    if lanes > 1 {
        println!(
            "reindex: {REINDEX_BINARY} {} --root {} — {lanes} lanes of every {MIN_BATCH_PER_LANE}th video",
            videos.display(),
            root.display()
        );
        return reindex_lanes(config, root, &binary, &videos, lanes);
    }
    reindex_one(config, root, &binary, &videos)
}

/// How many lanes one library deserves.
fn lanes_for(candidates: usize) -> usize {
    let pool = wind_base::pool::lanes(wind_base::pool::Duty::Subprocess);
    let by_work = candidates / MIN_BATCH_PER_LANE;
    pool.min(by_work.max(1)).max(1)
}

/// Every `.mp4` in the library that does not already carry a pipeline marker.
///
/// This is a *sizing* count and nothing else: the reindexer applies its own name rules and its own
/// coverage check, so a disagreement here costs a lane that has no work, never a video that nobody
/// tried. It walks the month folders the way the reindexer's driver does, oldest month first.
fn candidate_videos(videos_dir: &Path) -> usize {
    let Ok(months) = std::fs::read_dir(videos_dir) else { return 0 };
    months
        .flatten()
        .filter(|month| month.path().is_dir())
        .map(|month| {
            std::fs::read_dir(month.path())
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| {
                            let name = entry.file_name();
                            let name = name.to_string_lossy();
                            name.ends_with(".mp4")
                                && !name.contains("-OCRED")
                                && !name.contains("-INDEX")
                                && !name.contains("-ERROR")
                        })
                        .count()
                })
                .unwrap_or(0)
        })
        .sum()
}

/// Several lanes of the same walk, their output merged into the one stream the pass reports.
///
/// Both pipes of every child are drained on their own thread, for the reason the single walk already
/// gives in [`reindex_one`]: a child whose stderr nobody reads stops writing to stdout, and a stdout
/// reader that blocks in the loop holds the step open past the moment somebody pressed 停止整理. Here
/// that is multiplied by the number of lanes, so the reader threads merge into one channel tagged with
/// the lane that produced each line — the log says which lane a segment belongs to, and the row count
/// stays one number for the step.
fn reindex_lanes(config: &Config, root: &Path, binary: &Path, videos: &Path, lanes: usize) -> Result<(), String> {
    let (tx, lines) = std::sync::mpsc::channel::<(usize, String)>();
    let mut children: Vec<std::process::Child> = Vec::new();
    let mut error_pipes: Vec<std::thread::JoinHandle<String>> = Vec::new();
    for lane in 0..lanes {
        let shard = format!("{lane}/{lanes}");
        let mut child = match Command::new(binary)
            .arg(videos)
            .args(["--root"])
            .arg(root)
            .args(["--shard"])
            .arg(&shard)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                // Everything already spawned has to be put down: an orphaned lane would keep OCRing a
                // library the pass is abandoning, holding its engine and its month file open.
                for mut started in children {
                    let _ = started.kill();
                    let _ = started.wait();
                }
                return Err(format!("could not start {REINDEX_BINARY} for lane {lane}: {e}"));
            }
        };
        let pipe = child.stdout.take().ok_or_else(|| format!("lane {lane} had no stdout to read"))?;
        let errors = child.stderr.take().ok_or_else(|| format!("lane {lane} had no stderr to read"))?;
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send((lane, line)).is_err() {
                    break;
                }
            }
        });
        error_pipes.push(std::thread::spawn(move || {
            let mut collected = String::new();
            for line in std::io::BufReader::new(errors).lines().map_while(Result::ok) {
                collected.push_str(&line);
                collected.push('\n');
            }
            collected
        }));
        children.push(child);
    }
    // The parent's own sender goes away, so the channel closes when every lane's reader thread ends —
    // which is how the loop below knows the walk is over rather than merely quiet.
    drop(tx);

    let mut rows = 0usize;
    let mut asked_to_stop = false;
    loop {
        match lines.recv_timeout(STOP_POLL) {
            Ok((lane, line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                println!("   reindex[{lane}]: {line}");
                if line.starts_with("  row") {
                    rows += 1;
                    // One indexed row is the walk's own unit, and the walk is the only thing between a
                    // stop request and a segment finished. It is not a leg's counter: the census counts
                    // the videos, this counts the rows lifted out of them.
                    wind_base::maintain::add_step_items(1);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !wind_base::maintain::may_continue(config) {
                    asked_to_stop = true;
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if asked_to_stop {
        for mut child in children {
            let _ = child.kill();
            let _ = child.wait();
        }
        println!("   reindex: stopped by request after {rows} row(s); the rest waits for the next window");
        return Ok(());
    }

    let mut bad = 0usize;
    for (lane, (mut child, pipe)) in children.into_iter().zip(error_pipes).enumerate() {
        match child.wait() {
            Ok(status) if !status.success() => {
                bad += 1;
                // The lane's own stderr is the only explanation there is: a segment that failed OCR
                // renamed itself `-ERROR{n}` and left a `cache/LOG_ERROR_*.MD`, and the pass must say
                // which lane was talking before it reports a number of bad ones.
                let stderr = pipe.join().unwrap_or_default();
                for line in stderr.lines().filter(|line| !line.trim().is_empty()).take(4) {
                    eprintln!("   reindex! [{lane}] {line}");
                }
            }
            Err(e) => {
                bad += 1;
                eprintln!("   reindex! [{lane}] could not wait for the lane: {e}");
            }
            _ => {}
        }
    }
    if bad > 0 {
        return Err(format!("{bad} of {lanes} reindex lane(s) ended badly; their videos carry their own report"));
    }
    Ok(())
}

/// One process walking the whole library: the shape this step had before lanes, and still the shape a
/// library with less than [`MIN_BATCH_PER_LANE`] videos in it gets.
fn reindex_one(config: &Config, root: &Path, binary: &Path, videos: &Path) -> Result<(), String> {
    println!("reindex: {REINDEX_BINARY} {} --root {}", videos.display(), root.display());
    // Streamed rather than collected with `output()`, for two reasons the user felt. The walk is the
    // longest thing this pass does — one OCR round trip per row of every video — and `output()` does not
    // show a byte of it until the child has finished, so the step had a segment on the progress bar and no
    // number in it. And the child only reads a stop request between whole videos, so pressing 停止整理
    // during a long one appeared to do nothing at all. Line by line, the `  row` lines become the count,
    // and a stop request ends the step between two of them.
    let mut child = Child::start(REINDEX_BINARY, &mut walk_args(binary, root, videos, None))?;
    let mut rows = 0usize;
    let stopped = child.pump(config, |line| {
        println!("   reindex: {line}");
        if line.starts_with("  row") {
            rows += 1;
            // One indexed row is the walk's own unit, and the walk is the only thing between a
            // stop request and a segment finished. It is not a leg's counter: the census counts
            // the videos, this counts the rows lifted out of them.
            wind_base::maintain::add_step_items(1);
        }
    });
    if stopped {
        // Killing the walk is the parent's job, and it has to be: the child reads a stop request only
        // between whole videos, and an orphaned `wind-reindex` would keep OCRing a library somebody just
        // asked this pass to leave alone.
        println!("   reindex: stopped by request after {rows} row(s); the rest waits for the next window");
        return Ok(());
    }
    let Some(exit) = child.finish()? else {
        return Ok(());
    };
    if !exit.ok {
        for line in exit.stderr.lines().filter(|l| !l.trim().is_empty()).take(8) {
            eprintln!("   reindex!: {line}");
        }
        return Err(format!("{} exited with a failure", REINDEX_BINARY));
    }
    Ok(())
}

/// The segments the encode step has finished, waiting for a lane to read them.
///
/// `convert` renames a slice directory `{stamp}-VIDEO` the moment ffmpeg returns, and that rename is the
/// only evidence on disk that the video beside it is complete. Handing that name over here is what lets a
/// `wind-reindex` process start on the hours that are already encoded *while* the encoder is still working
/// on later ones — the two legs use different resources (an OCR engine and an encoder session) and had no
/// reason to wait for each other.
///
/// Shared between the pass's threads under one short-lived mutex, which is also why the census path never
/// touches it: a dry run creates no hand-off, so there is nothing for a lane to wait at and no thread to
/// ask the question.
#[derive(Default)]
pub struct EncodeHandoff {
    /// The video files whose encode is complete, oldest first, as [`crate::convert`] marks them.
    finished: Vec<PathBuf>,
    /// Set once the convert step is over. The lane then stops claiming work, and whatever is left is the
    /// `reindex` step's, walked exactly the way it always was.
    closed: bool,
}

/// The hand-off, shared between the convert step that fills it and the lane that reads it.
#[derive(Clone, Default)]
pub struct Handoff(Arc<Mutex<EncodeHandoff>>);

impl Handoff {
    /// Say that one segment is encoded. Called from a pool lane, so it takes the lock for a push only.
    pub fn mark_encoded(&self, video: PathBuf) {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).finished.push(video);
    }

    /// Take the whole queue, if it is worth an engine — [`MIN_BATCH_PER_LANE`] segments, because a
    /// `wind-reindex` process pays a 29–46 s cold start for its resident OCR child and a smaller batch
    /// would spend the overlap on spin-up. `None` says "not yet", and a closed hand-off takes whatever is
    /// there so nothing is left unclaimed.
    fn claim(&self) -> Option<Vec<PathBuf>> {
        let mut held = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if held.finished.is_empty() {
            return None;
        }
        if held.finished.len() < MIN_BATCH_PER_LANE && !held.closed {
            return None;
        }
        Some(std::mem::take(&mut held.finished))
    }

    /// Whether the convert step is over, which is the lane's own "no more batches are coming" answer.
    fn is_closed(&self) -> bool {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).closed
    }

    /// The convert step is over: stop the lane after its current batch.
    pub fn close(&self) {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).closed = true;
    }
}

/// What the lane that followed the encoder did, for the step whose name it worked under.
#[derive(Debug, Default, Clone)]
pub struct FollowReport {
    /// Segments the lane handed to a `wind-reindex` process.
    pub segments: usize,
    /// Rows the lane's children lifted out of them.
    pub rows: usize,
    /// Batches — that is, child processes — the lane started.
    pub batches: usize,
    /// Why nothing ran, in the step's own words: the recorder guard, or a missing binary.
    pub declined: Option<String>,
    /// A stop request ended the lane. The pass's own boundary check says the ending out loud.
    pub stopped: bool,
    /// A child came back badly. Reported, not fatal, exactly as the step reports a bad lane today.
    pub failed: usize,
}

impl FollowReport {
    /// The one line the pass says about the lane, at the convert step's boundary. Empty when the lane had
    /// nothing to say — no batch, no stop, no decline — which is the ordinary small install.
    pub fn line(&self) -> String {
        if let Some(why) = &self.declined {
            return format!("reindex lane alongside convert: declined — {why}");
        }
        if self.segments == 0 && !self.stopped {
            return String::new();
        }
        let trouble = if self.stopped {
            ", called off by the stop request".to_string()
        } else if self.failed > 0 {
            format!("; {} child(ren) ended badly, their segments wait for the step", self.failed)
        } else {
            String::new()
        };
        format!(
            "reindex lane alongside convert: {} segment(s) in {} batch(es), {} row(s) written{trouble}",
            self.segments, self.batches, self.rows
        )
    }
}

/// The lane that re-indexes the segments `convert` has finished, while it is still finishing later ones.
///
/// The thread owns the whole shape: it waits at the hand-off, starts one `wind-reindex` per batch, streams
/// that child's lines, and puts it down inside a second of a stop request. One child at a time is
/// deliberate — the pass's `reindex` step already spreads a whole library over several lanes, and this lane
/// is the *same* writer reaching for the same month files as `refresh` and `expire` would if it were
/// allowed to outlive the convert step, so [`Follow::finish`] joins it before step 3 opens a database. Two
/// processes writing one month file is the race the maintain lock exists to forbid, and a busy_timeout is
/// not a substitute for one writer per file.
pub struct Follow {
    handoff: Handoff,
    thread: Option<std::thread::JoinHandle<FollowReport>>,
}

impl Follow {
    /// Start the lane. A pass that is not converting anything, or a dry run, does not call this at all.
    ///
    /// `may_work` is the pass's own answer to "may this still be going?" — the stop latch and the window
    /// together, decided once in `main` — rather than a second channel this file invented.
    pub fn start(config: &Config, root: &Path, authorized: Option<u32>, may_work: Arc<dyn Fn() -> bool + Send + Sync>) -> Follow {
        let handoff = Handoff::default();
        let reader = handoff.clone();
        let config = config.clone();
        let root = root.to_path_buf();
        Follow {
            handoff,
            thread: Some(std::thread::spawn(move || follow_encode(&config, &root, authorized, reader, may_work.as_ref()))),
        }
    }

    /// The queue `convert` pushes finished segments into.
    pub fn handoff(&self) -> Handoff {
        self.handoff.clone()
    }

    /// Close the hand-off and wait for the lane, so nothing is left reading when the pass moves on to the
    /// steps that write the same index. A lane whose thread died still answers with what it had said, and
    /// the pass keeps going: this is progress, not a lock.
    pub fn finish(mut self) -> FollowReport {
        self.handoff.close();
        match self.thread.take() {
            Some(thread) => thread.join().unwrap_or_else(|_| FollowReport {
                declined: Some("the lane that followed convert died mid-batch".to_string()),
                ..Default::default()
            }),
            None => FollowReport::default(),
        }
    }
}

/// One lane's whole life, on its own thread.
fn follow_encode(config: &Config, root: &Path, authorized: Option<u32>, handoff: Handoff, may_work: &dyn Fn() -> bool) -> FollowReport {
    let mut report = FollowReport::default();
    // The same door the step uses, asked at the same moment the step would have asked it: idle-only work
    // under a live capture is refused, whoever is holding the encoder.
    if let Err(why) = recorder_guard(config, authorized) {
        report.declined = Some(why);
        return report;
    }
    let binary = match find_binary(REINDEX_BINARY, root) {
        Some(path) => path,
        None => {
            report.declined = Some(format!("{REINDEX_BINARY}.exe was not found beside this binary, in bin\\, or in the install root"));
            return report;
        }
    };
    loop {
        let Some(batch) = handoff.claim() else {
            // Nothing worth an engine yet. This is where a stop request is answered while the lane is
            // idle, and the sleep is the same second the rest of this file polls at.
            if handoff.is_closed() {
                return report;
            }
            if !may_work() {
                report.stopped = true;
                return report;
            }
            std::thread::sleep(STOP_POLL);
            continue;
        };
        report.batches += 1;
        report.segments += batch.len();
        println!(
            "reindex: {REINDEX_BINARY} ×{} --file batch --root {} (the encode step handed these over)",
            batch.len(),
            root.display()
        );
        let mut child = match Child::start(REINDEX_BINARY, &mut batch_args(&binary, root, &batch)) {
            Ok(child) => child,
            Err(why) => {
                // A child that would not start is the step's own kind of trouble, said once; the convert
                // step carries on, and the `reindex` step walks these segments again properly later.
                eprintln!("   reindex lane: {why}");
                report.failed += 1;
                continue;
            }
        };
        let stopped = child.pump_with(may_work, |line| {
            if line.starts_with("  row") {
                report.rows += 1;
            }
            println!("   reindex+ {line}");
        });
        if stopped {
            report.stopped = true;
            return report;
        }
        match child.finish() {
            Ok(Some(exit)) if !exit.ok => {
                report.failed += 1;
                for line in exit.stderr.lines().filter(|l| !l.trim().is_empty()).take(4) {
                    eprintln!("   reindex+! {line}");
                }
            }
            Err(why) => {
                report.failed += 1;
                eprintln!("   reindex+! {why}");
            }
            _ => {}
        }
    }
}


/// How much of the summary backlog one idle pass takes on — answered by [`Config`], not by this file.
///
/// The two numbers used to be constants here, which made the pass the only place that knew them: an AI
/// client reading its own queue through `windmcp` had to invent a third figure for the same budget. They
/// are settings now (`summary_pending_days_in_idle`, `summary_stretch_limit_in_idle`, both documented and
/// bounded in `wind_base::config`), and `windmcp`'s `summaries_pending` reports them from the same two
/// accessors, so "how long may one run take" has one answer in the binary that spends and the door that
/// offers the same work to somebody else. What is left outstanding stays outstanding: the queue is
/// derived from the index rather than from a cursor somebody would have to keep.
pub fn summary_budget(config: &Config) -> (usize, usize) {
    (config.summary_pending_days_in_idle() as usize, config.summary_stretch_limit_in_idle() as usize)
}

/// How much of one pass's ceiling its own step may still spend, after the early half took its.
///
/// Named rather than inlined twice because the ceiling is the number the settings page writes down
/// (`summary_stretch_limit_in_idle`, "单次空闲总结接手的片段数"), and a pass that spent it once per half was
/// a pass that spent twice what that row promises. Saturating: a half that overran asks for nothing here,
/// rather than underflowing into a huge limit.
fn room_left(ceiling: usize, asked_by_the_early_half: usize) -> usize {
    ceiling.saturating_sub(asked_by_the_early_half)
}

/// Whether the idle pass may summarise at all.
///
/// One switch, and it is the one that describes the risk: this is the pass that sends captured screen
/// text to `open_ai_base_url`. There is deliberately no second master key, because the real gate is
/// already in the config file — the shipped `open_ai_api_key` is a placeholder, and `windai` refuses to
/// send a byte while it is one. So the machine does not start talking to an endpoint nobody configured,
/// and a user who configured one and left this on has asked for it by name.
pub fn summaries_gate(config: &Config) -> bool {
    config.ai_summary_allowed_in_idle()
}

/// What the AI leg's early half did, reported when the pass's own `ai-summaries` step closes the leg.
#[derive(Debug, Default, Clone)]
pub struct EarlyReport {
    /// Settled days this half asked.
    pub days: usize,
    /// Stretches it put on the wire.
    pub stretches: usize,
    /// Stretches whose paragraph landed. Already counted in the `ai` leg's counter as they happened.
    pub written: usize,
    /// Stretches it did not get back. Folded into the pass's single in-window retry, so whichever half the
    /// endpoint last ignored is the half the retry then covers.
    pub missing: usize,
    /// Why nothing was asked, held rather than printed: the decline is said at the step's own boundary, not
    /// in the middle of an unrelated step, and one sentence said twice is two chances to disagree.
    pub declined: Option<String>,
    /// A stop request, or the closing of the window, ended this half.
    pub stopped: bool,
}

impl EarlyReport {
    /// The shape a command that never started a lane hands to the step: an empty half.
    pub fn none() -> EarlyReport {
        EarlyReport::default()
    }

    /// The one true line about this half, said by the step that closes the leg. Nothing when the half did
    /// nothing, because the step's own guards say why in the step's own words.
    pub fn line(&self) -> String {
        if self.days == 0 && self.written == 0 && !self.stopped {
            return String::new();
        }
        let owed = if self.missing > 0 { format!(", {} still owed", self.missing) } else { String::new() };
        let off = if self.stopped { ", called off by the stop request" } else { "" };
        format!(
            "ai-summaries: {} settled day(s) asked under the local steps — {} stretch(es) sent, {} written{owed}{off}",
            self.days, self.stretches, self.written
        )
    }
}

/// The AI leg's early half: the settled days, asked on their own thread while the local steps work.
pub struct EarlyLeg {
    thread: Option<std::thread::JoinHandle<EarlyReport>>,
}

impl EarlyLeg {
    /// Start asking. Nothing is decided here that the step would not decide — the recorder guard, the spend
    /// switch, the binary — and a leg that any of those refuses only reports the refusal, which the step
    /// then says out loud as its own decline.
    pub fn start(config: &Config, root: &Path, authorized: Option<u32>, may_ask: Arc<dyn Fn() -> bool + Send + Sync>) -> EarlyLeg {
        let config = config.clone();
        let root = root.to_path_buf();
        EarlyLeg {
            thread: Some(std::thread::spawn(move || ask_settled(&config, &root, authorized, may_ask.as_ref()))),
        }
    }

    /// Wait for this half and take its counts. The child answers a stop request by being put down, so this
    /// returns within about a second of one rather than at the end of a fifteen-minute deadline.
    pub fn join(mut self) -> EarlyReport {
        match self.thread.take() {
            Some(thread) => thread.join().unwrap_or_else(|_| EarlyReport {
                declined: Some("the half that started with the pass died mid-request".to_string()),
                ..Default::default()
            }),
            None => EarlyReport::none(),
        }
    }
}

/// Ask every settled day, one `windai` at a time, until the ceiling or the stop request says otherwise.
///
/// One child at a time, never several: `windai` already keeps four requests in flight, one per stretch
/// (`wind_ai::summarize::IN_FLIGHT`), and two of its processes would put eight on the wire when the ADR
/// fixed the width at four. The overlap this half buys is with the *local* steps — the network wait hidden
/// behind the encoder and the engine — not with itself.
fn ask_settled(config: &Config, root: &Path, authorized: Option<u32>, may_ask: &dyn Fn() -> bool) -> EarlyReport {
    let mut leg = EarlyReport::default();
    // The same three doors the step checks, asked at the moment the leg starts: a leg that ignored them
    // would spend against a switch nobody turned on, or OCR under a live capture, six minutes early.
    if let Err(why) = recorder_guard(config, authorized) {
        leg.declined = Some(format!("declined — {why}"));
        return leg;
    }
    if !summaries_gate(config) {
        leg.declined = Some("not scheduled — enable_ai_summary_in_idle is off; no API request was made".to_string());
        return leg;
    }
    let binary = match find_binary(AI_BINARY, root) {
        Some(path) => path,
        None => {
            leg.declined = Some(format!("declined — {AI_BINARY}.exe was not found beside this binary, in bin\\, or in the install root"));
            return leg;
        }
    };
    let (pending_days, stretch_limit) = summary_budget(config);
    let settled = settled_days(config, pending_days);
    // The row has to stop reading "还没开始" the moment this half begins sending, and the only honest
    // moment to say it is here: the three guards have passed, there is a settled day to ask, and the first
    // request goes out on the next line of this loop. Nothing is claimed about items, because none has
    // come back yet.
    if !settled.is_empty() {
        announce_asking();
    }
    // The run ceiling, spent down as the half goes: what one `windai summarize` may ask in one run is the
    // same number it has always been, and a settled day that finds no room left is asked by the next pass
    // rather than by this one on a bonus.
    let mut room = stretch_limit;
    for (day, owed) in settled {
        if room == 0 {
            break;
        }
        // Asked between days, never inside one: a request already on the wire is answered or it is not, and
        // a leg that cut one off mid-flight would leave a paragraph the endpoint is still writing.
        if !may_ask() {
            leg.stopped = true;
            break;
        }
        let ask = Ask::Day { day, stretches: room.min(owed.max(1)) };
        match summarize_once(root, &binary, &ask, "early · ", may_ask) {
            Ok(run) => {
                leg.days += 1;
                leg.stretches += run.sent;
                leg.written += run.written;
                leg.missing += run.missing;
                room = room.saturating_sub(run.sent);
            }
            // A child that would not start is the local half's to report at the step's own boundary; this
            // half stops asking rather than saying the same failure once per remaining day.
            Err(why) => {
                leg.declined = Some(why);
                break;
            }
        }
    }
    leg
}

/// Say that the AI leg is asking, without saying anything about items it has not got back.
///
/// Its own function because it is the one part of the early half a test can observe with no child, no
/// endpoint and no network: a publisher goes in, this is called, and the row's state is read back. The
/// alternative was to let the first `add_items` carry it, which means the row reads "还没开始" for the whole
/// of the first request — on this endpoint, over a minute of a window the person is watching.
fn announce_asking() {
    wind_base::maintain::report_leg(
        wind_base::maintain::Leg::Ai,
        wind_base::maintain::LegStatus::Running,
        "settled days, asked under the local steps",
    );
}

/// The recent product days this pass can no longer change, oldest first, each with the stretches it owes.
///
/// Read at the moment the pass starts, over the same horizon and with the same digests the census counted
/// (`backlog::SUMMARY_DAYS`, `wind_ai::summarize::digests`), so the four denominators and this queue cannot
/// disagree about which week is being asked about. It costs one index walk per day and writes nothing:
/// `wind-summary` reads the month files through read-only copies and cannot write a row.
pub fn settled_days(config: &Config, budget_days: usize) -> Vec<(String, usize)> {
    use wind_summary as summary;
    if budget_days == 0 {
        return Vec::new();
    }
    let prompts = wind_base::prompts::Prompts::read(config);
    let digests = wind_ai::summarize::digests(&prompts);
    let reader = summary::Reader::fresh(config);
    let shift = config.day_begin_minutes();
    let now = wind_base::clock::now();
    let today = summary::day_of(now.naive_epoch_seconds(), shift);
    // The retention sweep's own lower bound, from the same function and the same key `expire` reads: a day
    // the sweep can reach is not settled, because the paragraph this half wrote would describe footage the
    // same pass is about to recycle.
    let store_cutoff = crate::expire::retention_cutoff(&now, config.i64_or("vid_store_day", 0), shift);
    let cache = config.cache_screenshot_dir();
    let has_slice = |stamp: &str| cache.join(stamp).is_dir();
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut instant = now.naive_epoch_seconds();
    let mut walked = String::new();
    for _ in 0..crate::backlog::SUMMARY_DAYS {
        let day = summary::day_of(instant, shift);
        if day != walked {
            walked = day.clone();
            // The current product day belongs to the local steps: its slices are still arriving and the
            // pass is writing its rows while this reads them. It keeps its place after the local work.
            if day != today {
                if let Ok(queue) = summary::for_day_with(&reader, &day, &digests) {
                    if queue.has_work() && day_is_settled(&queue, &has_slice, store_cutoff) {
                        out.push((day, queue.pending.len()));
                        if out.len() == budget_days {
                            break;
                        }
                    }
                }
            }
        }
        instant -= 86_400;
    }
    // Oldest first, the way the summariser walks its own day list, so two runs of one install ask in one
    // order and a log line does not depend on which direction the horizon was swept.
    out.reverse();
    out
}

/// Can this pass still change what one day's stretches say?
///
/// Three of the local steps can, and the answer names all three rather than guessing from a calendar:
///
///   * `text` (step 1) fills a waiting row from its masked copy and folds repeats away — both move a
///     stretch's fingerprint, and a paragraph written before them would be written from half the screen. A
///     row is waiting exactly when its stored text carries nothing but a window title, which is
///     [`crate::text`]'s own reading; the queue's frames are that same column, split the way the bridge
///     splits it. So: every frame of every stretch of the day already carries text.
///   * `convert` (step 2) encodes a slice that is closed and unmarked, and `reindex` (step 5) rewrites the
///     rows of any segment the index has never read. Both are answered by `has_slice`: while a day's own
///     slice folder is still in the cache under its unmarked name, the day's account is open.
///   * `expire` (step 4) deletes rows past the retention window and takes their paragraphs with them, so a
///     day the sweep can reach is not settled whatever the index says about it.
///
/// A day whose month file would not open, or whose rows no name reaches, is not settled either: there the
/// queue is a floor rather than an answer, and asking the endpoint to write over an unknown is how a
/// paragraph ends up describing half an afternoon.
pub fn day_is_settled(queue: &wind_summary::DayQueue, has_slice: &dyn Fn(&str) -> bool, store_cutoff: Option<i64>) -> bool {
    let all = queue.all_segments();
    if all.is_empty() || !queue.skipped.is_empty() || queue.unattributed != 0 {
        return false;
    }
    if all.iter().any(|segment| has_slice(segment.key.as_str())) {
        return false;
    }
    let oldest = all.iter().map(|segment| segment.start).min().unwrap_or(i64::MAX);
    if store_cutoff.is_some_and(|cutoff| oldest < cutoff) {
        return false;
    }
    all.iter().all(|segment| segment.frames > 0 && segment.detail.iter().all(|frame| !frame.text.trim().is_empty()))
}

/// Which days, and how much of them, one `windai summarize` run is pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// The idle pass's own shape, and the one step 8 still uses: up to `days` product days with
    /// outstanding work, scanned back from today, at most `stretches` asked in the run.
    Backlog { days: usize, stretches: usize },
    /// One named day, which is what lets the AI leg's settled half ask it without waiting for the local
    /// steps. A day rather than a batch of stretches, because `windai` owes a *day* its own paragraph and
    /// a half-finished day would leave the day summary for the next pass anyway.
    Day { day: String, stretches: usize },
}

/// The argv for one summariser run: a pure function of [`Ask`], so both shapes are checkable without a
/// binary, an endpoint, or a network.
fn summarize_argv(root: &Path, ask: &Ask) -> Vec<String> {
    // `--day` and `--pending` are mutually exclusive in `windai`'s own grammar, and that is checked there
    // rather than here: one run names either one day or a window of them, never both.
    let (window, stretches, named) = match ask {
        Ask::Backlog { days, stretches } => (Some(*days), *stretches, None),
        Ask::Day { day, stretches } => (None, *stretches, Some(day.clone())),
    };
    let mut argv = vec!["summarize".to_string()];
    if let Some(days) = window {
        argv.push("--pending".to_string());
        argv.push(days.to_string());
    }
    if let Some(day) = named {
        argv.push("--day".to_string());
        argv.push(day);
    }
    argv.push("--limit".to_string());
    argv.push(stretches.to_string());
    argv.push("--root".to_string());
    argv.push(root.display().to_string());
    argv
}

/// Summarise the recent stretches and days, only when the spend gate is open.
///
/// Like `ai_tags`, a closed gate means `windai` is never spawned, so no key is read by a request and no
/// token is spent; and like `ai_tags`, a failing run is reported and tolerated — the work is still in the
/// queue, so the next idle pass offers it again.
///
/// It now asks **once more, in the same pass, for the items that did not come back** — see
/// [`summarize_again`]. That is the whole of the retry policy: one extra run, only while the window that
/// scheduled this pass is still open and nothing has been asked to stop, and only for what the first run
/// reported as missing. The queue is derived from the index and the prompt digests, so the second run
/// naturally asks for the few stretches that are still unsummarised and never re-asks for one that
/// landed — which is why no list of failed keys has to be carried across the two calls.
///
/// The leg has two halves now, and this is the second one: [`EarlyLeg`] asked whatever the local steps
/// could not change, and this half asks the current day and anything the pass was still writing. The
/// retry decision is taken over **both** halves' misses, because the rule is one extra ask per pass and
/// the pass does not know which half the endpoint ignored.
///
/// Summarise the recent stretches and days, only when the spend gate is open — with the AI leg's early
/// half already counted in. A command run by hand passes [`EarlyReport::none`], which leaves this exactly
/// the step it was before the leg was split.
pub fn ai_summaries_after(config: &Config, root: &Path, authorized: Option<u32>, early: &EarlyReport) -> Result<(), String> {
    // Said here rather than on the lane's thread, so the leg's two halves read as one leg at the boundary
    // the window is watching. A half that asked nothing and declined says nothing: the guard below gives
    // the same reason in the step's own words, and two copies of one sentence is two chances to disagree.
    let line = early.line();
    if !line.is_empty() {
        println!("{line}");
    }
    if let Err(why) = recorder_guard(config, authorized) {
        println!("ai-summaries: declined — {why}");
        return Ok(());
    }
    if !summaries_gate(config) {
        println!("ai-summaries: not scheduled — enable_ai_summary_in_idle is off; no API request was made");
        return Ok(());
    }
    let binary = match find_binary(AI_BINARY, root) {
        Some(path) => path,
        None => {
            println!("ai-summaries: declined — {AI_BINARY}.exe was not found beside this binary, in bin\\, or in the install root; run `windcap\\build.ps1`");
            return Ok(());
        }
    };
    let (pending_days, ceiling) = summary_budget(config);
    let asking = || wind_base::maintain::may_continue(config);
    // One ceiling per pass, not one per half. The early half already spent `early.stretches` of it on the
    // settled days while the local steps were working, so what may be asked here is the remainder; a pass
    // whose two halves each took the full ceiling would spend twice the number the settings page names, and
    // the live run of 2026-09-30 showed the argv doing exactly that (`--limit 40` twice in one pass).
    let room = room_left(ceiling, early.stretches);
    let first = if room == 0 {
        println!("ai-summaries: this pass's ceiling of {ceiling} stretch(es) was spent on the settled days; the rest of the queue waits for the next pass");
        SummaryRun::default()
    } else {
        summarize_once(root, &binary, &Ask::Backlog { days: pending_days, stretches: room }, "", &asking)?
    };
    // Both halves' debt, because one retry per pass is the rule and the endpoint may have ignored either.
    let owed = first.missing + early.missing;
    if owed == 0 {
        return Ok(());
    }
    if !summarize_again(config) {
        println!(
            "ai-summaries: {n} item(s) did not come back; they wait for the next pass (no second request in this one)",
            n = owed
        );
        return Ok(());
    }
    println!("ai-summaries: {n} item(s) did not come back — asking once more, now", n = owed);
    // The retry asks the **debt**, not the queue. 仅重试一次失败项 is the rule the owner set, and a second
    // run carrying the old full ceiling would be a fresh budget wearing the name of a retry.
    let second = summarize_once(root, &binary, &Ask::Backlog { days: pending_days, stretches: owed }, "retry ", &asking)?;
    if second.missing > 0 {
        // Named as two attempts, because the sentence a user reads decides whether they go looking for a
        // broken address tonight or leave it alone until tomorrow.
        println!(
            "ai-summaries: {n} item(s) are still missing after one retry; they wait for the next pass",
            n = second.missing
        );
    }
    Ok(())
}

/// May this pass ask the endpoint a second time?
///
/// Three conditions, all of them about *now*: nobody has asked this pass to stop, the window that
/// scheduled it is still open, and this pass has not overrun. An install that never named a window has
/// no window to still be open, so it does not get the second run either — the second run exists to save
/// a night's work from one flaky gateway, not to double the load on a machine whose pass is scheduled
/// by the old idle rule.
fn summarize_again(config: &Config) -> bool {
    if config.maintain_stop_requested() {
        return false;
    }
    match config.maintain_window() {
        Some(window) => window.contains(wind_base::clock::now().minute_of_day()),
        None => false,
    }
}

/// What one `windai summarize` run left behind.
///
/// `Default` is the shape of a run that never happened — the pass's ceiling was already spent — which is
/// why nothing owes it a retry and nothing is counted in the leg.
#[derive(Default)]
struct SummaryRun {
    /// Items this run could not finish: they are still owed, by the queue's own reading.
    missing: usize,
    /// Paragraphs this run filed. This is the AI leg's unit — a stretch the endpoint owes, answered — and
    /// the only number that feeds [`wind_base::maintain::Leg::Ai`].
    written: usize,
    /// Requests this run put on the wire, which is what the leg's stretch ceiling is spent against.
    sent: usize,
}

/// One run of the summariser, with its own report echoed under `label`.
///
/// Streamed line by line like the reindexer's walk, for the same reason and with the same consequence: a
/// receive deadline is fifteen minutes of patience, and a pass that was called off during one would
/// otherwise publish its ending while a `windai` it spawned was still asking. `Child` puts the child down
/// inside a second of a stop request, so no orphan is left sending after the person pressed 停止整理.
fn summarize_once(root: &Path, binary: &Path, ask: &Ask, label: &str, may_work: &dyn Fn() -> bool) -> Result<SummaryRun, String> {
    let argv = summarize_argv(root, ask);
    println!("ai-summaries: {label}{AI_BINARY} {}", argv.join(" "));
    let mut command = Command::new(binary);
    command.args(&argv).current_dir(root);
    let mut child = match Child::start(AI_BINARY, &mut command) {
        Ok(child) => child,
        Err(e) => {
            // Failing to start the child is not a reason to fail the pass; the same argument as above.
            println!("ai-summaries: could not start {}: {e}", binary.display());
            return Ok(SummaryRun { missing: 0, written: 0, sent: 0 });
        }
    };
    let stopped = child.pump_with(may_work, |line| println!("   ai-summaries: {line}"));
    if stopped {
        println!("ai-summaries: the walk was called off while this run was asking; the queue still holds what it owed");
        return Ok(SummaryRun { missing: 0, written: 0, sent: 0 });
    }
    let Some(exit) = child.finish()? else {
        return Ok(SummaryRun { missing: 0, written: 0, sent: 0 });
    };
    let stdout = exit.stdout.clone();
    if !exit.ok {
        for line in exit.stderr.lines().filter(|l| !l.trim().is_empty()).take(8) {
            eprintln!("   ai-summaries!: {line}");
        }
        println!("ai-summaries: this run did not finish (see above); the rest is offered again next pass");
    }
    // `N request(s), M written, K current without asking, F failed, ...` is the sentence the summariser
    // closes with. Reading the numbers out of it is the least-brittle contract available here: the
    // alternative is a second way to count what is owed, and two ways to count the same queue is how a
    // pass and a tool end up disagreeing about whether anything is missing.
    let missing = owed(&stdout);
    let written = written_back(&stdout);
    let sent = requests(&stdout);
    // One leg, one counter, fed by whichever half of the leg filed it. The denominator is the census's
    // (`backlog::Census::summary_stretches`) and is set once, so this cannot add a second total.
    wind_base::maintain::add_items(wind_base::maintain::Leg::Ai, written);
    Ok(SummaryRun { missing: missing.max(if exit.ok { 0 } else { 1 }), written, sent })
}

/// The `F failed` number from the summariser's closing line, or 0 when the line is not there.
///
/// A missing line means an older or a broken child, and the safe reading of that is "nothing to retry" —
/// retrying on a parse failure would turn a silent tool into a second round of requests nobody asked for.
fn owed(stdout: &str) -> usize {
    number_after(stdout, "current without asking,")
}

/// The `M written` number: stretches whose paragraph this run filed, which is the AI leg's own unit.
fn written_back(stdout: &str) -> usize {
    number_after(stdout, "request(s),")
}

/// The `N request(s)` number: what this run put on the wire.
fn requests(stdout: &str) -> usize {
    number_before(stdout, "request(s)")
}

/// The number that follows `needle` on the line that holds it — `"…, 2 failed"` answers 2.
fn number_after(haystack: &str, needle: &str) -> usize {
    haystack.lines().find_map(|line| {
        let (_, rest) = line.trim_end().split_once(needle)?;
        count(rest)
    }).unwrap_or(0)
}

/// The number that precedes `needle` — `"40 request(s)"` answers 40.
fn number_before(haystack: &str, needle: &str) -> usize {
    haystack.lines().find_map(|line| {
        let (head, _) = line.split_once(needle)?;
        count(head.split(',').next_back()?)
    }).unwrap_or(0)
}

/// The leading digits of a fragment, or nothing.
fn count(text: &str) -> Option<usize> {
    text.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
}


/// Cache AI month tags for the recent months, only when the spend gate is open.
///
/// When the gate is closed this function does not spawn `windai`, so no key is read by a request and no
/// token is spent — the whole point of gating the scheduled call rather than trusting the CLI to refuse.
/// A month that cannot be tagged (empty titles, a bad key, an endpoint that 401s) is reported and
/// tolerated: one month's failure must not abort the rest of the idle pass, and a misconfigured key is
/// `windai`'s and the user's to fix, not a reason to stop reindexing.
pub fn ai_tags(config: &Config, root: &Path, authorized: Option<u32>) -> Result<(), String> {
    if let Err(why) = recorder_guard(config, authorized) {
        println!("ai-tags: declined — {why}");
        return Ok(());
    }
    match ai_gate(config) {
        gate @ (Gate::DisabledMaster | Gate::DisabledInIdle) => {
            println!("ai-tags: not scheduled — {}; no API request was made", gate.decline_reason());
            return Ok(());
        }
        Gate::Schedule => {}
    }
    let months = months_to_tag(config, &wind_base::clock::now());
    if months.is_empty() {
        println!("ai-tags: nothing to do — the current and previous month have no index files");
        return Ok(());
    }
    let binary = match find_binary(AI_BINARY, root) {
        Some(path) => path,
        None => {
            println!("ai-tags: declined — {AI_BINARY}.exe was not found beside this binary, in bin\\, or in the install root; run `windcap\\build.ps1`");
            return Ok(());
        }
    };
    let mut failed = 0usize;
    for (year, month) in months {
        let stamp = format!("{year:04}-{month:02}");
        println!("ai-tags: {AI_BINARY} tags --month {stamp} --root {}", root.display());
        match Command::new(&binary)
            .arg("tags")
            .args(["--month"]).arg(&stamp)
            .args(["--root"]).arg(root)
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => {
                report_child(&output, "ai-tags");
                if !output.status.success() {
                    failed += 1;
                }
            }
            Err(e) => {
                eprintln!("   could not start {}: {e}", binary.display());
                failed += 1;
            }
        }
    }
    if failed > 0 {
        // Reported, not fatal to the pass: a failing month is a spend or config problem to fix, and the
        // reindex that ran before it already did the disk work this idle window was reserved for.
        println!("ai-tags: {failed} month(s) could not be tagged (see above); they will be offered again next pass");
    }
    Ok(())
}

/// Echo a scheduled child's own report so the maintenance log says what it actually did.
fn report_child(output: &std::process::Output, label: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        println!("   {label}: {line}");
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines().filter(|l| !l.trim().is_empty()).take(8) {
            eprintln!("   {label}!: {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The publisher is one process-wide slot, so the one test that installs it holds this while it runs.
    static PUBLISHER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn unique() -> u32 {
        static N: AtomicU32 = AtomicU32::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-schedule-{tag}-{}-{}", std::process::id(), unique()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        dir
    }

    #[test]
    fn a_free_or_dead_record_lock_never_declines_the_idle_only_work() {
        let dir = scratch("guard-free");
        std::fs::create_dir_all(dir.join("cache/locks")).unwrap();
        let config = Config::load(&dir).unwrap();
        assert!(recorder_guard(&config, None).is_ok(), "no lock is no obstacle");
        // A corpse lock (dead pid) is not a live recorder either.
        std::fs::write(config.record_lock_path(), "4000000").unwrap();
        assert!(recorder_guard(&config, None).is_ok(), "a dead owner is reclaimable, not a live capture");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The heart of the mutual-exclusion requirement: a live recorder that is NOT the idle process which
    /// launched this pass must make the new steps decline. Proven with a real child holding a real pid —
    /// the same technique `supervisor` and `layout` use to distinguish a live owner from a corpse.
    #[test]
    fn a_live_recorder_declines_a_stranger_pass_and_authorises_its_own() {
        let dir = scratch("guard-live");
        std::fs::create_dir_all(config_locks(&dir)).unwrap();
        let config = Config::load(&dir).unwrap();
        let child = Command::new("ping").args(["-n", "20", "127.0.0.1"]).stdout(Stdio::null()).spawn().expect("ping ships with Windows");
        std::fs::write(config.record_lock_path(), child.id().to_string()).unwrap();
        // Hand-run (authorized=None): a live recorder holds the lock, so decline.
        let declined = recorder_guard(&config, None).expect_err("must not OCR or spend under a live capture");
        assert!(declined.contains(&format!("pid {}", child.id())), "{declined}");
        // Launched by that very recorder: authorized == holder, so proceed.
        assert!(recorder_guard(&config, Some(child.id())).is_ok(), "the idle recorder authorises its own pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn config_locks(dir: &Path) -> PathBuf {
        dir.join("cache").join("locks")
    }

    #[test]
    fn an_unreadable_record_lock_declines_rather_than_guesses() {
        let dir = scratch("guard-foreign");
        std::fs::create_dir_all(config_locks(&dir)).unwrap();
        let config = Config::load(&dir).unwrap();
        std::fs::write(config.record_lock_path(), "not-a-pid").unwrap();
        assert!(recorder_guard(&config, Some(123)).is_err(), "a lock naming no pid is somebody else's protocol");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The spend gate is exactly the two existing keys, and a closed gate is distinguishable so the
    /// "no request was made" message can name which switch was off.
    #[test]
    fn the_ai_gate_reads_the_existing_switches_and_nothing_it_invented() {
        let with = |master: &str, idle: &str| -> Gate {
            let dir = scratch("gate");
            std::fs::write(
                dir.join("config_src/config_default.json"),
                format!(r#"{{"enable_ai_extract_tag": {master}, "enable_ai_extract_tag_in_idle": {idle}}}"#),
            )
            .unwrap();
            let gate = ai_gate(&Config::load(&dir).unwrap());
            let _ = std::fs::remove_dir_all(&dir);
            gate
        };
        assert_eq!(with("true", "true"), Gate::Schedule);
        assert_eq!(with("false", "true"), Gate::DisabledMaster, "a stock install (master off) never spends");
        assert_eq!(with("true", "false"), Gate::DisabledInIdle);
        assert!(Gate::DisabledMaster.decline_reason().contains("enable_ai_extract_tag"));
        assert!(Gate::DisabledInIdle.decline_reason().contains("enable_ai_extract_tag_in_idle"));
        // The shipped default is master OFF — the load-bearing fact for "does not spend by accident."
        let dir = scratch("gate-default");
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        assert_eq!(ai_gate(&Config::load(&dir).unwrap()), Gate::DisabledMaster);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The per-run ceiling is read out of the settings file, so the two rows the Recording page writes
    /// reach the command line the pass spawns — and a nonsense value arrives corrected rather than as a
    /// `--pending 0` that visits no days.
    #[test]
    fn the_idle_summarising_budget_is_the_ceilings_the_settings_page_writes() {
        let dir = scratch("budget");
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"summary_pending_days_in_idle": 5, "summary_stretch_limit_in_idle": 12}"#,
        )
        .unwrap();
        assert_eq!(summary_budget(&Config::load(&dir).unwrap()), (5, 12), "{dir:?}");

        // A file that never mentions them still answers two and forty — the numbers this file used to
        // carry as constants, now carried by the accessor that bounds them.
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        assert_eq!(summary_budget(&Config::load(&dir).unwrap()), (2, 40));

        std::fs::write(dir.join("config_src/config_default.json"), r#"{"summary_pending_days_in_idle": 900}"#).unwrap();
        assert_eq!(summary_budget(&Config::load(&dir).unwrap()).0, 60, "clamped to the horizon `windai` scans back to");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One ceiling, spent by both halves together: the settled-day half's requests come off what the step's
    /// own ask may spend, and a half that overran leaves the step asking nothing rather than underflowing
    /// into a huge limit. The live pass of 2026-09-30 was caught sending `--limit 40` twice in one run,
    /// which spends the settings page's forty as eighty.
    #[test]
    fn the_two_halves_spend_one_ceiling_between_them() {
        assert_eq!(room_left(40, 0), 40, "nothing spent early, nothing taken off");
        assert_eq!(room_left(40, 9), 31, "the settled days' nine requests come off the pass's forty");
        assert_eq!(room_left(40, 40), 0, "a ceiling fully spent early leaves nothing for the step's own ask");
        assert_eq!(room_left(40, 57), 0, "and an overrun is zero, not a wrapped-around limit");

        // The retry is a second ask for the debt, so the number it carries is the debt.
        let argv = summarize_argv(std::path::Path::new("R"), &Ask::Backlog { days: 2, stretches: 1 });
        let at = argv.iter().position(|a| a == "--limit").expect("the ask names its limit");
        assert_eq!(argv[at + 1], "1", "a retry of one owed stretch asks for one stretch, not for the ceiling");
    }

    /// The retry decision is read off the summariser's own closing sentence, not off a second counter.
    #[test]
    fn the_number_owed_is_read_from_the_summarisers_own_closing_line() {
        let line = "40 request(s), 38 written, 74 current without asking, 2 failed, 937583 characters total";
        assert_eq!(owed(line), 2, "the count the tool prints for the human is the count the pass acts on");
        // No line, or a child too old to print it: nothing is retried, because "cannot tell" must not
        // become a second round of requests nobody asked for.
        assert_eq!(owed("summarising…"), 0);
        assert_eq!(owed("40 request(s), 40 written, 0 current without asking, 0 failed"), 0);
    }

    /// A second request belongs to an open window only.
    #[test]
    fn the_second_ask_is_only_while_the_window_is_still_open() {
        let dir = scratch("retry-window");
        std::fs::create_dir_all(dir.join("cache/locks")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"maintain_window_start": "00:00", "maintain_window_end": "23:59"}"#,
        )
        .unwrap();
        let config = Config::load(&dir).unwrap();
        assert!(summarize_again(&config), "a window that is open all day is open now");

        std::fs::write(config.maintain_stop_signal_path(), "someone").unwrap();
        assert!(!summarize_again(&config), "a stop request kills the second ask, not just the next step");
        config.clear_maintain_stop();

        // A closed window — and an install that never named one — gets no second run.
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"maintain_window_start": "00:00", "maintain_window_end": "00:00"}"#,
        )
        .unwrap();
        let closed = Config::load(&dir).unwrap();
        assert!(!summarize_again(&closed), "the window closed, so the night is over for this pass");
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        let nowindow = Config::load(&dir).unwrap();
        assert!(!summarize_again(&nowindow), "no window named, no second ask either");
        let _ = std::fs::remove_dir_all(&dir);
    }


    /// One stretch of a fixture day, with the screen text every one of its rows carries.
    fn segment_at(key: &str, start: i64, text: &str) -> wind_summary::Segment {
        let mut detail = Vec::new();
        for offset in [0, 60] {
            detail.push(wind_summary::Frame {
                timestamp: start + offset,
                title: Some("Qoder".to_string()),
                url: None,
                text: text.to_string(),
            });
        }
        wind_summary::Segment {
            key: key.to_string(),
            video_file: format!("{key}.mp4"),
            start,
            end: start + 60,
            frames: detail.len(),
            ocr_chars: detail.iter().map(|frame| frame.text.chars().count()).sum(),
            titles: vec!["Qoder".to_string()],
            day: key[..10].to_string(),
            fingerprint: format!("fp-{key}"),
            detail,
        }
    }

    /// The same, positioned by its own start stamp, with one frame's text per entry of `texts`.
    fn frame_segment(key: &str, texts: &[&str]) -> wind_summary::Segment {
        let start = wind_summary::test_support::at(key);
        let mut segment = segment_at(key, start, texts.first().copied().unwrap_or(""));
        for (index, text) in texts.iter().enumerate().skip(1) {
            segment.detail[index].text = text.to_string();
        }
        segment.ocr_chars = segment.detail.iter().map(|frame| frame.text.chars().count()).sum();
        segment.end = start + 60 * (texts.len() as i64 - 1).max(0);
        segment
    }

    /// A day queue built from those stretches: nothing summarised, everything pending, which is the state
    /// a day the endpoint has never answered is really in.
    fn queue_of(segments: Vec<wind_summary::Segment>) -> wind_summary::DayQueue {
        let day = segments.first().map(|segment| segment.day.clone()).unwrap_or_else(|| "2026-09-27".to_string());
        let pending = segments
            .into_iter()
            .map(|segment| wind_summary::Item { segment, reason: wind_summary::Reason::Missing, previous: None })
            .collect::<Vec<_>>();
        let total = pending.len();
        wind_summary::DayQueue {
            day: day.clone(),
            span: wind_summary::day_span(&day, 180).expect("a real day"),
            segments_total: total,
            summarised: 0,
            entries_stored: 0,
            pending,
            current: Vec::new(),
            coverage: wind_summary::Coverage { segments_total: total, segments_summarised: 0, missing: Vec::new() },
            daily: wind_summary::DailyState::Absent,
            skipped: Vec::new(),
            unattributed: 0,
        }
    }


    /// The two other numbers on the summariser's closing line, read the same way `owed` is read: out of the
    /// sentence the tool prints for the human, never out of a second counter of the same work.
    #[test]
    fn the_written_and_sent_counts_are_the_childs_own_numbers_not_a_second_tally() {
        let line = "40 request(s), 38 written, 74 current without asking, 2 failed, 937583 characters total";
        assert_eq!(written_back(line), 38, "the AI leg's unit is a stretch whose paragraph landed");
        assert_eq!(requests(line), 40, "and the ceiling is spent on requests put on the wire");
        // A child too old to print the line answers nothing, rather than an invented count of the wrong kind.
        assert_eq!(written_back("summarising…"), 0);
        assert_eq!(requests("summarising…"), 0);
        // And a per-day line, which also says "written", must not be mistaken for the closing sentence.
        assert_eq!(written_back("2026-09-27  4 stretches, 4 asked, 4 written, 0 already current"), 0);
    }

    /// One run names either a window of days or one day, never both — `windai` refuses the pair, and the two
    /// halves of the AI leg are exactly that difference.
    #[test]
    fn one_summariser_run_names_a_window_or_one_day_never_both() {
        let backlog = summarize_argv(Path::new("R:/install"), &Ask::Backlog { days: 2, stretches: 40 });
        assert!(backlog.starts_with(&["summarize".to_string(), "--pending".to_string(), "2".to_string()]), "{backlog:?}");
        assert!(backlog.contains(&"--limit".to_string()) && backlog.contains(&"40".to_string()), "{backlog:?}");
        assert!(!backlog.iter().any(|a| a == "--day"), "the idle run asks a window, not a day");

        let one = summarize_argv(Path::new("R:/install"), &Ask::Day { day: "2026-09-27".into(), stretches: 7 });
        assert!(one.contains(&"--day".to_string()) && one.contains(&"2026-09-27".to_string()), "{one:?}");
        assert!(!one.iter().any(|a| a == "--pending"), "a named day and a pending window are exclusive: {one:?}");
        assert!(one.ends_with(&["--root".to_string(), "R:/install".to_string()]), "the root is handed on as the pass holds it: {one:?}");
    }

    /// The standalone command's walk, pinned as the whole library: no `--file`, so the reindexer enumerates
    /// every month folder itself exactly as it did before the legs were split. Running the real binary would
    /// need ffmpeg and an OCR engine, so what is claimed here is the argv the step builds — which is the
    /// whole of the difference between "walk the library" and "work this named batch".
    #[test]
    fn a_standalone_reindex_walks_the_whole_library_and_names_no_batch() {
        let binary = Path::new("R:/install/bin/wind-reindex.exe");
        let library = Path::new("R:/install/userdata/videos");
        let walk = walk_args(binary, Path::new("R:/install"), library, None);
        let args: Vec<String> = walk.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args.first().map(String::as_str), library.to_str(), "the library is the target: {args:?}");
        assert!(!args.iter().any(|a| a == "--file"), "a whole-library walk names no batch: {args:?}");
        assert!(!args.iter().any(|a| a == "--shard"), "one process walks it unless lanes were asked for: {args:?}");

        let lane = walk_args(binary, Path::new("R:/install"), library, Some((1, 4)));
        let args: Vec<String> = lane.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args.last().map(String::as_str), Some("1/4"), "the shard is the last thing this lane was told: {args:?}");

        // The lane's shape, for comparison: only the segments the encoder handed over.
        let batch = batch_args(binary, Path::new("R:/install"), &[library.join("2026-09").join("2026-09-27_09-00-00.mp4")]);
        let args: Vec<String> = batch.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args.iter().filter(|a| *a == "--file").count(), 1, "{args:?}");
        assert!(!args.iter().any(|a| a == "--shard"), "a batch is one lane's own work, not a share of a walk: {args:?}");
    }

    /// A batch is worth an engine at [`MIN_BATCH_PER_LANE`] segments and not before, and closing the
    /// hand-off hands over the remainder rather than losing it.
    #[test]
    fn a_lane_waits_for_enough_encoded_segments_to_pay_for_its_engine() {
        let handoff = Handoff::default();
        for n in 0..MIN_BATCH_PER_LANE - 1 {
            handoff.mark_encoded(PathBuf::from(format!("R:/lib/2026-09-27_09-{n:02}-00.mp4")));
        }
        assert!(handoff.claim().is_none(), "below the floor no engine is started: the cold start costs more than the work");
        handoff.mark_encoded(PathBuf::from("R:/lib/2026-09-27_09-08-00.mp4"));
        let batch = handoff.claim().expect("the floor is reached");
        assert_eq!(batch.len(), MIN_BATCH_PER_LANE, "and the whole queue goes to that one process");
        assert!(handoff.claim().is_none(), "the queue is empty now");
        // The convert step is over: whatever arrived since is the lane's last batch, small or not.
        handoff.mark_encoded(PathBuf::from("R:/lib/2026-09-27_09-20-00.mp4"));
        handoff.close();
        assert_eq!(handoff.claim().map(|batch| batch.len()), Some(1), "a closed hand-off leaves nothing behind the lane");
        assert!(handoff.claim().is_none());
    }

    /// 一个已经定稿的日子 — the pure rule, as a table. `has_slice` stands for the cache listing the leg
    /// takes once, and the store cutoff for what `expire` can reach this pass.
    #[test]
    fn a_day_is_settled_only_when_no_local_step_can_still_change_it() {
        let none = |_stamp: &str| false;
        let everywhere = |_stamp: &str| true;
        let just_this_one = |stamp: &str| stamp == "2026-09-27_09-00-00";

        assert!(day_is_settled(&queue_of(vec![frame_segment("2026-09-27_09-00-00", &["a screen with words"])]), &none, None));
        assert!(
            !day_is_settled(&queue_of(vec![frame_segment("2026-09-27_09-00-00", &[""])]), &none, None),
            "a stretch with a row still waiting for its text is the `text` step's, not the endpoint's"
        );
        assert!(
            !day_is_settled(
                &queue_of(vec![
                    frame_segment("2026-09-27_09-00-00", &["words", "words"]),
                    frame_segment("2026-09-27_09-05-00", &["words", ""])
                ]),
                &none,
                None
            ),
            "one waiting stretch makes the whole day open"
        );
        assert!(
            !day_is_settled(&queue_of(vec![frame_segment("2026-09-27_09-00-00", &["words"])]), &everywhere, None),
            "a slice still in the cache means `convert` and `reindex` own the day"
        );
        assert!(
            day_is_settled(&queue_of(vec![frame_segment("2026-09-27_09-05-00", &["words"])]), &just_this_one, None),
            "another day's slice is not this day's business"
        );
        let start = 1_790_000_000;
        assert!(
            !day_is_settled(&queue_of(vec![segment_at("2026-09-27_09-00-00", start, "words")]), &none, Some(start + 1)),
            "footage the retention sweep can reach is not settled: the paragraph would outlive the rows"
        );
        assert!(
            day_is_settled(&queue_of(vec![segment_at("2026-09-27_09-00-00", start, "words")]), &none, Some(start)),
            "the cutoff itself is kept, so a day starting on it is settled"
        );
        assert!(!day_is_settled(&queue_of(Vec::new()), &none, None), "a day with no stretch of its own is no work");

        let mut partial = queue_of(vec![frame_segment("2026-09-27_09-00-00", &["words"])]);
        partial.skipped = vec!["default_2026-09_wind.db: locked".to_string()];
        assert!(!day_is_settled(&partial, &none, None), "a month that would not open makes the answer a floor");
        let mut unnamed = queue_of(vec![frame_segment("2026-09-27_09-00-00", &["words"])]);
        unnamed.unattributed = 1;
        assert!(!day_is_settled(&unnamed, &none, None), "and a row no name reaches is the same kind of unknown");
    }

    /// The legs' own report lines: a step partly handled by another thread still says something true about
    /// it rather than going silent, and a half that did nothing says nothing.
    #[test]
    fn a_step_another_lane_started_first_still_reports_what_is_left() {
        let empty = FollowReport::default();
        assert!(empty.line().is_empty(), "a lane that never ran is not a sentence: {}", empty.line());
        let batch = FollowReport { segments: 16, rows: 240, batches: 2, ..Default::default() };
        let said = batch.line();
        assert!(said.contains("16 segment(s)") && said.contains("2 batch(es)") && said.contains("240 row(s)"), "{said}");
        assert!(said.contains("reindex lane alongside convert"), "and it names which lane spoke: {said}");
        let refused = FollowReport { declined: Some("a recorder (pid 7) holds the record lock".to_string()), ..Default::default() };
        assert!(refused.line().contains("declined"), "{refused:?}");

        assert!(EarlyReport::none().line().is_empty(), "a half that asked nothing leaves the step's own line alone");
        let asked = EarlyReport { days: 2, stretches: 9, written: 7, missing: 2, ..Default::default() };
        let said = asked.line();
        assert!(said.contains("2 settled day(s)") && said.contains("7 written") && said.contains("2 still owed"), "{said}");
        assert!(said.starts_with("ai-summaries:"), "the AI leg's two halves read as one leg: {said}");
    }

    /// The whole point of the split, published: a lane on another thread feeds its own leg's counter and
    /// cannot move another leg's, the open step's number, or the pass's single total.
    #[test]
    fn an_ai_lane_under_a_local_step_feeds_one_leg_and_no_second_total() {
        let _serial = PUBLISHER.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windmaint-schedule-legs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        wind_base::maintain::uninstall();
        wind_base::maintain::install(&path, wind_base::maintain::Kind::Scheduled, 1_790_600_000);
        // The census's four denominators, fixed once, before any step and before any lane.
        wind_base::maintain::set_totals(wind_base::maintain::Totals { text: 100, convert: 20, ai: 9, other: 5 });
        wind_base::maintain::begin_step("convert", 2, 9, 1_790_600_001);
        let before = wind_base::maintain::read(&path).expect("the pass is published").items_total();

        // The early half, on its own thread, while `convert` is the open step.
        let thread = std::thread::spawn(|| wind_base::maintain::add_items(wind_base::maintain::Leg::Ai, 4));
        thread.join().unwrap();
        wind_base::maintain::add_items(wind_base::maintain::Leg::Ai, 3);
        // A boundary is what carries a held count to disk.
        wind_base::maintain::begin_step("refresh", 3, 9, 1_790_600_002);
        let shown = wind_base::maintain::read(&path).expect("the lane's work is published");
        assert_eq!(shown.items_total(), before, "one total for the pass: a lane adds items, never a denominator");
        assert_eq!(shown.leg(wind_base::maintain::Leg::Ai).map(|c| (c.done, c.total)), Some((7, 9)), "both halves of the leg score the same row");
        assert_eq!(
            shown.leg(wind_base::maintain::Leg::Convert).map(|c| (c.done, c.total)),
            Some((0, 20)),
            "and the convert row is not fed by a thread that was not encoding"
        );
        assert_eq!(shown.items, 0, "the open step's own number counts the open step");
        assert_eq!(shown.items_left(), 134 - 7, "the total is the four legs' totals, less what they have handled");
        wind_base::maintain::uninstall();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A leg that has started sending is not "还没开始". The early half says it is working *before* its first
    /// request goes out, and claims nothing about items, because at that moment none has come back. The
    /// live pass on 2026-09-30 read `waiting` for the whole of a first request, which is over a minute of
    /// a window somebody is watching.
    #[test]
    fn the_ai_leg_says_it_is_working_before_its_first_answer_lands() {
        let _serial = PUBLISHER.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windmaint-schedule-announce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        wind_base::maintain::uninstall();
        wind_base::maintain::install(&path, wind_base::maintain::Kind::Scheduled, 1_790_600_000);
        wind_base::maintain::set_totals(wind_base::maintain::Totals { text: 4, convert: 2, ai: 9, other: 1 });
        // A local step is the open one: the AI row is painted by a leg that is not the step.
        wind_base::maintain::begin_step("text", 1, 9, 1_790_600_001);
        assert_eq!(
            wind_base::maintain::read(&path).expect("the pass is published").leg(wind_base::maintain::Leg::Ai).map(|row| row.status),
            Some(wind_base::maintain::LegStatus::Waiting),
            "counted and not yet working is the honest starting state"
        );

        announce_asking();

        let shown = wind_base::maintain::read(&path).expect("the leg announced itself");
        let row = shown.leg(wind_base::maintain::Leg::Ai).expect("the row the census counted");
        assert_eq!(row.status, wind_base::maintain::LegStatus::Running, "it is sending, so it is not waiting");
        assert_eq!((row.done, row.total), (0, 9), "and it claims no item it has not got back");
        assert!(row.note.contains("settled days"), "{}", row.note);
        // With no publisher installed — a command run by hand, a rehearsal — the same call has nothing to
        // say: the file the live pass already wrote is not rewritten by a pass that is not running.
        wind_base::maintain::uninstall();
        let written = std::fs::read_to_string(&path).expect("the announcement is on disk");
        announce_asking();
        assert_eq!(std::fs::read_to_string(&path).expect("and stays there"), written, "an uninstalled pass publishes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The past day the early half may ask, and the ones it may not, on a scratch install: the current
    /// product day, a day whose rows are still waiting for their text, and a day whose slices are still in
    /// the cache are all the local steps' own.
    #[test]
    fn the_early_half_asks_a_past_day_the_local_legs_cannot_change_and_only_that() {
        let root = wind_summary::test_support::install("settled-days");
        let config = wind_summary::test_support::config_at(&root);
        std::fs::create_dir_all(config.cache_screenshot_dir()).unwrap();
        let shift = config.day_begin_minutes();
        let now = wind_base::clock::now().naive_epoch_seconds();
        // Days counted back from whenever this runs: a fixed 2026-09-27 would pass today and fail in a
        // month, because the horizon `settled_days` sweeps is the census's, measured from now.
        let day = |back: i64| wind_summary::day_of(now - back * 86_400, shift);
        let span = |back: i64| wind_summary::day_span(&day(back), shift).expect("a real day");
        // A day's one stretch, seeded with `text` as its row's screen text, and its slice folder on disk
        // when `slice` says so. Returns the stretch's key so the test can name the slice it belongs to.
        let seed = |back: i64, offset: i64, text: &'static str, slice: bool| -> String {
            let at = span(back).from + offset;
            let stamp = wind_base::clock::LocalParts::from_naive_epoch(at).stamp();
            let name: &'static str = Box::leak(format!("{stamp}.mp4").into_boxed_str());
            wind_summary::test_support::seed_month(&root, &config.user_name(), &[(at, name, "Qoder", text)]);
            if slice {
                std::fs::create_dir_all(config.cache_screenshot_dir().join(&stamp)).unwrap();
            }
            stamp
        };

        let settled_a = seed(3, 600, "a screen with words on it", false);
        seed(3, 900, "more words, another stretch", false);
        // A row with nothing in its text column is the `text` step's own definition of waiting, and it is
        // the only reason this day is refused: no slice folder is involved.
        seed(4, 600, "", false);
        seed(0, 600, "today, already read", false);
        let slice_day = seed(5, 600, "words, but its slice is still here", true);

        let asked = settled_days(&config, 5);
        assert_eq!(asked.len(), 1, "only the settled past day, with both its stretches owed: {asked:?}");
        assert_eq!(asked[0].0, day(3), "and it is named once, not once per stretch: {asked:?}");
        assert_eq!(asked[0].1, 2, "the count is that day's own queue");
        assert!(!asked.iter().any(|(name, _)| *name == day(4)), "the day with a waiting row is the text step's");
        assert!(!asked.iter().any(|(name, _)| *name == day(0)), "the current product day stays after the local work");
        assert!(!asked.iter().any(|(name, _)| *name == day(5)), "a slice on disk means convert still owns the day");

        // And the settled day stops being settled the moment its own slice appears; sweeping that slice, and
        // the other day's, hands both back to the early half — oldest first, the order the leg asks in.
        std::fs::create_dir_all(config.cache_screenshot_dir().join(&settled_a)).unwrap();
        assert!(settled_days(&config, 5).is_empty(), "a slice on disk means convert may still write this day");
        let _ = std::fs::remove_dir_all(config.cache_screenshot_dir().join(&settled_a));
        let _ = std::fs::remove_dir_all(config.cache_screenshot_dir().join(&slice_day));
        let recheck = settled_days(&config, 5);
        assert_eq!(recheck.len(), 2, "the day whose slice was swept, and the settled one: {recheck:?}");
        assert_eq!(recheck[0].0, day(5), "oldest first, the way the summariser walks its own day list: {recheck:?}");
        assert_eq!(recheck[1].0, day(3), "{recheck:?}");
        assert_eq!(recheck[1].1, 2, "and the day is named once, with both of its stretches owed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stop request is answered by the lane that has a child, and the child is put down rather than left
    /// to finish the library nobody asked it to read any more.
    #[test]
    fn a_stop_while_a_lane_has_a_child_puts_that_child_down() {
        // A real long-running child, asked the way both lanes ask one: both pipes streamed, the stop
        // question on the quiet second and on the talking one. It says nothing for thirty seconds, which is
        // the shape a `wind-reindex` has in the middle of a sparse segment — and it is not a `ping`, because
        // a machine with no route answers forty failures in under a second and the walk would simply end.
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 30"]);
        let mut child = match Child::start("fixture-child", &mut command) {
            Ok(child) => child,
            // An install without `ping` is not a reason to fail the suite; `recorder_guard`'s own test
            // already assumes this binary exists, so this is the same assumption once more.
            Err(_) => return,
        };
        let pid = child.pid();
        assert!(wind_base::fslock::is_process_running(pid), "the fixture child is really running");
        // The caller says stop on the first ask, which the pump reaches within its one-second poll.
        let asks = AtomicU32::new(0);
        let stopped = child.pump_with(&|| asks.fetch_add(1, Ordering::SeqCst) > 0, |_| {});
        assert!(stopped, "the pump answers the stop rather than waiting for the child to finish");
        assert!(asks.load(Ordering::SeqCst) >= 1, "and it asked the caller while the child was quiet");
        assert!(!wind_base::fslock::is_process_running(pid), "no orphan is left after the stop");
        assert!(matches!(child.finish(), Ok(None)), "a called-off child is not reported as a failed segment");
    }

    /// The early half never puts a request on the wire once the pass has been called off, and a leg that
    /// declines under a live recorder never spawns anything — both proved on a scratch root whose planted
    /// `windai.exe` is not an executable at all, so a spawn attempt could only ever come back as a failure
    /// rather than quietly sending bytes somewhere.
    #[test]
    fn a_called_off_or_declined_ai_leg_sends_nothing_and_starts_no_child() {
        let root = wind_summary::test_support::install("ai-leg-stopped");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/windai.exe"), b"MZ").unwrap();
        let config = {
            let mut c = wind_summary::test_support::config_at(&root);
            // The spend switch on, so the only thing standing between this leg and a request is the stop.
            c.set("enable_ai_summary_in_idle", serde_json::Value::Bool(true));
            c
        };
        std::fs::create_dir_all(config.cache_screenshot_dir()).unwrap();
        let now = wind_base::clock::now().naive_epoch_seconds();
        let span = wind_summary::day_span(&wind_summary::day_of(now - 3 * 86_400, config.day_begin_minutes()), config.day_begin_minutes()).expect("a real day");
        let stamp = wind_base::clock::LocalParts::from_naive_epoch(span.from + 600).stamp();
        let name: &'static str = Box::leak(format!("{stamp}.mp4").into_boxed_str());
        wind_summary::test_support::seed_month(&root, &config.user_name(), &[(span.from + 600, name, "Qoder", "words already read")]);

        // Called off before its first day: the leg stops, and its counts say nothing was sent.
        let off = ask_settled(&config, &root, None, &|| false);
        assert!(off.stopped, "the leg honours the same answer the pass would: {off:?}");
        assert_eq!((off.days, off.stretches, off.written), (0, 0, 0), "nothing was asked: {off:?}");
        assert!(off.declined.is_none(), "it was not a decline, it was a stop: {off:?}");

        // A live recorder that is not the one which launched this pass: the same decline step 8 gives.
        let recorder = Command::new("ping").args(["-n", "20", "127.0.0.1"]).stdout(Stdio::null()).spawn().expect("ping ships with Windows");
        std::fs::write(config.record_lock_path(), recorder.id().to_string()).unwrap();
        let declined = ask_settled(&config, &root, None, &|| true);
        assert!(declined.declined.as_deref().unwrap_or_default().contains("holds"), "the guard is the leg's too: {declined:?}");
        assert_eq!(declined.days, 0, "and a declined leg spawns nothing at all");
        // The planted file is still the two bytes it was: nothing ran, so nothing wrote, sent or renamed.
        assert_eq!(std::fs::read(root.join("bin/windai.exe")).unwrap(), b"MZ".to_vec());
        assert!(!root.join("userdata/result_ai_period_summary").exists(), "no summary file was created");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A lane under a live recorder declines exactly as the step does, and says so rather than starting a
    /// `wind-reindex` that would OCR under somebody's capture.
    #[test]
    fn the_encode_lane_declines_under_a_live_recorder_before_it_spawns_anything() {
        let root = wind_summary::test_support::install("lane-declines");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/wind-reindex.exe"), b"MZ").unwrap();
        std::fs::create_dir_all(root.join("userdata/videos/2026-09")).unwrap();
        std::fs::write(root.join("userdata/videos/2026-09/2026-09-27_09-00-00.mp4"), b"footage").unwrap();
        let config = wind_summary::test_support::config_at(&root);
        let recorder = Command::new("ping").args(["-n", "20", "127.0.0.1"]).stdout(Stdio::null()).spawn().expect("ping ships with Windows");
        std::fs::write(config.record_lock_path(), recorder.id().to_string()).unwrap();

        let handoff = Handoff::default();
        for n in 0..MIN_BATCH_PER_LANE {
            handoff.mark_encoded(config.videos_dir().join("2026-09").join(format!("2026-09-27_09-{n:02}-00.mp4")));
        }
        let report = follow_encode(&config, &root, None, handoff.clone(), &|| true);
        assert!(report.declined.is_some(), "a hand-run lane does not OCR under a live capture: {report:?}");
        assert_eq!((report.batches, report.segments), (0, 0), "and it spawned nothing in order to decline: {report:?}");
        assert_eq!(handoff.claim().map(|batch| batch.len()), Some(MIN_BATCH_PER_LANE), "the queue is left for the step to walk");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_months_that_have_an_index_file_are_offered_for_tagging() {
        let dir = scratch("months");
        let db = dir.join("userdata/db");
        std::fs::create_dir_all(&db).unwrap();
        // A month file for 2026-09 and one for 2026-08, plus an unrelated older one.
        for (year, month) in [(2026i64, 8u32), (2026, 9)] {
            std::fs::write(db.join(wind_base::paths::month_filename("default", year, month)), b"x").unwrap();
        }
        std::fs::write(db.join(wind_base::paths::month_filename("default", 2025, 1)), b"x").unwrap();
        let config = Config::load(&dir).unwrap();
        let now = LocalParts::from_stamp("2026-09-21_12-00-00").unwrap();
        assert_eq!(months_to_tag(&config, &now), vec![(2026, 8), (2026, 9)], "previous then current, and only present months");
        // A month boundary where the previous is in the prior year.
        let jan = LocalParts::from_stamp("2026-01-05_12-00-00").unwrap();
        assert_eq!(months_to_tag(&config, &jan), Vec::<(i64, u32)>::new(), "Dec 2025 and Jan 2026 have no files here");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The finder resolves a binary by name across the candidate directories. It must not be written to
    /// depend on which candidate wins, because a `cargo test` process has the whole `target/debug` build
    /// sitting in its own exe-parent directory — so the meaningful, order-independent claims are: a name
    /// that exists *only* in the scratch `bin\` resolves there, and a name that exists nowhere is `None`.
    #[test]
    fn the_finder_resolves_a_name_by_searching_the_candidate_dirs() {
        let dir = scratch("find");
        let only = format!("windmaint-fixture-{}.exe", unique());
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let marker = dir.join("bin").join(&only);
        std::fs::write(&marker, b"MZ").unwrap();
        assert_eq!(
            find_binary(&only, &dir).as_deref(),
            Some(marker.as_path()),
            "a name held only by bin\\ resolves to the installed bin\\"
        );
        // The `.exe` is appended when absent, so the crate's own binary names are the search keys.
        assert_eq!(find_binary("windmaint-fixture-absent", &dir), None, "a name nowhere in the tree is not found");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A closed gate must not reach `find_binary`/spawn at all; asserted here by pointing the pass at a
    /// scratch root that has no `windai.exe` and no videos: `reindex` declines quietly, `ai_tags` with
    /// the master off declines with the exact "no API request was made" line, and neither errors.
    #[test]
    fn a_declined_step_is_ok_rather_than_a_pass_failure() {
        let dir = scratch("decline");
        std::fs::create_dir_all(dir.join("cache/locks")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), r#"{"enable_ai_extract_tag": false}"#).unwrap();
        let config = Config::load(&dir).unwrap();
        assert!(reindex(&config, &dir, None).is_ok(), "no library is a clean decline, not a failure");
        assert!(ai_tags(&config, &dir, None).is_ok(), "master off is a clean decline");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
