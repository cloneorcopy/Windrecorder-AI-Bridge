//! Summarise recorded stretches, and the days built from them, in this machine's own words.
//!
//! # Why this exists next to the MCP writers rather than instead of them
//!
//! The bridge lets an outside AI write summaries; this lets the install do it on its own, with the key
//! the user already pasted into Settings. Both are needed — the outside agent brings a model the user
//! trusts with their real questions, and the inside pass is what makes the history summarised *at all* on
//! a day nobody sat down to work through. Neither is a special case in the code: both ask `wind-summary`
//! the same question ("what is left, and does what exists still stand") and both write through the same
//! functions. That shared queue is the only reason a day summarised by one producer is not redone by the
//! other, and it is why the answer does not depend on who asks first.
//!
//! # What leaves the machine
//!
//! The point of a summary is that it is written *from the screen text*, so unlike the month tagger —
//! which sends window titles only — this sends each stretch's captured text to the endpoint named by
//! `open_ai_base_url`. Nothing is clipped, deliberately: a summary of the first four thousand characters
//! of a twenty-minute stretch is a wrong answer that looks right, so the stretch goes out as it was
//! captured, and an endpoint whose context is too small says so in its own error, which is reported
//! verbatim rather than pre-empted by a truncation nobody asked for.
//!
//! [`Report::chars_sent`] is the number to look at before a big run, and `--dry-run` produces it without
//! sending anything or writing anything.
//!
//! # The idle path
//!
//! `windmaint` calls this with `--pending` while the machine is idle, gated on
//! `enable_ai_summary_in_idle`. A failing stretch is reported and tolerated: one bad request must not end
//! the pass, and the queue offers that stretch again next time, because it still has no summary.
//!
//! # The AI leg's pacing
//!
//! Stretches are asked **four at a time** ([`IN_FLIGHT`]), one request per stretch — the pass never folds
//! two stretches into one ask, because an answer that cannot be matched back to its own stretch, or that
//! mixes two of them into one paragraph, is a risk no network round trip is worth. Each request is
//! accounted for on its own: the one that did not come back is the only one that owes anything.
//!
//! Three rules sit on top of that, all of them from
//! `docs/adr/2026-09-30-the-organise-pass-runs-on-four-legs.md` (四.2, 四.3, 四.4, 四.6):
//!
//!   * **Busy means slower.** An endpoint that answers `429` or `503` is rate limited, not refusing, so
//!     this pass drops to [`IN_FLIGHT_AFTER_BUSY`] for every wave after that and does not climb back
//!     within the pass — a gateway has no way to say it recovered, and climbing on a guess is how a rate
//!     limit becomes a fight. The next pass starts at four again; the number lives in one `run`.
//!   * **Failure is counted per wave.** Four requests that all come back silent are **one** "this did not
//!     come back" event, not four, or the fifteen-minute receive deadline would turn one bad wave into a
//!     closed leg. [`GIVE_UP_AFTER_SILENT_WAVES`] silent waves in a row close the leg for the rest of the
//!     pass and the report says how many stretches were left unasked — the same shape of protection the
//!     OCR engine keeps, and it counts waves for the same reason.
//!   * **The disk is asked before anything is called missing.** The MCP bridge writes the same day files,
//!     so a request that timed out may have been answered by somebody else. At the end of each day the
//!     pass re-reads the queue through `wind-summary` — the one owner of those files, and the same
//!     derivation that produced the pending list — and any stretch the queue no longer owes is counted as
//!     done rather than failed. No second counter of "what is owed" exists here.
//!
//! Writing is safe across lanes because `write_period` is a read-merge-write of one day file taken under
//! both a process-wide mutex and a pid lock (`summary::files`), which is what the bridge and the idle pass
//! already depend on; and this path writes no index at all — `wind-summary` opens the month databases read
//! only, so there is no SQLite write to keep on the calling thread. The report still folds in *target*
//! order, the way `wind_base::pool` is designed for, so no line of it depends on which lane finished first.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::pool;
use wind_base::prompts::{render, Name, Prompts};
use wind_summary as summary;

use crate::client::{ChatRequest, Client, Completion, Transport};
use crate::error::AiError;
use crate::library::Index;

/// How many stretch requests this pass keeps in flight at once.
///
/// Four, written here rather than in a setting: the owner decided the number on 2026-09-30 and said no to
/// a configuration key for it, because a second knob on the settings page is a thing to reason about at
/// the moment the machine is already late. Four is the width that hides the wait behind other waits
/// without asking a hosted queue to do more than a person's own browser would, and each request is
/// fifteen minutes of patience at most (see `http::RECEIVE_TIMEOUT`) — a wider leg would not be faster, it
/// would be more requests that all time out together.
///
/// This is *not* `wind_base::pool::lanes(Duty::Decode)`: that number is about not starving a machine, and
/// this work is network-bound with its ceiling on the other end of the wire. The pool is still the thing
/// that runs the wave; only the lane count comes from here.
pub const IN_FLIGHT: usize = 4;

/// How many requests are in flight for the rest of this pass once the endpoint has said it is busy.
///
/// Half of [`IN_FLIGHT`], and one-way: see the pacing note in this module's header.
pub const IN_FLIGHT_AFTER_BUSY: usize = 2;

/// Silent waves in a row that close the stretch leg for the rest of this pass.
///
/// Modelled on the OCR engine's `wind_base::wxocr::GIVE_UP_AFTER`, which is also three and also exists
/// because a caller that keeps asking a thing that has stopped answering turns a slow night into a hung
/// one. Here the unit is a wave rather than a frame, for exactly the reason in the module header: four
/// requests that all came back silent are one event.
pub const GIVE_UP_AFTER_SILENT_WAVES: u32 = 3;

/// How far back `--pending` looks for days with outstanding work.
///
/// A bound, not conservatism: without it, a machine that has never summarised anything would scan its
/// whole library on every idle pass, and an idle pass that runs for an hour is one the user notices as a
/// warm laptop rather than as progress.
pub const PENDING_SCAN_DAYS: usize = 60;

/// Temperature for prose about what happened. The tagger uses 0.3 for the same reason: this is a reading
/// of what was on screen, not a creative act, and a hotter answer is a less repeatable one.
pub const SUMMARY_TEMPERATURE: f64 = 0.3;

/// Which days, and how much of them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    /// Exactly one product day, `YYYY-MM-DD`. Takes precedence over `pending`.
    pub day: Option<String>,
    /// Up to this many days with outstanding work, scanned back from today.
    pub pending: Option<usize>,
    /// At most this many stretches asked for in the whole run.
    pub limit: Option<usize>,
    /// Re-ask stretches even where a standing summary exists — the only way to regenerate a day after a
    /// deliberate prompt rewrite without deleting files by hand.
    pub force: bool,
    /// Write a day's summary over gaps, recording that it was done that way.
    pub allow_partial: bool,
    /// Build the requests and report their size; send nothing, write nothing.
    pub dry_run: bool,
}

/// What happened to one day's own summary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DailyOutcome {
    /// The day is incomplete and `--allow-partial` was not given, so nothing was asked.
    HeldBack { summarised: usize, total: usize },
    /// Already current, so no request was made.
    UpToDate,
    /// Would be written, on a dry run.
    Planned { chars: usize },
    Written,
    Failed(String),
    /// The day holds no recorded stretch, so there is nothing to summarise or to wait for. The default,
    /// because a `DayReport` is built before the day's own summary is reached.
    #[default]
    NothingRecorded,
}

/// One day's lines in the report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DayReport {
    pub date: String,
    pub stretches: usize,
    pub asked: usize,
    pub written: usize,
    pub cached: usize,
    pub failed: Vec<String>,
    pub chars_sent: usize,
    pub daily: DailyOutcome,
}

/// Everything a run did, in the shape `main` prints.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub days: Vec<DayReport>,
    pub dry_run: bool,
    pub sent: usize,
    pub written: usize,
    pub cached: usize,
    pub failed: usize,
    pub chars_sent: usize,
    /// How hard the pass leaned on the endpoint while it did this.
    pub pacing: Pacing,
}

