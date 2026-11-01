//! `ffcine doctor`: can this machine run the GPU path? Each check opens the real thing (a CUDA
//! and a Vulkan device, an NVENC session, the decoders' CUDA configs, the filters), so a missing
//! driver or a CPU-only ffmpeg build fails here, loudly, instead of mid-render.

use std::ffi::CStr;
use std::ptr;

use ffmpeg_next::ffi as ff;

use super::av::{BufferRef, CodecCtx, cstr};
use super::hw::Devices;
use crate::Device;

/// One check's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    /// `Ok(detail)` or `Err(why)`.
    pub result: Result<String, String>,
    /// What stops working without it.
    pub needed_for: &'static str,
}

fn check(name: &'static str, needed_for: &'static str, result: Result<String, String>) -> Check {
    Check {
        name,
        result,
        needed_for,
    }
}

fn version(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

fn hw_device(kind: ff::AVHWDeviceType) -> Result<String, String> {
    let mut p = ptr::null_mut();
    // SAFETY: out-pointer; null device name and options select the default device.
    let r =
        unsafe { ff::av_hwdevice_ctx_create(&raw mut p, kind, ptr::null(), ptr::null_mut(), 0) };
    if r < 0 {
        return Err(ffmpeg_next::Error::from(r).to_string());
    }
    // SAFETY: success gives us a reference, released on drop.
    drop(unsafe { BufferRef::from_raw(p, "av_hwdevice_ctx_create") });
    Ok("device 0 opened".into())
}

/// The decoder for `id` lists a CUDA hw config (NVDEC through `hwaccel cuda`).
fn nvdec(id: ff::AVCodecID) -> Result<String, String> {
    // SAFETY: plain lookups on static codec tables.
    unsafe {
        let codec = ff::avcodec_find_decoder(id);
        if codec.is_null() {
            return Err("no decoder".into());
        }
        let mut i = 0;
        loop {
            let cfg = ff::avcodec_get_hw_config(codec, i);
            if cfg.is_null() {
                return Err("decoder has no CUDA hwaccel (ffmpeg built without nvdec)".into());
            }
            if (*cfg).device_type == ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA {
                return Ok(CStr::from_ptr((*codec).name).to_string_lossy().into_owned());
            }
            i += 1;
        }
    }
}

/// Opens a small NVENC session: proves the encoder is built in and the driver takes it.
fn nvenc(name: &str) -> Result<String, String> {
    let n = cstr(name).map_err(|e| e.to_string())?;
    // SAFETY: lookup by valid C string.
    let codec = unsafe { ff::avcodec_find_encoder_by_name(n.as_ptr()) };
    if codec.is_null() {
        return Err(format!("{name} not in this ffmpeg build"));
    }
    let mut ctx = CodecCtx::new(codec).map_err(|e| e.to_string())?;
    let c = ctx.get_mut();
    c.width = 256;
    c.height = 256;
    c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;
    c.time_base = ff::AVRational { num: 1, den: 30 };
    c.framerate = ff::AVRational { num: 30, den: 1 };
    // SAFETY: configured context, matching codec, no options.
    let r = unsafe { ff::avcodec_open2(ctx.as_ptr(), codec, ptr::null_mut()) };
    if r < 0 {
        return Err(format!("open failed: {}", ffmpeg_next::Error::from(r)));
    }
    Ok("session opened".into())
}

fn filter(name: &str) -> Result<String, String> {
    let n = cstr(name).map_err(|e| e.to_string())?;
    // SAFETY: lookup by valid C string.
    let f = unsafe { ff::avfilter_get_by_name(n.as_ptr()) };
    if f.is_null() {
        Err("not in this ffmpeg build".into())
    } else {
        Ok("available".into())
    }
}

/// Runs every GPU check. The process should exit non-zero if any fails.
pub fn checks() -> Vec<Check> {
    let mut out = Vec::new();
    // SAFETY: version getters have no preconditions.
    let (avc, avf, avu) = unsafe {
        (
            ff::avcodec_version(),
            ff::avformat_version(),
            ff::avutil_version(),
        )
    };
    out.push(check(
        "libav",
        "everything",
        Ok(format!(
            "avcodec {}, avformat {}, avutil {}",
            version(avc),
            version(avf),
            version(avu)
        )),
    ));
    out.push(check(
        "cuda",
        "NVDEC decode, NVENC encode",
        hw_device(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA),
    ));
    out.push(check(
        "vulkan",
        "libplacebo grade",
        hw_device(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN),
    ));
    out.push(check(
        "cuda+vulkan interop",
        "zero-copy grade (frames stay in VRAM)",
        Devices::new(Device::Auto)
            .map(|d| {
                if d.vulkan_encode {
                    "Vulkan device with CUDA derived".to_owned()
                } else {
                    "CUDA device with Vulkan derived".to_owned()
                }
            })
            .map_err(|e| e.to_string()),
    ));
    for (name, id) in [
        ("nvdec hevc", ff::AVCodecID::AV_CODEC_ID_HEVC),
        ("nvdec h264", ff::AVCodecID::AV_CODEC_ID_H264),
    ] {
        out.push(check(name, "GPU decode of camera files", nvdec(id)));
    }
    for name in ["hevc_nvenc", "h264_nvenc"] {
        out.push(check(
            if name == "hevc_nvenc" {
                "nvenc hevc"
            } else {
                "nvenc h264"
            },
            "GPU encode",
            nvenc(name),
        ));
    }
    for (name, why) in [
        ("libplacebo", "libav-side libplacebo filter"),
        ("nlmeans_vulkan", "GPU denoise"),
    ] {
        out.push(check(
            if name == "libplacebo" {
                "libav filter libplacebo"
            } else {
                "libav filter nlmeans_vulkan"
            },
            why,
            filter(name),
        ));
    }
    out.push(check(
        "libplacebo api",
        "grade",
        Ok(format!(
            "built against PL_API_VER {}",
            super::placebo::api_version()
        )),
    ));
    out
}
