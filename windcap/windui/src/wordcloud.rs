//! Frequency to font size to a placed word, with no Python in the loop.
//!
//! `windrecorder/wordcloud.py` hands the month's whole OCR corpus to `wordcloud` plus `jieba`, gets
//! a PNG, and writes it into `result_wordcloud` — a masked, colour-sampled image that the page then
//! re-opens. What it is actually showing is a ranked list of words at sizes proportional to their
//! counts. That is a small amount of arithmetic and a collision loop, so this does it here and the
//! frame draws it, which means no file, no matplotlib, and no third-party segmenter in the binary.
//!
//! Two deliberate departures from upstream, both stated rather than hidden:
//!
//!   * **segmentation.** `jieba` is a dictionary segmenter and is not being called. Chinese runs are
//!     instead cut into overlapping *bigrams*, which is the standard no-dictionary fallback and is
//!     close in spirit to upstream's own `min_word_length=2`: the common two-character words
//!     (聊天, 文件, 新聊天's first pair) come out as units, and three-character phrases appear twice
//!     under two heads. Good enough to read at a glance, which is all a word cloud is for.
//!   * **scaling.** `python-wordcloud`'s `relative_scaling=0.4` blends rank share with frequency
//!     share. A square-root of the frequency share gives the same shape — one big word, a long tail,
//!     no cliff — without claiming to reproduce the library's pixels.

use serde::Serialize;

use std::collections::HashMap;

/// One word and how often the month's recognised text said it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudWord {
    pub text: String,
    pub count: usize,
}

/// A word the layout found room for. `box_` is `[left, top, right, bottom]` in the layout's own
/// coordinate space, which is the space the painter is given, so the drawn text lands where the
/// collision test said it would.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedWord {
    pub text: String,
    pub size: f32,
    pub box_: [f32; 4],
    /// Rank by count, so the view can shade the tail without re-sorting.
    pub rank: usize,
}

/// Is this the kind of character that has to be *cut* rather than split on whitespace.
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2A6DF)
}

