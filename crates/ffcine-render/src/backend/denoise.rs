//! Runs `ffcine-filters`' denoise kernels on the grade's `pl_gpu` (`docs/denoise.md`).
//!
//! Every decoded frame is copied into a ring slot (plus a quarter-size luma for motion search)
//! and released back to the decoder at once, so the decoder's pool doesn't run dry while we hold
//! `radius` frames of lookahead. Once frame `c + radius` is in, frame `c` is denoised (motion to
//! each neighbour, merge, spatial NLM) into output textures, and handed to `pl_render_image` as a
//! `pl_frame` with the decoded frame's colour metadata and our planes. All of it is compute on
//! libplacebo's dispatch, which orders it against the render with its own barriers; the CPU never
//! waits on the GPU here. NVOF motion runs on a CUDA stream, synchronised with Vulkan by two
//! exported timeline semaphores, one frame ahead of the render (see [`Nvof`]).

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_void};
use std::fmt::Write as _;
use std::ptr;

use ffcine_filters::denoise::{self as k, Compute, Plane};
use ffcine_filters::{Denoise, Motion, Spatial};
use ffcine_stabilize::cuda::{CuContext, Cuda, ExternalBuffer, ExternalSemaphore, Mem, Stream};
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

/// Which ring slot holds a frame, when a frame can be denoised and when its NVOF can be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Schedule {
    radius: u64,
    /// Frames rendered behind the newest one's lookahead: 1 with NVOF, so its flow is computed
    /// while the previous frame renders.
    lag: u64,
}

impl Schedule {
    fn slots(self) -> u64 {
        2 * self.radius + 1 + self.lag
    }

    fn slot(self, f: u64) -> usize {
        usize::try_from(f % self.slots()).unwrap_or(0)
    }

    /// Frame `c` can be denoised once `pushed` frames are in (or at the end of the stream).
    fn ready(self, c: u64, pushed: u64, flush: bool) -> bool {
        flush || c + self.radius + self.lag < pushed
    }

    /// Frames below this have all their neighbours in once `pushed` frames are, so their NVOF
    /// can be queued.
    fn nvof_upto(self, pushed: u64) -> u64 {
        pushed.saturating_sub(self.radius)
    }

    /// Neighbours of `c` that exist once `pushed` frames are in, by distance, with their motion
    /// index (-1, +1, -2, +2, ...).
    fn neighbours(self, c: u64, pushed: u64) -> Vec<(u64, usize)> {
        let mut nb = Vec::new();
        for d in 1..=self.radius {
            for (dir, f) in [(0, c.checked_sub(d)), (1, Some(c + d))] {
                if let Some(f) = f.filter(|&f| f < pushed) {
                    nb.push((f, usize::try_from(2 * (d - 1)).unwrap_or(0) + dir));
                }
            }
        }
        nb
    }
}

/// `VK_QUEUE_FAMILY_IGNORED`: hold/release without a queue family transfer.
const QF_IGNORED: u32 = u32::MAX;

