//! The screen-edge mask: the one geometry that decides what OCR is allowed to read.
//!
//! `ocr_image_crop_URBL` is the user's privacy control. It keeps the taskbar, the clock and the
//! notification corners out of the searchable index by listing, per display slot, four percentages in
//! the key's own order — **U**p, **R**ight, **B**ottom, **L**eft — and shipping `[6, 6, 6, 3]`. The
//! band is *painted black* on the copy the recogniser reads. Nothing is cropped and nothing is
//! discarded: the frame on disk keeps every pixel the user recorded, at its original size. Upstream is
//! built the same way — `record.py`'s `_crop_ocr_image` writes a separate `_cropped.png` beside the
//! screenshot and hands only that to `ocr_image`, while the row's `img_file_name`, the footage list and
//! the thumbnail all keep pointing at the untouched grab.
//!
//! # Why this module is here, and why there is only one of it
//!
//! A privacy boundary that exists twice is a privacy boundary that drifts. Before this module the
//! geometry lived in `wind-reindex` alone and the live recorder applied nothing at all, so a user who
//! excluded a region had that region indexed anyway from the moment the native recorder started
//! running. Both binaries now link this implementation: the reindexer reaches it through
//! `wind_reindex::crop`, the live loop through [`MaskPlan::for_grab`], and the rectangle a frame gets is
//! a function of this module and of nothing else.
//!
//! It lives in this crate rather than in `wind-base` because the inputs are topology, not plumbing: a
//! plan needs the panels [`crate::capture`] enumerated and the rectangle the frame was stretched from,
//! and `Monitor`/`VirtualDesktop` are this crate's types. `wind-base` would have had to redefine them,
//! which is a second source of truth about where a monitor is — the exact thing being unified. The cost
//! of the choice is nil: this module is arithmetic over `i64` and `u8`, so a crate that has deliberately
//! carried no third-party dependency still carries none.
//!
//! # The index order, and the one place upstream gets it wrong
//!
//! The key's name is the order. `windrecorder/ui/setting.py` writes index 0 from the *Top* box, index 1
//! from *Right*, index 2 from *Bottom* and index 3 from *Left*, and `record.py`'s `_crop_ocr_image`
//! reads them back in that same order. `ocr_manager.crop_iframe` — the reindex path — reads them as
//! `(top, bottom, left, right)` instead, so on the shipped default its left and right bands are the
//! wrong way round: a user who raised *Right* to keep a notification tray out would find their left
//! edge masked by that number and the tray read anyway. The values still hide *something*, which is why
//! the mistake survived. This module follows the key, the editor and the live path, because a control
//! whose edges are swapped is the same class of failure as one that is not applied at all.

use crate::capture::{Monitor, VirtualDesktop};

/// The band upstream falls back to whenever the configured list is short, the display validation
/// fails, or the frame does not match the current monitor layout (`crop_iframe`'s
/// `0.06/0.06/0.06/0.03`, and the shipped `config_default.json` value of the key itself).
pub const FALLBACK_URBL: [i64; 4] = [6, 6, 6, 3];

/// JPEG quality of the masked copy.
///
/// Upstream inherits PIL's default of 75. This matches the quality the live recorder encodes its OCR
/// input at, because the only consumer of these bytes is the recogniser and a second lossy pass at 75
/// measurably costs character accuracy.
pub const MASKED_JPEG_QUALITY: u8 = 92;

/// The config key, named once so the code that reads it and the text that describes it cannot disagree
/// about which setting this module implements.
pub const CONFIG_KEY: &str = "ocr_image_crop_URBL";

/// One rectangle: either a panel in desktop coordinates or a region of an image in frame coordinates,
/// depending on which function produced it. The type carries no unit so a [`MaskPlan`] can hold a panel
/// and its scaled image twin under one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl Tile {
    /// A tile covering all of a `width` x `height` image.
    pub fn whole_frame(width: u32, height: u32) -> Tile {
        Tile { x: 0, y: 0, width: i64::from(width), height: i64::from(height) }
    }
}

impl From<VirtualDesktop> for Tile {
    fn from(rect: VirtualDesktop) -> Tile {
        Tile {
            x: i64::from(rect.x),
            y: i64::from(rect.y),
            width: i64::from(rect.width),
            height: i64::from(rect.height),
        }
    }
}

impl From<Monitor> for Tile {
    fn from(monitor: Monitor) -> Tile {
        Tile::from(monitor.rect())
    }
}

/// A rectangle of pixels to paint black, in frame coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Band {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One display slot's four percentages, in the key's own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Urbl {
    pub top: i64,
    pub right: i64,
    pub bottom: i64,
    pub left: i64,
}

impl Urbl {
    /// The shipped default, which is also the answer for a slot the list does not reach.
    pub const FALLBACK: Urbl = Urbl { top: 6, right: 6, bottom: 6, left: 3 };

    /// Slot `index` of a raw config list.
    ///
    /// A slot without four values behind it takes [`Urbl::FALLBACK`] rather than zeros, because zero
    /// would leave the region readable. That is upstream's `except IndexError` branch, and the safe
    /// direction for a privacy control.
    pub fn slot(urbl: &[i64], index: usize) -> Urbl {
        let base = index * 4;
        match (urbl.get(base), urbl.get(base + 1), urbl.get(base + 2), urbl.get(base + 3)) {
            (Some(top), Some(right), Some(bottom), Some(left)) => {
                Urbl { top: *top, right: *right, bottom: *bottom, left: *left }
            }
            _ => Urbl::FALLBACK,
        }
    }

