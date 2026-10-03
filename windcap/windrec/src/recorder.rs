//! The recorder proper: one capture cycle at a time, and the segment lifecycle around it.
//!
//! Split out of `main.rs` for the same reason `segment.rs` is: the decisions (grab or don't, index
//! or discard, rotate or keep going, resume or stay paused) are the parts that can lose a user's
//! history, and they are testable only if the GDI call and the SQLite write are on the other side of
//! a boundary.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use wind_base::clock;
use wind_base::config::{Config, MaintainWindow};
use wind_base::fslock::{self, Availability};
use wind_base::image;
use wind_base::paths;
use windcap::capture::{foreground_rect, monitor_rect, virtual_desktop, Grabber, VirtualDesktop};
use windcap::crop::{MaskPlan, Tile};
use windcap::gate::{ChangeGate, GateConfig};
use windcap::winstate;
use wind_store::write::Store;

use crate::ocr::OcrEngine;
use crate::segment::{self, IdleTracker, Journal, OcrOutcome, Plan, Rejection, Segment};
use crate::wintitle;

/// Working resolution the frame is resampled to. Wide enough that a single changed glyph survives
/// the decimation for the gate's sake; GDI does the resampling on the way into the DIB section.
pub const CAPTURE_WIDTH: u32 = 1920;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ask the recorder to finish the segment it is in and exit. Called from the console control
/// handler, so it must be async-signal-safe: an atomic store is.
pub fn request_stop() {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Relaxed)
}

/// What a run does with a frame it decides to keep.
///
/// The three are distinct tools and the CLI reaches each with its own flag; they used to be two
/// names for one boolean, which is how `--gate-only` came to be parsed, echoed and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Grab, gate, OCR, index. What `loop` and a plain `run` do.
    Full,
    /// Grab, gate, and index on the window title alone: the OCR engine is never invoked. This is the
    /// row shape an OCR failure degrades to, so it can be exercised on a machine whose engine is
    /// perfectly healthy.
    TitlesOnly,
    /// Grab and gate, then stop. No frames and no rows: the mode that prices the capture path.
    GateOnly,
}

impl Mode {
    /// Whether this run asks the OCR engine for anything.
    pub fn reads_text(self) -> bool {
        matches!(self, Mode::Full)
    }
}

/// Counters for the closing line. Every one of these is a way the recorder can look healthy while
/// quietly dropping everything, so they are printed rather than kept private.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub kept: usize,
    /// Ticks that passed the change gate in gate-only mode, where a "kept" frame is never written.
    /// Counted apart from `kept` so that number always means "there is a JPEG on disk behind this
    /// tick", which is the only meaning a user can act on.
    pub gated: usize,
    pub dropped_unchanged: usize,
    pub dropped_repeat: usize,
    pub dropped_empty: usize,
    pub dropped_excluded: usize,
    pub dropped_early: usize,
    pub skipped_session: usize,
    /// Ticks dropped because the machine had slept; see `segment::sleep_intervened`.
    pub skipped_sleep: u32,
    pub paused_ticks: usize,
    /// Pauses that ended because the machine reported input again. Worth its own counter because the
    /// failure this guards against is a pause that *never* ends: `paused_ticks` alone grows quietly
    /// while the day records nothing. See [`segment::resumed_from_input`].
    pub resumed_from_pause: usize,
    /// Ticks that lost their frame: the grab failed, or the JPEG could not be written. Deliberately
    /// *not* charged for an OCR engine that could not run, because that costs the user nothing —
    /// see [`Stats::ocr_unavailable`]. Two different events in one counter is how a report ends up
    /// saying frames were thrown away when every frame was kept.
    pub failures: usize,
    /// Frames the OCR engine could not read, and which were kept and indexed on their window title
    /// anyway. A degraded run, not a discarded one.
    pub ocr_unavailable: usize,
    pub grabber_rebuilds: u32,
    pub segments_closed: usize,
    pub rows_committed: usize,
    pub collapsed: usize,
    /// Journals that could not be written. The frames and the in-memory rows are unaffected — a
    /// failed append costs only the crash protection for that one frame, which is why it is counted
    /// here rather than folded into `failures`.
    pub journal_failures: usize,
    /// Rows recovered from a dead instance's journal at startup, and how many segments they came
    /// from. Reported separately from `rows_committed` because they belong to a different run.
    pub swept_segments: usize,
    pub swept_rows: usize,
    /// Frames whose OCR input had at least one excluded edge painted over it.
    ///
    /// This is the privacy control's own counter, and it is printed for the same reason the others
    /// are: a mask that silently stopped being applied is invisible everywhere else. A run where
    /// `kept` is well above `masked` means the configured percentages are all zero, or that the frame
    /// size and the attached panels no longer agree and the default band took over — both worth
    /// reading out loud.
    pub masked: usize,
    /// Cost of the grabs, mean and best. The source rectangle dominates this number — it is why
    /// `capture_source` exists — so a recorder that cannot say what a frame cost cannot tell a
    /// regression from a monitor being unplugged. Best is reported alongside mean because the first
    /// grab of a process pays the DIB section's page faults, which on a short run swamps the average:
    /// three samples of a 3.7 MP grab have been measured higher than three of a 17 MP one.
    pub grab_millis: f64,
    pub fastest_grab_millis: f64,
    pub grabs: u32,
    /// Largest source rectangle seen, in megapixels: the worst case the mean above is drawn from.
    pub widest_source_mp: f64,
}

pub struct Recorder {
    config: Config,
    plan: Plan,
    store: Store,
    /// The (year, month) `store` is open for, so rotation across a midnight boundary is visible.
    store_month: (i64, u32),
    grabber: Grabber,
    source: VirtualDesktop,
    gate: ChangeGate,
    engine: OcrEngine,
    segment: Segment,
    idle: IdleTracker,
    /// One-shot latch so the "your monitor is gone" warning cannot repeat once per frame.
    warned_missing_display: AtomicBool,
    /// One-shot latch for the same reason, for the OCR engine: a missing engine cannot come back
    /// mid-run, so a line per frame is noise that buries every other line in the log.
    warned_ocr_engine: AtomicBool,
    /// Latched while the screen is idle, so the OCR engine is released once per idle stretch and not
    /// once per session — the pause counter cannot do that job, because it only ever reads zero once.
    engine_released: AtomicBool,
    cache_root: PathBuf,
    stats: Stats,
    mode: Mode,
    /// When false, the loop stops as soon as a segment closes: a one-shot run, which is what a
    /// scheduled task or a test wants.
    rotate: bool,
    titles: wintitle::Reader,
    /// The panels this run masks against, refreshed whenever the grabber is rebuilt — which is the
    /// one moment the topology is known to have changed. Enumerating them costs a window-message
    /// round trip, and a frame every few seconds does not need a fresher answer than that.
    panels: Vec<Tile>,
    /// The union of `panels`, i.e. `mss`'s `monitors[0]`, which is what an all-displays frame shows.
    desktop: Tile,
    /// `ocr_image_crop_URBL` as the file holds it: four percentages per display slot, in
    /// top/right/bottom/left order. Re-read with the rest of the plan, so a change in the settings
    /// page takes effect on the next frame rather than at the next start.
    urbl: Vec<i64>,
    /// The mask currently in force, rebuilt for every frame from `panels`, `desktop` and `urbl`.
    ///
    /// This is the recorder's privacy control, and it is applied to exactly one thing: the copy of the
    /// frame the OCR engine is handed. The JPEG written to disk, the footage the maintenance pass
    /// stitches and the stored thumbnail are made from the unmasked pixels, because the user recorded
    /// those and asked only that they not be searchable. Upstream is built the same way — see
    /// `windcap::crop`'s header — and a mask that also ate the footage would be a data-loss bug dressed
    /// as a security feature.
    mask: MaskPlan,
    /// The deferred pass this recorder launched and has not reaped yet, and whether the user asked
    /// for it by hand (`true`) or the schedule did (`false`).
    ///
    /// Held, not dropped: a pass nobody owns cannot be reported as running, cannot be refused a second
    /// start, and cannot be left to finish under a stop request. Before this field existed the handle
    /// was thrown away at spawn time, which is how an idle pass ran untracked across a whole session.
    maintain: Option<(std::process::Child, bool)>,
    /// The last capture state published to `RECORD_STATE.MD`.
    ///
    /// Held so the file is touched when the answer *changes* rather than every tick: the tick is seconds
    /// long, the tray only cares when the icon would look different, and a write per tick is background
    /// work serving nobody.
    published_capture: Option<wind_base::fslock::Capture>,
    /// The two settings files as last read: modification time and length.
    ///
    /// `reload_plan` compares against this instead of re-parsing both files on every tick, which used
    /// to be roughly 29k JSON parses a day for the seconds when nobody touched a setting. The length
    /// rides along because a save that lands inside the filesystem's timestamp granularity would
    /// otherwise look unchanged.
    plan_stamp: Option<SettingsStamp>,
}

/// The observable identity of the two settings files: `(defaults, user file)`, each as mtime and size.
type SettingsStamp = (
    (Option<std::time::SystemTime>, u64),
    (Option<std::time::SystemTime>, u64),
);

/// The stamp of one settings file, or `(None, 0)` when it does not exist — which is the ordinary
/// state of a fresh install that has never written a user setting.
fn settings_file_stamp(path: Option<&std::path::Path>) -> (Option<std::time::SystemTime>, u64) {
    let Some(path) = path else { return (None, 0) };
    match std::fs::metadata(path) {
        Ok(meta) => (meta.modified().ok(), meta.len()),
        Err(_) => (None, 0),
    }
}

/// Both files as they are on disk right now.
fn settings_stamp_of(config: &Config) -> SettingsStamp {
    let user = config.userdata_dir().join("config_user.json");
    (
        settings_file_stamp(config.defaults_path()),
        settings_file_stamp(Some(&user)),
    )
}

/// Does the plan have to be rebuilt? Pure, so the rule is testable without a screen or a settings file.
fn settings_changed(stored: Option<SettingsStamp>, current: SettingsStamp) -> bool {
    stored != Some(current)
}

/// The mask for one live frame.
///
/// The recorder's inputs, handed straight to the shared geometry: `source` is the desktop rectangle
/// [`Grabber`] stretched into `width` x `height` pixels, `panels` and `desktop` are what the OS
/// reported when that grabber was built, and `urbl` is the user's own list.
///
/// This function exists so the claim "the live path masks what the reindexer masks" is testable
/// without a screen: it is the whole of what `tick` contributes to the boundary, and the boundary
/// itself is in `windcap::crop` for both binaries to share.
fn mask_for_frame(
    width: u32,
    height: u32,
    source: VirtualDesktop,
    panels: &[Tile],
    desktop: Tile,
    urbl: &[i64],
) -> MaskPlan {
    MaskPlan::for_grab(width, height, Tile::from(source), panels, desktop, urbl)
}

/// Announce, once per process, the config the native recorder accepts but cannot yet honour.
///
/// A switch that is on and silently ignored is worse than a missing feature: the user believes
/// their browser addresses are being recorded, and finds out months later when a search result has
/// nothing to open. The `deep_linking` column is written as NULL by design until UIAutomation is
/// ported. The deleted Python implementation is the reference for what that costs: it walked the
/// accessibility tree per browser, and its Edge branch alone chained nine positional `foundIndex`
/// hops, so a port needs real browser windows to test against rather than a unit fixture.
fn warn_unported(plan: &Plan) {
    if plan.record_deep_linking {
        eprintln!(
            "note: record_deep_linking is on, but the native recorder has no UIAutomation support yet; \
             rows are indexed with an empty deep_linking, so a search result carries no link to reopen \
             the page it came from. Nothing records it on this branch -- there is no second recorder."
        );
    }
}

impl Recorder {
    /// Open a recording session.
    ///
    /// `mode` is the whole of what this run will do with a frame it keeps, and it arrives from the
    /// CLI's flags through one argument — there is no second, quieter path into it, which is the
    /// structural reason a flag cannot be parsed, echoed and then dropped on the floor here.
    pub fn open(config: Config, mode: Mode, rotate: bool) -> Result<Recorder, String> {
        windcap::capture::make_thread_dpi_aware();
        let plan = plan_from(&config, mode);
        let now = clock::now();
        let store = open_store(&config, &plan, now.year, now.month)?;
        let store_month = (now.year, now.month);
        let grabber = Grabber::new(CAPTURE_WIDTH).map_err(|e| e.to_string())?;
        // The mask's inputs are read once here and refreshed with the grabber. A recorder that asked
        // the OS where its monitors are on every tick would pay for an enumeration it does not need;
        // one that never asked again would keep masking a layout that was unplugged mid-run.
        let (panels, desktop) = Self::topology();
        let urbl = config.i64_list(windcap::crop::CONFIG_KEY);
        let (target_width, target_height) = grabber.target_size();
        let mask = mask_for_frame(target_width, target_height, grabber.source(), &panels, desktop, &urbl);
        let engine = OcrEngine::from_config(&config, std::env::temp_dir());
        if mode.reads_text() {
            if !engine.is_installed() {
                eprintln!(
                    "warning: OCR engine missing at {exe} — every frame will still be captured and \
                     indexed on its window title, but no text on screen will be searchable. \
                     `windrec doctor` says why; `windsetup engines` benchmarks the engine.",
                    exe = engine.program().display()
                );
            }
            // The config asked for an engine this install cannot drive. Indexing the user's screen with a
            // substitute is survivable; doing it without saying so is not.
            if let Some(note) = engine.note() {
                eprintln!("warning: {note}");
            }
        }
        warn_unported(&plan);
        // Said before the first frame, from the same list the first frame will be masked with.
        warn_crop(&urbl, &panels);
        let segment = Segment::opening(&now);
        // Before a single frame is written: whatever the last instance left half-committed is this
        // instance's first job, because its own change gate and segment state are about to move past
        // the directories it is recovering.
        let swept = sweep_stranded(&config, &plan, &segment.dir_name);
        report_sweep(&swept);
        Ok(Recorder {
            cache_root: config.cache_screenshot_dir(),
            titles: wintitle::Reader::start(config.win_title_dir()),
            config,
            plan,
            store,
            store_month,
            grabber,
            // A zero-sized source forces the first tick to build the grabber for the real rect.
            source: VirtualDesktop { x: 0, y: 0, width: 0, height: 0 },
            gate: ChangeGate::new(GateConfig::default()),
            engine,
            segment,
            idle: IdleTracker::default(),
            warned_missing_display: AtomicBool::new(false),
            warned_ocr_engine: AtomicBool::new(false),
            engine_released: AtomicBool::new(false),
            stats: Stats {
                fastest_grab_millis: f64::INFINITY,
                swept_segments: swept.replayed,
                swept_rows: swept.rows_committed,
                ..Stats::default()
            },
            mode,
            rotate,
            panels,
            desktop,
            urbl,
            mask,
            maintain: None,
            published_capture: None,
            plan_stamp: None,
        })
    }

