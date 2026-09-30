//! The whole Win32 surface of the launcher: two calls and one encoder.
//!
//! Declared by hand, following `core/src/ffi.rs` and `supervisor/src/ffi.rs`: the workspace's rule is
//! that `cargo build --offline` has to work on a machine whose cargo cache holds nothing beyond what
//! is already vendored, and reconnecting a console is one `kernel32` call. Everything below is
//! therefore a declaration rather than an abstraction.
//!
//! Only what is actually called is declared: an unused item in a private module is a `dead_code`
//! warning, and the crate's build is required to be warning-free.

use core::ffi::c_void;

pub type BOOL = i32;
pub type DWORD = u32;
pub type UINT = u32;
pub type WORD = u16;
pub type HWND = *mut c_void;

/// `MessageBoxW`'s buttons and icon. `MB_OK` is zero and so is left implicit; what has to be named is
/// the stop sign, because the only reason this dialog can appear is that the install is broken.
pub const MB_ICONERROR: UINT = 0x0010;
/// The box a double-click brings up must not land behind the window the user was looking at. Without
/// it this dialog is created, unfocused, and reads as a hung click.
pub const MB_SETFOREGROUND: UINT = 0x0001_0000;

/// The sentinel meaning "the console of whatever process started me", not a process id.
pub const ATTACH_PARENT_PROCESS: DWORD = u32::MAX;

#[link(name = "kernel32")]
extern "system" {
    /// Reconnect this process's standard handles to the console it was launched from.
    ///
    /// A GUI-subsystem binary inherits its parent's handles but is never *attached*, which is the
    /// half that decides where text goes on some Windows versions. `supervisor/src/main.rs` documents
    /// the same call and the same quiet failure: this returns 0 when the process was started by
    /// Explorer or the shell's autorun, and that is the normal case for an icon, not an error.
    pub fn AttachConsole(process_id: DWORD) -> BOOL;
}

#[link(name = "user32")]
extern "system" {
    /// The strings are `LPCWSTR`, so they arrive through [`wide`] and not as a `*const u8`.
    pub fn MessageBoxW(window: HWND, text: *const WORD, caption: *const WORD, kind: UINT) -> i32;
}

/// Encode a Rust string as a NUL-terminated UTF-16 buffer.
pub fn wide(text: &str) -> Vec<WORD> {
    let mut units: Vec<WORD> = text.encode_utf16().collect();
    units.push(0);
    units
}

/// Reconnect to the launching console, ignoring the ordinary case where there is none.
pub fn attach_parent_console() {
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// One modal box, no owner window, and nothing done with the answer.
///
/// This is the launcher's only reporting channel for the double-click path, and it exists because the
/// alternative is silence: a GUI-subsystem binary that cannot find its tray, writes to stderr, and is
/// double-clicked has no stderr. `smoke.ps1` records exactly that failure mode for the tray itself --
/// "no console to show the words, no window, no lock, no error, and `Start-Process` reports success".
pub fn error_box(caption: &str, text: &str) {
    let caption = wide(caption);
    let text = wide(text);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_ICONERROR | MB_SETFOREGROUND);
    }
}
