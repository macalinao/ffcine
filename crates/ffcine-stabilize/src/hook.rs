//! The per-frame warps as an mpv `.hook` user shader for libplacebo.
//!
//! The hook runs at `MAIN` (RGB, source resolution, before scaling) and resamples `HOOKED` at
//! `M_i * (output pixel)` with a 16-tap Catmull-Rom bicubic (vidstab's `interpol=bicubic`
//! kernel), black outside the source (`crop=black`). The matrices live in an embedded
//! `//!TEXTURE` (rgba32f, 3 x frames: one texel per matrix row), indexed by libplacebo's `frame`.
//!
//! libplacebo only shows `MAIN` the source crop, so with libavfilter the warp needs its own
//! `libplacebo` pass on the full frame ahead of the one that crops. ffcine's own grade pass
//! instead folds the crop into the matrices and renders the full frame
//! ([`warp_hook_dynamic`], frame index set per frame). [`WARP_GLSL`] is the sampling code alone.

use std::fmt::Write as _;

use ffcine_core::geom::Mat3;

/// The warp sampling on its own: `ffcine_warp(m0, m1, m2, d)` returns the source sampled at
/// `M * (d, 1)` (rows `m0..m2`) for output pixel position `d`. Needs `WARP_TEX(pos)` (normalised
/// sampling), `WARP_PT` (texel size) and `WARP_SIZE` (source size in pixels) defined first.
pub const WARP_GLSL: &str = r"
vec4 ffcine_cubic(float f) {
    // Catmull-Rom weights for taps -1, 0, 1, 2 at fraction f.
    return vec4(f * (-0.5 + f * (1.0 - 0.5 * f)),
                1.0 + f * f * (-2.5 + 1.5 * f),
                f * (0.5 + f * (2.0 - 1.5 * f)),
                f * f * (-0.5 + 0.5 * f));
}

vec4 ffcine_warp(vec3 m0, vec3 m1, vec3 m2, vec2 d) {
    vec3 h = vec3(dot(m0, vec3(d, 1.0)), dot(m1, vec3(d, 1.0)), dot(m2, vec3(d, 1.0)));
    vec2 p = h.xy / h.z;
    if (any(lessThan(p, vec2(0.0))) || any(greaterThan(p, WARP_SIZE)))
        return vec4(0.0, 0.0, 0.0, 1.0);
    // Texel centres sit at i + 0.5.
    vec2 t = p - 0.5;
    vec2 i0 = floor(t);
    vec4 wx = ffcine_cubic(t.x - i0.x);
    vec4 wy = ffcine_cubic(t.y - i0.y);
    vec4 acc = vec4(0.0);
    for (int j = 0; j < 4; j++) {
        vec4 row = vec4(0.0);
        for (int i = 0; i < 4; i++)
            row += wx[i] * WARP_TEX((i0 + vec2(float(i) - 0.5, float(j) - 0.5)) * WARP_PT);
        acc += wy[j] * row;
    }
    return acc;
}
";

/// Name of the dynamic `int` parameter [`warp_hook_dynamic`] reads the frame index from.
pub const FRAME_PARAM: &str = "ffcine_stab_frame";

/// The hook for `transforms` on a `width x height` source. `passes` is how many hook passes run
/// per frame in the shader file this ends up in (libplacebo's `frame` counts every pass of the
/// file, starting at 1), so frame `i` reads row `(frame - 1) / passes`.
///
/// Comments in the generated GLSL never contain the directive marker, which libplacebo would parse
/// even inside a comment.
pub fn warp_hook(transforms: &[Mat3], width: u32, height: u32, passes: u32) -> String {
    let index = format!("(frame - 1) / {}", passes.max(1));
    hook_text("", &index, transforms, width, height)
}

/// As [`warp_hook`], but frame `i` reads row [`FRAME_PARAM`], a `DYNAMIC int` the renderer sets
/// before each frame (through the parsed hook's `pl_hook_par::data`). Doesn't depend on how many
/// passes or frames the hook has seen, so one parsed hook can render any range of a shot.
pub fn warp_hook_dynamic(transforms: &[Mat3], width: u32, height: u32) -> String {
    let param = format!(
        "//!PARAM {FRAME_PARAM}\n//!DESC output frame index\n//!TYPE DYNAMIC int\n//!MINIMUM 0\n0\n\n"
    );
    hook_text(&param, FRAME_PARAM, transforms, width, height)
}

fn hook_text(prelude: &str, index: &str, transforms: &[Mat3], width: u32, height: u32) -> String {
    let n = transforms.len().max(1);
    let mut s = String::with_capacity(n * 100 + 2048);
    s.push_str(prelude);
    let _ = write!(
        s,
        "//!HOOK MAIN
//!BIND HOOKED
//!BIND FFCINE_STAB
//!DESC ffcine stabilize warp ({n} frames, {width}x{height})
#define WARP_TEX HOOKED_tex
#define WARP_PT HOOKED_pt
#define WARP_SIZE vec2({width}.0, {height}.0)
{WARP_GLSL}
vec4 hook() {{
    int idx = clamp({index}, 0, {last});
    vec3 m0 = texelFetch(FFCINE_STAB, ivec2(0, idx), 0).xyz;
    vec3 m1 = texelFetch(FFCINE_STAB, ivec2(1, idx), 0).xyz;
    vec3 m2 = texelFetch(FFCINE_STAB, ivec2(2, idx), 0).xyz;
    // Output pixel position in source pixels (the hook runs at source size).
    return ffcine_warp(m0, m1, m2, HOOKED_pos * WARP_SIZE);
}}

//!TEXTURE FFCINE_STAB
//!SIZE 3 {n}
//!FORMAT rgba32f
//!FILTER NEAREST
",
        last = n - 1,
    );
    let identity = [ffcine_core::geom::IDENTITY];
    let rows = if transforms.is_empty() {
        &identity[..]
    } else {
        transforms
    };
    for m in rows {
        for row in m {
            for v in [row[0], row[1], row[2], 0.0] {
                for b in (v as f32).to_le_bytes() {
                    let _ = write!(s, "{b:02x}");
                }
            }
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_core::geom;

    #[test]
    fn hook_layout() {
        let t = [geom::IDENTITY, geom::translation(1.5, -2.0)];
        let h = warp_hook(&t, 3840, 2160, 3);
        assert!(h.starts_with("//!HOOK MAIN\n"));
        assert!(h.contains("(frame - 1) / 3, 0, 1)"));
        assert!(h.contains("//!SIZE 3 2\n"));
        // Directives only at line starts, outside the GLSL comments.
        for line in h.lines() {
            if let Some(i) = line.find("//!") {
                assert_eq!(i, 0, "{line}");
            }
        }
        let hex: Vec<&str> = h.lines().rev().take(2).collect();
        // 3 texels x 4 floats x 4 bytes per frame.
        assert!(hex.iter().all(|l| l.len() == 3 * 4 * 8));
        // Frame 1, row 0: (1, 0, 1.5, 0) as little-endian f32.
        assert!(hex[0].starts_with("0000803f000000000000c03f00000000"));
    }

    #[test]
    fn dynamic_hook_reads_the_param() {
        let h = warp_hook_dynamic(&[geom::IDENTITY], 1920, 1080);
        assert!(h.starts_with("//!PARAM ffcine_stab_frame\n"));
        assert!(h.contains("//!TYPE DYNAMIC int\n"));
        assert!(h.contains("clamp(ffcine_stab_frame, 0, 0)"));
        assert!(h.contains("\n0\n\n//!HOOK MAIN\n"));
    }
}
