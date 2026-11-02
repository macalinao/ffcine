//! Video stabilization: optical-flow motion detect on the GPU, camera path smoothing on the CPU,
//! and a per-frame warp ([`Mat3`], output pixel -> source pixel) for the grade pass.
//!
//! 1. [`detect`] (feature `nvof`): NVDEC decode, `scale_cuda` to an analysis size, NVIDIA
//!    optical flow between consecutive frames, a robust similarity per frame
//!    ([`motion::estimate`]). Cached on disk by source and parameters ([`detect_cached`]).
//! 2. [`stabilize`]: the camera path, vidstab's Gaussian smoothing, and the static zoom that
//!    keeps every frame free of black borders. Cheap, so it is recomputed every time.
//! 3. [`hook::warp_hook`]: the warps as an mpv `.hook` for libplacebo.
//!
//! [`gyro`] is the other way to get the motions of step 1, from what the camera recorded;
//! [`detect_motion`] chooses between them.

// Numeric code: pixel counts and grid indices convert to f64 and back all over, always in range.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

pub mod cache;
// Public for ffcine-render's NVOF denoise motion; the driver API is raw by nature.
#[cfg(feature = "nvof")]
#[allow(clippy::missing_safety_doc, clippy::not_unsafe_ptr_arg_deref)]
pub mod cuda;
#[cfg(feature = "nvof")]
mod detect;
pub mod fx;
pub mod gyro;
pub mod hook;
pub mod motion;
#[cfg(feature = "nvof")]
pub mod nvof;
#[cfg(feature = "telemetry")]
mod parser;
pub mod path;
#[cfg(feature = "nvof")]
mod read;

#[cfg(feature = "nvof")]
pub use cache::detect_cached;
pub use cache::motions_text;
#[cfg(feature = "nvof")]
pub use detect::{analysis_size, detect};
pub use gyro::GyroParams;
pub use motion::{EstimateParams, Motion};
pub use path::Zoom;

use std::fmt;
use std::str::FromStr;

use ffcine_core::Error;
use ffcine_core::color::named_enum;
use ffcine_core::geom::Mat3;
use ffcine_core::probe::Rational;

/// The frames to analyse, indexed like render.py's shots: `-ss start` on the input, then
/// `setpts=PTS-STARTPTS,fps=rate`, `frames` frames (the last repeats if the source runs out).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceRange {
    /// Seconds from the start of the file.
    pub start: f64,
    pub frames: usize,
    pub rate: Rational,
}

named_enum!(
    /// Optical flow engine speed/quality trade-off (NVOF perf level).
    #[derive(Default)]
    FlowQuality {
        #[default]
        Slow = "slow",
        Medium = "medium",
        Fast = "fast",
    }
);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectParams {
    /// Analysis width in pixels (height keeps the aspect, rounded to even). Flow runs at this
    /// size; motions are rescaled to source pixels.
    pub analysis_width: u32,
    pub quality: FlowQuality,
    /// Flow vector spacing in analysis pixels: 1, 2 or 4.
    pub grid: u32,
    pub estimate: EstimateParams,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self {
            analysis_width: 1920,
            quality: FlowQuality::Slow,
            grid: 4,
            estimate: EstimateParams::default(),
        }
    }
}

/// What detect found: per output frame, the motion from it to the previous frame in source
/// pixels (`motions[0]` is the identity).
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    pub width: u32,
    pub height: u32,
    pub analysis_width: u32,
    pub analysis_height: u32,
    pub motions: Vec<Motion>,
}

named_enum!(
    /// Where a stabilizer's camera motion comes from.
    #[derive(Default)]
    MotionSource {
        /// Gyro if the file has telemetry `auto` trusts, else optical flow.
        #[default]
        Auto = "auto",
        /// The file's gyro or attitude telemetry; an error if it has none.
        Gyro = "gyro",
        /// Optical flow.
        Flow = "flow",
    }
);

/// Which motion source to use, and how.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MotionOptions {
    pub source: MotionSource,
    pub gyro: GyroParams,
}

/// What [`detect_motion`] found, and how.
#[derive(Debug, Clone, PartialEq)]
pub struct MotionDetection {
    pub detection: Detection,
    /// [`MotionSource::Gyro`] or [`MotionSource::Flow`], never `Auto`.
    pub source: MotionSource,
    /// Why `auto` fell back to flow.
    pub fallback: Option<String>,
    /// Read from the cache.
    pub cached: bool,
}

