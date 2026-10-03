//! JPEG encoding and the card-width base64 preview the index stores.
//!
//! The Python path spends three full-frame image round-trips per kept frame: `to_png` writes the
//! grab, `_crop_ocr_image` decodes it again just to paint black bars and re-encode at the same
//! size, and the thumbnail opens the PNG a third time and decodes all of it to reach 70 px. Here
//! the pixels are already in memory from the single GDI pass, so this module costs one encode.

use base64::{engine::general_purpose::STANDARD, Engine as _};

/// The narrowest preview a result card can be drawn from without stretching it.
///
/// One number for three questions — what the recorder stores, what the re-index pass stores, and what
/// the settings page warns about — because they were answering it separately and disagreeing. It comes
/// from the widest box either window paints a preview in: the HTML window's result card draws its
/// picture across roughly 450 CSS pixels, and the egui window's draws 114. Measured on this install's
/// own frames, a 512 px / quality-70 preview of a 1920×1080 grab is 7-12 KB, and of a tall
/// multi-monitor grab up to 21 KB.
pub const CARD_PREVIEW_FLOOR: u32 = 512;

/// Enclose the given RGB buffer in a baseline JPEG at `quality` (0-100).
pub fn encode_jpeg(rgb: &[u8], width: usize, height: usize, quality: u8) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("zero-sized image".into());
    }
    if rgb.len() < width * height * 3 {
        return Err(format!("short RGB buffer: {} < {}", rgb.len(), width * height * 3));
    }
    let mut out = Vec::with_capacity(width * height / 4);
    {
        let mut writer = std::io::BufWriter::new(&mut out);
        let encoder = jpeg_encoder::Encoder::new(&mut writer, quality);
        encoder
            .encode(
                rgb,
                width as u16,
                height as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .map_err(|e| format!("jpeg: {e}"))?;
    }
    Ok(out)
}

/// Integer box downscale of an RGB image to exactly `target_width` (height keeps aspect).
/// Used for the thumbnail only; the OCR input is encoded at the grab's own resolution, because
/// decimating before OCR costs recognition accuracy.
pub fn box_resize_rgb(src: &[u8], w: usize, h: usize, tw: usize) -> (Vec<u8>, usize, usize) {
    let tw = tw.clamp(1, w);
    let th = (((h as f64) * (tw as f64) / (w as f64)).round() as usize).max(1);
    let mut dst = vec![0u8; tw * th * 3];

    for y in 0..th {
        let y0 = y * h / th;
        let y1 = ((y + 1) * h / th).max(y0 + 1).min(h);
        for x in 0..tw {
            let x0 = x * w / tw;
            let x1 = ((x + 1) * w / tw).max(x0 + 1).min(w);
            let mut acc = [0u64; 3];
            let mut n = 0u64;
            for sy in y0..y1 {
                let row = sy * w;
                for sx in x0..x1 {
                    let o = (row + sx) * 3;
                    for c in 0..3 {
                        acc[c] += u64::from(src[o + c]);
                    }
                    n += 1;
                }
            }
            let o = (y * tw + x) * 3;
            if n > 0 {
                for c in 0..3 {
                    dst[o + c] = (acc[c] / n) as u8;
                }
            }
        }
    }
    (dst, tw, th)
}

/// The value stored in `video_text.thumbnail`: base64 JPEG, matching what `utils.resize_image_as_base64`
/// produces (that one is PNG-in-base64 upstream; readers only ever hand it to an <img> tag, and the
/// bridge filters on "non-empty", so the container is not load-bearing).
pub fn thumbnail_base64(rgb: &[u8], w: usize, h: usize, target_width: u32, quality: u8) -> Result<String, String> {
    let (small, sw, sh) = box_resize_rgb(rgb, w, h, target_width as usize);
    let jpeg = encode_jpeg(&small, sw, sh, quality)?;
    Ok(STANDARD.encode(&jpeg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                v.push(((x * 7 + y * 3) % 256) as u8);
                v.push(((x * 13) % 256) as u8);
                v.push(((y * 11) % 256) as u8);
            }
        }
        v
    }

    #[test]
    fn jpeg_output_carries_the_so_marker() {
        let g = gradient(64, 48);
        let bytes = encode_jpeg(&g, 64, 48, 85).expect("encode");
        assert!(bytes.starts_with(&[0xFF, 0xD8]), "not a JPEG");
        assert_eq!(&bytes[bytes.len() - 2..], &[0xFF, 0xD9], "missing EOI");
    }

    #[test]
    fn lower_quality_is_smaller() {
        let g = gradient(128, 128);
        let hi = encode_jpeg(&g, 128, 128, 95).unwrap();
        let lo = encode_jpeg(&g, 128, 128, 20).unwrap();
        assert!(lo.len() < hi.len(), "{} !< {}", lo.len(), hi.len());
    }

    #[test]
    fn box_resize_preserves_aspect_and_is_averaged_not_sampled() {
        let src = vec![200u8; 100 * 50 * 3];
        let (dst, tw, th) = box_resize_rgb(&src, 100, 50, 40);
        assert_eq!(tw, 40);
        assert_eq!(th, 20);
        assert_eq!(dst.len(), 40 * 20 * 3);
        assert!(dst.iter().all(|p| *p == 200));
    }

    #[test]
    fn thumbnail_is_decodable_base64_jpeg() {
        let g = gradient(640, 360);
        let b64 = thumbnail_base64(&g, 640, 360, 70, 30).expect("thumb");
        let raw = STANDARD.decode(&b64).expect("base64");
        assert!(raw.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn degenerate_inputs_are_rejected_not_panicked() {
        assert!(encode_jpeg(&[], 0, 0, 30).is_err());
        assert!(encode_jpeg(&[0u8; 3], 10, 10, 30).is_err());
    }
}
