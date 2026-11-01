//! Picture layers composited over a pipeline's graded frames in the same GPU pass: split-screen
//! panels, picture-in-picture, stickers and logos, the incoming side of a transition.
//!
//! Each [`Layer`] has its own source (a video decoded alongside the main input, or a still
//! image) and its own [`Grade`]. The backend renders the layer's frame into an RGBA texture of
//! the grade's `size` (its hooks can write alpha: [`crate::shaders::alpha`] for opacity and
//! rounded corners), then draws it over the main picture at the frame's placement, under the
//! text [`Overlay`](crate::Overlay)s.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::{Crop, Grade};

/// An RGBA image, 8 bits per channel, not premultiplied, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Where a layer's pictures come from.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerSource {
    /// A video file, decoded from `start` and conformed to the pipeline's frame rate. Past its
    /// end the last frame holds.
    Video { input: PathBuf, start: Duration },
    /// A still picture, drawn on every frame. Its values are taken as the output's colour
    /// space (no conversion), so a logo keeps its colours.
    Image(Arc<Image>),
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
    }
}
