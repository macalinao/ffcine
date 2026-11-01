//! Delivery extras generated from a timeline (`docs/delivery.md`): YouTube chapter text from
//! markers ([`chapters`]), music credits from music clips' metadata ([`credits`]), and the
//! source frame under a timeline time for stills ([`frame_at`]).

#![allow(clippy::doc_markdown)] // "YouTube"

use std::fmt::Write as _;
use std::path::PathBuf;

use ffcine_core::{Error, Result};
use ffcine_timeline::{
    Clip, Effect, HasMetadata, Item, MediaReference, Placed, Timeline, track_kind,
};
use serde_json::Value;

/// `167`, `2:47`, `1:02:47.5` -> seconds.
pub fn parse_time(s: &str) -> Result<f64> {
    let invalid = || Error::Invalid {
        what: "time (S, M:SS or H:MM:SS)",
        value: s.to_owned(),
    };
    let mut t = 0.0;
    for (i, part) in s.trim().split(':').enumerate() {
        if i > 2 {
            return Err(invalid());
        }
        let v: f64 = part.parse().map_err(|_| invalid())?;
        if !(v.is_finite() && v >= 0.0) {
            return Err(invalid());
        }
        t = t * 60.0 + v;
    }
    Ok(t)
}

/// Seconds as YouTube writes them: `M:SS`, or `H:MM:SS` when `long`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn timestamp(seconds: f64, long: bool) -> String {
    let s = seconds.max(0.0).floor() as u64;
    if long {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start: f64,
    pub title: String,
}

/// Chapters and what YouTube would reject about them.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapters {
    pub chapters: Vec<Chapter>,
    pub duration: f64,
    pub warnings: Vec<String>,
}

impl Chapters {
    /// The description block: one `0:00 Title` line per chapter.
    pub fn youtube(&self) -> String {
        let long = self.duration >= 3600.0;
        let mut s = String::new();
        for c in &self.chapters {
            let _ = writeln!(s, "{} {}", timestamp(c.start, long), c.title);
        }
        s
    }
}

fn flagged(m: &serde_json::Map<String, Value>) -> bool {
    m.get("ffcine")
        .and_then(|f| f.get("chapter"))
        .is_some_and(|v| v.as_bool().unwrap_or(true))
}

