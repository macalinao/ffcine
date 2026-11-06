//! The `ffcine.comp/1` model: read from JSON or YAML, validated, every track compiled.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use ffcine_core::anim::{Clock, Env, Scope, Track, Vars, is_keys};
use ffcine_core::camera::ProfileChoice;
use ffcine_core::{Error, Result};
use ffcine_render::BlendMode;
use ffcine_render::filter_node::{self as fnode, Hw, MAX_ROLL, Node, Reach};
use ffcine_timeline::{ClipGrade, Project, Timeline};
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

/// Limits a comp is checked against (`docs/compositing.md#limits`).
pub mod limits {
    /// Canvas px a side.
    pub const CANVAS_SIDE: f64 = 16384.0;
    /// Canvas px in all.
    pub const CANVAS_PIXELS: f64 = 8192.0 * 8192.0;
    /// Long side over short side, canvas and render.
    pub const ASPECT: f64 = 4.0;
    /// Frames per second, at least and at most.
    pub const FPS: (f64, f64) = (1.0, 240.0);
    /// Comp seconds (and layer `in`/`out`, `source_in`).
    pub const DURATION: f64 = 3600.0;
    /// Layers in a comp.
    pub const LAYERS: usize = 64;
    /// Video layers in a comp (each one a decoder while it shows).
    pub const VIDEO_LAYERS: usize = 16;
    /// A layer's box, px a side.
    pub const BOX_SIDE: f64 = 65536.0;
    /// A layer's texture (its picture at its largest scale, render px), px a side.
    pub const TEXTURE_SIDE: f64 = 16384.0;
    /// Position and anchor values, either way (px, box fractions).
    pub const COORD: f64 = 1e9;
    /// Rotation, degrees either way.
    pub const ROTATION: f64 = 1e6;
    /// A time remap's `speed`, either way (source s per layer s).
    pub const SPEED: f64 = 100.0;
    /// Comps nested in comps (`{"comp": ...}` sources), levels below the top.
    pub const DEPTH: usize = 4;
    /// `content` zooms (`zoom`, a punch's, `push`), and how far in they and `crop` may go in
    /// all: the source window is at least 1/ZOOM of the source a side.
    pub const ZOOM: f64 = 1e4;
    /// A still source, px a side.
    pub const STILL_SIDE: u32 = ffcine_render::STILL_SIDE;
    /// A still source, px in all.
    pub const STILL_PIXELS: u64 = ffcine_render::STILL_PIXELS;
}

/// Whether `v` is a comp document (`schema` is `ffcine.comp/N`), as opposed to a timeline.
pub fn is_comp(v: &Value) -> bool {
    v.get("schema")
        .and_then(Value::as_str)
        .is_some_and(|s| s.starts_with("ffcine.comp/"))
}

/// Whether the file at `path` is a comp document (JSON, or YAML by extension); `false` for
/// timelines and unreadable files.
pub fn is_comp_file(path: &Path) -> bool {
    // By name first, so a comp with a syntax error reports it as a comp.
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    [".comp.json", ".comp.yaml", ".comp.yml"]
        .iter()
        .any(|e| name.ends_with(e))
        || read(path).is_ok_and(|v| is_comp(&v))
}

/// A JSON or (by extension) YAML document.
fn read(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let yaml = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
    if yaml {
        yaml_serde::from_str(&text).map_err(|e| err(path.display().to_string(), e.to_string()))
    } else {
        Ok(serde_json::from_str(&text)?)
    }
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
    /// A still picture (PNG, JPEG, WebP, HEIC, ...), decoded once through libav.
    Still(PathBuf),
    /// A colour, `[r, g, b, a]` sRGB 0..1; the alpha multiplies the opacity.
    Solid(Track<[f64; 4]>),
    /// `ffcine.text` parameters (`docs/text.md`); font files resolve against `dir`.
    Text {
        params: Map<String, Value>,
        dir: PathBuf,
    },
    /// A nested comp, its canvas defaulted from the layer and its parent.
    Comp(Box<Comp>),
}

/// `content`: the picture inside the box (`ffcine.geo.reframe/1` parameters, the box as the
/// profile).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Content {
    pub zoom: Track<f64>,
    pub zoom_center: Track<[f64; 2]>,
    /// Window position on sources wider (`crop_x`) or taller (`crop_y`) than the box: 0
    /// left/top, 1 right/bottom.
    pub crop_x: Track<f64>,
    pub crop_y: Track<f64>,
    pub punch: Vec<Punch>,
    /// `[from, to]` zoom eased over the layer (half linear, half cosine, as reframe's).
    pub push: Option<[f64; 2]>,
    pub push_center: Option<[f64; 2]>,
    pub pan: Option<[[f64; 2]; 2]>,
}

/// A punch-in: `zoom` times the base zoom from `at` to `to` (layer seconds).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Punch {
    pub at: f64,
    pub to: Option<f64>,
    pub zoom: f64,
    pub center: Option<[f64; 2]>,
}

