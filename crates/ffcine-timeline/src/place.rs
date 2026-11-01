//! Where things sit on the timeline: clips and markers in timeline seconds.
//!
//! OTIO stores positions implicitly (a track's items play one after another; transitions
//! overlap their neighbours and take no time). [`Timeline::placed_clips`] and
//! [`Timeline::placed_markers`] resolve them, recursing into nested stacks and tracks.

use crate::{Clip, Item, Marker, Stack, Timeline, Track};

/// A clip with its position on the timeline.
#[derive(Debug, Clone, Copy)]
pub struct Placed<'a> {
    pub clip: &'a Clip,
    /// Index of the top-level track it is on (bottom first).
    pub track: usize,
    /// The top-level track's kind (`Video`, `Audio`).
    pub kind: &'a str,
    /// Timeline seconds.
    pub start: f64,
    pub duration: f64,
}

impl Placed<'_> {
    pub fn end(&self) -> f64 {
        self.start + self.duration
    }

    /// Timeline seconds for a time in the clip's source coordinates (its markers', keyframes').
    pub fn timeline_time(&self, source_seconds: f64) -> f64 {
        let in_point = self
            .clip
            .source_range
            .as_ref()
            .map_or(0.0, |r| r.start_time.to_seconds());
        self.start + source_seconds - in_point
    }
}

/// A marker at timeline seconds.
#[derive(Debug, Clone, Copy)]
pub struct PlacedMarker<'a> {
    pub marker: &'a Marker,
    pub start: f64,
    pub duration: f64,
}

fn duration(item: &Item) -> f64 {
    match item {
        Item::Transition(_) | Item::Unknown(_) => 0.0,
        Item::Stack(s) => s
            .source_range
            .as_ref()
            .map_or_else(|| stack_duration(s), |r| r.duration.to_seconds()),
        Item::Track(t) => t
            .source_range
            .as_ref()
            .map_or_else(|| track_duration(t), |r| r.duration.to_seconds()),
        _ => item.source_range().map_or(0.0, |r| r.duration.to_seconds()),
    }
}

fn track_duration(t: &Track) -> f64 {
    t.children.iter().map(duration).sum()
}

fn stack_duration(s: &Stack) -> f64 {
    s.children.iter().map(duration).fold(0.0, f64::max)
}

struct Walk<'a, 'o> {
    track: usize,
    kind: &'a str,
    clips: &'o mut Vec<Placed<'a>>,
    markers: &'o mut Vec<PlacedMarker<'a>>,
}

impl<'a> Walk<'a, '_> {
    fn markers(&mut self, ms: &'a [Marker], origin: f64) {
        for m in ms {
            self.markers.push(PlacedMarker {
                marker: m,
                start: origin + m.marked_range.start_time.to_seconds(),
                duration: m.marked_range.duration.to_seconds(),
            });
        }
    }

    /// `item` placed at `at`.
    fn item(&mut self, item: &'a Item, at: f64) {
        match item {
            Item::Clip(c) => {
                let p = Placed {
                    clip: c,
                    track: self.track,
                    kind: self.kind,
                    start: at,
                    duration: duration(item),
                };
                self.markers(&c.markers, p.timeline_time(0.0));
                self.clips.push(p);
            }
            Item::Gap(g) => self.markers(&g.markers, at),
            Item::Track(t) => {
                let origin = at
                    - t.source_range
                        .as_ref()
                        .map_or(0.0, |r| r.start_time.to_seconds());
                self.markers(&t.markers, origin);
                self.track_items(&t.children, origin);
            }
            Item::Stack(s) => {
                let origin = at
                    - s.source_range
                        .as_ref()
                        .map_or(0.0, |r| r.start_time.to_seconds());
                self.markers(&s.markers, origin);
                for child in &s.children {
                    self.item(child, origin);
                }
            }
            Item::Transition(_) | Item::Unknown(_) => {}
        }
    }

    fn track_items(&mut self, items: &'a [Item], origin: f64) {
        let mut at = origin;
        for item in items {
            self.item(item, at);
            at += duration(item);
        }
    }
}

