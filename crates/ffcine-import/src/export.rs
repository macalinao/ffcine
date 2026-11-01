//! Exporters: an ffcine [`Timeline`] -> files other apps open, so a cut can be finished there
//! (`docs/import.md`, "Export"). Each is the inverse of the importer of the same name.
//!
//! - [`resolve`]: OTIO JSON for `DaVinci Resolve` 18.5+ (`File > Import > Timeline`), with
//!   `Resolve_OTIO` restored and ffcine geometry, blend and audio effects written back as
//!   `Resolve Effect`s.
//! - [`fcpxml`]: FCPXML 1.10, the second route into Resolve (and Final Cut Pro).
//! - [`cdl_edl`] and [`ccc`]: the primary grade as ASC CDL. Resolve's OTIO import ignores grades;
//!   a CMX 3600 EDL with `*ASC_SOP` / `*ASC_SAT` comments (or a `.ccc` collection) is how it takes
//!   them.
//!
//! A clip's CDL is its `ffcine.color.cdl/1` effect, else its `metadata.cdl` (the CMX adapter
//! shape), else its resolved grade ([`Project::resolve_grade`]) approximated as a CDL by
//! [`grade_cdl`]. The exporters also write it as `metadata.cdl` on each clip.
//!
//! Exporters never fail on something they can't express: it stays as-is (an ffcine effect Resolve
//! ignores) or is left out, and the [`ExportReport`] says so.

pub mod fcpxml;
pub mod resolve;

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::ImportReport;
use ffcine_core::{Error, Result};
use ffcine_timeline::{
    Clip, ClipGrade, Effect, GradeParams, HasMetadata, Item, MediaReference, Project, SourceKind,
    Timeline, track_kind,
};

/// Settings for the exporters.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions {
    /// Output frame size when the timeline has no `metadata.ffcine.frame_size`.
    pub frame_size: Option<(u32, u32)>,
    /// Frame rate when the timeline has none (no `global_start_time`, project rate or clips).
    pub rate: Option<f64>,
    /// Drop `metadata.ffcine` from the output. Off by default: it is harmless to Resolve (which
    /// drops it) and lets the file be read back by ffcine as-is.
    pub strip_ffcine: bool,
}

/// What an export couldn't write exactly: approximations (`warnings`) and data left out
/// (`unmapped`).
pub type ExportReport = ImportReport;

pub(crate) fn write_file(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

// ---------------------------------------------------------------------------------------------
// Timeline facts
// ---------------------------------------------------------------------------------------------

/// The timeline's frame rate: `global_start_time`, the project's, the first clip's, the option.
pub(crate) fn timeline_rate(tl: &Timeline, opts: &ExportOptions) -> f64 {
    tl.global_start_time
        .as_ref()
        .map(|t| t.rate)
        .or_else(|| tl.project().and_then(Result::ok).and_then(|p| p.rate))
        .or_else(|| {
            tl.clips()
                .iter()
                .find_map(|c| c.source_range.as_ref().map(|r| r.duration.rate))
        })
        .or(opts.rate)
        .filter(|r| *r > 0.0)
        .unwrap_or(24.0)
}

/// `metadata.ffcine.frame_size`, else the option.
pub(crate) fn frame_size(tl: &Timeline, opts: &ExportOptions) -> Option<(u32, u32)> {
    let fs = tl.ffcine_value("frame_size").and_then(Value::as_array);
    let dim = |i: usize| {
        fs.and_then(|a| a.get(i))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    match (dim(0), dim(1)) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => opts.frame_size,
    }
}

/// Length of an item on its parent, in frames at `rate`.
pub(crate) fn item_frames(item: &Item, rate: f64) -> f64 {
    match item {
        Item::Transition(_) | Item::Unknown(_) => 0.0,
        Item::Stack(s) => s.source_range.as_ref().map_or_else(
            || {
                s.children
                    .iter()
                    .map(|c| item_frames(c, rate))
                    .fold(0.0, f64::max)
            },
            |r| r.duration.rescaled_to(rate).value,
        ),
        Item::Track(t) => t.source_range.as_ref().map_or_else(
            || t.children.iter().map(|c| item_frames(c, rate)).sum(),
            |r| r.duration.rescaled_to(rate).value,
        ),
        _ => item
            .source_range()
            .map_or(0.0, |r| r.duration.rescaled_to(rate).value),
    }
}

// ---------------------------------------------------------------------------------------------
// Animated parameters
// ---------------------------------------------------------------------------------------------

/// One key of an ffcine parameter, at frames from the clip's first frame.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Key {
    pub frame: f64,
    pub v: Vec<f64>,
    pub interp: String,
}

