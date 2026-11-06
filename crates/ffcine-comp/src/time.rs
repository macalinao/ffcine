//! Time remapping (`docs/compositing.md#time-remapping`): a video layer's `time` key, the
//! source time it gives each layer frame, and the frame map lowering hands the decoder.

use std::ops::Range;

use ffcine_core::Result;
use ffcine_core::anim::{Clock, Ease, Env, Frame, Key, Scope, Track};
use serde_json::{Map, Value};

use crate::model::{err, limits};

/// A layer's `time`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Remap {
    /// `speed` (source s per layer s) times `sign` (−1 with `reverse`), integrated from
    /// `source_in`.
    Speed { track: Track<f64>, sign: f64 },
    /// `source`: the source time, s, per layer frame.
    Source(Track<f64>),
    /// `freeze`: one source time, s.
    Freeze(f64),
}

/// Parses `time`: exactly one of `speed`, `source` and `freeze`, or `reverse` alone (speed
/// −1) or with `speed` (negating it).
pub(crate) fn parse(v: &Value, scope: &Scope, path: &str) -> Result<Remap> {
    let m: &Map<String, Value> = v
        .as_object()
        .ok_or_else(|| err(path, format!("expected an object, got {v}")))?;
    let known = ["speed", "source", "freeze", "reverse"];
    if let Some(k) = m
        .keys()
        .find(|k| !k.starts_with("x_") && !known.contains(&k.as_str()))
    {
        return Err(err(
            format!("{path}.{k}"),
            format!("unknown key (known: {})", known.join(", ")),
        ));
    }
    let reverse = match m.get("reverse") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(v) => {
            return Err(err(
                format!("{path}.reverse"),
                format!("expected true or false, got {v}"),
            ));
        }
    };
    let given: Vec<&str> = ["speed", "source", "freeze"]
        .into_iter()
        .filter(|k| m.contains_key(*k))
        .collect();
    let at = |k: &str| format!("{path}.{k}");
    match given.as_slice() {
        [] if reverse => Ok(Remap::Speed {
            track: Track::Const(1.0),
            sign: -1.0,
        }),
        [] => Err(err(
            path,
            "expected one of speed, source, freeze or reverse",
        )),
        ["speed"] => {
            let t: Track<f64> = Track::parse_in(&m["speed"], scope, &at("speed"))?;
            if let Some(v) = constants(&t).into_iter().find(|v| v.abs() > limits::SPEED) {
                return Err(err(
                    at("speed"),
                    format!("{v}: at most {} either way", limits::SPEED),
                ));
            }
            Ok(Remap::Speed {
                track: t,
                sign: if reverse { -1.0 } else { 1.0 },
            })
        }
        [k] if reverse => Err(err(
            at("reverse"),
            format!("reverse goes with speed (or alone), not {k}"),
        )),
        ["source"] => Ok(Remap::Source(Track::parse_in(
            &m["source"],
            scope,
            &at("source"),
        )?)),
        ["freeze"] => match m["freeze"].as_f64() {
            Some(s) if (0.0..=limits::DURATION).contains(&s) => Ok(Remap::Freeze(s)),
            _ => Err(err(
                at("freeze"),
                format!(
                    "expected source seconds 0 to {}, got {}",
                    limits::DURATION,
                    m["freeze"]
                ),
            )),
        },
        _ => Err(err(path, format!("{} given: use one", given.join(" and ")))),
    }
}

/// A track's constant values (its `Const` or its keys' `v`).
fn constants(t: &Track<f64>) -> Vec<f64> {
    match t {
        Track::Const(v) => vec![*v],
        _ => t
            .keys()
            .map_or_else(Vec::new, |k| k.iter().map(|k| k.v).collect()),
    }
}

/// Where a frame's speed is read: the layer's env at a comp time between frames too.
struct Speed<'a> {
    track: &'a Track<f64>,
    env: &'a Env<'a>,
    /// −1 with `reverse`.
    sign: f64,
    path: &'a str,
}

impl Speed<'_> {
    /// The speed at layer time `lt`, checked finite and within [`limits::SPEED`].
    fn at(&self, lt: f64) -> Result<f64> {
        let b = self.env.builtins_at(self.env.in_ + lt);
        let vars = match self.env.vars {
            Some(v) => v.eval(&b).map_err(|e| err(self.path, e.to_string()))?,
            None => Vec::new(),
        };
        let v = self
            .track
            .try_eval(Clock::Layer, &Frame { b, vars: &vars })
            .map_err(|e| err(self.path, e.to_string()))?
            .ok_or_else(|| err(self.path, "no keys"))?
            * self.sign;
        if !v.is_finite() || v.abs() > limits::SPEED {
            return Err(err(
                self.path,
                format!(
                    "{v} at lt = {lt}: a speed is finite and at most {} either way",
                    limits::SPEED
                ),
            ));
        }
        Ok(v)
    }
}

/// `∫₀^lt speed`: closed form for constants and linear or hold keys (per segment, as
/// `Ramp::segments`), Simpson's rule on a grid otherwise.
enum Integral {
    Const(f64),
    /// Keys on layer time, negative times resolved.
    Keys(Vec<Key<f64>>),
    /// Grid step (layer s) and the integral at each grid point from 0.
    Numeric {
        h: f64,
        cum: Vec<f64>,
    },
}

/// Simpson steps per frame.
const STEPS: f64 = 8.0;

