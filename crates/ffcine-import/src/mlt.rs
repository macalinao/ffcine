//! MLT XML reader: Kdenlive `.kdenlive` and Shotcut `.mlt` projects (and anything `melt` writes),
//! straight from the XML. The OTIO Kdenlive adapter ignores `timewarp`, transforms, titles, LUTs
//! and wipe types (`docs/import.md`).
//!
//! What maps (fields and units: [`common`](super::common)):
//! - The main tractor's tracks (Kdenlive's two-playlist track tractors and sequences included;
//!   the black / background track is skipped), playlist entries and blanks -> `Track`s, `Clip`s
//!   and `Gap`s. Clips on a nested tractor or playlist -> nested `Stack`.
//! - Media: file producers -> `ExternalReference`; `color` -> `ffcine.solid` generator;
//!   `kdenlivetitle`, and Shotcut text (`dynamictext` / `qtext` on a colour clip) -> `ffcine.text`;
//!   `timewarp` -> the file + `LinearTimeWarp(warp_speed)`; the `timeremap` link ->
//!   `ffcine.time.ramp/1` + average `LinearTimeWarp`.
//! - Same-track mixes (Kdenlive `kdenlive:mixcut`, Shotcut / `melt -mix` transition tractors) ->
//!   `Transition(SMPTE_Dissolve)`; a `luma` with a wipe file also gets
//!   `ffcine.transition/1 {kind: "wipe", resource}` flat in its `metadata.ffcine`.
//! - Filters: `affine` / `qtblend` rect -> `ffcine.geo.transform/1` + opacity in
//!   `ffcine.comp.blend/1`; `crop` -> `ffcine.geo.crop/1`; `volume` (`level` dB, `gain`) ->
//!   `ffcine.audio.level/1 {gain_db}`, Kdenlive `fadein`/`fadeout` -> `{fade_in, fade_out}`; `fade_from_black`/`fade_to_black`,
//!   Shotcut brightness fades and `brightnessOpacity` -> opacity; `avfilter.lut3d` ->
//!   `ffcine.color.grade/1 {lut}`.
//! - Markers: Kdenlive bin-clip `kdenlive:markers` and timeline guides, Shotcut `shotcut:markers`.
//!
//! Keyframe strings (`time<op>=value;...`): no op is linear, `|`/`!` hold, everything else
//! (`~`, `$`, `-`, the `a`–`D` easings) ease. Rects are `"x y w h opacity"` in pixels or `%` of
//! the profile, or the legacy `"x/y:wxh:opacity%"`. Everything not mapped is kept under
//! `metadata.ffcine.foreign.mlt` on the object it belongs to.

#![allow(clippy::many_single_char_names)] // maths on x, y, w, h, t, v

mod map;

use std::collections::HashMap;
use std::path::Path;

use super::common::Interp;
use super::xml::Element;
use super::{ImportOptions, Imported};
use ffcine_core::{Error, Result};

/// Reads a `.kdenlive` or `.mlt` file.
pub fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let src = std::fs::read_to_string(input).map_err(|source| Error::Io {
        path: input.to_owned(),
        source,
    })?;
    let mut doc = Doc::parse(&src)?;
    if doc.root.is_none() {
        doc.root = input
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| !p.is_empty());
    }
    map::map(&doc, opts)
}

/// Reads MLT XML from a string; relative paths resolve against the document's `root`.
pub fn import_str(src: &str, opts: &ImportOptions) -> Result<Imported> {
    let doc = Doc::parse(src)?;
    map::map(&doc, opts)
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid {
        what: "MLT XML",
        value: msg.into(),
    }
}

/// `<profile>`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Profile {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
}

/// A producer, chain, playlist, tractor, filter, transition or link: its element, id and
/// `<property>`s.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub el: Element,
    pub props: Vec<(String, String)>,
}

impl Node {
    pub fn new(el: &Element) -> Self {
        let props = el
            .children_named("property")
            .filter_map(|p| Some((p.attr("name")?.to_owned(), p.text.clone())))
            .collect();
        Self {
            el: el.clone(),
            props,
        }
    }

