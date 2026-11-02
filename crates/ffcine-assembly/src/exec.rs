//! Runs a [`Plan`]: renders segments missing from the cache, joins them by stream copy, mixes
//! the audio, and muxes the master.
//!
//! Cache layout (`<cache>/`): `seg/<key>.mp4` (one per segment), `audio/<key>-aac<bitrate>.m4a`
//! (the mix, encoded), `tmp/` (intermediates). Every entry is written to a `.partial` file and
//! renamed when complete, so a killed render resumes: the next run reuses what finished. Nothing is pruned, and old masters are never touched.
//!
//! Shots go through the GPU [`Pipeline`] (decode, grade, scale, encode). The audio is
//! `ffcine_audio::render_audio`'s mix, encoded once. The joins, retime prep, dissolve blends, AAC
//! encode and mux use the `ffmpeg` CLI.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use std::sync::Arc;

use ffcine_filters::Grain;
use ffcine_render::{
    Backend, Codec, Crop, Decode, Device, Grade, Hook, Image, Layer, LayerSource, Lut, LutStage,
    LutType, Overlay, ParamTrack, Pipeline, Rational, Size, shaders,
};
use serde_json::{Value, json};

use crate::comp::{self, Finish, Layout, Placement, TransitionKind};
use crate::plan::{
    AudioPlan, OverlaySource, Plan, SegLayer, Segment, Shot, Split, TextLayer, Unit, grain_for,
};
use crate::recipe::Key;
use ffcine_render::Warp;
use ffcine_stabilize::{Detection, SourceRange};

/// Errors from [`Renderer::render`].
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("{0}")]
    Render(#[from] ffcine_core::Error),
    #[error("{what}: {source}")]
    Io {
        what: String,
        source: std::io::Error,
    },
    #[error("grade of {shot:?}: {msg}")]
    Grade { shot: String, msg: String },
    #[error("stabilizing {shot:?}: {msg}")]
    Stabilize { shot: String, msg: String },
    #[error("ffmpeg ({what}) failed: {log}")]
    Ffmpeg { what: String, log: String },
}

type Result<T> = std::result::Result<T, ExecError>;

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> ExecError {
    let what = what.into();
    move |source| ExecError::Io { what, source }
}

/// How progress is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Progress {
    /// Lines on stderr.
    #[default]
    Human,
    /// One JSON object per event on stdout.
    Json,
    Quiet,
}

/// What a render did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub segments: usize,
    pub rendered: usize,
    pub rendered_frames: i64,
    /// Time spent rendering segments.
    pub render: Duration,
    /// Mixing audio (`None`: cached or no audio).
    pub audio: Option<Duration>,
    /// Muxing the master (`None`: already up to date).
    pub mux: Option<Duration>,
    pub total: Duration,
}

/// Renders plans against a cache directory.
pub struct Renderer<'a> {
    pub backend: &'a dyn Backend,
    pub cache: PathBuf,
    pub ffmpeg: OsString,
    pub device: Device,
    pub decode: Decode,
    pub progress: Progress,
    /// Builds the GPU grade for a shot's resolved params (LUT, look hook, sharpen); the size is
    /// set by the renderer. `None`: the LUT and sharpen only.
    pub grader: Option<&'a Grader<'a>>,
    /// Stabilizer motion detect on the GPU (NVOF, cached on disk): a source file and range to
    /// per-frame motions. Required by stabilized clips; there is no CPU fallback.
    pub detect: Option<&'a Detector<'a>>,
}

/// See [`Renderer::grader`].
pub type Grader<'g> = dyn Fn(&Shot) -> std::result::Result<Grade, String> + 'g;

/// See [`Renderer::detect`].
pub type Detector<'d> = dyn Fn(&Path, &SourceRange) -> std::result::Result<Detection, String> + 'd;

/// The timeline rate as a rational (`23.976...` -> `24000/1001`).
#[allow(clippy::cast_possible_truncation)]
pub fn rational(rate: f64) -> Rational {
    for den in [1, 1001, 1000, 100] {
        let num = rate * f64::from(den);
        if (num - num.round()).abs() < 1e-3 * f64::from(den) / 1000.0_f64.max(1.0) + 1e-6 {
            return Rational {
                num: num.round() as i32,
                den,
            };
        }
    }
    Rational {
        num: (rate * 1000.0).round() as i32,
        den: 1000,
    }
}

#[allow(clippy::cast_precision_loss)]
fn secs(frames: i64, rate: f64) -> f64 {
    frames as f64 / rate
}

/// A ramped piece's retime: where to seek (source seconds after the clip's start), how much to
/// read, and the filter giving output frames `from..from + n` of the whole clip.
#[derive(Debug, Clone, PartialEq)]
struct RampCut {
    seek: f64,
    duration: f64,
    filter: String,
}

/// See [`RampCut`]. Source time is read on the source's own clock (`-copyts -start_at_zero`)
/// from the clip's start, so a source frame lands on the same output time whichever piece
/// decodes it, and `fps` numbers output frames from the clip's start: a piece seeks to just
/// before its first source frame and gets the frames the whole clip would have there.
#[allow(clippy::cast_precision_loss)]
fn ramp_cut(ramp: &crate::plan::Ramp, clip_start: f64, from: i64, n: i64, rate: f64) -> RampCut {
    let r = rational(rate);
    // Half a second covers any source frame interval before the first one needed.
    let seek = (ramp.source_time(secs(from, rate)) - 0.5).max(0.0);
    RampCut {
        seek,
        duration: ramp.source_duration - seek + secs(2, rate),
        filter: format!(
            "{},fps={}/{},trim=start_pts={from}:end_pts={},setpts=PTS-STARTPTS",
            ramp.setpts_of(&format!("(PTS*TB-{clip_start})")),
            r.num,
            r.den,
            from + n
        ),
    }
}

