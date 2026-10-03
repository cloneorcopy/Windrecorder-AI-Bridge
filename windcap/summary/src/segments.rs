//! Which recorded stretches a day actually holds, read out of the index.
//!
//! Coverage, the daily gate, and the source fingerprint all ask the same question — "what segments are
//! there, and what do they contain" — and only one of them may answer it from a guess. This module is
//! the only place in the crate that opens a month file, and it opens it the way the bridge does:
//! through `wind-store`, read-only, on the `_TEMP_READ.db` copy, degrading per month.
//!
//! # The straddle is the interesting part
//!
//! A window is asked for as a product day, but a segment is free to begin yesterday at 02:50 and run
//! past the boundary. Grouping only the rows that fall inside the window would report that stretch with
//! half its frames, half its characters, and — fatally — a different `source_fingerprint` depending on
//! which day asked. So the read is widened to the earliest start of any segment that appears in the
//! window, and only segments *present* in the window are kept. The cost is one extra query, and only on
//! the days that straddle.
//!
//! # Why a read reports its own failures instead of raising them
//!
//! The recorder runs without WAL and without writer retry, so the month currently being indexed can
//! raise where an older one answers. Failing the whole call would then mean "this day has no
//! summaries" is sometimes answered from a locked file, and that is the one answer this crate must not
//! give. So [`DayRead`] carries `skipped` and `unattributed`, and every caller that prints coverage
//! prints them too.

use std::collections::BTreeMap;
use std::time::Duration;

use wind_base::clock::LocalParts;
use wind_base::config::Config;
use wind_store::read::{self, Row};

use crate::error::SummaryError;
use crate::fingerprint;
use crate::keys;

/// Five minutes, which is the bridge's own window and `db_manager.get_temp_dbfilepath`'s.
///
/// Repeated here rather than imported from `wind-mcp` because the dependency points the other way — the
/// bridge will call this crate. It is a copy of a constant, and both sides name the same upstream
/// source, which is the least bad way to keep two readers from disagreeing about when to re-copy.
pub const READ_STALE_AFTER: Duration = Duration::from_secs(300);

/// Six hours: how far past a window's edge the newest stretch is followed.
///
/// The recorder rotates a segment at `record_seconds` (900 as shipped) and pauses an idle machine, so a
/// real stretch ends within minutes of starting. Six hours is twenty-odd times that, and the cost of the
/// slack is one bounded read on the days that straddle an edge — against the cost of the alternative,
/// which is a fingerprint that changes depending on which day asked.
pub const STRADDLE_MARGIN: i64 = 6 * 3_600;

/// One indexed frame, in the shape a prompt and a payload both want.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub timestamp: i64,
    /// `None` on rows from a month that predates the `win_title` column.
    pub title: Option<String>,
    pub url: Option<String>,
    /// The recognized text with the folded-in title split off (`Row::body`), verbatim.
    pub text: String,
}

/// One recorded stretch: the unit a summary is written about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The canonical key — the start stamp, [`keys::canonical_key`].
    pub key: String,
    /// The filename the index holds, so `video_file` and `offset_in_segment` stay reachable from here.
    pub video_file: String,
    /// First and last row time, on the stored axis.
    pub start: i64,
    pub end: i64,
    pub frames: usize,
    /// Total characters of recognized text across those frames — printed so a caller can see what a
    /// request will cost without making it.
    pub ocr_chars: usize,
    /// Distinct window titles, in order of first appearance. Coarse context for choosing what to read.
    pub titles: Vec<String>,
    /// The product day that owns [`Segment::start`], and therefore the file a summary of this stretch
    /// is stored in.
    pub day: String,
    /// Digest of the frames, so "summarised" can be checked against "still describes this".
    pub fingerprint: String,
    /// The frames themselves. Empty when the reader was asked for facts only — the gate needs counts,
    /// and pulling a day of OCR into memory for that would be the mistake this rewrite exists to stop.
    pub detail: Vec<Frame>,
}

impl Segment {
    pub fn duration_seconds(&self) -> i64 {
        (self.end - self.start).max(0)
    }

    /// `2m50s` / `1h2m3s`, the compact form a prompt weighs by.
    pub fn compact_duration(&self) -> String {
        compact_duration(self.duration_seconds())
    }

