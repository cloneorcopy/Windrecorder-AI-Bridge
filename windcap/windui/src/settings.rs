//! The typed view of the config keys that actually change what these two screens show.
//!
//! Upstream's settings page is a wall of ~100 keys, most of which the recorder owns; the WebUI's own
//! `st.number_input` widgets clamp with `min_value`/`max_value` and nothing else, so a value can only ever
//! be in range if the widget enforces it. Here the range is a property of the *field*, not of a widget, so
//! the same clamp applies to a hand-edited config file and to a headless test — and nonsense never reaches
//! `userdata/config_user.json`, which the Python app also reads.
//!
//! The ranges below are the ones `windrecorder/ui/setting.py` ships with, kept deliberately: the Python app
//! will happily read this file back, and a value it refuses to display would look like corruption on the
//! other side.
//!
//! Two of the fields are *pickers* rather than numbers, and both of those existed as widgets in the Python
//! settings page and were lost with it: which OCR engine indexes the screen, and which language the
//! product speaks. A picker is only honest if its list comes from the machine and the catalog, which is
//! what [`Options`] is for.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use wind_base::config::Config;
use wind_base::i18n::Catalog;

/// The fifteen keys Search / OneDay / Settings consume. Everything else in the config belongs to the
/// recorder and is left exactly as found — `Config::save` writes the whole merged map back, so an
/// untouched key round-trips unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Results per page. `db_manager` calls this `db_max_page_result`; it is a page *size*.
    pub max_page_result: i64,
    /// How many thumbnails the OneDay timeline strip is allowed to hold.
    pub oneday_timeline_pic_num: i64,
    /// Minutes past midnight where "today" starts. 180 means 01:00 on the 22nd is the 21st's work.
    pub day_begin_minutes: i64,
    pub use_similar_ch_char_to_search: bool,
    pub ocr_lang: String,
    /// Which OCR engine reads the screen. The recorder and the indexer run it through
    /// [`wind_base::ocr::Engine::select`]; this is the same key they both read.
    pub ocr_engine: String,
    /// The interface language: `en`, `sc`, `ja`, … whatever `languages.json` holds a table for. Upstream's
    /// own key, and the tray, the window and the HTML front end all read it.
    pub lang: String,
    pub exclude_words: Vec<String>,
    /// The privacy mask: four percentages per display slot, in the key's own order — top, right, bottom,
    /// left. Read by `windcap::crop`, which paints those edges black on the copy the recogniser sees and
    /// on nothing else, so this is the setting that decides what never becomes searchable.
    ///
    /// It is a list rather than one group because `Urbl::slot` gives a slot the list does not reach the
    /// shipped fallback, not slot 0: on a machine with four panels, one group means the other three get
    /// 6/6/6/3 whatever the user typed here. The widget therefore paints one group per detected display.
    pub ocr_image_crop_urbl: Vec<i64>,
    pub enable_ocr_str_highlight_indicator: bool,
    pub thumbnail_generation_size_width: i64,
    /// What the window's close button does. Upstream had no equivalent because upstream's window was a
    /// browser tab; here the window is a child of the tray, and closing it used to end the process.
    pub close_window_to_tray: bool,
    /// `maintain_window_start` exactly as typed: `03:30`, or empty for "no scheduled window".
    pub maintain_window_start: String,
    /// `maintain_window_end`: the pass stops at this minute. Empty, or the same as the start, means
    /// nothing is scheduled and the old idle rule runs instead.
    pub maintain_window_end: String,
    /// Register the tray to run at sign-in. The recorder keeps running while this is off — this is about
    /// the machine, not about the session.
    pub start_app_on_boot: bool,
}

/// Which field a widget is editing. An enum rather than a `&'static str` key so the compiler
/// exhaustiveness check is what notices when a ninth setting is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    MaxPageResult,
    TimelinePics,
    DayBeginMinutes,
    SimilarChars,
    OcrLang,
    OcrEngine,
    Lang,
    ExcludeWords,
    /// The privacy mask's four edges, one group of four per display.
    Mask,
    Highlight,
    ThumbWidth,
    CloseToTray,
    StartOnBoot,
    /// The two clock times that bound when the deferred pass may run.
    MaintainStart,
    MaintainEnd,
}

/// One row of a picker: the value the config holds, and the words the widget shows.
///
/// They differ for a language, which is stored as `sc` and read as 简体中文.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

impl Choice {
    fn same(text: &str) -> Choice {
        Choice { value: text.to_string(), label: text.to_string() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Int { min: i64, max: i64 },
    Bool,
    /// One free-text token (`ocr_lang`).
    Text { max_chars: usize },
    /// A list, one entry per line (`exclude_words`).
    Lines { max_entries: usize },
    /// One group of percentages per display slot, in the key's own top/right/bottom/left order. The
    /// widget edits four boxes per screen; the text the draft holds is the same list either window can
    /// parse, so no door needs a private representation of it.
    Urbl { slots: usize },
    /// One of a list the *machine* or the catalog supplies, never a literal in this file: what the picker
    /// offers is what this install can actually do.
    Choice(Vec<Choice>),
}

/// What this install can offer, as distinct from what the config happens to say.
///
/// Read once per form, the way `record::RecOptions` is. The reason it is a parameter and not a lookup
/// inside `Field::kind` is that both doors — the egui window and `winduiweb`'s `settings_read` — have to
/// see the same list, and a second place that asks the OS or re-reads a JSON file is where they diverge.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Every engine `wind_base::ocr` knows about, including the ones it reports as not driveable.
    pub engines: Vec<wind_base::ocr::Choice>,
    /// The locales `languages.json` translates, each with the name it is called by in its own language.
    pub locales: Vec<(String, String)>,
    /// The panels plugged in right now, in slot order, each as the pixels the mask is a percentage of.
    /// Empty means the enumeration was unavailable — a headless test, a session with no desktop — and the
    /// row then offers the groups the config already holds rather than one, because a control that hides
    /// three of a user's four screens is worse than one that guesses at four.
    pub mask_panels: Vec<(i64, i64)>,
}

impl Options {
    /// Ask the install and the catalog. Nothing here is allowed to fail: an install with no engines and one
    /// locale still has to be able to show its settings page.
    pub fn scan(root: &Path, config: &Config) -> Options {
        Options {
            engines: wind_base::ocr::choices(config),
            locales: Catalog::locales(root),
            mask_panels: windcap::capture::monitors()
                .into_iter()
                .map(|panel| (i64::from(panel.width), i64::from(panel.height)))
                .collect(),
        }
    }

