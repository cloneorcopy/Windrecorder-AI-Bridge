//! Path and lock rules that the maintenance pass must never get wrong.
//!
//! Every name this binary acts on can come out of a database the user has been writing to for
//! years, and every destructive step is one `..` away from the wrong folder. The helpers here are
//! therefore deliberately independent of the database and of ffmpeg: they are the parts that can be
//! reasoned about, and tested, on their own.

use std::path::{Component, Path, PathBuf};


use crate::encode::EncoderAvailability;

// The stamp rules are `wind_base::paths`', re-exported under the names this crate uses. They moved
// because a reader of the index has to answer "is this prefix really a recording?" identically to the
// pass that deletes recordings, and two copies is how one of them starts accepting a name the other
// rejects.
pub use wind_base::paths::{same_segment, segment_stamp_of};

/// Is `target` strictly inside `dir`, by name alone?
///
/// `canonicalize` is not an option: the paths this guards are frequently ones that do not exist
/// yet, and following reparse points would let a junction inside `cache_screenshot/` point the
/// deletion at the user's Documents folder. A lexical walk that folds `.` and `..` is both total
/// and strict enough, and the comparison is case-insensitive because NTFS is.
///
/// A directory is not inside itself, and a base that names no component contains nothing at all, so
/// a misconfigured empty directory cannot swallow the disk.
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
/// A `..` that has nothing to pop is kept rather than discarded, so `cache/../../Windows` stays
/// visibly escaping and can never compare equal to a path under `cache/`.
fn normalize(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            // The prefix and the root are kept as ordinary components: dropping them would make
            // `C:/cache/x` and the relative `cache/x` look interchangeable to the comparison above.
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

/// Where a discarded file lands when `recycle_deleted_files` is on.
///
/// Keyed by the run's stamp plus the path relative to the install root, so a trashed
/// `userdata/videos/2026-09/x.mp4` can be put back where it came from instead of arriving as a
/// bare `x.mp4` that has lost the month folder it belonged to.
pub fn trash_destination(trash_dir: &Path, root: &Path, target: &Path, stamp: &str) -> PathBuf {
    let relative = target.strip_prefix(root).unwrap_or_else(|_| {
        Path::new(target.file_name().unwrap_or_else(|| std::ffi::OsStr::new("item")))
    });
    trash_dir.join(stamp).join(relative)
}

/// Remove one file or directory, or move it into `trash_dir` first.
///
/// `recycle = false` on a directory is the one call site allowed to reach `remove_dir_all`, so the
/// caller must have proven the path [`inside`] the screenshot cache; this function will not repeat
/// that check and does not accept a list.
pub fn discard(target: &Path, trash_dir: &Path, root: &Path, stamp: &str, recycle: bool) -> Result<Option<PathBuf>, String> {
    if !recycle {
        let is_dir = std::fs::metadata(target).map(|m| m.is_dir()).unwrap_or(false);
        return remove(target, is_dir).map(|()| None);
    }
    let destination = trash_destination(trash_dir, root, target, stamp);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::rename(target, &destination)
        .map(|()| Some(destination))
        .map_err(|e| format!("{}: {e}", target.display()))
}

fn remove(target: &Path, is_dir: bool) -> Result<(), String> {
    let result = if is_dir { std::fs::remove_dir_all(target) } else { std::fs::remove_file(target) };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", target.display())),
    }
}

/// The exclusive maintenance lock.
///
/// `config.maintain_lock_dir()` names a *directory* upstream (`cache\locks\LOCK_MAINTAIN`), which
/// `windrecorder.lock.FileLock` uses as a container for one lock file per video being indexed.
/// `wind_base::fslock::PidLock` cannot model that: given a directory path its `create_new` file open
/// fails, and the state read then reports `Unreadable`, i.e. it refuses forever. So the claim here
/// is `create_dir` on the lock path itself (atomic on NTFS, and the removal half is `remove_dir`,
/// never `remove_dir_all`, so a foreign `.md` left inside survives us), with our pid in a `PID`
/// child so a second pass can tell a live owner from the empty directory a Python run leaves behind.
#[derive(Debug)]
pub struct MaintainLock {
    dir: PathBuf,
    pid_file: PathBuf,
    /// Only a directory we created is ours to delete: an existing one may be Python's container.
    created_dir: bool,
}

