//! The one file that answers "how far has this pass got?".
//!
//! A deferred pass is two processes: `windrec` spawns `windmaint`, and the window is a third party
//! that can see neither of them. Every other thing this product needs to say across a process
//! boundary it says through a file in `cache/locks`, so progress follows the same rule — the pass
//! publishes its own state, and a reader never has to guess or parse somebody else's log.
//!
//! Before this file existed the answer lived only in `cache/logs/windmaint-idle.log`, which is a
//! report for the person who is going to read it afterwards, not a state for the person watching a
//! button they just pressed: it is prose, it is appended across passes, and it has no notion of
//! "which step is open right now". The two buttons on the settings page therefore produced one toast
//! naming a request file and then said nothing more until the pass was over.
//!
//! # Why the writer is installed once per process
//!
//! The nine steps each take `&Config` and return their own `Outcome`; threading a progress handle
//! through eight signatures, every standalone `windmaint <step>` call site, and their tests would buy
//! nothing over installing it once in `run_pipeline`. So the pipeline installs it, and a step calls
//! [`add_items`] with what it just did and which of the four legs it did it for. Nothing installed means
//! nothing published, which is the right answer for `windmaint expire` run by hand: a single command has
//! no nine-step shape to report.
//!
//! # Who is allowed to say a pass is running
//!
//! Never this file alone. [`Pass::is_running`] cross-checks the published pid against the process
//! table, because a pass that was killed mid-step leaves the file saying `running` forever, and a
//! window that believed the file would show a spinner for a process that died an hour ago. The lock
//! directory holds the same answer from the other side ([`crate::fslock::directory_lock_claimed`]),
//! and a reader that disagrees with both has found a bug in one of them.
//!
//! # Why the bar is four counters and not one number out of nine
//!
//! `step 5 of 9` is an honest sentence about a queue of commands and a useless one about how far a
//! person is from a searchable library: step 5 (`reindex`) is the one that can hold a whole night and
//! report no items at all, while step 8 (`ai-summaries`) answers in seconds when the endpoint is awake
//! and in a quarter of an hour when it is not. The ADR that moved the pass onto four legs
//! (`docs/adr/2026-09-30-the-organise-pass-runs-on-four-legs.md`) therefore changed what is
//! *published*, not what is run: the nine steps stay nine, and the state becomes [`Pass::items_total`]
//! plus one [`LegCount`] per [`Leg`] — 文字识别 in pictures, 视频合成 in segments, AI 总结 in the
//! stretches the endpoint owes, 其他整理 in everything else the pass takes on.
//!
//! Three rules fall out of that and are enforced here rather than left to a reader:
//!
//!   * **Items only, never seconds.** No estimate, no weighting one leg's work against another's by
//!     duration; a leg's `done` is its own count and its `total` is a count of the same unit. What the
//!     machine is worth is not the person's question — whether tonight's footage will be searchable is.
//!   * **The denominators are fixed at the moment the pass starts**, from the same dry-run census the
//!     settings page counts with (`windmaint backlog`), and [`set_totals`] is answered once per pass: a
//!     census asked for again while a bar is moving is refused, because a denominator that grows under a
//!     running pass is a bar that recedes. Work recorded after the census therefore lands *past* its
//!     total: the row reaches its end and stays there, and what is left over is the next pass's census,
//!     not this one's moving target. The pass's own closing sentence says how much is still owed.
//!   * **A leg with nothing in it is not in the file at all** — an empty row is a bar that promises a
//!     queue nobody counted, and four rows of which three read `0 / 0` is the shape that asks to be
//!     explained. A leg that reported trouble is drawn whatever its counts: 本轮 0 件 means no queue to
//!     draw, not permission to hide a failure.
//!
//! # Why the old keys stay published
//!
//! The window shipped today reads `step`, `steps`, `step_name` and `items` and nothing else, and it
//! reads a file written by whichever `windmaint` is on disk when it asks. So [`Pass::encode`] keeps
//! writing those four keys, and keeps them meaning what they meant: [`begin_step`] still opens one of
//! nine, and [`add_items`] still feeds the open step's own number when the category it was handed
//! belongs to that step. The leg counters ride alongside rather than replacing them, which is what makes
//! a new binary and an old window a progress bar instead of a blank one.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often [`add_items`] is allowed to touch the disk.
///
/// A step can finish thousands of rows, and each one asking for a `Write` of a small file is IO that
/// buys a progress bar nothing renders at that resolution. One second is the window's own poll
/// interval, so a refresh can never be shown a staler number than the last one it asked for.
const PUBLISH_EVERY: Duration = Duration::from_secs(1);

/// How long one leg's reason is allowed to be.
///
/// A step's error string is a sentence written for a log, and the ADR asks the row for 一句人话: the
/// reason is a label beside a bar, read in the two seconds before the bar moves again. Long ones are cut
/// at a word, so a walk that names every month it failed in does not push the other three rows off screen.
const LEG_NOTE_MAX: usize = 160;

/// Cut a reason down to [`LEG_NOTE_MAX`] at a word boundary, on a char boundary always.
fn short(text: &str) -> String {
    let text = flatten(text);
    if text.chars().count() <= LEG_NOTE_MAX {
        return text;
    }
    let cut = text.char_indices().take(LEG_NOTE_MAX).map(|(at, _)| at).last().unwrap_or(0);
    let head = match text[..cut].rsplit_once(' ') {
        Some((head, _)) if !head.is_empty() => head,
        _ => &text[..cut],
    };
    format!("{head}…")
}

/// Who started this pass, which decides what is allowed to stop it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The `立刻整理` button: bounded by a stop request, not by the clock.
    Manual,
    /// The named window or the idle rule: bounded by both.
    Scheduled,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Manual => "manual",
            Kind::Scheduled => "scheduled",
        }
    }

    fn parse(text: &str) -> Option<Kind> {
        match text {
            "manual" => Some(Kind::Manual),
            "scheduled" => Some(Kind::Scheduled),
            _ => None,
        }
    }
}

/// Where the pass is. `Stopped` and `Failed` are both endings, and they are not the same sentence:
/// one was asked for, the other is why the log has to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Complete,
    Stopped,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Complete => "complete",
            State::Stopped => "stopped",
            State::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Option<State> {
        match text {
            "running" => Some(State::Running),
            "complete" => Some(State::Complete),
            "stopped" => Some(State::Stopped),
            "failed" => Some(State::Failed),
            _ => None,
        }
    }
}

/// One of the pass's four counters — the leg the ADR names, not the step that happens to be open.
///
/// The category is what a work item is told to, rather than being inferred from `begin_step`: once the
/// legs run at the same time (the ADR's second phase) the step that is open says nothing about which
/// counter a row on another thread belongs to, and a counter inferred from the wrong thread is the same
/// kind of bug as a progress bar that lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    /// 文字识别 — one picture the text step read a masked copy for. Unit: a picture.
    Text,
    /// 视频合成 — one slice `convert` was asked to encode. Unit: a segment.
    Convert,
    /// AI 总结 — one stretch the endpoint has not answered for. The only leg that waits on a network.
    Ai,
    /// 其他整理 — the pass's own work that is neither a picture, a segment nor a stretch: a segment
    /// re-compressed by the retention rule, a preview redrawn for a card. Unit: an item, which is why the
    /// census rather than this file decides what one is — and why `reindex`'s rows and `backup`'s month
    /// files report through [`add_step_items`] instead: the census counts those queues as videos and does
    /// not count month files at all, and a leg fed in one unit while denominatorated in another is a bar
    /// that lies.
    Other,
}

