//! Segment planner: flattens a timeline's picture track into render units (one per clip, one per
//! dissolve overlap, one per gap) and its audio tracks into one mix, each keyed by content.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ffcine_audio::schema::{CLEAN, Clean, clip_effect};
use ffcine_audio::{MixOptions, sources};

use ffcine_timeline::project::GRADE_SCHEMA;
use ffcine_timeline::{
    Clip, ClipGrade, Effect, GradeParams, HasMetadata, Item, MediaReference, Project, SourceKind,
    Timeline, Track, track_kind,
};
use serde_json::{Map, Value, json};

use ffcine_filters::{Denoise, Motion, Spatial, Temporal};
use ffcine_reframe::{Geo, Reframe};
use ffcine_stabilize::{DetectParams, StabilizeParams};

use crate::comp::{Finish, TransitionKind};
use crate::profile::Profile;
use crate::recipe::{AUDIO_REVISION, Fingerprint, Fingerprinter, Key, RENDER_REVISION};
pub use ffcine_text::TextLayer;

/// `ffcine.time.ramp/1`.
pub const RAMP_SCHEMA: &str = "ffcine.time.ramp/1";

/// The generator kind of a comp clip in a timeline ([`plan_with`]).
pub const COMP_GENERATOR: &str = "ffcine.comp";

/// Errors from [`plan`].
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("timeline has no video track")]
    NoVideo,
    #[error("timeline has no frame rate (project.rate or a clip's)")]
    NoRate,
    #[error("project metadata: {0}")]
    Project(#[from] ffcine_core::Error),
    #[error("audio: {0}")]
    Audio(ffcine_core::Error),
    /// A comp piece that doesn't lower (a parameter out of range, an unreadable layer source).
    #[error("comp: {0}")]
    Comp(ffcine_core::Error),
    /// An `ffcine.comp` clip that can't be read or doesn't fit the timeline.
    #[error("comp clip {clip:?}: {msg}")]
    CompClip { clip: String, msg: String },
    #[error("clip {clip:?}: {source}")]
    Grade {
        clip: String,
        source: ffcine_timeline::project::GradeError,
    },
    #[error("clip {clip:?}: {effect}: {msg}")]
    Effect {
        clip: String,
        effect: &'static str,
        msg: String,
    },
}

/// A speed ramp (`ffcine.time.ramp/1`): `points [(u, speed)]` piecewise linear over the clip's
/// `source_duration` seconds of source (u 0..1).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Ramp {
    pub points: Vec<(f64, f64)>,
    pub source_duration: f64,
}

impl Ramp {
    /// `render.py` `ramp_map`: `(t0, t1, s0, s1, out0)` per source-time segment, output time =
    /// `out0 + integral(dt / speed)`.
    pub fn segments(&self) -> Vec<(f64, f64, f64, f64, f64)> {
        let mut segs = Vec::new();
        let mut out = 0.0;
        for w in self.points.windows(2) {
            let ((u0, a), (u1, b)) = (w[0], w[1]);
            let (t0, t1) = (u0 * self.source_duration, u1 * self.source_duration);
            if t1 - t0 <= 1e-6 {
                continue;
            }
            segs.push((t0, t1, a, b, out));
            let m = (b - a) / (t1 - t0);
            out += if m.abs() < 1e-9 {
                (t1 - t0) / a
            } else {
                (b / a).ln() / m
            };
        }
        segs
    }

    /// Source seconds since the clip's first frame shown at output second `t` (the inverse of
    /// [`Ramp::segments`]' map).
    pub fn source_time(&self, t: f64) -> f64 {
        let mut last = 0.0;
        for (t0, t1, a, b, o0) in self.segments() {
            let m = (b - a) / (t1 - t0);
            let dt = t - o0;
            let x = if m.abs() < 1e-9 {
                t0 + dt * a
            } else {
                t0 + (a * (m * dt).exp() - a) / m
            };
            if x <= t1 {
                return x.max(t0);
            }
            last = t1;
        }
        last
    }

    /// ffmpeg `setpts` expression mapping source time since the first frame to output time
    /// (`render.py` `ramp_setpts`).
    pub fn setpts(&self) -> String {
        self.setpts_of("((PTS-STARTPTS)*TB)")
    }

    /// [`Ramp::setpts`] with the source time given by the expression `x`.
    pub fn setpts_of(&self, x: &str) -> String {
        let mut expr: Option<String> = None;
        for (t0, t1, a, b, o0) in self.segments().into_iter().rev() {
            let m = (b - a) / (t1 - t0);
            let e = if m.abs() < 1e-9 {
                format!("({o0:.6}+({x}-{t0:.6})/{a:.6})")
            } else {
                format!("({o0:.6}+log(({a:.6}+{m:.6}*({x}-{t0:.6}))/{a:.6})/{m:.6})")
            };
            expr = Some(match expr {
                None => e,
                Some(rest) => format!("if(lt({x},{t1:.6}),{e},{rest})"),
            });
        }
        format!("setpts='({})/TB'", expr.unwrap_or_else(|| x.to_owned()))
    }
}

/// A clip's picture filters, read from its effects and resolved for the render profile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filters {
    /// `ffcine.fx.stabilize/1`: GPU optical-flow detect over the clip, smoothed, warped in the
    /// grade pass.
    pub stabilize: Option<StabilizeParams>,
    /// `ffcine.fx.denoise/1`, else the grade's `denoise` number: temporal (NVOF motion) and
    /// spatial denoise on the decoded planes.
    pub denoise: Option<Denoise>,
    /// `ffcine.geo.reframe/1` with the profile's `profiles.<aspect>` override applied.
    pub reframe: Option<Reframe>,
    /// `ffcine.time.ramp/1`: the clip is retimed through the ramp, not its average speed.
    pub ramp: Option<Ramp>,
    /// `ffcine.time.blend/1` `frames` (> 1): each output frame is the mean of the last `frames`
    /// frames at the timeline rate (the prototype's `tmix`), history from before the piece.
    pub blend: Option<u32>,
    /// `ffcine.geo.crop/1` and `ffcine.geo.transform/1` (keyed through the clip's `fx`).
    pub geo: Geo,
}

impl Filters {
    /// Whether a piece of the clip depends on where it sits in the clip (stabilized path,
    /// animated reframe, ramp).
    fn whole_clip(&self) -> bool {
        self.stabilize.is_some()
            || self.reframe.is_some()
            || self.ramp.is_some()
            || self.geo.is_animated()
    }

    /// Everything about the filters that changes pixels, including the inputs they derive
    /// from (the stabilizer's detect settings and motion cache format).
    fn recipe(&self) -> Value {
        json!({
            "stabilize": self.stabilize.map(|p| json!({
                "smoothing": p.smoothing,
                "zoom": format!("{:?}", p.zoom),
                "detect": format!("{:?}", DetectParams::default()),
                "motion_cache": ffcine_stabilize::cache::FORMAT_VERSION,
            })),
            "denoise": self.denoise,
            "reframe": self.reframe.as_ref().map(|r| format!("{r:?}")),
            "ramp": self.ramp,
            "blend": self.blend,
        })
    }
}

/// `GradeParams.denoise` (the edit.yaml / `render.py` hqdn3d number `d`) as the GPU denoiser:
/// temporal (NVOF, radius 2) at strength `d`, spatial NLM at `0.2 d` (the prototype's
/// `nlmeans_vulkan s=0.2*d`).
#[allow(clippy::cast_possible_truncation)]
pub fn grade_denoise(d: f64) -> Option<Denoise> {
    (d > 0.0).then(|| {
        let d = d as f32;
        Denoise {
            temporal: Some(Temporal {
                radius: 2,
                motion: Motion::Nvof,
                strength: d,
                chroma: None,
                protect: 0.0,
            }),
            spatial: Some(Spatial {
                radius: 2,
                luma: 0.2 * d,
                chroma: 0.3 * d,
                protect: 0.0,
            }),
        }
        .clamped()
    })
}

/// A clip's picture as one render input: a source range at a speed, graded.
#[derive(Debug, Clone, PartialEq)]
pub struct Shot {
    /// Display name (not part of the key).
    pub name: String,
    pub media: PathBuf,
    pub fingerprint: Fingerprint,
    /// Source time of the first frame, seconds.
    pub start: f64,
    /// Source seconds per timeline second: 1 normal, 0 freeze frame.
    pub speed: f64,
    /// Timeline frames.
    pub frames: i64,
    /// Resolved grade (base <- look <- source-kind rule <- clip).
    pub grade: GradeParams,
    /// The `.cube` file the grade's `lut` names.
    pub lut: Option<PathBuf>,
    /// Recipe of the resolved grade with the LUT by content.
    grade_recipe: Value,
    /// The clip's other ffcine effects (reframe, stabilize, ...), canonical, for the key.
    pub(crate) fx: Vec<Value>,
    /// Vignette, darken, fades, flash.
    pub finish: Finish,
    /// The clip's `ffcine.fx.grain/1`, overriding the timeline's.
    pub grain: Option<Map<String, Value>>,
    /// Frames from the clip's first frame to this one's (negative for the incoming side of a
    /// transition), and the clip's length: fades, and whole-clip filters (stabilized path,
    /// reframe animation, ramp) are evaluated over the clip, then sliced.
    pub clip_at: i64,
    pub clip_frames: i64,
    pub filters: Filters,
    /// Source time of the clip's first frame.
    pub clip_start: f64,
    /// Clip frames `[lo, hi)` the stabilizer analyses: every piece of the clip, so all of them
    /// share one camera path.
    pub span: (i64, i64),
    /// Which clip of the timeline the piece is from (not part of the key).
    clip_index: usize,
}

impl Shot {
    /// The same shot starting `k` timeline frames later (earlier if negative).
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn shifted(&self, k: i64, rate: f64) -> Self {
        Self {
            start: (self.start + k as f64 * self.speed / rate).max(0.0),
            frames: self.frames - k,
            clip_at: self.clip_at + k,
            ..self.clone()
        }
    }

    #[must_use]
    pub fn with_frames(mut self, n: i64) -> Self {
        self.frames = n;
        self
    }

    /// Frames of the clip before and after this piece that a temporal denoiser reads as history
    /// and lookahead: its radius on each side, fewer at the clip's ends.
    pub fn context(&self) -> (i64, i64) {
        let r = self
            .filters
            .denoise
            .filter(|d| !d.is_off())
            .map_or(0, |d| i64::from(d.radius()));
        if r == 0 || self.speed.abs() < 1e-9 {
            return (0, 0);
        }
        (
            r.min(self.clip_at).max(0),
            r.min(self.clip_frames - self.clip_at - self.frames).max(0),
        )
    }

    fn recipe(&self) -> Value {
        let mut v = json!({
            "media": self.fingerprint,
            "start": self.start,
            "speed": self.speed,
            "frames": self.frames,
            "grade": self.grade_recipe,
            "fx": self.fx,
            "filters": self.filters.recipe(),
        });
        // Fades, flashes and whole-clip filters depend on where the piece sits in its clip;
        // nothing else does, so trimming a clip's tail leaves its other pieces' keys alone.
        if self.finish.is_timed() || self.filters.whole_clip() {
            v["clip"] = json!([self.clip_at, self.clip_frames]);
        }
        if self.filters.whole_clip() {
            v["clip_start"] = json!(self.clip_start);
        }
        if self.filters.stabilize.is_some() {
            v["span"] = json!([self.span.0, self.span.1]);
        }
        let ctx = self.context();
        if ctx != (0, 0) {
            v["context"] = json!([ctx.0, ctx.1]);
        }
        v
    }
}

/// One stretch of a split-screen panel: a shot, or nothing (the backdrop shows).
#[derive(Debug, Clone, PartialEq)]
pub struct PanelPart {
    /// Frames from the split's start.
    pub from: i64,
    pub frames: i64,
    pub shot: Option<Shot>,
}

/// A split-screen (`Stack` with `ffcine.comp.layout/1`), or a piece of one.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    /// `ffcine.comp.layout/1` parameters.
    pub layout: Map<String, Value>,
    /// Per panel, its parts covering the split.
    pub panels: Vec<Vec<PanelPart>>,
    /// The stack's own finish (vignette, fades).
    pub finish: Finish,
    /// Frames from the split's start to this piece's, and the split's length.
    pub at: i64,
    pub total: i64,
}

impl Split {
    fn recipe(&self) -> Value {
        let panels: Vec<Vec<Value>> = self
            .panels
            .iter()
            .map(|p| {
                p.iter()
                    .map(|part| {
                        json!({"from": part.from, "frames": part.frames,
                               "shot": part.shot.as_ref().map(Shot::recipe)})
                    })
                    .collect()
            })
            .collect();
        json!({"layout": self.layout, "panels": panels, "finish": format!("{:?}", self.finish),
               "at": self.at, "total": self.total})
    }
}

