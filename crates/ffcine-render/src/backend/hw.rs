//! CUDA + Vulkan hw device contexts on one physical GPU, and frames contexts on them.

use std::ptr;

use ffmpeg_next::ffi as ff;

use super::av::{BufferRef, Dict, check, cstr};
use crate::{Device, Result};

/// A CUDA device (NVDEC, NVENC) and a Vulkan device (libplacebo) on the same GPU.
#[derive(Debug)]
pub(crate) struct Devices {
    pub cuda: BufferRef,
    pub vulkan: BufferRef,
    /// Vulkan video encode works: not on a Vulkan device derived from CUDA (ffmpeg's
    /// encoders crash there, as on any device with `disable_multiplane`).
    pub vulkan_encode: bool,
}

impl Devices {
    /// `Vulkan(n)` and `Auto`: a Vulkan device with CUDA derived from it (matched by UUID).
    /// `Cuda(n)`: like the prototype's `-init_hw_device cuda=cu:N -init_hw_device vulkan=vk@cu`,
    /// picking the GPU by CUDA index. `Auto` falls back to that if Vulkan device 0 has no CUDA.
    /// Frames shared with CUDA are one image per plane per frames context
    /// (`frames(.., cuda: true)`), so the device keeps multiplane images for Vulkan video.
    pub(crate) fn new(device: Device) -> Result<Self> {
        match device {
            Device::Vulkan(n) => Self::from_vulkan(&n.to_string()),
            Device::Cuda(n) => Self::from_cuda(&n.to_string()),
            _ => Self::from_vulkan("0").or_else(|_| Self::from_cuda("0")),
        }
    }

    fn from_vulkan(name: &str) -> Result<Self> {
        let vulkan = create(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN, name, &[])?;
        let cuda = derive(&vulkan, ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA)?;
        Ok(Self {
            cuda,
            vulkan,
            vulkan_encode: true,
        })
    }

    fn from_cuda(name: &str) -> Result<Self> {
        let cuda = create(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, name, &[])?;
        let vulkan = derive(&cuda, ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN)?;
        Ok(Self {
            cuda,
            vulkan,
            vulkan_encode: false,
        })
    }
}

fn create(kind: ff::AVHWDeviceType, name: &str, opts: &[(&str, &str)]) -> Result<BufferRef> {
    let mut dict = Dict::default();
    for (k, v) in opts {
        dict.set(k, v)?;
    }
    let name = cstr(name)?;
    let mut p = ptr::null_mut();
    // SAFETY: out-pointer and valid strings.
    check(
        unsafe {
            ff::av_hwdevice_ctx_create(&raw mut p, kind, name.as_ptr(), *dict.as_mut_ptr(), 0)
        },
        "av_hwdevice_ctx_create",
    )?;
    // SAFETY: success gives us a reference.
    unsafe { BufferRef::from_raw(p, "av_hwdevice_ctx_create") }
}

fn derive(src: &BufferRef, kind: ff::AVHWDeviceType) -> Result<BufferRef> {
    let mut p = ptr::null_mut();
    // SAFETY: out-pointer, valid source device.
    check(
        unsafe { ff::av_hwdevice_ctx_create_derived(&raw mut p, kind, src.as_ptr(), 0) },
        "av_hwdevice_ctx_create_derived",
    )?;
    // SAFETY: success gives us a reference.
    unsafe { BufferRef::from_raw(p, "av_hwdevice_ctx_create_derived") }
}

/// `VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT`: libplacebo render targets must be renderable.
pub(crate) const VK_USAGE_RENDER: i32 = 0x10;

/// A frames context on `device` (`AV_PIX_FMT_CUDA` or `AV_PIX_FMT_VULKAN`, from the device).
/// Vulkan only: `vk_usage` is added to ffmpeg's default image usage, and `cuda` makes one
/// image per plane so CUDA can import the frames.
pub(crate) fn frames(
    device: &BufferRef,
    sw_format: ff::AVPixelFormat,
    width: u32,
    height: u32,
    vk_usage: i32,
    cuda: bool,
) -> Result<BufferRef> {
    // SAFETY: valid device; we fill the context before init.
    unsafe {
        let r = BufferRef::from_raw(
            ff::av_hwframe_ctx_alloc(device.as_ptr()),
            "av_hwframe_ctx_alloc",
        )?;
        let fc = r.data::<ff::AVHWFramesContext>();
        let dev = device.data::<ff::AVHWDeviceContext>();
        (*fc).format = match (*dev).type_ {
            ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA => ff::AVPixelFormat::AV_PIX_FMT_CUDA,
            _ => ff::AVPixelFormat::AV_PIX_FMT_VULKAN,
        };
        (*fc).sw_format = sw_format;
        (*fc).width = i32::try_from(width).unwrap_or(i32::MAX);
        (*fc).height = i32::try_from(height).unwrap_or(i32::MAX);
        if (*fc).format == ff::AVPixelFormat::AV_PIX_FMT_VULKAN {
            let vk = (*fc).hwctx.cast::<ff::AVVulkanFramesContext>();
            (*vk).usage |= vk_usage;
            if cuda {
                // A bit set, bound as a Rust enum (0 isn't a variant): write it as an integer.
                let flags = (&raw mut (*vk).flags).cast::<u32>();
                *flags |= ff::AVVkFrameFlags::AV_VK_FRAME_FLAG_DISABLE_MULTIPLANE as u32;
            }
        }
        check(ff::av_hwframe_ctx_init(r.as_ptr()), "av_hwframe_ctx_init")?;
        Ok(r)
    }
}
