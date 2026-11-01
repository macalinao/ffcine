//! `DaVinci Resolve` OTIO exporter (Resolve 18.5+, `File > Import > Timeline`): the inverse of
//! [`crate::resolve`].
//!
//! Editorial stays core OTIO. On every object, `metadata.ffcine.foreign.resolve` goes back to
//! `metadata.Resolve_OTIO`, a generator's `foreign.resolve_parameters` to
//! `parameters.Resolve_OTIO`, and a clip's `foreign.resolve_effects` (the sections the importer
//! didn't map) back to `Resolve Effect`s. `metadata.ffcine` stays too ([`ExportOptions::strip_ffcine`]
//! drops it): Resolve ignores and drops it, so ids merge back on our side.
//!
//! ffcine effects with a Resolve section become `Effect(effect_name: "Resolve Effect")` with
//! `metadata.Resolve_OTIO = {Effect Name, Type, Enabled, Name, Parameters[]}`:
//!
//! | ffcine effect | Resolve section (`Type`) |
//! |---|---|
//! | `ffcine.geo.reframe/1 {mode}` (Resolve modes) | Retime and Scaling (22) `scale` |
//! | `ffcine.geo.crop/1` | Cropping (3) |
//! | `ffcine.geo.transform/1` | Transform (2); Dynamic Zoom (59) if it came from one, unedited |
//! | `ffcine.comp.blend/1` | Composite (1) `opacity`, `composite mode` (+ Video Faders (36)) |
//! | `ffcine.audio.level/1` | Fairlight Clip Volume and Fades (62) |
//! | `ffcine.audio.pan/1` | Fairlight Clip Pan (72) |
//!
//! An effect whose parameters still read back from the section it was imported from is written
//! as that section, verbatim; an edited one is rebuilt on top of it (keeping its other
//! parameters), a new one from scratch. Keyframes go to `Key Frames` at frames from the clip's
//! first frame, linear (`ease` keys lose their curve). Sections are written in Resolve's own
//! order (Transform, Cropping, Dynamic Zoom, Composite, ..., Fairlight).
//!
//! Units: `"units": "picture"` geometry is Resolve's own and maps exactly. `"units": "frame"`
//! (FCPXML, MLT) is converted **assuming the picture fills the frame** (Resolve fit/fill with a
//! source of the frame's aspect, or stretch): picture fractions are then frame fractions. Resolve
//! stores no source size, so that is the best a file can do; the report says when it was assumed.
//! A reframe that isn't a Resolve mode (`zoom`, `punch`, ... from `edit.yaml`), the grade,
//! stabilization and the rest have no Resolve section: they stay ffcine effects, which Resolve
//! drops on import, and are reported.
//!
//! Every clip with a CDL (see [`super`]) also gets `metadata.cdl`; Resolve's OTIO import
//! ignores it, so use [`super::cdl_edl`] for the grade.

use std::path::Path;

use serde_json::{Map, Value, json};

use super::{ExportOptions, ExportReport, num, timeline_rate};
use crate::resolve::{
    self as imp, APP, BLEND_MODES, Block, ClipCtx, NAMESPACE, PRESERVED_EFFECTS,
    PRESERVED_PARAMETERS, Section,
};
use ffcine_core::Result;
use ffcine_timeline::{Clip, FFCINE, Timeline};

/// The ffcine effects that have a Resolve section.
const MAPPED: [&str; 6] = [
    imp::REFRAME,
    imp::CROP,
    imp::TRANSFORM,
    imp::BLEND,
    imp::LEVEL,
    imp::PAN,
];

/// Section `Type`s in the order Resolve writes them.
const ORDER: [u64; 12] = [2, 3, 59, 1, 43, 22, 36, 62, 72, 67, 64, 63];

/// The Resolve OTIO for `tl`.
pub fn export(tl: &Timeline, opts: &ExportOptions) -> Result<Timeline> {
    export_report(tl, opts).map(|(t, _)| t)
}

