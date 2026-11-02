//! The ffcine render path: [`Pipeline`], [`Grade`] and [`EncoderSettings`] (backend-neutral),
//! and [`backend::Ffmpeg`], which runs them on ffmpeg's libav* and libplacebo/Vulkan with frames
//! kept in VRAM end to end.

pub mod backend;
pub mod encode;
pub mod grade;
pub mod layer;
pub mod overlay;
pub mod pipeline;
pub mod profile;
pub mod shaders;

pub use encode::{Codec, EncoderSettings, PixelFormat, RateControl};
pub use ffcine_core::{ColorSpace, Error, Lut, LutType, MediaInfo, Rational, Result};
pub use grade::{
    ColorMapTuning, ColorOverride, Crop, Deband, Dither, DitherMethod, GamutMapping, Grade, Hook,
    LutStage, ParamTrack, Scaler, Scaling, Size, ToneMapping, Warp,
};
pub use layer::{Image, Layer, LayerSource};
pub use overlay::{Mask, Overlay, OverlayPart};
pub use pipeline::{Backend, Decode, Device, FrameContext, Pipeline, RunStats, Stage};
pub use profile::DeliveryProfile;
