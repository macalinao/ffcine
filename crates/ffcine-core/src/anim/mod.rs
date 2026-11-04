//! Param tracks (`docs/compositing.md#param-tracks`): one type for every numeric, vector and
//! colour parameter, a constant, keyframes with easing, or an [expression](expr).
//!
//! A [`Track`] is parsed once from its JSON form against a [`Clock`] (what a key's `t` means)
//! and a [`Scope`] (the names an expression may use), then evaluated per frame on a [`Frame`]
//! or sampled over a range of frames with an [`Env`]. Everything here is a pure function of the
//! frame: no state, no GPU, no libav.

pub mod expr;
mod vars;

use std::f64::consts::PI;
use std::ops::Range;

use serde_json::{Map, Value};

use crate::{Error, Result};
pub use expr::{Compiled, ExprError};
pub use vars::Vars;

/// The shape of a move over its progress `u` (0..1): a key's `interp` (alias `ease`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Ease {
    #[default]
    Linear,
    /// Keeps this key's value until the next key.
    Hold,
    /// `0.5 - 0.5 cos(pi u)` (reframe's `ease`).
    Ease,
    /// Cubic, accelerating.
    In,
    /// Cubic, decelerating.
    Out,
    /// Cubic ease in and out.
    InOut,
    /// Decelerates past the end and settles back (overshoot).
    Back,
    /// `3u² − 2u³` (the audio schemas' `ease`).
    Smoothstep,
    /// CSS `cubic-bezier(x1, y1, x2, y2)`.
    Bezier([f64; 4]),
}

impl Ease {
    /// A name: `linear`, `hold`, `ease`, `in`, `out`, `in_out`, `back`, `smoothstep` (and the
    /// aliases `ease_in`, `ease_out`, `ease_in_out`, `overshoot`).
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "linear" => Self::Linear,
            "hold" => Self::Hold,
            "ease" => Self::Ease,
            "in" | "ease_in" => Self::In,
            "out" | "ease_out" => Self::Out,
            "in_out" | "ease_in_out" => Self::InOut,
            "back" | "overshoot" => Self::Back,
            "smoothstep" => Self::Smoothstep,
            _ => return None,
        })
    }

    /// A name or `[x1, y1, x2, y2]`.
    pub fn parse(v: &Value) -> Option<Self> {
        if let Some(a) = v.as_array() {
            let c: Vec<f64> = a.iter().filter_map(Value::as_f64).collect();
            return match c[..] {
                [x1, y1, x2, y2] if c.len() == a.len() => Some(Self::Bezier([x1, y1, x2, y2])),
                _ => None,
            };
        }
        Self::from_name(v.as_str()?)
    }

    /// Progress at `u`, clamped to 0..1 (`Back` overshoots 1 on the way).
    pub fn at(self, u: f64) -> f64 {
        let u = u.clamp(0.0, 1.0);
        match self {
            Self::Linear => u,
            Self::Hold => {
                if u >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Ease => 0.5 - 0.5 * (PI * u).cos(),
            Self::In => u * u * u,
            Self::Out => 1.0 - (1.0 - u).powi(3),
            Self::InOut if u < 0.5 => 4.0 * u * u * u,
            Self::InOut => 1.0 - (-2.0 * u + 2.0).powi(3) / 2.0,
            Self::Back => {
                let (c1, c3) = (1.701_58, 2.701_58);
                1.0 + c3 * (u - 1.0).powi(3) + c1 * (u - 1.0).powi(2)
            }
            Self::Smoothstep => u * u * (3.0 - 2.0 * u),
            Self::Bezier(c) => bezier(c, u),
        }
    }
}

/// `y` of a CSS cubic bezier at `x` (bisection on the curve's `x`).
fn bezier([x1, y1, x2, y2]: [f64; 4], x: f64) -> f64 {
    let curve = |a: f64, b: f64, t: f64| {
        3.0 * a * t * (1.0 - t).powi(2) + 3.0 * b * t * t * (1.0 - t) + t.powi(3)
    };
    let (mut lo, mut hi, mut t) = (0.0, 1.0, x);
    for _ in 0..32 {
        let v = curve(x1, x2, t) - x;
        if v.abs() < 1e-7 {
            break;
        }
        if v > 0.0 {
            hi = t;
        } else {
            lo = t;
        }
        t = f64::midpoint(lo, hi);
    }
    curve(y1, y2, t)
}

