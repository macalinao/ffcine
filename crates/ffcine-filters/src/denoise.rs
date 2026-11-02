//! Motion-compensated temporal denoise plus a spatial NLM, as Vulkan compute kernels.
//!
//! The chain per output frame (`docs/denoise.md`), every kernel on the grade's `pl_gpu`:
//!
//! 1. **ingest** (once per decoded frame): [`copy_kernel`] copies each source plane into the ring
//!    slot, [`quarter_kernel`] writes a quarter-resolution luma for motion search.
//! 2. **motion** ([`motion_kernel`], one dispatch per neighbour): block matching, one vector per
//!    16x16 luma block. Exhaustive SAD search at quarter resolution around a predictor (the vector
//!    to the nearer neighbour, scaled by distance) and zero, then a +-2 px full-resolution refine.
//!    Writes `(dx, dy, mad)`: the flow from the centre to the neighbour in luma pixels and the
//!    block's mean absolute difference (8-bit code values) at that vector, the match confidence.
//! 3. **merge** ([`merge_kernel`], one dispatch per plane): warps each neighbour onto the centre
//!    (bilinear vectors and samples) and averages with per-pixel weights from the 3x3 patch
//!    distance and the block confidence. A pixel whose neighbours all mismatch (occlusion, cut,
//!    bad vector) keeps the centre value, which is what keeps it from ghosting.
//! 4. **spatial** ([`spatial_kernel`], one dispatch per plane): NLM with a 3x3 patch over a
//!    `(2r+1)^2` search window, read from a shared-memory tile.
//!
//! Distances are in 8-bit code values of the normalised signal (`FFC_NORM` turns a sampled value
//! into 0..1 for LSB-aligned formats), so strengths mean the same for 8-, 10- and 16-bit input.
//! Planes are sampled as stored (`r16`/`rg16` unorm for P010/P210), never converted to 8 bits.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Temporal (motion-compensated) part.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Temporal {
    /// Frames on each side, 1..=5.
    pub radius: u32,
    /// Motion source.
    pub motion: Motion,
    /// Luma strength: the noise sigma (8-bit code values) treated as "the same pixel".
    pub strength: f32,
    /// Chroma strength; `None` = `strength`.
    pub chroma: Option<f32>,
    /// Point protection (luma): how far, in strengths, a warped neighbour may fall below a bright
    /// centre pixel before it stops counting; 0 = off (`docs/denoise.md#point-protection`).
    pub protect: f32,
}

impl Default for Temporal {
    fn default() -> Self {
        Self {
            radius: 2,
            motion: Motion::default(),
            strength: 4.0,
            chroma: None,
            protect: 0.0,
        }
    }
}

impl Temporal {
    pub const MAX_RADIUS: u32 = 5;

    pub fn chroma_strength(&self) -> f32 {
        self.chroma.unwrap_or(self.strength)
    }
}

/// Spatial NLM part.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Spatial {
    /// Search radius (window `2r+1` square), 1..=4.
    pub radius: u32,
    /// Luma strength, 8-bit code values; 0 skips the luma plane.
    pub luma: f32,
    /// Chroma strength; 0 skips the chroma planes.
    pub chroma: f32,
    /// Point protection (luma), as in [`Temporal::protect`].
    pub protect: f32,
}

impl Default for Spatial {
    fn default() -> Self {
        Self {
            radius: 2,
            luma: 2.0,
            chroma: 3.0,
            protect: 0.0,
        }
    }
}

impl Spatial {
    pub const MAX_RADIUS: u32 = 4;
}

/// A denoise stage: temporal, then spatial on its result. Either may be off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Denoise {
    pub temporal: Option<Temporal>,
    pub spatial: Option<Spatial>,
}

impl Denoise {
    pub fn is_off(&self) -> bool {
        self.temporal.is_none() && self.spatial.is_none()
    }

    /// Frames of lookahead (and history) the stage needs.
    pub fn radius(&self) -> u32 {
        self.temporal.map_or(0, |t| t.radius)
    }

    /// Clamps radii into range.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        if let Some(t) = &mut self.temporal {
            t.radius = t.radius.clamp(1, Temporal::MAX_RADIUS);
            t.protect = t.protect.max(0.0);
        }
        if let Some(s) = &mut self.spatial {
            s.radius = s.radius.clamp(1, Spatial::MAX_RADIUS);
            s.protect = s.protect.max(0.0);
        }
        self
    }
}

/// Parameters tuned for a kind of footage (`docs/denoise.md#presets`). Individual values override
/// it: [`Overrides::resolve`], and the timeline's `preset` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    /// Flat dark sky: a strong temporal pass with stars and point lights protected.
    Night,
    /// Bright, low noise, fine texture: gentle.
    Snow,
}

