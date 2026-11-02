//! `ffcine.geo.reframe/1`: the prototype's zoom / punch / push / pan / fit fields (`edit.yaml`,
//! `render.py` `gpu_geo`), any of `zoom`, `zoom_center`, `crop_x`, `crop_y` also in the keyframe
//! form, plus per-profile overrides. Evaluated per output frame into a crop window.

// Frame maths reads best with the usual one-letter names.
#![allow(clippy::many_single_char_names)]

use ffcine_core::{Error, Result};
use serde_json::{Map, Value};

use crate::keyframe::{Key, Keyed, ease, is_keys};
use crate::profile::Profile;
use crate::window::Rect;

pub const SCHEMA: &str = "ffcine.geo.reframe";
pub const MAJOR: u32 = 1;

/// A timed punch-in: `zoom` (capped at the profile's `max_punch`, times the base zoom) from `at`
/// to `to` seconds, centred on `center` (else `zoom_center`).
#[derive(Debug, Clone, PartialEq)]
pub struct Punch {
    pub at: f64,
    pub to: Option<f64>,
    pub zoom: f64,
    pub center: Option<[f64; 2]>,
}

/// How the window fills the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Crop to the output aspect (the window slides by `crop_x` / `crop_y`).
    #[default]
    Cover,
    /// The whole picture at full output width, centred at `fit_y`, a blurred copy around it
    /// (`fill_blur`; 0 = black).
    Contain,
}

/// Where the output-aspect window sits on its sliding axis.
#[derive(Debug, Clone, PartialEq)]
pub enum Slide {
    /// Constant, or the keyframe form (`t` in source time).
    Keyed(Keyed<f64>),
    /// The prototype's `[[t, x], ...]`: eased keys, `t` in seconds from the clip's first frame.
    ClipKeys(Keyed<f64>),
    /// `[from, to]`: an eased pan over the whole shot.
    Pan(f64, f64),
}

impl Default for Slide {
    fn default() -> Self {
        Self::Keyed(Keyed::Const(0.5))
    }
}

