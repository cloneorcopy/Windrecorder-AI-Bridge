//! The application shell: what the threads are, and how a [`Command`] becomes one.
//!
//! `update` is the only place this crate is allowed to do anything besides draw, and even here it
//! does no reading — it drains replies, paints, and hands work to `workers`. Three properties are
//! worth naming because every other line in the file exists to keep them true:
//!
//!   * nothing between one `update` and the next touches a disk, so a frame is bounded by how much
//!     there is to draw and never by how much there is to find;
//!   * the only synchronous I/O left in the app is the config write behind the Save button, which is
//!     one temp-file-plus-rename of a twenty-kilobyte JSON document and which the user asked for by
//!     name;
//!   * a reply that arrives for a request the user has since replaced is dropped in `model`, not
//!     here, so the rule is testable without a window.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wind_base::{clock, config::Config};

use crate::ai::{self, AiSettings};
use crate::backend::{self, Env};
use crate::model::{self, AppEvent, AppState, Command, Player};
use crate::record::{Rec, RecDraft};
use crate::settings::Settings;
use crate::textures::Cache;
use crate::wordcloud;
use crate::{flags, play, thumbs, view, workers};

/// Search, day loads and thumbnail decodes are different shapes of work: the first two are one
/// long query each, the third is hundreds of short ones. Four threads lets a page of thumbnails
/// drain while a month's `COUNT(*)` runs, without the fan-out a per-job thread would give.
const WORKER_THREADS: usize = 4;

/// How often a hidden window looks for the tray's "come back" request. Matches `winduiweb`'s watcher:
/// short enough that a double-click feels answered, long enough that an idle hidden window costs nothing.
const BACKGROUND_POLL: Duration = Duration::from_millis(300);

/// Events folded in per frame. A burst of thumbnail replies must not be allowed to make one frame
/// arbitrarily long; whatever is left waits for the repaint this function asks for anyway.
const DRAIN_LIMIT: usize = 256;

/// How many distinct words a cloud is allowed to show.
///
/// Upstream's `max_words` for the month mask is 300. It is 300 because the picture is 1000x800 pixels
/// of its own; the box here is a few hundred points of a side, and a spiral that fails to place its
/// two-hundredth word spends its steps doing nothing. Ranked first, so the cap is a truncation of the
/// most-frequent list and never a random sample of it.
const CLOUD_WORDS: usize = 120;

pub struct App {
    pub root: PathBuf,
    pub state: AppState,
    pub config: Config,
    env: Env,
    /// The month's stop-word set, read once. Held here rather than in `Env` because it is only ever
    /// needed by the one worker job that builds a cloud, and cloning it into every job would copy
    /// nine hundred strings to answer a question three of four threads were never asked.
    stop_words: wordcloud::StopWords,
    textures: Cache,
    /// The full-frame overlay's own three-entry cache.
    ///
    /// Separate from `textures` because the two hold pictures an order of magnitude apart in size: a
    /// preview at the shipped width is ~0.6 MB of decoded RGBA and a 1080p frame ~8 MB, so a frame must
    /// never be pushed out by the same eviction rules that keep a scrolled page of thumbnails alive — and
    /// a 2000-entry cap for full frames would be 16 GB. Three entries: the frame being looked at, the one
    /// being decoded, and the second the player is drawing (`model::player_key`), which needs its own
    /// slot rather than the row's because a paused viewer shows both at once. See `textures::FRAME_CAP`.
    frames: Cache,
    workers: workers::Workers,
    events: Receiver<AppEvent>,
    opened: Instant,
    /// This window is invisible because its close button hid it, rather than because it never showed.
    /// Only this process knows which, and only this process can undo it — so it has to remember, and keep
    /// waking itself to look for the tray's request.
    window_hidden: bool,
    /// `Some` only in a debug build: `main`'s `--exit-after` arm is compiled out of a release one,
    /// so the value that reaches `check_exit` there is always the `None` that returns early.
    exit_after: Option<Duration>,
    /// Emitted once, from the first frame that draws, so `--exit-after` can prove the window opened
    /// and the footer filled in without anyone watching a screen.
    /// Which tab the last frame showed, so the prompt panel reloads on entering the AI page rather
    /// than on every frame.
    last_tab: crate::model::Tab,
    banner_sent: bool,
}

