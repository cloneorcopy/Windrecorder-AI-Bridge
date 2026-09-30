//! Every read and write the HTML window is allowed to ask for.
//!
//! These are thin on purpose: each one loads what it needs from the install, calls the same
//! `wind-ui` function the egui window calls, and returns it. There is no query logic, no validation and
//! no path building in here, because that logic already exists — in `backend`, `settings` and
//! `record` — and a second copy in this file is how the two windows start answering different questions
//! about the same afternoon.
//!
//! `Env` is rebuilt per call rather than kept in managed state. It is a config map, a list of month
//! files and a shared segment cache (`backend::Env::load`), so the cost is one directory listing per
//! user action — a search, a day, a refresh — against the alternative of proving the whole type
//! `Send + Sync` for a window that spends its time idle behind the tray. The egui window caches it
//! because it repaints at 60 Hz; this one paints nothing.
//!
//! ## Why the forms are described, not hard-coded
//!
//! `settings_read` and `recording_read` return the field list with its kind, bounds, options, help text
//! and current value, and the front end renders whatever it is told. That is not generic-ui laziness:
//! `Field::ALL` and `RField::ALL` are already the single lists that drive the *validation* on the Rust
//! side, so a form built from the same list cannot offer a switch the engine ignores. This product has
//! one such dead control in its history (`use_native_core`), and the commit that removed it deleted the
//! control rather than deriving it. Deriving is what stops the next one.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wind_ui::model::SearchParams;
use wind_ui::{backend, model, record, settings, wordcloud};
use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_base::prompts::Name as PromptName;
use wind_summary as summary;

use crate::video;

/// The install this window is showing. Managed state, and the only thing in it.
pub struct State {
    pub root: PathBuf,
    /// Which screen to open on, when the caller said. `None` leaves the front end's own default.
    pub tab: Option<String>,
}

impl State {
    fn env(&self) -> Result<backend::Env, String> {
        backend::Env::load(&self.root)
    }
}

/// One command's work, run off the thread the window paints on.
///
/// A plain `fn` command runs on the main thread, so anything that waits — a month of OCR read out of the
/// index, one ffmpeg seek into a segment, a round trip to an AI endpoint — stops the window mid-paint.
/// The user's words for it were "卡住主程序": the scroll bars freeze, the close button stops answering,
/// and the picture they are waiting for is the last thing to appear. These bodies are blocking file and
/// process work rather than futures, so they go to the blocking pool instead of being awaited on the
/// runtime, which would tie up a worker that expects not to sleep.
///
/// The commands that stay on the main thread are the ones that answer in milliseconds from a config file
/// already in memory — `about`, `ui_strings`, the form reads and the form writes. Moving them would buy
/// nothing and would put every one of their `state` borrows behind a `'static` boundary.
async fn off_main<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(work).await {
        // The job's own answer, or the sentence for a thread that died with it — a panic in the work is
        // reported the same way a failure to read is, because both leave the window with nothing to show.
        Ok(answer) => answer,
        Err(e) => Err(format!("this job did not finish: {e}")),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct About {
    pub root: String,
    pub version: String,
    /// How many month files the index holds. The difference between an empty window that is correct
    /// and an empty window that is broken, which a viewer cannot decide from the pixels alone.
    pub months: usize,
}

#[tauri::command]
pub fn about(state: tauri::State<'_, State>) -> Result<About, String> {
    let env = state.env()?;
    Ok(About {
        root: state.root.display().to_string(),
        version: wind_base::version::line("winduiweb", env!("CARGO_PKG_VERSION")),
        months: env.months.len(),
    })
}

/// The library's shape: how many months, how many rows, and whether a month file refused to open.
///
/// `backend::scan` reports through a progress callback because the egui window paints that progress
/// while the count runs. Folding it to the last report is deliberate: the webview has no frame loop to
/// animate against, and inventing an event stream to display a number that is ready in milliseconds
/// would be a second way to be wrong. `first`/`last` are not forwarded either — they are raw
/// `videofile_time` seconds, and the window that shows them needs a formatted stamp, which the cards
/// already carry as `clock` and `day`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub months_total: usize,
    pub months_scanned: usize,
    pub rows: i64,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn library_stats(state: tauri::State<'_, State>) -> Result<Stats, String> {
    let root = state.root.clone();
    off_main(move || {
        let env = backend::Env::load(&root)?;
        let mut out = Stats { months_total: env.months.len(), months_scanned: 0, rows: 0, error: None };
        backend::scan(&env, |progress| {
            out = Stats {
                months_total: progress.months_total,
                months_scanned: progress.months_scanned,
                rows: progress.rows,
                error: progress.error.clone(),
            };
        });
        Ok(out)
    })
    .await
}

#[tauri::command]
pub async fn search(state: tauri::State<'_, State>, params: SearchParams) -> Result<model::SearchOutcome, String> {
    let root = state.root.clone();
    off_main(move || backend::run_search(&backend::Env::load(&root)?, &params)).await
}

#[tauri::command]
pub async fn day(state: tauri::State<'_, State>, year: i64, month: u32, day: u32) -> Result<model::DayOutcome, String> {
    let root = state.root.clone();
    off_main(move || backend::load_day(&backend::Env::load(&root)?, wind_base::clock::LocalParts { year, month, day, hour: 0, minute: 0, second: 0 })).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub month: model::MonthTotals,
    pub year: model::YearTotals,
}

#[tauri::command]
pub async fn totals(state: tauri::State<'_, State>, year: i64, month: u32) -> Result<Totals, String> {
    let root = state.root.clone();
    off_main(move || {
        let env = backend::Env::load(&root)?;
        Ok(Totals { month: backend::month_totals(&env, year, month)?, year: backend::year_totals(&env, year)? })
    })
    .await
}

/// Reveal a recorded segment in Explorer — `backend::locate`, the same call the egui card makes.
///
/// Named and described the same way in both windows. It used to be the *only* door a row's footage had,
/// and `play_source` beside it is the reason that stopped being true: this is now the way out to another
/// program, not the way to watch.
#[tauri::command]
pub async fn locate(path: String) -> Result<(), String> {
    off_main(move || backend::locate(&PathBuf::from(path))).await
}

/// Where one row's segment can be played from, if this install still holds it.
///
/// The argument is a row *key*, exactly like `frame`, and for exactly the same reason: the window names the
/// row it means and the index decides which file that is, so a front end cannot steer the player at a file
/// the index does not name ([`crate::video`]'s header states the rule the protocol itself enforces). It is
/// also what lets a lightbox tile — a key, a time and a preview, and deliberately nothing more — open the
/// same segment a result card does.
///
/// `offset` is returned rather than taken on trust from an earlier query: it is the row's own
/// `videofile_time` measured against the segment that holds it, and it is the number the player seeks to,
/// which is the whole promise of "watch this moment".
///
/// `Ok(None)` is "there is nothing to play": the segment went out with `vid_store_day`, or this machine
/// never had an ffmpeg to encode it. The window says so out loud rather than opening a box that shows black.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaySource {
    /// The segment this door will answer, named the way the index stores it. For a copy produced by
    /// [`play_prepare`], the copy's own name.
    pub name: String,
    /// What to point a `<video>` at, spelled the way this platform's webview reaches a custom scheme.
    pub url: String,
    /// Seconds into the segment, from the row itself. `None` for a row that cannot be placed in it.
    pub offset: Option<i64>,
    /// `h264`, `hevc`, `av1`, `vp9`, or `unknown` — read out of the file, not out of the config that
    /// wrote it, because a library holds footage from before whatever the encoder is set to now.
    pub codec: String,
    /// The RFC 6381 string to ask `canPlayType` with, or `None` when the codec is one no player names.
    /// The window asks its own webview rather than being told: HEVC plays in this webview only if the
    /// optional store extension is installed, and that is a fact about the machine, not about the file.
    pub mime: Option<String>,
}

#[tauri::command]
pub async fn play_source(state: tauri::State<'_, State>, key: model::RowKey) -> Result<Option<PlaySource>, String> {
    let root = state.root.clone();
    off_main(move || {
        let env = backend::Env::load(&root)?;
        let card = backend::card_of_key(&env, &key)?;
        // `segment_path` is the answer of the same resolver the Locate button uses, so the two doors cannot
        // disagree about whether this row has footage.
        if card.segment_path.is_none() {
            return Ok(None);
        }
        let Some(name) = video::bare_name(&card.segment) else {
            // A row whose stored name is not a bare segment name is a corrupt index, not a request to be
            // humoured: there is no URL that would be safe to hand back for it.
            return Err(format!("'{}' is not a segment name this window may open", card.segment));
        };
        let file = card.segment_path.clone().unwrap_or_default();
        let codec = video::codec_of(&file);
        Ok(Some(PlaySource {
            name: name.to_string(),
            url: video::source_url(name),
            offset: card.offset,
            mime: video::codec_mime(&codec).map(str::to_string),
            codec,
        }))
    })
    .await
}

/// A copy of this row's segment that the window's own media element can decode.
///
/// The recorder writes HEVC or AV1 when the user picks `NVIDIA_h265` or `SVT-AV1`, and a `<video>` on
/// Windows shows nothing for either without the store's HEVC extension. The row was therefore reported as
/// "the file is there and this machine cannot decode it" — true, and useless, on a machine that had just
/// spent hours producing the file. `ffmpeg` is the decoder this product already trusts for every other
/// piece of footage work, so the door asks it for one H.264 copy, keeps it in `cache\playback`, and hands
/// the player that instead. Called only when the webview itself says it cannot play the codec
/// ([`PlaySource::mime`]), so a machine with the extension never waits for a copy it did not need.
#[tauri::command]
pub async fn play_prepare(state: tauri::State<'_, State>, key: model::RowKey) -> Result<Option<PlaySource>, String> {
    let root = state.root.clone();
    off_main(move || {
        let env = backend::Env::load(&root)?;
        let card = backend::card_of_key(&env, &key)?;
        let Some(source) = card.segment_path.clone() else { return Ok(None) };
        let codec = video::codec_of(&source);
        let copy = video::playable_copy(&env.config, &source)?;
        let name = copy
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "the copy this window made has no name it could be served under".to_string())?
            .to_string();
        Ok(Some(PlaySource {
            url: video::copy_url(&name),
            offset: card.offset,
            mime: video::codec_mime("h264").map(str::to_string),
            name,
            codec,
        }))
    })
    .await
}

/// One form row as the renderer needs it: label, explanation, control kind, legal range, options, and
/// the value stored right now.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldDto {
    pub key: String,
    pub label: String,
    pub help: String,
    /// `"int" | "fraction" | "bool" | "text" | "lines" | "choice"`.
    pub kind: String,
    /// The bound that applies to this kind: a number's range, or a text/lines limit. `None` when the
    /// kind has no bound to draw.
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub options: Vec<String>,
    /// What each entry of `options` is called, positionally. Equal to `options` for an engine, which is
    /// named the same in every language, and different for a locale: the config stores `sc`, the row
    /// reads 简体中文.
    pub option_labels: Vec<String>,
    /// A dynamic sentence the row needs beside its static help — which engines this install refused, for
    /// instance. `None` for a row with nothing to add.
    pub note: Option<String>,
    pub group: Option<String>,
    /// A number, a bool, a string, or an array of strings — the `Typed`/`RTyped` behind `kind`.
    pub value: Value,
    /// One heading per group of the mask row — a screen this machine has plugged in. Empty for every
    /// other kind, because no other row is drawn per display.
    pub panels: Vec<String>,
    /// The four edges a mask group edits, already named in the language the row is drawn in. Empty for
    /// every other kind; `options` stays the picker's business and is not reused for this.
    pub edges: Vec<String>,
}

fn row(kind: &str, min: Option<f64>, max: Option<f64>, options: Vec<String>) -> FieldDto {
    FieldDto {
        key: String::new(),
        label: String::new(),
        help: String::new(),
        kind: kind.into(),
        min,
        max,
        option_labels: options.clone(),
        options,
        note: None,
        group: None,
        value: Value::Null,
        panels: Vec::new(),
        edges: Vec::new(),
    }
}