/// NVOF motion, on a CUDA stream that the CPU never waits for. Per pushed frame `f`:
///
/// - Vulkan renders half-res 8-bit luma into `luma8`, downloads it into the exported `buf` and
///   holds `luma8`, which signals `vk_sem`; the stream waits for it, copies the buffer into NVOF
///   input slot `f`, signals `cu_sem`, and `luma8` is released waiting on that (so the next
///   frame's luma can't overwrite `buf` before CUDA has read it).
/// - Frame `f - radius` then has all its neighbours: its `2r` flows are queued on the stream
///   and copied (S10.5 shorts) into flow set `c % 2`, an exported buffer, then `cu_sem` again.
/// - The frame is denoised one push later: `flow_tex[set][0]` is released waiting on that
///   signal, a Vulkan pass converts the set into `rg32f` textures, the confidence passes read
///   them, and `flow_tex[set][0]` is held again: the stream waits for that hold before reusing
///   the set two frames later.
struct Nvof {
    of: OpticalFlow,
    stream: Stream,
    luma8: pl::pl_tex,
    buf: pl::pl_buf,
    ext: ExternalBuffer,
    pitch: usize,
    ctx: CuContext,
    /// Per set: the flows (`2r` regions of `grid_w x grid_h` `[i16; 2]`), CUDA's mapping of them,
    /// and per neighbour the vectors in luma pixels (`rg32f`, one per grid cell).
    flow_buf: [pl::pl_buf; 2],
    flow_ext: Vec<ExternalBuffer>,
    flow_tex: [Vec<pl::pl_tex>; 2],
    /// `flow_tex[set][0]`'s layout while held.
    flow_layout: [pl::VkImageLayout; 2],
    /// Per set: `vk_sem` value once Vulkan is done reading it, `cu_sem` value once it's written.
    set_free: [u64; 2],
    set_ready: [u64; 2],
    /// Signalled by Vulkan (holds), waited on by CUDA; and the reverse. Last values used.
    vk_sem: pl::VkSemaphore,
    cu_sem: pl::VkSemaphore,
    vk_ext: ExternalSemaphore,
    cu_ext: ExternalSemaphore,
    vk_val: u64,
    cu_val: u64,
    /// Next frame whose flows aren't queued yet.
    next: u64,
    /// Wall time queueing NVOF and CUDA work, ns.
    wall_ns: u64,
}

impl Nvof {
    /// Holds `tex`, which signals `vk_sem` once Vulkan is done with it: returns its layout and
    /// the value.
    fn hold(&mut self, gpu: pl::pl_gpu, tex: pl::pl_tex) -> Result<(pl::VkImageLayout, u64)> {
        self.vk_val += 1;
        let mut layout = pl::VkImageLayout::VK_IMAGE_LAYOUT_UNDEFINED;
        let params = pl::pl_vulkan_hold_params {
            tex,
            layout,
            out_layout: &raw mut layout,
            qf: QF_IGNORED,
            semaphore: pl::pl_vulkan_sem {
                sem: self.vk_sem,
                value: self.vk_val,
            },
        };
        // SAFETY: our texture, not held; the semaphore is ours.
        if !unsafe { pl::pl_vulkan_hold_ex(gpu, &raw const params) } {
            return Err(gpu_err("denoise: pl_vulkan_hold_ex failed"));
        }
        Ok((layout, self.vk_val))
    }

    /// Hands a held `tex` back to libplacebo, which waits for `cu_sem` to reach `value` before
    /// its next use.
    fn release(&self, gpu: pl::pl_gpu, tex: pl::pl_tex, layout: pl::VkImageLayout, value: u64) {
        let params = pl::pl_vulkan_release_params {
            tex,
            layout,
            qf: QF_IGNORED,
            semaphore: pl::pl_vulkan_sem {
                sem: self.cu_sem,
                value,
            },
        };
        // SAFETY: held by `hold`; the semaphore lives until the denoiser's drop, after
        // `pl_gpu_finish`.
        unsafe { pl::pl_vulkan_release_ex(gpu, &raw const params) };
    }

    /// Queues the next `cu_sem` signal on the stream (needs the context current); returns its
    /// value.
    fn signal(&mut self) -> Result<u64> {
        self.cu_val += 1;
        Cuda::get()?.signal_async(&self.cu_ext, self.cu_val, &self.stream)?;
        Ok(self.cu_val)
    }
}

/// libplacebo objects being made for [`Nvof`], destroyed on drop unless kept.
struct Res {
    gpu: pl::pl_gpu,
    texs: Vec<pl::pl_tex>,
    bufs: Vec<pl::pl_buf>,
    sems: Vec<pl::VkSemaphore>,
}

impl Res {
    fn new(gpu: pl::pl_gpu) -> Self {
        Self {
            gpu,
            texs: Vec::new(),
            bufs: Vec::new(),
            sems: Vec::new(),
        }
    }

    fn tex(&mut self, params: pl::pl_tex_params) -> Result<pl::pl_tex> {
        if params.format.is_null() {
            return Err(gpu_err("denoise: NVOF texture format"));
        }
        // SAFETY: valid params.
        let t = unsafe { pl::pl_tex_create(self.gpu, &raw const params) };
        if t.is_null() {
            return Err(gpu_err("denoise: NVOF texture"));
        }
        self.texs.push(t);
        Ok(t)
    }

