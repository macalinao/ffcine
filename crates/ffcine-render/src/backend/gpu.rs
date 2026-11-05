//! The GPU grade path: NVDEC -> Vulkan -> one libplacebo pass -> NVENC.
//!
//! One [`Gpu`] (CUDA + Vulkan devices, `pl_vulkan`, `pl_renderer`, on-disk `pl_cache`) is kept for
//! every shot a process renders, so device init and shader compilation are paid once.

use std::path::PathBuf;
use std::ptr;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use ffmpeg_next::ffi as ff;
use libplacebo_sys as pl;
use pl::libav::{pl_avframe_params, pl_map_avframe_ex, pl_unmap_avframe};

use super::av::{BufferRef, Frame, check, gpu_err};
use super::denoise::Denoiser;
use super::hw::{self, Devices};
use super::io::{self, Input, Output};
use super::placebo::{
    self, Cache, CompositeLayer, Compositor, CustomLut, Hooks, Log, OverlayTex, RenderParams,
    Renderer, RgbaTex, Vulkan,
};
use super::retime::{Q, Retimer};
use crate::pipeline::RunStats;
use crate::{
    ColorSpace, Decode, Device, Grade, HdrMetadata, Layer, LayerSource, LutStage, LutType, Overlay,
    Pipeline, Result, Stage,
};

/// Persistent GPU state. Field order is drop order: renderer and libplacebo before the cache
/// and the devices they use.
#[derive(Debug)]
pub(crate) struct Gpu {
    renderer: Renderer,
    vk: Vulkan,
    _cache: Option<Cache>,
    log: Log,
    devices: Devices,
    pub device: Device,
}

impl Gpu {
    pub(crate) fn new(device: Device, cache_dir: Option<PathBuf>) -> Result<Self> {
        let devices = Devices::new(device)?;
        let log = Log::new();
        // SAFETY: a Vulkan device context, kept alive in `devices` past `vk`.
        let vk = unsafe { Vulkan::import(&log, devices.vulkan.data())? };
        let cache = cache_dir.map(|d| Cache::new(&log, &d)).transpose()?;
        if let Some(c) = &cache {
            // SAFETY: valid gpu and cache; the cache outlives the gpu (field order).
            unsafe { pl::pl_gpu_set_cache(vk.gpu, c.cache) };
        }
        let renderer = Renderer::new(&log, &vk)?;
        Ok(Self {
            renderer,
            vk,
            _cache: cache,
            log,
            devices,
            device,
        })
    }

