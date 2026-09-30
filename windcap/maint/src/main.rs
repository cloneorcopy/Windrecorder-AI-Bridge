//! `windmaint` — the idle maintenance pass, as a command rather than a thread.
//!
//! The Python app folded all of this into `record_screen.py::idle_maintain_process_main`, which only
//! ever ran from inside the recorder after the screen had sat idle for forty minutes: unobservable,
//! uncappable and untestable. Each of its jobs is a subcommand here, so a user can ask for one, time it,
//! and run it on an install whose recorder is not switched on. `windrec run` leaves JPEG frames in
//! `cache_screenshot/{stamp}/` and index rows pointing at a `{stamp}.mp4` that does not exist yet; this
//! binary is what turns those frames into that video and keeps the index honest afterwards.
//!
//! Convert and expire rename and delete files, and reindex OCRs the whole library, so they take the
//! maintain lock and cannot walk under a live recorder; the read-only `doctor` takes nothing. The AI
//! month tagger (`ai-tags`) reads the index and spends API quota, so it runs under the same lock and is
//! gated on `windai`'s own switches before it spawns anything. `--dry-run` is a genuine no-op on every
//! subcommand: the plan is computed and printed, and no file is created, renamed or deleted — which is
//! also why a dry run does not need, and must not steal, the lock. `windrec` launches this whole pass
//! (`all --idle-granted-by <pid>`) when the screen has sat idle, which is the only time heavy work runs.

mod backup;
mod backlog;
mod convert;
mod doctor;
mod encode;
mod expire;
mod forget;
mod layout;
mod previews;
mod refresh;
mod schedule;
mod summaries;
mod text;

use std::path::PathBuf;

use wind_base::clock;
use wind_base::config::Config;
use wind_base::version;

use layout::MaintainLock;

/// What the caller asked for, after the argument list has been read.
#[derive(Debug)]
struct Options {
    command: Command,
    root: PathBuf,
    dry_run: bool,
    limit: Option<usize>,
    /// The period and word for `forget`, kept as typed until the config's day-start is known.
    forget: forget::Args,
    /// The pid of the idle recorder that launched this pass (`--idle-granted-by`), if any. The
    /// reindex and AI-tag steps use it to tell "the recorder that went idle and spawned me" from "some
    /// live capture I must not race" — see `schedule::recorder_guard`. A hand-run pass has `None` and so
    /// declines those two steps whenever a recorder holds the record lock.
    idle_granted_by: Option<u32>,
    /// Set when a person pressed the interface's "整理现在" button (`--manual`), which is what lets
    /// this pass ignore the closing of the maintenance window: the window is a promise about when
    /// *unattended* work may run, and this work was asked for out loud.
    manual: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Doctor,
    Text,
    Convert,
    Refresh,
    Expire,
    Forget,
    Reindex,
    Previews,
    AiTags,
    AiSummaries,
    Backup,
    /// What is waiting, counted without touching anything. Not a step: it takes no lock,
    /// writes nothing, and is never part of `all`.
    Backlog,
    All,
}

/// The mutating steps, in the order one pass runs them.
///
/// Order matters at the edges: `text` first, ahead of every step that renames, encodes or eventually
/// recycles a slice directory, because it is the one reading the words the recorder left out; then
/// convert, because a slice that has become a video is what
/// refresh then reports as existing; refresh before expire, because the existence flags are how
/// expire tells a kept file from a missing one; reindex after the index is honest, so a segment whose
/// video only just exists gets its rows; ai-tags after reindex, because the month tagger summarises
/// the very window titles reindex writes; ai-summaries after the tags, because both are network work
/// and the local index work should finish first; backup last, so the snapshot it takes is the one the pass
/// just produced rather than the state before it. Reindex and ai-tags run under this same pass's
/// maintain lock, which is what keeps the two of them — both of which touch the monthly index — from
/// ever racing each other or the four steps that were here first.
///
/// What this list no longer describes is *how many things happen at once*. The nine steps are still
/// nine, in this order, with the same names and the same `step N/9` the window paints; what changed is
/// that six of them now work on several items at a time instead of one after another — `convert` on as
/// many ffmpeg processes as [`wind_base::pool::Duty::Subprocess`] allows, `previews` on decode lanes,
/// `text`'s cache listing folded into one read, `refresh`/`backup` per month file, and `reindex` on
/// several `wind-reindex` processes each with its own OCR engine. A step's unit of parallelism is the
/// thing its own module says is independent; the pass's shape, its lock, its stop request and its
/// progress file are unchanged, because a pass that finishes is the only optimisation a user notices.
///
/// `forget` is not in this list, and adding it here would be the bug: it destroys what the user indexed
/// on a period only a human can name, so it exists solely as a command somebody typed.
const PIPELINE: [Command; 9] = [
    // First, ahead of `convert`: this is the step that reads the slice directories the recorder filled
    // without text, and every step after it is one that renames, encodes or eventually recycles them.
    Command::Text,
    Command::Convert,
    Command::Refresh,
    Command::Expire,
    Command::Reindex,
    // After reindex, so the rows this pass has just indexed are covered by the same idle window that
    // indexed them; before ai-tags, because a bigger preview costs the tagger nothing and a smaller one
    // gains it nothing.
    Command::Previews,
    Command::AiTags,
    // After the tags, because both read the index this pass just refreshed and only the summariser
    // makes network requests: a pass that is interrupted by the user coming back has already done the
    // cheap local work. Before backup, so the copies taken are the ones this pass produced.
    Command::AiSummaries,
    Command::Backup,
];

impl Command {
    fn parse(text: &str) -> Option<Command> {
        match text {
            "doctor" => Some(Command::Doctor),
            "text" => Some(Command::Text),
            "convert" => Some(Command::Convert),
            "refresh" => Some(Command::Refresh),
            "expire" => Some(Command::Expire),
            "forget" => Some(Command::Forget),
            "reindex" => Some(Command::Reindex),
            "previews" => Some(Command::Previews),
            "ai-tags" => Some(Command::AiTags),
            "ai-summaries" => Some(Command::AiSummaries),
            "backup" => Some(Command::Backup),
            "all" => Some(Command::All),
            "backlog" => Some(Command::Backlog),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Command::Doctor => "doctor",
            Command::Text => "text",
            Command::Convert => "convert",
            Command::Refresh => "refresh",
            Command::Expire => "expire",
            Command::Forget => "forget",
            Command::Reindex => "reindex",
            Command::Previews => "previews",
            Command::AiTags => "ai-tags",
            Command::AiSummaries => "ai-summaries",
            Command::Backup => "backup",
            Command::All => "all",
            Command::Backlog => "backlog",
        }
    }

