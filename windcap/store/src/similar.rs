//! Shape-similar Chinese character expansion, the reason a search for `也化` also finds `量化`.
//!
//! `config_src/similar_CN_characters.txt` groups the characters that the OCR engine reliably
//! confuses with one another — 1008 groups in the shipped file, one per line, separated by the
//! full-width comma `，`. Upstream expands a query token into the Cartesian product of the
//! per-character groups and ORs the variants into the `LIKE` clause, capped at 100 combinations so
//! that a long token cannot build a monster query.
//!
//! This is a recall device, not a ranking device: every variant matches equally, and the order they
//! come back in is irrelevant. Unlike the Python version, which round-trips through `set()` and so
//! yields a different order on every run, the expansion here is sorted — which is what makes the SQL
//! builder testable and a saved query reproducible.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Upstream's cap on generated variants before it gives up and searches the literal token.
pub const MAX_VARIANTS: usize = 100;

/// The full-width comma the group file separates on. An ordinary `,` is a character, not a
/// separator, which is the trap in this format.
const GROUP_SEPARATOR: char = '，';

#[derive(Debug, Default, Clone)]
pub struct SimilarChars {
    /// A character maps to every alternative the OCR may have produced for it, including the
    /// character itself. Keys are single characters because that is the unit a lookup happens in.
    alternatives: BTreeMap<char, Vec<String>>,
}

impl SimilarChars {
    /// Read `config_src/similar_CN_characters.txt`.
    ///
    /// A missing file is not fatal to the application: search still works, it simply stops fuzzing
    /// glyph confusion, so a caller may fall back to [`SimilarChars::default`].
    pub fn load(path: &Path) -> std::io::Result<SimilarChars> {
        Ok(SimilarChars::parse(&std::fs::read_to_string(path)?))
    }

    pub fn parse(text: &str) -> SimilarChars {
        let mut merged: BTreeMap<char, BTreeSet<String>> = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let group: Vec<String> = line.split(GROUP_SEPARATOR).map(str::to_string).filter(|c| !c.is_empty()).collect();
            if group.is_empty() {
                continue;
            }
            for cell in &group {
                // Membership is exact: only a cell that *is* the character pulls in the group, so a
                // two-character cell (there is one in the shipped file) is a substitute, never a key.
                let mut chars = cell.chars();
                let single = chars.next().filter(|_| chars.next().is_none());
                if let Some(ch) = single {
                    merged.entry(ch).or_default().extend(group.iter().cloned());
                }
            }
        }
        SimilarChars { alternatives: merged.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.alternatives.is_empty()
    }

    /// How many characters this table can confuse, which is how a caller notices a truncated file.
    pub fn covered_characters(&self) -> usize {
        self.alternatives.len()
    }

    /// The substitutes for one character, or the character itself when it is not confusable.
    pub fn alternatives_for(&self, ch: char) -> Vec<String> {
        match self.alternatives.get(&ch) {
            Some(group) if !group.is_empty() => group.clone(),
            _ => vec![ch.to_string()],
        }
    }

    /// Every string a token may have been misread as, capped at [`MAX_VARIANTS`] combinations.
    ///
    /// Returns the literal token alone when the product would exceed the cap — the same escape hatch
    /// `generate_similar_ch_strings` uses, so behaviour on a long query is unchanged.
    pub fn variants_for_token(&self, token: &str, cap: usize) -> Vec<String> {
        let chars: Vec<char> = token.chars().collect();
        if chars.is_empty() {
            return vec![token.to_string()];
        }
        let per_char: Vec<Vec<String>> = chars.iter().map(|c| self.alternatives_for(*c)).collect();

        let mut total: usize = 1;
        for group in &per_char {
            total = total.saturating_mul(group.len());
            if total > cap {
                return vec![token.to_string()];
            }
        }

        let mut out = Vec::with_capacity(total);
        expand(&per_char, 0, String::new(), &mut out);
        out.sort();
        out.dedup();
        out
    }
}

