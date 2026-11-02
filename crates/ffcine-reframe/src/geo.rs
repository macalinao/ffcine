//! The NLE importers' `ffcine.geo.crop/1` and `ffcine.geo.transform/1` (`docs/timeline.md`) on a
//! clip's picture, evaluated per frame into one map from output pixels to source pixels and a
//! source-side crop mask. [`crate::pass::apply_mapped`] samples it in the grade pass, composed
//! with the reframe window and the stabilizer's warp, so the picture is resampled once more at
//! most (as on the warp path).
//!
//! The **picture** is the reframed picture: the source as the window shows it in the output,
//! extended past the window at the same scale (a zoomed-out or moved picture shows what the
//! window cut off, black past the source). `"units": "picture"` sizes are that picture's (for a
//! centred window, Resolve's picture: the source sized into the frame); `"units": "frame"` sizes
//! are the output frame's.

// Frame maths reads best with the usual one-letter names.
#![allow(clippy::many_single_char_names)]

use ffcine_core::geom::{self, Mat3};
use ffcine_core::{Error, Result};
use serde_json::{Map, Value};

use crate::keyframe::Keyed;
use crate::profile::Profile;
use crate::reframe::{Frame, Reframe, Shot};
use crate::window::Rect;

pub const CROP_SCHEMA: &str = "ffcine.geo.crop";
pub const TRANSFORM_SCHEMA: &str = "ffcine.geo.transform";

/// `ffcine.geo.crop/1`: sides as fractions of the source picture, cut before the transform.
#[derive(Debug, Clone, PartialEq)]
pub struct Crop {
    pub left: Keyed<f64>,
    pub right: Keyed<f64>,
    pub top: Keyed<f64>,
    pub bottom: Keyed<f64>,
    /// Soft edge inside the cut, in picture widths.
    pub feather: Keyed<f64>,
    /// Scale the cropped picture back up to the frame (FCPXML / FCP7 crop mode).
    pub fill: bool,
}

/// `ffcine.geo.transform/1`: picture point `q` (from the picture's middle) lands at
/// `A + R·S·(flip(q) − A) + T` from the frame's middle.
#[derive(Debug, Clone, PartialEq)]
pub struct Transform {
    /// `"units": "frame"`: anchor and translation in output frame sizes, else picture sizes.
    pub frame_units: bool,
    /// `"y": "up"`: positions with y up (negated); the importers write `down`.
    pub y_up: bool,
    pub anchor: Keyed<[f64; 2]>,
    pub translate_x: Keyed<f64>,
    pub translate_y: Keyed<f64>,
    pub scale_x: Keyed<f64>,
    pub scale_y: Keyed<f64>,
    /// Counter-clockwise on screen.
    pub rotate_deg: Keyed<f64>,
    pub flip: [bool; 2],
}

/// A clip's NLE crop and transforms.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Geo {
    pub crop: Option<Crop>,
    /// Applied in effect order (Resolve's Transform, then its Dynamic Zoom).
    pub transforms: Vec<Transform>,
}

/// One output frame's mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapped {
    /// Output pixel -> source pixel (before the stabilizer's warp).
    pub m: Mat3,
    /// The picture's part that shows, in source pixels: black outside.
    pub mask: Rect,
    /// Soft edge inside `mask`, source pixels.
    pub feather: f64,
}

fn num(m: &Map<String, Value>, k: &'static str, d: f64) -> Result<Keyed<f64>> {
    m.get(k)
        .filter(|v| !v.is_null())
        .map_or(Ok(Keyed::Const(d)), |v| Keyed::parse(v, k))
}

fn flag(m: &Map<String, Value>, k: &str) -> bool {
    m.get(k).and_then(Value::as_bool).unwrap_or(false)
}

impl Crop {
    pub fn parse(m: &Map<String, Value>) -> Result<Self> {
        Ok(Self {
            left: num(m, "left", 0.0)?,
            right: num(m, "right", 0.0)?,
            top: num(m, "top", 0.0)?,
            bottom: num(m, "bottom", 0.0)?,
            feather: num(m, "feather", 0.0)?,
            fill: flag(m, "fill"),
        })
    }

