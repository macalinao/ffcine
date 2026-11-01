//! `.ass` / `.ssa` subtitles -> `ffcine.text` clips, so existing subtitle files render
//! through the same GPU text path as cards.
//!
//! Each ASS style becomes `project.styles."ass:<Name>"` (font, size, colours, bold/italic,
//! alignment and margins, outline or box, shadow); each `Dialogue` becomes a text clip on a
//! `Subtitles` track (overlapping lines spill onto `Subtitles 2`, ...). Line-wide overrides at
//! the start of a line are kept: `\pos`, `\an`, `\fad`, `\b`, `\i`. Other override tags
//! (karaoke, per-span colours, transforms, drawing) are dropped and counted in the report.

use ffcine_core::{Error, Result};
use ffcine_timeline::{
    Clip, Gap, GeneratorReference, HasMetadata, Item, MediaReference, Project, RationalTime,
    TimeRange, Timeline, Track, track_kind,
};
use serde_json::{Map, Value, json};

use crate::GENERATOR;

/// A parsed `[V4+ Styles]` entry, fields by name.
#[derive(Debug, Clone, PartialEq)]
pub struct AssStyle {
    pub name: String,
    pub fields: Map<String, Value>,
}

/// A `Dialogue` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub start: f64,
    pub end: f64,
    pub style: String,
    /// Raw text with override tags.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ass {
    pub play_res: [f64; 2],
    pub styles: Vec<AssStyle>,
    pub events: Vec<Event>,
}

/// What [`import`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub events: usize,
    pub styles: usize,
    pub tracks: usize,
    /// Override tags that aren't rendered (dropped).
    pub dropped_tags: usize,
}

fn invalid(value: impl Into<String>) -> Error {
    Error::Invalid {
        what: "ASS subtitles",
        value: value.into(),
    }
}

/// `H:MM:SS.cc` -> seconds.
fn time(s: &str) -> Result<f64> {
    let mut parts = s.trim().split(':').rev();
    let sec: f64 = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| invalid(s))?;
    let min: f64 = parts
        .next()
        .map_or(Ok(0.0), |v| v.parse().map_err(|_| invalid(s)))?;
    let hour: f64 = parts
        .next()
        .map_or(Ok(0.0), |v| v.parse().map_err(|_| invalid(s)))?;
    Ok(hour * 3600.0 + min * 60.0 + sec)
}

/// Parses an `.ass` file's text.
pub fn parse(src: &str) -> Result<Ass> {
    let mut section = String::new();
    let mut play_res = [0.0, 0.0];
    let mut style_fmt: Vec<String> = Vec::new();
    let mut event_fmt: Vec<String> = Vec::new();
    let (mut styles, mut events) = (Vec::new(), Vec::new());
    for line in src.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match (section.as_str(), key.trim()) {
            ("script info", "PlayResX") => play_res[0] = value.parse().unwrap_or(0.0),
            ("script info", "PlayResY") => play_res[1] = value.parse().unwrap_or(0.0),
            (s, "Format") if s.contains("styles") => {
                style_fmt = value.split(',').map(|f| f.trim().to_owned()).collect();
            }
            ("events", "Format") => {
                event_fmt = value.split(',').map(|f| f.trim().to_owned()).collect();
            }
            (s, "Style") if s.contains("styles") => {
                let vals = value.splitn(style_fmt.len().max(1), ',');
                let fields: Map<String, Value> = style_fmt
                    .iter()
                    .zip(vals)
                    .map(|(k, v)| (k.clone(), Value::String(v.trim().to_owned())))
                    .collect();
                let name = fields
                    .get("Name")
                    .and_then(Value::as_str)
                    .unwrap_or("Default")
                    .to_owned();
                styles.push(AssStyle { name, fields });
            }
            ("events", "Dialogue") => {
                if event_fmt.is_empty() {
                    return Err(invalid("Dialogue before the [Events] Format line"));
                }
                // Text is last and may contain commas.
                let vals: Vec<&str> = value.splitn(event_fmt.len(), ',').collect();
                let get = |name: &str| {
                    event_fmt
                        .iter()
                        .position(|f| f == name)
                        .and_then(|i| vals.get(i))
                        .copied()
                        .unwrap_or("")
                };
                events.push(Event {
                    start: time(get("Start"))?,
                    end: time(get("End"))?,
                    style: get("Style").trim().trim_start_matches('*').to_owned(),
                    text: get("Text").to_owned(),
                });
            }
            _ => {}
        }
    }
    if play_res == [0.0, 0.0] {
        // libass's defaults when the script has none.
        play_res = [384.0, 288.0];
    } else if play_res[0] == 0.0 {
        play_res[0] = play_res[1] * 4.0 / 3.0;
    } else if play_res[1] == 0.0 {
        play_res[1] = play_res[0] * 3.0 / 4.0;
    }
    Ok(Ass {
        play_res,
        styles,
        events,
    })
}

