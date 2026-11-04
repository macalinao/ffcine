//! The `ffcine.comp/1` model: read from JSON or YAML, validated, every track compiled.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use ffcine_core::anim::{Clock, Env, Scope, Track, Vars, is_keys};
use ffcine_core::{Error, Result};
use ffcine_render::BlendMode;
use serde_json::{Map, Value};

/// The schema name (`"schema": "ffcine.comp/1"`).
pub const SCHEMA: &str = "ffcine.comp";
const VERSION: u64 = 1;

pub(crate) fn err(path: impl Into<String>, msg: impl Into<String>) -> Error {
    Error::Track {
        path: path.into(),
        msg: msg.into(),
    }
}

/// Whether `v` is a comp document (`schema` is `ffcine.comp/N`), as opposed to a timeline.
pub fn is_comp(v: &Value) -> bool {
    v.get("schema")
        .and_then(Value::as_str)
        .is_some_and(|s| s.starts_with("ffcine.comp/"))
}

/// The canvas: size, rate, duration, and the animated background and fade.
#[derive(Debug, Clone, PartialEq)]
pub struct Canvas {
    /// Px.
    pub size: [u32; 2],
    pub fps: f64,
    /// S.
    pub duration: f64,
    /// sRGB 0..1 (alpha ignored).
    pub background: Track<[f64; 4]>,
    /// 1 = the picture, 0 = `fade_colour`.
    pub fade: Track<f64>,
    pub fade_colour: Track<[f64; 4]>,
}

/// What a layer shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// A video file (relative paths are resolved against the comp file's directory).
    Video(PathBuf),
    /// A colour, `[r, g, b, a]` sRGB 0..1; the alpha multiplies the opacity.
    Solid(Track<[f64; 4]>),
}

/// How the source fills the layer's box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Fills the box, cropping the source to its aspect.
    #[default]
    Cover,
    /// The whole source inside the box, centred.
    Contain,
    /// Stretched to the box.
    Fill,
    /// Source pixels 1:1 (canvas pixels), centred, cropped to the box.
    None,
}

/// A mask's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskShape {
    Rect,
    Ellipse,
}

/// A shape mask in the layer's alpha.
#[derive(Debug, Clone, PartialEq)]
pub struct Mask {
    pub shape: MaskShape,
    /// `[x, y, w, h]`, box px.
    pub rect: Track<[f64; 4]>,
    /// Width of the soft edge, box px, centred on the shape's edge.
    pub feather: Track<f64>,
    pub invert: bool,
}

/// Uniform or per-axis scale.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Scale {
    Uniform(Track<f64>),
    Xy(Track<[f64; 2]>),
}

/// Where the box lands: `position + R·S·q` for a box point `q` (px from the anchor).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Transform {
    pub position: Track<[f64; 2]>,
    pub anchor: Track<[f64; 2]>,
    pub scale: Scale,
    pub rotation: Track<f64>,
    pub opacity: Track<f64>,
}

/// One layer.
#[derive(Debug, Clone, PartialEq)]
pub struct CompLayer {
    pub id: Option<String>,
    pub source: Source,
    /// Comp seconds: visible for `in_ <= t < out`.
    pub in_: f64,
    pub out: f64,
    /// Source seconds shown at `in_`.
    pub source_in: f64,
    /// Box, px.
    pub size: [f64; 2],
    pub fit: Fit,
    pub(crate) transform: Transform,
    pub(crate) radius: Track<f64>,
    pub mask: Option<Mask>,
    pub blend: BlendMode,
    pub(crate) fade: Track<f64>,
    pub(crate) fade_colour: Track<[f64; 4]>,
    /// The comp's vars, then the layer's.
    pub(crate) vars: Vars,
}

/// A composition: a canvas and layers, bottom first.
#[derive(Debug, Clone, PartialEq)]
pub struct Comp {
    pub canvas: Canvas,
    pub layers: Vec<CompLayer>,
    pub(crate) vars: Vars,
}

/// Rejects keys that aren't `known` (`x_*` keys are kept and ignored); `later` names keys of a
/// later step of the plan.
fn check_keys(
    m: &Map<String, Value>,
    path: &str,
    known: &[&str],
    later: &[(&str, &str)],
) -> Result<()> {
    for k in m.keys() {
        if k.starts_with("x_") || known.contains(&k.as_str()) {
            continue;
        }
        let at = format!("{path}.{k}");
        return Err(match later.iter().find(|(n, _)| n == k) {
            Some((_, what)) => err(at, format!("not supported yet ({what})")),
            None => err(at, format!("unknown key (known: {})", known.join(", "))),
        });
    }
    Ok(())
}

