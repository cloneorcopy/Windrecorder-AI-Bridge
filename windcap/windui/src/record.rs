//! The recorder's own keys, typed and bounded, because this form writes the file `windrec` reads
//! back on its next start.
//!
//! `settings.rs` edits the fifteen keys a *screen* reads; this edits the twenty-five a *recording*
//! reads — the twenty the grabber, the reindexer and the maintenance pass consume, the one that
//! decides whether the tray launches a recorder at all (`start_recording_on_startup`, read by
//! `supervisor`'s `supervisor.rs`), `record_mode`, and the three that say when the post-processing pass
//! runs and how much one run may take. The distinction is why the two forms are separate types with
//! separate Save buttons rather than one long list: a mistake in the first costs the user a page of
//! results, a mistake in the second costs them the next session's footage.
//! `Config::save` writes the whole merged map to `userdata/config_user.json`, so every value here is
//! read by another process the moment it lands.
//!
//! The three newest rows are on *this* page and not on Settings for that reason and one other: they are
//! read by `windrec` and `windmaint`, which are separate processes that boot from this file, and they are
//! one group ("Idle maintenance pass") holding the whole of "when, and for how long" — the gap that
//! launches the pass, and the two budgets that bound one run of it. Nothing here schedules anything: the
//! recorder still decides it is idle, spawns `windmaint`, and `windmaint` exits.
//!
//! Three keys the recorder's file carries are deliberately **not** widgets on this page, because a
//! control nothing downstream honours is a lie the user has to discover by losing footage:
//!
//!   * `record_deep_linking` — read only to decide whether `windrec` warns about a gap it cannot
//!     close; shown as a notice, never written. ([`Rec::deep_linking_promised`]).
//!   * `convert_screenshots_to_vid_energy_saving_mode` — the battery gate on the screenshot→video
//!     pass. `record_screen.py` (`is_power_plugged_in`, lines 85/176/280) honours all three modes,
//!     but no native binary reads it: `windrec` and `windmaint` stitch footage on their own schedule
//!     whatever the charger is doing. Offering modes 1 and 2 here would promise a power behaviour the
//!     native engine does not implement, so the switch is removed and the user's value preserved.
//!     ([`Rec::energy_saving_requested`]).
//!   * `record_crf` — *read* by `windmaint` and thrown away before it reaches ffmpeg, which makes it
//!     the tenth control this branch has had to take out. `encode::PresetTable::encoder_args` takes
//!     the CRF and appends `-crf <value>` only when the chosen preset states no rate control at all;
//!     every preset the payload ships states it with `-b:v BITRATE`, so on a stock install the number
//!     in this box changes nothing about any file. `windmaint`'s own
//!     `no_shipped_record_preset_carries_the_crf_into_the_command_line` is the measurement, and the
//!     record path's real rate control is `record_bitrate`, which is a widget. The key is not
//!     rewritten by this page either, because it is *not* inert everywhere: a preset someone authors
//!     by hand may name `-crf` or `CRF_NUM`, and then the stored value is what ffmpeg gets. So it
//!     leaves the form rather than being re-worded, and rides the merged map back out untouched.
//!
//! A fourth key left this page by deletion rather than by demotion: **`use_native_core`**. It chose
//! which of two implementations the tray launched, the Python application was deleted in 3f37cbf, and
//! the fallback that made the choice meaningful went with it — `supervisor/src/native.rs`'s
//! `recorder_argv` and `ui_argv` now return a native command unconditionally, or a `Missing` that
//! names the absent binary. So the key had no reader anywhere while this page still drew a checkbox
//! for it and still wrote it on Save. That is the worst shape a setting can take: it persists, it
//! changes nothing, and the user is left believing they configured something. It is therefore gone
//! outright — no field, no default, no stage line — rather than greyed out. A user's own file may
//! still carry the key from a pre-deletion install; `supervisor`'s tests pin that it changes nothing
//! there, and `Config::save`'s merged map leaves it alone.
//!
//! Where a range comes from matters, so each field says which of these it is:
//!
//!   * `recording.py`'s `st.number_input` bounds, kept verbatim (`screenshot_interval_second`
//!     3..=15, `record_bitrate` 50..=10000, `compress_quality` 0..=50). The Python app reads this
//!     file back and refuses to display a value outside its own widget, which would look like
//!     corruption on the other side.
//!   * keys with **no upstream widget at all** — `record_seconds`, `record_framerate`,
//!     `screenshot_interrupt_recording_count` — bounded here
//!     against what the native consumer actually does with them. Inventing a range silently is how
//!     a settings screen starts lying, so each one names its source in `help`.
//!   * `video_compress_rate`, which is stored as the *string* `"0.5"` because `recording.py` looks
//!     the value up in a string-keyed table and a JSON number there is a `KeyError` for the user.

use serde::Serialize;

use std::collections::BTreeMap;

use serde_json::Value;
use wind_base::config::Config;

/// The whole recording section, as the config holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Rec {
    pub record_mode: String,
    pub record_seconds: i64,
    pub record_framerate: i64,
    pub record_bitrate: i64,
    pub record_encoder: String,
    pub screenshot_interval_second: i64,
    pub screenshot_interrupt_recording_count: i64,
    pub ocr_compare_similarity: f64,
    pub ocr_compare_similarity_in_table: f64,
    pub index_reduce_same_content_at_different_time: bool,
    pub screentime_not_change_to_pause_record: i64,
    /// Minutes of idle after which `windrec` launches `windmaint`. Read by the recorder through
    /// [`wind_base::config::Config::idle_maintain_gap_minutes`], which is also where the default and the
    /// bounds live; zero is the off position, not a clamp boundary.
    pub idle_maintain_time_gap: i64,
    /// How many product days one idle summarising run takes on. The pass hands this to
    /// `windai summarize --pending`, and `windmcp`'s queue reports the same number from the same accessor.
    pub summary_pending_days_in_idle: i64,
    /// How many stretches one idle summarising run takes on — `windai summarize --limit`, the other half
    /// of the budget the two rows together are.
    pub summary_stretch_limit_in_idle: i64,
    pub multi_display_record_strategy: String,
    pub record_single_display_index: i64,
    pub record_screenshot_method_capture_foreground_window_only: bool,
    /// Whether the tray begins a recording the moment it starts. Read by
    /// `supervisor/src/supervisor.rs`'s `start_recording_on_startup` with a default of `true`.
    pub start_recording_on_startup: bool,
    pub vid_store_day: i64,
    pub vid_compress_day: i64,
    pub video_compress_rate: String,
    pub compress_encoder: String,
    pub compress_accelerator: String,
    pub compress_quality: i64,
    pub compress_cpu_threads: i64,
    /// Whether the user's config has promised them browser deep links.
    ///
    /// Carried, not editable: deliberately not an [`RField`], so it reaches no widget, no draft and
    /// no line of [`Rec::stage`], and a Save leaves `record_deep_linking` exactly as the file holds
    /// it. It exists for one thing only — `view::recording` reads it and says out loud, when it is
    /// `true`, that the native recorder indexes no row with an address to reopen. The alternative was
    /// the recorder's own `eprintln!`, which a tray-launched run has no console to show and whose log
    /// file is truncated at the next start.
    pub deep_linking_promised: bool,
    /// Whether the user has asked for the screenshot→video pass to be gated on the battery.
    ///
    /// Carried, not editable — the same treatment [`Rec::deep_linking_promised`] gets, and for the
    /// same reason. The native recorder and `windmaint` do not read `convert_screenshots_to_vid_energy_saving_mode`
    /// at all, so there is no native behaviour this widget could drive; a radio here would persist a
    /// choice the engine ignores. It is *not* staged either, so a value the user set through the
    /// Python recorder before this branch deleted it (which did honour it, via
    /// `is_power_plugged_in`) survives a Save from this
    /// page untouched, riding the merged map back out. `view::recording` reads this flag and says out
    /// loud that the gate is inert on the native path.
    pub energy_saving_requested: bool,
}

/// Which key a widget edits. An enum rather than a `&'static str` so the compiler's exhaustiveness
/// check is what notices a twenty-sixth setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RField {
    RecordMode,
    RecordSeconds,
    RecordFramerate,
    RecordBitrate,
    RecordEncoder,
    ScreenshotInterval,
    ScreenshotInterruptCount,
    ForegroundWindowOnly,
    DisplayStrategy,
    SingleDisplayIndex,
    OcrSimilarity,
    OcrSimilarityInTable,
    ReduceSameContent,
    IdlePauseMinutes,
    IdleMaintainGap,
    SummaryPendingDays,
    SummaryStretchLimit,
    VidStoreDay,
    VidCompressDay,
    CompressRate,
    CompressEncoder,
    CompressAccelerator,
    CompressQuality,
    CompressCpuThreads,
    StartOnBoot,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Int { min: i64, max: i64 },
    /// A stored fraction — `0.7` for a similarity threshold. `i64_or` falls through to its default
    /// on any non-integer, which is how such a threshold drifts without anyone typing it.
    Fraction { min: f64, max: f64 },
    Bool,
    /// One of a list whose contents live in a file beside the config, so the options are supplied
    /// per call rather than baked into the field.
    Choice(Vec<String>),
}

/// The shipped `config_default.json` values, which double as the fallbacks for a config that
/// predates a key — the same rule `settings.rs` follows.
impl Default for Rec {
    fn default() -> Rec {
        Rec {
            record_mode: "screenshot_array".into(),
            record_seconds: 900,
            record_framerate: 2,
            record_bitrate: 200,
            record_encoder: "cpu_h264".into(),
            screenshot_interval_second: 3,
            screenshot_interrupt_recording_count: 40,
            ocr_compare_similarity: 0.7,
            ocr_compare_similarity_in_table: 0.94,
            index_reduce_same_content_at_different_time: true,
            screentime_not_change_to_pause_record: 5,
            // The shipped `config_default.json` values, which are also what `windrec` and `windmaint`
            // fall back to — both read them through the same `Config` accessors `load` below calls, so
            // an install whose file predates the keys gets this answer from every door at once.
            idle_maintain_time_gap: 40,
            summary_pending_days_in_idle: 2,
            summary_stretch_limit_in_idle: 40,
            multi_display_record_strategy: "all".into(),
            record_single_display_index: 1,
            record_screenshot_method_capture_foreground_window_only: true,
            // The one surviving switch defaults to exactly what its reader defaults to, so a config
            // that never mentions the key behaves the way the tray already behaves: recording started
            // on launch. See `supervisor.rs` (`start_recording_on_startup`, true).
            start_recording_on_startup: true,
            vid_store_day: 1200,
            vid_compress_day: 300,
            video_compress_rate: "0.5".into(),
            compress_encoder: "x264".into(),
            compress_accelerator: "cpu".into(),
            compress_quality: 39,
            compress_cpu_threads: 2,
            // A `Rec` nobody read out of a config promised nothing and asked for no battery gate.
            // Only the file can make either promise, which is why `load` reads them the way it does.
            deep_linking_promised: false,
            energy_saving_requested: false,
        }
    }
}

