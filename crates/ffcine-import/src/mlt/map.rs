//! MLT document -> ffcine timeline.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{Doc, Node, invalid, parse_anim, parse_colour, parse_rect, parse_time};
use crate::common::{
    Event, Group, Interp, Kind, blend_mode_name, build_track, ffcine_effect, foreign_effect,
    keyframe, linear_to_db, marker_color, range, rebase_keyframes, rt, set_transition_kind,
    time_ramp, transform_params,
};
use crate::xml::{Element, Ids, Report};
use crate::{ImportOptions, Imported};
use ffcine_core::Result;
use ffcine_timeline::{
    Clip, Effect, ExternalReference, FFCINE_VERSION, GeneratorReference, HasMetadata, Item,
    LinearTimeWarp, Marker, MediaReference, MissingReference, Stack, Timeline, Track, Transition,
    track_kind,
};

const APP: &str = "mlt";

/// Producers whose `resource` is a media file.
const FILE_SERVICES: &[&str] = &[
    "avformat",
    "avformat-novalidate",
    "qimage",
    "pixbuf",
    "xml",
    "glaxnimate",
    "sox",
];

/// Kdenlive's marker for filters and transitions it adds itself (track mixing, compositing).
fn internal(n: &Node) -> bool {
    n.prop("internal_added") == Some("237") || n.prop("always_active") == Some("1")
}

pub(super) fn map(doc: &Doc, opts: &ImportOptions) -> Result<Imported> {
    let mut report = Report::default();
    let profile = doc.profile.clone().unwrap_or_default();
    if let (Some(want), Some(file)) = (opts.rate, profile.fps)
        && (want - file).abs() > 1e-6
    {
        report.warn(format!(
            "rate {want} ignored: the profile is {file} fps (retime it after import)"
        ));
    }
    let rate = profile
        .fps
        .or(opts.rate)
        .ok_or_else(|| invalid("no <profile> frame rate; pass a rate"))?;
    let (w, h) = match (profile.width, profile.height) {
        (Some(w), Some(h)) => (w, h),
        _ => opts.frame_size.unwrap_or_else(|| {
            report.warn("no frame size in the profile or options: assumed 1920x1080");
            (1920, 1080)
        }),
    };
    let main = main_tractor(doc).ok_or_else(|| invalid("no tractor or playlist to import"))?;
    let mut kd_markers = HashMap::new();
    for id in &doc.order {
        if let Some(n) = doc.get(id)
            && let (Some(kid), Some(m)) = (n.prop("kdenlive:id"), n.prop("kdenlive:markers"))
        {
            kd_markers
                .entry(kid.to_owned())
                .or_insert_with(|| m.to_owned());
        }
    }
    let sequences = doc
        .order
        .iter()
        .filter_map(|id| doc.get(id))
        .filter(|n| n.tag() == "tractor" && n.prop("kdenlive:uuid").is_some())
        .count();
    if sequences > 1 {
        report.warn(format!(
            "{sequences} Kdenlive sequences: imported the active one, others only where used"
        ));
    }
    let mut m = Mapper {
        doc,
        rate,
        w: f64::from(w),
        h: f64::from(h),
        report,
        ids: Ids::default(),
        kd_markers,
        depth: 0,
    };

    let mut tl = Timeline::new(
        main.prop("kdenlive:clipname")
            .or_else(|| doc.mlt.attr("title"))
            .unwrap_or_default(),
    );
    tl.global_start_time = Some(rt(0.0, rate));
    let (tracks, mut stack_foreign) = m.tractor(main, "");
    tl.tracks.children = tracks.into_iter().map(Item::Track).collect();
    tl.tracks.markers = m.guides(main);
    let cx = FCtx {
        entry_in: 0.0,
        len: 0.0,
    };
    let (effects, mut internal_fx) = m.filters(&main.children("filter"), &cx);
    tl.tracks.effects = effects;
    stack_foreign.append(&mut internal_fx);
    if !stack_foreign.is_empty() {
        tl.tracks
            .insert_foreign(APP, "children", stack_foreign.into());
    }

    let ns = tl.ffcine_mut();
    ns.insert("v".into(), FFCINE_VERSION.into());
    ns.insert("frame_size".into(), json!([w, h]));
    let mut foreign = Map::new();
    foreign.insert("mlt".into(), doc.mlt.to_json());
    foreign.insert("tractor".into(), props_json(main));
    if let Some(bin) = doc.get("main_bin") {
        foreign.insert("main_bin".into(), props_json(bin));
    }
    tl.set_foreign(APP, Value::Object(foreign));
    rebase_keyframes(&mut tl.tracks.children);
    Ok(Imported {
        timeline: tl,
        report: m.report.finish(),
    })
}

/// The tractor MLT would play: the last top-level tractor (else playlist), unwrapped while it is
/// a single-track wrapper around another tractor (Kdenlive's project tractor -> sequence).
fn main_tractor(doc: &Doc) -> Option<&Node> {
    let top: Vec<&Node> = doc.top.iter().filter_map(|id| doc.get(id)).collect();
    let mut main = top
        .iter()
        .rev()
        .find(|n| n.tag() == "tractor")
        .or_else(|| top.iter().rev().find(|n| n.tag() == "playlist"))
        .copied()?;
    for _ in 0..8 {
        let refs = track_refs(main);
        match refs.as_slice() {
            [only] => match doc.get(&only.producer) {
                Some(n) if n.tag() == "tractor" => main = n,
                _ => break,
            },
            _ => break,
        }
    }
    Some(main)
}

/// A `<track>` of a tractor.
struct TrackRef {
    producer: String,
    hide: Option<String>,
}

fn track_refs(t: &Node) -> Vec<TrackRef> {
    fn push(e: &Element, out: &mut Vec<TrackRef>) {
        for c in &e.children {
            let producer = match c.name.as_str() {
                "track" => c.attr("producer"),
                // Old MLT: services defined inline in a multitrack are tracks too.
                "playlist" | "producer" | "tractor" if e.name == "multitrack" => c.attr("id"),
                _ => None,
            };
            if let Some(p) = producer {
                out.push(TrackRef {
                    producer: p.to_owned(),
                    hide: c.attr("hide").map(str::to_owned),
                });
            }
        }
    }
    let mut out = Vec::new();
    push(&t.el, &mut out);
    for mt in t.el.children_named("multitrack") {
        push(mt, &mut out);
    }
    out
}