impl App {
    /// Opening the app must survive a root that has no config and no database — the same rule the
    /// recorder's `doctor` follows. A missing directory is an answer, not a crash.
    pub fn new(root: PathBuf, exit_after: Option<Duration>, ctx: &egui::Context) -> Result<App, String> {
        let env = Env::load(&root)?;
        let config = env.config.clone();
        let settings = env.settings.clone();
        let month_count = env.months.len();

        let today = clock::now();
        let mut state = AppState::new(settings, today);
        // The recorder's own section, then, from the file it is about to be written back to — and the
        // draft re-rendered from it, because the widgets show what the user's config says and not what
        // a fresh install would default to.
        state.rec = Rec::load(&config);
        state.rec_options = backend::rec_options(&root);
        // The Settings page's pickers, from the same install: which engines this machine can drive and
        // which locales the catalog really translates. Before the first frame, so the page opens with its
        // lists full rather than half-populated.
        state.settings_options = crate::settings::Options::scan(&root, &config);
        state.rec_draft = RecDraft::from(&state.rec);
        // The AI page's applied values, and the draft re-rendered from them. The key box opens empty
        // by construction (`AiDraft::from` never seeds it), so loading the file here does not put a
        // token anywhere the frame can reach.
        state.ai = ai::AiSettings::load(&config);
        state.ai_draft = ai::AiDraft::from(&state.ai);
        state.months = env.months.clone();
        state.flag_path = config.flag_note_path();
        state.notice = Some(format!("root {}", root.display()));
        // Upstream's own default for the lightbox watermark decides how the band opens, so the two UIs
        // show the same month the same way on first sight. After that it is a view option.
        state.stat.watermark = config.bool_or("enable_month_lightbox_watermark", true);

        let (workers, events) = workers::Workers::new(WORKER_THREADS, ctx);
        let mut app = App {
            root,
            state,
            config,
            env,
            stop_words: wordcloud::StopWords::default(),
            textures: Cache::new(),
            frames: Cache::with_cap(crate::textures::FRAME_CAP),
            workers,
            events,
            opened: Instant::now(),
            window_hidden: false,
            exit_after,
            last_tab: crate::model::Tab::Search,
            banner_sent: false,
        };
        app.stop_words = backend::stop_words(&app.root, &app.config);
        // Install the catalog of the root the window actually opened, in the user's own `lang`, so the
        // tray and the window translate themselves from the same `languages.json` — the whole reason
        // the catalog moved into `wind_base`. Before this the state carries the shipped `en` fallback,
        // which is what the headless render tests read.
        app.state.install_catalog(&app.root, &app.config.str_or("lang", "en"));
        // The library scan and the first day are independent, so both go out before the first frame
        // is drawn: whatever lands first is what the user sees first.
        if month_count == 0 {
            // Nothing to read: say so now rather than after a scan that will find nothing.
            app.state.apply(AppEvent::Library(model::LibraryStats::default()));
        }
        app.dispatch(Command::ScanLibrary);
        let day = app.state.day.date;
        let (id, date) = app.state.set_day(day);
        app.dispatch(Command::LoadDay { request_id: id, date });
        // Once, here and after a save — not per frame: half of this answer is a TCP connect, and the
        // other half is the file the bridge reads when the tray starts it, which no keystroke changes.
        app.state.bridge = ai::bridge_status(&app.root);
        Ok(app)
    }

    /// One frame. This is `eframe::App::update` minus the `Frame` argument, which is what lets the
    /// render tests call the real thing.
    pub fn frame(&mut self, ctx: &egui::Context) {
        self.drain(ctx);
        self.state.today = clock::now();
        self.refresh_ai_status();

        // Entering the AI page reloads the prompt panel from disk. Re-read rather than cached: the
        // files are the user's, `windai prompts` and a text editor write them too, and a panel showing
        // words that a save an hour ago replaced is the "a copy of the text" failure this section is
        // built to avoid.
        if self.state.tab == crate::model::Tab::Ai && self.last_tab != crate::model::Tab::Ai {
            self.state.reload_prompts(&self.config);
        }
        self.last_tab = self.state.tab;

        let mut commands = Vec::new();
        let started = Instant::now();
        view::paint(&mut self.state, &mut self.textures, &mut self.frames, ctx, &mut commands);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.state.paint_ms_last = ms;
        self.state.paint_ms_max = self.state.paint_ms_max.max(ms);

        if !self.banner_sent {
            self.banner_sent = true;
            eprintln!("windui: window up, root {}, {}", self.root.display(), self.state.footer.line());
        }

        // Prefetch after painting: only now does the frame know which cards it actually showed, and
        // the queue has to be bounded by the screen rather than by the page size.
        for job in self.state.thumbnail_jobs() {
            if !self.textures.contains(&job.key) {
                self.dispatch(Command::DecodeThumbnail(Box::new(job)));
            }
        }
        self.autoload(&mut commands);
        for command in commands {
            self.dispatch(command);
        }
        self.check_exit(ctx);
    }

