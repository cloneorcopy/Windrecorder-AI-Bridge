//! Turning rows into the pictures the UI draws: day overviews, timeline strips, histograms.
//!
//! Upstream computes all of these by building a pandas DataFrame per page render — a day's rows
//! filtered, resampled into 6-minute buckets, snapped to evenly spaced thumbnails, then joined
//! against the window-title log. Here the same arithmetic runs over a `Vec<Row>` in one pass, which
//! is what makes scrubbing a timeline feel instant instead of a spinner.

use std::collections::BTreeMap;

use wind_base::clock::LocalParts;

use crate::read::Row;

/// One vertical of the "activity during this day" area chart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    /// Bucket start, in the stored epoch.
    pub start: i64,
    pub count: usize,
    /// Which month file, so a caller can label the axis without re-deriving the date.
    pub label: String,
}

/// A day's shape: how much there is, when it starts and ends, and how it is distributed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayOverview {
    pub rows: usize,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub buckets: Vec<Bucket>,
    /// Time the user was at the machine, in seconds: every stretch between two captures, with a
    /// stretch longer than the caller's `presence_gap_secs` cut to that length. This is the "hours"
    /// figure the day header prints *and* the month scatter plots, from [`histogram`].
    pub active_seconds: i64,
}

impl DayOverview {
    pub fn hours(&self) -> f64 {
        self.active_seconds as f64 / 3600.0
    }
}

/// The seconds between two captures that still count as one session: everything up to
/// `presence_gap_secs`, and that length for anything past it.
fn presence_seconds(gap: i64, presence_gap_secs: i64) -> i64 {
    gap.clamp(0, presence_gap_secs.max(1))
}

/// Bucket rows into fixed-width intervals and count activity.
///
/// `bucket_secs` is 360 in the shipped UI (a 6-minute resolution across a day), and it is *only*
/// ever the width of a drawn bar. The presence ruler is a separate argument,
/// `presence_gap_secs`, because the two answer different questions and only one of them is allowed
/// to change when the window is resized:
/// [`wind_base::config::Config::presence_gap_secs`] derives it from the recorder's own settings.
///
/// Buckets are emitted for the whole `[from, to]` span including the empty ones, because the area
/// chart needs a continuous x-axis; a sparse list would draw a spike where the day is in fact blank.
pub fn overview(rows: &[Row], from: i64, to: i64, bucket_secs: i64, presence_gap_secs: i64) -> DayOverview {
    let bucket_secs = bucket_secs.max(1);
    let mut in_window: Vec<&Row> = rows.iter().filter(|r| r.time >= from && r.time <= to).collect();
    in_window.sort_by_key(|r| (r.time, r.rowid));

    let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
    for row in &in_window {
        *counts.entry(row.time.div_euclid(bucket_secs) * bucket_secs).or_insert(0) += 1;
    }

    // Time at the machine, read off the only evidence there is: the distance between consecutive
    // captures. A capture stops for two reasons the index cannot tell apart — nobody was there, or
    // nobody changed anything — so a stretch past the presence gap is cut rather than dropped.
    let active_seconds: i64 =
        in_window.windows(2).map(|pair| presence_seconds(pair[1].time - pair[0].time, presence_gap_secs)).sum();

    let mut buckets = Vec::new();
    let mut start = from.div_euclid(bucket_secs) * bucket_secs;
    while start <= to {
        buckets.push(Bucket {
            start,
            count: counts.get(&start).copied().unwrap_or(0),
            label: LocalParts::from_naive_epoch(start).display()[11..16].to_string(),
        });
        start += bucket_secs;
    }

    DayOverview {
        rows: in_window.len(),
        first: in_window.first().map(|r| r.time),
        last: in_window.last().map(|r| r.time),
        buckets,
        active_seconds,
    }
}

/// A timeline strip: `count` rows sampled at even *time* intervals across `[from, to]`.
///
/// This is `db_get_day_thumbnail_by_timeavg`. Sampling by time (not by row index) is what keeps the
/// strip honest as a scrubber: position along the strip means position in the day, so dragging to
/// the middle lands at midday whether the user was busy or idle.
#[derive(Debug, Clone)]
pub struct Timeline {
    pub points: Vec<Row>,
    pub from: i64,
    pub to: i64,
    /// Time each point stands for. `points[i]` covers `[spans[i].0, spans[i].1]`.
    pub spans: Vec<(i64, i64)>,
}

