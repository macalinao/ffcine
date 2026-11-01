//! FCPXML 1.10 exporter, the inverse of [`crate::fcpxml`]; Resolve (`File > Import > Timeline`)
//! and Final Cut Pro both read it.
//!
//! Layout: `<fcpxml><resources>` (one `format` for the sequence, an `asset` per media file, an
//! `effect` per transition, title and colour filter kind, a `media` per nested `Stack`)
//! `<library><event><project><sequence><spine>`.
//!
//! - The first video track is the primary storyline. Every other track is a secondary storyline
//!   (`<spine lane="n">`) connected to the primary's first item: video tracks above it at lanes
//!   1, 2, ..., audio tracks below at −1, −2, ... (audio clips get `srcEnable="audio"`).
//! - `Clip` -> `asset-clip` (`start` = the media in point), `Gap` -> `gap`, `Stack` ->
//!   `ref-clip` of a compound `media`, `ffcine.text` -> `title` (Basic Title, `text-style-def`,
//!   `Position`), `Transition` -> `transition` with a Cross Dissolve (other kinds are written as
//!   dissolves and reported).
//! - `LinearTimeWarp` / `FreezeFrame` -> a two-point `timeMap`; `ffcine.time.ramp/1` -> a
//!   `timeMap` with a point per ramp point (linear between them).
//! - `ffcine.geo.transform/1` -> `adjust-transform`, `ffcine.geo.crop/1` -> `adjust-crop` (`trim`,
//!   or `crop` when `fill`), `ffcine.comp.blend/1` -> `adjust-blend`, `ffcine.audio.level/1
//!   {gain_db}` -> `adjust-volume`, the clip's CDL ([`super`]) -> `filter-video` "Color Board"
//!   with `info-asc-cdl`. Keyframes become `keyframeAnimation`s (vector parameters keyed at the
//!   union of their components' keys); `hold` keys are written as linear.
//! - Clip markers -> `marker` (`completed` from the colour: green 1, red 0) or `chapter-marker`
//!   (`metadata.ffcine.chapter` or an orange marker).
//! - `metadata.ffcine.foreign.fcpx` is written back where the reader put it: timeline (project,
//!   sequence and format attributes, event, library), items (attributes and child elements),
//!   assets, `adjust-*` leftovers, title styles and parameters, marker attributes, unmapped
//!   filters (`filter-video` / `filter-audio` with their `effect` resource).
//!
//! Units are the reader's, which are *unverified* against FCP: `adjust-transform` position and
//! anchor in percent of the frame height (+y up), crop edges in percent of the frame height,
//! rotation counter-clockwise in degrees, title `Position` in pixels (+y up). Geometry in
//! `"units": "picture"` (Resolve) is written as if the picture filled the frame. Both the
//! canonical transform fields (`translate_x/y`, `scale_x/y`, `rotate_deg`) and the readers' older
//! ones (`position`, `scale`, `rotation` clockwise) are accepted.
//!
//! Not written (reported): audio pan and fades, other ffcine effects (grade, stabilize, ...),
//! markers on tracks or the timeline, track effects, generators other than `ffcine.text`.

#![allow(clippy::many_single_char_names)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value};

use super::{Anim, ExportOptions, ExportReport, clip_start, ffcine_effect, num, xml_escape};
use ffcine_core::Result;
use ffcine_timeline::{
    Clip, Effect, HasMetadata, Item, Marker, MediaReference, Stack, Timeline, Track, track_kind,
};

const APP: &str = "fcpx";
const CROSS_DISSOLVE_UID: &str = "FxPlug:4731E73A-8DAC-4113-9A30-AE85B1761265";
const BASIC_TITLE_UID: &str =
    ".../Titles.localized/Bumper:Opener.localized/Basic Title.localized/Basic Title.moti";
const POSITION_KEY: &str = "9999/999166631/999166633/1/100/101";

/// An XML element being built.
#[derive(Debug, Clone, Default)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
    text: String,
}

impl Node {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    fn a(mut self, k: &str, v: impl Into<String>) -> Self {
        self.set(k, v);
        self
    }

    fn set(&mut self, k: &str, v: impl Into<String>) {
        let v = v.into();
        match self.attrs.iter_mut().find(|(x, _)| x == k) {
            Some(slot) => slot.1 = v,
            None => self.attrs.push((k.into(), v)),
        }
    }

    fn has(&self, k: &str) -> bool {
        self.attrs.iter().any(|(x, _)| x == k)
    }

    /// Adds attributes from a JSON object that aren't set yet.
    fn merge_attrs(&mut self, attrs: Option<&Value>) {
        for (k, v) in attrs.and_then(Value::as_object).into_iter().flatten() {
            if !self.has(k)
                && let Some(s) = v.as_str()
            {
                self.attrs.push((k.clone(), s.into()));
            }
        }
    }

    fn child(mut self, c: Node) -> Self {
        self.children.push(c);
        self
    }

