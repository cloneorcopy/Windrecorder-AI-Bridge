//! The AI half of `config_user.json`, under exactly the names the rest of the product uses.
//!
//! # Why the key names are frozen
//!
//! These strings are a wire format, not identifiers. `onboard_setting.py` and the Streamlit settings
//! page write them, the native `windui` settings page writes them, the MCP bridge reads them, and the
//! Python app the user may still have installed reads them on every launch. Renaming one does not
//! break a build — it makes a configured feature quietly read its default and switch itself off, in
//! front of a user who has already paid for an API key. So `open_ai_base_url` and friends appear here
//! as string literals matched to `config_src/config_default.json` byte for byte, and
//! `tests::every_key_name_matches_the_shipped_default_file` asserts that against the real file rather
//! than against a copy of the list in this module. The two tagger switches are the exception: their
//! names are spelled once in `wind_base::config`, because `windmaint`'s gate and the settings page read
//! those same two keys and a fourth copy of a default is how the three of them disagree.
//!
//! `lang` is read nowhere here, and that is deliberate: the one thing this crate takes from it is the
//! answer language of a request, and that arrives through
//! [`wind_base::prompts::Prompts::language`] beside the templates it fills.
//!
//! # The one value this crate refuses to accept from anywhere else
//!
//! `open_ai_api_key` is read from `userdata/config_user.json` and from nowhere else. No `--api-key`
//! flag, no `OPENAI_API_KEY`, no keychain. Upstream made the same choice; the reasoning is in
//! `crate::error` and it is worth repeating at the definition, because the tempting "convenience"
//! patch later is an environment-variable fallback, and that is the one change that would put the
//! token into the environment block of every child process this app spawns.

use std::path::PathBuf;

use wind_base::config::Config;

use crate::error::{AiError, Faults};

/// The literal `open_ai_api_key` from `config_default.json`.
///
/// Upstream treats "not configured" as `not config.open_ai_api_key`, which is never true on a fresh
/// install because the shipped default is this non-empty string — so a stock install sends every
/// request with a bogus token and reports the resulting 401 as a generic failure. Recognising the
/// placeholder is a bug fix, and it is the difference between "configure your key in Settings" and a
/// mystery. A user who really names their key `your_api_key_here` is treated as unconfigured, which
/// is the correct outcome for them too.
pub const KEY_PLACEHOLDER: &str = "your_api_key_here";

/// The only `ai_api_endpoint_type` this crate can speak.
pub const OPENAI_COMPATIBLE: &str = "OpenAI compatible";

/// A bearer token. Debug prints a placeholder; see `crate::error` for why that is not enough on its
/// own and where the redaction actually happens.
#[derive(Clone)]
pub struct SecretKey(String);

impl SecretKey {
    pub fn new(value: impl Into<String>) -> SecretKey {
        SecretKey(value.into())
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// The single accessor into the token. `pub(crate)` so that no downstream binary can print it by
    /// reaching past the crate that knows the rules.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// A stable identifier for "which key is in use" that is safe in a log line. A truncated FNV-1a,
    /// so it carries no security claim — it exists so two runs can be correlated, nothing more.
    pub fn fingerprint(&self) -> String {
        format!("{:016x}", crate::hashing::fnv1a64(self.0.as_bytes()))
    }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretKey({}, {} bytes)", crate::error::REDACTED, self.0.len())
    }
}

/// Everything the AI features read out of configuration.
#[derive(Debug, Clone)]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    pub api_key: SecretKey,
    /// `ai_api_endpoint_selected` — checked, because a user who picks a different entry in
    /// `ai_api_endpoint_type` needs to be told this build only speaks one dialect, not silently
    /// pointed at an incompatible endpoint.
    pub endpoint_selected: String,
    pub endpoint_types: Vec<String>,
    pub enable_extract_tag: bool,
    pub enable_extract_tag_in_idle: bool,
    /// `ai_extract_tag_wintitle_limit`: rows of the title table handed to the model for a day. The
    /// month mode doubles it, which is what upstream does.
    pub wintitle_limit: usize,
    /// `ai_extract_max_tag_num`: tags kept for a day; a month keeps 1.5x.
    pub max_tag_num: usize,
    pub idle_batch_size: usize,
    /// `ai_extract_tag_filter_words`: substrings removed from the table before it is sent.
    pub filter_words: Vec<String>,
    /// `exclude_words`: window titles dropped entirely. This is a secrecy list, not a display
    /// nicety — it ships with `KeePass`, `1Password`, `Payment method`, `Card information`,
    /// `forget password` in it — so applying it is what keeps a password-manager window title out of
    /// an inference request.
    pub exclude_words: Vec<String>,
    /// `ai_extract_tag_result_dir`, resolved under `userdata`. Upstream's write target.
    pub tags_dir: PathBuf,
    pub day_begin_minutes: i64,
    /// `max_page_result`: the default hit count the search prints, so the CLI agrees with the UI.
    pub max_page_result: usize,
    /// The prompt text that will be sent, resolved from the install.
    ///
    /// Read once here rather than per request: a batch of ninety stretches must not be summarised in two
    /// styles because somebody saved a prompt halfway through, and `prompt_fingerprint` needs one value
    /// to record against the whole run.
    pub prompts: wind_base::prompts::Prompts,
}

