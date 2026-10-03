//! Session state: "may we capture the screen right now, and how long has the user been away".
//!
//! Replaces `windrecorder.utils.is_screen_locked` / `is_system_awake`. Both of those were
//! measured on this hardware at 6.76 s and (permanently-True) respectively; the equivalents here
//! are `OpenInputDesktop`+`GetUserObjectInformationW` at ~7 µs and a correctly-called
//! `GetLastInputInfo`. At that price no caching layer is needed, which is also why the three
//! Windrecorder threads stop racing over one shared probe result.

use crate::ffi;
use core::ffi::c_void;
use std::mem;

/// What the desktop is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// Input desktop is `Winsta0\Default` and the shell says a session is present.
    Usable,
    /// Cannot open the input desktop, or it is not `Default`: locked, Ctrl-Alt-Del,
    /// a secure screen saver, or the Winlogon desktop. Capturing now yields black.
    SecureDesktop,
    /// Desktop is reachable but the shell reports no active user session
    /// (screen saver on the *unsecure* desktop, or an inactive Fast-User-Switching session).
    NotPresent,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: SessionStatus,
    /// Name of the input desktop, when it could be opened. `None` means access denied.
    pub desktop_name: Option<String>,
    /// Seconds since the last keyboard/mouse input, or `None` when not measurable.
    pub idle_seconds: Option<f64>,
    /// Raw `QUNS` value from `SHQueryUserNotificationState`, for logs.
    pub notification_state: Option<i32>,
}

impl Snapshot {
    /// The one question the capture loop asks. Fails closed: anything but `Usable` is a skip.
    pub fn recordable(&self) -> bool {
        self.status == SessionStatus::Usable
    }
}

/// Name of the desktop that is receiving input, or `None` if it could not be opened.
///
/// Asking for anything beyond `DESKTOP_READOBJECTS` fails with ERROR_ACCESS_DENIED even while
/// unlocked on `Default`, so the mask here is load-bearing.
pub fn input_desktop_name() -> Option<String> {
    unsafe {
        let desktop = ffi::OpenInputDesktop(0, 0, ffi::DESKTOP_READOBJECTS);
        if desktop.is_null() {
            return None;
        }

        // UOI_NAME is a NUL-terminated WCHAR string.
        let mut buffer = [0u16; 256];
        let mut needed: u32 = 0;
        let ok = ffi::GetUserObjectInformationW(
            desktop,
            ffi::UOI_NAME,
            buffer.as_mut_ptr() as *mut c_void,
            (buffer.len() * 2) as u32,
            &mut needed,
        );
        ffi::CloseDesktop(desktop);

        if ok != ffi::TRUE {
            return None;
        }

        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        String::from_utf16(&buffer[..end]).ok()
    }
}

/// Seconds since the last user input.
///
/// `dw_time` is a 32-bit `GetTickCount` value, so the subtraction must wrap modulo 2^32.
/// A delta landing in the upper half of that space means `dw_time` was not monotonic with the
/// tick counter (documented for `SendInput`), which is "not measurable" — not "idle 24 days".
pub fn idle_seconds() -> Option<f64> {
    unsafe {
        let mut info = ffi::LASTINPUTINFO {
            cb_size: mem::size_of::<ffi::LASTINPUTINFO>() as u32,
            dw_time: 0,
        };
        if ffi::GetLastInputInfo(&mut info) != ffi::TRUE {
            return None;
        }
        let delta = ffi::GetTickCount().wrapping_sub(info.dw_time);
        if delta > u32::MAX / 2 {
            return None;
        }
        Some(f64::from(delta) / 1000.0)
    }
}

pub fn notification_state() -> Option<i32> {
    unsafe {
        let mut state: i32 = 0;
        if ffi::SHQueryUserNotificationState(&mut state) >= 0 {
            Some(state)
        } else {
            None
        }
    }
}

