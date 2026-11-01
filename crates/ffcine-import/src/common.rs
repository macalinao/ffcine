//! Timeline-building helpers shared by the FCPXML and MLT readers.
//!
//! # ffcine effects written by these readers
//!
//! Each is an `Effect.1` with `effect_name = "ffcine.<domain>.<name>"` and its fields under
//! `metadata.ffcine` next to `"schema"`. A field is either a plain value or, when animated, a
//! keyframe list `[{"t": RationalTime, "v": value, "interp": "linear" | "ease" | "hold"}]`, where
//! `t` counts from the clip's first frame on the timeline (not from the media start).
//!
//! - `ffcine.geo.transform/1` with `units: "frame"`, `y: "down"`: `translate_x`, `translate_y`,
//!   the image centre's offset from the frame centre as fractions of the output frame width and
//!   height; `scale_x`, `scale_y` (1 = unscaled); `rotate_deg` counter-clockwise; `anchor`
//!   `[x, y]`, the pivot's offset from the image centre in the same units. Readers build it from
//!   `[x, y]` position/scale and clockwise rotation with [`transform_params`].
//! - `ffcine.geo.crop/1` with `units: "picture"`: `left`, `right`, `top`, `bottom` as fractions
//!   of the source width (left, right) and height (top, bottom); `fill: true` if the cropped image
//!   is scaled back up to the frame (FCPXML crop mode).
//! - `ffcine.comp.blend/1`: `opacity` 0–1; `mode` one of [`crate::resolve::BLEND_MODES`]
//!   ([`blend_mode_name`]), or `<app>:<raw>`.
//! - Transitions that aren't dissolves: flat `ffcine.transition/1` fields in the `Transition`'s
//!   `metadata.ffcine` ([`set_transition_kind`]).
//!
//! Keyframe `t` is in the clip's source time; readers write it from the clip's first frame and
//! [`rebase_keyframes`] adds `source_range.start_time` once the timeline is built.
//! - `ffcine.audio.level/1`: `gain_db`; `fade_in`, `fade_out` in seconds (Kdenlive fades).
//! - `ffcine.color.cdl/1`: `slope`, `offset`, `power` (`[r, g, b]`) and optional `saturation`;
//!   mirrored in the clip's `metadata.cdl` as the CMX adapter writes it
//!   (`{"asc_sop": {"slope", "offset", "power"}, "asc_sat"}`).
//! - `ffcine.color.grade/1`: `lut` (a `.cube` path), from LUT filters.
//! - `ffcine.time.ramp/1` (`docs/timeline.md`): `points` `[[u, speed]]` over the source range
//!   (u 0–1) and `source_duration` in seconds. Time maps (source time per clip time, linear
//!   between points) become constant-speed steps: each segment is two points at its ends. The
//!   clip also gets a `LinearTimeWarp` with the average speed.
//!
//! Generators: `ffcine.text` (`text`, `font`, `size`, `colour` `[r, g, b, a]` 0–1, `position` as
//! in the transform, `alignment`, `bold`, `italic`) and `ffcine.solid` (`colour`).

#![allow(clippy::many_single_char_names)] // maths on x, y, w, h, t, v

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::xml::Report;

use ffcine_timeline::{
    Effect, Gap, GenericEffect, HasMetadata, Item, RationalTime, TimeRange, Track, Transition,
    track_kind,
};

pub(crate) fn rt(frames: f64, rate: f64) -> RationalTime {
    RationalTime::new(super::xml::clean(frames), rate)
}

pub(crate) fn range(start: f64, dur: f64, rate: f64) -> TimeRange {
    TimeRange::new(rt(start, rate), rt(dur, rate))
}

pub(crate) fn rt_json(frames: f64, rate: f64) -> Value {
    serde_json::to_value(rt(frames, rate)).unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interp {
    Linear,
    Ease,
    Hold,
}

impl Interp {
    pub fn as_str(self) -> &'static str {
        match self {
            Interp::Linear => "linear",
            Interp::Ease => "ease",
            Interp::Hold => "hold",
        }
    }
}