/// [`export`] and what it couldn't write exactly.
pub fn export_report(tl: &Timeline, opts: &ExportOptions) -> Result<(Timeline, ExportReport)> {
    let mut report = ExportReport::default();
    let project = tl.project().and_then(Result::ok);
    let rate = timeline_rate(tl, opts);
    let frame_size = super::frame_size(tl, opts);
    // CDL mirrors first, on the typed model.
    let mut tl = tl.clone();
    for_each_clip(&mut tl.tracks.children, &mut |c| {
        if let Some(cdl) = super::clip_cdl(c, project.as_ref(), &mut report) {
            c.metadata.insert("cdl".into(), cdl.mirror());
        }
    });
    let mut v = serde_json::to_value(&tl)?;
    let mut w = Writer {
        rate,
        frame_size,
        opts,
        report: &mut report,
    };
    w.walk(&mut v);
    if let Some(md) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        md.entry(NAMESPACE)
            .or_insert_with(|| json!({ "Resolve OTIO Meta Version": "1.0" }));
    }
    Ok((serde_json::from_value(v)?, report))
}

/// Writes [`export`] to `path` as OTIO JSON.
pub fn to_file(tl: &Timeline, opts: &ExportOptions, path: impl AsRef<Path>) -> Result<()> {
    export(tl, opts)?.to_file(path)
}

fn for_each_clip(items: &mut [ffcine_timeline::Item], f: &mut dyn FnMut(&mut Clip)) {
    use ffcine_timeline::Item;
    for i in items {
        match i {
            Item::Clip(c) => f(c),
            Item::Stack(s) => for_each_clip(&mut s.children, f),
            Item::Track(t) => for_each_clip(&mut t.children, f),
            _ => {}
        }
    }
}

struct Writer<'a> {
    rate: f64,
    frame_size: Option<(u32, u32)>,
    opts: &'a ExportOptions,
    report: &'a mut ExportReport,
}

fn obj<'m>(m: &'m mut Map<String, Value>, key: &str) -> &'m mut Map<String, Value> {
    let v = m.entry(key).or_insert_with(|| Value::Object(Map::new()));
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    v.as_object_mut().unwrap_or_else(|| unreachable!())
}

/// Removes `metadata.ffcine.foreign.<key>`.
fn take_foreign(o: &mut Map<String, Value>, key: &str) -> Option<Value> {
    o.get_mut("metadata")?
        .get_mut(FFCINE)?
        .get_mut("foreign")?
        .as_object_mut()?
        .remove(key)
}

fn foreign(o: &Map<String, Value>, key: &str) -> Option<Value> {
    o.get("metadata")?
        .get(FFCINE)?
        .get("foreign")?
        .get(key)
        .cloned()
}

/// Resolve's wrapper for a section.
#[allow(clippy::needless_pass_by_value)]
fn wrap(block: Value) -> Value {
    json!({
        "OTIO_SCHEMA": "Effect.1",
        "metadata": { NAMESPACE: block },
        "name": "",
        "effect_name": "Resolve Effect",
    })
}

fn type_rank(b: &Value) -> usize {
    b.get("Type")
        .and_then(Value::as_u64)
        .and_then(|t| ORDER.iter().position(|&o| o == t))
        .unwrap_or(ORDER.len())
}

