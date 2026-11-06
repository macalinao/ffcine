//! Lowering (`docs/compositing.md#rendering`): a piece of a comp becomes a base input and grade
//! plus GPU [`Layer`]s, every track sampled per frame on the CPU, and a key recipe of those
//! sampled values.

use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ffcine_core::anim::{Animatable, Clock, Env, Track};
use ffcine_core::camera::{self, CameraLut, Profile, ProfileChoice};
use ffcine_core::{Lut, LutType, Result};
use ffcine_hooks::ShaderEffect;
use ffcine_hooks::grade::{Luts, grade_for, look_params};
use ffcine_render::extra::{Filter, filter_hook, strict_hooks};
use ffcine_render::filter_node::{Clock as FClock, FILTER_TABLE_REVISION, Node, Prep, Reach};
use ffcine_render::{
    BlendMode, Crop, Deband, Grade, Hook, Image, Layer, LayerSource, LutStage, Overlay, ParamTrack,
    Scene, Size, shaders,
};
use ffcine_text::{TextClip, TextLayer};
use ffcine_timeline::{ClipGrade, SourceKind};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::model::{
    Comp, CompLayer, CropSpec, FILTER_HINT, FilterOpt, FilterSpec, Fit, LayerGrade, MaskShape,
    Scale, Source, err, limits,
};
use crate::time;

/// [`FILTER_TABLE_REVISION`], which tests can bump.
fn table_revision() -> u32 {
    #[cfg(test)]
    return TABLE.get();
    #[cfg(not(test))]
    FILTER_TABLE_REVISION
}

#[cfg(test)]
thread_local! {
    pub(crate) static TABLE: std::cell::Cell<u32> =
        const { std::cell::Cell::new(FILTER_TABLE_REVISION) };
}

/// What lowering needs to know about media files.
pub trait Media {
    /// A video's or still's display size, px (rotation and HEIF grids applied).
    fn dims(&self, path: &Path) -> Result<(u32, u32)>;
    /// The file's identity in keys (a fingerprint: path, size, modification time).
    fn identity(&self, path: &Path) -> Value;
    /// A video's or still's camera profile for `camera: auto` ([`Profile::detect_file`]).
    fn profile(&self, _path: &Path) -> Result<Profile> {
        Ok(Profile::Rec709)
    }
    /// A video's average frame rate and frame count, for time remapping.
    fn video_rate(&self, path: &Path) -> Result<(f64, u64)> {
        Err(err(
            path.display().to_string(),
            "the frame rate is unknown (needed for a time remap)",
        ))
    }
    /// A small file's contents in keys (a filter node's LUTs, fonts, text files): by default its
    /// [`identity`](Self::identity).
    fn content_hash(&self, path: &Path) -> Value {
        self.identity(path)
    }
    /// The ffmpeg build that runs filter nodes (in their keys).
    fn ffmpeg_build(&self) -> String {
        String::from("unknown")
    }
    /// `profile`'s camera LUT (`None` for display-referred profiles).
    fn camera_lut(&self, profile: Profile) -> Result<Option<CameraLut>> {
        profile.camera_lut(&camera::lut_dirs())
    }
}

/// The pass's input.
#[derive(Debug, Clone, PartialEq)]
pub enum Base {
    /// No promoted layer: a black clip, replaced by the canvas background hook.
    Black,
    /// The bottom layer promoted to the pass input (`docs/compositing.md#rendering`, step 2).
    Video { input: PathBuf, start: Duration },
}

/// A lowered piece: comp frames `from..from + frames`.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub from: i64,
    pub frames: i64,
    pub base: Base,
    /// The pass's grade (size: the render size).
    pub grade: Grade,
    /// Bottom first, frame ranges relative to `from`.
    pub layers: Vec<Layer>,
    /// Indices of the comp layers in `layers` (the promoted one excluded).
    pub shown: Vec<usize>,
    /// Text layers' masks, frame ranges relative to `from`, each under the layer it is below.
    pub overlays: Vec<Overlay>,
    /// Canonical JSON of what the piece renders: sampled values, not track definitions.
    pub recipe: Value,
}

/// The first frame at or after `t` s.
#[allow(clippy::cast_possible_truncation)]
fn frame_at(t: f64, fps: f64) -> i64 {
    (t * fps - 1e-6).ceil() as i64
}

/// A frame map in a key: its first frame and a digest of all of them.
fn map_digest(picks: &[i64]) -> Value {
    let mut h = Sha256::new();
    for k in picks {
        h.update(k.to_le_bytes());
    }
    let d = h.finalize();
    json!({
        "first": picks.first(),
        "frames": picks.len(),
        "digest": d[..16].iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        }),
    })
}

/// `values` in a key: the value if they're all equal, else a digest of their bits.
fn digest(values: &[f32]) -> Value {
    match values.first() {
        None => Value::Null,
        Some(v) if values.iter().all(|x| x.to_bits() == v.to_bits()) => json!(v),
        Some(_) => {
            let mut h = Sha256::new();
            for v in values {
                h.update(v.to_bits().to_le_bytes());
            }
            let d = h.finalize();
            Value::String(d[..16].iter().fold(String::new(), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            }))
        }
    }
}

/// A hook param: a constant (`params`) or a per-frame track.
fn param(h: &mut Hook, name: &str, values: Vec<f32>) {
    match values.first() {
        Some(v) if values.iter().all(|x| x.to_bits() == v.to_bits()) => {
            h.params.push((name.into(), f64::from(*v)));
        }
        Some(_) => h.tracks.push(ParamTrack {
            name: name.into(),
            values,
        }),
        None => {}
    }
}