    /// From the readers' JSON form `{name, attrs, children, text}`.
    fn from_json(v: &Value) -> Option<Self> {
        let mut n = Node::new(v.get("name")?.as_str()?);
        n.merge_attrs(v.get("attrs"));
        for c in v
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            n.children.extend(Node::from_json(c));
        }
        if let Some(t) = v.get("text").and_then(Value::as_str) {
            t.clone_into(&mut n.text);
        }
        Some(n)
    }

    /// Whether this element or one inside it points at a resource.
    fn has_ref(&self) -> bool {
        self.has("ref") || self.children.iter().any(Node::has_ref)
    }

    fn write(&self, out: &mut String, depth: usize) {
        let pad = "    ".repeat(depth);
        let _ = write!(out, "{pad}<{}", self.name);
        for (k, v) in &self.attrs {
            let _ = write!(out, " {k}=\"{}\"", xml_escape(v));
        }
        if self.children.is_empty() && self.text.is_empty() {
            out.push_str("/>\n");
        } else if self.children.is_empty() {
            let _ = writeln!(out, ">{}</{}>", xml_escape(&self.text), self.name);
        } else {
            out.push_str(">\n");
            if !self.text.is_empty() {
                let _ = writeln!(out, "{pad}    {}", xml_escape(&self.text));
            }
            for c in &self.children {
                c.write(out, depth + 1);
            }
            let _ = writeln!(out, "{pad}</{}>", self.name);
        }
    }
}

/// Frame duration as `num/den` seconds: NTSC rates `1001/(n·1000)`, others `1/round(rate)`.
fn frame_duration(rate: f64) -> (u64, u64) {
    let ntsc = rate * 1001.0 / 1000.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    if (ntsc - ntsc.round()).abs() < 1e-3 && (rate - rate.round()).abs() > 1e-3 {
        (1001, ntsc.round() as u64 * 1000)
    } else if (rate - rate.round()).abs() < 1e-6 {
        (1, rate.round() as u64)
    } else {
        // Any other rate: millisecond-ish ticks.
        (1000, (rate * 1000.0).round() as u64)
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// A time in frames at the timeline rate as an FCPXML rational (`1001/24000s`), kept to a
/// millionth of a frame.
fn fcp_time(frames: f64, (n, d): (u64, u64)) -> String {
    if frames.abs() < 1e-9 {
        return "0s".into();
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let milli = (frames.abs() * 1e6).round() as u64;
    let (num_, den) = (milli * n, d * 1_000_000);
    let g = gcd(num_, den).max(1);
    let (num_, den) = (num_ / g, den / g);
    let sign = if frames < 0.0 { "-" } else { "" };
    if den == 1 {
        format!("{sign}{num_}s")
    } else {
        format!("{sign}{num_}/{den}s")
    }
}

/// A time attribute of a written element, in frames.
fn time_of(n: &Node, key: &str, rate: f64) -> f64 {
    n.attrs
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| crate::fcpxml::parse_time(v))
        .map_or(0.0, |s| s * rate)
}

struct Ctx<'a> {
    rate: f64,
    fd: (u64, u64),
    w: f64,
    h: f64,
    report: &'a mut ExportReport,
    resources: Vec<Node>,
    next_id: u32,
    assets: HashMap<String, (String, usize)>,
    effects: HashMap<String, String>,
}

impl Ctx<'_> {
    fn t(&self, frames: f64) -> String {
        fcp_time(frames, self.fd)
    }

    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("r{}", self.next_id)
    }

    /// The id of an `effect` resource, added once per `uid`.
    fn effect(&mut self, name: &str, uid: &str, extra: Option<&Value>) -> String {
        if let Some(id) = self.effects.get(uid) {
            return id.clone();
        }
        let id = self.id();
        let mut n = Node::new("effect")
            .a("id", &id)
            .a("name", name)
            .a("uid", uid);
        n.merge_attrs(extra);
        self.resources.push(n);
        self.effects.insert(uid.into(), id.clone());
        id
    }

    /// An effect resource from the reader's JSON (`{name: "effect", attrs: {id, name, uid}}`).
    fn effect_json(&mut self, v: &Value) -> Option<String> {
        let attrs = v.get("attrs")?;
        let name = attrs
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let uid = attrs.get("uid").and_then(Value::as_str).unwrap_or(name);
        let mut rest = attrs.clone();
        if let Some(o) = rest.as_object_mut() {
            o.remove("id");
        }
        Some(self.effect(name, uid, Some(&rest)))
    }

    /// The asset for a clip's media, extended to cover `[start, end)` (frames).
    fn asset(&mut self, clip: &Clip, start: f64, end: f64, audio: bool) -> String {
        let (key, name, src, avail, foreign) = match clip.media_reference() {
            Some(MediaReference::External(e)) => (
                e.target_url.clone(),
                if e.name.is_empty() {
                    clip.name.clone()
                } else {
                    e.name.clone()
                },
                Some(e.target_url.clone()),
                e.available_range.clone(),
                e.foreign(APP).cloned(),
            ),
            Some(MediaReference::Missing(m)) => (
                format!("missing:{}:{}", m.name, clip.name),
                if m.name.is_empty() {
                    clip.name.clone()
                } else {
                    m.name.clone()
                },
                None,
                m.available_range.clone(),
                None,
            ),
            _ => (
                format!("other:{}", clip.name),
                clip.name.clone(),
                None,
                None,
                None,
            ),
        };
        if let Some((id, i)) = self.assets.get(&key).cloned() {
            let n = &mut self.resources[i];
            n.set(if audio { "hasAudio" } else { "hasVideo" }, "1");
            if avail.is_none() {
                widen(n, start, end, self.rate, self.fd);
            }
            return id;
        }
        let id = self.id();
        let (s, e) = avail.as_ref().map_or((start, end), |r| {
            let s = r.start_time.rescaled_to(self.rate).value;
            (s, s + r.duration.rescaled_to(self.rate).value)
        });
        let mut n = Node::new("asset")
            .a("id", &id)
            .a("name", name)
            .a("start", self.t(s))
            .a("duration", self.t(e - s))
            .a(if audio { "hasAudio" } else { "hasVideo" }, "1");
        if !audio {
            n.set("format", "r1");
        }
        n.merge_attrs(foreign.as_ref().and_then(|f| f.get("attrs")));
        match src {
            Some(src) => n.children.push(
                Node::new("media-rep")
                    .a("kind", "original-media")
                    .a("src", src),
            ),
            None => self.report.warn(format!(
                "{:?}: media missing, written as an asset without a file",
                clip.name
            )),
        }
        for c in foreign
            .as_ref()
            .and_then(|f| f.get("children"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if c.get("name").and_then(Value::as_str) != Some("media-rep") {
                n.children.extend(Node::from_json(c));
            }
        }
        self.resources.push(n);
        self.assets
            .insert(key, (id.clone(), self.resources.len() - 1));
        id
    }
}

