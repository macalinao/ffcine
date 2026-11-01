//! `DaVinci Resolve` native OTIO importer (`File > Export > Timeline`, `.otio`; Resolve 18.5+, seen
//! up to 21.x). `docs/import.md` has the field map; measured maths in vidcut `docs/gotchas.md`
//! ("Resolve's OTIO files").
//!
//! Resolve writes the editorial as plain OTIO (tracks, `Clip`s, `Gap`s, `LinearTimeWarp` /
//! `FreezeFrame`, `Transition`, `Marker`), which passes through unchanged, and everything else
//! under `metadata.Resolve_OTIO`. Each Inspector section arrives as one
//! `Effect.1 { effect_name: "Resolve Effect" }` per clip, with
//! `metadata.Resolve_OTIO = { "Effect Name", "Type", "Enabled", "Name", "Parameters": [..] }`.
//! Every clip carries every section, but only non-default parameters are listed. A parameter is
//! `{ "Parameter ID", "Parameter Value", "Variant Type", "Default Parameter Value"?,
//! "Key Frames"? }`; `Key Frames` maps a frame offset from the clip's first frame (`"3"`,
//! `"-86200"`) to `{ "Value", "Variant Type", "InBez"?, "OutBez"? }`, and a Bezier handle is
//! `{ "InBez": [frames, value], "Variant Type": "POINTF" }` (frames from its key, value in the
//! parameter's units).
//!
//! # Output
//!
//! Mapped sections become ffcine `Effect.1`s (`effect_name = "ffcine.<domain>.<name>"`, parameters
//! under `metadata.ffcine` with `"schema": "<effect_name>/<major>"`), in this order, which is the
//! order they apply in:
//!
//! | Resolve section (`Type`) | ffcine effect |
//! |---|---|
//! | Retime and Scaling (22) `scale` | `ffcine.geo.reframe/1 { mode, timeline_size? }` |
//! | Cropping (3) | `ffcine.geo.crop/1 { units, left, right, top, bottom, feather }` |
//! | Transform (2) | `ffcine.geo.transform/1 { units, y, anchor, translate_x/y, scale_x/y, rotate_deg, flip_x/y }` |
//! | Dynamic Zoom (59) | a second `ffcine.geo.transform/1`, keyed from the clip's start to its end |
//! | Composite (1) + Video Faders (36) | `ffcine.comp.blend/1 { opacity, mode }` |
//! | Fairlight Clip Volume and Fades (62) | `ffcine.audio.level/1 { gain_db, fade_in, fade_out }` (fades in seconds) |
//! | Fairlight Clip Pan (72) | `ffcine.audio.pan/1 { pan }` |
//!
//! Any parameter may be a plain value or keyframes,
//! `[{ "t": RationalTime, "v": .., "interp": "linear" | "ease" | "hold" }]`. `t` is in the clip's
//! source time (`source_range.start_time` + Resolve's key frame), like a `Marker`'s
//! `marked_range`; `interp` shapes the segment from that key to the next. Resolve's Bezier
//! segments are `ease` (approximate; the handles stay in the foreign block).
//!
//! Nothing Resolve wrote is dropped. On every object (Timeline, Track, Clip, Stack, Transition,
//! Marker, `MissingReference`, ...) the `Resolve_OTIO` namespace moves verbatim to
//! `metadata.ffcine.foreign.resolve`. A mapped ffcine effect keeps the section it came from as its
//! own `metadata.ffcine.foreign.resolve` (an array when two sections fed it: Composite and Video
//! Faders). Sections not mapped (the empty defaults every clip carries, Lens Correction, EQ, ...)
//! go verbatim, in order, to the clip's `metadata.ffcine.foreign.resolve_effects`, and a
//! generator's `parameters.Resolve_OTIO` to its `metadata.ffcine.foreign.resolve_parameters`.
//! A clip's `Link Group ID` is also `metadata.ffcine.link_group`, and a `MissingReference`'s
//! `Media Url` the clip's `metadata.ffcine.source.src` (a relink hint).
//!
//! # Units
//!
//! The file has no frame size and no source size, so geometry stays in Resolve's own unit, the
//! **picture**: the source as sized into the timeline frame by the reframe mode (`P`, in output
//! pixels, see [`Reframe::picture_size`]). That is exact whatever the source turns out to be; the
//! renderer finishes the conversion with the probed source size and its output size.
//!
//! - Positions are offsets from the middle of the picture in fractions of its width and height,
//!   **y down** (`"units": "picture"`, `"y": "down"`). Resolve's y is up; the importer negates it.
//! - `translate_x` = Pan (picture widths right), `translate_y` = −Tilt (picture heights down).
//! - `anchor` = `transformationAnchorPoint` with y negated. Zoom and rotation pivot on it.
//! - `rotate_deg` is counter-clockwise on screen, as in Resolve.
//! - Crop sides are fractions of the picture, cut before the transform; `feather` is in picture
//!   widths.
//!
//! With `A = anchor ⊙ P`, `T = translate ⊙ P`, `S = diag(scale_x, scale_y)` and `R` the rotation, a
//! picture point `q` (px from the picture's middle, y down, after the flips about the middle) lands
//! at `A + R·S·(q − A) + T` px from the frame's middle ([`Placement::apply`]). The picture's middle
//! lands at `A − R·S·A + T`, the measured Resolve behaviour.
//!
//! The frame size ([`ImportOptions::frame_size`], Resolve's timeline resolution) is only needed
//! for the crop sizing mode, where Resolve maps one source pixel to one timeline pixel: it is kept
//! as `timeline_size` so a render at another size scales the picture with it.

use std::path::Path;

use serde_json::{Map, Value, json};

use super::{ImportOptions, ImportReport, Imported};
use ffcine_core::{Error, Result};
use ffcine_timeline::{FFCINE_VERSION, Timeline};

/// The metadata namespace Resolve writes on every object.
pub const NAMESPACE: &str = "Resolve_OTIO";
/// Key under `metadata.ffcine.foreign` for an object's `Resolve_OTIO` namespace.
pub const APP: &str = "resolve";
/// Key under a clip's `metadata.ffcine.foreign` for the Resolve sections that weren't mapped.
pub const PRESERVED_EFFECTS: &str = "resolve_effects";
/// Key under a generator's `metadata.ffcine.foreign` for its `parameters.Resolve_OTIO`.
pub const PRESERVED_PARAMETERS: &str = "resolve_parameters";

/// Records an unmapped parameter or block (`Transform.transformationPitch`).
fn unmapped(report: &mut ImportReport, what: impl Into<String>) {
    report.unmapped([what.into()]);
}

/// Import a Resolve `.otio` file.
pub fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let text = std::fs::read_to_string(input).map_err(|source| Error::Io {
        path: input.to_path_buf(),
        source,
    })?;
    import_str(&text, opts)
}

/// Import Resolve OTIO JSON text.
pub fn import_str(text: &str, opts: &ImportOptions) -> Result<Imported> {
    import_value(serde_json::from_str(text)?, opts)
}

/// Import a parsed Resolve OTIO document.
pub fn import_value(timeline: Value, opts: &ImportOptions) -> Result<Imported> {
    let (json, report) = import_json(timeline, opts)?;
    Ok(Imported {
        timeline: serde_json::from_value::<Timeline>(json)?,
        report,
    })
}

