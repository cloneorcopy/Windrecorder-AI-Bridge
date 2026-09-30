//! `wind-reindex` — index an already-recorded video, or a directory of them.
//!
//! This is the native equivalent of the maintenance thread's "index my library" step, narrowed to one
//! command that takes one video end to end. The argument grammar is a pure function of argv, so the
//! whole surface is exercised without an install directory; `--dry-run` prints the plan built from the
//! same argv builders a real run uses, which proves the ffmpeg and OCR commands without running either.
//!
//! Output is ASCII-escaped by default. This machine's console code page is 936, and Chinese OCR text
//! written straight to it comes back as mojibake that looks like plausible garbage characters — a trap
//! that has already fooled one engineer on this project. An escape sequence cannot be mistaken for
//! content; `--show-text` opts out for a terminal that can render CJK.

use std::io::Write;
use std::path::{Path, PathBuf};

use wind_base::clock::LocalParts;
use wind_base::{paths, version, Config};
use wind_reindex::frames::{self, Strategy};
use wind_reindex::index::{self, Counts, Outcome, Report, Settings};
use wind_reindex::naming;
use wind_reindex::timeline::segment_start_seconds;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_options(&argv) {
        Ok(options) => options,
        Err(Usage::Help) => {
            print(format_args!("{}", usage()));
            return;
        }
        // Before `run`, whose first act is `Config::load`. A version request carries no target and
        // no root, and must not be answered with "a video or a directory is required".
        Err(Usage::Version) => {
            print(format_args!("{}", version_line()));
            return;
        }
        Err(Usage::Problem(message)) => {
            eprintln!("{message}\n");
            eprint!("{}", usage());
            std::process::exit(2);
        }
    };

    let outcome = run(&options);
    // A resident OCR child is still talking on its own thread when this function ends, and the channel
    // library does not survive the C runtime pulling the ground out from under it: the process faults on
    // the way out, with a status code that says nothing about the recording that just succeeded. So the
    // engine is put down here, on every path, before the exit.
    wind_base::wxocr::shutdown();
    match outcome {
        Ok(totals) => {
            print(format_args!(
                "\n{} indexed · {} skipped · {} failed · {} rows written",
                totals.indexed, totals.skipped, totals.failed, totals.rows
            ));
            if totals.failed > 0 {
                std::process::exit(1);
            }
        }
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    }
}

#[derive(Debug, Clone)]
struct Options {
    /// The video, or the directory of them. Also the directory renames stay inside.
    target: Option<PathBuf>,
    /// `--file <path>`, repeatable: exactly these videos and nothing else. This is how the maintenance
    /// pass hands a *batch* of segments to one lane as the encode step finishes them, instead of waiting
    /// for the whole convert step: the caller names the videos it has already marked `-VIDEO`, so a lane
    /// never reads a file the encoder is still writing, and never re-walks a month folder to reach one
    /// segment. The library they must live in is the install's own `userdata/videos`, so a name that came
    /// out of a database row or a command line still cannot reach sideways into other footage.
    files: Vec<PathBuf>,
    root: PathBuf,
    dry_run: bool,
    limit: Option<usize>,
    show_text: bool,
    /// `--shard K/N`: work only every Nth video, starting at the Kth. Several lanes walk one library at
    /// once this way, and because the list is sorted by the stamp in the name before it is dealt out, the
    /// lanes' sets are disjoint and stable — the same file is the same lane's work in every run of the
    /// same library, so a marker a lane writes cannot be missed by its neighbour.
    shard: (usize, usize),
}

#[derive(Debug)]
enum Usage {
    Help,
    /// `--version`/`-V`: name, package version, build profile. Its own arm rather than another
    /// spelling of `Help`, because the answer is one line about this executable and it must be
    /// given without a target, a root, a config or an index.
    Version,
    Problem(String),
}

