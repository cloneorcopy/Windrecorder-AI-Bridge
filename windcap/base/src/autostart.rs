//! Running the tray when the user signs in.
//!
//! Upstream did this by dropping a shortcut into the user's Startup folder, through Python's
//! `WScript.Shell` — `utils.change_startup_shortcut` — and the delete half of that survived the rewrite
//! while the create half did not, because a `.lnk` needs COM and `IShellLinkW` and nobody wanted to write
//! that by hand. The registry does not: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` is one string
//! value, it is where every Windows app that starts on sign-in actually lives, it is per-user, and it needs
//! no administrator rights — which is also why it is called "start on sign-in" and not "start on boot".
//!
//! The target is the **tray**, never a window. The tray is what starts the recorder and the bridge and owns
//! the menu; a sign-in that opened a window would leave recording to whoever clicked next.
//!
//! The FFI is declared inline for the reason `clock.rs` and `ansi.rs` give: this crate must build with no
//! `windows` crate in the cargo cache, because the whole workspace's offline build is the gate.
//!

use std::path::{Path, PathBuf};

/// The value name under the Run key. `Windrecorder`, not `windsvc`: the registry editor is where a user
/// goes to check what this wrote, and the app's name is the thing they recognise there.
pub const VALUE_NAME: &str = "Windrecorder";

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// What this install's sign-in entry says, as far as this process can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// Nothing is registered, which is also what "we could not look" reads as to a user — so
    /// [`current`] keeps the two apart and [`apply`] reports what it did.
    None,
    /// Registered, with the exact string the registry holds.
    Registered(String),
    /// The registry could not be read. Stated, because a checkbox that silently disagrees with the
    /// registry is the dead-control bug this product has already fixed once.
    Unreadable(String),
}

/// The command Windows will run: the path, in quotes, because an install under
/// `C:\Program Files\…` or a name with a space otherwise starts `"C:\Program` and fails.
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// Which binary a sign-in should run, for the install at `root`.
///
/// `bin\windsvc.exe` is what a payload ships and what the release gate proves, so it comes first. A
/// checkout has no `bin\` until `build.ps1 -Stage` makes one, and there the running `windsvc.exe` under
/// `target\debug\` is the honest answer — but only when this process *is* the tray: called from a window,
/// `current_exe` would register `windui.exe` to start at sign-in, which is a different program with a
/// different life.
pub fn target_exe(root: &Path, current_exe: &Path) -> Result<PathBuf, String> {
    let staged = root.join("bin").join("windsvc.exe");
    if staged.is_file() {
        return Ok(staged);
    }
    if current_exe.file_name().map(|name| name.eq_ignore_ascii_case("windsvc.exe")).unwrap_or(false) {
        return Ok(current_exe.to_path_buf());
    }
    Err(format!(
        "no windsvc.exe to register in {}: run `windcap\\build.ps1 -Stage`, or start this from the tray",
        staged.display()
    ))
}

/// What is registered right now.
pub fn current() -> Entry {
    let key = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    let mut kind = 0u32;
    let mut size = 0u32;
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if code == ERROR_NOT_FOUND || code == ERROR_FILE_NOT_FOUND {
        return Entry::None;
    }
    if code != ERROR_SUCCESS {
        return Entry::Unreadable(format!("RegGetValue reported {code}"));
    }
    // Ask for the size, then read it. `size` is a byte count including the terminating NUL.
    let words = (size as usize / 2).max(1);
    let mut buffer = vec![0u16; words + 1];
    let mut read = (buffer.len() * 2) as u32;
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut read,
        )
    };
    if code != ERROR_SUCCESS {
        return Entry::Unreadable(format!("RegGetValue reported {code}"));
    }
    let taken = (read as usize / 2).min(buffer.len());
    let end = buffer[..taken].iter().position(|c| *c == 0).unwrap_or(taken);
    Entry::Registered(String::from_utf16_lossy(&buffer[..end]))
}

/// What a settings page has to say about the sign-in entry after a save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The registry already said what the user's box says. Nothing was written, and nothing needs a
    /// sentence — a Save that announced the state of a control the user did not touch is noise.
    Unchanged,
    /// Something moved, in words that name the command Windows will run.
    Changed(String),
    /// The registry refused, or could not be read. The setting in the file and the state of the machine
    /// now disagree, and the user has to be told which one they are holding.
    Failed(String),
}

