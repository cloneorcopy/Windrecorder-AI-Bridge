//! The text the product sends to a model, as a file the user can edit.
//!
//! # Why this exists as its own module
//!
//! A prompt is the only part of the AI surface that is *the user's voice* rather than the program's: it
//! decides what a summary reads like, and it is the one thing a person can improve without a rebuild. So
//! the rule this module enforces is that **what you edit is what runs**. There is no "custom prompt on/off"
//! switch, no template chosen behind a setting, and no second copy of a sentence in Rust that silently
//! wins when the file is absent-but-expected: the effective text is read from disk at the moment the
//! request is built, and the settings screen shows that same text.
//!
//! # Where it lives
//!
//! ```text
//!   userdata/ai_prompts/<name>.txt        the user's own words, when they have any
//!   config_src/ai_prompts/<name>.txt      what this build ships
//! ```
//!
//! The override wins by existing, and `restore` deletes it — which is why "back to default" needs no
//! default text stored anywhere near the user's file. Each template is also embedded with
//! `include_str!`, from *the same file on disk*: the shipped copy and the compiled-in fallback are the
//! same bytes by construction, and a test asserts it, so neither can be edited into disagreeing with the
//! other. The embedded copy is what an install reads when its `config_src` was moved or the file was
//! deleted by a cleanup pass, and a feature whose prompts vanish on a bad upgrade would send an empty
//! request.
//!
//! # What is validated on save, and what is not
//!
//! Two things only, because both are correctness rather than taste:
//!
//!   * an unknown `{token}` is refused. An unrecognised placeholder is sent to the model verbatim, so the
//!     user believes content was inserted where a word was typed;
//!   * a required placeholder is refused. A stretch-summary prompt without `{frames_table}` is a request
//!     that contains no screen text at all, and the model would answer with an invented paragraph that
//!     reads like a memory.
//!
//! There is no length limit and no word list. Suggestions reuse the bridge's own nearest-name rule so a
//! typo of one letter gets "did you mean `{frames_table}`" rather than a shrug.
//!
//! # The one value a slot carries that is not text on disk
//!
//! `{language}` — what language the answer should be written in — is filled from the install's `lang`, the
//! interface language the user already chose, because a Chinese install that is answered in English reads
//! as a broken feature. See [`answer_language_for`] for the table and for why naming a language to a model
//! is not UI copy. It is a *default for the slot*, not a switch beside it: an override that writes its own
//! wording where `{language}` sits is substituted with nothing and runs as written, so "what you edit is
//! what runs" survives the whole of it.
//!
//! # The three templates that predate this module
//!
//! `tags_system`, `tags_user` and `search_system` used to be `format!` calls in
//! `windcap/ai/src/prompt.rs`. They moved here as files, and the move was made byte-first: each shipped
//! file is the *rendering* of the old `format!` with sentinel values in every slot, and the sentinels were
//! then replaced by the placeholder names. `prompts::shipped_text_is_what_the_product_shipped_before` pins
//! the result by digest, so a reworded default fails a test rather than quietly changing what users'
//! installs send.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// The folder inside both `config_src/` and `userdata/`.
pub const DIR: &str = "ai_prompts";

/// Every prompt the product can send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Name {
    PeriodSystem,
    PeriodUser,
    DailySystem,
    DailyUser,
    TagsSystem,
    TagsUser,
    SearchSystem,
}

/// Where the effective text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `userdata/ai_prompts/<name>.txt`.
    UserOverride,
    /// `config_src/ai_prompts/<name>.txt`.
    ShippedFile,
    /// Neither file was there, so the copy compiled into this binary answered.
    Embedded,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::UserOverride => "user",
            Origin::ShippedFile => "shipped",
            Origin::Embedded => "embedded",
        }
    }
}

/// One prompt as it stands right now: its text, its file, and which of the two supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub name: Name,
    pub text: String,
    /// The file that was read, or the one that would be edited.
    pub path: PathBuf,
    pub origin: Origin,
    /// The shipped text, so a settings screen can show the user's words beside the words this build
    /// would have used. An override that has drifted from a *newer* default is visible instead of lost.
    pub shipped: String,
}