fn expand(per_char: &[Vec<String>], depth: usize, current: String, out: &mut Vec<String>) {
    if depth == per_char.len() {
        out.push(current);
        return;
    }
    for cell in &per_char[depth] {
        let mut next = current.clone();
        next.push_str(cell);
        expand(per_char, depth + 1, next, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped file, if the checkout has it: a parse test against real data is worth more than
    /// any number of assertions against a fixture I wrote myself.
    ///
    /// Located through [`wind_base::install`] rather than by naming a directory, so this still reads
    /// the payload's `config_src/` and would equally have found the legacy one.
    fn shipped() -> Option<SimilarChars> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .parent()?
            .to_path_buf();
        SimilarChars::load(&wind_base::install::config_src_file(&root, "similar_CN_characters.txt")).ok()
    }

    fn table() -> SimilarChars {
        SimilarChars::parse("也，以，己，巳\r\n天，夫，失\r\n干，千\r\n")
    }

    #[test]
    fn a_known_confusion_expands_to_its_group() {
        let t = table();
        assert_eq!(t.variants_for_token("也", 100), vec!["也", "以", "己", "巳"]);
        assert_eq!(t.alternatives_for('天'), vec!["天", "夫", "失"]);
        assert_eq!(t.covered_characters(), 9);
    }

    #[test]
    fn an_unlisted_character_is_left_alone() {
        let t = table();
        assert_eq!(t.variants_for_token("z", 100), vec!["z"]);
    }

    /// Two characters, three and two alternatives: six products, in a stable order.
    #[test]
    fn multi_character_tokens_take_the_cartesian_product() {
        let t = table();
        let got = t.variants_for_token("天干", 100);
        assert_eq!(got.len(), 6, "three alternatives for 天 times two for 干");
        assert!(got.contains(&"天干".to_string()));
        assert!(got.contains(&"失千".to_string()));
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted, "the order must be deterministic");
    }

    #[test]
    fn an_over_complex_token_falls_back_to_the_literal() {
        let t = table();
        // 4^5 == 1024 > 100: upstream logs and gives up, and so does this.
        assert_eq!(t.variants_for_token("也也也也也", 100), vec!["也也也也也".to_string()]);
        // Three characters of a four-wide group is 64, just under the cap.
        assert_eq!(t.variants_for_token("也也也", 100).len(), 64);
    }

    #[test]
    fn an_empty_table_behaves_like_the_search_being_disabled() {
        let t = SimilarChars::default();
        assert!(t.is_empty());
        assert_eq!(t.variants_for_token("也好", 100), vec!["也好"]);
    }

    #[test]
    fn a_two_character_cell_is_a_substitute_never_a_key() {
        let t = SimilarChars::parse("一，二三\r\n");
        assert_eq!(t.variants_for_token("一", 100), vec!["一", "二三"]);
        assert_eq!(t.variants_for_token("二", 100), vec!["二"], "looking up inside a cell must not match");
    }

    #[test]
    fn malformed_lines_are_skipped_not_guessed_at() {
        let t = SimilarChars::parse("\n\n也，以\n，，己\n");
        assert_eq!(t.variants_for_token("也", 100), vec!["也", "以"]);
    }

    #[test]
    fn an_empty_token_does_not_panic() {
        assert_eq!(table().variants_for_token("", 100), vec![""]);
    }

    #[test]
    fn the_shipped_table_loads_and_confuses_the_characters_it_claims_to() {
        let Some(t) = shipped() else {
            eprintln!("skipping: config_src/similar_CN_characters.txt not found");
            return;
        };
        assert!(t.covered_characters() > 900, "the shipped file groups far more than that");
        // 建 and 健 are a classic OCR confusion; if the file ever stops covering it, search recall
        // quietly drops and this is where we find out.
        let group = t.alternatives_for('建');
        assert!(group.len() > 1, "建 has no alternatives in the table: {group:?}");
        assert!(group.iter().any(|g| g == "建"), "a character's alternatives must include itself");
        // Every group is capped, so a two-character query never exceeds the product of two groups.
        assert!(t.variants_for_token("建", 100).len() <= MAX_VARIANTS);
    }
}