/// Make the registry say what the user's setting says.
///
/// Idempotent by design: writing the same value again succeeds, and removing an absent value is reported
/// as already-off rather than as a failure. That is what lets a settings page call this on every Save
/// without having to know whether the box moved.
pub fn apply(root: &Path, want: bool) -> Outcome {
    let present = match current() {
        Entry::Registered(text) => Some(text),
        Entry::None => None,
        // An unreadable key is not an empty one. Refusing to write over a state this process cannot see
        // is the difference between a checkbox and a coin flip.
        Entry::Unreadable(why) => {
            return Outcome::Failed(format!("cannot read HKCU\\{RUN_KEY}\\{VALUE_NAME}: {why}, so nothing was changed"));
        }
    };
    if !want {
        return match present {
            None => Outcome::Unchanged,
            Some(_) => match remove() {
                Ok(()) => Outcome::Changed("start on sign-in: the entry was removed".to_string()),
                Err(code) => Outcome::Failed(format!("could not remove the sign-in entry (registry code {code})")),
            },
        };
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return Outcome::Failed(format!("cannot find this program to register it: {e}")),
    };
    let exe = match target_exe(root, &exe) {
        Ok(exe) => exe,
        Err(why) => return Outcome::Failed(why),
    };
    let command = command_for(&exe);
    if present.as_deref() == Some(command.as_str()) {
        return Outcome::Unchanged;
    }
    match write(&command) {
        Ok(()) => Outcome::Changed(format!("start on sign-in: Windows will run {command}")),
        Err(code) => Outcome::Failed(format!("could not write the sign-in entry for {command} (registry code {code})")),
    }
}

fn write(command: &str) -> Result<(), i32> {
    let key = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    let value = wide(command);
    let code = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    };
    if code == ERROR_SUCCESS {
        return Ok(());
    }
    Err(code)
}

fn remove() -> Result<(), i32> {
    let key = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    let code = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
    if code == ERROR_SUCCESS || code == ERROR_FILE_NOT_FOUND || code == ERROR_NOT_FOUND {
        return Ok(());
    }
    Err(code)
}

/// A NUL-terminated UTF-16 string, which is what every `…W` entry point wants.
fn wide(text: &str) -> Vec<u16> {
    let mut out: Vec<u16> = text.encode_utf16().collect();
    out.push(0);
    out
}

const HKEY_CURRENT_USER: *mut core::ffi::c_void = 0x8000_0001usize as *mut core::ffi::c_void;
const REG_SZ: u32 = 1;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_NOT_FOUND: i32 = 1168;

#[link(name = "advapi32")]
extern "system" {
    fn RegSetKeyValueW(
        key: *mut core::ffi::c_void,
        sub_key: *const u16,
        value_name: *const u16,
        value_type: u32,
        data: *const core::ffi::c_void,
        data_len: u32,
    ) -> i32;
    fn RegDeleteKeyValueW(key: *mut core::ffi::c_void, sub_key: *const u16, value_name: *const u16) -> i32;
    fn RegGetValueW(
        key: *mut core::ffi::c_void,
        sub_key: *const u16,
        value: *const u16,
        flags: u32,
        value_type: *mut u32,
        data: *mut core::ffi::c_void,
        data_len: *mut u32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The quoting is not cosmetic. Windows runs the Run value through the same command line a shell
    /// would, so an unquoted path with a space starts a program named `C:\Program` and reports nothing.
    #[test]
    fn the_registered_command_quotes_the_path_it_runs() {
        assert_eq!(
            command_for(Path::new(r"C:\Program Files\Windrecorder\bin\windsvc.exe")),
            r#""C:\Program Files\Windrecorder\bin\windsvc.exe""#
        );
    }

    /// The staged payload is the answer wherever it exists, because that is the folder the release gate
    /// proved and the one a user's antivirus is least likely to have quarantined.
    #[test]
    fn a_staged_payload_registers_its_own_tray_not_the_window_that_asked() {
        let dir = std::env::temp_dir().join(format!("windcap-autostart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/windsvc.exe"), b"MZ").unwrap();
        let window = dir.join("bin/winduiweb.exe");
        assert_eq!(target_exe(&dir, &window).expect("the payload has a tray"), dir.join("bin").join("windsvc.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A checkout has no `bin\`. There the tray may register itself — and a window must not register the
    /// window, which is how "start on sign-in" would end up opening a window instead of recording.
    #[test]
    fn a_checkout_falls_back_to_the_running_tray_and_refuses_a_window() {
        let root = Path::new("Z:/definitely-not-here/windrecorder");
        let tray = PathBuf::from("E:/repo/windcap/target/debug/windsvc.exe");
        assert_eq!(target_exe(root, &tray).expect("this process is the tray"), tray);

        let window = PathBuf::from("E:/repo/windcap/target/debug/winduiweb.exe");
        let refused = target_exe(root, &window).expect_err("a window is not a sign-in target");
        assert!(refused.contains("windsvc.exe"), "{refused}");
        assert!(refused.contains("build.ps1"), "and it names the command that fixes it: {refused}");
    }

    /// Reading the real key of the user running the tests is harmless and proves the FFI is wired to
    /// something: whatever the answer, it is one of the three states and never a panic. An install that
    /// registered itself is reported as registered, with a path in it.
    #[test]
    fn the_current_entry_is_read_without_harming_it() {
        match current() {
            Entry::None => {}
            Entry::Registered(text) => assert!(text.contains("windsvc"), "{text} is registered under our name and is not ours"),
            Entry::Unreadable(why) => panic!("HKCU\\{RUN_KEY} could not be read: {why}"),
        }
    }

    /// The value name is the whole identity of the entry, so it must not drift between what is written and
    /// what is looked for.
    #[test]
    fn one_value_name_names_the_entry_everywhere() {
        assert_eq!(VALUE_NAME, "Windrecorder");
        assert!(RUN_KEY.ends_with(r"\CurrentVersion\Run"));
    }
}
