//! base64 JPEG out of the index, pixels into the frame loop.
//!
//! `video_text.thumbnail` holds the whole picture as one base64 string, 1-2 KB a row. Decoding it
//! is the only per-row CPU cost the two screens have, and it is the reason a page of 100 results
//! would otherwise stutter: `image`'s JPEG decoder is fast but not free, and doing it on the UI
//! thread couples the frame rate to how many rows a page holds.
//!
//! So this module has no `egui` in it and is called from a worker only. What crosses back is
//! [`crate::model::DecodedImage`] — raw RGBA — which the UI thread turns into a texture.

use crate::model::DecodedImage;

/// Decode a stored thumbnail. `None` on anything unrecognisable, never a panic: a truncated base64
/// blob in one old row must cost the user that row's picture and nothing else.
pub fn decode(stored: &str) -> Option<DecodedImage> {
    // The WebUI prepends a data-URL header at render time and never stores one, but the flag/note
    // CSV and some pre-split month files do, so accept either shape.
    let body = match stored.split_once(',') {
        Some((head, rest)) if head.trim().starts_with("data:") => rest,
        _ => stored,
    };
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() < 24 {
        return None;
    }
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &cleaned).ok()?;
    decode_jpeg(&bytes)
}

/// Decode a picture that is already bytes — the full-resolution frame off the screenshot cache, or the one
/// ffmpeg pulled out of a segment. Same rule as [`decode`]: unrecognisable input is `None`, never a panic,
/// and never a blank card.
pub fn decode_jpeg(bytes: &[u8]) -> Option<DecodedImage> {
    let image = image::load_from_memory(bytes).ok()?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    Some(DecodedImage { width, height, rgba: rgba.into_raw() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real encoder output, made the same way the recorder makes it.
    fn stored_thumbnail() -> String {
        let rgb = vec![200u8; 64 * 36 * 3];
        wind_base::image::thumbnail_base64(&rgb, 64, 36, 64, 60).expect("encode")
    }

    #[test]
    fn a_stored_thumbnail_decodes_to_the_pixels_it_was_made_of() {
        let image = decode(&stored_thumbnail()).expect("decodes");
        assert_eq!((image.width, image.height), (64, 36));
        assert_eq!(image.rgba.len(), 64 * 36 * 4);
        assert_eq!(&image.rgba[0..3], &[200, 200, 200], "the JPEG is lossy but not wildly so");
    }

    #[test]
    fn junk_is_rejected_without_a_panic() {
        assert!(decode("").is_none());
        assert!(decode("not base64 at all!!!!").is_none());
        assert!(decode("!!!!").is_none());
        // Valid base64 that is not a JPEG: 44 characters of `A` decodes to a byte string with no
        // JPEG marker, and the decoder's answer is an error we must not escalate.
        assert!(decode(&"A".repeat(44)).is_none());
    }

    #[test]
    fn a_data_url_prefix_is_tolerated() {
        let raw = stored_thumbnail();
        let with_prefix = format!("data:image/png;base64,{raw}");
        assert!(decode(&with_prefix).is_some());
    }

    #[test]
    fn whitespace_inside_the_blob_is_ignored() {
        let mut raw = stored_thumbnail();
        raw.insert(raw.len() / 2, '\n');
        assert!(decode(&raw).is_some());
    }
}
