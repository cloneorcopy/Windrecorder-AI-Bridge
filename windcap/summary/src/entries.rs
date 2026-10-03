//! The two stored shapes, as types rather than as field-name soup.
//!
//! # Why structs and not `serde_json::Value`
//!
//! The bridge speaks `Value` because it renders whatever a handler gives it; this crate is the side
//! that *decides* whether a day is complete, and that decision reads four fields on every entry.
//! Hand-rolling `map.get("start").and_then(Value::as_i64)` in three call sites is how one of them
//! ends up defaulting a missing `start` to zero and quietly claiming a segment with no beginning.
//! Deriving `Deserialize` with `#[serde(default)]` only where absence is genuinely a state makes the
//! field names one list, and the required-vs-optional split becomes a compile-time fact.
//!
//! # What is deliberately absent
//!
//! No `id`, no schema version, no per-entry file offset, no deletion tombstone. A day file that will
//! not parse is reported as unreadable and left alone ([`crate::files::DayMap::readable`]) rather than
//! partially salvaged, because half a day's summaries shown as a whole day is the failure this crate
//! exists to avoid; and there is no version field because the shape has one writer, and `forget` and
//! `expire` prune entries rather than evolving them.

use serde::{Deserialize, Serialize};

/// What one recorded stretch was about, as written by either producer.
///
/// `start`, `end`, `frames` and `ocr_chars` are *not* the caller's to claim: the bridge and `windai`
/// both fill them from the index at write time, so a summary can never describe a window that was
/// never recorded, and the numbers a reader sees are the numbers the database holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodSummary {
    /// The prose. Stored byte for byte: no truncation, no character cap, no rewriting of newlines.
    pub text: String,
    pub start: i64,
    pub end: i64,
    pub frames: usize,
    pub ocr_chars: usize,
    /// Wall clock, `YYYY-MM-DD HH:MM:SS` on the stored axis, as [`wind_base::clock::LocalParts::display`]
    /// renders it. A string and not an epoch because a person opens these files.
    pub written_at: String,
    /// Who wrote it: `windai`, or the label an outside AI was told to send. Free text, empty when the
    /// caller did not say, and never used to decide anything.
    #[serde(default)]
    pub written_by: String,
    /// The model the caller says produced the text. Recorded, not trusted: this crate does not call an
    /// endpoint and cannot check it.
    #[serde(default)]
    pub model: String,
    /// Digest of the frames this was written from — see [`crate::fingerprint::of_frames`].
    pub source_fingerprint: String,
    /// Digest of the prompt text in force when it was written. `""` for an entry written before the
    /// prompt became editable, which is its own state rather than a match.
    #[serde(default)]
    pub prompt_fingerprint: String,
}

impl PeriodSummary {
    /// Whether this entry still describes what is on disk.
    pub fn describes(&self, current_source_fingerprint: &str) -> bool {
        !self.source_fingerprint.is_empty() && self.source_fingerprint == current_source_fingerprint
    }

    /// Whether this entry was written under the prompt now in force. An empty stored digest is
    /// *unknown*, and unknown is reported as unknown — an entry of unknown provenance is not silently
    /// counted as current just because the comparison cannot fail.
    pub fn under_prompt(&self, current_prompt_fingerprint: &str) -> bool {
        !self.prompt_fingerprint.is_empty() && self.prompt_fingerprint == current_prompt_fingerprint
    }
}

/// How much of a day has been summarised. Stored inside the day's own summary so a reader can never
/// take a finished-looking paragraph for coverage it does not have.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub segments_total: usize,
    pub segments_summarised: usize,
    /// Every key with no summary that stands, oldest first — never written, or written and since
    /// invalidated by changed content or a rewritten prompt. Full on disk; a *payload* may show a bounded
    /// prefix, and when it does it says how many it left out.
    #[serde(default)]
    pub missing: Vec<String>,
}

impl Coverage {
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && self.segments_summarised >= self.segments_total
    }

    pub fn fraction(&self) -> String {
        format!("{}/{}", self.segments_summarised, self.segments_total)
    }
}