/// How one pass paced its requests, recorded because "did it really slow down, and when" is otherwise a
/// question about timing that no test — and no user reading a log next week — can answer.
///
/// It is not a second tally of the work: every number in the closing sentence is still what it was, and
/// this says nothing about what is owed. It says how many things were in flight, which is the one fact a
/// throttled or closed leg changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pacing {
    /// How many requests each wave opened, in the order they ran. Empty for a `--dry-run`, which sends
    /// nothing, and for a pass whose queue was already satisfied.
    pub waves: Vec<usize>,
    /// The endpoint answered busy (`429`/`503`) at some point, so the waves after that ran
    /// [`IN_FLIGHT_AFTER_BUSY`] at a time.
    pub throttled: bool,
    /// The longest run of waves in which **nothing** came back. One, not four, however many requests the
    /// wave held: see [`GIVE_UP_AFTER_SILENT_WAVES`].
    pub silent_waves: u32,
    /// The leg closed before the queue ran out.
    pub given_up: bool,
    /// How many stretches were left unasked when it closed. They stay in the queue, because the queue is
    /// read off the disk and they have nothing on it.
    pub left_unasked: usize,
    /// The run of silent waves so far, which [`Pacing::note_wave`] resets the moment anything answers.
    silent_run: u32,
}

impl Pacing {
    /// How wide the next wave is.
    fn width(&self) -> usize {
        if self.throttled { IN_FLIGHT_AFTER_BUSY } else { IN_FLIGHT }
    }

    /// What one wave came back with: whether the endpoint is busy, and whether the whole wave was silent.
    fn note_wave(&mut self, wave: &[Answer]) {
        if wave.is_empty() {
            return;
        }
        if wave.iter().any(|answer| matches!(answer, Answer::Busy(_))) {
            self.throttled = true;
        }
        let silent = wave.iter().filter(|answer| matches!(answer, Answer::Silent(_))).count();
        if silent == wave.len() {
            self.silent_run += 1;
            self.silent_waves = self.silent_waves.max(self.silent_run);
            if self.silent_run >= GIVE_UP_AFTER_SILENT_WAVES {
                self.given_up = true;
            }
        } else {
            // One paragraph, one refusal, one body that is not JSON — anything with bytes in it says the
            // endpoint is alive, and the run of silence starts over.
            self.silent_run = 0;
        }
    }
}

/// What one stretch's request came back with, before the report folds it.
///
/// The three failure shapes are separate on purpose. They are the same one line in the report and three
/// different things to decide about the endpoint: it is busy, it has stopped answering, or it answered
/// with something unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    /// A paragraph arrived and it is on disk under this stretch's key.
    Written,
    /// `429`/`503`: the endpoint is rate limited. Nothing was written, and the pass slows down.
    Busy(String),
    /// Nothing came back at all — the deadline, the socket or the handshake. Counts once per wave.
    Silent(String),
    /// It answered, and the answer or the write was unusable. Not the endpoint's health.
    Failed(String),
    /// The leg had closed, so this stretch was never asked.
    NotAsked,
}

/// One stretch together with the request rendered for it, which is what a lane is handed.
struct Job<'a> {
    segment: &'a summary::Segment,
    request: Request,
}


/// One fully rendered request. Public because `--dry-run`'s whole value is showing it: this is the text
/// that would be sent, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub system: String,
    pub user: String,
    pub chars: usize,
}

impl Request {
    fn new(system: String, user: String) -> Request {
        let chars = system.chars().count() + user.chars().count();
        Request { system, user, chars }
    }
}

/// The block one frame occupies in `{frames_table}`.
///
/// Fixed in code rather than in a template on purpose: a user editing a prompt chooses *where* the frames
/// go and in what words they are introduced, not how a frame is encoded. If the encoding lived in the
/// template, an edit could break it and every later summary would quietly lose its URLs.
pub fn frame_block(frame: &summary::Frame) -> String {
    let when = LocalParts::from_naive_epoch(frame.timestamp).time_display();
    let title = frame.title.clone().unwrap_or_else(|| "‹no title captured›".to_string());
    let mut block = format!("[{when}] window: {title}");
    if let Some(url) = &frame.url {
        block.push_str(&format!("\n           link:   {url}"));
    }
    block.push_str(&format!("\n           text:\n{}", frame.text));
    block
}

/// The `{frames_table}` for one stretch: every frame, in index order, nothing dropped.
pub fn frames_table(segment: &summary::Segment) -> String {
    segment.detail.iter().map(frame_block).collect::<Vec<_>>().join("\n\n")
}

/// The stretch request, rendered from the templates this install resolved.
///
/// `{language}` is the install's own answer language — `lang`, resolved once into [`Prompts::language`] —
/// so a Chinese install is not answered in English because the screen it read happened to hold an English
/// window title. A template that writes its own language in place of the slot is unaffected: substitution
/// only touches tokens that are there.
pub fn period_request(prompts: &Prompts, segment: &summary::Segment) -> Request {
    let system = render(prompts.text(Name::PeriodSystem), &[("{language}", prompts.language)]);
    let table = frames_table(segment);
    let start = LocalParts::from_naive_epoch(segment.start).display();
    let end = LocalParts::from_naive_epoch(segment.end).display();
    let duration = segment.compact_duration();
    let frames = segment.frames.to_string();
    let user = render(
        prompts.text(Name::PeriodUser),
        &[
            ("{segment}", &segment.key),
            ("{start}", &start),
            ("{end}", &end),
            ("{duration}", &duration),
            ("{frames}", &frames),
            ("{frames_table}", &table),
        ],
    );
    Request::new(system, user)
}

