//! The one keyframe form, `[{"t": RationalTime, "v": value, "interp": "linear" | "ease" | "hold"}]`
//! (`docs/timeline.md#effect-schemas`), evaluated in Rust per frame: a thin wrapper over
//! [`ffcine_core::anim`]'s tracks on [`Clock::Source`].
//!
//! `t` is a `RationalTime` (or plain seconds) in the clip's source time (`source_range.start_time`
//! plus the time since the clip's first frame, `docs/timeline.md`). `interp` (or `ease`) shapes
//! the segment from that key to the next, with any [`Interp`] (the text keyframes' set). Before
//! the first key the first value holds, after the last the last. A value may also be an
//! expression over the source time `st` (`docs/compositing.md#expressions`).

use ffcine_core::Result;
use ffcine_core::anim::{Animatable, Clock, Track, keys_at};
use serde_json::Value;

/// How a segment moves from one key to the next (`ffcine_core::anim::Ease`).
pub use ffcine_core::anim::Ease as Interp;
pub use ffcine_core::anim::{Key, is_keys, seconds};

/// A value that can be keyframed.
pub use ffcine_core::anim::Animatable as Lerp;

/// `0.5 - 0.5 cos(pi u)`.
pub fn ease(u: f64) -> f64 {
    Interp::Ease.at(u)
}

/// A parameter: a constant, keys sorted by time, or an expression track.
#[derive(Debug, Clone, PartialEq)]
pub enum Keyed<T> {
    Const(T),
    Keys(Vec<Key<T>>),
    /// An expression, or keys under one (`{"keys", "expr"}`).
    Track(Track<T>),
}

impl<T: Default> Default for Keyed<T> {
    fn default() -> Self {
        Self::Const(T::default())
    }
}

impl<T: Animatable> Keyed<T> {
    /// A constant, the keyframe form or an expression; `what` names the parameter in errors.
    pub fn parse(v: &Value, what: &'static str) -> Result<Self> {
        Ok(match Track::parse(v, Clock::Source, what)? {
            Track::Const(c) => Self::Const(c),
            Track::Keys(k) => Self::Keys(k),
            t => Self::Track(t),
        })
    }

    /// Keys in any order (sorted here, stable for equal times).
    pub fn from_keys(mut keys: Vec<Key<T>>) -> Self {
        keys.sort_by(|a, b| a.t.total_cmp(&b.t));
        Self::Keys(keys)
    }

    pub fn is_animated(&self) -> bool {
        match self {
            Self::Const(_) => false,
            Self::Keys(k) => k.len() > 1,
            Self::Track(t) => t.is_animated(),
        }
    }

    /// The value at source time `t` (seconds); `None` for no keys, or an expression that isn't
    /// finite there.
    pub fn eval(&self, t: f64) -> Option<T> {
        self.try_eval(t).ok().flatten()
    }

    /// [`Keyed::eval`], with an expression's error.
    pub fn try_eval(&self, t: f64) -> Result<Option<T>> {
        Ok(match self {
            Self::Const(v) => Some(*v),
            Self::Keys(k) => keys_at(k, t),
            Self::Track(tr) => tr.try_eval(
                Clock::Source,
                &ffcine_core::anim::Frame::at(Clock::Source, t, 0.0),
            )?,
        })
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

    #[test]
    fn expressions_over_source_time() {
        let k: Keyed<f64> = Keyed::parse(&json!("1 + st / 10"), "zoom").unwrap();
        assert!(k.is_animated());
        assert!((k.eval(5.0).unwrap() - 1.5).abs() < 1e-12);
        let c: Keyed<[f64; 2]> =
            Keyed::parse(&json!([0.25, "0.5 + 0.1 * sin(st)"]), "centre").unwrap();
        let v = c.eval(1.0).unwrap();
        assert!((v[0] - 0.25).abs() < 1e-12 && (v[1] - (0.5 + 0.1 * 1f64.sin())).abs() < 1e-12);
        let ke: Keyed<f64> = Keyed::parse(
            &json!({"keys": [{"t": 0, "v": 1}, {"t": 2, "v": 3}], "expr": "value * 2"}),
            "zoom",
        )
        .unwrap();
        assert!((ke.eval(1.0).unwrap() - 4.0).abs() < 1e-12);
        // Only the source time is known in a timeline effect.
        assert!(Keyed::<f64>::parse(&json!("t * 2"), "zoom").is_err());
        let nan: Keyed<f64> = Keyed::parse(&json!("log(st)"), "zoom").unwrap();
        assert!(nan.eval(-1.0).is_none() && nan.try_eval(-1.0).is_err());
        // A constant expression folds.
        assert_eq!(
            Keyed::<f64>::parse(&json!("2 * 0.5"), "zoom").unwrap(),
            Keyed::Const(1.0)
        );
    }
}