impl MaintainLock {
    /// Claim the lock, reclaiming one whose owner process is gone. `Err` carries the reason.
    pub fn acquire(dir: &Path) -> Result<MaintainLock, String> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let created_dir = match std::fs::create_dir(dir) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        };
        let lock = MaintainLock { dir: dir.to_path_buf(), pid_file: dir.join("PID"), created_dir };
        lock.write_pid()?;
        Ok(lock)
    }

    fn write_pid(&self) -> Result<(), String> {
        match self.try_write_pid() {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(format!("{}: {e}", self.pid_file.display()));
            }
            Err(_) => {}
        }
        // Somebody else is (or was) inside. Their word, not ours, decides what happens next.
        let body = std::fs::read_to_string(&self.pid_file).unwrap_or_default();
        match body.trim().parse::<u32>() {
            Ok(pid) if pid == std::process::id() => Ok(()),
            Ok(pid) if wind_base::fslock::is_process_running(pid) => {
                Err(format!("{} is held by running process {pid}", self.dir.display()))
            }
            Ok(dead) => {
                std::fs::remove_file(&self.pid_file)
                    .map_err(|e| format!("{}: {e}", self.pid_file.display()))?;
                self.try_write_pid().map_err(|e| {
                    format!("could not reclaim stale lock in {} (dead pid {dead}): {e}", self.dir.display())
                })
            }
            // A `PID` file we cannot read is somebody else's protocol; taking it away would be a
            // guess about what it means.
            Err(_) => Err(format!("{} exists but names no pid; refusing to take it", self.pid_file.display())),
        }
    }

    fn try_write_pid(&self) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&self.pid_file)?;
        write!(file, "{}", std::process::id()).and_then(|_| file.flush())
    }
}

impl Drop for MaintainLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_file);
        // Fails, correctly, when another tool's lock file is still inside: an empty result is not
        // checked because there is nothing left to do about it either way.
        if self.created_dir {
            let _ = std::fs::remove_dir(&self.dir);
        }
    }
}

/// Why an encoder run said no.
#[derive(Debug)]
pub enum Called {
    /// 停止整理 was pressed while it ran, and the child was put down. This is not the encoder's failure
    /// and not the file's, and a caller that counts it as one more broken segment would be reporting a
    /// machine's own decision as its own mistake.
    Off,
    /// ffmpeg would not start, or came back with a bad status, with ffmpeg's own tail in the sentence.
    Failed(String),
}

/// Run ffmpeg, and put it down inside a second if the pass has been called off.
///
/// The wait used to be `.output()`, which cannot be interrupted: one long segment is a minute of encoder,
/// and a stop pressed during it was answered when that minute ended — which is what 立刻停止 actually means
/// to the person who pressed it. [`crate::schedule::Child`] is this pass's own shape for "spawn, read both
/// pipes, ask the stop question once a second, kill", so the encoder is handed to it rather than to a
/// second implementation that could drift from it.
///
/// Both callers remove their half-written output on any error, which is what makes the kill safe to do at
/// all: ffmpeg is told to write the video's *final* name, so a truncated file left there would be read by
/// the next pass as somebody else's finished hour.
///
/// ffmpeg writes progress and errors to stderr in the console code page, so its words go through the same
/// evidence-based decode the OCR output uses; `String::from_utf8_lossy` would turn a Chinese install's
/// error message into replacement characters and the report would be useless.
pub fn run_ffmpeg(ffmpeg: &Path, args: &[String], may_work: &dyn Fn() -> bool) -> Result<(), Called> {
    let mut command = std::process::Command::new(ffmpeg);
    command.args(args);
    let mut child = crate::schedule::Child::start("ffmpeg", &mut command).map_err(Called::Failed)?;
    // Read and throw away: ffmpeg talks on stderr, and a stdout pipe nobody drains is how an encoder that
    // is still alive ends up looking like an encode that never ends.
    if child.pump_with(may_work, |_| {}) {
        return Err(Called::Off);
    }
    match child.finish().map_err(Called::Failed)? {
        Some(exit) if exit.ok => Ok(()),
        Some(exit) => Err(Called::Failed(format!(
            "ffmpeg exited with {}: {tail}",
            ffmpeg.display(),
            tail = tail_lines(&wind_base::ansi::decode_console_bytes(exit.stderr.as_bytes()), 6)
        ))),
        // Only `Child::finish` can say this, and only for a child that was already put down.
        None => Err(Called::Off),
    }
}