impl Prompt {
    pub fn overridden(&self) -> bool {
        self.origin == Origin::UserOverride
    }

    /// The placeholders this template may carry, for the screen that lists them.
    pub fn placeholders(&self) -> &'static [&'static str] {
        self.name.placeholders()
    }
}

impl Name {
    /// The registered set. Seven templates, seven files, one place where the set is written down.
    pub const ALL: [Name; 7] = [
        Name::PeriodSystem,
        Name::PeriodUser,
        Name::DailySystem,
        Name::DailyUser,
        Name::TagsSystem,
        Name::TagsUser,
        Name::SearchSystem,
    ];

    pub fn file(self) -> &'static str {
        match self {
            Name::PeriodSystem => "period_summary_system.txt",
            Name::PeriodUser => "period_summary_user.txt",
            Name::DailySystem => "daily_summary_system.txt",
            Name::DailyUser => "daily_summary_user.txt",
            Name::TagsSystem => "tags_system.txt",
            Name::TagsUser => "tags_user.txt",
            Name::SearchSystem => "search_system.txt",
        }
    }

    /// The name a payload and the settings screen show.
    pub fn label(self) -> &'static str {
        match self {
            Name::PeriodSystem => "period_summary_system",
            Name::PeriodUser => "period_summary_user",
            Name::DailySystem => "daily_summary_system",
            Name::DailyUser => "daily_summary_user",
            Name::TagsSystem => "tags_system",
            Name::TagsUser => "tags_user",
            Name::SearchSystem => "search_system",
        }
    }

    /// The text compiled into this binary, from the same file the installer ships.
    pub fn embedded(self) -> &'static str {
        match self {
            Name::PeriodSystem => include_str!("../../../config_src/ai_prompts/period_summary_system.txt"),
            Name::PeriodUser => include_str!("../../../config_src/ai_prompts/period_summary_user.txt"),
            Name::DailySystem => include_str!("../../../config_src/ai_prompts/daily_summary_system.txt"),
            Name::DailyUser => include_str!("../../../config_src/ai_prompts/daily_summary_user.txt"),
            Name::TagsSystem => include_str!("../../../config_src/ai_prompts/tags_system.txt"),
            Name::TagsUser => include_str!("../../../config_src/ai_prompts/tags_user.txt"),
            Name::SearchSystem => include_str!("../../../config_src/ai_prompts/search_system.txt"),
        }
    }

    /// Every `{token}` this template understands.
    pub fn placeholders(self) -> &'static [&'static str] {
        match self {
            // `{language}` asks which language to answer in. Its value is the install's own `lang`
            // (see [`answer_language`]) and the sentence it sits in is the user's, so both halves stay
            // editable: the shipped text keeps the wording, an override that hardcodes a language keeps that.
            Name::PeriodSystem => &["{language}"],
            Name::PeriodUser => &["{segment}", "{start}", "{end}", "{duration}", "{frames}", "{frames_table}"],
            Name::DailySystem => &["{language}"],
            Name::DailyUser => &[
                "{date}",
                "{day_begin}",
                "{day_end}",
                "{day_rule}",
                "{segments_total}",
                "{segments_summarised}",
                "{period_summaries}",
            ],
            // The three that were already in the product before this module existed. Their slot names are
            // the parameters the Rust functions used to interpolate, so the migration is names, not words.
            // `tags_system` carries `{language}` too: tags are a list a person reads, so asking the table
            // for one language and the tags for another would be two rules in force over one answer.
            Name::TagsSystem => &["{max_tags}", "{language}"],
            Name::TagsUser => &["{table}"],
            // `search_system` deliberately has no such slot. Its answer is a JSON object of keywords, and
            // they are matched *literally* against the text captured off the screen: an instruction to
            // write them in the interface's language would translate a keyword, and a translated keyword
            // matches nothing. That is a data rule, not an answer language, so it stays in prose.
            Name::SearchSystem => &["{earliest}", "{latest}", "{today}"],
        }
    }

    /// Placeholders without which the request would carry no material, or could not answer the question
    /// it is asking.
    pub fn required(self) -> &'static [&'static str] {
        match self {
            Name::PeriodUser => &["{frames_table}"],
            Name::DailyUser => &["{period_summaries}"],
            Name::TagsUser => &["{table}"],
            Name::TagsSystem => &["{max_tags}"],
            Name::SearchSystem => &["{earliest}", "{latest}", "{today}"],
            _ => &[],
        }
    }

    /// The filename, relative to a folder.
    pub fn path_in(self, dir: &Path) -> PathBuf {
        dir.join(DIR).join(self.file())
    }
}

