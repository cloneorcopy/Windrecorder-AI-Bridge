//! Where a result's text stops being a blob and starts being an answer.
//!
//! The WebUI "highlighted" the matched terms by drawing a separate picture of the OCR bounding
//! boxes (`ocr_res_position_visualization`), which needed the screenshot on disk and a coordinate
//! transform, and showed nothing at all when the row came from the video path. This is the honest
//! version: the label itself is split into runs, each with its own colour, so the term the user
//! typed is visibly the reason the row appeared. It is a real `epaint` construct — a `LayoutJob`
//! with per-section formats — not markup inside one string.
//!
//! Runs are computed in byte offsets into the *lowercased* text with a map back to the original
//! offsets, because a naive `to_lowercase()` of both sides silently shifts indices for the handful
//! of Unicode characters whose lowercase form is longer than the character itself. The map makes
//! every split point a real character boundary, so slicing cannot panic mid-row.

use wind_store::similar::{SimilarChars, MAX_VARIANTS};

/// One maximal stretch of text with a single appearance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub matched: bool,
}

/// Everything a search can have matched, as far as the highlighter is concerned.
///
/// When glyph fuzzing is on, a row may have been found through a variant the user never typed
/// (`也化` → `量化`); highlighting only the literal token would then show a row with no visible
/// reason for being there, which reads as a bug in the search rather than in the OCR.
pub fn terms_for(tokens: &[String], similar: Option<&SimilarChars>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in tokens {
        let mut parts: Vec<String> = split_inner_hyphens(token);
        if parts.is_empty() {
            parts.push(token.clone());
        }
        match similar {
            Some(table) => {
                for part in &parts {
                    for variant in table.variants_for_token(part, MAX_VARIANTS) {
                        push(&mut out, &variant);
                    }
                }
            }
            None => {
                for part in &parts {
                    push(&mut out, part);
                }
            }
        }
    }
    out
}

fn push(out: &mut Vec<String>, term: &str) {
    let trimmed = term.trim();
    if !trimmed.is_empty() && !out.iter().any(|t| t == trimmed) {
        out.push(trimmed.to_string());
    }
}

