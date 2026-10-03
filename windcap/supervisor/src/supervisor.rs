//! The supervisor: what is running, what the menu means, and what each click does.
//!
//! This is the whole of `main.py` minus pystray. It starts and stops other executables, reads the
//! lock files they leave behind, and answers questions about both. It never opens the index and never
//! runs OCR. The one screen it does reach for is its own, and only to file a 🚩 flag — through
//! `wind-notes`' one grab, the same capture the day view uses, never a second GDI path of its own. The
//! moment it started re-implementing those it has stopped being a supervisor and become a second
//! implementation of something that already has a binary.
//!
//! Nothing here blocks on a child. Upstream's menu callbacks ran on a pystray worker thread and slept
//! inside `Popen.wait` or a ten-second URL loop; a tray whose window procedure sleeps is a tray whose
//! icon stops responding, so every wait is expressed as "look again on the next tick". The only
//! blocking call left is the five seconds a stuck recorder is granted before it is killed, which is
//! the one wait that must finish before this process may let go of the record lock's owner.

use std::path::Path;

use wind_base::clock;
use wind_base::config::Config;
use wind_base::fslock::{lock_state, LockState, PidLock};

use crate::child::{self, Child, Stopped};
use crate::ffi;
use wind_base::i18n::Catalog;
use crate::layout::Layout;
use crate::menu::{self, Snapshot};
use crate::native::{self, Missing, Spawn};
use crate::options::Options;
use crate::update;

/// A message for the user. The tray turns these into balloons; the supervisor only decides that
/// something is worth saying, which keeps every string reachable from a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The recording or interface state changed.
    Info { title: String, body: String },
    /// Something the user asked for did not work — the command and the OS error are in the body.
    Failure { title: String, body: String },
}

/// The one judgement behind "the switch means something": what the settings ask for, against what is
/// actually running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeAction {
    /// Leave the process alone. This includes the ordinary state — switch on, bridge up — which must not
    /// spawn a second server every time a menu is opened.
    None,
    Start,
    Stop,
}

/// [`BridgeAction`] for a pair of answers. A free function so all four cases can be named without a
/// tray, a config file or a child process.
pub fn reconcile(wanted: bool, running: bool) -> BridgeAction {
    match (wanted, running) {
        (true, false) => BridgeAction::Start,
        (false, true) => BridgeAction::Stop,
        _ => BridgeAction::None,
    }
}

/// The bridge's five settings, as the bridge itself reads them.
///
/// Held so a change to any one of them can be *noticed*: a recorder that stops is visible on the
/// desktop, a server still answering on the port it bound an hour ago is invisible, and the person who
/// just moved it from 21120 to 21121 in the settings window has no way to tell that nothing moved.
/// `wind_mcp::runtime::Runtime` reads these, which is the same reader the service uses at startup — a
/// second spelling of the five keys here is how the tray ends up obeying a copy nobody edits.
///
/// Deliberately a hand-written `Debug`: the token is in here because rotating it to another
/// same-length secret has to move the service too, and a derived one would put a bearer credential in
/// a test failure or a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct BridgeSettings {
    host: String,
    port: i64,
    auth_required: bool,
    token: String,
}

impl std::fmt::Debug for BridgeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth_required", &self.auth_required)
            .field("token", &format_args!("[REDACTED], {} chars", self.token.chars().count()))
            .finish()
    }
}

impl BridgeSettings {
    /// The address as a person reads it, for the notice that has to name where the service went.
    pub fn authority(&self) -> String {
        wind_mcp::runtime::format_authority(&self.host, self.port)
    }
}

/// The bridge's settings as the bridge would read them, or `None` when the install cannot be read at
/// all — in which case nothing is restarted, because "unknown" is not a change.
fn bridge_settings_of(root: &std::path::Path) -> Option<BridgeSettings> {
    let runtime = wind_mcp::runtime::Runtime::open(root).ok()?;
    Some(BridgeSettings {
        host: runtime.host(),
        port: runtime.port(),
        auth_required: runtime.auth_required(),
        token: runtime.token(),
    })
}

/// Where taking the tray lock left us.
pub enum Boot {
    Ready(Box<Supervisor>),
    /// A live tray holds the lock: the one case where refusing to start is the correct answer. The
    /// message is already translated, because the second copy of the app has no config to read from
    /// this one and the dialog is the only thing it will ever see.
    AlreadyRunning { message: String },
    Failed(String),
}

pub struct Supervisor {
    pub config: Config,
    pub layout: Layout,
    pub catalog: Catalog,
    recorder: Option<Child>,
    interface: Option<Child>,
    /// The `windmcp serve` child, when the user switched the bridge on. A network listener is the
    /// one supervised process whose absence is invisible from the desktop, so it is supervised by
    /// this tray and by no other means.
    bridge: Option<Child>,
    /// What the current bridge child was started with, so `refresh_config` can tell a moved port from
    /// an unchanged one. `None` whenever there is no child of ours.
    bridge_settings: Option<BridgeSettings>,
    /// The settings a "somebody else's bridge is on this port" notice was already raised for. The
    /// refresh runs on every menu open, and a balloon that returns each time you look at the icon is
    /// not information.
    bridge_stale_notice: Option<BridgeSettings>,
    /// Messages raised before there was an icon to hang them on.
    pub boot_notices: Vec<Notice>,
    quit: bool,
    /// The single-instance lock, and the reason `release_tray_lock` has to exist: the supervisor is
    /// reached from the window procedure through a `static`, and a `static` is never dropped.
    tray_lock: Option<PidLock>,
}

