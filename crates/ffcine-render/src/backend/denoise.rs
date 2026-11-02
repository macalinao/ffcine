//! Runs `ffcine-filters`' denoise kernels on the grade's `pl_gpu` (`docs/denoise.md`).
//!
//! Every decoded frame is copied into a ring slot (plus a quarter-size luma for motion search)
//! and released back to the decoder at once, so the decoder's pool doesn't run dry while we hold
//! `radius` frames of lookahead. Once frame `c + radius` is in, frame `c` is denoised (motion to
//! each neighbour, merge, spatial NLM) into output textures, and handed to `pl_render_image` as a
//! `pl_frame` with the decoded frame's colour metadata and our planes. All of it is compute on
//! libplacebo's dispatch, which orders it against the render with its own barriers; the CPU never
//! waits on the GPU here.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_void};
use std::ptr;

use ffcine_filters::denoise::{self as k, Compute, Plane};
use ffcine_filters::{Denoise, Motion, Spatial};
use ffcine_stabilize::cuda::{CuContext, Cuda, ExternalBuffer, Mem};
use ffcine_stabilize::nvof::{OpticalFlow, PerfLevel};
use ffmpeg_next::ffi as ff;
use libplacebo_sys as pl;
use pl::libav::{pl_avframe_params, pl_map_avframe_ex, pl_unmap_avframe};

use super::av::{Frame, check, gpu_err};
use crate::Result;

/// One decoded frame's planes in VRAM.
#[derive(Debug)]
struct Slot {
    planes: Vec<pl::pl_tex>,
    quarter: pl::pl_tex,
}

/// A frame waiting for its lookahead: its props (no buffers) and mapped `pl_frame` metadata.
struct Pending {
    props: Frame,
    image: pl::pl_frame,
    dovi: Option<Box<pl::pl_dovi_metadata>>,
    index: u64,
}