/// A value that can be keyframed: `f64`, `[f64; N]` (vectors, colours).
pub trait Animatable: Copy + std::fmt::Debug + PartialEq {
    /// Components.
    const N: usize;
    fn lerp(a: Self, b: Self, k: f64) -> Self;
    fn component(self, i: usize) -> f64;
    fn from_fn(f: impl FnMut(usize) -> f64) -> Self;

    /// A number (`N` = 1), an array of `N` numbers, or (`N` = 3 or 4) a colour name.
    fn from_json(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) if Self::N == 1 => n.as_f64().map(|x| Self::from_fn(|_| x)),
            Value::Array(a) if a.len() == Self::N => {
                let c: Vec<f64> = a.iter().map(Value::as_f64).collect::<Option<_>>()?;
                Some(Self::from_fn(|i| c[i]))
            }
            Value::String(s) if Self::N == 3 || Self::N == 4 => {
                colour(s).map(|c| Self::from_fn(|i| c[i]))
            }
            _ => None,
        }
    }
}

impl Animatable for f64 {
    const N: usize = 1;
    fn lerp(a: Self, b: Self, k: f64) -> Self {
        a + (b - a) * k
    }
    fn component(self, _: usize) -> f64 {
        self
    }
    fn from_fn(mut f: impl FnMut(usize) -> f64) -> Self {
        f(0)
    }
}

impl<const N: usize> Animatable for [f64; N] {
    const N: usize = N;
    fn lerp(a: Self, b: Self, k: f64) -> Self {
        std::array::from_fn(|i| a[i] + (b[i] - a[i]) * k)
    }
    fn component(self, i: usize) -> f64 {
        self[i]
    }
    fn from_fn(f: impl FnMut(usize) -> f64) -> Self {
        std::array::from_fn(f)
    }
}

/// `black`, `white` or `#rrggbb[aa]` as sRGB 0..1 with alpha.
pub fn colour(s: &str) -> Option<[f64; 4]> {
    match s {
        "black" => return Some([0.0, 0.0, 0.0, 1.0]),
        "white" => return Some([1.0; 4]),
        _ => {}
    }
    let h = s.strip_prefix('#')?;
    if !(h.len() == 6 || h.len() == 8) || !h.is_ascii() {
        return None;
    }
    let byte = |i: usize| {
        u8::from_str_radix(&h[i..i + 2], 16)
            .ok()
            .map(|b| f64::from(b) / 255.0)
    };
    Some([
        byte(0)?,
        byte(2)?,
        byte(4)?,
        if h.len() == 8 { byte(6)? } else { 1.0 },
    ])
}

/// A keyframe: value `v` at `t` on the track's [`Clock`], `ease` shaping the segment to the
/// next key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Key<T> {
    pub t: f64,
    pub v: T,
    pub ease: Ease,
}

/// The value of sorted keys at `t`: held before the first and after the last.
pub fn keys_at<T: Animatable>(keys: &[Key<T>], t: f64) -> Option<T> {
    let first = keys.first()?;
    if t <= first.t {
        return Some(first.v);
    }
    // The last key at or before t.
    let i = keys.partition_point(|k| k.t <= t) - 1;
    let a = &keys[i];
    let Some(b) = keys.get(i + 1) else {
        return Some(a.v);
    };
    let span = b.t - a.t;
    if span <= 0.0 {
        return Some(b.v);
    }
    Some(T::lerp(a.v, b.v, a.ease.at((t - a.t) / span)))
}

/// What a key's `t` means, and which variables an expression sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    /// The clip's source time, s (timeline reframe, transform, crop, shader and audio keys).
    /// Expressions see `st`.
    Source,
    /// Seconds from the clip's start, negative from its end (timeline text keys). Expressions
    /// see `lt` and `dur`.
    Clip,
    /// Seconds from a comp layer's `in`, negative from its `out`. Expressions see every
    /// variable.
    Layer,
}