/// [`import_value`] on the JSON tree, before the typed model.
pub fn import_json(mut timeline: Value, opts: &ImportOptions) -> Result<(Value, ImportReport)> {
    let schema = timeline.get("OTIO_SCHEMA").and_then(Value::as_str);
    if !schema.is_some_and(|s| s.starts_with("Timeline.")) {
        return Err(Error::Invalid {
            what: "Resolve OTIO root (expected Timeline)",
            value: schema.unwrap_or("none").to_owned(),
        });
    }
    let mut report = ImportReport::default();
    if !is_resolve(&timeline) {
        report.warn("no Resolve_OTIO metadata on the timeline: not a Resolve export?");
    }
    let rate = timeline
        .pointer("/global_start_time/rate")
        .and_then(Value::as_f64);
    if let (Some(want), Some(file)) = (opts.rate, rate)
        && !same(want, file)
    {
        report.warn(format!(
            "rate {want} ignored: Resolve's keyframes are frames at the file's {file} fps"
        ));
    }
    let mut walker = Walker {
        opts,
        rate: rate.or(opts.rate),
        report: &mut report,
    };
    walker.walk(&mut timeline);
    if let Some(o) = timeline.as_object_mut() {
        ffcine(o).insert("v".into(), json!(FFCINE_VERSION));
    }
    Ok((timeline, report))
}

/// Whether the file at `path` is a Resolve OTIO export; false if it can't be read as JSON.
pub fn detect(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| is_resolve(&v))
}

/// Whether an OTIO document is a Resolve export (`Resolve_OTIO` in the timeline's metadata).
pub fn is_resolve(timeline: &Value) -> bool {
    timeline
        .get("metadata")
        .and_then(|m| m.get(NAMESPACE))
        .is_some()
}

// ---------------------------------------------------------------------------------------------
// Parsed Resolve blocks
// ---------------------------------------------------------------------------------------------

/// One Inspector section: the `Resolve_OTIO` object of a `Resolve Effect`.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub effect_name: String,
    /// `Type`, the section's id.
    pub kind: Option<u64>,
    pub enabled: bool,
    pub name: Option<String>,
    pub params: Vec<Param>,
}

/// One entry of a section's `Parameters`.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub id: String,
    /// `Parameter Value`; ignored when there are keys.
    pub value: Value,
    pub default: Option<Value>,
    pub variant: Option<String>,
    /// Sorted by frame.
    pub keys: Vec<Key>,
}

/// One entry of `Key Frames`.
#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    /// Frames from the clip's first frame.
    pub frame: f64,
    pub value: Value,
    /// `[frames, value]` relative to the key; `None` when absent or zero-length.
    pub in_bez: Option<[f64; 2]>,
    pub out_bez: Option<[f64; 2]>,
}

/// The sections the importer maps, by `Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Composite,
    Transform,
    Cropping,
    RetimeScaling,
    VideoFaders,
    DynamicZoom,
    ClipVolume,
    ClipPan,
    Other,
}

impl Section {
    /// By `Type`; by `Effect Name` when `Type` is missing.
    pub fn of(kind: Option<u64>, effect_name: &str) -> Self {
        match (kind, effect_name) {
            (Some(1), _) | (None, "Composite") => Self::Composite,
            (Some(2), _) | (None, "Transform") => Self::Transform,
            (Some(3), _) | (None, "Cropping") => Self::Cropping,
            (Some(22), _) | (None, "Retime and Scaling") => Self::RetimeScaling,
            (Some(36), _) | (None, "Video Faders") => Self::VideoFaders,
            (Some(59), _) | (None, "Dynamic Zoom") => Self::DynamicZoom,
            (Some(62), _) | (None, "Fairlight Clip Volume and Fades") => Self::ClipVolume,
            (Some(72), _) | (None, "Fairlight Clip Pan") => Self::ClipPan,
            _ => Self::Other,
        }
    }
}

impl Block {
    pub fn parse(block: &Value) -> Option<Self> {
        let o = block.as_object()?;
        Some(Self {
            effect_name: o.get("Effect Name")?.as_str()?.to_owned(),
            kind: o.get("Type").and_then(code),
            enabled: o.get("Enabled").and_then(Value::as_bool).unwrap_or(true),
            name: o.get("Name").and_then(Value::as_str).map(str::to_owned),
            params: o
                .get("Parameters")
                .and_then(Value::as_array)
                .map(|ps| ps.iter().filter_map(Param::parse).collect())
                .unwrap_or_default(),
        })
    }

    pub fn param(&self, id: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.id == id)
    }

    pub fn section(&self) -> Section {
        Section::of(self.kind, &self.effect_name)
    }

    /// Whether the section changes anything: enabled, with a parameter off its default.
    fn in_use(&self) -> bool {
        self.enabled
            && self
                .params
                .iter()
                .any(|p| !p.is_default() && !STRUCTURAL.contains(&p.id.as_str()))
    }
}

/// Parameters that say where a section sits, not what it does.
const STRUCTURAL: [&str; 1] = ["eq band index"];

impl Param {
    pub fn parse(p: &Value) -> Option<Self> {
        let o = p.as_object()?;
        let mut keys: Vec<Key> = o
            .get("Key Frames")
            .and_then(Value::as_object)
            .map(|ks| ks.iter().filter_map(|(t, k)| Key::parse(t, k)).collect())
            .unwrap_or_default();
        keys.sort_by(|a, b| a.frame.total_cmp(&b.frame));
        Some(Self {
            id: o.get("Parameter ID")?.as_str()?.to_owned(),
            value: o.get("Parameter Value").cloned().unwrap_or(Value::Null),
            default: o.get("Default Parameter Value").cloned(),
            variant: o
                .get("Variant Type")
                .and_then(Value::as_str)
                .map(str::to_owned),
            keys,
        })
    }

    /// Unkeyed and at its declared default.
    fn is_default(&self) -> bool {
        self.keys.is_empty()
            && self
                .default
                .as_ref()
                .is_some_and(|d| same_value(d, &self.value))
    }

    /// A number or keyed numbers.
    pub fn scalar(&self) -> Option<Curve<f64>> {
        if self.keys.is_empty() {
            return self.value.as_f64().map(Curve::Const);
        }
        keyed(&self.keys, Value::as_f64)
    }

    /// A `POINTF` or keyed points.
    pub fn point(&self) -> Option<Curve<[f64; 2]>> {
        if self.keys.is_empty() {
            return as_point(&self.value).map(Curve::Const);
        }
        keyed(&self.keys, as_point)
    }

    pub fn flag(&self) -> Option<bool> {
        self.value
            .as_bool()
            .or_else(|| self.value.as_f64().map(|v| v.abs() > 0.0))
    }

    pub fn is_bezier(&self) -> bool {
        self.keys
            .iter()
            .any(|k| k.in_bez.is_some() || k.out_bez.is_some())
    }
}

impl Key {
    pub fn parse(frame: &str, key: &Value) -> Option<Self> {
        let frame = frame.trim().parse::<f64>().ok().filter(|f| f.is_finite())?;
        Some(Self {
            frame,
            value: key.get("Value")?.clone(),
            in_bez: handle(key, "InBez"),
            out_bez: handle(key, "OutBez"),
        })
    }
}

/// Resolve's handle form `{"InBez": {"InBez": [f, v], "Variant Type": "POINTF"}}`; a bare array
/// is accepted too. A handle of zero frames shapes nothing.
fn handle(key: &Value, name: &str) -> Option<[f64; 2]> {
    let h = key.get(name)?;
    let p = as_point(h.get(name).unwrap_or(h))?;
    (p[0].abs() > 0.0).then_some(p)
}

fn as_point(v: &Value) -> Option<[f64; 2]> {
    match v.as_array()?.as_slice() {
        [x, y] => Some([x.as_f64()?, y.as_f64()?]),
        _ => None,
    }
}

/// A whole number, from an integer or an integral float (`3` or `3.0`).
fn code(v: &Value) -> Option<u64> {
    if let Some(c) = v.as_u64() {
        return Some(c);
    }
    let f = v
        .as_f64()
        .filter(|f| f.fract() == 0.0 && (0.0..1e15).contains(f))?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(f as u64)
}

