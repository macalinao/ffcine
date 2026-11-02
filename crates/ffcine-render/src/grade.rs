//! The grade: everything libplacebo does to a frame between decode and encode.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::{ColorSpace, Error};
use ffcine_core::color::named_enum;
use ffcine_core::color::{HdrMetadata, Matrix, Primaries, Range, Transfer};
use ffcine_core::geom::Mat3;
use ffcine_core::lut::{Lut, LutType};
pub use ffcine_filters::{Denoise, Grain, Spatial, Temporal};

named_enum!(
    /// HDR -> SDR tone curve.
    ToneMapping {
        Auto = "auto",
        /// ITU-R BT.2390 EETF (the prototype's choice for iPhone HLG/Dolby Vision).
        Bt2390 = "bt.2390",
        Bt2446a = "bt.2446a",
        Spline = "spline",
        Reinhard = "reinhard",
        Mobius = "mobius",
        Hable = "hable",
        Clip = "clip",
        /// SMPTE ST 2094-40 (HDR10+) curve, from dynamic metadata when the source has it.
        St2094_40 = "st2094-40",
        /// SMPTE ST 2094-10 (Dolby Vision) curve.
        St2094_10 = "st2094-10",
        Gamma = "gamma",
        Linear = "linear",
    }
);

named_enum!(
    /// How out-of-gamut colours are brought into the target gamut.
    GamutMapping {
        Perceptual = "perceptual",
        Relative = "relative",
        Saturation = "saturation",
        Absolute = "absolute",
        Desaturate = "desaturate",
        Darken = "darken",
        Clip = "clip",
        /// Soft-knee clip in IPT, hue-preserving.
        Softclip = "softclip",
        /// Like `perceptual`, but keeps the highlights' saturation.
        Highlight = "highlight",
        /// Linear chroma compression of the whole gamut.
        Linear = "linear",
    }
);

/// Tuning for libplacebo's tone and gamut mapping (`pl_color_map_params`). The defaults are the
/// prototype's (libavfilter `libplacebo` with its defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorMapTuning {
    /// HDR contrast recovery strength, 0 = off.
    pub contrast_recovery: f32,
    /// How strongly `perceptual` gamut mapping desaturates in-gamut colours near the boundary.
    pub perceptual_strength: f32,
    /// Fraction of the gamut `perceptual` leaves untouched.
    pub perceptual_deadzone: f32,
}

impl Default for ColorMapTuning {
    fn default() -> Self {
        Self {
            contrast_recovery: 0.30,
            perceptual_strength: 0.80,
            perceptual_deadzone: 0.30,
        }
    }
}

named_enum!(
    /// Resampling filter.
    Scaler {
        Lanczos = "lanczos",
        EwaLanczos = "ewa_lanczos",
        /// Sharpened EWA Lanczos (the design's upscaler for reframes).
        EwaLanczosSharp = "ewa_lanczossharp",
        Spline36 = "spline36",
        Mitchell = "mitchell",
        Bicubic = "bicubic",
        Bilinear = "bilinear",
        Nearest = "nearest",
    }
);

/// Scaling behaviour. The default matches swscale (lanczos, gamma light, no sigmoid) so GPU and
/// CPU geometry agree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scaling {
    pub upscaler: Scaler,
    pub downscaler: Scaler,
    /// Scale in linear light (libplacebo's default; off in the prototype).
    pub linear: bool,
    /// Sigmoidized upscaling.
    pub sigmoid: bool,
}

impl Default for Scaling {
    fn default() -> Self {
        Self {
            upscaler: Scaler::Lanczos,
            downscaler: Scaler::Lanczos,
            linear: false,
            sigmoid: false,
        }
    }
}