/// Grows an asset's `start`/`duration` to cover `[start, end)` frames.
fn widen(n: &mut Node, start: f64, end: f64, rate: f64, fd: (u64, u64)) {
    let secs = |k: &str| {
        n.attrs
            .iter()
            .find(|(x, _)| x == k)
            .and_then(|(_, v)| crate::fcpxml::parse_time(v))
    };
    let (s0, d0) = (
        secs("start").unwrap_or(0.0) * rate,
        secs("duration").unwrap_or(0.0) * rate,
    );
    let (s, e) = (s0.min(start), (s0 + d0).max(end));
    n.set("start", fcp_time(s, fd));
    n.set("duration", fcp_time(e - s, fd));
}

/// FCPXML 1.10 for `tl`.
pub fn export(tl: &Timeline, opts: &ExportOptions) -> Result<String> {
    export_report(tl, opts).map(|(s, _)| s)
}

/// Writes [`export`] to `path`.
pub fn to_file(tl: &Timeline, opts: &ExportOptions, path: impl AsRef<Path>) -> Result<()> {
    super::write_file(path.as_ref(), &export(tl, opts)?)
}

/// [`export`] and what it couldn't write exactly.
pub fn export_report(tl: &Timeline, opts: &ExportOptions) -> Result<(String, ExportReport)> {
    let mut report = ExportReport::default();
    let rate = super::timeline_rate(tl, opts);
    let (w, h) = super::frame_size(tl, opts).unwrap_or_else(|| {
        report.warn("no frame size on the timeline or in the options: wrote 1920x1080");
        (1920, 1080)
    });
    let project = tl.project().and_then(Result::ok);
    let fd = frame_duration(rate);
    let foreign = tl.foreign(APP).cloned().unwrap_or_default();
    let mut c = Ctx {
        rate,
        fd,
        w: f64::from(w),
        h: f64::from(h),
        report: &mut report,
        resources: Vec::new(),
        next_id: 1,
        assets: HashMap::new(),
        effects: HashMap::new(),
    };
    let mut format = Node::new("format")
        .a("id", "r1")
        .a("frameDuration", format!("{}/{}s", fd.0, fd.1))
        .a("width", w.to_string())
        .a("height", h.to_string());
    format.merge_attrs(foreign.get("format").and_then(|f| f.get("attrs")));
    if !format.has("name") {
        format.set("name", "FFVideoFormatRateUndefined");
    }

    let tc_start = tl
        .global_start_time
        .as_ref()
        .map_or(0.0, |t| t.rescaled_to(rate).value);
    let tracks: Vec<&Track> = tl.track_list().collect();
    let mut b = Builder {
        c: &mut c,
        project: project.as_ref(),
    };
    let (spine, dur) = b.sequence(&tracks, tc_start);
    let mut seq = Node::new("sequence")
        .a("format", "r1")
        .a("duration", c.t(dur))
        .a("tcStart", c.t(tc_start))
        .a("tcFormat", "NDF")
        .a("audioLayout", "stereo")
        .a("audioRate", "48k");
    seq.merge_attrs(foreign.get("sequence"));
    seq.children.push(spine);
    for x in foreign
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        seq.children.extend(Node::from_json(x));
    }
    for m in tl
        .tracks
        .markers
        .iter()
        .chain(tracks.iter().flat_map(|t| &t.markers))
    {
        c.report
            .unmapped([format!("marker {:?} on a track or the timeline", m.name)]);
    }
    let mut proj = Node::new("project").a(
        "name",
        if tl.name.is_empty() {
            "ffcine"
        } else {
            &tl.name
        },
    );
    proj.merge_attrs(foreign.get("project"));
    proj.children.push(seq);
    let event_name = foreign
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("ffcine");
    let event = Node::new("event").a("name", event_name).child(proj);
    let mut library = Node::new("library");
    if let Some(l) = foreign.get("library").and_then(Value::as_str) {
        library.set("location", l);
    }
    library.children.push(event);

    let mut resources = Node::new("resources").child(format);
    resources.children.append(&mut c.resources);
    let root = Node::new("fcpxml")
        .a("version", "1.10")
        .child(resources)
        .child(library);
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE fcpxml>\n\n");
    root.write(&mut out, 0);
    Ok((out, report))
}

struct Builder<'a, 'c> {
    c: &'a mut Ctx<'c>,
    project: Option<&'a ffcine_timeline::Project>,
}

/// Local start (the `start` attribute) of a written story element, in frames.
fn local_start(n: &Node, rate: f64) -> f64 {
    time_of(n, "start", rate)
}

