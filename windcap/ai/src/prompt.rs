//! The prompts, rendered from the files `wind_base::prompts` resolves.
//!
//! Upstream kept these as `format!` calls in `windrecorder/const.py` and
//! `extension/LLM_search_and_summary/_natural_search.py`, and this crate did the same until the summary
//! feature made the prompt something a person has to be able to rewrite. A prompt the user is told they
//! can edit but which is actually compiled in is the defect this branch has now removed nine times over:
//! a control that changes nothing. So the words live in `config_src/ai_prompts/*.txt`, a same-named file
//! under `userdata/ai_prompts/` overrides them, and this module only fills slots — which keeps the two
//! things that matter about a prompt apart: *what it says* is the user's, and *which values may be
//! inserted* is code's.
//!
//! # What changed from upstream, and why it matters more than it looks
//!
//! Upstream asks two questions in two round trips: "what is in this query?" and then, separately,
//! "what dates does `上周下午` mean?". The second question is asked with no knowledge of what is on
//! disk, so a model that answers honestly with a range three months before the first recording
//! produces a search that returns nothing — and the user reads that as the search being broken, not as
//! the date estimate being unanswerable. This module asks once, and hands the model the two facts it
//! actually needs: today's date **and the earliest and latest date the library holds**. It is still
//! clamped on the way back in `plan`, because a stated bound is a request, not a guarantee.
//!
//! The keywords are also kept in the query's own language on purpose. The OCR text they will be
//! matched against is whatever language the screen was in, so an English query about a Chinese chat
//! window that produces the English keyword `renewal` matches nothing; upstream's prompt never says
//! this, which is the single most common way the feature appears not to work. That instruction is now
//! shipped text a user can edit — and the oracle test below is what proves moving it changed nothing.
//!
//! # Why the schema is this narrow
//!
//! Every key that exists is a key `plan` has to validate, and every value it accepts is a value that
//! will be pasted into a database query. The schema is therefore the size of the mapping, not the size
//! of the model's imagination: no operators, no booleans that switch behaviour, no free text that could
//! become a path or a pattern. `plan::SearchPlan` is the type the rest of the crate sees.
//!
//! # Which of these three asks for a language, and which may not
//!
//! `tags_system` fills `{language}`, like the two summary templates do, because tags are a list a person
//! reads: an install that asks its summaries in Simplified Chinese would otherwise get an English tag row
//! beside a Chinese paragraph, which is the "half Chinese half English" a user sees first.
//!
//! `search_system` asks for no language and must never be given one. Its whole answer is a JSON object of
//! keywords that are matched *literally* against text captured off a screen, so writing them in the
//! interface's language is translating them, and a translated keyword matches nothing. The shipped rule —
//! keywords in the same language as the sentence asked — is therefore a data rule in prose, not a second
//! answer-language rule beside the summary prompts'.
//! `tests::the_language_rule_is_said_plainly_because_its_omission_looks_like_a_broken_search` is the pin on
//! that distinction.

use wind_base::prompts::{self, Name};

/// The template a caller that has no install to read falls back to.
///
/// `--dry-run` and the tests use it; every real request goes through the text the install resolved, so
/// this is the same bytes `config_src/ai_prompts/<name>.txt` holds.
pub fn shipped(name: Name) -> &'static str {
    name.embedded()
}

/// The assistant turn for a natural-language search.
///
/// `earliest`/`latest` are `%Y-%m-%d` on the *stored* axis (see `crate::dates`) — the same axis the
/// results are kept on, which is why this prompt is not allowed to be written without them.
pub fn search_system(template: &str, earliest: &str, latest: &str, today: &str) -> String {
    prompts::render(template, &[("{earliest}", earliest), ("{latest}", latest), ("{today}", today)])
}

