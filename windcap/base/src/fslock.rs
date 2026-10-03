//! Pid-carrying lock files, the only cross-process coordination this app has.
//!
//! Upstream uses the `filelock` package with the owning pid written as the file body, and treats
//! an existing lock as live only if `is_process_running(pid)` agrees. That is a weak protocol —
//! a recycled pid or a machine that rebooted mid-write leaves a lock nobody owns — so the native
//! version keeps the same on-disk shape (another process may still be a Python one) but decides
//! liveness from the process handle rather than from a `tasklist` scan.

use std::io::Write;
use std::path::{Path, PathBuf};

/// A lock that is currently held by this process. Dropping it removes the file.
#[derive(Debug)]
pub struct PidLock {
    path: PathBuf,
    owned: bool,
}

/// A lock file plus the verdict on whoever wrote it, so a caller can report *why* it refused to
/// start instead of the bare "another instance is running" the Python tray produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
    Free,
    HeldBy { pid: u32, alive: bool },
    /// The file exists but carries nothing parseable — a truncated write, or a foreign tool.
    Unreadable,
    Owned,
}

pub fn lock_state(path: &Path) -> LockState {
    match std::fs::read_to_string(path) {
        Ok(body) => match body.trim().parse::<u32>() {
            Ok(pid) if pid == std::process::id() => LockState::Owned,
            Ok(pid) => LockState::HeldBy { pid, alive: is_process_running(pid) },
            Err(_) => LockState::Unreadable,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LockState::Free,
        Err(_) => LockState::Unreadable,
    }
}

/// Whether this process may act as the only writer in the tree the lock guards.
///
/// The same two facts [`PidLock::acquire`] already judges — nobody holds it, or whoever held it is
/// gone — asked *without* taking it, for a caller that only needs to know whether it may look
/// around. Startup recovery is that caller: it must not steal a running recorder's lock in order to
/// decide whether the recorder's own leftovers are safe to collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Free, ours, or a corpse whose owner is gone: the condition `acquire` would proceed on.
    Available,
    /// A live process other than us holds it. `acquire` refuses this one, and so must its readers.
    HeldByLive(u32),
    /// A lock that names no parseable pid. Reported separately rather than folded into
    /// `Available`, because the thing that writes an unreadable lock is a *foreign* recorder — and
    /// "probably nobody is home" is not a verdict worth destroying a recording over.
    Unclear,
}

pub fn availability(path: &Path) -> Availability {
    match lock_state(path) {
        LockState::Free | LockState::Owned | LockState::HeldBy { alive: false, .. } => Availability::Available,
        LockState::HeldBy { pid, alive: true } => Availability::HeldByLive(pid),
        LockState::Unreadable => Availability::Unclear,
    }
}

impl PidLock {
    /// Take the lock, clearing a stale one whose owner is gone. `Err` carries the reason.
    pub fn acquire(path: &Path) -> Result<PidLock, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        match Self::try_write(path) {
            Ok(()) => return Self::owned(path),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(format!("{}: {e}", path.display()));
            }
            Err(_) => {}
        }

        match lock_state(path) {
            LockState::Owned => Self::owned(path),
            LockState::HeldBy { pid, alive: true } => {
                Err(format!("{} is held by running process {pid}", path.display()))
            }
            LockState::HeldBy { pid, alive: false } => {
                // The owner is gone; the file is a corpse. Remove it once and retry, never loop.
                let _ = std::fs::remove_file(path);
                Self::try_write(path).map_err(|e| {
                    format!("could not reclaim stale lock {} (dead pid {pid}): {e}", path.display())
                })?;
                Self::owned(path)
            }
            LockState::Unreadable => Err(format!("{} exists but names no pid; refusing to take it", path.display())),
            LockState::Free => {
                // Raced with another starter: it won.
                Err(format!("{} was claimed by another process", path.display()))
            }
        }
    }

    fn try_write(path: &Path) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        write!(file, "{}", std::process::id()).and_then(|_| file.flush())
    }

    fn owned(path: &Path) -> Result<PidLock, String> {
        Ok(PidLock { path: path.to_path_buf(), owned: true })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Refresh the body so a monitor can see the owner is still alive. Cheap; call on a tick.
    pub fn touch(&self) {
        if self.owned {
            let _ = std::fs::write(&self.path, std::process::id().to_string());
        }
    }

    /// Release explicitly. Dropping does the same; this exists so a clean shutdown can report.
    pub fn release(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
            self.owned = false;
        }
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Is a pid alive? `OpenProcess` + `GetExitCodeProcess`: a still-running process reports
/// `STILL_ACTIVE`, which is the same test the Python helper performs without the process-table walk.
pub fn is_process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        #[allow(non_snake_case)]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
            fn GetExitCodeProcess(handle: *mut core::ffi::c_void, code: *mut u32) -> i32;
            fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
}

