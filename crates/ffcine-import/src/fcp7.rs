//! Final Cut Pro 7 / Premiere Pro XML (`<xmeml version="4|5">`) reader, straight from the XML.
//!
//! What maps (frames at the sequence rate; schemas in `docs/timeline.md`):
//! - The first top-level `sequence`: `rate` (`timebase`, `ntsc` -> ×1000/1001), `timecode/frame`
//!   -> `global_start_time`, `media/video/format/samplecharacteristics` `width`/`height` ->
//!   `metadata.ffcine.frame_size`. Video tracks first (`V1` bottom), then audio (`A1`, ...).
//! - `clipitem` -> `Clip`: `start`/`end` place it (`-1` next to a transition = the
//!   transition's cut), `in` is `source_range.start_time`. `file` (declared once, later
//!   referenced by `id`) -> `ExternalReference` (`pathurl`, `file://localhost/` -> `file:///`;
//!   available range `0..duration`), no `pathurl` -> `MissingReference`. Reel, file timecode and
//!   `logginginfo` -> `metadata.ffcine.source`; `link` groups -> `metadata.ffcine.link_group`
//!   (1, 2, ...); `id` -> `metadata.ffcine.id` = `fcp7-<hash of track@start>`.
//! - `transitionitem` -> `Transition(SMPTE_Dissolve)`, cut from `alignment` (`start`, `end`,
//!   else the middle). Anything but Cross Dissolve gets a flat `ffcine.transition/1` with
//!   `kind` = the effectid (the FCP audio cross fades map to `cross_fade` + `crossfade_db`).
//! - `generatoritem`: `Color` / mattes -> `ffcine.solid`; `Text`, `Outline Text`, ... ->
//!   `ffcine.text` (`str`, `fontname`, `fontsize`, `fontcolor`, `origin`, `fontstyle`,
//!   `fontalign`). Premiere `GraphicAndType` filters -> `ffcine.text` from the `Source Text`
//!   blob (base64: an 8-byte LE length, then UTF-16LE JSON with `mText`, `mFontName`,
//!   `mFontSize`; best effort).
//! - Filters by `effectid`: `basic` -> `ffcine.geo.transform/1` (`units: "frame"`: `center`
//!   `horiz`/`vert` as fractions of the frame width/height, y down; `scale` % -> 1 = unscaled;
//!   `rotation` clockwise -> `rotate_deg` counter-clockwise; `centerOffset` -> `anchor`);
//!   `crop` (%) -> `ffcine.geo.crop/1`; `opacity` (%) -> `ffcine.comp.blend/1`; `audiolevels`
//!   (linear 0–3.98) -> `ffcine.audio.level/1 {gain_db}`; `audiopan` (−1..1, or ±100) ->
//!   `ffcine.audio.pan/1`; `timeremap` -> `LinearTimeWarp(speed / 100)` (reverse: negative), or
//!   with `variablespeed` its `graphdict` -> `ffcine.time.ramp/1` + average `LinearTimeWarp`.
//! - Keyframes `<keyframe><when><value>` -> `[{t, v, interp}]`; `when` is taken as a media
//!   frame (the space of `in`/`out`), i.e. already the clip's source time. Curved
//!   interpolations become `ease`.
//! - `marker`s: sequence markers on `timeline.tracks.markers`, clip markers (media frames) on the
//!   clip; `out` `-1` = a point.
//!
//! *Unverified* against FCP7/Premiere themselves: the `center`/`centerOffset`/`origin` units and
//! y direction, the rotation sense, `when` being in media frames, and `in`/`out` of retimed clips.
//! A clipitem `rate` other than the sequence's is read at the sequence rate (warned). Nested
//! sequences are not expanded (`MissingReference`, sequence kept in foreign). Everything else is
//! kept under `metadata.ffcine.foreign.fcp_xml` on the object it came from.

#![allow(clippy::many_single_char_names)] // maths on x, y, w, h, t, v

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::{ImportOptions, Imported};
use crate::common::{
    Event, Group, Interp, build_track, ffcine_effect, foreign_effect, keyframe, linear_to_db,
    marker_color, range, rt, time_ramp,
};
use crate::xml::{Element, Ids, Report};
use ffcine_core::{Error, Result};
use ffcine_timeline::{
    Clip, Effect, ExternalReference, FFCINE_VERSION, GeneratorReference, HasMetadata, Item,
    LinearTimeWarp, Marker, MediaReference, MissingReference, Timeline, Track, Transition,
    track_kind,
};

const APP: &str = "fcp_xml";

/// Reads an FCP7 / Premiere XML file.
pub fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let src = std::fs::read_to_string(input).map_err(|source| Error::Io {
        path: input.to_owned(),
        source,
    })?;
    import_str(&src, opts)
}

