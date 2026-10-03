//! The notification-area icon itself: the hidden window that receives its messages, the menu that
//! pops up from them, and the pump that keeps both alive.
//!
//! This is the only module in the crate that runs a message loop, and the rule it is written to is
//! that no Win32 call which pumps messages may happen while the `Host` is borrowed. A tray icon's
//! `TrackPopupMenu` runs a modal loop of its own and `SetTimer` keeps firing inside it, so a borrow
//! held across either one would be a `RefCell` panic in the middle of the user's click. Every handler
//! therefore borrows briefly to compute something, drops the borrow, and borrows again to act.
//!
//! `Slot` is what turns that rule from a habit into a mechanism. Reaching a host that is already
//! borrowed answers `None` instead of panicking, and a pump holds the gate without holding a borrow —
//! so the tick that arrives while the menu is up is skipped, rather than aborting a tray that has no
//! console to say so in. Both halves are load-bearing: the first is why the process survives, the
//! second is why a balloon cannot take the menu away while the user is reading it.

use std::cell::Cell;
use std::cell::RefCell;
use std::time::Duration;

use crate::ffi;
use crate::icon::Icons;
use crate::menu::{self, Command, Row};
use crate::options::Options;
use crate::supervisor::{Boot, Notice, Supervisor};

/// The tray's private callback message.
const WM_TRAY: u32 = ffi::WM_APP + 1;
/// The liveness poll, and therefore how long the tray can show a stale "Stop window" for an interface
/// the user has already closed by hand.
const TIMER_ID: usize = 1;

/// How long a failure balloon gets to reach the screen before its icon is destroyed. `NIM_DELETE`
/// cancels a message the shell has queued but not drawn, and the pump ends as soon as the window is
/// gone — so without this the one sentence explaining a hard kill is the thing the user never sees.
const BALLOON_GRACE: Duration = Duration::from_millis(1500);

/// The window's own state plus the supervised world, both confined to the thread that owns the window.
/// A thread-local rather than a `static` because the type holds raw handles, and a thread-local says
/// so without an `unsafe impl Send` claiming a property nobody checks.
struct Host {
    window: ffi::HWND,
    tray: Tray,
    supervisor: Supervisor,
}

/// The host plus the gate that decides who may reach it.
///
/// Split out from `RefCell<Option<Host>>` so the gate is the thing under test: a second arrival has
/// to be refused rather than panic, and panicking is all a `RefCell` does on its own.
struct Slot<T> {
    value: RefCell<Option<T>>,
    /// Whether a handler is inside the gate right now. One level is all this ever reaches: a holder
    /// that nests does not take a second gate, it finds the first one still shut.
    held: Cell<bool>,
}

/// The gate, held for as long as no other handler may reach the host.
struct Gate<'a> {
    held: &'a Cell<bool>,
}

impl Drop for Gate<'_> {
    fn drop(&mut self) {
        // On the unwind path as well, or a handler that dies halfway would lock the tray out of
        // every later message for the rest of its life.
        self.held.set(false);
    }
}

impl<T> Slot<T> {
    const fn new() -> Slot<T> {
        Slot { value: RefCell::new(None), held: Cell::new(false) }
    }

    fn set(&self, value: T) {
        *self.value.borrow_mut() = Some(value);
    }

    fn take(&self) -> Option<T> {
        self.value.borrow_mut().take()
    }

