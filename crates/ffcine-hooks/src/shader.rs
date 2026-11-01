//! `ffcine.fx.shader/1 {shader, params}`: an mpv `.hook` shader on a clip, its `//!PARAM`s set
//! from the timeline.

use std::path::Path;

use ffcine_core::{Error, Result};
use ffcine_reframe::Keyed;
use ffcine_render::{Hook, ParamTrack};
use ffcine_timeline::Project;
use ffcine_timeline::schema::Effect;
use serde_json::{Map, Value};

use crate::parse::{HookInfo, parse};

pub const SCHEMA: &str = "ffcine.fx.shader";
pub const MAJOR: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct ShaderEffect {
    /// An alias in `project.shaders`, a vendored name ([`crate::vendored`]) or a path (relative to
    /// the timeline).
    pub shader: String,
    /// `//!PARAM` values: constants, or keyframes (`t` in the clip's source time).
    pub params: Vec<(String, Keyed<f64>)>,
}

impl ShaderEffect {
    /// From the effect's parameters (`metadata.ffcine` without `schema`).
    pub fn parse(params: &Map<String, Value>) -> Result<Self> {
        let shader = params
            .get("shader")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid {
                what: "ffcine.fx.shader/1 shader",
                value: Value::Object(params.clone()).to_string(),
            })?
            .to_owned();
        let mut out = Vec::new();
        if let Some(p) = params.get("params").and_then(Value::as_object) {
            for (k, v) in p {
                out.push((k.clone(), Keyed::parse(v, "ffcine.fx.shader/1 param")?));
            }
        }
        Ok(Self {
            shader,
            params: out,
        })
    }

    /// Every `ffcine.fx.shader/1` among a clip's effects, in order.
    pub fn from_effects(effects: &[Effect]) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        for e in effects {
            let Effect::Effect(g) = e else { continue };
            if let Some(p) = g.ffcine_params::<Map<String, Value>>(SCHEMA, MAJOR) {
                out.push(Self::parse(&p?)?);
            }
        }
        Ok(out)
    }

    /// The shader's name and text: `project.shaders` alias first, then vendored, then a path.
    /// Relative paths resolve against `base` (the timeline's directory). `OUTPUT` passes move to
    /// `PREOUTPUT` ([`crate::retarget_output`]).
    pub fn load(&self, project: &Project, base: &Path) -> Result<(String, String)> {
        let name = project
            .shaders
            .get(&self.shader)
            .map_or(self.shader.as_str(), String::as_str);
        if let Some(text) = crate::vendored(name) {
            return Ok((name.to_owned(), text.to_owned()));
        }
        let path = base.join(name);
        let text = std::fs::read_to_string(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        Ok((path.display().to_string(), crate::retarget_output(&text)))
    }

    /// The [`Hook`] for a clip of `frames` output frames at `rate`, starting at source time
    /// `start` seconds (`source_range.start_time`): constant params set once, keyframed ones as
    /// per-frame tracks. Every param must be a `//!PARAM` of the shader, in its bounds.
    pub fn hook(
        &self,
        project: &Project,
        base: &Path,
        (frames, rate, start): (u64, f64, f64),
    ) -> Result<Hook> {
        let (name, source) = self.load(project, base)?;
        let info = parse(&source)?;
        let mut hook = Hook {
            name,
            ..Hook::default()
        };
        self.set_params(&info, &mut hook, frames, rate, start)?;
        hook.source = source;
        Ok(hook)
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn set_params(
        &self,
        info: &HookInfo,
        hook: &mut Hook,
        frames: u64,
        rate: f64,
        start: f64,
    ) -> Result<()> {
        let rate = if rate > 0.0 { rate } else { 24.0 };
        for (k, v) in &self.params {
            let p = info.param(k).ok_or_else(|| Error::Invalid {
                what: "shader //!PARAM (not declared by the shader)",
                value: format!(
                    "{} in {} (has: {})",
                    k,
                    hook.name,
                    info.params
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })?;
            match v {
                Keyed::Const(c) => hook.params.push((k.clone(), p.check(*c)?)),
                Keyed::Keys(_) => {
                    let values = (0..frames.max(1))
                        .map(|i| {
                            let x = v.eval(start + i as f64 / rate).unwrap_or_default();
                            p.check(x).map(|x| x as f32)
                        })
                        .collect::<Result<_>>()?;
                    hook.tracks.push(ParamTrack {
                        name: k.clone(),
                        values,
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_timeline::schema::GenericEffect;
    use serde_json::json;

    fn effect(v: &Value) -> Effect {
        Effect::Effect(GenericEffect::ffcine(
            "ffcine.fx.shader/1",
            v.as_object().expect("object").clone(),
        ))
    }

    #[test]
    fn vendored_with_params_and_keys() {
        let mut project = Project::default();
        project
            .shaders
            .insert("sharp".into(), "adaptive-sharpen".into());
        let rt = |f: f64| json!({"OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": f});
        let fx = ShaderEffect::from_effects(&[
            effect(&json!({"shader": "sharp", "params": {"curve_height": 0.6}})),
            effect(
                &json!({"shader": "adaptive-sharpen", "params": {"curve_height": [
                    {"t": rt(0.0), "v": 0.0}, {"t": rt(24.0), "v": 2.0}
                ]}}),
            ),
        ])
        .unwrap();
        let h = fx[0]
            .hook(&project, Path::new("/"), (48, 24.0, 0.0))
            .unwrap();
        assert_eq!(h.name, "adaptive-sharpen");
        assert_eq!(h.params, [("curve_height".to_owned(), 0.6)]);
        let h = fx[1]
            .hook(&project, Path::new("/"), (48, 24.0, 0.0))
            .unwrap();
        let t = &h.tracks[0];
        assert_eq!(t.values.len(), 48);
        assert!((t.values[12] - 1.0).abs() < 1e-6 && (t.values[40] - 2.0).abs() < 1e-6);
        // Keys are in source time: a clip starting 0.5 s into the media starts halfway.
        let h = fx[1]
            .hook(&project, Path::new("/"), (48, 24.0, 0.5))
            .unwrap();
        assert!((h.tracks[0].values[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_unknown_and_out_of_range_params() {
        let p = Project::default();
        let e = |v: Value| ShaderEffect::parse(v.as_object().expect("object")).unwrap();
        let bad = e(json!({"shader": "adaptive-sharpen", "params": {"strength": 1.0}}));
        let msg = bad
            .hook(&p, Path::new("/"), (1, 24.0, 0.0))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("curve_height, overshoot_ctrl"), "{msg}");
        let bad = e(json!({"shader": "adaptive-sharpen", "params": {"overshoot_ctrl": 2}}));
        assert!(bad.hook(&p, Path::new("/"), (1, 24.0, 0.0)).is_err());
        assert!(ShaderEffect::parse(json!({"params": {}}).as_object().expect("o")).is_err());
    }

    #[test]
    fn loads_files_relative_to_the_timeline() {
        let dir = std::env::temp_dir().join(format!("ffcine-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("grain.glsl"),
            "//!PARAM g\n//!TYPE float\n0.1\n//!HOOK OUTPUT\n//!BIND HOOKED\n",
        )
        .unwrap();
        let mut project = Project::default();
        project.shaders.insert("grain".into(), "grain.glsl".into());
        let fx = ShaderEffect::parse(
            json!({"shader": "grain", "params": {"g": 0.3}})
                .as_object()
                .expect("o"),
        )
        .unwrap();
        let h = fx.hook(&project, &dir, (1, 24.0, 0.0)).unwrap();
        assert!(h.name.ends_with("grain.glsl"));
        assert!(h.source.starts_with("//!PARAM g"));
        assert!(h.source.contains("//!HOOK PREOUTPUT\n"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