/// An ffcine parameter as numbers (a scalar is one component): constant or keyframed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Anim {
    Const(Vec<f64>),
    Keys(Vec<Key>),
}

fn numbers(v: &Value) -> Option<Vec<f64>> {
    match v {
        Value::Number(n) => n.as_f64().map(|x| vec![x]),
        Value::Array(a) => a.iter().map(Value::as_f64).collect(),
        _ => None,
    }
}

/// Whether `v` is a keyframe list `[{t, v, interp}]`.
pub(crate) fn is_keys(v: &Value) -> bool {
    v.as_array()
        .and_then(|a| a.first())
        .is_some_and(|k| k.get("t").is_some())
}

impl Anim {
    /// Reads a parameter. Key `t` is in the clip's source time (`docs/timeline.md`):
    /// `start` is the clip's `source_range.start_time` in seconds, `rate` the frame rate keys
    /// are counted in.
    pub fn read(v: &Value, start: f64, rate: f64) -> Option<Self> {
        if !is_keys(v) {
            return numbers(v).map(Self::Const);
        }
        let mut keys = Vec::new();
        for k in v.as_array()? {
            let t = k.get("t")?;
            let secs = t.get("value")?.as_f64()? / t.get("rate")?.as_f64().filter(|r| *r > 0.0)?;
            keys.push(Key {
                frame: (secs - start) * rate,
                v: numbers(k.get("v")?)?,
                interp: k
                    .get("interp")
                    .and_then(Value::as_str)
                    .unwrap_or("linear")
                    .to_owned(),
            });
        }
        keys.sort_by(|a, b| a.frame.total_cmp(&b.frame));
        Some(Self::Keys(keys))
    }

    #[must_use]
    pub fn map(&self, f: impl Fn(&[f64]) -> Vec<f64>) -> Self {
        match self {
            Self::Const(v) => Self::Const(f(v)),
            Self::Keys(ks) => Self::Keys(
                ks.iter()
                    .map(|k| Key {
                        frame: k.frame,
                        v: f(&k.v),
                        interp: k.interp.clone(),
                    })
                    .collect(),
            ),
        }
    }

    /// The value at a frame, segments read as lines (holds hold).
    pub fn at(&self, frame: f64) -> Vec<f64> {
        let ks = match self {
            Self::Const(v) => return v.clone(),
            Self::Keys(ks) => ks,
        };
        let Some(first) = ks.first() else {
            return Vec::new();
        };
        if frame <= first.frame {
            return first.v.clone();
        }
        for w in ks.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if frame <= b.frame {
                if a.interp == "hold" || b.frame <= a.frame {
                    return a.v.clone();
                }
                let u = (frame - a.frame) / (b.frame - a.frame);
                return a.v.iter().zip(&b.v).map(|(x, y)| x + (y - x) * u).collect();
            }
        }
        ks.last().map_or_else(Vec::new, |k| k.v.clone())
    }

    /// Components side by side (`[x]`, `[y]` -> `[x, y]`), keyed at the union of the parts' keys.
    pub fn zip(parts: &[Self]) -> Self {
        let mut frames: Vec<(f64, String)> = parts
            .iter()
            .filter_map(|p| match p {
                Self::Keys(ks) => Some(ks.iter().map(|k| (k.frame, k.interp.clone()))),
                Self::Const(_) => None,
            })
            .flatten()
            .collect();
        if frames.is_empty() {
            return Self::Const(parts.iter().flat_map(|p| p.at(0.0)).collect());
        }
        frames.sort_by(|a, b| a.0.total_cmp(&b.0));
        frames.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-9);
        Self::Keys(
            frames
                .into_iter()
                .map(|(f, interp)| Key {
                    frame: f,
                    v: parts.iter().flat_map(|p| p.at(f)).collect(),
                    interp,
                })
                .collect(),
        )
    }
}

