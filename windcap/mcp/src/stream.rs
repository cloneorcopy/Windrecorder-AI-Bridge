//! One titled stream, one gap-credit rule, shared by every tool that measures time.
//!
//! `windrecorder_app_usage` and `windrecorder_day_summary` answer different questions — "rank the
//! applications" and "what happened this day" — but they must never disagree about how many seconds
//! a window was on screen. In the Python bridge they had to be kept in step by hand, and the day
//! summary quietly lost up to 100 s a day because it did not close its final gap. This module makes
//! the disagreement structural instead: both tools take their numbers from [`Credits`] and neither
//! owns a rule of its own.
//!
//! The rule, unchanged from `record_wintitle.count_all_page_times_by_raw_dataframe`: a frame owns
//! the wait until the *next* frame, capped at [`GAP_CAP_SECONDS`]. The cap is what stops a locked
//! screen or an overnight gap from being counted as one long session, and it is upstream's number,
//! not a tuned one.

use crate::runtime::Runtime;
use crate::title;

/// Upstream's hard clip on how much of a silence one frame may claim.
pub const GAP_CAP_SECONDS: i64 = 100;

/// Below this, a "session" is a sampling artefact rather than something the user did.
pub const MIN_CREDITED_SECONDS: i64 = 1;

/// How far past the end of a range to look for the frame that closes its last gap. A day is far
/// more than the cap can consume: any successor further away than [`GAP_CAP_SECONDS`] credits
/// exactly the cap anyway.
pub const SUCCESSOR_LOOKAHEAD_SECONDS: i64 = 86_400;

/// One counted frame: when, which window, and the browser URL if the recorder had one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub at: i64,
    pub title: String,
    pub url: Option<String>,
}

/// The stream both tools measure against.
#[derive(Debug, Clone)]
pub struct TitledStream {
    /// Time-ordered samples that survived normalisation, the privacy list and the empty-title rule.
    pub samples: Vec<Sample>,
    /// Frames that carried a usable title at all, before the privacy list was applied.
    pub titled: usize,
    /// Frames dropped because their title matched `exclude_words`. Reported, never hidden: a
    /// summary that silently lost an hour must say it lost an hour.
    pub withheld: usize,
}

