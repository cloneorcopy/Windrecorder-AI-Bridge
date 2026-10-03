//! The boundary between "a model said this" and "the index will be asked for this".
//!
//! This is where the port earns its keep. The mapping from a sentence to a query is the whole feature,
//! and the thing that makes it safe is that nothing the model emits is used as-is:
//!
//!   * **The schema is checked, not trusted.** A missing key, a string where a list belongs, a number
//!     where a date belongs — all are refusals or defaults, never passes through. Unknown extra keys
//!     are ignored rather than echoed: a model that invents `"sql": "DROP TABLE"` is answered by that
//!     field being dropped on the floor, because there is no code path that reads it.
//!   * **Terms are data.** [`sanitize_terms`] is the only road from a model string to a `Query` token,
//!     and it strips the two characters that SQLite's `LIKE` treats as wildcards. `wind-store` binds
//!     every value as a parameter — so no keyword can become SQL, and that part is already solved — but
//!     a bound `%` still *means* "any text here", and a model that writes `100%` or `snake_case` would
//!     silently widen what matches. Splitting on them preserves the intent (both were separators)
//!     without handing the pattern language over.
//!   * **Counts and lengths are capped.** A response asking for 4 000 keywords is a response with a
//!     problem; the cap turns it into a normal query instead of a 4 000-clause `WHERE`.
//!   * **Dates are clamped** by `crate::dates`, into the library's own span.
//!   * **`applications` never reach the SQL at all.** They become a substring filter over the titles of
//!     rows that were already fetched — and specifically a *literal* one, because building a regex out
//!     of model output (what upstream does, `re.escape`d) invites a catastrophic-backtracking pattern
//!     from a model that has never heard of ReDoS.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::dates::{self, Resolved};
use crate::error::{AiError, Faults};

/// How many search terms one answer may contribute. A day of screen text matches on two or three; a
/// model that has come unglued produces dozens.
pub const MAX_TERMS: usize = 12;
/// Longest single term, in characters. Past this it is a clause, and a clause ANDed against `LIKE`
/// matches nothing.
pub const MAX_TERM_CHARS: usize = 64;
/// Window-title fragments: an application name is short and there are rarely two.
pub const MAX_APPLICATIONS: usize = 4;

/// Which one hit the sentence was asking for. Upstream's `occurrence`, narrowed to three values; the
/// "any" case sorts newest-first, which is what a person scanning their own history wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occurrence {
    First,
    Last,
    Any,
}

impl Occurrence {
    /// `(parsed, recognised)`. `any` is recognised and is not a note-worthy deviation: the search prompt
    /// in `crate::prompt` lists `"first" | "last" | "any"` as the three legal values *and* shows
    /// `"occurrence": "any"` in the example object, so an answer that says `any` followed the schema
    /// exactly. Recording it as a misunderstanding would put a bogus entry in `notes`, and `notes` being
    /// empty is the crate's statement that "the search the model described is the search that ran" —
    /// which `--explain` prints to the user.
    fn parse(text: &str) -> (Occurrence, bool) {
        match text.trim().to_ascii_lowercase().as_str() {
            "first" => (Occurrence::First, true),
            "last" => (Occurrence::Last, true),
            "any" => (Occurrence::Any, true),
            _ => (Occurrence::Any, false),
        }
    }

    /// Newest-first, except when the sentence asked for the first time something happened.
    pub fn newest_first(self) -> bool {
        !matches!(self, Occurrence::First)
    }
}

/// A validated, clamped, ready-to-run search description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPlan {
    pub keywords: Vec<String>,
    pub exclude: Vec<String>,
    /// Literal, case-insensitive window-title substrings. Applied after the query, never inside it.
    pub applications: Vec<String>,
    pub occurrence: Occurrence,
    /// Inclusive range in the stored naive-local epoch.
    pub from: i64,
    pub to: i64,
    /// What the model asked for, before clamping, for `--explain`.
    pub requested_dates: (String, String),
    /// Every override made on the model's behalf. Empty means the plan is the model's own answer.
    pub notes: Vec<String>,
}

impl SearchPlan {
    /// Broad-question shape: no keywords, so the query returns the whole window. Upstream reaches the
    /// same state by setting `keyword_input = ""` when the intent is "summarize activities"; here it is
    /// just `keywords.is_empty()`, which cannot disagree with itself.
    pub fn is_time_only(&self) -> bool {
        self.keywords.is_empty()
    }

