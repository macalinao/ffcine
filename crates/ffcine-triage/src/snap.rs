//! Snap proposed in/out points to word boundaries and silence, so a line is never cut mid-word
//! (a line that ended on "in ni-").
//!
//! Units are the transcript's words, or the VAD speech spans when there is no transcript. A point
//! inside a unit moves to the nearer edge of it (`Prefer::Nearest`), or always outward to keep the
//! whole word (`Prefer::Keep`); a point in a gap is pulled at least `pad` clear of the neighbouring
//! units (never past the middle of the gap).

use crate::index::{Clip, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prefer {
    /// Move to the closer edge of the word (drops a word cut early, keeps one cut late).
    #[default]
    Nearest,
    /// Always keep the word: in-points move before it, out-points after it.
    Keep,
}

#[derive(Debug, Clone, Copy)]
pub struct SnapParams {
    /// Breathing room kept around words (seconds).
    pub pad: f64,
    pub prefer: Prefer,
}

impl Default for SnapParams {
    fn default() -> Self {
        Self {
            pad: 0.12,
            prefer: Prefer::Nearest,
        }
    }
}

/// Speech units of a clip: words when transcribed, else VAD speech spans.
pub fn units(clip: &Clip) -> Vec<Span> {
    let mut u: Vec<Span> = match &clip.transcript {
        Some(t) if t.words().next().is_some() => t
            .words()
            .map(|w| Span {
                start: w.start,
                end: w.end,
            })
            .collect(),
        _ => clip.speech.clone(),
    };
    u.sort_by(|a, b| a.start.total_cmp(&b.start));
    u
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    In,
    Out,
}

/// Snapped (in, out). The result always keeps `in < out` when the input did.
pub fn snap(units: &[Span], cut_in: f64, cut_out: f64, p: SnapParams) -> (f64, f64) {
    let i = snap_one(units, cut_in, Edge::In, p);
    let o = snap_one(units, cut_out, Edge::Out, p);
    if i < o { (i, o) } else { (cut_in, cut_out) }
}

pub fn snap_in(units: &[Span], t: f64, p: SnapParams) -> f64 {
    snap_one(units, t, Edge::In, p)
}

pub fn snap_out(units: &[Span], t: f64, p: SnapParams) -> f64 {
    snap_one(units, t, Edge::Out, p)
}

fn snap_one(units: &[Span], t: f64, edge: Edge, p: SnapParams) -> f64 {
    // Index of the first unit starting after t.
    let next = units.partition_point(|u| u.start <= t);
    let prev = next.checked_sub(1).map(|i| units[i]);
    let after = units.get(next).copied();
    // The gap around a unit edge: (end of the unit before, start of the unit after).
    let gap_before = |k: usize| {
        (
            k.checked_sub(1).map_or(f64::NEG_INFINITY, |j| units[j].end),
            units[k].start,
        )
    };
    let gap_after = |k: usize| {
        (
            units[k].end,
            units.get(k + 1).map_or(f64::INFINITY, |u| u.start),
        )
    };
    // Pull `x` (a word edge) `pad` into its gap, not past the gap's middle.
    let into_before = |k: usize| {
        let (a, b) = gap_before(k);
        (b - p.pad)
            .max(if a.is_finite() {
                f64::midpoint(a, b)
            } else {
                b - p.pad
            })
            .max(0.0)
    };
    let into_after = |k: usize| {
        let (a, b) = gap_after(k);
        (a + p.pad).min(if b.is_finite() {
            f64::midpoint(a, b)
        } else {
            a + p.pad
        })
    };
    match prev {
        // Inside unit next-1.
        Some(u) if t < u.end => {
            let k = next - 1;
            let to_start = match p.prefer {
                Prefer::Keep => edge == Edge::In,
                Prefer::Nearest => t - u.start < u.end - t,
            };
            if to_start {
                into_before(k)
            } else {
                into_after(k)
            }
        }
        // In a gap: keep t but clear of the neighbours.
        _ => {
            let lo = prev.map_or(f64::NEG_INFINITY, |_| into_after(next - 1));
            let hi = after.map_or(f64::INFINITY, |_| into_before(next));
            if lo <= hi { t.clamp(lo, hi) } else { t }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(start: f64, end: f64) -> Span {
        Span { start, end }
    }

    // "in ni- ... udon": words at 1.0-1.3, 1.35-1.8, then a pause, 3.0-3.5.
    fn words() -> Vec<Span> {
        vec![s(1.0, 1.3), s(1.35, 1.8), s(3.0, 3.5)]
    }

    #[test]
    fn mid_word_out_is_fixed() {
        let p = SnapParams::default();
        // Cut late in "ni..." (1.7): keep the word, end in the pause after it.
        assert!((snap_out(&words(), 1.7, p) - 1.92).abs() < 1e-9);
        // Cut early in it (1.4): drop it, end between "in" and "ni".
        assert!((snap_out(&words(), 1.4, p) - 1.325).abs() < 1e-9);
        // Keep mode always keeps the word.
        let keep = SnapParams {
            prefer: Prefer::Keep,
            ..p
        };
        assert!((snap_out(&words(), 1.4, keep) - 1.92).abs() < 1e-9);
    }

    #[test]
    fn in_points_land_before_words() {
        let p = SnapParams::default();
        assert!((snap_in(&words(), 3.1, p) - 2.88).abs() < 1e-9);
        // In a long pause, a free choice stays, but not hard against the next word.
        assert!((snap_in(&words(), 2.5, p) - 2.5).abs() < 1e-9);
        assert!((snap_in(&words(), 2.95, p) - 2.88).abs() < 1e-9);
        // Before the first word, never below zero.
        assert!((snap_in(&words(), 0.95, p) - 0.88).abs() < 1e-9);
        assert!(snap_in(&[s(0.05, 0.4)], 0.1, p) >= 0.0);
    }

    #[test]
    fn no_units_is_identity() {
        assert_eq!(snap(&[], 1.0, 2.0, SnapParams::default()), (1.0, 2.0));
    }
}
