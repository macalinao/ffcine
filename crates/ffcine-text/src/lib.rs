//! `ffcine.text` generator clips (place cards, time cards, subtitles, titles) rendered as GPU
//! overlays, only over their own time ranges: see `docs/text.md`.
//!
//! [`TextLayer`] collects a timeline's text clips with their resolved styles
//! (`metadata.ffcine.project.styles`); [`TextLayer::overlays`] lays out the ones overlapping a
//! segment with `cosmic-text` and returns [`Overlay`]s that the render pipeline composites in its
//! libplacebo pass ([`ffcine_render::Pipeline::overlays`]). Segment renderers split at
//! [`TextLayer::cuts`] and key their cache on [`TextLayer::fingerprint`], so a card edit
//! re-renders only the seconds it covers. [`ass`] imports `.ass` subtitles as text clips.
//!
//! ```no_run
//! use ffcine_render::{Pipeline, Size};
//! use ffcine_text::TextLayer;
//! use ffcine_timeline::Timeline;
//!
//! let t = Timeline::from_file("edit.otio")?;
//! let mut text = TextLayer::from_timeline(&t, ".")?;
//! // A segment of 96 frames starting 12.5 s into the timeline, rendered at 4K.
//! let size = Size { width: 3840, height: 2160 };
//! let overlays = text.overlays(12.5, 96, 24000.0 / 1001.0, size)?;
//! let p = Pipeline::new("shot.mov", "seg.mkv").overlays(overlays);
//! # Ok::<(), ffcine_core::Error>(())
//! ```

pub mod ass;
pub mod layout;
pub mod style;

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use ffcine_core::Result;
use ffcine_render::{Overlay, Size};
use ffcine_timeline::{HasMetadata, Item, MediaReference, Timeline};
use serde_json::{Map, Value};

pub use layout::{Layer, Rendered, Typesetter};
pub use style::{Align, TextStyle};

/// `generator_kind` of text clips.
pub const GENERATOR: &str = "ffcine.text";

/// One `ffcine.text` clip on the timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct TextClip {
    /// `metadata.ffcine.id`, if any.
    pub id: Option<String>,
    pub name: String,
    pub text: String,
    /// Timeline seconds.
    pub start: f64,
    pub end: f64,
    /// Top-level track index (higher draws on top).
    pub track: usize,
    /// Merged parameters: `styles.base` <- style chain <- the clip's (see [`style::cascade`]).
    pub params: Map<String, Value>,
}

impl TextClip {
    /// The style for a frame of `frame_size` px.
    pub fn style(&self, frame_size: [f32; 2]) -> TextStyle {
        style::resolve(&self.params, frame_size, None)
    }
}

/// A timeline's text clips, ready to render per segment.
#[derive(Debug)]
pub struct TextLayer {
    clips: Vec<TextClip>,
    /// `metadata.ffcine.frame_size`: what `size` and other px lengths are relative to.
    frame_size: Option<[f32; 2]>,
    base_dir: PathBuf,
    typesetter: Option<Typesetter>,
    /// Output profile name and safe area, see [`TextLayer::set_profile`].
    profile: Option<(String, [f32; 4])>,
}

impl TextLayer {
    /// Collects the enabled `ffcine.text` clips of `timeline`; font files resolve against
    /// `base_dir` (the timeline's directory).
    pub fn from_timeline(timeline: &Timeline, base_dir: impl Into<PathBuf>) -> Result<Self> {
        let styles = timeline
            .project()
            .transpose()?
            .map(|p| p.styles)
            .unwrap_or_default();
        let frame_size = timeline
            .metadata
            .get("ffcine")
            .and_then(|f| f.get("frame_size"))
            .and_then(Value::as_array)
            .and_then(|a| {
                #[allow(clippy::cast_possible_truncation)]
                let g = |i: usize| a.get(i).and_then(Value::as_f64).map(|v| v as f32);
                Some([g(0)?, g(1)?])
            });
        let enabled = |i: usize| match timeline.tracks.children.get(i) {
            Some(Item::Track(t)) => t.enabled,
            _ => true,
        };
        let mut clips: Vec<TextClip> = timeline
            .placed_clips()
            .into_iter()
            .filter(|p| p.clip.enabled && enabled(p.track))
            .filter_map(|p| {
                let MediaReference::Generator(g) = p.clip.media_reference()? else {
                    return None;
                };
                (g.generator_kind == GENERATOR).then(|| TextClip {
                    id: p.clip.ffcine_id().map(str::to_owned),
                    name: p.clip.name.clone(),
                    text: g
                        .parameters
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    start: p.start,
                    end: p.end(),
                    track: p.track,
                    params: style::cascade(&styles, &g.parameters),
                })
            })
            .collect();
        clips.sort_by(|a, b| a.track.cmp(&b.track).then(a.start.total_cmp(&b.start)));
        Ok(Self {
            clips,
            frame_size,
            base_dir: base_dir.into(),
            typesetter: None,
            profile: None,
        })
    }