impl Supervisor {
    /// Read the config, prepare the directories, take the tray lock, and start recording if the
    /// config says to.
    ///
    /// The recorder is started last and by the same path the menu item uses, so a failure to start it
    /// is a notice rather than a refusal to run: an icon that will not appear because ffmpeg is
    /// missing is strictly worse than an icon whose menu can fix the problem.
    pub fn boot(options: &Options) -> Boot {
        // Absolute before anything is derived from it: the recorder is started with this same string
        // as both its `--root` and its working directory, so a relative one would be resolved twice.
        let root = crate::options::absolute(options.root());
        // Refusing a directory that holds no install is the difference between a tray that says so and
        // one that creates `cache/locks` under an arbitrary path and then reports a recorder failure.
        // Refusing a wrong directory is `wind_base::install`'s judgement, the same one `doctor` makes
        // and the same one every other binary makes: a root that carries its shipped settings — in
        // either layout — is an install, and anything else is a typo. A tray that accepted what the
        // diagnostic tool rejects would offer a menu over somebody's `C:\`.
        if !wind_base::install::is_install_root(&root) {
            return Boot::Failed(format!(
                "{} is not a Windrecorder install — no {} in {} or {}",
                root.display(),
                wind_base::install::DEFAULTS_BASENAME,
                root.join(wind_base::install::CONFIG_SRC).display(),
                root.join(wind_base::install::LEGACY_CONFIG_SRC).display(),
            ));
        }
        let config = match Config::load(&root) {
            Ok(config) => config,
            Err(error) => return Boot::Failed(error.to_string()),
        };
        let layout = Layout::from_config(&config);
        let mut notices = Vec::new();
        if let Err(error) = layout.ensure_directories() {
            return Boot::Failed(error);
        }
        match layout.clear_maintain_markers() {
            Ok(0) => {}
            Ok(removed) => {
                notices.push(Notice::Info {
                    title: "Windrecorder".to_string(),
                    body: format!("Cleared {removed} stale maintenance marker(s) from {}", layout.maintain_lock.display()),
                });
            }
            Err(error) => notices.push(Notice::Failure { title: "Maintenance markers not cleared".to_string(), body: error }),
        }

        // The catalog is read before the lock is taken for one reason: the refusal below has to be
        // able to say it in the user's language.
        let catalog = Catalog::load(&layout.root, &config.str_or("lang", "en"));

        let tray_lock_path = layout.tray_lock.clone();
        // Read before acquiring so the user can be told *which* process to go and find, rather than
        // the bare "Windrecorder is already running." `main.py` produced.
        if let LockState::HeldBy { pid, alive: true } = lock_state(&tray_lock_path) {
            return Boot::AlreadyRunning {
                message: format!(
                    "{}
tray lock {}
owned by running process {pid}",
                    catalog.text("tray_text_already_run"),
                    tray_lock_path.display(),
                ),
            };
        }
        let tray_lock = match PidLock::acquire(&tray_lock_path) {
            Ok(lock) => lock,
            Err(error) => return Boot::Failed(error),
        };

        if let Some(error) = catalog.read_error.clone() {
            notices.push(Notice::Failure { title: "Translations unavailable".to_string(), body: error });
        }

        // The first-run layout, and the reason it sits exactly here: after the tray lock (so only one
        // tray can lay a tree out), after the catalog (so the notice it raises is in the user's
        // language), and before the migration below (because seeding `config_user.json` first is what
        // lets `migrate` reconcile a real settings file instead of noting that there is nothing to
        // reconcile). Until now `windsetup init` was a command a person had to read about and type
        // before the icon meant anything; the tray is the one process every desktop start passes
        // through, so the step is here and the knowledge stays in `windsetup`.
        let user_config = config.userdata_dir().join(CONFIG_USER_FILE);
        if first_run_pending(&user_config) {
            if let Err(error) = run_startup_init(&root) {
                return Boot::Failed(error);
            }
            notices.push(Notice::Info {
                title: catalog.text("tray_notify_title"),
                body: format!("{}\n{}", catalog.text("tray_first_run_laid_out"), user_config.display()),
            });
        }

        // The upgrade migration, and the reason it sits exactly here: after the tray lock (so only one
        // tray can migrate) and before the recorder or the bridge is built (so nothing writes to a tree
        // whose month files are missing their `win_title`/`deep_linking` columns, or whose folders have
        // not yet moved under `userdata/`). It is the tray's job to *trigger* it — the tray is the one
        // process every desktop launch passes through — and `windsetup`'s job to perform it, because a
        // supervisor that opened the index would be a second implementation of the migrator. `migrate`
        // is re-entrant and a no-op on an up-to-date tree, so this costs an already-migrated install one
        // fast, clean exit. A non-zero exit is not swallowed: the tray refuses to start rather than let
        // recording begin on data the migration could not bring forward — see [`run_startup_migration`].
        if let Err(error) = run_startup_migration(&root) {
            return Boot::Failed(error);
        }

        let mut supervisor = Supervisor {
            config,
            layout,
            catalog,
            recorder: None,
            interface: None,
            bridge: None,
            bridge_settings: None,
            bridge_stale_notice: None,
            boot_notices: notices,
            quit: false,
            tray_lock: Some(tray_lock),
        };
        if supervisor.start_recording_on_startup() {
            // `main.py` calls `start_stop_recording()` here, which toggles — so a second copy of the
            // tray started while a recorder was already running would *stop* that recording. The
            // startup path may only ever begin one, and it stays out of a lock it does not own.
            let already_running = supervisor.recording();
            let started = if already_running { Vec::new() } else { supervisor.start_record() };
            supervisor.boot_notices.extend(started);
        } else if let Err(missing) = native::recorder_argv(&supervisor.layout.root) {
            // Nothing is being attempted, so the idle icon is the config working as told. Say it
            // anyway: on this install the only thing between "recording is paused" and "there is
            // no recorder to unpause" is a config key nobody looking at the icon can see, and the
            // two states are otherwise one balloon apart.
            supervisor.boot_notices.push(cannot_launch(missing));
        }
        // Last, and by the same rule: the bridge is a service the config asks for, so booting the
        // tray is what starts it, and a failure to start it is a notice rather than a refusal to
        // show an icon. An install with `enable_mcp_server` on and no `windmcp` in it would
        // otherwise look exactly like the install that had no way to turn the bridge on at all.
        let started_bridge = supervisor.start_bridge();
        supervisor.boot_notices.extend(started_bridge);
        Boot::Ready(Box::new(supervisor))
    }

    /// The switch that decides what the icon looks like when it first appears.
    pub fn start_recording_on_startup(&self) -> bool {
        self.config.bool_or("start_recording_on_startup", true)
    }

    /// Is a recording running?
    ///
    /// Ours if we started one, and otherwise whatever holds the record lock: a recorder launched from
    /// a terminal or a scheduled task is still a recording, and a menu that ignored
    /// the lock would offer to start a second one and double every row the user searches for.
    fn recording(&mut self) -> bool {
        let died = match self.recorder.as_mut() {
            Some(child) => child.exited().is_some(),
            None => false,
        };
        if died {
            self.recorder = None;
        }
        self.recorder.is_some() || self.record_lock_is_held()
    }

    fn record_lock_is_held(&self) -> bool {
        matches!(lock_state(&self.layout.record_lock), LockState::HeldBy { alive: true, .. })
    }

    /// Is the MCP bridge up?
    ///
    /// The same two halves as `recording`, for the same reason. Ours if we started it; otherwise
    /// whatever the bridge's pid file names, because a `windmcp serve` launched by hand from a
    /// terminal is still a process holding the port, and a tray that ignored that would start a
    /// second one which `bind` refuses — a worse report than the one the file already gives.
    fn bridge_up(&mut self) -> bool {
        let died = match self.bridge.as_mut() {
            Some(child) => child.exited().is_some(),
            None => false,
        };
        if died {
            self.bridge = None;
            self.release_bridge_lock();
        }
        self.bridge.is_some() || self.bridge_lock_is_held()
    }

    fn bridge_lock_is_held(&self) -> bool {
        matches!(lock_state(&self.layout.bridge_lock), LockState::HeldBy { alive: true, .. })
    }

    /// `windmcp serve --root <root>`, when `enable_mcp_server` says so.
    ///
    /// There is no click-shaped toggle for this, deliberately: the switch is one config key that
    /// both this process and the bridge read, and a second copy of it that only the tray obeyed is
    /// exactly how the key ended up wired to nothing in the first place. What the tray owes the user
    /// is that setting the key makes the service run, and that when it cannot, they are told here —
    /// not left to infer the state from an AI client that quietly gets nothing back.
    fn start_bridge(&mut self) -> Vec<Notice> {
        if !native::bridge_enabled(&self.config) {
            return Vec::new();
        }
        if self.bridge_up() {
            return Vec::new();
        }
        let Some(spawn) = native::bridge_argv(&self.layout.root) else {
            // Enabled and no binary. Silent would be the old bug wearing a different hat.
            return vec![Notice::Failure {
                title: "Cannot start the MCP bridge".to_string(),
                body: "enable_mcp_server is on, but no windmcp binary was found in this install. \
                       `windsvc doctor` lists every directory it searched; windcap/build.ps1 builds \
                       it, and a release payload carries it in bin\\.".to_string(),
            }];
        };
        match child::start(&spawn, self.layout.bridge.clone(), &self.layout.root) {
            Ok(child) => {
                let pid = child.pid();
                let mut notices = self.claim_bridge_lock(pid);
                self.bridge = Some(child);
                // Recorded on the way in, so the next refresh can answer the only question that matters
                // after a settings change: is the process holding the port the one these settings ask
                // for?
                self.bridge_settings = bridge_settings_of(&self.layout.root);
                self.bridge_stale_notice = None;
                notices.push(Notice::Info {
                    title: self.catalog.text("tray_mcp_running"),
                    // The `.err` half is the one worth naming: `windmcp` writes its bind banner —
                    // and every refusal — to stderr, before the accept loop ever opens.
                    body: format!("{}\n{}", spawn.describe(), self.layout.bridge.err.display()),
                });
                notices
            }
            Err(error) => vec![Notice::Failure {
                title: "Cannot start the MCP bridge".to_string(),
                body: format!("{}\n{error}", spawn.describe()),
            }],
        }
    }