/// Reads FCP7 / Premiere XML from a string.
#[allow(clippy::too_many_lines)]
pub fn import_str(src: &str, opts: &ImportOptions) -> Result<Imported> {
    let root = Element::parse(src).map_err(|e| invalid(e.to_string()))?;
    if root.name != "xmeml" {
        return Err(invalid(format!("root <{}>, expected <xmeml>", root.name)));
    }
    let mut seqs = Vec::new();
    sequences(&root, &mut seqs);
    let seq = *seqs
        .first()
        .ok_or_else(|| invalid("no <sequence> in the file"))?;
    let mut report = Report::default();
    if seqs.len() > 1 {
        report.warn(format!(
            "{} sequences in the file: imported the first ({:?})",
            seqs.len(),
            txt(seq, "name").unwrap_or_default()
        ));
    }
    let rate = match parse_rate(seq) {
        Some(r) => {
            if let Some(want) = opts.rate.filter(|w| (w - r).abs() > 1e-6) {
                report.warn(format!(
                    "rate {want} ignored: the sequence is {r} fps (retime it after import)"
                ));
            }
            r
        }
        None => opts
            .rate
            .ok_or_else(|| invalid("the sequence has no <rate>; pass a rate"))?,
    };
    let media = seq.child("media");
    let sc = media
        .and_then(|m| m.child("video"))
        .and_then(|v| v.child("format"))
        .and_then(|f| f.child("samplecharacteristics"));
    let dim = |n: &str| sc.and_then(|s| txt(s, n)?.trim().parse::<u32>().ok());
    let (w, h) = match (dim("width"), dim("height")) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => opts.frame_size.unwrap_or_else(|| {
            report.warn("no frame size in the sequence format or options: assumed 1920x1080");
            (1920, 1080)
        }),
    };
    let mut files = HashMap::new();
    collect_files(&root, &mut files);
    let mut m = Mapper {
        files,
        links: link_groups(seq),
        rate,
        report,
        skipped: Vec::new(),
        ids: Ids::default(),
    };

    let mut tl = Timeline::new(txt(seq, "name").unwrap_or_default());
    let mut foreign = Map::new();
    foreign.insert(
        "version".into(),
        root.attr("version").unwrap_or_default().into(),
    );
    if let Some(a) = seq.attrs_except(&[]) {
        foreign.insert("attrs".into(), a);
    }
    let mut seq_rest = Vec::new();
    for c in &seq.children {
        match c.name.as_str() {
            "name" | "rate" | "media" | "duration" => {}
            "timecode" => {
                if let Some(f) = num(c, "frame") {
                    tl.global_start_time = Some(rt(f, rate));
                }
                foreign.insert("timecode".into(), c.to_json());
            }
            "marker" => {
                if let Some(mk) = m.marker(c) {
                    tl.tracks.markers.push(mk);
                }
            }
            _ => {
                m.report.unmapped(format!("sequence/{}", c.name));
                seq_rest.push(c.to_json());
            }
        }
    }
    let mut tracks = Vec::new();
    for (kind, tk, prefix) in [
        ("video", track_kind::VIDEO, "V"),
        ("audio", track_kind::AUDIO, "A"),
    ] {
        let Some(mk) = media.and_then(|x| x.child(kind)) else {
            continue;
        };
        let mut rest = Vec::new();
        for c in &mk.children {
            if c.name == "track" {
                let name = format!(
                    "{prefix}{}",
                    tracks.iter().filter(|t: &&Track| t.kind == tk).count() + 1
                );
                tracks.push(m.track(c, tk, &name));
            } else {
                rest.push(c.to_json());
            }
        }
        if !rest.is_empty() {
            foreign.insert(kind.into(), rest.into());
        }
    }
    if !seq_rest.is_empty() {
        foreign.insert("children".into(), seq_rest.into());
    }
    tl.tracks.children = tracks.into_iter().map(Item::Track).collect();
    let ns = tl.ffcine_mut();
    ns.insert("v".into(), FFCINE_VERSION.into());
    ns.insert("frame_size".into(), json!([w, h]));
    tl.set_foreign(APP, Value::Object(foreign));
    let mut report = m.report.finish();
    for s in m.skipped {
        report.skip(s);
    }
    Ok(Imported {
        timeline: tl,
        report,
    })
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid {
        what: "FCP7 XML",
        value: msg.into(),
    }
}

fn txt<'a>(el: &'a Element, name: &str) -> Option<&'a str> {
    el.child(name).map(|c| c.text.as_str())
}

fn num(el: &Element, name: &str) -> Option<f64> {
    txt(el, name)?.trim().parse().ok()
}

fn truthy(s: &str) -> Option<bool> {
    match s.trim().to_ascii_uppercase().as_str() {
        "TRUE" | "1" => Some(true),
        "FALSE" | "0" => Some(false),
        _ => None,
    }
}

fn flag(el: &Element, name: &str) -> Option<bool> {
    truthy(txt(el, name)?)
}

/// `<rate><timebase>24</timebase><ntsc>TRUE</ntsc></rate>` -> fps.
fn parse_rate(el: &Element) -> Option<f64> {
    let r = el.child("rate")?;
    let tb = num(r, "timebase").filter(|t| *t > 0.0)?;
    Some(if flag(r, "ntsc") == Some(true) {
        tb * 1000.0 / 1001.0
    } else {
        tb
    })
}