impl Leg {
    /// Every leg, in the order the rows are drawn in — which is also the order they are written and
    /// read back in, so a file and an in-memory pass compare equal instead of merely agreeing.
    pub const ALL: [Leg; 4] = [Leg::Text, Leg::Convert, Leg::Ai, Leg::Other];

    pub fn as_str(self) -> &'static str {
        match self {
            Leg::Text => "text",
            Leg::Convert => "convert",
            Leg::Ai => "ai",
            Leg::Other => "other",
        }
    }

    fn parse(text: &str) -> Option<Leg> {
        match text {
            "text" => Some(Leg::Text),
            "convert" => Some(Leg::Convert),
            "ai" => Some(Leg::Ai),
            "other" => Some(Leg::Other),
            _ => None,
        }
    }

    /// Which counter a step of the nine feeds, by the step's own published name.
    ///
    /// One function rather than a table per caller, because two answers to "whose total does this step
    /// move" is how a bar comes to count the same segment twice. `doctor`, `backlog`, `forget` and `all`
    /// name no leg: the first two change nothing, the third is not in the pipeline, and the fourth is
    /// the pipeline itself.
    pub fn of_step(name: &str) -> Option<Leg> {
        match name {
            "text" => Some(Leg::Text),
            "convert" => Some(Leg::Convert),
            "ai-tags" | "ai-summaries" => Some(Leg::Ai),
            "refresh" | "expire" | "reindex" | "previews" | "backup" => Some(Leg::Other),
            _ => None,
        }
    }
}

/// Where one leg itself is. Kept separate from [`State`] because the two answer different questions:
/// the pass's state is about the run as a whole, and a leg's state is about one row of the bar — and
/// one row can be yellow while the run is green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegStatus {
    /// Its total was counted and nothing has been handled yet — the row reads "还没开始".
    Waiting,
    /// Handling its items now.
    Running,
    /// Its own total has been reached. Nothing about the rest of the pass is claimed.
    Done,
    /// Its own trouble, and the red one: a file it could not write, an index it could not open.
    Failed,
    /// The other end did not answer. A network leg that gets nothing back is not a failed pass — the ADR
    /// says so out loud, and [`LegStatus::fails_the_pass`] is where that is written down.
    Offline,
}

impl LegStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LegStatus::Waiting => "waiting",
            LegStatus::Running => "running",
            LegStatus::Done => "done",
            LegStatus::Failed => "failed",
            LegStatus::Offline => "offline",
        }
    }

    fn parse(text: &str) -> Option<LegStatus> {
        match text {
            "waiting" => Some(LegStatus::Waiting),
            "running" => Some(LegStatus::Running),
            "done" => Some(LegStatus::Done),
            "failed" => Some(LegStatus::Failed),
            "offline" => Some(LegStatus::Offline),
            _ => None,
        }
    }

    /// Does this trouble belong to the pass, or only to the thing it was waiting for?
    ///
    /// `Offline` is `false`: an endpoint that did not answer leaves the footage on disk, un-summarised
    /// and offered again next pass, which is the same outcome as the pass never having been scheduled to
    /// reach it. `Failed` is `true`, because it is this machine not managing its own files.
    pub fn fails_the_pass(self) -> bool {
        matches!(self, LegStatus::Failed)
    }
}

/// One leg's counter, as published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegCount {
    pub leg: Leg,
    /// Handled since the pass started — never since the last publish, which would let the same row
    /// count a segment twice across two writes.
    pub done: usize,
    /// Fixed by [`set_totals`] at the moment the pass started. Zero here means the leg would not be in
    /// the file at all.
    pub total: usize,
    pub status: LegStatus,
    /// A short reason, when the leg has one: the sentence that goes on the row that turned red or
    /// yellow. Truncated on the way in, because this is a row label and not a log.
    pub note: String,
}

impl LegCount {
    /// Whether this leg has anything a reader could act on. A leg counted at zero and handled at zero,
    /// with nothing said about it, is absent from the file rather than drawn as an empty bar.
    ///
    /// A leg that *reported* something is in the file whatever its counts: 某类本轮 0 件 means the row has
    /// no queue to draw, not that the row may hide a failure.
    fn is_published(&self) -> bool {
        self.total > 0 || self.done > 0 || !self.note.is_empty() || self.status != LegStatus::Waiting
    }
}

/// The four denominators, as the census counted them at the moment the pass started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub text: usize,
    pub convert: usize,
    pub ai: usize,
    pub other: usize,
}

impl Totals {
    pub fn leg(self, leg: Leg) -> usize {
        match leg {
            Leg::Text => self.text,
            Leg::Convert => self.convert,
            Leg::Ai => self.ai,
            Leg::Other => self.other,
        }
    }
}

/// A leg group mid-read: the four keys it is written as may arrive in any order, and a group missing one
/// of its numbers is refused at the end of the walk rather than half-shown.
#[derive(Debug, Default)]
struct PartialLeg {
    done: Option<usize>,
    total: Option<usize>,
    status: Option<LegStatus>,
    note: Option<String>,
}

/// A value that lives on one line cannot hold a line. Applied to every free-text value on the way out,
/// so the record stays one key per line no matter what a step's error string was holding.
fn flatten(text: &str) -> String {
    text.replace('\n', " ").replace('\r', " ")
}

/// One published moment of one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pass {
    /// The `windmaint` process that wrote this.
    pub pid: u32,
    pub kind: Kind,
    /// Seconds since the epoch, as this product counts them (`clock::LocalParts`).
    pub pass_started: i64,
    /// The open step, one-based. Zero means the pipeline had not reached its first step yet.
    pub step: usize,
    /// How many steps this pass means to run — nine for `all`, one for a step run alone.
    pub steps: usize,
    /// The step's own name (`text`, `convert`, …), which is what the window looks a label up by.
    pub step_name: String,
    pub step_started: i64,
    /// Work items finished in the open step — the number the shipped window was built to read, kept
    /// alive by [`add_items`] for exactly the leg that step feeds. Zero for a step that does not count
    /// them; the four leg counters are where the pass's own totals now live.
    pub items: usize,
    pub state: State,
    /// The pass's own closing sentence, when it has one.
    pub note: String,
    pub finished: Option<i64>,
    /// The four counters, in [`Leg::ALL`] order and holding only the legs with something to show. The
    /// pass's own total bar is their sum — see [`Pass::items_total`] — because one denominator written
    /// twice is two numbers that can disagree.
    pub legs: Vec<LegCount>,
}

impl Pass {
    /// A pass that has begun and finished nothing yet.
    fn started(pid: u32, kind: Kind, at: i64) -> Pass {
        Pass {
            pid,
            kind,
            pass_started: at,
            step: 0,
            steps: 0,
            step_name: String::new(),
            step_started: at,
            items: 0,
            state: State::Running,
            note: String::new(),
            finished: None,
            legs: Vec::new(),
        }
    }

    /// The leg's own row, if it is being shown.
    pub fn leg(&self, leg: Leg) -> Option<&LegCount> {
        self.legs.iter().find(|count| count.leg == leg)
    }