/// Parse argv. Total and side-effect free: no file is opened and no config is read here, so the whole
/// grammar is testable without an install tree.
fn parse_options(argv: &[String]) -> Result<Options, Usage> {
    let mut target: Option<PathBuf> = None;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut root = None;
    let mut dry_run = false;
    let mut show_text = false;
    let mut limit = None;
    let mut shard: (usize, usize) = (0, 1);
    let mut position = 0;

    while position < argv.len() {
        let argument = argv[position].as_str();
        let (key, inline) = match argument.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (argument, None),
        };
        let mut value_of = |what: &str| -> Result<String, Usage> {
            if let Some(inline) = inline.clone() {
                return Ok(inline);
            }
            position += 1;
            argv.get(position).cloned().ok_or_else(|| Usage::Problem(format!("{what} needs a value")))
        };

        match key {
            "-h" | "--help" => return Err(Usage::Help),
            // Read here, in the same arm list as `--help`, so that a version request is refused
            // neither as an unknown option nor as the missing positional target.
            flag if version::is_flag(flag) => return Err(Usage::Version),
            "--root" => root = Some(PathBuf::from(value_of("--root")?)),
            "--dry-run" => dry_run = true,
            "--show-text" => show_text = true,
            "--limit" => {
                let text = value_of("--limit")?;
                let parsed: usize = text
                    .parse()
                    .map_err(|_| Usage::Problem(format!("--limit needs a number, got {text}")))?;
                limit = Some(parsed);
            }
            // `--shard K/N`: this process is lane K of N, and takes every Nth video of the sorted
            // library. Refused unless `K < N` and `N >= 1`, because `--shard 4/4` would be an empty lane
            // nobody asked for and `--shard 1/0` is a division by zero wearing the shape of a fraction.
            "--shard" => {
                let text = value_of("--shard")?;
                let (index, lanes) = text
                    .split_once('/')
                    .ok_or_else(|| Usage::Problem(format!("--shard wants K/N, got {text}")))?;
                let index: usize = index
                    .parse()
                    .map_err(|_| Usage::Problem(format!("--shard: {index} is not a lane number")))?;
                let lanes: usize = lanes
                    .parse()
                    .map_err(|_| Usage::Problem(format!("--shard: {lanes} is not a lane count")))?;
                if lanes == 0 || index >= lanes {
                    return Err(Usage::Problem(format!("--shard {text} names no lane of a {lanes}-lane walk")));
                }
                shard = (index, lanes);
            }
            // `--file <path>`, repeatable: this walk is exactly the named videos. Given by the
            // maintenance pass for a batch of segments the encode step has already marked `-VIDEO`, so
            // the back-index can start on the hours that are finished while the encoder is still
            // working on the later ones. A directory walk would reach half-written files and months of
            // footage nobody asked it to touch yet.
            "--file" => files.push(PathBuf::from(value_of("--file")?)),
            other if other.starts_with('-') => return Err(Usage::Problem(format!("unknown option '{other}'"))),
            other => {
                if target.is_some() {
                    return Err(Usage::Problem(format!("unexpected extra argument '{other}'")));
                }
                target = Some(PathBuf::from(other));
            }
        }
        position += 1;
    }

    if target.is_none() && files.is_empty() {
        return Err(Usage::Problem("a video, a directory, or at least one --file is required".to_string()));
    }
    Ok(Options { target, files, root: root.unwrap_or_else(default_root), dry_run, limit, show_text, shard })
}

fn usage() -> String {
    "wind-reindex — make already-recorded video searchable\n\
     \n\
     usage: wind-reindex <video-or-dir> [--root PATH] [--dry-run] [--limit N] [--show-text]\n\
                          [--shard K/N]\n\
            wind-reindex --file A.mp4 --file B.mp4 [...]   (every option above applies to a batch too)\n\
     \n\
       <video-or-dir>  one .mp4, or a directory walked recursively, oldest first. A directory is the\n\
                       containment root: every month folder under it is in scope, and a rename stays\n\
                       inside the folder its video lives in.\n\
       --file PATH     work exactly the named videos instead of walking anything. Repeatable, and the\n\
                       only way to name work without a target: this is how the maintenance pass hands\n\
                       one lane the batch of segments the encode step has just marked -VIDEO, so the\n\
                       back-index starts on the finished hours while the encoder is still working on\n\
                       later ones. They must live in this install's videos library, and they are\n\
                       ordered oldest first the way a walk orders them, so a batch dealt over lanes\n\
                       stays disjoint.\n\
       --root PATH     the install root holding config_src/, userdata/ and ocr_lib/. Defaults\n\
                       to this executable's directory, or to the ancestor of it that has either\n\
                       config_src/ or userdata/ — which is also how an older install that keeps\n\
                       its settings under windrecorder/ is still found.\n\
       --dry-run       extract nothing, OCR nothing, write nothing: print the plan instead.\n\
       --limit N       stop after N videos.\n\
       --shard K/N     work only every Nth video, starting at the Kth (0-based). This is how one\n\
                       library is walked by several processes at once: each lane gets its own OCR\n\
                       engine, and the deal is made after the oldest-first sort, so the lanes stay\n\
                       disjoint and the same file is the same lane's work in every run.\n\
       --show-text     print each stored row's text unescaped, for a terminal that renders CJK.\n\
       --version | -V  print this binary's name, its package version and its build profile. Needs\n\
                       no target and reads no config, so it answers on a broken install.\n\
     \n\
     A file already marked -OCRED is skipped and reported. A file marked -INDEX or -ERRORn is\n\
     rolled back and retried. Video files are never deleted, only renamed.\n\
     \n\
     A video whose segment the recorder already wrote rows for is not read a second time: the\n\
     maintenance pass's own text step recognises the frames as they were captured, and OCR-ing the\n\
     same hour off the video would double the work and double the rows. Such a video is marked\n\
     -OCRED and skipped, with the row count in the reason. If its rows are still waiting for text\n\
     and its slice folder is on disk, nothing is marked — that step can still finish the job.\n\
     A video whose slice folder is on disk *unmarked* is deferred and nothing is renamed on it: the\n\
     encode step writes the final name and marks the slice -VIDEO only after ffmpeg has returned, so\n\
     an unmarked slice is evidence the file may still be growing. A truncated video read here would\n\
     be marked done and the rest of the hour lost.\n"
        .to_string()
}

