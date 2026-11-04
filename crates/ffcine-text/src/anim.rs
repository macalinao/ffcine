//! Text animations (`animation.in` / `animation.out`: fade, slide, pop, typewriter, wipe, by
//! word, line or character with a stagger) and keyframed parameters (`keyframes.position`,
//! `scale`, `opacity`, `colour`), baked into per-frame overlay placements ([`Motion`]).
//!
//! The text is rasterised once per clip; every frame then only moves, scales, crops and tints
//! pieces of its masks, so animating costs no extra GPU pass.

use std::sync::Arc;

use ffcine_core::anim::{Clock, Track, is_keys, seconds};
use ffcine_render::{Mask, Motion, Overlay, OverlayPart};
use serde_json::{Map, Value};

use crate::layout::{Glyph, Rendered, Role};

/// The shape of a move over its progress `u` (0..1): `ffcine_core::anim::Ease`.
pub use ffcine_core::anim::Ease;

/// What an entrance or exit does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fade,
    /// Moves in from (out towards) `direction`, fading.
    Slide,
    /// Scales up from `scale` with an overshoot, fading.
    Pop,
    /// Shows one unit after another, no fade; `duration` is the whole typing time.
    Typewriter,
    /// Reveals (hides) along `direction`.
    Wipe,
    None,
}

/// The pieces a text animates in, each after the previous by `stagger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum By {
    Char,
    Word,
    Line,
    Whole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
    /// Wipes from the middle out.
    Center,
}

impl Dir {
    fn vector(self) -> [f32; 2] {
        match self {
            Self::Up => [0.0, -1.0],
            Self::Down => [0.0, 1.0],
            Self::Left => [-1.0, 0.0],
            Self::Right => [1.0, 0.0],
            Self::Center => [0.0, 0.0],
        }
    }
}

/// One of `animation.in` / `animation.out`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anim {
    pub kind: Kind,
    /// Seconds per unit (typewriter: for all of them).
    pub duration: f64,
    pub by: By,
    /// Seconds between units; `None` picks one for `by`.
    pub stagger: Option<f64>,
    pub ease: Ease,
    /// The way the text moves (slide) or the reveal travels (wipe).
    pub direction: Dir,
    /// Slide distance, fraction of the frame height.
    pub distance: f32,
    /// Pop's starting scale.
    pub scale: f32,
}

impl Anim {
    /// A plain fade over `duration` seconds.
    pub fn fade(duration: f64) -> Self {
        Self {
            kind: Kind::Fade,
            duration,
            by: By::Whole,
            stagger: None,
            ease: Ease::Linear,
            direction: Dir::Up,
            distance: 0.04,
            scale: 0.6,
        }
    }

    /// Reads `{kind, duration, by, stagger, ease, direction, distance, scale}`. Unknown kinds
    /// (another app's names) fade.
    pub fn parse(v: &Map<String, Value>) -> Self {
        let kind = match v.get("kind").and_then(Value::as_str).unwrap_or("fade") {
            "slide" | "slide_up" | "rise" => Kind::Slide,
            "pop" | "scale" | "zoom" => Kind::Pop,
            "typewriter" | "type" => Kind::Typewriter,
            "wipe" | "reveal" => Kind::Wipe,
            "none" | "cut" => Kind::None,
            _ => Kind::Fade,
        };
        let by = match v.get("by").and_then(Value::as_str) {
            Some("char" | "character" | "letter") => By::Char,
            Some("word") => By::Word,
            Some("line") => By::Line,
            None if kind == Kind::Typewriter => By::Char,
            _ => By::Whole,
        };
        let ease = v.get("ease").and_then(Ease::parse).unwrap_or(match kind {
            Kind::Slide => Ease::Out,
            Kind::Pop => Ease::Back,
            Kind::Wipe => Ease::InOut,
            _ => Ease::Linear,
        });
        let direction = match v.get("direction").and_then(Value::as_str) {
            Some("down") => Dir::Down,
            Some("left") => Dir::Left,
            Some("right") => Dir::Right,
            Some("center" | "centre") => Dir::Center,
            Some("up") => Dir::Up,
            _ if kind == Kind::Wipe => Dir::Right,
            _ => Dir::Up,
        };
        #[allow(clippy::cast_possible_truncation)]
        let num = |k: &str| v.get(k).and_then(Value::as_f64).map(|x| x as f32);
        Self {
            kind,
            duration: v
                .get("duration")
                .and_then(Value::as_f64)
                .unwrap_or(0.5)
                .max(0.0),
            by,
            stagger: v.get("stagger").and_then(Value::as_f64),
            ease,
            direction,
            distance: num("distance").unwrap_or(0.04),
            scale: num("scale").unwrap_or(0.6),
        }
    }

