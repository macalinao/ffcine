//! NVIDIA Optical Flow (the NVOFA engine, Turing and later) on CUDA, from
//! `libnvidia-opticalflow.so.1` at run time. Hand-written from the Optical Flow SDK 2.0 headers
//! (`nvOpticalFlowCommon.h`, `nvOpticalFlowCuda.h`), which include `cuda.h`, so no bindgen.

use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

use libloading::Library;

use super::cuda::{CuContext, CuDevicePtr, Cuda, Mem, open_driver_lib, sym};
use ffcine_core::{Error, Result};

type Status = c_int;
type Handle = *mut c_void;
type BufferHandle = *mut c_void;

/// `NV_OF_API_VERSION` for SDK 2.0: major << 4 | minor.
const API_VERSION: u32 = 2 << 4;

const MODE_OPTICALFLOW: u32 = 1;
const USAGE_INPUT: u32 = 1;
const USAGE_OUTPUT: u32 = 2;
const USAGE_COST: u32 = 4;
const FORMAT_GRAYSCALE8: u32 = 1;
const FORMAT_SHORT2: u32 = 5;
const FORMAT_UINT8: u32 = 7;
const CUDA_BUFFER_CUDEVICEPTR: u32 = 2;
const CAPS_OUTPUT_GRID_SIZES: u32 = 0;
const CAPS_WIDTH_MIN: u32 = 4;
const CAPS_WIDTH_MAX: u32 = 6;
const CAPS_HEIGHT_MAX: u32 = 7;

/// `NV_OF_PERF_LEVEL`: slower is better quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PerfLevel {
    Slow = 5,
    Medium = 10,
    Fast = 20,
}

#[repr(C)]
struct InitParams {
    width: u32,
    height: u32,
    out_grid_size: u32,
    hint_grid_size: u32,
    mode: u32,
    perf_level: u32,
    enable_external_hints: u32,
    enable_output_cost: u32,
    h_priv_data: *mut c_void,
    disparity_range: u32,
    enable_roi: u32,
}

#[repr(C)]
struct BufferDescriptor {
    width: u32,
    height: u32,
    buffer_usage: u32,
    buffer_format: u32,
}

#[repr(C)]
struct ExecuteInputParams {
    input_frame: BufferHandle,
    reference_frame: BufferHandle,
    external_hints: BufferHandle,
    disable_temporal_hints: u32,
    padding: u32,
    h_priv_data: *mut c_void,
    padding2: u32,
    num_rois: u32,
    roi_data: *mut c_void,
}

#[repr(C)]
struct ExecuteOutputParams {
    output_buffer: BufferHandle,
    output_cost_buffer: BufferHandle,
    h_priv_data: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct StrideInfo {
    stride: [[u32; 2]; 3],
    num_planes: u32,
}

/// `NV_OF_CUDA_API_FUNCTION_LIST`, filled in by `NvOFAPICreateInstanceCuda`.
#[repr(C)]
#[derive(Default)]
struct FunctionList {
    create: Option<unsafe extern "C" fn(CuContext, *mut Handle) -> Status>,
    init: Option<unsafe extern "C" fn(Handle, *const InitParams) -> Status>,
    create_buffer: Option<
        unsafe extern "C" fn(Handle, *const BufferDescriptor, u32, *mut BufferHandle) -> Status,
    >,
    buffer_cuarray: Option<unsafe extern "C" fn(BufferHandle) -> *mut c_void>,
    buffer_ptr: Option<unsafe extern "C" fn(BufferHandle) -> CuDevicePtr>,
    buffer_stride: Option<unsafe extern "C" fn(BufferHandle, *mut StrideInfo) -> Status>,
    set_streams: Option<unsafe extern "C" fn(Handle, *mut c_void, *mut c_void) -> Status>,
    execute: Option<
        unsafe extern "C" fn(Handle, *const ExecuteInputParams, *mut ExecuteOutputParams) -> Status,
    >,
    destroy_buffer: Option<unsafe extern "C" fn(BufferHandle) -> Status>,
    destroy: Option<unsafe extern "C" fn(Handle) -> Status>,
    last_error: Option<unsafe extern "C" fn(Handle, *mut c_char, *mut u32) -> Status>,
    caps: Option<unsafe extern "C" fn(Handle, u32, *mut u32, *mut u32) -> Status>,
}

struct Driver {
    create_instance: unsafe extern "C" fn(u32, *mut FunctionList) -> Status,
    max_version: unsafe extern "C" fn(*mut u32) -> Status,
    _lib: Library,
}

fn lib() -> Result<&'static Driver> {
    static LIB: OnceLock<Result<Driver, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        let lib = open_driver_lib("libnvidia-opticalflow.so.1").map_err(|e| e.to_string())?;
        // SAFETY: signatures from nvOpticalFlowCuda.h / nvOpticalFlowCommon.h.
        unsafe {
            Ok(Driver {
                create_instance: sym(&lib, "NvOFAPICreateInstanceCuda")
                    .map_err(|e| e.to_string())?,
                max_version: sym(&lib, "NvOFGetMaxSupportedApiVersion")
                    .map_err(|e| e.to_string())?,
                _lib: lib,
            })
        }
    })
    .as_ref()
    .map_err(|msg| Error::Gpu {
        api: "NVOF",
        msg: msg.clone(),
    })
}