fn same(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => same(
            a.as_f64().unwrap_or(f64::NAN),
            b.as_f64().unwrap_or(f64::NAN),
        ),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_value(x, y))
        }
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------------------------
// Curves
// ---------------------------------------------------------------------------------------------

/// A mapped parameter: one value, or keyframes at frames from the clip's first frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Curve<T> {
    Const(T),
    Keys(Vec<Keyed<T>>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyed<T> {
    pub frame: f64,
    pub v: T,
    /// How the segment to the next key moves.
    pub interp: Interp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interp {
    Linear,
    Ease,
    Hold,
}

impl Interp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Ease => "ease",
            Self::Hold => "hold",
        }
    }
}

/// A segment with a handle on either end is a Bezier: `ease`. Otherwise Resolve plays a line.
fn keyed<T>(keys: &[Key], read: impl Fn(&Value) -> Option<T>) -> Option<Curve<T>> {
    let mut out = Vec::with_capacity(keys.len());
    for (i, k) in keys.iter().enumerate() {
        let next = keys.get(i + 1);
        let ease =
            next.is_some() && (k.out_bez.is_some() || next.is_some_and(|n| n.in_bez.is_some()));
        out.push(Keyed {
            frame: k.frame,
            v: read(&k.value)?,
            interp: if ease { Interp::Ease } else { Interp::Linear },
        });
    }
    Some(Curve::Keys(out))
}

impl<T: Copy> Curve<T> {
    pub fn map<U>(&self, f: impl Fn(T) -> U) -> Curve<U> {
        match self {
            Self::Const(v) => Curve::Const(f(*v)),
            Self::Keys(ks) => Curve::Keys(
                ks.iter()
                    .map(|k| Keyed {
                        frame: k.frame,
                        v: f(k.v),
                        interp: k.interp,
                    })
                    .collect(),
            ),
        }
    }
}

impl Curve<f64> {
    /// The value at a frame, with every segment read as a line (holds hold).
    pub fn at(&self, frame: f64) -> f64 {
        let ks = match self {
            Self::Const(v) => return *v,
            Self::Keys(ks) => ks,
        };
        let Some(first) = ks.first() else {
            return 0.0;
        };
        if frame <= first.frame {
            return first.v;
        }
        for w in ks.windows(2) {
            let (a, b) = (w[0], w[1]);
            if frame <= b.frame {
                if a.interp == Interp::Hold || b.frame <= a.frame {
                    return a.v;
                }
                return a.v + (b.v - a.v) * (frame - a.frame) / (b.frame - a.frame);
            }
        }
        ks.last().map_or(first.v, |k| k.v)
    }
}

/// A curve's value type.
pub trait ToJson {
    fn to_json(&self) -> Value;
}

impl ToJson for f64 {
    fn to_json(&self) -> Value {
        json!(self)
    }
}

impl ToJson for [f64; 2] {
    fn to_json(&self) -> Value {
        json!(self)
    }
}

impl<T: ToJson> Curve<T> {
    fn to_json(&self, ctx: &ClipCtx) -> Value {
        match self {
            Self::Const(v) => v.to_json(),
            Self::Keys(ks) => ks
                .iter()
                .map(|k| json!({ "t": ctx.time(k.frame), "v": k.v.to_json(), "interp": k.interp.as_str() }))
                .collect(),
        }
    }
}

/// Timing of the clip a section sits on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipCtx {
    pub rate: f64,
    /// `source_range.start_time`, in frames.
    pub start: f64,
    /// `source_range.duration`, in frames.
    pub duration: f64,
    pub frame_size: Option<(u32, u32)>,
}

impl ClipCtx {
    /// A key `frame` frames from the clip's first frame, in the clip's source time.
    pub fn time(&self, frame: f64) -> Value {
        rational(self.rate, self.start + frame)
    }
}

fn rational(rate: f64, value: f64) -> Value {
    json!({ "OTIO_SCHEMA": "RationalTime.1", "rate": rate, "value": value })
}

// ---------------------------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------------------------

pub const REFRAME: &str = "ffcine.geo.reframe";
pub const CROP: &str = "ffcine.geo.crop";
pub const TRANSFORM: &str = "ffcine.geo.transform";
pub const BLEND: &str = "ffcine.comp.blend";
pub const LEVEL: &str = "ffcine.audio.level";
pub const PAN: &str = "ffcine.audio.pan";
pub const TRANSITION: &str = "ffcine.transition";

/// Composite modes 0–26, in Resolve's order (vidcut `CompositeMode::ALL`).
pub const BLEND_MODES: [&str; 27] = [
    "normal",
    "add",
    "subtract",
    "difference",
    "multiply",
    "screen",
    "overlay",
    "hardlight",
    "softlight",
    "darken",
    "lighten",
    "color_dodge",
    "color_burn",
    "exclusion",
    "hue",
    "saturation",
    "color",
    "luminosity",
    "divide",
    "linear_dodge",
    "linear_burn",
    "linear_light",
    "vivid_light",
    "pin_light",
    "hard_mix",
    "lighter_color",
    "darker_color",
];

/// Composite modes 27–31: keys, not blends.
pub const KEY_MODES: [&str; 5] = [
    "foreground",
    "alpha",
    "inverted_alpha",
    "lum",
    "inverted_lum",
];

const ZOOM_X: &str = "transformationZoomX";
const ZOOM_Y: &str = "transformationZoomY";
const ZOOM_LINK: &str = "transformationZoomLink";
const T_PAN: &str = "transformationPan";
const T_TILT: &str = "transformationTilt";
const ROTATION: &str = "transformationRotationAngle";
const ANCHOR: &str = "transformationAnchorPoint";
const FLIP_X: &str = "transformationFlipX";
const FLIP_Y: &str = "transformationFlipY";
const PITCH: &str = "transformationPitch";
const YAW: &str = "transformationYaw";
const CROPS: [(&str, &str); 4] = [
    ("cropLeft", "left"),
    ("cropRight", "right"),
    ("cropTop", "top"),
    ("cropBottom", "bottom"),
];
const SOFTNESS: &str = "cropSoftness";
const OPACITY: &str = "opacity";
const MODE: &str = "composite mode";
const SCALE: &str = "scale";
const DZ_CENTER: &str = "dynamicZoomCenter";
const DZ_SCALE: &str = "dynamicZoomScale";
const DZ_EASE: &str = "dynamicZoomEase";
const V_FADE_IN: &str = "videoFaderIn";
const V_FADE_OUT: &str = "videoFaderOut";
const VOLUME: &str = "volume";
const A_FADE_IN: &str = "faderIn";
const A_FADE_OUT: &str = "faderOut";
const A_PAN: &str = "pan";

/// An ffcine effect built from Resolve sections.
#[derive(Debug, Clone, PartialEq)]
pub struct FfEffect {
    /// `Effect.name`: Resolve's section name.
    pub name: String,
    pub effect_name: &'static str,
    /// Goes under `metadata.ffcine`, after `schema`.
    pub params: Map<String, Value>,
    /// The `Resolve_OTIO` blocks it came from, verbatim.
    pub sources: Vec<Value>,
}

impl FfEffect {
    pub fn schema(&self) -> String {
        format!("{}/1", self.effect_name)
    }

    pub fn to_otio(&self) -> Value {
        let mut ff = Map::new();
        ff.insert("schema".into(), json!(self.schema()));
        ff.extend(self.params.clone());
        let foreign = match self.sources.as_slice() {
            [one] => one.clone(),
            many => Value::Array(many.to_vec()),
        };
        ff.insert("foreign".into(), json!({ APP: foreign }));
        json!({
            "OTIO_SCHEMA": "Effect.1",
            "name": self.name,
            "effect_name": self.effect_name,
            "metadata": { "ffcine": ff },
        })
    }
}