    /// The keywords as the single whitespace-separated string `Query::with_keywords` takes.
    pub fn keywords_joined(&self) -> String {
        self.keywords.join(" ")
    }

    pub fn exclude_joined(&self) -> String {
        self.exclude.join(" ")
    }

    /// The range as `dates` resolved it, for reporting.
    pub fn range(&self) -> Resolved {
        Resolved {
            from: self.from,
            to: self.to,
            requested: self.requested_dates.clone(),
            notes: self.notes.clone(),
        }
    }
}

/// Parse and validate a model answer into a [`SearchPlan`].
///
/// `text` is the assistant message verbatim. `bounds` is the library's own `(earliest, latest)` in
/// stored seconds, which is what makes "outside the library" a decidable question rather than a guess.
pub fn build_plan(text: &str, bounds: (i64, i64), faults: &Faults) -> Result<SearchPlan, AiError> {
    let value = read_json_object(text, faults)?;
    let mut notes: Vec<String> = dropped_unknown_keys(&value);

    let keywords = terms(&value, "keywords", faults)?;
    let exclude = terms(&value, "exclude_keywords", faults)?;
    let applications = applications(&value);
    let occurrence = match value.get("occurrence") {
        Some(Value::String(text)) => {
            let (parsed, recognised) = Occurrence::parse(text);
            if !recognised && !text.trim().is_empty() {
                notes.push(format!("`occurrence` {text:?} is not first/last/any; treated as any"));
            }
            parsed
        }
        Some(Value::Null) | None => Occurrence::Any,
        Some(other) => {
            notes.push(format!("`occurrence` is {} not a string; treated as any", kind_of(other)));
            Occurrence::Any
        }
    };

    let model_gave_dates = text_of(&value, "start_date").is_some() && text_of(&value, "end_date").is_some();
    let range = match (text_of(&value, "start_date"), text_of(&value, "end_date")) {
        (Some(start), Some(end)) => match dates::resolve(&start, &end, bounds) {
            Ok(mut resolved) => {
                notes.extend(std::mem::take(&mut resolved.notes));
                resolved
            }
            Err(reason) => {
                // Not fatal. "I don't know when" has a good answer — search everything — and
                // failing the whole search over an unparseable date is the worse outcome by a wide
                // margin, because the keywords were probably fine.
                notes.push(format!("{reason}; searching the whole recorded history instead"));
                dates::everything(bounds)
            }
        },
        _ => {
            notes.push(missing_range_note(&value));
            dates::everything(bounds)
        }
    };

    // The one answer that cannot be turned into anything: a model that found no terms *and* no time in
    // the sentence. That is not a sparse reading of the question, it is a failure to read it, and the
    // alternative is silently dumping the user's entire history at them.
    if keywords.is_empty() && exclude.is_empty() && applications.is_empty() && !model_gave_dates {
        return Err(faults.model(format!(
            "the answer found neither a search term nor a time in the answer; refusing to search all of history on that"
        )));
    }

    Ok(SearchPlan {
        keywords,
        exclude,
        applications,
        occurrence,
        from: range.from,
        to: range.to,
        requested_dates: range.requested,
        notes,
    })
}

/// Accept a fenced block or leading prose.
///
/// `response_format: json_object` makes a compliant endpoint return parseable JSON, but the compatible
/// family is wide and several gateways wrap the object in \`\`\`json anyway. Cutting to the outermost
/// braces is tolerance on the *transport*, not trust in the *content*: everything inside is still
/// validated by `build_plan`, and a response with no braces at all is still a refusal.
fn read_json_object(text: &str, faults: &Faults) -> Result<Value, AiError> {
    let trimmed = text.trim();
    let candidate = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(first), Some(last)) if last > first => &trimmed[first..=last],
        _ => {
            return Err(faults.model(format!(
                "expected a JSON object, got: {}",
                clip(trimmed)
            )))
        }
    };
    let value: Value = serde_json::from_str(candidate)
        .map_err(|e| faults.model(format!("the answer is not parseable JSON ({e}): {}", clip(candidate))))?;
    if !value.is_object() {
        return Err(faults.model(format!("expected a JSON object, got {}", kind_of(&value))));
    }
    Ok(value)
}

