//! The content digests that make "there is a summary" mean something.
//!
//! 64-bit FNV-1a, printed as 16 hex digits — the same shape `windai`'s tag cache uses for its own
//! staleness check, chosen because it is two lines, has no dependency, and is being used to decide
//! *whether to spend an API call*, not to resist an adversary. A collision costs one redundant
//! request; it does not cost correctness, and nothing in this crate trusts a digest to be unfaked.
//!
//! It lives here rather than reaching into `wind_ai::hashing` because this crate must not depend on
//! the AI binary's crate (that dependency points the other way: `windai` will call this one). If a
//! third copy of `fnv1a64` ever appears, that is the moment to lift both into `wind-base`.

/// The separator between the fields of one frame inside the digested sequence. A NUL cannot occur in
/// window titles or OCR text, which is what makes it safe as a boundary that cannot be forged by
/// content — a text containing `|` must not shift a title into the URL slot.
const FIELD: char = '\u{0}';
/// The separator between frames, for the same reason.
const RECORD: char = '\u{1}';

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn hex64(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a64(bytes))
}

/// Digest a free-form string. Used for the *current* prompt text, and for anything else whose shape
/// the caller has already decided.
pub fn of_text(text: &str) -> String {
    hex64(text.as_bytes())
}

/// One frame's contribution, in the exact field order the digest is defined over.
fn frame_line(timestamp: i64, title: &str, url: &str, text: &str) -> String {
    format!("{timestamp}{FIELD}{title}{FIELD}{url}{FIELD}{text}")
}

/// The `source_fingerprint` of a stretch: over the whole (time, title, URL, OCR text) sequence, in
/// index order.
///
/// Order is included deliberately. The rows come out of `read::rows_in_window` sorted by
/// `(videofile_time, rowid)`, which is stable, so the same content digests the same twice; and a
/// re-index that *reorders* rows is a change in what the screen record says, so it should invalidate.
pub fn of_frames<I, T, U, X>(frames: I) -> String
where
    I: IntoIterator<Item = (i64, T, U, X)>,
    T: AsRef<str>,
    U: AsRef<str>,
    X: AsRef<str>,
{
    let mut joined = String::new();
    for (index, (timestamp, title, url, text)) in frames.into_iter().enumerate() {
        if index > 0 {
            joined.push(RECORD);
        }
        joined.push_str(&frame_line(timestamp, title.as_ref(), url.as_ref(), text.as_ref()));
    }
    of_text(&joined)
}

/// The `source_fingerprint` of a day's summary: over the list of (segment key, that segment's own
/// fingerprint) pairs it was written from.
///
/// Not over the segment *texts*: those are already covered by each segment's own digest, and covering
/// them again would make a single changed paragraph in one stretch invalidate the day for a second,
/// unrelated reason. What this answers is "did the set of inputs to that daily request survive".
pub fn of_day_inputs<I, K, F>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, F)>,
    K: AsRef<str>,
    F: AsRef<str>,
{
    let joined = pairs
        .into_iter()
        .map(|(key, fingerprint)| format!("{}{FIELD}{}", key.as_ref(), fingerprint.as_ref()))
        .collect::<Vec<_>>()
        .join(&RECORD.to_string());
    of_text(&joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stable_known_value_pins_the_algorithm() {
        // Changing the hash means every stored fingerprint on every user's disk silently stops
        // matching, and the whole library re-summarises itself. This asserts the arithmetic itself,
        // not just that it is deterministic.
        assert_eq!(hex64(b""), "cbf29ce484222325", "the offset basis, unchanged");
        assert_eq!(hex64(b"abc"), "e71fa2190541574b");
    }

    #[test]
    fn two_identical_frame_sequences_digest_identically() {
        let a = of_frames([(10, "t", "u", "text"), (11, "t2", "", "more")]);
        let b = of_frames([(10, "t", "u", "text"), (11, "t2", "", "more")]);
        assert_eq!(a, b);
    }

    #[test]
    fn one_changed_character_changes_the_digest() {
        let before = of_frames([(10, "Qoder", "", "the plan is done")]);
        let after = of_frames([(10, "Qoder", "", "the plan is gone")]);
        assert_ne!(before, after, "an edit to the screen text must not read as still-summarised");
    }

    #[test]
    fn fields_cannot_bleed_into_each_other() {
        // The failure this guards: a title ending in the separator plus a URL, versus a title that
        // simply contains the separator. Content must not be able to move a value between slots.
        let left = of_frames([(10, "a", "b", "c")]);
        let right = of_frames([(10, "a\u{0}b", "", "c")]);
        assert_ne!(left, right);
    }

    #[test]
    fn frame_order_is_part_of_the_content() {
        let one = of_frames([(10, "a", "", "first"), (11, "b", "", "second")]);
        let other = of_frames([(11, "b", "", "second"), (10, "a", "", "first")]);
        assert_ne!(one, other);
    }

    #[test]
    fn a_day_digest_moves_when_a_segment_digest_moves() {
        let before = of_day_inputs([("2026-09-27_10-00-00", "aaaa"), ("2026-09-27_11-00-00", "bbbb")]);
        let after = of_day_inputs([("2026-09-27_10-00-00", "aaaa"), ("2026-09-27_11-00-00", "cccc")]);
        assert_ne!(before, after);
        let added = of_day_inputs([
            ("2026-09-27_10-00-00", "aaaa"),
            ("2026-09-27_11-00-00", "bbbb"),
            ("2026-09-27_12-00-00", "dddd"),
        ]);
        assert_ne!(before, added, "a stretch added to the day invalidates what was written from it");
    }
}