/// The clip's `source_range.start_time` in seconds.
pub(crate) fn clip_start(clip: &Clip) -> f64 {
    clip.source_range
        .as_ref()
        .map_or(0.0, |r| r.start_time.to_seconds())
}

/// `metadata.ffcine` of an ffcine effect (`schema` set) with its name: `(name, params)`.
pub(crate) fn ffcine_effect(e: &Effect) -> Option<(&str, &Map<String, Value>)> {
    let Effect::Effect(g) = e else { return None };
    let (name, _) = g.ffcine_schema()?;
    Some((name, g.ffcine()?))
}

// ---------------------------------------------------------------------------------------------
// CDL
// ---------------------------------------------------------------------------------------------

/// An ASC CDL: `out = (in × slope + offset) ^ power`, then saturation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cdl {
    pub slope: [f64; 3],
    pub offset: [f64; 3],
    pub power: [f64; 3],
    pub saturation: Option<f64>,
}

impl Default for Cdl {
    fn default() -> Self {
        Self {
            slope: [1.0; 3],
            offset: [0.0; 3],
            power: [1.0; 3],
            saturation: None,
        }
    }
}

fn triple(v: Option<&Value>, def: f64) -> [f64; 3] {
    match v.and_then(numbers).as_deref() {
        Some([r, g, b, ..]) => [*r, *g, *b],
        Some([x]) => [*x; 3],
        _ => [def; 3],
    }
}

impl Cdl {
    /// From `ffcine.color.cdl/1` parameters.
    pub fn from_params(p: &Map<String, Value>) -> Self {
        Self {
            slope: triple(p.get("slope"), 1.0),
            offset: triple(p.get("offset"), 0.0),
            power: triple(p.get("power"), 1.0),
            saturation: p.get("saturation").and_then(Value::as_f64),
        }
    }

    /// From `metadata.cdl = {asc_sop: {slope, offset, power}, asc_sat}`.
    pub fn from_mirror(m: &Value) -> Option<Self> {
        let sop = m.get("asc_sop")?;
        Some(Self {
            slope: triple(sop.get("slope"), 1.0),
            offset: triple(sop.get("offset"), 0.0),
            power: triple(sop.get("power"), 1.0),
            saturation: m.get("asc_sat").and_then(Value::as_f64),
        })
    }

    /// `metadata.cdl`, the CMX adapter's shape.
    pub fn mirror(&self) -> Value {
        let mut m = json!({ "asc_sop": {
            "slope": self.slope, "offset": self.offset, "power": self.power } });
        if let Some(s) = self.saturation {
            m["asc_sat"] = json!(s);
        }
        m
    }

    /// `(s s s)(o o o)(p p p)`.
    pub fn sop_text(&self) -> String {
        let f = |v: [f64; 3]| format!("({} {} {})", num(v[0]), num(v[1]), num(v[2]));
        format!("{}{}{}", f(self.slope), f(self.offset), f(self.power))
    }
}

/// A short decimal: up to 6 places, trailing zeros trimmed, `1.0` for integers.
pub(crate) fn num(v: f64) -> String {
    let s = format!("{:.6}", if v.abs() < 5e-7 { 0.0 } else { v });
    let s = s.trim_end_matches('0');
    if s.ends_with('.') {
        format!("{s}0")
    } else {
        s.to_owned()
    }
}

