//! The whole Win32 surface of the tray, declared by hand.
//!
//! Following `core/src/ffi.rs`: the workspace's rule is that `cargo build --offline` has to work on
//! a machine whose cargo cache holds nothing beyond what is already vendored, and a notification-area
//! icon genuinely is `Shell_NotifyIconW` plus a hidden message window. Everything below is therefore
//! a declaration rather than an abstraction, and the traps that are invisible from the signature are
//! marked where they cost the most.
//!
//! Only what is actually called is declared: an unused item in a private module is a `dead_code`
//! warning, and the crate's build is required to be warning-free.

use core::ffi::c_void;
use std::mem;

pub type BOOL = i32;
pub type DWORD = u32;
pub type UINT = u32;
pub type WORD = u16;
pub type LONG = i32;
pub type LRESULT = isize;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type HWND = *mut c_void;
pub type HINSTANCE = *mut c_void;
pub type HMODULE = *mut c_void;
pub type HICON = *mut c_void;
pub type HMENU = *mut c_void;

pub const FALSE: BOOL = 0;
pub const TRUE: BOOL = 1;

// ------------------------------------------------------------------ window messages --
pub const WM_NULL: UINT = 0x0000;
pub const WM_DESTROY: UINT = 0x0002;
pub const WM_CLOSE: UINT = 0x0010;
pub const WM_TIMER: UINT = 0x0113;
pub const WM_CONTEXTMENU: UINT = 0x007B;
pub const WM_LBUTTONDBLCLK: UINT = 0x0203;
pub const WM_RBUTTONUP: UINT = 0x0205;
/// Private range: `Shell_NotifyIconW` rejects a callback message below `WM_APP`.
pub const WM_APP: UINT = 0x8000;

/// The event id is in the low word of `lParam` — and only there because the icon's callback version
/// is left at 0 by never calling `NIM_SETVERSION`. Version 4 rearranges the payload and replaces the
/// mouse messages with `NIN_*`; pystray never opted in, so neither does this, and both backends then
/// agree about which clicks mean what.
pub fn tray_event(lparam: LPARAM) -> UINT {
    ((lparam as u32) & 0xFFFF) as UINT
}

pub const SW_SHOWNORMAL: i32 = 1;

pub const WS_POPUP: DWORD = 0x8000_0000;

// --------------------------------------------------------------------------- menus --
pub const MF_STRING: UINT = 0x0000_0000;
pub const MF_GRAYED: UINT = 0x0000_0001;
pub const MF_DEFAULT: UINT = 0x0000_1000;
pub const MF_SEPARATOR: UINT = 0x0000_0800;
pub const TPM_LEFTALIGN: UINT = 0x0000_0000;
/// Bottom-align at the cursor: the tray lives at the bottom of the screen, and without this the menu
/// is drawn *down* from the click and most of it falls off the desktop.
pub const TPM_BOTTOMALIGN: UINT = 0x0000_0020;
/// Return the clicked id instead of sending `WM_COMMAND`, so every command is dispatched from one
/// place in the message loop and a dismissed menu is distinguishable from a chosen item.
pub const TPM_RETURNCMD: UINT = 0x0000_0100;

// ---------------------------------------------------------------------- notify icon --
pub const NIM_ADD: DWORD = 0x0000_0000;
pub const NIM_MODIFY: DWORD = 0x0000_0001;
pub const NIM_DELETE: DWORD = 0x0000_0002;

pub const NIF_MESSAGE: UINT = 0x0000_0001;
pub const NIF_ICON: UINT = 0x0000_0002;
pub const NIF_TIP: UINT = 0x0000_0004;
/// Balloon. It is only *raised* while this flag is set, and clearing the flag on the next
/// `NIM_MODIFY` is what dismisses it — the shell re-shows a balloon it still finds in the stored
/// struct, so a modify that rewrote the tip without `NIF_INFO` would resurrect stale text.
pub const NIF_INFO: UINT = 0x0000_0010;
/// Deliver the balloon even while the notification area is in quiet mode. Upstream's `icon.notify`
/// is an explicit "look at me now" at the moment recording starts and stops, which is exactly what
/// the default queue-and-suppress behaviour throws away.
pub const NIF_REALTIME: UINT = 0x0000_0040;

pub const NIIF_INFO: DWORD = 0x0000_0001;
pub const NIIF_ERROR: DWORD = 0x0000_0003;

/// The icon's id within the window. One icon per window, so it only has to be stable.
pub const TRAY_ICON_ID: UINT = 1;