    fn stagger(&self, n: usize) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        match (self.kind, self.stagger) {
            (_, Some(s)) => s,
            (Kind::Typewriter, None) => self.duration / n.max(1) as f64,
            (_, None) => match self.by {
                By::Char => 0.03,
                By::Word => 0.1,
                By::Line => 0.2,
                By::Whole => 0.0,
            },
        }
    }
}

/// A keyframed value: `[{t, v, interp}]` (the reframe keyframes' form), `t` seconds from the
/// clip's start (negative: from its end), `interp` shaping the move to the next key. Also an
/// expression over `lt` and `dur` (`docs/compositing.md#expressions`), per component in an
/// array, or `{"keys", "expr"}`: an `ffcine_core::anim::Track` on [`Clock::Clip`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Keys<const N: usize> {
    track: Option<Track<[f64; N]>>,
}

impl<const N: usize> Keys<N> {
    /// Reads keys whose values are numbers (`N` = 1 also takes a bare number; a number
    /// for `N` = 2 fills both, for scale; colours may leave out alpha). Keys that don't read
    /// are skipped; an expression that doesn't compile is dropped with a warning.
    #[allow(clippy::many_single_char_names)]
    fn parse(v: Option<&Value>) -> Self {
        let Some(v) = v else {
            return Self::default();
        };
        let value = |v: &Value| -> Option<Value> {
            Some(match v {
                Value::Number(n) => Value::Array(vec![Value::from(n.as_f64()?); N]),
                Value::Array(a) => {
                    let c: Vec<f64> = a.iter().filter_map(Value::as_f64).collect();
                    match (N, c.len()) {
                        (4, 3) => Value::from([c[0], c[1], c[2], 1.0].to_vec()),
                        (n, l) if n == l => Value::from(c),
                        _ => return None,
                    }
                }
                _ => return None,
            })
        };
        let keys = |a: &[Value]| -> Value {
            a.iter()
                .filter_map(|k| {
                    let t = k.get("t").and_then(seconds)?;
                    let v = value(k.get("v").or_else(|| k.get("value"))?)?;
                    let mut o = Map::new();
                    o.insert("t".into(), t.into());
                    o.insert("v".into(), v);
                    // Unknown names are linear, as before.
                    if let Some(e) = k.get("interp").or_else(|| k.get("ease"))
                        && Ease::parse(e).is_some()
                    {
                        o.insert("interp".into(), e.clone());
                    }
                    Some(Value::Object(o))
                })
                .collect()
        };
        let norm = match v {
            Value::Array(a) if is_keys(v) => keys(a),
            Value::Object(o) if o.contains_key("expr") => {
                let mut o = o.clone();
                if let Some(Value::Array(a)) = o.get("keys") {
                    let k = keys(a);
                    o.insert("keys".into(), k);
                }
                Value::Object(o)
            }
            Value::Number(_) | Value::Array(_) => value(v).unwrap_or_else(|| v.clone()),
            _ => v.clone(),
        };
        if norm.as_array().is_some_and(Vec::is_empty) {
            return Self::default();
        }
        match Track::parse(&norm, Clock::Clip, "text keyframes") {
            Ok(t) => Self { track: Some(t) },
            Err(e) => {
                eprintln!("warning: {e}");
                Self::default()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.track.is_none()
    }

    /// The value at `t` seconds into a clip `len` seconds long.
    pub fn at(&self, t: f64, len: f64) -> Option<[f64; N]> {
        self.track.as_ref()?.at(Clock::Clip, t, len)
    }

    fn max(&self) -> Option<f64> {
        let t = self.track.as_ref()?;
        match t {
            Track::Const(c) => c.iter().copied().reduce(f64::max),
            _ => t
                .keys()?
                .iter()
                .flat_map(|k| k.v.iter().copied())
                .reduce(f64::max),
        }
    }
}

/// A clip's animation: its entrance, exit and keyframed parameters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Animation {
    pub in_: Option<Anim>,
    pub out: Option<Anim>,
    /// Absolute `position` (fractions of the frame from its middle).
    pub position: Keys<2>,
    /// Scale about the anchor, `[x, y]`.
    pub scale: Keys<2>,
    /// Multiplies the alpha.
    pub opacity: Keys<1>,
    /// Replaces the text's colour.
    pub colour: Keys<4>,
}

impl Animation {
    /// From merged clip parameters: `animation.{in,out}` (else `fade_in` / `fade_out`) and
    /// `keyframes.{position,scale,opacity,colour}`.
    pub fn parse(m: &Map<String, Value>) -> Self {
        let side = |dir: &str, flat: &str| {
            match m.get("animation").and_then(|a| a.get(dir)) {
                Some(Value::Object(o)) => Some(Anim::parse(o)),
                _ => m.get(flat).and_then(Value::as_f64).map(Anim::fade),
            }
            .filter(|a| a.kind != Kind::None && a.duration > 0.0)
        };
        let k = m.get("keyframes");
        Self {
            in_: side("in", "fade_in"),
            out: side("out", "fade_out"),
            position: Keys::parse(k.and_then(|k| k.get("position"))),
            scale: Keys::parse(k.and_then(|k| k.get("scale"))),
            opacity: Keys::parse(k.and_then(|k| k.get("opacity"))),
            colour: Keys::parse(k.and_then(|k| k.get("colour").or_else(|| k.get("color")))),
        }
    }