/// Output size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl FromStr for Size {
    type Err = Error;
    /// `1920x1080`.
    fn from_str(s: &str) -> Result<Self, Error> {
        let invalid = || Error::Invalid {
            what: "size (WxH)",
            value: s.to_owned(),
        };
        let (w, h) = s.split_once('x').ok_or_else(invalid)?;
        Ok(Self {
            width: w.parse().map_err(|_| invalid())?,
            height: h.parse().map_err(|_| invalid())?,
        })
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// A rectangle in pixels: a source crop in input pixels, or a placement in output pixels.
///
/// Fractional values are kept as is: libplacebo resamples sub-pixel windows, so keyframed crops
/// (`Grade::crops`, from `ffcine-reframe`) move smoothly instead of stepping a pixel at a time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl FromStr for Crop {
    type Err = Error;
    /// `w:h:x:y`, like ffmpeg's `crop` filter.
    fn from_str(s: &str) -> Result<Self, Error> {
        let invalid = || Error::Invalid {
            what: "crop (w:h:x:y)",
            value: s.to_owned(),
        };
        let v: Vec<f32> = s
            .split(':')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| invalid())?;
        match v[..] {
            [width, height, x, y] => Ok(Self {
                x,
                y,
                width,
                height,
            }),
            _ => Err(invalid()),
        }
    }
}

named_enum!(
    /// Dither algorithm for the output quantization.
    DitherMethod {
        /// Blue noise (libplacebo's and the prototype's default).
        Blue = "blue",
        Ordered = "ordered",
        White = "white",
    }
);

/// Dithering to the output bit depth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dither {
    pub method: DitherMethod,
    /// Vary the pattern per frame.
    pub temporal: bool,
}

impl Default for Dither {
    fn default() -> Self {
        Self {
            method: DitherMethod::Blue,
            temporal: false,
        }
    }
}

/// libplacebo's debanding on the source planes (`pl_deband_params`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Deband {
    pub iterations: u32,
    pub threshold: f32,
    pub radius: f32,
    pub grain: f32,
}

impl Default for Deband {
    /// libavfilter's `libplacebo` defaults (`deband=1`).
    fn default() -> Self {
        Self {
            iterations: 1,
            threshold: 4.0,
            radius: 16.0,
            grain: 6.0,
        }
    }
}

/// Overrides for the source's colour tags (the prototype's `setparams` before the grade).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColorOverride {
    pub primaries: Option<Primaries>,
    pub transfer: Option<Transfer>,
    pub matrix: Option<Matrix>,
    pub range: Option<Range>,
}

impl FromStr for ColorOverride {
    type Err = Error;
    /// `matrix/primaries/transfer/range` as printed by `ffcine probe`, `-` keeps a tag:
    /// `bt709/bt709/bt709/tv`, `-/bt709/bt709/-`.
    #[allow(clippy::many_single_char_names)]
    fn from_str(s: &str) -> Result<Self, Error> {
        fn opt<T: FromStr<Err = Error>>(v: &str) -> Result<Option<T>, Error> {
            if v == "-" {
                Ok(None)
            } else {
                v.parse().map(Some)
            }
        }
        let v: Vec<&str> = s.split('/').collect();
        match v[..] {
            [m, p, t, r] => Ok(Self {
                matrix: opt(m)?,
                primaries: opt(p)?,
                transfer: opt(t)?,
                range: opt(r)?,
            }),
            _ => Err(Error::Invalid {
                what: "colour override (matrix/primaries/transfer/range)",
                value: s.to_owned(),
            }),
        }
    }
}

/// A LUT and how to apply it.
#[derive(Debug, Clone, PartialEq)]
pub struct LutStage {
    pub lut: Lut,
    pub kind: LutType,
}

/// A per-frame warp in the grade pass (the GPU stabilizer's output): for output frame `i`,
/// `transforms[i]` maps output pixels to source pixels, in source pixel coordinates before the
/// crop (the last one repeats past the end). Applied at libplacebo's `MAIN` stage with a
/// Catmull-Rom bicubic, black outside the source; the grade's crop is folded into it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Warp {
    pub transforms: Vec<Mat3>,
}