    /// A buffer exported as an fd and mapped into CUDA (`storable` for a shader to read it).
    fn shared_buf(
        &mut self,
        ctx: CuContext,
        size: usize,
        storable: bool,
    ) -> Result<(pl::pl_buf, ExternalBuffer)> {
        let bp = pl::pl_buf_params {
            size,
            storable,
            export_handle: pl::pl_handle_type::PL_HANDLE_FD,
            ..Default::default()
        };
        // SAFETY: valid params.
        let buf = unsafe { pl::pl_buf_create(self.gpu, &raw const bp) };
        if buf.is_null() {
            return Err(gpu_err("denoise: exportable buffer"));
        }
        self.bufs.push(buf);
        let cuda = Cuda::get()?;
        let _g = cuda.push(ctx)?;
        // SAFETY: an exported fd buffer; CUDA gets its own dup of the fd.
        unsafe {
            let sm = &(*buf).shared_mem;
            let fd = libc_dup(sm.handle.fd);
            let ext = cuda.import_fd(fd, sm.size as u64 - sm.offset as u64, sm.offset as u64);
            Ok((buf, ext?))
        }
    }

    /// A timeline semaphore, exported as an fd and imported by CUDA.
    fn semaphore(&mut self, ctx: CuContext) -> Result<(pl::VkSemaphore, ExternalSemaphore)> {
        let mut h = pl::pl_handle::default();
        let params = pl::pl_vulkan_sem_params {
            type_: pl::VkSemaphoreType::VK_SEMAPHORE_TYPE_TIMELINE,
            initial_value: 0,
            export_handle: pl::pl_handle_type::PL_HANDLE_FD,
            out_handle: &raw mut h,
            ..Default::default()
        };
        // SAFETY: valid params; we own the exported fd.
        let sem = unsafe { pl::pl_vulkan_sem_create(self.gpu, &raw const params) };
        if sem.is_null() {
            return Err(gpu_err("denoise: exportable timeline semaphore"));
        }
        self.sems.push(sem);
        let cuda = Cuda::get()?;
        let _g = cuda.push(ctx)?;
        // SAFETY: a Vulkan timeline semaphore's fd on this device; CUDA owns it on success.
        unsafe {
            let fd = h.fd;
            cuda.import_semaphore_fd(fd)
                .inspect_err(|_| {
                    libc_close(fd);
                })
                .map(|e| (sem, e))
        }
    }

    /// Keeps everything made so far (the caller destroys it).
    fn keep(&mut self) {
        self.texs.clear();
        self.bufs.clear();
        self.sems.clear();
    }
}

impl Drop for Res {
    fn drop(&mut self) {
        // SAFETY: ours and unused (CUDA's imports are dropped before this).
        unsafe {
            for t in &mut self.texs {
                pl::pl_tex_destroy(self.gpu, t);
            }
            for b in &mut self.bufs {
                pl::pl_buf_destroy(self.gpu, b);
            }
            for s in &mut self.sems {
                pl::pl_vulkan_sem_destroy(self.gpu, s);
            }
        }
    }
}