/// One keyframe, `t` in frames from the clip's first frame.
pub(crate) fn keyframe(t: f64, rate: f64, v: Value, interp: Interp) -> Value {
    let mut k = Map::new();
    k.insert("t".into(), rt_json(t, rate));
    k.insert("v".into(), v);
    k.insert("interp".into(), interp.as_str().into());
    Value::Object(k)
}

/// An ffcine effect with `params` (dropping nulls).
pub(crate) fn ffcine_effect(schema: &str, params: Map<String, Value>) -> Effect {
    let params = params.into_iter().filter(|(_, v)| !v.is_null()).collect();
    Effect::Effect(GenericEffect::ffcine(schema, params))
}

/// An effect that only carries foreign data (`effect_name` as the source names it).
pub(crate) fn foreign_effect(name: &str, effect_name: &str, app: &str, data: Value) -> Effect {
    let mut e = GenericEffect::new(name, effect_name);
    e.set_foreign(app, data);
    Effect::Effect(e)
}

/// `ffcine.time.ramp/1` from a time map, `(clip frame, source frame)` points from the clip's
/// first frame to its last, and the average speed. `None` if the map doesn't move forward.
pub(crate) fn time_ramp(
    map: &[(f64, f64)],
    rate: f64,
    report: &mut Report,
) -> Option<(Effect, f64)> {
    let (first, last) = (map.first()?, map.last()?);
    let (dur, src) = (last.0 - first.0, last.1 - first.1);
    if dur <= 0.0 || src <= 0.0 {
        return None;
    }
    let mut points = Vec::new();
    for w in map.windows(2) {
        let dt = w[1].0 - w[0].0;
        if dt <= 0.0 {
            continue;
        }
        let speed = (w[1].1 - w[0].1) / dt;
        if speed <= 0.0 {
            report.warn("speed ramp with a freeze or reverse segment: not representable, skipped");
            continue;
        }
        let u = |v: f64| super::xml::clean((v - first.1) / src * 1e9) / 1e9;
        points.push(serde_json::json!([u(w[0].1), speed]));
        points.push(serde_json::json!([u(w[1].1), speed]));
    }
    let mut p = Map::new();
    p.insert("points".into(), points.into());
    p.insert("source_duration".into(), (src / rate).into());
    Some((ffcine_effect("ffcine.time.ramp/1", p), src / dur))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Video,
    Audio,
}

/// Something placed on a track at an absolute position (frames).
#[derive(Debug, Clone)]
pub(crate) enum Event {
    Item {
        start: f64,
        dur: f64,
        item: Item,
    },
    /// A transition centred anywhere around `cut`; the items on either side abut at `cut`.
    Transition {
        cut: f64,
        transition: Transition,
    },
}

impl Event {
    fn pos(&self) -> (f64, u8) {
        match self {
            Event::Transition { cut, .. } => (*cut, 0),
            Event::Item { start, .. } => (*start, 1),
        }
    }
}

/// Events that must stay on one track (a storyline with its transitions, or a single clip).
#[derive(Debug, Clone, Default)]
pub(crate) struct Group {
    pub events: Vec<Event>,
}

impl Group {
    pub fn single(start: f64, dur: f64, item: Item) -> Self {
        Self {
            events: vec![Event::Item { start, dur, item }],
        }
    }

    fn extent(&self) -> Option<(f64, f64)> {
        let mut out: Option<(f64, f64)> = None;
        for e in &self.events {
            if let Event::Item { start, dur, .. } = e {
                let (s, t) = (*start, start + dur);
                out = Some(out.map_or((s, t), |(a, b)| (a.min(s), b.max(t))));
            }
        }
        out
    }
}

/// Lane-numbered groups, laid out into OTIO tracks. Lane 0 is the primary storyline; positive
/// lanes stack above it, negative ones below. Groups that would overlap on a lane get an extra
/// track for that lane.
#[derive(Debug, Default)]
pub(crate) struct Lanes {
    lanes: BTreeMap<(Kind, i32), Vec<Vec<Group>>>,
}