/// `wind_store::search::Query`'s private `(?\w)-(?=\w)` rule, restated so the highlighter splits
/// `self-help` into the same two terms the SQL matched. Kept in sync by the shared test vectors.
fn split_inner_hyphens(term: &str) -> Vec<String> {
    let chars: Vec<char> = term.chars().collect();
    let mut out = vec![String::new()];
    for (i, ch) in chars.iter().enumerate() {
        let is_word_sep = *ch == '-'
            && i > 0
            && i + 1 < chars.len()
            && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')
            && (chars[i + 1].is_alphanumeric() || chars[i + 1] == '_');
        if is_word_sep {
            out.push(String::new());
        } else {
            out.last_mut().unwrap().push(*ch);
        }
    }
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

/// Split `text` into runs, marking every stretch that matches one of `terms`.
///
/// Overlapping hits are merged rather than nested: two runs of the same colour drawn on top of each
/// other would just double-blend, and a term that matches inside another is one highlight.
pub fn runs(text: &str, terms: &[String]) -> Vec<Run> {
    let ranges = match_ranges(text, terms);
    if ranges.is_empty() {
        return vec![Run { text: text.to_string(), matched: false }];
    }
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in ranges {
        if start > cursor {
            out.push(Run { text: text[cursor..start].to_string(), matched: false });
        }
        out.push(Run { text: text[start..end].to_string(), matched: true });
        cursor = end;
    }
    if cursor < text.len() {
        out.push(Run { text: text[cursor..].to_string(), matched: false });
    }
    out
}

/// Byte ranges of every hit, merged and in order.
pub fn match_ranges(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let lowered = text.to_lowercase();
    let bounds = char_bounds(text, &lowered);
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        let needle = term.to_lowercase();
        if needle.is_empty() {
            continue;
        }
        let mut from = 0usize;
        while let Some(at) = lowered[from..].find(&needle) {
            let start = map_offset(&bounds, &lowered, from + at);
            let end = map_offset(&bounds, &lowered, from + at + needle.len());
            if end > start {
                hits.push((start, end));
            }
            from += at + needle.len().max(1);
        }
    }
    hits.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(hits.len());
    for (start, end) in hits {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// For each byte of `lowered`, the byte offset in `text` of the character it came from.
fn char_bounds(text: &str, lowered: &str) -> Vec<usize> {
    let mut bounds = Vec::with_capacity(lowered.len() + 1);
    for (at, ch) in text.char_indices() {
        // Byte width of this character's lowercase form. `char::to_lowercase` is an iterator and
        // its `len` counts *characters*, so a three-byte CJK ideograph would otherwise contribute
        // one entry and shift every offset after it — which is invisible in ASCII and wrong in the
        // language the index is mostly written in.
        let width: usize = ch.to_lowercase().map(|c| c.len_utf8()).sum();
        for _ in 0..width {
            bounds.push(at);
        }
    }
    bounds.push(text.len());
    bounds
}

/// A lowercase-text byte offset → the original-text byte offset, snapped to a character boundary.
fn map_offset(bounds: &[usize], lowered: &str, at: usize) -> usize {
    if at >= bounds.len() {
        return bounds.last().copied().unwrap_or(lowered.len());
    }
    bounds[at]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matched_terms_become_their_own_runs_in_order() {
        let out = runs("quarterly revenue for Q3", &["revenue".to_string(), "q3".to_string()]);
        assert_eq!(
            out,
            vec![
                Run { text: "quarterly ".into(), matched: false },
                Run { text: "revenue".into(), matched: true },
                Run { text: " for ".into(), matched: false },
                Run { text: "Q3".into(), matched: true },
            ]
        );
    }

    #[test]
    fn matching_is_case_insensitive_but_the_original_casing_survives() {
        let out = runs("ChatGPT and ChatGPT", &["chatgpt".to_string()]);
        assert_eq!(out[0], Run { text: "ChatGPT".into(), matched: true });
        assert_eq!(out.iter().filter(|r| r.matched).count(), 2);
        assert_eq!(out.into_iter().map(|r| r.text).collect::<String>(), "ChatGPT and ChatGPT");
    }

    #[test]
    fn multibyte_text_is_split_on_character_boundaries() {
        let text = "季度复盘 Q3 revenue 收入";
        let out = runs(text, &["收入".to_string(), "revenue".to_string()]);
        assert_eq!(
            out.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["季度复盘 Q3 ", "revenue", " ", "收入"]
        );
        assert_eq!(out.iter().filter(|r| r.matched).map(|r| r.text.as_str()).collect::<Vec<_>>(), vec![
            "revenue", "收入"
        ]);
    }

    #[test]
    fn overlapping_hits_merge_into_one_run() {
        let out = runs("revenue forecast", &["revenue".to_string(), "venu".to_string()]);
        assert_eq!(out.len(), 2, "one merged highlight plus the tail");
        assert_eq!(out[0], Run { text: "revenue".into(), matched: true });
    }

    #[test]
    fn a_text_with_no_hit_is_a_single_unmatched_run() {
        assert_eq!(runs("nothing here", &["absent".to_string()]), vec![Run { text: "nothing here".into(), matched: false }]);
        assert_eq!(runs("", &[]), vec![Run { text: String::new(), matched: false }]);
    }

    #[test]
    fn hyphenated_terms_are_split_the_way_the_query_builder_splits_them() {
        let terms = terms_for(&["self-help".to_string()], None);
        assert_eq!(terms, vec!["self".to_string(), "help".to_string()]);
        let out = runs("read the self-help manual", &terms);
        assert_eq!(out.iter().filter(|r| r.matched).map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["self", "help"]);
    }

    #[test]
    fn terms_for_a_fuzzy_table_carries_the_variants_too() {
        let table = SimilarChars::parse("收，改\n");
        let plain = terms_for(&["改入".to_string()], None);
        assert_eq!(plain, vec!["改入"]);
        let fuzzy = terms_for(&["改入".to_string()], Some(&table));
        assert!(fuzzy.contains(&"改入".to_string()), "{fuzzy:?}");
        assert!(fuzzy.contains(&"收入".to_string()), "the variant that actually matched must highlight too");
    }
}

#[cfg(test)]
mod dbg_tests {
    use super::*;
    #[test]
    fn debug_multibyte() {
        let text = "季度复盘 Q3 revenue 收入";
        println!("ranges = {:?}", match_ranges(text, &["收入".to_string(), "revenue".to_string()]));
        println!("runs = {:?}", runs(text, &["revenue".to_string()]));
        println!("bounds = {:?}", char_bounds(text, &text.to_lowercase()));
    }
}