impl Builder<'_, '_> {
    /// The primary spine for a sequence's tracks (bottom first) and its duration in frames.
    /// `local0` is the spine's local time at its first frame (the sequence's `tcStart`).
    fn sequence(&mut self, tracks: &[&Track], local0: f64) -> (Node, f64) {
        let rate = self.c.rate;
        let primary = tracks.iter().position(|t| t.kind == track_kind::VIDEO);
        let mut spine = Node::new("spine");
        let mut dur = 0.0;
        if let Some(p) = primary {
            let (items, d) = self.storyline(&tracks[p].children, false);
            spine.children = items;
            dur = d;
        }
        let primary_len = dur;
        let mut lanes = Vec::new();
        let (mut up, mut down) = (0, 0);
        for (i, t) in tracks.iter().enumerate() {
            if Some(i) == primary {
                continue;
            }
            let audio = t.kind == track_kind::AUDIO;
            let lane = if audio {
                down -= 1;
                down
            } else {
                up += 1;
                up
            };
            if !t.effects.is_empty() {
                self.c
                    .report
                    .unmapped([format!("effects on track {:?}", t.name)]);
            }
            let (items, d) = self.storyline(&t.children, audio);
            dur = f64::max(dur, d);
            lanes.push((lane, audio, items));
        }
        let total: f64 = dur;
        // The primary runs the whole length so everything has something to connect to.
        if total > primary_len + 1e-6 {
            spine.children.push(
                Node::new("gap")
                    .a("name", "Gap")
                    .a("offset", self.c.t(primary_len))
                    .a("duration", self.c.t(total - primary_len)),
            );
        }
        for n in &mut spine.children {
            let off = time_of(n, "offset", rate);
            n.set("offset", fcp_time(off + local0, self.c.fd));
        }
        for (lane, audio, mut items) in lanes {
            if audio {
                // Audio clips connect one by one: a storyline with gaps would read as video.
                for mut n in items {
                    match n.name.as_str() {
                        "gap" => continue,
                        "transition" => {
                            self.c
                                .report
                                .warn("transitions on audio tracks are not written to FCPXML");
                            continue;
                        }
                        _ => {}
                    }
                    let at = time_of(&n, "offset", rate);
                    n.set("lane", lane.to_string());
                    attach(&mut spine, n, at, local0, rate, self.c.fd);
                }
                continue;
            }
            // A leading gap becomes the storyline's offset.
            let mut lead = 0.0;
            if items.first().is_some_and(|n| n.name == "gap") {
                lead = time_of(&items.remove(0), "duration", rate);
                for n in &mut items {
                    let off = time_of(n, "offset", rate);
                    n.set("offset", fcp_time(off - lead, self.c.fd));
                }
            }
            if items.is_empty() {
                continue;
            }
            let mut st = Node::new("spine").a("lane", lane.to_string());
            st.children = items;
            attach(&mut spine, st, lead, local0, rate, self.c.fd);
        }
        (spine, total)
    }

    /// Story elements of a track, offsets from 0, and the track's length in frames.
    fn storyline(&mut self, items: &[Item], audio: bool) -> (Vec<Node>, f64) {
        let rate = self.c.rate;
        let mut out = Vec::new();
        let mut cursor = 0.0;
        for item in items {
            let d = super::item_frames(item, rate);
            match item {
                Item::Clip(c) => out.push(self.clip(c, cursor, d, audio)),
                Item::Gap(g) => {
                    let mut n = Node::new("gap")
                        .a("name", if g.name.is_empty() { "Gap" } else { &g.name })
                        .a("offset", self.c.t(cursor))
                        .a("duration", self.c.t(d));
                    self.restore_item(&mut n, g);
                    out.push(n);
                }
                Item::Stack(s) => out.push(self.stack(s, cursor, d, audio)),
                Item::Transition(t) => {
                    let (i, o) = (
                        t.in_offset.rescaled_to(rate).value,
                        t.out_offset.rescaled_to(rate).value,
                    );
                    let kind = t
                        .ffcine_value("kind")
                        .or_else(|| t.ffcine_value("transition").and_then(|x| x.get("kind")))
                        .and_then(Value::as_str);
                    if kind.is_some_and(|k| k != "dissolve") {
                        self.c.report.warn(format!(
                            "transition {:?} ({}) written as a Cross Dissolve",
                            t.name,
                            kind.unwrap_or_default()
                        ));
                    }
                    let fx = self.c.effect("Cross Dissolve", CROSS_DISSOLVE_UID, None);
                    let mut n = Node::new("transition")
                        .a(
                            "name",
                            if t.name.is_empty() {
                                "Cross Dissolve"
                            } else {
                                &t.name
                            },
                        )
                        .a("offset", self.c.t(cursor - i))
                        .a("duration", self.c.t(i + o))
                        .child(
                            Node::new("filter-video")
                                .a("ref", fx)
                                .a("name", "Cross Dissolve"),
                        );
                    n.merge_attrs(t.foreign(APP).and_then(|f| f.get("attrs")));
                    out.push(n);
                }
                Item::Track(_) | Item::Unknown(_) => {
                    self.c
                        .report
                        .unmapped([format!("{:?} in a track", item.name())]);
                    out.push(
                        Node::new("gap")
                            .a("name", "Gap")
                            .a("offset", self.c.t(cursor))
                            .a("duration", self.c.t(d)),
                    );
                }
            }
            cursor += d;
        }
        (out, cursor)
    }

