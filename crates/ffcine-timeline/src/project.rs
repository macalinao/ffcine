//! Project-level ffcine metadata (`Timeline.metadata.ffcine.project`) and grade layering.
//!
//! Schema `ffcine.project/1`, see `docs/timeline.md`. A clip's grade resolves as
//!
//! ```text
//! project.grade  <-  looks[look]  <-  source-kind rule  <-  clip's ffcine.color.grade/1
//! ```
//!
//! where `look` is the clip's `look`, else `project.default_look`, and the source-kind rule is the
//! prototype's "light" treatment of non-log footage (iPhone, AI): no LUT, gamma, lift, gain or
//! denoise, and warmth/contrast/saturation scaled by `rules.light`. Each layer only sets the
//! fields it has (`GradeParams` is all-`Option`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Schema tag of [`Project`].
pub const PROJECT_SCHEMA: &str = "ffcine.project/1";
/// Schema tag of a clip's grade effect ([`ClipGrade`]).
pub const GRADE_SCHEMA: &str = "ffcine.color.grade/1";

/// A partial grade: base, look or per-clip override. Units follow the prototype's ffmpeg chain
/// (`render.py` `grade_filters`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GradeParams {
    /// Camera profile (`ffcine_core::camera::Profile` name: `dlogm`, `dlogm-vivid`, `clog3`,
    /// `applelog`, `hdr`, `rec709`) or `auto` (from the probe, the default). Picks the camera
    /// LUT applied before the grade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
    /// LUT alias from `project.luts`, a `.cube` path, or `"none"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lut: Option<String>,
    /// LUT mix with identity, 0..1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lut_strength: Option<f64>,
    /// Per-channel multiplier before the LUT (log footage only unless set on the clip).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rgb_gain: Option<[f64; 3]>,
    /// + warmer mids/highs, - cooler (`colorbalance`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warmth: Option<f64>,
    /// + magenta, - green.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tint: Option<f64>,
    /// Stops, before the LUT.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<f64>,
    /// S-curve strength, 0 = off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<f64>,
    /// >1 brightens mids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gamma: Option<f64>,
    /// Raises blacks, 0..0.05.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lift: Option<f64>,
    /// Highlight gain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saturation: Option<f64>,
    /// Denoise strength (`hqdn3d` d; `nlmeans` s = 0.2 d), 0 = off. Settable in looks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denoise: Option<f64>,
    /// Unsharp amount, 0 = off. Settable in looks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpen: Option<f64>,
    /// Raw ffmpeg filters after the grade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Vec<String>>,
    /// Replaces `extra` under the light (non-log) rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_light: Option<Vec<String>>,
    /// Fields this version doesn't know, kept as-is.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

macro_rules! overlay_fields {
    ($self:ident, $top:ident; $($f:ident),*) => {
        $(if $top.$f.is_some() {
            $self.$f.clone_from(&$top.$f);
        })*
    };
}

impl GradeParams {
    /// Sets every field `top` has.
    pub fn overlay(&mut self, top: &GradeParams) {
        overlay_fields!(self, top; camera, lut, lut_strength, rgb_gain, warmth, tint, exposure, contrast,
            gamma, lift, gain, saturation, denoise, sharpen, extra, extra_light);
        for (k, v) in &top.other {
            self.other.insert(k.clone(), v.clone());
        }
    }

    /// The prototype's treatment of non-log footage at strength `k` (`render.py` `Project.grade`).
    pub fn apply_light(&mut self, k: f64) {
        self.exposure = None;
        self.rgb_gain = None;
        self.lut_strength = None;
        self.lut = Some("none".into());
        self.gamma = Some(1.0);
        self.lift = Some(0.0);
        self.gain = Some(1.0);
        self.denoise = Some(0.0);
        self.warmth = Some(self.warmth.unwrap_or(0.0) * k);
        self.contrast = Some(self.contrast.unwrap_or(0.0) * k);
        self.saturation = Some(1.0 + (self.saturation.unwrap_or(1.0) - 1.0) * k);
        if self.extra_light.is_some() {
            self.extra.clone_from(&self.extra_light);
        }
    }
}

/// What kind of source a clip is, for the source-kind grade rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Log footage that takes the camera LUT (DJI D-Log M).
    Log,
    /// Display-referred footage (iPhone, AI, stills): the light rule.
    Display,
}

/// Per-source-kind rules.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// Strength of the look on display-referred footage (prototype default 0.6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light: Option<f64>,
    /// iPhone yellow-green tamer LUT: `{lut, rot, knee, amount}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iphone_tame: Option<Value>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// `project.mix`: the timeline's mix settings, read by `ffcine audio` and `ffcine render`
