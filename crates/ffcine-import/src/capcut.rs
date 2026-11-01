//! CapCut / JianYing draft reader: `draft_content.json`, the newer `draft_info.json`, or the
//! project directory holding one. There is no official spec; the structure follows
//! `pyJianYingDraft` / `pyCapCut`. Encrypted drafts (JianYing > 5.9) are refused.
//!
//! Units: draft times are µs, converted to frames at the draft `fps` (or
//! [`ImportOptions::rate`]); `clip.transform` is in half-canvas units with +y up; `rotation` is
//! degrees clockwise.
//!
//! What maps (schemas: `docs/timeline.md`):
//! - `canvas_config {width, height}` -> `metadata.ffcine.frame_size`; `fps` -> the timeline rate.
//! - `tracks[]` -> OTIO tracks in file order: `video`, `text`, `sticker`, `effect`, `filter` are
//!   video tracks `V1..`, `audio` tracks `A1..`. Segments sit at `target_timerange` with `Gap`s
//!   between them; unknown track types are skipped.
//! - Segment -> `Clip` with `metadata.ffcine.id = "capcut-<segment id>"`, `source_range` =
//!   (`source_timerange.start`, `target_timerange.duration`), `enabled` = `visible`. Media
//!   (`materials.videos[]`, `audios[]`) -> `ExternalReference` (`##_draftpath_placeholder_..._##`
//!   and relative paths resolved against the draft directory), `metadata.ffcine.source.kind`
//!   from the material `type` (`video`, `photo`, `audio`).
//! - `speed` -> `LinearTimeWarp`; a `speeds[]` material with `curve_speed.speed_points`
//!   (`x` 0..1 along the clip, `y` speed) -> `ffcine.time.ramp/1` + the average
//!   `LinearTimeWarp` (approx: the curve is integrated piecewise linearly and scaled to the
//!   source duration CapCut consumed).
//! - `clip` -> `ffcine.geo.transform/1` (`units: "frame"`, `y: "down"`): `translate_x = x / 2`,
//!   `translate_y = -y / 2`, `scale_x/y`, `rotate_deg = -rotation`, `flip_x/y`; `alpha` ->
//!   `ffcine.comp.blend/1 {opacity}`.
//! - `volume` (linear) and `audio_fades[]` -> `ffcine.audio.level/1 {gain_db, fade_in,
//!   fade_out}` (seconds).
//! - `common_keyframes[]` -> keyframe lists on the same parameters (`KFTypePositionX/Y`,
//!   `ScaleX/Y`, `UniformScale`, `Rotation`, `Alpha`, `Volume`), `t` in the clip's source time;
//!   `curveType` `Line` (or absent) is `linear`, anything else `ease`.
//! - `KFTypeSaturation/Contrast/Brightness` keyframes and `effects[]` adjustments of those types
//!   -> `ffcine.color.grade/1` `saturation = 1 + v`, `contrast = v`, `exposure = v` (approx,
//!   CapCut's curves are unknown).
//! - Text segments (`materials.texts[]`) -> `ffcine.text` generator clips: `content` (JSON with
//!   `text` and `styles[]`, or plain text in older drafts), the first style's `size`, `font.path`
//!   (file stem as `font`), `fill.content.solid.color`, `bold`, `italic`; material `alignment`
//!   0/1/2; `position` from the clip transform; `material_animations[]` `in`/`out` ->
//!   `animation` (`fade` for names with "fade", else the name). `size` px = CapCut size x frame
//!   height / 160 (approx, unverified).
//! - `transitions[]` referenced by a segment -> `Transition(SMPTE_Dissolve)` at its end, centred
//!   on the cut, with `metadata.ffcine = {schema: "ffcine.transition/1", kind}`; non-dissolve
//!   store assets render as a dissolve (warned).
//! - `effects[]` (other than the adjustments above), `video_effects[]`, `filters[]` and
//!   `effect`/`filter` track segments are store assets: foreign effects (`capcut.<pool>`) on the
//!   clip, or on a `Gap` for effect/filter tracks. Stickers are `MissingReference` clips.
//!
//! Everything else stays verbatim in `metadata.ffcine.foreign.capcut` on the object it came from
//! (track, clip, media reference, generator, transition, timeline; other referenced materials
//! under the clip's `materials.<pool>`, unreferenced ones on the timeline) and is listed in
//! [`ImportReport::unmapped`](super::ImportReport::unmapped).

