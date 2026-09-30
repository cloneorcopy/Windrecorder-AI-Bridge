//! Did the screen actually change, and is this frame one we already have?
//!
//! Replaces two things in the Python path: `compare_image_similarity_np`, which runs cv2 ORB
//! keypoint detection plus brute-force matching over the *full* frame (189 ms here, and the single
//! most wasteful use of feature matching in the codebase — ORB measures geometric registration and
//! its match count is not even monotonic in "how different"), and `compare_image_similarity`,
//! whose own author left `# FIXME 这个函数太慢了，得优化` on it.
//!
//! The signal actually wanted is "did enough pixels change": block-wise mean absolute difference
//! over a 2 MB luma plane. The thresholds below are starting points, not calibrated numbers, and
//! must be re-checked against real recordings before this replaces the Python loop.
//!
//! The gate is expressed over a raw luma slice rather than a capture type, so it can be tested
//! against synthetic frames with no screen involved.
//!
//! # It scores the whole frame, and that is deliberate
//!
//! [`crate::crop`] masks the screen edges the user excluded out of the OCR input. The gate is *not*
//! masked, matching upstream, whose `compare_image_similarity_np` compares two full
//! `screenshot_current` buffers and never consults `ocr_image_crop_URBL`. The two answers a
//! `URBL` list must not be allowed to give are different ones: "is this screen new" and "may this
//! screen be searched". Gate on masked pixels and a change that happens entirely inside an excluded
//! region — a download finishing behind a black bar, a clock ticking — stops being a reason to keep
//! the frame, so the recorder would go idle-looking while the user's own screen moved.

#[derive(Debug, Clone, Copy)]
pub struct GateConfig {
    /// Edge length, in downscaled pixels, of one independently-judged block.
    pub block: u32,
    /// How many blocks must move before the frame counts as changed. Kills caret blink and the
    /// taskbar clock, which are always exactly one or two blocks.
    pub min_changed_blocks: u32,
    /// Mean |delta| within a block for that block to count as changed, on 0..255.
    pub block_mad: f64,
    /// Mean |delta| over the whole frame required alongside the block count.
    pub frame_mad: f64,
    /// A frame this different from the immediately previous one fires on its own, however few
    /// blocks moved: it is a full desktop swap and should never be waited out.
    pub strong_frame_mad: f64,
}

