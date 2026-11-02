//! Blend modes for [`crate::Layer`]s (`ffcine.comp.blend/1` `mode`, Resolve's composite modes):
//! the W3C compositing formulas (Photoshop's where W3C has none), as a CPU reference and as the
//! GLSL the backend's composite pass runs. Both work on encoded RGB in 0..1 (the output's
//! transfer), the backdrop opaque: `out = (1 − a) · b + a · B(b, s)` with `a` the layer's alpha
//! times its opacity.

use std::fmt;
use std::str::FromStr;

use crate::Error;

macro_rules! modes {
    ($($(#[$m:meta])* $v:ident = $s:literal),+ $(,)?) => {
        /// How a layer combines with the picture under it.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub enum BlendMode {
            #[default]
            $($(#[$m])* $v),+
        }

        impl BlendMode {
            /// Every mode, in shader index order.
            pub const ALL: &[Self] = &[$(Self::$v),+];

            /// The `ffcine.comp.blend/1` name.
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$v => $s),+ }
            }
        }

        impl FromStr for BlendMode {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                match s {
                    $($s => Ok(Self::$v),)+
                    _ => Err(Error::Invalid { what: "blend mode", value: s.to_owned() }),
                }
            }
        }
    };
}

modes! {
    Normal = "normal",
    Add = "add",
    Subtract = "subtract",
    Difference = "difference",
    Multiply = "multiply",
    Screen = "screen",
    Overlay = "overlay",
    HardLight = "hardlight",
    SoftLight = "softlight",
    Darken = "darken",
    Lighten = "lighten",
    ColorDodge = "color_dodge",
    ColorBurn = "color_burn",
    Exclusion = "exclusion",
    Hue = "hue",
    Saturation = "saturation",
    Color = "color",
    Luminosity = "luminosity",
    Divide = "divide",
    /// Same as `add` (Resolve lists both).
    LinearDodge = "linear_dodge",
    LinearBurn = "linear_burn",
    LinearLight = "linear_light",
    VividLight = "vivid_light",
    PinLight = "pin_light",
    HardMix = "hard_mix",
    LighterColor = "lighter_color",
    DarkerColor = "darker_color",
}

impl fmt::Display for BlendMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BlendMode {
    /// The `mode` uniform of [`glsl`].
    pub fn index(self) -> i32 {
        Self::ALL
            .iter()
            .position(|m| *m == self)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(0)
    }

    /// `B(b, s)`: backdrop `b`, layer `s`, both clamped to 0..1.
    pub fn apply(self, b: [f32; 3], s: [f32; 3]) -> [f32; 3] {
        let (b, s) = (b.map(|x| x.clamp(0.0, 1.0)), s.map(|x| x.clamp(0.0, 1.0)));
        let sep = |f: fn(f32, f32) -> f32| [f(b[0], s[0]), f(b[1], s[1]), f(b[2], s[2])];
        match self {
            Self::Normal => s,
            Self::Add | Self::LinearDodge => sep(|b, s| (b + s).min(1.0)),
            Self::Subtract => sep(|b, s| (b - s).max(0.0)),
            Self::Difference => sep(|b, s| (b - s).abs()),
            Self::Multiply => sep(|b, s| b * s),
            Self::Screen => sep(screen),
            Self::Overlay => sep(|b, s| hard_light(s, b)),
            Self::HardLight => sep(hard_light),
            Self::SoftLight => sep(soft_light),
            Self::Darken => sep(f32::min),
            Self::Lighten => sep(f32::max),
            Self::ColorDodge => sep(dodge),
            Self::ColorBurn => sep(burn),
            Self::Exclusion => sep(|b, s| b + s - 2.0 * b * s),
            Self::Hue => set_lum(set_sat(s, sat(b)), lum(b)),
            Self::Saturation => set_lum(set_sat(b, sat(s)), lum(b)),
            Self::Color => set_lum(s, lum(b)),
            Self::Luminosity => set_lum(b, lum(s)),
            Self::Divide => sep(|b, s| {
                if s <= 0.0 {
                    if b > 0.0 { 1.0 } else { 0.0 }
                } else {
                    (b / s).min(1.0)
                }
            }),
            Self::LinearBurn => sep(|b, s| (b + s - 1.0).max(0.0)),
            Self::LinearLight => sep(|b, s| (b + 2.0 * s - 1.0).clamp(0.0, 1.0)),
            Self::VividLight => sep(|b, s| {
                if s <= 0.5 {
                    burn(b, 2.0 * s)
                } else {
                    dodge(b, 2.0 * s - 1.0)
                }
            }),
            Self::PinLight => sep(|b, s| {
                if s <= 0.5 {
                    b.min(2.0 * s)
                } else {
                    b.max(2.0 * s - 1.0)
                }
            }),
            Self::HardMix => sep(|b, s| if b + s >= 1.0 { 1.0 } else { 0.0 }),
            Self::LighterColor => {
                if lum(s) > lum(b) {
                    s
                } else {
                    b
                }
            }
            Self::DarkerColor => {
                if lum(s) < lum(b) {
                    s
                } else {
                    b
                }
            }
        }
    }

