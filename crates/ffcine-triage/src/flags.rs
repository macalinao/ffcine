//! Clip flags: runaway takes (left recording), dead stretches, dark clips, blips.

use crate::index::{Clip, Flag, FlagKind, Span, timecode};

#[derive(Debug, Clone)]
pub struct FlagParams {
    /// A take at least this long (seconds, summed over camera file splits) is a runaway candidate.
    pub runaway: f64,
    /// Shortest dead stretch flagged (seconds).
    pub dead: f64,
    /// Activity under this counts as a frozen picture.
    pub still: f32,
    /// Mean luma under this counts as black.
    pub dark: f32,
}

impl Default for FlagParams {
    fn default() -> Self {
        Self {
            runaway: 20.0 * 60.0,
            dead: 3.0 * 60.0,
            still: 0.04,
            dark: 0.06,
        }
    }
}

/// Flags for one clip; `take_seconds` is the duration of the whole take it belongs to.
pub fn flags(clip: &Clip, take_seconds: f64, p: &FlagParams) -> Vec<Flag> {
    let mut out = Vec::new();
    if clip.duration < 1.5 {
        out.push(Flag {
            kind: FlagKind::Blip,
            detail: format!("{:.1} s", clip.duration),
            span: None,
        });
    }
    if !clip.has_audio {
        out.push(Flag {
            kind: FlagKind::NoAudio,
            detail: "no audio stream".into(),
            span: None,
        });
    }
    let dead = dead_stretches(clip, p);
    if !clip.luma.is_empty() {
        #[allow(clippy::cast_precision_loss)]
        let dark =
            clip.luma.iter().filter(|&&l| l < p.dark).count() as f64 / clip.luma.len() as f64;
        if dark > 0.8 {
            out.push(Flag {
                kind: FlagKind::Dark,
                detail: format!("{:.0}% black", dark * 100.0),
                span: None,
            });
        }
    }
    if take_seconds >= p.runaway {
        // The tail to drop: from the last live moment to the end, if the clip ends dead.
        let tail = dead.last().filter(|s| clip.duration - s.end < 2.0).copied();
        let detail = match tail {
            Some(s) => format!(
                "{} take, dead from {} ({} to drop)",
                timecode(take_seconds),
                timecode(s.start),
                timecode(s.len())
            ),
            None => format!("{} take", timecode(take_seconds)),
        };
        out.push(Flag {
            kind: FlagKind::Runaway,
            detail,
            span: tail,
        });
    }
    out.extend(dead.into_iter().map(|s| Flag {
        kind: FlagKind::Dead,
        detail: format!("{}-{} still, no speech", timecode(s.start), timecode(s.end)),
        span: Some(s),
    }));
    out
}

/// Runs of seconds with a frozen or black picture and no speech, at least `p.dead` long.
pub fn dead_stretches(clip: &Clip, p: &FlagParams) -> Vec<Span> {
    let mut speaking = vec![false; clip.activity.len()];
    for s in &clip.speech {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (a, b) = (
            s.start as usize,
            (s.end.ceil() as usize).min(speaking.len()),
        );
        for v in speaking.iter_mut().take(b).skip(a) {
            *v = true;
        }
    }
    let dead = |i: usize| {
        let still = clip.activity[i] < p.still || clip.luma.get(i).is_some_and(|&l| l < p.dark);
        still && !speaking[i]
    };
    let mut out = Vec::new();
    let mut start = None;
    for i in 0..=clip.activity.len() {
        match (i < clip.activity.len() && dead(i), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                #[allow(clippy::cast_precision_loss)]
                let span = Span {
                    start: s as f64,
                    end: (i as f64).min(clip.duration),
                };
                if span.len() >= p.dead {
                    out.push(span);
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runaway_with_dead_tail() {
        let n = 3000;
        let mut activity = vec![0.5f32; n];
        activity[600..].iter_mut().for_each(|a| *a = 0.01);
        let clip = Clip {
            duration: 3000.0,
            has_audio: true,
            activity,
            luma: vec![0.4; n],
            speech: vec![Span {
                start: 10.0,
                end: 20.0,
            }],
            ..Clip::default()
        };
        let f = flags(&clip, 3000.0, &FlagParams::default());
        let kinds: Vec<_> = f.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [FlagKind::Runaway, FlagKind::Dead]);
        assert_eq!(
            f[0].span,
            Some(Span {
                start: 600.0,
                end: 3000.0
            })
        );
    }

    #[test]
    fn speech_keeps_a_still_shot_alive() {
        let clip = Clip {
            duration: 400.0,
            has_audio: true,
            activity: vec![0.0; 400],
            luma: vec![0.5; 400],
            speech: vec![Span {
                start: 150.0,
                end: 160.0,
            }],
            ..Clip::default()
        };
        let d = dead_stretches(&clip, &FlagParams::default());
        assert_eq!(
            d,
            [Span {
                start: 160.0,
                end: 400.0
            }]
        );
    }
}