/// The library this run may touch.
///
/// A directory target is its own containment root, so every month folder under it is in scope; a single
/// video's folder is the root, so a hand-typed path cannot reach sideways into somebody else's library.
/// This used to be answered with the *first target's parent*, which for a whole-library walk named the
/// oldest month folder as the root and then reported every newer month's video as
/// `outside the videos directory` — an install with two months of footage could never finish indexing
/// the second one, and the newest hours stayed unsearchable without a word to say so.
fn containment_root(target: &Path, root: &Path) -> PathBuf {
    if target.is_dir() {
        return target.to_path_buf();
    }
    target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| root.to_path_buf())
}

/// The thing the caller named, for a report line: a directory, one file, or a batch of them.
fn named_source(options: &Options) -> String {
    match &options.target {
        Some(target) => target.display().to_string(),
        None => format!("{} --file target(s)", options.files.len()),
    }
}

/// The same order [`collect`] gives a directory walk, applied to a list the caller already holds: sorted
/// by the timestamp in the name, so a batch dealt over several lanes stays disjoint and stable exactly
/// the way a walked library is.
fn order_by_stamp(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by_key(|path| {
        let name = file_name_of(path);
        let base = naming::base_name(&name);
        (segment_start_seconds(&base).unwrap_or(i64::MAX), base)
    });
    paths
}

/// The lane's share of a sorted library: every `lanes`th video, starting at `index`.
///
/// A function of its own rather than three lines inside [`run`], because the whole promise of the flag
/// — that the lanes are disjoint and together cover the library — is a property of this arithmetic, and
/// that is worth a test that touches no disk.
fn dealt(targets: Vec<PathBuf>, shard: (usize, usize)) -> Vec<PathBuf> {
    let (index, lanes) = shard;
    if lanes <= 1 {
        return targets;
    }
    targets
        .into_iter()
        .enumerate()
        .filter(|(position, _)| position % lanes == index)
        .map(|(_, path)| path)
        .collect()
}

/// What `wind-reindex --version` prints. The format is `wind_base::version`'s, shared by all eleven
/// binaries; the name is the `[[bin]]` name and the version is this crate's own.
fn version_line() -> String {
    version::line("wind-reindex", env!("CARGO_PKG_VERSION"))
}

/// The install root: the directory carrying this install's shipped settings, found by walking up
/// from this executable. The same [`wind_base::install`] rule `windmaint` uses, so one `--root`
/// means one thing across the toolset — and a reindex that resolved somewhere else would rewrite
/// the wrong month files.
fn default_root() -> PathBuf {
    wind_base::install::resolve_root_from_exe(None)
}

/// The tally a whole command run leaves behind, which is what decides the exit status.
#[derive(Debug, Default)]
struct Totals {
    indexed: usize,
    skipped: usize,
    failed: usize,
    rows: usize,
}

#[derive(Debug)]
struct Stored {
    time: i64,
    wall_clock: String,
    picture: String,
    text: String,
}

