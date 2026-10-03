//! GDI desktop grab straight into a downscaled buffer.
//!
//! Why this shape: `mss` spends 234 ms/call on this 4-monitor box because it blits the full
//! 5920x2880 union (68.4 MB) and then copies `bytes` into an ndarray, after which the caller
//! paints black bars, re-encodes a PNG, and decodes it twice more. Here `StretchBlt` samples the
//! screen *into* a small DIB section, so 68.4 MB never becomes a Rust allocation and there is no
//! second copy. GDI does the resampling; we only ever touch a couple of megabytes.
//!
//! Deliberately GDI and not DXGI: at one frame every few seconds the GPU path buys nothing, while
//! DXGI needs four per-output duplications stitched by hand, goes stale on every desktop switch
//! (`DXGI_ERROR_ACCESS_LOST`), and returns un-rotated surfaces for portrait panels. Measured here:
//! the GDI cost tracks the *source* rectangle, not the destination (137 ms at 640-wide target vs
//! 168 ms at 2560-wide), so shrinking the output is not the lever — capturing less is.

use crate::ffi;
use std::ffi::c_void;
use std::mem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDesktop {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn virtual_desktop() -> VirtualDesktop {
    unsafe {
        VirtualDesktop {
            x: ffi::GetSystemMetrics(ffi::SM_XVIRTUALSCREEN),
            y: ffi::GetSystemMetrics(ffi::SM_YVIRTUALSCREEN),
            width: ffi::GetSystemMetrics(ffi::SM_CXVIRTUALSCREEN),
            height: ffi::GetSystemMetrics(ffi::SM_CYVIRTUALSCREEN),
        }
    }
}

/// GDI metrics are documented as not DPI aware; without this a scaled display reports virtual
/// coordinates that do not match the pixels `StretchBlt` actually reads. Call once per thread.
pub fn make_thread_dpi_aware() {
    unsafe {
        // Fails (returns NULL) if the process manifest already set this; that is fine.
        ffi::SetThreadDpiAwarenessContext(ffi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// One physical display, in the order `EnumDisplayMonitors` reports them.
///
/// `index` is 1-based to match the two consumers that matter: `mss` exposes `monitors[0]` as the
/// union and `monitors[1..]` as the displays, and the config key `record_single_display_index` is
/// documented against that same numbering. A 0-based native list would be tidier and would record
/// the wrong monitor for every existing user's setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub index: usize,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

impl Monitor {
    pub fn rect(&self) -> VirtualDesktop {
        VirtualDesktop { x: self.x, y: self.y, width: self.width, height: self.height }
    }

    pub fn megapixels(&self) -> f64 {
        f64::from(self.width * self.height) / 1e6
    }
}

/// Every monitor attached to the desktop.
///
/// The callback writes into a `Vec` handed through `lParam`, which is what keeps this free of a
/// global: `EnumDisplayMonitors` runs the callback synchronously on this thread before returning, so
/// the borrow of the local vector is sound for the duration of the call.
pub fn monitors() -> Vec<Monitor> {
    let mut found: Vec<Monitor> = Vec::new();
    unsafe {
        unsafe extern "system" fn collect(
            handle: *mut c_void,
            _dc: *mut c_void,
            rect: *mut ffi::RECT,
            data: *mut c_void,
        ) -> ffi::BOOL {
            if rect.is_null() {
                return ffi::TRUE;
            }
            let out = &mut *(data as *mut Vec<Monitor>);
            // `lprcMonitor` already carries the rectangle, so `GetMonitorInfo` is asked for nothing
            // beyond the flags bit that says whether this is the primary panel.
            let mut info: ffi::MONITORINFO = mem::zeroed();
            info.cb_size = mem::size_of::<ffi::MONITORINFO>() as ffi::DWORD;
            let primary = ffi::GetMonitorInfoW(handle, &mut info) == ffi::TRUE
                && info.dw_flags & ffi::MONITORINFOF_PRIMARY != 0;
            let r = &*rect;
            out.push(Monitor {
                index: out.len() + 1,
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
                primary,
            });
            ffi::TRUE
        }

        ffi::EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            &mut found as *mut Vec<Monitor> as *mut c_void,
        );
    }
    found
}

/// The display a `record_single_display_index` setting refers to.
pub fn monitor_rect(index: i32) -> Option<VirtualDesktop> {
    if index < 1 {
        return None;
    }
    monitors().into_iter().nth((index - 1) as usize).map(|m| m.rect())
}

/// Bounds of the window that currently has keyboard focus, in virtual-desktop coordinates.
///
/// The deployed config sets `record_screenshot_method_capture_foreground_window_only = true`, so
/// this — not the 17 MP monitor union — is the realistic source: frames on disk are 0.4-3.6 MP.
pub fn foreground_rect() -> Option<VirtualDesktop> {
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetForegroundWindow() -> *mut c_void;
        fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
    }
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut r = Rect { left: 0, top: 0, right: 0, bottom: 0 };
        if GetWindowRect(hwnd, &mut r) == 0 {
            return None;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return None;
        }
        Some(VirtualDesktop { x: r.left, y: r.top, width: w, height: h })
    }
}

#[derive(Debug)]
pub enum CaptureError {
    BadGeometry(VirtualDesktop),
    CreateDibSection,
    StretchBlt(u32),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::BadGeometry(g) => write!(f, "unusable capture geometry {g:?}"),
            CaptureError::CreateDibSection => write!(f, "CreateDIBSection failed"),
            CaptureError::StretchBlt(code) => write!(f, "StretchBlt failed, GetLastError={code}"),
        }
    }
}