macro_rules! builtins {
    ($($v:ident $name:literal,)*) => {
        /// The variables every frame defines (`docs/compositing.md#expressions`).
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u16)]
        pub enum Builtin { $($v,)* }
        impl Builtin {
            pub const ALL: &[Builtin] = &[$(Self::$v,)*];
            pub const COUNT: usize = Self::ALL.len();
            pub fn name(self) -> &'static str {
                match self { $(Self::$v => $name,)* }
            }
            pub fn from_name(s: &str) -> Option<Self> {
                match s { $($name => Some(Self::$v),)* _ => None }
            }
        }
    };
}

builtins! {
    T "t", Lt "lt", St "st", N "n", Ln "ln", Fps "fps", CanvasW "W", CanvasH "H",
    W "w", H "h", Sw "sw", Sh "sh", In "in", Out "out", Dur "dur", Total "T", Value "value",
}

impl Builtin {
    /// Whether an expression on `clock` may read it.
    fn on(self, clock: Clock) -> bool {
        match clock {
            Clock::Layer => true,
            Clock::Source => self == Self::St,
            Clock::Clip => matches!(self, Self::Lt | Self::Dur),
        }
    }
}

/// The names an expression compiles against: the clock's built-ins, `value` in the
/// `{keys, expr}` form, and [`Vars`].
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub clock: Clock,
    pub value: bool,
    pub vars: Option<&'a Vars>,
}

impl<'a> Scope<'a> {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            value: false,
            vars: None,
        }
    }

    pub fn with_vars(clock: Clock, vars: &'a Vars) -> Self {
        Self {
            clock,
            value: false,
            vars: Some(vars),
        }
    }

    /// The slot of `name`: a built-in's index, or `Builtin::COUNT` + a var's index.
    pub fn slot(&self, name: &str) -> Option<u16> {
        #[allow(clippy::cast_possible_truncation)]
        if let Some(b) = Builtin::from_name(name) {
            let ok = if b == Builtin::Value {
                self.value
            } else {
                b.on(self.clock)
            };
            return ok.then_some(b as u16);
        }
        #[allow(clippy::cast_possible_truncation)]
        self.vars?.slot(name).map(|i| (Builtin::COUNT + i) as u16)
    }

    pub(crate) fn unknown(&self, name: &str) -> String {
        match Builtin::from_name(name) {
            Some(Builtin::Value) => {
                "`value` is only defined in the {\"keys\", \"expr\"} form".into()
            }
            Some(_) => format!(
                "`{name}` isn't known on this parameter's clock ({:?})",
                self.clock
            ),
            None => format!("unknown variable `{name}`"),
        }
    }
}

/// One frame's variables: the built-ins (by [`Builtin`] index) and the var values.
#[derive(Debug, Clone)]
pub struct Frame<'a> {
    pub b: [f64; Builtin::COUNT],
    pub vars: &'a [f64],
}

impl Frame<'_> {
    /// A frame with only the clock's time known: `st` for [`Clock::Source`], `lt` (and `dur`)
    /// otherwise.
    pub fn at(clock: Clock, t: f64, dur: f64) -> Frame<'static> {
        let mut b = [0.0; Builtin::COUNT];
        match clock {
            Clock::Source => b[Builtin::St as usize] = t,
            Clock::Clip | Clock::Layer => b[Builtin::Lt as usize] = t,
        }
        b[Builtin::Dur as usize] = dur;
        Frame { b, vars: &[] }
    }

    pub fn get(&self, slot: u16) -> f64 {
        let s = usize::from(slot);
        match self.b.get(s) {
            Some(v) => *v,
            None => self
                .vars
                .get(s - Builtin::COUNT)
                .copied()
                .unwrap_or(f64::NAN),
        }
    }

    pub fn var(&self, b: Builtin) -> f64 {
        self.b[b as usize]
    }

    /// The time a key's `t` is compared with.
    fn key_time(&self, clock: Clock) -> f64 {
        match clock {
            Clock::Source => self.var(Builtin::St),
            Clock::Clip | Clock::Layer => self.var(Builtin::Lt),
        }
    }
}