impl Timeline {
    fn walk(&self) -> (Vec<Placed<'_>>, Vec<PlacedMarker<'_>>) {
        let (mut clips, mut markers) = (Vec::new(), Vec::new());
        for m in &self.tracks.markers {
            markers.push(PlacedMarker {
                marker: m,
                start: m.marked_range.start_time.to_seconds(),
                duration: m.marked_range.duration.to_seconds(),
            });
        }
        for (i, child) in self.tracks.children.iter().enumerate() {
            let kind = match child {
                Item::Track(t) => t.kind.as_str(),
                _ => "",
            };
            let mut w = Walk {
                track: i,
                kind,
                clips: &mut clips,
                markers: &mut markers,
            };
            w.item(child, 0.0);
        }
        (clips, markers)
    }

    /// Every clip with its timeline position, track by track (bottom first), in time order
    /// within each track.
    pub fn placed_clips(&self) -> Vec<Placed<'_>> {
        self.walk().0
    }

    /// Every marker (on the timeline's stack, tracks, gaps and clips) in timeline seconds,
    /// sorted by time.
    pub fn placed_markers(&self) -> Vec<PlacedMarker<'_>> {
        let mut m = self.walk().1;
        m.sort_by(|a, b| a.start.total_cmp(&b.start));
        m
    }

    /// Length in seconds: the longest top-level track.
    pub fn duration(&self) -> f64 {
        stack_duration(&self.tracks)
    }

    /// The timeline's frame rate: `project.rate`, else the first clip's.
    pub fn rate(&self) -> Option<f64> {
        self.project()
            .and_then(Result::ok)
            .and_then(|p| p.rate)
            .or_else(|| {
                self.clips()
                    .iter()
                    .find_map(|c| c.source_range.as_ref().map(|r| r.duration.rate))
            })
            .filter(|r| *r > 0.0)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Clip, Gap, Item, Marker, MediaReference, MissingReference, RationalTime, TimeRange,
        Timeline, Track, Transition, track_kind,
    };

    fn clip(name: &str, start: f64, end: f64) -> Item {
        Item::Clip(Clip::new(
            name,
            MediaReference::Missing(MissingReference::default()),
            Some(TimeRange::from_frames(start, end, 24.0)),
        ))
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn positions_skip_transitions_and_place_clip_markers() {
        let mut v = Track::new("V1", track_kind::VIDEO);
        v.children.push(clip("a", 100.0, 148.0));
        v.children.push(Item::Transition(Transition::dissolve(
            RationalTime::new(0.0, 24.0),
            RationalTime::new(12.0, 24.0),
        )));
        let Item::Clip(mut b) = clip("b", 0.0, 24.0) else {
            unreachable!()
        };
        b.markers.push(Marker::new(
            "m",
            TimeRange::from_frames(12.0, 12.0, 24.0),
            "RED",
        ));
        v.children.push(Item::Clip(b));
        let mut t2 = Track::new("Overlays", track_kind::VIDEO);
        t2.children
            .push(Item::Gap(Gap::new(RationalTime::new(36.0, 24.0))));
        t2.children.push(clip("c", 0.0, 24.0));
        let mut t = Timeline::default();
        t.tracks.children.push(Item::Track(v));
        t.tracks.children.push(Item::Track(t2));

        let p = t.placed_clips();
        let at: Vec<_> = p
            .iter()
            .map(|p| (p.clip.name.as_str(), p.track, p.start))
            .collect();
        assert_eq!(at, [("a", 0, 0.0), ("b", 0, 2.0), ("c", 1, 1.5)]);
        let m = t.placed_markers();
        assert_eq!(m.len(), 1);
        assert!((m[0].start - 2.5).abs() < 1e-9);
        assert!((t.duration() - 3.0).abs() < 1e-9);
        assert_eq!(t.rate(), Some(24.0));
    }
}
