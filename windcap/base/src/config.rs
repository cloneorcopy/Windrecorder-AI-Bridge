//! Reads the same two files the Python app does: `config_src/config_default.json` overlaid by
//! `userdata/config_user.json`. Nothing is re-declared here, so a key the Python settings page
//! writes is honoured by the native recorder without any change on this side.
//!
//! Which `config_default.json` — the payload's, or the one an overlay install still keeps under
//! `windrecorder/` — is decided by [`crate::install`], in one place, together with the copy
//! compiled into this binary that makes a missing on-disk file survivable instead of fatal.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::install::{self, DefaultsSource};

#[derive(Debug, Clone)]
pub struct Config {
    root: PathBuf,
    values: BTreeMap<String, Value>,
    /// Which `config_default.json` the base layer came from — on disk, or compiled in.
    defaults: DefaultsSource,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(PathBuf, std::io::Error),
    NotAnObject(PathBuf),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(p, e) => write!(f, "cannot read {}: {e}", p.display()),
            ConfigError::NotAnObject(p) => write!(f, "{} is not a JSON object", p.display()),
        }
    }
}

/// Read one on-disk layer into `values`, skipping it silently when the file is absent.
///
/// "Absent is not an error" is upstream's behaviour and it is load-bearing: an install with no
/// `userdata/config_user.json` yet runs on pure defaults, and the first run that writes a setting
/// creates the file. A layer that *exists* and cannot be parsed is a different matter and does
/// fail — a corrupt settings file must not be quietly ignored.
fn merge_layer(values: &mut BTreeMap<String, Value>, path: &Path) -> Result<(), ConfigError> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
    merge_json(values, &text, path)
}