    /// The kept part of a `sw x sh` source at source time `t`, at least a pixel each way.
    pub fn rect(&self, t: f64, sw: f64, sh: f64) -> Rect {
        let ev = |k: &Keyed<f64>| k.eval(t).unwrap_or(0.0).clamp(0.0, 1.0);
        let (x0, x1) = (ev(&self.left) * sw, sw - ev(&self.right) * sw);
        let (y0, y1) = (ev(&self.top) * sh, sh - ev(&self.bottom) * sh);
        Rect {
            x: x0.min(sw - 1.0),
            y: y0.min(sh - 1.0),
            w: (x1 - x0).max(1.0),
            h: (y1 - y0).max(1.0),
        }
    }

    fn is_animated(&self) -> bool {
        [&self.left, &self.right, &self.top, &self.bottom]
            .iter()
            .any(|k| k.is_animated())
    }
}

impl Transform {
    pub fn parse(m: &Map<String, Value>) -> Result<Self> {
        let anchor = match m.get("anchor").filter(|v| !v.is_null()) {
            Some(v) => Keyed::parse(v, "transform anchor")?,
            None => Keyed::Const([0.0, 0.0]),
        };
        let units = m.get("units").and_then(Value::as_str);
        if !matches!(units, None | Some("picture" | "frame")) {
            return Err(Error::Invalid {
                what: "transform units",
                value: format!("{units:?}"),
            });
        }
        Ok(Self {
            frame_units: units == Some("frame"),
            y_up: m.get("y").and_then(Value::as_str) == Some("up"),
            anchor,
            translate_x: num(m, "translate_x", 0.0)?,
            translate_y: num(m, "translate_y", 0.0)?,
            scale_x: num(m, "scale_x", 1.0)?,
            scale_y: num(m, "scale_y", 1.0)?,
            rotate_deg: num(m, "rotate_deg", 0.0)?,
            flip: [flag(m, "flip_x"), flag(m, "flip_y")],
        })
    }

    /// At source time `t`, for a picture `unit` px (anchor and translation units): picture point
    /// `q` -> frame point (both px from the middles, y down).
    pub fn forward(&self, t: f64, unit: [f64; 2]) -> Mat3 {
        let ev = |k: &Keyed<f64>, d: f64| k.eval(t).unwrap_or(d);
        let y = if self.y_up { -1.0 } else { 1.0 };
        let [ax, ay] = self.anchor.eval(t).unwrap_or([0.0, 0.0]);
        let a = [ax * unit[0], y * ay * unit[1]];
        let tr = [
            ev(&self.translate_x, 0.0) * unit[0],
            y * ev(&self.translate_y, 0.0) * unit[1],
        ];
        // Zero scales would have no inverse: a hair above, so the picture vanishes instead.
        let s = |v: f64| if v.abs() < 1e-6 { 1e-6 } else { v };
        let (sx, sy) = (s(ev(&self.scale_x, 1.0)), s(ev(&self.scale_y, 1.0)));
        let (sin, cos) = ev(&self.rotate_deg, 0.0).to_radians().sin_cos();
        let flip = [
            [if self.flip[0] { -1.0 } else { 1.0 }, 0.0, 0.0],
            [0.0, if self.flip[1] { -1.0 } else { 1.0 }, 0.0],
            [0.0, 0.0, 1.0],
        ];
        // Counter-clockwise on screen with y down (`import::resolve::Placement::apply`).
        let rs = [
            [cos * sx, sin * sy, 0.0],
            [-sin * sx, cos * sy, 0.0],
            [0.0, 0.0, 1.0],
        ];
        let about = geom::mul(
            &geom::translation(a[0] + tr[0], a[1] + tr[1]),
            &geom::mul(&rs, &geom::translation(-a[0], -a[1])),
        );
        geom::mul(&about, &flip)
    }

    fn is_animated(&self) -> bool {
        self.anchor.is_animated()
            || [
                &self.translate_x,
                &self.translate_y,
                &self.scale_x,
                &self.scale_y,
                &self.rotate_deg,
            ]
            .iter()
            .any(|k| k.is_animated())
    }
}

impl Geo {
    /// The clip's first `ffcine.geo.crop/1` and every `ffcine.geo.transform/1`.
    pub fn from_effects(effects: &[ffcine_timeline::schema::Effect]) -> Result<Self> {
        let mut geo = Self::default();
        for e in effects {
            let ffcine_timeline::schema::Effect::Effect(g) = e else {
                continue;
            };
            if geo.crop.is_none()
                && let Some(p) = g.ffcine_params::<Map<String, Value>>(CROP_SCHEMA, 1)
            {
                geo.crop = Some(Crop::parse(&p?)?);
            } else if let Some(p) = g.ffcine_params::<Map<String, Value>>(TRANSFORM_SCHEMA, 1) {
                geo.transforms.push(Transform::parse(&p?)?);
            }
        }
        Ok(geo)
    }