/// What the *machine* and the *preset files* say about a field's ceiling, as opposed to what the
/// config format allows.
///
/// `compress_cpu_threads` is bounded upstream by `multiprocessing.cpu_count()`, and the two encoder
/// boxes by the keys of `record_preset.json` and `video_compress_preset.json`. None of that is
/// knowledge a field can carry, so it arrives here — read once at boot by `backend::rec_options`.
#[derive(Debug, Clone, Serialize)]
pub struct RecOptions {
    /// Keys of `windrecorder/config_src/record_preset.json`.
    pub record_encoders: Vec<String>,
    /// `compress_encoder -> accelerators`, from `video_compress_preset.json`.
    pub compress: Vec<(String, Vec<String>)>,
    /// `std::thread::available_parallelism`, so the thread slider cannot offer sixty-four workers on
    /// a four-core laptop the way a hand-written ceiling would.
    pub cpu_cores: i64,
}

impl Default for RecOptions {
    /// The shipped preset files' contents. Used when a config directory is missing them, because a
    /// form whose encoder box is empty cannot be repaired by the user pressing Save.
    fn default() -> RecOptions {
        RecOptions {
            record_encoders: ["cpu_h264", "cpu_h265", "NVIDIA_h265", "AMD_h265", "SVT-AV1"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            compress: {
                const ACCELERATORS: [&str; 4] = ["cpu", "qsv", "nvenc", "amf"];
                ["x264", "x265", "av1"]
                    .iter()
                    .map(|encoder| (encoder.to_string(), ACCELERATORS.iter().map(|a| a.to_string()).collect()))
                    .collect()
            },
            cpu_cores: 1,
        }
    }
}

impl RecOptions {
    /// The accelerators that exist for one encoder. `recording.py` builds the same list from
    /// `CONFIG_VIDEO_COMPRESS_PRESET[encoder].keys()`.
    pub fn accelerators(&self, encoder: &str) -> Vec<String> {
        self.compress
            .iter()
            .find(|(name, _)| name == encoder)
            .map(|(_, list)| list.clone())
            .unwrap_or_default()
    }

    pub fn encoders(&self) -> Vec<String> {
        self.compress.iter().map(|(name, _)| name.clone()).collect()
    }
}

/// The monitors attached to the desktop, as a settings screen needs them.
///
/// Deliberately not `windcap::capture::Monitor`: this is what `model` is allowed to hold, and the
/// index is the config's 1-based `mss` numbering rather than a Rust-side 0-based one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayInfo {
    pub index: usize,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

impl DisplayInfo {
    /// `#2 1440 x 2560 (portrait)`. The panel exists so a user can tell which of four cables they
    /// just pointed the recorder at, so orientation and the primary mark are in the label rather
    /// than implied by a bare number.
    pub fn label(&self) -> String {
        let shape = if self.height > self.width { "portrait" } else { "landscape" };
        let mark = if self.primary { ", primary" } else { "" };
        format!("#{} {} x {} ({shape}{})", self.index, self.width, self.height, mark)
    }

    /// The other number that decides whether a panel is worth recording full-size, and the one
    /// `windcap`'s own cost note is written against: the grab tracks the *source* rectangle, so
    /// megapixels per frame is what a second monitor actually costs.
    pub fn megapixels(&self) -> f64 {
        f64::from(self.width * self.height) / 1e6
    }
}

impl RField {
    /// Grouped so one list drives both the panel's sections and the validation loop: the fields
    /// `validate` must see in dependency order (encoder before accelerator) and the fields
    /// `view::recording` groups are the same twenty-five.
    pub const ALL: [RField; 25] = [
        RField::RecordMode,
        RField::RecordSeconds,
        RField::RecordFramerate,
        RField::RecordBitrate,
        RField::RecordEncoder,
        RField::ScreenshotInterval,
        RField::ScreenshotInterruptCount,
        RField::ForegroundWindowOnly,
        RField::DisplayStrategy,
        RField::SingleDisplayIndex,
        RField::OcrSimilarity,
        RField::OcrSimilarityInTable,
        RField::ReduceSameContent,
        RField::IdlePauseMinutes,
        RField::IdleMaintainGap,
        RField::SummaryPendingDays,
        RField::SummaryStretchLimit,
        RField::VidStoreDay,
        RField::VidCompressDay,
        RField::CompressRate,
        RField::CompressEncoder,
        RField::CompressAccelerator,
        RField::CompressQuality,
        RField::CompressCpuThreads,
        RField::StartOnBoot,
    ];

    /// The section the field belongs to. Derived from the field rather than stored beside it so
    /// [`RField::ALL`] stays the single list that drives both the panel's layout and `validate`'s
    /// dependency order.
    pub fn group(self) -> &'static str {
        match self {
            RField::RecordMode
            | RField::RecordSeconds
            | RField::RecordFramerate
            | RField::RecordBitrate
            | RField::RecordEncoder
            | RField::ScreenshotInterval
            | RField::ScreenshotInterruptCount
            | RField::ForegroundWindowOnly => "Capture",
            RField::DisplayStrategy | RField::SingleDisplayIndex => "Displays",
            RField::OcrSimilarity | RField::OcrSimilarityInTable | RField::ReduceSameContent | RField::IdlePauseMinutes => {
                "Indexing and idle"
            }
            // The whole post-processing schedule in one section: when the pass runs, and how much of the
            // outstanding work one run of it may take on. Nothing in this group schedules anything — the
            // recorder still decides it is idle and spawns `windmaint`, which then exits.
            RField::IdleMaintainGap | RField::SummaryPendingDays | RField::SummaryStretchLimit => "Idle maintenance pass",
            RField::VidStoreDay
            | RField::VidCompressDay
            | RField::CompressRate
            | RField::CompressEncoder
            | RField::CompressAccelerator
            | RField::CompressQuality
            | RField::CompressCpuThreads => "Compression and retention",
            RField::StartOnBoot => "Engine and startup",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            RField::RecordMode => "record_mode",
            RField::RecordSeconds => "record_seconds",
            RField::RecordFramerate => "record_framerate",
            RField::RecordBitrate => "record_bitrate",
            RField::RecordEncoder => "record_encoder",
            RField::ScreenshotInterval => "screenshot_interval_second",
            RField::ScreenshotInterruptCount => "screenshot_interrupt_recording_count",
            RField::OcrSimilarity => "ocr_compare_similarity",
            RField::OcrSimilarityInTable => "ocr_compare_similarity_in_table",
            RField::ReduceSameContent => "index_reduce_same_content_at_different_time",
            RField::IdlePauseMinutes => "screentime_not_change_to_pause_record",
            // The three idle-pass keys, spelled exactly as `windrec`'s `plan_from` and `windmaint`'s
            // `summary_budget` read them out of the merged map.
            RField::IdleMaintainGap => "idle_maintain_time_gap",
            RField::SummaryPendingDays => "summary_pending_days_in_idle",
            RField::SummaryStretchLimit => "summary_stretch_limit_in_idle",
            RField::DisplayStrategy => "multi_display_record_strategy",
            RField::SingleDisplayIndex => "record_single_display_index",
            RField::ForegroundWindowOnly => "record_screenshot_method_capture_foreground_window_only",
            RField::VidStoreDay => "vid_store_day",
            RField::VidCompressDay => "vid_compress_day",
            RField::CompressRate => "video_compress_rate",
            RField::CompressEncoder => "compress_encoder",
            RField::CompressAccelerator => "compress_accelerator",
            RField::CompressQuality => "compress_quality",
            RField::CompressCpuThreads => "compress_cpu_threads",
            RField::StartOnBoot => "start_recording_on_startup",
        }
    }