fn props_json(n: &Node) -> Value {
    let m: Map<String, Value> = n
        .props
        .iter()
        .map(|(k, v)| (k.clone(), v.clone().into()))
        .collect();
    let mut o = Map::new();
    o.insert("id".into(), n.id().into());
    if let Some(a) = n.el.attrs_except(&["id"]) {
        o.insert("attrs".into(), a);
    }
    o.insert("props".into(), m.into());
    Value::Object(o)
}

/// A clip placed on a track (positions in frames).
struct Placed {
    start: f64,
    len: f64,
    item: Item,
    /// Media frames per timeline frame (`timewarp`).
    speed: f64,
    producer: String,
}

#[allow(clippy::large_enum_variant)] // short-lived, one per playlist entry
enum Slot {
    Clip(Placed),
    /// A Shotcut / `melt -mix` transition tractor between two entries.
    Mix {
        start: f64,
        len: f64,
        node: Node,
    },
}

/// Where a filter sits: keyframe positions are in producer frames, the clip starts at
/// `entry_in`.
struct FCtx {
    entry_in: f64,
    len: f64,
}

struct Mapper<'a> {
    doc: &'a Doc,
    rate: f64,
    w: f64,
    h: f64,
    report: Report,
    ids: Ids,
    /// `kdenlive:markers` JSON by `kdenlive:id`.
    kd_markers: HashMap<String, String>,
    depth: usize,
}

/// Moves an item's head by `head` frames (positive extends it earlier) and its tail by `tail`.
fn retime(item: &mut Item, head: f64, tail: f64, speed: f64) {
    let r = match item {
        Item::Clip(c) => c.source_range.as_mut(),
        Item::Stack(s) => s.source_range.as_mut(),
        Item::Gap(g) => g.source_range.as_mut(),
        _ => None,
    };
    if let Some(r) = r {
        r.start_time.value = crate::xml::clean(r.start_time.value - head * speed.abs());
        r.duration.value = crate::xml::clean(r.duration.value + head + tail);
    }
}