    /// The two transitions that need no click: opening OneDay for the first time, opening Stat for the
    /// first time, and a day that failed to load being retried once the library is known.
    fn autoload(&mut self, out: &mut Vec<Command>) {
        if self.state.tab == crate::model::Tab::OneDay && !self.state.day.loaded && !self.state.day.pending {
            let day = self.state.day.date;
            let (id, date) = self.state.set_day(day);
            out.push(Command::LoadDay { request_id: id, date });
        }
        // Stat's two scatters are the screen, so they are fetched on arrival rather than behind a
        // button. The lightbox and the cloud are not: one reads a month's pictures, the other its whole
        // recognised text, and neither belongs on the boot path of a window the user may have opened
        // only to check one number. They stay behind their buttons, upstream's shape for them.
        if self.state.tab == crate::model::Tab::Stat {
            if !self.state.stat.month_track.loaded && !self.state.stat.month_track.pending {
                let (id, year, month) = self.state.load_month();
                out.push(Command::LoadMonth { request_id: id, year, month });
            }
            if !self.state.stat.year_track.loaded && !self.state.stat.year_track.pending {
                let (id, year) = self.state.load_year();
                out.push(Command::LoadYear { request_id: id, year });
            }
        }
    }

    fn drain(&mut self, ctx: &egui::Context) {
        let mut count = 0usize;
        while let Ok(event) = self.events.try_recv() {
            // Every thumbnail in the batch, not just the first: the decodes run on four threads, so
            // a page of them lands together, and one upload per frame would fill a twenty-card
            // screen in twenty frames while re-dispatching the nineteen it dropped.
            if let AppEvent::Thumbnail { key, image: Some(image) } = &event {
                self.textures.insert(ctx, key, image.clone());
            }
            if let AppEvent::Frame { key, image: Some(image), .. } = &event {
                self.frames.insert(ctx, key, image.clone());
            }
            // The player's frame is uploaded behind the same test `apply` is about to run, and the test
            // has to be made twice because a cache cannot read an answer it was not given: the moving
            // picture lives in one shared slot (`model::player_key`), so a superseded stream's JPEG left
            // in it *is* what the next paint shows, pointer rule or no pointer rule. Every upload above
            // can go in blind, because its key already names the row the picture belongs to.
            if let AppEvent::PlayerFrame { image, run, .. } = &event {
                if self.state.player_run.as_ref().is_some_and(|live| Arc::ptr_eq(live, run)) {
                    self.frames.insert(ctx, &model::player_key(), image.clone());
                }
            }
            count += 1;
            // A scan re-lists the database directory, so its reply is also the only way `env` learns
            // about a month file the recorder created after the window opened. Without this the
            // footer would count files that the next query still refuses to open.
            let rescanned = matches!(&event, AppEvent::Library(_));
            let changed = self.state.apply(event);
            if rescanned {
                self.env.months = self.state.months.clone();
            }
            if changed {
                ctx.request_repaint();
            }
            if count >= DRAIN_LIMIT {
                // Still more to fold in: ask for the frame that will take the rest.
                ctx.request_repaint();
                break;
            }
        }
    }