    /// The panels attached right now, and their union, in desktop coordinates.
    ///
    /// The same two calls the reindexer makes, so the mask both paths compute is measured from the same
    /// description of the desktop.
    fn topology() -> (Vec<Tile>, Tile) {
        let panels = windcap::capture::monitors().into_iter().map(Tile::from).collect();
        (panels, Tile::from(virtual_desktop()))
    }

    /// Re-read the desktop after a topology change and rebuild everything measured against it.
    fn refresh_topology(&mut self) {
        let (panels, desktop) = Recorder::topology();
        self.panels = panels;
        self.desktop = desktop;
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Where slices are written. Reported by `run` so a user can find the frames.
    pub fn slice_root(&self) -> &Path {
        &self.cache_root
    }

    /// Override the capture interval for a one-off run.
    ///
    /// A diagnostic knob only: it changes how often the screen is sampled, not what is considered
    /// worth keeping, so it is applied after the plan is read and never persisted.
    pub fn override_interval(&mut self, seconds: i64) {
        self.plan.interval_seconds = seconds.max(1);
    }

    
/// Read the config once, at the top of a tick, so a settings change is picked up without a
    /// restart. Only the cheap-to-hot-reload keys are honoured live; anything that changes what a
    /// file on disk means (paths, user name) needs a restart, and says so here rather than
    /// half-applying it.
    pub fn reload_plan(&mut self) {
        // Nothing on disk has moved since the last read, so neither has the plan. The stamp is taken
        // from the files rather than from a timer: a setting the user changed is picked up on the next
        // tick, which is the guarantee the privacy mask's hot-reload makes below.
        let stamp = settings_stamp_of(&self.config);
        if !settings_changed(self.plan_stamp, stamp) {
            return;
        }
        let reloaded = match Config::load(self.config.root()) {
            Ok(c) => c,
            Err(_) => return,
        };
        self.plan_stamp = Some(stamp);
        self.config = reloaded;
        self.plan = plan_from(&self.config, self.mode);
        // The privacy control is hot-reloaded with the rest of the plan. A user who widens the mask
        // because something on screen should not have been indexed has to be able to do it from the
        // settings page and have the next frame honour it; making them restart the recorder first
        // would index the very thing they just asked to exclude.
        self.urbl = self.config.i64_list(windcap::crop::CONFIG_KEY);
    }

    /// Which of the three things this run does with a kept frame. Reported so the closing line can
    /// say "kept 0 frames" honestly rather than meaning "kept 0 rows".
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Tell the tray what this tick did, so the icon can mean something.
    ///
    /// The recorder holds its lock for its whole life, including through the hours when it is
    /// deliberately not capturing, so "a live lock" has never been the same fact as "recording" — and a
    /// tray that reads only the lock says 正在记录 over an untouched night and 暂停记录 when nothing of
    /// ours is running. This is the third answer, and it is written by the only process that knows it.
    fn publish_capture(&mut self, state: wind_base::fslock::Capture) {
        if self.published_capture == Some(state) {
            return;
        }
        self.published_capture = Some(state);
        if let Err(e) = wind_base::fslock::publish_capture(&self.config.record_state_path(), state) {
            // A state that cannot be published costs the tray its third answer and must not cost the
            // capture its tick.
            eprintln!("capture state: could not publish {}: {e}", state.as_str());
        }
    }

    /// One observation: session state, grab, change gate, exclusion, OCR, offer.
    pub fn tick(&mut self) -> Result<(), String> {
        let cycle_start = std::time::Instant::now();
        let now = clock::now();
        let seconds = now.naive_epoch_seconds();

        // First thing, ahead of the session probe: a pass that finished while the screen was locked is
        // reaped here, and a hand request is answered here rather than whenever the machine next goes
        // idle — which, on a machine being used, is the answer to "never".
        self.maintenance_control(&now);

        if self.store_month != (now.year, now.month) {
            self.close_segment()?;
            self.store = open_store(&self.config, &self.plan, now.year, now.month)?;
            self.store_month = (now.year, now.month);
        }

        // Checked before the session probe because a resume usually *is* an unlock, and the drift
        // answer arrives without touching the process table at all.
        if segment::sleep_intervened(winstate::sleep_drift_seconds(), self.plan.sleep_drift_limit_seconds) {
            self.stats.skipped_sleep += 1;
            self.publish_capture(wind_base::fslock::Capture::SleepDrift);
            self.reset_gate();
            self.wait(cycle_start);
            return Ok(());
        }

        let session = winstate::snapshot();
        if !session.recordable() {
            self.stats.skipped_session += 1;
            self.publish_capture(wind_base::fslock::Capture::SessionLocked);
            // Whatever was on screen before the lock is not a baseline for after it.
            self.reset_gate();
            // A locked screen is the one case where "nothing changed" is certain without grabbing.
            self.idle.observe(true, self.plan.pause_after_idle_minutes);
            self.wait(cycle_start);
            return Ok(());
        }

        if self.idle.paused {
            if self.stats.paused_ticks == 0 {
                eprintln!(
                    "screen unchanged for {:.1} minutes: pausing, Ctrl-C or activity resumes recording",
                    self.idle.minutes()
                );
            }
            self.stats.paused_ticks += 1;
            self.publish_capture(wind_base::fslock::Capture::ScreenUnchanged);
            // The OCR engine goes with the pause, and the latch is per pause rather than per session:
            // `paused_ticks` reaches zero only once, so hanging this off that test would let every
            // later idle stretch keep the engine resident anyway. It is a separate process with ~21 MB
            // of loaded models, and it is asked something only when a frame is kept — through the night
            // on an untouched machine, a cost serving nobody. The next `recognize` starts it again on
            // demand, which is the same path a failed engine already takes, so no wake call pairs with
            // this one.
            if !self.engine_released.swap(true, Ordering::Relaxed) {
                wind_base::wxocr::shutdown();
            }
            // Idle is the only time the maintenance pass may run: converting slices to video and
            // pruning expired files is exactly the kind of disk work that would stutter a live
            // capture. The close happens first, so maintenance never sees a half-written segment.
            if self.segment.is_committable() {
                self.close_segment()?;
            }
            // ...and that is the whole trap: `close_segment` was the only caller of `idle.resume()`,
            // it stops being committable one tick after the pause, and nothing past this point ever
            // reads the pixels again — so nothing could clear `paused`. A machine left unrecorded
            // from the moment one picture sat still for five minutes is what this branch is for.
            // The way out is the session snapshot taken above: input inside the poll window means
            // somebody is back at this desk, whatever the last frame looked like.
            if segment::resumed_from_input(session.idle_seconds) {
                self.idle.resume();
                // The frame the gate remembers predates the pause and is no baseline for what comes
                // after it — the same reason the locked-session branch above resets it.
                self.reset_gate();
                self.stats.resumed_from_pause += 1;
                return Ok(());
            }
            self.maybe_launch_maintenance(&now);
            std::thread::sleep(std::time::Duration::from_secs(segment::PAUSE_POLL_SECS));
            return Ok(());
        }
        // Working again, so the next time this stretch of idleness ends the engine is worth putting
        // down again rather than leaving up for the rest of the session.
        self.engine_released.store(false, Ordering::Relaxed);

        let rect = self.capture_source();
        if rect != self.source {
            self.source = rect;
            self.grabber = Grabber::with_source(CAPTURE_WIDTH, rect, false).map_err(|e| e.to_string())?;
            self.stats.grabber_rebuilds += 1;
            self.refresh_topology();
        }

        let grab_start = std::time::Instant::now();
        let frame = match self.grabber.grab() {
            Ok(Some(f)) => f,
            Ok(None) => {
                // Monitors were added, removed or reordered: rebuild for the new virtual desktop, and
                // re-read the panels the mask is measured against while doing it. A mask left pointing
                // at a panel that has been unplugged hides a region that is no longer on screen and
                // leaves the one that is newly attached unprotected.
                self.grabber = Grabber::new(CAPTURE_WIDTH).map_err(|e| e.to_string())?;
                self.source = VirtualDesktop { x: 0, y: 0, width: 0, height: 0 };
                self.stats.grabber_rebuilds += 1;
                self.refresh_topology();
                return Ok(());
            }
            Err(e) => {
                eprintln!("grab failed: {e}");
                self.stats.failures += 1;
                self.wait(cycle_start);
                return Ok(());
            }
        };
        let grab_dt = grab_start.elapsed().as_secs_f64() * 1000.0;
        self.stats.grab_millis += grab_dt;
        self.stats.grabs += 1;
        // A frame is in hand: this is the tick the icon is supposed to advertise.
        self.publish_capture(wind_base::fslock::Capture::Capturing);
        if grab_dt < self.stats.fastest_grab_millis {
            self.stats.fastest_grab_millis = grab_dt;
        }
        self.stats.widest_source_mp = self
            .stats
            .widest_source_mp
            .max(f64::from(self.source.width * self.source.height) / 1e6);
        let (w, h) = (frame.width as usize, frame.height as usize);
        // The mask is rebuilt for every frame from the cached topology — arithmetic only, no syscalls —
        // because it is measured against *this* frame's pixels. A plan left at last tick's size is a
        // band landing in the middle of the screen.
        self.mask = mask_for_frame(frame.width, frame.height, self.source, &self.panels, self.desktop, &self.urbl);

        // The gate deliberately scores the *unmasked* frame, as upstream's
        // `compare_image_similarity_np(screenshot_previous, screenshot_current)` does: the question it
        // answers is "did anything move", and answering it from masked pixels would let a change that
        // happens entirely inside an excluded region pass for a frozen screen. Exclusion is about what
        // becomes searchable, not about what is worth keeping.
        let decision = self.gate.observe(frame.width, frame.height, &frame.luma);
        self.idle.observe(!decision.changed, self.plan.pause_after_idle_minutes);
        if !decision.changed {
            self.stats.dropped_unchanged += 1;
            self.wait(cycle_start);
            return Ok(());
        }

        if self.plan.record_only_gate() {
            self.stats.gated += 1;
            self.wait(cycle_start);
            return Ok(());
        }

        let title = self.titles.current();
        let frame_name = format!("{}.jpg", now.stamp());
        let (text, ocr) = if self.plan.defer_text && matches!(self.mode, Mode::Full) {
            // No engine call in the tick. What has to happen *here* is the masked copy: the mask, the
            // panel geometry and these pixels are all live right now, and a window that later re-read
            // the raw JPEG would be recognising exactly the edges the user asked never to become
            // searchable. So the copy is written next to the frame and the row is stored with no text.
            let (masked, painted) = windcap::crop::masked_copy(&frame.rgb, w, h, &self.mask);
            if painted > 0 {
                self.stats.masked += 1;
            }
            if let Err(e) = self.write_frame(&masked, w, h, &Self::cropped_name(&frame_name)) {
                // The frame itself is still kept: a row that misses its masked copy is a row the pass
                // reports as unreadable, which is honest, whereas losing the pixels would not be.
                eprintln!("masked copy failed: {e} — the row will wait for text that cannot be read");
            }
            (String::new(), OcrOutcome::Unavailable)
        } else { match self.mode {
            // `recognize_masked`, never `recognize`: the only copy of these pixels the engine may see is
            // the one with the user's excluded edges painted over. The raw buffer stays raw — it is what
            // the JPEG on disk, the footage and the thumbnail below are all made from.
            Mode::Full => match self.engine.recognize_masked(&frame.rgb, w, h, &self.mask) {
                Ok((text, painted)) => {
                    if painted > 0 {
                        self.stats.masked += 1;
                    }
                    (text, OcrOutcome::Read)
                }
                Err(e) => {
                    // An engine that cannot run cannot start running again mid-process, so this is
                    // said once and counted from then on. It used to be said every frame *and* cost
                    // the user the frame, which is how a quarantined .exe quietly zeroed a day.
                    self.stats.ocr_unavailable += 1;
                    if !self.warned_ocr_engine.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "ocr failed: {e} — keeping the frame and indexing its window title; \
                             this line is not repeated, the closing report counts them"
                        );
                    }
                    (String::new(), OcrOutcome::Unavailable)
                }
            },
            // `GateOnly` returned above: it never reaches an offer. It shares the arm because the
            // answer it would give is the same one — no text was read.
            Mode::TitlesOnly | Mode::GateOnly => (String::new(), OcrOutcome::Unavailable),
        }};