    /// The row for a leg, made room for in [`Leg::ALL`] order if it is not in the list yet.
    ///
    /// Insertion order is not cosmetic: a `Pass` built by a running pipeline has to compare equal to the
    /// same `Pass` read back off disk, and a reader that walks the file gets the legs in the order they
    /// were counted, not the order the work happened to finish in.
    fn touch_leg(&mut self, leg: Leg) -> &mut LegCount {
        if let Some(at) = self.legs.iter().position(|count| count.leg == leg) {
            return &mut self.legs[at];
        }
        let rank = Leg::ALL.iter().position(|l| *l == leg).unwrap_or(usize::MAX);
        let insert = self
            .legs
            .iter()
            .position(|count| Leg::ALL.iter().position(|l| *l == count.leg).unwrap_or(usize::MAX) > rank)
            .unwrap_or(self.legs.len());
        self.legs.insert(
            insert,
            LegCount { leg, done: 0, total: 0, status: LegStatus::Waiting, note: String::new() },
        );
        &mut self.legs[insert]
    }

    /// The denominator of the pass's own total bar: the sum of the legs this pass counted.
    pub fn items_total(&self) -> usize {
        self.legs.iter().map(|count| count.total).sum()
    }

    /// The numerator of the same bar. May pass [`Pass::items_total`] for a pass that did work the census
    /// never saw; the bar reads full, and the extra items belong to the next pass's count.
    pub fn items_done(&self) -> usize {
        self.legs.iter().map(|count| count.done).sum()
    }

    /// Items this pass's own denominators still owe — which is both 还差 N on a running row and the
    /// "下一轮还有 N" a closing sentence names. Never negative, and never counting work that arrived after
    /// the census: a pass cannot be behind on a promise it did not make.
    pub fn items_left(&self) -> usize {
        self.legs.iter().map(|count| count.total.saturating_sub(count.done)).sum()
    }

    /// The file body. One `key=value` per line, in this order, because a person who ends up reading
    /// it in an issue report should see the shape of the pass before seeing its details.
    pub fn encode(&self) -> String {
        let note = flatten(&self.note);
        let step_name = flatten(&self.step_name);
        let mut out = String::new();
        out.push_str(&format!("pid={}\n", self.pid));
        out.push_str(&format!("kind={}\n", self.kind.as_str()));
        out.push_str(&format!("state={}\n", self.state.as_str()));
        out.push_str(&format!("pass_started={}\n", self.pass_started));
        out.push_str(&format!("finished={}\n", self.finished.unwrap_or(0)));
        out.push_str(&format!("step={}/{}\n", self.step, self.steps));
        out.push_str(&format!("step_name={step_name}\n"));
        out.push_str(&format!("step_started={}\n", self.step_started));
        out.push_str(&format!("items={}\n", self.items));
        // One group of four lines per leg, in the order the rows are drawn, and only for a leg with
        // something in it. The keys are namespaced so an old reader's `_ => continue` drops the whole
        // group without losing the file: the shipped window reads the nine keys above and nothing else.
        for count in self.legs.iter().filter(|count| count.is_published()) {
            let name = count.leg.as_str();
            out.push_str(&format!("leg.{name}.done={}\n", count.done));
            out.push_str(&format!("leg.{name}.total={}\n", count.total));
            out.push_str(&format!("leg.{name}.state={}\n", count.status.as_str()));
            out.push_str(&format!("leg.{name}.note={}\n", flatten(&count.note)));
        }
        out.push_str(&format!("note={note}\n"));
        out
    }

    /// Read a body back. Anything that fails to parse the state, the pid, or the step counter is
    /// `None` rather than a half-built `Pass`: an invented step number on a progress bar is worse
    /// than a window that says it cannot tell what is happening. The same rule covers a leg group —
    /// a row that reached three of its four keys would otherwise paint a bar with an invented
    /// denominator, which is the exact thing this file is not allowed to do.
    pub fn decode(body: &str) -> Option<Pass> {
        let mut pid = None;
        let mut kind = None;
        let mut state = None;
        let mut pass_started = 0;
        let mut finished = None;
        let mut step = None;
        let mut steps = None;
        let mut step_name = String::new();
        let mut step_started = 0;
        let mut items = 0;
        let mut note = String::new();
        let mut partials: [Option<PartialLeg>; 4] = std::array::from_fn(|_| None);
        for line in body.lines() {
            let (key, raw) = line.split_once('=')?;
            let value = raw.trim();
            if let Some(leg_key) = key.trim().strip_prefix("leg.") {
                // `leg.<name>.<field>`. A leg name or field this reader does not know is a newer
                // writer's fifth counter, not a broken file — the same rule as the unknown key below. A
                // value that will not parse is the other thing, and it stops the read.
                let Some((name, field)) = leg_key.split_once('.') else { continue };
                let Some(leg) = Leg::parse(name.trim()) else { continue };
                let Some(rank) = Leg::ALL.iter().position(|l| *l == leg) else { continue };
                let partial = partials[rank].get_or_insert_with(PartialLeg::default);
                match field.trim() {
                    "done" => partial.done = Some(value.parse::<usize>().ok()?),
                    "total" => partial.total = Some(value.parse::<usize>().ok()?),
                    "state" => partial.status = Some(LegStatus::parse(value)?),
                    "note" => partial.note = Some(raw.to_string()),
                    _ => {}
                }
                continue;
            }
            match key.trim() {
                "pid" => pid = value.parse::<u32>().ok(),
                "kind" => kind = Kind::parse(value),
                "state" => state = State::parse(value),
                "pass_started" => pass_started = value.parse().unwrap_or(0),
                "finished" => finished = value.parse::<i64>().ok().filter(|v| *v != 0),
                "step" => {
                    let (now, total) = value.split_once('/')?;
                    step = now.parse::<usize>().ok();
                    steps = total.parse::<usize>().ok();
                }
                "step_name" => step_name = value.to_string(),
                "step_started" => step_started = value.parse().unwrap_or(0),
                "items" => items = value.parse().unwrap_or(0),
                "note" => note = raw.to_string(),
                // An unknown key is a newer writer than this reader, not a broken file.
                _ => continue,
            }
        }
        let mut legs = Vec::new();
        for (index, partial) in partials.into_iter().enumerate() {
            let Some(partial) = partial else { continue };
            // A group is all of its numbers or it is nothing: half a counter is refused the way a
            // half-named step is.
            let (Some(done), Some(total), Some(status)) = (partial.done, partial.total, partial.status) else {
                return None;
            };
            legs.push(LegCount { leg: Leg::ALL[index], done, total, status, note: partial.note.unwrap_or_default() });
        }
        Some(Pass {
            pid: pid?,
            kind: kind?,
            state: state?,
            pass_started,
            finished,
            step: step?,
            steps: steps?,
            step_name,
            step_started,
            items,
            note,
            legs,
        })
    }

    /// Is this pass running *right now*?
    ///
    /// Two conditions, and the file alone cannot answer either: the pass has not declared an ending,
    /// and the process that wrote it is still in the table. The second is what makes a crash readable
    /// — a `windmaint` killed mid-step leaves the file saying `running` forever, and a window that
    /// believed the file would spin for a process that died an hour ago. An ending, by contrast, is
    /// not "nothing": the reader shows it until the next pass replaces it, and asks nothing of the pid.
    pub fn is_running(&self) -> bool {
        matches!(self.state, State::Running) && crate::fslock::is_process_running(self.pid)
    }