    /// Attributes and children from `foreign.fcpx` (children pointing at resources are dropped).
    fn restore_item(&mut self, n: &mut Node, obj: &impl HasMetadata) {
        let Some(f) = obj.foreign(APP) else { return };
        n.merge_attrs(f.get("attrs"));
        for c in f
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(c) = Node::from_json(c) else {
                continue;
            };
            if c.has_ref() {
                self.c.report.unmapped([format!(
                    "fcpx <{}> under {:?} (refers to a resource)",
                    c.name,
                    n.attrs
                        .iter()
                        .find(|(k, _)| k == "name")
                        .map_or("", |(_, v)| v.as_str())
                )]);
                continue;
            }
            n.children.push(c);
        }
    }

    fn stack(&mut self, s: &Stack, at: f64, d: f64, audio: bool) -> Node {
        let rate = self.c.rate;
        let tracks: Vec<&Track> = s
            .children
            .iter()
            .filter_map(|i| match i {
                Item::Track(t) => Some(t),
                _ => None,
            })
            .collect();
        let (spine, inner) = self.sequence(&tracks, 0.0);
        let id = self.c.id();
        let name = if s.name.is_empty() {
            "Compound Clip"
        } else {
            &s.name
        };
        let seq = Node::new("sequence")
            .a("format", "r1")
            .a("duration", self.c.t(inner))
            .a("tcStart", "0s")
            .a("tcFormat", "NDF")
            .child(spine);
        self.c
            .resources
            .push(Node::new("media").a("id", &id).a("name", name).child(seq));
        let start = s
            .source_range
            .as_ref()
            .map_or(0.0, |r| r.start_time.rescaled_to(rate).value);
        let mut n = Node::new("ref-clip")
            .a("ref", id)
            .a("name", name)
            .a("offset", self.c.t(at))
            .a("duration", self.c.t(d));
        if start.abs() > 1e-9 {
            n.set("start", self.c.t(start));
        }
        if audio {
            n.set("srcEnable", "audio");
        }
        if !s.enabled {
            n.set("enabled", "0");
        }
        let (adjust, filters) = self.effects(&s.effects, 0.0, 0.0, None);
        n.children.extend(adjust);
        n.children.extend(filters);
        self.restore_item(&mut n, s);
        n
    }

    fn clip(&mut self, clip: &Clip, at: f64, d: f64, audio: bool) -> Node {
        let rate = self.c.rate;
        let src_in = clip
            .source_range
            .as_ref()
            .map_or(0.0, |r| r.start_time.rescaled_to(rate).value);
        let mut n = match clip.media_reference() {
            Some(MediaReference::Generator(g)) if g.generator_kind == "ffcine.text" => {
                self.title(clip, g, src_in)
            }
            Some(MediaReference::Generator(g)) => {
                self.c.report.unmapped([format!(
                    "generator {} ({:?}), written as a gap",
                    g.generator_kind, clip.name
                )]);
                Node::new("gap").a("name", &clip.name)
            }
            _ => {
                let speed = speed(clip);
                let end = src_in + d * speed.unwrap_or(1.0).abs().max(1e-9);
                let r = self.c.asset(clip, src_in, end, audio);
                // Always `start`: without it the reader takes the asset's.
                let mut n = Node::new("asset-clip")
                    .a("ref", r)
                    .a("name", &clip.name)
                    .a("start", self.c.t(src_in));
                if audio {
                    n.set("srcEnable", "audio");
                }
                n
            }
        };
        n.set("offset", self.c.t(at));
        n.set("duration", self.c.t(d));
        // `offset` and `duration` first, as FCP writes them.
        n.attrs.sort_by_key(|(k, _)| match k.as_str() {
            "ref" => 0,
            "offset" => 1,
            "name" => 2,
            "start" => 3,
            "duration" => 4,
            _ => 5,
        });
        if !clip.enabled {
            n.set("enabled", "0");
        }
        let speed = if n.name == "asset-clip" {
            self.time_map(clip, src_in, d)
        } else {
            None
        };
        if let Some(tm) = &speed {
            n.children.insert(0, tm.0.clone());
        }
        let k = speed.as_ref().map_or(1.0, |s| s.1);
        let cdl = if n.name == "asset-clip" {
            super::clip_cdl(clip, self.project, self.c.report)
        } else {
            None
        };
        let (adjust, filters) = self.effects(&clip.effects, src_in, clip_start(clip), cdl);
        n.children.extend(adjust);
        for m in &clip.markers {
            n.children.push(self.marker(m, src_in, k));
        }
        n.children.extend(filters);
        self.restore_item(&mut n, clip);
        n
    }

    /// The Color Board `filter-video` for a CDL, on top of the reader's leftovers (`rest`).
    fn cdl_filter(&mut self, cdl: &super::Cdl, rest: Option<&Value>) -> Node {
        let fx = self.c.effect("Color Board", "FFColorBoard", None);
        let mut info = Node::new("info-asc-cdl")
            .a("slope", three(cdl.slope))
            .a("offset", three(cdl.offset))
            .a("power", three(cdl.power));
        if let Some(s) = cdl.saturation {
            info.set("sat", num(s));
        }
        let mut f = rest
            .and_then(Node::from_json)
            .filter(|n| n.name == "filter-video")
            .unwrap_or_else(|| Node::new("filter-video").a("name", "Color Board"));
        f.set("ref", fx);
        f.children.push(info);
        f
    }

    /// `timeMap` for a retimed clip and its average speed.
    fn time_map(&mut self, clip: &Clip, src_in: f64, d: f64) -> Option<(Node, f64)> {
        let rate = self.c.rate;
        let ramp = clip.effects.iter().find_map(|e| match ffcine_effect(e) {
            Some(("ffcine.time.ramp", p)) => Some(p),
            _ => None,
        });
        let k = speed(clip).or(ramp.map(|_| 1.0))?;
        let mut tm = Node::new("timeMap");
        let pt = |c: &Ctx, t: f64, v: f64| {
            Node::new("timept")
                .a("time", c.t(t))
                .a("value", c.t(v))
                .a("interp", "linear")
        };
        // Clip time runs from `src_in` (the clip's `start`) so its first frame is the media in.
        let mut pts = vec![(src_in, src_in)];
        if let Some(p) = ramp
            && let (Some(points), Some(sd)) = (
                p.get("points").and_then(Value::as_array),
                p.get("source_duration").and_then(Value::as_f64),
            )
        {
            let src = sd * rate;
            let pts_in: Vec<(f64, f64)> = points
                .iter()
                .filter_map(|x| Some((x.get(0)?.as_f64()?, x.get(1)?.as_f64()?)))
                .collect();
            let (mut t, mut v) = (src_in, src_in);
            for w in pts_in.windows(2) {
                let ((u0, s0), (u1, s1)) = (w[0], w[1]);
                let ds = (u1 - u0) * src;
                if ds <= 0.0 || s0 <= 0.0 || s1 <= 0.0 {
                    continue;
                }
                // Speed linear in source position: dt = ds·ln(s1/s0)/(s1 − s0).
                let dt = if (s1 - s0).abs() < 1e-9 {
                    ds / s0
                } else {
                    ds * (s1 / s0).ln() / (s1 - s0)
                };
                t += dt;
                v += ds;
                pts.push((t, v));
            }
            if pts.len() < 2 {
                pts.push((src_in + d, src_in + d * k));
            }
        } else {
            pts.push((src_in + d, src_in + d * k));
        }
        for (t, v) in pts {
            tm.children.push(pt(self.c, t, v));
        }
        Some((tm, k))
    }

    fn title(&mut self, clip: &Clip, g: &ffcine_timeline::GeneratorReference, src_in: f64) -> Node {
        let p = &g.parameters;
        let gf = g.foreign(APP);
        let fx = gf
            .and_then(|f| f.get("effect"))
            .and_then(|e| self.c.effect_json(e))
            .unwrap_or_else(|| self.c.effect("Basic Title", BASIC_TITLE_UID, None));
        let mut n = Node::new("title")
            .a("ref", fx)
            .a(
                "name",
                if clip.name.is_empty() {
                    &g.name
                } else {
                    &clip.name
                },
            )
            .a("start", self.c.t(src_in));
        if let Some([x, y]) = p
            .get("position")
            .and_then(Value::as_array)
            .and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]))
        {
            n.children.push(
                Node::new("param")
                    .a("name", "Position")
                    .a("key", POSITION_KEY)
                    .a(
                        "value",
                        format!("{} {}", num(x * self.c.w), num(-y * self.c.h)),
                    ),
            );
        }
        for x in gf
            .and_then(|f| f.get("params"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            n.children.extend(Node::from_json(x));
        }
        self.c.next_id += 1;
        let ts = format!("ts{}", self.c.next_id);
        let text = p.get("text").and_then(Value::as_str).unwrap_or_default();
        let mut span = Node::new("text-style").a("ref", &ts);
        span.text = text.into();
        n.children.push(Node::new("text").child(span));
        let mut style = Node::new("text-style");
        if let Some(f) = p.get("font").and_then(Value::as_str) {
            style.set("font", f);
        }
        if let Some(s) = p.get("size").and_then(Value::as_f64) {
            style.set("fontSize", num(s).trim_end_matches(".0"));
        }
        if let Some(f) = p.get("face").and_then(Value::as_str) {
            style.set("fontFace", f);
        }
        if let Some(c) = p.get("colour").and_then(Value::as_array) {
            let c: Vec<String> = c.iter().filter_map(Value::as_f64).map(num).collect();
            style.set("fontColor", c.join(" "));
        }
        for k in ["bold", "italic"] {
            if p.get(k).and_then(Value::as_bool) == Some(true) {
                style.set(k, "1");
            }
        }
        if let Some(a) = p.get("alignment").and_then(Value::as_str) {
            style.set("alignment", a);
        }
        style.merge_attrs(p.get("fcpx_style"));
        if p.contains_key("style") || p.contains_key("animation") {
            self.c.report.unmapped([format!(
                "ffcine.text style/animation on {:?} (not written)",
                clip.name
            )]);
        }
        n.children
            .push(Node::new("text-style-def").a("id", ts).child(style));
        n
    }

    fn marker(&mut self, m: &Marker, src_in: f64, k: f64) -> Node {
        let rate = self.c.rate;
        let s = m.marked_range.start_time.rescaled_to(rate).value;
        // Media time back to the clip's local time.
        let local = if (k - 1.0).abs() > 1e-9 && k.abs() > 1e-9 {
            src_in + (s - src_in) / k
        } else {
            s
        };
        let f = m.foreign(APP);
        let chapter = m.ffcine_value("chapter").and_then(Value::as_bool) == Some(true)
            || f.and_then(|f| f.get("element")).and_then(Value::as_str) == Some("chapter-marker")
            || m.extra.get("color").and_then(Value::as_str) == Some("ORANGE");
        let dur = m.marked_range.duration.rescaled_to(rate).value;
        let mut n = Node::new(if chapter { "chapter-marker" } else { "marker" })
            .a("start", self.c.t(local))
            .a("duration", self.c.t(if dur > 0.0 { dur } else { 1.0 }))
            .a("value", &m.name);
        if let Some(c) = m.extra.get("comment").and_then(Value::as_str)
            && !c.is_empty()
        {
            n.set("note", c);
        }
        if !chapter {
            match m.extra.get("color").and_then(Value::as_str) {
                Some("GREEN") => n.set("completed", "1"),
                Some("RED") => n.set("completed", "0"),
                _ => {}
            }
        }
        n.merge_attrs(f.and_then(|f| f.get("attrs")));
        if chapter && !n.has("posterOffset") {
            n.set("posterOffset", "0s");
        }
        n
    }

    /// `adjust-*` and filters for an item's effects. `ls` is the item's local start (frames),
    /// `start` its source start (seconds), for keyframe times.
    /// `cdl` is the clip's CDL: written where its `ffcine.color.cdl/1` is, else last.
    fn effects(
        &mut self,
        effects: &[Effect],
        ls: f64,
        start: f64,
        mut cdl: Option<super::Cdl>,
    ) -> (Vec<Node>, Vec<Node>) {
        let rate = self.c.rate;
        let mut adjust: Vec<(usize, Node)> = Vec::new();
        let mut filters = Vec::new();
        for e in effects {
            let Effect::Effect(g) = e else { continue };
            let Some((name, p)) = ffcine_effect(e) else {
                // An unmapped filter the reader kept.
                if let Some(data) = g.foreign(APP)
                    && let Some(mut n) = Node::from_json(data)
                {
                    if let Some(fx) = data.get("effect").and_then(|x| self.c.effect_json(x)) {
                        n.set("ref", fx);
                    }
                    filters.push(n);
                } else {
                    self.c
                        .report
                        .unmapped([format!("effect {:?} (no FCPXML equivalent)", g.effect_name)]);
                }
                continue;
            };
            if name == "ffcine.color.cdl" {
                if let Some(c) = cdl.take() {
                    let n = self.cdl_filter(&c, g.foreign(APP));
                    filters.push(n);
                }
                continue;
            }
            let a = Anims {
                p,
                start,
                rate,
                ls,
                c: self.c,
            };
            let node = match name {
                "ffcine.geo.transform" => Some((1, a.transform())),
                "ffcine.geo.crop" => Some((0, a.crop())),
                "ffcine.comp.blend" => Some((2, a.blend())),
                "ffcine.audio.level" => Some((3, a.volume())),
                "ffcine.color.cdl" | "ffcine.time.ramp" => None,
                other => {
                    self.c
                        .report
                        .unmapped([format!("{other} (no FCPXML equivalent)")]);
                    None
                }
            };
            if let Some((rank, mut n)) = node {
                let f = g.foreign(APP);
                n.merge_attrs(f.and_then(|f| f.get("attrs")));
                for c in f
                    .and_then(|f| f.get("children"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    n.children.extend(Node::from_json(c));
                }
                if !n.attrs.is_empty() || !n.children.is_empty() {
                    adjust.push((rank, n));
                }
            }
        }
        if let Some(c) = cdl {
            let n = self.cdl_filter(&c, None);
            filters.push(n);
        }
        adjust.sort_by_key(|(r, _)| *r);
        (adjust.into_iter().map(|(_, n)| n).collect(), filters)
    }
}