impl Preset {
    pub fn denoise(self) -> Denoise {
        match self {
            Self::Night => Denoise {
                temporal: Some(Temporal {
                    radius: 4,
                    motion: Motion::Nvof,
                    strength: 2.0,
                    chroma: Some(3.0),
                    protect: 2.0,
                }),
                spatial: Some(Spatial {
                    radius: 2,
                    luma: 1.0,
                    chroma: 2.5,
                    protect: 2.0,
                }),
            },
            Self::Snow => Denoise {
                temporal: Some(Temporal {
                    radius: 2,
                    motion: Motion::Nvof,
                    strength: 1.5,
                    chroma: None,
                    protect: 0.0,
                }),
                spatial: None,
            },
        }
    }
}

impl std::str::FromStr for Preset {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "night" => Ok(Self::Night),
            "snow" => Ok(Self::Snow),
            _ => Err(format!("unknown denoise preset {s:?} (night, snow)")),
        }
    }
}

/// Values set one by one (`ffcine grade` flags); what isn't set comes from the preset, or without
/// one from the defaults, where both passes are off until asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Overrides {
    /// 0 turns the temporal pass off.
    pub temporal_radius: Option<u32>,
    pub temporal_motion: Option<Motion>,
    pub temporal_strength: Option<f32>,
    pub temporal_chroma: Option<f32>,
    /// Spatial luma strength; 0 turns the spatial pass off unless `chroma` is set.
    pub luma: Option<f32>,
    pub chroma: Option<f32>,
    pub spatial_radius: Option<u32>,
    /// Both passes' [`Temporal::protect`].
    pub protect: Option<f32>,
}

impl Overrides {
    /// The stage for `preset` with these values on top, clamped.
    #[must_use]
    pub fn resolve(&self, preset: Option<Preset>) -> Denoise {
        let base = preset.map_or_else(Denoise::default, Preset::denoise);
        let t0 = base.temporal.unwrap_or_default();
        let radius = self
            .temporal_radius
            .unwrap_or(base.temporal.map_or(0, |t| t.radius));
        let temporal = (radius > 0).then(|| Temporal {
            radius,
            motion: self.temporal_motion.unwrap_or(t0.motion),
            strength: self.temporal_strength.unwrap_or(t0.strength),
            chroma: self.temporal_chroma.or(t0.chroma),
            protect: self.protect.unwrap_or(t0.protect),
        });
        Denoise {
            temporal,
            spatial: spatial_over(
                base.spatial,
                self.luma,
                self.chroma,
                self.spatial_radius,
                self.protect,
            ),
        }
        .clamped()
    }
}

/// `base` with spatial values on top. Without a base the luma strength decides and chroma defaults
/// to 1.5x it; with one, luma 0 alone still means "spatial off".
pub(crate) fn spatial_over(
    base: Option<Spatial>,
    luma: Option<f32>,
    chroma: Option<f32>,
    radius: Option<u32>,
    protect: Option<f32>,
) -> Option<Spatial> {
    let d = Spatial::default();
    let l = luma.or(base.map(|b| b.luma)).unwrap_or(0.0);
    let c = chroma.unwrap_or_else(|| match (base, luma) {
        (Some(_), Some(v)) if v <= 0.0 => 0.0,
        (Some(b), _) => b.chroma,
        (None, _) => 1.5 * l,
    });
    (l > 0.0 || c > 0.0).then(|| Spatial {
        radius: radius.or(base.map(|b| b.radius)).unwrap_or(d.radius),
        luma: l,
        chroma: c,
        protect: protect.or(base.map(|b| b.protect)).unwrap_or(d.protect),
    })
}

/// Workgroup size of every kernel here.
pub const GROUP: u32 = 16;
/// Luma pixels per motion vector, each way.
pub const BLOCK: u32 = 16;
/// Quarter-resolution search range around the predictor, in quarter pixels (+-24 luma pixels).
pub const SEARCH: u32 = 6;

/// A generated compute kernel: `header` (functions, shared memory) and `body` (inside `main`),
/// reading and writing the descriptors named in the doc of the function that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compute {
    pub header: String,
    pub body: String,
    /// Shared memory in bytes.
    pub shmem: usize,
}

fn swizzle(comps: u32) -> (&'static str, &'static str) {
    if comps == 1 {
        ("float", "r")
    } else {
        ("vec2", "rg")
    }
}

fn store(comps: u32, v: &str) -> String {
    if comps == 1 {
        format!("vec4({v}, 0.0, 0.0, 1.0)")
    } else {
        format!("vec4({v}, 0.0, 1.0)")
    }
}

/// Copies `src` (sampled) into `dst` (storage), same size.
pub fn copy_kernel() -> Compute {
    Compute {
        header: String::new(),
        body: r"
    ivec2 p = ivec2(gl_GlobalInvocationID.xy);
    if (all(lessThan(p, imageSize(dst))))
        imageStore(dst, p, texelFetch(src, p, 0));
"
        .into(),
        shmem: 0,
    }
}

