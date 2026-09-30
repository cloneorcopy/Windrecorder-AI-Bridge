//! Upstream's fifth tab — the "Lab" page at `windrecorder/ui/lab.py` — as a sixth screen here, and
//! therefore the only place in the whole product where `windai`'s endpoint, key and model can be set.
//!
//! # Why this is a sixth tab and not a section of Settings
//!
//! `settings.rs` opens with "The typed view of the config keys that actually change what *these two
//! screens* show", `Field` says "The fifteen keys Search / OneDay / Settings consume", and the page's
//! own heading is "Settings **these two screens read**" with the subtitle "Fifteen keys, typed and
//! bounded. Everything else in the config belongs to the recorder and is written back untouched."
//! None of that is true of an API key: Search and OneDay never read `open_ai_base_url`, and the thing
//! that does is a separate process the user runs from a terminal.
//!
//! The crate has met this exact problem before and answered it once. `Recording` is its own tab
//! because its keys "are read by another process the moment they land", and `record.rs` states the
//! rule: "The distinction is why the two forms are separate types with separate Save buttons rather
//! than one long list: a mistake in the first costs the user a page of results, a mistake in the
//! second costs them the next session's footage." A mistake here costs a pay-per-token call to a
//! third party carrying the user's window titles — a third and heavier cost class, not a ninth row
//! on the page whose whole contract is "the two screens' keys". So: `Tab::Ai`, its own struct, its
//! own draft, its own Save.
//!
//! # What is *not* on this page, and why
//!
//! Fifteen fields, on four counts. Five are the MCP bridge's — `enable_mcp_server` and the four
//! values it binds with — because a resident service that can be switched on only by hand-editing a
//! JSON file is a feature with no door. Seven are upstream's Lab page: it wrote nine keys, and two are
//! not here, because a control nothing downstream honours is the defect this branch has now removed
//! eight times over — a widget that persists a value, changes nothing, and leaves the user believing
//! they configured something. The last three — `enable_ai_extract_tag`,
//! `enable_ai_extract_tag_in_idle` and `enable_ai_summary_in_idle` — are read by `windmaint`'s idle
//! pass before it spends requests on a month or on screen text; they are switches rather than a file to
//! edit because the alternative is a timer that talks to a third party on a machine the user believes is
//! asleep:
//!
//!   * **`enable_img_embed_search`** — the image-embedding-search toggle, gated upstream on
//!     `img_embed_module_install`. Neither key has a reader anywhere in this workspace: not `windai`,
//!     not `windmcp`, not the search path in `wind-store`. There is no native image-embedding index
//!     to switch on. So it is not a field and is never staged, and a value an earlier install carries
//!     rides `Config::save`'s merged map straight back out unchanged.
//!     [`AiSettings::image_search_promised`] exists so the page can *say* that out loud to the one
//!     user whose config asks for it.
//!   * **`enable_ai_day_poem`** — already refused in `main.rs`'s "not ported, and why" list ("The
//!     setting is initialised and never read, the widgets are rendered `disabled=True`").
//!
//! Three further keys `wind_ai::settings::Settings` reads were, until this page grew, not fields on it.
//! Two of them — `enable_ai_extract_tag` and `enable_ai_extract_tag_in_idle` — are *not* inert,
//! whatever this comment used to claim: `windmaint`'s `schedule::ai_gate` reads both before it spends a
//! request on a month, so they decide whether idle tagging runs at all. They are rows now precisely
//! because a pass that exists must not be gated by two switches nobody can see without opening the file,
//! and because the page counts itself in its own prose ("fifteen keys") — adding a row means changing
//! that number everywhere it is quoted, which is the cost of a row and not a reason to refuse one. Both
//! are read through the same two `Config` accessors `ai_gate` asks, so
//! a key that appears in neither file means the same *false* and the same *true* in all three.
//! `ai_extract_tag_in_idle_batch_size` is the third key and it stays refused: it reaches `windai doctor`'s
//! line and no request, and this page has no row for it. A person editing it today has to use the file,
//! and `Config::save` carries its value through every Save from here.
//!
//! `exclude_words` is *counted* here, because `windai` reads it and the user needs to know it exists,
//! but it is not edited: `Field::ExcludeWords` on the Settings page already owns that key, and two
//! pages staging one key is how a Save from one silently reverts the other.
//!
//! # The one rule this file exists to enforce
//!
//! **The API key is never displayed, and no text that reaches the screen is printed unredacted.**
//! The field is a masked `TextEdit` whose box starts *empty* rather than prefilled, so the page
//! cannot leak even the key's length; what it shows instead is a state and
//! `SecretKey::fingerprint()`, which `wind-ai` documents as a loggable identifier with no security
//! claim. Every string that can reach the frame from outside this crate — a transport failure, an
//! HTTP body the endpoint chose to write, a model's own answer — is built by or passed through
//! `wind_ai::error`, which removes the key at construction time and in both percent-encoding hex
//! cases. `Debug` for [`AiSettings`] and [`AiDraft`] is hand-written for the same reason: `AppState`
//! derives `Debug`, and `{:?}` on it is the shape any future `eprintln!` will take.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Instant;

use serde_json::Value;
use wind_ai::client::{ChatRequest, Client, Transport, WinHttp};
use wind_ai::error::{redact, Faults};
use wind_ai::settings::{SecretKey, Settings as AiRead, KEY_PLACEHOLDER, OPENAI_COMPATIBLE};
use wind_base::config::Config;
// The five bridge keys on this page are the bridge's, not this window's: `startup_guard` is the one
// sentence a service that will not bind prints, and `Runtime` is the one reading of the address it
// binds. A second rule written here is how the page starts accepting configurations whose bridge
// never comes up.
use wind_mcp::{
    auth,
    runtime::{self, Runtime},
};

/// The ceiling on a one-line echo of the model's answer. `windai doctor` clips the same echo to 72
/// characters for the same reason: the point of the line is "it spoke", not "here is all it said".
const REPLY_CLIP: usize = 72;

/// The AI keys this page writes, as the form holds them.
#[derive(Clone, PartialEq, Eq)]
pub struct AiSettings {
    /// `ai_api_endpoint_selected`. One of [`AiSettings::endpoint_types`], and `windai` refuses to run
    /// unless it is also the one dialect this build speaks.
    pub endpoint_selected: String,
    /// `ai_api_endpoint_type` — the menu the install ships, not a user choice. Carried so the
    /// endpoint box can offer exactly what `require_usable` will accept, and never staged.
    pub endpoint_types: Vec<String>,
    pub base_url: String,
    pub model: String,
    /// `open_ai_api_key`. The only `String` in this crate allowed to hold a token, and only because
    /// writing one back to `config_user.json` needs the bytes. `Debug` is hand-written below so that
    /// `{:?}` on this struct — and therefore on `AppState`, which derives it — cannot print them;
    /// [`AiSettings::key`] is `pub(crate)` for the same reason `SecretKey::expose` is `pub(crate)` to
    /// its own crate.
    api_key: String,
    /// `ai_extract_tag_wintitle_limit`.
    pub wintitle_limit: i64,
    /// `ai_extract_max_tag_num`.
    pub max_tag_num: i64,
    /// `ai_extract_tag_filter_words`.
    pub filter_words: Vec<String>,
    /// `enable_ai_extract_tag` — the master switch for the tagger.
    ///
    /// Read through [`Config::ai_extract_tag_enabled`] rather than a `bool_or` of this page's own,
    /// because that one accessor is also what `windai` refuses to spend on
    /// (`wind_ai::settings::Settings::read`) and what `windmaint`'s `schedule::ai_gate` declines on. An
    /// absent key has to mean the same *false* to all three, or the page shows a switch the pass is not
    /// obeying.
    pub tag_enabled: bool,
    /// `enable_ai_extract_tag_in_idle` — and whether that pass may run on a machine the user believes is
    /// asleep. Same single reader as above: [`Config::ai_extract_tag_allowed_in_idle`].
    pub tag_in_idle: bool,
    /// `enable_mcp_server`. The bridge is a resident HTTP service on this machine; the switch is the
    /// only thing that turns it on, and until this row existed nothing in the product wrote it.
    pub mcp_enabled: bool,
    /// `mcp_server_host`. Loopback by default, which is the only address that keeps a stranger off a
    /// service that can read every screen the recorder saw.
    pub mcp_host: String,
    /// `mcp_server_port`.
    pub mcp_port: i64,
    /// `mcp_server_auth_required`. Off means the port answers anyone on a reachable interface, and the
    /// verdict line says that in those words rather than hiding behind "auth disabled".
    pub mcp_auth_required: bool,
    /// `mcp_server_token`. A secret for the same reason `api_key` is one: it is a bearer credential,
    /// and it is only ever *written* from this page, never displayed.
    mcp_token: String,
    /// How many `exclude_words` the install holds. Read for the line that points at the *other* page,
    /// never staged.
    pub exclude_words: usize,
    /// `enable_ai_summary_in_idle`. The one switch for the pass that sends captured screen text to
    /// the endpoint while the machine is idle. There is deliberately no second master key: the shipped
    /// `open_ai_api_key` is a placeholder, and `windai` refuses to send a byte while it is one, so a
    /// machine that has never been given an endpoint cannot start talking to one by itself.
    pub summary_in_idle: bool,
    /// Whether the user's config asks for an image-embedding search this product cannot provide.
    ///
    /// Carried, not editable, for the reason the module header gives. Both keys are read as the
    /// conjunction upstream used for its own checkbox — the toggle only meant anything once the
    /// module was installed — so a stock install shows no notice and only the users who genuinely
    /// installed it are told it does nothing here.
    pub image_search_promised: bool,
}

/// The shipped `config_default.json` values, doubling as the answers for a config that predates a
/// key — the same rule `settings.rs` and `record.rs` follow. The literals are not a second source of
/// truth that can drift unnoticed: `windcap/ai/src/settings.rs`'s
/// `every_key_name_matches_the_shipped_default_file` and
/// `the_shipped_config_reads_back_its_own_defaults` assert every one of these values against the real
/// file, and `tests::the_default_is_what_the_shipped_file_says` here repeats that against
/// `wind_ai::settings::Settings::read` so a rename on the config side fails in *both* crates.
impl Default for AiSettings {
    fn default() -> AiSettings {
        AiSettings {
            endpoint_selected: OPENAI_COMPATIBLE.to_string(),
            endpoint_types: vec![OPENAI_COMPATIBLE.to_string()],
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            api_key: KEY_PLACEHOLDER.to_string(),
            wintitle_limit: 30,
            max_tag_num: 15,
            filter_words: vec!["Kim Jong-un".into()],
            // The two tagger switches, at what `Config::ai_extract_tag_enabled` and
            // `Config::ai_extract_tag_allowed_in_idle` answer for a file that never mentions them — which
            // is the same answer `windai`'s reader gives, so a stock install shows a tagger that is off
            // but allowed, and nothing on screen disagrees with the pass.
            tag_enabled: false,
            tag_in_idle: true,
            // The shipped `config_default.json` values, which is what `load` falls back to as well:
            // the bridge is off until a person says otherwise, on loopback, on upstream's own port,
            // with the token required and no token set.
            mcp_enabled: false,
            mcp_host: "127.0.0.1".into(),
            mcp_port: 21120,
            mcp_auth_required: true,
            mcp_token: String::new(),
            summary_in_idle: true,
            // Neither a default install nor a default `AppState` has asked for anything. Only a file
            // can make either promise, which is why `load` is where they are read.
            exclude_words: 0,
            image_search_promised: false,
        }
    }
}

