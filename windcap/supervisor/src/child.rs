//! Spawning, signalling and reaping the processes the tray supervises.
//!
//! The tray's whole relationship with the recorder is two facts, inherited from `main.py`: it is a
//! foreground process that ends its current segment on `CTRL_BREAK_EVENT`, and it is in a process
//! group of its own so that signal can be aimed at it alone. Everything here exists to preserve
//! those two facts across the change of host language — in particular the graceful stop is never
//! quietly upgraded into a kill, because a killed recorder loses the segment it was writing.
//!
//! The signal path is the one real difference from Python. `subprocess.send_signal` works because
//! `main.py` shares a console with its child; a `windows_subsystem` binary has no console to share,
//! so the supervisor has to *attach* to the child's console before it can generate an event in it.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::ffi;
use crate::layout::LogPair;
use crate::native::Spawn;

/// How long a supervised recorder gets to close its current segment before it is killed. Upstream's
/// `RECORDING_STOP_TIMEOUT`, and the number quoted in the message shown when it was not enough.
pub const GRACEFUL_STOP: Duration = Duration::from_secs(5);
/// How often a running child is checked, and therefore the worst-case delay before the tray notices
/// the user closed the interface window by hand.
pub const TICK: Duration = Duration::from_millis(1000);

/// How a stop attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// The child closed its segment and exited after the break.
    Graceful { code: Option<i32> },
    /// `CTRL_BREAK_EVENT` was delivered and ignored, so the process was terminated instead. The user
    /// is told, because their last minutes of recording are the price.
    KilledAfterTimeout,
    /// The signal could not be delivered at all — no console to attach to. Killed, and reported.
    KilledAfterSignalFailure { reason: String },
    /// It had already gone: the user closed the window, or the process crashed.
    AlreadyGone,
    /// Killed on request, which is how a window is stopped.
    Terminated,
}

impl Stopped {
    /// Did the stop cost the user anything? `Some(text)` is what the balloon says.
    pub fn failure(&self) -> Option<String> {
        match self {
            Stopped::KilledAfterTimeout => Some("Failed to exit the recording service gracefully. Killing it.".to_string()),
            Stopped::KilledAfterSignalFailure { reason } => Some(format!(
                "Failed to signal the recording service ({reason}); killed it without closing the segment."
            )),
            _ => None,
        }
    }
}

/// A process under supervision.
#[derive(Debug)]
pub struct Child {
    inner: std::process::Child,
    /// What it is, for the messages that have to name the command that failed.
    pub line: String,
    /// Where its output went, so a failure message can point at a file.
    pub logs: LogPair,
}

impl Child {
    /// The process id the OS gave this child.
    ///
    /// Needed because a supervised *server* announces itself by pid, not by a lock it writes
    /// itself: the tray is the only thing that knows whether the bridge is up, and `windsvc doctor`
    /// runs in a different process and cannot ask. Handing the pid out is what lets the tray leave
    /// the same pid-in-a-file answer the recorder already gives.
    pub fn pid(&self) -> u32 {
        self.inner.id()
    }

    /// The exit code, once the child has actually exited. Reaping happens here: an interface window
    /// dies the instant the user closes it and nobody tells the tray, so without this the menu would
    /// go on offering "Stop" for a process that is already gone.
    pub fn exited(&mut self) -> Option<i32> {
        match self.inner.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            // A failed `try_wait` means the handle is unusable, which cannot happen while this
            // struct owns it. Reporting "not exited" keeps a transient error from declaring a
            // segment closed.
            Ok(None) | Err(_) => None,
        }
    }

    /// Send `CTRL_BREAK_EVENT` to this child's process group.
    ///
    /// `GenerateConsoleCtrlEvent` only reaches a console the calling process shares, and windsvc has
    /// no console of its own, so the order is `AttachConsole(pid)` → raise the break against the
    /// group id (equal to the pid, because the child was created with `CREATE_NEW_PROCESS_GROUP`) →
    /// `FreeConsole`. Detaching immediately is safe: the event is already in the child's console
    /// input queue, and the break is addressed to the child's group alone, so the tray does not
    /// receive its own signal.
    ///
    /// The precondition is that *this* process holds no console — `AttachConsole` answers
    /// `ERROR_ACCESS_DENIED` to anything that does — which is what `windows_subsystem = "windows"`
    /// plus `main`'s rule that only `doctor` and the usage line ever attach the parent console
    /// guarantee. `FreeConsole` also invalidates our standard handles afterwards, which is why every
    /// diagnostic the tray has goes into a balloon and why `doctor` re-reads the world from disk.
    fn signal_break(&self) -> Result<(), String> {
        let pid = self.inner.id();
        unsafe {
            if ffi::AttachConsole(pid) == ffi::FALSE {
                return Err(format!("cannot attach to the console of pid {pid}: {}", ffi::last_os_error()));
            }
            let sent = ffi::GenerateConsoleCtrlEvent(ffi::CTRL_BREAK_EVENT, pid);
            ffi::FreeConsole();
            if sent == ffi::FALSE {
                return Err(format!("GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, {pid}): {}", ffi::last_os_error()));
            }
        }
        Ok(())
    }

    /// Stop the recorder: break, wait up to [`GRACEFUL_STOP`], and only then kill.
    pub fn stop_gracefully(mut self) -> Stopped {
        if self.exited().is_some() {
            return Stopped::AlreadyGone;
        }
        if let Err(reason) = self.signal_break() {
            self.kill();
            return Stopped::KilledAfterSignalFailure { reason };
        }
        if wait_for(&mut self.inner, GRACEFUL_STOP) {
            Stopped::Graceful { code: self.inner.wait().ok().and_then(|status| status.code()) }
        } else {
            self.kill();
            Stopped::KilledAfterTimeout
        }
    }

    /// Stop the interface. A window has no graceful protocol to speak — it is not being asked to
    /// close a file — so the only courtesy here is not killing one that already went away.
    pub fn stop_forced(mut self) -> Stopped {
        if self.exited().is_some() {
            return Stopped::AlreadyGone;
        }
        self.kill();
        Stopped::Terminated
    }

    fn kill(&mut self) {
        let _ = self.inner.kill();
        let _ = self.inner.wait();
    }
}