impl Renderer<'_> {
    fn dir(&self, sub: &str) -> Result<PathBuf> {
        let d = self.cache.join(sub);
        std::fs::create_dir_all(&d).map_err(io(format!("creating {}", d.display())))?;
        Ok(d)
    }

    fn event(&self, v: &serde_json::Value, human: impl FnOnce() -> String) {
        match self.progress {
            Progress::Human => eprintln!("{}", human()),
            Progress::Json => println!("{v}"),
            Progress::Quiet => {}
        }
    }

    fn ffmpeg(&self, what: &str, args: &[OsString]) -> Result<()> {
        let out = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
            .args(args)
            .output()
            .map_err(io(format!("running {}", self.ffmpeg.to_string_lossy())))?;
        if out.status.success() {
            Ok(())
        } else {
            let log = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = log.lines().rev().take(8).collect();
            Err(ExecError::Ffmpeg {
                what: what.into(),
                log: tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
            })
        }
    }

    /// Runs `make(partial)` unless `path` exists, then renames the partial into place.
    fn atomic(path: &Path, make: impl FnOnce(&Path) -> Result<()>) -> Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mkv");
        let partial = path.with_extension(format!("partial.{ext}"));
        make(&partial)?;
        std::fs::rename(&partial, path).map_err(io(format!("renaming {}", partial.display())))?;
        Ok(true)
    }

    /// Renders `plan` to `output` (an `.mp4`); writes `<output>.ffcine.json` next to it.
    #[allow(clippy::too_many_lines)]
    pub fn render(
        &self,
        plan: &Plan,
        mut text: Option<&mut TextLayer>,
        output: &Path,
    ) -> Result<Report> {
        let t0 = Instant::now();
        if self.cache.is_relative() {
            let cwd = std::env::current_dir().map_err(io("current directory"))?;
            return Renderer {
                cache: cwd.join(&self.cache),
                ffmpeg: self.ffmpeg.clone(),
                ..*self
            }
            .render(plan, text, output);
        }
        let mut report = Report {
            segments: plan.segments.len(),
            ..Report::default()
        };
        if let Some(t) = text.as_deref_mut() {
            let a = &plan.profile.aspect;
            let safe = a.default_safe_area();
            #[allow(clippy::cast_possible_truncation)]
            t.set_profile(
                &a.name,
                [safe.top, safe.right, safe.bottom, safe.left].map(|v| v as f32),
            );
        }
        let manifest_path = manifest_path(output);
        let master_key = plan.master_key();
        if output.exists() && read_manifest_key(&manifest_path).as_ref() == Some(&master_key) {
            self.event(&json!({"event": "up_to_date"}), || {
                format!("{}: up to date", output.display())
            });
            report.total = t0.elapsed();
            return Ok(report);
        }

        let seg_dir = self.dir("seg")?;
        let mut files = Vec::new();
        let n = plan.segments.len();
        for (i, s) in plan.segments.iter().enumerate() {
            let path = seg_dir.join(format!("{}.mp4", s.key));
            let t = Instant::now();
            let made = Self::atomic(&path, |tmp| {
                #[allow(clippy::cast_precision_loss)]
                let overlays = match text.as_deref_mut() {
                    Some(t) => t.overlays(
                        s.start as f64 / plan.rate,
                        u64::try_from(s.frames).unwrap_or(0),
                        plan.rate,
                        plan.profile.size,
                    )?,
                    None => Vec::new(),
                };
                self.segment(plan, s, overlays, tmp)
            })?;
            let el = t.elapsed();
            if made {
                report.rendered += 1;
                report.rendered_frames += s.frames;
                report.render += el;
            }
            self.event(
                &json!({"event": "segment", "i": i + 1, "n": n, "name": s.name, "key": s.key,
                        "frames": s.frames, "status": if made { "rendered" } else { "cached" },
                        "secs": el.as_secs_f64()}),
                || {
                    format!(
                        "[{}/{n}] {} {} ({}f){}",
                        i + 1,
                        if made { "rendered" } else { "cached  " },
                        s.name,
                        s.frames,
                        if made {
                            format!(" in {:.1} s", el.as_secs_f64())
                        } else {
                            String::new()
                        }
                    )
                },
            );
            files.push(path);
        }

        let audio = match &plan.audio {
            Some(a) => {
                let path = self
                    .dir("audio")?
                    .join(format!("{}-aac{}.m4a", a.key, plan.profile.audio_bitrate));
                let t = Instant::now();
                let mut level = None;
                if Self::atomic(&path, |tmp| {
                    level = Some(self.mix(plan, a, tmp)?);
                    Ok(())
                })? {
                    report.audio = Some(t.elapsed());
                }
                if let (Some((lufs, tp)), Some(el)) = (level, report.audio) {
                    self.event(
                        &json!({"event": "audio", "key": a.key, "clips": a.clips, "lufs": lufs,
                                "true_peak": tp, "secs": el.as_secs_f64()}),
                        || {
                            format!(
                                "mixed {} audio clips in {:.1} s: {lufs:.1} LUFS, {tp:.1} dBTP",
                                a.clips,
                                el.as_secs_f64()
                            )
                        },
                    );
                }
                Some(path)
            }
            None => None,
        };

        let t = Instant::now();
        self.mux(plan, &files, audio.as_deref(), output)?;
        report.mux = Some(t.elapsed());
        let manifest = json!({
            "master": master_key,
            "program": plan.program_key(),
            "audio": plan.audio.as_ref().map(|a| &a.key),
            "mix": plan.audio.as_ref().map(|a| &a.options),
            "profile": plan.profile.name,
            "segments": plan.segments.iter().map(|s| &s.key).collect::<Vec<_>>(),
        });
        std::fs::write(&manifest_path, format!("{manifest:#}\n"))
            .map_err(io(format!("writing {}", manifest_path.display())))?;
        report.total = t0.elapsed();
        self.event(
            &json!({"event": "done", "rendered": report.rendered, "segments": n,
                    "secs": report.total.as_secs_f64()}),
            || {
                format!(
                    "{}: {} of {n} segments rendered, {:.1} s",
                    output.display(),
                    report.rendered,
                    report.total.as_secs_f64()
                )
            },
        );
        Ok(report)
    }

    fn pipeline(&self, input: &Path, out: &Path, start: f64, frames: i64, rate: f64) -> Pipeline {
        Pipeline::new(input, out)
            .range(
                Duration::from_secs_f64(start.max(0.0)),
                Some(u64::try_from(frames).unwrap_or(0)),
            )
            .frame_rate(rational(rate))
            .device(self.device)
            .decode(self.decode)
    }

    /// Renders one segment to `out` with the profile's encoder: one GPU pass with the unit's
    /// main picture as the base and everything else (the incoming side of a transition,
    /// split-screen panels, overlay clips) as layers over it.
    #[allow(clippy::too_many_lines)]
    #[allow(
        clippy::many_single_char_names,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn segment(
        &self,
        plan: &Plan,
        seg: &Segment,
        overlays: Vec<Overlay>,
        out: &Path,
    ) -> Result<()> {
        let size = plan.profile.size;
        let (w, h) = (size.width, size.height);
        let frames = seg.frames;
        let key = &seg.key;
        let mut temps = Vec::new();
        let mut layers = Vec::new();
        let grain = |s: Option<&Shot>| -> Result<Option<Grain>> {
            let m = s
                .and_then(|s| s.grain.clone())
                .and_then(|g| grain_for(g, &plan.profile.name))
                .or_else(|| plan.grain.clone());
            m.map(|m| {
                ffcine_filters::effect::grain(&m).map_err(|msg| ExecError::Grade {
                    shot: seg.name.clone(),
                    msg: format!("grain: {msg}"),
                })
            })
            .transpose()
        };
        // The denoiser's history and lookahead around the main picture's piece.
        let mut ctx = (0, 0);
        let (base, start, mut grade) = match &seg.unit {
            Unit::Shot(s) => {
                ctx = s.context();
                let (input, start) = self.input_ctx(plan, s, ctx, key, "shot", &mut temps)?;
                let mut g = self.shot_grade(s, plan, size, None, None)?;
                g.grain = grain(Some(s))?;
                (input, start, g)
            }
            Unit::Gap => (
                self.black(plan, frames, key, &mut temps)?,
                0.0,
                identity(size),
            ),
            Unit::Transition { a, b, kind } => {
                let n = frames;
                let p = |i| comp::progress(i, n);
                match *kind {
                    TransitionKind::Dissolve => {
                        ctx = a.context();
                        let (input, start) = self.input_ctx(plan, a, ctx, key, "a", &mut temps)?;
                        let mut g = self.shot_grade(a, plan, size, None, None)?;
                        g.grain = grain(Some(a))?;
                        let mut bg = self.shot_grade(b, plan, size, None, None)?;
                        bg.grain = g.grain;
                        bg.hooks
                            .push(alpha_hook((0..n).map(|i| p(i) as f32).collect(), 0.0));
                        layers.push(self.video_layer(
                            plan,
                            b,
                            bg,
                            0,
                            n,
                            Vec::new(),
                            key,
                            "b",
                            &mut temps,
                        )?);
                        (input, start, g)
                    }
                    TransitionKind::Whip(d) => {
                        let e = |i: i64| comp::smoothstep(p(i));
                        let (fw, fh) = (f64::from(w), f64::from(h));
                        let travel = d[0].abs() * fw + d[1].abs() * fh;
                        let blur: Vec<f32> = (0..n)
                            .map(|i| {
                                let pp = p(i);
                                (travel * 6.0 * pp * (1.0 - pp) / n.max(1) as f64) as f32
                            })
                            .collect();
                        let rects = |sign: f64, off: f64| -> Vec<Crop> {
                            (0..n)
                                .map(|i| {
                                    let k = e(i) - off;
                                    Crop {
                                        x: (sign * d[0] * fw * k) as f32,
                                        y: (sign * d[1] * fh * k) as f32,
                                        width: w as f32,
                                        height: h as f32,
                                    }
                                })
                                .collect()
                        };
                        for (side, s, off) in [("a", a, 0.0), ("b", b, 1.0)] {
                            let mut g = self.shot_grade(s, plan, size, None, None)?;
                            g.grain = grain(Some(s))?;
                            g.hooks.push(Hook {
                                name: "whip".into(),
                                source: shaders::motion_blur().into(),
                                params: vec![("dir_x".into(), d[0]), ("dir_y".into(), d[1])],
                                tracks: vec![ParamTrack {
                                    name: "blur".into(),
                                    values: blur.clone(),
                                }],
                            });
                            layers.push(self.video_layer(
                                plan,
                                s,
                                g,
                                0,
                                n,
                                rects(1.0, off),
                                key,
                                side,
                                &mut temps,
                            )?);
                        }
                        let mut g = identity(size);
                        g.grain = grain(None)?;
                        (self.black(plan, frames, key, &mut temps)?, 0.0, g)
                    }
                    TransitionKind::Dip(_) | TransitionKind::Flash => {
                        // Dip and flash: `a` goes out to the colour over the first half, `b`
                        // comes in from it over the second.
                        let half = n / 2;
                        let (out_k, in_k): (Vec<f32>, Vec<f32>) = (0..n)
                            .map(|i| {
                                let q = p(i);
                                (
                                    ((1.0 - 2.0 * q).clamp(0.0, 1.0)) as f32,
                                    ((2.0 * q - 1.0).clamp(0.0, 1.0)) as f32,
                                )
                            })
                            .unzip();
                        let (colour, flash) = match *kind {
                            TransitionKind::Dip(c) => (c, false),
                            _ => ([1.0; 3], true),
                        };
                        let mix = |k: Vec<f32>| -> Mix {
                            if flash {
                                Mix::Lift(k.into_iter().map(|v| (1.0 - v).powi(2)).collect())
                            } else {
                                Mix::Fade(k, colour)
                            }
                        };
                        ctx = a.context();
                        let (input, start) = self.input_ctx(plan, a, ctx, key, "a", &mut temps)?;
                        let mut g = self.shot_grade(a, plan, size, Some(&mix(out_k)), None)?;
                        g.grain = grain(Some(a))?;
                        let b2 = b.shifted(half, plan.rate);
                        let mut bg = self.shot_grade(
                            &b2,
                            plan,
                            size,
                            Some(&mix(in_k[usize::try_from(half).unwrap_or(0)..].to_vec())),
                            None,
                        )?;
                        bg.grain = g.grain;
                        layers.push(self.video_layer(
                            plan,
                            &b2,
                            bg,
                            half,
                            n,
                            Vec::new(),
                            key,
                            "b",
                            &mut temps,
                        )?);
                        (input, start, g)
                    }
                }
            }
            Unit::Split(sp) => {
                let g_split = grain(None)?;
                self.split(plan, sp, frames, key, g_split, &mut layers, &mut temps)?
            }
            Unit::SplitDissolve {
                split,
                shot,
                split_out,
            } => {
                let g_split = grain(None)?;
                let (input, start, g) =
                    self.split(plan, split, frames, key, g_split, &mut layers, &mut temps)?;
                let mut sg = self.shot_grade(shot, plan, size, None, None)?;
                sg.grain = grain(Some(shot))?;
                let op = (0..frames)
                    .map(|i| {
                        let q = comp::progress(i, frames) as f32;
                        if *split_out { q } else { 1.0 - q }
                    })
                    .collect();
                sg.hooks.push(alpha_hook(op, 0.0));
                layers.push(self.video_layer(
                    plan,
                    shot,
                    sg,
                    0,
                    frames,
                    Vec::new(),
                    key,
                    "x",
                    &mut temps,
                )?);
                (input, start, g)
            }
        };
        // Overlay clips (picture-in-picture, stickers), bottom first.
        for (k, l) in seg.layers.iter().enumerate() {
            layers.push(self.overlay_layer(plan, l, key, k, &mut temps)?);
        }
        grade.size = Some(size);
        let pipe = self
            .pipeline(&base, out, start, frames, plan.rate)
            .context(
                u64::try_from(ctx.0).unwrap_or(0),
                u64::try_from(ctx.1).unwrap_or(0),
            )
            .grade(grade)
            .layers(layers)
            .overlays(overlays)
            .encoder(plan.profile.encoder.clone());
        let r = self.run_dv_retry(&pipe, key);
        for t in temps {
            let _ = std::fs::remove_file(t);
        }
        r
    }

    /// A split-screen piece: the backdrop as the base, the panels as layers. A panel's first
    /// shot also covers frames before the split (a transition into it) and its last holds after.
    #[allow(
        clippy::too_many_arguments,
        clippy::many_single_char_names,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn split(
        &self,
        plan: &Plan,
        sp: &Split,
        frames: i64,
        key: &Key,
        g_split: Option<Grain>,
        layers: &mut Vec<Layer>,
        temps: &mut Vec<PathBuf>,
    ) -> Result<(PathBuf, f64, Grade)> {
        let size = plan.profile.size;
        let (w, h) = (size.width, size.height);
        let lay = Layout::parse(&sp.layout);
        let rects = lay.rects(sp.panels.len(), w, h);
        let radius = lay.radius_px(h);
        for (i, (parts, r)) in sp.panels.iter().zip(&rects).enumerate() {
            for part in parts {
                let Some(shot) = &part.shot else { continue };
                let from = part.from.max(sp.at) - sp.at;
                let to = (part.from + part.frames).min(sp.at + frames) - sp.at;
                if to <= from {
                    continue;
                }
                let s = shot
                    .shifted(from + sp.at - part.from, plan.rate)
                    .with_frames(to - from);
                let (pw, ph) = (r.w.round().max(2.0) as u32, r.h.round().max(2.0) as u32);
                let pr: Vec<Crop> = (from..to)
                    .map(|j| {
                        let t = secs(sp.at + j, plan.rate);
                        let [ox, oy] = lay.enter_offset(i, t, w, h);
                        Crop {
                            x: (r.x + ox) as f32,
                            y: (r.y + oy) as f32,
                            width: r.w as f32,
                            height: r.h as f32,
                        }
                    })
                    .collect();
                let fin = sp
                    .finish
                    .tracks(sp.at + from, to - from, sp.total, plan.rate);
                let mut g = self.shot_grade(
                    &s,
                    plan,
                    Size {
                        width: pw,
                        height: ph,
                    },
                    Some(&Mix::Both(fin, sp.finish.fade_colour)),
                    Some(self.panel_crop(&s, pw, ph)?),
                )?;
                g.grain = g_split;
                if radius > 0.0 {
                    g.hooks.push(alpha_hook(vec![1.0], radius));
                }
                layers.push(self.video_layer(
                    plan,
                    &s,
                    g,
                    from,
                    to,
                    pr,
                    key,
                    &format!("p{i}"),
                    temps,
                )?);
            }
        }
        let mut g = identity(size);
        g.hooks.push(Hook {
            name: "backdrop".into(),
            source: shaders::background().into(),
            params: vec![
                ("bg_r".into(), 22.0 / 255.0),
                ("bg_g".into(), 20.0 / 255.0),
                ("bg_b".into(), 18.0 / 255.0),
                ("vignette".into(), 0.35),
            ],
            tracks: Vec::new(),
        });
        if let Some(hk) = finish_hook(&sp.finish, sp.at, frames, sp.total, plan.rate, None) {
            g.hooks.push(hk);
        }
        g.grain = g_split;
        Ok((self.black(plan, frames, key, temps)?, 0.0, g))
    }

    /// Runs `p`; if it fails, retries once with Dolby Vision RPUs stripped from every source
    /// file (a stream copy): the grade pass can't take them (iPhone HLG).
    fn run_dv_retry(&self, p: &Pipeline, key: &Key) -> Result<()> {
        let Err(e) = p.run(self.backend) else {
            return Ok(());
        };
        let tmp = self.dir("tmp")?;
        let mut stripped = Vec::new();
        let mut strip = |input: &Path, i: usize| -> Option<PathBuf> {
            if input.starts_with(&tmp) {
                return Some(input.to_owned());
            }
            let nodv = tmp.join(format!("{key}.{i}.nodv.mov"));
            let mut v = vec![OsString::from("-i"), input.into()];
            v.extend(args(&[
                "-map",
                "0:v:0",
                "-c:v",
                "copy",
                "-bsf:v",
                "dovi_rpu=strip=1",
            ]));
            v.push(nodv.clone().into());
            self.ffmpeg("strip Dolby Vision", &v).ok()?;
            stripped.push(nodv.clone());
            Some(nodv)
        };
        let mut q = p.clone();
        let mut ok = strip(&p.input, 0).map(|i| q.input = i).is_some();
        for (i, l) in q.layers.iter_mut().enumerate() {
            if let LayerSource::Video { input, .. } = &mut l.source {
                match strip(input, i + 1) {
                    Some(n) => *input = n,
                    None => ok = false,
                }
            }
        }
        let retry = ok && q.run(self.backend).is_ok();
        for f in stripped {
            let _ = std::fs::remove_file(f);
        }
        if retry { Ok(()) } else { Err(e.into()) }
    }

    /// Black, `frames` long, as a base for units without one (gaps, split-screens, whips).
    fn black(
        &self,
        plan: &Plan,
        frames: i64,
        key: &Key,
        temps: &mut Vec<PathBuf>,
    ) -> Result<PathBuf> {
        let black = self.dir("tmp")?.join(format!("{key}.black.mkv"));
        let r = rational(plan.rate);
        self.ffmpeg(
            "black",
            &args(&[
                "-f",
                "lavfi",
                "-i",
                // Tiny: the pass scales it to the frame (backdrops replace it).
                &format!("color=c=black:s=64x36:r={}/{}", r.num, r.den),
                "-frames:v",
                &frames.to_string(),
                "-pix_fmt",
                "yuv420p10le",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
                "-c:v",
                "ffv1",
            ])
            .chain([black.clone().into()])
            .collect::<Vec<_>>(),
        )?;
        temps.push(black.clone());
        Ok(black)
    }

    /// A shot's source for the GPU pass: its media at its start, or, with a speed change or a
    /// freeze, a lossless retimed intermediate (ffmpeg on NVDEC/NVENC) from 0.
    fn input(
        &self,
        plan: &Plan,
        s: &Shot,
        key: &Key,
        tag: &str,
        temps: &mut Vec<PathBuf>,
    ) -> Result<(PathBuf, f64)> {
        self.input_ctx(plan, s, (0, 0), key, tag, temps)
    }

    /// [`Renderer::input`] with `ctx` (before, after) frames of the clip around the piece for
    /// the denoiser ([`Pipeline::context`]): a retimed intermediate holds them too.
    #[allow(clippy::too_many_lines)]
    fn input_ctx(
        &self,
        plan: &Plan,
        s: &Shot,
        ctx: (i64, i64),
        key: &Key,
        tag: &str,
        temps: &mut Vec<PathBuf>,
    ) -> Result<(PathBuf, f64)> {
        if (s.speed - 1.0).abs() < 1e-9 && s.filters.ramp.is_none() {
            return Ok((s.media.clone(), s.start));
        }
        let r = rational(plan.rate);
        let fr = format!("{}/{}", r.num, r.den);
        let prep = self
            .dir("tmp")?
            .join(format!("{key}.{tag}.{}.retime.mkv", s.frames));
        let (before, after) = if s.speed.abs() < 1e-9 { (0, 0) } else { ctx };
        let total = before + s.frames + after;
        let n = total.to_string();
        let mut v = Vec::new();
        let filter = if let Some(ramp) = &s.filters.ramp {
            let from = s.clip_at.max(0);
            let cut = ramp_cut(ramp, s.clip_start, from - before, total, plan.rate);
            v.extend(args(&[
                "-hwaccel",
                "cuda",
                "-hwaccel_output_format",
                "cuda",
                "-copyts",
                "-start_at_zero",
                "-ss",
                &(s.clip_start + cut.seek).to_string(),
                "-t",
                &cut.duration.to_string(),
                "-i",
            ]));
            v.push(s.media.clone().into());
            format!("{},scale_cuda=format=p010le", cut.filter)
        } else if s.speed.abs() < 1e-9 {
            v.extend(args(&["-ss", &s.start.to_string(), "-i"]));
            v.push(s.media.clone().into());
            format!("trim=end_frame=1,loop=loop=-1:size=1:start=0,setpts=N/({fr})/TB,format=p010le")
        } else {
            let src = secs(before + s.frames + after + 2, plan.rate) * s.speed;
            // Frames stay in VRAM from NVDEC to the lossless NVENC intermediate: a timelapse
            // reads 20x its length of source.
            v.extend(args(&[
                "-hwaccel",
                "cuda",
                "-hwaccel_output_format",
                "cuda",
                "-ss",
                &s.shifted(-before, plan.rate).start.to_string(),
                "-t",
                &src.to_string(),
                "-i",
            ]));
            v.push(s.media.clone().into());
            format!(
                "setpts=(PTS-STARTPTS)/{},fps={fr},scale_cuda=format=p010le",
                s.speed
            )
        };
        v.extend(args(&[
            "-map",
            "0:v:0",
            "-vf",
            &filter,
            "-frames:v",
            &n,
            "-c:v",
            "hevc_nvenc",
            "-tune",
            "lossless",
        ]));
        v.push(prep.clone().into());
        self.ffmpeg("retime", &v)?;
        temps.push(prep.clone());
        Ok((prep, secs(before, plan.rate)))
    }

    /// A shot's grade at `size`: the grader's (LUT, look, sharpen), then its finish with a
    /// transition's `mix` folded in, and an optional source crop.
    fn shot_grade(
        &self,
        s: &Shot,
        plan: &Plan,
        size: Size,
        mix: Option<&Mix>,
        crops: Option<Vec<Crop>>,
    ) -> Result<Grade> {
        let base = match self.grader {
            Some(g) => g(s).map_err(|msg| ExecError::Grade {
                shot: s.name.clone(),
                msg,
            })?,
            None => Self::lut_grade(s)?,
        };
        let mut g = Grade {
            size: Some(size),
            denoise: s.filters.denoise,
            ..base
        };
        let rate = plan.rate;
        if let Some(c) = crops {
            g.crops = c;
        } else if size == plan.profile.size {
            self.geometry(plan, s, &mut g)?;
        }
        if let Some(h) = finish_hook(&s.finish, s.clip_at, s.frames, s.clip_frames, rate, mix) {
            g.hooks.push(h);
        }
        Ok(g)
    }

    /// A video layer of `s` over segment frames `from..to`.
    #[allow(clippy::too_many_arguments)]
    fn video_layer(
        &self,
        plan: &Plan,
        s: &Shot,
        grade: Grade,
        from: i64,
        to: i64,
        rects: Vec<Crop>,
        key: &Key,
        tag: &str,
        temps: &mut Vec<PathBuf>,
    ) -> Result<Layer> {
        let (input, start) = self.input(plan, s, key, tag, temps)?;
        Ok(Layer {
            source: LayerSource::Video {
                input,
                start: Duration::from_secs_f64(start.max(0.0)),
            },
            grade,
            from,
            to,
            rects,
        })
    }

    /// A panel's source crop: the picture covering a `w x h` slot around its reframe `focus`,
    /// pushed in per frame by `push [from, to]` (or a constant `zoom`).
    #[allow(
        clippy::many_single_char_names,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn panel_crop(&self, s: &Shot, w: u32, h: u32) -> Result<Vec<Crop>> {
        let (sw, sh) = self.dims(&s.media)?;
        let rf =
            s.fx.iter()
                .find(|v| v.get("effect").and_then(Value::as_str) == Some("ffcine.geo.reframe"))
                .and_then(|v| v.get("ffcine"))
                .cloned()
                .unwrap_or_default();
        let pair = |k: &str| {
            rf.get(k)
                .and_then(Value::as_array)
                .and_then(|a| match a.as_slice() {
                    [x, y] => Some([x.as_f64()?, y.as_f64()?]),
                    _ => None,
                })
        };
        let focus = pair("focus").unwrap_or([0.5, 0.5]);
        let push = pair("push");
        let zoom = rf.get("zoom").and_then(Value::as_f64).unwrap_or(1.0);
        let n = s.frames.max(1);
        Ok((0..n)
            .map(|i| {
                let z = push.map_or(zoom, |[z0, z1]| {
                    z0 + (z1 - z0) * (s.clip_at + i) as f64 / s.clip_frames.max(1) as f64
                });
                let center = pair("push_center")
                    .filter(|_| push.is_some())
                    .unwrap_or(focus);
                let c = comp::cover(
                    f64::from(sw),
                    f64::from(sh),
                    f64::from(w),
                    f64::from(h),
                    center,
                    z,
                );
                Crop {
                    x: c.x as f32,
                    y: c.y as f32,
                    width: c.w as f32,
                    height: c.h as f32,
                }
            })
            .collect())
    }

    /// An overlay clip's layer: placed and faded per frame from its transform and blend.
    #[allow(
        clippy::many_single_char_names,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn overlay_layer(
        &self,
        plan: &Plan,
        l: &SegLayer,
        key: &Key,
        k: usize,
        temps: &mut Vec<PathBuf>,
    ) -> Result<Layer> {
        let (w, h) = (plan.profile.size.width, plan.profile.size.height);
        let mut warns = Vec::new();
        let place = Placement::parse(&l.clip.transform, &l.clip.blend, &mut |m| warns.push(m));
        for m in warns {
            self.event(&json!({"event": "warning", "msg": m}), || {
                format!("warning: {}: {m}", l.clip.name)
            });
        }
        let (source, sw, sh, shot) = match &l.clip.source {
            OverlaySource::Image { path, .. } => {
                let img = image::open(path)
                    .map_err(|e| ExecError::Grade {
                        shot: l.clip.name.clone(),
                        msg: format!("{}: {e}", path.display()),
                    })?
                    .to_rgba8();
                let (iw, ih) = img.dimensions();
                (
                    LayerSource::Image(Arc::new(Image {
                        width: iw,
                        height: ih,
                        data: img.into_raw(),
                    })),
                    iw,
                    ih,
                    None,
                )
            }
            OverlaySource::Video(_) => {
                let s = l.shot(plan.rate).unwrap_or_else(|| unreachable!());
                let (iw, ih) = self.dims(&s.media)?;
                let (input, start) = self.input(plan, &s, key, &format!("ov{k}"), temps)?;
                (
                    LayerSource::Video {
                        input,
                        start: Duration::from_secs_f64(start.max(0.0)),
                    },
                    iw,
                    ih,
                    Some(s),
                )
            }
        };
        let n = l.to - l.from;
        let t = |j: i64| l.clip.source_start + secs(l.clip_at + j, plan.rate);
        let rects: Vec<comp::Rect> = (0..n)
            .map(|j| place.rect(t(j), f64::from(sw), f64::from(sh), w, h))
            .collect();
        let tw = rects.iter().map(|r| r.w).fold(1.0, f64::max).ceil();
        let th = rects.iter().map(|r| r.h).fold(1.0, f64::max).ceil();
        let size = Size {
            width: tw as u32,
            height: th as u32,
        };
        let opacity: Vec<f32> = (0..n).map(|j| place.opacity(t(j)) as f32).collect();
        let radius = place.radius * f64::from(h) * tw / rects.first().map_or(tw, |r| r.w.max(1.0));
        let mut grade = match &shot {
            Some(s) => self.shot_grade(s, plan, size, None, None)?,
            None => Grade {
                dither: None,
                peak_detect: false,
                size: Some(size),
                ..Grade::default()
            },
        };
        grade.dither = None;
        grade.hooks.push(alpha_hook(opacity, radius));
        Ok(Layer {
            source,
            grade,
            from: l.from,
            to: l.to,
            rects: rects
                .into_iter()
                .map(|r| Crop {
                    x: r.x as f32,
                    y: r.y as f32,
                    width: r.w as f32,
                    height: r.h as f32,
                })
                .collect(),
        })
    }

    /// A video file's picture size.
    fn dims(&self, path: &Path) -> Result<(u32, u32)> {
        let info = self.backend.probe(path)?;
        info.video()
            .map(|(_, v)| (v.width, v.height))
            .filter(|(w, h)| *w > 0 && *h > 0)
            .ok_or_else(|| ExecError::Grade {
                shot: path.display().to_string(),
                msg: "no video stream".into(),
            })
    }

    /// The LUT and sharpen of a shot's grade.
    fn lut_grade(s: &Shot) -> Result<Grade> {
        Ok(Grade {
            lut: s
                .lut
                .as_ref()
                .map(|p| {
                    Lut::from_cube_file(p).map(|lut| LutStage {
                        lut,
                        kind: LutType::Normalized,
                    })
                })
                .transpose()?,
            #[allow(clippy::cast_possible_truncation)]
            sharpen: s.grade.sharpen.unwrap_or(0.0) as f32,
            ..Grade::default()
        })
    }

    /// Reframe windows and the stabilizer warp of `s` into `grade` (the main picture, at the
    /// profile size).
    ///
    /// Windows come from the clip's reframe for the profile's aspect, evaluated at the piece's
    /// frames of the whole clip (so punch-ins and pushes run across cuts); without a reframe they
    /// only crop a source of another aspect to the output's. The stabilizer detects over the
    /// source frames the clip's [`Shot::span`] covers (one cached detect for every piece, also
    /// for retimed and ramped clips, whose output frames map to source frames through their
    /// speed), smooths, and keeps this piece's warps.
    #[allow(
        clippy::many_single_char_names,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn geometry(&self, plan: &Plan, s: &Shot, grade: &mut Grade) -> Result<()> {
        let stab = s.filters.stabilize.filter(|_| s.speed.abs() > 1e-9);
        let out = &plan.profile.aspect;
        let (w, h) = self.dims(&s.media)?;
        if s.filters.reframe.is_none()
            && stab.is_none()
            && (f64::from(w) / f64::from(h) - out.aspect()).abs() < 0.01
        {
            return Ok(());
        }
        let reframe = s.filters.reframe.clone().unwrap_or_default();
        let shot = ffcine_reframe::Shot {
            width: w,
            height: h,
            frames: s.clip_frames.max(s.clip_at + s.frames).max(1) as u64,
            rate: plan.rate,
            start: s.clip_start,
        };
        let windows: Vec<_> = (0..s.frames)
            .map(|i| reframe.frame((s.clip_at + i).max(0) as u64, &shot, out))
            .collect();
        ffcine_reframe::pass::apply(grade, &windows, &[], (w, h), out);
        let Some(params) = stab else {
            return Ok(());
        };
        let detect = self.detect.ok_or_else(|| ExecError::Stabilize {
            shot: s.name.clone(),
            msg: "needs the GPU motion detect (NVOF)".into(),
        })?;
        // Source seconds since the clip's first frame for clip frame `k` (timeline frames).
        let src = |k: i64| -> f64 {
            let t = secs(k, plan.rate);
            match &s.filters.ramp {
                Some(r) => r.source_time(t),
                None => t * s.speed,
            }
        };
        let (lo, hi) = s.span;
        let first = src(lo.max(0));
        let last = src(hi) + secs(1, plan.rate);
        let range = SourceRange {
            start: (s.clip_start + first).max(0.0),
            frames: ((last - first) * plan.rate).ceil().max(1.0) as usize,
            rate: rational(plan.rate),
        };
        let det = detect(&s.media, &range).map_err(|msg| ExecError::Stabilize {
            shot: s.name.clone(),
            msg,
        })?;
        let st = ffcine_stabilize::stabilize(&det, &params);
        let n = st.transforms.len();
        let transforms = (0..s.frames)
            .filter_map(|i| {
                let k = ((src((s.clip_at + i).max(0)) - first) * plan.rate).round();
                st.transforms
                    .get((k.max(0.0) as usize).min(n.saturating_sub(1)))
                    .copied()
            })
            .collect();
        grade.warp = Some(Warp { transforms });
        Ok(())
    }

    /// Mixes the audio (`ffcine_audio::render_audio`, the picture's length) and encodes it to
    /// AAC. Returns the master's loudness and true peak.
    fn mix(&self, plan: &Plan, a: &AudioPlan, out: &Path) -> Result<(f64, f64)> {
        let mix = ffcine_audio::render_audio(&a.timeline, &a.options)?;
        let wav = self.dir("tmp")?.join(format!("{}.wav", a.key));
        mix.master.write_wav(&wav)?;
        let mut v = vec![OsString::from("-i"), wav.clone().into()];
        v.extend(args(&[
            "-c:a",
            "aac",
            "-b:a",
            &plan.profile.audio_bitrate.to_string(),
        ]));
        v.push(out.into());
        let r = self.ffmpeg("audio encode", &v);
        let _ = std::fs::remove_file(wav);
        r.map(|()| (mix.lufs, mix.true_peak))
    }

    /// The segments joined by stream copy (concat demuxer) plus the audio, to `output`
    /// (atomic): one pass over the picture.
    fn mux(
        &self,
        plan: &Plan,
        files: &[PathBuf],
        audio: Option<&Path>,
        output: &Path,
    ) -> Result<()> {
        let ext = output.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let partial = output.with_extension(format!("partial.{ext}"));
        let list = output.with_extension("partial.txt");
        let body = files.iter().fold(String::new(), |mut s, f| {
            let _ = writeln!(
                s,
                "file '{}'",
                f.display().to_string().replace('\'', r"'\''")
            );
            s
        });
        std::fs::write(&list, body).map_err(io("writing concat list"))?;
        let mut v = args(&["-f", "concat", "-safe", "0", "-i"]).collect::<Vec<_>>();
        v.push(list.clone().into());
        if let Some(a) = audio {
            v.push("-i".into());
            v.push(a.into());
        }
        v.extend(args(&["-map", "0:v", "-c:v", "copy"]));
        if matches!(
            plan.profile.encoder.codec,
            Codec::HevcNvenc | Codec::HevcVulkan | Codec::HevcVaapi | Codec::Libx265
        ) {
            v.extend(args(&["-tag:v", "hvc1"]));
        }
        if audio.is_some() {
            v.extend(args(&["-map", "1:a", "-c:a", "copy"]));
        }
        v.extend(args(&["-movflags", "+faststart"]));
        v.push(partial.clone().into());
        let r = self.ffmpeg("mux", &v);
        let _ = std::fs::remove_file(list);
        r?;
        std::fs::rename(&partial, output).map_err(io(format!("writing {}", output.display())))
    }
}

