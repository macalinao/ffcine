//! `ffcine.title` generator clips: a designed title sequence or end screen (title words
//! staggering in, a rule drawing out from the middle, subtitle and credit lines after them, an
//! optional backdrop), expanded into `ffcine.text` pieces that render like any text clip.
//!
//! Every piece is a text clip's parameter map, so styles, highlights, animations and keyframes
//! all apply; [`expand`] stacks them around `position` with good defaults for `kind` `title`
//! and `end`, and [`insert`] adds a title clip to a timeline (`ffcine title`).

use ffcine_core::Result;
use ffcine_timeline::{
    Clip, Gap, GeneratorReference, HasMetadata, Item, MediaReference, RationalTime, TimeRange,
    Timeline, Track, track_kind,
};
use serde_json::{Map, Value, json};

use crate::style::{self, merge};

/// `generator_kind` of title clips.
pub const GENERATOR: &str = "ffcine.title";

/// Keys a shared style may not set on a piece: where it sits and how it moves are the title's.
const LAYOUT: [&str; 10] = [
    "size",
    "position",
    "anchor",
    "alignment",
    "animation",
    "fade_in",
    "fade_out",
    "delay",
    "x",
    "y",
];

/// One piece of an expanded title.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    /// `title`, `rule`, `subtitle`, `lines` or `background`.
    pub role: &'static str,
    pub text: String,
    /// Merged `ffcine.text` parameters.
    pub params: Map<String, Value>,
}

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn num(m: &Map<String, Value>, k: &str) -> Option<f64> {
    m.get(k).and_then(Value::as_f64)
}

/// The text of `key`: a string, or an array of lines.
fn text(params: &Map<String, Value>, key: &str) -> Option<String> {
    let t = match params.get(key)? {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!t.trim().is_empty()).then_some(t)
}

/// Words in a text (for the stagger's length).
fn words(t: &str) -> usize {
    t.split_whitespace().count().max(1)
}