/// Split a corpus into countable terms.
///
/// ASCII runs become one lowercase word each. CJK runs become overlapping bigrams. Everything else
/// — punctuation, whitespace, control bytes — is a boundary. A term shorter than two characters is
/// dropped, matching upstream's `min_word_length=2`, and so is a run of bare digits: the most
/// frequent "word" in a screen recording's OCR is a clock, and a cloud dominated by `12` and `00` is
/// a picture of nothing.
pub fn terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut latin: Vec<char> = Vec::new();
    let mut cjk: Vec<char> = Vec::new();
    let flush_latin = |latin: &mut Vec<char>, out: &mut Vec<String>| {
        if latin.len() >= 2 {
            out.push(latin.iter().collect::<String>().to_lowercase());
        }
        latin.clear();
    };
    let flush_cjk = |cjk: &mut Vec<char>, out: &mut Vec<String>| {
        // A single isolated character is dropped by the length rule; a run of n yields n-1 bigrams.
        for pair in cjk.windows(2) {
            out.push(pair.iter().collect());
        }
        cjk.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            flush_cjk(&mut cjk, &mut out);
            latin.push(c);
        } else if is_cjk(c) {
            flush_latin(&mut latin, &mut out);
            cjk.push(c);
        } else {
            flush_latin(&mut latin, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_latin(&mut latin, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out.retain(|t| t.chars().count() >= 2 && !t.chars().all(|c| c.is_ascii_digit()));
    out
}

/// A running term count, so a corpus may be fed in chunks.
///
/// `backend::word_cloud` reads a month page by page and must not hold the text, which makes an
/// accumulator — not a `Vec<&str>` argument — the right shape for this type. `tally` is the same
/// thing over one string, and both end in [`WordCounts::rank`], so there is exactly one rule for what
/// a cloud shows.
#[derive(Debug, Default, Clone)]
pub struct WordCounts {
    counts: HashMap<String, usize>,
    /// Non-fatal complaints from whatever was being read, carried so a partial cloud can say so.
    pub warnings: Vec<String>,
}

impl WordCounts {
    pub fn add(&mut self, term: &str) {
        if term.chars().count() < 2 {
            return;
        }
        *self.counts.entry(term.to_string()).or_insert(0) += 1;
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    /// Hottest first, ties broken by text.
    ///
    /// The tie rule is not decoration: an ordering that depends on `HashMap` iteration would shuffle
    /// equal-count words between the head and the tail of the list, and the layout caches its result
    /// by request id, so the cloud would visibly move on a repaint that changed nothing.
    pub fn rank(mut self, limit: usize) -> Vec<CloudWord> {
        let mut words: Vec<CloudWord> = self
            .counts
            .drain()
            .map(|(text, count)| CloudWord { text, count })
            .collect();
        words.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
        words.truncate(limit);
        words
    }
}

/// The largest and smallest type sizes the cloud is allowed to use, from upstream's own
/// `min_font_size` / `max_font_size` pair for the month mask.
pub const SIZE_RANGE: (f32, f32) = (9.0, 64.0);

/// Font size for one word's count, given the counts of the list's head and tail.
pub fn size_for(count: usize, top: usize, bottom: usize) -> f32 {
    let (small, large) = SIZE_RANGE;
    if top == 0 {
        return small;
    }
    let span = (top - bottom).max(1) as f64;
    let share = ((count - bottom) as f64 / span).clamp(0.0, 1.0).sqrt();
    small + (large - small) * share as f32
}

/// How many steps the spiral takes before giving up on a word.
const SPIRAL_STEPS: usize = 420;

/// Pack words into a box, largest first, on an Archimedean spiral from the centre.
///
/// `measure` is injected rather than calling `egui::Fonts` here for the reason `model` gives for
/// having no egui in it: this file must stay testable without a render pass, and the production
/// caller passes real glyph metrics from the frame that asked for them. Each word is measured once —
/// the spiral then only does rectangle arithmetic — which is what keeps a hundred-word layout
/// inside a frame's budget instead of proportional to steps × placed × words of glyph work.
///
/// A word that cannot be placed inside the box within `SPIRAL_STEPS` is skipped rather than
/// overflowing it: a cloud that stops short is legible, one that draws over its neighbours is not.
pub fn place(
    words: &[CloudWord],
    width: f32,
    height: f32,
    measure: &dyn Fn(&str, f32) -> (f32, f32),
) -> Vec<PlacedWord> {
    let mut out: Vec<PlacedWord> = Vec::new();
    if width <= 0.0 || height <= 0.0 || words.is_empty() {
        return out;
    }
    let top = words.first().map(|w| w.count).unwrap_or(0);
    let bottom = words.last().map(|w| w.count).unwrap_or(0);
    let (cx, cy) = (width / 2.0, height / 2.0);
    // The type scale is bounded by the canvas, not only by the corpus. SIZE_RANGE's 64 px ceiling is
    // what a large panel can carry; a word that tall in a box 240 px high leaves four lines and
    // three words in total, which reads as a broken screen rather than as a sparse cloud. Eight
    // lines tall and sixteen across is the limit that keeps the head word dominant while leaving the
    // rest of the list somewhere to stand — and this same code draws into whatever size the Stat
    // tab's panel happens to be, so the ceiling has to travel with the box.
    let max_size = (height / 8.0).min(width / 16.0).max(SIZE_RANGE.0);
    // The spiral's reach must be derived from the canvas. It used to be a fixed 0.55 px per step,
    // which tops out at a radius of about 58 px after SPIRAL_STEPS — so every cloud, however large
    // the panel, packed its words into a small disc in the middle and reported the rest as "no room".
    // Growing to the half-diagonal by the last step is what makes the whole box usable.
    let reach = (width.max(height) / 2.0).max(1.0);
    let spiral_gain = reach / (SPIRAL_STEPS as f32 * 0.25);
    for (rank, word) in words.iter().enumerate() {
        let size = size_for(word.count, top, bottom).min(max_size);
        let (w, h) = measure(&word.text, size);
        let (w, h) = (w + 2.0, h + 2.0);
        let mut placed = None;
        for step in 0..SPIRAL_STEPS {
            // Half-width on x, full on y: the vertical gain is what clears the previous line, while
            // a square spiral walks off the edge of a landscape box in one turn.
            let t = step as f32 * 0.25;
            let r = spiral_gain * t;
            let x = cx + r * t.cos() * 0.6;
            let y = cy + r * t.sin();
            if x - w / 2.0 < 0.0 || x + w / 2.0 > width || y - h / 2.0 < 0.0 || y + h / 2.0 > height {
                continue;
            }
            let candidate = [x - w / 2.0, y - h / 2.0, x + w / 2.0, y + h / 2.0];
            if out.iter().all(|p| !overlaps(p.box_, candidate)) {
                placed = Some(candidate);
                break;
            }
        }
        if let Some(box_) = placed {
            out.push(PlacedWord { text: word.text.clone(), size, box_, rank });
        }
    }
    out
}

/// Half an pixel of slack, so two words that merely touch still read as two words.
fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[2] - 0.5 && b[0] < a[2] - 0.5 && a[1] < b[3] - 0.5 && b[1] < a[3] - 0.5
}

/// A stop-word set that answers by lowercase lookup. Built by `backend`, which is the only module
/// allowed to open `wordcloud_stopword.txt`.
#[derive(Debug, Default, Clone)]
pub struct StopWords {
    words: Vec<String>,
}

impl StopWords {
    pub fn new(list: impl IntoIterator<Item = String>) -> StopWords {
        let mut words: Vec<String> = list.into_iter().map(|w| w.trim().to_lowercase()).filter(|w| !w.is_empty()).collect();
        words.sort();
        words.dedup();
        StopWords { words }
    }

    /// The callable `tally` takes. A `binary search` rather than a `HashSet` because the shipped file
    /// is nine hundred entries and this is called once per distinct term, not once per character.
    pub fn contains(&self, term: &str) -> bool {
        self.words.binary_search_by(|w| w.as_str().cmp(term)).is_ok()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed-width stand-in for `egui::Fonts`: nine units per character per em over ten.
    fn measure(text: &str, size: f32) -> (f32, f32) {
        (text.chars().count() as f32 * size * 0.9, size * 1.2)
    }

    /// The one production path: feed a corpus through `terms` into a `WordCounts`, rank it, and see
    /// what a cloud would be drawn from.
    fn count(text: &str, stop: &StopWords, limit: usize) -> Vec<CloudWord> {
        let mut counts = WordCounts::default();
        for term in terms(text) {
            if !stop.contains(&term) {
                counts.add(&term);
            }
        }
        counts.rank(limit)
    }

    #[test]
    fn an_ascii_run_is_one_lowercase_term() {
        assert_eq!(terms("Quarterly Revenue	total, TOTAL 42"), vec!["quarterly", "revenue", "total", "total"]);
    }

    /// A run of digits is the most common "word" in a screen recording — a clock — and a cloud built
    /// from it is a picture of nothing, so the length rule alone is not enough.
    #[test]
    fn bare_numbers_are_dropped_but_alphanumeric_terms_survive() {
        assert_eq!(terms("12:00:34 v2 beta"), vec!["v2", "beta"]);
    }

    #[test]
    fn chinese_is_cut_into_overlapping_bigrams() {
        assert_eq!(terms("新聊天"), vec!["新聊", "聊天"]);
        assert_eq!(terms("文件 聊天"), vec!["文件", "聊天"]);
        assert!(terms("单").is_empty(), "one character is below the length rule");
    }

    #[test]
    fn mixed_scripts_do_not_leak_across_a_boundary() {
        // The bug shape this guards: a latin buffer flushed only by a non-alphanumeric would let a CJK
        // character end a word without the run being carried, and vice versa.
        assert_eq!(terms("ChatGPT新聊天置顶"), vec!["chatgpt", "新聊", "聊天", "天置", "置顶"]);
    }

    #[test]
    fn ranking_is_by_count_then_text_and_honours_the_limit() {
        let stop = StopWords::new(Vec::new());
        let words = count("alpha beta beta gamma gamma gamma", &stop, 10);
        assert_eq!(
            words.iter().map(|w| (w.text.as_str(), w.count)).collect::<Vec<_>>(),
            vec![("gamma", 3), ("beta", 2), ("alpha", 1)]
        );
        assert_eq!(count("alpha beta beta gamma gamma gamma", &stop, 2).len(), 2);
    }

    /// The tie rule is what lets the layout be cached: an ordering that depended on `HashMap`
    /// iteration would move equal-count words between runs, and the cloud would visibly shuffle on a
    /// repaint that changed nothing about the data.
    #[test]
    fn equally_frequent_words_rank_in_text_order_every_time() {
        let stop = StopWords::new(Vec::new());
        let once = count("zebra apple mango", &stop, 10);
        let twice = count("mango apple zebra", &stop, 10);
        assert_eq!(once, twice);
        assert_eq!(once.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn stop_words_are_case_insensitive_and_trimmed() {
        let stop = StopWords::new(vec![" Login ".into(), "ACCOUNT".into()]);
        assert!(stop.contains("login") && stop.contains("account"));
        assert!(!stop.contains("twitter"));
        assert_eq!(count("login account login chat chat chat", &stop, 10), vec![CloudWord { text: "chat".into(), count: 3 }]);
    }

    #[test]
    fn the_head_of_the_list_is_the_biggest_type_and_the_tail_the_smallest() {
        let (small, large) = SIZE_RANGE;
        assert_eq!(size_for(100, 100, 1), large);
        assert!((size_for(1, 100, 1) - small).abs() < 1e-4);
        assert!(size_for(50, 100, 1) > size_for(25, 100, 1), "monotonic in the count");
        // A flat list — every word once — must not divide by zero nor blow up to the maximum.
        assert!((size_for(1, 1, 1) - small).abs() < 1e-4);
    }

    #[test]
    fn placed_words_stay_inside_the_box_and_off_each_other() {
        let words: Vec<CloudWord> = (0..12).map(|i| CloudWord { text: format!("word{i}"), count: 40 - i }).collect();
        let placed = place(&words, 420.0, 240.0, &measure);
        assert!(placed.len() >= 8, "the box is big enough for most of them: {}", placed.len());
        for p in &placed {
            assert!(p.box_[0] >= 0.0 && p.box_[2] <= 420.0 + 0.5, "{p:?}");
            assert!(p.box_[1] >= 0.0 && p.box_[3] <= 240.0 + 0.5, "{p:?}");
        }
        for (i, a) in placed.iter().enumerate() {
            for b in &placed[i + 1..] {
                assert!(!overlaps(a.box_, b.box_), "{a:?} sits on {b:?}");
            }
        }
        assert!(placed[0].size > placed[placed.len() - 1].size, "the rank order survived placement");
    }

    /// A word longer than the box is not a bug to place around: `place` must skip it and carry on,
    /// because OCR text regularly contains a two-hundred-character path.
    #[test]
    fn a_word_that_cannot_fit_is_skipped_rather_than_overflowing_the_box() {
        let words = vec![
            CloudWord { text: "impossible".repeat(20), count: 100 },
            CloudWord { text: "fits".into(), count: 50 },
        ];
        let placed = place(&words, 200.0, 100.0, &measure);
        assert_eq!(placed.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), vec!["fits"]);
    }

    #[test]
    fn an_empty_layout_ask_returns_nothing_without_dividing_by_zero() {
        assert!(place(&[], 100.0, 100.0, &measure).is_empty());
        assert!(place(&[CloudWord { text: "x".into(), count: 1 }], 0.0, 100.0, &measure).is_empty());
    }
}
