//! `ffcine diff`: which segments of a new plan aren't in the old one's cache.

use std::fmt::Write as _;

use crate::plan::{Plan, key_set};

/// Diff of two plans by segment key.
#[derive(Debug, Clone, PartialEq)]
pub struct Diff {
    /// `(segment index in the new plan, re-renders)`.
    pub segments: Vec<(usize, bool)>,
    /// Old segments no longer used.
    pub dropped: usize,
    pub audio_changed: bool,
}

impl Diff {
    pub fn new(old: &Plan, new: &Plan) -> Self {
        let old_keys = key_set(old);
        let new_keys = key_set(new);
        Self {
            segments: new
                .segments
                .iter()
                .enumerate()
                .map(|(i, s)| (i, !old_keys.contains(&s.key)))
                .collect(),
            dropped: old_keys.difference(&new_keys).count(),
            audio_changed: old.audio.as_ref().map(|a| &a.key) != new.audio.as_ref().map(|a| &a.key),
        }
    }

    pub fn render_count(&self) -> usize {
        self.segments.iter().filter(|(_, r)| *r).count()
    }

    /// One line per segment (`+` re-renders, `=` cached) and a summary.
    #[allow(clippy::cast_precision_loss)]
    pub fn report(&self, new: &Plan, all: bool) -> String {
        let mut out = String::new();
        let secs = |f: i64| f as f64 / new.rate;
        let mut render_frames = 0;
        for &(i, render) in &self.segments {
            let s = &new.segments[i];
            if render {
                render_frames += s.frames;
            }
            if render || all {
                let t = secs(s.start);
                let _ = writeln!(
                    out,
                    "{} {:>3}:{:06.3} {:>5}f  {:<32} {}",
                    if render { '+' } else { '=' },
                    (t / 60.0).floor(),
                    t % 60.0,
                    s.frames,
                    s.name,
                    s.key.short()
                );
            }
        }
        let n = self.render_count();
        let _ = writeln!(
            out,
            "{n} of {} segments render ({:.1} s of {:.1} s), {} old dropped; audio {}",
            self.segments.len(),
            secs(render_frames),
            secs(new.frames()),
            self.dropped,
            if self.audio_changed {
                "changed"
            } else {
                "same"
            },
        );
        if n == 0 {
            let _ = writeln!(
                out,
                "{}",
                if self.audio_changed {
                    "picture unchanged: audio remux only (no video encode)"
                } else {
                    "nothing to render"
                }
            );
        }
        out
    }
}
