//! Text handling copied behaviour-for-behaviour from `windrecorder/utils.py`.
//!
//! These functions decide what a row *is*, not just how it looks: `clean_dirty_text` runs before the
//! similarity test and before the value is stored, so a difference here changes which frames survive
//! deduplication and what a search for last year returns. Every quirk is therefore reproduced, including
//! the ones that look like mistakes — the tests below pin them rather than fix them.

/// The punctuation upstream breaks lines on, in `utils.wrap_text_by_symbol`.
const WRAP_SYMBOLS: [&str; 7] = ["。", "！", "？", "），", "）。", "，", "．"];

/// Is this character inside the CJK block the upstream regex uses (`\u4e00-\u9fa5`)?
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fa5}')
}

/// Remove the whitespace between two runs of Chinese characters.
///
/// This is `re.sub(r"([\u4e00-\u9fa5]+)\s+([\u4e00-\u9fa5]+)", r"\1\2", text)`, and the detail that
/// matters is that Python's `re.sub` does *not* rescan what it replaced and does not backtrack into
/// an earlier group. So `"A B C"` (three single-character runs) collapses only the first gap, giving
/// `"AB C"` — the scan resumes after the `C`. A "nicer" implementation that fully joined adjacent
/// runs would produce text that differs byte-for-byte from what is already in users' databases.
fn collapse_cjk_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        match match_cjk_pair(&chars, i) {
            Some((left, gap, right, end)) => {
                // The replacement is group1 followed by group2 — the gap between them is what goes.
                out.extend(chars[i..i + left].iter());
                out.extend(chars[i + left + gap..i + left + gap + right].iter());
                i += end;
            }
            // Not a match start: emit this character and step one, exactly as the regex engine's
            // "try next position" advance does.
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Try to match `[cjk]+ [ws]+ [cjk]+` anchored at `at`.
///
/// Returns the four run lengths `(left_cjk, gap_ws, right_cjk, total)` — greedy, like the regex — or
/// `None` when no match starts here.
fn match_cjk_pair(chars: &[char], at: usize) -> Option<(usize, usize, usize, usize)> {
    let rest = &chars[at..];
    let left = rest.iter().take_while(|c| is_cjk(**c)).count();
    if left == 0 {
        return None;
    }
    let gap = rest[left..].iter().take_while(|c| c.is_whitespace()).count();
    if gap == 0 {
        return None;
    }
    let right = rest[left + gap..].iter().take_while(|c| is_cjk(**c)).count();
    if right == 0 {
        return None;
    }
    Some((left, gap, right, left + gap + right))
}

/// `utils.wrap_text_by_symbol`: newlines become spaces, a break is inserted after each sentence-ending
/// symbol, and Chinese runs are rejoined across the spaces the OCR engine inserts.
pub fn wrap_text_by_symbol(text: &str) -> String {
    let mut text = text.replace('\n', " ").replace('\r', " ");
    for symbol in WRAP_SYMBOLS {
        text = text.replace(symbol, &format!("{symbol}\n"));
    }
    collapse_cjk_spaces(&text)
}

/// `utils.merge_short_lines`: a line of `less_than` characters or fewer is glued onto its predecessor.
///
/// This is what turns the OCR engine's one-line-per-detection output back into paragraphs, and why a
/// column of short menu labels ends up as one long row rather than many tiny ones.
pub fn merge_short_lines(text: &str, less_than: usize) -> String {
    let lines = split_on_newlines(text);
    let mut merged: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        if merged.is_empty() {
            merged.push(line);
            continue;
        }
        // `len(line) <= less_than` in Python counts characters, so count characters here too — a
        // byte count would merge on ASCII and split on Chinese.
        if line.chars().count() <= less_than {
            merged.last_mut().expect("non-empty").push_str(&line);
        } else {
            merged.push(line);
        }
    }
    merged.join("\n")
}

/// `re.split(r"[\n\r]+", text)` — note that a leading separator yields a leading empty field, and an
/// empty input yields `[""]`, which is what Python's `lines[0]` then reads.
fn split_on_newlines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' || c == '\r' {
            while matches!(chars.peek(), Some('\n') | Some('\r')) {
                chars.next();
            }
            out.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    out.push(current);
    out
}

/// `utils.clean_dirty_text` — the shape every stored `ocr_text` is in.
pub fn clean_dirty_text(text: &str) -> String {
    merge_short_lines(&wrap_text_by_symbol(text), 20)
}

/// `utils.is_str_contain_list_word`: case-insensitive substring test against a list.
pub fn is_str_contain_list_word(haystack: &str, needles: &[String]) -> bool {
    let haystack = haystack.to_lowercase();
    needles.iter().any(|needle| haystack.contains(&needle.to_lowercase()))
}