/// (`docs/audio.md`). Unset fields keep `ffcine-audio`'s defaults (in brackets); the CLI's
/// flags override them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MixParams {
    /// Master loudness, LUFS [-14].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lufs: Option<f64>,
    /// `false` leaves the master loudness as mixed (still limited) [true].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalize: Option<bool>,
    /// Master true-peak ceiling, dBTP [-1].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub true_peak: Option<f64>,
    /// Limiter this far under `true_peak`, dB [default: 2].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headroom: Option<f64>,
    /// Master fade-out at the end, s [default: 0.6].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_out: Option<f64>,
    /// R128 target of `dialogue` clips, LUFS [-18].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialogue_lufs: Option<f64>,
    /// R128 target of `nat` clips, LUFS [-22].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nat_lufs: Option<f64>,
    /// Clamp of a dialogue clip's R128 gain, dB [-10, 14].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_range: Option<[f64; 2]>,
    /// Crossfade at dialogue cuts, s [default: 0.125].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xfade: Option<f64>,
    /// Music dip under `dialogue` clips without their own `duck_db`, dB [-14].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duck_db: Option<f64>,
    /// Music dip under `nat` clips, dB [-7].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nat_duck_db: Option<f64>,
    /// Duck look-ahead and hold, s [0.25, 0.45].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duck_attack: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duck_release: Option<f64>,
    /// Duck only while the dialogue is audible [false].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidechain: Option<bool>,
    /// Dialogue presence threshold of `sidechain`, dBFS [-40].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duck_threshold: Option<f64>,
    /// Clean-chain strength of dialogue clips without `ffcine.audio.clean/1` [default: 0.6].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean: Option<f64>,
    /// The same for `nat` clips [default: 0].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean_nat: Option<f64>,
    /// High-pass of that default chain, Hz [default: 80].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highpass: Option<f64>,
    /// Bus settings by name: `dialogue`, `music`, `ambience`, `sfx`, and any other name is an
    /// aux bus fed by sends.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub buses: BTreeMap<String, BusParams>,
    /// Fields this version doesn't know, kept as-is.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// One entry of `project.mix.buses`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BusParams {
    /// Fader, dB [0]: after the bus's `filter`, before its ducking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
    /// libavfilter chain over the bus (on an aux, after its `reverb`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Feeds to aux buses (dialogue, music, ambience and sfx only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<BusSend>,
    /// Aux only: a reverb return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverb: Option<BusReverb>,
    /// Aux only: a key bus. It isn't heard; while it is loud it ducks its targets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duck: Option<BusDuck>,
}

/// A feed from a bus to an aux bus.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BusSend {
    /// The aux bus.
    pub to: String,
    /// Send level, dB [0].
    pub gain_db: f64,
    /// Tap before the bus's fader and ducking, not after [false].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pre: bool,
}

/// `reverb` of an aux bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BusReverb {
    /// Decay, 0..=1 [0.5].
    pub room: f64,
    /// High-frequency damping, 0..=1 [0.5].
    pub damp: f64,
}

impl Default for BusReverb {
    fn default() -> Self {
        Self {
            room: 0.5,
            damp: 0.5,
        }
    }
}

/// `duck` of an aux bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BusDuck {
    /// The buses it ducks (of dialogue, music, ambience and sfx).
    pub targets: Vec<String>,
    /// How far, dB [-12].
    pub depth_db: f64,
    /// Ducks while the key's 10 ms RMS is over this, dBFS [-40].
    pub threshold_db: f64,
    /// Look-ahead and hold, s [0.04, 0.35].
    pub attack: f64,
    pub release: f64,
}

impl Default for BusDuck {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            depth_db: -12.0,
            threshold_db: -40.0,
            attack: 0.04,
            release: 0.35,
        }
    }
}

impl MixParams {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// `Timeline.metadata.ffcine.project`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub schema: String,
    /// Frame rate of the timeline (the render rate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
    /// Base grade.
    #[serde(default)]
    pub grade: GradeParams,
    /// Look used by clips that don't name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_look: Option<String>,
    #[serde(default)]
    pub looks: BTreeMap<String, GradeParams>,
    /// LUT aliases -> paths (relative to the timeline file).
    #[serde(default)]
    pub luts: BTreeMap<String, String>,
    /// `.hook` shader aliases -> paths.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shaders: BTreeMap<String, String>,
    #[serde(default)]
    pub rules: Rules,
    /// Text styles by name (`ffcine.text` clips pick one with `style`).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub styles: Map<String, Value>,
    /// Output frame of `ffcine render` (aspect, per-quality size and punch cap).
    #[serde(default, skip_serializing_if = "Output::is_empty")]
    pub output: Output,
    /// Mix settings.
    #[serde(default, skip_serializing_if = "MixParams::is_empty")]
    pub mix: MixParams,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// `project.output`: what `ffcine render` renders the timeline into. `ffcine render
/// --render-profile` overrides `aspect`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Output {
    /// Render profile: `landscape` (16:9), `reel` (9:16), `feed` (4:5), `square` (1:1), `WxH`
    /// or `name=WxH`. Unset: landscape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect: Option<String>,
    /// The `preview` render.
    #[serde(skip_serializing_if = "OutputMode::is_empty")]
    pub preview: OutputMode,
    /// The `master` render.
    #[serde(skip_serializing_if = "OutputMode::is_empty")]
    pub master: OutputMode,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

impl Output {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// `project.output.preview` / `.master`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputMode {
    /// `[w, h]` in pixels; unset: the aspect at 1080 (preview) or 2160 (master) on the short side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<[u32; 2]>,
    /// Cap on reframe punch-ins (`edit.yaml` `output.<mode>.max_punch`, default 1.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_punch: Option<f64>,
}

impl OutputMode {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for Project {
    fn default() -> Self {
        Self {
            schema: PROJECT_SCHEMA.into(),
            rate: None,
            grade: GradeParams::default(),
            default_look: None,
            looks: BTreeMap::new(),
            luts: BTreeMap::new(),
            shaders: BTreeMap::new(),
            rules: Rules::default(),
            styles: Map::new(),
            output: Output::default(),
            mix: MixParams::default(),
            other: Map::new(),
        }
    }
}

/// Parameters of a clip's `ffcine.color.grade/1` effect: a look and a partial override.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClipGrade {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<String>,
    #[serde(flatten)]
    pub params: GradeParams,
}

/// Errors from [`Project::resolve_grade`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GradeError {
    #[error("unknown look {0:?}")]
    UnknownLook(String),
}