        let candidate = wind_store::Record {
            videofile_name: self.segment.videofile_name.clone(),
            // Basename only. The slice directory gains a `-SUBMIT`/`-VIDEO` marker once it is
            // closed, so an absolute path captured here would point at a directory that stops
            // existing; the segment's own name locates it by its invariant 19-character prefix.
            picturefile_name: frame_name,
            videofile_time: seconds,
            ocr_text: text,
            win_title: title,
            deep_linking: None,
            // From the *unmasked* pixels, which is upstream's choice too: it thumbnails
            // `img_orgin_not_crop_filepath`, not the `_cropped` copy. A black bar here would hide from
            // the user what their own footage contains, and protect nothing — the full frame is on disk
            // behind this row either way.
            //
            // With a window named, this encode is not done here at all: the row is written with no
            // preview and the `previews` step redraws it from the JPEG this frame is about to become.
            // That is the safe half of the deferral rule — a preview can be recomputed from what is
            // already on disk, whereas text cannot be filled in later without writing the masked copy
            // of the frame at capture time as well, and a row that waits for text that never arrives is
            // permanently unsearchable. So the engine stays in the tick and the preview does not.
            thumbnail: if self.plan.defer_previews {
                None
            } else {
                image::thumbnail_base64(
                    &frame.rgb,
                    w,
                    h,
                    self.config.thumbnail_width(),
                    self.config.thumbnail_quality(),
                )
                .ok()
            },
        };

        match self.segment.offer(seconds, &candidate, &self.plan, ocr) {
            Ok(()) => {
                if let Err(e) = self.write_frame(&frame.rgb, w, h, &candidate.picturefile_name) {
                    // This one *is* a lost frame: the JPEG is the thing the row points at.
                    eprintln!("frame write failed: {e}");
                    self.stats.failures += 1;
                    return Ok(());
                }
                // Journal the row only once its frame is on disk. The order is the whole point of
                // the journal: it can fall behind the pixels by at most the one write that was in
                // flight when the process died, and it can never run ahead of them — a replayed row
                // always has a JPEG behind it.
                if let Err(e) = Journal::append(
                    &self.segment.directory(&self.cache_root),
                    &self.segment.journal_owner(),
                    &candidate,
                ) {
                    eprintln!("journal write failed: {e}");
                    self.stats.journal_failures += 1;
                }
                self.stats.kept += 1;
            }
            Err(Rejection::Empty) => self.stats.dropped_empty += 1,
            Err(Rejection::SameAsPrevious) => self.stats.dropped_repeat += 1,
            Err(Rejection::Excluded) => self.stats.dropped_excluded += 1,
            Err(Rejection::TooEarly) => self.stats.dropped_early += 1,
        }

        if self.segment.is_broken(&self.plan) {
            eprintln!("segment {} hit the interrupt ceiling, restarting", self.segment.stamp);
            self.close_segment()?;
        } else if self.segment.is_full(seconds, &self.plan) {
            self.close_segment()?;
        }
        self.wait(cycle_start);
        Ok(())
    }

    /// Close the current segment: collapse its repeats, commit its rows, mark its directory.
    ///
    /// The order is the point. Rows are committed *before* the directory is renamed, so a crash
    /// between the two leaves frames on disk that the index already describes — recoverable by the
    /// maintenance pass — rather than a renamed directory whose rows were never written, which is
    /// silently lost history.
    ///
    /// The journal outlives every step it is not settled with. `store.append` returning `Err` leaves
    /// it in place, and so does dying anywhere between the commit and the deletion: the next
    /// instance replays it, and replay dedups against the rows that are already indexed, so the
    /// window costs a duplicate check rather than a second copy of the user's history.
    pub fn close_segment(&mut self) -> Result<(), String> {
        let segment = std::mem::replace(&mut self.segment, Segment::opening(&clock::now()));
        if segment.records.is_empty() {
            // Nothing was captured, and a journal is only ever created by a captured frame, so this
            // directory holds no evidence and no pending rows: remove it. The sweep is the other half
            // of the story — a directory the recorder never even got to write a frame into (a kill
            // between `create_dir_all` and the first JPEG) is retired there.
            let _ = std::fs::remove_dir_all(self.cache_root.join(&segment.dir_name));
            return Ok(());
        }

        let mut segment = segment;
        // With text deferred, every row in here has an empty body, and a fold that compares empty
        // strings to each other would collapse the whole segment into its first line. The pass does
        // this folding after it has read the text back.
        let dropped = if self.plan.defer_text {
            0
        } else {
            segment.collapse_repeats(&self.plan)
        };
        self.stats.collapsed += dropped;

        let committed = if segment.is_committable() {
            self.store.append(&segment.records).map_err(|e| e.to_string())?
        } else {
            // Too thin to make a playable video. The frames are kept: they cost a directory, and
            // deleting user pixels on a threshold judgement is the wrong trade.
            eprintln!(
                "segment {} holds {} rows, below the 5-row floor: left uncommitted",
                segment.stamp,
                segment.records.len()
            );
            0
        };
        self.stats.rows_committed += committed;
        self.stats.segments_closed += 1;

        let source = self.cache_root.join(&segment.dir_name);
        // Committed segments gain a nested `-SUBMIT` marker and keep their name, so the maintenance
        // pass still finds them; discarded ones are renamed, because they are never coming back.
        let mut settled = true;
        let outcome = if committed > 0 {
            let marker = source.join(segment.submit_marker());
            match std::fs::create_dir_all(&marker) {
                Ok(()) => marker.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string(),
                Err(e) => {
                    // The rows are committed and the frames are on disk; a missing marker means the
                    // conversion pass will not pick this slice up, which is recoverable by hand and
                    // is not a reason to lose the segment or stop recording over it.
                    settled = false;
                    eprintln!("could not mark {marker}: {e}", marker = marker.display());
                    "UNMARKED".to_string()
                }
            }
        } else {
            let target = self.cache_root.join(segment.discarded_dir());
            if source.exists() {
                std::fs::rename(&source, &target)
                    .map_err(|e| format!("could not mark {}: {e}", source.display()))?;
            }
            target.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string()
        };
        // The journal is deleted only once the segment is in a state the pipeline can act on. The
        // `?` above and the `store.append` before it are the two ways to leave this function early,
        // and both leave the journal behind on purpose: it is the only copy of `win_title` and the
        // per-frame text that will ever exist for these frames. An unmarked-but-committed segment
        // keeps it too, because the next sweep replays it, finds every row already in the index, and
        // does nothing but write the marker back.
        if settled && !segment::clear_journals(&source).is_empty() {
            eprintln!("could not clear the journal in {}", source.display());
        }
        eprintln!(
            "closed {} : {} rows committed, {} collapsed, marked {}",
            segment.stamp, committed, dropped, outcome
        );

        if self.rotate {
            self.segment = Segment::opening(&clock::now());
            self.idle.resume();
        } else {
            // A one-shot run ends at its segment boundary; make the loop condition stop.
            request_stop();
        }
        Ok(())
    }

    /// Run until `deadline`, or forever when `until` is `None`, honouring a stop request.
    pub fn drive(&mut self, until: Option<std::time::Instant>) -> Result<(), String> {
        while !stop_requested() {
            if let Some(deadline) = until {
                if std::time::Instant::now() >= deadline {
                    break;
                }
            }
            self.reload_plan();
            self.tick()?;
        }
        if self.segment.records.is_empty() {
            let _ = std::mem::replace(&mut self.segment, Segment::opening(&clock::now()));
        } else {
            self.close_segment()?;
        }
        self.titles.shutdown();
        Ok(())
    }

    /// The masked sibling of a frame's own name: `2026-09-27_03-30-00.jpg` becomes the `_cropped` copy
    /// the back-indexer names the same way, so one convention covers both writers.
    fn cropped_name(frame: &str) -> String {
        frame.replace(".jpg", "_cropped.jpg")
    }

    fn write_frame(&self, rgb: &[u8], w: usize, h: usize, name: &str) -> Result<(), String> {
        let dir = self.cache_root.join(&self.segment.dir_name);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let jpeg = image::encode_jpeg(rgb, w, h, 92)?;
        std::fs::write(frame_path(&dir, name)?, jpeg).map_err(|e| e.to_string())
    }

    /// The rectangle this frame comes from.
    ///
    /// Foreground-window mode takes precedence, exactly as upstream's config does. The rest is the
    /// display strategy, and `single` is the case that used to be silently substituted: on a
    /// three-panel 5920x2880 desktop, "capture display 1" quietly captured all three, which is both
    /// a 17 MP grab and a frame whose pixels match no monitor the user named.
    fn capture_source(&self) -> VirtualDesktop {
        if self.config.bool_or("record_screenshot_method_capture_foreground_window_only", true) {
            return foreground_rect().unwrap_or_else(virtual_desktop);
        }
        if self.plan.display_strategy == "single" {
            return monitor_rect(self.plan.single_display_index).unwrap_or_else(|| {
                if !self.warned_missing_display.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "display {} is not attached; capturing the whole virtual desktop instead",
                        self.plan.single_display_index
                    );
                }
                virtual_desktop()
            });
        }
        virtual_desktop()
    }

    /// Throw away the change gate's anchor and start from the next frame.
    ///
    /// Rebuilding is cheaper than a per-frame "orphan" state inside the gate, and it is the honest
    /// thing to do after anything that means the previous frame is no longer a fair comparison: a
    /// resume, a lock, a monitor change.
    fn reset_gate(&mut self) {
        let config = self.gate.config();
        self.gate = ChangeGate::new(config);
    }

    fn wait(&self, cycle_start: std::time::Instant) {
        let interval = self.plan.interval_seconds.max(1) as f64;
        let remaining = interval - cycle_start.elapsed().as_secs_f64();
        if remaining > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(remaining));
        }
    }

    /// Launch `windmaint` if the screen has been idle long enough since the last time it ran.
    ///
    /// The recorder does not wait for it, and does not track whether it succeeded: `windmaint`
    /// takes the maintain lock itself, so a repeated launch here is harmless and a failed one is
    /// its own report. Spawning is the whole extent of the coupling between the two processes.
    ///
    /// The one argument beyond the root is `--idle-granted-by <our pid>`. The maintenance pass now
    /// also runs the reindex and AI-tag steps, and those must not fire while a recorder is
    /// actively capturing — but they *must* fire in this window, during which this (paused, idle)
    /// recorder still legally holds the record lock for its whole life. Handing over our pid is what
    /// lets `windmaint` tell "the idle process that launched me" apart from "some other live capture,"
    /// so those steps run exactly here and nowhere a hand-run pass could race a recording.
    ///
    /// Its output goes to `cache/logs/windmaint-idle.log`, appended, because this pass is the only
    /// thing that ever deletes or re-encodes and it is not a `windsvc` child, so the supervised log
    /// rotation never sees it: dropped instead, every "N segment(s) expired, M file(s) removed" line
    /// would be discarded by the one run whose deletions a user has to be able to account for.
    fn maybe_launch_maintenance(&mut self, now: &clock::LocalParts) {
        if self.maintain.is_some() {
            return;
        }
        let last = std::fs::read_to_string(self.config.last_idle_maintain_path())
            .ok()
            .and_then(|body| body.trim().parse::<i64>().ok());
        // A named window replaces the idle-gap rule outright rather than joining it. Two triggers that
        // both fire is how a machine gets two passes in one night, and the user who set `03:30` set it
        // instead of the old rule, not alongside it.
        let due = match self.plan.maintain_window {
            Some(window) => scheduled_pass_is_due(window, last, now),
            None => maintenance_is_due(last, now.naive_epoch_seconds(), self.plan.maintain_after_idle_minutes),
        };
        if due {
            self.launch_maintenance(now, false);
        }
    }

    /// Every tick, before anything else: collect a finished pass, and honour a hand request.
    ///
    /// The manual request is checked here and not in the idle-paused branch, because a person who
    /// pressed "整理现在" in the settings page has just authorised the disk work by hand — that is the
    /// entire difference between the button and the schedule — and they are sitting at a changing
    /// screen while it is being asked for.
    fn maintenance_control(&mut self, now: &clock::LocalParts) {
        self.reap_maintenance();
        if self.maintain.is_some() {
            return;
        }
        // Nothing is running, so a stop request on disk belongs to nobody. Swept here rather than left,
        // because the next pass would otherwise stop itself before doing the work that was asked for —
        // and `--manual` in particular would look like a button that does nothing.
        if self.config.maintain_stop_requested() {
            self.config.clear_maintain_stop();
        }
        // `take` rather than `exists`: a request nobody consumes would relaunch the pass every tick
        // for the rest of the session, which is the same bug the window-raise signal had and solved.
        if !fslock::take_show_request(&self.config.maintain_start_signal_path()) {
            return;
        }
        self.launch_maintenance(now, true);
    }

    /// Has the owned pass stopped? Report it, so the next window can start another one.
    fn reap_maintenance(&mut self) {
        let finished = match self.maintain.as_mut() {
            Some((child, _)) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
            None => false,
        };
        if !finished {
            return;
        }
        let Some((mut child, manual)) = self.maintain.take() else { return };
        let kind = if manual { "hand-requested" } else { "scheduled" };
        match child.wait() {
            Ok(status) => eprintln!("maintenance: the {kind} pass (pid {}) finished with {status}", child.id()),
            Err(e) => eprintln!("maintenance: the {kind} pass (pid {}) is gone: {e}", child.id()),
        }
    }

    /// Start one deferred pass and keep its handle. `manual` is the button, and it is also a word
    /// `windmaint` itself reads: a hand-requested pass ignores the closing of the window, because
    /// nothing scheduled it to be inside the window in the first place.
    fn launch_maintenance(&mut self, now: &clock::LocalParts, manual: bool) {
        let Some(binary) = maintenance_binary() else {
            if manual {
                eprintln!("maintenance: nothing to launch — no windmaint.exe beside this recorder");
            }
            return;
        };
        let path = self.config.last_idle_maintain_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Written before the spawn, not after: a pass that dies on contact with a broken install must
        // not be relaunched on every tick of a nine-hour window. The cost of that ordering is that a
        // stopped pass is not retried until the next window, which is the honest reading of "留到明天".
        let _ = std::fs::write(&path, now.naive_epoch_seconds().to_string());
        let log = self.config.log_dir().join("windmaint-idle.log");
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Twice on purpose: one `File` cannot be inherited by both streams, and a failure to open the
        // log costs the pass its report but must not cost it the run.
        let append = || match std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            Ok(file) => std::process::Stdio::from(file),
            Err(_) => std::process::Stdio::null(),
        };
        let granted_by = std::process::id().to_string();
        let mut command = std::process::Command::new(&binary);
        command
            .args(["all", "--root"]).arg(self.config.root())
            .args(["--idle-granted-by"]).arg(&granted_by)
            .current_dir(self.config.root())
            .stdout(append())
            .stderr(append());
        if manual {
            command.arg("--manual");
        }
        let kind = if manual { "hand-requested" } else { "scheduled" };
        match command.spawn() {
            Ok(child) => {
                eprintln!(
                    "maintenance: launched the {kind} pass (pid {}) from {} — its report is appended to {}",
                    child.id(),
                    binary.display(),
                    log.display()
                );
                self.maintain = Some((child, manual));
            }
            Err(e) => eprintln!("maintenance launch failed: {e}"),
        }
    }
}

