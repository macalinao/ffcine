//! `.hook` shaders (roadmap step 9, `docs/design.md#roadmap`, `docs/hooks.md`): mpv user shaders
//! run in the libplacebo grade pass through `pl_mpv_user_shader_parse`, as
//! [`ffcine_render::Hook`]s with their `//!PARAM`s set from the timeline.
//!
//! - Vendored: [`ADAPTIVE_SHARPEN`] (BSD-2-Clause, header kept), the default sharpen, replacing
//!   the generated unsharp hook (`ffcine_render::shaders::unsharp`), which stays for prototype
//!   diffs ([`Sharpen::Unsharp`]).
//! - Timeline: `ffcine.fx.shader/1 {shader, params}` with `shader` an alias in
//!   `metadata.ffcine.project.shaders`, a vendored name or a path; `params` constants or
//!   keyframes ([`shader`]).
//! - Order: [`order`] sorts a grade's hooks by stage, then geometry, sharpen, user shaders and the
//!   review overlay within a stage.
//!
//! Our own filters (denoise, grain) are compute passes in `ffcine-filters` instead.

pub mod parse;
pub mod shader;

use ffcine_render::{Grade, Hook};

pub use parse::{HookInfo, ParamInfo, ParamKind, parse};
pub use shader::ShaderEffect;

/// bacondither's Adaptive-sharpen (mpv port by igv) at `PREOUTPUT`, with `curve_height` and
/// `overshoot_ctrl` as `//!PARAM`s. Licence: BSD-2-Clause (header in the file).
pub const ADAPTIVE_SHARPEN: &str = include_str!("../shaders/adaptive-sharpen.glsl");

/// Vendored shaders by name.
pub fn vendored(name: &str) -> Option<&'static str> {
    match name {
        "adaptive-sharpen" => Some(ADAPTIVE_SHARPEN),
        _ => None,
    }
}

pub const SHARPEN_HOOK: &str = "ffcine:sharpen";

/// Moves `//!HOOK OUTPUT` passes to `PREOUTPUT`. mpv runs `OUTPUT` on RGB; libplacebo runs it
/// after encoding to the target's colour system, which for ffcine's YUV outputs is YCbCr, so an
/// RGB shader there would sharpen chroma and tint the picture. `PREOUTPUT` is the RGB stage mpv
/// shaders expect.
pub fn retarget_output(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim_end() == "//!HOOK OUTPUT" {
                "//!HOOK PREOUTPUT"
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Which sharpener a `sharpen` amount drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sharpen {
    /// [`ADAPTIVE_SHARPEN`] at `PREOUTPUT` with `curve_height = amount`.
    #[default]
    Adaptive,
    /// The prototype's `unsharp=5:5:amount` at `PREOUTPUT` (`Grade::sharpen`), for diffs.
    Unsharp,
}

impl Sharpen {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "adaptive" | "adaptive-sharpen" => Some(Self::Adaptive),
            "unsharp" => Some(Self::Unsharp),
            _ => None,
        }
    }
}

/// Sets `grade`'s sharpening to `amount` (0 = off) with `kind`, replacing any earlier one.
pub fn sharpen(grade: &mut Grade, amount: f32, kind: Sharpen) {
    grade.hooks.retain(|h| h.name != SHARPEN_HOOK);
    grade.sharpen = 0.0;
    if amount <= 0.0 {
        return;
    }
    match kind {
        Sharpen::Unsharp => grade.sharpen = amount,
        Sharpen::Adaptive => {
            grade.hooks.push(Hook {
                name: SHARPEN_HOOK.to_owned(),
                source: ADAPTIVE_SHARPEN.to_owned(),
                params: vec![("curve_height".to_owned(), f64::from(amount))],
                tracks: Vec::new(),
            });
            order(&mut grade.hooks);
        }
    }
}

/// Within a stage: the reframe/warp geometry, then sharpening, then user shaders (in timeline
/// order: grain after sharpening), then the review overlay on top.
fn role(h: &Hook) -> u8 {
    match h.name.as_str() {
        ffcine_reframe::pass::GEOMETRY_HOOK => 0,
        SHARPEN_HOOK => 1,
        ffcine_reframe::pass::SAFE_AREA_HOOK => 3,
        _ => 2,
    }
}

/// Sorts hooks by their first stage ([`parse::STAGES`]), then [`role`]; stable otherwise.
/// libplacebo runs each hook at its own stage regardless, so this only decides the order of
/// hooks sharing a stage, and makes the array read in pipeline order.
pub fn order(hooks: &mut [Hook]) {
    hooks.sort_by_cached_key(|h| {
        let stage = parse(&h.source).map_or(parse::STAGES.len(), |i| i.first_stage());
        (stage, role(h))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_reframe::{Profile, pass};

    fn user(name: &str, stage: &str) -> Hook {
        Hook {
            name: name.to_owned(),
            source: format!(
                "//!HOOK {stage}\n//!BIND HOOKED\nvec4 hook() {{ return HOOKED_texOff(0); }}\n"
            ),
            ..Hook::default()
        }
    }

    #[test]
    fn output_hooks_move_to_preoutput() {
        let t = retarget_output("//!HOOK OUTPUT\n//!BIND HOOKED\n//!HOOK MAIN\n");
        assert_eq!(t, "//!HOOK PREOUTPUT\n//!BIND HOOKED\n//!HOOK MAIN\n");
        assert_eq!(parse(ADAPTIVE_SHARPEN).unwrap().stages, ["PREOUTPUT"]);
    }

    #[test]
    fn licence_header_kept() {
        assert!(ADAPTIVE_SHARPEN.starts_with("// Copyright (c) 2015-2021, bacondither\n"));
        assert!(ADAPTIVE_SHARPEN.contains("THIS SOFTWARE IS PROVIDED BY THE AUTHORS ``AS IS''"));
        assert_eq!(vendored("adaptive-sharpen"), Some(ADAPTIVE_SHARPEN));
    }

    #[test]
    fn sharpen_replaces_unsharp() {
        let mut g = Grade::default();
        sharpen(&mut g, 0.6, Sharpen::Adaptive);
        assert!(g.sharpen == 0.0 && g.hooks.len() == 1);
        assert_eq!(
            g.hooks[0].params,
            [("curve_height".to_owned(), f64::from(0.6_f32))]
        );
        sharpen(&mut g, 0.45, Sharpen::Unsharp);
        assert!(g.hooks.is_empty() && (g.sharpen - 0.45).abs() < f32::EPSILON);
        sharpen(&mut g, 0.0, Sharpen::Adaptive);
        assert!(g.hooks.is_empty() && g.sharpen == 0.0);
    }

    #[test]
    fn hooks_in_pipeline_order() {
        let mut reel = Profile::preset("reel").expect("preset");
        reel.safe_area = Some(reel.default_safe_area());
        let r = ffcine_reframe::Reframe::default();
        let shot = ffcine_reframe::Shot {
            width: 3840,
            height: 2160,
            frames: 10,
            rate: 24.0,
            start: 0.0,
        };
        let mut g = Grade::default();
        g.hooks.push(user("grain", "PREOUTPUT"));
        g.hooks.push(user("krig", "CHROMA"));
        pass::apply(
            &mut g,
            &r.frames(&shot, &reel),
            &[ffcine_core::geom::IDENTITY],
            (3840, 2160),
            &reel,
        );
        sharpen(&mut g, 1.0, Sharpen::Adaptive);
        let names: Vec<&str> = g.hooks.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "krig",
                pass::GEOMETRY_HOOK,
                SHARPEN_HOOK,
                "grain",
                pass::SAFE_AREA_HOOK
            ]
        );
    }
}