    /// The catalog key this row's label is translated under.
    ///
    /// Sixteen of the twenty-five are upstream's own `rs_*` rows: its recording page already named the
    /// same setting and already has `sc` and `ja` text for it, so writing a fresh key per row would
    /// throw those translations away and leave contributors two rows to maintain for one control. The
    /// nine rows upstream never worded this way are `windui_rec_*`, and they are all this page adds.
    pub fn label_key(self) -> &'static str {
        match self {
            RField::RecordMode => "rs_text_record_mode",
            RField::RecordSeconds => "windui_rec_record_seconds",
            RField::RecordFramerate => "windui_rec_framerate",
            RField::RecordBitrate => "rs_text_record_bitrate",
            RField::RecordEncoder => "rs_text_record_encoder",
            RField::ScreenshotInterval => "rs_input_screenshot_interval_second",
            RField::ScreenshotInterruptCount => "windui_rec_interrupt_count",
            RField::OcrSimilarity => "windui_rec_ocr_similarity",
            RField::OcrSimilarityInTable => "windui_rec_ocr_similarity_in_table",
            RField::ReduceSameContent => "set_checkbox_reduce_same_content_at_different_time",
            RField::IdlePauseMinutes => "rs_input_stop_recording_when_screen_freeze",
            RField::IdleMaintainGap => "windui_rec_idle_maintain_gap",
            RField::SummaryPendingDays => "windui_rec_summary_pending_days",
            RField::SummaryStretchLimit => "windui_rec_summary_stretch_limit",
            RField::DisplayStrategy => "rs_text_record_range",
            RField::SingleDisplayIndex => "rs_text_record_single_display_select",
            RField::ForegroundWindowOnly => "rs_checkbox_record_screenshot_method_capture_foreground_window_only",
            RField::VidStoreDay => "windui_rec_vid_store_day",
            RField::VidCompressDay => "rs_input_vid_compress_time",
            RField::CompressRate => "rs_selectbox_compress_ratio",
            RField::CompressEncoder => "rs_text_compress_encoder",
            RField::CompressAccelerator => "rs_text_compress_accelerator",
            RField::CompressQuality => "rs_text_compress_CRF",
            RField::CompressCpuThreads => "rs_text_compress_cpu_threads",
            RField::StartOnBoot => "rs_checkbox_is_start_recording_on_start_app",
        }
    }

    /// The catalog key for the section heading, so a Chinese install does not get one Chinese page and
    /// one English one.
    pub fn group_key(self) -> &'static str {
        match self.group() {
            "Capture" => "windui_rec_group_capture",
            "Displays" => "windui_rec_group_displays",
            "Indexing and idle" => "windui_rec_group_indexing",
            "Idle maintenance pass" => "windui_rec_group_idle_pass",
            "Compression and retention" => "windui_rec_group_compression",
            _ => "windui_rec_group_startup",
        }
    }

    /// The catalog key this row's explanation is translated under.
    ///
    /// The page used to forward [`help`] — engineering notes about `wind-reindex` and moov atoms, in
    /// English, on a page whose every other word followed the user's language. The catalog rows are one
    /// sentence each and say what the number does; the Rust strings stay as the fallback and as the
    /// maintainer's comment on the same field.
    ///
    /// Two of these rows moved to a second key when the Statistics page's "hours" figure was explained.
    /// `presence_gap_secs` is *derived* from `record_seconds` and `screentime_not_change_to_pause_record`
    /// — editing either moves every hours figure the Stat tab prints — and the only honest place to say so
    /// is beside the two numbers that cause it. The old `windui_rec_help_record_seconds` and
    /// `windui_rec_help_idle_pause` rows are left in `languages.json` untouched, because a translation
    /// contributor's history is worth more than one orphaned key, and the new rows carry their text
    /// forward plus the sentence that was missing.
    pub fn help_key(self) -> &'static str {
        match self {
            RField::RecordMode => "windui_rec_help_record_mode",
            RField::RecordSeconds => "windui_rec_help_record_seconds_and_hours",
            RField::RecordFramerate => "windui_rec_help_record_framerate",
            RField::RecordBitrate => "windui_rec_help_record_bitrate",
            RField::RecordEncoder => "windui_rec_help_record_encoder",
            RField::ScreenshotInterval => "windui_rec_help_screenshot_interval",
            RField::ScreenshotInterruptCount => "windui_rec_help_interrupt_count",
            RField::OcrSimilarity => "windui_rec_help_ocr_similarity",
            RField::OcrSimilarityInTable => "windui_rec_help_ocr_similarity_in_table",
            RField::ReduceSameContent => "windui_rec_help_reduce_same_content",
            RField::IdlePauseMinutes => "windui_rec_help_idle_pause_and_hours",
            RField::IdleMaintainGap => "windui_rec_help_idle_maintain_gap",
            RField::SummaryPendingDays => "windui_rec_help_summary_pending_days",
            RField::SummaryStretchLimit => "windui_rec_help_summary_stretch_limit",
            RField::DisplayStrategy => "windui_rec_help_display_strategy",
            RField::SingleDisplayIndex => "windui_rec_help_single_display",
            RField::ForegroundWindowOnly => "windui_rec_help_foreground_only",
            RField::StartOnBoot => "windui_rec_help_start_on_boot",
            RField::VidStoreDay => "windui_rec_help_vid_store_day",
            RField::VidCompressDay => "windui_rec_help_vid_compress_day",
            RField::CompressRate => "windui_rec_help_compress_rate",
            RField::CompressEncoder => "windui_rec_help_compress_encoder",
            RField::CompressAccelerator => "windui_rec_help_compress_accelerator",
            RField::CompressQuality => "windui_rec_help_compress_quality",
            RField::CompressCpuThreads => "windui_rec_help_compress_cpu_threads",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RField::RecordMode => "Record mode",
            RField::RecordSeconds => "Seconds per segment",
            RField::RecordFramerate => "Frames per second",
            RField::RecordBitrate => "Record bitrate (kbps)",
            RField::RecordEncoder => "Record encoder",
            RField::ScreenshotInterval => "Screenshot analysis interval (s)",
            RField::ScreenshotInterruptCount => "Screenshots before a forced cut",
            RField::OcrSimilarity => "Repeated-text threshold",
            RField::OcrSimilarityInTable => "In-table dedup threshold",
            RField::ReduceSameContent => "Drop same content across time",
            RField::IdlePauseMinutes => "Pause after a frozen screen (minutes)",
            RField::IdleMaintainGap => "Run the idle maintenance pass after (minutes idle)",
            RField::SummaryPendingDays => "Days one idle run summarises",
            RField::SummaryStretchLimit => "Stretches one idle run summarises",
            RField::DisplayStrategy => "Record which displays",
            RField::SingleDisplayIndex => "Record display index",
            RField::ForegroundWindowOnly => "Capture the foreground window only",
            RField::VidStoreDay => "Keep videos for (days)",
            RField::VidCompressDay => "Compress videos older than (days)",
            RField::CompressRate => "Compress scale",
            RField::CompressEncoder => "Compress encoder",
            RField::CompressAccelerator => "Compress accelerator",
            RField::CompressQuality => "Compress CRF",
            RField::CompressCpuThreads => "Compress CPU threads",
            RField::StartOnBoot => "Start recording when the tray launches",
        }
    }

    /// The effect, in the consumer's own words. This is the tooltip, and the user is editing a file
    /// another process boots from.
    pub fn help(self) -> &'static str {
        match self {
            RField::RecordMode => {
                "`screenshot_array` grabs still frames and lets the maintenance pass stitch them \
                 into a video; `ffmpeg` records a live stream. The native recorder implements only \
                 the screenshot path — `windrec` never reads this key and always grabs still frames \
                 — so `ffmpeg` is deliberately not offered here: a native page must not persist a \
                 mode the native engine cannot honour. A config that already names `ffmpeg` is \
                 still read back and kept rather than rewritten, because this page owns the other \
                 twenty-four keys and silently rewriting one it does not is how a settings editor \
                 loses a user's data."
            }
            RField::RecordSeconds => {
                "How long one segment runs before the recorder rotates to a new file. No widget \
                 exists upstream, so the range is `windrec`'s own: `segment.rs` compares `now - opened_at` \
                 against it, so a minute would index footage into dozens of files and ten hours would put \
                 a long time between the last closed moov atom and a crash. It also moves a number this \
                 page does not have a row for: the Statistics page's hours are counted with a gap of \
                 `max(this, the pause row below)`, clamped between 5 minutes and 2 hours, so raising this \
                 raises every hours figure in the window — a stretch with nothing new on screen is then \
                 read as you still being there for up to that long."
            }
            RField::RecordBitrate => {
                "Target bitrate the maintenance pass encodes each slice at, in kbps, substituted into \
                 the `BITRATE` placeholder of the `record_encoder` preset above. 50..=10000 is \
                 upstream's own widget range. It is the record path's real rate control: the shipped \
                 presets all state their rate this way rather than with a constant rate factor."
            }
            RField::RecordFramerate => {
                "Frames per second the screenshot array is converted at, which is how a frame index \
                 becomes a second inside a segment. Read by `wind-reindex`, whose row timestamps are \
                 `round(frame_index / record_framerate)`, while `windrec` itself derives a row's \
                 offset from `screenshot_interval_second`. Editable because the two must not disagree \
                 about the same files."
            }
            RField::RecordEncoder => {
                "A key of `config_src/record_preset.json`, whose `ffmpeg_cmd` is spliced into the \
                 encode `windmaint` runs when it stitches a screenshot slice into a video — so the \
                 name chosen here is the name that reaches `-c:v`. `windmaint`'s table refuses a name \
                 that is not in that file and lists the ones that are, which is why this is a combo \
                 over the file's contents and not a text box. Being in the file is not the same as \
                 working here: `NVIDIA_h265` needs an NVIDIA encoder and `AMD_h265` an AMD one, and on \
                 a machine that does not have one `windmaint` says so, names what ffmpeg answered, and \
                 encodes the footage with `cpu_h264` instead rather than leaving it as JPEGs forever. \
                 `windmaint doctor` prints the same verdict before you record anything."
            }
            RField::ScreenshotInterval => {
                "Seconds between analysed frames. Upstream holds this at three or more because one \
                 frame's OCR and comparison take longer than that otherwise; `windrec` additionally \
                 clamps it upward rather than trusting the file."
            }
            RField::ScreenshotInterruptCount => {
                "How many screenshots may pile up before the recorder cuts a segment anyway, so a \
                 nine-hour stare at one page cannot eat the disk. `windrec` casts this to `u32` after \
                 clamping it to at least 1: zero would mean a segment that never closes."
            }
            RField::OcrSimilarity => {
                "Two frames this alike in recognised text are the same content and the second is not \
                 indexed. `windrec` multiplies it by 100 and compares a character-set Jaccard — \
                 deliberately upstream's metric, quirks and all — so this decides how many rows you \
                 get, not what a row means."
            }
            RField::OcrSimilarityInTable => {
                "The same idea across a segment's whole frame table instead of only the previous \
                 frame: rows this similar collapse into one. It is the knob that decides how much of \
                 a static page survives into the index."
            }
            RField::ReduceSameContent => {
                "Mark repeated content as duplicate across time rather than only within one segment, \
                 so a page left open overnight indexes once. Off means the index keeps every frame the \
                 OCR ran on."
            }
            RField::IdlePauseMinutes => {
                "Minutes of an unchanged screen before the recorder pauses itself. 0 means never \
                 pause, which is the case `segment.rs` tests for by name; 240 is a working day, and \
                 anything past it is indistinguishable from off. Like `Seconds per segment` above, this \
                 is one of the two numbers the Statistics page's hours are measured with: a still screen \
                 that lasts less than this is not yet a pause, so a gap of up to `max(this, seconds per \
                 segment)` — clamped between 5 minutes and 2 hours — still counts as time at the machine."
            }
            RField::IdleMaintainGap => {
                "How long the screen must sit idle before the recorder launches `windmaint`: slices \
                 become video, expired footage is pruned, the index is corrected and the AI steps run. \
                 This is the whole schedule — there is no service, timer or background helper that would \
                 otherwise wake the disk, and the pass exits when it is done. 0 switches it off, which \
                 means footage stays as JPEG slices until you run `windmaint all` yourself. The ceiling \
                 is one day; a longer wait reads as a broken recorder rather than as a choice."
            }
            RField::SummaryPendingDays => {
                "How many product days one idle summarising run takes on, and the first half of how \
                 long the pass can take. Not everything unsummarised since the library began: an idle \
                 window is borrowed time on a machine you may come back to. Whatever is left is offered \
                 again to the next run, because the queue is derived from the index and not from a \
                 cursor. Past 60 the summariser stops looking for days with outstanding work at all, so \
                 the top of this row would be a slider that does nothing."
            }
            RField::SummaryStretchLimit => {
                "How many stretches that same run asks for — the two rows above together are the \
                 pass's whole budget, in requests, and both are passed to `windai summarize`. The AI \
                 switch that lets any of this spend money is on the AI page. Three per-batch counts the \
                 pass steps through on its other legs (`batch_size_embed_video_in_idle`, \
                 `batch_size_remove_video_in_idle`, `batch_size_compress_video_in_idle`) stay \
                 config-only by decision: each is how many files one step opens at a time rather than how \
                 much work or time the run is allowed, so a row per batch would be four controls for one \
                 decision this page does not need to offer. A person who wants to change them edits the \
                 file, and `Config::save` carries their values through every Save from here."
            }
            RField::DisplayStrategy => {
                "`all` grabs the virtual desktop — every monitor, and the empty space between them \
                 too — and `single` grabs the one panel named below. Upstream only shows the row at \
                 all when more than one display is attached, and so does this panel."
            }
            RField::SingleDisplayIndex => {
                "1-based, matching `mss`'s `monitors[1..]` and every existing user's saved value. \
                 The list beside it is what this machine reported, so an index with no row next to it is \
                 a monitor that is not plugged in right now."
            }
            RField::ForegroundWindowOnly => {
                "Grab the focused window's rectangle instead of the whole display. Far cheaper — a \
                 four-panel desktop is a 17-megapixel union per frame — and it means a second \
                 monitor's activity is never indexed at all."
            }
            RField::VidStoreDay => {
                "Age at which `windmaint` deletes a segment along with its screenshot slice and its \
                 index rows. 0 disables deletion. `day_begin_minutes` decides where a day starts, so \
                 this is not a plain file-age cutoff."
            }
            RField::VidCompressDay => {
                "Age at which a segment is re-encoded smaller but kept. It should sit below `Keep \
                 videos for`, or every file is compressed the same day it would have been deleted."
            }
            RField::CompressRate => {
                "Linear scale applied to the frame on re-encode. Stored as the text `1`, `0.75`, \
                 `0.5` or `0.25`, because the Python settings page looks the value up in a string-keyed \
                 table and a JSON number there breaks the other UI."
            }
            RField::CompressEncoder => {
                "A key of `config_src/video_compress_preset.json`, whose row `windmaint expire` \
                 splices into the re-encode of every segment past `Compress videos older than`. \
                 `windmaint doctor` prints the encoder this resolves to."
            }
            RField::CompressAccelerator => {
                "Which hardware column of that encoder's row to use. Only `cpu` takes the thread \
                 count below, because upstream only passes `-threads` for it. A column is a name \
                 this machine may not be able to honour — `qsv` needs Intel, `nvenc` an NVIDIA card, \
                 `amf` an AMD one — and when ffmpeg refuses to open it `windmaint` says so, names \
                 what ffmpeg answered, and re-encodes on `cpu` instead, so the library still \
                 shrinks. `windmaint doctor` reports which columns actually work here."
            }
            RField::CompressQuality => "CRF for the re-encode. 0..=50 is upstream's own widget range.",
            RField::CompressCpuThreads => {
                "Encoder worker threads, bounded by what this machine reported. Only meaningful \
                 while the accelerator is `cpu`."
            }
            RField::StartOnBoot => {
                "Whether the tray begins a recording the moment it launches. `supervisor`'s \
                 `supervisor.rs` reads it (`start_recording_on_startup`, default on) and only ever \
                 *begins* one there, so a second tray started over a running recorder will not stop \
                 it. Off means the tray comes up idle and recording starts by hand."
            }
        }
    }

    /// The field's contract. `current` is the record being validated (or drawn) so that
    /// [`RField::CompressAccelerator`] can offer exactly the accelerators that exist for the encoder
    /// chosen above it — the coupling `recording.py` gets from
    /// `CONFIG_VIDEO_COMPRESS_PRESET[encoder].keys()`.
    pub fn kind(self, options: &RecOptions, current: &Rec) -> Kind {
        match self {
            // Upstream's widget bounds, verbatim wherever a widget exists.
            RField::RecordBitrate => Kind::Int { min: 50, max: 10_000 },
            RField::ScreenshotInterval => Kind::Int { min: 3, max: 15 },
            RField::CompressQuality => Kind::Int { min: 0, max: 50 },
            RField::RecordSeconds => Kind::Int { min: 60, max: 36_000 },
            RField::RecordFramerate => Kind::Int { min: 1, max: 30 },
            RField::ScreenshotInterruptCount => Kind::Int { min: 1, max: 1_000 },
            RField::IdlePauseMinutes => Kind::Int { min: 0, max: 240 },
            // The same bounds `wind_base::config` clamps to in the accessors these rows write through,
            // restated deliberately: the widget's job is to keep the user out of the correction, and the
            // accessor's job is to keep a hand-edited file out of the engine. `tests::
            // every_idle_pass_row_offers_what_the_accessor_accepts` is the guard that the two stay equal.
            RField::IdleMaintainGap => Kind::Int { min: 0, max: 1_440 },
            RField::SummaryPendingDays => Kind::Int { min: 1, max: 60 },
            RField::SummaryStretchLimit => Kind::Int { min: 1, max: 1_000 },
            // 1 and not 0: `capture::monitor_rect` returns `None` below 1, so an index of 0 is a
            // recorder that captures nothing and says nothing about it.
            RField::SingleDisplayIndex => Kind::Int { min: 1, max: 16 },
            RField::VidStoreDay | RField::VidCompressDay => Kind::Int { min: 0, max: 3_650 },
            RField::CompressCpuThreads => Kind::Int { min: 1, max: options.cpu_cores.max(1) },
            RField::OcrSimilarity | RField::OcrSimilarityInTable => {
                Kind::Fraction { min: 0.0, max: 1.0 }
            }
            RField::ReduceSameContent | RField::ForegroundWindowOnly => Kind::Bool,
            RField::StartOnBoot => Kind::Bool,
            // The native recorder implements exactly one mode. `ffmpeg` is not in this list because
            // `windrec` cannot honour it; see `RField::RecordMode`'s help and the round-trip test
            // that pins the offered set to the values `windrec` actually implements.
            RField::RecordMode => Kind::Choice(vec!["screenshot_array".into()]),
            RField::DisplayStrategy => Kind::Choice(vec!["all".into(), "single".into()]),
            RField::CompressRate => Kind::Choice(vec!["1".into(), "0.75".into(), "0.5".into(), "0.25".into()]),
            RField::RecordEncoder => Kind::Choice(options.record_encoders.clone()),
            RField::CompressEncoder => Kind::Choice(options.encoders()),
            RField::CompressAccelerator => Kind::Choice(options.accelerators(&current.compress_encoder)),
        }
    }
}

