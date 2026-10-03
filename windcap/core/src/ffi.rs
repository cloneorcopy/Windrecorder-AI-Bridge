//! Raw Win32 bindings. Deliberately no `windows`/`winapi` crate: the whole point of this
//! core is a dependency tree a stranger can build offline.

use core::ffi::{c_int, c_void};
use std::mem;

pub type BOOL = i32;
pub type HANDLE = *mut c_void;
pub type UINT = u32;
pub type DWORD = u32;
pub type HRESULT = i32;

pub const TRUE: BOOL = 1;

/// `LASTINPUTINFO.cbSize` must be set to `size_of` before the call, or the API fails with
/// ERROR_INVALID_PARAMETER and reports nothing useful.
#[repr(C)]
#[derive(Debug, Default)]
pub struct LASTINPUTINFO {
    pub cb_size: UINT,
    pub dw_time: DWORD,
}

pub const DESKTOP_READOBJECTS: UINT = 0x0002;
pub const UOI_NAME: c_int = 2;

/// `SHQueryUserNotificationState` outcomes we actually branch on.
pub mod quns {
    pub const NOT_PRESENT: i32 = 1;
    pub const BUSY: i32 = 2;
    pub const RUNNING_ON_BATTERY: i32 = 3;
    pub const RUNNING_ON_BATTERY_CRITICAL: i32 = 4;
    pub const ACCEPTS_NOTIFICATIONS: i32 = 5;
    pub const QUIET_TIME: i32 = 6;
    pub const APP: i32 = 7;
    pub const SUSPENDING: i32 = 8;
}

#[link(name = "user32")]
extern "system" {
    /// NULL on failure. On a locked / secure desktop the open itself is denied.
    pub fn OpenInputDesktop(flags: UINT, inherit: BOOL, access: UINT) -> HANDLE;
    pub fn GetUserObjectInformationW(
        obj: HANDLE,
        index: c_int,
        info: *mut c_void,
        size: UINT,
        needed: *mut UINT,
    ) -> BOOL;
    pub fn CloseDesktop(desktop: HANDLE) -> BOOL;
    pub fn GetLastInputInfo(info: *mut LASTINPUTINFO) -> BOOL;
    pub fn GetLastError() -> DWORD;
}

#[link(name = "kernel32")]
extern "system" {
    /// 32-bit millisecond counter that excludes sleep/hibernate. Pair with `dw_time` via
    /// wrapping subtraction; never mix with wall clock or GetTickCount64.
    pub fn GetTickCount() -> DWORD;
}

#[link(name = "shell32")]
extern "system" {
    pub fn SHQueryUserNotificationState(state: *mut i32) -> HRESULT;
}

// --------------------------------------------------------------------------- capture (GDI) --

pub type HDC = *mut c_void;
pub type HBITMAP = *mut c_void;
pub type HGDIOBJ = *mut c_void;

pub const SM_XVIRTUALSCREEN: c_int = 76;
pub const SM_YVIRTUALSCREEN: c_int = 77;
pub const SM_CXVIRTUALSCREEN: c_int = 78;
pub const SM_CYVIRTUALSCREEN: c_int = 79;

/// Averaging mode; `COLORONCOLOR` (3) drops pixels, `HALFTONE` (4) filters and is the one that
/// keeps thin high-contrast text legible after a 3x decimation.
pub const HALFTONE: c_int = 4;
pub const SRCCOPY: u32 = 0x00CC_0033;
pub const DIB_RGB_COLORS: u32 = 0;
pub const BI_RGB: u32 = 0;

/// `GDI32!BITMAPINFOHEADER`. A negative `bi_height` requests a top-down DIB, which is what makes
/// row 0 of the buffer the top of the screen; without it the image arrives vertically flipped.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BITMAPINFOHEADER {
    pub bi_size: u32,
    pub bi_width: i32,
    pub bi_height: i32,
    pub bi_planes: u16,
    pub bi_bit_count: u16,
    pub bi_compression: u32,
    pub bi_size_image: u32,
    pub bi_x_pixels_per_meter: i32,
    pub bi_y_pixels_per_meter: i32,
    pub bi_clr_used: u32,
    pub bi_clr_important: u32,
}