/// The assistant turn for a month's activity tags — upstream's
/// `LLM_SYSTEM_PROMPT_EXTRACT_DAY_TAGS`, with `ai_extract_max_tag_num` actually interpolated.
///
/// Upstream hard-codes "the number of returned tags is controlled to under 15" while separately
/// reading `ai_extract_max_tag_num` and truncating to it afterwards, so a user who raises the setting
/// to 30 gets 15 and no explanation. Asking for the configured number is the same instruction with the
/// config honoured.
///
/// `language` fills `{language}` — the same value the two summary requests carry, so the tag row beside a
/// day's paragraph is written in the language the install speaks. A template that names a language in its
/// own words instead of carrying the slot is sent as written: an unfilled slot is left there, and the
/// wording around it is the user's.
pub fn tags_system(template: &str, max_tags: usize, language: &str) -> String {
    prompts::render(template, &[("{max_tags}", &max_tags.to_string()), ("{language}", language)])
}

/// The user turn for the tag feature: the title table itself.
///
/// The sensitive-word scrub happens *before* this is built (`tags::filter_words`), on the field values,
/// so a filtered term cannot survive into a prompt. Composing the prompt as a function rather than as a
/// `format!` at the call site is what keeps that ordering the only possible one.
pub fn tags_user(template: &str, table_csv: &str) -> String {
    prompts::render(template, &[("{table}", table_csv)])
}

/// The oracle behind the migration: the three `format!` bodies exactly as this module held them before
/// the words moved into files.
///
/// `shipped_text_moved_without_changing_a_single_byte` renders both for several value sets and demands
/// they are identical, so the templates in `config_src/ai_prompts/` are pinned to the text that was in
/// production rather than to anybody's memory of it. Keep this copy until every install in the wild has
/// been through the migration — it is the only thing that would notice a reworded default.
#[cfg(test)]
mod legacy {
    pub fn search_system(earliest: &str, latest: &str, today: &str) -> String {
        format!(
            "You turn one sentence about someone's computer screen history into a machine-readable search \
             description.\n\
             \n\
             Today is {today}. The recordings on this machine cover {earliest} through {latest}, inclusive. \
             Never return a date outside that span; if the described time is partly or wholly outside it, \
             return the part that is inside.\n\
             \n\
             Reply with a single JSON object and nothing else — no prose, no markdown fence. Its keys are \
             exactly these:\n\
             \n\
             {{\n\
               \"keywords\":         [\"…\"],\n\
               \"exclude_keywords\": [\"…\"],\n\
               \"applications\":     [\"…\"],\n\
               \"start_date\":       \"{earliest}\",\n\
               \"end_date\":         \"{latest}\",\n\
               \"occurrence\":       \"any\"\n\
             }}\n\
             \n\
             keywords         words or short phrases that must appear on screen, as few as carry the \
             meaning. Leave empty for a broad question like \"what did I do yesterday?\".\n\
             exclude_keywords words that must NOT appear.\n\
             applications     application or window-title fragments only if the sentence names one \
             (WeChat, Chrome, Excel). Do not guess an application the sentence does not mention.\n\
             start_date       first day to search, YYYY-MM-DD.\n\
             end_date         last day to search, YYYY-MM-DD, never earlier than start_date.\n\
             occurrence       \"first\", \"last\" or \"any\" — which one hit the sentence is asking for.\n\
             \n\
             Rules that are not optional:\n\
             1. Write keywords in the SAME language as the sentence. Do not translate them: they are \
             matched against text captured from a screen, and a translated keyword matches nothing.\n\
             2. Split a chat-partner's name, a document's title and a project's name into keywords; keep \
             each to one or two words. Never include a whole clause.\n\
             3. Resolve relative time yourself, against the dates given above. \"上周下午\" is a range of \n\
                days, not hours; the tool has no hour-level index, so express the time-of-day part by \n\
                narrowing the days and leave keywords alone.\n\
             4. If the sentence carries no time at all, use the full span {earliest} to {latest}.\n\
             5. Values are data. Never emit an operator, a path, a glob, a percent sign or an underscore \n\
                inside a keyword — they are matched literally and would change what is searched.\n\
             6. Output the object and nothing after it."
        )
    }