fn object<'a>(v: &'a Value, path: &str) -> Result<&'a Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| err(path, format!("expected an object, got {v}")))
}

/// A colour: `[r, g, b]`, `[r, g, b, a]`, a name or `#rrggbb[aa]`, keys of those, or an array
/// of four constants or expressions.
fn colour(v: &Value, scope: &Scope, path: &str) -> Result<Track<[f64; 4]>> {
    fn rgba(v: &Value) -> Value {
        match v {
            Value::Array(a) if a.len() == 3 && a.iter().all(Value::is_number) => {
                let mut a = a.clone();
                a.push(1.0.into());
                Value::Array(a)
            }
            v => v.clone(),
        }
    }
    let norm = if is_keys(v) {
        Value::Array(
            v.as_array()
                .into_iter()
                .flatten()
                .map(|k| {
                    let mut k = k.clone();
                    if let Some(x) = k.get("v").map(rgba) {
                        k["v"] = x;
                    }
                    k
                })
                .collect(),
        )
    } else {
        rgba(v)
    };
    Track::parse_in(&norm, scope, path)
}

/// `name` in `m` as a track, else `default`.
fn track<T: ffcine_core::anim::Animatable>(
    m: &Map<String, Value>,
    name: &str,
    scope: &Scope,
    path: &str,
    default: T,
) -> Result<Track<T>> {
    m.get(name).map_or(Ok(Track::Const(default)), |v| {
        Track::parse_in(v, scope, &format!("{path}.{name}"))
    })
}

fn pair(v: &Value, path: &str) -> Result<[f64; 2]> {
    match v.as_array().map(Vec::as_slice) {
        Some([a, b]) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) if a > 0.0 && b > 0.0 => Ok([a, b]),
            _ => Err(err(path, format!("expected [w, h] > 0, got {v}"))),
        },
        _ => Err(err(path, format!("expected [w, h], got {v}"))),
    }
}

/// A frame rate: a number, `"30"` or `"30000/1001"`.
fn fps(v: &Value, path: &str) -> Result<f64> {
    let r = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => match s.split_once('/') {
            Some((n, d)) => n
                .trim()
                .parse::<f64>()
                .ok()
                .zip(d.trim().parse::<f64>().ok())
                .map(|(n, d)| n / d),
            None => s.trim().parse().ok(),
        },
        _ => None,
    };
    r.filter(|r| r.is_finite() && *r > 0.0)
        .ok_or_else(|| err(path, format!("expected a frame rate, got {v}")))
}