impl Mapper<'_> {
    fn time(&self, s: Option<&str>) -> Option<f64> {
        s.and_then(|s| parse_time(s, self.rate))
    }

    /// Tracks of a tractor, plus the foreign JSON of transitions between tracks.
    fn tractor(&mut self, t: &Node, scope: &str) -> (Vec<Track>, Vec<Value>) {
        let mut tracks = Vec::new();
        for (i, r) in track_refs(t).iter().enumerate() {
            let Some(node) = self.doc.get(&r.producer) else {
                self.report
                    .warn(format!("MLT track producer {:?} not found", r.producer));
                continue;
            };
            if is_background(node) {
                continue;
            }
            let kind = track_kind_of(self.doc, node, r.hide.as_deref());
            let key = format!("{scope}/{i}");
            tracks.extend(self.track(node, kind, &key));
        }
        let (mut nv, mut na) = (0, 0);
        for t in &mut tracks {
            let n = if t.kind == track_kind::AUDIO {
                na += 1;
                na
            } else {
                nv += 1;
                nv
            };
            if t.name.is_empty() {
                t.name = format!(
                    "{}{n}",
                    if t.kind == track_kind::AUDIO {
                        "A"
                    } else {
                        "V"
                    }
                );
            }
        }
        let mut foreign = Vec::new();
        for tr in t.children("transition") {
            if internal(&tr) || plain_compositing(&tr) {
                foreign.push(tr.el.to_json());
                continue;
            }
            let s = tr.service().to_owned();
            self.report.unmapped(format!("composition {s}"));
            self.report.warn(format!(
                "MLT {s} composition between tracks kept as foreign data, not rendered"
            ));
            foreign.push(tr.el.to_json());
        }
        (tracks, foreign)
    }

    /// One MLT track (a playlist, a Kdenlive track tractor, or a bare producer) -> one track,
    /// or more if clips overlap without a mix.
    fn track(&mut self, node: &Node, kind: Kind, key: &str) -> Vec<Track> {
        let mut placed = Vec::new();
        let mut transitions = Vec::new();
        let mut filters = node.children("filter");
        let name;
        match node.tag() {
            "playlist" => {
                name = node.prop("shotcut:name").unwrap_or_default().to_owned();
                self.playlist(node, kind, key, &mut placed, &mut transitions);
            }
            "tractor" => {
                name = node
                    .prop("kdenlive:track_name")
                    .unwrap_or_default()
                    .to_owned();
                for (si, r) in track_refs(node).iter().enumerate() {
                    if let Some(sub) = self.doc.get(&r.producer) {
                        if sub.tag() == "playlist" {
                            filters.extend(sub.children("filter"));
                            let k = format!("{key}.{si}");
                            self.playlist(sub, kind, &k, &mut placed, &mut transitions);
                        } else {
                            placed.push(self.bare(sub, kind, &format!("{key}.{si}")));
                        }
                    }
                }
                for tr in node.children("transition") {
                    if internal(&tr) {
                        continue;
                    }
                    if let Some(t) = self.kd_mix(&mut placed, &tr) {
                        transitions.push(t);
                    } else {
                        self.report.warn(format!(
                            "MLT {} transition in a track not matched to a mix",
                            tr.service()
                        ));
                    }
                }
            }
            _ => {
                name = String::new();
                placed.push(self.bare(node, kind, key));
            }
        }

        let lanes = spread(placed, transitions, &mut self.report);
        let cx = FCtx {
            entry_in: 0.0,
            len: 0.0,
        };
        let (effects, internal_fx) = self.filters(&filters, &cx);
        let k = if kind == Kind::Audio {
            track_kind::AUDIO
        } else {
            track_kind::VIDEO
        };
        let n = lanes.len();
        lanes
            .into_iter()
            .enumerate()
            .map(|(i, g)| {
                let nm = if i == 0 || name.is_empty() {
                    name.clone()
                } else {
                    format!("{name} ({})", i + 1)
                };
                let mut t = Track::new(nm, k);
                t.children = build_track(vec![g], self.rate);
                if i == 0 {
                    t.effects.clone_from(&effects);
                    let mut f = props_json(node);
                    if !internal_fx.is_empty() {
                        f["internal_filters"] = internal_fx.clone().into();
                    }
                    t.set_foreign(APP, f);
                }
                if n > 1 {
                    t.ffcine_mut()
                        .insert("overflow_of".into(), name.clone().into());
                }
                t
            })
            .collect()
    }

    /// A producer used directly as a track, from its own in..out.
    fn bare(&mut self, p: &Node, kind: Kind, key: &str) -> Placed {
        let in_ = self.time(p.el.attr("in")).unwrap_or(0.0);
        let len = self.time(p.el.attr("out")).map_or(0.0, |o| o - in_ + 1.0);
        let (item, speed) =
            self.entry_item(&Node::new(&Element::default()), p, in_, len, kind, key);
        Placed {
            start: 0.0,
            len,
            item,
            speed,
            producer: p.id().to_owned(),
        }
    }

    /// Places a playlist's entries from frame 0.
    fn playlist(
        &mut self,
        pl: &Node,
        kind: Kind,
        key: &str,
        placed: &mut Vec<Placed>,
        transitions: &mut Vec<(f64, Transition)>,
    ) {
        let mut slots = Vec::new();
        let mut pos = 0.0;
        for c in &pl.el.children {
            let (pid, entry) = match c.name.as_str() {
                "blank" => {
                    pos += self.time(c.attr("length")).unwrap_or(0.0);
                    continue;
                }
                "entry" => (c.attr("producer").unwrap_or_default(), Node::new(c)),
                "producer" | "chain" | "tractor" | "playlist" => (
                    c.attr("id").unwrap_or_default(),
                    Node::new(&Element::default()),
                ),
                _ => continue,
            };
            let Some(p) = self.doc.get(pid) else {
                self.report
                    .warn(format!("MLT entry producer {pid:?} not found: dropped"));
                continue;
            };
            let in_ = self
                .time(entry.el.attr("in"))
                .or_else(|| self.time(p.el.attr("in")))
                .unwrap_or(0.0);
            let out = self
                .time(entry.el.attr("out"))
                .or_else(|| self.time(p.el.attr("out")))
                .or_else(|| self.time(p.prop("length")).map(|l| l - 1.0))
                .unwrap_or(in_);
            let len = out - in_ + 1.0;
            if p.tag() == "tractor" && is_transition_tractor(p) {
                slots.push(Slot::Mix {
                    start: pos,
                    len,
                    node: p.clone(),
                });
            } else {
                let k = format!("{key}@{pos}");
                let (item, speed) = self.entry_item(&entry, p, in_, len, kind, &k);
                slots.push(Slot::Clip(Placed {
                    start: pos,
                    len,
                    item,
                    speed,
                    producer: pid.to_owned(),
                }));
            }
            pos += len;
        }
        // Transition tractors: extend the neighbours over them and cut in the middle.
        for i in 0..slots.len() {
            let Slot::Mix { start, len, node } = &slots[i] else {
                continue;
            };
            let (start, len, node) = (*start, *len, node.clone());
            let refs = track_refs(&node);
            let ok = i > 0
                && i + 1 < slots.len()
                && matches!((&slots[i - 1], &slots[i + 1]), (Slot::Clip(a), Slot::Clip(b))
                    if refs.len() == 2 && refs[0].producer == a.producer && refs[1].producer == b.producer
                        && (a.start + a.len - start).abs() < 1e-6 && (b.start - start - len).abs() < 1e-6);
            if !ok {
                let k = format!("{key}@{start}");
                let (item, speed) =
                    self.entry_item(&Node::new(&Element::default()), &node, 0.0, len, kind, &k);
                slots[i] = Slot::Clip(Placed {
                    start,
                    len,
                    item,
                    speed,
                    producer: node.id().to_owned(),
                });
                continue;
            }
            let k = (len / 2.0).floor();
            if let Slot::Clip(a) = &mut slots[i - 1] {
                a.len += k;
                retime(&mut a.item, 0.0, k, a.speed);
            }
            if let Slot::Clip(b) = &mut slots[i + 1] {
                let d = len - k;
                b.start -= d;
                b.len += d;
                retime(&mut b.item, d, 0.0, b.speed);
            }
            let tr = self.mix_transition(&node.children("transition"), k, len - k);
            transitions.push((start + k, tr));
        }
        placed.extend(slots.into_iter().filter_map(|s| match s {
            Slot::Clip(p) => Some(p),
            Slot::Mix { .. } => None,
        }));
    }

    /// A Kdenlive same-track mix: the transition overlaps the end of a clip on one playlist and
    /// the start of a clip on the other. Trims both to the cut.
    fn kd_mix(&mut self, placed: &mut [Placed], tr: &Node) -> Option<(f64, Transition)> {
        let tin = self.time(tr.el.attr("in"))?;
        let tout = self.time(tr.el.attr("out")).unwrap_or(tin);
        let y = placed
            .iter()
            .enumerate()
            .filter(|(_, p)| p.start >= tin - 1.0 && p.start <= tout)
            .min_by(|a, b| (a.1.start - tin).abs().total_cmp(&(b.1.start - tin).abs()))
            .map(|(i, _)| i)?;
        let ys = placed[y].start;
        let x = placed
            .iter()
            .enumerate()
            .filter(|(i, p)| *i != y && p.start < ys && p.start + p.len > ys)
            .min_by(|a, b| {
                let ea = a.1.start + a.1.len - tout;
                let eb = b.1.start + b.1.len - tout;
                ea.abs().total_cmp(&eb.abs())
            })
            .map(|(i, _)| i)?;
        let xe = placed[x].start + placed[x].len;
        let mixcut = self
            .time(tr.prop("kdenlive:mixcut"))
            .unwrap_or(((xe - ys) / 2.0).floor());
        let cut = (ys + mixcut).clamp(ys, xe);
        let px = &mut placed[x];
        px.len -= xe - cut;
        retime(&mut px.item, 0.0, -(xe - cut), px.speed);
        let py = &mut placed[y];
        let d = cut - ys;
        py.start = cut;
        py.len -= d;
        retime(&mut py.item, -d, 0.0, py.speed);
        let t = self.mix_transition(std::slice::from_ref(tr), cut - ys, xe - cut);
        Some((cut, t))
    }

    fn mix_transition(&mut self, nodes: &[Node], in_off: f64, out_off: f64) -> Transition {
        let mut t = Transition::dissolve(rt(in_off, self.rate), rt(out_off, self.rate));
        if let Some(l) = nodes.iter().find(|n| n.service() == "luma") {
            t.name = "luma".into();
            if let Some(res) = l.prop("resource").filter(|r| !r.is_empty()) {
                self.report
                    .warn("MLT luma wipe rendered as a dissolve (wipe kind and file kept)");
                let mut kind = json!({ "kind": "wipe", "resource": res });
                for p in ["softness", "invert", "reverse"] {
                    if let Some(v) = l.prop(p) {
                        kind[p] = v.into();
                    }
                }
                set_transition_kind(&mut t, kind);
            }
        } else if let Some(n) = nodes.first() {
            n.service().clone_into(&mut t.name);
            if !matches!(n.service(), "mix" | "luma" | "") {
                self.report.warn(format!(
                    "MLT {} mix rendered as a dissolve (kind kept)",
                    n.service()
                ));
                set_transition_kind(&mut t, json!({ "kind": n.service() }));
            }
        }
        let f: Vec<Value> = nodes.iter().map(|n| n.el.to_json()).collect();
        t.set_foreign(APP, json!({ "transitions": f }));
        t
    }

    fn resolve(&self, res: &str) -> String {
        let p = std::path::Path::new(res);
        if res.contains("://") || p.is_absolute() {
            return res.to_owned();
        }
        match &self.doc.root {
            Some(root) => std::path::Path::new(root)
                .join(p)
                .to_string_lossy()
                .into_owned(),
            None => res.to_owned(),
        }
    }

    fn external(&self, p: &Node, path: &str) -> MediaReference {
        let len = self.time(p.prop("length")).filter(|l| *l > 0.0 && *l < 1e8);
        let avail = len.map(|l| range(0.0, l, self.rate));
        let path = self.resolve(path);
        let mut e = if path.contains("://") {
            ExternalReference::new(path, avail)
        } else {
            ExternalReference::from_path(std::path::Path::new(&path), avail)
        };
        if let Some(h) = p.prop("kdenlive:file_hash") {
            e.ffcine_mut()
                .insert("source".into(), json!({ "kdenlive_hash": h }));
        }
        MediaReference::External(e)
    }

    /// The OTIO item for a playlist entry of producer `p` (`in_`, `len` in producer frames).
    fn entry_item(
        &mut self,
        entry: &Node,
        p: &Node,
        in_: f64,
        len: f64,
        kind: Kind,
        key: &str,
    ) -> (Item, f64) {
        let id = self.ids.make(APP, &format!("{key}|{}|{in_}", p.id()));
        if matches!(p.tag(), "tractor" | "playlist") {
            return (self.nested(p, in_, len, kind, &id), 1.0);
        }
        let service = p.service();
        let resource = p.prop("resource").unwrap_or_default();
        let mut filters: Vec<Node> = p.children("filter");
        filters.extend(entry.children("filter"));
        let mut effects = Vec::new();
        let mut speed = 1.0;
        let text_filter = filters
            .iter()
            .position(|f| matches!(f.service(), "dynamictext" | "qtext"));
        let media = match service {
            "timewarp" => {
                let (sp, path) = resource.split_once(':').unwrap_or(("1", resource));
                speed = p
                    .prop("warp_speed")
                    .and_then(|s| s.parse().ok())
                    .or_else(|| sp.parse().ok())
                    .unwrap_or(1.0);
                let path = p.prop("warp_resource").unwrap_or(path);
                if speed < 0.0 {
                    self.report
                        .warn("MLT reverse timewarp: source in-point approximated");
                }
                effects.push(Effect::LinearTimeWarp(LinearTimeWarp::new(speed)));
                self.external(p, path)
            }
            "color" | "colour" => {
                if let Some(i) = text_filter {
                    let f = filters.remove(i);
                    self.shotcut_text(&f, in_, len)
                } else {
                    let c = parse_colour(resource).unwrap_or([0.0, 0.0, 0.0, 1.0]);
                    let mut g = GeneratorReference::new(
                        "ffcine.solid",
                        Map::from_iter([("colour".to_owned(), json!(c))]),
                    );
                    g.available_range = Some(range(0.0, in_ + len, self.rate));
                    MediaReference::Generator(g)
                }
            }
            "kdenlivetitle" => self.kd_title(p, in_ + len),
            s if FILE_SERVICES.contains(&s) || (s.is_empty() && !resource.is_empty()) => {
                self.external(p, resource)
            }
            s => {
                self.report.unmapped(format!("producer {s}"));
                self.report.warn(format!(
                    "MLT {s} producer not mapped: left as a clip with missing media"
                ));
                MediaReference::Missing(MissingReference {
                    name: s.to_owned(),
                    ..MissingReference::default()
                })
            }
        };
        let name = clip_name(p);
        let mut clip = Clip::new(name, media, Some(range(in_ * speed.abs(), len, self.rate)));
        let cx = FCtx { entry_in: in_, len };
        for l in p.children("link") {
            if l.service() == "timeremap" {
                effects.extend(self.time_remap(&l, &cx));
            } else {
                self.report.unmapped(format!("link {}", l.service()));
                effects.push(foreign_effect(
                    l.service(),
                    l.service(),
                    APP,
                    l.el.to_json(),
                ));
            }
        }
        let (fx, internal_fx) = self.filters(&filters, &cx);
        effects.extend(fx);
        clip.effects = effects;
        clip.markers = self.clip_markers(p);
        clip.set_ffcine_id(id);
        let mut f = Map::new();
        f.insert("producer".into(), props_json(p));
        if !entry.props.is_empty() {
            f.insert("entry".into(), props_json(entry));
        }
        if !internal_fx.is_empty() {
            f.insert("internal_filters".into(), internal_fx.into());
        }
        clip.set_foreign(APP, f.into());
        (Item::Clip(clip), speed)
    }

    fn nested(&mut self, p: &Node, in_: f64, len: f64, kind: Kind, id: &str) -> Item {
        if self.depth > 16 {
            self.report
                .warn("MLT tractors nested too deep (cycle?): left as a missing clip");
            let mut c = Clip::new(
                p.id(),
                MediaReference::Missing(MissingReference::default()),
                Some(range(in_, len, self.rate)),
            );
            c.set_ffcine_id(id);
            return Item::Clip(c);
        }
        self.depth += 1;
        let (tracks, foreign) = if p.tag() == "tractor" {
            self.tractor(p, id)
        } else {
            (self.track(p, kind, id), Vec::new())
        };
        self.depth -= 1;
        let mut s = Stack {
            name: p
                .prop("kdenlive:clipname")
                .or_else(|| p.prop("shotcut:name"))
                .unwrap_or(p.id())
                .to_owned(),
            source_range: Some(range(in_, len, self.rate)),
            children: tracks.into_iter().map(Item::Track).collect(),
            ..Stack::default()
        };
        s.markers = self.guides(p);
        s.set_ffcine_id(id);
        let mut f = props_json(p);
        if !foreign.is_empty() {
            f["children"] = foreign.into();
        }
        s.set_foreign(APP, f);
        Item::Stack(s)
    }

    /// Kdenlive title clip (`xmldata`) -> `ffcine.text`.
    fn kd_title(&mut self, p: &Node, len: f64) -> MediaReference {
        let mut params = Map::new();
        let xml = p.prop("xmldata").unwrap_or_default();
        if let Ok(t) = Element::parse(xml) {
            let tw: f64 = t
                .attr("width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.w);
            let th: f64 = t
                .attr("height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.h);
            let items: Vec<&Element> = t
                .children_named("item")
                .filter(|i| i.attr("type") == Some("QGraphicsTextItem"))
                .collect();
            if items.len() > 1 {
                self.report
                    .warn("Kdenlive title with several text items: joined, first item's style");
            }
            let texts: Vec<&str> = items
                .iter()
                .filter_map(|i| i.child("content"))
                .map(|c| c.text.as_str())
                .collect();
            params.insert("text".into(), texts.join("\n").into());
            if let Some(item) = items.first()
                && let Some(c) = item.child("content")
            {
                if let Some(f) = c.attr("font") {
                    params.insert("font".into(), f.into());
                }
                if let Some(s) = c
                    .attr("font-pixel-size")
                    .and_then(|s| s.parse::<f64>().ok())
                {
                    params.insert("size".into(), (s * self.h / th).into());
                }
                if let Some(col) = c.attr("font-color").and_then(parse_colour) {
                    params.insert("colour".into(), json!(col));
                }
                params.insert(
                    "bold".into(),
                    c.attr("font-weight")
                        .and_then(|w| w.parse::<f64>().ok())
                        .is_some_and(|w| w >= 600.0)
                        .into(),
                );
                params.insert("italic".into(), (c.attr("font-italic") == Some("1")).into());
                if let Some(pos) = item.child("position") {
                    let x: f64 = pos.attr("x").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    let y: f64 = pos.attr("y").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    let bw: f64 = c
                        .attr("box-width")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0.0);
                    let bh: f64 = c
                        .attr("box-height")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0.0);
                    params.insert(
                        "position".into(),
                        json!([(x + bw / 2.0) / tw - 0.5, (y + bh / 2.0) / th - 0.5]),
                    );
                }
            }
        } else {
            self.report
                .warn("Kdenlive title xmldata unreadable: kept as foreign data");
        }
        self.report.warn(
            "Kdenlive title: text, font, colour and position kept; shapes, images, outline and \
             animation are not rendered",
        );
        let mut g = GeneratorReference::new("ffcine.text", params);
        g.available_range = Some(range(0.0, len, self.rate));
        MediaReference::Generator(g)
    }

    /// Shotcut text filter on a colour clip -> `ffcine.text`.
    fn shotcut_text(&mut self, f: &Node, in_: f64, len: f64) -> MediaReference {
        let mut params = Map::new();
        let text = f
            .prop("argument")
            .or_else(|| f.prop("text"))
            .unwrap_or_default();
        params.insert("text".into(), text.into());
        if let Some(v) = f.prop("family") {
            params.insert("font".into(), v.into());
        }
        if let Some(v) = f.prop("size").and_then(|s| s.parse::<f64>().ok()) {
            params.insert("size".into(), v.into());
        }
        if let Some(c) = f.prop("fgcolour").and_then(parse_colour) {
            params.insert("colour".into(), json!(c));
        }
        if let Some(r) = f
            .prop("geometry")
            .filter(|g| !g.contains('='))
            .and_then(|g| parse_rect(g, self.w, self.h))
        {
            params.insert(
                "position".into(),
                json!([
                    (r[0] + r[2] / 2.0) / self.w - 0.5,
                    (r[1] + r[3] / 2.0) / self.h - 0.5
                ]),
            );
        }
        if let Some(a) = f.prop("halign") {
            params.insert("alignment".into(), a.into());
        }
        params.insert(
            "bold".into(),
            f.prop("weight")
                .and_then(|w| w.parse::<f64>().ok())
                .is_some_and(|w| w >= 600.0)
                .into(),
        );
        params.insert("italic".into(), (f.prop("style") == Some("italic")).into());
        self.report.warn(
            "Shotcut text: text, font, colour and position kept; outline, background and \
             animation are not rendered",
        );
        let mut g = GeneratorReference::new("ffcine.text", params);
        g.available_range = Some(range(0.0, in_ + len, self.rate));
        g.set_foreign(APP, f.el.to_json());
        MediaReference::Generator(g)
    }

    /// Kdenlive bin-clip markers (positions in source frames).
    fn clip_markers(&mut self, p: &Node) -> Vec<Marker> {
        let json = p.prop("kdenlive:markers").map(str::to_owned).or_else(|| {
            p.prop("kdenlive:id")
                .and_then(|id| self.kd_markers.get(id).cloned())
        });
        json.map(|j| self.kd_marker_list(&j)).unwrap_or_default()
    }

    fn kd_marker_list(&mut self, json: &str) -> Vec<Marker> {
        let Ok(Value::Array(list)) = serde_json::from_str::<Value>(json) else {
            if !json.trim().is_empty() {
                self.report
                    .warn("Kdenlive markers JSON unreadable: dropped");
            }
            return Vec::new();
        };
        list.iter()
            .filter_map(|m| {
                let pos = m.get("pos")?.as_f64()?;
                let ty = m.get("type").and_then(Value::as_i64).unwrap_or(0);
                let colour = match ty {
                    0 => "PURPLE",
                    1 => "CYAN",
                    2 | 3 => "GREEN",
                    4 => "YELLOW",
                    5 | 6 => "ORANGE",
                    7 | 8 => "RED",
                    _ => "BLUE",
                };
                let mut mk = Marker::new(
                    m.get("comment").and_then(Value::as_str).unwrap_or_default(),
                    range(pos, 0.0, self.rate),
                    colour,
                );
                mk.set_foreign(APP, m.clone());
                Some(mk)
            })
            .collect()
    }

    /// Timeline markers: Kdenlive guides, Shotcut `shotcut:markers`.
    fn guides(&mut self, t: &Node) -> Vec<Marker> {
        if let Some(g) = t.prop("kdenlive:sequenceproperties.guides") {
            return self.kd_marker_list(g);
        }
        if let Some(g) = self
            .doc
            .get("main_bin")
            .and_then(|b| b.prop("kdenlive:docproperties.guides"))
            .filter(|_| t.prop("kdenlive:uuid").is_none())
        {
            return self.kd_marker_list(g);
        }
        let Some(sm) =
            t.el.children_named("properties")
                .find(|p| p.attr("name") == Some("shotcut:markers"))
        else {
            return Vec::new();
        };
        sm.children_named("properties")
            .filter_map(|m| {
                let n = Node::new(m);
                let start = self.time(n.prop("start"))?;
                let end = self.time(n.prop("end")).unwrap_or(start);
                let colour = n
                    .prop("color")
                    .and_then(parse_colour)
                    .map_or("RED", |c| marker_color(c[0], c[1], c[2]));
                let mut mk = Marker::new(
                    n.prop("text").unwrap_or_default(),
                    range(start, end - start, self.rate),
                    colour,
                );
                mk.set_foreign(APP, m.to_json());
                Some(mk)
            })
            .collect()
    }

    /// Mapped effects, and the JSON of Kdenlive's internal filters (kept, not reported).
    fn filters(&mut self, filters: &[Node], cx: &FCtx) -> (Vec<Effect>, Vec<Value>) {
        let mut fx = Vec::new();
        let mut internal_fx = Vec::new();
        for f in filters {
            if internal(f) {
                internal_fx.push(f.el.to_json());
                continue;
            }
            fx.extend(self.filter(f, cx));
        }
        (fx, internal_fx)
    }

    /// Keyframe time (clip frames) of animation position `pos` in filter `f`: positions count
    /// from the filter's `in` (producer frames), or from the clip's first frame without one.
    fn key_t(&self, f: &Node, pos: f64, cx: &FCtx) -> f64 {
        let f_in = self.time(f.el.attr("in")).unwrap_or(cx.entry_in);
        let flen = self
            .time(f.el.attr("out"))
            .map_or(cx.len, |out| out - f_in + 1.0);
        let pos = if pos < 0.0 { flen + pos } else { pos };
        f_in + pos - cx.entry_in
    }

    /// Property `prop` of `f`: keyframes if animated, else the converted value; `Null` if absent.
    fn anim(
        &mut self,
        f: &Node,
        prop: &str,
        cx: &FCtx,
        conv: &dyn Fn(&str) -> Option<Value>,
    ) -> Value {
        let Some(raw) = f.prop(prop) else {
            return Value::Null;
        };
        let Some(keys) = parse_anim(raw, self.rate) else {
            return conv(raw).unwrap_or(Value::Null);
        };
        if keys.iter().any(|k| k.interp == Interp::Ease) {
            self.report
                .warn("MLT smooth/eased keyframes (~ $ - a–D) approximated as ease");
        }
        let list: Vec<Value> = keys
            .iter()
            .filter_map(|k| {
                let v = conv(&k.value)?;
                Some(keyframe(self.key_t(f, k.pos, cx), self.rate, v, k.interp))
            })
            .collect();
        Value::Array(list)
    }

    /// A rect property as (position, scale, opacity), each static or keyframed.
    fn rect(&mut self, f: &Node, prop: &str, cx: &FCtx) -> Option<(Value, Value, Value)> {
        f.prop(prop)?;
        let (w, h) = (self.w, self.h);
        let conv = |i: usize| {
            move |s: &str| {
                let r = parse_rect(s, w, h)?;
                Some(match i {
                    0 => json!([(r[0] + r[2] / 2.0) / w - 0.5, (r[1] + r[3] / 2.0) / h - 0.5]),
                    1 => json!([r[2] / w, r[3] / h]),
                    _ => json!(r[4]),
                })
            }
        };
        let pos = self.anim(f, prop, cx, &conv(0));
        let scale = self.anim(f, prop, cx, &conv(1));
        let opacity = self.anim(f, prop, cx, &conv(2));
        if pos.is_null() {
            return None;
        }
        Some((pos, scale, opacity))
    }

    fn filter(&mut self, f: &Node, cx: &FCtx) -> Vec<Effect> {
        let service = f.service();
        let kid = f.prop("kdenlive_id").unwrap_or_default();
        let sc = f.prop("shotcut:filter").unwrap_or_default();
        let label = [sc, kid, service]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("filter")
            .to_owned();
        if f.prop("disable") == Some("1") {
            self.report.warn(format!(
                "MLT disabled filter {label:?} kept as foreign data"
            ));
            return vec![foreign_effect(&label, service, APP, f.el.to_json())];
        }
        let mut used: Vec<&str> = vec!["mlt_service", "kdenlive_id", "shotcut:filter"];
        let fade_opacity = matches!(kid, "fade_from_black" | "fade_to_black")
            || matches!(
                sc,
                "fadeInBrightness" | "fadeOutBrightness" | "brightnessOpacity" | "movitOpacity"
            );
        let mapped = match service {
            "volume" => self.volume(f, cx, &mut used),
            "brightness" if fade_opacity => Some(self.opacity(f, cx, &mut used)),
            "movit.opacity" => Some(self.opacity(f, cx, &mut used)),
            "affine" | "qtblend" => self.transform(f, cx, &mut used),
            "crop" => Some(self.crop(f, cx, &mut used)),
            "avfilter.lut3d" => f.prop("av.file").map(|file| {
                used.push("av.file");
                let mut p = Map::new();
                p.insert("lut".into(), self.resolve(file).into());
                vec![ffcine_effect("ffcine.color.grade/1", p)]
            }),
            _ => None,
        };
        let Some(mut out) = mapped else {
            return self.unmapped_filter(f, &label);
        };
        let rest: Map<String, Value> = f
            .props
            .iter()
            .filter(|(k, _)| !used.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone().into()))
            .collect();
        let mut fj = Map::new();
        fj.insert("service".into(), service.into());
        if let Some(a) = f.el.attrs_except(&[]) {
            fj.insert("attrs".into(), a);
        }
        if !kid.is_empty() {
            fj.insert("kdenlive_id".into(), kid.into());
        }
        if !sc.is_empty() {
            fj.insert("shotcut_filter".into(), sc.into());
        }
        if !rest.is_empty() {
            fj.insert("props".into(), rest.into());
        }
        for e in &mut out {
            e.set_foreign(APP, fj.clone().into());
        }
        out
    }

    /// A linear ramp from `v0` to `v1` over the filter's in..out (clip frames).
    fn ramp(&self, f: &Node, cx: &FCtx, v0: f64, v1: f64) -> Value {
        let t0 = self.time(f.el.attr("in")).unwrap_or(cx.entry_in) - cx.entry_in;
        let t1 = self
            .time(f.el.attr("out"))
            .map_or(cx.len - 1.0, |o| o - cx.entry_in);
        json!([
            keyframe(t0, self.rate, v0.into(), Interp::Linear),
            keyframe(t1, self.rate, v1.into(), Interp::Linear),
        ])
    }

    /// `volume`: `level` (dB), `gain` (linear), a gain ramp, or Kdenlive `fadein`/`fadeout`.
    fn volume(&mut self, f: &Node, cx: &FCtx, used: &mut Vec<&str>) -> Option<Vec<Effect>> {
        let kid = f.prop("kdenlive_id").unwrap_or_default();
        if matches!(kid, "fadein" | "fadeout") {
            // Kdenlive puts fades at the clip's head / tail, as long as the filter.
            let len = match (self.time(f.el.attr("in")), self.time(f.el.attr("out"))) {
                (Some(a), Some(b)) => b - a + 1.0,
                _ => cx.len,
            };
            used.extend(["gain", "end", "level"]);
            let key = if kid == "fadein" {
                "fade_in"
            } else {
                "fade_out"
            };
            let mut p = Map::new();
            p.insert(key.into(), (len / self.rate).into());
            return Some(vec![ffcine_effect("ffcine.audio.level/1", p)]);
        }
        let gain = if f.prop("level").is_none() && f.prop("end").is_some() {
            let g0 = f
                .prop("gain")
                .and_then(|g| g.parse::<f64>().ok())
                .unwrap_or(1.0);
            let g1 = f
                .prop("end")
                .and_then(|g| g.parse::<f64>().ok())
                .unwrap_or(g0);
            self.report
                .warn("MLT volume gain ramp mapped to a dB ramp (curve differs)");
            used.extend(["gain", "end"]);
            self.ramp(f, cx, linear_to_db(g0), linear_to_db(g1))
        } else if f.prop("level").is_some() {
            let db = |s: &str| {
                let s = s.trim();
                s.strip_suffix("dB")
                    .unwrap_or(s)
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .map(Value::from)
            };
            used.push("level");
            self.anim(f, "level", cx, &db)
        } else if f.prop("gain").is_some() {
            let lin = |s: &str| s.trim().parse::<f64>().ok().map(|g| linear_to_db(g).into());
            used.push("gain");
            self.anim(f, "gain", cx, &lin)
        } else {
            return None;
        };
        let mut p = Map::new();
        p.insert("gain_db".into(), gain);
        Some(vec![ffcine_effect("ffcine.audio.level/1", p)])
    }

    /// Fades to/from black and opacity filters -> `opacity`.
    fn opacity(&mut self, f: &Node, cx: &FCtx, used: &mut Vec<&str>) -> Vec<Effect> {
        let num = |s: &str| s.trim().parse::<f64>().ok().map(Value::from);
        let key = ["alpha", "opacity", "level"]
            .into_iter()
            .find(|k| f.prop(k).is_some_and(|v| v.contains('=')))
            .or_else(|| {
                ["alpha", "opacity"]
                    .into_iter()
                    .find(|k| f.prop(k).is_some())
            });
        let v = if let Some(k) = key {
            used.push(k);
            self.anim(f, k, cx, &num)
        } else {
            let s = f
                .prop("start")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            let e = f
                .prop("end")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(1.0);
            used.extend(["start", "end"]);
            self.ramp(f, cx, s, e)
        };
        let kid = f.prop("kdenlive_id").unwrap_or_default();
        let sc = f.prop("shotcut:filter").unwrap_or_default();
        if kid.starts_with("fade_") || sc.starts_with("fade") {
            self.report
                .warn("MLT fade to/from black mapped to an opacity ramp (approx.)");
        }
        let mut p = Map::new();
        p.insert("opacity".into(), v);
        vec![ffcine_effect("ffcine.comp.blend/1", p)]
    }

    /// `affine` / `qtblend` rect -> transform, plus blend if not opaque / normal.
    fn transform(&mut self, f: &Node, cx: &FCtx, used: &mut Vec<&str>) -> Option<Vec<Effect>> {
        let qt = f.service() == "qtblend";
        let rect_prop = if qt {
            "rect"
        } else if f.prop("transition.rect").is_some() {
            "transition.rect"
        } else {
            "transition.geometry"
        };
        let rot_prop = if qt {
            "rotation"
        } else {
            "transition.fix_rotate_x"
        };
        let (pos, scale, opacity) = self.rect(f, rect_prop, cx)?;
        used.extend([rect_prop, rot_prop]);
        let num = |s: &str| s.trim().parse::<f64>().ok().map(Value::from);
        let p = transform_params(
            &pos,
            &scale,
            &self.anim(f, rot_prop, cx, &num),
            &Value::Null,
        );
        if f.prop("distort").or_else(|| f.prop("transition.distort")) != Some("1") {
            self.report.warn(
                "MLT transform rect: aspect-fit inside the rect not modelled (exact when the \
                 rect has the frame's aspect)",
            );
        }
        let mut out = vec![ffcine_effect("ffcine.geo.transform/1", p)];
        let opaque = match &opacity {
            Value::Array(keys) => keys.iter().all(|k| k["v"].as_f64() == Some(1.0)),
            v => v.as_f64() == Some(1.0),
        };
        let mode = f.prop("compositing").and_then(|c| c.parse::<i64>().ok());
        if !opaque || mode.is_some_and(|m| m != 0) {
            let mut b = Map::new();
            b.insert("opacity".into(), opacity);
            if let Some(m) = mode {
                used.push("compositing");
                b.insert("mode".into(), qt_mode(m).into());
            }
            out.push(ffcine_effect("ffcine.comp.blend/1", b));
        }
        Some(out)
    }

    /// `crop` edges in pixels -> fractions of the profile size.
    fn crop(&mut self, f: &Node, cx: &FCtx, used: &mut Vec<&str>) -> Vec<Effect> {
        let (w, h) = (self.w, self.h);
        let px = |d: f64| move |s: &str| s.trim().parse::<f64>().ok().map(|v| json!(v / d));
        let mut p = Map::new();
        p.insert("units".into(), "picture".into());
        p.insert("left".into(), self.anim(f, "left", cx, &px(w)));
        p.insert("right".into(), self.anim(f, "right", cx, &px(w)));
        p.insert("top".into(), self.anim(f, "top", cx, &px(h)));
        p.insert("bottom".into(), self.anim(f, "bottom", cx, &px(h)));
        used.extend(["left", "right", "top", "bottom"]);
        if f.prop("center") == Some("1") {
            self.report.warn("MLT crop center mode not modelled");
        }
        self.report
            .warn("MLT crop pixels taken relative to the profile size (approx.)");
        vec![ffcine_effect("ffcine.geo.crop/1", p)]
    }

    fn unmapped_filter(&mut self, f: &Node, label: &str) -> Vec<Effect> {
        self.report.unmapped(format!("filter {label}"));
        vec![foreign_effect(label, f.service(), APP, f.el.to_json())]
    }

    /// `timeremap` link: `time_map` / `map` keyframes (source seconds) -> ramp + average speed.
    fn time_remap(&mut self, l: &Node, cx: &FCtx) -> Vec<Effect> {
        let Some(raw) = l.prop("time_map").or_else(|| l.prop("map")) else {
            self.report.unmapped("link timeremap (speed_map)");
            return vec![foreign_effect(
                "timeremap",
                "timeremap",
                APP,
                l.el.to_json(),
            )];
        };
        let Some(keys) = parse_anim(raw, self.rate) else {
            self.report.unmapped("link timeremap");
            return vec![foreign_effect(
                "timeremap",
                "timeremap",
                APP,
                l.el.to_json(),
            )];
        };
        let map: Vec<(f64, f64)> = keys
            .iter()
            .filter_map(|k| {
                let v: f64 = k.value.trim().parse().ok()?;
                Some((self.key_t(l, k.pos, cx), v * self.rate))
            })
            .collect();
        let Some((mut e, avg)) = time_ramp(&map, self.rate, &mut self.report) else {
            self.report.unmapped("link timeremap");
            return vec![foreign_effect(
                "timeremap",
                "timeremap",
                APP,
                l.el.to_json(),
            )];
        };
        self.report.warn(
            "Shotcut time remap: kept as ffcine.time.ramp/1 with constant speed between keyframes \
             (map values taken as source seconds, unverified)",
        );
        e.set_foreign(APP, l.el.to_json());
        vec![e, Effect::LinearTimeWarp(LinearTimeWarp::new(avg))]
    }
}