/// `crop`: fractions of the source cut from each side, and the soft edge (a fraction of the
/// source width), as `ffcine.geo.crop/1`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CropSpec {
    pub left: Track<f64>,
    pub right: Track<f64>,
    pub top: Track<f64>,
    pub bottom: Track<f64>,
    pub feather: Track<f64>,
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
    pub(crate) content: Option<Content>,
    pub(crate) crop: Option<CropSpec>,
    pub(crate) transform: Transform,
    pub(crate) radius: Track<f64>,
    pub mask: Option<Mask>,
    pub blend: BlendMode,
    pub(crate) fade: Track<f64>,
    pub(crate) fade_colour: Track<[f64; 4]>,
    /// `grade` (video, still and comp layers).
    pub(crate) grade: Option<LayerGrade>,
    /// `time` (video layers).
    pub(crate) time: Option<crate::time::Remap>,
    /// `filter`, the libavfilter escape hatch (video layers), and its resolved reach.
    pub(crate) filter: Option<(Node, Reach)>,
    /// The comp's vars, then the layer's.
    pub(crate) vars: Vars,
}

/// The look params a layer's `grade` may animate (the look hook's `//!PARAM`s).
pub(crate) const LOOK_PARAMS: [&str; 8] = [
    "exposure",
    "warmth",
    "tint",
    "contrast",
    "gamma",
    "lift",
    "gain",
    "saturation",
];

/// A layer's `grade` (`docs/compositing.md#per-layer-grade-and-filters`): resolved at lowering
/// through the comp's project like a timeline clip's `ffcine.color.grade/1`.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct LayerGrade {
    /// The constant fields, as a clip's grade effect.
    pub params: ClipGrade,
    /// Animated look params ([`LOOK_PARAMS`]), set over the resolved grade per frame.
    pub tracks: Vec<(&'static str, Track<f64>)>,
    /// `filters`, after the resolved grade's `extra`.
    pub filters: Vec<FilterSpec>,
    /// `shaders`, after the filters.
    pub shaders: Vec<ShaderSpec>,
}

/// A `grade.filters` entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FilterSpec {
    /// An ffmpeg filter chain (`ffcine_render::extra`), every filter and option supported.
    Chain(String),
    /// `{"name": {option: number, track or string}}`.
    Object {
        name: String,
        opts: Vec<(String, FilterOpt)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FilterOpt {
    Num(Track<f64>),
    Text(String),
}

/// A `grade.shaders` entry, as `ffcine.fx.shader/1`: `//!PARAM`s as layer-clock tracks.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ShaderSpec {
    pub shader: String,
    pub params: Vec<(String, Track<f64>)>,
}

/// The project a comp's grades resolve through: `project` (inline, or a timeline's), else the
/// default one.
#[derive(Debug, Clone, PartialEq)]
pub struct CompProject {
    pub project: Project,
    /// LUT and shader paths resolve against it: the timeline's directory for a borrowed
    /// project, else the comp file's.
    pub dir: PathBuf,
    /// Whether `project` was given: then every video, still and comp layer is graded.
    pub given: bool,
}

/// A composition: a canvas and layers, bottom first.
#[derive(Debug, Clone, PartialEq)]
pub struct Comp {
    pub canvas: Canvas,
    pub layers: Vec<CompLayer>,
    pub(crate) vars: Vars,
    /// Shared with nested comps.
    pub project: Arc<CompProject>,
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

/// Appended to a per-layer `filters` entry the GPU path can't run.
pub(crate) const FILTER_HINT: &str =
    "(other libavfilter graphs run through the layer's `filter`, the escape hatch)";

/// A layer's `filter` (`docs/compositing.md#libavfilter-escape-hatch`) and its reach.
fn filter_node(v: &Value, path: &str, dir: &Path) -> Result<(Node, Reach)> {
    let m = object(v, path)?;
    check_keys(
        m,
        path,
        &[
            "graph",
            "hw",
            "clock",
            "preroll",
            "postroll",
            "whole_clip",
            "timing",
        ],
        &[],
    )?;
    let at = |k: &str| format!("{path}.{k}");
    let word = |k: &str| -> Result<Option<&str>> {
        m.get(k)
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| err(at(k), format!("expected a string, got {v}")))
            })
            .transpose()
    };
    let graph = word("graph")?
        .filter(|g| !g.trim().is_empty())
        .ok_or_else(|| err(at("graph"), "missing (a libavfilter graph)"))?;
    let hw = match word("hw")? {
        None => Hw::Cpu,
        Some(s) => {
            Hw::parse(s).ok_or_else(|| err(at("hw"), format!("{s:?}: cpu, cuda or vulkan")))?
        }
    };
    let clock = match word("clock")? {
        None => fnode::Clock::Layer,
        Some(s) => fnode::Clock::parse(s)
            .ok_or_else(|| err(at("clock"), format!("{s:?}: layer or comp")))?,
    };
    let free_timing = match word("timing")? {
        None | Some("keep") => false,
        Some("free") => true,
        Some(s) => return Err(err(at("timing"), format!("{s:?}: keep or free"))),
    };
    let roll = |k: &str| -> Result<Option<u32>> {
        m.get(k)
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n <= MAX_ROLL)
                    .ok_or_else(|| err(at(k), format!("{v}: frames, 0 to {MAX_ROLL}")))
            })
            .transpose()
    };
    let whole_clip = m
        .get("whole_clip")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| err(at("whole_clip"), format!("expected true or false, got {v}")))
        })
        .transpose()?;
    let mut node = Node {
        graph: graph.to_owned(),
        hw,
        clock,
        preroll: roll("preroll")?,
        postroll: roll("postroll")?,
        whole_clip,
        free_timing,
        files: Vec::new(),
        run: graph.to_owned(),
    };
    let reach = node.resolve().map_err(|e| err(at("graph"), e))?;
    node.locate(dir).map_err(|e| err(at("graph"), e))?;
    Ok((node, reach))
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
    let (lo, hi) = limits::FPS;
    r.filter(|r| r.is_finite() && (lo..=hi).contains(r))
        .ok_or_else(|| err(path, format!("expected a frame rate {lo} to {hi}, got {v}")))
}

