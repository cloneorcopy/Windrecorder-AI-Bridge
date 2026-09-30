//! Whether a day is done — the one question both producers and the daily gate have to agree on.
//!
//! # What "summarised" is allowed to mean
//!
//! An entry counts towards coverage only while it still describes what is on disk. A paragraph written
//! about a stretch whose rows have since been blanked by `windmaint forget`, deleted by `expire`, or
//! rewritten by a re-index is *not* coverage, and counting it would let a day pass the gate that
//! produces the daily summary while being silently wrong underneath it. So a stored entry whose
//! `source_fingerprint` no longer matches is reported as [`Reason::ContentChanged`] and stays out of
//! the count.
//!
//! A second, independent invalidator is the question rather than the content: [`Reason::PromptChanged`]
//! fires when the entry was written under a different prompt text than the one now in force. That is a
//! *quality* state, not a correctness one — the old paragraph still describes its stretch — so it puts
//! the stretch back in the queue without pretending the day was never done. When no prompt digest is
//! supplied, the comparison is not made at all: unknown is not the same as mismatched.
//!
//! # What a month that would not open does
//!
//! It does not become zero. [`DayQueue::skipped`] carries it, and a caller that prints coverage must
//! print that too, because "96/96" is a lie in the presence of a locked month file — the honest reading
//! is "complete as far as could be counted".

use wind_base::config::Config;
use wind_base::range::Span;

use crate::entries::{Coverage, DaySummary, PeriodSummary};
use crate::error::SummaryError;
use crate::fingerprint;
use crate::files::{self, DailyFile, DayMap};
use crate::keys;
use crate::segments::{Reader, Segment};

/// Why a stretch is in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No entry at all.
    Missing,
    /// An entry exists and no longer describes the stretch's content.
    ContentChanged,
    /// An entry exists, still matches its content, and was written under different prompt text.
    PromptChanged,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Missing => "missing",
            Reason::ContentChanged => "content_changed",
            Reason::PromptChanged => "prompt_changed",
        }
    }

    /// What to tell a reader about why this one came back.
    pub fn note(self) -> &'static str {
        match self {
            Reason::Missing => "no summary has ever been written for this stretch",
            Reason::ContentChanged => "the summary of this stretch was written from screen text that is no longer what the index holds",
            Reason::PromptChanged => "the summary of this stretch was written from a different prompt than the one in force now",
        }
    }
}

/// Why a day's summary no longer stands. Several may be true at once, and the list says all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyReason {
    /// It was written over an incomplete day (`allow_partial`), or the day has since grown gaps.
    CoverageIncomplete,
    /// The set of stretch summaries it was written from has moved.
    SummariesChanged,
    /// The daily prompt text has changed since it was written.
    PromptChanged,
}

impl DailyReason {
    pub fn label(self) -> &'static str {
        match self {
            DailyReason::CoverageIncomplete => "coverage_incomplete",
            DailyReason::SummariesChanged => "summaries_changed",
            DailyReason::PromptChanged => "prompt_changed",
        }
    }
}

/// The state of one day's daily summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DailyState {
    /// Nothing has been written for this day.
    Absent,
    /// A file is there and is not a summary. This is an unknown, not an absence.
    Unreadable(String),
    /// Written, complete, and still describing the day it claims to.
    Current(DaySummary),
    /// Written, and something about its premise no longer holds. The text is kept and shown with why.
    Stale(DaySummary, Vec<DailyReason>),
}

impl DailyState {
    pub fn summary(&self) -> Option<&DaySummary> {
        match self {
            DailyState::Current(summary) | DailyState::Stale(summary, _) => Some(summary),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            DailyState::Absent => "not_generated",
            DailyState::Unreadable(_) => "unreadable",
            DailyState::Current(_) => "answered",
            DailyState::Stale(_, _) => "stale",
        }
    }
}

/// The two prompt digests currently in force. An empty string means the caller does not know, and no
/// prompt comparison is then made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptDigests {
    pub period: String,
    pub daily: String,
}