/// One coherent read of everything the capture loop needs.
///
/// `SHQueryUserNotificationState` is the only expensive probe here, and it is expensive in a way
/// the others are not: it is a round-trip into the shell process, so a starved or busy
/// `explorer.exe` blocks it for as long as it likes. Measured under a CPU-and-disk load,
/// `snapshot()` spent p95 6.7 s and max 7.1 s inside this call while the local win32k probes under
/// `input_desktop_name` and `idle_seconds` stayed at microseconds. The uncontended mean is 302 µs,
/// which is why it took a soak to find.
///
/// So it is asked only when its answer can change the verdict, which is narrower than it looks:
///
/// - The desktop could not be opened, or is not `Default`. That is already `SecureDesktop`, and
///   [`decide`] never reads the notification state on that path. Asking the shell for its opinion
///   while the shell is mid-transition — which is exactly what a lock, a Ctrl-Alt-Del or a
///   Winlogon prompt is — is the worst possible moment to make an inter-process call, and it buys
///   nothing. Skipped.
/// - The desktop is `Default` and the user is actively giving input. `NOT_PRESENT` means no user
///   session is present: a screensaver on the unsecure desktop, or an inactive Fast-User-Switching
///   session. Neither can be true while keyboard and mouse input is arriving, so the answer is
///   already known. Skipped.
/// - The desktop is `Default` and nobody has touched the machine for
///   [`QUNS_ASKED_AFTER_IDLE_SECOND`]. It is now the only signal that distinguishes a screensaver
///   from an unattended-but-awake desktop, so it is asked.
///
/// The gate fails in the safe direction on purpose. Skipping the call can only ever *add*
/// permission to record, and it is skipped only in states where `NOT_PRESENT` is impossible; it is
/// never skipped while the user is away, because a stale "someone is here" answer is how a PIN
/// prompt ends up indexed as the user's work.
pub fn snapshot() -> Snapshot {
    let name = input_desktop_name();
    let idle = idle_seconds();

    let desktop_reachable = matches!(&name, Some(desktop) if desktop == "Default");
    let quns = if desktop_reachable && !session_clearly_present(idle) {
        notification_state()
    } else {
        None
    };

    Snapshot {
        status: decide(&name, quns),
        desktop_name: name,
        idle_seconds: idle,
        notification_state: quns,
    }
}

/// How long the user must have been away before the shell is asked whether a session is present.
///
/// One second, deliberately below the recorder's own cadence (`screenshot_interval_second` is 3 in
/// production), so any cycle that is not the very first tick after activity is eligible to ask. A
/// larger value would skip the probe on a desktop sitting behind a screensaver that has not started
/// yet, which is the case the probe exists to catch.
pub const QUNS_ASKED_AFTER_IDLE_SECOND: f64 = 1.0;

/// Whether input is recent enough that `NOT_PRESENT` cannot be the truth.
///
/// `None` — idle not measurable — counts as *not* clearly present, so the shell still gets asked.
fn session_clearly_present(idle: Option<f64>) -> bool {
    matches!(idle, Some(seconds) if seconds < QUNS_ASKED_AFTER_IDLE_SECOND)
}

/// The verdict, as a pure function of what the probes returned. Split out of [`snapshot`] so the
/// whole decision table, including which rows fail closed, is testable without a desktop.
fn decide(name: &Option<String>, quns: Option<i32>) -> SessionStatus {
    match name {
        None => SessionStatus::SecureDesktop,
        Some(desktop) if desktop != "Default" => SessionStatus::SecureDesktop,
        Some(_) if quns == Some(ffi::quns::NOT_PRESENT) => SessionStatus::NotPresent,
        Some(_) => SessionStatus::Usable,
    }
}

/// A measured wall-over-tick gap larger than this is a sleep/hibernate, not scheduling jitter.
pub const SLEEP_DRIFT_THRESHOLD_SECOND: f64 = 30.0;
/// How long a detected resume stays visible. Three threads poll this with different cadences
/// (per frame, every 2 s, every 30 s); without a latch the fastest poller would consume the
/// event and the capture loop would never see the boundary it is supposed to break the slice on.
pub const SLEEP_DRIFT_LATCH_SECOND: f64 = 10.0;

#[derive(Debug)]
struct DriftState {
    seeded: bool,
    tick_ms: u32,
    wall_ms: u64,
    latched_second: f64,
}

impl Default for DriftState {
    fn default() -> Self {
        DriftState {
            seeded: false,
            tick_ms: 0,
            wall_ms: 0,
            latched_second: 0.0,
        }
    }
}