/// The day request: the stretch paragraphs, each labelled with the span it describes.
pub fn daily_request(prompts: &Prompts, queue: &summary::DayQueue, periods: &BTreeMap<String, summary::PeriodSummary>) -> Request {
    let system = render(prompts.text(Name::DailySystem), &[("{language}", prompts.language)]);
    let table = queue
        .current
        .iter()
        .filter_map(|segment| {
            periods
                .get(&segment.key)
                .map(|written| format!("{} ({}): {}", segment.clock_span(), segment.compact_duration(), written.text))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let begin = queue.span.day_begin_label();
    let end = LocalParts::from_naive_epoch(queue.span.to).display();
    let total = queue.segments_total.to_string();
    let done = queue.summarised.to_string();
    let user = render(
        prompts.text(Name::DailyUser),
        &[
            ("{date}", &queue.day),
            ("{day_begin}", &begin),
            ("{day_end}", &end),
            ("{day_rule}", &format!("the product day, which begins at {begin}")),
            ("{segments_total}", &total),
            ("{segments_summarised}", &done),
            ("{period_summaries}", &table),
        ],
    );
    Request::new(system, user)
}

/// The two digests that make "written under which words" a comparable fact.
pub fn digests(prompts: &Prompts) -> summary::PromptDigests {
    summary::PromptDigests::of(&prompts.period_system, &prompts.period_user, &prompts.daily_system, &prompts.daily_user)
}

/// One rendered request plus what it is about, for the settings screen's "try these words" button.
///
/// The newest stretch the day can offer is used — pending first, because that is the work on screen, and
/// otherwise the most recent finished one, because a user who has just rewritten a prompt wants to see the
/// new style even when there is nothing left to do. `None` says the template is not a summary template:
/// the tag and search prompts are exercised by `windai tags --dry-run` and `windai search --explain`, and
/// pretending otherwise would put a button on the page that does something other than what it says.
pub fn trial(
    prompts: &Prompts,
    name: Name,
    queue: &summary::DayQueue,
    periods: &summary::DayMap,
) -> Option<(String, (String, String))> {
    let period = matches!(name, Name::PeriodSystem | Name::PeriodUser);
    if !period && !matches!(name, Name::DailySystem | Name::DailyUser) {
        return None;
    }
    if period {
        let segment = queue
            .pending
            .iter()
            .map(|item| &item.segment)
            .chain(queue.current.iter())
            .filter(|segment| !segment.detail.is_empty())
            .last()?;
        let request = period_request(prompts, segment);
        return Some((format!("stretch {}", segment.key), (request.system, request.user)));
    }
    let request = daily_request(prompts, queue, &periods.entries);
    Some((format!("day {}", queue.day), (request.system, request.user)))
}

/// A whole run.
pub fn run<T>(index: &Index, client: &Client<T>, options: &Options) -> Result<Report, AiError>
where
    T: Transport + Clone + Send + Sync,
{
    let config = index.config.clone();
    let prompts = Prompts::read(&config);
    let digests = digests(&prompts);
    let reader = summary::Reader::fresh(&config);
    let days = select_days(index, options, &reader, &digests)?;

    let mut report = Report { dry_run: options.dry_run, ..Default::default() };
    let mut budget = options.limit.unwrap_or(usize::MAX);
    let mut pacing = Pacing::default();
    for date in days {
        let queue = summary::for_day_with(&reader, &date, &digests).map_err(|e| index.faults().store(&e))?;
        let periods = summary::read_period(&config, &date);
        let mut day = DayReport { date: date.clone(), stretches: queue.segments_total, ..Default::default() };
        if periods.exists && !periods.readable {
            day.failed.push(periods.note.clone());
        }

        let targets: Vec<&summary::Segment> = if options.force {
            queue.all_segments()
        } else {
            queue.pending.iter().map(|item| &item.segment).collect()
        };
        let targets = if targets.len() > budget { &targets[..budget] } else { &targets[..] };
        budget = budget.saturating_sub(targets.len());
        day.cached = queue.segments_total.saturating_sub(targets.len());

        // What the queue owed when this pass began, by key — the ledger the day's closing look at the disk
        // is compared against. `--force` re-asks stretches the queue was already happy with, and those are
        // deliberately not in this set: an entry that was standing before the pass and is still standing
        // after it is not somebody else's answer to a request that failed.
        let owed_before: BTreeSet<&str> = queue.pending.iter().map(|item| item.segment.key.as_str()).collect();

        // Every request is rendered on the calling thread before any of them is sent. `--dry-run` has to
        // report the cost of the whole set it would have sent — which is the entire point of it — and a
        // lane that stopped to read a prompt template is one fewer request in flight.
        let jobs: Vec<Job> = targets.iter().map(|segment| Job { segment, request: period_request(&prompts, segment) }).collect();
        for job in &jobs {
            day.asked += 1;
            day.chars_sent += job.request.chars;
            report.chars_sent += job.request.chars;
        }

        let answers = if options.dry_run || jobs.is_empty() { Vec::new() } else { ask_in_waves(&jobs, client, &config, &prompts, &mut pacing) };

        // The day itself, recomputed after the stretch writes so the gate sees this run's own work
        // rather than the state the run started in — and so that a stretch whose paragraph arrived from
        // the other producer, while this pass's own request was timing out, can be recognised before it
        // is written off. That is the closing look at the disk this leg owes (ADR 四.6): the queue is the
        // one place that decides what is still owed, and this pass adds no counter of its own.
        let queue = summary::for_day_with(&reader, &date, &digests).map_err(|e| index.faults().store(&e))?;
        let owed_now: BTreeSet<&str> = queue.pending.iter().map(|item| item.segment.key.as_str()).collect();

        // Folded in target order, never in completion order — the rule `wind_base::pool` is built on, so
        // two runs of the same install print the same lines whatever order the lanes came back in.
        for (job, answer) in jobs.iter().zip(answers) {
            let key = job.segment.key.as_str();
            // `sent` is requests this pass actually put on the wire: everything except what a closed leg
            // left unasked. `asked` above is the wider number — what the queue handed this pass to do.
            if !matches!(answer, Answer::NotAsked) {
                report.sent += 1;
            }
            let line = match answer {
                Answer::Written => {
                    day.written += 1;
                    report.written += 1;
                    None
                }
                Answer::Busy(why) | Answer::Silent(why) | Answer::Failed(why) => Some(format!("{key}: {why}")),
                Answer::NotAsked => Some(format!(
                    "{key}: not asked — this pass had already stopped asking, because the endpoint sent nothing back for {GIVE_UP_AFTER_SILENT_WAVES} waves in a row"
                )),
            };
            match line {
                // Somebody else's answer counts as done, and the `cached` count below picks it up from the
                // queue re-read above — which is why there is no `written by another producer` counter.
                Some(_) if was_answered_on_disk(key, &owed_before, &owed_now) => {}
                Some(why) => day.failed.push(why),
                None => {}
            }
        }
        report.failed += day.failed.len();

        let periods = summary::read_period(&config, &date);
        day.cached = day.cached.max(queue.summarised);
        day.daily = finish_day(client, &config, &prompts, &digests, &queue, &periods, options, pacing.given_up);
        if let DailyOutcome::Failed(_) = day.daily {
            report.failed += 1;
        }
        report.days.push(day);
    }
    report.cached = report.days.iter().map(|day| day.cached).sum();
    report.pacing = pacing;
    Ok(report)
}

/// Whether a stretch this pass failed to write has been answered on the disk anyway.
///
/// Two conditions, both of which need the pair of queues: the stretch had to be owed when the pass started
/// (`owed_before`) and must not be owed now (`owed_now`). Anything the queue no longer owes has an entry
/// that still describes the stretch's own content digest and was written under the prompt now in force —
/// `for_day_with` is what decides that, so this asks it rather than reading a day file and comparing
/// fingerprints by hand.
fn was_answered_on_disk(key: &str, owed_before: &BTreeSet<&str>, owed_now: &BTreeSet<&str>) -> bool {
    owed_before.contains(key) && !owed_now.contains(key)
}

/// Sends one day's stretch requests in waves and returns one answer per job, in job order.
///
/// A wave is at most [`Pacing::width`] jobs, and the pool runs each of them on its own thread, so that is
/// exactly how many requests are on the wire at once. The wave boundary is where both endpoint-health rules
/// are decided — the throttle and the give-up — because "a wave of four came back silent" is one event,
/// and deciding it any finer would make four-in-flight look like an instant failure.
///
/// Each lane builds its own [`Client`] from the settings this pass was handed, and says why in `client.rs`:
/// `Client<T>` makes no `Sync` promise for its transport, and the borrow `wind_base::pool` hands to a
/// shared closure would need one. Sharing a client would have shared nothing anyway — `http::post` opens
/// and closes a WinHTTP session per request — so what the lanes genuinely have in common is the settings.
fn ask_in_waves<T>(jobs: &[Job<'_>], client: &Client<T>, config: &Config, prompts: &Prompts, pacing: &mut Pacing) -> Vec<Answer>
where
    T: Transport + Clone + Send + Sync,
{
    let settings = client.settings.clone();
    let transport = client.transport();
    let mut answers = vec![Answer::NotAsked; jobs.len()];
    let mut next = 0;
    while next < jobs.len() {
        if pacing.given_up {
            // Everything from here keeps the `NotAsked` answer it was initialised with. The pass does not
            // pretend these are failures of the endpoint's data: they are work this machine never started,
            // and the queue still holds every one of them.
            pacing.left_unasked += jobs.len() - next;
            break;
        }
        let width = pacing.width().min(jobs.len() - next);
        let wave = &jobs[next..next + width];
        pacing.waves.push(width);
        let slots = pool::run(wave, width, || true, |job| {
            let client = Client::with_transport(settings.clone(), transport.clone());
            answer_one(&client, job, config, prompts)
        });
        for (offset, slot) in slots.into_iter().enumerate() {
            answers[next + offset] = slot.expect("a wave claims every job it was given");
        }
        pacing.note_wave(&answers[next..next + width]);
        next += width;
    }
    answers
}

/// One stretch, one request, one answer: send it, and file whatever usable paragraph came back.
fn answer_one<T: Transport>(client: &Client<T>, job: &Job<'_>, config: &Config, prompts: &Prompts) -> Answer {
    match ask(client, &job.request) {
        Ok(completion) => match write_stretch(config, prompts, job.segment, completion.text.trim().to_string()) {
            Ok(()) => Answer::Written,
            // A refused write is this program's business, not the endpoint's: bytes came back, so the wave
            // this belongs to is not a silent one.
            Err(why) => Answer::Failed(why),
        },
        Err(why) if why.is_busy() => Answer::Busy(why.to_string()),
        Err(why) if why.is_silent() => Answer::Silent(why.to_string()),
        Err(why) => Answer::Failed(why.to_string()),
    }
}

/// One model call, with the failure kept as the error it arrived as.
///
/// The pass has to tell three shapes apart before it writes a line — busy, silent, answered-with-noise —
/// so nothing is stringified here on the way out; each caller decides what its report needs.
fn ask<T: Transport>(client: &Client<T>, request: &Request) -> Result<Completion, AiError> {
    client.ask(&ChatRequest { system: &request.system, user: &request.user, temperature: SUMMARY_TEMPERATURE, json_mode: false })
}

/// File one stretch's paragraph, with every number taken from the index rather than from the answer.
///
/// Safe from several lanes at once: `summary::write_period` takes a process-wide mutex and a pid lock around
/// its read-merge-write of one day file (see `summary::files`, and `two_threads_writing_one_day_keep_both_
/// paragraphs` there), which is the same gate the bridge writes under. Nothing here touches the month index
/// — `wind-summary` reads it through read-only connections and cannot write a row — so no index write had to
/// be left on the calling thread when this became a multi-lane path.
fn write_stretch(config: &Config, prompts: &Prompts, segment: &summary::Segment, text: String) -> Result<(), String> {
    let digests = summary::PromptDigests::of(&prompts.period_system, &prompts.period_user, &prompts.daily_system, &prompts.daily_user);
    let entry = summary::PeriodSummary {
        text,
        start: segment.start,
        end: segment.end,
        frames: segment.frames,
        ocr_chars: segment.ocr_chars,
        written_at: summary::now_stamp(),
        written_by: "windai".to_string(),
        model: config.str_or("open_ai_modelname", ""),
        source_fingerprint: segment.fingerprint.clone(),
        prompt_fingerprint: digests.period,
    };
    summary::write_period(config, &segment.day, &segment.key, &entry).map(|_| ()).map_err(|e| e.to_string())
}

/// The day's own paragraph, if the day is ready for one.
///
/// `leg_closed` is the stretch leg's give-up: after enough silent waves, the day is reported as not asked
/// rather than sent one more request into the same silence, which would cost the pass another deadline
/// before it ends.
fn finish_day<T: Transport>(
    client: &Client<T>,
    config: &Config,
    prompts: &Prompts,
    digests: &summary::PromptDigests,
    queue: &summary::DayQueue,
    periods: &summary::DayMap,
    options: &Options,
    leg_closed: bool,
) -> DailyOutcome {
    if queue.segments_total == 0 {
        return DailyOutcome::NothingRecorded;
    }
    if !queue.gate_open() && !options.allow_partial {
        return DailyOutcome::HeldBack { summarised: queue.summarised, total: queue.segments_total };
    }
    if let summary::DailyState::Unreadable(note) = &queue.daily {
        // A file that exists and is not a day summary is not this pass's to overwrite: something else
        // wrote there, and only the user can say which of the two is real.
        return DailyOutcome::Failed(format!("refusing to overwrite it — {note}"));
    }
    if matches!(queue.daily, summary::DailyState::Current(_)) && !options.force {
        return DailyOutcome::UpToDate;
    }
    if leg_closed {
        // Named as the leg's own decision, because the sentence a user reads has to say why nothing was
        // tried: the day is not broken, the endpoint stopped answering and this pass stopped asking.
        return DailyOutcome::Failed(format!(
            "not asked — this pass had already stopped asking, because the endpoint sent nothing back for {GIVE_UP_AFTER_SILENT_WAVES} waves in a row"
        ));
    }
    let request = daily_request(prompts, queue, &periods.entries);
    if options.dry_run {
        return DailyOutcome::Planned { chars: request.chars };
    }
    let completion = match ask(client, &request) {
        Ok(completion) => completion,
        Err(why) => return DailyOutcome::Failed(why.to_string()),
    };
    let row = summary::DaySummary {
        date: queue.day.clone(),
        text: completion.text.trim().to_string(),
        coverage: queue.coverage.clone(),
        partial: !queue.gate_open(),
        written_at: summary::now_stamp(),
        written_by: "windai".to_string(),
        model: config.str_or("open_ai_modelname", ""),
        source_fingerprint: summary::daily_inputs_for(config, queue),
        stale: false,
        prompt_fingerprint: digests.daily.clone(),
    };
    match summary::write_daily(config, &row) {
        Ok(_) => DailyOutcome::Written,
        Err(why) => DailyOutcome::Failed(why.to_string()),
    }
}

/// The days a run will visit, oldest first.
fn select_days(
    index: &Index,
    options: &Options,
    reader: &summary::Reader,
    digests: &summary::PromptDigests,
) -> Result<Vec<String>, AiError> {
    let shift = index.config.day_begin_minutes();
    if let Some(day) = &options.day {
        if summary::day_span(day, shift).is_none() {
            return Err(index.faults().usage(format!("`{day}` is not a day; expected YYYY-MM-DD")));
        }
        return Ok(vec![day.clone()]);
    }
    let want = options.pending.unwrap_or(1).max(1);
    let today = wind_base::clock::now();
    let mut cursor = summary::day_of(today.naive_epoch_seconds(), shift);
    let mut out: Vec<String> = Vec::new();
    for _ in 0..PENDING_SCAN_DAYS {
        let queue = summary::for_day_with(reader, &cursor, digests).map_err(|e| index.faults().store(&e))?;
        if queue.has_work() {
            out.push(cursor.clone());
            if out.len() == want {
                break;
            }
        }
        cursor = summary::day_of(queue.span.from - 1, shift);
    }
    out.reverse();
    Ok(out)
}

/// What a run did, as the CLI's text report.
pub fn render_report(report: &Report) -> String {
    let mut out = String::new();
    if report.dry_run {
        out.push_str("dry run: nothing was sent and nothing was written\n");
    }
    for day in &report.days {
        out.push_str(&format!(
            "{}  {} stretches, {} asked, {} written, {} already current\n",
            day.date, day.stretches, day.asked, day.written, day.cached
        ));
        for failure in &day.failed {
            out.push_str(&format!("   ! {failure}\n"));
        }
        out.push_str(&format!("   day summary: {}\n", describe(&day.daily)));
        if day.chars_sent > 0 {
            out.push_str(&format!("   sent: {} characters\n", day.chars_sent));
        }
    }
    // What the leg did about the endpoint, said once rather than per day, because the throttle and the
    // give-up are decisions for the whole pass. Neither line repeats a count from the closing sentence.
    if report.pacing.throttled {
        out.push_str(&format!(
            "the endpoint said it was busy, so this pass asked {} at a time from there on\n",
            IN_FLIGHT_AFTER_BUSY
        ));
    }
    if report.pacing.given_up {
        out.push_str(&format!(
            "the endpoint sent nothing back for {} waves in a row, so this leg stopped asking; {} stretch(es) were not asked and are still owed\n",
            GIVE_UP_AFTER_SILENT_WAVES, report.pacing.left_unasked
        ));
    }
    out.push_str(&format!(
        "\n{} request(s), {} written, {} current without asking, {} failed, {} characters total\n",
        report.sent, report.written, report.cached, report.failed, report.chars_sent
    ));
    out
}

fn describe(outcome: &DailyOutcome) -> String {
    match outcome {
        DailyOutcome::HeldBack { summarised, total } => {
            format!("held back — {summarised} of {total} stretches are summarised, and --allow-partial was not given")
        }
        DailyOutcome::UpToDate => "already current, so no request was made".to_string(),
        DailyOutcome::Planned { chars } => format!("planned, {chars} characters would be sent"),
        DailyOutcome::Written => "written".to_string(),
        DailyOutcome::Failed(why) => format!("failed — {why}"),
        DailyOutcome::NothingRecorded => "the day holds no recorded stretch".to_string(),
    }
}

/// The JSON form, for `--json`.
pub fn json(report: &Report) -> Value {
    json!({
        "dry_run": report.dry_run,
        "sent": report.sent,
        "written": report.written,
        "cached": report.cached,
        "failed": report.failed,
        "chars_sent": report.chars_sent,
        "pacing": {
            "waves": report.pacing.waves,
            "in_flight": if report.pacing.throttled { IN_FLIGHT_AFTER_BUSY } else { IN_FLIGHT },
            "throttled": report.pacing.throttled,
            "silent_waves": report.pacing.silent_waves,
            "given_up": report.pacing.given_up,
            "left_unasked": report.pacing.left_unasked,
        },
        "days": report.days.iter().map(|day| json!({
            "date": day.date,
            "stretches": day.stretches,
            "asked": day.asked,
            "written": day.written,
            "already_current": day.cached,
            "failed": day.failed,
            "chars_sent": day.chars_sent,
            "daily": describe(&day.daily),
        })).collect::<Vec<Value>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::test_support as support;
    use std::path::{Path, PathBuf};
    use wind_base::prompts::validate;
    use wind_summary::test_support as corpus;

    const DAY: &str = "2026-09-27";

    /// A throwaway install pointed at a canned endpoint, with one product day indexed in it.
    ///
    /// `long` is how many characters the second frame's screen text carries, so one fixture serves both
    /// the ordinary case and the "does anything here clip what it sends" case.
    fn fixture(tag: &str, long: usize, replies: Vec<(u16, String)>) -> (PathBuf, support::Canned, Index) {
        fixture_with(tag, json!({}), long, replies)
    }

    /// As [`fixture`], with `extra` written over the shipped defaults — how a test reaches `lang`.
    fn fixture_with(tag: &str, extra: Value, long: usize, replies: Vec<(u16, String)>) -> (PathBuf, support::Canned, Index) {
        let server = support::Canned::start(replies);
        let mut config = json!({ "open_ai_base_url": server.base_url(), "open_ai_api_key": "sk-somebody-elses-key" });
        for (key, value) in extra.as_object().expect("extra overrides must be a JSON object") {
            config[key] = value.clone();
        }
        let root = support::install(&format!("summarize-{tag}"), &config);
        let text: &'static str =
            Box::leak("屏幕上的长文本 ".repeat(long / 8 + 1).chars().take(long).collect::<String>().into_boxed_str());
        corpus::seed_month(
            &root,
            "default",
            &[
                (corpus::at("2026-09-27_09-00-00"), "2026-09-27_09-00-00.mp4", "Excel — Q3", "quarterly forecast sheet"),
                (corpus::at("2026-09-27_09-01-00"), "2026-09-27_09-00-00.mp4", "Excel — Q3", text),
                (corpus::at("2026-09-27_10-00-00"), "2026-09-27_10-00-00.mp4", "WeChat", "chat with 张伟"),
            ],
        );
        let index = Index::open(&root).expect("the fixture install opens");
        (root, server, index)
    }

    /// Both requests of one fixture install, the stretch's and the day's, so a promise about the wording
    /// that leaves the machine is checked on the two templates that carry it rather than on one.
    fn both_requests(index: &Index) -> (Request, Request) {
        let segment = summary::Reader::fresh(&index.config).of_day(DAY).expect("read").segments.remove(0);
        let period = period_request(&index.settings.prompts, &segment);
        let queue = summary::for_day_with(&summary::Reader::fresh(&index.config), DAY, &summary::PromptDigests::unknown()).expect("queue");
        (period, daily_request(&index.settings.prompts, &queue, &BTreeMap::new()))
    }

    fn canned(text: &str) -> Vec<(u16, String)> {
        vec![(200, support::completion_body(text))]
    }

    /// An install with `count` stretches on the product day, ten minutes apart, each its own file.
    ///
    /// A pass that keeps four requests in flight cannot be read in a two-stretch install — the whole queue
    /// is one wave, and a wave tells you nothing about wave widths. Five rows is the smallest fixture that
    /// has a second wave; thirteen has a fourth.
    fn fixture_stretches(tag: &str, count: usize, replies: Vec<(u16, String)>) -> (PathBuf, support::Canned, Index) {
        let server = support::Canned::start(replies);
        let config = json!({ "open_ai_base_url": server.base_url(), "open_ai_api_key": "sk-somebody-elses-key" });
        let root = support::install(&format!("summarize-{tag}"), &config);
        let rows: Vec<(i64, String, String, String)> = (0..count)
            .map(|i| {
                let minutes = 9 * 60 + i * 10;
                let stamp = format!("2026-09-27_{:02}-{:02}-00", minutes / 60, minutes % 60);
                (corpus::at(&stamp), format!("{stamp}.mp4"), "Qoder".to_string(), format!("the screen of stretch {i}"))
            })
            .collect();
        corpus::seed_month(&root, "default", &rows);
        let index = Index::open(&root).expect("the fixture install opens");
        (root, server, index)
    }

    /// Which stretch request `index` was about, read out of the rendered prompt — it names its own segment.
    /// `None` for a request that is not a stretch ask (the day's own request names no segment).
    ///
    /// The lanes arrive in whatever order the threads took, so a test about four requests cannot point at
    /// `request(0)` and mean the first stretch. The request itself says which stretch it is, and the answer
    /// it got is `replies[index]` — which is how an answer is matched back to its stretch here.
    fn segment_key_of(server: &support::Canned, index: usize) -> Option<String> {
        let user = server.request_json(index)["messages"][1]["content"].as_str().expect("the user turn").to_string();
        let mut head = user.split_whitespace();
        match (head.next(), head.next()) {
            // The template writes `Segment {key}: `, so the colon comes along with the key.
            (Some("Segment"), Some(key)) => Some(key.trim_end_matches(':').to_string()),
            _ => None,
        }
    }

    /// Where in what the listener logged the pass asked about one named stretch.
    fn request_of(server: &support::Canned, key: &str) -> usize {
        (0..server.request_count())
            .find(|i| segment_key_of(server, *i).as_deref() == Some(key))
            .unwrap_or_else(|| panic!("no request about {key} arrived"))
    }

    fn segments(index: &Index) -> Vec<summary::Segment> {
        summary::Reader::fresh(&index.config).of_day(DAY).expect("read").segments
    }

    fn release(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_frame_block_carries_time_title_url_and_the_whole_text() {
        let frame = summary::Frame {
            timestamp: corpus::at("2026-09-27_09-00-00"),
            title: Some("Excel — Q3".into()),
            url: Some("https://example.test/x".into()),
            text: "first line\nsecond line".into(),
        };
        let block = frame_block(&frame);
        assert!(
            block.starts_with("[09:00:00] window: Excel — Q3\n           link:   https://example.test/x\n           text:\nfirst line\nsecond line"),
            "{block}"
        );
        let bare = summary::Frame { timestamp: frame.timestamp, title: None, url: None, text: "x".into() };
        let block = frame_block(&bare);
        assert!(block.contains("‹no title captured›"), "an absent title is said, not printed blank: {block}");
        assert!(!block.contains("link:"), "an absent URL takes no line: {block}");
    }

    #[test]
    fn the_stretch_request_names_the_segment_and_ends_with_every_frame() {
        let (root, _server, index) = fixture("request", 40, canned("unused"));
        let segment = segments(&index).remove(0);
        let request = period_request(&index.settings.prompts, &segment);
        assert!(request.system.contains("one stretch of someone's recorded screen"), "{}", request.system);
        assert!(request.user.starts_with("Segment 2026-09-27_09-00-00: "), "{}", request.user);
        assert!(request.user.contains("(1m0s), 2 frames"), "span and count come from the index: {}", request.user);
        assert!(request.user.contains("quarterly forecast sheet"));
        assert_eq!(request.user.matches("window: ").count(), 2, "one block per frame");
        assert_eq!(request.chars, request.system.chars().count() + request.user.chars().count());
        release(&root);
    }

    /// The promise the whole feature is built on: what goes out is the captured text, uncut.
    #[test]
    fn a_long_frame_is_sent_whole_rather_than_trimmed_to_something_cheaper() {
        let (root, _server, index) = fixture("whole", 12_000, canned("unused"));
        let segment = segments(&index).into_iter().find(|s| s.key == "2026-09-27_09-00-00").expect("segment");
        assert!(segment.ocr_chars > 11_000, "the fixture really is long: {}", segment.ocr_chars);
        let request = period_request(&index.settings.prompts, &segment);
        assert!(request.chars > 12_000, "so the request is at least as long: {}", request.chars);
        let marker = "屏幕上的长文本";
        assert_eq!(request.user.matches(marker).count(), segment.detail[1].text.matches(marker).count(), "every repetition survives");
        release(&root);
    }

    #[test]
    fn a_run_writes_every_stretch_then_the_day_and_asks_for_each_thing_once() {
        let (root, server, index) = fixture("run", 20, canned("a paragraph about the screen."));
        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");

        assert_eq!(report.days.len(), 1, "{report:?}");
        let day = &report.days[0];
        assert_eq!(day.stretches, 2);
        assert_eq!(day.written, 2, "both stretches in one pass: {:?}", day.failed);
        assert_eq!(day.daily, DailyOutcome::Written, "and the day, once its gate opened");
        assert_eq!(server.request_count(), 3, "two stretches plus one day");
        assert!(day.chars_sent > 0);

        // Both stretches are in flight at once, so which of them reached the listener first is the lanes'
        // business; the claim is that the one about 09:00 carried its own frames whole.
        let morning = request_of(&server, "2026-09-27_09-00-00");
        let first = server.request(morning);
        assert!(first.contains("quarterly forecast sheet"), "the text went out whole: {first}");
        assert!(first.contains("Segment 2026-09-27_09-00-00"));
        let daily = server.request(2);
        assert!(daily.contains("a paragraph about the screen."), "the day is written from the stretch paragraphs");
        assert!(daily.contains("09:00:00-09:01:00"), "each paragraph is labelled with its span: {daily}");
        assert!(!daily.contains("quarterly forecast sheet"), "the day request carries paragraphs, not raw frames");

        let stored = summary::read_period(&index.config, DAY);
        assert_eq!(stored.len(), 2);
        let entry = stored.get("2026-09-27_09-00-00").expect("entry");
        assert_eq!(entry.written_by, "windai");
        assert_eq!(entry.frames, 2, "the numbers came from the index, not from the answer");
        // Which of the two stretches the endpoint answered first is the lanes' decision, not the pass's, so
        // the pairing is asserted the other way round: the stretch whose request arrived first is the one
        // standing with the answer that was served first.
        let arrived_first = segment_key_of(&server, 0).expect("a stretch request");
        assert_eq!(stored.get(&arrived_first).expect("entry").text, "a paragraph about the screen.", "the first answer went to the first request");
        let row = summary::read_daily(&index.config, DAY).summary.expect("daily");
        assert!(!row.partial);
        assert_eq!(row.coverage.segments_summarised, 2);
        assert!(row.coverage.missing.is_empty());
        release(&root);
    }

    #[test]
    fn the_second_run_asks_for_nothing_because_nothing_changed() {
        let (root, server, index) = fixture("cached", 20, canned("a paragraph."));
        let client = Client::new(index.settings.clone());
        let options = Options { day: Some(DAY.into()), ..Default::default() };
        run(&index, &client, &options).expect("first");
        let before = server.request_count();
        let again = run(&index, &client, &options).expect("second");
        assert_eq!(server.request_count(), before, "nothing changed, so nothing was asked again");
        assert_eq!(again.days[0].asked, 0);
        assert_eq!(again.days[0].daily, DailyOutcome::UpToDate);
        assert_eq!(again.days[0].cached, 2);

        let forced = run(&index, &client, &Options { force: true, ..options.clone() }).expect("forced");
        assert_eq!(forced.days[0].asked, 2, "--force re-asks what the queue is content with");
        assert_eq!(forced.days[0].daily, DailyOutcome::Written);
        release(&root);
    }

    #[test]
    fn a_day_that_cannot_be_finished_is_held_back_and_says_the_count() {
        let (root, _server, index) = fixture("held", 20, canned("only one answer"));
        let one = segments(&index).remove(1);
        summary::write_period(
            &index.config,
            &one.day,
            &one.key,
            &summary::PeriodSummary {
                text: "the chat".into(),
                start: one.start,
                end: one.end,
                frames: one.frames,
                ocr_chars: one.ocr_chars,
                written_at: summary::now_stamp(),
                written_by: "test".into(),
                model: String::new(),
                source_fingerprint: one.fingerprint.clone(),
                prompt_fingerprint: String::new(),
            },
        )
        .expect("write");
        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), limit: Some(1), ..Default::default() }).expect("run");
        let day = &report.days[0];
        assert_eq!(day.daily, DailyOutcome::HeldBack { summarised: 1, total: 2 }, "{day:?}");
        let text = describe(&day.daily);
        assert!(text.contains("1 of 2") && text.contains("--allow-partial"), "{text}");
        assert_eq!(
            summary::read_period(&index.config, DAY).get("2026-09-27_10-00-00").expect("kept").written_by,
            "test",
            "a held-back day leaves the one paragraph that was already there"
        );

        let partial =
            run(&index, &client, &Options { day: Some(DAY.into()), allow_partial: true, limit: Some(0), ..Default::default() })
                .expect("run");
        assert_eq!(partial.days[0].daily, DailyOutcome::Written, "and the flag the message names is the one that does it");
        let row = summary::read_daily(&index.config, DAY).summary.expect("row");
        assert!(row.partial);
        assert_eq!(
            row.coverage.missing,
            vec!["2026-09-27_10-00-00".to_string()],
            "the gap is stored with the text: the paragraph written by hand predates this prompt, so the              day is incomplete even though both stretches have text on disk"
        );
        release(&root);
    }

    #[test]
    fn a_dry_run_reports_the_cost_and_sends_and_writes_nothing() {
        let (root, server, index) = fixture("dry", 500, canned("should never be asked"));
        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), dry_run: true, ..Default::default() }).expect("run");
        assert_eq!(server.request_count(), 0, "a dry run that bills is not a dry run");
        assert!(report.pacing.waves.is_empty(), "and it opened no wave at all: {:?}", report.pacing);
        assert_eq!(report.written, 0);
        assert!(report.chars_sent > 1_000, "the cost is still reported: {}", report.chars_sent);
        assert_eq!(report.days[0].asked, 2, "and it says what would have been asked");
        assert_eq!(report.days[0].daily, DailyOutcome::HeldBack { summarised: 0, total: 2 }, "nothing was written, so no day either");
        assert!(summary::read_period(&index.config, DAY).absent());
        let printed = render_report(&report);
        assert!(printed.contains("dry run"), "{printed}");
        assert!(printed.contains("characters total"), "{printed}");
        assert!(json(&report)["days"][0]["daily"].as_str().expect("json line").contains("held back"));
        release(&root);
    }

    #[test]
    fn pending_walks_back_to_days_that_still_have_work() {
        let (root, _server, index) = fixture("pending", 20, canned("x"));
        // The same digests the run itself computes, or the queue would report every paragraph as written
        // under other words and the day would never stop being work.
        let prompts = &index.settings.prompts;
        let digests = summary::PromptDigests::of(&prompts.period_system, &prompts.period_user, &prompts.daily_system, &prompts.daily_user);
        let reader = summary::Reader::fresh(&index.config);
        let days = select_days(&index, &Options { pending: Some(2), ..Default::default() }, &reader, &digests).expect("days");
        assert_eq!(days, vec![DAY.to_string()], "one day in this fixture holds work");

        let client = Client::new(index.settings.clone());
        run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("finish it");
        let after = select_days(&index, &Options { pending: Some(1), ..Default::default() }, &reader, &digests).expect("days");
        assert!(!after.contains(&DAY.to_string()), "the queue no longer offers a day that is done: {after:?}");

        let bad = select_days(&index, &Options { day: Some("2026-13-99".into()), ..Default::default() }, &reader, &digests);
        assert!(matches!(&bad, Err(e) if e.to_string().contains("YYYY-MM-DD")), "{bad:?}");
        release(&root);
    }

    #[test]
    fn an_endpoint_failure_is_a_line_in_the_report_and_not_the_end_of_the_run() {
        let (root, _server, index) = fixture(
            "fail",
            20,
            vec![(500, "the endpoint is having a day".into()), (500, "again".into()), (500, "and again".into())],
        );
        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");
        assert_eq!(report.days[0].written, 0);
        assert_eq!(report.days[0].failed.len(), 2, "one named line per stretch: {:?}", report.days[0].failed);
        assert!(report.days[0].failed[0].starts_with("2026-09-27_09-00-00: "), "{:?}", report.days[0].failed);
        assert!(report.days[0].failed[0].contains("500"), "the endpoint's own status is quoted: {}", report.days[0].failed[0]);
        assert!(matches!(report.days[0].daily, DailyOutcome::HeldBack { .. }), "the day is held back, not half-written");
        assert_eq!(report.failed, 2, "the two stretches; a held-back day is not a failure");
        assert!(summary::read_period(&index.config, DAY).absent(), "a failed run writes nothing");
        release(&root);
    }

    #[test]
    fn four_requests_are_in_flight_at_once_and_every_answer_lands_on_its_own_stretch() {
        let paragraphs: Vec<String> = (0..4).map(|i| format!("the paragraph written for arrival {i} of four.")).collect();
        let mut replies: Vec<(u16, String)> = paragraphs.iter().map(|p| (200u16, support::completion_body(p))).collect();
        replies.push((200, support::completion_body("the day as a whole.")));
        let (root, server, index) = fixture_stretches("four-in-flight", 4, replies);
        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");

        assert_eq!(report.pacing.waves, vec![IN_FLIGHT], "one wave of four, not four waves of one: {:?}", report.pacing);
        assert_eq!(report.sent, 4, "four on the wire");
        assert_eq!(report.written, 4, "{:?}", report.days[0].failed);
        assert_eq!(server.request_count(), 5, "four stretches and then the day, once its gate opened");

        // The pairing, read off the wire rather than assumed. Whatever order the four lanes reached the
        // listener in, the reply to request `i` is the paragraph standing under the key request `i` named.
        // Four lanes writing one day file at once is also the reason `write_period` takes both its gates:
        // four paragraphs in, four of them still there.
        let stored = summary::read_period(&index.config, DAY);
        assert_eq!(stored.len(), 4, "no lane's paragraph was overwritten by another's");
        for arrival in 0..4 {
            let key = segment_key_of(&server, arrival).expect("a stretch request");
            let entry = stored.get(&key).unwrap_or_else(|| panic!("request {arrival} was about {key}"));
            assert_eq!(entry.text, paragraphs[arrival], "the answer to {key}'s own request is what stands for {key}");
        }
        let distinct: BTreeSet<&String> = stored.entries.values().map(|entry| &entry.text).collect();
        assert_eq!(distinct.len(), 4, "no paragraph was reused, and no two stretches were merged into one");
        release(&root);
    }

    /// ADR 四.3: the endpoint said `429`, so this pass never again opens more than two.
    #[test]
    fn a_busy_answer_drops_the_rest_of_this_pass_to_two_at_a_time_and_the_next_pass_starts_at_four_again() {
        let mut replies: Vec<(u16, String)> = vec![(429, "rate limited, come back later".to_string())];
        replies.extend((0..8).map(|_| (200u16, support::completion_body("a paragraph."))));
        let (root, server, index) = fixture_stretches("busy-throttle", 7, replies);
        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");

        assert!(report.pacing.throttled, "the endpoint answered 429: {:?}", report.pacing);
        assert_eq!(report.pacing.waves, vec![4, 2, 1], "four to open with, then two for the rest of this pass");
        assert_eq!(report.written, 6, "the throttle costs width, not the work: {:?}", report.days[0].failed);
        assert_eq!(report.failed, 1, "the one stretch that was refused, and nothing else");
        assert_eq!(server.request_count(), 7, "every stretch asked once, and no day ask over the gap the refusal left");
        let printed = render_report(&report);
        assert!(printed.contains(&format!("asked {IN_FLIGHT_AFTER_BUSY} at a time")), "the user is told: {printed}");

        // The next pass is a different pass: it opens wide again whatever this one learned.
        let again = run(&index, &client, &Options { day: Some(DAY.into()), force: true, ..Default::default() }).expect("a second pass");
        assert!(!again.pacing.throttled, "no busy answer reached this one: {:?}", again.pacing);
        assert_eq!(again.pacing.waves, vec![4, 3], "and it starts at four again");
        release(&root);
    }

    /// ADR 四.4: four silent requests are one event, so one bad wave does not close the leg.
    #[test]
    fn a_wave_of_four_that_all_came_back_silent_is_one_failure_of_the_endpoint_and_not_four() {
        let mut replies: Vec<(u16, String)> = vec![(0, String::new()); 4];
        replies.extend((0..5).map(|_| (200u16, support::completion_body("a paragraph."))));
        let (root, server, index) = fixture_stretches("silent-wave", 8, replies);
        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");

        assert_eq!(report.pacing.silent_waves, 1, "one wave came back with nothing in it, and that is one: {:?}", report.pacing);
        assert!(!report.pacing.given_up, "one silent wave does not close the leg");
        assert_eq!(report.pacing.waves, vec![4, 4], "so the second wave went out as asked");
        assert_eq!(server.request_count(), 8, "nothing was skipped after the silent wave");
        assert_eq!(report.written, 4, "the four that did answer");
        assert_eq!(report.failed, 4, "and each of the silent ones still owes its own stretch");
        let failed = &report.days[0].failed;
        assert_eq!(failed.len(), 4, "{failed:?}");
        for line in failed {
            assert!(line.contains("network — "), "a request that never came back is the network's own sentence: {line}");
        }
        release(&root);
    }

    /// ADR 四.4, the other side of the same rule: three silent waves *is* the endpoint gone quiet.
    #[test]
    fn three_silent_waves_in_a_row_close_the_leg_and_the_stretches_after_them_are_never_asked() {
        let mut replies: Vec<(u16, String)> = vec![(0, String::new()); 12];
        replies.push((200, support::completion_body("never reached")));
        let (root, server, index) = fixture_stretches("leg-closed", 13, replies);
        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");

        assert!(report.pacing.given_up, "three waves of nothing: {:?}", report.pacing);
        assert_eq!(report.pacing.silent_waves, GIVE_UP_AFTER_SILENT_WAVES);
        assert_eq!(report.pacing.waves, vec![4, 4, 4], "and the pass stopped opening waves");
        assert_eq!(server.request_count(), 12, "the thirteenth stretch was never asked, and neither was the day");
        assert_eq!(report.sent, 12);
        assert_eq!(report.pacing.left_unasked, 1);
        assert_eq!(report.failed, 13, "twelve silent and one unasked — all of them still owed");
        assert_eq!(report.written, 0);
        let printed = render_report(&report);
        assert!(printed.contains("1 stretch(es) were not asked and are still owed"), "{printed}");
        assert!(printed.contains("current without asking, 13 failed"), "the closing sentence `windmaint` reads: {printed}");
        assert!(report.days[0].failed.iter().any(|line| line.contains("not asked")), "{:?}", report.days[0].failed);
        assert_eq!(summary::read_period(&index.config, DAY).len(), 0, "a closed leg writes nothing");
        release(&root);
    }

    /// ADR 四.6: the paragraph may have arrived from the other producer while this request was timing out.
    #[test]
    fn a_request_that_never_came_back_counts_as_done_when_the_day_file_already_holds_its_paragraph() {
        let mut replies: Vec<(u16, String)> = vec![(0, String::new()); 4];
        replies.push((200, support::completion_body("the fifth stretch's own paragraph.")));
        let (root, mut server, index) = fixture_stretches("disk-first", 5, replies);
        // The oldest stretch is in the first wave by construction, so its request is one of the four that
        // will come back silent — and while one of them is in flight, the bridge files its paragraph.
        let answered = segments(&index).remove(0);
        let (config, key, segment) = (index.config.clone(), answered.key.clone(), answered.clone());
        let digests = digests(&index.settings.prompts);
        server.add_effect(0, std::sync::Arc::new(move || {
            summary::write_period(
                &config,
                DAY,
                &key,
                &summary::PeriodSummary {
                    text: "written next door while this pass was still waiting.".into(),
                    start: segment.start,
                    end: segment.end,
                    frames: segment.frames,
                    ocr_chars: segment.ocr_chars,
                    written_at: summary::now_stamp(),
                    written_by: "windmcp".into(),
                    model: String::new(),
                    source_fingerprint: segment.fingerprint.clone(),
                    prompt_fingerprint: digests.period.clone(),
                },
            )
            .expect("the other producer can write the day file too");
        }));

        let client = Client::new(index.settings.clone());
        let report = run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");
        let day = &report.days[0];

        assert!(!day.failed.iter().any(|line| line.starts_with(&answered.key)), "{:?} — its paragraph is on disk", day.failed);
        assert_eq!(day.failed.len(), 3, "the three the disk cannot account for: {:?}", day.failed);
        assert_eq!(report.failed, 3, "so the failure count `windmaint` reads does not claim a fourth");
        assert_eq!(day.written, 1, "this pass wrote only the fifth stretch");
        assert_eq!(day.cached, 2, "the queue counts both standing paragraphs: the bridge's and this pass's own");
        let stored = summary::read_period(&index.config, DAY);
        let entry = stored.get(&answered.key).expect("the bridge's paragraph");
        assert_eq!(entry.written_by, "windmcp", "and it was left standing, not asked for a second time");
        assert_eq!(report.pacing.silent_waves, 1, "the wave was still a silent one: {:?}", report.pacing);
        assert_eq!(server.request_count(), 5, "four in the first wave, one in the second, and no day ask over a gap");
        release(&root);
    }

    #[test]
    fn the_day_request_names_the_rule_that_decided_which_frames_were_in_it() {
        let (root, _server, index) = fixture("daily-request", 20, canned("x"));
        let digests = summary::PromptDigests::unknown();
        let queue = summary::for_day_with(&summary::Reader::fresh(&index.config), DAY, &digests).expect("queue");
        let request = daily_request(&index.settings.prompts, &queue, &BTreeMap::new());
        assert!(request.user.contains("Product day 2026-09-27"), "{}", request.user);
        assert!(request.user.contains("03:00"), "the day start is stated in the request itself: {}", request.user);
        assert!(request.user.contains("Recorded segments: 2. Summarised: 0."), "{}", request.user);
        assert!(request.system.contains("journal entry"), "{}", request.system);
        release(&root);
    }

    #[test]
    fn a_user_prompt_edit_changes_what_is_sent_and_what_is_recorded() {
        // The editable prompt is not decoration: the text that goes out is the file, and each entry says
        // which words produced it — which is what later puts that stretch back in the queue.
        let (root, _server, _index) = fixture("prompt-edit", 20, canned("a paragraph."));
        let dir = root.join("userdata").join("ai_prompts");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("period_summary_system.txt"), "Summarise this stretch for me.\n").expect("write");
        std::fs::write(dir.join("period_summary_user.txt"), "The frames:\n{frames_table}\n").expect("write");

        let index = Index::open(&root).expect("reopen with the override");
        assert!(index.settings.prompts.period_system.starts_with("Summarise this stretch"), "{:?}", index.settings.prompts.period_system);
        let client = Client::new(index.settings.clone());
        run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");

        let stored = summary::read_period(&index.config, DAY);
        let entry = stored.get("2026-09-27_09-00-00").expect("entry");
        assert!(!entry.prompt_fingerprint.is_empty(), "the entry records which words wrote it");
        let defaults = summary::PromptDigests::of(
            Name::PeriodSystem.embedded(),
            Name::PeriodUser.embedded(),
            Name::DailySystem.embedded(),
            Name::DailyUser.embedded(),
        );
        assert_ne!(entry.prompt_fingerprint, defaults.period, "and it is not mistaken for the shipped prompt's output");
        let edited = summary::PromptDigests::of("Summarise this stretch for me.\n", "The frames:\n{frames_table}\n", "", "");
        assert_eq!(entry.prompt_fingerprint, edited.period, "the digest is of the effective text");
        release(&root);
    }

    /// Every `lang` this install can be set to, checked on both requests that carry the slot.
    ///
    /// The phrases are written out here rather than imported from `wind-base`, because this is the promise
    /// a user sees: these are the words that leave the machine.
    #[test]
    fn both_requests_ask_for_the_language_the_interface_is_set_to() {
        for (lang, phrase) in [("sc", "Chinese (Simplified Han)"), ("ja", "Japanese"), ("en", "English")] {
            let (root, _server, index) = fixture_with(&format!("lang-{lang}"), json!({ "lang": lang }), 20, canned("unused"));
            let (period, daily) = both_requests(&index);
            for (which, request) in [("stretch", &period), ("day", &daily)] {
                assert!(
                    request.system.contains(&format!("One paragraph in {phrase}, three to six sentences")),
                    "the {which} request of a {lang} install asks for {phrase:?}: {}",
                    request.system
                );
            }
            release(&root);
        }
    }

    /// An install that never named a `lang` is the English case, not a fourth answer.
    #[test]
    fn an_install_that_never_named_a_lang_behaves_exactly_like_the_english_one() {
        let (root_named, _server, named) = fixture_with("lang-explicit-en", json!({ "lang": "en" }), 20, canned("unused"));
        let written = both_requests(&named);

        // The shipped defaults do name `lang`, so the absent case has to have the key taken out of both
        // files — a fixture that merely does not override it is the shipped default again.
        let (root_bare, _server, _) = fixture("lang-absent", 20, canned("unused"));
        for file in [root_bare.join("config_src/config_default.json"), root_bare.join("userdata/config_user.json")] {
            let body: Value = serde_json::from_str(&std::fs::read_to_string(&file).expect("config")).expect("a JSON config");
            let mut object = body.as_object().expect("an object").clone();
            assert!(object.remove("lang").is_some(), "{file:?} carried no `lang` to remove");
            std::fs::write(&file, serde_json::to_string(&Value::Object(object)).expect("serialises")).expect("write");
        }
        let index = Index::open(&root_bare).expect("reopen without the key");
        assert_eq!(index.settings.prompts.language, "English", "the codebase default is `en`");
        let bare = both_requests(&index);
        assert_eq!(bare.0.system, written.0.system, "the stretch request cannot tell the two apart");
        assert_eq!(bare.1.system, written.1.system, "nor the day request");
        release(&root_named);
        release(&root_bare);
    }

    /// What the slot must never do is hand the model a code.
    #[test]
    fn the_slot_names_a_language_in_words_and_never_leaks_the_locale_code() {
        let codes = ["en", "sc", "ja"];
        for lang in codes {
            let (root, _server, index) = fixture_with(&format!("lang-leak-{lang}"), json!({ "lang": lang }), 20, canned("unused"));
            let (period, daily) = both_requests(&index);
            for (which, request) in [("stretch", &period), ("day", &daily)] {
                let text = format!("{}{}", request.system, request.user);
                let leaked: Vec<String> = text
                    .split(|c: char| !c.is_alphabetic())
                    .map(|word| word.to_ascii_lowercase())
                    .filter(|word| codes.contains(&word.as_str()))
                    .collect();
                assert!(leaked.is_empty(), "a {lang} install put {leaked:?} in the {which} request: {text}");
                assert!(!text.contains("{language}"), "and no unfilled slot either: {text}");
            }
            release(&root);
        }
    }

    /// The escape hatch the slot scheme keeps: `{language}` is a default for the sentence, not a switch
    /// over the user's file. A prompt that names its own language is sent as written, and `validate` has
    /// nothing to say about it.
    #[test]
    fn a_prompt_that_names_its_own_language_is_sent_exactly_as_written() {
        let (root, server, _index) = fixture_with("lang-verbatim", json!({ "lang": "sc" }), 20, canned("a paragraph."));
        let mine = "One paragraph in Brazilian Portuguese, three to six sentences, and nothing else.\n";
        assert!(validate(Name::PeriodSystem, mine).is_ok(), "naming no slot is not an error");
        let dir = root.join("userdata").join("ai_prompts");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("period_summary_system.txt"), mine).expect("write");

        let index = Index::open(&root).expect("reopen with the override");
        assert_eq!(index.settings.prompts.language, "Chinese (Simplified Han)", "the install still speaks sc");
        let client = Client::new(index.settings.clone());
        run(&index, &client, &Options { day: Some(DAY.into()), ..Default::default() }).expect("run");

        let stretch = server.request(0);
        assert!(stretch.contains("One paragraph in Brazilian Portuguese"), "the user's words went out whole: {stretch}");
        assert!(!stretch.contains("Chinese (Simplified Han)"), "and the derived phrase did not: {stretch}");
        // The other half of the same install, unedited: the day template still carries the slot, so it is
        // filled. What you edit is what runs — and what you do not edit runs as this build ships it.
        assert!(server.request(2).contains("One paragraph in Chinese (Simplified Han)"), "the day follows lang: {}", server.request(2));
        release(&root);
    }
}
