//! CMX 3600 EDL importer, with ASC CDL (`*ASC_SOP` / `*ASC_SAT` comments and `.ccc` / `.cdl`
//! sidecars), the colour path out of Resolve and most finishing tools.
//!
//! An EDL has no frame rate: [`ImportOptions::rate`] sets it, else 29.97 for `FCM: DROP FRAME`
//! (or `;` timecodes) and 24 otherwise, with a warning. Record timecodes place events; the
//! first record in becomes `global_start_time`.
//!
//! | EDL | ffcine |
//! |---|---|
//! | `TITLE:`, `FCM:` | timeline name, `foreign.cmx_3600` |
//! | event `C` | `Clip`: `source_range` = source in + record length, media `* SOURCE FILE:` (else a `MissingReference` named by the reel) |
//! | track `V`, `A`, `A2`, `AA`, `B`, `AA/V` | `V1`, `A1`, `A2`, ... (one clip per channel, linked by `metadata.ffcine.link_group`) |
//! | `D nnn` (with its `C` from-line) | `Transition(SMPTE_Dissolve)` at the cut, `in_offset` 0, `out_offset` nnn (the mix runs into the incoming event, as CMX plays it) |
//! | `W### nnn` | same dissolve with `ffcine.transition/1 {kind: "wipe", smpte}` (rendered as a dissolve) |
//! | `K`, `KB`, `KO` keys | skipped ([`crate::ImportReport::skipped`]) |
//! | `M2 reel speed tc` | `LinearTimeWarp(speed / rate)`, `FreezeFrame` at 0 |
//! | `* FROM CLIP NAME:` / `* TO CLIP NAME:` | clip name |
//! | `*ASC_SOP`, `*ASC_SAT` | `ffcine.color.cdl/1` + `metadata.cdl` |
//! | `* ASC_CC_XML <id>` | the correction with that id in the sidecar |
//! | `* LOC: tc colour name` | `Marker` on the clip (source time) |
//! | reel, source timecodes | `metadata.ffcine.source = {src, kind: "reel"}`, `foreign.cmx_3600` |
//! | other comments | `foreign.cmx_3600.comments` |
//!
//! Sidecar: `<stem>.ccc` or `<stem>.cdl` next to the EDL is read automatically
//! ([`import_with_cdl`] takes an explicit one). A correction applies to the events whose
//! `ASC_CC_XML` id, clip name, reel or event number equals its id; inline `*ASC_SOP` wins.
//!
//! Clip ids are `edl-<hash>` of the track, event number, reel and timecodes, so re-imports of the
//! same EDL give the same ids.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::{ImportOptions, ImportReport, Imported};
use crate::cdl::{self, Cdl};
use crate::common::{Event, Group, Kind, Lanes, marker_color, range, rt, set_transition_kind};
use crate::xml::Ids;
use ffcine_core::{Error, Result};
use ffcine_timeline::{
    Clip, Effect, ExternalReference, FFCINE_VERSION, HasMetadata, Item, LinearTimeWarp, Marker,
    MediaReference, MissingReference, Timeline, Transition,
};

/// Key under `metadata.ffcine.foreign`.
pub const APP: &str = "cmx_3600";

/// Imports an EDL, with a sidecar `<stem>.ccc` / `<stem>.cdl` if there is one.
pub fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    import_with_cdl(input, sidecar(input).as_deref(), opts)
}

/// Imports an EDL with the corrections in `cdl` (a `.ccc`, `.cdl` or `.cc` file).
pub fn import_with_cdl(input: &Path, cdl: Option<&Path>, opts: &ImportOptions) -> Result<Imported> {
    let src = std::fs::read(input).map_err(|source| Error::Io {
        path: input.to_owned(),
        source,
    })?;
    let src = String::from_utf8_lossy(&src);
    let ccs = cdl.map(cdl::read).transpose()?.unwrap_or_default();
    let mut out = import_str(&src, &ccs, opts)?;
    if let Some(p) = cdl {
        out.timeline
            .insert_foreign(APP, "cdl_file", p.display().to_string().into());
    }
    Ok(out)
}

/// [`import_with_cdl`] on the EDL text and already-read corrections.
pub fn import_str(src: &str, ccs: &[Cdl], opts: &ImportOptions) -> Result<Imported> {
    let mut report = ImportReport::default();
    let doc = parse(src, &mut report)?;
    let df = doc.drop_frame || doc.events.iter().any(|e| e.rec_in.contains(';'));
    let rate = opts.rate.unwrap_or_else(|| {
        let r = if df { 30000.0 / 1001.0 } else { 24.0 };
        report.warn(format!(
            "EDL has no frame rate: assumed {r:.3} fps (pass a rate)"
        ));
        r
    });
    Ok(build(&doc, ccs, rate, df, report))
}