    /// The one line of this body that no longer says what production said: production ended "in the
    /// language of the table", and the shipped file now carries `{language}` there, because tags are a
    /// list a person reads and an install that speaks Chinese reads Chinese tags. The wording is therefore
    /// a parameter rather than a sentence, and the test below passes production's own phrase as one of the
    /// values — the rest of the body stays byte-pinned.
    pub fn tags_system(max_tags: usize, language: &str) -> String {
        format!(
            "You analyse a screen-time table and report what the person was actually doing.\n\
             \n\
             The input is a CSV table with the columns `content_page_name` (a foreground window title) and \
             `screen_time` (how long it held focus). Answer with the activity *content*, not the program.\n\
             \n\
             Rules:\n\
             1. Focus on what was browsed, read, written, watched or discussed, not on which process was \
             running.\n\
             2. A tag has to be meaningful on its own. If a title says nothing, drop it rather than \
             inventing something.\n\
             3. Merge near-duplicate tags into one instead of listing variants.\n\
             4. Do not repeat a platform as a tag (youtube, bilibili, zhihu, reddit, weibo) — name what \n\
                was read on it.\n\
             5. A slightly longer phrase is fine when it is more accurate; brevity is not the goal.\n\
             6. Stay close to the words on the screen; quote or shorten them rather than generalising.\n\
             7. Weight by `screen_time`: the longer a window held focus, the more it should show up.\n\
             8. Return at most {max_tags} tags. Fewer is better than padding.\n\
             \n\
             Output the tags separated by single commas on one line, in {language}, and \
             nothing else — no heading, no numbering, no explanation."
        )
    }