/// The `lang` value an install reads when the key is absent — the shipped default, and the same one every
/// other reader of this key falls back to.
pub const DEFAULT_INTERFACE_LANG: &str = "en";

/// `{language}` for a locale this build ships no phrase for: follow the material rather than guess.
pub const FOLLOW_SCREEN_LANGUAGE: &str = "the language of the screen text itself";

/// The phrase `{language}` carries for one `lang` code.
///
/// This is the whole table, and the only place a language is named for a model. A caller that builds a
/// request interpolates what it is handed and never writes a language word of its own, so adding a locale
/// to `languages.json` and adding its row here are one change rather than four.
///
/// The phrases name the target language *in English*, because they sit inside an English instruction
/// (`One paragraph in {language}, three to six sentences`) and a bare code is not something a model
/// reliably obeys: `sc` is noise, "Chinese (Simplified Han)" is an instruction. They are instructions to a
/// model and not UI copy, so they are deliberately absent from `languages.json` — a Japanese install is
/// asked, in English, to answer in Japanese.
///
/// A `lang` with no row here gets [`FOLLOW_SCREEN_LANGUAGE`], which is what every prompt said before the
/// key was consulted at all: an unknown locale is answered by following the screen, not by guessing English.
pub fn answer_language_for(lang: &str) -> &'static str {
    match lang.trim().to_ascii_lowercase().as_str() {
        "en" => "English",
        "sc" => "Chinese (Simplified Han)",
        "ja" => "Japanese",
        _ => FOLLOW_SCREEN_LANGUAGE,
    }
}

/// The answer language of this install, read from the key the user chose it with.
pub fn answer_language(config: &Config) -> &'static str {
    answer_language_for(&config.str_or("lang", DEFAULT_INTERFACE_LANG))
}

/// The folder a user's own prompts live in: `userdata/ai_prompts`.
pub fn override_dir(config: &Config) -> PathBuf {
    config.userdata_dir().join(DIR)
}

/// The folder this build ships its prompts in: `config_src/ai_prompts`.
pub fn shipped_dir(config: &Config) -> PathBuf {
    config.config_src_dir().join(DIR)
}

/// The file the user's own version of this prompt would live in.
pub fn override_path(config: &Config, name: Name) -> PathBuf {
    override_dir(config).join(name.file())
}

/// The file this install ships.
pub fn shipped_path(config: &Config, name: Name) -> PathBuf {
    shipped_dir(config).join(name.file())
}

/// Read the prompt that would be sent right now.
///
/// Never fails. A missing override is the normal case, and a missing shipped file is answered from the
/// embedded copy rather than by refusing to summarise — but it is *reported* as [`Origin::Embedded`],
/// because an install whose `config_src` went missing should be visible in the settings screen.
pub fn read(config: &Config, name: Name) -> Prompt {
    let ship = shipped_path(config, name);
    let mine = override_path(config, name);
    let shipped = read_file(&ship).unwrap_or_else(|| name.embedded().to_string());
    if let Some(text) = read_file(&mine) {
        return Prompt { name, text, path: mine, origin: Origin::UserOverride, shipped };
    }
    let (text, origin) = if ship.exists() { (shipped.clone(), Origin::ShippedFile) } else { (shipped.clone(), Origin::Embedded) };
    Prompt { name, text, path: ship, origin, shipped }
}

/// All of them, for the settings page and for `windrecorder_prompts_read`.
pub fn read_all(config: &Config) -> Vec<Prompt> {
    Name::ALL.iter().copied().map(|name| read(config, name)).collect()
}