    /// The mask groups a form has to draw: what the machine reports, or what the file already holds,
    /// whichever is more — never below one.
    pub fn mask_groups(&self, stored: &[i64]) -> usize {
        self.mask_panels.len().max(stored.len() / 4).max(1)
    }

    /// The engines on offer: what is driveable, and the stored value too when it is not — because a picker
    /// that omits the current value rewrites a migrated config the moment anyone saves the form.
    pub fn engine_choices(&self, current: &str) -> Vec<Choice> {
        let mut out: Vec<Choice> = self
            .engines
            .iter()
            .filter(|engine| engine.available)
            .map(|engine| Choice::same(&engine.name))
            .collect();
        if !out.iter().any(|choice| choice.value == current) {
            out.push(Choice::same(current));
        }
        out
    }

    /// The names of the engines this binary cannot drive, so the page can say which ones it left out and
    /// why, instead of looking like the install forgot them.
    pub fn unavailable_engines(&self) -> Vec<String> {
        self.engines.iter().filter(|engine| !engine.available).map(|engine| engine.name.clone()).collect()
    }

    /// The languages on offer, with the current one kept visible even when the catalog has no table for it
    /// — a user who typed `zh` into the file by hand gets told, not overwritten.
    pub fn language_choices(&self, current: &str) -> Vec<Choice> {
        let mut out: Vec<Choice> = self
            .locales
            .iter()
            .map(|(code, name)| Choice { value: code.clone(), label: name.clone() })
            .collect();
        if !out.iter().any(|choice| choice.value == current) {
            out.push(Choice::same(current));
        }
        out
    }

    /// A locale the catalog has no table for, when that is the stored value. `None` when the language is
    /// one the file holds.
    pub fn unknown_language(&self, current: &str) -> Option<String> {
        if self.locales.iter().any(|(code, _)| code == current) {
            None
        } else {
            Some(self.locales.iter().map(|(code, _)| code.as_str()).collect::<Vec<_>>().join(", "))
        }
    }
}

impl Field {
    pub const ALL: [Field; 15] = [
        Field::MaxPageResult,
        Field::TimelinePics,
        Field::DayBeginMinutes,
        Field::SimilarChars,
        Field::OcrLang,
        Field::OcrEngine,
        Field::Lang,
        Field::ExcludeWords,
        Field::Mask,
        Field::Highlight,
        Field::ThumbWidth,
        Field::CloseToTray,
        Field::StartOnBoot,
        Field::MaintainStart,
        Field::MaintainEnd,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Field::MaxPageResult => "max_page_result",
            Field::TimelinePics => "oneday_timeline_pic_num",
            Field::DayBeginMinutes => "day_begin_minutes",
            Field::SimilarChars => "use_similar_ch_char_to_search",
            Field::OcrLang => "ocr_lang",
            Field::OcrEngine => wind_base::ocr::ENGINE_KEY,
            Field::Lang => "lang",
            Field::ExcludeWords => "exclude_words",
            // The constant, not a copy of it: `windcap::crop` is the code that paints these edges,
            // so the name the form writes and the name the painter reads cannot drift apart.
            Field::Mask => windcap::crop::CONFIG_KEY,
            Field::Highlight => "enable_ocr_str_highlight_indicator",
            Field::ThumbWidth => "thumbnail_generation_size_width",
            Field::CloseToTray => "close_window_to_tray",
            Field::MaintainStart => "maintain_window_start",
            Field::MaintainEnd => "maintain_window_end",
            Field::StartOnBoot => "start_app_on_boot",
        }
    }