/// A field's value in the shape the form holds it.
#[derive(Debug, Clone, PartialEq)]
pub enum RTyped {
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
}

impl Rec {
    pub fn load(config: &Config) -> Rec {
        let base = Rec::default();
        Rec {
            record_mode: config.str_or("record_mode", &base.record_mode),
            record_seconds: config.i64_or("record_seconds", base.record_seconds),
            record_framerate: config.i64_or("record_framerate", base.record_framerate),
            record_bitrate: config.i64_or("record_bitrate", base.record_bitrate),
            record_encoder: config.str_or("record_encoder", &base.record_encoder),
            screenshot_interval_second: config.i64_or("screenshot_interval_second", base.screenshot_interval_second),
            screenshot_interrupt_recording_count: config
                .i64_or("screenshot_interrupt_recording_count", base.screenshot_interrupt_recording_count),
            ocr_compare_similarity: config.f64_or("ocr_compare_similarity", base.ocr_compare_similarity),
            ocr_compare_similarity_in_table: config
                .f64_or("ocr_compare_similarity_in_table", base.ocr_compare_similarity_in_table),
            index_reduce_same_content_at_different_time: config
                .bool_or("index_reduce_same_content_at_different_time", base.index_reduce_same_content_at_different_time),
            screentime_not_change_to_pause_record: config
                .i64_or("screentime_not_change_to_pause_record", base.screentime_not_change_to_pause_record),
            // The three idle-pass rows read through `Config`'s own accessors rather than through
            // `i64_or(key, base.x)`. That is the difference between a form that *shows* a setting and one
            // that shows what the engine will do with it: the accessor holds the default and the clamp, so
            // a hand-edited 900-minute gap is drawn as the 1440 the recorder counts with, and the next
            // Save writes 1440 back rather than round-tripping a value nothing honours.
            idle_maintain_time_gap: config.idle_maintain_gap_minutes(),
            summary_pending_days_in_idle: config.summary_pending_days_in_idle(),
            summary_stretch_limit_in_idle: config.summary_stretch_limit_in_idle(),
            multi_display_record_strategy: config.str_or("multi_display_record_strategy", &base.multi_display_record_strategy),
            record_single_display_index: config.i64_or("record_single_display_index", base.record_single_display_index),
            record_screenshot_method_capture_foreground_window_only: config
                .bool_or(
                    "record_screenshot_method_capture_foreground_window_only",
                    base.record_screenshot_method_capture_foreground_window_only,
                ),
            // Read with the *reader's* own default, not this struct's: the tray's `supervisor.rs`
            // does `bool_or("start_recording_on_startup", true)`. The two screens must not disagree
            // about what an absent key means, or this page shows a switch set the way the tray
            // already behaves and a Save silently flips it.
            start_recording_on_startup: config
                .bool_or("start_recording_on_startup", base.start_recording_on_startup),
            vid_store_day: config.i64_or("vid_store_day", base.vid_store_day),
            vid_compress_day: config.i64_or("vid_compress_day", base.vid_compress_day),
            compress_encoder: config.str_or("compress_encoder", &base.compress_encoder),
            compress_accelerator: config.str_or("compress_accelerator", &base.compress_accelerator),
            compress_quality: config.i64_or("compress_quality", base.compress_quality),
            compress_cpu_threads: config.i64_or("compress_cpu_threads", base.compress_cpu_threads),
            // Read with the *recorder's* own default, not this struct's: `windrec`'s plan does
            // `config.bool_or("record_deep_linking", true)`, so on an install that never mentions the
            // key the recorder believes it is recording addresses. The two screens must not disagree
            // about what an absent key means, or this page stays silent in exactly the case that
            // needs saying.
            deep_linking_promised: config.bool_or("record_deep_linking", true),
            // The same read-the-file-as-the-reader-does discipline as the promise above, but for a
            // key this page refuses to edit: it is only carried so `view::recording` can warn when a
            // user set a battery gate that the native engine will ignore. Default `0` (the shipped
            // value) means "no gate asked for", matching what the recorder does — it always converts.
            energy_saving_requested: config.i64_or("convert_screenshots_to_vid_energy_saving_mode", 0) != 0,
            // Normalised to the file's own text spelling rather than to a float: `1.0` and `1` are
            // the same scale but only one of them is a key of the Python table.
            video_compress_rate: match config.str_or("video_compress_rate", &base.video_compress_rate).as_str() {
                "1" | "1.0" => "1".into(),
                other => other.into(),
            },
        }
    }

    pub fn get(&self, field: RField) -> RTyped {
        match field {
            RField::RecordMode => RTyped::Text(self.record_mode.clone()),
            RField::RecordSeconds => RTyped::Int(self.record_seconds),
            RField::RecordFramerate => RTyped::Int(self.record_framerate),
            RField::RecordBitrate => RTyped::Int(self.record_bitrate),
            RField::RecordEncoder => RTyped::Text(self.record_encoder.clone()),
            RField::ScreenshotInterval => RTyped::Int(self.screenshot_interval_second),
            RField::ScreenshotInterruptCount => RTyped::Int(self.screenshot_interrupt_recording_count),
            RField::OcrSimilarity => RTyped::Float(self.ocr_compare_similarity),
            RField::OcrSimilarityInTable => RTyped::Float(self.ocr_compare_similarity_in_table),
            RField::ReduceSameContent => RTyped::Bool(self.index_reduce_same_content_at_different_time),
            RField::IdlePauseMinutes => RTyped::Int(self.screentime_not_change_to_pause_record),
            RField::IdleMaintainGap => RTyped::Int(self.idle_maintain_time_gap),
            RField::SummaryPendingDays => RTyped::Int(self.summary_pending_days_in_idle),
            RField::SummaryStretchLimit => RTyped::Int(self.summary_stretch_limit_in_idle),
            RField::DisplayStrategy => RTyped::Text(self.multi_display_record_strategy.clone()),
            RField::SingleDisplayIndex => RTyped::Int(self.record_single_display_index),
            RField::ForegroundWindowOnly => RTyped::Bool(self.record_screenshot_method_capture_foreground_window_only),
            RField::VidStoreDay => RTyped::Int(self.vid_store_day),
            RField::VidCompressDay => RTyped::Int(self.vid_compress_day),
            RField::CompressRate => RTyped::Text(self.video_compress_rate.clone()),
            RField::CompressEncoder => RTyped::Text(self.compress_encoder.clone()),
            RField::CompressAccelerator => RTyped::Text(self.compress_accelerator.clone()),
            RField::CompressQuality => RTyped::Int(self.compress_quality),
            RField::CompressCpuThreads => RTyped::Int(self.compress_cpu_threads),
            RField::StartOnBoot => RTyped::Bool(self.start_recording_on_startup),
        }
    }

