//! Just enough of the mpv `.hook` format to check a shader before libplacebo sees it: the stages
//! it hooks and its `//!PARAM`s (name, type, bounds, default).

use ffcine_core::{Error, Result};

/// libplacebo's hook stages in pipeline order (mpv names). `MAIN` is libplacebo's `RGB` stage,
/// after the planes are merged, converted to RGB and the source LUT (the grade) applied.
pub const STAGES: &[&str] = &[
    "RGB",
    "LUMA",
    "CHROMA",
    "ALPHA",
    "XYZ",
    "CHROMA_SCALED",
    "ALPHA_SCALED",
    "NATIVE",
    "MAINPRESUB",
    "MAIN",
    "LINEAR",
    "SIGMOID",
    "PREKERNEL",
    "POSTKERNEL",
    "SCALED",
    "PREOUTPUT",
    "OUTPUT",
];

/// Where `stage` runs in the pipeline ([`STAGES`] index); unknown names sort last.
pub fn stage_rank(stage: &str) -> usize {
    STAGES
        .iter()
        .position(|s| *s == stage)
        .unwrap_or(STAGES.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Int,
    Uint,
    Float,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParamInfo {
    pub name: String,
    pub kind: ParamKind,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// `None` for `ENUM` params, whose default is a name.
    pub default: Option<f64>,
}

impl ParamInfo {
    /// `v` if it fits the type and bounds.
    pub fn check(&self, v: f64) -> Result<f64> {
        let ok = v.is_finite()
            && self.min.is_none_or(|m| v >= m)
            && self.max.is_none_or(|m| v <= m)
            && (self.kind != ParamKind::Uint || v >= 0.0);
        if ok {
            Ok(v)
        } else {
            Err(Error::Invalid {
                what: "shader //!PARAM value (outside its bounds)",
                value: format!("{} = {v}", self.name),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HookInfo {
    /// Every `//!HOOK` stage, in file order.
    pub stages: Vec<String>,
    pub params: Vec<ParamInfo>,
}

impl HookInfo {
    pub fn param(&self, name: &str) -> Option<&ParamInfo> {
        self.params.iter().find(|p| p.name == name)
    }

    /// The earliest stage this shader hooks.
    pub fn first_stage(&self) -> usize {
        self.stages
            .iter()
            .map(|s| stage_rank(s))
            .min()
            .unwrap_or(STAGES.len())
    }
}

/// Reads the directives of a `.hook` shader.
pub fn parse(text: &str) -> Result<HookInfo> {
    let invalid = |line: &str| Error::Invalid {
        what: "mpv .hook directive",
        value: line.to_owned(),
    };
    let mut info = HookInfo::default();
    // The PARAM block being read and whether its default line was seen.
    let mut param: Option<(ParamInfo, bool)> = None;
    let finish = |info: &mut HookInfo, p: Option<(ParamInfo, bool)>| {
        if let Some((p, _)) = p {
            info.params.push(p);
        }
    };
    for line in text.lines() {
        let Some(d) = line.strip_prefix("//!") else {
            match &mut param {
                Some((p, seen)) if !*seen && !line.trim().is_empty() => {
                    *seen = true;
                    p.default = line.trim().parse().ok();
                }
                _ => {}
            }
            continue;
        };
        let mut words = d.split_whitespace();
        let key = words.next().unwrap_or_default();
        match key {
            "PARAM" => {
                finish(&mut info, param.take());
                let name = words.next().ok_or_else(|| invalid(line))?;
                param = Some((
                    ParamInfo {
                        name: name.to_owned(),
                        kind: ParamKind::Float,
                        min: None,
                        max: None,
                        default: None,
                    },
                    false,
                ));
            }
            "HOOK" | "TEXTURE" | "BUFFER" => {
                finish(&mut info, param.take());
                if key == "HOOK" {
                    info.stages
                        .push(words.next().ok_or_else(|| invalid(line))?.to_owned());
                }
            }
            "TYPE" | "MINIMUM" | "MAXIMUM" => {
                let Some((p, _)) = &mut param else { continue };
                let rest: Vec<&str> = words.collect();
                match key {
                    "TYPE" => {
                        // `[ENUM|DEFINE] [DYNAMIC|CONSTANT] type`
                        p.kind = match rest.last().copied() {
                            Some("int" | "ENUM" | "DEFINE") => ParamKind::Int,
                            Some("uint") => ParamKind::Uint,
                            Some("float") => ParamKind::Float,
                            _ => return Err(invalid(line)),
                        };
                        if rest.first() == Some(&"ENUM") {
                            p.kind = ParamKind::Int;
                        }
                    }
                    "MINIMUM" => p.min = rest.first().and_then(|v| v.parse().ok()),
                    _ => p.max = rest.first().and_then(|v| v.parse().ok()),
                }
            }
            _ => {}
        }
    }
    finish(&mut info, param);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_sharpen_directives() {
        let info = parse(crate::ADAPTIVE_SHARPEN).unwrap();
        assert_eq!(info.stages, ["PREOUTPUT"]);
        let ch = info.param("curve_height").unwrap();
        assert_eq!(
            (ch.kind, ch.default, ch.min),
            (ParamKind::Float, Some(1.0), Some(0.0))
        );
        let oc = info.param("overshoot_ctrl").unwrap();
        assert_eq!(
            (oc.kind, oc.default, oc.max),
            (ParamKind::Int, Some(0.0), Some(1.0))
        );
        assert!(ch.check(0.6).is_ok() && ch.check(-1.0).is_err());
        assert!(stage_rank("MAIN") < stage_rank("SCALED"));
        assert!(stage_rank("CHROMA") < stage_rank("MAIN"));
        assert!(stage_rank("PREOUTPUT") < stage_rank("OUTPUT"));
    }

    #[test]
    fn multi_pass_and_textures() {
        let text = "//!HOOK CHROMA\n//!BIND HOOKED\nvec4 hook() { return HOOKED_tex(HOOKED_pos); }\n\
                    //!PARAM k\n//!TYPE DYNAMIC uint\n3\n//!TEXTURE T\n//!SIZE 1 1\n00\n\
                    //!HOOK MAIN\n//!BIND HOOKED\n";
        let info = parse(text).unwrap();
        assert_eq!(info.stages, ["CHROMA", "MAIN"]);
        assert_eq!(info.first_stage(), stage_rank("CHROMA"));
        let k = info.param("k").unwrap();
        assert_eq!((k.kind, k.default), (ParamKind::Uint, Some(3.0)));
        assert!(parse("//!HOOK\n").is_err());
    }
}
