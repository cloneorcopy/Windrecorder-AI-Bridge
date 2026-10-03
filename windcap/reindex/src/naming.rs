//! Renaming, and the guard that keeps a rename inside the video library.
//!
//! Upstream marks a video's state *in its filename* rather than in a table: `X.mp4` becomes
//! `X-INDEX.mp4` before any work starts, `X-OCRED.mp4` when the rows are committed, and
//! `X-ERROR1.mp4` when the run dies. The `-INDEX` rename first is the important one — a crash between
//! the rename and the commit leaves a marker behind, and the next run reads it as "this one's rows are
//! suspect", rolls them back, and tries again. A tool that wrote rows before renaming would leave no
//! trace of an interrupted run at all.
//!
//! Two rules hold throughout. A video file is never deleted — only renamed, and the file it is renamed
//! to is derived from its own parent directory so a bad name cannot move it anywhere. And a name read
//! out of a database row, or typed by a caller, is checked to be a bare filename in the library before
//! anything touches it: `%APPDATA%/../..` is a legal string in a TEXT column.
//!
//! Note that `-OCRED` here is the *video* index marker and is a different string from
//! `wind_base::paths::MARKER_OCRED` (`-SCREENSHOTS-OCRED`), which marks a finished screenshot slice.

use std::path::{Component, Path, PathBuf};

/// Set while a run is in progress.
pub const MARKER_INDEX: &str = "-INDEX";
/// Committed. `ocr_process_videos` skips these, so the same video is never indexed twice.
pub const MARKER_OCRED: &str = "-OCRED";
/// Failed, with the attempt count appended: `-ERROR1`, `-ERROR2`, …
pub const MARKER_ERROR: &str = "-ERROR";

/// `const.ERROR_VIDEO_RETRY_TIMES`: past this many attempts a file is left alone.
pub const ERROR_VIDEO_RETRY_TIMES: i64 = 3;

/// The extension everything in this pipeline understands.
pub const VIDEO_EXTENSION: &str = ".mp4";

/// What a filename says about the state of its video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Never indexed. It gets marked `-INDEX` before work starts.
    Fresh,
    /// A previous run was interrupted between the marker rename and the commit. The rows written
    /// under the old name have to be rolled back, and the file is already marked, so it is not
    /// renamed again.
    Interrupted,
    /// A previous attempt failed and is being retried; `attempt` is the number the *next* failure
    /// will write.
    Retrying { attempt: i64 },
    /// Already indexed. Skipped and reported, never re-run.
    Done,
    /// Failed too many times; the driver's `ERROR_VIDEO_RETRY_TIMES` gate.
    RetryExhausted { attempt: i64 },
    /// Not a video file at all.
    NotAVideo,
}

impl State {
    /// Whether `index_video` should go ahead, and with what retry counter.
    pub fn should_index(&self) -> Option<i64> {
        match self {
            State::Fresh | State::Interrupted => Some(1),
            State::Retrying { attempt } => Some(*attempt),
            _ => None,
        }
    }

    /// Why indexing was declined, for the report.
    pub fn skip_reason(&self) -> Option<&'static str> {
        Some(match self {
            State::Done => "already indexed (-OCRED)",
            State::RetryExhausted { .. } => "too many failed attempts",
            State::NotAVideo => "not an .mp4",
            _ => return None,
        })
    }
}

/// Split a filename into `(stem, extension)`, at the *last* dot.
fn split_name(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, ext),
        _ => (name, ""),
    }
}

/// The recorded-at name a state-marked file came from: `{stem}.mp4`, no `-INDEX`, no `-ERRORn`.
///
/// This is what every database row is keyed by, so a run that used the marked name would write rows
/// no later lookup can find.
pub fn base_name(name: &str) -> String {
    let (stem, ext) = split_name(name);
    // Everything after the first marker is pipeline state, never part of the recorded-at name.
    let stem = stem.split(MARKER_ERROR).next().unwrap_or(stem);
    let stem = stem.replace(MARKER_INDEX, "").replace(MARKER_OCRED, "");
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}

/// The attempt number a `-ERROR{n}` name carries, or `None` if it is not an error file.
///
/// Upstream reads a single character after `-ERROR`, so a hand-made `-ERROR12.mp4` retries as 2. The
/// whole digit run is read here instead, which only differs for names this tool never produces.
pub fn error_attempt(name: &str) -> Option<i64> {
    let (_, digits) = name.split_once(MARKER_ERROR)?;
    let run: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!run.is_empty()).then(|| run.parse().ok()).flatten()
}