    /// Whether the overlay's own linear fades draw it (no motion needed).
    pub fn is_plain_fade(&self) -> bool {
        let plain = |a: &Option<Anim>| {
            a.is_none_or(|a| a.kind == Kind::Fade && a.by == By::Whole && a.ease == Ease::Linear)
        };
        plain(&self.in_)
            && plain(&self.out)
            && self.position.is_empty()
            && self.scale.is_empty()
            && self.opacity.is_empty()
            && self.colour.is_empty()
    }

    /// The largest scale keyframed (at least 1): rasterise at it so the text stays sharp.
    #[allow(clippy::cast_possible_truncation)]
    pub fn raster_scale(&self) -> f32 {
        self.scale.max().map_or(1.0, |s| s.clamp(1.0, 4.0) as f32)
    }
}

/// A piece of the text: a crop of the masks (mask px) and its group under each animation.
#[derive(Debug, Clone, Copy)]
struct Unit {
    crop: [f32; 4],
    line: usize,
    group: [usize; 2],
}

/// Splits the masks of `r` into units of `by`, cropping each line into adjacent pieces so the
/// pieces tile the masks exactly.
fn units(r: &Rendered, mask: [f32; 2], by: By) -> Vec<(Unit, usize)> {
    let pad = r.pad;
    let n_lines = r.lines.len().saturating_sub(1);
    if by == By::Whole || r.glyphs.is_empty() || n_lines == 0 {
        return vec![(
            Unit {
                crop: [0.0, 0.0, mask[0], mask[1]],
                line: 0,
                group: [0; 2],
            },
            0,
        )];
    }
    let band = |l: usize| {
        let y0 = if l == 0 { 0.0 } else { pad + r.lines[l] };
        let y1 = if l + 1 >= n_lines {
            mask[1]
        } else {
            pad + r.lines[l + 1]
        };
        (y0, y1)
    };
    if by == By::Line {
        return (0..n_lines)
            .map(|l| {
                let (y0, y1) = band(l);
                (
                    Unit {
                        crop: [0.0, y0, mask[0], y1],
                        line: l,
                        group: [0; 2],
                    },
                    l,
                )
            })
            .collect();
    }
    // Pieces: characters, or words (a word keeps its trailing spaces).
    let mut pieces: Vec<(usize, f32)> = Vec::new();
    let mut prev: Option<&Glyph> = None;
    for g in &r.glyphs {
        let starts = match prev {
            None => true,
            Some(p) if p.line != g.line => true,
            Some(p) => by == By::Char || (p.space && !g.space),
        };
        if starts {
            pieces.push((g.line, pad + g.x[0]));
        }
        prev = Some(g);
    }
    (0..pieces.len())
        .map(|i| {
            let (line, x) = pieces[i];
            let first = i == 0 || pieces[i - 1].0 != line;
            let next = pieces.get(i + 1).filter(|p| p.0 == line);
            let (y0, y1) = band(line);
            (
                Unit {
                    crop: [
                        if first { 0.0 } else { x },
                        y0,
                        next.map_or(mask[0], |p| p.1),
                        y1,
                    ],
                    line,
                    group: [0; 2],
                },
                i,
            )
        })
        .collect()
}

/// The finest units both animations need, with each one's group index under each.
fn partition(r: &Rendered, mask: [f32; 2], by: [By; 2]) -> (Vec<Unit>, [usize; 2]) {
    let fine = by[0].min(by[1]);
    let base = units(r, mask, fine);
    let mut counts = [1usize; 2];
    let mut out: Vec<Unit> = base.iter().map(|(u, _)| *u).collect();
    for side in 0..2 {
        if by[side] == fine {
            for (o, (_, i)) in out.iter_mut().zip(&base) {
                o.group[side] = *i;
            }
            counts[side] = base.len();
            continue;
        }
        // Group fine units by the coarser unit containing their left edge.
        let coarse = units(r, mask, by[side]);
        for o in &mut out {
            let cx = f32::midpoint(o.crop[0], o.crop[2]);
            o.group[side] = coarse
                .iter()
                .find(|(c, _)| c.line == o.line && cx >= c.crop[0] && cx <= c.crop[2])
                .or_else(|| {
                    coarse
                        .iter()
                        .find(|(c, _)| by[side] == By::Whole || c.line == o.line)
                })
                .map_or(0, |(_, i)| *i);
        }
        counts[side] = coarse.len();
    }
    (out, counts)
}

/// Placement of a piece in one frame: scale `k` about `centre`, then offset `d` (raster px),
/// a crop fraction from a wipe, and an alpha.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Look {
    k: f32,
    d: [f32; 2],
    alpha: f32,
    /// Visible part along the wipe: `[start, end]` fractions of the crop, and its axis.
    wipe: Option<([f32; 2], bool)>,
}

