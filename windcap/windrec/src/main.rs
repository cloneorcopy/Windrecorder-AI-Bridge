//! `windrec` — the recorder as a single self-contained executable.
//!
//! This replaces the Python capture loop (`windrecorder/record.py::record_screen_via_screenshot_process`)
//! end to end: it reads the same config files, decides what is worth keeping, runs the OCR engine,
//! and writes the same monthly SQLite index. It embeds SQLite, so the shipped .exe needs nothing
//! but the OCR engine binary that already ships in `ocr_lib/`.
//!
//! Two ordering decisions differ from the Python loop, and both are measurable:
//!   * the change gate runs *before* OCR. The Python loop wrote the frame, cropped it, paid for an
//!     OCR subprocess, and only then threw the frame away for having text that overlapped the last
//!     one. Gating first means unchanged screens cost nothing.
//!   * nothing is re-encoded from disk. Pixels come out of one GDI pass and feed the gate, the OCR
//!     input and the thumbnail in memory.

mod ocr;
mod recorder;
mod segment;
mod status;
mod wintitle;

use std::path::PathBuf;

use wind_base::config::Config;
use wind_base::image as thumb;
use wind_base::version;
use wind_setup::engines::{self, Probe, Status};
use windcap::capture::{foreground_rect, make_thread_dpi_aware, virtual_desktop, Grabber};
use windcap::gate::{ChangeGate, GateConfig};
use windcap::winstate;

#[derive(Debug)]
struct Options {
    root: PathBuf,
    /// `--seconds N`: the wall-clock budget for `run`, in seconds. `0` means "use `record_seconds`".
    seconds: i64,
    /// `--interval-seconds N`, already known to be whole seconds. `None` means "use the config".
    interval_seconds: Option<i64>,
    /// Which of the three things a run does with a kept frame. Built from `--no-ocr`/`--gate-only`.
    mode: recorder::Mode,
}

/// Everything `run` and `loop` hand to the recorder, derived from the flags and the config in one
/// place so that a flag cannot be parsed, echoed, and then quietly left out of the call.
///
/// This is the only producer of the arguments to [`recorder::Recorder::open`] and of the deadline
/// given to `drive`, which is what makes the tests below worth anything: asserting that a flag
/// parses is worthless when the bug is that the parsed value goes nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Session {
    mode: recorder::Mode,
    /// Wall-clock budget for a one-shot run.
    seconds: i64,
    /// Whether a closed segment is followed by a fresh one. `false` would make `--seconds` a ceiling
    /// rather than a duration, which is the other thing this struct used to get wrong.
    rotate: bool,
    interval_seconds: Option<i64>,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let command = argv.first().map(String::as_str).unwrap_or("help");
    // Answered before `parse_options`, before `Config::load` and before the record lock, in the
    // slot where a subcommand would otherwise be read: the one question a user with a broken
    // install -- no `config_src/`, an unreadable `userdata/config_user.json`, a locked cache --
    // must always be able to ask is which build they are holding.
    if version::is_flag(command) {
        println!("{}", version_line());
        return;
    }
    let rest = &argv[if argv.is_empty() { 0 } else { 1 }..];

    let options = match parse_options(command, rest) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    let result = match command {
        "doctor" => doctor(&options),
        "run" => run(&options),
        "loop" => daemon(&options),
        "status" => run_status(&options),
        other => {
            eprintln!("{}", usage(other));
            std::process::exit(2);
        }
    };
    // A resident OCR child is still talking on its own thread when this function ends, and the channel
    // library does not survive the C runtime pulling the ground out from under it: the process faults on
    // the way out, with a status code that says nothing about the recording that just succeeded. So the
    // engine is put down here, on every path, before the exit.
    wind_base::wxocr::shutdown();
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// The help text, with the reason it was asked for in front.
///
/// Returns its text rather than printing it, as `windmaint` and `windsetup` both do, so that the
/// test below can assert the screen names every flag the binary answers -- a `usage()` that only
/// writes to stderr is a screen nobody can check.
fn usage(command: &str) -> String {
    format!(
        "unknown command '{command}'\n\
         \n\
         usage:\n\
         \x20 windrec doctor  [--root PATH]                self-check the install — including whether\n\
         \x20                   the OCR engine is present and actually runnable — and time each stage\n\
         \x20 windrec run     [--root PATH] [--seconds N] [--interval-seconds N] [--no-ocr]\n\
         \x20                   [--gate-only]\n\
         \x20                   record for N seconds (default record_seconds) and then exit, starting\n\
         \x20                   a new segment every record_seconds while frames are being kept — an\n\
         \x20                   unchanged screen rotates nothing, because there is nothing to close\n\
         \x20 windrec loop    [--root PATH] [--no-ocr] [--interval-seconds N]\n\
         \x20                   record forever, starting a new segment every record_seconds while\n\
         \x20                   frames are being kept, and holding\n\
         \x20                   the record lock; Ctrl-C closes the current segment and exits\n\
         \x20 windrec status  [--root PATH]\n\
         \x20                   report whether a recorder is running, what is indexed, and what\n\
         \x20                   slices are waiting for the maintenance pass\n\
         \x20 windrec --version | -V\n\
         \x20                   print this binary's name, its package version and its build\n\
         \x20                   profile, without reading a config or an install root\n\
         \n\
         flags:\n\
         \x20 --seconds N            wall-clock budget for `run`, in seconds. A budget longer than\n\
         \x20                        record_seconds buys more segments, not an earlier stop.\n\
         \x20 --interval-seconds N   whole seconds between grabs, for this run only (default\n\
         \x20                        screenshot_interval_second). SECONDS, not milliseconds; a\n\
         \x20                        fraction is refused rather than rounded down. The older spelling\n\
         \x20                        --interval is accepted and means exactly the same thing.\n\
         \x20 --no-ocr               never invoke the OCR engine. Frames are still kept and still\n\
         \x20                        indexed, on their window title alone.\n\
         \x20 --gate-only            grab and gate, then stop: no OCR, no frames, no rows. Prices the\n\
         \x20                        capture path; it validates nothing about the data.\n\
         \n\
         --root defaults to the install directory: the folder carrying config_src/ (or, on an\n\
         \x20                  install that has not moved its data up, windrecorder/config_src/),\n\
         \x20                  found by walking up from this executable — so a binary run out of\n\
         \x20                  bin\\ settles on the install, not on bin\\ itself."
    )
}