    pub fn tag(&self) -> &str {
        &self.el.name
    }

    pub fn id(&self) -> &str {
        self.el.attr("id").unwrap_or_default()
    }

    pub fn prop(&self, name: &str) -> Option<&str> {
        self.props
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn service(&self) -> &str {
        self.prop("mlt_service").unwrap_or_default()
    }

    /// Child `filter`s (or `transition`s, `link`s) as nodes.
    pub fn children(&self, tag: &str) -> Vec<Node> {
        self.el.children_named(tag).map(Node::new).collect()
    }
}

/// An MLT document: the profile and every named service, in document order.
#[derive(Debug, Clone)]
pub(crate) struct Doc {
    pub profile: Option<Profile>,
    pub root: Option<String>,
    pub mlt: Element,
    pub services: HashMap<String, Node>,
    pub order: Vec<String>,
    /// Ids of the services defined directly under `<mlt>`, in order.
    pub top: Vec<String>,
}

const SERVICES: &[&str] = &["producer", "chain", "playlist", "tractor", "multitrack"];

impl Doc {
    pub fn parse(src: &str) -> Result<Self> {
        let root = Element::parse(src).map_err(|e| invalid(e.to_string()))?;
        if root.name != "mlt" {
            return Err(invalid(format!("root element is <{}>", root.name)));
        }
        let profile = root.child("profile").map(|p| {
            let num: Option<f64> = p.attr("frame_rate_num").and_then(|v| v.parse().ok());
            let den: Option<f64> = p.attr("frame_rate_den").and_then(|v| v.parse().ok());
            Profile {
                width: p.attr("width").and_then(|v| v.parse().ok()),
                height: p.attr("height").and_then(|v| v.parse().ok()),
                fps: match (num, den) {
                    (Some(n), Some(d)) if d > 0.0 => Some(n / d),
                    _ => None,
                },
            }
        });
        let mut doc = Doc {
            profile,
            root: root.attr("root").map(str::to_owned),
            mlt: Element {
                name: root.name.clone(),
                attrs: root.attrs.clone(),
                children: root
                    .children
                    .iter()
                    .filter(|c| c.name == "profile")
                    .cloned()
                    .collect(),
                text: String::new(),
            },
            services: HashMap::new(),
            order: Vec::new(),
            top: root
                .children
                .iter()
                .filter(|c| SERVICES.contains(&c.name.as_str()))
                .filter_map(|c| c.attr("id").map(str::to_owned))
                .collect(),
        };
        doc.collect(&root);
        Ok(doc)
    }

    /// Registers every service with an id, including ones defined inline.
    fn collect(&mut self, e: &Element) {
        for c in &e.children {
            if SERVICES.contains(&c.name.as_str()) {
                if let Some(id) = c.attr("id") {
                    self.order.push(id.to_owned());
                    self.services.insert(id.to_owned(), Node::new(c));
                }
            }
            self.collect(c);
        }
    }

    pub fn get(&self, id: &str) -> Option<&Node> {
        self.services.get(id)
    }
}

/// An MLT time value in frames: a frame count, `hh:mm:ss.mmm` / `mm:ss.mmm` (clock) or
/// `hh:mm:ss:ff` / `hh:mm:ss;ff` (SMPTE; `;` drop-frame treated as non-drop).
pub(crate) fn parse_time(s: &str, fps: f64) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let parts: Vec<&str> = s.split([':', ';']).collect();
    let frames = match parts.len() {
        1 => s.parse::<f64>().ok()?,
        2 | 3 => {
            let mut secs = 0.0;
            for p in &parts {
                secs = secs * 60.0 + p.parse::<f64>().ok()?;
            }
            (secs * fps).round()
        }
        4 => {
            let h: f64 = parts[0].parse().ok()?;
            let m: f64 = parts[1].parse().ok()?;
            let sec: f64 = parts[2].parse().ok()?;
            let ff: f64 = parts[3].parse().ok()?;
            (h * 3600.0 + m * 60.0 + sec) * fps.round() + ff
        }
        _ => return None,
    };
    Some(if neg { -frames } else { frames })
}