#[derive(Debug, Default)]
struct Doc {
    title: Option<String>,
    fcm: Option<String>,
    drop_frame: bool,
    events: Vec<Ev>,
    /// Lines before the first event that aren't TITLE/FCM.
    header: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Ev {
    num: String,
    reel: String,
    channels: String,
    /// `C`, `D`, `W001`, ...
    edit: String,
    trans_dur: Option<f64>,
    src_in: String,
    src_out: String,
    rec_in: String,
    rec_out: String,
    from_name: Option<String>,
    to_name: Option<String>,
    source_file: Option<String>,
    sop: Option<([f64; 3], [f64; 3], [f64; 3])>,
    sat: Option<f64>,
    cc_ref: Option<String>,
    speed: Option<f64>,
    locs: Vec<(String, String, String)>,
    comments: Vec<String>,
    line: usize,
}

fn tc_like(s: &str) -> bool {
    s.len() >= 11 && s.chars().filter(|c| matches!(c, ':' | ';' | '.')).count() == 3
}

fn parse(src: &str, report: &mut ImportReport) -> Result<Doc> {
    let mut doc = Doc::default();
    // The event comments apply to (the last event line, or the pending dissolve pair).
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim_end();
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        if let Some(v) = t.strip_prefix("TITLE:") {
            doc.title = Some(v.trim().to_owned());
            continue;
        }
        if let Some(v) = t.strip_prefix("FCM:") {
            let v = v.trim().to_owned();
            doc.drop_frame = v.to_ascii_uppercase().starts_with("DROP");
            doc.fcm = Some(v);
            continue;
        }
        if t.starts_with('*') {
            let Some(ev) = doc.events.last_mut() else {
                doc.header.push(t.to_owned());
                continue;
            };
            comment(ev, t, report);
            continue;
        }
        if let Some(rest) = t.strip_prefix("M2") {
            let f: Vec<&str> = rest.split_whitespace().collect();
            let speed = f
                .iter()
                .rev()
                .nth(1)
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|_| f.len() >= 3);
            let reel = f.first().copied().unwrap_or_default();
            match (speed, doc.events.last_mut()) {
                (Some(s), Some(ev)) => {
                    if ev.reel != reel {
                        report.warn(format!(
                            "EDL line {}: M2 reel {reel:?} differs from event {}'s {:?}; applied to it",
                            i + 1,
                            ev.num,
                            ev.reel
                        ));
                    }
                    ev.speed = Some(s);
                }
                _ => report.skip(format!("EDL line {}: unreadable M2 {t:?}", i + 1)),
            }
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        if f.first()
            .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
            && f.len() >= 8
        {
            let (edit, dur, rest) = match f[3] {
                "C" => ("C".to_owned(), None, &f[4..]),
                e => (
                    e.to_owned(),
                    f.get(4).and_then(|d| d.parse::<f64>().ok()),
                    f.get(5..).unwrap_or_default(),
                ),
            };
            if rest.len() < 4 || !rest[..4].iter().all(|s| tc_like(s)) {
                report.skip(format!("EDL line {}: unreadable event {t:?}", i + 1));
                continue;
            }
            doc.events.push(Ev {
                num: f[0].to_owned(),
                reel: f[1].to_owned(),
                channels: f[2].to_owned(),
                edit,
                trans_dur: dur,
                src_in: rest[0].to_owned(),
                src_out: rest[1].to_owned(),
                rec_in: rest[2].to_owned(),
                rec_out: rest[3].to_owned(),
                line: i + 1,
                ..Ev::default()
            });
            continue;
        }
        report.skip(format!("EDL line {}: {t:?}", i + 1));
    }
    if doc.events.is_empty() {
        return Err(Error::Invalid {
            what: "EDL (no events)",
            value: doc.title.unwrap_or_default(),
        });
    }
    Ok(doc)
}

