//! Where a frame's row lands on the timeline.
//!
//! `ocr_core_logic` computes a row's `videofile_time` from two numbers it already has and nothing
//! else: the timestamp in the video's filename, and the frame's own index divided by the recording
//! framerate. There is no demuxing, no container timestamp, no seek table — the filename *is* the
//! clock, which is why every other part of the product also parses it.
//!
//! Both numbers are in the app's naive-local epoch (`wind_base::clock`): `dtstr_to_seconds` subtracts
//! 1970-01-01 from a local wall clock as if it were UTC, so on a UTC+8 machine the stored value is
//! true-POSIX + 28800. Writing POSIX seconds here would put every re-indexed row eight hours from the
//! rows beside it, and the day views would silently show a different day.

use wind_base::clock::LocalParts;

use crate::text::round_half_even;

/// The length of the timestamp that opens every recording name: `%Y-%m-%d_%H-%M-%S`.
const STAMP_LEN: usize = 19;

/// The wall-clock second a segment started, read out of its filename.
///
/// `utils.dtstr_to_seconds(os.path.splitext(name)[0].replace("-INDEX", ""))`. Anchoring on the first
/// nineteen characters subsumes that `replace` and does one better: upstream's `strptime` rejects the
/// whole string when the file carries a `-ERROR1` suffix, so a retry of a failed video raised
/// `ValueError` inside the indexing loop and the file was renamed to `-ERROR2` without ever being
/// re-indexed. The same name works here.
pub fn segment_start_seconds(video_file_name: &str) -> Option<i64> {
    let stem = video_file_name.rsplit_once('.').map_or(video_file_name, |(stem, _)| stem);
    let stamp: String = stem.chars().take(STAMP_LEN).collect();
    LocalParts::from_stamp(&stamp).map(|parts| parts.naive_epoch_seconds())
}

/// Seconds from the start of the segment at which a given source frame was captured.
///
/// `round(int(frame_index) / int(config.record_framerate))` — and the rounding is Python's
/// round-half-to-even, which is what [`round_half_even`] reproduces. With the shipped framerate of 2
/// every odd frame index sits exactly on `.5`, so the tie-breaking rule is not academic: half of an
/// AV1 segment's key-frame rows move by a second under Rust's `f64::round`.
pub fn frame_offset_seconds(frame_index: i64, framerate: i64) -> i64 {
    if framerate <= 0 {
        return 0;
    }
    round_half_even(frame_index as f64 / framerate as f64)
}

/// The `videofile_time` for one frame of one segment.
pub fn row_time(video_file_name: &str, frame_index: i64, framerate: i64) -> Option<i64> {
    Some(segment_start_seconds(video_file_name)? + frame_offset_seconds(frame_index, framerate))
}

/// The calendar month a stored timestamp belongs to, which decides which `userdata/db` file receives
/// the row — upstream opens the database per row by `seconds_to_datetime`.
pub fn month_of(time: i64) -> (i64, u32) {
    let parts = LocalParts::from_naive_epoch(time);
    (parts.year, parts.month)
}

/// A segment's own start month, from its filename, for the rollback that has to find the rows a
/// crashed run left behind before any of this run's frames existed.
pub fn segment_start_month(video_file_name: &str) -> Option<(i64, u32)> {
    Some(month_of(segment_start_seconds(video_file_name)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pair the live database holds, quoted from
    /// `userdata/db/default_2026-09_wind.db`: the row named `2026-09-21_21-16-12.mp4` carries
    /// `videofile_time = 1790025372`. It is frame 0 of that segment, so this is the whole of the
    /// filename-to-epoch convention in one number — get the naive-local interpretation wrong on this
    /// UTC+8 box and the assertion moves by 28800.
    #[test]
    fn a_real_row_from_a_real_database_anchors_the_epoch() {
        assert_eq!(segment_start_seconds("2026-09-21_21-16-12.mp4"), Some(1_790_025_372));
        assert_eq!(row_time("2026-09-21_21-16-12.mp4", 0, 2), Some(1_790_025_372));
    }

    #[test]
    fn the_offset_ignores_every_suffix_a_segment_picked_up() {
        // The name on disk during a run is `-INDEX`, and after a failed one `-ERROR1`; upstream's
        // `replace("-INDEX","")` handles the first and crashes on the second.
        for name in [
            "2026-09-21_21-16-12.mp4",
            "2026-09-21_21-16-12-INDEX.mp4",
            "2026-09-21_21-16-12-ERROR1.mp4",
            "2026-09-21_21-16-12-OCRED.mp4",
        ] {
            assert_eq!(segment_start_seconds(name), Some(1_790_025_372), "{name}");
        }
    }

    #[test]
    fn frames_map_to_seconds_at_the_recording_rate() {
        assert_eq!(frame_offset_seconds(0, 2), 0);
        assert_eq!(frame_offset_seconds(8, 2), 4, "the default stride's second kept frame");
        assert_eq!(frame_offset_seconds(240, 2), 120);
        assert_eq!(frame_offset_seconds(100, 0), 0, "a zero framerate must not divide");
        assert_eq!(frame_offset_seconds(100, -1), 0);
        // Half-to-even, not half-up.
        assert_eq!(frame_offset_seconds(1, 2), 0);
        assert_eq!(frame_offset_seconds(3, 2), 2);
        assert_eq!(frame_offset_seconds(5, 2), 2);
    }

    #[test]
    fn a_row_time_is_the_segment_start_plus_the_frame_offset() {
        assert_eq!(row_time("2026-09-21_21-16-12.mp4", 8, 2), Some(1_790_025_376));
        assert_eq!(row_time("2026-12-31_23-59-00.mp4", 120, 2), Some(1_798_761_600));
        assert_eq!(row_time("holiday.mp4", 8, 2), None, "a name with no stamp has no timeline");
    }

    #[test]
    fn the_month_is_the_store_a_row_belongs_in() {
        let (y, m) = month_of(1_790_025_372);
        assert_eq!((y, m), (2026, 9));
        // 2026-09-30 23:59:59 and one second later, across the file boundary.
        let end_of_month = LocalParts::from_stamp("2026-09-30_23-59-59").unwrap().naive_epoch_seconds();
        assert_eq!(month_of(end_of_month), (2026, 9));
        assert_eq!(month_of(end_of_month + 1), (2026, 10));
        assert_eq!(segment_start_month("2026-09-21_21-16-12-INDEX.mp4"), Some((2026, 9)));
        assert_eq!(segment_start_month("nonsense.mp4"), None);
    }

    /// A segment recorded at 23:59 with a 900-second length puts its last rows in next month's file,
    /// which is why the writer routes per row rather than per segment.
    #[test]
    fn a_segment_crossing_midnight_routes_its_rows_to_two_files() {
        let name = "2026-09-30_23-55-00.mp4";
        assert_eq!(row_time(name, 0, 2).map(month_of), Some((2026, 9)));
        assert_eq!(row_time(name, 600, 2).map(month_of), Some((2026, 10)));
    }
}