    /// The catalog key this field's label is translated under.
    ///
    /// Where upstream's settings page already had a translated row for the same setting, that key is
    /// reused rather than a new one written: its `sc` and `ja` text already exists, contributors
    /// maintain one row per setting, and `(key) not found` markers are how a localization effort dies.
    /// The English [`Field::label`] stays as the fallback, so a row no one has translated yet still names
    /// itself.
    pub fn label_key(self) -> &'static str {
        match self {
            Field::MaxPageResult => "set_input_max_num_search_page",
            Field::TimelinePics => "set_input_oneday_timeline_thumbnail_num",
            Field::DayBeginMinutes => "set_input_day_begin_minutes",
            Field::SimilarChars => "set_checkbox_use_similar_zh_char_to_search",
            Field::OcrLang => "set_selectbox_ocr_lang",
            Field::OcrEngine => "set_selectbox_local_ocr_engine",
            Field::Lang => "set_selectbox_interface_language",
            Field::ExcludeWords => "set_input_exclude_words",
            Field::Mask => "set_input_mask_edges",
            Field::Highlight => "set_checkbox_highlight_matched",
            Field::ThumbWidth => "set_input_thumbnail_width",
            Field::CloseToTray => "set_checkbox_close_to_tray",
            Field::MaintainStart => "set_input_maintain_window_start",
            Field::MaintainEnd => "set_input_maintain_window_end",
            Field::StartOnBoot => "set_checkbox_start_on_boot",
        }
    }

    /// The catalog key this field's explanation is translated under.
    pub fn help_key(self) -> &'static str {
        match self {
            Field::MaxPageResult => "set_help_max_page_result",
            Field::TimelinePics => "set_input_oneday_timeline_thumbnail_num_help",
            Field::DayBeginMinutes => "set_help_day_begin_minutes",
            Field::SimilarChars => "set_checkbox_use_similar_zh_char_to_search_help",
            Field::OcrLang => "set_help_ocr_lang",
            Field::OcrEngine => "set_help_local_ocr_engine",
            Field::Lang => "set_help_interface_language",
            Field::ExcludeWords => "set_help_exclude_words",
            Field::Mask => "set_help_mask_edges",
            Field::Highlight => "set_help_highlight_matched",
            Field::ThumbWidth => "set_help_thumbnail_width",
            Field::CloseToTray => "set_help_close_to_tray",
            Field::MaintainStart => "set_help_maintain_window_start",
            Field::MaintainEnd => "set_help_maintain_window_end",
            Field::StartOnBoot => "set_help_start_on_boot",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Field::MaxPageResult => "Results per page",
            Field::TimelinePics => "Timeline thumbnails",
            Field::DayBeginMinutes => "Day begins at (minutes past midnight)",
            Field::SimilarChars => "Fuzzy similar Chinese glyphs",
            Field::OcrLang => "OCR language",
            Field::OcrEngine => "OCR engine",
            Field::Lang => "Interface language",
            Field::ExcludeWords => "Exclude words (one per line)",
            Field::Mask => "Masked edges of each screen",
            Field::Highlight => "Highlight matched text",
            Field::ThumbWidth => "Thumbnail width (px)",
            Field::CloseToTray => "Close button keeps the app running in the tray",
            Field::MaintainStart => "Organise the backlog from (HH:MM)",
            Field::MaintainEnd => "Organise the backlog until (HH:MM)",
            Field::StartOnBoot => "Start on sign-in",
        }
    }

    /// What the field is for, in the constraint's own words — this is the tooltip, and a setting whose
    /// effect is invisible gets left at its default by mistake.
    pub fn help(self) -> &'static str {
        match self {
            Field::MaxPageResult => {
                "How many rows one search page holds. Every page is read out of the month files in \
                 full and sliced in memory, so a large value costs decode and texture upload, not \
                 query time."
            }
            Field::TimelinePics => "Upper bound on the thumbnails painted into the OneDay strip.",
            Field::DayBeginMinutes => {
                "Activity before this hour belongs to the previous day. The OneDay timeline, the \
                 search date range and the day's row set all use it; getting it wrong shows the \
                 user the wrong 'today'."
            }
            Field::SimilarChars => {
                "Expand each keyword into its shape-similar characters, so a search for 也化 finds \
                 量化. Costs query width: one LIKE group per generated variant."
            }
            Field::OcrLang => "Language the OCR engine is asked in. It changes what gets recognised, \
                 not which engine does the recognising — that is the engine row above.",
            Field::OcrEngine => {
                "Which engine reads the screen: the recorder indexes with it live, and \
                 `wind-reindex` reads old footage with it too. An engine whose recognition used to \
                 live inside Python is listed as unavailable rather than offered, because there is no \
                 Python left to run it. `windsetup check-engines` benchmarks what is here."
            }
            Field::Lang => {
                "The language every label in the product is drawn in — the tray menu, this window, \
                 and the HTML front end. Relabels what a running tray or window shows only after it \
                 starts again; nothing you recorded is affected."
            }
            Field::ExcludeWords => {
                "Window titles the recorder refuses to index at all — passwords, banking, \
                 password managers. Search cannot find what was never written."
            }
            Field::Mask => {
                "How much of each screen never reaches the recogniser, as a percentage of that panel: \
                 the band is painted black on the copy the engine reads, and on nothing else — the \
                 screenshot, the video and the preview keep every pixel. A screen the list has no group \
                 for takes 6/6/6/3, the same default the recorder applies, so filling every row is what \
                 makes the setting mean what it says."
            }
            Field::Highlight => "Split the result text into coloured runs at the matched terms.",
            Field::ThumbWidth => {
                "Width the stored base64 JPEG thumbnail was made at. It sizes the small preview only: \
                 click a result and the original frame is read off the screenshot cache, or out of the \
                 video, at full resolution."
            }
            Field::CloseToTray => {
                "Hide the window instead of ending it, so the recorder and the tray keep running and \
                 the tray's own menu brings the window back. Off, the window closes for good — but \
                 recording is a separate process and survives either way."
            }
            Field::MaintainStart => {
                "The hour the deferred work is allowed to start: reading text off the frames the \
                 recorder kept, dropping the ones that repeat, redrawing previews, turning slices into \
                 video, re-indexing footage and backing up. Left empty, nothing is scheduled by clock \
                 and the old rule stands — the pass waits for forty idle minutes instead."
            }
            Field::MaintainEnd => {
                "The hour it must stop, and the rest waits for tomorrow. A time earlier than the start \
                 is read as one night through midnight, so `22:00` to `06:00` is a single appointment \
                 and not two. Whatever is unfinished when this minute passes is picked up by the next \
                 window, never lost: the re-indexer skips what it has already done."
            }
            Field::StartOnBoot => {
                "Start the tray when you sign in to Windows, from your own account only and without \
                 administrator rights. The entry names this install's `windsvc.exe`, so moving the \
                 folder means turning this off and on again where the app now lives."
            }
        }
    }

    /// The widget's shape, given what this install can offer and what the config already holds.
    pub fn kind(self, options: &Options, current: &Settings) -> Kind {
        match self {
            // 5..=500 and 50..=100 are upstream's own widget bounds, not inventions.
            Field::MaxPageResult => Kind::Int { min: 5, max: 500 },
            Field::TimelinePics => Kind::Int { min: 50, max: 100 },
            // Upstream offers a 00:00..06:00 select box; the same ceiling as a range keeps a hand-edited
            // config honest without inventing a new limit.
            Field::DayBeginMinutes => Kind::Int { min: 0, max: 360 },
            // The shipped default is `CARD_PREVIEW_FLOOR` — the narrowest picture a card can be drawn
            // from without stretching it — so the ceiling has to sit above it or the row is a knob that
            // can only be turned down.
            Field::ThumbWidth => Kind::Int { min: 16, max: 1024 },
            // The two clock boxes are text, not a picker: an install that names nothing must be able
            // to say so by leaving them empty, which a list of hours cannot express.
            Field::MaintainStart | Field::MaintainEnd => Kind::Text { max_chars: 5 },
            Field::SimilarChars | Field::Highlight | Field::CloseToTray | Field::StartOnBoot => Kind::Bool,
            Field::OcrLang => Kind::Text { max_chars: 32 },
            Field::OcrEngine => Kind::Choice(options.engine_choices(&current.ocr_engine)),
            Field::Lang => Kind::Choice(options.language_choices(&current.lang)),
            Field::ExcludeWords => Kind::Lines { max_entries: 200 },
            Field::Mask => Kind::Urbl { slots: options.mask_groups(&current.ocr_image_crop_urbl) },
        }
    }
}