fn comment(ev: &mut Ev, t: &str, report: &mut ImportReport) {
    let body = t.trim_start_matches('*').trim();
    let upper = body.to_ascii_uppercase();
    let after = |key: &str| {
        body[key.len()..]
            .trim_start_matches([':', ' '])
            .trim()
            .to_owned()
    };
    if upper.starts_with("FROM CLIP NAME") {
        ev.from_name = Some(after("FROM CLIP NAME"));
    } else if upper.starts_with("TO CLIP NAME") {
        ev.to_name = Some(after("TO CLIP NAME"));
    } else if upper.starts_with("SOURCE FILE") {
        ev.source_file = Some(after("SOURCE FILE"));
    } else if upper.starts_with("ASC_SOP") {
        match Cdl::parse_sop(&after("ASC_SOP")) {
            Some(sop) => ev.sop = Some(sop),
            None => report.skip(format!("EDL event {}: unreadable {t:?}", ev.num)),
        }
    } else if upper.starts_with("ASC_SAT") {
        match after("ASC_SAT").parse() {
            Ok(s) => ev.sat = Some(s),
            Err(_) => report.skip(format!("EDL event {}: unreadable {t:?}", ev.num)),
        }
    } else if upper.starts_with("ASC_CC_XML") {
        ev.cc_ref = Some(after("ASC_CC_XML"));
    } else if upper.starts_with("LOC") {
        let rest = after("LOC");
        let mut f = rest.splitn(3, char::is_whitespace);
        let tc = f.next().unwrap_or_default().to_owned();
        let colour = f.next().unwrap_or_default().trim().to_owned();
        let name = f.next().unwrap_or_default().trim().to_owned();
        if tc_like(&tc) {
            ev.locs.push((tc, colour, name));
        } else {
            report.skip(format!("EDL event {}: unreadable {t:?}", ev.num));
        }
    } else {
        ev.comments.push(t.to_owned());
    }
}

/// Timecode -> frames at `rate` (drop-frame for 29.97/59.94 when `df`).
fn tc_frames(tc: &str, rate: f64, df: bool) -> Option<f64> {
    let parts: Vec<f64> = tc
        .split([':', ';', '.'])
        .map(|p| p.parse::<f64>().ok())
        .collect::<Option<_>>()?;
    let [h, m, s, f] = parts[..] else {
        return None;
    };
    let nominal = rate.round();
    let mut frames = ((h * 60.0 + m) * 60.0 + s) * nominal + f;
    if df && ((nominal - 30.0).abs() < 0.5 || (nominal - 60.0).abs() < 0.5) {
        let drop = nominal / 15.0;
        let mins = h * 60.0 + m;
        frames -= drop * (mins - (mins / 10.0).floor());
    }
    Some(frames)
}

/// Channels of an event: `(Kind, channel)`, video channel 1.
fn channels(c: &str) -> Vec<(Kind, i32)> {
    let mut out = Vec::new();
    for part in c.split('/') {
        match part {
            "V" => out.push((Kind::Video, 1)),
            "A" | "A1" => out.push((Kind::Audio, 1)),
            "AA" => out.extend([(Kind::Audio, 1), (Kind::Audio, 2)]),
            "B" => out.extend([(Kind::Video, 1), (Kind::Audio, 1)]),
            p if p.starts_with('A') => {
                if let Ok(n) = p[1..].parse() {
                    out.push((Kind::Audio, n));
                }
            }
            _ => {}
        }
    }
    out
}

