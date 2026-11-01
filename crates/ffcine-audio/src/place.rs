//! Where each clip of a track lands on the timeline, in seconds, with transitions turned into
//! overlaps (OTIO: the outgoing clip runs `out_offset` past the cut, the incoming one starts
//! `in_offset` before it).

use std::path::{Path, PathBuf};

use ffcine_timeline::{Clip, Effect, Item, MediaReference, Track};

/// A clip placed on the record timeline.
#[derive(Debug, Clone)]
pub struct Placed<'a> {
    pub clip: &'a Clip,
    /// Record time (s) of the first sample.
    pub start: f64,
    /// Record duration (s).
    pub duration: f64,
    /// Media time (s) of the first sample, relative to the media's first sample.
    pub source_start: f64,
    /// Crossfade lengths (s) from transitions at either end; 0 for a cut.
    pub xfade_in: f64,
    pub xfade_out: f64,
    /// `LinearTimeWarp` speed (1 normal, 0 freeze).
    pub speed: f64,
}

impl Placed<'_> {
    pub fn end(&self) -> f64 {
        self.start + self.duration
    }

    /// The media file, resolved against `base` when relative.
    pub fn path(&self, base: Option<&Path>) -> Option<PathBuf> {
        media_path(self.clip, base)
    }
}

pub fn media_path(clip: &Clip, base: Option<&Path>) -> Option<PathBuf> {
    let MediaReference::External(r) = clip.media_reference()? else {
        return None;
    };
    let p = r.path()?;
    Some(match base {
        Some(b) if p.is_relative() => b.join(p),
        _ => p,
    })
}

/// Lays out the clips of `track` (gaps advance time; nested stacks/tracks are skipped but
/// advance by their `source_range`). Disabled clips are dropped.
pub fn layout(track: &Track) -> (Vec<Placed<'_>>, f64) {
    let mut out: Vec<Placed<'_>> = Vec::new();
    let mut t = 0.0;
    let mut pending_in = 0.0f64;
    let mut pending_xfade = 0.0f64;
    for item in &track.children {
        match item {
            Item::Clip(c) => {
                let range = c.source_range.as_ref().or_else(|| {
                    c.media_reference()
                        .and_then(MediaReference::available_range)
                });
                let Some(range) = range else { continue };
                let avail0 = c
                    .media_reference()
                    .and_then(MediaReference::available_range)
                    .map_or(0.0, |r| r.start_time.to_seconds());
                let duration = range.duration.to_seconds();
                let speed = c
                    .effects
                    .iter()
                    .find_map(|e| match e {
                        Effect::LinearTimeWarp(w) => Some(w.time_scalar),
                        Effect::FreezeFrame(_) => Some(0.0),
                        _ => None,
                    })
                    .unwrap_or(1.0);
                if c.enabled {
                    out.push(Placed {
                        clip: c,
                        start: t - pending_in,
                        duration: duration + pending_in,
                        source_start: range.start_time.to_seconds() - avail0 - pending_in * speed,
                        xfade_in: pending_xfade,
                        xfade_out: 0.0,
                        speed,
                    });
                }
                pending_in = 0.0;
                pending_xfade = 0.0;
                t += duration;
            }
            Item::Transition(tr) => {
                let i = tr.in_offset.to_seconds();
                let o = tr.out_offset.to_seconds();
                if let Some(prev) = out.last_mut().filter(|p| (p.end() - t).abs() < 1e-6) {
                    prev.duration += o;
                    prev.xfade_out = i + o;
                }
                pending_in = i;
                pending_xfade = i + o;
            }
            Item::Gap(g) => {
                t += g
                    .source_range
                    .as_ref()
                    .map_or(0.0, |r| r.duration.to_seconds());
            }
            Item::Stack(s) => {
                t += s
                    .source_range
                    .as_ref()
                    .map_or(0.0, |r| r.duration.to_seconds());
            }
            Item::Track(s) => {
                t += s
                    .source_range
                    .as_ref()
                    .map_or(0.0, |r| r.duration.to_seconds());
            }
            Item::Unknown(_) => {}
        }
    }
    (out, t)
}