/// Map a clip's sections. Returns the ffcine effects, in application order, and which of `blocks`
/// they used up; the rest stay foreign.
pub fn map_sections(
    blocks: &[(Block, Value)],
    ctx: &ClipCtx,
    report: &mut ImportReport,
) -> (Vec<FfEffect>, Vec<bool>) {
    let mut out = Sections {
        blocks,
        used: vec![false; blocks.len()],
        effects: Vec::new(),
    };
    out.one(Section::RetimeScaling, REFRAME, map_reframe, ctx, report);
    out.one(Section::Cropping, CROP, map_crop, ctx, report);
    out.one(Section::Transform, TRANSFORM, map_transform, ctx, report);
    out.one(
        Section::DynamicZoom,
        TRANSFORM,
        map_dynamic_zoom,
        ctx,
        report,
    );
    let pictured = !out.effects.is_empty();
    let sized = out.effects.iter().any(|e| e.effect_name == REFRAME);

    let composite = out.find(Section::Composite, report);
    let faders = out.find(Section::VideoFaders, report);
    let pick = |i: Option<usize>| i.map(|i| &blocks[i].0);
    if let Some(params) = map_blend(pick(composite), pick(faders), ctx, report) {
        // Only the sections that changed something; an empty one stays with the clip.
        let from: Vec<usize> = [composite, faders]
            .into_iter()
            .flatten()
            .filter(|&i| blocks[i].0.in_use())
            .collect();
        for &i in &from {
            out.used[i] = true;
        }
        out.effects.push(FfEffect {
            name: "Composite".into(),
            effect_name: BLEND,
            params,
            sources: from.iter().map(|&i| blocks[i].1.clone()).collect(),
        });
    }
    out.one(Section::ClipVolume, LEVEL, map_volume, ctx, report);
    out.one(Section::ClipPan, PAN, map_pan, ctx, report);

    if pictured && !sized {
        report.warn(
            "Retime and Scaling has no scale on some placed clips: Resolve sizes them by the \
             project setting (fit in a new project); no reframe written, read as fit",
        );
    }
    for ((b, _), &u) in blocks.iter().zip(&out.used) {
        if !u && b.section() == Section::Other && b.in_use() {
            unmapped(report, b.effect_name.clone());
        }
    }
    (out.effects, out.used)
}

type MapFn = fn(&Block, &ClipCtx, &mut ImportReport) -> Option<Map<String, Value>>;

struct Sections<'a> {
    blocks: &'a [(Block, Value)],
    used: Vec<bool>,
    effects: Vec<FfEffect>,
}

impl Sections<'_> {
    /// The first enabled block of a section.
    fn find(&self, s: Section, report: &mut ImportReport) -> Option<usize> {
        let mut of = (0..self.blocks.len())
            .filter(|&i| self.blocks[i].0.enabled && self.blocks[i].0.section() == s);
        let first = of.next()?;
        if of.next().is_some() {
            report.warn(format!(
                "two enabled {} sections on one clip: only the first is mapped",
                self.blocks[first].0.effect_name
            ));
        }
        Some(first)
    }

    fn one(
        &mut self,
        s: Section,
        effect_name: &'static str,
        f: MapFn,
        ctx: &ClipCtx,
        report: &mut ImportReport,
    ) {
        let Some(i) = self.find(s, report) else {
            return;
        };
        let (b, raw) = &self.blocks[i];
        if let Some(params) = f(b, ctx, report) {
            self.used[i] = true;
            self.effects.push(FfEffect {
                name: b.name.clone().unwrap_or_else(|| b.effect_name.clone()),
                effect_name,
                params,
                sources: vec![raw.clone()],
            });
        }
    }
}

/// Report the parameters of `b` that aren't in `known` and are off their defaults.
fn unknown_params(b: &Block, known: &[&str], report: &mut ImportReport) {
    for p in &b.params {
        if !known.contains(&p.id.as_str()) && !p.is_default() {
            unmapped(report, format!("{}.{}", b.effect_name, p.id));
        }
    }
}

/// A number parameter off `default`, or keyed.
fn scalar(b: &Block, id: &str, default: f64, report: &mut ImportReport) -> Option<Curve<f64>> {
    let p = b.param(id)?;
    let Some(c) = p.scalar() else {
        unmapped(report, format!("{}.{} (unreadable)", b.effect_name, id));
        return None;
    };
    if let Curve::Const(v) = c
        && same(v, default)
    {
        return None;
    }
    bezier_note(b, p, report);
    Some(c)
}

fn point(b: &Block, id: &str, report: &mut ImportReport) -> Option<Curve<[f64; 2]>> {
    let p = b.param(id)?;
    let Some(c) = p.point() else {
        unmapped(report, format!("{}.{} (unreadable)", b.effect_name, id));
        return None;
    };
    if let Curve::Const([x, y]) = c
        && same(x, 0.0)
        && same(y, 0.0)
    {
        return None;
    }
    bezier_note(b, p, report);
    Some(c)
}

fn bezier_note(b: &Block, p: &Param, report: &mut ImportReport) {
    if p.is_bezier() {
        report.warn(format!(
            "{}.{}: Bezier keyframes read as ease (approx)",
            b.effect_name, p.id
        ));
    }
}

/// A plain number parameter (fader lengths), 0 when absent.
fn number(b: &Block, id: &str) -> f64 {
    b.param(id).and_then(|p| p.value.as_f64()).unwrap_or(0.0)
}

fn put<T: ToJson>(m: &mut Map<String, Value>, key: &str, c: Option<Curve<T>>, ctx: &ClipCtx) {
    if let Some(c) = c {
        m.insert(key.into(), c.to_json(ctx));
    }
}

/// Picture-relative geometry header: see the module docs.
fn picture_units() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("units".into(), json!("picture"));
    m.insert("y".into(), json!("down"));
    m
}

/// Retime and Scaling `scale`: 0 project, 1 crop, 2 fit, 3 fill, 4 stretch.
fn map_reframe(b: &Block, ctx: &ClipCtx, report: &mut ImportReport) -> Option<Map<String, Value>> {
    unknown_params(b, &[SCALE], report);
    let p = b.param(SCALE)?;
    let mode = match code(&p.value) {
        Some(0) => {
            report.warn(
                "Retime and Scaling scale 0 (project setting) read as fit, the default of a new \
                 project",
            );
            "fit"
        }
        Some(1) => "crop",
        Some(2) => "fit",
        Some(3) => "fill",
        Some(4) => "stretch",
        _ => {
            unmapped(report, format!("Retime and Scaling.scale={}", p.value));
            return None;
        }
    };
    let mut m = Map::new();
    m.insert("mode".into(), json!(mode));
    if mode == "crop" {
        match ctx.frame_size {
            Some((width, height)) => {
                m.insert("timeline_size".into(), json!([width, height]));
            }
            None => report.warn(
                "Retime and Scaling crop maps a source pixel to a timeline pixel: without the \
                 timeline frame size (--frame-size) it is read at the output size",
            ),
        }
    }
    Some(m)
}

fn map_crop(b: &Block, ctx: &ClipCtx, report: &mut ImportReport) -> Option<Map<String, Value>> {
    let known: Vec<&str> = CROPS.iter().map(|(id, _)| *id).chain([SOFTNESS]).collect();
    unknown_params(b, &known, report);
    let mut m = picture_units();
    let header = m.len();
    for (id, key) in CROPS {
        put(&mut m, key, scalar(b, id, 0.0, report), ctx);
    }
    if let Some(s) = scalar(b, SOFTNESS, 0.0, report) {
        report.warn(
            "Cropping.cropSoftness read as a linear feather of |softness|/400 picture widths \
             (approx; the sign is dropped)",
        );
        put(&mut m, "feather", Some(s.map(|v| v.abs() / 400.0)), ctx);
    }
    (m.len() > header).then_some(m)
}