/// Top-level sequences (not the ones nested in clipitems).
fn sequences<'a>(el: &'a Element, out: &mut Vec<&'a Element>) {
    for c in &el.children {
        match c.name.as_str() {
            "sequence" => out.push(c),
            "clipitem" | "generatoritem" | "file" => {}
            _ => sequences(c, out),
        }
    }
}

/// `file` declarations by id (the first one with content wins).
fn collect_files<'a>(el: &'a Element, out: &mut HashMap<String, &'a Element>) {
    for c in &el.children {
        if c.name == "file"
            && !c.children.is_empty()
            && let Some(id) = c.attr("id")
        {
            out.entry(id.to_owned()).or_insert(c);
        }
        collect_files(c, out);
    }
}

/// Link groups: clipitem id -> group number (1, 2, ... in document order), for groups of two
/// or more clips.
fn link_groups(seq: &Element) -> HashMap<String, usize> {
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    fn walk<'a>(el: &'a Element, out: &mut Vec<(&'a str, Vec<&'a str>)>) {
        for c in &el.children {
            if c.name == "clipitem"
                && let Some(id) = c.attr("id")
            {
                let refs = c
                    .children_named("link")
                    .filter_map(|l| txt(l, "linkclipref"))
                    .collect();
                out.push((id, refs));
            } else if c.name != "sequence" {
                walk(c, out);
            }
        }
    }
    let mut items = Vec::new();
    walk(seq, &mut items);
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, (id, _)) in items.iter().enumerate() {
        index.entry(id).or_insert(i);
    }
    let mut parent: Vec<usize> = (0..items.len()).collect();
    for (i, (_, refs)) in items.iter().enumerate() {
        for r in refs {
            if let Some(&j) = index.get(r) {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let mut size: HashMap<usize, usize> = HashMap::new();
    for i in 0..items.len() {
        *size.entry(find(&mut parent, i)).or_default() += 1;
    }
    let mut numbers: HashMap<usize, usize> = HashMap::new();
    let mut out = HashMap::new();
    for (i, (id, _)) in items.iter().enumerate() {
        let root = find(&mut parent, i);
        if size[&root] < 2 {
            continue;
        }
        let next = numbers.len() + 1;
        let n = *numbers.entry(root).or_insert(next);
        out.insert((*id).to_owned(), n);
    }
    out
}

/// The cut point of a `transitionitem`.
fn cut_of(t: &Element) -> Option<(f64, f64, f64)> {
    let (s, e) = (num(t, "start")?, num(t, "end")?);
    if e < s {
        return None;
    }
    let cut = match txt(t, "alignment").unwrap_or("center") {
        "start" | "start-black" => s,
        "end" | "end-black" => e,
        _ => f64::midpoint(s, e),
    };
    Some((s, cut, e))
}

fn param<'a>(e: &'a Element, id: &str) -> Option<&'a Element> {
    e.children_named("parameter").find(|p| {
        txt(p, "parameterid").is_some_and(|x| x.eq_ignore_ascii_case(id))
            || txt(p, "name").is_some_and(|x| x.eq_ignore_ascii_case(id))
    })
}

fn scalar(v: &Element) -> Option<f64> {
    v.text.trim().parse().ok()
}

fn point(v: &Element) -> Option<Value> {
    Some(json!([num(v, "horiz")?, num(v, "vert")?]))
}

fn colour(v: &Element) -> Option<Value> {
    let c = |n: &str| num(v, n).map(|x| x / 255.0);
    Some(json!([
        c("red")?,
        c("green")?,
        c("blue")?,
        c("alpha").unwrap_or(1.0)
    ]))
}

/// Component `i` of a vector value or of vector keyframes.
fn component(v: &Value, i: usize) -> Value {
    match v {
        Value::Array(a) if a.first().is_some_and(Value::is_object) => Value::Array(
            a.iter()
                .map(|k| {
                    let mut k = k.clone();
                    if let Some(o) = k.as_object_mut() {
                        let x = o.get("v").and_then(|x| x.get(i)).cloned();
                        o.insert("v".into(), x.unwrap_or_default());
                    }
                    k
                })
                .collect(),
        ),
        Value::Array(a) => a.get(i).cloned().unwrap_or_default(),
        _ => Value::Null,
    }
}

/// Standard `effect` children that carry no parameters.
const EFFECT_INFO: &[&str] = &[
    "name",
    "effectid",
    "effectcategory",
    "effecttype",
    "mediatype",
];

/// `effect` without the `used` parameters, if anything unmapped is left.
fn effect_rest(e: &Element, used: &[&str], report: &mut Report) -> Option<Value> {
    let id = txt(e, "effectid").unwrap_or_default();
    let mut rest = e.clone();
    rest.children.retain(|c| {
        if c.name != "parameter" {
            return true;
        }
        let pid = txt(c, "parameterid").or_else(|| txt(c, "name"));
        !pid.is_some_and(|p| used.iter().any(|u| u.eq_ignore_ascii_case(p)))
    });
    let mut any = false;
    for c in &rest.children {
        if c.name == "parameter" {
            let pid = txt(c, "parameterid").or_else(|| txt(c, "name"));
            report.unmapped(format!("{id}/{}", pid.unwrap_or("parameter")));
            any = true;
        } else if !EFFECT_INFO.contains(&c.name.as_str()) {
            report.unmapped(format!("{id}/{}", c.name));
            any = true;
        }
    }
    any.then(|| rest.to_json())
}