/// Splits placed clips into non-overlapping groups (the first gets the transitions).
fn spread(
    mut placed: Vec<Placed>,
    transitions: Vec<(f64, Transition)>,
    report: &mut Report,
) -> Vec<Group> {
    placed.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut lanes: Vec<(f64, Group)> = vec![(0.0, Group::default())];
    for p in placed {
        let lane = lanes.iter().position(|(end, _)| p.start >= *end - 1e-6);
        let i = lane.unwrap_or_else(|| {
            lanes.push((0.0, Group::default()));
            lanes.len() - 1
        });
        if i > 0 {
            report.warn("MLT clips overlap on a track without a mix: moved to an extra track");
        }
        lanes[i].0 = p.start + p.len;
        lanes[i].1.events.push(Event::Item {
            start: p.start,
            dur: p.len,
            item: p.item,
        });
    }
    for (cut, transition) in transitions {
        lanes[0]
            .1
            .events
            .push(Event::Transition { cut, transition });
    }
    lanes.into_iter().map(|(_, g)| g).collect()
}

/// `QPainter::CompositionMode` (qtblend `compositing`) -> blend mode name.
fn qt_mode(m: i64) -> String {
    let name = match m {
        0 => "normal",
        12 => "add",
        13 => "multiply",
        14 => "screen",
        15 => "overlay",
        16 => "darken",
        17 => "lighten",
        18 => "colordodge",
        19 => "colorburn",
        20 => "hardlight",
        21 => "softlight",
        22 => "difference",
        23 => "exclusion",
        _ => return format!("qt:{m}"),
    };
    blend_mode_name(name)
}