/// Unpacks `count` regions of NVOF's S10.5 `[x, y]` shorts (`fl`, one `uint` per vector, `gw x
/// gh` each) into `f0..`, in luma pixels (the flow is at half resolution, so / 16).
fn flow_kernel(count: usize, gw: u32, gh: u32) -> Compute {
    let mut body = format!(
        "
    ivec2 b = ivec2(gl_GlobalInvocationID.xy);
    if (all(lessThan(b, ivec2({gw}, {gh})))) {{
        uint i = uint(b.y * {gw} + b.x);
"
    );
    for m in 0..count {
        let off = m * gw as usize * gh as usize;
        let _ = write!(
            body,
            "        {{
            uint v = fl[{off}u + i];
            vec2 f = vec2(ivec2(int(v << 16) >> 16, int(v) >> 16)) / 16.0;
            imageStore(f{m}, b, vec4(f, 0.0, 1.0));
        }}
"
        );
    }
    body += "    }\n";
    Compute {
        header: String::new(),
        body,
        shmem: 0,
    }
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
    sched: Schedule,
    last_dovi: Option<Box<pl::pl_dovi_metadata>>,
    /// Frames pushed so far.
    pushed: u64,
    /// One timer per dispatch ordinal in a frame, and what that dispatch is.
    timers: Vec<(pl::pl_timer, &'static str)>,
    /// Summed GPU time (ns) per kernel kind, and frames denoised.
    gpu_ns: Vec<(&'static str, u64)>,
    frames: u64,
    /// Wall time in `push` and `pop`, ns.
    wall_ns: u64,
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
const F: [&CStr; 10] = [
    c"f0", c"f1", c"f2", c"f3", c"f4", c"f5", c"f6", c"f7", c"f8", c"f9",
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
        let cfg = cfg.clamped();
        Ok(Self {
            gpu,
            dp,
            cfg,
            layout: None,
            ring: Vec::new(),
            motion: Vec::new(),
            merged: Vec::new(),
            out: Vec::new(),
            pending: VecDeque::new(),
            cuda,
            nvof: None,
            sched: Schedule {
                radius: u64::from(cfg.radius()),
                lag: 0,
            },
            last_dovi: None,
            pushed: 0,
            timers: Vec::new(),
            gpu_ns: Vec::new(),
            frames: 0,
            wall_ns: 0,
        })
    }

    fn radius(&self) -> u64 {
        self.sched.radius
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
        let want_nvof = self.cfg.temporal.is_some_and(|t| t.motion == Motion::Nvof);
        if want_nvof {
            match self.init_nvof(lw, lh) {
                Ok(n) => {
                    self.nvof = Some(n);
                    self.sched.lag = 1;
                }
                Err(e) => eprintln!("denoise: NVOF unavailable ({e}); using block matching"),
            }
        }
        for _ in 0..self.sched.slots() {
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
        let cuda = Cuda::get()?;
        let (w2, h2) = ((lw + 1) / 2, (lh + 1) / 2);
        let (uw, uh) = (
            u32::try_from(w2).unwrap_or(0),
            u32::try_from(h2).unwrap_or(0),
        );
        let sched = Schedule {
            lag: 1,
            ..self.sched
        };
        let slots = usize::try_from(sched.slots()).unwrap_or(4);
        // SAFETY: ffmpeg's CUDA context on this GPU, alive for the whole run.
        let of = unsafe { OpticalFlow::with_slots(ctx, uw, uh, 4, PerfLevel::Medium, slots)? };
        let stream = cuda.stream(ctx)?;
        of.set_stream(&stream)?;
        // Everything below is ours until `Nvof` owns it; `res` frees it on the error paths.
        let mut res = Res::new(self.gpu);
        let luma8 = res.tex(pl::pl_tex_params {
            w: w2,
            h: h2,
            // SAFETY: valid gpu and name.
            format: unsafe { pl::pl_find_named_fmt(self.gpu, c"r8".as_ptr()) },
            storable: true,
            host_readable: true,
            ..Default::default()
        })?;
        let pitch = usize::try_from(w2).unwrap_or(0).next_multiple_of(256);
        let (buf, ext) = res.shared_buf(ctx, pitch * usize::try_from(h2).unwrap_or(0), false)?;
        let (gw, gh) = (of.grid_w as usize, of.grid_h as usize);
        let n = usize::try_from(2 * self.radius()).unwrap_or(0);
        let mut flow_buf = [ptr::null(); 2];
        let mut flow_ext = Vec::new();
        let mut flow_tex = [Vec::new(), Vec::new()];
        for p in 0..2 {
            let (b, e) = res.shared_buf(ctx, n * gw * gh * 4, true)?;
            flow_buf[p] = b;
            flow_ext.push(e);
            for _ in 0..n {
                flow_tex[p].push(res.tex(pl::pl_tex_params {
                    w: i32::try_from(gw).unwrap_or(0),
                    h: i32::try_from(gh).unwrap_or(0),
                    // SAFETY: valid gpu and name.
                    format: unsafe { pl::pl_find_named_fmt(self.gpu, c"rg32f".as_ptr()) },
                    sampleable: true,
                    storable: true,
                    ..Default::default()
                })?);
            }
        }
        let (vk_sem, vk_ext) = res.semaphore(ctx)?;
        let (cu_sem, cu_ext) = res.semaphore(ctx)?;
        res.keep();
        let mut nv = Nvof {
            of,
            stream,
            luma8,
            buf,
            ext,
            pitch,
            ctx,
            flow_buf,
            flow_ext,
            flow_tex,
            flow_layout: [pl::VkImageLayout::VK_IMAGE_LAYOUT_UNDEFINED; 2],
            set_free: [0; 2],
            set_ready: [0; 2],
            vk_sem,
            cu_sem,
            vk_ext,
            cu_ext,
            vk_val: 0,
            cu_val: 0,
            next: 0,
            wall_ns: 0,
        };
        // Each set's first texture stays held while CUDA may write the set (from here on).
        for p in 0..2 {
            let (layout, v) = nv.hold(self.gpu, nv.flow_tex[p][0])?;
            nv.flow_layout[p] = layout;
            nv.set_free[p] = v;
        }
        Ok(nv)
    }

    /// Renders slot `s`'s half-res 8-bit luma and queues its copy into NVOF input slot `s`.
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
        // SAFETY: our texture and buffer.
        unsafe {
            if !pl::pl_tex_download(self.gpu, &raw const tp) || !pl::pl_buf_export(self.gpu, buf) {
                return Err(gpu_err("denoise: luma export failed"));
            }
        }
        let gpu = self.gpu;
        let nv = self.nvof.as_mut().ok_or_else(|| gpu_err("nvof"))?;
        let (layout, held) = nv.hold(gpu, luma8)?;
        let cuda = Cuda::get()?;
        let _g = cuda.push(nv.ctx)?;
        cuda.wait_async(&nv.vk_ext, held, &nv.stream)?;
        // SAFETY: the mapped buffer holds h rows of `pitch` bytes; `luma8` stays held (so `buf`
        // unwritten) until the copy is done, by the release below.
        unsafe {
            nv.of
                .upload_async(slot, Mem::Device(nv.ext.ptr), pitch, &nv.stream)?;
        };
        let copied = nv.signal()?;
        nv.release(gpu, luma8, layout, copied);
        nv.wall_ns += u64::try_from(t0.elapsed().as_nanos()).unwrap_or(0);
        Ok(())
    }

    /// Queues the NVOF flows of the frames below `upto` not queued yet.
    fn nvof_queue(&mut self, upto: u64) -> Result<()> {
        let (sched, pushed) = (self.sched, self.pushed);
        let Some(nv) = self.nvof.as_mut() else {
            return Ok(());
        };
        let t0 = std::time::Instant::now();
        let cuda = Cuda::get()?;
        let _g = cuda.push(nv.ctx)?;
        while nv.next < upto.min(pushed) {
            let c = nv.next;
            let p = usize::from(c % 2 == 1);
            // The set's previous frame (c - 2) has been read by Vulkan.
            cuda.wait_async(&nv.vk_ext, nv.set_free[p], &nv.stream)?;
            let (gw, gh) = (nv.of.grid_w as usize, nv.of.grid_h as usize);
            let (src, src_pitch) = nv.of.flow_output();
            for (f, m) in sched.neighbours(c, pushed) {
                nv.of.execute(sched.slot(c), sched.slot(f), false)?;
                // SAFETY: the set holds 2r regions of gw x gh shorts pairs; the stream orders the
                // copy after the execute and before the next one.
                unsafe {
                    cuda.copy_2d_async(
                        (Mem::Device(src), src_pitch),
                        (
                            Mem::Device(nv.flow_ext[p].ptr + (m * gw * gh * 4) as u64),
                            gw * 4,
                        ),
                        gw * 4,
                        gh,
                        &nv.stream,
                    )?;
                }
            }
            nv.set_ready[p] = nv.signal()?;
            nv.next += 1;
        }
        nv.wall_ns += u64::try_from(t0.elapsed().as_nanos()).unwrap_or(0);
        Ok(())
    }

    /// Converts frame `c`'s NVOF flows into `flow_tex` (luma pixels), once CUDA has written them.
    /// Returns the set; [`Self::nvof_done`] hands it back to CUDA after the confidence passes.
    fn nvof_flow(&mut self, c: u64, n: usize) -> Result<usize> {
        self.nvof_queue(c + 1)?;
        let gpu = self.gpu;
        let nv = self.nvof.as_mut().ok_or_else(|| gpu_err("nvof"))?;
        let set = usize::from(c % 2 == 1);
        nv.release(
            gpu,
            nv.flow_tex[set][0],
            nv.flow_layout[set],
            nv.set_ready[set],
        );
        let (gw, gh) = (nv.of.grid_w, nv.of.grid_h);
        let count = nv.flow_tex[set].len();
        let var = pl::pl_var {
            name: c"fl".as_ptr(),
            type_: pl::pl_var_type::PL_VAR_UINT,
            dim_v: 1,
            dim_m: 1,
            dim_a: i32::try_from(count * (gw * gh) as usize).unwrap_or(0),
        };
        let mut bv = pl::pl_buffer_var {
            var,
            // SAFETY: plain computation on a valid var.
            layout: unsafe { pl::pl_std430_layout(0, &raw const var) },
        };
        let mut descs = vec![pl::pl_shader_desc {
            desc: pl::pl_desc {
                name: c"flowbuf".as_ptr(),
                type_: pl::pl_desc_type::PL_DESC_BUF_STORAGE,
                access: pl::pl_desc_access::PL_DESC_ACCESS_READONLY,
                ..Default::default()
            },
            binding: pl::pl_desc_binding {
                object: nv.flow_buf[set].cast::<c_void>(),
                ..Default::default()
            },
            buffer_vars: &raw mut bv,
            num_buffer_vars: 1,
            ..Default::default()
        }];
        for (m, &t) in nv.flow_tex[set].iter().enumerate() {
            let mut sd = storage(t);
            sd.desc.name = F[m].as_ptr();
            descs.push(sd);
        }
        let kern = flow_kernel(count, gw, gh);
        let grid = groups(
            i32::try_from(gw).unwrap_or(0),
            i32::try_from(gh).unwrap_or(0),
        );
        self.dispatch(n, "flow", &kern, &descs, grid)?;
        Ok(set)
    }

    /// Frame `c`'s flows (set `p`) are read: CUDA may rewrite the set once Vulkan gets here.
    fn nvof_done(&mut self, p: usize) -> Result<()> {
        let gpu = self.gpu;
        let nv = self.nvof.as_mut().ok_or_else(|| gpu_err("nvof"))?;
        // SAFETY: our buffer.
        if !unsafe { pl::pl_buf_export(gpu, nv.flow_buf[p]) } {
            return Err(gpu_err("denoise: flow export failed"));
        }
        let (layout, v) = nv.hold(gpu, nv.flow_tex[p][0])?;
        nv.flow_layout[p] = layout;
        nv.set_free[p] = v;
        Ok(())
    }

    /// NVOF flow from slot `sc` to slot `sf` (in `flow`) into `self.motion[m]`, scored for
    /// confidence.
    fn nvof_motion(
        &mut self,
        flow: pl::pl_tex,
        (sc, sf): (usize, usize),
        m: usize,
        luma: usize,
        n: usize,
    ) -> Result<()> {
        let norm = self.layout.as_ref().map_or(1.0, |l| l.norm);
        let (mw, mh) = size(self.motion[m]);
        let d = [
            sampled(c"flow", flow, false),
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
        let t0 = std::time::Instant::now();
        let r = self.push_inner(up);
        self.wall_ns += u64::try_from(t0.elapsed().as_nanos()).unwrap_or(0);
        r
    }

    fn push_inner(&mut self, up: &Frame) -> Result<()> {
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
        // Flows of the frame whose last neighbour just came in.
        self.nvof_queue(self.sched.nvof_upto(self.pushed))
    }

    fn ingest(&mut self, image: &pl::pl_frame) -> Result<()> {
        if self.layout.is_none() {
            self.init(image)?;
        }
        let layout = self
            .layout
            .clone()
            .ok_or_else(|| gpu_err("denoise: no layout"))?;
        let s = self.sched.slot(self.pushed);
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
        let t0 = std::time::Instant::now();
        let r = self.pop_inner(flush);
        self.wall_ns += u64::try_from(t0.elapsed().as_nanos()).unwrap_or(0);
        r
    }

    fn pop_inner(&mut self, flush: bool) -> Result<Option<(Frame, pl::pl_frame)>> {
        let Some(front) = self.pending.front() else {
            return Ok(None);
        };
        if !self.sched.ready(front.index, self.pushed, flush) {
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
        let sched = self.sched;
        let slot = |f: u64| sched.slot(f);
        let sc = slot(c);
        let mut n = layout.planes.len() + 2;
        let mut src: Vec<pl::pl_tex> = self.ring[sc].planes.clone();

        if let Some(t) = self.cfg.temporal {
            let set = if self.nvof.is_some() {
                n += 1;
                Some(self.nvof_flow(c, n - 1)?)
            } else {
                None
            };
            // Neighbours by distance, each seeded by the vector to the nearer one on its side.
            let mut nb: Vec<(u64, usize)> = Vec::new();
            for (f, m) in sched.neighbours(c, self.pushed) {
                let d = c.abs_diff(f);
                if let Some(p) = set {
                    let flow = self.nvof.as_ref().map(|nv| nv.flow_tex[p][m]);
                    let flow = flow.ok_or_else(|| gpu_err("nvof"))?;
                    self.nvof_motion(flow, (sc, slot(f)), m, layout.luma, n)?;
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
            if let Some(p) = set {
                self.nvof_done(p)?;
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
                .map(|n| format!(", NVOF queueing {:.2}", ms(n.wall_ns)))
                .unwrap_or_default();
            eprintln!(
                "denoise: {} frames, {:.2} ms GPU/frame ({}), {:.2} ms wall/frame{nvof}, \
                 motion {} {:?}",
                self.frames,
                ms(total),
                parts.join(", "),
                ms(self.wall_ns),
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
            // Nothing of ours in flight on either side before freeing what they share.
            let _ = n.stream.synchronize();
            // SAFETY: valid gpu.
            unsafe { pl::pl_gpu_finish(gpu) };
            all.push(n.luma8);
            all.extend(n.flow_tex.iter().flatten());
            // SAFETY: ours; CUDA's imports are released with `n` after this.
            unsafe {
                let [a, b] = &mut n.flow_buf;
                for b in [&mut n.buf, a, b] {
                    pl::pl_buf_destroy(gpu, b);
                }
                pl::pl_vulkan_sem_destroy(gpu, &raw mut n.vk_sem);
                pl::pl_vulkan_sem_destroy(gpu, &raw mut n.cu_sem);
            }
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
    #[link_name = "close"]
    fn libc_close(fd: i32) -> i32;
}

/// `dup(2)`: CUDA takes ownership of the fd it imports, libplacebo keeps its own.
unsafe fn libc_dup(fd: i32) -> i32 {
    // SAFETY: plain syscall wrapper.
    unsafe { libc_dup_raw(fd) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives a [`Schedule`] like the render thread (push, pop while ready, flush) and checks the
    /// NVOF pipeline's invariants: every frame's flows are queued once, after all its neighbours
    /// are in and before it renders; a flow set isn't rewritten before the frame that last used it
    /// has rendered; and no ring slot a rendering frame reads has been overwritten.
    type Sets = [Option<(u64, Vec<(u64, usize)>)>; 2];

    fn simulate(radius: u64, lag: u64, frames: u64) {
        let s = Schedule { radius, lag };
        let slots = usize::try_from(s.slots()).unwrap();
        let mut ring: Vec<Option<u64>> = vec![None; slots];
        let (mut pushed, mut queued, mut next) = (0, 0, 0);
        // Per flow set: the frame whose flows it holds (and their neighbours), until it renders.
        let mut sets: Sets = [None, None];
        let mut rendered = Vec::new();
        let queue = |upto: u64, pushed: u64, queued: &mut u64, sets: &mut Sets| {
            while *queued < upto.min(pushed) {
                let c = *queued;
                let p = usize::from(c % 2 == 1);
                assert!(
                    sets[p].is_none(),
                    "set {p} rewritten for {c} before it rendered"
                );
                sets[p] = Some((c, s.neighbours(c, pushed)));
                *queued += 1;
            }
        };
        let mut render = |c: u64, pushed: u64, ring: &[Option<u64>], sets: &mut Sets| {
            let p = usize::from(c % 2 == 1);
            if lag > 0 {
                // Same neighbours as when its flows were queued.
                assert_eq!(sets[p].take(), Some((c, s.neighbours(c, pushed))));
            }
            assert_eq!(ring[s.slot(c)], Some(c));
            for (f, m) in s.neighbours(c, pushed) {
                assert_eq!(
                    ring[s.slot(f)],
                    Some(f),
                    "frame {c}: neighbour {f} overwritten"
                );
                assert!((m as u64) < 2 * radius && c.abs_diff(f) <= radius);
            }
            rendered.push(c);
        };
        for f in 0..frames {
            ring[s.slot(f)] = Some(f);
            pushed += 1;
            if lag > 0 {
                queue(s.nvof_upto(pushed), pushed, &mut queued, &mut sets);
            }
            while next < pushed && s.ready(next, pushed, false) {
                if lag > 0 {
                    queue(next + 1, pushed, &mut queued, &mut sets);
                }
                render(next, pushed, &ring, &mut sets);
                next += 1;
            }
        }
        while next < pushed {
            if lag > 0 {
                queue(next + 1, pushed, &mut queued, &mut sets);
            }
            render(next, pushed, &ring, &mut sets);
            next += 1;
        }
        assert_eq!(rendered, (0..frames).collect::<Vec<_>>());
        if lag > 0 {
            assert_eq!(queued, frames);
        }
    }

    #[test]
    fn schedule_invariants() {
        for radius in 1..=5 {
            for lag in 0..=1 {
                for frames in 0..24 {
                    simulate(radius, lag, frames);
                }
            }
        }
    }

    #[test]
    fn schedule_lag_renders_one_frame_later() {
        let s = Schedule { radius: 2, lag: 1 };
        assert_eq!(s.slots(), 6);
        // Frame 0 waits for frames 0..=3 (its lookahead plus the frame NVOF runs ahead).
        assert!(!s.ready(0, 3, false));
        assert!(s.ready(0, 4, false));
        assert!(s.ready(0, 1, true));
        // Its flows can be queued as soon as frame 2 is in.
        assert_eq!(s.nvof_upto(3), 1);
        assert_eq!(s.nvof_upto(1), 0);
        let blocks = Schedule { radius: 2, lag: 0 };
        assert_eq!(blocks.slots(), 5);
        assert!(blocks.ready(0, 3, false));
    }

    #[test]
    fn neighbours_by_distance() {
        let s = Schedule { radius: 2, lag: 1 };
        assert_eq!(s.neighbours(5, 10), vec![(4, 0), (6, 1), (3, 2), (7, 3)]);
        assert_eq!(s.neighbours(0, 2), vec![(1, 1)]);
        assert_eq!(s.neighbours(1, 2), vec![(0, 0)]);
    }

    #[test]
    fn flow_kernel_unpacks_every_set_region() {
        let k = flow_kernel(4, 10, 3);
        for m in 0..4 {
            assert!(k.body.contains(&format!("imageStore(f{m}, b")));
            assert!(k.body.contains(&format!("fl[{}u + i]", m * 30)));
        }
        assert!(!k.body.contains("f4"));
    }

    /// The kernel's S10.5 unpacking, on the CPU: low 16 bits are x, high are y, sign-extended.
    #[test]
    fn s10_5_unpacking_matches_host_path() {
        for (x, y) in [(0_i16, 0_i16), (-1, 1), (i16::MIN, i16::MAX), (37, -1234)] {
            let v = u32::from(x.cast_unsigned()) | (u32::from(y.cast_unsigned()) << 16);
            let (ux, uy) = ((v << 16).cast_signed() >> 16, v.cast_signed() >> 16);
            #[allow(clippy::cast_precision_loss)]
            let got = [ux as f32 / 16.0, uy as f32 / 16.0];
            assert_eq!(got, [f32::from(x) / 16.0, f32::from(y) / 16.0]);
        }
    }
}
