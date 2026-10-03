//! Where a bookmark lands on the day view's timeline strip.
//!
//! A day's strip is one image whose x axis is *recorded* time: upstream builds it from the day's
//! first and last captured row and puts a flag at
//!
//! ```text
//! ratio = (flag_seconds - day_min_seconds) / (day_max_seconds - day_min_seconds)
//! x     = int(strip_width * ratio)
//! ```
//!
//! drawing, at that x, a 4 px bar of `(255, 0, 0, 200)` down the full height plus a right-pointing
//! triangle in the upper half (`add_visual_mark_on_oneday_timeline_thumbnail`). The bar and the
//! triangle are pasted from one square canvas `strip_height` on a side, which is why the geometry
//! below reports a canvas as well as the two shapes inside it.
//!
//! Two deviations from upstream, both deliberate:
//!
//!   * **the ends are inclusive.** Upstream tests `day_min < t < day_max`, so a flag taken at the
//!     day's very first or very last captured instant — the common case for "bookmark what I am
//!     looking at right now", since the last row is the newest one — is silently not drawn. The
//!     endpoints are inside the day by definition, so they are drawn here.
//!   * **the bar is clamped into the strip.** `int(width * 1.0)` is `width`, i.e. one pixel past the
//!     last column, and a flag at the end of the day was invisible. Clamping moves it by less than
//!     `BAR_WIDTH` pixels, which cannot put it on the wrong hour, whereas dropping it off the edge
//!     loses the bookmark entirely.
//!
//! A flag *outside* the span is rejected rather than clamped, and says so. Proportionality is only
//! meaningful between the two captured instants the strip was built from; pinning a bookmark from
//! another day to an edge would draw a line that lies about where the moment was, and the caller can
//! always say "1 flag is outside the recorded range" instead.

use wind_base::clock::{self, LocalParts};

use crate::flag::Flag;

/// Upstream's `mark_width`, in pixels.
pub const BAR_WIDTH: u32 = 4;

/// Upstream's `mark_color`, RGBA. Kept here so the strip and its bookmarks are one decision.
pub const BAR_COLOR: [u8; 4] = [255, 0, 0, 200];

/// The span the strip's x axis covers, inclusive at both ends.
///
/// Both numbers are on the app's naive-local epoch (`wind_base::clock`) — the same axis
/// `video_text.videofile_time` and [`Flag::epoch`] are on — so a marker is computed from the values
/// that come out of the index with no conversion anywhere in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaySpan {
    pub from: i64,
    pub to: i64,
}

impl DaySpan {
    pub const fn new(from: i64, to: i64) -> DaySpan {
        DaySpan { from, to }
    }

    /// The whole product day — `[day 03:00:00, next day 02:59:59]` at the shipped
    /// `day_begin_minutes` — for a strip that spans the day rather than the captured range.
    pub fn product_day(date: LocalParts, day_begin_minutes: i64) -> DaySpan {
        let (from, to) = clock::day_bounds(date.year, date.month, date.day, day_begin_minutes);
        DaySpan { from, to }
    }

    /// Inclusive, which is the point of the type: a day's last second belongs to the day.
    pub const fn contains(&self, time: i64) -> bool {
        time >= self.from && time <= self.to
    }

    pub const fn seconds(&self) -> i64 {
        self.to - self.from
    }

    /// Is the range wide enough to be proportional across? A day whose only capture is one instant
    /// has no axis, and inventing one is how a bookmark ends up drawn at "midday" by accident.
    pub const fn is_measurable(&self) -> bool {
        self.to > self.from
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i64,
    pub y: i64,
}

impl Point {
    pub const fn new(x: i64, y: i64) -> Point {
        Point { x, y }
    }
}

/// Half-open on the right, so `x + width` is the first column *not* covered and two adjacent
/// rectangles cannot both claim a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl Rect {
    pub const fn new(x: i64, y: i64, width: i64, height: i64) -> Rect {
        Rect { x, y, width, height }
    }

    pub const fn right(&self) -> i64 {
        self.x + self.width
    }

    pub const fn bottom(&self) -> i64 {
        self.y + self.height
    }

    /// The part of this rectangle that lies inside a strip of `width` columns.
    pub fn clipped_to_width(&self, width: i64) -> Option<Rect> {
        if self.x >= width || self.right() <= 0 {
            return None;
        }
        let right = self.right().min(width);
        Some(Rect { x: self.x.max(0), y: self.y, width: right - self.x.max(0), height: self.height })
    }
}