fn hook(name: &str, source: &str) -> Hook {
    Hook {
        name: name.into(),
        source: source.into(),
        params: Vec::new(),
        tracks: Vec::new(),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn f32s(v: impl IntoIterator<Item = f64>) -> Vec<f32> {
    v.into_iter().map(|x| x as f32).collect()
}

/// [`shaders::finish`] as a fade towards a colour, `None` when it never fades.
fn fade_hook(name: &str, fade: &[f64], colour: &[[f64; 4]]) -> Option<Hook> {
    if fade.iter().all(|f| *f >= 1.0) {
        return None;
    }
    let mut h = hook(name, shaders::finish());
    param(&mut h, "fade", f32s(fade.iter().map(|f| f.clamp(0.0, 1.0))));
    for (i, c) in ["fade_r", "fade_g", "fade_b"].into_iter().enumerate() {
        param(&mut h, c, f32s(colour.iter().map(|v| v[i])));
    }
    Some(h)
}

fn grade_recipe(g: &Grade) -> Value {
    let mut v = grade_json(g);
    // Only when set, so step 2's keys stay as they were.
    if !g.crops.is_empty() {
        v["crops"] = digest(
            &g.crops
                .iter()
                .flat_map(|c| [c.x, c.y, c.width, c.height])
                .collect::<Vec<_>>(),
        );
    }
    v
}

fn grade_json(g: &Grade) -> Value {
    json!({
        "size": g.size.map(|s| [s.width, s.height]),
        "crop": g.crop.map(|c| [c.x, c.y, c.width, c.height]),
        "hooks": g.hooks.iter().map(|h| json!({
            "name": h.name,
            "params": h.params,
            "tracks": h.tracks.iter().map(|t| json!([t.name, digest(&t.values)])).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// A digest of `bytes`, hex.
fn sha(bytes: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    let mut h = Sha256::new();
    for b in bytes {
        h.update(b.as_ref());
    }
    h.finalize()[..16].iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A LUT in keys: its contents, not its file's identity.
fn lut_digest(l: &Lut) -> String {
    let head = [
        u32::try_from(l.size)
            .unwrap_or(u32::MAX)
            .to_le_bytes()
            .to_vec(),
        l.domain_min
            .iter()
            .chain(&l.domain_max)
            .flat_map(|v| v.to_bits().to_le_bytes())
            .collect(),
    ];
    sha(head.into_iter().chain(
        l.data
            .iter()
            .map(|p| p.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect()),
    ))
}

/// A layer's resolved grade (without hooks) and its hooks, plus what keys need beyond
/// [`grade_json`].
struct Graded {
    grade: Grade,
    hooks: Vec<Hook>,
    recipe: Value,
}

/// A solid layer's picture: one white pixel, coloured by [`shaders::solid`].
fn solid_image() -> LayerSource {
    LayerSource::Image(Arc::new(Image {
        width: 1,
        height: 1,
        data: vec![255; 4],
    }))
}

/// A layer's source windows (`content`, `crop`).
struct Framing {
    crop: Option<Crop>,
    /// Per visible frame, when they move (else empty and `crop` holds).
    crops: Vec<Crop>,
    /// The picture in box px.
    pic: [f64; 2],
    /// The source after `crop` (its first frame), px: `sw`, `sh` in expressions.
    cropped: [f64; 2],
    /// `crop.feather` per visible frame, box px.
    feather: Vec<f64>,
}

/// A `w x h` window in `r` (`[x, y, w, h]`), placed by `pos` (0..1) in the slack.
fn place(r: [f64; 4], w: f64, h: f64, [px, py]: [f64; 2]) -> [f64; 4] {
    [
        r[0] + (r[2] - w) * px.clamp(0.0, 1.0),
        r[1] + (r[3] - h) * py.clamp(0.0, 1.0),
        w,
        h,
    ]
}

/// `r` zoomed in by `zoom` (below 1 counts as 1, so the window stays in the picture) about
/// `pos`, as reframe's.
fn zoomed(r: [f64; 4], zoom: f64, pos: [f64; 2]) -> [f64; 4] {
    let z = if zoom.is_finite() { zoom.max(1.0) } else { 1.0 };
    place(r, r[2] / z, r[3] / z, pos)
}

/// A sampled layer over its visible frames.
struct Sampled {
    rects: Vec<Crop>,
    rotation: Vec<f64>,
    opacity: Vec<f64>,
}

impl Comp {
    /// Frames in the comp.
    pub fn frames(&self) -> i64 {
        frame_at(self.canvas.duration, self.canvas.fps).max(1)
    }

    /// Comp frames where `layer` shows.
    pub fn span(&self, layer: &CompLayer) -> Range<i64> {
        let fps = self.canvas.fps;
        let n = self.frames();
        frame_at(layer.in_, fps).clamp(0, n)..frame_at(layer.out, fps).clamp(0, n)
    }

    /// Pieces of the comp, cut at layer in/out points, none shorter than a second (unless the
    /// comp is).
    pub fn pieces(&self) -> Vec<Range<i64>> {
        let n = self.frames();
        #[allow(clippy::cast_possible_truncation)]
        let min = (self.canvas.fps.round() as i64).max(1);
        let mut cuts: Vec<i64> = self
            .layers
            .iter()
            .flat_map(|l| {
                let s = self.span(l);
                [s.start, s.end]
            })
            .filter(|c| *c > 0 && *c < n)
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut out = Vec::new();
        let mut start = 0;
        for c in cuts {
            if c - start >= min && n - c >= min {
                out.push(start..c);
                start = c;
            }
        }
        out.push(start..n);
        out
    }

    fn env<'a>(&self, l: &'a CompLayer, source: [f64; 2]) -> Env<'a> {
        Env {
            fps: self.canvas.fps,
            canvas: self.canvas.size.map(f64::from),
            size: l.size,
            source,
            in_: l.in_,
            out: l.out,
            total: self.canvas.duration,
            source_in: l.source_in,
            vars: Some(&l.vars),
        }
    }

    /// The source windows of a layer with `content` or `crop` over its visible frames `vis`.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::float_cmp,
        clippy::many_single_char_names,
        clippy::too_many_lines
    )]
    fn framing(
        &self,
        l: &CompLayer,
        [sw, sh]: [f64; 2],
        span: &Range<i64>,
        vis: Range<i64>,
        env: &Env,
        path: &str,
    ) -> Result<Framing> {
        let at = |name: &str| format!("{path}.{name}");
        let n = usize::try_from(vis.end - vis.start).unwrap_or(0);
        // The cropped source, per frame.
        let regions: Vec<[f64; 4]> = match &l.crop {
            None => vec![[0.0, 0.0, sw, sh]; n],
            Some(CropSpec {
                left,
                right,
                top,
                bottom,
                ..
            }) => {
                let side = |t: &Track<f64>, name: &str| -> Result<Vec<f64>> {
                    let v = sample(t, &at(&format!("crop.{name}")), vis.clone(), env)?;
                    match v.iter().find(|f| !(0.0..1.0).contains(*f)) {
                        Some(f) => Err(err(
                            at(&format!("crop.{name}")),
                            format!("{f}: expected a fraction 0 to 1"),
                        )),
                        None => Ok(v),
                    }
                };
                let (l_, r) = (side(left, "left")?, side(right, "right")?);
                let (t, b) = (side(top, "top")?, side(bottom, "bottom")?);
                (0..n)
                    .map(|j| {
                        if l_[j] + r[j] >= 1.0 || t[j] + b[j] >= 1.0 {
                            return Err(err(
                                at("crop"),
                                "left + right and top + bottom must each be under 1",
                            ));
                        }
                        Ok([
                            l_[j] * sw,
                            t[j] * sh,
                            (1.0 - l_[j] - r[j]) * sw,
                            (1.0 - t[j] - b[j]) * sh,
                        ])
                    })
                    .collect::<Result<_>>()?
            }
        };
        let first = regions.first().copied().unwrap_or([0.0, 0.0, sw, sh]);
        let cropped = [first[2], first[3]];
        let [bw, bh] = l.size;
        let pic = match l.fit {
            Fit::Cover | Fit::Fill => l.size,
            Fit::Contain => {
                let s = (bw / cropped[0]).min(bh / cropped[1]);
                [cropped[0] * s, cropped[1] * s]
            }
            Fit::None => [cropped[0].min(bw), cropped[1].min(bh)],
        };
        let half = Track::Const(0.5);
        let one = Track::Const(1.0);
        let centre = Track::Const([0.5, 0.5]);
        let (zoom, zc, cx, cy) = match &l.content {
            Some(c) => (&c.zoom, &c.zoom_center, &c.crop_x, &c.crop_y),
            None => (&one, &centre, &half, &half),
        };
        let zoom = sample(zoom, &at("content.zoom"), vis.clone(), env)?;
        if let Some(z) = zoom
            .iter()
            .find(|z| !(z.is_finite() && **z > 0.0 && **z <= limits::ZOOM))
        {
            return Err(err(
                at("content.zoom"),
                format!("{z}: expected a zoom > 0, at most {}", limits::ZOOM),
            ));
        }
        let zc = sample(zc, &at("content.zoom_center"), vis.clone(), env)?;
        let cx = sample(cx, &at("content.crop_x"), vis.clone(), env)?;
        let cy = sample(cy, &at("content.crop_y"), vis.clone(), env)?;
        finite(&at("content"), zc.iter().flatten().chain(&cx).chain(&cy))?;
        let fps = self.canvas.fps;
        let life = (span.end - span.start - 1).max(1) as f64;
        let windows: Vec<[f64; 4]> = (0..n)
            .map(|j| {
                let r = regions[j];
                let base = match l.fit {
                    Fit::Cover => {
                        let ar = bw / bh;
                        let (w, h) = if r[2] / r[3] > ar {
                            (r[3] * ar, r[3])
                        } else {
                            (r[2], r[2] / ar)
                        };
                        place(r, w, h, [cx[j], cy[j]])
                    }
                    Fit::Fill | Fit::Contain => r,
                    Fit::None => place(r, r[2].min(bw), r[3].min(bh), [cx[j], cy[j]]),
                };
                // Layer frame `i`: punches and the push run on the layer's clock.
                let i = (vis.start + j as i64 - span.start) as f64;
                let punch = l
                    .content
                    .iter()
                    .flat_map(|c| c.punch.iter())
                    .rev()
                    .find(|p| {
                        let to = p.to.map_or(f64::INFINITY, |t| (t * fps).round());
                        ((p.at * fps).round()..to).contains(&i)
                    });
                let (z, c) = punch.map_or((zoom[j], zc[j]), |p| {
                    (p.zoom * zoom[j], p.center.unwrap_or(zc[j]))
                });
                let w = zoomed(base, z, c);
                match l.content.as_ref().and_then(|c| c.push.map(|p| (c, p))) {
                    Some((c, [z0, z1])) => {
                        let u = (i / life).clamp(0.0, 1.0);
                        let e = 0.5 * u + 0.5 * (0.5 - 0.5 * (std::f64::consts::PI * u).cos());
                        let centre = match c.pan {
                            Some([a, b]) => [a[0] + (b[0] - a[0]) * e, a[1] + (b[1] - a[1]) * e],
                            None => c.push_center.unwrap_or(zc[j]),
                        };
                        zoomed(w, z0 + (z1 - z0) * e, centre)
                    }
                    None => w,
                }
            })
            .collect();
        // Zooms and crops multiply: keep the window a usable size.
        if windows
            .iter()
            .any(|w| !(w[2] >= sw / limits::ZOOM && w[3] >= sh / limits::ZOOM))
        {
            return Err(err(
                at("content"),
                format!(
                    "zoom and crop together go in over {}x (the source window would be under \
                     1/{} of the source a side)",
                    limits::ZOOM,
                    limits::ZOOM
                ),
            ));
        }
        #[allow(clippy::cast_possible_truncation)]
        let to_crop = |w: &[f64; 4]| Crop {
            x: w[0] as f32,
            y: w[1] as f32,
            width: w[2] as f32,
            height: w[3] as f32,
        };
        let full = [0.0, 0.0, sw, sh];
        let (crop, crops) = match windows.first() {
            None => (None, Vec::new()),
            Some(w) if windows.iter().all(|v| v == w) => {
                ((*w != full).then(|| to_crop(w)), Vec::new())
            }
            Some(_) => (None, windows.iter().map(to_crop).collect()),
        };
        let feather = match &l.crop {
            Some(c) => sample(&c.feather, &at("crop.feather"), vis, env)?
                .into_iter()
                .zip(&windows)
                // A fraction of the source width, in box px.
                .map(|(f, w)| f * sw * pic[0] / w[2].max(1e-6))
                .collect(),
            None => Vec::new(),
        };
        Ok(Framing {
            crop,
            crops,
            pic,
            cropped,
            feather,
        })
    }

    fn canvas_env(&self) -> Env<'_> {
        let [cw, ch] = self.canvas.size.map(f64::from);
        Env {
            fps: self.canvas.fps,
            canvas: [cw, ch],
            size: [cw, ch],
            source: [cw, ch],
            in_: 0.0,
            out: self.canvas.duration,
            total: self.canvas.duration,
            source_in: 0.0,
            vars: Some(&self.vars),
        }
    }

    /// Lowers comp frames `range` for a render of `out` px (lengths scale by out/canvas).
    #[allow(
        clippy::too_many_lines,
        clippy::many_single_char_names,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn lower(&self, range: Range<i64>, out: Size, media: &dyn Media) -> Result<Piece> {
        self.lower_in(range, out, media, true)
    }

    /// A filtered video layer's prep over its visible frames `vis` of `span`, and its key: the
    /// node's recipe (`docs/compositing.md#libavfilter-escape-hatch`) and the frames shown.
    #[allow(
        clippy::too_many_arguments,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn filter_prep(
        &self,
        l: &CompLayer,
        p: &Path,
        node: &Node,
        reach: Reach,
        span: &Range<i64>,
        vis: &Range<i64>,
        env: &Env<'_>,
        path: &str,
        media: &dyn Media,
    ) -> Result<(Prep, Value)> {
        let at = |k: &str| format!("{path}.{k}");
        let (rate, n) = media.video_rate(p)?;
        if !(rate.is_finite() && rate > 0.0 && n > 0) {
            return Err(err(
                at("filter"),
                format!("{}: no frames or frame rate to filter", p.display()),
            ));
        }
        // The frames fed: the piece's, with pre-roll and post-roll inside the layer, or the
        // whole layer.
        let ext = if reach.whole_clip {
            span.clone()
        } else {
            (vis.start - i64::from(reach.preroll)).max(span.start)
                ..(vis.end + i64::from(reach.postroll)).min(span.end)
        };
        let fps = self.canvas.fps;
        let picks: Vec<i64> = match &l.time {
            Some(t) => time::source_times(t, env, ext.clone(), &at("time"))?
                .into_iter()
                .map(|(s, mid)| time::pick(s, mid, rate, n))
                .collect(),
            None => ext
                .clone()
                .map(|f| {
                    let s = l.source_in + f as f64 / fps - l.in_;
                    time::pick(s, s + 0.5 / fps, rate, n)
                })
                .collect(),
        };
        let lo = picks.iter().copied().min().unwrap_or(0);
        let pts = match node.clock {
            FClock::Layer => ext.start - span.start,
            FClock::Comp => ext.start,
        };
        let shown = (vis.start - ext.start) as usize..(vis.end - ext.start) as usize;
        let (store, show) = if reach.whole_clip {
            (0..picks.len(), shown)
        } else {
            (shown.clone(), 0..shown.len())
        };
        let mut rec = json!({
            "graph": node.graph,
            "hw": node.hw.as_str(),
            "clock": node.clock.as_str(),
            "preroll": reach.preroll,
            "postroll": reach.postroll,
            "whole_clip": reach.whole_clip,
            "timing": if node.free_timing { "free" } else { "keep" },
            "fps": fps,
            "files": node.files.iter().map(|f| media.content_hash(f)).collect::<Vec<_>>(),
            "ffmpeg": media.ffmpeg_build(),
            "video": media.identity(p),
            "map": map_digest(&picks),
            "pts": pts,
        });
        if reach.stateful() {
            rec["table"] = json!(table_revision());
        }
        if node.clock == FClock::Comp {
            rec["in"] = json!(l.in_);
        }
        let id = json!({"node": rec, "store": [store.start, store.end]});
        let key = Sha256::digest(id.to_string().as_bytes())[..16].iter().fold(
            String::new(),
            |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            },
        );
        let prep = Prep {
            graph: node.run.clone(),
            labelled: node.labelled,
            dims: media.dims(p)?,
            hw: node.hw,
            free_timing: node.free_timing,
            input: p.to_owned(),
            // Half a frame early: the decoder keeps the first frame at or after it.
            start: Duration::from_secs_f64(((lo as f64 - 0.5) / rate).max(0.0)),
            map: picks.iter().map(|k| k - lo).collect(),
            rate: fps,
            pts,
            store: store.clone(),
            show: show.clone(),
            key,
        };
        Ok((prep, json!({"filter": id, "show": [show.start, show.end]})))
    }

    /// Layer `l`'s grade over visible frames `vis`
    /// (`docs/compositing.md#per-layer-grade-and-filters`): `grade`, or `{}` when the comp has
    /// a `project`, resolved like a timeline clip's and built by [`grade_for`]. `None`: ungraded.
    #[allow(
        clippy::too_many_lines,
        clippy::many_single_char_names,
        clippy::cast_possible_truncation
    )]
    fn layer_grade(
        &self,
        l: &CompLayer,
        path: &str,
        vis: &Range<i64>,
        env: &Env,
        media: &dyn Media,
    ) -> Result<Option<Graded>> {
        let cp = &*self.project;
        let empty = LayerGrade::default();
        let lg = match (&l.grade, &l.source) {
            (_, Source::Solid(_) | Source::Text { .. }) => return Ok(None),
            (Some(g), _) => g,
            (None, _) if cp.given => &empty,
            (None, _) => return Ok(None),
        };
        let gp = format!("{path}.grade");
        let at = |k: &str| format!("{gp}.{k}");
        let gerr = |e: &dyn std::fmt::Display| err(&gp, e.to_string());
        let resolve = |c: &ClipGrade, kind| {
            cp.project
                .resolve_grade(Some(c), kind)
                .map_err(|e| gerr(&e))
        };
        // The camera: the layer's, the look's, then the project's; `auto` probes the source.
        let merged = resolve(&lg.params, SourceKind::Display)?;
        let choice = match merged.camera.as_deref() {
            None => ProfileChoice::Auto,
            Some(c) => c
                .parse::<ProfileChoice>()
                .map_err(|e| err(at("camera"), e.to_string()))?,
        };
        let profile = match (&l.source, choice) {
            (Source::Video(_), ProfileChoice::Fixed(p)) => p,
            (Source::Video(p) | Source::Still(p), ProfileChoice::Auto) => media.profile(p)?,
            // A still with a project camera, or a comp: display-referred.
            _ => Profile::Rec709,
        };
        let log = !matches!(profile, Profile::Rec709 | Profile::Hdr);
        let kind = if log {
            SourceKind::Log
        } else {
            SourceKind::Display
        };
        let mut params = resolve(&lg.params, kind)?;
        // Per-layer denoise is not built yet: a project's is the main picture's only.
        params.denoise = None;
        let (mut g, warn) =
            grade_for(&cp.project, &cp.dir, &params, &mut Luts::default()).map_err(|e| gerr(&e))?;
        if let Some(w) = warn.first() {
            return Err(err(&gp, w));
        }
        // The camera LUT, as `ffcine grade --camera`: ahead of the grade LUT; with `auto` only
        // when the grade has no LUT of its own (a project's log LUT is the camera transform).
        if matches!(l.source, Source::Video(_))
            && log
            && (matches!(choice, ProfileChoice::Fixed(_))
                || cp.project.lut_path(params.lut.as_deref()).is_none())
            && let Some(cam) = media.camera_lut(profile)?
        {
            if profile.as_str().starts_with("dlog") {
                g.deband.get_or_insert_with(Deband::default);
            }
            g.input_color.primaries.get_or_insert(cam.primaries);
            g.input_color.transfer.get_or_insert(cam.transfer);
            g.input_lut = Some(LutStage {
                lut: cam.lut,
                kind: LutType::Normalized,
            });
        }
        let mut hooks = std::mem::take(&mut g.hooks);

        // Animated look params: resolved per frame (so the light rule still scales them).
        if !lg.tracks.is_empty() {
            let mut cols: Vec<(&str, Vec<f64>)> = Vec::new();
            for (k, t) in &lg.tracks {
                let v = sample(t, &at(k), vis.clone(), env)?;
                finite(&at(k), &v)?;
                if *k == "gamma"
                    && let Some(x) = v.iter().find(|x| **x < 0.01)
                {
                    return Err(err(at(k), format!("{x}: at least 0.01")));
                }
                cols.push((k, v));
            }
            let n = cols[0].1.len();
            let mut frames: Vec<Vec<(String, f64)>> = Vec::with_capacity(n);
            for j in 0..n {
                let mut c = lg.params.clone();
                for (k, v) in &cols {
                    let x = Some(v[j]);
                    let p = &mut c.params;
                    match *k {
                        "exposure" => p.exposure = x,
                        "warmth" => p.warmth = x,
                        "tint" => p.tint = x,
                        "contrast" => p.contrast = x,
                        "gamma" => p.gamma = x,
                        "lift" => p.lift = x,
                        "gain" => p.gain = x,
                        "saturation" => p.saturation = x,
                        _ => unreachable!("not a look param: {k}"),
                    }
                }
                frames.push(look_params(&resolve(&c, kind)?).params());
            }
            let mut h = hook("look", shaders::look());
            for (i, (name, _)) in frames[0].iter().enumerate() {
                param(&mut h, name, f32s(frames.iter().map(|f| f[i].1)));
            }
            match hooks.first_mut() {
                Some(first) if first.name == "look" => *first = h,
                _ => hooks.insert(0, h),
            }
        }

        // `filters`, after the resolved grade's `extra`.
        for (fi, f) in lg.filters.iter().enumerate() {
            let fp = at(&format!("filters[{fi}]"));
            match f {
                FilterSpec::Chain(c) => {
                    hooks.extend(
                        strict_hooks(std::slice::from_ref(c))
                            .map_err(|e| err(&fp, format!("{e} {FILTER_HINT}")))?,
                    );
                }
                FilterSpec::Object { name, opts } => {
                    let fp = format!("{fp}.{name}");
                    let mut cols = Vec::new();
                    for (k, o) in opts {
                        cols.push(match o {
                            FilterOpt::Num(t) => {
                                let v = sample(t, &format!("{fp}.{k}"), vis.clone(), env)?;
                                finite(&format!("{fp}.{k}"), &v)?;
                                Err(v)
                            }
                            FilterOpt::Text(s) => Ok(s.clone()),
                        });
                    }
                    let moving = cols.iter().any(
                        |c| matches!(c, Err(v) if v.iter().any(|x| x.to_bits() != v[0].to_bits())),
                    );
                    let n = if moving { vis.end - vis.start } else { 1 };
                    let mut per = Vec::new();
                    for j in 0..usize::try_from(n).unwrap_or(0) {
                        let f = Filter {
                            name: name.clone(),
                            args: opts
                                .iter()
                                .zip(&cols)
                                .map(|((k, _), c)| {
                                    (
                                        Some(k.clone()),
                                        match c {
                                            Ok(s) => s.clone(),
                                            Err(v) => format!("{}", v[j]),
                                        },
                                    )
                                })
                                .collect(),
                        };
                        per.push(
                            filter_hook(&f).map_err(|e| err(&fp, format!("{e} {FILTER_HINT}")))?,
                        );
                    }
                    let first = &per[0];
                    if per.iter().any(|h| {
                        h.source != first.source
                            || h.params.len() != first.params.len()
                            || h.params.iter().zip(&first.params).any(|(a, b)| a.0 != b.0)
                    }) {
                        return Err(err(
                            &fp,
                            "an animated option changes the filter's shader (only its numbers can \
                             move)",
                        ));
                    }
                    let mut h = hook(&first.name, &first.source);
                    for (i, (pn, _)) in first.params.iter().enumerate() {
                        param(&mut h, pn, f32s(per.iter().map(|x| x.params[i].1)));
                    }
                    hooks.push(h);
                }
            }
        }

        // `shaders`, as `ffcine.fx.shader/1`.
        for (si, sh) in lg.shaders.iter().enumerate() {
            let sp = at(&format!("shaders[{si}]"));
            let (name, source) = ShaderEffect {
                shader: sh.shader.clone(),
                params: Vec::new(),
            }
            .load(&cp.project, &cp.dir)
            .map_err(|e| err(&sp, e.to_string()))?;
            let info = ffcine_hooks::parse(&source).map_err(|e| err(&sp, e.to_string()))?;
            let mut h = hook(&name, &source);
            for (k, t) in &sh.params {
                let pp = format!("{sp}.params.{k}");
                let p = info.param(k).ok_or_else(|| {
                    err(
                        &pp,
                        format!(
                            "not a //!PARAM of {} (has: {})",
                            sh.shader,
                            info.params
                                .iter()
                                .map(|p| p.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                })?;
                let v = sample(t, &pp, vis.clone(), env)?
                    .into_iter()
                    .map(|x| p.check(x))
                    .collect::<Result<Vec<_>>>()
                    .map_err(|e| err(&pp, e.to_string()))?;
                param(&mut h, k, f32s(v));
            }
            hooks.push(h);
        }

        let recipe = json!({
            "profile": profile.as_str(),
            "input_lut": g.input_lut.as_ref().map(|s| lut_digest(&s.lut)),
            "input_color": format!("{:?}", g.input_color),
            "lut": g.lut.as_ref().map(|s| lut_digest(&s.lut)),
            "rgb_gain": g.rgb_gain,
            "sharpen": g.sharpen,
            "deband": g.deband.is_some(),
            "shaders": hooks.iter().map(|h| sha([&h.source])).collect::<Vec<_>>(),
        });
        Ok(Some(Graded {
            grade: g,
            hooks,
            recipe,
        }))
    }

    /// [`Self::lower`]; `top` is false for a nested comp (no base promotion: its layers go into
    /// a [`Scene`]).
    #[allow(
        clippy::too_many_lines,
        clippy::many_single_char_names,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn lower_in(
        &self,
        range: Range<i64>,
        out: Size,
        media: &dyn Media,
        top: bool,
    ) -> Result<Piece> {
        let [cw, ch] = self.canvas.size.map(f64::from);
        let k = [f64::from(out.width) / cw, f64::from(out.height) / ch];
        let p0 = range.start;
        let frames = range.end - range.start;
        let cenv = self.canvas_env();
        let canvas_fade = sample(&self.canvas.fade, "canvas.fade", range.clone(), &cenv)?;
        let canvas_colour = sample(
            &self.canvas.fade_colour,
            "canvas.fade_colour",
            range.clone(),
            &cenv,
        )?;
        let mut base = Base::Black;
        let mut base_grade = None;
        let mut layers = Vec::new();
        let mut shown = Vec::new();
        let mut overlays = Vec::new();
        let mut recipes = Vec::new();

        for (i, l) in self.layers.iter().enumerate() {
            let span = self.span(l);
            let vis = span.start.max(range.start)..span.end.min(range.end);
            if vis.is_empty() {
                continue;
            }
            let path = format!("layers[{i}]");
            let at = |name: &str| format!("{path}.{name}");
            // Text: masks drawn under whatever layer comes next (above all if none does).
            if let Source::Text { params, dir } = &l.source {
                let clip = TextClip {
                    id: l.id.clone(),
                    name: l.id.clone().unwrap_or_default(),
                    text: params
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    start: l.in_,
                    end: l.out,
                    track: 0,
                    params: params.clone(),
                };
                let mut text =
                    TextLayer::new(vec![clip], Some([cw as f32, ch as f32]), dir.clone());
                let under = layers.len();
                let first = overlays.len();
                let o = text
                    .overlays(
                        p0 as f64 / self.canvas.fps,
                        frames as u64,
                        self.canvas.fps,
                        out,
                    )
                    .map_err(|e| err(at("source.text"), e.to_string()))?;
                overlays.extend(o.into_iter().map(|mut o| {
                    o.under = Some(under);
                    o
                }));
                // The drawn overlays themselves (masks, placements, fades: they depend on the
                // piece's start and the font files' contents), and the font files' identities.
                recipes.push(json!({
                    "text": params,
                    "in": l.in_,
                    "out": l.out,
                    "under": under,
                    "overlays": overlay_digest(&overlays[first..]),
                    "fonts": font_files(params, dir).iter().map(|f| media.identity(f)).collect::<Vec<_>>(),
                }));
                continue;
            }
            let (src_dims, source) = match &l.source {
                Source::Video(p) => {
                    let (w, h) = media.dims(p)?;
                    (Some([f64::from(w), f64::from(h)]), None)
                }
                Source::Still(p) => {
                    let (w, h) = media.dims(p)?;
                    if !ffcine_render::still_fits(w, h) {
                        return Err(err(
                            at("source.still"),
                            format!(
                                "{}: {w}x{h} px (a still is at most {} px a side, {} in all)",
                                p.display(),
                                limits::STILL_SIDE,
                                limits::STILL_PIXELS
                            ),
                        ));
                    }
                    (Some([f64::from(w), f64::from(h)]), None)
                }
                Source::Comp(c) => (Some(c.canvas.size.map(f64::from)), None),
                Source::Solid(c) => (None, Some(c)),
                Source::Text { .. } => unreachable!("text is lowered above"),
            };
            // The picture inside the box: the source window (per frame with `content` or
            // `crop`) and its size in box px.
            let framing = match src_dims {
                Some(src) if l.content.is_some() || l.crop.is_some() => {
                    let env = self.env(l, src);
                    Some(self.framing(l, src, &span, vis.clone(), &env, &path)?)
                }
                _ => None,
            };
            let (crop, crops, pic, cropped) = if let Some(f) = &framing {
                (f.crop, f.crops.clone(), f.pic, f.cropped)
            } else {
                let (c, p) = fit(l.fit, src_dims, l.size);
                (c, Vec::new(), p, src_dims.unwrap_or(l.size))
            };
            let env = self.env(l, cropped);
            let off = [(l.size[0] - pic[0]) / 2.0, (l.size[1] - pic[1]) / 2.0];

            let position = sample(
                &l.transform.position,
                &at("transform.position"),
                vis.clone(),
                &env,
            )?;
            let anchor = sample(
                &l.transform.anchor,
                &at("transform.anchor"),
                vis.clone(),
                &env,
            )?;
            let scale: Vec<[f64; 2]> = match &l.transform.scale {
                Scale::Uniform(t) => sample(t, &at("transform.scale"), vis.clone(), &env)?
                    .into_iter()
                    .map(|s| [s.max(0.0); 2])
                    .collect(),
                Scale::Xy(t) => sample(t, &at("transform.scale"), vis.clone(), &env)?
                    .into_iter()
                    .map(|s| s.map(|v| v.max(0.0)))
                    .collect(),
            };
            let rotation = sample(
                &l.transform.rotation,
                &at("transform.rotation"),
                vis.clone(),
                &env,
            )?;
            let mut opacity: Vec<f64> = sample(
                &l.transform.opacity,
                &at("transform.opacity"),
                vis.clone(),
                &env,
            )?
            .into_iter()
            .map(|o| o.clamp(0.0, 1.0))
            .collect();
            let solid = match source {
                Some(c) => {
                    let c = sample(c, &at("source.solid"), vis.clone(), &env)?;
                    for (o, c) in opacity.iter_mut().zip(&c) {
                        *o *= c[3].clamp(0.0, 1.0);
                    }
                    Some(c)
                }
                None => None,
            };
            let radius = sample(&l.radius, &at("radius"), vis.clone(), &env)?;
            finite(&at("radius"), &radius)?;
            let fade = sample(&l.fade, &at("fade"), vis.clone(), &env)?;
            let fade_colour = sample(&l.fade_colour, &at("fade_colour"), vis.clone(), &env)?;

            // The texture: the picture at the largest scale in the piece, in render px.
            let smax = scale
                .iter()
                .fold([0.0f64; 2], |m, s| [m[0].max(s[0]), m[1].max(s[1])]);
            let tw = (pic[0] * smax[0].max(1e-3) * k[0]).ceil().max(2.0);
            let th = (pic[1] * smax[1].max(1e-3) * k[1]).ceil().max(2.0);
            if !(tw.is_finite() && th.is_finite() && tw.max(th) <= limits::TEXTURE_SIDE) {
                return Err(err(
                    at("transform.scale"),
                    format!(
                        "the layer's texture would be {tw}x{th} px at its largest scale (at most {} \
                         px a side at this render size)",
                        limits::TEXTURE_SIDE
                    ),
                ));
            }
            finite(&at("transform.position"), position.iter().flatten())?;
            finite(&at("transform.anchor"), anchor.iter().flatten())?;
            if let Some(r) = rotation
                .iter()
                .find(|r| !r.is_finite() || r.abs() > limits::ROTATION)
            {
                return Err(err(
                    at("transform.rotation"),
                    format!("{r}: at most {} degrees either way", limits::ROTATION),
                ));
            }
            let s = Sampled {
                rects: (0..position.len())
                    .map(|j| {
                        let [sx, sy] = scale[j];
                        // The box middle from the anchor, scaled and turned (counter-clockwise on
                        // screen, y down), from the position.
                        let q = [
                            l.size[0] * (0.5 - anchor[j][0]) * sx,
                            l.size[1] * (0.5 - anchor[j][1]) * sy,
                        ];
                        let (sin, cos) = rotation[j].to_radians().sin_cos();
                        let c = [
                            position[j][0] + cos * q[0] + sin * q[1],
                            position[j][1] - sin * q[0] + cos * q[1],
                        ];
                        let (w, h) = (pic[0] * sx, pic[1] * sy);
                        Crop {
                            x: ((c[0] - w / 2.0) * k[0]) as f32,
                            y: ((c[1] - h / 2.0) * k[1]) as f32,
                            width: (w * k[0]) as f32,
                            height: (h * k[1]) as f32,
                        }
                    })
                    .collect(),
                rotation,
                opacity,
            };

            // Hooks: the solid colour or the layer's grade, the layer's fade, then alpha.
            let graded = self.layer_grade(l, &path, &vis, &env, media)?;
            let (layer_grade, mut hooks, graded_recipe) = match graded {
                Some(g) => (g.grade, g.hooks, Some(g.recipe)),
                None => (Grade::default(), Vec::new(), None),
            };
            if let Some(c) = &solid {
                let mut h = hook("solid", shaders::solid());
                for (n, name) in ["solid_r", "solid_g", "solid_b"].into_iter().enumerate() {
                    param(&mut h, name, f32s(c.iter().map(|v| v[n])));
                }
                hooks.push(h);
            }
            hooks.extend(fade_hook("fade", &fade, &fade_colour));
            // Box px to texture px, horizontally; `ky` is the vertical factor over it (a
            // non-uniform scale), so corners and feathers stay round in box px.
            let px = tw / pic[0].max(1e-6);
            let ky = (th / pic[1].max(1e-6)) / px;
            let edge = framing
                .as_ref()
                .map(|f| f.feather.clone())
                .filter(|f| f.iter().any(|v| *v > 0.0));
            let has_mask = l.mask.is_some() || edge.is_some();
            if has_mask || radius.iter().any(|r| *r > 0.0) || s.opacity.iter().any(|o| *o < 1.0) {
                let mut h = hook("alpha", shaders::layer_alpha());
                param(&mut h, "opacity", f32s(s.opacity.iter().copied()));
                if (ky - 1.0).abs() > 1e-9 {
                    param(&mut h, "ky", vec![ky as f32]);
                }
                param(
                    &mut h,
                    "radius",
                    f32s(radius.iter().map(|r| r.max(0.0) * px)),
                );
                let mask = match (&l.mask, &edge) {
                    (Some(m), _) => {
                        let rect = sample(&m.rect, &at("mask.rect"), vis.clone(), &env)?;
                        let feather = sample(&m.feather, &at("mask.feather"), vis.clone(), &env)?;
                        finite(&at("mask.rect"), rect.iter().flatten())?;
                        finite(&at("mask.feather"), &feather)?;
                        Some((m.shape, rect, feather, m.invert))
                    }
                    // A feathered crop: a rect mask on the picture, the soft edge inside it.
                    (None, Some(f)) => {
                        finite(&at("crop.feather"), f)?;
                        let rect = f
                            .iter()
                            .map(|f| {
                                let f = f.max(0.0);
                                [off[0] + f / 2.0, off[1] + f / 2.0, pic[0] - f, pic[1] - f]
                            })
                            .collect::<Vec<_>>();
                        Some((MaskShape::Rect, rect, f.clone(), false))
                    }
                    (None, None) => None,
                };
                if let Some((shape, rect, feather, invert)) = &mask {
                    param(
                        &mut h,
                        "shape",
                        vec![if *shape == MaskShape::Rect { 1.0 } else { 2.0 }],
                    );
                    param(
                        &mut h,
                        "mask_x",
                        f32s(rect.iter().map(|r| (r[0] - off[0]) / pic[0])),
                    );
                    param(
                        &mut h,
                        "mask_y",
                        f32s(rect.iter().map(|r| (r[1] - off[1]) / pic[1])),
                    );
                    param(&mut h, "mask_w", f32s(rect.iter().map(|r| r[2] / pic[0])));
                    param(&mut h, "mask_h", f32s(rect.iter().map(|r| r[3] / pic[1])));
                    param(
                        &mut h,
                        "feather",
                        f32s(feather.iter().map(|f| f.max(0.0) * px)),
                    );
                    param(&mut h, "invert", vec![if *invert { 1.0 } else { 0.0 }]);
                }
                hooks.push(h);
            }

            let (mut crop, mut crops) = (crop, crops);
            let mut scene = None;
            let mut remap = None;
            let src = match (&l.source, &solid) {
                (Source::Video(p), _) if let Some((node, reach)) = &l.filter => {
                    let (prep, r) =
                        self.filter_prep(l, p, node, *reach, &span, &vis, &env, &path, media)?;
                    remap = Some(r);
                    LayerSource::Filtered(Arc::new(prep))
                }
                (Source::Video(p), _) if let Some(t) = &l.time => {
                    let (rate, n) = media.video_rate(p)?;
                    if !(rate.is_finite() && rate > 0.0 && n > 0) {
                        return Err(err(
                            at("time"),
                            format!("{}: no frames or frame rate to remap", p.display()),
                        ));
                    }
                    let picks: Vec<i64> = time::source_times(t, &env, vis.clone(), &at("time"))?
                        .into_iter()
                        .map(|(s, mid)| time::pick(s, mid, rate, n))
                        .collect();
                    let lo = picks.iter().copied().min().unwrap_or(0);
                    // Which source frames the piece shows: its key.
                    remap = Some(json!({
                        "video": media.identity(p),
                        "map": map_digest(&picks),
                    }));
                    LayerSource::Video {
                        input: p.clone(),
                        // Half a frame early: the decoder keeps the first frame at or after it.
                        start: Duration::from_secs_f64(((lo as f64 - 0.5) / rate).max(0.0)),
                        map: Some(picks.iter().map(|k| k - lo).collect()),
                    }
                }
                (Source::Video(p), _) => {
                    let st = l.source_in + vis.start as f64 / self.canvas.fps - l.in_;
                    LayerSource::Video {
                        input: p.clone(),
                        start: Duration::from_secs_f64(st.max(0.0)),
                        map: None,
                    }
                }
                (Source::Still(p), _) => LayerSource::Still(p.clone()),
                (Source::Comp(c), _) => {
                    // The scene at the texture's resolution: sub-comp px times texture px per
                    // window px, capped.
                    let [sw, sh] = c.canvas.size.map(f64::from);
                    let wmin = |f: fn(&Crop) -> f32, full: f64| {
                        crop.iter()
                            .chain(&crops)
                            .map(|c| f64::from(f(c)))
                            .fold(full, f64::min)
                            .max(1e-3)
                    };
                    let mut size = [
                        sw * tw / wmin(|c| c.width, sw),
                        sh * th / wmin(|c| c.height, sh),
                    ];
                    let cap = limits::TEXTURE_SIDE / size[0].max(size[1]);
                    if cap < 1.0 {
                        size = size.map(|v| v * cap);
                    }
                    let size = size.map(|v| v.ceil().max(2.0));
                    let ratio = [size[0] / sw, size[1] / sh];
                    let scale = |c: &Crop| Crop {
                        x: (f64::from(c.x) * ratio[0]) as f32,
                        y: (f64::from(c.y) * ratio[1]) as f32,
                        width: (f64::from(c.width) * ratio[0]) as f32,
                        height: (f64::from(c.height) * ratio[1]) as f32,
                    };
                    crop = crop.as_ref().map(scale);
                    crops = crops.iter().map(scale).collect();
                    // Its clock: 0 at the layer's `source_in`.
                    let start =
                        (l.source_in * self.canvas.fps).round() as i64 + (vis.start - span.start);
                    let size = Size {
                        width: size[0] as u32,
                        height: size[1] as u32,
                    };
                    let sub =
                        c.lower_in(start..start + (vis.end - vis.start), size, media, false)?;
                    scene = Some(json!({"comp": sub.recipe, "start": start}));
                    LayerSource::Scene(Arc::new(Scene {
                        size,
                        grade: sub.grade,
                        layers: sub.layers,
                        overlays: sub.overlays,
                    }))
                }
                (Source::Solid(_), _) => solid_image(),
                (Source::Text { .. }, _) => unreachable!("text is lowered above"),
            };

            // Base promotion: the bottom layer, a normal opaque unturned video covering the
            // canvas over the whole piece, becomes the pass input.
            let covers = |r: &Crop| {
                (f64::from(r.x)).abs() < 0.5
                    && (f64::from(r.y)).abs() < 0.5
                    && (f64::from(r.width) - f64::from(out.width)).abs() < 0.5
                    && (f64::from(r.height) - f64::from(out.height)).abs() < 0.5
            };
            let promote = top
                && i == 0
                && matches!(l.source, Source::Video(_))
                && l.time.is_none()
                && l.filter.is_none()
                && l.blend == BlendMode::Normal
                && vis == range
                && !has_mask
                && radius.iter().all(|r| *r <= 0.0)
                && s.opacity.iter().all(|o| *o >= 1.0)
                && s.rotation.iter().all(|r| r % 360.0 == 0.0)
                && s.rects.iter().all(covers);
            if promote {
                let LayerSource::Video { input, start, .. } = src else {
                    unreachable!()
                };
                let mut r = json!({
                    "base": media.identity(&input),
                    "start": start.as_secs_f64(),
                });
                if let Some(g) = &graded_recipe {
                    r["layer_grade"] = g.clone();
                }
                recipes.push(r);
                base = Base::Video { input, start };
                base_grade = Some(Grade {
                    size: Some(out),
                    crop,
                    crops,
                    hooks,
                    ..layer_grade
                });
                continue;
            }

            let grade = Grade {
                size: Some(Size {
                    width: tw as u32,
                    height: th as u32,
                }),
                crop,
                crops,
                dither: None,
                peak_detect: false,
                hooks,
                ..layer_grade
            };
            let rotation = if s.rotation.iter().all(|r| *r == 0.0) {
                Vec::new()
            } else {
                f32s(s.rotation.iter().copied())
            };
            let layer = Layer {
                source: src,
                grade,
                from: vis.start - p0,
                to: vis.end - p0,
                rects: s.rects,
                rotation,
                blend: l.blend,
            };
            let mut r = json!({
                "source": match &layer.source {
                    LayerSource::Video { input, start, .. } => remap.take().unwrap_or_else(|| json!({"video": media.identity(input), "start": start.as_secs_f64()})),
                    LayerSource::Image(_) => json!("solid"),
                    LayerSource::Still(p) => json!({"still": media.identity(p)}),
                    LayerSource::Scene(_) => scene.take().unwrap_or_default(),
                    LayerSource::Filtered(_) => remap.take().unwrap_or_default(),
                },
                "from": layer.from,
                "to": layer.to,
                "rects": digest(&layer.rects.iter().flat_map(|r| [r.x, r.y, r.width, r.height]).collect::<Vec<_>>()),
                "rotation": digest(&layer.rotation),
                "blend": layer.blend.as_str(),
                "grade": grade_recipe(&layer.grade),
            });
            if let Some(g) = graded_recipe {
                r["layer_grade"] = g;
            }
            recipes.push(r);
            layers.push(layer);
            shown.push(i);
        }

        let grade = if let Some(g) = base_grade {
            g
        } else {
            let bg = sample(
                &self.canvas.background,
                "canvas.background",
                range.clone(),
                &cenv,
            )?;
            let mut h = hook("background", shaders::background());
            for (n, name) in ["bg_r", "bg_g", "bg_b"].into_iter().enumerate() {
                param(&mut h, name, f32s(bg.iter().map(|v| v[n])));
            }
            Grade {
                size: Some(out),
                hooks: vec![h],
                ..Grade::default()
            }
        };
        // The canvas fade: a top-most normal solid of `fade_colour` at opacity `1 - fade`, so
        // it fades the composite whatever the layers' blend modes.
        if canvas_fade.iter().any(|f| *f < 1.0) {
            let mut h = hook("solid", shaders::solid());
            for (n, name) in ["solid_r", "solid_g", "solid_b"].into_iter().enumerate() {
                param(&mut h, name, f32s(canvas_colour.iter().map(|v| v[n])));
            }
            let mut a = hook("alpha", shaders::layer_alpha());
            param(
                &mut a,
                "opacity",
                f32s(canvas_fade.iter().map(|f| 1.0 - f.clamp(0.0, 1.0))),
            );
            let layer = Layer {
                source: solid_image(),
                grade: Grade {
                    size: Some(Size {
                        width: 2,
                        height: 2,
                    }),
                    hooks: vec![h, a],
                    ..Grade::default()
                },
                from: 0,
                to: frames,
                rects: Vec::new(),
                rotation: Vec::new(),
                blend: BlendMode::Normal,
            };
            recipes.push(json!({
                "canvas_fade": grade_recipe(&layer.grade),
            }));
            layers.push(layer);
        }
        let recipe = json!({
            "comp": 1,
            "canvas": self.canvas.size,
            "out": [out.width, out.height],
            "grade": grade_recipe(&grade),
            "layers": recipes,
        });
        Ok(Piece {
            from: p0,
            frames,
            base,
            grade,
            layers,
            shown,
            overlays,
            recipe,
        })
    }
}

/// A digest of what `overlays` draw on every frame: masks, placements, frame ranges and fades.
fn overlay_digest(overlays: &[Overlay]) -> Value {
    let mut h = Sha256::new();
    let f32s = |h: &mut Sha256, v: &[f32]| {
        for x in v {
            h.update(x.to_bits().to_le_bytes());
        }
    };
    let parts = |h: &mut Sha256, parts: &[ffcine_render::OverlayPart]| {
        h.update((parts.len() as u64).to_le_bytes());
        for p in parts {
            f32s(h, &p.pos);
            f32s(h, &p.size.unwrap_or([f32::NAN; 2]));
            f32s(h, &p.src.unwrap_or([f32::NAN; 4]));
            f32s(h, &p.colour);
        }
    };
    for o in overlays {
        h.update(o.mask.width.to_le_bytes());
        h.update(o.mask.height.to_le_bytes());
        h.update(Sha256::digest(&o.mask.data));
        parts(&mut h, &o.parts);
        h.update(o.from.to_le_bytes());
        h.update(o.to.to_le_bytes());
        h.update(o.fade_in.to_bits().to_le_bytes());
        h.update(o.fade_out.to_bits().to_le_bytes());
        if let Some(m) = &o.motion {
            h.update(m.first.to_le_bytes());
            h.update((m.frames.len() as u64).to_le_bytes());
            for f in &m.frames {
                parts(&mut h, f);
            }
        }
    }
    let d = h.finalize();
    Value::String(d[..16].iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// The font files a text layer's `params` name (`font_file`, also under `profiles`), resolved
/// as `ffcine-text` does: against `fonts_dir`, then `dir`.
fn font_files(params: &serde_json::Map<String, Value>, dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let top = Value::Object(params.clone());
    let mut stack = vec![&top];
    while let Some(v) = stack.pop() {
        match v {
            Value::Object(m) => {
                if let Some(f) = m.get("font_file").and_then(Value::as_str) {
                    let sub = m.get("fonts_dir").and_then(Value::as_str).unwrap_or("");
                    out.push(dir.join(sub).join(f));
                }
                stack.extend(m.values());
            }
            Value::Array(a) => stack.extend(a),
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// An error at `what` unless every value is finite and within [`limits::COORD`] (position,
/// anchor, radius, mask).
fn finite<'a>(what: &str, values: impl IntoIterator<Item = &'a f64>) -> Result<()> {
    match values
        .into_iter()
        .find(|v| !v.is_finite() || v.abs() > limits::COORD)
    {
        Some(v) => Err(err(
            what,
            format!("{v}: out of range (at most {:e} either way)", limits::COORD),
        )),
        None => Ok(()),
    }
}

/// `track` over comp frames `range`, errors prefixed with the parameter's path.
fn sample<T: Animatable>(
    track: &Track<T>,
    what: &str,
    range: Range<i64>,
    env: &Env,
) -> Result<Vec<T>> {
    track
        .sample(Clock::Layer, range, env)
        .map_err(|e| err(what, e.to_string()))
}

/// The source window (`None`: all of it) and the picture's size in box px for `fit`.
fn fit(fit: Fit, src: Option<[f64; 2]>, size: [f64; 2]) -> (Option<Crop>, [f64; 2]) {
    let Some([sw, sh]) = src.filter(|s| s[0] > 0.0 && s[1] > 0.0) else {
        return (None, size);
    };
    #[allow(clippy::cast_possible_truncation)]
    let crop = |w: f64, h: f64| {
        Some(Crop {
            x: ((sw - w) / 2.0) as f32,
            y: ((sh - h) / 2.0) as f32,
            width: w as f32,
            height: h as f32,
        })
    };
    let [bw, bh] = size;
    match fit {
        Fit::Fill => (None, size),
        Fit::Cover => {
            let s = (bw / sw).max(bh / sh);
            let (w, h) = ((bw / s).min(sw), (bh / s).min(sh));
            if (w - sw).abs() < 1e-6 && (h - sh).abs() < 1e-6 {
                (None, size)
            } else {
                (crop(w, h), size)
            }
        }
        Fit::Contain => {
            let s = (bw / sw).min(bh / sh);
            (None, [sw * s, sh * s])
        }
        Fit::None => {
            // A source pixel is a canvas pixel.
            let (w, h) = (sw.min(bw), sh.min(bh));
            let c = if w < sw || h < sh { crop(w, h) } else { None };
            (c, [w, h])
        }
    }
}
