//! Path rules, isolated from everything else so they can be attacked in a test on their own.
//!
//! Every name this crate acts on arrives from one of two untrustworthy places: a `read_dir` listing of
//! a folder the user has been writing to for years, or a SQLite file the user can open in a text
//! editor. Both produce strings that get concatenated onto a root and then *written to*. The whole
//! difference between "migrated the user's September index" and "overwrote their Documents folder" is
//! whether that concatenation was checked, so nothing below is allowed to be a `starts_with` on a
//! string.
//!
//! `windcap/maint/src/layout.rs` already carries an equivalent `inside()`. It is private to that
//! binary and cannot be reused from here, so it is reproduced rather than worked around — see the
//! gap note in the crate README section at the bottom of `lib.rs`.

use std::path::{Component, Path, PathBuf};

/// Maximum characters accepted in one path component.
///
/// Windows caps a full path at 260 bytes without the long-path opt-in. A name close to that limit is
/// legal on its own and still fails the moment `_BACKUP_2026-09-23_01-02-03.db` is appended to it, so
/// the check exists to turn "os error 206: The filename or extension is too long" mid-migration into a
/// line in the plan that says this file will not be touched.
const MAX_COMPONENT_LEN: usize = 200;

/// Total path length the install root is allowed to consume before a component is rejected.
///
/// Not enforced precisely — an absolute path's length depends on the root, which the caller knows and
/// this module does not. It is a coarse early rejection for names that are obviously pathological.
const MAX_PATH_LEN: usize = 240;

/// The Windows names that address a device rather than a file.
///
/// A file called `CON` is creatable on some configurations and unreadable through a normal open, and
/// `nul` as a rename target silently discards bytes. Upstream never produced either, so refusing them
/// costs nothing and closes a class of "the write succeeded and the data is gone".
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The characters a Windows file name may not contain, plus the ones that would survive into a
/// shell command line.
const FORBIDDEN_CHARS: [char; 11] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\n', '\r'];

/// Why a name found on disk cannot be used as a single path component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Empty,
    /// `.` or `..`, in either spelling and with any trailing dots or spaces Windows folds away.
    Traversal,
    ReservedDevice(String),
    ForbiddenChar(char),
    ControlChar,
    TrailingDotOrSpace,
    TooLong(usize),
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameError::Empty => write!(f, "name is empty"),
            NameError::Traversal => write!(f, "name is a directory hop (\".\" or \"..\")"),
            NameError::ReservedDevice(n) => write!(f, "\"{n}\" addresses a Windows device, not a file"),
            NameError::ForbiddenChar(c) => write!(f, "name contains {c:?}, which cannot be a path component"),
            NameError::ControlChar => write!(f, "name contains a control character"),
            NameError::TrailingDotOrSpace => write!(f, "name ends in a dot or space, which Windows strips"),
            NameError::TooLong(len) => write!(f, "name is {len} characters, over the {MAX_COMPONENT_LEN} limit"),
        }
    }
}

/// Can `name` be joined onto a directory and stay inside it?
///
/// This is the guard for a *single* component: a month file's user name, or a video file's name. It is
/// deliberately stricter than "does it contain a slash", because `.._2026-09_wind.db` is a perfectly
/// legal file name that `wind_store`'s month parser will happily read as `user = ".."`, and the next
/// `paths::month_filename(user, ..)` produces `userdata/db/../../../x_wind.db`.
pub fn check_component(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_COMPONENT_LEN {
        return Err(NameError::TooLong(name.len()));
    }
    for c in name.chars() {
        if c.is_control() {
            return Err(NameError::ControlChar);
        }
        if FORBIDDEN_CHARS.contains(&c) {
            return Err(NameError::ForbiddenChar(c));
        }
    }
    // Windows ignores trailing dots and spaces when resolving a name, so `..` and `..   ` and `..`
    // with a trailing dot are all the same hop. Fold before comparing.
    let folded = name.trim_end_matches(['.', ' ']);
    if folded.is_empty() || folded == "." || folded == ".." {
        return Err(NameError::Traversal);
    }
    // The device check ignores case *and* extension: `con.txt` is `CON` to the Win32 namespace.
    let stem = folded.split('.').next().unwrap_or(folded).to_ascii_uppercase();
    if RESERVED_NAMES.contains(&stem.as_str()) {
        return Err(NameError::ReservedDevice(folded.to_string()));
    }
    Ok(())
}