    /// The composite of layer `s` with alpha `a` (times opacity) over opaque backdrop `b`.
    pub fn composite(self, b: [f32; 3], s: [f32; 3], a: f32) -> [f32; 3] {
        let a = a.clamp(0.0, 1.0);
        let m = self.apply(b, s);
        [0, 1, 2].map(|i| b[i] + a * (m[i] - b[i]))
    }
}

fn screen(b: f32, s: f32) -> f32 {
    b + s - b * s
}

fn hard_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b * 2.0 * s
    } else {
        screen(b, 2.0 * s - 1.0)
    }
}

fn soft_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b - (1.0 - 2.0 * s) * b * (1.0 - b)
    } else {
        let d = if b <= 0.25 {
            ((16.0 * b - 12.0) * b + 4.0) * b
        } else {
            b.sqrt()
        };
        b + (2.0 * s - 1.0) * (d - b)
    }
}

fn dodge(b: f32, s: f32) -> f32 {
    if b <= 0.0 {
        0.0
    } else if s >= 1.0 {
        1.0
    } else {
        (b / (1.0 - s)).min(1.0)
    }
}

fn burn(b: f32, s: f32) -> f32 {
    if b >= 1.0 {
        1.0
    } else if s <= 0.0 {
        0.0
    } else {
        1.0 - ((1.0 - b) / s).min(1.0)
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip_color(c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    let mut c = c;
    if n < 0.0 {
        c = c.map(|v| l + (v - l) * l / (l - n));
    }
    if x > 1.0 {
        c = c.map(|v| l + (v - l) * (1.0 - l) / (x - l));
    }
    c
}

fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color(c.map(|v| v + d))
}

fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    if x > n {
        c.map(|v| (v - n) * s / (x - n))
    } else {
        [0.0; 3]
    }
}

/// GLSL for the composite pass: `vec3 blend_mode(int mode, vec3 b, vec3 s)`, the same formulas
/// as [`BlendMode::apply`], `mode` as [`BlendMode::index`].
pub fn glsl() -> &'static str {
    r"
