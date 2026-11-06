//! Lowering: every planned [`Unit`] becomes an [`ffcine_comp::Piece`] (a base, its grade, the
//! layers over it), the one thing [`Renderer`] renders. A shot is its picture graded on the
//! base; a dissolve, dip or flash puts the incoming side in a layer with an opacity or fade
//! track; a whip slides both sides as layers over black; a split-screen lays its panels out with
//! [`Layout::rects`] over a backdrop; overlay clips go on top. A [`Unit::Comp`] is already a
//! piece. Lowering runs the preps a piece needs (retimes, filter and intra preps) and
//! records the files it made in `temps`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ffcine_comp::{Base, Piece};
use ffcine_filters::Grain;
use ffcine_render::{
    BlendMode, Crop, Grade, Hook, Image, Layer, LayerSource, ParamTrack, Size, shaders,
};
use serde_json::json;

use crate::comp::{self, Layout, Placement, TransitionKind};
use crate::exec::{ExecError, Mix, Pic, Renderer, Result, alpha_hook, finish_hook, identity, secs};
use crate::plan::{OverlaySource, Plan, SegLayer, Segment, Shot, Split, Unit, grain_for};
use crate::recipe::Key;

/// A video base: `input` from `start` s.
fn video(input: PathBuf, start: f64) -> Base {
    Base::Video {
        input,
        start: Duration::from_secs_f64(start.max(0.0)),
    }
}

impl Renderer<'_> {
    /// Lowers `seg` to the piece its pass renders, and the denoiser's history and lookahead
    /// around its main picture (frames before and after).
    #[allow(clippy::too_many_lines)]
    #[allow(
        clippy::many_single_char_names,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    pub(crate) fn lower(
        &self,
        plan: &Plan,
        seg: &Segment,
        temps: &mut Vec<PathBuf>,
    ) -> Result<(Piece, (i64, i64))> {
        let size = plan.profile.size;
        let (w, h) = (size.width, size.height);
        let frames = seg.frames;
        let key = &seg.key;
        let mut overlays = Vec::new();
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
        let (base, mut grade) = match &seg.unit {
            Unit::Shot(s) => {
                ctx = s.context();
                let (input, start) = self.input_ctx(plan, s, ctx, key, "shot", temps)?;
                let mut g = self.shot_grade(s, plan, size, None, Pic::Main)?;
                g.grain = grain(Some(s))?;
                (video(input, start), g)
            }
            Unit::Gap => (Base::Black, identity(size)),
            Unit::Transition { a, b, kind } => {
                let n = frames;
                let p = |i| comp::progress(i, n);
                match *kind {
                    TransitionKind::Dissolve => {
                        ctx = a.context();
                        let (input, start) = self.input_ctx(plan, a, ctx, key, "a", temps)?;
                        let mut g = self.shot_grade(a, plan, size, None, Pic::Main)?;
                        g.grain = grain(Some(a))?;
                        let mut bg = self.shot_grade(b, plan, size, None, Pic::Main)?;
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
                            temps,
                        )?);
                        (video(input, start), g)
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
                            let mut g = self.shot_grade(s, plan, size, None, Pic::Main)?;
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
                                temps,
                            )?);
                        }
                        let mut g = identity(size);
                        g.grain = grain(None)?;
                        (Base::Black, g)
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
                        let (input, start) = self.input_ctx(plan, a, ctx, key, "a", temps)?;
                        let mut g = self.shot_grade(a, plan, size, Some(&mix(out_k)), Pic::Main)?;
                        g.grain = grain(Some(a))?;
                        let b2 = b.shifted(half, plan.rate);
                        let mut bg = self.shot_grade(
                            &b2,
                            plan,
                            size,
                            Some(&mix(in_k[usize::try_from(half).unwrap_or(0)..].to_vec())),
                            Pic::Main,
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
                            temps,
                        )?);
                        (video(input, start), g)
                    }
                }
            }
            Unit::Split(sp) => {
                let g_split = grain(None)?;
                let g = self.split(plan, sp, frames, key, g_split, &mut layers, temps)?;
                (Base::Black, g)
            }
            Unit::SplitDissolve {
                split,
                shot,
                split_out,
            } => {
                let g_split = grain(None)?;
                let g = self.split(plan, split, frames, key, g_split, &mut layers, temps)?;
                let mut sg = self.shot_grade(shot, plan, size, None, Pic::Main)?;
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
                    temps,
                )?);
                (Base::Black, g)
            }
            Unit::Comp(p) => {
                let mut ls = p.layers.clone();
                self.filter_preps(&mut ls)?;
                self.intra_preps(&mut ls)?;
                layers.extend(ls);
                overlays.extend(p.overlays.iter().cloned());
                (p.base.clone(), p.grade.clone())
            }
        };
        // Overlay clips (picture-in-picture, stickers), bottom first.
        for (k, l) in seg.layers.iter().enumerate() {
            layers.push(self.overlay_layer(plan, l, key, k, temps)?);
        }
        grade.size = Some(size);
        let piece = Piece {
            from: 0,
            frames,
            base,
            grade,
            shown: (0..layers.len()).collect(),
            layers,
            overlays,
            recipe: serde_json::Value::Null,
        };
        Ok((piece, ctx))
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
    ) -> Result<Grade> {
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
                    Pic::Panel,
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
                map: None,
            },
            grade,
            from,
            to,
            rects,
            rotation: Vec::new(),
            blend: BlendMode::Normal,
        })
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
                        map: None,
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
            Some(s) => self.shot_grade(s, plan, size, None, Pic::Overlay)?,
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
            rotation: (0..n).map(|j| place.rotation(t(j)) as f32).collect(),
            blend: place.mode,
        })
    }
}
