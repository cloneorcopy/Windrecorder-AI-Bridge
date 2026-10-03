//! What is waiting to be organised, counted without organising any of it.
//!
//! The button that starts the deferred pass used to be a guess: nine steps, and no way to know before
//! pressing it whether there is one slice to encode or eleven days of unsummarised footage. This is the
//! census the window shows beside the button — every number produced by the same selector the step
//! itself uses, in dry-run mode, so the answer cannot drift from the work.
//!
//! Two rules hold here and nowhere else in this file's callers:
//!
//!   * **Nothing is written.** Each step is asked in dry-run, the reindex count is a directory listing,
//!     and no AI step is asked at all — a census that spent a token would be the thing the user was
//!     trying to decide about, not an input to the decision.
//!   * **Nothing is claimed.** No maintain lock is taken, because a read-only walk that blocks the real
//!     pass would be worse than no census, and because the two cannot disagree: a pass that runs while
//!     this counts is changing the numbers, and the window says the census is a moment, not a promise.
//!
//! The output is one JSON object, on the **last** line of stdout. The steps' own dry-run prose comes
//! before it, and that is deliberate: counting with the step's own selector means counting with the step,
//! reporting included. A reader takes the last line and gets a field name and a number, never a sentence
//! to parse.
//!
//! The same walk answers [`wind_base::maintain::set_totals`] at the moment a pass starts, which is why
//! the counts live in [`Census`] rather than inside [`report`]: the bar's four denominators and the
//! numbers beside the button must be the *same* count, or the pass promises a queue the button did not
//! show and shows a queue the pass never promised to do.

use std::path::Path;

use serde_json::json;
use wind_base::config::Config;
use wind_base::maintain::Totals;

/// How many product days back the summary census looks.
///
/// The same horizon the summariser itself scans back (`wind_ai::summarize::PENDING_SCAN_DAYS` is the
/// ceiling on that queue), capped lower here because a census runs on a click and the far days are not
/// what somebody deciding whether to press 立刻整理 is asking about.
pub(crate) const SUMMARY_DAYS: usize = 14;

/// Every queue, counted once by the step that would work it.
///
/// The names are the steps' own `Outcome` fields, because a census is the steps asked in dry run: renaming
/// a number here would mean the report and the bar stop agreeing with the step that produced both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Census {
    // `text`.
    pub text_rows: usize,
    pub text_fillable: usize,
    pub text_no_masked_copy: usize,
    // `convert`.
    pub convert_slices: usize,
    pub convert_frames: usize,
    pub convert_seconds: i64,
    // `reindex`, which is a listing rather than a dry run: the walk declines to rehearse.
    pub reindex_videos: usize,
    // `expire`.
    pub expire_to_compress: usize,
    pub expire_to_delete: usize,
    pub expire_rows_to_drop: usize,
    pub expire_slice_dirs_to_sweep: usize,
    // `previews`.
    pub preview_rows: usize,
    pub preview_missing_source: usize,
    // `ai-summaries`, from the index and the prompt digests rather than from anybody's cursor.
    pub summary_stretches: usize,
}

impl Census {
    /// The four denominators the pass publishes, out of this same walk.
    ///
    /// Each leg is counted in the unit its own step reports an item in — a leg whose total was counted in
    /// a different unit from its items is a bar that reads 8896 of 70, which is the same lie as a bar that
    /// recedes. So two queues are deliberately *not* here:
    ///
    ///   * `reindex` counts the rows it lifts out of a video and the census counts videos. Its rows are
    ///     published as the step's own number (`wind_base::maintain::add_step_items`), not as a leg.
    ///   * `backup` copies month files and nothing here counts them; a denominator of `0` is the honest
    ///     answer, and the ADR says a leg counted at nothing is not drawn at all.
    pub fn totals(&self) -> Totals {
        Totals {
            text: self.text_rows,
            convert: self.convert_slices,
            ai: self.summary_stretches,
            other: self.expire_to_compress + self.preview_rows,
        }
    }

    /// Every count, as one JSON line. The key order and the field names are what the settings page
    /// parses, so they are pinned by a test rather than by memory.
    pub fn to_json(&self) -> String {
        json!({
            "text": {
                "waiting": self.text_rows,
                "fillable": self.text_fillable,
                "noMaskedCopy": self.text_no_masked_copy
            },
            "convert": {
                "slices": self.convert_slices,
                "frames": self.convert_frames,
                "seconds": self.convert_seconds
            },
            "reindex": { "videos": self.reindex_videos },
            "expire": {
                "toCompress": self.expire_to_compress,
                "toDelete": self.expire_to_delete,
                "rowsToDrop": self.expire_rows_to_drop,
                "sliceDirsToSweep": self.expire_slice_dirs_to_sweep
            },
            "previews": { "rows": self.preview_rows, "missingSource": self.preview_missing_source },
            "summaries": { "stretches": self.summary_stretches }
        })
        .to_string()
    }
}