impl BITMAPINFOHEADER {
    pub fn new_rgb32_top_down(width: i32, height: i32) -> Self {
        BITMAPINFOHEADER {
            bi_size: mem::size_of::<BITMAPINFOHEADER>() as u32,
            bi_width: width,
            bi_height: -height,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: BI_RGB,
            bi_size_image: 0,
            bi_x_pixels_per_meter: 0,
            bi_y_pixels_per_meter: 0,
            bi_clr_used: 0,
            bi_clr_important: 0,
        }
    }
}

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`, a pseudo-handle of value -4.
pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: HANDLE = -4isize as HANDLE;

#[link(name = "gdi32")]
extern "system" {
    pub fn CreateCompatibleDC(hdc: HDC) -> HDC;
    pub fn CreateDIBSection(
        hdc: HDC,
        info: *const BITMAPINFOHEADER,
        usage: u32,
        bits: *mut *mut c_void,
        section: HANDLE,
        offset: u32,
    ) -> HBITMAP;
    pub fn SelectObject(hdc: HDC, obj: HGDIOBJ) -> HGDIOBJ;
    pub fn DeleteObject(obj: HGDIOBJ) -> BOOL;
    pub fn DeleteDC(hdc: HDC) -> BOOL;
    pub fn StretchBlt(
        dst: HDC,
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
        src: HDC,
        sx: c_int,
        sy: c_int,
        sw: c_int,
        sh: c_int,
        rop: u32,
    ) -> BOOL;
    pub fn SetStretchBltMode(hdc: HDC, mode: c_int) -> c_int;
}

/// The part of a monitor's description every caller actually needs.
///
/// `MONITORINFOEXW` is deliberately *not* used, even though it is the struct that carries the device
/// name: the shipped `GetMonitorInfo` compares `cbSize` against its internal tagMONITORINFOEX, which
/// has an extra trailing LONG, so a correct-by-the-documents 360 is rejected with
/// ERROR_INVALID_PARAMETER while 40 succeeds. Measured here on Windows 10 19045: `cbSize=40` returns
/// TRUE, `cbSize=360` returns FALSE with error 87. Asking for the plain MONITORINFO is both the
/// documented path and the one that works, and the device name is not worth an undocumented layout.
#[repr(C)]
pub struct RECT {
    pub left: c_int,
    pub top: c_int,
    pub right: c_int,
    pub bottom: c_int,
}

#[repr(C)]
pub struct MONITORINFO {
    pub cb_size: DWORD,
    pub rc_monitor: RECT,
    pub rc_work: RECT,
    pub dw_flags: DWORD,
}

/// `MONITORINFOF_PRIMARY`; the flags field also carries the old `MONITORINFOF_COMPOSITE`.
pub const MONITORINFOF_PRIMARY: DWORD = 0x0000_0001;

#[link(name = "user32")]
extern "system" {
    /// `lParam` is handed straight to the callback, which is how the enumeration collects into a
    /// `Vec` without a global.
    pub fn EnumDisplayMonitors(
        hdc: *mut c_void,
        lprc_clip: *const RECT,
        callback: Option<
            unsafe extern "system" fn(*mut c_void, *mut c_void, *mut RECT, *mut c_void) -> BOOL,
        >,
        data: *mut c_void,
    ) -> BOOL;
    pub fn GetMonitorInfoW(handle: *mut c_void, info: *mut MONITORINFO) -> BOOL;
}

#[link(name = "user32")]
extern "system" {
    pub fn GetDesktopWindow() -> HANDLE;
    pub fn GetDC(window: HANDLE) -> HDC;
    pub fn ReleaseDC(window: HANDLE, hdc: HDC) -> c_int;
    pub fn GetSystemMetrics(index: c_int) -> c_int;
    pub fn SetThreadDpiAwarenessContext(context: HANDLE) -> HANDLE;
}