    /// Renders one pipeline (one shot).
    pub(crate) fn run(&mut self, p: &Pipeline) -> Result<RunStats> {
        let t0 = Instant::now();
        let mut grade = p
            .stages
            .iter()
            .map(|Stage::Grade(g)| g)
            .next()
            .cloned()
            .unwrap_or_default();
        grade.merge_luts();

        let dec_device = match p.decode {
            Decode::Cuda => &self.devices.cuda,
            _ => &self.devices.vulkan,
        };
        // History and lookahead for the temporal denoiser: decoded and denoised, not output.
        let ctx = if grade.denoise.is_some_and(|d| !d.is_off()) {
            p.context
        } else {
            crate::FrameContext::default()
        };
        // Generous: the seek only has to land before the frames kept as history.
        #[allow(clippy::cast_precision_loss)]
        let lead = Duration::from_secs_f64(
            (ctx.before + u64::from(ctx.before > 0)) as f64
                * p.frame_rate
                    .filter(|r| r.num > 0)
                    .map_or(0.1, |r| f64::from(r.den) / f64::from(r.num)),
        );
        let input = Input::open_with_lead(&p.input, p.start, lead, dec_device)?;
        let (in_w, in_h) = input.size();
        let chroma = input.decoder().chroma_sample_location;
        let (w, h) = grade.size.map_or((in_w, in_h), |s| (s.width, s.height));
        let rate = p.frame_rate.map(|r| Q::new(r.num.into(), r.den.into()));
        let out_rate = rate.unwrap_or_else(|| {
            let fr = input.decoder().framerate;
            if fr.num > 0 && fr.den > 0 {
                Q::new(fr.num.into(), fr.den.into())
            } else {
                Q::new(24000, 1001)
            }
        });

        let sw_out = io::av_pix_fmt(p.encoder.pixel_format);
        let vk_encode = p.encoder.codec.is_vulkan();
        if vk_encode && !self.devices.vulkan_encode {
            return Err(gpu_err(format!(
                "{} needs a Vulkan device that isn't derived from CUDA: use --vulkan N, not --cuda N",
                p.encoder.codec
            )));
        }
        // Render targets: handed to a Vulkan encoder as is, or copied into CUDA for NVENC.
        let vk_out = hw::frames(
            &self.devices.vulkan,
            sw_out,
            w,
            h,
            hw::VK_USAGE_RENDER,
            !vk_encode,
        )?;
        let enc_frames = if vk_encode {
            Some(vk_out.clone())
        } else if p.encoder.codec.is_hardware() {
            Some(hw::frames(&self.devices.cuda, sw_out, w, h, 0, false)?)
        } else {
            None
        };
        let output = Output::open(
            &p.output,
            &p.encoder,
            w,
            h,
            out_rate,
            chroma,
            enc_frames.as_ref(),
            grade.output_hdr.as_ref().filter(|_| grade.output.is_hdr()),
        )?;

        let pass = Pass::new(&self.vk, &grade, in_w, in_h)?;
        let frame_param = pass.params.hooks.param(ffcine_stabilize::hook::FRAME_PARAM);
        let mut denoiser = grade
            .denoise
            .filter(|d| !d.is_off())
            .map(|d| {
                Denoiser::new(
                    self.log.0,
                    self.vk.gpu,
                    d,
                    Some(cuda_ctx(&self.devices.cuda)),
                )
            })
            .transpose()?;
        let overlays = p
            .overlays
            .iter()
            .map(|o| Ok((o, OverlayTex::new(&self.vk, &o.mask)?)))
            .collect::<Result<Vec<_>>>()?;
        // Layers: each its own decoder (videos), renderer and RGBA target.
        let mut layer_inputs = Vec::new();
        let mut layers = self.layer_runs(&p.layers, dec_device, &mut layer_inputs)?;
        // Layers that are turned or blended (all of them on an HDR output) go through the
        // composite pass; the others are `pl_overlay`s on the grade pass.
        let hdr = grade.output.is_hdr();
        // One HDR colour space for the picture, the layers and the composite: the output's, with
        // the mastering display the output frames carry (`Grade::output_hdr`).
        let out_hdr = grade.output_hdr.as_ref().filter(|_| hdr);
        let colors = (
            layer_color(grade.output, out_hdr),
            still_color(grade.output),
        );
        let composites = |l: &Layer, at: i64| hdr || l.needs_composite(at);
        let mut composite = if p
            .layers
            .iter()
            .any(|l| (l.from..l.to).any(|at| composites(l, at)))
        {
            Some(Composite::new(&self.log, &self.vk, w, h, &grade)?)
        } else {
            None
        };
        let setup = t0.elapsed();

        let want = p.frames.map_or(u64::MAX, |n| n.saturating_add(ctx.after));
        let lead_frames = usize::try_from(ctx.before).unwrap_or(0);
        let vulkan = self.devices.vulkan.clone();
        // Three stages in flight: decode + upload N+1 | render N | download + encode N-1. Each
        // hands frames on with ffmpeg's/libplacebo's semaphores attached, so no stage waits on
        // the GPU for another; the bounded channels cap VRAM use.
        let (dec_tx, dec_rx) = sync_channel::<(Frame, bool)>(DEPTH);
        let (enc_tx, enc_rx) = sync_channel::<Frame>(DEPTH);
        let (dec, rendered, enc) = thread::scope(|sc| {
            let grade = &grade;
            let layer_dec: Vec<_> = layer_inputs
                .into_iter()
                .map(|(inp, n, tx, g)| {
                    let vk = vulkan.clone();
                    sc.spawn(move || decode_loop(inp, rate, n, &vk, g, &tx))
                })
                .collect();
            let dec = sc.spawn(move || {
                decode_lead(input, rate, lead_frames, want, &vulkan, grade, &dec_tx)
            });
            let enc = sc.spawn(move || encode_loop(output, &enc_rx, p.frames, t0));
            let mut t_render = Busy::new("render");
            let mut index = 0;
            let mut send = |gpu: &mut Self, src: Src<'_>| -> Result<bool> {
                if let Some(p) = frame_param {
                    // SAFETY: the parameter's live value, owned by the parsed hook in `params`;
                    // only this thread renders.
                    unsafe { (*p).i = i32::try_from(index).unwrap_or(i32::MAX) };
                }
                let at = i64::try_from(index).unwrap_or(i64::MAX);
                let mut shown = Vec::new();
                for (n, lr) in layers.iter_mut().enumerate() {
                    if let Some(rect) = lr.layer.rect_at(at, w, h) {
                        let local = usize::try_from(at - lr.layer.from).unwrap_or(0);
                        if !reusable(lr, local) {
                            t_render.time(|| gpu.render_layer(lr, local, colors))?;
                            lr.rendered = Some(local);
                        }
                        shown.push((n, &*lr, rect));
                    }
                }
                let items = order(&shown, &overlays, at);
                let first = items
                    .iter()
                    .position(|it| matches!(it, Item::Layer(lr, _) if composites(lr.layer, at)));
                let color = colors.0;
                let out = if let (Some(first), Some(c)) = (first, composite.as_mut()) {
                    let (below, steps, text) = split(&items, first, at, color);
                    t_render.time(|| {
                        gpu.render(
                            src,
                            index,
                            &vk_out,
                            grade,
                            &pass,
                            &below,
                            Some((c, &steps, &text)),
                        )
                    })?
                } else {
                    let ov = overlays_at(&items, at, color);
                    t_render.time(|| gpu.render(src, index, &vk_out, grade, &pass, &ov, None))?
                };
                index += 1;
                Ok(enc_tx.send(out).is_ok())
            };
            let rendered = (|| -> Result<()> {
                let Some(dn) = &mut denoiser else {
                    for (up, _) in dec_rx {
                        if !send(self, Src::Frame(&up))? {
                            break;
                        }
                    }
                    return Ok(());
                };
                // Context frames go through the denoiser in order but aren't rendered.
                let mut lead = std::collections::VecDeque::new();
                let mut left = p.frames.unwrap_or(u64::MAX);
                let mut rx = dec_rx.into_iter();
                loop {
                    let flush = match rx.next() {
                        Some((up, l)) => {
                            dn.push(&up)?;
                            lead.push_back(l);
                            false
                        }
                        None => true,
                    };
                    while let Some((props, image)) = dn.pop(flush)? {
                        if lead.pop_front().unwrap_or(false) {
                            continue;
                        }
                        if left == 0 || !send(self, Src::Image(&props, &image))? {
                            return Ok(());
                        }
                        left -= 1;
                    }
                    if flush {
                        return Ok(());
                    }
                }
            })();
            drop(send);
            // Layer decoders still sending (the main input ran short) stop when hung up on.
            drop(layers);
            drop(enc_tx);
            t_render.report();
            if let Some(dn) = &denoiser {
                dn.report();
            }
            let layer_dec: Result<()> = layer_dec.into_iter().try_for_each(join);
            (join(dec).and(layer_dec), rendered, join(enc))
        });
        dec?;
        rendered?;
        let (done, first_frame) = enc?;
        if done == 0 {
            return Err(gpu_err(format!(
                "{}: no frames in range",
                p.input.display()
            )));
        }
        Ok(RunStats {
            frames: done,
            setup,
            first_frame,
            wall: t0.elapsed(),
        })
    }

    /// Runs the grade pass on an uploaded (or denoised) frame `index` into a new output frame.
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        src: Src<'_>,
        index: usize,
        vk_out: &BufferRef,
        grade: &Grade,
        pass: &Pass,
        overlays: &Overlays,
        composite: Option<(&mut Composite, &[Step], &Overlays)>,
    ) -> Result<Frame> {
        let (luts, params) = (pass.luts(), &*pass.params);
        let up = match src {
            Src::Frame(f) | Src::Image(f, _) => f,
        };
        let mut out = Frame::hw(vk_out)?;
        // SAFETY: valid frames.
        unsafe {
            check(
                ff::av_frame_copy_props(out.as_ptr(), up.as_ptr()),
                "copy props",
            )?
        };
        // The source's Dolby Vision RPU and HDR metadata describe the source, not the output:
        // libplacebo would try to re-apply them to the target. An HDR output gets its own.
        for t in [
            ff::AVFrameSideDataType::AV_FRAME_DATA_DOVI_RPU_BUFFER,
            ff::AVFrameSideDataType::AV_FRAME_DATA_DOVI_METADATA,
            ff::AVFrameSideDataType::AV_FRAME_DATA_MASTERING_DISPLAY_METADATA,
            ff::AVFrameSideDataType::AV_FRAME_DATA_CONTENT_LIGHT_LEVEL,
            ff::AVFrameSideDataType::AV_FRAME_DATA_DYNAMIC_HDR_PLUS,
        ] {
            // SAFETY: a valid frame we own.
            unsafe { ff::av_frame_remove_side_data(out.as_ptr(), t) };
        }
        if let Some(m) = grade.output_hdr.as_ref().filter(|_| grade.output.is_hdr()) {
            io::attach_hdr(out.as_ptr(), m)?;
        }
        io::tag_frame(out.get_mut(), grade.output);
        out.get_mut().sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };

        set_tracks(params, grade, index)?;

        let gpu = self.vk.gpu;
        let mapped = matches!(src, Src::Frame(_));
        let mut image = match src {
            Src::Image(_, image) => *image,
            Src::Frame(_) => pl::pl_frame::default(),
        };
        let mut target = pl::pl_frame::default();
        // SAFETY: mapping valid Vulkan AVFrames on the device libplacebo imported; both are
        // unmapped (releasing the images back to ffmpeg with semaphores) before returning. A
        // denoised image reads the denoiser's own textures and isn't mapped.
        unsafe {
            if mapped && !pl_map_avframe_ex(gpu, &raw mut image, &avframe(up)) {
                return Err(gpu_err("pl_map_avframe_ex (source) failed"));
            }
            if !pl_map_avframe_ex(gpu, &raw mut target, &avframe(&out)) {
                if mapped {
                    pl_unmap_avframe(gpu, &raw mut image);
                }
                return Err(gpu_err("pl_map_avframe_ex (target) failed"));
            }
            let res = if let Some((c, above, text)) = composite {
                c.render(
                    &self.renderer,
                    &mut image,
                    &mut target,
                    (grade, &luts, index, params),
                    overlays,
                    above,
                    text,
                )
            } else {
                apply_grade(&mut image, &mut target, grade, &luts, index);
                overlays.set(&mut target);
                if pl::pl_render_image(
                    self.renderer.0,
                    &raw const image,
                    &raw const target,
                    &raw const params.params,
                ) {
                    Ok(())
                } else {
                    Err(gpu_err("pl_render_image failed"))
                }
            };
            pl_unmap_avframe(gpu, &raw mut target);
            if mapped {
                pl_unmap_avframe(gpu, &raw mut image);
            }
            res?;
        }
        Ok(out)
    }
}