#![allow(clippy::doc_markdown)] // CapCut, JianYing
#![allow(clippy::many_single_char_names)] // maths on x, y, t, v

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::common::{Event, Interp, ffcine_effect, foreign_effect, keyframe, linear_to_db, rt};
use super::common::{Group, build_track, range, time_ramp};
use super::xml::{Ids, Report, clean};
use super::{ImportOptions, Imported};
use ffcine_core::{Error, Result};
use ffcine_timeline::{
    Clip, Effect, ExternalReference, FFCINE_VERSION, Gap, GeneratorReference, HasMetadata, Item,
    LinearTimeWarp, MediaReference, MissingReference, Timeline, Track, Transition, track_kind,
};

const APP: &str = "capcut";
const US: f64 = 1e6;
const PLACEHOLDER: &str = "##_draftpath_placeholder_";

/// Reads a CapCut draft file or project directory.
pub fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let path = if input.is_dir() {
        draft_file(input).ok_or_else(|| {
            invalid(format!(
                "{}: no draft_content.json or draft_info.json",
                input.display()
            ))
        })?
    } else {
        input.to_owned()
    };
    let bytes = std::fs::read(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let dir = path.parent().map(Path::to_owned).unwrap_or_default();
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let doc = parse(&bytes)?;
    Ok(map(&doc, &dir, name, opts))
}

/// The draft JSON inside a CapCut project directory (`draft_content.json`, or `draft_info.json`
/// in newer versions).
pub fn draft_file(dir: &Path) -> Option<PathBuf> {
    ["draft_content.json", "draft_info.json"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid {
        what: "CapCut draft",
        value: msg.into(),
    }
}

fn parse(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes)
        .ok()
        .map(|t| t.trim_start_matches('\u{feff}').trim_start())
        .filter(|t| t.starts_with('{'))
        .ok_or_else(|| {
            invalid(
                "not JSON: the draft is encrypted (CapCut and JianYing > 5.9 encrypt drafts; \
                 save it with JianYing <= 5.9 or export another format)",
            )
        })?;
    let doc: Value = serde_json::from_str(text).map_err(|e| invalid(format!("JSON: {e}")))?;
    if doc.get("tracks").is_none() && doc.get("materials").is_none() {
        return Err(invalid("no tracks or materials: not a CapCut draft"));
    }
    Ok(doc)
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64()
}

fn st<'v>(v: &'v Value, key: &str) -> Option<&'v str> {
    v.get(key)?.as_str()
}

fn arr<'v>(v: &'v Value, key: &str) -> impl Iterator<Item = &'v Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// `(start, duration)` in µs.
fn timerange(v: &Value, key: &str) -> Option<(f64, f64)> {
    let r = v.get(key)?;
    Some((num(r, "start").unwrap_or(0.0), num(r, "duration")?))
}

/// Our parameters for a CapCut keyframe property.
fn kf_params(ty: &str) -> &'static [&'static str] {
    match ty.trim_start_matches("KFType") {
        "PositionX" => &["translate_x"],
        "PositionY" => &["translate_y"],
        "ScaleX" => &["scale_x"],
        "ScaleY" => &["scale_y"],
        "UniformScale" | "Scale" => &["scale_x", "scale_y"],
        "Rotation" => &["rotate_deg"],
        "Alpha" => &["opacity"],
        "Volume" => &["gain_db"],
        "Saturation" => &["saturation"],
        "Contrast" => &["contrast"],
        "Brightness" => &["exposure"],
        _ => &[],
    }
}

/// A CapCut value in our units for `param`.
fn convert(param: &str, v: f64) -> f64 {
    match param {
        "translate_x" => v / 2.0,
        "translate_y" => -v / 2.0,
        "rotate_deg" => -v,
        "gain_db" => linear_to_db(v),
        "saturation" => 1.0 + v,
        _ => v,
    }
}

/// The grade parameter for an `effects[]` adjustment type.
fn adjust_param(ty: &str) -> Option<&'static str> {
    match ty {
        "saturation" => Some("saturation"),
        "contrast" => Some("contrast"),
        "brightness" => Some("exposure"),
        _ => None,
    }
}

struct Mapper<'a> {
    rate: f64,
    h: f64,
    dir: &'a Path,
    materials: HashMap<&'a str, (&'a str, &'a Value)>,
    used: HashSet<&'a str>,
    report: Report,
    skipped: Vec<String>,
    ids: Ids,
}

/// A mapped segment.
struct Seg {
    start: f64,
    dur: f64,
    item: Item,
    /// The transition into the next segment.
    transition: Option<Transition>,
}

/// Everything a segment's `extra_material_refs` and keyframes contribute.
#[derive(Default)]
struct Deco {
    effects: Vec<Effect>,
    foreign: Map<String, Value>,
    keys: HashMap<&'static str, Vec<Value>>,
    grade: Map<String, Value>,
    fade_in: Option<f64>,
    fade_out: Option<f64>,
    curve: Option<Value>,
    animation: Map<String, Value>,
    transition: Option<Transition>,
}