impl Writer<'_> {
    fn walk(&mut self, v: &mut Value) {
        match v {
            Value::Object(o) => {
                let schema = o
                    .get("OTIO_SCHEMA")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let base = schema.split('.').next().unwrap_or("");
                if matches!(base, "Clip" | "Stack") {
                    self.item(o);
                }
                for (k, child) in o.iter_mut() {
                    if k != "metadata" {
                        self.walk(child);
                    }
                }
                if !base.is_empty() {
                    self.restore(base, o);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.walk(x)),
            _ => {}
        }
    }

    /// `foreign.resolve` -> `Resolve_OTIO`, and the generator's parameters.
    fn restore(&mut self, base: &str, o: &mut Map<String, Value>) {
        if base == "GeneratorReference"
            && let Some(p) = foreign(o, PRESERVED_PARAMETERS)
        {
            obj(o, "parameters").insert(NAMESPACE.into(), p);
        }
        let is_ffcine_effect = base == "Effect"
            && o.get("metadata")
                .and_then(|m| m.get(FFCINE))
                .and_then(|f| f.get("schema"))
                .is_some();
        if !is_ffcine_effect && let Some(ns) = foreign(o, APP) {
            obj(o, "metadata").insert(NAMESPACE.into(), ns);
        }
        if self.opts.strip_ffcine
            && let Some(md) = o.get_mut("metadata").and_then(Value::as_object_mut)
        {
            md.remove(FFCINE);
        }
    }

    fn ctx(&self, o: &Map<String, Value>) -> ClipCtx {
        let r = o.get("source_range");
        let get = |a: &str, b: &str| {
            r.and_then(|r| r.get(a))
                .and_then(|t| t.get(b))
                .and_then(Value::as_f64)
        };
        let rate = get("start_time", "rate")
            .or(get("duration", "rate"))
            .filter(|r| *r > 0.0)
            .unwrap_or(self.rate);
        let duration = match (get("duration", "value"), get("duration", "rate")) {
            (Some(v), Some(dr)) if dr > 0.0 => v * rate / dr,
            (Some(v), _) => v,
            _ => 0.0,
        };
        ClipCtx {
            rate,
            start: get("start_time", "value").unwrap_or(0.0),
            duration,
            frame_size: self.frame_size,
        }
    }

    /// A Clip's or Stack's effects.
    fn item(&mut self, o: &mut Map<String, Value>) {
        let ctx = self.ctx(o);
        let effects = match o.get_mut("effects") {
            Some(Value::Array(a)) => std::mem::take(a),
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        let mut blocks = Vec::new();
        let mut at = None;
        for e in effects {
            let name = e
                .pointer("/metadata/ffcine/schema")
                .and_then(Value::as_str)
                .and_then(ffcine_timeline::metadata::split_ffcine_schema);
            match name {
                Some((n, 1)) if MAPPED.contains(&n) => match self.blocks_for(n, &e, &ctx) {
                    Some(bs) => {
                        at.get_or_insert(out.len());
                        blocks.extend(bs);
                    }
                    None => out.push(e),
                },
                Some((n, _)) => {
                    self.report.unmapped([format!("{n} (no Resolve section)")]);
                    out.push(e);
                }
                None => out.push(e),
            }
        }
        if let Some(Value::Array(p)) = take_foreign(o, PRESERVED_EFFECTS) {
            blocks.extend(p);
        }
        blocks.sort_by_key(type_rank);
        let at = at.unwrap_or(out.len());
        out.splice(at..at, blocks.into_iter().map(wrap));
        o.insert("effects".into(), Value::Array(out));
    }

    /// The sections for an ffcine effect, or `None` to keep it as it is.
    fn blocks_for(&mut self, name: &str, e: &Value, ctx: &ClipCtx) -> Option<Vec<Value>> {
        let ff = e.pointer("/metadata/ffcine")?.as_object()?;
        let sources: Vec<Value> = match ff.get("foreign").and_then(|f| f.get(APP)) {
            Some(Value::Array(a)) => a.clone(),
            Some(v @ Value::Object(_)) => vec![v.clone()],
            _ => Vec::new(),
        };
        let mut params = ff.clone();
        params.shift_remove("schema");
        params.shift_remove("foreign");
        // A crop reframe carries the frame size it was read at.
        let sized;
        let ctx = match params
            .get("timeline_size")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        {
            Some([w, h]) => {
                let dim = |v: &Value| v.as_u64().and_then(|x| u32::try_from(x).ok());
                sized = ClipCtx {
                    frame_size: dim(w).zip(dim(h)).or(ctx.frame_size),
                    ..*ctx
                };
                &sized
            }
            _ => ctx,
        };
        // Unedited: the sections it came from, verbatim.
        let parsed: Vec<(Block, Value)> = sources
            .iter()
            .filter_map(|s| Some((Block::parse(s)?, s.clone())))
            .collect();
        if !parsed.is_empty() {
            let (mapped, _) = imp::map_sections(&parsed, ctx, &mut ExportReport::default());
            if mapped.iter().any(|m| {
                m.effect_name == name
                    && same_json(
                        &Value::Object(m.params.clone()),
                        &Value::Object(params.clone()),
                    )
            }) {
                return Some(sources);
            }
        }
        let template = |s: Section| {
            parsed
                .iter()
                .find(|(b, _)| b.section() == s)
                .map(|(_, raw)| raw.clone())
        };
        let mut p = Params {
            ff: &params,
            ctx,
            report: &mut *self.report,
        };
        match name {
            imp::REFRAME => p.reframe(template(Section::RetimeScaling)).map(|b| vec![b]),
            imp::CROP => Some(vec![p.crop(template(Section::Cropping))]),
            imp::TRANSFORM => {
                if parsed
                    .iter()
                    .any(|(b, _)| b.section() == Section::DynamicZoom)
                {
                    p.report.warn(
                        "an edited Dynamic Zoom transform can't be written back: kept as an \
                         ffcine effect (Resolve drops it)",
                    );
                    return None;
                }
                Some(vec![p.transform(template(Section::Transform))])
            }
            imp::BLEND => {
                let mut out = vec![p.blend(template(Section::Composite))];
                if let Some(mut f) = template(Section::VideoFaders) {
                    // The fades are in the opacity keys now.
                    for id in ["videoFaderIn", "videoFaderOut"] {
                        set(&mut f, id, "Double", Some(json!(0.0)), None, json!(0.0));
                    }
                    out.push(f);
                }
                Some(out)
            }
            imp::LEVEL => Some(vec![p.level(template(Section::ClipVolume))]),
            imp::PAN => Some(vec![p.pan(template(Section::ClipPan))]),
            _ => None,
        }
    }
}

/// JSON equality with numbers within 1e-9 (relative).
fn same_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => {
            let (x, y) = (
                a.as_f64().unwrap_or(f64::NAN),
                b.as_f64().unwrap_or(f64::NAN),
            );
            (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_json(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| same_json(v, w)))
        }
        _ => a == b,
    }
}