/// What an overlay clip shows.
#[allow(clippy::large_enum_variant)] // a few per timeline
#[derive(Debug, Clone, PartialEq)]
pub enum OverlaySource {
    /// Picture-in-picture: a video clip, graded like a shot.
    Video(Shot),
    /// A still (PNG logo, sticker), drawn as is.
    Image {
        path: PathBuf,
        fingerprint: Fingerprint,
    },
}

/// A non-text clip on a video track above the picture.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayClip {
    pub name: String,
    pub source: OverlaySource,
    /// Timeline frame of its first frame, and its length.
    pub start: i64,
    pub frames: i64,
    /// `source_range.start_time` in seconds: where keyframe time starts.
    pub source_start: f64,
    /// `ffcine.geo.transform/1` and `ffcine.comp.blend/1` parameters.
    pub transform: Map<String, Value>,
    pub blend: Map<String, Value>,
}

/// An overlay clip's part within one segment.
#[derive(Debug, Clone, PartialEq)]
pub struct SegLayer {
    pub clip: Arc<OverlayClip>,
    /// Segment frames `from..to`.
    pub from: i64,
    pub to: i64,
    /// Frames from the clip's first frame to `from`.
    pub clip_at: i64,
}

impl SegLayer {
    /// The video shot for this part (shifted into the clip), `None` for images.
    pub fn shot(&self, rate: f64) -> Option<Shot> {
        match &self.clip.source {
            OverlaySource::Video(s) => Some(
                s.shifted(self.clip_at, rate)
                    .with_frames(self.to - self.from),
            ),
            OverlaySource::Image { .. } => None,
        }
    }

    fn recipe(&self, rate: f64) -> Value {
        let src = match &self.clip.source {
            OverlaySource::Video(_) => self.shot(rate).map(|s| s.recipe()).unwrap_or_default(),
            OverlaySource::Image { fingerprint, .. } => json!({"image": fingerprint}),
        };
        json!({"source": src, "transform": self.clip.transform, "blend": self.clip.blend,
               "source_start": self.clip.source_start, "clip_at": self.clip_at,
               "from": self.from, "to": self.to})
    }
}

/// What a segment renders.
#[allow(clippy::large_enum_variant)] // a few per timeline
#[derive(Debug, Clone, PartialEq)]
pub enum Unit {
    Shot(Shot),
    /// Black.
    Gap,
    /// A transition from `a` to `b`, both `frames` long.
    Transition {
        a: Shot,
        b: Shot,
        kind: TransitionKind,
    },
    /// A split-screen.
    Split(Split),
    /// A dissolve between a split-screen and a shot (`split_out`: the split is outgoing).
    SplitDissolve {
        split: Split,
        shot: Shot,
        split_out: bool,
    },
    /// A piece of a comp (`ffcine.comp/1`), lowered.
    Comp(Arc<ffcine_comp::Piece>),
}

impl Unit {
    fn recipe(&self) -> Value {
        match self {
            Self::Shot(s) => json!({"shot": s.recipe()}),
            Self::Gap => json!("gap"),
            // The original form for dissolves, so their keys only change with the revision.
            Self::Transition {
                a,
                b,
                kind: TransitionKind::Dissolve,
            } => json!({"dissolve": [a.recipe(), b.recipe()]}),
            Self::Transition { a, b, kind } => {
                json!({"transition": [a.recipe(), b.recipe()], "kind": kind.recipe()})
            }
            Self::Split(s) => json!({"split": s.recipe()}),
            Self::SplitDissolve {
                split,
                shot,
                split_out,
            } => json!({"split_dissolve": [split.recipe(), shot.recipe()], "split_out": split_out}),
            Self::Comp(p) => p.recipe.clone(),
        }
    }
}

/// One render unit of the picture.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub key: Key,
    pub name: String,
    /// Timeline frame it starts at (display only, not in the key).
    pub start: i64,
    pub frames: i64,
    pub unit: Unit,
    /// Overlay clips over it (picture-in-picture, stickers), bottom first.
    pub layers: Vec<SegLayer>,
}

/// The timeline's audio: one `ffcine_audio::render_audio` mix (clean chain, R128 levelling,
/// buses with ducking, synced recorder clips, master limiter), keyed by its recipe: every clip
/// it reads (placement, media identity, `ffcine.audio.*` effects, sync) and the mix options.
/// Nothing about the picture's grade or effects is in it, and it isn't in any segment key.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioPlan {
    pub key: Key,
    /// Clips mixed.
    pub clips: usize,
    /// What the mix reads.
    pub timeline: Arc<Timeline>,
    /// Defaults <- `project.mix` <- overrides ([`AudioPlan::set_options`]); `length` is the
    /// picture's.
    pub options: MixOptions,
    /// The clip part of the key.
    inputs: Value,
}

impl AudioPlan {
    fn key_of(inputs: &Value, options: &MixOptions) -> Key {
        Key::of(&json!({
            "rev": AUDIO_REVISION,
            "ffcine": env!("CARGO_PKG_VERSION"),
            "options": options,
            "clips": inputs,
        }))
    }

    /// Replaces the mix options (CLI overrides of `project.mix`) and re-keys. `base` and
    /// `length` stay the plan's.
    pub fn set_options(&mut self, options: MixOptions) {
        self.options = MixOptions {
            base: self.options.base.clone(),
            length: self.options.length,
            ..options
        };
        self.key = Self::key_of(&self.inputs, &self.options);
    }
}

/// A planned render.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Timeline frame rate.
    pub rate: f64,
    pub profile: Profile,
    pub segments: Vec<Segment>,
    pub audio: Option<AudioPlan>,
    /// The timeline's film grain (`ffcine.fx.grain/1` on its top-level stack, with the
    /// profile's override folded in), on every segment.
    pub grain: Option<Map<String, Value>>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn frames(&self) -> i64 {
        self.segments.iter().map(|s| s.frames).sum()
    }

    /// Key of the joined picture: the segment keys in order.
    pub fn program_key(&self) -> Key {
        let keys: Vec<&str> = self.segments.iter().map(|s| s.key.0.as_str()).collect();
        Key::of(&json!({"program": keys, "rate": self.rate}))
    }

    /// Key of the muxed master: picture + audio + audio encode.
    pub fn master_key(&self) -> Key {
        Key::of(&json!({
            "rev": RENDER_REVISION,
            "program": self.program_key(),
            "audio": self.audio.as_ref().map(|a| &a.key),
            "audio_bitrate": self.profile.audio_bitrate,
        }))
    }
}

/// Plans `timeline` (its file's directory is `base_dir`, for relative LUT paths) for `profile`.
/// With `text`, segments are cut at every text in/out point and keyed by the text they show,
/// so a card edit re-renders only the seconds it covers.
///
/// The picture is the first video track. Split-screen `Stack`s render as their first panel until
/// compositing lands; other video tracks are text (`TextLayer`) or not yet composited (warned).
/// `ffcine.comp` clips need probed media: plan those timelines with [`plan_with`].
pub fn plan(
    timeline: &Timeline,
    base_dir: &Path,
    profile: &Profile,
    fp: &dyn Fingerprinter,
    text: Option<&TextLayer>,
) -> Result<Plan, PlanError> {
    plan_with(timeline, base_dir, profile, fp, text, None)
}

/// Whether `timeline` has an `ffcine.comp` clip (so [`plan_with`] needs `media`).
pub fn has_comp_clips(timeline: &Timeline) -> bool {
    timeline.clips().iter().any(|c| comp_clip(c).is_some())
}

/// `clip`'s `ffcine.comp` generator parameters, if it is one.
fn comp_clip(clip: &Clip) -> Option<&Map<String, Value>> {
    match clip.media_reference()? {
        MediaReference::Generator(g) if g.generator_kind == COMP_GENERATOR => Some(&g.parameters),
        _ => None,
    }
}

/// [`plan`], with `media` to lower `ffcine.comp` clips (`docs/timeline.md`): a clip whose
/// `GeneratorReference` is `ffcine.comp` with parameters `{"schema": "ffcine.comp/1", "src":
/// PATH}` (relative to the timeline) or the comp document itself, inline. Its `source_range` is
/// comp time; each comp piece it covers is a segment ([`Unit::Comp`]) keyed by its lowered
/// layers, as [`plan_comp`]'s are.
#[allow(clippy::too_many_lines)]
pub fn plan_with(
    timeline: &Timeline,
    base_dir: &Path,
    profile: &Profile,
    fp: &dyn Fingerprinter,
    text: Option<&TextLayer>,
    media: Option<&dyn ffcine_comp::Media>,
) -> Result<Plan, PlanError> {
    let project = match timeline.project() {
        Some(p) => p?,
        None => Project::default(),
    };
    let rate = project
        .rate
        .or_else(|| {
            timeline
                .clips()
                .iter()
                .find_map(|c| c.source_range.as_ref().map(|r| r.duration.rate))
        })
        .filter(|r| *r > 0.0)
        .ok_or(PlanError::NoRate)?;
    let mut p = Planner {
        project: &project,
        base_dir,
        rate,
        fp,
        profile,
        media,
        clips: 0,
        warnings: Vec::new(),
    };
    let videos: Vec<&Track> = timeline
        .track_list()
        .filter(|t| t.kind == track_kind::VIDEO && t.enabled)
        .collect();
    let picture = videos.first().ok_or(PlanError::NoVideo)?;
    let elems = p.flatten(&picture.children)?;
    let mut units = p.resolve_transitions(elems);
    let overlays: Vec<Arc<OverlayClip>> = videos
        .iter()
        .skip(1)
        .map(|t| p.overlays(t))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .map(Arc::new)
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    let mut cuts: Vec<i64> = text
        .map(|t| t.cuts().iter().map(|c| (c * rate).round() as i64).collect())
        .unwrap_or_default();
    cuts.extend(overlays.iter().flat_map(|o| [o.start, o.start + o.frames]));
    cuts.sort_unstable();
    cuts.dedup();
    if !cuts.is_empty() {
        units = split_at(units, &cuts, rate);
    }
    set_spans(&mut units);
    let grain = timeline_grain(timeline, &profile.name);

    let mut segments = Vec::new();
    let mut at = 0;
    for (name, frames, unit) in units {
        #[allow(clippy::cast_precision_loss)]
        let text_key = text.map(|t| {
            let (s, e) = (at as f64 / rate, (at + frames) as f64 / rate);
            (t.in_range(s, e).next().is_some()).then(|| t.fingerprint(s, e).to_string())
        });
        let layers: Vec<SegLayer> = overlays
            .iter()
            .filter_map(|o| {
                let from = o.start.max(at);
                let to = (o.start + o.frames).min(at + frames);
                (to > from).then(|| SegLayer {
                    clip: Arc::clone(o),
                    from: from - at,
                    to: to - at,
                    clip_at: from - o.start,
                })
            })
            .collect();
        let mut recipe = json!({
            "rev": RENDER_REVISION,
            "ffcine": env!("CARGO_PKG_VERSION"),
            "rate": rate,
            "profile": profile.recipe(),
            "frames": frames,
            "unit": unit.recipe(),
            "text": text_key.flatten(),
        });
        // Only when set, so timelines without them keep their keys.
        if !layers.is_empty() {
            recipe["layers"] = layers.iter().map(|l| l.recipe(rate)).collect();
        }
        if let Some(g) = &grain {
            recipe["grain"] = Value::Object(g.clone());
        }
        segments.push(Segment {
            key: Key::of(&recipe),
            name,
            start: at,
            frames,
            unit,
            layers,
        });
        at += frames;
    }

    let frames: i64 = segments.iter().map(|s| s.frames).sum();
    #[allow(clippy::cast_precision_loss)]
    let audio = p.audio(timeline, frames as f64 / rate)?;
    Ok(Plan {
        rate,
        profile: profile.clone(),
        segments,
        audio,
        grain,
        warnings: p.warnings,
    })
}

/// A source's camera profile, probed ([`ffcine_core::camera::Profile::detect_file`]).
pub type ProbeProfile<'a> = &'a dyn Fn(&Path) -> ffcine_core::Result<ffcine_core::camera::Profile>;

/// A video's probed average frame rate and frame count.
pub type ProbeRate<'a> = &'a dyn Fn(&Path) -> ffcine_core::Result<(f64, u64)>;

