//! Encoder settings, backend-neutral (names follow ffmpeg's encoder options).

use std::fmt;
use std::str::FromStr;

use crate::{ColorSpace, Error};
use ffcine_core::color::named_enum;

named_enum!(
    /// Video encoder.
    Codec {
        HevcNvenc = "hevc_nvenc",
        H264Nvenc = "h264_nvenc",
        Av1Nvenc = "av1_nvenc",
        HevcVaapi = "hevc_vaapi",
        /// Vulkan video encode: takes the rendered Vulkan frames with no copy (no presets).
        HevcVulkan = "hevc_vulkan",
        H264Vulkan = "h264_vulkan",
        Av1Vulkan = "av1_vulkan",
        Libx264 = "libx264",
        Libx265 = "libx265",
        /// PNG stills (`ffcine still`), with [`PixelFormat::Rgba`].
        Png = "png",
    }
);

impl Codec {
    /// Hardware encoders that can take GPU frames without a download.
    pub const fn is_hardware(self) -> bool {
        matches!(
            self,
            Self::HevcNvenc | Self::H264Nvenc | Self::Av1Nvenc | Self::HevcVaapi
        ) || self.is_vulkan()
    }

    /// Vulkan video encoders (fed Vulkan frames directly).
    pub const fn is_vulkan(self) -> bool {
        matches!(self, Self::HevcVulkan | Self::H264Vulkan | Self::Av1Vulkan)
    }
}

named_enum!(
    /// Pixel format handed to the encoder.
    PixelFormat {
        /// 10-bit 4:2:0, semi-planar (NVENC main10).
        P010 = "p010le",
        Nv12 = "nv12",
        Yuv420p = "yuv420p",
        Yuv420p10 = "yuv420p10le",
        Gbrp16 = "gbrp16le",
        /// 8-bit packed RGBA (PNG stills).
        Rgba = "rgba",
    }
);

/// Rate control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateControl {
    /// Constant QP (NVENC `-rc constqp -qp N`).
    ConstQp(u8),
    /// Constant rate factor / constant quality (`-crf N` or NVENC `-cq N`).
    Crf(f32),
    /// Average bitrate in bits/s.
    Bitrate(u64),
}

impl FromStr for RateControl {
    type Err = Error;
    /// `qp=12`, `crf=18` or `bitrate=50000000`.
    fn from_str(s: &str) -> Result<Self, Error> {
        let invalid = || Error::Invalid {
            what: "rate control (qp=N, crf=N, bitrate=N)",
            value: s.to_owned(),
        };
        let (k, v) = s.split_once('=').ok_or_else(invalid)?;
        match k {
            "qp" => v.parse().map(Self::ConstQp).map_err(|_| invalid()),
            "crf" => v.parse().map(Self::Crf).map_err(|_| invalid()),
            "bitrate" => v.parse().map(Self::Bitrate).map_err(|_| invalid()),
            _ => Err(invalid()),
        }
    }
}

impl fmt::Display for RateControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConstQp(q) => write!(f, "qp={q}"),
            Self::Crf(c) => write!(f, "crf={c}"),
            Self::Bitrate(b) => write!(f, "bitrate={b}"),
        }
    }
}

/// How to encode the graded video.
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderSettings {
    pub codec: Codec,
    pub preset: Option<String>,
    pub tune: Option<String>,
    pub profile: Option<String>,
    pub rate: RateControl,
    pub pixel_format: PixelFormat,
    /// Keyframe interval in frames.
    pub gop: Option<u32>,
    /// Tags written to the stream; should match [`crate::Grade::output`].
    pub color: ColorSpace,
    /// Raw encoder options passed through untouched (`key`, `value`).
    pub extra: Vec<(String, String)>,
}

impl EncoderSettings {
    /// 10-bit HEVC NVENC intermediate/master: `hevc_nvenc -preset p6 -tune hq -rc constqp -qp 12
    /// -profile:v main10 -pix_fmt p010le -g 24`, as in the prototype.
    pub fn nvenc_hevc_master() -> Self {
        Self {
            codec: Codec::HevcNvenc,
            preset: Some("p6".into()),
            tune: Some("hq".into()),
            profile: Some("main10".into()),
            rate: RateControl::ConstQp(12),
            pixel_format: PixelFormat::P010,
            gop: Some(24),
            color: ColorSpace::BT709_TV,
            extra: Vec::new(),
        }
    }

    /// 8-bit H.264 preview: `libx264 -preset veryfast -crf 10 -pix_fmt yuv420p -g 24`.
    pub fn x264_preview() -> Self {
        Self {
            codec: Codec::Libx264,
            preset: Some("veryfast".into()),
            tune: None,
            profile: None,
            rate: RateControl::Crf(10.0),
            pixel_format: PixelFormat::Yuv420p,
            gop: Some(24),
            color: ColorSpace::BT709_TV,
            extra: Vec::new(),
        }
    }
}

impl EncoderSettings {
    /// A full-range RGB PNG frame, for stills; the grade's `output` must be
    /// [`ColorSpace::BT709_RGB`].
    pub fn png_still() -> Self {
        Self {
            codec: Codec::Png,
            preset: None,
            tune: None,
            profile: None,
            rate: RateControl::ConstQp(0),
            pixel_format: PixelFormat::Rgba,
            gop: None,
            color: ColorSpace::BT709_RGB,
            extra: Vec::new(),
        }
    }
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self::nvenc_hevc_master()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_control_round_trip() {
        for s in ["qp=12", "crf=18.5", "bitrate=50000000"] {
            assert_eq!(s.parse::<RateControl>().unwrap().to_string(), s);
        }
        assert!("vbr".parse::<RateControl>().is_err());
    }

    #[test]
    fn master_preset() {
        let e = EncoderSettings::default();
        assert!(e.codec.is_hardware());
        assert_eq!(e.pixel_format.as_str(), "p010le");
    }
}