/// The catalog of the install this call is answering for, in the language that install is set to.
///
/// Loaded per call, like `Env`: the form's English would otherwise be the English `Config` was first read
/// with, and the language row changes precisely that value. Re-reading is what lets the page relabel
/// itself the moment the user picks another language, which is the whole point of offering the choice.
fn catalog(state: &State) -> wind_base::i18n::Catalog {
    let lang = Config::load(&state.root).map(|c| c.str_or("lang", "en")).unwrap_or_else(|_| "en".to_string());
    wind_base::i18n::Catalog::load(&state.root, &lang)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormDto {
    pub page: &'static str,
    pub fields: Vec<FieldDto>,
}

#[tauri::command]
pub fn settings_read(state: tauri::State<'_, State>) -> Result<FormDto, String> {
    let env = state.env()?;
    let options = settings::Options::scan(&state.root, &env.config);
    let catalog = catalog(&state);
    let refused = options.unavailable_engines();
    let fields = settings::Field::ALL
        .iter()
        .map(|field| {
            let mut dto = match field.kind(&options, &env.settings) {
                settings::Kind::Int { min, max } => row("int", Some(min as f64), Some(max as f64), vec![]),
                settings::Kind::Bool => row("bool", None, None, vec![]),
                settings::Kind::Text { max_chars } => row("text", None, Some(max_chars as f64), vec![]),
                settings::Kind::Lines { max_entries } => row("lines", None, Some(max_entries as f64), vec![]),
                // The mask row carries its own geometry: one heading per screen the machine reports, and
                // the four edge names already translated. The numbers themselves travel as the same
                // comma-separated text the egui draft holds, so one save path serves both windows.
                settings::Kind::Urbl { slots } => {
                    let mut dto = row("urbl", Some(0.0), Some(settings::MASK_EDGE_MAX as f64), vec![]);
                    dto.panels = (0..slots)
                        .map(|index| match options.mask_panels.get(index) {
                            Some((width, height)) => format!("#{} · {}×{}", index + 1, width, height),
                            None => format!("#{}", index + 1),
                        })
                        .collect();
                    dto.edges = ["set_text_top_padding", "set_text_right_padding", "set_text_bottom_padding", "set_text_left_padding"]
                        .into_iter()
                        .zip(["Top", "Right", "Bottom", "Left"])
                        .map(|(key, fallback)| catalog.text_or(key, fallback))
                        .collect();
                    dto
                }
                settings::Kind::Choice(choices) => {
                    let values = choices.iter().map(|c| c.value.clone()).collect();
                    let mut dto = row("choice", None, None, values);
                    dto.option_labels = choices.iter().map(|c| c.label.clone()).collect();
                    dto
                }
            };
            dto.key = field.key().to_string();
            dto.label = catalog.text_or(field.label_key(), field.label());
            dto.help = catalog.text_or(field.help_key(), field.help());
            dto.value = match env.settings.get(*field) {
                settings::Typed::Int(v) => Value::from(v),
                settings::Typed::Bool(v) => Value::from(v),
                settings::Typed::Text(v) => Value::from(v),
                settings::Typed::Lines(v) => Value::from(v),
                settings::Typed::Ints(v) => Value::from(v.iter().map(i64::to_string).collect::<Vec<_>>().join(", ")),
            };
            // The engine row is the one with something to confess: names the user registered that this
            // binary cannot run are listed out loud rather than left out of the picker in silence.
            if *field == settings::Field::OcrEngine && !refused.is_empty() {
                dto.note = Some(catalog.formatted_or(
                    "set_refused_ocr_engines",
                    &[("engines", &refused.join(", "))],
                    "{engines} are listed but this install cannot run them",
                ));
            }
            if *field == settings::Field::Lang {
                dto.note = options.unknown_language(&env.settings.lang).map(|held| {
                    catalog.formatted_or(
                        "set_unknown_lang",
                        &[("lang", &env.settings.lang), ("held", &held)],
                        "\"{lang}\" is not one of the languages this catalog holds ({held})",
                    )
                });
            }
            // A preview narrower than the box it is drawn in is *stretched*, and stretching is what a
            // person reports as blur. This row cannot be allowed to look like a dead control: the number
            // changes only the rows written from now on, and the rows already on disk wait for the idle
            // pass. Both halves of that have to be beside the number, not in a changelog nobody reads.
            if *field == settings::Field::ThumbWidth && env.config.preview_is_a_stamp() {
                let stored = env.config.thumbnail_width().to_string();
                let floor = wind_base::image::CARD_PREVIEW_FLOOR.to_string();
                dto.note = Some(catalog.formatted_or(
                    "set_note_small_preview",
                    &[("stored", stored.as_str()), ("floor", floor.as_str())],
                    "This install stores previews {stored} px wide, and a result card is drawn about {floor} px across, so every picture in the window is being stretched. Raise this row and the idle maintenance pass redraws the rows already on disk from their retained screenshot or video.",
                ));
            }
            dto
        })
        .collect();
    Ok(FormDto { page: "settings", fields })
}

#[tauri::command]
pub fn recording_read(state: tauri::State<'_, State>) -> Result<FormDto, String> {
    let env = state.env()?;
    let options = backend::rec_options(&state.root);
    let current = record::Rec::load(&env.config);
    let catalog = catalog(&state);
    let fields = record::RField::ALL
        .iter()
        .map(|field| {
            let mut dto = match field.kind(&options, &current) {
                record::Kind::Int { min, max } => row("int", Some(min as f64), Some(max as f64), vec![]),
                record::Kind::Fraction { min, max } => row("fraction", Some(min), Some(max), vec![]),
                record::Kind::Bool => row("bool", None, None, vec![]),
                record::Kind::Choice(options) => row("choice", None, None, options),
            };
            dto.key = field.key().to_string();
            // The row's words come from the catalog now. This page used to forward Rust's English
            // `&'static str`, which is precisely why an install set to Chinese still read English here.
            dto.label = catalog.text_or(field.label_key(), field.label());
            dto.help = catalog.text_or(field.help_key(), field.help());
            dto.group = Some(catalog.text_or(field.group_key(), field.group()));
            dto.value = match current.get(*field) {
                record::RTyped::Int(v) => Value::from(v),
                record::RTyped::Float(v) => Value::from(v),
                record::RTyped::Bool(v) => Value::from(v),
                record::RTyped::Text(v) => Value::from(v),
            };
            dto
        })
        .collect();
    Ok(FormDto { page: "recording", fields })
}

/// Typed form values as strings, keyed by the config key.
///
/// Strings for every kind on purpose: `Draft`/`RecDraft` hold what the user typed rather than what the
/// text parses to, because "7" mid-deletion and 7 are not the same thing and a numeric JSON input would
/// re-decide that question here, in the wrong layer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormInput {
    pub values: HashMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub ok: bool,
    /// What `validate` refused, verbatim. A save that quietly dropped an illegal field is the failure
    /// this return type exists to prevent.
    pub problems: Vec<String>,
    /// The file the merged map was written to, on success.
    pub written: Option<String>,
    /// What the save did *outside* the config file, in words. The sign-in registry entry is the one
    /// setting here that a `config_user.json` cannot hold, so the page has to be told what became of it
    /// — and told even when it failed, because a checkbox and a registry that disagree is the dead
    /// control this product has already shipped once.
    pub notices: Vec<String>,
}

/// Stage one validated draft onto a freshly loaded config and write it.
///
/// The reload is the point: `Config::save` writes the whole merged map, so staging onto a config
/// cached from an earlier click would silently drop anything the recorder or a second window wrote in
/// between. One rename is cheap; a lost setting is not.
fn write_form(root: &Path, stage: impl FnOnce(&mut Config) -> Vec<String>) -> Result<SaveResult, String> {
    let mut config = Config::load(root).map_err(|e| e.to_string())?;
    let problems = stage(&mut config);
    if !problems.is_empty() {
        return Ok(SaveResult { ok: false, problems, written: None, notices: vec![] });
    }
    let written = config.save().map_err(|e| e.to_string())?;
    Ok(SaveResult { ok: true, problems: vec![], written: Some(written.display().to_string()), notices: vec![] })
}

#[tauri::command]
pub fn settings_save(state: tauri::State<'_, State>, input: FormInput) -> Result<SaveResult, String> {
    let env = state.env()?;
    let base = env.settings.clone();
    let options = settings::Options::scan(&state.root, &env.config);
    // Carried out of the closure rather than re-read from the form's text: this is the value the draft
    // parsed and validated, and the registry must be moved to exactly what the file now says.
    let mut boot: Option<bool> = None;
    let mut outcome = write_form(&state.root, |config| {
        let mut draft = settings::Draft::from(&base);
        for field in settings::Field::ALL {
            if let Some(text) = input.values.get(field.key()) {
                draft.set_text(field, text);
            }
        }
        let (validated, problems) = draft.validate(&base, &options);
        if problems.is_empty() {
            validated.stage(config);
            boot = Some(validated.start_app_on_boot);
        }
        problems
    })?;
    // The sign-in entry is the one setting in this form that cannot live in a JSON file. Applied only
    // after the write succeeded, and reported either way: a checkbox that quietly disagrees with
    // `HKCU\...\Run` is a control the user believes and the machine ignores.
    if outcome.ok {
        if let Some(want) = boot {
            match wind_base::autostart::apply(&state.root, want) {
                wind_base::autostart::Outcome::Unchanged => {}
                wind_base::autostart::Outcome::Changed(sentence) | wind_base::autostart::Outcome::Failed(sentence) => {
                    outcome.notices.push(sentence);
                }
            }
        }
    }
    Ok(outcome)
}

#[tauri::command]
pub fn recording_save(state: tauri::State<'_, State>, input: FormInput) -> Result<SaveResult, String> {
    let env = state.env()?;
    let options = backend::rec_options(&state.root);
    let base = record::Rec::load(&env.config);
    write_form(&state.root, |config| {
        let mut draft = record::RecDraft::from(&base);
        for field in record::RField::ALL {
            if let Some(text) = input.values.get(field.key()) {
                draft.set_text(field, text);
            }
        }
        let (validated, problems) = draft.validate(&base, &options);
        if problems.is_empty() {
            validated.stage(config);
        }
        problems
    })
}

/// The monitors the recorder can be pointed at, from `backend::displays` — the one Win32 truth in this
/// window that cannot be delegated to the front end, because the 1-based numbering has to match what
/// `windrec` will later honour.
#[tauri::command]
pub fn displays() -> Vec<record::DisplayInfo> {
    backend::displays()
}

/// The AI page, as a form plus the two verdicts the page exists to show.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiForm {
    pub fields: Vec<FieldDto>,
    /// `KeyState::describe` — whether a usable key is already stored, without its value or its length.
    pub key_state: String,
    /// `windai`'s own answer about the stored configuration: "ready", or which key needs editing.
    pub ready: bool,
    pub verdict: String,
    /// The bridge's answer about this install, as the bridge gives it: the address it binds, whether
    /// anything is answering there right now, the URL to paste, and the refusal it would print if it
    /// would not start.
    pub bridge: BridgeDto,
}

/// [`wind_ui::ai::BridgeStatus`] as the front end consumes it: the *catalog key* of the sentence that
/// applies, not the sentence. Which state applies is decided once in `wind-ui`; translating it here and
/// in the egui window separately is how the two windows start describing one service two ways.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeDto {
    /// The state, already in the language the window speaks — resolved here, the same way every other
    /// label on this page is, so the front end holds no table of its own.
    pub state: String,
    pub authority: String,
    pub url: String,
    pub enabled: bool,
    pub listening: bool,
    pub refused: Option<String>,
}

impl BridgeDto {
    fn of(status: wind_ui::ai::BridgeStatus, catalog: &wind_base::i18n::Catalog) -> BridgeDto {
        let (key, english) = status.state_row();
        BridgeDto {
            state: catalog.text_or(key, english),
            authority: status.authority(),
            url: status.url,
            enabled: status.enabled,
            listening: status.listening,
            refused: status.refused,
        }
    }
}

/// A probe result or a readiness answer: both are already redacted by `wind-ai` on the way out, which
/// is why shipping them as a pair of strings is safe here and would not be safe with a hand-built
/// message.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiStatus {
    pub ok: bool,
    pub message: String,
}