/// What a comp is parsed in: the top file's directory, its `comps`, and how deep it is.
struct Ctx<'a> {
    dir: &'a Path,
    comps: &'a Map<String, Value>,
    depth: usize,
    project: &'a Arc<CompProject>,
}

/// A nested comp's defaults, from the layer that uses it.
struct Parent<'a> {
    vars: &'a Vars,
    fps: f64,
    duration: f64,
    size: [f64; 2],
}

/// A `[x, y]` of numbers.
fn xy(v: &Value, path: &str) -> Result<[f64; 2]> {
    match v.as_array().map(Vec::as_slice) {
        Some([a, b]) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) if a.is_finite() && b.is_finite() => Ok([a, b]),
            _ => Err(err(path, format!("expected [x, y], got {v}"))),
        },
        _ => Err(err(path, format!("expected [x, y], got {v}"))),
    }
}

/// `content`: reframe parameters, the layer's box as the profile.
fn content(v: &Value, scope: &Scope, path: &str) -> Result<Content> {
    let m = object(v, path)?;
    check_keys(
        m,
        path,
        &[
            "zoom",
            "zoom_center",
            "crop_x",
            "crop_y",
            "focus",
            "punch",
            "push",
            "push_center",
            "pan",
        ],
        &[],
    )
    .map_err(|e| match e {
        Error::Track { path, msg } if msg.starts_with("unknown key") => Error::Track {
            msg: format!("{msg}; the layer's `fit` places the picture, not content's"),
            path,
        },
        e => e,
    })?;
    let at = |k: &str| format!("{path}.{k}");
    let focus = m.get("focus").map(|v| xy(v, &at("focus"))).transpose()?;
    let [fx, fy] = focus.unwrap_or([0.5, 0.5]);
    let punch = match m.get("punch") {
        None => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let pp = format!("{path}.punch[{i}]");
                let o = object(p, &pp)?;
                check_keys(o, &pp, &["at", "to", "zoom", "center"], &[])?;
                let num = |k: &str| o.get(k).and_then(Value::as_f64);
                let punch = Punch {
                    at: num("at").ok_or_else(|| err(format!("{pp}.at"), "expected seconds"))?,
                    to: num("to"),
                    zoom: num("zoom").unwrap_or(1.25),
                    center: o
                        .get("center")
                        .map(|v| xy(v, &format!("{pp}.center")))
                        .transpose()?,
                };
                if !(punch.zoom.is_finite() && punch.zoom > 0.0 && punch.zoom <= limits::ZOOM) {
                    return Err(err(
                        format!("{pp}.zoom"),
                        format!("expected a number > 0, at most {}", limits::ZOOM),
                    ));
                }
                Ok(punch)
            })
            .collect::<Result<_>>()?,
        Some(v) => return Err(err(at("punch"), format!("expected an array, got {v}"))),
    };
    let pan = match m.get("pan") {
        None => None,
        Some(v) => match v.as_array().map(Vec::as_slice) {
            Some([a, b]) => Some([xy(a, &at("pan"))?, xy(b, &at("pan"))?]),
            _ => {
                return Err(err(
                    at("pan"),
                    format!("expected [[x, y], [x, y]], got {v}"),
                ));
            }
        },
    };
    let push = m.get("push").map(|v| xy(v, &at("push"))).transpose()?;
    if push.is_some_and(|p| p.iter().any(|z| !(*z > 0.0 && *z <= limits::ZOOM))) {
        return Err(err(
            at("push"),
            format!("zooms must be > 0, at most {}", limits::ZOOM),
        ));
    }
    Ok(Content {
        zoom: track(m, "zoom", scope, path, 1.0)?,
        zoom_center: track(m, "zoom_center", scope, path, [fx, fy])?,
        crop_x: track(m, "crop_x", scope, path, fx)?,
        crop_y: track(m, "crop_y", scope, path, fy)?,
        punch,
        push,
        push_center: m
            .get("push_center")
            .map(|v| xy(v, &at("push_center")))
            .transpose()?,
        pan,
    })
}