/// ffmpeg's first `-version` line, or `None` when it cannot be executed at all.
pub fn ffmpeg_version(ffmpeg: &Path) -> Option<String> {
    let output = std::process::Command::new(ffmpeg)
        .arg("-version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    wind_base::ansi::decode_console_bytes(&output.stdout).lines().next().map(str::to_string)
}

/// Can this ffmpeg actually open `encoder`? Ask it, once, on a frame nobody will ever see.
///
/// The answer is [`crate::encode::EncoderAvailability`], the type the resolvers in that module
/// decide against; this function is the only thing in the crate that reaches a process to get it.
///
/// A preset name being *in* `config_src/record_preset.json` says the operator wrote it down; it does
/// not say the machine can encode with it. `hevc_amf` is a valid name in a stock ffmpeg and still
/// fails on a machine with no AMD encoder, and the failure ffmpeg reports is a non-zero exit that
/// leaves the slice unconverted — so an unavailable hardware encoder would cost the user their
/// footage rather than merely their file size. Whether a codec was compiled in is not the same
/// question as whether it initialises, and `ffmpeg -encoders` answers only the first, which is why
/// this runs the encoder instead of reading a list: measured on this workspace's own ffmpeg,
/// `hevc_amf` is listed by `-encoders` on a machine where every `hevc_amf` encode fails with
/// `Could not open encoder before EOF`.
///
/// The trial is a `yuv420p` frame through `-c:v <encoder>`, which is the shape both real encodes
/// ask for, so the answer is about the thing that actually varies between machines. It is a probe of
/// *initialisation*, not of every flag the real command line adds — an encoder that opens here can
/// still reject a later option such as `-preset medium` — and the pass reports that failure in
/// ffmpeg's own words when it happens. One frame into the null muxer, so it costs a process spawn and
/// nothing else, and it writes no file.
pub fn probe_encoder(ffmpeg: &Path, encoder: &str) -> EncoderAvailability {
    // 256x256 and not 64x64: NVENC refuses to *initialise* below its own minimum frame size, so a
    // probe small enough to be cheap would report a working `hevc_nvenc` as unusable and quietly
    // downgrade every encode on an nVIDIA machine. Measured on this workspace's ffmpeg — a 128x128
    // trial fails hevc_nvenc with "Frame dimensions are less than the minimum supported value", a
    // 256x256 one succeeds. Still one frame of a quarter-megapixel, so the trial stays near free.
    let source = "color=c=black:s=256x256:r=1";
    let args = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        source,
        "-frames:v",
        "1",
        "-c:v",
        encoder,
        "-pix_fmt",
        "yuv420p",
        "-f",
        "null",
        "-",
    ];
    let output = match std::process::Command::new(ffmpeg)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) => output,
        // Nothing was learned about the encoder, which is exactly what `Unknown` means: the real
        // encode will report `cannot start ffmpeg: <reason>` and that message is the useful one.
        Err(_) => return EncoderAvailability::Unknown,
    };
    if output.status.success() {
        return EncoderAvailability::Usable;
    }
    EncoderAvailability::Unusable(probe_reason(&wind_base::ansi::decode_console_bytes(&output.stderr), encoder, output.status.code()))
}

