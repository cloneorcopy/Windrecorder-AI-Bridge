//! The window-title normaliser, ported from `record_wintitle.optimize_wintitle_name`.
//!
//! It lives in the bridge rather than in `wind-base` only because `wind-base` does not have it yet:
//! the recorder writes the *raw* title into the index and every consumer normalises on read, so
//! this is a reader's function. It must not become a second opinion. The bridge's whole value is
//! that the string an agent reads here is the string the web UI's "where the time went" list shows
//! for the same frame — two normalisers in one product would split a single afternoon across two
//! buckets, which is exactly the bug this file exists to avoid. Move it down to `wind-base` the
//! moment a second reader needs it.
//!
//! Ported from `record_wintitle.py:189` *including its rule order*, because the order is
//! observable: upstream removes the asterisks before the generic `(123)` badge, so `"Foo * (3)"`
//! becomes `"Foo"` and not `"Foo )"`. A tidier ordering is a different function.
//!
//! There is no regex crate in the workspace lock, so the rules are hand-written scanners. Each is
//! a non-overlapping left-to-right substitution — what `re.sub` does — and the tests quote the
//! Python's own output.

/// Byte length of `123)` at the start of `run`, or `None` — the inside of `\(\d+\)` once the
/// opening paren has been matched by the rule's own literal.
///
/// The split is what a two-piece rule needs: `candidate_rule` hands `follows` the text *after* its
/// needle, so a helper that expected to see the paren itself would match nothing and every badge in
/// the title would survive cleaning.
fn close_after_digits(run: &str) -> Option<usize> {
    let digits = run.len() - run.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    run[digits..].starts_with(')').then_some(digits + 1)
}

/// `\(\d+\)` matched from the paren, for a rule whose needle is not the paren.
fn paren_digits_len(run: &str) -> Option<usize> {
    close_after_digits(run.strip_prefix('(')?).map(|inner| inner + 1)
}

