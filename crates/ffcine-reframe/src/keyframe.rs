//! The one keyframe form, `[{"t": RationalTime, "v": value, "interp": "linear" | "ease" | "hold"}]`
//! (`docs/timeline.md#effect-schemas`), evaluated in Rust per frame.
//!
//! `t` is a `RationalTime` (or plain seconds) in the clip's source time (`source_range.start_time`
//! plus the time since the clip's first frame, `docs/timeline.md`). `interp` shapes the segment
//! from that key to the next. Before the first key the first value holds, after the last
//! the last.

use std::f64::consts::PI;

use ffcine_core::{Error, Result};
use serde_json::Value;

/// How a segment moves from one key to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interp {
    #[default]
    Linear,
    /// Cosine ease in and out, `0.5 - 0.5 cos(pi u)`: the prototype's `crop_x` pan curve.
    Ease,
    /// Keep this key's value until the next key.
    Hold,
}

impl Interp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "linear" => Some(Self::Linear),
            "ease" => Some(Self::Ease),
            "hold" => Some(Self::Hold),
            _ => None,
        }
    }

    /// The segment's progress at `u` (0..1 of the way to the next key).
    pub fn shape(self, u: f64) -> f64 {
        let u = u.clamp(0.0, 1.0);
        match self {
            Self::Linear => u,
            Self::Ease => ease(u),
            Self::Hold => 0.0,
        }
    }
}

/// `0.5 - 0.5 cos(pi u)`.
pub fn ease(u: f64) -> f64 {
    0.5 - 0.5 * (PI * u.clamp(0.0, 1.0)).cos()
}

/// A value that can be keyframed.
pub trait Lerp: Copy {
    fn lerp(a: Self, b: Self, k: f64) -> Self;
    fn from_json(v: &Value) -> Option<Self>;
}

impl Lerp for f64 {
    fn lerp(a: Self, b: Self, k: f64) -> Self {
        a + (b - a) * k
    }
    fn from_json(v: &Value) -> Option<Self> {
        v.as_f64()
    }
}

impl Lerp for [f64; 2] {
    fn lerp(a: Self, b: Self, k: f64) -> Self {
        [f64::lerp(a[0], b[0], k), f64::lerp(a[1], b[1], k)]
    }
    fn from_json(v: &Value) -> Option<Self> {
        match v.as_array()?.as_slice() {
            [x, y] => Some([x.as_f64()?, y.as_f64()?]),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Key<T> {
    /// Seconds of source time.
    pub t: f64,
    pub v: T,
    pub interp: Interp,
}

/// A parameter: a constant, or keys sorted by time.
#[derive(Debug, Clone, PartialEq)]
pub enum Keyed<T> {
    Const(T),
    Keys(Vec<Key<T>>),
}

impl<T: Default> Default for Keyed<T> {
    fn default() -> Self {
        Self::Const(T::default())
    }
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

/// Whether `v` is the keyframe form (an array of `{t, v}` objects).
pub fn is_keys(v: &Value) -> bool {
    v.as_array()
        .and_then(|a| a.first())
        .is_some_and(|k| k.get("t").is_some() && k.get("v").is_some())
}

impl<T: Lerp> Keyed<T> {
    /// A constant or the keyframe form; `what` names the parameter in errors.
    pub fn parse(v: &Value, what: &'static str) -> Result<Self> {
        let invalid = || Error::Invalid {
            what,
            value: v.to_string(),
        };
        if !is_keys(v) {
            return T::from_json(v).map(Self::Const).ok_or_else(invalid);
        }
        let mut keys = Vec::new();
        for k in v.as_array().into_iter().flatten() {
            let interp = match k.get("interp").and_then(Value::as_str) {
                None => Interp::Linear,
                Some(s) => Interp::parse(s).ok_or_else(invalid)?,
            };
            keys.push(Key {
                t: k.get("t").and_then(seconds).ok_or_else(invalid)?,
                v: k.get("v").and_then(T::from_json).ok_or_else(invalid)?,
                interp,
            });
        }
        Ok(Self::from_keys(keys))
    }

    /// Keys in any order (sorted here, stable for equal times).
    pub fn from_keys(mut keys: Vec<Key<T>>) -> Self {
        keys.sort_by(|a, b| a.t.total_cmp(&b.t));
        Self::Keys(keys)
    }

    pub fn is_animated(&self) -> bool {
        matches!(self, Self::Keys(k) if k.len() > 1)
    }

    /// The value at source time `t` (seconds).
    pub fn eval(&self, t: f64) -> Option<T> {
        let keys = match self {
            Self::Const(v) => return Some(*v),
            Self::Keys(k) => k,
        };
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
        Some(T::lerp(a.v, b.v, a.interp.shape((t - a.t) / span)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rt(value: f64) -> Value {
        json!({"OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": value})
    }

    #[test]
    fn parse_and_eval() {
        let k: Keyed<f64> = Keyed::parse(
            &json!([
                {"t": rt(48.0), "v": 2.0, "interp": "hold"},
                {"t": rt(0.0), "v": 1.0, "interp": "linear"},
                {"t": rt(72.0), "v": 4.0, "interp": "ease"},
                {"t": rt(96.0), "v": 0.0},
            ]),
            "zoom",
        )
        .unwrap();
        assert!(k.is_animated());
        let at = |t| k.eval(t).unwrap();
        assert!((at(-1.0) - 1.0).abs() < 1e-12);
        assert!((at(1.0) - 1.5).abs() < 1e-12);
        // Hold until the next key, then jump.
        assert!((at(2.9) - 2.0).abs() < 1e-12);
        assert!((at(3.0) - 4.0).abs() < 1e-12);
        // Ease is symmetric about the segment's middle.
        assert!((at(3.5) - 2.0).abs() < 1e-12);
        assert!((at(3.25) - (4.0 - 4.0 * ease(0.25))).abs() < 1e-12);
        assert!((at(10.0) - 0.0).abs() < 1e-12);

        let c: Keyed<[f64; 2]> = Keyed::parse(&json!([0.25, 0.5]), "centre").unwrap();
        assert_eq!(c.eval(3.0), Some([0.25, 0.5]));
        assert!(Keyed::<f64>::parse(&json!("x"), "zoom").is_err());
        assert!(Keyed::<f64>::parse(&json!([{"t": 0, "v": 1, "interp": "bounce"}]), "z").is_err());
    }

    #[test]
    fn ease_is_smooth() {
        assert!(ease(0.0).abs() < 1e-15 && (ease(1.0) - 1.0).abs() < 1e-15);
        // Zero slope at both ends.
        assert!(ease(1e-4) < 1e-7 && 1.0 - ease(1.0 - 1e-4) < 1e-7);
    }
}