/// What a range of frames is sampled in: the frame `n` (on the comp's or timeline's rate) maps
/// to `t = n / fps`, `lt = t − in`, `st = source_in + lt`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Env<'a> {
    pub fps: f64,
    /// Canvas size, px.
    pub canvas: [f64; 2],
    /// Layer box, px.
    pub size: [f64; 2],
    /// Source display size, px.
    pub source: [f64; 2],
    pub in_: f64,
    pub out: f64,
    /// Comp duration, s.
    pub total: f64,
    pub source_in: f64,
    pub vars: Option<&'a Vars>,
}

impl Env<'_> {
    /// The built-ins on frame `n`.
    #[allow(clippy::cast_precision_loss)]
    pub fn builtins(&self, n: i64) -> [f64; Builtin::COUNT] {
        let fps = if self.fps > 0.0 { self.fps } else { 24.0 };
        let t = n as f64 / fps;
        let lt = t - self.in_;
        let mut b = [0.0; Builtin::COUNT];
        for (k, v) in [
            (Builtin::T, t),
            (Builtin::Lt, lt),
            (Builtin::St, self.source_in + lt),
            (Builtin::N, n as f64),
            (Builtin::Ln, n as f64 - (self.in_ * fps).round()),
            (Builtin::Fps, fps),
            (Builtin::CanvasW, self.canvas[0]),
            (Builtin::CanvasH, self.canvas[1]),
            (Builtin::W, self.size[0]),
            (Builtin::H, self.size[1]),
            (Builtin::Sw, self.source[0]),
            (Builtin::Sh, self.source[1]),
            (Builtin::In, self.in_),
            (Builtin::Out, self.out),
            (Builtin::Dur, self.out - self.in_),
            (Builtin::Total, self.total),
        ] {
            b[k as usize] = v;
        }
        b
    }
}

/// A parameter: a constant, keyframes, an expression per component, or an expression over its
/// own keys (`value`).
#[derive(Debug, Clone, PartialEq)]
pub enum Track<T> {
    Const(T),
    /// Sorted by `t`; held before the first and after the last.
    Keys(Vec<Key<T>>),
    /// One per component.
    Expr(Vec<Compiled>),
    /// An expression per component over its own keys (`value`).
    KeysExpr(Vec<Key<T>>, Vec<Compiled>),
    /// Per frame, from the range's first frame (internal: exact lowering).
    Sampled(Vec<T>),
}

impl<T: Default> Default for Track<T> {
    fn default() -> Self {
        Self::Const(T::default())
    }
}

/// Whether `v` is the keyframe form (an array of `{t, v}` objects).
pub fn is_keys(v: &Value) -> bool {
    v.as_array()
        .and_then(|a| a.first())
        .is_some_and(|k| k.get("t").is_some() && k.get("v").is_some())
}

/// Seconds from a `RationalTime` (`{rate, value}`) or a plain number.
pub fn seconds(v: &Value) -> Option<f64> {
    if let Some(s) = v.as_f64() {
        return Some(s);
    }
    let rate = v.get("rate")?.as_f64()?;
    let value = v.get("value")?.as_f64()?;
    (rate > 0.0).then(|| value / rate)
}

fn invalid(what: &str, v: &Value) -> Error {
    Error::Track {
        path: what.to_owned(),
        msg: format!("invalid value {v}"),
    }
}

fn compile(src: &str, scope: &Scope, what: &str) -> Result<Compiled> {
    Compiled::new(src, scope).map_err(|e| Error::Expr {
        path: what.to_owned(),
        col: e.col,
        msg: e.msg,
        src: src.to_owned(),
    })
}

/// Sorts keys by `t` (stable for equal times).
fn sorted<T>(mut keys: Vec<Key<T>>) -> Vec<Key<T>> {
    keys.sort_by(|a, b| a.t.total_cmp(&b.t));
    keys
}