/// A field's value as the form holds it: the parsed, in-range truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Typed {
    Int(i64),
    Bool(bool),
    Text(String),
    Lines(Vec<String>),
    /// The mask, flat and in the key's own order: four per screen.
    Ints(Vec<i64>),
}

impl Settings {
    /// Read the fifteen keys. Defaults are `config_default.json`'s own values, so a config that predates a key
    /// behaves the way the Python app treats it rather than as broken.
    pub fn load(config: &Config) -> Settings {
        Settings {
            max_page_result: config.i64_or("max_page_result", 20),
            oneday_timeline_pic_num: config.i64_or("oneday_timeline_pic_num", 50),
            day_begin_minutes: config.day_begin_minutes(),
            use_similar_ch_char_to_search: config.bool_or("use_similar_ch_char_to_search", true),
            ocr_lang: config.str_or("ocr_lang", "zh-Hans-CN"),
            ocr_engine: wind_base::ocr::configured_name(config),
            lang: config.str_or("lang", "en"),
            exclude_words: config.str_list("exclude_words"),
            ocr_image_crop_urbl: config.i64_list(windcap::crop::CONFIG_KEY),
            enable_ocr_str_highlight_indicator: config.bool_or("enable_ocr_str_highlight_indicator", true),
            thumbnail_generation_size_width: i64::from(config.thumbnail_width()),
            // On by default: the tray is the product's front door, and a window whose close button ends
            // the process is what the user reported as "the program quit".
            close_window_to_tray: config.bool_or("close_window_to_tray", true),
            maintain_window_start: config.str_or("maintain_window_start", ""),
            maintain_window_end: config.str_or("maintain_window_end", ""),
            // Off by default: registering something to run at sign-in is the user's decision to make.
            start_app_on_boot: config.bool_or("start_app_on_boot", false),
        }
    }

    pub fn get(&self, field: Field) -> Typed {
        match field {
            Field::MaxPageResult => Typed::Int(self.max_page_result),
            Field::TimelinePics => Typed::Int(self.oneday_timeline_pic_num),
            Field::DayBeginMinutes => Typed::Int(self.day_begin_minutes),
            Field::ThumbWidth => Typed::Int(self.thumbnail_generation_size_width),
            Field::CloseToTray => Typed::Bool(self.close_window_to_tray),
            Field::MaintainStart => Typed::Text(self.maintain_window_start.clone()),
            Field::MaintainEnd => Typed::Text(self.maintain_window_end.clone()),
            Field::StartOnBoot => Typed::Bool(self.start_app_on_boot),
            Field::SimilarChars => Typed::Bool(self.use_similar_ch_char_to_search),
            Field::Highlight => Typed::Bool(self.enable_ocr_str_highlight_indicator),
            Field::OcrLang => Typed::Text(self.ocr_lang.clone()),
            Field::OcrEngine => Typed::Text(self.ocr_engine.clone()),
            Field::Lang => Typed::Text(self.lang.clone()),
            Field::ExcludeWords => Typed::Lines(self.exclude_words.clone()),
            Field::Mask => Typed::Ints(self.ocr_image_crop_urbl.clone()),
        }
    }

    fn set(&mut self, field: Field, value: Typed) {
        match (field, value) {
            (Field::MaxPageResult, Typed::Int(v)) => self.max_page_result = v,
            (Field::TimelinePics, Typed::Int(v)) => self.oneday_timeline_pic_num = v,
            (Field::DayBeginMinutes, Typed::Int(v)) => self.day_begin_minutes = v,
            (Field::ThumbWidth, Typed::Int(v)) => self.thumbnail_generation_size_width = v,
            (Field::CloseToTray, Typed::Bool(v)) => self.close_window_to_tray = v,
            (Field::MaintainStart, Typed::Text(v)) => self.maintain_window_start = v,
            (Field::MaintainEnd, Typed::Text(v)) => self.maintain_window_end = v,
            (Field::StartOnBoot, Typed::Bool(v)) => self.start_app_on_boot = v,
            (Field::SimilarChars, Typed::Bool(v)) => self.use_similar_ch_char_to_search = v,
            (Field::Highlight, Typed::Bool(v)) => self.enable_ocr_str_highlight_indicator = v,
            (Field::OcrLang, Typed::Text(v)) => self.ocr_lang = v,
            (Field::OcrEngine, Typed::Text(v)) => self.ocr_engine = v,
            (Field::Lang, Typed::Text(v)) => self.lang = v,
            (Field::ExcludeWords, Typed::Lines(v)) => self.exclude_words = v,
            (Field::Mask, Typed::Ints(v)) => self.ocr_image_crop_urbl = v,
            // A field/value mismatch is a programming error in `Field::ALL`, not user input.
            _ => unreachable!("{} cannot hold that type", field.key()),
        }
    }

    /// Stage every key into the config. `Config::save` is what hits the disk.
    pub fn stage(&self, config: &mut Config) {
        config.set("max_page_result", Value::from(self.max_page_result));
        config.set("oneday_timeline_pic_num", Value::from(self.oneday_timeline_pic_num));
        config.set("day_begin_minutes", Value::from(self.day_begin_minutes));
        config.set("use_similar_ch_char_to_search", Value::Bool(self.use_similar_ch_char_to_search));
        config.set("ocr_lang", Value::String(self.ocr_lang.clone()));
        config.set(wind_base::ocr::ENGINE_KEY, Value::String(self.ocr_engine.clone()));
        config.set("lang", Value::String(self.lang.clone()));
        config.set(
            "exclude_words",
            Value::Array(self.exclude_words.iter().cloned().map(Value::String).collect()),
        );
        config.set(
            windcap::crop::CONFIG_KEY,
            Value::Array(self.ocr_image_crop_urbl.iter().copied().map(Value::from).collect()),
        );
        config.set(
            "enable_ocr_str_highlight_indicator",
            Value::Bool(self.enable_ocr_str_highlight_indicator),
        );
        config.set("thumbnail_generation_size_width", Value::from(self.thumbnail_generation_size_width));
        config.set("close_window_to_tray", Value::Bool(self.close_window_to_tray));
        // The two clock boxes. They were missing here while `load` read them, `Field::key` named them and
        // `validate` parsed them, which made the settings page the only place in the product that could
        // edit a schedule the file never received: the widget showed what was typed, the save answered
        // "written", and the next read printed the old hours. `every_field_the_page_edits_survives_a_stage`
        // is the guard that no tenth field joins that list.
        config.set("maintain_window_start", Value::String(self.maintain_window_start.clone()));
        config.set("maintain_window_end", Value::String(self.maintain_window_end.clone()));
        config.set("start_app_on_boot", Value::Bool(self.start_app_on_boot));
    }
}