/// Python's `round()` on a float: half to *even*, not half away from zero.
///
/// `round(int(frame_index) / framerate)` decides the stored `videofile_time`, and with the shipped
/// `record_framerate` of 2 every odd frame index lands exactly on `.5`. Rust's `f64::round` would send
/// those up and Python sends them to the nearest even value, so the two writers would disagree by a
/// second on alternate frames.
pub fn round_half_even(value: f64) -> i64 {
    let floor = value.floor();
    let rest = value - floor;
    if rest > 0.5 {
        floor as i64 + 1
    } else if rest < 0.5 {
        floor as i64
    } else {
        // Exactly .5 -> the even neighbour. `floor` is an integer value here.
        let f = floor as i64;
        if f % 2 == 0 {
            f
        } else {
            f + 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_symbols_get_a_line_break_after_them() {
        let cleaned = wrap_text_by_symbol("甲。乙！丙");
        assert_eq!(cleaned, "甲。\n乙！\n丙");
    }

    /// `），` sits before `，` in `symbol_list`, and the rules are a chain of `str.replace` rather than
    /// one alternation — so the comma inside the already-broken `），` collects a second newline. That
    /// doubled break is upstream's output, and `merge_short_lines` splits on `[\\n\\r]+`, which collapses
    /// it again. Asserting the real shape is what stops a tidier implementation sneaking in and
    /// changing which text a user already has stored.
    #[test]
    fn the_symbol_rules_stack_because_they_are_a_replace_chain() {
        assert_eq!(wrap_text_by_symbol("（x），y"), "（x），\n\ny");
        assert_eq!(clean_dirty_text("（x），y"), "（x），y", "the merge step undoes the doubled break");
    }

    #[test]
    fn carriage_returns_and_newlines_become_spaces_first() {
        assert_eq!(wrap_text_by_symbol("a\r\nb"), "a  b");
    }

    #[test]
    fn cjk_spaces_collapse_but_only_one_gap_per_pass() {
        assert_eq!(collapse_cjk_spaces("甲 乙"), "甲乙");
        assert_eq!(collapse_cjk_spaces("甲乙 丙丁"), "甲乙丙丁");
        // The documented no-rescan behaviour: three runs collapse the first gap only.
        assert_eq!(collapse_cjk_spaces("甲 乙 丙"), "甲乙 丙");
        // Latin is not in the block, so its word spacing survives.
        assert_eq!(collapse_cjk_spaces("Semantic satiation is a"), "Semantic satiation is a");
    }

    #[test]
    fn short_lines_are_glued_onto_their_predecessor() {
        assert_eq!(merge_short_lines("aaaaaaaaaaaaaaaaaaaaa\nbb\ncc", 20), "aaaaaaaaaaaaaaaaaaaaabbcc");
        assert_eq!(merge_short_lines("x\ny", 20), "xy");
        // An empty string is one empty line, not zero lines — `lines[0]` must exist.
        assert_eq!(merge_short_lines("", 20), "");
        assert_eq!(split_on_newlines("\na"), vec!["".to_string(), "a".to_string()]);
    }

    #[test]
    fn the_two_step_clean_matches_upstreams_composition() {
        let raw = "庞加莱复现定理\r\n在数学上，庞加莱复现定理（英语：Poincare recurrence theorem），简称为了庞加莱回归定理";
        let cleaned = clean_dirty_text(raw);
        // Line 1 is short and gets absorbed; the "，" split then leaves pieces that re-absorb.
        assert!(cleaned.starts_with("庞加莱复现定理在数学上，"), "{cleaned:?}");
        assert!(!cleaned.contains('\r'), "carriage returns are gone before merging");
    }

    #[test]
    fn exclusion_test_is_case_insensitive_substring() {
        let needles = ["Windrecorder".to_string(), "KeePass".to_string()];
        assert!(is_str_contain_list_word("WINDRECORDER - Settings", &needles));
        assert!(is_str_contain_list_word("keepass safe", &needles));
        assert!(!is_str_contain_list_word("notepad", &needles));
        assert!(!is_str_contain_list_word("anything", &[]));
    }

    /// Quirk kept on purpose: an empty entry in `exclude_words` makes every row match, and upstream
    /// behaves that way. A "fix" here would change which rows an existing library contains.
    #[test]
    fn an_empty_exclusion_entry_matches_everything_just_like_upstream() {
        assert!(is_str_contain_list_word("x", &["".to_string()]));
    }

    #[test]
    fn rounding_follows_python_not_rust() {
        assert_eq!(round_half_even(0.5), 0, "Python rounds .5 to even, Rust would give 1");
        assert_eq!(round_half_even(1.5), 2);
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(3.5), 4);
        assert_eq!(round_half_even(2.4), 2);
        assert_eq!(round_half_even(2.6), 3);
        assert_eq!(round_half_even(0.0), 0);
        // The case the pipeline actually hits: frame index / 2 fps, where half of the frames land
        // exactly on .5 and must go to the even neighbour or the row is a second off.
        assert_eq!(
            (0..12i64).map(|f| round_half_even(f as f64 / 2.0)).collect::<Vec<_>>(),
            vec![0, 0, 1, 2, 2, 2, 3, 4, 4, 4, 5, 6]
        );
    }
}
