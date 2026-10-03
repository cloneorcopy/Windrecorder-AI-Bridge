//! Masking the regions the user excluded before the OCR engine sees them.
//!
//! The geometry is **not in this file**. It lives in `windcap::crop`, the one implementation both
//! binaries link — see that module's header for what `ocr_image_crop_URBL` means, for the order its
//! four percentages are stored in, and for why a privacy boundary written twice is a privacy boundary
//! that drifts. What is left here is the part only the reindexer needs: decoding an extracted i-frame
//! from disk, and encoding the masked copy and the thumbnail back out.
//!
//! Upstream does the whole thing with PIL *after* writing the frame out — decode, paint, re-encode.
//! That round trip is not a requirement, only the black pixels are, so the masked copy is written once
//! and the thumbnail is made from the untouched decode beside it.
//!
//! # The one frame, and the one difference between the two paths
//!
//! A frame off a video knows its size and nothing else, so [`MaskPlan::for_frame`] can only compare
//! that against the union of the panels plugged in now. A frame the recorder has just grabbed also
//! knows which rectangle of the desktop it came from, so [`MaskPlan::for_grab`] can do better and match
//! the panels exactly. At 1:1 the two plans are the same plan, and
//! `the_geometry_is_the_shared_one_not_a_copy_of_it` at the bottom of this file pins that from this
//! side; `windcap::crop`'s own agreement test pins it from the other.

use std::path::Path;

pub use windcap::crop::{
    black_bands, paint_rgb, tiles_for_unknown_layout, tiles_from_monitors, Band, MaskPlan, Tile, Urbl,
    CONFIG_KEY, FALLBACK_URBL, MASKED_JPEG_QUALITY,
};

#[derive(Debug)]
pub enum CropError {
    /// The extracted frame could not be read — a truncated JPEG from an interrupted ffmpeg run.
    Read(String),
    Write(String),
}

impl std::fmt::Display for CropError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CropError::Read(m) => write!(f, "cannot read frame: {m}"),
            CropError::Write(m) => write!(f, "cannot write masked frame: {m}"),
        }
    }
}

impl std::error::Error for CropError {}