/// A grade as the nearest CDL (approx): slope `rgb_gain × gain × 2^exposure`, offset `lift`,
/// power `1/gamma`, saturation. The LUT, contrast, warmth, tint, denoise and sharpening have no
/// CDL equivalent and are left out. `None` for an identity.
pub fn grade_cdl(g: &GradeParams) -> Option<Cdl> {
    let rgb = g.rgb_gain.unwrap_or([1.0; 3]);
    let k = g.gain.unwrap_or(1.0) * g.exposure.unwrap_or(0.0).exp2();
    let lift = g.lift.unwrap_or(0.0);
    let power = g.gamma.filter(|x| *x > 0.0).map_or(1.0, |x| 1.0 / x);
    let cdl = Cdl {
        slope: rgb.map(|c| c * k),
        offset: [lift; 3],
        power: [power; 3],
        saturation: g.saturation.filter(|s| (s - 1.0).abs() > 1e-9),
    };
    (cdl != Cdl::default()).then_some(cdl)
}

/// A clip's CDL (see the module docs); `report` notes grade approximations.
pub fn clip_cdl(clip: &Clip, project: Option<&Project>, report: &mut ExportReport) -> Option<Cdl> {
    let mut grade = None;
    for e in &clip.effects {
        match ffcine_effect(e) {
            Some(("ffcine.color.cdl", p)) => return Some(Cdl::from_params(p)),
            Some(("ffcine.color.grade", p)) => {
                let mut p = p.clone();
                p.shift_remove("schema");
                p.shift_remove("foreign");
                grade = serde_json::from_value::<ClipGrade>(Value::Object(p)).ok();
            }
            _ => {}
        }
    }
    if let Some(c) = clip.metadata.get("cdl").and_then(Cdl::from_mirror) {
        return Some(c);
    }
    let project = project?;
    let kind = match clip
        .ffcine_value("source")
        .and_then(|s| s.get("kind"))
        .and_then(Value::as_str)
    {
        Some("dji") => SourceKind::Log,
        _ => SourceKind::Display,
    };
    let g = project.resolve_grade(grade.as_ref(), kind).ok()?;
    let cdl = grade_cdl(&g)?;
    report.warn(
        "grade written as an approximate CDL (slope/offset/power/sat from gain, exposure, lift, \
         gamma, saturation; LUT, contrast and colour balance left out)",
    );
    Some(cdl)
}

/// Video clips with a media file, in track order: `(clip, record start frame)`.
fn video_events(tl: &Timeline, rate: f64) -> Vec<(&Clip, f64)> {
    fn walk<'a>(items: &'a [Item], rate: f64, at: f64, out: &mut Vec<(&'a Clip, f64)>) {
        let mut cursor = at;
        for i in items {
            match i {
                Item::Clip(c)
                    if !matches!(c.media_reference(), Some(MediaReference::Generator(_))) =>
                {
                    out.push((c, cursor));
                }
                Item::Stack(s) => {
                    for t in &s.children {
                        if let Item::Track(t) = t {
                            walk(&t.children, rate, cursor, out);
                        }
                    }
                }
                _ => {}
            }
            cursor += item_frames(i, rate);
        }
    }
    let mut out = Vec::new();
    for t in tl.track_list().filter(|t| t.kind == track_kind::VIDEO) {
        walk(&t.children, rate, 0.0, &mut out);
    }
    out
}

/// `HH:MM:SS:FF` at the nominal (rounded) rate, non-drop.
pub(crate) fn timecode(frames: f64, rate: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let fps = (rate.round() as i64).max(1);
    #[allow(clippy::cast_possible_truncation)]
    let f = (frames.round() as i64).max(0);
    let (h, m, s, ff) = (f / (fps * 3600), f / (fps * 60) % 60, f / fps % 60, f % fps);
    format!("{h:02}:{m:02}:{s:02}:{ff:02}")
}