/// Is `target` strictly inside `dir`, by name alone?
///
/// `canonicalize` is not an option, for the same reason `windcap/maint` rejects it: the paths guarded
/// here frequently do not exist yet (a backup destination), and following reparse points would let a
/// junction the user made inside `userdata/` point a write at anywhere on the machine. A lexical walk
/// that folds `.` and `..` is total, and the compare is case-insensitive because NTFS is.
pub fn inside(dir: &Path, target: &Path) -> bool {
    let base = normalize(dir);
    let goal = normalize(target);
    if base.is_empty() || goal.len() <= base.len() {
        return false;
    }
    base.iter().zip(goal.iter()).all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// The components of a path with `.` dropped and `..` folded against what precedes it.
///
/// A `..` with nothing to pop is *kept*, so `userdata/../../../Windows` stays visibly escaping instead
/// of collapsing into something that looks like a relative path inside `userdata/`.
fn normalize(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            // Prefix and root are kept as ordinary components: dropping them would make
            // `C:/install/userdata/x` and the relative `install/userdata/x` compare equal.
            Component::Prefix(prefix) => out.push(prefix.as_os_str().to_string_lossy().into_owned()),
            Component::RootDir => out.push("/".to_string()),
            Component::ParentDir => {
                if out.last().is_some_and(|last| last == "..") || out.is_empty() {
                    out.push("..".to_string());
                } else {
                    out.pop();
                }
            }
            Component::Normal(name) => out.push(name.to_string_lossy().into_owned()),
        }
    }
    out
}

/// Reject anything that is not a plain descendant of `root`.
///
/// Used on paths assembled from a directory listing, where "the listing said it was in `userdata/db`"
/// is the only evidence available. The error text quotes the path because a migration log that says
/// "refused a bad path" without naming it cannot be acted on.
pub fn confine(root: &Path, target: &Path) -> Result<(), String> {
    if normalize(target).len() > MAX_PATH_LEN {
        return Err(format!("{}: path is too long to be safe to write", target.display()));
    }
    if inside(root, target) || root == target {
        Ok(())
    } else {
        Err(format!(
            "{} resolves outside the install root {} — refusing",
            target.display(),
            root.display()
        ))
    }
}

/// A path read from disk must be confined *before* it is opened, and the check has to survive a
/// junction: `normalize` is lexical, so `userdata/link/x.db` where `link` is a junction to `C:\Windows`
/// passes it. `no_reparse_points` catches that by asking the filesystem about each existing component.
///
/// Only components that exist are inspected — a destination that does not exist yet is exactly the
/// common case for a backup.
pub fn no_reparse_points(path: &Path) -> Result<(), String> {
    // Walk from the deepest existing ancestor upwards, collecting first and checking after, because
    // `parent()` borrows the buffer it returns and reassigning a path mid-walk cannot borrow from itself.
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut walked = path.parent().unwrap_or(path).to_path_buf();
    while walked.exists() {
        let is_root = walked.parent().is_none();
        chain.push(walked.clone());
        if is_root {
            break;
        }
        walked = walked.parent().unwrap_or(Path::new("")).to_path_buf();
        if walked.as_os_str().is_empty() {
            break;
        }
    }
    for ancestor in chain {
        let metadata = std::fs::symlink_metadata(&ancestor)
            // `Err` here is a race with the entry disappearing, which the caller's own open will hit
            // anyway; reporting it as a security refusal would send a user chasing a phantom.
            .map_err(|e| format!("{}: cannot be inspected ({e}) — refusing", ancestor.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("{} is a symlink or junction — refusing to write through it", ancestor.display()));
        }
    }
    Ok(())
}

/// Make `path` absolute, lexically, without asking the filesystem anything.
///
/// Needed because a child process resolves a relative argument against *its own* working directory, and
/// the whole point of running the OCR tool with the install root as its cwd is that Python did. Handing
/// it `..\..\__assets__ixture.png` and a different cwd produces "the system cannot find the file
/// specified" for a file that is plainly present, which the report then renders as a broken engine.
///
/// `std::fs::canonicalize` is not usable: it requires the target to exist, and it returns
/// `\?\`-prefixed verbatim paths on Windows, which an arbitrary third-party command line may or may not
/// accept. Folding `.` and `..` against the components we already have is total, deterministic, and needs
/// no reparse-point traversal — the same trade [`inside`] makes for the same reason.
pub fn anchor(cwd: &Path, path: &Path) -> PathBuf {
    // Join first, fold second. Normalising the two halves separately would leave a leading `..` with
    // nothing to pop against and reassemble to a path that is absolute but still says `..`, which the
    // child process then resolves against whichever cwd the operating system gave it — the exact
    // ambiguity this function exists to remove.
    let joined = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    reassemble(normalize(&joined))
}

fn reassemble(components: Vec<String>) -> PathBuf {
    let mut out = PathBuf::new();
    for component in components {
        out.push(component);
    }
    out
}