struct Mapper<'d> {
    files: HashMap<String, &'d Element>,
    links: HashMap<String, usize>,
    rate: f64,
    report: Report,
    skipped: Vec<String>,
    ids: Ids,
}

/// The clip a filter belongs to.
struct Ctx<'a> {
    name: &'a str,
}

impl Mapper<'_> {
    fn skip(&mut self, msg: String) {
        if !self.skipped.contains(&msg) {
            self.skipped.push(msg);
        }
    }

    fn track(&mut self, t: &Element, kind: &str, name: &str) -> Track {
        let cuts: Vec<Option<f64>> = t
            .children
            .iter()
            .map(|c| {
                (c.name == "transitionitem")
                    .then(|| cut_of(c).map(|x| x.1))
                    .flatten()
            })
            .collect();
        let mut group = Group::default();
        let mut foreign = Vec::new();
        for (i, c) in t.children.iter().enumerate() {
            match c.name.as_str() {
                "clipitem" | "generatoritem" => {
                    let prev = cuts[..i].iter().rev().find_map(|x| *x);
                    let next = cuts[i + 1..].iter().find_map(|x| *x);
                    if let Some((start, dur, item)) = self.item(c, name, prev, next) {
                        group.events.push(Event::Item { start, dur, item });
                    }
                }
                "transitionitem" => {
                    if let Some((cut, tr)) = self.transition(c, name) {
                        group.events.push(Event::Transition {
                            cut,
                            transition: tr,
                        });
                    }
                }
                "enabled" => {}
                n if c.child("start").is_some() || n.ends_with("item") => {
                    self.skip(format!("{name}: unknown item <{n}>"));
                }
                n => {
                    self.report.unmapped(format!("track/{n}"));
                    foreign.push(c.to_json());
                }
            }
        }
        let mut tr = Track::new(name, kind);
        tr.children = build_track(vec![group], self.rate);
        tr.enabled = flag(t, "enabled") != Some(false);
        let mut f = Map::new();
        if let Some(a) = t.attrs_except(&[]) {
            f.insert("attrs".into(), a);
        }
        if !foreign.is_empty() {
            f.insert("children".into(), foreign.into());
        }
        if !f.is_empty() {
            tr.set_foreign(APP, f.into());
        }
        tr
    }

    /// Timeline start, length and media in point of a clip or generator.
    fn timing(c: &Element, prev: Option<f64>, next: Option<f64>) -> Option<(f64, f64, f64)> {
        let (s, e) = (num(c, "start")?, num(c, "end")?);
        let inp = num(c, "in").unwrap_or(0.0);
        let len = num(c, "out").map(|o| o - inp);
        let s = if s < 0.0 {
            prev.or_else(|| Some(e - len?))?
        } else {
            s
        };
        let e = if e < 0.0 {
            next.or_else(|| Some(s + len?))?
        } else {
            e
        };
        (e > s).then_some((s, e - s, inp))
    }

    fn item(
        &mut self,
        c: &Element,
        track: &str,
        prev: Option<f64>,
        next: Option<f64>,
    ) -> Option<(f64, f64, Item)> {
        let name = txt(c, "name").unwrap_or_default();
        let Some((start, dur, src_in)) = Self::timing(c, prev, next) else {
            self.skip(format!(
                "{track}: <{}> {name:?} without a readable start/end",
                c.name
            ));
            return None;
        };
        let id = self.ids.make("fcp7", &format!("{track}@{start}"));
        let mut media: Option<MediaReference> = None;
        let mut text: Option<MediaReference> = None;
        let mut effects = Vec::new();
        let mut markers = Vec::new();
        let mut source = Map::new();
        let mut rest = Vec::new();
        let ctx = Ctx { name };
        for ch in &c.children {
            match ch.name.as_str() {
                "name" | "enabled" | "duration" | "start" | "end" | "in" | "out" | "link" => {}
                "rate" => {
                    if parse_rate(c).is_some_and(|r| (r - self.rate).abs() > 1e-6) {
                        self.report.warn(
                            "FCP7 clipitem rate differs from the sequence's: read at the sequence rate",
                        );
                    }
                }
                "file" => media = Some(self.file(ch, &mut source)),
                "effect" if c.name == "generatoritem" => media = Some(self.generator(ch, name)),
                "filter" => {
                    if let Some(g) = self.filter(ch, &ctx, &mut effects) {
                        text = Some(g);
                    }
                }
                "marker" => {
                    if let Some(m) = self.marker(ch) {
                        markers.push(m);
                    }
                }
                "logginginfo" => {
                    let info: Map<String, Value> = ch
                        .children
                        .iter()
                        .filter(|x| !x.text.is_empty())
                        .map(|x| (x.name.clone(), x.text.clone().into()))
                        .collect();
                    if !info.is_empty() {
                        source.insert("logginginfo".into(), info.into());
                    }
                }
                "sequence" => {
                    let n = txt(ch, "name").unwrap_or_default();
                    self.report.warn(format!(
                        "nested sequence {n:?} not expanded (kept under foreign)"
                    ));
                    self.report.unmapped("clipitem/sequence");
                    let mut m = MissingReference {
                        name: n.to_owned(),
                        ..MissingReference::default()
                    };
                    m.set_foreign(APP, ch.to_json());
                    media = Some(MediaReference::Missing(m));
                }
                n => {
                    self.report.unmapped(format!("{}/{n}", c.name));
                    rest.push(ch.to_json());
                }
            }
        }
        let media = text.or(media).unwrap_or_else(|| {
            MediaReference::Missing(MissingReference {
                name: name.to_owned(),
                ..MissingReference::default()
            })
        });
        let mut clip = Clip::new(name, media, Some(range(src_in, dur, self.rate)));
        clip.set_ffcine_id(id);
        clip.enabled = flag(c, "enabled") != Some(false);
        clip.effects = effects;
        clip.markers = markers;
        if let Some(g) = c.attr("id").and_then(|i| self.links.get(i)) {
            clip.ffcine_mut().insert("link_group".into(), (*g).into());
        }
        if !source.is_empty() {
            clip.ffcine_mut().insert("source".into(), source.into());
        }
        let mut f = Map::new();
        f.insert("element".into(), c.name.clone().into());
        if let Some(a) = c.attrs_except(&[]) {
            f.insert("attrs".into(), a);
        }
        for k in ["duration", "out"] {
            if let Some(x) = txt(c, k) {
                f.insert(k.into(), x.into());
            }
        }
        if !rest.is_empty() {
            f.insert("children".into(), rest.into());
        }
        clip.set_foreign(APP, f.into());
        Some((start, dur, Item::Clip(clip)))
    }

    /// A `file` (or a reference to one declared earlier) -> media reference, filling `source`.
    fn file(&mut self, el: &Element, source: &mut Map<String, Value>) -> MediaReference {
        let id = el.attr("id").unwrap_or_default();
        let f = if el.children.is_empty() {
            let Some(f) = self.files.get(id) else {
                self.report
                    .warn(format!("FCP7 file {id:?} referenced but never declared"));
                return MediaReference::Missing(MissingReference {
                    name: id.to_owned(),
                    ..MissingReference::default()
                });
            };
            *f
        } else {
            el
        };
        let name = txt(f, "name").unwrap_or_default();
        let avail = num(f, "duration").map(|d| range(0.0, d, self.rate));
        if let Some(tc) = f.child("timecode") {
            if let Some(r) = tc.child("reel").and_then(|r| txt(r, "name")) {
                source.insert("reel".into(), r.into());
            }
            if let Some(s) = txt(tc, "string") {
                source.insert("timecode".into(), s.into());
            }
        }
        let has_video = f.child("media").is_some_and(|m| m.child("video").is_some());
        source.insert(
            "kind".into(),
            if has_video { "video" } else { "audio" }.into(),
        );
        let mut rest = f.clone();
        rest.children
            .retain(|c| !matches!(c.name.as_str(), "name" | "pathurl" | "duration"));
        let foreign = rest.to_json();
        let Some(url) = txt(f, "pathurl").filter(|u| !u.is_empty()) else {
            self.report
                .warn(format!("FCP7 file {name:?} has no pathurl: media missing"));
            let mut m = MissingReference {
                name: name.to_owned(),
                available_range: avail,
                ..MissingReference::default()
            };
            m.set_foreign(APP, foreign);
            return MediaReference::Missing(m);
        };
        let url = match url.strip_prefix("file://localhost/") {
            Some(r) => format!("file:///{r}"),
            None => url.to_owned(),
        };
        source.insert("src".into(), url.clone().into());
        let mut e = ExternalReference::new(url, avail);
        name.clone_into(&mut e.name);
        e.set_foreign(APP, foreign);
        MediaReference::External(e)
    }

    fn anim(&mut self, p: Option<&Element>, conv: &dyn Fn(&Element) -> Option<Value>) -> Value {
        let Some(p) = p else {
            return Value::Null;
        };
        let mut eased = false;
        let keys: Vec<Value> = p
            .children_named("keyframe")
            .filter_map(|k| {
                let t = num(k, "when")?;
                let v = conv(k.child("value")?)?;
                let curve = k
                    .child("interpolation")
                    .and_then(|i| txt(i, "name"))
                    .is_some_and(|n| {
                        let n = n.to_ascii_lowercase();
                        n.contains("curve") || n.contains("bezier") || n.contains("smooth")
                    })
                    || k.child("inscale").is_some();
                eased |= curve;
                let interp = if curve { Interp::Ease } else { Interp::Linear };
                Some(keyframe(t, self.rate, v, interp))
            })
            .collect();
        if eased {
            self.report
                .warn("FCP7 curved keyframe interpolation approximated as ease");
        }
        if !keys.is_empty() {
            return Value::Array(keys);
        }
        p.child("value").and_then(conv).unwrap_or(Value::Null)
    }

    fn scaled(&mut self, e: &Element, id: &str, k: f64) -> Value {
        self.anim(param(e, id), &|v| scalar(v).map(|x| json!(x * k)))
    }

    /// One `filter` onto `effects`; a Premiere `GraphicAndType` returns the text generator.
    fn filter(
        &mut self,
        f: &Element,
        ctx: &Ctx<'_>,
        effects: &mut Vec<Effect>,
    ) -> Option<MediaReference> {
        let Some(e) = f.child("effect") else {
            self.report.unmapped("filter (no effect)");
            effects.push(foreign_effect("", "", APP, f.to_json()));
            return None;
        };
        let id = txt(e, "effectid").unwrap_or_default();
        let ename = txt(e, "name").unwrap_or(id);
        if flag(f, "enabled") == Some(false) {
            self.report.unmapped(format!("filter {id} (disabled)"));
            effects.push(foreign_effect(ename, id, APP, f.to_json()));
            return None;
        }
        let (mut fx, used): (Effect, &[&str]) = match id {
            "basic" => {
                self.report.warn(
                    "FCP7 basic motion: center/anchor as fractions of the frame, y down, rotation \
                     clockwise (unverified)",
                );
                let mut p = Map::new();
                p.insert("units".into(), "frame".into());
                p.insert("y".into(), "down".into());
                let s = self.scaled(e, "scale", 0.01);
                p.insert("scale_x".into(), s.clone());
                p.insert("scale_y".into(), s);
                p.insert("rotate_deg".into(), self.scaled(e, "rotation", -1.0));
                let c = self.anim(param(e, "center"), &point);
                p.insert("translate_x".into(), component(&c, 0));
                p.insert("translate_y".into(), component(&c, 1));
                p.insert("anchor".into(), self.anim(param(e, "centerOffset"), &point));
                (
                    ffcine_effect("ffcine.geo.transform/1", p),
                    &["scale", "rotation", "center", "centerOffset"],
                )
            }
            "crop" => {
                let mut p = Map::new();
                p.insert("units".into(), "frame".into());
                for side in ["left", "right", "top", "bottom"] {
                    p.insert(side.into(), self.scaled(e, side, 0.01));
                }
                (
                    ffcine_effect("ffcine.geo.crop/1", p),
                    &["left", "right", "top", "bottom"],
                )
            }
            "opacity" => {
                let mut p = Map::new();
                p.insert("opacity".into(), self.scaled(e, "opacity", 0.01));
                (ffcine_effect("ffcine.comp.blend/1", p), &["opacity"])
            }
            "audiolevels" => {
                let mut p = Map::new();
                let g = self.anim(param(e, "level"), &|v| {
                    scalar(v).map(|x| json!(linear_to_db(x)))
                });
                p.insert("gain_db".into(), g);
                (ffcine_effect("ffcine.audio.level/1", p), &["level"])
            }
            "audiopan" => {
                let mut p = Map::new();
                let g = self.anim(param(e, "pan"), &|v| {
                    scalar(v).map(|x| json!(if x.abs() > 1.0 { x / 100.0 } else { x }))
                });
                p.insert("pan".into(), g);
                (ffcine_effect("ffcine.audio.pan/1", p), &["pan"])
            }
            "timeremap" => {
                effects.extend(self.timeremap(e));
                return None;
            }
            "GraphicAndType" => return Some(self.graphic(e, ctx.name)),
            _ => {
                self.report.unmapped(format!("filter {id}"));
                effects.push(foreign_effect(ename, id, APP, f.to_json()));
                return None;
            }
        };
        if let Effect::Effect(g) = &mut fx {
            ename.clone_into(&mut g.name);
        }
        if let Some(r) = effect_rest(e, used, &mut self.report) {
            fx.insert_foreign(APP, "effect", r);
        }
        effects.push(fx);
        None
    }

    fn timeremap(&mut self, e: &Element) -> Vec<Effect> {
        let pv = |id: &str| param(e, id).and_then(|p| p.child("value"));
        let speed = pv("speed").and_then(scalar).unwrap_or(100.0);
        let reverse = pv("reverse").and_then(|v| truthy(&v.text)) == Some(true);
        let variable = pv("variablespeed").and_then(|v| truthy(&v.text)) == Some(true);
        let used = &["speed", "reverse", "variablespeed", "graphdict"];
        let rest = effect_rest(e, used, &mut self.report);
        if variable {
            let map: Vec<(f64, f64)> = param(e, "graphdict")
                .into_iter()
                .flat_map(|p| p.children_named("keyframe"))
                .filter_map(|k| Some((num(k, "when")?, k.child("value").and_then(scalar)?)))
                .collect();
            if let Some((ramp, avg)) = time_ramp(&map, self.rate, &mut self.report) {
                self.report
                    .warn("FCP7 variable speed (graphdict) mapped to ffcine.time.ramp/1 (approx)");
                let mut w = LinearTimeWarp::new(avg);
                if let Some(r) = rest {
                    w.set_foreign(APP, r);
                }
                return vec![ramp, Effect::LinearTimeWarp(w)];
            }
            self.report
                .warn("FCP7 variable speed without a usable graphdict: constant speed used");
        }
        let mut k = speed / 100.0;
        if reverse {
            k = -k.abs();
            self.report
                .warn("FCP7 reverse speed mapped to a negative LinearTimeWarp time_scalar");
        }
        if (k - 1.0).abs() < 1e-9 && rest.is_none() {
            return Vec::new();
        }
        let mut w = LinearTimeWarp::new(k);
        if let Some(r) = rest {
            w.set_foreign(APP, r);
        }
        vec![Effect::LinearTimeWarp(w)]
    }

    /// A `generatoritem`'s `effect` -> `ffcine.solid` / `ffcine.text`, or missing media.
    fn generator(&mut self, e: &Element, clip_name: &str) -> MediaReference {
        let id = txt(e, "effectid").unwrap_or_default();
        let cat = txt(e, "effectcategory").unwrap_or_default();
        let lid = id.to_ascii_lowercase();
        let mut p = Map::new();
        let (kind, used): (&str, &[&str]) = if lid == "color"
            || lid.contains("solid")
            || (cat.eq_ignore_ascii_case("matte") && param(e, "color").is_some())
        {
            p.insert("colour".into(), self.anim(param(e, "color"), &colour));
            ("ffcine.solid", &["color"])
        } else if cat.eq_ignore_ascii_case("text") || lid.contains("text") {
            let first = |ids: &[&str]| ids.iter().find_map(|i| param(e, i));
            let text = first(&["str", "text"])
                .and_then(|p| p.child("value"))
                .map_or_else(|| clip_name.to_owned(), |v| v.text.replace('\r', "\n"));
            p.insert("text".into(), text.into());
            if let Some(f) = first(&["fontname", "font"]).and_then(|p| p.child("value")) {
                p.insert("font".into(), f.text.clone().into());
            }
            p.insert(
                "size".into(),
                self.anim(first(&["fontsize", "size"]), &|v| scalar(v).map(Into::into)),
            );
            p.insert(
                "colour".into(),
                self.anim(first(&["fontcolor", "textcolor", "color"]), &colour),
            );
            p.insert(
                "position".into(),
                self.anim(first(&["origin", "center"]), &point),
            );
            if let Some(s) = first(&["fontstyle", "style"])
                .and_then(|p| p.child("value"))
                .map(|v| v.text.trim())
            {
                p.insert("bold".into(), matches!(s, "2" | "4").into());
                p.insert("italic".into(), matches!(s, "3" | "4").into());
            }
            if let Some(a) = first(&["fontalign", "alignment"])
                .and_then(|p| p.child("value"))
                .map(|v| v.text.trim())
            {
                let a = match a {
                    "1" => "left",
                    "3" => "right",
                    _ => "center",
                };
                p.insert("alignment".into(), a.into());
            }
            self.report
                .warn("FCP7 text generator: origin as fractions of the frame, y down (unverified)");
            (
                "ffcine.text",
                &[
                    "str",
                    "text",
                    "fontname",
                    "font",
                    "fontsize",
                    "size",
                    "fontcolor",
                    "textcolor",
                    "color",
                    "origin",
                    "center",
                    "fontstyle",
                    "style",
                    "fontalign",
                    "alignment",
                ],
            )
        } else {
            self.report.unmapped(format!("generator {id}"));
            self.report
                .warn(format!("FCP7 generator {id:?} not mapped: media missing"));
            let mut m = MissingReference {
                name: clip_name.to_owned(),
                ..MissingReference::default()
            };
            m.set_foreign(APP, e.to_json());
            return MediaReference::Missing(m);
        };
        let p = p.into_iter().filter(|(_, v)| !v.is_null()).collect();
        let mut g = GeneratorReference::new(kind, p);
        txt(e, "name").unwrap_or(id).clone_into(&mut g.name);
        let mut f = Map::new();
        f.insert("effectid".into(), id.into());
        if let Some(r) = effect_rest(e, used, &mut self.report) {
            f.insert("effect".into(), r);
        }
        g.set_foreign(APP, f.into());
        MediaReference::Generator(g)
    }

    /// Premiere `GraphicAndType` -> `ffcine.text` from its `Source Text` blob.
    fn graphic(&mut self, e: &Element, clip_name: &str) -> MediaReference {
        self.report
            .warn("Premiere GraphicAndType decoded best effort (text, font, size)");
        let blob = param(e, "Source Text")
            .and_then(|p| p.child("value"))
            .and_then(|v| premiere_text(&v.text));
        let mut p = Map::new();
        let get = |k: &str| blob.as_ref().and_then(|b| find_key(b, k)).cloned();
        let text = get("mText")
            .and_then(|t| t.as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                self.report
                    .warn("Premiere GraphicAndType Source Text unreadable: clip name used");
                clip_name.to_owned()
            });
        p.insert("text".into(), text.replace('\r', "\n").into());
        if let Some(f) = get("mFontName").filter(Value::is_string) {
            p.insert("font".into(), f);
        }
        if let Some(s) = get("mFontSize").filter(Value::is_number) {
            p.insert("size".into(), s);
        }
        let mut g = GeneratorReference::new("ffcine.text", p);
        clip_name.clone_into(&mut g.name);
        let mut f = Map::new();
        f.insert("effectid".into(), "GraphicAndType".into());
        f.insert("effect".into(), e.to_json());
        g.set_foreign(APP, f.into());
        MediaReference::Generator(g)
    }

    fn transition(&mut self, t: &Element, track: &str) -> Option<(f64, Transition)> {
        let Some((s, cut, e)) = cut_of(t) else {
            self.skip(format!(
                "{track}: transitionitem without a readable start/end"
            ));
            return None;
        };
        let mut tr = Transition::dissolve(rt(cut - s, self.rate), rt(e - cut, self.rate));
        let eff = t.child("effect");
        let id = eff.and_then(|x| txt(x, "effectid")).unwrap_or_default();
        let name = eff.and_then(|x| txt(x, "name")).unwrap_or(id);
        name.clone_into(&mut tr.name);
        let lid = id.to_ascii_lowercase();
        let kind = if lid.is_empty() || lid.contains("cross dissolve") || lid == "dissolve" {
            None
        } else if lid.starts_with("kgaudiotranscrossfade") {
            let db = if lid.ends_with("0db") { 0.0 } else { 3.0 };
            Some(json!({"schema": "ffcine.transition/1", "kind": "cross_fade", "crossfade_db": db}))
        } else {
            self.report.warn(format!(
                "FCP7 transition {id:?} rendered as a dissolve (kind kept)"
            ));
            Some(json!({"schema": "ffcine.transition/1", "kind": id}))
        };
        if let Some(Value::Object(k)) = kind {
            tr.ffcine_mut().extend(k);
        }
        let mut f = Map::new();
        if let Some(a) = t.attrs_except(&[]) {
            f.insert("attrs".into(), a);
        }
        let rest: Vec<Value> = t
            .children
            .iter()
            .filter(|c| !matches!(c.name.as_str(), "start" | "end" | "rate"))
            .map(Element::to_json)
            .collect();
        if !rest.is_empty() {
            f.insert("children".into(), rest.into());
        }
        if !f.is_empty() {
            tr.set_foreign(APP, f.into());
        }
        Some((cut, tr))
    }

    fn marker(&mut self, m: &Element) -> Option<Marker> {
        let name = txt(m, "name").unwrap_or_default();
        let Some(inp) = num(m, "in") else {
            self.skip(format!("marker {name:?} without a readable in"));
            return None;
        };
        let out = num(m, "out").filter(|o| *o >= inp).unwrap_or(inp);
        let c = m.child("color").and_then(colour).and_then(|c| {
            let f = |i: usize| c.get(i).and_then(Value::as_f64);
            Some(marker_color(f(0)?, f(1)?, f(2)?))
        });
        let mut mk = Marker::new(name, range(inp, out - inp, self.rate), c.unwrap_or("RED"));
        if let Some(cm) = txt(m, "comment").filter(|x| !x.is_empty()) {
            mk.extra.insert("comment".into(), cm.into());
        }
        let rest: Vec<Value> = m
            .children
            .iter()
            .filter(|c| !matches!(c.name.as_str(), "name" | "in" | "out" | "comment" | "color"))
            .map(Element::to_json)
            .collect();
        if !rest.is_empty() {
            for r in &m.children {
                if !matches!(r.name.as_str(), "name" | "in" | "out" | "comment" | "color") {
                    self.report.unmapped(format!("marker/{}", r.name));
                }
            }
            mk.set_foreign(APP, json!({ "children": rest }));
        }
        Some(mk)
    }
}

fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(o) => o
            .get(key)
            .or_else(|| o.values().find_map(|x| find_key(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

/// Premiere `Source Text`: base64 of a little-endian length prefix and UTF-16LE JSON.
fn premiere_text(b64: &str) -> Option<Value> {
    let bytes = base64(b64)?;
    for skip in [8, 4, 0] {
        let Some(body) = bytes.get(skip..) else {
            continue;
        };
        let units: Vec<u16> = body
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16_lossy(&units);
        if let (Some(a), Some(b)) = (s.find('{'), s.rfind('}'))
            && a < b
            && let Ok(v) = serde_json::from_str(&s[a..=b])
        {
            return Some(v);
        }
    }
    None
}

/// Standard base64 (padding and whitespace ignored).
fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes() {
        assert_eq!(base64("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64("aGk").as_deref(), Some(&b"hi"[..]));
        assert!(base64("a*b").is_none());
    }
}