/// `crop`: fractions of the source.
fn crop(v: &Value, scope: &Scope, path: &str) -> Result<CropSpec> {
    let m = object(v, path)?;
    check_keys(m, path, &["left", "right", "top", "bottom", "feather"], &[])?;
    Ok(CropSpec {
        left: track(m, "left", scope, path, 0.0)?,
        right: track(m, "right", scope, path, 0.0)?,
        top: track(m, "top", scope, path, 0.0)?,
        bottom: track(m, "bottom", scope, path, 0.0)?,
        feather: track(m, "feather", scope, path, 0.0)?,
    })
}

impl CompLayer {
    /// Layers and video layers this one stands for (itself and a nested comp's).
    fn count(&self) -> (usize, usize) {
        match &self.source {
            Source::Video(_) => (1, 1),
            Source::Comp(c) => {
                let (n, v) = c.count();
                (n + 1, v)
            }
            _ => (1, 0),
        }
    }
}

impl Comp {
    /// Reads `NAME.comp.json` or `NAME.comp.yaml`; relative media paths resolve against its
    /// directory.
    pub fn from_file(path: &Path) -> Result<Self> {
        let v = read(path)?;
        Self::from_value(&v, path.parent().unwrap_or(Path::new(".")))
    }

    /// Reads a comp document; `dir` resolves relative media paths.
    pub fn from_value(v: &Value, dir: &Path) -> Result<Self> {
        let m = object(v, "comp")?;
        Self::schema(m, "schema", false)?;
        check_keys(
            m,
            "comp",
            &["schema", "canvas", "vars", "comps", "layers", "project"],
            &[("template", "compositing step 7, templates")],
        )?;
        let project = Arc::new(match m.get("project") {
            None => CompProject {
                project: Project::default(),
                dir: dir.to_owned(),
                given: false,
            },
            Some(v) => comp_project(v, dir)?,
        });
        let empty = Map::new();
        let comps = match m.get("comps") {
            Some(v) => object(v, "comps")?,
            None => &empty,
        };
        let ctx = Ctx {
            dir,
            comps,
            depth: 0,
            project: &project,
        };
        Self::parse(m, "", &ctx, None)
    }

    fn schema(m: &Map<String, Value>, path: &str, optional: bool) -> Result<()> {
        match m.get("schema").and_then(Value::as_str) {
            Some(s) if s == format!("{SCHEMA}/{VERSION}") => {}
            Some(s) if s.starts_with("ffcine.comp/") => {
                return Err(err(
                    "schema",
                    format!("unsupported version {s:?} (this ffcine reads {SCHEMA}/{VERSION})"),
                ));
            }
            None if optional => {}
            other => {
                return Err(err(
                    path,
                    format!("expected \"{SCHEMA}/{VERSION}\", got {other:?}"),
                ));
            }
        }
        Ok(())
    }