pub const MB_OK: UINT = 0x0000_0000;
pub const MB_ICONINFORMATION: UINT = 0x0000_0040;
pub const MB_ICONWARNING: UINT = 0x0000_0030;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct POINT {
    pub x: LONG,
    pub y: LONG,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct RECT {
    pub left: LONG,
    pub top: LONG,
    pub right: LONG,
    pub bottom: LONG,
}

/// `NOTIFYICONDATAW`, all five versions of it in one struct.
///
/// Field order and widths are what make the implicit version negotiation work: the shell reads
/// `cbSize` and treats the Vista-and-later size as the layout that carries `szInfo`. Getting one
/// field wrong does not fail to compile — it fails at runtime with `Shell_NotifyIcon` returning FALSE
/// and no error code, which is why this is spelled out against the header rather than guessed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NOTIFYICONDATAW {
    pub cb_size: DWORD,
    pub hwnd: HWND,
    pub uid: UINT,
    pub u_flags: UINT,
    pub u_callback_message: UINT,
    pub h_icon: HICON,
    pub i_x: WORD,
    pub i_y: WORD,
    pub sz_tip: [WORD; 64],
    pub dw_state: DWORD,
    pub dw_state_mask: DWORD,
    pub sz_info: [WORD; 256],
    /// `uVersion` and `uTimeout` share this slot. Neither is ever set, but the union still occupies
    /// four bytes and dropping it shifts `sz_info_title` and everything after it.
    pub u_version_or_timeout: DWORD,
    pub sz_info_title: [WORD; 64],
    pub dw_info_flags: DWORD,
    pub guid_item: GUID,
    pub h_balloon_icon: HICON,
}

impl Default for NOTIFYICONDATAW {
    fn default() -> Self {
        // SAFETY: every member is an integer, a pointer or an array of those, and all-zero is a
        // valid representation of each — a null handle is exactly "not set" to this API.
        unsafe { mem::zeroed() }
    }
}

impl NOTIFYICONDATAW {
    pub fn new(hwnd: HWND) -> Self {
        NOTIFYICONDATAW { cb_size: mem::size_of::<NOTIFYICONDATAW>() as DWORD, hwnd, uid: TRAY_ICON_ID, ..Self::default() }
    }
}

#[repr(C)]
pub struct WNDCLASSEXW {
    pub cb_size: UINT,
    pub style: UINT,
    pub lpfn_wnd_proc: Option<unsafe extern "system" fn(HWND, UINT, WPARAM, LPARAM) -> LRESULT>,
    pub cb_cls_extra: LONG,
    pub cb_wnd_extra: LONG,
    pub h_instance: HINSTANCE,
    pub h_icon: HICON,
    pub h_cursor: HICON,
    pub h_brush: HICON,
    pub lpsz_menu_name: *const WORD,
    pub lpsz_class_name: *const WORD,
    pub h_icon_sm: HICON,
}

impl Default for WNDCLASSEXW {
    fn default() -> Self {
        // SAFETY: as for `NOTIFYICONDATAW` — plain integers and pointers, zero is valid for all.
        unsafe { mem::zeroed() }
    }
}

#[repr(C)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: UINT,
    pub w_param: WPARAM,
    pub l_param: LPARAM,
    pub time: DWORD,
    pub pt: POINT,
}

impl Default for MSG {
    fn default() -> Self {
        // SAFETY: integers and pointers only.
        unsafe { mem::zeroed() }
    }
}

// ---------------------------------------------------------------------- process spawn --
/// The child gets its own process group, which is what makes its pid a usable address for
/// `GenerateConsoleCtrlEvent`.
pub const CREATE_NEW_PROCESS_GROUP: DWORD = 0x0000_0200;
/// A console child of a GUI-subsystem parent would otherwise flash a window. This still creates a
/// console — it is a new console with the window suppressed — which is precisely why the graceful
/// stop works at all: with `DETACHED_PROCESS` there is no console to send a break to.
pub const CREATE_NO_WINDOW: DWORD = 0x0800_0000;

pub const CTRL_BREAK_EVENT: DWORD = 1;
pub const ATTACH_PARENT_PROCESS: DWORD = u32::MAX;

// ----------------------------------------------------------------------------- externs --

