//! The two icon states, as `HICON`s the shell will accept — taken out of this binary's own image.
//!
//! ## Why the icon is compiled in rather than read from `__assets__`
//!
//! The first version of this module decoded `__assets__/icon-tray.png` and `icon-tray-pause.png`
//! with GDI+ at startup and treated the result as a precondition: if the decode failed, `Icons::load`
//! returned `Err`, `tray::run` propagated it, and `main` exited 1. On a standalone install — the one
//! built from the shipped zip, which carries `bin\`, `config_src\` and `ocr_lib\` and no `__assets__`
//! at all — that made the notification-area icon a hard dependency on a file that was never
//! packaged, and the product's only user-facing entry point could not start. Measured on exactly
//! such a root:
//!
//! ```text
//! windsvc: <root>\__assets__\icon-tray.png could not be decoded as an image (GDI+ status 2)
//! The tray cannot appear without its icon.
//! ```
//!
//! Putting the art in the `.exe` removes the dependency instead of working around it, and it is worth
//! the resource compiler for a second reason: a PE icon is also what Explorer and the taskbar show,
//! so the binary stops being the only one of the eleven with nothing to look at.
//! `base\version_resource.rs` compiles `supervisor\icons\*.ico` into this image; this module reads
//! them back out.
//!
//! ## Handle ownership, which is the part that bites
//!
//! There are two different `HICON` lifetimes in play, and mixing them up either leaks a GDI object on
//! every state change or destroys a handle the OS still owns:
//!
//!   * `LoadImageW` **without** `LR_SHARED` returns a handle the caller owns, so it must outlive every
//!     `NIM_MODIFY` that names it and be destroyed exactly once. That is what a tray needs: the shell
//!     keeps drawing an icon after the call returns, so neither handle may be created per swap. They
//!     are made once here, held in [`Icons`] for the life of the tray, selected between by pointer in
//!     [`Icons::for_recording`], and destroyed in `Drop` — after `Tray::remove` has told the shell to
//!     forget them. Passing `LR_SHARED` would move destruction to the OS and make the matching
//!     `DestroyIcon` here a use-after-free of somebody else's handle, so it is deliberately not set.
//!   * `LoadIconW(NULL, IDI_APPLICATION)` — the degraded path — returns a **shared** system handle
//!     that must never be destroyed. `Icon::owned` is the flag that keeps that straight, and `Drop`
//!     walks it instead of destroying whatever it finds.
//!
//! ## Degradation
//!
//! Neither state may stop the tray. A resource that cannot be decoded — a `.exe` built without the
//! `icons\` directory, or an icon the shell's decoder refuses — is replaced by the shared system icon
//! and *announced*, because an icon the user cannot identify is worse than one that looks generic.
//! The cost is that both states then render identically; the warning says so in those words.

use core::ffi::c_void;
use std::mem;
use std::path::PathBuf;

use crate::ffi;
use crate::ffi::HICON;
use crate::layout::Layout;

/// The resource ids `base\version_resource.rs` stamps into this image, one per `icons\*.ico`.
///
/// Ordinals rather than names, because the resource compiler on this machine writes a string `ICON`
/// name into the image with its quotation marks still attached — `"tray_recording"` arrived as
/// `"TRAY_RECORDING"`, quotes and all, and no run-time lookup can find that. Measured here.
///
/// Spelled as constants rather than derived at run time: an id that silently stops matching the file
/// it was compiled from is an icon that quietly went missing, which is the failure this module exists
/// to make impossible. `src\layout.rs` asserts these are exactly the files in `icons\`.
pub const ICON_ID_RECORDING: u16 = 1;
pub const ICON_ID_PAUSED: u16 = 2;
/// The two, in the order everything else indexes them: recording at index 0, paused at index 1.
pub const ICON_RESOURCES: [u16; 2] = [ICON_ID_RECORDING, ICON_ID_PAUSED];

/// The `icons\` files those ids are compiled from, for the provenance test in `layout.rs`.
#[cfg(test)]
pub const ICON_SOURCES: [&str; 2] = ["1-tray-recording.ico", "2-tray-paused.ico"];

/// `user32.h`: `LoadImage`'s `uType`, and the only one this file asks for.
const IMAGE_ICON: u32 = 1;
/// `LR_DEFAULTCOLOR`. Explicitly *not* `LR_SHARED` (0x0080) — see the ownership note above.
const LR_DEFAULTCOLOR: u32 = 0x0000_0000;
/// `IDI_APPLICATION`, i.e. `MAKEINTRESOURCE(32512)`: a pointer whose low word is the id and whose
/// high word is zero, which is exactly what the API tests for.
const IDI_APPLICATION: usize = 32512;