    /// Take the gate, or `None` when a handler is already inside it.
    ///
    /// A refusal leaves the gate shut, which is the point: the handler that owns it is still inside,
    /// and putting the gate down on its behalf would let the next arrival borrow under that borrow.
    fn gate(&self) -> Option<Gate<'_>> {
        if self.held.replace(true) {
            return None;
        }
        Some(Gate { held: &self.held })
    }

    /// Run `run` against the host, or return `None` when nothing is installed yet *or* the gate is
    /// already held.
    ///
    /// The second case is the refusal that keeps the tray up. A `RefCell` panics on a concurrent
    /// `borrow_mut`, and a panic that starts inside a window procedure cannot unwind through the
    /// `extern "system"` boundary — it aborts the process, and a `windows_subsystem` binary that
    /// attaches no console aborts in silence. Skipping one tick is invisible; that is not.
    fn with<R>(&self, run: impl FnOnce(&mut T) -> R) -> Option<R> {
        let _held = self.gate()?;
        self.value.borrow_mut().as_mut().map(run)
    }

    /// Run something that pumps messages, holding the gate *without* borrowing the value.
    ///
    /// The other half of the rule. `TrackPopupMenuEx` runs its own modal loop and dispatches the
    /// liveness `WM_TIMER` from inside it, so the pump has to happen with no borrow live — and it
    /// still has to shut the gate, because the tick that arrives there must be refused rather than
    /// allowed to show a balloon that takes the menu away from the user. The icon catches up on the
    /// next tick, once the menu is gone.
    fn pumping<R>(&self, run: impl FnOnce() -> R) -> R {
        match self.gate() {
            Some(gate) => {
                let _held = gate;
                run()
            }
            // Already held, so the gate is shut either way and this needs no second one.
            None => run(),
        }
    }
}

thread_local! {
    static HOST: Slot<Host> = const { Slot::new() };
}

fn with_host<T>(run: impl FnOnce(&mut Host) -> T) -> Option<T> {
    HOST.with(|slot| slot.with(run))
}

/// The one call site in this file allowed to run a modal Win32 loop. See `Slot::pumping`.
fn pump_with_gate_held<T>(run: impl FnOnce() -> T) -> T {
    HOST.with(|slot| slot.pumping(run))
}

/// The icon's window, its two states and what it is currently showing.
struct Tray {
    icons: Icons,
    /// Reused for ADD / MODIFY / DELETE; only `u_flags` and the info fields change between them.
    data: ffi::NOTIFYICONDATAW,
    showing_recording: bool,
    showing_tip: String,
}

impl Tray {
    fn new(icons: Icons) -> Tray {
        Tray { icons, data: ffi::NOTIFYICONDATAW::default(), showing_recording: false, showing_tip: String::new() }
    }

