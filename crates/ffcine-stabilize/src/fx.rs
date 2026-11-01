//! The timeline's `ffcine.fx.stabilize/1` effect: its parameters as [`StabilizeParams`].
//!
//! `smoothing` (frames, vidstab's), `zoom` (`"auto"` or a fixed factor) and `max_zoom` drive
//! [`stabilize`](crate::stabilize). render.py's vidstab-only keys (`shakiness`, `accuracy`,
//! `zoomspeed`) have no NVOF equivalent and are ignored; the detect side always uses
//! [`DetectParams::default`](crate::DetectParams) so every clip shares the cache.

use serde_json::{Map, Value};

use crate::{StabilizeParams, Zoom};
use ffcine_core::Error;

/// Schema tag of the effect.
pub const SCHEMA: &str = "ffcine.fx.stabilize/1";

/// [`StabilizeParams`] from the effect's parameters (`metadata.ffcine` minus `schema`).
pub fn params(m: &Map<String, Value>) -> Result<StabilizeParams, Error> {
    let bad = |what: &'static str, v: &Value| Error::Invalid {
        what,
        value: v.to_string(),
    };
    let mut p = StabilizeParams::default();
    if let Some(v) = m.get("smoothing") {
        let s = v
            .as_f64()
            .filter(|s| s.is_finite() && *s >= 0.0)
            .ok_or_else(|| bad("stabilize smoothing", v))?;
        p.smoothing = s as u32;
    }
    let max = match m.get("max_zoom") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|z| *z >= 1.0)
                .ok_or_else(|| bad("stabilize max_zoom", v))?,
        ),
    };
    p.zoom = match m.get("zoom") {
        None | Some(Value::Null) => Zoom::Auto { max },
        Some(Value::String(s)) if s == "auto" => Zoom::Auto { max },
        Some(v) => Zoom::Fixed(
            v.as_f64()
                .filter(|z| *z > 0.0)
                .ok_or_else(|| bad("stabilize zoom", v))?,
        ),
    };
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    #[test]
    fn from_effect() {
        // What the edit.yaml importer writes.
        let p = params(&obj(
            json!({"smoothing": 22, "shakiness": 6, "accuracy": 15,
            "zoomspeed": 0.25}),
        ))
        .unwrap();
        assert_eq!(p.smoothing, 22);
        assert_eq!(p.zoom, Zoom::Auto { max: None });
        let p = params(&obj(json!({"zoom": 1.1}))).unwrap();
        assert_eq!(p.zoom, Zoom::Fixed(1.1));
        assert_eq!(
            params(&obj(json!({"max_zoom": 1.2}))).unwrap().zoom,
            Zoom::Auto { max: Some(1.2) }
        );
        assert!(params(&obj(json!({"smoothing": "lots"}))).is_err());
    }
}