const REST: Look = Look {
    k: 1.0,
    d: [0.0, 0.0],
    alpha: 1.0,
    wipe: None,
};

/// The look of group `g` (of `n`) at `t` seconds into a clip showing for `len` seconds.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::many_single_char_names
)]
fn look(anim: &Animation, g: [usize; 2], n: [usize; 2], t: f64, len: f64, fh: f32) -> Look {
    let mut l = REST;
    for (side, a) in [(0, &anim.in_), (1, &anim.out)] {
        let Some(a) = a else { continue };
        let s = a.stagger(n[side]);
        let (gi, ni) = (g[side] as f64, n[side] as f64);
        let raw = if a.kind == Kind::Typewriter {
            let shown = if side == 0 {
                t >= gi * s
            } else {
                // Backspacing: the last unit goes first.
                t < len - a.duration + (ni - 1.0 - gi) * s
            };
            if shown { 1.0 } else { 0.0 }
        } else {
            let rel = if side == 0 {
                t - gi * s
            } else {
                len - t - (ni - 1.0 - gi) * s
            };
            if a.duration > 0.0 {
                (rel / a.duration).clamp(0.0, 1.0)
            } else {
                1.0
            }
        };
        if raw >= 1.0 {
            continue;
        }
        let p = a.ease.at(raw) as f32;
        let raw = raw as f32;
        match a.kind {
            Kind::Fade => l.alpha *= p.clamp(0.0, 1.0),
            Kind::Typewriter => l.alpha *= raw,
            Kind::Slide => {
                let v = a.direction.vector();
                let sign = if side == 0 { -1.0 } else { 1.0 };
                let dist = sign * (1.0 - p) * a.distance * fh;
                l.d = [l.d[0] + v[0] * dist, l.d[1] + v[1] * dist];
                l.alpha *= raw;
            }
            Kind::Pop => {
                l.k *= a.scale + (1.0 - a.scale) * p;
                l.alpha *= raw;
            }
            Kind::Wipe => {
                let p = p.clamp(0.0, 1.0);
                let horizontal = matches!(a.direction, Dir::Left | Dir::Right | Dir::Center);
                // Exits keep the far side: the edge keeps travelling the same way.
                let span = match (a.direction, side) {
                    (Dir::Center, _) => [0.5 - p / 2.0, 0.5 + p / 2.0],
                    (Dir::Right | Dir::Down, 0) | (Dir::Left | Dir::Up, 1) => [0.0, p],
                    _ => [1.0 - p, 1.0],
                };
                let prev = l.wipe.map_or([0.0, 1.0], |w| w.0);
                l.wipe = Some(([prev[0].max(span[0]), prev[1].min(span[1])], horizontal));
            }
            Kind::None => {}
        }
    }
    l
}

/// Crops `src` (and the matching `dst`) to a wipe's visible span.
fn wiped(
    src: [f32; 4],
    dst: [f32; 4],
    w: Option<([f32; 2], bool)>,
) -> Option<([f32; 4], [f32; 4])> {
    let Some(([a, b], horizontal)) = w else {
        return Some((src, dst));
    };
    if b <= a {
        return None;
    }
    let cut = |r: [f32; 4], i: usize| {
        let len = r[i + 2] - r[i];
        let mut o = r;
        o[i] = r[i] + a * len;
        o[i + 2] = r[i] + b * len;
        o
    };
    let i = usize::from(!horizontal);
    Some((cut(src, i), cut(dst, i)))
}

/// Scales `r` by `k` about `c`, then moves it by `d`.
fn transform(r: [f32; 4], k: [f32; 2], c: [f32; 2], d: [f32; 2]) -> [f32; 4] {
    [
        c[0] + (r[0] - c[0]) * k[0] + d[0],
        c[1] + (r[1] - c[1]) * k[1] + d[1],
        c[0] + (r[2] - c[0]) * k[0] + d[0],
        c[1] + (r[3] - c[1]) * k[1] + d[1],
    ]
}

/// The timing of a clip in a segment, in frames relative to the segment's first frame.
#[derive(Debug, Clone, Copy)]
pub struct Span {
    /// First and end (exclusive) frame the text shows (after its `delay`).
    pub from: i64,
    pub to: i64,
    /// Frames in the segment.
    pub frames: i64,
    pub rate: f64,
    /// Seconds from the clip's start to `from` (keyframe times count from the clip's start).
    pub delay: f64,
}