/// Walk every queue. [`report`] is this and one line of JSON; a pass that wants the numbers as numbers
/// calls this and does not parse its own output.
pub fn census(root: &Path, config: &Config) -> Result<Census, String> {
    let text = crate::text::run(config, true, None)?;
    let convert = crate::convert::run(root, config, true, None)?;
    let expire = crate::expire::run(root, config, true, None)?;
    let previews = crate::previews::run(config, true, None)?;

    // The reindexer declines a dry run — it renames files, so there is nothing to rehearse — and what it
    // would pick up is a listing: a segment still carrying `-VIDEO` has never been indexed.
    //
    // It is not only a listing any more. A segment the recorder already wrote rows for, and whose rows
    // the text step has read, is footage the user can already search, and the step marks it and moves on
    // — so the census asks the step's own question of each candidate, through the same
    // [`wind_store::maintain::segment_coverage`], and counts only the videos the walk would really work.
    // A number beside the button that the pass then does not do is exactly the lie this file exists to
    // avoid.
    let unindexed = unindexed_videos(&config.videos_dir(), config);

    Ok(Census {
        text_rows: text.rows,
        text_fillable: text.filled,
        text_no_masked_copy: text.missing_copy,
        convert_slices: convert.converted,
        convert_frames: convert.frames,
        convert_seconds: convert.seconds,
        reindex_videos: unindexed,
        expire_to_compress: expire.segments_compressed,
        expire_to_delete: expire.segments_deleted,
        expire_rows_to_drop: expire.rows_deleted,
        expire_slice_dirs_to_sweep: expire.slices_removed,
        preview_rows: previews.planned,
        preview_missing_source: previews.missing_source,
        summary_stretches: pending_summaries(config),
    })
}

/// Every count, as one JSON line on the last line of what this returns.
pub fn report(root: &Path, config: &Config) -> Result<String, String> {
    census(root, config).map(|census| census.to_json())
}

/// Segments the reindex step would actually work: a `.mp4` whose name carries no pipeline marker *and*
/// whose footage the index has not already read.
///
/// The name test is the reindexer's own (`-OCRED` done, `-INDEX` in somebody's hands, `-ERRORn` capped).
/// The second half is [`wind_store::maintain::segment_coverage`] — the same call the step makes, from the
/// same file, so a census can never promise a video the walk will then skip. A candidate whose index
/// cannot be read counts as work, which is the step's own fall-back: reading an hour twice is cheaper
/// than hiding footage.
fn unindexed_videos(dir: &Path, config: &Config) -> usize {
    let Ok(months) = std::fs::read_dir(dir) else { return 0 };
    months
        .flatten()
        .filter_map(|month| {
            let entries = std::fs::read_dir(month.path()).ok()?;
            Some(entries.flatten().filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.ends_with(".mp4") || name.contains("-OCRED") || name.contains("-INDEX") || name.contains("-ERROR") {
                    return false;
                }
                !already_searchable(config, &name)
            }))
        })
        .flatten()
        .count()
}

/// Does the index already hold this segment, read?
///
/// The month list is the segment's own span, walked rather than guessed: a video recorded at 23:50 under
/// a 900-second cap has rows in two month files, and asking only the first would answer "uncovered" for
/// the tail and re-OCR an hour the user can already search.
fn already_searchable(config: &Config, name: &str) -> bool {
    use wind_store::maintain::SegmentCoverage;
    let Some(stamp) = wind_base::paths::segment_stamp_of(name) else { return false };
    let Some(parts) = wind_base::clock::LocalParts::from_stamp(&stamp) else { return false };
    let start = parts.naive_epoch_seconds();
    let end = start + config.i64_or("record_seconds", 900).max(0);
    let first = (parts.year, parts.month);
    let last = {
        let at = wind_base::clock::LocalParts::from_naive_epoch(end.max(start));
        (at.year, at.month)
    };
    let mut months = Vec::new();
    let mut current = first;
    while current <= last {
        months.push(current);
        current = if current.1 == 12 { (current.0 + 1, 1) } else { (current.0, current.1 + 1) };
    }
    matches!(
        wind_store::maintain::segment_coverage(
            &config.db_dir(),
            &config.user_name(),
            &months,
            &config.cache_screenshot_dir(),
            name,
        ),
        SegmentCoverage::Read { .. } | SegmentCoverage::Waiting { slice_on_disk: true, .. }
    )
}