/// What `windrec --version` prints. The format is `wind_base::version`'s, shared by all eleven
/// binaries; the name and `env!` here are what make it *this* crate's answer.
fn version_line() -> String {
    version::line("windrec", env!("CARGO_PKG_VERSION"))
}

fn parse_options(command: &str, args: &[String]) -> Result<Options, String> {
    // Held as `Option` until the parse is over, because `install_root` has to be able to tell "the
    // user said where" from "nobody said": assigned into a default mid-loop the two are the same
    // value, and `--root` would then be a hint the walk-up could overrule.
    let mut root = None;
    let mut seconds = 0i64;
    let mut interval_seconds = None;
    let mut mode = recorder::Mode::Full;

    let mut i = 0;
    while i < args.len() {
        let (key, inline) = match args[i].split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (args[i].as_str(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{what} needs a value"))
        };
        match key {
            "--root" => root = Some(PathBuf::from(value("--root")?)),
            "--seconds" => {
                seconds = value("--seconds")?.parse().map_err(|e| format!("--seconds: {e}"))?;
                if seconds < 0 {
                    return Err(format!("--seconds is a budget of seconds and cannot be {seconds}"));
                }
            }
            // `--interval` is the older spelling of the same thing and stays accepted: it is seconds
            // too, and what was wrong about it was the silence, not the name alone.
            "--interval-seconds" | "--interval" => {
                interval_seconds = Some(whole_seconds(key, &value(key)?)?)
            }
            "--no-ocr" => mode = recorder::Mode::TitlesOnly,
            // Strictly weaker than `--no-ocr`, so it wins when both are given: a run that writes
            // nothing cannot accidentally index something the user meant not to record.
            "--gate-only" => mode = recorder::Mode::GateOnly,
            other => return Err(format!("unexpected argument '{other}' for {command}")),
        }
        i += 1;
    }
    Ok(Options { root: install_root(root), seconds, interval_seconds, mode })
}

/// The install root this binary records into: `--root` when it was given, otherwise the directory
/// carrying this install's shipped settings, found by walking up from this executable.
///
/// [`wind_base::install`] owns the rule, and every binary in the workspace asks it the same
/// question. `windrec` was one of the three that still hand-rolled a `windrecorder/`-exists walk,
/// and the failure was not cosmetic: the standalone payload this project is building has no
/// `windrecorder/` directory, so the walk found nothing and the recorder settled for the folder it
/// was launched from — `bin\` — and laid `bin\userdata\db\`, `bin\cache\` and `bin\cache_screenshot\`
/// inside its own install. That footage is invisible to `windcapctl`, `windmcp` and `windmaint`,
/// which had already moved to the shared rule and so read the real `userdata\` two levels up, and
/// the next upgrade unpacks straight over it. Measured on a standalone install before this change;
/// the directory listing is in the acceptance notes.
fn install_root(explicit: Option<PathBuf>) -> PathBuf {
    wind_base::install::resolve_root_from_exe(explicit)
}

/// Parse a count of whole seconds, refusing everything else by name.
///
/// The old code did `options.interval as i64` on an `f64` field, which is a silent double lie: it
/// read `--interval 0.4` as one second through the `max(1)` clamp, and it left every reader of the
/// flag's name to guess the unit. A soak brief written by a competent author guessed milliseconds.
fn whole_seconds(flag: &str, raw: &str) -> Result<i64, String> {
    if let Ok(seconds) = raw.parse::<i64>() {
        if seconds < 1 {
            return Err(format!("{flag} counts seconds between grabs and must be at least 1, not {seconds}"));
        }
        return Ok(seconds);
    }
    if raw.parse::<f64>().is_ok() {
        return Err(format!(
            "{flag} counts whole SECONDS — not milliseconds, not a fraction — and {raw:?} would be \
             rounded down to {rounded}, so it is refused. The smallest legal value is 1 second.",
            rounded = raw.parse::<f64>().unwrap_or(0.0) as i64
        ));
    }
    Err(format!("{flag} wants a whole number of seconds, not {raw:?}"))
}

/// What `run` is about to do, in the four values the recorder actually consumes.
fn session(options: &Options, record_seconds: i64) -> Session {
    Session {
        mode: options.mode,
        seconds: if options.seconds > 0 { options.seconds } else { record_seconds.max(1) },
        // A `run` used to open with `rotate: false`, which made the first `is_full` segment boundary
        // stop the process: `--seconds 1080` on a machine with `record_seconds = 180` gave up at
        // 242 s. Honouring a duration means rotating through it.
        rotate: true,
        interval_seconds: options.interval_seconds,
    }
}

/// Report what the pipeline will actually do on this machine, and how long each stage takes.
///
/// The OCR engine gets a verdict, not a tick: a missing engine used to surface only as a line
/// repeated per frame into a stderr file the supervisor truncates on every start, which is how a
/// user found out their recorder had captured nothing for a week. This is the one line they get
/// instead, and `run`/`loop` restate it at session start.
fn doctor(options: &Options) -> Result<(), String> {
    let config = Config::load(&options.root).map_err(|e| e.to_string())?;
    make_thread_dpi_aware();
    let desktop = virtual_desktop();

    println!("root            {}", options.root.display());
    println!(
        "display layout  {}x{} at ({},{})  {:.1} MP union",
        desktop.width,
        desktop.height,
        desktop.x,
        desktop.y,
        f64::from(desktop.width * desktop.height) / 1e6
    );
    println!("foreground      {:?}", foreground_rect().map(|r| (r.width, r.height)));

    let session = winstate::snapshot();
    println!(
        "session         {:?} desktop={:?} idle={:?}s recordable={}",
        session.status, session.desktop_name, session.idle_seconds, session.recordable()
    );

    // The capture cost is a function of the source rectangle, so a report that names the strategy
    // without naming the panels it can choose from tells the user nothing about what to change.
    let monitors = windcap::capture::monitors();
    println!("displays        {} attached", monitors.len());
    for monitor in &monitors {
        println!(
            "  {} {}x{} at ({},{})  {:.1} MP{}",
            monitor.index,
            monitor.width,
            monitor.height,
            monitor.x,
            monitor.y,
            monitor.megapixels(),
            if monitor.primary { "  primary" } else { "" }
        );
    }
    println!(
        "capture source  {}",
        match config.str_or("multi_display_record_strategy", "all").as_str() {
            "single" => format!(
                "display {} -> {:?}",
                config.i64_or("record_single_display_index", 1),
                windcap::capture::monitor_rect(config.i64_or("record_single_display_index", 1) as i32)
                    .map(|r| (r.width, r.height))
            ),
            other => format!("{other} (whole virtual desktop)"),
        }
    );

    println!("db dir          {}", config.db_dir().display());
    println!("cache dir       {}", config.cache_screenshot_dir().display());

    // The privacy mask as this machine and this config combine to apply it, in rows and columns and not
    // only percentages: a user who set an edge has to be able to check their taskbar is inside it, and
    // this is the one place the number the recorder will paint is reported.
    for line in recorder::describe_crop(&config).lines() {
        println!("ocr mask        {line}");
    }

    // The engine, first and worst-case-only: everything below costs a few seconds of real OCR and is
    // pointless on a machine that has no engine to time.
    let engine = ocr::OcrEngine::from_config(&config, std::env::temp_dir());
    let probe = engines::probe(&config);
    let (verdict, impact) = engine_report(&probe);
    println!("ocr engine      {}", engine.name());
    println!("ocr program     {}", engine.program().display());
    println!("ocr invoke      {}", engine.describe());
    // Named separately from `ocr engine`, because the two differ exactly when the user needs to know.
    if let Some(note) = engine.note() {
        println!("ocr selected    {} — {note}", engine.requested());
    }
    println!("ocr status      {verdict}");
    println!("ocr impact      {impact}");

    // Time each stage so a regression in any one of them is visible without profiling tools.
    let mut grabber = Grabber::new(1920).map_err(|e| e.to_string())?;
    let mut gate = ChangeGate::new(GateConfig::default());
    let (mut grab_ms, mut gate_ms, mut ocr_ms, mut thumb_ms) = (0.0, 0.0, 0.0, 0.0);
    const SAMPLES: u32 = 5;

    for _ in 0..SAMPLES {
        let t = std::time::Instant::now();
        let frame = grabber.grab().map_err(|e| e.to_string())?.ok_or("topology changed")?;
        grab_ms += t.elapsed().as_secs_f64() * 1000.0;
        let (w, h) = (frame.width as usize, frame.height as usize);

        let t = std::time::Instant::now();
        gate.observe(frame.width, frame.height, &frame.luma);
        gate_ms += t.elapsed().as_secs_f64() * 1000.0;

        let t = std::time::Instant::now();
        let _ = thumb::thumbnail_base64(&frame.rgb, w, h, 70, 30);
        thumb_ms += t.elapsed().as_secs_f64() * 1000.0;

        if engine.is_installed() {
            let t = std::time::Instant::now();
            match engine.recognize(&frame.rgb, w, h) {
                Ok(text) => println!("ocr sample      {} chars", text.chars().count()),
                Err(e) => println!("ocr sample      {e}"),
            }
            ocr_ms += t.elapsed().as_secs_f64() * 1000.0;
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }

    let n = f64::from(SAMPLES);
    println!("\nper stage, mean of {SAMPLES}   (grab includes the single luma+RGB pass)");
    println!("  grab+convert  {:>8.2} ms", grab_ms / n);
    println!("  change gate   {:>8.2} ms", gate_ms / n);
    println!("  thumbnail     {:>8.2} ms", thumb_ms / n);
    if ocr_ms > 0.0 {
        println!("  ocr engine    {:>8.2} ms", ocr_ms / n);
    }
    println!("\npython baseline on this machine: session probe 8226 ms, ORB gate 189 ms, grab 57 ms/3.7MP");
    Ok(())
}

/// The configured engine's verdict as one line, and what it means for the user's data as another.
///
/// Built out of `windsetup`'s probe rather than a second prober: `windsetup engines` is the command
/// that already knows how to ask an OCR binary whether it can actually run — it invokes `-s`, the
/// tool's own language list, which only answers if the executable *and* its .NET host are intact.
/// Two probers drift, and the one that was written second is the one that lies.
///
/// Both lines come out of one function because they used to be able to disagree: a machine with the
/// configured language missing but another language installed would print MISSING next to "your
/// engine is fine".
fn engine_report(report: &engines::Report) -> (String, String) {
    let matching: Vec<&Probe> = report
        .probes
        .iter()
        .filter(|probe| probe.engine == report.configured_engine)
        .collect();
    // The row that speaks for this install is the one for the language the config asks for; a
    // different language working is not this run's answer.
    let chosen = matching
        .iter()
        .find(|probe| probe.language == report.configured_language)
        .or_else(|| matching.iter().find(|probe| probe.status == Status::Available))
        .or_else(|| matching.first())
        .copied();
    let Some(probe) = chosen else {
        return (
            format!("NOT PROBED — windsetup has no probe for {:?}, which is not a configured OCR engine", report.configured_engine),
            "cannot tell whether text will be indexed; run `windsetup engines`".to_string(),
        );
    };
    let language = if probe.language == "-" { String::new() } else { format!(" [{}]", probe.language) };
    let head = format!("{}{}", probe.status.ascii(), language);
    if probe.status == Status::Available {
        let measured = match (probe.accuracy, probe.elapsed_ms) {
            (Some(accuracy), Some(ms)) => format!("{accuracy:.1}% of the fixture in {ms} ms"),
            (Some(accuracy), None) => format!("{accuracy:.1}% of the fixture"),
            _ => "the shipped fixture".to_string(),
        };
        return (
            format!("{head} — read {measured}; {detail}", detail = probe.detail),
            "every frame is indexed by its on-screen text and by its window title".to_string(),
        );
    }
    (
        format!("{head} — {detail}", detail = probe.detail),
        "the recorder still keeps and still indexes every changed frame, but on its window title \
         ALONE: a search for anything you actually read on screen will find nothing. Run \
         `windsetup engines` for the benchmark, and remember the engine is a loose .exe the payload \
         has to carry — an update or an antivirus quarantine costs the text, not the footage."
            .to_string(),
    )
}


/// Read-only report; see the `status` module.
fn run_status(options: &Options) -> Result<(), String> {
    let config = Config::load(&options.root).map_err(|e| e.to_string())?;
    status::report(&config)
}

/// Record for `--seconds` and stop. The unit a scheduler or a test drives.
///
/// The budget is a duration, not a ceiling: a `--seconds` longer than one segment rotates through
/// as many segments as it needs. It used to open the recorder with rotation switched off, so the
/// first `is_full` boundary closed the segment *and* asked the process to stop — `--seconds 1080`
/// on a `record_seconds = 180` machine gave up at 242 s, which is a genuine surprise for anything
/// meant to run as a daemon.
fn run(options: &Options) -> Result<(), String> {
    let config = Config::load(&options.root).map_err(|e| e.to_string())?;
    let session = session(options, config.i64_or("record_seconds", 900));
    let mut recorder = recorder::Recorder::open(config, session.mode, session.rotate)?;
    if let Some(interval) = session.interval_seconds {
        recorder.override_interval(interval);
    }
    install_console_stop_handler();
    eprintln!(
        "recording {budget}s into {root} ({mode})",
        budget = session.seconds,
        root = recorder.slice_root().display(),
        mode = describe(session.mode),
    );
    recorder.drive(Some(std::time::Instant::now() + std::time::Duration::from_secs(session.seconds.max(1) as u64)))?;
    report(recorder.stats(), recorder.mode());
    Ok(())
}

/// The replacement for `record_screen.py`: rotate segments forever until the user stops it.
///
/// A single-instance lock is what makes this safe to leave running, and the reason the lock is
/// checked before any file is opened is that a second recorder does not fail — it doubles the
/// indexed rows for the same screen and makes the user's search results noisy.
fn daemon(options: &Options) -> Result<(), String> {
    let config = Config::load(&options.root).map_err(|e| e.to_string())?;
    let lock_path = config.record_lock_path();
    let _lock = match wind_base::fslock::PidLock::acquire(&lock_path) {
        Ok(lock) => lock,
        Err(e) => {
            return Err(format!("refusing to start a second recorder: {e}"));
        }
    };
    let session = session(options, config.i64_or("record_seconds", 900));
    let mut recorder = recorder::Recorder::open(config, session.mode, true)?;
    if let Some(interval) = session.interval_seconds {
        recorder.override_interval(interval);
    }
    install_console_stop_handler();
    eprintln!("recording continuously ({}); Ctrl-C closes the current segment and exits", describe(session.mode));
    recorder.drive(None)?;
    report(recorder.stats(), recorder.mode());
    Ok(())
}

/// The mode in the user's words, for the line that says what a run is about to do.
fn describe(mode: recorder::Mode) -> &'static str {
    match mode {
        recorder::Mode::Full => "ocr on, frames and rows written",
        recorder::Mode::TitlesOnly => "ocr off (--no-ocr), frames and rows written on window title alone",
        recorder::Mode::GateOnly => "gate-only: no ocr, no frames, no rows",
    }
}

fn report(stats: recorder::Stats, mode: recorder::Mode) {
    println!(
        "
segments {} closed, {} rows committed, {} collapsed as repeats",
        stats.segments_closed, stats.rows_committed, stats.collapsed
    );
    println!(
        "kept {} frames | skipped {} unchanged, {} repeat-text, {} empty, {} excluded, {} early, {} session, {} paused ({} resumed on input)",
        stats.kept,
        stats.dropped_unchanged,
        stats.dropped_repeat,
        stats.dropped_empty,
        stats.dropped_excluded,
        stats.dropped_early,
        stats.skipped_session,
        stats.paused_ticks,
        stats.resumed_from_pause
    );
    if stats.gated > 0 {
        println!(
            "{} ticks passed the change gate without a frame being kept — gate-only writes no frames \
             and no rows, so this run validates capture cost and nothing about your data",
            stats.gated
        );
    }
    if stats.ocr_unavailable > 0 {
        let (what, kept) = match mode {
            // A deliberate `--no-ocr` run is not a failure; it was told not to ask.
            recorder::Mode::TitlesOnly => (
                "frames indexed with OCR switched off (--no-ocr)",
                "on their window title alone",
            ),
            _ => ("frames the OCR engine could not read", "on their window title alone"),
        };
        println!(
            "{} {what}: every one of them was still kept and indexed {kept}, and text on screen was \
             not. Run `windrec doctor` to see why the engine is unusable.",
            stats.ocr_unavailable
        );
    }
    if let Some(line) = recorder::masked_line(stats, mode) {
        println!("{line}");
    }
    if stats.journal_failures > 0 {
        println!(
            "{} journal writes failed — a hard kill now costs those frames instead of the whole segment",
            stats.journal_failures
        );
    }
    if stats.swept_segments > 0 {
        println!(
            "recovered {} stranded segment(s) from a previous instance at startup, {} rows",
            stats.swept_segments, stats.swept_rows
        );
    }
    if stats.failures > 0 {
        println!(
            "{} failures — frames actually lost (the grab failed or the JPEG could not be written); \
             see the lines above",
            stats.failures
        );
    }
    if stats.grabber_rebuilds > 0 {
        println!("{} grabber rebuilds (display or foreground-window changes)", stats.grabber_rebuilds);
    }
    if stats.grabs > 0 {
        println!(
            "{} grabs, mean {:.1} ms / best {:.1} ms (widest source {:.1} MP)",
            stats.grabs,
            stats.grab_millis / f64::from(stats.grabs),
            stats.fastest_grab_millis,
            stats.widest_source_mp
        );
    }
}

/// Ctrl-C must not kill the process mid-transaction.
///
/// The default handler terminates the process wherever it happens to be, which — given that a
/// segment's rows are committed at its close — loses everything captured since the last rotation.
/// A console control handler that only flips a flag lets the loop finish the segment it is in, which
/// is the difference between "lost 14 minutes" and "lost nothing" after a reboot-free stop.
fn install_console_stop_handler() {
    unsafe extern "system" fn handler(control: u32) -> i32 {
        // CTRL_C_EVENT == 1, CTRL_CLOSE_EVENT == 2, CTRL_LOGOFF_EVENT == 5, CTRL_SHUTDOWN_EVENT == 6.
        if matches!(control, 1 | 2 | 5 | 6) {
            recorder::request_stop();
            return 1;
        }
        0
    }
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line a user pastes into a bug report. It has to name the binary they typed, carry this
    /// crate's version, and distinguish the profile -- `release.ps1` warns that a debug build is
    /// "roughly an order of magnitude slower", so `(debug)` in that line is load-bearing.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windrec "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        assert_eq!(line.lines().count(), 1, "one line: {line}");
    }

    /// Both spellings, and only the command slot -- `windrec doctor --version` is a doctor run with
    /// an argument the recorder does not have, and it is refused as one.
    #[test]
    fn version_is_read_where_a_subcommand_would_be() {
        assert!(version::is_flag("--version"));
        assert!(version::is_flag("-V"));
        assert!(!version::is_flag("doctor"));
        let usage = usage("nope");
        assert!(usage.contains("--version"), "{usage}");
        assert!(usage.contains("-V"), "{usage}");
    }

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    /// Where the recorder puts a frame is the one decision with no undo, and this binary used to make
    /// it with its own hand-rolled `windrecorder/` walk while the other seven had already moved to
    /// [`wind_base::install`]. Measured on a standalone payload — `bin\`, `config_src\`, `ocr_lib\`,
    /// and no `windrecorder\` anywhere — the walk found nothing and returned the folder holding the
    /// exe, so a root-less run built `bin\userdata\db\`, `bin\cache\` and `bin\cache_screenshot\`
    /// inside the install itself: footage no reader would ever find, and state the next upgrade
    /// unpacks over.
    #[test]
    fn the_root_comes_from_the_shared_install_rule_and_never_settles_in_bin() {
        let scratch = std::env::temp_dir().join(format!("windrec-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let payload = scratch.join("payload");
        std::fs::create_dir_all(payload.join("config_src")).unwrap();
        std::fs::write(payload.join("config_src").join("config_default.json"), "{}").unwrap();
        std::fs::create_dir_all(payload.join("bin")).unwrap();
        assert!(!payload.join("windrecorder").exists(), "the standalone case carries no Python package");

        // The rule, through the recorder's own entry point: a `bin\` one level under a payload root
        // resolves to that root, and never to `bin\`.
        assert_eq!(wind_base::install::resolve_root(None, &payload.join("bin")), payload);
        // …and a told root is still told, in both spellings, over an auto-detected one.
        assert_eq!(install_root(Some(PathBuf::from("E:/chosen"))), PathBuf::from("E:/chosen"));
        for spelling in [args("--root E:/chosen"), args("--root=E:/chosen")] {
            assert_eq!(parse_options("run", &spelling).expect("accepted").root, PathBuf::from("E:/chosen"));
        }

        // Run from this development tree, the parse must land above the folder holding the test
        // binary — which is exactly where the old walk gave up.
        let options = parse_options("run", &args("")).expect("a bare run has a root");
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        assert!(wind_base::install::is_install_root(&options.root), "{:?} is not an install root", options.root);
        assert!(exe_dir.starts_with(&options.root), "{exe_dir:?} is not inside the resolved root {:?}", options.root);
        assert!(!options.root.ends_with("bin"), "the recorder resolved its own program directory: {:?}", options.root);
        std::fs::remove_dir_all(scratch).unwrap();
    }

    /// `--gate-only` used to be parsed into a field, printed back to the user, and never read again:
    /// the real mode came only from `--no-ocr`. Asserting that it *parses* is worthless, because
    /// parsing was never what broke. These go as far as the four values `run` hands to the recorder
    /// and to the deadline it hands to `drive`, which is the whole of what a flag can reach.
    #[test]
    fn gate_only_reaches_the_recorder_as_a_mode_and_not_as_a_message() {
        let options = parse_options("run", &args("--gate-only")).expect("accepted");
        assert_eq!(options.mode, recorder::Mode::GateOnly);
        let plan = session(&options, 900);
        assert_eq!(plan.mode, recorder::Mode::GateOnly, "the flag must be what open() is given");
        // And the mode means what the help says it means: the recorder is opened with the mode, and
        // `describe` is the line the user sees before the first grab.
        assert!(describe(plan.mode).contains("no frames, no rows"), "{}", describe(plan.mode));
    }

    /// The two flags are now two different promises. They used to be one boolean with two names, and
    /// the collapsed form is what left `--gate-only` with nothing to do.
    #[test]
    fn no_ocr_keeps_the_row_where_gate_only_would_have_thrown_it_away() {
        let unread = parse_options("run", &args("--no-ocr")).expect("accepted");
        let plan = session(&unread, 900);
        assert_eq!(plan.mode, recorder::Mode::TitlesOnly);
        assert_ne!(plan.mode, recorder::Mode::GateOnly, "--no-ocr writes frames and rows");
        // Whichever is asked for last is the stricter one, so a confused command line cannot record
        // something the user meant not to record.
        let both = parse_options("run", &args("--no-ocr --gate-only")).expect("accepted");
        assert_eq!(session(&both, 900).mode, recorder::Mode::GateOnly);
        let neither = parse_options("run", &args("--seconds 60")).expect("accepted");
        assert_eq!(session(&neither, 900).mode, recorder::Mode::Full);
    }

    /// `run --seconds N` is a duration. It used to open the recorder with rotation switched off, so
    /// the first `is_full` segment boundary closed the segment *and* asked the process to stop: a
    /// soak of `--seconds 1080` on a `record_seconds = 180` machine self-terminated at 242 s.
    #[test]
    fn a_seconds_budget_longer_than_one_segment_rotates_instead_of_ending_early() {
        let options = parse_options("run", &args("--seconds 1080")).expect("accepted");
        let plan = session(&options, 180);
        assert_eq!(plan.seconds, 1080, "the budget is the number the user typed");
        assert!(plan.rotate, "reaching record_seconds must start a segment, not stop the run");
        // The default budget is still exactly one segment, which is what makes `run` a unit.
        let plain = parse_options("run", &args("")).expect("accepted");
        let plan = session(&plain, 900);
        assert_eq!((plan.seconds, plan.rotate), (900, true));
        // `--seconds 0` means "not given", and a negative budget is refused, not clamped.
        assert_eq!(session(&parse_options("run", &args("--seconds 0")).unwrap(), 900).seconds, 900);
        assert!(parse_options("run", &args("--seconds -5")).is_err());
    }

    /// The flag is seconds, and it always was: `wait()` sleeps `interval_seconds` as seconds. The
    /// name invited a milliseconds reading and the `as i64` cast then swallowed the difference —
    /// `--interval 0.4` silently became one second, `--interval 2000` would have meant 33 minutes.
    #[test]
    fn the_interval_is_whole_seconds_and_says_so_when_it_is_not() {
        let named = parse_options("run", &args("--interval-seconds 3")).expect("accepted");
        assert_eq!(session(&named, 900).interval_seconds, Some(3));
        // The older spelling keeps working, and means the same number of seconds.
        let older = parse_options("run", &args("--interval 3")).expect("accepted");
        assert_eq!(session(&older, 900).interval_seconds, Some(3));
        assert_eq!(older.interval_seconds, named.interval_seconds);
        // A fraction is refused by name instead of being rounded down to a different interval.
        for bad in ["0.4", "0.5", "2.9"] {
            let err = parse_options("run", &args(&format!("--interval-seconds {bad}"))).unwrap_err();
            assert!(err.contains("SECONDS"), "{bad}: {err}");
            assert!(err.contains(bad), "{bad}: the message has to quote the value it refused: {err}");
        }
        // So is a zero, which used to survive parsing and be clamped to 1 in the recorder.
        let zero = parse_options("run", &args("--interval-seconds 0")).unwrap_err();
        assert!(zero.contains("at least 1"), "{zero}");
        assert!(parse_options("run", &args("--interval-seconds nine")).is_err());
        // 2000 is legal and is 2000 *seconds*; the help text is what tells a reader that.
        assert_eq!(session(&parse_options("run", &args("--interval 2000")).unwrap(), 900).interval_seconds, Some(2000));
    }

    /// The help text is part of the flag. A reader who only reads `--help` must get the unit, the
    /// budget semantics and the fact that gate-only writes nothing. Each fragment below is asserted
    /// as it appears on one line, because the wording is allowed to wrap and the meaning is not.
    #[test]
    fn the_help_says_what_each_number_means() {
        let help = usage("run");
        assert!(help.contains("--interval-seconds"), "{help}");
        assert!(help.contains("whole seconds between grabs"), "{help}");
        assert!(help.contains("SECONDS, not milliseconds"), "{help}");
        assert!(help.contains("--interval is accepted and means exactly the same thing"), "{help}");
        assert!(help.contains("wall-clock budget"), "{help}");
        assert!(help.contains("record_seconds buys more segments"), "{help}");
        assert!(help.contains("no OCR, no frames, no rows"), "{help}");
        assert!(help.contains("it validates nothing about the data"), "{help}");
        assert!(help.contains("on their window title alone"), "{help}");
        // The two flags must not be described with the same words, or the help re-collapses what the
        // code just separated. Flattened first: the descriptions wrap across lines.
        let flat: String = help.split_whitespace().collect::<Vec<_>>().join(" ");
        let unread = section(&flat, "--no-ocr", "--gate-only");
        let gated = section(&flat, "--gate-only", "flags:");
        assert!(unread.contains("still kept") && !unread.contains("no frames"), "{unread}");
        assert!(gated.contains("no frames") && !gated.contains("still kept"), "{gated}");
    }

    /// The text of one flag's description: from the *last* time the flag is named — the usage lines
    /// name it too, and the description is the later one — up to the next flag or to `until`.
    fn section(haystack: &str, from: &str, until: &str) -> String {
        let Some(at) = haystack.rfind(from) else { return String::new() };
        let rest = &haystack[at + from.len()..];
        let ends = [until, "--no-ocr", "--gate-only", "--root defaults"]
            .into_iter()
            .filter(|needle| *needle != from)
            .filter_map(|needle| rest.find(needle))
            .min()
            .unwrap_or(rest.len());
        rest[..ends].trim().to_string()
    }

    /// `windrec doctor` is the one line that has to tell a user their engine is gone, with the path it
    /// looked at, before they spend a week finding out from empty search results.
    #[test]
    fn doctor_names_a_missing_engine_and_the_path_it_checked() {
        let path = r"C:\Scratch\ocr_lib\Windows.Media.Ocr.Cli.exe";
        let report = engines::Report {
            configured_engine: "Windows.Media.Ocr.Cli".into(),
            configured_language: "zh-Hans-CN".into(),
            probes: vec![Probe {
                engine: "Windows.Media.Ocr.Cli".into(),
                language: "-".into(),
                status: Status::Missing,
                detail: format!("{path} is not present in this install"),
                accuracy: None,
                elapsed_ms: None,
                fixture: None,
                sample: None,
            }],
        };
        let (verdict, impact) = engine_report(&report);
        assert!(verdict.starts_with("MISSING"), "{verdict}");
        assert!(verdict.contains(path), "the report has to name where it looked: {verdict}");
        // And say what it costs, because "missing" alone does not tell the user whether they have
        // lost the footage or only the text.
        assert!(impact.contains("window title"), "{impact}");
        assert!(impact.contains("still keeps"), "{impact}");
    }

    /// The other three verdicts, each distinguishable, so a user reading one line knows which of
    /// "absent", "present but broken" and "present and fine" they are holding.
    #[test]
    fn doctor_tells_apart_an_unusable_engine_from_a_healthy_one() {
        let probe = |status, language: &str, detail: &str| Probe {
            engine: "Windows.Media.Ocr.Cli".into(),
            language: language.into(),
            status,
            detail: detail.into(),
            accuracy: (status == Status::Available).then_some(91.5),
            elapsed_ms: (status == Status::Available).then_some(612),
            fixture: None,
            sample: None,
        };
        let report = |probes: Vec<Probe>| engines::Report {
            configured_engine: "Windows.Media.Ocr.Cli".into(),
            configured_language: "zh-Hans-CN".into(),
            probes,
        };
        let (failing, _) = engine_report(&report(vec![probe(Status::Failing, "-", "exists but cannot be run: .NET missing")]));
        assert!(failing.starts_with("FAILING"), "{failing}");
        assert!(failing.contains(".NET missing"), "{failing}");
        let (ready, ready_impact) =
            engine_report(&report(vec![probe(Status::Available, "zh-Hans-CN", "read the fixture")]));
        assert!(ready.starts_with("OK [zh-Hans-CN]"), "{ready}");
        assert!(ready.contains("91.5%"), "{ready}");
        assert!(!ready_impact.contains("ALONE"), "{ready_impact}");
        // The language the config actually asks for wins the line, even when another language works:
        // a machine with `en-US` installed and `zh-Hans-CN` configured is a machine that will not
        // index the user's text.
        let mixed = engine_report(&report(vec![
            probe(Status::Available, "en-US", "read the fixture"),
            probe(Status::Missing, "zh-Hans-CN", "no language pack: this machine offers en-US"),
        ]));
        assert!(mixed.0.starts_with("MISSING [zh-Hans-CN]"), "{}", mixed.0);
        let nothing = engine_report(&report(vec![]));
        assert!(nothing.0.contains("NOT PROBED"), "{}", nothing.0);
    }
}