/// Read a filename's state.
pub fn classify(name: &str) -> State {
    if !name.ends_with(VIDEO_EXTENSION) {
        return State::NotAVideo;
    }
    if name.contains(MARKER_OCRED) {
        return State::Done;
    }
    if let Some(attempt) = error_attempt(name) {
        // Upstream's driver treats a non-digit suffix (`-ERRORX.mp4`) as un-retryable rather than as
        // attempt 1, because it reads one character and `isdigit()` fails.
        return if attempt > ERROR_VIDEO_RETRY_TIMES {
            State::RetryExhausted { attempt }
        } else {
            State::Retrying { attempt: attempt + 1 }
        };
    }
    if name.contains(MARKER_ERROR) {
        return State::RetryExhausted { attempt: ERROR_VIDEO_RETRY_TIMES + 1 };
    }
    if name.contains(MARKER_INDEX) {
        return State::Interrupted;
    }
    State::Fresh
}

/// `X.mp4` -> `X-INDEX.mp4`.
pub fn index_name(name: &str) -> String {
    let (stem, ext) = split_name(name);
    insert_marker(stem, MARKER_INDEX, ext)
}

/// `X-INDEX.mp4` (or any marked name) -> `X-OCRED.mp4`.
pub fn ocred_name(name: &str) -> String {
    let base = base_name(name);
    let (stem, ext) = split_name(&base);
    insert_marker(stem, MARKER_OCRED, ext)
}

/// The name a failed attempt leaves behind: `X-ERROR{n}.mp4`.
pub fn error_name(name: &str, attempt: i64) -> String {
    let base = base_name(name);
    let (stem, ext) = split_name(&base);
    format!("{stem}{MARKER_ERROR}{attempt}.{ext}")
}

fn insert_marker(stem: &str, marker: &str, ext: &str) -> String {
    if ext.is_empty() {
        format!("{stem}{marker}")
    } else {
        format!("{stem}{marker}.{ext}")
    }
}

/// The failure artefact's filename: `LOG_ERROR_{the name the file was renamed to}.MD`.
///
/// Upstream interpolates the *whole* renamed filename, extension included, so the artefact for
/// `X-ERROR1.mp4` is `LOG_ERROR_X-ERROR1.mp4.MD`. Users read these; matching the name is what lets a
/// `.MD` be lined up with the video beside it.
pub fn error_log_name(renamed: &str) -> String {
    format!("LOG_ERROR_{renamed}.MD")
}

/// Is `name` a bare filename that can be safely joined onto a library directory?
///
/// Rejects path separators, parent references, rooted and drive-qualified paths, and anything whose
/// final component is not the whole string. `Path::file_name` alone would still accept a name like
/// `foo/bar.mp4` only after the join, which is too late.
pub fn is_bare_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':', '\0'])
        && !name.starts_with('.')
        && Path::new(name).components().count() == 1
        && matches!(Path::new(name).components().next(), Some(Component::Normal(_)))
}

