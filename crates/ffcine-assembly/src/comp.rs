//! Compositing maths for assembly, backend-neutral and per frame: the per-shot finish
//! (`ffcine.fx.vignette/1`, `fade/1`, `flash/1`, `darken/1`), transition curves
//! (`ffcine.transition/1`), split-screen geometry (`ffcine.comp.layout/1`) and overlay placement
//! (`ffcine.geo.transform/1` + `ffcine.comp.blend/1`). `exec` turns these into GPU hooks and
//! layers.

use std::f64::consts::FRAC_PI_2;

use ffcine_reframe::Keyed;
use ffcine_render::BlendMode;
use serde_json::{Map, Value, json};

fn f(m: &Map<String, Value>, k: &str) -> Option<f64> {
    m.get(k).and_then(Value::as_f64)
}

/// `[r, g, b]` from a list or `"black"`/`"white"`.
pub fn colour(v: Option<&Value>, default: [f64; 3]) -> [f64; 3] {
    match v {
        Some(Value::String(s)) if s == "white" => [1.0; 3],
        Some(Value::String(s)) if s == "black" => [0.0; 3],
        Some(Value::Array(a)) if a.len() >= 3 => {
            let c = |i: usize| a[i].as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
            [c(0), c(1), c(2)]
        }
        _ => default,
    }
}

/// Per-shot finishing after the grade (`render.py`'s `post` filters).
#[derive(Debug, Clone, PartialEq)]
pub struct Finish {
    /// `ffcine.fx.vignette/1` `amount`; ffmpeg's `vignette` angle is `amount · π/2`.
    pub vignette: f64,
    /// `ffcine.fx.darken/1` `amount`: RGB multiplier.
    pub darken: f64,
    /// `ffcine.fx.fade/1`: seconds from / to `fade_colour` at the clip's start / end.
    pub fade_in: f64,
    pub fade_out: f64,
    pub fade_colour: [f64; 3],
    /// `ffcine.fx.flash/1`: `at` seconds from the clip's first frame, then one lift per frame.
    pub flash: Option<(f64, Vec<f64>)>,
}

impl Default for Finish {
    fn default() -> Self {
        Self {
            vignette: 0.0,
            darken: 1.0,
            fade_in: 0.0,
            fade_out: 0.0,
            fade_colour: [0.0; 3],
            flash: None,
        }
    }
}

impl Finish {
    /// Folds one effect's `metadata.ffcine` into the finish; `false` if `schema` isn't one.
    pub fn add(&mut self, schema: (&str, u32), m: &Map<String, Value>) -> bool {
        match schema {
            ("ffcine.fx.vignette", 1) => self.vignette = f(m, "amount").unwrap_or(0.0),
            ("ffcine.fx.darken", 1) => self.darken = f(m, "amount").unwrap_or(1.0),
            ("ffcine.fx.fade", 1) => {
                self.fade_in = f(m, "in").unwrap_or(0.0).max(0.0);
                self.fade_out = f(m, "out").unwrap_or(0.0).max(0.0);
                self.fade_colour = colour(m.get("colour"), [0.0; 3]);
            }
            ("ffcine.fx.flash", 1) => {
                let levels = m.get("levels").and_then(Value::as_array).map_or_else(
                    || vec![0.5, 0.5, 0.2],
                    |a| a.iter().filter_map(Value::as_f64).collect(),
                );
                self.flash = Some((f(m, "at").unwrap_or(0.0), levels));
            }
            _ => return false,
        }
        true
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the result depends on where a piece sits in its clip (fades, flash).
    pub fn is_timed(&self) -> bool {
        self.fade_in > 0.0 || self.fade_out > 0.0 || self.flash.is_some()
    }

    /// Per-frame `(fade, lift)` for frames `at..at + n` of a clip `len` frames long, ffmpeg's
    /// `fade` maths (frame time `i / rate`; `fade` 0 is the colour, 1 the picture).
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub fn tracks(&self, at: i64, n: i64, len: i64, rate: f64) -> (Vec<f32>, Vec<f32>) {
        let flash_at = self
            .flash
            .as_ref()
            .map(|(s, l)| ((s * rate).round() as i64, l));
        (at..at + n)
            .map(|i| {
                let t = i as f64 / rate;
                let mut k: f64 = 1.0;
                if self.fade_in > 0.0 {
                    k = k.min(t / self.fade_in);
                }
                if self.fade_out > 0.0 {
                    let st = len as f64 / rate - self.fade_out;
                    k = k.min(1.0 - (t - st) / self.fade_out);
                }
                let lift = flash_at
                    .and_then(|(s, l)| usize::try_from(i - s).ok().and_then(|j| l.get(j)))
                    .copied()
                    .unwrap_or(0.0);
                (k.clamp(0.0, 1.0) as f32, lift as f32)
            })
            .unzip()
    }

    /// `render.py`'s vignette angle in radians.
    pub fn vignette_angle(&self) -> f64 {
        self.vignette * FRAC_PI_2
    }
}

/// How a transition mixes its two sides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransitionKind {
    Dissolve,
    /// Out to the colour over the first half, in from it over the second.
    Dip([f64; 3]),
    /// Blacks lifted to white and back (a shutter flash at the cut).
    Flash,
    /// The picture moves the way of the unit vector, motion-blurred.
    Whip([f64; 2]),
}