impl Settings {
    /// Read the AI keys. Nothing here fails: a missing key is a state to report, not an error to
    /// raise, because `doctor` has to describe an unconfigured install precisely by reading it.
    pub fn read(config: &Config) -> Settings {
        let list = |key: &str| config.str_list(key);
        let positive = |key: &str, default: i64| config.i64_or(key, default).max(0) as usize;
        Settings {
            base_url: config.str_or("open_ai_base_url", "").trim().to_string(),
            model: config.str_or("open_ai_modelname", "").trim().to_string(),
            api_key: SecretKey::new(config.str_or("open_ai_api_key", "")),
            endpoint_selected: config.str_or("ai_api_endpoint_selected", OPENAI_COMPATIBLE),
            endpoint_types: list("ai_api_endpoint_type"),
            // Both tagger switches through `Config`'s accessors, which is where their defaults live now:
            // this reader, `windmaint`'s `ai_gate` and the AI page must not disagree about what an absent
            // key means, and a fourth copy of `false`/`true` here is how they would.
            enable_extract_tag: config.ai_extract_tag_enabled(),
            enable_extract_tag_in_idle: config.ai_extract_tag_allowed_in_idle(),
            wintitle_limit: positive("ai_extract_tag_wintitle_limit", 30),
            max_tag_num: positive("ai_extract_max_tag_num", 15),
            idle_batch_size: positive("ai_extract_tag_in_idle_batch_size", 15),
            filter_words: list("ai_extract_tag_filter_words"),
            exclude_words: list("exclude_words"),
            tags_dir: config.result_dir("ai_extract_tag_result_dir", "result_ai_extract_tag"),
            day_begin_minutes: config.day_begin_minutes(),
            max_page_result: positive("max_page_result", 20).max(1),
            prompts: wind_base::prompts::Prompts::read(config),
        }
    }

    /// A key the user actually typed, rather than the placeholder the installer shipped.
    pub fn key_configured(&self) -> bool {
        !self.api_key.is_empty() && self.api_key.expose().trim() != KEY_PLACEHOLDER
    }

    pub fn base_url_configured(&self) -> bool {
        !self.base_url.is_empty()
    }

    /// Everything needed before a byte can be sent. The message names the exact key to edit, because
    /// "AI is not configured" is not actionable and "set `open_ai_base_url`" is.
    /// A configuration problem is described using only strings this crate wrote and only values from
    /// the shipped default file, so it is reported through an anonymous `Faults`: running it through the
    /// real one would scrub the very placeholder the message tells the user to replace, because until it
    /// is replaced the configured key *is* that string.
    pub fn require_usable(&self, _given: &Faults) -> Result<(), AiError> {
        let faults = &Faults::anonymous();
        if !self.base_url_configured() {
            return Err(faults.unconfigured(
                "`open_ai_base_url` is empty — set it in Settings, or in userdata/config_user.json",
            ));
        }
        if !self.key_configured() {
            return Err(faults.unconfigured(
                "`open_ai_api_key` is unset, or is still the `your_api_key_here` placeholder the installer wrote; replace it in userdata/config_user.json",
            ));
        }
        if self.model.is_empty() {
            return Err(faults.unconfigured("`open_ai_modelname` is empty — name the model to call"));
        }
        if !self.endpoint_types.is_empty() && !self.endpoint_types.iter().any(|t| t == &self.endpoint_selected) {
            return Err(faults.unconfigured(format!(
                "`ai_api_endpoint_selected` = {:?} is not one of `ai_api_endpoint_type` {:#?}",
                self.endpoint_selected, self.endpoint_types
            )));
        }
        if self.endpoint_selected != OPENAI_COMPATIBLE {
            return Err(faults.unconfigured(format!(
                "this build speaks `{OPENAI_COMPATIBLE}` only; `ai_api_endpoint_selected` is {:?}",
                self.endpoint_selected
            )));
        }
        Ok(())
    }