    /// The widest band this config asks for on *any* panel, or `None` when it names none.
    ///
    /// Used where a frame cannot be attributed to a panel, and the choice there is between two wrong
    /// answers: slot 0's band over a frame that came from a 40 % panel hides too little, and guessing a
    /// slot hides something arbitrary. Taking the element-wise maximum over the complete slots is the one
    /// answer that is never *less* protective than what the user asked for, which is the same direction
    /// [`Urbl::slot`] takes for a missing slot.
    ///
    /// An all-zero config returns `Some` all-zero rather than `None`: an explicit "mask nothing" is an
    /// instruction, and only an absent or truncated list is the shipped default's job.
    pub fn widest(urbl: &[i64]) -> Option<Urbl> {
        let mut widest: Option<Urbl> = None;
        let mut index = 0usize;
        while urbl.get(index * 4).is_some() {
            let band = Urbl::slot(urbl, index);
            widest = Some(match widest {
                None => band,
                Some(acc) => Urbl {
                    top: acc.top.max(band.top),
                    right: acc.right.max(band.right),
                    bottom: acc.bottom.max(band.bottom),
                    left: acc.left.max(band.left),
                },
            });
            index += 1;
        }
        widest
    }

    /// The same four values back in the key's list order, for a plan that carries a single tile.
    pub fn array(&self) -> [i64; 4] {
        [self.top, self.right, self.bottom, self.left]
    }

    /// The four edges as `name, percent` pairs in reading order, for reports.
    pub fn edges(&self) -> [(&'static str, i64); 4] {
        [("top", self.top), ("right", self.right), ("bottom", self.bottom), ("left", self.left)]
    }

    /// How many rows and columns this band actually excludes from `tile`.
    ///
    /// `int()` truncates toward zero, so 1080 * 0.06 = 64.8 is 64 rows. Rounding instead would move the
    /// boundary by a row, and one row is one row of the user's excluded content back in the index. A
    /// negative percentage is clamped rather than trusted, since these are fractions of a tile and not
    /// offsets.
    ///
    /// This is the *only* place the truncation happens, so [`MaskPlan::describe`] and the bands
    /// themselves cannot report two different numbers for one setting.
    pub fn pixel_band(&self, tile: &Tile) -> [i64; 4] {
        let tw = tile.width.max(0) as f64;
        let th = tile.height.max(0) as f64;
        let rows = |percent: i64| ((percent.max(0) as f64) * 0.01 * th).clamp(0.0, th) as i64;
        let cols = |percent: i64| ((percent.max(0) as f64) * 0.01 * tw).clamp(0.0, tw) as i64;
        [rows(self.top), cols(self.right), rows(self.bottom), cols(self.left)]
    }

    /// Every percentage is zero or negative, i.e. this tile excludes nothing.
    pub fn is_empty(&self) -> bool {
        self.top <= 0 && self.right <= 0 && self.bottom <= 0 && self.left <= 0
    }
}

/// Which bands to paint, and over what.
///
/// The pair is decided together because the fallback is not "no tiles" but "one whole-frame tile with
/// the default band": applying a multi-panel `URBL` list to a frame whose tiling cannot be confirmed
/// would black out arbitrary content the user never excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskPlan {
    /// The tiles, already in *frame* coordinates.
    pub tiles: Vec<Tile>,
    /// The raw config list the tiles were measured against.
    pub urbl: Vec<i64>,
    /// The image this plan was built for. Carried so [`MaskPlan::bands`] cannot be asked about a frame
    /// other than that one — a plan and a frame disagreeing in size is how a band lands off the edge.
    pub frame_width: u32,
    pub frame_height: u32,
}

impl MaskPlan {
    /// One canvas covering the whole image, with the caller's own settings.
    pub fn whole_frame(width: u32, height: u32, urbl: &[i64]) -> MaskPlan {
        MaskPlan {
            tiles: tiles_for_unknown_layout(width, height),
            urbl: urbl.to_vec(),
            frame_width: width,
            frame_height: height,
        }
    }

    /// The whole-frame default mask.
    pub fn fallback(width: u32, height: u32) -> MaskPlan {
        MaskPlan::whole_frame(width, height, &FALLBACK_URBL)
    }

    /// The bands this plan paints on its own frame.
    pub fn bands(&self) -> Vec<Band> {
        black_bands(self.frame_width, self.frame_height, &self.tiles, &self.urbl)
    }

    /// The band that applies to tile `index`.
    pub fn slot(&self, index: usize) -> Urbl {
        if self.tiles.len() <= 1 {
            // A one-tile plan is one canvas, whatever the machine is plugged into now: the user's
            // first slot is the answer, and the list behind it may be shorter than the number of
            // panels that were attached when it was written.
            return Urbl::slot(&self.urbl, 0);
        }
        Urbl::slot(&self.urbl, index)
    }

    /// The first tile's band — the only one a single-monitor user can set, and the one a report leads
    /// with.
    pub fn applied(&self) -> Urbl {
        self.slot(0)
    }

    /// Whether this plan hides anything at all.
    pub fn is_empty(&self) -> bool {
        self.bands().is_empty()
    }