/// Chapters from the timeline's markers: the ones flagged `metadata.ffcine.chapter` (all markers
/// when none are). YouTube wants the first at 0:00, so a later first chapter gets an `intro`
/// one before it.
pub fn chapters(t: &Timeline, intro: &str) -> Chapters {
    let markers = t.placed_markers();
    let any_flagged = markers.iter().any(|m| flagged(&m.marker.metadata));
    let mut list: Vec<Chapter> = Vec::new();
    for m in markers
        .iter()
        .filter(|m| !any_flagged || flagged(&m.marker.metadata))
    {
        if list.last().is_some_and(|c| (m.start - c.start).abs() < 1.0) {
            continue;
        }
        let title = m.marker.name.trim();
        list.push(Chapter {
            start: m.start.max(0.0),
            title: if title.is_empty() {
                format!("Chapter {}", list.len() + 1)
            } else {
                title.to_owned()
            },
        });
    }
    let duration = t.duration();
    let mut warnings = Vec::new();
    match list.first_mut() {
        Some(c) if c.start < 1.0 => c.start = 0.0,
        Some(_) => list.insert(
            0,
            Chapter {
                start: 0.0,
                title: intro.to_owned(),
            },
        ),
        None => warnings.push("no markers on the timeline".to_owned()),
    }
    if !list.is_empty() && list.len() < 3 {
        warnings.push(format!("{} chapters: YouTube needs at least 3", list.len()));
    }
    let long = duration >= 3600.0;
    for (i, c) in list.iter().enumerate() {
        let end = list.get(i + 1).map_or(duration, |n| n.start);
        if end - c.start < 10.0 {
            warnings.push(format!(
                "{} {}: {:.1} s long, YouTube needs 10 s",
                timestamp(c.start, long),
                c.title,
                end - c.start
            ));
        }
    }
    Chapters {
        chapters: list,
        duration,
        warnings,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Credit {
    /// `metadata.ffcine.music` (the track's name), or the clip's name.
    pub music: String,
    pub attribution: Option<String>,
    /// Timeline seconds of its first use.
    pub first: f64,
}

/// Music credits: every enabled clip with `metadata.ffcine.music` or `attribution`, once per
/// track, in order of first use. `public` leaves out `private` clips (the public cut swaps or
/// drops them) and uses a `public_alt`'s own credit when the alt is given as an object.
pub fn credits(t: &Timeline, public: bool) -> (Vec<Credit>, Vec<String>) {
    let mut out: Vec<Credit> = Vec::new();
    let mut warnings = Vec::new();
    let mut placed = t.placed_clips();
    placed.sort_by(|a, b| a.start.total_cmp(&b.start));
    for p in placed.iter().filter(|p| p.clip.enabled) {
        let Some(ns) = p.clip.ffcine() else { continue };
        if !ns.contains_key("music") && !ns.contains_key("attribution") {
            continue;
        }
        let mut ns = ns.clone();
        if public && ns.get("private").and_then(Value::as_bool) == Some(true) {
            match ns.get("public_alt") {
                Some(Value::Object(alt)) => ns = alt.clone(),
                Some(Value::String(path)) => {
                    warnings.push(format!(
                        "{}: public cut uses {path}, which has no credit here",
                        p.clip.name
                    ));
                    continue;
                }
                _ => continue,
            }
        }
        let music = ns.get("music").and_then(Value::as_str);
        let attribution = ns
            .get("attribution")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if music.is_none() && attribution.is_none() {
            continue;
        }
        let music = music.map_or_else(|| p.clip.name.clone(), str::to_owned);
        if out.iter().any(|c| c.music == music) {
            continue;
        }
        if attribution.is_none() {
            warnings.push(format!("{music}: no attribution (CC BY tracks need one)"));
        }
        out.push(Credit {
            music,
            attribution,
            first: p.start,
        });
    }
    (out, warnings)
}

/// The credits block: one line per track, the attribution as written (else the name).
pub fn credits_text(credits: &[Credit]) -> String {
    let mut s = String::new();
    for c in credits {
        let _ = writeln!(s, "{}", c.attribution.as_deref().unwrap_or(&c.music));
    }
    s
}

/// The source frame shown at a timeline time.
#[derive(Debug, Clone)]
pub struct Frame<'a> {
    pub clip: &'a Clip,
    pub path: PathBuf,
    /// Seconds into the media file.
    pub source_time: f64,
    pub placed: Placed<'a>,
}

fn time_scalar(c: &Clip) -> f64 {
    c.effects
        .iter()
        .find_map(|e| match e {
            Effect::LinearTimeWarp(w) => Some(w.time_scalar),
            Effect::FreezeFrame(_) => Some(0.0),
            _ => None,
        })
        .unwrap_or(1.0)
}

/// The top-most enabled video clip with media at timeline second `at` (text and other
/// generators are skipped: they are drawn as overlays).
pub fn frame_at(t: &Timeline, at: f64) -> Option<Frame<'_>> {
    let enabled = |i: usize| match t.tracks.children.get(i) {
        Some(Item::Track(tr)) => tr.enabled,
        _ => true,
    };
    t.placed_clips()
        .into_iter()
        .rev()
        .filter(|p| p.kind == track_kind::VIDEO && p.clip.enabled && enabled(p.track))
        .filter(|p| p.start <= at && at < p.end())
        .find_map(|p| {
            let MediaReference::External(r) = p.clip.media_reference()? else {
                return None;
            };
            let in_point = p
                .clip
                .source_range
                .as_ref()
                .map_or(0.0, |r| r.start_time.to_seconds());
            Some(Frame {
                clip: p.clip,
                path: r.path()?,
                source_time: in_point + (at - p.start) * time_scalar(p.clip),
                placed: p,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_timeline::{
        ExternalReference, Gap, LinearTimeWarp, Marker, RationalTime, TimeRange, Track, track_kind,
    };
    use serde_json::json;

    #[allow(clippy::many_single_char_names)]
    fn timeline() -> Timeline {
        let mut v = Track::new("V1", track_kind::VIDEO);
        let media =
            |p: &str| MediaReference::External(ExternalReference::from_path(p.as_ref(), None));
        let mut a = Clip::new(
            "a",
            media("/m/a.mov"),
            Some(TimeRange::from_frames(240.0, 2640.0, 24.0)),
        );
        let mut m = Marker::new(
            "Old Town",
            TimeRange::from_frames(1440.0, 1440.0, 24.0),
            "RED",
        );
        m.metadata.insert("ffcine".into(), json!({"chapter": true}));
        a.markers.push(m);
        let mut b = Clip::new(
            "b",
            media("/m/b.mov"),
            Some(TimeRange::from_frames(0.0, 960.0, 24.0)),
        );
        b.effects
            .push(Effect::LinearTimeWarp(LinearTimeWarp::new(2.0)));
        v.children.push(Item::Clip(a));
        v.children.push(Item::Clip(b));
        let mut music = Track::new("Music", track_kind::AUDIO);
        for (name, attr, private) in [
            (
                "Carefree",
                Some("\"Carefree\" Kevin MacLeod, CC BY 4.0"),
                false,
            ),
            ("Meme", None, true),
            (
                "Carefree",
                Some("\"Carefree\" Kevin MacLeod, CC BY 4.0"),
                false,
            ),
        ] {
            let mut c = Clip::new(
                name,
                media("/m/x.wav"),
                Some(TimeRange::from_frames(0.0, 240.0, 24.0)),
            );
            let ns = c.ffcine_mut();
            ns.insert("music".into(), name.into());
            if let Some(a) = attr {
                ns.insert("attribution".into(), a.into());
            }
            if private {
                ns.insert("private".into(), true.into());
            }
            music.children.push(Item::Clip(c));
            music
                .children
                .push(Item::Gap(Gap::new(RationalTime::new(240.0, 24.0))));
        }
        let mut t = Timeline::default();
        t.tracks.children.push(Item::Track(v));
        t.tracks.children.push(Item::Track(music));
        let mut start = Marker::new("Arrival", TimeRange::from_frames(0.0, 0.0, 24.0), "RED");
        start
            .metadata
            .insert("ffcine".into(), json!({"chapter": true}));
        t.tracks.markers.push(start);
        t.tracks.markers.push(Marker::new(
            "not a chapter",
            TimeRange::from_frames(24.0, 24.0, 24.0),
            "RED",
        ));
        t
    }

    #[test]
    fn times() {
        assert!((parse_time("2:47").expect("t") - 167.0).abs() < 1e-9);
        assert!((parse_time("1:02:47.5").expect("t") - 3767.5).abs() < 1e-9);
        assert!((parse_time("12.5").expect("t") - 12.5).abs() < 1e-9);
        assert!(parse_time("1:x").is_err());
        assert_eq!(timestamp(167.9, false), "2:47");
        assert_eq!(timestamp(3767.0, true), "1:02:47");
    }

    #[test]
    fn chapters_from_flagged_markers() {
        let c = chapters(&timeline(), "Intro");
        // The clip marker at source frame 1440 is 50 s into the clip that starts at 0 (in 240).
        assert_eq!(c.youtube(), "0:00 Arrival\n0:50 Old Town\n");
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
    }

    #[test]
    fn credits_dedupe_and_public() {
        let t = timeline();
        let (c, w) = credits(&t, false);
        assert_eq!(c.len(), 2);
        assert_eq!(w.len(), 1);
        let (c, _) = credits(&t, true);
        assert_eq!(credits_text(&c), "\"Carefree\" Kevin MacLeod, CC BY 4.0\n");
    }

    #[test]
    fn frame_under_a_time() {
        let t = timeline();
        let f = frame_at(&t, 10.0).expect("frame");
        assert_eq!(f.path, PathBuf::from("/m/a.mov"));
        assert!((f.source_time - 20.0).abs() < 1e-9);
        // b starts at 100 s and plays at 2x.
        let f = frame_at(&t, 101.0).expect("frame");
        assert!((f.source_time - 2.0).abs() < 1e-9);
        assert!(frame_at(&t, 500.0).is_none());
    }
}