/// What the AI page shows per field: kind, bounds, help, current value — except the key, which is
/// `secret` with no value. The egui page seeds that box blank because there is no way to show what is
/// stored without putting the token in a text field, and the same reason applies with equal force to
/// handing it to a webview: an empty box means "leave what is there", not "clear it".
#[tauri::command]
pub fn ai_read(state: tauri::State<'_, State>) -> Result<AiForm, String> {
    let env = state.env()?;
    let form = wind_ui::ai::AiSettings::load(&env.config);
    let catalog = catalog(&state);
    let fields = wind_ui::ai::AField::ALL
        .iter()
        .map(|field| {
            let mut dto = match field.kind(&form) {
                wind_ui::ai::AKind::Int { min, max } => row("int", Some(min as f64), Some(max as f64), vec![]),
                wind_ui::ai::AKind::Text { max_chars } => row("text", None, Some(max_chars as f64), vec![]),
                wind_ui::ai::AKind::Secret { max_chars } => row("secret", None, Some(max_chars as f64), vec![]),
                wind_ui::ai::AKind::Lines { max_entries } => row("lines", None, Some(max_entries as f64), vec![]),
                wind_ui::ai::AKind::Choice(options) => row("choice", None, None, options),
                wind_ui::ai::AKind::Bool => row("bool", None, None, vec![]),
            };
            dto.key = field.key().to_string();
            // Resolved through the catalog like every other label in the product: this page used to
            // forward Rust's English `&'static str` straight through, which is why a Chinese install
            // still read English here.
            dto.label = catalog.text_or(field.label_key(), field.label());
            dto.help = catalog.text_or(field.help_key(), field.help());
            dto.group = Some(catalog.text_or(field.group_key(), field.group()));
            if dto.kind != "secret" {
                dto.value = match form.get(*field) {
                    wind_ui::ai::ATyped::Int(v) => Value::from(v),
                    wind_ui::ai::ATyped::Bool(v) => Value::from(v),
                    wind_ui::ai::ATyped::Text(v) => Value::from(v),
                    wind_ui::ai::ATyped::Lines(v) => Value::from(v),
                };
            }
            dto
        })
        .collect();
    let state_text = wind_ui::ai::key_state(&env.config, &form).describe_in(&catalog);
    let verdict = wind_ui::ai::verdict(&env.config, &form);
    // Read from the install, not from the draft: this row answers "where does the service listen", and
    // the service reads the file when the tray starts it. What the person is mid-way through typing is
    // answered by `ai_save` refusing it, through the same `startup_guard`.
    let bridge = wind_ui::ai::bridge_status(&state.root);
    Ok(AiForm { fields, key_state: state_text, ready: verdict.ok, verdict: verdict.message, bridge: BridgeDto::of(bridge, &catalog) })
}

/// The values the page is holding, staged onto a fresh config. Shared by save and test so the two can
/// never disagree about what "what I typed" means.
fn stage_ai(
    config: &Config,
    input: &AiInput,
) -> Result<(wind_ui::ai::AiSettings, Vec<String>), String> {
    let form = wind_ui::ai::AiSettings::load(config);
    let mut draft = wind_ui::ai::AiDraft::from(&form);
    for field in wind_ui::ai::AField::ALL {
        if let Some(text) = input.values.get(field.key()) {
            draft.set_text(field, text);
        }
    }
    if input.clear_key {
        draft.clear_key();
    }
    Ok(draft.validate(&form))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiInput {
    pub values: HashMap<String, String>,
    /// The box that says "throw the stored token away", kept separate from the token box because an
    /// empty text field cannot mean both "unchanged" and "deleted".
    #[serde(default)]
    pub clear_key: bool,
}

#[tauri::command]
pub fn ai_save(state: tauri::State<'_, State>, input: AiInput) -> Result<SaveResult, String> {
    let seed = Config::load(&state.root).map_err(|e| e.to_string())?;
    let (staged, problems) = stage_ai(&seed, &input)?;
    if !problems.is_empty() {
        return Ok(SaveResult { ok: false, problems, written: None, notices: vec![] });
    }
    write_form(&state.root, move |config| {
        staged.stage(config);
        vec![]
    })
}

/// One real round trip against the endpoint the page is holding, through the same
/// `wind_ai::client::Client` the CLI and the egui page use — which is the only way the answer means
/// what it says about the shipped transport.
#[tauri::command]
pub async fn ai_test(state: tauri::State<'_, State>, input: AiInput) -> Result<AiStatus, String> {
    let root = state.root.clone();
    // A round trip to somebody else's endpoint is the one call in this window whose duration is not the
    // user's machine: an unreachable host answers on its own schedule, and the page must keep painting
    // while the button spins.
    off_main(move || {
        let config = Config::load(&root).map_err(|e| e.to_string())?;
        let (staged, problems) = stage_ai(&config, &input)?;
        if !problems.is_empty() {
            return Ok(AiStatus { ok: false, message: problems.join("\n") });
        }
        let report = wind_ui::ai::probe(&config, &staged);
        Ok(AiStatus { ok: report.ok, message: report.message })
    })
    .await
}

#[tauri::command]
pub async fn lightbox(state: tauri::State<'_, State>, year: i64, month: u32) -> Result<Vec<model::LightboxTile>, String> {
    let root = state.root.clone();
    off_main(move || backend::lightbox(&backend::Env::load(&root)?, year, month)).await
}

/// The frame a row was indexed from, at the resolution it was recorded.
///
/// A webview cannot read a disk, so this is the one place the HTML window ships an image as bytes instead
/// of a path — and it ships *one* image, on the click, rather than a page of them: a 1080p JPEG is a few
/// hundred KB, and the thumbnail grid stays on the small stored blob it already has.
///
/// The argument is a row *key*, not a card. The window re-reads the row from the index, so the paths a
/// frame is opened from are the ones the index holds rather than the ones whoever is asking named: a
/// front end that can be handed `picturePath` is a front end that can be handed any path. It is also what
/// lets a lightbox tile — which carries a key, a time, and a preview, because reading a month's text for
/// a grid of tiles is not a thing — open the same picture a result card does.
///
/// `Ok(None)` is the honest answer for a row whose screenshot slice was swept and whose video is gone. The
/// front end says so; it must not fall back to stretching the stored preview and calling it the original.
///
/// This one is off the main thread for a reason: reading the frame is either a file or a seek into a
/// segment, and a seek measured 0.5–5.7 s on real footage. A window that freezes for that long reads as
/// broken rather than as working, and the user cannot even press Esc to give up.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameDto {
    pub base64: String,
    /// Which door the picture came through, as a catalog key the front end resolves — `windui_web_*` is
    /// never hand-written into a string that a user reads.
    pub source_key: String,
    /// Which second *of its segment* the picture actually is, when the door had to seek for it.
    ///
    /// A row's caption is a claim about an instant computed from its frame number, and the segment on disk
    /// runs at one frame per second, so the two can sit minutes apart. `None` means no seek was needed —
    /// the picture is the row's own stored file — and the front end then says nothing rather than naming a
    /// second it invented.
    pub shown_offset: Option<i64>,
}

#[tauri::command]
pub async fn frame(state: tauri::State<'_, State>, key: model::RowKey) -> Result<Option<FrameDto>, String> {
    let root = state.root.clone();
    off_main(move || {
        let env = backend::Env::load(&root)?;
        Ok(backend::frame_of_key_with_second(&env, &key)?.map(|(base64, source_key, shown_offset)| FrameDto { base64, source_key: source_key.to_string(), shown_offset }))
    })
    .await
}

/// The month's word cloud. `backend::word_cloud` counts the words and applies the install's own stop
/// list; the front end decides only how big to draw each one. No layout box is returned, because none
/// is computed — a `width`/`height` field here would be a number that is always zero, which is worse
/// than no field.
#[tauri::command]
pub async fn word_cloud(state: tauri::State<'_, State>, year: i64, month: u32) -> Result<Vec<wordcloud::CloudWord>, String> {
    let root = state.root.clone();
    off_main(move || {
        let env = backend::Env::load(&root)?;
        let stop = backend::stop_words(&root, &env.config);
        backend::word_cloud(&env, year, month, &stop, 60)
    })
    .await
}

/// Which screen this process was told to open, if any.
///
/// `--tab` exists for the release gate and for the tray: proving six screens render means opening
/// each one, and a window that can only ever start on Search forces a click no script can make. It is
/// a hint, not a command — an unknown name is the front end's problem to ignore, and the default
/// screen still has to work when nobody passes anything.
#[tauri::command]
pub fn startup_tab(state: tauri::State<'_, State>) -> Option<String> {
    state.tab.clone()
}

/// Whether the recorder is live, answered from the lock file it owns rather than from a process list.
///
/// The header pill says "recording", and the only honest source for that is the same
/// `cache/locks/LOCK_FILE_RECORD.MD` the tray reads: a pid, proved alive. Listing processes would
/// answer a different question — whether *some* `windrec.exe` exists — which is true of another user's
/// session and of a run that died without cleaning up.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderState {
    pub running: bool,
    pub pid: Option<u32>,
}

#[tauri::command]
pub fn recorder_state(state: tauri::State<'_, State>) -> Result<RecorderState, String> {
    let env = state.env()?;
    Ok(match wind_base::fslock::lock_state(&env.config.record_lock_path()) {
        wind_base::fslock::LockState::HeldBy { pid, alive: true } => RecorderState { running: true, pid: Some(pid) },
        // `Free`, and a `HeldBy` whose pid is gone, both mean nobody is recording. `Unreadable` is
        // reported the same way rather than as a confident "no": the lock file exists and carries
        // nothing parseable, which is a fact the pill cannot resolve — but it is not evidence of a
        // running recorder either, and the pill is only allowed to say "recording" when it can prove it.
        _ => RecorderState { running: false, pid: None },
    })
}

/// The words this window draws, resolved by the same catalog the tray and the egui window read.
///
/// A webview is where it would be easiest to hard-code English and call it done — the HTML is new, so
/// nothing tells you the tray already ships `en`/`sc`/`ja` for these very labels. Asking by key keeps
/// `config_src/languages.json` the single place a string lives: a label that goes missing shows up as
/// the catalog's own loud `(key) not found …` marker, exactly as it does in the tray, rather than as
/// one window quietly staying in English.
#[tauri::command]
pub fn ui_strings(state: tauri::State<'_, State>, keys: Vec<String>) -> Result<serde_json::Map<String, Value>, String> {
    let env = state.env()?;
    let lang = env.config.str_or("lang", "en");
    let catalog = wind_base::i18n::Catalog::load(&state.root, &lang);
    let mut out = serde_json::Map::new();
    for key in keys {
        let text = catalog.text(&key);
        out.insert(key, Value::String(text));
    }
    Ok(out)
}

#[tauri::command]
pub fn ui_locale(state: tauri::State<'_, State>) -> Result<String, String> {
    let env = state.env()?;
    Ok(env.config.str_or("lang", "en"))
}

/// Start the deferred pass now, whatever the clock says.
///
/// This writes a request and does not spawn: the recorder owns the pass it launched (it is the only
/// process that knows whether one is already running, and it holds the handle so a second cannot be
/// started against the same index), and the window has no business becoming a second writer of that
/// index. The request is picked up on the recorder's next tick, which is at most one capture interval
/// away — and if a pass is running when the button is pressed, the request waits for it instead of
/// racing it.
#[tauri::command]
pub fn maintenance_start(state: tauri::State<'_, State>) -> Result<String, String> {
    let env = state.env()?;
    wind_base::fslock::write_signal(&env.config.maintain_start_signal_path())?;
    Ok(env.config.maintain_start_signal_path().display().to_string())
}

/// Stop the deferred pass. It finishes the file it has open and stops at the next work item.
///
/// The request is left for `windmaint` to read rather than deleted here: one pass is two processes —
/// this one plus the `wind-reindex` walking the library — and a flag the first of them consumes is
/// invisible to the other. The pass that honours it is the pass that clears it.
#[tauri::command]
pub fn maintenance_stop(state: tauri::State<'_, State>) -> Result<String, String> {
    let env = state.env()?;
    wind_base::fslock::write_signal(&env.config.maintain_stop_signal_path())?;
    Ok(env.config.maintain_stop_signal_path().display().to_string())
}