/// The driver's highest supported optical flow API version, as `(major, minor)`.
pub fn max_api_version() -> Result<(u32, u32)> {
    let mut v = 0;
    // SAFETY: out-pointer is valid.
    let r = unsafe { (lib()?.max_version)(&raw mut v) };
    status(r, "NvOFGetMaxSupportedApiVersion", None)?;
    Ok((v >> 4, v & 0xf))
}

fn status_name(r: Status) -> &'static str {
    const NAMES: [&str; 12] = [
        "NV_OF_SUCCESS",
        "NV_OF_ERR_OF_NOT_AVAILABLE",
        "NV_OF_ERR_UNSUPPORTED_DEVICE",
        "NV_OF_ERR_DEVICE_DOES_NOT_EXIST",
        "NV_OF_ERR_INVALID_PTR",
        "NV_OF_ERR_INVALID_PARAM",
        "NV_OF_ERR_INVALID_CALL",
        "NV_OF_ERR_INVALID_VERSION",
        "NV_OF_ERR_OUT_OF_MEMORY",
        "NV_OF_ERR_NOT_INITIALIZED",
        "NV_OF_ERR_UNSUPPORTED_FEATURE",
        "NV_OF_ERR_GENERIC",
    ];
    usize::try_from(r)
        .ok()
        .and_then(|i| NAMES.get(i))
        .unwrap_or(&"unknown status")
}

/// Maps a status to an error, with the session's last-error text when there is one.
fn status(r: Status, what: &str, session: Option<(&FunctionList, Handle)>) -> Result<()> {
    if r == 0 {
        return Ok(());
    }
    let mut detail = String::new();
    if let Some((api, h)) = session
        && let Some(f) = api.last_error
    {
        let mut buf = [0 as c_char; 256];
        let mut len = 256;
        // SAFETY: buffer and length are valid; the driver NUL-terminates within `len`.
        if unsafe { f(h, buf.as_mut_ptr(), &raw mut len) } == 0 {
            let bytes: Vec<u8> = buf
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            detail = format!(": {}", String::from_utf8_lossy(&bytes).trim());
        }
    }
    Err(Error::Gpu {
        api: "NVOF",
        msg: format!("{what}: {} ({r}){detail}", status_name(r)),
    })
}

struct Buffer {
    handle: BufferHandle,
    ptr: CuDevicePtr,
    pitch: usize,
}

/// An optical flow session: two input frames (ping-pong) and the flow + cost outputs, all
/// `CUdeviceptr` buffers in `ctx`.
pub struct OpticalFlow {
    api: FunctionList,
    of: Handle,
    ctx: CuContext,
    frames: Vec<Buffer>,
    flow: Buffer,
    cost: Buffer,
    pub width: u32,
    pub height: u32,
    pub grid: u32,
    /// Flow/cost grid size in vectors.
    pub grid_w: u32,
    pub grid_h: u32,
}

impl OpticalFlow {
    /// A session for `width x height` 8-bit luma, one vector per `grid x grid` block (1, 2 or 4).
    ///
    /// # Safety
    /// `ctx` must be a live CUDA context that outlives the session.
    pub unsafe fn new(
        ctx: CuContext,
        width: u32,
        height: u32,
        grid: u32,
        perf: PerfLevel,
    ) -> Result<Self> {
        // SAFETY: forwarded.
        unsafe { Self::with_slots(ctx, width, height, grid, perf, 2) }
    }

