//! FCPXML document -> ffcine timeline.

#![allow(clippy::many_single_char_names)] // maths on x, y, w, h, t, v

use serde_json::{Map, Value, json};

use super::{Asset, Doc, invalid, parse_floats, parse_time};
use crate::common::{
    Event, Group, Interp, Kind, Lanes, blend_mode_name, ffcine_effect, foreign_effect, keyframe,
    range, rebase_keyframes, rt, set_transition_kind, time_ramp, transform_params,
};
use crate::xml::{Element, Ids, Report};
use crate::{ImportOptions, Imported};
use ffcine_core::Result;
use ffcine_timeline::{
    Clip, Effect, ExternalReference, FFCINE_VERSION, Gap, GeneratorReference, HasMetadata, Item,
    LinearTimeWarp, Marker, MediaReference, MissingReference, Stack, Timeline, Track, Transition,
};

const APP: &str = "fcpx";

/// Attributes every story element's mapping consumes.
const ITEM_ATTRS: &[&str] = &[
    "name", "offset", "duration", "start", "ref", "lane", "enabled",
];

/// Elements that sit in a storyline.
const STORY: &[&str] = &[
    "asset-clip",
    "clip",
    "sync-clip",
    "ref-clip",
    "mc-clip",
    "video",
    "audio",
    "gap",
    "title",
    "audition",
    "caption",
    "live-drawing",
];

/// Cross Dissolve's effect uid.
const CROSS_DISSOLVE_UID: &str = "FxPlug:4731E73A-8DAC-4113-9A30-AE85B1761265";

pub(super) fn map(doc: &Doc, opts: &ImportOptions) -> Result<Imported> {
    let project = doc
        .projects
        .first()
        .ok_or_else(|| invalid("no <project> with a <sequence>"))?;
    let mut report = Report::default();
    if doc.projects.len() > 1 {
        let rest: Vec<&str> = doc.projects[1..].iter().map(|p| p.name.as_str()).collect();
        report.warn(format!(
            "{} projects in the file: imported {:?}, skipped {rest:?}",
            doc.projects.len(),
            project.name
        ));
    }
    let seq = &project.sequence;
    let fmt = seq.attr("format").and_then(|f| doc.formats.get(f));
    let rate = match fmt.and_then(|f| f.frame_duration) {
        Some(fd) if fd > 0.0 => {
            let file = 1.0 / fd;
            if let Some(want) = opts.rate.filter(|r| (r - file).abs() > 1e-6) {
                report.warn(format!(
                    "rate {want} ignored: the sequence is {file} fps (retime it after import)"
                ));
            }
            file
        }
        _ => opts
            .rate
            .ok_or_else(|| invalid("the sequence format has no frameDuration; pass a rate"))?,
    };
    let (w, h) = match (fmt.and_then(|f| f.width), fmt.and_then(|f| f.height)) {
        (Some(w), Some(h)) => (w, h),
        _ => opts.frame_size.unwrap_or_else(|| {
            report.warn("no frame size in the sequence format or options: assumed 1920x1080");
            (1920, 1080)
        }),
    };
    let mut m = Mapper {
        doc,
        rate,
        w: f64::from(w),
        h: f64::from(h),
        report,
        ids: Ids::default(),
        scope: String::new(),
        depth: 0,
    };
    let tc_start = seq.attr("tcStart").and_then(parse_time).unwrap_or(0.0);
    let tracks = m.sequence(seq, tc_start);

    let mut tl = Timeline::new(project.name.clone());
    tl.global_start_time = Some(rt(tc_start * rate, rate));
    tl.tracks.children = tracks.into_iter().map(Item::Track).collect();
    let ns = tl.ffcine_mut();
    ns.insert("v".into(), FFCINE_VERSION.into());
    ns.insert("frame_size".into(), json!([w, h]));
    let mut foreign = Map::new();
    foreign.insert("version".into(), doc.version.clone().into());
    if let Some(a) = project.el.attrs_except(&["name"]) {
        foreign.insert("project".into(), a);
    }
    if let Some(a) = seq.attrs_except(&["tcStart"]) {
        foreign.insert("sequence".into(), a);
    }
    if let Some(f) = fmt {
        foreign.insert("format".into(), f.el.to_json());
    }
    if let Some(e) = &project.event {
        foreign.insert("event".into(), e.clone().into());
    }
    if let Some(l) = &doc.library_location {
        foreign.insert("library".into(), l.clone().into());
    }
    let seq_extra: Vec<Value> = seq
        .children
        .iter()
        .filter(|c| c.name != "spine")
        .map(Element::to_json)
        .collect();
    if !seq_extra.is_empty() {
        foreign.insert("children".into(), seq_extra.into());
    }
    tl.set_foreign(APP, Value::Object(foreign));
    rebase_keyframes(&mut tl.tracks.children);
    Ok(Imported {
        timeline: tl,
        report: m.report.finish(),
    })
}

/// A story element turned into an OTIO item.
struct Mapped {
    kind: Kind,
    item: Item,
    /// The element's local time at its first frame.
    local_start: f64,
}

/// Effects, markers and leftovers gathered from an element's children.
#[derive(Default)]
struct Deco {
    effects: Vec<Effect>,
    markers: Vec<Marker>,
    foreign: Vec<Value>,
    cdl: Option<Value>,
}

/// `timeMap`: clip local time -> source time, piecewise.
struct TimeMap {
    pts: Vec<(f64, f64, Interp)>,
}

impl TimeMap {
    fn eval(&self, t: f64) -> f64 {
        let p = &self.pts;
        match p.len() {
            0 => t,
            1 => p[0].1 + (t - p[0].0),
            n => {
                let i = p.windows(2).position(|w| t < w[1].0).unwrap_or(n - 2);
                let (a, b) = (p[i], p[i + 1]);
                if (b.0 - a.0).abs() < 1e-12 {
                    return a.1;
                }
                a.1 + (t - a.0) * (b.1 - a.1) / (b.0 - a.0)
            }
        }
    }