/// An inline mpv `.hook` shader and the values of its `//!PARAM`s.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hook {
    /// For error messages.
    pub name: String,
    /// The shader text.
    pub source: String,
    /// `//!PARAM` values set before the first frame (integer params are rounded).
    pub params: Vec<(String, f64)>,
    /// `//!PARAM` values per output frame.
    pub tracks: Vec<ParamTrack>,
}

/// One `//!PARAM`'s value per output frame: frame `i` uses `values[i]`, the last one holds past
/// the end.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParamTrack {
    pub name: String,
    pub values: Vec<f32>,
}

impl ParamTrack {
    pub fn at(&self, frame: usize) -> Option<f32> {
        self.values.get(frame).or(self.values.last()).copied()
    }
}

impl Grade {
    /// Bakes a camera LUT and a normalized grade LUT into one source LUT ([`crate::bake`]), so
    /// the grade LUT runs before the look hooks, as it does without a camera LUT, instead of on
    /// the target after them. The lattice is the larger of the two, at least 33.
    pub fn merge_luts(&mut self) {
        let (Some(cam), Some(lut)) = (&self.input_lut, &self.lut) else {
            return;
        };
        if cam.kind != LutType::Normalized || lut.kind != LutType::Normalized {
            return;
        }
        let size = cam.lut.size.max(lut.lut.size).max(33);
        let baked = crate::bake::Bake {
            camera: Some(&cam.lut),
            lut: Some(&lut.lut),
            look: crate::shaders::LookParams::default(),
        }
        .bake(size);
        self.input_lut = Some(LutStage {
            lut: baked,
            kind: LutType::Normalized,
        });
        self.lut = None;
    }

    /// The source crop for output frame `i`: `crops[i]` (the last one past the end), else `crop`.
    pub fn crop_at(&self, i: usize) -> Option<Crop> {
        self.crops
            .get(i)
            .or(self.crops.last())
            .copied()
            .or(self.crop)
    }
}

/// What fills the output around a placed picture ([`Grade::place`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Border {
    #[default]
    Black,
    /// A blurred copy of the picture scaled to cover the frame (libplacebo's `PL_CLEAR_BLUR`, in
    /// the same pass); the radius is in pixels of the placed picture.
    Blur(f32),
}

/// One libplacebo render pass: colour conversion (incl. tone and gamut mapping), crop/scale, LUT
/// and user shaders, ending in `output`.
#[derive(Debug, Clone, PartialEq)]
pub struct Grade {
    /// Source colour tags to override before anything else.
    pub input_color: ColorOverride,
    /// Camera LUT (log -> Rec.709, [`ffcine_core::camera`]) on the source. With one set, `lut`
    /// moves to the output side (libplacebo's target LUT), so it grades the converted picture.
    pub input_lut: Option<LutStage>,
    pub lut: Option<LutStage>,
    /// Per-channel gain on the decoded source RGB ahead of every LUT, clamped to 0..1 (ffmpeg's
    /// `colorchannelmixer rr:gg:bb` before `lut3d`). Folded into the first (normalized) LUT, or
    /// an identity one without.
    pub rgb_gain: Option<[f32; 3]>,
    pub tone_mapping: ToneMapping,
    pub gamut_mapping: GamutMapping,
    pub color_map: ColorMapTuning,
    /// Measure the HDR peak per frame (`pl_peak_detect`) instead of trusting metadata.
    pub peak_detect: bool,
    /// `None` disables debanding.
    pub deband: Option<Deband>,
    /// `None` truncates to the output depth.
    pub dither: Option<Dither>,
    pub crop: Option<Crop>,
    /// Stabilization warp, before the crop.
    pub warp: Option<Warp>,
    /// Per-output-frame source crops, overriding `crop`: frame `i` uses `crops[i]`, the last one
    /// holds past the end (keyframed punch-ins, pushes, pans and aspect windows).
    pub crops: Vec<Crop>,
    /// Where the picture lands in the output, in output pixels (`fit: contain`); the rest of the
    /// frame is black. `None` fills the frame.
    pub place: Option<Crop>,
    /// What fills the frame around a `place`d picture.
    pub border: Border,
    /// `None` keeps the input size.
    pub size: Option<Size>,
    pub scaling: Scaling,
    /// ffmpeg `unsharp=5:5:amount` equivalent, after the LUT (see [`crate::shaders::unsharp`]).
    pub sharpen: f32,
    /// Inline mpv `.hook` shaders (generated or vendored, see `ffcine-hooks`), after `sharpen`
    /// and before `shaders`. libplacebo runs each at the stage it hooks; within a stage they run in
    /// this order.
    pub hooks: Vec<Hook>,
    /// Extra mpv `.hook` shader files, applied in order.
    pub shaders: Vec<PathBuf>,
    /// Temporal + spatial denoise on the decoded planes, before the pass (`ffcine-filters`).
    pub denoise: Option<Denoise>,
    /// Film grain on the graded output (`ffcine-filters`).
    pub grain: Option<Grain>,
    pub output: ColorSpace,
    /// HDR deliverable metadata, with a PQ or HLG `output`: libplacebo maps to its peak, and it
    /// goes out as the stream's mastering display and content light levels. The source's own HDR
    /// side data and Dolby Vision RPU never reach the output.
    pub output_hdr: Option<HdrMetadata>,
}