fn map_transform(
    b: &Block,
    ctx: &ClipCtx,
    report: &mut ImportReport,
) -> Option<Map<String, Value>> {
    unknown_params(
        b,
        &[
            ZOOM_X, ZOOM_Y, ZOOM_LINK, T_PAN, T_TILT, ROTATION, ANCHOR, FLIP_X, FLIP_Y, PITCH, YAW,
        ],
        report,
    );
    for id in [PITCH, YAW] {
        if scalar(b, id, 0.0, report).is_some() {
            unmapped(report, format!("Transform.{id}"));
        }
    }
    let mut m = picture_units();
    let header = m.len();
    put(
        &mut m,
        "anchor",
        point(b, ANCHOR, report).map(|c| c.map(|[x, y]| [x, 0.0 - y])),
        ctx,
    );
    put(&mut m, "translate_x", scalar(b, T_PAN, 0.0, report), ctx);
    put(
        &mut m,
        "translate_y",
        scalar(b, T_TILT, 0.0, report).map(|c| c.map(|v| 0.0 - v)),
        ctx,
    );
    put(&mut m, "scale_x", scalar(b, ZOOM_X, 1.0, report), ctx);
    put(&mut m, "scale_y", scalar(b, ZOOM_Y, 1.0, report), ctx);
    put(&mut m, "rotate_deg", scalar(b, ROTATION, 0.0, report), ctx);
    for (id, key) in [(FLIP_X, "flip_x"), (FLIP_Y, "flip_y")] {
        if let Some(p) = b.param(id) {
            if !p.keys.is_empty() {
                unmapped(report, format!("Transform.{id} (keyframed)"));
            }
            if p.flag() == Some(true) {
                m.insert(key.into(), json!(true));
            }
        }
    }
    (m.len() > header).then_some(m)
}

/// The start and end of a Dynamic Zoom parameter. Resolve writes two keys (at −s and 1000 − s,
/// s the clip's first source frame) and reads the earlier as the start, whatever their times.
fn ends<T: Copy>(c: Option<Curve<T>>, default: T) -> [T; 2] {
    match c {
        None => [default, default],
        Some(Curve::Const(v)) => [v, v],
        Some(Curve::Keys(ks)) => match (ks.first(), ks.last()) {
            (Some(a), Some(b)) => [a.v, b.v],
            _ => [default, default],
        },
    }
}

/// Dynamic Zoom: a box of `dynamicZoomScale` pictures centred on `dynamicZoomCenter` (picture
/// fractions from the middle, y up) moves from its start to its end over the clip. Showing box
/// `(c, s)` is a zoom of `1/s` with `translate = −c'/s` (`c'` = `c` with y down), keyed at the
/// clip's start and end. Resolve plays every `dynamicZoomEase` as a straight line (measured), so
/// the keys are linear.
fn map_dynamic_zoom(
    b: &Block,
    ctx: &ClipCtx,
    report: &mut ImportReport,
) -> Option<Map<String, Value>> {
    unknown_params(b, &[DZ_CENTER, DZ_SCALE, DZ_EASE], report);
    let center = b.param(DZ_CENTER).and_then(Param::point);
    let size = b.param(DZ_SCALE).and_then(Param::scalar);
    let [c0, c1] = ends(center, [0.0, 0.0]);
    let [s0, s1] = ends(size, 1.0);
    if !(s0 > 0.0 && s1 > 0.0) {
        unmapped(report, "Dynamic Zoom.dynamicZoomScale (not positive)");
        return None;
    }
    let identity = |c: [f64; 2], s: f64| same(c[0], 0.0) && same(c[1], 0.0) && same(s, 1.0);
    if identity(c0, s0) && identity(c1, s1) {
        return None;
    }
    report.warn(
        "Dynamic Zoom read as transform keyframes from the clip's start to its end (approx: \
         composition with Transform and the box's aspect are unverified)",
    );
    let curve = |a: f64, b: f64| {
        if same(a, b) {
            Curve::Const(a)
        } else {
            Curve::Keys(vec![
                Keyed {
                    frame: 0.0,
                    v: a,
                    interp: Interp::Linear,
                },
                Keyed {
                    frame: ctx.duration,
                    v: b,
                    interp: Interp::Linear,
                },
            ])
        }
    };
    let mut m = picture_units();
    put(
        &mut m,
        "translate_x",
        Some(curve(0.0 - c0[0] / s0, 0.0 - c1[0] / s1)),
        ctx,
    );
    put(
        &mut m,
        "translate_y",
        Some(curve(c0[1] / s0, c1[1] / s1)),
        ctx,
    );
    put(&mut m, "scale_x", Some(curve(1.0 / s0, 1.0 / s1)), ctx);
    put(&mut m, "scale_y", Some(curve(1.0 / s0, 1.0 / s1)), ctx);
    Some(m)
}

/// Composite opacity (0–100) and mode, times the Video Faders ramps (lengths in frames, linear).
fn map_blend(
    composite: Option<&Block>,
    faders: Option<&Block>,
    ctx: &ClipCtx,
    report: &mut ImportReport,
) -> Option<Map<String, Value>> {
    let mut opacity = None;
    let mut mode = None;
    if let Some(b) = composite {
        unknown_params(b, &[OPACITY, MODE], report);
        opacity = scalar(b, OPACITY, 100.0, report).map(|c| c.map(|v| v / 100.0));
        if let Some(p) = b.param(MODE) {
            match code(&p.value) {
                _ if !p.keys.is_empty() => unmapped(report, "Composite.composite mode (keyframed)"),
                Some(0) => {}
                Some(c @ 1..=26) => {
                    mode = BLEND_MODES.get(usize::try_from(c).unwrap_or(0)).copied();
                }
                Some(c @ 27..=31) => {
                    let name = KEY_MODES[usize::try_from(c - 27).unwrap_or(0)];
                    unmapped(report, format!("Composite.composite mode={name}"));
                    report.warn(format!(
                        "Composite mode {name} (a key) is rendered as normal"
                    ));
                }
                _ => unmapped(report, format!("Composite.composite mode={}", p.value)),
            }
        }
    }
    if let Some(b) = faders {
        unknown_params(b, &[V_FADE_IN, V_FADE_OUT], report);
        let (fin, fout) = (number(b, V_FADE_IN), number(b, V_FADE_OUT));
        if fin > 0.0 || fout > 0.0 {
            let base = opacity.unwrap_or(Curve::Const(1.0));
            opacity = Some(faded(&base, fin, fout, ctx.duration, report));
        }
    }
    if opacity.is_none() && mode.is_none() {
        return None;
    }
    let mut m = Map::new();
    put(&mut m, "opacity", opacity, ctx);
    if let Some(mode) = mode {
        m.insert("mode".into(), json!(mode));
    }
    Some(m)
}

