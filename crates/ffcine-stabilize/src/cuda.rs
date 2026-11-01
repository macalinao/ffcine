//! The CUDA driver API subset optical flow needs, from `libcuda.so.1` at run time: there is no
//! CUDA toolkit in the build, and the nix sandbox has no driver.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use libloading::Library;

use ffcine_core::{Error, Result};

pub type CuResult = c_int;
pub type CuDevice = c_int;
pub type CuContext = *mut c_void;
pub type CuDevicePtr = u64;

const MEMORYTYPE_HOST: c_uint = 1;
const MEMORYTYPE_DEVICE: c_uint = 2;

/// `CUDA_MEMCPY2D` (v2).
#[repr(C)]
struct Memcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_uint,
    src_host: *const c_void,
    src_device: CuDevicePtr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_uint,
    dst_host: *mut c_void,
    dst_device: CuDevicePtr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

/// One side of a 2D copy: host memory or a device pointer, with its pitch in bytes.
#[derive(Debug, Clone, Copy)]
pub enum Mem {
    Host(*const u8),
    HostMut(*mut u8),
    Device(CuDevicePtr),
}

pub struct Cuda {
    init: unsafe extern "C" fn(c_uint) -> CuResult,
    // Only the tests make their own context; detect uses ffmpeg's.
    #[cfg_attr(not(test), allow(dead_code))]
    device_get: unsafe extern "C" fn(*mut CuDevice, c_int) -> CuResult,
    #[cfg_attr(not(test), allow(dead_code))]
    primary_ctx_retain: unsafe extern "C" fn(*mut CuContext, CuDevice) -> CuResult,
    #[cfg_attr(not(test), allow(dead_code))]
    primary_ctx_release: unsafe extern "C" fn(CuDevice) -> CuResult,
    ctx_push: unsafe extern "C" fn(CuContext) -> CuResult,
    ctx_pop: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    memcpy_2d: unsafe extern "C" fn(*const Memcpy2D) -> CuResult,
    error_string: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
    import_ext: Option<unsafe extern "C" fn(*mut CuExtMem, *const ExtMemHandleDesc) -> CuResult>,
    ext_buffer: Option<
        unsafe extern "C" fn(*mut CuDevicePtr, CuExtMem, *const ExtMemBufferDesc) -> CuResult,
    >,
    destroy_ext: Option<unsafe extern "C" fn(CuExtMem) -> CuResult>,
    _lib: Library,
}

pub type CuExtMem = *mut c_void;

/// `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` for an opaque fd.
#[repr(C)]
struct ExtMemHandleDesc {
    kind: c_uint,
    _pad: c_uint,
    fd: c_int,
    _union_rest: [u8; 12],
    size: u64,
    flags: c_uint,
    reserved: [c_uint; 16],
}

/// `CUDA_EXTERNAL_MEMORY_BUFFER_DESC`.
#[repr(C)]
struct ExtMemBufferDesc {
    offset: u64,
    size: u64,
    flags: c_uint,
    reserved: [c_uint; 16],
}

/// Memory another API (Vulkan) exported as an opaque fd, mapped as a device pointer.
pub struct ExternalBuffer {
    mem: CuExtMem,
    pub ptr: CuDevicePtr,
}

impl Drop for ExternalBuffer {
    fn drop(&mut self) {
        if let Ok(cuda) = Cuda::get()
            && let Some(f) = cuda.destroy_ext
        {
            // SAFETY: imported once by `import_fd`.
            unsafe { f(self.mem) };
        }
    }
}

/// dlopen by soname first (finds the copy ffmpeg already loaded), then NixOS's driver path.
pub fn open_driver_lib(name: &str) -> Result<Library> {
    let mut errs = Vec::new();
    for path in [name.to_owned(), format!("/run/opengl-driver/lib/{name}")] {
        // SAFETY: NVIDIA driver libraries have no unsound initialisers.
        match unsafe { Library::new(&path) } {
            Ok(lib) => return Ok(lib),
            Err(e) => errs.push(e.to_string()),
        }
    }
    Err(Error::Gpu {
        api: "dlopen",
        msg: errs.join("; "),
    })
}

