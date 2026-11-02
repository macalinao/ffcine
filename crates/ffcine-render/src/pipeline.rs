//! A pipeline: one input, a chain of GPU stages, one encoded output.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{EncoderSettings, Error, Grade, Layer, MediaInfo, Overlay, Rational, Result};
use ffcine_core::color::named_enum;
use std::fmt;
use std::str::FromStr;

named_enum!(
    /// Hardware decode API. Both drive the same video decoder; Vulkan hands frames straight to
    /// the renderer, CUDA (NVDEC) needs a copy into Vulkan memory per frame.
    #[derive(Default)]
    Decode {
        #[default]
        Vulkan = "vulkan",
        Cuda = "cuda",
    }
);

/// Which GPU to run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Device {
    /// Vulkan device 0 (else CUDA device 0).
    #[default]
    Auto,
    /// A CUDA device, with the Vulkan device derived from it so decode, processing and encode
    /// share one physical GPU regardless of Vulkan enumeration order. No Vulkan video encode.
    Cuda(u32),
    /// A Vulkan physical device index, with CUDA (NVENC) derived from it.
    Vulkan(u32),
}

/// Frames read around a pipeline's range for temporal filters. See [`Pipeline::context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameContext {
    pub before: u64,
    pub after: u64,
}

/// One processing step. Frames stay on the GPU between stages.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Stage {
    /// A libplacebo render pass (tone mapping, crop/scale, LUT, shaders).
    Grade(Grade),
}

/// Decode `input`, run `stages` in order, encode with `encoder` into `output`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Seek here first; earlier frames are decoded and dropped (ffmpeg's accurate `-ss`).
    pub start: Duration,
    /// Output exactly this many frames (repeating the last one if the source runs out).
    pub frames: Option<u64>,
    /// Frames before `start` and after the last output frame that a temporal denoiser reads as
    /// history and lookahead but that aren't output (fewer where the input runs out). Ignored
    /// without one.
    pub context: FrameContext,
    /// Output frame rate (ffmpeg's `fps` filter); `None` keeps the source's frames.
    pub frame_rate: Option<Rational>,
    pub stages: Vec<Stage>,
    pub encoder: EncoderSettings,
    pub device: Device,
    pub decode: Decode,
    /// Pictures composited over the graded frames in the same pass, in order, under the
    /// overlays.
    pub layers: Vec<Layer>,
    /// Drawn over the graded frames in the same pass, each over its frame range.
    pub overlays: Vec<Overlay>,
}

impl Pipeline {
    pub fn new(input: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
            start: Duration::ZERO,
            frames: None,
            context: FrameContext::default(),
            frame_rate: None,
            stages: Vec::new(),
            encoder: EncoderSettings::default(),
            device: Device::Auto,
            decode: Decode::default(),
            layers: Vec::new(),
            overlays: Vec::new(),
        }
    }

    #[must_use]
    pub fn stage(mut self, stage: Stage) -> Self {
        self.stages.push(stage);
        self
    }

    #[must_use]
    pub fn grade(self, grade: Grade) -> Self {
        self.stage(Stage::Grade(grade))
    }

    /// Source range: start at `start`, output `frames` frames.
    #[must_use]
    pub fn range(mut self, start: Duration, frames: Option<u64>) -> Self {
        self.start = start;
        self.frames = frames;
        self
    }

    /// See [`Pipeline::context`].
    #[must_use]
    pub fn context(mut self, before: u64, after: u64) -> Self {
        self.context = FrameContext { before, after };
        self
    }

    #[must_use]
    pub fn decode(mut self, decode: Decode) -> Self {
        self.decode = decode;
        self
    }

    #[must_use]
    pub fn frame_rate(mut self, rate: Rational) -> Self {
        self.frame_rate = Some(rate);
        self
    }

    #[must_use]
    pub fn encoder(mut self, encoder: EncoderSettings) -> Self {
        self.encoder = encoder;
        self
    }

    /// Overlays (text) to composite, frame ranges relative to the first output frame.
    #[must_use]
    pub fn overlays(mut self, overlays: Vec<Overlay>) -> Self {
        self.overlays = overlays;
        self
    }

    /// Layers to composite, frame ranges relative to the first output frame.
    #[must_use]
    pub fn layers(mut self, layers: Vec<Layer>) -> Self {
        self.layers = layers;
        self
    }

    #[must_use]
    pub fn device(mut self, device: Device) -> Self {
        self.device = device;
        self
    }

    /// Runs the pipeline on `backend`.
    pub fn run(&self, backend: &dyn Backend) -> Result<RunStats> {
        backend.run(self)
    }
}

/// An implementation of ffcine's operations. The library's types don't depend on any backend;
/// the ffmpeg + libplacebo one is [`crate::backend::Ffmpeg`].
pub trait Backend {
    fn name(&self) -> &'static str;
    fn probe(&self, path: &Path) -> Result<MediaInfo>;
    fn run(&self, pipeline: &Pipeline) -> Result<RunStats>;
}

/// What a [`Pipeline::run`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunStats {
    /// Frames encoded.
    pub frames: u64,
    /// Time before the first frame: opening, codec setup, LUT/shader upload (and, on the first
    /// run of a process, device init).
    pub setup: Duration,
    /// Time until the first frame was handed to the encoder.
    pub first_frame: Duration,
    /// Total wall time.
    pub wall: Duration,
}

impl RunStats {
    /// Frames per second after the first frame (excludes setup and pipeline fill).
    #[allow(clippy::cast_precision_loss)]
    pub fn steady_fps(&self) -> Option<f64> {
        let t = self.wall.checked_sub(self.first_frame)?.as_secs_f64();
        (self.frames > 1 && t > 0.0).then(|| (self.frames - 1) as f64 / t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder() {
        let p = Pipeline::new("in.mov", "out.mkv")
            .grade(Grade::default())
            .device(Device::Cuda(0));
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.device, Device::Cuda(0));
        assert_eq!(p.encoder, EncoderSettings::nvenc_hevc_master());
    }
}