    /// Human-readable statement of what this plan protects, for `windrec doctor` and the settings page.
    ///
    /// Reports the percentages *and* the pixels they land on: `6%` tells a user nothing about whether
    /// their taskbar is inside the mask, and the whole point of showing this is that they can check.
    ///
    /// A single-tile plan is one canvas, so it is named by its caller rather than repeated here; a
    /// multi-tile plan has to say which band belongs to which panel or the report is unreadable.
    pub fn describe(&self) -> String {
        if self.tiles.is_empty() {
            return "nothing excluded — the frame matches no tile, so no band was computed".to_string();
        }
        let applied = self.applied();
        if self.tiles.len() == 1 && applied.is_empty() {
            return format!("{CONFIG_KEY} is all zero: no screen edge is excluded from OCR").to_string();
        }
        let mut lines: Vec<String> = Vec::new();
        for (index, tile) in self.tiles.iter().enumerate() {
            let band = self.slot(index);
            let pixels = band.pixel_band(tile);
            let named = [
                format!("top {}% = {} rows", band.top, pixels[0]),
                format!("right {}% = {} columns", band.right, pixels[1]),
                format!("bottom {}% = {} rows", band.bottom, pixels[2]),
                format!("left {}% = {} columns", band.left, pixels[3]),
            ]
            .join(", ");
            lines.push(if self.tiles.len() == 1 {
                named
            } else {
                format!(
                    "display {} ({}x{} at ({},{})): {named}",
                    index + 1,
                    tile.width,
                    tile.height,
                    tile.x,
                    tile.y
                )
            });
        }
        lines.join("; ")
    }

    /// Decide the mask for an already-written frame at its own resolution — the reindexer's question.
    ///
    /// `monitors` are the panels attached *now*, in desktop coordinates, and `desktop` is their union,
    /// which is what `mss` reports as `monitors[0]`. A frame off a video has no position on the
    /// desktop, so the only check available is its size, exactly as upstream's `crop_iframe` limits
    /// itself to. The rules are the ones that change the result:
    ///
    ///   * one panel, any resolution — the frame *is* that panel, so slot 0 of the config applies to
    ///     the whole image. Upstream reaches the same conclusion through `display_index`, which for the
    ///     first (and only) panel selects the whole-frame boundary box.
    ///   * several panels and the frame equals the union — a true all-displays recording, so each slot
    ///     masks its own tile, offset into the frame by the union's origin.
    ///   * anything else — the frame came from a different setup than is plugged in now, so no slot can
    ///     be attributed to a tile. The whole image then takes the widest band the config asks for
    ///     anywhere, which cannot hide less than the user configured; only a config that names no slot
    ///     at all falls to [`FALLBACK_URBL`].
    pub fn for_frame(width: u32, height: u32, monitors: &[Tile], desktop: Tile, configured: &[i64]) -> MaskPlan {
        if monitors.len() <= 1 {
            return MaskPlan::whole_frame(width, height, configured);
        }
        if i64::from(width) == desktop.width && i64::from(height) == desktop.height {
            return MaskPlan {
                tiles: tiles_from_monitors(monitors, desktop.x, desktop.y),
                urbl: configured.to_vec(),
                frame_width: width,
                frame_height: height,
            };
        }
        MaskPlan::unattributed(width, height, configured)
    }

    /// The plan for a frame that cannot be matched to a panel: one canvas, and the widest band the
    /// config asks for on any panel.
    ///
    /// This is the only branch that has to *choose* rather than read off the config, and the choice is
    /// deliberately asymmetric in one direction — over-masking costs searchable rows on footage the user
    /// already decided to hide something on, under-masking puts a private row in the database, which is
    /// the thing this module exists to prevent.
    fn unattributed(width: u32, height: u32, configured: &[i64]) -> MaskPlan {
        match Urbl::widest(configured) {
            Some(band) => MaskPlan::whole_frame(width, height, &band.array()),
            None => MaskPlan::fallback(width, height),
        }
    }

    /// Decide the mask for a frame the recorder has *just* grabbed — the live path's question.
    ///
    /// The same geometry as [`MaskPlan::for_frame`], plus the one thing a live grab knows and a
    /// recording does not: which rectangle of the desktop the pixels came from. `source` is that
    /// rectangle in desktop coordinates, and the image may be a resample of it (`windrec` stretches its
    /// source into a 1920-wide DIB section), so the tiles are scaled as well as offset.
    ///
    /// The cases, in the order they are settled:
    ///
    ///   * one panel, or none: the whole image is that panel, so slot 0 applies.
    ///   * the source *is* the desktop union: every panel gets its own slot, scaled into the image.
    ///   * the source is exactly one panel of several (`multi_display_record_strategy = single`): the
    ///     whole image is that panel and it gets *that panel's* slot — upstream's `display_index`
    ///     branch of `crop_iframe` makes the same choice.
    ///   * anything else — the foreground window's rectangle, a source that is no panel at all: the
    ///     configured band over the whole image, proportionally. That is what the shipped help promises
    ///     ("when recording only the foreground window, this option masks each window proportionally")
    ///     and what `record.py`'s `_crop_ocr_image` does.
    ///
    /// No branch here can produce an unmasked frame from a configured one: the worst case is the first
    /// slot's band over everything, which is exactly what a single-monitor user sees in the editor.
    pub fn for_grab(
        width: u32,
        height: u32,
        source: Tile,
        monitors: &[Tile],
        desktop: Tile,
        configured: &[i64],
    ) -> MaskPlan {
        if monitors.len() <= 1 {
            return MaskPlan::whole_frame(width, height, configured);
        }
        if source == desktop {
            return MaskPlan {
                tiles: scale_tiles(
                    &tiles_from_monitors(monitors, desktop.x, desktop.y),
                    width,
                    height,
                    source,
                ),
                urbl: configured.to_vec(),
                frame_width: width,
                frame_height: height,
            };
        }
        if let Some(index) = monitors.iter().position(|m| *m == source) {
            let band = Urbl::slot(configured, index);
            return MaskPlan::whole_frame(width, height, &band.array());
        }
        MaskPlan::whole_frame(width, height, configured)
    }
}