impl Project {
    /// The layered grade for a clip (see the module docs). The result still has `lut` as an
    /// alias; [`Project::lut_path`] resolves it.
    pub fn resolve_grade(
        &self,
        clip: Option<&ClipGrade>,
        kind: SourceKind,
    ) -> Result<GradeParams, GradeError> {
        let mut g = self.grade.clone();
        let look = clip
            .and_then(|c| c.look.as_deref())
            .or(self.default_look.as_deref());
        if let Some(name) = look {
            let l = self
                .looks
                .get(name)
                .ok_or_else(|| GradeError::UnknownLook(name.to_owned()))?;
            g.overlay(l);
        }
        if kind == SourceKind::Display {
            g.apply_light(self.rules.light.unwrap_or(0.6));
        }
        if let Some(c) = clip {
            g.overlay(&c.params);
        }
        Ok(g)
    }

    /// The `.cube` path for a `lut` value: an alias, a path, or `None` for `"none"` / unset.
    pub fn lut_path<'a>(&'a self, lut: Option<&'a str>) -> Option<&'a str> {
        match lut {
            None | Some("" | "none") => None,
            Some(name) => Some(self.luts.get(name).map_or(name, String::as_str)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project() -> Project {
        serde_json::from_value(json!({
            "schema": PROJECT_SCHEMA,
            "grade": {"lut": "normal", "warmth": 0.0, "gamma": 1.0, "saturation": 1.0, "denoise": 0},
            "default_look": "day",
            "looks": {
                "day": {"warmth": 0.022, "contrast": 0.16, "saturation": 1.05,
                        "extra": ["a", "b"], "extra_light": ["b"]},
                "night": {"gamma": 1.22, "denoise": 3},
            },
            "luts": {"normal": "/luts/n.cube"},
            "rules": {"light": 0.5},
        }))
        .expect("project")
    }

    #[test]
    fn layers_base_look_override() {
        let p = project();
        let clip = ClipGrade {
            look: Some("night".into()),
            params: GradeParams {
                gamma: Some(1.3),
                ..GradeParams::default()
            },
        };
        let g = p
            .resolve_grade(Some(&clip), SourceKind::Log)
            .expect("grade");
        assert_eq!(g.gamma, Some(1.3));
        assert_eq!(g.denoise, Some(3.0));
        assert_eq!(g.warmth, Some(0.0));
        assert_eq!(p.lut_path(g.lut.as_deref()), Some("/luts/n.cube"));

        let g = p.resolve_grade(None, SourceKind::Log).expect("grade");
        assert_eq!(g.warmth, Some(0.022));
        assert_eq!(g.extra, Some(vec!["a".to_owned(), "b".to_owned()]));
    }

    #[test]
    fn light_rule_then_override() {
        let p = project();
        let clip = ClipGrade {
            params: GradeParams {
                lut: Some("normal".into()),
                ..GradeParams::default()
            },
            ..ClipGrade::default()
        };
        let g = p
            .resolve_grade(Some(&clip), SourceKind::Display)
            .expect("grade");
        assert!((g.warmth.unwrap_or_default() - 0.011).abs() < 1e-12);
        assert!((g.saturation.unwrap_or_default() - 1.025).abs() < 1e-12);
        assert_eq!(g.gamma, Some(1.0));
        assert_eq!(g.extra, Some(vec!["b".to_owned()]));
        // A clip override wins over the light rule, as in the prototype.
        assert_eq!(g.lut.as_deref(), Some("normal"));
    }

    #[test]
    fn unknown_look() {
        let p = project();
        let clip = ClipGrade {
            look: Some("noir".into()),
            ..ClipGrade::default()
        };
        assert_eq!(
            p.resolve_grade(Some(&clip), SourceKind::Log),
            Err(GradeError::UnknownLook("noir".into()))
        );
    }

    #[test]
    fn grade_effect_round_trips_unknown_fields() {
        let v = json!({"look": "day", "gamma": 1.2, "future": {"x": 1}});
        let g: ClipGrade = serde_json::from_value(v.clone()).expect("grade");
        assert_eq!(g.params.other["future"], json!({"x": 1}));
        assert_eq!(serde_json::to_value(&g).expect("json"), v);
    }
}