/// One drawn bookmark.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Marker {
    /// Position in the caller's list of times, so a marker can be traced back to the row it came
    /// from without this module knowing what a row is.
    pub index: usize,
    pub time: i64,
    /// Where along the span the instant sits, `0.0` at `span.from`. Reported with the position
    /// because it is the number that shows a proportionality bug; `x` alone could be off by a
    /// rounding and still look plausible.
    pub ratio: f64,
    pub x: i64,
    /// The square `strip_height` on a side that upstream pastes at `(x, 0)`; the bar and the
    /// triangle both live inside it.
    pub canvas: Rect,
    /// The pole.
    pub bar: Rect,
    /// The banner: pole-top, apex, pole-middle — in the strip's own coordinates, not the canvas'.
    pub triangle: [Point; 3],
    /// `canvas` reaches past the strip's right edge, so a blit must clip it.
    pub clipped: bool,
}

/// Everything one strip needs: what to draw, and what was refused.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub span: DaySpan,
    pub width: u32,
    pub height: u32,
    pub markers: Vec<Marker>,
    /// `(position in the input, time)` for every flag outside the span, in input order.
    pub outside: Vec<(usize, i64)>,
}

impl Layout {
    pub fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }
}

/// `0.0` at the start of the span, `1.0` at the end; `None` when the flag is outside an
/// unmeasurable or degenerate range.
pub fn ratio_at(span: DaySpan, time: i64) -> Option<f64> {
    if !span.is_measurable() || !span.contains(time) {
        return None;
    }
    Some((time - span.from) as f64 / span.seconds() as f64)
}

/// The left column of a marker's canvas. See the module note for the clamp.
pub fn position_x(span: DaySpan, time: i64, width: u32) -> Option<i64> {
    let ratio = ratio_at(span, time)?;
    let width = width as i64;
    // Truncation, as upstream's `int()` did: rounding up would let a flag a second from the end of
    // a day land a whole pixel further along than its own ratio says.
    let raw = (ratio * width as f64) as i64;
    Some(raw.clamp(0, (width - i64::from(BAR_WIDTH)).max(0)))
}

/// The inverse, for hit-testing a click: the instant column `x` stands for.
///
/// Within half a pixel of the true time, and of a marker's own position only when the click is
/// measured from the marker's left edge — which is what a pointer over the bar gives you.
pub fn time_at(span: DaySpan, width: u32, x: i64) -> i64 {
    if !span.is_measurable() || width == 0 {
        return span.from;
    }
    let ratio = (x as f64 / width as f64).clamp(0.0, 1.0);
    span.from + (ratio * span.seconds() as f64).round() as i64
}

/// Every marker for one strip. `times` is the caller's ordered list; the returned [`Marker::index`]
/// and [`Layout::outside`] refer to positions in it.
pub fn layout(span: DaySpan, times: &[i64], width: u32, height: u32) -> Layout {
    let mut markers = Vec::new();
    let mut outside = Vec::new();
    for (index, time) in times.iter().copied().enumerate() {
        match (ratio_at(span, time), position_x(span, time, width)) {
            (Some(ratio), Some(x)) => markers.push(marker_at(index, time, x, ratio, width, height)),
            _ => outside.push((index, time)),
        }
    }
    Layout { span, width, height, markers, outside }
}

/// The same, from the table: one entry per flag, in the order given.
pub fn layout_flags<'a>(span: DaySpan, flags: impl IntoIterator<Item = &'a Flag>, width: u32, height: u32) -> Layout {
    let times: Vec<i64> = flags.into_iter().map(|flag| flag.epoch()).collect();
    layout(span, &times, width, height)
}