/// The one line of ffmpeg's complaint worth putting in front of a user.
///
/// A failed encoder init prints a pile: the muxer's "Nothing was written into output file", a progress
/// line, "Conversion failed!", and then the lines that carry the encoder's own name and the actual
/// reason (`Could not open encoder before EOF`, `Unknown encoder`, `Cannot initialize queues`). Those
/// are the ones that answer the question the note is being written to answer, so they are picked out
/// by name first and the generic tail is only the fallback for a build that words its errors
/// differently — and for the generic tail, the *last* lines, which is where ffmpeg puts the cause.
fn probe_reason(stderr: &str, encoder: &str, code: Option<i32>) -> String {
    let named: Vec<String> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && line.contains(encoder))
        .map(str::to_string)
        .collect();
    if !named.is_empty() {
        // Oldest first, because ffmpeg states the cause and then the consequences of it.
        return named.join(" | ");
    }
    let tail = tail_lines(stderr, 4);
    if tail.is_empty() {
        format!("ffmpeg exited with status {}", code.unwrap_or(-1))
    } else {
        tail
    }
}

/// The last `n` non-empty lines, newest first, joined for a single-line error message.
pub fn tail_lines(text: &str, n: usize) -> String {
    let kept: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).rev().take(n).collect();
    kept.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffmpeg's complaint on a failed hardware init is a stack of consequences, and only the lines
    /// carrying the encoder's own name state the cause. Picked from a captured real AMF failure:
    /// the note a user reads has to say `amfrt64.dll failed to open`, not `Nothing was written into
    /// output file`, which is what a plain tail of the output would have surfaced.
    #[test]
    fn a_probe_reason_names_the_encoder_that_refused_to_open() {
        let amf = "Conversion failed!\n\
             frame=    0 fps=0.0 q=0.0 Lsize=       0KiB time=N/A bitrate=N/A speed=N/A\n\
             [out#0/null @ 000001d8] Nothing was written into output file, because at least one of its streams received no packets.\n\
             [hevc_amf @ 000001d82867ff40] DLL amfrt64.dll failed to open\n\
             [vost#0:0/hevc_amf @ 000001d82867f780] [enc:hevc_amf @ 000001d82866d240] Error while opening encoder\n";
        assert_eq!(probe_reason(amf, "hevc_amf", Some(1)),
            "[hevc_amf @ 000001d82867ff40] DLL amfrt64.dll failed to open | [vost#0:0/hevc_amf @ 000001d82867f780] [enc:hevc_amf @ 000001d82866d240] Error while opening encoder");

        // A build that words its errors differently still gets a reason rather than an empty string,
        // and an unknown encoder — which names nothing in its own output — falls to the tail.
        let plain = "some other failure\nsecond line\n";
        assert!(probe_reason(plain, "hevc_amf", Some(1)).contains("some other failure"), "{}", probe_reason(plain, "hevc_amf", Some(1)));
        assert_eq!(probe_reason("", "hevc_amf", Some(3)), "ffmpeg exited with status 3");
    }

    #[test]
    fn a_segment_is_recognised_by_its_stamp_not_its_whole_name() {
        assert!(same_segment("2026-09-21_21-16-12.mp4", "2026-09-21_21-16-12-OCRED-COMPRESS.mp4"));
        assert!(!same_segment("2026-09-21_21-16-12.mp4", "2026-09-21_21-16-13.mp4"));
        // Two names too short to carry a stamp must not match each other.
        assert!(!same_segment("a", "a"));
        assert!(!same_segment("", ""));
    }

    /// The prefix has to be a date the calendar accepts: `notes.txt` and a row whose name was
    /// truncated share a first character with a stamp and must not be called a segment.
    #[test]
    fn only_a_real_date_starts_a_segment() {
        assert_eq!(segment_stamp_of("2026-09-21_21-16-12.mp4").as_deref(), Some("2026-09-21_21-16-12"));
        assert_eq!(segment_stamp_of("2026-09-21_21-16-12-OCRED.mp4").as_deref(), Some("2026-09-21_21-16-12"));
        assert_eq!(segment_stamp_of("2026-13-45_99-99-99.mp4"), None);
        assert_eq!(segment_stamp_of("notes.txt"), None);
        assert_eq!(segment_stamp_of("2026-09-21"), None);
    }

    #[test]
    fn inside_accepts_only_descendants() {
        let dir = Path::new("E:/install/cache_screenshot");
        assert!(inside(dir, Path::new("E:/install/cache_screenshot/2026-09-21_21-16-12")));
        assert!(inside(dir, Path::new("E:/install/cache_screenshot/2026-09-21_21-16-12/x.jpg")));
        assert!(inside(dir, Path::new("E:/install/cache_screenshot/./a/../b")), "a folded path still lives inside");
        assert!(!inside(dir, Path::new("E:/install/cache_screenshot")));
        assert!(!inside(dir, Path::new("E:/install/videos/x.mp4")));
    }

    /// The attack the retention sweep makes possible: a row whose `picturefile_name` was edited to
    /// escape the cache. Case is folded because NTFS paths are, so the check cannot be dodged by
    /// spelling.
    #[test]
    fn inside_rejects_escapes_and_absolute_paths() {
        let dir = Path::new("E:/install/cache_screenshot");
        assert!(!inside(dir, Path::new("E:/install/cache_screenshot/../../Documents/")));
        assert!(!inside(dir, Path::new("E:/install/cache_screenshot/..")));
        assert!(!inside(dir, Path::new("C:/Users/me/Documents")));
        assert!(!inside(dir, Path::new("/etc/passwd")));
        assert!(!inside(Path::new("cache"), Path::new("../cache/x")));
        assert!(inside(Path::new("cache"), Path::new("cache/x")));
        assert!(inside(
            Path::new("E:/Install/CACHE_SCREENSHOT"),
            Path::new("e:/install/cache_screenshot/2026-09-21_21-16-12")
        ));
    }

    #[test]
    fn a_trashed_file_keeps_the_route_it_came_from() {
        let root = Path::new("E:/install");
        let dest = trash_destination(
            &root.join("userdata/trash"),
            root,
            &root.join("userdata/videos/2026-09-21_21-16-12.mp4"),
            "2026-09-22_03-00-00",
        );
        assert_eq!(dest, Path::new("E:/install/userdata/trash/2026-09-22_03-00-00/userdata/videos/2026-09-21_21-16-12.mp4"));

        // A path from outside the install still lands in the trash rather than vanishing.
        let outside = trash_destination(&root.join("userdata/trash"), root, Path::new("D:/other/x.mp4"), "s");
        assert_eq!(outside, Path::new("E:/install/userdata/trash/s/x.mp4"));
    }

    #[test]
    fn discarding_a_file_moves_it_or_deletes_it_as_configured() {
        let dir = temp_tree("discard");
        let file = dir.join("userdata/videos/x.mp4");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"video").unwrap();

        let moved = discard(&file, &dir.join("userdata/trash"), &dir, "s", true).unwrap().unwrap();
        assert!(!file.exists());
        assert_eq!(std::fs::read(&moved).unwrap(), b"video");

        std::fs::write(&file, b"video").unwrap();
        assert!(discard(&file, &dir.join("userdata/trash"), &dir, "s", false).unwrap().is_none());
        assert!(!file.exists());

        // A missing target is not a failure: the sweep may run twice over the same plan.
        assert!(discard(&file, &dir.join("userdata/trash"), &dir, "s", false).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discarding_a_directory_takes_its_contents() {
        let dir = temp_tree("discard-dir");
        let slice = dir.join("cache_screenshot/2026-09-22_03-00-00");
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("f.jpg"), b"j").unwrap();
        discard(&slice, &dir.join("trash"), &dir, "s", true).unwrap();
        assert!(!slice.exists());
        assert!(dir.join("trash/s/cache_screenshot/2026-09-22_03-00-00/f.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lock_is_claimed_held_and_released() {
        let dir = temp_tree("lock");
        let lock_path = dir.join("cache/locks/LOCK_MAINTAIN");
        {
            let lock = MaintainLock::acquire(&lock_path).expect("free lock");
            assert!(lock_path.is_dir(), "the lock is the directory itself");
            assert_eq!(std::fs::read_to_string(&lock.pid_file).unwrap().trim(), std::process::id().to_string());
            drop(lock);
        }
        assert!(!lock_path.exists(), "release must rmdir what it created");
        assert!(MaintainLock::acquire(&lock_path).is_ok(), "a released lock is free again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Upstream creates `cache\locks\LOCK_MAINTAIN` as a plain container and leaves it there, so its
    /// mere existence says nothing about an owner — and a foreign lock file inside it must survive.
    #[test]
    fn a_foreign_container_is_claimed_and_left_behind() {
        let dir = temp_tree("lock-foreign");
        let lock_path = dir.join("cache/locks/LOCK_MAINTAIN");
        std::fs::create_dir_all(&lock_path).unwrap();
        std::fs::write(lock_path.join("2026-09-22_03-00-00.md"), b"python holds this").unwrap();

        {
            let lock = MaintainLock::acquire(&lock_path).expect("an empty container is not a lock");
            drop(lock);
        }
        assert!(lock_path.exists(), "a directory we did not create is not ours to delete");
        assert!(lock_path.join("2026-09-22_03-00-00.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_owner_is_reclaimed_and_a_live_one_is_not() {
        let dir = temp_tree("lock-dead");
        let lock_path = dir.join("locks/LOCK_MAINTAIN");
        std::fs::create_dir_all(&lock_path).unwrap();
        std::fs::write(lock_path.join("PID"), "4000000").unwrap();
        MaintainLock::acquire(&lock_path).expect("a corpse lock is reclaimable");

        let other = lock_path.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let child = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ping ships with every Windows install");
        std::fs::write(other.join("PID"), child.id().to_string()).unwrap();
        let err = MaintainLock::acquire(&other).expect_err("a live owner is never stolen");
        assert!(err.contains(&format!("running process {}", child.id())), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffmpeg_output_is_trimmed_to_what_a_human_needs() {
        let text = "line 1\n\nline 2\nline 3\n";
        assert_eq!(tail_lines(text, 2), "line 3 | line 2");
        assert_eq!(tail_lines("", 3), "");
    }

    /// 停止整理 means *the encoder*, not the queue behind it. A child that is mid-file is put down inside
    /// its own first second of silence rather than given the rest of the minute it thinks it needs — which
    /// is the whole reason this wait is not `.output()`, and the reason `convert` and `expire` can delete a
    /// partial file and leave the slice unmarked instead of waiting to find out.
    #[test]
    fn a_running_encoder_is_put_down_when_the_pass_is_called_off() {
        let args: Vec<String> = ["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 30"]
            .into_iter()
            .map(str::to_string)
            .collect();
        if std::process::Command::new("powershell")
            .arg("-Version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            return; // the same assumption `schedule`'s lane fixture already makes
        }
        let asked = std::sync::atomic::AtomicUsize::new(0);
        // The answer turns to "no" on the first ask, which is what a stop request looks like from in here.
        let result = run_ffmpeg(std::path::Path::new("powershell"), &args, &|| {
            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
        });
        assert!(matches!(result, Err(Called::Off)), "{result:?}");
        assert!(
            asked.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "the wait asked the stop question instead of simply waiting"
        );
    }

    /// The other two answers stay distinguishable from a called-off one. A bad status is the *encoder's*
    /// failure and carries its own words; a missing binary is the same kind of answer, because the caller's
    /// cleanup is the same cleanup. Neither may read as "stopped", and a stop may never read as a failure —
    /// the step's report line counts them in separate columns for exactly that reason.
    #[test]
    fn an_encoder_that_fails_is_not_reported_as_an_encoder_that_was_stopped() {
        let args: Vec<String> = ["-NoProfile", "-NonInteractive", "-Command", "exit 3"].into_iter().map(str::to_string).collect();
        if std::process::Command::new("powershell")
            .arg("-Version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            return;
        }
        match run_ffmpeg(std::path::Path::new("powershell"), &args, &|| true) {
            Err(Called::Failed(why)) => assert!(why.contains("ffmpeg exited with"), "{why}"),
            other => panic!("a bad status must be a failure, not {other:?}"),
        }
        let nothing: Vec<String> = Vec::new();
        assert!(matches!(run_ffmpeg(std::path::Path::new("definitely-not-an-encoder.exe"), &nothing, &|| true), Err(Called::Failed(_))));
    }

    fn temp_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