    /// Whether this command may change anything on disk, which is what the lock is for.
    ///
    /// Reindex writes index rows and renames videos; ai-tags reads the index and writes the result
    /// cache, and both touch the monthly database the other steps touch — so both take the maintain
    /// lock, standalone and inside `all`, exactly like the four they joined.
    fn exclusive(self) -> bool {
        matches!(
            self,
            Command::Text
                | Command::Convert
                | Command::Refresh
                | Command::Expire
                | Command::Forget
                | Command::Reindex
                | Command::Previews
                | Command::AiTags
                | Command::AiSummaries
                | Command::Backup
                | Command::All
        )
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Ahead of the subcommand test below and ahead of `Config::load`: `windmaint` is the binary
    // most often reached for when an install is already in trouble, and "which build is this" has
    // to survive a root that does not exist or a config that will not parse. Note that it is also
    // ahead of the maintain lock, which a version request must never take.
    if argv.first().map(String::as_str).is_some_and(version::is_flag) {
        println!("{}", version_line());
        return;
    }
    if !matches!(argv.first().map(String::as_str), Some(command) if Command::parse(command).is_some()) {
        println!("{}", usage(None));
        std::process::exit(if argv.is_empty() { 2 } else { 0 });
    }

    let options = match parse_options(&argv) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let config = match Config::load(&options.root) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = dispatch(&options, &config) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn dispatch(options: &Options, config: &Config) -> Result<(), String> {
    // Acquired once, for the whole pass. `all` runs four steps that rename and delete files; taking
    // and releasing the lock around each would leave a window in which a second pass could start
    // while this one is halfway through a rename.
    let _lock = match (options.command.exclusive(), options.dry_run) {
        (true, false) => Some(MaintainLock::acquire(&config.maintain_lock_dir())?),
        _ => None,
    };

    match options.command {
        Command::Backlog => {
            let body = backlog::report(&options.root, config)?;
            println!("{body}");
            Ok(())
        }
        Command::All => run_pipeline(options, config),
        // One command run by hand has no nine-step shape to overlap: no leg starts early, so no thread
        // spawns a child or sends a request that the command's own name does not account for.
        command => run_command(options, config, command, &mut Legs::standalone()),
    }
}

/// May this pass still be working, right now?
///
/// Two independent reasons to stop: somebody asked (`MAINTAIN_STOP.MD`), or the window that scheduled
/// this pass has closed. The second does not apply to a hand-requested pass — see [`Options::manual`]
/// — and does not apply at all when the install never named a window, which leaves the old idle rule
/// as the only thing that ever started one.
///
/// Asked between steps, and answered by the clock rather than by a signal: `windrec` owns a console,
/// so the tray's `AttachConsole`-and-break trick cannot be used against a pass it launched, and a pass
/// that stops itself between two work items has the same latency without the Win32 hazard.
fn may_still_work(options: &Options, config: &Config) -> Result<(), String> {
    may_still_work_for(options.manual, config)
}

/// The same two reasons to stop, from the two values a leg on another thread can own.
///
/// One function rather than a copy per thread, because a lane that answers "may I go on" by a different
/// rule than the pass that started it is a leg that keeps working after the window closed — and a second
/// stop channel is the thing this project has already paid for once. The `may_continue` latch underneath
/// it is process-wide, which is exactly what makes one 停止整理 request reach every lane.
fn may_still_work_for(manual: bool, config: &Config) -> Result<(), String> {
    if config.maintain_stop_requested() {
        return Err("stopped by request".to_string());
    }
    if manual {
        return Ok(());
    }
    match config.maintain_window() {
        Some(window) if !window.contains(wind_base::clock::now().minute_of_day()) => {
            Err(format!("the maintenance window {} has closed", window.label()))
        }
        _ => Ok(()),
    }
}

/// Every mutating step, under the one lock `dispatch` took.
///
/// A step that fails does not stop the rest: an idle window is scarce, and refusing to back up or to
/// correct the index because ffmpeg could not encode one corrupt slice would turn a partial success
/// into no result at all. The failures are collected and reported as the exit status instead.
///
/// What the *published* pass says is a different question from what the exit status says, and the ADR
/// answers it: a leg that only the endpoint is answering for is not the pass's failure
/// (`docs/adr/2026-09-30-the-organise-pass-runs-on-four-legs.md` 一). So each failure is reported to its
/// own leg, the pass's state is decided by [`closing`] out of those, and a hand-run `windmaint all` still
/// exits nonzero for a step that failed — the person at the console asked for every step to work, and the
/// exit status is the only answer they get. Both statements are on disk for whoever reads the file.
fn run_pipeline(options: &Options, config: &Config) -> Result<(), String> {
    use wind_base::maintain::{Leg, LegStatus};

    let mut done = 0usize;
    // Every step that came back with an error, and the subset of those whose error is this machine's own
    // rather than an endpoint that said nothing. The two lists decide the pass's published state; the
    // first one alone decides the exit status, which answers a different question.
    let mut failed: Vec<&'static str> = Vec::new();
    let mut local: Vec<&'static str> = Vec::new();
    // The legs that start before their own step, and the lock + census whose denominators they report
    // against. A dry run gets none of them: with no thread there is nothing that could spawn a child, send
    // a request, write or rename anything, so the rehearsal claims nothing the pass then does — and its own
    // census stays the only walk it makes. One guard for both, in one place, because the test that asks
    // "`--dry-run` starts nothing" has to ask the same question the pass asked itself.
    if !options.dry_run {
        open_the_pass(options, config);
    }
    let mut legs = Legs::for_pass(options, config);
    for (index, step) in PIPELINE.iter().enumerate() {
        // Checked before the step, never during it: a step is allowed to finish the file it opened.
        if let Err(why) = may_still_work(options, config) {
            // Said *first*, before anything is waited for. Between this sentence and the pass's real ending
            // there is a lane coming down and, until this round, an encoder finishing a minute of video —
            // and a 停止整理 button whose only answer is silence for that long is the thing that makes a
            // person press it twice. It claims no numbers, because the numbers are still moving; `finish`
            // below replaces this sentence with the one the lanes came back and said.
            wind_base::maintain::stopping(&format!(
                "stop requested — putting the work in hand down (step {} of {})",
                index + 1,
                PIPELINE.len()
            ));
            // The latch is asked *before* the flag is cleared underneath it, and both legs are waited for
            // before the ending is published: a pass that said "stopped" while a `wind-reindex` it started
            // was still OCRing, or a `windai` still sending, would be a sentence the process table proves
            // false — and the orphan is the failure this product has already paid for once.
            legs.put_down(config);
            config.clear_maintain_stop();
            let left = wind_base::maintain::items_left();
            let note = stopped_sentence(done, &why, left);
            wind_base::maintain::finish(wind_base::maintain::State::Stopped, &note, wind_base::clock::now().naive_epoch_seconds());
            wind_base::maintain::uninstall();
            println!(
                "\npass stopped early after {done} of {} steps: {why}\n  {left} item(s) still waiting for the next pass",
                PIPELINE.len()
            );
            return Ok(());
        }
        wind_base::maintain::begin_step(step.name(), index + 1, PIPELINE.len(), wind_base::clock::now().naive_epoch_seconds());
        println!("== {step} ==", step = step.name());
        if let Err(e) = run_command(options, config, *step, &mut legs) {
            eprintln!("   {step} failed: {e}", step = step.name());
            failed.push(step.name());
            // The leg says what happened to it, in the row the person reads: the red kind of trouble is
            // this machine's, the yellow kind belongs to an endpoint that did not answer.
            let status = if networks_on_the_endpoint(*step) { LegStatus::Offline } else { LegStatus::Failed };
            if status.fails_the_pass() {
                local.push(step.name());
            }
            if let Some(leg) = Leg::of_step(step.name()) {
                wind_base::maintain::report_leg(leg, status, &e);
            }
        }
        done += 1;
    }
    // Both legs are in by now — the convert step waited for one and the AI step for the other — and this
    // says so again rather than trusting it, because `items_left` is answered by the publisher and a leg
    // still working would move the number after the closing sentence was built from it.
    legs.put_down(config);
    // A request that arrived while the last step was running has been honoured by this pass
    // completing: leaving it on disk would stop the next window's pass before it began.
    config.clear_maintain_stop();
    let ending = closing(&failed, &local, PIPELINE.len(), wind_base::maintain::items_left());
    wind_base::maintain::finish(ending.state, &ending.note, wind_base::clock::now().naive_epoch_seconds());
    wind_base::maintain::uninstall();
    match ending.error {
        Some(why) => Err(why),
        None => {
            println!("\npass complete: {}", ending.note);
            Ok(())
        }
    }
}

/// The two legs that start before their own step, and the handles the pass puts them down with.
///
/// The ADR's 二 says the three things that cost time use three different resources — the recognition
/// engine, the encoder, the endpoint — and that queueing them one after another adds their waits together
/// instead of hiding them. So this pass starts two of them early and joins each at the boundary of the
/// step whose name it works under:
///
///   * **the AI leg's settled half** starts with the pass, on its own thread, and is waited for by
///     `ai-summaries` (step 8). Only days this pass can no longer change are asked there — see
///     [`schedule::settled_days`] for what makes a day one of those — so the current day and anything the
///     local steps are still writing keeps its place after them.
///   * **the back-index lane that follows `convert`** starts with the pass, waits at the encoder's
///     hand-off, and is waited for by `convert` (step 2) itself: its child writes rows and renames
///     videos, so `refresh` and `expire` must not be halfway through the same month files when it is
///     still going.
///
/// The nine steps are still the nine steps, in this order, with these names, for a standalone command and
/// for the published progress. What moved is *when* some of their work starts, and each step still says
/// one true line about the leg that got there before it.
///
/// [`Legs::standalone`] is what a command run by hand gets: no thread, no hand-off, no join. Nothing that
/// a single command's own name does not account for starts, and nothing at all starts on a dry run.
struct Legs {
    /// The AI leg's settled half, on its own thread. Taken by `ai-summaries`, or by [`Legs::put_down`].
    ai: Option<schedule::EarlyLeg>,
    /// The lane waiting at the encoder's hand-off. Taken by `convert`.
    follow: Option<schedule::Follow>,
    /// What that lane did, kept for the `reindex` step's one honest line about it.
    followed: Option<schedule::FollowReport>,
    /// The AI half after [`Legs::put_down`] joined it, kept for the step that closes the leg.
    early: Option<schedule::EarlyReport>,
}

impl Legs {
    /// Nothing starts early: the shape every standalone command runs under.
    fn standalone() -> Legs {
        Legs { ai: None, follow: None, followed: None, early: None }
    }

    /// Both legs, or none at all: the shape `run_pipeline` starts a pass with, and the only place in this
    /// file where a leg is begun.
    ///
    /// A dry run gets [`Legs::standalone`] — no thread exists, so nothing can spawn a child, send a
    /// request, write or rename a byte, and the rehearsal cannot claim work the pass then does. The line it
    /// says is the pass's own: a rehearsal that is silent looks like a pass that found nothing to do.
    fn for_pass(options: &Options, config: &Config) -> Legs {
        if options.dry_run {
            println!("== dry run: no leg starts early — nothing is spawned, sent, written or renamed ==");
            Legs::standalone()
        } else {
            Legs::start(options, config)
        }
    }

    /// Start both legs, at the moment the pass's four denominators were fixed.
    ///
    /// Each leg is handed one answer to "may this still be going?" — [`may_still_work_for`] plus the
    /// process-wide [`wind_base::maintain::may_continue`] latch — so a stop request or a closing window
    /// reaches the threads within about a second and puts their children down, without a second stop
    /// channel that a later reader would have to reconcile with the first.
    fn start(options: &Options, config: &Config) -> Legs {
        let manual = options.manual;
        let owned = config.clone();
        // One answer to "may this still be going?", shared by both legs and by no channel of their own: the
        // pass's two reasons to stop (a request, and the closing of the window that scheduled it) and the
        // process-wide latch that every step is already watching.
        let may_ask: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new(move || {
            may_still_work_for(manual, &owned).is_ok() && wind_base::maintain::may_continue(&owned)
        });
        // The guards each leg shares with its own step — the recorder, the spend switch, the binary — are
        // asked inside the thread and *held*, so a decline is said at the step's own boundary and not in the
        // middle of an unrelated one.
        let ai = schedule::EarlyLeg::start(config, &options.root, options.idle_granted_by, std::sync::Arc::clone(&may_ask));
        let follow = schedule::Follow::start(config, &options.root, options.idle_granted_by, may_ask);
        Legs { ai: Some(ai), follow: Some(follow), followed: None, early: None }
    }

    /// Stop both legs and wait for them, so a pass that is ending — early or on its feet — leaves no
    /// child behind. Idempotent: the step that owns a leg takes it first, and this is the backstop for
    /// the boundaries it never reached.
    fn put_down(&mut self, config: &Config) {
        // Asked for its side effect: the latch is what every lane's own loop is watching, and a stop
        // request that is about to be cleared from disk has to be latched before it is.
        let _ = wind_base::maintain::may_continue(config);
        if let Some(follow) = self.follow.take() {
            self.followed = Some(follow.finish());
        }
        if let Some(leg) = self.ai.take() {
            // The half's writes are already in the leg's counter; its owed stretches are kept so the step
            // that closes the leg can still fold them into the pass's one retry. A pass that stops before
            // `ai-summaries` never runs that step, and what was not answered stays in the queue — which is
            // how the queue has always been rebuilt: derived from the disk, not from a cursor.
            self.early = Some(leg.join());
        }
    }
}

/// Count the four queues, then say the pass has begun — with its denominators already fixed.
///
/// The census comes first, deliberately: it is the steps run in dry run, and a step reports the items it
/// handles through the same publisher the pass uses. Installed before the walk, the pass would publish its
/// own rehearsal as work — the text step's dry run handing the 文字识别 row a picture it never read, the
/// 视频合成 row a slice it never encoded, before `== text ==` had even been printed. With nothing installed
/// the walk is silent, which is the rule this file already answers to: no publisher, no progress. The
/// maintain lock this pass took before `dispatch` is what tells a reader a pass is underway during the
/// walk, and a pass that dies inside the census leaves the previous ending on disk rather than a `running`
/// file about work that never started.
///
/// A census that could not answer costs the pass its four rows and nothing else: the nine steps still run,
/// still honour a stop, and still publish the step they are in — the same rule that lets a failed write of
/// the progress file cost the bar rather than the run.
fn open_the_pass(options: &Options, config: &Config) {
    // Named in the log, because the walk prints the steps' own dry-run prose and a reader who finds an
    // `expire:` block before `== expire ==` should know they are looking at a rehearsal, not a run.
    println!("== backlog census (dry run: nothing written) ==");
    let counted = backlog::census(&options.root, config);
    wind_base::maintain::install(
        &config.maintain_progress_path(),
        if options.manual { wind_base::maintain::Kind::Manual } else { wind_base::maintain::Kind::Scheduled },
        wind_base::clock::now().naive_epoch_seconds(),
    );
    match counted {
        Ok(counted) => {
            let (promise, deferred) = promised(config, counted.totals());
            wind_base::maintain::set_totals(promise);
            if deferred > 0 {
                // Said on the row it belongs to, and in the row's own state: the pass is not working that
                // queue yet, and the number the bar will reach is the ceiling, not the backlog.
                wind_base::maintain::report_leg(
                    wind_base::maintain::Leg::Ai,
                    wind_base::maintain::LegStatus::Waiting,
                    &format!("{deferred} stretch(es) are outside one pass's ceiling; they wait for a later pass"),
                );
            }
        }
        Err(why) => eprintln!("   note: the backlog census failed ({why}); the pass runs without four totals"),
    }
}

/// What one pass may promise its four bars, and what it therefore leaves out.
///
/// The census answers "what is waiting" — the number the settings panel and the 数一数 button show. A
/// pass can only promise what its own ceilings allow, and the AI leg is the one place the two differ: the
/// backlog is every pending stretch in fourteen days, while `summary_stretch_limit_in_idle` lets one pass
/// ask forty. A bar whose denominator no pass can ever reach is read as a stall, which is the lie the ADR
/// names, so the denominator is the ceiling and the remainder is said out loud on the same row rather than
/// left as a gap nobody explains.
fn promised(config: &Config, counted: wind_base::maintain::Totals) -> (wind_base::maintain::Totals, usize) {
    let ceiling = schedule::summary_budget(config).1;
    if counted.ai <= ceiling {
        return (counted, 0);
    }
    (wind_base::maintain::Totals { ai: ceiling, ..counted }, counted.ai - ceiling)
}

/// Is this step waiting on something that is not this machine?
///
/// The AI leg is the only one that reaches outside the install, and the ADR puts it in words: 网络类失败
/// 不算整理失败. A step of that leg coming back with nothing leaves the footage on disk, un-summarised and
/// offered again next pass — the outcome of a night with no idle window, not of a broken install.
fn networks_on_the_endpoint(step: Command) -> bool {
    matches!(step, Command::AiTags | Command::AiSummaries)
}

/// How the pass closes: its own word for the ending, the sentence for the person reading it, and — kept
/// apart on purpose — whether the exit status should say a step failed.
struct Ending {
    state: wind_base::maintain::State,
    note: String,
    error: Option<String>,
}

/// Decide the ending from the steps that reported trouble and which kind of trouble it was.
///
/// A pass whose only trouble came from the endpoint publishes `complete` with that endpoint named, because
/// the state is read as "did the pass do what it promised" and a leg that had nothing come back is the
/// ADR's yellow row, not a red one. `error` is still set for any failed step — the exit status answers a
/// different question (did every command succeed), and a hand-run `windmaint all` that quietly exited 0 on
/// a summariser that never answered would hide it from the only person who can fix the address.
fn closing(failed: &[&'static str], local: &[&'static str], steps: usize, left: usize) -> Ending {
    let owed = if left == 0 { String::new() } else { format!("; {left} item(s) still waiting for the next pass") };
    let quiet: Vec<&'static str> = failed.iter().filter(|name| !local.contains(name)).copied().collect();
    let error = if failed.is_empty() {
        None
    } else {
        Some(format!("{} of {steps} steps failed ({})", failed.len(), failed.join(", ")))
    };
    if !local.is_empty() {
        let mut note = format!("{} of {steps} steps failed ({})", local.len(), local.join(", "));
        if !quiet.is_empty() {
            note.push_str(&format!("; and {} had nothing come back ({})", quiet.len(), quiet.join(", ")));
        }
        note.push_str(&owed);
        return Ending { state: wind_base::maintain::State::Failed, note, error };
    }
    if !quiet.is_empty() {
        return Ending {
            state: wind_base::maintain::State::Complete,
            note: format!("{steps} steps ran, and {} had nothing come back ({})", quiet.len(), quiet.join(", ")) + &owed,
            error,
        };
    }
    Ending { state: wind_base::maintain::State::Complete, note: format!("{steps} steps, no failures{owed}"), error }
}

/// The closing sentence of a pass that was called off: what it had done, why it stopped, and how much is
/// still owed — the last of those being the reason 停止整理 can be pressed without losing the count.
fn stopped_sentence(done: usize, why: &str, left: usize) -> String {
    format!("stopped after {done} of {} steps: {why}; {left} item(s) still waiting for the next pass", PIPELINE.len())
}

fn run_command(options: &Options, config: &Config, command: Command, legs: &mut Legs) -> Result<(), String> {
    match command {
        // Routed by `dispatch` into `run_pipeline`; it is not a step of itself.
        Command::All => Ok(()),
        // The census is answered by `dispatch` too: it is not a step, and it must never be reached from
        // `all`, where a dry-run pass would otherwise count the same work twice.
        Command::Backlog => Ok(()),
        Command::Doctor => doctor::run(&options.root, options.limit),
        Command::Text => {
            let outcome = text::run(config, options.dry_run, options.limit)?;
            println!("{}", text::report(&outcome, options.dry_run));
            Ok(())
        }
        Command::Convert => {
            // A pass that also means to back-index hands the encoder's finished segments to the lane that
            // started with the pass, so the OCR engine reads the earlier hours while ffmpeg is still
            // encoding the later ones. A standalone `windmaint convert` has no lane — a step run by hand
            // must not silently start a second command's work — and a dry run has no thread at all.
            let follow = legs.follow.take();
            let handoff = follow.as_ref().map(|follow| follow.handoff());
            let outcome = match handoff.as_ref() {
                Some(handoff) => convert::run_followed(&options.root, config, options.dry_run, options.limit, Some(handoff))?,
                None => convert::run(&options.root, config, options.dry_run, options.limit)?,
            };
            if let Some(follow) = follow {
                // The encoder has stopped marking, so the lane takes no further batch. It is waited for
                // *here*, before `refresh` opens the month files: the lane's child renames videos and
                // commits rows, and a video caught mid-rename is exactly what `refresh`'s existence flags
                // and `expire`'s "kept or missing" judgement must not have to guess about.
                legs.followed = Some(follow.finish());
            }
            let line = legs.followed.as_ref().map(|report| report.line()).unwrap_or_default();
            if !line.is_empty() {
                println!("{line}");
            }
            println!(
                "\nconvert: {} slice(s) encoded, {} discarded, {} already had a video, {} failed, {} put down by the stop, {} frame(s) read{}",
                outcome.converted,
                outcome.discarded,
                outcome.already_present,
                outcome.failed,
                outcome.called_off,
                outcome.frames,
                if options.dry_run { ", dry run: nothing written" } else { "" }
            );
            if outcome.failed > 0 {
                return Err(format!("{} slice(s) could not be encoded; they are left unmarked for the next run", outcome.failed));
            }
            Ok(())
        }
        Command::Refresh => {
            let outcome = refresh::run(config, options.dry_run, options.limit)?;
            println!(
                "\nrefresh: {} month file(s), {} row(s) scanned, {} video name(s) and {} frame name(s) probed across {} dir list(s)",
                outcome.months, outcome.rows_scanned, outcome.video_names, outcome.picture_names, outcome.directories_listed
            );
            println!(
                "         {} flag correction(s), {} row(s) written, timestamp index {} {} of {} month(s)",
                outcome.flags_corrected,
                outcome.rows_written,
                if options.dry_run { "needed by" } else { "created in" },
                outcome.indexes,
                outcome.months
            );
            Ok(())
        }
        Command::Expire => {
            let outcome = expire::run(&options.root, config, options.dry_run, options.limit)?;
            println!(
                "\nexpire: {} month(s), {} row(s) examined, {} segment(s) expired ({} file(s), {} slice(s), {} row(s) deleted), {} re-compressed, {} encode failure(s), {} trash run folder(s) pruned ({} byte(s) reclaimed){}",
                outcome.months,
                outcome.rows_examined,
                outcome.segments_deleted,
                outcome.files_removed,
                outcome.slices_removed,
                outcome.rows_deleted,
                outcome.segments_compressed,
                outcome.compress_failures,
                outcome.trash_runs_pruned,
                outcome.trash_bytes_freed,
                if options.dry_run { ", dry run: nothing removed" } else { "" }
            );
            if outcome.compress_failures > 0 {
                return Err(format!("{} segment(s) could not be re-encoded; their sources are intact", outcome.compress_failures));
            }
            if outcome.summaries.entries > 0 || outcome.summaries.days_marked > 0 {
                // Only when it happened: an expire that deleted nothing must not read as though it had
                // reached somebody's prose.
                println!("         {}", outcome.summaries.report(options.dry_run));
            }
            Ok(())
        }
        Command::Forget => {
            // The window is resolved against *this* install's day-start: `--day 2026-09-22` on an
            // install that begins its product day at 03:00 is a different twelve hours than on one that
            // begins at midnight, and the same flag has to mean the same thing here that `windcapctl
            // query --day` means next door. `forget::run` prints its own report, including what it did
            // not reach.
            let window = options.forget.window(config.day_begin_minutes())?;
            forget::run(config, &window, options.forget.keyword(), options.dry_run).map(|_| ())
        }
        Command::Reindex => {
            if options.dry_run {
                println!("reindex: skipped (dry run: `wind-reindex` renames and writes the index)");
                return Ok(());
            }
            // Said before the walk rather than after it, so a reader can see that the number of segments
            // this step now works is smaller than the encoder's because a lane that started earlier has
            // already walked them — and that this step still walks the whole library, as it always did.
            if let Some(report) = legs.followed.as_ref() {
                if report.segments > 0 {
                    println!(
                        "reindex: {n} segment(s) were already walked by the lane that followed convert; walking the whole library for the rest",
                        n = report.segments
                    );
                }
            }
            schedule::reindex(config, &options.root, options.idle_granted_by)
        }
        Command::Previews => {
            let outcome = previews::run(config, options.dry_run, options.limit)?;
            println!(
                "\npreviews: {} month file(s), {} row(s), {} already wide enough, {} redrawn ({} from a retained screenshot, {} from the video), {} with neither on disk, {} failed{}",
                outcome.months,
                outcome.rows,
                outcome.already_wide_enough,
                outcome.written,
                outcome.from_screenshot,
                outcome.from_video,
                outcome.missing_source,
                outcome.failed,
                if options.dry_run { ", dry run: nothing written" } else { "" }
            );
            if options.dry_run {
                // The count above cannot say "would be redrawn" and be true at the same time, so the
                // dry-run line is printed apart rather than folded into the format string.
                println!("          {} row(s) would be redrawn on the next real run", outcome.planned);
            }
            if outcome.failed > 0 {
                return Err(format!("{} row(s) could not be redrawn; they keep the preview they had", outcome.failed));
            }
            Ok(())
        }
        Command::AiTags => {
            if options.dry_run {
                println!("ai-tags: skipped (dry run: `windai` makes network requests and spends API quota)");
                return Ok(());
            }
            schedule::ai_tags(config, &options.root, options.idle_granted_by)
        }
        Command::AiSummaries => {
            if options.dry_run {
                println!("ai-summaries: skipped (dry run: `windai` makes network requests and spends API quota)");
                return Ok(());
            }
            // The leg's other half started with the pass, on its own thread, and is waited for here: the
            // day the local steps were still writing is asked after them, and the pass's single in-window
            // retry then covers whichever half the endpoint last left hanging.
            let early = match legs.ai.take() {
                Some(leg) => leg.join(),
                // Already put down — by a stop this pass honoured before it reached this step, or by a pass
                // that is closing on its feet. Either way the half's own counts are still the step's to say.
                None => legs.early.take().unwrap_or_else(schedule::EarlyReport::none),
            };
            schedule::ai_summaries_after(config, &options.root, options.idle_granted_by, &early)
        }
        Command::Backup => {
            let outcome = backup::run(config, &clock::now(), options.dry_run, options.limit)?;
            println!(
                "\nbackup: {} month file(s), {} copied, {} pruned beyond the {} kept{}",
                outcome.months,
                outcome.written,
                outcome.pruned,
                backup::KEEP,
                if options.dry_run { ", dry run: nothing written" } else { "" }
            );
            println!(
                "        AI summaries: {} day file(s), {} copied, {} already identical to their newest copy, {} pruned beyond the {} kept per day{}",
                outcome.summaries.days,
                outcome.summaries.copied,
                outcome.summaries.unchanged,
                outcome.summaries.pruned,
                backup::KEEP,
                if options.dry_run { ", dry run: nothing written" } else { "" }
            );
            if outcome.months == 0 && outcome.summaries.days == 0 {
                println!(
                    "        nothing to back up: no month file under {} and no summary under {}",
                    config.db_dir().display(),
                    wind_summary::PERIOD_DIR
                );
            }
            Ok(())
        }
    }
}

/// The help text, with the reason it was asked for in front when that reason is an error.
fn usage(note: Option<&str>) -> String {
    let mut text = match note {
        Some(note) => format!("error: {note}\n\n"),
        None => String::new(),
    };
    text.push_str(&format!(
        "usage: windmaint <command> [--root PATH] [--dry-run] [--limit N] [--idle-granted-by PID] [--manual]\n\
         \n\
         \x20 --manual  a pass a person asked for, started now: it runs to the end of its steps and is\n\
         \x20           bounded by the stop request, not by the closing of the maintenance window\n\
         \n\
         \x20 doctor   [--root PATH] [--limit N]   report the install and time each discovery step\n\
         \x20 text     [--root PATH] [--dry-run]    read the text the recorder left out of its rows, from\n\
         \x20                                      the masked copy each frame was written with, and fold\n\
         \x20                                      away the rows that turned out to repeat\n\
         \x20 convert  [--root PATH] [--dry-run]    turn cached screenshot slices into their videos\n\
         \x20 refresh  [--root PATH] [--dry-run]    correct the index's existence flags, add the time index\n\
         \x20 expire   [--root PATH] [--dry-run]    apply the retention windows: delete, re-compress\n\
         \x20 forget   [--root PATH] --day YYYY-MM-DD | --from T --to T   blank the indexed text, window\n\
         \x20          [--keyword WORD] [--dry-run]  titles and previews for a period the user names,\n\
         \x20                                       without touching the footage; never part of `all`\n\
         \x20 reindex  [--root PATH]               run `wind-reindex` over the whole library so unsearchable\n\
         \x20                                       footage (and any failed-OCR segment) becomes searchable\n\
         \x20 previews [--root PATH] [--dry-run]   redraw each row's stored picture at the width a card\n\
         \x20                                      is drawn at: from the retained screenshot when the\n\
         \x20                                      slice is still on disk, otherwise from the video, and\n\
         \x20                                      never by running OCR over the library again\n\
         \x20 ai-tags  [--root PATH]               run `windai` to cache the recent months' AI tags, only\n\
         \x20                                       when enable_ai_extract_tag *and* enable_ai_extract_tag_in_idle\n\
         \x20                                       are on — otherwise no request is made at all\n\
         \x20 ai-summaries [--root PATH] [--dry-run]  send the stretches that have no summary yet to the\n\
         \x20                                       configured endpoint, and a day's whole set once the\n\
         \x20                                       day is covered; needs the same AI settings as ai-tags,\n\
         \x20                                       and declines when enable_ai_summary_in_idle is off\n\
         \x20 backup   [--root PATH] [--dry-run]    copy each month file and each AI summary day file,\n\
         \x20                                       keeping the newest {} of them\n\
         \x20 all      [--root PATH] [--dry-run]    convert, refresh, expire, reindex, previews, ai-tags,\n\
         \x20                                       ai-summaries and backup in one pass, under one\n\
         \x20                                       maintain lock (what `windrec` runs when idle, with\n\
         \x20                                       --idle-granted-by <its own pid>)\n\n\
         \x20 --version | -V                      print name, package version and build profile;\n\
         \x20                                     reads no config and takes no lock\n\
         --root defaults to the directory containing this executable.\n\
         convert, refresh, expire, forget, reindex, previews, ai-tags, ai-summaries and backup refuse to run\n\
         while another process holds the maintain lock. reindex, ai-tags and ai-summaries additionally\n\
         refuse to run while a recorder holds the record lock, unless that recorder is the one that\n\
         launched this pass (--idle-granted-by).",
        backup::KEEP
    ));
    text
}

/// What `windmaint --version` prints. The format is shared with the other ten binaries by
/// `wind_base::version`; the name and the `env!` are this crate's own.
fn version_line() -> String {
    version::line("windmaint", env!("CARGO_PKG_VERSION"))
}

/// Read one command's arguments. `--flag=value` and `--flag value` are both accepted, matching the
/// recorder's CLI so a user only has to learn one shape.
fn parse_one(args: &[String], command: Command) -> Result<Options, String> {
    let mut root = default_root();
    let mut dry_run = false;
    let mut limit = None;
    let mut idle_granted_by = None;
    let mut manual = false;
    let mut forget_args = forget::Args::default();
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
            "--root" => root = PathBuf::from(value("--root")?),
            "--dry-run" => dry_run = true,
            // Only `all` reads it, and only to decide whether the window applies. Accepted by every
            // command rather than refused by six of them, because the recorder passes one string
            // whether or not it happened to pick a subcommand, and an unknown flag there would mean
            // the button does nothing on the steps it is not about.
            "--manual" => manual = true,
            "--limit" => {
                let parsed: usize = value("--limit")?.parse().map_err(|e| format!("--limit: {e}"))?;
                if parsed == 0 {
                    return Err("--limit must be at least 1".to_string());
                }
                limit = Some(parsed);
            }
            // The idle recorder's own pid. It authorises the two idle-only steps (reindex, ai-tags) to
            // run while that recorder still holds the record lock, and nothing else. A bad value is a
            // hard parse error rather than a silent `None`, so an automated pass can never quietly lose
            // its authorisation and skip the work it was launched to do.
            "--idle-granted-by" => {
                let parsed: u32 = value("--idle-granted-by")?.parse().map_err(|e| format!("--idle-granted-by: {e}"))?;
                if parsed == 0 {
                    return Err("--idle-granted-by must be a real pid, not 0".to_string());
                }
                idle_granted_by = Some(parsed);
            }
            // The period `forget` erases. They belong to that command alone for the same reason
            // `--idle-granted-by` does: a flag accepted by a command that ignores it is a user believing
            // they bounded a destruction.
            "--day" | "--from" | "--to" | "--keyword" => {
                if command != Command::Forget {
                    return Err(format!("{key} belongs to forget, not {}", command.name()));
                }
                let text = value(key)?;
                match key {
                    "--day" => forget_args.day = Some(text),
                    "--from" => forget_args.from = Some(text),
                    "--to" => forget_args.to = Some(text),
                    _ => forget_args.keyword = Some(text),
                }
            }
            other => return Err(format!("unexpected argument '{other}' for {}", command.name())),
        }
        i += 1;
    }
    if !root.exists() {
        return Err(format!("--root {} does not exist", root.display()));
    }
    if command == Command::Forget && forget_args.is_empty() {
        return Err(usage(Some(
            "forget needs a period: --day YYYY-MM-DD, or --from and --to. It is the one command here \
             that destroys what the user indexed, so an omitted window is refused rather than read as \
             \"everything\"",
        )));
    }
    Ok(Options { command, root, dry_run, limit, forget: forget_args, idle_granted_by, manual })
}