/// The clip's `LinearTimeWarp` / `FreezeFrame` speed, `None` at 1.
fn speed(clip: &Clip) -> Option<f64> {
    let k = clip.effects.iter().find_map(|e| match e {
        Effect::LinearTimeWarp(w) => Some(w.time_scalar),
        Effect::FreezeFrame(_) => Some(0.0),
        _ => None,
    })?;
    ((k - 1.0).abs() > 1e-9).then_some(k)
}

fn three(v: [f64; 3]) -> String {
    format!("{} {} {}", num(v[0]), num(v[1]), num(v[2]))
}

/// ffcine parameters -> FCPXML attributes / `param keyframeAnimation`s.
struct Anims<'a, 'c> {
    p: &'a Map<String, Value>,
    start: f64,
    rate: f64,
    ls: f64,
    c: &'a mut Ctx<'c>,
}

impl Anims<'_, '_> {
    fn get(&self, key: &str) -> Option<Anim> {
        Anim::read(self.p.get(key)?, self.start, self.rate)
    }

    /// Sets `name` on `n`: an attribute when constant, a `param` with keys otherwise.
    fn put(&mut self, n: &mut Node, name: &str, a: &Anim, f: &dyn Fn(&[f64]) -> String) {
        match a {
            Anim::Const(v) => n.set(name, f(v)),
            Anim::Keys(ks) => {
                let mut ka = Node::new("keyframeAnimation");
                for k in ks {
                    let mut kf = Node::new("keyframe")
                        .a("time", self.c.t(self.ls + k.frame))
                        .a("value", f(&k.v));
                    match k.interp.as_str() {
                        "ease" => kf.set("interp", "ease"),
                        "hold" => self
                            .c
                            .report
                            .warn("hold keyframes written as linear FCPXML keys (approx)"),
                        _ => {}
                    }
                    ka.children.push(kf);
                }
                n.children
                    .push(Node::new("param").a("name", name).child(ka));
            }
        }
    }