impl AiSettings {
    /// Read the AI keys through **`windai`'s own reader**.
    ///
    /// This is the load-bearing reuse. `AiRead::read` is the function `windai doctor` prints from and
    /// `Client::ask` refuses with, so every default, every clamp (a negative limit becomes zero) and
    /// every placeholder rule on this page is the consumer's rather than this page's invention.
    pub fn load(config: &Config) -> AiSettings {
        let read = AiRead::read(config);
        AiSettings {
            endpoint_selected: read.endpoint_selected,
            endpoint_types: read.endpoint_types,
            base_url: read.base_url,
            model: read.model,
            // `SecretKey::expose` is `pub(crate)` to `wind-ai`, so this page cannot read the token
            // back out of the reader it just used — which is the boundary working as designed. The
            // form's own copy comes from the config, the same place `windai` gets it from.
            api_key: config.str_or("open_ai_api_key", ""),
            wintitle_limit: read.wintitle_limit as i64,
            max_tag_num: read.max_tag_num as i64,
            filter_words: read.filter_words,
            // The two tagger switches through the accessors, not through `read`'s fields and not through
            // a `bool_or` here: [`Config::ai_extract_tag_enabled`] is the answer `windmaint`'s `ai_gate`
            // asks, so the page and the pass cannot disagree about what an absent key means. That they
            // still land in the same place `windai` reads is what
            // `every_key_lands_where_windai_reads_it` proves from the other side.
            tag_enabled: config.ai_extract_tag_enabled(),
            tag_in_idle: config.ai_extract_tag_allowed_in_idle(),
            mcp_enabled: config.bool_or("enable_mcp_server", false),
            mcp_host: config.str_or("mcp_server_host", "127.0.0.1"),
            mcp_port: config.i64_or("mcp_server_port", 21120),
            mcp_auth_required: config.bool_or("mcp_server_auth_required", true),
            mcp_token: config.str_or("mcp_server_token", ""),
            summary_in_idle: config.ai_summary_allowed_in_idle(),
            exclude_words: read.exclude_words.len(),
            image_search_promised: config.bool_or("enable_img_embed_search", false)
                && config.bool_or("img_embed_module_install", false),
        }
    }

    pub fn get(&self, field: AField) -> ATyped {
        match field {
            AField::EndpointType => ATyped::Text(self.endpoint_selected.clone()),
            AField::BaseUrl => ATyped::Text(self.base_url.clone()),
            AField::Model => ATyped::Text(self.model.clone()),
            // Deliberately not the value: the box starts empty and an empty box means "unchanged".
            AField::ApiKey => ATyped::Text(String::new()),
            AField::TitleLimit => ATyped::Int(self.wintitle_limit),
            AField::MaxTags => ATyped::Int(self.max_tag_num),
            AField::FilterWords => ATyped::Lines(self.filter_words.clone()),
            AField::TagEnabled => ATyped::Bool(self.tag_enabled),
            AField::TagInIdle => ATyped::Bool(self.tag_in_idle),
            AField::McpEnabled => ATyped::Bool(self.mcp_enabled),
            AField::McpHost => ATyped::Text(self.mcp_host.clone()),
            AField::McpPort => ATyped::Int(self.mcp_port),
            AField::McpAuth => ATyped::Bool(self.mcp_auth_required),
            AField::SummaryInIdle => ATyped::Bool(self.summary_in_idle),
            // Like the API key: the box starts empty, and empty means "leave what is stored alone".
            AField::McpToken => ATyped::Text(String::new()),
        }
    }

    fn set(&mut self, field: AField, value: ATyped) {
        match (field, value) {
            (AField::EndpointType, ATyped::Text(v)) => self.endpoint_selected = v,
            (AField::BaseUrl, ATyped::Text(v)) => self.base_url = v,
            (AField::Model, ATyped::Text(v)) => self.model = v,
            (AField::ApiKey, ATyped::Text(v)) => self.api_key = v,
            (AField::TitleLimit, ATyped::Int(v)) => self.wintitle_limit = v,
            (AField::MaxTags, ATyped::Int(v)) => self.max_tag_num = v,
            (AField::FilterWords, ATyped::Lines(v)) => self.filter_words = v,
            (AField::TagEnabled, ATyped::Bool(v)) => self.tag_enabled = v,
            (AField::TagInIdle, ATyped::Bool(v)) => self.tag_in_idle = v,
            (AField::McpEnabled, ATyped::Bool(v)) => self.mcp_enabled = v,
            (AField::McpHost, ATyped::Text(v)) => self.mcp_host = v,
            (AField::McpPort, ATyped::Int(v)) => self.mcp_port = v,
            (AField::McpAuth, ATyped::Bool(v)) => self.mcp_auth_required = v,
            (AField::SummaryInIdle, ATyped::Bool(v)) => self.summary_in_idle = v,
            (AField::McpToken, ATyped::Text(v)) => self.mcp_token = v,
            // A field/value mismatch is a programming error in `AField::ALL`, not user input.
            _ => unreachable!("{} cannot hold that type", field.key()),
        }
    }

    /// Stage these fifteen keys into the merged config. `Config::save` is what hits the disk, and it
    /// writes the whole map, so the fifteen keys `settings.rs` owns, the twenty-five `record.rs` owns,
    /// and every AI key this page refuses — `ai_extract_tag_in_idle_batch_size`, `enable_img_embed_search`,
    /// `enable_ai_day_poem`, `ai_api_endpoint_type`, `exclude_words` — ride along exactly as they were read.
    ///
    /// The two tagger switches are in the staged list rather than the refused one. They used to be
    /// refused on the theory that nothing read them; `windmaint`'s `schedule::ai_gate` did, and a pass
    /// whose two gating switches could only be edited in the file by hand is the dead control this branch
    /// exists to remove. `ai_extract_tag_in_idle_batch_size` stays refused: it reaches `windai doctor`'s
    /// line and no request, and this page has no row for it.
    ///
    /// The list is explicit for the reason `Rec::stage`'s is: it is the only place to state which
    /// keys this page claims, and
    /// `tests::stage_writes_exactly_the_keys_this_page_owns` is the guard on it.
    pub fn stage(&self, config: &mut Config) {
        config.set("ai_api_endpoint_selected", Value::String(self.endpoint_selected.clone()));
        config.set("open_ai_base_url", Value::String(self.base_url.clone()));
        config.set("open_ai_modelname", Value::String(self.model.clone()));
        config.set("open_ai_api_key", Value::String(self.api_key.clone()));
        config.set("ai_extract_tag_wintitle_limit", Value::from(self.wintitle_limit));
        config.set("ai_extract_max_tag_num", Value::from(self.max_tag_num));
        config.set(
            "ai_extract_tag_filter_words",
            Value::Array(self.filter_words.iter().cloned().map(Value::String).collect()),
        );
        // The two switches, staged through the same accessors `ai_gate` and `windai` read them with, and
        // as real JSON booleans — `bool_or` takes a `Value::Bool` straight and coerces only the three
        // exact strings "true"/"1"/"yes".
        config.set("enable_ai_extract_tag", Value::Bool(self.tag_enabled));
        config.set("enable_ai_extract_tag_in_idle", Value::Bool(self.tag_in_idle));
        config.set("enable_mcp_server", Value::Bool(self.mcp_enabled));
        config.set("mcp_server_host", Value::String(self.mcp_host.clone()));
        config.set("mcp_server_port", Value::from(self.mcp_port));
        config.set("mcp_server_auth_required", Value::Bool(self.mcp_auth_required));
        config.set("mcp_server_token", Value::String(self.mcp_token.clone()));
        config.set("enable_ai_summary_in_idle", Value::Bool(self.summary_in_idle));
    }

    /// The bearer token, for the three things allowed to use it: [`stage`][AiSettings::stage],
    /// [`verdict`] and [`probe_with`].
    ///
    /// `pub` rather than `pub(crate)` because the crate split moved its callers: `render_tests.rs`
    /// asserts on the painted AI page and lives in the binary, so it now reaches this through the
    /// library target. That widens nothing that was secret — the value is in
    /// `userdata/config_user.json`, which this process reads anyway, and the boundary that keeps it
    /// off screen is `wind_ai::error::redact` on the display path, not the visibility of a getter.
    pub fn key(&self) -> &str {
        &self.api_key
    }
}

/// Two ways to build a variant of the form, for the tests elsewhere in this crate that have to put a
/// recognisable secret into the page they are about to prove does not display it, and point a probe at
/// a loopback listener they started.
///
/// Gated on `debug_assertions` rather than `test` for the same reason [`key`] had to widen: since the
/// data half of this window became a library target, `render_tests.rs` is an *outside* consumer, and a
/// `cfg(test)` impl is not even compiled into the library the binary links against. The rule that
/// matters survives intact — a release build, which is all any user ever gets, carries no setter
/// whose only caller is a test.
#[cfg(debug_assertions)]
impl AiSettings {
    pub fn with_key(&self, key: &str) -> AiSettings {
        let mut out = self.clone();
        out.api_key = key.to_string();
        out
    }

    pub fn at_url(&self, base_url: &str) -> AiSettings {
        let mut out = self.clone();
        out.base_url = base_url.to_string();
        out
    }
}

impl fmt::Debug for AiSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiSettings")
            .field("endpoint_selected", &self.endpoint_selected)
            .field("endpoint_types", &self.endpoint_types)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            // `SecretKey`'s own `Debug` is `SecretKey([REDACTED], n bytes)`, which is the shape
            // `wind-ai` chose for this exact moment. The byte count is not printed raw anywhere: the
            // placeholder-vs-real question is answered by [`key_state`], not by a length.
            .field("api_key", &SecretKey::new(self.api_key.clone()))
            .field("wintitle_limit", &self.wintitle_limit)
            .field("max_tag_num", &self.max_tag_num)
            .field("filter_words", &self.filter_words)
            .field("tag_enabled", &self.tag_enabled)
            .field("tag_in_idle", &self.tag_in_idle)
            .field("mcp_enabled", &self.mcp_enabled)
            .field("mcp_host", &self.mcp_host)
            .field("mcp_port", &self.mcp_port)
            .field("mcp_auth_required", &self.mcp_auth_required)
            // Redacted for the same reason as `api_key`: `{:?}` on `AppState` must never be able to
            // print a bearer token, and this one guards the whole index.
            .field("mcp_token", &SecretKey::new(self.mcp_token.clone()))
            .field("exclude_words", &self.exclude_words)
            .field("image_search_promised", &self.image_search_promised)
            .finish()
    }
}

/// Which of the three states a key can be in, as far as the screen is allowed to describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    /// The file holds no key at all.
    Absent,
    /// The file holds the literal string the installer wrote.
    Placeholder,
    /// A key the user typed, identified by a short hash and nothing else.
    Set(String),
}

impl KeyState {
    /// The line beside the key box. It answers "did my save take?" without answering "what is my
    /// key?", which is the entire design constraint in one sentence.
    pub fn describe(&self) -> String {
        match self {
            KeyState::Absent => "no key is stored; type one to set it".to_string(),
            KeyState::Placeholder => {
                format!("still holds the installer placeholder `{KEY_PLACEHOLDER}`; type a key to replace it")
            }
            KeyState::Set(fingerprint) => format!(
                "a key is stored, fingerprint {fingerprint}; the box is blank so that it stays hidden"
            ),
        }
    }

    /// The same line, in the language the window is painted in.
    ///
    /// `describe` stays as the words the binary carries: the tests read it, and a status pill whose
    /// catalog row went missing must fall back to a sentence rather than to a `(key) not found` marker in
    /// the one place the user is checking whether their save took.
    pub fn describe_in(&self, catalog: &wind_base::i18n::Catalog) -> String {
        match self {
            KeyState::Absent => catalog.text_or("ai_key_absent", &self.describe()),
            KeyState::Placeholder => catalog.text_or("ai_key_placeholder", &self.describe()),
            KeyState::Set(fingerprint) => {
                catalog.formatted_or("ai_key_set", &[("fingerprint", fingerprint)], &self.describe())
            }
        }
    }

    pub fn is_unusable(&self) -> bool {
        matches!(self, KeyState::Absent | KeyState::Placeholder)
    }
}

/// `windai`'s own three-way answer about the key this page is holding —
/// `SecretKey::is_empty`, then `Settings::key_configured`, then a fingerprint — reached through
/// [`read_back`] so the placeholder rule is not restated here in any form.
pub fn key_state(config: &Config, staged: &AiSettings) -> KeyState {
    let read = read_back(config, staged);
    if read.api_key.is_empty() {
        KeyState::Absent
    } else if !read.key_configured() {
        KeyState::Placeholder
    } else {
        KeyState::Set(read.api_key.fingerprint())
    }
}