#[allow(clippy::too_many_lines)]
fn map(doc: &Value, dir: &Path, name: String, opts: &ImportOptions) -> Imported {
    let mut report = Report::default();
    let rate = match (opts.rate, num(doc, "fps").filter(|r| *r > 0.0)) {
        (Some(r), _) | (None, Some(r)) => r,
        (None, None) => {
            report.warn("no fps in the draft: assumed 30");
            30.0
        }
    };
    let canvas = doc.get("canvas_config");
    let size = canvas
        .and_then(|c| Some((c.get("width")?.as_u64()?, c.get("height")?.as_u64()?)))
        .and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)))
        .filter(|(w, h)| *w > 0 && *h > 0);
    let (w, h) = size.or(opts.frame_size).unwrap_or_else(|| {
        report.warn("no canvas_config size in the draft or options: assumed 1920x1080");
        (1920, 1080)
    });
    let mut materials = HashMap::new();
    if let Some(Value::Object(pools)) = doc.get("materials") {
        for (pool, list) in pools {
            for m in list.as_array().into_iter().flatten() {
                if let Some(id) = st(m, "id") {
                    materials.insert(id, (pool.as_str(), m));
                }
            }
        }
    }
    let mut m = Mapper {
        rate,
        h: f64::from(h),
        dir,
        materials,
        used: HashSet::new(),
        report,
        skipped: Vec::new(),
        ids: Ids::default(),
    };

    let (mut video, mut audio) = (Vec::new(), Vec::new());
    for (i, t) in arr(doc, "tracks").enumerate() {
        let ty = st(t, "type").unwrap_or_default();
        let kind = match ty {
            "video" | "text" | "sticker" | "effect" | "filter" => track_kind::VIDEO,
            "audio" => track_kind::AUDIO,
            other => {
                m.skip(format!("tracks[{i}]: unknown track type {other:?}"));
                continue;
            }
        };
        let track = m.track(i, ty, kind, t);
        if kind == track_kind::VIDEO {
            video.push(track);
        } else {
            audio.push(track);
        }
    }
    let mut tl = Timeline::new(
        st(doc, "name")
            .filter(|n| !n.is_empty())
            .map_or(name, str::to_owned),
    );
    for (i, mut t) in video.into_iter().enumerate() {
        t.name = format!("V{}", i + 1);
        tl.tracks.children.push(Item::Track(t));
    }
    for (i, mut t) in audio.into_iter().enumerate() {
        t.name = format!("A{}", i + 1);
        tl.tracks.children.push(Item::Track(t));
    }
    let ns = tl.ffcine_mut();
    ns.insert("v".into(), FFCINE_VERSION.into());
    ns.insert("frame_size".into(), json!([w, h]));

    let mut foreign = m.rest(
        doc,
        &[
            "tracks",
            "materials",
            "fps",
            "canvas_config",
            "duration",
            "name",
        ],
        "",
    );
    if let Some(c) = canvas {
        foreign.insert("canvas_config".into(), c.clone());
    }
    let mut unused = Map::new();
    if let Some(Value::Object(pools)) = doc.get("materials") {
        for (pool, list) in pools {
            let left: Vec<Value> = list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| st(v, "id").is_none_or(|id| !m.used.contains(id)))
                .cloned()
                .collect();
            if !left.is_empty() {
                unused.insert(pool.clone(), left.into());
            }
        }
    }
    if !unused.is_empty() {
        foreign.insert("materials".into(), unused.into());
    }
    if !foreign.is_empty() {
        tl.set_foreign(APP, foreign.into());
    }
    let mut report = m.report.finish();
    for s in m.skipped {
        report.skip(s);
    }
    Imported {
        timeline: tl,
        report,
    }
}

impl<'a> Mapper<'a> {
    fn fr(&self, us: f64) -> f64 {
        clean(us * self.rate / US)
    }

    fn skip(&mut self, msg: String) {
        if !self.skipped.contains(&msg) {
            self.skipped.push(msg);
        }
    }