    /// Stop the bridge on the way out.
    ///
    /// A server has no half-written segment to protect, so there is nothing to be graceful about and
    /// `stop_forced` is the whole stop. Nothing of ours means nothing is touched: a lock naming a
    /// process this tray never held a handle to is somebody else's server, and killing a pid read
    /// out of a file is not something the tray does — `stop_record` refuses for the same reason.
    fn stop_bridge(&mut self) {
        let Some(child) = self.bridge.take() else { return };
        child.stop_forced();
        self.bridge_settings = None;
        self.release_bridge_lock();
    }

    /// Let go of the bridge on any way out of the message loop, not only the Exit item.
    ///
    /// The recorder is deliberately *not* treated this way: a tray that ends because the shell
    /// recycled its window, or because a pump message arrived that this process has no handler for,
    /// must not silently end the segment being written. A listener has no work in flight and no icon
    /// that shows it, so the only state in which leaving one running is acceptable is one somebody
    /// chose — and a tray on its way out is not a choice. Without this, `windsvc` could exit and
    /// leave a port open behind it with nothing left on the desktop to say so.
    ///
    /// Idempotent with [`Supervisor::exit`], which stops the bridge first and leaves nothing to take.
    pub fn relinquish_bridge(&mut self) {
        self.stop_bridge();
    }

    /// Leave the child's pid where a separate process can find it.
    ///
    /// Deliberately not a `PidLock`: that type writes *this* process's own pid and deletes the file
    /// when dropped, and what needs recording here is a different process. The tray is the only
    /// writer — it holds the tray lock, so no second tray can race it — which is why a plain write
    /// is enough and no acquire protocol is wanted. A failure is reported rather than swallowed,
    /// because an unwritten lock makes `doctor` answer "not running" about a listener that is.
    fn claim_bridge_lock(&self, pid: u32) -> Vec<Notice> {
        let path = &self.layout.bridge_lock;
        match std::fs::write(path, pid.to_string()) {
            Ok(()) => Vec::new(),
            Err(error) => vec![Notice::Failure {
                title: "MCP bridge state is not reportable".to_string(),
                body: format!("{}: {error}\nA bridge is running and `windsvc doctor` cannot see it.", path.display()),
            }],
        }
    }

    /// Take the bridge's pid file back. Only ever called about a child this tray owned.
    fn release_bridge_lock(&self) {
        let _ = std::fs::remove_file(&self.layout.bridge_lock);
    }

    /// Reap whatever has died — the recorder, the bridge, an interface window the user closed by
    /// hand — and finish any pending interface startup. Returns the messages the tray should raise,
    /// which are none almost every tick.
    pub fn tick(&mut self) -> Vec<Notice> {
        let mut notices = Vec::new();
        let recorder_exit = self.recorder.as_mut().and_then(|child| child.exited());
        if recorder_exit.is_some() {
            let child = self.recorder.take().expect("just seen alive");
            // Saying nothing would leave the icon claiming to record; saying *why* the user should
            // look is what the log path in the body is for.
            notices.push(Notice::Failure {
                title: self.catalog.text("tray_notify_title_record_pause"),
                body: format!(
                    "The recording process exited with {}. Check {}.",
                    recorder_exit.unwrap_or(-1),
                    child.logs.out.display()
                ),
            });
        }
        let bridge_exit = self.bridge.as_mut().and_then(|child| child.exited());
        if bridge_exit.is_some() {
            let child = self.bridge.take().expect("just seen alive");
            // The pid file describes a process that has stopped, so it goes with it: left behind, it
            // would tell the next start that a live bridge holds the port when nothing does.
            self.release_bridge_lock();
            // A bridge dies for reasons the user can fix — a token too short to be a secret, a port
            // already taken — and every one of them is written to stderr before the accept loop
            // opens. Without this balloon the failure is invisible from the desktop, which is the
            // defect this feature exists to close.
            notices.push(Notice::Failure {
                title: self.catalog.text("tray_mcp_stopped"),
                body: format!(
                    "The MCP bridge process exited with {}, so nothing is listening. Check {}.",
                    bridge_exit.unwrap_or(-1),
                    child.logs.err.display()
                ),
            });
        }
        let interface_gone = match self.interface.as_mut() {
            Some(child) => child.exited().is_some(),
            None => false,
        };
        if interface_gone {
            // A window dies the instant the user closes it and nothing tells the tray, so this poll
            // is the only thing that turns "Stop window" back into "Start window".
            self.interface = None;
        }
        notices
    }

    /// Re-read the settings, and bring the running processes in line with them.
    ///
    /// The tray used to read the config exactly once, at boot. That left two of the window's switches
    /// inert: the language row changed nothing in the tray until a restart, and `enable_mcp_server` —
    /// which the settings page only just gained a door for — decided nothing at all after boot, so a user
    /// could turn the bridge on and still get a refused connection until they happened to restart the
    /// tray. Both are the dead-control shape this product has now been fixed for twice. The menu is
    /// rebuilt on every opening anyway, so this runs at a moment the user is about to see the answer, and
    /// the two JSON reads it costs are nothing beside a click.
    pub fn refresh_config(&mut self) -> Vec<Notice> {
        let Ok(config) = wind_base::config::Config::load(&self.layout.root) else {
            return Vec::new();
        };
        let mut notices = Vec::new();
        let lang = config.str_or("lang", "en");
        if lang != self.catalog.lang() {
            self.catalog = wind_base::i18n::Catalog::load(&self.layout.root, &lang);
        }
        // Asked for and not running, or running and no longer asked for. `bridge_up` reaps a dead child
        // first, so a bridge that crashed is restarted by the same rule rather than reported as up.
        let wanted = native::bridge_enabled(&config);
        let running = self.bridge_up();
        self.config = config;
        match reconcile(wanted, running) {
            BridgeAction::None => {
                // "Switched on and a bridge is up" is not the same claim as "this bridge, on this
                // port". A recorder that stopped is visible; a server still answering on the address it
                // bound an hour ago is not, and the person who just moved the port in the settings
                // window has no way to learn that nothing moved. So the tray moves the service with the
                // settings — ours only: a `windmcp serve` started from a terminal is somebody else's
                // process, and the tray does not kill what it holds no handle to. That case gets said
                // once, and stays said.
                if wanted {
                    let now = bridge_settings_of(&self.layout.root);
                    let moved = now.is_some() && self.bridge_settings != now;
                    if moved {
                        if self.bridge.is_some() {
                            self.stop_bridge();
                            notices.extend(self.start_bridge());
                        } else if self.bridge_stale_notice != now {
                            self.bridge_stale_notice = now.clone();
                            notices.push(Notice::Info {
                                title: self.catalog.text("tray_mcp_running"),
                                body: format!(
                                    "the settings ask for {} now, but the bridge holding that port was not started by this tray, so it keeps \
                                     running where it is. Stop that `windmcp serve` and the next refresh starts one on the address in the \
                                     settings window.",
                                    now.map(|settings| settings.authority()).unwrap_or_default()
                                ),
                            });
                        }
                    }
                }
            }
            BridgeAction::Start => notices.extend(self.start_bridge()),
            BridgeAction::Stop => {
                self.stop_bridge();
                notices.push(Notice::Info {
                    title: self.catalog.text("tray_mcp_stopped"),
                    body: "enable_mcp_server was switched off in the settings window, so the bridge was stopped.".to_string(),
                });
            }
        }
        notices
    }