    /// Renders for output profile `name` (`reel`, `feed`, ...): a clip's `profiles.<name>`
    /// parameters override the rest. When the output's aspect differs from `frame_size`, lengths
    /// scale with the short side instead of the height and each text box is moved inside `safe`
    /// (top, right, bottom, left margins as fractions of the output).
    pub fn set_profile(&mut self, name: &str, safe: [f32; 4]) {
        self.profile = Some((name.to_owned(), safe));
    }

    pub fn clips(&self) -> &[TextClip] {
        &self.clips
    }

    /// Clips overlapping `start..end` (timeline seconds).
    pub fn in_range(&self, start: f64, end: f64) -> impl Iterator<Item = &TextClip> {
        self.clips
            .iter()
            .filter(move |c| c.start < end && c.end > start)
    }

    /// Every text in and out point, sorted: where a segment renderer should cut so each
    /// segment's text is fixed.
    pub fn cuts(&self) -> Vec<f64> {
        let mut v: Vec<f64> = self.clips.iter().flat_map(|c| [c.start, c.end]).collect();
        v.sort_by(f64::total_cmp);
        v.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
        v
    }

    /// A hash of the text drawn over `start..end`: the clips' parameters and their times
    /// relative to `start`. Equal fingerprints render equal pixels, so it belongs in a segment's
    /// cache key.
    pub fn fingerprint(&self, start: f64, end: f64) -> u64 {
        let mut h = DefaultHasher::new();
        for c in self.in_range(start, end) {
            // Sizes and positions are relative to it.
            self.frame_size.map(|f| f.map(f32::to_bits)).hash(&mut h);
            ((c.start - start) * 1e4).round().to_bits().hash(&mut h);
            ((c.end - start) * 1e4).round().to_bits().hash(&mut h);
            c.track.hash(&mut h);
            serde_json::to_string(&c.params)
                .unwrap_or_default()
                .hash(&mut h);
        }
        h.finish()
    }

    /// The overlays for a segment of `frames` frames at `rate` starting at timeline second
    /// `start`, rendered at `out` px. Frame ranges are relative to the segment's first frame
    /// (they may start before 0 or end past `frames`, so fades carry across segment cuts).
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn overlays(
        &mut self,
        start: f64,
        frames: u64,
        rate: f64,
        out: Size,
    ) -> Result<Vec<Overlay>> {
        let end = start + frames as f64 / rate;
        let frame = self
            .frame_size
            .unwrap_or([out.width as f32, out.height as f32]);
        let (ow, oh) = (out.width as f32, out.height as f32);
        let reshaped = (frame[0] / frame[1] - ow / oh).abs() > 0.01;
        let scale = if reshaped {
            ow.min(oh) / frame[0].min(frame[1])
        } else {
            oh / frame[1]
        };
        let ts = self
            .typesetter
            .get_or_insert_with(|| Typesetter::new(self.base_dir.clone()));
        let mut v = Vec::new();
        for c in self.clips.iter().filter(|c| c.start < end && c.end > start) {
            let s = match &self.profile {
                Some((name, _)) => style::resolve(&for_profile(&c.params, name), frame, None),
                None => c.style(frame),
            };
            let from = ((c.start + s.delay - start) * rate).round() as i64;
            let to = ((c.end - start) * rate).round() as i64;
            if to <= from || to <= 0 || from >= i64::try_from(frames).unwrap_or(i64::MAX) {
                continue;
            }
            let Some(r) = ts.render(&c.text, &s, scale, [out.width, out.height])? else {
                continue;
            };
            let shift = match &self.profile {
                Some((_, safe)) if reshaped => into_safe(&r, *safe, [ow, oh]),
                _ => [0.0, 0.0],
            };
            for mut l in r.layers {
                for p in &mut l.parts {
                    p.pos = [p.pos[0] + shift[0], p.pos[1] + shift[1]];
                }
                v.push(Overlay {
                    mask: l.mask,
                    parts: l.parts,
                    from,
                    to,
                    fade_in: s.fade_in * rate,
                    fade_out: s.fade_out * rate,
                });
            }
        }
        Ok(v)
    }
}

/// `params` with its `profiles.<name>` object merged over it (and `profiles` dropped).
fn for_profile(params: &Map<String, Value>, name: &str) -> Map<String, Value> {
    let mut m = params.clone();
    let over = m.remove("profiles");
    if let Some(Value::Object(top)) = over.as_ref().and_then(|p| p.get(name)) {
        style::merge(&mut m, top);
    }
    m
}