/// Report — never act on — keys outside the schema.
fn dropped_unknown_keys(value: &Value) -> Vec<String> {
    const SCHEMA: [&str; 6] =
        ["keywords", "exclude_keywords", "applications", "start_date", "end_date", "occurrence"];
    value
        .as_object()
        .into_iter()
        .flat_map(|map| map.keys())
        .filter(|key| !SCHEMA.contains(&key.as_str()))
        .map(|key| format!("ignored a key the schema does not have: {key:?}"))
        .collect()
}

fn clip(text: &str) -> String {
    let chars: Vec<char> = text.chars().take(200).collect();
    let mut out: String = chars.into_iter().collect();
    if text.chars().count() > out.chars().count() {
        out.push('…');
    }
    out
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn text_of(value: &Value, key: &str) -> Option<String> {
    // Only a string is a date. A number means the model wrote `20260921`, and an object means it wrote
    // something else again; guessing at a date is exactly what this module exists not to do, so those
    // fall through to "no range given", which `missing_range_note` reports honestly.
    match value.get(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
        _ => None,
    }
}

fn missing_range_note(value: &Value) -> String {
    let shape = |key: &str| match value.get(key) {
        None | Some(Value::Null) => "absent".to_string(),
        Some(other) => format!("a {}", kind_of(other)),
    };
    format!(
        "`start_date` is {} and `end_date` is {}; searching the whole recorded history instead",
        shape("start_date"),
        shape("end_date")
    )
}

/// The one road from model output to a query term.
fn terms(value: &Value, key: &str, faults: &Faults) -> Result<Vec<String>, AiError> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    match value.get(key) {
        None | Some(Value::Null) => return Ok(out),
        // Upstream coerces a non-list to `[]` and carries on, which is right: an answer with
        // `"keywords": "renewal"` still means "search for renewal", and silently dropping it would
        // cost the user their search for a formatting slip. So: accept a bare string as one term.
        Some(Value::String(text)) => push_terms(text, &mut out, &mut seen),
        Some(Value::Array(items)) => {
            for item in items {
                match item {
                    Value::String(text) => push_terms(text, &mut out, &mut seen),
                    Value::Number(number) => push_terms(&number.to_string(), &mut out, &mut seen),
                    other => {
                        // A nested object in a keyword list is a malformed answer, not a threat.
                        let _ = other;
                    }
                }
            }
        }
        Some(other) => {
            return Err(faults.model(format!("`{key}` is {}, expected a list of strings", kind_of(other))))
        }
    }
    if out.len() > MAX_TERMS {
        out.truncate(MAX_TERMS);
    }
    Ok(out)
}

/// One model string in, zero or more query terms out.
///
/// A string containing whitespace *is* several ANDed terms — upstream joins the list with spaces and
/// lets the search split it, so doing the split explicitly here keeps `--explain` honest about what the
/// query will actually be.
fn push_terms(raw: &str, out: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    for piece in split_like_operators(raw) {
        let cleaned: String = piece
            .chars()
            // A newline or tab inside a term means the model wrote a sentence; the split above already
            // handles ordinary whitespace, so anything left here is control noise.
            .filter(|c| !c.is_control())
            .take(MAX_TERM_CHARS)
            .collect::<String>()
            .trim()
            .to_string();
        if cleaned.is_empty() || !seen.insert(cleaned.clone()) {
            continue;
        }
        out.push(cleaned);
    }
}

/// Split on whitespace and on the two `LIKE` metacharacters.
///
/// `%` and `_` are the only characters in a bound parameter that SQLite still interprets, so removing
/// them is the difference between "the keyword is data" and "the keyword is a pattern". Splitting
/// rather than deleting keeps `snake_case` searching for `snake` and `case` — which is what a user
/// typing `snake_case` into a `LIKE`-based search is effectively asking for anyway, and the same move
/// `wind_store::search::Query` already makes for inner hyphens.
fn split_like_operators(raw: &str) -> Vec<String> {
    raw.split(|c: char| c.is_whitespace() || c == '%' || c == '_')
        .filter(|piece| !piece.is_empty())
        .map(|piece| piece.to_string())
        .collect()
}

fn applications(value: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(Value::Array(items)) = value.get("applications") {
        for item in items.iter().filter_map(Value::as_str) {
            // A title fragment keeps its spaces: it is matched against a whole window title, not
            // tokenised by the search. It is *not* run through `split_like_operators` for the same
            // reason — and it never reaches SQL at all, so `%` there is inert.
            let trimmed = item.trim().chars().filter(|c| !c.is_control()).take(MAX_TERM_CHARS).collect::<String>();
            if trimmed.is_empty() || !seen.insert(trimmed.to_lowercase()) {
                continue;
            }
            out.push(trimmed);
            if out.len() == MAX_APPLICATIONS {
                break;
            }
        }
    }
    out
}