/// What the deferred pass has said about itself, in the shape this window draws.
///
/// The pass is a child of the recorder, so the window cannot see inside it: this is the file the pass
/// publishes (`cache\locks\LOCK_MAINTAIN\PROGRESS.MD`), read on demand. `known` is false when there is
/// no file or it cannot be understood, which the page shows as "cannot tell" rather than as the more
/// confident "nothing is running".
///
/// The shape a person is given is one total bar and one row per leg — `items_total`/`items_done`/
/// `items_left` and [`Self::legs`] — which is what the four-leg ADR replaced "step 5 of 9" with. The step
/// fields ride along and mean exactly what they meant, for the same reason the writer still publishes
/// them: a new window beside an old `windmaint`, or an old window beside a new one, has to be a progress
/// bar rather than a blank one.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceProgressDto {
    pub known: bool,
    /// The pass says it is running *and* the process table agrees. A `windmaint` that was killed
    /// mid-step leaves the file saying `running` forever, and a spinner for a dead process is worse
    /// than no bar at all.
    pub running: bool,
    pub pid: u32,
    /// `manual` (the button) or `scheduled` (the window or the idle rule).
    pub kind: String,
    /// `running`, `complete`, `stopped`, or `failed` — the pass's own word for how it ended.
    pub state: String,
    /// The nine-step shape, still published and no longer drawn: the command line answers to it, and a
    /// person reading this window reads items.
    pub step: usize,
    pub steps: usize,
    /// The open step's name (`text`, `convert`, …) as the pass wrote it. The page no longer labels a bar
    /// with it — the rows are the legs now — but it is the word `windmaint` itself answers to, so it is
    /// passed through rather than dropped.
    pub step_name: String,
    /// Work items finished in the open step. Zero for a step that does not count them: `reindex` walks
    /// the library in a child process, and this number is not invented for it.
    pub items: usize,
    /// Seconds this pass has been running, answered by the Rust clock rather than the webview's.
    ///
    /// Kept because the page may still want it; it is no longer painted, because the ADR's rule is that
    /// the pass is measured in items and not in anything a second can be turned into (`elapsed_seconds`
    /// is a duration, and a bar is not allowed to be a fraction of it).
    pub elapsed_seconds: i64,
    /// The pass's own closing sentence, when it has one.
    pub note: String,
    /// Items the four denominators counted when the pass started — the total bar's denominator.
    ///
    /// Summed from [`wind_base::maintain::Pass::items_total`] rather than recomputed here: one fact about
    /// the size of a pass living in two files is two numbers that disagree, and the page would be the one
    /// holding the worse one.
    pub items_total: usize,
    /// Items handled so far. May pass [`Self::items_total`] for a pass that did work the census never
    /// saw; the page clamps the bar and prints both numbers as they were said.
    pub items_done: usize,
    /// Items the pass's own denominators still owe — 还差 N, and never negative.
    pub items_left: usize,
    /// The legs, in drawing order, one row each. See [`leg_rows`].
    pub legs: Vec<MaintenanceLegDto>,
}

/// One leg's row: the ADR's 四条腿, one per counter, each in its own unit.
///
/// `name` is the writer's own leg name (`text`, `convert`, `ai`, `other`) and nothing has been renamed
/// here, so the page looks its label up by the same word the file carries and a fifth leg in a newer
/// `windmaint` is a row the page does not draw rather than one it mislabels.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceLegDto {
    pub name: String,
    pub done: usize,
    pub total: usize,
    /// The leg's own state — `waiting`, `running`, `done`, `failed`, `offline` — which is a fact about
    /// one row and never about the pass. `offline` is the endpoint not answering, and the page draws it
    /// as its own muted row; only `failed` is this machine's trouble, and only trouble colours a row.
    pub state: String,
    /// The leg's one-line reason, as the pass wrote it. Shown verbatim: it is the product's sentence
    /// about its own trouble, not this window's copy.
    pub note: String,
}

/// The legs a page may draw, in [`wind_base::maintain::Leg::ALL`] order.
///
/// 某类本轮 0 件 → 那一行整行不显示: a leg counted at nothing has no queue, and an empty bar is a
/// promise nobody made. The one exception is the writer's own rule (`maintain.rs`'s
/// `a_leg_in_trouble_shows_its_row_even_when_nothing_was_counted`) — an empty queue is not permission to
/// hide a leg that said it could not do its work, so a row that carries trouble, a reason, or items past
/// a count of zero is still sent. The total bar is unaffected either way: a leg at zero adds nothing to
/// it, which is why this filter cannot make the two disagree.
fn leg_rows(pass: &wind_base::maintain::Pass) -> Vec<MaintenanceLegDto> {
    pass.legs
        .iter()
        .filter(|count| {
            count.total > 0
                || count.done > 0
                || !count.note.is_empty()
                || count.status.fails_the_pass()
                || count.status == wind_base::maintain::LegStatus::Offline
        })
        .map(|count| MaintenanceLegDto {
            name: count.leg.as_str().to_string(),
            done: count.done,
            total: count.total,
            state: count.status.as_str().to_string(),
            note: count.note.clone(),
        })
        .collect()
}

/// The published pass, or `known: false` when nothing was published to read.
fn maintenance_progress_dto(pass: Option<&wind_base::maintain::Pass>, now: i64) -> MaintenanceProgressDto {
    let Some(pass) = pass else {
        return MaintenanceProgressDto {
            known: false,
            running: false,
            pid: 0,
            kind: String::new(),
            state: String::new(),
            step: 0,
            steps: 0,
            step_name: String::new(),
            items: 0,
            elapsed_seconds: 0,
            note: String::new(),
            items_total: 0,
            items_done: 0,
            items_left: 0,
            legs: Vec::new(),
        };
    };
    MaintenanceProgressDto {
        known: true,
        running: pass.is_running(),
        pid: pass.pid,
        kind: pass.kind.as_str().to_string(),
        state: pass.state.as_str().to_string(),
        step: pass.step,
        steps: pass.steps,
        step_name: pass.step_name.clone(),
        items: pass.items,
        elapsed_seconds: (now - pass.pass_started).max(0),
        note: pass.note.clone(),
        items_total: pass.items_total(),
        items_done: pass.items_done(),
        items_left: pass.items_left(),
        legs: leg_rows(pass),
    }
}

#[tauri::command]
pub fn maintenance_progress(state: tauri::State<'_, State>) -> Result<MaintenanceProgressDto, String> {
    let env = state.env()?;
    let path = env.config.maintain_progress_path();
    Ok(maintenance_progress_dto(
        wind_base::maintain::read(&path).as_ref(),
        wind_base::clock::now().naive_epoch_seconds(),
    ))
}

/// What is waiting to be organised, counted by the steps that would do the work.
///
/// The button that starts the deferred pass is a decision, and a decision needs a size: nine steps and
/// no way to know beforehand whether there is one slice to encode or a thousand rows to read text off.
/// So this asks `windmaint backlog` — the same command a person can run in a terminal, and the one that
/// runs each step's own selector in dry-run — and hands the numbers straight through.
///
/// Spawning rather than re-implementing is the point: a census written twice is a census that disagrees
/// with the pass it is counting for. And the answer is a JSON object on the last line of the child's
/// stdout, not prose to parse, so the window cannot be fooled by a step that changes how it reports.
#[tauri::command]
pub async fn maintenance_backlog(state: tauri::State<'_, State>) -> Result<serde_json::Value, String> {
    let root = state.root.clone();
    off_main(move || backlog_at(&root)).await
}

/// Find `windmaint` the way the tray does, then run the census.
fn backlog_at(root: &Path) -> Result<serde_json::Value, String> {
    let binary = maintenance_binary().ok_or_else(|| {
        "windmaint.exe was not found beside this window or in this install's bin\\ — run `windcap\\build.ps1`".to_string()
    })?;
    let output = std::process::Command::new(&binary)
        .args(["backlog", "--root"])
        .arg(root)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not start {}: {e}", binary.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .ok_or_else(|| {
            let tail = stdout.lines().rev().next().unwrap_or("").to_string();
            format!(
                "{} answered with no census. {}",
                binary.display(),
                if output.status.success() { tail } else { format!("It said: {tail}") }
            )
        })?;
    serde_json::from_str(last).map_err(|e| format!("the census is not readable JSON: {e}"))
}

/// Where `windmaint.exe` is, in the order the tray searches: beside this window, then the install's
/// `bin\`, then a cargo release tree, then a debug one.
fn maintenance_binary() -> Option<PathBuf> {
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
    candidates.push(root.join("windcap").join("target").join("release").join("windmaint.exe"));
    candidates.push(root.join("windcap").join("target").join("debug").join("windmaint.exe"));
    candidates.into_iter().find(|path| path.is_file())
}

// =============================================================================================
// Prompts — the seven templates the product sends, as files the user edits from this window
// =============================================================================================
//
// Everything about a prompt is decided in `wind_base::prompts` (which file wins, what a placeholder
// is, what a template cannot be without) and wrapped for a window in `wind_ui::ai` (which rows a
// screen shows, what a save says when it refuses). This section adds *nothing* to those rules: it
// names them in camelCase and carries the whole seven back with every write, so the page repaints
// from the answer rather than asking again — the same posture `settings_save` takes.
//
// The reason the window is allowed to hold this at all is that a prompt is the one part of the AI
// surface that is the user's voice. Editing it in a file the installer ships and a cleanup pass can
// delete is not editing it.

/// One template as the editor needs it: the words in force, the words this build would send, which
/// of them is answering, and the slots the validator insists on.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRowDto {
    /// The template's name — `period_summary_user`. The same string its file is called, the same one
    /// `prompt_save` and `prompt_restore` take back, and the only handle a screen needs.
    pub name: String,
    /// What the next request would send.
    pub text: String,
    /// What this build ships, so an override that has drifted from a newer default is visible.
    pub shipped: String,
    pub overridden: bool,
    /// `"user"`, `"shipped"` or `"embedded"` — `Origin::label`, the three ways text can be in force.
    /// `"embedded"` is the loud one: this install's `config_src` is not where it says it is.
    pub origin: String,
    /// The file `text` came from, or the one a save would write.
    pub path: String,
    /// An override whose text actually differs from the default it replaced.
    pub changed: bool,
    /// Every `{token}` this template understands.
    pub placeholders: Vec<String>,
    /// The subset without which the request would carry no material.
    pub required: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFormDto {
    pub prompts: Vec<PromptRowDto>,
}

/// What a write did, and what the seven templates are now that it has.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptOutcomeDto {
    pub ok: bool,
    /// The refusal, verbatim from `wind_base::prompts::validate` — the product's own sentence about
    /// its own placeholders, not a widget's paraphrase of one.
    pub error: Option<String>,
    /// The file written, on success.
    pub saved_path: Option<String>,
    /// A sentence for the states `ok` alone cannot tell apart — above all the restore that had
    /// nothing to restore, which must not read like a restore that changed a file.
    pub note: Option<String>,
    /// All seven rows, as they stand *after* the write (or unchanged after a refusal), so the page
    /// never has to reload to show what it just did.
    pub prompts: Vec<PromptRowDto>,
}

/// What "try these words on a real stretch" produced. `wind_ui::ai::PromptTrial` as the front end
/// consumes it: the reply `wind-ai` already redacted, and no key in any field.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTrialDto {
    pub ok: bool,
    /// Which stretch was asked about, so the line cannot be read as a general claim.
    pub segment: String,
    /// Characters of prompt plus screen text that left the machine.
    pub chars: usize,
    pub message: String,
}

/// The template a payload names, or the refusal that lists the seven it may name.
fn prompt_name(label: &str) -> Result<PromptName, String> {
    PromptName::ALL
        .into_iter()
        .find(|name| name.label() == label)
        .ok_or_else(|| format!("'{label}' is not one of this build's prompts: {}", PromptName::ALL.iter().map(|name| name.label()).collect::<Vec<_>>().join(", ")))
}