/// A transition's contribution to a side's finish.
enum Mix {
    /// Per-frame fade towards the colour (1 = the picture).
    Fade(Vec<f32>, [f64; 3]),
    /// Per-frame lift towards white.
    Lift(Vec<f32>),
    /// A split-screen's own `(fade, lift)` tracks and fade colour.
    Both((Vec<f32>, Vec<f32>), [f64; 3]),
}

/// No grade but the output size (gaps, backdrops).
fn identity(size: Size) -> Grade {
    Grade {
        size: Some(size),
        ..Grade::default()
    }
}

/// [`shaders::alpha`] with per-frame `opacity` and a fixed corner `radius` in layer pixels.
fn alpha_hook(opacity: Vec<f32>, radius: f64) -> Hook {
    Hook {
        name: "alpha".into(),
        source: shaders::alpha().into(),
        params: vec![("radius".into(), radius)],
        tracks: vec![ParamTrack {
            name: "opacity".into(),
            values: opacity,
        }],
    }
}

/// [`shaders::finish`] for frames `at..at + n` of a clip `len` frames long, with a transition's
/// `mix` folded in; `None` when it would do nothing.
#[allow(clippy::many_single_char_names)]
fn finish_hook(
    f: &Finish,
    at: i64,
    n: i64,
    len: i64,
    rate: f64,
    mix: Option<&Mix>,
) -> Option<Hook> {
    if f.is_identity() && mix.is_none() {
        return None;
    }
    let (mut fade, mut lift) = f.tracks(at, n, len, rate);
    let mut colour = f.fade_colour;
    let mut fold = |k: &[f32], l: &[f32], c: Option<[f64; 3]>| {
        for (i, v) in k.iter().enumerate() {
            if let Some(x) = fade.get_mut(i) {
                *x = x.min(*v);
            }
        }
        for (i, v) in l.iter().enumerate() {
            if let Some(x) = lift.get_mut(i) {
                *x = x.max(*v);
            }
        }
        if let Some(c) = c {
            colour = c;
        }
    };
    match mix {
        Some(Mix::Fade(k, c)) => fold(k, &[], Some(*c)),
        Some(Mix::Both((k, l), c)) if k.iter().any(|v| *v < 1.0) => fold(k, l, Some(*c)),
        Some(Mix::Lift(l) | Mix::Both((_, l), _)) => fold(&[], l, None),
        None => {}
    }
    Some(Hook {
        name: "finish".into(),
        source: shaders::finish().into(),
        params: vec![
            ("darken".into(), f.darken),
            ("vignette".into(), f.vignette_angle()),
            ("fade_r".into(), colour[0]),
            ("fade_g".into(), colour[1]),
            ("fade_b".into(), colour[2]),
        ],
        tracks: vec![
            ParamTrack {
                name: "fade".into(),
                values: fade,
            },
            ParamTrack {
                name: "lift".into(),
                values: lift,
            },
        ],
    })
}

