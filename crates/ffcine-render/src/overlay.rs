//! Overlays composited onto the output in the grade pass (text cards, captions), backend-neutral.
//!
//! An [`Overlay`] is a coverage mask (8-bit alpha) drawn in one or more colours over a range of
//! output frames, with linear fades at its ends. The backend uploads each mask once per run and
//! hands the visible ones to libplacebo as `pl_overlay`s (`PL_OVERLAY_MONOCHROME`) on the render
//! target, so overlays cost no extra pass and frames outside their ranges are untouched. Animated
//! overlays (sliding, popping, typed text) carry a [`Motion`]: their placements per frame.

use std::sync::Arc;

/// An 8-bit coverage mask, row-major, `width * height` bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OverlayPart {
    /// Top left in output pixels.
    pub pos: [f32; 2],
    /// Size in output pixels; `None` draws the (cropped) mask at 1:1.
    pub size: Option<[f32; 2]>,
    /// The part of the mask drawn, `[x0, y0, x1, y1]` in mask pixels; `None` is all of it.
    pub src: Option<[f32; 4]>,
    /// `[r, g, b, a]`, sRGB, not premultiplied; multiplied by the mask and the fade.
    pub colour: [f32; 4],
}

/// A mask drawn over output frames `from..to` (pipeline frame indices, 0 = the first frame
/// output), fading in over the first `fade_in` frames and out over the last `fade_out`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overlay {
    pub mask: Arc<Mask>,
    pub parts: Vec<OverlayPart>,
    pub from: i64,
    pub to: i64,
    pub fade_in: f64,
    pub fade_out: f64,
    /// Per-frame placements, replacing `parts` (the fades still apply).
    pub motion: Option<Arc<Motion>>,
}

/// An animated overlay's placements: output frame `first + i` draws `frames[i]`. Frames before
/// `first` draw the first entry, frames after the last the last one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Motion {
    pub first: i64,
    pub frames: Vec<Vec<OverlayPart>>,
}

impl OverlayPart {
    /// The mask crop and the output rectangle it lands on, both `[x0, y0, x1, y1]`.
    pub fn rects(&self, mask: &Mask) -> ([f32; 4], [f32; 4]) {
        #[allow(clippy::cast_precision_loss)]
        let src = self
            .src
            .unwrap_or([0.0, 0.0, mask.width as f32, mask.height as f32]);
        let [w, h] = self.size.unwrap_or([src[2] - src[0], src[3] - src[1]]);
        let [x, y] = self.pos;
        (src, [x, y, x + w, y + h])
    }
}

impl Overlay {
    /// The placements drawn on output frame `frame`.
    pub fn parts_at(&self, frame: i64) -> &[OverlayPart] {
        match &self.motion {
            Some(m) if !m.frames.is_empty() => {
                let last = m.frames.len() - 1;
                let i = usize::try_from(frame - m.first).map_or(0, |i| i.min(last));
                &m.frames[i]
            }
            _ => &self.parts,
        }
    }

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
            motion: None,
        };
        assert!(o.opacity(9).abs() < f32::EPSILON);
        assert!((o.opacity(10) - 0.25).abs() < 1e-6);
        assert!((o.opacity(12) - 1.0).abs() < 1e-6);
        assert!((o.opacity(19) - 0.125).abs() < 1e-6);
        assert!(o.opacity(20).abs() < f32::EPSILON);
    }

    #[test]
    fn motion_parts_and_crops() {
        let part = |x: f32| OverlayPart {
            pos: [x, 0.0],
            ..OverlayPart::default()
        };
        let o = Overlay {
            mask: Arc::new(Mask::new(8, 4)),
            parts: vec![part(-1.0)],
            from: 0,
            to: 10,
            motion: Some(Arc::new(Motion {
                first: 2,
                frames: vec![vec![part(2.0)], vec![part(3.0)]],
            })),
            ..Overlay::default()
        };
        assert_eq!(o.parts_at(0)[0].pos[0], 2.0);
        assert_eq!(o.parts_at(3)[0].pos[0], 3.0);
        assert_eq!(o.parts_at(9)[0].pos[0], 3.0);
        let p = OverlayPart {
            pos: [10.0, 20.0],
            src: Some([2.0, 0.0, 6.0, 4.0]),
            ..OverlayPart::default()
        };
        assert_eq!(
            p.rects(&o.mask),
            ([2.0, 0.0, 6.0, 4.0], [10.0, 20.0, 14.0, 24.0])
        );
    }
}