/// Resample a frame-relative tile list from desktop units into the image's pixels.
///
/// The tiles coming in are already offset by the virtual-desktop origin — [`tiles_from_monitors`] did
/// that, and it is the same call the reindexer makes. All that is left is the ratio, so at 1:1 this is
/// the identity function and the two entry points provably agree.
///
/// This is the only place a frame's pixels are related to the desktop's, so the live path and the
/// reindexer cannot diverge about scale.
fn scale_tiles(tiles: &[Tile], width: u32, height: u32, source: Tile) -> Vec<Tile> {
    if source.width <= 0 || source.height <= 0 {
        return tiles.to_vec();
    }
    let (fw, fh) = (f64::from(width), f64::from(height));
    let (sw, sh) = (source.width as f64, source.height as f64);
    tiles
        .iter()
        .map(|tile| {
            // Floor the near edge, ceil the far one: a scaled tile is never *smaller* than the panel it
            // stands for, so a percentage measured against it cannot under-mask a boundary the real
            // pixels would have covered.
            let x0 = ((tile.x as f64) * fw / sw).floor() as i64;
            let y0 = ((tile.y as f64) * fh / sh).floor() as i64;
            let x1 = (((tile.x + tile.width) as f64) * fw / sw).ceil() as i64;
            let y1 = (((tile.y + tile.height) as f64) * fh / sh).ceil() as i64;
            Tile { x: x0, y: y0, width: (x1 - x0).max(1), height: (y1 - y0).max(1) }
        })
        .collect()
}

/// The black bands to paint over a `width` x `height` frame.
///
/// `urbl` is the raw config list: four percent-values per display slot, in slot order. A slot with no
/// entry falls back to [`Urbl::FALLBACK`] rather than to zero, because zero would leave the region
/// readable.
///
/// Bands are clipped to the frame. Upstream lets PIL clip implicitly; doing it here is what keeps a
/// frame that arrived at a different size than the monitors claim — normal for a recording made before
/// a display was changed — from panicking instead.
pub fn black_bands(width: u32, height: u32, tiles: &[Tile], urbl: &[i64]) -> Vec<Band> {
    let mut out = Vec::with_capacity(tiles.len() * 4);
    for (index, tile) in tiles.iter().enumerate() {
        let band = Urbl::slot(urbl, index);
        let [rows_top, cols_right, rows_bottom, cols_left] = band.pixel_band(tile);

        let x0 = tile.x.max(0);
        let y0 = tile.y.max(0);
        let x1 = (tile.x + tile.width.max(0)).min(i64::from(width));
        let y1 = (tile.y + tile.height.max(0)).min(i64::from(height));
        if x1 <= x0 || y1 <= y0 {
            continue; // a tile wholly outside the frame masks nothing
        }

        // Top band, bottom band, then the two side bands down the full tile height — the same order
        // and the same deliberate overlap as upstream's four `draw.rectangle` calls.
        push(&mut out, width, height, x0, y0, x1, y0 + rows_top);
        push(&mut out, width, height, x0, y1 - rows_bottom, x1, y1);
        push(&mut out, width, height, x0, y0, x0 + cols_left, y1);
        push(&mut out, width, height, x1 - cols_right, y0, x1, y1);
    }
    out
}

fn push(out: &mut Vec<Band>, width: u32, height: u32, x0: i64, y0: i64, x1: i64, y1: i64) {
    let (x0, y0) = (x0.clamp(0, i64::from(width)), y0.clamp(0, i64::from(height)));
    let (x1, y1) = (x1.clamp(0, i64::from(width)), y1.clamp(0, i64::from(height)));
    if x1 > x0 && y1 > y0 {
        out.push(Band { x: x0 as u32, y: y0 as u32, width: (x1 - x0) as u32, height: (y1 - y0) as u32 });
    }
}

/// The tile list for a frame that matches no configured display: one panel covering all of it.
///
/// Applying a per-display `URBL` to a canvas of unknown tiling would black out arbitrary content the
/// user never excluded, so the default mask over the whole frame is the only honest answer.
pub fn tiles_for_unknown_layout(width: u32, height: u32) -> Vec<Tile> {
    vec![Tile::whole_frame(width, height)]
}

/// Monitor rectangles, offset into frame coordinates the way `crop_iframe` does it with
/// `monitor["left"] - display_all_full_size["left"]`.
///
/// `mss` reports a panel left of the primary at negative x, so the virtual-desktop origin has to be
/// subtracted before a tile means anything inside the captured image.
pub fn tiles_from_monitors(monitors: &[Tile], virtual_desktop_x: i64, virtual_desktop_y: i64) -> Vec<Tile> {
    monitors
        .iter()
        .map(|m| Tile { x: m.x - virtual_desktop_x, y: m.y - virtual_desktop_y, width: m.width, height: m.height })
        .collect()
}

