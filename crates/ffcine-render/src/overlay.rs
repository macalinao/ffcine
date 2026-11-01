//! Overlays composited onto the output in the grade pass (text cards, captions), backend-neutral.
//!
//! An [`Overlay`] is a coverage mask (8-bit alpha) drawn in one or more colours over a range of
//! output frames, with linear fades at its ends. The backend uploads each mask once per run and
//! hands the visible ones to libplacebo as `pl_overlay`s (`PL_OVERLAY_MONOCHROME`) on the render
//! target, so overlays cost no extra pass and frames outside their ranges are untouched.

use std::sync::Arc;

/// An 8-bit coverage mask, row-major, `width * height` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Mask {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; width as usize * height as usize],
        }
    }

    /// A 1×1 opaque mask: stretched over a part's `size`, a solid box.
    pub fn solid() -> Self {
        Self {
            width: 1,
            height: 1,
            data: vec![255],
        }
    }
}

/// One placement of an overlay's mask.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayPart {
    /// Top left in output pixels.
    pub pos: [f32; 2],
    /// Size in output pixels; `None` draws the mask at 1:1.
    pub size: Option<[f32; 2]>,
    /// `[r, g, b, a]`, sRGB, not premultiplied; multiplied by the mask and the fade.
    pub colour: [f32; 4],
}

/// A mask drawn over output frames `from..to` (pipeline frame indices, 0 = the first frame
/// output), fading in over the first `fade_in` frames and out over the last `fade_out`.
#[derive(Debug, Clone, PartialEq)]
pub struct Overlay {
    pub mask: Arc<Mask>,
    pub parts: Vec<OverlayPart>,
    pub from: i64,
    pub to: i64,
    pub fade_in: f64,
    pub fade_out: f64,
}

impl Overlay {
    /// Opacity at output frame `frame`, 0 outside `from..to`. Fades are linear and sampled at
    /// the frame's middle, so a 1-frame fade still shows a half step.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub fn opacity(&self, frame: i64) -> f32 {
        if frame < self.from || frame >= self.to {
            return 0.0;
        }
        let t = (frame - self.from) as f64 + 0.5;
        let left = (self.to - frame) as f64 - 0.5;
        let a = if self.fade_in > 0.0 {
            (t / self.fade_in).min(1.0)
        } else {
            1.0
        };
        let b = if self.fade_out > 0.0 {
            (left / self.fade_out).min(1.0)
        } else {
            1.0
        };
        a.min(b).clamp(0.0, 1.0) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_fades_and_range() {
        let o = Overlay {
            mask: Arc::new(Mask::solid()),
            parts: Vec::new(),
            from: 10,
            to: 20,
            fade_in: 2.0,
            fade_out: 4.0,
        };
        assert!(o.opacity(9).abs() < f32::EPSILON);
        assert!((o.opacity(10) - 0.25).abs() < 1e-6);
        assert!((o.opacity(12) - 1.0).abs() < 1e-6);
        assert!((o.opacity(19) - 0.125).abs() < 1e-6);
        assert!(o.opacity(20).abs() < f32::EPSILON);
    }
}