impl Integral {
    #[allow(clippy::many_single_char_names)]
    fn new(sp: &Speed<'_>, upto: f64, fps: f64) -> Result<Self> {
        let dur = sp.env.out - sp.env.in_;
        match sp.track {
            Track::Const(v) => return Ok(Self::Const(v * sp.sign)),
            Track::Keys(k)
                if k.iter()
                    .all(|k| matches!(k.ease, Ease::Linear | Ease::Hold)) =>
            {
                let mut k: Vec<Key<f64>> = k
                    .iter()
                    .map(|k| Key {
                        t: if k.t < 0.0 { dur + k.t } else { k.t },
                        v: k.v * sp.sign,
                        ..*k
                    })
                    .collect();
                k.sort_by(|a, b| a.t.total_cmp(&b.t));
                return Ok(Self::Keys(k));
            }
            _ => {}
        }
        let h = 1.0 / (fps * STEPS);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (upto.max(0.0) / h).ceil() as usize + 1;
        let mut cum = Vec::with_capacity(n + 1);
        cum.push(0.0);
        let mut a = sp.at(0.0)?;
        for j in 0..n {
            #[allow(clippy::cast_precision_loss)]
            let t0 = j as f64 * h;
            let m = sp.at(t0 + h / 2.0)?;
            let b = sp.at(t0 + h)?;
            cum.push(cum[j] + h / 6.0 * (a + 4.0 * m + b));
            a = b;
        }
        Ok(Self::Numeric { h, cum })
    }

    fn at(&self, sp: &Speed<'_>, lt: f64) -> Result<f64> {
        Ok(match self {
            Self::Const(v) => v * lt,
            Self::Keys(k) => keys_area(k, lt) - keys_area(k, 0.0),
            Self::Numeric { h, cum } => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let j = ((lt.max(0.0) / h).floor() as usize).min(cum.len() - 1);
                #[allow(clippy::cast_precision_loss)]
                let t0 = j as f64 * h;
                let (a, m, b) = (sp.at(t0)?, sp.at(f64::midpoint(t0, lt))?, sp.at(lt)?);
                cum[j] + (lt - t0) / 6.0 * (a + 4.0 * m + b)
            }
        })
    }
}

/// `∫ keys` from the first key's time to `x` (negative before it), the value held outside.
#[allow(clippy::many_single_char_names)]
fn keys_area(k: &[Key<f64>], x: f64) -> f64 {
    let Some(first) = k.first() else {
        return 0.0;
    };
    if x <= first.t {
        return first.v * (x - first.t);
    }
    let mut area = 0.0;
    for w in k.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if x <= a.t {
            break;
        }
        let hi = x.min(b.t);
        let d = hi - a.t;
        let span = b.t - a.t;
        area += match a.ease {
            _ if span <= 0.0 => 0.0,
            Ease::Hold => a.v * d,
            _ => a.v * d + (b.v - a.v) * d * d / (2.0 * span),
        };
    }
    let last = &k[k.len() - 1];
    if x > last.t {
        area += last.v * (x - last.t);
    }
    area
}

/// Source seconds for each comp frame of `frames`: at the frame and half a frame later (the
/// pick in [`pick`] needs both).
pub(crate) fn source_times(
    remap: &Remap,
    env: &Env<'_>,
    frames: Range<i64>,
    path: &str,
) -> Result<Vec<(f64, f64)>> {
    let fps = env.fps;
    #[allow(clippy::cast_precision_loss)]
    let lt = |n: i64| n as f64 / fps - env.in_;
    let half = 0.5 / fps;
    match remap {
        Remap::Freeze(s) => Ok(frames.map(|_| (*s, *s)).collect()),
        Remap::Source(t) => {
            let sp = Speed {
                track: t,
                env,
                sign: 1.0,
                path,
            };
            frames
                .map(|n| {
                    let s = |lt: f64| -> Result<f64> {
                        let b = env.builtins_at(env.in_ + lt);
                        let vars = match env.vars {
                            Some(v) => v.eval(&b).map_err(|e| err(path, e.to_string()))?,
                            None => Vec::new(),
                        };
                        let v = sp
                            .track
                            .try_eval(Clock::Layer, &Frame { b, vars: &vars })
                            .map_err(|e| err(path, e.to_string()))?
                            .ok_or_else(|| err(path, "no keys"))?;
                        if v.is_finite() && v.abs() <= limits::DURATION {
                            Ok(v)
                        } else {
                            Err(err(
                                path,
                                format!(
                                    "{v} at lt = {lt}: a source time is 0 to {} s",
                                    limits::DURATION
                                ),
                            ))
                        }
                    };
                    Ok((s(lt(n))?, s(lt(n) + half)?))
                })
                .collect()
        }
        Remap::Speed { track, sign } => {
            let sp = Speed {
                track,
                env,
                sign: *sign,
                path,
            };
            let upto = lt(frames.end) + half;
            let int = Integral::new(&sp, upto, fps)?;
            frames
                .map(|n| {
                    let a = lt(n);
                    Ok((
                        env.source_in + int.at(&sp, a)?,
                        env.source_in + int.at(&sp, a + half)?,
                    ))
                })
                .collect()
        }
    }
}

/// The source frame a layer frame shows, from its source times at the frame (`s`) and half
/// a frame later (`mid`), on a source of `rate` fps and `frames` frames. Moving forward, it is
/// the `fps` filter's pick (`round=near`): the last source frame whose remapped time rounds to
/// this frame or earlier, i.e. the last one before `mid` (a tie counts as before, as ffmpeg's
/// truncating `setpts` makes it). Holding or going back, the nearest frame to `s`.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn pick(s: f64, mid: f64, rate: f64, frames: u64) -> i64 {
    let k = if mid > s + 1e-12 {
        (rate * mid + 1e-9).floor()
    } else {
        (rate * s).round()
    };
    let last = i64::try_from(frames.max(1) - 1).unwrap_or(i64::MAX);
    (k.max(0.0) as i64).min(last)
}