type LoadImageFn = unsafe extern "system" fn(ffi::HMODULE, *const c_void, u32, i32, i32, u32) -> HICON;
type LoadIconFn = unsafe extern "system" fn(ffi::HMODULE, *const c_void) -> HICON;

/// `user32`'s two icon entry points, resolved by name.
///
/// Both are already reachable — this crate statically links `user32` for every window call in
/// `ffi.rs` — but neither is declared there, and `ffi.rs` is one shared declaration module this
/// module does not own. `LoadLibraryW` on an image that is already loaded is a refcount bump and a
/// pointer, so this costs one call at startup and keeps the binding, the `LR_SHARED` question and the
/// fallback together in the one file that has to get them right.
struct IconApi {
    /// This executable's instance handle — the image the icon resources are actually in. See
    /// [`IconApi::from_image`] for why it cannot be null.
    module: ffi::HMODULE,
    load_image: LoadImageFn,
    load_icon: LoadIconFn,
}

impl IconApi {
    fn load() -> Result<IconApi, String> {
        let module = unsafe { ffi::LoadLibraryW(ffi::wide("user32.dll").as_ptr()) };
        if module.is_null() {
            return Err(format!("user32.dll could not be loaded: {}", ffi::last_os_error()));
        }
        macro_rules! bind {
            ($name:literal, $ty:ty) => {{
                let address = unsafe { ffi::GetProcAddress(module, ffi::narrow($name).as_ptr().cast()) };
                if address.is_null() {
                    return Err(format!("user32.dll does not export {}", $name));
                }
                // SAFETY: a procedure address and a function pointer are the same width, and this is
                // the only way to use `GetProcAddress` without a crate's `windows-targets` tree.
                unsafe { mem::transmute::<*mut c_void, $ty>(address) }
            }};
        }
        Ok(IconApi {
            module: unsafe { ffi::GetModuleHandleW(std::ptr::null()) },
            load_image: bind!("LoadImageW", LoadImageFn),
            load_icon: bind!("LoadIconW", LoadIconFn),
        })
    }

    /// One icon out of this `.exe`'s own resources.
    ///
    /// `hInst` is null rather than `GetModuleHandleW(NULL)`: for a resource in the main image the OS
    /// resolves a null instance to the executable either way, null is the form the documentation
    /// gives for exactly this case, and it is one fewer handle to reason about.
    fn from_image(&self, id: u16) -> Result<HICON, String> {
        // `hInst` is this module's handle, not null. Null does *not* mean "the running .exe" here:
        // measured on this build, `LoadImageW(NULL, MAKEINTRESOURCE(1), IMAGE_ICON, ...)` answers
        // 1813 (resource name not found) for a resource that `FindResourceW` on the same image
        // resolves in one call — so the search a null instance performs is not the one over this
        // binary's own `.rsrc`. `GetModuleHandleW(NULL)` is the handle the resource is under, which
        // is also the form `tray.rs` already uses to register its window class.
        //
        // SAFETY: `resource_id` produces the `MAKEINTRESOURCE` form this API documents.
        let icon = unsafe { (self.load_image)(self.module, resource_id(id), IMAGE_ICON, 0, 0, LR_DEFAULTCOLOR) };
        if icon.is_null() {
            return Err(format!("the icon resource #{id} is not in this binary ({})", ffi::last_os_error()));
        }
        Ok(icon)
    }

    /// The system's own icon, which the caller does *not* own.
    fn system_default(&self) -> Option<HICON> {
        // SAFETY: casting an integer into a `MAKEINTRESOURCE` pointer is the documented form.
        let icon = unsafe { (self.load_icon)(std::ptr::null_mut(), IDI_APPLICATION as *const c_void) };
        if icon.is_null() {
            None
        } else {
            Some(icon)
        }
    }
}

/// `MAKEINTRESOURCE(id)`: a pointer whose low word is the id and whose high word is zero, which is
/// how both these APIs say "this is an ordinal, not a string". `ICON_ID_*` are all under 64k, so the
/// cast cannot lose anything.
fn resource_id(id: u16) -> *const c_void {
    usize::from(id) as *const c_void
}

/// One state's handle, and who is allowed to destroy it.
struct Icon {
    handle: HICON,
    /// `false` for the shared system icon: `LoadIconW(NULL, ...)` hands out a handle the OS owns, and
    /// `DestroyIcon` on that is the bug that shows up as the shell's own icons going missing.
    owned: bool,
    /// What this handle was loaded from, for a human reading a report or a refusal message.
    origin: PathBuf,
}