/// The seven rows, in `Name::ALL`'s order.
///
/// `wind_ui::ai::prompt_rows` is the window-side reader and owns what a row *is* — its text, the
/// shipped copy, whether it drifted from it. `origin` is decided in exactly one other place,
/// `wind_base::prompts::read`, so the two are joined here by name rather than one of them being
/// re-implemented to have a third field. Both lists are `Name::ALL`, so the join cannot miss; the
/// fallback keeps that a wrong label rather than a panic in a settings page, were the two ever to
/// stop being the same set.
fn prompt_rows_dto(config: &Config) -> Vec<PromptRowDto> {
    let origins: BTreeMap<String, String> = wind_base::prompts::read_all(config)
        .into_iter()
        .map(|prompt| (prompt.name.label().to_string(), prompt.origin.label().to_string()))
        .collect();
    wind_ui::ai::prompt_rows(config)
        .into_iter()
        .map(|row| {
            let name = row.name.label().to_string();
            let origin = origins.get(&name).cloned().unwrap_or(if row.overridden { "user" } else { "shipped" }.to_string());
            PromptRowDto {
                placeholders: row.name.placeholders().iter().map(|token| (*token).to_string()).collect(),
                required: row.name.required().iter().map(|token| (*token).to_string()).collect(),
                changed: row.changed,
                overridden: row.overridden,
                text: row.text,
                shipped: row.shipped,
                path: row.path,
                origin,
                name,
            }
        })
        .collect()
}

#[tauri::command]
pub fn prompts_read(state: tauri::State<'_, State>) -> Result<PromptFormDto, String> {
    let env = state.env()?;
    Ok(PromptFormDto { prompts: prompt_rows_dto(&env.config) })
}

/// The write, and the seven rows after it. A refusal is an `Ok` answer with `ok: false`: the page has
/// to be able to show the sentence *and* the state it refused, and an `Err` would throw one away.
fn save_prompt_dto(config: &Config, name: &str, text: &str) -> Result<PromptOutcomeDto, String> {
    let target = prompt_name(name)?;
    // Read back *after* the write, both ways round: a page that repainted from rows gathered before it
    // would show the override it just made as still-not-there.
    match wind_ui::ai::save_prompt(config, target, text) {
        Ok(path) => Ok(PromptOutcomeDto { ok: true, error: None, saved_path: Some(path), note: None, prompts: prompt_rows_dto(config) }),
        // The sentence is the validator's, scrubbed of newlines by the wrapper and of nothing else.
        Err(error) => Ok(PromptOutcomeDto { ok: false, error: Some(error), saved_path: None, note: None, prompts: prompt_rows_dto(config) }),
    }
}

#[tauri::command]
pub fn prompt_save(state: tauri::State<'_, State>, name: String, text: String) -> Result<PromptOutcomeDto, String> {
    let env = state.env()?;
    save_prompt_dto(&env.config, &name, &text)
}

fn restore_prompt_dto(config: &Config, name: &str) -> Result<PromptOutcomeDto, String> {
    let target = prompt_name(name)?;
    // Worded as the egui page words it, because "back to default" and "there was nothing to put back"
    // are two facts and a screen that cannot tell them apart lies about the second.
    let outcome = match wind_ui::ai::restore_prompt(config, target) {
        Ok(true) => Ok(format!("{} is back to the shipped words", target.label())),
        Ok(false) => Ok(format!("{} was never overridden, so nothing changed", target.label())),
        Err(why) => Err(why),
    };
    let prompts = prompt_rows_dto(config);
    match outcome {
        Ok(note) => Ok(PromptOutcomeDto { ok: true, error: None, saved_path: None, note: Some(note), prompts }),
        Err(error) => Ok(PromptOutcomeDto { ok: false, error: Some(error), saved_path: None, note: None, prompts }),
    }
}

#[tauri::command]
pub fn prompt_restore(state: tauri::State<'_, State>, name: String) -> Result<PromptOutcomeDto, String> {
    let env = state.env()?;
    restore_prompt_dto(&env.config, &name)
}

/// One real request built from the text in the box, against the endpoint in the box.
///
/// `text` is the draft, not the file — the thing under test is what the user is writing, and a trial
/// that read the disk would answer a question nobody asked. Nothing here writes: not the prompt, not
/// a summary, and not the key, which `input` may carry unsaved precisely so a person can try a key
/// before deciding to keep it. `staged` is built by `stage_ai`, the same fold `ai_test` runs, so the
/// two buttons cannot disagree about which configuration they are talking to; and every string that
/// comes back has been through `wind-ai`'s redaction on the way out, which is why shipping three of
/// them to a webview is safe and hand-building a fourth would not be.
fn prompt_trial_dto(config: &Config, staged: &wind_ui::ai::AiSettings, name: &str, text: &str) -> Result<PromptTrialDto, String> {
    let target = prompt_name(name)?;
    let trial = wind_ui::ai::try_prompt(config, staged, target, text);
    Ok(PromptTrialDto { ok: trial.ok, segment: trial.segment, chars: trial.chars, message: trial.message })
}

/// The whole trial, from the draft's own text to the line the page paints.
///
/// Folded out of the command so the two guards that keep a request from being spent by accident are
/// testable: `input` goes through `stage_ai` — the same fold `ai_save` and `ai_test` share — and its
/// refusals answer *before* `wind-ai` is reached at all.
fn trial_prompt_at(config: &Config, input: Option<&AiInput>, name: &str, text: &str) -> Result<PromptTrialDto, String> {
    let untouched = AiInput { values: HashMap::new(), clear_key: false };
    let (staged, problems) = stage_ai(config, input.unwrap_or(&untouched))?;
    // "Base URL: must not be empty" is the page's own wording, and it is the answer to a trial asked of
    // an install that has never been pointed at an endpoint. Nothing is sent, so nothing is billed.
    if !problems.is_empty() {
        return Ok(PromptTrialDto { ok: false, segment: String::new(), chars: 0, message: problems.join("\n") });
    }
    prompt_trial_dto(config, &staged, name, text)
}

#[tauri::command]
pub async fn prompt_trial(
    state: tauri::State<'_, State>,
    name: String,
    text: String,
    // `input` is the AI page's draft, so a key typed but not saved is the one that gets used. The
    // front end sends `null`, or leaves the argument out, and the trial runs on what
    // `config_user.json` holds — which is what the next real summary would run on.
    input: Option<AiInput>,
) -> Result<PromptTrialDto, String> {
    let root = state.root.clone();
    off_main(move || {
        let config = Config::load(&root).map_err(|e| e.to_string())?;
        trial_prompt_at(&config, input.as_ref(), &name, &text)
    })
    .await
}

// =============================================================================================
// Summaries — what this machine's AI said about a day, and about one minute of it
// =============================================================================================
//
// Two directories under `userdata/`, one file per product day, written by whichever producer got
// there first: this machine's `windai`, or an outside AI over the bridge. The window has no idea
// which of the two it is reading and must not guess, so every paragraph travels with `writtenBy` and
// a `state` that says whether it still describes what the index holds.
//
// Days are asked for and answered by `wind-summary`'s own arithmetic (`day_of` under the install's
// `day_begin_minutes`), never by a calendar date computed here. That is the rule that keeps a 02:50
// stretch's paragraph out of a file the day it belongs to never reads, and it is the rule the egui
// window, the bridge and `windmaint` already meet.

/// One day's daily paragraph, as the file holds it and as this install can check it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySummaryDto {
    /// There is a file at this day's path. `false` is a different fact from `true, readable: false`,
    /// and both are different from a file that holds nothing.
    pub exists: bool,
    pub readable: bool,
    pub text: String,
    /// Written over an incomplete day, by the writer's own admission.
    pub partial: bool,
    /// The retention pass flagged it, *or* the day's stretches have moved since it was written.
    /// Which of the two, in words, is in [`DaySummariesDto::notes`].
    pub stale: bool,
    /// `windai`, or the label an outside AI sent. `None` means the producer did not say.
    pub written_by: Option<String>,
    pub written_at: Option<String>,
    pub segments_total: usize,
    pub segments_summarised: usize,
    /// The stretches of the day with no standing paragraph, oldest first.
    pub missing: Vec<String>,
    /// Why it is missing or unreadable, in the reader's words.
    pub note: Option<String>,
}

/// One stretch's paragraph.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeriodSummaryDto {
    /// The segment key — its start stamp, the same string the file is keyed by.
    pub segment: String,
    /// Naive-local seconds, exactly as `RowCard::time` carries them. **Do not** hand these to a JS
    /// `Date` to get a clock: it would read them as an instant and land the zone offset away from the
    /// minute they name. `span` is that string already, formatted where it was read.
    pub start: i64,
    pub end: i64,
    /// `15:47:17 → 15:50:07`.
    pub span: String,
    pub frames: usize,
    pub text: String,
    pub text_chars: usize,
    pub written_at: String,
    pub written_by: Option<String>,
    /// Whether this paragraph still stands: `"current"`, `"content_changed"`, `"prompt_changed"`,
    /// `"unindexed"` (the index holds no such stretch, so it cannot be checked), or `"unknown"`
    /// (the index could not be read at all).
    pub state: String,
    /// Which product day's file this came out of. `summary_for_key` looks back one day, because a
    /// stretch that began at 02:50 yesterday runs into the minute being asked about.
    pub day: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaySummariesDto {
    /// The product day asked about, `YYYY-MM-DD`.
    pub date: String,
    pub daily: DailySummaryDto,
    pub periods: Vec<PeriodSummaryDto>,
    /// The newest day a daily paragraph exists for, when it is not the day asked about and the day
    /// asked about has none. How a screen says "this is the latest there is" without passing off
    /// somebody's yesterday as their today.
    pub fallback_date: Option<String>,
    /// Where the three coverage numbers came from: `"stored"` (the day's own paragraph says so),
    /// `"index"` (counted here, now, because no paragraph exists to claim it) or `"unknown"` (the
    /// index could not be read, so the zeroes are not a count).
    pub coverage_from: String,
    /// Every caveat the numbers need: a month file that would not open, a paragraph whose premise
    /// has moved. Coverage printed without these is the lie `wind-summary` documents against.
    pub notes: Vec<String>,
}

/// The prompt digests in force, so "still standing?" is the same comparison the producers' queue
/// makes — built the way the bridge's own summary tools build it.
fn prompt_digests(config: &Config) -> summary::PromptDigests {
    let resolved = wind_base::prompts::Prompts::read(config);
    summary::PromptDigests::of(&resolved.period_system, &resolved.period_user, &resolved.daily_system, &resolved.daily_user)
}

/// One day's work queue, with a failure reported as a note rather than as a zero.
fn queue_for(config: &Config, digests: &summary::PromptDigests, day: &str, notes: &mut Vec<String>) -> Option<summary::DayQueue> {
    match summary::for_day_with(&summary::Reader::new(config), day, digests) {
        Ok(queue) => {
            if !queue.skipped.is_empty() {
                notes.push(format!(
                    "{} of this day's month files could not be opened, so its coverage is a floor rather than a count: {}",
                    queue.skipped.len(),
                    queue.skipped.join(", ")
                ));
            }
            Some(queue)
        }
        Err(why) => {
            notes.push(format!("this day's stretches could not be counted: {why}"));
            None
        }
    }
}

/// Which state each stored paragraph is in, keyed by segment.
fn entry_states(queue: &summary::DayQueue) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for segment in &queue.current {
        out.insert(segment.key.clone(), "current".to_string());
    }
    for item in &queue.pending {
        out.insert(item.segment.key.clone(), item.reason.label().to_string());
    }
    out
}

/// `15:47:17 → 15:50:07`, formatted on this side of the boundary for the reason `StripCell::clock`
/// is: naive-local seconds handed to a JS `Date` come back the zone offset away from the minute.
fn span_label(from: i64, to: i64) -> String {
    let clock = |instant: i64| {
        let parts = LocalParts::from_naive_epoch(instant);
        format!("{:02}:{:02}:{:02}", parts.hour, parts.minute, parts.second)
    };
    format!("{} → {}", clock(from), clock(to))
}

fn period_dto(day: &str, written: &summary::PeriodSummary, key: &str, states: &BTreeMap<String, String>, counted: bool) -> PeriodSummaryDto {
    PeriodSummaryDto {
        segment: key.to_string(),
        start: written.start,
        end: written.end,
        span: span_label(written.start, written.end),
        frames: written.frames,
        text_chars: written.text.chars().count(),
        written_at: written.written_at.clone(),
        // An empty producer field is "not said", which is not the same claim as a name.
        written_by: (!written.written_by.is_empty()).then(|| written.written_by.clone()),
        text: written.text.clone(),
        state: states.get(key).cloned().unwrap_or_else(|| if counted { "unindexed" } else { "unknown" }.to_string()),
        day: day.to_string(),
    }
}