/// Paint the bands black over an RGB24 buffer, in place.
///
/// Returns how many bands actually painted something, which is what a "frames masked" counter wants:
/// a number a user can check against the frames they kept. Zero means this configuration excluded
/// nothing from this frame, and the caller should say so rather than claim protection it did not give.
///
/// Pixels outside the bands are left exactly alone and the buffer is never resized — this masks, it
/// does not crop.
pub fn paint_rgb(rgb: &mut [u8], width: usize, height: usize, bands: &[Band]) -> usize {
    let mut painted = 0usize;
    for band in bands {
        let y1 = usize::try_from(band.y + band.height).unwrap_or(usize::MAX).min(height);
        let x1 = usize::try_from(band.x + band.width).unwrap_or(usize::MAX).min(width);
        let (y0, x0) = (band.y as usize, band.x as usize);
        if y1 <= y0 || x1 <= x0 {
            continue;
        }
        painted += 1;
        for y in y0..y1 {
            let row = y * width * 3;
            for x in x0..x1 {
                let o = row + x * 3;
                if o + 2 < rgb.len() {
                    rgb[o] = 0;
                    rgb[o + 1] = 0;
                    rgb[o + 2] = 0;
                }
            }
        }
    }
    painted
}

/// A masked *copy* of a frame, leaving the caller's buffer whole.
///
/// The recorder needs both: the original becomes the JPEG on disk and the stored thumbnail, and only
/// this copy goes to the recogniser. The returned band count is what lets the closing report say how
/// many frames were masked without the caller recomputing the geometry.
pub fn masked_copy(rgb: &[u8], width: usize, height: usize, plan: &MaskPlan) -> (Vec<u8>, usize) {
    let mut out = rgb.to_vec();
    let painted = paint_rgb(&mut out, width, height, &plan.bands());
    (out, painted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32) -> Vec<Tile> {
        tiles_for_unknown_layout(w, h)
    }

    /// The pixel band is in the key's own order: `[top rows, right columns, bottom rows, left
    /// columns]`.
    #[test]
    fn the_key_is_read_in_its_own_order() {
        let tile = Tile::whole_frame(1000, 500);
        let band = Urbl::slot(&[6, 6, 6, 3], 0);
        assert_eq!(band.top, 6);
        assert_eq!(band.right, 6);
        assert_eq!(band.bottom, 6);
        assert_eq!(band.left, 3);
        assert_eq!(band.pixel_band(&tile), [30, 60, 30, 30]);

        let bands = black_bands(1000, 500, &frame(1000, 500), &[6, 6, 6, 3]);
        assert_eq!(bands.len(), 4);
        assert_eq!(bands[0], Band { x: 0, y: 0, width: 1000, height: 30 }, "top: 6% of 500");
        assert_eq!(bands[1], Band { x: 0, y: 470, width: 1000, height: 30 }, "bottom: the last 6% of 500");
        assert_eq!(bands[2], Band { x: 0, y: 0, width: 30, height: 500 }, "left: 3% of 1000, full height");
        assert_eq!(bands[3], Band { x: 940, y: 0, width: 60, height: 500 }, "right: 6% of 1000");
    }

    /// The counterpart of the test above, and the reason the order is worth arguing about: index 1 is
    /// the *right* edge, so a user who raises it alone must see the right band grow and nothing else
    /// move. Under the order `ocr_manager.crop_iframe` reads, this same input grows the bottom band
    /// instead and leaves the tray the user was hiding fully readable.
    #[test]
    fn raising_the_right_edge_grows_the_right_band_and_nothing_else() {
        let narrow = black_bands(1000, 500, &frame(1000, 500), &[6, 6, 6, 3]);
        let wide = black_bands(1000, 500, &frame(1000, 500), &[6, 40, 6, 3]);
        assert_eq!(wide[3], Band { x: 600, y: 0, width: 400, height: 500 }, "index 1 is the right edge");
        assert_eq!(wide[0], narrow[0], "top unchanged");
        assert_eq!(wide[1], narrow[1], "bottom unchanged");
        assert_eq!(wide[2], narrow[2], "left unchanged");
    }

    /// `int(monitor["height"] * top)` truncates. At 1080p with 6% the true value is 64.8, so the band
    /// is 64 rows and not 65.
    #[test]
    fn band_sizes_truncate_toward_zero() {
        let bands = black_bands(1920, 1080, &frame(1920, 1080), &[6, 6, 6, 3]);
        assert_eq!(bands[0].height, 64, "1080 * 0.06 = 64.8 -> 64");
        assert_eq!(bands[3].width, 115, "1920 * 0.06 = 115.2 -> 115");
        assert_eq!(bands[2].width, 57, "1920 * 0.03 = 57.6 -> 57");
        assert_eq!(bands[1].y, 1016, "the bottom band starts 64 rows from the edge");
    }

    #[test]
    fn a_zero_slot_paints_nothing() {
        assert!(black_bands(1920, 1080, &frame(1920, 1080), &[0, 0, 0, 0]).is_empty());
        assert!(Urbl::slot(&[0, 0, 0, 0], 0).is_empty());
    }

    #[test]
    fn each_display_slot_masks_only_its_own_tile() {
        let tiles = [
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            Tile { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        // Slot 0 asks for 6/6/6/3 on the left panel; slot 1 asks for nothing on the right panel.
        let bands = black_bands(3840, 1080, &tiles, &[6, 6, 6, 3, 0, 0, 0, 0]);
        assert!(bands.iter().all(|b| b.x + b.width <= 3840));
        assert_eq!(bands.iter().filter(|b| b.x + b.width <= 1920).count(), 4, "four bands on panel 1");
        assert_eq!(bands.iter().filter(|b| b.x >= 1920).count(), 0, "panel 2 asked for nothing");
    }

    #[test]
    fn a_missing_slot_falls_back_to_the_default_band_not_to_nothing() {
        // Two displays configured, one set of percentages: the second must still be masked.
        let tiles = [
            Tile { x: 0, y: 0, width: 100, height: 100 },
            Tile { x: 100, y: 0, width: 100, height: 100 },
        ];
        let bands = black_bands(200, 100, &tiles, &[10, 10, 10, 10]);
        assert_eq!(bands.len(), 8);
        assert!(bands.contains(&Band { x: 100, y: 0, width: 100, height: 6 }), "panel 2 got 6% of 100");
    }

    #[test]
    fn absurd_and_negative_percentages_are_clamped_to_the_frame() {
        for urbl in [&[900i64, 900, 900, 900][..], &[-50i64, -50, -50, -50][..]] {
            let bands = black_bands(100, 100, &frame(100, 100), urbl);
            for b in &bands {
                assert!(b.x + b.width <= 100 && b.y + b.height <= 100, "{b:?} from {urbl:?} escapes the frame");
            }
        }
        // A wholly off-frame tile masks nothing rather than wrapping around.
        let off = Tile { x: 500, y: 500, width: 100, height: 100 };
        assert!(black_bands(100, 100, &[off], &[6, 6, 6, 3]).is_empty());
    }

    #[test]
    fn an_empty_tile_list_paints_nothing() {
        assert!(black_bands(100, 100, &[], &[6, 6, 6, 3]).is_empty());
    }

    #[test]
    fn multi_monitor_offsets_are_relative_to_the_virtual_desktop() {
        // mss reports the leftmost panel at a negative x when it sits left of the primary.
        let monitors = [
            Tile { x: -1920, y: 0, width: 1920, height: 1080 },
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
        ];
        let tiles = tiles_from_monitors(&monitors, -1920, 0);
        assert_eq!(tiles[0].x, 0);
        assert_eq!(tiles[1].x, 1920);
    }

    #[test]
    fn the_fallback_list_is_upstreams_own_constants() {
        assert_eq!(Urbl::FALLBACK.array(), FALLBACK_URBL);
        assert_eq!(FALLBACK_URBL, [6, 6, 6, 3]);
        assert_eq!(tiles_for_unknown_layout(1234, 567), vec![Tile { x: 0, y: 0, width: 1234, height: 567 }]);
    }

    #[test]
    fn one_panel_masks_the_whole_frame_with_its_own_slot() {
        let plan = MaskPlan::for_frame(
            1920,
            1080,
            &[Tile { x: 0, y: 0, width: 1920, height: 1080 }],
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            &[10, 10, 10, 5],
        );
        assert_eq!(plan.tiles, vec![Tile::whole_frame(1920, 1080)]);
        assert_eq!(plan.urbl, vec![10, 10, 10, 5], "the configured band is used, not the default");
    }

    #[test]
    fn a_frame_matching_nothing_takes_the_widest_band_the_config_asks_for() {
        // The machine now has two panels at 1080p each, but this recording is a single 1366x768 panel,
        // so neither slot can be tied to a tile. Slot 0 asks for 10% and slot 1 for nothing: the frame
        // must not come back with 6% painted, because 6% is a number the user never wrote.
        let monitors = [
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            Tile { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = Tile { x: 0, y: 0, width: 3840, height: 1080 };
        let plan = MaskPlan::for_frame(1366, 768, &monitors, desktop, &[10, 10, 10, 10, 0, 0, 0, 0]);
        assert_eq!(plan.urbl, vec![10, 10, 10, 10], "a configured band is never traded for the default");
        assert_eq!(plan.tiles, vec![Tile::whole_frame(1366, 768)]);
        // 10% of 768 rows is 76 and 10% of 1366 columns is 136, on one canvas rather than two.
        let bands = plan.bands();
        assert_eq!(bands.len(), 4);
        assert!(bands.iter().any(|b| b.y == 0 && b.height == 76), "{bands:?}");
        assert!(bands.iter().any(|b| b.x == 1230 && b.width == 136), "{bands:?}");
    }

    #[test]
    fn a_frame_matching_nothing_and_naming_no_slot_takes_the_default_band() {
        let monitors = [
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            Tile { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = Tile { x: 0, y: 0, width: 3840, height: 1080 };
        let plan = MaskPlan::for_frame(1366, 768, &monitors, desktop, &[]);
        assert_eq!(plan.urbl, FALLBACK_URBL.to_vec(), "with nothing configured the shipped default still masks");
        assert_eq!(plan.bands().len(), 4);
    }

    #[test]
    fn the_widest_band_is_taken_over_complete_slots_only() {
        assert_eq!(Urbl::widest(&[]), None, "an empty list configures no slot at all");
        assert_eq!(Urbl::widest(&[0, 0, 0, 0]), Some(Urbl { top: 0, right: 0, bottom: 0, left: 0 }), "an explicit all-zero is an instruction, not an absence");
        assert_eq!(
            Urbl::widest(&[6, 6, 6, 3, 40, 20, 40, 20]),
            Some(Urbl { top: 40, right: 20, bottom: 40, left: 20 }),
            "each edge takes its own maximum, so the band is the union of both panels'"
        );
        assert_eq!(
            Urbl::widest(&[6, 6, 6, 3, 40]),
            Some(Urbl::FALLBACK),
            "a truncated second slot lands on the default rather than being read as zeros"
        );
    }

    #[test]
    fn an_all_displays_frame_masks_each_panel_with_its_own_slot() {
        let monitors = [
            Tile { x: -1920, y: 0, width: 1920, height: 1080 },
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = Tile { x: -1920, y: 0, width: 3840, height: 1080 };
        let plan = MaskPlan::for_frame(3840, 1080, &monitors, desktop, &[6, 6, 6, 3, 50, 50, 50, 50]);
        assert_eq!(plan.tiles[0].x, 0, "the leftmost panel starts at the frame's own origin");
        assert_eq!(plan.tiles[1].x, 1920);
        // Slot 1 asks for half the panel: the boundary must land inside panel 2, not panel 1.
        let bands = plan.bands();
        assert!(bands.iter().any(|b| b.x == 1920 && b.width == 960), "{bands:?}");
        assert!(bands.iter().any(|b| b.y == 0 && b.height == 64), "panel 1's 6% band");
    }

    #[test]
    fn no_monitors_known_is_the_same_as_a_single_panel() {
        let plan = MaskPlan::for_frame(800, 600, &[], Tile { x: 0, y: 0, width: 0, height: 0 }, &[6, 6, 6, 3]);
        assert_eq!(plan.tiles, vec![Tile::whole_frame(800, 600)]);
    }

    // -----------------------------------------------------------------------------------------
    // The live path
    // -----------------------------------------------------------------------------------------

    /// THE agreement test. An all-displays recording at native resolution and an all-displays grab of
    /// the same desktop at 1:1 are one physical frame described through two entry points — `for_frame`
    /// because a frame off a video has no position on the desktop, `for_grab` because a live one does.
    /// If they ever disagree about a rectangle, the boundary exists twice and one of them is lying.
    #[test]
    fn a_native_resolution_grab_and_a_reindex_frame_are_the_same_plan() {
        let monitors = [
            Tile { x: -1920, y: 0, width: 1920, height: 1080 },
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = Tile { x: -1920, y: 0, width: 3840, height: 1080 };
        let configured = [6i64, 6, 6, 3, 12, 4, 20, 2];

        let reindexed = MaskPlan::for_frame(3840, 1080, &monitors, desktop, &configured);
        let live = MaskPlan::for_grab(3840, 1080, desktop, &monitors, desktop, &configured);

        assert_eq!(live.tiles, reindexed.tiles, "the same panels");
        assert_eq!(live.urbl, reindexed.urbl, "the same settings");
        assert_eq!(live.bands(), reindexed.bands(), "the same rectangles");
        assert!(!live.bands().is_empty(), "and not because both paint nothing");
    }

    /// Same agreement on a single-panel machine, which is the common case and the one where the two
    /// entry points have no shared input to compare against.
    #[test]
    fn a_single_panel_grab_and_a_single_panel_reindex_frame_are_the_same_plan() {
        let monitors = [Tile { x: 0, y: 0, width: 1920, height: 1080 }];
        let desktop = monitors[0];
        let configured = [7i64, 2, 9, 4];
        let reindexed = MaskPlan::for_frame(1920, 1080, &monitors, desktop, &configured);
        let live = MaskPlan::for_grab(1920, 1080, desktop, &monitors, desktop, &configured);
        assert_eq!(live, reindexed);
    }

    /// The recorder's frame is a `StretchBlt` of a 5920-wide desktop into a much narrower image, so the
    /// tiles have to be scaled with it — and the mask must still land on the same *content*.
    #[test]
    fn a_resampled_grab_scales_the_tiles_but_keeps_the_proportions() {
        let monitors = [
            Tile { x: 0, y: 0, width: 3840, height: 2160 },
            Tile { x: 3840, y: 0, width: 2080, height: 720 },
        ];
        let desktop = Tile { x: 0, y: 0, width: 5920, height: 2160 };
        // 5920 -> 1480 is a clean quarter, so the expected pixels are exact.
        let plan = MaskPlan::for_grab(1480, 540, desktop, &monitors, desktop, &[10, 10, 10, 10, 50, 50, 50, 50]);
        assert_eq!(plan.tiles.len(), 2);
        assert_eq!(plan.tiles[0], Tile { x: 0, y: 0, width: 960, height: 540 });
        assert_eq!(plan.tiles[1], Tile { x: 960, y: 0, width: 520, height: 180 });
        let bands = plan.bands();
        assert!(bands.contains(&Band { x: 0, y: 0, width: 96, height: 540 }), "panel 1's left 10% of 960");
        assert!(bands.contains(&Band { x: 960, y: 0, width: 260, height: 180 }), "panel 2's left half");
    }

    #[test]
    fn a_single_panel_grab_on_a_multi_panel_desktop_uses_that_panels_slot() {
        let monitors = [
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            Tile { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = Tile { x: 0, y: 0, width: 3840, height: 1080 };
        // `record_single_display_index = 2`, so the second slot's 40% is the one that applies.
        let plan = MaskPlan::for_grab(1920, 1080, monitors[1], &monitors, desktop, &[5, 5, 5, 5, 40, 40, 40, 40]);
        assert_eq!(plan.tiles, vec![Tile::whole_frame(1920, 1080)], "the frame *is* that panel");
        assert_eq!(plan.applied(), Urbl { top: 40, right: 40, bottom: 40, left: 40 });
        assert_eq!(plan.bands()[0].height, 432, "40% of 1080, not 5%");
    }

    /// Foreground-window capture is neither a panel nor the union, and upstream's help text promises a
    /// proportional mask of the window. The fallback must still be a *mask*: a plan that painted
    /// nothing here would be the leak this module exists to close.
    #[test]
    fn a_window_grab_masks_the_whole_image_with_the_first_slot() {
        let monitors = [
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            Tile { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = Tile { x: 0, y: 0, width: 3840, height: 1080 };
        let window = Tile { x: 400, y: 200, width: 900, height: 500 };
        let plan = MaskPlan::for_grab(900, 500, window, &monitors, desktop, &[8, 8, 8, 8]);
        assert_eq!(plan.tiles, vec![Tile::whole_frame(900, 500)]);
        assert_eq!(plan.bands().len(), 4);
        assert_eq!(plan.bands()[0].height, 40, "8% of 500");
    }

    #[test]
    fn a_window_grab_still_masks_when_the_list_is_shorter_than_the_panels() {
        let monitors = [
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
            Tile { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = Tile { x: 0, y: 0, width: 3840, height: 1080 };
        let window = Tile { x: 10, y: 10, width: 900, height: 500 };
        let plan = MaskPlan::for_grab(900, 500, window, &monitors, desktop, &[]);
        assert_eq!(plan.applied(), Urbl::FALLBACK, "an empty config is not an empty mask");
        assert_eq!(plan.bands().len(), 4);
    }

    // -----------------------------------------------------------------------------------------
    // Painting
    // -----------------------------------------------------------------------------------------

    #[test]
    fn painting_blacks_only_the_bands_and_leaves_the_buffer_the_same_size() {
        let (w, h) = (40usize, 20usize);
        let original = vec![180u8; w * h * 3];
        let plan = MaskPlan::whole_frame(w as u32, h as u32, &[10, 10, 10, 10]);
        let (masked, painted) = masked_copy(&original, w, h, &plan);
        assert_eq!(painted, 4, "four bands");
        assert_eq!(masked.len(), original.len(), "masking never resizes");
        assert_eq!(original, vec![180u8; w * h * 3], "the caller's pixels are untouched");
        // 10% of 20 rows is 2 and of 40 columns is 4, so row 0 is black and row 10, column 20 is not.
        assert_eq!(&masked[0..3], &[0, 0, 0]);
        assert_eq!(&masked[(10 * w + 20) * 3..(10 * w + 20) * 3 + 3], &[180, 180, 180]);
        assert_eq!(&masked[(10 * w + 3) * 3..(10 * w + 3) * 3 + 3], &[0, 0, 0], "inside the left band");
    }

    #[test]
    fn a_config_that_excludes_nothing_paints_nothing_and_says_so() {
        let plan = MaskPlan::whole_frame(64, 64, &[0, 0, 0, 0]);
        let (masked, painted) = masked_copy(&vec![180u8; 64 * 64 * 3], 64, 64, &plan);
        assert_eq!(painted, 0);
        assert!(masked.iter().all(|&v| v == 180));
        assert!(plan.describe().contains("all zero"), "{}", plan.describe());
    }

    #[test]
    fn describe_names_the_percentages_and_the_pixels_they_land_on() {
        let plan = MaskPlan::whole_frame(1920, 1080, &[6, 6, 6, 3]);
        let text = plan.describe();
        assert!(text.contains("top 6% = 64 rows"), "{text}");
        assert!(text.contains("left 3% = 57 columns"), "{text}");

        // More than one tile, so the report has to say which band is whose.
        let monitors = [Tile { x: 0, y: 0, width: 1920, height: 1080 }, Tile { x: 1920, y: 0, width: 1280, height: 1024 }];
        let desktop = Tile { x: 0, y: 0, width: 3200, height: 1080 };
        let two = MaskPlan::for_frame(3200, 1080, &monitors, desktop, &[6, 6, 6, 3, 10, 10, 10, 10]);
        let text = two.describe();
        assert!(text.contains("display 1 (1920x1080 at (0,0))"), "{text}");
        assert!(text.contains("display 2 (1280x1024 at (1920,0))"), "{text}");
        assert!(text.contains("top 10% = 102 rows"), "{text}");
    }

    #[test]
    fn a_monitor_becomes_a_tile_in_desktop_coordinates() {
        let monitor = Monitor { index: 1, x: -1920, y: 0, width: 1920, height: 1080, primary: false };
        assert_eq!(Tile::from(monitor), Tile { x: -1920, y: 0, width: 1920, height: 1080 });
        let desktop = VirtualDesktop { x: -1920, y: 0, width: 3840, height: 1080 };
        assert_eq!(Tile::from(desktop), Tile { x: -1920, y: 0, width: 3840, height: 1080 });
    }
}