    fn transform(mut self) -> Node {
        let (w, h) = (self.c.w, self.c.h);
        if self.p.get("units").and_then(Value::as_str) == Some("picture") {
            self.c.report.warn(
                "picture-relative transform written to FCPXML as if the picture filled the frame",
            );
        }
        let pos_s = move |v: &[f64]| {
            format!(
                "{} {}",
                num(v[0] * w / h * 100.0),
                num(-v.get(1).copied().unwrap_or(0.0) * 100.0)
            )
        };
        let mut n = Node::new("adjust-transform");
        let pos = match (self.get("translate_x"), self.get("translate_y")) {
            (None, None) => self.get("position"),
            (x, y) => Some(Anim::zip(&[
                x.unwrap_or(Anim::Const(vec![0.0])),
                y.unwrap_or(Anim::Const(vec![0.0])),
            ])),
        };
        if let Some(a) = pos {
            self.put(&mut n, "position", &a, &pos_s);
        }
        let scale = match (self.get("scale_x"), self.get("scale_y")) {
            (None, None) => self
                .get("scale")
                .map(|a| a.map(|v| vec![v[0], *v.get(1).unwrap_or(&v[0])])),
            (x, y) => Some(Anim::zip(&[
                x.unwrap_or(Anim::Const(vec![1.0])),
                y.unwrap_or(Anim::Const(vec![1.0])),
            ])),
        };
        let flip = |k: &str| self.p.get(k).and_then(Value::as_bool) == Some(true);
        let (fx, fy) = (flip("flip_x"), flip("flip_y"));
        if let Some(a) = scale {
            let a = a.map(|v| vec![if fx { -v[0] } else { v[0] }, if fy { -v[1] } else { v[1] }]);
            self.put(&mut n, "scale", &a, &|v: &[f64]| {
                format!("{} {}", num(v[0]), num(v[1]))
            });
        } else if fx || fy {
            let s = format!(
                "{} {}",
                if fx { "-1" } else { "1" },
                if fy { "-1" } else { "1" }
            );
            n.set("scale", s);
        }
        let rot = self
            .get("rotate_deg")
            .or_else(|| self.get("rotation").map(|a| a.map(|v| vec![-v[0]])));
        if let Some(a) = rot {
            self.put(&mut n, "rotation", &a, &|v: &[f64]| num(v[0]));
        }
        if let Some(a) = self.get("anchor") {
            self.put(&mut n, "anchor", &a, &pos_s);
        }
        n
    }