fn merge_json(values: &mut BTreeMap<String, Value>, text: &str, path: &Path) -> Result<(), ConfigError> {
    let parsed: Value = serde_json::from_str(text)
        .map_err(|e| ConfigError::Io(path.to_path_buf(), std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    let map = parsed.as_object().ok_or_else(|| ConfigError::NotAnObject(path.to_path_buf()))?;
    for (k, v) in map {
        values.insert(k.clone(), v.clone());
    }
    Ok(())
}

impl Config {
    /// `root` is the installation directory — see [`crate::install`] for what makes one.
    pub fn load(root: &Path) -> Result<Config, ConfigError> {
        let mut values = BTreeMap::new();
        // The default file is the contract; the user file only ever overrides keys present in it.
        let defaults = install::defaults_source(root);
        match &defaults {
            DefaultsSource::Embedded => {
                // No on-disk settings at all. The compiled-in copy keeps this far: a binary that
                // cannot find its defaults used to be a binary that could not be installed, which
                // is the shape of the failure this path exists to remove.
                merge_json(&mut values, install::embedded_defaults(), Path::new("embedded config_default.json"))?;
            }
            DefaultsSource::Payload(path) | DefaultsSource::Legacy(path) => {
                merge_layer(&mut values, path)?;
            }
        }
        merge_layer(&mut values, &root.join(install::USERDATA_DIR).join("config_user.json"))?;
        Ok(Config { root: root.to_path_buf(), values, defaults })
    }

    /// The installation directory: the folder holding `config_src/`, `userdata/` and `cache/`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the factory settings behind this config actually came from.
    ///
    /// `windsetup doctor` prints it, because "your settings are being read from the copy your
    /// upgrade left behind" is a fact about the install that only this value can tell you.
    pub fn defaults_source(&self) -> &DefaultsSource {
        &self.defaults
    }

    /// The `config_default.json` in effect, when there is one on disk.
    pub fn defaults_path(&self) -> Option<&Path> {
        match &self.defaults {
            DefaultsSource::Payload(path) | DefaultsSource::Legacy(path) => Some(path),
            DefaultsSource::Embedded => None,
        }
    }

    /// The settings directory, honouring `config_src_dir` and the two install layouts.
    ///
    /// Order matters and is the whole point of the function. A `config_src_dir` the user genuinely
    /// typed is respected while it names a directory that exists — that is how an install with its
    /// settings on a share, or wherever else, keeps working. The two values this project has ever
    /// *shipped as the default* are not such a choice (see [`install::is_default_src_literal`]):
    /// `Config::save` snapshots the merged config, so an upgraded overlay's user file still says
    /// `windrecorder\\config_src` and letting that win would keep the install reading the settings
    /// layer its own upgrade replaced. Failing an override, [`install`] decides, which is where
    /// the "both layouts present, the payload one wins" rule lives. Only if the root has neither
    /// does this fall back to the payload spelling, so callers always have a real path to name.
    pub fn config_src_dir(&self) -> PathBuf {
        if let Some(configured) = self.values.get(install::CONFIG_SRC_KEY).and_then(Value::as_str) {
            if !configured.trim().is_empty() && !install::is_default_src_literal(configured) {
                let candidate = install::confined_join(&self.root, configured);
                if candidate.is_dir() {
                    return candidate;
                }
            }
        }
        // Read from the same source the defaults layer came from, so the keys and the lookup
        // tables they index into can never be resolved out of two different directories.
        match self.defaults.config_src() {
            Some(dir) => dir.to_path_buf(),
            None => self.root.join(install::CONFIG_SRC),
        }
    }
    /// A named file inside the settings directory (`similar_CN_characters.txt`, a preset table).
    pub fn config_src_file(&self, name: &str) -> PathBuf {
        self.config_src_dir().join(name)
    }

    pub fn str_or(&self, key: &str, default: &str) -> String {
        match self.values.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) => n.to_string(),
            _ => default.to_string(),
        }
    }

    pub fn i64_or(&self, key: &str, default: i64) -> i64 {
        match self.values.get(key) {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(default),
            Some(Value::String(s)) => s.parse().unwrap_or(default),
            Some(Value::Bool(b)) => i64::from(*b),
            _ => default,
        }
    }

    pub fn bool_or(&self, key: &str, default: bool) -> bool {
        match self.values.get(key) {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => matches!(s.as_str(), "true" | "1" | "yes"),
            _ => default,
        }
    }

    pub fn str_list(&self, key: &str) -> Vec<String> {
        match self.values.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The value exactly as the JSON holds it, for a key whose shape is not one of the accessors above —
    /// an argv list (`ocr_engine_command`) that may also be written as a single program string.
    pub fn raw(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// `userdata_dir` + `db_path`, matching `config.db_path_ud`.
    pub fn db_dir(&self) -> PathBuf {
        self.root
            .join(self.str_or("userdata_dir", "userdata"))
            .join(self.str_or("db_path", "db"))
    }

    pub fn cache_screenshot_dir(&self) -> PathBuf {
        self.root.join("cache_screenshot")
    }

    /// Where the **Windows** OCR engine lives, whether or not it is the engine in use.
    ///
    /// The path is [`crate::ocr::windows_exe`]'s, so the settings page's list, the recorder's spawn and
    /// `windsetup check-engines` name one file. Nothing that *runs* OCR should reach for this: the engine
    /// a user selects in `ocr_engine` is resolved by [`crate::ocr::Engine::select`].
    pub fn ocr_exe(&self) -> PathBuf {
        // Python invoked this relatively with cwd == install root, so do the same.
        crate::ocr::windows_exe(&self.root)
    }

    /// A value the app stores as a fraction (`0.7` for a similarity threshold) must not be read
    /// back through `i64_or`, which silently falls through to the default for any non-integer.
    pub fn f64_or(&self, key: &str, default: f64) -> f64 {
        match self.values.get(key) {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
            Some(Value::String(s)) => s.parse().unwrap_or(default),
            Some(Value::Bool(b)) => *b as u8 as f64,
            _ => default,
        }
    }

    pub fn i64_list(&self, key: &str) -> Vec<i64> {
        match self.values.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::Number(n) => n.as_i64(),
                    Value::String(s) => s.parse().ok(),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn contains_str(&self, key: &str, needle: &str) -> bool {
        self.str_list(key).iter().any(|v| v == needle)
    }

    // --- On-disk layout, all of it derived from the same keys the Python `config` object builds. ---

    pub fn user_name(&self) -> String {
        self.str_or("user_name", "default")
    }

    pub fn userdata_dir(&self) -> PathBuf {
        self.root.join(self.str_or("userdata_dir", "userdata"))
    }

    /// `userdata/videos`, the root of the monthly video folders the UI browses.
    pub fn videos_dir(&self) -> PathBuf {
        self.userdata_dir().join(self.str_or("record_videos_dir", "videos"))
    }

    /// `userdata/videos/2026-09`, the folder a month's segments live in.
    pub fn month_videos_dir(&self, year: i64, month: u32) -> PathBuf {
        self.videos_dir().join(format!("{year:04}-{month:02}"))
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.join(self.str_or("log_dir", "cache\\logs"))
    }

    pub fn win_title_dir(&self) -> PathBuf {
        self.root.join(self.str_or("win_title_dir", "cache\\win_title"))
    }

    pub fn iframe_dir(&self) -> PathBuf {
        self.root.join(self.str_or("iframe_dir", "cache\\i_frames"))
    }

    pub fn lock_dir(&self) -> PathBuf {
        self.root.join(self.str_or("lock_file_dir", "cache\\locks"))
    }

    /// A directory lock: the file is the directory itself, which is why removal must be a `rmdir`.
    pub fn maintain_lock_dir(&self) -> PathBuf {
        self.lock_dir()
            .join(self.str_or("maintain_lock_subdir", "LOCK_MAINTAIN"))
    }

    /// Is the idle maintenance pass rewriting the index right now?
    ///
    /// Every reader asks this before refreshing its `_TEMP_READ.db` copy, and the answer comes from
    /// the `PID` inside [`Self::maintain_lock_dir`] rather than from the directory's existence — see
    /// [`crate::fslock::directory_lock_claimed`] for why the existence is not the claim. Asking the
    /// wrong one of the two is how a window keeps showing last week's data after the recorder
    /// committed rows all evening.
    pub fn maintain_lock_claimed(&self) -> bool {
        crate::fslock::directory_lock_claimed(&self.maintain_lock_dir())
    }

    /// Where a running pass publishes how far it has got: `cache\locks\LOCK_MAINTAIN\PROGRESS.MD`.
    ///
    /// Inside the lock directory rather than beside it, because the file describes the pass that owns
    /// that lock and nothing else — a reader that found one of the two has found the other. It is left
    /// behind when the pass ends on purpose: the window shows what the last pass did until the next one
    /// replaces it, and [`crate::maintain::Pass::is_running`] is what stops a finished file reading as
    /// work still underway.
    pub fn maintain_progress_path(&self) -> PathBuf {
        self.maintain_lock_dir().join("PROGRESS.MD")
    }

    pub fn record_lock_path(&self) -> PathBuf {
        self.lock_dir().join(self.str_or("record_lock_name", "LOCK_FILE_RECORD.MD"))
    }

    /// `cache\locks\RECORD_STATE.MD` — what the capture loop did on its last tick, in one word.
    ///
    /// Beside the record lock rather than inside it, because it is not a lock: nobody claims it, and a
    /// stale one is only ever a missing answer (`read_capture` returns `None` for anything it cannot
    /// parse), never a reason to refuse to start.
    pub fn record_state_path(&self) -> PathBuf {
        self.lock_dir().join("RECORD_STATE.MD")
    }

    pub fn tray_lock_path(&self) -> PathBuf {
        self.lock_dir().join(self.str_or("tray_lock_name", "LOCK_FILE_TRAY.MD"))
    }

    /// Where a hiding window looks for the tray's "come back" request. See [`crate::fslock::request_show`].
    pub fn window_show_signal_path(&self) -> PathBuf {
        self.lock_dir().join("WINDOW_SHOW.MD")
    }

    /// Where the interface leaves a "start the deferred pass now" request for the recorder.
    ///
    /// A file in `cache/locks` rather than a window message because the recorder is not listening for
    /// messages — it wakes on its own capture tick — and this is the same take-it-once shape the
    /// tray's raise request already uses, so nothing new has to be kept in sync.
    pub fn maintain_start_signal_path(&self) -> PathBuf {
        self.lock_dir().join("MAINTAIN_START.MD")
    }

    /// Where the interface leaves a "stop the deferred pass" request for `windmaint` itself.
    ///
    /// The pass consumes this, not the recorder: `windrec` owns a console, so the tray's
    /// `AttachConsole`-and-break trick does not work from here, and a pass that stops itself between
    /// two work items is the same latency without the Win32 hazard.
    pub fn maintain_stop_signal_path(&self) -> PathBuf {
        self.lock_dir().join("MAINTAIN_STOP.MD")
    }

    /// Has somebody asked the running pass to stop?
    ///
    /// Read, not taken. One pass is several processes — `windmaint` and the `wind-reindex` it spawns —
    /// and a request the first of them consumes is invisible to the rest, which would let the step
    /// after the one that noticed carry on doing the work that was just cancelled. The pass that
    /// honours it clears it, and a recorder with no pass running clears a request nobody owes.
    pub fn maintain_stop_requested(&self) -> bool {
        self.maintain_stop_signal_path().exists()
    }

    /// Remove a stop request that has been honoured (or that nobody was around to honour).
    pub fn clear_maintain_stop(&self) {
        let _ = std::fs::remove_file(self.maintain_stop_signal_path());
    }

    /// What the window's close button does: hide to the tray, or end the process.
    ///
    /// On unless the user says otherwise, because the thing the tray is for is recording while nobody is
    /// looking at a window — and the window dying on a click made the whole product look quit.
    pub fn close_window_to_tray(&self) -> bool {
        self.bool_or("close_window_to_tray", true)
    }

    /// Is the tray alive to receive a hidden window? A window that hides itself with no tray running has
    /// no way back, so it must be allowed to close.
    pub fn tray_is_running(&self) -> bool {
        matches!(
            crate::fslock::lock_state(&self.tray_lock_path()),
            crate::fslock::LockState::HeldBy { alive: true, .. } | crate::fslock::LockState::Owned
        )
    }

    /// The close button's whole answer, for either window: hide if the user asked for a background mode
    /// *and* somebody is left who can bring the window back.
    ///
    /// One function because the egui window and the HTML window are the same product's front door, and two
    /// copies of a two-question rule is how one of them ends up stranding a user with an invisible window.
    pub fn window_hides_on_close(&self) -> bool {
        self.close_window_to_tray() && self.tray_is_running()
    }

    pub fn last_idle_maintain_path(&self) -> PathBuf {
        self.root.join(self.str_or("last_idle_maintain_file_path", "cache\\LAST_IDLE_MAINTAIN.MD"))
    }

    /// The two clock times between which the deferred work may run, or `None` when this install has
    /// not named a window.
    ///
    /// The recorder spends the whole day doing the one thing that cannot be redone later — catching
    /// the pixels. Everything else it used to do inside the same tick (reading text off a frame,
    /// drawing its preview, dropping a duplicate) can be recomputed from the JPEG that was written,
    /// so this window is where that work belongs. Both keys must parse for a window to exist: one
    /// end without the other is a half-sentence, and inventing the missing half would run heavy disk
    /// work at an hour nobody chose. A config with no window keeps upstream's rule — the pass fires
    /// after enough idle minutes — so `None` is a working state, not a broken one.
    pub fn maintain_window(&self) -> Option<MaintainWindow> {
        let start = parse_clock_time(&self.str_or("maintain_window_start", ""))?;
        let end = parse_clock_time(&self.str_or("maintain_window_end", ""))?;
        Some(MaintainWindow { start_minutes: start, end_minutes: end })
    }

    pub fn flag_note_path(&self) -> PathBuf {
        self.userdata_dir().join(self.str_or("flag_mark_note_filename", "flag_mark_note.csv"))
    }

    pub fn search_history_path(&self) -> PathBuf {
        self.userdata_dir().join(self.str_or("search_history_note_filename", "search_history.csv"))
    }

    /// The generated-image folders (`result_timeline`, `result_lightbox`, …) live under `userdata`.
    pub fn result_dir(&self, key: &str, default: &str) -> PathBuf {
        self.userdata_dir().join(self.str_or(key, default))
    }

    /// Which product day an instant belongs to, in minutes past midnight. One reader, one default.
    ///
    /// Every day-shaped question in the workspace — `summary::keys::day_of`, `notes::DaySpan::product_day`,
    /// `store::aggregate::histogram`, `mcp::stream::day_label`, `wind_ui::Settings::load` — takes the number
    /// as a parameter, and this is the only answer any of them may be handed. A missing key therefore means
    /// 03:00 in every crate or in none of them; there is no third state in which one binary guesses
    /// differently, and `tests::the_product_day_key_is_read_in_exactly_one_place` is the guard on that
    /// rather than a comment asking future readers to be careful.
    pub fn day_begin_minutes(&self) -> i64 {
        self.i64_or("day_begin_minutes", 180)
    }

    /// Minutes the screen must sit idle before the recorder launches the maintenance pass.
    ///
    /// The whole of "when does the post-processing run": `windrec` writes the timestamp of a launch to
    /// [`Config::last_idle_maintain_path`] and refuses to spawn another until this much time has passed
    /// since it. Nothing schedules the pass otherwise — there is no service, no timer and no resident
    /// helper, by design — so this number is the only thing standing between a user and an idle pass that
    /// wakes the disk up every few minutes, and the only thing standing between them and a night's slices
    /// that never became video.
    ///
    /// **0 switches the pass off**, and that is a value rather than a clamp boundary: `windrec`'s
    /// `maintenance_is_due` says so by name. The ceiling is one day, because a gap longer than the
    /// machine's own working day is indistinguishable from off and reads as a broken recorder. The shipped
    /// 40 is the number `windrec` carried as a bare `i64_or` default before this row existed.
    pub fn idle_maintain_gap_minutes(&self) -> i64 {
        self.i64_or("idle_maintain_time_gap", 40).clamp(0, 1_440)
    }

    /// How many product days one idle summarising run may take on. Part of "how long the pass runs".
    ///
    /// The idle pass asks `windai summarize --pending N` for the N days with outstanding work rather than
    /// "everything unsummarised since the library began", because an idle window is borrowed time on a
    /// machine the user may come back to. The ceiling is `wind_ai::summarize::PENDING_SCAN_DAYS`, the
    /// horizon past which that command stops looking for days with work anyway: offering more here would
    /// be a slider whose top third does nothing. `wind_ui::record.rs` pins the two numbers agree.
    pub fn summary_pending_days_in_idle(&self) -> i64 {
        self.i64_or("summary_pending_days_in_idle", 2).clamp(1, 60)
    }

    /// How many stretches one idle summarising run may ask for. The other half of "how long".
    ///
    /// `windai summarize --limit N`: a day's worth of footage is dozens of stretches, and the pair of
    /// these two numbers is the whole budget of the pass, in requests. There is no ceiling in `windai` for
    /// `--limit` above zero, so 1 000 is this row's own guard against a value that cannot finish inside an
    /// idle window; the shipped 40 is what `windmaint` passed on the command line before it was a setting.
    pub fn summary_stretch_limit_in_idle(&self) -> i64 {
        self.i64_or("summary_stretch_limit_in_idle", 40).clamp(1, 1_000)
    }

    /// `enable_ai_extract_tag` — is the month/day tagger switched on at all.
    ///
    /// The defaults here are `wind_ai::settings::Settings::read`'s own, deliberately: this key has two
    /// readers (`windai`, which refuses to spend, and `windmaint`'s `ai_gate`, which refuses to spawn) and
    /// a page that writes it, and an absent key must not mean three different things to the three of them.
    /// `windui::ai::tests::the_idle_switches_default_where_windai_they_default` is the cross-crate pin.
    pub fn ai_extract_tag_enabled(&self) -> bool {
        self.bool_or("enable_ai_extract_tag", false)
    }

    /// `enable_ai_extract_tag_in_idle` — and is it allowed *during the idle pass*.
    ///
    /// Off, the tagger still works when a person runs it; it simply never runs on a machine the user
    /// believes is asleep. Read with `true` because that is what both other readers read an absent key as.
    pub fn ai_extract_tag_allowed_in_idle(&self) -> bool {
        self.bool_or("enable_ai_extract_tag_in_idle", true)
    }

    /// `enable_ai_summary_in_idle` — may the summarising pass send screen text while the machine idles.
    ///
    /// The one switch for the step that ships a day's captured text to `open_ai_base_url`; the shipped
    /// key is a placeholder and `windai` refuses to send a byte while it is one, so there is deliberately
    /// no second master switch beside it. Read by `windmaint`'s `summaries_gate` and written by the AI
    /// page, both through here.
    pub fn ai_summary_allowed_in_idle(&self) -> bool {
        self.bool_or("enable_ai_summary_in_idle", true)
    }

    /// How long a stretch with nothing captured can be and still count as the same session at the
    /// machine — the one answer behind every "hours" figure the index reports.
    ///
    /// Two recorder settings bound it, and neither of them is the chart's bucket. A segment runs
    /// `record_seconds` before it rolls, and the recorder only notices a still screen after
    /// `screentime_not_change_to_pause_record` minutes (both read by the recording plan in
    /// `windrec::recorder`), so a gap up to the longer of the two is a stretch this index cannot
    /// tell apart from working: the rows say nothing was *new*, not that nobody was there. Past that
    /// the time is cut at this same length rather than dropped, which is the conservative half of a
    /// guess the rows still cannot settle. On the shipped settings that is 900 s. The clamp is for
    /// the two ways installs differ from shipped: a fixture recording 60 s segments must not make
    /// every pause read as an hour of absence, and a user who set `record_seconds` to an hour must
    /// not have a nap counted as work.
    ///
    /// It is derived, and it is *said* to be derived: `Seconds per segment` and `Pause after a frozen
    /// screen` on the Recording page both carry this arithmetic in their help line, because a number
    /// the Statistics page labels "hours" that moves when somebody edits a differently-named setting is
    /// a label that promised more than the measurement delivers. It is deliberately not a row of its
    /// own — a control that writes a key no engine reads is the defect this branch keeps removing, and
    /// an editable one would let a user set a gap longer than their own segment length, which is the
    /// one thing this figure exists not to do.
    pub fn presence_gap_secs(&self) -> i64 {
        let segment = self.i64_or("record_seconds", 900);
        let pause = self.i64_or("screentime_not_change_to_pause_record", 5) * 60;
        segment.max(pause).clamp(300, 7200)
    }

    /// The width the stored preview is made at — one answer for the recorder, the back-indexer, the
    /// flag's picture and the settings page, which each used to carry their own `70` fallback.
    ///
    /// [`crate::image::CARD_PREVIEW_FLOOR`], because that is the narrowest picture either window can
    /// draw without *stretching* it: the HTML window paints a result card's picture about 450 CSS
    /// pixels across, and a source narrower than the box is exactly what reads as mush — which is why
    /// upstream's 70 px stamp cannot survive in this fork. It is also the settings row's own ceiling
    /// raised one notch, so the knob still has somewhere to go.
    ///
    /// The cost is honest and disclosed: 7-12 KB a row at the shipped quality for a 1080p grab, up to
    /// 21 KB for a tall multi-monitor one. A five-year library is hundreds of thousands of rows, which
    /// is why the row is in the settings page at all rather than a constant nobody can argue with.
    ///
    /// This key only ever affects rows written from now on; [`crate::config`] cannot redraw a user's
    /// history, but `windmaint previews` can, and the idle pass runs it. Clicking a row still reads the
    /// original frame (`wind_ui::backend::frame`), which is the answer when a preview is not enough.
    pub fn thumbnail_width(&self) -> u32 {
        self.i64_or("thumbnail_generation_size_width", crate::image::CARD_PREVIEW_FLOOR as i64)
            .clamp(8, 4096) as u32
    }

    /// JPEG quality of that preview. 70 rather than upstream's 30: at 30 the text on a screen is the
    /// first thing to go, and a preview whose whole job is to let a person recognise their own monitor
    /// at a glance cannot be encoded as if recognition were someone else's problem.
    pub fn thumbnail_quality(&self) -> u8 {
        self.i64_or("thumbnail_generation_jpg_quality", 70).clamp(1, 100) as u8
    }

    /// Is this install drawing its cards from a picture narrower than the card?
    ///
    /// The settings page asks, because a row that changes only the future looks broken to a user whose
    /// entire history is already on disk at 70 px — and the answer has to be said out loud there rather
    /// than left as a number that appears to do nothing.
    pub fn preview_is_a_stamp(&self) -> bool {
        self.thumbnail_width() < crate::image::CARD_PREVIEW_FLOOR
    }

    /// `ffmpeg` is resolved exactly as `config.py` does: bundled inside the venv for a release
    /// install, otherwise whatever is on `PATH`.
    pub fn ffmpeg_path(&self) -> PathBuf {
        let name = if self.bool_or("release_ver", false) {
            PathBuf::from(".venv").join("ffmpeg.exe")
        } else {
            PathBuf::from("ffmpeg.exe")
        };
        let local = self.root.join(&name);
        if local.exists() {
            local
        } else {
            PathBuf::from("ffmpeg")
        }
    }

    // --- Writing back. -----------------------------------------------------------------------

    /// Stage a change in memory. Nothing hits the disk until [`Config::save`].
    pub fn set(&mut self, key: &str, value: Value) {
        self.values.insert(key.to_string(), value);
    }

    /// Rewrite `userdata/config_user.json` with the merged view, the way
    /// `config.set_and_save_config` does: two-space indent, keys sorted, non-ASCII left as itself.
    ///
    /// Writing the *merged* map rather than only the overrides is deliberate — the Python loader
    /// reconciles the user file against the defaults on every read either way, so a file that is a
    /// complete snapshot is the format it already produces today.
    pub fn save(&self) -> Result<PathBuf, ConfigError> {
        let path = self.userdata_dir().join("config_user.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::Io(parent.to_path_buf(), e))?;
        }
        let object = serde_json::Map::from_iter(self.values.iter().map(|(k, v)| (k.clone(), v.clone())));
        let text = serde_json::to_string_pretty(&Value::Object(object))
            .map_err(|e| ConfigError::Io(path.clone(), std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        // A torn write here costs the user every setting, so stage it beside the target and rename.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &text).map_err(|e| ConfigError::Io(tmp.clone(), e))?;
        std::fs::rename(&tmp, &path).map_err(|e| ConfigError::Io(path.clone(), e))?;
        Ok(path)
    }
}

/// The hours during which the deferred work is allowed to run, as minutes after midnight.
///
/// Held as minutes rather than as a `LocalParts` because a window is a rule about clock faces: it
/// means the same thing on every date, and the recorder asks "am I inside it?" many times per pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintainWindow {
    pub start_minutes: u32,
    pub end_minutes: u32,
}

impl MaintainWindow {
    /// Does this minute of the day fall inside the window?
    ///
    /// The end is exclusive. A pass told to stop at `06:00` must not still be encoding at `06:00`,
    /// because the alternative reading — "inclusive" — means the last thing it does runs over into
    /// the morning the window was carved out to avoid.
    ///
    /// `start > end` is the ordinary overnight case (`22:00`–`06:00`), not a mistake, so it wraps
    /// rather than failing to parse.
    pub fn contains(&self, minute_of_day: u32) -> bool {
        if self.start_minutes <= self.end_minutes {
            (self.start_minutes..self.end_minutes).contains(&minute_of_day)
        } else {
            minute_of_day >= self.start_minutes || minute_of_day < self.end_minutes
        }
    }

    /// `03:30-05:00`, for the one log line a user reads to check what they set.
    pub fn label(&self) -> String {
        format!("{}-{}", clock_text(self.start_minutes), clock_text(self.end_minutes))
    }
}

/// Minutes after midnight back into `HH:MM`.
fn clock_text(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Read a clock time written by a person: `03:30`, `3:30`, whitespace around it tolerated.
///
/// Deliberately unforgiving about everything else. `30 8` or `8am` or `25:00` returns `None`, which
/// the caller reads as "no window was set" — the safe direction, because guessing at a malformed
/// time would run a whole-library re-OCR at an hour nobody asked for. Public because the settings
/// page validates the box with exactly this rule, so the two cannot disagree about `3:5`.
pub fn parse_clock_time(text: &str) -> Option<u32> {
    let text = text.trim();
    let (hour, minute) = text.split_once(':')?;
    if hour.is_empty() || hour.len() > 2 || minute.len() != 2 {
        return None;
    }
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The install root as seen from a test binary: two levels up from `windcap/base`.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap()
    }

    fn defaults() -> Config {
        Config::load(&repo_root()).expect("the shipped config_default.json must parse")
    }

    /// The window is two clock times, and the overnight case is the one a person actually sets.
    #[test]
    fn a_maintenance_window_is_read_off_two_clock_times() {
        let dir = scratch("window");
        write(&dir.join("config_src/config_default.json"), r#"{"maintain_window_start": "22:00", "maintain_window_end": "06:00"}"#);

        let window = Config::load(&dir).unwrap().maintain_window().expect("both ends are spelled");
        assert_eq!((window.start_minutes, window.end_minutes), (1_320, 360));
        assert!(window.contains(22 * 60), "it opens at the minute it says");
        assert!(window.contains(23 * 60 + 59), "and it is still open one minute before midnight");
        assert!(window.contains(5 * 60 + 59), "it wraps into the next date");
        assert!(!window.contains(6 * 60), "and it is shut at its own end, not one minute after");
        assert!(!window.contains(12 * 60), "the afternoon is outside an overnight window");
        assert_eq!(window.label(), "22:00-06:00");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A same-minute window is shut, not open for the whole day: `05:00`-`05:00` reads as "no time
    /// was really chosen", and running a pass all day on that reading would be the worst possible
    /// interpretation of a typo.
    #[test]
    fn a_window_that_opens_and_closes_at_the_same_minute_is_never_open() {
        let dir = scratch("empty-window");
        write(&dir.join("config_src/config_default.json"), r#"{"maintain_window_start": "05:00", "maintain_window_end": "05:00"}"#);

        let window = Config::load(&dir).unwrap().maintain_window().expect("both ends parse");
        for minute in (0..1_440).step_by(7) {
            assert!(!window.contains(minute), "{minute} must not be inside an empty window");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One end without the other is not a window, and neither is anything that is not a clock time.
    /// Every one of these falls back to the idle rule rather than to a guessed hour.
    #[test]
    fn a_half_spelled_or_malformed_window_is_no_window_at_all() {
        for (start, end) in [
            ("03:30", ""),
            ("", "05:00"),
            ("", ""),
            ("8am", "5"),
            ("25:00", "05:00"),
            ("03:60", "05:00"),
            ("03:3", "05:00"),
        ] {
            let dir = scratch(&format!("bad-window-{}", start.len()));
            write(
                &dir.join("config_src/config_default.json"),
                &format!(r#"{{"maintain_window_start": "{start}", "maintain_window_end": "{end}"}}"#),
            );
            assert_eq!(
                Config::load(&dir).unwrap().maintain_window(),
                None,
                "start={start:?} end={end:?} is not a window"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A person typing `3:30` means half past three. Refusing that would make the setting look
    /// broken over something no reader cares about.
    #[test]
    fn a_one_digit_hour_is_still_a_clock_time() {
        assert_eq!(parse_clock_time("3:30"), Some(210));
        assert_eq!(parse_clock_time(" 03:30 "), Some(210));
        assert_eq!(parse_clock_time("00:00"), Some(0));
        assert_eq!(parse_clock_time("23:59"), Some(1_439));
    }

    /// The shipped install ships no window, so this change cannot move anyone's maintenance to a
    /// different hour without them asking for it.
    #[test]
    fn the_shipped_config_leaves_the_window_unset() {
        assert_eq!(defaults().maintain_window(), None, "the default stays on upstream's idle rule");
    }

    /// This is a compatibility test, not a unit test: if a key is renamed or a default changes in
    /// the Python tree, the native side has to notice here rather than in a user's broken install.
    #[test]
    fn the_real_default_config_parses_and_answers_with_the_shipped_values() {
        let c = defaults();
        assert_eq!(c.str_or("user_name", "?"), "default");
        assert_eq!(c.str_or("ocr_engine", "?"), "Windows.Media.Ocr.Cli");
        assert_eq!(c.str_or("record_mode", "?"), "screenshot_array");
        assert_eq!(c.i64_or("record_seconds", 0), 900);
        assert_eq!(c.i64_or("screenshot_interval_second", 0), 3);
        assert_eq!(c.day_begin_minutes(), 180);
        assert!(c.bool_or("index_reduce_same_content_at_different_time", false));
        assert!(c.str_list("exclude_words").contains(&"Windrecorder".to_string()));
    }

    /// The product day is one question and has one answer, and this is the test that keeps it that way.
    ///
    /// `day_begin_minutes` reaches the merged map through exactly one reader, so a config that predates
    /// the key means 03:00 in `windmcp`, `windsummary`, `windnotes`, `windstore` and both windows — or
    /// means nothing anywhere. A second `i64_or("day_begin_minutes", …)` with its own fallback is
    /// invisible until a day's rows land in two different files, which is why the guard reads the source
    /// rather than trusting a comment: the failure it prevents cannot be observed from any one crate.
    #[test]
    fn the_product_day_key_is_read_in_exactly_one_place() {
        let reads = source_lines_reading("day_begin_minutes");
        assert_eq!(
            reads,
            vec!["windcap/base/src/config.rs".to_string()],
            "the product day is read by `Config::day_begin_minutes` and by nobody else; a second reader \
             with its own default is how one install answers the same question two ways: {reads:?}"
        );
    }

    /// Every idle-pass number is declared, so a fresh install's file *says* what its post-processing
    /// schedule is instead of leaving it in a `default` argument three crates deep — and the accessors
    /// agree with the file they are a fallback for.
    ///
    /// Read against the shipped file rather than against this machine's `config_user.json`, and repeated
    /// against a file that predates the keys: the two answers must be the same number, which is what
    /// "one default, one place" has to mean before a settings row can promise anything by it.
    #[test]
    fn the_shipped_file_declares_the_idle_schedule_and_the_accessors_agree_with_it() {
        let shipped = std::fs::read_to_string(repo_root().join("config_src/config_default.json"))
            .expect("the payload settings must exist in the tree");
        let file: Value = serde_json::from_str(&shipped).expect("the shipped settings are JSON");

        let declared = scratch("idle-declared");
        write(&declared.join("config_src/config_default.json"), &shipped);
        let c = Config::load(&declared).unwrap();

        let empty = scratch("idle-predates");
        write(&empty.join("config_src/config_default.json"), "{}");
        let bare = Config::load(&empty).unwrap();

        for (key, from_file, from_shipped, from_a_file_that_predates_it) in [
            ("day_begin_minutes", file["day_begin_minutes"].as_i64(), c.day_begin_minutes(), bare.day_begin_minutes()),
            ("idle_maintain_time_gap", file["idle_maintain_time_gap"].as_i64(), c.idle_maintain_gap_minutes(), bare.idle_maintain_gap_minutes()),
            (
                "summary_pending_days_in_idle",
                file["summary_pending_days_in_idle"].as_i64(),
                c.summary_pending_days_in_idle(),
                bare.summary_pending_days_in_idle(),
            ),
            (
                "summary_stretch_limit_in_idle",
                file["summary_stretch_limit_in_idle"].as_i64(),
                c.summary_stretch_limit_in_idle(),
                bare.summary_stretch_limit_in_idle(),
            ),
        ] {
            let from_file = from_file.unwrap_or_else(|| panic!("`{key}` is not declared in the shipped settings file"));
            assert_eq!(from_file, from_shipped, "the accessor does not answer the shipped value for `{key}`");
            assert_eq!(from_file, from_a_file_that_predates_it, "the accessor's fallback drifts from `{key}` in the file");
        }
        assert_eq!(Some(c.ai_extract_tag_enabled()), file["enable_ai_extract_tag"].as_bool());
        assert_eq!(Some(c.ai_extract_tag_allowed_in_idle()), file["enable_ai_extract_tag_in_idle"].as_bool());
        assert!(
            !bare.ai_extract_tag_enabled(),
            "the tagger is off until a person says otherwise, on a file that never mentions it"
        );
        assert!(bare.ai_extract_tag_allowed_in_idle(), "and allowed while idle once it is on");
        let _ = std::fs::remove_dir_all(declared);
        let _ = std::fs::remove_dir_all(empty);
    }

    /// The one test for the whole promise this branch was built on.
    ///
    /// Three processes decide "when does post-processing run, and how much may one run take": `windrec`
    /// calls `maintenance_is_due` with [`Config::idle_maintain_gap_minutes`], `windmaint` builds its
    /// `windai summarize` command line from [`Config::summary_pending_days_in_idle`] and
    /// [`Config::summary_stretch_limit_in_idle`], and `windmcp` reports that same budget to whatever is
    /// asking. Before this branch each of them carried its own copy — two constants in `windmaint`, a bare
    /// `i64_or` in `windrec`, and `windmcp` inventing a third figure for a queue it could not see the
    /// ceiling of. Two of the three are separate binaries this crate cannot link against, so the promise
    /// is proved the only way it can be proved across a process boundary: **behaviourally** against one
    /// file, and **structurally** against the source of every binary in the workspace.
    #[test]
    fn the_idle_schedule_the_page_writes_is_the_schedule_every_pass_asks_once() {
        // (a) One file, four different numbers, none of them a shipped default. Every accessor has to
        // answer *this* file. An accessor that still carried its own copy would answer the default here,
        // and the pass would work to a schedule nobody wrote.
        let dir = scratch("idle-one-schedule");
        write(
            &dir.join("config_src/config_default.json"),
            r#"{"idle_maintain_time_gap": 7,
                "summary_pending_days_in_idle": 9,
                "summary_stretch_limit_in_idle": 31,
                "enable_ai_extract_tag": true,
                "enable_ai_summary_in_idle": false}"#,
        );
        let c = Config::load(&dir).unwrap();
        assert_eq!(c.idle_maintain_gap_minutes(), 7, "the recorder's wait comes from the file");
        assert_eq!(c.summary_pending_days_in_idle(), 9, "so does the pass's day budget");
        assert_eq!(c.summary_stretch_limit_in_idle(), 31, "and its stretch budget");
        assert!(c.ai_extract_tag_enabled(), "and the switch the tagger is gated on");
        assert!(!c.ai_summary_allowed_in_idle(), "and the one the summariser is gated on");

        // (b) The structural half, and the one that reaches the other two binaries. Each of these keys is
        // now read in exactly one file — this one — so `windrec`, `windmaint` and `windmcp` cannot be
        // asking the same question of a default of their own. A second `i64_or` anywhere in `windcap`
        // appears in `reads` and fails here. Writers are not flagged: `Config::set` is a stage, not a read,
        // which is what lets the two settings pages own the keys without becoming a second answer.
        for key in [
            "idle_maintain_time_gap",
            "summary_pending_days_in_idle",
            "summary_stretch_limit_in_idle",
        ] {
            let reads = source_lines_reading(key);
            assert_eq!(
                reads,
                vec!["windcap/base/src/config.rs".to_string()],
                "`{key}` is read in more than one place, so the idle pass and the page that schedules it \
                 can disagree about it: {reads:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A gap of zero is a switch, not a value to be clamped away: `windrec`'s `maintenance_is_due` says
    /// "the user turned it off" in those words, and a floor of 1 here would make the off position of the
    /// new row mean "run every hour".
    #[test]
    fn an_idle_gap_of_zero_is_the_off_position_rather_than_a_clamp_boundary() {
        let dir = scratch("idle-gap");
        write(&dir.join("config_src/config_default.json"), r#"{"idle_maintain_time_gap": 0}"#);
        assert_eq!(Config::load(&dir).unwrap().idle_maintain_gap_minutes(), 0, "off survives the accessor");

        write(&dir.join("userdata/config_user.json"), r#"{"idle_maintain_time_gap": 99999}"#);
        assert_eq!(Config::load(&dir).unwrap().idle_maintain_gap_minutes(), 1_440, "a longer wait than a day is clamped");

        // The load-bearing half: an install whose file predates the key keeps the recorder's old answer.
        // The override goes with it, because it is the *user's* answer and would win whatever the
        // defaults layer says.
        std::fs::remove_file(dir.join("userdata/config_user.json")).unwrap();
        write(&dir.join("config_src/config_default.json"), "{}");
        assert_eq!(Config::load(&dir).unwrap().idle_maintain_gap_minutes(), 40, "the shipped default, not zero");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two budget rows answer the same question the pass asked itself when the numbers were
    /// constants, and an out-of-range value is corrected rather than obeyed — a `--pending 0` is a
    /// summarising run that visits no days, and a `--limit` past what `windai` parses is a spawn that
    /// fails after the lock is taken.
    #[test]
    fn the_idle_summarising_budget_is_bounded_by_the_command_that_receives_it() {
        let dir = scratch("idle-budget");
        write(
            &dir.join("config_src/config_default.json"),
            r#"{"summary_pending_days_in_idle": 500, "summary_stretch_limit_in_idle": 0}"#,
        );
        let c = Config::load(&dir).unwrap();
        assert_eq!(c.summary_pending_days_in_idle(), 60, "past the horizon `windai` scans back to is one ceiling");
        assert_eq!(c.summary_stretch_limit_in_idle(), 1, "and a run of no stretches is not a run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fractional_thresholds_survive_the_accessor() {
        let c = defaults();
        // Reading these with i64_or yields the default, which is how a threshold silently drifts.
        assert!((c.f64_or("screenshot_compare_similarity", 0.0) - 0.7).abs() < 1e-9);
        assert!((c.f64_or("ocr_compare_similarity_in_table", 0.0) - 0.94).abs() < 1e-9);
        assert_eq!(c.i64_list("ocr_image_crop_URBL"), vec![6, 6, 6, 3]);
    }

    #[test]
    fn derived_paths_keep_the_upstream_shape() {
        let c = defaults();
        let root = repo_root();
        assert_eq!(c.userdata_dir(), root.join("userdata"));
        assert_eq!(c.db_dir(), root.join("userdata").join("db"));
        assert_eq!(c.videos_dir(), root.join("userdata").join("videos"));
        assert_eq!(c.month_videos_dir(2026, 9), root.join("userdata").join("videos").join("2026-09"));
        assert_eq!(c.flag_note_path(), root.join("userdata").join("flag_mark_note.csv"));
        assert_eq!(c.record_lock_path(), root.join("cache").join("locks").join("LOCK_FILE_RECORD.MD"));
    }

    #[test]
    fn a_saved_config_round_trips_and_only_the_user_file_moves() {
        let dir = std::env::temp_dir().join(format!("windcap-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            r#"{"max_page_result": 20, "exclude_words": ["a"], "lang": "en"}"#,
        )
        .unwrap();

        let mut c = Config::load(&dir).unwrap();
        assert_eq!(c.i64_or("max_page_result", 0), 20);
        c.set("max_page_result", Value::from(50));
        let written = c.save().unwrap();
        assert_eq!(written, dir.join("userdata/config_user.json"));
        assert!(written.exists());

        // Reload: the override wins, and an unrelated default is untouched.
        let again = Config::load(&dir).unwrap();
        assert_eq!(again.i64_or("max_page_result", 0), 50);
        assert_eq!(again.str_or("lang", "?"), "en");
        // The default file is never rewritten by the native side.
        assert_eq!(
            std::fs::read_to_string(dir.join("config_src/config_default.json")).unwrap(),
            r#"{"max_page_result": 20, "exclude_words": ["a"], "lang": "en"}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The close button's rule, in one place for both windows: hide only when the user asked for a
    /// background mode *and* a tray is alive to bring the window back. This process writes its own pid to
    /// the tray lock, which is exactly what `lock_state` reports as a live holder.
    #[test]
    fn a_window_hides_only_when_somebody_is_left_to_raise_it_again() {
        let dir = scratch("close-tray");
        write(&dir.join("config_src/config_default.json"), r#"{"close_window_to_tray": true}"#);
        let lock = dir.join("cache").join("locks").join("LOCK_FILE_TRAY.MD");
        let config = Config::load(&dir).unwrap();
        assert!(config.close_window_to_tray(), "background mode is what a user gets unless they say otherwise");
        assert!(!config.window_hides_on_close(), "no tray, no hiding: the close button must still close");

        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, format!("{}", std::process::id())).unwrap();
        assert!(Config::load(&dir).unwrap().window_hides_on_close(), "a live tray means the window can be brought back");

        // A dead pid in the lock is no tray at all — which is the ordinary state of an install whose
        // machine lost power mid-session, and the one state in which hiding would strand the user.
        std::fs::write(&lock, "4294967295").unwrap();
        assert!(!Config::load(&dir).unwrap().window_hides_on_close(), "a corpse in the lock is not a tray");

        write(&dir.join("userdata/config_user.json"), r#"{"close_window_to_tray": false}"#);
        std::fs::write(&lock, format!("{}", std::process::id())).unwrap();
        assert!(!Config::load(&dir).unwrap().window_hides_on_close(), "the user's own choice wins over a running tray");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The three install layouts, at the layer every binary actually reads through.
    ///
    /// `install::config_src_dir` already pins the rule; these exist because `Config` is the only
    /// thing the eight call sites touch, so a mistake in *this* wiring — a defaults layer that
    /// loads from one place while `config_src_dir` reports another — would be invisible to every
    /// other test in the workspace.
    #[test]
    fn a_legacy_overlay_install_loads_its_settings_from_windrecorder() {
        let dir = scratch("legacy");
        write(&dir.join("windrecorder/config_src/config_default.json"), r#"{"user_name": "legacy", "config_src_dir": "windrecorder\\config_src"}"#);

        let c = Config::load(&dir).unwrap();
        assert_eq!(c.str_or("user_name", "?"), "legacy");
        assert!(matches!(c.defaults_source(), DefaultsSource::Legacy(_)));
        assert_eq!(c.config_src_dir(), dir.join("windrecorder/config_src"));
        assert_eq!(c.config_src_file("languages.json"), dir.join("windrecorder/config_src/languages.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_standalone_install_loads_its_settings_from_config_src() {
        let dir = scratch("standalone");
        write(&dir.join("config_src/config_default.json"), r#"{"user_name": "standalone", "config_src_dir": "config_src"}"#);

        let c = Config::load(&dir).unwrap();
        assert_eq!(c.str_or("user_name", "?"), "standalone");
        assert!(matches!(c.defaults_source(), DefaultsSource::Payload(_)));
        assert_eq!(c.config_src_dir(), dir.join("config_src"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The transition install, through `Config`: both layers on disk, and *every* accessor has to
    /// agree that the payload copy is the one in effect. A config that loaded the new defaults but
    /// reported the old settings directory would hand search a lookup table from one install and
    /// its keys from another.
    #[test]
    fn an_install_mid_upgrade_reads_the_payload_layer_and_not_the_stale_one() {
        let dir = scratch("transition");
        write(&dir.join("windrecorder/config_src/config_default.json"), r#"{"user_name": "stale", "record_seconds": 60}"#);
        write(&dir.join("config_src/config_default.json"), r#"{"user_name": "current", "record_seconds": 900, "config_src_dir": "config_src"}"#);

        let c = Config::load(&dir).unwrap();
        assert_eq!(c.str_or("user_name", "?"), "current");
        assert_eq!(c.i64_or("record_seconds", 0), 900);
        assert_eq!(c.defaults_path().unwrap(), dir.join("config_src/config_default.json"));
        assert_eq!(c.config_src_dir(), dir.join("config_src"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A user who moved the settings directory by hand is still obeyed, because the shipped key
    /// predates this change and thousands of `config_user.json` files carry the old literal.
    #[test]
    fn an_explicit_config_src_dir_override_is_honoured() {
        let dir = scratch("override");
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        write(&dir.join("config_src/config_default.json"), r#"{"config_src_dir": "elsewhere"}"#);

        let c = Config::load(&dir).unwrap();
        assert_eq!(c.config_src_dir(), dir.join("elsewhere"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An override pointing at a directory that is not there is *not* obeyed — it would take every
    /// lookup table down with it, and the sentinel's answer is the one that still works.
    #[test]
    fn an_explicit_config_src_dir_that_does_not_exist_falls_back_to_the_sentinel() {
        let dir = scratch("bad-override");
        write(&dir.join("config_src/config_default.json"), r#"{"config_src_dir": "D:/nope"}"#);

        let c = Config::load(&dir).unwrap();
        assert_eq!(c.config_src_dir(), dir.join("config_src"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The last-resort seed, seen from the reader: a root with no settings file anywhere still
    /// answers with the compiled-in factory values instead of emptying every key.
    #[test]
    fn a_root_with_no_settings_file_still_answers_from_the_compiled_in_defaults() {
        let dir = scratch("embedded");
        std::fs::create_dir_all(dir.join("userdata")).unwrap();

        let c = Config::load(&dir).unwrap();
        assert_eq!(*c.defaults_source(), DefaultsSource::Embedded);
        assert_eq!(c.str_or("user_name", "?"), "default", "the real shipped value, not the caller's fallback");
        assert_eq!(c.i64_or("record_seconds", 0), 900);
        // A user file still overrides it, exactly as it overrides the on-disk defaults.
        std::fs::write(dir.join("userdata/config_user.json"), r#"{"user_name": "mine"}"#).unwrap();
        assert_eq!(Config::load(&dir).unwrap().str_or("user_name", "?"), "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The compiled-in copy and the file the payload ships must not be allowed to drift: this test
    /// reads the same bytes `release.ps1` stages.
    #[test]
    fn the_embedded_defaults_are_the_shipped_file_verbatim() {
        let on_disk = std::fs::read_to_string(repo_root().join("config_src/config_default.json")).expect("the payload settings must exist in the tree");
        assert_eq!(on_disk, install::embedded_defaults());
    }

    /// A preview is a promise about pixels, not a preference: a stored picture narrower than the box it
    /// is drawn in is *stretched*, and an install that came from upstream carries exactly that number in
    /// its own `config_user.json`. So the shipped answer and the user's answer have to stay two different
    /// things — the accessor obeys the second, and the settings page is allowed to say what the first
    /// means. Both halves are pinned here because a clamp that quietly "fixed" the user's value would
    /// make the settings row a decoy, and that is the bug this product has already shipped once.
    #[test]
    fn a_stored_preview_narrower_than_the_card_is_called_a_stamp() {
        let dir = scratch("preview-floor");
        write(&dir.join("userdata/config_user.json"), "{}");
        let shipped = Config::load(&dir).unwrap();
        assert_eq!(shipped.thumbnail_width(), crate::image::CARD_PREVIEW_FLOOR, "a fresh install answers with the floor");
        assert_eq!(shipped.thumbnail_quality(), 70, "and the quality the shipped file says, not the one the codec defaults to");
        assert!(!shipped.preview_is_a_stamp());

        write(&dir.join("userdata/config_user.json"), r#"{"thumbnail_generation_size_width": 70, "thumbnail_generation_jpg_quality": 30}"#);
        let upstream = Config::load(&dir).unwrap();
        assert_eq!(upstream.thumbnail_width(), 70, "the user's own number still wins");
        assert!(upstream.preview_is_a_stamp(), "and the window can name what that costs them");

        // An out-of-range value is clamped rather than obeyed, because a width of zero would mean no
        // picture anywhere in the product and a width of a megapixel would mean a disk full of JPEG.
        write(&dir.join("userdata/config_user.json"), r#"{"thumbnail_generation_size_width": 0}"#);
        assert_eq!(Config::load(&dir).unwrap().thumbnail_width(), 8, "clamped, and still honest about being a stamp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same install mid-upgrade, but with the other half of it: `Config::save` snapshots the
    /// merged config, so this install's `userdata/config_user.json` still carries the old
    /// `windrecorder\\config_src` literal inherited from the defaults it was seeded from. That is
    /// not a hand-written override, and honouring it would keep the upgraded install reading the
    /// settings layer its own upgrade replaced — the single most confusing field bug available here.
    #[test]
    fn an_upgraded_overlay_whose_user_file_still_names_the_legacy_directory_reads_the_payload_one() {
        let dir = scratch("inherited-literal");
        write(&dir.join("windrecorder/config_src/config_default.json"), r#"{"user_name": "stale", "config_src_dir": "windrecorder\\config_src"}"#);
        write(&dir.join("config_src/config_default.json"), r#"{"user_name": "current", "config_src_dir": "config_src"}"#);
        write(&dir.join("userdata/config_user.json"), r#"{"user_name": "mine", "config_src_dir": "windrecorder\\config_src"}"#);

        let c = Config::load(&dir).unwrap();
        assert_eq!(c.str_or("user_name", "?"), "mine", "the user's own setting is still theirs");
        assert_eq!(c.config_src_dir(), dir.join("config_src"), "but the inherited default literal is not an override");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Which files in the workspace read `key` back out of the merged map, as repo-relative paths.
    ///
    /// A *read* means a call to one of `Config`'s typed accessors with the quoted key on the same line,
    /// which is the shape a second source of truth takes. Writing a key (`config.set("k", …)`), naming it
    /// in a doc comment, and naming it inside a JSON fixture are all left alone: the first is what a
    /// settings page is for, and the other two cannot disagree with an accessor because they hold no value.
    ///
    /// Test code is excluded twice over — a file whose name says so, and everything at or after a file's
    /// own `#[cfg(test)]` — because a fixture that reads a key with its own fallback is exactly the
    /// duplication this guard exists to catch in *production*, and asserting about a written file has to
    /// read it somehow.
    fn source_lines_reading(key: &str) -> Vec<String> {
        let quoted = format!("\"{key}\"");
        let readers = ["i64_or(", "bool_or(", "str_or(", "f64_or(", "i64_list(", "str_list(", "raw(", ".get("];
        let mut hits: Vec<String> = Vec::new();
        collect_reads(&repo_root().join("windcap"), &quoted, &readers, &mut hits);
        hits.sort();
        hits.dedup();
        hits
    }

    fn collect_reads(dir: &Path, quoted: &str, readers: &[&str], hits: &mut Vec<String>) {
        const SKIP: [&str; 5] = ["target", "dist", "node_modules", "gen", ".worktrees"];
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !SKIP.contains(&name.as_str()) {
                    collect_reads(&path, quoted, readers, hits);
                }
                continue;
            }
            if !name.ends_with(".rs")
                || name.contains("test")
                || name == "fixtures.rs"
                || path.components().any(|part| part.as_os_str() == "tests")
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            // Everything from the file's own test module onward is a test, whatever file it lives in.
            let production = text.lines().take_while(|line| !line.trim_start().starts_with("#[cfg(test)]"));
            if production.filter(|line| line.contains(quoted) && readers.iter().any(|call| line.contains(call))).next().is_some() {
                let relative = path
                    .strip_prefix(repo_root())
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                hits.push(relative);
            }
        }
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}