impl TransitionKind {
    /// From a `Transition`'s `metadata.ffcine` (`ffcine.transition/1`); anything else is a
    /// dissolve (docs/timeline.md).
    pub fn from_meta(m: Option<&Map<String, Value>>) -> Self {
        let Some(m) = m else { return Self::Dissolve };
        match m.get("kind").and_then(Value::as_str) {
            Some("dip") => Self::Dip(colour(m.get("colour"), [0.0; 3])),
            Some("flash") => {
                let c = colour(m.get("colour"), [1.0; 3]);
                if c.iter().all(|x| (x - 1.0).abs() < 1e-9) {
                    Self::Flash
                } else {
                    Self::Dip(c)
                }
            }
            Some("whip") => Self::Whip(match m.get("direction").and_then(Value::as_str) {
                Some("right") => [1.0, 0.0],
                Some("up") => [0.0, -1.0],
                Some("down") => [0.0, 1.0],
                _ => [-1.0, 0.0],
            }),
            _ => Self::Dissolve,
        }
    }

    pub fn recipe(&self) -> Value {
        match self {
            Self::Dissolve => json!("dissolve"),
            Self::Dip(c) => json!({"dip": c}),
            Self::Flash => json!("flash"),
            Self::Whip(d) => json!({"whip": d}),
        }
    }
}

/// Progress through frame `i` of `n`, sampled at the frame's middle.
#[allow(clippy::cast_precision_loss)]
pub fn progress(i: i64, n: i64) -> f64 {
    ((i as f64 + 0.5) / n.max(1) as f64).clamp(0.0, 1.0)
}