/// A decoded frame held as RGB, so a caller that needs both the masked copy and a thumbnail of the
/// original pays for exactly one decode.
#[derive(Debug, Clone)]
pub struct Pixels {
    pub rgb: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Pixels {
    /// Decode a frame from disk.
    pub fn read(path: &Path) -> Result<Pixels, CropError> {
        let image = image::ImageReader::open(path)
            .map_err(|e| CropError::Read(format!("{}: {e}", path.display())))?
            .with_guessed_format()
            .map_err(|e| CropError::Read(format!("{}: {e}", path.display())))?
            .decode()
            .map_err(|e| CropError::Read(format!("{}: {e}", path.display())))?;
        let rgb = image.to_rgb8();
        Ok(Pixels { rgb: rgb.into_raw(), width: image.width(), height: image.height() })
    }

    /// A copy with the excluded bands painted black. The receiver is untouched, which is what lets the
    /// thumbnail below be made from the pixels the user recorded.
    pub fn masked(&self, plan: &MaskPlan) -> Pixels {
        let mut out = self.clone();
        out.paint(plan);
        out
    }

    /// Paint the plan's bands black in place, and report how many of them painted something.
    ///
    /// The bands come from `plan`, which carries the frame size it was built for; painting a plan
    /// onto a differently sized buffer is the mistake that would put a mask across the middle of a
    /// screen, and [`MaskPlan::bands`] makes that impossible to do by accident.
    pub fn paint(&mut self, plan: &MaskPlan) -> usize {
        paint_rgb(&mut self.rgb, self.width as usize, self.height as usize, &plan.bands())
    }

    /// Encode as JPEG, the container the OCR engine and the index both expect.
    pub fn to_jpeg(&self, quality: u8) -> Result<Vec<u8>, CropError> {
        wind_base::image::encode_jpeg(&self.rgb, self.width as usize, self.height as usize, quality)
            .map_err(CropError::Write)
    }

    /// The `video_text.thumbnail` value: base64 JPEG at the configured width, made from the *unmasked*
    /// frame — upstream thumbnails `img_orgin_not_crop_filepath`, so the user's preview shows the
    /// taskbar the index could not read.
    pub fn thumbnail_base64(&self, target_width: u32, quality: u8) -> Result<String, CropError> {
        wind_base::image::thumbnail_base64(&self.rgb, self.width as usize, self.height as usize, target_width, quality)
            .map_err(CropError::Write)
    }
}

/// Paint the excluded bands black and write the result to `dst`.
pub fn crop_for_ocr(src: &Path, dst: &Path, urbl: &[i64]) -> Result<(), CropError> {
    crop_masked(src, dst, urbl, None)
}

/// [`crop_for_ocr`] with the display tiling supplied, for a frame captured across several monitors.
pub fn crop_masked(src: &Path, dst: &Path, urbl: &[i64], tiles: Option<&[Tile]>) -> Result<(), CropError> {
    let frame = Pixels::read(src)?;
    let tiles = tiles.map_or_else(|| tiles_for_unknown_layout(frame.width, frame.height), |t| t.to_vec());
    let plan = MaskPlan { tiles, urbl: urbl.to_vec(), frame_width: frame.width, frame_height: frame.height };
    let masked = frame.masked(&plan);
    std::fs::write(dst, masked.to_jpeg(MASKED_JPEG_QUALITY)?).map_err(|e| CropError::Write(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This crate must not carry a second geometry. Everything `crate::crop` names above is the shared
    /// item itself, so this asserts identity rather than similarity: the same function, reached through
    /// the path the reindexer uses, on the inputs the live recorder passes.
    #[test]
    fn the_geometry_is_the_shared_one_not_a_copy_of_it() {
        let monitors = [
            Tile { x: -1920, y: 0, width: 1920, height: 1080 },
            Tile { x: 0, y: 0, width: 1920, height: 1080 },
        ];
        let desktop = Tile { x: -1920, y: 0, width: 3840, height: 1080 };
        let urbl = [6i64, 6, 6, 3, 40, 1, 20, 2];
        let tiles = tiles_from_monitors(&monitors, desktop.x, desktop.y);
        assert_eq!(black_bands(3840, 1080, &tiles, &urbl), windcap::crop::black_bands(3840, 1080, &tiles, &urbl));

        let reindexed = MaskPlan::for_frame(3840, 1080, &monitors, desktop, &urbl);
        let live = MaskPlan::for_grab(3840, 1080, desktop, &monitors, desktop, &urbl);
        assert_eq!(reindexed.bands(), live.bands(), "same frame, same config, same rectangle");
        assert_eq!(reindexed.describe(), live.describe(), "and the same thing said about it");
        assert!(!reindexed.bands().is_empty());
    }

    /// End-to-end over real pixels, with no ffmpeg and no OCR engine: a synthetic frame with a bright
    /// marker inside every band and in the readable centre, masked, then decoded back.
    ///
    /// The percentages are deliberately asymmetric — `[10, 2, 10, 5]` is top 10%, right 2%, bottom 10%,
    /// left 5% — because a symmetric list cannot tell the four slots apart, and the bug this guards is
    /// the four slots being read in the wrong order.
    #[test]
    fn masking_a_written_frame_blacks_exactly_the_configured_bands() {
        let dir = temp_dir("mask");
        let src = dir.join("0.jpg");
        let dst = dir.join("0_cropped.jpg");

        let (w, h) = (400u32, 200u32);
        // Mid-grey everywhere: far enough from black that a painted band is unambiguous, and far enough
        // from white that a JPEG ringing artefact cannot fake a surviving region.
        let mut buf = vec![180u8; (w * h * 3) as usize];
        // Bands: top and bottom 20 rows (10% of 200), left 20 columns (5% of 400), right 8 columns
        // (2% of 400, i.e. x 392..=399).
        let inside_bands = [(200u32, 10u32), (200, 190), (10, 100), (396, 100)];
        for (x, y) in inside_bands {
            let o = ((y * w + x) * 3) as usize;
            buf[o] = 250;
            buf[o + 1] = 250;
            buf[o + 2] = 250;
        }
        std::fs::write(&src, wind_base::image::encode_jpeg(&buf, w as usize, h as usize, 95).unwrap()).unwrap();

        crop_for_ocr(&src, &dst, &[10, 2, 10, 5]).expect("crop");
        let out = image::open(&dst).unwrap().to_rgb8();
        for (x, y) in inside_bands {
            assert!(out.get_pixel(x, y)[0] < 10, "({x},{y}) should be black");
        }
        // Just outside each band the frame must be untouched: these are the assertions that catch a band
        // one row or column too tall, which is the difference between truncating and rounding, and the
        // ones that catch a band on the wrong edge.
        assert!(out.get_pixel(200, 21)[0] > 150, "the top band stops at row 20");
        assert!(out.get_pixel(200, 179)[0] > 150, "the bottom band starts at row 180");
        assert!(out.get_pixel(20, 100)[0] > 150, "the left band stops at column 20");
        assert!(out.get_pixel(391, 100)[0] > 150, "the right band starts at column 392");
        let centre = out.get_pixel(200, 100)[0];
        assert!(centre > 150, "the readable centre must survive the mask, got {centre}");
        assert_eq!((out.width(), out.height()), (w, h), "the masked copy keeps the frame's size");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of "masking, not cropping": the *original* on disk is not rewritten, and the
    /// thumbnail the index stores is made from it. Cropping here would throw away footage the user
    /// asked to keep, and masking the thumbnail would hide from them what is actually in their video.
    #[test]
    fn the_original_frame_and_its_thumbnail_keep_every_pixel() {
        let dir = temp_dir("original");
        let src = dir.join("0.jpg");
        let (w, h) = (400u32, 200u32);
        // White everywhere, so a painted band is unmistakable and an untouched original is trivially
        // checkable after the mask has been written beside it.
        let buf = vec![240u8; (w * h * 3) as usize];
        std::fs::write(&src, wind_base::image::encode_jpeg(&buf, w as usize, h as usize, 95).unwrap()).unwrap();

        let before = std::fs::read(&src).expect("read original");
        crop_for_ocr(&src, &src.with_file_name("0_cropped.jpg"), &[6, 6, 6, 3]).expect("crop");

        assert_eq!(std::fs::read(&src).expect("re-read"), before, "crop_for_ocr must not touch its input");
        let frame = Pixels::read(&src).unwrap();
        assert_eq!((frame.width, frame.height), (w, h), "the stored frame is still full size");
        assert!(frame.rgb.iter().all(|&p| p > 200), "and no band was painted into it");

        let thumbnail = frame.thumbnail_base64(70, 30).expect("thumbnail");
        let bytes = base64_decode(&thumbnail);
        assert!(bytes.starts_with(&[0xFF, 0xD8]), "the stored thumbnail is a JPEG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_frame_is_an_error_not_a_panic() {
        let dir = temp_dir("bad");
        let src = dir.join("0.jpg");
        std::fs::write(&src, b"not a jpeg at all").unwrap();
        assert!(matches!(crop_for_ocr(&src, &dir.join("o.jpg"), &[6, 6, 6, 3]), Err(CropError::Read(_))));
        assert!(matches!(crop_for_ocr(&dir.join("nope.jpg"), &dir.join("o.jpg"), &[6, 6, 6, 3]), Err(CropError::Read(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `image` crate is a dev-only convenience here; `base64` is not in this crate's dependency
    /// list, and a thumbnail is only checked for being a JPEG, so this is the three lines that do it.
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn base64_decode(text: &str) -> Vec<u8> {
        let mut bits = 0u32;
        let mut count = 0u32;
        let mut out = Vec::new();
        for byte in text.bytes().filter(|b| *b != b'=' && !b.is_ascii_whitespace()) {
            let value = TABLE.iter().position(|&t| t == byte).expect("valid base64") as u32;
            bits = (bits << 6) | value;
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
            }
        }
        out
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-reindex-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