    fn set(&mut self, field: RField, value: RTyped) {
        match (field, value) {
            (RField::RecordMode, RTyped::Text(v)) => self.record_mode = v,
            (RField::RecordSeconds, RTyped::Int(v)) => self.record_seconds = v,
            (RField::RecordFramerate, RTyped::Int(v)) => self.record_framerate = v,
            (RField::RecordBitrate, RTyped::Int(v)) => self.record_bitrate = v,
            (RField::RecordEncoder, RTyped::Text(v)) => self.record_encoder = v,
            (RField::ScreenshotInterval, RTyped::Int(v)) => self.screenshot_interval_second = v,
            (RField::ScreenshotInterruptCount, RTyped::Int(v)) => self.screenshot_interrupt_recording_count = v,
            (RField::OcrSimilarity, RTyped::Float(v)) => self.ocr_compare_similarity = v,
            (RField::OcrSimilarityInTable, RTyped::Float(v)) => self.ocr_compare_similarity_in_table = v,
            (RField::ReduceSameContent, RTyped::Bool(v)) => self.index_reduce_same_content_at_different_time = v,
            (RField::IdlePauseMinutes, RTyped::Int(v)) => self.screentime_not_change_to_pause_record = v,
            (RField::IdleMaintainGap, RTyped::Int(v)) => self.idle_maintain_time_gap = v,
            (RField::SummaryPendingDays, RTyped::Int(v)) => self.summary_pending_days_in_idle = v,
            (RField::SummaryStretchLimit, RTyped::Int(v)) => self.summary_stretch_limit_in_idle = v,
            (RField::DisplayStrategy, RTyped::Text(v)) => self.multi_display_record_strategy = v,
            (RField::SingleDisplayIndex, RTyped::Int(v)) => self.record_single_display_index = v,
            (RField::ForegroundWindowOnly, RTyped::Bool(v)) => self.record_screenshot_method_capture_foreground_window_only = v,
            (RField::VidStoreDay, RTyped::Int(v)) => self.vid_store_day = v,
            (RField::VidCompressDay, RTyped::Int(v)) => self.vid_compress_day = v,
            (RField::CompressRate, RTyped::Text(v)) => self.video_compress_rate = v,
            (RField::CompressEncoder, RTyped::Text(v)) => self.compress_encoder = v,
            (RField::CompressAccelerator, RTyped::Text(v)) => self.compress_accelerator = v,
            (RField::CompressQuality, RTyped::Int(v)) => self.compress_quality = v,
            (RField::CompressCpuThreads, RTyped::Int(v)) => self.compress_cpu_threads = v,
            (RField::StartOnBoot, RTyped::Bool(v)) => self.start_recording_on_startup = v,
            // A field/value mismatch is a programming error in `RField::ALL`, not user input.
            _ => unreachable!("{} cannot hold a value of another kind", field.key()),
        }
    }

    /// Stage every key. `Config::save` is what hits the disk, and it writes the merged map, so the
    /// fifteen keys `settings.rs` owns and the hundred the recorder keeps ride along untouched.
    ///
    /// The list is explicit, and that is the point. Two recorder keys are *not* in it even though
    /// [`Rec`] carries them — `record_deep_linking` (via [`Rec::deep_linking_promised`]) because a
    /// form must not write back a switch it offers no way to honour, and
    /// `convert_screenshots_to_vid_energy_saving_mode` (via [`Rec::energy_saving_requested`]) because
    /// the native engine ignores it and the only value of writing it here would be to clobber a
    /// Python-set gate. A third, `record_crf`, is not in the list *or* in [`Rec`], which is the
    /// `use_native_core` treatment: it is read by `windmaint` and discarded by every preset the
    /// payload ships, so the control is gone and the key is left exactly where the file has it. All
    /// three ride the merged map back out exactly as found. See [`RField::ALL`].
    pub fn stage(&self, config: &mut Config) {
        config.set("record_mode", Value::String(self.record_mode.clone()));
        config.set("record_seconds", Value::from(self.record_seconds));
        config.set("record_framerate", Value::from(self.record_framerate));
        config.set("record_bitrate", Value::from(self.record_bitrate));
        config.set("record_encoder", Value::String(self.record_encoder.clone()));
        config.set("screenshot_interval_second", Value::from(self.screenshot_interval_second));
        config.set(
            "screenshot_interrupt_recording_count",
            Value::from(self.screenshot_interrupt_recording_count),
        );
        config.set("ocr_compare_similarity", Value::from(self.ocr_compare_similarity));
        config.set("ocr_compare_similarity_in_table", Value::from(self.ocr_compare_similarity_in_table));
        config.set(
            "index_reduce_same_content_at_different_time",
            Value::Bool(self.index_reduce_same_content_at_different_time),
        );
        config.set(
            "screentime_not_change_to_pause_record",
            Value::from(self.screentime_not_change_to_pause_record),
        );
        // The idle pass's three keys. Written here, read by two other processes: `windrec`'s
        // `plan_from` takes the gap through `Config::idle_maintain_gap_minutes` and `windmaint`'s
        // `schedule::summary_budget` takes the two ceilings through theirs, and `windmcp` reports the
        // same pair on its summary queue. A Save is therefore the whole of the schedule's user-facing
        // door — there is no third store of these numbers and nothing resident that has to be told.
        config.set("idle_maintain_time_gap", Value::from(self.idle_maintain_time_gap));
        config.set("summary_pending_days_in_idle", Value::from(self.summary_pending_days_in_idle));
        config.set("summary_stretch_limit_in_idle", Value::from(self.summary_stretch_limit_in_idle));
        config.set("multi_display_record_strategy", Value::String(self.multi_display_record_strategy.clone()));
        config.set("record_single_display_index", Value::from(self.record_single_display_index));
        config.set(
            "record_screenshot_method_capture_foreground_window_only",
            Value::Bool(self.record_screenshot_method_capture_foreground_window_only),
        );
        config.set("vid_store_day", Value::from(self.vid_store_day));
        config.set("vid_compress_day", Value::from(self.vid_compress_day));
        config.set("video_compress_rate", Value::String(self.video_compress_rate.clone()));
        config.set("compress_encoder", Value::String(self.compress_encoder.clone()));
        config.set("compress_accelerator", Value::String(self.compress_accelerator.clone()));
        config.set("compress_quality", Value::from(self.compress_quality));
        config.set("compress_cpu_threads", Value::from(self.compress_cpu_threads));
        // `use_native_core` is deliberately absent from this list. It chose between the native
        // recorder and the Python one; the Python application was deleted in 3f37cbf and
        // `supervisor/src/native.rs` now returns a native command whatever the file says, so nothing
        // reads the key at all. Writing it here would keep persisting a value that changes nothing —
        // the defect class this branch keeps eliminating, and worse than no setting because the user
        // believes they configured something. `tests::a_save_never_writes_the_dead_use_native_core_key`
        // is the guard; put the line back and it fails.
        //
        // `start_recording_on_startup` is still written as a real JSON boolean rather than the text
        // of one: `supervisor.rs` reads it with `bool_or`, which takes a `Value::Bool` straight and
        // only coerces the three exact strings "true"/"1"/"yes". A bool is the honest spelling.
        config.set("start_recording_on_startup", Value::Bool(self.start_recording_on_startup));
    }
}

/// What the user is typing, per field, kept apart from the parsed value for the reason
/// `settings.rs` states: a number mid-deletion is not zero, and text that will not parse must not
/// silently take the previous value's place.
#[derive(Debug, Clone)]
pub struct RecDraft(BTreeMap<RField, String>);

impl RecDraft {
    pub fn from(rec: &Rec) -> RecDraft {
        let mut raw = BTreeMap::new();
        for field in RField::ALL {
            raw.insert(field, render(rec.get(field)));
        }
        RecDraft(raw)
    }

    pub fn text(&self, field: RField) -> &str {
        self.0.get(&field).map(String::as_str).unwrap_or("")
    }

    pub fn set_text(&mut self, field: RField, text: &str) {
        self.0.insert(field, text.to_string());
    }

    pub fn bool_of(&self, field: RField) -> bool {
        matches!(self.text(field), "true")
    }

    /// Parse and clamp every field. `notes` is the reason a value had to be corrected — empty means
    /// the draft is exactly what will be written.
    ///
    /// Fields are visited in [`RField::ALL`]'s order, which is deliberately encoder-before-accelerator
    /// so the second one's option list can be the first one's, as validated.
    pub fn validate(&self, base: &Rec, options: &RecOptions) -> (Rec, Vec<String>) {
        let mut out = base.clone();
        let mut notes = Vec::new();
        for field in RField::ALL {
            apply(field, self.text(field), base, options, &mut out, &mut notes);
        }
        (out, notes)
    }
}

fn render(value: RTyped) -> String {
    match value {
        RTyped::Int(v) => v.to_string(),
        RTyped::Float(v) => format!("{v}"),
        RTyped::Bool(v) => v.to_string(),
        RTyped::Text(v) => v,
    }
}