/// One keyframe of an MLT animation string.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Key {
    /// Frames from the animation's start; negative counts back from its end.
    pub pos: f64,
    pub interp: Interp,
    /// The raw operator character, if any (kept for the approximation warning).
    pub op: Option<char>,
    pub value: String,
}

/// Parses `time<op>=value;...`. `None` if `s` isn't animated (no `=`).
pub(crate) fn parse_anim(s: &str, fps: f64) -> Option<Vec<Key>> {
    if !s.contains('=') {
        return None;
    }
    let mut keys = Vec::new();
    for item in s.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (t, v) = item.split_once('=')?;
        let t = t.trim();
        let (t, op) = match t.chars().last() {
            Some(c) if c == '|' || c == '!' || c == '~' || c == '$' || c == '-' => {
                (&t[..t.len() - 1], Some(c))
            }
            Some(c) if c.is_ascii_alphabetic() => (&t[..t.len() - 1], Some(c)),
            _ => (t, None),
        };
        let interp = match op {
            None => Interp::Linear,
            Some('|' | '!') => Interp::Hold,
            Some(_) => Interp::Ease,
        };
        keys.push(Key {
            pos: parse_time(t, fps)?,
            interp,
            op,
            value: v.to_owned(),
        });
    }
    Some(keys)
}

/// An MLT rect in pixels: `[x, y, w, h, opacity]` (opacity 0–1, default 1). Accepts
/// `"x y w h [o]"` with optional `%` per component, and the legacy `"x/y:wxh[:o%]"`.
pub(crate) fn parse_rect(s: &str, width: f64, height: f64) -> Option<[f64; 5]> {
    let s = s.trim();
    let legacy = s.contains('/') || s.contains(':') || s.contains('x');
    let parts: Vec<&str> = if legacy {
        s.split(['/', ':', 'x']).collect()
    } else {
        s.split_whitespace().collect()
    };
    if parts.len() < 4 {
        return None;
    }
    let mut out = [0.0, 0.0, 0.0, 0.0, 1.0];
    let dims = [width, height, width, height];
    for (i, p) in parts.iter().take(5).enumerate() {
        let p = p.trim();
        let (num, pct) = match p.strip_suffix('%') {
            Some(n) => (n.parse::<f64>().ok()?, true),
            None => (p.parse::<f64>().ok()?, false),
        };
        out[i] = if i == 4 {
            if legacy || pct { num / 100.0 } else { num }
        } else if pct {
            num / 100.0 * dims[i]
        } else {
            num
        };
    }
    Some(out)
}