    /// The step still to come, for a reader that wants to say "第 3/9 步".
    pub fn remaining(&self) -> usize {
        self.steps.saturating_sub(self.step)
    }
}

/// The publisher for this process, when a pipeline is running.
struct Live {
    path: PathBuf,
    pass: Pass,
    published: Instant,
    /// Whether this pass's four denominators have been fixed. One set of totals per pass, because a
    /// second census taken while the bar is moving is the receding bar the ADR forbids.
    denominated: bool,
}

static CURRENT: Mutex<Option<Live>> = Mutex::new(None);

/// Say where the pass lives, what started it, and when it began. Call once, in `run_pipeline`.
pub fn install(path: &Path, kind: Kind, started: i64) {
    let live = Live {
        path: path.to_path_buf(),
        pass: Pass::started(std::process::id(), kind, started),
        published: Instant::now(),
        denominated: false,
    };
    // A poisoned lock means a panic happened while writing, which is the pass's own failure to report
    // rather than a reason to stop the pass: recover and carry the state we still hold.
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(live);
    let live = guard.as_mut().expect("just installed");
    publish_to(&live.path, &live.pass);
}

/// Open a step, publishing immediately: a boundary is the one moment a reader is certain to care about.
///
/// The nine boundaries are still what `run_pipeline` walks, so the `step`/`steps`/`step_name` keys keep
/// their old meaning for the window that is already shipped. A boundary also lifts the leg that step
/// feeds out of "还没开始": a step that is running is running whatever its own count says, which is the
/// answer to the `reindex` of 2026-09-30 — fifty minutes of a live walk publishing `items=0`.
pub fn begin_step(name: &str, index: usize, total: usize, at: i64) {
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(live) = guard.as_mut() else { return };
    live.pass.step = index;
    live.pass.steps = total;
    live.pass.step_name = name.to_string();
    live.pass.step_started = at;
    live.pass.items = 0;
    live.pass.state = State::Running;
    live.pass.finished = None;
    if let Some(leg) = Leg::of_step(name) {
        // Only a leg the census counted gets a row: opening a step must not invent a denominator for it.
        if let Some(count) = live.pass.legs.iter_mut().find(|count| count.leg == leg) {
            if count.status == LegStatus::Waiting {
                count.status = LegStatus::Running;
            }
        }
    }
    live.published = Instant::now();
    publish_to(&live.path, &live.pass);
}

/// Fix the four denominators, from the census taken at the moment the pass started.
///
/// Called once, by `run_pipeline` before its first step, and it is the only way a total enters the file.
/// Once fixed, a pass keeps its totals for the rest of that pass: a second census — the settings page
/// pressing again, a second pass trying to open — is refused outright rather than re-fitting a row, which
/// is what 条永不倒退 means as an API rule. Work recorded after the census therefore lands *past* its
/// denominator: the bar reaches its end and stays there, and the extra items belong to the next pass.
///
/// With nothing installed it does nothing, like every other call here: a hand-run `windmaint text` has no
/// nine-step pass to be the denominator of.
pub fn set_totals(totals: Totals) {
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(live) = guard.as_mut() else { return };
    // One-shot, for the whole pass: a census asked for again after the bar has started moving is the
    // receding bar the ADR forbids, whatever unit it counted in.
    if live.denominated {
        return;
    }
    live.denominated = true;
    for leg in Leg::ALL {
        let total = totals.leg(leg);
        if total == 0 {
            // Nothing counted, nothing drawn. A leg that later turns up work makes its own row by
            // adding to it, which is the honest direction for a surprise.
            continue;
        }
        live.pass.touch_leg(leg).total = total;
    }
    live.published = Instant::now();
    publish_to(&live.path, &live.pass);
}

/// Report work items finished in one leg of the pass.
///
/// The category is an argument rather than an inference from the open step, because the legs are going to
/// be published from several threads and the step boundary belongs to the pipeline, not to a row. Cheap
/// enough to call once per row: with nothing installed it returns at once, and otherwise it writes at
/// most once a second.
///
/// It still feeds the old `items` key when the category is the one the open step belongs to, so the
/// shipped window's number keeps meaning "work finished in the open step" instead of freezing at zero.
pub fn add_items(leg: Leg, handled: usize) {
    if handled == 0 {
        return;
    }
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(live) = guard.as_mut() else { return };
    if Leg::of_step(&live.pass.step_name) == Some(leg) {
        live.pass.items += handled;
    }
    let count = live.pass.touch_leg(leg);
    count.done += handled;
    // Trouble is not permanent: a leg that handles another item is answering again, so the row goes back
    // to working and loses the reason it had — a row that says `41 / 200` and "the endpoint did not
    // answer" at the same time is a sentence that cannot be acted on.
    if matches!(count.status, LegStatus::Waiting | LegStatus::Failed | LegStatus::Offline) {
        count.status = LegStatus::Running;
        count.note = String::new();
    }
    // A leg that reaches the number the census counted is finished, even if a step has yet to say so.
    if count.total > 0 && count.done >= count.total {
        count.status = LegStatus::Done;
    }
    if live.published.elapsed() < PUBLISH_EVERY {
        return;
    }
    live.published = Instant::now();
    publish_to(&live.path, &live.pass);
}

/// Say how one leg itself is doing, with the short reason that goes on its row. Publishing is immediate:
/// trouble is the other boundary a reader is certain to care about, and a leg that turned red a minute ago
/// is a minute of a person watching a bar that still says `running`.
///
/// Only the leg the ADR puts on the network (`Leg::Ai`) is allowed to say `offline`, and a leg that says
/// so is not the pass's failure — see [`LegStatus::fails_the_pass`].
pub fn report_leg(leg: Leg, status: LegStatus, reason: &str) {
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(live) = guard.as_mut() else { return };
    let count = live.pass.touch_leg(leg);
    count.status = status;
    count.note = short(reason);
    live.published = Instant::now();
    publish_to(&live.path, &live.pass);
}

/// How many items the pass's own denominators still owe. `0` with nothing installed, which is the same
/// answer as "nothing was counted": a closing sentence built from this cannot claim a queue exists.
pub fn items_left() -> usize {
    let guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|live| live.pass.items_left()).unwrap_or(0)
}

/// Report work items finished in the open step, in whatever unit that step counts its own rows in, and
/// claim no part of a leg's denominator for them.
///
/// Two ways of adding an item because there are two kinds of number a pass can honestly publish. The four
/// leg counters are the pass's promise — each denomininated by the census, in the unit the step reports
/// in — and a step whose unit the census cannot count must not be scored against it. `reindex` lifts rows
/// out of a video the census counted as one thing, and `backup` copies month files, which no census
/// counts at all. Both still get to say how far they have got, through the `items` key the shipped window
/// already reads, and neither gets to move a bar it was never measured against.
pub fn add_step_items(handled: usize) {
    if handled == 0 {
        return;
    }
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(live) = guard.as_mut() else { return };
    live.pass.items += handled;
    if live.published.elapsed() < PUBLISH_EVERY {
        return;
    }
    live.published = Instant::now();
    publish_to(&live.path, &live.pass);
}