/// `base` times linear fades of `fin` frames from 0 and `fout` frames to `d`. Exact for a constant
/// `base` and fades that don't overlap.
fn faded(base: &Curve<f64>, fin: f64, fout: f64, d: f64, report: &mut ImportReport) -> Curve<f64> {
    let fade = |f: f64| {
        let mut g: f64 = 1.0;
        if fin > 0.0 {
            g = g.min(f / fin);
        }
        if fout > 0.0 {
            g = g.min((d - f) / fout);
        }
        g.clamp(0.0, 1.0)
    };
    let mut frames = Vec::new();
    if fin > 0.0 {
        frames.extend([0.0, fin.min(d)]);
    }
    if fout > 0.0 {
        frames.extend([(d - fout).max(0.0), d]);
    }
    if fin > 0.0 && fout > 0.0 && fin + fout > d {
        frames.push(d * fin / (fin + fout));
        report.warn("Video Faders longer than their clip: the fades are read as overlapping lines");
    }
    if let Curve::Keys(ks) = base {
        frames.extend(ks.iter().map(|k| k.frame));
        report.warn(
            "Video Faders on keyframed opacity: the product is sampled at the keys and fade ends \
             (approx)",
        );
    }
    frames.sort_by(f64::total_cmp);
    frames.dedup_by(|a, b| same(*a, *b));
    Curve::Keys(
        frames
            .into_iter()
            .map(|f| Keyed {
                frame: f,
                v: base.at(f) * fade(f),
                interp: Interp::Linear,
            })
            .collect(),
    )
}

/// Fairlight Clip Volume (dB, keyable) and its fades (lengths in frames).
fn map_volume(b: &Block, ctx: &ClipCtx, report: &mut ImportReport) -> Option<Map<String, Value>> {
    unknown_params(b, &[VOLUME, A_FADE_IN, A_FADE_OUT], report);
    let gain = scalar(b, VOLUME, 0.0, report);
    let (fin, fout) = (number(b, A_FADE_IN), number(b, A_FADE_OUT));
    if gain.is_none() && fin <= 0.0 && fout <= 0.0 {
        return None;
    }
    let mut m = Map::new();
    put(&mut m, "gain_db", gain, ctx);
    for (key, len) in [("fade_in", fin), ("fade_out", fout)] {
        if len > 0.0 {
            m.insert(key.into(), json!(len / ctx.rate));
        }
    }
    if fin > 0.0 || fout > 0.0 {
        report.warn("Fairlight clip fades read as fade_in/fade_out lengths (curve shape approx)");
    }
    Some(m)
}

/// Fairlight Clip Pan `pan`, −100 (left) to 100 (right), as −1 to 1.
fn map_pan(b: &Block, ctx: &ClipCtx, report: &mut ImportReport) -> Option<Map<String, Value>> {
    unknown_params(b, &[A_PAN], report);
    let pan = scalar(b, A_PAN, 0.0, report)?;
    report.warn("Fairlight Clip Pan read as −100..100 (range unverified)");
    let mut m = Map::new();
    put(&mut m, "pan", Some(pan.map(|v| v / 100.0)), ctx);
    Some(m)
}

/// A transition's `Resolve_OTIO = { "Effects": <section>, "Transition Type" }` as
/// `metadata.ffcine` fields: `kind` (`dissolve`, `cross_fade` with `crossfade_db`, `wipe` with
/// `angle_deg`) and `ease` (`in`, `out`, `in_out`).
pub fn map_transition(
    transition_type: Option<&str>,
    ns: &Value,
    report: &mut ImportReport,
) -> Map<String, Value> {
    let block = ns.get("Effects").and_then(Block::parse);
    let label = ns
        .get("Transition Type")
        .and_then(Value::as_str)
        .or(block.as_ref().map(|b| b.effect_name.as_str()))
        .unwrap_or("")
        .to_owned();
    let effect = block
        .as_ref()
        .map_or(label.as_str(), |b| b.effect_name.as_str());
    let mut m = Map::new();
    let mut known = vec!["ease", "transitionCustomCurvesKeyframes"];
    if let Some(db) = cross_fade_db(effect).or_else(|| cross_fade_db(&label)) {
        m.insert("kind".into(), json!("cross_fade"));
        m.insert("crossfade_db".into(), json!(db));
    } else if effect == "Edge Wipe" {
        m.insert("kind".into(), json!("wipe"));
        let angle = block
            .as_ref()
            .and_then(|b| b.param("angle"))
            .and_then(|p| p.value.as_f64());
        m.insert("angle_deg".into(), json!(angle.unwrap_or(0.0)));
        known.push("angle");
    } else if transition_type == Some("SMPTE_Dissolve") && matches!(effect, "" | "Cross Dissolve") {
        m.insert("kind".into(), json!("dissolve"));
    } else {
        unmapped(report, format!("Transition.{label}"));
    }
    if let Some(b) = &block {
        if !m.is_empty() {
            for p in &b.params {
                if !known.contains(&p.id.as_str()) && !p.is_default() {
                    unmapped(report, format!("Transition.{label}.{}", p.id));
                }
            }
        }
        match b.param("ease").map(|p| code(&p.value)) {
            None | Some(Some(0)) => {}
            Some(Some(e @ 1..=3)) => {
                let ease = ["in", "out", "in_out"][usize::try_from(e - 1).unwrap_or(0)];
                m.insert("ease".into(), json!(ease));
                report.warn(format!(
                    "transition ease {ease}: Resolve's Bezier curve read as ffcine's ease (approx)"
                ));
            }
            Some(_) => unmapped(report, "Transition.ease=custom"),
        }
    }
    if !m.is_empty() {
        let mut out = Map::new();
        out.insert("schema".into(), json!(format!("{TRANSITION}/1")));
        out.extend(m);
        return out;
    }
    m
}

/// `Cross Fade 0 dB`, `Cross Fade +3 dB`, `Cross Fade -3dB`, `Cross Fade 0DB` -> the dB number.
fn cross_fade_db(name: &str) -> Option<f64> {
    let rest = name.strip_prefix("Cross Fade")?.trim();
    let num = rest
        .strip_suffix("dB")
        .or_else(|| rest.strip_suffix("DB"))
        .or_else(|| rest.strip_suffix("db"))?
        .trim();
    num.parse().ok()
}

// ---------------------------------------------------------------------------------------------
// Geometry (for the renderer; documents the units)
// ---------------------------------------------------------------------------------------------

/// Resolve's sizing modes, as `ffcine.geo.reframe/1 { mode }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reframe {
    Fit,
    Fill,
    Stretch,
    /// One source pixel per timeline pixel.
    Crop,
}

impl Reframe {
    /// The picture's size in output pixels, for a `source` (display size, square pixels) in an
    /// output `frame`. `timeline` is Resolve's frame size (`timeline_size`), used by crop only;
    /// without it crop is one source pixel per output pixel.
    pub fn picture_size(
        self,
        source: [f64; 2],
        frame: [f64; 2],
        timeline: Option<[f64; 2]>,
    ) -> [f64; 2] {
        let ([sw, sh], [fw, fh]) = (source, frame);
        match self {
            Self::Fit => {
                let k = (fw / sw).min(fh / sh);
                [sw * k, sh * k]
            }
            Self::Fill => {
                let k = (fw / sw).max(fh / sh);
                [sw * k, sh * k]
            }
            Self::Stretch => frame,
            Self::Crop => {
                let k = timeline.map_or(1.0, |t| fw / t[0]);
                [sw * k, sh * k]
            }
        }
    }
}

/// `ffcine.geo.transform/1` at one instant, in picture units (y down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub anchor: [f64; 2],
    pub translate: [f64; 2],
    pub scale: [f64; 2],
    pub rotate_deg: f64,
    pub flip: [bool; 2],
}

impl Default for Placement {
    fn default() -> Self {
        Self {
            anchor: [0.0, 0.0],
            translate: [0.0, 0.0],
            scale: [1.0, 1.0],
            rotate_deg: 0.0,
            flip: [false, false],
        }
    }
}