/// An MLT colour (`0xRRGGBBAA`, `#RRGGBB`, `#AARRGGBB`, or a few names) as `[r, g, b, a]` 0–1.
pub(crate) fn parse_colour(s: &str) -> Option<[f64; 4]> {
    let s = s.trim();
    let byte = |h: &str| u8::from_str_radix(h, 16).ok().map(|b| f64::from(b) / 255.0);
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return match h.len() {
            8 => Some([
                byte(&h[0..2])?,
                byte(&h[2..4])?,
                byte(&h[4..6])?,
                byte(&h[6..8])?,
            ]),
            6 => Some([byte(&h[0..2])?, byte(&h[2..4])?, byte(&h[4..6])?, 1.0]),
            _ => None,
        };
    }
    if let Some(h) = s.strip_prefix('#') {
        return match h.len() {
            8 => Some([
                byte(&h[2..4])?,
                byte(&h[4..6])?,
                byte(&h[6..8])?,
                byte(&h[0..2])?,
            ]),
            6 => Some([byte(&h[0..2])?, byte(&h[2..4])?, byte(&h[4..6])?, 1.0]),
            _ => None,
        };
    }
    if s.contains(',') {
        let v: Vec<f64> = s
            .split(',')
            .filter_map(|x| x.trim().parse::<f64>().ok())
            .map(|x| x / 255.0)
            .collect();
        return match v[..] {
            [r, g, b, a] => Some([r, g, b, a]),
            [r, g, b] => Some([r, g, b, 1.0]),
            _ => None,
        };
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "black" => [0.0, 0.0, 0.0, 1.0],
        "white" => [1.0, 1.0, 1.0, 1.0],
        "red" => [1.0, 0.0, 0.0, 1.0],
        "green" => [0.0, 1.0, 0.0, 1.0],
        "blue" => [0.0, 0.0, 1.0, 1.0],
        "transparent" => [0.0, 0.0, 0.0, 0.0],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("125", 25.0), Some(125.0));
        assert_eq!(parse_time("00:00:01.000", 25.0), Some(25.0));
        assert_eq!(parse_time("00:02:07.120", 25.0), Some(3178.0));
        assert_eq!(parse_time("01:00.500", 30.0), Some(1815.0));
        assert_eq!(parse_time("00:00:01:05", 25.0), Some(30.0));
        assert_eq!(parse_time("00:00:01;05", 30000.0 / 1001.0), Some(35.0));
        assert_eq!(parse_time("-5", 25.0), Some(-5.0));
        assert_eq!(parse_time("", 25.0), None);
        assert_eq!(parse_time("abc", 25.0), None);
    }

    #[test]
    fn animations() {
        assert_eq!(parse_anim("0.5", 25.0), None);
        let k = parse_anim("0=0;24|=1; 00:00:02.000~=0.5;-1a=0.25", 25.0).expect("anim");
        let got: Vec<_> = k
            .iter()
            .map(|k| (k.pos, k.interp, k.value.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (0.0, Interp::Linear, "0"),
                (24.0, Interp::Hold, "1"),
                (50.0, Interp::Ease, "0.5"),
                (-1.0, Interp::Ease, "0.25"),
            ]
        );
        assert_eq!(k[3].op, Some('a'));
        let r = parse_anim("0=0 0 1920 1080 1;50-=10% 10% 50% 50% 0.5", 25.0).expect("rect");
        assert_eq!(r[1].op, Some('-'));
        assert_eq!(r[1].value, "10% 10% 50% 50% 0.5");
    }

    #[test]
    fn rects() {
        assert_eq!(
            parse_rect("10% 10% 50% 50% 0.8", 1920.0, 1080.0),
            Some([192.0, 108.0, 960.0, 540.0, 0.8])
        );
        assert_eq!(
            parse_rect("0 0 1920 1080", 1920.0, 1080.0),
            Some([0.0, 0.0, 1920.0, 1080.0, 1.0])
        );
        assert_eq!(
            parse_rect("10%/10%:50%x50%:80", 1920.0, 1080.0),
            Some([192.0, 108.0, 960.0, 540.0, 0.8])
        );
        assert_eq!(parse_rect("1 2", 1920.0, 1080.0), None);
    }

    #[test]
    fn colours() {
        assert_eq!(
            parse_colour("0xff000080"),
            Some([1.0, 0.0, 0.0, 128.0 / 255.0])
        );
        assert_eq!(
            parse_colour("#80ff0000"),
            Some([1.0, 0.0, 0.0, 128.0 / 255.0])
        );
        assert_eq!(parse_colour("#00ff00"), Some([0.0, 1.0, 0.0, 1.0]));
        assert_eq!(parse_colour("255,0,0,255"), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_colour("black"), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_colour("nope"), None);
    }

    #[test]
    fn doc_registers_nested_services() {
        let d = Doc::parse(
            r#"<mlt root="/p"><profile width="1920" height="1080" frame_rate_num="30000"
                frame_rate_den="1001"/>
              <tractor id="t"><multitrack><playlist id="pl"><entry producer="p0"/></playlist>
              </multitrack></tractor>
              <producer id="p0"><property name="resource">a.mp4</property>
                <property name="mlt_service">avformat</property></producer></mlt>"#,
        )
        .expect("parse");
        let p = d.profile.clone().unwrap_or_default();
        assert!((p.fps.unwrap_or_default() - 29.97).abs() < 1e-2);
        assert_eq!(d.order, ["t", "pl", "p0"]);
        assert_eq!(d.get("p0").map(Node::service), Some("avformat"));
        assert_eq!(d.root.as_deref(), Some("/p"));
    }
}