/// `&HAABBGGRR` (alpha 0 = opaque) -> `[r, g, b, a]`.
fn ass_colour(s: &str) -> Option<[f64; 4]> {
    let h = s
        .trim()
        .trim_start_matches("&H")
        .trim_start_matches("&h")
        .trim_end_matches('&');
    let v = u32::from_str_radix(h, 16).ok()?;
    let b = |shift: u32| f64::from((v >> shift) & 0xff) / 255.0;
    Some([b(0), b(8), b(16), 1.0 - b(24)])
}

/// An ASS alignment number, 1..9.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn numpad(v: f64) -> u8 {
    v.clamp(1.0, 9.0) as u8
}

/// Placement for numpad alignment `an` with margins, in [`crate::TextStyle`] terms.
#[allow(clippy::many_single_char_names)]
fn placement(an: u8, ml: f64, mr: f64, mv: f64, [w, h]: [f64; 2]) -> Map<String, Value> {
    let col = (an.clamp(1, 9) - 1) % 3;
    let row = (an.clamp(1, 9) - 1) / 3; // 0 bottom, 1 middle, 2 top
    let (x, ax, align) = match col {
        0 => (-0.5 + ml / w, 0.0, "left"),
        2 => (0.5 - mr / w, 1.0, "right"),
        _ => ((ml - mr) / (2.0 * w), 0.5, "center"),
    };
    let (y, ay) = match row {
        0 => (0.5 - mv / h, 1.0),
        2 => (-0.5 + mv / h, 0.0),
        _ => (0.0, 0.5),
    };
    let mut m = Map::new();
    m.insert("position".into(), json!([x, y]));
    m.insert("anchor".into(), json!([ax, ay]));
    m.insert("alignment".into(), align.into());
    m.insert("max_width".into(), json!(1.0 - (ml + mr) / w));
    m
}

/// An ASS style as `project.styles` parameters. Lengths are scaled by `k` (frame px per
/// script px); `frac` writes `size` as a fraction of the height instead (no `frame_size`).
fn style_params(s: &AssStyle, play_res: [f64; 2], k: f64, frac: bool) -> Map<String, Value> {
    let num = |n: &str| {
        s.fields
            .get(n)
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<f64>().ok())
    };
    let col = |n: &str| s.fields.get(n).and_then(Value::as_str).and_then(ass_colour);
    let mut m = Map::new();
    if let Some(f) = s.fields.get("Fontname").and_then(Value::as_str) {
        m.insert("font".into(), f.into());
    }
    if let Some(size) = num("Fontsize") {
        let v = if frac { size / play_res[1] } else { size * k };
        m.insert("size".into(), json!(v));
    }
    if let Some(c) = col("PrimaryColour") {
        m.insert("colour".into(), json!(c));
    }
    m.insert("bold".into(), json!(num("Bold").is_some_and(|b| b != 0.0)));
    m.insert(
        "italic".into(),
        json!(num("Italic").is_some_and(|b| b != 0.0)),
    );
    let px = |v: f64| {
        if frac {
            v / play_res[1] * 1080.0
        } else {
            v * k
        }
    };
    let outline = num("Outline").unwrap_or(0.0);
    let back = col("BackColour").unwrap_or([0.0, 0.0, 0.0, 0.5]);
    if num("BorderStyle") == Some(3.0) {
        let c = col("OutlineColour").unwrap_or(back);
        m.insert(
            "box".into(),
            json!({"colour": c, "padding": px(outline.max(2.0))}),
        );
    } else if outline > 0.0 {
        let c = col("OutlineColour").unwrap_or([0.0, 0.0, 0.0, 1.0]);
        m.insert("outline".into(), json!({"colour": c, "width": px(outline)}));
    }
    if let Some(d) = num("Shadow").filter(|d| *d > 0.0) {
        m.insert(
            "shadow".into(),
            json!({"colour": back, "offset": [px(d), px(d)], "blur": 0.0}),
        );
    }
    if let Some(sp) = num("Spacing").filter(|v| *v != 0.0) {
        if let Some(size) = num("Fontsize").filter(|v| *v > 0.0) {
            m.insert("tracking".into(), json!(sp / size));
        }
    }
    let an = num("Alignment").map_or(2, numpad);
    let margin = |n: &str| num(n).unwrap_or(10.0);
    m.extend(placement(
        an,
        margin("MarginL"),
        margin("MarginR"),
        margin("MarginV"),
        play_res,
    ));
    m
}