/// The move that puts a rendered text box inside the `safe` margins (top, right, bottom, left,
/// fractions) of an `[w, h]` frame; a box bigger than the area is centred on it.
fn into_safe(r: &layout::Rendered, safe: [f32; 4], [w, h]: [f32; 2]) -> [f32; 2] {
    let [top, right, bottom, left] = safe;
    let axis = |pos: f32, size: f32, lo: f32, hi: f32| {
        if size > hi - lo {
            (lo + hi - size) / 2.0 - pos
        } else if pos < lo {
            lo - pos
        } else if pos + size > hi {
            hi - (pos + size)
        } else {
            0.0
        }
    };
    [
        axis(r.origin[0], r.size[0], left * w, w - right * w),
        axis(r.origin[1], r.size[1], top * h, h - bottom * h),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_timeline::{
        Clip, Gap, GeneratorReference, Project, RationalTime, TimeRange, Track, track_kind,
    };
    use serde_json::json;

    pub(crate) fn text_clip(text: &str, style: &str, frames: f64) -> Item {
        let p = json!({"text": text, "style": style});
        Item::Clip(Clip::new(
            text,
            MediaReference::Generator(GeneratorReference::new(
                GENERATOR,
                p.as_object().cloned().unwrap_or_default(),
            )),
            Some(TimeRange::from_frames(0.0, frames, 24.0)),
        ))
    }

    fn timeline() -> Timeline {
        let mut tr = Track::new("Overlays 1", track_kind::VIDEO);
        tr.children
            .push(Item::Gap(Gap::new(RationalTime::new(48.0, 24.0))));
        tr.children.push(text_clip("Arrival", "card", 72.0));
        let mut t = Timeline::default();
        t.tracks.children.push(Item::Track(tr));
        let p = Project {
            styles: json!({"card": {"size": 0.1, "x": 0.05, "y": 0.15, "fade_in": 0.5}})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ..Project::default()
        };
        t.set_project(&p).expect("project");
        t
    }

    #[test]
    fn collects_clips_cuts_and_fingerprints() {
        let t = timeline();
        let layer = TextLayer::from_timeline(&t, ".").expect("layer");
        assert_eq!(layer.clips().len(), 1);
        assert_eq!(layer.cuts(), [2.0, 5.0]);
        assert_eq!(layer.clips()[0].params["size"], 0.1);
        assert_eq!(layer.fingerprint(0.0, 1.0), DefaultHasher::new().finish());
        // Same text at the same relative time: same fingerprint.
        assert_eq!(layer.fingerprint(0.0, 3.0), layer.fingerprint(0.0, 3.5));
        assert_ne!(layer.fingerprint(0.0, 3.0), layer.fingerprint(1.0, 3.0));
        let mut sized = TextLayer::from_timeline(&t, ".").expect("layer");
        sized.frame_size = Some([1080.0, 1920.0]);
        assert_ne!(sized.fingerprint(0.0, 3.0), layer.fingerprint(0.0, 3.0));
    }

    #[test]
    fn overlays_only_in_range() {
        let t = timeline();
        let mut layer = TextLayer::from_timeline(&t, ".").expect("layer");
        let out = Size {
            width: 1920,
            height: 1080,
        };
        assert_eq!(layer.overlays(0.0, 24, 24.0, out).expect("ov"), []);
        // cosmic-text panics shaping with no fonts at all.
        if !Typesetter::new(".").has_fonts() {
            eprintln!("no system fonts; skipped");
            return;
        }
        let ov = layer.overlays(1.0, 48, 24.0, out).expect("ov");
        assert_eq!((ov[0].from, ov[0].to), (24, 96));
        assert!((ov[0].fade_in - 12.0).abs() < 1e-9);
    }

    #[test]
    fn profile_overrides_and_safe_area() {
        let p = json!({"size": 40, "position": [0.0, 0.4],
                       "profiles": {"reel": {"position": [0.0, 0.2]}}});
        let p = p.as_object().unwrap();
        let reel = for_profile(p, "reel");
        assert_eq!(reel.get("position"), Some(&json!([0.0, 0.2])));
        assert!(!reel.contains_key("profiles"));
        assert_eq!(
            for_profile(p, "feed").get("position"),
            Some(&json!([0.0, 0.4]))
        );
        // A box under the reels' bottom UI moves up into the safe area; one inside stays.
        let r = |origin: [f32; 2], size: [f32; 2]| layout::Rendered {
            layers: Vec::new(),
            origin,
            size,
        };
        let safe = [0.115, 0.11, 0.22, 0.055];
        let [dx, dy] = into_safe(&r([100.0, 1700.0], [300.0, 80.0]), safe, [1080.0, 1920.0]);
        assert!(dx.abs() < 1e-3);
        assert!((1700.0 + dy + 80.0 - 1920.0 * 0.78).abs() < 1e-2);
        assert_eq!(
            into_safe(&r([300.0, 900.0], [300.0, 80.0]), safe, [1080.0, 1920.0]),
            [0.0, 0.0]
        );
    }
}