impl Default for GateConfig {
    fn default() -> Self {
        GateConfig {
            block: 16,
            min_changed_blocks: 3,
            block_mad: 6.0,
            frame_mad: 1.5,
            strong_frame_mad: 12.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    pub changed: bool,
    pub changed_blocks: u32,
    /// Mean |delta| against the last committed frame; -1.0 for the first frame, which has none.
    pub mad_vs_anchor: f64,
    /// Mean |delta| against the immediately preceding frame; -1.0 when there is none.
    pub mad_vs_prev: f64,
}

/// Reference-managed block-SAD comparator with a dual reference.
///
/// *Anchor* = the last frame judged worth keeping. *Prev* = the frame immediately before this one,
/// kept only so an instantaneous full-screen swap fires without waiting for the anchor distance to
/// build up. Judging scroll and caret creep against the anchor is what stops a slow scroll from
/// being logged as sixty separate "changes".
pub struct ChangeGate {
    cfg: GateConfig,
    anchor: Option<Vec<u8>>,
    prev: Option<Vec<u8>>,
    width: u32,
    height: u32,
}

impl ChangeGate {
    pub fn new(cfg: GateConfig) -> Self {
        ChangeGate { cfg, anchor: None, prev: None, width: 0, height: 0 }
    }

    pub fn config(&self) -> GateConfig {
        self.cfg
    }

    /// Feed one frame. `changed == true` means "new content, keep it", and the frame becomes the
    /// anchor.
    ///
    /// A size change resets both references: frames of different geometry are not comparable, and
    /// scoring mismatched buffers yields a permanent "always changed" or, worse, "never changed".
    pub fn observe(&mut self, width: u32, height: u32, luma: &[u8]) -> Decision {
        let area = width as usize * height as usize;
        if luma.len() < area {
            // Too short to score against this geometry at all. Drop both references so the next
            // frame re-anchors, rather than reading past the end or trusting a partial plane.
            self.width = 0;
            self.height = 0;
            self.anchor = None;
            self.prev = None;
            return Decision {
                changed: true,
                changed_blocks: 0,
                mad_vs_anchor: -1.0,
                mad_vs_prev: -1.0,
            };
        }
        let resized = self.width != width || self.height != height;
        let fresh = resized || self.anchor.is_none();
        if fresh {
            self.width = width;
            self.height = height;
            self.anchor = None;
            self.prev = None;
        }

        let plane = &luma[..area];
        let (mad_anchor, moved) = match &self.anchor {
            Some(reference) => self.score(reference, plane, width as usize, height as usize),
            None => (f64::INFINITY, usize::MAX),
        };
        let mad_prev = match &self.prev {
            Some(reference) => self.score(reference, plane, width as usize, height as usize).0,
            None => f64::INFINITY,
        };

        let changed = fresh
            || (moved >= self.cfg.min_changed_blocks as usize && mad_anchor >= self.cfg.frame_mad)
            || mad_prev >= self.cfg.strong_frame_mad;

        if changed {
            self.anchor = Some(plane.to_vec());
        }
        self.prev = Some(plane.to_vec());

        Decision {
            changed,
            changed_blocks: if moved == usize::MAX { 0 } else { moved as u32 },
            mad_vs_anchor: if mad_anchor.is_finite() { mad_anchor } else { -1.0 },
            mad_vs_prev: if mad_prev.is_finite() { mad_prev } else { -1.0 },
        }
    }

    /// Mean |delta| over the plane, plus how many blocks exceed `block_mad` on their own.
    fn score(&self, reference: &[u8], current: &[u8], w: usize, h: usize) -> (f64, usize) {
        let block = self.cfg.block.max(1) as usize;
        let mut total = 0u64;
        let mut counted = 0u64;
        let mut moved = 0usize;

        for by in (0..h).step_by(block) {
            for bx in (0..w).step_by(block) {
                let bw = block.min(w - bx);
                let bh = block.min(h - by);
                let mut sum = 0u64;
                for y in by..by + bh {
                    let row = y * w;
                    for x in bx..bx + bw {
                        let i = row + x;
                        sum += u64::from(current[i].abs_diff(reference[i]));
                    }
                }
                let cells = (bw * bh) as u64;
                total += sum;
                counted += cells;
                if (sum as f64 / cells as f64) >= self.cfg.block_mad {
                    moved += 1;
                }
            }
        }

        let mean = if counted > 0 { total as f64 / counted as f64 } else { 0.0 };
        (mean, moved)
    }
}

/// 64-bit difference hash of a frame: a 9x8 area-averaged grid, one bit per horizontal neighbour
/// pair. A *dedup key* for thumbnails only — far too coarse to gate on, since one glyph change
/// flips 0-2 of the 64 bits.
pub fn dhash(luma: &[u8], width: usize, height: usize) -> u64 {
    let grid = area_average(luma, width, height, 9, 8);
    let mut bits = 0u64;
    for row in 0..8usize {
        for col in 0..8usize {
            if grid[row * 9 + col] > grid[row * 9 + col + 1] {
                bits |= 1u64 << (row * 8 + col);
            }
        }
    }
    bits
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

fn area_average(pixels: &[u8], w: usize, h: usize, gw: usize, gh: usize) -> Vec<u8> {
    let mut out = vec![0u8; gw * gh];
    if w == 0 || h == 0 {
        return out;
    }
    for gy in 0..gh {
        let y0 = gy * h / gh;
        let y1 = ((gy + 1) * h / gh).max(y0 + 1).min(h);
        for gx in 0..gw {
            let x0 = gx * w / gw;
            let x1 = ((gx + 1) * w / gw).max(x0 + 1).min(w);
            let (mut sum, mut n) = (0u64, 0u64);
            for row in y0..y1 {
                for &value in &pixels[row * w + x0..row * w + x1] {
                    sum += u64::from(value);
                    n += 1;
                }
            }
            out[gy * gw + gx] = if n > 0 { (sum / n) as u8 } else { 0 };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: usize, h: usize, v: u8) -> Vec<u8> {
        vec![v; w * h]
    }

    fn patch(img: &mut [u8], w: usize, x: usize, y: usize, size: usize, v: u8) {
        for j in y..y + size {
            for i in x..x + size {
                img[j * w + i] = v;
            }
        }
    }

    const W: u32 = 256;
    const H: u32 = 256;

    #[test]
    fn first_frame_always_counts_as_changed() {
        let mut gate = ChangeGate::new(GateConfig::default());
        assert!(gate.observe(W, H, &flat(256, 256, 10)).changed);
    }

    #[test]
    fn identical_frames_do_not_change() {
        let mut gate = ChangeGate::new(GateConfig::default());
        gate.observe(W, H, &flat(256, 256, 10));
        for _ in 0..5 {
            assert!(!gate.observe(W, H, &flat(256, 256, 10)).changed);
        }
    }

    #[test]
    fn a_caret_blink_sized_edit_is_not_a_change() {
        let mut gate = ChangeGate::new(GateConfig::default());
        gate.observe(W, H, &flat(256, 256, 10));
        let mut next = flat(256, 256, 10);
        patch(&mut next, 256, 100, 100, 6, 240); // 6x6: one block, below min_changed_blocks
        assert!(!gate.observe(W, H, &next).changed);
    }

    #[test]
    fn a_full_screen_swap_is_a_change() {
        let mut gate = ChangeGate::new(GateConfig::default());
        gate.observe(W, H, &flat(256, 256, 10));
        assert!(gate.observe(W, H, &flat(256, 256, 200)).changed);
    }

    #[test]
    fn slow_drift_accumulates_against_the_anchor_not_the_previous_frame() {
        let mut gate = ChangeGate::new(GateConfig::default());
        gate.observe(W, H, &flat(256, 256, 10));
        // Ten steps of 2/255 each: every step is ~2 against its predecessor, well under block_mad,
        // but cumulatively ~20 against the anchor, which is a real change.
        let fired: Vec<bool> = (1..=10u8)
            .map(|step| gate.observe(W, H, &flat(256, 256, 10 + step * 2)).changed)
            .collect();
        assert!(fired.iter().any(|f| *f), "drift should eventually trip the anchor");
        assert!(!fired[0], "the first small step must not fire");
    }

    #[test]
    fn resize_resets_references_instead_of_scoring_mismatched_buffers() {
        let mut gate = ChangeGate::new(GateConfig::default());
        gate.observe(W, H, &flat(256, 256, 10));
        assert!(gate.observe(128, 128, &flat(128, 128, 10)).changed, "new geometry must re-anchor");
    }

    #[test]
    fn a_short_buffer_reanchors_instead_of_reading_past_the_end() {
        let mut gate = ChangeGate::new(GateConfig::default());
        gate.observe(W, H, &flat(256, 256, 10));
        let truncated = vec![10u8; 100];
        let decision = gate.observe(W, H, &truncated);
        assert!(decision.changed);
    }

    #[test]
    fn dhash_is_stable_and_sensitive_in_the_right_direction() {
        let a = dhash(&flat(256, 256, 10), 256, 256);
        assert_eq!(a, dhash(&flat(256, 256, 10), 256, 256));

        let mut c = flat(256, 256, 10);
        patch(&mut c, 256, 0, 0, 128, 250);
        assert_ne!(a, dhash(&c, 256, 256));
    }

    #[test]
    fn hamming_counts_bits() {
        assert_eq!(hamming(0b1010, 0b0011), 2);
        assert_eq!(hamming(0, u64::MAX), 64);
    }
}