/// What the user is typing, per field, kept apart from the parsed value so a half-entered number does not
/// destroy the one that is there.
#[derive(Debug, Clone)]
pub struct Draft(BTreeMap<Field, String>);

impl Draft {
    pub fn from(settings: &Settings) -> Draft {
        let mut raw = BTreeMap::new();
        for field in Field::ALL {
            raw.insert(field, render(settings.get(field)));
        }
        Draft(raw)
    }

    pub fn text(&self, field: Field) -> &str {
        self.0.get(&field).map(String::as_str).unwrap_or("")
    }

    pub fn set_text(&mut self, field: Field, text: &str) {
        self.0.insert(field, text.to_string());
    }

    pub fn bool_of(&self, field: Field) -> bool {
        matches!(self.0.get(&field).map(String::as_str), Some("true"))
    }

    /// The label a picker row shows for the draft's text of `field`, which is what a widget paints. The
    /// stored value is the config's business; 简体中文 is the user's.
    pub fn label_of(&self, field: Field, options: &Options, current: &Settings) -> String {
        let text = self.text(field).to_string();
        match field.kind(options, current) {
            Kind::Choice(choices) => choices
                .iter()
                .find(|choice| choice.value == text)
                .map(|choice| choice.label.clone())
                .unwrap_or(text),
            _ => text,
        }
    }

    /// Parse and clamp every field. The returned notes are the "why" the widget shows next to a value that
    /// had to be corrected; an empty Vec means the draft is exactly what will be saved.
    pub fn validate(&self, settings: &Settings, options: &Options) -> (Settings, Vec<String>) {
        let mut out = settings.clone();
        let mut notes = Vec::new();
        for field in Field::ALL {
            match apply(field, self.text(field), options, settings, &mut out, &mut notes) {
                Ok(()) => {}
                Err(note) => notes.push(note),
            }
        }
        (out, notes)
    }
}

/// The ceiling upstream's own widgets allowed on one edge: 40% of a panel, because past that the band
/// stops hiding chrome and starts hiding the work.
pub const MASK_EDGE_MAX: i64 = 40;

/// Read a mask draft. Anything that is not a number is the caller's business — the form keeps the
/// previous value and says so — but a number out of range is clamped here, because the widget cannot
/// un-type what the user wrote and a 900% band would hide the whole screen from the recogniser.
pub fn parse_mask(raw: &str) -> Option<Vec<i64>> {
    let mut out = Vec::new();
    for token in raw.split([' ', ',', '\n', '\t', ';']).map(str::trim).filter(|token| !token.is_empty()) {
        let parsed: i64 = token.parse().ok()?;
        out.push(parsed.clamp(0, MASK_EDGE_MAX));
    }
    Some(out)
}

/// The mask as the key stores it, padded to `groups` complete slots with the same fallback the painter
/// uses for a slot the list does not reach.
pub fn fit_mask(values: &[i64], groups: usize) -> Vec<i64> {
    let mut out = values.to_vec();
    let fallback = windcap::crop::Urbl::FALLBACK.array();
    while out.len() < groups * 4 {
        out.extend_from_slice(&fallback);
    }
    out.truncate(groups * 4);
    out
}

fn render(value: Typed) -> String {
    match value {
        Typed::Int(v) => v.to_string(),
        Typed::Bool(v) => v.to_string(),
        Typed::Text(v) => v,
        Typed::Lines(v) => v.join("\n"),
        Typed::Ints(v) => v.iter().map(i64::to_string).collect::<Vec<_>>().join(", "),
    }
}