/// One grabbed frame, handed out by value so the caller never holds a borrow of the grabber.
#[derive(Debug, Clone)]
pub struct Grab {
    pub width: u32,
    pub height: u32,
    /// Rec.601 luma plane; what the change gate scores.
    pub luma: Vec<u8>,
    /// Same frame as RGB; what the OCR input and the thumbnail are encoded from.
    pub rgb: Vec<u8>,
}

/// A reusable grabber owning one memory DC and one DIB section, rebuilt only when the requested
/// width or the source rectangle changes.
pub struct Grabber {
    mem_dc: *mut c_void,
    bitmap: *mut c_void,
    previous_object: *mut c_void,
    bits: *mut u8,
    target_width: u32,
    target_height: u32,
    desktop: VirtualDesktop,
    /// True when `desktop` tracks the live virtual screen, so a topology change must be detected.
    track_topology: bool,
    luma: Vec<u8>,
    rgb: Vec<u8>,
}

unsafe impl Send for Grabber {}

impl Grabber {
    /// Grab the whole virtual desktop (union of every monitor, negative origin included).
    pub fn new(target_width: u32) -> Result<Grabber, CaptureError> {
        Grabber::with_source(target_width, virtual_desktop(), true)
    }

    /// Grab one fixed rectangle, e.g. the foreground window's bounds.
    pub fn with_source(
        target_width: u32,
        desktop: VirtualDesktop,
        track_topology: bool,
    ) -> Result<Grabber, CaptureError> {
        if desktop.width <= 0 || desktop.height <= 0 {
            return Err(CaptureError::BadGeometry(desktop));
        }
        let target_height =
            ((target_width as i64 * desktop.height as i64) / desktop.width as i64).max(1) as u32;
        let pixels = (target_width * target_height) as usize;

        unsafe {
            let mem_dc = ffi::CreateCompatibleDC(std::ptr::null_mut());
            if mem_dc.is_null() {
                return Err(CaptureError::CreateDibSection);
            }

            let header =
                ffi::BITMAPINFOHEADER::new_rgb32_top_down(target_width as i32, target_height as i32);
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = ffi::CreateDIBSection(
                mem_dc,
                &header,
                ffi::DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if bitmap.is_null() || bits.is_null() {
                ffi::DeleteDC(mem_dc);
                return Err(CaptureError::CreateDibSection);
            }

            let previous_object = ffi::SelectObject(mem_dc, bitmap);
            ffi::SetStretchBltMode(mem_dc, ffi::HALFTONE);

            Ok(Grabber {
                mem_dc,
                bitmap,
                previous_object,
                bits: bits as *mut u8,
                target_width,
                target_height,
                desktop,
                track_topology,
                luma: vec![0u8; pixels],
                rgb: vec![0u8; pixels * 3],
            })
        }
    }

    pub fn source(&self) -> VirtualDesktop {
        self.desktop
    }

    pub fn target_size(&self) -> (u32, u32) {
        (self.target_width, self.target_height)
    }

    /// Grab one frame. `Ok(None)` means the live virtual desktop no longer matches the geometry
    /// this grabber was built for, which the caller answers by rebuilding it.
    pub fn grab(&mut self) -> Result<Option<Grab>, CaptureError> {
        let src = if self.track_topology {
            let now = virtual_desktop();
            if now != self.desktop {
                return Ok(None);
            }
            now
        } else {
            self.desktop
        };

        unsafe {
            let screen_dc = ffi::GetDC(std::ptr::null_mut());
            let ok = ffi::StretchBlt(
                self.mem_dc,
                0,
                0,
                self.target_width as i32,
                self.target_height as i32,
                screen_dc,
                src.x,
                src.y,
                src.width,
                src.height,
                ffi::SRCCOPY,
            );
            ffi::ReleaseDC(std::ptr::null_mut(), screen_dc);

            if ok != ffi::TRUE {
                return Err(CaptureError::StretchBlt(ffi::GetLastError()));
            }
            self.convert();
        }

        Ok(Some(Grab {
            width: self.target_width,
            height: self.target_height,
            luma: self.luma.clone(),
            rgb: self.rgb.clone(),
        }))
    }

    /// Single pass over the DIB: derive the luma plane the gate scores and the RGB the OCR input
    /// and thumbnail encode from. This one pass is what removes the Python path's three
    /// full-frame PNG decode/encode round-trips.
    #[inline]
    fn convert(&mut self) {
        let (w, h) = (self.target_width as usize, self.target_height as usize);
        let stride = w * 4;
        // Copy the pointer out first so the slice does not borrow `self`, which the two
        // destination buffers below need mutably.
        let bits = self.bits;
        let luma = &mut self.luma[..w * h];
        let rgb = &mut self.rgb[..w * h * 3];
        let src = unsafe { std::slice::from_raw_parts(bits, stride * h) };

        for (i, px) in src.chunks_exact(4).enumerate() {
            // DIB order is little-endian 00RRGGBB, i.e. the bytes are B, G, R, A.
            let (b, g, r) = (u32::from(px[0]), u32::from(px[1]), u32::from(px[2]));
            // Rec.601 weights summing to 256, so the shift is exact and matches cv2's grayscale.
            luma[i] = ((b * 29 + g * 150 + r * 77) >> 8) as u8;
            let o = i * 3;
            rgb[o] = px[2];
            rgb[o + 1] = px[1];
            rgb[o + 2] = px[0];
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        unsafe {
            if !self.previous_object.is_null() {
                ffi::SelectObject(self.mem_dc, self.previous_object);
            }
            if !self.bitmap.is_null() {
                ffi::DeleteObject(self.bitmap);
            }
            if !self.mem_dc.is_null() {
                ffi::DeleteDC(self.mem_dc);
            }
        }
    }
}

/// Geometry is machine-specific, so nothing here asserts a size. What is asserted is the
/// invariants a caller relies on when it picks a display by index: the numbering is dense and
/// 1-based, exactly one panel is primary, and every rect lies inside the union it is grabbed from.
#[cfg(test)]
mod monitor_tests {
    use super::*;

    /// Geometry is machine-specific, so nothing here asserts a size. What is asserted is the
    /// invariants a caller relies on when it picks a display by index: the numbering is dense and
    /// 1-based, exactly one panel is primary, and every rect lies inside the union it is grabbed from.
    #[test]
    fn monitors_are_numbered_from_one_and_covered_by_the_virtual_desktop() {
        make_thread_dpi_aware();
        let list = monitors();
        assert!(!list.is_empty(), "every Windows desktop has at least one monitor");
        let union = virtual_desktop();
        let mut primary = 0;
        for (position, monitor) in list.iter().enumerate() {
            assert_eq!(monitor.index, position + 1, "index must be mss-compatible");
            assert!(monitor.width > 0 && monitor.height > 0, "{monitor:?} has no pixels");
            assert!(
                monitor.x >= union.x
                    && monitor.y >= union.y
                    && monitor.x + monitor.width <= union.x + union.width
                    && monitor.y + monitor.height <= union.y + union.height,
                "{monitor:?} lies outside {union:?} - the grab would read the wrong pixels"
            );
            if monitor.primary {
                primary += 1;
            }
        }
        assert_eq!(primary, 1, "exactly one primary display");
    }

    #[test]
    fn an_index_outside_the_display_set_resolves_to_nothing_rather_than_a_guess() {
        let count = monitors().len() as i32;
        assert!(monitor_rect(0).is_none(), "0 is not a valid display number");
        assert!(monitor_rect(-1).is_none());
        assert!(monitor_rect(count + 5).is_none());
        assert_eq!(monitor_rect(1), monitors().first().map(|m| m.rect()));
    }
}