    /// `15:47:17-15:50:07`, the line prefix a day's summary list is built from.
    pub fn clock_span(&self) -> String {
        let from = LocalParts::from_naive_epoch(self.start);
        let to = LocalParts::from_naive_epoch(self.end);
        let render = |p: LocalParts| format!("{:02}:{:02}:{:02}", p.hour, p.minute, p.second);
        format!("{}-{}", render(from), render(to))
    }
}

/// `1h2m3s` / `5m3s` / `3s`. Same rule `windai`'s tag table uses; kept local because this crate must
/// not depend on the AI binary, and a duration string is not worth a crate edge.
pub fn compact_duration(total: i64) -> String {
    let seconds = total.max(0);
    let hours = seconds / 3_600;
    let minutes = (seconds / 60) % 60;
    let rest = seconds % 60;
    let mut out = String::new();
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if minutes > 0 || hours > 0 {
        out.push_str(&format!("{minutes}m"));
    }
    out.push_str(&format!("{rest}s"));
    out
}

/// What one window of the index contained, plus everything that refused to be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayRead {
    /// The stretches, oldest first.
    pub segments: Vec<Segment>,
    /// Month files that would not open, with the reason. A non-empty list means coverage is *unknown*
    /// below that month, not zero.
    pub skipped: Vec<String>,
    /// Rows whose `videofile_name` this build cannot read as a segment start — a legacy naming scheme,
    /// or an empty column. They cannot hold a summary (a stretch with no key has nothing to file it
    /// under), so they are outside [`DayRead::segments`], and a day that is all unattributed rows
    /// reports total 0. Reported, because that is a fact about the install, not an empty day.
    pub unattributed: usize,
}

/// Which snapshot of a month file a read may answer from.
///
/// The choice is a real one and belongs to the caller, not to this crate's default: a query copy that is
/// five minutes stale is what keeps a resident bridge from re-copying an index on every keystroke, and it
/// is what would make a batch summariser skip the frames the indexing pass wrote thirty seconds ago. Same
/// answer, two different needs, so the two producers say which one they are running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Refresh {
    /// [`READ_STALE_AFTER`], which is the bridge's posture and `db_manager`'s window.
    #[default]
    Bridge,
    /// Re-copy on every query. `windai summarize` uses this, because it runs immediately after the
    /// indexing pass it is about to summarise.
    Always,
}

/// A read-only view of the index for one install.
#[derive(Debug)]
pub struct Reader<'a> {
    config: &'a Config,
    refresh: Refresh,
}