fn run(options: &Options) -> Result<Totals, String> {
    let config = Config::load(&options.root).map_err(|e| e.to_string())?;
    let mut settings = Settings::from_config(&config);

    // Two shapes of work list: a directory (or one file) the caller pointed at, or the batch of videos
    // named one by one with `--file`. Both end up oldest-first, because the walk and the lane deal are
    // both defined on that order.
    let targets = match &options.target {
        Some(target) => collect(target)?,
        None => order_by_stamp(options.files.clone()),
    };
    let library = match &options.target {
        // Renames happen beside the file being renamed, so the directory the containment check enforces
        // is the one the caller pointed at — see [`containment_root`] for why that is not the first
        // target's parent, which is what this line used to say and what silently fenced a multi-month
        // library into its oldest folder.
        Some(target) => containment_root(target, &options.root),
        // A named batch belongs to this install's library, wherever in it the month folders are.
        None => config.videos_dir(),
    };
    if targets.is_empty() {
        return Err(match &options.target {
            Some(target) => format!("no .mp4 files under {}", target.display()),
            None => "no --file was readable".to_string(),
        });
    }
    settings.videos_dir = library;

    // The lane's share. Dealt after the oldest-first sort and before anything is opened, so a lane that
    // has nothing to do says so and leaves without touching a database or starting an engine.
    let targets = dealt(targets, options.shard);
    if targets.is_empty() {
        // An empty lane is not a failure. This is what lets the caller start more lanes than the library
        // strictly needs without a spurious non-zero exit for the surplus.
        print(format_args!(
            "{} holds no video for lane {}/{}",
            named_source(options),
            options.shard.0,
            options.shard.1
        ));
        return Ok(Totals::default());
    }

    print(format_args!(
        "root      {}\nlibrary   {}\nencoder   {} ({})\nframerate {} · displays {} · threshold {:.3}\n",
        options.root.display(),
        settings.videos_dir.display(),
        settings.encoder,
        classify_strategy(&settings.encoder),
        settings.framerate,
        settings.display_count,
        settings.similarity_threshold(),
    ));

    let mut totals = Totals::default();
    for path in &targets {
        // Somebody pressed stop. Read, not taken: the walk is a step inside `windmaint`'s pass, and the
        // steps after it have to see the same request. The pass that honours it clears it.
        if !options.dry_run && config.maintain_stop_requested() {
            print(format_args!(
                "… stopped by request: the remaining {} target(s) were not attempted",
                targets.len() - totals.considered().min(targets.len())
            ));
            break;
        }
        // The limit counts videos looked at, not rows written, so `--limit 3` means "the three oldest".
        if options.limit.is_some_and(|limit| totals.considered() >= limit) {
            print(format_args!("… stopped at the --limit"));
            break;
        }
        // The engine went quiet inside the video before this one, which is what stopped it. Starting the
        // next would spend three more timeouts proving the same thing, once per video, for every video in
        // the library. The counter is this engine's own, so a command-line engine never trips it.
        if !options.dry_run && wind_base::wxocr::has_gone_quiet() {
            print(format_args!(
                "… stopped: the OCR engine has not answered for {} frames in a row. \
                 `windsetup check-engines` scores it; the remaining {} target(s) were not attempted",
                wind_base::wxocr::GIVE_UP_AFTER,
                targets.len() - totals.considered().min(targets.len())
            ));
            break;
        }
        let name = file_name_of(path);
        print(format_args!("\n{} [{name}]", if options.dry_run { "would" } else { "indexing" }));
        let report = if options.dry_run {
            let report = dry_run(&settings, path, &name);
            print_report(&report);
            report
        } else {
            let report = index::index_video_path(&settings, path);
            // Read back over a separate read-only connection, so what is printed is what a search will
            // actually find rather than what the writer believed it had stored.
            let rows = read_back(&settings, &report);
            print_report_rows(&report, &rows, options.show_text);
            index::cleanup_frames(&settings, &report.video);
            report
        };
        totals.tally(&report);
    }
    Ok(totals)
}

impl Totals {
    fn considered(&self) -> usize {
        self.indexed + self.skipped + self.failed
    }

    fn tally(&mut self, report: &Report) {
        match &report.outcome {
            Outcome::Indexed => {
                self.indexed += 1;
                self.rows += report.counts.written;
            }
            Outcome::Skipped(_) => self.skipped += 1,
            Outcome::Failed { .. } => self.failed += 1,
        }
    }
}

fn classify_strategy(encoder: &str) -> &'static str {
    match frames::strategy_for_encoder(encoder) {
        Strategy::Stride => "one frame every 4s",
        Strategy::KeyFrame => "I-frames only",
    }
}