    fn crop(mut self) -> Node {
        let (w, h) = (self.c.w, self.c.h);
        let fill = self.p.get("fill").and_then(Value::as_bool) == Some(true);
        let mut rect = Node::new(if fill { "crop-rect" } else { "trim-rect" });
        for side in ["left", "right", "top", "bottom"] {
            if let Some(a) = self.get(side) {
                let k = if matches!(side, "left" | "right") {
                    w / h * 100.0
                } else {
                    100.0
                };
                self.put(&mut rect, side, &a, &move |v: &[f64]| num(v[0] * k));
            }
        }
        if self.p.contains_key("feather") {
            self.c
                .report
                .unmapped(["ffcine.geo.crop/1.feather (no FCPXML crop softness)".to_owned()]);
        }
        Node::new("adjust-crop")
            .a("mode", if fill { "crop" } else { "trim" })
            .child(rect)
    }

    fn blend(mut self) -> Node {
        let mut n = Node::new("adjust-blend");
        if let Some(a) = self.get("opacity") {
            self.put(&mut n, "amount", &a, &|v: &[f64]| num(v[0]));
        }
        if let Some(m) = self.p.get("mode").and_then(Value::as_str) {
            n.set("mode", fcpx_mode(m));
        }
        n
    }

    fn volume(mut self) -> Node {
        let mut n = Node::new("adjust-volume");
        if let Some(a) = self.get("gain_db") {
            self.put(&mut n, "amount", &a, &|v: &[f64]| {
                format!("{}dB", num(v[0]))
            });
        }
        for k in ["fade_in", "fade_out", "mode", "duck_db", "nat_db", "lufs"] {
            if self.p.contains_key(k) {
                self.c
                    .report
                    .unmapped([format!("ffcine.audio.level/1.{k} (not written to FCPXML)")]);
            }
        }
        n
    }
}

/// An ffcine blend mode as FCP names it (`color_dodge` -> `Color Dodge`); `fcpx:N` -> `N`.
fn fcpx_mode(m: &str) -> String {
    if let Some(raw) = m.strip_prefix("fcpx:") {
        return raw.into();
    }
    let m = m.replace("colour", "color");
    let fixed = match m.as_str() {
        "hardlight" => "hard_light",
        "softlight" => "soft_light",
        "lighter_color" | "lightercolor" => "lighter_color",
        "darker_color" | "darkercolor" => "darker_color",
        other => other,
    };
    fixed
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Connects `n` to the primary item under timeline frame `at` (`local0` = the spine's start).
fn attach(spine: &mut Node, mut n: Node, at: f64, local0: f64, rate: f64, fd: (u64, u64)) {
    let span = |c: &Node| {
        let off = time_of(c, "offset", rate) - local0;
        (off, off + time_of(c, "duration", rate))
    };
    let mut pick = None;
    for (i, c) in spine.children.iter().enumerate() {
        if c.name == "transition" {
            continue;
        }
        let (s, e) = span(c);
        if pick.is_none() || (s <= at + 1e-6 && at < e - 1e-6) {
            pick = Some(i);
        }
        if s <= at + 1e-6 && at < e - 1e-6 {
            break;
        }
    }
    let Some(i) = pick else { return };
    let parent = &mut spine.children[i];
    let (s, _) = span(parent);
    n.set("offset", fcp_time(local_start(parent, rate) + at - s, fd));
    insert_anchored(parent, n);
}

/// Puts a connected storyline among `parent`'s children, before its markers and filters.
fn insert_anchored(parent: &mut Node, s: Node) {
    let at = parent
        .children
        .iter()
        .position(|c| {
            matches!(
                c.name.as_str(),
                "marker"
                    | "chapter-marker"
                    | "rating"
                    | "keyword"
                    | "filter-video"
                    | "filter-audio"
                    | "metadata"
            )
        })
        .unwrap_or(parent.children.len());
    parent.children.insert(at, s);
}