fn new_block(name: &str, kind: u64) -> Value {
    json!({ "Effect Name": name, "Enabled": true, "Name": name, "Parameters": [], "Type": kind })
}

/// Sets parameter `id` of a section: `value` (or `keys`, frame -> value), with `default` used
/// when both are absent.
#[allow(clippy::needless_pass_by_value)]
fn set(
    block: &mut Value,
    id: &str,
    variant: &str,
    value: Option<Value>,
    keys: Option<Vec<(f64, Value)>>,
    default: Value,
) {
    let Some(params) = block
        .as_object_mut()
        .map(|b| b.entry("Parameters").or_insert_with(|| json!([])))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let i = params
        .iter()
        .position(|p| p.get("Parameter ID").and_then(Value::as_str) == Some(id))
        .unwrap_or_else(|| {
            params.push(json!({
                "Default Parameter Value": default,
                "Key Frames": {},
                "Parameter ID": id,
                "Parameter Value": default,
                "Variant Type": variant,
            }));
            params.len() - 1
        });
    let p = &mut params[i];
    let variant = p
        .get("Variant Type")
        .and_then(Value::as_str)
        .unwrap_or(variant)
        .to_owned();
    let kf: Map<String, Value> = keys
        .iter()
        .flatten()
        .map(|(f, v)| {
            (
                num(*f).trim_end_matches(".0").to_owned(),
                json!({ "Value": v, "Variant Type": variant }),
            )
        })
        .collect();
    let first = keys
        .as_ref()
        .and_then(|k| k.first())
        .map(|(_, v)| v.clone());
    p["Parameter Value"] = value.or(first).unwrap_or(default);
    p["Key Frames"] = Value::Object(kf);
}

