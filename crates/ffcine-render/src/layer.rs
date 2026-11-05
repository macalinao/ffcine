//! Picture layers composited over a pipeline's graded frames in the same GPU pass: split-screen
//! panels, picture-in-picture, stickers and logos, the incoming side of a transition.
//!
//! Each [`Layer`] has its own source (a video decoded alongside the main input, or a still
//! image) and its own [`Grade`]. The backend renders the layer's frame into an RGBA texture of
//! the grade's `size` (its hooks can write alpha: [`crate::shaders::alpha`] for opacity and
//! rounded corners), then draws it over the main picture at the frame's placement, under the
//! text [`Overlay`](crate::Overlay)s. A layer that is turned or not [`BlendMode::Normal`] is
//! composited by a shader pass that reads the picture under it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::{BlendMode, Crop, Grade};

/// An RGBA image, 8 bits per channel, not premultiplied, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// The largest still a [`LayerSource::Still`] decodes, px a side.
pub const STILL_SIDE: u32 = 16384;
/// The largest still a [`LayerSource::Still`] decodes, px in all (256 MP).
pub const STILL_PIXELS: u64 = 1 << 28;

/// Whether a `w` x `h` still is within [`STILL_SIDE`] and [`STILL_PIXELS`].
pub fn still_fits(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w.max(h) <= STILL_SIDE && u64::from(w) * u64::from(h) <= STILL_PIXELS
}

/// Where a layer's pictures come from.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerSource {
    /// A video file, decoded from `start` and conformed to the pipeline's frame rate. Past its
    /// end the last frame holds.
    Video { input: PathBuf, start: Duration },
    /// A still picture, drawn on every frame. Its values are taken as the output's colour
    /// space (no conversion) for an SDR output, so a logo keeps its colours, and as sRGB with
    /// white at the HDR reference white (203 nits, BT.2408) for a PQ or HLG one.
    Image(Arc<Image>),
    /// A still file (PNG, JPEG, WebP, HEIC with tile grids and `irot`), decoded through libav
    /// once per layer of a rendered piece. A tagged file (primaries or transfer set: HEIC
    /// `colr`, PNG `cICP` or `sRGB`) is converted from its tags; an untagged one is taken as an
    /// [`Image`](Self::Image) is. JPEG EXIF orientation is not applied; at most
    /// [`STILL_SIDE`] px a side and [`STILL_PIXELS`] in all.
    Still(PathBuf),
    /// A nested composition rendered in the same pass into its own texture.
    Scene(Arc<Scene>),
}

/// A composition drawn into a layer's source texture: its `grade` (the background hook, `size`
/// the scene's size) over a black frame, then `layers` bottom first and `overlays`, frame
/// ranges relative to the using layer's `from`.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub size: crate::Size,
    pub grade: Grade,
    pub layers: Vec<Layer>,
    pub overlays: Vec<crate::Overlay>,
}

/// One layer over output frames `from..to`.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub source: LayerSource,
    /// The layer's grade; `size` is its texture size (default: the source's). Hook tracks and
    /// `crops` are indexed by layer frame (`0` = output frame `from`).
    pub grade: Grade,
    pub from: i64,
    pub to: i64,
    /// Placement in output pixels per layer frame (the last one holds); empty fills the frame.
    pub rects: Vec<Crop>,
    /// Rotation in degrees per layer frame, counter-clockwise on screen about the rectangle's
    /// middle (the last one holds); empty is unrotated. Corners outside the picture stay
    /// transparent.
    pub rotation: Vec<f32>,
    pub blend: BlendMode,
}

impl Layer {
    /// The placement at output frame `frame`, `None` outside `from..to`.
    pub fn rect_at(&self, frame: i64, width: u32, height: u32) -> Option<Crop> {
        if frame < self.from || frame >= self.to {
            return None;
        }
        let i = usize::try_from(frame - self.from).unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        Some(
            self.rects
                .get(i)
                .or(self.rects.last())
                .copied()
                .unwrap_or(Crop {
                    x: 0.0,
                    y: 0.0,
                    width: width as f32,
                    height: height as f32,
                }),
        )
    }

    /// The rotation at output frame `frame` (0 outside `from..to`).
    pub fn rotation_at(&self, frame: i64) -> f32 {
        if frame < self.from || frame >= self.to {
            return 0.0;
        }
        let i = usize::try_from(frame - self.from).unwrap_or(0);
        self.rotation
            .get(i)
            .or(self.rotation.last())
            .copied()
            .unwrap_or(0.0)
    }

    /// Whether frame `frame` needs the composite pass (turned, or blended other than normal).
    pub fn needs_composite(&self, frame: i64) -> bool {
        self.blend != BlendMode::Normal || self.rotation_at(frame) % 360.0 != 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_holds_and_ranges() {
        let r = |x| Crop {
            x,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        let l = Layer {
            source: LayerSource::Image(Arc::new(Image {
                width: 1,
                height: 1,
                data: vec![0; 4],
            })),
            grade: Grade::default(),
            from: 5,
            to: 10,
            rects: vec![r(0.0), r(1.0)],
            rotation: vec![0.0, 30.0],
            blend: BlendMode::Normal,
        };
        assert_eq!(l.rect_at(4, 100, 50), None);
        assert_eq!(l.rect_at(5, 100, 50), Some(r(0.0)));
        assert_eq!(l.rect_at(9, 100, 50), Some(r(1.0)));
        assert_eq!(l.rect_at(10, 100, 50), None);
        let full = Layer {
            rects: Vec::new(),
            ..l
        };
        assert_eq!(full.rect_at(5, 100, 50).map(|c| c.width), Some(100.0));
        assert!(!full.needs_composite(5));
        assert!(full.needs_composite(7));
        assert!((full.rotation_at(9) - 30.0).abs() < f32::EPSILON);
        assert!(full.rotation_at(10).abs() < f32::EPSILON);
        let multiply = Layer {
            rotation: Vec::new(),
            blend: BlendMode::Multiply,
            ..full
        };
        assert!(multiply.needs_composite(5));
    }
}