/// Line-wide overrides at the start of `raw` (`{\an8\pos(..)\fad(..)}`) applied to `params`;
/// returns the plain text and the number of tags dropped.
fn line(
    raw: &str,
    params: &mut Map<String, Value>,
    base: &AssStyle,
    play_res: [f64; 2],
) -> (String, usize) {
    let mut text = String::new();
    let mut dropped = 0;
    let mut rest = raw;
    let mut leading = true;
    let mut an: Option<u8> = None;
    let mut pos: Option<[f64; 2]> = None;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('{') {
            let end = after.find('}').unwrap_or(after.len());
            for tag in after[..end].split('\\').filter(|t| !t.is_empty()) {
                let arg = |p: &str| {
                    tag.strip_prefix(p)
                        .map(|a| a.trim_matches(|c| c == '(' || c == ')'))
                };
                let nums = |a: &str| -> Vec<f64> {
                    a.split(',').filter_map(|v| v.trim().parse().ok()).collect()
                };
                if !leading {
                    dropped += 1;
                } else if let Some(a) = arg("an") {
                    an = a.parse().ok();
                } else if let Some(a) = arg("pos") {
                    let n = nums(a);
                    if n.len() == 2 {
                        pos = Some([n[0], n[1]]);
                    }
                } else if let Some(a) = arg("fade").or_else(|| arg("fad")) {
                    let n = nums(a);
                    if n.len() >= 2 {
                        params.insert(
                            "animation".into(),
                            json!({"in": {"kind": "fade", "duration": n[0] / 1000.0},
                                   "out": {"kind": "fade", "duration": n[1] / 1000.0}}),
                        );
                    }
                } else if let Some(a) = arg("b").filter(|a| a.len() <= 3) {
                    params.insert("bold".into(), json!(a.parse::<u32>().is_ok_and(|v| v != 0)));
                } else if let Some(a) = arg("i").filter(|a| a.len() <= 1) {
                    params.insert("italic".into(), json!(a == "1"));
                } else {
                    dropped += 1;
                }
            }
            rest = after.get(end + 1..).unwrap_or("");
            continue;
        }
        leading = false;
        let next = rest.find('{').unwrap_or(rest.len());
        text.push_str(&rest[..next]);
        rest = &rest[next..];
    }
    let text = text
        .replace("\\N", "\n")
        .replace("\\n", " ")
        .replace("\\h", "\u{a0}");
    if an.is_some() || pos.is_some() {
        let num = |n: &str| {
            base.fields
                .get(n)
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<f64>().ok())
        };
        let a = an.unwrap_or_else(|| num("Alignment").map_or(2, numpad));
        let margin = |n: &str| num(n).unwrap_or(10.0);
        let mut p = placement(
            a,
            margin("MarginL"),
            margin("MarginR"),
            margin("MarginV"),
            play_res,
        );
        if let Some([x, y]) = pos {
            p.insert(
                "position".into(),
                json!([x / play_res[0] - 0.5, y / play_res[1] - 0.5]),
            );
        }
        params.extend(p);
    }
    (text.trim().to_owned(), dropped)
}