impl Default for Grade {
    fn default() -> Self {
        Self {
            input_color: ColorOverride::default(),
            input_lut: None,
            lut: None,
            rgb_gain: None,
            tone_mapping: ToneMapping::Bt2390,
            gamut_mapping: GamutMapping::Perceptual,
            color_map: ColorMapTuning::default(),
            peak_detect: true,
            deband: None,
            dither: Some(Dither::default()),
            crop: None,
            warp: None,
            crops: Vec::new(),
            place: None,
            border: Border::Black,
            size: None,
            scaling: Scaling::default(),
            sharpen: 0.0,
            hooks: Vec::new(),
            shaders: Vec::new(),
            denoise: None,
            grain: None,
            output: ColorSpace::BT709_TV,
            output_hdr: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_camera_and_grade_luts() {
        let mut half = Lut::identity(5);
        for e in &mut half.data {
            *e = e.map(|v| v * 0.5);
        }
        let stage = |lut| LutStage {
            lut,
            kind: LutType::Normalized,
        };
        let mut g = Grade {
            input_lut: Some(stage(half.clone())),
            lut: Some(stage(half)),
            ..Grade::default()
        };
        g.merge_luts();
        assert!(g.lut.is_none());
        let m = g.input_lut.unwrap().lut;
        assert_eq!(m.size, 33);
        assert!((m.sample([1.0, 0.5, 0.0])[0] - 0.25).abs() < 1e-5);
    }

    #[test]
    fn parse_size_and_crop() {
        assert_eq!(
            "3840x2160".parse::<Size>().unwrap(),
            Size {
                width: 3840,
                height: 2160
            }
        );
        assert!("3840".parse::<Size>().is_err());
        let c: Crop = "1920:1080:10:20".parse().unwrap();
        assert_eq!((c.width, c.x), (1920.0, 10.0));
        assert!("1:2:3".parse::<Crop>().is_err());
        let o: ColorOverride = "-/bt709/bt709/tv".parse().unwrap();
        assert_eq!(o.matrix, None);
        assert_eq!(o.range, Some(Range::Limited));
        assert!("bt709/bt709".parse::<ColorOverride>().is_err());
        assert_eq!(
            "bt.2390".parse::<ToneMapping>().unwrap(),
            ToneMapping::Bt2390
        );
        assert_eq!(
            "st2094-40".parse::<ToneMapping>().unwrap(),
            ToneMapping::St2094_40
        );
        assert_eq!(
            "softclip".parse::<GamutMapping>().unwrap(),
            GamutMapping::Softclip
        );
    }
}
