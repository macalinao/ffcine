//! libplacebo (Vulkan): the processing half of the ffmpeg backend. Safe-ish owners for the
//! libplacebo objects the grade pass needs, and the mapping from [`Grade`] to `pl_render_params`.

use std::ffi::CString;
use std::path::Path;
use std::ptr;

use libplacebo_sys as pl;

use crate::grade::{Deband, DitherMethod, Grade, Scaler, ToneMapping};
use crate::{Error, Result};
use ffcine_core::lut::{Lut, LutType};

/// The libplacebo API version ffcine was built against (`PL_API_VER`).
pub const fn api_version() -> u32 {
    pl::PL_API_VER
}

fn err(msg: impl Into<String>) -> Error {
    Error::Pipeline(msg.into())
}

/// A `pl_log` printing to stderr (`FFCINE_PL_LOG=debug|info|warn|err` sets the level).
#[derive(Debug)]
pub(crate) struct Log(pub pl::pl_log);

// SAFETY: pl_log is thread-safe.
unsafe impl Send for Log {}
unsafe impl Sync for Log {}

impl Log {
    pub(crate) fn new() -> Self {
        use pl::pl_log_level as L;
        let level = match std::env::var("FFCINE_PL_LOG").as_deref() {
            Ok("trace") => L::PL_LOG_TRACE,
            Ok("debug") => L::PL_LOG_DEBUG,
            Ok("info") => L::PL_LOG_INFO,
            Ok("err") => L::PL_LOG_ERR,
            _ => L::PL_LOG_WARN,
        };
        let params = pl::pl_log_params {
            log_cb: Some(pl::pl_log_simple),
            log_priv: ptr::null_mut(),
            log_level: level,
        };
        #[allow(clippy::cast_possible_wrap)]
        // SAFETY: valid params; libplacebo copies them.
        Self(unsafe { pl::pl_log_create_375(pl::PL_API_VER as i32, &raw const params) })
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        // SAFETY: we own the log.
        unsafe { pl::pl_log_destroy(&raw mut self.0) };
    }
}

/// libplacebo on ffmpeg's `VkDevice` (`pl_vulkan_import`), so `AVVkFrame`s map without copies.
#[derive(Debug)]
pub(crate) struct Vulkan {
    pub vk: pl::pl_vulkan,
    pub gpu: pl::pl_gpu,
}

// SAFETY: pl_gpu is thread-safe; the device outlives it (held by the caller).
unsafe impl Send for Vulkan {}
unsafe impl Sync for Vulkan {}

const VK_QUEUE_FAMILY_IGNORED: u32 = !0;
const VK_QUEUE_GRAPHICS_BIT: u32 = 1;
const VK_QUEUE_COMPUTE_BIT: u32 = 2;
const VK_QUEUE_TRANSFER_BIT: u32 = 4;
const VK_API_VERSION_1_3: u32 = (1 << 22) | (3 << 12);

unsafe extern "C" fn lock_queue(ctx: *mut std::os::raw::c_void, qf: u32, qidx: u32) {
    use ffmpeg_next::ffi as ff;
    let dev = ctx.cast::<ff::AVHWDeviceContext>();
    // SAFETY: ctx is the AVHWDeviceContext we passed as queue_ctx.
    unsafe {
        let hw = (*dev).hwctx.cast::<ff::AVVulkanDeviceContext>();
        #[allow(deprecated)]
        if let Some(f) = (*hw).lock_queue {
            f(dev, qf, qidx);
        }
    }
}

unsafe extern "C" fn unlock_queue(ctx: *mut std::os::raw::c_void, qf: u32, qidx: u32) {
    use ffmpeg_next::ffi as ff;
    let dev = ctx.cast::<ff::AVHWDeviceContext>();
    // SAFETY: as in lock_queue.
    unsafe {
        let hw = (*dev).hwctx.cast::<ff::AVVulkanDeviceContext>();
        #[allow(deprecated)]
        if let Some(f) = (*hw).unlock_queue {
            f(dev, qf, qidx);
        }
    }
}