impl PromptDigests {
    pub fn unknown() -> PromptDigests {
        PromptDigests::default()
    }

    /// Digest a pair of prompt texts. The period digest covers both its halves, so a user who edits
    /// only the system turn still invalidates.
    pub fn of(period_system: &str, period_user: &str, daily_system: &str, daily_user: &str) -> PromptDigests {
        PromptDigests {
            period: fingerprint::of_text(&format!("{period_system}\u{0}{period_user}")),
            daily: fingerprint::of_text(&format!("{daily_system}\u{0}{daily_user}")),
        }
    }
}

/// One stretch that came back for work, with what it is and what was previously written about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub segment: Segment,
    pub reason: Reason,
    /// The stale paragraph, when there was one. Given so a producer can improve on it instead of
    /// starting from a blank page — and so a reader can compare the two.
    pub previous: Option<PeriodSummary>,
}

/// Everything one product day consists of, and how much of it has been summarised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayQueue {
    pub day: String,
    pub span: Span,
    pub segments_total: usize,
    /// Entries that still describe their stretch. This is the coverage numerator.
    pub summarised: usize,
    /// Entries on disk, including ones that no longer stand. Reported beside the numerator so
    /// "108 stored, 96 standing" is visible as a difference rather than as a mystery.
    pub entries_stored: usize,
    pub pending: Vec<Item>,
    /// The stretches that need no work, oldest first.
    pub current: Vec<Segment>,
    pub coverage: Coverage,
    pub daily: DailyState,
    /// Month files that would not open during the read. Non-empty means the totals below are a floor.
    pub skipped: Vec<String>,
    /// Rows whose filename this build cannot read as a stretch, so they can hold no summary.
    pub unattributed: usize,
}

impl DayQueue {
    /// Whether a daily summary may be written over this day without `allow_partial`.
    pub fn gate_open(&self) -> bool {
        self.coverage.complete()
    }

    pub fn complete(&self) -> bool {
        self.pending.is_empty()
    }

    /// Every stretch of the day, standing or not, oldest first.
    ///
    /// `--force` needs it: re-asking a day means walking the stretches the queue *is* happy with too,
    /// and a producer that instead read `pending` would silently leave the good ones alone and call the
    /// day regenerated.
    /// Whether this day still owes work: a stretch with no standing summary, or a day summary that no
    /// longer stands while the stretches it was written from are all in place.
    ///
    /// The second half matters as much as the first. `--pending` that looked only at stretches would
    /// never revisit a day whose prompt was rewritten, which is precisely the case the user edits a
    /// prompt in order to cause.
    pub fn has_work(&self) -> bool {
        !self.pending.is_empty()
            || (self.gate_open()
                && !self.current.is_empty()
                && matches!(self.daily, DailyState::Absent | DailyState::Stale(_, _) | DailyState::Unreadable(_)))
    }

    pub fn all_segments(&self) -> Vec<&Segment> {
        let mut all: Vec<&Segment> = self.current.iter().collect();
        all.extend(self.pending.iter().map(|item| &item.segment));
        all.sort_by_key(|segment| (segment.start, segment.key.clone()));
        all
    }
}

/// Read one day's index and its stored summaries, and answer what is done, what is not, and why.
pub fn for_day(config: &Config, day: &str, prompts: &PromptDigests) -> Result<DayQueue, SummaryError> {
    for_day_with(&Reader::new(config), day, prompts)
}

/// The same, on a reader whose index-staleness policy the caller chose — see [`Reader::fresh`].
pub fn for_day_with(reader: &Reader, day: &str, prompts: &PromptDigests) -> Result<DayQueue, SummaryError> {
    let config = reader.config();
    let read = reader.of_day(day)?;
    Ok(build(config, day, read.segments, &read.skipped, read.unattributed, prompts))
}

/// The same, from a caller that already read the index — which is what lets `windai summarize` and the
/// bridge's write tool check the gate once per turn rather than once per stretch.
pub fn for_day_of_segments(config: &Config, day: &str, segments: Vec<Segment>, skipped: &[String], unattributed: usize, prompts: &PromptDigests) -> DayQueue {
    build(config, day, segments, skipped, unattributed, prompts)
}