/// The camera motion of `range` from the source `options` chooses, through the cache if `cache`.
/// With [`MotionSource::Auto`], a file with no telemetry it trusts goes to optical flow and says
/// why in [`MotionDetection::fallback`].
#[cfg(feature = "nvof")]
pub fn detect_motion(
    path: &std::path::Path,
    range: &SourceRange,
    params: &DetectParams,
    options: &MotionOptions,
    cache: bool,
) -> ffcine_core::Result<MotionDetection> {
    let flow = |fallback| -> ffcine_core::Result<MotionDetection> {
        let (detection, cached) = if cache {
            detect_cached(path, range, params)?
        } else {
            (detect(path, range, params)?, false)
        };
        Ok(MotionDetection {
            detection,
            source: MotionSource::Flow,
            fallback,
            cached,
        })
    };
    if options.source == MotionSource::Flow {
        return flow(None);
    }
    let strict = options.source == MotionSource::Gyro;
    let gyro = || -> Result<Detection, String> {
        let (telemetry, size) = read::read(path, range, strict)?;
        if let Some(why) = telemetry.auto_refusal.filter(|_| !strict) {
            return Err(why.into());
        }
        gyro::detection(&telemetry, range, &options.gyro, size)
    };
    let got = if cache {
        cache::gyro_cached(path, range, options, gyro)?
    } else {
        gyro().map(|d| (d, false))
    };
    match got {
        Ok((detection, cached)) => Ok(MotionDetection {
            detection,
            source: MotionSource::Gyro,
            fallback: None,
            cached,
        }),
        Err(why) if strict => Err(Error::Pipeline(format!("gyro: {why}"))),
        Err(why) => flow(Some(why)),
    }
}

/// Smoothing and zoom; the defaults are render.py's (`smoothing` 20, optzoom 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StabilizeParams {
    /// Gaussian half-window in frames, as vidstab's `smoothing`.
    pub smoothing: u32,
    pub zoom: Zoom,
}