    /// The request target: `{base_url}/chat/completions`.
    ///
    /// Base URLs are pasted from provider documentation with and without a trailing slash, and with a
    /// path that may carry a query. Naive concatenation produces `//chat/completions` (a 404 that
    /// looks like a server fault) or `?x=1/chat/completions` (a broken query). The endpoint is
    /// therefore inserted before any query string, and duplicate slashes collapse.
    pub fn chat_completions_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let (path, query) = match base.split_once('?') {
            Some((head, tail)) => (head, Some(tail)),
            None => (base, None),
        };
        let path = path.trim_end_matches('/');
        match query {
            Some(query) => format!("{path}/chat/completions?{query}"),
            None => format!("{path}/chat/completions"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap()
    }

    /// A synthetic install carrying the *real* shipped defaults, so a test about one key does not have
    /// to re-implement the other fourteen. `Config::load` merges `config_user.json` over
    /// `config_default.json`, so everything asserted below is a statement about the shipped file.
    fn install_with(overrides: &str) -> Config {
        let dir = temp_dir(overrides);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::copy(
            repo_root().join("config_src/config_default.json"),
            dir.join("config_src/config_default.json"),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        std::fs::write(dir.join("userdata/config_user.json"), overrides).unwrap();
        Config::load(&dir).unwrap()
    }

    /// Not a unit test: a compatibility check against the file the Python app ships. A rename on that
    /// side has to fail here rather than turn a user's feature off.
    #[test]
    fn every_key_name_matches_the_shipped_default_file() {
        let raw: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(repo_root().join("config_src/config_default.json")).unwrap(),
        )
        .unwrap();
        for key in [
            "open_ai_base_url",
            "open_ai_api_key",
            "open_ai_modelname",
            "ai_api_endpoint_type",
            "ai_api_endpoint_selected",
            "enable_ai_extract_tag",
            "enable_ai_extract_tag_in_idle",
            "ai_extract_tag_wintitle_limit",
            "ai_extract_max_tag_num",
            "ai_extract_tag_in_idle_batch_size",
            "ai_extract_tag_filter_words",
            "ai_extract_tag_result_dir",
            "exclude_words",
            "day_begin_minutes",
            "max_page_result",
        ] {
            assert!(raw.get(key).is_some(), "{key} is gone from config_default.json");
        }
        assert_eq!(raw["open_ai_api_key"].as_str(), Some(KEY_PLACEHOLDER));
        assert_eq!(raw["ai_api_endpoint_selected"].as_str(), Some(OPENAI_COMPATIBLE));
        assert_eq!(raw["ai_extract_tag_result_dir"].as_str(), Some("result_ai_extract_tag"));
    }

    #[test]
    fn the_shipped_config_reads_back_its_own_defaults() {
        let settings = Settings::read(&Config::load(&repo_root()).unwrap());
        assert_eq!(settings.base_url, "https://api.openai.com/v1");
        assert_eq!(settings.model, "gpt-4o");
        assert_eq!(settings.wintitle_limit, 30);
        assert_eq!(settings.max_tag_num, 15);
        assert_eq!(settings.idle_batch_size, 15);
        assert!(!settings.enable_extract_tag);
        assert!(settings.enable_extract_tag_in_idle);
        assert_eq!(settings.filter_words, vec!["Kim Jong-un".to_string()]);
        assert!(settings.exclude_words.iter().any(|w| w == "KeePass"), "the secrecy list must load");
        assert_eq!(settings.day_begin_minutes, 180);
        assert!(settings.tags_dir.ends_with(Path::new("result_ai_extract_tag")));
        assert_eq!(
            settings.chat_completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    /// The upstream bug, fixed and pinned: a fresh install's key is a truthy placeholder, so "is it
    /// configured?" has to look at the value and not only at emptiness.
    #[test]
    fn the_installer_placeholder_does_not_count_as_a_key() {
        let fresh = Settings::read(&Config::load(&repo_root()).unwrap());
        assert_eq!(fresh.api_key.expose(), KEY_PLACEHOLDER, "the shipped file must still carry it");
        assert!(!fresh.key_configured(), "a placeholder is not a credential");
        let error = fresh.require_usable(&Faults::new(&fresh.api_key)).expect_err("must be refused");
        assert_eq!(error.kind(), crate::error::ErrorKind::Unconfigured);
        assert!(error.to_string().contains(KEY_PLACEHOLDER), "{error}");

        let configured = Settings::read(&install_with(r#"{"open_ai_api_key": "sk-somebody-elses-key"}"#));
        assert!(configured.key_configured());
        configured.require_usable(&Faults::anonymous()).expect("url, key and model are all present");
    }

    /// Each case must be *one* broken value, or the message can only name the highest-priority problem
    /// and the case proves nothing about the value it overrode. That matters for a stock install, whose
    /// `open_ai_api_key` is the `your_api_key_here` placeholder: overriding `open_ai_modelname` alone
    /// leaves a second defect standing, and `require_usable` correctly reports the key first (it is the
    /// one that costs money, and the one the settings page asks for first). So the three cases checked
    /// after the key — model and endpoint — carry a real key, which is what makes "names the key that
    /// needs editing" a statement about the value under test. The `base_url` cases need none, because
    /// `base_url` is checked before the key.
    #[test]
    fn a_blank_or_wrong_value_names_the_key_that_needs_editing() {
        let cases = [
            (r#"{"open_ai_base_url": ""}"#, "`open_ai_base_url`"),
            (r#"{"open_ai_base_url": "   "}"#, "`open_ai_base_url`"),
            (r#"{"open_ai_api_key": ""}"#, "`open_ai_api_key`"),
            (r#"{"open_ai_api_key": "sk-somebody-elses-key", "open_ai_modelname": ""}"#, "`open_ai_modelname`"),
            (
                r#"{"open_ai_api_key": "sk-somebody-elses-key", "ai_api_endpoint_selected": "Anthropic"}"#,
                "OpenAI compatible",
            ),
            (
                r#"{"open_ai_api_key": "sk-somebody-elses-key", "ai_api_endpoint_selected": "Watson"}"#,
                "ai_api_endpoint_type",
            ),
        ];
        for (body, expected) in cases {
            let settings = Settings::read(&install_with(body));
            let error = settings.require_usable(&Faults::anonymous()).expect_err("must refuse: {body}");
            assert!(error.to_string().contains(expected), "{body} -> {error}");
            assert_eq!(error.kind(), crate::error::ErrorKind::Unconfigured, "{body}");
        }
    }

    #[test]
    fn endpoint_joining_survives_every_shape_a_provider_page_offers() {
        let base = |url: &str| {
            Settings::read(&install_with(&format!(r#"{{"open_ai_base_url": "{url}"}}"#))).chat_completions_url()
        };
        assert_eq!(base("https://x.test/v1"), "https://x.test/v1/chat/completions");
        assert_eq!(base("https://x.test/v1/"), "https://x.test/v1/chat/completions");
        assert_eq!(base("https://x.test/v1///"), "https://x.test/v1/chat/completions");
        assert_eq!(base("https://x.test"), "https://x.test/chat/completions");
        assert_eq!(
            base("https://x.test/v1?api-version=2"),
            "https://x.test/v1/chat/completions?api-version=2",
            "a query must stay a query"
        );
        assert_eq!(base("http://127.0.0.1:8123/v1/"), "http://127.0.0.1:8123/v1/chat/completions");
    }

    #[test]
    fn a_negative_limit_from_a_hand_edited_file_cannot_become_a_huge_usize() {
        let settings = Settings::read(&install_with(
            r#"{"ai_extract_tag_wintitle_limit": -5, "max_page_result": 0}"#,
        ));
        assert_eq!(settings.wintitle_limit, 0);
        assert_eq!(settings.max_page_result, 1, "a zero page size would make the CLI print nothing");
    }

    /// A scratch install per distinct override set: `cargo test` runs these in threads inside one
    /// process, so a fixed directory name is a race on `config_user.json`.
    fn temp_dir(marker: &str) -> PathBuf {
        let digest = crate::hashing::fnv1a64(marker.as_bytes());
        let dir = std::env::temp_dir().join(format!("windai-settings-{:x}", digest));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