fn marker_at(index: usize, time: i64, x: i64, ratio: f64, width: u32, height: u32) -> Marker {
    let height = height as i64;
    let bar_width = i64::from(BAR_WIDTH);
    // Upstream draws the triangle at `h/2` and `h/4` in float coordinates; a blit needs integers,
    // and half a pixel of apex is nowhere near the resolution a bookmark is judged at.
    let (half, quarter) = ((height as f64 / 2.0).round() as i64, (height as f64 / 4.0).round() as i64);
    Marker {
        index,
        time,
        ratio,
        x,
        canvas: Rect::new(x, 0, height, height),
        bar: Rect::new(x, 0, bar_width, height),
        triangle: [Point::new(x + bar_width, 0), Point::new(x + half, quarter), Point::new(x + bar_width, half)],
        clipped: x + height > width as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A measurable, ordinary day: 08:00 to 16:00, eight hours, four hundred and eighty `minute`s
    /// of axis to spread across a thousand pixel columns.
    fn day() -> DaySpan {
        let from = LocalParts::from_stamp("2026-09-21_08-00-00").unwrap().naive_epoch_seconds();
        let to = LocalParts::from_stamp("2026-09-21_16-00-00").unwrap().naive_epoch_seconds();
        DaySpan::new(from, to)
    }

    const WIDTH: u32 = 1000;
    const HEIGHT: u32 = 40;

    fn at(time: &str) -> i64 {
        LocalParts::from_stamp(time).unwrap().naive_epoch_seconds()
    }

    #[test]
    fn the_span_is_eight_hours_and_the_start_is_the_left_edge() {
        let span = day();
        assert_eq!(span.seconds(), 8 * 3600);
        assert_eq!(ratio_at(span, span.from), Some(0.0));
        assert_eq!(position_x(span, span.from, WIDTH), Some(0));
        let marker = layout(span, &[span.from], WIDTH, HEIGHT).markers.remove(0);
        assert_eq!(marker.ratio, 0.0);
        assert_eq!(marker.canvas, Rect::new(0, 0, 40, 40));
        assert!(!marker.clipped);
    }

    #[test]
    fn the_middle_of_the_day_is_the_middle_of_the_strip() {
        let span = day();
        let noon = at("2026-09-21_12-00-00");
        assert_eq!(ratio_at(span, noon), Some(0.5));
        assert_eq!(position_x(span, noon, WIDTH), Some(500));
        assert_eq!(layout(span, &[noon], WIDTH, HEIGHT).markers[0].x, 500);
    }

    /// The clamp and the inclusive end are the same decision: a flag at `span.to` must be *drawn*.
    #[test]
    fn the_end_of_the_day_lands_on_the_last_column_that_can_show_a_bar() {
        let span = day();
        assert_eq!(ratio_at(span, span.to), Some(1.0));
        assert_eq!(position_x(span, span.to, WIDTH), Some(996));
        let marker = layout(span, &[span.to], WIDTH, HEIGHT).markers.remove(0);
        assert_eq!(marker.bar, Rect::new(996, 0, 4, 40));
        assert_eq!(marker.bar.right(), 1000, "the bar ends exactly at the strip's edge");
        assert!(marker.clipped, "the square canvas cannot fit in the last 40 columns");
    }

    #[test]
    fn a_proportion_of_the_day_is_a_proportion_of_the_strip() {
        let span = day();
        let quarter = at("2026-09-21_10-00-00");
        let three_quarters = at("2026-09-21_14-00-00");
        assert_eq!(position_x(span, quarter, WIDTH), Some(250));
        assert_eq!(position_x(span, three_quarters, WIDTH), Some(750));
        // One hour of an eight-hour day, at either end, is one eighth of a thousand columns.
        assert_eq!(position_x(span, at("2026-09-21_09-00-00"), WIDTH), Some(125));
        assert_eq!(position_x(span, at("2026-09-21_15-00-00"), WIDTH), Some(875));
    }

    /// The choice, stated: a flag from another day is *not* pinned to an edge. See the module note.
    #[test]
    fn a_flag_outside_the_day_is_refused_not_clamped() {
        let span = day();
        for time in [span.from - 1, span.from - 3600, span.to + 1, span.to + 86_400] {
            assert_eq!(ratio_at(span, time), None);
            assert_eq!(position_x(span, time, WIDTH), None);
        }
        let plan = layout(span, &[span.from - 1, at("2026-09-21_12-00-00"), span.to + 1], WIDTH, HEIGHT);
        assert_eq!(plan.markers.len(), 1);
        assert_eq!(plan.markers[0].index, 1, "the survivor keeps its own position in the input");
        assert_eq!(plan.outside, vec![(0, span.from - 1), (2, span.to + 1)]);
    }

    /// Upstream tested `day_min < t < day_max`, so the two endpoints vanished from the strip; these
    /// are the rows a user is most likely to have flagged.
    #[test]
    fn a_flag_at_the_first_or_last_capture_is_drawn() {
        let span = day();
        let plan = layout(span, &[span.from, span.to], WIDTH, HEIGHT);
        assert_eq!(plan.markers.len(), 2);
        assert!(plan.outside.is_empty());
    }

    #[test]
    fn a_day_with_no_range_has_no_axis_to_be_proportional_on() {
        let instant = at("2026-09-21_12-00-00");
        let span = DaySpan::new(instant, instant);
        assert!(!span.is_measurable());
        let plan = layout(span, &[instant], WIDTH, HEIGHT);
        assert!(plan.markers.is_empty());
        assert_eq!(plan.outside, vec![(0, instant)]);
        // A reversed span is as degenerate as a zero-length one, not a negative ratio.
        assert!(layout(DaySpan::new(instant + 1, instant), &[instant], WIDTH, HEIGHT).markers.is_empty());
    }

    #[test]
    fn the_bar_and_the_banner_are_where_upstream_drew_them() {
        let span = day();
        let noon = at("2026-09-21_12-00-00");
        let marker = layout(span, &[noon], WIDTH, HEIGHT).markers.remove(0);
        assert_eq!(marker.bar, Rect::new(500, 0, 4, 40), "4 px wide, the full height of the strip");
        assert_eq!(marker.canvas, Rect::new(500, 0, 40, 40), "a square, as upstream's paste region was");
        assert_eq!(
            marker.triangle,
            [Point::new(504, 0), Point::new(520, 10), Point::new(504, 20)],
            "pole top, apex at half-height/half-width, pole middle"
        );
        assert_eq!(marker.triangle[1].x, marker.canvas.x + i64::from(HEIGHT / 2));
    }

    /// A strip shorter than the pole's own geometry cannot draw a banner; it must still not panic and
    /// must keep the bar visible.
    #[test]
    fn a_very_short_or_very_narrow_strip_still_draws_something_sane() {
        let span = day();
        let noon = at("2026-09-21_12-00-00");
        let short = layout(span, &[noon], WIDTH, 8).markers.remove(0);
        // A strip 8 px tall is exactly the case where upstream's own arithmetic collapses: `h/2` is
        // `mark_width`, so the banner's three points share one column and PIL filled nothing. The
        // pole is still drawn and no pixel is invented for a shape that has no area.
        assert_eq!(short.triangle, [Point::new(504, 0), Point::new(504, 2), Point::new(504, 4)]);
        assert_eq!(short.canvas, Rect::new(500, 0, 8, 8));

        let narrow = layout(span, &[span.to], 2, HEIGHT).markers.remove(0);
        assert_eq!(narrow.x, 0, "a strip narrower than the bar cannot be pushed off the edge");
        assert!(narrow.clipped);
        assert_eq!(layout(span, &[noon], 0, HEIGHT).markers[0].x, 0);
    }

    #[test]
    fn a_blit_can_be_clipped_to_the_strip() {
        let span = day();
        let marker = layout(span, &[span.to], WIDTH, HEIGHT).markers.remove(0);
        let visible = marker.canvas.clipped_to_width(i64::from(WIDTH)).unwrap();
        assert_eq!((visible.x, visible.right(), visible.width), (996, 1000, 4));
        assert_eq!(marker.canvas.clipped_to_width(990), None, "fully past the edge is not drawn");
    }

    #[test]
    fn positions_increase_with_time_and_a_click_finds_its_instant() {
        let span = day();
        let times: Vec<i64> = (0..=8).map(|hour| span.from + hour * 3600).collect();
        let plan = layout(span, &times, WIDTH, HEIGHT);
        assert_eq!(plan.markers.len(), times.len());
        for pair in plan.markers.windows(2) {
            assert!(pair[1].x > pair[0].x, "an hour of the day is an eighth of the strip");
            assert!(pair[1].ratio > pair[0].ratio);
        }
        // Hit-testing: a click on a marker's own column reads back within a second of its time. The
        // clamped marker at the end of the day is the exception the clamp buys, and its error is
        // bounded by BAR_WIDTH columns of the span rather than by a pixel.
        for marker in &plan.markers {
            let back = time_at(span, WIDTH, marker.x);
            let tolerance = if marker.clipped {
                (span.seconds() * i64::from(BAR_WIDTH) / i64::from(WIDTH)) + 1
            } else {
                1
            };
            assert!((back - marker.time).abs() <= tolerance, "{} -> {} -> {back}", marker.time, marker.x);
        }
        assert_eq!(time_at(span, WIDTH, 500), at("2026-09-21_12-00-00"));
        assert_eq!(time_at(DaySpan::new(span.from, span.from), WIDTH, 700), span.from);
    }

    #[test]
    fn the_product_day_span_honours_day_begin_minutes() {
        let date = LocalParts::from_date("2026-09-21").unwrap();
        let shifted = DaySpan::product_day(date, 180);
        assert_eq!(shifted.from, at("2026-09-21_03-00-00"));
        assert_eq!(shifted.to, at("2026-09-22_02-59-59"));
        let midnight = DaySpan::product_day(date, 0);
        assert_eq!(midnight.from, at("2026-09-21_00-00-00"));
        assert_eq!(midnight.to, at("2026-09-21_23-59-59"));
        assert!(midnight.contains(at("2026-09-21_21-16-12")));
        assert!(!DaySpan::product_day(date, 180).contains(at("2026-09-21_02-59-59")));
    }

    #[test]
    fn flags_layout_from_their_own_instants() {
        let span = day();
        let flags = vec![
            Flag::at(LocalParts::from_naive_epoch(at("2026-09-21_09-00-00"))),
            Flag::at(LocalParts::from_naive_epoch(span.to)),
            Flag::at(LocalParts::from_naive_epoch(span.to + 60)),
        ];
        let plan = layout_flags(span, &flags, WIDTH, HEIGHT);
        assert_eq!(plan.markers.iter().map(|m| m.x).collect::<Vec<_>>(), vec![125, 996]);
        assert_eq!(plan.outside.len(), 1);
    }
}