/// Is a *directory* lock currently claimed by a live process?
///
/// `windmaint` claims `cache\locks\LOCK_MAINTAIN` by creating the directory and writing its pid into
/// a `PID` child — because [`crate::fslock::PidLock`] cannot model a directory at all, given one its
/// `create_new` open fails and the state read reports `Unreadable`, i.e. it refuses forever. What
/// makes this a different question from [`availability`] is what a finished pass leaves behind: the
/// release removes its own `PID` and rmdir's only a directory *it* created, so a pass that reclaimed
/// a corpse — or an install whose tray swept a Python container empty — ends with the directory
/// still there and nothing in it. An empty container is therefore the ordinary shape of *idle*, and
/// answering this question with [`Path::exists`] turns it into a claim that never expires.
///
/// That is not a hypothetical: it is how a reader freezes on its own snapshot, so the two report
/// paths that describe the lock to a human (`windsetup doctor`, the tray's) already distinguish an
/// abandoned container from a live claim. This is that distinction, in one place, for the callers
/// that *act* on it.
///
/// A `PID` that exists but names no parseable pid counts as claimed. That is somebody else's
/// protocol, [`PidLock::acquire`] refuses to take it, and a reader must not be more confident about
/// it than the lock's owner is.
pub fn directory_lock_claimed(dir: &Path) -> bool {
    match std::fs::read_to_string(dir.join("PID")) {
        Ok(body) => match body.trim().parse::<u32>() {
            Ok(pid) => is_process_running(pid),
            Err(_) => true,
        },
        // No claim to read: no container yet, or a container with no owner inside it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// `LockFile` is the alias callers that do not care about ownership semantics use: "may I start?".
pub type LockFile = LockState;

// ---- the window's "come back" signal ------------------------------------------------------------
//
// A window that hides itself instead of exiting is still the tray's child, so the tray knows it is
// running and cannot spawn a second one — and a user who double-clicks the tray icon wants the hidden
// window back, not a duplicate and not a balloon explaining why nothing happened. Neither process can
// see inside the other, so the request is written where both of them look: one file, consumed by
// whoever was asked.

/// Ask a hiding window to come back. The body is the asking pid, so a log can say who raised it; the
/// file's existence is the whole message.
pub fn request_show(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(path, format!("{}", std::process::id())).map_err(|e| format!("{}: {e}", path.display()))
}

/// Leave a one-shot request for another process, which consumes it with [`take_show_request`].
///
/// The same file protocol the tray's window-raise uses, under a name that says nothing about windows:
/// a request that survives until somebody reads it, one reader, and no second channel to keep in sync
/// with the lock conventions this install already has.
pub fn write_signal(path: &Path) -> Result<(), String> {
    request_show(path)
}

/// Has a show request been left for this window? Consumes it, so the next poll does not raise the
/// window again an hour later. A file that cannot be removed is still a consumed request: the window
/// came forward, and a leftover in `cache/locks` costs nothing but a repeat raise.
pub fn take_show_request(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let _ = std::fs::remove_file(path);
    true
}

// ---- what the recorder is doing right now ------------------------------------------------------
//
// "Is a recording running?" has never meant what the tray icon is supposed to mean. The recorder holds
// its lock for its whole life, including while it is deliberately not capturing — the screen has not
// changed, the session is locked, the machine slept — and those are the ordinary hours of a tool that
// records a screen. So a tray that reads the lock alone says 正在记录 through a night of nothing
// happening, and says 暂停记录 when no recorder is running at all, and the user cannot tell the two
// apart from the icon. This file is the third answer: the recorder says what it is doing, and the tray
// reads it.

/// What the capture loop did on its last tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// A frame was grabbed and the segment is growing.
    Capturing,
    /// Nothing new to grab: the screen has been unchanged past the configured wait.
    ScreenUnchanged,
    /// The session will not hand over a picture — locked, or not the input desktop.
    SessionLocked,
    /// The machine slept through the tick, so the interval it covers is not footage.
    SleepDrift,
}

impl Capture {
    pub fn as_str(self) -> &'static str {
        match self {
            Capture::Capturing => "capturing",
            Capture::ScreenUnchanged => "screen-unchanged",
            Capture::SessionLocked => "session-locked",
            Capture::SleepDrift => "sleep",
        }
    }

    /// Read back what was published. Anything unrecognisable is `None` — a tray must not guess a state
    /// out of a half-written or foreign file, and "I cannot tell" is already a thing it can say.
    pub fn parse(text: &str) -> Option<Capture> {
        match text.trim() {
            "capturing" => Some(Capture::Capturing),
            "screen-unchanged" => Some(Capture::ScreenUnchanged),
            "session-locked" => Some(Capture::SessionLocked),
            "sleep" => Some(Capture::SleepDrift),
            _ => None,
        }
    }
}