impl Gpu {
    /// The runs for `layers` (nested scenes' too), each its own decoder (videos), renderer and
    /// RGBA target; video decoders go into `inputs`.
    fn layer_runs<'a>(
        &self,
        layers: &'a [Layer],
        dec_device: &BufferRef,
        inputs: &mut Vec<LayerInput<'a>>,
    ) -> Result<Vec<LayerRun<'a>>> {
        let mut runs = Vec::new();
        for l in layers {
            let (src, lw, lh) = match &l.source {
                LayerSource::Video { input, start } => {
                    let inp = Input::open(input, *start, dec_device)?;
                    let (lw, lh) = inp.size();
                    let (tx, rx) = sync_channel::<Frame>(DEPTH);
                    let want = u64::try_from(l.to - l.from).unwrap_or(0);
                    inputs.push((inp, want, tx, &l.grade));
                    (LayerSrc::Video { rx, last: None }, lw, lh)
                }
                LayerSource::Image(img) => (
                    LayerSrc::Image(RgbaTex::image(&self.vk, img)?),
                    img.width,
                    img.height,
                ),
                LayerSource::Still(path) => {
                    let (frame, tagged, (lw, lh)) =
                        decode_still(path, dec_device, &self.devices.vulkan, &l.grade)?;
                    (LayerSrc::Still { frame, tagged }, lw, lh)
                }
                LayerSource::Scene(sc) => {
                    let (sw, sh) = (sc.size.width, sc.size.height);
                    let grade = Grade {
                        dither: None,
                        peak_detect: false,
                        ..sc.grade.clone()
                    };
                    let run = SceneRun {
                        layers: self.layer_runs(&sc.layers, dec_device, inputs)?,
                        overlays: sc
                            .overlays
                            .iter()
                            .map(|o| Ok((o, OverlayTex::new(&self.vk, &o.mask)?)))
                            .collect::<Result<_>>()?,
                        pass: Pass::new(&self.vk, &grade, 2, 2)?,
                        bg: RgbaTex::image(
                            &self.vk,
                            &crate::Image {
                                width: 2,
                                height: 2,
                                data: [0, 0, 0, 255].repeat(4),
                            },
                        )?,
                        comp: Composite::new(&self.log, &self.vk, sw, sh, &grade)?,
                        renderer: Renderer::new(&self.log, &self.vk)?,
                        out: RgbaTex::target(&self.vk, sw, sh)?,
                        grade,
                    };
                    (LayerSrc::Scene(Box::new(run)), sw, sh)
                }
            };
            let (tw, th) = l.grade.size.map_or((lw, lh), |s| (s.width, s.height));
            runs.push(LayerRun {
                layer: l,
                pass: Pass::new(&self.vk, &l.grade, lw, lh)?,
                tex: RgbaTex::target(&self.vk, tw, th)?,
                src,
                renderer: Renderer::new(&self.log, &self.vk)?,
                rendered: None,
            });
        }
        Ok(runs)
    }

    /// Composites scene frame `local` into `sc.out`: the background, then every layer through
    /// the composite pass, text in z-order.
    fn render_scene(
        &mut self,
        sc: &mut SceneRun<'_>,
        local: usize,
        colors: &(pl::pl_color_space, pl::pl_color_space),
    ) -> Result<()> {
        let at = i64::try_from(local).unwrap_or(i64::MAX);
        let (w, h) = (sc.out.width, sc.out.height);
        let mut rects = Vec::new();
        for (n, lr) in sc.layers.iter_mut().enumerate() {
            if let Some(rect) = lr.layer.rect_at(at, w, h) {
                let l = usize::try_from(at - lr.layer.from).unwrap_or(0);
                if !reusable(lr, l) {
                    self.render_layer(lr, l, *colors)?;
                    lr.rendered = Some(l);
                }
                rects.push((n, rect));
            }
        }
        let shown: Vec<_> = rects.iter().map(|&(n, r)| (n, &sc.layers[n], r)).collect();
        let items = order(&shown, &sc.overlays, at);
        let first = items
            .iter()
            .position(|it| matches!(it, Item::Layer(..)))
            .unwrap_or(items.len());
        let color = colors.0;
        let (below, steps, text) = split(&items, first, at, color);
        set_tracks(&sc.pass.params, &sc.grade, local)?;
        let luts = sc.pass.luts();
        let mut image = sc.bg.frame_opaque(color);
        let mut target = sc.out.frame(color);
        // SAFETY: our own textures; the LUTs live for the call.
        unsafe {
            sc.comp.render(
                &sc.renderer,
                &mut image,
                &mut target,
                (&sc.grade, &luts, local, &sc.pass.params),
                &below,
                &steps,
                &text,
            )
        }
    }

    /// Renders layer frame `local` of `lr` into its RGBA target, in the colour space layers
    /// are composited in for an `output`.
    fn render_layer(
        &mut self,
        lr: &mut LayerRun<'_>,
        local: usize,
        (color, still): (pl::pl_color_space, pl::pl_color_space),
    ) -> Result<()> {
        match &mut lr.src {
            LayerSrc::Video { rx, last } => {
                if let Ok(f) = rx.recv() {
                    *last = Some(f);
                }
            }
            LayerSrc::Scene(sc) => self.render_scene(sc, local, &(color, still))?,
            LayerSrc::Image(_) | LayerSrc::Still { .. } => {}
        }
        let grade = &lr.layer.grade;
        let params = &*lr.pass.params;
        set_tracks(params, grade, local)?;
        let luts = lr.pass.luts();
        let mut target = lr.tex.frame(color);
        let gpu = self.vk.gpu;
        // SAFETY: a mapped Vulkan AVFrame (unmapped before returning) or our own texture, and
        // our own target texture, on the gpu libplacebo imported.
        unsafe {
            let (mut image, mapped) = match &lr.src {
                LayerSrc::Video { last: Some(f), .. } => {
                    let mut image = pl::pl_frame::default();
                    if !pl_map_avframe_ex(gpu, &raw mut image, &avframe(f)) {
                        return Err(gpu_err("pl_map_avframe_ex (layer) failed"));
                    }
                    (image, true)
                }
                LayerSrc::Video { last: None, .. } => {
                    return Err(gpu_err("layer video has no frames in range"));
                }
                LayerSrc::Image(t) => (t.frame(still), false),
                // Tagged stills are converted from their colour space; untagged ones are taken
                // as `still`, like solids and logos.
                LayerSrc::Still { frame, tagged } => {
                    let mut image = pl::pl_frame::default();
                    if !pl_map_avframe_ex(gpu, &raw mut image, &avframe(frame)) {
                        return Err(gpu_err("pl_map_avframe_ex (still) failed"));
                    }
                    if !*tagged {
                        image.color = still;
                    }
                    (image, true)
                }
                LayerSrc::Scene(sc) => (sc.out.frame(color), false),
            };
            apply_grade(&mut image, &mut target, grade, &luts, local);
            let ok = pl::pl_render_image(
                lr.renderer.0,
                &raw const image,
                &raw const target,
                &raw const params.params,
            );
            if mapped {
                pl_unmap_avframe(gpu, &raw mut image);
            }
            if !ok {
                return Err(gpu_err("pl_render_image (layer) failed"));
            }
        }
        Ok(())
    }
}