/// `3p² − 2p³`.
pub fn smoothstep(p: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

pub use ffcine_comp::templates::layout::{Layout, Rect};

/// The source window (x, y, w, h) of a `sw x sh` picture covering a `w x h` slot around `focus`
/// (fractions, `[0.5, 0.5]` = centred), zoomed in by `zoom` (>= 1) around the same point.
pub fn cover(sw: f64, sh: f64, w: f64, h: f64, focus: [f64; 2], zoom: f64) -> Rect {
    let (mut cw, mut ch) = if sw / sh > w / h {
        (sh * w / h, sh)
    } else {
        (sw, sw * h / w)
    };
    let z = zoom.max(1e-3);
    cw /= z;
    ch /= z;
    Rect {
        x: (sw - cw) * focus[0].clamp(0.0, 1.0),
        y: (sh - ch) * focus[1].clamp(0.0, 1.0),
        w: cw,
        h: ch,
    }
}

/// An overlay's placement and opacity over time: `ffcine.geo.transform/1` and
/// `ffcine.comp.blend/1` (`opacity`, `mode`, `radius` as a fraction of the frame height). Each
/// number may be keyframes in source time.
///
/// The picture `P` is the source fitted inside the frame. `translate_x`/`translate_y` and
/// `anchor` are fractions of `P` (`units: "picture"`) or of the frame (`units: "frame"`, the
/// default), y down; with `A` the anchor and `T` the translation in pixels, `S` the scale and `R`
/// the rotation (`rotate_deg`, counter-clockwise on screen), picture point `q` (from its middle)
/// lands at `A + R·S·(q − A) + T` from the frame's middle (docs/timeline.md).
#[derive(Debug, Clone)]
pub struct Placement {
    picture_units: bool,
    tx: Keyed<f64>,
    ty: Keyed<f64>,
    sx: Keyed<f64>,
    sy: Keyed<f64>,
    rotate: Keyed<f64>,
    anchor: Keyed<[f64; 2]>,
    opacity: Keyed<f64>,
    pub mode: BlendMode,
    pub radius: f64,
}

impl Placement {
    /// From the two effects' parameters (either may be empty). Unparseable values fall back
    /// to the defaults; `warn` gets what's ignored.
    pub fn parse(
        transform: &Map<String, Value>,
        blend: &Map<String, Value>,
        warn: &mut dyn FnMut(String),
    ) -> Self {
        let mut key = |m: &Map<String, Value>, k: &'static str, d: f64| match m.get(k) {
            None => Keyed::Const(d),
            Some(v) => Keyed::parse(v, k).unwrap_or_else(|e| {
                warn(format!("{k}: {e}"));
                Keyed::Const(d)
            }),
        };
        let tx = key(transform, "translate_x", 0.0);
        let ty = key(transform, "translate_y", 0.0);
        let sx = key(transform, "scale_x", 1.0);
        let sy = key(transform, "scale_y", 1.0);
        let rotate = key(transform, "rotate_deg", 0.0);
        let opacity = key(blend, "opacity", 1.0);
        let anchor = match transform.get("anchor") {
            None => Keyed::Const([0.0; 2]),
            Some(v) => Keyed::parse(v, "anchor").unwrap_or_else(|e| {
                warn(format!("anchor: {e}"));
                Keyed::Const([0.0; 2])
            }),
        };
        let mode = match blend.get("mode").and_then(Value::as_str) {
            None => BlendMode::Normal,
            Some(m) => m.parse().unwrap_or_else(|_| {
                warn(format!("blend mode {m} rendered as normal"));
                BlendMode::Normal
            }),
        };
        Self {
            picture_units: transform.get("units").and_then(Value::as_str) == Some("picture"),
            tx,
            ty,
            sx,
            sy,
            rotate,
            anchor,
            opacity,
            mode,
            radius: f(blend, "radius").unwrap_or(0.0),
        }
    }

    /// The unrotated rectangle of a `sw x sh` picture at source time `t` in a `w x h` frame;
    /// [`Self::rotation`] turns it about its middle.
    #[allow(clippy::many_single_char_names)]
    pub fn rect(&self, t: f64, sw: f64, sh: f64, w: u32, h: u32) -> Rect {
        let (fw, fh) = (f64::from(w), f64::from(h));
        let fit = (fw / sw).min(fh / sh);
        let ev = |k: &Keyed<f64>, d| k.eval(t).unwrap_or(d);
        let (s0, s1) = (ev(&self.sx, 1.0), ev(&self.sy, 1.0));
        let picture = [sw * fit, sh * fit];
        let unit = if self.picture_units {
            picture
        } else {
            [fw, fh]
        };
        let [ax, ay] = self.anchor.eval(t).unwrap_or([0.0; 2]);
        let a = [ax * unit[0], ay * unit[1]];
        // The picture's middle lands at A − R·S·A + T.
        let (sin, cos) = self.rotation(t).to_radians().sin_cos();
        let z = [-a[0] * s0, -a[1] * s1];
        let turned = [cos * z[0] + sin * z[1], -sin * z[0] + cos * z[1]];
        let (pw, ph) = (picture[0] * s0, picture[1] * s1);
        let (cx, cy) = (
            fw / 2.0 + a[0] + turned[0] + ev(&self.tx, 0.0) * unit[0],
            fh / 2.0 + a[1] + turned[1] + ev(&self.ty, 0.0) * unit[1],
        );
        Rect {
            x: cx - pw / 2.0,
            y: cy - ph / 2.0,
            w: pw,
            h: ph,
        }
    }

    /// Degrees counter-clockwise on screen at source time `t`.
    pub fn rotation(&self, t: f64) -> f64 {
        self.rotate.eval(t).unwrap_or(0.0)
    }

    pub fn opacity(&self, t: f64) -> f64 {
        self.opacity.eval(t).unwrap_or(1.0).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)]
    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn fades_follow_ffmpeg() {
        let mut fin = Finish::default();
        assert!(fin.add(
            ("ffcine.fx.fade", 1),
            &obj(json!({"in": 1.0, "out": 0.5, "colour": "white"}))
        ));
        assert!(fin.is_timed());
        assert_eq!(fin.fade_colour, [1.0; 3]);
        // 24 fps, a 48-frame clip: in over frames 0..24, out over the last 12.
        let (k, lift) = fin.tracks(0, 48, 48, 24.0);
        assert!(k[0].abs() < 1e-6);
        assert!((k[12] - 0.5).abs() < 1e-6);
        assert!((k[30] - 1.0).abs() < 1e-6);
        assert!((k[47] - 1.0 / 12.0).abs() < 1e-5);
        assert!(lift.iter().all(|l| *l == 0.0));
        // A later piece of the same clip gets the same values.
        let (k2, _) = fin.tracks(40, 8, 48, 24.0);
        assert_eq!(k2, k[40..]);
    }

    #[test]
    fn flash_lifts_frames() {
        let mut fin = Finish::default();
        fin.add(("ffcine.fx.flash", 1), &obj(json!({"at": 0.5})));
        let (_, lift) = fin.tracks(10, 6, 100, 24.0);
        assert_eq!(lift, [0.0, 0.0, 0.5, 0.5, 0.2, 0.0]);
    }

    #[test]
    fn transition_kinds() {
        let k = |v: Value| TransitionKind::from_meta(Some(&obj(v)));
        assert_eq!(k(json!({"kind": "dip"})), TransitionKind::Dip([0.0; 3]));
        assert_eq!(
            k(json!({"kind": "dip", "colour": "white"})),
            TransitionKind::Dip([1.0; 3])
        );
        assert_eq!(k(json!({"kind": "flash"})), TransitionKind::Flash);
        assert_eq!(
            k(json!({"kind": "whip", "direction": "up"})),
            TransitionKind::Whip([0.0, -1.0])
        );
        assert_eq!(k(json!({"kind": "wipe"})), TransitionKind::Dissolve);
        assert_eq!(TransitionKind::from_meta(None), TransitionKind::Dissolve);
    }

    #[test]
    fn cover_and_placement() {
        let c = cover(3840.0, 2160.0, 960.0, 1080.0, [0.5, 0.5], 1.0);
        assert!((c.w - 1920.0).abs() < 1e-9 && (c.x - 960.0).abs() < 1e-9);
        let mut warns = Vec::new();
        let p = Placement::parse(
            &obj(json!({"translate_x": 0.25, "scale_x": 0.5, "scale_y": 0.5})),
            &obj(json!({"opacity": [{"t": 0.0, "v": 0.0}, {"t": 1.0, "v": 1.0}], "radius": 0.02})),
            &mut |w| warns.push(w),
        );
        assert_eq!(warns, Vec::<String>::new());
        let r = p.rect(0.0, 1920.0, 1080.0, 1920, 1080);
        assert_eq!((r.x, r.y, r.w, r.h), (960.0, 270.0, 960.0, 540.0));
        assert!((p.opacity(0.5) - 0.5).abs() < 1e-9);
        assert_eq!(p.mode, BlendMode::Normal);
        assert!(p.rotation(0.0).abs() < 1e-12);
    }

    #[test]
    fn rotation_pivots_on_the_anchor() {
        let mut warns = Vec::new();
        // A half-size picture turned 90° about its top left corner (anchor in picture units).
        let p = Placement::parse(
            &obj(
                json!({"units": "picture", "anchor": [-0.5, -0.5], "scale_x": 0.5,
                        "scale_y": 0.5, "rotate_deg": 90.0}),
            ),
            &obj(json!({"mode": "multiply"})),
            &mut |w| warns.push(w),
        );
        assert_eq!(warns, Vec::<String>::new());
        assert_eq!(p.mode, BlendMode::Multiply);
        assert!((p.rotation(0.0) - 90.0).abs() < 1e-12);
        let r = p.rect(0.0, 1920.0, 1080.0, 1920, 1080);
        // The anchor (frame point (0, 0)) stays put; counter-clockwise with y down, the
        // picture's middle (480, 270) from it goes to (270, −480).
        let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!((r.w - 960.0).abs() < 1e-9 && (r.h - 540.0).abs() < 1e-9);
        assert!((cx - 270.0).abs() < 1e-9, "{cx}");
        assert!((cy + 480.0).abs() < 1e-9, "{cy}");
        // Matches the importer's maths (resolve::Placement::apply) at the picture's middle.
        let q = ffcine_import::resolve::Placement {
            anchor: [-0.5, -0.5],
            scale: [0.5, 0.5],
            rotate_deg: 90.0,
            ..Default::default()
        }
        .apply([1920.0, 1080.0], [0.0, 0.0]);
        assert!((cx - 960.0 - q[0]).abs() < 1e-9 && (cy - 540.0 - q[1]).abs() < 1e-9);
        // Keyframed rotation, frame units; a foreign mode falls back to normal with a warning.
        let k = Placement::parse(
            &obj(
                json!({"rotate_deg": [{"t": 0.0, "v": 0.0}, {"t": 1.0, "v": 30.0}],
                        "anchor": [0.1, 0.0]}),
            ),
            &obj(json!({"mode": "resolve:27"})),
            &mut |w| warns.push(w),
        );
        assert!((k.rotation(0.5) - 15.0).abs() < 1e-9);
        assert_eq!(k.mode, BlendMode::Normal);
        assert_eq!(warns.len(), 1);
        // Unturned, the anchor doesn't move the picture.
        let r0 = k.rect(0.0, 1920.0, 1080.0, 1920, 1080);
        assert!(r0.x.abs() < 1e-9 && r0.y.abs() < 1e-9);
    }
}