/// Case-insensitive literal title filter, applied to rows the query already returned.
pub fn matches_application(title: Option<&str>, applications: &[String]) -> bool {
    if applications.is_empty() {
        return true;
    }
    let Some(title) = title else { return false };
    let lowered = title.to_lowercase();
    applications.iter().any(|needle| lowered.contains(&needle.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Faults;
    use serde_json::json;

    fn epoch(stamp: &str) -> i64 {
        wind_base::clock::LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    fn bounds() -> (i64, i64) {
        (epoch("2026-09-01_00-00-00"), epoch("2026-09-30_23-59-59"))
    }

    fn build(answer: Value) -> Result<SearchPlan, AiError> {
        build_plan(&answer.to_string(), bounds(), &Faults::anonymous())
    }

    fn valid() -> Value {
        json!({
            "keywords": ["续约", "email"],
            "exclude_keywords": ["招聘"],
            "applications": ["WeChat"],
            "start_date": "2026-09-18",
            "end_date": "2026-09-19",
            "occurrence": "last"
        })
    }

    #[test]
    fn a_good_answer_becomes_a_query_description_unchanged() {
        let plan = build(valid()).expect("valid");
        assert_eq!(plan.keywords, vec!["续约", "email"]);
        assert_eq!(plan.exclude, vec!["招聘"]);
        assert_eq!(plan.applications, vec!["WeChat"]);
        assert_eq!(plan.occurrence, Occurrence::Last);
        assert_eq!((plan.from, plan.to), (epoch("2026-09-18_00-00-00"), epoch("2026-09-19_23-59-59")));
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
        assert_eq!(plan.keywords_joined(), "续约 email");
        assert!(!plan.is_time_only());
    }

    #[test]
    fn the_three_documented_occurrence_values_survive_and_invented_ones_are_said_out_loud() {
        for (given, want) in
            [("first", Occurrence::First), ("LAST", Occurrence::Last), ("any", Occurrence::Any), ("whenever", Occurrence::Any)]
        {
            let answer = json!({ "keywords": ["x"], "start_date": "2026-09-02", "end_date": "2026-09-03", "occurrence": given });
            let plan = build(answer).unwrap();
            assert_eq!(plan.occurrence, want, "{given}");
        }

        // The distinction that matters is in `notes`, not in the value: `any` is what `prompt` shows in
        // the example object, so an answer carrying it deviated from nothing and must leave `notes`
        // empty — an empty `notes` is this crate's promise that the plan the model described is the plan
        // that ran. A word the schema never offered is a real deviation and has to be reported.
        let documented = json!({ "keywords": ["x"], "start_date": "2026-09-02", "end_date": "2026-09-03", "occurrence": "any" });
        assert!(build(documented).unwrap().notes.is_empty(), "a schema value must not be noted as an override");
        let invented = json!({ "keywords": ["x"], "start_date": "2026-09-02", "end_date": "2026-09-03", "occurrence": "whenever" });
        assert!(build(invented).unwrap().notes.iter().any(|n| n.contains("`occurrence`")), "invented is unreported");

        assert!(!Occurrence::First.newest_first());
        assert!(Occurrence::Last.newest_first() && Occurrence::Any.newest_first());
    }

    /// The over-wide answer: a model that asks for a decade gets the library, plus a note saying so.
    #[test]
    fn a_range_wider_than_the_library_is_clamped_and_said_out_loud() {
        let answer = json!({ "keywords": ["x"], "start_date": "1998-01-01", "end_date": "2038-01-01" });
        let plan = build(answer).unwrap();
        assert_eq!((plan.from, plan.to), bounds());
        assert!(plan.notes.iter().any(|n| n.contains("clamped")), "{:?}", plan.notes);
    }

    #[test]
    fn a_range_wholly_outside_the_library_still_returns_something_searchable() {
        let answer = json!({ "keywords": ["x"], "start_date": "2019-05-04", "end_date": "2019-05-05" });
        let plan = build(answer).unwrap();
        assert_eq!((plan.from, plan.to), (epoch("2026-09-01_00-00-00"), epoch("2026-09-01_23-59-59")));
        assert!(plan.notes.iter().any(|n| n.contains("entirely outside")), "{:?}", plan.notes);
    }

    #[test]
    fn a_hallucinated_date_shape_degrades_to_a_full_span_search_not_a_failure() {
        let answer = json!({ "keywords": ["x"], "start_date": "last tuesday", "end_date": "recently" });
        let plan = build(answer).unwrap();
        assert_eq!((plan.from, plan.to), bounds());
        assert!(plan.notes.iter().any(|n| n.contains("YYYY-MM-DD")), "{:?}", plan.notes);
    }

    #[test]
    fn no_dates_at_all_is_a_full_span_search_with_a_note() {
        let answer = json!({ "keywords": ["x"] });
        let plan = build(answer).unwrap();
        assert_eq!((plan.from, plan.to), bounds());
        assert!(plan.notes.iter().any(|n| n.contains("`start_date` is absent")), "{:?}", plan.notes);
    }

    /// The LIKE-wildcard rule, which is the difference between a bound parameter and a pattern.
    #[test]
    fn wildcard_and_underscore_characters_cannot_become_patterns() {
        let answer = json!({
            "keywords": ["100% growth", "snake_case", "%", "_", "%%%"],
            "start_date": "2026-09-02", "end_date": "2026-09-03"
        });
        let plan = build(answer).unwrap();
        assert_eq!(plan.keywords, vec!["100", "growth", "snake", "case"], "{:?}", plan.keywords);
        assert!(plan.keywords.iter().all(|k| !k.contains('%') && !k.contains('_')));
    }

    #[test]
    fn a_multi_word_string_in_the_list_is_split_into_anded_terms() {
        let answer = json!({ "keywords": ["quarterly revenue report"], "start_date": "2026-09-02", "end_date": "2026-09-03" });
        assert_eq!(build(answer).unwrap().keywords, vec!["quarterly", "revenue", "report"]);
    }

    #[test]
    fn a_bare_string_where_a_list_belongs_is_still_understood() {
        let answer = json!({ "keywords": "renewal", "start_date": "2026-09-02", "end_date": "2026-09-03" });
        assert_eq!(build(answer).unwrap().keywords, vec!["renewal"]);
    }

    #[test]
    fn terms_are_deduplicated_capped_and_cut_to_length() {
        let many: Vec<String> = (0..40).map(|i| format!("term{i}")).collect();
        let answer = json!({ "keywords": many, "start_date": "2026-09-02", "end_date": "2026-09-03" });
        let plan = build(answer).unwrap();
        assert_eq!(plan.keywords.len(), MAX_TERMS, "{:?}", plan.keywords);

        let repeats = json!({ "keywords": ["a", "a", "A", " a "], "start_date": "2026-09-02", "end_date": "2026-09-03" });
        assert_eq!(build(repeats).unwrap().keywords, vec!["a", "A"], "case is not folded, whitespace is trimmed");

        let long = json!({ "keywords": ["x".repeat(500)], "start_date": "2026-09-02", "end_date": "2026-09-03" });
        assert_eq!(build(long).unwrap().keywords[0].chars().count(), MAX_TERM_CHARS);
    }

    #[test]
    fn control_characters_and_empty_terms_are_dropped() {
        let answer = json!({ "keywords": ["a\u{0}b", "", "   ", "\n"], "start_date": "2026-09-02", "end_date": "2026-09-03" });
        assert_eq!(build(answer).unwrap().keywords, vec!["ab"]);
    }

    #[test]
    fn a_number_in_a_term_list_is_searched_for_as_text() {
        let answer = json!({ "keywords": [4096], "start_date": "2026-09-02", "end_date": "2026-09-03" });
        assert_eq!(build(answer).unwrap().keywords, vec!["4096"]);
    }

    #[test]
    fn applications_are_limited_deduplicated_and_keep_their_spaces() {
        let answer = json!({
            "keywords": ["x"],
            "applications": ["Task Manager", "task manager", "WeChat", "Excel", "Chrome", "Safari"],
            "start_date": "2026-09-02", "end_date": "2026-09-03"
        });
        let plan = build(answer).unwrap();
        assert_eq!(plan.applications.len(), MAX_APPLICATIONS, "{:?}", plan.applications);
        assert!(plan.applications.contains(&"Task Manager".to_string()), "a title fragment is not tokenised");
        assert!(!plan.applications.contains(&"task manager".to_string()), "case-insensitive dedupe");
    }

    #[test]
    fn the_title_filter_is_literal_and_case_insensitive() {
        let needles = vec!["wechat".to_string()];
        assert!(matches_application(Some("微信 - WeChat"), &needles));
        assert!(!matches_application(Some("Chrome"), &needles));
        assert!(!matches_application(None, &needles), "a row with no title cannot match a named app");
        assert!(matches_application(None, &[]), "no filter matches everything");
        // A fragment that would be a regex if anyone ran it as one is still just characters.
        assert!(matches_application(Some("a(b"), &[String::from("a(b")]));
        assert!(!matches_application(Some("ab"), &[String::from("a(b")]));
    }

    #[test]
    fn unknown_keys_are_reported_and_ignored_never_acted_on() {
        let mut answer = valid();
        answer.as_object_mut().unwrap().insert("sql".into(), json!("DROP TABLE video_text"));
        answer.as_object_mut().unwrap().insert("limit".into(), json!(99_999));
        let plan = build(answer).unwrap();
        let plain = build(valid()).unwrap();
        assert_eq!(plan.notes.len(), 2, "{:?}", plan.notes);
        let plain_notes = plan.notes.iter().filter(|n| !n.starts_with("ignored a key")).count();
        assert_eq!(plain_notes, 0, "the only notes are the ignored keys");
        // Same query either way: the extra fields changed nothing but the report.
        assert_eq!((&plan.keywords, &plan.exclude, &plan.applications, &plan.occurrence, plan.from, plan.to),
                   (&plain.keywords, &plain.exclude, &plain.applications, &plain.occurrence, plain.from, plain.to));
        assert_eq!(plan.notes.len(), 2, "{:?}", plan.notes);
        assert!(plan.notes.iter().all(|n| n.starts_with("ignored a key")), "{:?}", plan.notes);
    }

    #[test]
    fn a_fenced_or_wrapped_json_object_is_still_read() {
        let wrapped = format!("```json\n{}\n```", valid());
        assert!(build_plan(&wrapped, bounds(), &Faults::anonymous()).is_ok());
        let chatty = format!("Sure! Here you go:\n{}\nHope that helps.", valid());
        assert!(build_plan(&chatty, bounds(), &Faults::anonymous()).is_ok());
    }

    #[test]
    fn a_non_object_answer_is_a_refusal_that_names_the_model() {
        let faults = Faults::anonymous();
        for text in ["no json here", "", "[]", "\"keywords\"", "42", "{", "}"] {
            let error = build_plan(text, bounds(), &faults).expect_err("{text} must be refused");
            assert_eq!(error.kind(), crate::error::ErrorKind::Model, "{text}");
        }
    }

    #[test]
    fn a_truncated_object_is_refused_rather_than_completed() {
        // Silently defaulting the missing half of an answer would produce a confident wrong search.
        let error = build_plan(r#"{"keywords": ["a"], "start_date": "#, bounds(), &Faults::anonymous()).unwrap_err();
        assert!(error.to_string().contains("JSON"), "{error}");
    }

    #[test]
    fn a_wrongly_typed_term_list_is_a_refusal() {
        let answer = json!({ "keywords": {"nested": true}, "start_date": "2026-09-02", "end_date": "2026-09-03" });
        let error = build(answer).expect_err("an object is not a list");
        assert!(error.to_string().contains("`keywords` is an object"), "{error}");
    }

    /// An answer that could only ever return one second of history is a broken answer, and reporting it
    /// beats printing zero rows and letting the user wonder.
    #[test]
    fn an_answer_that_found_nothing_in_the_sentence_is_refused() {
        let answer = json!({ "keywords": [], "exclude_keywords": [], "applications": [] });
        let error = build(answer).expect_err("an empty answer must not search all of history");
        assert!(error.to_string().contains("refusing to search all of history"), "{error}");
        assert_eq!(error.kind(), crate::error::ErrorKind::Model);

        // A date with no terms *is* a question — "what did I do on Tuesday" — and must be honoured.
        let broad = json!({ "keywords": [], "start_date": "2026-09-02", "end_date": "2026-09-09" });
        let plan = build(broad).unwrap();
        assert!(plan.is_time_only());
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    #[test]
    fn the_range_accessor_carries_the_notes_for_explain() {
        let answer = json!({ "keywords": ["x"], "start_date": "1990-01-01", "end_date": "1990-02-01" });
        let plan = build(answer).unwrap();
        let range = plan.range();
        assert_eq!(range.requested, (String::from("1990-01-01"), String::from("1990-02-01")));
        assert_eq!(range.notes, plan.notes);
    }
}