impl TitledStream {
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Seconds each sample owns, positionally aligned with [`TitledStream::samples`].
#[derive(Debug, Clone)]
pub struct Credits {
    pub seconds: Vec<i64>,
    /// The sum of the above. Both tools report this same number, which is the point of the module.
    pub total: i64,
}

/// The frames of a range, as normalised titled samples, oldest first.
///
/// A month that cannot be opened is appended to `skipped` and contributes nothing, matching the
/// reader's rule everywhere else in this binary.
pub fn titled_stream(runtime: &Runtime, from: i64, to: i64, skipped: &mut Vec<String>) -> TitledStream {
    let banned = runtime.exclude_words();
    let (rows, mut month_skips) = runtime.rows_in(from, to);
    skipped.append(&mut month_skips);

    let mut samples = Vec::with_capacity(rows.len());
    let mut titled = 0;
    let mut withheld = 0;
    for row in &rows {
        // `Row::title()` already prefers the column and falls back to the `" -||- "` suffix the
        // video-indexing path writes into `ocr_text`, which is why a title survives whichever way
        // the frame was indexed.
        let Some(raw) = row.title() else { continue };
        let Some(clean) = title::normalize(raw) else { continue };
        // The pandas path that used to write this column stored a missing value as the literal text.
        if matches!(clean.to_ascii_lowercase().as_str(), "none" | "nan") {
            continue;
        }
        titled += 1;
        if is_banned(&clean, &banned) {
            withheld += 1;
            continue;
        }
        samples.push(Sample { at: row.time, title: clean, url: row.deep_linking.clone().filter(|u| !u.trim().is_empty()) });
    }
    TitledStream { samples, titled, withheld }
}

fn is_banned(clean: &str, banned: &[String]) -> bool {
    let lowered = clean.to_lowercase();
    banned.iter().any(|word| lowered.contains(word.as_str()))
}

/// The first counted frame after `after`, which closes the range's final gap.
///
/// Without a successor the last frame owns nothing, and a day summary would silently lose up to
/// [`GAP_CAP_SECONDS`] every day that a week-long measurement did count.
pub fn successor(runtime: &Runtime, after: i64) -> Option<Sample> {
    let stream = titled_stream(runtime, after + 1, after + SUCCESSOR_LOOKAHEAD_SECONDS, &mut Vec::new());
    stream.samples.into_iter().next()
}

/// Credit each sample with the wait until the next one, capped, with the range's final frame closed
/// out by `successor` if there is one.
///
/// This is the only place the cap and the last-frame rule are stated. Nothing else may recompute
/// them, or the two tools can disagree again.
pub fn credit(stream: &TitledStream, successor: Option<&Sample>) -> Credits {
    let mut seconds = Vec::with_capacity(stream.samples.len());
    for index in 0..stream.samples.len() {
        let next = match stream.samples.get(index + 1) {
            Some(following) => Some(following.at),
            None => successor.map(|s| s.at),
        };
        let owned = next.map_or(0, |at| (at - stream.samples[index].at).min(GAP_CAP_SECONDS).max(0));
        seconds.push(owned);
    }
    let total = seconds.iter().sum();
    Credits { seconds, total }
}

/// Group credits by window title, longest first, then by name so two runs on the same data print
/// the same order.
pub fn by_title(stream: &TitledStream, credits: &Credits) -> Vec<(String, i64)> {
    let mut totals: std::collections::BTreeMap<&str, i64> = std::collections::BTreeMap::new();
    for (sample, owned) in stream.samples.iter().zip(&credits.seconds) {
        *totals.entry(sample.title.as_str()).or_default() += owned;
    }
    let mut ranked: Vec<(String, i64)> = totals
        .into_iter()
        .filter(|(_, seconds)| *seconds > MIN_CREDITED_SECONDS)
        .map(|(title, seconds)| (title.to_string(), seconds))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// A run of frames under one title: the thing a day is actually made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub from: i64,
    pub to: i64,
    pub frames: usize,
    pub title: String,
    pub url: Option<String>,
    pub seconds: i64,
}

/// Merge consecutive same-title samples into runs, carrying each sample's credit with it.
///
/// The run ends where a *different* title takes over, so it is the hand-off point an agent needs in
/// order to drill down: `from` is a valid argument to `windrecorder_around`.
pub fn runs(stream: &TitledStream, credits: &Credits) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for (index, sample) in stream.samples.iter().enumerate() {
        match out.last_mut() {
            Some(last) if last.title == sample.title => {
                last.to = sample.at;
                last.frames += 1;
            }
            _ => out.push(Run {
                from: sample.at,
                to: sample.at,
                frames: 1,
                title: sample.title.clone(),
                url: sample.url.clone(),
                seconds: 0,
            }),
        }
        // The wait after this frame belongs to whoever was foreground when it was captured, which
        // is the run just opened or extended — not the one that follows it.
        if let Some(last) = out.last_mut() {
            last.seconds += credits.seconds.get(index).copied().unwrap_or(0);
        }
    }
    out
}

/// Which product day an instant falls on, honouring the app's configurable day start: with
/// `day_begin_minutes` at 180 a 01:00 frame is the previous day's work, exactly as the web UI files
/// it there.
///
/// The arithmetic is `wind-summary`'s, not this module's. There were two spellings of "which day owns
/// this instant" in the workspace — subtract the shift and read the calendar date, or ask
/// `clock::day_bounds` whose window contains it — and they agree, which is precisely why the second one
/// is the keeper: it is the function that decided which file a stretch's summary was written into, so a
/// day view and a summary file cannot be produced by two rules that drift.
pub fn day_label(runtime: &Runtime, stored: i64) -> String {
    wind_summary::day_of(stored, runtime.day_begin_minutes())
}