/// Which AI key a widget edits. An enum rather than a `&'static str` so the compiler's
/// exhaustiveness check is what notices an eighth one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AField {
    EndpointType,
    BaseUrl,
    Model,
    ApiKey,
    TitleLimit,
    MaxTags,
    FilterWords,
    /// `enable_ai_extract_tag` — the master switch for the tagger. Carried as a row because `windai`
    /// refuses to spend on it and `windmaint`'s idle pass declines on it, and a pass that exists must not
    /// be gated by a switch nobody can see.
    TagEnabled,
    /// `enable_ai_extract_tag_in_idle` — and whether that pass may run unattended.
    TagInIdle,
    /// The MCP bridge's five keys, as one group on this page.
    McpEnabled,
    McpHost,
    McpPort,
    McpAuth,
    McpToken,
    /// The summariser's idle switch. Lives here rather than on the Settings page because the Settings
    /// page's stated contract is the keys Search and OneDay read, and this key is read by `windmaint`.
    SummaryInIdle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AKind {
    Int { min: i64, max: i64 },
    Text { max_chars: usize },
    /// A token. Drawn masked, seeded blank, and — uniquely — an empty box means "leave the stored
    /// value where it is" rather than "reject", because there is no way to show what is there to
    /// edit.
    Secret { max_chars: usize },
    Lines { max_entries: usize },
    /// One of a list the *install* ships (`ai_api_endpoint_type`), so the options arrive per call.
    Choice(Vec<String>),
    /// A switch. Drafted as the text `true`/`false`, exactly like the other two forms do it, so one
    /// stringly-typed draft type still describes every widget on the tab.
    Bool,
}

impl AField {
    pub const ALL: [AField; 15] = [
        AField::EndpointType,
        AField::BaseUrl,
        AField::Model,
        AField::ApiKey,
        AField::TitleLimit,
        AField::MaxTags,
        AField::FilterWords,
        AField::TagEnabled,
        AField::TagInIdle,
        AField::McpEnabled,
        AField::McpHost,
        AField::McpPort,
        AField::McpAuth,
        AField::McpToken,
        AField::SummaryInIdle,
    ];

    pub fn group(self) -> &'static str {
        match self {
            AField::EndpointType | AField::BaseUrl | AField::Model | AField::ApiKey => "Endpoint",
            AField::TitleLimit
            | AField::MaxTags
            | AField::FilterWords
            | AField::TagEnabled
            | AField::TagInIdle => "Monthly activity tags",
            AField::McpEnabled | AField::McpHost | AField::McpPort | AField::McpAuth | AField::McpToken => "MCP bridge",
            AField::SummaryInIdle => "Summaries",
        }
    }

    /// The catalog key this section heading is translated under, so a Chinese install does not get one
    /// Chinese page and one English heading on it.
    pub fn group_key(self) -> &'static str {
        match self.group() {
            "Endpoint" => "ai_group_endpoint",
            "Monthly activity tags" => "ai_group_tags",
            "Summaries" => "ai_group_summaries",
            _ => "ai_group_mcp",
        }
    }

    /// The catalog key this field's label is translated under. `ai_*` is upstream's own prefix for
    /// this page, and the MCP rows are new because upstream's settings page had no MCP section at all
    /// — the bridge was configured by hand-editing the file, which is the thing being fixed.
    pub fn label_key(self) -> &'static str {
        match self {
            AField::EndpointType => "ai_selectbox_endpoint_type",
            AField::BaseUrl => "ai_text_base_url",
            AField::Model => "ai_text_modelname",
            AField::ApiKey => "ai_text_api_key",
            AField::TitleLimit => "ai_input_title_limit",
            AField::MaxTags => "ai_input_max_tag_num",
            AField::FilterWords => "ai_input_filter_words",
            AField::TagEnabled => "ai_checkbox_extract_tag",
            AField::TagInIdle => "ai_checkbox_extract_tag_in_idle",
            AField::McpEnabled => "ai_checkbox_mcp_enabled",
            AField::McpHost => "ai_text_mcp_host",
            AField::McpPort => "ai_input_mcp_port",
            AField::McpAuth => "ai_checkbox_mcp_auth",
            AField::McpToken => "ai_text_mcp_token",
            AField::SummaryInIdle => "ai_checkbox_summary_in_idle",
        }
    }

    /// The catalog key this field's explanation is translated under.
    pub fn help_key(self) -> &'static str {
        match self {
            AField::EndpointType => "ai_help_endpoint_type",
            AField::BaseUrl => "ai_help_base_url",
            AField::Model => "ai_help_modelname",
            AField::ApiKey => "ai_help_api_key",
            AField::TitleLimit => "ai_help_title_limit",
            AField::MaxTags => "ai_help_max_tag_num",
            AField::FilterWords => "ai_help_filter_words",
            AField::TagEnabled => "ai_help_extract_tag",
            AField::TagInIdle => "ai_help_extract_tag_in_idle",
            AField::McpEnabled => "ai_help_mcp_enabled",
            AField::McpHost => "ai_help_mcp_host",
            AField::McpPort => "ai_help_mcp_port",
            AField::McpAuth => "ai_help_mcp_auth",
            AField::McpToken => "ai_help_mcp_token",
            AField::SummaryInIdle => "ai_help_summary_in_idle",
        }
    }

    /// The config key. These fifteen strings are a wire format shared with `windai`, the MCP bridge
    /// and any Python install still reading the file, so they are checked against
    /// `wind_ai::settings::Settings::read` by
    /// [`tests::every_key_lands_where_windai_reads_it`] rather than against a second list that nobody
    /// would notice drifting.
    pub fn key(self) -> &'static str {
        match self {
            AField::EndpointType => "ai_api_endpoint_selected",
            AField::BaseUrl => "open_ai_base_url",
            AField::Model => "open_ai_modelname",
            AField::ApiKey => "open_ai_api_key",
            AField::TitleLimit => "ai_extract_tag_wintitle_limit",
            AField::MaxTags => "ai_extract_max_tag_num",
            AField::FilterWords => "ai_extract_tag_filter_words",
            // The two tagger switches, spelled as `Config::ai_extract_tag_enabled` and
            // `Config::ai_extract_tag_allowed_in_idle` read them, and as
            // `wind_ai::settings::Settings::read` reads them.
            AField::TagEnabled => "enable_ai_extract_tag",
            AField::TagInIdle => "enable_ai_extract_tag_in_idle",
            AField::McpEnabled => "enable_mcp_server",
            AField::McpHost => "mcp_server_host",
            AField::McpPort => "mcp_server_port",
            AField::McpAuth => "mcp_server_auth_required",
            AField::McpToken => "mcp_server_token",
            AField::SummaryInIdle => "enable_ai_summary_in_idle",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AField::EndpointType => "Endpoint dialect",
            AField::BaseUrl => "Base URL",
            AField::Model => "Model name",
            AField::ApiKey => "API key",
            AField::TitleLimit => "Window titles per day",
            AField::MaxTags => "Tags kept per day",
            AField::FilterWords => "AI filter words (one per line)",
            AField::TagEnabled => "Extract activity tags with AI",
            AField::TagInIdle => "Let the tagger run during the idle pass",
            AField::McpEnabled => "Expose this library to AI tools over MCP",
            AField::McpHost => "Listen on address",
            AField::McpPort => "Port",
            AField::McpAuth => "Require the bearer token",
            AField::McpToken => "Access token",
            AField::SummaryInIdle => "Summarise on screen text while idle",
        }
    }

    /// The effect, in the consumer's own words — and, where a bound below is not upstream's, which of
    /// the two it is. `windai` is the reader named here because it is the only one.
    pub fn help(self) -> &'static str {
        match self {
            AField::EndpointType => {
                "`ai_api_endpoint_selected`. The choices are `ai_api_endpoint_type`, which the \
                 install ships and this page never rewrites. `windai`'s own `require_usable` refuses \
                 anything outside that list and then refuses anything that is not `OpenAI \
                 compatible`, so a dialect added to the install's file by hand is reported by the \
                 verdict line rather than quietly sent to an endpoint that cannot answer it."
            }
            AField::BaseUrl => {
                "`open_ai_base_url`. `windai` posts to `{base_url}/chat/completions`, inserting the \
                 path before any query string and collapsing a trailing slash, so both \
                 `https://host/v1` and `https://host/v1/` are correct. `http://` is sent exactly as \
                 written, because a gateway on your own network is normally spelled \
                 `http://192.168.x.x:3000/v1`; on such an address the key crosses the wire in the \
                 clear, and that trade is yours to make rather than ours to refuse."
            }
            AField::Model => {
                "`open_ai_modelname`, sent as the request's `model`. Named because the endpoint \
                 cannot guess it: `windai`'s diagnostic is \"`open_ai_modelname` is empty — name the \
                 model to call\", not a default that may not exist on your account."
            }
            AField::ApiKey => {
                "`open_ai_api_key`, sent as one `Authorization: Bearer` header and nowhere else — not \
                 in argv, not in the environment, not in a log. The box starts empty on purpose: an \
                 empty box leaves the stored key where it is, so this page never has to display it, \
                 not even masked and at its true length. Type a key to replace it, or press Clear to \
                 remove it. `windai` counts the installer's `your_api_key_here` as no key at all."
            }
            AField::TitleLimit => {
                "`ai_extract_tag_wintitle_limit`: rows of the window-title table `windai tags` hands \
                 the model for a day, and it doubles that figure for a month. Upstream's widget had \
                 no bounds. The floor here is the one `windai` computes with anyway — \
                 `Settings::read` clamps a negative to zero and `tags.rs` then takes `.max(1)` — and \
                 the ceiling is this page's own guard against a table nobody could pay to send, since \
                 `windai` imposes no upper limit at all."
            }
            AField::MaxTags => {
                "`ai_extract_max_tag_num`: how many tags a day keeps; a month keeps one and a half \
                 times it. The figure is interpolated into the system prompt as \"Return at most N \
                 tags\" *and* the answer is truncated to it afterwards, so this is the only place the \
                 two can agree — upstream's prompt hard-coded 15 while the setting said otherwise. \
                 Zero would ask for no tags, hence the floor of 1."
            }
            AField::FilterWords => {
                "`ai_extract_tag_filter_words`: substrings cut out of every title *before* the table \
                 is assembled, so a term listed here never crosses the network. This is not a display \
                 filter and not the same list as `exclude_words` on the Settings page, which drops a \
                 window from the index entirely and is edited there."
            }
            AField::TagEnabled => {
                "`enable_ai_extract_tag`. The master switch for the tagger, and one of the two gates \
                 `windmaint`'s idle pass checks before it spends a request on a month: off, `windai tags` \
                 declines at the one binary that actually spends and the idle pass never spawns it, so \
                 nothing crosses the network. The month table it would have sent is \
                 `ai_extract_tag_wintitle_limit` rows wide, and the tags it keeps are \
                 `ai_extract_max_tag_num`. `Config::ai_extract_tag_enabled` is the only place the absent \
                 key's meaning is written — read here, by `windai`'s own reader and by the pass."
            }
            AField::TagInIdle => {
                "`enable_ai_extract_tag_in_idle`. Only matters once the switch above is on. On, the idle \
                 maintenance pass runs `windai tags` over the current and previous month while nobody is \
                 at the machine; off, the tagger still works when you or an AI tool run it, and never on a \
                 timer. It sends window *titles* — filtered through `ai_extract_tag_filter_words` and \
                 `exclude_words` first — not screen text; that is the other idle switch, \
                 `enable_ai_summary_in_idle`, below."
            }
            AField::McpEnabled => {
                "`enable_mcp_server`. The tray starts and stops `windmcp.exe` with this switch, and \
                 until it is on nothing listens at all — every AI tool on the machine gets a refused \
                 connection. The tray notices the change when it next opens its menu, so a restart is \
                 not needed; `windmcp status` and `windsvc doctor` both answer what is running now."
            }
            AField::McpHost => {
                "`mcp_server_host`. `127.0.0.1` is the whole point: the service answers questions \
                 about everything this computer's screen showed. Any other address puts that on the \
                network, and the token below stops being decoration at that moment."
            }
            AField::McpPort => {
                "`mcp_server_port`. Upstream's own default is 21120. A port already taken is reported \
                 by the status line rather than tried again in silence."
            }
            AField::McpAuth => {
                "`mcp_server_auth_required`. On, every request must carry the token below. Off, \
                 anything that can reach the address above reads the index — which is why the bridge \
                 also refuses to serve on a non-loopback address with auth off, rather than letting \
                 this box be the only thing between a stranger and your recordings."
            }
            AField::SummaryInIdle => {
                "`enable_ai_summary_in_idle`. The idle maintenance pass runs `windai summarize`, which \
                 sends each stretch's captured screen text to `open_ai_base_url` — unlike the month \
                 tagger, which sends window titles only. Off, the feature still works when you run it \
                 from a terminal or an AI tool, and nothing is ever sent on a timer. There is no key to \
                 set for it to be true: with the shipped placeholder `open_ai_api_key`, `windai` refuses \
                 to send a byte at all."
            }
            AField::McpToken => {
                "`mcp_server_token`, compared as a bearer token. It is never displayed here: an empty \
                 box leaves the stored token exactly where it is, so type one to replace it. Shorter \
                 than 24 characters the bridge treats as no token at all, because a token that can be \
                 guessed is not an access control."
            }
        }
    }

    pub fn kind(self, current: &AiSettings) -> AKind {
        match self {
            // The menu is the install's own file. `require_usable` refuses a selection outside it, so
            // the box may only ever offer what that check will accept — plus the one dialect this
            // build can actually speak, so a config whose list was hand-narrowed to nothing still has
            // something to be put right to.
            AField::EndpointType => {
                let mut list = current.endpoint_types.clone();
                if list.iter().all(|t| t != OPENAI_COMPATIBLE) {
                    list.push(OPENAI_COMPATIBLE.to_string());
                }
                AKind::Choice(list)
            }
            AField::BaseUrl => AKind::Text { max_chars: 300 },
            AField::Model => AKind::Text { max_chars: 120 },
            AField::ApiKey => AKind::Secret { max_chars: 400 },
            AField::TitleLimit => AKind::Int { min: 1, max: 10_000 },
            AField::MaxTags => AKind::Int { min: 1, max: 200 },
            AField::FilterWords => AKind::Lines { max_entries: 200 },
            AField::TagEnabled | AField::TagInIdle => AKind::Bool,
            AField::McpEnabled | AField::McpAuth | AField::SummaryInIdle => AKind::Bool,
            AField::McpHost => AKind::Text { max_chars: 64 },
            AField::McpPort => AKind::Int { min: 1, max: 65_535 },
            AField::McpToken => AKind::Secret { max_chars: 200 },
        }
    }
}