/// What one day was about, written from that day's [`PeriodSummary`] entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaySummary {
    /// The product day, `YYYY-MM-DD`, repeated inside the file it lives in so a moved or renamed file
    /// cannot present one day's text as another's.
    pub date: String,
    pub text: String,
    pub coverage: Coverage,
    /// Set when the caller passed `allow_partial` over an incomplete day. The gate is a rule about the
    /// *write*, not a claim about the data, and this field is what keeps the two apart afterwards.
    #[serde(default)]
    pub partial: bool,
    pub written_at: String,
    #[serde(default)]
    pub written_by: String,
    #[serde(default)]
    pub model: String,
    /// Digest of the (segment key, segment source fingerprint) list this was written from.
    pub source_fingerprint: String,
    /// Set by `windmaint expire` when a segment this was written from left the library. Never rewritten
    /// into `text`: a paragraph that stopped matching its inputs says so in a field.
    #[serde(default)]
    pub stale: bool,
    /// Digest of the daily prompt text in force when it was written.
    #[serde(default)]
    pub prompt_fingerprint: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> PeriodSummary {
        PeriodSummary {
            text: text.into(),
            start: 1,
            end: 2,
            frames: 3,
            ocr_chars: 4,
            written_at: "2026-09-27 18:04:11".into(),
            written_by: String::new(),
            model: String::new(),
            source_fingerprint: "aaaa".into(),
            prompt_fingerprint: String::new(),
        }
    }

    /// An old file must still read after new optional fields appear, and the round trip must be the
    /// identity: this JSON is the user's, and a lossy re-serialisation is a rewrite of their history.
    #[test]
    fn a_missing_optional_field_is_a_state_and_not_a_failure() {
        let bare = r#"{"text":"t","start":1,"end":2,"frames":3,"ocr_chars":4,"written_at":"2026-09-27 18:04:11","source_fingerprint":"aaaa"}"#;
        let parsed: PeriodSummary = serde_json::from_str(bare).expect("the six required fields are enough");
        assert_eq!(parsed, entry("t"));

        let written = serde_json::to_string(&parsed).expect("serialisable");
        let again: PeriodSummary = serde_json::from_str(&written).expect("round trip");
        assert_eq!(again, parsed, "a read-merge-write must not drift the entries it did not touch");
    }

    #[test]
    fn an_unknown_field_survives_a_read_and_a_write_instead_of_being_dropped() {
        // serde ignores unknown fields on read, so a future build's extra field would be silently
        // deleted by this one's merge-write. Asserting the *documented* behaviour is the point: it is
        // why `forget`/`expire` prune by key rather than rewriting whole files.
        let future = r#"{"text":"t","start":1,"end":2,"frames":3,"ocr_chars":4,"written_at":"w","source_fingerprint":"aaaa","who_knows":7}"#;
        let parsed: PeriodSummary = serde_json::from_str(future).expect("unknown fields are ignored");
        assert_eq!(parsed.text, "t");
    }

    #[test]
    fn an_entry_of_unknown_prompt_is_not_reported_as_current() {
        let mut e = entry("t");
        assert!(!e.under_prompt("ppp"), "empty stored digest means unknown, and unknown is not a match");
        e.prompt_fingerprint = "ppp".into();
        assert!(e.under_prompt("ppp"));
        assert!(!e.under_prompt("qqq"));
        assert!(e.describes("aaaa"));
        assert!(!e.describes(""), "a caller that passed no digest must not be told the entry matches");
    }

    #[test]
    fn a_gap_named_in_coverage_is_never_complete_however_the_counts_read() {
        let mut c = Coverage { segments_total: 3, segments_summarised: 3, missing: vec![] };
        assert!(c.complete());
        c.missing = vec!["2026-09-27_15-47-17".into()];
        assert!(!c.complete(), "a missing list is the authority, and 3/3 with a gap is a contradiction");
        c.segments_summarised = 2;
        assert!(!c.complete());
        c.missing.clear();
        assert!(!c.complete(), "and 2/3 with no gaps named is still short");
        assert_eq!(Coverage::default().fraction(), "0/0");
    }

    #[test]
    fn a_day_summary_defaults_are_the_ones_that_tell_no_lies() {
        let json = r#"{"date":"2026-09-27","text":"t","coverage":{"segments_total":0,"segments_summarised":0,"missing":[]},"written_at":"2026-09-27 21:10:04","source_fingerprint":"s"}"#;
        let day: DaySummary = serde_json::from_str(json).expect("required fields only");
        assert!(!day.partial, "absent means it was never downgraded, not that it was");
        assert!(!day.stale);
        assert!(day.written_by.is_empty());
        assert!(day.coverage.complete());
    }
}