#[link(name = "kernel32")]
extern "system" {
    pub fn GetModuleHandleW(name: *const WORD) -> HINSTANCE;
    /// Console attachment for the graceful stop. A GUI-subsystem process has no console of its own,
    /// so the only way to reach a child's control-event dispatch is to attach to the child's.
    pub fn AttachConsole(process_id: DWORD) -> BOOL;
    pub fn FreeConsole() -> BOOL;
    pub fn GenerateConsoleCtrlEvent(control_event: DWORD, process_group_id: DWORD) -> BOOL;
    /// `gdiplus.dll` is resolved at run time rather than linked, so this crate needs no import
    /// library for it on a machine whose SDK does not ship one.
    pub fn LoadLibraryW(library: *const WORD) -> HMODULE;
    /// `name` is a byte string, not a wide one: `GetProcAddress` takes an `LPCSTR` while every other
    /// loader API here takes `LPCWSTR`, and passing UTF-16 to it is a silent NULL.
    pub fn GetProcAddress(module: HMODULE, name: *const i8) -> *mut c_void;
}

#[link(name = "user32")]
extern "system" {
    pub fn RegisterClassExW(class: *const WNDCLASSEXW) -> WORD;
    pub fn CreateWindowExW(
        ex_style: DWORD,
        class_name: *const WORD,
        window_name: *const WORD,
        style: DWORD,
        x: LONG,
        y: LONG,
        width: LONG,
        height: LONG,
        parent: HWND,
        menu: HMENU,
        instance: HINSTANCE,
        param: *mut c_void,
    ) -> HWND;
    pub fn DefWindowProcW(window: HWND, message: UINT, w_param: WPARAM, l_param: LPARAM) -> LRESULT;
    pub fn DestroyWindow(window: HWND) -> BOOL;
    pub fn GetMessageW(message: *mut MSG, window: HWND, filter_min: UINT, filter_max: UINT) -> BOOL;
    pub fn TranslateMessage(message: *const MSG) -> BOOL;
    pub fn DispatchMessageW(message: *const MSG) -> LRESULT;
    pub fn PostMessageW(window: HWND, message: UINT, w_param: WPARAM, l_param: LPARAM) -> BOOL;
    pub fn PostQuitMessage(exit_code: i32);
    pub fn SetTimer(window: HWND, event_id: usize, elapsed: UINT, callback: Option<unsafe extern "system" fn(HWND, UINT, usize, DWORD)>) -> usize;
    pub fn KillTimer(window: HWND, event_id: usize) -> BOOL;
    pub fn GetCursorPos(point: *mut POINT) -> BOOL;
    pub fn SetForegroundWindow(window: HWND) -> BOOL;
    pub fn CreatePopupMenu() -> HMENU;
    pub fn AppendMenuW(menu: HMENU, flags: UINT, item_id: UINT, text: *const WORD) -> BOOL;
    pub fn DestroyMenu(menu: HMENU) -> BOOL;
    pub fn TrackPopupMenuEx(menu: HMENU, flags: UINT, x: LONG, y: LONG, window: HWND, button_rect: *const RECT) -> LONG;
    pub fn MessageBoxW(window: HWND, text: *const WORD, caption: *const WORD, kind: UINT) -> i32;
    pub fn DestroyIcon(icon: HICON) -> BOOL;
}

#[link(name = "shell32")]
extern "system" {
    pub fn Shell_NotifyIconW(message: DWORD, data: *const NOTIFYICONDATAW) -> BOOL;
    /// Returns an `HINSTANCE` whose value is an error code when the low word is <= 32.
    pub fn ShellExecuteW(
        window: HWND,
        operation: *const WORD,
        file: *const WORD,
        parameters: *const WORD,
        directory: *const WORD,
        show: i32,
    ) -> isize;
}

// --------------------------------------------------------------------------- utilities --

/// Encode an API name the way the module loader wants it: NUL-terminated bytes, not UTF-16.
pub fn narrow(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// Encode a Rust string as a NUL-terminated UTF-16 buffer.
pub fn wide(text: &str) -> Vec<WORD> {
    let mut units: Vec<WORD> = text.encode_utf16().collect();
    units.push(0);
    units
}

/// Write into one of the fixed `TCHAR[N]` arrays of a Win32 struct, truncating rather than
/// overflowing: the tooltip cap is 63 code units, and a long LAN address must degrade rather than
/// corrupt the fields that follow it in memory.
pub fn write_fixed<const N: usize>(slot: &mut [WORD; N], text: &str) {
    for unit in slot.iter_mut() {
        *unit = 0;
    }
    for (slot, unit) in slot.iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
    }
}

/// `ShellExecuteW` reports failure as a value that *is* a negative error number, not a handle.
pub fn shell_execute_failed(returned: isize) -> bool {
    returned <= 32
}

/// The last Win32 error as text, for the messages shown when something could not start.
pub fn last_os_error() -> std::io::Error {
    std::io::Error::last_os_error()
}