    /// The menu's labels are a function of this, so it is the one place the truth is assembled.
    pub fn snapshot(&mut self) -> Snapshot {
        let recording = self.recording();
        let running = self.interface.is_some();
        // Read after every other question, because `bridge_up` is the one that reaps a dead child.
        let bridge_running = self.bridge_up();
        Snapshot {
            recording,
            interface_running: running,
            current_version: update::local_version(&self.layout.root),
            changelog_present: self.layout.changelog_target().is_some(),
            bridge_enabled: native::bridge_enabled(&self.config),
            bridge_running,
            // What the recorder last said it was doing. Read rather than remembered: a recorder started
            // from a terminal is not our child, and its file is the only place its state can be seen.
            capture: wind_base::fslock::read_capture(&self.config.record_state_path()),
        }
    }

    /// ⏸️ / ▶️ — the item the icon exists for.
    pub fn toggle_record(&mut self) -> Vec<Notice> {
        if self.recording() {
            let mut notices = self.stop_record();
            let snapshot = self.snapshot();
            let (title, body) = menu::balloon(&snapshot, &self.catalog);
            notices.push(Notice::Info { title, body });
            notices
        } else {
            self.start_record()
        }
    }

    fn start_record(&mut self) -> Vec<Notice> {
        let spawn = match native::recorder_argv(&self.layout.root) {
            Ok(spawn) => spawn,
            // Nothing was spawned, so there is no log file to point at and no exit code to quote.
            // What the user gets instead is the file that is absent and every directory it was
            // looked for in -- see `cannot_launch`.
            Err(missing) => return vec![cannot_launch(missing)],
        };
        match child::start(&spawn, self.layout.recording.clone(), &self.layout.root) {
            Ok(child) => {
                self.recorder = Some(child);
                let (title, body) = menu::balloon(&self.snapshot(), &self.catalog);
                vec![Notice::Info { title, body }]
            }
            Err(error) => vec![Notice::Failure {
                title: "Cannot start recording".to_string(),
                body: format!("{}\n{error}", spawn.describe()),
            }],
        }
    }

    fn stop_record(&mut self) -> Vec<Notice> {
        let Some(child) = self.recorder.take() else {
            // Nothing of ours is running, so there is nothing we can signal: the lock belongs to
            // somebody else's process, and a break addressed at a pid we cannot open a console for
            // would be a guess about whose recorder we are killing.
            return vec![Notice::Failure {
                title: "Cannot pause recording".to_string(),
                body: format!(
                    "A recorder holds {}, but this tray did not start it, so no stop signal can be sent to it. Stop that process directly.",
                    self.layout.record_lock.display()
                ),
            }];
        };
        let line = child.line.clone();
        match child.stop_gracefully() {
            stopped @ (Stopped::KilledAfterTimeout | Stopped::KilledAfterSignalFailure { .. }) => vec![Notice::Failure {
                title: "Recording stopped hard".to_string(),
                body: format!("{}\n{line}\n{}", stopped.failure().unwrap_or_default(), LOG_HINT),
            }],
            // `Graceful` and `AlreadyGone` need no message of their own: `toggle_record` follows this
            // with the "recording paused" balloon every stop produces.
            _ => Vec::new(),
        }
    }

    /// 🦝 — start or stop the search and settings window.
    pub fn toggle_interface(&mut self) -> Vec<Notice> {
        if let Some(child) = self.interface.take() {
            child.stop_forced();
            return Vec::new();
        }
        self.start_interface()
    }

    /// The one path that starts the interface (`native::INTERFACE`, so `winduiweb.exe`), shared by the
    /// menu's toggle and the icon's double click.
    ///
    /// Both gestures used to reach the window by different routes, and only one of them worked; keeping
    /// a single spawn here is what stops the two from drifting apart again, and it is the only place
    /// `ui_argv` is called for a launch rather than for a report.
    fn start_interface(&mut self) -> Vec<Notice> {
        let spawn = match native::ui_argv(&self.layout.root) {
            Ok(spawn) => spawn,
            Err(missing) => return vec![cannot_launch(missing)],
        };
        match child::start(&spawn, self.layout.interface.clone(), &self.layout.root) {
            Ok(child) => {
                // A window never announces that it is ready and serves no address, so there is
                // nothing to wait for and nothing to scrape. `tick` is what notices the close.
                self.interface = Some(child);
                Vec::new()
            }
            Err(error) => vec![Notice::Failure {
                title: "Cannot start the interface".to_string(),
                body: format!("{}\n{error}", spawn.describe()),
            }],
        }
    }

    /// The default item, and the icon's double-click target: bring the window up.
    ///
    /// This answered "No address to open" for every state, which was a true statement about the *menu
    /// row* it inherited — upstream's default item was a URL handed to a browser, and there is no URL
    /// left — read as though it were the click's contract. The effect was that the shell's most
    /// common gesture on a tray icon produced a guaranteed error balloon while the window sat there
    /// unopenable except from the menu. There is no third answer now: if `winduiweb.exe` cannot start,
    /// [`start_interface`] says so, naming the file and every directory it searched.
    pub fn open_interface(&mut self) -> Vec<Notice> {
        match double_click_action(self.interface.is_some()) {
            DoubleClick::Raise => self.raise_interface(),
            DoubleClick::Start => self.start_interface(),
        }
    }

    /// Ask the window this tray is running to come forward.
    ///
    /// One file, consumed by the window's own watcher: the tray cannot see whether a child window is
    /// hidden, minimised or simply behind a browser, and it has no business guessing. If the window is
    /// not listening — because it closed for good, or because it is a build that does not run the
    /// watcher — the signal sits there harmlessly until something takes it.
    fn raise_interface(&self) -> Vec<Notice> {
        let signal = self.config.window_show_signal_path();
        match wind_base::fslock::request_show(&signal) {
            Ok(()) => Vec::new(),
            Err(error) => vec![Notice::Failure {
                title: self.catalog.text("tray_native_window_raise"),
                body: format!("The window was asked to come forward and the request could not be written.\n{error}"),
            }],
        }
    }

    /// 🚩 Flag this instant, with the screen that is up right now filed beside it.
    ///
    /// The row is appended through `wind-notes`' store — the same single writer the editor and the CLI
    /// use — so the tray never carries its own copy of the CSV's shape. The thumbnail is `wind-notes`'
    /// one grab (`capture::capture_frame`), the same preview-scale JPEG (`thumbnail_generation_size_width`) the index and the day view share; the
    /// tray does not reimplement it. Upstream's tray did neither: it wrote `["", stamp, "_"]`, an empty
    /// thumbnail and a placeholder note, on the documented reasoning that the tray must not touch the
    /// screen and the flagged frame is on the strip anyway. That reasoning no longer holds once the
    /// capture path is reachable from here, so the tray now grabs — and a flag becomes a picture of a
    /// moment rather than a timestamp pointing at one. The note is still the placeholder: typing it is
    /// the editor's job, and opening a text box from a tray menu is not.
    ///
    /// A failed grab still files the flag, just with no picture: a bookmark that lost its thumbnail
    /// beats a menu item that says "cannot add the mark" because the desktop moved while the user was
    /// reaching for it.
    pub fn flag_now(&self) -> Vec<Notice> {
        match append_flag_with_thumbnail(&self.config, &self.layout.flag_note) {
            Ok(stamp) => vec![Notice::Info {
                title: self.catalog.text("tray_add_flag_mark_note_for_now"),
                body: format!("{stamp}\n{}", self.layout.flag_note.display()),
            }],
            Err(error) => vec![Notice::Failure {
                title: "Cannot add the mark".to_string(),
                body: format!("{}: {error}", self.layout.flag_note.display()),
            }],
        }
    }