impl Placement {
    /// Where picture point `q` (px from the picture's middle, y down) lands, in px from the
    /// frame's middle (y down), for a picture of `picture` px: `A + R·S·(flip(q) − A) + T`.
    pub fn apply(&self, picture: [f64; 2], q: [f64; 2]) -> [f64; 2] {
        let [pw, ph] = picture;
        let flipped = [
            if self.flip[0] { -q[0] } else { q[0] },
            if self.flip[1] { -q[1] } else { q[1] },
        ];
        let anchor = [self.anchor[0] * pw, self.anchor[1] * ph];
        let zoomed = [
            (flipped[0] - anchor[0]) * self.scale[0],
            (flipped[1] - anchor[1]) * self.scale[1],
        ];
        // Counter-clockwise on screen, with y down.
        let (sin, cos) = self.rotate_deg.to_radians().sin_cos();
        let turned = [
            cos * zoomed[0] + sin * zoomed[1],
            -sin * zoomed[0] + cos * zoomed[1],
        ];
        [
            anchor[0] + turned[0] + self.translate[0] * pw,
            anchor[1] + turned[1] + self.translate[1] * ph,
        ]
    }
}

// ---------------------------------------------------------------------------------------------
// The OTIO tree
// ---------------------------------------------------------------------------------------------

struct Walker<'a> {
    opts: &'a ImportOptions,
    rate: Option<f64>,
    report: &'a mut ImportReport,
}

/// `m[key]` as an object, made one if it isn't.
fn obj_entry<'m>(m: &'m mut Map<String, Value>, key: &str) -> &'m mut Map<String, Value> {
    let v = m.entry(key).or_insert_with(|| Value::Object(Map::new()));
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    match v {
        Value::Object(o) => o,
        _ => unreachable!("just made an object"),
    }
}

/// `metadata.ffcine` of an OTIO object.
fn ffcine(o: &mut Map<String, Value>) -> &mut Map<String, Value> {
    obj_entry(obj_entry(o, "metadata"), "ffcine")
}

fn put_foreign(o: &mut Map<String, Value>, key: &str, value: Value) {
    obj_entry(ffcine(o), "foreign").insert(key.into(), value);
}

/// The `Resolve_OTIO` block of a `Resolve Effect` in Resolve's usual wrapper (empty `name`, no
/// other metadata or fields), which can be stored compactly and rebuilt on export.
fn plain_resolve_effect(e: &Value) -> Option<&Value> {
    let o = e.as_object()?;
    if !o.get("OTIO_SCHEMA")?.as_str()?.starts_with("Effect.") {
        return None;
    }
    let md = o.get("metadata")?.as_object()?;
    let block = md.get(NAMESPACE)?;
    let plain = md.len() == 1
        && o.keys().all(|k| {
            matches!(
                k.as_str(),
                "OTIO_SCHEMA" | "metadata" | "name" | "effect_name"
            )
        })
        && o.get("name")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        && o.get("effect_name").and_then(Value::as_str) == Some("Resolve Effect");
    plain.then_some(block)
}

enum Slot {
    Keep(Value),
    Resolve(usize),
}