/// A field's value in the shape the form holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ATyped {
    Int(i64),
    Bool(bool),
    Text(String),
    Lines(Vec<String>),
}

/// What the user is typing, per field, kept apart from the parsed value for the reason `settings.rs`
/// gives — a number mid-deletion is not zero — plus the two things no other draft in this crate
/// needs: a revision counter, so the verdict line recomputes when the form changes rather than every
/// frame, and a `key_cleared` flag, because "leave the stored key" and "erase the stored key" cannot
/// both be spelled by an empty box.
#[derive(Clone)]
pub struct AiDraft {
    fields: BTreeMap<AField, String>,
    key_cleared: bool,
    revision: u64,
}

impl fmt::Debug for AiDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `AppState` derives `Debug`, so this is the shape every future `eprintln!("{state:?}")`
        // takes. The key field is reported as a presence marker only: not the value, not its length.
        let mut map = f.debug_map();
        for (field, text) in &self.fields {
            if *field == AField::ApiKey {
                map.entry(field, &"(held)");
            } else {
                map.entry(field, text);
            }
        }
        map.finish()
    }
}

impl AiDraft {
    /// Seed every box from the settings except the key, which always starts empty.
    pub fn from(settings: &AiSettings) -> AiDraft {
        let mut fields = BTreeMap::new();
        for field in AField::ALL {
            fields.insert(field, render(settings.get(field)));
        }
        AiDraft { fields, key_cleared: false, revision: 0 }
    }

    pub fn text(&self, field: AField) -> &str {
        self.fields.get(&field).map(String::as_str).unwrap_or("")
    }

    /// A switch's current draft text, read the way `AKind::Bool` writes it.
    pub fn bool_of(&self, field: AField) -> bool {
        self.text(field) == "true"
    }

    pub fn set_text(&mut self, field: AField, text: &str) {
        self.fields.insert(field, text.to_string());
        self.revision += 1;
    }

    /// Bumped by every edit, so `app.rs` can tell whether the verdict line is still the one this form
    /// deserves.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Press Clear: the next Save writes an empty `open_ai_api_key`. The bridge's token has no button,
    /// and an untouched token box is left where it is — see [`apply`]'s `cleared`.
    pub fn clear_key(&mut self) {
        self.key_cleared = true;
        self.fields.insert(AField::ApiKey, String::new());
        self.revision += 1;
    }

    /// Parse and clamp every field. The returned notes are what the widget says next to a value it had
    /// to correct; an empty `Vec` means the draft is exactly what will be written.
    pub fn validate(&self, settings: &AiSettings) -> (AiSettings, Vec<String>) {
        let mut out = settings.clone();
        let mut notes = Vec::new();
        for field in AField::ALL {
            apply(field, self.text(field), self.key_cleared, settings, &mut out, &mut notes);
        }
        // The bridge's own bind rule, applied to what this page is about to write. Only when the
        // switch is on: a dormant combination costs nothing and refusing to save it would block every
        // other edit on the tab, while an enabled one that cannot bind is precisely the silence this
        // page exists to remove. The sentence is the service's, verbatim — including the token length
        // it wants and the loopback rule it will not bend — because two answers to "why won't it
        // start" is one more than the person reading the page can use.
        if out.mcp_enabled {
            if let Err(refused) = auth::startup_guard(&out.mcp_host, out.mcp_port, &out.mcp_token, out.mcp_auth_required) {
                notes.push(refused.0);
            }
        }
        (out, notes)
    }
}

fn render(value: ATyped) -> String {
    match value {
        ATyped::Int(v) => v.to_string(),
        ATyped::Bool(v) => v.to_string(),
        ATyped::Text(v) => v,
        ATyped::Lines(v) => v.join("\n"),
    }
}

fn apply(field: AField, raw: &str, key_cleared: bool, base: &AiSettings, into: &mut AiSettings, notes: &mut Vec<String>) {
    // The Clear button belongs to `open_ai_api_key` and to nothing else. The bridge's token is this
    // page's other secret and shares the "an empty box means leave the stored value alone" rule, so one
    // flag read by both would make removing an inference key also revoke the bridge — and say so in a
    // note about a field the user never touched.
    let cleared = key_cleared && field == AField::ApiKey;
    // What a rejection calls the field's current value. For the key this is a *category*, never a
    // rendering: "kept <the key>" would be a leak in a note, and a row of bullets would be a lie
    // about a value this page has never looked at.
    let kept = || match field {
        AField::ApiKey => "the stored key".to_string(),
        AField::McpToken => "the stored token".to_string(),
        other => render(base.get(other)),
    };
    let typed = match field.kind(into) {
        AKind::Bool => ATyped::Bool(raw == "true"),
        AKind::Int { min, max } => {
            let trimmed = raw.trim();
            let parsed: i64 = match trimmed.parse() {
                // Rejection, not silence: the field keeps its previous value and says so.
                Ok(v) => v,
                Err(_) => {
                    notes.push(format!("{}: '{trimmed}' is not a whole number, kept {}", field.label(), kept()));
                    return;
                }
            };
            let clamped = parsed.clamp(min, max);
            if clamped != parsed {
                notes.push(format!("{}: {parsed} is outside {min}..={max}, clamped to {clamped}", field.label()));
            }
            ATyped::Int(clamped)
        }
        AKind::Text { max_chars } => {
            let value = raw.trim().to_string();
            if value.is_empty() {
                notes.push(format!("{}: must not be empty, kept '{}'", field.label(), kept()));
                return;
            }
            let mut value = value;
            if value.chars().count() > max_chars {
                notes.push(format!("{}: longer than {max_chars} characters, truncated", field.label()));
                value = value.chars().take(max_chars).collect();
            }
            ATyped::Text(value)
        }
        AKind::Secret { max_chars } => {
            // Not `AKind::Text` in disguise. An empty secret box means "leave the stored token where
            // it is", not "the user typed nothing so reject it": the box *starts* empty on a page
            // whose key is already set, and treating that as a rejection would make every unrelated
            // Save on this tab impossible.
            let mut value = raw.trim().to_string();
            if value.is_empty() {
                if cleared {
                    notes.push(format!("{}: cleared, the stored key will be removed", field.label()));
                    into.set(field, ATyped::Text(String::new()));
                }
                return;
            }
            // Anything typed here beats the Clear button, because it is the last thing the user did
            // to the field.
            if value.chars().count() > max_chars {
                notes.push(format!("{}: longer than {max_chars} characters, truncated", field.label()));
                value = value.chars().take(max_chars).collect();
            }
            ATyped::Text(value)
        }
        AKind::Lines { max_entries } => {
            let mut entries: Vec<String> =
                raw.lines().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
            entries.sort();
            entries.dedup();
            if entries.len() > max_entries {
                notes.push(format!("{}: more than {max_entries} entries, kept the first {max_entries}", field.label()));
                entries.truncate(max_entries);
            }
            ATyped::Lines(entries)
        }
        AKind::Choice(list) => {
            let value = raw.trim().to_string();
            if list.iter().any(|known| *known == value) {
                ATyped::Text(value)
            } else {
                // A kept value, not a lost one: a config naming a dialect this build cannot speak must
                // still load, and the verdict line is where it gets named.
                notes.push(format!(
                    "{}: '{value}' is not one of {}; kept {}",
                    field.label(),
                    if list.is_empty() { "any dialect the install offers".to_string() } else { list.join(", ") },
                    kept()
                ));
                return;
            }
        }
    };
    into.set(field, typed);
}

// ---------------------------------------------------------------------------------------------
// Reusing `windai`'s judgement rather than restating it
// ---------------------------------------------------------------------------------------------

/// Stage `staged` over a copy of the live config and hand the result to `windai`'s reader.
///
/// This is the seam that makes "the UI and the CLI can never disagree about what a valid
/// configuration is" true rather than aspirational: what comes back is the same
/// `wind_ai::settings::Settings` `windai doctor` prints and `Client::ask` sends from, built by the
/// same function, from the same merged map this page's Save is about to write. The clone is
/// in-memory only — no file is read and none is written.
pub fn read_back(config: &Config, staged: &AiSettings) -> AiRead {
    let mut merged = config.clone();
    staged.stage(&mut merged);
    AiRead::read(&merged)
}

/// The verdict line: `windai`'s own `require_usable`, in its own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiVerdict {
    pub ok: bool,
    /// "ready", or the precise *which key needs editing* sentence `require_usable` produced.
    pub message: String,
}

/// Ask `windai` whether the values this page is about to write are usable and, if not, which key it
/// objects to.
///
/// `Faults` is built holding the key under discussion so the sentence is scrubbed on its way out like
/// every other string this crate shows. `require_usable` itself only ever interpolates endpoint names
/// and the placeholder, and its own comment argues for `Faults::anonymous()` on exactly that ground;
/// routing it through the real key anyway costs one clone and means this call site can never be the
/// one that started printing a token.
pub fn verdict(config: &Config, staged: &AiSettings) -> AiVerdict {
    let read = read_back(config, staged);
    let faults = Faults::new(&SecretKey::new(staged.key().to_string()));
    match read.require_usable(&faults) {
        Ok(()) => AiVerdict { ok: true, message: "windai is configured and can send a request".to_string() },
        Err(error) => AiVerdict { ok: false, message: error.message() },
    }
}

// ---------------------------------------------------------------------------------------------
// The bridge's own answer
// ---------------------------------------------------------------------------------------------