static DRIFT: std::sync::Mutex<DriftState> = std::sync::Mutex::new(DriftState {
    seeded: false,
    tick_ms: 0,
    wall_ms: 0,
    latched_second: 0.0,
});

/// Seconds the wall clock gained over `GetTickCount` since the previous call.
///
/// `GetTickCount` does not advance while the machine sleeps or hibernates; the wall clock does.
/// That gap is the only cheap, reliable "we just resumed" signal, and it is what Windrecorder's
/// `is_system_awake()` was trying and failing to be (it called `GetLastInputInfo` with no
/// argument, took an access violation, and returned `True` forever behind a bare `except`).
///
/// Returns the latched value while a recent resume is still in the visibility window, so every
/// caller observes the same boundary.
pub fn sleep_drift_seconds() -> f64 {
    let raw_tick = unsafe { ffi::GetTickCount() };
    let wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut guard = match DRIFT.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };

    if !guard.seeded {
        guard.tick_ms = raw_tick;
        guard.wall_ms = wall_ms;
        guard.seeded = true;
        return 0.0;
    }

    // Polls here are seconds apart, so modular subtraction recovers the true delta even if the
    // 49.7-day tick counter wrapped in between.
    let tick_delta = raw_tick.wrapping_sub(guard.tick_ms) as f64 / 1000.0;
    let prev_wall_ms = guard.wall_ms;
    let wall_delta = wall_ms.saturating_sub(prev_wall_ms) as f64 / 1000.0;
    let drift = (wall_delta - tick_delta).max(0.0);

    guard.tick_ms = raw_tick;
    guard.wall_ms = wall_ms;

    if drift > SLEEP_DRIFT_THRESHOLD_SECOND {
        guard.latched_second = drift;
        return drift;
    }

    if guard.latched_second > 0.0 {
        // Keep the previous detection visible for SLEEP_DRIFT_LATCH_SECOND so that the slower
        // pollers still observe the boundary they missed.
        let held_second = wall_ms.saturating_sub(prev_wall_ms) as f64 / 1000.0;
        if held_second < SLEEP_DRIFT_LATCH_SECOND {
            return guard.latched_second;
        }
        guard.latched_second = 0.0;
    }

    0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first read seeds the baseline and must report no drift; a busy loop right after it
    /// must not fabricate one either. Guards the wraparound handling and the latch together.
    #[test]
    fn rapid_consecutive_reads_report_no_sleep() {
        assert_eq!(sleep_drift_seconds(), 0.0);
        for _ in 0..50 {
            assert!(
                sleep_drift_seconds() < SLEEP_DRIFT_THRESHOLD_SECOND,
                "no real sleep happened, so drift must stay under the threshold"
            );
        }
    }

    /// `cbSize` wrong makes the API fail with ERROR_INVALID_PARAMETER and report nothing, so a
    /// `None` here means the struct layout or the size field regressed.
    #[test]
    fn idle_seconds_is_measurable() {
        let idle = idle_seconds().expect("GetLastInputInfo must succeed with cbSize=8");
        assert!(idle >= 0.0);
    }

    /// The whole point of the rewrite: this answers in microseconds, not seconds.
    ///
    /// Best-of-N batches rather than one mean, because this is a correctness guard and not a
    /// benchmark, and the two need different statistics. As originally written -- one average over
    /// 1000 calls against a 1 ms bound -- it went red whenever the machine happened to be busy: a
    /// full `cargo test --workspace` runs 29 suites in parallel and this measured 4.5 ms per call
    /// under that, for a probe that costs single-digit microseconds when nothing else is running.
    /// A test that fails for a reason unrelated to the code it guards is worse than no test,
    /// because it teaches everyone to read red as noise, and the regression it exists to catch is
    /// someone reintroducing an 8.2 s process-table walk.
    ///
    /// Taking the fastest batch keeps the original tight bound intact. Contention is transient, so
    /// at least one batch runs clean on a loaded machine; a genuinely slow probe is slow in every
    /// batch, so this cannot be satisfied by luck.
    #[test]
    fn input_desktop_probe_is_not_silently_slow() {
        const BATCHES: usize = 5;
        const CALLS_PER_BATCH: usize = 200;

        let mut best = std::time::Duration::MAX;
        for _ in 0..BATCHES {
            let start = std::time::Instant::now();
            for _ in 0..CALLS_PER_BATCH {
                let _ = input_desktop_name();
            }
            let per_call = start.elapsed() / CALLS_PER_BATCH as u32;
            best = best.min(per_call);
        }

        assert!(
            best < std::time::Duration::from_millis(1),
            "input_desktop_name never averaged under 1 ms in any of {BATCHES} batches; \
             best was {best:?} over {CALLS_PER_BATCH} calls each -- the psutil baseline was 8.2 s"
        );
    }

    /// The decision table, with the notification state as a plain input.
    ///
    /// The rows that matter are the first two: a desktop that cannot be opened, or opens as
    /// something other than `Default`, is `SecureDesktop` no matter what the shell claims. That is
    /// what lets [`snapshot`] skip the shell round-trip on those paths — skipping it must never be
    /// able to talk the recorder into a frame it would otherwise have refused.
    #[test]
    fn an_unreachable_desktop_is_refused_whatever_the_shell_claims() {
        for claimed in [
            Some(ffi::quns::NOT_PRESENT),
            Some(ffi::quns::BUSY),
            Some(ffi::quns::ACCEPTS_NOTIFICATIONS),
            None,
        ] {
            assert_eq!(
                decide(&None, claimed),
                SessionStatus::SecureDesktop,
                "closed input desktop, shell said {claimed:?}"
            );
            assert_eq!(
                decide(&Some("Winlogon".into()), claimed),
                SessionStatus::SecureDesktop,
                "input desktop was Winlogon, shell said {claimed:?}"
            );
        }
    }

    /// The one row where the shell's answer is load-bearing, and the row the skip-gate protects.
    #[test]
    fn a_reachable_desktop_still_listens_when_the_shell_says_nobody_is_here() {
        assert_eq!(
            decide(&Some("Default".into()), Some(ffi::quns::NOT_PRESENT)),
            SessionStatus::NotPresent,
            "a screensaver on the unsecure desktop must not be recorded as the user's work"
        );
        assert_eq!(decide(&Some("Default".into()), Some(ffi::quns::ACCEPTS_NOTIFICATIONS)), SessionStatus::Usable);
        // `None` here means "we did not ask", which `snapshot` only does on a path where the
        // verdict is already settled, or because the user is plainly present.
        assert_eq!(decide(&Some("Default".into()), None), SessionStatus::Usable);
    }

    /// The gate that decides whether to pay for the shell round-trip.
    #[test]
    fn the_shell_is_skipped_only_while_input_proves_someone_is_here() {
        assert!(session_clearly_present(Some(0.0)), "input this instant means a session is present");
        assert!(session_clearly_present(Some(0.99)), "just inside the threshold");
        assert!(!session_clearly_present(Some(1.0)), "at the threshold the shell is asked");
        assert!(!session_clearly_present(Some(600.0)), "an unattended desktop is exactly when it matters");
        assert!(!session_clearly_present(None), "unmeasurable idle is not the same as recent input");
    }

    /// Why the threshold is one second and not, say, thirty.
    ///
    /// Skipping the probe on an *idle* desktop is the one way this gate could hide a screensaver,
    /// so the threshold has to sit below the interval at which the recorder actually captures. If
    /// someone raises it past the production cadence, a desktop left alone long enough for a
    /// screensaver would stop being asked, and the frames in between would be indexed.
    #[test]
    fn the_idle_gate_stays_below_the_capture_cadence() {
        const PRODUCTION_SCREENSHOT_INTERVAL_SECOND: f64 = 3.0;
        assert!(
            QUNS_ASKED_AFTER_IDLE_SECOND < PRODUCTION_SCREENSHOT_INTERVAL_SECOND,
            "asking only after {QUNS_ASKED_AFTER_IDLE_SECOND}s idle is useless at a \
             {PRODUCTION_SCREENSHOT_INTERVAL_SECOND}s capture interval: every idle cycle would \
             skip the one probe that detects a screensaver"
        );
    }
}