/// A CMX 3600 EDL of the video clips (all video tracks, bottom first), with `*ASC_SOP` and
/// `*ASC_SAT` comment lines on each event that has a CDL. Record times start at the timeline's
/// `global_start_time`; source times are the clips' media in and out points; reels are
/// `metadata.ffcine.source.reel`, else `AX`.
pub fn cdl_edl(tl: &Timeline) -> String {
    let rate = timeline_rate(tl, &ExportOptions::default());
    let project = tl.project().and_then(Result::ok);
    let mut report = ExportReport::default();
    let rec0 = tl
        .global_start_time
        .as_ref()
        .map_or(0.0, |t| t.rescaled_to(rate).value);
    let mut out = String::new();
    let title = if tl.name.is_empty() {
        "ffcine"
    } else {
        &tl.name
    };
    let _ = writeln!(out, "TITLE: {title}\nFCM: NON-DROP FRAME\n");
    for (n, (clip, at)) in video_events(tl, rate).into_iter().enumerate() {
        let (src_in, dur) = clip.source_range.as_ref().map_or((0.0, 0.0), |r| {
            (
                r.start_time.rescaled_to(rate).value,
                r.duration.rescaled_to(rate).value,
            )
        });
        let reel: String = clip
            .ffcine_value("source")
            .and_then(|s| s.get("reel"))
            .and_then(Value::as_str)
            .unwrap_or("AX")
            .chars()
            .filter(|c| !c.is_whitespace())
            .take(8)
            .collect();
        let _ = writeln!(
            out,
            "{:03}  {reel:<8} V     C        {} {} {} {}",
            n + 1,
            timecode(src_in, rate),
            timecode(src_in + dur, rate),
            timecode(rec0 + at, rate),
            timecode(rec0 + at + dur, rate),
        );
        let _ = writeln!(out, "* FROM CLIP NAME: {}", clip.name);
        if let Some(MediaReference::External(e)) = clip.media_reference() {
            let src = e
                .path()
                .map_or(e.target_url.clone(), |p| p.display().to_string());
            let _ = writeln!(out, "* SOURCE FILE: {src}");
        }
        if let Some(cdl) = clip_cdl(clip, project.as_ref(), &mut report) {
            let _ = writeln!(out, "*ASC_SOP {}", cdl.sop_text());
            let _ = writeln!(out, "*ASC_SAT {}", num(cdl.saturation.unwrap_or(1.0)));
        }
        out.push('\n');
    }
    out
}

pub(crate) fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            '\n' => o.push_str("&#10;"),
            c => o.push(c),
        }
    }
    o
}

/// An ASC `ColorCorrectionCollection` (`.ccc`): one `ColorCorrection` per clip with a CDL, its
/// `id` the clip's `metadata.ffcine.id` (else its name), in [`cdl_edl`]'s order.
pub fn ccc(tl: &Timeline) -> String {
    let rate = timeline_rate(tl, &ExportOptions::default());
    let project = tl.project().and_then(Result::ok);
    let mut report = ExportReport::default();
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ColorCorrectionCollection \
         xmlns=\"urn:ASC:CDL:v1.01\">\n",
    );
    let three = |v: [f64; 3]| format!("{} {} {}", num(v[0]), num(v[1]), num(v[2]));
    for (clip, _) in video_events(tl, rate) {
        let Some(cdl) = clip_cdl(clip, project.as_ref(), &mut report) else {
            continue;
        };
        let id = clip.ffcine_id().unwrap_or(&clip.name);
        let _ = write!(
            out,
            "    <ColorCorrection id=\"{}\">\n        <SOPNode>\n            \
             <Description>{}</Description>\n            <Slope>{}</Slope>\n            \
             <Offset>{}</Offset>\n            <Power>{}</Power>\n        </SOPNode>\n        \
             <SatNode>\n            <Saturation>{}</Saturation>\n        </SatNode>\n    \
             </ColorCorrection>\n",
            xml_escape(id),
            xml_escape(&clip.name),
            three(cdl.slope),
            three(cdl.offset),
            three(cdl.power),
            num(cdl.saturation.unwrap_or(1.0)),
        );
    }
    out.push_str("</ColorCorrectionCollection>\n");
    out
}

/// Writes [`cdl_edl`] to `path`.
pub fn cdl_edl_to_file(tl: &Timeline, path: impl AsRef<Path>) -> Result<()> {
    write_file(path.as_ref(), &cdl_edl(tl))
}

/// Writes [`ccc`] to `path`.
pub fn ccc_to_file(tl: &Timeline, path: impl AsRef<Path>) -> Result<()> {
    write_file(path.as_ref(), &ccc(tl))
}