/// What the MCP bridge would do with the settings on disk, as one row the page can put under the
/// group it belongs to.
///
/// This is the part of the bridge a person can actually act on: the switch, the address, whether
/// anything is answering there this second, and the URL to paste into an assistant. It is deliberately
/// not a second opinion about validity — [`BridgeStatus::refused`] carries the service's own sentence
/// verbatim, and the page's Save path runs the same `startup_guard` on what is being typed, so the row
/// and the refusal cannot disagree with the process they describe.
///
/// The bearer token is not in here. `Runtime::token()` returns it, and this type keeps only its
/// length, for the same reason [`key_state`] keeps the API key as a category: the question the row
/// answers is "is it set, is it long enough", and the answer must survive a screenshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStatus {
    pub enabled: bool,
    /// Something accepted a TCP connect on this address just now.
    pub listening: bool,
    pub host: String,
    pub port: i64,
    pub url: String,
    pub auth_required: bool,
    pub token_chars: usize,
    /// The bridge's refusal, word for word. `None` means it would bind if the tray started it.
    pub refused: Option<String>,
}

impl BridgeStatus {
    /// Which sentence this state is, as `(catalog key, the English to fall back on)`. One function
    /// decides which applies, so the egui window and the HTML window cannot describe the same service
    /// two ways — and neither of them carries a translation of its own.
    pub fn state_row(&self) -> (&'static str, &'static str) {
        match (self.enabled, &self.refused, self.listening) {
            (false, _, _) => ("ai_bridge_state_off", "switched off"),
            (_, Some(_), _) => ("ai_bridge_state_refused", "will not start"),
            (_, None, true) => ("ai_bridge_state_up", "listening"),
            (_, None, false) => ("ai_bridge_state_idle", "asked for, not running"),
        }
    }

    /// The address as the bridge spells it, for a row that has to name the port it was asked about.
    pub fn authority(&self) -> String {
        runtime::format_authority(&self.host, self.port)
    }
}

/// Ask the install, not the page's own draft: the bridge reads its settings from disk when it starts,
/// so "is it up" and "what will it do" are questions about the file. A page that has just been typed
/// into gets its answer from [`AiDraft::validate`], which runs the same guard on the same rules.
pub fn bridge_status(root: &Path) -> BridgeStatus {
    let runtime = match Runtime::open(root) {
        Ok(runtime) => runtime,
        // Not a readable install. Say so through the same row rather than painting an "off" that is
        // really "unknown", which is how a broken path reads as a switch nobody asked to be thrown.
        Err(error) => {
            return BridgeStatus {
                enabled: false,
                listening: false,
                host: String::new(),
                port: 0,
                url: String::new(),
                auth_required: true,
                token_chars: 0,
                refused: Some(error.to_string()),
            }
        }
    };
    let (host, port, token, auth_required) =
        (runtime.host(), runtime.port(), runtime.token(), runtime.auth_required());
    // Asked only when the person actually asked for a listener. A switched-off bridge with no token is
    // a dormant configuration, not a broken one, and printing the service's refusal under it reads as
    // "something is wrong here" on the one row that is fine — the same reasoning that keeps
    // [`AiDraft::validate`] from refusing that save.
    let refused = if runtime.enabled() {
        auth::startup_guard(&host, port, &token, auth_required).err().map(|refused| refused.0)
    } else {
        None
    };
    BridgeStatus {
        enabled: runtime.enabled(),
        listening: runtime::listening(&host, port),
        url: runtime.client_url(),
        refused,
        token_chars: token.chars().count(),
        host,
        port,
        auth_required,
    }
}

// ---------------------------------------------------------------------------------------------
// Test connection
// ---------------------------------------------------------------------------------------------

/// One template as the panel shows it: the words in force, the words this build would use, and where
/// each came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRow {
    pub name: PromptName,
    /// What is sent. The user's file if they have one, the shipped file otherwise.
    pub text: String,
    /// The shipped copy, painted beside it so a stale override is visible rather than silent.
    pub shipped: String,
    /// Whether `text` is the user's.
    pub overridden: bool,
    /// Where `text` came from, as a path a person can open.
    pub path: String,
    /// What `text` is missing relative to the shipped copy, said as a count rather than a diff.
    pub changed: bool,
}

/// A template, by the name the file and the panel both use.
pub type PromptName = wind_base::prompts::Name;

/// The seven templates, as this install's files hold them.
pub fn prompt_rows(config: &Config) -> Vec<PromptRow> {
    wind_base::prompts::read_all(config)
        .into_iter()
        .map(|prompt| {
            let overridden = prompt.overridden();
            let changed = overridden && prompt.text.trim() != prompt.shipped.trim();
            PromptRow {
                changed,
                name: prompt.name,
                text: prompt.text,
                shipped: prompt.shipped,
                overridden,
                path: prompt.path.display().to_string(),
            }
        })
        .collect()
}

/// Write the user's own words for one template, or refuse them with the reason.
///
/// The validation is `wind_base::prompts`', the same one `windai` and the bridge's own writers meet, so
/// an edit that would send `{frames}` and forget `{frames_table}` cannot be saved from here and reach the
/// endpoint as a prompt with no screen text in it.
pub fn save_prompt(config: &Config, name: PromptName, text: &str) -> Result<String, String> {
    let path = wind_base::prompts::save(config, name, text).map_err(|e| scrub_lines(&e))?;
    Ok(path.display().to_string())
}

/// Delete one override so the shipped words answer again.
pub fn restore_prompt(config: &Config, name: PromptName) -> Result<bool, String> {
    wind_base::prompts::restore(config, name).map_err(|e| scrub_lines(&e))
}

fn scrub_lines(text: &str) -> String {
    text.replace('\r', "").replace('\n', " ")
}

/// What "try these words on a real stretch" produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTrial {
    pub ok: bool,
    /// Which stretch was asked about, so the line cannot be read as a general claim.
    pub segment: String,
    /// Characters of prompt plus screen text that left the machine.
    pub chars: usize,
    /// The reply, already redacted and clipped, or the failure as the client phrased it.
    pub message: String,
}

/// Send one real request built from `text` as the template named by `name`, and report what came back.
///
/// This is the live half of the prompt editor: without it, a person editing thirty lines of prose has to
/// save, then leave the window, then run a command to learn whether the result is what they meant. The
/// draft is used rather than the file, so the thing tested is the thing on screen. Nothing is written
/// here — not the prompt, not a summary.
pub fn try_prompt_with<T: Transport>(config: &Config, staged: &AiSettings, name: PromptName, text: &str, transport: T) -> PromptTrial {
    use wind_summary as summary;
    let failure = |message: String| PromptTrial { ok: false, segment: String::new(), chars: 0, message };
    let mut prompts = wind_base::prompts::Prompts::read(config);
    prompts.set(name, text.to_string());
    let reader = summary::Reader::fresh(config);
    let digests = wind_ai::summarize::digests(&prompts);
    let day = wind_base::clock::now();
    let shift = config.day_begin_minutes();
    // The most recent product day with anything in it, within two weeks. Newer than that, an idle
    // machine has usually not indexed the frames yet, and a trial against a stretch that is not in the
    // index would be reported as a broken prompt rather than as a missing one.
    let mut cursor = summary::day_of(day.naive_epoch_seconds(), shift);
    let mut found: Option<(summary::DayQueue, summary::DayMap)> = None;
    for _ in 0..14 {
        let queue = match summary::for_day_with(&reader, &cursor, &digests) {
            Ok(queue) => queue,
            Err(e) => return failure(e.to_string()),
        };
        let periods = summary::read_period(config, &cursor);
        if !queue.pending.is_empty() || !periods.entries.is_empty() {
            found = Some((queue, periods));
            break;
        }
        cursor = summary::day_of(queue.span.from - 1, shift);
    }
    let Some((queue, periods)) = found else {
        return failure("nothing to try it on: no stretch in the last two weeks has screen text to summarise".to_string());
    };
    let client = Client::with_transport(read_back(config, staged), transport);
    let (label, request) = match wind_ai::summarize::trial(&prompts, name, &queue, &periods) {
        Some(built) => built,
        None => return failure(format!("`{}` is not a summary template this trial can run", name.label())),
    };
    let started = Instant::now();
    let chat = ChatRequest { system: &request.0, user: &request.1, temperature: 0.3, json_mode: false };
    match client.ask(&chat) {
        Ok(completion) => {
            let held = redact(completion.text.trim(), staged.key());
            PromptTrial {
                ok: true,
                segment: format!("{label} · {} · {} chars out · {:.0} ms", queue.day, request.0.chars().count() + request.1.chars().count(), started.elapsed().as_secs_f64() * 1000.0),
                chars: request.0.chars().count() + request.1.chars().count(),
                message: clip(&held),
            }
        }
        Err(error) => PromptTrial { ok: false, segment: label.to_string(), chars: 0, message: error.message() },
    }
}

/// The real request path, for `app.rs` to hand to a worker.
pub fn try_prompt(config: &Config, staged: &AiSettings, name: PromptName, text: &str) -> PromptTrial {
    try_prompt_with(config, staged, name, text, WinHttp)
}

/// What "test the connection" produced: one line to paint, and which colour to paint it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReport {
    pub ok: bool,
    /// Already redacted. Nothing in here is reachable from the key by construction: the failure
    /// branches come from `Faults`, and the success branch is passed through `error::redact`.
    pub message: String,
}

/// One real round trip against the endpoint this page is holding.
///
/// Generic over `Transport` so the tests can drive a loopback listener through the *same*
/// `wind_ai::client::Client` the shipped build uses, and so the shipped build is the identical call
/// with [`WinHttp`]. Nothing here decides what is valid: `Client::ask` runs `require_usable` and the
/// plain-HTTP policy itself before a byte reaches a socket, and every error it returns was built by a
/// `Faults` that already holds the key.
///
/// The one thing `ask` does *not* redact is a **successful** reply, because on the happy path a
/// response body is not an error. An endpoint that reflects its own request headers into
/// `choices[0].message.content` is unusual but entirely possible, so the answer text goes through
/// `wind_ai::error::redact` here — before it is clipped, and long before it reaches a label.
pub fn probe_with<T: Transport>(config: &Config, staged: &AiSettings, transport: T) -> TestReport {
    let client = Client::with_transport(read_back(config, staged), transport);
    let request = ChatRequest {
        system: "You are a connectivity check. Reply with the single word: ok",
        user: "ping",
        temperature: 0.0,
        json_mode: false,
    };
    let started = Instant::now();
    match client.ask(&request) {
        Ok(completion) => {
            let held = redact(completion.text.trim(), staged.key());
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let tokens = completion.usage.map(|u| format!(" · {} tokens", u.total_tokens.max(0))).unwrap_or_default();
            TestReport {
                ok: true,
                message: format!(
                    "the endpoint answered in {ms:.0} ms · {} characters back{tokens}: 「{}」",
                    held.chars().count(),
                    clip(&held)
                ),
            }
        }
        Err(error) => TestReport { ok: false, message: error.message() },
    }
}

/// The real request path, for `app.rs` to hand to a worker.
pub fn probe(config: &Config, staged: &AiSettings) -> TestReport {
    probe_with(config, staged, WinHttp)
}

fn clip(text: &str) -> String {
    let kept: String = text.chars().take(REPLY_CLIP).collect();
    if kept.chars().count() < text.chars().count() {
        format!("{kept}…")
    } else {
        kept
    }
}

/// Redact a message this page produced about a write that did not happen. `ConfigError`'s text names
/// paths, and a path is allowed to contain anything a user typed into a config file — including a
/// token. This is the one call site that decides the AI page's own status line is safe to paint.
pub fn scrub(text: &str, staged: &AiSettings) -> String {
    redact(text, staged.key())
}


#[cfg(test)]
pub mod tests {
    use super::*;

    /// A key worth hunting for: recognisable, and long enough that a partial match would be obvious.
    pub const SECRET: &str = "sk-UIPROOF-0123456789abcdef";

