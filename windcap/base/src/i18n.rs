//! The product's copy, read from the same `languages.json` the Python app read — now shared by the
//! tray (`windsvc`) and the native window (`windui`).
//!
//! Reproducing `utils.get_text` rather than embedding English strings: the file is the product's
//! copy, it is translated by contributors who are not reading Rust, and the tray's labels are the
//! most-visible text in the install. The lookup rules are upstream's too — the configured locale
//! first, then `en`, then a message that says which key is missing.
//!
//! This module used to live in `supervisor`. It moved here for the reason every shared thing moves
//! up a level: `windui` needs the *same* catalog, and a second implementation is how one binary ends
//! up silently rendering keys while the other works. The two failure modes the tray depends on — a
//! missing key naming itself, an unreadable file reporting why — are load-bearing and are tested
//! below, so neither binary can quietly lose them.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The key for a string nobody translated. Exactly what the Python app shows, so a missing
/// translation looks the same in every implementation.
pub fn missing(key: &str) -> String {
    format!("({key}) not found in i18n, please feedback to contributors.")
}

#[derive(Debug, Clone)]
pub struct Catalog {
    locales: Value,
    lang: String,
    /// Set when the file could not be read at all, so `doctor` can say why every label is a
    /// `(key) not found` string instead of pretending the install is only missing a translation.
    pub read_error: Option<String>,
}

impl Catalog {
    /// `root` is the install directory; the catalog is read from `config_src` next to the defaults.
    pub fn load(root: &Path, lang: &str) -> Catalog {
        let path = languages_path(root);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(locales) => Catalog { locales, lang: lang.to_string(), read_error: None },
                Err(e) => empty(Some(format!("{}: {e}", path.display()))),
            },
            Err(e) => empty(Some(format!("{}: {e}", path.display()))),
        }
    }

    /// The text for `key`, following `utils.get_text`: the configured locale, then `en`, then a
    /// message naming the key.
    ///
    /// One deliberate difference — a locale the file does not contain (`config.lang` is a free
    /// string in the user's config) falls back to `en` here instead of raising a `KeyError` the way
    /// `d_lang[config.lang]` does. A tray that will not start because someone typed `zh` into a
    /// settings field is not a better tray.
    pub fn text(&self, key: &str) -> String {
        for locale in [self.lang.as_str(), "en"] {
            if let Some(Value::String(text)) = self.locales.get(locale).and_then(|table| table.get(key)) {
                return text.clone();
            }
        }
        missing(key)
    }

    /// The text for `key` with `{name}` placeholders filled, which is how `{address_port}` and
    /// `{version}` reach the labels.
    pub fn formatted(&self, key: &str, args: &[(&str, &str)]) -> String {
        fill(&self.text(key), args)
    }

    /// The text for `key`, falling back to a string the *binary* carries instead of the missing-key
    /// marker.
    ///
    /// For a label a control must always show: a settings field whose translation went missing still has
    /// to name itself, and `(field_label) not found in i18n` in place of a widget's whole label is a worse
    /// answer than the English the Rust code already knows.
    pub fn text_or(&self, key: &str, fallback: &str) -> String {
        let text = self.text(key);
        if text == missing(key) {
            fallback.to_string()
        } else {
            text
        }
    }

    /// Which locales the catalog actually holds, with the name each one is called by in its own language.
    ///
    /// The picker for the interface language has to offer what a user can read, and only the file knows
    /// that: `lang_map` is the locale's own spelling of its name, which is why a Chinese menu says
    /// 简体中文 rather than "sc". A file without that row falls back to its own top-level keys, so the
    /// list is never shorter than the languages that exist.
    pub fn locales(root: &Path) -> Vec<(String, String)> {
        let path = languages_path(root);
        let Ok(bytes) = std::fs::read(&path) else { return vec![] };
        let Ok(Value::Object(table)) = serde_json::from_slice::<Value>(&bytes) else { return vec![] };
        let named: Vec<(String, String)> = match table.get("lang_map").and_then(Value::as_object) {
            Some(map) => map
                .iter()
                .map(|(code, name)| (code.clone(), name.as_str().unwrap_or(code.as_str()).to_string()))
                .collect(),
            None => table
                .keys()
                .filter(|code| code.as_str() != "lang_map")
                .map(|code| (code.clone(), code.clone()))
                .collect(),
        };
        // A locale `lang_map` names but the file does not translate is not an option: it would show its
        // own name in the picker and then fall back to English everywhere else.
        named
            .into_iter()
            .filter(|(code, _)| table.get(code).map(Value::is_object).unwrap_or(false))
            .collect()
    }

    /// [`Self::formatted`] with the same fallback rule as [`Self::text_or`]: a sentence the catalog has no
    /// row for is filled from the template the binary carries, so a dynamic note can never render as
    /// `(key) not found …` where a warning belongs.
    pub fn formatted_or(&self, key: &str, args: &[(&str, &str)], fallback: &str) -> String {
        fill(&self.text_or(key, fallback), args)
    }

    /// The locale this catalog is answering in. A window that is about to re-read the config compares
    /// against this to decide whether it has to load a different set of strings.
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// Is the file itself readable? Reported by `doctor` rather than acted upon.
    pub fn loaded(&self) -> bool {
        self.read_error.is_none()
    }
}

