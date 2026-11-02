//! ffcine: GPU video finishing. Decode, grade (3D LUTs, tone and gamut mapping, crop/scale, user
//! shaders) and encode, with frames kept in VRAM end to end.
//!
//! The types here ([`Pipeline`], [`Grade`], [`Lut`], [`ColorSpace`], [`EncoderSettings`]) are
//! backend-neutral; [`backend::Ffmpeg`] runs them on ffmpeg's libav* and libplacebo/Vulkan.
//!
//! ```no_run
//! use ffcine::{Backend, Grade, Lut, LutStage, LutType, Pipeline, backend};
//!
//! let ff = backend::Ffmpeg::new()?;
//! println!("{}", ff.probe("in.mov".as_ref())?);
//! let grade = Grade {
//!     lut: Some(LutStage { lut: Lut::from_cube_file("grade.cube")?, kind: LutType::Normalized }),
//!     ..Grade::default()
//! };
//! Pipeline::new("in.mov", "out.mkv").grade(grade).run(&ff)?;
//! # Ok::<(), ffcine::Error>(())
//! ```

//!
//! This crate is a facade: each part lives in its own crate (`docs/design.md#crates`) and is
//! re-exported here under the module names it always had.

pub use ffcine_assembly as assembly;
pub use ffcine_audio as audio;
pub use ffcine_breakdown as breakdown;
pub use ffcine_core::{ColorSpace, Error, Lut, LutType, MediaInfo, Rational, Result};
pub use ffcine_core::{camera, color, geom, lut, probe};
pub use ffcine_delivery as delivery;
pub use ffcine_filters as filters;
pub use ffcine_filters::{Denoise, Grain, Spatial, Temporal};
pub use ffcine_hooks as hooks;
pub use ffcine_import as import;
pub use ffcine_ingest as ingest;
pub use ffcine_reframe as reframe;
pub use ffcine_render::{
    Backend, Bake, Codec, ColorMapTuning, ColorOverride, Crop, Deband, Decode, DeliveryProfile,
    Device, Dither, DitherMethod, EncoderSettings, GamutMapping, Grade, HdrMetadata, Hook, Image,
    Layer, LayerSource, LutStage, Mask, Overlay, OverlayPart, ParamTrack, Pipeline, PixelFormat,
    RateControl, RunStats, Scaler, Scaling, Size, Stage, ToneMapping, Warp,
};
pub use ffcine_render::{
    backend, bake, encode, extra, grade, layer, overlay, pipeline, profile, shaders,
};
pub use ffcine_stabilize as stabilize;
pub use ffcine_text as text;
pub use ffcine_timeline as timeline;
pub use ffcine_triage as triage;

/// Stabilizes a pipeline's shot in its grade pass: camera motion over the pipeline's range from
/// the source `motion` chooses (cached unless `cache` is false), smoothing and zoom per `params`, and the warps set on the first
/// grade stage (one is added if there is none). The range's rate is the pipeline's output rate,
/// else the source's (probed on `backend`); with no `frames` it runs to the end of the source.
///
/// This is what `ffcine grade --stabilize` and a timeline clip's `ffcine.fx.stabilize/1`
/// ([`stabilize::fx::params`]) do.
#[cfg(feature = "nvof")]
pub fn stabilize_pipeline(
    backend: &dyn Backend,
    pipeline: &mut Pipeline,
    params: &stabilize::StabilizeParams,
    motion: &stabilize::MotionOptions,
    cache: bool,
) -> Result<(stabilize::Stabilization, stabilize::MotionDetection)> {
    let start = pipeline.start.as_secs_f64();
    let (rate, frames) = match (pipeline.frame_rate, pipeline.frames) {
        (Some(r), Some(n)) => (r, n),
        (r, n) => {
            let info = backend.probe(&pipeline.input)?;
            let rate = r
                .or_else(|| info.video().and_then(|(_, v)| v.frame_rate))
                .ok_or_else(|| Error::Pipeline("stabilize: source has no frame rate".into()))?;
            let n = n.unwrap_or_else(|| {
                let left = info.duration.map_or(0.0, |d| d.as_secs_f64()) - start;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let n = (left.max(0.0) * rate.as_f64()).round() as u64;
                n
            });
            (rate, n)
        }
    };
    let range = stabilize::SourceRange {
        start,
        frames: usize::try_from(frames).unwrap_or(usize::MAX),
        rate,
    };
    let detect = stabilize::DetectParams::default();
    let det = stabilize::detect_motion(&pipeline.input, &range, &detect, motion, cache)?;
    let st = stabilize::stabilize(&det.detection, params);
    let warp = Some(Warp {
        transforms: st.transforms.clone(),
    });
    if let Some(Stage::Grade(g)) = pipeline.stages.first_mut() {
        g.warp = warp;
    } else {
        pipeline.stages.insert(
            0,
            Stage::Grade(Grade {
                warp,
                ..Grade::default()
            }),
        );
    }
    Ok((st, det))
}