/// Shotcut's per-track mixing and normal-mode blending (Kdenlive marks its own as internal).
fn plain_compositing(t: &Node) -> bool {
    let plain_mode =
        t.prop("1").is_none_or(|m| m == "normal") && t.prop("compositing").is_none_or(|m| m == "0");
    matches!(
        t.service(),
        "mix" | "frei0r.cairoblend" | "qtblend" | "movit.overlay" | "cairoblend"
    ) && t.prop("rect").is_none()
        && t.prop("geometry").is_none()
        && plain_mode
}

/// Bin name, caption, or the media file name.
fn clip_name(p: &Node) -> String {
    let service = p.service();
    let resource = p.prop("resource").unwrap_or_default();
    [
        p.prop("kdenlive:clipname"),
        p.prop("shotcut:caption"),
        (FILE_SERVICES.contains(&service) || service == "timewarp").then_some(resource),
    ]
    .into_iter()
    .flatten()
    .find(|s| !s.is_empty())
    .map_or_else(
        || service.to_owned(),
        |s| {
            let s = s
                .rsplit_once(':')
                .filter(|_| service == "timewarp")
                .map_or(s, |x| x.1);
            std::path::Path::new(s)
                .file_name()
                .map_or(s.to_owned(), |f| f.to_string_lossy().into_owned())
        },
    )
}