impl Slide {
    fn parse(v: &Value, what: &'static str) -> Result<Self> {
        let invalid = || Error::Invalid {
            what,
            value: v.to_string(),
        };
        if v.is_null() {
            return Ok(Self::default());
        }
        if is_keys(v) || v.is_number() {
            return Keyed::parse(v, what).map(Self::Keyed);
        }
        let a = v.as_array().ok_or_else(invalid)?;
        if a.first().is_some_and(Value::is_array) {
            let keys = a
                .iter()
                .map(|k| match k.as_array().map(Vec::as_slice) {
                    Some([t, x]) => Some(Key {
                        t: t.as_f64()?,
                        v: x.as_f64()?,
                        interp: crate::keyframe::Interp::Ease,
                    }),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(invalid)?;
            return Ok(Self::ClipKeys(Keyed::from_keys(keys)));
        }
        match a.as_slice() {
            [x, y] => Ok(Self::Pan(
                x.as_f64().ok_or_else(invalid)?,
                y.as_f64().ok_or_else(invalid)?,
            )),
            _ => Err(invalid()),
        }
    }

    /// At `t` seconds into the clip, which starts at `start` in source time.
    fn eval(&self, t: f64, start: f64, duration: f64) -> f64 {
        match self {
            Self::Keyed(k) => k.eval(start + t).unwrap_or(0.5),
            Self::ClipKeys(k) => k.eval(t).unwrap_or(0.5),
            Self::Pan(a, b) => a + (b - a) * ease(t / duration.max(1e-3)),
        }
    }
}

/// The shot being reframed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shot {
    /// Source size in pixels.
    pub width: u32,
    pub height: u32,
    /// Output frames in the shot.
    pub frames: u64,
    /// Output frame rate.
    pub rate: f64,
    /// Source time of the clip's first frame in seconds (`source_range.start_time`): keyframe
    /// `t`s are source time, so frame `i` evaluates them at `start + i / rate`.
    pub start: f64,
}

/// One output frame's geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// Source window, in source pixels.
    pub crop: Rect,
    /// Where it lands in the output, in output pixels; `None` fills the frame.
    pub place: Option<Rect>,
    /// Around a placed picture: a blurred copy of it, this radius in pixels of the placed
    /// picture; `None` is black.
    pub blur: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reframe {
    /// Base zoom (1 = the whole output-aspect window).
    pub zoom: Option<Keyed<f64>>,
    pub zoom_center: Option<Keyed<[f64; 2]>>,
    pub punch: Vec<Punch>,
    /// `[from, to]` zoom, eased over the whole shot (half linear, half cosine, as `render.py`).
    pub push: Option<(f64, f64)>,
    /// Centre of the push (default `zoom_center`).
    pub push_center: Option<[f64; 2]>,
    /// The push centre moving `[from, to]` with the push's easing.
    pub pan: Option<([f64; 2], [f64; 2])>,
    /// Window position on wider sources (0 left .. 1 right).
    pub crop_x: Slide,
    /// Window position on taller sources (0 top .. 1 bottom).
    pub crop_y: Slide,
    pub fit: Fit,
    /// `fit: contain`: the picture's centre as a fraction of the output height (0.42).
    pub fit_y: f64,
    /// `fit: contain`: blur radius of the fill around the picture, as a fraction of the placed
    /// picture's height (0.04); 0 for black.
    pub fill_blur: f64,
    /// A split-screen panel's `[x, y]` focus: the default `crop_x`, `crop_y` and `zoom_center`
    /// when it renders as a panel ([`Self::focused`]).
    pub focus: Option<[f64; 2]>,
}

/// `fill_blur`'s default.
pub const FILL_BLUR: f64 = 0.04;

fn pair(v: &Value) -> Option<[f64; 2]> {
    match v.as_array()?.as_slice() {
        [a, b] => Some([a.as_f64()?, b.as_f64()?]),
        _ => None,
    }
}

impl Reframe {
    /// Reads the effect's parameters (`metadata.ffcine` without `schema`) for `profile`:
    /// `profiles.<profile>` fields replace the base ones. Unknown fields are ignored (the
    /// Resolve importer's `timeline_size`, the panels' `focus`).
    pub fn parse(params: &Map<String, Value>, profile: Option<&str>) -> Result<Self> {
        let mut p = params.clone();
        if let Some(over) = profile
            .and_then(|n| params.get("profiles")?.get(n))
            .and_then(Value::as_object)
        {
            for (k, v) in over {
                p.insert(k.clone(), v.clone());
            }
        }
        let invalid = |what: &'static str, v: &Value| Error::Invalid {
            what,
            value: v.to_string(),
        };
        let mut r = Self {
            fit_y: 0.42,
            fill_blur: FILL_BLUR,
            ..Self::default()
        };
        for (k, v) in &p {
            if v.is_null() {
                continue;
            }
            match k.as_str() {
                "zoom" => r.zoom = Some(Keyed::parse(v, "reframe zoom")?),
                "zoom_center" => r.zoom_center = Some(Keyed::parse(v, "reframe zoom_center")?),
                "punch" => {
                    for pu in v.as_array().ok_or_else(|| invalid("reframe punch", v))? {
                        let num = |k: &str| pu.get(k).and_then(Value::as_f64);
                        r.punch.push(Punch {
                            at: num("at").ok_or_else(|| invalid("punch at", pu))?,
                            to: num("to"),
                            zoom: num("zoom").unwrap_or(1.25),
                            center: pu.get("center").and_then(pair),
                        });
                    }
                }
                "push" => {
                    let [a, b] = pair(v).ok_or_else(|| invalid("reframe push", v))?;
                    r.push = Some((a, b));
                }
                "push_center" => {
                    r.push_center = Some(pair(v).ok_or_else(|| invalid("push_center", v))?);
                }
                "pan" => {
                    let a = v.as_array().ok_or_else(|| invalid("reframe pan", v))?;
                    match a.as_slice() {
                        [x, y] => {
                            r.pan = Some((
                                pair(x).ok_or_else(|| invalid("reframe pan", v))?,
                                pair(y).ok_or_else(|| invalid("reframe pan", v))?,
                            ));
                        }
                        _ => return Err(invalid("reframe pan", v)),
                    }
                }
                "crop_x" => r.crop_x = Slide::parse(v, "reframe crop_x")?,
                "crop_y" => r.crop_y = Slide::parse(v, "reframe crop_y")?,
                "fit" => {
                    r.fit = match v.as_str() {
                        Some("contain") => Fit::Contain,
                        Some("cover" | "fill") => Fit::Cover,
                        _ => return Err(invalid("reframe fit", v)),
                    }
                }
                // Resolve's sizing mode (`fit` letterboxes, `fill` crops).
                "mode" => {
                    r.fit = match v.as_str() {
                        Some("fit") => Fit::Contain,
                        _ => Fit::Cover,
                    }
                }
                "fit_y" => r.fit_y = v.as_f64().ok_or_else(|| invalid("reframe fit_y", v))?,
                "fill_blur" => {
                    r.fill_blur = v
                        .as_f64()
                        .filter(|b| *b >= 0.0)
                        .ok_or_else(|| invalid("reframe fill_blur", v))?;
                }
                "focus" => r.focus = Some(pair(v).ok_or_else(|| invalid("reframe focus", v))?),
                _ => {}
            }
        }
        Ok(r)
    }