/// The seven effective templates as one value, so a caller that is about to build a request reads the
/// disk once and passes one thing down.
///
/// This is what `windai`'s `Settings` carries. Reading per call would mean every summary in a batch
/// re-opens the files, and a prompt changed *mid-batch* would then produce a day in two styles with no
/// record of which entry used which words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompts {
    pub period_system: String,
    pub period_user: String,
    pub daily_system: String,
    pub daily_user: String,
    pub tags_system: String,
    pub tags_user: String,
    pub search_system: String,
    /// Which of the seven came from the user's own file, for the screen that has to show it.
    pub overridden: Vec<String>,
    /// What `{language}` becomes in a request built from these templates: this install's answer language,
    /// named by [`answer_language`] from the `lang` key.
    ///
    /// Carried on the bundle rather than looked up per request, for the reason the seven texts are: a
    /// batch that answered a day in two languages because the interface setting was re-read halfway
    /// through would be worse than any language choice. It is the value of a slot and not a template, so
    /// it enters no prompt digest — the words on disk stay the fingerprint, and switching `lang` does not
    /// put a finished day back in the queue.
    pub language: &'static str,
}

impl Prompts {
    pub fn read(config: &Config) -> Prompts {
        let take = |name: Name| read(config, name);
        let prompts: Vec<Prompt> = Name::ALL.iter().copied().map(take).collect();
        let overridden = prompts.iter().filter(|p| p.overridden()).map(|p| p.name.label().to_string()).collect();
        let text = |name: Name| {
            prompts.iter().find(|p| p.name == name).map(|p| p.text.clone()).unwrap_or_else(|| name.embedded().to_string())
        };
        Prompts {
            period_system: text(Name::PeriodSystem),
            period_user: text(Name::PeriodUser),
            daily_system: text(Name::DailySystem),
            daily_user: text(Name::DailyUser),
            tags_system: text(Name::TagsSystem),
            tags_user: text(Name::TagsUser),
            search_system: text(Name::SearchSystem),
            overridden,
            language: answer_language(config),
        }
    }

    /// Every template as this build ships it, with no disk read at all.
    ///
    /// For a caller that has an install to read, this is the fallback it should never need; for a unit
    /// test and for `--dry-run` on a machine with no `userdata/`, it is the only honest way to say "the
    /// words are these" without inventing a second default somewhere else.
    ///
    /// With no file to read, `lang` answers as `config_src/config_default.json` ships it —
    /// [`DEFAULT_INTERFACE_LANG`] — so the answer language here is that default's, not a fourth value.
    pub fn embedded() -> Prompts {
        let build = |name: Name| name.embedded().to_string();
        Prompts {
            period_system: build(Name::PeriodSystem),
            period_user: build(Name::PeriodUser),
            daily_system: build(Name::DailySystem),
            daily_user: build(Name::DailyUser),
            tags_system: build(Name::TagsSystem),
            tags_user: build(Name::TagsUser),
            search_system: build(Name::SearchSystem),
            overridden: Vec::new(),
            language: answer_language_for(DEFAULT_INTERFACE_LANG),
        }
    }

    pub fn text(&self, name: Name) -> &str {
        match name {
            Name::PeriodSystem => &self.period_system,
            Name::PeriodUser => &self.period_user,
            Name::DailySystem => &self.daily_system,
            Name::DailyUser => &self.daily_user,
            Name::TagsSystem => &self.tags_system,
            Name::TagsUser => &self.tags_user,
            Name::SearchSystem => &self.search_system,
        }
    }

    /// Replace one template's text in this bundle.
    ///
    /// The prompt editor needs to try *unsaved* words: the thing under test is what is in the box, not
    /// what is on disk, and a trial that read the file would show the user the answer to a question they
    /// did not ask. The answer language is not part of the text being tried, so it stays the install's
    /// own — a trial of a reworded `period_summary_system` answers in the language the window speaks.
    pub fn set(&mut self, name: Name, text: String) {
        match name {
            Name::PeriodSystem => self.period_system = text,
            Name::PeriodUser => self.period_user = text,
            Name::DailySystem => self.daily_system = text,
            Name::DailyUser => self.daily_user = text,
            Name::TagsSystem => self.tags_system = text,
            Name::TagsUser => self.tags_user = text,
            Name::SearchSystem => self.search_system = text,
        }
    }
}