/// Both checks in one call, for the "found this on disk, about to write to it" case.
pub fn safe_write_target(root: &Path, target: &Path) -> Result<(), String> {
    confine(root, target)?;
    no_reparse_points(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ordinary_names_pass() {
        for name in ["default", "2024_snow_white", "amy", "2026-09-21_21-16-12.mp4", "d.walters"] {
            assert_eq!(check_component(name), Ok(()), "{name}");
        }
    }

    /// The attack the brief asks about. `.._2026-09_wind.db` is a valid file name on NTFS and parses as
    /// a month file belonging to a user called `..`; `paths::month_filename` would then walk the write
    /// three directories up.
    #[test]
    fn a_month_file_cannot_disguise_its_owner_as_a_directory_hop() {
            for name in ["..", ".", "...", ".. ", ". "] {
            assert_eq!(check_component(name), Err(NameError::Traversal), "{name:?}");
        }
    }

    #[test]
    fn separators_are_rejected_in_every_form() {
        assert_eq!(check_component("..\\evil"), Err(NameError::ForbiddenChar('\\')));
        assert_eq!(check_component("../evil"), Err(NameError::ForbiddenChar('/')));
        assert_eq!(check_component("C:/x"), Err(NameError::ForbiddenChar(':')));
        assert_eq!(check_component("a\u{0}b"), Err(NameError::ControlChar));
        assert_eq!(check_component("nul"), Err(NameError::ReservedDevice("nul".into())));
        assert_eq!(check_component("COM1.mp4"), Err(NameError::ReservedDevice("COM1.mp4".into())));
        assert_eq!(check_component("&"), Ok(()), "a shell metacharacter is a legal file name");
    }

    #[test]
    fn an_overlong_name_fails_here_rather_than_as_error_206_mid_migration() {
        assert_eq!(check_component(&"a".repeat(201)), Err(NameError::TooLong(201)));
        assert_eq!(check_component(&"a".repeat(200)), Ok(()));
    }

    #[test]
    fn inside_accepts_only_descendants() {
        let dir = Path::new("E:/install/userdata");
        assert!(inside(dir, Path::new("E:/install/userdata/db")));
        assert!(inside(dir, Path::new("E:/install/userdata/./db/../db/x.db")));
        assert!(!inside(dir, Path::new("E:/install/userdata")), "a directory is not inside itself");
        assert!(!inside(dir, Path::new("E:/install/cache/x")));
        // NTFS folds case, so the guard has to as well or it can be dodged by spelling.
        assert!(inside(dir, Path::new("e:/INSTALL/UserData/db")));
    }

    #[test]
    fn inside_rejects_escapes_and_absolute_paths() {
        let dir = Path::new("E:/install/userdata");
        assert!(!inside(dir, Path::new("E:/install/userdata/../../Documents")));
        assert!(!inside(dir, Path::new("E:/install/userdata/..")));
        assert!(!inside(dir, Path::new("C:/Users/me/Documents")));
        assert!(!inside(dir, Path::new("/etc/passwd")));
        assert!(!inside(Path::new("userdata"), Path::new("../userdata/x")));
        assert!(inside(Path::new("userdata"), Path::new("userdata/x")));
        // An empty base must not swallow the disk.
        assert!(!inside(Path::new(""), Path::new("anything")));
    }

    #[test]
    fn confine_allows_the_root_itself_because_migrate_renames_directories_inside_it() {
        let root = PathBuf::from("E:/install");
        assert!(confine(&root, &root.join("userdata/db/x.db")).is_ok());
        assert!(confine(&root, &root).is_ok());
        assert!(confine(&root, Path::new("E:/install2/x")).is_err());
        assert!(confine(&root, Path::new("E:/x/y")).is_err());
    }

    /// A refusal has to name the path: a user told "one month file was skipped" cannot act on it.
    #[test]
    fn confine_quotes_the_path_it_refused() {
        let err = confine(Path::new("E:/install"), Path::new("E:/install/db/../../Windows/x.db")).unwrap_err();
        assert!(err.contains("Windows"), "{err}");
        assert!(err.contains("install root"), "{err}");
    }

    #[test]
    fn a_relative_root_becomes_absolute_without_asking_the_filesystem() {
        let cwd = Path::new("C:/install/windcap/setup");
        assert_eq!(anchor(cwd, Path::new("../..")), PathBuf::from("C:/install"));
        assert_eq!(anchor(cwd, Path::new("./userdata/db")), PathBuf::from("C:/install/windcap/setup/userdata/db"));
        assert_eq!(anchor(cwd, Path::new("fixtures/x.png")), PathBuf::from("C:/install/windcap/setup/fixtures/x.png"));
        // Already absolute stays put, and a `..` that escapes the drive root stays visible rather than
        // silently resolving to something inside it.
        assert_eq!(anchor(cwd, Path::new("D:/other/y")), PathBuf::from("D:/other/y"));
        assert_eq!(anchor(cwd, Path::new("../../..")), PathBuf::from("C:/"));
        assert_eq!(anchor(Path::new("relative/cwd"), Path::new("x")), PathBuf::from("relative/cwd/x"));
    }

    #[test]
    fn an_anchored_path_still_passes_the_containment_check() {
        // The two rules have to compose: anchoring must not turn an escape into a legal path.
        let root = Path::new("C:/install");
        let escaped = anchor(root, Path::new("../../windows/system32/x.db"));
        assert!(!inside(root, &escaped), "{escaped:?} escaped the containment check");
        let fine = anchor(root, Path::new("./userdata/db/x.db"));
        assert!(inside(root, &fine), "{fine:?} was rejected");
    }

    #[test]
    fn a_normal_temporary_directory_has_no_reparse_points() {
        let dir = std::env::temp_dir().join(format!("wind-setup-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(no_reparse_points(&dir.join("db/x.db")).is_ok(), "a destination that does not exist yet is fine");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
