//! ASC CDL: `.cdl` (`ColorDecisionList`), `.ccc` (`ColorCorrectionCollection`) and `.cc`
//! (`ColorCorrection`) files, and the `ffcine.color.cdl/1` effect with its `metadata.cdl` mirror
//! (the CMX adapter's shape, `{asc_sop: {slope, offset, power}, asc_sat}`).
//!
//! The EDL importer ([`crate::edl`]) reads a sidecar file through [`read`] and matches its
//! corrections to events by id (`*ASC_CC_XML <id>` comments, else clip name, reel or event
//! number).

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::common::ffcine_effect;
use crate::xml::Element;
use ffcine_core::{Error, Result};
use ffcine_timeline::{Clip, Effect};

/// One ASC colour correction.
#[derive(Debug, Clone, PartialEq)]
pub struct Cdl {
    /// `ColorCorrection id`, if any.
    pub id: Option<String>,
    pub slope: [f64; 3],
    pub offset: [f64; 3],
    pub power: [f64; 3],
    /// `SatNode` / `*ASC_SAT`; `None` = 1.
    pub saturation: Option<f64>,
    /// `Description` text, kept for export.
    pub description: Vec<String>,
}

impl Default for Cdl {
    fn default() -> Self {
        Self {
            id: None,
            slope: [1.0; 3],
            offset: [0.0; 3],
            power: [1.0; 3],
            saturation: None,
            description: Vec::new(),
        }
    }
}

impl Cdl {
    /// `metadata.cdl` as the CMX adapter writes it.
    pub fn mirror(&self) -> Value {
        let mut m =
            json!({"asc_sop": {"slope": self.slope, "offset": self.offset, "power": self.power}});
        if let Some(s) = self.saturation {
            m["asc_sat"] = s.into();
        }
        m
    }

    /// The `ffcine.color.cdl/1` effect.
    pub fn effect(&self) -> Effect {
        let mut p = Map::new();
        p.insert("slope".into(), json!(self.slope));
        p.insert("offset".into(), json!(self.offset));
        p.insert("power".into(), json!(self.power));
        if let Some(s) = self.saturation {
            p.insert("saturation".into(), s.into());
        }
        ffcine_effect("ffcine.color.cdl/1", p)
    }

    /// Adds the effect and the `metadata.cdl` mirror to `clip`, replacing an earlier CDL.
    pub fn apply(&self, clip: &mut Clip) {
        clip.effects
            .retain(|e| e.effect_name() != Some("ffcine.color.cdl"));
        clip.effects.push(self.effect());
        clip.metadata.insert("cdl".into(), self.mirror());
    }

    /// From a `metadata.cdl` mirror.
    pub fn from_mirror(v: &Value) -> Option<Self> {
        let sop = v.get("asc_sop")?;
        let three = |k: &str| -> Option<[f64; 3]> {
            let a = sop.get(k)?.as_array()?;
            Some([
                a.first()?.as_f64()?,
                a.get(1)?.as_f64()?,
                a.get(2)?.as_f64()?,
            ])
        };
        Some(Self {
            slope: three("slope")?,
            offset: three("offset")?,
            power: three("power")?,
            saturation: v.get("asc_sat").and_then(Value::as_f64),
            ..Self::default()
        })
    }

    /// `*ASC_SOP (s s s)(o o o)(p p p)` (the part after `ASC_SOP`).
    pub fn parse_sop(s: &str) -> Option<([f64; 3], [f64; 3], [f64; 3])> {
        let groups: Vec<Vec<f64>> = s
            .split(['(', ')'])
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(|g| {
                g.split_whitespace()
                    .filter_map(|n| n.parse().ok())
                    .collect()
            })
            .collect();
        let t = |g: &Vec<f64>| -> Option<[f64; 3]> { (g.len() == 3).then(|| [g[0], g[1], g[2]]) };
        match &groups[..] {
            [s, o, p] => Some((t(s)?, t(o)?, t(p)?)),
            _ => None,
        }
    }
}

/// Reads every `ColorCorrection` in a `.cdl`, `.ccc` or `.cc` file, in order.
pub fn read(path: &Path) -> Result<Vec<Cdl>> {
    let src = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    parse(&src).map_err(|e| Error::Invalid {
        what: "ASC CDL file",
        value: format!("{}: {e}", path.display()),
    })
}