/// The colour space layers are rendered in and composited from for an `output`: BT.709 and
/// BT.1886 for SDR, the output's primaries with PQ or HLG for HDR, with the output's mastering
/// display (`hdr`) as the output frames carry it, so layers map to the same peak as the picture.
fn layer_color(output: ColorSpace, hdr: Option<&HdrMetadata>) -> pl::pl_color_space {
    use ffcine_core::color::{Primaries, Transfer};
    use pl::pl_color_primaries as P;
    use pl::pl_color_transfer as T;
    if !output.is_hdr() {
        return pl::pl_color_space {
            primaries: P::PL_COLOR_PRIM_BT_709,
            transfer: T::PL_COLOR_TRC_BT_1886,
            ..Default::default()
        };
    }
    pl::pl_color_space {
        primaries: match output.primaries {
            Primaries::Bt709 => P::PL_COLOR_PRIM_BT_709,
            Primaries::DisplayP3 => P::PL_COLOR_PRIM_DISPLAY_P3,
            Primaries::DciP3 => P::PL_COLOR_PRIM_DCI_P3,
            Primaries::Bt601 => P::PL_COLOR_PRIM_BT_601_525,
            _ => P::PL_COLOR_PRIM_BT_2020,
        },
        transfer: if output.transfer == Transfer::Hlg {
            T::PL_COLOR_TRC_HLG
        } else {
            T::PL_COLOR_TRC_PQ
        },
        hdr: hdr.map_or_else(Default::default, pl_hdr),
    }
}

/// `m` as libplacebo reads it from the output frame's side data.
#[allow(clippy::cast_possible_truncation)]
fn pl_hdr(m: &HdrMetadata) -> pl::pl_hdr_metadata {
    let xy = |[x, y]: [f64; 2]| pl::pl_cie_xy {
        x: x as f32,
        y: y as f32,
    };
    let [red, green, blue, white] = m.primaries;
    pl::pl_hdr_metadata {
        prim: pl::pl_raw_primaries {
            red: xy(red),
            green: xy(green),
            blue: xy(blue),
            white: xy(white),
        },
        min_luma: m.min_luminance as f32,
        max_luma: m.max_luminance as f32,
        #[allow(clippy::cast_precision_loss)]
        max_cll: m.max_cll as f32,
        #[allow(clippy::cast_precision_loss)]
        max_fall: m.max_fall as f32,
        ..Default::default()
    }
}

/// The colour space of a still layer's pixels: as the layers' for SDR (no conversion), sRGB on
/// an HDR output, which libplacebo puts at its SDR reference white (`PL_COLOR_SDR_WHITE`,
/// 203 nits as BT.2408), so logos aren't blown out or dim.
fn still_color(output: ColorSpace) -> pl::pl_color_space {
    if output.is_hdr() {
        // SAFETY: reading libplacebo's exported constant.
        unsafe { pl::pl_color_space_srgb }
    } else {
        layer_color(output, None)
    }
}

/// The composite pass's state: two output-sized RGBA textures (ping-pong), the dispatch, and
/// the renderer and params for the final pass to the output frame.
struct Composite {
    tex: [RgbaTex; 2],
    compositor: Compositor,
    renderer: Renderer,
    params: Box<RenderParams>,
    /// For the text steps: a plain copy (no dither).
    copy: Box<RenderParams>,
}

/// A composite pass step over the picture so far.
enum Step {
    Layer(CompositeLayer),
    /// Text between layers, drawn in a pass of its own.
    Text(Overlays),
}

impl Composite {
    fn new(log: &Log, vk: &Vulkan, w: u32, h: u32, grade: &Grade) -> Result<Self> {
        let out = Grade {
            dither: grade.dither,
            peak_detect: false,
            ..Grade::default()
        };
        Ok(Self {
            tex: [RgbaTex::target(vk, w, h)?, RgbaTex::target(vk, w, h)?],
            compositor: Compositor::new(log, vk)?,
            renderer: Renderer::new(log, vk)?,
            params: RenderParams::new(&out, Hooks::default())?,
            copy: RenderParams::new(
                &Grade {
                    dither: None,
                    peak_detect: false,
                    ..Grade::default()
                },
                Hooks::default(),
            )?,
        })
    }

    /// The grade pass into a texture in the target's colour space (with the `below` layers and
    /// text as overlays), each `above` step drawn over it in turn (a layer composited, or text
    /// in a copy pass), then the result and the `text` overlays onto `target`.
    ///
    /// # Safety
    /// As for the grade pass: mapped frames, LUTs alive for the call.
    #[allow(clippy::too_many_arguments)]
    unsafe fn render(
        &mut self,
        renderer: &Renderer,
        image: &mut pl::pl_frame,
        target: &mut pl::pl_frame,
        (grade, luts, index, params): (&Grade, &Luts<'_>, usize, &RenderParams),
        below: &Overlays,
        above: &[Step],
        text: &Overlays,
    ) -> Result<()> {
        let color = target.color;
        let mut cur = 0;
        let mut pic = self.tex[cur].frame_opaque(color);
        // SAFETY: the caller's frames and LUTs; our own textures.
        unsafe {
            apply_grade(image, &mut pic, grade, luts, index);
            below.set(&mut pic);
            if !pl::pl_render_image(renderer.0, image, &raw const pic, &raw const params.params) {
                return Err(gpu_err("pl_render_image (composite) failed"));
            }
        }
        self.compositor.reset_frame();
        for s in above {
            match s {
                Step::Layer(l) => {
                    self.compositor
                        .run(self.tex[cur].tex, l, self.tex[1 - cur].tex)?;
                }
                Step::Text(o) => {
                    let src = self.tex[cur].frame_opaque(color);
                    let mut dst = self.tex[1 - cur].frame_opaque(color);
                    o.set(&mut dst);
                    // SAFETY: our own textures; the overlays outlive the call.
                    let ok = unsafe {
                        pl::pl_render_image(
                            self.renderer.0,
                            &raw const src,
                            &raw const dst,
                            &raw const self.copy.params,
                        )
                    };
                    if !ok {
                        return Err(gpu_err("pl_render_image (composite text) failed"));
                    }
                }
            }
            cur = 1 - cur;
        }
        let pic = self.tex[cur].frame_opaque(color);
        text.set(target);
        // SAFETY: our texture into the caller's mapped target.
        let ok = unsafe {
            pl::pl_render_image(
                self.renderer.0,
                &raw const pic,
                target,
                &raw const self.params.params,
            )
        };
        if ok {
            Ok(())
        } else {
            Err(gpu_err("pl_render_image (composite output) failed"))
        }
    }
}

/// A shown layer as a composite pass input, `None` if it has no area.
fn composite_layer(lr: &LayerRun<'_>, r: crate::Crop, at: i64) -> Option<CompositeLayer> {
    if r.width < 1e-3 || r.height < 1e-3 {
        return None;
    }
    let (sin, cos) = lr.layer.rotation_at(at).to_radians().sin_cos();
    Some(CompositeLayer {
        tex: lr.tex.tex,
        centre: [r.x + r.width / 2.0, r.y + r.height / 2.0],
        half: [r.width / 2.0, r.height / 2.0],
        rot: [cos, sin],
        mode: lr.layer.blend.index(),
    })
}

/// A grade's render state: its LUTs and render params (hooks parsed).
struct Pass {
    lut: Option<(Box<CustomLut>, LutType)>,
    input_lut: Option<(Box<CustomLut>, LutType)>,
    params: Box<RenderParams>,
}

/// The grade's LUTs with `rgb_gain` folded into the first one applied (an identity one without):
/// `(input, grade)`.
type LutPair = (Option<LutStage>, Option<LutStage>);
fn gained_luts(grade: &Grade) -> Result<LutPair> {
    let (mut input, mut lut) = (grade.input_lut.clone(), grade.lut.clone());
    let Some(g) = grade
        .rgb_gain
        .filter(|g| g.iter().any(|c| (c - 1.0).abs() > 1e-6))
    else {
        return Ok((input, lut));
    };
    let first = match (&mut input, &mut lut) {
        (Some(s), _) | (None, Some(s)) => s,
        (None, None) => lut.insert(LutStage {
            lut: crate::Lut::identity(33),
            kind: LutType::Normalized,
        }),
    };
    if first.kind != LutType::Normalized {
        return Err(gpu_err("rgb_gain needs a normalized first LUT"));
    }
    first.lut = first.lut.with_input_gain(g);
    Ok((input, lut))
}

impl Pass {
    /// For `grade` on a `width x height` source.
    fn new(vk: &Vulkan, grade: &Grade, width: u32, height: u32) -> Result<Self> {
        let (input_lut, lut) = gained_luts(grade)?;
        let up = |l: Option<LutStage>| -> Result<Option<(Box<CustomLut>, LutType)>> {
            l.map(|l| Ok((CustomLut::new(&l.lut)?, l.kind))).transpose()
        };
        let (input_lut, lut) = (up(input_lut)?, up(lut)?);
        let hooks = Hooks::parse(vk, &hook_sources(grade, width, height)?)?;
        // Static //!PARAMs, and frame 0 of the tracks so unknown names fail before decoding.
        for (k, h) in grade.hooks.iter().enumerate() {
            let i = first_inline_hook(grade) + k;
            for (name, v) in &h.params {
                hooks.set(i, name, *v)?;
            }
            for t in &h.tracks {
                if let Some(v) = t.at(0) {
                    hooks.set(i, &t.name, f64::from(v))?;
                }
            }
        }
        Ok(Self {
            lut,
            input_lut,
            params: RenderParams::new(grade, hooks)?,
        })
    }