fn apply(
    field: Field,
    raw: &str,
    options: &Options,
    base: &Settings,
    into: &mut Settings,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let typed = match field.kind(options, base) {
        Kind::Int { min, max } => {
            let trimmed = raw.trim();
            let parsed: i64 = match trimmed.parse() {
                Ok(v) => v,
                // Rejection, not silence: the field keeps its previous value and the widget says so.
                Err(_) => {
                    notes.push(format!(
                        "{}: '{}' is not a whole number, kept {}",
                        field.label(),
                        trimmed,
                        render(into.get(field))
                    ));
                    return Ok(());
                }
            };
            let clamped = parsed.clamp(min, max);
            if clamped != parsed {
                notes.push(format!(
                    "{}: {} is outside {}..={}, clamped to {}",
                    field.label(),
                    parsed,
                    min,
                    max,
                    clamped
                ));
            }
            Typed::Int(clamped)
        }
        Kind::Bool => Typed::Bool(raw == "true"),
        Kind::Text { max_chars } => {
            let mut value = raw.trim().to_string();
            if value.is_empty() {
                if matches!(field, Field::MaintainStart | Field::MaintainEnd) {
                    // Empty is how "no schedule" is stored, so clearing a clock box really clears the
                    // window instead of being refused as a missing answer.
                    into.set(field, Typed::Text(String::new()));
                    return Ok(());
                }
                notes.push(format!("{}: must not be empty, kept '{}'", field.label(), render(into.get(field))));
                return Ok(());
            }
            // The recorder's own reader is the rule here, so the box cannot accept a time the running
            // program would then quietly ignore.
            if matches!(field, Field::MaintainStart | Field::MaintainEnd)
                && wind_base::config::parse_clock_time(&value).is_none()
            {
                notes.push(format!(
                    "{}: '{}' is not a clock time like 03:30, kept {}",
                    field.label(),
                    value,
                    render(into.get(field))
                ));
                return Ok(());
            }
            if value.chars().count() > max_chars {
                notes.push(format!("{}: longer than {} characters, truncated", field.label(), max_chars));
                value = value.chars().take(max_chars).collect();
            }
            Typed::Text(value)
        }
        Kind::Urbl { slots } => {
            let parsed = match parse_mask(raw) {
                Some(values) => values,
                None => {
                    notes.push(format!(
                        "{}: '{}' is not a list of whole percentages, kept {}",
                        field.label(),
                        raw.trim(),
                        render(into.get(field))
                    ));
                    return Ok(());
                }
            };
            // An empty draft is not a refusal. The widget fills every group it shows with the band the
            // painter would apply to an uncovered screen anyway, so a config that holds nothing at all
            // becomes exactly what is on the screen — which is the only answer that keeps the row, the
            // file and the index telling one story.
            let fitted = fit_mask(&parsed, slots);
            // Truncation is the only case worth a note. Padding is not: the widget paints the shipped
            // 6/6/6/3 into every group it adds, so the user is looking at exactly the numbers being
            // written — and those are the numbers the recorder already applies to a slot the list does
            // not reach. Saying so on every first save of a fresh install would be news about nothing.
            if fitted.len() < parsed.len() {
                notes.push(format!(
                    "{}: {} numbers is more than {} screens need, kept the first {}",
                    field.label(),
                    parsed.len(),
                    slots,
                    fitted.len()
                ));
            }
            Typed::Ints(fitted)
        }
        Kind::Lines { max_entries } => {
            let mut entries: Vec<String> = raw.lines().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
            entries.sort();
            entries.dedup();
            if entries.len() > max_entries {
                notes.push(format!("{}: more than {} entries, kept the first {}", field.label(), max_entries, max_entries));
                entries.truncate(max_entries);
            }
            Typed::Lines(entries)
        }
        Kind::Choice(choices) => {
            let value = raw.trim();
            // A picker writes what it offered and nothing else. Refusing a value that is not on the list is
            // what keeps `ocr_engine` a name the recorder can resolve; the row's own `Field::help` names the
            // way to make a new one.
            if value.is_empty() || choices.iter().any(|choice| choice.value == value) {
                Typed::Text(value.to_string())
            } else {
                notes.push(format!(
                    "{}: '{}' is not one of the {} this install offers, kept {}",
                    field.label(),
                    value,
                    field.key(),
                    render(into.get(field))
                ));
                return Ok(());
            }
        }
    };
    into.set(field, typed);
    Ok(())
}