/// How many stretches in the last [`SUMMARY_DAYS`] product days have no summary the index still agrees
/// with.
///
/// Built from the same digest the summariser compares against, so a paragraph an outside AI wrote over
/// the bridge counts exactly as much as one this machine wrote: the queue is derived from the index and
/// the files, never from who produced what.
fn pending_summaries(config: &Config) -> usize {
    use wind_summary as summary;
    let prompts = wind_base::prompts::Prompts::read(config);
    let digests = wind_ai::summarize::digests(&prompts);
    let reader = summary::Reader::fresh(config);
    let shift = config.day_begin_minutes();
    let mut instant = wind_base::clock::now().naive_epoch_seconds();
    let mut walked = String::new();
    let mut pending = 0usize;
    // Stepping the *instant* rather than the date is what keeps the product's own day boundary honest:
    // a day that begins at 03:00 has 21 hours of the next calendar date inside it, and subtracting one
    // calendar day twice would ask about the same day and skip another.
    for _ in 0..SUMMARY_DAYS {
        let day = summary::day_of(instant, shift);
        if day != walked {
            walked = day.clone();
            // A day whose index cannot be opened is not a census failure: the number the window shows is
            // a floor, and a missing month file at the far end of the walk is the ordinary case.
            if let Ok(queue) = summary::for_day_with(&reader, &day, &digests) {
                pending += queue.pending.len();
            }
        }
        instant -= 86_400;
    }
    pending
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A census that wrote something would be the bug this test exists to catch, so the whole report is
    /// run against a throwaway install and the tree is compared with itself.
    #[test]
    fn the_census_answers_and_touches_nothing() {
        let root = std::env::temp_dir().join(format!("windmaint-backlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::create_dir_all(root.join("userdata/db")).unwrap();
        std::fs::create_dir_all(root.join("userdata/videos")).unwrap();
        std::fs::write(root.join("config_src/config_default.json"), b"{}").unwrap();

        let before = listing(&root);
        let config = Config::load(&root).unwrap();
        let body = report(&root, &config).expect("an empty install still has an answer");
        let json: serde_json::Value = serde_json::from_str(&body).expect("one JSON object");
        for key in ["text", "convert", "reindex", "expire", "previews", "summaries"] {
            assert!(json.get(key).is_some(), "{key} is missing from {body}");
        }
        assert_eq!(json["reindex"]["videos"].as_u64(), Some(0), "nothing is on disk to index");
        assert_eq!(json["text"]["waiting"].as_u64(), Some(0), "and no rows are waiting");
        // The same walk fixes the pass's four denominators. Read the line the page parses back into the
        // four counters and compare: one queue, two readers, and any second enumeration would show up
        // here as a disagreement rather than as a bar that quietly promises the wrong number.
        let counted = census(&root, &config).expect("an empty install still has an answer");
        assert_eq!(counted.totals(), totals_from_json(&json), "the bar's denominators are the button's numbers");
        assert_eq!(counted.totals(), Totals::default(), "and an install with nothing waiting promises no bar");
        assert_eq!(listing(&root), before, "a census writes nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The four counters, as a reader of the published JSON line would build them: what `totals()` is
    /// supposed to do, written against the field names rather than the Rust ones so a mapping that
    /// started reading a different field would be caught.
    fn totals_from_json(json: &serde_json::Value) -> Totals {
        let number = |value: &serde_json::Value| value.as_u64().unwrap_or(0) as usize;
        Totals {
            text: number(&json["text"]["waiting"]),
            convert: number(&json["convert"]["slices"]),
            ai: number(&json["summaries"]["stretches"]),
            other: number(&json["expire"]["toCompress"]) + number(&json["previews"]["rows"]),
        }
    }

    /// The line the settings page parses, pinned byte for byte — including the key order, which a `json!`
    /// built from a struct can silently lose. The pass's own four counters are read out of the same walk,
    /// so a field that moved here would have moved the bar too, and the two would disagree.
    #[test]
    fn the_json_line_is_the_one_the_settings_page_parses() {
        let census = Census {
            text_rows: 3_799,
            text_fillable: 3_700,
            text_no_masked_copy: 99,
            convert_slices: 70,
            convert_frames: 14_236,
            convert_seconds: 56_940,
            reindex_videos: 6,
            expire_to_compress: 4,
            expire_to_delete: 2,
            expire_rows_to_drop: 811,
            expire_slice_dirs_to_sweep: 12,
            preview_rows: 23,
            preview_missing_source: 1,
            summary_stretches: 9,
        };
        assert_eq!(
            census.to_json(),
            r#"{"convert":{"frames":14236,"seconds":56940,"slices":70},"expire":{"rowsToDrop":811,"sliceDirsToSweep":12,"toCompress":4,"toDelete":2},"previews":{"missingSource":1,"rows":23},"reindex":{"videos":6},"summaries":{"stretches":9},"text":{"fillable":3700,"noMaskedCopy":99,"waiting":3799}}"#,
        );
        // The ADR's four rows, in the units each step publishes an item in.
        assert_eq!(
            census.totals(),
            Totals { text: 3_799, convert: 70, ai: 9, other: 4 + 23 },
            "the bar's denominators are this walk's own numbers, not a second count of the same queues"
        );
    }

    /// The two queues left out of the denominators on purpose, named so they are not "restored" later:
    /// `reindex` counts rows against a census of videos, and `backup` is not counted at all.
    #[test]
    fn a_leg_whose_items_are_not_in_its_unit_gets_no_denominator() {
        let census = Census { reindex_videos: 70, preview_rows: 8, expire_to_compress: 1, ..Census::default() };
        assert_eq!(census.totals().other, 9, "the two queues counted in the unit their steps report in");
        assert_eq!(census.totals().text, 0, "the reindex videos are not rows of screen text");
        assert_eq!(census.totals().convert, 0, "and they are not segments to encode either");
    }

    /// The reindex count is a name test *and* an index test, so it has to ignore the three markers the
    /// pipeline writes, count everything else on an install whose index has never heard of them — and on
    /// a tree with no videos at all it must say zero rather than error.
    #[test]
    fn unindexed_counts_the_names_the_reindexer_would_pick_up() {
        let root = std::env::temp_dir().join(format!("windmaint-backlog-videos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(root.join("config_src/config_default.json"), "{}").unwrap();
        let config = Config::load(&root).unwrap();
        let month = root.join("2026-09");
        std::fs::create_dir_all(&month).unwrap();
        for name in [
            "2026-09-01_10-00-00-VIDEO.mp4",
            "2026-09-01_11-00-00-OCRED.mp4",
            "2026-09-01_12-00-00-INDEX.mp4",
            "2026-09-01_13-00-00-ERROR1.mp4",
            "2026-09-01_14-00-00.mp4",
            "notes.txt",
        ] {
            std::fs::write(month.join(name), b"x").unwrap();
        }
        assert_eq!(unindexed_videos(&root, &config), 2, "the two names without a pipeline marker are the work");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The census and the step must not disagree: a segment the index has read is not work, the same
    /// segment with a row still waiting is not work either while its slice is on disk, and it is work
    /// again once the slice is gone and only the video holds the words.
    #[test]
    fn the_census_skips_the_footage_the_step_will_skip_and_only_then_asks_for_the_video() {
        let root = std::env::temp_dir().join(format!("windmaint-backlog-covered-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config_src")).unwrap();
        std::fs::write(root.join("config_src/config_default.json"), "{}").unwrap();
        let config = Config::load(&root).unwrap();
        let library = root.join("userdata/videos/2026-09");
        let cache = root.join("cache_screenshot");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let video = "2026-09-21_21-16-12.mp4";
        std::fs::write(library.join(video), b"footage").unwrap();
        let db = config.db_dir().join(wind_base::paths::month_filename(&config.user_name(), 2026, 9));
        std::fs::create_dir_all(config.db_dir()).unwrap();

        // Nothing in the index: the walk owns this video, and the census says so.
        assert_eq!(unindexed_videos(&config.videos_dir(), &config), 1, "an index that never heard of it");

        let read = wind_store::Record {
            videofile_name: video.into(),
            picturefile_name: "2026-09-21_21-16-12.jpg".into(),
            videofile_time: wind_base::clock::LocalParts::from_stamp("2026-09-21_21-16-12").unwrap().naive_epoch_seconds(),
            ocr_text: "a screen the text step already read".into(),
            win_title: None,
            deep_linking: None,
            thumbnail: None,
        };
        let mut store = wind_store::write::Store::open(&db).unwrap();
        store.append(&[read.clone()]).unwrap();
        drop(store);
        assert_eq!(unindexed_videos(&config.videos_dir(), &config), 0, "searchable footage is not work");

        // One row waiting, and the slice still on disk: the text step can still reach it, so the step
        // will skip it — and the census skips it too.
        let mut store = wind_store::write::Store::open(&db).unwrap();
        store
            .append(&[wind_store::Record { ocr_text: String::new(), ..read.clone() }])
            .unwrap();
        drop(store);
        std::fs::create_dir_all(cache.join("2026-09-21_21-16-12")).unwrap();
        assert_eq!(unindexed_videos(&config.videos_dir(), &config), 0, "the text step still owns these frames");

        // Same rows, slice swept away: now the video is the only copy of the words left.
        let _ = std::fs::remove_dir_all(cache.join("2026-09-21_21-16-12"));
        assert_eq!(unindexed_videos(&config.videos_dir(), &config), 1, "stranded rows are the back-index's job");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn listing(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut walk = vec![root.to_path_buf()];
        while let Some(dir) = walk.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                out.push(path.strip_prefix(root).unwrap().display().to_string());
                if path.is_dir() {
                    walk.push(path);
                }
            }
        }
        out.sort();
        out
    }
}
