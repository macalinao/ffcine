//! Resolving an `ffcine.text` clip's look: `project.styles.base` <- its `style` (and that
//! style's `based_on` chain) <- the clip's own parameters, then read into a [`TextStyle`].
//!
//! Besides the parameters in `docs/timeline.md`, styles take the additive keys in
//! `docs/text.md` (`anchor`, `shadow`, `outline`, `box`, `fade_in`, ...) and the prototype's
//! `edit.yaml` keys (`size` as a fraction of the frame height, `color` 0..255, `x`/`y`,
//! `shadow_opacity`, `pill_opacity`, `delay`), so imported projects render as they were.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Horizontal alignment of lines within the text box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shadow {
    pub colour: [f32; 4],
    /// Offset in px at `frame_size`.
    pub offset: [f32; 2],
    /// Blur radius in px at `frame_size`.
    pub blur: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    pub colour: [f32; 4],
    /// Width in px at `frame_size`.
    pub width: f32,
}

/// A box behind the text (caption pill, card scrim).
#[derive(Debug, Clone, PartialEq)]
pub struct BoxStyle {
    pub colour: [f32; 4],
    /// Padding around the text in px at `frame_size`.
    pub padding: f32,
}

/// A fully resolved text look. Lengths are px at the timeline's `frame_size`.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub font: Option<String>,
    pub face: Option<String>,
    /// A font file to load (absolute, or relative to the timeline's directory).
    pub font_file: Option<PathBuf>,
    /// Face index in a collection (`.ttc`).
    pub font_index: u32,
    pub size: f32,
    pub colour: [f32; 4],
    /// Colour of `*marked*` spans; `None` leaves asterisks as text.
    pub highlight: Option<[f32; 4]>,
    /// Where `anchor` lands: offset from the frame's middle, fractions of the frame, y down.
    pub position: [f32; 2],
    /// Point of the text box placed at `position`: fractions of the box, `[0, 0]` top left.
    pub anchor: [f32; 2],
    pub alignment: Align,
    pub bold: bool,
    pub italic: bool,
    /// Line height as a multiple of `size`.
    pub line_height: f32,
    /// Wrap width, fraction of the frame width.
    pub max_width: f32,
    /// Extra space between letters, in ems.
    pub tracking: f32,
    pub uppercase: bool,
    pub shadow: Option<Shadow>,
    pub outline: Option<Outline>,
    pub box_: Option<BoxStyle>,
    /// Seconds from the clip's start before the text shows.
    pub delay: f64,
    pub fade_in: f64,
    pub fade_out: f64,
}

impl TextStyle {
    /// Defaults at a frame `height` px tall: white sans at 5 % of the height, centred.
    pub fn default_for(height: f32) -> Self {
        Self {
            font: None,
            face: None,
            font_file: None,
            font_index: 0,
            size: 0.05 * height,
            colour: [1.0; 4],
            highlight: None,
            position: [0.0, 0.0],
            anchor: [0.5, 0.5],
            alignment: Align::Center,
            bold: false,
            italic: false,
            line_height: 1.2,
            max_width: 0.9,
            tracking: 0.0,
            uppercase: false,
            shadow: None,
            outline: None,
            box_: None,
            delay: 0.0,
            fade_in: 0.0,
            fade_out: 0.0,
        }
    }
}