/// `day_begin_minutes` as the user reads it, so the number is never mistaken for an hour.
pub fn hhmm(minutes: i64) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            max_page_result: 20,
            oneday_timeline_pic_num: 50,
            day_begin_minutes: 180,
            maintain_window_start: String::new(),
            maintain_window_end: String::new(),
            use_similar_ch_char_to_search: true,
            ocr_lang: "zh-Hans-CN".into(),
            ocr_engine: wind_base::ocr::WINDOWS_ENGINE.into(),
            lang: "en".into(),
            exclude_words: vec!["KeePass".into()],
            // Two groups, because the fixture's `options()` reports two panels: a stored mask that
            // covers one of them is a state the form legitimately corrects, and the round-trip test
            // below must stay a test of the text, not of that correction.
            ocr_image_crop_urbl: vec![6, 6, 6, 3, 6, 6, 6, 3],
            enable_ocr_str_highlight_indicator: true,
            thumbnail_generation_size_width: 70,
            close_window_to_tray: true,
            start_app_on_boot: false,
        }
    }

    /// The two pickers' whole content, as an install that has one driveable engine and three locales.
    fn options() -> Options {
        Options {
            mask_panels: vec![(1920, 1080), (2560, 1440)],
            engines: vec![
                engine(wind_base::ocr::WINDOWS_ENGINE, true),
                engine(wind_base::ocr::TESSERACT_ENGINE, true),
                engine("PaddleOCR", false),
            ],
            locales: vec![
                ("en".into(), "English".into()),
                ("sc".into(), "简体中文".into()),
                ("ja".into(), "日本語".into()),
            ],
        }
    }

    fn engine(name: &str, available: bool) -> wind_base::ocr::Choice {
        wind_base::ocr::Choice { name: name.into(), available, detail: String::new() }
    }

    #[test]
    fn every_field_round_trips_through_its_own_draft_text() {
        let base = settings();
        let draft = Draft::from(&base);
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "a pristine draft must not complain: {notes:?}");
        assert_eq!(parsed, base);
    }

    #[test]
    fn out_of_range_input_is_clamped_and_explained_not_written() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::MaxPageResult, "99999");
        draft.set_text(Field::DayBeginMinutes, "9000");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.max_page_result, 500);
        assert_eq!(parsed.day_begin_minutes, 360);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("clamped"), "{}", notes[0]);
    }

    #[test]
    fn unparseable_input_keeps_the_previous_value_and_says_why() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::ThumbWidth, "wide");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.thumbnail_generation_size_width, 70);
        assert!(notes[0].contains("not a whole number"), "{notes:?}");
    }

    /// Four numbers per screen, and the screen count is the machine's, not the file's. A desk with two
    /// panels and one group typed gets the second group filled with the band the painter would have used
    /// anyway — which is why this is silent: the widget shows those four numbers on the row the user is
    /// looking at, and a note on every first save of a fresh install would be news about nothing.
    #[test]
    fn a_mask_shorter_than_the_screens_is_filled_with_the_band_the_recorder_would_have_used() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::Mask, "20, 0, 0, 0");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.ocr_image_crop_urbl, vec![20, 0, 0, 0, 6, 6, 6, 3], "{:?}", parsed.ocr_image_crop_urbl);
        assert!(notes.is_empty(), "padding a visible row is not a correction: {notes:?}");
    }

    /// The one case that *is* news: numbers typed for a fifth screen on a four-screen desk are dropped,
    /// and the file would otherwise quietly stop meaning what the user wrote.
    #[test]
    fn a_mask_longer_than_the_screens_says_what_it_dropped() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::Mask, "1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.ocr_image_crop_urbl, vec![1, 1, 1, 1, 2, 2, 2, 2]);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("more than"), "{}", notes[0]);
    }

    /// Every edge has a ceiling, and it is upstream's widget's own (`max_value=40`), because past that
    /// the band stops hiding the taskbar and starts hiding the work.
    #[test]
    fn a_mask_edge_beyond_the_ceiling_is_clamped_and_the_list_still_round_trips() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::Mask, "90, -4, 12, 7, 0, 0, 0, 0");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.ocr_image_crop_urbl, vec![40, 0, 12, 7, 0, 0, 0, 0]);
        assert!(notes.is_empty(), "clamping is not a correction worth a note when nothing was missing: {notes:?}");
    }

    /// The whole point of the row: what it writes is the key the painter reads, as numbers, under the
    /// constant `windcap::crop` itself names.
    #[test]
    fn the_mask_is_stored_as_numbers_under_the_painters_own_key() {
        let dir = std::env::temp_dir().join(format!("windui-mask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), b"{}").unwrap();
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::Mask, "0, 0, 12, 0, 5, 5, 5, 5");
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "{notes:?}");

        let mut config = wind_base::Config::load(&dir).unwrap();
        parsed.stage(&mut config);
        config.save().unwrap();
        let raw = std::fs::read_to_string(dir.join("userdata/config_user.json")).unwrap();
        // Read the stored file as data rather than matching its bytes: what matters is that the key is a
        // JSON array of numbers under the name the painter reads, and how serde chooses to space that
        // array is not this test's business.
        let stored: serde_json::Value = serde_json::from_str(&raw).expect("the user config is JSON");
        let list = stored["ocr_image_crop_URBL"].as_array().expect("the mask is stored as a list");
        assert_eq!(list.len(), 8, "{list:?}");
        assert_eq!(list[2], serde_json::Value::from(12), "a number, not the form's text: {list:?}");
        let back = Settings::load(&wind_base::Config::load(&dir).unwrap());
        assert_eq!(back.ocr_image_crop_urbl, vec![0, 0, 12, 0, 5, 5, 5, 5]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclude_words_are_trimmed_deduped_and_never_empty_lines() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::ExcludeWords, "  KeePass\n\nEnpass.exe \nKeePass\n");
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(parsed.exclude_words, vec!["Enpass.exe".to_string(), "KeePass".to_string()]);
    }

    #[test]
    fn an_empty_ocr_lang_is_refused_rather_than_stored() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::OcrLang, "   ");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.ocr_lang, "zh-Hans-CN");
        assert!(notes[0].contains("must not be empty"), "{notes:?}");
    }

    /// The engine picker is the regression the user reported: the setting has to be *writable*, and it has
    /// to write a name the recorder can resolve.
    #[test]
    fn choosing_another_ocr_engine_is_stored_under_upstreams_own_key() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::OcrEngine, wind_base::ocr::TESSERACT_ENGINE);
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(parsed.ocr_engine, wind_base::ocr::TESSERACT_ENGINE);

        let mut config = Config::load(&repo_root()).unwrap();
        parsed.stage(&mut config);
        assert_eq!(config.str_or(wind_base::ocr::ENGINE_KEY, "?"), wind_base::ocr::TESSERACT_ENGINE);
        // The name is the one the recorder's own door resolves, so the picker and the engine cannot drift.
        assert_eq!(wind_base::ocr::configured_name(&config), wind_base::ocr::TESSERACT_ENGINE);
    }

    /// Every row the page edits has to reach the file.
    ///
    /// This is the guard for a class, not for one field: a `Field` can be added to `Field::ALL` with its
    /// label, its help, its widget, its `key`, its `get` and its `load` all wired, and no line in `stage` —
    /// and nothing else in the product will notice. The settings page then edits a value it shows, saves it,
    /// is told "written", and prints the old value on the next read. That is what happened to the two clock
    /// boxes (`maintain_window_start` / `maintain_window_end`), which the user reported as "保存后立刻变为
    /// 默认值"; the schedule they set was real in the window and never existed on disk.
    #[test]
    fn every_field_the_page_edits_survives_a_stage() {
        let dir = std::env::temp_dir().join(format!("windui-stage-all-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        // An empty default file: anything `stage` fails to write reads back as *this build's default*,
        // which is exactly the value the user said the box jumped to.
        std::fs::write(dir.join("config_src/config_default.json"), b"{}").unwrap();

        let base = settings();
        let mut draft = Draft::from(&base);
        // A value per field that differs from the fixture's own, so a dropped write cannot hide behind a
        // default that happens to equal what was typed.
        let typed: Vec<(Field, &str)> = vec![
            (Field::MaxPageResult, "17"),
            (Field::TimelinePics, "77"),
            (Field::DayBeginMinutes, "120"),
            (Field::SimilarChars, "false"),
            (Field::OcrLang, "en-US"),
            (Field::OcrEngine, wind_base::ocr::TESSERACT_ENGINE),
            (Field::Lang, "sc"),
            (Field::ExcludeWords, "Enpass.exe"),
            (Field::Mask, "1, 2, 3, 4, 5, 6, 7, 8"),
            (Field::Highlight, "false"),
            (Field::ThumbWidth, "640"),
            (Field::CloseToTray, "false"),
            (Field::MaintainStart, "03:30"),
            (Field::MaintainEnd, "05:00"),
            (Field::StartOnBoot, "true"),
        ];
        assert_eq!(typed.len(), Field::ALL.len(), "a field added to the page needs a value here too");
        for (field, text) in &typed {
            draft.set_text(*field, text);
        }
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "every value above is legal for its own row: {notes:?}");

        let mut config = wind_base::Config::load(&dir).unwrap();
        parsed.stage(&mut config);
        config.save().unwrap();
        let back = Settings::load(&wind_base::Config::load(&dir).unwrap());

        for (field, text) in &typed {
            let stored = render(back.get(*field));
            assert_eq!(
                stored,
                render(parsed.get(*field)),
                "{} was staged and did not come back — the file holds {stored:?}, the page typed {:?}",
                field.key(),
                text
            );
        }
        // The two boxes the bug was about, named: a schedule that never reaches the file cannot be
        // scheduled by anything, and `Config::maintain_window` is the reader the recorder asks.
        assert_eq!(back.maintain_window_start, "03:30");
        assert_eq!(back.maintain_window_end, "05:00");
        assert_eq!(
            back.maintain_window_start.as_str(),
            wind_base::Config::load(&dir).unwrap().str_or("maintain_window_start", "?").as_str(),
            "the key the recorder reads is the key the page wrote"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A value the install cannot drive is refused out loud, not accepted and then ignored — which is
    /// exactly the shape of the `use_native_core` bug this product already fixed once.
    #[test]
    fn a_python_hosted_engine_is_not_accepted_by_the_picker() {
        let base = settings();
        let mut draft = Draft::from(&base);
        draft.set_text(Field::OcrEngine, "PaddleOCR");
        let (parsed, notes) = draft.validate(&base, &options());
        assert_eq!(parsed.ocr_engine, wind_base::ocr::WINDOWS_ENGINE, "unchanged");
        assert!(notes[0].contains("not one of"), "{notes:?}");
    }

    /// An install migrated from the Python app whose `ocr_engine` says `PaddleOCR` must still show that
    /// value, or saving any other field would silently rewrite the user's choice.
    #[test]
    fn a_stored_engine_the_install_cannot_drive_stays_selectable_and_named() {
        let mut base = settings();
        base.ocr_engine = "PaddleOCR".into();
        let choices = options().engine_choices(&base.ocr_engine);
        assert!(choices.iter().any(|choice| choice.value == "PaddleOCR"), "{choices:?}");
        // And the page says which names it left out, rather than leaving the user to notice.
        assert_eq!(options().unavailable_engines(), vec!["PaddleOCR".to_string()]);
        let draft = Draft::from(&base);
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(parsed.ocr_engine, "PaddleOCR");
    }

    #[test]
    fn the_language_picker_offers_what_the_catalog_holds_in_its_own_name() {
        let base = settings();
        let choices = options().language_choices(&base.lang);
        assert_eq!(choices.len(), 3, "{choices:?}");
        assert_eq!(choices[1].value, "sc");
        assert_eq!(choices[1].label, "简体中文", "a Chinese menu must be able to say so in Chinese");

        let mut draft = Draft::from(&base);
        assert_eq!(draft.label_of(Field::Lang, &options(), &base), "English", "the stored `en` reads as its own name");
        draft.set_text(Field::Lang, "sc");
        assert_eq!(draft.label_of(Field::Lang, &options(), &base), "简体中文", "what the row shows is never what the file stores");
        let (parsed, notes) = draft.validate(&base, &options());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(parsed.lang, "sc");
        let after = Draft::from(&parsed);
        assert_eq!(after.label_of(Field::Lang, &options(), &parsed), "简体中文");
    }

    /// A hand-typed locale is kept and named, not dropped: the file is the user's, and a picker that
    /// silently resets it is a picker that loses a setting.
    #[test]
    fn a_locale_the_catalog_has_no_table_for_is_kept_and_reported() {
        let mut base = settings();
        base.lang = "zh".into();
        let choices = options().language_choices(&base.lang);
        assert!(choices.iter().any(|choice| choice.value == "zh"), "{choices:?}");
        assert_eq!(options().unknown_language("zh").unwrap(), "en, sc, ja");
        assert!(options().unknown_language("ja").is_none());
    }

    #[test]
    fn day_begin_minutes_reads_as_a_clock_time() {
        assert_eq!(hhmm(180), "03:00");
        assert_eq!(hhmm(0), "00:00");
        assert_eq!(hhmm(59), "00:59");
    }

    /// Every field has a catalog row in both doors — and in all three shipped locales, since a `ja`
    /// install reads this page too — and the English the binary carries is what shows when a
    /// translation is missing, so a settings page can never render a widget with no name.
    #[test]
    fn every_field_names_itself_in_the_catalog_and_in_english() {
        for locale in ["en", "sc", "ja"] {
            let catalog = Catalog::load(&repo_root(), locale);
            for field in Field::ALL {
                assert!(!field.label().is_empty() && !field.help().is_empty(), "{field:?}");
                assert_ne!(field.label_key(), field.help_key(), "{field:?}");
                assert_ne!(catalog.text(field.label_key()), wind_base::i18n::missing(field.label_key()), "the shipped {locale} catalog has no label row for {:?}", field.key());
                assert_ne!(catalog.text(field.help_key()), wind_base::i18n::missing(field.help_key()), "the shipped {locale} catalog has no help row for {:?}", field.key());
                assert_eq!(catalog.text_or(field.label_key(), field.label()), catalog.text(field.label_key()), "{:?} must not need the Rust fallback", field.key());
            }
        }
    }

    /// The shipped defaults still parse into the ten-field form, which is what proves the field list and
    /// `config_default.json` describe the same settings.
    #[test]
    fn the_shipped_config_answers_every_field() {
        let config = Config::load(&repo_root()).unwrap();
        let read = Settings::load(&config);
        assert_eq!(read.ocr_engine, wind_base::ocr::WINDOWS_ENGINE);
        assert_eq!(read.lang, config.str_or("lang", "en"));
        let draft = Draft::from(&read);
        let (parsed, notes) = draft.validate(&read, &Options::scan(&repo_root(), &config));
        assert!(notes.is_empty(), "{notes:?}");
        // Two things a round trip of the shipped file is allowed to change, and both are the form making
        // what the engine already does explicit rather than doing anything new: `exclude_words` is sorted
        // and de-duplicated, and the mask is written out once per screen this machine has. The shipped
        // default carries one group; on a four-panel desk the painted result is identical either way,
        // because a slot the list does not reach takes the same 6/6/6/3 — but the file now says so.
        let mut expected = read.clone();
        expected.exclude_words.sort();
        expected.ocr_image_crop_urbl =
            fit_mask(&read.ocr_image_crop_urbl, Options::scan(&repo_root(), &config).mask_groups(&read.ocr_image_crop_urbl));
        assert_eq!(parsed, expected);
    }

    fn repo_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(Path::to_path_buf).unwrap()
    }
}