/// Join `name` onto `root` and prove the result is still inside `root`.
///
/// The lexical check is the one that rejects a crafted name; the canonical check runs only when the
/// file exists, because a symlink inside the library can point elsewhere and `canonicalize` is the
/// only way to see it. Both must pass, and a name that fails is `None` — the caller reports it, it
/// does not reach the filesystem.
pub fn resolve_in_library(root: &Path, name: &str) -> Option<PathBuf> {
    if !is_bare_filename(name) {
        return None;
    }
    let joined = root.join(name);
    let canonical_root = root.canonicalize().ok()?;
    match joined.canonicalize() {
        Ok(canonical) => {
            // Hand back the canonical form: it is the one path whose containment has actually been
            // proven, and on Windows it is the only form a later comparison can be trusted against.
            let is_file = canonical.is_file();
            let parent_ok = canonical.parent().map(|p| p.starts_with(&canonical_root)).unwrap_or(false);
            (is_file && parent_ok).then_some(canonical)
        }
        // A name that is bare, in the right directory and simply not present yet: fine, and the only
        // way a rename *target* can legitimately look.
        Err(_) => joined.parent().map(|p| p.starts_with(root)).unwrap_or(false).then_some(joined),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEG: &str = "2026-09-21_21-16-12.mp4";

    #[test]
    fn a_plain_video_is_fresh_and_its_stem_survives_every_suffix() {
        assert_eq!(classify(SEG), State::Fresh);
        assert_eq!(classify(SEG).should_index(), Some(1));
        assert_eq!(base_name(SEG), SEG);
        assert_eq!(index_name(SEG), "2026-09-21_21-16-12-INDEX.mp4");
        assert_eq!(ocred_name(SEG), "2026-09-21_21-16-12-OCRED.mp4");
        assert_eq!(error_name(SEG, 1), "2026-09-21_21-16-12-ERROR1.mp4");
    }

    #[test]
    fn an_interrupted_run_is_resumed_without_a_second_rename() {
        let name = "2026-09-21_21-16-12-INDEX.mp4";
        assert_eq!(classify(name), State::Interrupted);
        assert_eq!(classify(name).should_index(), Some(1));
        // Rows were written under the unmarked name, which is what the rollback deletes on.
        assert_eq!(base_name(name), SEG);
        // Failure from here still counts as attempt 1, exactly as upstream's branch sets it.
        assert_eq!(error_name(name, 1), "2026-09-21_21-16-12-ERROR1.mp4");
    }

    #[test]
    fn an_error_video_retries_at_the_next_number() {
        assert_eq!(classify("X-ERROR1.mp4"), State::Retrying { attempt: 2 });
        assert_eq!(classify("X-ERROR2.mp4"), State::Retrying { attempt: 3 });
        // Upstream's gate is `> ERROR_VIDEO_RETRY_TIMES`, so the third attempt is still allowed.
        assert_eq!(classify("X-ERROR3.mp4"), State::Retrying { attempt: 4 });
        assert_eq!(classify("X-ERROR4.mp4"), State::RetryExhausted { attempt: 4 }, "past the cap");
        assert_eq!(classify("X-ERROR3.mp4").should_index(), Some(4), "the last allowed attempt");
        assert_eq!(classify("X-ERROR4.mp4").should_index(), None);
        assert_eq!(error_attempt("X-ERROR1.mp4"), Some(1));
        assert_eq!(error_attempt("X.mp4"), None);
        // `-ERROR` with no number is upstream's non-digit case: it is not retried.
        assert_eq!(error_attempt("X-ERRORX.mp4"), None);
        assert_eq!(classify("X-ERRORX.mp4"), State::RetryExhausted { attempt: 4 });
    }

    /// Re-indexing a finished video would duplicate every row in the user's library, so the skip is a
    /// result and not an error — and it has to be reported, or a directory that looks like it did
    /// nothing looks like it worked.
    #[test]
    fn a_finished_video_is_skipped_and_says_why() {
        assert_eq!(classify("X-OCRED.mp4"), State::Done);
        assert_eq!(classify("X-OCRED.mp4").should_index(), None);
        assert_eq!(classify("X-OCRED.mp4").skip_reason(), Some("already indexed (-OCRED)"));
        assert!(classify("X.mp4").skip_reason().is_none());
    }

    #[test]
    fn only_mp4_files_are_videos() {
        assert_eq!(classify("notes.txt"), State::NotAVideo);
        assert_eq!(classify("X.MP4"), State::NotAVideo, "upstream's endswith is case-sensitive");
        assert_eq!(classify("X.mkv"), State::NotAVideo);
    }

    #[test]
    fn markers_are_immune_to_being_applied_twice() {
        // A `-ERROR2` file that succeeds must come out `-OCRED`, not `-ERROR2-OCRED`, because the row
        // keys and the driver's skip test both read the base name.
        assert_eq!(ocred_name("X-ERROR2.mp4"), "X-OCRED.mp4");
        assert_eq!(ocred_name("X-INDEX.mp4"), "X-OCRED.mp4");
        assert_eq!(base_name("X-ERROR2.mp4"), "X.mp4");
        assert_eq!(base_name("X-OCRED.mp4"), "X.mp4", "a finished name still keys its rows the same way");
        assert_eq!(index_name("X.mp4"), "X-INDEX.mp4");
    }

    #[test]
    fn the_error_log_is_named_after_the_renamed_file_extension_included() {
        assert_eq!(error_log_name("X-ERROR1.mp4"), "LOG_ERROR_X-ERROR1.mp4.MD");
    }

    #[test]
    fn a_crafted_name_is_rejected_before_it_touches_the_filesystem() {
        for name in [
            "../elsewhere.mp4",
            "..\\elsewhere.mp4",
            "sub/dir.mp4",
            "sub\\dir.mp4",
            "/absolute.mp4",
            "C:/Windows/x.mp4",
            r"\\server\share\x.mp4",
            "",
            ".hidden.mp4",
            "./x.mp4",
        ] {
            assert!(!is_bare_filename(name), "{name} must not be treated as a filename");
            assert_eq!(resolve_in_library(Path::new("videos"), name), None, "{name} escaped");
        }
        assert!(is_bare_filename("2026-09-21_21-16-12.mp4"));
        assert!(is_bare_filename("weird name-INDEX.mp4"));
    }

    #[test]
    fn a_real_file_in_the_library_resolves_and_a_missing_one_does_not_move() {
        let dir = std::env::temp_dir().join(format!("windcap-reindex-naming-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let video = SEG;
        std::fs::write(dir.join(video), b"x").unwrap();

        let resolved = resolve_in_library(&dir, video).expect("resolves");
        assert_eq!(resolved.file_name().unwrap().to_string_lossy(), video);
        assert!(resolved.starts_with(dir.canonicalize().unwrap()));
        assert!(resolved.is_file(), "and the answer is a path that can actually be opened");

        // A rename target that does not exist yet stays inside the library.
        let target = resolve_in_library(&dir, &index_name(video)).expect("target resolves");
        assert_eq!(target, dir.join(index_name(video)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_never_resolves_as_a_video() {
        let dir = std::env::temp_dir().join(format!("windcap-reindex-namedir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub.mp4")).unwrap();
        assert_eq!(resolve_in_library(&dir, "sub.mp4"), None, "a directory is not a segment");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