/// Reads ffcine parameters for one effect.
struct Params<'a> {
    ff: &'a Map<String, Value>,
    ctx: &'a ClipCtx,
    report: &'a mut ExportReport,
}

/// A converted parameter: constant or keys.
enum Conv {
    Const(Value),
    Keys(Vec<(f64, Value)>),
}

impl Params<'_> {
    /// `key` through `f`, keys at frames from the clip's first frame.
    fn get(&mut self, key: &str, f: &dyn Fn(&[f64]) -> Value) -> Option<Conv> {
        let v = self.ff.get(key)?;
        let start = self.ctx.start / self.ctx.rate;
        match super::Anim::read(v, start, self.ctx.rate)? {
            super::Anim::Const(x) => Some(Conv::Const(f(&x))),
            super::Anim::Keys(ks) => {
                if ks.iter().any(|k| k.interp == "ease") {
                    self.report
                        .warn("eased keyframes written as linear Resolve keys (approx)");
                }
                Some(Conv::Keys(ks.iter().map(|k| (k.frame, f(&k.v))).collect()))
            }
        }
    }

    fn put(
        &mut self,
        b: &mut Value,
        key: &str,
        id: &str,
        variant: &str,
        default: Value,
        f: &dyn Fn(&[f64]) -> Value,
    ) {
        match self.get(key, f) {
            Some(Conv::Const(v)) => set(b, id, variant, Some(v), None, default),
            Some(Conv::Keys(k)) => set(b, id, variant, None, Some(k), default),
            None => set(b, id, variant, None, None, default),
        }
    }

    fn frame_units(&mut self) -> bool {
        let frame = self.ff.get("units").and_then(Value::as_str) == Some("frame");
        if frame {
            self.report.warn(
                "frame-relative geometry written to Resolve assuming the picture fills the frame \
                 (exact for stretch, or fit/fill with a source of the frame's aspect)",
            );
        }
        frame
    }

    fn reframe(&mut self, t: Option<Value>) -> Option<Value> {
        let code = match self.ff.get("mode").and_then(Value::as_str) {
            Some("crop") => 1,
            Some("fit") => 2,
            Some("fill") => 3,
            Some("stretch") => 4,
            _ => {
                self.report.unmapped([
                    "ffcine.geo.reframe/1 (not a Resolve sizing mode; kept as an ffcine effect)"
                        .to_owned(),
                ]);
                return None;
            }
        };
        let mut b = t.unwrap_or_else(|| new_block("Retime and Scaling", 22));
        set(&mut b, "scale", "Int", Some(json!(code)), None, json!(0));
        Some(b)
    }

    fn crop(&mut self, t: Option<Value>) -> Value {
        let mut b = t.unwrap_or_else(|| new_block("Cropping", 3));
        for (key, id) in [
            ("left", "cropLeft"),
            ("right", "cropRight"),
            ("top", "cropTop"),
            ("bottom", "cropBottom"),
        ] {
            self.put(&mut b, key, id, "Double", json!(0.0), &|v| json!(v[0]));
        }
        self.put(
            &mut b,
            "feather",
            "cropSoftness",
            "Double",
            json!(0.0),
            &|v| json!(v[0] * 400.0),
        );
        if self.ff.get("fill").and_then(Value::as_bool) == Some(true) {
            self.report.warn(
                "crop fill (scale the crop back up to the frame) has no Resolve parameter: left out",
            );
        }
        b
    }

    fn transform(&mut self, t: Option<Value>) -> Value {
        self.frame_units();
        let mut b = t.unwrap_or_else(|| new_block("Transform", 2));
        let s = |v: &[f64]| json!(v[0]);
        let neg = |v: &[f64]| json!(0.0 - v[0]);
        self.put(
            &mut b,
            "translate_x",
            "transformationPan",
            "Double",
            json!(0.0),
            &s,
        );
        self.put(
            &mut b,
            "translate_y",
            "transformationTilt",
            "Double",
            json!(0.0),
            &neg,
        );
        self.put(
            &mut b,
            "scale_x",
            "transformationZoomX",
            "Double",
            json!(1.0),
            &s,
        );
        self.put(
            &mut b,
            "scale_y",
            "transformationZoomY",
            "Double",
            json!(1.0),
            &s,
        );
        self.put(
            &mut b,
            "rotate_deg",
            "transformationRotationAngle",
            "Double",
            json!(0.0),
            &s,
        );
        self.put(
            &mut b,
            "anchor",
            "transformationAnchorPoint",
            "POINTF",
            json!([0.0, 0.0]),
            &|v| json!([v[0], 0.0 - v.get(1).copied().unwrap_or(0.0)]),
        );
        let sx = self.ff.get("scale_x");
        let sy = self.ff.get("scale_y");
        if sx.is_some() || sy.is_some() {
            let linked = sx == sy;
            set(
                &mut b,
                "transformationZoomLink",
                "Bool",
                Some(json!(linked)),
                None,
                json!(true),
            );
        }
        for (key, id) in [
            ("flip_x", "transformationFlipX"),
            ("flip_y", "transformationFlipY"),
        ] {
            let on = self.ff.get(key).and_then(Value::as_bool).unwrap_or(false);
            set(&mut b, id, "Bool", Some(json!(on)), None, json!(false));
        }
        for old in ["position", "scale", "rotation"] {
            if self.ff.contains_key(old) {
                self.report.unmapped([format!(
                    "ffcine.geo.transform/1.{old} (pre-canonical field; use translate_*/scale_*/rotate_deg)"
                )]);
            }
        }
        b
    }

    fn blend(&mut self, t: Option<Value>) -> Value {
        let mut b = t.unwrap_or_else(|| new_block("Composite", 1));
        self.put(&mut b, "opacity", "opacity", "Double", json!(100.0), &|v| {
            json!(v[0] * 100.0)
        });
        let mode = self
            .ff
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("normal");
        let code = BLEND_MODES
            .iter()
            .position(|m| *m == mode)
            .unwrap_or_else(|| {
                self.report.warn(format!(
                    "blend mode {mode:?} has no Resolve composite mode: normal"
                ));
                0
            });
        set(
            &mut b,
            "composite mode",
            "Int",
            Some(json!(code)),
            None,
            json!(0),
        );
        b
    }

    fn level(&mut self, t: Option<Value>) -> Value {
        let mut b = t.unwrap_or_else(|| new_block("Fairlight Clip Volume and Fades", 62));
        self.put(&mut b, "gain_db", "volume", "Double", json!(0.0), &|v| {
            json!(v[0])
        });
        let rate = self.ctx.rate;
        for (key, id) in [("fade_in", "faderIn"), ("fade_out", "faderOut")] {
            let secs = self.ff.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            set(
                &mut b,
                id,
                "Double",
                Some(json!(secs * rate)),
                None,
                json!(0.0),
            );
        }
        for k in ["mode", "duck_db", "nat_db", "lufs"] {
            if self.ff.contains_key(k) {
                self.report
                    .unmapped([format!("ffcine.audio.level/1.{k} (no Resolve parameter)")]);
            }
        }
        b
    }

    fn pan(&mut self, t: Option<Value>) -> Value {
        let mut b = t.unwrap_or_else(|| new_block("Fairlight Clip Pan", 72));
        self.put(&mut b, "pan", "pan", "Double", json!(0.0), &|v| {
            json!(v[0] * 100.0)
        });
        b
    }
}