float bl_screen(float b, float s) { return b + s - b * s; }
float bl_hard(float b, float s) { return s <= 0.5 ? b * 2.0 * s : bl_screen(b, 2.0 * s - 1.0); }
float bl_soft(float b, float s) {
    if (s <= 0.5)
        return b - (1.0 - 2.0 * s) * b * (1.0 - b);
    float d = b <= 0.25 ? ((16.0 * b - 12.0) * b + 4.0) * b : sqrt(b);
    return b + (2.0 * s - 1.0) * (d - b);
}
float bl_dodge(float b, float s) {
    return b <= 0.0 ? 0.0 : (s >= 1.0 ? 1.0 : min(b / (1.0 - s), 1.0));
}
float bl_burn(float b, float s) {
    return b >= 1.0 ? 1.0 : (s <= 0.0 ? 0.0 : 1.0 - min((1.0 - b) / s, 1.0));
}
float bl_divide(float b, float s) {
    return s <= 0.0 ? (b > 0.0 ? 1.0 : 0.0) : min(b / s, 1.0);
}
float bl_vivid(float b, float s) { return s <= 0.5 ? bl_burn(b, 2.0 * s) : bl_dodge(b, 2.0 * s - 1.0); }
float bl_pin(float b, float s) { return s <= 0.5 ? min(b, 2.0 * s) : max(b, 2.0 * s - 1.0); }
float bl_lum(vec3 c) { return dot(c, vec3(0.3, 0.59, 0.11)); }
vec3 bl_clip(vec3 c) {
    float l = bl_lum(c);
    float n = min(c.r, min(c.g, c.b));
    float x = max(c.r, max(c.g, c.b));
    if (n < 0.0)
        c = l + (c - l) * l / (l - n);
    if (x > 1.0)
        c = l + (c - l) * (1.0 - l) / (x - l);
    return c;
}
vec3 bl_set_lum(vec3 c, float l) { return bl_clip(c + (l - bl_lum(c))); }
float bl_sat(vec3 c) { return max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b)); }
vec3 bl_set_sat(vec3 c, float s) {
    float n = min(c.r, min(c.g, c.b));
    float x = max(c.r, max(c.g, c.b));
    return x > n ? (c - n) * s / (x - n) : vec3(0.0);
}
#define BL_SEP(f) vec3(f(b.r, s.r), f(b.g, s.g), f(b.b, s.b))
vec3 blend_mode(int mode, vec3 b, vec3 s) {
    b = clamp(b, 0.0, 1.0);
    s = clamp(s, 0.0, 1.0);
    switch (mode) {
    case 1: case 19: return min(b + s, 1.0);
    case 2: return max(b - s, 0.0);
    case 3: return abs(b - s);
    case 4: return b * s;
    case 5: return b + s - b * s;
    case 6: return vec3(bl_hard(s.r, b.r), bl_hard(s.g, b.g), bl_hard(s.b, b.b));
    case 7: return BL_SEP(bl_hard);
    case 8: return BL_SEP(bl_soft);
    case 9: return min(b, s);
    case 10: return max(b, s);
    case 11: return BL_SEP(bl_dodge);
    case 12: return BL_SEP(bl_burn);
    case 13: return b + s - 2.0 * b * s;
    case 14: return bl_set_lum(bl_set_sat(s, bl_sat(b)), bl_lum(b));
    case 15: return bl_set_lum(bl_set_sat(b, bl_sat(s)), bl_lum(b));
    case 16: return bl_set_lum(s, bl_lum(b));
    case 17: return bl_set_lum(b, bl_lum(s));
    case 18: return BL_SEP(bl_divide);
    case 20: return max(b + s - 1.0, 0.0);
    case 21: return clamp(b + 2.0 * s - 1.0, 0.0, 1.0);
    case 22: return BL_SEP(bl_vivid);
    case 23: return BL_SEP(bl_pin);
    case 24: return step(1.0, b + s);
    case 25: return bl_lum(s) > bl_lum(b) ? s : b;
    case 26: return bl_lum(s) < bl_lum(b) ? s : b;
    default: return s;
    }
}
"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn names_round_trip_and_index_matches_glsl() {
        assert_eq!(BlendMode::ALL.len(), 27);
        for (i, m) in BlendMode::ALL.iter().enumerate() {
            assert_eq!(m.as_str().parse::<BlendMode>().unwrap(), *m);
            assert_eq!(m.index(), i32::try_from(i).unwrap());
            if !matches!(
                m,
                BlendMode::Normal | BlendMode::Add | BlendMode::LinearDodge
            ) {
                assert!(glsl().contains(&format!("case {i}:")), "{m}");
            }
        }
        assert!(glsl().contains("case 1: case 19:"));
        assert!("bogus".parse::<BlendMode>().is_err());
        assert_eq!(BlendMode::default(), BlendMode::Normal);
    }

    #[test]
    fn separable_modes() {
        let (b, s) = ([0.2, 0.5, 0.8], [0.6, 0.5, 0.1]);
        let t = |m: BlendMode| m.apply(b, s);
        assert!(near(t(BlendMode::Normal), s));
        assert!(near(t(BlendMode::Add), [0.8, 1.0, 0.9]));
        assert!(near(t(BlendMode::LinearDodge), t(BlendMode::Add)));
        assert!(near(t(BlendMode::Subtract), [0.0, 0.0, 0.7]));
        assert!(near(t(BlendMode::Difference), [0.4, 0.0, 0.7]));
        assert!(near(t(BlendMode::Multiply), [0.12, 0.25, 0.08]));
        assert!(near(t(BlendMode::Screen), [0.68, 0.75, 0.82]));
        assert!(near(t(BlendMode::Darken), [0.2, 0.5, 0.1]));
        assert!(near(t(BlendMode::Lighten), [0.6, 0.5, 0.8]));
        assert!(near(t(BlendMode::Exclusion), [0.56, 0.5, 0.74]));
        assert!(near(t(BlendMode::LinearBurn), [0.0, 0.0, 0.0]));
        assert!(near(t(BlendMode::LinearLight), [0.4, 0.5, 0.0]));
        // overlay(b, s) = hardlight(s, b): b 0.2 -> 2·0.2·0.6, b 0.8 -> screen(0.1, 0.6).
        assert!(near(t(BlendMode::Overlay), [0.24, 0.5, 0.64]));
        assert!(near(t(BlendMode::HardLight), [0.36, 0.5, 0.16]));
        assert!(near(t(BlendMode::ColorDodge), [0.5, 1.0, 0.888_889]));
        assert!(near(t(BlendMode::ColorBurn), [0.0, 0.0, 0.0]));
        assert!(near(t(BlendMode::Divide), [1.0 / 3.0, 1.0, 1.0]));
        assert!(near(t(BlendMode::PinLight), [0.2, 0.5, 0.2]));
        assert!(near(t(BlendMode::HardMix), [0.0, 1.0, 0.0]));
        // Soft light: s 0.5 leaves b; s 1 over b 0.81 is sqrt(b).
        assert!(near(
            BlendMode::SoftLight.apply([0.3; 3], [0.5; 3]),
            [0.3; 3]
        ));
        assert!(near(
            BlendMode::SoftLight.apply([0.81; 3], [1.0; 3]),
            [0.9; 3]
        ));
        // Vivid light: burn below a half, dodge above.
        assert!(near(
            BlendMode::VividLight.apply([0.6; 3], [0.25; 3]),
            [0.2; 3]
        ));
        assert!(near(
            BlendMode::VividLight.apply([0.3; 3], [0.75; 3]),
            [0.6; 3]
        ));
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn non_separable_modes() {
        let (b, s) = ([0.2, 0.4, 0.6], [0.9, 0.1, 0.1]);
        // Luminosity keeps the backdrop's hue and takes the layer's luma, and colour the reverse.
        let l = BlendMode::Luminosity.apply(b, s);
        assert!((lum(l) - lum(s)).abs() < 1e-5);
        let c = BlendMode::Color.apply(b, s);
        assert!((lum(c) - lum(b)).abs() < 1e-5);
        assert!(c[0] > c[1] && (c[1] - c[2]).abs() < 1e-6);
        // Saturation of a grey layer leaves grey at the backdrop's luma.
        let g = BlendMode::Saturation.apply(b, [0.5; 3]);
        assert!(near(g, [lum(b); 3]));
        // Hue takes the layer's hue at the backdrop's saturation and luma.
        let h = BlendMode::Hue.apply(b, s);
        assert!((lum(h) - lum(b)).abs() < 1e-5);
        assert!((sat(h) - sat(b)).abs() < 1e-5);
        assert!(h[0] > h[1]);
        assert!(near(BlendMode::LighterColor.apply(b, s), b));
        assert!(near(BlendMode::DarkerColor.apply(b, s), s));
    }

    #[test]
    fn composite_mixes_by_alpha() {
        let b = [0.2, 0.4, 0.6];
        let s = [1.0, 0.0, 0.5];
        assert!(near(BlendMode::Multiply.composite(b, s, 0.0), b));
        assert!(near(
            BlendMode::Multiply.composite(b, s, 1.0),
            [0.2, 0.0, 0.3]
        ));
        assert!(near(
            BlendMode::Multiply.composite(b, s, 0.5),
            [0.2, 0.2, 0.45]
        ));
    }
}