    /// 🚀 See what's new — open the changelog this install actually carries.
    ///
    /// The row above this function's old twin used to hand the shell a script that no release ships
    /// any more; the menu now only builds the row when [`Layout::changelog_target`] resolves to a
    /// file on disk, and this opens exactly that file. The `None` arm exists for the race in which
    /// the file goes away between the menu being built and the click landing, and says which names
    /// were looked for rather than opening nothing quietly.
    pub fn open_changelog(&self) -> Vec<Notice> {
        changelog_notices(&self.layout)
    }

    /// ❌ — stop the interface, stop the bridge, stop the recorder the same graceful way the menu
    /// item uses, then let the lock drop. The `CTRL_BREAK_EVENT` is not skipped here either: quitting
    /// the tray must not cost the user the segment being written.
    ///
    /// The bridge goes before the recorder because the recorder's stop is the one that may take its
    /// full five-second budget, and a tray on its way out should not spend that time with a port
    /// still open behind it.
    pub fn exit(&mut self) -> Vec<Notice> {
        self.quit = true;
        let mut notices = Vec::new();
        if let Some(child) = self.interface.take() {
            child.stop_forced();
        }
        self.stop_bridge();
        if self.recorder.is_some() {
            notices.extend(self.stop_record());
        }
        notices
    }

    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// Hand the tray lock back.
    ///
    /// `PidLock`'s `Drop` would do it, but the supervisor is reached from the window procedure
    /// through a `static`, and dropping a `static` is not a thing that happens — so an exited tray
    /// would leave a lock file naming a pid that no longer exists, and the next start would have to
    /// prove that pid dead before it could run.
    pub fn release_tray_lock(&mut self) {
        self.tray_lock = None;
    }
}

/// A supervisor that goes away does not leave a network listener behind.
///
/// The explicit `relinquish_bridge` on the way out of the pump covers the normal ending; this covers
/// the endings that never reach it, where `run` returns early because the shell refused the icon or
/// the message window could not be registered — paths that have already started a bridge in `boot`
/// and would otherwise drop the child handle and leave the port open with nothing supervising it and
/// no balloon to explain it. The recorder is deliberately absent from this: a tray that failed to
/// draw its icon must not silently end the segment being written.
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_bridge();
    }
}

/// The tray's 🚩 data path: grab the current screen, then append one flag row through `wind-notes`.
///
/// The grab runs on a short-lived thread rather than the tray's message loop because
/// `wind_notes::capture::capture_frame` makes the calling thread per-monitor DPI aware
/// (`SetThreadDpiAwarenessContext`), and changing the loop thread's context would shift the geometry of
/// the very popup menu that is calling this. Joining costs the user one imperceptible pause on a click
/// they chose; the alternative — a second GDI grab written here — is exactly what the module note
/// forbids, and it is also what the day view and the CLI already share.
///
/// A failed grab files the flag with an empty thumbnail; only a failure to *write* is reported. Returns
/// the stamped datetime so the balloon can name the moment it bookmarked.
fn append_flag_with_thumbnail(config: &Config, path: &Path) -> Result<String, String> {
    use wind_notes::flag::{Flag, NOTE_PLACEHOLDER};
    use wind_notes::store::FlagStore;
    let when = clock::now();
    let stamp = when.display();
    let grab = config.clone();
    let thumbnail = std::thread::spawn(move || wind_notes::capture::capture_frame(&grab).map(|(base64, _)| base64).unwrap_or_default())
        .join()
        .unwrap_or_default();
    let mut store = FlagStore::load(path).map_err(|e| e.to_string())?;
    store.append_persisted(Flag { thumbnail, when, note: NOTE_PLACEHOLDER.to_string() }, false).map_err(|e| e.to_string())?;
    Ok(stamp)
}

/// The two things a double-click on the tray icon can mean, and the only two.
#[derive(Debug, PartialEq, Eq)]
enum DoubleClick {
    /// No window is up: start one.
    Start,
    /// One is already up: ask it to come forward. With close-to-tray on — the default — a window the
    /// user "closed" is alive and hidden, and this is the only gesture that brings it back. When the
    /// window is simply behind another, the raise re-focuses it, which is what a double-click on an
    /// icon is for; a visible window losing focus is a smaller surprise than a click that does nothing.
    Raise,
}

/// What the icon's double-click means, given the only state the tray can see. Extracted because
/// `cargo test` builds no binaries and a test of the real method would open a window on whoever ran
/// the suite; the enum having exactly these two arms *is* the assertion, since the bug this replaced
/// was a third one — a refusal, returned whatever the state was.
fn double_click_action(interface_running: bool) -> DoubleClick {
    if interface_running { DoubleClick::Raise } else { DoubleClick::Start }
}

/// The one notice for "this install has no binary for that".
///
/// Every other failure the tray can hit is a process that started and then did something wrong,
/// and its message carries an exit code and names a log file. This one has neither: nothing was
/// spawned, so there is no log to open and no code to quote, and the only facts worth putting in
/// front of a user are which file is absent, every place it was looked for, and that an absent
/// file is not the same thing as a feature they switched off. `Failure` rather than `Info` because
/// it is the shell's error icon that tells them this is not the ordinary "recording paused"
/// balloon they get alongside it.
fn cannot_launch(missing: Missing) -> Notice {
    Notice::Failure { title: missing.title(), body: missing.explain() }
}

/// What clicking 🚀 "See what's new" does: hand the install's own changelog to the shell.
///
/// Split out so the *decision* — which file, or the honest refusal when there is none — is testable
/// against a scratch layout without a desktop and without opening a real file in an editor. The menu
/// only builds the row when a target exists, so the `None` arm is reachable solely by a file deleted
/// between building the menu and the click landing; it still answers in words, because a silent row
/// is how this tray's release story went wrong the first time.
fn changelog_notices(layout: &Layout) -> Vec<Notice> {
    match layout.changelog_target() {
        Some(target) => to_notices(
            open_externally(&target.display().to_string(), &layout.root),
            "Cannot open the changelog",
        ),
        None => vec![Notice::Failure {
            title: "No changelog in this install".to_string(),
            body: format!(
                "Nothing to open: this root carries neither {} nor {}.",
                layout.release_notes.display(),
                layout.changelog.display()
            ),
        }],
    }
}

/// The file whose presence means somebody has already initialised this install.
///
/// `windsetup init` seeds it and never overwrites one that exists, which makes it the single path
/// that answers "first run?" without reading the settings it would have seeded. `userdata/` alone is
/// not the signal: the recorder half-creates its own folders (`store/src/write.rs:51`), so a tree can
/// hold `userdata/db` and still have no config, no `result_*` folders and no answer for `windsetup
/// doctor`. Four places in the workspace already spell this path off `userdata_dir()` —
/// `maint/src/doctor.rs:66`, `setup/src/configfile.rs`, `mcp/src/fixture.rs:44`, `ai/src/args.rs:293`
/// — and this is the fifth, not a new rule.
const CONFIG_USER_FILE: &str = "config_user.json";

/// Is this the first run? See [`CONFIG_USER_FILE`] for why that one file is the whole answer. Takes
/// the path rather than the install so the decision is testable without a tree to lay out and, per
/// this module's rule that `cargo test` builds no binaries, without a `windsetup.exe` to run.
fn first_run_pending(user_config: &Path) -> bool {
    !user_config.is_file()
}