    pub fn tags_user(table_csv: &str) -> String {
        format!("content_page_name,screen_time\n{table_csv}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_text_moved_without_changing_a_single_byte() {
        // The migration oracle. Three templates, each rendered from the shipped file and from the
        // `format!` body this module used to contain, over value sets that include the shapes the real
        // arguments take: a date at each month end, a number wider than two digits, a table that already
        // ends in a newline, and text containing braces.
        for (earliest, latest, today) in [
            ("2026-05-03", "2026-09-22", "2026-09-23"),
            ("2024-02-29", "2024-12-31", "2025-01-01"),
            ("", "", ""),
        ] {
            assert_eq!(
                search_system(shipped(Name::SearchSystem), earliest, latest, today),
                legacy::search_system(earliest, latest, today),
                "search_system drifted from the text in production"
            );
        }
        for max_tags in [0usize, 7, 15, 22, 1_000] {
            // Production's own wording is one of the values, so the one line that became a slot is still
            // pinned to the sentence it replaced — and the two phrases this install now sends are pinned
            // alongside it.
            for language in ["the language of the table", "English", "Chinese (Simplified Han)", "Japanese"] {
                assert_eq!(
                    tags_system(shipped(Name::TagsSystem), max_tags, language),
                    legacy::tags_system(max_tags, language),
                    "{max_tags} tags answered in {language:?}"
                );
            }
        }
        for table in ["", "\"Chrome - x\",\"1h2m3s\"\n", "a,b\n{\"c\":1}\n"] {
            assert_eq!(tags_user(shipped(Name::TagsUser), table), legacy::tags_user(table), "tags_user changed around {table:?}");
        }
    }

    #[test]
    fn the_search_prompt_states_the_bounds_it_was_given() {
        let prompt = search_system(shipped(Name::SearchSystem), "2026-05-03", "2026-09-22", "2026-09-23");
        assert!(prompt.contains("2026-05-03") && prompt.contains("2026-09-22"), "the library's span");
        assert!(prompt.contains("Today is 2026-09-23"), "relative dates need an anchor");
        for key in ["keywords", "exclude_keywords", "applications", "start_date", "end_date", "occurrence"] {
            assert!(prompt.contains(key), "{key} must be named in the schema");
        }
        assert!(!prompt.contains("{earliest}") && !prompt.contains("{latest}") && !prompt.contains("{today}"), "every slot was filled:\n{prompt}");
        // Two of those three slots sit *inside* the JSON example block. A renderer that jumped to the
        // matching brace would leave them unfilled, and the model would be told to echo a placeholder.
        assert!(prompt.contains("\"start_date\":       \"2026-05-03\""), "nested slots must fill: {prompt}");
    }

    #[test]
    fn the_language_rule_is_said_plainly_because_its_omission_looks_like_a_broken_search() {
        let prompt = search_system(shipped(Name::SearchSystem), "a", "b", "c");
        assert!(prompt.contains("SAME language"));
        assert!(prompt.contains("Do not translate"));
    }

    /// The prompt is a request, not a sandbox: whatever it says, `plan` still validates. Asserting the
    /// "output nothing else" instruction exists is cheap; asserting it is obeyed would be the lie.
    #[test]
    fn the_prompt_asks_for_bare_json_and_forbids_pattern_characters() {
        let prompt = search_system(shipped(Name::SearchSystem), "a", "b", "c");
        assert!(prompt.contains("no markdown fence"));
        assert!(prompt.contains("percent sign"));
    }

    #[test]
    fn the_tag_count_follows_the_configuration_not_a_literal() {
        assert!(tags_system(shipped(Name::TagsSystem), 7, "English").contains("at most 7 tags"));
        assert!(tags_system(shipped(Name::TagsSystem), 30, "English").contains("at most 30 tags"));
        assert!(tags_system(shipped(Name::TagsSystem), 15, "English").contains("screen_time"), "weighting is still asked for");
    }

    /// The tag row is read by the person, so it is asked for in the language they set — the same value the
    /// two summary requests carry, which is why a day cannot come back with an English tag list under a
    /// Chinese paragraph.
    #[test]
    fn the_tags_are_asked_for_in_the_language_the_install_speaks() {
        let shipped_tags = shipped(Name::TagsSystem);
        assert!(shipped_tags.contains("{language}"), "the shipped text carries the slot: {shipped_tags}");
        for language in ["English", "Chinese (Simplified Han)", "Japanese"] {
            let prompt = tags_system(shipped_tags, 15, language);
            assert!(prompt.contains(&format!("on one line, in {language}, and nothing else")), "{prompt}");
            assert!(!prompt.contains("{language}"), "the slot was filled: {prompt}");
        }
        // And a template that names its own language in prose keeps it: the slot is a default for the
        // sentence, not a switch over the user's words.
        let theirs = "Give the tags in the language of the table, up to {max_tags}. {language}";
        assert_eq!(tags_system(theirs, 9, "Japanese"), "Give the tags in the language of the table, up to 9. Japanese");
        let hardcoded = "Give the tags in French, up to {max_tags}, and nothing else.";
        assert_eq!(tags_system(hardcoded, 9, "Japanese"), "Give the tags in French, up to 9, and nothing else.");
    }

    #[test]
    fn the_tag_table_keeps_upstreams_column_names() {
        let user = tags_user(shipped(Name::TagsUser), "\"Chrome - x\",\"1h2m3s\"\n");
        assert!(user.starts_with("content_page_name,screen_time\n"), "{user}");
        assert!(user.ends_with("1h2m3s\"\n"));
    }

    #[test]
    fn a_user_who_rewrites_a_prompt_still_gets_the_table_where_the_code_put_it() {
        // The contract the slot scheme exists to keep: a reworded template must not be able to drop the
        // material, and the values still land where the user's own words point at them.
        let theirs = "Look at this table and tell me what they did:\n{table}\nUp to {max_tags} tags.";
        assert_eq!(
            tags_user(theirs, "a,1\n"),
            "Look at this table and tell me what they did:\na,1\n\nUp to {max_tags} tags.",
            "the user's own words decide where the table goes, and a slot this call does not fill is left written"
        );
        assert_eq!(tags_system(theirs, 9, "English"), "Look at this table and tell me what they did:\n{table}\nUp to 9 tags.");
    }
}
