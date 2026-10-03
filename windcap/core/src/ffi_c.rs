//! C ABI surface consumed by `windrecorder/native_bridge.py` through `ctypes`.
//!
//! Two rules the Python host depends on:
//!   * nothing here may unwind into CPython — every entry point is `catch_unwind`ed and
//!     returns a fail-safe value, because an abort would take the recorder down with it;
//!   * buffers are bounds-checked; too small is an error code, never a partial write.

use crate::winstate;
use std::ffi::c_char;
use std::panic::{self, AssertUnwindSafe};

/// Bump on any change to the signatures or semantics below.
pub const ABI_VERSION: u32 = 1;

/// 1 = safe to capture, 0 = locked / secure desktop / no active session.
/// A panic inside the probe reports 0, i.e. we skip a frame rather than capture a locked screen.
#[no_mangle]
pub extern "C" fn windcap_recordable() -> i32 {
    match panic::catch_unwind(AssertUnwindSafe(|| winstate::snapshot().recordable())) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Seconds since last input, or -1.0 when not measurable.
#[no_mangle]
pub extern "C" fn windcap_idle_seconds() -> f64 {
    match panic::catch_unwind(AssertUnwindSafe(winstate::idle_seconds)) {
        Ok(Some(v)) => v,
        _ => -1.0,
    }
}

/// Seconds the wall clock gained over the tick counter since the previous call.
/// `GetTickCount` stops during sleep/hibernate while the wall clock does not, so a clearly
/// positive value is the one reliable, cheap signal that the machine just resumed.
#[no_mangle]
pub extern "C" fn windcap_sleep_drift_seconds() -> f64 {
    match panic::catch_unwind(AssertUnwindSafe(winstate::sleep_drift_seconds)) {
        Ok(v) => v,
        Err(_) => -1.0,
    }
}

/// Writes the input desktop name as UTF-8 into `buf` (NUL included).
/// Returns bytes written excluding NUL, -1 if the desktop could not be opened,
/// -2 if `buf` was too small, -3 on panic.
#[no_mangle]
pub unsafe extern "C" fn windcap_desktop_name(buf: *mut c_char, len: u32) -> i32 {
    if buf.is_null() {
        return -1;
    }
    let name = match panic::catch_unwind(AssertUnwindSafe(winstate::input_desktop_name)) {
        Ok(Some(n)) => n,
        Ok(None) => return -1,
        Err(_) => return -3,
    };
    let bytes = name.as_bytes();
    if (bytes.len() + 1) as u32 > len {
        return -2;
    }
    for (i, b) in bytes.iter().enumerate() {
        *buf.add(i) = *b as c_char;
    }
    *buf.add(bytes.len()) = 0;
    bytes.len() as i32
}

#[no_mangle]
pub extern "C" fn windcap_abi_version() -> u32 {
    ABI_VERSION
}