/// Box-averages 4x4 luma of `src` into quarter-size `dst` (`r16f`), normalised to 0..1.
pub fn quarter_kernel(norm: f32) -> Compute {
    Compute {
        header: String::new(),
        body: format!(
            r"
    ivec2 q = ivec2(gl_GlobalInvocationID.xy);
    if (all(lessThan(q, imageSize(dst)))) {{
        ivec2 hi = textureSize(src, 0) - 1;
        float s = 0.0;
        for (int j = 0; j < 4; j++)
            for (int i = 0; i < 4; i++)
                s += texelFetch(src, min(q * 4 + ivec2(i, j), hi), 0).r;
        imageStore(dst, q, vec4(s * ({norm:.8} / 16.0), 0.0, 0.0, 1.0));
    }}
"
        ),
        shmem: 0,
    }
}

/// Block matching from the centre frame to one neighbour.
///
/// Descriptors: `qc`, `qn` (quarter luma of centre and neighbour), `lc`, `ln` (full luma),
/// `pred` (motion to the nearer neighbour, only if `pred_scale` is set: its vectors times
/// `pred_scale` seed the search), `dst` (`rgba16f`, one texel per [`BLOCK`]).
pub fn motion_kernel(norm: f32, pred_scale: Option<f32>) -> Compute {
    let pred = pred_scale.map_or_else(
        || "vec2 vp = vec2(0.0);".to_owned(),
        |s| format!("vec2 vp = texelFetch(pred, b, 0).xy * {s:.6};"),
    );
    Compute {
        header: String::new(),
        body: format!(
            r"
    ivec2 b = ivec2(gl_GlobalInvocationID.xy);
    if (all(lessThan(b, imageSize(dst)))) {{
        const float K = {norm:.8} * 255.0;
        ivec2 qhi = textureSize(qn, 0) - 1;
        ivec2 q0 = b * 4;
        float cq[16];
        for (int i = 0; i < 16; i++)
            cq[i] = texelFetch(qc, min(q0 + ivec2(i & 3, i >> 2), qhi), 0).r;
        {pred}
        ivec2 seed = ivec2(round(vp * 0.25));
        ivec2 best = ivec2(0);
        float bc = 1e9;
        for (int pass = 0; pass < 2; pass++) {{
            ivec2 c0 = pass == 0 ? ivec2(0) : seed;
            int r = pass == 0 ? 1 : {SEARCH};
            for (int dy = -r; dy <= r; dy++)
                for (int dx = -r; dx <= r; dx++) {{
                    ivec2 v = c0 + ivec2(dx, dy);
                    float c = 0.0;
                    for (int i = 0; i < 16; i++)
                        c += abs(cq[i] - texelFetch(qn, clamp(q0 + ivec2(i & 3, i >> 2) + v, ivec2(0), qhi), 0).r);
                    // Prefer short vectors on flat or ambiguous blocks.
                    c += 0.002 * float(abs(v.x) + abs(v.y));
                    if (c < bc) {{ bc = c; best = v; }}
                }}
        }}
        // Full-resolution refine around 4x the quarter vector: steps of 2 px, then 1 px, on the
        // 16x16 block subsampled by 2 (centre samples cached).
        ivec2 lhi = textureSize(ln, 0) - 1;
        ivec2 l0 = b * {BLOCK};
        float cl[64];
        for (int i = 0; i < 64; i++)
            cl[i] = texelFetch(lc, min(l0 + 2 * ivec2(i & 7, i >> 3), lhi), 0).r;
        ivec2 fbest = best * 4;
        float fc = 1e9;
        for (int step = 2; step >= 1; step--) {{
            ivec2 c1 = fbest;
            for (int dy = -1; dy <= 1; dy++)
                for (int dx = -1; dx <= 1; dx++) {{
                    if (step == 1 && dx == 0 && dy == 0) continue;
                    ivec2 v = c1 + step * ivec2(dx, dy);
                    float c = 0.0;
                    for (int i = 0; i < 64; i++)
                        c += abs(cl[i] - texelFetch(ln, clamp(l0 + 2 * ivec2(i & 7, i >> 3) + v, ivec2(0), lhi), 0).r);
                    if (c < fc) {{ fc = c; fbest = v; }}
                }}
        }}
        float mad = fc * K / float(({BLOCK} / 2) * ({BLOCK} / 2));
        imageStore(dst, b, vec4(vec2(fbest), mad, 1.0));
    }}
"
        ),
        shmem: 0,
    }
}

/// Where the motion vectors come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Motion {
    /// NVIDIA optical flow (NVOFA) on half-resolution 8-bit luma, 4x4 grid (8x8 luma pixels),
    /// via CUDA, rescored in Vulkan; falls back to `blocks` where NVOF isn't available.
    #[default]
    Nvof,
    /// Vulkan compute block matching, 16x16 blocks.
    Blocks,
}

impl std::str::FromStr for Motion {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "nvof" => Ok(Self::Nvof),
            "blocks" => Ok(Self::Blocks),
            _ => Err(format!("unknown motion source {s:?} (nvof, blocks)")),
        }
    }
}