    /// A session with `slots` input frames (a ring for comparing a frame with several others).
    ///
    /// # Safety
    /// `ctx` must be a live CUDA context that outlives the session.
    pub unsafe fn with_slots(
        ctx: CuContext,
        width: u32,
        height: u32,
        grid: u32,
        perf: PerfLevel,
        slots: usize,
    ) -> Result<Self> {
        let cuda = Cuda::get()?;
        let lib = lib()?;
        let mut api = FunctionList::default();
        // SAFETY: `api` is a valid NV_OF_CUDA_API_FUNCTION_LIST to fill in.
        status(
            unsafe { (lib.create_instance)(API_VERSION, &raw mut api) },
            "NvOFAPICreateInstanceCuda",
            None,
        )?;
        let missing = |name: &str| Error::Gpu {
            api: "NVOF",
            msg: format!("driver left {name} null"),
        };
        let create = api
            .create
            .ok_or_else(|| missing("nvCreateOpticalFlowCuda"))?;
        let init = api.init.ok_or_else(|| missing("nvOFInit"))?;
        let _ctx = cuda.push(ctx)?;
        let mut of = std::ptr::null_mut();
        // SAFETY: `ctx` is a live context; `of` is an out-pointer.
        status(
            unsafe { create(ctx, &raw mut of) },
            "nvCreateOpticalFlowCuda",
            None,
        )?;
        let mut s = Self {
            api,
            of,
            ctx,
            frames: Vec::new(),
            flow: Self::null_buffer(),
            cost: Self::null_buffer(),
            width,
            height,
            grid,
            grid_w: width.div_ceil(grid),
            grid_h: height.div_ceil(grid),
        };
        s.check_caps()?;
        let params = InitParams {
            width,
            height,
            out_grid_size: grid,
            hint_grid_size: 0,
            mode: MODE_OPTICALFLOW,
            perf_level: perf as u32,
            enable_external_hints: 0,
            enable_output_cost: 1,
            h_priv_data: std::ptr::null_mut(),
            disparity_range: 0,
            enable_roi: 0,
        };
        // SAFETY: `params` is a valid NV_OF_INIT_PARAMS.
        status(
            unsafe { init(s.of, &raw const params) },
            "nvOFInit",
            Some((&s.api, s.of)),
        )?;
        for _ in 0..slots.max(2) {
            let b = s.buffer(width, height, USAGE_INPUT, FORMAT_GRAYSCALE8)?;
            s.frames.push(b);
        }
        s.flow = s.buffer(s.grid_w, s.grid_h, USAGE_OUTPUT, FORMAT_SHORT2)?;
        s.cost = s.buffer(s.grid_w, s.grid_h, USAGE_COST, FORMAT_UINT8)?;
        Ok(s)
    }

    const fn null_buffer() -> Buffer {
        Buffer {
            handle: std::ptr::null_mut(),
            ptr: 0,
            pitch: 0,
        }
    }

    fn caps(&self, cap: u32) -> Result<Vec<u32>> {
        let f = self.api.caps.ok_or_else(|| Error::Gpu {
            api: "NVOF",
            msg: "no nvOFGetCaps".into(),
        })?;
        let mut n = 0;
        // SAFETY: two-stage query: count with a null array, then fill.
        unsafe {
            status(
                f(self.of, cap, std::ptr::null_mut(), &raw mut n),
                "nvOFGetCaps",
                Some((&self.api, self.of)),
            )?;
            let mut v = vec![0; n as usize];
            status(
                f(self.of, cap, v.as_mut_ptr(), &raw mut n),
                "nvOFGetCaps",
                Some((&self.api, self.of)),
            )?;
            v.truncate(n as usize);
            Ok(v)
        }
    }

    fn check_caps(&self) -> Result<()> {
        let grids = self.caps(CAPS_OUTPUT_GRID_SIZES)?;
        let first = |c| self.caps(c).map(|v| v.first().copied().unwrap_or(0));
        let (wmin, wmax, hmax) = (
            first(CAPS_WIDTH_MIN)?,
            first(CAPS_WIDTH_MAX)?,
            first(CAPS_HEIGHT_MAX)?,
        );
        if !grids.contains(&self.grid)
            || self.width < wmin
            || (wmax > 0 && self.width > wmax)
            || (hmax > 0 && self.height > hmax)
        {
            return Err(Error::Gpu {
                api: "NVOF",
                msg: format!(
                    "unsupported session {}x{} grid {} (grids {grids:?}, width {wmin}..={wmax}, \
                     height ..={hmax})",
                    self.width, self.height, self.grid
                ),
            });
        }
        Ok(())
    }