/// `r`'s layers as overlays whose placements follow `anim` frame by frame. `r` was laid out at
/// `raster` times its size (see [`Animation::raster_scale`]) with its anchor on `a` (output
/// px); `base` is the text's `position` and `frame` the output size.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::many_single_char_names
)]
pub fn animate(
    r: &Rendered,
    anim: &Animation,
    raster: f32,
    a: [f32; 2],
    base: [f32; 2],
    frame: [f32; 2],
    span: Span,
) -> Vec<Overlay> {
    let [fw, fh] = frame;
    let Some(fill) = r
        .layers
        .iter()
        .find(|l| l.role != Role::Box)
        .or_else(|| r.layers.first())
    else {
        return Vec::new();
    };
    let at = fill.parts.first().map_or(r.origin, |p| p.pos);
    let mask = [fill.mask.width as f32, fill.mask.height as f32];
    let by = [
        anim.in_.map_or(By::Whole, |a| a.by),
        anim.out.map_or(By::Whole, |a| a.by),
    ];
    let solid = r.glyphs.is_empty();
    let (units, counts) = partition(r, mask, by);
    // Each group's centre (pops scale about it), raster px, from the fill's placement.
    let centre = |side: usize, g: usize| {
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for u in units.iter().filter(|u| u.group[side] == g) {
            b = [
                b[0].min(u.crop[0]),
                b[1].min(u.crop[1]),
                b[2].max(u.crop[2]),
                b[3].max(u.crop[3]),
            ];
        }
        [
            at[0] + f32::midpoint(b[0], b[2]),
            at[1] + f32::midpoint(b[1], b[3]),
        ]
    };
    // Pops scale about their own piece.
    let side = usize::from(
        anim.in_.is_none_or(|a| a.kind != Kind::Pop)
            && anim.out.is_some_and(|a| a.kind == Kind::Pop),
    );
    let centres: Vec<[f32; 2]> = units.iter().map(|u| centre(side, u.group[side])).collect();
    let box_centre = [r.origin[0] + r.size[0] / 2.0, r.origin[1] + r.size[1] / 2.0];
    let len = (span.to - span.from) as f64 / span.rate;
    let lo = span.from.max(0);
    let hi = span.to.min(span.frames).max(lo + 1);
    let mut frames: Vec<Vec<Vec<OverlayPart>>> = vec![Vec::new(); r.layers.len()];
    for f in lo..hi {
        let t = (f - span.from) as f64 / span.rate + 0.5 / span.rate;
        let kt = t + span.delay;
        let klen = len + span.delay;
        let g = anim
            .scale
            .at(kt, klen)
            .map_or([1.0, 1.0], |s| s.map(|v| v as f32));
        let g = [g[0] / raster, g[1] / raster];
        let move_ = anim.position.at(kt, klen).map_or([0.0, 0.0], |p| {
            [(p[0] as f32 - base[0]) * fw, (p[1] as f32 - base[1]) * fh]
        });
        let opacity = anim.opacity.at(kt, klen).map_or(1.0, |o| o[0] as f32);
        let colour = anim.colour.at(kt, klen).map(|c| c.map(|v| v as f32));
        let looks: Vec<Look> = units
            .iter()
            .map(|u| look(anim, u.group, counts, t, len, fh * raster))
            .collect();
        let whole = by == [By::Whole; 2];
        for (li, layer) in r.layers.iter().enumerate() {
            let tint = |p: &OverlayPart, alpha: f32| {
                let mut c = p.colour;
                if let Some(k) = colour
                    .filter(|_| layer.role == Role::Fill || (solid && layer.role == Role::Box))
                {
                    c = k;
                }
                c[3] *= alpha * opacity;
                c
            };
            let mut parts: Vec<OverlayPart> = Vec::new();
            for p in &layer.parts {
                if layer.role == Role::Box {
                    // The box (or a rect clip) moves with the text when it animates whole;
                    // otherwise it shows as soon as any piece does.
                    let (src, dst) = p.rects(&layer.mask);
                    let l = if whole || solid {
                        looks[0]
                    } else {
                        Look {
                            alpha: looks.iter().map(|l| l.alpha).fold(0.0, f32::max),
                            ..REST
                        }
                    };
                    let Some((src, dst)) = wiped(src, dst, l.wipe) else {
                        continue;
                    };
                    let dst = transform(dst, [l.k, l.k], box_centre, l.d);
                    let dst = transform(dst, g, a, move_);
                    let colour = tint(p, l.alpha);
                    if colour[3] > 1.0 / 512.0 {
                        parts.push(OverlayPart {
                            pos: [dst[0], dst[1]],
                            size: Some([dst[2] - dst[0], dst[3] - dst[1]]),
                            // A stretched 1x1 solid: the crop changes nothing.
                            src: p.src.map(|_| src),
                            colour,
                        });
                    }
                    continue;
                }
                // Merge neighbours on a line that look the same (typed text, settled words).
                let mut run: Option<(Look, usize, [f32; 4])> = None;
                let flush = |run: Option<(Look, usize, [f32; 4])>, parts: &mut Vec<OverlayPart>| {
                    let Some((l, ui, crop)) = run else { return };
                    let dst = [
                        p.pos[0] + crop[0],
                        p.pos[1] + crop[1],
                        p.pos[0] + crop[2],
                        p.pos[1] + crop[3],
                    ];
                    let Some((src, dst)) = wiped(crop, dst, l.wipe) else {
                        return;
                    };
                    let dst = transform(dst, [l.k, l.k], centres[ui], l.d);
                    let dst = transform(dst, g, a, move_);
                    let colour = tint(p, l.alpha);
                    if colour[3] > 1.0 / 512.0 {
                        parts.push(OverlayPart {
                            pos: [dst[0], dst[1]],
                            size: Some([dst[2] - dst[0], dst[3] - dst[1]]),
                            src: Some(src),
                            colour,
                        });
                    }
                };
                for (ui, (u, l)) in units.iter().zip(&looks).enumerate() {
                    match &mut run {
                        Some((rl, rui, crop))
                            if rl == l
                                && (l.k - 1.0).abs() < f32::EPSILON
                                && l.wipe.is_none()
                                && units[*rui].line == u.line
                                && (crop[2] - u.crop[0]).abs() < 1e-3 =>
                        {
                            crop[2] = u.crop[2];
                        }
                        _ => {
                            flush(run.take(), &mut parts);
                            run = Some((*l, ui, u.crop));
                        }
                    }
                }
                flush(run, &mut parts);
            }
            frames[li].push(parts);
        }
    }
    r.layers
        .iter()
        .zip(frames)
        .map(|(l, frames)| Overlay {
            mask: Arc::clone(&l.mask),
            parts: frames.last().cloned().unwrap_or_default(),
            from: span.from,
            to: span.to,
            fade_in: 0.0,
            fade_out: 0.0,
            motion: Some(Arc::new(Motion { first: lo, frames })),
        })
        .collect()
}