    /// A scratch install root carrying `defaults` as its factory settings. Same convention the rest of
    /// this crate's tests use — a numbered temp directory, removed at the end — because `cargo test`
    /// runs these in threads inside one process and a fixed name is a race on `config_user.json`.
    fn root_holding(defaults: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("windui-ai-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), defaults).unwrap();
        dir
    }

    fn config_holding(defaults: &str) -> Config {
        Config::load(&root_holding(defaults)).expect("the fixture root must load")
    }

    /// A form the CLI would accept, for the tests that are about some *other* field. Needed rather
    /// than convenient: `require_usable` reports the highest-priority defect only, so a case aimed at
    /// the model name has to carry a real key or it silently tests the key instead. The AI crate's own
    /// `a_blank_or_wrong_value_names_the_key_that_needs_editing` states the same rule.
    fn usable_form() -> AiSettings {
        AiSettings { api_key: SECRET.into(), ..AiSettings::default() }
    }

    /// The page may not accept what the service will refuse to start with. The sentence is the bridge's
    /// own, verbatim — two answers to "why will it not start" is one more than the person reading the
    /// page can use, and `windmcp serve` prints this one before it exits.
    #[test]
    fn an_enabled_bridge_that_cannot_bind_is_refused_in_the_bridges_own_words() {
        let config = config_holding("{}");
        let form = AiSettings::load(&config);
        let mut draft = AiDraft::from(&form);
        draft.set_text(AField::McpEnabled, "true");
        draft.set_text(AField::McpPort, "21121");
        let (staged, problems) = draft.validate(&form);
        assert!(staged.mcp_enabled && staged.mcp_port == 21121, "the two fields were legal on their own");
        let joined = problems.join("\n");
        assert!(joined.contains("mcp_server_token"), "{problems:?}");
        assert!(joined.contains(&auth::TOKEN_MIN_CHARS.to_string()), "{problems:?}");
    }

    /// Off is dormant, not broken. Refusing that save would block every unrelated edit on the tab for a
    /// service nobody is starting.
    #[test]
    fn a_disabled_bridge_is_not_the_validators_business() {
        let config = config_holding("{}");
        let form = AiSettings::load(&config);
        let mut draft = AiDraft::from(&form);
        draft.set_text(AField::McpEnabled, "false");
        draft.set_text(AField::McpPort, "21121");
        let (_, problems) = draft.validate(&form);
        assert!(!problems.iter().any(|problem| problem.contains("mcp_server")), "{problems:?}");
    }

    /// The one combination the bridge will not open at all: an address anyone on the network can reach,
    /// with the bearer gate switched off. It has to be refused before the file is written, because a
    /// saved setting that never took effect is the failure this whole page exists to remove.
    #[test]
    fn an_open_bind_without_the_bearer_gate_is_refused_before_it_is_written() {
        let config = config_holding("{}");
        let form = AiSettings::load(&config);
        let mut draft = AiDraft::from(&form);
        draft.set_text(AField::McpEnabled, "true");
        draft.set_text(AField::McpHost, "0.0.0.0");
        draft.set_text(AField::McpAuth, "false");
        draft.set_text(AField::McpToken, &"t".repeat(auth::TOKEN_MIN_CHARS));
        let (_, problems) = draft.validate(&form);
        assert!(problems.iter().any(|problem| problem.contains("0.0.0.0")), "{problems:?}");
    }

    /// A bridge nobody asked for is not a broken bridge. The row must not print the service's refusal
    /// under a switched-off install — that is a person with the feature off reading a fault into their
    /// own machine — while an enabled one with no token says exactly why it will not come up.
    #[test]
    fn a_switched_off_bridge_is_reported_as_off_and_not_as_broken() {
        let off = root_holding(r#"{"enable_mcp_server": false}"#);
        let status = bridge_status(&off);
        assert!(!status.enabled);
        assert_eq!(status.state_row().0, "ai_bridge_state_off");
        assert!(status.refused.is_none(), "off is dormant: {:?}", status.refused);

        let on = root_holding(r#"{"enable_mcp_server": true}"#);
        let status = bridge_status(&on);
        assert!(status.enabled);
        assert!(status.refused.as_deref().unwrap_or_default().contains("mcp_server_token"), "no token yet, so the row says why");
        assert_eq!(status.authority(), "127.0.0.1:21120", "the address the service would bind");
        assert_eq!(status.url, "http://127.0.0.1:21120/mcp", "and the URL to hand a client");
        let _ = std::fs::remove_dir_all(&off);
        let _ = std::fs::remove_dir_all(&on);
    }

    #[test]
    fn the_bridge_row_names_whichever_state_is_true() {
        let asked = BridgeStatus {
            enabled: true,
            listening: false,
            host: "127.0.0.1".into(),
            port: 21120,
            url: "http://127.0.0.1:21120/mcp".into(),
            auth_required: true,
            token_chars: 30,
            refused: None,
        };
        assert_eq!(asked.state_row().0, "ai_bridge_state_idle");
        assert_eq!(asked.authority(), "127.0.0.1:21120");
        assert_eq!(BridgeStatus { listening: true, ..asked.clone() }.state_row().0, "ai_bridge_state_up");
        assert_eq!(
            BridgeStatus { refused: Some("refused".into()), ..asked.clone() }.state_row().0,
            "ai_bridge_state_refused"
        );
        assert_eq!(BridgeStatus { enabled: false, ..asked.clone() }.state_row().0, "ai_bridge_state_off");
    }

    /// The state is chosen in Rust and translated by the catalog, so a locale missing a row would print
    /// a raw key on a Chinese or Japanese install. The same promise the tray's menu labels are held to.
    #[test]
    fn every_bridge_state_row_is_translated_in_every_locale_the_app_ships() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config_src/languages.json"))
            .expect("the shipped catalog");
        let catalog: BTreeMap<String, Value> = serde_json::from_str(&raw).expect("valid JSON");
        let rows = ["ai_bridge_state_off", "ai_bridge_state_up", "ai_bridge_state_idle", "ai_bridge_state_refused"];
        for locale in ["en", "sc", "ja"] {
            let table = catalog.get(locale).and_then(Value::as_object).unwrap_or_else(|| panic!("{locale} is missing"));
            for row in rows {
                let text = table.get(row).and_then(Value::as_str).unwrap_or_default();
                assert!(!text.trim().is_empty(), "{locale} ships no {row}");
            }
        }
    }

    #[test]
    fn the_default_is_what_the_shipped_file_says() {
        let dir = root_holding(
            r#"{"open_ai_base_url": "https://api.openai.com/v1", "open_ai_modelname": "gpt-4o",
                "open_ai_api_key": "your_api_key_here", "ai_api_endpoint_selected": "OpenAI compatible",
                "ai_api_endpoint_type": ["OpenAI compatible"], "ai_extract_tag_wintitle_limit": 30,
                "ai_extract_max_tag_num": 15, "ai_extract_tag_filter_words": ["Kim Jong-un"]}"#,
        );
        let config = Config::load(&dir).unwrap();
        let loaded = AiSettings::load(&config);
        // `exclude_words` is not one of this page's keys — the Settings page owns it — so it is the
        // one field a stock file can legitimately differ on, and is asserted apart rather than folded
        // into the default.
        assert_eq!(loaded.exclude_words, 0, "this fixture file names no exclude_words");
        assert_eq!(loaded, AiSettings::default(), "a stock settings file and this struct must not drift");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The anti-dead-control test, and the reason the key names in [`AField::key`] are worth fifteen
    /// literals instead of a shared constant: each staged key is read back through
    /// `wind_ai::settings::Settings::read` — the function `windai` itself reads with — and asserted to
    /// have landed in the field `windai` uses. A rename on either side fails here, in the UI crate,
    /// rather than turning a configured feature off in front of a user who already paid for a key.
    #[test]
    fn every_key_lands_where_windai_reads_it() {
        let config = config_holding("{}");
        let staged = AiSettings {
            endpoint_selected: OPENAI_COMPATIBLE.into(),
            endpoint_types: vec![OPENAI_COMPATIBLE.into()],
            base_url: "https://gateway.test/v1".into(),
            model: "some-model".into(),
            api_key: SECRET.into(),
            summary_in_idle: true,
            wintitle_limit: 77,
            max_tag_num: 9,
            filter_words: vec!["alpha".into(), "beta".into()],
            tag_enabled: true,
            tag_in_idle: false,
            exclude_words: 0,
            image_search_promised: false,
            mcp_enabled: true,
            mcp_host: "127.0.0.1".into(),
            mcp_port: 21123,
            mcp_auth_required: false,
            mcp_token: "a-token-long-enough-to-be-a-secret".into(),
        };
        let read = read_back(&config, &staged);
        assert_eq!(read.base_url, "https://gateway.test/v1", "{} did not reach windai", AField::BaseUrl.key());
        assert_eq!(read.model, "some-model", "{} did not reach windai", AField::Model.key());
        assert_eq!(read.wintitle_limit, 77, "{} did not reach windai", AField::TitleLimit.key());
        assert_eq!(read.max_tag_num, 9, "{} did not reach windai", AField::MaxTags.key());
        assert_eq!(read.filter_words, vec!["alpha".to_string(), "beta".to_string()], "{} did not reach windai", AField::FilterWords.key());
        // The two switches the idle pass gates on. Set to the *opposite* of their defaults here, so a
        // page that staged neither — or staged a key `windai` does not read — cannot pass this by
        // accident. `ai_gate` asks the same two keys of `Config`; what this half proves is that the name
        // this page writes is the name the spender reads.
        assert!(read.enable_extract_tag, "{} did not reach windai", AField::TagEnabled.key());
        assert!(!read.enable_extract_tag_in_idle, "{} did not reach windai", AField::TagInIdle.key());
        assert_eq!(read.endpoint_selected, OPENAI_COMPATIBLE, "{} did not reach windai", AField::EndpointType.key());
        assert!(read.key_configured(), "{} must reach windai as a real key", AField::ApiKey.key());
        // The derived request target is the one number windai builds out of two of these fields.
        assert_eq!(read.chat_completions_url(), "https://gateway.test/v1/chat/completions");
        // The five MCP keys have no `windai` field to land in — `windmcp`'s runtime and the tray read
        // them out of the merged config themselves — so what this page can prove at this seam is the
        // narrower, still-fatal one: that the value it staged is the value a re-read of that config
        // hands back. The key *names* are asserted in `stage_writes_exactly_the_keys_this_page_owns`,
        // and the other half of the contract — that those names are the ones the bridge actually binds
        // with — is `windcap/mcp/tests/bridge.rs`, which starts a real server from a real file.
        let mut bridged = config.clone();
        staged.stage(&mut bridged);
        let bridge = AiSettings::load(&bridged);
        assert!(bridge.mcp_enabled, "{} did not reach the bridge", AField::McpEnabled.key());
        assert_eq!(bridge.mcp_host, "127.0.0.1", "{} did not reach the bridge", AField::McpHost.key());
        assert_eq!(bridge.mcp_port, 21123, "{} did not reach the bridge", AField::McpPort.key());
        assert!(!bridge.mcp_auth_required, "{} did not reach the bridge", AField::McpAuth.key());
        assert_eq!(
            bridge.mcp_token, "a-token-long-enough-to-be-a-secret",
            "{} did not reach the bridge",
            AField::McpToken.key()
        );
    }

    #[test]
    fn stage_writes_exactly_the_keys_this_page_owns() {
        let dir = root_holding(
            r#"{"enable_ai_extract_tag": true, "enable_ai_extract_tag_in_idle": false,
                "ai_extract_tag_in_idle_batch_size": 44, "enable_img_embed_search": true,
                "img_embed_module_install": true, "enable_ai_day_poem": false,
                "ai_api_endpoint_type": ["OpenAI compatible"], "exclude_words": ["KeePass"],
                "ai_extract_tag_result_dir": "result_ai_extract_tag", "user_name": "default"}"#,
        );
        let mut config = Config::load(&dir).unwrap();
        AiSettings { api_key: SECRET.into(), ..AiSettings::default() }.stage(&mut config);
        let path = config.save().expect("save");
        let raw = std::fs::read_to_string(&path).expect("written");
        for owned in AField::ALL {
            assert!(raw.contains(&format!("\"{}\":", owned.key())), "{} was not written: {raw}", owned.key());
        }
        // Everything this page refuses must come back byte-for-byte, which is the promise the Settings
        // page has always made about keys it does not own. The two tagger switches left this list when
        // they became rows: they are staged now, and `ai_extract_tag_in_idle_batch_size` is what is left
        // of the three keys `windai` reads that this page still refuses.
        for refused in [
            "ai_extract_tag_in_idle_batch_size",
            "enable_img_embed_search",
            "img_embed_module_install",
            "enable_ai_day_poem",
            "ai_api_endpoint_type",
            "exclude_words",
            "ai_extract_tag_result_dir",
        ] {
            assert!(raw.contains(&format!("\"{refused}\":")), "{refused} vanished from the merged map: {raw}");
        }
        // The switches are live in both directions: the file said true/false, this form's defaults say
        // false/true, and what landed is what the form holds. A row that only ever echoed the file would
        // pass a "survived unchanged" assertion and still be a dead control.
        assert!(raw.contains("\"enable_ai_extract_tag\": false"), "the tagger switch did not land: {raw}");
        assert!(raw.contains("\"enable_ai_extract_tag_in_idle\": true"), "and neither did its idle half: {raw}");
        assert!(raw.contains("\"ai_extract_tag_in_idle_batch_size\": 44"), "the batch size was rewritten: {raw}");
        assert!(raw.contains("\"exclude_words\": ["), "the Settings page's key was rewritten: {raw}");
        assert!(raw.contains("\"user_name\": \"default\""), "an unrelated key was lost: {raw}");
        assert!(raw.contains(&format!("\"open_ai_api_key\": \"{SECRET}\"")), "the key did not land: {raw}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_untouched_key_box_leaves_the_stored_token_alone() {
        let dir = root_holding(&format!(
            r#"{{"open_ai_api_key": "{SECRET}", "open_ai_base_url": "https://api.openai.com/v1",
                "open_ai_modelname": "gpt-4o"}}"#
        ));
        let config = Config::load(&dir).unwrap();
        let loaded = AiSettings::load(&config);
        assert_eq!(loaded.api_key, SECRET, "the file's key was read");
        let draft = AiDraft::from(&loaded);
        assert_eq!(draft.text(AField::ApiKey), "", "the box must open empty, not prefilled");
        let (validated, notes) = draft.validate(&loaded);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(validated, loaded, "an unedited key field is not a change to it");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clearing_the_key_is_the_only_way_this_page_writes_an_empty_one() {
        let dir = root_holding(&format!(
            r#"{{"open_ai_api_key": "{SECRET}", "open_ai_base_url": "https://api.openai.com/v1",
                "open_ai_modelname": "gpt-4o"}}"#
        ));
        let config = Config::load(&dir).unwrap();
        let loaded = AiSettings::load(&config);
        let mut draft = AiDraft::from(&loaded);
        draft.clear_key();
        let (validated, notes) = draft.validate(&loaded);
        assert!(validated.api_key.is_empty(), "Clear must actually clear");
        assert!(notes.iter().any(|n| n.contains("cleared")), "{notes:?}");
        assert_eq!(key_state(&config, &validated), KeyState::Absent, "and windai sees no key");