impl Default for StabilizeParams {
    fn default() -> Self {
        Self {
            smoothing: 20,
            zoom: Zoom::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stabilization {
    /// Per output frame: output pixel -> source pixel, zoom included.
    pub transforms: Vec<Mat3>,
    /// The static zoom applied (1.0 = none).
    pub zoom: f64,
    /// The zoom needed for no black borders (`INFINITY` if no zoom suffices).
    pub min_zoom: f64,
}

/// Smooths the detected camera path and computes the per-frame warps.
pub fn stabilize(det: &Detection, params: &StabilizeParams) -> Stabilization {
    let (w, h) = (f64::from(det.width), f64::from(det.height));
    let centre = [w / 2.0, h / 2.0];
    let actual = path::camera_path(&det.motions, centre);
    let smooth = path::gaussian(&actual, params.smoothing);
    let mut transforms = path::corrections(&actual, &smooth, centre);
    let min_zoom = path::min_zoom(&transforms, w, h);
    let zoom = path::choose_zoom(params.zoom, min_zoom);
    path::apply_zoom(&mut transforms, centre, zoom);
    Stabilization {
        transforms,
        zoom,
        min_zoom,
    }
}

/// Folds a static source crop `(x, y, width, height)` (source pixels) into `transforms`, for a
/// pass that renders the full `width x height` frame: output pixel `d` of the full-size
/// intermediate maps to crop position `(x + d.x * cw / w, y + d.y * ch / h)`, then through the
/// warp. libplacebo only shows the `MAIN` hook the crop, so the grade pass crops this way instead.
pub fn fold_crop(transforms: &[Mat3], crop: [f64; 4], width: u32, height: u32) -> Vec<Mat3> {
    let [x, y, cw, ch] = crop;
    let a = [
        [cw / f64::from(width), 0.0, x],
        [0.0, ch / f64::from(height), y],
        [0.0, 0.0, 1.0],
    ];
    transforms
        .iter()
        .map(|m| ffcine_core::geom::mul(m, &a))
        .collect()
}

/// Warps as text: per frame, the row-major 3x3 matrix (output pixel -> source pixel).
pub fn transforms_text(transforms: &[Mat3]) -> String {
    use std::fmt::Write as _;
    let mut s =
        String::from("# frame m00 m01 m02 m10 m11 m12 m20 m21 m22; output px -> source px\n");
    for (k, m) in transforms.iter().enumerate() {
        let _ = write!(s, "{k}");
        for v in m.iter().flatten() {
            let _ = write!(s, " {v}");
        }
        s.push('\n');
    }
    s
}

/// High-frequency residual motion of a clip, the stabilization quality metric: RMS of the
/// frame-to-frame motion minus its Gaussian-smoothed (`sigma` frames) version.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Jitter {
    /// Pixels.
    pub tx: f64,
    pub ty: f64,
    /// Degrees.
    pub angle: f64,
    /// Frame pairs that counted.
    pub pairs: usize,
    /// Their mean motion-fit confidence.
    pub confidence: f64,
}

/// [`Jitter`] of `motions` (as from [`detect`]), skipping the frame pairs ending at the indices
/// in `exclude` (cuts, punch-ins) in both the smoothing and the RMS.
pub fn jitter(motions: &[Motion], sigma: f64, exclude: &[usize]) -> Jitter {
    let keep: Vec<usize> = (1..motions.len())
        .filter(|i| !exclude.contains(i))
        .collect();
    let series = |f: fn(&Motion) -> f64| -> f64 {
        let reach = (3.0 * sigma).ceil() as usize;
        let mut sum2 = 0.0;
        for &i in &keep {
            let (mut s, mut ws) = (0.0, 0.0);
            for &j in &keep {
                if j + reach >= i && j <= i + reach {
                    let d = j as f64 - i as f64;
                    let w = (-d * d / (2.0 * sigma * sigma)).exp();
                    s += w * f(&motions[j]);
                    ws += w;
                }
            }
            sum2 += (f(&motions[i]) - s / ws).powi(2);
        }
        (sum2 / keep.len().max(1) as f64).sqrt()
    };
    Jitter {
        tx: series(|m| m.tx),
        ty: series(|m| m.ty),
        angle: series(|m| m.angle.to_degrees()),
        pairs: keep.len(),
        confidence: keep.iter().map(|&i| motions[i].confidence).sum::<f64>()
            / keep.len().max(1) as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_core::geom;

    fn shaky(n: usize) -> Vec<Motion> {
        (0..n)
            .map(|i| {
                if i == 0 {
                    return Motion::IDENTITY;
                }
                let t = i as f64;
                Motion {
                    // A slow pan plus shake.
                    tx: -3.0 + 2.5 * (t * 1.7).sin(),
                    ty: 1.5 * (t * 2.3).cos(),
                    angle: 0.002 * (t * 1.1).sin(),
                    scale: 1.0,
                    confidence: 1.0,
                }
            })
            .collect()
    }

    #[test]
    fn stabilizing_removes_jitter() {
        let det = Detection {
            width: 3840,
            height: 2160,
            analysis_width: 1920,
            analysis_height: 1080,
            motions: shaky(120),
        };
        let before = jitter(&det.motions, 5.0, &[]);
        let st = stabilize(&det, &StabilizeParams::default());
        assert_eq!(st.transforms.len(), 120);
        assert!(st.zoom > 1.0 && st.zoom < 1.05, "{st:?}");
        // The stabilized clip's own motion: what output frame k shows, mapped to frame k-1.
        let c = [1920.0, 1080.0];
        let after: Vec<Motion> = (0..120)
            .map(|k| {
                if k == 0 {
                    return Motion::IDENTITY;
                }
                let m = det.motions[k].to_mat3(c);
                let prev = geom::inverse(&st.transforms[k - 1]).unwrap();
                let rel = geom::mul(&prev, &geom::mul(&m, &st.transforms[k]));
                let (tx, ty, angle, scale) = geom::decompose_similarity(&rel, c);
                Motion {
                    tx,
                    ty,
                    angle,
                    scale,
                    confidence: 1.0,
                }
            })
            .collect();
        let after = jitter(&after, 5.0, &[]);
        assert!(after.tx < before.tx * 0.1, "{before:?} -> {after:?}");
        assert!(after.ty < before.ty * 0.1, "{before:?} -> {after:?}");
        assert!(after.angle < before.angle * 0.1, "{before:?} -> {after:?}");
        assert_eq!(jitter(&det.motions, 5.0, &[50]).pairs, 118);
    }

    #[cfg(feature = "nvof")]
    #[test]
    fn no_gyro_is_an_error_only_when_asked_for() {
        let dir = std::env::temp_dir().join(format!("ffcine-motion-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("not-a-video.mp4");
        std::fs::write(&f, b"not a video").unwrap();
        let range = SourceRange {
            start: 0.0,
            frames: 2,
            rate: Rational { num: 25, den: 1 },
        };
        let run = |source| {
            let options = MotionOptions {
                source,
                ..MotionOptions::default()
            };
            detect_motion(&f, &range, &DetectParams::default(), &options, false)
                .unwrap_err()
                .to_string()
        };
        // Asked for gyro: the gyro path's error. Auto: on to flow, which fails on this file itself.
        assert!(run(MotionSource::Gyro).contains("gyro: "));
        assert!(!run(MotionSource::Auto).contains("gyro: "));
        assert!(!run(MotionSource::Flow).contains("gyro: "));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn crop_folds_before_the_warp() {
        let t = [geom::translation(10.0, -4.0)];
        let f = fold_crop(&t, [100.0, 50.0, 1920.0, 1080.0], 3840, 2160);
        // Full-frame output corner (3840, 2160) is crop corner (2020, 1130), then the warp.
        let p = geom::apply(&f[0], [3840.0, 2160.0]);
        assert!(
            (p[0] - 2030.0).abs() < 1e-9 && (p[1] - 1126.0).abs() < 1e-9,
            "{p:?}"
        );
    }
}