/// Copies a symbol's function pointer out of `lib`; `T` must be its exact C signature.
pub unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> Result<T> {
    // SAFETY: the caller guarantees `T` matches the symbol.
    unsafe { lib.get::<T>(name.as_bytes()) }
        .map(|s| *s)
        .map_err(|e| Error::Gpu {
            api: "dlsym",
            msg: format!("{name}: {e}"),
        })
}

impl Cuda {
    /// The process-wide driver handle, loaded and `cuInit`ed once.
    pub fn get() -> Result<&'static Self> {
        static CUDA: OnceLock<Result<Cuda, String>> = OnceLock::new();
        CUDA.get_or_init(|| Self::load().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|msg| Error::Gpu {
                api: "CUDA",
                msg: msg.clone(),
            })
    }

    fn load() -> Result<Self> {
        let lib = open_driver_lib("libcuda.so.1")?;
        // SAFETY: signatures from cuda.h (the _v2 entry points the headers map to).
        let cuda = unsafe {
            Self {
                init: sym(&lib, "cuInit")?,
                device_get: sym(&lib, "cuDeviceGet")?,
                primary_ctx_retain: sym(&lib, "cuDevicePrimaryCtxRetain")?,
                primary_ctx_release: sym(&lib, "cuDevicePrimaryCtxRelease_v2")?,
                ctx_push: sym(&lib, "cuCtxPushCurrent_v2")?,
                ctx_pop: sym(&lib, "cuCtxPopCurrent_v2")?,
                memcpy_2d: sym(&lib, "cuMemcpy2D_v2")?,
                error_string: sym(&lib, "cuGetErrorString")?,
                import_ext: sym(&lib, "cuImportExternalMemory").ok(),
                ext_buffer: sym(&lib, "cuExternalMemoryGetMappedBuffer").ok(),
                destroy_ext: sym(&lib, "cuDestroyExternalMemory").ok(),
                _lib: lib,
            }
        };
        // SAFETY: plain call; flags must be 0.
        cuda.check(unsafe { (cuda.init)(0) }, "cuInit")?;
        Ok(cuda)
    }

    pub fn check(&self, r: CuResult, what: &str) -> Result<()> {
        if r == 0 {
            return Ok(());
        }
        let mut s: *const c_char = std::ptr::null();
        // SAFETY: cuGetErrorString returns a static string or leaves `s` null.
        let text = unsafe {
            if (self.error_string)(r, &raw mut s) == 0 && !s.is_null() {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            } else {
                "unknown error".to_owned()
            }
        };
        Err(Error::Gpu {
            api: "CUDA",
            msg: format!("{what}: {text} ({r})"),
        })
    }

    #[cfg(test)]
    /// Retains device `ordinal`'s primary context (what ffmpeg's CUDA device uses too).
    pub fn primary_context(&self, ordinal: c_int) -> Result<PrimaryContext> {
        let mut dev = 0;
        let mut ctx = std::ptr::null_mut();
        // SAFETY: out-pointers are valid.
        unsafe {
            self.check((self.device_get)(&raw mut dev, ordinal), "cuDeviceGet")?;
            self.check(
                (self.primary_ctx_retain)(&raw mut ctx, dev),
                "cuDevicePrimaryCtxRetain",
            )?;
        }
        Ok(PrimaryContext { dev, ctx })
    }

    /// Makes `ctx` current on this thread until the guard drops.
    pub fn push(&self, ctx: CuContext) -> Result<CtxGuard<'_>> {
        // SAFETY: `ctx` is a live context owned by the caller.
        self.check(unsafe { (self.ctx_push)(ctx) }, "cuCtxPushCurrent")?;
        Ok(CtxGuard { cuda: self })
    }

    /// Imports `size` bytes of an opaque-fd export (CUDA takes ownership of `fd`) at `offset`
    /// as a device pointer. Needs a current context.
    ///
    /// # Safety
    /// `fd` must be an opaque fd export of a dedicated allocation of at least `size` bytes.
    pub unsafe fn import_fd(&self, fd: c_int, size: u64, offset: u64) -> Result<ExternalBuffer> {
        let (Some(import), Some(map)) = (self.import_ext, self.ext_buffer) else {
            return Err(Error::Gpu {
                api: "CUDA",
                msg: "driver lacks external memory".into(),
            });
        };
        let desc = ExtMemHandleDesc {
            kind: 1, // CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD
            _pad: 0,
            fd,
            _union_rest: [0; 12],
            size: size + offset,
            // CUDA_EXTERNAL_MEMORY_DEDICATED: libplacebo gives exported buffers their own
            // allocation.
            flags: u32::from(offset == 0),
            reserved: [0; 16],
        };
        let mut mem = std::ptr::null_mut();
        // SAFETY: valid descriptor.
        self.check(
            unsafe { import(&raw mut mem, &raw const desc) },
            "cuImportExternalMemory",
        )?;
        let mut buf = ExternalBuffer { mem, ptr: 0 };
        let bd = ExtMemBufferDesc {
            offset,
            size,
            flags: 0,
            reserved: [0; 16],
        };
        // SAFETY: mem was just imported.
        self.check(
            unsafe { map(&raw mut buf.ptr, mem, &raw const bd) },
            "cuExternalMemoryGetMappedBuffer",
        )?;
        Ok(buf)
    }

    /// Synchronous pitched copy of `height` rows of `width_bytes`. Needs a current context.
    ///
    /// # Safety
    /// Both sides must be valid for `height` rows of `pitch` bytes (`width_bytes` on the last).
    pub unsafe fn copy_2d(
        &self,
        (src, src_pitch): (Mem, usize),
        (dst, dst_pitch): (Mem, usize),
        width_bytes: usize,
        height: usize,
    ) -> Result<()> {
        let mut p = Memcpy2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: 0,
            src_host: std::ptr::null(),
            src_device: 0,
            src_array: std::ptr::null_mut(),
            src_pitch,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: 0,
            dst_host: std::ptr::null_mut(),
            dst_device: 0,
            dst_array: std::ptr::null_mut(),
            dst_pitch,
            width_in_bytes: width_bytes,
            height,
        };
        match src {
            Mem::Host(h) => (p.src_memory_type, p.src_host) = (MEMORYTYPE_HOST, h.cast()),
            Mem::HostMut(h) => (p.src_memory_type, p.src_host) = (MEMORYTYPE_HOST, h.cast()),
            Mem::Device(d) => (p.src_memory_type, p.src_device) = (MEMORYTYPE_DEVICE, d),
        }
        match dst {
            Mem::HostMut(h) => (p.dst_memory_type, p.dst_host) = (MEMORYTYPE_HOST, h.cast()),
            Mem::Device(d) => (p.dst_memory_type, p.dst_device) = (MEMORYTYPE_DEVICE, d),
            Mem::Host(_) => {
                return Err(Error::Invalid {
                    what: "copy destination",
                    value: "read-only host memory".into(),
                });
            }
        }
        // SAFETY: the caller guarantees the extents.
        self.check(unsafe { (self.memcpy_2d)(&raw const p) }, "cuMemcpy2D")
    }
}

pub struct CtxGuard<'a> {
    cuda: &'a Cuda,
}

impl Drop for CtxGuard<'_> {
    fn drop(&mut self) {
        let mut old = std::ptr::null_mut();
        // SAFETY: pops the context this guard pushed.
        unsafe { (self.cuda.ctx_pop)(&raw mut old) };
    }
}

#[cfg(test)]
/// A retained primary context, released on drop.
pub struct PrimaryContext {
    dev: CuDevice,
    pub ctx: CuContext,
}

#[cfg(test)]
impl Drop for PrimaryContext {
    fn drop(&mut self) {
        if let Ok(cuda) = Cuda::get() {
            // SAFETY: balances the retain in `primary_context`.
            unsafe { (cuda.primary_ctx_release)(self.dev) };
        }
    }
}