impl Vulkan {
    /// Imports the Vulkan device of an `AVHWDeviceContext` (as libavfilter's `vf_libplacebo`).
    ///
    /// # Safety
    /// `device` must be a Vulkan `AVHWDeviceContext` that outlives the returned object.
    pub(crate) unsafe fn import(
        log: &Log,
        device: *mut ffmpeg_next::ffi::AVHWDeviceContext,
    ) -> Result<Self> {
        use ffmpeg_next::ffi as ff;
        // SAFETY: caller guarantees a Vulkan device context.
        unsafe {
            let hw = &*(*device).hwctx.cast::<ff::AVVulkanDeviceContext>();
            let ignored = pl::pl_vulkan_queue {
                index: VK_QUEUE_FAMILY_IGNORED,
                count: 0,
                flags: 0,
            };
            let mut p = pl::pl_vulkan_import_params {
                instance: hw.inst.cast(),
                get_proc_addr: std::mem::transmute::<
                    ff::PFN_vkGetInstanceProcAddr,
                    pl::PFN_vkGetInstanceProcAddr,
                >(hw.get_proc_addr),
                phys_device: hw.phys_dev.cast(),
                device: hw.act_dev.cast(),
                extensions: hw.enabled_dev_extensions,
                num_extensions: hw.nb_enabled_dev_extensions,
                queue_graphics: ignored,
                queue_compute: ignored,
                queue_transfer: ignored,
                features: (&raw const hw.device_features).cast(),
                lock_queue: Some(lock_queue),
                unlock_queue: Some(unlock_queue),
                queue_ctx: device.cast(),
                max_api_version: VK_API_VERSION_1_3,
                ..Default::default()
            };
            let nb = usize::try_from(hw.nb_qf).unwrap_or(0);
            for qf in &hw.qf[..nb.min(hw.qf.len())] {
                let q = pl::pl_vulkan_queue {
                    index: u32::try_from(qf.idx).unwrap_or(VK_QUEUE_FAMILY_IGNORED),
                    count: u32::try_from(qf.num).unwrap_or(0),
                    flags: hw.queue_flags,
                };
                if qf.flags & VK_QUEUE_GRAPHICS_BIT != 0 {
                    p.queue_graphics = q;
                }
                if qf.flags & VK_QUEUE_COMPUTE_BIT != 0 {
                    p.queue_compute = q;
                }
                if qf.flags & VK_QUEUE_TRANSFER_BIT != 0 {
                    p.queue_transfer = q;
                }
            }
            let vk = pl::pl_vulkan_import(log.0, &raw const p);
            if vk.is_null() {
                return Err(err("pl_vulkan_import failed"));
            }
            Ok(Self { vk, gpu: (*vk).gpu })
        }
    }
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        // SAFETY: we own the pl_vulkan (not the VkDevice, which ffmpeg owns).
        unsafe { pl::pl_vulkan_destroy(&raw mut self.vk) };
    }
}

/// A shader/pipeline cache persisted as one file per object under a directory.
#[derive(Debug)]
pub(crate) struct Cache {
    pub cache: pl::pl_cache,
    _dir: Box<CString>,
}

// SAFETY: pl_cache is thread-safe; the dir string is immutable.
unsafe impl Send for Cache {}
unsafe impl Sync for Cache {}

impl Cache {
    pub(crate) fn new(log: &Log, dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
        // pl_cache_{get,set}_file append the hex key to this string as is.
        let mut d = dir.to_string_lossy().into_owned();
        if !d.ends_with('/') {
            d.push('/');
        }
        let d = Box::new(super::av::cstr(&d)?);
        let params = pl::pl_cache_params {
            log: log.0,
            max_object_size: 0,
            max_total_size: 0,
            set: Some(pl::pl_cache_set_file),
            get: Some(pl::pl_cache_get_file),
            priv_: d.as_ptr().cast_mut().cast(),
        };
        // SAFETY: valid params; `d` lives as long as the cache.
        let cache = unsafe { pl::pl_cache_create(&raw const params) };
        if cache.is_null() {
            return Err(err("pl_cache_create failed"));
        }
        Ok(Self { cache, _dir: d })
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        // SAFETY: we own the cache.
        unsafe { pl::pl_cache_destroy(&raw mut self.cache) };
    }
}