    fn buffer(&self, width: u32, height: u32, usage: u32, format: u32) -> Result<Buffer> {
        let desc = BufferDescriptor {
            width,
            height,
            buffer_usage: usage,
            buffer_format: format,
        };
        let sess = Some((&self.api, self.of));
        let create = self.api.create_buffer.ok_or_else(|| Error::Gpu {
            api: "NVOF",
            msg: "no nvOFCreateGPUBufferCuda".into(),
        })?;
        let mut handle = std::ptr::null_mut();
        // SAFETY: valid descriptor and out-pointer.
        status(
            unsafe {
                create(
                    self.of,
                    &raw const desc,
                    CUDA_BUFFER_CUDEVICEPTR,
                    &raw mut handle,
                )
            },
            "nvOFCreateGPUBufferCuda",
            sess,
        )?;
        let mut stride = StrideInfo::default();
        // SAFETY: `handle` was just created.
        let (ptr, r) = unsafe {
            (
                self.api.buffer_ptr.map_or(0, |f| f(handle)),
                self.api
                    .buffer_stride
                    .map_or(-1, |f| f(handle, &raw mut stride)),
            )
        };
        let buf = Buffer {
            handle,
            ptr,
            pitch: stride.stride[0][0] as usize,
        };
        status(r, "nvOFGPUBufferGetStrideInfo", sess).inspect_err(|_| self.free(&buf))?;
        Ok(buf)
    }

    fn free(&self, buf: &Buffer) {
        if let Some(f) = self.api.destroy_buffer
            && !buf.handle.is_null()
        {
            // SAFETY: a buffer this session created, freed once.
            unsafe { f(buf.handle) };
        }
    }

    /// Copies an 8-bit luma plane (`width x height`, rows `pitch` apart) into input slot `slot`.
    /// For device memory, `ctx` must be the session's context (or share its address space).
    ///
    /// # Safety
    /// `src` must be valid for `height` rows of `pitch` bytes.
    pub unsafe fn upload(&self, slot: usize, src: Mem, pitch: usize) -> Result<()> {
        let cuda = Cuda::get()?;
        let _ctx = cuda.push(self.ctx)?;
        let dst = &self.frames[slot];
        // SAFETY: the caller guarantees `src`; `dst` is our buffer of `width x height`.
        unsafe {
            cuda.copy_2d(
                (src, pitch),
                (Mem::Device(dst.ptr), dst.pitch),
                self.width as usize,
                self.height as usize,
            )
        }
    }

    /// Flow from input slot `input` to slot `reference`: for the block at `p` in `input`, the
    /// vector `f` with `input(p) ~ reference(p + f)`, in S10.5 pixels. Temporal hints stay on
    /// (consecutive video frames). Then copies flow (`[x, y]` per block, row-major) and the 8-bit
    /// cost (lower is better) back to the host.
    pub fn run(
        &self,
        input: usize,
        reference: usize,
        flow: &mut Vec<[i16; 2]>,
        cost: &mut Vec<u8>,
    ) -> Result<()> {
        self.run_with(input, reference, true, flow, cost)
    }

    /// [`Self::run`], with temporal hints off when the pairs aren't consecutive frames.
    pub fn run_with(
        &self,
        input: usize,
        reference: usize,
        hints: bool,
        flow: &mut Vec<[i16; 2]>,
        cost: &mut Vec<u8>,
    ) -> Result<()> {
        let cuda = Cuda::get()?;
        let _ctx = cuda.push(self.ctx)?;
        let exec = self.api.execute.ok_or_else(|| Error::Gpu {
            api: "NVOF",
            msg: "no nvOFExecute".into(),
        })?;
        let inp = ExecuteInputParams {
            input_frame: self.frames[input].handle,
            reference_frame: self.frames[reference].handle,
            external_hints: std::ptr::null_mut(),
            disable_temporal_hints: u32::from(!hints),
            padding: 0,
            h_priv_data: std::ptr::null_mut(),
            padding2: 0,
            num_rois: 0,
            roi_data: std::ptr::null_mut(),
        };
        let mut out = ExecuteOutputParams {
            output_buffer: self.flow.handle,
            output_cost_buffer: self.cost.handle,
            h_priv_data: std::ptr::null_mut(),
        };
        // SAFETY: handles belong to this session.
        status(
            unsafe { exec(self.of, &raw const inp, &raw mut out) },
            "nvOFExecute",
            Some((&self.api, self.of)),
        )?;
        let (gw, gh) = (self.grid_w as usize, self.grid_h as usize);
        flow.resize(gw * gh, [0; 2]);
        cost.resize(gw * gh, 0);
        // Synchronous copies on the default stream, which NVOF also runs on: they wait for it.
        // SAFETY: host vectors hold gw x gh elements; device buffers are gw x gh at their pitch.
        unsafe {
            cuda.copy_2d(
                (Mem::Device(self.flow.ptr), self.flow.pitch),
                (Mem::HostMut(flow.as_mut_ptr().cast()), gw * 4),
                gw * 4,
                gh,
            )?;
            cuda.copy_2d(
                (Mem::Device(self.cost.ptr), self.cost.pitch),
                (Mem::HostMut(cost.as_mut_ptr()), gw),
                gw,
                gh,
            )?;
        }
        Ok(())
    }
}