/// Adds `src`'s subtitles to `timeline`: styles into `project.styles`, lines as text clips on
/// new `Subtitles` tracks, shifted by `offset` seconds.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn import(src: &str, timeline: &mut Timeline, offset: f64) -> Result<Report> {
    let ass = parse(src)?;
    let rate = timeline.rate().unwrap_or(24.0);
    let frame_h = timeline
        .metadata
        .get("ffcine")
        .and_then(|f| f.get("frame_size"))
        .and_then(|s| s.get(1))
        .and_then(Value::as_f64);
    let k = frame_h.map_or(1.0, |h| h / ass.play_res[1]);
    let mut project = timeline
        .project()
        .transpose()?
        .unwrap_or_else(Project::default);
    let mut report = Report {
        styles: ass.styles.len(),
        ..Report::default()
    };
    for s in &ass.styles {
        project.styles.insert(
            format!("ass:{}", s.name),
            Value::Object(style_params(s, ass.play_res, k, frame_h.is_none())),
        );
    }
    timeline.set_project(&project)?;

    let default = AssStyle {
        name: "Default".into(),
        fields: Map::new(),
    };
    let mut events: Vec<&Event> = ass.events.iter().filter(|e| e.end > e.start).collect();
    events.sort_by(|a, b| a.start.total_cmp(&b.start));
    // Track lanes: (end frame, items).
    let mut lanes: Vec<(i64, Vec<Item>)> = Vec::new();
    for (n, e) in events.iter().enumerate() {
        let base = ass
            .styles
            .iter()
            .find(|s| s.name == e.style)
            .unwrap_or(&default);
        let mut params = Map::new();
        params.insert("style".into(), format!("ass:{}", base.name).into());
        let (text, dropped) = line(&e.text, &mut params, base, ass.play_res);
        report.dropped_tags += dropped;
        if text.is_empty() {
            continue;
        }
        params.insert("text".into(), text.clone().into());
        let f0 = ((e.start + offset) * rate).round().max(0.0) as i64;
        let f1 = ((e.end + offset) * rate).round() as i64;
        if f1 <= f0 {
            continue;
        }
        let lane = if let Some(i) = lanes.iter().position(|(end, _)| *end <= f0) {
            i
        } else {
            lanes.push((0, Vec::new()));
            lanes.len() - 1
        };
        let (end, items) = &mut lanes[lane];
        if f0 > *end {
            #[allow(clippy::cast_precision_loss)]
            items.push(Item::Gap(Gap::new(RationalTime::new(
                (f0 - *end) as f64,
                rate,
            ))));
        }
        let name: String = text.chars().take(32).collect();
        #[allow(clippy::cast_precision_loss)]
        let mut clip = Clip::new(
            name,
            MediaReference::Generator(GeneratorReference::new(GENERATOR, params)),
            Some(TimeRange::from_frames(0.0, (f1 - f0) as f64, rate)),
        );
        clip.set_ffcine_id(format!("ass{}", n + 1));
        items.push(Item::Clip(clip));
        *end = f1;
        report.events += 1;
    }
    report.tracks = lanes.len();
    for (i, (_, items)) in lanes.into_iter().enumerate() {
        let name = if i == 0 {
            "Subtitles".to_owned()
        } else {
            format!("Subtitles {}", i + 1)
        };
        let mut t = Track::new(name, track_kind::VIDEO);
        t.children = items;
        timeline.tracks.children.push(Item::Track(t));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TextLayer;

    const SAMPLE: &str = "\u{feff}[Script Info]
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Inter,54,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,-1,0,0,0,100,100,0,0,1,3,2,2,60,60,50,1
Style: Place,Cormorant Garamond,90,&H00EEF6FA,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,0,0,7,100,40,90,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,Hello, world!\\Nsecond line
Dialogue: 0,0:00:02.00,0:00:04.00,Place,,0,0,0,,{\\fad(300,500)\\an7}Old Town
Comment: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,not shown
Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,{\\pos(960,100)}top {\\c&H0000FF&}red
";

    #[test]
    fn parses_styles_and_events() {
        let a = parse(SAMPLE).expect("parse");
        assert_eq!(a.play_res, [1920.0, 1080.0]);
        assert_eq!(a.styles.len(), 2);
        assert_eq!(a.events.len(), 3);
        assert_eq!(a.events[0].text, "Hello, world!\\Nsecond line");
        assert!((a.events[1].start - 2.0).abs() < 1e-9);
        assert_eq!(
            ass_colour("&H80000000"),
            Some([0.0, 0.0, 0.0, 1.0 - 128.0 / 255.0])
        );
        assert_eq!(ass_colour("&H00FF8000").map(|c| c[2]), Some(1.0));
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn imports_as_text_clips() {
        let mut t = Timeline::default();
        let mut md = Map::new();
        md.insert("frame_size".into(), json!([3840, 2160]));
        t.metadata.insert("ffcine".into(), Value::Object(md));
        let r = import(SAMPLE, &mut t, 0.0).expect("import");
        assert_eq!((r.events, r.styles, r.tracks), (3, 2, 2));
        assert_eq!(r.dropped_tags, 1);
        let p = t.project().expect("project").expect("ok");
        let d = &p.styles["ass:Default"];
        assert_eq!(d["size"], 108.0);
        assert_eq!(d["bold"], true);
        assert_eq!(d["anchor"], json!([0.5, 1.0]));
        assert_eq!(d["outline"]["width"], 6.0);
        let layer = TextLayer::from_timeline(&t, ".").expect("layer");
        let c = layer.clips();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].text, "Hello, world!\nsecond line");
        assert!((c[0].start - 1.0).abs() < 1e-9 && (c[0].end - 3.5).abs() < 1e-9);
        // The overlapping line went to the second track, with its fade and top-left anchor.
        let place = c.iter().find(|c| c.text == "Old Town").expect("place");
        assert_eq!(place.track, 1);
        assert_eq!(place.params["anchor"], json!([0.0, 0.0]));
        let s = place.style([3840.0, 2160.0]);
        assert!((s.fade_in - 0.3).abs() < 1e-9 && (s.fade_out - 0.5).abs() < 1e-9);
        let top = c.iter().find(|c| c.text.starts_with("top")).expect("pos");
        assert_eq!(top.params["position"], json!([0.0, 100.0 / 1080.0 - 0.5]));
    }
}