/// Deep-merges `top` over `base` (objects merge key by key, anything else replaces).
pub fn merge(base: &mut Map<String, Value>, top: &Map<String, Value>) {
    for (k, v) in top {
        match (base.get_mut(k), v) {
            (Some(Value::Object(b)), Value::Object(t)) => merge(b, t),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// The merged parameter map for a clip: `base`, the `style` chain, then `params`.
pub fn cascade(styles: &Map<String, Value>, params: &Map<String, Value>) -> Map<String, Value> {
    let mut chain = Vec::new();
    let mut name = params.get("style").and_then(Value::as_str);
    while let Some(n) = name {
        if n == "base" || chain.iter().any(|(c, _)| *c == n) || chain.len() > 16 {
            break;
        }
        let Some(s) = styles.get(n).and_then(Value::as_object) else {
            break;
        };
        chain.push((n, s));
        name = s.get("based_on").and_then(Value::as_str);
    }
    let mut out = styles
        .get("base")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (_, s) in chain.iter().rev() {
        merge(&mut out, s);
    }
    merge(&mut out, params);
    out
}

fn f(v: Option<&Value>) -> Option<f32> {
    #[allow(clippy::cast_possible_truncation)]
    v.and_then(Value::as_f64).map(|x| x as f32)
}

fn vec2(v: Option<&Value>) -> Option<[f32; 2]> {
    let a = v?.as_array()?;
    Some([f(a.first())?, f(a.get(1))?])
}

/// `[r, g, b]` or `[r, g, b, a]`, 0..1, or 0..255 when any channel is above 1 (`edit.yaml`).
pub fn colour(v: Option<&Value>) -> Option<[f32; 4]> {
    let a = v?.as_array()?;
    let mut c: Vec<f32> = a.iter().map(|x| f(Some(x))).collect::<Option<_>>()?;
    if c.len() < 3 {
        return None;
    }
    if c.iter().take(3).any(|x| *x > 1.0) {
        for x in c.iter_mut().take(3) {
            *x /= 255.0;
        }
    }
    Some([c[0], c[1], c[2], c.get(3).copied().unwrap_or(1.0)])
}

fn bool_(v: Option<&Value>) -> Option<bool> {
    v.and_then(|v| v.as_bool().or_else(|| v.as_f64().map(|x| x != 0.0)))
}

/// A length: px at `frame_size`, or a fraction of the frame height when 0 < v <= 1 and
/// `fractions` (the prototype's sizes).
fn length(v: Option<&Value>, height: f32, fractions: bool) -> Option<f32> {
    let x = f(v)?;
    Some(if fractions && x > 0.0 && x <= 1.0 {
        x * height
    } else {
        x
    })
}

/// Fade seconds from `animation.{in,out}` (`kind: fade`) or the flat `fade_in`/`fade_out`.
fn fade(m: &Map<String, Value>, dir: &str, flat: &str) -> Option<f64> {
    let a = m.get("animation").and_then(|a| a.get(dir));
    if let Some(a) = a {
        // Kinds not drawn yet (slide, pop, typewriter) fade over their duration.
        return a.get("duration").and_then(Value::as_f64);
    }
    m.get(flat).and_then(Value::as_f64)
}

#[allow(clippy::too_many_lines)] // one key after another
/// Reads a merged parameter map (see [`cascade`]) for a `frame` of `[w, h]` px.
pub fn resolve(m: &Map<String, Value>, frame: [f32; 2], name: Option<&str>) -> TextStyle {
    let [_, fh] = frame;
    let mut s = TextStyle::default_for(fh);
    s.font = m.get("font").and_then(Value::as_str).map(str::to_owned);
    s.face = m.get("face").and_then(Value::as_str).map(str::to_owned);
    let style_name = m
        .get("style")
        .and_then(Value::as_str)
        .or(name)
        .unwrap_or("");
    // The prototype names font files per element in its `style` section (now `base`).
    let legacy = match style_name {
        _ if m.contains_key("font_file") || m.contains_key("font") => None,
        "card" => m.get("serif_card").or_else(|| m.get("serif")),
        "location" => m.get("serif_sc"),
        "title" => m.get("serif"),
        "endcard" | "endscreen" => m.get("serif_italic"),
        "caption" => m.get("sans"),
        _ => None,
    };
    s.font_file = m
        .get("font_file")
        .or(legacy)
        .and_then(Value::as_str)
        .map(|p| {
            let dir = m.get("fonts_dir").and_then(Value::as_str).unwrap_or("");
            Path::new(dir).join(p)
        });
    // Captions default to Inter Bold (face 14 of Inter.ttc) as in the prototype.
    #[allow(clippy::cast_possible_truncation)]
    let index = m
        .get("font_index")
        .and_then(Value::as_u64)
        .map(|i| i as u32)
        .or((style_name == "caption" && legacy.is_some()).then_some(14));
    s.font_index = index.unwrap_or(0);
    if let Some(x) = length(m.get("size"), fh, true) {
        s.size = x;
    }
    if let Some(c) = colour(m.get("colour").or_else(|| m.get("color"))) {
        s.colour = c;
    }
    s.highlight = colour(m.get("highlight"));
    if let Some(o) = f(m.get("opacity")) {
        s.colour[3] *= o;
    }
    // The prototype's placement: `x`/`y` from the top left as fractions of the frame; `y` is the
    // baseline (cards, locations) or the block's bottom (captions); locations measure `x` from
    // the right edge.
    let (lx, ly) = (f(m.get("x")), f(m.get("y")));
    if lx.is_some() || ly.is_some() {
        let y = ly.unwrap_or(0.9) - 0.5;
        (s.position, s.anchor, s.alignment) = match style_name {
            "location" => ([0.5 - lx.unwrap_or(0.05), y], [1.0, 1.0], Align::Right),
            "caption" | "subs" | "subtitle" => ([0.0, y], [0.5, 1.0], Align::Center),
            _ if lx.is_none() => ([0.0, y], [0.5, 1.0], Align::Center),
            _ => ([lx.unwrap_or(0.05) - 0.5, y], [0.0, 1.0], Align::Left),
        };
    }
    if let Some(p) = vec2(m.get("position")) {
        s.position = p;
    }
    if let Some(a) = vec2(m.get("anchor")) {
        s.anchor = a;
    }
    match m.get("alignment").and_then(Value::as_str) {
        Some("left") => s.alignment = Align::Left,
        Some("right") => s.alignment = Align::Right,
        Some("center" | "centre") => s.alignment = Align::Center,
        _ => {}
    }
    s.bold = bool_(m.get("bold")).unwrap_or(false);
    s.italic = bool_(m.get("italic")).unwrap_or(false);
    s.uppercase = bool_(m.get("uppercase")).unwrap_or(false);
    if let Some(x) = f(m.get("line_height").or_else(|| m.get("line_spacing"))) {
        s.line_height = x;
    }
    if let Some(x) = f(m.get("max_width")) {
        s.max_width = x;
    }
    if let Some(x) = f(m.get("tracking")) {
        s.tracking = x;
    }
    s.shadow = match m.get("shadow") {
        Some(Value::Object(o)) => Some(Shadow {
            colour: colour(o.get("colour").or_else(|| o.get("color")))
                .unwrap_or([0.0, 0.0, 0.0, 0.5]),
            offset: vec2(o.get("offset")).unwrap_or([0.0, 0.002 * fh]),
            blur: f(o.get("blur")).unwrap_or(0.006 * fh),
        }),
        Some(Value::Bool(false) | Value::Null) => None,
        _ => f(m.get("shadow_opacity"))
            .filter(|o| *o > 0.0)
            .map(|o| Shadow {
                colour: [0.0, 0.0, 0.0, o],
                offset: [0.0, 0.002 * fh],
                blur: length(m.get("shadow_blur"), fh, true).unwrap_or(0.006 * fh),
            }),
    };
    s.outline = match m.get("outline") {
        Some(Value::Object(o)) => Some(Outline {
            colour: colour(o.get("colour").or_else(|| o.get("color")))
                .unwrap_or([0.0, 0.0, 0.0, 1.0]),
            width: f(o.get("width")).unwrap_or(0.002 * fh),
        }),
        _ => None,
    };
    s.box_ = match m.get("box") {
        Some(Value::Object(o)) => Some(BoxStyle {
            colour: colour(o.get("colour").or_else(|| o.get("color")))
                .unwrap_or([0.0, 0.0, 0.0, 0.4]),
            padding: f(o.get("padding")).unwrap_or(0.25 * s.size),
        }),
        _ => f(m.get("pill_opacity").or_else(|| m.get("scrim")))
            .filter(|o| *o > 0.0)
            .map(|o| BoxStyle {
                colour: [0.0, 0.0, 0.0, o],
                padding: 0.25 * s.size,
            }),
    };
    s.delay = m.get("delay").and_then(Value::as_f64).unwrap_or(0.0);
    s.fade_in = fade(m, "in", "fade_in").unwrap_or(0.0);
    s.fade_out = fade(m, "out", "fade_out").unwrap_or(0.0);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::needless_pass_by_value)]
    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn cascade_order_and_based_on() {
        let styles = obj(json!({
            "base": {"font": "Inter", "colour": [1, 1, 1, 1], "size": 40, "shadow": {"blur": 3}},
            "card": {"based_on": "place", "size": 80},
            "place": {"alignment": "left", "font": "Cormorant", "shadow": {"offset": [0, 2]}},
        }));
        let m = cascade(
            &styles,
            &obj(json!({"style": "card", "text": "Arrival", "bold": true})),
        );
        assert_eq!(m["font"], "Cormorant");
        assert_eq!(m["size"], 80);
        assert_eq!(m["alignment"], "left");
        assert_eq!(m["shadow"], json!({"blur": 3, "offset": [0, 2]}));
        let s = resolve(&m, [3840.0, 2160.0], None);
        assert!(s.bold);
        assert!((s.size - 80.0).abs() < 1e-6);
        assert_eq!(s.alignment, Align::Left);
        assert_eq!(s.shadow.map(|s| s.offset), Some([0.0, 2.0]));
    }

    #[test]
    fn prototype_keys() {
        let m = obj(json!({
            "style": "card", "size": 0.10, "x": 0.055, "y": 0.175, "fade_in": 0.7,
            "fade_out": 0.6, "color": [250, 246, 238], "shadow_opacity": 0.55, "scrim": 0.34,
        }));
        let s = resolve(&m, [1920.0, 1080.0], None);
        assert!((s.size - 108.0).abs() < 1e-3);
        assert_eq!(s.anchor, [0.0, 1.0]);
        assert!((s.position[0] - (0.055 - 0.5)).abs() < 1e-6);
        assert!((s.colour[0] - 250.0 / 255.0).abs() < 1e-6);
        assert!(s.shadow.is_some() && s.box_.is_some());
        assert!((s.fade_in - 0.7).abs() < 1e-9);
    }

    #[test]
    fn animation_fade_wins() {
        let m =
            obj(json!({"fade_in": 1.0, "animation": {"in": {"kind": "fade", "duration": 0.25}}}));
        let s = resolve(&m, [1920.0, 1080.0], None);
        assert!((s.fade_in - 0.25).abs() < 1e-9);
    }
}