fn build(config: &Config, day: &str, segments: Vec<Segment>, skipped: &[String], unattributed: usize, prompts: &PromptDigests) -> DayQueue {
    let span = keys::day_span(day, config.day_begin_minutes()).expect("validated by the caller");
    let map = files::read_period(config, day);
    let mut pending: Vec<Item> = Vec::new();
    let mut current: Vec<Segment> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for segment in segments {
        let entry = map.get(&segment.key);
        // Every stretch without a standing summary is a gap, however it got that way: "missing" is what
        // was never written, "stale" is what was written and no longer counts, and a daily summary
        // written over either is a day with a hole in it. A `missing` list that named only the first of
        // those would let a stored daily read as complete while half its stretches were out of date.
        let reason = match entry {
            None => Some(Reason::Missing),
            Some(stored) if !stored.describes(&segment.fingerprint) => Some(Reason::ContentChanged),
            Some(stored) if !prompts.period.is_empty() && !stored.under_prompt(&prompts.period) => Some(Reason::PromptChanged),
            Some(_) => None,
        };
        if reason.is_some() {
            missing.push(segment.key.clone());
        }
        match reason {
            Some(reason) => pending.push(Item { segment, reason, previous: entry.cloned() }),
            None => current.push(segment),
        }
    }
    let coverage = Coverage {
        segments_total: current.len() + pending.len(),
        segments_summarised: current.len(),
        missing,
    };
    let daily = daily_state(config, day, &current, &map, prompts);
    DayQueue {
        day: day.to_string(),
        span,
        segments_total: coverage.segments_total,
        summarised: coverage.segments_summarised,
        entries_stored: map.len(),
        pending,
        current,
        coverage,
        daily,
        skipped: skipped.to_vec(),
        unattributed,
    }
}

/// Where one day's daily summary stands against the day as it is now.
fn daily_state(config: &Config, day: &str, current: &[Segment], periods: &DayMap, prompts: &PromptDigests) -> DailyState {
    let file: DailyFile = files::read_daily(config, day);
    if file.absent() {
        return DailyState::Absent;
    }
    let Some(summary) = file.summary else {
        return DailyState::Unreadable(file.note);
    };
    let mut why: Vec<DailyReason> = Vec::new();
    if summary.partial || !summary.coverage.complete() || current.len() < summary.coverage.segments_total {
        why.push(DailyReason::CoverageIncomplete);
    }
    let inputs = daily_inputs_of(current, periods);
    if !summary.source_fingerprint.is_empty() && summary.source_fingerprint != inputs {
        why.push(DailyReason::SummariesChanged);
    }
    if !prompts.daily.is_empty() && !summary.prompt_fingerprint.is_empty() && summary.prompt_fingerprint != prompts.daily {
        why.push(DailyReason::PromptChanged);
    }
    if summary.stale {
        // `windmaint expire` says it plainly rather than leaving it to be re-derived.
        why.push(DailyReason::SummariesChanged);
    }
    if why.is_empty() {
        DailyState::Current(summary)
    } else {
        DailyState::Stale(summary, why)
    }
}

/// The digest a daily summary is checked against: the (key, stretch source fingerprint) list of the
/// summaries it was written from.
///
/// The *stored* entry digests rather than freshly computed ones: this is a record of what the paragraph
/// was written from. Deriving it from the index instead would make a daily read as stale for a reason
/// that has nothing to do with the day's own summaries.
fn daily_inputs_of(current: &[Segment], periods: &DayMap) -> String {
    fingerprint::of_day_inputs(current.iter().map(|segment| {
        (segment.key.clone(), periods.get(&segment.key).map(|entry| entry.source_fingerprint.clone()).unwrap_or_default())
    }))
}