fn apply(field: RField, raw: &str, base: &Rec, options: &RecOptions, into: &mut Rec, notes: &mut Vec<String>) {
    let kept = || render(base.get(field));
    let typed = match field.kind(options, into) {
        Kind::Int { min, max } => {
            let trimmed = raw.trim();
            match trimmed.parse::<i64>() {
                // Rejection, not silence: the field keeps its previous value and says so.
                Err(_) => {
                    notes.push(format!("{}: '{trimmed}' is not a whole number, kept {}", field.label(), kept()));
                    return;
                }
                Ok(parsed) => {
                    let clamped = parsed.clamp(min, max);
                    if clamped != parsed {
                        notes.push(format!(
                            "{}: {parsed} is outside {min}..={max}, clamped to {clamped}",
                            field.label()
                        ));
                    }
                    RTyped::Int(clamped)
                }
            }
        }
        Kind::Fraction { min, max } => {
            let trimmed = raw.trim();
            match trimmed.parse::<f64>() {
                Err(_) => {
                    notes.push(format!("{}: '{trimmed}' is not a number, kept {}", field.label(), kept()));
                    return;
                }
                Ok(parsed) => {
                    let clamped = parsed.clamp(min, max);
                    if (clamped - parsed).abs() > f64::EPSILON {
                        notes.push(format!(
                            "{}: {parsed} is outside {min}..={max}, clamped to {clamped}",
                            field.label()
                        ));
                    }
                    RTyped::Float(clamped)
                }
            }
        }
        Kind::Bool => RTyped::Bool(raw.trim() == "true"),
        Kind::Choice(list) => {
            let value = raw.trim().to_string();
            if list.iter().any(|known| *known == value) {
                RTyped::Text(value)
            } else {
                // Not a rejection that loses data: the loaded value survives, which is how a config
                // naming an encoder someone added to the preset file by hand keeps working after a
                // Save from a form whose list was read before that edit.
                notes.push(format!(
                    "{}: '{value}' is not one of {}; kept {}",
                    field.label(),
                    if list.is_empty() {
                        "any known option — the preset file appears to be missing".to_string()
                    } else {
                        list.join(", ")
                    },
                    kept()
                ));
                return;
            }
        }
    };
    into.set(field, typed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of this page has a catalog name, a help line and a section heading, in all three shipped
    /// locales — and none of them needs the Rust fallback to appear. `settings.rs` and `ai.rs` carry the
    /// same guard for their own forms; this is the one that would have caught the Recording page reading
    /// English on a Chinese install.
    #[test]
    fn every_recording_row_names_itself_in_the_catalog() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(std::path::Path::parent).unwrap();
        for locale in ["en", "sc", "ja"] {
            let catalog = wind_base::i18n::Catalog::load(&root, locale);
            assert!(catalog.loaded(), "{locale}: {:?}", catalog.read_error);
            for field in RField::ALL {
                assert_ne!(field.label_key(), field.help_key(), "{field:?} shares one key for both");
                for key in [field.label_key(), field.help_key(), field.group_key()] {
                    assert_ne!(
                        catalog.text(key),
                        wind_base::i18n::missing(key),
                        "the shipped {locale} catalog has no row for {} ({key})",
                        field.key()
                    );
                }
                assert_eq!(
                    catalog.text_or(field.label_key(), field.label()),
                    catalog.text(field.label_key()),
                    "{:?} still needs the Rust fallback",
                    field.key()
                );
            }
        }
    }

    fn options() -> RecOptions {
        RecOptions { record_encoders: vec!["cpu_h264".into(), "NVIDIA_h265".into()], compress: RecOptions::default().compress, cpu_cores: 8 }
    }

    /// The option set a real boot reads off the shipped preset files, rather than the two-name list
    /// `options()` keeps for the tests that assert on a refused encoder by name. The core count is
    /// `options()`', not `RecOptions::default()`'s `1`, because the stock `compress_cpu_threads` is 2
    /// and a one-core fixture would clamp it and every note-free assertion below would trip on it.
    fn options_with_the_shipped_encoders() -> RecOptions {
        RecOptions { cpu_cores: 8, ..RecOptions::default() }
    }

    /// A scratch install root whose `config_default.json` is exactly `defaults`, for the tests that
    /// must ask what `Rec::load` makes of a key rather than assert what a struct literal holds.
    fn root_holding(defaults: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("windui-record-deep-{}", crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("windrecorder/config_src")).unwrap();
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        std::fs::write(dir.join("windrecorder/config_src/config_default.json"), defaults).unwrap();
        dir
    }

    /// The recorder reads `record_deep_linking` defaulting to *on*, and the one line it prints about
    /// that goes to a stderr nobody watching a tray icon can read, into a log that is truncated at the
    /// next start. So this page has to read the key exactly as the recorder does: agree about what an
    /// absent key means, or the notice stays silent on precisely the installs that need it.
    #[test]
    fn an_absent_record_deep_linking_is_a_promise_because_the_recorder_reads_it_that_way() {
        let absent = root_holding("{}");
        let silent = Rec::load(&Config::load(&absent).unwrap());
        assert!(silent.deep_linking_promised, "no key in the file, and `windrec`'s plan still defaults it to on");
        let _ = std::fs::remove_dir_all(absent);

        let declined_dir = root_holding(r#"{"record_deep_linking": false}"#);
        let declined = Rec::load(&Config::load(&declined_dir).unwrap());
        assert!(!declined.deep_linking_promised, "an explicit false is honoured, and the notice goes off");
        let _ = std::fs::remove_dir_all(declined_dir);

        let promised_dir = root_holding(r#"{"record_deep_linking": true}"#);
        let promised = Rec::load(&Config::load(&promised_dir).unwrap());
        assert!(promised.deep_linking_promised, "and an explicit true is the same answer, not a weaker one");
        let _ = std::fs::remove_dir_all(promised_dir);

        // A `Rec` nobody read out of a file promises nothing. That is a test's starting state, not any
        // user's screen — every real one goes through `load`, which is where the promise is made.
        assert!(!Rec::default().deep_linking_promised);
    }

    /// A carried field is not a widget, and that has to be true rather than merely intended:
    /// `RecDraft::from`, `validate` and the panel's own loop all walk [`RField::ALL`], while `stage`
    /// names its keys one by one. A field outside all of them cannot reach the file.
    #[test]
    fn the_promise_is_no_widget_of_its_own_and_survives_validation_unchanged() {
        assert!(
            !RField::ALL.iter().any(|field| field.key() == "record_deep_linking"),
            "a setting this page cannot honour must not be offered as one it can change"
        );
        let base = Rec { deep_linking_promised: true, ..Rec::default() };
        let mut draft = RecDraft::from(&base);
        assert_eq!(RField::ALL.len(), 25, "twenty-two recording keys and the three that schedule the idle pass");
        assert_eq!(draft.0.len(), RField::ALL.len(), "the draft holds exactly one entry per editable field");
        draft.set_text(RField::RecordSeconds, "601");
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(parsed.record_seconds, 601, "the field that was edited did change");
        assert!(parsed.deep_linking_promised, "validation carries the promise through instead of resetting it");
    }

    /// `stage`'s list is the promise the panel states out loud in the Save tooltip — *every other key
    /// round-trips exactly as it was read* — and this is the key that proves the list is deliberate
    /// rather than a coincidence. The `Rec` here carries a promise and the file does not; if a future
    /// write-back "helpfully" staged what it was holding, a Save would switch a dead feature on.
    #[test]
    fn stage_does_not_write_record_deep_linking_even_while_carrying_a_promise() {
        let dir = root_holding("{}");
        let mut config = Config::load(&dir).unwrap();
        let rec = Rec { deep_linking_promised: true, ..Rec::default() };
        rec.stage(&mut config);
        config.save().unwrap();

        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(!raw.contains("record_deep_linking"), "the form wrote a key it does not own: {raw}");
        let after = Config::load(&dir).unwrap();
        assert!(Rec::load(&after).deep_linking_promised, "a load after the Save still reads the recorder's own default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The three new rows are not decoration, and "the page writes the key the engine reads" is the only
    /// promise that makes them controls rather than costumes. So the test goes out through `stage`, into a
    /// real file, and back through the accessors `windrec`'s `plan_from` and `windmaint`'s `summary_budget`
    /// actually call — with a fourth assertion nobody asked for: that the page owns exactly these three
    /// keys and no others in the idle-pass group, because a row that staged nothing would pass a test that
    /// only read values back.
    #[test]
    fn the_idle_pass_rows_write_the_keys_the_recorder_and_the_pass_read() {
        let dir = root_holding("{}");
        let mut config = Config::load(&dir).unwrap();
        let mut rec = Rec::load(&config);
        rec.idle_maintain_time_gap = 12;
        rec.summary_pending_days_in_idle = 4;
        rec.summary_stretch_limit_in_idle = 250;
        rec.stage(&mut config);
        config.save().unwrap();

        let back = Config::load(&dir).unwrap();
        assert_eq!(back.idle_maintain_gap_minutes(), 12, "the gap `windrec` counts idle minutes with");
        assert_eq!(back.summary_pending_days_in_idle(), 4, "the `--pending` `windmaint` passes to `windai`");
        assert_eq!(back.summary_stretch_limit_in_idle(), 250, "and its `--limit`");
        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        for key in [RField::IdleMaintainGap, RField::SummaryPendingDays, RField::SummaryStretchLimit] {
            assert!(raw.contains(&format!("\"{}\":", key.key())), "{} was never staged: {raw}", key.key());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where a row stops, the accessor must stop — or the page offers a number the engine quietly
    /// corrects, which is the shape of every dead control this branch has removed.
    #[test]
    fn every_idle_pass_row_bounds_what_the_accessor_clamps_to() {
        for (field, min, max) in [
            (RField::IdleMaintainGap, 0, 1_440),
            (RField::SummaryPendingDays, 1, 60),
            (RField::SummaryStretchLimit, 1, 1_000),
        ] {
            let (lo, hi) = match field.kind(&options(), &Rec::default()) {
                Kind::Int { min, max } => (min, max),
                other => panic!("{field:?} is not a bounded number row: {other:?}"),
            };
            assert_eq!((lo, hi), (min, max), "{field:?}'s widget bound drifted from the accessor's");
        }

        // And the clamp is not merely documented: a hand-edited file answers with the ceiling the row
        // would have written, so the number on screen and the number in the engine are the same number.
        let dir = root_holding(
            r#"{"idle_maintain_time_gap": 5000, "summary_pending_days_in_idle": 500, "summary_stretch_limit_in_idle": 0}"#,
        );
        let rec = Rec::load(&Config::load(&dir).unwrap());
        assert_eq!(rec.idle_maintain_time_gap, 1_440, "a day is as long a wait as the row offers");
        assert_eq!(rec.summary_pending_days_in_idle, 60, "past the summariser's scan-back horizon is one ceiling");
        assert_eq!(rec.summary_stretch_limit_in_idle, 1, "and a run of no stretches is not a run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The days row's ceiling is not this page's number. `Config::summary_pending_days_in_idle` documents
    /// it as `wind_ai::summarize::PENDING_SCAN_DAYS` — the horizon past which the summariser stops
    /// looking for days with outstanding work at all — and `base/src/config.rs` goes on to say that
    /// *this page* pins the two together. This is that pin.
    ///
    /// It is worth a test rather than a sentence because the drift is silent in both directions: raise
    /// `PENDING_SCAN_DAYS` upstream and the top of this row quietly stops buying anything, lower it and
    /// the row starts offering a number the engine truncates without saying so.
    #[test]
    fn the_days_row_stops_where_the_summariser_stops_looking_back() {
        let ceiling = wind_ai::summarize::PENDING_SCAN_DAYS as i64;
        let (lo, hi) = match RField::SummaryPendingDays.kind(&options(), &Rec::default()) {
            Kind::Int { min, max } => (min, max),
            other => panic!("{:?} is not a bounded number row: {other:?}", RField::SummaryPendingDays),
        };
        assert_eq!(hi, ceiling, "the row's ceiling drifted from the summariser's own scan-back horizon");
        assert_eq!(lo, 1, "and a run over no days is not a run");

        // The accessor is the one that clamps, so a hand-edited file past the horizon answers with this
        // row's own top rather than with a number `windai` would truncate.
        let dir = root_holding(&format!(r#"{{"summary_pending_days_in_idle": {}}}"#, ceiling * 3));
        let rec = Rec::load(&Config::load(&dir).unwrap());
        assert_eq!(rec.summary_pending_days_in_idle, ceiling, "the page and the pass stop at the same day");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Zero is a position on this row, not a rejected value: it is how the pass is switched off, and a
    /// widget floor of 1 would take that choice away while looking like a guard.
    #[test]
    fn an_idle_gap_of_zero_is_saved_as_off_rather_than_corrected_to_one() {
        let dir = root_holding("{}");
        let mut config = Config::load(&dir).unwrap();
        let base = Rec::load(&config);
        let mut draft = RecDraft::from(&base);
        draft.set_text(RField::IdleMaintainGap, "0");
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "switching the pass off is not a correction: {notes:?}");
        assert_eq!(parsed.idle_maintain_time_gap, 0);
        parsed.stage(&mut config);
        config.save().unwrap();
        assert_eq!(Config::load(&dir).unwrap().idle_maintain_gap_minutes(), 0, "and the recorder agrees");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Statistics page's "hours" figure is derived from two rows on *this* page, and a label may not
    /// promise more than the measurement. Both parents now say so, in the catalog as well as in Rust,
    /// because a confession only the maintainer reads is a confession the user does not.
    #[test]
    fn the_two_rows_that_move_the_hours_figure_say_that_they_do() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(std::path::Path::parent).unwrap();
        let catalog = wind_base::i18n::Catalog::load(&root, "en");
        for field in [RField::RecordSeconds, RField::IdlePauseMinutes] {
            let help = catalog.text(field.help_key());
            assert_ne!(help, wind_base::i18n::missing(field.key()), "{} has no catalog help row", field.key());
            assert!(
                help.contains("hours"),
                "{}'s help must name the figure it moves: {help}",
                field.key()
            );
            assert!(field.help().contains("hours"), "{}'s Rust fallback must say it too", field.key());
        }
        // The two rows are not the answer to the question; the accessor is, and it is derived.
        let config = Config::load(&root).unwrap();
        let gap = config.presence_gap_secs();
        let pause_floor = config.i64_or("screentime_not_change_to_pause_record", 5) * 60;
        assert!(gap >= pause_floor, "the ruler is at least as long as the pause threshold: {gap} < {pause_floor}");
    }

    #[test]
    fn every_field_round_trips_through_its_own_draft_text() {
        let base = Rec::default();
        let draft = RecDraft::from(&base);
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "a pristine draft must not complain: {notes:?}");
        assert_eq!(parsed, base);
    }

    /// The write must be a round trip through the *config*, not just through the struct: `windrec`
    /// and `windmaint` read these keys back with the accessors they chose, and a fraction written as
    /// a string or an integer written as a float would come back as the default on that side.
    #[test]
    fn staged_values_survive_a_write_and_a_read_by_the_other_accessors() {
        let dir = std::env::temp_dir().join(format!("windui-record-rt-{}", crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("windrecorder/config_src")).unwrap();
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        std::fs::write(dir.join("windrecorder/config_src/config_default.json"), "{}").unwrap();

        let mut config = Config::load(&dir).unwrap();
        // Loaded from the file rather than built from the struct's own defaults, because one field of
        // `Rec` is *only* ever read out of the file (`deep_linking_promised`) and this test's last
        // assertion compares against a second load. Every staged field is still the shipped default
        // here — the config this fixture writes is `{}` — so nothing about what is being proved moves.
        let mut rec = Rec::load(&config);
        rec.record_seconds = 600;
        rec.record_screenshot_method_capture_foreground_window_only = false;
        rec.start_recording_on_startup = false;
        rec.video_compress_rate = "0.25".into();
        rec.stage(&mut config);
        config.save().unwrap();

        let back = Rec::load(&Config::load(&dir).unwrap());
        assert_eq!(back, rec, "what was written is what the recorder will read");
        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(raw.contains("\"video_compress_rate\": \"0.25\""), "the scale stays text: {raw}");
        assert!(raw.contains("\"start_recording_on_startup\": false"), "the switch is a JSON bool: {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The row is gone; the key is not. A `config_user.json` that carries
    /// `screenshot_compare_similarity` keeps its value through a Save of everything else, because the
    /// form writes a merged map and the Python recorder still honours the number. Deleting a control
    /// must never turn into deleting what the user had typed.
    #[test]
    fn a_key_no_longer_offered_rides_through_a_save_of_the_rest() {
        let dir = std::env::temp_dir().join(format!("windrec-orphan-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            b"{\"screenshot_compare_similarity\": 0.42}",
        )
        .unwrap();

        let mut config = Config::load(&dir).unwrap();
        let mut draft = Rec::load(&config);
        draft.record_seconds = 700;
        draft.stage(&mut config);
        config.save().unwrap();

        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(raw.contains("\"screenshot_compare_similarity\": 0.42"), "the unoffered key survives: {raw}");
        assert!(raw.contains("\"record_seconds\": 700"), "and the offered one was written: {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_input_is_clamped_and_explained() {
        let base = Rec::default();
        let mut draft = RecDraft::from(&base);
        draft.set_text(RField::RecordBitrate, "99999");
        draft.set_text(RField::OcrSimilarityInTable, "1.9");
        draft.set_text(RField::CompressCpuThreads, "64");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.record_bitrate, 10_000);
        assert_eq!(parsed.ocr_compare_similarity_in_table, 1.0);
        assert_eq!(parsed.compress_cpu_threads, 8, "bounded by what the machine reported");
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes.iter().all(|n| n.contains("clamped")), "{notes:?}");
    }

    #[test]
    fn unparseable_input_keeps_the_previous_value_and_says_why() {
        let base = Rec::default();
        let mut draft = RecDraft::from(&base);
        draft.set_text(RField::VidStoreDay, "forever");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.vid_store_day, 1200);
        assert!(notes[0].contains("not a whole number"), "{notes:?}");
    }

    #[test]
    fn an_encoder_that_is_not_in_the_preset_file_is_refused_rather_than_written() {
        let base = Rec::default();
        let mut draft = RecDraft::from(&base);
        draft.set_text(RField::RecordEncoder, "gpu_that_does_not_exist");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.record_encoder, "cpu_h264", "the loaded value survives");
        assert!(notes[0].contains("cpu_h264, NVIDIA_h265"), "the note lists what is known: {notes:?}");
    }

    #[test]
    fn the_accelerator_offers_only_what_the_chosen_encoder_has() {
        let options = RecOptions {
            // The record-encoder list is populated even though this test is about the *compress*
            // accelerator: `Rec::default()` names a record encoder, and validating it against an
            // empty list is the broken-install case the other test asserts on deliberately.
            record_encoders: vec!["cpu_h264".into(), "NVIDIA_h265".into()],
            compress: vec![("x264".into(), vec!["cpu".into(), "qsv".into()]), ("av1".into(), vec!["cpu".into()])],
            cpu_cores: 4,
        };
        let base = Rec { compress_encoder: "av1".into(), compress_accelerator: "cpu".into(), ..Rec::default() };
        let Kind::Choice(list) = RField::CompressAccelerator.kind(&options, &base) else {
            panic!("the accelerator is a choice");
        };
        assert_eq!(list, vec!["cpu".to_string()], "av1 has no qsv row to point at");

        // And a draft that moved the encoder moves the accelerator's contract with it, because
        // `RField::ALL` puts the encoder first in the validation order.
        let mut draft = RecDraft::from(&base);
        draft.set_text(RField::CompressEncoder, "x264");
        draft.set_text(RField::CompressAccelerator, "qsv");
        let (parsed, notes) = draft.validate(&base, &options);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!((parsed.compress_encoder.as_str(), parsed.compress_accelerator.as_str()), ("x264", "qsv"));
    }

    /// The lying control that was here offered `ffmpeg`, a mode no native binary implements. The fix
    /// is that the offered set is now exactly the one mode `windrec` records in, and this is the test
    /// that fails the moment `ffmpeg` is put back on the menu.
    #[test]
    fn the_native_recorder_offers_no_mode_it_does_not_implement() {
        let base = Rec::default();
        let Kind::Choice(offered) = RField::RecordMode.kind(&options(), &base) else {
            panic!("record mode is a choice");
        };
        assert_eq!(offered, vec!["screenshot_array".to_string()], "the only mode the native grabber implements");
        assert!(!offered.iter().any(|m| m == "ffmpeg"), "`windrec` cannot honour ffmpeg, so it must not be offered");
    }

    /// The mirror half of removing `ffmpeg`: a config that already names it — because the user set it
    /// through the Python recorder, which did honour it before this branch deleted it — must survive a Save from this page rather
    /// than be silently rewritten to `screenshot_array`. Narrowing the choice may not corrupt a
    /// Python install. And a stock config needs no such note, because its value is on the list.
    #[test]
    fn a_persisted_ffmpeg_survives_a_save_but_screenshot_array_needs_no_note() {
        let dir = root_holding(r#"{"record_mode": "ffmpeg"}"#);
        let config = Config::load(&dir).unwrap();
        let rec = Rec::load(&config);
        assert_eq!(rec.record_mode, "ffmpeg", "the page reads the value the Python recorder left");

        let draft = RecDraft::from(&rec);
        let (parsed, notes) = draft.validate(&rec, &options());
        assert_eq!(parsed.record_mode, "ffmpeg", "the kept value wins over an option not on the list");
        assert!(
            notes.iter().any(|n| n.contains("ffmpeg") && n.contains("kept")),
            "and the user is told, not silently overridden: {notes:?}"
        );

        let mut config = Config::load(&dir).unwrap();
        parsed.stage(&mut config);
        config.save().unwrap();
        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(raw.contains("\"record_mode\": \"ffmpeg\""), "the Python recorder's mode is untouched: {raw}");
        let _ = std::fs::remove_dir_all(&dir);

        // A pristine default page records `screenshot_array`, which IS on the list, so it saves clean.
        let base = Rec::default();
        let (clean, notes) = RecDraft::from(&base).validate(&base, &options());
        assert_eq!(clean.record_mode, "screenshot_array");
        assert!(notes.is_empty(), "nothing to warn about on a native config: {notes:?}");
    }

    /// The surviving switch must default to exactly what its reader defaults to, or a config that
    /// never mentions the key shows a checkbox the tray is not honouring. `supervisor.rs` treats an
    /// absent `start_recording_on_startup` as `true`. If the default drifts here, this fails.
    #[test]
    fn the_startup_switch_defaults_the_way_its_reader_does() {
        // Supervisor's own literal, copied here as the contract it encodes.
        const START_ON_BOOT_READER_DEFAULT: bool = true; // supervisor.rs: bool_or(.., true)

        assert_eq!(Rec::default().start_recording_on_startup, START_ON_BOOT_READER_DEFAULT);

        let dir = root_holding("{}");
        let absent = Rec::load(&Config::load(&dir).unwrap());
        assert_eq!(absent.start_recording_on_startup, START_ON_BOOT_READER_DEFAULT, "an absent key is auto-start-on");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The round trip this whole file exists to guarantee: set it through the draft, validate, stage,
    /// write, reload, and read it back through the *exact accessor and default the tray uses*. If the
    /// key name, the merge, or the boolean encoding breaks, the value stops reaching `supervisor.rs`
    /// and this test goes red — which is the whole point, because the bug this page keeps dying of is
    /// a widget nothing downstream read.
    #[test]
    fn the_startup_switch_lands_in_the_key_its_reader_opens() {
        let dir = root_holding("{}");
        let mut config = Config::load(&dir).unwrap();
        let rec = Rec::load(&config);
        let mut draft = RecDraft::from(&rec);
        draft.set_text(RField::StartOnBoot, "false");
        let (validated, notes) = draft.validate(&rec, &options());
        assert!(notes.is_empty(), "{notes:?}");
        assert!(!validated.start_recording_on_startup);
        validated.stage(&mut config);
        config.save().unwrap();

        let back = Config::load(&dir).unwrap();
        assert!(!back.bool_or("start_recording_on_startup", true), "supervisor.rs's own read sees it off");
        // A bare JSON boolean, not the text of one: `bool_or` takes a `Value::Bool` straight, and
        // every other writer of this file has spelled the key that way.
        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(raw.contains("\"start_recording_on_startup\": false"), "written as a JSON bool: {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `use_native_core` chose between the native recorder and the Python one. The Python application
    /// was deleted in 3f37cbf and the tray's fallback with it, so the key now has **no reader
    /// anywhere in this workspace** — while this page still drew a checkbox for it and still wrote it
    /// on Save. That is the defect class this branch has eliminated seven times over: a setting that
    /// persists and changes nothing is worse than a missing setting, because the user believes they
    /// configured something. Both halves are pinned here — that no field maps to the key, and that a
    /// Save never writes it — and `render_tests` pins the third, that nothing paints it.
    #[test]
    fn a_save_never_writes_the_dead_use_native_core_key() {
        assert!(
            !RField::ALL.iter().any(|field| field.key() == "use_native_core"),
            "nothing reads this key, so this page must not offer it as a control"
        );

        let dir = root_holding("{}");
        let mut config = Config::load(&dir).unwrap();
        Rec::load(&config).stage(&mut config);
        config.save().unwrap();

        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(!raw.contains("use_native_core"), "Save wrote a key no binary reads: {raw}");
        // The page still owns the switch that survives in the same group, so this failure would not
        // be a silent "nothing was written at all".
        assert!(raw.contains("\"start_recording_on_startup\": true"), "the sibling key is still written: {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the same removal: the switch is not merely unstaged, it is not a field, so
    /// there is no draft text that could reach it and no `Rec` value for `stage` to have skipped.
    /// Driving *every* boolean the page does offer and saving must therefore still leave the key out
    /// of the file.
    #[test]
    fn no_draft_text_can_reach_use_native_core() {
        let dir = root_holding("{}");
        let mut config = Config::load(&dir).unwrap();
        let base = Rec::load(&config);
        let mut draft = RecDraft::from(&base);
        let mut driven = 0;
        for field in RField::ALL {
            if let Kind::Bool = field.kind(&options(), &base) {
                draft.set_text(field, "false");
                driven += 1;
            }
        }
        assert_eq!(driven, 3, "the three bools this page offers: dedup, foreground window, startup");
        assert!(
            draft.0.keys().all(|field| field.key() != "use_native_core"),
            "a draft entry for the dead key means it crept back into RField::ALL"
        );
        let (validated, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "{notes:?}");
        assert!(!validated.start_recording_on_startup, "the draft really did move the surviving switch");
        validated.stage(&mut config);
        config.save().unwrap();

        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(raw.contains("\"start_recording_on_startup\": false"), "what was driven was written: {raw}");
        assert!(!raw.contains("use_native_core"), "a Save from a fully-driven page still wrote it: {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `convert_screenshots_to_vid_energy_saving_mode` is honoured by `record_screen.py` and by nothing
    /// in this workspace, so it is not a widget here and is not staged — but a value the user set
    /// through the Python recorder must ride the merged map straight back out, exactly as found.
    /// Removing the control may not open the hole it was meant to close.
    #[test]
    fn the_battery_gate_is_carried_not_offered_and_a_set_value_survives_a_save() {
        assert!(
            !RField::ALL.iter().any(|field| field.key() == "convert_screenshots_to_vid_energy_saving_mode"),
            "no native binary honours this key, so this page must not offer to change it"
        );

        // A Python-set gate (mode 2) is read as "requested" and left byte-for-byte in the file.
        let dir = root_holding(r#"{"convert_screenshots_to_vid_energy_saving_mode": 2}"#);
        let mut config = Config::load(&dir).unwrap();
        let rec = Rec::load(&config);
        assert!(rec.energy_saving_requested, "the page notices the user asked for a battery gate");
        rec.stage(&mut config);
        config.save().unwrap();
        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(raw.contains("\"convert_screenshots_to_vid_energy_saving_mode\": 2"), "Save did not clobber it: {raw}");
        let _ = std::fs::remove_dir_all(&dir);

        // And a config that never mentions it neither invents a request nor writes the key.
        let clean = root_holding("{}");
        let mut config = Config::load(&clean).unwrap();
        let rec = Rec::load(&config);
        assert!(!rec.energy_saving_requested);
        rec.stage(&mut config);
        config.save().unwrap();
        let raw = std::fs::read_to_string(clean.join("userdata/config_user.json")).unwrap();
        assert!(!raw.contains("convert_screenshots_to_vid_energy_saving_mode"), "the form wrote a key it does not own: {raw}");
        let _ = std::fs::remove_dir_all(&clean);
    }

    /// The encoder row, proved the way every other surviving row on this page has to be proved: the
    /// draft the widget writes to, then the real `Config::save`, then the file read back through
    /// **`windmaint`'s own accessor and its own default** (`convert.rs`:
    /// `config.str_or("record_encoder", "cpu_h264")`). Nothing less is enough here, because the defect
    /// this page keeps catching is exactly a widget that exists, saves, and is never looked at.
    ///
    /// The name chosen is `SVT-AV1`, which is in the shipped `record_preset.json` and is not the
    /// default, so a fall-through to `cpu_h264` on either side of the write shows up as a failure
    /// rather than as a passing test that proved nothing.
    #[test]
    fn the_record_encoder_the_page_saves_is_the_encoder_windmaint_looks_up() {
        let dir = root_holding("{}");
        let mut config = Config::load(&dir).unwrap();
        let base = Rec::load(&config);
        assert_eq!(base.record_encoder, "cpu_h264", "the stock install is on the CPU preset");

        let mut draft = RecDraft::from(&base);
        draft.set_text(RField::RecordEncoder, "SVT-AV1");
        let (validated, notes) = draft.validate(&base, &options_with_the_shipped_encoders());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(validated.record_encoder, "SVT-AV1");
        validated.stage(&mut config);
        config.save().unwrap();

        // The bytes on disk, and then the consumer's read of them.
        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(raw.contains("\"record_encoder\": \"SVT-AV1\""), "{raw}");
        let back = Config::load(&dir).unwrap();
        assert_eq!(
            back.str_or("record_encoder", "cpu_h264"),
            "SVT-AV1",
            "convert.rs's own accessor sees the encoder the user picked"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Every name this page offers must be a name `windmaint`'s resolver can look up, or the combo is
    /// offering a lie. The shipped-file copy of that rule, pinned against the real payload file rather
    /// than against a fixture: `RecOptions`' built-in list exists precisely for the case where the
    /// preset file cannot be read, and a list that drifts from the file would then be offering
    /// encoders nothing can resolve and hiding ones the user's own file names.
    #[test]
    fn the_offered_record_encoders_are_exactly_the_shipped_preset_file() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(std::path::Path::parent).unwrap();
        let text = std::fs::read_to_string(root.join("config_src/record_preset.json")).expect("the payload's record_preset.json");
        let value: Value = serde_json::from_str(&text).expect("the payload's record_preset.json parses");
        let mut from_file: Vec<String> = value.as_object().expect("an object").keys().cloned().collect();
        from_file.sort();
        assert_eq!(
            from_file,
            {
                let mut defaults = RecOptions::default().record_encoders;
                defaults.sort();
                defaults
            },
            "`RecOptions::default` has drifted from the file it stands in for"
        );
    }

    /// The tenth instance of the defect this branch exists to eliminate, and the guard against its
    /// return. `record_crf` is read by `windmaint`'s `encoder_args` and reaches ffmpeg only when the
    /// chosen preset states no rate control at all; every preset the payload ships states it with
    /// `-b:v BITRATE`, so the box changed nothing on any stock install while its own tooltip promised
    /// it was "passed straight to ffmpeg". The measurement is `windmaint`'s
    /// `no_shipped_record_preset_carries_the_crf_into_the_command_line`; this pins the removal.
    ///
    /// Both halves matter. No field may map to the key — the same treatment `use_native_core` got — and
    /// a Save must not touch a value the user has, because the key is *not* universally inert: a
    /// hand-authored preset naming `-crf` or `CRF_NUM` does read it, and a settings page that rewrote
    /// what it no longer owns would silently reset a working configuration.
    #[test]
    fn record_crf_is_no_longer_offered_and_a_value_the_user_kept_survives_a_save() {
        assert!(
            !RField::ALL.iter().any(|field| field.key() == "record_crf"),
            "no native preset reads this, so the page must not offer it as a control"
        );

        // A user's own CRF, set back when this page still had the box, rides the merged map out again
        // unchanged however many other fields are driven.
        let dir = root_holding(r#"{"record_crf": 22}"#);
        let mut config = Config::load(&dir).unwrap();
        let base = Rec::load(&config);
        let mut draft = RecDraft::from(&base);
        draft.set_text(RField::RecordBitrate, "600");
        let (validated, notes) = draft.validate(&base, &options_with_the_shipped_encoders());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(validated.record_bitrate, 600, "the field that WAS edited did change");
        validated.stage(&mut config);
        config.save().unwrap();

        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(raw.contains("\"record_crf\": 22"), "Save rewrote a key this page no longer owns: {raw}");
        assert!(raw.contains("\"record_bitrate\": 600"), "and the page did still save: {raw}");
        let back = Config::load(&dir).unwrap();
        assert_eq!(back.i64_or("record_crf", 39), 22, "windmaint's own read still sees the user's number");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A stock config that never mentions `record_crf` must not gain the key from this page either:
    /// `windmaint` would then be reading a number the user never chose, through a preset that ignores
    /// it, and the file would look like this page owns the setting.
    #[test]
    fn a_save_never_invents_a_record_crf_key() {
        let dir = root_holding("{}");
        let mut config = Config::load(&dir).unwrap();
        Rec::load(&config).stage(&mut config);
        config.save().unwrap();
        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        assert!(!raw.contains("record_crf"), "the form wrote a key it does not own: {raw}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_display_label_names_the_panel_rather_than_only_its_number() {
        let portrait = DisplayInfo { index: 2, width: 1440, height: 2560, primary: false };
        assert_eq!(portrait.label(), "#2 1440 x 2560 (portrait)");
        let primary = DisplayInfo { index: 1, width: 1920, height: 1080, primary: true };
        assert_eq!(primary.label(), "#1 1920 x 1080 (landscape, primary)");
    }
}