/// Expands a title clip's `params` (with `project.styles`) for a frame `frame_h` px tall
/// (`frame_size`; text sizes in px convert with it). Pieces come bottom first.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub fn expand(
    styles: &Map<String, Value>,
    params: &Map<String, Value>,
    frame_h: f64,
) -> Vec<Piece> {
    let end = params.get("kind").and_then(Value::as_str) == Some("end");
    let stagger = num(params, "stagger").unwrap_or(if end { 0.15 } else { 0.12 });
    let dur = num(params, "duration").unwrap_or(0.9);
    let out = num(params, "fade_out").unwrap_or(0.8);
    let gap = num(params, "gap").unwrap_or(0.03);
    let pos = params
        .get("position")
        .and_then(Value::as_array)
        .and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]))
        .unwrap_or([0.0, 0.0]);
    let align = params
        .get("alignment")
        .and_then(Value::as_str)
        .unwrap_or("center");
    let ax = match align {
        "left" => 0.0,
        "right" => 1.0,
        _ => 0.5,
    };
    let fade_out = json!({"kind": "fade", "duration": out});

    // Shared look: `base`, then the title's `style`, without layout keys.
    let mut shared = style::cascade(styles, &{
        let mut m = Map::new();
        if let Some(s) = params.get("style") {
            m.insert("style".into(), s.clone());
        }
        m
    });
    shared.remove("style");
    for k in LAYOUT {
        shared.remove(k);
    }
    // A piece: defaults <- shared <- its own `<role>_style` (a style name or an object).
    let piece = |role: &str, defaults: Value| {
        let mut m = obj(&defaults);
        merge(&mut m, &shared);
        match params.get(&format!("{role}_style")) {
            Some(Value::String(name)) => {
                let mut one = Map::new();
                one.insert("style".into(), name.clone().into());
                let mut c = style::cascade(styles, &one);
                c.remove("style");
                merge(&mut m, &c);
            }
            Some(Value::Object(o)) => merge(&mut m, o),
            _ => {}
        }
        m
    };
    let colour = shared
        .get("colour")
        .or_else(|| shared.get("color"))
        .and_then(|c| style::colour(Some(c)))
        .unwrap_or([1.0; 4]);

    let title = text(params, "title");
    let subtitle = text(params, "subtitle");
    let lines = text(params, "lines");
    let title_words = title.as_deref().map_or(0, words);
    let mut pieces = Vec::new();
    if let Some(bg) = match params.get("background") {
        Some(Value::Bool(false) | Value::Null) => None,
        // `true` or anything not a colour: the default dim.
        Some(v) if style::colour(Some(v)).is_some() => Some(v.clone()),
        Some(_) => Some(json!([0.0, 0.0, 0.0, 0.6])),
        None if end => Some(json!([0.0, 0.0, 0.0, 0.6])),
        None => None,
    } {
        pieces.push(Piece {
            role: "background",
            text: String::new(),
            params: obj(&json!({
                "rect": [1.0, 1.0], "colour": bg, "position": [0.0, 0.0],
                "animation": {"in": {"kind": "fade", "duration": 0.6}, "out": fade_out},
            })),
        });
    }

    // Stack top to bottom: title, rule, subtitle, lines; times one after another.
    let mut stack: Vec<(&'static str, String, Map<String, Value>)> = Vec::new();
    let mut next = 0.0;
    if let Some(t) = &title {
        let m = piece(
            "title",
            json!({
                "size": if end { 0.07 } else { 0.085 }, "tracking": 0.02, "line_height": 1.15,
                "max_width": 0.8,
                "animation": {
                    "in": if end {
                        json!({"kind": "fade", "by": "word", "duration": dur, "stagger": stagger})
                    } else {
                        json!({"kind": "slide", "by": "word", "duration": dur,
                               "stagger": stagger, "distance": 0.03})
                    },
                    "out": fade_out,
                },
            }),
        );
        stack.push(("title", t.clone(), m));
        next = stagger * (title_words.saturating_sub(1)) as f64 + dur * 0.6;
    }
    let rule = match params.get("rule") {
        Some(Value::Bool(false) | Value::Null) => None,
        Some(Value::Object(o)) => Some(o.clone()),
        _ if title.is_some() && (subtitle.is_some() || lines.is_some() || !end) => Some(Map::new()),
        _ => None,
    };
    if let Some(r) = rule {
        let width = num(&r, "width").unwrap_or(0.12);
        let thickness = num(&r, "thickness").unwrap_or(0.0025);
        let c = style::colour(r.get("colour").or_else(|| r.get("color"))).unwrap_or([
            colour[0],
            colour[1],
            colour[2],
            colour[3] * 0.8,
        ]);
        let delay = num(&r, "delay").unwrap_or(if title.is_some() { 0.25 } else { 0.0 });
        stack.push((
            "rule",
            String::new(),
            obj(&json!({
                "rect": [width, thickness], "colour": c, "delay": delay,
                "animation": {"in": {"kind": "wipe", "direction": "center", "duration": dur},
                              "out": fade_out},
            })),
        ));
        next = f64::max(next, delay + dur * 0.5);
    }
    if let Some(t) = &subtitle {
        let m = piece(
            "subtitle",
            json!({
                "size": 0.032, "uppercase": true, "tracking": 0.25, "opacity": 0.85,
                "max_width": 0.8, "delay": next,
                "animation": {"in": {"kind": "slide", "duration": dur, "distance": 0.015},
                              "out": fade_out},
            }),
        );
        stack.push(("subtitle", t.clone(), m));
        next += dur * 0.5;
    }
    if let Some(t) = &lines {
        let m = piece(
            "lines",
            json!({
                "size": 0.034, "line_height": 1.5, "opacity": 0.9, "max_width": 0.8,
                "delay": next,
                "animation": {"in": {"kind": "slide", "by": "line", "duration": dur,
                                     "stagger": stagger * 2.0, "distance": 0.015},
                              "out": fade_out},
            }),
        );
        stack.push(("lines", t.clone(), m));
    }

    // Heights as fractions of the frame, to centre the stack on `position`.
    let height = |role: &str, text: &str, m: &Map<String, Value>| {
        if role == "rule" {
            return m
                .get("rect")
                .and_then(|r| r.get(1))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
        }
        let size = num(m, "size").unwrap_or(0.05);
        let size = if size > 1.0 { size / frame_h } else { size };
        let lh = num(m, "line_height").unwrap_or(1.2);
        text.lines().count().max(1) as f64 * size * lh
    };
    let heights: Vec<f64> = stack.iter().map(|(r, t, m)| height(r, t, m)).collect();
    let total = heights.iter().sum::<f64>() + gap * stack.len().saturating_sub(1) as f64;
    let mut y = pos[1] - total / 2.0;
    for ((role, text, mut m), h) in stack.into_iter().zip(heights) {
        m.entry("position").or_insert(json!([pos[0], y]));
        m.entry("anchor").or_insert(json!([ax, 0.0]));
        if role != "rule" {
            m.entry("alignment").or_insert(align.into());
        }
        y += h + gap;
        pieces.push(Piece {
            role,
            text,
            params: m,
        });
    }
    pieces
}