    fn luts(&self) -> Luts<'_> {
        Luts {
            input: self.input_lut.as_ref().map(|(l, k)| (&**l, *k)),
            grade: self.lut.as_ref().map(|(l, k)| (&**l, *k)),
        }
    }
}

/// Sets the grade's per-frame hook params for frame `index`.
fn set_tracks(params: &RenderParams, grade: &Grade, index: usize) -> Result<()> {
    let first = first_inline_hook(grade);
    for (k, h) in grade.hooks.iter().enumerate() {
        for t in &h.tracks {
            if let Some(v) = t.at(index) {
                params.hooks.set(first + k, &t.name, f64::from(v))?;
            }
        }
    }
    Ok(())
}

/// The grade's LUTs, source crop and placement on a mapped `image` -> `target` pair.
///
/// # Safety
/// The LUTs must outlive the render call.
unsafe fn apply_grade(
    image: &mut pl::pl_frame,
    target: &mut pl::pl_frame,
    grade: &Grade,
    luts: &Luts<'_>,
    index: usize,
) {
    // The camera LUT on the source; the grade LUT on the source too, or, after a camera
    // LUT, on the target (applied to the converted picture before encoding). Normalized pairs
    // arrive baked into one (`Grade::merge_luts`).
    if let Some((l, kind)) = luts.input {
        image.lut = &raw const l.lut;
        image.lut_type = placebo::lut_type(kind);
        if let Some((g, gk)) = luts.grade {
            target.lut = &raw const g.lut;
            target.lut_type = placebo::lut_type(gk);
        }
    } else if let Some((l, kind)) = luts.grade {
        image.lut = &raw const l.lut;
        image.lut_type = placebo::lut_type(kind);
    }
    // With a warp the crops are folded into its matrices (the hook only sees the crop).
    if let Some(c) = grade.crop_at(index).filter(|_| grade.warp.is_none()) {
        image.crop = rect(c);
    }
    if let Some(c) = grade.place {
        target.crop = rect(c);
    }
}

/// A layer being rendered.
struct LayerRun<'a> {
    layer: &'a Layer,
    pass: Pass,
    tex: RgbaTex,
    src: LayerSrc<'a>,
    renderer: Renderer,
    /// The layer frame its texture holds.
    rendered: Option<usize>,
}

/// A layer whose source and pass inputs are unchanged since frame `rendered` keeps its texture.
fn reusable(lr: &LayerRun<'_>, local: usize) -> bool {
    matches!(lr.src, LayerSrc::Image(_) | LayerSrc::Still { .. })
        && lr
            .rendered
            .is_some_and(|prev| lr.layer.grade.same_frame(prev, local))
}

enum LayerSrc<'a> {
    Video {
        rx: Receiver<Frame>,
        /// Held past the source's end.
        last: Option<Frame>,
    },
    Image(RgbaTex),
    /// A still decoded once and uploaded; `tagged` if it carries colour tags.
    Still {
        frame: Frame,
        tagged: bool,
    },
    Scene(Box<SceneRun<'a>>),
}

/// A nested composition: its layers and text composited over its background into `out`.
struct SceneRun<'a> {
    layers: Vec<LayerRun<'a>>,
    overlays: Vec<(&'a Overlay, OverlayTex)>,
    /// The scene's background pass (dither off) over `bg`, an opaque black texture.
    grade: Grade,
    pass: Pass,
    bg: RgbaTex,
    comp: Composite,
    renderer: Renderer,
    out: RgbaTex,
}

/// A layer video's decoder: input, frames wanted, where they go, its grade.
type LayerInput<'a> = (Input, u64, SyncSender<Frame>, &'a Grade);

/// A layer or a text overlay, in drawing order.
#[derive(Clone, Copy)]
enum Item<'b> {
    Layer(&'b LayerRun<'b>, crate::Crop),
    Text(&'b Overlay, &'b OverlayTex),
}

/// The `shown` layers (pipeline index, run, placement) bottom first, with each text overlay
/// visible at `at` under the first shown layer it is under (on top if none).
fn order<'b>(
    shown: &[(usize, &'b LayerRun<'b>, crate::Crop)],
    text: &'b [(&'b Overlay, OverlayTex)],
    at: i64,
) -> Vec<Item<'b>> {
    let slot = |o: &Overlay| {
        o.under.map_or(shown.len(), |u| {
            shown
                .iter()
                .position(|(n, ..)| *n >= u)
                .unwrap_or(shown.len())
        })
    };
    let mut items = Vec::new();
    for j in 0..=shown.len() {
        items.extend(
            text.iter()
                .filter(|(o, _)| o.opacity(at) > 0.0 && slot(o) == j)
                .map(|(o, t)| Item::Text(o, t)),
        );
        if let Some((_, lr, r)) = shown.get(j) {
            items.push(Item::Layer(lr, *r));
        }
    }
    items
}