/// The plan for one video, with nothing executed.
fn dry_run(settings: &Settings, path: &Path, name: &str) -> Report {
    let video = naming::base_name(name);
    let state = naming::classify(name);
    let out_dir = settings.frame_dir(&video);
    let step = frames::frame_step_for(settings.framerate.max(1) as f64, frames::IFRAME_INTERVAL_MS);
    let command = match frames::strategy_for_encoder(&settings.encoder) {
        Strategy::Stride => frames::stride_args(&settings.ffmpeg, path, &out_dir, step),
        Strategy::KeyFrame => frames::iframe_args(&settings.ffmpeg, path, &out_dir),
    };
    print(format_args!("  extract {}", join_argv(&command)));
    // The engine the *config* selects, not the one this binary used to assume: a dry run that printed the
    // built-in while indexing with something else is the lie this command exists to avoid.
    //
    // `describe()` rather than the argv, because an argv is only one engine's shape. The resident service
    // has no command line at all, and printing the empty one here would read as "this engine runs nothing".
    print(format_args!(
        "  ocr     {} ({})",
        settings.ocr.describe(),
        settings.ocr.name()
    ));
    if let Some(note) = settings.ocr.note() {
        print(format_args!("  ocr note  {note}"));
    }
    print(format_args!("  mask    {:?}", settings.urbl));
    print(format_args!("  index   {}", settings.db_dir.display()));

    let outcome = match state.should_index() {
        None => Outcome::Skipped(state.skip_reason().unwrap_or("skipped").to_string()),
        Some(_) if !path.is_file() => Outcome::Skipped("not a file".to_string()),
        Some(_) if !settings.ocr.is_installed() => {
            Outcome::Skipped(format!("OCR engine {} is not installed", settings.ocr.name()))
        }
        Some(_) => Outcome::Skipped("would index".to_string()),
    };
    Report { video, outcome, counts: Counts::default(), final_name: name.to_string() }
}

fn print_report(report: &Report) {
    let counts = &report.counts;
    print(format_args!(
        "  {:<7} frames {} · kept {} · similar {} · short {} · excluded {} · dup {} · unreadable {}",
        report.outcome.label(),
        counts.frames,
        counts.written,
        counts.similar,
        counts.too_short,
        counts.excluded,
        counts.duplicates,
        counts.unreadable,
    ));
    match &report.outcome {
        Outcome::Indexed => print(format_args!("  renamed {}", report.final_name)),
        Outcome::Skipped(reason) => print(format_args!("  reason  {reason}")),
        Outcome::Failed { error, renamed_to, log_file } => {
            print(format_args!("  renamed {renamed_to}"));
            print(format_args!("  log     {log_file}"));
            print(format_args!("  error   {}", ascii(error)));
        }
    }
}

/// [`print_report`] plus every stored row, which is the part a dry run has nothing to show.
fn print_report_rows(report: &Report, rows: &[Stored], show_text: bool) {
    print_report(report);
    for row in rows {
        print(format_args!("  row     {}  epoch {}  {}", row.wall_clock, row.time, ascii(&row.picture)));
        print(format_args!("          {}", if show_text { row.text.clone() } else { ascii(&row.text) }));
    }
}

/// The committed rows, read back through a read-only connection.
fn read_back(settings: &Settings, report: &Report) -> Vec<Stored> {
    if !matches!(report.outcome, Outcome::Indexed) {
        return Vec::new();
    }
    let Some(start) = segment_start_seconds(&report.video) else { return Vec::new() };
    let mut out = Vec::new();
    for (year, month) in index::months_between(start, start + settings.record_seconds) {
        let database = settings.db_dir.join(paths::month_filename(&settings.user_name, year, month));
        if !database.is_file() {
            continue;
        }
        let Ok(connection) = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else { continue };
        let Ok(mut statement) = connection.prepare(
            "SELECT videofile_time, picturefile_name, ocr_text FROM video_text \
             WHERE videofile_name LIKE ? ORDER BY videofile_time, rowid",
        ) else { continue };
        let mapped = statement.query_map(rusqlite::params![index::like_pattern(&report.video)], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        });
        if let Ok(mapped) = mapped {
            for row in mapped.flatten() {
                out.push(Stored {
                    time: row.0,
                    wall_clock: LocalParts::from_naive_epoch(row.0).display(),
                    picture: row.1,
                    text: row.2,
                });
            }
        }
    }
    out.sort_by_key(|row| row.time);
    out
}