/// A `pl_renderer` (keeps scaler/LUT/shader state across frames and shots).
#[derive(Debug)]
pub(crate) struct Renderer(pub pl::pl_renderer);

// SAFETY: used from one thread at a time.
unsafe impl Send for Renderer {}

impl Renderer {
    pub(crate) fn new(log: &Log, vk: &Vulkan) -> Result<Self> {
        // SAFETY: valid log and gpu.
        let rr = unsafe { pl::pl_renderer_create(log.0, vk.gpu) };
        if rr.is_null() {
            return Err(err("pl_renderer_create failed"));
        }
        Ok(Self(rr))
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: we own the renderer.
        unsafe { pl::pl_renderer_destroy(&raw mut self.0) };
    }
}

/// An [`Overlay`](crate::Overlay)'s mask as a sampleable `r8` texture.
#[derive(Debug)]
pub(crate) struct OverlayTex {
    gpu: pl::pl_gpu,
    pub tex: pl::pl_tex,
}

// SAFETY: used from one thread at a time.
unsafe impl Send for OverlayTex {}

impl OverlayTex {
    pub(crate) fn new(vk: &Vulkan, mask: &crate::Mask) -> Result<Self> {
        let w = i32::try_from(mask.width).map_err(|_| err("overlay too wide"))?;
        let h = i32::try_from(mask.height).map_err(|_| err("overlay too tall"))?;
        if w == 0 || h == 0 || mask.data.len() != mask.width as usize * mask.height as usize {
            return Err(err("empty or malformed overlay mask"));
        }
        // SAFETY: valid gpu; `initial_data` is `w * h` bytes, copied during the call.
        unsafe {
            let fmt = pl::pl_find_named_fmt(vk.gpu, c"r8".as_ptr());
            if fmt.is_null() {
                return Err(err("no r8 texture format"));
            }
            let params = pl::pl_tex_params {
                w,
                h,
                format: fmt,
                sampleable: true,
                initial_data: mask.data.as_ptr().cast(),
                debug_tag: c"ffcine overlay".as_ptr(),
                ..Default::default()
            };
            let tex = pl::pl_tex_create(vk.gpu, &raw const params);
            if tex.is_null() {
                return Err(err("pl_tex_create (overlay) failed"));
            }
            Ok(Self { gpu: vk.gpu, tex })
        }
    }
}

impl Drop for OverlayTex {
    fn drop(&mut self) {
        // SAFETY: we own the texture; the gpu outlives it.
        unsafe { pl::pl_tex_destroy(self.gpu, &raw mut self.tex) };
    }
}

/// An RGBA texture: a layer's render target (`rgba16f`, renderable and sampleable) or a still
/// image (`rgba8`, uploaded once).
#[derive(Debug)]
pub(crate) struct RgbaTex {
    gpu: pl::pl_gpu,
    pub tex: pl::pl_tex,
    pub width: u32,
    pub height: u32,
}

// SAFETY: used from one thread at a time.
unsafe impl Send for RgbaTex {}

impl RgbaTex {
    /// A `rgba16f` render target.
    pub(crate) fn target(vk: &Vulkan, width: u32, height: u32) -> Result<Self> {
        Self::create(vk, c"rgba16f", width, height, true, None)
    }

    /// A `rgba8` texture holding `image`.
    pub(crate) fn image(vk: &Vulkan, image: &crate::Image) -> Result<Self> {
        if image.data.len() != image.width as usize * image.height as usize * 4 {
            return Err(err("malformed RGBA image"));
        }
        Self::create(
            vk,
            c"rgba8",
            image.width,
            image.height,
            false,
            Some(&image.data),
        )
    }