impl Comp {
    /// Reads `NAME.comp.json` or `NAME.comp.yaml`; relative media paths resolve against its
    /// directory.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        let yaml = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
        let v: Value = if yaml {
            yaml_serde::from_str(&text)
                .map_err(|e| err(path.display().to_string(), e.to_string()))?
        } else {
            serde_json::from_str(&text)?
        };
        Self::from_value(&v, path.parent().unwrap_or(Path::new(".")))
    }

    /// Reads a comp document; `dir` resolves relative media paths.
    #[allow(clippy::too_many_lines)]
    pub fn from_value(v: &Value, dir: &Path) -> Result<Self> {
        let m = object(v, "comp")?;
        match m.get("schema").and_then(Value::as_str) {
            Some(s) if s == format!("{SCHEMA}/{VERSION}") => {}
            Some(s) if s.starts_with("ffcine.comp/") => {
                return Err(err(
                    "schema",
                    format!("unsupported version {s:?} (this ffcine reads {SCHEMA}/{VERSION})"),
                ));
            }
            other => {
                return Err(err(
                    "schema",
                    format!("expected \"{SCHEMA}/{VERSION}\", got {other:?}"),
                ));
            }
        }
        check_keys(
            m,
            "comp",
            &["schema", "canvas", "vars", "layers"],
            &[
                ("project", "compositing step 4, per-layer grade"),
                ("comps", "compositing step 3, nested comps"),
                ("template", "compositing step 7, templates"),
            ],
        )?;
        let mut vars = Vars::new();
        if let Some(v) = m.get("vars") {
            vars.push_level(object(v, "vars")?, Clock::Layer, "vars")?;
        }
        let scope = Scope::with_vars(Clock::Layer, &vars);

        let c = object(
            m.get("canvas").ok_or_else(|| err("canvas", "missing"))?,
            "canvas",
        )?;
        check_keys(
            c,
            "canvas",
            &[
                "size",
                "fps",
                "duration",
                "background",
                "fade",
                "fade_colour",
            ],
            &[
                ("grain", "compositing step 4, per-layer grade"),
                ("filter", "compositing step 6, libavfilter node"),
            ],
        )?;
        let size = pair(
            c.get("size").ok_or_else(|| err("canvas.size", "missing"))?,
            "canvas.size",
        )?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let size = size.map(|v| v.round().max(1.0) as u32);
        let fps = fps(
            c.get("fps").ok_or_else(|| err("canvas.fps", "missing"))?,
            "canvas.fps",
        )?;
        let duration = c
            .get("duration")
            .and_then(Value::as_f64)
            .filter(|d| d.is_finite() && *d > 0.0)
            .ok_or_else(|| err("canvas.duration", "expected seconds > 0"))?;
        let black = Track::Const([0.0, 0.0, 0.0, 1.0]);
        let canvas = Canvas {
            size,
            fps,
            duration,
            background: match c.get("background") {
                Some(v) => colour(v, &scope, "canvas.background")?,
                None => black.clone(),
            },
            fade: track(c, "fade", &scope, "canvas", 1.0)?,
            fade_colour: match c.get("fade_colour") {
                Some(v) => colour(v, &scope, "canvas.fade_colour")?,
                None => black.clone(),
            },
        };

        let mut comp = Self {
            canvas,
            layers: Vec::new(),
            vars,
        };
        let layers = match m.get("layers") {
            Some(Value::Array(a)) => a.as_slice(),
            Some(v) => return Err(err("layers", format!("expected an array, got {v}"))),
            None => &[],
        };
        for (i, l) in layers.iter().enumerate() {
            let layer = comp.layer(l, &format!("layers[{i}]"), dir)?;
            comp.layers.push(layer);
        }
        Ok(comp)
    }

    #[allow(clippy::too_many_lines)]
    fn layer(&self, v: &Value, path: &str, dir: &Path) -> Result<CompLayer> {
        let m = object(v, path)?;
        check_keys(
            m,
            path,
            &[
                "id",
                "source",
                "in",
                "out",
                "source_in",
                "size",
                "fit",
                "transform",
                "radius",
                "mask",
                "blend",
                "fade",
                "fade_colour",
                "vars",
            ],
            &[
                ("content", "reframe parameters inside the box"),
                ("crop", "source crop"),
                ("grade", "compositing step 4, per-layer grade"),
                ("time", "compositing step 5, time remapping"),
                ("filter", "compositing step 6, libavfilter node"),
            ],
        )?;
        let mut vars = self.vars.clone();
        if let Some(v) = m.get("vars") {
            vars.push_level(
                object(v, &format!("{path}.vars"))?,
                Clock::Layer,
                &format!("{path}.vars"),
            )?;
        }
        let scope = Scope::with_vars(Clock::Layer, &vars);
        let [cw, ch] = self.canvas.size.map(f64::from);

        // `in`, `out` and `source_in`: numbers or expressions over the vars, evaluated once.
        let env = Env {
            fps: self.canvas.fps,
            canvas: [cw, ch],
            size: [cw, ch],
            source: [cw, ch],
            in_: 0.0,
            out: self.canvas.duration,
            total: self.canvas.duration,
            source_in: 0.0,
            vars: Some(&vars),
        };
        let once = |name: &str, default: f64| -> Result<f64> {
            let Some(v) = m.get(name) else {
                return Ok(default);
            };
            let what = format!("{path}.{name}");
            let t: Track<f64> = Track::parse_in(v, &scope, &what)?;
            let x = t
                .sample(Clock::Layer, 0..1, &env)
                .map_err(|e| err(&what, e.to_string()))?[0];
            Ok(x)
        };
        let in_ = once("in", 0.0)?;
        let out = once("out", self.canvas.duration)?;
        let source_in = once("source_in", 0.0)?;
        if !(out > in_) {
            return Err(err(
                path,
                format!("`out` ({out}) must be after `in` ({in_})"),
            ));
        }
        if source_in < 0.0 {
            return Err(err(format!("{path}.source_in"), "must be ≥ 0"));
        }

        let sp = format!("{path}.source");
        let s = object(m.get("source").ok_or_else(|| err(&sp, "missing"))?, &sp)?;
        let source = match (s.len(), s.iter().next()) {
            (1, Some((k, v))) => match k.as_str() {
                "video" => Source::Video(dir.join(v.as_str().ok_or_else(|| {
                    err(format!("{sp}.video"), format!("expected a path, got {v}"))
                })?)),
                "solid" => Source::Solid(colour(v, &scope, &format!("{sp}.solid"))?),
                "still" | "text" | "comp" => {
                    return Err(err(
                        format!("{sp}.{k}"),
                        "not supported yet (compositing step 3, media layers)",
                    ));
                }
                _ => return Err(err(format!("{sp}.{k}"), "unknown source (video, solid)")),
            },
            _ => {
                return Err(err(
                    &sp,
                    "expected one of {\"video\": PATH}, {\"solid\": COLOUR}",
                ));
            }
        };

        let size = match m.get("size") {
            Some(v) => pair(v, &format!("{path}.size"))?,
            None => [cw, ch],
        };
        let fit = match m.get("fit").map(|v| (v.as_str(), v)) {
            None => Fit::Cover,
            Some((Some("cover"), _)) => Fit::Cover,
            Some((Some("contain"), _)) => Fit::Contain,
            Some((Some("fill"), _)) => Fit::Fill,
            Some((Some("none"), _)) => Fit::None,
            Some((_, v)) => {
                return Err(err(
                    format!("{path}.fit"),
                    format!("expected cover, contain, fill or none, got {v}"),
                ));
            }
        };

        let tp = format!("{path}.transform");
        let empty = Map::new();
        let t = match m.get("transform") {
            Some(v) => object(v, &tp)?,
            None => &empty,
        };
        check_keys(
            t,
            &tp,
            &["position", "anchor", "scale", "rotation", "opacity"],
            &[],
        )?;
        let scale = match t.get("scale") {
            None => Scale::Uniform(Track::Const(1.0)),
            Some(v) => {
                let uniform = v.is_number()
                    || v.is_string()
                    || (is_keys(v) && v[0]["v"].is_number())
                    || v.get("keys")
                        .is_some_and(|k| is_keys(k) && k[0]["v"].is_number());
                if uniform {
                    Scale::Uniform(Track::parse_in(v, &scope, &format!("{tp}.scale"))?)
                } else {
                    Scale::Xy(Track::parse_in(v, &scope, &format!("{tp}.scale"))?)
                }
            }
        };
        let transform = Transform {
            position: track(t, "position", &scope, &tp, [cw / 2.0, ch / 2.0])?,
            anchor: track(t, "anchor", &scope, &tp, [0.5, 0.5])?,
            scale,
            rotation: track(t, "rotation", &scope, &tp, 0.0)?,
            opacity: track(t, "opacity", &scope, &tp, 1.0)?,
        };

        let mask = match m.get("mask") {
            None => None,
            Some(v) => {
                let mp = format!("{path}.mask");
                let k = object(v, &mp)?;
                check_keys(k, &mp, &["shape", "rect", "feather", "invert"], &[])?;
                let shape = match k.get("shape").and_then(Value::as_str) {
                    Some("rect") => MaskShape::Rect,
                    Some("ellipse") => MaskShape::Ellipse,
                    _ => return Err(err(format!("{mp}.shape"), "expected rect or ellipse")),
                };
                Some(Mask {
                    shape,
                    rect: track(k, "rect", &scope, &mp, [0.0, 0.0, size[0], size[1]])?,
                    feather: track(k, "feather", &scope, &mp, 0.0)?,
                    invert: match k.get("invert") {
                        None => false,
                        Some(Value::Bool(b)) => *b,
                        Some(v) => {
                            return Err(err(
                                format!("{mp}.invert"),
                                format!("expected true or false, got {v}"),
                            ));
                        }
                    },
                })
            }
        };
        let blend = match m.get("blend") {
            None => BlendMode::Normal,
            Some(v) => v
                .as_str()
                .and_then(|s| BlendMode::from_str(s).ok())
                .ok_or_else(|| err(format!("{path}.blend"), format!("unknown blend mode {v}")))?,
        };
        Ok(CompLayer {
            id: m.get("id").and_then(Value::as_str).map(str::to_owned),
            source,
            in_,
            out,
            source_in,
            size,
            fit,
            transform,
            radius: track(m, "radius", &scope, path, 0.0)?,
            mask,
            blend,
            fade: track(m, "fade", &scope, path, 1.0)?,
            fade_colour: match m.get("fade_colour") {
                Some(v) => colour(v, &scope, &format!("{path}.fade_colour"))?,
                None => Track::Const([0.0, 0.0, 0.0, 1.0]),
            },
            vars,
        })
    }
}