/// The privacy mask as this machine and this config combine to apply it, for `windrec doctor`.
///
/// A control the user cannot see is not a control. The settings page now edits this one, so this line is
/// the check on what the edit became: it says three things, the raw list as the file holds it, the order
/// that list is read in, and the rows and columns it actually paints on each frame this recorder could
/// produce — because "6%" tells nobody whether their taskbar is inside the mask and "64 rows on a
/// 1080-row panel" does.
///
/// The list is padded, not truncated, in its own description: four entries on a machine with three
/// panels means the second and third panels take the shipped default, which is upstream's rule and the
/// one direction that cannot leave a region readable. Saying that out loud is the difference between a
/// user believing they hid one edge and knowing which two they left showing.
///
/// Printed by `doctor` in `main.rs`, one line per shape of frame, under the `ocr mask` label.
pub fn describe_crop(config: &Config) -> String {
    // Panel sizes are only real pixels on a per-monitor-DPI-aware thread; `Recorder::open` does the
    // same thing for the same reason, and it is idempotent.
    windcap::capture::make_thread_dpi_aware();
    let key = windcap::crop::CONFIG_KEY;
    let urbl = config.i64_list(key);
    let (panels, desktop) = Recorder::topology();
    let listed = urbl.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
    let mut lines = vec![format!(
        "{key} = [{listed}] — four percentages per display, read in the key's own order: top, right, bottom, left"
    )];
    if urbl.is_empty() {
        lines.push(format!(
            "  the key is absent, so every display takes the shipped default {:?}",
            windcap::crop::FALLBACK_URBL
        ));
    } else if urbl.len() < 4 {
        lines.push(format!(
            "  the list is shorter than one display's four values, so every display takes the shipped default {:?}",
            windcap::crop::FALLBACK_URBL
        ));
    } else if (urbl.len() / 4) < panels.len().max(1) {
        lines.push(format!(
            "  the list holds {} display slot(s) and this machine has {}, so the panels past the end take \
             the shipped default {:?}",
            urbl.len() / 4,
            panels.len(),
            windcap::crop::FALLBACK_URBL
        ));
    }

    // One line per shape of frame this recorder can actually write, because the mask is proportional and
    // a percentage means a different number of rows on every one of them.
    let mut plans: Vec<(String, MaskPlan)> = Vec::new();
    if panels.len() > 1 {
        for (index, panel) in panels.iter().enumerate() {
            let (w, h) = (u32::try_from(panel.width).unwrap_or(0), u32::try_from(panel.height).unwrap_or(0));
            let plan = MaskPlan::for_grab(w, h, *panel, &panels, desktop, &urbl);
            plans.push((format!("a grab of display {} ({w}x{h})", index + 1), plan));
        }
    }
    let (dw, dh) = (u32::try_from(desktop.width).unwrap_or(0), u32::try_from(desktop.height).unwrap_or(0));
    plans.push((format!("an all-displays frame ({dw}x{dh})"), MaskPlan::for_grab(dw, dh, desktop, &panels, desktop, &urbl)));
    plans.push((
        format!("a foreground-window grab (any size, proportionally)"),
        MaskPlan::whole_frame(1000, 1000, &urbl),
    ));

    let masks_anywhere = plans.iter().any(|(_, plan)| !plan.is_empty());
    if !masks_anywhere {
        lines.push(
            "  WARNING: every edge is zero on every frame shape, so nothing on screen is excluded from \
             OCR — the taskbar, the clock and the notification corners are indexed like everything else"
                .to_string(),
        );
    }
    for (shape, plan) in &plans {
        lines.push(format!("  {shape}: {}", plan.describe()));
    }
    lines.push(
        "  painted on the OCR input only — the saved frame, the footage and the thumbnail keep every \
         pixel that was recorded"
            .to_string(),
    );
    lines.join("\n")
}

/// The mask line of the closing report, or `None` when this run had no OCR to mask.
///
/// `report` in `main.rs` prints one conditional line per counter for exactly this reason: a run that
/// masked nothing has to be able to say so, and a run that masked everything has to be able to prove
/// it. Counted against `kept` rather than `grabs` because a kept frame is the one with a row behind it.
///
/// Printed by `report` in `main.rs`, which prints one conditional line per counter for exactly this
/// reason.
pub fn masked_line(stats: Stats, mode: Mode) -> Option<String> {
    if !mode.reads_text() {
        // `--no-ocr` and `--gate-only` never hand a frame to the engine, so there is no privacy boundary
        // in this run for the counter to describe. Saying "0 frames masked" here would read like a
        // failure of a control that was never engaged.
        return None;
    }
    if stats.masked > 0 {
        return Some(format!(
            "{} of {} kept frames had the excluded screen edges painted out of what OCR read \
             (the frames themselves are untouched)",
            stats.masked, stats.kept
        ));
    }
    if stats.kept == 0 {
        return None;
    }
    Some(format!(
        "warning: none of the {} kept frames had anything masked — {:?} excludes no edge, or the \
         frames match no attached display; nothing at the screen edges is being kept out of this index",
        stats.kept,
        windcap::crop::CONFIG_KEY
    ))
}

/// Announce, once per process, a `ocr_image_crop_URBL` that will not hide anything.
///
/// The mirror image of [`warn_unported`], and it earns the same one line for the same reason: an
/// all-zero mask is a privacy control that is *off* while looking configured, and the only other place
/// it can surface is `windrec doctor`, which a user runs after they have already searched for something
/// they did not want indexed. Zero on one display slot of a three-panel machine is not this case — the
/// panels past the end of a short list take the shipped default — so the check is per slot, against the
/// panels actually attached.
fn warn_crop(urbl: &[i64], panels: &[Tile]) {
    if crop_excludes_nothing(urbl, panels) {
        eprintln!(
            "warning: {key} excludes nothing on every attached display, so the taskbar, the clock and \
             the notification corners are indexed like the rest of the screen. Set a percentage above \
             zero to keep an edge out of the index; `windrec doctor` prints the mask in force.",
            key = windcap::crop::CONFIG_KEY
        );
    }
}

/// Whether this list, applied to these panels, paints no pixel anywhere.
///
/// Zero on one slot of a multi-panel machine is *not* this case: a panel past the end of a short list
/// takes the shipped default band, so something is still hidden. That is why the check asks the shared
/// geometry what each slot resolves to rather than scanning the numbers in the file.
fn crop_excludes_nothing(urbl: &[i64], panels: &[Tile]) -> bool {
    let slots = panels.len().max(1);
    (0..slots).all(|index| windcap::crop::Urbl::slot(urbl, index).is_empty())
}

/// Is it time to run the idle maintenance pass again?
///
/// No history at all means "yes" — a first idle stretch after install is exactly when the slices
/// the recorder has already written need becoming video.
pub fn maintenance_is_due(last_run: Option<i64>, now: i64, gap_minutes: i64) -> bool {
    if gap_minutes <= 0 {
        return false;
    }
    match last_run {
        None => true,
        Some(at) => now - at >= gap_minutes * 60,
    }
}

/// Is the scheduled pass due at this instant?
///
/// Two conditions, both necessary: the clock is inside the window, and this window has not already
/// been spent. The second one is asked against [`window_date`] rather than the calendar date, because
/// a window that crosses midnight is one appointment and not two — answering with the calendar date
/// would let a 22:00-06:00 window start a pass at 23:05 and a second one at 01:00, since 01:00 looks
/// like a fresh day.
pub fn scheduled_pass_is_due(window: MaintainWindow, last_run: Option<i64>, now: &clock::LocalParts) -> bool {
    if !window.contains(now.minute_of_day()) {
        return false;
    }
    let Some(at) = last_run else { return true };
    clock::LocalParts::from_naive_epoch(at).date_only().date_stamp() != window_date(window, now).date_stamp()
}

/// The date this window belongs to: the night it opened on, not the date the clock now shows.
fn window_date(window: MaintainWindow, now: &clock::LocalParts) -> clock::LocalParts {
    let crosses_midnight = window.start_minutes > window.end_minutes;
    if crosses_midnight && now.minute_of_day() < window.end_minutes {
        // Inside the after-midnight leg, so the window opened yesterday evening.
        return clock::LocalParts::from_naive_epoch(now.naive_epoch_seconds() - 86_400).date_only();
    }
    now.date_only()
}

/// Where `windmaint.exe` is, in the same search order the Python supervisor uses
/// (`windrecorder::native_runtime`): beside the recorder, then an installed `bin/`, then a cargo
/// release tree, then a debug one. A debug build is found and labelled as such rather than
/// silently preferred.
pub fn maintenance_binary() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("windmaint.exe"));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("windmaint.exe"));
            }
        }
    }
    let root = std::env::current_dir().ok()?;
    candidates.push(root.join("bin").join("windmaint.exe"));
    candidates.push(root.join("windmaint.exe"));
    candidates.push(root.join("windcap").join("target").join("release").join("windmaint.exe"));
    candidates.push(root.join("windcap").join("target").join("debug").join("windmaint.exe"));
    candidates.into_iter().find(|path| path.is_file())
}

/// What the startup sweep did about the last instance's leftovers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sweep {
    /// Journals replayed: their rows are in the index and their directory is marked submitted.
    pub replayed: usize,
    pub rows_committed: usize,
    pub rows_collapsed: usize,
    /// Rows a replay dropped because the index already had them — the commit-then-die-before-delete
    /// interleaving, which is the only reason the replay is allowed to look at the database at all.
    pub rows_already_indexed: usize,
    /// Stranded segments below the 5-row floor: left exactly as found, journal and all, because the
    /// close path would not have committed them either.
    pub left_below_floor: usize,
    /// Journals whose writer is still running. Not stranded segments at all — a live recorder's
    /// directory is never touched.
    pub left_live: usize,
    /// Empty directories with no frames and no journal: the strays a kill between `create_dir_all`
    /// and the first JPEG leaves behind, and the ones the close path's comment always promised a
    /// sweep for.
    pub retired_empty_dirs: usize,
    /// Rows dropped because the kill cut their line in half. At most one per journal.
    pub torn_rows: usize,
    /// Why the whole sweep stood down, when it did.
    pub deferred: Option<String>,
    /// Everything else it refused to do, and why.
    pub notes: Vec<String>,
}

impl Sweep {
    fn note(&mut self, message: String) {
        self.notes.push(message);
    }
}

/// Collect what a dead instance left in the slice cache, before this one starts writing over it.
///
/// Three things make this safe to run against a directory the user may still be working in, and each
/// of them is a real refusal rather than a caution: the record lock must not name a live process
/// other than us (`fslock`'s own dead-pid verdict, the same one that lets `PidLock::acquire` reclaim
/// a corpse); the journal's header must name a pid that is not running; and the header must describe
/// the directory it was found in, so a copied or renamed slice is not replayed under an identity it
/// never had.
///
/// Replay reuses the close path's own methods — `collapse_repeats`, `is_committable`,
/// `submit_marker` — rather than restating them, so there is exactly one commit policy in this
/// binary and recovery cannot be the laxer one. Rows that are already in the index are dropped
/// before the INSERT, which is what makes a crash between committing and deleting the journal cost a
/// no-op instead of a second copy of the user's history.
///
/// `own_dir` is this instance's fresh segment directory: it exists for all of one tick before the
/// first frame is written, and it is never anything to recover.
pub fn sweep_stranded(config: &Config, plan: &Plan, own_dir: &str) -> Sweep {
    let mut sweep = Sweep::default();
    let cache_root = config.cache_screenshot_dir();
    match fslock::availability(&config.record_lock_path()) {
        Availability::HeldByLive(pid) => {
            sweep.deferred = Some(format!("a recorder is already running (pid {pid}): nothing was swept"));
            return sweep;
        }
        // An unreadable lock is what a foreign recorder writes. Standing down is the only reading
        // of it that cannot interrupt someone else's segment.
        Availability::Unclear => {
            sweep.deferred = Some(format!(
                "{} names no pid: nothing was swept",
                config.record_lock_path().display()
            ));
            return sweep;
        }
        Availability::Available => {}
    }
    let entries = match std::fs::read_dir(&cache_root) {
        Ok(entries) => entries,
        Err(_) => return sweep,
    };
    for entry in entries.flatten() {
        let slice = entry.path();
        if !slice.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        // Only a directory this binary could have filled and not yet closed: a name that parses as a
        // segment stamp and carries no pipeline marker. `-VIDEO`, `-DISCARD` and `-SCREENSHOTS-OCRED`
        // are decisions somebody already made, and a stale journal inside one is not ours to relitigate.
        if name == own_dir || paths::has_segment_marker(&name) || segment::Segment::stranded(&name).is_none() {
            continue;
        }
        if segment::journal_paths(&slice).is_empty() {
            retire_if_empty(&slice, &mut sweep);
            continue;
        }
        replay_slice(config, plan, &slice, &name, &mut sweep);
    }
    sweep
}