    pub fn is_empty(&self) -> bool {
        self.crop.is_none() && self.transforms.is_empty()
    }

    pub fn is_animated(&self) -> bool {
        self.crop.as_ref().is_some_and(Crop::is_animated)
            || self.transforms.iter().any(Transform::is_animated)
    }

    /// Output frame `i`'s window (in the cropped picture with `fill`) and its full mapping.
    #[allow(clippy::cast_precision_loss)]
    pub fn frame(&self, reframe: &Reframe, i: u64, shot: &Shot, out: &Profile) -> (Frame, Mapped) {
        let rate = if shot.rate > 0.0 { shot.rate } else { 24.0 };
        let t = shot.start + i as f64 / rate;
        let (sw, sh) = (f64::from(shot.width), f64::from(shot.height));
        let full = Rect::frame(sw, sh);
        let cut = self.crop.as_ref().map(|c| c.rect(t, sw, sh));
        let picture = match (&self.crop, cut) {
            (Some(c), Some(r)) if c.fill => r,
            _ => full,
        };
        let frame = reframe.frame_in(i, shot, out, picture);
        let (ow, oh) = (f64::from(out.width), f64::from(out.height));
        // The window: output pixel -> source pixel, onto the placed rectangle with `contain`.
        let at = frame.place.unwrap_or(Rect::frame(ow, oh));
        let (kx, ky) = (frame.crop.w / at.w, frame.crop.h / at.h);
        let window = [
            [kx, 0.0, frame.crop.x - at.x * kx],
            [0.0, ky, frame.crop.y - at.y * ky],
            [0.0, 0.0, 1.0],
        ];
        let m = if self.transforms.is_empty() {
            window
        } else {
            {
                // The picture's middle and size in output pixels.
                let inv = geom::inverse(&window).unwrap_or(geom::IDENTITY);
                let mid = geom::apply(
                    &inv,
                    [picture.x + picture.w / 2.0, picture.y + picture.h / 2.0],
                );
                let fwd = self.transforms.iter().fold(geom::IDENTITY, |acc, tf| {
                    let unit = if tf.frame_units {
                        [ow, oh]
                    } else {
                        [picture.w / kx, picture.h / ky]
                    };
                    geom::mul(&tf.forward(t, unit), &acc)
                });
                let back = geom::inverse(&fwd).unwrap_or(geom::IDENTITY);
                // Output pixel -> frame point -> picture point -> output pixel of the picture.
                let g = geom::mul(
                    &geom::translation(mid[0], mid[1]),
                    &geom::mul(&back, &geom::translation(-ow / 2.0, -oh / 2.0)),
                );
                geom::mul(&window, &g)
            }
        };
        let feather = self
            .crop
            .as_ref()
            .map_or(0.0, |c| c.feather.eval(t).unwrap_or(0.0).max(0.0) * sw);
        (
            frame,
            Mapped {
                m,
                mask: cut.unwrap_or(full),
                feather,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HD: Shot = Shot {
        width: 1920,
        height: 1080,
        frames: 48,
        rate: 24.0,
        start: 0.0,
    };

    fn geo(crop: Option<Value>, transform: Option<Value>) -> Geo {
        Geo {
            crop: crop.map(|v| Crop::parse(v.as_object().unwrap()).unwrap()),
            transforms: transform
                .map(|v| Transform::parse(v.as_object().unwrap()).unwrap())
                .into_iter()
                .collect(),
        }
    }

    fn near(a: [f64; 2], b: [f64; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6
    }

    #[test]
    fn identity_is_the_window() {
        let out = Profile::new("hd", 1920, 1080);
        let (f, m) = geo(None, None).frame(&Reframe::default(), 0, &HD, &out);
        assert_eq!(f.crop, Rect::frame(1920.0, 1080.0));
        assert!(near(geom::apply(&m.m, [10.0, 20.0]), [10.0, 20.0]));
        assert_eq!(m.mask, Rect::frame(1920.0, 1080.0));
    }

    #[test]
    fn matches_the_importers_placement() {
        // The importer's worked example: picture 1000x500 at 1:1, anchor (0.25, -0.1),
        // scale 2, 90° ccw: picture point A lands at A + T; the middle at A − R·S·A + T.
        let out = Profile::new("o", 1000, 500);
        let shot = Shot {
            width: 1000,
            height: 500,
            ..HD
        };
        let tf = json!({"units": "picture", "y": "down", "anchor": [0.25, -0.1],
                        "translate_x": 0.1, "translate_y": 0.0, "scale_x": 2.0, "scale_y": 2.0,
                        "rotate_deg": 90.0});
        let (_, m) = geo(None, Some(tf)).frame(&Reframe::default(), 0, &shot, &out);
        // Output pixel of the anchor: frame middle + A + T = (500 + 250 + 100, 250 - 50).
        let src = geom::apply(&m.m, [850.0, 200.0]);
        assert!(near(src, [750.0, 200.0]), "{src:?}");
        // The picture's middle: A − R·S·A + T = (250, -50) − (−100, −500) + (100, 0).
        let src = geom::apply(&m.m, [500.0 + 450.0, 250.0 + 450.0]);
        assert!(near(src, [500.0, 250.0]), "{src:?}");
    }

    #[test]
    fn zoom_out_shows_past_the_window_and_frame_units() {
        let out = Profile::new("hd", 1920, 1080);
        let tf = json!({"units": "frame", "translate_x": 0.25, "scale_x": 0.5, "scale_y": 0.5});
        let (_, m) = geo(None, Some(tf)).frame(&Reframe::default(), 0, &HD, &out);
        // Half size, its middle a quarter frame right of centre.
        assert!(near(geom::apply(&m.m, [1440.0, 540.0]), [960.0, 540.0]));
        assert!(near(geom::apply(&m.m, [960.0, 270.0]), [0.0, 0.0]));
        // Left of the picture maps outside the source (black).
        assert!(geom::apply(&m.m, [100.0, 540.0])[0] < 0.0);
    }

    #[test]
    fn crop_masks_or_fills() {
        let out = Profile::new("hd", 1920, 1080);
        let cut = json!({"left": 0.25, "right": 0.25, "top": 0.0, "bottom": 0.5, "feather": 0.01});
        let (f, m) = geo(Some(cut.clone()), None).frame(&Reframe::default(), 0, &HD, &out);
        assert_eq!(f.crop, Rect::frame(1920.0, 1080.0));
        assert_eq!(
            m.mask,
            Rect {
                x: 480.0,
                y: 0.0,
                w: 960.0,
                h: 540.0
            }
        );
        assert!((m.feather - 19.2).abs() < 1e-9);
        // `fill`: the kept part is the picture the window fits the frame to.
        let mut fill = cut;
        fill["fill"] = json!(true);
        let (f, m) = geo(Some(fill), None).frame(&Reframe::default(), 0, &HD, &out);
        assert_eq!(f.crop, m.mask);
        assert!(near(geom::apply(&m.m, [0.0, 0.0]), [480.0, 0.0]));
        assert!(near(geom::apply(&m.m, [1920.0, 1080.0]), [1440.0, 540.0]));
    }

    #[test]
    fn keyframes_and_effects() {
        let tf = json!({"translate_x": [{"t": 0.0, "v": 0.0}, {"t": 1.0, "v": 0.5}]});
        let g = geo(None, Some(tf));
        assert!(g.is_animated() && !g.is_empty());
        let out = Profile::new("hd", 1920, 1080);
        let (_, m) = g.frame(&Reframe::default(), 12, &HD, &out);
        // Half a second in: a quarter picture right, so output 960 + 480 shows the middle.
        assert!(near(geom::apply(&m.m, [1440.0, 540.0]), [960.0, 540.0]));
        assert!(Transform::parse(json!({"units": "inches"}).as_object().unwrap()).is_err());
        // Several transforms chain in effect order: x2 then x0.5 is the identity.
        let mut two = geo(
            None,
            Some(json!({"scale_x": 2.0, "scale_y": 2.0, "translate_x": 0.1})),
        );
        two.transforms.push(
            Transform::parse(json!({"scale_x": 0.5, "scale_y": 0.5}).as_object().unwrap()).unwrap(),
        );
        let (_, m) = two.frame(&Reframe::default(), 0, &HD, &out);
        // The first's translation is halved by the second: 0.1 picture widths x 0.5 = 96 px.
        assert!(near(
            geom::apply(&m.m, [960.0 + 96.0, 540.0]),
            [960.0, 540.0]
        ));
        assert!(near(geom::apply(&m.m, [96.0, 0.0]), [0.0, 0.0]));
    }
}