impl Timeline {
    /// Slot `i` stands for the slice `[from + i*step, from + (i+1)*step)` and is filled by the row
    /// nearest its centre. A slice with no rows contributes no point, which is the honest answer:
    /// a day with two hours of activity has two hours of strip, and inventing thumbnails for the
    /// rest would make the scrubber lie about where the user actually was.
    pub fn sample(rows: &[Row], from: i64, to: i64, count: usize) -> Timeline {
        let mut points = Vec::new();
        let mut spans = Vec::new();
        if count == 0 || rows.is_empty() || to <= from {
            return Timeline { points, spans, from, to };
        }
        let step = (to - from) as f64 / count as f64;
        for i in 0..count {
            let low = from + (i as f64 * step) as i64;
            let high = if i + 1 == count { to } else { from + ((i + 1) as f64 * step) as i64 };
            let centre = low + (high - low) / 2;
            let in_slot: Vec<Row> = rows
                .iter()
                .filter(|r| r.time >= low && r.time < high)
                .cloned()
                .collect();
            if let Some(row) = nearest(&in_slot, centre, (high - low) / 2 + 1) {
                points.push(row);
                spans.push((low, high));
            }
        }
        Timeline { points, spans, from, to }
    }

    /// Which sample index a wall-clock time falls in, for hit-testing a click on the strip.
    pub fn index_for(&self, time: i64) -> Option<usize> {
        self.spans.iter().position(|(a, b)| time >= *a && time <= *b)
    }
}

/// The row closest to `target`, no further than `window`.
pub fn nearest(rows: &[Row], target: i64, window: i64) -> Option<Row> {
    let mut best: Option<(i64, &Row)> = None;
    for row in rows {
        let distance = (row.time - target).abs();
        if distance > window {
            continue;
        }
        // A tie resolves to the earlier row, matching the "look backwards first" behaviour users
        // see when two frames share a second.
        let better = match best {
            None => true,
            Some((d, _)) => distance < d,
        };
        if better {
            best = Some((distance, row));
        }
    }
    best.map(|(_, row)| row.clone())
}

/// Sample by row index instead of by time — `db_get_day_thumbnail_by_distributeavg`, which keeps a
/// busy day's strip representative even when the user was active in bursts.
pub fn evenly_by_index(rows: &[Row], count: usize) -> Vec<Row> {
    if rows.is_empty() || count == 0 {
        return Vec::new();
    }
    if rows.len() <= count {
        return rows.to_vec();
    }
    let gap = (rows.len() / count).max(1);
    let mut out = Vec::with_capacity(count);
    for i in (0..rows.len()).step_by(gap) {
        out.push(rows[i].clone());
        if out.len() == count {
            break;
        }
    }
    out
}

/// One day's totals inside a month, for the Stat tab's scatter.
#[derive(Debug, Clone, PartialEq)]
pub struct DayStat {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub rows: usize,
    /// Hours at the machine, by the same ruler as [`DayOverview::active_seconds`] — the month's dot
    /// and the day's header must disagree about nothing but precision, or the scatter says a day
    /// spanned 10 hours and opening it reports 2.4.
    pub hours: f64,
}

/// Group rows into "product days", honouring `day_begin_minutes`: activity at 01:00 belongs to the
/// previous day, which is why this is not a `GROUP BY` on the date substring.
pub fn histogram(rows: &[Row], day_begin_minutes: i64, presence_gap_secs: i64) -> Vec<DayStat> {
    let mut days: BTreeMap<(i64, u32, u32), Vec<i64>> = BTreeMap::new();
    for row in rows {
        let shifted = row.time - day_begin_minutes * 60;
        let when = LocalParts::from_naive_epoch(shifted);
        days.entry((when.year, when.month, when.day)).or_default().push(row.time);
    }
    days.into_iter()
        .map(|((year, month, day), mut times)| {
            // A month file is read in time order, but a day is assembled here from whatever order
            // the caller had, and a ruler that sums differences only means anything sorted.
            times.sort_unstable();
            let count = times.len();
            let seconds: i64 = times.windows(2).map(|pair| presence_seconds(pair[1] - pair[0], presence_gap_secs)).sum();
            DayStat { year, month, day, rows: count, hours: seconds as f64 / 3600.0 }
        })
        .collect()
}