    fn create(
        vk: &Vulkan,
        fmt: &std::ffi::CStr,
        width: u32,
        height: u32,
        renderable: bool,
        data: Option<&[u8]>,
    ) -> Result<Self> {
        let w = i32::try_from(width).map_err(|_| err("layer too wide"))?;
        let h = i32::try_from(height).map_err(|_| err("layer too tall"))?;
        if w == 0 || h == 0 {
            return Err(err("empty layer"));
        }
        // SAFETY: valid gpu; `initial_data` (if any) is `w * h * 4` bytes, copied during the call.
        unsafe {
            let f = pl::pl_find_named_fmt(vk.gpu, fmt.as_ptr());
            if f.is_null() {
                return Err(err(format!("no {} texture format", fmt.to_string_lossy())));
            }
            let params = pl::pl_tex_params {
                w,
                h,
                format: f,
                sampleable: true,
                renderable,
                blit_dst: renderable,
                initial_data: data.map_or(ptr::null(), |d| d.as_ptr().cast()),
                debug_tag: c"ffcine layer".as_ptr(),
                ..Default::default()
            };
            let tex = pl::pl_tex_create(vk.gpu, &raw const params);
            if tex.is_null() {
                return Err(err("pl_tex_create (layer) failed"));
            }
            Ok(Self {
                gpu: vk.gpu,
                tex,
                width,
                height,
            })
        }
    }

    /// The texture as a full-range RGB frame with independent alpha in `color`, cropped to the
    /// whole texture.
    pub(crate) fn frame(&self, color: pl::pl_color_space) -> pl::pl_frame {
        let mut f = pl::pl_frame {
            num_planes: 1,
            // SAFETY: reading libplacebo's exported constant.
            repr: unsafe { pl::pl_color_repr_rgb },
            color,
            ..Default::default()
        };
        f.repr.alpha = pl::pl_alpha_mode::PL_ALPHA_INDEPENDENT;
        f.planes[0].texture = self.tex;
        f.planes[0].components = 4;
        f.planes[0].component_mapping = [0, 1, 2, 3];
        #[allow(clippy::cast_precision_loss)]
        {
            f.crop = pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: self.width as f32,
                y1: self.height as f32,
            };
        }
        f
    }

    /// [`Self::frame`] with the alpha channel ignored (an opaque picture).
    pub(crate) fn frame_opaque(&self, color: pl::pl_color_space) -> pl::pl_frame {
        let mut f = self.frame(color);
        f.repr.alpha = pl::pl_alpha_mode::PL_ALPHA_NONE;
        f.planes[0].components = 3;
        f
    }
}

impl Drop for RgbaTex {
    fn drop(&mut self) {
        // SAFETY: we own the texture; the gpu outlives it.
        unsafe { pl::pl_tex_destroy(self.gpu, &raw mut self.tex) };
    }
}

/// A 3D LUT in libplacebo's form, built from our own `.cube` parser's [`Lut`].
#[derive(Debug)]
pub(crate) struct CustomLut {
    pub lut: pl::pl_custom_lut,
    data: Vec<f32>,
}

// SAFETY: immutable after construction.
unsafe impl Send for CustomLut {}
unsafe impl Sync for CustomLut {}

impl CustomLut {
    pub(crate) fn new(lut: &Lut) -> Result<Box<Self>> {
        #[allow(clippy::float_cmp)] // exact: the .cube default domain
        if lut.domain_min != [0.0; 3] || lut.domain_max != [1.0; 3] {
            return Err(Error::Invalid {
                what: "LUT domain (only 0..1 is supported)",
                value: format!("{:?}..{:?}", lut.domain_min, lut.domain_max),
            });
        }
        let data: Vec<f32> = lut.data.iter().flatten().copied().collect();
        let n = i32::try_from(lut.size).map_err(|_| err("LUT too large"))?;
        let mut this = Box::new(Self {
            lut: pl::pl_custom_lut {
                signature: fnv1a(&data),
                size: [n, n, n],
                ..Default::default()
            },
            data,
        });
        this.lut.data = this.data.as_ptr();
        Ok(this)
    }
}