const EPS: f64 = 1e-6;

impl Lanes {
    pub fn add(&mut self, kind: Kind, lane: i32, group: Group) {
        let Some((s, e)) = group.extent() else {
            // Only transitions: nothing to place against.
            return;
        };
        let tracks = self.lanes.entry((kind, lane)).or_default();
        for t in tracks.iter_mut() {
            let free = t
                .iter()
                .all(|g| g.extent().is_none_or(|(a, b)| e <= a + EPS || s >= b - EPS));
            if free {
                t.push(group);
                return;
            }
        }
        tracks.push(vec![group]);
    }

    /// Tracks bottom to top: video lanes ascending, then audio lanes from the primary outwards
    /// (`-1`, `-2`, ..., then any positive audio lanes).
    pub fn into_tracks(self, rate: f64) -> Vec<Track> {
        let mut video = Vec::new();
        let mut audio = Vec::new();
        for ((kind, lane), tracks) in self.lanes {
            for groups in tracks {
                let t = build_track(groups, rate);
                match kind {
                    Kind::Video => video.push((lane, t)),
                    Kind::Audio => audio.push((lane, t)),
                }
            }
        }
        audio.sort_by_key(|(lane, _)| if *lane < 0 { -lane } else { 1000 + lane });
        let mut out = Vec::new();
        for (i, (lane, mut children)) in video.into_iter().enumerate() {
            let mut t = Track::new(format!("V{}", i + 1), track_kind::VIDEO);
            t.children.append(&mut children);
            t.ffcine_mut().insert("lane".into(), lane.into());
            out.push(t);
        }
        for (i, (lane, mut children)) in audio.into_iter().enumerate() {
            let mut t = Track::new(format!("A{}", i + 1), track_kind::AUDIO);
            t.children.append(&mut children);
            t.ffcine_mut().insert("lane".into(), lane.into());
            out.push(t);
        }
        out
    }
}

/// Track children from absolute-positioned events, with `Gap`s in between.
pub(crate) fn build_track(groups: Vec<Group>, rate: f64) -> Vec<Item> {
    let mut events: Vec<Event> = groups.into_iter().flat_map(|g| g.events).collect();
    events.sort_by(|a, b| {
        let (pa, ra) = a.pos();
        let (pb, rb) = b.pos();
        pa.total_cmp(&pb).then(ra.cmp(&rb))
    });
    let mut out = Vec::new();
    let mut cursor = 0.0;
    for e in events {
        let (pos, _) = e.pos();
        if pos > cursor + EPS {
            out.push(Item::Gap(Gap::new(rt(pos - cursor, rate))));
            cursor = pos;
        }
        match e {
            Event::Item { start, dur, item } => {
                out.push(item);
                cursor = cursor.max(start + dur);
            }
            Event::Transition { transition, .. } => out.push(Item::Transition(transition)),
        }
    }
    out
}

/// Nearest OTIO marker colour to an sRGB colour (0–1).
pub(crate) fn marker_color(r: f64, g: f64, b: f64) -> &'static str {
    const COLOURS: &[(&str, [f64; 3])] = &[
        ("RED", [1.0, 0.0, 0.0]),
        ("PINK", [1.0, 0.6, 0.8]),
        ("ORANGE", [1.0, 0.55, 0.0]),
        ("YELLOW", [1.0, 1.0, 0.0]),
        ("GREEN", [0.0, 0.8, 0.0]),
        ("CYAN", [0.0, 1.0, 1.0]),
        ("BLUE", [0.0, 0.0, 1.0]),
        ("PURPLE", [0.55, 0.2, 0.8]),
        ("MAGENTA", [1.0, 0.0, 1.0]),
        ("BLACK", [0.0, 0.0, 0.0]),
        ("WHITE", [1.0, 1.0, 1.0]),
    ];
    let d = |c: &[f64; 3]| (c[0] - r).powi(2) + (c[1] - g).powi(2) + (c[2] - b).powi(2);
    COLOURS
        .iter()
        .min_by(|a, b| d(&a.1).total_cmp(&d(&b.1)))
        .map_or("RED", |c| c.0)
}