/// [`read`] on a string.
pub fn parse(src: &str) -> std::result::Result<Vec<Cdl>, String> {
    let root = Element::parse(src).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    collect(&root, &mut out)?;
    if out.is_empty() {
        return Err(format!("no ColorCorrection in <{}>", root.name));
    }
    Ok(out)
}

fn collect(el: &Element, out: &mut Vec<Cdl>) -> std::result::Result<(), String> {
    if el.name == "ColorCorrection" {
        out.push(correction(el)?);
        return Ok(());
    }
    for c in &el.children {
        collect(c, out)?;
    }
    Ok(())
}

fn correction(el: &Element) -> std::result::Result<Cdl, String> {
    let mut cdl = Cdl {
        id: el.attr("id").map(str::to_owned),
        ..Cdl::default()
    };
    let three = |e: &Element, name: &str| -> std::result::Result<Option<[f64; 3]>, String> {
        let Some(c) = e.child(name) else {
            return Ok(None);
        };
        let v: Vec<f64> = c
            .text
            .split_whitespace()
            .map(|n| n.parse().map_err(|_| format!("bad {name} {:?}", c.text)))
            .collect::<std::result::Result<_, _>>()?;
        match v[..] {
            [a, b, c] => Ok(Some([a, b, c])),
            _ => Err(format!("{name} needs three numbers, got {:?}", c.text)),
        }
    };
    for c in &el.children {
        match c.name.as_str() {
            "SOPNode" => {
                if let Some(s) = three(c, "Slope")? {
                    cdl.slope = s;
                }
                if let Some(o) = three(c, "Offset")? {
                    cdl.offset = o;
                }
                if let Some(p) = three(c, "Power")? {
                    cdl.power = p;
                }
                cdl.description
                    .extend(c.children_named("Description").map(|d| d.text.clone()));
            }
            "SatNode" | "SATNode" => {
                let s = c.child("Saturation").map_or("", |s| s.text.as_str());
                cdl.saturation = Some(
                    s.trim()
                        .parse()
                        .map_err(|_| format!("bad Saturation {s:?}"))?,
                );
            }
            "Description" => cdl.description.push(c.text.clone()),
            _ => {}
        }
    }
    Ok(cdl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ccc_and_cc() {
        let ccc = r#"<?xml version="1.0" encoding="UTF-8"?>
<ColorCorrectionCollection xmlns="urn:ASC:CDL:v1.01">
  <ColorCorrection id="sh010">
    <SOPNode><Description>warm</Description>
      <Slope>1.1 1.0 0.9</Slope><Offset>0.01 0 -0.01</Offset><Power>1 1 1.05</Power>
    </SOPNode>
    <SatNode><Saturation>0.9</Saturation></SatNode>
  </ColorCorrection>
  <ColorCorrection id="sh020"><SOPNode><Slope>2 2 2</Slope></SOPNode></ColorCorrection>
</ColorCorrectionCollection>"#;
        let v = parse(ccc).expect("ccc");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id.as_deref(), Some("sh010"));
        assert_eq!(v[0].slope, [1.1, 1.0, 0.9]);
        assert_eq!(v[0].saturation, Some(0.9));
        assert_eq!(v[0].description, ["warm"]);
        assert_eq!(v[1].offset, [0.0; 3]);
        let cc =
            "<ColorCorrection id=\"x\"><SOPNode><Power>1 2 3</Power></SOPNode></ColorCorrection>";
        assert_eq!(parse(cc).expect("cc")[0].power, [1.0, 2.0, 3.0]);
        assert!(parse("<ColorCorrectionCollection/>").is_err());
        assert!(
            parse("<ColorCorrection><SOPNode><Slope>1 2</Slope></SOPNode></ColorCorrection>")
                .is_err()
        );
    }

    #[test]
    fn sop_and_mirror() {
        let (s, o, p) = Cdl::parse_sop("(1.1 1 0.9)(0.01 0 -0.01)(1 1 1.05)").expect("sop");
        assert_eq!((s[0], o[2], p[2]), (1.1, -0.01, 1.05));
        assert!(Cdl::parse_sop("(1 1)(0 0 0)(1 1 1)").is_none());
        let c = Cdl {
            slope: s,
            offset: o,
            power: p,
            saturation: Some(0.8),
            ..Cdl::default()
        };
        assert_eq!(Cdl::from_mirror(&c.mirror()), Some(c));
    }
}
