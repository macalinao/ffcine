//! Crop windows in source pixels, kept as `f64` end to end: nothing is rounded, so a moving
//! window moves by fractions of a pixel and its size never steps out of sync with its position.

use ffcine_core::geom::Mat3;
use ffcine_render::Crop;

/// An axis-aligned rectangle in continuous pixel coordinates (`ffcine_core::geom`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    /// The whole `w x h` frame.
    pub fn frame(w: f64, h: f64) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }
    }

    pub fn aspect(&self) -> f64 {
        self.w / self.h
    }

    /// The largest window of aspect `ar` (width / height) inside this one, placed by `pos`: 0 at
    /// the left/top edge, 0.5 centred, 1 at the right/bottom edge (fractions of the slack, as the
    /// prototype's `crop_x` / `crop_y`).
    #[must_use]
    pub fn fit_aspect(&self, ar: f64, pos: [f64; 2]) -> Self {
        let (w, h) = if self.aspect() > ar {
            (self.h * ar, self.h)
        } else {
            (self.w, self.w / ar)
        };
        self.place(w, h, pos)
    }

    /// A window `1 / zoom` of this one's size, placed by `pos` in the slack: the prototype's
    /// `crop=iw/z:ih/z:(iw-iw/z)*cx:(ih-ih/z)*cy`. Zooms below 1 count as 1, so the window never
    /// leaves the picture.
    #[must_use]
    pub fn zoom(&self, zoom: f64, pos: [f64; 2]) -> Self {
        let z = if zoom.is_finite() { zoom.max(1.0) } else { 1.0 };
        self.place(self.w / z, self.h / z, pos)
    }

    #[must_use]
    fn place(&self, w: f64, h: f64, [px, py]: [f64; 2]) -> Self {
        Self {
            x: self.x + (self.w - w) * px.clamp(0.0, 1.0),
            y: self.y + (self.h - h) * py.clamp(0.0, 1.0),
            w,
            h,
        }
    }

    /// The map from a pixel of a `out_w x out_h` image showing this window to the source pixel
    /// it samples (a [`Mat3`] in the stabilizer's convention: output -> source).
    pub fn to_mat3(&self, out_w: f64, out_h: f64) -> Mat3 {
        let (sx, sy) = (self.w / out_w, self.h / out_h);
        [[sx, 0.0, self.x], [0.0, sy, self.y], [0.0, 0.0, 1.0]]
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn to_crop(&self) -> Crop {
        Crop {
            x: self.x as f32,
            y: self.y as f32,
            width: self.w as f32,
            height: self.h as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_core::geom;

    #[test]
    fn vertical_windows_from_uhd() {
        let src = Rect::frame(3840.0, 2160.0);
        let reel = src.fit_aspect(9.0 / 16.0, [0.5, 0.5]);
        assert!((reel.w - 1215.0).abs() < 1e-9 && (reel.h - 2160.0).abs() < 1e-9);
        assert!((reel.x - (3840.0 - 1215.0) / 2.0).abs() < 1e-9 && reel.y == 0.0);
        let feed = src.fit_aspect(4.0 / 5.0, [0.0, 0.5]);
        assert!((feed.w - 1728.0).abs() < 1e-9 && feed.x == 0.0);
        let right = src.fit_aspect(4.0 / 5.0, [1.0, 0.5]);
        assert!((right.x + right.w - 3840.0).abs() < 1e-9);
        // Taller sources slide on y.
        let portrait = Rect::frame(2160.0, 3840.0).fit_aspect(16.0 / 9.0, [0.5, 0.25]);
        assert!((portrait.h - 1215.0).abs() < 1e-9 && portrait.x == 0.0);
        assert!((portrait.y - (3840.0 - 1215.0) * 0.25).abs() < 1e-9);
    }

    #[test]
    fn zoom_matches_prototype_crop() {
        let src = Rect::frame(3840.0, 2160.0);
        let z = src.zoom(1.25, [0.3, 0.7]);
        assert!((z.w - 3072.0).abs() < 1e-9 && (z.h - 1728.0).abs() < 1e-9);
        assert!((z.x - 768.0 * 0.3).abs() < 1e-9 && (z.y - 432.0 * 0.7).abs() < 1e-9);
        // Below 1 clamps; the aspect is kept exactly.
        assert_eq!(src.zoom(0.8, [0.5, 0.5]), src);
        assert!((z.aspect() - src.aspect()).abs() < 1e-12);
    }

    #[test]
    fn matrix_maps_corners() {
        let r = Rect {
            x: 100.25,
            y: 50.5,
            w: 1215.0,
            h: 2160.0,
        };
        let m = r.to_mat3(1080.0, 1920.0);
        let [x, y] = geom::apply(&m, [0.0, 0.0]);
        assert!((x - 100.25).abs() < 1e-9 && (y - 50.5).abs() < 1e-9);
        let [x, y] = geom::apply(&m, [1080.0, 1920.0]);
        assert!((x - 1315.25).abs() < 1e-9 && (y - 2210.5).abs() < 1e-9);
    }
}