/// `20 log10(gain)`, floored at -96 dB.
pub(crate) fn linear_to_db(gain: f64) -> f64 {
    if gain <= 0.000_015_85 {
        -96.0
    } else {
        20.0 * gain.log10()
    }
}

/// Blend-mode name in [`crate::resolve::BLEND_MODES`] from forms like `"Multiply"`,
/// `"2 (Multiply)"`, `"Soft Light"`, `"colordodge"`; anything else lowercased without separators.
pub(crate) fn blend_mode_name(raw: &str) -> String {
    let s = raw.trim();
    let s = match (s.find('('), s.rfind(')')) {
        (Some(a), Some(b)) if b > a => &s[a + 1..b],
        _ => s,
    };
    let bare = |m: &str| {
        m.to_lowercase()
            .replace([' ', '-', '_'], "")
            .replace("colour", "color")
    };
    let want = bare(s.trim());
    crate::resolve::BLEND_MODES
        .iter()
        .find(|m| bare(m) == want)
        .map_or(want, |m| (*m).to_owned())
}

/// Component `i` of a vector parameter, plain or keyframed (`[x, y]` -> `x`).
fn component(v: &Value, i: usize) -> Value {
    match v {
        Value::Array(keys) if keys.first().is_some_and(|k| k.get("t").is_some()) => Value::Array(
            keys.iter()
                .map(|k| {
                    let mut k = k.clone();
                    k["v"] = k["v"].get(i).cloned().unwrap_or(Value::Null);
                    k
                })
                .collect(),
        ),
        Value::Array(a) => a.get(i).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// A scalar parameter, plain or keyframed, through `f`.
fn map_scalar(v: &Value, f: impl Fn(f64) -> f64) -> Value {
    match v {
        Value::Array(keys) => Value::Array(
            keys.iter()
                .map(|k| {
                    let mut k = k.clone();
                    if let Some(x) = k["v"].as_f64() {
                        k["v"] = f(x).into();
                    }
                    k
                })
                .collect(),
        ),
        Value::Number(n) => n.as_f64().map_or(Value::Null, |x| f(x).into()),
        _ => Value::Null,
    }
}

/// `ffcine.geo.transform/1` parameters (`units: "frame"`) from `position` and `scale` as
/// `[x, y]` (frame fractions, y down), clockwise `rotation` and `anchor`, each plain or keyframed.
pub(crate) fn transform_params(
    position: &Value,
    scale: &Value,
    rotation: &Value,
    anchor: &Value,
) -> Map<String, Value> {
    let mut p = Map::new();
    p.insert("units".into(), "frame".into());
    p.insert("y".into(), "down".into());
    p.insert("anchor".into(), anchor.clone());
    p.insert("translate_x".into(), component(position, 0));
    p.insert("translate_y".into(), component(position, 1));
    p.insert("scale_x".into(), component(scale, 0));
    p.insert("scale_y".into(), component(scale, 1));
    p.insert("rotate_deg".into(), map_scalar(rotation, |r| -r));
    p
}

/// Marks a transition that isn't a plain dissolve: `fields` (`kind`, `resource`, ...) go flat
/// into its `metadata.ffcine` with `"schema": "ffcine.transition/1"`.
pub(crate) fn set_transition_kind(t: &mut Transition, fields: Value) {
    let ns = t.ffcine_mut();
    ns.insert("schema".into(), "ffcine.transition/1".into());
    if let Value::Object(f) = fields {
        ns.extend(f);
    }
}

/// Moves keyframe times from "frames since the clip's first frame" to the clip's source time
/// (adds `source_range.start_time`) in the ffcine effects of every clip and stack.
pub(crate) fn rebase_keyframes(items: &mut [Item]) {
    for item in items {
        let (range, effects) = match item {
            Item::Clip(c) => (c.source_range.clone(), &mut c.effects),
            Item::Stack(s) => {
                rebase_keyframes(&mut s.children);
                (s.source_range.clone(), &mut s.effects)
            }
            Item::Track(t) => {
                rebase_keyframes(&mut t.children);
                continue;
            }
            _ => continue,
        };
        let Some(start) = range.map(|r| r.start_time) else {
            continue;
        };
        if start.value == 0.0 {
            continue;
        }
        for e in effects.iter_mut() {
            let Effect::Effect(g) = e else { continue };
            let Some(ns) = g.metadata.get_mut(ffcine_timeline::FFCINE) else {
                continue;
            };
            let Some(ns) = ns.as_object_mut() else {
                continue;
            };
            for v in ns.values_mut() {
                let Value::Array(keys) = v else { continue };
                for k in keys {
                    let Some(t) = k.get_mut("t") else { continue };
                    if let Ok(mut rt) = serde_json::from_value::<RationalTime>(t.clone()) {
                        rt.value = super::xml::clean(rt.value + start.rescaled_to(rt.rate).value);
                        *t = serde_json::to_value(rt).unwrap_or_default();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_timeline::{Clip, MediaReference, MissingReference};
    use serde_json::json;

    fn clip(name: &str) -> Item {
        Item::Clip(Clip::new(
            name,
            MediaReference::Missing(MissingReference::default()),
            None,
        ))
    }

    #[test]
    fn lanes_fill_gaps_and_split_overlaps() {
        let mut l = Lanes::default();
        l.add(Kind::Video, 0, Group::single(0.0, 10.0, clip("a")));
        l.add(Kind::Video, 1, Group::single(5.0, 10.0, clip("b")));
        l.add(Kind::Video, 1, Group::single(8.0, 4.0, clip("c")));
        l.add(Kind::Audio, -1, Group::single(0.0, 3.0, clip("d")));
        let tracks = l.into_tracks(24.0);
        let names: Vec<_> = tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["V1", "V2", "V3", "A1"]);
        let v2: Vec<_> = tracks[1].children.iter().map(Item::name).collect();
        assert_eq!(v2, ["", "b"]);
        assert_eq!(
            tracks[1].children[0]
                .source_range()
                .map(|r| r.duration.value),
            Some(5.0)
        );
        let v3: Vec<_> = tracks[2].children.iter().map(Item::name).collect();
        assert_eq!(v3, ["", "c"]);
    }

    #[test]
    fn transitions_sit_between_abutting_items() {
        let mut g = Group::default();
        g.events.push(Event::Item {
            start: 0.0,
            dur: 10.0,
            item: clip("a"),
        });
        g.events.push(Event::Item {
            start: 10.0,
            dur: 10.0,
            item: clip("b"),
        });
        g.events.push(Event::Transition {
            cut: 10.0,
            transition: Transition::dissolve(rt(2.0, 24.0), rt(2.0, 24.0)),
        });
        let items = build_track(vec![g], 24.0);
        let kinds: Vec<_> = items
            .iter()
            .map(|i| match i {
                Item::Transition(_) => "T",
                _ => i.name(),
            })
            .collect();
        assert_eq!(kinds, ["a", "T", "b"]);
    }

    #[test]
    fn helpers() {
        assert_eq!(marker_color(0.9, 0.1, 0.1), "RED");
        assert_eq!(blend_mode_name("2 (Multiply)"), "multiply");
        assert_eq!(blend_mode_name("Soft Light"), "softlight");
        assert_eq!(blend_mode_name("Color Dodge"), "color_dodge");
        assert_eq!(blend_mode_name("darkercolour"), "darker_color");
        let k = |v: Value| keyframe(0.0, 24.0, v, Interp::Linear);
        let p = transform_params(
            &json!([k(json!([0.1, 0.2]))]),
            &json!([1.5, 2.0]),
            &json!(30.0),
            &Value::Null,
        );
        assert_eq!(p["translate_y"][0]["v"], json!(0.2));
        assert_eq!(p["scale_y"], json!(2.0));
        assert_eq!(p["rotate_deg"], json!(-30.0));
        assert!((linear_to_db(0.5) + 6.0206).abs() < 1e-3);
    }
}