        // Typing over Clear wins, because it is the last thing the user did to the field.
        let mut retyped = AiDraft::from(&loaded);
        retyped.clear_key();
        retyped.set_text(AField::ApiKey, "sk-replacement");
        let (validated, notes) = retyped.validate(&loaded);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(validated.api_key, "sk-replacement");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_placeholder_key_and_an_absent_one_are_told_apart_the_way_windai_does() {
        let config = config_holding("{}");
        let mut staged = AiSettings { api_key: String::new(), ..AiSettings::default() };
        assert_eq!(key_state(&config, &staged), KeyState::Absent);
        staged.api_key = KEY_PLACEHOLDER.into();
        assert_eq!(key_state(&config, &staged), KeyState::Placeholder, "the installer's string is not a key");
        assert!(key_state(&config, &staged).is_unusable());
        staged.api_key = SECRET.into();
        let set = key_state(&config, &staged);
        assert!(matches!(set, KeyState::Set(_)), "{set:?}");
        let line = set.describe();
        assert!(!line.contains(SECRET), "the state line must not carry the key: {line}");
        assert!(line.contains("fingerprint"), "and must say what it does show: {line}");
        assert!(line.chars().count() < 160, "a fingerprint is short: {line}");
    }

    /// The reuse, asserted rather than claimed: the verdict text this page paints is *the same
    /// string* `require_usable` produced, for each of the four ways a configuration can be unusable.
    /// There is no second validator on this page for the CLI to disagree with.
    #[test]
    fn the_verdict_line_is_windais_own_diagnostic_verbatim() {
        // The install's own dialect menu, because `require_usable` compares the selection against the
        // *file's* `ai_api_endpoint_type` — which this page carries but never writes — and an empty
        // list skips that check entirely, sending both endpoint cases down the same branch.
        let config = config_holding(r#"{"ai_api_endpoint_type": ["OpenAI compatible"]}"#);
        // One broken value per case, or the message can only name the highest-priority defect and the
        // case proves nothing about the value it was written for — which is why the last two carry a
        // real key and the first, whose own field is checked before the key's, does not need one.
        let cases = [
            (AiSettings { base_url: String::new(), ..usable_form() }, "`open_ai_base_url`"),
            (AiSettings { api_key: KEY_PLACEHOLDER.into(), ..usable_form() }, "`open_ai_api_key`"),
            (AiSettings { api_key: String::new(), ..usable_form() }, "`open_ai_api_key`"),
            (AiSettings { model: String::new(), ..usable_form() }, "`open_ai_modelname`"),
            (AiSettings { endpoint_selected: "Anthropic".into(), ..usable_form() }, "OpenAI compatible"),
            (AiSettings { endpoint_selected: "Watson".into(), ..usable_form() }, "ai_api_endpoint_type"),
        ];
        for (staged, expected) in cases {
            let direct = read_back(&config, &staged)
                .require_usable(&Faults::new(&SecretKey::new(staged.key().to_string())))
                .expect_err("each case is one broken value")
                .message();
            let shown = verdict(&config, &staged);
            assert!(!shown.ok, "{staged:?} must be refused");
            assert_eq!(shown.message, direct, "the page must not paraphrase windai");
            assert!(shown.message.contains(expected), "{expected} missing from: {}", shown.message);
        }
        assert!(verdict(&config, &usable_form()).ok);
    }

    /// A stock install's file has a placeholder key, so the honest verdict is *not* "ready" — and the
    /// sentence that says so is the one naming the key to edit. This is the state every new user is
    /// actually in, and the reason `windai doctor` prints `NOT SET` today with nowhere to fix it.
    #[test]
    fn a_stock_install_is_reported_as_needing_a_key_not_as_broken() {
        let dir = root_holding(
            r#"{"open_ai_api_key": "your_api_key_here", "open_ai_base_url": "https://api.openai.com/v1",
                "open_ai_modelname": "gpt-4o"}"#,
        );
        let config = Config::load(&dir).unwrap();
        let loaded = AiSettings::load(&config);
        let shown = verdict(&config, &loaded);
        assert!(!shown.ok);
        assert!(shown.message.contains("open_ai_api_key"), "{}", shown.message);
        assert!(shown.message.contains(KEY_PLACEHOLDER), "and it must name the string to replace: {}", shown.message);
        assert!(!shown.message.contains("open_ai_base_url"), "the base url is fine and must not be blamed: {}", shown.message);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn out_of_range_and_unparseable_input_is_corrected_and_explained() {
        let base = AiSettings::default();
        let mut draft = AiDraft::from(&base);
        draft.set_text(AField::TitleLimit, "999999");
        draft.set_text(AField::MaxTags, "0");
        draft.set_text(AField::Model, "   ");
        let (parsed, notes) = draft.validate(&base);
        assert_eq!(parsed.wintitle_limit, 10_000);
        assert_eq!(parsed.max_tag_num, 1, "a month would ask for no tags at all otherwise");
        assert_eq!(parsed.model, "gpt-4o", "a text field that cannot be read keeps what it had");
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert_eq!(notes.iter().filter(|n| n.contains("clamped")).count(), 2, "{notes:?}");
    }

    #[test]
    fn a_note_about_the_key_field_names_the_field_and_never_the_value() {
        let base = AiSettings { api_key: SECRET.into(), ..AiSettings::default() };
        let mut draft = AiDraft::from(&base);
        draft.set_text(AField::EndpointType, "Watson");
        let (parsed, notes) = draft.validate(&base);
        assert_eq!(parsed.endpoint_selected, OPENAI_COMPATIBLE, "the loaded value survives");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("Endpoint dialect"), "{notes:?}");
        assert!(notes.iter().all(|n| !n.contains(SECRET)), "a note leaked the key: {notes:?}");
    }

    #[test]
    fn every_field_round_trips_through_its_own_draft_text() {
        let base = AiSettings::default();
        let draft = AiDraft::from(&base);
        let (parsed, notes) = draft.validate(&base);
        assert!(notes.is_empty(), "a pristine draft must not complain: {notes:?}");
        assert_eq!(parsed, base);
    }

    #[test]
    fn the_draft_carries_one_entry_per_editable_field() {
        let draft = AiDraft::from(&AiSettings::default());
        assert_eq!(
            AField::ALL.len(),
            15,
            "fifteen keys: upstream's seven Lab ones, the tagger's two switches, the bridge's five, and \
             the summariser's idle switch"
        );
        assert_eq!(draft.fields.len(), AField::ALL.len());
        assert!(draft.fields.keys().all(|field| AField::ALL.contains(field)));
    }

    #[test]
    fn editing_bumps_the_revision_and_reading_it_does_not() {
        let mut draft = AiDraft::from(&AiSettings::default());
        let before = draft.revision();
        assert_eq!(draft.revision(), before, "reading must not look like an edit");
        draft.set_text(AField::Model, "gpt-5");
        assert_eq!(draft.revision(), before + 1);
        draft.clear_key();
        assert_eq!(draft.revision(), before + 2);
    }

    /// The property the two hand-written `Debug` impls exist for: `AppState` derives `Debug`, so
    /// `{:?}` is the shape any future `eprintln!` of a whole screen state takes.
    #[test]
    fn the_key_survives_no_form_of_debug_printing() {
        let staged = AiSettings { api_key: SECRET.into(), ..AiSettings::default() };
        let mut draft = AiDraft::from(&staged);
        draft.set_text(AField::ApiKey, SECRET);
        draft.set_text(AField::BaseUrl, "https://x.test/v1");
        assert!(!format!("{staged:?}").contains(SECRET), "AiSettings::Debug leaked the key");
        assert!(!format!("{draft:?}").contains(SECRET), "AiDraft::Debug leaked the key");
        assert!(format!("{staged:?}").contains(wind_ai::error::REDACTED), "and Debug still says a key is there");
        assert!(format!("{draft:?}").contains("(held)"), "and says which field holds one");
        assert!(format!("{draft:?}").contains("https://x.test/v1"), "the other fields stay readable");
    }

    #[test]
    fn the_painted_report_is_clean_of_a_key_the_endpoint_reflected_back() {
        let config = config_holding("{}");
        let staged = AiSettings { api_key: SECRET.into(), ..AiSettings::default() };
        // An endpoint that echoes its own request headers into a 401 body — the shape
        // `error::redact` exists for, and the one error path a user of a misconfigured gateway hits.
        // Built with `json!` rather than a raw `format!` because a doubled-brace count off by one is a
        // compile error here and a silently-malformed body somewhere less obvious.
        let reflected = serde_json::json!({"error": {"message": format!("bad auth for Bearer {SECRET}")}}).to_string();
        let report = probe_with(&config, &staged, Echo { status: 401, body: reflected });
        assert!(!report.ok);
        assert!(!report.message.contains(SECRET), "the failure line leaked the key: {}", report.message);
        assert!(report.message.contains("401"), "and still says what happened: {}", report.message);

        // The same reflection on a 200, which `ask` treats as a successful completion: this is the
        // branch `Faults` never sees, and the reason `probe_with` redacts the answer itself.
        let echoed = format!("accepted {SECRET}, thank you very much");
        let body = serde_json::json!({"choices": [{"message": {"content": echoed}}]}).to_string();
        let report = probe_with(&config, &staged, Echo { status: 200, body });
        assert!(report.ok, "{}", report.message);
        assert!(!report.message.contains(SECRET), "the success line leaked the key: {}", report.message);
    }

