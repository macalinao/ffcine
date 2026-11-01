//! Reframe in the grade pass: the per-frame windows become `Grade::crops` (libplacebo's own
//! sub-pixel crop and EWA scaler), or, with a stabilizer warp, one `MAIN` hook that samples the
//! source at `warp_i * crop_i` so warp and crop are a single resample in the same
//! `pl_render_image` call.

use std::fmt::Write as _;

use ffcine_core::geom::{self, Mat3};
use ffcine_render::{Grade, Hook, ParamTrack, Size};
use ffcine_stabilize::hook::WARP_GLSL;

use crate::profile::{Profile, SafeArea};
use crate::reframe::Frame;

/// Hook names, so `ffcine-hooks` can order them against user shaders.
pub const GEOMETRY_HOOK: &str = "ffcine:geometry";
pub const SAFE_AREA_HOOK: &str = "ffcine:safe-area";

const M: [&str; 9] = [
    "m00", "m01", "m02", "m10", "m11", "m12", "m20", "m21", "m22",
];

/// Puts `frames` (one per output frame, from [`crate::Reframe::frames`]) and the stabilizer's
/// `warps` (output -> source, before the crop; empty for none) into `grade`, rendering at `out`'s
/// size, plus the safe-area overlay if `out` asks for it.
///
/// Without warps the windows are `Grade::crops`: libplacebo crops sub-pixel and scales once with
/// the grade's scalers. With warps, `MAIN` is resampled (Catmull-Rom, as the stabilizer) at
/// `warp_i * window_i` into an intermediate the size of the largest window, which the grade's
/// scaler then takes to the output size; the image crop is the whole frame so the warp can reach
/// outside the window.
pub fn apply(
    grade: &mut Grade,
    frames: &[Frame],
    warps: &[Mat3],
    source: (u32, u32),
    out: &Profile,
) {
    grade.size = Some(Size {
        width: out.width,
        height: out.height,
    });
    grade.place = frames.first().and_then(|f| f.place).map(|r| r.to_crop());
    if warps.is_empty() {
        grade.crop = None;
        grade.crops = frames.iter().map(|f| f.crop.to_crop()).collect();
    } else {
        grade.crop = None;
        grade.crops.clear();
        grade.hooks.insert(0, geometry_hook(frames, warps, source));
    }
    if let Some(area) = out.safe_area {
        grade.hooks.push(safe_area_hook(&area));
    }
}

/// Intermediate size for the warp path: the largest window, in the windows' aspect, rounded up.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn intermediate_size(frames: &[Frame]) -> (u32, u32) {
    let w = frames.iter().map(|f| f.crop.w).fold(1.0_f64, f64::max);
    let ar = frames.first().map_or(1.0, |f| f.crop.aspect());
    (w.ceil() as u32, (w / ar).round().max(1.0) as u32)
}

/// Per-frame `warp_i * window_i`: intermediate pixel -> source pixel. Past the end of either
/// list its last entry holds.
pub fn compose(frames: &[Frame], warps: &[Mat3], iw: u32, ih: u32) -> Vec<Mat3> {
    let n = frames.len().max(warps.len());
    (0..n)
        .map(|i| {
            let crop = frames.get(i).or(frames.last()).map_or(geom::IDENTITY, |f| {
                f.crop.to_mat3(f64::from(iw), f64::from(ih))
            });
            let warp = warps.get(i).or(warps.last()).unwrap_or(&geom::IDENTITY);
            geom::mul(warp, &crop)
        })
        .collect()
}

/// The `MAIN` hook resampling the source at the composed matrices, given per frame as nine
/// dynamic `//!PARAM`s (`m00` .. `m22`, row-major).
pub fn geometry_hook(frames: &[Frame], warps: &[Mat3], (sw, sh): (u32, u32)) -> Hook {
    let (iw, ih) = intermediate_size(frames);
    let mats = compose(frames, warps, iw, ih);
    let mut s = String::with_capacity(4096);
    for (k, name) in M.iter().enumerate() {
        let init = if k % 4 == 0 { "1.0" } else { "0.0" };
        let _ = write!(s, "//!PARAM {name}\n//!TYPE DYNAMIC float\n{init}\n\n");
    }
    let _ = write!(
        s,
        "//!HOOK MAIN
//!BIND HOOKED
//!WIDTH {iw}
//!HEIGHT {ih}
//!DESC ffcine reframe + warp ({sw}x{sh} -> {iw}x{ih})
#define WARP_TEX HOOKED_tex
#define WARP_PT HOOKED_pt
#define WARP_SIZE vec2({sw}.0, {sh}.0)
{WARP_GLSL}
vec4 hook() {{
    return ffcine_warp(vec3(m00, m01, m02), vec3(m10, m11, m12), vec3(m20, m21, m22),
                       HOOKED_pos * vec2({iw}.0, {ih}.0));
}}
"
    );
    #[allow(clippy::cast_possible_truncation)]
    let tracks = M
        .iter()
        .enumerate()
        .map(|(k, name)| ParamTrack {
            name: (*name).to_owned(),
            values: mats.iter().map(|m| m[k / 3][k % 3] as f32).collect(),
        })
        .collect();
    Hook {
        name: GEOMETRY_HOOK.to_owned(),
        source: s,
        params: Vec::new(),
        tracks,
    }
}

/// Review overlay at `PREOUTPUT` (RGB, after scaling, tone mapping and sharpening): dims
/// everything outside `area` and outlines it.
pub fn safe_area_hook(area: &SafeArea) -> Hook {
    let SafeArea {
        top,
        right,
        bottom,
        left,
    } = *area;
    let source = format!(
        "//!HOOK PREOUTPUT
//!BIND HOOKED
//!DESC ffcine safe area
vec4 hook() {{
    vec4 c = HOOKED_texOff(vec2(0.0));
    vec2 s = HOOKED_size;
    vec2 p = HOOKED_pos * s;
    vec2 lo = vec2({left:.5}, {top:.5}) * s;
    vec2 hi = vec2({:.5}, {:.5}) * s;
    float lw = max(2.0, s.y / 540.0);
    bool inside = all(greaterThanEqual(p, lo)) && all(lessThan(p, hi));
    vec2 d = min(abs(p - lo), abs(p - hi));
    bool edge = all(greaterThanEqual(p, lo - lw)) && all(lessThan(p, hi + lw)) && min(d.x, d.y) < lw;
    if (!inside)
        c.rgb *= 0.5;
    if (edge)
        c.rgb = vec3(1.0, 0.85, 0.0);
    return c;
}}
",
        1.0 - right,
        1.0 - bottom,
    );
    Hook {
        name: SAFE_AREA_HOOK.to_owned(),
        source,
        ..Hook::default()
    }
}