/// The digest to store in a new daily summary, from the queue the caller was just gated on.
///
/// Public so both producers write the same value from the same [`DayQueue`], instead of one of them
/// re-reading the index a moment later and getting a different answer.
pub fn daily_inputs_for(config: &Config, queue: &DayQueue) -> String {
    let periods = files::read_period(config, &queue.day);
    daily_inputs_of(&queue.current, &periods)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support as support;
    use crate::files::write_period;

    /// The queue for [`support::DAY`] on a reader that re-copies the snapshot every time.
    ///
    /// Two tests below index a new frame and then ask whether the queue noticed. The bridge's own
    /// five-minute window deliberately does not notice for five minutes, which is right for a resident
    /// service and would make those two tests a coin flip on machine speed.
    fn queue_at(config: &Config) -> Result<DayQueue, SummaryError> {
        for_day_with(&Reader::fresh(config), support::DAY, &PromptDigests::unknown())
    }

    fn standing(text: &str, segment: &Segment, prompt: &str) -> PeriodSummary {
        PeriodSummary {
            text: text.into(),
            start: segment.start,
            end: segment.end,
            frames: segment.frames,
            ocr_chars: segment.ocr_chars,
            written_at: support::STAMP.into(),
            written_by: "test".into(),
            model: String::new(),
            source_fingerprint: segment.fingerprint.clone(),
            prompt_fingerprint: prompt.into(),
        }
    }

    /// A fresh day with three stretches and nothing written about them.
    fn fixture(tag: &str, stretches: usize) -> std::path::PathBuf {
        let root = support::install(tag);
        support::seed_morning(&root, "default", stretches);
        root
    }

    fn segments_of(root: &std::path::Path) -> Vec<Segment> {
        Reader::fresh(&support::config_at(root)).of_day(support::DAY).expect("read").segments
    }

    #[test]
    fn an_empty_day_is_all_missing_and_in_the_right_order() {
        let root = fixture("queue-empty", 3);
        let config = support::config_at(&root);
        let queue = queue_at(&config).expect("queue");
        assert_eq!(queue.segments_total, 3);
        assert_eq!(queue.summarised, 0);
        assert_eq!(queue.entries_stored, 0);
        assert_eq!(queue.pending.len(), 3);
        assert!(queue.current.is_empty());
        assert_eq!(queue.pending.iter().map(|i| i.reason).collect::<Vec<_>>(), vec![Reason::Missing; 3]);
        assert_eq!(
            queue.coverage.missing,
            vec!["2026-09-27_09-00-00", "2026-09-27_09-05-00", "2026-09-27_09-10-00"],
            "oldest first, so a producer works forward through the day"
        );
        assert!(!queue.gate_open(), "nothing is summarised");
        assert_eq!(queue.daily, DailyState::Absent);
        assert!(queue.skipped.is_empty());
        support::cleanup(&root);
    }

    #[test]
    fn a_written_stretch_leaves_the_queue_and_opens_the_gate_only_when_all_are_written() {
        let root = fixture("queue-partial", 3);
        let config = support::config_at(&root);
        let segments = segments_of(&root);
        write_period(&config, support::DAY, &segments[0].key, &standing("one", &segments[0], "")).expect("write");
        let queue = queue_at(&config).expect("queue");
        assert_eq!(queue.summarised, 1);
        assert_eq!(queue.pending.len(), 2);
        assert_eq!(queue.coverage.missing, vec!["2026-09-27_09-05-00", "2026-09-27_09-10-00"]);
        assert!(!queue.gate_open());
        for segment in &segments[1..] {
            write_period(&config, support::DAY, &segment.key, &standing("more", segment, "")).expect("write");
        }
        let done = queue_at(&config).expect("queue");
        assert!(done.pending.is_empty(), "{:?}", done.pending);
        assert!(done.gate_open());
        assert_eq!(done.summarised, 3);
        support::cleanup(&root);
    }

    #[test]
    fn a_summary_whose_screen_text_changed_comes_back_as_content_changed_not_as_done() {
        let root = fixture("queue-content", 1);
        let config = support::config_at(&root);
        let before = segments_of(&root);
        write_period(&config, support::DAY, &before[0].key, &standing("a paragraph", &before[0], "")).expect("write");
        assert!(queue_at(&config).expect("clean").pending.is_empty());

        // Index one more frame into the same stretch, which is what a re-run of the OCR pass or a
        // `windmaint forget` that blanked the text does to a segment's content.
        support::seed_month(&root, "default", &[(support::at("2026-09-27_09-00-30"), "2026-09-27_09-00-00.mp4", "Qoder", "an extra frame")]);
        let queue = queue_at(&config).expect("queue");
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending[0].reason, Reason::ContentChanged);
        assert_eq!(queue.summarised, 0, "a stale paragraph is not coverage");
        assert_eq!(queue.entries_stored, 1, "…but it is still on disk, and that difference is reported");
        assert_eq!(
            queue.pending[0].previous.as_ref().expect("previous text kept").text,
            "a paragraph",
            "the old wording comes back with the work, so a producer can improve on it"
        );
        assert!(!queue.gate_open());
        support::cleanup(&root);
    }

    #[test]
    fn editing_the_prompt_puts_finished_work_back_in_the_queue_and_says_why() {
        let root = fixture("queue-prompt", 2);
        let config = support::config_at(&root);
        let segments = segments_of(&root);
        let old = PromptDigests::of("sys old", "user old", "dsys", "duser");
        for segment in &segments {
            write_period(&config, support::DAY, &segment.key, &standing("x", segment, &old.period)).expect("write");
        }
        assert!(for_day(&config, support::DAY, &old).expect("old prompt").pending.is_empty());
        let new = PromptDigests::of("sys new", "user old", "dsys", "duser");
        let queue = for_day(&config, support::DAY, &new).expect("new prompt");
        assert_eq!(queue.pending.iter().map(|i| i.reason).collect::<Vec<_>>(), vec![Reason::PromptChanged; 2]);
        assert_eq!(queue.summarised, 0);
        assert!(queue.pending[0].reason.note().contains("prompt"), "{}", queue.pending[0].reason.note());
        let _ = segments;
        support::cleanup(&root);
    }

    #[test]
    fn an_entry_of_unknown_prompt_is_not_counted_against_a_known_digest() {
        // The comparison must not fire for entries written before the prompt became editable, or the
        // day an external AI finished would be dumped back in the queue the moment a digest exists.
        let root = fixture("queue-legacy", 1);
        let config = support::config_at(&root);
        let segment = segments_of(&root).remove(0);
        write_period(&config, support::DAY, &segment.key, &standing("legacy", &segment, "")).expect("write");
        let digests = PromptDigests::of("sys", "user", "d", "d");
        let queue = for_day(&config, support::DAY, &digests).expect("queue");
        assert_eq!(queue.pending.len(), 1, "unknown provenance is reported, and it is reported as prompt_changed");
        assert_eq!(queue.pending[0].reason, Reason::PromptChanged);
        assert!(queue_at(&config).expect("no digest").pending.is_empty(), "with no digest in force, nothing is compared");
        support::cleanup(&root);
    }

    #[test]
    fn a_daily_summary_is_current_only_until_its_inputs_move() {
        let root = fixture("queue-daily", 2);
        let config = support::config_at(&root);
        let segments = segments_of(&root);
        for segment in &segments {
            write_period(&config, support::DAY, &segment.key, &standing("x", segment, "")).expect("write");
        }
        let queue = queue_at(&config).expect("queue");
        let inputs = daily_inputs_for(&config, &queue);
        let mut summary = DaySummary {
            date: support::DAY.into(),
            text: "the day".into(),
            coverage: queue.coverage.clone(),
            partial: false,
            written_at: support::STAMP.into(),
            written_by: "windai".into(),
            model: String::new(),
            source_fingerprint: inputs.clone(),
            stale: false,
            prompt_fingerprint: String::new(),
        };
        files::write_daily(&config, &summary).expect("daily");
        assert!(matches!(queue_at(&config).expect("q").daily, DailyState::Current(_)));

        // A third stretch is recorded, summarised: the daily paragraph still reads true, but its premise
        // has moved, and the payload has to say so.
        support::seed_month(&root, "default", &[(support::at("2026-09-27_11-00-00"), "2026-09-27_11-00-00.mp4", "Qoder", "an evening screen")]);
        let grew = segments_of(&root);
        write_period(&config, support::DAY, &grew[2].key, &standing("late", &grew[2], "")).expect("write");
        let after = queue_at(&config).expect("q");
        let DailyState::Stale(kept, why) = after.daily else { panic!("expected stale, got {:?}", after.daily) };
        assert_eq!(why, vec![DailyReason::SummariesChanged], "{why:?}");
        assert_eq!(kept.text, "the day", "staling never rewrites the prose");
        assert_eq!(kept.coverage.segments_total, 2, "and it reports the coverage the day had *then*");

        // A day written over a gap is stale from the moment it exists.
        summary.partial = true;
        summary.coverage = Coverage { segments_total: 9, segments_summarised: 4, missing: vec!["x".into()] };
        files::write_daily(&config, &summary).expect("partial daily");
        let DailyState::Stale(_, why) = queue_at(&config).expect("q").daily else {
            panic!("a partial day's summary is never current")
        };
        assert!(why.contains(&DailyReason::CoverageIncomplete), "{why:?}");
        support::cleanup(&root);
    }

    #[test]
    fn a_torn_daily_file_is_an_unknown_rather_than_absence() {
        let root = fixture("queue-daily-torn", 1);
        let config = support::config_at(&root);
        let path = files::day_path(&config, files::Kind::Daily, support::DAY);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"nonsense").expect("torn");
        let DailyState::Unreadable(note) = queue_at(&config).expect("q").daily else {
            panic!("a file that exists and will not parse is not an absent day")
        };
        assert!(note.contains("result_ai_daily_summary"), "{note}");
        support::cleanup(&root);
    }

    #[test]
    fn the_gate_survives_a_bad_day_name_and_a_day_with_nothing_in_it() {
        let root = fixture("queue-nodata", 0);
        let config = support::config_at(&root);
        assert!(matches!(for_day(&config, "yesterday", &PromptDigests::unknown()), Err(SummaryError::BadDate(_))));
        let queue = for_day(&config, "2026-01-05", &PromptDigests::unknown()).expect("an empty day is a real answer");
        assert_eq!(queue.segments_total, 0);
        assert!(queue.gate_open(), "a day with no recorded stretch has nothing missing from it");
        assert_eq!(queue.coverage.fraction(), "0/0");
        support::cleanup(&root);
    }

    #[test]
    fn a_locked_month_is_reported_and_not_counted_as_an_empty_day() {
        let root = fixture("queue-locked", 2);
        let config = support::config_at(&root);
        for entry in std::fs::read_dir(config.db_dir()).expect("db dir") {
            std::fs::write(entry.expect("entry").path(), b"not a database").expect("torn month");
        }
        let queue = queue_at(&config).expect("an answer, with a caveat");
        assert_eq!(queue.segments_total, 0);
        assert_eq!(queue.skipped.len(), 1, "{:?}", queue.skipped);
        assert!(queue.gate_open(), "the gate opens over nothing, which is why the payload must carry `skipped`");
        support::cleanup(&root);
    }

    #[test]
    fn the_digest_pair_covers_both_halves_of_both_prompts() {
        let base = PromptDigests::of("s", "u", "ds", "du");
        assert_eq!(base, PromptDigests::of("s", "u", "ds", "du"));
        assert_ne!(base.period, PromptDigests::of("s2", "u", "ds", "du").period, "editing the system turn invalidates");
        assert_ne!(base.period, PromptDigests::of("s", "u2", "ds", "du").period, "and the user turn");
        assert_ne!(base.daily, PromptDigests::of("s", "u", "ds2", "du").daily);
        assert_eq!(base.period, PromptDigests::of("s", "u", "ds2", "du2").period, "a daily edit must not invalidate every stretch");
        assert_ne!(base.period, base.daily);
    }
}