    pub(crate) fn dispatch(&mut self, command: Command) {
        match command {
            Command::Search { request_id, params } => {
                let env = self.env.clone();
                self.workers.submit(move |sink| {
                    let outcome = backend::run_search(&env, &params);
                    sink.reply(AppEvent::Search { request_id, outcome });
                });
            }
            Command::LoadDay { request_id, date } => {
                let env = self.env.clone();
                self.workers.submit(move |sink| {
                    let outcome = backend::load_day(&env, date);
                    sink.reply(AppEvent::Day { request_id, date, outcome });
                });
            }
            Command::ScanLibrary => {
                let env = self.env.clone();
                self.workers.submit(move |sink| {
                    backend::scan(&env, |stats| sink.reply(AppEvent::Library(stats)));
                });
            }
            Command::DecodeThumbnail(job) => {
                self.workers.submit(move |sink| {
                    let image = thumbs::decode(&job.base64);
                    sink.reply(AppEvent::Thumbnail { key: job.key, image });
                });
            }
            Command::ShowFrame(card) => {
                // The overlay opens on the click, not on the reply. Reading a JPEG is milliseconds and
                // seeking a video is up to a second; a window that shows nothing while it works reads as a
                // control that does nothing.
                if self.state.open_frame(&card) {
                    let env = self.env.clone();
                    let key = card.key.clone();
                    self.workers.submit(move |sink| {
                        let (source, image) = match backend::frame(&env, &card) {
                            Some(frame) => (Some(frame.source), thumbs::decode_jpeg(&frame.bytes)),
                            None => (None, None),
                        };
                        sink.reply(AppEvent::Frame { key, source, image });
                    });
                }
            }
            Command::RefreshSegments => {
                self.env.segments.clear();
                self.env.pictures.clear();
            }
            Command::PlaySegment { key, segment, from, run } => {
                // One stream at a time, and the arm that starts one is the arm that ends the previous.
                // Not the painter's job: "at most one ffmpeg behind this window" is a property of the
                // dispatch, and a rule a widget has to remember is one the next widget forgets. The child
                // itself is reaped by `play::stream`, which wakes often enough to notice the flag within a
                // tenth of a second and kills the process before it returns.
                if let Some(previous) = &self.state.player_run {
                    previous.store(true, Ordering::Relaxed);
                }
                let name = segment.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| segment.display().to_string());
                self.state.player = Some(Player { key: key.clone(), name, at: from, duration: None, waiting: true, failure: None });
                self.state.player_run = Some(run.clone());
                // Resolved here, from the config the window already holds, and handed in: the worker that
                // reads the footage has no business re-reading the user's settings for itself, and the
                // still door and the moving one must not be able to answer with two different binaries.
                let ffmpeg = self.ffmpeg();
                self.workers.submit(move |sink| {
                    let source = play::Source::new(ffmpeg, segment, from);
                    // The length first, in its own short process: without it the transport row has no ends
                    // to put a scrub bar on, and a bar over an unknown range is a control that lies.
                    let duration = match play::probe(&source) {
                        Ok(seconds) => Some(seconds),
                        Err(complaint) => {
                            // The sentence is the answer, and it is the whole answer. A viewer that went
                            // quiet here would leave the user choosing between "it is still loading" and
                            // "this machine cannot read the file", and only one of those is fixable.
                            sink.reply(AppEvent::PlayerDone { run, key, duration: None, failure: Some(complaint) });
                            return;
                        }
                    };
                    let mut unreadable = None;
                    let outcome = play::stream(&source, &run, |second, jpeg| {
                        // One decoder path for the whole window — `thumbs::decode_jpeg` is what the still
                        // comes through — so "the player's bytes and the still's bytes are not the same
                        // kind of thing" is not a state this app can be in.
                        match thumbs::decode_jpeg(&jpeg) {
                            Some(image) => sink.reply(AppEvent::PlayerFrame { run: run.clone(), key: key.clone(), at: second, image }),
                            None => {
                                unreadable = Some(format!("second {second} of {} came out of ffmpeg as bytes no decoder here can read", source.segment.display()))
                            }
                        }
                    });
                    let failure = unreadable.or(outcome.failure).or_else(|| {
                        // Ran out with nothing shown and nothing complained about is the ending this window
                        // refuses to paint as a box, and there are exactly two ways to reach it: the seek
                        // is past the last second the file holds, or this machine has no decoder for what
                        // the file contains. They look the same from inside a paused viewer and need
                        // different answers — one is nothing, the other is a codec — so they are said as
                        // two sentences. The words are composed here rather than read from the catalog
                        // because a worker has no `AppState` to translate with, which is already true of
                        // every `ffmpeg` complaint that reaches the screen through `play`.
                        (outcome.frames == 0).then(|| match duration {
                            Some(seconds) if from >= seconds => {
                                format!("the segment ends at second {seconds}, before the {from} this row asked to play from")
                            }
                            _ => format!(
                                "the segment gave no picture from second {from} — this machine has no decoder for what is \
                                 inside it, and Locate hands the file to a player that may have one"
                            ),
                        })
                    });
                    sink.reply(AppEvent::PlayerDone { run, key, duration, failure });
                });
            }
            Command::StopSegment { run } => {
                // Synchronous, in the same shape as `Locate`: raising a flag is not I/O, and it must not
                // queue behind the pool the stream is using — a stop dispatched as a job could sit in the
                // queue waiting for a free worker while the worker it needs is the one still streaming.
                run.store(true, Ordering::Relaxed);
                // Both halves of the transport are this arm's to retire, and only when this *is* the live
                // run: `player` is what makes the viewer paint the moving picture rather than the still, so
                // a stop that left it standing would freeze the box on its last second with a button on it
                // that had already been pressed. And a `Stop` drained after the user sought away carries a
                // flag this arm retired itself — clearing the live player for it would blank the transport
                // row of the stream that is actually playing.
                if self.state.player_run.as_ref().is_some_and(|live| Arc::ptr_eq(live, &run)) {
                    self.state.player = None;
                    self.state.player_run = None;
                }
            }
            Command::Locate { path } => {
                if let Err(error) = backend::locate(&path) {
                    self.state.notice = Some(error);
                }
            }
            Command::Flag { time, path } => {
                self.state.notice = flags::add_at(&path, time, "").err();
                if self.state.notice.is_none() {
                    self.reload_day();
                }
            }
            Command::EditFlag { path, when, note, index, new_note } => {
                // The write is `wind-notes`' guarded whole-table path; this only decides what to say
                // and to refetch. A saved edit needs no message — the reloaded row shows the new
                // text — but a row that moved or a file that changed under us does, because the user
                // must know the click did not land on the row they meant.
                match flags::edit_note(&path, &when, &note, index, &new_note) {
                    Ok(flags::FlagEdit::Saved) | Ok(flags::FlagEdit::Unchanged) => {}
                    Ok(flags::FlagEdit::Gone) => self.state.notice = Some("that flag is gone; the list was reloaded".to_string()),
                    Ok(flags::FlagEdit::Conflict) => {
                        self.state.notice = Some("the flag file changed while saving; reloaded — try again".to_string())
                    }
                    Err(e) => self.state.notice = Some(e),
                }
                self.reload_day();
            }
            Command::RemoveFlag { path, when, note, index } => {
                match flags::remove(&path, &when, &note, index) {
                    Ok(flags::FlagEdit::Saved) => self.state.notice = Some("flag deleted".to_string()),
                    Ok(flags::FlagEdit::Unchanged) => {}
                    Ok(flags::FlagEdit::Gone) => self.state.notice = Some("that flag was already gone; the list was reloaded".to_string()),
                    Ok(flags::FlagEdit::Conflict) => {
                        self.state.notice = Some("the flag file changed while deleting; reloaded — try again".to_string())
                    }
                    Err(e) => self.state.notice = Some(e),
                }
                self.reload_day();
            }
            Command::SaveSettings(settings) => self.save(&*settings),
            Command::SaveRecording(rec) => self.save_recording(&rec),
            Command::SaveAi(settings) => self.save_ai(&settings),
            Command::SavePrompt { name, text } => {
                // Validated by the writer itself — the same validator `windai prompts` and the bridge
                // meet — so no door on this page can save a prompt that would send no screen text.
                match ai::save_prompt(&self.config, name, &text) {
                    Ok(path) => {
                        self.state.ai_prompts.status = format!("saved {path}");
                        self.state.reload_prompts(&self.config);
                    }
                    Err(why) => self.state.ai_prompts.status = why,
                }
            }
            Command::RestorePrompt { name } => match ai::restore_prompt(&self.config, name) {
                Ok(true) => {
                    self.state.ai_prompts.status = format!("{} is back to the shipped words", name.label());
                    self.state.reload_prompts(&self.config);
                }
                Ok(false) => self.state.ai_prompts.status =
                    format!("{} was never overridden, so nothing changed", name.label()),
                Err(why) => self.state.ai_prompts.status = why,
            },
            Command::TestPrompt { request_id, name, text, settings } => {
                let config = self.config.clone();
                self.workers.submit(move |sink| {
                    let trial = ai::try_prompt(&config, &settings, name, &text);
                    sink.reply(AppEvent::PromptTried { request_id, trial });
                });
            }
            Command::TestAi { request_id, settings } => {
                // A worker, for the reason every other network-shaped thing in this app is one: a
                // hosted inference call is seconds, and `http.rs` gives the receive phase a hundred
                // and eighty of them on purpose. On the frame thread that is a frozen window, which a
                // user reads as a crashed one.
                //
                // The config is cloned in rather than re-read on the worker, so the probe runs
                // against the same merged map the Save button would write and cannot drift from it.
                let config = self.config.clone();
                self.workers.submit(move |sink| {
                    let report = ai::probe(&config, &settings);
                    sink.reply(AppEvent::AiTested {
                        request_id,
                        outcome: if report.ok { Ok(report.message) } else { Err(report.message) },
                    });
                });
            }
            Command::LoadMonth { request_id, year, month } => {
                let env = self.env.clone();
                self.workers.submit(move |sink| {
                    let outcome = backend::month_totals(&env, year, month);
                    sink.reply(AppEvent::StatMonth { request_id, outcome });
                });
            }
            Command::LoadYear { request_id, year } => {
                let env = self.env.clone();
                self.workers.submit(move |sink| {
                    let outcome = backend::year_totals(&env, year);
                    sink.reply(AppEvent::StatYear { request_id, outcome });
                });
            }
            Command::BuildLightbox { request_id, year, month } => {
                let env = self.env.clone();
                self.workers.submit(move |sink| {
                    let outcome = backend::lightbox(&env, year, month);
                    sink.reply(AppEvent::Lightbox { request_id, outcome });
                });
            }
            Command::BuildCloud { request_id, year, month } => {
                let env = self.env.clone();
                // The stop-word set is a nine-hundred-entry list the same query needs every time; it
                // is cloned in rather than re-read, because reading it on the worker would be a second
                // answer to a question the app already settled at startup.
                let stop = self.stop_words.clone();
                self.workers.submit(move |sink| {
                    let outcome = backend::word_cloud(&env, year, month, &stop, CLOUD_WORDS);
                    sink.reply(AppEvent::Cloud { request_id, outcome });
                });
            }
            Command::ProbeDisplays => {
                self.workers.submit(move |sink| {
                    let found = backend::displays();
                    // An empty enumeration is a real answer that must not look like "nothing is
                    // attached": on a session without a desktop `EnumDisplayMonitors` simply reports no
                    // monitors, and the panel has to be able to tell that apart from a probe that has
                    // not run yet.
                    sink.reply(AppEvent::Displays(if found.is_empty() {
                        Err("the desktop reported no displays".to_string())
                    } else {
                        Ok(found)
                    }));
                });
            }
        }
    }

    /// Refetch the day currently on screen, so a flag written, edited or deleted here is shown as the
    /// file holds it and the panel's per-row drafts — keyed to positions that a delete shifts — are
    /// dropped. One place, because three commands (Flag, EditFlag, RemoveFlag) all owe the user a list
    /// that matches the disk rather than the click that changed it.
    fn reload_day(&mut self) {
        let day = self.state.day.date;
        let (id, date) = self.state.set_day(day);
        self.dispatch(Command::LoadDay { request_id: id, date });
    }

    /// The ffmpeg this install runs, from the config the window already holds.
    ///
    /// Asked of `env.config` — the same accessor `backend::frame_from_video` goes through — rather than
    /// resolved once into a field on `App`, because a Save replaces that config and the still door and the
    /// moving one must not be able to answer from two different files. Called once per play request, which
    /// is once per click, so "resolve it once" means once per thing that needs it and not once per frame.
    fn ffmpeg(&self) -> PathBuf {
        self.env.config.ffmpeg_path()
    }

    /// Does this window still hold the picture a row was opened at?
    ///
    /// A method rather than an open field, because the frame cache belongs to the app and a test that
    /// reached past it would be a second owner of the thing whose cap the player had to negotiate. It is
    /// what lets the end-to-end player test say the still survived the moving picture — the whole job of
    /// the third cache slot — without taking the cache away from the app to prove it.
    // Test-facing, and therefore `#[cfg(test)]`: the app owns this cache outright, so nothing on the
    // frame path needs to ask it a question — which is also why leaving it open would be a warning
    // about a method the binary never calls.
    #[cfg(test)]
    pub(crate) fn holds_frame(&self, key: &model::RowKey) -> bool {
        self.frames.contains(key)
    }

    /// Stage the typed settings and write them. Synchronous on purpose: the status line that
    /// reports the outcome belongs to the same click that caused it, and the write is one rename.
    pub(crate) fn save(&mut self, settings: &Settings) {
        settings.stage(&mut self.config);
        let result = self.config.save().map_err(|e| e.to_string());
        let boot_note: Option<String> = if result.is_ok() {
            // The env is what the workers read, so a changed page size or day boundary has to reach
            // it before the next query — otherwise the settings screen would lie about taking
            // effect.
            self.env.config = self.config.clone();
            self.env.months = self.state.months.clone();
            self.env.settings = settings.clone();
            self.env.similar = backend::similar_table(&self.root, settings.use_similar_ch_char_to_search);
            self.env.segments.clear();
            // Both pickers ask the machine, and the machine's answer can have changed since boot — a
            // Tesseract installed while the window was open should appear without a restart.
            self.state.settings_options = crate::settings::Options::scan(&self.root, &self.config);
            // `lang` is the one setting that changes what this window *says* rather than what it shows, so
            // it is the one that has to re-read the catalog: the labels change in the frame after the save,
            // not at the next launch.
            if self.state.catalog.lang() != self.state.settings.lang {
                let lang = self.state.settings.lang.clone();
                self.state.install_catalog(&self.root, &lang);
            }
            // The one setting in this form that does not live in the config file. It is applied after the
            // write succeeded — a registry entry for a setting that was never saved would be a promise the
            // file does not keep — and whatever it says is put in front of the user, because a checkbox
            // that quietly disagrees with `HKCU\...\Run` is the dead-control bug this product has already
            // fixed once.
            match wind_base::autostart::apply(&self.root, settings.start_app_on_boot) {
                wind_base::autostart::Outcome::Unchanged => None,
                wind_base::autostart::Outcome::Changed(sentence) | wind_base::autostart::Outcome::Failed(sentence) => Some(sentence),
            }
        } else {
            None
        };
        self.state.apply(AppEvent::SettingsSaved(result));
        // After the apply, which rewrites `notes` from the re-validated draft: the registry's sentence is
        // the one thing in this frame the draft cannot re-derive.
        if let Some(sentence) = boot_note {
            self.state.notes.push(sentence);
        }
        // `day_begin_minutes` decides which rows belong to the day on screen, and
        // `oneday_timeline_pic_num` decides the strip: both need a refetch to take effect.
        if self.state.day.loaded {
            let day = self.state.day.date;
            let (id, date) = self.state.set_day(day);
            self.dispatch(Command::LoadDay { request_id: id, date });
        }
    }

    /// Stage the recorder's keys and write them. Same one-rename write as `save`, and the same reason
    /// it is synchronous: the status line belongs to the click that caused it.
    ///
    /// Deliberately *not* a reload of the whole `Rec`: `Config::save` writes the merged map, so the
    /// fifteen keys the Settings tab owns and the hundred nobody edits ride along as they already are.
    /// The live `Env` is refreshed too, because `windmaint` and `windrec` are separate processes that
    /// read this file at their own start, and the next search this window runs should see the same
    /// config the recorder will.
    pub(crate) fn save_recording(&mut self, rec: &Rec) {
        rec.stage(&mut self.config);
        let result = self.config.save().map_err(|e| e.to_string());
        if result.is_ok() {
            self.env.config = self.config.clone();
        }
        self.state.apply(AppEvent::RecordingSaved(result));
    }

    /// Recompute the AI page's status lines when the form has changed under them.
    ///
    /// Gated on the draft's revision rather than run every frame because it is the one thing on this
    /// screen that is not a memcpy: it stages the whole form over the merged config and hands the
    /// result to `wind_ai`'s reader. Nothing is read from disk and nothing is written — `Config` is
    /// already in memory here — but a hundred-and-fifty-key map clone per frame is a frame budget
    /// spent on a line that changes when a key is pressed.
    fn refresh_ai_status(&mut self) {
        let revision = self.state.ai_draft.revision();
        if self.state.ai_status_for == revision {
            return;
        }
        self.state.ai_status_for = revision;
        let (effective, _) = self.state.ai_draft.validate(&self.state.ai);
        self.state.ai_status = model::AiStatus {
            verdict: ai::verdict(&self.config, &effective),
            key: ai::key_state(&self.config, &effective),
        };
    }

    /// Stage the AI keys and write them. Same one-rename write as `save` and `save_recording`, and
    /// the same reason it is synchronous: the status line belongs to the click that caused it.
    ///
    /// The failure text goes through `ai::scrub` before it reaches the status line, and that is the
    /// only real difference from the other two forms: `ConfigError` interpolates a path, and a path
    /// is allowed to contain anything a user typed into a config file — including, on an install whose
    /// `userdata_dir` was hand-edited, a bearer token. Nothing else on this page has to be careful,
    /// because everything else it paints was already built by `wind_ai`'s redacting `Faults`.
    pub(crate) fn save_ai(&mut self, settings: &AiSettings) {
        settings.stage(&mut self.config);
        let result = self.config.save().map_err(|e| ai::scrub(&e.to_string(), settings));
        if result.is_ok() {
            self.env.config = self.config.clone();
            // The row under the bridge's five fields has to describe the file as it now is, on the
            // same click that made it so — otherwise the page is showing where the service listens
            // from before the change, which is the old silence with a number attached.
            self.state.bridge = ai::bridge_status(&self.root);
        }
        self.state.apply(AppEvent::AiSaved(result));
    }

    /// The close button, when this install runs in the background: stay alive and hide.
    ///
    /// The two questions — did the user ask for this, and is a tray left that can bring the window back —
    /// are [`wind_base::config::Config::window_hides_on_close`]'s, the same answer the HTML window acts on.
    /// Anything else and this returns without a word, so eframe closes the window the way it always did.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || !self.config.window_hides_on_close() {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.window_hidden = true;
        // An invisible window gets no events, so nothing would ever wake this process again: the click
        // that hid it would also have ended it, quietly and forever. It wakes itself instead, on the same
        // beat the HTML window polls on.
        ctx.request_repaint_after(BACKGROUND_POLL);
    }

    /// Has the tray asked for this window back? Consumes the request and un-hides.
    fn watch_for_show(&mut self, ctx: &egui::Context) {
        if !self.window_hidden {
            return;
        }
        ctx.request_repaint_after(BACKGROUND_POLL);
        if wind_base::fslock::take_show_request(&self.config.window_show_signal_path()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.window_hidden = false;
        }
    }

    /// `--exit-after` is a development flag, and it exists so that "it opens a window" can be
    /// checked by a script: it closes the app itself rather than leaving a process behind.
    fn check_exit(&mut self, ctx: &egui::Context) {
        let Some(after) = self.exit_after else { return };
        let elapsed = self.opened.elapsed();
        // The soft deadline waits for the footer's scan so the proof run can report a real count;
        // the hard one exists so a hung disk cannot make the process immortal.
        let settled = !self.state.footer.scanning;
        if (elapsed >= after && settled) || elapsed >= after + Duration::from_secs(20) {
            eprintln!(
                "windui: footer [{}] · {} dropped stale replies · {:.2} ms last frame, {:.2} ms slowest",
                self.state.footer.line(),
                self.state.dropped_stale,
                self.state.paint_ms_last,
                self.state.paint_ms_max
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // egui draws on demand, and a window that has finished its boot has nothing left to draw:
        // without asking for this wake-up, the only thing that would ever look at the clock again is
        // an event that happens to arrive, and a proof run that settled early would leave the process
        // running forever. Fine enough not to spin, short enough to catch the scan finishing late.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// Read-only probes into the parts of the app the render tests watch from outside. They are test
/// accessors, so they exist only in a test build rather than being dead code in a shipped one.
#[cfg(test)]
impl App {
    pub(crate) fn has_texture(&self, key: &crate::model::RowKey) -> bool {
        self.textures.contains(key)
    }

    pub(crate) fn texture_size(&self, key: &crate::model::RowKey) -> (u32, u32) {
        self.textures.size_of(key).unwrap_or((0, 0))
    }

    /// How many rows have been decoded, resident or failed. The prefetch's dedupe set is what makes
    /// this one per row rather than one per frame.
    pub(crate) fn decode_requests(&self) -> usize {
        self.textures.len() + self.state.decode_failures.len()
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Before the paint: a frame that is about to be hidden has no reason to lay out its panels again.
        self.handle_close(ctx);
        self.watch_for_show(ctx);
        self.frame(ctx);
    }
}

/// The two rules the player's stop flag lives by, tested where the flag is raised.
///
/// They cannot live in `model`: the state only *holds* the handle, and the decisions these assert on —
/// which run a `Stop` is allowed to clear, and what starting a stream does to the one already running —
/// belong to whoever can touch a process, which in this crate is the dispatch.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::Library;
    use crate::model::{Player, RowKey};
    use std::sync::atomic::AtomicBool;

    /// A window over a scratch install, with a stream running on a row of its own.
    ///
    /// The `Library` comes back with it and is dropped after the `App`, because a fixture that clears its
    /// own directory while a boot scan is still walking it is a test that fails for the weather.
    fn window_with_a_stream(tag: &str) -> (Library, App) {
        let lib = Library::empty(tag).with_config(r#"{"user_name": "default", "max_page_result": 5}"#);
        let ctx = egui::Context::default();
        let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots on an empty library");
        app.state.player = Some(Player {
            key: RowKey::new("default_2026-09_wind.db", 1),
            name: "2026-09-21_10-00-00.mp4".into(),
            at: 3,
            duration: Some(9),
            waiting: false,
            failure: None,
        });
        app.state.player_run = Some(Arc::new(AtomicBool::new(false)));
        (lib, app)
    }

    /// A `Stop` for a stream the user has already replaced stops *that* stream and leaves the live one
    /// alone. It has to: a seek retires the old handle by overwriting it, and a stale `Stop` that also
    /// cleared `player_run` would cost the window the only way it has left of stopping what is playing —
    /// an ffmpeg no control on screen can reach.
    #[test]
    fn a_stop_for_a_superseded_stream_leaves_the_live_one_running() {
        let (_lib, mut app) = window_with_a_stream("stale-stop");
        let live = app.state.player_run.clone().expect("a stream is running");
        let superseded = Arc::new(AtomicBool::new(false));

        app.dispatch(Command::StopSegment { run: superseded.clone() });
        assert!(superseded.load(Ordering::Relaxed), "the stream named by the stop was told to end");
        assert!(!live.load(Ordering::Relaxed), "and the one that is playing heard nothing");
        assert!(Arc::ptr_eq(app.state.player_run.as_ref().expect("still held"), &live), "the live handle survives a stop that was not for it");
        assert!(app.state.player.is_some(), "and neither is the transport row taken away by it");

        app.dispatch(Command::StopSegment { run: live.clone() });
        assert!(live.load(Ordering::Relaxed), "the stop that WAS for it lands");
        assert!(app.state.player_run.is_none(), "and only then does the window let go of it");
        assert!(app.state.player.is_none(), "with the player gone the viewer is a still again");
    }

    /// One stream at a time, from the side that raises the flag: starting a play stops the run it
    /// replaces, so a seek cannot leave an ffmpeg writing frames for a picture nobody is looking at. The
    /// segment named here does not exist, which is the cheap way to be sure the job that goes out for it
    /// is not what the assertions are waiting on — they are all synchronous.
    #[test]
    fn starting_a_stream_stops_the_one_it_replaces() {
        let (_lib, mut app) = window_with_a_stream("replace-run");
        let live = app.state.player_run.clone().expect("a stream is running");
        let fresh = Arc::new(AtomicBool::new(false));

        app.dispatch(Command::PlaySegment {
            key: RowKey::new("default_2026-09_wind.db", 1),
            segment: PathBuf::from("userdata/videos/2026-09/no-such-segment.mp4"),
            from: 4,
            run: fresh.clone(),
        });
        assert!(live.load(Ordering::Relaxed), "the run the new one replaces was raised");
        assert!(!fresh.load(Ordering::Relaxed), "and the new stream starts unstopped");
        assert!(Arc::ptr_eq(app.state.player_run.as_ref().expect("the new run is live"), &fresh));
        let player = app.state.player.clone().expect("a player was installed");
        assert_eq!(player.at, 4, "it opens on the second that was asked for");
        assert!(player.waiting && player.duration.is_none(), "and it says it is opening, not that it is empty");
    }
}