fn day_summaries_at(config: &Config, year: i32, month: u32, day: u32) -> Result<DaySummariesDto, String> {
    let stamp = format!("{year:04}-{month:02}-{day:02}");
    let Some((year, month, day)) = summary::keys::parse_day(&stamp) else {
        return Err(format!("`{stamp}` is not a day; expected YYYY-MM-DD"));
    };
    let shift = config.day_begin_minutes();
    // Noon, and that is load-bearing: `day_of` answers which product day *owns an instant*, and a date
    // a screen named is the day that begins on it. Midnight would answer yesterday at the shipped
    // 03:00 rollover, and any hour would do it for an install that rolls the day late.
    let date = summary::day_of(LocalParts { year, month, day, hour: 12, minute: 0, second: 0 }.naive_epoch_seconds(), shift);
    let digests = prompt_digests(config);
    let mut notes: Vec<String> = Vec::new();
    let day_map = summary::read_period(config, &date);
    let daily_file = summary::read_daily(config, &date);
    let queue = queue_for(config, &digests, &date, &mut notes);
    // A period file that exists and cannot be parsed is reported; one that is simply not there yet is
    // not a caveat, it is the state `exists: false` already says.
    if day_map.exists && !day_map.note.is_empty() {
        notes.push(day_map.note.clone());
    }
    let counted = queue.is_some();
    let states = queue.as_ref().map(entry_states).unwrap_or_default();

    // The day's own paragraph claims coverage numbers; when there is none, the only honest source left
    // is the index, and when neither can answer the zeroes say so through `coverage_from`.
    let written = daily_file.summary.clone();
    let (segments_total, segments_summarised, missing, coverage_from): (usize, usize, Vec<String>, &'static str) = match (&written, &queue) {
        (Some(stored), _) => (stored.coverage.segments_total, stored.coverage.segments_summarised, stored.coverage.missing.clone(), "stored"),
        (None, Some(live)) => (live.segments_total, live.summarised, live.coverage.missing.clone(), "index"),
        (None, None) => (0, 0, Vec::new(), "unknown"),
    };
    let reasons = queue.as_ref().and_then(|live| match &live.daily {
        summary::DailyState::Stale(_, reasons) => Some(reasons.iter().map(|reason| reason.label()).collect::<Vec<_>>().join(", ")),
        _ => None,
    });
    if let Some(reasons) = &reasons {
        notes.push(format!("this day's paragraph no longer stands: {reasons}"));
    }
    if written.as_ref().is_some_and(|stored| stored.stale) {
        notes.push("the retention pass marked this day's paragraph stale: a stretch it was written from has left the library".to_string());
    }
    let periods = day_map
        .entries
        .iter()
        .map(|(key, entry)| period_dto(&date, entry, key, &states, counted))
        .collect();
    Ok(DaySummariesDto {
        daily: DailySummaryDto {
            exists: daily_file.exists,
            readable: daily_file.readable,
            stale: written.as_ref().is_some_and(|stored| stored.stale) || reasons.is_some(),
            partial: written.as_ref().is_some_and(|stored| stored.partial),
            text: written.as_ref().map(|stored| stored.text.clone()).unwrap_or_default(),
            written_by: written.as_ref().map(|stored| stored.written_by.clone()).filter(|held| !held.is_empty()),
            written_at: written.as_ref().map(|stored| stored.written_at.clone()),
            segments_total,
            segments_summarised,
            missing,
            note: (!daily_file.note.is_empty()).then(|| daily_file.note.clone()),
        },
        // A day with no paragraph of its own is reported as that, and the newest day that does have
        // one is named beside it. Never a silently-empty list, never somebody else's text.
        fallback_date: (!daily_file.exists)
            .then(|| summary::days_present(config, summary::Kind::Daily).last().cloned())
            .flatten()
            .filter(|newest| newest != &date),
        notes,
        coverage_from: coverage_from.to_string(),
        date,
        periods,
    })
}

#[tauri::command]
pub async fn day_summaries(state: tauri::State<'_, State>, year: i32, month: u32, day: u32) -> Result<DaySummariesDto, String> {
    let root = state.root.clone();
    off_main(move || day_summaries_at(&Config::load(&root).map_err(|e| e.to_string())?, year, month, day)).await
}

/// What the AI said about one row's moment: every stored paragraph whose `[start, end]` window
/// contains it, oldest first.
///
/// Free of `tauri` so the containment rule and the day arithmetic around it are testable; the command
/// below is this call and nothing more.
fn summaries_for_row(env: &backend::Env, rowid: i64, table_key: &str, time: i64) -> Vec<PeriodSummaryDto> {
    let config = &env.config;
    let shift = config.day_begin_minutes();
    let digests = prompt_digests(config);
    let day = summary::day_of(time, shift);
    // A stretch may have begun in the day before this one and run into it, and its paragraph is
    // filed under the day its *start* belongs to: two day files, and no more.
    let mut days: Vec<String> = vec![day.clone()];
    if let Some(span) = summary::keys::day_span(&day, shift) {
        let before = summary::day_of(span.from - 1, shift);
        if before != day {
            days.push(before);
        }
    }
    // The row's own segment, from the index rather than from anything the window named. A paragraph
    // whose stored window has since been re-indexed around the moment still describes this row, and
    // dropping it because two numbers disagree is how the rail goes blank on a row the summariser
    // wrote about. A row the index no longer holds is not an error here: the paragraphs on disk do
    // not depend on it, so containment alone is the answer.
    let wanted = backend::card_of_key(env, &model::RowKey::new(table_key, rowid))
        .ok()
        .and_then(|card| summary::canonical_key(&card.segment));
    if let Some(key) = &wanted {
        if let Some(begins) = LocalParts::from_stamp(key) {
            let owner = summary::day_of(begins.naive_epoch_seconds(), shift);
            if !days.contains(&owner) {
                days.push(owner);
            }
        }
    }
    let mut out: Vec<PeriodSummaryDto> = Vec::new();
    for day in &days {
        let map = summary::read_period(config, day);
        let hits: Vec<(&String, &summary::PeriodSummary)> = map
            .entries
            .iter()
            .filter(|(key, entry)| (entry.start <= time && time <= entry.end) || wanted.as_deref() == Some(key.as_str()))
            .collect();
        if hits.is_empty() {
            continue;
        }
        // The queue is read only for a day that actually answered, so a row in an empty afternoon
        // costs two file opens rather than two index walks.
        let mut notes = Vec::new();
        let queue = queue_for(config, &digests, day, &mut notes);
        let states = queue.as_ref().map(entry_states).unwrap_or_default();
        out.extend(hits.iter().map(|(key, entry)| period_dto(day, entry, key, &states, queue.is_some())));
    }
    out.sort_by_key(|dto| (dto.start, dto.segment.clone()));
    out
}

#[tauri::command]
pub async fn summary_for_key(state: tauri::State<'_, State>, rowid: i64, table_key: String, time: i64) -> Result<Vec<PeriodSummaryDto>, String> {
    let root = state.root.clone();
    off_main(move || Ok(summaries_for_row(&backend::Env::load(&root)?, rowid, &table_key, time))).await
}