    /// `obj` without `mapped`, recording non-empty leftovers as unmapped `<path>.<key>`.
    fn rest(&mut self, obj: &Value, mapped: &[&str], path: &str) -> Map<String, Value> {
        let mut out = Map::new();
        for (k, v) in obj.as_object().into_iter().flatten() {
            if mapped.contains(&k.as_str()) {
                continue;
            }
            if !is_empty(v) {
                self.report.unmapped(if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                });
            }
            out.insert(k.clone(), v.clone());
        }
        out
    }

    fn resolve(&self, p: &str) -> PathBuf {
        if let Some(rest) = p.strip_prefix(PLACEHOLDER)
            && let Some((_, tail)) = rest.split_once("_##")
        {
            return self.dir.join(tail.trim_start_matches(['/', '\\']));
        }
        let path = PathBuf::from(p);
        if path.is_absolute() {
            path
        } else {
            self.dir.join(path)
        }
    }

    fn track(&mut self, i: usize, ty: &str, kind: &str, t: &'a Value) -> Track {
        let mut events = Vec::new();
        let mut pending: Option<(f64, Transition)> = None;
        for seg in arr(t, "segments") {
            let Some(s) = self.segment(ty, seg, i) else {
                continue;
            };
            if let Some((cut, tr)) = pending.take() {
                if (cut - s.start).abs() < 1e-6 {
                    events.push(Event::Transition {
                        cut,
                        transition: tr,
                    });
                } else {
                    self.report
                        .warn("CapCut transition not followed by an abutting segment: dropped");
                }
            }
            if let Some(tr) = s.transition {
                pending = Some((s.start + s.dur, tr));
            }
            events.push(Event::Item {
                start: s.start,
                dur: s.dur,
                item: s.item,
            });
        }
        if pending.is_some() {
            self.report
                .warn("CapCut transition after a track's last segment: dropped");
        }
        let mut track = Track::new(String::new(), kind);
        track.children = build_track(vec![Group { events }], self.rate);
        let mut f = self.rest(t, &["segments", "type", "id"], "tracks[]");
        f.insert("type".into(), ty.into());
        if let Some(id) = t.get("id") {
            f.insert("id".into(), id.clone());
        }
        track.set_foreign(APP, f.into());
        track
    }

    #[allow(clippy::too_many_lines)]
    fn segment(&mut self, ty: &str, seg: &'a Value, ti: usize) -> Option<Seg> {
        let sid = st(seg, "id");
        let label = sid.map_or_else(
            || format!("tracks[{ti}] segment"),
            |s| format!("segment {s}"),
        );
        let Some((tstart, tdur)) = timerange(seg, "target_timerange").filter(|r| r.1 > 0.0) else {
            self.skip(format!("{label}: no target_timerange"));
            return None;
        };
        let mid = st(seg, "material_id").unwrap_or_default();
        let Some(&(pool, mat)) = self.materials.get(mid) else {
            self.skip(format!("{label}: material {mid:?} not found"));
            return None;
        };
        self.used.insert(mid);
        let id = sid.map_or_else(
            || self.ids.make("capcut", &format!("{ti}:{tstart}:{mid}")),
            |s| format!("capcut-{s}"),
        );
        let (start, dur) = (self.fr(tstart), self.fr(tdur));
        let (src_start, src_dur) = timerange(seg, "source_timerange").unwrap_or((0.0, tdur));
        let src0 = self.fr(src_start);

        let mut d = Deco::default();
        for r in arr(seg, "extra_material_refs").filter_map(Value::as_str) {
            match self.materials.get(r) {
                Some(&(p, v)) => {
                    self.used.insert(r);
                    self.extra(p, v, ty, &mut d);
                }
                None => self.skip(format!("{label}: extra material {r:?} not found")),
            }
        }
        self.keyframes(seg, src0, &mut d);

        let clip_v = seg.get("clip").cloned().unwrap_or(Value::Null);
        let text = ty == "text" || pool == "texts";
        let mut item = match (ty, pool) {
            ("effect" | "filter", _) => {
                let mut gap = Gap::new(rt(dur, self.rate));
                st(mat, "name")
                    .unwrap_or_default()
                    .clone_into(&mut gap.name);
                gap.effects.extend(self.fx(pool, mat, &mut d.grade));
                Item::Gap(gap)
            }
            (_, "texts") => {
                let g = self.text(mat, &clip_v, &d, dur);
                let mut c = Clip::new(
                    g.name.clone(),
                    MediaReference::Generator(g),
                    Some(range(0.0, dur, self.rate)),
                );
                c.enabled = seg.get("visible").and_then(Value::as_bool) != Some(false);
                Item::Clip(c)
            }
            (_, "videos" | "audios") => {
                let (name, media, kind) = self.media(pool, mat);
                let mut c = Clip::new(name, media, Some(range(src0, dur, self.rate)));
                if let Some(p) = st(mat, "path").filter(|p| !p.is_empty()) {
                    c.ffcine_mut()
                        .insert("source".into(), json!({ "src": p, "kind": kind }));
                }
                c.enabled = seg.get("visible").and_then(Value::as_bool) != Some(false);
                Item::Clip(c)
            }
            (_, "stickers") => {
                self.report
                    .warn("CapCut sticker kept as a MissingReference clip: not rendered");
                self.report.unmapped("materials.stickers");
                d.foreign.insert("material".into(), mat.clone());
                Item::Clip(Clip::new(
                    st(mat, "name").unwrap_or_default(),
                    MediaReference::Missing(MissingReference::default()),
                    Some(range(0.0, dur, self.rate)),
                ))
            }
            (_, other) => {
                self.skip(format!("{label}: material pool {other:?} on a {ty} track"));
                return None;
            }
        };

        let mut effects = self.speed(seg, &mut d, dur, self.fr(src_dur));
        if let Some(e) = Self::transform(&clip_v, &mut d, !text) {
            effects.push(e);
        }
        if let Some(e) = Self::blend(&clip_v, &mut d) {
            effects.push(e);
        }
        if let Some(e) = Self::level(seg, &mut d) {
            effects.push(e);
        }
        if !d.grade.is_empty() {
            self.report
                .warn("CapCut saturation/contrast/brightness mapped to the grade approximately");
            effects.push(ffcine_effect("ffcine.color.grade/1", d.grade.clone()));
        }
        effects.append(&mut d.effects);

        let mut f = self.rest(
            seg,
            &[
                "id",
                "material_id",
                "target_timerange",
                "source_timerange",
                "speed",
                "volume",
                "extra_material_refs",
                "clip",
                "common_keyframes",
                "visible",
            ],
            "segments[]",
        );
        f.append(&mut d.foreign);
        match &mut item {
            Item::Clip(c) => {
                c.set_ffcine_id(&id);
                c.effects = effects;
                if !f.is_empty() {
                    c.set_foreign(APP, f.into());
                }
            }
            Item::Gap(g) => {
                g.set_ffcine_id(&id);
                g.effects.extend(effects);
                if !f.is_empty() {
                    g.set_foreign(APP, f.into());
                }
            }
            _ => {}
        }
        Some(Seg {
            start,
            dur,
            item,
            transition: d.transition,
        })
    }

    /// One `extra_material_refs` entry.
    fn extra(&mut self, pool: &str, v: &Value, ty: &str, d: &mut Deco) {
        match pool {
            "speeds" => {
                if num(v, "mode") == Some(1.0) || v.get("curve_speed").is_some_and(|c| !c.is_null())
                {
                    d.curve = v.get("curve_speed").filter(|c| !c.is_null()).cloned();
                }
            }
            "audio_fades" => {
                d.fade_in = num(v, "fade_in_duration")
                    .filter(|x| *x > 0.0)
                    .map(|x| x / US);
                d.fade_out = num(v, "fade_out_duration")
                    .filter(|x| *x > 0.0)
                    .map(|x| x / US);
            }
            "transitions" => d.transition = Some(self.transition(v)),
            "material_animations" if ty == "text" => {
                for a in arr(v, "animations") {
                    let (Some(kind @ ("in" | "out")), Some(name)) = (st(a, "type"), st(a, "name"))
                    else {
                        self.report.unmapped("materials.material_animations");
                        continue;
                    };
                    let k = if name.to_lowercase().contains("fade") {
                        "fade".to_owned()
                    } else {
                        name.to_owned()
                    };
                    let secs = num(a, "duration").unwrap_or(0.0) / US;
                    d.animation
                        .insert(kind.into(), json!({ "kind": k, "duration": secs }));
                }
            }
            "effects" | "video_effects" | "filters" => {
                if let Some(e) = self.fx(pool, v, &mut d.grade) {
                    d.effects.push(e);
                }
            }
            _ => {
                self.report.unmapped(format!("materials.{pool}"));
                let mats = d
                    .foreign
                    .entry("materials")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(mats) = mats {
                    let list = mats.entry(pool).or_insert_with(|| json!([]));
                    if let Value::Array(list) = list {
                        list.push(v.clone());
                    }
                }
            }
        }
    }

    /// A store effect/filter as a foreign effect, or `None` for an adjustment folded into `grade`.
    fn fx(&mut self, pool: &str, v: &Value, grade: &mut Map<String, Value>) -> Option<Effect> {
        if pool == "effects"
            && let Some(p) = st(v, "type").and_then(adjust_param)
            && let Some(x) = num(v, "value")
        {
            let name = if p == "exposure" { "brightness" } else { p };
            grade.insert(p.into(), convert(name, x).into());
            return None;
        }
        let name = st(v, "name").unwrap_or(pool);
        self.report.warn(format!(
            "CapCut {pool} {name:?} is a store asset: kept, not rendered"
        ));
        self.report.unmapped(format!("materials.{pool}"));
        Some(foreign_effect(
            name,
            &format!("capcut.{pool}"),
            APP,
            v.clone(),
        ))
    }

    fn transition(&mut self, v: &Value) -> Transition {
        let name = st(v, "name").unwrap_or_default();
        let half = self.fr(num(v, "duration").unwrap_or(500_000.0)) / 2.0;
        let mut t = Transition::dissolve(rt(half, self.rate), rt(half, self.rate));
        name.clone_into(&mut t.name);
        let lower = name.to_lowercase();
        let kind = if ["dissolve", "mix", "叠化", "溶解", "混合"]
            .iter()
            .any(|k| lower.contains(k))
        {
            "dissolve"
        } else {
            self.report.warn(format!(
                "CapCut transition {name:?} is a store asset: rendered as a dissolve"
            ));
            name
        };
        let ns = t.ffcine_mut();
        ns.insert("schema".into(), "ffcine.transition/1".into());
        ns.insert("kind".into(), kind.into());
        let f = self.rest(
            v,
            &["name", "duration", "id", "type"],
            "materials.transitions[]",
        );
        if !f.is_empty() {
            t.set_foreign(APP, v.clone());
        }
        t
    }

    fn media(&mut self, pool: &str, mat: &Value) -> (String, MediaReference, &'static str) {
        let kind = match (pool, st(mat, "type")) {
            ("audios", _) => "audio",
            (_, Some("photo")) => "photo",
            _ => "video",
        };
        let path = st(mat, "path")
            .filter(|p| !p.is_empty())
            .map(|p| self.resolve(p));
        let avail = num(mat, "duration")
            .filter(|d| *d > 0.0)
            .map(|d| range(0.0, self.fr(d), self.rate));
        let name = st(mat, "material_name")
            .or(st(mat, "name"))
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                path.as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let f = self.rest(
            mat,
            &[
                "id",
                "path",
                "duration",
                "type",
                "width",
                "height",
                "material_name",
                "name",
            ],
            &format!("materials.{pool}[]"),
        );
        let media = if let Some(p) = path {
            let mut e = ExternalReference::from_path(&p, avail);
            if !f.is_empty() {
                e.set_foreign(APP, f.into());
            }
            name.clone_into(&mut e.name);
            MediaReference::External(e)
        } else {
            self.report
                .warn("CapCut media material without a path: MissingReference");
            let mut m = MissingReference {
                available_range: avail,
                ..MissingReference::default()
            };
            m.set_foreign(APP, mat.clone());
            MediaReference::Missing(m)
        };
        (name, media, kind)
    }

    fn text(&mut self, mat: &Value, clip: &Value, d: &Deco, dur: f64) -> GeneratorReference {
        let content = st(mat, "content").unwrap_or_default();
        let parsed: Option<Value> = serde_json::from_str(content).ok().filter(Value::is_object);
        let (text, style) = match &parsed {
            Some(c) => (
                st(c, "text").unwrap_or_default(),
                c.get("styles").and_then(|s| s.get(0)),
            ),
            None => (content, None),
        };
        let mut p = Map::new();
        p.insert("text".into(), text.into());
        let style_or_mat = |k: &str, mk: &str| {
            style
                .and_then(|s| s.get(k))
                .or_else(|| mat.get(mk))
                .filter(|v| !v.is_null())
        };
        if let Some(size) = style_or_mat("size", "font_size").and_then(Value::as_f64) {
            p.insert("size".into(), clean(size * self.h / 160.0).into());
            self.report
                .warn("CapCut text size converted to px approximately (size x height / 160)");
        }
        let font = style
            .and_then(|s| s.get("font"))
            .and_then(|f| st(f, "path"))
            .or(st(mat, "font_path"))
            .filter(|f| !f.is_empty());
        if let Some(f) = font.and_then(|f| Path::new(f).file_stem()) {
            p.insert("font".into(), f.to_string_lossy().into_owned().into());
        }
        let alpha = style
            .and_then(|s| s.get("fill"))
            .and_then(|f| num(f, "alpha"))
            .or(num(mat, "text_alpha"))
            .unwrap_or(1.0);
        let rgb = style
            .and_then(|s| s.pointer("/fill/content/solid/color"))
            .and_then(Value::as_array)
            .map(|c| c.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
            .or_else(|| st(mat, "text_color").and_then(hex_rgb));
        if let Some([r, g, b]) = rgb.as_deref() {
            p.insert("colour".into(), json!([r, g, b, alpha]));
        }
        for k in ["bold", "italic"] {
            if let Some(b) = style.and_then(|s| s.get(k)).and_then(Value::as_bool) {
                p.insert(k.into(), b.into());
            }
        }
        if let Some(a) = mat.get("alignment").and_then(Value::as_u64) {
            let a = match a {
                0 => "left",
                2 => "right",
                _ => "center",
            };
            p.insert("alignment".into(), a.into());
        }
        let keyed = d.keys.contains_key("translate_x") || d.keys.contains_key("translate_y");
        if !keyed && let Some(t) = clip.get("transform") {
            let x = convert("translate_x", num(t, "x").unwrap_or(0.0));
            let y = convert("translate_y", num(t, "y").unwrap_or(0.0));
            p.insert("position".into(), json!([x + 0.0, y + 0.0]));
        }
        if !d.animation.is_empty() {
            p.insert("animation".into(), d.animation.clone().into());
        }
        let mut g = GeneratorReference::new("ffcine.text", p);
        text.lines()
            .next()
            .unwrap_or_default()
            .clone_into(&mut g.name);
        g.available_range = Some(range(0.0, dur, self.rate));
        let mut f = self.rest(
            mat,
            &[
                "id",
                "type",
                "content",
                "alignment",
                "font_size",
                "font_path",
                "text_color",
                "text_alpha",
            ],
            "materials.texts[]",
        );
        let styles = parsed
            .as_ref()
            .and_then(|c| c.get("styles"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if styles > 1 {
            self.report
                .warn("CapCut text with several styles: only the first is mapped");
            self.report.unmapped("materials.texts[].content.styles");
            f.insert("content".into(), content.into());
        }
        if !f.is_empty() {
            g.set_foreign(APP, f.into());
        }
        g
    }

    fn keyframes(&mut self, seg: &Value, src0: f64, d: &mut Deco) {
        let mut kept = Vec::new();
        for kf in arr(seg, "common_keyframes") {
            let ty = st(kf, "property_type").unwrap_or_default();
            let params = kf_params(ty);
            if params.is_empty() {
                self.report
                    .unmapped(format!("segments[].common_keyframes.{ty}"));
                kept.push(kf.clone());
                continue;
            }
            for &param in params {
                let mut list = Vec::new();
                for k in arr(kf, "keyframe_list") {
                    let Some(v) = k
                        .get("values")
                        .and_then(|v| v.get(0))
                        .and_then(Value::as_f64)
                    else {
                        continue;
                    };
                    let t = src0 + self.fr(num(k, "time_offset").unwrap_or(0.0));
                    let interp = match st(k, "curveType") {
                        None | Some("Line" | "") => Interp::Linear,
                        Some(_) => Interp::Ease,
                    };
                    let v = clean(convert(param, v) * 1e9) / 1e9;
                    list.push(keyframe(t, self.rate, (v + 0.0).into(), interp));
                }
                if list.is_empty() {
                    continue;
                }
                match param {
                    "saturation" | "contrast" | "exposure" => {
                        d.grade.insert(param.into(), list.into());
                    }
                    _ => {
                        d.keys.insert(param, list);
                    }
                }
            }
        }
        if !kept.is_empty() {
            d.foreign.insert("common_keyframes".into(), kept.into());
        }
    }

    /// `LinearTimeWarp`, or a ramp from `curve_speed`.
    fn speed(&mut self, seg: &Value, d: &mut Deco, dur: f64, src_dur: f64) -> Vec<Effect> {
        if let Some(curve) = d.curve.take()
            && let Some((ramp, avg)) = self.ramp(&curve, dur, src_dur)
        {
            self.report
                .warn("CapCut curve_speed mapped to ffcine.time.ramp/1 approximately");
            let mut w = LinearTimeWarp::new(avg);
            w.set_foreign(APP, curve);
            return vec![ramp, Effect::LinearTimeWarp(w)];
        }
        match num(seg, "speed") {
            Some(s) if s > 0.0 && (s - 1.0).abs() > 1e-9 => {
                vec![Effect::LinearTimeWarp(LinearTimeWarp::new(s))]
            }
            _ => Vec::new(),
        }
    }

    fn ramp(&mut self, curve: &Value, dur: f64, src_dur: f64) -> Option<(Effect, f64)> {
        const STEPS: u32 = 8;
        let pts: Vec<(f64, f64)> = arr(curve, "speed_points")
            .filter_map(|p| Some((num(p, "x")?, num(p, "y")?)))
            .collect();
        if pts.len() < 2 || dur <= 0.0 {
            return None;
        }
        let mut map = vec![(pts[0].0 * dur, 0.0)];
        let mut s = 0.0;
        for w in pts.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            for i in 1..=STEPS {
                let f = |i: u32| f64::from(i) / f64::from(STEPS);
                let (a, b) = (x0 + (x1 - x0) * f(i - 1), x0 + (x1 - x0) * f(i));
                let (va, vb) = (y0 + (y1 - y0) * f(i - 1), y0 + (y1 - y0) * f(i));
                s += f64::midpoint(va, vb) * (b - a) * dur;
                map.push((b * dur, s));
            }
        }
        if s > 0.0 && src_dur > 0.0 {
            for p in &mut map {
                p.1 *= src_dur / s;
            }
        }
        time_ramp(&map, self.rate, &mut self.report)
    }

    /// A parameter: its keyframes, else the static value when it isn't `ident`.
    fn param(d: &mut Deco, p: &mut Map<String, Value>, name: &'static str, v: f64, ident: f64) {
        if let Some(k) = d.keys.remove(name) {
            p.insert(name.into(), k.into());
        } else if (v - ident).abs() > 1e-9 {
            p.insert(name.into(), (clean(v * 1e9) / 1e9).into());
        }
    }

    fn transform(clip: &Value, d: &mut Deco, translate: bool) -> Option<Effect> {
        let t = clip.get("transform");
        let sc = clip.get("scale");
        let mut p = Map::new();
        if translate || d.keys.contains_key("translate_x") || d.keys.contains_key("translate_y") {
            let x = t.and_then(|t| num(t, "x")).unwrap_or(0.0);
            let y = t.and_then(|t| num(t, "y")).unwrap_or(0.0);
            let x = if translate { x } else { 0.0 };
            let y = if translate { y } else { 0.0 };
            Self::param(d, &mut p, "translate_x", convert("translate_x", x), 0.0);
            Self::param(d, &mut p, "translate_y", convert("translate_y", y), 0.0);
        }
        let sx = sc.and_then(|s| num(s, "x")).unwrap_or(1.0);
        let sy = sc.and_then(|s| num(s, "y")).unwrap_or(1.0);
        Self::param(d, &mut p, "scale_x", sx, 1.0);
        Self::param(d, &mut p, "scale_y", sy, 1.0);
        let rot = num(clip, "rotation").unwrap_or(0.0);
        Self::param(d, &mut p, "rotate_deg", convert("rotate_deg", rot), 0.0);
        let flip = clip.get("flip");
        for (k, ours) in [("horizontal", "flip_x"), ("vertical", "flip_y")] {
            if flip.and_then(|f| f.get(k)).and_then(Value::as_bool) == Some(true) {
                p.insert(ours.into(), true.into());
            }
        }
        if p.is_empty() {
            return None;
        }
        p.insert("units".into(), "frame".into());
        p.insert("y".into(), "down".into());
        Some(ffcine_effect("ffcine.geo.transform/1", p))
    }

    fn blend(clip: &Value, d: &mut Deco) -> Option<Effect> {
        let mut p = Map::new();
        let alpha = num(clip, "alpha").unwrap_or(1.0);
        Self::param(d, &mut p, "opacity", alpha, 1.0);
        (!p.is_empty()).then(|| ffcine_effect("ffcine.comp.blend/1", p))
    }

    fn level(seg: &Value, d: &mut Deco) -> Option<Effect> {
        let mut p = Map::new();
        let vol = num(seg, "volume").unwrap_or(1.0);
        let db = linear_to_db(vol);
        Self::param(d, &mut p, "gain_db", db, 0.0);
        if let Some(x) = d.fade_in {
            p.insert("fade_in".into(), x.into());
        }
        if let Some(x) = d.fade_out {
            p.insert("fade_out".into(), x.into());
        }
        (!p.is_empty()).then(|| ffcine_effect("ffcine.audio.level/1", p))
    }
}

/// `#rrggbb` -> sRGB 0..1.
fn hex_rgb(s: &str) -> Option<Vec<f64>> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    (0..3)
        .map(|i| {
            u8::from_str_radix(h.get(i * 2..i * 2 + 2)?, 16)
                .ok()
                .map(|b| clean(f64::from(b) / 255.0 * 1e6) / 1e6)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        assert!((convert("translate_x", 1.0) - 0.5).abs() < 1e-12);
        assert!((convert("translate_y", 1.0) + 0.5).abs() < 1e-12);
        assert!((convert("rotate_deg", 30.0) + 30.0).abs() < 1e-12);
        assert_eq!(hex_rgb("#ff0000"), Some(vec![1.0, 0.0, 0.0]));
        assert!(parse(b"\x8a\x01binary").is_err());
    }
}