/// Luma pixels per NVOF vector: half-resolution input, 4x4 output grid.
pub const NVOF_BLOCK: u32 = 8;

/// Half-resolution 8-bit luma for NVOF: 2x2 box average of `src` into `dst` (`r8`).
pub fn luma8_kernel(norm: f32) -> Compute {
    Compute {
        header: String::new(),
        body: format!(
            r"
    ivec2 q = ivec2(gl_GlobalInvocationID.xy);
    if (all(lessThan(q, imageSize(dst)))) {{
        ivec2 hi = textureSize(src, 0) - 1;
        float s = 0.0;
        for (int i = 0; i < 4; i++)
            s += texelFetch(src, min(q * 2 + ivec2(i & 1, i >> 1), hi), 0).r;
        imageStore(dst, q, vec4((clamp(s * ({norm:.8} / 4.0), 0.0, 1.0)), 0.0, 0.0, 1.0));
    }}
"
        ),
        shmem: 0,
    }
}

/// Scores NVOF's vectors like [`motion_kernel`] does its own: `flow` (`rg32f`, luma pixels, one
/// per [`NVOF_BLOCK`]) in, `(dx, dy, mad)` out to `dst` (`rgba16f`), the MAD of the block at its
/// vector between full-resolution `lc` and `ln`.
pub fn confidence_kernel(norm: f32) -> Compute {
    Compute {
        header: String::new(),
        body: format!(
            r"
    ivec2 b = ivec2(gl_GlobalInvocationID.xy);
    if (all(lessThan(b, imageSize(dst)))) {{
        // NVOF's vector, its four neighbours' and zero: keep the best match. NVOF regularises
        // flat dark areas into arbitrary vectors; static ones are usually right there.
        ivec2 fhi = imageSize(dst) - 1;
        vec2 cand[6] = vec2[6](
            texelFetch(flow, b, 0).xy, vec2(0.0),
            texelFetch(flow, clamp(b + ivec2(1, 0), ivec2(0), fhi), 0).xy,
            texelFetch(flow, clamp(b - ivec2(1, 0), ivec2(0), fhi), 0).xy,
            texelFetch(flow, clamp(b + ivec2(0, 1), ivec2(0), fhi), 0).xy,
            texelFetch(flow, clamp(b - ivec2(0, 1), ivec2(0), fhi), 0).xy);
        ivec2 lhi = textureSize(ln, 0) - 1;
        ivec2 l0 = b * {NVOF_BLOCK};
        float cl[{NVOF_BLOCK} * {NVOF_BLOCK}];
        for (int i = 0; i < {NVOF_BLOCK} * {NVOF_BLOCK}; i++)
            cl[i] = texelFetch(lc, min(l0 + ivec2(i % {NVOF_BLOCK}, i / {NVOF_BLOCK}), lhi), 0).r;
        vec2 v = cand[0];
        float c = 1e9;
        for (int k = 0; k < 6; k++) {{
            ivec2 iv = ivec2(round(cand[k]));
            float ck = 0.0;
            for (int i = 0; i < {NVOF_BLOCK} * {NVOF_BLOCK}; i++) {{
                ivec2 p = min(l0 + ivec2(i % {NVOF_BLOCK}, i / {NVOF_BLOCK}), lhi);
                ck += abs(cl[i] - texelFetch(ln, clamp(p + iv, ivec2(0), lhi), 0).r);
            }}
            if (ck < c) {{ c = ck; v = cand[k]; }}
        }}
        float mad = c * {norm:.8} * 255.0 / float({NVOF_BLOCK} * {NVOF_BLOCK});
        imageStore(dst, b, vec4(v, mad, 1.0));
    }}
"
        ),
        shmem: 0,
    }
}

/// Shape of a plane relative to luma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    /// 1 (luma or a single chroma plane) or 2 (interleaved chroma).
    pub comps: u32,
    /// Subsampling relative to luma (1 or 2 each way).
    pub sub: (u32, u32),
}

/// Point protection (`docs/denoise.md#point-protection`), GLSL: `pw`, the weight of a sample from
/// how far it falls below the centre pixel `cen`. The 3x3 patch distance divides one pixel's
/// difference by 9, so a point a few sigmas above a flat sky matches every empty patch; this looks
/// at the pixel alone, and only down: bright points stay, dark outliers still average away.
fn protect_glsl(h: f32, protect: f32, cen: &str, sample: &str) -> String {
    let t = protect * h;
    format!(
        "float pw = exp(-max(pow(max({cen} - {sample}, 0.0), 2.0) - {t2:.6}, 0.0) / {h2:.6});",
        t2 = t * t,
        h2 = h * h,
    )
}