/// Replay one segment's journal into the index, or leave it exactly as it was.
fn replay_slice(config: &Config, plan: &Plan, slice: &Path, name: &str, sweep: &mut Sweep) {
    let claimed = match claim_stranded(slice, name) {
        Claim::Taken(claimed) => claimed,
        Claim::WriterAlive => {
            sweep.left_live += 1;
            return;
        }
        Claim::Refused(reason) => {
            sweep.note(format!("{name}: {reason}"));
            return;
        }
    };
    sweep.torn_rows += claimed.torn;
    let mut segment = match Segment::stranded(name) {
        Some(segment) => segment,
        None => {
            segment::release_journal(slice, &claimed.path);
            sweep.note(format!("{name}: its stamp does not parse, which the sweep should have ruled out"));
            return;
        }
    };
    segment.records = claimed.records;
    let dropped = segment.collapse_repeats(plan);
    if !segment.is_committable() {
        // The same floor the close path applies. The journal goes back under its canonical name so
        // that a later startup — with a different plan, or a segment this one cannot see the end of —
        // reaches the same verdict rather than inheriting this one's.
        segment::release_journal(slice, &claimed.path);
        sweep.left_below_floor += 1;
        sweep.note(format!(
            "{name}: {} rows is below the 5-row floor: left uncommitted",
            segment.records.len()
        ));
        return;
    }

    let replayed = replay_rows(config, plan, slice, &segment, sweep);
    match replayed {
        Ok(committed) => {
            let already = segment.records.len() - committed;
            sweep.replayed += 1;
            sweep.rows_committed += committed;
            sweep.rows_collapsed += dropped;
            sweep.rows_already_indexed += already;
            eprintln!(
                "recovered {name}: {committed} rows committed, {dropped} collapsed, {already} already indexed"
            );
        }
        Err(reason) => {
            // Nothing was committed and the marker was not written, so the journal is handed back and
            // the next startup tries again. A half-done replay is always safe to repeat; an abandoned
            // one is not.
            segment::release_journal(slice, &claimed.path);
            sweep.note(format!("{name}: {reason}"));
        }
    }
}

/// The committed part of one replay: dedup against the index, insert, mark the directory, drop the
/// journal. Returns how many rows were actually written.
fn replay_rows(
    config: &Config,
    plan: &Plan,
    slice: &Path,
    segment: &Segment,
    sweep: &mut Sweep,
) -> Result<usize, String> {
    // Rows go to the month the segment opened in, which is the month `close_segment` would have used
    // because the tick that crosses midnight closes the old segment before opening the new store.
    let parts = clock::LocalParts::from_stamp(&segment.dir_name).ok_or("segment stamp does not parse")?;
    let mut store = open_store(config, plan, parts.year, parts.month)?;
    let indexed = store.committed_pictures(&segment.videofile_name).map_err(|e| e.to_string())?;
    let pending: Vec<wind_store::Record> = segment
        .records
        .iter()
        .filter(|record| !indexed.contains(&record.picturefile_name))
        .cloned()
        .collect();
    let committed = store.append(&pending).map_err(|e| e.to_string())?;
    // The marker last: it is what tells `windmaint` the directory is finished with, and it must never
    // describe a segment whose rows are not in the index. With it written, the journal has nothing
    // left to protect.
    std::fs::create_dir_all(slice.join(segment.submit_marker())).map_err(|e| e.to_string())?;
    let left = segment::clear_journals(slice);
    if !left.is_empty() {
        sweep.note(format!("{}: journal still present: {}", slice.display(), left.join(", ")));
    }
    Ok(committed)
}

/// What a journal is worth to this instance.
enum Claim {
    /// Renamed under our pid, writer proven gone, rows in hand.
    Taken(Claimed),
    /// Its writer is running: this is a live segment, not a leftover.
    WriterAlive,
    /// Not ours to interpret — a header that contradicts its directory, a claim another live process
    /// holds, two journals where there can only be one.
    Refused(String),
}

struct Claimed {
    path: PathBuf,
    records: Vec<wind_store::Record>,
    torn: usize,
}

/// Read a stranded journal and take it, in that order, checking liveness on both sides of the claim.
///
/// The first read decides whether the writer is gone; the rename is the exclusive step; the second
/// read is the set of rows to commit, because a writer that came back to life between the two has
/// been appending to a file we now hold under a different name. A journal already carrying a claim
/// name is adoptable only when its claimant is gone too — one replay at a time, in both directions.
fn claim_stranded(slice: &Path, name: &str) -> Claim {
    let journals = segment::journal_paths(slice);
    let [only] = journals.as_slice() else {
        return Claim::Refused(format!("holds {} journal files", journals.len()));
    };
    let journal = match Journal::read(only) {
        Ok(journal) => journal,
        Err(reason) => return Claim::Refused(reason),
    };
    if let Some(reason) = why_not_replayable(&journal, only, name) {
        return claim_of(reason);
    }
    let claimed = match segment::claim_journal(slice, only, std::process::id()) {
        Ok(claimed) => claimed,
        Err(reason) => return Claim::Refused(reason),
    };
    let journal = match Journal::read(&claimed) {
        Ok(journal) => journal,
        Err(reason) => {
            segment::release_journal(slice, &claimed);
            return Claim::Refused(reason);
        }
    };
    if let Some(reason) = why_not_replayable(&journal, &claimed, name) {
        segment::release_journal(slice, &claimed);
        return claim_of(reason);
    }
    if journal.records.is_empty() {
        segment::release_journal(slice, &claimed);
        return Claim::Refused("journal holds no whole rows".to_string());
    }
    Claim::Taken(Claimed { path: claimed, records: journal.records, torn: journal.torn_tail })
}

/// Report a failed verdict to the caller that asked for it.
fn claim_of(reason: Verdict) -> Claim {
    match reason {
        Verdict::WriterAlive => Claim::WriterAlive,
        Verdict::Refused(why) => Claim::Refused(why),
    }
}

/// Why a journal may not be replayed into the directory it was found in.
enum Verdict {
    /// Its writer is running.
    WriterAlive,
    /// Not ours to interpret.
    Refused(String),
}

/// Why a journal may not be replayed into the directory it was found in; `None` means every check
/// holds and its rows are ours to commit.
///
/// Four things have to be true, and each one is a refusal rather than a warning, because the cost of
/// guessing wrong is a user's history indexed twice or indexed under a video that cannot exist.
fn why_not_replayable(journal: &Journal, file: &Path, name: &str) -> Option<Verdict> {
    let Some(owner) = &journal.owner else {
        // The header and the first row go out in one write, so a journal with no header is one with
        // nothing recoverable in it — a file created and then abandoned mid-frame.
        return Some(Verdict::Refused("journal has no readable header".to_string()));
    };
    // The one liveness test this app has, asked of the writer named in the header: a journal whose
    // owner is running is not a leftover, it is someone else's recording in progress.
    if fslock::is_process_running(owner.owner_pid) {
        return Some(Verdict::WriterAlive);
    }
    // A claim left by a process that is still running belongs to that process; one left by a process
    // that is gone is a replay that died halfway and is safe to take over. Our own claim is the one
    // this function is asked about after the rename, so it is not a conflict.
    let file_name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if let Some(claimant) = segment::journal_claimant(file_name) {
        if claimant != std::process::id() && fslock::is_process_running(claimant) {
            return Some(Verdict::Refused(format!("{file_name} is being replayed right now")));
        }
    }
    // The header has to describe the directory it was found in. A slice copied in from another
    // machine, or renamed by hand, names a pid that is gone for reasons that have nothing to do with
    // this install having lost a recorder, and its rows would then be indexed under a video file no
    // frame directory can ever become.
    let expected = Segment::stranded(name).map(|segment| segment.videofile_name);
    if owner.dir_name != name || Some(owner.videofile_name.as_str()) != expected.as_deref() {
        return Some(Verdict::Refused(format!(
            "header names {} / {}, not this directory",
            owner.dir_name, owner.videofile_name
        )));
    }
    None
}

/// Remove a segment directory that holds nothing at all.
///
/// Deliberately narrower than "empty": a directory with a frame, a journal, or a `-SUBMIT` marker is
/// evidence of something, and the retention pass — not a recorder that started five seconds ago — is
/// what retires a converted slice.
fn retire_if_empty(slice: &Path, sweep: &mut Sweep) {
    if has_frames(slice) || slice.join(segment::SUBMIT_MARKER).is_dir() {
        return;
    }
    match std::fs::remove_dir_all(slice) {
        Ok(()) => sweep.retired_empty_dirs += 1,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => sweep.note(format!("{}: could not retire an empty directory: {e}", slice.display())),
    }
}

/// Does this directory hold anything `windmaint` would read as a frame?
///
/// The rule has to match `maint`'s `read_frames`: an image file whose whole stem is a 19-character
/// stamp. The two disagreeing is how a sweep retires a directory that still has a video in it.
fn has_frames(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|entry| {
            let path = entry.path();
            path.is_file()
                && entry
                    .file_name()
                    .into_string()
                    .ok()
                    .map_or(false, |name| is_frame_name(&name))
        }),
        Err(_) => false,
    }
}

fn is_frame_name(name: &str) -> bool {
    let (stem, extension) = match name.rsplit_once('.') {
        Some(pair) => pair,
        None => return false,
    };
    matches!(extension.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png")
        && stem.len() == paths::STAMP_LEN
        && clock::LocalParts::from_stamp(stem).is_some()
}

/// Say what the sweep did. Read-only verdicts are silent; every action and every refusal is not.
fn report_sweep(sweep: &Sweep) {
    if let (None, 0, 0, 0, 0) = (&sweep.deferred, sweep.replayed, sweep.left_below_floor, sweep.left_live, sweep.retired_empty_dirs) {
        for note in &sweep.notes {
            eprintln!("startup sweep: {note}");
        }
        return;
    }
    if let Some(reason) = &sweep.deferred {
        eprintln!("startup sweep: {reason}");
    }
    if sweep.retired_empty_dirs > 0 {
        eprintln!("startup sweep: retired {} empty directories", sweep.retired_empty_dirs);
    }
    if sweep.left_below_floor > 0 {
        eprintln!(
            "startup sweep: left {} segment(s) below the 5-row floor uncommitted",
            sweep.left_below_floor
        );
    }
    if sweep.left_live > 0 {
        eprintln!("startup sweep: left {} journal(s) whose writer is still running", sweep.left_live);
    }
    if sweep.torn_rows > 0 {
        eprintln!("startup sweep: dropped {} torn journal row(s)", sweep.torn_rows);
    }
    for note in &sweep.notes {
        eprintln!("startup sweep: {note}");
    }
}

/// A frame path that cannot escape the slice directory.
///
/// `picturefile_name` ends up in the user's index, and the index is readable and writable by any
/// process the user runs; a frame name of `../../../../Windows/win.ini` must not become an
/// arbitrary write once a maintenance or rewrite pass resolves it back against the cache directory.
fn frame_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || name.contains(['/', '\\'])
        || name.contains(".//")
        || name.contains("..")
        || Path::new(name).is_absolute()
    {
        return Err(format!("refusing to write a frame outside the slice directory: {name:?}"));
    }
    Ok(dir.join(name))
}

fn open_store(config: &Config, plan: &Plan, year: i64, month: u32) -> Result<Store, String> {
    Store::open_month(&config.db_dir(), &plan.user_name, year, month).map_err(|e| e.to_string())
}