/// `items` split for the composite pass from item `first` (a composited layer) on: the
/// overlays on the grade pass, the steps after it, and the text on top.
fn split(
    items: &[Item<'_>],
    first: usize,
    at: i64,
    color: pl::pl_color_space,
) -> (Overlays, Vec<Step>, Overlays) {
    let below = overlays_at(&items[..first], at, color);
    let mut steps = Vec::new();
    let mut run = Vec::new();
    for it in &items[first..] {
        match it {
            Item::Layer(lr, r) => {
                if !run.is_empty() {
                    steps.push(Step::Text(overlays_at(&run, at, color)));
                    run.clear();
                }
                if let Some(c) = composite_layer(lr, *r, at) {
                    steps.push(Step::Layer(c));
                }
            }
            Item::Text(..) => run.push(*it),
        }
    }
    (below, steps, overlays_at(&run, at, color))
}

/// Decodes a still once and uploads it: software pictures go through swscale to RGBA64 first
/// (the frame's matrix and range; BT.601 when untagged, as JPEG). Returns the frame, whether it
/// carries colour tags (primaries or transfer; ICC profiles are dropped), and its size.
fn decode_still(
    path: &std::path::Path,
    device: &BufferRef,
    vulkan: &BufferRef,
    grade: &Grade,
) -> Result<(Frame, bool, (u32, u32))> {
    let mut inp = Input::open(path, Duration::ZERO, device)?;
    let size = inp.size();
    let mut f = inp
        .next_frame()?
        .ok_or_else(|| gpu_err(format!("{}: no picture", path.display())))?;
    let s = f.get();
    let tagged = s.color_primaries != ff::AVColorPrimaries::AVCOL_PRI_UNSPECIFIED
        || s.color_trc != ff::AVColorTransferCharacteristic::AVCOL_TRC_UNSPECIFIED;
    // SAFETY: a frame we own.
    unsafe {
        ff::av_frame_remove_side_data(
            f.as_ptr(),
            ff::AVFrameSideDataType::AV_FRAME_DATA_ICC_PROFILE,
        );
    }
    if f.get().hw_frames_ctx.is_null() {
        f = to_rgba64(&f)?;
    }
    Ok((upload(&f, &mut None, vulkan, grade)?, tagged, size))
}

/// A software frame converted to full-range RGBA64 (props and colour tags kept).
fn to_rgba64(src: &Frame) -> Result<Frame> {
    let s = src.get();
    let mut dst = Frame::new()?;
    {
        let d = dst.get_mut();
        d.width = s.width;
        d.height = s.height;
        d.format = ff::AVPixelFormat::AV_PIX_FMT_RGBA64LE as i32;
    }
    // SAFETY: valid frames; the context is freed before returning.
    unsafe {
        let fmt = std::mem::transmute::<i32, ff::AVPixelFormat>(s.format);
        let ctx = ff::sws_getContext(
            s.width,
            s.height,
            fmt,
            s.width,
            s.height,
            ff::AVPixelFormat::AV_PIX_FMT_RGBA64LE,
            (ff::SwsFlags::SWS_BICUBIC as i32)
                | (ff::SwsFlags::SWS_ACCURATE_RND as i32)
                | (ff::SwsFlags::SWS_FULL_CHR_H_INT as i32),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if ctx.is_null() {
            return Err(gpu_err("still: no swscale conversion to RGBA"));
        }
        let matrix = match s.colorspace {
            ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED | ff::AVColorSpace::AVCOL_SPC_RGB => {
                ff::AVColorSpace::AVCOL_SPC_SMPTE170M
            }
            m => m,
        } as i32;
        let jpeg = matches!(
            fmt,
            ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P
                | ff::AVPixelFormat::AV_PIX_FMT_YUVJ422P
                | ff::AVPixelFormat::AV_PIX_FMT_YUVJ444P
                | ff::AVPixelFormat::AV_PIX_FMT_YUVJ440P
                | ff::AVPixelFormat::AV_PIX_FMT_YUVJ411P
        );
        let full = i32::from(jpeg || s.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG);
        let coefs = ff::sws_getCoefficients(matrix);
        ff::sws_setColorspaceDetails(ctx, coefs, full, coefs, 1, 0, 1 << 16, 1 << 16);
        let r = ff::sws_scale_frame(ctx, dst.as_ptr(), src.as_ptr());
        ff::sws_freeContext(ctx);
        check(r, "convert still")?;
        check(
            ff::av_frame_copy_props(dst.as_ptr(), src.as_ptr()),
            "copy props",
        )?;
    }
    let d = dst.get_mut();
    d.colorspace = ff::AVColorSpace::AVCOL_SPC_RGB;
    d.color_range = ff::AVColorRange::AVCOL_RANGE_JPEG;
    Ok(dst)
}

/// ffmpeg's CUDA context (the device the Vulkan one is paired with).
fn cuda_ctx(cuda: &BufferRef) -> ffcine_stabilize::cuda::CuContext {
    // SAFETY: a CUDA device context's `hwctx` is an `AVCUDADeviceContext`.
    unsafe {
        let dev = cuda.data::<ff::AVHWDeviceContext>();
        (*(*dev).hwctx.cast::<ff::AVCUDADeviceContext>())
            .cuda_ctx
            .cast()
    }
}

/// The grade's uploaded LUTs.
struct Luts<'a> {
    input: Option<(&'a CustomLut, LutType)>,
    grade: Option<(&'a CustomLut, LutType)>,
}

/// The render's source: a decoded frame, or a denoised image with its frame's props.
#[derive(Clone, Copy)]
enum Src<'a> {
    Frame(&'a Frame),
    Image(&'a Frame, &'a pl::pl_frame),
}

/// The `pl_overlay`s visible on one frame; `list` points into `parts`.
struct Overlays {
    list: Vec<pl::pl_overlay>,
    _parts: Vec<Vec<pl::pl_overlay_part>>,
}

impl Overlays {
    /// Draws them on `target` (which must not outlive `self`).
    fn set(&self, target: &mut pl::pl_frame) {
        if !self.list.is_empty() {
            target.overlays = self.list.as_ptr();
            target.num_overlays = i32::try_from(self.list.len()).unwrap_or(i32::MAX);
        }
    }
}

/// `items` in order: rendered layers (in `color`) at their placements, and the text overlays
/// visible on frame `index` with their fade applied to the part colours.
fn overlays_at(items: &[Item<'_>], index: i64, color: pl::pl_color_space) -> Overlays {
    // SAFETY: reading libplacebo's exported constants.
    let (srgb_repr, srgb) = unsafe { (pl::pl_color_repr_rgb, pl::pl_color_space_srgb) };
    let mut parts = Vec::new();
    let mut list = Vec::new();
    for it in items {
        match it {
            Item::Layer(lr, r) => {
                #[allow(clippy::cast_precision_loss)]
                let (tw, th) = (lr.tex.width as f32, lr.tex.height as f32);
                parts.push(vec![pl::pl_overlay_part {
                    src: pl::pl_rect2df {
                        x0: 0.0,
                        y0: 0.0,
                        x1: tw,
                        y1: th,
                    },
                    dst: rect(*r),
                    color: [1.0; 4],
                }]);
                let f = lr.tex.frame(color);
                list.push((
                    lr.tex.tex,
                    pl::pl_overlay_mode::PL_OVERLAY_NORMAL,
                    f.repr,
                    f.color,
                ));
            }
            Item::Text(o, tex) => {
                let fade = o.opacity(index);
                if fade <= 0.0 {
                    continue;
                }
                let p: Vec<pl::pl_overlay_part> = o
                    .parts_at(index)
                    .iter()
                    .map(|part| {
                        let ([sx0, sy0, sx1, sy1], [dx0, dy0, dx1, dy1]) = part.rects(&o.mask);
                        let [red, green, blue, alpha] = part.colour;
                        pl::pl_overlay_part {
                            src: pl::pl_rect2df {
                                x0: sx0,
                                y0: sy0,
                                x1: sx1,
                                y1: sy1,
                            },
                            dst: pl::pl_rect2df {
                                x0: dx0,
                                y0: dy0,
                                x1: dx1,
                                y1: dy1,
                            },
                            color: [red, green, blue, alpha * fade],
                        }
                    })
                    .collect();
                if p.is_empty() {
                    continue;
                }
                parts.push(p);
                list.push((
                    tex.tex,
                    pl::pl_overlay_mode::PL_OVERLAY_MONOCHROME,
                    srgb_repr,
                    srgb,
                ));
            }
        }
    }
    let list = list
        .into_iter()
        .zip(&parts)
        .map(|((tex, mode, repr, color), p)| pl::pl_overlay {
            tex,
            mode,
            coords: pl::pl_overlay_coords::PL_OVERLAY_COORDS_DST_FRAME,
            repr,
            color,
            parts: p.as_ptr(),
            num_parts: i32::try_from(p.len()).unwrap_or(i32::MAX),
        })
        .collect();
    Overlays {
        list,
        _parts: parts,
    }
}

/// Frames in flight between stages.
const DEPTH: usize = 3;

fn join<T>(h: thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    h.join()
        .unwrap_or_else(|_| Err(gpu_err("pipeline thread panicked")))
}

/// Decode, select frames (seek trim, fps), upload to Vulkan. Stops quietly if the renderer hangs up.
fn decode_loop(
    input: Input,
    rate: Option<Q>,
    want: u64,
    vulkan: &BufferRef,
    grade: &Grade,
    tx: &SyncSender<Frame>,
) -> Result<()> {
    decode_frames(input, rate, 0, want, vulkan, grade, tx, |f, _| f)
}

/// [`decode_loop`], first sending up to `lead` frames from before the seek point, flagged
/// `true` (not counted in `want`).
fn decode_lead(
    input: Input,
    rate: Option<Q>,
    lead: usize,
    want: u64,
    vulkan: &BufferRef,
    grade: &Grade,
    tx: &SyncSender<(Frame, bool)>,
) -> Result<()> {
    decode_frames(input, rate, lead, want, vulkan, grade, tx, |f, l| (f, l))
}

#[allow(clippy::too_many_arguments)]
fn decode_frames<T>(
    mut input: Input,
    rate: Option<Q>,
    lead: usize,
    want: u64,
    vulkan: &BufferRef,
    grade: &Grade,
    tx: &SyncSender<T>,
    wrap: fn(Frame, bool) -> T,
) -> Result<()> {
    let mut retimer = Retimer::new(input.tb, input.trim, rate).lead(lead);
    let mut out = 0usize;
    let mut vk_in: Option<BufferRef> = None;
    let mut ready = Vec::new();
    let mut sent = 0u64;
    let mut eof = false;
    let (mut t_dec, mut t_up) = (Busy::new("decode"), Busy::new("upload"));
    while sent < want && !eof {
        let next = t_dec.time(|| input.next_frame())?;
        if let Some(f) = next {
            let pts = f.get().pts;
            retimer.push(pts, f, &mut ready);
        } else {
            retimer.finish(&mut ready);
            eof = true;
        }
        for f in ready.drain(..) {
            let lead = out < retimer.led();
            if !lead && sent == want {
                break;
            }
            if out == 0 && f.get().hw_frames_ctx.is_null() {
                // ffmpeg falls back to software decode quietly when the hardware decoder rejects
                // a format (4:2:2 HEVC on NVDEC and Vulkan video as of driver 615).
                // SAFETY: software frames carry their pixel format in `format`; the name is a
                // static string or NULL.
                let fmt = unsafe {
                    let p = ff::av_get_pix_fmt_name(std::mem::transmute::<i32, ff::AVPixelFormat>(
                        f.get().format,
                    ));
                    if p.is_null() {
                        "?".into()
                    } else {
                        std::ffi::CStr::from_ptr(p).to_string_lossy()
                    }
                };
                eprintln!(
                    "decode: software ({fmt}; the hardware decoder doesn't take it), uploaded per frame"
                );
            }
            let up = t_up.time(|| upload(&f, &mut vk_in, vulkan, grade))?;
            if tx.send(wrap(up, lead)).is_err() {
                return Ok(());
            }
            out += 1;
            sent += u64::from(!lead);
        }
    }
    t_dec.report();
    t_up.report();
    Ok(())
}

/// A decoded frame as a Vulkan frame for the renderer: Vulkan decoder output as is (zero copy),
/// CUDA or software frames through one copy into a new Vulkan frame (hwupload).
fn upload(
    src: &Frame,
    vk_in: &mut Option<BufferRef>,
    vulkan: &BufferRef,
    grade: &Grade,
) -> Result<Frame> {
    let s = src.get();
    if s.format == ff::AVPixelFormat::AV_PIX_FMT_VULKAN as i32 {
        let mut f = src.clone();
        override_color(f.get_mut(), grade);
        return Ok(f);
    }
    let sw = if s.hw_frames_ctx.is_null() {
        // SAFETY: software frames carry their pixel format in `format`.
        unsafe { std::mem::transmute::<i32, ff::AVPixelFormat>(s.format) }
    } else {
        // SAFETY: hw frames carry their frames context.
        unsafe { (*(*s.hw_frames_ctx).data.cast::<ff::AVHWFramesContext>()).sw_format }
    };
    if vk_in.as_ref().is_none_or(|f| {
        // SAFETY: valid frames context.
        let fc = unsafe { &*f.data::<ff::AVHWFramesContext>() };
        fc.sw_format != sw || fc.width != s.width || fc.height != s.height
    }) {
        let w = u32::try_from(s.width).unwrap_or(0);
        let h = u32::try_from(s.height).unwrap_or(0);
        *vk_in = Some(hw::frames(vulkan, sw, w, h, 0, true)?);
    }
    let frames_in = vk_in.as_ref().ok_or_else(|| gpu_err("no Vulkan frames"))?;
    let mut up = Frame::hw(frames_in)?;
    // SAFETY: valid frames.
    unsafe {
        check(
            ff::av_hwframe_transfer_data(up.as_ptr(), src.as_ptr(), 0),
            "upload to Vulkan",
        )?;
        check(
            ff::av_frame_copy_props(up.as_ptr(), src.as_ptr()),
            "copy props",
        )?;
    }
    override_color(up.get_mut(), grade);
    Ok(up)
}

/// Download (for NVENC) and encode everything the renderer sends, then hold the last frame up
/// to `frames` (`tpad=stop_mode=clone`). Returns the number of frames encoded.
fn encode_loop(
    mut output: Output,
    rx: &Receiver<Frame>,
    frames: Option<u64>,
    t0: Instant,
) -> Result<(u64, Duration)> {
    let mut done = 0u64;
    let mut last = None;
    let mut first = Duration::ZERO;
    let mut t_enc = Busy::new("download+encode");
    for out in rx {
        t_enc.time(|| encode(&mut output, &out, done))?;
        if done == 0 {
            first = t0.elapsed();
        }
        last = Some(out);
        done += 1;
    }
    t_enc.report();
    if let (Some(l), Some(want)) = (&last, frames) {
        while done < want {
            encode(&mut output, l, done)?;
            done += 1;
        }
    }
    if done > 0 {
        output.finish()?;
    }
    Ok((done, first))
}

/// Busy time of one pipeline stage, printed with `FFCINE_TRACE=1`.
struct Busy {
    name: &'static str,
    spent: Duration,
    n: u32,
}

impl Busy {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            spent: Duration::ZERO,
            n: 0,
        }
    }

    fn time<T>(&mut self, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let r = f();
        self.spent += t.elapsed();
        self.n += 1;
        r
    }

    fn report(&self) {
        if std::env::var_os("FFCINE_TRACE").is_some() && self.n > 0 {
            eprintln!(
                "  {:>16}: {:7.1} ms busy, {:5.2} ms/frame over {} frames",
                self.name,
                self.spent.as_secs_f64() * 1e3,
                self.spent.as_secs_f64() * 1e3 / f64::from(self.n),
                self.n
            );
        }
    }
}

/// Hands a graded Vulkan frame to the encoder (Vulkan -> CUDA copy for NVENC).
fn encode(output: &mut Output, out: &Frame, index: u64) -> Result<()> {
    let to_vulkan = output.frames.as_ref().is_some_and(|fr| {
        // SAFETY: valid frames context.
        unsafe {
            (*fr.data::<ff::AVHWFramesContext>()).format == ff::AVPixelFormat::AV_PIX_FMT_VULKAN
        }
    });
    if to_vulkan {
        // The rendered frame is already in the encoder's frames context: no copy.
        let mut enc = out.clone();
        let e = enc.get_mut();
        e.pts = i64::try_from(index).unwrap_or(i64::MAX);
        e.duration = 1;
        e.pict_type = ff::AVPictureType::AV_PICTURE_TYPE_NONE;
        return output.encode(Some(&enc));
    }
    let mut enc = if let Some(fr) = &output.frames {
        Frame::hw(fr)?
    } else {
        let mut f = Frame::new()?;
        let o = out.get();
        // SAFETY: the Vulkan frame's sw format.
        let sw = unsafe { (*(*o.hw_frames_ctx).data.cast::<ff::AVHWFramesContext>()).sw_format };
        f.get_mut().format = sw as i32;
        f
    };
    // SAFETY: valid frames.
    unsafe {
        check(
            ff::av_hwframe_transfer_data(enc.as_ptr(), out.as_ptr(), 0),
            "download from Vulkan",
        )?;
        check(
            ff::av_frame_copy_props(enc.as_ptr(), out.as_ptr()),
            "copy props",
        )?;
    }
    let e = enc.get_mut();
    e.pts = i64::try_from(index).unwrap_or(i64::MAX);
    e.duration = 1;
    e.pict_type = ff::AVPictureType::AV_PICTURE_TYPE_NONE;
    output.encode(Some(&enc))
}

fn avframe(f: &Frame) -> pl_avframe_params {
    pl_avframe_params {
        frame: f.as_ptr().cast_const().cast(),
        tex: ptr::null_mut(),
        map_dovi: true,
        map_dovi_force: false,
    }
}

fn override_color(f: &mut ff::AVFrame, grade: &Grade) {
    let o = grade.input_color;
    if let Some(p) = o.primaries {
        f.color_primaries = io::av_primaries(p);
    }
    if let Some(t) = o.transfer {
        f.color_trc = io::av_transfer(t);
    }
    if let Some(m) = o.matrix {
        f.colorspace = io::av_matrix(m);
    }
    if let Some(r) = o.range {
        f.color_range = io::av_range(r);
    }
}

fn rect(c: crate::Crop) -> pl::pl_rect2df {
    pl::pl_rect2df {
        x0: c.x,
        y0: c.y,
        x1: c.x + c.width,
        y1: c.y + c.height,
    }
}

/// Index of `grade.hooks[0]` among the parsed hooks ([`hook_sources`] order).
fn first_inline_hook(grade: &Grade) -> usize {
    usize::from(grade.warp.is_some())
        + usize::from(crate::shaders::unsharp(grade.sharpen).is_some())
}

/// The grade's hook shaders as (name, source): the stabilize warp (`MAIN`, on the full
/// `width x height` source, the crops folded in), the generated unsharp, the inline hooks,
/// user files, then grain.
fn hook_sources(grade: &Grade, width: u32, height: u32) -> Result<Vec<(String, String)>> {
    let mut v = Vec::new();
    if let Some(w) = &grade.warp {
        let n = w.transforms.len().max(grade.crops.len());
        let t: Vec<_> = (0..n)
            .map(|i| {
                let m = w
                    .transforms
                    .get(i)
                    .or(w.transforms.last())
                    .copied()
                    .unwrap_or(ffcine_core::geom::IDENTITY);
                match grade.crop_at(i) {
                    Some(c) => ffcine_stabilize::fold_crop(
                        &[m],
                        [c.x, c.y, c.width, c.height].map(f64::from),
                        width,
                        height,
                    )[0],
                    None => m,
                }
            })
            .collect();
        v.push((
            "stabilize warp".to_owned(),
            ffcine_stabilize::hook::warp_hook_dynamic(&t, width, height),
        ));
    }
    if let Some(s) = crate::shaders::unsharp(grade.sharpen) {
        v.push(("unsharp".to_owned(), s));
    }
    for h in &grade.hooks {
        v.push((h.name.clone(), h.source.clone()));
    }
    for p in &grade.shaders {
        let text = std::fs::read_to_string(p).map_err(|source| crate::Error::Io {
            path: p.clone(),
            source,
        })?;
        v.push((p.display().to_string(), text));
    }
    // Last, so the inline hooks keep their indices (`first_inline_hook`).
    if let Some(s) = grade.grain.as_ref().and_then(ffcine_filters::grain::hook) {
        v.push(("grain".to_owned(), s));
    }
    Ok(v)
}

/// Default on-disk shader cache: `$XDG_CACHE_HOME/ffcine/placebo` (or `~/.cache/...`).
pub(crate) fn default_cache_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|d| d.join("ffcine").join("placebo"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_core::color::{Matrix, Primaries, Range, Transfer};

    #[test]
    fn layer_colour_follows_the_output() {
        use pl::pl_color_primaries as P;
        use pl::pl_color_transfer as T;
        let sdr = layer_color(ColorSpace::BT709_TV, None);
        assert_eq!(
            (sdr.primaries, sdr.transfer),
            (P::PL_COLOR_PRIM_BT_709, T::PL_COLOR_TRC_BT_1886)
        );
        let s = still_color(ColorSpace::BT709_TV);
        assert_eq!((s.primaries, s.transfer), (sdr.primaries, sdr.transfer));
        let hdr = |transfer| ColorSpace {
            primaries: Primaries::Bt2020,
            transfer,
            matrix: Matrix::Bt2020Ncl,
            range: Range::Limited,
        };
        let pq = layer_color(hdr(Transfer::Pq), None);
        assert_eq!(
            (pq.primaries, pq.transfer),
            (P::PL_COLOR_PRIM_BT_2020, T::PL_COLOR_TRC_PQ)
        );
        assert!(pq.hdr.max_luma.abs() < 1e-6);
        // The output's mastering display, as on the output frames, so layers share its peak.
        let m = HdrMetadata::P3_D65_1000.with_peak(600.0);
        let pq = layer_color(hdr(Transfer::Pq), Some(&m));
        assert!((pq.hdr.max_luma - 600.0).abs() < 1e-3 && (pq.hdr.max_cll - 600.0).abs() < 1e-3);
        assert!((pq.hdr.prim.red.x - 0.680).abs() < 1e-6);
        let hlg = layer_color(hdr(Transfer::Hlg), None);
        assert_eq!(
            (hlg.primaries, hlg.transfer),
            (P::PL_COLOR_PRIM_BT_2020, T::PL_COLOR_TRC_HLG)
        );
        // Stills on an HDR output are SDR (sRGB), mapped to the reference white.
        let st = still_color(hdr(Transfer::Pq));
        assert_eq!(
            (st.primaries, st.transfer),
            (P::PL_COLOR_PRIM_BT_709, T::PL_COLOR_TRC_SRGB)
        );
    }
}