/// Motion-compensated merge of `k` neighbours into the centre.
///
/// Descriptors: `cur` (centre plane, nearest), `n0..n{k-1}` (neighbour planes, linear),
/// `m0..m{k-1}` (their motion, linear), `dst` (storage, plane size).
pub fn merge_kernel(
    k: usize,
    plane: Plane,
    norm: f32,
    strength: f32,
    protect: f32,
    block: u32,
) -> Compute {
    let (ty, sw) = swizzle(plane.comps);
    let (sx, sy) = plane.sub;
    let h = strength.max(0.1);
    let protected = protect > 0.0 && plane.comps == 1;
    // Block MAD between two frames of pure noise sigma h is ~1.13 h (luma, at the vector);
    // beyond that the match itself is doubtful.
    let mth = 1.5 * h + 1.0;
    let header = format!(
        r"
const float FFC_K = {norm:.8} * 255.0;
const vec2 FFC_SUB = vec2({sx}.0, {sy}.0);
const float FFC_H2 = {h2:.6};
const float FFC_MTH = {mth:.6};
const float FFC_MS = {ms:.6};

void ffc_merge(sampler2D n, sampler2D m, ivec2 p, vec2 sz, {ty} cp[9], inout {ty} acc, inout float wsum) {{
    vec2 msz = vec2(textureSize(m, 0)) * {block}.0;
    vec3 mv = textureLod(m, (vec2(p) + 0.5) * FFC_SUB / msz, 0.0).xyz;
    vec2 q = vec2(p) + 0.5 + mv.xy / FFC_SUB;
    if (any(lessThan(q, vec2(0.5))) || any(greaterThan(q, sz - 0.5)))
        return;
    float d2 = 0.0;
    float vmax = 0.0;
    {ty} c = {ty}(0.0);
    for (int i = 0; i < 9; i++) {{
        {ty} v = textureLod(n, (q + vec2(i % 3 - 1, i / 3 - 1)) / sz, 0.0).{sw};
        if (i == 4) c = v * FFC_K;
        {ty} d = v * FFC_K - cp[i];
        d2 += dot(d, d);
{track}
    }}
    d2 /= {n9}.0;
    float w = exp(-max(d2 - 2.0 * FFC_H2, 0.0) / FFC_H2)
            * exp(-max(mv.z - FFC_MTH, 0.0) / FFC_MS);
{prot}
    acc += w * c;
    wsum += w;
}}
",
        h2 = h * h,
        ms = 0.5 * h + 0.5,
        n9 = 9 * plane.comps,
        track = if protected {
            "        vmax = max(vmax, v * FFC_K);"
        } else {
            ""
        },
        prot = if protected {
            // A point in this frame that the neighbour also has, within a pixel, and that the
            // vector just misses: keep it out. A lone bright pixel in the centre (noise) has no
            // such evidence in the neighbour and averages away as usual.
            format!(
                "    {}\n    w *= mix(1.0, pw, clamp((vmax - cp[4] + {t:.6}) / {h:.6} + 1.0, 0.0, 1.0));",
                protect_glsl(h, protect, "cp[4]", "c"),
                t = protect * h,
            )
        } else {
            String::new()
        },
    );
    let mut body = format!(
        r"
    ivec2 p = ivec2(gl_GlobalInvocationID.xy);
    ivec2 isz = textureSize(cur, 0);
    if (all(lessThan(p, isz))) {{
        vec2 sz = vec2(isz);
        {ty} cp[9];
        for (int i = 0; i < 9; i++)
            cp[i] = texelFetch(cur, clamp(p + ivec2(i % 3 - 1, i / 3 - 1), ivec2(0), isz - 1), 0).{sw} * FFC_K;
        {ty} acc = cp[4];
        float wsum = 1.0;
"
    );
    for i in 0..k {
        let _ = writeln!(body, "        ffc_merge(n{i}, m{i}, p, sz, cp, acc, wsum);");
    }
    let _ = writeln!(
        body,
        "        imageStore(dst, p, {});\n    }}",
        store(plane.comps, "acc / (wsum * FFC_K)")
    );
    Compute {
        header,
        body,
        shmem: 0,
    }
}

/// NLM (3x3 patch, `(2 radius + 1)^2` search) from `src` (sampled) to `dst` (storage).
pub fn spatial_kernel(radius: u32, comps: u32, norm: f32, strength: f32, protect: f32) -> Compute {
    let (ty, sw) = swizzle(comps);
    let halo = radius + 1;
    let t = GROUP + 2 * halo;
    let h = strength.max(0.1);
    let header = format!(
        r"
const int FFC_T = {t};
const int FFC_R = {radius};
const float FFC_K = {norm:.8} * 255.0;
const float FFC_H2 = {h2:.6};
shared {ty} ffc_tile[FFC_T * FFC_T];
{ty} ffc_at(ivec2 l) {{ return ffc_tile[l.y * FFC_T + l.x]; }}
",
        h2 = h * h,
    );
    let body = format!(
        r"
    ivec2 isz = textureSize(src, 0);
    ivec2 g0 = ivec2(gl_WorkGroupID.xy) * {GROUP} - {halo};
    for (uint i = gl_LocalInvocationIndex; i < uint(FFC_T * FFC_T); i += {GROUP}u * {GROUP}u) {{
        ivec2 o = ivec2(int(i) % FFC_T, int(i) / FFC_T);
        ffc_tile[i] = texelFetch(src, clamp(g0 + o, ivec2(0), isz - 1), 0).{sw} * FFC_K;
    }}
    barrier();
    ivec2 p = ivec2(gl_GlobalInvocationID.xy);
    if (all(lessThan(p, isz))) {{
        ivec2 l = ivec2(gl_LocalInvocationID.xy) + {halo};
        {ty} acc = {ty}(0.0);
        float wsum = 0.0;
        for (int sy = -FFC_R; sy <= FFC_R; sy++)
            for (int sx = -FFC_R; sx <= FFC_R; sx++) {{
                ivec2 s = l + ivec2(sx, sy);
                float d2 = 0.0;
                for (int j = -1; j <= 1; j++)
                    for (int i = -1; i <= 1; i++) {{
                        {ty} d = ffc_at(s + ivec2(i, j)) - ffc_at(l + ivec2(i, j));
                        d2 += dot(d, d);
                    }}
                d2 /= {n9}.0;
                float w = exp(-max(d2 - 2.0 * FFC_H2, 0.0) / FFC_H2);
                {prot}
                acc += w * ffc_at(s);
                wsum += w;
            }}
        imageStore(dst, p, {st});
    }}
",
        n9 = 9 * comps,
        prot = if protect > 0.0 && comps == 1 {
            format!(
                "{} w *= pw;",
                protect_glsl(h, protect, "ffc_at(l)", "ffc_at(s)")
            )
        } else {
            String::new()
        },
        st = store(comps, "acc / (wsum * FFC_K)"),
    );
    let shmem = (t * t) as usize * 4 * comps as usize;
    Compute {
        header,
        body,
        shmem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denoise_params_parse_and_clamp() {
        let d: Denoise =
            serde_json::from_str(r#"{"temporal": {"radius": 9, "strength": 6}}"#).unwrap();
        let d = d.clamped();
        assert_eq!(d.radius(), 5);
        assert!(d.spatial.is_none());
        assert!((d.temporal.unwrap().chroma_strength() - 6.0).abs() < f32::EPSILON);
        assert!(Denoise::default().is_off());
    }

    #[test]
    fn merge_calls_every_neighbour() {
        let c = merge_kernel(
            4,
            Plane {
                comps: 2,
                sub: (2, 2),
            },
            1.0,
            4.0,
            0.0,
            BLOCK,
        );
        assert_eq!(c.body.matches("ffc_merge(").count(), 4);
        assert!(c.body.contains("n3, m3"));
        assert!(c.header.contains("vec2 cp[9]"));
    }

    #[test]
    fn protect_only_when_asked() {
        let plane = Plane {
            comps: 1,
            sub: (1, 1),
        };
        let off = merge_kernel(2, plane, 1.0, 3.0, 0.0, BLOCK);
        assert!(!off.header.contains("pw"), "{}", off.header);
        assert!(!spatial_kernel(2, 1, 1.0, 2.0, 0.0).body.contains("pw"));
        // 3 strengths of 2: the threshold is (3 * 2)^2 = 36.
        let want = "- 36.000000, 0.0) / 4.000000";
        let c = merge_kernel(2, plane, 1.0, 2.0, 3.0, BLOCK);
        assert!(c.header.contains(want), "{}", c.header);
        let c = spatial_kernel(2, 1, 1.0, 2.0, 3.0);
        assert!(c.body.contains(want), "{}", c.body);
        // Chroma never: a point is a luma thing.
        let chroma = Plane {
            comps: 2,
            sub: (2, 2),
        };
        assert!(
            !merge_kernel(2, chroma, 1.0, 2.0, 3.0, BLOCK)
                .header
                .contains("pw")
        );
        assert!(!spatial_kernel(2, 2, 1.0, 2.0, 3.0).body.contains("pw"));
    }

    #[test]
    fn presets_parse_and_stay_in_range() {
        for (name, p) in [("night", Preset::Night), ("snow", Preset::Snow)] {
            assert_eq!(name.parse::<Preset>().unwrap(), p);
            let d = p.denoise();
            assert_eq!(d.clamped(), d);
            assert_eq!(Overrides::default().resolve(Some(p)), d);
        }
        assert!("aurora".parse::<Preset>().is_err());
    }

    #[test]
    fn overrides_without_a_preset_keep_the_old_flags() {
        assert!(Overrides::default().resolve(None).is_off());
        let d = Overrides {
            temporal_radius: Some(2),
            luma: Some(2.0),
            ..Overrides::default()
        }
        .resolve(None);
        assert_eq!(d.temporal, Some(Temporal::default()));
        let s = d.spatial.unwrap();
        assert_eq!((s.radius, s.luma, s.chroma), (2, 2.0, 3.0));
        // Only strengths given, no radius: still off, as before.
        let d = Overrides {
            temporal_strength: Some(9.0),
            ..Overrides::default()
        }
        .resolve(None);
        assert!(d.is_off());
    }

    #[test]
    fn overrides_replace_preset_values_one_by_one() {
        let night = Preset::Night.denoise();
        let d = Overrides {
            temporal_strength: Some(9.0),
            protect: Some(-1.0),
            ..Overrides::default()
        }
        .resolve(Some(Preset::Night));
        let t = d.temporal.unwrap();
        assert_eq!(t.radius, night.temporal.unwrap().radius);
        assert_eq!((t.strength, t.protect), (9.0, 0.0));
        assert_eq!(d.spatial.unwrap().luma, night.spatial.unwrap().luma);
        // Luma 0 alone turns the spatial pass off; with a chroma strength it stays for chroma.
        let off = Overrides {
            luma: Some(0.0),
            ..Overrides::default()
        };
        assert!(off.resolve(Some(Preset::Night)).spatial.is_none());
        let chroma = Overrides {
            chroma: Some(2.0),
            ..off
        };
        let s = chroma.resolve(Some(Preset::Night)).spatial.unwrap();
        assert_eq!((s.luma, s.chroma), (0.0, 2.0));
    }

    /// CPU references of the merge and the NLM, the weights of the generated GLSL: known zero
    /// motion, no block confidence, one plane in 8-bit code values. Enough to see what the weights
    /// do to a point.
    mod reference {
        use super::*;

        pub const N: usize = 96;
        pub type Frame = Vec<f32>;

        /// xorshift + Box-Muller, so the tests need no dependency and never change.
        pub struct Noise(pub u64);

        impl Noise {
            fn uniform(&mut self) -> f32 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                ((self.0 >> 40) as f32 + 1.0) / ((1u64 << 24) as f32 + 2.0)
            }

            pub fn gauss(&mut self) -> f32 {
                let (a, b) = (self.uniform(), self.uniform());
                (-2.0 * a.ln()).sqrt() * (std::f32::consts::TAU * b).cos()
            }
        }

        /// `exp(-max(d2 - 2h^2, 0) / h^2)`.
        fn patch_weight(d2: f32, h: f32) -> f32 {
            (-(d2 - 2.0 * h * h).max(0.0) / (h * h)).exp()
        }

        /// `pw` of [`protect_glsl`].
        fn protect_weight(centre: f32, sample: f32, h: f32, protect: f32) -> f32 {
            let (t, d) = (protect * h, (centre - sample).max(0.0));
            (-(d * d - t * t).max(0.0) / (h * h)).exp()
        }

        fn at(f: &[f32], x: i32, y: i32) -> f32 {
            let c = |v: i32| v.clamp(0, N as i32 - 1) as usize;
            f[c(y) * N + c(x)]
        }

        fn d2(a: &[f32], ax: i32, ay: i32, b: &[f32], bx: i32, by: i32) -> f32 {
            let mut s = 0.0;
            for j in -1..=1 {
                for i in -1..=1 {
                    let d = at(a, ax + i, ay + j) - at(b, bx + i, by + j);
                    s += d * d;
                }
            }
            s / 9.0
        }

        pub fn merge(frames: &[Frame], centre: usize, h: f32, protect: f32) -> Frame {
            let cur = &frames[centre];
            let mut out = vec![0.0; N * N];
            for y in 0..N as i32 {
                for x in 0..N as i32 {
                    let c = at(cur, x, y);
                    let (mut acc, mut wsum) = (c, 1.0);
                    for (k, f) in frames.iter().enumerate() {
                        if k != centre {
                            let v = at(f, x, y);
                            let mut w = patch_weight(d2(f, x, y, cur, x, y), h);
                            if protect > 0.0 {
                                let mut vmax = 0.0_f32;
                                for j in -1..=1 {
                                    for i in -1..=1 {
                                        vmax = vmax.max(at(f, x + i, y + j));
                                    }
                                }
                                let seen = ((vmax - c + protect * h) / h + 1.0).clamp(0.0, 1.0);
                                let pw = protect_weight(c, v, h, protect);
                                w *= 1.0 + (pw - 1.0) * seen;
                            }
                            acc += w * v;
                            wsum += w;
                        }
                    }
                    out[y as usize * N + x as usize] = acc / wsum;
                }
            }
            out
        }

        pub fn nlm(f: &[f32], radius: i32, h: f32, protect: f32) -> Frame {
            let mut out = vec![0.0; N * N];
            for y in 0..N as i32 {
                for x in 0..N as i32 {
                    let (mut acc, mut wsum) = (0.0, 0.0);
                    for sy in -radius..=radius {
                        for sx in -radius..=radius {
                            let v = at(f, x + sx, y + sy);
                            let mut w = patch_weight(d2(f, x + sx, y + sy, f, x, y), h);
                            if protect > 0.0 {
                                w *= protect_weight(at(f, x, y), v, h, protect);
                            }
                            acc += w * v;
                            wsum += w;
                        }
                    }
                    out[y as usize * N + x as usize] = acc / wsum;
                }
            }
            out
        }

        pub fn run(d: Denoise, frames: &[Frame]) -> Frame {
            let centre = frames.len() / 2;
            let (t, s) = (d.temporal.unwrap(), d.spatial);
            let r = t.radius as usize;
            let m = merge(&frames[centre - r..=centre + r], r, t.strength, t.protect);
            match s {
                Some(s) => nlm(&m, s.radius as i32, s.luma, s.protect),
                None => m,
            }
        }

        pub fn stars(amp: f32, sigma: f32) -> (Vec<Frame>, Vec<(i32, i32)>) {
            let mut n = Noise(0x9E37_79B9_7F4A_7C15);
            let points: Vec<_> = (0..25)
                .map(|i| (9 + (i % 5) * 18, 9 + (i / 5) * 18))
                .collect();
            // The centre frame's points sit on `points`, other frames' a pixel off: the vectors
            // of a flat sky never quite line them up.
            let frames = (0..11)
                .map(|k: i32| {
                    let mut f: Frame = (0..N * N).map(|_| 42.0 + sigma * n.gauss()).collect();
                    for &(x, y) in &points {
                        f[y as usize * N + (x + (k + 1) % 2) as usize] += amp;
                    }
                    f
                })
                .collect();
            (frames, points)
        }

        /// Mean height of the points above the sky around them.
        pub fn peak(f: &[f32], points: &[(i32, i32)]) -> f32 {
            let sum: f32 = points
                .iter()
                .map(|&(x, y)| at(f, x, y) - at(f, x + 7, y + 7))
                .sum();
            sum / points.len() as f32
        }

        /// Temporal-free sigma of the sky between the points.
        pub fn sky_sigma(f: &[f32]) -> f32 {
            let v: Vec<f32> = (0..N * N)
                .filter(|i| !(6..13).contains(&(i % N % 18)) || !(6..13).contains(&(i / N % 18)))
                .map(|i| f[i])
                .collect();
            let m = v.iter().sum::<f32>() / v.len() as f32;
            (v.iter().map(|a| (a - m) * (a - m)).sum::<f32>() / v.len() as f32).sqrt()
        }
    }

    #[test]
    fn night_keeps_faint_stars_and_flattens_the_sky() {
        use reference::*;
        let night = Preset::Night.denoise();
        let unprotected = Denoise {
            temporal: night.temporal.map(|t| Temporal { protect: 0.0, ..t }),
            spatial: night.spatial.map(|s| Spatial { protect: 0.0, ..s }),
        };
        let (amp, sigma) = (6.0, 0.7);
        let (frames, points) = stars(amp, sigma);
        let source = &frames[frames.len() / 2];
        let (a, b) = (run(night, &frames), run(unprotected, &frames));
        let (kept, lost) = (peak(&a, &points) / amp, peak(&b, &points) / amp);
        assert!(kept > 0.9, "protected peak {kept}");
        assert!(lost < 0.2, "unprotected peak {lost}");
        // The sky still flattens, at least 3x.
        let (before, after) = (sky_sigma(source), sky_sigma(&a));
        assert!(after < before / 3.0, "sky {before} -> {after}");
    }

    #[test]
    fn snow_keeps_fine_texture() {
        use reference::*;
        // Fine low-contrast texture, the same in every frame, under little noise.
        let mut n = Noise(0x1234_5678_9ABC_DEF1);
        let tex: Frame = (0..N * N).map(|_| 215.0 + 2.5 * n.gauss()).collect();
        let frames: Vec<Frame> = (0..11)
            .map(|_| tex.iter().map(|v| v + 0.8 * n.gauss()).collect())
            .collect();
        let hp = |f: &[f32]| {
            let m = f.iter().sum::<f32>() / f.len() as f32;
            (f.iter().map(|a| (a - m) * (a - m)).sum::<f32>() / f.len() as f32).sqrt()
        };
        let out = run(Preset::Snow.denoise(), &frames);
        let kept = hp(&out) / hp(&tex);
        assert!((0.9..1.1).contains(&kept), "texture energy {kept}");
        let err = |f: &[f32]| {
            let s: f32 = f.iter().zip(&tex).map(|(a, b)| (a - b) * (a - b)).sum();
            (s / f.len() as f32).sqrt()
        };
        assert!(
            err(&out) < 0.7 * err(&frames[5]),
            "{} vs {}",
            err(&out),
            err(&frames[5])
        );
    }

    #[test]
    fn spatial_shmem_fits() {
        let c = spatial_kernel(Spatial::MAX_RADIUS, 2, 1.0, 2.0, 0.0);
        assert!(c.shmem <= 16 * 1024, "{}", c.shmem);
    }
}