/// Run `windsetup init` to completion before the tray is allowed to record onto the tree.
///
/// The counterpart of [`run_startup_migration`] for the other half of a first run: the migration
/// brings existing data forward, this one creates the tree there was never any data in. Both are
/// `windsetup`'s, both run in `boot` before anything is spawned, and both refuse the tray on a
/// non-zero exit — a recording written into a half-created install is the same class of damage as one
/// written onto an unmigrated one. Output is piped for the same reason: the whole result is one
/// decision plus, on failure, the lines explaining it.
fn run_startup_init(root: &Path) -> Result<(), String> {
    let spawn = native::init_argv(root).map_err(|missing| missing.explain())?;
    let output = std::process::Command::new(&spawn.program)
        .args(&spawn.args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run the first-run layout\n  {}\n{e}", spawn.describe()))?;
    classify_init_exit(&spawn, output.status.code(), &output.stderr, &output.stdout)
}

/// Map `init`'s exit status to "the tray may start" or a message saying why not.
///
/// Separate from [`classify_migration_exit`] on purpose rather than merged into it: what each refusal
/// tells a user to go and do differs, because the failures do. A blocked migration points at
/// `windsetup`'s BLOCKED report and a tree that can still be brought forward; a failed layout points
/// at a directory that could not be created, which is a disk, permission or antivirus problem with no
/// migration to re-run — and, on an install whose `userdata_dir` was edited to sit outside the root,
/// at a `confine` refusal only `init` makes. Sharing one sentence between them would send somebody to
/// re-run `migrate` while their `userdata/` still does not exist.
fn classify_init_exit(spawn: &Spawn, code: Option<i32>, stderr: &[u8], stdout: &[u8]) -> Result<(), String> {
    if code == Some(0) {
        return Ok(());
    }
    Err(format!(
        "Windrecorder could not lay out this install on its first run: {} exited with {code:?}.\n\
         It will not record onto a tree whose data folders are missing. Run `{}` yourself and \
         resolve what it reports; `init` creates only what is absent and never rewrites \
         userdata/config_user.json, so re-running it once the problem is cleared is safe.\n\
         {}",
        spawn.program.file_name().and_then(|s| s.to_str()).unwrap_or("windsetup"),
        spawn.describe(),
        setup_output_detail(stderr, stdout),
    ))
}

/// The lines a startup `windsetup` run left behind, stderr first because that is where both of its
/// refusals write. Shared by [`classify_migration_exit`] and [`classify_init_exit`] so neither can
/// quietly start preferring stdout over the other.
fn setup_output_detail(stderr: &[u8], stdout: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr);
    let trimmed = err.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    let out = String::from_utf8_lossy(stdout);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return "(the process wrote no explanation — see `windsetup doctor`)".to_string();
    }
    trimmed.to_string()
}