/// A stretch of time one window title held focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleInterval {
    pub title: String,
    pub from: i64,
    pub to: i64,
}

impl TitleInterval {
    pub fn seconds(&self) -> i64 {
        (self.to - self.from).max(0)
    }
}

/// Collapse a day's rows into per-title intervals, the side panel's "where the time went" list.
///
/// A row carries only the instant it was captured, so an interval ends where the next row of a
/// different title begins — bounded by `max_gap`, because a five-hour gap between two rows of the
/// same title is two sessions, not one. Upstream clips at 100 s; that number is kept.
pub fn title_intervals(rows: &[Row], max_gap: i64) -> Vec<TitleInterval> {
    let mut ordered: Vec<&Row> = rows.iter().collect();
    ordered.sort_by_key(|r| (r.time, r.rowid));
    let mut out: Vec<TitleInterval> = Vec::new();
    for row in ordered {
        let title = row.title().unwrap_or("").trim().to_string();
        if title.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.title == title && row.time - last.to <= max_gap => last.to = row.time,
            _ => out.push(TitleInterval { title, from: row.time, to: row.time }),
        }
    }
    out
}

/// Total seconds per title across the day, longest first — what the side panel sorts by.
pub fn title_totals(rows: &[Row], max_gap: i64) -> Vec<(String, i64)> {
    let mut totals: BTreeMap<String, i64> = BTreeMap::new();
    for interval in title_intervals(rows, max_gap) {
        *totals.entry(interval.title).or_default() += interval.seconds();
    }
    let mut out: Vec<(String, i64)> = totals.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    fn row(stamp: &str, title: &str) -> Row {
        Row {
            rowid: 1,
            videofile_name: "2026-09-21_10-00-00.mp4".into(),
            picturefile_name: String::new(),
            time: ts(stamp),
            ocr_text: format!("text {stamp}"),
            win_title: Some(title.to_string()),
            deep_linking: None,
            thumbnail: Some("AAA".into()),
            video_exists: true,
            picture_exists: false,
            month_path: None,
        }
    }

    fn day(stamps: &[&str], title: &str) -> Vec<Row> {
        stamps.iter().map(|s| row(s, title)).collect()
    }

    /// The presence ruler the shipped settings answer with (`record_seconds` 900, longer than the
    /// recorder's 5-minute still-screen pause). Tests name it because the two `i64` arguments to
    /// `overview` are different questions that look the same at a call site.
    const PRESENCE: i64 = 900;

    #[test]
    fn buckets_cover_the_whole_day_including_the_empty_hours() {
        let rows = day(&["2026-09-21_09-00-10", "2026-09-21_09-02-00", "2026-09-21_18-30-00"], "Excel");
        let o = overview(&rows, ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"), 360, PRESENCE);
        assert_eq!(o.rows, 3);
        // 24 hours at 6-minute resolution.
        assert_eq!(o.buckets.len(), 240);
        assert_eq!(o.buckets[0].count, 0);
        assert_eq!(o.buckets.iter().filter(|b| b.count > 0).count(), 2, "09:00 and 18:30 are apart");
        assert_eq!(o.buckets.iter().find(|b| b.count == 2).unwrap().label, "09:00");
        assert_eq!((o.first, o.last), (Some(ts("2026-09-21_09-00-10")), Some(ts("2026-09-21_18-30-00"))));
    }

    #[test]
    fn rows_outside_the_window_are_ignored_not_counted() {
        let rows = day(&["2026-09-20_23-00-00", "2026-09-21_10-00-00"], "Excel");
        let o = overview(&rows, ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"), 360, PRESENCE);
        assert_eq!(o.rows, 1);
        assert_eq!(o.first, Some(ts("2026-09-21_10-00-00")));
    }

    #[test]
    fn an_empty_day_still_draws_its_axis() {
        let o = overview(&[], ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"), 360, PRESENCE);
        assert_eq!(o.rows, 0);
        assert_eq!(o.buckets.len(), 240);
        assert!(o.buckets.iter().all(|b| b.count == 0));
        assert_eq!(o.first, None);
    }

    /// What the figure is *for*: a still screen is the recorder having nothing new to write, not the
    /// user having left. A 40-minute read used to be worth one chart bucket of 6 minutes; on the
    /// shipped settings it is now worth the recorder's own segment, and only the time past that is
    /// thrown away.
    #[test]
    fn a_still_screen_over_the_pause_threshold_is_still_time_at_the_machine() {
        let rows = day(&["2026-09-21_09-00-00", "2026-09-21_09-40-00"], "Acrobat");
        let o = overview(&rows, ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"), 360, PRESENCE);
        assert_eq!(o.active_seconds, 900, "2400 s of still screen is cut at one segment, not at one bar");
    }

    /// The decoupling itself, which is the actual defect: the ruler used to be `bucket_secs`, so
    /// redrawing the same day at a different resolution changed how long the user had been at their
    /// desk. Nothing but `presence_gap_secs` may move that number.
    #[test]
    fn the_chart_resolution_does_not_decide_how_long_the_day_was() {
        let rows = day(&["2026-09-21_09-00-00", "2026-09-21_09-40-00", "2026-09-21_10-00-00"], "Acrobat");
        let wide = overview(&rows, ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"), 360, PRESENCE);
        let narrow = overview(&rows, ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"), 60, PRESENCE);
        assert_eq!(wide.active_seconds, narrow.active_seconds);
        assert_eq!(wide.buckets.len(), 240, "and the bars still follow the resolution they were asked for");
        assert_eq!(narrow.buckets.len(), 1440);
    }

    /// One ruler, two views. The month scatter used to plot `last - first` with no cut at all, so a
    /// day could stand 10 hours tall in the month and read 2.4 hours when opened.
    #[test]
    fn the_month_dot_and_the_day_header_answer_with_the_same_seconds() {
        let rows = day(
            &["2026-09-21_09-00-00", "2026-09-21_09-40-00", "2026-09-21_12-00-00", "2026-09-21_12-05-00"],
            "Excel",
        );
        let o = overview(&rows, ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"), 360, PRESENCE);
        let stats = histogram(&rows, 0, PRESENCE);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].rows, 4);
        assert!(
            (stats[0].hours * 3600.0 - o.active_seconds as f64).abs() < 1e-6,
            "month said {:.1} h, day said {:.1} h",
            stats[0].hours,
            o.hours()
        );
    }

    #[test]
    fn a_timeline_spreads_points_over_time_not_over_rows() {
        // Two clusters an hour apart: a time-even strip must show both, an index-even strip would
        // spend all its slots on the busier cluster.
        let mut rows = day(&["2026-09-21_09-00-00", "2026-09-21_09-00-05", "2026-09-21_09-00-10"], "Excel");
        rows.extend(day(&["2026-09-21_17-00-00"], "Chrome"));
        let strip = Timeline::sample(&rows, ts("2026-09-21_00-00-00"), ts("2026-09-21_23-59-59"), 8);
        assert_eq!(strip.points.len(), 2, "only two places have data within the snap window");
        assert_eq!(strip.points[0].title(), Some("Excel"));
        assert_eq!(strip.points[1].title(), Some("Chrome"));
        assert_eq!(strip.index_for(ts("2026-09-21_17-00-00")), Some(1));
        assert_eq!(strip.index_for(ts("2026-09-21_12-00-00")), None);
    }

    #[test]
    fn nearest_respects_its_window() {
        let rows = day(&["2026-09-21_10-00-00"], "Excel");
        assert!(nearest(&rows, ts("2026-09-21_10-02-00"), 300).is_some());
        assert!(nearest(&rows, ts("2026-09-21_10-06-00"), 300).is_none());
        assert!(nearest(&[], ts("2026-09-21_10-00-00"), 300).is_none());
    }

    #[test]
    fn index_sampling_degrades_gracefully() {
        let rows = day(&["2026-09-21_10-00-00", "2026-09-21_11-00-00"], "Excel");
        assert_eq!(evenly_by_index(&rows, 10).len(), 2, "fewer rows than slots returns them all");
        assert_eq!(evenly_by_index(&rows, 0).len(), 0);
        let stamps: Vec<String> = (0..100).map(|i| format!("2026-09-21_10-{:02}-{:02}", i / 60 % 60, i % 60)).collect();
        let refs: Vec<&str> = stamps.iter().map(String::as_str).collect();
        let many = day(&refs, "x");
        assert_eq!(evenly_by_index(&many, 10).len(), 10);
    }

    /// The behaviour the day-begin setting exists for: 01:00 on the 22nd is still the 21st's work.
    #[test]
    fn histogram_shifts_by_day_begin_minutes() {
        let rows = vec![
            row("2026-09-21_09-00-00", "Excel"),
            row("2026-09-21_23-00-00", "Excel"),
            row("2026-09-22_01-00-00", "Excel"),
            row("2026-09-22_10-00-00", "Excel"),
        ];
        let shifted = histogram(&rows, 180, PRESENCE);
        assert_eq!(shifted.len(), 2);
        assert_eq!(shifted[0].day, 21);
        assert_eq!(shifted[0].rows, 3, "01:00 belongs to the previous day");
        assert_eq!(shifted[1].day, 22);
        assert_eq!(shifted[1].rows, 1);

        let midnight = histogram(&rows, 0, PRESENCE);
        assert_eq!(midnight.iter().map(|d| (d.day, d.rows)).collect::<Vec<_>>(), vec![(21, 2), (22, 2)]);
    }

    #[test]
    fn a_histogram_month_rolls_over_a_year_boundary() {
        let rows = vec![row("2026-12-31_23-30-00", "x"), row("2027-01-01_10-00-00", "x")];
        let stats = histogram(&rows, 0, PRESENCE);
        assert_eq!(stats.iter().map(|s| (s.year, s.month, s.day)).collect::<Vec<_>>(), vec![
            (2026, 12, 31),
            (2027, 1, 1)
        ]);
    }

    #[test]
    fn title_intervals_merge_consecutive_rows_and_split_on_a_switch() {
        let rows = vec![
            row("2026-09-21_10-00-00", "Excel"),
            row("2026-09-21_10-01-00", "Excel"),
            row("2026-09-21_10-02-00", "Chrome"),
            row("2026-09-21_10-03-00", "Excel"),
        ];
        let intervals = title_intervals(&rows, 100);
        assert_eq!(intervals.len(), 3);
        assert_eq!(intervals[0].seconds(), 60);
        assert_eq!(intervals[1].title, "Chrome");
        assert_eq!(intervals[2].seconds(), 0);

        // Out-of-order input is sorted, so a batch appended across a segment boundary still reads right.
        let mut shuffled = rows.clone();
        shuffled.reverse();
        assert_eq!(title_intervals(&shuffled, 100).len(), 3);
    }

    #[test]
    fn a_long_gap_splits_two_sessions_of_the_same_title() {
        let rows = vec![row("2026-09-21_09-00-00", "Excel"), row("2026-09-21_17-00-00", "Excel")];
        let intervals = title_intervals(&rows, 100);
        assert_eq!(intervals.len(), 2);
        assert_eq!(intervals[0].seconds(), 0, "an 8-hour silence is not 8 hours of Excel");
    }

    #[test]
    fn untitled_rows_are_skipped_and_totals_sort_by_time() {
        let rows = vec![
            row("2026-09-21_10-00-00", ""),
            row("2026-09-21_10-00-30", "Excel"),
            row("2026-09-21_10-01-00", "Excel"),
            row("2026-09-21_10-01-30", "Chrome"),
        ];
        let totals = title_totals(&rows, 100);
        assert_eq!(totals, vec![("Excel".to_string(), 30), ("Chrome".to_string(), 0)]);
    }
}