/// FNV-1a over the LUT's bits: the cache signature `pl_lut_parse_cube` would derive from the file.
fn fnv1a(data: &[f32]) -> u64 {
    data.iter()
        .flat_map(|v| v.to_bits().to_le_bytes())
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

pub(crate) fn lut_type(t: LutType) -> pl::pl_lut_type {
    match t {
        LutType::Native => pl::pl_lut_type::PL_LUT_NATIVE,
        LutType::Conversion => pl::pl_lut_type::PL_LUT_CONVERSION,
        _ => pl::pl_lut_type::PL_LUT_NORMALIZED,
    }
}

/// Parsed mpv `.hook` shaders.
#[derive(Debug, Default)]
pub(crate) struct Hooks(Vec<*const pl::pl_hook>);

// SAFETY: parsed hooks are immutable.
unsafe impl Send for Hooks {}

impl Hooks {
    pub(crate) fn parse(vk: &Vulkan, sources: &[(String, String)]) -> Result<Self> {
        let mut hooks = Self::default();
        for (name, text) in sources {
            // SAFETY: valid text for the call's duration; libplacebo copies what it needs.
            let h =
                unsafe { pl::pl_mpv_user_shader_parse(vk.gpu, text.as_ptr().cast(), text.len()) };
            if h.is_null() {
                return Err(err(format!("{name}: failed to parse mpv user shader")));
            }
            hooks.0.push(h);
        }
        Ok(hooks)
    }
}

impl Hooks {
    /// The live value of the first hook parameter called `name`, if any hook has one.
    pub(crate) fn param(&self, name: &str) -> Option<*mut pl::pl_var_data> {
        self.0.iter().find_map(|&h| {
            // SAFETY: parsed hooks with `num_parameters` valid entries and C-string names.
            unsafe {
                let n = usize::try_from((*h).num_parameters).unwrap_or(0);
                if n == 0 {
                    return None;
                }
                std::slice::from_raw_parts((*h).parameters, n)
                    .iter()
                    .find(|p| std::ffi::CStr::from_ptr(p.name).to_bytes() == name.as_bytes())
                    .map(|p| p.data)
            }
        })
    }

    /// Sets `//!PARAM name` of hook `i` (integers rounded). Takes effect on the next render.
    pub(crate) fn set(&self, i: usize, name: &str, v: f64) -> Result<()> {
        let h = *self
            .0
            .get(i)
            .ok_or_else(|| err(format!("no hook #{i} for //!PARAM {name}")))?;
        // SAFETY: the hook came from pl_mpv_user_shader_parse and lives as long as `self`; its
        // parameters' `data` pointers are the live values libplacebo reads at render time, which
        // only happens on this thread, between calls.
        unsafe {
            let n = usize::try_from((*h).num_parameters).unwrap_or(0);
            let pars = if n == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts((*h).parameters, n)
            };
            for p in pars {
                if std::ffi::CStr::from_ptr(p.name).to_bytes() != name.as_bytes() {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                match p.type_ {
                    pl::pl_var_type::PL_VAR_SINT => (*p.data).i = v.round() as i32,
                    pl::pl_var_type::PL_VAR_UINT => (*p.data).u = v.round().max(0.0) as u32,
                    _ => (*p.data).f = v as f32,
                }
                return Ok(());
            }
        }
        Err(err(format!("hook #{i} has no //!PARAM {name}")))
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        for h in &mut self.0 {
            // SAFETY: each hook came from pl_mpv_user_shader_parse.
            unsafe { pl::pl_mpv_user_shader_destroy(h) };
        }
    }
}

/// `pl_render_params` for a [`Grade`], plus the sub-structs it points into.
///
/// Defaults reproduce libavfilter's `libplacebo` filter as the prototype runs it (`upscaler=lanczos:
/// downscaler=lanczos:disable_linear=1:sigmoid=0`, blue-noise dither, peak detection on, contrast
/// recovery 0.3), so the compat render is the prototype's picture.
#[derive(Debug)]
pub(crate) struct RenderParams {
    pub params: pl::pl_render_params,
    color_map: pl::pl_color_map_params,
    peak: pl::pl_peak_detect_params,
    dither: pl::pl_dither_params,
    deband: pl::pl_deband_params,
    sigmoid: pl::pl_sigmoid_params,
    deinterlace: pl::pl_deinterlace_params,
    pub hooks: Hooks,
    hook_ptrs: Vec<*const pl::pl_hook>,
}

// SAFETY: plain data plus immutable hooks.
unsafe impl Send for RenderParams {}

fn scaler(s: Scaler) -> Result<*const pl::pl_filter_config> {
    let name = super::av::cstr(s.as_str())?;
    // SAFETY: valid name; presets are static.
    let p = unsafe { pl::pl_find_filter_preset(name.as_ptr()) };
    if p.is_null() {
        return Err(err(format!("libplacebo has no scaler {s}")));
    }
    // SAFETY: non-null preset.
    Ok(unsafe { (*p).filter })
}

/// A libplacebo tone or gamut mapping function by its libplacebo name.
fn find_fn<T>(
    name: &str,
    find: unsafe extern "C" fn(*const std::os::raw::c_char) -> *const T,
) -> Result<*const T> {
    let c = super::av::cstr(name)?;
    // SAFETY: valid name; the functions are static.
    let f = unsafe { find(c.as_ptr()) };
    if f.is_null() {
        return Err(err(format!("libplacebo has no tone/gamut mapping {name}")));
    }
    Ok(f)
}

impl RenderParams {
    pub(crate) fn new(grade: &Grade, hooks: Hooks) -> Result<Box<Self>> {
        // SAFETY: reading libplacebo's exported default structs.
        let (color_map, sigmoid, deinterlace) = unsafe {
            (
                pl::pl_color_map_default_params,
                pl::pl_sigmoid_default_params,
                pl::pl_deinterlace_default_params,
            )
        };
        let mut this = Box::new(Self {
            params: pl::pl_render_params::default(),
            color_map,
            peak: pl::pl_peak_detect_params {
                smoothing_period: 20.0,
                scene_threshold_low: 1.0,
                scene_threshold_high: 3.0,
                percentile: 99.995,
                black_cutoff: 1.0,
                ..Default::default()
            },
            dither: pl::pl_dither_params {
                method: pl::pl_dither_method::PL_DITHER_BLUE_NOISE,
                lut_size: 6,
                temporal: false,
                ..Default::default()
            },
            deband: pl::pl_deband_params::default(),
            sigmoid,
            deinterlace,
            hook_ptrs: hooks.0.clone(),
            hooks,
        });

        {
            this.color_map.tone_mapping_function = match grade.tone_mapping {
                // pl_tone_map_auto is NULL.
                ToneMapping::Auto => ptr::null(),
                // ffmpeg spells it with a dot, libplacebo without.
                ToneMapping::Bt2390 => &raw const pl::pl_tone_map_bt2390,
                ToneMapping::Bt2446a => &raw const pl::pl_tone_map_bt2446a,
                t => find_fn(t.as_str(), pl::pl_find_tone_map_function)?,
            };
            this.color_map.gamut_mapping =
                find_fn(grade.gamut_mapping.as_str(), pl::pl_find_gamut_map_function)?;
        }
        let t = grade.color_map;
        this.color_map.gamut_constants.perceptual_strength = t.perceptual_strength;
        this.color_map.gamut_constants.perceptual_deadzone = t.perceptual_deadzone;
        this.color_map.lut_size = 256;
        this.color_map.contrast_recovery = t.contrast_recovery;
        this.color_map.contrast_smoothness = 3.5;

        if let Some(d) = grade.dither {
            this.dither.method = match d.method {
                DitherMethod::Ordered => pl::pl_dither_method::PL_DITHER_ORDERED_FIXED,
                DitherMethod::White => pl::pl_dither_method::PL_DITHER_WHITE_NOISE,
                _ => pl::pl_dither_method::PL_DITHER_BLUE_NOISE,
            };
            this.dither.temporal = d.temporal;
        }
        if let Some(Deband {
            iterations,
            threshold,
            radius,
            grain,
        }) = grade.deband
        {
            this.deband = pl::pl_deband_params {
                iterations: i32::try_from(iterations).unwrap_or(1),
                threshold,
                radius,
                grain,
                ..Default::default()
            };
        }

        let p = &mut this.params;
        // PL_RENDER_DEFAULTS
        p.color_adjustment = &raw const pl::pl_color_adjustment_neutral;
        p.tile_colors = [[0.93; 3], [0.87; 3]];
        p.tile_size = 32;
        p.blur_radius = 16.0;
        if let crate::Border::Blur(r) = grade.border {
            p.border = pl::pl_clear_mode::PL_CLEAR_BLUR;
            p.blur_radius = r;
        }
        // vf_libplacebo
        p.background_transparency = 1.0;
        p.upscaler = scaler(grade.scaling.upscaler)?;
        p.downscaler = scaler(grade.scaling.downscaler)?;
        p.disable_linear_scaling = !grade.scaling.linear;
        p.num_hooks = i32::try_from(this.hook_ptrs.len()).unwrap_or(0);

        let p = &raw mut this.params;
        // SAFETY: the pointers target fields of the same boxed (address-stable) struct.
        unsafe {
            (*p).color_map_params = &raw const this.color_map;
            (*p).deinterlace_params = &raw const this.deinterlace;
            (*p).peak_detect_params = if grade.peak_detect {
                &raw const this.peak
            } else {
                ptr::null()
            };
            (*p).dither_params = if grade.dither.is_some() {
                &raw const this.dither
            } else {
                ptr::null()
            };
            (*p).deband_params = if grade.deband.is_some() {
                &raw const this.deband
            } else {
                ptr::null()
            };
            (*p).sigmoid_params = if grade.scaling.sigmoid {
                &raw const this.sigmoid
            } else {
                ptr::null()
            };
            (*p).hooks = if this.hook_ptrs.is_empty() {
                ptr::null()
            } else {
                this.hook_ptrs.as_ptr()
            };
        }
        Ok(this)
    }
}

/// One layer of a [`Compositor`] pass, in target pixels: its texture's middle at `centre`,
/// `half` its half size, turned by `(cos, sin)` counter-clockwise on screen.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompositeLayer {
    pub tex: pl::pl_tex,
    pub centre: [f32; 2],
    pub half: [f32; 2],
    pub rot: [f32; 2],
    pub mode: i32,
}