/// Adds a title clip with `params` (see `docs/text.md#titles-and-end-screens`) at `at` seconds,
/// `duration` long, on a new `Titles` track. Returns the track's name.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn insert(
    timeline: &mut Timeline,
    params: Map<String, Value>,
    at: f64,
    duration: f64,
) -> Result<String> {
    let rate = timeline.rate().unwrap_or(24.0);
    let f0 = (at * rate).round().max(0.0);
    let len = (duration * rate).round().max(1.0);
    let n = timeline
        .tracks
        .children
        .iter()
        .filter(|i| matches!(i, Item::Track(t) if t.name.starts_with("Titles")))
        .count();
    let name = if n == 0 {
        "Titles".to_owned()
    } else {
        format!("Titles {}", n + 1)
    };
    let label: String = text(&params, "title")
        .or_else(|| text(&params, "lines"))
        .unwrap_or_else(|| "Title".into())
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(32)
        .collect();
    let mut t = Track::new(name.clone(), track_kind::VIDEO);
    if f0 > 0.0 {
        t.children
            .push(Item::Gap(Gap::new(RationalTime::new(f0, rate))));
    }
    let mut clip = Clip::new(
        label,
        MediaReference::Generator(GeneratorReference::new(GENERATOR, params)),
        Some(TimeRange::from_frames(0.0, len, rate)),
    );
    clip.set_ffcine_id(format!("title{}", n + 1));
    t.children.push(Item::Clip(clip));
    timeline.tracks.children.push(Item::Track(t));
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_defaults_stack_and_stagger() {
        let styles =
            obj(&json!({"base": {"font": "DejaVu Serif", "size": 40, "colour": [1, 0.9, 0.8, 1]}}));
        let p = obj(&json!({"title": "A Long Way Down", "subtitle": "Chapter one"}));
        let pieces = expand(&styles, &p, 1080.0);
        let roles: Vec<_> = pieces.iter().map(|p| p.role).collect();
        assert_eq!(roles, ["title", "rule", "subtitle"]);
        let t = &pieces[0].params;
        // The base's font applies, its size doesn't.
        assert_eq!(t["font"], "DejaVu Serif");
        assert_eq!(t["size"], 0.085);
        assert_eq!(t["animation"]["in"]["by"], "word");
        // Stacked downwards around the middle.
        let y = |i: usize| pieces[i].params["position"][1].as_f64().unwrap();
        assert!(y(0) < y(1) && y(1) < y(2));
        assert!(y(0) < 0.0 && y(2) > 0.0);
        // The subtitle waits for the title's words.
        assert!(pieces[2].params["delay"].as_f64().unwrap() > 0.5);
        let rule = &pieces[1].params;
        assert_eq!(rule["rect"][0], 0.12);
        assert!((rule["colour"][1].as_f64().unwrap() - 0.9).abs() < 1e-6);
    }

    #[test]
    fn end_screen_has_backdrop_and_lines() {
        let p = obj(&json!({
            "kind": "end", "title": "Thanks for watching",
            "lines": ["Music: Example Artist", "Shot on location"],
            "lines_style": {"size": 0.03}, "background": [0, 0, 0, 0.8],
        }));
        let pieces = expand(&Map::new(), &p, 1080.0);
        let roles: Vec<_> = pieces.iter().map(|p| p.role).collect();
        assert_eq!(roles, ["background", "title", "rule", "lines"]);
        assert_eq!(pieces[0].params["colour"], json!([0, 0, 0, 0.8]));
        assert_eq!(pieces[3].text, "Music: Example Artist\nShot on location");
        assert_eq!(pieces[3].params["size"], 0.03);
        assert_eq!(pieces[3].params["animation"]["in"]["by"], "line");
        let none = expand(
            &Map::new(),
            &obj(&json!({"title": "x", "rule": false})),
            1080.0,
        );
        assert_eq!(none.len(), 1);
    }

    #[test]
    fn inserts_on_a_titles_track() {
        let mut t = Timeline::default();
        let name = insert(&mut t, obj(&json!({"title": "Hello"})), 2.0, 4.0).unwrap();
        assert_eq!(name, "Titles");
        let name = insert(
            &mut t,
            obj(&json!({"kind": "end", "title": "Bye"})),
            0.0,
            4.0,
        )
        .unwrap();
        assert_eq!(name, "Titles 2");
        let placed = t.placed_clips();
        assert_eq!(placed.len(), 2);
        assert!((placed[0].start - 2.0).abs() < 1e-9);
        assert!((placed[0].end() - 6.0).abs() < 1e-9);
    }
}
