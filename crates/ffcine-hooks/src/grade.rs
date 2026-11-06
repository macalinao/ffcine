//! A resolved timeline grade ([`GradeParams`]) as a GPU [`Grade`]: the grade LUT mixed by
//! `lut_strength`, the post-LUT look hook, `extra` filters as hooks, `rgb_gain` and `sharpen`.
//! Shared by timeline shots (`ffcine render`, `looks`, `still`) and comp layers' `grade`, so
//! the same grade renders the same pixels in both.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ffcine_core::{Error, Lut, LutType, Result};
use ffcine_render::shaders::{self, LookParams};
use ffcine_render::{Grade, Hook, LutStage};
use ffcine_timeline::{GradeParams, Project};

/// Loaded LUTs by path and strength.
#[derive(Debug, Default)]
pub struct Luts(HashMap<(PathBuf, u32), Lut>);

impl Luts {
    /// The LUT at `path` mixed with identity by `strength` (`lut_strength`, [`Lut::mixed`]).
    pub fn get(&mut self, path: &Path, strength: f64) -> Result<Lut> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let key = (path.to_owned(), (strength.clamp(0.0, 1.0) * 1000.0) as u32);
        if let Some(l) = self.0.get(&key) {
            return Ok(l.clone());
        }
        #[allow(clippy::cast_possible_truncation)]
        let lut = Lut::from_cube_file(path)
            .map_err(|e| Error::Pipeline(format!("LUT {}: {e}", path.display())))?
            .mixed(strength.clamp(0.0, 1.0) as f32);
        self.0.insert(key, lut.clone());
        Ok(lut)
    }
}

/// The look hook's parameters in `g` (unset ones at their identity).
#[allow(clippy::cast_possible_truncation)]
pub fn look_params(g: &GradeParams) -> LookParams {
    let f = |v: Option<f64>, d: f32| v.map_or(d, |v| v as f32);
    LookParams {
        exposure: f(g.exposure, 0.0),
        warmth: f(g.warmth, 0.0),
        tint: f(g.tint, 0.0),
        contrast: f(g.contrast, 0.0),
        gamma: f(g.gamma, 1.0),
        lift: f(g.lift, 0.0),
        gain: f(g.gain, 1.0),
        saturation: f(g.saturation, 1.0),
    }
}

/// The look hook for `look`, `None` at identity.
pub fn look_hook(look: &LookParams) -> Option<Hook> {
    (!look.is_identity()).then(|| Hook {
        name: "look".into(),
        source: shaders::look().to_owned(),
        params: look.params(),
        tracks: Vec::new(),
    })
}

/// The GPU grade for resolved `params`: `rgb_gain` ahead of the LUT (alias or path, relative to
/// `dir`, mixed by `lut_strength`), the post-LUT look hook (warmth, tint, contrast, gamma, lift,
/// gain, saturation), `extra` filters as hooks ([`ffcine_render::extra`]), plus `sharpen`.
/// Returns what isn't applied as warnings.
#[allow(clippy::cast_possible_truncation)]
pub fn grade_for(
    project: &Project,
    dir: &Path,
    g: &GradeParams,
    luts: &mut Luts,
) -> Result<(Grade, Vec<String>)> {
    let lut = project
        .lut_path(g.lut.as_deref())
        .map(|p| luts.get(&dir.join(p), g.lut_strength.unwrap_or(1.0)))
        .transpose()?
        .map(|lut| LutStage {
            lut,
            kind: LutType::Normalized,
        });
    let mut skipped = Vec::new();
    if g.denoise.is_some_and(|v| v != 0.0) {
        skipped.push("denoise not applied yet".to_owned());
    }
    let mut hooks: Vec<Hook> = look_hook(&look_params(g)).into_iter().collect();
    let (extra, warn) = ffcine_render::extra::hooks(g.extra.as_deref().unwrap_or_default());
    hooks.extend(extra);
    skipped.extend(warn);
    Ok((
        Grade {
            lut,
            rgb_gain: g.rgb_gain.map(|v| v.map(|c| c as f32)),
            hooks,
            sharpen: g.sharpen.map_or(0.0, |v| v as f32),
            ..Grade::default()
        },
        skipped,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grade_maps_the_look() {
        let p = Project::default();
        let g = GradeParams {
            warmth: Some(0.02),
            exposure: Some(0.3),
            ..GradeParams::default()
        };
        let (grade, skipped) =
            grade_for(&p, Path::new("."), &g, &mut Luts::default()).expect("grade");
        assert_eq!(grade.hooks.len(), 1);
        assert!(
            grade.hooks[0]
                .params
                .contains(&("exposure".to_owned(), f64::from(0.3_f32)))
        );
        assert_eq!(skipped, Vec::<String>::new());
        let (grade, _) = grade_for(
            &p,
            Path::new("."),
            &GradeParams::default(),
            &mut Luts::default(),
        )
        .expect("grade");
        assert_eq!(grade.hooks, []);
        assert_eq!(grade.rgb_gain, None);
        // rgb_gain goes to the GPU grade; extra filters follow the look as hooks.
        let g = GradeParams {
            warmth: Some(0.02),
            rgb_gain: Some([1.1, 1.0, 0.9]),
            extra: Some(vec!["eq=saturation=1.2,hqdn3d".into()]),
            ..GradeParams::default()
        };
        let (grade, skipped) =
            grade_for(&p, Path::new("."), &g, &mut Luts::default()).expect("grade");
        assert_eq!(grade.rgb_gain, Some([1.1, 1.0, 0.9]));
        let names: Vec<_> = grade.hooks.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["look", "extra eq"]);
        assert_eq!(skipped, ["extra filter hqdn3d not supported"]);
    }
}