/// Say that the pass has been called off, the moment the request is read — before it waits for anything.
///
/// [`finish`] is still the only place that may say how the pass ended and how much is left, and reaching
/// it takes the pass as long as the lanes it started need to come down: a `wind-reindex` in the middle of
/// a batch, a `windai` with a request on the wire. A 停止整理 button that produces no answer for the length
/// of that wait is a button that looks broken, so this writes the one sentence that is already true —
/// *called off, still putting the work down* — and says nothing about items, because the lanes have not
/// finished spending them.
///
/// The state word does change here, and that is what the window shows while the pass is on its feet: the
/// lane waits end seconds apart, and a bar that keeps breathing through them is the pulse this file's own
/// reader learned to distrust. The pass's real ending replaces both the state and this sentence, and a pass
/// that dies before it gets there is left saying the last true thing it knew.
pub fn stopping(note: &str) {
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(live) = guard.as_mut() else { return };
    live.pass.state = State::Stopped;
    live.pass.note = short(note);
    live.published = Instant::now();
    publish_to(&live.path, &live.pass);
}

/// Close the pass with its own sentence. Keeps the publisher installed until [`uninstall`] so a step
/// that finishes after the ending still cannot resurrect a `running` state.
pub fn finish(state: State, note: &str, at: i64) {
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(live) = guard.as_mut() else { return };
    live.pass.state = state;
    live.pass.note = note.to_string();
    live.pass.finished = Some(at);
    // A pass that reached the end of its steps has finished with the legs it never heard the last word
    // from; a leg that reported its own trouble keeps what it said.
    for count in live.pass.legs.iter_mut() {
        if count.total > 0 && count.done >= count.total && matches!(count.status, LegStatus::Waiting | LegStatus::Running) {
            count.status = LegStatus::Done;
        }
    }
    live.published = Instant::now();
    publish_to(&live.path, &live.pass);
}

/// Stop publishing. Called at the end of `run_pipeline`, so a long-lived process cannot leave a
/// publisher behind that a later command would write through.
pub fn uninstall() {
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// May the work still go on?
///
/// A step's item loop calls this once per row, so it cannot afford a file read per row: the answer is
/// re-asked at most once a second and held in between, and once a stop has been seen it stays `false` for
/// the rest of the process. Holding a `false` is deliberate — the stop flag is cleared by the pass that
/// honours it (`Config::clear_maintain_stop`), and a step that kept asking would watch its own pass erase
/// the reason it stopped.
///
/// This is what makes 停止整理 land inside a step rather than only between steps. Before it, a stop was
/// checked at the nine boundaries, so pressing it during an OCR pass over a night of frames meant waiting
/// for that step to finish every row it had — minutes, with no answer on screen except an unchanged bar.
/// The step that sees this returns early and successfully; the pipeline's own boundary check then says
/// the ending out loud, so the sentence comes from one place.
pub fn may_continue(config: &crate::config::Config) -> bool {
    use std::sync::Mutex;
    static SEEN: Mutex<Seen> = Mutex::new(Seen { asked: None, stopped: false });

    struct Seen {
        /// When the flag was last read from disk. `None` until the first ask.
        asked: Option<Instant>,
        stopped: bool,
    }

    let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if guard.stopped {
        return false;
    }
    let fresh = guard.asked.is_some_and(|at| at.elapsed() < PUBLISH_EVERY);
    if fresh {
        return true;
    }
    guard.asked = Some(Instant::now());
    if config.maintain_stop_requested() {
        guard.stopped = true;
        return false;
    }
    true
}

/// Write the file. Takes the path as an argument because every caller already holds `CURRENT`: the
/// lock is not reentrant, and a publisher that deadlocks its own process would stop the pass it is
/// meant to be reporting on. A failure to publish costs the pass its progress bar and must not cost
/// it the run — the same rule that lets the recorder's log-open failure fall back to `Stdio::null`.
fn publish_to(path: &Path, pass: &Pass) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, pass.encode());
}