/// What a comp's lowering reads of its media: probed sizes (`dims`), camera profiles for
/// `camera: auto` (`profile`; `None`: every source is display-referred), frame rates for time
/// remaps (`rate`; `None`: a remap is an error) and fingerprints.
pub struct CompMedia<'a> {
    pub fp: &'a dyn Fingerprinter,
    pub dims: &'a dyn Fn(&Path) -> ffcine_core::Result<(u32, u32)>,
    pub profile: Option<ProbeProfile<'a>>,
    pub rate: Option<ProbeRate<'a>>,
}

impl ffcine_comp::Media for CompMedia<'_> {
    fn dims(&self, path: &Path) -> ffcine_core::Result<(u32, u32)> {
        (self.dims)(path)
    }
    fn identity(&self, path: &Path) -> Value {
        serde_json::to_value(self.fp.fingerprint(path)).unwrap_or(Value::Null)
    }
    fn ffmpeg_build(&self) -> String {
        self.fp.ffmpeg_build()
    }
    fn content_hash(&self, path: &Path) -> Value {
        self.fp
            .content_hash(path)
            .map_or_else(|| self.identity(path), Value::String)
    }
    fn profile(&self, path: &Path) -> ffcine_core::Result<ffcine_core::camera::Profile> {
        self.profile
            .map_or(Ok(ffcine_core::camera::Profile::Rec709), |f| f(path))
    }
    fn video_rate(&self, path: &Path) -> ffcine_core::Result<(f64, u64)> {
        match self.rate {
            Some(f) => f(path),
            None => Err(ffcine_core::Error::Invalid {
                what: "time remap (no frame rate probe)",
                value: path.display().to_string(),
            }),
        }
    }
}

/// Plans a comp (`docs/compositing.md`) for `profile` (see [`Profile::for_canvas`]): one
/// segment per piece ([`ffcine_comp::Comp::pieces`]), keyed by its lowered, sampled values, so
/// the segment cache, `diff` and the manifest work as for timelines. No audio.
pub fn plan_comp(
    comp: &ffcine_comp::Comp,
    profile: &Profile,
    media: &dyn ffcine_comp::Media,
) -> Result<Plan, PlanError> {
    let rate = comp.canvas.fps;
    let mut segments = Vec::new();
    for r in comp.pieces() {
        let piece = comp
            .lower(r.clone(), profile.size, media)
            .map_err(PlanError::Comp)?;
        let unit = Unit::Comp(Arc::new(piece));
        let recipe = json!({
            "rev": RENDER_REVISION,
            "ffcine": env!("CARGO_PKG_VERSION"),
            "rate": rate,
            "profile": profile.recipe(),
            "frames": r.end - r.start,
            "unit": unit.recipe(),
        });
        #[allow(clippy::cast_precision_loss)]
        segments.push(Segment {
            key: Key::of(&recipe),
            name: format!(
                "comp {:.2}-{:.2} s",
                r.start as f64 / rate,
                r.end as f64 / rate
            ),
            start: r.start,
            frames: r.end - r.start,
            unit,
            layers: Vec::new(),
        });
    }
    Ok(Plan {
        rate,
        profile: profile.clone(),
        segments,
        audio: None,
        grain: None,
        warnings: Vec::new(),
    })
}

/// `ffcine.fx.grain/1` on the timeline's top-level stack, `profiles.<name>` folded in.
fn timeline_grain(timeline: &Timeline, profile: &str) -> Option<Map<String, Value>> {
    let g = timeline.tracks.effects.iter().find_map(|e| match e {
        Effect::Effect(g) if g.ffcine_schema() == Some(("ffcine.fx.grain", 1)) => {
            g.ffcine().cloned()
        }
        _ => None,
    })?;
    grain_for(g, profile)
}

/// A `ffcine.fx.grain/1`'s parameters for render profile `profile` (`profiles.<name>` folded
/// in, `schema` dropped); `None` if its strength is 0.
pub fn grain_for(mut g: Map<String, Value>, profile: &str) -> Option<Map<String, Value>> {
    let over = g.remove("profiles").and_then(|p| p.get(profile).cloned());
    if let Some(Value::Object(o)) = over {
        g.extend(o);
    }
    g.remove("schema");
    g.get("strength")
        .and_then(Value::as_f64)
        .is_none_or(|s| s > 0.0)
        .then_some(g)
}

#[allow(clippy::large_enum_variant)] // a few per timeline
enum Elem {
    Shot(Shot),
    Gap(i64),
    Transition {
        name: String,
        before: i64,
        after: i64,
        kind: TransitionKind,
    },
    Split(Split),
    /// An `ffcine.comp` clip: its lowered pieces, `frames` in all.
    Comp {
        name: String,
        pieces: Vec<(String, i64, Unit)>,
        frames: i64,
    },
}

struct Planner<'a> {
    project: &'a Project,
    base_dir: &'a Path,
    rate: f64,
    fp: &'a dyn Fingerprinter,
    profile: &'a Profile,
    /// For lowering `ffcine.comp` clips.
    media: Option<&'a dyn ffcine_comp::Media>,
    /// Clips seen so far ([`Shot::clip_index`]).
    clips: usize,
    warnings: Vec<String>,
}

/// Sets every stabilized piece's [`Shot::span`] to the union of its clip's pieces and the clip
/// itself.
fn set_spans(units: &mut [(String, i64, Unit)]) {
    fn each(units: &mut [(String, i64, Unit)], f: &mut dyn FnMut(&mut Shot)) {
        for (_, _, u) in units.iter_mut() {
            match u {
                Unit::Shot(s) => f(s),
                Unit::Transition { a, b, .. } => {
                    f(a);
                    f(b);
                }
                Unit::SplitDissolve { shot, .. } => f(shot),
                Unit::Gap | Unit::Split(_) | Unit::Comp(_) => {}
            }
        }
    }
    let mut spans: std::collections::HashMap<usize, (i64, i64)> = std::collections::HashMap::new();
    each(units, &mut |s| {
        let e = spans.entry(s.clip_index).or_insert((0, s.clip_frames));
        *e = (e.0.min(s.clip_at), e.1.max(s.clip_at + s.frames));
    });
    each(units, &mut |s| {
        if let Some(&span) = spans.get(&s.clip_index) {
            s.span = span;
        }
    });
}

#[allow(clippy::cast_possible_truncation)]
fn frames_at(t: &ffcine_timeline::RationalTime, rate: f64) -> i64 {
    t.rescaled_to(rate).value.round() as i64
}

fn item_frames(item: &Item, rate: f64) -> i64 {
    match item {
        Item::Clip(c) => c
            .source_range
            .as_ref()
            .map_or(0, |r| frames_at(&r.duration, rate)),
        Item::Gap(g) => g
            .source_range
            .as_ref()
            .map_or(0, |r| frames_at(&r.duration, rate)),
        Item::Track(t) => t.children.iter().map(|i| item_frames(i, rate)).sum(),
        Item::Stack(s) => s.source_range.as_ref().map_or_else(
            || {
                s.children
                    .iter()
                    .map(|i| item_frames(i, rate))
                    .max()
                    .unwrap_or(0)
            },
            |r| frames_at(&r.duration, rate),
        ),
        Item::Transition(_) | Item::Unknown(_) => 0,
    }
}