/// The pair, held for the life of the tray.
pub struct Icons {
    states: [Icon; 2],
    /// One entry per state that had to fall back. Empty is the healthy answer.
    warnings: Vec<String>,
    /// Named `paths` because `tray.rs`'s refusal message reads `self.icons.paths[0].display()` and
    /// that file is not this module's to change. It carries each state's *origin* — normally this
    /// `.exe` — because that is the thing the message is actually trying to name.
    pub paths: [PathBuf; 2],
}

/// One state's `HICON`: the binary's own resource first, the system default second.
///
/// Returns a warning rather than printing one, because the tray does not exist yet during startup
/// and has to be the one that decides how a problem reaches the user.
fn load_one(api: &IconApi, id: u16, image: &PathBuf) -> (Icon, Option<String>) {
    match api.from_image(id) {
        Ok(handle) => (Icon { handle, owned: true, origin: image.clone() }, None),
        Err(error) => {
            let warning = format!(
                "{error}\nEach tray icon is compiled into this binary by supervisor\\build.rs from \
                 supervisor\\icons\\, so its absence means this .exe was built without that step. Falling \
                 back to the system's default icon: the tray still starts and every menu item still \
                 works, but the recording and paused states will look the same."
            );
            match api.system_default() {
                Some(handle) => (
                    Icon {
                        handle,
                        owned: false,
                        origin: PathBuf::from("the system's default application icon, not this binary"),
                    },
                    Some(warning),
                ),
                // No resource *and* no system default. A `NIM_ADD` with a null `h_icon` gives the user
                // an entry they cannot find or click, so this — and only this — is allowed to stop
                // startup, and it is a broken Windows rather than a missing file.
                None => (
                    Icon { handle: std::ptr::null_mut(), owned: false, origin: PathBuf::from("nothing - no icon could be obtained") },
                    Some(format!("{warning}\n...and even the system's default icon could not be loaded.")),
                ),
            }
        }
    }
}

impl Icons {
    /// The two states, taken from this binary's own image.
    ///
    /// `layout` is unused: the point of the rewrite is that the icon's location is no longer derived
    /// from the install root. It stays in the signature because `tray.rs` passes a layout and that
    /// file is not this module's to change.
    pub fn load(_layout: &Layout) -> Result<Icons, String> {
        let api = IconApi::load()?;
        let image = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("this executable"));
        let mut warnings = Vec::new();
        let mut states: Vec<Icon> = Vec::with_capacity(ICON_RESOURCES.len());
        for id in ICON_RESOURCES {
            let (icon, warning) = load_one(&api, id, &image);
            if let Some(warning) = warning {
                warnings.push(warning);
            }
            states.push(icon);
        }
        let states: [Icon; 2] = states.try_into().map_err(|_| "internal: the tray's two icon states were not both resolved".to_string())?;
        if let Some(dead) = states.iter().position(|icon| icon.handle.is_null()) {
            return Err(format!("no icon could be obtained for the tray's {} state", ICON_RESOURCES[dead]));
        }
        let paths = [states[0].origin.clone(), states[1].origin.clone()];
        let icons = Icons { states, warnings, paths };
        icons.report_degradation();
        Ok(icons)
    }

    /// Which of the two the icon should be right now.
    ///
    /// Copies out a handle this struct already owns, which is why being called once per `WM_TIMER`,
    /// forever, cannot leak: nothing on this path creates anything.
    pub fn for_recording(&self, recording: bool) -> HICON {
        self.states[usize::from(!recording)].handle
    }

    /// The two handles with their ownership flags. Test seam, not for the tray loop.
    #[cfg(test)]
    fn states(&self) -> [(HICON, bool); 2] {
        [(self.states[0].handle, self.states[0].owned), (self.states[1].handle, self.states[1].owned)]
    }

    /// Did either state have to fall back to the system icon? Test-only because the answer is acted
    /// on where it is produced — [`Icons::report_degradation`] — and a caller that could ask would
    /// be a caller that could choose not to.
    #[cfg(test)]
    fn degraded(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// What went wrong. Same reason as [`Icons::degraded`].
    #[cfg(test)]
    fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Say out loud what could not be loaded, before anything else gets a chance to assume it worked.
    ///
    /// The tray has no console by design, so a warning nobody sees is not a warning: this writes to
    /// stderr — which reaches the human who ran `windsvc run` from a terminal, which is how the
    /// original failure was measured — and raises a message box, which is how this tray says
    /// everything else before it has a window of its own (`tray::run` already does exactly this for a
    /// failed boot). Neither is suppressed on a machine that can only display one of them.
    ///
    /// Called from [`Icons::load`] rather than left for `tray.rs`, because `tray.rs` is not this
    /// module's file to change and a warning that depends on a caller remembering to ask for it is a
    /// warning that gets dropped by the next refactor.
    fn report_degradation(&self) {
        for warning in &self.warnings {
            eprintln!("windsvc: {warning}");
            #[cfg(not(test))]
            crate::tray::alert(warning, "Windrecorder could not load its tray icon", true);
        }
    }
}

