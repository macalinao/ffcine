//! Timeline effects (`docs/timeline.md#effect-schemas`): `ffcine.fx.denoise/1` and
//! `ffcine.fx.grain/1`, from an effect's `metadata.ffcine` object.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{Denoise, Grain, Spatial, Temporal};

pub const DENOISE: &str = "ffcine.fx.denoise/1";
pub const GRAIN: &str = "ffcine.fx.grain/1";

/// `ffcine.fx.denoise/1`: `luma`, `chroma` (spatial strengths, 8-bit code values), `low_light`
/// (bigger spatial search), `temporal: {radius, strength, chroma}`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DenoiseV1 {
    luma: Option<f32>,
    chroma: Option<f32>,
    low_light: bool,
    temporal: Option<Temporal>,
}

fn params<T: for<'de> Deserialize<'de>>(meta: &Map<String, Value>) -> Result<T, String> {
    let mut m = meta.clone();
    m.remove("schema");
    serde_json::from_value(Value::Object(m)).map_err(|e| e.to_string())
}

/// Reads `ffcine.fx.denoise/1` parameters (the effect's `metadata.ffcine`).
pub fn denoise(meta: &Map<String, Value>) -> Result<Denoise, String> {
    let v: DenoiseV1 = params(meta)?;
    let spatial = (v.luma.is_some() || v.chroma.is_some()).then(|| {
        let luma = v.luma.unwrap_or(0.0);
        Spatial {
            radius: if v.low_light { 3 } else { 2 },
            luma,
            chroma: v.chroma.unwrap_or(luma * 1.5),
        }
    });
    Ok(Denoise {
        temporal: v.temporal,
        spatial,
    }
    .clamped())
}

/// Reads `ffcine.fx.grain/1` parameters: `strength`, `size`, `chroma`; `profiles` is ignored
/// (see [`grain_for`]).
pub fn grain(meta: &Map<String, Value>) -> Result<Grain, String> {
    grain_for(meta, None)
}

/// `ffcine.fx.grain/1` for a render profile: `profiles.<profile>` (`preview`, `master`, ...)
/// overrides the top-level fields.
pub fn grain_for(meta: &Map<String, Value>, profile: Option<&str>) -> Result<Grain, String> {
    let mut m = meta.clone();
    let profiles = m.shift_remove("profiles");
    if let Some(Value::Object(over)) = profile.and_then(|p| profiles.as_ref()?.get(p)) {
        m.extend(over.clone());
    }
    params(&m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(s: &str) -> Map<String, Value> {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn denoise_v1() {
        let d = denoise(&obj(
            r#"{"schema": "ffcine.fx.denoise/1", "luma": 2, "low_light": true,
                "temporal": {"radius": 3, "strength": 5}}"#,
        ))
        .unwrap();
        let s = d.spatial.unwrap();
        assert_eq!((s.radius, s.luma, s.chroma), (3, 2.0, 3.0));
        assert_eq!(d.temporal.unwrap().radius, 3);
        assert!(denoise(&obj(r#"{"temporal": {"radius": 2, "bogus": 1}}"#)).is_err());
    }

    #[test]
    fn grain_v1() {
        let g = grain(&obj(r#"{"schema": "ffcine.fx.grain/1", "strength": 2}"#)).unwrap();
        assert!((g.strength - 2.0).abs() < f32::EPSILON);
        assert!((g.size - Grain::default().size).abs() < f32::EPSILON);
        let m = obj(r#"{"strength": 2, "chroma": 0.4,
                        "profiles": {"preview": {"strength": 1}, "master": {"strength": 1.5}}}"#);
        assert!((grain(&m).unwrap().strength - 2.0).abs() < f32::EPSILON);
        let g = grain_for(&m, Some("master")).unwrap();
        assert!((g.strength - 1.5).abs() < f32::EPSILON);
        assert!((g.chroma - 0.4).abs() < f32::EPSILON);
        assert!((grain_for(&m, Some("other")).unwrap().strength - 2.0).abs() < f32::EPSILON);
    }
}