impl Planner<'_> {
    fn warn(&mut self, w: String) {
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }

    fn media_path(&self, clip: &Clip) -> Option<PathBuf> {
        match clip.media_reference()? {
            MediaReference::External(r) => r.path().map(|p| self.base_dir.join(p)),
            _ => None,
        }
    }

    fn flatten(&mut self, items: &[Item]) -> Result<Vec<Elem>, PlanError> {
        let mut out = Vec::new();
        for item in items {
            match item {
                Item::Clip(c) if !c.enabled => {
                    out.push(Elem::Gap(item_frames(item, self.rate)));
                }
                Item::Clip(c) if comp_clip(c).is_some() => out.push(self.comp(c)?),
                Item::Clip(c) => match self.shot(c)? {
                    Some(s) => out.push(Elem::Shot(s)),
                    None => out.push(Elem::Gap(item_frames(item, self.rate))),
                },
                Item::Gap(_) => out.push(Elem::Gap(item_frames(item, self.rate))),
                Item::Transition(t) => out.push(Elem::Transition {
                    name: t.name.clone(),
                    before: frames_at(&t.in_offset, self.rate),
                    after: frames_at(&t.out_offset, self.rate),
                    kind: TransitionKind::from_meta(t.ffcine()),
                }),
                Item::Stack(s)
                    if s.children
                        .iter()
                        .filter(|c| !matches!(c, Item::Unknown(_)))
                        .count()
                        > 1 =>
                {
                    out.push(Elem::Split(self.split(s, item_frames(item, self.rate))?));
                }
                Item::Stack(s) => {
                    let total = item_frames(item, self.rate);
                    let first = s.children.iter().find(|c| !matches!(c, Item::Unknown(_)));
                    let mut inner = match first {
                        Some(Item::Track(t)) => self.flatten(&t.children)?,
                        Some(other) => self.flatten(std::slice::from_ref(other))?,
                        None => Vec::new(),
                    };
                    fit(&mut inner, total);
                    out.extend(inner);
                }
                Item::Track(t) => out.extend(self.flatten(&t.children)?),
                Item::Unknown(_) => {}
            }
        }
        Ok(out)
    }

    /// A split-screen stack: each child is a panel, flattened to its shots over `total` frames.
    fn split(&mut self, s: &ffcine_timeline::Stack, total: i64) -> Result<Split, PlanError> {
        let mut layout = Map::new();
        let mut finish = Finish::default();
        for e in &s.effects {
            if let Effect::Effect(g) = e
                && let (Some(schema), Some(m)) = (g.ffcine_schema(), g.ffcine())
            {
                if schema == ("ffcine.comp.layout", 1) {
                    layout.clone_from(m);
                    layout.remove("schema");
                } else {
                    finish.add(schema, m);
                }
            }
        }
        let mut panels = Vec::new();
        for child in s.children.iter().filter(|c| !matches!(c, Item::Unknown(_))) {
            let mut elems = match child {
                Item::Track(t) => self.flatten(&t.children)?,
                other => self.flatten(std::slice::from_ref(other))?,
            };
            fit(&mut elems, total);
            let mut parts = Vec::new();
            let mut at = 0;
            for e in elems {
                let (frames, shot) = match e {
                    Elem::Shot(sh) => (sh.frames, Some(sh)),
                    Elem::Gap(n) => (n, None),
                    Elem::Transition { name, .. } => {
                        self.warn(format!(
                            "split-screen {:?}: transition {name:?} inside a panel rendered as a cut",
                            s.name
                        ));
                        continue;
                    }
                    Elem::Split(_) => {
                        self.warn(format!(
                            "split-screen {:?}: nested split-screen left out",
                            s.name
                        ));
                        continue;
                    }
                    Elem::Comp { name, frames, .. } => {
                        self.warn(format!(
                            "split-screen {:?}: comp clip {name:?} in a panel rendered black",
                            s.name
                        ));
                        (frames, None)
                    }
                };
                if frames > 0 {
                    parts.push(PanelPart {
                        from: at,
                        frames,
                        shot,
                    });
                }
                at += frames;
            }
            panels.push(parts);
        }
        Ok(Split {
            layout,
            panels,
            finish,
            at: 0,
            total,
        })
    }

    /// An `ffcine.comp` clip lowered over its source range (comp time; default the whole comp).
    fn comp(&mut self, c: &Clip) -> Result<Elem, PlanError> {
        let bad = |msg: String| PlanError::CompClip {
            clip: c.name.clone(),
            msg,
        };
        let params = comp_clip(c).cloned().unwrap_or_default();
        let schema = params.get("schema").and_then(Value::as_str);
        if schema != Some("ffcine.comp/1") {
            return Err(bad(format!(
                "parameters need \"schema\": \"ffcine.comp/1\" (got {schema:?})"
            )));
        }
        let comp = match params.get("src") {
            Some(Value::String(src)) => {
                if let Some(k) = params
                    .keys()
                    .find(|k| !matches!(k.as_str(), "schema" | "src"))
                {
                    return Err(bad(format!(
                        "`{k}` beside `src`: give `src` or an inline comp, not both"
                    )));
                }
                ffcine_comp::Comp::from_file(&self.base_dir.join(src))
            }
            Some(other) => return Err(bad(format!("`src`: expected a path, got {other}"))),
            None => ffcine_comp::Comp::from_value(&Value::Object(params), self.base_dir),
        }
        .map_err(|e| bad(e.to_string()))?;
        if (comp.canvas.fps - self.rate).abs() > 1e-6 * self.rate {
            return Err(bad(format!(
                "canvas fps {} differs from the timeline's {}",
                comp.canvas.fps, self.rate
            )));
        }
        let [cw, ch] = comp.canvas.size;
        let out = self.profile.size;
        let aspect = |w: u32, h: u32| f64::from(w) / f64::from(h);
        if (aspect(cw, ch) / aspect(out.width, out.height) - 1.0).abs() > 0.01 {
            return Err(bad(format!(
                "canvas {cw}x{ch} isn't the output's aspect ({}x{}); it would stretch",
                out.width, out.height
            )));
        }
        let Some(media) = self.media else {
            return Err(bad("comp clips need probed media".into()));
        };
        let (from, frames) = match &c.source_range {
            Some(r) => (
                frames_at(&r.start_time, self.rate),
                frames_at(&r.duration, self.rate),
            ),
            None => (0, comp.frames()),
        };
        if from < 0 {
            return Err(bad(format!(
                "source_range starts at comp frame {from}; it can't be before the comp's start"
            )));
        }
        for e in &c.effects {
            let schema = match e {
                Effect::Effect(g) => g.ffcine_schema().map_or_else(
                    || g.effect_name.clone(),
                    |(name, major)| format!("{name}/{major}"),
                ),
                other => other.effect_name().unwrap_or("unnamed effect").to_owned(),
            };
            self.warn(format!(
                "comp clip {:?}: comp clip effects are not applied yet: {schema}",
                c.name
            ));
        }
        let mut pieces = Vec::new();
        for r in comp.pieces() {
            let (a, b) = (r.start.max(from), r.end.min(from + frames));
            if b > a {
                let piece = comp
                    .lower(a..b, self.profile.size, media)
                    .map_err(|e| bad(e.to_string()))?;
                pieces.push((c.name.clone(), b - a, Unit::Comp(Arc::new(piece))));
            }
        }
        let got: i64 = pieces.iter().map(|p| p.1).sum();
        if got < frames {
            // Past the comp's end: black, like a gap.
            pieces.push((c.name.clone(), frames - got, Unit::Gap));
        }
        Ok(Elem::Comp {
            name: c.name.clone(),
            pieces,
            frames,
        })
    }

    /// The non-text clips of an overlay video track.
    fn overlays(&mut self, t: &Track) -> Result<Vec<OverlayClip>, PlanError> {
        let mut out = Vec::new();
        let mut at = 0;
        for item in &t.children {
            let n = item_frames(item, self.rate);
            if let Item::Clip(c) = item
                && c.enabled
                && comp_clip(c).is_some()
            {
                self.warn(format!(
                    "track {:?}: comp clip {:?} composited on the picture track only, left out",
                    t.name, c.name
                ));
                at += n;
                continue;
            }
            if let Item::Clip(c) = item
                && c.enabled
                && !matches!(c.media_reference(),
                    Some(MediaReference::Generator(g)) if g.generator_kind == "ffcine.text")
            {
                let mut transform = Map::new();
                let mut blend = Map::new();
                for e in &c.effects {
                    if let Effect::Effect(g) = e
                        && let (Some(schema), Some(m)) = (g.ffcine_schema(), g.ffcine())
                    {
                        match schema {
                            ("ffcine.geo.transform", 1) => transform.clone_from(m),
                            ("ffcine.comp.blend", 1) => blend.clone_from(m),
                            _ => {}
                        }
                    }
                }
                transform.remove("schema");
                blend.remove("schema");
                if blend
                    .get("mode")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m != "normal")
                {
                    self.warn(format!(
                        "overlay {:?}: blend mode {} rendered as normal",
                        c.name, blend["mode"]
                    ));
                }
                let kind = c
                    .ffcine_value("source")
                    .and_then(|s| s.get("kind"))
                    .and_then(Value::as_str);
                let media = self.media_path(c);
                let image = kind == Some("image")
                    || media.as_ref().is_some_and(|p| {
                        p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                            matches!(
                                e.to_ascii_lowercase().as_str(),
                                "png" | "jpg" | "jpeg" | "webp"
                            )
                        })
                    });
                let source = match media {
                    Some(path) if image => Some(OverlaySource::Image {
                        fingerprint: self.fp.fingerprint(&path),
                        path,
                    }),
                    _ => self.shot(c)?.map(OverlaySource::Video),
                };
                if let Some(source) = source {
                    out.push(OverlayClip {
                        name: c.name.clone(),
                        source,
                        start: at,
                        frames: n,
                        source_start: c
                            .source_range
                            .as_ref()
                            .map_or(0.0, |r| r.start_time.to_seconds()),
                        transform,
                        blend,
                    });
                }
            }
            at += n;
        }
        Ok(out)
    }

    /// A clip's picture, or `None` (rendered black) if its media isn't a file.
    #[allow(clippy::too_many_lines)] // one arm per effect
    fn shot(&mut self, c: &Clip) -> Result<Option<Shot>, PlanError> {
        let Some(range) = c.source_range.as_ref().or_else(|| {
            c.media_reference()
                .and_then(MediaReference::available_range)
        }) else {
            self.warn(format!("clip {:?}: no source range, skipped", c.name));
            return Ok(None);
        };
        let Some(media) = self.media_path(c) else {
            let generated = matches!(c.media_reference(), Some(MediaReference::Generator(_)));
            if !generated {
                self.warn(format!("clip {:?}: no media file, rendered black", c.name));
            }
            return Ok(None);
        };
        let media = if self.profile.proxies {
            crate::proxy::find(&media, self.profile.proxy_dir.as_deref(), self.fp).unwrap_or_else(
                || {
                    self.warn(format!(
                        "clip {:?}: no proxy, rendered from the source",
                        c.name
                    ));
                    media
                },
            )
        } else {
            media
        };
        let mut speed = 1.0;
        let mut clip_grade = None;
        let mut fx = Vec::new();
        let mut finish = Finish::default();
        let mut grain = None;
        let mut filters = Filters::default();
        for e in &c.effects {
            match e {
                Effect::LinearTimeWarp(w) => speed *= w.time_scalar,
                Effect::FreezeFrame(_) => speed = 0.0,
                Effect::Effect(g) if Self::filter(&c.name, g, &mut filters)? => {}
                Effect::Effect(g) => {
                    if let Some(r) = g.ffcine_params::<ClipGrade>(
                        GRADE_SCHEMA.split('/').next().unwrap_or_default(),
                        1,
                    ) {
                        clip_grade = Some(r?);
                    } else if g.effect_name.starts_with("ffcine.audio.") {
                        // The mix's, keyed there.
                    } else {
                        if let (Some(schema), Some(m)) = (g.ffcine_schema(), g.ffcine()) {
                            if schema == ("ffcine.fx.grain", 1) {
                                grain = Some(m.clone());
                            } else {
                                finish.add(schema, m);
                            }
                        }
                        let mut params = g.ffcine().cloned();
                        if g.ffcine_schema() == Some(("ffcine.geo.reframe", 1)) {
                            let m = params.clone().unwrap_or_default();
                            filters.reframe =
                                Some(Reframe::parse(&m, Some(&self.profile.aspect.name)).map_err(
                                    |e| PlanError::Effect {
                                        clip: c.name.clone(),
                                        effect: "reframe",
                                        msg: e.to_string(),
                                    },
                                )?);
                            // Only this aspect's override is keyed (in `filters` too).
                            if let Some(Value::Object(p)) =
                                params.as_mut().and_then(|m| m.get_mut("profiles"))
                            {
                                p.retain(|k, _| *k == self.profile.aspect.name);
                            }
                        }
                        fx.push(json!({"effect": g.effect_name, "ffcine": params}));
                    }
                }
                Effect::Unknown(m) => fx.push(Value::Object(m.clone())),
            }
        }
        filters.geo = Geo::from_effects(&c.effects).map_err(|e| PlanError::Effect {
            clip: c.name.clone(),
            effect: "geo",
            msg: e.to_string(),
        })?;
        let kind = match c
            .ffcine_value("source")
            .and_then(|s| s.get("kind"))
            .and_then(Value::as_str)
        {
            Some("iphone" | "ai" | "still" | "display") => SourceKind::Display,
            _ => SourceKind::Log,
        };
        let grade = self
            .project
            .resolve_grade(clip_grade.as_ref(), kind)
            .map_err(|source| PlanError::Grade {
                clip: c.name.clone(),
                source,
            })?;
        let lut = self
            .project
            .lut_path(grade.lut.as_deref())
            .map(|l| self.base_dir.join(l));
        if filters.denoise.is_none() {
            filters.denoise = grade.denoise.and_then(grade_denoise);
        }
        let mut grade_recipe = serde_json::to_value(&grade).unwrap_or_default();
        if let Value::Object(m) = &mut grade_recipe {
            let lut_id = lut.as_ref().map(|p| {
                self.fp
                    .content_hash(p)
                    .map_or_else(|| json!({"missing": p}), |h| json!({"sha": h}))
            });
            m.insert("lut".into(), lut_id.unwrap_or(Value::Null));
        }
        let frames = frames_at(&range.duration, self.rate);
        let start = range.start_time.to_seconds();
        self.clips += 1;
        Ok(Some(Shot {
            name: c.name.clone(),
            fingerprint: self.fp.fingerprint(&media),
            media,
            start,
            speed,
            frames,
            grade,
            lut,
            grade_recipe,
            fx,
            finish,
            grain,
            clip_at: 0,
            clip_frames: frames,
            filters,
            clip_start: start,
            span: (0, frames),
            clip_index: self.clips,
        }))
    }

    /// Reads a stabilize, denoise, ramp or blend effect of clip `clip` into `filters`; `false` if `g`
    /// isn't one (they're keyed through `filters`, not `fx`).
    fn filter(
        clip: &str,
        g: &ffcine_timeline::GenericEffect,
        filters: &mut Filters,
    ) -> Result<bool, PlanError> {
        let bad = |effect: &'static str, msg: String| PlanError::Effect {
            clip: clip.to_owned(),
            effect,
            msg,
        };
        let params = || {
            let mut m = g.ffcine().cloned().unwrap_or_default();
            m.remove("schema");
            m
        };
        match g.ffcine_schema() {
            Some(("ffcine.fx.stabilize", 1)) => {
                filters.stabilize = Some(
                    ffcine_stabilize::fx::params(&params())
                        .map_err(|e| bad("stabilize", e.to_string()))?,
                );
            }
            Some(("ffcine.fx.denoise", 1)) => {
                let d =
                    ffcine_filters::effect::denoise(&params()).map_err(|e| bad("denoise", e))?;
                filters.denoise = (!d.is_off()).then_some(d);
            }
            Some(("ffcine.time.ramp", 1)) => {
                let m = params();
                let points: Vec<(f64, f64)> = m
                    .get("points")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| {
                                let p = p.as_array()?;
                                Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                match m.get("source_duration").and_then(Value::as_f64) {
                    Some(d) if d > 0.0 && points.len() >= 2 && points.iter().all(|p| p.1 > 0.0) => {
                        filters.ramp = Some(Ramp {
                            points,
                            source_duration: d,
                        });
                    }
                    _ => {
                        return Err(bad(
                            "ramp",
                            "needs points [[u, speed > 0]] and source_duration".into(),
                        ));
                    }
                }
            }
            Some(("ffcine.time.blend", 1)) => {
                let n = params().get("frames").and_then(Value::as_f64);
                match n {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    Some(n) if (1.0..=64.0).contains(&n.round()) => {
                        let n = n.round() as u32;
                        filters.blend = (n > 1).then_some(n);
                    }
                    _ => return Err(bad("blend", "needs frames 1..64".into())),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Turns transitions into dissolve units: the outgoing shot loses `before` frames, the
    /// incoming one `after`, and the overlap (`before + after`) is its own unit.
    #[allow(clippy::too_many_lines)]
    fn resolve_transitions(&mut self, elems: Vec<Elem>) -> Vec<(String, i64, Unit)> {
        let mut out: Vec<(String, i64, Unit)> = Vec::new();
        let mut pending: Option<(String, i64, i64, TransitionKind)> = None;
        for e in elems {
            match e {
                Elem::Transition {
                    name,
                    before,
                    after,
                    kind,
                } => pending = Some((name, before, after, kind)),
                Elem::Comp { name, pieces, .. } => {
                    if let Some((t, ..)) = pending.take() {
                        self.warn(format!(
                            "transition {t:?} into comp clip {name:?} rendered as a cut"
                        ));
                    }
                    out.extend(pieces);
                }
                Elem::Split(sp) => {
                    let label = format!("split {}", sp.panels.len());
                    let mut sp = sp;
                    let mut frames = sp.total;
                    if let Some((name, before, after, kind)) = pending.take() {
                        let n = before + after;
                        match out.last_mut() {
                            Some((_, a_frames, Unit::Shot(a)))
                                if n > 0 && a.frames > before && sp.total > after =>
                            {
                                if kind != TransitionKind::Dissolve {
                                    self.warn(format!(
                                        "transition {name:?} into a split-screen rendered as a dissolve"
                                    ));
                                }
                                let shot = a.shifted(a.frames - before, self.rate).with_frames(n);
                                a.frames -= before;
                                *a_frames = a.frames;
                                let into = Split {
                                    at: -before,
                                    ..sp.clone()
                                };
                                out.push((
                                    format!("{} x {label}", shot.name),
                                    n,
                                    Unit::SplitDissolve {
                                        split: into,
                                        shot,
                                        split_out: false,
                                    },
                                ));
                                sp.at = after;
                                frames -= after;
                            }
                            _ => self.warn(format!(
                                "transition {name:?} needs a shot on one side, rendered as a cut"
                            )),
                        }
                    }
                    out.push((label, frames, Unit::Split(sp)));
                }
                Elem::Gap(n) if n > 0 => {
                    if let Some((name, ..)) = pending.take() {
                        self.warn(format!(
                            "transition {name:?} next to a gap, rendered as a cut"
                        ));
                    }
                    match out.last_mut() {
                        Some((_, frames, Unit::Gap)) => *frames += n,
                        _ => out.push(("gap".into(), n, Unit::Gap)),
                    }
                }
                Elem::Gap(_) => {}
                Elem::Shot(b) => {
                    let mut b = b;
                    if let Some((name, before, after, kind)) = pending.take() {
                        let n = before + after;
                        match out.last_mut() {
                            Some((_, a_frames, Unit::Shot(a)))
                                if n > 0 && a.frames > before && b.frames > after =>
                            {
                                let a_part = a.shifted(a.frames - before, self.rate).with_frames(n);
                                let b_part = b.shifted(-before, self.rate).with_frames(n);
                                a.frames -= before;
                                *a_frames = a.frames;
                                let label = format!("{} x {}", a.name, b.name);
                                out.push((
                                    label,
                                    n,
                                    Unit::Transition {
                                        a: a_part,
                                        b: b_part,
                                        kind,
                                    },
                                ));
                                b = b.shifted(after, self.rate);
                            }
                            Some((a_label, a_frames, Unit::Split(sp)))
                                if n > 0 && *a_frames > before && b.frames > after =>
                            {
                                if kind != TransitionKind::Dissolve {
                                    self.warn(format!(
                                        "transition {name:?} out of a split-screen rendered as a dissolve"
                                    ));
                                }
                                let out_of = Split {
                                    at: sp.at + *a_frames - before,
                                    ..sp.clone()
                                };
                                *a_frames -= before;
                                let label = format!("{a_label} x {}", b.name);
                                out.push((
                                    label,
                                    n,
                                    Unit::SplitDissolve {
                                        split: out_of,
                                        shot: b.shifted(-before, self.rate).with_frames(n),
                                        split_out: true,
                                    },
                                ));
                                b = b.shifted(after, self.rate);
                            }
                            _ => self.warn(format!(
                                "transition {name:?} needs a shot on one side, rendered as a cut"
                            )),
                        }
                    }
                    if b.frames > 0 {
                        out.push((b.name.clone(), b.frames, Unit::Shot(b)));
                    }
                }
            }
        }
        out.retain(|(_, n, _)| *n > 0);
        out
    }

    /// The mix of `timeline`, `length` seconds long (the picture's).
    fn audio(&mut self, timeline: &Timeline, length: f64) -> Result<Option<AudioPlan>, PlanError> {
        for t in timeline
            .track_list()
            .filter(|t| t.kind == track_kind::AUDIO && t.enabled)
        {
            if t.children.iter().any(|i| matches!(i, Item::Stack(_))) {
                self.warn(format!(
                    "audio track {:?}: nested stacks not mixed yet",
                    t.name
                ));
            }
        }
        let mut options =
            MixOptions::for_timeline(timeline, Some(self.base_dir)).map_err(PlanError::Audio)?;
        options.length = Some(length);
        let (clips, _) = sources(timeline, options.video_audio);
        if clips.is_empty() {
            return Ok(None);
        }
        let mut inputs = Vec::new();
        for (bus, p) in &clips {
            let c = p.clip;
            let media = p.path(Some(self.base_dir)).map(|m| self.fp.fingerprint(&m));
            // Only what the mix reads: a picture effect (grade, reframe) on a camera clip whose
            // audio is the dialogue changes no audio.
            let effects: Vec<Value> = c
                .effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Effect(g) if g.effect_name.starts_with("ffcine.audio.") => {
                        Some(json!({"effect": g.effect_name, "ffcine": g.ffcine()}))
                    }
                    _ => None,
                })
                .collect();
            let model = clip_effect::<Clean>(c, CLEAN)
                .ok()
                .flatten()
                .and_then(|cl| cl.model)
                .map(|m| self.fp.content_hash(&self.base_dir.join(m)));
            inputs.push(json!({
                "bus": bus.name(),
                "media": media,
                "start": p.start,
                "duration": p.duration,
                "source_start": p.source_start,
                "xfade": [p.xfade_in, p.xfade_out],
                "speed": p.speed,
                "effects": effects,
                "sync": c.ffcine_value("sync"),
                "model": model,
            }));
        }
        let inputs = Value::Array(inputs);
        Ok(Some(AudioPlan {
            key: AudioPlan::key_of(&inputs, &options),
            clips: clips.len(),
            timeline: Arc::new(timeline.clone()),
            options,
            inputs,
        }))
    }
}

/// Pads with black or cuts `elems` to `total` frames.
fn fit(elems: &mut Vec<Elem>, total: i64) {
    let len = |e: &Elem| match e {
        Elem::Shot(s) => s.frames,
        Elem::Gap(n) => *n,
        Elem::Transition { .. } => 0,
        Elem::Split(s) => s.total,
        Elem::Comp { frames, .. } => *frames,
    };
    let mut sum: i64 = elems.iter().map(len).sum();
    while sum > total {
        let Some(last) = elems.last_mut() else { break };
        let over = sum - total;
        match last {
            Elem::Shot(s) if s.frames > over => {
                s.frames -= over;
                sum = total;
            }
            Elem::Gap(n) if *n > over => {
                *n -= over;
                sum = total;
            }
            _ => {
                sum -= len(last);
                elems.pop();
            }
        }
    }
    if sum < total {
        elems.push(Elem::Gap(total - sum));
    }
}

/// Splits shot, split-screen and gap units at timeline frames `cuts` (transitions stay whole), skipping cuts
/// that would leave a piece shorter than a second.
fn split_at(units: Vec<(String, i64, Unit)>, cuts: &[i64], rate: f64) -> Vec<(String, i64, Unit)> {
    let mut out = Vec::new();
    let mut at = 0;
    for (name, frames, unit) in units {
        let end = at + frames;
        // Pieces under a second cost more in per-segment setup than they save.
        #[allow(clippy::cast_possible_truncation)]
        let min = rate.round() as i64;
        let mut inner: Vec<i64> = Vec::new();
        for &c in cuts {
            if c - inner.last().copied().unwrap_or(at) >= min && end - c >= min {
                inner.push(c);
            }
        }
        // Comp pieces are cut where their values change already, and a part of one would need
        // lowering again.
        if inner.is_empty()
            || matches!(
                unit,
                Unit::Transition { .. } | Unit::SplitDissolve { .. } | Unit::Comp(_)
            )
        {
            out.push((name, frames, unit));
        } else {
            let mut from = at;
            for to in inner.into_iter().chain([end]) {
                let n = to - from;
                let piece = match &unit {
                    Unit::Shot(s) => Unit::Shot(s.shifted(from - at, rate).with_frames(n)),
                    Unit::Split(s) => Unit::Split(Split {
                        at: s.at + from - at,
                        ..s.clone()
                    }),
                    other => other.clone(),
                };
                out.push((name.clone(), n, piece));
                from = to;
            }
        }
        at = end;
    }
    out
}

/// Segment keys of `plan` as a set.
pub fn key_set(plan: &Plan) -> HashSet<&Key> {
    plan.segments.iter().map(|s| &s.key).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Diff;
    use ffcine_timeline::{
        ExternalReference, Gap, GenericEffect, LinearTimeWarp, RationalTime, TimeRange, Transition,
    };
    use serde_json::Map;

    const RATE: f64 = 24000.0 / 1001.0;

    /// Fingerprints from the path alone.
    struct Fake;
    impl Fingerprinter for Fake {
        fn fingerprint(&self, path: &Path) -> Fingerprint {
            Fingerprint {
                path: path.to_owned(),
                size: Some(path.as_os_str().len() as u64),
                mtime_ns: Some(1),
            }
        }
        fn content_hash(&self, path: &Path) -> Option<String> {
            Some(format!("hash-of-{}", path.display()))
        }
    }

    #[test]
    fn comp_pieces_are_segments_keyed_by_their_values() {
        let comp = |fade: &str| {
            ffcine_comp::Comp::from_value(
                &json!({
                    "schema": "ffcine.comp/1",
                    "canvas": { "size": [1080, 1920], "fps": 30, "duration": 4.0 },
                    "layers": [
                        { "source": { "video": "a.mov" }, "out": 2.0 },
                        { "source": { "solid": "white" }, "in": 2.0, "fade": fade },
                    ],
                }),
                Path::new("/media"),
            )
            .expect("comp")
        };
        let dims = |_: &Path| Ok((1920, 1080));
        let media = CompMedia {
            fp: &Fake,
            dims: &dims,
            profile: None,
            rate: None,
        };
        let profile = Profile::for_canvas("master", 1080, 1920).expect("profile");
        assert_eq!(
            (profile.size.width, profile.size.height),
            (2160, 3840),
            "the canvas aspect at the master's short side"
        );
        let a = plan_comp(&comp("1"), &profile, &media).expect("plan");
        let b = plan_comp(&comp("clip(4 - t, 0, 1)"), &profile, &media).expect("plan");
        assert_eq!(a.segments.len(), 2);
        assert_eq!(a.frames(), 120);
        assert!(
            matches!(&a.segments[0].unit, Unit::Comp(p) if matches!(p.base, ffcine_comp::Base::Video { .. }))
        );
        assert_eq!(a.segments[0].key, b.segments[0].key);
        assert_ne!(a.segments[1].key, b.segments[1].key);
        // The diff sees it like a timeline edit.
        let d = Diff::new(&a, &b);
        assert!(d.report(&b, false).contains("comp 2.00-4.00 s"));
    }

    /// [`Fake`] with an ffmpeg build string.
    struct Build(&'static str);
    impl Fingerprinter for Build {
        fn fingerprint(&self, path: &Path) -> Fingerprint {
            Fake.fingerprint(path)
        }
        fn content_hash(&self, path: &Path) -> Option<String> {
            Fake.content_hash(path)
        }
        fn ffmpeg_build(&self) -> String {
            self.0.into()
        }
    }

    /// A new ffmpeg (the injected fingerprinter's build string) re-renders the segments with a
    /// filter node, and only those.
    #[test]
    fn ffmpeg_build_keys_filter_nodes_only() {
        let comp = ffcine_comp::Comp::from_value(
            &json!({
                "schema": "ffcine.comp/1",
                "canvas": { "size": [1920, 1080], "fps": 30, "duration": 4.0 },
                "layers": [
                    { "source": { "video": "a.mov" }, "out": 2.0,
                      "filter": { "graph": "hqdn3d" } },
                    { "source": { "video": "b.mov" }, "in": 2.0 },
                ],
            }),
            Path::new("/media"),
        )
        .expect("comp");
        let dims = |_: &Path| Ok((1920, 1080));
        let rate = |_: &Path| Ok((30.0, 300));
        let plan = |fp: &dyn Fingerprinter| {
            let media = CompMedia {
                fp,
                dims: &dims,
                profile: None,
                rate: Some(&rate),
            };
            let profile = Profile::for_canvas("master", 1920, 1080).expect("profile");
            plan_comp(&comp, &profile, &media).expect("plan")
        };
        let (a, b) = (plan(&Build("ffmpeg 8")), plan(&Build("ffmpeg 9")));
        assert_eq!(a.segments.len(), 2);
        assert_ne!(a.segments[0].key, b.segments[0].key);
        assert_eq!(a.segments[1].key, b.segments[1].key);
    }

    fn clip(name: &str, file: &str, start: f64, frames: f64, look: &str) -> Item {
        let range = TimeRange::from_frames(start, start + frames, RATE);
        let media = MediaReference::External(ExternalReference::from_path(
            Path::new(&format!("/media/{file}.mp4")),
            None,
        ));
        let mut c = Clip::new(name, media, Some(range));
        let mut params = Map::new();
        params.insert("look".into(), look.into());
        c.effects
            .push(Effect::Effect(GenericEffect::ffcine(GRADE_SCHEMA, params)));
        Item::Clip(c)
    }

    fn music(file: &str) -> Item {
        let media = MediaReference::External(ExternalReference::from_path(
            Path::new(&format!("/media/{file}.flac")),
            None,
        ));
        Item::Clip(Clip::new(
            file,
            media,
            Some(TimeRange::from_frames(0.0, 400.0, RATE)),
        ))
    }

    /// An `ffcine.comp` generator clip over `from..to` frames of comp time.
    fn comp_clip(name: &str, params: &Value, from: f64, to: f64) -> Item {
        let g = ffcine_timeline::GeneratorReference::new(
            COMP_GENERATOR,
            params.as_object().cloned().unwrap_or_default(),
        );
        Item::Clip(Clip::new(
            name,
            MediaReference::Generator(g),
            Some(TimeRange::from_frames(from, to, RATE)),
        ))
    }

    #[test]
    fn comp_clips_lower_to_comp_segments() {
        let inline = json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [1920, 1080], "fps": "24000/1001", "duration": 3.0 },
            "layers": [
                { "source": { "still": "a.png" } },
                { "source": { "solid": "white" }, "in": 1.0, "fade": 0.5 },
            ],
        });
        let dims = |_: &Path| Ok((1920, 1080));
        let media = CompMedia {
            fp: &Fake,
            dims: &dims,
            profile: None,
            rate: None,
        };
        let with = |t: &Timeline| {
            plan_with(
                t,
                Path::new("/proj"),
                &Profile::preview(),
                &Fake,
                None,
                Some(&media),
            )
        };
        let shot = || clip("a", "A", 0.0, 48.0, "day");
        let t = timeline(
            vec![shot(), comp_clip("c", &inline, 12.0, 60.0), shot()],
            vec![],
        );
        assert!(has_comp_clips(&t));
        let p = with(&t).expect("plan");
        let comps: Vec<&Segment> = p
            .segments
            .iter()
            .filter(|s| matches!(s.unit, Unit::Comp(_)))
            .collect();
        assert_eq!(comps.len(), 2, "split where the white layer comes in");
        assert_eq!(comps.iter().map(|s| s.frames).sum::<i64>(), 48);
        assert_eq!(p.frames(), 144);
        // The shots around it key as they would around a gap.
        let gap = timeline(
            vec![
                shot(),
                Item::Gap(Gap::new(RationalTime::new(48.0, RATE))),
                shot(),
            ],
            vec![],
        );
        assert!(!has_comp_clips(&gap));
        let g = plan_of(&gap);
        assert_eq!(p.segments[0].key, g.segments[0].key);
        assert_eq!(
            p.segments.last().map(|s| &s.key),
            g.segments.last().map(|s| &s.key)
        );

        // Past the comp's end is black; a transition into it is a cut.
        let mut tr = vec![shot()];
        tr.push(Item::Transition(Transition::dissolve(
            RationalTime::new(6.0, RATE),
            RationalTime::new(6.0, RATE),
        )));
        tr.push(comp_clip("c", &inline, 48.0, 96.0));
        let p = with(&timeline(tr, vec![])).expect("plan");
        assert!(matches!(
            p.segments.last().map(|s| &s.unit),
            Some(Unit::Gap)
        ));
        assert!(
            p.warnings.iter().any(|w| w.contains("rendered as a cut")),
            "{:?}",
            p.warnings
        );

        // No media, the wrong schema or rate, or `src` beside an inline comp: errors.
        let err = |params: &Value, m: Option<&dyn ffcine_comp::Media>| {
            let t = timeline(vec![comp_clip("c", params, 0.0, 24.0)], vec![]);
            plan_with(&t, Path::new("/proj"), &Profile::preview(), &Fake, None, m)
                .expect_err("an error")
                .to_string()
        };
        assert!(err(&inline, None).contains("probed media"));
        let mut bad = inline.clone();
        bad["schema"] = json!("ffcine.comp/2");
        assert!(err(&bad, Some(&media)).contains("ffcine.comp/1"));
        let mut bad = inline.clone();
        bad["canvas"]["fps"] = json!(30);
        assert!(err(&bad, Some(&media)).contains("fps"));
        let mut bad = inline.clone();
        bad["canvas"]["size"] = json!([1080, 1920]);
        assert!(err(&bad, Some(&media)).contains("aspect"));
        let both = json!({"schema": "ffcine.comp/1", "src": "a.comp.json", "layers": []});
        assert!(err(&both, Some(&media)).contains("`layers` beside `src`"));
    }

    #[test]
    fn comp_clip_effects_warn_and_a_negative_start_is_an_error() {
        let inline = json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [1920, 1080], "fps": "24000/1001", "duration": 2.0 },
            "layers": [{ "source": { "solid": "white" } }],
        });
        let dims = |_: &Path| Ok((1920, 1080));
        let media = CompMedia {
            fp: &Fake,
            dims: &dims,
            profile: None,
            rate: None,
        };
        let with = |t: &Timeline| {
            plan_with(
                t,
                Path::new("/proj"),
                &Profile::preview(),
                &Fake,
                None,
                Some(&media),
            )
        };
        let early = timeline(vec![comp_clip("c", &inline, -6.0, 24.0)], vec![]);
        let msg = with(&early).expect_err("a negative start").to_string();
        assert!(msg.contains("before the comp's start"), "{msg}");

        // Effects on a comp clip aren't applied yet: one warning per effect.
        let fx = with_fx(
            with_fx(
                comp_clip("c", &inline, 0.0, 24.0),
                effect("ffcine.comp.blend/1", json!({"opacity": 0.5})),
            ),
            effect("ffcine.geo.transform/1", json!({"scale": 0.5})),
        );
        let planned = with(&timeline(vec![fx], vec![])).expect("plan");
        for schema in ["ffcine.comp.blend/1", "ffcine.geo.transform/1"] {
            let w = format!("comp clip \"c\": comp clip effects are not applied yet: {schema}");
            assert!(planned.warnings.contains(&w), "{:?}", planned.warnings);
        }
    }

    fn timeline(video: Vec<Item>, audio: Vec<Item>) -> Timeline {
        let mut t = Timeline::new("t");
        let mut project = Project {
            rate: Some(RATE),
            ..Project::default()
        };
        for (name, warmth) in [("day", 0.02), ("night", -0.03)] {
            project.looks.insert(
                name.into(),
                GradeParams {
                    warmth: Some(warmth),
                    lut: Some("normal".into()),
                    ..GradeParams::default()
                },
            );
        }
        project
            .luts
            .insert("normal".into(), "luts/normal.cube".into());
        t.set_project(&project).unwrap();
        let mut v = Track::new("V1", track_kind::VIDEO);
        v.children = video;
        let mut a = Track::new("Music", track_kind::AUDIO);
        a.children = audio;
        t.tracks.children = vec![Item::Track(v), Item::Track(a)];
        t
    }

    /// `review` decodes a camera proxy beside the clip when there is one, and keys apart from
    /// `preview`; a clip without one renders from its source, warned.
    #[test]
    fn review_renders_from_proxies() {
        let dir = std::env::temp_dir().join(format!("ffcine-plan-proxy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["A.mp4", "A.LRF", "B.mp4"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let file = |name: &str, f: &str| {
            let media = MediaReference::External(ExternalReference::from_path(&dir.join(f), None));
            Item::Clip(Clip::new(
                name,
                media,
                Some(TimeRange::from_frames(0.0, 48.0, RATE)),
            ))
        };
        let t = timeline(vec![file("a", "A.mp4"), file("b", "B.mp4")], vec![]);
        let media = |p: &Plan| -> Vec<PathBuf> {
            p.segments
                .iter()
                .filter_map(|s| match &s.unit {
                    Unit::Shot(s) => Some(s.media.clone()),
                    _ => None,
                })
                .collect()
        };
        let review = plan(&t, &dir, &Profile::review(), &Fake, None).unwrap();
        assert_eq!(media(&review), [dir.join("A.LRF"), dir.join("B.mp4")]);
        assert!(
            review
                .warnings
                .iter()
                .any(|w| w.contains("\"b\": no proxy")),
            "{:?}",
            review.warnings
        );
        let preview = plan(&t, &dir, &Profile::preview(), &Fake, None).unwrap();
        assert_eq!(media(&preview), [dir.join("A.mp4"), dir.join("B.mp4")]);
        assert!(key_set(&review).is_disjoint(&key_set(&preview)));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn plan_of(t: &Timeline) -> Plan {
        plan(t, Path::new("/proj"), &Profile::preview(), &Fake, None).unwrap()
    }

    fn keys(p: &Plan) -> Vec<(String, Key)> {
        p.segments
            .iter()
            .map(|s| (s.name.clone(), s.key.clone()))
            .collect()
    }

    fn shots() -> Vec<Item> {
        vec![
            clip("a", "A", 0.0, 48.0, "day"),
            clip("b", "B", 100.0, 72.0, "night"),
            clip("c", "C", 10.0, 30.0, "day"),
            clip("d", "D", 0.0, 50.0, "night"),
        ]
    }

    /// Regression (a pale clip after an insert): a cache keyed by position gave inserted-after clips other clips'
    /// grades. Inserting or removing a clip must change no other clip's key.
    #[test]
    fn insert_or_remove_changes_no_other_key() {
        let base = plan_of(&timeline(shots(), vec![]));
        let mut inserted = shots();
        inserted.insert(1, clip("new", "N", 5.0, 40.0, "night"));
        inserted.insert(0, clip("new0", "N", 50.0, 20.0, "day"));
        let ins = plan_of(&timeline(inserted, vec![]));
        let ins_keys = keys(&ins);
        for (name, key) in keys(&base) {
            assert!(
                ins_keys.contains(&(name.clone(), key)),
                "{name} changed key on insert"
            );
        }
        let mut removed = shots();
        removed.remove(1);
        let rem = plan_of(&timeline(removed, vec![]));
        let base_keys = keys(&base);
        for k in keys(&rem) {
            assert!(base_keys.contains(&k), "{} changed key on remove", k.0);
        }
        let d = Diff::new(&base, &ins);
        assert_eq!(d.render_count(), 2);
        assert_eq!(d.dropped, 0);
    }

    #[test]
    fn grade_edit_changes_only_that_clip() {
        let base = plan_of(&timeline(shots(), vec![]));
        let mut edited = shots();
        edited[2] = clip("c", "C", 10.0, 30.0, "night");
        let new = plan_of(&timeline(edited, vec![]));
        let changed: Vec<_> = base
            .segments
            .iter()
            .zip(&new.segments)
            .filter(|(a, b)| a.key != b.key)
            .map(|(a, _)| a.name.as_str())
            .collect();
        assert_eq!(changed, ["c"]);
        // Same clip, same content at another position: same key.
        let mut moved = shots();
        moved.swap(0, 3);
        let m = plan_of(&timeline(moved, vec![]));
        assert_eq!(Diff::new(&base, &m).render_count(), 0);
    }

    #[test]
    fn identical_content_shares_a_key_whatever_the_name() {
        let p = plan_of(&timeline(
            vec![
                clip("x", "A", 0.0, 48.0, "day"),
                clip("y", "A", 0.0, 48.0, "day"),
            ],
            vec![],
        ));
        assert_eq!(p.segments[0].key, p.segments[1].key);
    }

    #[test]
    fn music_swap_keeps_picture() {
        let a = plan_of(&timeline(shots(), vec![music("song1")]));
        let b = plan_of(&timeline(shots(), vec![music("song2")]));
        assert_eq!(a.program_key(), b.program_key());
        assert_ne!(a.master_key(), b.master_key());
        let d = Diff::new(&a, &b);
        assert_eq!(d.render_count(), 0);
        assert!(d.audio_changed);
        assert!(d.report(&b, false).contains("audio remux only"));
    }

    /// Every mix edit (clip gain keyframes, fades, duck, sync drift, `project.mix`, CLI
    /// overrides) changes the audio key and nothing else: a remux, never a video render.
    #[test]
    fn mix_edits_change_only_the_audio() {
        let base = || timeline(shots(), vec![music("song1")]);
        let a = plan_of(&base());
        let keyed = |t: &Timeline| plan_of(t);
        let mut edits: Vec<(&str, Plan)> = Vec::new();

        let gain = json!({"gain_db": [
            {"t": {"OTIO_SCHEMA": "RationalTime.1", "rate": RATE, "value": 0.0}, "v": -6.0},
            {"t": {"OTIO_SCHEMA": "RationalTime.1", "rate": RATE, "value": 48.0}, "v": 0.0}
        ]});
        let fade = json!({"fade_in": 1.5});
        for (name, params) in [("gain keyframes", gain), ("fade", fade)] {
            let t = timeline(
                shots(),
                vec![with_fx(
                    music("song1"),
                    effect("ffcine.audio.level/1", params),
                )],
            );
            edits.push((name, keyed(&t)));
        }

        let mut t = base();
        let mut h2 = Track::new("H2", track_kind::AUDIO);
        let mut rec = music("h2-0001");
        if let Item::Clip(c) = &mut rec {
            c.ffcine_mut().insert(
                "sync".into(),
                json!({"schema": "ffcine.audio.sync/1", "video": "a", "offset": 3.2,
                       "drift_ppm": 12.0, "confidence": 0.8}),
            );
        }
        h2.children = vec![rec];
        t.tracks.children.push(Item::Track(h2));
        let synced = keyed(&t);
        if let Some(Item::Track(h2)) = t.tracks.children.last_mut()
            && let Some(Item::Clip(c)) = h2.children.first_mut()
        {
            c.ffcine_mut()["sync"]["drift_ppm"] = json!(15.0);
        }
        let drifted = keyed(&t);
        assert_ne!(synced.audio, drifted.audio, "sync drift");
        edits.push(("synced recorder", synced));

        let mut t = base();
        let mut project = t.project().unwrap().unwrap();
        project.mix.duck_db = Some(-10.0);
        t.set_project(&project).unwrap();
        edits.push(("project.mix", keyed(&t)));

        let mut over = a.clone();
        if let Some(au) = &mut over.audio {
            let mut o = au.options.clone();
            o.master.lufs = Some(-16.0);
            au.set_options(o);
        }
        edits.push(("override", over));

        for (name, b) in &edits {
            assert_eq!(a.program_key(), b.program_key(), "{name}");
            let d = Diff::new(&a, b);
            assert_eq!(d.render_count(), 0, "{name}");
            assert!(d.audio_changed, "{name}");
        }
    }

    /// With no audio tracks the camera audio is the dialogue: a grade edit changes its segment,
    /// not the mix; a level edit on the clip changes the mix, not the segment.
    #[test]
    fn camera_audio_is_keyed_apart_from_the_picture() {
        let a = plan_of(&timeline(shots(), vec![]));
        assert_eq!(a.audio.as_ref().map(|x| x.clips), Some(4));
        let mut v = shots();
        v[1] = clip("b", "B", 100.0, 72.0, "day");
        let graded = plan_of(&timeline(v, vec![]));
        assert_eq!(Diff::new(&a, &graded).render_count(), 1);
        assert_eq!(
            a.audio.as_ref().map(|x| &x.key),
            graded.audio.as_ref().map(|x| &x.key)
        );
        let mut v = shots();
        v[1] = with_fx(
            v[1].clone(),
            effect("ffcine.audio.level/1", json!({"gain_db": 3.0})),
        );
        let louder = plan_of(&timeline(v, vec![]));
        assert_eq!(a.program_key(), louder.program_key());
        assert!(Diff::new(&a, &louder).audio_changed);
    }

    #[test]
    fn dissolve_splits_the_overlap_out() {
        let t = |extra: f64| {
            let mut v = shots();
            v.insert(
                2,
                Item::Transition(Transition::dissolve(
                    RationalTime::new(6.0, RATE),
                    RationalTime::new(6.0 + extra, RATE),
                )),
            );
            plan_of(&timeline(v, vec![]))
        };
        let p = t(0.0);
        let names: Vec<_> = p
            .segments
            .iter()
            .map(|s| (s.name.as_str(), s.frames))
            .collect();
        assert_eq!(
            names,
            [("a", 48), ("b", 66), ("b x c", 12), ("c", 24), ("d", 50)]
        );
        assert_eq!(p.frames(), 48 + 72 + 30 + 50);
        let Unit::Transition { a, b, .. } = &p.segments[2].unit else {
            panic!("not a dissolve")
        };
        // The outgoing side continues where "b" stops; the incoming side starts 6 frames before
        // "c"'s cut.
        assert!((a.start - (100.0 + 66.0) / RATE).abs() < 1e-9);
        assert!((b.start - (10.0 - 6.0) / RATE).abs() < 1e-9);
        // A longer dissolve changes the overlap and the incoming clip, not the others.
        let q = t(4.0);
        assert_eq!(Diff::new(&p, &q).render_count(), 2);
    }

    #[test]
    fn gaps_freezes_and_speed() {
        let mut freeze = clip("f", "A", 47.0, 24.0, "day");
        let mut fast = clip("s", "B", 0.0, 24.0, "day");
        if let Item::Clip(c) = &mut freeze {
            c.effects
                .push(Effect::FreezeFrame(LinearTimeWarp::freeze_frame()));
        }
        if let Item::Clip(c) = &mut fast {
            c.effects
                .push(Effect::LinearTimeWarp(LinearTimeWarp::new(2.0)));
        }
        let p = plan_of(&timeline(
            vec![
                Item::Gap(Gap::new(RationalTime::new(12.0, RATE))),
                Item::Gap(Gap::new(RationalTime::new(12.0, RATE))),
                freeze,
                fast,
            ],
            vec![],
        ));
        assert_eq!(p.segments[0].unit, Unit::Gap);
        assert_eq!(p.segments[0].frames, 24);
        let speeds: Vec<f64> = p.segments[1..]
            .iter()
            .map(|s| match &s.unit {
                Unit::Shot(s) => s.speed,
                _ => f64::NAN,
            })
            .collect();
        assert_eq!(speeds, [0.0, 2.0]);
    }

    #[test]
    fn text_cuts_split_long_units_only() {
        let p = plan_of(&timeline(vec![clip("a", "A", 100.0, 240.0, "day")], vec![]));
        let shot = p.segments[0].unit.clone();
        let units = vec![("a".to_owned(), 240, shot), ("g".into(), 48, Unit::Gap)];
        // 10 is too close to the start, 250 too close to the gap's end.
        let out = split_at(units, &[10, 100, 110, 200, 250], RATE);
        let lens: Vec<i64> = out.iter().map(|u| u.1).collect();
        assert_eq!(lens, [100, 100, 40, 48]);
        let Unit::Shot(second) = &out[1].2 else {
            panic!()
        };
        assert!((second.start - 200.0 / RATE).abs() < 1e-9);
        assert_eq!(second.frames, 100);
    }

    fn with_filter(mut item: Item, schema: &str, params: &Value) -> Item {
        if let Item::Clip(c) = &mut item {
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                schema,
                params.as_object().cloned().unwrap_or_default(),
            )));
        }
        item
    }

    /// Per-clip filter parameters: `(clip index, schema, params)`.
    fn filtered(fx: &[(usize, &str, Value)]) -> Vec<Item> {
        let mut v = shots();
        for (i, schema, params) in fx {
            v[*i] = with_filter(v[*i].clone(), schema, params);
        }
        v
    }

    fn base_fx() -> Vec<(usize, &'static str, Value)> {
        vec![
            (
                0,
                "ffcine.time.ramp/1",
                json!({"points": [[0, 1], [0.5, 2], [1, 1]],
                                             "source_duration": 3.0}),
            ),
            (1, "ffcine.fx.stabilize/1", json!({"smoothing": 20})),
            (
                2,
                "ffcine.fx.denoise/1",
                json!({"luma": 1.0, "temporal": {"radius": 2}}),
            ),
            (
                3,
                "ffcine.geo.reframe/1",
                json!({"zoom": 1.2, "profiles": {"reel": {"crop_x": 0.2}}}),
            ),
        ]
    }

    fn changed(a: &Plan, b: &Plan) -> Vec<String> {
        a.segments
            .iter()
            .zip(&b.segments)
            .filter(|(x, y)| x.key != y.key)
            .map(|(x, _)| x.name.clone())
            .collect()
    }

    #[test]
    fn filter_edits_rerender_only_their_clip() {
        let base = plan_of(&timeline(filtered(&base_fx()), vec![]));
        // Nothing changed: every segment comes from the cache.
        let again = plan_of(&timeline(filtered(&base_fx()), vec![]));
        assert_eq!(Diff::new(&base, &again).render_count(), 0);
        assert_eq!(base.master_key(), again.master_key());
        // Adding the filters re-renders the four clips.
        let none = plan_of(&timeline(shots(), vec![]));
        assert_eq!(Diff::new(&none, &base).render_count(), 4);

        let edits: [(usize, &str, Value); 6] = [
            (
                0,
                "ffcine.time.ramp/1",
                json!({"points": [[0, 1], [0.5, 2.5], [1, 1]],
                                             "source_duration": 3.0}),
            ),
            (1, "ffcine.fx.stabilize/1", json!({"smoothing": 30})),
            (
                1,
                "ffcine.fx.stabilize/1",
                json!({"smoothing": 20, "zoom": 1.1}),
            ),
            (
                2,
                "ffcine.fx.denoise/1",
                json!({"luma": 1.0, "temporal": {"radius": 3}}),
            ),
            (
                2,
                "ffcine.fx.denoise/1",
                json!({"luma": 1.0, "temporal": {"radius": 2, "motion": "blocks"}}),
            ),
            (
                3,
                "ffcine.geo.reframe/1",
                json!({"zoom": 1.3, "profiles": {"reel": {"crop_x": 0.2}}}),
            ),
        ];
        let names = ["a", "b", "c", "d"];
        for (i, schema, params) in edits {
            let mut fx = base_fx();
            fx.retain(|f| f.0 != i);
            fx.push((i, schema, params.clone()));
            let p = plan_of(&timeline(filtered(&fx), vec![]));
            assert_eq!(changed(&base, &p), [names[i]], "{schema} {params}");
        }
        // The grade's exposure and denoise number are keyed with the clip.
        let mut v = filtered(&base_fx());
        v[1] = with_filter(
            v[1].clone(),
            GRADE_SCHEMA,
            &json!({"look": "night", "exposure": 0.5}),
        );
        assert_eq!(changed(&base, &plan_of(&timeline(v, vec![]))), ["b"]);
    }

    #[test]
    fn blend_is_a_keyed_filter() {
        let base = plan_of(&timeline(shots(), vec![]));
        let blend = |n: f64| {
            let mut v = shots();
            v[2] = with_filter(v[2].clone(), "ffcine.time.blend/1", &json!({"frames": n}));
            plan_of(&timeline(v, vec![]))
        };
        let p = blend(3.0);
        assert_eq!(changed(&base, &p), ["c"]);
        let Unit::Shot(s) = &p.segments[2].unit else {
            panic!("shot")
        };
        assert_eq!(s.filters.blend, Some(3));
        assert!(s.fx.is_empty(), "keyed as a filter, not an effect");
        assert_eq!(changed(&p, &blend(4.0)), ["c"]);
        // One frame is no blend.
        assert_eq!(changed(&base, &blend(1.0)), Vec::<String>::new());
    }

    #[test]
    fn reframe_override_is_keyed_per_aspect() {
        let reel =
            Profile::resolve("preview", &ffcine_timeline::Output::default(), Some("reel")).unwrap();
        let at = |fx: &[(usize, &str, Value)], p: &Profile| {
            plan(
                &timeline(filtered(fx), vec![]),
                Path::new("/proj"),
                p,
                &Fake,
                None,
            )
            .unwrap()
        };
        let mut fx = base_fx();
        fx[3].2 = json!({"zoom": 1.2, "profiles": {"reel": {"crop_x": 0.8}}});
        // The reel override changes the reel render of "d" only, not the landscape one.
        assert_eq!(changed(&at(&base_fx(), &reel), &at(&fx, &reel)), ["d"]);
        let land = Profile::preview();
        assert_eq!(
            changed(&at(&base_fx(), &land), &at(&fx, &land)),
            Vec::<String>::new()
        );
        let reframe = |p: &Profile| match &at(&fx, p).segments[3].unit {
            Unit::Shot(d) => d.filters.reframe.clone(),
            _ => None,
        };
        assert_ne!(reframe(&reel), reframe(&land));
    }

    #[test]
    fn nle_geometry_is_read_and_keyed() {
        let at = |fx: &[(usize, &str, Value)]| {
            plan(
                &timeline(filtered(fx), vec![]),
                Path::new("/proj"),
                &Profile::preview(),
                &Fake,
                None,
            )
            .unwrap()
        };
        let mut fx = base_fx();
        fx.push((
            2,
            "ffcine.geo.transform/1",
            json!({"units": "picture", "y": "down",
                   "scale_x": [{"t": 0.0, "v": 1.0}, {"t": 1.0, "v": 0.5}]}),
        ));
        fx.push((2, "ffcine.geo.crop/1", json!({"left": 0.1})));
        let p = at(&fx);
        assert_eq!(changed(&at(&base_fx()), &p), ["c"]);
        let Unit::Shot(c) = &p.segments[2].unit else {
            panic!("shot")
        };
        assert!(!c.filters.geo.transforms.is_empty() && c.filters.geo.crop.is_some());
        // Animated: keyed by where the piece sits in its clip.
        assert!(c.filters.whole_clip());
        let mut bad = base_fx();
        bad.push((2, "ffcine.geo.transform/1", json!({"units": "inches"})));
        assert!(
            plan(
                &timeline(filtered(&bad), vec![]),
                Path::new("/proj"),
                &Profile::preview(),
                &Fake,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn stabilized_pieces_share_one_span_and_detect_inputs_are_keyed() {
        let p = plan_of(&timeline(filtered(&base_fx()), vec![]));
        let Unit::Shot(b) = p.segments[1].unit.clone() else {
            panic!()
        };
        let recipe = b.filters.recipe().to_string();
        assert!(recipe.contains(&format!(
            "\"motion_cache\":{}",
            ffcine_stabilize::cache::FORMAT_VERSION
        )));
        assert!(recipe.contains("analysis_width"));
        // Text cuts split "b" in three; every piece analyses the whole clip.
        let mut units = vec![("b".to_owned(), 72, Unit::Shot(b))];
        units = split_at(units, &[24, 48], RATE);
        set_spans(&mut units);
        let pieces: Vec<(i64, (i64, i64))> = units
            .iter()
            .map(|u| match &u.2 {
                Unit::Shot(s) => (s.clip_at, s.span),
                _ => panic!(),
            })
            .collect();
        assert_eq!(pieces, [(0, (0, 72)), (24, (0, 72)), (48, (0, 72))]);
    }

    #[test]
    fn denoised_pieces_read_their_neighbours_inside_the_clip() {
        let p = plan_of(&timeline(filtered(&base_fx()), vec![]));
        let Unit::Shot(mut b) = p.segments[1].unit.clone() else {
            panic!()
        };
        b.filters.denoise = grade_denoise(2.0);
        let len = b.clip_frames;
        assert_eq!(len, 72);
        let units = split_at(vec![("b".to_owned(), len, Unit::Shot(b))], &[24], RATE);
        let ctx: Vec<(i64, i64, (i64, i64))> = units
            .iter()
            .map(|u| match &u.2 {
                Unit::Shot(s) => (s.clip_at, s.frames, s.context()),
                _ => panic!(),
            })
            .collect();
        assert_eq!(ctx, [(0, 24, (0, 2)), (24, 48, (2, 0))]);
        for (at, n, (before, after)) in ctx {
            assert!(at - before >= 0 && at + n + after <= len);
        }
        let Unit::Shot(s) = &units[1].2 else { panic!() };
        assert_eq!(s.recipe()["context"], json!([2, 0]));
        // A frame from either end: one frame of history, one of lookahead.
        let inner = s.shifted(-23, RATE).with_frames(70);
        assert_eq!((inner.clip_at, inner.context()), (1, (1, 1)));
        let mut frozen = s.clone();
        frozen.speed = 0.0;
        assert_eq!(frozen.context(), (0, 0));
    }

    #[test]
    fn ramp_maps_like_render_py() {
        let r = Ramp {
            points: vec![(0.0, 1.0), (0.5, 3.0), (1.0, 1.0)],
            source_duration: 6.0,
        };
        let segs = r.segments();
        assert_eq!(segs.len(), 2);
        // 3 s of source from speed 1 to 3: ln(3) / (2/3) s of output.
        assert!((segs[1].4 - 3.0_f64.ln() * 1.5).abs() < 1e-9);
        // Inverse: the mid point of the source is reached at the first segment's end.
        assert!((r.source_time(segs[1].4) - 3.0).abs() < 1e-9);
        assert!((r.source_time(0.5) - 0.5).abs() < 0.2);
        let e = r.setpts();
        assert!(e.starts_with("setpts='(if(lt(((PTS-STARTPTS)*TB),3.000000),"));
        assert!(e.contains("log("));
    }

    #[test]
    fn lut_is_keyed_by_content() {
        struct Changed;
        impl Fingerprinter for Changed {
            fn fingerprint(&self, path: &Path) -> Fingerprint {
                Fake.fingerprint(path)
            }
            fn content_hash(&self, _: &Path) -> Option<String> {
                Some("edited".into())
            }
        }
        let t = timeline(shots(), vec![]);
        let a = plan_of(&t);
        let b = plan(&t, Path::new("/proj"), &Profile::preview(), &Changed, None).unwrap();
        assert_eq!(Diff::new(&a, &b).render_count(), 4);
        let c = plan(&t, Path::new("/proj"), &Profile::master(), &Fake, None).unwrap();
        assert_eq!(Diff::new(&a, &c).render_count(), 4);
    }

    #[allow(clippy::needless_pass_by_value)]
    fn effect(schema: &str, v: Value) -> Effect {
        Effect::Effect(GenericEffect::ffcine(
            schema,
            v.as_object().cloned().unwrap_or_default(),
        ))
    }

    fn with_fx(mut item: Item, e: Effect) -> Item {
        if let Item::Clip(c) = &mut item {
            c.effects.push(e);
        }
        item
    }

    #[test]
    fn finish_effects_key_only_their_clip() {
        let base = plan_of(&timeline(shots(), vec![]));
        let mut v = shots();
        v[1] = with_fx(
            v.remove(1),
            effect("ffcine.fx.vignette/1", json!({"amount": 0.3})),
        );
        let p = plan_of(&timeline(v, vec![]));
        assert_eq!(Diff::new(&base, &p).render_count(), 1);
        let Unit::Shot(b) = &p.segments[1].unit else {
            panic!()
        };
        assert!((b.finish.vignette - 0.3).abs() < 1e-9);
        // A fade keys the piece's place in its clip: text cuts then give pieces their own keys.
        let mut v = shots();
        v[0] = with_fx(
            clip("a", "A", 0.0, 240.0, "day"),
            effect("ffcine.fx.fade/1", json!({"out": 1.0})),
        );
        let p = plan_of(&timeline(v, vec![]));
        let Unit::Shot(a) = &p.segments[0].unit else {
            panic!()
        };
        assert!(a.finish.is_timed());
        let pieces = split_at(vec![("a".into(), 240, Unit::Shot(a.clone()))], &[120], RATE);
        let Unit::Shot(second) = &pieces[1].2 else {
            panic!()
        };
        assert_eq!((second.clip_at, second.clip_frames), (120, 240));
    }

    #[test]
    fn transition_kinds_key_the_overlap_only() {
        let t = |meta: Option<Value>| {
            let mut v = shots();
            let mut tr =
                Transition::dissolve(RationalTime::new(0.0, RATE), RationalTime::new(8.0, RATE));
            if let Some(m) = meta {
                tr.ffcine_mut()
                    .extend(m.as_object().cloned().unwrap_or_default());
            }
            v.insert(2, Item::Transition(tr));
            plan_of(&timeline(v, vec![]))
        };
        let d = t(None);
        let w = t(Some(
            json!({"schema": "ffcine.transition/1", "kind": "whip"}),
        ));
        let Unit::Transition { kind, .. } = &w.segments[2].unit else {
            panic!()
        };
        assert_eq!(*kind, TransitionKind::Whip([-1.0, 0.0]));
        assert_eq!(Diff::new(&d, &w).render_count(), 1);
    }

    fn split_stack(panels: usize, frames: f64) -> Item {
        let mut st = ffcine_timeline::Stack {
            name: "split".into(),
            ..ffcine_timeline::Stack::default()
        };
        for k in 0..panels {
            let mut t = Track::new(format!("panel {k}"), track_kind::VIDEO);
            t.children = vec![clip(&format!("p{k}"), &format!("P{k}"), 0.0, frames, "day")];
            st.children.push(Item::Track(t));
        }
        st.effects.push(effect(
            "ffcine.comp.layout/1",
            json!({"layout": "split", "gap": 0.01}),
        ));
        Item::Stack(st)
    }

    #[test]
    fn split_screens_keep_every_panel() {
        let mut v = shots();
        v.insert(1, split_stack(3, 60.0));
        let p = plan_of(&timeline(v, vec![]));
        let Unit::Split(sp) = &p.segments[1].unit else {
            panic!("not a split: {:?}", p.segments[1].name)
        };
        assert_eq!(sp.panels.len(), 3);
        assert!(
            sp.panels
                .iter()
                .all(|parts| parts.len() == 1 && parts[0].shot.is_some())
        );
        assert_eq!(p.segments[1].frames, 60);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        // Long splits are cut at text/overlay points like shots, each piece knowing its offset.
        let pieces = split_at(vec![("s".into(), 60, Unit::Split(sp.clone()))], &[30], RATE);
        let Unit::Split(second) = &pieces[1].2 else {
            panic!()
        };
        assert_eq!(second.at, 30);
    }

    fn overlay_track(at: f64, frames: f64, opacity: f64) -> Item {
        let mut t = Track::new("PiP 1", track_kind::VIDEO);
        let logo = MediaReference::External(ExternalReference::from_path(
            Path::new("/media/logo.png"),
            None,
        ));
        let mut c = Clip::new(
            "logo",
            logo,
            Some(TimeRange::from_frames(0.0, frames, RATE)),
        );
        c.effects.push(effect(
            "ffcine.geo.transform/1",
            json!({"units": "frame", "translate_x": 0.3, "scale_x": 0.2, "scale_y": 0.2}),
        ));
        c.effects
            .push(effect("ffcine.comp.blend/1", json!({"opacity": opacity})));
        t.children = vec![
            Item::Gap(Gap::new(RationalTime::new(at, RATE))),
            Item::Clip(c),
        ];
        Item::Track(t)
    }

    #[test]
    fn overlays_cut_and_key_the_segments_under_them() {
        let with = |opacity: f64| {
            let mut t = timeline(shots(), vec![]);
            t.tracks
                .children
                .insert(1, overlay_track(60.0, 60.0, opacity));
            plan_of(&t)
        };
        let base = plan_of(&timeline(shots(), vec![]));
        let p = with(1.0);
        // "b" (48..120) is cut at 60: its first 12 frames are too short a piece, so the cut is
        // skipped and "b" carries the overlay from frame 12.
        let layered: Vec<_> = p
            .segments
            .iter()
            .filter(|s| !s.layers.is_empty())
            .map(|s| {
                (
                    s.name.as_str(),
                    s.layers[0].from,
                    s.layers[0].to,
                    s.layers[0].clip_at,
                )
            })
            .collect();
        assert_eq!(layered, [("b", 12, 72, 0)]);
        assert!(matches!(
            p.segments[1].layers[0].clip.source,
            OverlaySource::Image { .. }
        ));
        let d = Diff::new(&base, &p);
        assert_eq!(d.render_count(), 1);
        // Changing the overlay re-renders only what it covers.
        assert_eq!(Diff::new(&p, &with(0.5)).render_count(), 1);
    }

    #[test]
    fn timeline_grain_keys_everything_per_profile() {
        let mut t = timeline(shots(), vec![]);
        let base = plan_of(&t);
        t.tracks.effects.push(effect(
            "ffcine.fx.grain/1",
            json!({"strength": 1.7, "chroma": 0.4, "profiles": {"master": {"strength": 0.0}}}),
        ));
        let p = plan_of(&t);
        assert_eq!(
            p.grain.as_ref().and_then(|g| g.get("strength")),
            Some(&json!(1.7))
        );
        assert!(
            p.grain
                .as_ref()
                .is_some_and(|g| !g.contains_key("profiles"))
        );
        assert_eq!(Diff::new(&base, &p).render_count(), 4);
        let m = plan(&t, Path::new("/proj"), &Profile::master(), &Fake, None).unwrap();
        assert!(m.grain.is_none());
    }

    #[test]
    fn dissolves_into_and_out_of_splits() {
        let mut v = shots();
        let tr = || {
            Item::Transition(Transition::dissolve(
                RationalTime::new(0.0, RATE),
                RationalTime::new(10.0, RATE),
            ))
        };
        v.insert(1, tr());
        v.insert(2, split_stack(2, 60.0));
        v.insert(3, tr());
        let p = plan_of(&timeline(v, vec![]));
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        let kinds: Vec<_> = p
            .segments
            .iter()
            .map(|s| match &s.unit {
                Unit::SplitDissolve {
                    split, split_out, ..
                } => format!("sd{}@{}", u8::from(*split_out), split.at),
                Unit::Split(sp) => format!("split@{}", sp.at),
                _ => s.name.clone(),
            })
            .collect();
        assert_eq!(kinds, ["a", "sd0@0", "split@10", "sd1@60", "b", "c", "d"]);
        assert_eq!(p.frames(), 48 + 60 + 72 + 30 + 50);
    }
}
