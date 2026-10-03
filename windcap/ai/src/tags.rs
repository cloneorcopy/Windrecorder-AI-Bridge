//! Monthly activity tags: the window titles of a month, summarised into what it was about.
//!
//! This is `enable_ai_extract_tag` / `generate_day_or_month_tags_lst(…, type="month")`, ported. The
//! shape is upstream's and so are the four configuration keys that shape it:
//! `ai_extract_tag_wintitle_limit` (how many titles the model is shown — doubled for a month, as
//! upstream doubles it), `ai_extract_max_tag_num` (how many tags are kept — 1.5x for a month),
//! `ai_extract_tag_filter_words` (substrings cut out of the table before it is sent) and
//! `ai_extract_tag_result_dir` (where the answer is written).
//!
//! # Where it is written, and what is added alongside
//!
//! `userdata/result_ai_extract_tag/{YYYY}.json`, mapping `"YYYY-MM"` to a list of tag strings — the
//! file and key format `get_cache_data_by_date` already reads, so the Streamlit page and the MCP bridge
//! keep working without knowing this binary exists.
//!
//! The one addition is a sibling `{YYYY}.hash.json` holding the content fingerprint each entry was
//! derived from. Upstream's cache is keyed by month alone, so a month whose tags were generated in
//! week one keeps those tags forever even as three more weeks of history arrive. Keying on
//! (month, title-set hash) fixes that without touching the tags file's shape: a re-run over an
//! unchanged month costs nothing, and a changed month costs one request. The hash lives in its own file
//! precisely because upstream renders *every string in the list* as a visible tag pill — a metadata
//! field smuggled into the tag array would show up in the UI, which is not hypothetical: upstream's own
//! `retry_times:N` marker does exactly that after a failure.
//!
//! # What is not sent
//!
//! Window titles only, never `ocr_text`. The month's titles come from the index via
//! `wind_store::aggregate::title_totals`, and the `exclude_words` list — which ships containing
//! `KeePass`, `1Password`, `Payment method`, `Card information` — is applied before the table is built,
//! because that list is a secrecy boundary and this is the one function in the crate that puts user data
//! in a network request.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::client::{ChatRequest, Client, Transport, Usage};
use crate::error::{AiError, Faults};
use crate::hashing;
use crate::library::Index;
use crate::prompt;

/// Upstream's `LLM_TEMPERATURE_EXTRACT_DAY_TAGS`.
pub const TAG_TEMPERATURE: f64 = 0.3;
/// The gap that ends one focus session, in seconds — `aggregate::title_intervals`' upstream value, and
/// the reason a title held from 09:00 to 17:00 with the machine asleep is not counted as eight hours.
pub const MAX_TITLE_GAP: i64 = 100;
/// The floor upstream applies before a title counts as activity at all.
pub const MIN_TAGGED_SECONDS: i64 = 1;
/// Month mode shows the model twice as many titles as day mode does.
pub const MONTH_TITLE_MULTIPLIER: usize = 2;
/// Month mode keeps half again as many tags as day mode. Upstream computes `int(n * 1.5)`; matching the
/// truncation keeps the cached lists the same length as the ones already on disk.
pub const MONTH_TAG_MULTIPLIER: f64 = 1.5;

/// A month's title table, ready to send, and the fingerprint of the set it was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleTable {
    /// CSV rows without the header: `prompt::tags_user` adds that, so the fingerprint and the payload
    /// cannot disagree about where the header is.
    pub csv: String,
    /// Titles kept, in the order sent (longest-focused first).
    pub titles: Vec<String>,
    /// A content fingerprint of the *whole* title set, capped or not. See [`Tags::month_key`].
    pub fingerprint: String,
    pub total_seconds: i64,
    /// Titles dropped by `exclude_words` — reported, because a month that quietly produces a short
    /// table because half of it was a password manager is a fact the user should see.
    pub excluded: usize,
}

/// The result of asking about one month.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonthTags {
    /// `YYYY-MM`, the key upstream uses.
    pub month: String,
    pub tags: Vec<String>,
    pub table: TitleTable,
    /// Whether the answer came from disk or from the endpoint.
    pub from_cache: bool,
    pub usage: Option<Usage>,
}