    /// Put the icon in the notification area.
    ///
    /// Explorer owns the tray, so a shell that is starting up or restarting can reject the first
    /// `NIM_ADD` and accept the next one; a single retry is the difference between an icon that
    /// appears and a tray that exits and looks like a crash.
    fn add(&mut self, window: ffi::HWND, recording: bool, tip: &str) -> Result<(), String> {
        self.data = ffi::NOTIFYICONDATAW::new(window);
        self.data.u_flags = ffi::NIF_MESSAGE | ffi::NIF_ICON | ffi::NIF_TIP;
        self.data.u_callback_message = WM_TRAY;
        self.data.h_icon = self.icons.for_recording(recording);
        ffi::write_fixed(&mut self.data.sz_tip, tip);
        for attempt in 0..2 {
            if unsafe { ffi::Shell_NotifyIconW(ffi::NIM_ADD, &self.data) } == ffi::TRUE {
                self.showing_recording = recording;
                self.showing_tip = tip.to_string();
                return Ok(());
            }
            if attempt == 0 {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        Err(format!(
            "the notification area refused the icon\n{}\n{}",
            self.icons.paths[0].display(),
            ffi::last_os_error()
        ))
    }

    /// Swap the icon and the hover text if the state moved.
    ///
    /// This is also the only thing that clears a balloon: `NIF_INFO` is absent from the flags below,
    /// and the shell dismisses a balloon it no longer sees in the struct.
    fn sync(&mut self, recording: bool, tip: &str) {
        if recording == self.showing_recording && tip == self.showing_tip {
            return;
        }
        self.data.h_icon = self.icons.for_recording(recording);
        ffi::write_fixed(&mut self.data.sz_tip, tip);
        self.data.u_flags = ffi::NIF_MESSAGE | ffi::NIF_ICON | ffi::NIF_TIP;
        self.data.dw_info_flags = ffi::NIIF_INFO;
        if unsafe { ffi::Shell_NotifyIconW(ffi::NIM_MODIFY, &self.data) } == ffi::TRUE {
            self.showing_recording = recording;
            self.showing_tip = tip.to_string();
        }
    }

    /// A balloon. `NIF_REALTIME` is why the message is not swallowed by the shell's quiet mode.
    fn balloon(&self, notice: &Notice) {
        let (title, body, flags) = match notice {
            Notice::Info { title, body } => (title, body, ffi::NIIF_INFO),
            Notice::Failure { title, body } => (title, body, ffi::NIIF_ERROR),
        };
        let mut data = self.data;
        data.u_flags = ffi::NIF_INFO | ffi::NIF_REALTIME;
        data.dw_info_flags = flags;
        ffi::write_fixed(&mut data.sz_info_title, title);
        ffi::write_fixed(&mut data.sz_info, body);
        unsafe { ffi::Shell_NotifyIconW(ffi::NIM_MODIFY, &data) };
    }

    fn remove(&mut self) {
        if self.data.hwnd.is_null() {
            return;
        }
        unsafe { ffi::Shell_NotifyIconW(ffi::NIM_DELETE, &self.data) };
        self.data.hwnd = std::ptr::null_mut();
    }
}

impl Host {
    /// Repaint the icon from the truth about what is running.
    fn sync_icon(&mut self) {
        // The notices are dropped here on purpose: this runs on a timer, and a balloon every few seconds
        // would be worse than the state it announced. `popup_menu` carries them instead.
        //
        // And it re-reads the settings instead of this arm doing it. The icon and its tip answer one
        // question — is a live pid holding the record lock — and `snapshot()` reads that off the locks;
        // the config decides nothing about the picture. Pulling two JSON files and opening the bridge's
        // runtime here once a second was 86k parses a day to repaint an icon that had not changed, while
        // every path where a setting can actually matter — the menu opening, a click landing in it —
        // already refreshed for itself.
        let snapshot = self.supervisor.snapshot();
        let tip = menu::tooltip(&snapshot, &self.supervisor.catalog);
        self.tray.sync(snapshot.recording, &tip);
    }

    /// What the menu is about to say, and what the config refresh brought up on the way there.
    ///
    /// Ends before anything pumps, and the caller shows the menu in the gap that leaves. Written this
    /// way on purpose: the rows describe the state the click is about to act on, and the borrow that
    /// reads it has to be gone before `TrackPopupMenuEx` starts its modal loop.
    fn prepare_menu(&mut self) -> (Vec<Notice>, Vec<Row>) {
        // First, so the rows a user is about to read are in the language they last chose *and*
        // describe the state this menu is about to act on.
        let notices = self.supervisor.refresh_config();
        let snapshot = self.supervisor.snapshot();
        (notices, menu::rows(&snapshot, &self.supervisor.catalog))
    }

    fn dispatch(&mut self, command: Command) -> Vec<Notice> {
        match command {
            Command::FlagNow => self.supervisor.flag_now(),
            Command::ToggleInterface => self.supervisor.toggle_interface(),
            Command::OpenInterface => self.supervisor.open_interface(),
            Command::ToggleRecord => self.supervisor.toggle_record(),
            Command::Changelog => self.supervisor.open_changelog(),
            Command::Exit => self.supervisor.exit(),
            // Display-only rows, grayed for the same reason and the better one: this tray ships no
            // updater, so the version is a fact about the binary and not a thing a click could do,
            // the bridge's state is switched by `enable_mcp_server` rather than by a menu item that
            // appeared to control a network listener it had no authority over, and the capture state
            // is the screen's own behaviour — the row above it already offers every action there is.
            Command::Version | Command::BridgeState | Command::RecordState => Vec::new(),
        }
    }

    /// Show what happened, repaint the icon, and close if the command was Exit.
    fn present(&mut self, notices: Vec<Notice>) {
        let mut waited = false;
        for notice in &notices {
            if self.supervisor.quit_requested() && matches!(notice, Notice::Failure { .. }) && !waited {
                // Give the shell time to draw the failure before `WM_CLOSE` destroys its icon.
                std::thread::sleep(BALLOON_GRACE);
                waited = true;
            }
            self.tray.balloon(notice);
        }
        self.sync_icon();
        if self.supervisor.quit_requested() {
            unsafe { ffi::PostMessageW(self.window, ffi::WM_CLOSE, 0, 0) };
        }
    }
}

/// The menu, in the order `rows` returns it.
fn build_menu(rows: &[Row]) -> Option<ffi::HMENU> {
    let menu = unsafe { ffi::CreatePopupMenu() };
    if menu.is_null() {
        return None;
    }
    for row in rows {
        match row {
            Row::Separator => {
                unsafe { ffi::AppendMenuW(menu, ffi::MF_SEPARATOR, 0, std::ptr::null()) };
            }
            Row::Item(item) => {
                let label = ffi::wide(&item.label);
                let mut flags = ffi::MF_STRING;
                if !item.enabled {
                    flags |= ffi::MF_GRAYED;
                }
                if item.default {
                    flags |= ffi::MF_DEFAULT;
                }
                unsafe { ffi::AppendMenuW(menu, flags, item.command as u32, label.as_ptr()) };
            }
        }
    }
    Some(menu)
}

/// Bring the menu up at the cursor.
///
/// `SetForegroundWindow` first is documented in MSDN's own note on tray menus and is load-bearing
/// twice: without it the menu does not dismiss when the user clicks on another window, and the dummy
/// `PostMessage` afterwards is what stops that first click being eaten by us rather than reaching the
/// window under it. The second `SetForegroundWindow` is the follow-up half of the same workaround,
/// because `TrackPopupMenuEx` leaves the shell's own menu window in the foreground.
unsafe fn track(window: ffi::HWND, menu: ffi::HMENU, point: ffi::POINT) -> Option<Command> {
    ffi::SetForegroundWindow(window);
    let picked = ffi::TrackPopupMenuEx(
        menu,
        ffi::TPM_BOTTOMALIGN | ffi::TPM_LEFTALIGN | ffi::TPM_RETURNCMD,
        point.x,
        point.y,
        window,
        std::ptr::null(),
    );
    ffi::PostMessageW(window, ffi::WM_NULL, 0, 0);
    ffi::SetForegroundWindow(window);
    if picked <= 0 {
        return None;
    }
    Command::from_id(picked as u32)
}

/// The hidden message window the shell posts to.
///
/// A zero-sized `WS_POPUP` that is never shown, deliberately not a message-only window:
/// `HWND_MESSAGE` cannot be passed to `SetForegroundWindow`, and the `TrackPopupMenuEx` workaround
/// above needs a window the shell will accept as a foreground candidate. Zero size is also what keeps
/// it out of the taskbar and the alt+tab list without needing `ShowWindow` at all.
fn create_window() -> Result<ffi::HWND, String> {
    let instance = unsafe { ffi::GetModuleHandleW(std::ptr::null()) };
    let class_name = ffi::wide("WindrecorderTraySupervisor");
    let title = ffi::wide("Windrecorder");
    let mut record = ffi::WNDCLASSEXW::default();
    record.cb_size = std::mem::size_of::<ffi::WNDCLASSEXW>() as u32;
    record.lpfn_wnd_proc = Some(wndproc);
    record.h_instance = instance;
    record.lpsz_class_name = class_name.as_ptr();
    if unsafe { ffi::RegisterClassExW(&record) } == 0 {
        return Err(format!("RegisterClassExW: {}", ffi::last_os_error()));
    }
    // SAFETY: both wide strings and `record` outlive this call, which copies the class name into its
    // own atom. A null menu means "no window menu", which a window with no title bar has no use for.
    let window = unsafe {
        ffi::CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            ffi::WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        )
    };
    if window.is_null() {
        return Err(format!("CreateWindowExW: {}", ffi::last_os_error()));
    }
    Ok(window)
}

/// Register the class, create the hidden window, put the icon up, and pump until it closes.
pub fn run(options: &Options) -> Result<(), String> {
    let mut supervisor = match Supervisor::boot(options) {
        Boot::Ready(supervisor) => *supervisor,
        Boot::AlreadyRunning { message } => {
            alert(&message, crate::layout::ALREADY_RUNNING_CAPTION, false);
            return Ok(());
        }
        Boot::Failed(error) => {
            alert(&error, "Windrecorder cannot start", true);
            return Err(error);
        }
    };

    let icons = Icons::load(&supervisor.layout)?;
    let window = match create_window() {
        Ok(window) => window,
        Err(error) => {
            // The tray lock is still ours here; dropping the supervisor below releases it, which is
            // why the error path returns instead of continuing with a window-less tray.
            return Err(error);
        }
    };
    let mut tray = Tray::new(icons);
    let snapshot = supervisor.snapshot();
    let tip = menu::tooltip(&snapshot, &supervisor.catalog);
    if let Err(error) = tray.add(window, snapshot.recording, &tip) {
        return Err(error);
    }
    // SAFETY: `window` is alive and this thread owns its queue, which is the thread the callback
    // messages will be delivered on.
    unsafe { ffi::SetTimer(window, TIMER_ID, crate::child::TICK.as_millis() as u32, None) };

    let boot_notices = std::mem::take(&mut supervisor.boot_notices);
    HOST.with(|slot| slot.set(Host { window, tray, supervisor }));
    // Upstream notified from pystray's `setup`, i.e. once the icon was visible; the same ordering here
    // means the balloons describe the state the icon is showing.
    with_host(|host| host.present(boot_notices));

    let mut message = ffi::MSG::default();
    loop {
        // SAFETY: the pump runs on the thread that created the window, and every pointer inside
        // `message` is owned by the OS for the duration of the two calls below.
        let result = unsafe { ffi::GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            // 0 is `WM_QUIT`; a negative return is a real failure. Both end the pump.
            break;
        }
        unsafe {
            ffi::TranslateMessage(&message);
            ffi::DispatchMessageW(&message);
        }
    }

    // The icon is removed by `WM_CLOSE`; reaching here without that means the pump ended on an error,
    // in which case the shell drops the orphan when the process does. The lock always goes, and so
    // does the bridge: a listener left behind by an ended tray is invisible on the desktop, which is
    // the one supervised process whose silence is not an acceptable way to end.
    with_host(|host| {
        host.tray.remove();
        host.supervisor.relinquish_bridge();
        host.supervisor.release_tray_lock();
    });
    HOST.with(|slot| slot.take());
    Ok(())
}

unsafe extern "system" fn wndproc(window: ffi::HWND, message: u32, wparam: ffi::WPARAM, lparam: ffi::LPARAM) -> ffi::LRESULT {
    match message {
        WM_TRAY => {
            let event = ffi::tray_event(lparam);
            if event == ffi::WM_RBUTTONUP || event == ffi::WM_CONTEXTMENU {
                // Both, because both arrive: `WM_RBUTTONUP` is the right click, `WM_CONTEXTMENU` is
                // what the shell sends for the keyboard menu key and, on some Windows builds, instead
                // of the button message. Handling one is a tray that sometimes has no menu.
                let point = menu_position(lparam);
                let (mut notices, rows) = match with_host(|host| host.prepare_menu()) {
                    Some(turn) => turn,
                    // The gate was already held, so a menu is up on this thread and the shell has
                    // sent a second right click into the first one's modal loop. The first menu wins.
                    None => return 0,
                };
                match build_menu(&rows) {
                    Some(menu) => {
                        // The only line here that runs a message loop: the gate is shut for it and no
                        // borrow is live across it, which is the whole of why the tray no longer dies
                        // on a right click.
                        let picked = pump_with_gate_held(|| unsafe { track(window, menu, point) });
                        unsafe { ffi::DestroyMenu(menu) };
                        if let Some(command) = picked {
                            notices.extend(with_host(|host| host.dispatch(command)).unwrap_or_default());
                        }
                    }
                    None => notices.push(Notice::Failure {
                        title: "The menu could not be opened".to_string(),
                        body: format!("CreatePopupMenu: {}", ffi::last_os_error()),
                    }),
                }
                with_host(|host| host.present(notices));
            } else if event == ffi::WM_LBUTTONDBLCLK {
                // The default item, which is what pystray's `default=True` made a double click do.
                let notices = with_host(|host| host.dispatch(Command::OpenInterface)).unwrap_or_default();
                with_host(|host| host.present(notices));
            }
            0
        }
        ffi::WM_TIMER => {
            let notices = with_host(|host| host.supervisor.tick()).unwrap_or_default();
            if notices.is_empty() {
                // The state also moves without anyone sending a message — a recorder that finished a
                // segment and exited, most often — and the icon has to follow it.
                with_host(|host| host.sync_icon());
            } else {
                with_host(|host| host.present(notices));
            }
            0
        }
        ffi::WM_CLOSE => {
            // Killed before the window goes: a timer that fires during the shutdown would tick a
            // supervisor that has already released its lock.
            unsafe { ffi::KillTimer(window, TIMER_ID) };
            with_host(|host| host.tray.remove());
            unsafe { ffi::DestroyWindow(window) };
            0
        }
        ffi::WM_DESTROY => {
            unsafe { ffi::PostQuitMessage(0) };
            0
        }
        _ => unsafe { ffi::DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// `WM_CONTEXTMENU` carries a position in `lparam` (and -1 when the keyboard raised it), while
/// `WM_RBUTTONUP` carries nothing useful because the icon's window is zero-sized. Asking the cursor is
/// the answer that is right in both cases, and the `lparam` split is only for a desktop with no cursor.
unsafe fn menu_position(lparam: ffi::LPARAM) -> ffi::POINT {
    let mut point = ffi::POINT::default();
    if ffi::GetCursorPos(&mut point) == ffi::FALSE {
        point.x = ((lparam as u32) & 0xFFFF) as i16 as i32;
        point.y = (((lparam as u32) >> 16) & 0xFFFF) as i16 as i32;
    }
    point
}

/// A blocking modal message box. With `windows_subsystem = "windows"` there is no console to write a
/// failure to, so this is how the tray says "nothing happened, and here is why".
pub fn alert(text: &str, caption: &str, warning: bool) {
    let body = ffi::wide(text);
    let title = ffi::wide(caption);
    let flags = ffi::MB_OK | if warning { ffi::MB_ICONWARNING } else { ffi::MB_ICONINFORMATION };
    // SAFETY: both buffers outlive the call; a null owner centres it on the monitor.
    unsafe { ffi::MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), flags) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The right-click branch used to hold a `borrow_mut` across `TrackPopupMenuEx`, whose modal loop
    /// dispatches the liveness `WM_TIMER` back into this same thread. A `RefCell` answers a second
    /// `borrow_mut` by panicking, a panic that begins in an `extern "system"` function cannot unwind
    /// through, and the binary is `windows_subsystem = "windows"` with no console attached — so the
    /// whole tray vanished on a right click, which is what this slot exists to make impossible.
    #[test]
    fn a_handler_that_reenters_while_the_host_is_borrowed_gets_none_not_an_abort() {
        let slot: Slot<u32> = Slot::new();
        slot.set(7);
        let nested = slot.with(|outer| slot.with(|inner| *outer + *inner));
        assert_eq!(nested, Some(None), "the re-entrant entry must be refused, not entered");
        assert_eq!(slot.with(|value| *value), Some(7), "and the slot must be usable again afterwards");
    }

    /// One bad handler must not wedge the tray: the flag has to come down on the unwind path too,
    /// or every later tick is refused and the icon silently stops following the recorder.
    #[test]
    fn a_handler_that_panics_leaves_the_slot_enterable_for_the_next_tick() {
        let slot: Slot<u32> = Slot::new();
        slot.set(7);
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slot.with(|value| {
                *value += 1;
                panic!("a handler that dies halfway")
            })
        }));
        assert!(died.is_err(), "the handler's panic must still reach the caller");
        assert_eq!(slot.with(|value| *value), Some(8), "a panicked handler must not lock the tray out");
    }