fn is_background(n: &Node) -> bool {
    matches!(n.id(), "black_track" | "background")
        || n.prop("kdenlive:playlistid") == Some("black_track")
}

/// A tractor used as a playlist entry to cross-fade two cuts (Shotcut, `melt -mix`).
fn is_transition_tractor(t: &Node) -> bool {
    t.prop("shotcut:transition").is_some()
        || (track_refs(t).len() == 2
            && !t.children("transition").is_empty()
            && t.children("transition").iter().all(|x| {
                matches!(
                    x.service(),
                    "luma"
                        | "mix"
                        | "composite"
                        | "movit.luma_mix"
                        | "qtblend"
                        | "frei0r.cairoblend"
                )
            }))
}

fn track_kind_of(doc: &Doc, n: &Node, hide: Option<&str>) -> Kind {
    match hide {
        Some("video") => return Kind::Audio,
        Some("audio") => return Kind::Video,
        _ => {}
    }
    if n.prop("kdenlive:audio_track") == Some("1") {
        return Kind::Audio;
    }
    if n.prop("shotcut:audio") == Some("1") && n.prop("shotcut:video") != Some("1") {
        return Kind::Audio;
    }
    if n.tag() == "tractor"
        && let Some(first) = track_refs(n).first().and_then(|r| doc.get(&r.producer))
        && first.prop("kdenlive:audio_track") == Some("1")
    {
        return Kind::Audio;
    }
    Kind::Video
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qt_modes() {
        assert_eq!(qt_mode(0), "normal");
        assert_eq!(qt_mode(13), "multiply");
        assert_eq!(qt_mode(3), "qt:3");
    }
}