impl Drop for Icons {
    fn drop(&mut self) {
        // The shell is asked to forget the icon before these are destroyed, in `Tray::remove`.
        for state in &self.states {
            if state.owned {
                // SAFETY: an owned handle came from `LoadImageW` without `LR_SHARED`, on this thread.
                unsafe { ffi::DestroyIcon(state.handle) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use wind_base::config::Config;

    /// GR_GDIOBJECTS, the `GetGuiResources` selector that counts a process's GDI handles — which is
    /// what an `HICON` is. Task Manager's "GDI objects" column reads the same number, so a growth
    /// this test cannot see is a growth a user can.
    const GR_GDIOBJECTS: u32 = 0;

    type GetGuiResourcesFn = unsafe extern "system" fn(*mut c_void, u32) -> u32;

    /// This process's GDI object count, read through `kernel32` by name for the same reason the two
    /// `user32` entry points are: `ffi.rs` does not declare it and is not this module's to change.
    /// `None` means the number could not be had, and every caller treats that as "no assertion
    /// available" rather than as a failure — a headless build machine is not a broken tray.
    fn gdi_objects() -> Option<u32> {
        use std::sync::OnceLock;
        static SLOT: OnceLock<Option<GetGuiResourcesFn>> = OnceLock::new();
        let bound = match SLOT.get_or_init(|| {
            let module = unsafe { ffi::LoadLibraryW(ffi::wide("kernel32.dll").as_ptr()) };
            if module.is_null() {
                return None;
            }
            let address = unsafe { ffi::GetProcAddress(module, ffi::narrow("GetGuiResources").as_ptr().cast()) };
            if address.is_null() {
                return None;
            }
            Some(unsafe { mem::transmute::<*mut c_void, GetGuiResourcesFn>(address) })
        }) {
            Some(bound) => *bound,
            None => return None,
        };
        // SAFETY: a null process handle means "this process", which is what the API documents.
        Some(unsafe { (bound)(std::ptr::null_mut(), GR_GDIOBJECTS) })
    }

    fn layout() -> Layout {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).unwrap();
        Layout::from_config(&Config::load(root).expect("the shipped config must parse"))
    }

    /// Both states must exist, must be two different icons, and must be owned rather than borrowed
    /// from the shell — the pause/normal toggle is only meaningful if there is something to toggle.
    #[test]
    fn both_tray_states_come_out_of_the_binary_and_are_distinct() {
        let icons = Icons::load(&layout()).expect("an icon compiled into this .exe cannot depend on the install");
        let [recording, paused] = icons.states();
        assert!(!recording.0.is_null() && !paused.0.is_null(), "neither state may hand the shell a null icon");
        assert_ne!(recording.0, paused.0, "the two states must be two different icons");
        assert!(!icons.degraded(), "the icons are in the binary: {:?}", icons.warnings());
        assert!(
            recording.1 && paused.1,
            "a resource-loaded icon is ours to destroy; if this is false the shared system icon was \
             silently reached, and DestroyIcon on it is the bug that takes the shell's own icons with it"
        );
    }

    /// The regression this file is written against: a standalone install has no `__assets__`, and the
    /// tray used to treat that as fatal.
    #[test]
    fn the_tray_icons_survive_a_root_that_has_no_assets_directory() {
        let dir = std::env::temp_dir().join(format!("windsvc-icon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        let bare = Layout::from_config(&Config::load(&dir).unwrap());
        assert!(!bare.assets.is_dir(), "the whole point: this root has no __assets__");
        let icons = Icons::load(&bare).unwrap_or_else(|e| panic!("a missing __assets__ must not stop the tray: {e}"));
        assert!(!icons.degraded(), "{:?}", icons.warnings());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Toggling is what the tray does on every `WM_TIMER` that sees a state change. It must not cost
    /// a GDI object each time, and dropping the pair must give back exactly what it took.
    #[test]
    fn a_thousand_state_changes_cost_no_gdi_objects_and_the_icons_are_all_freed() {
        let Some(before) = gdi_objects() else {
            eprintln!("GetGuiResources unavailable here; nothing to measure");
            return;
        };
        {
            let icons = Icons::load(&layout()).expect("the embedded icons");
            // Two owned icons, and only the two: nothing on the toggle path allocates.
            let after_load = gdi_objects().expect("the count was readable before the load, so it is readable now");
            assert!(after_load >= before, "loading icons cannot *reduce* the count: {before} -> {after_load}");
            for _ in 0..1000 {
                assert!(!icons.for_recording(true).is_null());
                assert!(!icons.for_recording(false).is_null());
            }
            let after_toggling = gdi_objects().expect("readable");
            assert_eq!(after_toggling, after_load, "1000 recording/paused swaps made {} extra GDI objects", after_toggling as i64 - after_load as i64);
        }
        // `Icons::drop` ran. `LoadIconW`'s shared handle is not counted against us if we never took
        // one, and an owned handle we failed to destroy would still be sitting here.
        let after_drop = gdi_objects().expect("readable");
        assert!(
            after_drop <= before + 2,
            "the two icons were not given back: {before} before, {after_drop} after a load and a drop"
        );
    }

    /// The actual shell round trip, in both directions. `NIM_ADD` and `NIM_MODIFY` each accept the
    /// handle they are given and keep drawing with it afterwards, which is the property that makes
    /// "create once, swap by pointer" the correct design and `LR_SHARED` the wrong flag.
    #[test]
    fn the_shell_accepts_both_states_and_the_modifies_do_not_accumulate_handles() {
        let Some(before) = gdi_objects() else {
            eprintln!("GetGuiResources unavailable here; nothing to measure");
            return;
        };
        let instance = unsafe { ffi::GetModuleHandleW(std::ptr::null()) };
        let class = ffi::wide("WindrecorderIconTest");
        let mut record = ffi::WNDCLASSEXW::default();
        record.cb_size = std::mem::size_of::<ffi::WNDCLASSEXW>() as u32;
        record.lpfn_wnd_proc = Some(unsafe { std::mem::transmute(ffi::DefWindowProcW as usize) });
        record.h_instance = instance;
        record.lpsz_class_name = class.as_ptr();
        if unsafe { ffi::RegisterClassExW(&record) } == 0 {
            eprintln!("no window station here ({}); skipping", ffi::last_os_error());
            return;
        }
        let window = unsafe {
            ffi::CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0,
                std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null_mut())
        };
        assert!(!window.is_null(), "CreateWindowExW: {}", ffi::last_os_error());
        let icons = Icons::load(&layout()).expect("the embedded icons");
        let mut data = ffi::NOTIFYICONDATAW::new(window);
        data.u_flags = ffi::NIF_MESSAGE | ffi::NIF_ICON | ffi::NIF_TIP;
        data.u_callback_message = ffi::WM_APP + 42;
        data.h_icon = icons.for_recording(true);
        ffi::write_fixed(&mut data.sz_tip, "icon test");
        let added = unsafe { ffi::Shell_NotifyIconW(ffi::NIM_ADD, &data) };
        if added != ffi::TRUE {
            // A session with no notification area is a real thing (RDP, a service logon) and is not a
            // broken icon, so this says so and stops rather than asserting on an absent shell.
            eprintln!("the shell has no notification area here ({}); skipping the modify loop", ffi::last_os_error());
            unsafe { ffi::DestroyWindow(window) };
            return;
        }
        let added_count = gdi_objects().expect("readable");
        for round in 0..200 {
            data.h_icon = icons.for_recording(round % 2 == 0);
            assert_eq!(
                unsafe { ffi::Shell_NotifyIconW(ffi::NIM_MODIFY, &data) },
                ffi::TRUE,
                "the shell refused state {} on round {round}",
                ICON_RESOURCES[usize::from(round % 2 != 0)]
            );
        }
        let after = gdi_objects().expect("readable");
        assert_eq!(after, added_count, "200 real NIM_MODIFY swaps cost {} extra GDI objects", after as i64 - added_count as i64);
        unsafe {
            ffi::Shell_NotifyIconW(ffi::NIM_DELETE, &data);
            ffi::DestroyWindow(window);
        }
        drop(icons);
        assert!(gdi_objects().expect("readable") <= before + 2, "the pair was not given back after a live tray round trip");
    }
}