impl Drop for OpticalFlow {
    fn drop(&mut self) {
        let Ok(cuda) = Cuda::get() else { return };
        let Ok(_ctx) = cuda.push(self.ctx) else {
            return;
        };
        for b in self.frames.iter().chain([&self.flow, &self.cost]) {
            self.free(b);
        }
        if let Some(f) = self.api.destroy {
            // SAFETY: destroyed once, after its buffers.
            unsafe { f(self.of) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smooth random texture, sampled bilinearly at `(x - dx, y - dy)`: content moved by `d`.
    fn texture(w: usize, h: usize, dx: f64, dy: f64) -> Vec<u8> {
        // Sum of a few sinusoids at incommensurate frequencies: textured everywhere, no aliasing.
        let waves: Vec<(f64, f64, f64)> = (0..24_u32)
            .map(|i| {
                let a = f64::from(i) * 2.399;
                let f = 0.02 + 0.006 * f64::from(i % 7);
                (f * a.cos(), f * a.sin(), f64::from(i) * 1.3)
            })
            .collect();
        let mut img = vec![0_u8; w * h];
        for (j, row) in img.chunks_mut(w).enumerate() {
            for (i, px) in row.iter_mut().enumerate() {
                let (x, y) = (i as f64 - dx, j as f64 - dy);
                let v: f64 = waves
                    .iter()
                    .map(|&(fx, fy, ph)| (fx * x + fy * y + ph).sin())
                    .sum();
                *px = (128.0 + 20.0 * v).clamp(0.0, 255.0) as u8;
            }
        }
        img
    }

    /// Reachability probe: a translated synthetic frame pair through the whole NVOF path.
    /// Skips (passes) without the driver libraries or a GPU.
    #[test]
    fn nvof_measures_translation() {
        let cuda = match Cuda::get() {
            Ok(c) => c,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let Ok(primary) = cuda.primary_context(0) else {
            return eprintln!("skipping: no CUDA device");
        };
        if let Err(e) = lib() {
            return eprintln!("skipping: {e}");
        }
        eprintln!("NVOF max API version {:?}", max_api_version().unwrap());
        let (w, h) = (640_usize, 360_usize);
        // SAFETY: `primary` outlives `of`.
        let of = unsafe { OpticalFlow::new(primary.ctx, w as u32, h as u32, 4, PerfLevel::Slow) }
            .unwrap();
        let (a, b) = (texture(w, h, 0.0, 0.0), texture(w, h, 3.25, -2.5));
        let (mut flow, mut cost) = (Vec::new(), Vec::new());
        // SAFETY: host images are w x h, pitch w.
        unsafe {
            of.upload(0, Mem::Host(a.as_ptr()), w).unwrap();
            of.upload(1, Mem::Host(b.as_ptr()), w).unwrap();
        }
        // b is a moved by (3.25, -2.5), so b(p) = a(p - d): flow from b to a is -d.
        of.run(1, 0, &mut flow, &mut cost).unwrap();
        let mut fx: Vec<f64> = flow.iter().map(|v| f64::from(v[0]) / 32.0).collect();
        let mut fy: Vec<f64> = flow.iter().map(|v| f64::from(v[1]) / 32.0).collect();
        fx.sort_by(f64::total_cmp);
        fy.sort_by(f64::total_cmp);
        let (mx, my) = (fx[fx.len() / 2], fy[fy.len() / 2]);
        eprintln!(
            "median flow ({mx}, {my}), mean cost {}",
            cost.iter().map(|&c| f64::from(c)).sum::<f64>() / cost.len() as f64
        );
        assert!((mx + 3.25).abs() < 0.2 && (my - 2.5).abs() < 0.2);
    }
}