fn args<'a>(a: &'a [&'a str]) -> impl Iterator<Item = OsString> + 'a {
    a.iter().map(OsString::from)
}

/// `<output>.ffcine.json`: keys of what the master was made from.
pub fn manifest_path(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_owned();
    s.push(".ffcine.json");
    PathBuf::from(s)
}

fn read_manifest_key(path: &Path) -> Option<Key> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(Key(v.get("master")?.as_str()?.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Ramp;

    fn ramp() -> Ramp {
        Ramp {
            points: vec![(0.0, 1.0), (0.5, 3.0), (1.0, 1.0)],
            source_duration: 8.0,
        }
    }

    #[test]
    fn ramp_pieces_seek_to_their_own_source() {
        let rate = 24000.0 / 1001.0;
        let r = ramp();
        let first = ramp_cut(&r, 2.5, 0, 30, rate);
        assert!(first.seek.abs() < 1e-12);
        assert!((first.duration - (8.0 + 2.0 / rate)).abs() < 1e-9);
        assert!(first.filter.contains("(PTS*TB-2.5)"));
        assert!(
            first
                .filter
                .ends_with("fps=24000/1001,trim=start_pts=0:end_pts=30,setpts=PTS-STARTPTS")
        );
        let late = ramp_cut(&r, 2.5, 70, 30, rate);
        let t = r.source_time(70.0 / rate);
        assert!(late.seek > 1.0 && late.seek < t && t - late.seek <= 0.5 + 1e-9);
        assert!((late.duration - (8.0 - late.seek + 2.0 / rate)).abs() < 1e-9);
        assert!(late.filter.contains("trim=start_pts=70:end_pts=100,"));
    }

    fn ffmpeg(args: &[&str]) -> Vec<u8> {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(args)
            .output()
            .expect("ffmpeg on PATH");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// Per-frame hashes of a ramped piece, the way `input` cuts it (on the CPU).
    fn ramp_piece(src: &Path, from: i64, n: i64) -> Vec<String> {
        let rate = 24000.0 / 1001.0;
        let clip_start = 1.0;
        let c = ramp_cut(&ramp(), clip_start, from, n, rate);
        let out = ffmpeg(&[
            "-copyts",
            "-start_at_zero",
            "-ss",
            &(clip_start + c.seek).to_string(),
            "-t",
            &c.duration.to_string(),
            "-i",
            src.to_str().expect("utf-8"),
            "-vf",
            &c.filter,
            "-frames:v",
            &n.to_string(),
            "-f",
            "framemd5",
            "-",
        ]);
        String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.rsplit(',').next().unwrap_or("").trim().to_owned())
            .collect()
    }

    #[test]
    #[ignore = "needs ffmpeg"]
    fn ramp_pieces_match_the_whole_clip() {
        let d = std::env::temp_dir().join(format!("ffcine-ramp-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("temp dir");
        let src = d.join("src.mp4");
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=30000/1001",
            "-t",
            "10",
            "-g",
            "15",
            "-c:v",
            "libx264",
            "-qp",
            "0",
            src.to_str().expect("utf-8"),
        ]);
        let whole = ramp_piece(&src, 0, 100);
        assert_eq!(whole.len(), 100);
        let distinct: std::collections::HashSet<_> = whole.iter().collect();
        assert!(distinct.len() > 90, "the source's frames all differ");
        for (from, n) in [(0, 17), (17, 40), (57, 3), (60, 40)] {
            let piece = ramp_piece(&src, from, n);
            let (a, b) = (
                usize::try_from(from).unwrap(),
                usize::try_from(from + n).unwrap(),
            );
            assert_eq!(piece, whole[a..b], "piece {from}+{n}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rates() {
        assert_eq!(
            rational(24000.0 / 1001.0),
            Rational {
                num: 24000,
                den: 1001
            }
        );
        assert_eq!(
            rational(23.976_023_976_023_978),
            Rational {
                num: 24000,
                den: 1001
            }
        );
        assert_eq!(rational(25.0), Rational { num: 25, den: 1 });
        assert_eq!(
            rational(60000.0 / 1001.0),
            Rational {
                num: 60000,
                den: 1001
            }
        );
    }
}