    /// Before `run()` installs a host there is nothing to hand out, and that case must not consume
    /// the entry either.
    #[test]
    fn an_empty_slot_says_no_and_stays_enterable() {
        let slot: Slot<u32> = Slot::new();
        assert_eq!(slot.with(|value| *value), None, "no host installed is not an entry");
        slot.set(1);
        assert_eq!(slot.with(|value| *value), Some(1));
    }

    /// Turning a handler away belongs to the entry that is still live. A refusal that cleared the
    /// flag would open the slot for the *next* handler while the first one still held the borrow —
    /// the same abort, one tick later.
    #[test]
    fn a_refused_entry_does_not_open_the_slot_for_the_next_one() {
        let slot: Slot<u32> = Slot::new();
        slot.set(7);
        let inside = slot.with(|_| {
            let first = slot.with(|value| *value).is_none();
            let second = slot.with(|value| *value).is_none();
            (first, second)
        });
        assert_eq!(inside, Some((true, true)), "every handler arriving during a live entry is refused");
        assert_eq!(slot.with(|value| *value), Some(7), "free again only once the outer entry returns");
    }

    /// The pump is the half that shows the menu. It must shut the gate *without* borrowing the host:
    /// the borrow is what a `RefCell` punishes, and the gate is what the arriving tick is turned away
    /// at. Asserting both is the difference between "the tray survives" and "the tray survives by
    /// refusing to do anything, forever".
    #[test]
    fn a_pump_holds_the_gate_but_not_a_borrow() {
        let slot: Slot<u32> = Slot::new();
        slot.set(7);
        let inside = slot.pumping(|| (slot.with(|value| *value).is_none(), slot.value.try_borrow().is_ok()));
        assert_eq!(inside, (true, true), "refused to handlers, readable by nobody but borrowed by none");
        let nested = slot.pumping(|| slot.pumping(|| "ran"));
        assert_eq!(nested, "ran", "a pump inside a held gate still runs; the gate is shut either way");
        assert_eq!(slot.with(|value| *value), Some(7), "and the gate opens again when the pump returns");
    }

    /// The menu ids are what `AppendMenuW` stamps and what `TrackPopupMenuEx` hands back, so the
    /// round trip has to be exact — an id that does not map back is a menu item that does nothing.
    #[test]
    fn every_command_survives_a_round_trip_through_its_menu_id() {
        let all = [
            Command::FlagNow,
            Command::ToggleInterface,
            Command::OpenInterface,
            Command::ToggleRecord,
            Command::Version,
            Command::Changelog,
            Command::Exit,
            Command::BridgeState,
        ];
        for command in all {
            assert_eq!(Command::from_id(command as u32), Some(command), "{command:?} must map back");
        }
        assert_eq!(Command::from_id(0), None);
        assert_eq!(Command::from_id(0x9999), None);
    }

    #[test]
    fn the_tray_callback_message_is_in_the_private_range() {
        // `Shell_NotifyIconW` rejects anything below `WM_APP`, and it fails without an error code.
        assert!(WM_TRAY >= ffi::WM_APP);
        assert!(crate::child::TICK <= Duration::from_secs(2), "a stale icon is worse than a busy one");
    }
}