fn parse_options(argv: &[String]) -> Result<Options, String> {
    let first = argv.first().ok_or_else(|| usage(None))?;
    let command = Command::parse(first).ok_or_else(|| usage(Some(&format!("unknown command '{first}'"))))?;
    parse_one(&argv[1..], command)
}

/// The install root: the directory carrying this install's shipped settings, found by walking up
/// from this executable.
///
/// [`wind_base::install`] owns the rule and every binary in the workspace goes through it — this
/// one used to hand-roll `windrecorder/`-exists, and so did six others, which is how "is this an
/// install" came to have seven answers. A `windmaint` that resolved a different root than the
/// `windrec` writing the slices would either find no work or convert a segment that is still open.
fn default_root() -> PathBuf {
    wind_base::install::resolve_root_from_exe(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_base::maintain::{Leg, LegStatus, State};

    /// The publisher is one process-wide slot, so the one test that installs it holds this while it runs.
    static PUBLISHER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn parse(args: &str) -> Result<Options, String> {
        parse_options(&args.split(' ').filter(|a| !a.is_empty()).map(str::to_string).collect::<Vec<_>>())
    }

    /// A scratch install that answers with `settings` and nothing else.
    fn install(tag: &str, settings: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windmaint-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), settings).unwrap();
        dir
    }

    /// A `--manual` pass is one a person asked for. The window that closes at 05:00 must not be the
    /// thing that decides whether the button they pressed at 04:58 does anything.
    #[test]
    fn a_hand_requested_pass_is_not_told_its_window_closed() {
        // Opens and closes at the same minute: a window that is never open, whatever the clock says.
        let dir = install("manual", r#"{"maintain_window_start": "00:00", "maintain_window_end": "00:00"}"#);
        let config = Config::load(&dir).unwrap();
        let scheduled = parse("all --root .").unwrap();
        let hand = parse("all --root . --manual").unwrap();

        assert!(
            may_still_work(&scheduled, &config).is_err(),
            "a scheduled pass outside its window must stop, not carry on until the disk finishes"
        );
        assert!(may_still_work(&hand, &config).is_ok(), "the hand pass is allowed to finish");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An install that never named a window keeps upstream's behaviour exactly: the clock has no
    /// authority to stop a pass, because it was never asked to schedule one.
    #[test]
    fn an_install_without_a_window_is_never_stopped_by_the_clock() {
        let dir = install("nowindow", r#"{"record_seconds": 900}"#);
        let config = Config::load(&dir).unwrap();
        assert_eq!(config.maintain_window(), None, "nothing was named");
        assert!(may_still_work(&parse("all --root .").unwrap(), &config).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stop request stops whatever is running, hand-requested or not, and it is seen by the whole
    /// pass rather than consumed by whichever step looked first.
    #[test]
    fn a_stop_request_stops_even_the_pass_somebody_asked_for() {
        let dir = install("stop", r#"{"maintain_window_start": "00:00", "maintain_window_end": "23:59"}"#);
        let config = Config::load(&dir).unwrap();
        let options = parse("all --root . --manual").unwrap();
        assert!(may_still_work(&options, &config).is_ok(), "nothing has been asked yet");

        std::fs::create_dir_all(config.lock_dir()).unwrap();
        std::fs::write(config.maintain_stop_signal_path(), "someone").unwrap();
        assert!(config.maintain_stop_requested(), "the request is visible, not taken");
        assert!(config.maintain_stop_requested(), "and reading it leaves it for every later step");
        assert_eq!(
            may_still_work(&options, &config).unwrap_err(),
            "stopped by request"
        );
        config.clear_maintain_stop();
        assert!(!config.maintain_stop_requested(), "honouring it clears it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The flag parses on every command, so the recorder's one command line cannot fail on the steps
    /// the flag is not about.
    #[test]
    fn manual_is_accepted_by_every_command_and_defaults_to_false() {
        assert!(!parse("all --root .").unwrap().manual);
        assert!(parse("all --root . --manual").unwrap().manual);
        assert!(parse("convert --root . --manual").unwrap().manual);
        assert!(parse("reindex --root . --manual").unwrap().manual);
    }

    #[test]
    fn every_command_is_reachable_and_only_the_writers_are_exclusive() {
        for (name, command, period) in [
            ("doctor", Command::Doctor, ""),
            ("text", Command::Text, ""),
            ("convert", Command::Convert, ""),
            ("refresh", Command::Refresh, ""),
            ("expire", Command::Expire, ""),
            // The one command whose arguments are not optional: see `forget_names_the_period_it_erases`.
            ("forget", Command::Forget, " --day 2026-09-22"),
            ("reindex", Command::Reindex, ""),
            ("ai-tags", Command::AiTags, ""),
            ("ai-summaries", Command::AiSummaries, ""),
            ("backup", Command::Backup, ""),
            ("all", Command::All, ""),
        ] {
            let options = parse(&format!("{name} --root .{period}")).expect(name);
            assert_eq!(options.command, command);
            assert_eq!(command.name(), name);
            // Everything that touches a file takes the lock; `doctor` alone is read-only. Backup is
            // in that set because copying a month file while `refresh` is rewriting it stores a
            // snapshot that is neither the old nor the new database. Reindex and ai-tags are in it too:
            // both touch the monthly index, so running one while a convert/refresh/expire or the other
            // is mid-write is the race the maintain lock exists to forbid.
            assert_eq!(
                command.exclusive(),
                !matches!(command, Command::Doctor),
                "{name} exclusivity"
            );
        }
        assert!(parse("nonsense --root .").is_err());
        assert!(parse("").is_err(), "no arguments is a usage message, not a run");
    }

    /// A `forget` with no period is refused at the argument list, before a database is opened.
    #[test]
    fn forget_names_the_period_it_erases_and_refuses_to_guess_one() {
        assert!(parse("forget --root .").is_err(), "no period at all must not mean the whole library");
        // Half a window parses — the CLI cannot tell `--from` alone from a later `--to` — and is refused
        // by `Args::window`, which is the one place a period is judged.
        let half = parse("forget --root . --from 2026-09-22").unwrap();
        assert!(half.forget.window(180).is_err(), "--from without --to is not a period");
        let options = parse("forget --root . --day 2026-09-22 --keyword revenue --dry-run").unwrap();
        assert_eq!(options.forget.day.as_deref(), Some("2026-09-22"));
        assert_eq!(options.forget.keyword(), Some("revenue"));
        assert!(options.dry_run);
        // A blank keyword is the same as none, and none means every row in the window — so the argument
        // that looks like a narrowing must not silently be the widest possible erase. Built as an argv
        // list because a value of spaces cannot survive the `parse` helper's own splitting.
        let blank = parse_options(&[
            "forget".to_string(),
            "--root".to_string(),
            ".".to_string(),
            "--day".to_string(),
            "2026-09-22".to_string(),
            "--keyword".to_string(),
            "   ".to_string(),
        ])
        .unwrap();
        assert_eq!(blank.forget.keyword(), None);
        assert!(blank.forget.window(180).is_ok());
        // And the flags belong to `forget` alone.
        assert!(parse("expire --root . --day 2026-09-22").is_err(), "a flag another command ignores");
    }

    /// `all` is what `windrec` spawns when the screen has been idle, so the set and the order of its
    /// steps are a contract with the recorder, not an implementation detail.
    #[test]
    fn the_pipeline_is_the_mutating_steps_in_the_order_that_makes_them_safe() {
        assert_eq!(
            PIPELINE,
            [
                Command::Text,
                Command::Convert,
                Command::Refresh,
                Command::Expire,
                Command::Reindex,
                Command::Previews,
                Command::AiTags,
                Command::AiSummaries,
                Command::Backup,
            ]
        );
        assert!(PIPELINE.iter().all(|step| step.exclusive()));
        assert!(!PIPELINE.contains(&Command::Doctor), "a pass must not include a read-only step");
        assert!(!PIPELINE.contains(&Command::All), "and must not recurse");
        // The one that matters most: an automated idle pass must never be able to erase a period nobody
        // chose. `forget` has no period to carry into `all` and gets no other home.
        assert!(!PIPELINE.contains(&Command::Forget), "nothing that runs on a timer may destroy the index");
        // Reindex precedes ai-tags: the month tagger summarises the window titles reindex writes, so
        // ordering them the other way would tag last month's stale picture of the library.
        let reindex_at = PIPELINE.iter().position(|s| *s == Command::Reindex).unwrap();
        let ai_at = PIPELINE.iter().position(|s| *s == Command::AiTags).unwrap();
        assert!(reindex_at < ai_at, "reindex must run before the tags that read what it wrote");
    }

    #[test]
    fn flags_are_read_in_both_shapes_and_reject_nonsense() {
        let options = parse("convert --root=. --dry-run --limit 3").unwrap();
        assert!(options.dry_run);
        assert_eq!(options.limit, Some(3));
        assert_eq!(options.root, PathBuf::from("."));

        let options = parse("refresh --root . --limit=12").unwrap();
        assert_eq!((options.dry_run, options.limit), (false, Some(12)));

        // The idle recorder's authorisation pid, in both shapes, and absent for a hand-run pass.
        assert_eq!(parse("all --root . --idle-granted-by 4321").unwrap().idle_granted_by, Some(4321));
        assert_eq!(parse("reindex --root . --idle-granted-by=99").unwrap().idle_granted_by, Some(99));
        assert_eq!(parse("all --root .").unwrap().idle_granted_by, None, "a hand-run pass grants itself nothing");

        assert!(parse("convert --root").is_err(), "a flag with no value");
        assert!(parse("convert --limit").is_err());
        assert!(parse("convert --limit zero").is_err(), "a limit that is not a number");
        assert!(parse("convert --limit 0").is_err(), "a limit of zero would silently do nothing");
        assert!(parse("convert --idle-granted-by").is_err(), "the pid flag needs a value");
        assert!(parse("convert --idle-granted-by many").is_err(), "a pid that is not a number is refused, not defaulted to None");
        assert!(parse("convert --idle-granted-by 0").is_err(), "pid 0 authorises nothing and is not a real grant");
        assert!(parse("convert --nope").is_err(), "an unknown flag");
        assert!(parse("doctor --root Z:/no/such/install").is_err(), "a root that is not there is an error, not an empty report");
    }

    #[test]
    fn usage_names_every_command_and_the_reason_it_was_asked_for() {
        let message = usage(Some("unknown command 'frobnicate'"));
        for name in [
            "doctor", "convert", "refresh", "expire", "forget", "reindex", "previews", "ai-tags", "ai-summaries", "backup", "all",
            "--dry-run", "--limit", "frobnicate", "maintain lock", "--idle-granted-by",
        ] {
            assert!(message.contains(name), "{name} missing from the usage text:\n{message}");
        }
        assert!(!usage(None).contains("error:"), "plain help must not look like a failure");
        // Every name `Command::name` can print has to appear, or help hides a command that works.
        for command in [
            Command::Doctor, Command::Convert, Command::Refresh, Command::Expire, Command::Forget, Command::Reindex, Command::Previews,
            Command::AiTags, Command::AiSummaries, Command::Backup, Command::All,
        ] {
            assert!(message.contains(command.name()), "{} missing from the usage text", command.name());
        }
    }

    /// The version line is the one answer this binary gives without a root, a config or the
    /// maintain lock, so it has to be complete on its own: name, version, profile.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windmaint "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
        assert!(usage(None).contains("--version"), "advertised nowhere but answered:\n{}", usage(None));
    }

    /// The lock the writers take is the directory upstream calls `LOCK_MAINTAIN`, and a pass that finds
    /// it held by a live process has to stop rather than race the recorder.
    #[test]
    fn the_maintain_lock_path_is_a_directory_and_holds_a_live_owner() {
        let dir = std::env::temp_dir().join(format!("windmaint-main-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config::load(&dir).unwrap();
        let lock_dir = config.maintain_lock_dir();
        assert_eq!(lock_dir, dir.join("cache").join("locks").join("LOCK_MAINTAIN"));
        {
            let _held = MaintainLock::acquire(&lock_dir).unwrap();
            assert!(lock_dir.is_dir());
            let foreign = std::fs::read_to_string(lock_dir.join("PID")).unwrap();
            assert_eq!(foreign.trim(), std::process::id().to_string());
        }
        assert!(!lock_dir.exists(), "the pass leaves nothing behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_reports_a_root_that_has_no_user_data_yet() {
        let dir = std::env::temp_dir().join(format!("windmaint-main-dry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config::load(&dir).unwrap();
        let options = Options { command: Command::Refresh, root: dir.clone(), dry_run: true, limit: None, forget: forget::Args::default(), idle_granted_by: None, manual: false };
        assert!(dispatch(&options, &config).is_ok());
        assert!(!dir.join("cache").exists(), "a dry run did not take the lock directory");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The nine steps are unchanged, and each of them has exactly one counter to report to. A step added
    /// to `PIPELINE` without a leg would run invisibly: the published pass would say nine steps and draw
    /// three rows, and the fourth row's absence would read as a queue nobody counted.
    #[test]
    fn every_step_of_the_pipeline_reports_to_one_leg() {
        for step in PIPELINE {
            assert!(Leg::of_step(step.name()).is_some(), "{} is a step of the pass and feeds no counter", step.name());
        }
        // The hand-run commands that are not steps of a pass name nothing.
        for command in [Command::Doctor, Command::Backlog, Command::Forget, Command::All] {
            assert_eq!(Leg::of_step(command.name()), None, "{} is not a step of a pass", command.name());
        }
    }

    /// 网络类失败不算整理失败: a pass whose only trouble came from the endpoint publishes its own ending as
    /// a completed run with the quiet leg named, while the exit status still reports the failed step. The
    /// two are allowed to differ because they answer different questions — "did the pass do what it
    /// promised" and "did every command succeed" — and a window that painted the whole night red because
    /// a gateway was down on one stretch would teach the person to ignore the red.
    #[test]
    fn an_endpoint_that_said_nothing_is_not_the_pass_failure() {
        let ending = closing(&["ai-summaries"], &[], PIPELINE.len(), 6);
        assert_eq!(ending.state, State::Complete, "the pass itself finished its work");
        assert!(ending.note.contains("ai-summaries"), "and says which leg stayed quiet: {}", ending.note);
        assert!(ending.note.contains("had nothing come back"), "{}", ending.note);
        assert!(ending.note.contains("6 item(s) still waiting"), "and what the next pass still has: {}", ending.note);
        assert_eq!(ending.error.as_deref(), Some("1 of 9 steps failed (ai-summaries)"), "the console is still told");
        // The leg this maps to is the yellow one, which is not the pass's own kind of trouble.
        assert!(!LegStatus::Offline.fails_the_pass());
    }

    /// The same ending, for the trouble that *is* the pass's own: a step that could not write its files
    /// makes the pass's state `failed`, with the same exit status it always had.
    #[test]
    fn a_step_of_its_own_trouble_closes_the_pass_as_failed() {
        let ending = closing(&["convert", "ai-tags"], &["convert"], PIPELINE.len(), 0);
        assert_eq!(ending.state, State::Failed, "this machine could not do its own work");
        assert!(ending.note.contains("1 of 9 steps failed (convert)"), "{}", ending.note);
        assert!(ending.note.contains("and 1 had nothing come back (ai-tags)"), "the quiet leg is still named: {}", ending.note);
        assert!(!ending.note.contains("still waiting"), "nothing was owed: {}", ending.note);
        // An exit status that lists both, as it always did: `dispatch` does not know which kind it was.
        assert_eq!(ending.error.as_deref(), Some("2 of 9 steps failed (convert, ai-tags)"));
        assert!(LegStatus::Failed.fails_the_pass());
    }

    /// A clean pass says so, and owes nothing — and with items left over (a `--limit`, or footage that
    /// arrived while the pass was running) it says that too rather than closing its mouth.
    #[test]
    fn a_pass_that_ran_says_what_it_ran_into() {
        let clean = closing(&[], &[], PIPELINE.len(), 0);
        assert_eq!(clean.state, State::Complete);
        assert_eq!(clean.note, "9 steps, no failures");
        assert_eq!(clean.error, None);
        let overshot = closing(&[], &[], PIPELINE.len(), 412);
        assert_eq!(overshot.state, State::Complete, "a census that came in low is not a failure");
        assert!(overshot.note.contains("412 item(s) still waiting for the next pass"), "{}", overshot.note);
    }

    /// 叫停 still has to say what it left: the same sentence on disk and on the console, counting from the
    /// denominators fixed at the start rather than from a fresh census the pass could not afford.
    #[test]
    fn the_stop_sentence_says_what_the_next_pass_still_has() {
        let note = stopped_sentence(3, "the maintenance window 03:30-05:00 has closed", 4_286);
        assert!(note.starts_with("stopped after 3 of 9 steps: the maintenance window"), "{note}");
        assert!(note.contains("4286") || note.contains("4 286") || note.contains("4,286"), "{note}");
        assert!(note.contains("still waiting for the next pass"), "{note}");
        let nothing_owed = stopped_sentence(9, "stopped by request", 0);
        assert!(nothing_owed.contains("0 item(s) still waiting"), "{nothing_owed}");
    }

    /// The pass's four denominators are the census's four numbers, fixed before its first step, out of the
    /// same walk the settings page reads. A second enumeration of the same queues would show up here as a
    /// bar that promises work the button did not count — which is the bug `backlog.rs` exists to prevent.
    #[test]
    fn the_bar_is_fixed_from_the_census_before_the_first_step() {
        let _serial = PUBLISHER.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windmaint-main-bar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("userdata/db")).unwrap();
        std::fs::create_dir_all(dir.join("userdata/videos")).unwrap();
        let config = Config::load(&dir).unwrap();
        // One row waiting for its text, which is the smallest queue the census can see.
        let db = config.db_dir().join(wind_base::paths::month_filename(&config.user_name(), 2026, 9));
        let mut store = wind_store::write::Store::open(&db).unwrap();
        store
            .append(&[wind_store::Record {
                videofile_name: "2026-09-21_21-16-12.mp4".into(),
                picturefile_name: "2026-09-21_21-16-12.jpg".into(),
                videofile_time: wind_base::clock::LocalParts::from_stamp("2026-09-21_21-16-12").unwrap().naive_epoch_seconds(),
                ocr_text: String::new(),
                win_title: None,
                deep_linking: None,
                thumbnail: None,
            }])
            .unwrap();
        drop(store);

        // The census is a walk of the steps in dry run, and a step reports what it handles: taken with no
        // publisher installed, it counts without scoring its own rehearsal.
        wind_base::maintain::uninstall();
        let counted = backlog::census(&dir, &config).expect("the census answers");
        assert_eq!(counted.text_rows, 1, "the row above is the queue");
        assert_eq!(counted.totals().text, 1, "and it is the text leg's denominator");

        let options = Options { command: Command::All, root: dir.clone(), dry_run: false, limit: None, forget: forget::Args::default(), idle_granted_by: None, manual: true };
        open_the_pass(&options, &config);
        let shown = wind_base::maintain::read(&config.maintain_progress_path()).expect("the pass is published before its first step");
        let totals = counted.totals();
        assert_eq!(
            shown.items_total(),
            totals.text + totals.convert + totals.ai + totals.other,
            "the total bar is the census's own four numbers, added — not a second count"
        );
        assert_eq!(shown.leg(Leg::Text).map(|count| (count.done, count.total, count.status)), Some((0, 1, LegStatus::Waiting)), "the row the month file is waiting for, counted and not yet worked");
        assert!(shown.leg(Leg::Convert).is_none(), "nothing on disk to encode, so no row drawn");
        // And the nine-step keys the shipped window reads are still there and still honest: a pass that has
        // not reached its first step says so, rather than going blank on a reader that knows only `step`.
        assert_eq!((shown.step, shown.steps, shown.state), (0, 0, State::Running));
        wind_base::maintain::uninstall();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One pass promises its ceiling, not the backlog. The census counts every pending stretch in fourteen
    /// days (137 on the install this was written on) while the settings page lets one pass ask forty; a bar
    /// that no pass can ever fill is read as a stall, so the denominator is the ceiling and the remainder
    /// is a number the row says out loud.
    #[test]
    fn one_pass_promises_its_ceiling_and_names_what_it_leaves_out() {
        let dir = std::env::temp_dir().join(format!("windmaint-main-ceiling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"summary_pending_days_in_idle": 2, "summary_stretch_limit_in_idle": 40}"#,
        )
        .unwrap();
        let config = Config::load(&dir).unwrap();
        let waiting = wind_base::maintain::Totals { text: 62, convert: 3, ai: 137, other: 27 };

        let (promise, deferred) = promised(&config, waiting);
        assert_eq!(promise.ai, 40, "the AI bar is drawn to a height this pass can actually reach");
        assert_eq!(deferred, 97, "and what it cannot reach is still a number somebody can read");
        assert_eq!((promise.text, promise.convert, promise.other), (62, 3, 27), "the local legs keep the census's own counts");

        // A queue inside the ceiling is the queue: no cap, no remainder, no invented sentence.
        let small = wind_base::maintain::Totals { text: 0, convert: 0, ai: 9, other: 0 };
        assert_eq!(promised(&config, small), (small, 0), "a backlog one pass can finish promises the backlog");
        // The ceiling is the setting's, so a person who raises it raises the bar with it.
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"summary_pending_days_in_idle": 2, "summary_stretch_limit_in_idle": 200}"#,
        )
        .unwrap();
        let raised = Config::load(&dir).unwrap();
        assert_eq!(promised(&raised, waiting), (waiting, 0), "200 asks the whole 137, so 137 is the promise");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--dry-run` with the new legs claims nothing and starts nothing: no thread exists, so no thread can
    /// spawn a child, send a request, write or rename anything. The planted `windai.exe` and
    /// `wind-reindex.exe` are two bytes of `MZ` — not executables — so a leg that ignored the rehearsal
    /// would have to report a spawn failure rather than quietly do the work it was only meant to describe,
    /// and the tree comparison says which of the two happened.
    #[test]
    fn a_dry_run_pass_starts_no_leg_and_claims_nothing_it_did_not_do() {
        let dir = std::env::temp_dir().join(format!("windmaint-main-dry-legs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"maintain_window_start": "00:00", "maintain_window_end": "23:59", "enable_ai_summary_in_idle": true}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/windai.exe"), b"MZ").unwrap();
        std::fs::write(dir.join("bin/wind-reindex.exe"), b"MZ").unwrap();

        // A settled past day with one stretch of already-read text, so the AI leg's first question — "is
        // there anything the local steps cannot change?" — has a real yes to answer; and a closed slice, so
        // `convert` has a real segment to hand over. Both are work the two legs would have started on.
        let at = wind_base::clock::now().naive_epoch_seconds() - 3 * 86_400;
        let stamp = wind_base::clock::LocalParts::from_naive_epoch(at).stamp();
        let name: &'static str = Box::leak(format!("{stamp}.mp4").into_boxed_str());
        wind_summary::test_support::seed_month(&dir, "default", &[(at, name, "Qoder", "a screen this pass has nothing left to read")]);
        let slice = dir.join("cache_screenshot").join(wind_base::clock::LocalParts::from_naive_epoch(at + 120).stamp());
        std::fs::create_dir_all(slice.join(wind_base::paths::SUBMIT_MARKER_DIR)).unwrap();
        for offset in 0..6 {
            let frame = wind_base::clock::LocalParts::from_naive_epoch(at + offset).stamp();
            std::fs::write(slice.join(format!("{frame}.jpg")), b"jpeg").unwrap();
        }

        let config = Config::load(&dir).unwrap();
        let dry = Options {
            command: Command::All,
            root: dir.clone(),
            dry_run: true,
            limit: None,
            forget: forget::Args::default(),
            idle_granted_by: None,
            manual: true,
        };

        // The legs themselves: a rehearsal starts no thread and hands nothing to anybody.
        let legs = Legs::for_pass(&dry, &config);
        assert!(legs.ai.is_none(), "a dry run starts no AI lane, so nothing can send a request");
        assert!(legs.follow.is_none(), "and no reindex lane, so nothing can spawn `wind-reindex`");
        let alone = Legs::standalone();
        assert!(alone.ai.is_none() && alone.follow.is_none(), "and a command run by hand is the same shape");

        let before = listing(&dir);
        dispatch(&dry, &config).expect("a dry run of the whole pass answers, and answers without the lock");
        assert_eq!(listing(&dir), before, "a dry run wrote, created or renamed nothing — including no child's output");
        assert!(!dir.join("cache").exists(), "a dry run took no maintain lock and published no progress file");
        assert_eq!(std::fs::read(dir.join("bin/windai.exe")).unwrap(), b"MZ".to_vec(), "no request was attempted");
        assert_eq!(std::fs::read(dir.join("bin/wind-reindex.exe")).unwrap(), b"MZ".to_vec(), "and no walk was attempted");

        // The same pass with the rehearsal turned off does start both legs — which is what makes the
        // assertion above a statement about `--dry-run` rather than about a leg that never works. Put down
        // immediately: a test must not leave a lane waiting at a hand-off nobody is filling.
        let running = Options { dry_run: false, ..copy_of(&dry) };
        let mut started = Legs::for_pass(&running, &config);
        assert!(started.ai.is_some(), "a real pass starts the AI leg");
        assert!(started.follow.is_some(), "and the lane that waits at the encoder");
        started.put_down(&config);
        assert!(started.ai.is_none() && started.follow.is_none(), "and both are in again once they are put down");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The nine steps are unchanged, so a pass that starts legs still publishes nine boundaries in nine
    /// names — and a leg is joined by the step whose work it did, never by a new one.
    #[test]
    fn the_legs_move_no_step_out_of_the_nine_and_rename_nothing() {
        let names: Vec<&'static str> = PIPELINE.iter().map(|step| step.name()).collect();
        assert_eq!(names, vec!["text", "convert", "refresh", "expire", "reindex", "previews", "ai-tags", "ai-summaries", "backup"]);
        // The two steps that own a leg are the two the legs are joined at, and each leg's work is scored in
        // the counter its own step scores in: the lane's rows in the step's number, the AI half in the leg.
        assert_eq!(wind_base::maintain::Leg::of_step("convert"), Some(wind_base::maintain::Leg::Convert));
        assert_eq!(wind_base::maintain::Leg::of_step("reindex"), Some(wind_base::maintain::Leg::Other));
        assert_eq!(wind_base::maintain::Leg::of_step("ai-summaries"), Some(wind_base::maintain::Leg::Ai));
    }

    /// `Options` carries the `forget` window and is not `Clone`, and a test that needs the same pass with one
    /// flag changed is worth a copy function rather than a constructor that can silently forget a field.
    fn copy_of(options: &Options) -> Options {
        Options {
            command: options.command,
            root: options.root.clone(),
            dry_run: options.dry_run,
            limit: options.limit,
            forget: forget::Args::default(),
            idle_granted_by: options.idle_granted_by,
            manual: options.manual,
        }
    }

    /// Every path under a scratch root, sorted: the same comparison `backlog`'s census test makes, so a pass
    /// that wrote a byte somewhere is caught whatever it named the file.
    fn listing(root: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut walk = vec![root.to_path_buf()];
        while let Some(dir) = walk.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                out.push(path.strip_prefix(root).unwrap().display().to_string());
                if path.is_dir() {
                    walk.push(path);
                }
            }
        }
        out.sort();
        out
    }
}