    /// Whether every segment covering `[a, b]` has the same slope.
    fn is_constant(&self, a: f64, b: f64) -> bool {
        let slopes: Vec<f64> = self
            .pts
            .windows(2)
            .filter(|w| w[1].0 > a && w[0].0 < b && (w[1].0 - w[0].0).abs() > 1e-12)
            .map(|w| (w[1].1 - w[0].1) / (w[1].0 - w[0].0))
            .collect();
        slopes.windows(2).all(|s| (s[0] - s[1]).abs() < 1e-6)
    }
}

struct Mapper<'a> {
    doc: &'a Doc,
    rate: f64,
    w: f64,
    h: f64,
    report: Report,
    ids: Ids,
    /// Id scope: the compound clips we are inside.
    scope: String,
    depth: usize,
}

fn time_attr(el: &Element, name: &str) -> Option<f64> {
    el.attr(name).and_then(parse_time)
}

fn lane_of(el: &Element) -> Option<i32> {
    el.attr("lane").and_then(|l| l.parse().ok())
}

impl Mapper<'_> {
    fn fr(&self, secs: f64) -> f64 {
        secs * self.rate
    }

    /// Tracks of a `sequence` whose spine starts at local time `local0`.
    fn sequence(&mut self, seq: &Element, local0: f64) -> Vec<Track> {
        let mut lanes = Lanes::default();
        if let Some(spine) = seq.child("spine") {
            self.spine(spine, 0.0, local0, 0, &mut lanes);
        }
        lanes.into_tracks(self.rate)
    }

    /// Lays out a storyline whose local time `local0` sits at timeline second `origin`.
    fn spine(&mut self, spine: &Element, origin: f64, local0: f64, lane: i32, lanes: &mut Lanes) {
        let story: Vec<&Element> = spine
            .children
            .iter()
            .filter(|c| c.name == "transition" || STORY.contains(&c.name.as_str()))
            .collect();
        for c in &spine.children {
            if c.name != "transition" && !STORY.contains(&c.name.as_str()) {
                self.report.unmapped(format!("spine child <{}>", c.name));
                self.report
                    .warn(format!("FCPXML <{}> in a storyline was dropped", c.name));
            }
        }
        // Timeline start/end (seconds) of each non-transition item.
        let mut spans: Vec<Option<(f64, f64)>> = Vec::with_capacity(story.len());
        let mut cursor = local0;
        for c in &story {
            if c.name == "transition" {
                spans.push(None);
                continue;
            }
            let off = time_attr(c, "offset").unwrap_or(cursor);
            let dur = time_attr(c, "duration").unwrap_or(0.0);
            cursor = off + dur;
            let s = origin + (off - local0);
            spans.push(Some((s, s + dur)));
        }

        let mut group = Group::default();
        let mut kinds = Vec::new();
        let mut connected = Vec::new();
        for (i, c) in story.iter().enumerate() {
            if c.name == "transition" {
                let off = time_attr(c, "offset").unwrap_or(0.0);
                let dur = time_attr(c, "duration").unwrap_or(0.0);
                let start = origin + (off - local0);
                let cut = spans[i + 1..]
                    .iter()
                    .flatten()
                    .next()
                    .map(|s| s.0)
                    .or_else(|| spans[..i].iter().flatten().last().map(|s| s.1))
                    .unwrap_or(start);
                let in_off = (cut - start).max(0.0);
                let out_off = (start + dur - cut).max(0.0);
                let transition = self.transition(c, in_off, out_off);
                group.events.push(Event::Transition {
                    cut: self.fr(cut),
                    transition,
                });
                continue;
            }
            let Some((s, e)) = spans[i] else { continue };
            let m = self.item(c, s, lane);
            kinds.push(m.kind);
            group.events.push(Event::Item {
                start: self.fr(s),
                dur: self.fr(e - s),
                item: m.item,
            });
            connected.push((*c, s, m.local_start));
        }
        let kind = if !kinds.is_empty() && kinds.iter().all(|k| *k == Kind::Audio) {
            Kind::Audio
        } else {
            Kind::Video
        };
        lanes.add(kind, lane, group);
        for (c, s, ls) in connected {
            self.connected(c, s, ls, lane, lanes);
        }
    }

    /// Connected clips and storylines anchored to `parent`, which starts at timeline second
    /// `parent_tl` and local time `parent_ls`.
    fn connected(
        &mut self,
        parent: &Element,
        parent_tl: f64,
        parent_ls: f64,
        base_lane: i32,
        lanes: &mut Lanes,
    ) {
        for d in &parent.children {
            let Some(l) = lane_of(d) else { continue };
            let lane = base_lane + l;
            let d_tl = parent_tl + (time_attr(d, "offset").unwrap_or(parent_ls) - parent_ls);
            if d.name == "spine" {
                self.spine(d, d_tl, 0.0, lane, lanes);
                continue;
            }
            if !STORY.contains(&d.name.as_str()) || d.name == "gap" {
                continue;
            }
            let dur = time_attr(d, "duration").unwrap_or(0.0);
            let m = self.item(d, d_tl, lane);
            lanes.add(
                m.kind,
                lane,
                Group::single(self.fr(d_tl), self.fr(dur), m.item),
            );
            self.connected(d, d_tl, m.local_start, lane, lanes);
        }
    }

    fn id(&mut self, el: &Element, tl: f64, lane: i32) -> String {
        let key = format!(
            "{}|{}|{}|{}|{lane}|{tl:.6}",
            self.scope,
            el.name,
            el.attr("ref").unwrap_or_default(),
            el.attr("name").unwrap_or_default()
        );
        self.ids.make(APP, &key)
    }

    /// Local time of an element's first frame: `start`, else its asset's or compound's start.
    fn local_start(&self, el: &Element) -> f64 {
        if let Some(s) = time_attr(el, "start") {
            return s;
        }
        let r = el.attr("ref").unwrap_or_default();
        if let Some(a) = self.doc.assets.get(r) {
            return a.start;
        }
        self.doc
            .media
            .get(r)
            .and_then(|m| m.child("sequence"))
            .and_then(|s| time_attr(s, "tcStart"))
            .unwrap_or(0.0)
    }

    fn item(&mut self, el: &Element, tl: f64, lane: i32) -> Mapped {
        match el.name.as_str() {
            "asset-clip" | "video" | "audio" => self.media_clip(el, tl, lane),
            "clip" | "sync-clip" => self.clip(el, tl, lane),
            "ref-clip" => self.ref_clip(el, tl, lane),
            "title" => self.title(el, tl, lane),
            "gap" => self.gap(el),
            "audition" => self.audition(el, tl, lane),
            _ => self.placeholder(el, tl, lane),
        }
    }

    fn time_map(el: &Element) -> Option<TimeMap> {
        let tm = el.child("timeMap")?;
        let pts = tm
            .children_named("timept")
            .filter_map(|p| {
                let t = time_attr(p, "time")?;
                let v = time_attr(p, "value")?;
                let interp = match p.attr("interp") {
                    Some("linear") => Interp::Linear,
                    _ => Interp::Ease,
                };
                Some((t, v, interp))
            })
            .collect();
        Some(TimeMap { pts })
    }

    /// `LinearTimeWarp` / `ffcine.time.ramp/1` from `timeMap` or `conform-rate`.
    fn time_effects(&mut self, el: &Element, ls: f64, dur: f64) -> Vec<Effect> {
        if let Some(tm) = Self::time_map(el) {
            let end = ls + dur;
            let avg = if dur > 0.0 {
                (tm.eval(end) - tm.eval(ls)) / dur
            } else {
                1.0
            };
            let mut out = Vec::new();
            if !tm.is_constant(ls, end) {
                self.report.warn(
                    "FCPXML timeMap speed ramp: kept as ffcine.time.ramp/1 with constant speed \
                     between points (smooth segments approx.)",
                );
                let mut map = vec![(0.0, self.fr(tm.eval(ls)))];
                map.extend(
                    tm.pts
                        .iter()
                        .filter(|(t, _, _)| *t > ls && *t < end)
                        .map(|(t, v, _)| (self.fr(t - ls), self.fr(*v))),
                );
                map.push((self.fr(dur), self.fr(tm.eval(end))));
                if let Some((e, _)) = time_ramp(&map, self.rate, &mut self.report) {
                    out.push(e);
                }
            }
            if (avg - 1.0).abs() > 1e-9 || !out.is_empty() {
                out.push(Effect::LinearTimeWarp(LinearTimeWarp::new(avg)));
            }
            return out;
        }
        if let Some(cr) = el.child("conform-rate") {
            let src = cr
                .attr("srcFrameRate")
                .and_then(|r| r.parse::<f64>().ok())
                .map(ntsc);
            if cr.attr("scaleEnabled") != Some("0")
                && let Some(src) = src.filter(|s| *s > 0.0)
            {
                let k = self.rate / src;
                if (k - 1.0).abs() > 1e-3 {
                    self.report
                        .warn("FCPXML conform-rate mapped to a LinearTimeWarp (unverified)");
                    let mut w = LinearTimeWarp::new(k);
                    if let Some(a) = cr.attrs_except(&["srcFrameRate", "scaleEnabled"]) {
                        w.set_foreign(APP, a);
                    }
                    return vec![Effect::LinearTimeWarp(w)];
                }
            }
        }
        Vec::new()
    }

    fn external_ref(&mut self, r: &str) -> MediaReference {
        let Some(a) = self.doc.assets.get(r) else {
            self.report.warn(format!(
                "FCPXML reference {r:?} is not an asset: media missing"
            ));
            return MediaReference::Missing(MissingReference {
                name: r.to_owned(),
                ..MissingReference::default()
            });
        };
        let a: &Asset = a;
        let avail = a
            .duration
            .map(|d| range(self.fr(a.start), self.fr(d), self.rate));
        let Some(src) = &a.src else {
            let mut m = MissingReference::default();
            m.name.clone_from(&a.name);
            m.available_range = avail;
            m.set_foreign(APP, a.el.to_json());
            return MediaReference::Missing(m);
        };
        let mut e = ExternalReference::new(src.clone(), avail);
        e.name.clone_from(&a.name);
        let mut source = Map::new();
        if let Some(uid) = a.el.attr("uid") {
            source.insert("uid".into(), uid.into());
        }
        for md in
            a.el.child("metadata")
                .into_iter()
                .flat_map(|m| m.children_named("md"))
        {
            if md.attr("key") == Some("com.apple.proapps.studio.reel")
                && let Some(v) = md.attr("value")
            {
                source.insert("reel".into(), v.into());
            }
        }
        if !source.is_empty() {
            e.ffcine_mut().insert("source".into(), source.into());
        }
        let mut f = Map::new();
        if let Some(at) =
            a.el.attrs_except(&["id", "name", "src", "start", "duration"])
        {
            f.insert("attrs".into(), at);
        }
        if !a.el.children.is_empty() {
            let c: Vec<Value> = a.el.children.iter().map(Element::to_json).collect();
            f.insert("children".into(), c.into());
        }
        if !f.is_empty() {
            e.set_foreign(APP, f.into());
        }
        MediaReference::External(e)
    }

    fn media_kind(&self, el: &Element) -> Kind {
        match el.name.as_str() {
            "audio" => return Kind::Audio,
            "video" => return Kind::Video,
            _ => {}
        }
        match el.attr("srcEnable") {
            Some("audio") => return Kind::Audio,
            Some("video") => return Kind::Video,
            _ => {}
        }
        match self.doc.assets.get(el.attr("ref").unwrap_or_default()) {
            Some(a) if a.has_audio && !a.has_video => Kind::Audio,
            _ => Kind::Video,
        }
    }

    fn media_clip(&mut self, el: &Element, tl: f64, lane: i32) -> Mapped {
        let ls = self.local_start(el);
        let dur = time_attr(el, "duration").unwrap_or(0.0);
        let tm = Self::time_map(el);
        let src_in = tm.as_ref().map_or(ls, |t| t.eval(ls));
        let media = self.external_ref(el.attr("ref").unwrap_or_default());
        let name = el
            .attr("name")
            .map(str::to_owned)
            .or_else(|| {
                self.doc
                    .assets
                    .get(el.attr("ref").unwrap_or_default())
                    .map(|a| a.name.clone())
            })
            .unwrap_or_default();
        let mut clip = Clip::new(
            name,
            media,
            Some(range(self.fr(src_in), self.fr(dur), self.rate)),
        );
        let mut d = Deco::default();
        let to_src = |t: f64| tm.as_ref().map_or(t, |m| m.eval(t));
        self.decorate(el, ls, &to_src, &[], &mut d);
        d.effects.extend(self.time_effects(el, ls, dur));
        let id = self.id(el, tl, lane);
        finish_clip(&mut clip, el, d, &id);
        Mapped {
            kind: self.media_kind(el),
            item: Item::Clip(clip),
            local_start: ls,
        }
    }

    /// The media component of a `clip`: its primary `video`/`audio`/`asset-clip`, possibly
    /// inside a primary `gap` (the detached-audio pattern) or a nested `clip`.
    fn component<'e>(el: &'e Element, chain: &mut Vec<&'e Element>) -> bool {
        let Some(p) = el.children.iter().find(|c| {
            lane_of(c).is_none() && STORY.contains(&c.name.as_str()) || c.name == "spine"
        }) else {
            return false;
        };
        match p.name.as_str() {
            "video" | "audio" | "asset-clip" => {
                chain.push(p);
                true
            }
            "gap" => {
                let Some(m) = p
                    .children
                    .iter()
                    .find(|c| lane_of(c).is_some() && matches!(c.name.as_str(), "video" | "audio"))
                else {
                    return false;
                };
                chain.push(p);
                chain.push(m);
                true
            }
            "clip" | "sync-clip" => {
                chain.push(p);
                Self::component(p, chain)
            }
            _ => false,
        }
    }

    fn clip(&mut self, el: &Element, tl: f64, lane: i32) -> Mapped {
        let ls = time_attr(el, "start").unwrap_or(0.0);
        let dur = time_attr(el, "duration").unwrap_or(0.0);
        let mut chain = Vec::new();
        if !Self::component(el, &mut chain) {
            if let Some(spine) = el.child("spine") {
                return self.storyline_clip(el, spine, tl, lane);
            }
            return self.placeholder(el, tl, lane);
        }
        let tm = Self::time_map(el);
        let starts: Vec<(f64, f64)> = chain
            .iter()
            .map(|c| (time_attr(c, "offset").unwrap_or(0.0), self.local_start(c)))
            .collect();
        let to_src = |t: f64| {
            let mut t = tm.as_ref().map_or(t, |m| m.eval(t));
            for (off, st) in &starts {
                t = st + (t - off);
            }
            t
        };
        let media_el = chain[chain.len() - 1];
        let media = self.external_ref(media_el.attr("ref").unwrap_or_default());
        let mut clip = Clip::new(
            el.attr("name").unwrap_or_default(),
            media,
            Some(range(self.fr(to_src(ls)), self.fr(dur), self.rate)),
        );
        let mut d = Deco::default();
        self.decorate(el, ls, &to_src, &[], &mut d);
        // Adjustments on the component itself (in its own local time).
        let comp_ls = self.local_start(media_el);
        self.decorate(media_el, comp_ls, &|t| t, &[], &mut d);
        d.effects.extend(self.time_effects(el, ls, dur));
        let id = self.id(el, tl, lane);
        finish_clip(&mut clip, el, d, &id);
        let kind = if chain.iter().any(|c| c.name == "video")
            || (media_el.name == "asset-clip" && self.media_kind(media_el) == Kind::Video)
        {
            Kind::Video
        } else {
            Kind::Audio
        };
        Mapped {
            kind,
            item: Item::Clip(clip),
            local_start: ls,
        }
    }

    /// A `clip` holding a storyline: a nested `Stack`.
    fn storyline_clip(&mut self, el: &Element, spine: &Element, tl: f64, lane: i32) -> Mapped {
        let ls = time_attr(el, "start").unwrap_or(0.0);
        let dur = time_attr(el, "duration").unwrap_or(0.0);
        let id = self.id(el, tl, lane);
        let saved = std::mem::replace(&mut self.scope, id.clone());
        let mut lanes = Lanes::default();
        let off = time_attr(spine, "offset").unwrap_or(0.0);
        self.spine(spine, off, 0.0, 0, &mut lanes);
        self.scope = saved;
        let mut stack = Stack {
            name: el.attr("name").unwrap_or_default().to_owned(),
            source_range: Some(range(self.fr(ls), self.fr(dur), self.rate)),
            children: lanes
                .into_tracks(self.rate)
                .into_iter()
                .map(Item::Track)
                .collect(),
            ..Stack::default()
        };
        let mut d = Deco::default();
        self.decorate(el, ls, &|t| t, &[], &mut d);
        finish_stack(&mut stack, el, d, &id);
        Mapped {
            kind: Kind::Video,
            item: Item::Stack(stack),
            local_start: ls,
        }
    }

    fn ref_clip(&mut self, el: &Element, tl: f64, lane: i32) -> Mapped {
        let r = el.attr("ref").unwrap_or_default();
        let Some(seq) = self.doc.media.get(r).and_then(|m| m.child("sequence")) else {
            if self
                .doc
                .media
                .get(r)
                .is_some_and(|m| m.child("multicam").is_some())
            {
                self.report.unmapped("multicam");
            }
            return self.placeholder(el, tl, lane);
        };
        if self.depth > 16 {
            self.report
                .warn("FCPXML compound clips nested too deep (cycle?): left as missing media");
            return self.placeholder(el, tl, lane);
        }
        let ls = self.local_start(el);
        let dur = time_attr(el, "duration").unwrap_or(0.0);
        let tc = time_attr(seq, "tcStart").unwrap_or(0.0);
        if let Some(fd) = self.doc.frame_duration(seq.attr("format"))
            && (1.0 / fd - self.rate).abs() > 1e-3
        {
            self.report.warn(format!(
                "FCPXML compound clip {r:?} has another frame rate: laid out at the project rate"
            ));
        }
        let tm = Self::time_map(el);
        let to_src = |t: f64| tm.as_ref().map_or(t, |m| m.eval(t)) - tc;
        let id = self.id(el, tl, lane);
        let saved = std::mem::replace(&mut self.scope, id.clone());
        self.depth += 1;
        let tracks = self.sequence(seq, tc);
        self.depth -= 1;
        self.scope = saved;
        let mut stack = Stack {
            name: el.attr("name").unwrap_or_default().to_owned(),
            source_range: Some(range(self.fr(to_src(ls)), self.fr(dur), self.rate)),
            children: tracks.into_iter().map(Item::Track).collect(),
            ..Stack::default()
        };
        let mut d = Deco::default();
        self.decorate(el, ls, &to_src, &[], &mut d);
        d.effects.extend(self.time_effects(el, ls, dur));
        finish_stack(&mut stack, el, d, &id);
        Mapped {
            kind: if el.attr("srcEnable") == Some("audio") {
                Kind::Audio
            } else {
                Kind::Video
            },
            item: Item::Stack(stack),
            local_start: ls,
        }
    }

    fn gap(&mut self, el: &Element) -> Mapped {
        let ls = time_attr(el, "start").unwrap_or(0.0);
        let dur = time_attr(el, "duration").unwrap_or(0.0);
        let mut gap = Gap::new(rt(self.fr(dur), self.rate));
        el.attr("name")
            .unwrap_or_default()
            .clone_into(&mut gap.name);
        let mut d = Deco::default();
        self.decorate(el, ls, &|t| t - ls, &[], &mut d);
        gap.effects = d.effects;
        gap.markers = d.markers;
        set_foreign(&mut gap, el, d.foreign);
        Mapped {
            kind: Kind::Video,
            item: Item::Gap(gap),
            local_start: ls,
        }
    }

    fn audition(&mut self, el: &Element, tl: f64, lane: i32) -> Mapped {
        let mut picks = el
            .children
            .iter()
            .filter(|c| STORY.contains(&c.name.as_str()));
        let Some(first) = picks.next() else {
            return self.placeholder(el, tl, lane);
        };
        let alts: Vec<Value> = picks.map(Element::to_json).collect();
        let mut m = self.item(first, tl, lane);
        if !alts.is_empty() {
            self.report
                .warn("FCPXML audition: only the active pick is on the timeline");
            m.item.insert_foreign(APP, "audition", alts.into());
        }
        m
    }

    /// Anything we can't place as media: a missing-media clip carrying the element.
    fn placeholder(&mut self, el: &Element, tl: f64, lane: i32) -> Mapped {
        let ls = self.local_start(el);
        let dur = time_attr(el, "duration").unwrap_or(0.0);
        self.report.unmapped(el.name.clone());
        self.report.warn(format!(
            "FCPXML <{}> {:?} not mapped: left as a clip with missing media",
            el.name,
            el.attr("name").unwrap_or_default()
        ));
        let m = MissingReference {
            name: el.attr("name").unwrap_or_default().to_owned(),
            ..MissingReference::default()
        };
        let mut clip = Clip::new(
            el.attr("name").unwrap_or_default(),
            MediaReference::Missing(m),
            Some(range(self.fr(ls), self.fr(dur), self.rate)),
        );
        let id = self.id(el, tl, lane);
        clip.set_ffcine_id(id);
        clip.set_foreign(APP, el.to_json());
        Mapped {
            kind: self.media_kind(el),
            item: Item::Clip(clip),
            local_start: ls,
        }
    }

    fn title(&mut self, el: &Element, tl: f64, lane: i32) -> Mapped {
        let ls = time_attr(el, "start").unwrap_or(0.0);
        let dur = time_attr(el, "duration").unwrap_or(0.0);
        let mut text = String::new();
        let mut style_ref = None;
        for t in el.children_named("text") {
            if t.children.is_empty() {
                text.push_str(&t.text);
            }
            for s in t.children_named("text-style") {
                text.push_str(&s.text);
                if style_ref.is_none() {
                    style_ref = s.attr("ref");
                }
            }
        }
        let mut p = Map::new();
        p.insert("text".into(), text.into());
        if let Some(st) = style_ref.and_then(|r| self.doc.text_styles.get(r)) {
            if let Some(f) = st.attr("font") {
                p.insert("font".into(), f.into());
            }
            if let Some(f) = st.attr("fontFace") {
                p.insert("face".into(), f.into());
            }
            if let Some(s) = st.attr("fontSize").and_then(|s| s.parse::<f64>().ok()) {
                p.insert("size".into(), s.into());
            }
            let c = st.attr("fontColor").map(parse_floats).unwrap_or_default();
            if c.len() >= 3 {
                p.insert(
                    "colour".into(),
                    json!([c[0], c[1], c[2], c.get(3).copied().unwrap_or(1.0)]),
                );
            }
            if let Some(a) = st.attr("alignment") {
                p.insert("alignment".into(), a.into());
            }
            p.insert("bold".into(), (st.attr("bold") == Some("1")).into());
            p.insert("italic".into(), (st.attr("italic") == Some("1")).into());
            if let Some(rest) = st.attrs_except(&[
                "font",
                "fontFace",
                "fontSize",
                "fontColor",
                "alignment",
                "bold",
                "italic",
            ]) {
                p.insert("fcpx_style".into(), rest);
            }
        }
        let mut other_params = Vec::new();
        for prm in el.children_named("param") {
            if prm.attr("name") == Some("Position")
                && let Some(v) = prm.attr("value")
                && let [x, y, ..] = parse_floats(v)[..]
            {
                p.insert("position".into(), json!([x / self.w, -y / self.h]));
            } else {
                other_params.push(prm.to_json());
            }
        }
        self.report.warn(
            "FCPXML title: text, font, colour and position kept; Motion template and animation \
             are not rendered (position in pixels is unverified)",
        );
        let mut g = GeneratorReference::new("ffcine.text", p);
        el.attr("name").unwrap_or_default().clone_into(&mut g.name);
        g.available_range = Some(range(self.fr(ls), self.fr(dur), self.rate));
        let mut gf = Map::new();
        if let Some(e) = self.doc.effects.get(el.attr("ref").unwrap_or_default()) {
            gf.insert("effect".into(), e.to_json());
        }
        if !other_params.is_empty() {
            gf.insert("params".into(), other_params.into());
        }
        if !gf.is_empty() {
            g.set_foreign(APP, gf.into());
        }
        let mut clip = Clip::new(
            el.attr("name").unwrap_or_default(),
            MediaReference::Generator(g),
            Some(range(self.fr(ls), self.fr(dur), self.rate)),
        );
        let mut d = Deco::default();
        self.decorate(el, ls, &|t| t, &["param", "text", "text-style-def"], &mut d);
        let id = self.id(el, tl, lane);
        finish_clip(&mut clip, el, d, &id);
        Mapped {
            kind: Kind::Video,
            item: Item::Clip(clip),
            local_start: ls,
        }
    }

    fn transition(&mut self, el: &Element, in_off: f64, out_off: f64) -> Transition {
        let mut t = Transition::dissolve(
            rt(self.fr(in_off), self.rate),
            rt(self.fr(out_off), self.rate),
        );
        el.attr("name").unwrap_or_default().clone_into(&mut t.name);
        let video = el.child("filter-video");
        let res = video.and_then(|v| self.doc.effects.get(v.attr("ref").unwrap_or_default()));
        let dissolve = match (video, res) {
            (None, _) => true,
            (Some(_), Some(r)) => {
                r.attr("uid") == Some(CROSS_DISSOLVE_UID)
                    || r.attr("name") == Some("Cross Dissolve")
            }
            (Some(v), None) => v.attr("name") == Some("Cross Dissolve"),
        };
        if !dissolve {
            let kind = video
                .and_then(|v| v.attr("name"))
                .or_else(|| res.and_then(|r| r.attr("name")))
                .unwrap_or(&t.name)
                .to_owned();
            self.report.warn(format!(
                "FCPXML transition {kind:?} rendered as a dissolve (kind kept)"
            ));
            set_transition_kind(&mut t, json!({ "kind": kind }));
        }
        let mut f = Map::new();
        if let Some(a) = el.attrs_except(&["name", "offset", "duration"]) {
            f.insert("attrs".into(), a);
        }
        if !el.children.is_empty() {
            let c: Vec<Value> = el.children.iter().map(Element::to_json).collect();
            f.insert("children".into(), c.into());
        }
        let effects: Vec<Value> = el
            .children
            .iter()
            .filter_map(|c| self.doc.effects.get(c.attr("ref").unwrap_or_default()))
            .map(Element::to_json)
            .collect();
        if !effects.is_empty() {
            f.insert("effects".into(), effects.into());
        }
        if !f.is_empty() {
            t.set_foreign(APP, f.into());
        }
        t
    }

    /// Effects, markers and leftovers from `el`'s children. `ls` is `el`'s local start (keyframe
    /// times are local), `to_src` maps local time to the item's source time (for markers).
    fn decorate(
        &mut self,
        el: &Element,
        ls: f64,
        to_src: &dyn Fn(f64) -> f64,
        handled: &[&str],
        d: &mut Deco,
    ) {
        for c in &el.children {
            let n = c.name.as_str();
            if handled.contains(&n)
                || STORY.contains(&n)
                || matches!(n, "spine" | "transition" | "timeMap" | "conform-rate")
            {
                continue;
            }
            match n {
                "adjust-transform" => {
                    let e = self.transform(c, ls);
                    d.effects.push(e);
                }
                "adjust-crop" => match self.crop(c, ls) {
                    Some(e) => d.effects.push(e),
                    None => d.foreign.push(c.to_json()),
                },
                "adjust-volume" => {
                    let e = self.volume(c, ls);
                    d.effects.push(e);
                }
                "adjust-blend" => {
                    let e = self.blend(c, ls);
                    d.effects.push(e);
                }
                "filter-video" => self.filter_video(c, ls, d),
                "filter-audio" => {
                    let name = c.attr("name").unwrap_or_default();
                    self.report.unmapped(format!("filter-audio {name:?}"));
                    let e = self.foreign_filter(c);
                    d.effects.push(e);
                }
                "marker" | "chapter-marker" => {
                    let m = self.marker(c, to_src);
                    d.markers.push(m);
                }
                _ => {
                    self.report.unmapped(n.to_owned());
                    d.foreign.push(c.to_json());
                }
            }
        }
    }

    /// A param animated by `keyframeAnimation`, a static `param value`, or the attribute
    /// `name`; `Null` if none.
    fn anim(
        &mut self,
        el: &Element,
        name: &str,
        ls: f64,
        conv: &dyn Fn(&str) -> Option<Value>,
    ) -> Value {
        if let Some(p) = find_param(el, name) {
            if let Some(ka) = p.child("keyframeAnimation") {
                let keys: Vec<Value> = ka
                    .children_named("keyframe")
                    .filter_map(|k| {
                        let t = time_attr(k, "time")?;
                        let v = conv(k.attr("value")?)?;
                        let interp = match k.attr("interp") {
                            None | Some("linear") => Interp::Linear,
                            _ => Interp::Ease,
                        };
                        Some(keyframe(self.fr(t - ls), self.rate, v, interp))
                    })
                    .collect();
                if keys.iter().any(|_| true) {
                    if ka
                        .children_named("keyframe")
                        .any(|k| k.attr("interp").is_some_and(|i| i != "linear"))
                    {
                        self.report
                            .warn("FCPXML keyframe easings (easeIn/easeOut) approximated as ease");
                    }
                    return Value::Array(keys);
                }
            }
            if let Some(v) = p.attr("value").and_then(conv) {
                return v;
            }
        }
        el.attr(name).and_then(conv).unwrap_or(Value::Null)
    }

    fn transform(&mut self, c: &Element, ls: f64) -> Effect {
        let (w, h) = (self.w, self.h);
        let pos = move |s: &str| match parse_floats(s)[..] {
            [x, y, ..] => Some(json!([x * h / 100.0 / w, -y / 100.0])),
            _ => None,
        };
        let scale = |s: &str| match parse_floats(s)[..] {
            [x, y, ..] => Some(json!([x, y])),
            [x] => Some(json!([x, x])),
            _ => None,
        };
        let rot = |s: &str| s.trim().parse::<f64>().ok().map(|r| json!(-r));
        let p = transform_params(
            &self.anim(c, "position", ls, &pos),
            &self.anim(c, "scale", ls, &scale),
            &self.anim(c, "rotation", ls, &rot),
            &self.anim(c, "anchor", ls, &pos),
        );
        self.report.warn(
            "FCPXML adjust-transform position/anchor taken as percent of frame height, +y up \
             (unverified)",
        );
        let mut e = ffcine_effect("ffcine.geo.transform/1", p);
        leftovers(&mut e, c, &["position", "scale", "rotation", "anchor"]);
        e
    }

    fn crop(&mut self, c: &Element, ls: f64) -> Option<Effect> {
        let mode = c.attr("mode").unwrap_or("trim");
        let rect = match mode {
            "trim" => c.child("trim-rect"),
            "crop" => c.child("crop-rect"),
            _ => None,
        };
        let Some(rect) = rect else {
            self.report.unmapped(format!("adjust-crop mode={mode}"));
            self.report.warn(format!(
                "FCPXML adjust-crop mode {mode:?} (Ken Burns) not mapped"
            ));
            return None;
        };
        let (w, h) = (self.w, self.h);
        let horiz = move |s: &str| {
            s.trim()
                .parse::<f64>()
                .ok()
                .map(|v| json!(v / 100.0 * h / w))
        };
        let vert = |s: &str| s.trim().parse::<f64>().ok().map(|v| json!(v / 100.0));
        let mut p = Map::new();
        p.insert("units".into(), "picture".into());
        p.insert("left".into(), self.anim(rect, "left", ls, &horiz));
        p.insert("right".into(), self.anim(rect, "right", ls, &horiz));
        p.insert("top".into(), self.anim(rect, "top", ls, &vert));
        p.insert("bottom".into(), self.anim(rect, "bottom", ls, &vert));
        if mode == "crop" {
            p.insert("fill".into(), true.into());
        }
        self.report
            .warn("FCPXML adjust-crop edges taken as percent of frame height (unverified)");
        let mut e = ffcine_effect("ffcine.geo.crop/1", p);
        let others: Vec<Value> = c
            .children
            .iter()
            .filter(|r| !std::ptr::eq(*r, rect))
            .map(Element::to_json)
            .collect();
        if !others.is_empty() {
            e.insert_foreign(APP, "children", others.into());
        }
        Some(e)
    }

    fn volume(&mut self, c: &Element, ls: f64) -> Effect {
        let db = |s: &str| {
            let s = s.trim();
            let s = s.strip_suffix("dB").unwrap_or(s).trim();
            if s == "-inf" || s == "-∞" {
                return Some(json!(-96.0));
            }
            s.parse::<f64>().ok().map(|v| json!(v))
        };
        let mut p = Map::new();
        p.insert("gain_db".into(), self.anim(c, "amount", ls, &db));
        if find_param(c, "amount")
            .is_some_and(|p| p.child("fadeIn").is_some() || p.child("fadeOut").is_some())
        {
            self.report
                .warn("FCPXML audio fade handles (fadeIn/fadeOut) kept as foreign data only");
        }
        let mut e = ffcine_effect("ffcine.audio.level/1", p);
        leftovers(&mut e, c, &["amount"]);
        e
    }

    fn blend(&mut self, c: &Element, ls: f64) -> Effect {
        let num = |s: &str| s.trim().parse::<f64>().ok().map(|v| json!(v));
        let mut p = Map::new();
        p.insert("opacity".into(), self.anim(c, "amount", ls, &num));
        if let Some(m) = c.attr("mode") {
            let name = blend_mode_name(m);
            if name.chars().all(|ch| ch.is_ascii_digit()) {
                self.report.warn(format!(
                    "FCPXML blend mode {m:?} has no name: kept as fcpx:{name}"
                ));
                p.insert("mode".into(), format!("fcpx:{name}").into());
            } else {
                p.insert("mode".into(), name.into());
            }
        }
        let mut e = ffcine_effect("ffcine.comp.blend/1", p);
        leftovers(&mut e, c, &["amount", "mode"]);
        e
    }

    fn filter_video(&mut self, c: &Element, _ls: f64, d: &mut Deco) {
        let Some(cdl) = c.child("info-asc-cdl") else {
            let name = c.attr("name").unwrap_or_default();
            self.report.unmapped(format!("filter-video {name:?}"));
            let e = self.foreign_filter(c);
            d.effects.push(e);
            return;
        };
        let triple = |k: &str, def: f64| {
            let v = cdl.attr(k).map(parse_floats).unwrap_or_default();
            match v[..] {
                [r, g, b, ..] => json!([r, g, b]),
                [x] => json!([x, x, x]),
                _ => json!([def, def, def]),
            }
        };
        let (slope, offset, power) = (
            triple("slope", 1.0),
            triple("offset", 0.0),
            triple("power", 1.0),
        );
        let sat = cdl
            .attr("saturation")
            .or_else(|| cdl.attr("sat"))
            .and_then(|s| s.trim().parse::<f64>().ok());
        let mut p = Map::new();
        p.insert("slope".into(), slope.clone());
        p.insert("offset".into(), offset.clone());
        p.insert("power".into(), power.clone());
        p.insert("saturation".into(), sat.map_or(Value::Null, Value::from));
        let mut e = ffcine_effect("ffcine.color.cdl/1", p);
        let mut mirror = json!({ "asc_sop": { "slope": slope, "offset": offset, "power": power } });
        if let Some(s) = sat {
            mirror["asc_sat"] = s.into();
        }
        d.cdl = Some(mirror);
        let mut rest = c.clone();
        rest.children.retain(|x| x.name != "info-asc-cdl");
        if !rest.children.is_empty() {
            self.report.warn(format!(
                "FCPXML {:?}: only the ASC CDL is rendered, other parameters kept as foreign data",
                c.attr("name").unwrap_or("colour filter")
            ));
        }
        e.set_foreign(APP, rest.to_json());
        d.effects.push(e);
    }

    fn foreign_filter(&self, c: &Element) -> Effect {
        let name = c.attr("name").unwrap_or_default();
        let res = self.doc.effects.get(c.attr("ref").unwrap_or_default());
        let effect_name = res
            .and_then(|r| r.attr("uid").or_else(|| r.attr("name")))
            .unwrap_or(name);
        let mut data = c.to_json();
        if let Some(r) = res {
            data["effect"] = r.to_json();
        }
        foreign_effect(name, effect_name, APP, data)
    }

    fn marker(&mut self, c: &Element, to_src: &dyn Fn(f64) -> f64) -> Marker {
        let start = time_attr(c, "start").unwrap_or(0.0);
        let dur = time_attr(c, "duration").unwrap_or(0.0);
        let colour = match (c.name.as_str(), c.attr("completed")) {
            ("chapter-marker", _) => "ORANGE",
            (_, Some("1")) => "GREEN",
            (_, Some(_)) => "RED",
            _ => "BLUE",
        };
        let mut m = Marker::new(
            c.attr("value").unwrap_or_default(),
            range(self.fr(to_src(start)), self.fr(dur), self.rate),
            colour,
        );
        if let Some(n) = c.attr("note") {
            m.extra.insert("comment".into(), n.into());
        }
        let mut f = Map::new();
        f.insert("element".into(), c.name.clone().into());
        if let Some(a) = c.attrs_except(&["start", "duration", "value", "note"]) {
            f.insert("attrs".into(), a);
        }
        if c.name != "marker" || f.len() > 1 {
            m.set_foreign(APP, f.into());
        }
        m
    }
}