fn plan_from(config: &Config, mode: Mode) -> Plan {
    Plan {
        record_seconds: config.i64_or("record_seconds", 900),
        interval_seconds: config.i64_or("screenshot_interval_second", 3).max(1),
        interrupt_limit: config.i64_or("screenshot_interrupt_recording_count", 40).max(1) as u32,
        pause_after_idle_minutes: config.i64_or("screentime_not_change_to_pause_record", 5) as f64,
        repeat_text_similarity: config.f64_or("ocr_compare_similarity", 0.7) * 100.0,
        in_table_similarity: config.f64_or("ocr_compare_similarity_in_table", 0.94) * 100.0,
        reduce_duplicates: config.bool_or("index_reduce_same_content_at_different_time", true),
        record_deep_linking: config.bool_or("record_deep_linking", true),
        sleep_drift_limit_seconds: 30.0,
        maintain_after_idle_minutes: config.idle_maintain_gap_minutes(),
        display_strategy: config.str_or("multi_display_record_strategy", "all"),
        single_display_index: config.i64_or("record_single_display_index", 1) as i32,
        minimum_text_chars: 5,
        exclude_words: config
            .str_list("exclude_words")
            .into_iter()
            .map(|w| w.to_lowercase())
            .collect(),
        user_name: config.user_name(),
        maintain_window: config.maintain_window(),
        // A window was named, so the previews have somewhere to be redrawn: the frame this row points
        // at is on disk, and `previews` reads from it.
        defer_previews: config.maintain_window().is_some(),
        // The engine follows the same rule for the same reason: a window named is a promise that a
        // pass will come back for the rows written without text.
        defer_text: config.maintain_window().is_some(),
        // Only `GateOnly` suppresses the row and the frame. A run that simply has no text to put in
        // the row still writes both — see [`Mode::TitlesOnly`].
        gate_only: matches!(mode, Mode::GateOnly),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::JournalOwner;
    use std::time::Duration;
    use wind_store::read;

    /// A pid no live process can hold — the same stand-in `fslock`'s own tests use for "the owner is
    /// gone", which is the whole basis on which a journal may be replayed.
    const DEAD: u32 = 4_000_000;

    fn scratch_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("windcap-sweep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A date and time on the 27th of September 2026, which is all the window judgement looks at.
    fn at(day: u32, hour: u32, minute: u32) -> clock::LocalParts {
        clock::LocalParts { year: 2026, month: 9, day, hour, minute, second: 0 }
    }

    const EARLY: MaintainWindow = MaintainWindow { start_minutes: 3 * 60 + 30, end_minutes: 5 * 60 };
    const OVERNIGHT: MaintainWindow = MaintainWindow { start_minutes: 22 * 60, end_minutes: 6 * 60 };

    /// The whole point of naming two clock times: the pass runs then, and not at any other minute.
    #[test]
    fn a_scheduled_pass_is_due_inside_its_window_and_nowhere_else() {
        let last = None;
        assert!(scheduled_pass_is_due(EARLY, last, &at(27, 3, 30)), "it opens at the minute it says");
        assert!(scheduled_pass_is_due(EARLY, last, &at(27, 4, 59)));
        assert!(!scheduled_pass_is_due(EARLY, last, &at(27, 5, 0)), "its own end minute is outside it");
        assert!(!scheduled_pass_is_due(EARLY, last, &at(27, 3, 29)), "and so is the minute before it opens");
        assert!(!scheduled_pass_is_due(EARLY, last, &at(27, 15, 0)), "afternoon is not a window");
    }

    /// One window, one pass. The marker says when the last one started, and the same window cannot
    /// earn a second pass — including at 04:59, four minutes before it closes.
    #[test]
    fn one_window_is_spent_by_one_pass() {
        let opened = at(27, 3, 31).naive_epoch_seconds();
        assert!(!scheduled_pass_is_due(EARLY, Some(opened), &at(27, 4, 0)), "this window already ran");
        // The next night, the same clock time is a fresh appointment.
        let yesterday = at(26, 4, 0).naive_epoch_seconds();
        assert!(scheduled_pass_is_due(EARLY, Some(yesterday), &at(27, 4, 0)), "yesterday's run does not own today");
    }

    /// The case a calendar-date test would miss: a `22:00`-`06:00` window is one night, and 01:00 is
    /// inside the night that opened on the 27th, not a new day's window.
    #[test]
    fn an_overnight_window_is_one_appointment_not_two() {
        let started_early_evening = at(27, 23, 5).naive_epoch_seconds();
        assert!(
            !scheduled_pass_is_due(OVERNIGHT, Some(started_early_evening), &at(28, 1, 0)),
            "01:00 belongs to the 27th's night, which already ran"
        );
        assert!(
            scheduled_pass_is_due(OVERNIGHT, Some(started_early_evening), &at(28, 22, 30)),
            "and the 28th's own evening is a fresh window"
        );
        assert!(scheduled_pass_is_due(OVERNIGHT, None, &at(28, 5, 59)), "the leg after midnight is inside");
        assert!(!scheduled_pass_is_due(OVERNIGHT, None, &at(28, 6, 0)), "and it shuts at 06:00");
        assert!(!scheduled_pass_is_due(OVERNIGHT, None, &at(28, 12, 0)), "noon is outside an overnight window");
    }

    /// A window a user named reaches the plan, so the tick that checks it and the settings page that
    /// wrote it are reading the same one truth.
    #[test]
    fn the_named_window_reaches_the_plan_that_asks_it() {
        let root = scratch_root("window-plan");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(
            root.join("config_src/config_default.json"),
            r#"{"maintain_window_start": "03:30", "maintain_window_end": "05:00"}"#,
        )
        .unwrap();

        let config = Config::load(&root).unwrap();
        let plan = plan_from(&config, Mode::TitlesOnly);
        assert_eq!(plan.maintain_window.map(|w| (w.start_minutes, w.end_minutes)), Some((210, 300)));
        // The window is also what defers the preview encode: with somewhere to redraw it, the row can
        // be written without one; without a window, nothing would ever fill it in.
        assert!(plan.defer_previews, "a named window defers the previews to itself");
        // An install that named nothing keeps the idle rule, and the plan says so by holding nothing.
        let bare = scratch_root("window-bare");
        std::fs::create_dir_all(bare.join("config_src")).unwrap();
        std::fs::write(bare.join("config_src/config_default.json"), "{}").unwrap();
        let bare_plan = plan_from(&Config::load(&bare).unwrap(), Mode::TitlesOnly);
        assert_eq!(bare_plan.maintain_window, None);
        assert!(!bare_plan.defer_previews, "no window, so the preview is still made where it always was");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }

    /// Re-parsing two settings files every tick is what this gate removes. Identical stamps mean there
    /// is nothing to learn from disk; either file moving — by time, or by size within the same clock
    /// tick — means the plan has to be rebuilt, including the privacy mask below `reload_plan`.
    #[test]
    fn the_plan_is_reloaded_only_when_a_settings_file_actually_moved() {
        use std::time::{Duration, SystemTime};
        let early = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let later = early + Duration::from_secs(1);
        let stamp = |t: SystemTime, len: u64| ((Some(t), len), (Some(t), len));

        assert!(!settings_changed(Some(stamp(early, 10)), stamp(early, 10)), "nothing moved, nothing to read");
        assert!(settings_changed(None, stamp(early, 10)), "never read is always read once");
        assert!(settings_changed(Some(stamp(early, 10)), stamp(later, 10)), "a rewrite moves the time");
        assert!(settings_changed(Some(stamp(early, 10)), stamp(early, 11)), "a save inside one clock tick moves the size");
    }

    /// A window that opens and closes at the same minute is empty, so nothing is ever scheduled.
    #[test]
    fn an_empty_window_schedules_nothing() {
        let none = MaintainWindow { start_minutes: 300, end_minutes: 300 };
        assert!(!scheduled_pass_is_due(none, None, &at(27, 5, 0)));
        assert!(!scheduled_pass_is_due(none, None, &at(27, 4, 59)));
    }

    /// A config for a scratch install. No config files at all: every path the sweep needs is derived
    /// from the defaults, which is also the point — recovery cannot depend on a key the user set.
    fn plan_for(root: &Path) -> (Config, Plan) {
        let config = Config::load(root).expect("an empty root still loads, with defaults");
        let plan = plan_from(&config, Mode::Full);
        (config, plan)
    }

    /// One frame as the recorder would have journaled it, with the JPEG beside it.
    struct Stranded {
        dir: PathBuf,
        rows: Vec<wind_store::Record>,
    }

    /// A segment directory exactly as a `taskkill /F` leaves it: frames on disk, rows in a journal
    /// named for a writer that is gone, no `-SUBMIT` marker, nothing in the index.
    fn stranded_slice(root: &Path, stamp: &str, texts: &[&str], owner_pid: u32) -> Stranded {
        let (_, plan) = plan_for(root);
        let opened = clock::LocalParts::from_stamp(stamp).expect("a scratch stamp parses");
        let dir = root.join("cache_screenshot").join(stamp);
        let owner = JournalOwner {
            owner_pid,
            dir_name: stamp.to_string(),
            videofile_name: format!("{stamp}.mp4"),
            opened_at: opened.naive_epoch_seconds(),
        };
        let rows: Vec<wind_store::Record> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| wind_store::Record {
                videofile_name: format!("{stamp}.mp4"),
                picturefile_name: format!("{}.jpg", clock::LocalParts::from_naive_epoch(
                    opened.naive_epoch_seconds() + index as i64 * plan.interval_seconds
                ).stamp()),
                videofile_time: opened.naive_epoch_seconds() + index as i64 * plan.interval_seconds,
                ocr_text: (*text).to_string(),
                win_title: Some(format!("Window {index} - Scratch")),
                deep_linking: None,
                thumbnail: Some("AAAA".to_string()),
            })
            .collect();
        for row in &rows {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(&row.picturefile_name), b"fake jpeg").unwrap();
            Journal::append(&dir, &owner, row).unwrap();
        }
        Stranded { dir, rows }
    }

    fn rows_indexed(root: &Path, month: (i64, u32)) -> Vec<read::Row> {
        let months = read::discover(&root.join("userdata").join("db"));
        let wanted: Vec<&read::Month> = months
            .iter()
            .filter(|m| (m.year, m.month) == month)
            .collect();
        assert!(wanted.len() <= 1, "at most one month file for {month:?}");
        // No file at all is the honest answer for "nothing was ever committed into this month", and
        // several tests below are asserting exactly that.
        if wanted.is_empty() {
            return Vec::new();
        }
        let conn = wanted[0].open_read(Duration::from_secs(300), false).unwrap();
        let mut rows = read::rows_in_window(&conn, None, None).unwrap();
        rows.sort_by_key(|row| row.time);
        rows
    }

    fn marker(dir: &Path) -> bool {
        dir.join(crate::segment::SUBMIT_MARKER).is_dir()
    }

    fn journals(dir: &Path) -> Vec<String> {
        crate::segment::journal_paths(dir)
            .into_iter()
            .filter_map(|path| Some(path.file_name()?.to_str()?.to_string()))
            .collect()
    }

    const SIX: [&str; 6] = [
        "quarterly revenue report",
        "shopping cart with socks",
        "video call with the team",
        "database query running slow",
        "pdf manual page twelve",
        "flight booking summary",
    ];

    /// The defect, stated as a test: a killed instance leaves frames nothing will ever index. The
    /// sweep is what makes the next instance index them, at full fidelity, and hand the directory to
    /// the ordinary maintenance pipeline.
    #[test]
    fn a_hard_kills_stranded_segment_is_indexed_by_the_next_startup() {
        let root = scratch_root("replay");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, DEAD);
        assert!(journals(&slice.dir).iter().any(|n| n == crate::segment::JOURNAL_FILE));
        assert!(!marker(&slice.dir));

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.rows_committed, sweep.left_below_floor, sweep.left_live), (1, 6, 0, 0), "{sweep:?}");

        let rows = rows_indexed(&root, (2026, 9));
        assert_eq!(rows.len(), 6);
        // The two columns a JPEG cannot give back. If these came out empty the journal had been
        // decorative, and the recovery would be the lossy re-OCR it was designed to replace.
        assert_eq!(rows[0].title(), Some("Window 0 - Scratch"));
        assert_eq!(rows[5].title(), Some("Window 5 - Scratch"));
        assert_eq!(rows[2].body(), "video call with the team");
        assert_eq!(rows[0].videofile_name, "2026-09-21_10-00-00.mp4");
        assert!(marker(&slice.dir), "and the directory now says what the maintenance pass looks for");
        assert!(journals(&slice.dir).is_empty(), "a settled segment leaves no journal behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The crash window that makes replay dangerous: `store.append` has committed, the process dies
    /// before the journal is deleted, and a naive replay would index the user's history twice. The
    /// journal is deleted *after* the commit and the replay dedups against it, so this interleaving
    /// costs a no-op. `rows_already_indexed` is the count of what would otherwise have been duplicated.
    #[test]
    fn a_replay_that_follows_its_own_commit_does_not_commit_twice() {
        let root = scratch_root("double");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, DEAD);

        // The interleaving itself: every row is already in the index, and the journal survived anyway.
        let mut store = open_store(&config, &plan, 2026, 9).unwrap();
        assert_eq!(store.append(&slice.rows).unwrap(), 6);
        drop(store);
        assert!(!marker(&slice.dir), "the kill landed before the marker, or after it: both are this case");

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.rows_committed, sweep.rows_already_indexed), (1, 0, 6), "{sweep:?}");
        assert_eq!(rows_indexed(&root, (2026, 9)).len(), 6, "six rows, not twelve");

        // And a third startup has nothing left to do at all.
        let again = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((again.replayed, again.rows_committed), (0, 0), "{again:?}");
        assert!(journals(&slice.dir).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Half-committed is the same case with the rows split across the window: the dedup is per row,
    /// so the missing tail is what gets written and nothing else.
    #[test]
    fn a_replay_fills_only_the_rows_that_are_missing_from_a_partial_commit() {
        let root = scratch_root("partial");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, DEAD);
        let mut store = open_store(&config, &plan, 2026, 9).unwrap();
        store.append(&slice.rows[..2]).unwrap();
        drop(store);

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.rows_committed, sweep.rows_already_indexed), (4, 2), "{sweep:?}");
        assert_eq!(rows_indexed(&root, (2026, 9)).len(), 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Four stranded rows are as unconvertible as four rows are at close, and the sweep must say so
    /// rather than quietly invent a laxer rule for recovery. The journal stays: this is a decision
    /// about a threshold, not a dead end.
    #[test]
    fn a_stranded_segment_below_the_floor_is_left_uncommitted_just_as_close_would() {
        let root = scratch_root("floor");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX[..4], DEAD);

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.left_below_floor), (0, 1), "{sweep:?}");
        assert!(rows_indexed(&root, (2026, 9)).is_empty(), "not one row reached the index");
        assert!(!marker(&slice.dir), "unmarked, because nothing was committed to convert");
        assert_eq!(journals(&slice.dir), vec![crate::segment::JOURNAL_FILE.to_string()], "and still recoverable next time");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Five rows that only reach four once the repeats are folded away fail the floor on the way in,
    /// which is the close path's ordering: collapse, then floor, then commit.
    #[test]
    fn collapsing_can_take_a_stranded_segment_under_the_floor_too() {
        let root = scratch_root("collapse-floor");
        let (config, plan) = plan_for(&root);
        let texts = [
            "quarterly revenue report",
            "quarterly revenue report",
            "shopping cart with socks",
            "video call with the team",
            "database query running slow",
        ];
        stranded_slice(&root, "2026-09-21_10-00-00", &texts, DEAD);

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.left_below_floor, sweep.rows_committed), (0, 1, 0), "{sweep:?}");
        assert_eq!(sweep.rows_collapsed, 0, "a segment left alone reports no collapse");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The dedup and the collapse are segment-wide, so a stranded screen held for twenty minutes
    /// becomes one row on replay exactly as it would have at close — the property that makes
    /// per-frame commits the wrong fix.
    #[test]
    fn repeats_inside_a_stranded_segment_collapse_before_a_replay_commits_them() {
        let root = scratch_root("collapse");
        let (config, plan) = plan_for(&root);
        let mut texts = Vec::new();
        for _ in 0..10 {
            texts.push("quarterly revenue report 2026");
        }
        texts.push("shopping cart with socks and shoes");
        texts.push("video call with the whole team today");
        texts.push("database query running rather slowly");
        texts.push("pdf manual page twelve of twenty");
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &texts, DEAD);

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.rows_committed, sweep.rows_collapsed), (1, 5, 9), "{sweep:?}");
        let rows = rows_indexed(&root, (2026, 9));
        assert_eq!(rows.len(), 5, "one searchable row for the ten-minute screen, not ten");
        assert!(journals(&slice.dir).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A journal whose writer is running is somebody's recording in progress. Replaying it would
    /// commit rows the live instance is about to commit again at its own close, and the user would
    /// get two copies of the same screen for it.
    #[test]
    fn a_journal_being_written_right_now_is_never_touched() {
        let root = scratch_root("live-writer");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, std::process::id());

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.left_live, sweep.rows_committed), (0, 1, 0), "{sweep:?}");
        assert_eq!(journals(&slice.dir), vec![crate::segment::JOURNAL_FILE.to_string()], "still the writer's own file");
        assert!(!marker(&slice.dir));
        assert!(rows_indexed(&root, (2026, 9)).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The lock, not a second heartbeat: while a live recorder holds the record lock the sweep does
    /// not read the cache at all, so a `windrec run` beside a running `windrec loop` cannot reach
    /// into the loop's in-flight segment.
    #[test]
    fn the_sweep_stands_down_for_a_recorder_that_holds_the_lock() {
        let root = scratch_root("locked");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, DEAD);
        let live = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ping is on every Windows install");
        std::fs::create_dir_all(config.record_lock_path().parent().unwrap()).unwrap();
        std::fs::write(config.record_lock_path(), live.id().to_string()).unwrap();

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!(sweep.deferred.as_deref(), Some(format!("a recorder is already running (pid {}): nothing was swept", live.id()).as_str()));
        assert_eq!((sweep.replayed, sweep.retired_empty_dirs), (0, 0));
        assert_eq!(journals(&slice.dir), vec![crate::segment::JOURNAL_FILE.to_string()]);

        // The same sweep, with the owner gone: the corpse of a lock is no obstacle, which is the
        // behaviour `PidLock::acquire` already applies to it.
        let corpse = std::fs::read_to_string(config.record_lock_path()).unwrap().trim().parse::<u32>().unwrap();
        let _ = live;
        std::fs::write(config.record_lock_path(), DEAD.to_string()).unwrap();
        let after = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert!(after.deferred.is_none(), "dead pid {corpse} does not defer anything");
        assert_eq!(after.replayed, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A journal whose header contradicts the directory it was found in arrived here by copy or
    /// rename, not by a kill, and its rows belong to a video this directory cannot become.
    #[test]
    fn a_journal_belonging_to_another_segment_is_not_replayed_into_this_one() {
        let root = scratch_root("wrong-dir");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, DEAD);
        let original = crate::segment::journal_paths(&slice.dir)[0].clone();
        let body = std::fs::read_to_string(&original).unwrap();
        let moved = body.replace("2026-09-21_10-00-00", "2026-09-21_11-00-00");
        std::fs::write(&original, moved).unwrap();

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.rows_committed), (0, 0), "{sweep:?}");
        assert_eq!(sweep.notes.len(), 1, "{:?}", sweep.notes);
        assert!(sweep.notes[0].contains("not this directory"), "{:?}", sweep.notes);
        assert_eq!(journals(&slice.dir), vec![crate::segment::JOURNAL_FILE.to_string()], "refused, not consumed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A replay that died after its rename leaves a journal under the dead claimant's pid. The next
    /// startup adopts it — the writer's pid is what decides, not the file's name.
    #[test]
    fn a_claim_left_behind_by_a_dead_replay_is_adopted() {
        let root = scratch_root("adopt");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, DEAD);
        let canonical = crate::segment::journal_paths(&slice.dir)[0].clone();
        let stale = slice.dir.join(crate::segment::claimed_journal_name(5_000_000));
        std::fs::rename(&canonical, &stale).unwrap();

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.rows_committed), (1, 6), "{sweep:?}");
        assert!(!stale.exists() && rows_indexed(&root, (2026, 9)).len() == 6);
        assert!(marker(&slice.dir));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A journal torn down to nothing but a partial header is not a claim on anyone's attention, and
    /// it is certainly not a licence to mark the directory converted.
    #[test]
    fn a_journal_with_nothing_in_it_is_reported_and_left() {
        let root = scratch_root("empty-journal");
        let (config, plan) = plan_for(&root);
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, DEAD);
        let path = crate::segment::journal_paths(&slice.dir)[0].clone();
        std::fs::write(&path, "h1\t40000").unwrap();

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.rows_committed, sweep.torn_rows), (0, 0, 0), "{sweep:?}");
        assert!(sweep.notes.iter().any(|n| n.contains("no readable header")), "{:?}", sweep.notes);
        assert!(!marker(&slice.dir));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rows belong to the month their segment opened in — which is the month the close path used,
    /// because the tick that crosses midnight commits the old segment before opening the new store.
    /// Routing a replay by today's date would move last month's history into this month's file.
    #[test]
    fn a_stranded_segment_lands_in_the_month_file_it_would_have_closed_into() {
        let root = scratch_root("month");
        let (config, plan) = plan_for(&root);
        stranded_slice(&root, "2026-08-15_10-00-00", &SIX, DEAD);
        let db = root.join("userdata").join("db");

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!(sweep.replayed, 1);
        assert!(db.join("default_2026-08_wind.db").exists(), "august's file, not september's");
        assert!(!db.join("default_2026-09_wind.db").exists());
        assert_eq!(rows_indexed(&root, (2026, 8)).len(), 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of the leftover: a directory created and then abandoned before its first JPEG.
    /// Nothing in it is evidence, and the close path's comment has promised a sweep for it since
    /// there was no sweep at all.
    #[test]
    fn a_directory_holding_no_frames_and_no_journal_is_retired_and_everything_else_is_not() {
        let root = scratch_root("retire");
        let (config, plan) = plan_for(&root);
        let cache = root.join("cache_screenshot");
        std::fs::create_dir_all(&cache).unwrap();
        let empty = cache.join("2026-09-20_09-00-00");
        std::fs::create_dir_all(&empty).unwrap();
        let with_frame = stranded_slice(&root, "2026-09-20_08-00-00", &SIX[..1], DEAD);
        let stranded = stranded_slice(&root, "2026-09-20_07-00-00", &SIX, DEAD);
        let converted = cache.join("2026-09-19_07-00-00-VIDEO");
        std::fs::create_dir_all(&converted).unwrap();
        let committed_empty = cache.join("2026-09-18_07-00-00");
        std::fs::create_dir_all(committed_empty.join(crate::segment::SUBMIT_MARKER)).unwrap();
        let not_ours = cache.join("thumbs");
        std::fs::create_dir_all(&not_ours).unwrap();

        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!(sweep.retired_empty_dirs, 1, "{sweep:?}");
        assert!(!empty.exists(), "the stray is gone");
        assert!(with_frame.dir.exists(), "one frame is a frame");
        assert!(stranded.dir.exists() && marker(&stranded.dir), "and a stranded segment is recovered, not retired");
        assert!(converted.exists(), "a marked directory is somebody else's decision");
        assert!(committed_empty.exists(), "an empty directory that was submitted is retention's work, not ours");
        assert!(not_ours.exists(), "a name that is not a segment stamp is not ours either");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Frame recognition has to agree with the rule `windmaint` reads slices by, or the sweep
    /// deletes a directory that still holds a convertible video.
    #[test]
    fn only_a_file_whose_stem_is_a_full_stamp_counts_as_a_frame() {
        let dir = scratch_root("frames");
        std::fs::write(dir.join("2026-09-21_10-00-00.jpg"), b"j").unwrap();
        assert!(has_frames(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        for junk in [
            "windmaint_concat.txt",
            crate::segment::JOURNAL_FILE,
            "2026-09-21_10-00-00_cropped.jpg",
            "2026-09-21_10-00.jpg",
            "notes.txt",
            "2026-13-45_99-99-99.jpg",
        ] {
            let dir = scratch_root("not-frames");
            std::fs::write(dir.join(junk), b"x").unwrap();
            assert!(!has_frames(&dir), "{junk} is not a frame");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_journal_row_carries_what_a_jpeg_cannot_and_nothing_else_is_invented() {
        // The point of the journal as a unit test: `win_title` never reaches the frame file, so a
        // row recovered without the journal would be permanently degraded. The five fields that do
        // survive on disk are asserted here as the ones the codec must not lose either.
        let root = scratch_root("fidelity");
        let slice = stranded_slice(&root, "2026-09-21_10-00-00", &SIX, DEAD);
        let path = crate::segment::journal_paths(&slice.dir)[0].clone();
        let journal = Journal::read(&path).unwrap();
        assert_eq!(journal.records, slice.rows);
        for (index, row) in journal.records.iter().enumerate() {
            assert_eq!(row.win_title.as_deref(), Some(format!("Window {index} - Scratch").as_str()));
            assert!(std::fs::metadata(slice.dir.join(&row.picturefile_name)).is_ok(), "its frame is on disk");
            assert_eq!(row.videofile_time, slice.rows[index].videofile_time, "the instant is the filename's, exactly");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_frame_name_cannot_walk_out_of_the_slice_directory() {
        let dir = Path::new("cache_screenshot/2026-09-21_10-00-00");
        assert!(frame_path(dir, "2026-09-21_10-00-05.jpg").is_ok());
        for bad in [
            "",
            "../evil.jpg",
            "..\\evil.jpg",
            "a/b.jpg",
            "a\\b.jpg",
            "/absolute.jpg",
            "C:/Windows/win.ini",
            "sub/../x.jpg",
        ] {
            assert!(frame_path(dir, bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn maintenance_runs_once_per_idle_gap_and_never_when_switched_off() {
        assert!(maintenance_is_due(None, 1_000_000, 40));
        assert!(!maintenance_is_due(Some(1_000_000), 1_000_000 + 39 * 60, 40));
        assert!(maintenance_is_due(Some(1_000_000), 1_000_000 + 40 * 60, 40));
        // A clock that goes backwards across a sleep must not schedule a maintenance every tick.
        assert!(!maintenance_is_due(Some(1_000_000), 900_000, 40));
        assert!(!maintenance_is_due(None, 1_000_000, 0), "gap 0 means the user switched it off");
    }

    /// The whole point of promoting `idle_maintain_time_gap` to a declared setting: the number the
    /// Recording page writes into the file is the number this process counts minutes with.
    ///
    /// Asserted through `plan_from`, because that is the one place the file becomes a `Plan`, and
    /// through `maintenance_is_due` with the plan's own field, because a row that set a gap the recorder
    /// rounded, clamped or ignored would pass a test that only compared two literals.
    #[test]
    fn the_idle_gap_the_page_shows_is_the_gap_the_recorder_uses() {
        let root = scratch_root("idle-gap");
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(root.join("config_src/config_default.json"), r#"{"idle_maintain_time_gap": 15}"#).unwrap();
        let config = Config::load(&root).unwrap();
        let plan = plan_from(&config, Mode::Full);
        assert_eq!(plan.maintain_after_idle_minutes, 15, "the file's number reaches the plan unaltered");
        assert_eq!(plan.maintain_after_idle_minutes, config.idle_maintain_gap_minutes(), "and the plan is asked of the accessor the page writes through");

        // The consequence, not the plumbing: fourteen minutes of idle is not a pass, fifteen is.
        assert!(!maintenance_is_due(Some(0), 14 * 60, plan.maintain_after_idle_minutes));
        assert!(maintenance_is_due(Some(0), 15 * 60, plan.maintain_after_idle_minutes));

        // Zero is still the off position after the accessor took over the reading.
        std::fs::write(root.join("config_src/config_default.json"), r#"{"idle_maintain_time_gap": 0}"#).unwrap();
        let off = plan_from(&Config::load(&root).unwrap(), Mode::Full);
        assert_eq!(off.maintain_after_idle_minutes, 0);
        assert!(!maintenance_is_due(None, 10_000_000, off.maintain_after_idle_minutes), "an idle recorder that never spawns anything");

        // And an install whose file predates the key keeps the gap this binary used before the row
        // existed — 40 minutes, the shipped default, not zero and not the caller's fallback.
        let _ = std::fs::remove_dir_all(root.join("config_src"));
        let shipped = plan_from(&Config::load(&root).unwrap(), Mode::Full);
        assert_eq!(
            shipped.maintain_after_idle_minutes,
            Config::load(&workspace_root()).unwrap().idle_maintain_gap_minutes(),
            "the recorder's fallback and the shipped file must not be two different waits"
        );
        assert_eq!(shipped.maintain_after_idle_minutes, 40);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The repository root as a `windrec` test sees it: two levels up from `windcap/windrec`.
    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(Path::to_path_buf).unwrap()
    }

    #[test]
    fn stop_requests_are_visible_without_racing() {
        STOP_REQUESTED.store(false, Ordering::Relaxed);
        assert!(!stop_requested());
        request_stop();
        assert!(stop_requested());
        STOP_REQUESTED.store(false, Ordering::Relaxed);
    }

    /// The plan is the compatibility surface: these keys are the ones a user can set in the GUI, and
    /// a renamed key silently reverts the recorder to its defaults.
    #[test]
    fn the_plan_reads_the_shipped_config_keys() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap();
        let config = Config::load(root).expect("install config");
        let plan = plan_from(&config, Mode::Full);
        assert_eq!(plan.record_seconds, 900);
        assert_eq!(plan.interval_seconds, 3);
        assert_eq!(plan.interrupt_limit, 40);
        assert!((plan.repeat_text_similarity - 70.0).abs() < 1e-6);
        assert!((plan.in_table_similarity - 94.0).abs() < 1e-6);
        assert!(plan.reduce_duplicates);
        assert_eq!(plan.user_name, "default");
        assert!(plan.exclude_words.contains(&"windrecorder".to_string()));
        assert!(!plan.record_only_gate());
        // `--no-ocr` is *not* gate-only any more: it drops the text and keeps the row. The two are
        // separate modes and the old conflation is what let `--gate-only` exist unwired.
        assert!(!plan_from(&config, Mode::TitlesOnly).record_only_gate());
        assert!(plan_from(&config, Mode::GateOnly).record_only_gate());
    }

    /// The three modes have to be distinguishable from the plan alone, because the plan is the only
    /// thing `tick()` consults before it decides whether to write a frame at all.
    #[test]
    fn each_mode_is_a_different_promise_about_the_row() {
        assert!(Mode::Full.reads_text());
        assert!(!Mode::TitlesOnly.reads_text());
        assert!(!Mode::GateOnly.reads_text());
        let root = std::env::temp_dir();
        let config = Config::load(&root).expect("defaults load anywhere");
        for (mode, gate_only) in [
            (Mode::Full, false),
            (Mode::TitlesOnly, false),
            (Mode::GateOnly, true),
        ] {
            assert_eq!(
                plan_from(&config, mode).record_only_gate(),
                gate_only,
                "{mode:?} writes the wrong thing"
            );
        }
    }

    /// The write-ahead journal has to carry an OCR-less row exactly as it carries any other, or the
    /// recovery path replays a different set of rows than the live run wrote: `close_segment` would
    /// commit six title-only rows and the sweep would commit none of them, because an empty
    /// `ocr_text` is the one field the new path leaves behind.
    #[test]
    fn a_frame_kept_without_ocr_text_is_journalled_and_replayed_like_any_other() {
        let root = scratch_root("ocr-less");
        let (config, plan) = plan_for(&root);
        let stamp = "2026-09-21_10-00-00";
        let opened = clock::LocalParts::from_stamp(stamp).unwrap();
        let base = opened.naive_epoch_seconds();
        let mut segment = Segment::opening(&opened);
        // The writer has to be a process that is gone, or the sweep correctly refuses to touch it.
        let owner = JournalOwner {
            owner_pid: DEAD,
            dir_name: segment.dir_name.clone(),
            videofile_name: segment.videofile_name.clone(),
            opened_at: segment.opened_at,
        };
        let dir = segment.directory(&config.cache_screenshot_dir());
        // Six window changes, no text anywhere: what an afternoon with a quarantined engine is.
        let titles = ["qxkzj", "wpvtm", "syrbn", "gchae", "uolif", "d6789"];
        for (index, title) in titles.iter().enumerate() {
            let at = base + index as i64 * plan.interval_seconds * 10;
            let candidate = wind_store::Record {
                videofile_name: segment.videofile_name.clone(),
                picturefile_name: format!("{}.jpg", clock::LocalParts::from_naive_epoch(at).stamp()),
                videofile_time: at,
                ocr_text: String::new(),
                win_title: Some((*title).to_string()),
                deep_linking: None,
                thumbnail: Some("AAAA".to_string()),
            };
            segment.offer(at, &candidate, &plan, OcrOutcome::Unavailable).expect(
                "a titled row survives an unread screen",
            );
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(&candidate.picturefile_name), b"fake jpeg").unwrap();
            Journal::append(&dir, &owner, &candidate).unwrap();
        }
        assert_eq!(segment.records.len(), 6, "one row per window, none of them with text");

        // What the live close path would have committed...
        let mut live = segment.clone();
        assert_eq!(live.collapse_repeats(&plan), 0, "six different windows are six screens");
        assert!(live.is_committable());
        let expected: Vec<String> = live
            .records
            .iter()
            .map(|r| format!("{}|{}", r.picturefile_name, r.win_title.as_deref().unwrap_or("-")))
            .collect();

        // ...is exactly what the startup sweep replays from that journal.
        let sweep = sweep_stranded(&config, &plan, "2026-09-23_00-00-00");
        assert_eq!((sweep.replayed, sweep.rows_committed, sweep.left_below_floor), (1, 6, 0), "{sweep:?}");
        let rows = rows_indexed(&root, (2026, 9));
        assert_eq!(rows.len(), 6);
        let actual: Vec<String> = rows
            .iter()
            .map(|r| format!("{}|{}", r.picturefile_name, r.title().unwrap_or("-")))
            .collect();
        assert_eq!(actual, expected, "the replay committed the live run's rows, in order");
        for row in &rows {
            assert_eq!(row.body(), "", "no text was invented for a screen that was never read");
            assert!(row.title().is_some(), "the title is all the row has, and it survived");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------------------------
    // The privacy mask on the live path
    // -----------------------------------------------------------------------------------------

    /// The defect this closes: `ocr_image_crop_URBL` was honoured when re-indexing a video and ignored
    /// when grabbing a screen, so the same exclusion protected old footage and not new.
    ///
    /// Both halves here are the real call sites — `tick`'s [`mask_for_frame`] and `index_one`'s
    /// `MaskPlan::for_frame` — fed one frame and one config. A rectangle is then written out, not
    /// merely compared to itself, so a change to the shared geometry has to be chosen twice.
    #[test]
    fn the_live_path_masks_the_rectangle_the_reindexer_masks() {
        let panels = vec![
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            Tile { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = VirtualDesktop { x: 0, y: 0, width: 3840, height: 1080 };
        let urbl = vec![6i64, 6, 6, 3];

        // An all-displays grab at the desktop's own resolution: what the recorder sees on a machine
        // that is not scaling, and byte-for-byte what the reindexer sees in an i-frame off the video
        // the maintenance pass later stitches from these frames.
        let live = mask_for_frame(3840, 1080, desktop, &panels, Tile::from(desktop), &urbl);
        let reindexed = MaskPlan::for_frame(3840, 1080, &panels, Tile::from(desktop), &urbl);
        assert_eq!(live.bands(), reindexed.bands(), "one privacy boundary, two callers");

        let expected = vec![
            windcap::crop::Band { x: 0, y: 0, width: 1920, height: 64 },
            windcap::crop::Band { x: 0, y: 1016, width: 1920, height: 64 },
            windcap::crop::Band { x: 0, y: 0, width: 57, height: 1080 },
            windcap::crop::Band { x: 1805, y: 0, width: 115, height: 1080 },
            windcap::crop::Band { x: 1920, y: 0, width: 1920, height: 64 },
            windcap::crop::Band { x: 1920, y: 1016, width: 1920, height: 64 },
            windcap::crop::Band { x: 1920, y: 0, width: 57, height: 1080 },
            windcap::crop::Band { x: 3725, y: 0, width: 115, height: 1080 },
        ];
        assert_eq!(live.bands(), expected, "top 6, right 6, bottom 6, left 3, per panel");

        // And the mask is real pixels, not a claim: the excluded top row goes black in the copy while
        // the caller's buffer — the one that becomes the JPEG on disk — keeps its values.
        let (w, h) = (3840usize, 1080usize);
        let frame = vec![200u8; w * h * 3];
        let (masked, painted) = windcap::crop::masked_copy(&frame, w, h, &live);
        assert_eq!(painted, expected.len());
        assert_eq!(masked.len(), frame.len(), "masking never resizes a frame");
        assert_eq!(&masked[0..3], &[0, 0, 0], "the excluded edge is black in the OCR input");
        // The middle of panel 1: below its top band, above its bottom one, inside its left and right
        // edges — the one place on this frame that must still be readable.
        assert_eq!(&masked[(500 * w + 900) * 3..(500 * w + 900) * 3 + 3], &[200, 200, 200]);
        assert!(frame.iter().all(|&p| p == 200), "the recorded frame is untouched");
    }

    /// `windrec` stretches its source into a 1920-wide image, so the panels have to be scaled with it —
    /// and the result must still be the same *proportions* the reindexer would apply to the same
    /// desktop at full size.
    #[test]
    fn a_resampled_grab_masks_the_same_edges_at_the_frames_own_scale() {
        let panels = vec![Tile { x: 0, y: 0, width: 3840, height: 2160 }, Tile { x: 3840, y: 0, width: 2080, height: 720 }];
        let desktop = VirtualDesktop { x: 0, y: 0, width: 5920, height: 2160 };
        let live = mask_for_frame(1480, 540, desktop, &panels, Tile::from(desktop), &[10, 10, 10, 10]);
        assert_eq!(live.tiles.len(), 2, "both panels are still named, at the image's scale");
        assert!(live.bands().iter().all(|b| (b.x + b.width) <= 1480 && (b.y + b.height) <= 540), "{:?}", live.bands());
        // The seam between the panels is at 3840/5920 of the width — 960 of 1480.
        assert!(live.bands().iter().any(|b| b.x == 960), "panel 2's mask starts at the seam: {:?}", live.bands());
    }

    /// The deployed default is foreground-window capture, which is neither a panel nor the union, and
    /// upstream's own help text promises a proportional mask of the window. Falling through to "no
    /// tiles" here would be the leak all over again.
    #[test]
    fn a_foreground_window_is_masked_proportionally_rather_than_left_alone() {
        let panels = vec![Tile { x: 0, y: 0, width: 1920, height: 1080 }];
        let desktop = VirtualDesktop { x: 0, y: 0, width: 1920, height: 1080 };
        let window = VirtualDesktop { x: 300, y: 120, width: 800, height: 600 };
        let live = mask_for_frame(800, 600, window, &panels, Tile::from(desktop), &[5, 5, 5, 5]);
        assert_eq!(live.bands().len(), 4, "the window gets the band the user set");
        assert_eq!(live.bands()[0].height, 30, "5% of the window's 600 rows");
    }

    #[test]
    fn a_recorder_that_masked_nothing_says_so_instead_of_being_quiet_about_it() {
        let stats = Stats { kept: 40, masked: 37, ..Stats::default() };
        let line = masked_line(stats, Mode::Full).expect("a run that read text has a line");
        assert!(line.contains("37 of 40"), "{line}");

        let unmasked = Stats { kept: 40, masked: 0, ..Stats::default() };
        let warning = masked_line(unmasked, Mode::Full).expect("and so does one that masked nothing");
        assert!(warning.starts_with("warning:"), "{warning}");
        assert!(warning.contains("ocr_image_crop_URBL"), "{warning}");

        // The two modes that never hand a frame to the engine have no boundary to report on, and a
        // "0 frames masked" line there would read like a failure of a switch that is off by design.
        assert_eq!(masked_line(Stats { kept: 40, ..Stats::default() }, Mode::TitlesOnly), None);
        assert_eq!(masked_line(Stats { gated: 40, ..Stats::default() }, Mode::GateOnly), None);
        assert_eq!(masked_line(Stats::default(), Mode::Full), None, "a run that kept nothing says nothing");
    }

    #[test]
    fn a_crop_that_hides_nothing_is_called_out_before_the_first_frame() {
        let one = [Tile { x: 0, y: 0, width: 1920, height: 1080 }];
        let three: Vec<Tile> = (0..3).map(|i| Tile { x: i * 1920, y: 0, width: 1920, height: 1080 }).collect();

        assert!(crop_excludes_nothing(&[0, 0, 0, 0], &one), "an explicit zero is a control that is off");
        assert!(crop_excludes_nothing(&[0; 12], &three));
        assert!(!crop_excludes_nothing(&[6, 6, 6, 3], &one), "the shipped default hides something");
        // One zeroed slot on a three-panel machine still leaves panels 2 and 3 on the shipped default,
        // so this is not the case the startup warning is for — and saying it were would train the user
        // to ignore the warning.
        assert!(!crop_excludes_nothing(&[0, 0, 0, 0], &three), "short lists are padded, not zeroed");
        assert!(!crop_excludes_nothing(&[], &one), "an absent key takes the default band");
    }

    #[test]
    fn the_doctor_line_names_the_setting_the_order_and_the_pixels() {
        let set = scratch_root("doctor-crop");
        let text = describe_crop(&config_holding(&set, &[12, 3, 9, 6]));
        assert!(text.contains("ocr_image_crop_URBL = [12, 3, 9, 6]"), "{text}");
        assert!(text.contains("top, right, bottom, left"), "{text}");
        assert!(text.contains("top 12%"), "{text}");
        assert!(text.contains("left 6%"), "{text}");
        assert!(text.contains("keep every pixel"), "{text}");
        assert!(text.contains("painted on the OCR input only"), "{text}");

        // Every slot on this machine zeroed — one four-tuple per attached panel, whatever number that
        // happens to be — because a report that warns about a mask the user did configure is as useless
        // as one that stays silent about a mask they removed.
        let panels = windcap::capture::monitors().len().max(1);
        let zeroed = scratch_root("doctor-crop-zero");
        let all_zero = describe_crop(&config_holding(&zeroed, &vec![0; panels * 4]));
        assert!(all_zero.contains("WARNING"), "{all_zero}");
        assert!(all_zero.contains("indexed like everything else"), "{all_zero}");
        let _ = std::fs::remove_dir_all(&set);
        let _ = std::fs::remove_dir_all(&zeroed);
    }

    /// A scratch install whose user layer sets one key: `describe_crop` reads a `Config`, and a test
    /// that could not hand it a value could not check what it says about one.
    fn config_holding(root: &Path, urbl: &[i64]) -> Config {
        std::fs::create_dir_all(root.join("userdata")).unwrap();
        let listed = urbl.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
        std::fs::write(
            root.join("userdata/config_user.json"),
            format!("{{\"ocr_image_crop_URBL\": [{listed}]}}"),
        )
        .unwrap();
        Config::load(root).expect("a one-key user layer over the embedded defaults")
    }
}