// =============================================================================================
// Tests
// =============================================================================================
//
// These drive the free functions the commands wrap, for the same reason `off_main` exists: a
// `tauri::State` cannot be built in a unit test, and the rules worth pinning — which file wins, what a
// refusal says, which day a paragraph is filed under — do not live in the plumbing. Each test gets its
// own throwaway install under the OS temp directory, because every one of them writes into the two
// `userdata/` folders the product itself uses.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wind_base::prompts::{self, Name};

    /// A throwaway install that *ships* the seven real templates, so an override is an override of
    /// something and a restore has something to fall back to.
    fn install(tag: &str) -> PathBuf {
        let root = summary::test_support::install(tag);
        let shipped = root.join("config_src").join(prompts::DIR);
        fs::create_dir_all(&shipped).expect("shipped prompt folder");
        for name in Name::ALL {
            fs::write(name.path_in(&root.join("config_src")), name.embedded()).expect("ship a prompt");
        }
        root
    }

    fn config_at(root: &Path) -> Config {
        Config::load(root).expect("fixture config loads")
    }

    fn row<'a>(rows: &'a [PromptRowDto], name: &str) -> &'a PromptRowDto {
        rows.iter().find(|row| row.name == name).unwrap_or_else(|| panic!("{} is not in {:?}", name, rows.iter().map(|r| &r.name).collect::<Vec<_>>()))
    }

    /// One day's paragraph, written by hand rather than through the writers: this is a test about what
    /// a reader reports about files some other producer put on disk.
    fn write_daily(config: &Config, day: &str, body: Value) {
        let dir = summary::dir(config, summary::Kind::Daily);
        fs::create_dir_all(&dir).expect("daily dir");
        fs::write(dir.join(format!("{day}.json")), body.to_string()).expect("daily file");
    }

    fn write_periods(config: &Config, day: &str, body: Value) {
        let dir = summary::dir(config, summary::Kind::Period);
        fs::create_dir_all(&dir).expect("period dir");
        fs::write(dir.join(format!("{day}.json")), body.to_string()).expect("period file");
    }

    /// Every object key anywhere in a serialized value, arrays walked too.
    fn collect_keys(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    out.push(key.clone());
                    collect_keys(child, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|child| collect_keys(child, out)),
            _ => {}
        }
    }

    #[test]
    fn a_prompt_save_and_a_restore_report_who_is_answering_at_every_step() {
        let root = install("prompt-round-trip");
        let config = config_at(&root);
        let rows = prompt_rows_dto(&config);
        assert_eq!(rows.len(), Name::ALL.len(), "seven templates, one row each");
        assert!(rows.iter().all(|row| row.origin == "shipped"), "a fresh install answers from config_src");
        assert!(rows.iter().all(|row| !row.overridden && !row.changed));
        assert!(row(&rows, "period_summary_user").required.contains(&"{frames_table}".to_string()));

        let mine = "Say what they were doing. {frames_table}\n";
        let saved = save_prompt_dto(&config, "period_summary_user", mine).expect("the name resolves");
        assert!(saved.ok, "{:?}", saved.error);
        assert!(saved.error.is_none());
        let path = saved.saved_path.clone().expect("a save names its file");
        assert!(path.ends_with("period_summary_user.txt"), "{path}");
        assert_eq!(path, prompts::override_path(&config, Name::PeriodUser).display().to_string());
        // The outcome carries the seven rows as they now stand, so the page repaints from the answer.
        assert_eq!(saved.prompts.len(), Name::ALL.len());
        let overridden = row(&saved.prompts, "period_summary_user");
        assert_eq!(overridden.origin, "user");
        assert_eq!(overridden.text, mine);
        assert!(overridden.overridden && overridden.changed);
        assert_eq!(overridden.shipped, Name::PeriodUser.embedded(), "the default stays visible beside it");
        assert_eq!(row(&prompt_rows_dto(&config), "period_summary_user").origin, "user", "and prompts_read agrees");

        let restored = restore_prompt_dto(&config, "period_summary_user").expect("the name resolves");
        assert!(restored.ok);
        let note = restored.note.clone().expect("a restore that changed a file says so");
        assert!(note.contains("shipped words"), "{note}");
        assert_eq!(row(&restored.prompts, "period_summary_user").origin, "shipped");
        assert!(!prompts::override_path(&config, Name::PeriodUser).exists(), "restore deletes the override rather than overwriting it");

        // The same press a second time changed nothing, and must not read as though it did.
        let again = restore_prompt_dto(&config, "period_summary_user").expect("the name resolves");
        assert!(again.ok, "nothing to restore is not a failure");
        assert_ne!(again.note, restored.note, "and the sentence is a different one");
        assert!(again.note.clone().expect("a did-nothing restore says so").contains("nothing changed"));
        assert!(again.saved_path.is_none());
        assert_eq!(row(&again.prompts, "period_summary_user").origin, "shipped");
        assert!(prompt_name("period_summary_system").is_ok());
        assert!(prompt_name("my_custom_prompt").is_err(), "a name that is not one of the seven is refused, listing those that are");
        summary::test_support::cleanup(&root);
    }

    /// The refusals the validator knows, phrased by it and not by this window.
    #[test]
    fn a_refused_prompt_save_answers_with_the_validators_own_sentence_and_writes_nothing() {
        let root = install("prompt-refused");
        let config = config_at(&root);
        for (name, text) in [("period_summary_user", "text {frame_table}"), ("period_summary_user", "Summarise my day please."), ("daily_summary_user", "no list here"), ("tags_user", "   ")] {
            let target = prompt_name(name).expect("a real name");
            let outcome = save_prompt_dto(&config, name, text).expect("the name resolves");
            assert!(!outcome.ok, "{name} should have been refused");
            assert_eq!(outcome.saved_path, None);
            let error = outcome.error.expect("a refusal carries a sentence");
            assert_eq!(error, prompts::validate(target, text).expect_err("the same rule, stated once"), "{name}: not the validator's verbatim words");
            assert!(!error.contains('\n'), "one line, because the widget paints it inline: {error}");
            assert!(!prompts::override_path(&config, target).exists(), "and it never reached the disk");
            // Rejected text leaves the seven rows exactly as they were, so the page has something to
            // show without asking again.
            assert_eq!(outcome.prompts.len(), Name::ALL.len());
            assert_eq!(row(&outcome.prompts, name).origin, "shipped");
        }
        let near_miss = save_prompt_dto(&config, "period_summary_user", "text {frame_table}").expect("resolves");
        assert!(near_miss.error.expect("refused").contains("Did you mean `{frames_table}`"), "the product's own suggestion, not a paraphrase");
        assert!(!prompts::override_path(&config, Name::PeriodUser).exists());
        assert!(!prompts::override_dir(&config).exists(), "not even the folder");
        summary::test_support::cleanup(&root);
    }

    /// The trial path is the one command in this file that can reach a socket, so both of the doors in
    /// front of it are pinned here — mirroring the `#[ignore]`-gated stdout checks in `wind-ui`: a
    /// configuration that cannot send is answered locally, and the key appears in nothing that comes
    /// back.
    /// A published pass reaches the page as the step it is in, and the page's `running` is the
    /// process table's answer rather than the file's.
    #[test]
    fn the_progress_a_pass_published_is_the_progress_the_page_shows() {
        use wind_base::maintain::{Kind, Pass, State};

        // Nothing on disk: the page is allowed to say it cannot tell, which is not the same claim as
        // "no pass is running" — the file is the only witness, and it is missing.
        let empty = maintenance_progress_dto(None, 1_790_600_500);
        assert!(!empty.known && !empty.running, "an absent file is reported as absent");
        assert_eq!(empty.steps, 0, "and it invents no nine-step shape");

        // A pass in its sixth step, counted, still alive because this test process is its owner.
        let open = Pass {
            pid: std::process::id(),
            kind: Kind::Manual,
            pass_started: 1_790_600_000,
            legs: Vec::new(),
            step: 6,
            steps: 9,
            step_name: "previews".to_string(),
            step_started: 1_790_600_300,
            items: 412,
            state: State::Running,
            note: String::new(),
            finished: None,
        };
        let shown = maintenance_progress_dto(Some(&open), 1_790_600_500);
        assert!(shown.running, "the pid is alive, so the bar moves");
        assert_eq!((shown.step, shown.steps, shown.step_name.as_str(), shown.items), (6, 9, "previews", 412));
        assert_eq!(shown.elapsed_seconds, 500, "answered by the Rust clock, not the webview's");
        assert_eq!(shown.kind, "manual", "the button, not the window");

        // The crash the file cannot report: `running` in its own words, gone from the process table.
        let crashed = Pass { pid: u32::MAX, ..open.clone() };
        let after = maintenance_progress_dto(Some(&crashed), 1_790_600_500);
        assert!(after.known, "the file is still readable");
        assert!(!after.running, "a dead pid is not a pass in progress, whatever the file says");

        // An ending stays readable until the next pass replaces it, with its own sentence attached.
        let ended = Pass {
            state: State::Stopped,
            note: "stopped after 5 of 9 steps: the maintenance window 03:30-05:00 has closed".to_string(),
            finished: Some(1_790_600_480),
            ..open
        };
        let done = maintenance_progress_dto(Some(&ended), 1_790_600_500);
        assert_eq!(done.state, "stopped");
        assert!(!done.running, "a pass that ended is not running");
        assert!(done.note.contains("has closed"), "{}", done.note);
    }

    /// The four counters reach the page as four rows, and the total bar is their sum.
    ///
    /// The shape the ADR asks for (`docs/adr/2026-09-30-the-organise-pass-runs-on-four-legs.md` §一): one
    /// total line and one line per leg, each in its own unit, each with its own state — and none of them
    /// the pass's state. `items` keeps meaning "work finished in the open step", which is what the page
    /// shipped today already reads, so a new binary beside an old bundle is still a bar and not a blank.
    #[test]
    fn the_four_legs_reach_the_page_as_four_rows_under_one_total_bar() {
        use wind_base::maintain::{Kind, Leg, LegCount, LegStatus, Pass, State};

        let open = Pass {
            pid: std::process::id(),
            kind: Kind::Manual,
            pass_started: 1_790_600_000,
            step: 1,
            steps: 9,
            step_name: "text".to_string(),
            step_started: 1_790_600_001,
            items: 3_700,
            state: State::Running,
            note: String::new(),
            finished: None,
            legs: vec![
                LegCount { leg: Leg::Text, done: 3_700, total: 4_200, status: LegStatus::Running, note: String::new() },
                LegCount { leg: Leg::Convert, done: 28, total: 68, status: LegStatus::Done, note: String::new() },
                LegCount { leg: Leg::Ai, done: 3, total: 9, status: LegStatus::Offline, note: "the endpoint did not answer".to_string() },
                LegCount { leg: Leg::Other, done: 0, total: 23, status: LegStatus::Waiting, note: String::new() },
            ],
        };
        let shown = maintenance_progress_dto(Some(&open), 1_790_600_500);

        assert_eq!(
            shown.legs.iter().map(|leg| (leg.name.as_str(), leg.done, leg.total, leg.state.as_str())).collect::<Vec<_>>(),
            [("text", 3_700, 4_200, "running"), ("convert", 28, 68, "done"), ("ai", 3, 9, "offline"), ("other", 0, 23, "waiting")],
            "four rows, in the writer's own order, with the writer's own words",
        );
        assert_eq!(shown.legs[2].note, "the endpoint did not answer", "a leg's reason rides with its row, not with the pass");
        // The total bar is the sum of the rows the pass counted — 4200 + 68 + 9 + 23 — and not a fifth
        // number this file could invent.
        assert_eq!((shown.items_total, shown.items_done, shown.items_left), (4_300, 3_731, 569));
        // And the old fields still answer the old questions.
        assert_eq!((shown.step, shown.steps, shown.step_name.as_str(), shown.items), (1, 9, "text", 3_700));
        assert_eq!(shown.state, "running", "a quiet endpoint is one yellow row and not a failed pass");

        // The camelCase names are the contract with `src/types.ts`, so they are pinned here rather than
        // trusted to a rename attribute nobody reads.
        let json = serde_json::to_value(&shown).expect("the DTO serialises");
        assert_eq!(json["itemsTotal"], 4_300);
        assert_eq!(json["itemsDone"], 3_731);
        assert_eq!(json["itemsLeft"], 569);
        assert_eq!(json["legs"][2]["name"], "ai");
        assert_eq!(json["legs"][2]["state"], "offline");
        assert_eq!(json["legs"][2]["note"], "the endpoint did not answer");
        assert_eq!(json["stepName"], "text", "the fields the shipped page already reads are still there");
        assert_eq!(json["elapsedSeconds"], 500);
    }

    /// 某类本轮 0 件，那一行整行不显示 — with the writer's own exception, which is that trouble is not a
    /// queue: a leg that reported it could not work gets its row whatever it was counted at.
    #[test]
    fn a_leg_counted_at_nothing_is_no_row_until_it_has_something_to_say() {
        use wind_base::maintain::{Kind, Leg, LegCount, LegStatus, Pass, State};

        fn counted(legs: Vec<LegCount>) -> Pass {
            Pass {
                pid: std::process::id(),
                kind: Kind::Scheduled,
                pass_started: 1_790_600_000,
                step: 3,
                steps: 9,
                step_name: "expire".to_string(),
                step_started: 1_790_600_001,
                items: 0,
                state: State::Running,
                note: String::new(),
                finished: None,
                legs,
            }
        }
        fn rows(legs: Vec<LegCount>) -> Vec<MaintenanceLegDto> {
            maintenance_progress_dto(Some(&counted(legs)), 1_790_600_100).legs
        }

        // Silent and counted at nothing: nothing to draw, and the total bar does not change either way.
        let quiet = vec![
            LegCount { leg: Leg::Text, done: 4, total: 4, status: LegStatus::Done, note: String::new() },
            LegCount { leg: Leg::Ai, done: 0, total: 0, status: LegStatus::Waiting, note: String::new() },
        ];
        let shown = rows(quiet.clone());
        let names = shown.iter().map(|leg| leg.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["text"], "the empty AI row is not sent");
        assert_eq!(maintenance_progress_dto(Some(&counted(quiet)), 1_790_600_100).items_total, 4);

        // The same leg, now with trouble: the row is sent, because 本轮 0 件 never meant "hide a failure".
        let failed = vec![LegCount { leg: Leg::Other, done: 0, total: 0, status: LegStatus::Failed, note: "2026-08: no such table records".to_string() }];
        let shown = rows(failed);
        assert_eq!(shown.iter().map(|leg| leg.name.as_str()).collect::<Vec<_>>(), ["other"]);
        assert_eq!((shown[0].state.as_str(), shown[0].note.as_str()), ("failed", "2026-08: no such table records"));

        // An endpoint that said nothing is a row too — and it is the yellow kind, so the page must see it
        // as its own state rather than as the pass failing.
        let offline = vec![LegCount { leg: Leg::Ai, done: 0, total: 0, status: LegStatus::Offline, note: "the endpoint did not answer".to_string() }];
        let shown = rows(offline);
        assert_eq!(shown.len(), 1, "a quiet endpoint is worth a row even with no queue behind it");
        assert_eq!(shown[0].state, "offline");
        // Items past a denominator the census never counted still get their row, and the bar that reads
        // full is the honest answer rather than a receding one.
        let overrun = vec![LegCount { leg: Leg::Text, done: 130, total: 100, status: LegStatus::Done, note: String::new() }];
        let shown = maintenance_progress_dto(Some(&counted(overrun)), 1_790_600_100);
        assert_eq!((shown.items_total, shown.items_done, shown.items_left), (100, 130, 0), "full, owing nothing it promised");
    }

    /// The door the pass writes and the door the page reads are one path, derived once.
    #[test]
    fn the_pass_and_the_page_agree_on_where_progress_lives() {
        let root = install("progress-path");
        let config = config_at(&root);
        let path = config.maintain_progress_path();
        assert_eq!(path.parent(), Some(config.maintain_lock_dir().as_path()), "inside the lock the pass owns");

        wind_base::maintain::install(&path, wind_base::maintain::Kind::Scheduled, 1_790_600_000);
        wind_base::maintain::begin_step("convert", 2, 9, 1_790_600_010);
        let shown = maintenance_progress_dto(wind_base::maintain::read(&path).as_ref(), 1_790_600_020);
        assert!(shown.running, "this process installed the publisher, and it is alive");
        assert_eq!((shown.step, shown.steps, shown.step_name.as_str()), (2, 9, "convert"), "what the pipeline said is what the page sees");
        wind_base::maintain::uninstall();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_trial_sends_nothing_and_names_no_key_when_the_endpoint_is_not_usable() {
        const SECRET: &str = "sk-winduiweb-trial-key-0123456789abcdef";
        let root = install("prompt-trial");
        // Material to try it on: without a stretch of screen text the call would stop at "nothing to
        // try it on" and never reach the client, which is a weaker claim than the one made here.
        summary::test_support::seed_morning(&root, "default", 1);
        let config = config_at(&root);
        let draft = Name::PeriodUser.embedded();

        // Nothing typed at all: the draft's own refusals answer before `wind-ai` is reached.
        let unconfigured = trial_prompt_at(&config, None, "period_summary_user", draft).expect("a real name");
        assert!(!unconfigured.ok);
        assert_eq!(unconfigured.chars, 0, "nothing left the machine");
        assert!(unconfigured.message.contains("Base URL"), "the page's own wording, not a socket error three layers away: {}", unconfigured.message);

        // An address and a key in the box, but the address names no scheme, so the transport refuses it
        // before a hostname is looked up. The key was held, and never named.
        let mut values: HashMap<String, String> = HashMap::new();
        values.insert(wind_ui::ai::AField::ApiKey.key().to_string(), SECRET.to_string());
        values.insert(wind_ui::ai::AField::BaseUrl.key().to_string(), "api.somewhere.test/v1".to_string());
        values.insert(wind_ui::ai::AField::Model.key().to_string(), "a-model".to_string());
        let typed = AiInput { values, clear_key: false };
        let trial = trial_prompt_at(&config, Some(&typed), "period_summary_user", draft).expect("a real name");
        assert!(!trial.ok, "an address that cannot become a request does not answer: {}", trial.message);
        assert_eq!(trial.chars, 0, "and it sent nothing to be counted");
        assert!(trial.message.contains("not a valid URL"), "the transport's own sentence, not a policy invented here: {}", trial.message);
        assert!(!trial.message.contains(SECRET), "the refusal leaked the key: {}", trial.message);
        assert!(!trial.segment.contains(SECRET), "the label leaked the key: {}", trial.segment);
        assert!(!format!("{} | {}", trial.message, trial.segment).contains("sk-winduiweb"), "no fragment of it came back");
        // A trial writes nothing: not the prompt under test, not a summary.
        assert!(!prompts::override_path(&config, Name::PeriodUser).exists());
        assert!(summary::days_present(&config, summary::Kind::Period).is_empty(), "a trial files no paragraph");
        summary::test_support::cleanup(&root);
    }

    #[test]
    fn the_day_dto_reports_the_paragraphs_who_wrote_them_and_the_gaps() {
        let root = install("day-summaries");
        let config = config_at(&root);
        let start = summary::test_support::at("2026-09-27_09-00-00");
        write_daily(
            &config,
            "2026-09-27",
            serde_json::json!({
                "date": "2026-09-27",
                "text": "Spent the morning between a spreadsheet and a chat.",
                "coverage": { "segments_total": 2, "segments_summarised": 1, "missing": ["2026-09-27_09-05-00"] },
                "partial": true,
                "written_at": summary::test_support::STAMP,
                "written_by": "windai",
                "model": "a-model",
                "source_fingerprint": "aaaa",
                "stale": false,
                "prompt_fingerprint": ""
            }),
        );
        write_periods(
            &config,
            "2026-09-27",
            serde_json::json!({
                "2026-09-27_09-00-00": {
                    "text": "第一段。",
                    "start": start,
                    "end": start + 180,
                    "frames": 4,
                    "ocr_chars": 100,
                    "written_at": summary::test_support::STAMP,
                    "written_by": "an outside AI",
                    "model": "",
                    "source_fingerprint": "aaaa",
                    "prompt_fingerprint": ""
                }
            }),
        );

        let dto = day_summaries_at(&config, 2026, 9, 27).expect("a real day");
        assert_eq!(dto.date, "2026-09-27");
        assert!(dto.daily.exists && dto.daily.readable);
        assert_eq!(dto.daily.written_by.as_deref(), Some("windai"), "the internal producer, named");
        assert_eq!(dto.daily.text, "Spent the morning between a spreadsheet and a chat.");
        assert_eq!(dto.daily.written_at.as_deref(), Some(summary::test_support::STAMP));
        assert!(dto.daily.partial, "the file's own admission, carried through");
        assert_eq!((dto.daily.segments_total, dto.daily.segments_summarised), (2, 1));
        assert_eq!(dto.daily.missing, vec!["2026-09-27_09-05-00".to_string()], "the gap it was written over");
        assert_eq!(dto.coverage_from, "stored", "the paragraph's own claim, not a recount");
        assert_eq!(dto.fallback_date, None, "the day asked about answered for itself");
        // Nothing was marked stale by the retention pass, but this install holds no stretches at all,
        // so the paragraph's premise has moved — reported, because "there is a paragraph" is not the
        // same question as "it still stands".
        assert!(dto.daily.stale, "{:?}", dto.notes);
        assert!(dto.notes.iter().any(|note| note.contains("no longer stands")), "{:?}", dto.notes);

        assert_eq!(dto.periods.len(), 1);
        let period = &dto.periods[0];
        assert_eq!(period.segment, "2026-09-27_09-00-00");
        assert_eq!(period.text, "第一段。");
        assert_eq!(period.text_chars, 4, "characters, not bytes");
        assert_eq!(period.written_by.as_deref(), Some("an outside AI"), "the external producer, told apart from the internal one");
        assert_eq!((period.start, period.end), (start, start + 180));
        assert_eq!(period.span, "09:00:00 → 09:03:00", "formatted here, where the stored axis is known");
        assert_eq!(period.day, "2026-09-27");
        assert_eq!(period.state, "unindexed", "the index holds no such stretch, so nothing vouches for it");
        summary::test_support::cleanup(&root);
    }

    #[test]
    fn a_day_with_no_paragraph_says_so_and_labels_the_day_that_has_one() {
        let root = install("day-absent");
        let config = config_at(&root);
        let start = summary::test_support::at("2026-09-26_14-00-00");
        write_daily(
            &config,
            "2026-09-26",
            serde_json::json!({
                "date": "2026-09-26", "text": "yesterday", "coverage": { "segments_total": 0, "segments_summarised": 0, "missing": [] },
                "partial": false, "written_at": summary::test_support::STAMP, "written_by": "windai", "model": "",
                "source_fingerprint": "aaaa", "stale": false, "prompt_fingerprint": ""
            }),
        );
        write_periods(
            &config,
            "2026-09-26",
            serde_json::json!({ "2026-09-26_14-00-00": { "text": "x", "start": start, "end": start + 60, "frames": 1, "ocr_chars": 1, "written_at": summary::test_support::STAMP, "written_by": "", "model": "", "source_fingerprint": "a", "prompt_fingerprint": "" } }),
        );

        let asked = day_summaries_at(&config, 2026, 9, 27).expect("a real day");
        assert_eq!(asked.date, "2026-09-27", "the day asked about, whatever answered");
        assert!(!asked.daily.exists, "and it is reported as having nothing");
        assert!(!asked.daily.readable);
        assert!(asked.daily.text.is_empty(), "no text is invented for a day that has none");
        assert_eq!(asked.daily.written_by, None);
        assert!(asked.periods.is_empty());
        assert!(asked.notes.iter().all(|note| !note.contains("could not be counted")), "an empty day is not an unreadable index: {:?}", asked.notes);
        let fallback = asked.fallback_date.expect("the newest day that does have one is named");
        assert_eq!(fallback, "2026-09-26");
        assert_ne!(fallback, asked.date, "labelled as a different day, so a screen cannot pass it off as today's");
        // Coverage from the index, because there is no paragraph to quote.
        assert_eq!(asked.coverage_from, "index");
        assert_eq!((asked.daily.segments_total, asked.daily.segments_summarised), (0, 0));

        let held = day_summaries_at(&config, 2026, 9, 26).expect("the day that has one");
        assert!(held.daily.exists && held.fallback_date.is_none(), "asked directly, it answers for itself");
        assert_eq!(held.periods.len(), 1);
        assert_eq!(held.periods[0].written_by, None, "an empty producer field is 'not said', not a name");
        assert!(day_summaries_at(&config, 2026, 2, 30).is_err(), "a day the calendar does not have is refused");
        summary::test_support::cleanup(&root);
    }

    /// What the AI said about one row's minute, found through the index's own row lookup.
    #[test]
    fn a_rows_moment_finds_the_paragraph_whose_window_contains_it() {
        let root = install("summary-for-key");
        summary::test_support::seed_morning(&root, "default", 2);
        let config = config_at(&root);
        let segments = summary::Reader::new(&config).of_day("2026-09-27").expect("the fixture index reads").segments;
        assert_eq!(segments.len(), 2, "two stretches, 09:00 and 09:05");
        let digests = prompt_digests(&config);
        let first = &segments[0];
        // A paragraph that stands: it names the stretch's real digest and the prompt in force.
        let (key, fingerprint, prompt) = (first.key.clone(), first.fingerprint.clone(), digests.period.clone());
        write_periods(
            &config,
            &first.day,
            serde_json::json!({ key: {
                "text": "Reconciling the Q3 numbers.",
                "start": first.start,
                "end": first.start + 240,
                "frames": first.frames,
                "ocr_chars": first.ocr_chars,
                "written_at": summary::test_support::STAMP,
                "written_by": "windai",
                "model": "",
                "source_fingerprint": fingerprint,
                "prompt_fingerprint": prompt
            } }),
        );
        let env = backend::Env::load(&root).expect("the fixture is an install");
        let month_file = "default_2026-09_wind.db";
        let hits = summaries_for_row(&env, 1, month_file, first.start + 60);
        assert_eq!(hits.len(), 1, "{:?}", hits.iter().map(|hit| &hit.segment).collect::<Vec<_>>());
        assert_eq!(hits[0].segment, first.key);
        assert_eq!(hits[0].state, "current", "the index still says this paragraph describes its stretch");
        assert_eq!(hits[0].written_by.as_deref(), Some("windai"));
        // A minute in the second stretch, past the first one's window, is an honest empty answer.
        assert!(summaries_for_row(&env, 2, month_file, segments[1].start).is_empty(), "nothing was written about a stretch that has no paragraph");
        // A row the index no longer holds is not an error: containment still answers.
        assert_eq!(summaries_for_row(&env, 9_999, "gone_2026-09_wind.db", first.start + 60).len(), 1);
        summary::test_support::cleanup(&root);
    }

    /// Asserted, not assumed: this project's wire contract is camelCase on both sides, and a
    /// snake_case slip is invisible until a screen shows nothing.
    #[test]
    fn every_field_this_window_ships_reaches_the_front_end_in_camel_case() {
        let root = install("camel-case");
        let config = config_at(&root);
        let mut keys: Vec<String> = Vec::new();
        collect_keys(&serde_json::to_value(day_summaries_at(&config, 2026, 9, 27).expect("a day")).expect("serialises"), &mut keys);
        collect_keys(&serde_json::to_value(PromptFormDto { prompts: prompt_rows_dto(&config) }).expect("serialises"), &mut keys);
        collect_keys(&serde_json::to_value(save_prompt_dto(&config, "tags_user", Name::TagsUser.embedded()).expect("a real name")).expect("serialises"), &mut keys);
        collect_keys(&serde_json::to_value(PromptTrialDto { ok: true, segment: String::new(), chars: 0, message: String::new() }).expect("serialises"), &mut keys);
        // Built by hand rather than read off a disk: this test is about the spelling of a name, and an
        // empty `periods` array would hide the whole struct's keys away.
        collect_keys(
            &serde_json::to_value(vec![PeriodSummaryDto {
                segment: String::new(),
                start: 0,
                end: 0,
                span: String::new(),
                frames: 0,
                text: String::new(),
                text_chars: 0,
                written_at: String::new(),
                written_by: None,
                state: String::new(),
                day: String::new(),
            }])
            .expect("serialises"),
            &mut keys,
        );
        assert!(keys.contains(&"segmentsSummarised".to_string()), "{keys:?}");
        for expected in ["segmentsTotal", "writtenBy", "writtenAt", "fallbackDate", "coverageFrom", "textChars", "savedPath", "placeholders", "overridden"] {
            assert!(keys.iter().any(|key| key == expected), "{expected} never reached the wire: {keys:?}");
        }
        let snake: Vec<&String> = keys.iter().filter(|key| key.contains('_')).collect();
        assert!(snake.is_empty(), "snake_case leaked onto the wire: {snake:?}");
        summary::test_support::cleanup(&root);
    }
}