/// Publish what the recorder is doing, and report whether it changed.
///
/// Written on transition only — the caller holds the previous value and skips the call when nothing
/// moved — because the tick is every few seconds and this file is read by a process that only cares
/// when the answer differs.
pub fn publish_capture(path: &Path, state: Capture) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(path, state.as_str()).map_err(|e| format!("{}: {e}", path.display()))
}

/// What the recorder last said, or `None` when nothing was ever published.
pub fn read_capture(path: &Path) -> Option<Capture> {
    Capture::parse(&std::fs::read_to_string(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_capture_state_reads_back_or_refuses_to_be_guessed() {
        let path = temp_lock("capture.MD");
        for state in [Capture::Capturing, Capture::ScreenUnchanged, Capture::SessionLocked, Capture::SleepDrift] {
            publish_capture(&path, state).expect("published");
            assert_eq!(read_capture(&path), Some(state), "{}", state.as_str());
        }
        std::fs::write(&path, "who knows").unwrap();
        assert_eq!(read_capture(&path), None, "a foreign body is not a state");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_state_that_was_never_published_is_absent_rather_than_capturing() {
        // The default matters: a missing file must not read as "recording", which is the bug this file
        // exists to fix.
        let path = temp_lock("never-published.MD");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_capture(&path), None);
    }

    fn temp_lock(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-fslock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn a_free_path_is_free() {
        let path = temp_lock("free.MD");
        let _ = std::fs::remove_file(&path);
        assert_eq!(lock_state(&path), LockState::Free);
    }

    #[test]
    fn acquiring_then_dropping_leaves_nothing_behind() {
        let path = temp_lock("owned.MD");
        let _ = std::fs::remove_file(&path);
        {
            let lock = PidLock::acquire(&path).expect("acquired");
            assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), std::process::id().to_string());
            assert_eq!(lock_state(&path), LockState::Owned);
            drop(lock);
        }
        assert!(!path.exists(), "Drop must remove the file");
    }

    #[test]
    fn a_dead_owner_is_reclaimed() {
        let path = temp_lock("stale.MD");
        // A pid no live process can hold: the corpse of an interrupted run must not wedge startup.
        std::fs::write(&path, "4000000").unwrap();
        let lock = PidLock::acquire(&path).expect("stale lock reclaimed");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), std::process::id().to_string());
        drop(lock);
    }

    /// The refusal path is the one that protects a recording session from a second recorder, so it
    /// is tested against a genuinely live owner rather than an assumed-reserved pid.
    #[test]
    fn a_live_owner_is_never_stolen() {
        let path = temp_lock("live.MD");
        let _ = std::fs::remove_file(&path);
        let child = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ping is available on every Windows install");
        std::fs::write(&path, child.id().to_string()).unwrap();

        let err = PidLock::acquire(&path).expect_err("must not steal a live owner");
        assert!(err.contains(&format!("running process {}", child.id())), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), child.id().to_string());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unparseable_body_is_refused_not_deleted() {
        let path = temp_lock("junk.MD");
        std::fs::write(&path, "streamlit, not a pid").unwrap();
        assert_eq!(lock_state(&path), LockState::Unreadable);
        assert!(PidLock::acquire(&path).is_err());
        assert!(path.exists(), "a lock we do not understand must survive");
    }

    #[test]
    fn our_own_pid_is_reported_alive() {
        assert!(is_process_running(std::process::id()));
        assert!(!is_process_running(0));
        assert!(!is_process_running(4_000_000));
    }

    /// Every state `acquire` is willing to act on must read as available to a caller that only wants
    /// to know whether it may look around — and the two states `acquire` refuses must not. This is
    /// the gate the recorder's startup recovery stands behind, so a verdict that drifts from the one
    /// `acquire` reaches is a recovery pass that either duplicates work or refuses to do any.
    /// The show request is a message, not a lock: writing one must not fail because the directory does
    /// not exist yet on a fresh install, and reading one must leave nothing behind for the next reader.
    #[test]
    fn a_show_request_is_written_making_its_own_directory_and_taken_once() {
        let dir = std::env::temp_dir().join(format!("windcap-show-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let signal = dir.join("locks").join("WINDOW_SHOW.MD");

        assert!(!take_show_request(&signal), "nothing asked yet");
        request_show(&signal).expect("the writer makes the directory it needs");
        assert!(signal.exists(), "the request is on disk");
        assert!(take_show_request(&signal), "the window takes it");
        assert!(!take_show_request(&signal), "and only once — an old request must not steal focus later");
        assert!(!signal.exists(), "the file does not outlive its message");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn availability_agrees_with_what_acquire_would_do() {
        let path = temp_lock("availability.MD");

        let _ = std::fs::remove_file(&path);
        assert_eq!(availability(&path), Availability::Available, "no lock at all");

        // A corpse: `acquire` removes it and takes the field, so a reader may not be blocked by it.
        std::fs::write(&path, "4000000").unwrap();
        assert_eq!(availability(&path), Availability::Available);

        // Ours: holding the lock is the strongest possible answer to "may I act as the only writer".
        let lock = PidLock::acquire(&path).expect("the corpse is reclaimable");
        assert_eq!(availability(&path), Availability::Available);
        drop(lock);

        let child = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ping is available on every Windows install");
        std::fs::write(&path, child.id().to_string()).unwrap();
        assert_eq!(availability(&path), Availability::HeldByLive(child.id()));
        assert!(PidLock::acquire(&path).is_err(), "and acquire refuses the same thing");

        std::fs::write(&path, "streamlit, not a pid").unwrap();
        assert_eq!(availability(&path), Availability::Unclear, "a lock we cannot read is not an invitation");
        let _ = std::fs::remove_file(&path);
    }

    /// The whole truth table of the one question a reader asks before it refreshes its snapshot: is
    /// somebody maintaining right now? Every row here is a state a real install reaches, and the two
    /// `false` rows are the ones that were being answered `true` — which is a frozen window.
    #[test]
    fn a_directory_lock_is_claimed_only_by_a_live_pid_inside_it() {
        let dir = std::env::temp_dir().join(format!("windcap-dirlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(!directory_lock_claimed(&dir.join("absent")), "no container at all is idle");

        let empty = dir.join("LOCK_MAINTAIN");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(!directory_lock_claimed(&empty), "the empty container a finished pass leaves is idle");

        std::fs::write(empty.join("2026-09-22_03-00-00.md"), b"python holds this").unwrap();
        assert!(
            !directory_lock_claimed(&empty),
            "a Python per-video marker is not a native claim, and it must survive either way"
        );
        assert!(empty.join("2026-09-22_03-00-00.md").exists(), "asking never deletes");

        std::fs::write(empty.join("PID"), "4000000").unwrap();
        assert!(!directory_lock_claimed(&empty), "a corpse whose owner is gone claims nothing, as `availability` judges a file lock");

        let mut child = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ping ships with every Windows install");
        std::fs::write(empty.join("PID"), child.id().to_string()).unwrap();
        assert!(directory_lock_claimed(&empty), "a running process is the only thing that claims it");

        std::fs::write(empty.join("PID"), "streamlit, not a pid").unwrap();
        assert!(directory_lock_claimed(&empty), "a claim we cannot read is refused, not disbelieved");

        let _ = child.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