impl Walker<'_> {
    fn walk(&mut self, v: &mut Value) {
        match v {
            Value::Object(o) => {
                if let Some(schema) = o.get("OTIO_SCHEMA").and_then(Value::as_str) {
                    let base = schema.split('.').next().unwrap_or("").to_owned();
                    self.object(&base, o);
                }
                for (k, child) in o.iter_mut() {
                    if k != "metadata" {
                        self.walk(child);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.walk(x)),
            _ => {}
        }
    }

    fn object(&mut self, base: &str, o: &mut Map<String, Value>) {
        match base {
            "Clip" | "Stack" => self.item(o),
            "GeneratorReference" => {
                if let Some(Value::Object(params)) = o.get_mut("parameters")
                    && let Some(ns) = params.remove(NAMESPACE)
                {
                    for b in ns.as_array().into_iter().flatten().filter_map(Block::parse) {
                        unmapped(self.report, format!("Generator.{}", b.effect_name));
                    }
                    put_foreign(o, PRESERVED_PARAMETERS, ns);
                }
            }
            _ => {}
        }
        let Some(ns) = o
            .get_mut("metadata")
            .and_then(Value::as_object_mut)
            .and_then(|m| m.remove(NAMESPACE))
        else {
            return;
        };
        match base {
            "Clip" | "Stack" => {
                if let Some(id) = ns.get("Link Group ID") {
                    ffcine(o).insert("link_group".into(), id.clone());
                }
            }
            "Transition" => {
                let tt = o.get("transition_type").and_then(Value::as_str);
                let m = map_transition(tt, &ns, self.report);
                ffcine(o).extend(m);
            }
            "Effect" => {
                if let Some(b) = Block::parse(&ns)
                    && b.in_use()
                {
                    unmapped(self.report, b.effect_name);
                }
            }
            _ => {}
        }
        put_foreign(o, APP, ns);
    }

    fn ctx(&mut self, o: &Map<String, Value>) -> ClipCtx {
        let range = o.get("source_range");
        let start = range.and_then(|r| r.get("start_time"));
        let dur = range.and_then(|r| r.get("duration"));
        let rate_of = |t: Option<&Value>| t.and_then(|t| t.get("rate")).and_then(Value::as_f64);
        let value_of = |t: Option<&Value>| t.and_then(|t| t.get("value")).and_then(Value::as_f64);
        let rate = rate_of(start).or(rate_of(dur)).or(self.rate);
        let rate = rate.unwrap_or_else(|| {
            self.report
                .warn("no frame rate in the file: keyframes timed at 24 fps");
            24.0
        });
        let duration = match (value_of(dur), rate_of(dur)) {
            (Some(v), Some(r)) if r > 0.0 => v * rate / r,
            (Some(v), None) => v,
            _ => 0.0,
        };
        ClipCtx {
            rate,
            start: value_of(start).unwrap_or(0.0),
            duration,
            frame_size: self.opts.frame_size,
        }
    }

    /// A Clip's or Stack's `effects`: Resolve sections become ffcine effects or stay foreign.
    fn item(&mut self, o: &mut Map<String, Value>) {
        if let Some(url) = media_url(o) {
            ffcine(o)
                .entry("source")
                .or_insert_with(|| json!({ "src": url }));
        }
        let Some(Value::Array(effects)) = o.get_mut("effects").map(std::mem::take) else {
            return;
        };
        let mut blocks: Vec<(Block, Value)> = Vec::new();
        let mut slots = Vec::new();
        for e in effects {
            let parsed =
                plain_resolve_effect(&e).and_then(|raw| Some((Block::parse(raw)?, raw.clone())));
            match parsed {
                Some(pb) => {
                    slots.push(Slot::Resolve(blocks.len()));
                    blocks.push(pb);
                }
                None => slots.push(Slot::Keep(e)),
            }
        }
        if blocks.is_empty() {
            o.insert(
                "effects".into(),
                Value::Array(
                    slots
                        .into_iter()
                        .filter_map(|s| match s {
                            Slot::Keep(e) => Some(e),
                            Slot::Resolve(_) => None,
                        })
                        .collect(),
                ),
            );
            return;
        }
        let ctx = self.ctx(o);
        let (mapped, used) = map_sections(&blocks, &ctx, self.report);
        let keyed = mapped.iter().any(|e| e.params.values().any(is_keyframes));
        let mut out = Vec::new();
        let mut mapped = Some(mapped);
        let mut preserved = Vec::new();
        for s in slots {
            match s {
                Slot::Keep(e) => {
                    if keyed && is_retimed(&e) {
                        self.report.warn(
                            "keyframes on a retimed clip are timed in timeline frames from the \
                             clip's start (approx)",
                        );
                    }
                    out.push(e);
                }
                Slot::Resolve(i) => {
                    if let Some(m) = mapped.take() {
                        out.extend(m.iter().map(FfEffect::to_otio));
                    }
                    if !used[i] {
                        preserved.push(blocks[i].1.clone());
                    }
                }
            }
        }
        o.insert("effects".into(), Value::Array(out));
        if !preserved.is_empty() {
            put_foreign(o, PRESERVED_EFFECTS, Value::Array(preserved));
        }
    }
}

/// The active media reference's `Resolve_OTIO."Media Url"` (on `MissingReference`s).
fn media_url(o: &Map<String, Value>) -> Option<String> {
    let key = o
        .get("active_media_reference_key")
        .and_then(Value::as_str)
        .unwrap_or("DEFAULT_MEDIA");
    let r = o.get("media_references")?.get(key)?;
    let url = r.get("metadata")?.get(NAMESPACE)?.get("Media Url")?;
    url.as_str().map(str::to_owned)
}

fn is_keyframes(v: &Value) -> bool {
    v.as_array()
        .and_then(|a| a.first())
        .is_some_and(|k| k.get("t").is_some())
}

fn is_retimed(e: &Value) -> bool {
    let schema = e.get("OTIO_SCHEMA").and_then(Value::as_str).unwrap_or("");
    schema.starts_with("FreezeFrame")
        || (schema.starts_with("LinearTimeWarp")
            && e.get("time_scalar")
                .and_then(Value::as_f64)
                .is_some_and(|s| !same(s, 1.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(v: &Value) -> Block {
        Block::parse(v).expect("block")
    }

    fn ctx() -> ClipCtx {
        ClipCtx {
            rate: 24.0,
            start: 100.0,
            duration: 48.0,
            frame_size: Some((1920, 1080)),
        }
    }

    #[test]
    fn parses_keys_and_handles() {
        let b = block(&json!({
            "Effect Name": "Transform", "Enabled": true, "Name": "Transform", "Type": 2,
            "Parameters": [{
                "Default Parameter Value": 1.0, "Parameter ID": "transformationZoomX",
                "Parameter Value": 1.19, "Variant Type": "Double",
                "Key Frames": {
                    "424": { "InBez": { "InBez": [-49.75, 0.0], "Variant Type": "POINTF" },
                             "Value": 1.5, "Variant Type": "Double" },
                    "3": { "Value": 2.0, "Variant Type": "Double",
                           "OutBez": { "OutBez": [20.0, 0.1], "Variant Type": "POINTF" } },
                    "200": { "Value": 1.7, "Variant Type": "Double" }
                }
            }]
        }));
        assert_eq!(b.section(), Section::Transform);
        let p = b.param(ZOOM_X).expect("zoom");
        assert_eq!(
            p.keys.iter().map(|k| k.frame).collect::<Vec<_>>(),
            [3.0, 200.0, 424.0]
        );
        assert_eq!(p.keys[0].out_bez, Some([20.0, 0.1]));
        assert_eq!(p.keys[2].in_bez, Some([-49.75, 0.0]));
        let Some(Curve::Keys(ks)) = p.scalar() else {
            panic!("keys")
        };
        let interps: Vec<_> = ks.iter().map(|k| k.interp).collect();
        // 3 -> 200 has an OutBez, 200 -> 424 has an InBez at 424; the last key ends nothing.
        assert_eq!(interps, [Interp::Ease, Interp::Ease, Interp::Linear]);
    }

    #[test]
    fn zero_length_and_bare_handles() {
        let k = Key::parse(
            "5",
            &json!({ "Value": 1.0, "InBez": { "InBez": [0.0, 0.3] }, "OutBez": [2.0, 0.0] }),
        )
        .expect("key");
        assert_eq!(k.in_bez, None);
        assert_eq!(k.out_bez, Some([2.0, 0.0]));
    }

    #[test]
    fn curve_at_is_linear_between_keys() {
        let c = Curve::Keys(vec![
            Keyed {
                frame: 0.0,
                v: 0.0,
                interp: Interp::Linear,
            },
            Keyed {
                frame: 10.0,
                v: 1.0,
                interp: Interp::Hold,
            },
            Keyed {
                frame: 20.0,
                v: 0.0,
                interp: Interp::Linear,
            },
        ]);
        assert!(same(c.at(-5.0), 0.0));
        assert!(same(c.at(5.0), 0.5));
        assert!(same(c.at(15.0), 1.0));
        assert!(same(c.at(25.0), 0.0));
    }

    #[test]
    fn faders_on_constant_opacity_are_exact_keys() {
        let mut r = ImportReport::default();
        let c = faded(&Curve::Const(0.8), 12.0, 6.0, 48.0, &mut r);
        let Curve::Keys(ks) = c else { panic!("keys") };
        let pts: Vec<_> = ks.iter().map(|k| (k.frame, k.v)).collect();
        assert_eq!(pts, [(0.0, 0.0), (12.0, 0.8), (42.0, 0.8), (48.0, 0.0)]);
        assert_eq!(r.warnings, Vec::<String>::new());
    }

    #[test]
    fn transition_kinds() {
        let mut r = ImportReport::default();
        let m = map_transition(
            Some("SMPTE_Dissolve"),
            &json!({ "Effects": { "Effect Name": "Cross Fade +3 dB", "Type": 40, "Parameters": [] },
                     "Transition Type": "Cross Fade +3DB" }),
            &mut r,
        );
        assert_eq!(m["kind"], "cross_fade");
        assert_eq!(m["crossfade_db"], 3.0);
        assert_eq!(cross_fade_db("Cross Fade 0DB"), Some(0.0));
        assert_eq!(cross_fade_db("Cross Fade -3 dB"), Some(-3.0));
    }

    #[test]
    fn placement_centre_lands_at_a_minus_rsa_plus_t() {
        let pic = [1000.0, 500.0];
        let p = Placement {
            anchor: [0.25, -0.1],
            translate: [0.1, 0.2],
            scale: [2.0, 2.0],
            rotate_deg: 90.0,
            flip: [false, false],
        };
        let c = p.apply(pic, [0.0, 0.0]);
        // A = (250, -50); S·A = (500, -100); R(90° ccw, y down)·(500, -100) = (-100, -500).
        // A − R·S·A + T = (250 + 100 + 100, -50 + 500 + 100).
        assert!(same(c[0], 450.0) && same(c[1], 550.0), "{c:?}");
        // The anchor itself only moves by T.
        let a = p.apply(pic, [250.0, -50.0]);
        assert!(same(a[0], 350.0) && same(a[1], 50.0), "{a:?}");
    }

    #[test]
    fn picture_sizes() {
        let src = [3840.0, 2160.0];
        let frame = [1080.0, 1920.0];
        assert_eq!(Reframe::Fit.picture_size(src, frame, None), [1080.0, 607.5]);
        let fill = Reframe::Fill.picture_size(src, frame, None);
        assert!(
            same(fill[0], 3840.0 * 1920.0 / 2160.0) && same(fill[1], 1920.0),
            "{fill:?}"
        );
        assert_eq!(Reframe::Stretch.picture_size(src, frame, None), frame);
        assert_eq!(
            Reframe::Crop.picture_size(src, frame, Some([540.0, 960.0])),
            [7680.0, 4320.0]
        );
    }

    #[test]
    fn keyframe_times_are_clip_source_time() {
        let c = Curve::Keys(vec![Keyed {
            frame: 3.0,
            v: 1.0,
            interp: Interp::Ease,
        }]);
        let v = c.to_json(&ctx());
        assert_eq!(v[0]["t"]["value"], 103.0);
        assert_eq!(v[0]["t"]["rate"], 24.0);
        assert_eq!(v[0]["interp"], "ease");
    }
}
