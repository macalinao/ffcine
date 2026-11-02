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
    self, Cache, CustomLut, Hooks, Log, OverlayTex, RenderParams, Renderer, RgbaTex, Vulkan,
};
use super::retime::{Q, Retimer};
use crate::pipeline::RunStats;
use crate::{
    Decode, Device, Grade, Layer, LayerSource, LutStage, LutType, Overlay, Pipeline, Result, Stage,
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
        let mut layers = Vec::new();
        let mut layer_inputs = Vec::new();
        for l in &p.layers {
            let (src, lw, lh) = match &l.source {
                LayerSource::Video { input, start } => {
                    let inp = Input::open(input, *start, dec_device)?;
                    let (lw, lh) = inp.size();
                    let (tx, rx) = sync_channel::<Frame>(DEPTH);
                    let want = u64::try_from(l.to - l.from).unwrap_or(0);
                    layer_inputs.push((inp, want, tx, &l.grade));
                    (LayerSrc::Video { rx, last: None }, lw, lh)
                }
                LayerSource::Image(img) => (
                    LayerSrc::Image(RgbaTex::image(&self.vk, img)?),
                    img.width,
                    img.height,
                ),
            };
            let (tw, th) = l.grade.size.map_or((lw, lh), |s| (s.width, s.height));
            layers.push(LayerRun {
                layer: l,
                pass: Pass::new(&self.vk, &l.grade, lw, lh)?,
                tex: RgbaTex::target(&self.vk, tw, th)?,
                src,
                renderer: Renderer::new(&self.log, &self.vk)?,
            });
        }
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
                for lr in &mut layers {
                    if let Some(rect) = lr.layer.rect_at(at, w, h) {
                        let local = usize::try_from(at - lr.layer.from).unwrap_or(0);
                        t_render.time(|| gpu.render_layer(lr, local))?;
                        shown.push((&*lr, rect));
                    }
                }
                let ov = overlays_at(&shown, &overlays, at);
                let out = t_render.time(|| gpu.render(src, index, &vk_out, grade, &pass, &ov))?;
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
            apply_grade(&mut image, &mut target, grade, &luts, index);
            if !overlays.list.is_empty() {
                target.overlays = overlays.list.as_ptr();
                target.num_overlays = i32::try_from(overlays.list.len()).unwrap_or(i32::MAX);
            }
            let ok = pl::pl_render_image(
                self.renderer.0,
                &raw const image,
                &raw const target,
                &raw const params.params,
            );
            pl_unmap_avframe(gpu, &raw mut target);
            if mapped {
                pl_unmap_avframe(gpu, &raw mut image);
            }
            if !ok {
                return Err(gpu_err("pl_render_image failed"));
            }
        }
        Ok(out)
    }
}

impl Gpu {
    /// Renders layer frame `local` of `lr` into its RGBA target.
    fn render_layer(&mut self, lr: &mut LayerRun<'_>, local: usize) -> Result<()> {
        if let LayerSrc::Video { rx, last } = &mut lr.src
            && let Ok(f) = rx.recv()
        {
            *last = Some(f);
        }
        let grade = &lr.layer.grade;
        let params = &*lr.pass.params;
        set_tracks(params, grade, local)?;
        let luts = lr.pass.luts();
        let color = layer_color();
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
                LayerSrc::Image(t) => (t.frame(color), false),
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

/// The colour space layers are rendered in and composited from: the output's (BT.709, BT.1886).
fn layer_color() -> pl::pl_color_space {
    pl::pl_color_space {
        primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
        transfer: pl::pl_color_transfer::PL_COLOR_TRC_BT_1886,
        ..Default::default()
    }
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
    src: LayerSrc,
    renderer: Renderer,
}

enum LayerSrc {
    Video {
        rx: Receiver<Frame>,
        /// Held past the source's end.
        last: Option<Frame>,
    },
    Image(RgbaTex),
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

/// The rendered `layers` at their placements, then the overlays visible on output frame `index`
/// with their fade applied to the part colours.
fn overlays_at(
    layers: &[(&LayerRun<'_>, crate::Crop)],
    all: &[(&Overlay, OverlayTex)],
    index: i64,
) -> Overlays {
    let mut parts = Vec::new();
    let mut texs = Vec::new();
    let mut list = Vec::new();
    for (lr, r) in layers {
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
        let f = lr.tex.frame(layer_color());
        list.push((
            lr.tex.tex,
            pl::pl_overlay_mode::PL_OVERLAY_NORMAL,
            f.repr,
            f.color,
        ));
    }
    for (o, tex) in all {
        let fade = o.opacity(index);
        if fade <= 0.0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let (mw, mh) = (o.mask.width as f32, o.mask.height as f32);
        let p: Vec<pl::pl_overlay_part> = o
            .parts
            .iter()
            .map(|part| {
                let [width, height] = part.size.unwrap_or([mw, mh]);
                let [left, top] = part.pos;
                let [red, green, blue, alpha] = part.colour;
                pl::pl_overlay_part {
                    src: pl::pl_rect2df {
                        x0: 0.0,
                        y0: 0.0,
                        x1: mw,
                        y1: mh,
                    },
                    dst: pl::pl_rect2df {
                        x0: left,
                        y0: top,
                        x1: left + width,
                        y1: top + height,
                    },
                    color: [red, green, blue, alpha * fade],
                }
            })
            .collect();
        texs.push(tex.tex);
        parts.push(p);
    }
    // SAFETY: reading libplacebo's exported constants.
    let (repr, color) = unsafe { (pl::pl_color_repr_rgb, pl::pl_color_space_srgb) };
    list.extend(
        texs.into_iter()
            .map(|t| (t, pl::pl_overlay_mode::PL_OVERLAY_MONOCHROME, repr, color)),
    );
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