fn find_param<'e>(el: &'e Element, name: &str) -> Option<&'e Element> {
    el.children_named("param")
        .find(|p| p.attr("name").is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

/// NTSC rates written rounded (`23.98`, `29.97`, `59.94`) -> exact.
fn ntsc(r: f64) -> f64 {
    for base in [24.0, 30.0, 48.0, 60.0, 120.0] {
        let n = base * 1000.0 / 1001.0;
        if (r - n).abs() < 0.01 {
            return n;
        }
    }
    r
}

/// Unconsumed attributes and params of an `adjust-*` element, onto its effect's foreign block.
fn leftovers(e: &mut Effect, c: &Element, used: &[&str]) {
    if let Some(a) = c.attrs_except(used) {
        e.insert_foreign(APP, "attrs", a);
    }
    let rest: Vec<Value> = c
        .children
        .iter()
        .filter(|p| {
            p.name != "param"
                || !p
                    .attr("name")
                    .is_some_and(|n| used.iter().any(|u| n.eq_ignore_ascii_case(u)))
                || p.children.iter().any(|k| k.name != "keyframeAnimation")
        })
        .map(Element::to_json)
        .collect();
    if !rest.is_empty() {
        e.insert_foreign(APP, "children", rest.into());
    }
}

fn set_foreign(obj: &mut impl HasMetadata, el: &Element, children: Vec<Value>) {
    let mut f = Map::new();
    if let Some(a) = el.attrs_except(ITEM_ATTRS) {
        f.insert("attrs".into(), a);
    }
    if !children.is_empty() {
        f.insert("children".into(), children.into());
    }
    if !f.is_empty() {
        f.insert("element".into(), el.name.clone().into());
        obj.set_foreign(APP, f.into());
    }
}

fn finish_clip(clip: &mut Clip, el: &Element, d: Deco, id: &str) {
    clip.set_ffcine_id(id);
    clip.enabled = el.attr("enabled") != Some("0");
    clip.effects = d.effects;
    clip.markers = d.markers;
    if let Some(cdl) = d.cdl {
        clip.metadata.insert("cdl".into(), cdl);
    }
    set_foreign(clip, el, d.foreign);
}

fn finish_stack(stack: &mut Stack, el: &Element, d: Deco, id: &str) {
    stack.set_ffcine_id(id);
    stack.enabled = el.attr("enabled") != Some("0");
    stack.effects = d.effects;
    stack.markers = d.markers;
    if let Some(cdl) = d.cdl {
        stack.metadata.insert("cdl".into(), cdl);
    }
    set_foreign(stack, el, d.foreign);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_map_eval_and_constancy() {
        let tm = TimeMap {
            pts: vec![
                (0.0, 0.0, Interp::Linear),
                (2.0, 4.0, Interp::Linear),
                (4.0, 6.0, Interp::Ease),
            ],
        };
        assert!((tm.eval(1.0) - 2.0).abs() < 1e-9);
        assert!((tm.eval(3.0) - 5.0).abs() < 1e-9);
        assert!((tm.eval(5.0) - 7.0).abs() < 1e-9);
        assert!(tm.is_constant(0.0, 2.0));
        assert!(!tm.is_constant(0.0, 4.0));
    }

    #[test]
    fn ntsc_rates() {
        assert!((ntsc(23.98) - 24000.0 / 1001.0).abs() < 1e-9);
        assert!((ntsc(25.0) - 25.0).abs() < 1e-9);
    }
}