    #[test]
    fn the_encoded_reflection_is_scrubbed_in_both_hex_cases_through_the_ui_path() {
        // The percent-encoding pair `ai/src/error.rs` documents as a real fixed bug on this branch,
        // asserted from the *page's* side rather than the library's. `redact`'s own encoder escapes
        // every non-alphanumeric byte, so a key of `sk+ab/cd=1` has an encoded spelling at all — and
        // a gateway that lower-cases its hex digits is the case a single-substitution filter misses.
        let config = config_holding("{}");
        let staged = AiSettings { api_key: "sk+ab/cd=1".into(), ..AiSettings::default() };
        let body = r#"{"error":{"message":"rejected sk%2Bab%2Fcd%3D1 and sk%2bab%2fcd%3d1"}}"#.to_string();
        let report = probe_with(&config, &staged, Echo { status: 400, body });
        assert!(!report.message.contains("sk%2Bab%2Fcd%3D1"), "{}", report.message);
        assert!(!report.message.contains("sk%2bab%2fcd%3d1"), "{}", report.message);
        assert!(!report.message.contains("sk+ab/cd=1"), "{}", report.message);
        assert_eq!(report.message.matches(wind_ai::error::REDACTED).count(), 2, "{}", report.message);
    }

    /// The address is the user's to write. This page used to refuse `http://` beyond loopback before
    /// the transport was ever reached and call the result a configuration fault, which is what made a
    /// gateway on the same network impossible to use from the window — the row said "unencrypted"
    /// while the user's LAN endpoint was sitting there answering. Nothing filters the scheme now, so
    /// the probe is asked for exactly the URL that was typed.
    #[test]
    fn a_cleartext_endpoint_on_the_local_network_is_probed_like_any_other() {
        let config = config_holding("{}");
        let staged = AiSettings { base_url: "http://192.0.2.10:3321/v1".into(), ..usable_form() };
        let sent = Sent::default();
        let report = probe_with(&config, &staged, sent.clone());
        assert!(report.ok, "the page asked and the transport answered: {}", report.message);
        assert_eq!(
            sent.last_url(),
            "http://192.0.2.10:3321/v1/chat/completions",
            "the address as written, with the endpoint inserted"
        );
    }

    /// An unusable configuration is refused by `require_usable`, inside `Client::ask`, before the
    /// transport is reached — so the page shows `windai`'s sentence rather than a socket error three
    /// layers away, and a half-filled form cannot spend money.
    #[test]
    fn an_unusable_configuration_never_reaches_the_transport() {
        let config = config_holding("{}");
        let staged = AiSettings { api_key: KEY_PLACEHOLDER.into(), ..AiSettings::default() };
        let report = probe_with(&config, &staged, FailIfCalled);
        assert!(!report.ok);
        assert!(report.message.contains("open_ai_api_key"), "{}", report.message);
    }

    #[test]
    fn scrub_reaches_the_same_text_redact_does() {
        let staged = AiSettings { api_key: SECRET.into(), ..AiSettings::default() };
        let dirty = format!("cannot write C:\\userdata\\{SECRET}\\config_user.json: denied");
        let clean = scrub(&dirty, &staged);
        assert!(!clean.contains(SECRET), "{clean}");
        assert!(clean.contains(wind_ai::error::REDACTED), "{clean}");
    }

    /// One row of index for today, so a trial has real screen text to build a request from. Built from
    /// the clock rather than a fixed stamp because `try_prompt_with` starts at *this* product day and
    /// walks back — a fixture dated last month would be found, but only after fourteen reads.
    fn seed_a_stretch_today(root: &std::path::Path) -> String {
        use wind_summary::test_support as support;
        let stamp = wind_base::clock::now().stamp();
        let file = format!("{stamp}.mp4");
        std::fs::create_dir_all(root.join("userdata/db")).expect("db dir");
        support::seed_month(
            root,
            "default",
            &[(support::at(&stamp), file.as_str(), "Qoder — summarise.rs", "a spreadsheet with the Q3 numbers in it")],
        );
        stamp
    }

    /// The panel's whole contract in one place: seven rows, each naming the file that answered, and only
    /// the one the user rewrote marked as theirs. `shipped` is the embedded copy on a scratch install,
    /// which is the case worth pinning: the page must still show the default words when the install has
    /// no `config_src/ai_prompts` beside it.
    #[test]
    fn the_seven_rows_name_the_file_that_answered_and_flag_only_the_one_the_user_rewrote() {
        let root = root_holding(r#"{"user_name":"default","day_begin_minutes":180}"#);
        let config = Config::load(&root).expect("fixture config");
        let rows = prompt_rows(&config);
        assert_eq!(rows.len(), wind_base::prompts::Name::ALL.len(), "one row per editable template");
        assert!(rows.iter().all(|row| !row.overridden && !row.changed), "a fresh install answers from the shipped words");
        assert!(rows.iter().all(|row| !row.text.trim().is_empty()), "and the shipped words are really there: {:?}", rows.iter().map(|row| row.name).collect::<Vec<_>>());

        save_prompt(&config, PromptName::PeriodUser, "Summarise the stretch. {frames_table}").expect("a kept placeholder saves");
        let after = prompt_rows(&config);
        let rewritten = after.iter().find(|row| row.name == PromptName::PeriodUser).expect("the row is still listed");
        assert!(rewritten.overridden && rewritten.changed, "the user's copy is marked as the user's");
        assert_eq!(rewritten.text, "Summarise the stretch. {frames_table}");
        assert!(rewritten.path.replace('\\', "/").contains("userdata/ai_prompts/"), "{}", rewritten.path);
        assert_eq!(after.iter().filter(|row| row.overridden).count(), 1, "and exactly one row moved");
        let untouched = after.iter().find(|row| row.name == PromptName::DailyUser).expect("row");
        assert_eq!(untouched.text, untouched.shipped, "the others still read as shipped");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The page must not be able to save a prompt that would send no screen text, and the refusal it
    /// shows is `wind_base::prompts`' own sentence with its newlines flattened — the same rule `windai`
    /// and the bridge's writers meet, so there is no way to store a prompt from here that the sender
    /// would later have to reject.
    #[test]
    fn a_prompt_that_drops_the_placeholder_it_must_keep_is_refused_and_leaves_no_file() {
        let root = root_holding(r#"{"user_name":"default","day_begin_minutes":180}"#);
        let config = Config::load(&root).expect("fixture config");
        let refused = save_prompt(&config, PromptName::PeriodUser, "Summarise it well.").expect_err("a prompt with no {frames_table} is not a prompt");
        assert!(refused.contains("{frames_table}"), "{refused}");
        assert!(!refused.contains('\n'), "one line, because the row paints it inline: {refused}");
        assert!(!wind_base::prompts::override_path(&config, PromptName::PeriodUser).exists(), "and the refusal wrote nothing");
        assert!(!save_prompt(&config, PromptName::PeriodUser, "   ").expect_err("blank is refused too").is_empty(), "an empty instruction is named as such");

        save_prompt(&config, PromptName::PeriodUser, "Summarise. {frames_table}").expect("kept placeholder saves");
        assert!(restore_prompt(&config, PromptName::PeriodUser).expect("the override is there to go"));
        assert!(!prompt_rows(&config).iter().any(|row| row.overridden), "restored, so the shipped words answer again");
        assert!(!restore_prompt(&config, PromptName::PeriodUser).expect("restoring twice is not an error"), "and there is nothing left to restore");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The live half of the editor: it sends the words on screen, not the words on disk, and it writes
    /// nothing. Without this row a person editing thirty lines of prose has to save, leave the window,
    /// and run a command to learn whether the result is what they meant — and saving a bad prompt is the
    /// thing the trial exists to make unnecessary.
    #[test]
    fn a_trial_sends_the_words_on_screen_and_writes_nothing() {
        let root = root_holding(r#"{"user_name":"default","day_begin_minutes":180}"#);
        let stamp = seed_a_stretch_today(&root);
        let config = Config::load(&root).expect("fixture config");
        let draft = "Say what this stretch of the screen was about.";
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"  它在看第三季度的表。  "}}]}"#.to_string();

        let trial = try_prompt_with(&config, &usable_form(), PromptName::PeriodSystem, draft, Echo { status: 200, body });
        assert!(trial.ok, "{}", trial.message);
        assert!(trial.segment.contains(&format!("stretch {stamp}")), "which stretch it is about is in the line: {}", trial.segment);
        assert!(trial.chars as usize > draft.chars().count(), "the request carries the screen text too: {} chars", trial.chars);
        assert_eq!(trial.message, "它在看第三季度的表。", "the reply, trimmed, as it came back");

        assert!(prompt_rows(&config).iter().all(|row| !row.overridden), "a trial tests the draft; it does not save it");
        let day = wind_summary::day_of(wind_summary::test_support::at(&stamp), config.day_begin_minutes());
        assert!(wind_summary::read_period(&config, &day).absent() && wind_summary::read_daily(&config, &day).absent(), "and it files no summary");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two ways a trial cannot run, each answered in its own words rather than by sending an empty
    /// request or by a silent no-op row: nothing on the machine to try it on, and a template that is not
    /// a summary template at all. `FailIfCalled` is the assertion that neither opened a socket.
    #[test]
    fn a_trial_says_what_it_cannot_do_instead_of_sending_nothing() {
        let root = root_holding(r#"{"user_name":"default","day_begin_minutes":180}"#);
        std::fs::create_dir_all(root.join("userdata/db")).expect("db dir");
        let config = Config::load(&root).expect("fixture config");

        let empty = try_prompt_with(&config, &usable_form(), PromptName::PeriodSystem, "Anything at all.", FailIfCalled);
        assert!(!empty.ok && empty.chars == 0, "{empty:?}");
        assert!(empty.message.contains("nothing to try it on"), "{}", empty.message);

        seed_a_stretch_today(&root);
        let tags = try_prompt_with(&config, &usable_form(), PromptName::TagsSystem, "Tag it. {max_tags}", FailIfCalled);
        assert!(!tags.ok, "{tags:?}");
        assert!(tags.message.contains("not a summary template"), "{}", tags.message);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `Transport` that answers from a string, so a reflected key can be shown to be scrubbed
    /// without a socket. The real request path is covered over a real loopback listener in
    /// `render_tests`, which is what proves these are the bytes `WinHttp` would put on the wire.
    struct Echo {
        status: u16,
        body: String,
    }

    impl Transport for Echo {
        fn post(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> Result<wind_ai::http::Response, wind_ai::http::TransportError> {
            Ok(wind_ai::http::Response { status: self.status, body: self.body.clone().into_bytes() })
        }
    }

    struct FailIfCalled;

    impl Transport for FailIfCalled {
        fn post(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> Result<wind_ai::http::Response, wind_ai::http::TransportError> {
            Err(wind_ai::http::TransportError("a socket was opened".to_string()))
        }
    }

    /// A transport that answers *and* keeps the URL it was handed, so "the page did not filter the
    /// scheme" is read off the request rather than inferred from the absence of an error.
    #[derive(Clone, Default)]
    struct Sent(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl Sent {
        fn last_url(&self) -> String {
            self.0
                .lock()
                .expect("sent log")
                .last()
                .cloned()
                .expect("nothing was handed to the transport")
        }
    }

    impl Transport for Sent {
        fn post(
            &self,
            url: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> Result<wind_ai::http::Response, wind_ai::http::TransportError> {
            self.0.lock().expect("sent log").push(url.to_string());
            Ok(wind_ai::http::Response {
                status: 200,
                body: br#"{"choices":[{"message":{"content":"ok"}}]}"#.to_vec(),
            })
        }
    }
}