impl<'a> Reader<'a> {
    /// The bridge's posture: five minutes, so a resident service does not re-copy the index per query.
    pub fn new(config: &'a Config) -> Reader<'a> {
        Reader { config, refresh: Refresh::Bridge }
    }

    /// A reader that re-copies the read snapshot on every query.
    ///
    /// For a process that runs right after the indexing pass it is about to summarise. Without it a
    /// batch that writes ninety paragraphs in one sitting works through a snapshot of the library from
    /// before it started, and the day it just finished describing still has frames in it that were never
    /// summarised — every one of which the fingerprints will then call *current*.
    pub fn fresh(config: &'a Config) -> Reader<'a> {
        Reader { config, refresh: Refresh::Always }
    }

    pub fn with_refresh(config: &'a Config, refresh: Refresh) -> Reader<'a> {
        Reader { config, refresh }
    }

    /// The install this reader was built on, so a caller can share one staleness policy across the index
    /// read and the summary reads.
    pub fn config(&self) -> &Config {
        self.config
    }

    pub fn day_begin_minutes(&self) -> i64 {
        self.config.day_begin_minutes()
    }

    fn options(&self) -> read::ReadOptions {
        read::ReadOptions {
            stale_after: match self.refresh {
                Refresh::Bridge => READ_STALE_AFTER,
                Refresh::Always => Duration::ZERO,
            },
            maintaining: self.config.maintain_lock_claimed(),
        }
    }

    /// The segments of one product day, oldest first, with their frame text.
    pub fn of_day(&self, day: &str) -> Result<DayRead, SummaryError> {
        self.day_window(day, true)
    }

    /// The facts of one product day without any frame text — what the coverage gate needs.
    pub fn facts_of_day(&self, day: &str) -> Result<DayRead, SummaryError> {
        self.day_window(day, false)
    }

    fn day_window(&self, day: &str, with_text: bool) -> Result<DayRead, SummaryError> {
        let shift = self.day_begin_minutes();
        let span = keys::day_span(day, shift).ok_or_else(|| SummaryError::BadDate(day.to_string()))?;
        self.segments_in(span.from, span.to, with_text)
    }

    /// Every row in a window, oldest first, merged across the months it touches, with the months that
    /// refused to open named.
    pub fn rows_in(&self, from: i64, to: i64) -> (Vec<Row>, Vec<String>) {
        let options = self.options();
        let mut rows = Vec::new();
        let mut skipped = Vec::new();
        let months = read::discover(&self.config.db_dir());
        for month in read::months_in_range(&months, from, to) {
            let conn = match month.open_with(options) {
                Ok(conn) => conn,
                Err(e) => {
                    skipped.push(format!("{}: {e}", name(month)));
                    continue;
                }
            };
            match read::rows_in_window(&conn, Some(from), Some(to)) {
                Ok(found) => rows.extend(found),
                Err(e) => skipped.push(format!("{}: {e}", name(month))),
            }
        }
        rows.sort_by_key(|row| (row.time, row.rowid));
        (rows, skipped)
    }

    /// The segments whose rows touch `[from, to]`, widened so a straddling stretch comes out whole.
    pub fn segments_in(&self, from: i64, to: i64, with_text: bool) -> Result<DayRead, SummaryError> {
        let (inside, mut skipped) = self.rows_in(from, to);
        // Which stretches appear in the window at all, and how early the earliest of them begins.
        let mut named: Vec<String> = Vec::new();
        let mut earliest: Option<i64> = None;
        let mut latest: Option<i64> = None;
        let mut unattributed = 0usize;
        for row in &inside {
            match keys::canonical_key(&row.videofile_name) {
                Some(key) => {
                    if !named.iter().any(|seen| seen == &key) {
                        if let Some(stamp) = LocalParts::from_stamp(&key) {
                            let begins = stamp.naive_epoch_seconds();
                            earliest = Some(earliest.map_or(begins, |so_far| so_far.min(begins)));
                            latest = Some(latest.map_or(begins, |so_far| so_far.max(begins)));
                        }
                        named.push(key);
                    }
                }
                None => unattributed += 1,
            }
        }
        // A stretch can outlive the window at either edge: one that began before `from` runs in, and
        // only the newest of them can still be running at `to`. Both are fixed by reading out to the
        // margin and letting `group` drop every key the window never named — so the extra rows cost one
        // bounded read, and only when an edge is actually straddled.
        let left = earliest.map_or(from, |begins| begins.min(from));
        let right = match latest {
            Some(last) if last + STRADDLE_MARGIN > to => last + STRADDLE_MARGIN,
            _ => to,
        };
        let rows = if left == from && right == to {
            inside
        } else {
            let (widened, more) = self.rows_in(left, right);
            skipped.extend(more);
            widened
        };
        Ok(DayRead { segments: group(rows, &named, with_text, self.day_begin_minutes()), skipped, unattributed })
    }

    /// The one segment a reference names, inside a window.
    pub fn resolve(&self, reference: &str, from: i64, to: i64) -> Result<Segment, SummaryError> {
        self.pick(reference, &self.segments_in(from, to, true)?.segments)
    }

    /// Same resolution against a list the caller already read, so a tool that lists and writes in one
    /// turn pays for the index once.
    ///
    /// Three spellings, one answer: the filename, the bare stamp, or any instant that falls in the
    /// stretch. An instant that falls in a *gap* between stretches is refused rather than attributed to
    /// the nearest one — a summary filed against the wrong stretch is a false memory with a timestamp
    /// attached, which is the one outcome this whole key scheme exists to prevent.
    pub fn pick(&self, reference: &str, segments: &[Segment]) -> Result<Segment, SummaryError> {
        let trimmed = reference.trim();
        if trimmed.is_empty() {
            return Err(SummaryError::BadReference(reference.to_string()));
        }
        if let Some(key) = keys::canonical_key(trimmed) {
            return segments.iter().find(|s| s.key == key).cloned().ok_or_else(|| SummaryError::UnknownSegment(key));
        }
        // A bare date names a day, and a day is not a stretch. Answering it with the day's first
        // segment would file one paragraph over eighteen of them.
        if keys::parse_day(trimmed).is_some() {
            return Err(SummaryError::BadReference(format!(
                "{trimmed} names a day. A summary is written about one stretch: give its file, its start \
                 stamp, or a timestamp inside it."
            )));
        }
        let instant = trimmed
            .parse::<i64>()
            .ok()
            .or_else(|| instant_text(trimmed))
            .ok_or_else(|| SummaryError::BadReference(reference.to_string()))?;
        segments
            .iter()
            .find(|s| instant >= s.start && instant <= s.end)
            .cloned()
            .ok_or_else(|| SummaryError::UnknownSegment(format!("{reference} (no stretch covers it)")))
    }
}

/// The datetime shapes the bridge's own `timestamp` argument documents — `2026-09-20 14:30:00` and
/// `2026-09-20T14:30:00` — on top of the `%Y-%m-%d_%H-%M-%S` the index stores.
///
/// An agent that was handed a datetime by `windrecorder_search` and passes it back unchanged must not be
/// told it wrote the wrong shape; the two promises are made by the same product.
fn instant_text(text: &str) -> Option<i64> {
    if let Some(at) = wind_base::range::parse_instant(text, false) {
        return Some(at);
    }
    let normalised: String = text
        .chars()
        .map(|c| match c {
            ' ' | 'T' => '_',
            ':' => '-',
            other => other,
        })
        .collect();
    LocalParts::from_stamp(&normalised).map(|parts| parts.naive_epoch_seconds())
}

/// The name reports quote for a month file.
fn name(month: &read::Month) -> String {
    month.path.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string()
}

/// Turn merged, sorted rows into segments, keeping only the stretches named in the window.
fn group(rows: Vec<Row>, named: &[String], with_text: bool, day_begin_minutes: i64) -> Vec<Segment> {
    let mut buckets: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for row in rows {
        let Some(key) = keys::canonical_key(&row.videofile_name) else { continue };
        if !named.iter().any(|seen| *seen == key) {
            continue;
        }
        buckets.entry(key).or_default().push(row);
    }
    buckets
        .into_iter()
        .map(|(key, rows)| {
            let frames: Vec<Frame> = rows
                .iter()
                .map(|row| Frame {
                    timestamp: row.time,
                    title: row.title().map(str::to_string),
                    url: row.deep_linking.clone().filter(|value| !value.trim().is_empty()),
                    text: row.body().to_string(),
                })
                .collect();
            let ocr_chars: usize =
                if with_text { frames.iter().map(|f| f.text.chars().count()).sum() } else { rows.iter().map(|row| row.body().chars().count()).sum() };
            let mut titles: Vec<String> = Vec::new();
            for frame in &frames {
                if let Some(title) = &frame.title {
                    if !titles.iter().any(|seen| seen == title) {
                        titles.push(title.clone());
                    }
                }
            }
            let start = frames.iter().map(|f| f.timestamp).min().unwrap_or(0);
            let end = frames.iter().map(|f| f.timestamp).max().unwrap_or(0);
            let digest = fingerprint::of_frames(frames.iter().map(|f| {
                (f.timestamp, f.title.clone().unwrap_or_default(), f.url.clone().unwrap_or_default(), f.text.clone())
            }));
            Segment {
                video_file: rows.first().map(|r| r.videofile_name.clone()).unwrap_or_default(),
                fingerprint: digest,
                day: keys::day_of(start, day_begin_minutes),
                frames: frames.len(),
                ocr_chars,
                titles,
                start,
                end,
                detail: if with_text { frames } else { Vec::new() },
                key,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support as support;

    fn config_with(root: &std::path::Path, rows: &[(i64, &str, &str, &str)]) -> Config {
        support::seed_month(root, "default", rows);
        support::config_at(root)
    }

    #[test]
    fn a_day_reads_back_its_segments_in_order() {
        let root = support::install("segments-day");
        let config = config_with(
            &root,
            &[
                (support::at("2026-09-27_15-00-10"), "2026-09-27_15-00-00.mp4", "Qoder — main.rs", "first text"),
                (support::at("2026-09-27_15-00-40"), "2026-09-27_15-00-00.mp4", "Qoder — main.rs", "second text"),
                (support::at("2026-09-27_16-10-05"), "2026-09-27_16-10-00.mp4", "firefox", "a page"),
                // Another day's stretch, which the day under test must not see.
                (support::at("2026-09-26_14-00-00"), "2026-09-26_14-00-00.mp4", "notepad", "older"),
            ],
        );
        let read = Reader::new(&config).of_day("2026-09-27").expect("read");
        assert!(read.skipped.is_empty(), "{:?}", read.skipped);
        assert_eq!(read.unattributed, 0);
        let segments = read.segments;
        assert_eq!(segments.len(), 2, "{segments:?}");
        assert_eq!(segments[0].key, "2026-09-27_15-00-00");
        assert_eq!(segments[0].frames, 2);
        assert_eq!(segments[0].start, support::at("2026-09-27_15-00-10"));
        assert_eq!(segments[0].end, support::at("2026-09-27_15-00-40"));
        assert_eq!(segments[0].video_file, "2026-09-27_15-00-00.mp4");
        assert_eq!(segments[1].key, "2026-09-27_16-10-00");
        support::cleanup(&root);
    }

    /// The whole point of the widening: the same stretch must not have two fingerprints, one per day.
    #[test]
    fn a_stretch_across_the_day_boundary_comes_out_whole_either_way() {
        let root = support::install("segments-straddle");
        let config = config_with(
            &root,
            &[
                (support::at("2026-09-27_02-50-00"), "2026-09-27_02-45-00.mp4", "Qoder", "before three"),
                (support::at("2026-09-27_03-10-00"), "2026-09-27_02-45-00.mp4", "Qoder", "after three"),
            ],
        );
        let reader = Reader::new(&config);
        let day27 = reader.of_day("2026-09-27").expect("the window its rows touch");
        assert_eq!(day27.segments.len(), 1);
        let one = &day27.segments[0];
        assert_eq!(one.frames, 2, "both frames, not only the ones past 03:00");
        assert_eq!(one.day, "2026-09-26", "its start is 02:45, so yesterday owns it");

        let day26 = reader.of_day("2026-09-26").expect("previous day");
        let same = day26.segments.iter().find(|s| s.key == one.key).expect("present");
        assert_eq!(same.fingerprint, one.fingerprint, "one stretch, one fingerprint");
        assert_eq!(same.ocr_chars, one.ocr_chars);
        support::cleanup(&root);
    }

    #[test]
    fn all_three_reference_spellings_name_one_segment() {
        let root = support::install("segments-resolve");
        let config = config_with(
            &root,
            &[
                (support::at("2026-09-27_15-00-10"), "2026-09-27_15-00-00.mp4", "Qoder", "text"),
                (support::at("2026-09-27_18-00-00"), "2026-09-27_18-00-00.mp4", "mail", "evening"),
            ],
        );
        let reader = Reader::new(&config);
        let span = keys::day_span("2026-09-27", reader.day_begin_minutes()).expect("day");
        let segments = reader.segments_in(span.from, span.to, true).expect("read").segments;
        for given in ["2026-09-27_15-00-00.mp4", "2026-09-27_15-00-00"] {
            assert_eq!(reader.pick(given, &segments).expect(given).key, "2026-09-27_15-00-00");
        }
        let inside = support::at("2026-09-27_15-00-10");
        assert_eq!(reader.pick(&inside.to_string(), &segments).expect("int").key, "2026-09-27_15-00-00");
        assert_eq!(reader.pick("2026-09-27 15:00:10", &segments).expect("iso").key, "2026-09-27_15-00-00");
        support::cleanup(&root);
    }

    #[test]
    fn an_instant_in_a_gap_is_refused_not_guessed() {
        let root = support::install("segments-gap");
        let config = config_with(
            &root,
            &[
                (support::at("2026-09-27_15-00-10"), "2026-09-27_15-00-00.mp4", "Qoder", "text"),
                (support::at("2026-09-27_18-00-00"), "2026-09-27_18-00-00.mp4", "mail", "evening"),
            ],
        );
        let reader = Reader::new(&config);
        let span = keys::day_span("2026-09-27", reader.day_begin_minutes()).expect("day");
        let segments = reader.segments_in(span.from, span.to, true).expect("read").segments;
        let gap = support::at("2026-09-27_16-30-00");
        let error = reader.pick(&gap.to_string(), &segments).expect_err("a gap names nothing");
        assert!(matches!(error, SummaryError::UnknownSegment(_)), "{error:?}");
        for bad in ["", "   ", "yesterday", "2026-09-27"] {
            assert!(matches!(reader.pick(bad, &segments), Err(SummaryError::BadReference(_))), "{bad:?} is not a shape this accepts");
        }
        support::cleanup(&root);
    }

    #[test]
    fn facts_only_pulls_no_text_but_still_digests_it() {
        let root = support::install("segments-facts");
        let config = config_with(&root, &[(support::at("2026-09-27_15-00-10"), "2026-09-27_15-00-00.mp4", "Qoder", "some text")]);
        let reader = Reader::new(&config);
        let facts = reader.facts_of_day("2026-09-27").expect("facts").segments;
        assert_eq!(facts[0].detail.len(), 0, "no frame text held for a coverage pass");
        assert_eq!(facts[0].ocr_chars, 9);
        let full = reader.of_day("2026-09-27").expect("full").segments;
        assert_eq!(full[0].detail.len(), 1);
        assert_eq!(full[0].fingerprint, facts[0].fingerprint, "and the digest does not depend on which");
        support::cleanup(&root);
    }

    #[test]
    fn the_title_folds_out_of_the_text_and_into_its_own_field() {
        // The video-indexing path stores `"<ocr> -||- <title>"` in one column; `body()` and `title()`
        // split it, so a prompt built from raw `ocr_text` would carry the title twice.
        let root = support::install("segments-split");
        let config = config_with(
            &root,
            &[(support::at("2026-09-27_15-00-10"), "2026-09-27_15-00-00.mp4", "", "screen words -||- Qoder — main.rs")],
        );
        let segments = Reader::new(&config).of_day("2026-09-27").expect("read").segments;
        assert_eq!(segments[0].titles, vec!["Qoder — main.rs".to_string()]);
        assert_eq!(segments[0].detail[0].text, "screen words");
        assert_eq!(segments[0].ocr_chars, 12);
        support::cleanup(&root);
    }

    #[test]
    fn durations_and_span_lines_render_for_a_prompt() {
        let root = support::install("segments-labels");
        let config = config_with(
            &root,
            &[
                (support::at("2026-09-27_15-00-10"), "2026-09-27_15-00-00.mp4", "a", "t"),
                (support::at("2026-09-27_15-03-00"), "2026-09-27_15-00-00.mp4", "a", "t"),
            ],
        );
        let segments = Reader::new(&config).of_day("2026-09-27").expect("read").segments;
        assert_eq!(segments[0].compact_duration(), "2m50s");
        assert_eq!(segments[0].clock_span(), "15:00:10-15:03:00");
        assert_eq!(compact_duration(3_723), "1h2m3s");
        assert_eq!(compact_duration(0), "0s");
        assert_eq!(compact_duration(-5), "0s", "a negative span is reported as none, not as a huge one");
        support::cleanup(&root);
    }

    #[test]
    fn a_month_that_will_not_open_is_named_and_not_denied() {
        let root = support::install("segments-locked");
        let config = config_with(&root, &[(support::at("2026-09-27_15-00-10"), "2026-09-27_15-00-00.mp4", "a", "t")]);
        let db_dir = config.db_dir();
        for entry in std::fs::read_dir(&db_dir).expect("fixture db dir") {
            std::fs::write(entry.expect("entry").path(), b"not a database at all").expect("torn file");
        }
        let read = Reader::new(&config).of_day("2026-09-27").expect("an empty answer, not a crash");
        assert!(read.segments.is_empty());
        assert_eq!(read.skipped.len(), 1, "the month that refused says so: {:?}", read.skipped);
        support::cleanup(&root);
    }

    #[test]
    fn a_filename_that_is_not_a_stamp_is_counted_not_invented() {
        let root = support::install("segments-legacy");
        let config = config_with(
            &root,
            &[
                // Upstream's prefixed naming, which `LocalParts::from_stamp` cannot read.
                (support::at("2026-09-27_14-00-00"), "default_2026-09-27-14-00-00.mp4", "a", "t"),
                (support::at("2026-09-27_15-00-10"), "2026-09-27_15-00-00.mp4", "a", "t"),
            ],
        );
        let read = Reader::new(&config).of_day("2026-09-27").expect("read");
        assert_eq!(read.segments.len(), 1, "the readable stretch still answers");
        assert_eq!(read.unattributed, 1, "and the other one is reported, not filed under a made-up key");
        support::cleanup(&root);
    }
}