fn read_file(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => Some(text),
        _ => None,
    }
}

/// Write the user's own version, refusing text that could not produce a meaningful request.
pub fn save(config: &Config, name: Name, text: &str) -> Result<PathBuf, String> {
    validate(name, text)?;
    let path = override_path(config, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let staging = path.with_extension("tmp");
    std::fs::write(&staging, text).map_err(|e| format!("{}: {e}", staging.display()))?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    std::fs::rename(&staging, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Delete the user's version, so the shipped words answer again.
pub fn restore(config: &Config, name: Name) -> Result<bool, String> {
    let path = override_path(config, name);
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(true)
}

/// Check text before it becomes what the product sends.
///
/// Returns the sentence a settings screen shows verbatim, so the wording is this module's and not a
/// widget's guess.
pub fn validate(name: Name, text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!(
            "`{}` cannot be empty: an empty instruction asks the model to summarise a stretch of your screen \
             however it likes.",
            name.label()
        ));
    }
    for token in unknown_tokens(name, text) {
        let mut message = format!(
            "`{token}` is not a placeholder of `{}`. Its placeholders are {}.",
            name.label(),
            name.placeholders().join(", ")
        );
        if let Some(some) = nearest(name.placeholders(), &token) {
            message.push_str(&format!(" Did you mean `{some}`?"));
        }
        return Err(message);
    }
    for required in name.required() {
        if !text.contains(required) {
            return Err(format!(
                "`{}` must contain {required}, because without it the request carries no screen content at \
                 all and the model can only invent something.",
                name.label()
            ));
        }
    }
    Ok(())
}

/// The `{...}` words in `text` that this template does not define.
fn unknown_tokens(name: Name, text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != '{' {
            index += 1;
            continue;
        }
        let Some(end) = (index + 1..bytes.len()).find(|&at| bytes[at] == '}') else { break };
        let word: String = bytes[index + 1..end].iter().collect();
        // Only a bare word is a placeholder candidate: `{` inside JSON braces or prose is not one.
        if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            let token = format!("{{{word}}}");
            if !name.placeholders().contains(&token.as_str()) && !out.contains(&token) {
                out.push(token);
            }
        }
        index = end + 1;
    }
    out
}

/// The closest known placeholder, within two edits — the same bound the bridge's argument checker uses,
/// because past that a suggestion is a guess.
fn nearest(known: &[&str], given: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for candidate in known {
        let distance = edits(candidate, given);
        if distance <= 2 && best.as_ref().map_or(true, |(closest, _)| distance < *closest) {
            best = Some((distance, (*candidate).to_string()));
        }
    }
    best.map(|(_, candidate)| candidate)
}