/// Poll for exit until `budget` runs out. `true` means the process stopped by itself.
fn wait_for(child: &mut std::process::Child, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Start a supervised process with stdout and stderr redirected into `logs` — truncated on every
/// start, as upstream truncates them — in its own process group and with no console window.
///
/// The caller picks the `logs` pair, and that is the whole of what a process is: `windrec`, `winduiweb`
/// and `windmcp` differ in what they do with the handles, not in how they are started, and the one
/// thing that used to be recorded here — which *implementation* an interface launch had started —
/// stopped being a question when there was only one implementation left to launch.
///
/// `Err` always names the command and the OS error, because "nothing happened" is the failure mode
/// this tray exists to remove.
pub fn start(spawn: &Spawn, logs: LogPair, cwd: &Path) -> Result<Child, String> {
    let line = spawn.describe();
    let mut command = std::process::Command::new(&spawn.program);
    command
        .args(&spawn.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(make_log(&logs.out)?))
        .stderr(Stdio::from(make_log(&logs.err)?));
    // CREATE_NEW_PROCESS_GROUP makes the pid a signal address; CREATE_NO_WINDOW stops a console
    // window flashing open behind the icon. Both flags are `main.py`'s, and neither is optional.
    command.creation_flags(ffi::CREATE_NEW_PROCESS_GROUP | ffi::CREATE_NO_WINDOW);
    let inner = command
        .spawn()
        .map_err(|e| format!("could not start\n  {line}\nas {cwd:?}\nwrote to {}\n{e}", logs.out.display()))?;
    Ok(Child { inner, line, logs })
}

/// Create (and truncate) one log file. The directory is made first because `cache/logs` is not
/// guaranteed to exist on a fresh install, and a redirect that cannot open its file fails at
/// `CreateProcess` with an error naming neither the program nor the file.
fn make_log(path: &Path) -> Result<std::fs::File, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windsvc-child-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn logs(dir: &Path) -> LogPair {
        LogPair { out: dir.join("o.log"), err: dir.join("e.log") }
    }

    /// `ping` is on every Windows install, is still running a second later, and ends by itself.
    fn sleeper() -> Spawn {
        Spawn { program: PathBuf::from("ping.exe"), args: vec!["-n".into(), "30".into(), "127.0.0.1".into()] }
    }

    #[test]
    fn a_spawned_child_is_alive_then_reported_gone() {
        let dir = scratch("live");
        let child = start(&sleeper(), logs(&dir), &dir).expect("ping must start");
        // The redirect is real: a supervised process whose output went nowhere is undiagnosable.
        assert!(dir.join("o.log").exists() && dir.join("e.log").exists());
        assert!(child.line.contains("ping.exe -n 30 127.0.0.1"), "{}", child.line);
        // The pid is the one the OS gave this child, not ours: it is what goes into the bridge's
        // lock file, and a lock naming the tray would claim a killed tray killed nothing.
        assert!(child.pid() > 0 && child.pid() != std::process::id(), "a child names itself, not its supervisor");
        assert_eq!(child.stop_forced(), Stopped::Terminated, "a live child is killed, not waited on");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both stops have to be able to say "it was already gone", because that is the case where
    /// killing a handle whose process has exited would be a lie about what the tray did. The exit is
    /// polled for rather than slept out: how fast `cmd.exe` starts is not what either test asserts.
    #[test]
    fn a_child_that_exited_by_itself_is_reaped_and_not_killed_again() {
        let dir = scratch("dead");
        let done = Spawn { program: PathBuf::from("cmd.exe"), args: vec!["/c".into(), "exit 3".into()] };
        let mut child = start(&done, logs(&dir), &dir).expect("cmd must start");
        wait_until_exit(&mut child);
        assert_eq!(child.exited(), Some(3), "the exit status is the only evidence a window left");
        assert_eq!(child.stop_forced(), Stopped::AlreadyGone);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recorder_that_already_went_is_reported_gone_rather_than_gracefully_stopped() {
        let dir = scratch("gone");
        let done = Spawn { program: PathBuf::from("cmd.exe"), args: vec!["/c".into(), "exit".into()] };
        let mut child = start(&done, logs(&dir), &dir).expect("cmd must start");
        wait_until_exit(&mut child);
        assert_eq!(child.stop_gracefully(), Stopped::AlreadyGone);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wait_until_exit(child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.exited().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Which stops cost the user something. A live child's graceful path is deliberately not
    /// exercised here: it needs this process to hold no console, and a test harness is a console
    /// program, so the only honest statement about a real `CTRL_BREAK_EVENT` is the one `windsvc run`
    /// makes in front of a user. The classification, and the message attached to each of them, are
    /// what the reporting is actually made of.
    #[test]
    fn only_the_two_hard_stops_have_to_be_reported() {
        for (stopped, must_report) in [
            (Stopped::Graceful { code: Some(0) }, false),
            (Stopped::AlreadyGone, false),
            (Stopped::Terminated, false),
            (Stopped::KilledAfterTimeout, true),
            (Stopped::KilledAfterSignalFailure { reason: "os error 5".to_string() }, true),
        ] {
            assert_eq!(stopped.failure().is_some(), must_report, "{stopped:?}");
        }
        let timeout = Stopped::KilledAfterTimeout.failure().expect("a killed recorder must be explained");
        assert!(timeout.contains("gracefully") && timeout.contains("Killing"), "{timeout}");
        let unsendable = Stopped::KilledAfterSignalFailure { reason: "os error 5".to_string() }
            .failure()
            .expect("a recorder that could not be signalled must say why it was killed anyway");
        assert!(unsendable.contains("os error 5"), "{unsendable}");
    }

    #[test]
    fn a_missing_program_fails_with_the_command_and_the_os_error() {
        let dir = scratch("missing");
        let missing = Spawn { program: PathBuf::from("Z:/nope/windrec.exe"), args: vec!["loop".into()] };
        let error = start(&missing, logs(&dir), &dir).expect_err("a nonexistent binary must not look like a start");
        assert!(error.contains("Z:/nope/windrec.exe loop"), "{error}");
        assert!(error.contains("could not start"), "{error}");
        assert!(error.contains("o.log"), "the log file must be named: {error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logs_are_created_even_when_the_directory_did_not_exist() {
        let dir = scratch("logs");
        let nested = dir.join("cache").join("logs");
        let echo = Spawn { program: PathBuf::from("cmd.exe"), args: vec!["/c".into(), "echo hi".into()] };
        let pair = LogPair { out: nested.join("recording.log"), err: nested.join("recording.err") };
        let child = start(&echo, pair, &dir).expect("the log directory must be made on the way");
        let stopped = child.stop_forced();
        assert!(!matches!(stopped, Stopped::KilledAfterSignalFailure { .. }));
        assert!(nested.join("recording.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_timeouts_are_the_numbers_upstream_advertises() {
        // Pinned because both appear in text the user reads when something went wrong.
        assert_eq!(GRACEFUL_STOP, Duration::from_secs(5));
        assert!(TICK < Duration::from_secs(2), "the tray must notice a closed window quickly");
    }

    /// There is no longer a port to find: the interface window is a window, so the tray starts it and
    /// never asks what address it is serving. What the launch *does* have to get right is the log
    /// pair, and that is asserted above by reading the files back off disk.
    #[test]
    fn an_interface_launch_needs_no_port_and_still_needs_its_logs() {
        let dir = scratch("no-port");
        let echo = Spawn { program: PathBuf::from("cmd.exe"), args: vec!["/c".into(), "echo hi".into()] };
        let pair = LogPair { out: dir.join("windui.log"), err: dir.join("windui.err") };
        let child = start(&echo, pair.clone(), &dir).expect("a window is started the same way");
        assert_eq!(child.logs, pair, "the caller's pair is the one the process writes into");
        let _ = child.stop_forced();
        assert!(pair.out.exists() && pair.err.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