    /// The first `ffcine.geo.reframe/1` among a clip's effects, for `profile`.
    pub fn from_effects(
        effects: &[ffcine_timeline::schema::Effect],
        profile: Option<&str>,
    ) -> Result<Option<Self>> {
        for e in effects {
            let ffcine_timeline::schema::Effect::Effect(g) = e else {
                continue;
            };
            if let Some(params) = g.ffcine_params::<Map<String, Value>>(SCHEMA, MAJOR) {
                return Self::parse(&params?, profile).map(Some);
            }
        }
        Ok(None)
    }

    /// For a split-screen panel: `focus` stands in for `crop_x`, `crop_y` and `zoom_center`
    /// where those aren't set.
    #[must_use]
    pub fn focused(mut self) -> Self {
        if let Some([x, y]) = self.focus {
            if self.crop_x == Slide::default() {
                self.crop_x = Slide::Keyed(Keyed::Const(x));
            }
            if self.crop_y == Slide::default() {
                self.crop_y = Slide::Keyed(Keyed::Const(y));
            }
            self.zoom_center.get_or_insert(Keyed::Const([x, y]));
        }
        self
    }

    /// Output frame `i`'s geometry.
    pub fn frame(&self, i: u64, shot: &Shot, out: &Profile) -> Frame {
        let full = Rect::frame(f64::from(shot.width), f64::from(shot.height));
        self.frame_in(i, shot, out, full)
    }

    /// Output frame `i`'s geometry with the picture being `full`, a part of the source (an NLE
    /// crop that fills the frame).
    #[allow(clippy::cast_precision_loss)]
    pub fn frame_in(&self, i: u64, shot: &Shot, out: &Profile, full: Rect) -> Frame {
        let rate = if shot.rate > 0.0 { shot.rate } else { 24.0 };
        let t = i as f64 / rate;
        let duration = shot.frames as f64 / rate;
        let (ow, oh) = (f64::from(out.width), f64::from(out.height));

        let src_t = shot.start + t;
        let base_zoom = self
            .zoom
            .as_ref()
            .and_then(|z| z.eval(src_t))
            .unwrap_or(1.0);
        let base_center = self
            .zoom_center
            .as_ref()
            .and_then(|c| c.eval(src_t))
            .unwrap_or([0.5, 0.5]);
        // The punch active at frame i (frame-exact like the prototype's s2f boundaries).
        let frame_of = |s: f64| (s * rate).round();
        let punch = self.punch.iter().rev().find(|p| {
            let (a, b) = (frame_of(p.at), p.to.map_or(f64::INFINITY, frame_of));
            (a..b).contains(&(i as f64))
        });
        let (zoom, center) = punch.map_or((base_zoom, base_center), |p| {
            (
                p.zoom.min(out.max_punch) * base_zoom,
                p.center.unwrap_or(base_center),
            )
        });

        let (window, place) = if self.fit == Fit::Contain {
            let w = full.zoom(base_zoom, base_center);
            let ph = ow / w.aspect();
            let y = (oh * self.fit_y - ph / 2.0).round();
            (
                w,
                Some(Rect {
                    x: 0.0,
                    y,
                    w: ow,
                    h: ph.round(),
                }),
            )
        } else {
            let ar = ow / oh;
            let aspect = if (full.aspect() / ar - 1.0).abs() > 0.01 {
                full.fit_aspect(
                    ar,
                    [
                        self.crop_x.eval(t, shot.start, duration),
                        self.crop_y.eval(t, shot.start, duration),
                    ],
                )
            } else {
                full
            };
            (aspect.zoom(zoom, center), None)
        };

        let crop = match self.push {
            Some((z0, z1)) => {
                let n = shot.frames.saturating_sub(1).max(1) as f64;
                let u = (i as f64 / n).min(1.0);
                let e = 0.5 * u + 0.5 * ease(u);
                let c = match self.pan {
                    Some((a, b)) => [a[0] + (b[0] - a[0]) * e, a[1] + (b[1] - a[1]) * e],
                    None => self.push_center.unwrap_or(base_center),
                };
                window.zoom(z0 + (z1 - z0) * e, c)
            }
            None => window,
        };
        let blur = place
            .filter(|_| self.fill_blur > 0.0)
            .map(|p| self.fill_blur * p.h);
        Frame { crop, place, blur }
    }

    /// Every output frame's geometry.
    pub fn frames(&self, shot: &Shot, out: &Profile) -> Vec<Frame> {
        (0..shot.frames.max(1))
            .map(|i| self.frame(i, shot, out))
            .collect()
    }

    /// Whether the window changes over the shot.
    pub fn is_animated(&self) -> bool {
        self.zoom.as_ref().is_some_and(Keyed::is_animated)
            || self.zoom_center.as_ref().is_some_and(Keyed::is_animated)
            || !self.punch.is_empty()
            || self.push.is_some()
            || [&self.crop_x, &self.crop_y].iter().any(|s| match s {
                Slide::Pan(..) => true,
                Slide::Keyed(k) | Slide::ClipKeys(k) => k.is_animated(),
            })
    }
}