impl<T: Animatable> Track<T> {
    /// A constant, keys, an expression string (`N` = 1), an array of `N` constants or
    /// expressions, or `{"keys": [...], "expr": ...}`. `what` names the parameter in errors.
    pub fn parse(v: &Value, clock: Clock, what: &str) -> Result<Self> {
        Self::parse_in(v, &Scope::new(clock), what)
    }

    /// [`Track::parse`] against `scope` (its clock and vars).
    pub fn parse_in(v: &Value, scope: &Scope, what: &str) -> Result<Self> {
        if let Some(c) = T::from_json(v) {
            return Ok(Self::Const(c));
        }
        if is_keys(v) {
            return Ok(Self::Keys(Self::parse_keys(v, what)?));
        }
        let exprs = match v {
            Value::String(s) if T::N == 1 => vec![compile(s, scope, what)?],
            Value::Array(a) if a.len() == T::N => a
                .iter()
                .enumerate()
                .map(|(i, c)| match c {
                    Value::Number(n) => n
                        .as_f64()
                        .map(Compiled::constant)
                        .ok_or_else(|| invalid(what, v)),
                    Value::String(s) => compile(s, scope, &format!("{what}[{i}]")),
                    _ => Err(invalid(what, v)),
                })
                .collect::<Result<_>>()?,
            Value::Object(o) if o.contains_key("keys") && o.contains_key("expr") => {
                return Self::parse_keys_expr(o, scope, what);
            }
            _ => return Err(invalid(what, v)),
        };
        Ok(Self::from_exprs(exprs))
    }

    fn from_exprs(exprs: Vec<Compiled>) -> Self {
        match exprs
            .iter()
            .map(Compiled::as_const)
            .collect::<Option<Vec<_>>>()
        {
            Some(c) => Self::Const(T::from_fn(|i| c[i])),
            None => Self::Expr(exprs),
        }
    }

    fn parse_keys_expr(o: &Map<String, Value>, scope: &Scope, what: &str) -> Result<Self> {
        let kv = &o["keys"];
        if !is_keys(kv) {
            return Err(invalid(&format!("{what}.keys"), kv));
        }
        let keys = Self::parse_keys(kv, what)?;
        let s = Scope {
            value: true,
            ..*scope
        };
        let what = format!("{what}.expr");
        let exprs = match &o["expr"] {
            Value::String(e) => {
                let c = compile(e, &s, &what)?;
                vec![c; T::N]
            }
            Value::Array(a) if a.len() == T::N => a
                .iter()
                .enumerate()
                .map(|(i, e)| match e {
                    Value::String(e) => compile(e, &s, &format!("{what}[{i}]")),
                    Value::Number(n) => n
                        .as_f64()
                        .map(Compiled::constant)
                        .ok_or_else(|| invalid(&what, e)),
                    _ => Err(invalid(&what, e)),
                })
                .collect::<Result<_>>()?,
            e => return Err(invalid(&what, e)),
        };
        Ok(Self::KeysExpr(keys, exprs))
    }

    fn parse_keys(v: &Value, what: &str) -> Result<Vec<Key<T>>> {
        let mut keys = Vec::new();
        for k in v.as_array().into_iter().flatten() {
            let ease = match k.get("interp").or_else(|| k.get("ease")) {
                None => Ease::Linear,
                Some(e) => Ease::parse(e).ok_or_else(|| invalid(what, v))?,
            };
            keys.push(Key {
                t: k.get("t")
                    .and_then(seconds)
                    .ok_or_else(|| invalid(what, v))?,
                v: k.get("v")
                    .and_then(T::from_json)
                    .ok_or_else(|| invalid(what, v))?,
                ease,
            });
        }
        Ok(sorted(keys))
    }

    /// Keys in any order (sorted here, stable for equal times).
    pub fn from_keys(keys: Vec<Key<T>>) -> Self {
        Self::Keys(sorted(keys))
    }

    /// Whether the value can change from frame to frame.
    pub fn is_animated(&self) -> bool {
        match self {
            Self::Const(_) => false,
            Self::Keys(k) => k.len() > 1,
            Self::Sampled(s) => s.windows(2).any(|w| w[0] != w[1]),
            Self::Expr(..) | Self::KeysExpr(..) => true,
        }
    }