/// `Option<(start, end, replacement)>` for the next real match in `run`.
type Rule<'a> = &'a mut dyn FnMut(&str) -> Option<(usize, usize, &'static str)>;

/// A rule built from a fixed literal plus a test on what follows it.
///
/// The walk over *every* candidate is what makes this different from a single `find`. In
/// `"a - b - (3)"` the first `" - "` is an ordinary separator and only the second precedes a badge;
/// a rule that gave up after the first would clean nothing, and the same window would then land in
/// two different buckets depending on where its title happened to put a hyphen.
fn candidate_rule(needle: &'static str, replacement: &'static str, follows: fn(&str) -> Option<usize>) -> impl FnMut(&str) -> Option<(usize, usize, &'static str)> {
    move |run: &str| {
        let mut from = 0;
        while let Some(hit) = run[from..].find(needle) {
            let at = from + hit;
            if let Some(extra) = follows(&run[at + needle.len()..]) {
                return Some((at, at + needle.len() + extra, replacement));
            }
            from = at + 1;
        }
        None
    }
}

/// A rule that fires on an exact string and nothing else.
fn literal_rule(needle: &'static str, replacement: &'static str) -> impl FnMut(&str) -> Option<(usize, usize, &'static str)> {
    move |run: &str| run.find(needle).map(|at| (at, at + needle.len(), replacement))
}

/// One `re.sub` pass.
fn substitute(source: &str, find: Rule) -> String {
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    while cursor < source.len() {
        match find(&source[cursor..]) {
            // `end > start` cannot fail for any rule below — each requires a literal character —
            // but a zero-length match would otherwise spin this loop forever.
            Some((start, end, replacement)) if end > start => {
                out.push_str(&source[cursor..cursor + start]);
                out.push_str(replacement);
                cursor += end;
            }
            Some(_) => {
                out.push_str(&source[cursor..cursor + 1]);
                cursor += 1;
            }
            // The rule reports no match anywhere in what is left, so the tail is already final.
            // Copying it whole is also what keeps a long title linear instead of quadratic.
            None => {
                out.push_str(&source[cursor..]);
                break;
            }
        }
    }
    out
}

fn run_rule(source: &str, mut rule: impl FnMut(&str) -> Option<(usize, usize, &'static str)>) -> String {
    substitute(source, &mut rule)
}

/// `re.sub(prefix + r"\(\d+\)", "", text)` — the two telegram unread-count spellings, which differ
/// only in whether the dash between the spaces is U+2013 or a hyphen.
fn drop_paren_after(source: &str, prefix: &'static str) -> String {
    run_rule(source, candidate_rule(prefix, "", paren_digits_len))
}

/// `re.sub(r"^\(\d+\) ", "", text)` — anchored, so it can only fire at position zero.
fn drop_leading_paren(source: &str) -> String {
    match paren_digits_len(source) {
        Some(len) if source[len..].starts_with(' ') => String::from(&source[len + 1..]),
        _ => String::from(source),
    }
}

/// `re.sub(r"\(\d+\)", "", text)` — the generic badge, at any position.
fn drop_badges(source: &str) -> String {
    run_rule(source, candidate_rule("(", "", close_after_digits))
}

/// `re.sub(r" and \d+ more pages", "", text)` — the Microsoft Edge tab count.
fn drop_extra_tabs(source: &str) -> String {
    run_rule(
        source,
        candidate_rule(" and ", "", |rest| {
            let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            (digits > 0 && rest[digits..].starts_with(" more pages")).then_some(digits + " more pages".len())
        }),
    )
}

/// The three saved-state asterisk rules, in the Python's order. Three rules, not one pattern,
/// because `" * "`, `" *"` and `"* "` are three different edits — and each replaces with a space
/// rather than nothing, which is what stops `"Blender* a.blend"` becoming `"Blendera.blend"`.
fn drop_asterisks(source: &str) -> String {
    let source = run_rule(source, literal_rule(" * ", " "));
    let source = run_rule(&source, literal_rule(" *", " "));
    run_rule(&source, literal_rule("* ", " "))
}

/// The shipped normaliser. `None` only when nothing survives the cleaning: an empty bucket in a
/// usage table is not a window the user was ever in.
///
/// The literal strings a missing database value reads as (`"None"`, `"nan"`, from the pandas path
/// that wrote them) are filtered by the caller that groups titles, not here — `optimize_wintitle_
/// name` does not filter them and neither does this, so the two cannot drift apart over whether a
/// window genuinely called *None* is a window.
pub fn normalize(title: &str) -> Option<String> {
    let mut text = drop_paren_after(title, " \u{2013} ");
    text = drop_paren_after(&text, " - ");
    text = drop_leading_paren(&text);
    text = drop_extra_tabs(&text);
    text = run_rule(&text, literal_rule(" - Personal", ""));
    text = drop_asterisks(&text);
    text = drop_badges(&text);
    text = text.trim().to_string();
    text = drop_asterisks(&text);
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation below is the value the running Python returns for the same input, taken
    /// from the examples in `record_wintitle.py`'s own comments.
    #[test]
    fn telegram_unread_counts_are_removed() {
        assert_eq!(normalize("(1) 大懒趴俱乐部 – (283859)").as_deref(), Some("大懒趴俱乐部"));
        assert_eq!(normalize("ChatGPT - (12)").as_deref(), Some("ChatGPT"));
        assert_eq!(normalize("(7) 新聊天 - ChatGPT").as_deref(), Some("新聊天 - ChatGPT"));
    }

    #[test]
    fn edge_tab_counts_and_profile_suffix_are_removed() {
        assert_eq!(
            normalize("XXXX and 64 more pages - Personal - Microsoft Edge").as_deref(),
            Some("XXXX - Microsoft Edge")
        );
        assert_eq!(normalize("ChatGPT - Personal - Microsoft Edge").as_deref(), Some("ChatGPT - Microsoft Edge"));
    }

    #[test]
    fn unsaved_document_asterisks_collapse_to_a_single_space() {
        assert_eq!(normalize("Blender* a.blend").as_deref(), Some("Blender a.blend"));
        assert_eq!(normalize("Blender * a.blend").as_deref(), Some("Blender a.blend"));
        assert_eq!(normalize("notepad *").as_deref(), Some("notepad"));
        // Upstream leaves a double space here; reproducing that is the compatibility, not a bug.
        assert_eq!(normalize("a * * b").as_deref(), Some("a  b"));
    }

    #[test]
    fn badge_rules_run_after_the_asterisks_because_the_order_is_observable() {
        assert_eq!(normalize("(12) Home / X.com").as_deref(), Some("Home / X.com"));
        assert_eq!(normalize("Foo * (3)").as_deref(), Some("Foo"));
    }

    #[test]
    fn a_title_that_is_only_decoration_normalises_to_nothing() {
        assert_eq!(normalize("   "), None);
        assert_eq!(normalize("(12)"), None);
        assert_eq!(normalize("* "), None);
    }

    /// The behaviour a single-try search gets wrong: the qualifier sits on the *second* candidate.
    #[test]
    fn a_match_is_found_at_whichever_candidate_qualifies_not_just_the_first() {
        assert_eq!(normalize("a - b - (3)").as_deref(), Some("a - b"));
        assert_eq!(normalize("(1) one and 2 more pages - (3)").as_deref(), Some("one"));
    }

    /// The grouping that turns frames into buckets depends on this: a title that changed shape
    /// between two reads would split one window across two rows of the same table.
    #[test]
    fn normalizing_twice_changes_nothing() {
        for title in [
            "(1) 大懒趴俱乐部 – (283859)",
            "Blender* a.blend",
            "ChatGPT - Personal - Microsoft Edge",
            "Q3 (2026) review - Excel",
            "Home / X.com",
            "notepad *",
            "a - b - (3)",
        ] {
            let once = normalize(title);
            let twice = once.as_deref().and_then(normalize);
            assert_eq!(once, twice, "not idempotent for {title:?}");
        }
    }

    #[test]
    fn digits_that_are_not_a_badge_survive() {
        assert_eq!(normalize("Chapter (1 of 12) - Reader").as_deref(), Some("Chapter (1 of 12) - Reader"));
        assert_eq!(normalize("127.0.0.1 - Chrome").as_deref(), Some("127.0.0.1 - Chrome"));
        assert_eq!(normalize("and 5 more pages").as_deref(), Some("and 5 more pages"), "no leading space, no match");
        assert_eq!(normalize("()"), Some("()".to_string()), "an empty group is not a count");
        assert_eq!(normalize("v(1.2) notes").as_deref(), Some("v(1.2) notes"));
    }

    #[test]
    fn the_dash_rules_are_exactly_two_characters_wide() {
        // An em dash is not the telegram shape, and an unspaced hyphen is not either — but the
        // generic badge rule still fires, which is the behaviour being pinned here.
        assert_eq!(normalize("A — (3) B").as_deref(), Some("A —  B"));
        assert_eq!(normalize("A-(3)B").as_deref(), Some("A-B"));
    }

    /// Titles are normalised once per row and a busy month is a hundred thousand rows, so the
    /// scanner must not go quadratic on a title built out of repeated separators.
    #[test]
    fn a_long_title_with_many_candidates_stays_linear() {
        let title = "x - ".repeat(2000) + "(9) tail";
        let started = std::time::Instant::now();
        let cleaned = normalize(&title).expect("survives the rules");
        let elapsed = started.elapsed();
        assert!(!cleaned.contains("(9)"), "the badge was not removed");
        assert!(cleaned.ends_with("tail"), "{cleaned}");
        assert!(elapsed.as_millis() < 250, "{:?} for a {} byte title", elapsed, title.len());
    }
}