    /// A comp's canvas, vars and layers; `parent` is `None` at the top, else the using layer's
    /// box, vars and its comp's rate and duration (`p` prefixes error paths).
    #[allow(
        clippy::too_many_lines,
        clippy::many_single_char_names,
        clippy::needless_pass_by_value
    )]
    fn parse(
        m: &Map<String, Value>,
        p: &str,
        ctx: &Ctx<'_>,
        parent: Option<Parent<'_>>,
    ) -> Result<Self> {
        let at = |k: &str| {
            if p.is_empty() {
                k.to_owned()
            } else {
                format!("{p}.{k}")
            }
        };
        let mut vars = parent.as_ref().map_or_else(Vars::new, |p| p.vars.clone());
        if let Some(v) = m.get("vars") {
            vars.push_level(object(v, &at("vars"))?, Clock::Layer, &at("vars"))?;
        }
        let scope = Scope::with_vars(Clock::Layer, &vars);

        let empty = Map::new();
        let c = match (m.get("canvas"), &parent) {
            (Some(v), _) => object(v, &at("canvas"))?,
            (None, Some(_)) => &empty,
            (None, None) => return Err(err("canvas", "missing")),
        };
        let cp = at("canvas");
        let ca = |k: &str| format!("{cp}.{k}");
        check_keys(
            c,
            &cp,
            &[
                "size",
                "fps",
                "duration",
                "background",
                "fade",
                "fade_colour",
            ],
            &[
                ("grain", "canvas grain, see docs/compositing.md#not-yet"),
                ("filter", "canvas.filter, see docs/compositing.md#not-yet"),
            ],
        )?;
        let size = match (c.get("size"), &parent) {
            (Some(v), _) => pair(v, &ca("size"))?,
            (None, Some(p)) => p.size,
            (None, None) => return Err(err("canvas.size", "missing")),
        };
        let [w, h] = size.map(|v| v.round().max(1.0));
        if w.max(h) > limits::CANVAS_SIDE || w * h > limits::CANVAS_PIXELS {
            return Err(err(
                ca("size"),
                format!(
                    "{w}x{h}: at most {} px a side and {} px in all",
                    limits::CANVAS_SIDE,
                    limits::CANVAS_PIXELS
                ),
            ));
        }
        if w.max(h) / w.min(h) > limits::ASPECT {
            return Err(err(
                ca("size"),
                format!(
                    "{w}x{h}: the long side is at most {} times the short",
                    limits::ASPECT
                ),
            ));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let size = size.map(|v| v.round().max(1.0) as u32);
        let fps = match (c.get("fps"), &parent) {
            (Some(v), Some(p)) => {
                let f = fps(v, &ca("fps"))?;
                if (f - p.fps).abs() > 1e-9 {
                    return Err(err(
                        ca("fps"),
                        format!(
                            "{f}: a nested comp runs at its parent's rate ({}); time remapping \
                             of nested comps is not supported yet",
                            p.fps
                        ),
                    ));
                }
                f
            }
            (Some(v), None) => fps(v, &ca("fps"))?,
            (None, Some(p)) => p.fps,
            (None, None) => return Err(err("canvas.fps", "missing")),
        };
        let duration = match (c.get("duration"), &parent) {
            (None, Some(p)) => Some(p.duration),
            (d, _) => d.and_then(Value::as_f64),
        }
        .filter(|d| d.is_finite() && *d > 0.0 && *d <= limits::DURATION)
        .ok_or_else(|| {
            err(
                ca("duration"),
                format!("expected seconds > 0, at most {}", limits::DURATION),
            )
        })?;
        let black = Track::Const([0.0, 0.0, 0.0, 1.0]);
        let canvas = Canvas {
            size,
            fps,
            duration,
            background: match c.get("background") {
                Some(v) => colour(v, &scope, &ca("background"))?,
                None => black.clone(),
            },
            fade: track(c, "fade", &scope, &cp, 1.0)?,
            fade_colour: match c.get("fade_colour") {
                Some(v) => colour(v, &scope, &ca("fade_colour"))?,
                None => black.clone(),
            },
        };

        let mut comp = Self {
            canvas,
            layers: Vec::new(),
            vars,
            project: Arc::clone(ctx.project),
        };
        let layers = match m.get("layers") {
            Some(Value::Array(a)) => a.as_slice(),
            Some(v) => return Err(err(at("layers"), format!("expected an array, got {v}"))),
            None => &[],
        };
        if layers.len() > limits::LAYERS {
            return Err(err(
                at("layers"),
                format!("{} layers: at most {}", layers.len(), limits::LAYERS),
            ));
        }
        let (mut count, mut videos) = (0, 0);
        for (i, l) in layers.iter().enumerate() {
            let path = at(&format!("layers[{i}]"));
            let layer = comp.layer(l, &path, ctx)?;
            if let Some(id) = &layer.id
                && let Some(j) = comp.layers.iter().position(|o| o.id.as_ref() == Some(id))
            {
                return Err(err(
                    format!("{path}.id"),
                    format!("{id:?} is already layers[{j}]'s id"),
                ));
            }
            let (n, v) = layer.count();
            count += n;
            videos += v;
            if count > limits::LAYERS {
                return Err(err(
                    format!("{path}.source"),
                    format!(
                        "at most {} layers in all (nested comps' included)",
                        limits::LAYERS
                    ),
                ));
            }
            if videos > limits::VIDEO_LAYERS {
                return Err(err(
                    format!("{path}.source"),
                    format!(
                        "at most {} video layers (nested comps' included)",
                        limits::VIDEO_LAYERS
                    ),
                ));
            }
            comp.layers.push(layer);
        }
        Ok(comp)
    }

    /// Layers and video layers, nested comps' included.
    fn count(&self) -> (usize, usize) {
        self.layers.iter().fold((0, 0), |(n, v), l| {
            let (a, b) = l.count();
            (n + a, v + b)
        })
    }

    #[allow(clippy::too_many_lines, clippy::many_single_char_names)]
    fn layer(&self, v: &Value, path: &str, ctx: &Ctx<'_>) -> Result<CompLayer> {
        let dir = ctx.dir;
        let m = object(v, path)?;
        let text = m
            .get("source")
            .and_then(Value::as_object)
            .is_some_and(|s| s.contains_key("text"));
        let known: &[&str] = if text {
            // A text layer places itself: its parameters are the text clip's.
            &["id", "source", "in", "out", "vars"]
        } else {
            &[
                "id",
                "source",
                "in",
                "out",
                "source_in",
                "size",
                "fit",
                "content",
                "crop",
                "transform",
                "radius",
                "mask",
                "blend",
                "fade",
                "fade_colour",
                "grade",
                "time",
                "filter",
                "vars",
            ]
        };
        check_keys(m, path, known, &[])?;
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
        if out.partial_cmp(&in_) != Some(std::cmp::Ordering::Greater) {
            return Err(err(
                path,
                format!("`out` ({out}) must be after `in` ({in_})"),
            ));
        }
        for (name, v) in [("in", in_), ("out", out), ("source_in", source_in)] {
            if !(0.0..=limits::DURATION).contains(&v) {
                return Err(err(
                    format!("{path}.{name}"),
                    format!("{v}: must be 0 to {} s", limits::DURATION),
                ));
            }
        }

        let size = match m.get("size") {
            Some(v) => pair(v, &format!("{path}.size"))?,
            None => [cw, ch],
        };
        if size.iter().any(|v| *v > limits::BOX_SIDE) {
            return Err(err(
                format!("{path}.size"),
                format!("{size:?}: at most {} px a side", limits::BOX_SIDE),
            ));
        }

        let sp = format!("{path}.source");
        let s = object(m.get("source").ok_or_else(|| err(&sp, "missing"))?, &sp)?;
        let file = |k: &str, v: &Value| -> Result<PathBuf> {
            Ok(dir
                .join(v.as_str().ok_or_else(|| {
                    err(format!("{sp}.{k}"), format!("expected a path, got {v}"))
                })?))
        };
        let source = match (s.len(), s.iter().next()) {
            (1, Some((k, v))) => match k.as_str() {
                "video" => Source::Video(file(k, v)?),
                "still" => Source::Still(file(k, v)?),
                "solid" => Source::Solid(colour(v, &scope, &format!("{sp}.solid"))?),
                "text" => {
                    let params = object(v, &format!("{sp}.text"))?;
                    if !params.get("text").is_some_and(Value::is_string) {
                        return Err(err(format!("{sp}.text.text"), "expected a string"));
                    }
                    Source::Text {
                        params: params.clone(),
                        dir: dir.to_owned(),
                    }
                }
                "comp" => {
                    let cp = format!("{sp}.comp");
                    if ctx.depth >= limits::DEPTH {
                        return Err(err(
                            &cp,
                            format!("comps nest at most {} levels deep", limits::DEPTH),
                        ));
                    }
                    let (body, bp) = match v {
                        Value::String(name) => (
                            ctx.comps.get(name).ok_or_else(|| {
                                err(&cp, format!("no comp {name:?} in the top-level `comps`"))
                            })?,
                            format!("comps.{name}"),
                        ),
                        v => (v, cp.clone()),
                    };
                    let b = object(body, &bp)?;
                    Self::schema(b, &format!("{bp}.schema"), true)?;
                    check_keys(
                        b,
                        &bp,
                        &["schema", "canvas", "vars", "layers"],
                        &[
                            ("comps", "comps are named at the top level only"),
                            ("project", "a nested comp uses its parent's project"),
                            ("template", "compositing step 7, templates"),
                        ],
                    )?;
                    let sub = Ctx {
                        dir,
                        comps: ctx.comps,
                        depth: ctx.depth + 1,
                        project: ctx.project,
                    };
                    let parent = Parent {
                        vars: &vars,
                        fps: self.canvas.fps,
                        duration: self.canvas.duration,
                        size,
                    };
                    Source::Comp(Box::new(Self::parse(b, &bp, &sub, Some(parent))?))
                }
                _ => {
                    return Err(err(
                        format!("{sp}.{k}"),
                        "unknown source (video, still, solid, text, comp)",
                    ));
                }
            },
            _ => {
                return Err(err(
                    &sp,
                    "expected one of {\"video\": PATH}, {\"still\": PATH}, {\"solid\": \
                     COLOUR}, {\"text\": {...}}, {\"comp\": NAME or {...}}",
                ));
            }
        };
        let fit = match m.get("fit").map(|v| (v.as_str(), v)) {
            None | Some((Some("cover"), _)) => Fit::Cover,
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
        let picture = matches!(
            source,
            Source::Video(_) | Source::Still(_) | Source::Comp(_)
        );
        for k in ["content", "crop"] {
            if !picture && m.contains_key(k) {
                return Err(err(
                    format!("{path}.{k}"),
                    "only video, still and comp layers have a picture to frame",
                ));
            }
        }
        let content = m
            .get("content")
            .map(|v| content(v, &scope, &format!("{path}.content")))
            .transpose()?;
        let crop = m
            .get("crop")
            .map(|v| crop(v, &scope, &format!("{path}.crop")))
            .transpose()?;
        if let Some(c) = &crop {
            let animated = [&c.left, &c.right, &c.top, &c.bottom]
                .iter()
                .any(|t| !matches!(t, Track::Const(_)));
            if animated && matches!(fit, Fit::Contain | Fit::None) {
                return Err(err(
                    format!("{path}.crop"),
                    "an animated crop needs fit cover or fill (contain and none would resize the box's picture)",
                ));
            }
            if m.contains_key("mask") && !matches!(c.feather, Track::Const(f) if f == 0.0) {
                return Err(err(
                    format!("{path}.crop.feather"),
                    "a feathered crop and a mask share the layer's alpha: use one",
                ));
            }
        }

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
        let grade = match m.get("grade") {
            None => None,
            Some(_) if !picture => {
                return Err(err(
                    format!("{path}.grade"),
                    "only video, still and comp layers have a picture to grade",
                ));
            }
            Some(v) => {
                let gp = format!("{path}.grade");
                let g = layer_grade(v, &scope, &gp)?;
                if !matches!(source, Source::Video(_))
                    && let Some(c) = &g.params.params.camera
                    && c != "auto"
                {
                    return Err(err(
                        format!("{gp}.camera"),
                        "a camera profile applies to video layers only (stills and comps are \
                         display-referred)",
                    ));
                }
                Some(g)
            }
        };
        let time = match m.get("time") {
            None => None,
            Some(_) if !matches!(source, Source::Video(_)) => {
                return Err(err(
                    format!("{path}.time"),
                    if matches!(source, Source::Comp(_)) {
                        "time remapping of nested comps is not supported yet"
                    } else {
                        "only video layers have a source time to remap"
                    },
                ));
            }
            Some(v) => Some(crate::time::parse(v, &scope, &format!("{path}.time"))?),
        };
        let filter = match m.get("filter") {
            None => None,
            Some(_) if !matches!(source, Source::Video(_)) => {
                return Err(err(
                    format!("{path}.filter"),
                    "only video layers take a filter graph for now (stills and nested comps: \
                     see docs/compositing.md#not-yet)",
                ));
            }
            Some(v) => Some(filter_node(v, &format!("{path}.filter"), dir)?),
        };
        let blend = match m.get("blend") {
            None => BlendMode::Normal,
            Some(v) => v
                .as_str()
                .and_then(|s| BlendMode::from_str(s).ok())
                .ok_or_else(|| err(format!("{path}.blend"), format!("unknown blend mode {v}")))?,
        };
        Ok(CompLayer {
            id: match m.get("id") {
                None => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(v) => {
                    return Err(err(
                        format!("{path}.id"),
                        format!("expected a string, got {v}"),
                    ));
                }
            },
            source,
            in_,
            out,
            source_in,
            size,
            fit,
            content,
            crop,
            transform,
            radius: track(m, "radius", &scope, path, 0.0)?,
            mask,
            blend,
            fade: track(m, "fade", &scope, path, 1.0)?,
            fade_colour: match m.get("fade_colour") {
                Some(v) => colour(v, &scope, &format!("{path}.fade_colour"))?,
                None => Track::Const([0.0, 0.0, 0.0, 1.0]),
            },
            grade,
            time,
            filter,
            vars,
        })
    }
}

/// `project`: an inline `ffcine.project/1`, or a timeline file (relative to `dir`) whose project
/// is borrowed.
fn comp_project(v: &Value, dir: &Path) -> Result<CompProject> {
    match v {
        Value::String(f) => {
            let path = dir.join(f);
            let t = Timeline::from_file(&path).map_err(|e| err("project", e.to_string()))?;
            let project = t
                .project()
                .ok_or_else(|| err("project", format!("{f}: the timeline has no project")))?
                .map_err(|e| err("project", format!("{f}: {e}")))?;
            Ok(CompProject {
                project,
                dir: path.parent().unwrap_or(Path::new(".")).to_owned(),
                given: true,
            })
        }
        Value::Object(m) => {
            match m.get("schema").and_then(Value::as_str) {
                None | Some(ffcine_timeline::project::PROJECT_SCHEMA) => {}
                Some(s) => {
                    return Err(err(
                        "project.schema",
                        format!(
                            "expected {:?}, got {s:?}",
                            ffcine_timeline::project::PROJECT_SCHEMA
                        ),
                    ));
                }
            }
            let mut m = m.clone();
            m.entry("schema")
                .or_insert_with(|| ffcine_timeline::project::PROJECT_SCHEMA.into());
            let project: Project = serde_json::from_value(Value::Object(m))
                .map_err(|e| err("project", e.to_string()))?;
            Ok(CompProject {
                project,
                dir: dir.to_owned(),
                given: true,
            })
        }
        v => Err(err(
            "project",
            format!("expected an ffcine.project/1 object or a timeline path, got {v}"),
        )),
    }
}

/// A layer's `grade` object.
#[allow(clippy::many_single_char_names, clippy::too_many_lines)]
fn layer_grade(v: &Value, scope: &Scope, path: &str) -> Result<LayerGrade> {
    let m = object(v, path)?;
    let mut known = vec![
        "look",
        "camera",
        "lut",
        "lut_strength",
        "rgb_gain",
        "sharpen",
        "filters",
        "shaders",
    ];
    known.extend(LOOK_PARAMS);
    check_keys(
        m,
        path,
        &known,
        &[
            ("cdl", "per-layer CDL, see docs/compositing.md#not-yet"),
            (
                "denoise",
                "per-layer denoise, see docs/compositing.md#not-yet",
            ),
            (
                "stabilize",
                "per-layer stabilize, see docs/compositing.md#not-yet",
            ),
            ("grain", "per-layer grain, see docs/compositing.md#not-yet"),
            ("extra", "use `filters`"),
        ],
    )?;
    let at = |k: &str| format!("{path}.{k}");
    let mut consts = Map::new();
    let mut tracks = Vec::new();
    for k in LOOK_PARAMS {
        if let Some(v) = m.get(k) {
            match Track::<f64>::parse_in(v, scope, &at(k))? {
                Track::Const(c) => {
                    consts.insert(k.into(), c.into());
                }
                t => tracks.push((k, t)),
            }
        }
    }
    for k in ["look", "camera", "lut"] {
        if let Some(v) = m.get(k) {
            let s = v
                .as_str()
                .ok_or_else(|| err(at(k), format!("expected a string, got {v}")))?;
            if k == "camera" {
                ProfileChoice::from_str(s).map_err(|e| err(at(k), e.to_string()))?;
            }
            consts.insert(k.into(), s.into());
        }
    }
    if let Some(v) = m.get("lut_strength") {
        let x = match v.as_f64() {
            Some(x) if (0.0..=1.0).contains(&x) => x,
            Some(x) => return Err(err(at("lut_strength"), format!("{x}: expected 0 to 1"))),
            None => {
                return Err(err(
                    at("lut_strength"),
                    "expected a number 0 to 1 (an animated lut_strength is not supported yet, \
                     see docs/compositing.md#not-yet)",
                ));
            }
        };
        consts.insert("lut_strength".into(), x.into());
    }
    if let Some(v) = m.get("sharpen") {
        match v.as_f64() {
            Some(x) if x.is_finite() && x >= 0.0 => {
                consts.insert("sharpen".into(), x.into());
            }
            _ => {
                return Err(err(
                    at("sharpen"),
                    format!("expected a number >= 0 (held per layer), got {v}"),
                ));
            }
        }
    }
    if let Some(v) = m.get("rgb_gain") {
        match v.as_array().map(Vec::as_slice) {
            Some([r, g, b])
                if [r, g, b]
                    .iter()
                    .all(|c| c.as_f64().is_some_and(|c| c.is_finite() && c >= 0.0)) =>
            {
                consts.insert("rgb_gain".into(), v.clone());
            }
            _ => {
                return Err(err(
                    at("rgb_gain"),
                    format!("expected [r, g, b], numbers >= 0 (held per layer), got {v}"),
                ));
            }
        }
    }
    let params: ClipGrade =
        serde_json::from_value(Value::Object(consts)).map_err(|e| err(path, e.to_string()))?;

    let list = |k: &str| -> Result<&[Value]> {
        match m.get(k) {
            None => Ok(&[]),
            Some(Value::Array(a)) => Ok(a.as_slice()),
            Some(v) => Err(err(at(k), format!("expected an array, got {v}"))),
        }
    };
    let mut filters = Vec::new();
    for (i, f) in list("filters")?.iter().enumerate() {
        let fp = format!("{path}.filters[{i}]");
        filters.push(match f {
            Value::String(chain) => {
                ffcine_render::extra::strict_hooks(std::slice::from_ref(chain))
                    .map_err(|e| err(&fp, format!("{e} {FILTER_HINT}")))?;
                FilterSpec::Chain(chain.clone())
            }
            Value::Object(o) if o.len() == 1 => {
                let (name, opts) = o.iter().next().unwrap_or_else(|| unreachable!());
                let op = format!("{fp}.{name}");
                let mut out = Vec::new();
                for (k, v) in object(opts, &op)? {
                    out.push((
                        k.clone(),
                        match v {
                            Value::String(s) => FilterOpt::Text(s.clone()),
                            v => FilterOpt::Num(Track::parse_in(v, scope, &format!("{op}.{k}"))?),
                        },
                    ));
                }
                FilterSpec::Object {
                    name: name.clone(),
                    opts: out,
                }
            }
            v => {
                return Err(err(
                    &fp,
                    format!(
                        "expected a filter chain string or {{\"filter\": {{option: value}}}}, got {v}"
                    ),
                ));
            }
        });
    }
    let mut shaders = Vec::new();
    for (i, v) in list("shaders")?.iter().enumerate() {
        let sp = format!("{path}.shaders[{i}]");
        let o = object(v, &sp)?;
        check_keys(o, &sp, &["shader", "params"], &[])?;
        let shader = o
            .get("shader")
            .and_then(Value::as_str)
            .ok_or_else(|| err(format!("{sp}.shader"), "expected a name or path"))?
            .to_owned();
        let mut params = Vec::new();
        if let Some(p) = o.get("params") {
            for (k, v) in object(p, &format!("{sp}.params"))? {
                params.push((
                    k.clone(),
                    Track::parse_in(v, scope, &format!("{sp}.params.{k}"))?,
                ));
            }
        }
        shaders.push(ShaderSpec { shader, params });
    }
    Ok(LayerGrade {
        params,
        tracks,
        filters,
        shaders,
    })
}