/// Plane layout, fixed for a run by the first frame.
#[derive(Debug, Clone)]
struct Layout {
    /// Per plane: texture size, channel count, format name of our copies, shape vs luma.
    planes: Vec<(i32, i32, Plane, &'static CStr)>,
    luma: usize,
    norm: f32,
}

/// NVOF motion: half-res 8-bit luma rendered by Vulkan into an exported buffer, copied device to
/// device into the session's input ring, flow read back (small) and uploaded as a texture.
struct Nvof {
    of: OpticalFlow,
    luma8: pl::pl_tex,
    buf: pl::pl_buf,
    ext: ExternalBuffer,
    pitch: usize,
    ctx: CuContext,
    /// Per neighbour, NVOF's vectors in luma pixels (`rg32f`, one per grid cell).
    flow_tex: Vec<pl::pl_tex>,
    flow: Vec<[i16; 2]>,
    cost: Vec<u8>,
    staging: Vec<f32>,
    /// Wall time in NVOF and the CUDA copies (synchronous), ns.
    wall_ns: u64,
}

pub(crate) struct Denoiser {
    gpu: pl::pl_gpu,
    dp: pl::pl_dispatch,
    cfg: Denoise,
    layout: Option<Layout>,
    ring: Vec<Slot>,
    /// Motion to each neighbour, ordered -1, +1, -2, +2, ...
    motion: Vec<pl::pl_tex>,
    merged: Vec<pl::pl_tex>,
    out: Vec<pl::pl_tex>,
    pending: VecDeque<Pending>,
    cuda: Option<CuContext>,
    nvof: Option<Nvof>,
    last_dovi: Option<Box<pl::pl_dovi_metadata>>,
    /// Frames pushed so far.
    pushed: u64,
    /// One timer per dispatch ordinal in a frame, and what that dispatch is.
    timers: Vec<(pl::pl_timer, &'static str)>,
    /// Summed GPU time (ns) per kernel kind, and frames denoised.
    gpu_ns: Vec<(&'static str, u64)>,
    frames: u64,
}

// SAFETY: used from the render thread only, like the rest of the libplacebo state.
unsafe impl Send for Denoiser {}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

fn tex(gpu: pl::pl_gpu, w: i32, h: i32, fmt: &CStr) -> Result<pl::pl_tex> {
    // SAFETY: valid gpu and name.
    let format = unsafe { pl::pl_find_named_fmt(gpu, fmt.as_ptr()) };
    if format.is_null() {
        return Err(gpu_err(format!("denoise: no {fmt:?} format")));
    }
    let params = pl::pl_tex_params {
        w,
        h,
        format,
        sampleable: true,
        storable: true,
        ..Default::default()
    };
    // SAFETY: valid params.
    let t = unsafe { pl::pl_tex_create(gpu, &raw const params) };
    if t.is_null() {
        return Err(gpu_err(format!("denoise: pl_tex_create {w}x{h} {fmt:?}")));
    }
    Ok(t)
}

fn sampled(name: &'static CStr, t: pl::pl_tex, linear: bool) -> pl::pl_shader_desc {
    pl::pl_shader_desc {
        desc: pl::pl_desc {
            name: name.as_ptr(),
            type_: pl::pl_desc_type::PL_DESC_SAMPLED_TEX,
            ..Default::default()
        },
        binding: pl::pl_desc_binding {
            object: t.cast::<c_void>(),
            address_mode: pl::pl_tex_address_mode::PL_TEX_ADDRESS_CLAMP,
            sample_mode: if linear {
                pl::pl_tex_sample_mode::PL_TEX_SAMPLE_LINEAR
            } else {
                pl::pl_tex_sample_mode::PL_TEX_SAMPLE_NEAREST
            },
        },
        ..Default::default()
    }
}

fn storage(t: pl::pl_tex) -> pl::pl_shader_desc {
    pl::pl_shader_desc {
        desc: pl::pl_desc {
            name: c"dst".as_ptr(),
            type_: pl::pl_desc_type::PL_DESC_STORAGE_IMG,
            access: pl::pl_desc_access::PL_DESC_ACCESS_WRITEONLY,
            ..Default::default()
        },
        binding: pl::pl_desc_binding {
            object: t.cast::<c_void>(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Descriptor names for neighbour `i` (planes, motion).
const N: [&CStr; 10] = [
    c"n0", c"n1", c"n2", c"n3", c"n4", c"n5", c"n6", c"n7", c"n8", c"n9",
];
const M: [&CStr; 10] = [
    c"m0", c"m1", c"m2", c"m3", c"m4", c"m5", c"m6", c"m7", c"m8", c"m9",
];

fn size(t: pl::pl_tex) -> (i32, i32) {
    // SAFETY: valid texture.
    let p = unsafe { &(*t).params };
    (p.w, p.h)
}

fn groups(w: i32, h: i32) -> [i32; 3] {
    let g = i32::try_from(k::GROUP).unwrap_or(16);
    [(w + g - 1) / g, (h + g - 1) / g, 1]
}

impl Denoiser {
    /// `cuda`: the CUDA context on the same GPU, for NVOF motion.
    pub(crate) fn new(
        log: pl::pl_log,
        gpu: pl::pl_gpu,
        cfg: Denoise,
        cuda: Option<CuContext>,
    ) -> Result<Self> {
        // SAFETY: valid log and gpu, destroyed in Drop before them (field order in Gpu).
        let dp = unsafe { pl::pl_dispatch_create(log, gpu) };
        if dp.is_null() {
            return Err(gpu_err("pl_dispatch_create failed"));
        }
        Ok(Self {
            gpu,
            dp,
            cfg: cfg.clamped(),
            layout: None,
            ring: Vec::new(),
            motion: Vec::new(),
            merged: Vec::new(),
            out: Vec::new(),
            pending: VecDeque::new(),
            cuda,
            nvof: None,
            last_dovi: None,
            pushed: 0,
            timers: Vec::new(),
            gpu_ns: Vec::new(),
            frames: 0,
        })
    }

    fn radius(&self) -> u64 {
        u64::from(self.cfg.radius())
    }

    fn slots(&self) -> u64 {
        2 * self.radius() + 1
    }

    /// Runs one compute kernel; `n` is its ordinal within the frame (for its timer).
    fn dispatch(
        &mut self,
        n: usize,
        what: &'static str,
        c: &Compute,
        descs: &[pl::pl_shader_desc],
        grid: [i32; 3],
    ) -> Result<()> {
        while self.timers.len() <= n {
            // SAFETY: valid gpu.
            self.timers
                .push((unsafe { pl::pl_timer_create(self.gpu) }, what));
        }
        // Results arrive a few frames late; book them under what the timer measured then.
        let (timer, kind) = self.timers[n];
        self.timers[n].1 = what;
        let header = CString::new(c.header.as_str()).map_err(|e| gpu_err(e.to_string()))?;
        let body = CString::new(c.body.as_str()).map_err(|e| gpu_err(e.to_string()))?;
        let desc = CString::new(format!("ffcine denoise {what}")).unwrap_or_default();
        let params = pl::pl_custom_shader {
            header: header.as_ptr(),
            body: body.as_ptr(),
            description: desc.as_ptr(),
            input: pl::pl_shader_sig::PL_SHADER_SIG_NONE,
            output: pl::pl_shader_sig::PL_SHADER_SIG_NONE,
            descriptors: descs.as_ptr(),
            num_descriptors: i32::try_from(descs.len()).unwrap_or(0),
            compute: true,
            compute_shmem: c.shmem,
            compute_group_size: [16, 16],
            ..Default::default()
        };
        // SAFETY: valid dispatch; the strings and descriptors outlive the calls, which copy them.
        unsafe {
            let mut sh = pl::pl_dispatch_begin(self.dp);
            if !pl::pl_shader_custom(sh, &raw const params) {
                pl::pl_dispatch_abort(self.dp, &raw mut sh);
                return Err(gpu_err(format!("denoise {what}: pl_shader_custom failed")));
            }
            let ok = pl::pl_dispatch_compute(
                self.dp,
                &pl::pl_dispatch_compute_params {
                    shader: &raw mut sh,
                    dispatch_size: grid,
                    timer,
                    ..Default::default()
                },
            );
            if !ok {
                return Err(gpu_err(format!(
                    "denoise {what}: pl_dispatch_compute failed"
                )));
            }
            loop {
                let t = pl::pl_timer_query(self.gpu, timer);
                if t == 0 {
                    break;
                }
                match self.gpu_ns.iter_mut().find(|(k, _)| *k == kind) {
                    Some((_, ns)) => *ns += t,
                    None => self.gpu_ns.push((kind, t)),
                }
            }
        }
        Ok(())
    }

    fn init(&mut self, image: &pl::pl_frame) -> Result<()> {
        let mut planes = Vec::new();
        let mut luma = None;
        let n = usize::try_from(image.num_planes).unwrap_or(0);
        let (lw, lh) = (0..n)
            .find(|&i| image.planes[i].component_mapping[0] == pl::pl_channel::PL_CHANNEL_Y as i32)
            .map(|i| size(image.planes[i].texture))
            .ok_or_else(|| gpu_err("denoise needs a YUV source with a luma plane"))?;
        for i in 0..n {
            let p = &image.planes[i];
            let (w, h) = size(p.texture);
            let comps = u32::try_from(p.components).unwrap_or(0);
            let fmt = match comps {
                1 => c"r16",
                2 => c"rg16",
                _ => return Err(gpu_err("denoise needs planar or semi-planar YUV")),
            };
            if p.component_mapping[0] == pl::pl_channel::PL_CHANNEL_Y as i32 {
                luma = Some(i);
            }
            let sub = (
                u32::try_from((lw + w - 1) / w.max(1)).unwrap_or(1),
                u32::try_from((lh + h - 1) / h.max(1)).unwrap_or(1),
            );
            planes.push((w, h, Plane { comps, sub }, fmt));
        }
        let mut repr = image.repr;
        // SAFETY: plain struct; normalize only rewrites the copy's bit depth fields.
        let norm = unsafe { pl::pl_color_repr_normalize(&raw mut repr) };
        let layout = Layout {
            planes,
            luma: luma.unwrap_or(0),
            norm,
        };
        let gpu = self.gpu;
        let (qw, qh) = ((lw + 3) / 4, (lh + 3) / 4);
        let temporal = self.cfg.temporal.is_some();
        for _ in 0..self.slots() {
            let planes = layout
                .planes
                .iter()
                .map(|&(w, h, _, f)| tex(gpu, w, h, f))
                .collect::<Result<_>>()?;
            let quarter = if temporal {
                tex(gpu, qw, qh, c"r16f")?
            } else {
                ptr::null()
            };
            self.ring.push(Slot { planes, quarter });
        }
        let want_nvof = self.cfg.temporal.is_some_and(|t| t.motion == Motion::Nvof);
        if want_nvof {
            match self.init_nvof(lw, lh) {
                Ok(n) => self.nvof = Some(n),
                Err(e) => eprintln!("denoise: NVOF unavailable ({e}); using block matching"),
            }
        }
        let b = i32::try_from(self.block()).unwrap_or(16);
        for _ in 0..2 * self.radius() {
            self.motion
                .push(tex(gpu, (lw + b - 1) / b, (lh + b - 1) / b, c"rgba16f")?);
        }
        for &(w, h, _, f) in &layout.planes {
            if temporal {
                self.merged.push(tex(gpu, w, h, f)?);
            }
            self.out.push(tex(gpu, w, h, f)?);
        }
        self.layout = Some(layout);
        Ok(())
    }

    /// Luma pixels per motion vector.
    fn block(&self) -> u32 {
        if self.nvof.is_some() {
            k::NVOF_BLOCK
        } else {
            k::BLOCK
        }
    }

    fn init_nvof(&mut self, lw: i32, lh: i32) -> Result<Nvof> {
        let ctx = self.cuda.ok_or_else(|| gpu_err("no CUDA device"))?;
        let (w2, h2) = ((lw + 1) / 2, (lh + 1) / 2);
        let (uw, uh) = (
            u32::try_from(w2).unwrap_or(0),
            u32::try_from(h2).unwrap_or(0),
        );
        let slots = usize::try_from(self.slots()).unwrap_or(3);
        // SAFETY: ffmpeg's CUDA context on this GPU, alive for the whole run.
        let of = unsafe { OpticalFlow::with_slots(ctx, uw, uh, 4, PerfLevel::Medium, slots)? };
        let format = c"r8";
        // SAFETY: valid gpu and name.
        let fmt = unsafe { pl::pl_find_named_fmt(self.gpu, format.as_ptr()) };
        let lp = pl::pl_tex_params {
            w: w2,
            h: h2,
            format: fmt,
            storable: true,
            host_readable: true,
            ..Default::default()
        };
        // SAFETY: valid params.
        let luma8 = unsafe { pl::pl_tex_create(self.gpu, &raw const lp) };
        if luma8.is_null() {
            return Err(gpu_err("denoise: r8 luma texture"));
        }
        let pitch = usize::try_from(w2).unwrap_or(0).next_multiple_of(256);
        let size = pitch * usize::try_from(h2).unwrap_or(0);
        let bp = pl::pl_buf_params {
            size,
            export_handle: pl::pl_handle_type::PL_HANDLE_FD,
            ..Default::default()
        };
        // SAFETY: valid params.
        let mut buf = unsafe { pl::pl_buf_create(self.gpu, &raw const bp) };
        // SAFETY: an exported fd buffer; CUDA gets its own dup of the fd.
        let ext = if buf.is_null() {
            Err(gpu_err("denoise: exportable buffer"))
        } else {
            // SAFETY: as above.
            unsafe {
                let sm = &(*buf).shared_mem;
                Cuda::get().and_then(|cuda| {
                    let _g = cuda.push(ctx)?;
                    let fd = libc_dup(sm.handle.fd);
                    cuda.import_fd(fd, sm.size as u64 - sm.offset as u64, sm.offset as u64)
                })
            }
        };
        let ext = match ext {
            Ok(e) => e,
            Err(e) => {
                let mut luma8 = luma8;
                // SAFETY: ours, unused.
                unsafe {
                    pl::pl_tex_destroy(self.gpu, &raw mut luma8);
                    if !buf.is_null() {
                        pl::pl_buf_destroy(self.gpu, &raw mut buf);
                    }
                }
                return Err(e);
            }
        };
        let mut flow_tex = Vec::new();
        for _ in 0..2 * self.radius() {
            let fmt = unsafe { pl::pl_find_named_fmt(self.gpu, c"rg32f".as_ptr()) };
            let p = pl::pl_tex_params {
                w: i32::try_from(of.grid_w).unwrap_or(0),
                h: i32::try_from(of.grid_h).unwrap_or(0),
                format: fmt,
                sampleable: true,
                host_writable: true,
                ..Default::default()
            };
            // SAFETY: valid params.
            let t = unsafe { pl::pl_tex_create(self.gpu, &raw const p) };
            if t.is_null() {
                return Err(gpu_err("denoise: flow texture"));
            }
            flow_tex.push(t);
        }
        Ok(Nvof {
            of,
            luma8,
            buf,
            ext,
            pitch,
            ctx,
            flow_tex,
            flow: Vec::new(),
            cost: Vec::new(),
            staging: Vec::new(),
            wall_ns: 0,
        })
    }

    /// Renders slot `s`'s half-res 8-bit luma and copies it into NVOF input slot `s`.
    fn nvof_ingest(&mut self, slot: usize, luma: pl::pl_tex, norm: f32, n: usize) -> Result<()> {
        let Some((luma8, buf, pitch)) = self.nvof.as_ref().map(|v| (v.luma8, v.buf, v.pitch))
        else {
            return Ok(());
        };
        let (w, h) = size(luma8);
        let d = [sampled(c"src", luma, false), storage(luma8)];
        self.dispatch(n, "luma8", &k::luma8_kernel(norm), &d, groups(w, h))?;
        let t0 = std::time::Instant::now();
        let tp = pl::pl_tex_transfer_params {
            tex: luma8,
            row_pitch: pitch,
            buf,
            ..Default::default()
        };
        // SAFETY: our texture and buffer; the export hands the memory to CUDA once the GPU is
        // done, which the poll waits for.
        unsafe {
            if !pl::pl_tex_download(self.gpu, &raw const tp) || !pl::pl_buf_export(self.gpu, buf) {
                return Err(gpu_err("denoise: luma export failed"));
            }
            while pl::pl_buf_poll(self.gpu, buf, 1_000_000_000) {}
        }
        let nv = self.nvof.as_mut().ok_or_else(|| gpu_err("nvof"))?;
        // SAFETY: the mapped buffer holds h rows of `pitch` bytes.
        unsafe { nv.of.upload(slot, Mem::Device(nv.ext.ptr), pitch)? };
        nv.wall_ns += u64::try_from(t0.elapsed().as_nanos()).unwrap_or(0);
        Ok(())
    }

    /// NVOF flow from slot `sc` to slot `sf` into `self.motion[m]`, scored for confidence.
    fn nvof_motion(&mut self, sc: usize, sf: usize, m: usize, luma: usize, n: usize) -> Result<()> {
        let norm = self.layout.as_ref().map_or(1.0, |l| l.norm);
        let nv = self.nvof.as_mut().ok_or_else(|| gpu_err("nvof"))?;
        let t0 = std::time::Instant::now();
        nv.of.run_with(sc, sf, false, &mut nv.flow, &mut nv.cost)?;
        // S10.5 at half resolution -> luma pixels.
        nv.staging.clear();
        nv.staging
            .extend(nv.flow.iter().flat_map(|v| v.map(|x| f32::from(x) / 16.0)));
        let ft = nv.flow_tex[m];
        let tp = pl::pl_tex_transfer_params {
            tex: ft,
            row_pitch: nv.of.grid_w as usize * 8,
            ptr: nv.staging.as_mut_ptr().cast(),
            ..Default::default()
        };
        // SAFETY: staging holds grid_w x grid_h rg32f texels; libplacebo copies before returning
        // (no callback).
        if !unsafe { pl::pl_tex_upload(self.gpu, &raw const tp) } {
            return Err(gpu_err("denoise: flow upload failed"));
        }
        nv.wall_ns += u64::try_from(t0.elapsed().as_nanos()).unwrap_or(0);
        let (mw, mh) = size(self.motion[m]);
        let d = [
            sampled(c"flow", ft, false),
            sampled(c"lc", self.ring[sc].planes[luma], false),
            sampled(c"ln", self.ring[sf].planes[luma], false),
            storage(self.motion[m]),
        ];
        self.dispatch(
            n,
            "confidence",
            &k::confidence_kernel(norm),
            &d,
            groups(mw, mh),
        )
    }

    /// Takes a decoded frame into the ring. Its images are released before this returns.
    pub(crate) fn push(&mut self, up: &Frame) -> Result<()> {
        let mut image = pl::pl_frame::default();
        let params = pl_avframe_params {
            frame: up.as_ptr().cast_const().cast(),
            tex: ptr::null_mut(),
            map_dovi: true,
            map_dovi_force: false,
        };
        // SAFETY: a valid Vulkan AVFrame on libplacebo's device; unmapped below.
        if !unsafe { pl_map_avframe_ex(self.gpu, &raw mut image, &raw const params) } {
            return Err(gpu_err("pl_map_avframe_ex (denoise) failed"));
        }
        // The mapped images are held by ffmpeg until acquired (pl_render_image does this itself).
        // SAFETY: the frame's own callbacks; the copies are ordered before the release.
        let acquired = image
            .acquire
            .is_none_or(|acq| unsafe { acq(self.gpu, &raw mut image) });
        let r = if acquired {
            self.ingest(&image)
        } else {
            Err(gpu_err("denoise: acquiring the decoded frame failed"))
        };
        // Unmapping clears the struct and frees the DV metadata; keep both.
        let mapped = image;
        let dovi = (acquired && !image.repr.dovi.is_null()).then(|| {
            // SAFETY: non-null, owned by the mapping until the unmap below.
            Box::new(unsafe { *image.repr.dovi })
        });
        // SAFETY: mapped above.
        unsafe {
            if acquired && let Some(rel) = image.release {
                rel(self.gpu, &raw mut image);
            }
            pl_unmap_avframe(self.gpu, &raw mut image);
        }
        r?;
        let mut image = mapped;

        image.acquire = None;
        image.release = None;
        image.user_data = ptr::null_mut();
        image.profile = pl::pl_icc_profile::default();
        image.film_grain = pl::pl_film_grain_data::default();
        image.lut = ptr::null();
        let mut props = Frame::new()?;
        // SAFETY: valid frames; props only, no buffers.
        unsafe {
            check(
                ff::av_frame_copy_props(props.as_ptr(), up.as_ptr()),
                "copy props",
            )?
        };
        props.get_mut().width = up.get().width;
        props.get_mut().height = up.get().height;
        self.pending.push_back(Pending {
            props,
            image,
            dovi,
            index: self.pushed,
        });
        self.pushed += 1;
        Ok(())
    }

    fn ingest(&mut self, image: &pl::pl_frame) -> Result<()> {
        if self.layout.is_none() {
            self.init(image)?;
        }
        let layout = self
            .layout
            .clone()
            .ok_or_else(|| gpu_err("denoise: no layout"))?;
        let s = usize::try_from(self.pushed % self.slots()).unwrap_or(0);
        for (i, &(w, h, _, _)) in layout.planes.iter().enumerate() {
            let src = image.planes[i].texture;
            if size(src) != (w, h) {
                return Err(gpu_err("denoise: frame size changed mid-shot"));
            }
            let d = [sampled(c"src", src, false), storage(self.ring[s].planes[i])];
            self.dispatch(i, "copy", &k::copy_kernel(), &d, groups(w, h))?;
        }
        if self.cfg.temporal.is_some() {
            let q = self.ring[s].quarter;
            let (qw, qh) = size(q);
            let d = [
                sampled(c"src", self.ring[s].planes[layout.luma], false),
                storage(q),
            ];
            self.dispatch(
                layout.planes.len(),
                "quarter",
                &k::quarter_kernel(layout.norm),
                &d,
                groups(qw, qh),
            )?;
            let luma = self.ring[s].planes[layout.luma];
            self.nvof_ingest(s, luma, layout.norm, layout.planes.len() + 1)?;
        }
        Ok(())
    }

    /// Next frame to render, once its lookahead is in (or at the end of the stream, `flush`).
    /// Returns its props and a `pl_frame` reading our denoised planes; valid until the next call.
    pub(crate) fn pop(&mut self, flush: bool) -> Result<Option<(Frame, pl::pl_frame)>> {
        let Some(front) = self.pending.front() else {
            return Ok(None);
        };
        if !flush && front.index + self.radius() >= self.pushed {
            return Ok(None);
        }
        let c = front.index;
        self.run(c)?;
        let Some(mut p) = self.pending.pop_front() else {
            return Ok(None);
        };
        for (i, t) in self.out.iter().enumerate() {
            p.image.planes[i].texture = *t;
        }
        // Kept until the next pop, so the returned frame's pointer stays valid.
        self.last_dovi = p.dovi.take();
        p.image.repr.dovi = self.last_dovi.as_deref().map_or(ptr::null(), ptr::from_ref);
        self.frames += 1;
        Ok(Some((p.props, p.image)))
    }

    /// Denoises frame `c` into `self.out`.
    fn run(&mut self, c: u64) -> Result<()> {
        let layout = self
            .layout
            .clone()
            .ok_or_else(|| gpu_err("denoise: no layout"))?;
        let slots = self.slots();
        let slot = |f: u64| usize::try_from(f % slots).unwrap_or(0);
        let sc = slot(c);
        let mut n = layout.planes.len() + 2;
        let mut src: Vec<pl::pl_tex> = self.ring[sc].planes.clone();

        if let Some(t) = self.cfg.temporal {
            // Neighbours by distance, each seeded by the vector to the nearer one on its side.
            let mut nb: Vec<(u64, usize)> = Vec::new();
            for d in 1..=self.radius() {
                for (dir, f) in [(0, c.checked_sub(d)), (1, Some(c + d))] {
                    let Some(f) = f.filter(|&f| f < self.pushed) else {
                        continue;
                    };
                    let m = usize::try_from(2 * (d - 1)).unwrap_or(0) + dir;
                    if self.nvof.is_some() {
                        self.nvof_motion(sc, slot(f), m, layout.luma, n)?;
                        n += 1;
                        nb.push((f, m));
                        continue;
                    }
                    let pred = (d > 1 && nb.iter().any(|&(_, pm)| pm + 2 == m)).then(|| m - 2);
                    let mut descs = vec![
                        sampled(c"qc", self.ring[sc].quarter, false),
                        sampled(c"qn", self.ring[slot(f)].quarter, false),
                        sampled(c"lc", self.ring[sc].planes[layout.luma], false),
                        sampled(c"ln", self.ring[slot(f)].planes[layout.luma], false),
                        storage(self.motion[m]),
                    ];
                    #[allow(clippy::cast_precision_loss)]
                    let scale = pred.map(|_| d as f32 / (d - 1) as f32);
                    if let Some(pm) = pred {
                        descs.push(sampled(c"pred", self.motion[pm], false));
                    }
                    let (mw, mh) = size(self.motion[m]);
                    self.dispatch(
                        n,
                        "motion",
                        &k::motion_kernel(layout.norm, scale),
                        &descs,
                        [(mw + 15) / 16, (mh + 15) / 16, 1],
                    )?;
                    n += 1;
                    nb.push((f, m));
                }
            }
            for (i, &(w, h, plane, _)) in layout.planes.iter().enumerate() {
                let (strength, protect) = if i == layout.luma {
                    (t.strength, t.protect)
                } else {
                    (t.chroma_strength(), 0.0)
                };
                let mut descs = vec![sampled(c"cur", src[i], false), storage(self.merged[i])];
                for (j, &(f, m)) in nb.iter().enumerate() {
                    descs.push(sampled(N[j], self.ring[slot(f)].planes[i], true));
                    descs.push(sampled(M[j], self.motion[m], true));
                }
                let kern = k::merge_kernel(
                    nb.len(),
                    plane,
                    layout.norm,
                    strength,
                    protect,
                    self.block(),
                );
                self.dispatch(n, "merge", &kern, &descs, groups(w, h))?;
                n += 1;
            }
            src.clone_from(&self.merged);
        }

        for (i, &(w, h, plane, _)) in layout.planes.iter().enumerate() {
            let strength =
                self.cfg.spatial.map_or(
                    0.0,
                    |s: Spatial| {
                        if i == layout.luma { s.luma } else { s.chroma }
                    },
                );
            let d = [sampled(c"src", src[i], false), storage(self.out[i])];
            if strength > 0.0 {
                let r = self.cfg.spatial.map_or(2, |s| s.radius);
                let protect = self
                    .cfg
                    .spatial
                    .filter(|_| i == layout.luma)
                    .map_or(0.0, |s| s.protect);
                let kern = k::spatial_kernel(r, plane.comps, layout.norm, strength, protect);
                self.dispatch(n, "spatial", &kern, &d, groups(w, h))?;
            } else {
                self.dispatch(n, "copy", &k::copy_kernel(), &d, groups(w, h))?;
            }
            n += 1;
        }
        Ok(())
    }

    /// GPU time per frame and frames, for the run's report.
    pub(crate) fn report(&self) {
        if self.frames > 0 {
            #[allow(clippy::cast_precision_loss)]
            let ms = |ns: u64| ns as f64 / 1e6 / self.frames as f64;
            let total: u64 = self.gpu_ns.iter().map(|(_, ns)| ns).sum();
            let parts: Vec<String> = self
                .gpu_ns
                .iter()
                .map(|&(k, ns)| format!("{k} {:.2}", ms(ns)))
                .collect();
            let nvof = self
                .nvof
                .as_ref()
                .map(|n| format!(", NVOF + copies {:.2} ms wall/frame", ms(n.wall_ns)))
                .unwrap_or_default();
            eprintln!(
                "denoise: {} frames, {:.2} ms GPU/frame ({}){nvof}, motion {} {:?}",
                self.frames,
                ms(total),
                parts.join(", "),
                if self.nvof.is_some() {
                    "nvof"
                } else {
                    "blocks"
                },
                self.cfg
            );
        }
    }
}

impl Drop for Denoiser {
    fn drop(&mut self) {
        let gpu = self.gpu;
        let mut all: Vec<pl::pl_tex> = Vec::new();
        for s in &self.ring {
            all.extend(&s.planes);
            all.push(s.quarter);
        }
        all.extend(&self.motion);
        all.extend(&self.merged);
        all.extend(&self.out);
        if let Some(n) = &mut self.nvof {
            all.push(n.luma8);
            all.extend(&n.flow_tex);
            // SAFETY: ours; CUDA's import is released with `ext` after this.
            unsafe { pl::pl_buf_destroy(gpu, &raw mut n.buf) };
            let _ = n.ctx;
        }
        // SAFETY: our own objects on a live gpu; libplacebo waits for pending use.
        unsafe {
            for mut t in all.into_iter().filter(|t| !t.is_null()) {
                pl::pl_tex_destroy(gpu, &raw mut t);
            }
            for (t, _) in &mut self.timers {
                pl::pl_timer_destroy(gpu, t);
            }
            pl::pl_dispatch_destroy(&raw mut self.dp);
        }
    }
}

unsafe extern "C" {
    #[link_name = "dup"]
    fn libc_dup_raw(fd: i32) -> i32;
}

/// `dup(2)`: CUDA takes ownership of the fd it imports, libplacebo keeps its own.
unsafe fn libc_dup(fd: i32) -> i32 {
    // SAFETY: plain syscall wrapper.
    unsafe { libc_dup_raw(fd) }
}