fn edits(a: &str, b: &str) -> usize {
    let target: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=target.len()).collect();
    let mut current = vec![0usize; target.len() + 1];
    for (row, source) in a.chars().enumerate() {
        current[0] = row + 1;
        for (column, want) in target.iter().enumerate() {
            let same = usize::from(source == *want);
            current[column + 1] = (previous[column] + 1 - same).min(current[column] + 1).min(previous[column + 1] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[target.len()]
}

/// Fill a template. Values are given as `{token}` -> text pairs.
///
/// Substitution is single-pass and literal: a value that itself contains `{frames_table}` is never
/// rescanned, so screen text cannot inject a second copy of the table or smuggle a placeholder into
/// somebody else's slot.
pub fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let chars: Vec<char> = template.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '{' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        // Only a `{word}` run of placeholder characters is a slot candidate. Anything else — a JSON
        // example inside the prompt's own text, a stray brace — is emitted one character at a time,
        // because jumping to the matching `}` would swallow the real slots nested in that block. The
        // shipped search prompt has exactly such a block around `{earliest}` and `{latest}`.
        let mut end = index + 1;
        while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        if !(end < chars.len() && chars[end] == '}' && end > index + 1) {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let token: String = chars[index..=end].iter().collect();
        match values.iter().find(|(key, _)| *key == token) {
            Some((_, value)) => out.push_str(value),
            // Left as written: `validate` rejects unknown tokens on save, so this is a template that
            // came from somewhere else, and silently dropping it would lose text nobody asked to lose.
            None => out.push_str(&token),
        }
        index = end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn install(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("windcap-prompts-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("config_src/ai_prompts")).expect("shipped dir");
        fs::create_dir_all(root.join("userdata")).expect("userdata dir");
        // The fixture ships the real files, so a test about precedence is a test about this install.
        for name in Name::ALL {
            fs::write(name.path_in(&root.join("config_src")), name.embedded()).expect("ship");
        }
        fs::write(root.join("config_src/config_default.json"), b"{}").expect("defaults");
        fs::write(root.join("userdata/config_user.json"), b"{}").expect("user");
        root
    }

    fn config_at(root: &Path) -> Config {
        Config::load(root).expect("config")
    }

    #[test]
    fn the_shipped_file_and_the_compiled_copy_are_the_same_bytes() {
        // The two could otherwise drift apart silently: one is what an installer ships, the other is what
        // answers when the installer's folder has been moved.
        let root = install("identity");
        let config = config_at(&root);
        for name in Name::ALL {
            let shipped = fs::read_to_string(shipped_path(&config, name)).expect("shipped file");
            assert_eq!(shipped, name.embedded(), "{} differs from its compiled copy", name.label());
            let read = read(&config, name);
            assert_eq!(read.origin, Origin::ShippedFile, "{} should answer from the file", name.label());
            assert_eq!(read.text, shipped);
            assert!(!read.overridden());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_moved_config_src_falls_back_to_the_embedded_text_and_says_so() {
        let root = install("embedded");
        fs::remove_dir_all(root.join("config_src/ai_prompts")).expect("removed");
        let config = config_at(&root);
        for name in Name::ALL {
            let read = read(&config, name);
            assert_eq!(read.origin, Origin::Embedded, "{} fell back", name.label());
            assert_eq!(read.text, name.embedded());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_users_own_words_win_and_the_shipped_ones_stay_visible() {
        let root = install("override");
        let config = config_at(&root);
        let mine = "Say what they were doing. {frames_table}\n";
        save(&config, Name::PeriodUser, mine).expect("saved");
        let prompt = read(&config, Name::PeriodUser);
        assert_eq!(prompt.origin, Origin::UserOverride);
        assert_eq!(prompt.text, mine);
        assert_eq!(prompt.shipped, Name::PeriodUser.embedded(), "the settings page can show both");
        assert!(prompt.path.ends_with(Path::new("userdata").join(DIR).join("period_summary_user.txt")), "{:?}", prompt.path);
        // Restoring deletes the file rather than writing the default over it, so a later upgrade's new
        // words arrive instead of a frozen copy of the old ones.
        assert!(restore(&config, Name::PeriodUser).expect("restored"));
        assert!(!override_path(&config, Name::PeriodUser).exists(), "no override is left behind");
        assert!(!restore(&config, Name::PeriodUser).expect("nothing to restore"));
        assert_eq!(read(&config, Name::PeriodUser).origin, Origin::ShippedFile);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_unknown_placeholder_is_refused_and_names_the_one_they_meant() {
        let root = install("validate-unknown");
        let config = config_at(&root);
        let error = validate(Name::PeriodUser, "text {frame_table}").expect_err("one letter short");
        assert!(error.contains("Did you mean `{frames_table}`"), "{error}");
        assert!(validate(Name::PeriodUser, "{frames_table} {nonsense}").is_err(), "a far-off word is refused without a guess");
        assert!(save(&config, Name::PeriodUser, "{nonsense}").is_err(), "and it never reaches the disk");
        assert!(!override_path(&config, Name::PeriodUser).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_template_without_its_content_placeholder_is_refused_because_it_could_only_invent() {
        let root = install("validate-required");
        let error = validate(Name::PeriodUser, "Summarise my day please.").expect_err("no table, no material");
        assert!(error.contains("{frames_table}"), "{error}");
        assert!(error.contains("invent"), "{error}");
        assert!(validate(Name::DailyUser, "no list here").is_err());
        assert!(validate(Name::DailyUser, "{period_summaries}").is_ok(), "and one with it is accepted");
        assert!(validate(Name::PeriodSystem, "anything without a placeholder").is_ok(), "a system turn carries no material");
        assert!(validate(Name::PeriodSystem, "   ").is_err(), "but empty is never fine");
        let _ = fs::remove_dir_all(root);
    }

    /// The exact text that will be sent, from the exact file on disk.
    #[test]
    fn rendering_fills_every_slot_the_producer_knows_about() {
        let root = install("render");
        let config = config_at(&root);
        let values = [
            ("{segment}", "2026-09-27_15-47-17"),
            ("{start}", "2026-09-27 15:47:17"),
            ("{end}", "2026-09-27 15:50:07"),
            ("{duration}", "2m50s"),
            ("{frames}", "30"),
            ("{frames_table}", "[15:47:17] window: Qoder"),
        ];
        let filled = render(&read(&config, Name::PeriodUser).text, &values);
        assert!(filled.starts_with("Segment 2026-09-27_15-47-17: 2026-09-27 15:47:17 to 2026-09-27 15:50:07 (2m50s), 30 frames"), "{filled}");
        assert!(filled.ends_with("[15:47:17] window: Qoder\n"), "the table goes in last: {filled:?}");
        assert!(!filled.contains('{'), "every slot was filled: {filled}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_value_cannot_inject_a_second_round_of_substitution() {
        // Screen text is untrusted input in the literal sense: it is typed by whatever was on the screen.
        let table = "[15:47:17] window: {frames_table} and {segment}";
        let filled = render("Segment {segment}: {frames_table}", &[("{segment}", "2026-09-27_15-47-17"), ("{frames_table}", table)]);
        assert_eq!(filled, "Segment 2026-09-27_15-47-17: [15:47:17] window: {frames_table} and {segment}");
    }

    #[test]
    fn an_unmatched_brace_survives_rather_than_eating_the_rest() {
        assert_eq!(render("ends with {", &[]), "ends with {");
        assert_eq!(render("json {\"a\": 1}", &[]), "json {\"a\": 1}", "braces that are not placeholders are left alone");
        assert_eq!(render("{known} {unknown}", &[("{known}", "k")]), "k {unknown}");
    }

    #[test]
    fn unknown_tokens_ignores_prose_braces_and_catches_a_near_miss() {
        assert!(unknown_tokens(Name::PeriodSystem, "a JSON object like {\"key\": 1}").is_empty());
        assert_eq!(unknown_tokens(Name::PeriodSystem, "{langauge}"), vec!["{langauge}".to_string()]);
        assert_eq!(unknown_tokens(Name::PeriodUser, "{nonsense} {nonsense}"), vec!["{nonsense}".to_string()], "one word reported once, however many times it appears");
        assert!(unknown_tokens(Name::PeriodUser, "{frames_table} {duration}").is_empty());
    }

    #[test]
    fn read_all_covers_the_whole_registered_set_in_order() {
        let root = install("all");
        let all = read_all(&config_at(&root));
        assert_eq!(all.len(), Name::ALL.len());
        assert_eq!(
            all.iter().map(|p| p.name.label()).collect::<Vec<_>>(),
            vec![
                "period_summary_system",
                "period_summary_user",
                "daily_summary_system",
                "daily_summary_user",
                "tags_system",
                "tags_user",
                "search_system"
            ]
        );
        assert!(all.iter().all(|p| !p.text.trim().is_empty()), "none of them is empty on a fresh install");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_shipped_prompts_ask_for_the_thing_the_feature_promises() {
        // Cheap, and it catches the edit that would otherwise only show up as bad summaries: the defaults
        // must name the handles worth keeping and forbid inventing, in prose a user can rewrite.
        let period = Name::PeriodSystem.embedded();
        assert!(period.contains("what was being done"), "{period}");
        assert!(period.contains("no invented"), "{period}");
        assert!(period.contains("{language}"), "the answer language is a slot the producer can fill: {period}");
        let daily = Name::DailySystem.embedded();
        assert!(daily.contains("threads"), "the day's value is seeing one task run through it: {daily}");
        assert!(daily.contains("not summarised"), "and it must be told to admit a gap: {daily}");
        // The tags read as a list of words beside the paragraph, so they are asked for in the same slot —
        // one rule about the answer's language, in one place, rather than two.
        let tags = Name::TagsSystem.embedded();
        assert!(tags.contains("in {language},"), "the tag row carries the slot too: {tags}");
        assert!(!tags.contains("in the language of the table"), "and not the older rule beside it: {tags}");
        // The search template is the deliberate exception: its answer is keywords matched against screen
        // text, so a language slot there would be an instruction to translate them.
        let search = Name::SearchSystem.embedded();
        assert!(!search.contains("{language}"), "keywords stay literal: {search}");
        assert!(search.contains("SAME language as the sentence"), "and say so in prose: {search}");
        assert!(!period.contains('\u{201c}'), "the shipped text stays plain ASCII quotes");
    }

    /// The `{language}` slot's value, for every `lang` this install can be set to.
    ///
    /// One table, checked from both ends: the code it is keyed by, and the phrase a model is told. The
    /// phrases are pinned as literals on purpose — they are the promise, and a reworded one changes what
    /// leaves the machine.
    #[test]
    fn the_answer_language_comes_from_the_interface_key_and_names_the_language_in_words() {
        assert_eq!(answer_language_for("en"), "English");
        assert_eq!(answer_language_for("sc"), "Chinese (Simplified Han)");
        assert_eq!(answer_language_for("ja"), "Japanese");
        // Whitespace and case are the shapes a hand-edited file takes; neither is a second locale.
        assert_eq!(answer_language_for(" SC "), "Chinese (Simplified Han)");
        // A locale this build has no phrase for is answered by following the screen, which is what the
        // product did before `lang` was consulted at all — not by guessing one language for everyone.
        assert_eq!(answer_language_for("ko"), FOLLOW_SCREEN_LANGUAGE);
        assert_eq!(answer_language_for(""), FOLLOW_SCREEN_LANGUAGE);
        // An absent key answers as `config_default.json` ships it, which is `en`.
        let root = install("lang");
        assert_eq!(answer_language(&config_at(&root)), answer_language_for(DEFAULT_INTERFACE_LANG));
        assert_eq!(answer_language(&config_at(&root)), "English", "the shipped default is the English case");
        let _ = fs::remove_dir_all(root);
    }

    /// The same value, carried by the bundle every request is built from, so a batch cannot answer a day
    /// in two languages because the key was re-read halfway through it.
    #[test]
    fn the_prompts_bundle_carries_the_installs_own_answer_language() {
        let root = install("lang-bundle");
        fs::write(root.join("userdata/config_user.json"), br#"{"lang": "sc"}"#).expect("user config");
        let config = config_at(&root);
        assert_eq!(Prompts::read(&config).language, "Chinese (Simplified Han)");
        assert_eq!(Name::PeriodSystem.placeholders(), &["{language}"], "and it fills a slot the user can drop");

        fs::write(root.join("userdata/config_user.json"), br#"{"lang": "ja"}"#).expect("user config");
        assert_eq!(Prompts::read(&config_at(&root)).language, "Japanese");

        // What you edit is what runs: the bundle carries a value, and only a template that asks for it by
        // name receives one. An override that names its own language is sent as written.
        fs::write(root.join("userdata/config_user.json"), br#"{"lang": "en"}"#).expect("user config");
        let config = config_at(&root);
        let prompts = Prompts::read(&config);
        let theirs = "One paragraph in 简体中文, three to six sentences, and nothing else.\n";
        save(&config, Name::PeriodSystem, theirs).expect("saved");
        let filled = render(&read(&config, Name::PeriodSystem).text, &[("{language}", prompts.language)]);
        assert_eq!(filled, theirs, "an override with no slot is not touched by the substitution");
        assert!(validate(Name::PeriodSystem, theirs).is_ok(), "and `validate` never complains about it");
        let _ = fs::remove_dir_all(root);
    }
}