/// A filled rectangle `size` px with its `anchor` on `base` (a `rect` clip), as a layout.
pub fn rect(
    size: [f32; 2],
    base: [f32; 2],
    anchor: [f32; 2],
    frame: [f32; 2],
    colour: [f32; 4],
) -> Rendered {
    let a = [(base[0] + 0.5) * frame[0], (base[1] + 0.5) * frame[1]];
    let origin = [a[0] - anchor[0] * size[0], a[1] - anchor[1] * size[1]];
    Rendered {
        layers: vec![crate::layout::Layer {
            mask: Arc::new(Mask::solid()),
            parts: vec![OverlayPart {
                pos: origin,
                size: Some(size),
                src: None,
                colour,
            }],
            role: Role::Box,
        }],
        origin,
        size,
        pad: 0.0,
        lines: vec![0.0, size[1]],
        glyphs: Vec::new(),
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]
mod tests {
    use super::*;
    use crate::layout::Layer;
    use serde_json::json;

    #[allow(clippy::needless_pass_by_value)]
    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    /// A 100x20 text box at (50, 40) of five 20 px "glyphs", `ab cd` (one line), opaque.
    fn text_box() -> Rendered {
        let mask = Mask {
            width: 100,
            height: 20,
            data: vec![255; 2000],
        };
        let glyphs = (0..5)
            .map(|i| Glyph {
                line: 0,
                x: [i as f32 * 20.0, i as f32 * 20.0 + 20.0],
                space: i == 2,
            })
            .collect();
        Rendered {
            layers: vec![Layer {
                mask: Arc::new(mask),
                parts: vec![OverlayPart {
                    pos: [50.0, 40.0],
                    colour: [1.0; 4],
                    ..OverlayPart::default()
                }],
                role: Role::Fill,
            }],
            origin: [50.0, 40.0],
            size: [100.0, 20.0],
            pad: 0.0,
            lines: vec![0.0, 20.0],
            glyphs,
        }
    }

    /// Alpha of a 200x100 frame with `ov` drawn (nearest sampling): the CPU reference of the
    /// GPU pass, for checking frames.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn coverage(ov: &[Overlay], frame: i64) -> Vec<f32> {
        let (w, h) = (200usize, 100usize);
        let mut out = vec![0.0f32; w * h];
        for o in ov {
            let fade = o.opacity(frame);
            for p in o.parts_at(frame) {
                let (s, d) = p.rects(&o.mask);
                for y in 0..h {
                    for x in 0..w {
                        let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                        if cx < d[0] || cx >= d[2] || cy < d[1] || cy >= d[3] {
                            continue;
                        }
                        let u = s[0] + (cx - d[0]) / (d[2] - d[0]) * (s[2] - s[0]);
                        let v = s[1] + (cy - d[1]) / (d[3] - d[1]) * (s[3] - s[1]);
                        let m = o.mask.data[(v as usize).min(o.mask.height as usize - 1)
                            * o.mask.width as usize
                            + (u as usize).min(o.mask.width as usize - 1)];
                        let a = f32::from(m) / 255.0 * p.colour[3] * fade;
                        let px = &mut out[y * w + x];
                        *px = *px + a * (1.0 - *px);
                    }
                }
            }
        }
        out
    }

    fn span(frames: i64) -> Span {
        Span {
            from: 0,
            to: frames,
            frames,
            rate: 10.0,
            delay: 0.0,
        }
    }

    fn run(params: Value, frames: i64) -> Vec<Overlay> {
        let a = Animation::parse(&obj(params));
        animate(
            &text_box(),
            &a,
            1.0,
            [100.0, 50.0],
            [0.0, 0.0],
            [200.0, 100.0],
            span(frames),
        )
    }

    #[test]
    fn eases() {
        for e in [
            Ease::Linear,
            Ease::In,
            Ease::Out,
            Ease::InOut,
            Ease::Ease,
            Ease::Back,
        ] {
            assert!(
                e.at(0.0).abs() < 1e-9 && (e.at(1.0) - 1.0).abs() < 1e-9,
                "{e:?}"
            );
        }
        assert!(Ease::Back.at(0.7) > 1.0, "overshoots");
        assert!(Ease::Out.at(0.5) > 0.5 && Ease::In.at(0.5) < 0.5);
        assert!((Ease::Bezier([0.0, 0.0, 1.0, 1.0]).at(0.3) - 0.3).abs() < 1e-4);
        assert_eq!(
            Ease::parse(&json!([0.4, 0, 0.2, 1])),
            Some(Ease::Bezier([0.4, 0.0, 0.2, 1.0]))
        );
        assert!(Ease::Hold.at(0.99).abs() < 1e-9);
    }

    #[test]
    fn keyframes_interpolate_and_count_from_the_end() {
        let k: Keys<2> = Keys::parse(Some(&json!([
            {"t": 0, "v": [0.0, 0.0], "interp": "linear"},
            {"t": {"value": 24, "rate": 24}, "v": [1.0, 2.0]},
            {"t": -1, "v": [3.0, 3.0]},
        ])));
        assert_eq!(k.at(-1.0, 10.0), Some([0.0, 0.0]));
        assert_eq!(k.at(0.5, 10.0), Some([0.5, 1.0]));
        assert_eq!(k.at(5.0, 10.0), Some([2.0, 2.5]));
        assert_eq!(k.at(20.0, 10.0), Some([3.0, 3.0]));
        let s: Keys<2> = Keys::parse(Some(&json!([{"t": 0, "v": 2}])));
        assert_eq!(s.at(0.0, 1.0), Some([2.0, 2.0]));
    }

    #[test]
    fn parses_and_tells_plain_fades() {
        let a = Animation::parse(&obj(json!({"fade_in": 0.5})));
        assert!(a.is_plain_fade());
        let a = Animation::parse(&obj(
            json!({"animation": {"in": {"kind": "typewriter", "duration": 1}}}),
        ));
        assert!(!a.is_plain_fade());
        let t = a.in_.unwrap();
        assert_eq!((t.kind, t.by), (Kind::Typewriter, By::Char));
        let a = Animation::parse(&obj(json!({"animation": {"in": {"kind": "rotate3d"}}})));
        assert_eq!(a.in_.map(|a| a.kind), Some(Kind::Fade));
        let a = Animation::parse(&obj(
            json!({"keyframes": {"scale": [{"t": 0, "v": 1}, {"t": 1, "v": 2.5}]}}),
        ));
        assert!((a.raster_scale() - 2.5).abs() < 1e-6);
    }

    #[test]
    fn typewriter_types_left_to_right_and_merges() {
        // 5 characters over 1 s at 10 fps: one every 2 frames.
        let ov = run(
            json!({"animation": {"in": {"kind": "typewriter", "duration": 1}}}),
            30,
        );
        let c = coverage(&ov, 4);
        let row = |c: &[f32], x: usize| c[50 * 200 + x];
        // Frame 4 (0.45 s): 3 characters typed: x 50..110 shown, 110.. not yet.
        assert!(row(&c, 55) > 0.99 && row(&c, 105) > 0.99);
        assert!(row(&c, 115) < 0.01);
        // Typed characters are one part, not one each.
        assert_eq!(ov[0].parts_at(4).len(), 1);
        assert!(row(&coverage(&ov, 20), 145) > 0.99);
    }

    #[test]
    fn slide_by_word_staggers() {
        let ov = run(
            json!({"animation": {"in": {"kind": "slide", "by": "word", "duration": 1, "stagger": 0.5}}}),
            30,
        );
        assert_eq!(ov[0].parts_at(2).len(), 1, "the second word not yet");
        let p = ov[0].parts_at(7);
        assert_eq!(p.len(), 2, "two words");
        // The first word is further along: higher up (sliding up) and more opaque.
        assert!(p[0].pos[1] < p[1].pos[1]);
        assert!(p[0].colour[3] > p[1].colour[3]);
        assert!(p[0].pos[1] > 40.0, "still below its place");
        // Settled: whole again, in place.
        let p = ov[0].parts_at(20);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].pos, [50.0, 40.0]);
    }

    #[test]
    fn pop_scales_about_the_centre_and_overshoots() {
        let ov = run(
            json!({"animation": {"in": {"kind": "pop", "duration": 1}}}),
            30,
        );
        let size = |f: i64| ov[0].parts_at(f)[0].size.unwrap();
        let centre = |f: i64| {
            let p = ov[0].parts_at(f)[0];
            p.pos[0] + p.size.unwrap()[0] / 2.0
        };
        assert!(size(1)[0] < 100.0);
        assert!(size(7)[0] > 100.0, "overshoot");
        assert!((centre(1) - 100.0).abs() < 1e-3);
        assert!((size(15)[0] - 100.0).abs() < 1e-3);
    }

    #[test]
    fn wipe_and_exit() {
        let ov = run(
            json!({"animation": {"in": {"kind": "wipe", "duration": 1, "ease": "linear"},
                                 "out": {"kind": "slide", "direction": "down", "duration": 1}}}),
            30,
        );
        let p = ov[0].parts_at(4)[0];
        assert_eq!(p.src.map(|s| s[2]), Some(45.0));
        assert_eq!(p.size.map(|s| s[0]), Some(45.0));
        let p = ov[0].parts_at(25)[0];
        assert!(p.pos[1] > 40.0 && p.colour[3] < 1.0, "leaving downwards");
    }

    #[test]
    fn keyframes_move_scale_and_tint() {
        let a = Animation::parse(&obj(json!({"keyframes": {
            "position": [{"t": 0, "v": [0.0, 0.0]}, {"t": 1, "v": [0.25, 0.0]}],
            "scale": [{"t": 0, "v": 1}, {"t": 1, "v": 2}],
            "opacity": [{"t": 0, "v": 0.5}],
            "colour": [{"t": 0, "v": [1, 0, 0]}],
        }})));
        let r = text_box();
        // Laid out at raster 2: the box is twice the size around the anchor.
        let mut big = r.clone();
        big.layers[0].parts[0].pos = [0.0, 30.0];
        big.layers[0].parts[0].src = None;
        big.layers[0].mask = Arc::new(Mask {
            width: 200,
            height: 40,
            data: vec![255; 8000],
        });
        let ov = animate(
            &big,
            &a,
            2.0,
            [100.0, 50.0],
            [0.0, 0.0],
            [200.0, 100.0],
            span(20),
        );
        let p = ov[0].parts_at(0)[0];
        assert!((p.size.unwrap()[0] - 105.0).abs() < 1e-3, "{p:?}");
        let p = ov[0].parts_at(15)[0];
        // Scale 2 about the anchor (100, 50), moved a quarter frame right.
        assert!((p.size.unwrap()[0] - 200.0).abs() < 1e-3);
        assert!((p.pos[0] - 50.0).abs() < 1e-3);
        assert_eq!(p.colour, [1.0, 0.0, 0.0, 0.5]);
        let _ = r;
    }

    #[test]
    fn rect_wipes_from_the_centre() {
        let r = rect(
            [100.0, 4.0],
            [0.0, 0.0],
            [0.5, 0.5],
            [200.0, 100.0],
            [1.0; 4],
        );
        let a = Animation::parse(&obj(
            json!({"animation": {"in": {"kind": "wipe", "direction": "center", "duration": 1, "ease": "linear"}}}),
        ));
        let ov = animate(
            &r,
            &a,
            1.0,
            [100.0, 50.0],
            [0.0, 0.0],
            [200.0, 100.0],
            span(20),
        );
        let p = ov[0].parts_at(4)[0];
        assert_eq!(p.pos, [77.5, 48.0]);
        assert_eq!(p.size, Some([45.0, 4.0]));
        assert_eq!(p.src, None);
    }

    #[test]
    fn keyframes_take_expressions() {
        let o: Keys<1> = Keys::parse(Some(&json!("lt / dur")));
        assert_eq!(o.at(2.5, 10.0), Some([0.25]));
        let p: Keys<2> = Keys::parse(Some(&json!([0.1, "0.3 - 0.05 * lt"])));
        let v = p.at(2.0, 10.0).unwrap();
        assert!((v[0] - 0.1).abs() < 1e-12 && (v[1] - 0.2).abs() < 1e-12);
        let k: Keys<1> = Keys::parse(Some(&json!({
            "keys": [{"t": 0, "v": 0}, {"t": -1, "v": 1}], "expr": "value * 0.5"
        })));
        assert_eq!(k.at(4.5, 10.0), Some([0.25]));
        // An expression that doesn't compile is dropped.
        assert!(Keys::<1>::parse(Some(&json!("st * 2"))).is_empty());
    }
}