#[allow(clippy::too_many_lines)]
fn build(doc: &Doc, ccs: &[Cdl], rate: f64, df: bool, mut report: ImportReport) -> Imported {
    let tc = |s: &str| tc_frames(s, rate, df);
    let origin = doc
        .events
        .iter()
        .filter_map(|e| tc(&e.rec_in))
        .fold(f64::INFINITY, f64::min);
    let by_id: BTreeMap<&str, &Cdl> = ccs
        .iter()
        .filter_map(|c| c.id.as_deref().map(|id| (id, c)))
        .collect();
    let mut used_ccs = 0usize;
    let mut groups: BTreeMap<(Kind, i32), Group> = BTreeMap::new();
    let mut ids = Ids::default();
    for (k, ev) in doc.events.iter().enumerate() {
        let (Some(si), Some(so), Some(ri), Some(ro)) = (
            tc(&ev.src_in),
            tc(&ev.src_out),
            tc(&ev.rec_in),
            tc(&ev.rec_out),
        ) else {
            report.skip(format!("EDL line {}: bad timecode", ev.line));
            continue;
        };
        let edit = ev.edit.to_ascii_uppercase();
        if edit.starts_with('K') {
            report.skip(format!(
                "EDL event {}: key ({}) not supported",
                ev.num, ev.edit
            ));
            continue;
        }
        let chans = channels(&ev.channels);
        if chans.is_empty() {
            report.skip(format!(
                "EDL event {}: unknown track {:?}",
                ev.num, ev.channels
            ));
            continue;
        }
        let dur = ro - ri;
        // The `C` from-line of a dissolve pair: zero length, the outgoing clip is the event before.
        let next_is_mix = doc
            .events
            .get(k + 1)
            .is_some_and(|n| n.num == ev.num && n.edit != "C");
        if dur <= 0.0 {
            if !next_is_mix {
                report.warn(format!("EDL event {}: zero length, dropped", ev.num));
            }
            continue;
        }
        let transition = match edit.as_str() {
            "C" => None,
            "D" => Some(None),
            e if e.starts_with('W') => Some(Some(e[1..].to_owned())),
            _ => {
                report.skip(format!("EDL event {}: unknown edit {:?}", ev.num, ev.edit));
                continue;
            }
        };
        let name = if transition.is_some() {
            ev.to_name.clone().or_else(|| ev.from_name.clone())
        } else {
            ev.from_name.clone()
        }
        .unwrap_or_else(|| ev.reel.clone());
        let media = match &ev.source_file {
            Some(f) if f.contains("://") => {
                MediaReference::External(ExternalReference::new(f.clone(), None))
            }
            Some(f) => MediaReference::External(ExternalReference::from_path(Path::new(f), None)),
            None => MediaReference::Missing(MissingReference {
                name: ev.reel.clone(),
                ..MissingReference::default()
            }),
        };
        let speed = ev.speed.map(|s| s / rate);
        if let Some(s) = speed {
            let m2 = (so - si) / dur;
            if (s.abs() - m2).abs() > 0.05 && s != 0.0 {
                report.warn(format!(
                    "EDL event {}: M2 speed {s:.3}x differs from the source/record ratio {m2:.3}",
                    ev.num
                ));
            }
        }
        // CDL: inline, else the sidecar by ASC_CC_XML id, clip name, reel or event number.
        let cdl = ev
            .sop
            .map(|(slope, offset, power)| Cdl {
                slope,
                offset,
                power,
                saturation: ev.sat,
                ..Cdl::default()
            })
            .or_else(|| {
                let keys = [
                    ev.cc_ref.as_deref(),
                    Some(name.as_str()),
                    Some(ev.reel.as_str()),
                    Some(ev.num.as_str()),
                ];
                keys.iter()
                    .flatten()
                    .find_map(|k| by_id.get(k).copied().cloned())
                    .inspect(|_| used_ccs += 1)
            });
        if ev.cc_ref.is_some() && ev.sop.is_none() && cdl.is_none() {
            report.warn(format!(
                "EDL event {}: ASC_CC_XML {:?} not in the CDL file",
                ev.num,
                ev.cc_ref.as_deref().unwrap_or_default()
            ));
        }
        let link = (chans.len() > 1).then(|| format!("edl-{}", ev.num));
        for (kind, ch) in chans {
            let mut clip = Clip::new(name.clone(), media.clone(), Some(range(si, dur, rate)));
            let key = format!(
                "{kind:?}{ch}/{}/{}/{}/{}",
                ev.num, ev.reel, ev.src_in, ev.rec_in
            );
            clip.set_ffcine_id(ids.make("edl", &key));
            clip.ffcine_mut()
                .insert("source".into(), json!({"src": ev.reel, "kind": "reel"}));
            if let Some(l) = &link {
                clip.ffcine_mut()
                    .insert("link_group".into(), l.clone().into());
            }
            match speed {
                Some(s) if s.abs() < 1e-12 => clip
                    .effects
                    .push(Effect::LinearTimeWarp(LinearTimeWarp::freeze_frame())),
                Some(s) if (s - 1.0).abs() > 1e-9 => clip
                    .effects
                    .push(Effect::LinearTimeWarp(LinearTimeWarp::new(s))),
                _ => {}
            }
            if kind == Kind::Video
                && let Some(c) = &cdl
            {
                c.apply(&mut clip);
            }
            for (t, colour, mname) in &ev.locs {
                if let Some(f) = tc(t) {
                    let colour = match colour.to_ascii_uppercase().as_str() {
                        c @ ("RED" | "PINK" | "ORANGE" | "YELLOW" | "GREEN" | "CYAN" | "BLUE"
                        | "PURPLE" | "MAGENTA" | "BLACK" | "WHITE") => c.to_owned(),
                        _ => marker_color(1.0, 0.0, 0.0).to_owned(),
                    };
                    clip.markers.push(Marker::new(
                        mname.clone(),
                        range(si + (f - ri), 0.0, rate),
                        &colour,
                    ));
                }
            }
            let mut foreign = Map::new();
            foreign.insert("event".into(), ev.num.clone().into());
            foreign.insert("reel".into(), ev.reel.clone().into());
            foreign.insert("track".into(), ev.channels.clone().into());
            foreign.insert("edit".into(), ev.edit.clone().into());
            foreign.insert(
                "timecode".into(),
                json!([ev.src_in, ev.src_out, ev.rec_in, ev.rec_out]),
            );
            if let Some(s) = ev.speed {
                foreign.insert("m2_speed".into(), s.into());
            }
            if !ev.comments.is_empty() {
                foreign.insert("comments".into(), ev.comments.clone().into());
            }
            if let Some(n) = &ev.from_name
                && transition.is_some()
            {
                foreign.insert("from_clip_name".into(), n.clone().into());
            }
            clip.set_foreign(APP, Value::Object(foreign));

            let g = groups.entry((kind, ch)).or_default();
            if let Some(wipe) = &transition {
                let d = ev.trans_dur.unwrap_or(0.0).min(dur);
                let mut t = Transition::dissolve(rt(0.0, rate), rt(d, rate));
                if let Some(code) = wipe {
                    t.name = format!("W{code}");
                    report.warn(format!(
                        "EDL event {}: wipe W{code} rendered as a dissolve (code kept)",
                        ev.num
                    ));
                    set_transition_kind(&mut t, json!({"kind": "wipe", "smpte": code}));
                }
                g.events.push(Event::Transition {
                    cut: ri - origin,
                    transition: t,
                });
            }
            g.events.push(Event::Item {
                start: ri - origin,
                dur,
                item: Item::Clip(clip),
            });
        }
    }
    if !ccs.is_empty() && used_ccs == 0 {
        report.warn("no CDL in the sidecar file matched an event");
    }
    let mut lanes = Lanes::default();
    for ((kind, ch), mut g) in groups {
        // Events that overlap on one channel (bad EDLs) go to their own track.
        let mut spill = Group::default();
        let mut end = f64::NEG_INFINITY;
        g.events.retain(|e| match e {
            Event::Item { start, .. } if *start < end - 1e-6 => {
                spill.events.push(e.clone());
                false
            }
            Event::Item { start, dur, .. } => {
                end = start + dur;
                true
            }
            Event::Transition { .. } => true,
        });
        let lane = match kind {
            Kind::Video => ch - 1,
            Kind::Audio => -ch,
        };
        lanes.add(kind, lane, g);
        for e in spill.events {
            report.warn("EDL events overlap on one track: moved to another track");
            if let Event::Item { start, dur, item } = e {
                lanes.add(kind, lane, Group::single(start, dur, item));
            }
        }
    }
    let mut tl = Timeline::new(doc.title.clone().unwrap_or_default());
    tl.global_start_time = Some(rt(origin, rate));
    tl.tracks.children = lanes
        .into_tracks(rate)
        .into_iter()
        .map(Item::Track)
        .collect();
    tl.ffcine_mut().insert("v".into(), FFCINE_VERSION.into());
    let mut foreign = Map::new();
    if let Some(f) = &doc.fcm {
        foreign.insert("fcm".into(), f.clone().into());
    }
    if !doc.header.is_empty() {
        foreign.insert("comments".into(), doc.header.clone().into());
    }
    if !foreign.is_empty() {
        tl.set_foreign(APP, Value::Object(foreign));
    }
    Imported {
        timeline: tl,
        report,
    }
}