/// Run `windsetup migrate` to completion and decide whether the tray may proceed.
///
/// This is the only place in the tray that spawns a process and *waits* for it, which is legitimate
/// only because it happens during `boot`, before the message window exists — so a slow migration is a
/// late-appearing icon, never a frozen one, and the invariant that the window procedure never blocks on
/// a child is untouched. Output is piped rather than logged to a file because the whole result is one
/// decision (may I start?) plus, on failure, the last lines explaining it; the migration writes its own
/// detailed report and backups under `userdata/`.
///
/// A missing `windsetup.exe`, a failure to launch it, and a non-zero exit all become `Err`, and `boot`
/// turns that into [`Boot::Failed`] — a modal "cannot start", not a silent continue. That is the loud
/// half of the fix: the defect this closes is an upgrade that looked healthy while its data stayed
/// unmigrated, so the tray may not proceed on a tree it cannot prove was brought forward.
fn run_startup_migration(root: &Path) -> Result<(), String> {
    let spawn = native::migrate_argv(root).map_err(|missing| missing.explain())?;
    let output = std::process::Command::new(&spawn.program)
        .args(&spawn.args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run the upgrade migration\n  {}\n{e}", spawn.describe()))?;
    classify_migration_exit(&spawn, output.status.code(), &output.stderr, &output.stdout)
}

/// Map a migration process's exit status to "the tray may start" or a message that says why not.
///
/// Split out from [`run_startup_migration`] so the *decision* — the part that can wrongly let an
/// unmigrated tree through, or wrongly refuse a clean one — is testable without an install to migrate or
/// a `windsetup.exe` to spawn (`cargo test` does not build binaries). Zero is the only success: a
/// migration that hit a blocker exits non-zero and names it on stderr, and that is exactly the tree the
/// tray must not record onto.
fn classify_migration_exit(spawn: &Spawn, code: Option<i32>, stderr: &[u8], stdout: &[u8]) -> Result<(), String> {
    if code == Some(0) {
        return Ok(());
    }
    Err(format!(
        "the upgrade migration did not complete: {} exited with {code:?}.\n\
         Windrecorder will not record onto an unmigrated install. Run `{}` yourself and resolve \
         everything it reports as BLOCKED; the migration is re-entrant and copies every file to \
         userdata/backup/ before it changes it, so re-running once the blocker is cleared is safe.\n\
         {}",
        spawn.program.file_name().and_then(|s| s.to_str()).unwrap_or("windsetup"),
        spawn.describe(),
        setup_output_detail(stderr, stdout),
    ))
}

/// What every "check the log" message points at.
const LOG_HINT: &str = "Check the recording log for what it was doing.";

fn to_notices(result: Result<(), String>, title: &str) -> Vec<Notice> {
    match result {
        Ok(()) => Vec::new(),
        Err(body) => vec![Notice::Failure { title: title.to_string(), body }],
    }
}

/// Ask the shell to open a file. Upstream's `webbrowser.open` was `os.startfile` on a path and a URL
/// handler on a string, and it was handed both a changelog URL and the updater script; `ShellExecuteW`
/// with the `open` verb is the same first half, and the changelog is now the only thing here that
/// needs it — as a file inside the install, never as an address something has to be online to reach.
fn open_externally(target: &str, directory: &Path) -> Result<(), String> {
    let operation = ffi::wide("open");
    let object = ffi::wide(target);
    let folder = ffi::wide(&directory.display().to_string());
    // SAFETY: all three buffers outlive the call, and a null window means "no owner".
    let returned = unsafe {
        ffi::ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            object.as_ptr(),
            std::ptr::null(),
            folder.as_ptr(),
            ffi::SW_SHOWNORMAL,
        )
    };
    if ffi::shell_execute_failed(returned) {
        return Err(format!("could not open {target}\nshell error {returned}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The tray decides "did the settings move" by comparing what its bridge was started with against
    /// what the file says now, and both halves come through `wind-mcp` — the reader the service itself
    /// uses at startup. A second reading of the five keys in the tray is how a moved port stops meaning
    /// anything, which is the bug this pins: the defaults here are asserted against the bridge's own
    /// constants, not against numbers this file invented.
    #[test]
    fn the_bridge_settings_the_tray_compares_come_from_the_bridges_own_reader() {
        let dir = std::env::temp_dir().join(format!("windsvc-bridge-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), r#"{"enable_mcp_server": true}"#).unwrap();

        let shipped = bridge_settings_of(&dir).expect("the fixture is an install");
        assert_eq!(shipped.port, wind_mcp::runtime::DEFAULT_PORT);
        assert_eq!(shipped.host, wind_mcp::runtime::DEFAULT_HOST);
        assert_eq!(shipped.authority(), format!("{}:{}", shipped.host, shipped.port));

        std::fs::write(dir.join("userdata/config_user.json"), r#"{"enable_mcp_server": true, "mcp_server_port": 21121}"#)
            .unwrap();
        let moved = bridge_settings_of(&dir).expect("the edited file still loads");
        assert_ne!(shipped, moved, "a moved port is a change the tray can see");
        assert_eq!(moved.port, 21121);
        assert_eq!(moved.host, shipped.host, "and only the port moved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Replaces `streamlit_urls_are_scraped_the_way_main_py_scrapes_them`, which proved the tray could
    /// read a `Local URL:` line out of a webui log. Nothing prints one any more, so what is pinned here
    /// is the promise that took its place in the launch path: an absent binary reaches the user as a
    /// failure naming the file and the search, in a shape the shell draws with its error icon, and is
    /// never mistaken for the "recording paused" balloon an idle tray raises beside it.
    #[test]
    fn a_missing_binary_reaches_the_user_as_an_error_and_never_as_a_chosen_state() {
        let missing = Missing { role: "recording", name: "windrec", searched: vec![PathBuf::from("D:/Windrecorder/bin")] };
        let notice = cannot_launch(missing);
        let Notice::Failure { title, body } = notice else {
            panic!("an absent binary is a failure, not an informational state change");
        };
        assert_eq!(title, "Cannot start recording");
        assert!(body.contains("windrec.exe"), "the message has to name the file: {body}");
        assert!(body.contains("D:/Windrecorder/bin"), "and show the search, not summarise it: {body}");
        assert!(body.contains("not recording being switched off"), "{body}");
        // The other half of the claim: this is not the sentence an idle recorder produces.
        let catalog = Catalog::load(Path::new("."), "en");
        let (paused_title, _) = menu::balloon(&Snapshot::idle("0.1.0".into()), &catalog);
        assert_ne!(title, paused_title, "the two states cannot share a balloon title");
        for forbidden in ["python", "record_screen", "streamlit", "webui"] {
            assert!(!format!("{title} {body}").contains(forbidden), "{forbidden} is back: {body}");
        }
    }

    #[test]
    fn a_lock_is_live_only_if_its_owner_pid_is_running() {
        // The whole interpretation the menu and `doctor` depend on, asserted on the three shapes a
        // `cache/locks` directory actually contains.
        let dir = std::env::temp_dir().join(format!("windsvc-supervisor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("LOCK_FILE_RECORD.MD");

        assert!(matches!(lock_state(&lock), LockState::Free));
        std::fs::write(&lock, "4000000").unwrap();
        assert!(matches!(lock_state(&lock), LockState::HeldBy { pid: 4000000, alive: false }));
        let child = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ping is on every Windows install");
        std::fs::write(&lock, child.id().to_string()).unwrap();
        assert!(matches!(lock_state(&lock), LockState::HeldBy { pid, alive: true } if pid == child.id()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_notice_keeps_both_the_command_and_the_os_error() {
        let notices = vec![Notice::Failure {
            title: "Cannot start recording".to_string(),
            body: "windrec.exe loop --root X\ncould not start\nos error 2".to_string(),
        }];
        let Notice::Failure { title, body } = &notices[0] else { panic!("expected a failure") };
        assert!(body.contains("loop --root") && body.contains("os error 2"), "{body}");
        assert!(!title.is_empty());
    }

    #[test]
    fn every_thing_the_tray_can_get_wrong_points_at_a_log() {
        assert!(LOG_HINT.contains("log"), "a hard stop must tell the user where to look");
    }

    #[test]
    fn a_shell_failure_says_what_it_tried_to_open() {
        let error = open_externally("Z:\\definitely-not-here\\RELEASE-NOTES.txt", Path::new(".")).expect_err("nothing can open this");
        assert!(error.contains("Z:\\definitely-not-here\\RELEASE-NOTES.txt"), "{error}");
    }

    /// What "See what's new" answers when the install carries nothing it could open: a refusal that
    /// names both files it looked for, and — load-bearing — launches nothing. The success arm is
    /// deliberately *not* exercised here: opening a real changelog starts the user's editor, which
    /// is a fact to be proven once on a standalone install by hand, not a side effect to inflict on
    /// every `cargo test`. What is asserted instead is the whole of the tray's remaining release
    /// promise: every row either does the real thing or says, in words, that it cannot.
    #[test]
    fn a_changelog_row_with_nothing_behind_it_refuses_by_name_and_opens_nothing() {
        let dir = std::env::temp_dir().join(format!("windsvc-changelog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), "{}").unwrap();
        let layout = Layout::from_config(&Config::load(&dir).unwrap());
        assert!(layout.changelog_target().is_none(), "a scratch root carries neither changelog file");
        let notices = changelog_notices(&layout);
        assert_eq!(notices.len(), 1, "one honest refusal, and no shell call was made");
        let Notice::Failure { title, body } = &notices[0] else { panic!("an absent file is a failure, not an informational notice") };
        assert_eq!(title, "No changelog in this install");
        assert!(body.contains("RELEASE-NOTES.txt") && body.contains("CHANGELOG.md"), "{body}");
        assert!(body.contains(&layout.root.display().to_string()), "the refusal names this install, not a generic file: {body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The decision `run_startup_migration` hands `boot`, asserted without an install or a built
    /// `windsetup.exe` (which `cargo test` does not compile): exit 0 is the only "may I start". A
    /// blocker that exits non-zero must refuse, and its stderr must survive into the reason so the user
    /// reads the migration's own words rather than a bare "it failed" — that message is the difference
    /// between this fix being actionable and it being another silent tray.
    #[test]
    fn only_a_clean_migration_exit_lets_the_tray_start() {
        let spawn = Spawn {
            program: PathBuf::from("D:/Windrecorder/bin/windsetup.exe"),
            args: vec!["migrate".into(), "--root".into(), "D:/Windrecorder".into()],
        };
        assert!(classify_migration_exit(&spawn, Some(0), b"", b"nothing to do").is_ok(), "a no-op migration must not block boot");
        let refused = classify_migration_exit(&spawn, Some(1), b"2 step(s) are blocked and were left for a human: index-schema: locked", b"")
            .expect_err("a blocked migration must not let the tray record onto the tree");
        assert!(refused.contains("will not record onto an unmigrated install"), "{refused}");
        assert!(refused.contains("left for a human"), "the migration's own explanation must survive: {refused}");
        assert!(refused.contains("migrate --root"), "and it must say the command to re-run: {refused}");
        // A killed process (no exit code) is as much a refusal as a non-zero one.
        assert!(classify_migration_exit(&spawn, None, b"", b"").is_err(), "no exit code is not a success");
    }

    /// "Has this install ever been initialised" is one file, and it has to be *that* file. `init`
    /// seeds `userdata/config_user.json` as its last act and never overwrites one, so its absence is
    /// the whole of "never laid out" — and its presence is why a second double-click runs no `init`
    /// at all. `userdata/` itself is not the signal, because the recorder half-creates folders: a
    /// tree can hold `userdata/db` and still have no config, no `result_*` folders and nothing for
    /// `windsetup doctor` to read.
    #[test]
    fn a_never_initialised_install_is_the_one_with_no_seeded_config() {
        let dir = std::env::temp_dir().join(format!("windsvc-firstrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let user_config = dir.join("userdata").join(CONFIG_USER_FILE);
        assert!(first_run_pending(&user_config), "nothing on disk means first run");

        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        assert!(first_run_pending(&user_config), "a bare userdata folder is not an install — that is what the recorder's own half-creation leaves behind");

        std::fs::write(&user_config, "{}").unwrap();
        assert!(!first_run_pending(&user_config), "a seeded config means the layout ran");

        // An install whose config exists but will not parse is NOT a first run. Overwriting somebody's
        // damaged settings quietly is worse than a tray that refuses to boot and names the file, and
        // `is_file` is the only check that keeps those two apart — a parse-based test would treat the
        // broken file as absent and seed over it.
        std::fs::write(&user_config, "{ not json").unwrap();
        assert!(!first_run_pending(&user_config), "a broken config is someone's data, not an absent one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same policy as the migration's, asserted the same way: `init` exiting non-zero is a
    /// refusal, not a warning, and its own words have to reach the balloon. A tray that started
    /// recording onto a tree whose folders were half-created is the failure this closes, so a soft
    /// pass here would undo it.
    #[test]
    fn only_a_clean_init_exit_lets_the_tray_start() {
        let spawn = Spawn {
            program: PathBuf::from("D:/Windrecorder/bin/windsetup.exe"),
            args: vec!["init".into(), "--root".into(), "D:/Windrecorder".into()],
        };
        assert!(classify_init_exit(&spawn, Some(0), b"", b"created 16 slot(s)").is_ok(), "a layout that ran must not block boot");
        let refused = classify_init_exit(&spawn, Some(1), b"cache/logs: access denied (os error 5)", b"")
            .expect_err("a failed layout must not let the tray record onto the tree");
        assert!(refused.contains("could not lay out"), "{refused}");
        assert!(refused.contains("os error 5"), "init's own explanation must survive: {refused}");
        assert!(refused.contains("init --root"), "and it must say the command to re-run: {refused}");
        assert!(classify_init_exit(&spawn, None, b"", b"").is_err(), "no exit code is not a success");
        // The two refusals must not be interchangeable, or the message sends the user to the wrong
        // command: re-running `migrate` cannot help an install whose folders were never created.
        let migrate_refused = classify_migration_exit(&spawn, Some(1), b"blocked", b"").expect_err("shared shape, different words");
        assert!(!refused.contains("unmigrated"), "the layout's message must not mention migration: {refused}");
        assert!(!migrate_refused.contains("could not lay out"), "and the migration's must not claim to be the layout: {migrate_refused}");
    }

    /// What a double-click on the icon means, as a decision pure enough to test without a window: the
    /// real method would spawn `winduiweb.exe`, and `cargo test` builds no binaries. Both arms are
    /// actions, and the enum has no third one — which is the assertion that carries. The bug this pins
    /// shut is the old `open_interface`, answering *every* state with a Failure balloon, so the most
    /// common gesture on a tray icon was a guaranteed error message. An already-open window is not one:
    /// it is the window to raise, which is the only way back from a close that hid it to the tray.
    #[test]
    fn double_clicking_the_icon_has_two_answers_and_neither_is_a_failure() {
        assert_eq!(double_click_action(false), DoubleClick::Start, "a closed window is opened by the click that used to complain");
        assert_eq!(double_click_action(true), DoubleClick::Raise, "an open one is brought forward, not toggled shut");
    }

    /// The tray's 🚩 now files a picture, not just a timestamp. Run the exact data path `flag_now`
    /// calls against a scratch root and read the row back: the `thumbnail` column must decode as a real
    /// JPEG at the index's own scale. On a session with no desktop the grab is refused and the
    /// flag is still filed picture-less — the documented fallback, never a lost bookmark.
    /// The ignored half of the changelog evidence: a real click on "See what's new" against a real
    /// standalone payload, asserting the shell accepts the install's own `RELEASE-NOTES.txt` and
    /// the tray raises no notice. Run by hand (`cargo test -p windsvc -- --ignored`) with
    /// `WINDSVC_EVIDENCE_ROOT` pointing at an unpacked payload, because it does exactly what the
    /// menu item does — opens the user's default text handler — and that side effect has no place
    /// in a normal test run. No environment variable means the row is not being proven, which is
    /// not a failure; the refusal path above is the half every run checks.
    #[test]
    #[ignore = "opens the desktop's text handler for a real file; run with WINDSVC_EVIDENCE_ROOT set"]
    fn clicking_see_whats_new_opens_the_release_notes_that_shipped_in_the_zip() {
        let Ok(raw) = std::env::var("WINDSVC_EVIDENCE_ROOT") else {
            eprintln!("SKIPPED: WINDSVC_EVIDENCE_ROOT is not set");
            return;
        };
        let root = PathBuf::from(raw);
        let layout = Layout::from_config(&Config::load(&root).expect("the evidence root must be an install"));
        let target = layout.changelog_target().expect("the payload must carry its release notes");
        assert_eq!(target, layout.release_notes, "a standalone install resolves the row to the notes in its own root");
        assert!(target.is_file(), "{target:?} must exist before the click is proven");
        let notices = changelog_notices(&layout);
        assert!(notices.is_empty(), "opening a file the install carries must raise no notice: {notices:?}");
        eprintln!("OPENED {} with no notice — the shell accepted it", target.display());
    }

    /// The whole point of the tray reading the settings again: the switch decides in both directions,
    /// and a state that already agrees does nothing at all.
    #[test]
    fn the_bridge_switch_decides_in_both_directions_and_not_otherwise() {
        assert_eq!(reconcile(true, false), BridgeAction::Start, "asked for and absent: start it");
        assert_eq!(reconcile(false, true), BridgeAction::Stop, "no longer asked for: stop it");
        assert_eq!(reconcile(true, true), BridgeAction::None, "asked for and running: leave one server alone");
        assert_eq!(reconcile(false, false), BridgeAction::None, "off and absent: nothing to announce");
    }

    /// And the flag is read from the same key the AI page writes, so the two ends cannot drift into the
    /// state where a user ticks a box and nothing answers.
    #[test]
    fn the_bridge_wanted_flag_is_the_key_the_settings_page_writes() {
        let dir = std::env::temp_dir().join(format!("windsvc-bridge-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), b"{}").unwrap();
        let off = wind_base::config::Config::load(&dir).expect("a bare install loads");
        assert!(
            !native::bridge_enabled(&off),
            "absent means off, which is what a remote read of somebody's screen history has to default to"
        );

        std::fs::write(dir.join("config_src/config_default.json"), b"{\"enable_mcp_server\": true}").unwrap();
        let on = wind_base::config::Config::load(&dir).expect("the edited file loads");
        assert!(native::bridge_enabled(&on), "the key the AI page writes is the key the tray reads");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tray_flag_carries_a_real_screen_thumbnail() {
        let dir = std::env::temp_dir().join(format!("windsvc-flag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config::load(&dir).expect("an empty directory loads as an install with only defaults");
        let path = config.flag_note_path();

        let stamp = append_flag_with_thumbnail(&config, &path).expect("the flag is filed even when nothing can be grabbed");

        let rows = wind_base::csv::read_rows(&path, true).expect("the table is readable");
        assert_eq!(rows.len(), 1, "one flag appended");
        assert_eq!(rows[0][1], stamp, "and its datetime is the instant the tray took");
        assert_eq!(rows[0][2], wind_notes::flag::NOTE_PLACEHOLDER, "the note starts as the placeholder; the editor owns the text");

        let thumbnail = &rows[0][0];
        match wind_notes::capture::thumbnail_size(thumbnail) {
            Some((width, height)) => {
                eprintln!("tray flag thumbnail decoded as {width}x{height} JPEG from {} base64 chars", thumbnail.len());
                assert!(!thumbnail.is_empty(), "a decoded thumbnail cannot be an empty cell");
                assert_eq!(
                    width,
                    config.thumbnail_width(),
                    "the same thumbnail scale the index and the day view use"
                );
                assert!(height > 0, "{width}x{height}");
            }
            None => {
                eprintln!("no desktop available to the tray; flag filed picture-less as the fallback");
                assert!(thumbnail.is_empty(), "no grab means no bytes, not a corrupt thumbnail: {thumbnail:?}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