/// Read what the pass last said. `None` means the file does not exist or cannot be understood, which
/// a reader should show as "cannot tell" rather than as "nothing is running".
pub fn read(path: &Path) -> Option<Pass> {
    Pass::decode(&std::fs::read_to_string(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The publisher is one process-wide slot, so the tests that use it go one at a time.
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    /// A pass mid-pipeline with three of its four legs showing, and the one the census counted at zero
    /// (`ai`) deliberately absent — which is what the leg test below is for. The open step is `expire`,
    /// the leg that reads `other`, so its `items` and that leg's `done` are the same 41 by design.
    fn pass() -> Pass {
        Pass {
            pid: 4242,
            kind: Kind::Manual,
            pass_started: 1_790_600_000,
            step: 3,
            steps: 9,
            step_name: "expire".to_string(),
            step_started: 1_790_600_120,
            items: 41,
            state: State::Running,
            note: String::new(),
            finished: None,
            legs: vec![
                LegCount { leg: Leg::Text, done: 3_700, total: 4_200, status: LegStatus::Running, note: String::new() },
                LegCount { leg: Leg::Convert, done: 28, total: 68, status: LegStatus::Running, note: String::new() },
                LegCount {
                    leg: Leg::Other,
                    done: 41,
                    total: 200,
                    status: LegStatus::Running,
                    note: "the index for 2026-08 would not open".to_string(),
                },
            ],
        }
    }

    /// All four legs counted, as one set of denominators.
    fn four_totals() -> Totals {
        Totals { text: 4_200, convert: 68, ai: 9, other: 200 }
    }

    #[test]
    fn a_published_pass_reads_back_as_the_pass_that_was_published() {
        let written = pass();
        let read_back = Pass::decode(&written.encode()).expect("the writer's own output must parse");
        assert_eq!(read_back, written, "the file is the whole state, so nothing may be encoded lossily");
    }

    #[test]
    fn the_step_counter_survives_the_round_trip_as_one_number_pair() {
        // The window paints `3/9` out of these two fields; a file that lost one of them would render a
        // progress bar with a bar count nobody asked for.
        let body = pass().encode();
        assert!(body.contains("step=3/9"), "{body}");
        let read_back = Pass::decode(&body).unwrap();
        assert_eq!((read_back.step, read_back.steps, read_back.remaining()), (3, 9, 6));
    }

    #[test]
    fn a_file_that_names_no_step_is_not_a_pass_anybody_can_show() {
        // Half-written, truncated, or written by a foreign tool: the reader gets `None` and says it
        // cannot tell, rather than inventing step zero of nine.
        for body in ["", "pid=1\n", "pid=not-a-number\nstate=running\nkind=manual\nstep=1/9\n", "step=3/9\n"] {
            assert_eq!(Pass::decode(body), None, "{body:?} must be refused");
        }
    }

    /// The four counters are the published shape now, so every one of their fields is pinned on the way
    /// out and on the way back: a row that read `3700 / 4200` off a file which said something else is the
    /// bug this file exists to make impossible.
    #[test]
    fn each_leg_is_written_as_four_keys_and_reads_back_field_for_field() {
        let written = pass();
        let body = written.encode();
        // Byte for byte, in the order the rows are drawn, and the group order is the display order.
        for expected in [
            "leg.text.done=3700\nleg.text.total=4200\nleg.text.state=running\nleg.text.note=\n",
            "leg.convert.done=28\nleg.convert.total=68\nleg.convert.state=running\nleg.convert.note=\n",
            "leg.other.done=41\nleg.other.total=200\nleg.other.state=running\nleg.other.note=the index for 2026-08 would not open\n",
        ] {
            assert!(body.contains(expected), "{expected:?} missing from\n{body}");
        }
        // The counters sit between the step's own number and the pass's sentence: identity, then shape,
        // then details, then the one line a person reads first.
        let (items, first_leg, closing) = (body.find("items=").unwrap(), body.find("leg.text.done=").unwrap(), body.find("\nnote=").unwrap());
        assert!(items < first_leg && first_leg < closing, "leg rows after the step, before the sentence:\n{body}");
        let read_back = Pass::decode(&body).expect("the writer's own leg rows must parse");
        assert_eq!(read_back.legs, written.legs, "a counter read back is the counter that was published");
        assert_eq!(read_back.encode(), body, "and re-publishing what was read changes nothing");
    }

    /// 某类本轮 0 件，那一行整行不显示 — an empty row is a bar promising a queue nobody counted.
    #[test]
    fn a_leg_the_census_counted_at_zero_is_not_in_the_file_at_all() {
        let body = pass().encode();
        assert!(pass().leg(Leg::Ai).is_none(), "the fixture's ai leg was counted at nothing");
        assert!(!body.contains("leg.ai."), "{body} must not carry a row for a leg with nothing in it");
        // And what is absent from the file stays absent on the way back, rather than becoming a `0 / 0`
        // row the reader would have to be told to ignore.
        let read_back = Pass::decode(&body).unwrap();
        assert_eq!(read_back.legs.len(), 3);
        assert!(read_back.leg(Leg::Ai).is_none());
    }

    /// The pass's own total bar is the sum of the rows that are there. It is not a key of its own, for
    /// the reason the census gives for itself: two numbers for one fact is how they come to disagree.
    #[test]
    fn the_total_bar_is_the_sum_of_the_legs_being_shown() {
        let written = pass();
        assert_eq!(written.items_total(), 4_200 + 68 + 200, "the legs shown, and the absent one as nothing");
        assert_eq!(written.items_done(), 3_700 + 28 + 41);
        assert_eq!(written.items_left(), 500 + 40 + 159, "还差 N on each row, added up");
        let read_back = Pass::decode(&written.encode()).unwrap();
        assert_eq!((read_back.items_total(), read_back.items_done(), read_back.items_left()), (4_468, 3_769, 699));
    }

    #[test]
    fn a_half_written_leg_group_is_refused_the_way_a_half_named_step_is() {
        let head = "pid=4242\nkind=manual\nstate=running\nstep=3/9\nitems=1\n";
        let bodies = [
            // Three of the four keys: the denominator or the status is what is missing, and a row cannot
            // be drawn from the other two without inventing it.
            format!("{head}leg.text.done=5\nleg.text.total=9\n"),
            format!("{head}leg.text.done=5\nleg.text.state=running\nleg.text.note=\n"),
            // Present but not a number: a bar that reads `/?` is worse than a window that says it cannot
            // tell, and both are better than a bar that quietly reads `0`.
            format!("{head}leg.text.done=many\nleg.text.total=9\nleg.text.state=running\nleg.text.note=\n"),
            format!("{head}leg.convert.done=1\nleg.convert.total=2\nleg.convert.state=frobnicated\nleg.convert.note=\n"),
        ];
        for body in &bodies {
            assert_eq!(Pass::decode(body), None, "{} must be refused", body.trim_end());
        }
    }

    #[test]
    fn an_unknown_key_is_a_newer_writer_and_still_reads() {
        let mut body = pass().encode();
        // A fifth counter and a sixth field on one of the four: both are a writer ahead of this reader,
        // and neither may cost the reader the three rows it does understand.
        body.push_str("progress_percent=44\nleg.markdown.done=3\nleg.markdown.total=9\nleg.text.priority=high\n");
        assert_eq!(Pass::decode(&body), Some(pass()), "a reader must not be dumber than the file it found");
    }

    /// The nine steps are unchanged, so which counter each one moves is a fact about the step's name and
    /// lives here, once. A step that fed two legs would double-count its own work, and a step that fed
    /// none would leave its items unpublished.
    #[test]
    fn every_step_of_the_nine_feeds_exactly_one_leg() {
        let mapped: Vec<(&str, Option<Leg>)> = [
            "text", "convert", "refresh", "expire", "reindex", "previews", "ai-tags", "ai-summaries", "backup",
        ]
        .iter()
        .map(|name| (*name, Leg::of_step(name)))
        .collect();
        for (name, leg) in &mapped {
            assert!(leg.is_some(), "{name} is a step of the pass and feeds no counter");
        }
        // The two legs the ADR names for themselves, and the rest swept up as 其他整理.
        assert_eq!(Leg::of_step("text"), Some(Leg::Text));
        assert_eq!(Leg::of_step("convert"), Some(Leg::Convert));
        assert_eq!(Leg::of_step("ai-tags"), Some(Leg::Ai));
        assert_eq!(Leg::of_step("ai-summaries"), Some(Leg::Ai));
        assert_eq!(mapped.iter().filter(|(_, leg)| *leg == Some(Leg::Other)).count(), 5, "refresh, expire, reindex, previews, backup");
        // What is not a step of the pass names no counter: `doctor` and `backlog` change nothing, `forget`
        // is never in the pipeline, `all` is the pipeline.
        for name in ["doctor", "backlog", "forget", "all", ""] {
            assert_eq!(Leg::of_step(name), None, "{name} is not a step of a pass");
        }
    }

    /// The yellow row and the red row are different sentences, and only one of them is about the pass.
    #[test]
    fn a_leg_that_only_the_endpoint_is_answering_for_does_not_fail_the_pass() {
        assert!(LegStatus::Failed.fails_the_pass(), "this machine could not do its own work");
        assert!(!LegStatus::Offline.fails_the_pass(), "an endpoint that said nothing leaves the footage on disk, owed, and offered again next pass");
        for status in [LegStatus::Waiting, LegStatus::Running, LegStatus::Done] {
            assert!(!status.fails_the_pass(), "{} is a leg going about it", status.as_str());
        }
        // The five statuses are the whole vocabulary of a row, and each has to survive the file.
        for status in [LegStatus::Waiting, LegStatus::Running, LegStatus::Done, LegStatus::Failed, LegStatus::Offline] {
            let mut written = pass();
            written.legs[0].status = status;
            let read_back = Pass::decode(&written.encode()).expect("a row in every one of its states reads back");
            assert_eq!(read_back.legs[0].status, status, "a status invented on the way out must not be read back as another");
        }
    }

    #[test]
    fn a_note_is_flattened_because_the_line_is_the_record() {
        let mut open = pass();
        open.note = "three lines\nof\nfailure".to_string();
        let read_back = Pass::decode(&open.encode()).expect("a flattened note must still parse");
        assert_eq!(read_back.note, "three lines of failure", "the sentence survives, the newlines do not");
    }

    #[test]
    fn only_a_running_pass_whose_pid_is_alive_is_running() {
        // The two ways the file can overstate itself, kept separate because the window says them
        // differently: a crash (the file still claims `running`, the process table disagrees) and an
        // ending (the file says `stopped`, which is a fact about a pass that is no longer running).
        let mut dead = pass();
        dead.pid = u32::MAX;
        assert_eq!(dead.state, State::Running);
        assert!(!dead.is_running(), "a pid nobody holds is not a pass in progress");

        let mut ended = pass();
        ended.state = State::Stopped;
        ended.finished = Some(ended.pass_started + 30);
        assert!(!ended.is_running(), "an ending is not a pass under way, alive pid or not");
        assert_eq!(ended.note, "", "and nothing about the pass itself was lost by the state changing");
    }

    #[test]
    fn a_stop_request_is_answered_once_and_then_held() {
        let _serial = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windrec-maintain-stop-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("cache/locks")).unwrap();
        let config = crate::Config::load(&dir).expect("a throwaway install");
        let flag = config.maintain_stop_signal_path();

        // Nothing asked: the loop may go on, and asking twice inside the second does not re-read the disk.
        let _ = std::fs::remove_file(&flag);
        assert!(may_continue(&config), "no request, no reason to stop");

        // The request arrives. The throttle is what makes the first answer late by up to a second — that
        // is the price of calling this once per row — and after it the latch holds `false` even though the
        // pass that honours the request deletes the file underneath the running step.
        crate::fslock::write_signal(&flag).expect("a stop request is a file");
        std::thread::sleep(PUBLISH_EVERY + std::time::Duration::from_millis(120));
        assert!(!may_continue(&config), "the request is seen inside a step, not only between steps");
        std::fs::remove_file(&flag).unwrap();
        assert!(!may_continue(&config), "a step does not get to change its mind because the flag moved");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pass_with_nothing_installed_publishes_nothing() {
        // `windmaint expire` run by hand has no nine-step shape to report, and it must not write a
        // file that would make the window think a pass was underway.
        //
        // The tests that install a publisher share one process-wide `CURRENT`, so they take this
        // lock rather than racing each other's `uninstall` — a green suite that depends on thread
        // order is how a publisher bug starts passing again.
        let _serial = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windrec-maintain-idle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        let _ = std::fs::remove_file(&path);
        uninstall();
        // Every way a pass speaks: a count for a leg, a denominator, a step, a leg's trouble, an ending.
        add_items(Leg::Other, 7);
        set_totals(four_totals());
        begin_step("text", 1, 9, 1_790_600_001);
        report_leg(Leg::Text, LegStatus::Offline, "the endpoint did not answer");
        finish(State::Complete, "9 steps, no failures", 1_790_600_400);
        assert!(!path.exists(), "no pipeline, no progress file — whatever the four counters are asked to say");
        assert_eq!(items_left(), 0, "and a pass that was never installed owes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pipeline_publishes_its_boundaries_and_its_ending() {
        let _serial = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windrec-maintain-pass-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        install(&path, Kind::Scheduled, 1_790_600_000);
        // The denominators come from the census, before anything has been handled.
        set_totals(four_totals());
        begin_step("text", 1, 9, 1_790_600_001);
        let opened = read(&path).expect("a boundary is on disk the moment it is taken");
        assert_eq!((opened.step, opened.steps, opened.step_name.as_str(), opened.state), (1, 9, "text", State::Running));
        assert_eq!(opened.kind, Kind::Scheduled, "who started it decides what may stop it");
        // The four counters are published alongside the step, and the leg the open step belongs to has
        // left "还没开始" the moment its step opened.
        assert_eq!(opened.items_total(), 4_477, "4200 + 68 + 9 + 200, counted at the start and no later");
        assert_eq!(opened.leg(Leg::Text).map(|count| count.status), Some(LegStatus::Running));
        assert_eq!(opened.leg(Leg::Convert).map(|count| count.status), Some(LegStatus::Waiting), "a leg whose step has not opened yet is not running");

        // Inside the one-second window the count is held rather than dropped: the next boundary or
        // ending carries it, and a reader never pays for a row loop.
        add_items(Leg::Text, 3);
        let throttled = read(&path).unwrap();
        assert_eq!(throttled.items, 0, "one second, one write");
        assert_eq!(throttled.leg(Leg::Text).unwrap().done, 0, "the leg's own counter rides the same throttle");
        finish(State::Stopped, "the maintenance window 03:30-05:00 has closed", 1_790_600_400);
        let closed = read(&path).expect("an ending is published");
        assert_eq!(closed.state, State::Stopped);
        assert_eq!(closed.finished, Some(1_790_600_400));
        assert_eq!(closed.items, 3, "the held count rode out with the ending");
        assert_eq!(closed.leg(Leg::Text).unwrap().done, 3, "and so did the leg's");
        assert!(closed.note.contains("has closed"), "{}", closed.note);
        assert_eq!(closed.items_left(), 4_477 - 3, "an ending still owes what the census counted");

        // A leg that reached the number counted for it is finished, and one that did not is not:
        // `finish` says which without being told each row's fate.
        assert_eq!(closed.leg(Leg::Convert).unwrap().status, LegStatus::Waiting, "nothing of its 68 was handled");
        // After the pipeline is over, nothing further is published — and the ending stays on disk for
        // the window to show until the next pass replaces it.
        uninstall();
        begin_step("backup", 9, 9, 1_790_600_500);
        assert_eq!(read(&path).unwrap().state, State::Stopped, "a stale step cannot overwrite the ending");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stop is answered the moment it is read, and that answer is not the ending.
    ///
    /// The two are different sentences and only one of them has numbers in it: between the flag and the
    /// ending there is a lane being put down, and a window that stayed on `running` through it is why a
    /// person presses the button again. So `stopping` says the one thing that is already true and touches
    /// nothing else — no `finished`, no leg closed, no item claimed.
    #[test]
    fn the_stop_is_answered_before_the_pass_has_stopped_waiting() {
        let _serial = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windrec-maintain-stopping-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        install(&path, Kind::Manual, 1_790_600_000);
        set_totals(four_totals());
        begin_step("convert", 2, 9, 1_790_600_001);
        add_items(Leg::Convert, 4);

        stopping("stop requested — putting the work in hand down (step 2 of 9)");
        let shown = read(&path).expect("the acknowledgement is on disk the second it is said");
        assert_eq!(shown.state, State::Stopped, "the page stops saying 整理中 as soon as the flag is read");
        assert!(shown.note.contains("putting the work in hand down"), "{}", shown.note);
        assert_eq!(shown.finished, None, "this is not an ending, and cannot say when one happened");
        assert_eq!(
            shown.leg(Leg::Convert).map(|count| (count.done, count.total, count.status)),
            Some((4, 68, LegStatus::Running)),
            "the rows are left exactly where the lanes have them — only `finish` closes a leg"
        );
        assert!(!shown.is_running(), "a pass that has been called off does not get a live bar");
        assert_eq!(shown.step, 2, "and it still says which step it was in the middle of");

        // The ending replaces the sentence, is the only place `finished` appears, and is the same word the
        // pass has always closed with.
        finish(State::Stopped, "stopped after 1 of 9 steps: stopped by request", 1_790_600_400);
        let ended = read(&path).unwrap();
        assert_eq!(ended.finished, Some(1_790_600_400));
        assert_eq!(ended.note, "stopped after 1 of 9 steps: stopped by request");
        uninstall();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The four rows are independent, which is the whole point of publishing four of them: one leg
    /// finishing, one leg going quiet, and the pass as a whole are three different statements.
    #[test]
    fn one_leg_can_finish_and_another_go_quiet_while_the_pass_still_runs() {
        let _serial = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windrec-maintain-legs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        install(&path, Kind::Manual, 1_790_600_000);
        set_totals(four_totals());
        begin_step("convert", 2, 9, 1_790_600_001);
        add_items(Leg::Convert, 68);
        // The endpoint goes quiet. `report_leg` is a boundary, so it carries the held convert count out
        // with it and the two rows are readable at once — which is what the window needs to be able to
        // say "the合成 bar is full and the AI bar is yellow" in one look.
        report_leg(Leg::Ai, LegStatus::Offline, "the endpoint did not answer in 900 s");
        let shown = read(&path).expect("both rows are on disk");
        assert_eq!(shown.state, State::Running, "and the pass itself has said nothing about ending");
        assert_eq!(shown.leg(Leg::Convert).map(|c| (c.done, c.total, c.status)), Some((68, 68, LegStatus::Done)));
        assert_eq!(shown.leg(Leg::Ai).map(|c| (c.done, c.total, c.status)), Some((0, 9, LegStatus::Offline)));
        assert_eq!(shown.leg(Leg::Ai).unwrap().note, "the endpoint did not answer in 900 s");
        assert_eq!(shown.leg(Leg::Text).map(|c| c.status), Some(LegStatus::Waiting), "a third row nobody has spoken for yet");
        assert_eq!(shown.items_left(), 4_477 - 68, "the quiet leg still owes all nine of its items");
        assert!(!shown.leg(Leg::Ai).unwrap().status.fails_the_pass(), "a row of yellow is not a failed pass");

        // The quiet leg answers: its own row goes back to work and loses the reason it had, because a bar
        // that moves and a sentence saying it is not coming back cannot both be true.
        add_items(Leg::Ai, 1);
        finish(State::Complete, "9 steps, no failures", 1_790_600_900);
        let closed = read(&path).unwrap();
        assert_eq!(closed.leg(Leg::Ai).map(|c| (c.done, c.status, c.note.as_str())), Some((1, LegStatus::Running, "")));
        assert_eq!(closed.leg(Leg::Convert).unwrap().status, LegStatus::Done, "and the finished row stayed finished");
        assert_eq!(closed.leg(Leg::Text).unwrap().status, LegStatus::Waiting, "the ending does not claim a leg that never ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 分母在开工那一刻数定, and 条永不倒退: a second census — or a first one that came in low because the
    /// recorder kept working — may not move a denominator a leg has already been scored against.
    #[test]
    fn the_denominators_are_fixed_at_the_start_and_never_move_under_a_running_bar() {
        let _serial = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windrec-maintain-fixed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        install(&path, Kind::Scheduled, 1_790_600_000);
        set_totals(Totals { text: 100, convert: 0, ai: 0, other: 0 });
        begin_step("text", 1, 9, 1_790_600_001);
        // The step outruns its own census: 130 rows of frames the recorder filled after the count.
        add_items(Leg::Text, 130);
        // A census asked for again — the settings page pressed, or a second pass trying to open on a live
        // one — must not move denominators a bar is already being scored against.
        set_totals(Totals { text: 900, convert: 70, ai: 0, other: 0 });
        // The next boundary is what carries the held count to disk.
        begin_step("convert", 2, 9, 1_790_600_050);
        let shown = read(&path).unwrap();
        assert_eq!(shown.leg(Leg::Text).map(|c| (c.done, c.total)), Some((130, 100)), "the total it was counted at, not the one asked for later");
        assert_eq!(shown.leg(Leg::Text).map(|c| c.status), Some(LegStatus::Done), "past its own count is finished, not behind");
        assert_eq!(shown.items_total(), 100, "the pass's own bar is 130 of 100: full, and not receding");
        assert_eq!(shown.items_left(), 0, "and it owes nothing it promised — the 30 extra belong to next pass's census");
        // A leg the census found nothing for stays off the file even when its step runs: `convert` was
        // counted at zero, so opening its step invents no row and no denominator.
        assert!(shown.leg(Leg::Convert).is_none() && shown.leg(Leg::Ai).is_none() && shown.leg(Leg::Other).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Trouble is drawn whatever the counts say. 某类本轮 0 件 means a row has no queue to draw — it is not
    /// permission for a leg that could not do its work to disappear from the file.
    #[test]
    fn a_leg_in_trouble_shows_its_row_even_when_nothing_was_counted() {
        let _serial = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windrec-maintain-trouble-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        install(&path, Kind::Scheduled, 1_790_600_000);
        set_totals(Totals { text: 4, convert: 0, ai: 0, other: 0 });
        begin_step("expire", 3, 9, 1_790_600_010);
        report_leg(Leg::Other, LegStatus::Failed, "2026-08: no such table records");
        let shown = read(&path).unwrap();
        assert_eq!(shown.leg(Leg::Other).map(|c| (c.done, c.total, c.status)), Some((0, 0, LegStatus::Failed)), "a row of red with no queue behind it is still a row");
        assert_eq!(shown.leg(Leg::Other).unwrap().note, "2026-08: no such table records");
        assert_eq!(shown.items_total(), 4, "and it adds nothing to the total bar, because nothing was counted");
        assert_eq!(shown.items_left(), 4, "the trouble is not a debt the pass can be scored against");
        // A leg that said nothing about itself and was counted at nothing is still absent.
        assert!(shown.leg(Leg::Ai).is_none() && shown.leg(Leg::Convert).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The publisher's own arithmetic for the closing sentence, read by the pass rather than decoded from
    /// a file it wrote: `run_pipeline` asks this when it builds the line that says what is still owed.
    #[test]
    fn the_publisher_answers_with_what_the_next_pass_still_has() {
        let _serial = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("windrec-maintain-left-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PROGRESS.MD");
        uninstall();
        assert_eq!(items_left(), 0, "with no pass running, nobody is owed anything");
        install(&path, Kind::Manual, 1_790_600_000);
        assert_eq!(items_left(), 0, "a pass that has not been counted yet owes no invented number");
        set_totals(four_totals());
        assert_eq!(items_left(), 4_477);
        begin_step("text", 1, 9, 1_790_600_001);
        add_items(Leg::Text, 400);
        assert_eq!(items_left(), 4_077, "held behind the throttle or not, the answer is the state in hand");
        uninstall();
        assert_eq!(items_left(), 0, "and once the pass is over the publisher stops answering for it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A leg's reason is a row label, not a log: the file is read every two seconds and a step's error
    /// string can be a paragraph naming every month it could not open.
    #[test]
    fn a_legs_reason_is_kept_short_enough_to_read_on_one_row() {
        let long = "2026-08: no such table records; ".repeat(20);
        let cut = short(&long);
        assert!(cut.chars().count() <= LEG_NOTE_MAX, "{} chars is not a row label", cut.chars().count());
        assert!(cut.ends_with('…'), "and it says so: {cut}");
        assert!(cut.starts_with("2026-08: no such table records;"), "the beginning of the sentence is the part that explains it");
        // Short ones come back as they were; a reason with a newline in it cannot, because the line is the record.
        assert_eq!(short("ffmpeg could not encode one slice"), "ffmpeg could not encode one slice");
        assert_eq!(short("two\nlines"), "two lines");
    }
}