/// Sidecar CDL path next to an EDL, if any (`.ccc`, `.cdl`, `.cc`).
pub fn sidecar(edl: &Path) -> Option<PathBuf> {
    ["ccc", "cdl", "cc"]
        .iter()
        .map(|e| edl.with_extension(e))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecodes() {
        assert_eq!(tc_frames("01:00:00:00", 24.0, false), Some(86_400.0));
        assert_eq!(tc_frames("00:00:01:12", 25.0, false), Some(37.0));
        // Drop frame: 00:01:00;02 is frame 1800 at 29.97.
        assert_eq!(
            tc_frames("00:01:00;02", 30000.0 / 1001.0, true),
            Some(1800.0)
        );
        assert_eq!(
            tc_frames("00:10:00;00", 30000.0 / 1001.0, true),
            Some(17_982.0)
        );
        assert_eq!(tc_frames("bad", 24.0, false), None);
    }

    #[test]
    fn channel_codes() {
        assert_eq!(channels("V"), [(Kind::Video, 1)]);
        assert_eq!(
            channels("AA/V"),
            [(Kind::Audio, 1), (Kind::Audio, 2), (Kind::Video, 1)]
        );
        assert_eq!(channels("A3"), [(Kind::Audio, 3)]);
        assert_eq!(channels("X"), Vec::new());
    }
}