/// The `.mp4` files to work on, oldest first.
///
/// Recursive, as upstream's `os.walk` driver is, and sorted by the timestamp in the name rather than by
/// directory order: a month folder read in filesystem order would index a user's December before their
/// March, and the whole point of "oldest first" is that the newest material is the last thing touched.
fn collect(target: &Path) -> Result<Vec<PathBuf>, String> {
    if target.is_file() {
        return Ok(vec![target.to_path_buf()]);
    }
    if !target.is_dir() {
        return Err(format!("{} is neither a file nor a directory", target.display()));
    }
    let mut found = Vec::new();
    let mut stack = vec![target.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if file_name_of(&path).ends_with(".mp4") {
                found.push(path);
            }
        }
    }
    Ok(order_by_stamp(found))
}

fn file_name_of(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// Render `text` with every non-ASCII character escaped, so it survives any console.
fn ascii(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => "\\n".to_string(),
            '\r' => String::new(),
            '\t' => "\\t".to_string(),
            c if c == ' ' || c.is_ascii_graphic() => c.to_string(),
            c => format!("\\u{{{:x}}}", c as u32),
        })
        .collect()
}

fn join_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| if argument.contains(' ') { format!("'{argument}'") } else { argument.clone() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `println` taking a pre-built `Arguments`, so a report line can be assembled from data without a
/// format string.
fn print(args: std::fmt::Arguments<'_>) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{args}");
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_bare_target_is_enough() {
        let options = parse_options(&argv(&["D:/lib/X.mp4"])).unwrap();
        assert_eq!(options.target.as_deref(), Some(Path::new("D:/lib/X.mp4")));
        assert!(!options.dry_run);
        assert!(!options.show_text);
        assert_eq!(options.limit, None);
        assert_eq!(options.shard, (0, 1), "no flag means this process walks the whole library");
    }

    /// A batch is named one file at a time, and a batch is deliberately not a target: the two shapes of
    /// work differ exactly in whether anything is walked, so a parse that folded them together would let a
    /// lane handed one segment decide to re-index the month folder it lives in.
    #[test]
    fn a_batch_is_named_one_file_at_a_time_and_walks_nothing() {
        let options = parse_options(&argv(&[
            "--file",
            "D:/lib/2026-09-21_10-00-00.mp4",
            "--file=D:/lib/2026-09-20_10-00-00.mp4",
            "--root",
            "D:/i",
        ]))
        .unwrap();
        assert!(options.target.is_none(), "a batch names no directory, so [`collect`] is never asked");
        assert_eq!(options.files.len(), 2, "both spellings of the value are the same flag");
        assert_eq!(options.files[0], PathBuf::from("D:/lib/2026-09-21_10-00-00.mp4"), "the options keep the given order");
        assert_eq!(options.root, PathBuf::from("D:/i"));
        assert_eq!(named_source(&options), "2 --file target(s)", "and a report line says how many, not a path");
    }

    /// A batch is ordered oldest first exactly the way [`collect`] orders a walk, because the lane deal and
    /// the "this lane has nothing to do" report are both defined on that order. A name with no stamp in it
    /// sorts last rather than being dropped — the walk keeps such a file today, and a batch that quietly
    /// lost one would leave a segment nobody's census counted.
    #[test]
    fn a_batch_is_ordered_the_way_a_walk_is() {
        let ordered = order_by_stamp(vec![
            PathBuf::from("D:/lib/2026-09-21_10-05-00.mp4"),
            PathBuf::from("D:/lib/holiday.mp4"),
            PathBuf::from("D:/lib/2026-09-21_09-59-59.mp4"),
            PathBuf::from("D:/lib/2026-09-20_23-59-00.mp4"),
        ]);
        let names: Vec<String> = ordered.iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(
            names,
            vec!["2026-09-20_23-59-00.mp4", "2026-09-21_09-59-59.mp4", "2026-09-21_10-05-00.mp4", "holiday.mp4"],
            "oldest first, and the unstamped name last"
        );
    }

    /// Naming nothing is a refusal, not an invitation to walk the library. A lane whose batch arrived empty
    /// must be told so by its own command line; a second process re-indexing the whole install because a
    /// caller passed no arguments is the opposite of the containment rule this binary exists to hold.
    #[test]
    fn naming_nothing_at_all_is_refused() {
        let refused = parse_options(&argv(&["--root", "D:/i"])).expect_err("no target and no --file is not a walk");
        assert!(matches!(&refused, Usage::Problem(why) if why.contains("--file")), "{refused:?}");
    }

    /// The lanes have to be disjoint and together cover the library, or two processes would fight over
    /// one video and a third would be nobody's work.
    #[test]
    fn the_shard_deal_covers_the_library_once_and_never_twice() {
        let library: Vec<PathBuf> =
            (0..11).map(|n| PathBuf::from(format!("2026-09-21_10-{n:02}-00.mp4"))).collect();
        for lanes in 1..=4 {
            let dealt: Vec<Vec<PathBuf>> = (0..lanes).map(|index| dealt(library.clone(), (index, lanes))).collect();
            let total: usize = dealt.iter().map(Vec::len).sum();
            assert_eq!(total, library.len(), "{lanes} lanes lost or invented a video");
            let mut seen = dealt.concat();
            let unique = {
                seen.sort();
                seen.dedup();
                seen.len()
            };
            assert_eq!(unique, library.len(), "a video appeared in two lanes");
            for (index, lane) in dealt.iter().enumerate() {
                for path in lane {
                    assert_eq!(library.iter().position(|one| one == path).unwrap() % lanes, index, "{path:?}");
                }
            }
        }
        // A lane past the end of a small library is an empty lane, and an empty lane is not an error.
        let one = vec![PathBuf::from("2026-09-21_10-00-00.mp4")];
        assert!(dealt(one, (1, 8)).is_empty(), "lane 1 of 8 has nothing in a one-video library");
        assert_eq!(dealt(library.clone(), (0, 1)).len(), library.len(), "no deal, no change");
    }

    /// `--shard` is a fraction or it is nothing: `4/4` is an empty lane somebody typed wrong and `1/0`
    /// is a modulo by zero wearing the shape of a flag.
    #[test]
    fn a_shard_is_read_in_both_spellings_and_nonsense_is_refused() {
        for form in [argv(&["X.mp4", "--shard", "2/5"]), argv(&["X.mp4", "--shard=2/5"])] {
            assert_eq!(parse_options(&form).unwrap().shard, (2, 5));
        }
        for bad in ["5/5", "9/4", "0/0", "a/b", "2", "/4", "2/"] {
            assert!(parse_options(&argv(&["X.mp4", "--shard", bad])).is_err(), "{bad} must be refused");
        }
    }

    /// The whole reason a multi-month library is one library: the directory the caller pointed at is the
    /// containment root, not the month folder the first video happens to sit in.
    #[test]
    fn a_directory_target_holds_every_month_folder_under_it() {
        let root = std::env::temp_dir().join(format!("windcap-reindex-root-{}", std::process::id()));
        let library = root.join("userdata/videos");
        for month in ["2026-08", "2026-09"] {
            std::fs::create_dir_all(library.join(month)).unwrap();
        }
        let one = library.join("2026-09/2026-09-10_00-00-00.mp4");
        std::fs::write(&one, b"x").unwrap();

        assert_eq!(containment_root(&library, &root), library, "a directory walks everything under it");
        assert_eq!(
            containment_root(&one, &root),
            library.join("2026-09"),
            "one video may only be renamed beside itself"
        );
        // A bare name typed in a shell has no parent to speak of: it falls back to the install root.
        assert_eq!(containment_root(Path::new("loose.mp4"), &root), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn usage_names_the_shard_and_the_skip_a_covered_segment_gets() {
        let text = usage();
        for wanted in ["--shard K/N", "containment root", "already wrote rows", "-OCRED"] {
            assert!(text.contains(wanted), "{wanted} missing from the help:\n{text}");
        }
    }

    #[test]
    fn every_option_has_a_spelling_with_a_space_and_one_with_an_equals() {
        let spaced = argv(&["X.mp4", "--root", "D:/i", "--dry-run", "--limit", "3", "--show-text"]);
        let attached = argv(&["X.mp4", "--root=D:/i", "--dry-run", "--limit=3", "--show-text"]);
        for form in [spaced, attached] {
            let options = parse_options(&form).unwrap();
            assert_eq!(options.root, PathBuf::from("D:/i"));
            assert!(options.dry_run);
            assert!(options.show_text);
            assert_eq!(options.limit, Some(3));
        }
    }

    #[test]
    fn nothing_about_the_filesystem_happens_while_parsing() {
        // A nonexistent target parses fine: `--dry-run` has to be able to describe what it would do to
        // a file it cannot yet open.
        assert!(parse_options(&argv(&["Z:/nope/missing.mp4", "--dry-run"])).is_ok());
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        for bad in [
            argv(&[]),
            argv(&["--dryrun"]),
            argv(&["a.mp4", "b.mp4"]),
            argv(&["X.mp4", "--limit"]),
            argv(&["X.mp4", "--limit", "many"]),
            argv(&["X.mp4", "--limit", "-1"]),
            argv(&["--root"]),
        ] {
            assert!(matches!(parse_options(&bad), Err(Usage::Problem(_))), "{bad:?} should be refused");
        }
        assert!(matches!(parse_options(&argv(&["--help"])), Err(Usage::Help)));
        assert!(matches!(parse_options(&argv(&["-h"])), Err(Usage::Help)));
    }

    #[test]
    fn a_limit_of_zero_is_a_number_and_means_no_videos() {
        assert_eq!(parse_options(&argv(&["X.mp4", "--limit", "0"])).unwrap().limit, Some(0));
    }

    #[test]
    fn non_ascii_output_is_escaped_rather_than_misrendered() {
        assert_eq!(ascii("plain text"), "plain text");
        assert_eq!(ascii("庞加莱"), "\\u{5e9e}\\u{52a0}\\u{83b1}");
        assert_eq!(ascii("a\r\nb"), "a\\nb", "a CR is a line ending, not content");
        assert_eq!(ascii("tab\there"), "tab\\there");
        assert_eq!(ascii(" -||- ChatGPT"), " -||- ChatGPT", "the separator must stay readable");
    }

    #[test]
    fn an_argv_with_spaces_is_quoted_for_reading_back() {
        let joined = join_argv(&["ffmpeg".to_string(), "-vf".to_string(), "select='eq(a\\,b)'".to_string()]);
        assert_eq!(joined, "ffmpeg -vf select='eq(a\\,b)'");
        assert_eq!(join_argv(&["a".to_string(), "b c".to_string()]), "a 'b c'");
    }

    #[test]
    fn the_encoder_name_shows_up_as_a_human_word() {
        assert_eq!(classify_strategy("cpu_h264"), "one frame every 4s");
        assert_eq!(classify_strategy("cpu_av1"), "I-frames only");
    }

    #[test]
    fn usage_lists_every_option_it_accepts() {
        let text = usage();
        for needle in ["--root", "--dry-run", "--limit", "<video-or-dir>", "--show-text", "--shard", "--file"] {
            assert!(text.contains(needle), "{needle} undocumented");
        }
    }

    /// The version is asked for without a target, and answered without one: the two are the same
    /// claim, since a request that fell through to the grammar below would fail with "a video or a
    /// directory is required" instead.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("wind-reindex "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        for spelling in ["--version", "-V"] {
            assert!(matches!(parse_options(&argv(&[spelling])), Err(Usage::Version)), "{spelling}");
            // Not a target, and not an unknown option either.
            assert!(matches!(parse_options(&argv(&[spelling, "Z:/nope"])), Err(Usage::Version)), "{spelling}");
        }
        assert!(usage().contains("--version"), "answered but undocumented:\n{}", usage());
    }

    #[test]
    fn a_directory_is_walked_and_read_in_time_order() {
        let root = std::env::temp_dir().join(format!("windcap-reindex-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("2026-10")).unwrap();
        std::fs::create_dir_all(root.join("2026-09")).unwrap();
        // Written newest-first on purpose: the sort has to undo the directory order, not inherit it.
        for (dir, name) in [
            ("2026-10", "2026-10-01_09-00-00.mp4"),
            ("2026-09", "2026-09-30_23-50-00.mp4"),
            ("2026-09", "2026-09-21_21-16-12.mp4"),
            ("2026-09", "notes.txt"),
        ] {
            std::fs::write(root.join(dir).join(name), b"x").unwrap();
        }
        let found = collect(&root).unwrap();
        let names: Vec<String> = found.iter().map(|p| file_name_of(p)).collect();
        assert_eq!(
            names,
            vec!["2026-09-21_21-16-12.mp4", "2026-09-30_23-50-00.mp4", "2026-10-01_09-00-00.mp4"],
            "oldest first, across folders"
        );
        assert_eq!(collect(&found[0]).unwrap().len(), 1, "a single file is its own one-item list");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_target_that_is_not_a_video_directory_is_a_clear_error() {
        let missing = std::env::temp_dir().join(format!("windcap-reindex-missing-{}", std::process::id()));
        assert!(collect(&missing).is_err());
    }
}
