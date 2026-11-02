//! `.hook` shaders (roadmap step 9, `docs/design.md#roadmap`, `docs/hooks.md`): mpv user shaders
//! run in the libplacebo grade pass through `pl_mpv_user_shader_parse`, as
//! [`ffcine_render::Hook`]s with their `//!PARAM`s set from the timeline.
//!
//! - Vendored: [`ADAPTIVE_SHARPEN`] (BSD-2-Clause, header kept), the default sharpen, replacing
//!   the generated unsharp hook (`ffcine_render::shaders::unsharp`), which stays for prototype
//!   diffs ([`Sharpen::Unsharp`]); ArtCNN's luma doublers ([`ARTCNN_C4F16`], MIT).
//! - Not vendored (GPL/LGPL): KrigBilateral and FSRCNNX are known by name ([`EXTERNAL`]) and
//!   loaded from user-supplied files on the shader path ([`shader_path`]).
//! - Timeline: `ffcine.fx.shader/1 {shader, params}` with `shader` an alias in
//!   `metadata.ffcine.project.shaders`, a vendored name or a path; `params` constants or
//!   keyframes ([`shader`]).
//! - Order: [`order`] sorts a grade's hooks by stage, then geometry, sharpen, user shaders and the
//!   review overlay within a stage.
//!
//! Our own filters (denoise, grain) are compute passes in `ffcine-filters` instead.

pub mod parse;
pub mod shader;

use std::path::{Path, PathBuf};

use ffcine_core::{Error, Result};
use ffcine_render::{Grade, Hook};

pub use parse::{HookInfo, ParamInfo, ParamKind, parse};
pub use shader::ShaderEffect;

/// bacondither's Adaptive-sharpen (mpv port by igv) at `PREOUTPUT`, with `curve_height` and
/// `overshoot_ctrl` as `//!PARAM`s. Licence: BSD-2-Clause (header in the file).
pub const ADAPTIVE_SHARPEN: &str = include_str!("../shaders/adaptive-sharpen.glsl");

/// ArtCNN C4F16 (Artoriuz), a CNN luma doubler at `LUMA`: runs when the output is more than
/// 1.3x the source on both axes. Licence: MIT (header in the file, `shaders/LICENSE-ArtCNN`).
pub const ARTCNN_C4F16: &str = include_str!("../shaders/artcnn-c4f16.glsl");

/// [`ARTCNN_C4F16`] trained to also denoise and sharpen.
pub const ARTCNN_C4F16_DS: &str = include_str!("../shaders/artcnn-c4f16-ds.glsl");

/// Vendored shaders by name.
pub fn vendored(name: &str) -> Option<&'static str> {
    match name {
        "adaptive-sharpen" => Some(ADAPTIVE_SHARPEN),
        "artcnn-c4f16" => Some(ARTCNN_C4F16),
        "artcnn-c4f16-ds" => Some(ARTCNN_C4F16_DS),
        _ => None,
    }
}

/// A shader known by name whose licence keeps it out of ffcine (Apache-2.0): the user downloads
/// `file` from `url` into a [`shader_path`] directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct External {
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub licence: &'static str,
}

/// Shaders loaded by name from the [`shader_path`], not shipped.
pub const EXTERNAL: &[External] = &[
    External {
        name: "krigbilateral",
        file: "KrigBilateral.glsl",
        url: "https://gist.github.com/igv/a015fc885d5c22e6891820ad89555637",
        licence: "LGPL-3.0-or-later",
    },
    External {
        name: "fsrcnnx",
        file: "FSRCNNX_x2_8-0-4-1.glsl",
        url: "https://github.com/igv/FSRCNN-TensorFlow/releases",
        licence: "GPL-3.0",
    },
    External {
        name: "fsrcnnx-16",
        file: "FSRCNNX_x2_16-0-4-1.glsl",
        url: "https://github.com/igv/FSRCNN-TensorFlow/releases",
        licence: "GPL-3.0",
    },
];

/// Directories searched for [`EXTERNAL`] shaders: `FFCINE_SHADER_PATH` (`:`-separated), then
/// `$XDG_DATA_HOME/ffcine/shaders` (default `~/.local/share/ffcine/shaders`).
pub fn shader_path() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("FFCINE_SHADER_PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    dirs.extend(data.map(|d| d.join("ffcine/shaders")));
    dirs
}

/// The [`EXTERNAL`] shader `name`'s path and text, from the first of `dirs` holding its file.
/// `None` if `name` isn't external; an error naming the download if no directory has it.
pub fn external(name: &str, dirs: &[PathBuf]) -> Option<Result<(PathBuf, String)>> {
    let e = EXTERNAL.iter().find(|e| e.name == name)?;
    let Some(path) = dirs.iter().map(|d| d.join(e.file)).find(|p| p.is_file()) else {
        let dirs: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
        return Some(Err(Error::Import {
            path: PathBuf::from(e.file),
            msg: format!(
                "{} is {} and not bundled with ffcine: download it from {} into one of [{}] \
                 (or a directory on FFCINE_SHADER_PATH)",
                e.name,
                e.licence,
                e.url,
                dirs.join(", ")
            ),
        }));
    };
    Some(
        std::fs::read_to_string(&path)
            .map(|t| (path.clone(), t))
            .map_err(|source| Error::Io { path, source }),
    )
}

/// A shader by name, vendored or [`EXTERNAL`] (on the [`shader_path`]), as a [`Hook`] named
/// after it. `None` if the name is neither.
pub fn named(name: &str) -> Option<Result<Hook>> {
    let source = match vendored(name) {
        Some(t) => Ok(t.to_owned()),
        None => external(name, &shader_path())?.map(|(_, t)| retarget_output(&t)),
    };
    Some(source.map(|source| Hook {
        name: name.to_owned(),
        source,
        ..Hook::default()
    }))
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

/// Sorts hooks by their first stage ([`parse::STAGES`]), then by role; stable otherwise.
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
        for s in [ARTCNN_C4F16, ARTCNN_C4F16_DS] {
            assert!(s.starts_with("// MIT License\n"));
            assert!(s.contains("The above copyright notice and this permission notice"));
            assert!(parse(s).unwrap().stages.iter().all(|s| s == "LUMA"));
        }
        assert_eq!(vendored("artcnn-c4f16-ds"), Some(ARTCNN_C4F16_DS));
    }

    #[test]
    fn external_from_shader_path() {
        let dir =
            std::env::temp_dir().join(format!("ffcine-hooks-external-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = [dir.join("missing"), dir.clone()];
        assert!(external("adaptive-sharpen", &dirs).is_none());
        let err = external("krigbilateral", &dirs).unwrap().unwrap_err();
        assert!(err.to_string().contains("LGPL-3.0-or-later"), "{err}");
        assert!(err.to_string().contains("gist.github.com/igv"), "{err}");
        let src = "//!HOOK CHROMA\n//!BIND HOOKED\nvec4 hook() { return HOOKED_texOff(0); }\n";
        std::fs::write(dir.join("KrigBilateral.glsl"), src).unwrap();
        let (path, text) = external("krigbilateral", &dirs).unwrap().unwrap();
        assert_eq!((path, text.as_str()), (dir.join("KrigBilateral.glsl"), src));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(named("artcnn-c4f16").unwrap().unwrap().source, ARTCNN_C4F16);
        assert!(named("nope").is_none());
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