fn empty(read_error: Option<String>) -> Catalog {
    Catalog { locales: Value::Null, lang: "en".to_string(), read_error }
}

/// Where the strings live: `languages.json` inside this install's settings directory.
///
/// Resolved by [`crate::install`], so a standalone payload reads `config_src/` and an overlay install
/// that has not moved its data up still reads `windrecorder/config_src/` — and a binary cannot end up
/// translating its menu with a catalog from a different install than the one whose config it just
/// loaded.
pub fn languages_path(root: &Path) -> PathBuf {
    crate::install::config_src_file(root, "languages.json")
}

/// Substitute Python's `str.format` for the only thing these strings use it for: named fields with no
/// format spec. Anything that is not `{name}` in the table survives untouched.
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        match rest.find('}') {
            Some(close) => {
                let name = &rest[..close];
                match args.iter().find(|(key, _)| *key == name) {
                    Some((_, value)) => out.push_str(value),
                    // An unknown field is left as written: a half-translated string that still
                    // names its own placeholder is diagnosable, one that swallowed it is not.
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &rest[close + 1..];
            }
            None => {
                out.push('{');
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(Path::to_path_buf).unwrap()
    }

    fn catalog(json: &str) -> Catalog {
        Catalog { locales: serde_json::from_str(json).unwrap(), lang: "en".to_string(), read_error: None }
    }

    #[test]
    fn the_shipped_catalog_answers_the_tray_keys() {
        let c = Catalog::load(&repo_root(), "en");
        assert!(c.loaded(), "{:?}", c.read_error);
        assert_eq!(c.text("tray_exit"), "❌ Exit");
        assert_eq!(c.text("tray_record_start"), "▶️ Start Recording");
        assert_eq!(c.text("tray_record_stop"), "⏸️ Pause Recording");
    }

    #[test]
    fn a_locale_without_the_key_falls_back_to_english() {
        let c = catalog(r#"{"en": {"a": "A"}, "sc": {"b": "B"}}"#);
        let mut c = c;
        c.lang = "sc".into();
        assert_eq!(c.text("a"), "A");
        assert_eq!(c.text("b"), "B");
    }

    #[test]
    fn an_unknown_locale_still_reads_english_instead_of_failing() {
        let mut c = catalog(r#"{"en": {"a": "A"}}"#);
        c.lang = "zh".into();
        assert_eq!(c.text("a"), "A");
    }

    #[test]
    fn a_key_no_one_translated_names_itself() {
        let c = catalog(r#"{"en": {}}"#);
        assert_eq!(c.text("tray_nope"), "(tray_nope) not found in i18n, please feedback to contributors.");
    }

    #[test]
    fn a_missing_file_degrades_to_the_fallback_and_says_so() {
        let c = Catalog::load(Path::new("Z:/definitely-not-here"), "en");
        assert!(!c.loaded());
        assert!(c.read_error.clone().unwrap().contains("languages.json"));
        assert!(c.text("tray_exit").starts_with("(tray_exit)"));
    }

    #[test]
    fn placeholders_are_filled_and_everything_else_survives() {
        assert_eq!(fill("Browser {address_port} to access webui", &[("address_port", "http://127.0.0.1:8501")]),
                   "Browser http://127.0.0.1:8501 to access webui");
        assert_eq!(fill("      LAN address: {address_port}", &[("address_port", "")]), "      LAN address: ");
        assert_eq!(fill("Version {version}", &[]), "Version {version}");
        assert_eq!(fill("no fields", &[("version", "1")]), "no fields");
        assert_eq!(fill("brace at the end {", &[("version", "1")]), "brace at the end {");
    }

    /// The two release strings the menu still builds, asserted against the shipped catalog: the
    /// version row interpolates the binary's own number, and the changelog row is a plain label.
    /// (The catalog still carries the offer strings of the updater that was deleted; `menu.rs`'s
    /// own tests are what pin that no row can reach them.)
    #[test]
    fn the_real_labels_interpolate_their_fields() {
        let c = Catalog::load(&repo_root(), "en");
        assert_eq!(c.formatted("tray_version_info", &[("version", "0.1.0 (release)")]), "🚀 Version 0.1.0 (release)");
        assert_eq!(c.text("tray_updatelog"), "🚀 See what's new");
    }

    /// The tray's version row must not claim a thing the product can no longer verify. The remote
    /// version poll was removed, so `en` reads "Version {version}" and every locale has to say the
    /// same neutral thing — `sc`/`ja` once claimed "已是最新版" / "現在は最新バージョン" ("already the
    /// latest version"), which this install cannot know and which this test now refuses.
    #[test]
    fn the_version_row_says_nothing_about_being_up_to_date_in_any_locale() {
        for lang in ["en", "sc", "ja"] {
            let text = Catalog::load(&repo_root(), lang).formatted("tray_version_info", &[("version", "0.1.0")]);
            assert!(!text.contains("最新"), "{lang}: {text} claims an unverifiable latest");
            assert!(!text.to_lowercase().contains("latest"), "{lang}: {text} claims an unverifiable latest");
            assert!(text.contains("0.1.0"), "{lang}: {text} must still interpolate the version");
        }
    }

    /// The dead update-offer strings of the deleted Python updater were dropped from the shipped
    /// catalog (one of them, `set_update_new`, told the user to open a `install_update.bat` that no
    /// longer exists). Nothing in this workspace reads them, and a catalog is not a place to keep a
    /// lie in case someone wants it — so they stay out, asserted here. `tray_native_no_address` joined
    /// them for the same reason: the tray's default row now raises the window instead of announcing
    /// that there is no address to raise in a browser, so the sentence describes a menu that no longer
    /// exists.
    #[test]
    fn the_catalog_holds_no_dead_updater_offers() {
        let root = repo_root();
        let raw: Value = serde_json::from_slice(&std::fs::read(languages_path(&root)).unwrap()).unwrap();
        for lang in ["en", "sc", "ja"] {
            for dead in ["set_update_new", "set_update_checking", "set_update_latest", "set_update_fail", "set_update_changelog", "set_toggle_use_native_core", "set_help_use_native_core", "tray_native_no_address"] {
                assert!(raw.get(lang).and_then(|table| table.get(dead)).is_none(), "{lang} still carries the dead key {dead}");
            }
        }
    }

    /// Where the web window writes down the keys it is going to ask for.
    fn web_copy_ts(root: &Path) -> PathBuf {
        root.join("windcap").join("winduiweb").join("src").join("copy.ts")
    }

    /// The `COPY_KEYS` array of `windcap/winduiweb/src/copy.ts`, lifted out of the file text.
    ///
    /// Parsed rather than mirrored: a second list to keep in step is the thing this test exists to
    /// catch, and an `include!`-style import would mean teaching TypeScript to be Rust. The slice is
    /// cut at the array's own brackets so the rest of the file — the fallback map, the doc comments,
    /// the `ui_strings` call — cannot bleed in.
    fn web_copy_keys() -> Vec<String> {
        let text = std::fs::read_to_string(web_copy_ts(&repo_root()))
            .expect("windcap/winduiweb/src/copy.ts is readable — the web window's own key list lives there");
        let declared = text.find("export const COPY_KEYS").expect("`copy.ts` declares `export const COPY_KEYS`");
        let open = declared + text[declared..].find('[').expect("the `COPY_KEYS` declaration opens an array");
        let close = open + text[open..].find("];").expect("the `COPY_KEYS` array is closed");
        let body: Vec<char> = text[open..close].chars().collect();
        // A comment-per-line file, so the comments are walked over rather than harvested: one of them
        // quotes an English phrase in double quotes, and a "key" with spaces in it would otherwise be
        // reported as a missing catalog row no locale can ever ship.
        let mut keys = Vec::new();
        let mut current = String::new();
        let mut inside = false;
        let mut at = 0;
        while at < body.len() {
            if !inside && body[at] == '/' && body.get(at + 1) == Some(&'/') {
                while at < body.len() && body[at] != '\n' {
                    at += 1;
                }
                continue;
            }
            if body[at] == '"' {
                if inside {
                    keys.push(std::mem::take(&mut current));
                }
                inside = !inside;
            } else if inside {
                current.push(body[at]);
            }
            at += 1;
        }
        assert!(!inside, "an unterminated string literal in the `COPY_KEYS` array");
        keys
    }

    /// The web window and the shipped catalog are one system with two halves, and the halves fail
    /// asymmetrically, which is why this is a gate rather than a note in a comment:
    ///
    /// * `src/App.tsx` seeds its copy map with `{ ...FALLBACK, ...resolved }`, where `resolved` comes
    ///   from `commands::ui_strings` — and that command calls [`Catalog::text`] with **no fallback**,
    ///   so a key the catalog does not hold comes back as the `(key) not found …` marker and
    ///   *overwrites* the English the TypeScript fallback would have shown. A key added to `copy.ts`
    ///   but not to `languages.json` is therefore worse than useless: it paints the marker over the
    ///   copy that would otherwise have rendered, in every locale including English.
    /// * The mirror case is quieter but just as real: `windui_web_*` rows the catalog carries that
    ///   `COPY_KEYS` no longer lists are rows nothing can ask for, and they will drift and rot.
    ///
    /// So both directions are asserted here, per locale, against the file the binaries actually read.
    #[test]
    fn the_web_window_and_the_catalog_hold_exactly_the_same_keys() {
        let root = repo_root();
        // Read through the loader first, so an unreadable or malformed file fails as the missing-file
        // case the tray already reports rather than as a confusing key-by-key panic.
        let loader = Catalog::load(&root, "en");
        assert!(loader.loaded(), "the shipped catalog did not load: {:?}", loader.read_error);

        let raw = std::fs::read(languages_path(&root)).expect("the shipped catalog");
        let locales: Value = serde_json::from_slice(&raw).expect("the shipped catalog is valid JSON");

        let keys = web_copy_keys();
        // A parser that quietly found nothing would make every assertion below true and the gate
        // meaningless, so the list is pinned to a size the window really is at before it is walked.
        assert!(keys.len() > 100, "parsed only {} keys out of `copy.ts` — the parser is wrong, not the file", keys.len());
        assert_eq!(keys.len(), keys.iter().collect::<std::collections::BTreeSet<_>>().len(), "`COPY_KEYS` lists a key twice");

        // Direction one: every key the window asks for is shipped, in every locale it ships, in words.
        for locale in ["en", "sc", "ja"] {
            let table = locales.get(locale).and_then(Value::as_object).unwrap_or_else(|| panic!("{locale} is not a table in the shipped catalog"));
            for key in &keys {
                let text = table
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("the {locale} catalog has no row for {key}, so `ui_strings` answers it with the not-found marker and the web window paints that over its own English"));
                assert!(!text.trim().is_empty(), "the {locale} catalog ships {key} as nothing at all");
            }
        }

        // Direction two: nothing under `windui_web_` survives in the catalog that the window does not
        // ask for — in any locale, because a row only one locale carries is a row half the installs
        // read and the other half cannot.
        let asked = keys.iter().cloned().collect::<std::collections::BTreeSet<_>>();
        for locale in ["en", "sc", "ja"] {
            let table = locales.get(locale).and_then(Value::as_object).expect("a locale this test just walked");
            for key in table.keys() {
                if key.starts_with("windui_web_") {
                    assert!(asked.contains(key), "{locale} carries {key}, which is no longer in `COPY_KEYS` — nothing can ask the catalog for it, so it is a dead row");
                }
            }
        }
    }

    /// Every key `winduiweb/src/copy.ts` names in `COPY_KEYS` must exist with non-empty text in each
    /// of `en`, `sc` and `ja` in the real shipped `config_src/languages.json` — not a fixture.
    ///
    /// This is the one-sided-key guard. The web window resolves every `COPY_KEYS` entry through `t()`,
    /// and a locale that lacks one renders `(key) not found in i18n, please feedback to
    /// contributors.` to a real user — the failure mode `copy.ts`'s own header says it exists to make
    /// loud, previously only asserted by hand with `node -e` scripts, which is how 72 `ja` rows still
    /// slipped through. Two checks in this repo's history passed by verifying an empty set, so this
    /// one refuses to run vacuously: parsing zero keys is itself a failure, and both ends of the real
    /// array (`windui_tab_search` first, `day.summary.fallback_missing` last) must survive the parse.
    ///
    /// `COPY_KEYS` is read as text — the region between `COPY_KEYS` and the first `];` after it —
    /// because this crate has no TypeScript parser and must not grow one. Only lines whose trimmed
    /// text begins with a `"` count as entries, so the array's comment lines (one of which quotes the
    /// English phrase "where did this count come from") can never be mistaken for keys.
    ///
    /// `WIND_I18N_TEST_CATALOG` overrides the catalog path and nothing else; it exists so the red run
    /// can point this test at a one-key-deleted copy of the shipped file without touching that file.
    #[test]
    fn every_copy_ts_key_has_text_in_every_shipped_locale() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"));
        let copy_path = base.join("../winduiweb/src/copy.ts");
        let catalog_path = match std::env::var("WIND_I18N_TEST_CATALOG") {
            Ok(overridden) if !overridden.is_empty() => PathBuf::from(overridden),
            _ => base.join("../../config_src/languages.json"),
        };

        let copy_src = std::fs::read_to_string(&copy_path).unwrap_or_else(|e| panic!("{}: {e}", copy_path.display()));
        let opened = copy_src.find("COPY_KEYS").expect("copy.ts must name COPY_KEYS at all");
        let region = &copy_src[opened..opened + copy_src[opened..].find("];").expect("the COPY_KEYS array must close with `];`")];
        let keys: Vec<&str> = region
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('"'))
            .filter_map(|line| line[1..].find('"').map(|close| &line[1..=close]))
            .filter(|key| !key.is_empty())
            .collect();

        assert!(!keys.is_empty(), "parsed zero keys out of COPY_KEYS — the text parser stopped matching copy.ts and this test would assert on nothing");
        assert!(keys.contains(&"windui_tab_search"), "COPY_KEYS parse lost the array's first real key (windui_tab_search)");
        assert!(keys.contains(&"day.summary.fallback_missing"), "COPY_KEYS parse lost the array's last real key (day.summary.fallback_missing)");

        let bytes = std::fs::read(&catalog_path).unwrap_or_else(|e| panic!("{}: {e}", catalog_path.display()));
        let raw: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", catalog_path.display()));
        let mut offenders: Vec<String> = Vec::new();
        for key in &keys {
            for locale in ["en", "sc", "ja"] {
                match raw.get(locale).and_then(|table| table.get(*key)).and_then(Value::as_str) {
                    Some(text) if !text.trim().is_empty() => {}
                    found => offenders.push(format!("{locale}:{key} -> {found:?}")),
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "{} COPY_KEYS rows checked against {}; missing or empty: {offenders:?}",
            keys.len() * 3,
            catalog_path.display()
        );
    }
}
