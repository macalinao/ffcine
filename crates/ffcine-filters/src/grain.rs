//! Film grain: luma-weighted, optionally coarse (value noise), fresh every frame. A pointwise
//! kernel at [`Point::Output`](crate::Point::Output), run as an mpv `.hook` on the grade pass's
//! `OUTPUT` stage so it costs no dispatch of its own: libplacebo folds it into the pass that
//! writes the target.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grain {
    /// Grain sigma in 8-bit code values at mid-grey; 0 = off.
    pub strength: f32,
    /// Grain cell size in output pixels (1 = per pixel, 2-3 = coarser, more filmic).
    pub size: f32,
    /// Chroma grain relative to luma grain (0 = monochrome).
    pub chroma: f32,
}

impl Default for Grain {
    fn default() -> Self {
        Self {
            strength: 3.0,
            size: 1.5,
            chroma: 0.0,
        }
    }
}

/// `vec3 ffc_grain(vec2 pos, uint seed)` and its helpers; `pos` in output pixels. Standard
/// normal per channel (r = luma, gb = chroma), as bilinear value noise over cells of `size`.
fn noise_glsl(size: f32) -> String {
    format!(
        r"
uint ffc_hash(uvec3 v) {{
    v = v * 1664525u + 1013904223u;
    v.x += v.y * v.z; v.y += v.z * v.x; v.z += v.x * v.y;
    v ^= v >> 16u;
    v.x += v.y * v.z; v.y += v.z * v.x; v.z += v.x * v.y;
    return v.x ^ v.y ^ v.z;
}}
vec3 ffc_gauss(ivec2 c, uint seed) {{
    vec3 g = vec3(0.0);
    for (uint i = 0u; i < 4u; i++) {{
        uint h = ffc_hash(uvec3(uvec2(c), seed * 4u + i));
        g += vec3(h & 1023u, (h >> 10) & 1023u, (h >> 20) & 1023u) / 1023.0;
    }}
    return (g - 2.0) * 1.7320508;
}}
vec3 ffc_grain(vec2 pos, uint seed) {{
    vec2 p = pos / {size:.4} - 0.5;
    ivec2 i = ivec2(floor(p));
    vec2 f = fract(p);
    f = f * f * (3.0 - 2.0 * f);
    vec3 a = mix(ffc_gauss(i, seed), ffc_gauss(i + ivec2(1, 0), seed), f.x);
    vec3 b = mix(ffc_gauss(i + ivec2(0, 1), seed), ffc_gauss(i + ivec2(1, 1), seed), f.x);
    // Interpolation lowers the variance; put it back so `strength` stays the sigma.
    return mix(a, b, f.y) * {gain:.4};
}}
",
        size = size.max(1.0),
        gain = if size > 1.0 { 1.6 } else { 1.0 },
    )
}

/// The grain as an mpv user shader on `OUTPUT` (graded, scaled RGB, before dither). `None` when
/// off.
pub fn hook(g: &Grain) -> Option<String> {
    if g.strength <= 0.0 {
        return None;
    }
    Some(format!(
        r"//!HOOK OUTPUT
//!BIND HOOKED
//!DESC ffcine grain strength={s} size={z} chroma={c}
{noise}
vec4 hook() {{
    vec4 c = HOOKED_texOff(vec2(0.0));
    float y = dot(c.rgb, vec3(0.2126, 0.7152, 0.0722));
    // Strongest in the midtones, as on film; never zero, so shadows don't look plastic.
    float resp = mix(0.35, 1.0, 4.0 * y * (1.0 - y));
    vec3 n = ffc_grain(HOOKED_pos * HOOKED_size, uint(frame)) * ({s:.4} / 255.0) * resp;
    c.rgb += n.r + {c:.4} * vec3(n.g, -0.5 * (n.g + n.b), n.b);
    return c;
}}
",
        s = g.strength,
        z = g.size,
        c = g.chroma,
        noise = noise_glsl(g.size),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_off_and_on() {
        assert!(
            hook(&Grain {
                strength: 0.0,
                ..Grain::default()
            })
            .is_none()
        );
        let h = hook(&Grain::default()).unwrap();
        assert!(h.starts_with("//!HOOK OUTPUT"));
        assert!(h.contains("uint(frame)"));
    }
}