/// A whole run over a month, so the CLI can report what it did and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRun {
    pub tags: MonthTags,
    /// Where the tags file was written, or would have been for a `--dry-run`.
    pub written_to: PathBuf,
    pub dry_run: bool,
    /// No request was made: the cached entry's fingerprint still matches the month's titles.
    pub cache_hit: bool,
}

pub struct Tags<'a> {
    index: &'a Index,
}

impl<'a> Tags<'a> {
    pub fn new(index: &'a Index) -> Tags<'a> {
        Tags { index }
    }

    fn faults(&self) -> &Faults {
        self.index.faults()
    }

    /// `{year}.json`, the file `get_cache_data_by_date` reads.
    pub fn cache_path(&self, year: i64) -> PathBuf {
        self.index.settings.tags_dir.join(format!("{year}.json"))
    }

    /// `{year}.hash.json`, the sibling that keeps the content fingerprint out of the visible tag list.
    pub fn hash_path(&self, year: i64) -> PathBuf {
        self.index.settings.tags_dir.join(format!("{year}.hash.json"))
    }

    /// The month's title table. Public because `--dry-run` and the cache check both need it without
    /// asking the model.
    pub fn title_table(&self, year: i64, month: u32) -> Result<TitleTable, AiError> {
        let settings = &self.index.settings;
        let rows = self.index.month_rows(year, month)?;
        if rows.is_empty() {
            return Err(self.faults().store(format!(
                "the index holds no rows for {year}-{month:02}; nothing to tag"
            )));
        }

        let totals = wind_store::aggregate::title_totals(&rows, MAX_TITLE_GAP);
        // Upstream's `> 1` second floor: a title that appeared for one frame is noise, and it dilutes
        // the table the model is being asked to weigh by duration.
        // Upstream's floor of one second: a title that appeared for a single frame is noise, and it
        // dilutes a table the model is being asked to weigh by duration.
        let mut kept: Vec<(String, i64)> = totals.into_iter().filter(|(_, secs)| *secs > MIN_TAGGED_SECONDS).collect();
        let focusable = kept.len();
        let excluded_before = kept.len();

        let exclude = settings.exclude_words.clone();
        kept.retain(|(title, _)| !contains_any_case(title, &exclude));
        let excluded = excluded_before - kept.len();

        // `ai_extract_tag_wintitle_limit` is a day-mode number; a month gets twice as many rows, which
        // is what upstream's `limit * 2` is for.
        let limit = settings.wintitle_limit.saturating_mul(MONTH_TITLE_MULTIPLIER).max(1);
        kept.truncate(limit);

        if kept.is_empty() {
            // Two very different reasons, and the user can only fix one of them from the settings page.
            return Err(if focusable == 0 {
                self.faults().store(format!(
                    "no window title in {year}-{month:02} held focus for more than {MIN_TAGGED_SECONDS} second, so the month has rows but nothing attributable"
                ))
            } else {
                self.faults().disabled(format!(
                    "all {focusable} window titles in {year}-{month:02} were removed by exclude_words, so nothing is left to tag; that list is why this feature is quiet"
                ))
            });
        }

        let total_seconds: i64 = kept.iter().map(|(_, secs)| *secs).sum();
        let titles: Vec<String> = kept.iter().map(|(title, _)| title.clone()).collect();
        let csv = kept
            .iter()
            .map(|(title, secs)| {
                format!(
                    "{},{}",
                    wind_base::csv::escape_field(&filter_words(title, &settings.filter_words)),
                    wind_base::csv::escape_field(&compact_duration(*secs))
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        // The fingerprint covers the *set*, sorted and de-duplicated, not the table: two runs whose rows
        // came back in different orders (a re-index, a different tie-break in the sort) must hit the
        // same cache entry, and the only thing that legitimately invalidates one is different titles.
        let mut sorted: Vec<&str> = titles.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        sorted.dedup();
        let fingerprint = hashing::hex64(sorted.join("\n").as_bytes());

        Ok(TitleTable { csv, titles, fingerprint, total_seconds, excluded })
    }

    /// The cache key for a month, as upstream writes it: `YYYY-MM`.
    pub fn month_key(year: i64, month: u32) -> String {
        format!("{year:04}-{month:02}")
    }

    /// Read a month's cached tags, and the fingerprint they were generated from.
    ///
    /// A cache entry whose hash file has no fingerprint is treated as a miss rather than as a hit with
    /// unknown content: upstream wrote these files without one, so a directory of pre-port tags must
    /// not be reported as current for a month the user has since recorded more of.
    pub fn cached(&self, year: i64, month: u32) -> Result<Option<(Vec<String>, String)>, AiError> {
        let key = Self::month_key(year, month);
        let tags = self.read_json_map(&self.cache_path(year))?.map(|map| {
            map.get(&key)
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
        });
        let Some(Some(tags)) = tags else { return Ok(None) };
        let fingerprint = self
            .read_json_map(&self.hash_path(year))?
            .and_then(|map| map.get(&key).and_then(Value::as_str).map(str::to_string));
        Ok(fingerprint.map(|hash| (tags, hash)))
    }

    /// Ask for a month's tags, write them, and return the whole story.
    ///
    /// `dry_run` reads the cache and builds the table exactly as a real run would, then writes nothing —
    /// which is what makes it worth having: it is the only way to see what a month *would* cost, and the
    /// only path through this feature that needs no key and sends no bytes.
    pub fn run<T: Transport>(
        &self,
        client: &Client<T>,
        year: i64,
        month: u32,
        dry_run: bool,
    ) -> Result<TagRun, AiError> {
        let settings = &self.index.settings;
        let table = self.title_table(year, month)?;
        let key = Self::month_key(year, month);
        let written_to = self.cache_path(year);

        if let Some((tags, hash)) = self.cached(year, month)? {
            if hash == table.fingerprint {
                return Ok(TagRun {
                    tags: MonthTags {
                        month: key,
                        tags,
                        table,
                        from_cache: true,
                        usage: None,
                    },
                    written_to,
                    dry_run,
                    cache_hit: true,
                });
            }
        }

        let max_tags = month_tag_limit(settings.max_tag_num);
        let completion = client.ask(&ChatRequest {
            system: &prompt::tags_system(&settings.prompts.tags_system, max_tags, settings.prompts.language),
            user: &prompt::tags_user(&settings.prompts.tags_user, &table.csv),
            temperature: TAG_TEMPERATURE,
            // Not JSON mode: the upstream contract for this file is a comma-separated line, and every
            // existing tag list on disk is one. A schema here would be a *second* format for the same
            // cached values, and the Streamlit page reads the first.
            json_mode: false,
        })?;
        let tags = parse_tags(&completion.text, max_tags);
        if tags.is_empty() {
            // An empty list is a legitimate cache value upstream writes for a month with no titles, but
            // arriving here means the model answered with nothing for a table that had rows. Caching it
            // would hide the month behind a hit forever.
            return Err(self.faults().model(format!(
                "the model returned no tags for {key} ({} titles were sent); nothing was cached",
                table.titles.len()
            )));
        }

        if !dry_run {
            self.write_tags(year, &key, &tags, &table.fingerprint)?;
        }
        Ok(TagRun {
            tags: MonthTags { month: key, tags, table, from_cache: false, usage: completion.usage },
            written_to,
            dry_run,
            cache_hit: false,
        })
    }

    /// Merge one month's tags into its year file, leaving every other month's entry alone.
    ///
    /// Read-merge-write rather than overwrite: the file holds a whole year, and regenerating November
    /// must not erase January. Written through a temp file and renamed for the same reason
    /// `wind_base::Config::save` is — a torn write here costs a year of generated tags.
    fn write_tags(&self, year: i64, key: &str, tags: &[String], fingerprint: &str) -> Result<(), AiError> {
        write_map_entry(&self.cache_path(year), key, Value::Array(tags.iter().map(|t| Value::String(t.clone())).collect()))?;
        write_map_entry(&self.hash_path(year), key, Value::String(fingerprint.to_string()))?;
        Ok(())
    }

    fn read_json_map(&self, path: &Path) -> Result<Option<Map<String, Value>>, AiError> {
        let faults = self.faults();
        match std::fs::read(path) {
            Ok(bytes) if bytes.iter().any(|b| !b.is_ascii_whitespace()) => {
                let value: Value = serde_json::from_slice(&bytes).map_err(|e| faults.json(path, e))?;
                match value {
                    Value::Object(map) => Ok(Some(map)),
                    other => Err(faults.io(path, format!("expected a JSON object, found {}", type_name(&other)))),
                }
            }
            Ok(_) => Ok(Some(Map::new())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(faults.io(path, e)),
        }
    }
}

/// Merge one key into a JSON object file and save it, keys sorted and two-space indented — the shape
/// `file_utils.save_dict_as_json_to_path` produces, so a file this wrote and one Python wrote are
/// interchangeable.
fn write_map_entry(path: &Path, key: &str, value: Value) -> Result<(), AiError> {
    let faults = Faults::anonymous();
    let mut map = match std::fs::read(path) {
        Ok(bytes) if bytes.iter().any(|b| !b.is_ascii_whitespace()) => {
            let parsed: Value = serde_json::from_slice(&bytes).map_err(|e| faults.json(path, e))?;
            match parsed {
                Value::Object(map) => map,
                other => return Err(faults.io(path, format!("expected a JSON object, found {}", type_name(&other)))),
            }
        }
        _ => Map::new(),
    };
    map.insert(key.to_string(), value);
    // Sorted on the way out, the way `save_dict_as_json_to_path` writes it, so a file this rewrote and
    // one Python rewrote have the same bytes for the same content.
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    let mut sorted = Map::new();
    for key in keys {
        let entry = map.remove(&key).expect("just listed");
        sorted.insert(key, entry);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| faults.io(parent, e))?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(sorted)).map_err(|e| faults.io(path, e))?;
    let staging = path.with_extension("json.tmp");
    std::fs::write(&staging, &text).map_err(|e| faults.io(&staging, e))?;
    std::fs::rename(&staging, path).map_err(|e| faults.io(path, e))
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// How many tags a month keeps. Upstream: `int(config.ai_extract_max_tag_num * 1.5)`.
pub fn month_tag_limit(day_limit: usize) -> usize {
    (day_limit as f64 * MONTH_TAG_MULTIPLIER) as usize
}

/// Split the model's comma-separated line into tags.
///
/// Upstream does `text.replace("\n","").replace("\r","").split(",")`, which produces two artefacts worth
/// not reproducing: a tag with leading whitespace survives as `" tag"` and renders as a pill with a gap
/// inside it, and an answer ending in a comma yields a final empty tag. Both are fixed by trimming and
/// dropping empties, which changes no legitimate tag because a tag's own content is never whitespace.
pub fn parse_tags(text: &str, max_tags: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let flattened = text.replace(['\n', '\r'], " ");
    for piece in flattened.split(',') {
        let tag = piece.trim().to_string();
        if tag.is_empty() || tag.chars().all(|c| !c.is_alphanumeric()) {
            // A run of punctuation is a model separating with ",," or trailing with ", .".
            continue;
        }
        if out.iter().any(|existing: &String| existing.eq_ignore_ascii_case(&tag)) {
            continue;
        }
        out.push(tag);
        if out.len() == max_tags {
            break;
        }
    }
    out
}

/// Remove `ai_extract_tag_filter_words` from a field value.
///
/// Applied per field rather than to the assembled CSV as upstream does, so that a filter word cannot
/// straddle a quote or a newline and quietly corrupt the table's structure on the way out.
fn filter_words(value: &str, words: &[String]) -> String {
    let mut out = value.to_string();
    for word in words {
        if word.is_empty() {
            continue;
        }
        out = out.replace(word.as_str(), "");
    }
    // A title that was entirely the filtered word collapses to nothing; an empty CSV field would make a
    // row the model can only guess at.
    if out.trim().is_empty() {
        return "‹redacted›".to_string();
    }
    out
}

fn contains_any_case(haystack: &str, needles: &[String]) -> bool {
    let lowered = haystack.to_lowercase();
    needles.iter().any(|needle| !needle.is_empty() && lowered.contains(&needle.to_lowercase()))
}

/// `1h2m3s` / `5m3s` / `3s` — upstream's `convert_seconds_to_hhmmss(x, complete_with_zero=False)`.
///
/// A private helper because `wind_base::clock::seconds_to_hhmmss` renders `1:02:03`, which is the
/// *display* format for the UI's locate column. The duration unit the tag model is asked to weight by is
/// the other one, and sending `1:02:03` would be a silent change to what the prompt means.
pub fn compact_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let rest = seconds % 60;
    let mut out = String::new();
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if minutes > 0 || hours > 0 {
        out.push_str(&format!("{minutes}m"));
    }
    out.push_str(&format!("{rest}s"));
    out
}

/// The tag tests live in `tags_tests.rs` because the fixture — a month of window titles written
/// through the real index writer — is longer than any single case.
#[cfg(test)]
#[path = "tags_tests.rs"]
mod tests;