/// The layer composite pass (`pl_dispatch` with a custom shader): reads the backdrop and one
/// layer texture (not premultiplied alpha) and writes `mix(b, B(b, s), a)` to an opaque target,
/// `B` the layer's blend mode ([`crate::blend::glsl`]). Pixels outside the turned rectangle
/// keep the backdrop; its edges are antialiased over one target pixel.
#[derive(Debug)]
pub(crate) struct Compositor {
    dp: pl::pl_dispatch,
    header: CString,
    body: CString,
}

// SAFETY: used from one thread at a time.
unsafe impl Send for Compositor {}

impl Compositor {
    pub(crate) fn new(log: &Log, vk: &Vulkan) -> Result<Self> {
        // SAFETY: valid log and gpu.
        let dp = unsafe { pl::pl_dispatch_create(log.0, vk.gpu) };
        if dp.is_null() {
            return Err(err("pl_dispatch_create failed"));
        }
        let cs = |s: &str| CString::new(s).map_err(|_| err("NUL in shader"));
        Ok(Self {
            dp,
            header: cs(crate::blend::glsl())?,
            body: cs(r"
    ivec2 px = ivec2(gl_FragCoord.xy);
    vec3 b = texelFetch(fc_backdrop, px, 0).rgb;
    vec2 d = gl_FragCoord.xy - fc_centre;
    vec2 z = vec2(fc_rot.x * d.x - fc_rot.y * d.y, fc_rot.y * d.x + fc_rot.x * d.y);
    vec2 e = fc_half - abs(z);
    float cover = clamp(min(e.x, e.y) + 0.5, 0.0, 1.0);
    vec4 s = texture(fc_layer, z / (2.0 * fc_half) + 0.5);
    color = vec4(mix(b, blend_mode(fc_mode, b, s.rgb), clamp(s.a, 0.0, 1.0) * cover), 1.0);
")?,
        })
    }

    /// Call once per output frame, before its passes.
    pub(crate) fn reset_frame(&mut self) {
        // SAFETY: our dispatch.
        unsafe { pl::pl_dispatch_reset_frame(self.dp) };
    }

    /// Composites `layer` over `backdrop` into `target` (same size as `backdrop`, renderable).
    pub(crate) fn run(
        &mut self,
        backdrop: pl::pl_tex,
        layer: &CompositeLayer,
        target: pl::pl_tex,
    ) -> Result<()> {
        let tex = |name: &std::ffi::CStr, t: pl::pl_tex, sample| pl::pl_shader_desc {
            desc: pl::pl_desc {
                name: name.as_ptr(),
                type_: pl::pl_desc_type::PL_DESC_SAMPLED_TEX,
                ..Default::default()
            },
            binding: pl::pl_desc_binding {
                object: t.cast(),
                address_mode: pl::pl_tex_address_mode::PL_TEX_ADDRESS_CLAMP,
                sample_mode: sample,
            },
            ..Default::default()
        };
        let descs = [
            tex(
                c"fc_backdrop",
                backdrop,
                pl::pl_tex_sample_mode::PL_TEX_SAMPLE_NEAREST,
            ),
            tex(
                c"fc_layer",
                layer.tex,
                pl::pl_tex_sample_mode::PL_TEX_SAMPLE_LINEAR,
            ),
        ];
        let var = |name: &std::ffi::CStr, type_, dim_v, data: *const std::ffi::c_void| {
            pl::pl_shader_var {
                var: pl::pl_var {
                    name: name.as_ptr(),
                    type_,
                    dim_v,
                    dim_m: 1,
                    dim_a: 1,
                },
                data,
                dynamic: true,
            }
        };
        let f = pl::pl_var_type::PL_VAR_FLOAT;
        let vars = [
            var(c"fc_centre", f, 2, layer.centre.as_ptr().cast()),
            var(c"fc_half", f, 2, layer.half.as_ptr().cast()),
            var(c"fc_rot", f, 2, layer.rot.as_ptr().cast()),
            var(
                c"fc_mode",
                pl::pl_var_type::PL_VAR_SINT,
                1,
                (&raw const layer.mode).cast(),
            ),
        ];
        let custom = pl::pl_custom_shader {
            header: self.header.as_ptr(),
            body: self.body.as_ptr(),
            description: c"ffcine layer composite".as_ptr(),
            input: pl::pl_shader_sig::PL_SHADER_SIG_NONE,
            output: pl::pl_shader_sig::PL_SHADER_SIG_COLOR,
            descriptors: descs.as_ptr(),
            num_descriptors: 2,
            variables: vars.as_ptr(),
            num_variables: 4,
            ..Default::default()
        };
        // SAFETY: our dispatch; the descriptors, variables and strings outlive the calls (the
        // shader copies the variables' data); the textures are valid and the target renderable.
        unsafe {
            let mut sh = pl::pl_dispatch_begin(self.dp);
            if !pl::pl_shader_custom(sh, &raw const custom) {
                pl::pl_dispatch_abort(self.dp, &raw mut sh);
                return Err(err("pl_shader_custom (composite) failed"));
            }
            let params = pl::pl_dispatch_params {
                shader: &raw mut sh,
                target,
                ..Default::default()
            };
            if !pl::pl_dispatch_finish(self.dp, &raw const params) {
                return Err(err("pl_dispatch_finish (composite) failed"));
            }
        }
        Ok(())
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        // SAFETY: we own the dispatch.
        unsafe { pl::pl_dispatch_destroy(&raw mut self.dp) };
    }
}