    /// The keys, if keyframed.
    pub fn keys(&self) -> Option<&[Key<T>]> {
        match self {
            Self::Keys(k) | Self::KeysExpr(k, ..) => Some(k),
            _ => None,
        }
    }

    fn keyed(keys: &[Key<T>], clock: Clock, f: &Frame) -> Option<T> {
        let t = f.key_time(clock);
        if clock == Clock::Source || keys.iter().all(|k| k.t >= 0.0) {
            return keys_at(keys, t);
        }
        // Negative times count from the end.
        let dur = f.var(Builtin::Dur);
        let k: Vec<Key<T>> = keys
            .iter()
            .map(|k| Key {
                t: if k.t < 0.0 { dur + k.t } else { k.t },
                ..*k
            })
            .collect();
        keys_at(&sorted(k), t)
    }

    /// The value on `frame`, `None` for empty keys; an error if an expression isn't finite.
    /// `Sampled` reads `frame`'s `n`.
    pub fn try_eval(&self, clock: Clock, frame: &Frame) -> Result<Option<T>> {
        let finite = |v: f64, c: &Compiled| {
            if v.is_finite() {
                Ok(v)
            } else {
                Err(Error::Track {
                    path: c.source().to_owned(),
                    msg: format!(
                        "not finite ({v}) at t = {}, lt = {}, st = {}, n = {}",
                        frame.var(Builtin::T),
                        frame.var(Builtin::Lt),
                        frame.var(Builtin::St),
                        frame.var(Builtin::N)
                    ),
                })
            }
        };
        let comps = |e: &[Compiled], f: &Frame| -> Result<T> {
            let mut out = [0.0; 4];
            for (i, c) in e.iter().enumerate().take(4) {
                out[i] = finite(c.eval(f), c)?;
            }
            Ok(T::from_fn(|i| out[i]))
        };
        Ok(match self {
            Self::Const(v) => Some(*v),
            Self::Keys(k) => Self::keyed(k, clock, frame),
            Self::Expr(e) => Some(comps(e, frame)?),
            Self::KeysExpr(k, e) => {
                let Some(v) = Self::keyed(k, clock, frame) else {
                    return Ok(None);
                };
                let mut out = [0.0; 4];
                for (i, c) in e.iter().enumerate().take(4) {
                    let mut f = frame.clone();
                    f.b[Builtin::Value as usize] = v.component(i);
                    out[i] = finite(c.eval(&f), c)?;
                }
                Some(T::from_fn(|i| out[i]))
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Self::Sampled(s) => {
                let n = frame.var(Builtin::N).max(0.0) as usize;
                s.get(n).or(s.last()).copied()
            }
        })
    }

    /// The value on `frame`, keys on [`Clock::Layer`] (`lt`).
    pub fn eval(&self, frame: &Frame) -> Result<T> {
        self.try_eval(Clock::Layer, frame)?
            .ok_or_else(|| Error::Track {
                path: String::new(),
                msg: "no keys".into(),
            })
    }

    /// The value at `t` on `clock` when only that time is known (timeline effects), `None`
    /// for empty keys or a non-finite expression.
    pub fn at(&self, clock: Clock, t: f64, dur: f64) -> Option<T> {
        self.try_eval(clock, &Frame::at(clock, t, dur))
            .ok()
            .flatten()
    }

    /// The values on `frames` in `env` (vars evaluated per frame). `Sampled` tracks index from
    /// the range's start.
    pub fn sample(&self, clock: Clock, frames: Range<i64>, env: &Env) -> Result<Vec<T>> {
        let start = frames.start;
        frames
            .map(|n| {
                let b = env.builtins(n);
                let vars = match env.vars {
                    Some(v) => v.eval(&b)?,
                    None => Vec::new(),
                };
                let mut frame = Frame { b, vars: &vars };
                #[allow(clippy::cast_precision_loss)]
                if let Self::Sampled(_) = self {
                    frame.b[Builtin::N as usize] = (n - start) as f64;
                }
                self.try_eval(clock, &frame)?.ok_or_else(|| Error::Track {
                    path: String::new(),
                    msg: "no keys".into(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
