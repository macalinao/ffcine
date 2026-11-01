//! The audio metadata schemas (`docs/audio.md`): `ffcine.audio.clean/1`, `ffcine.audio.level/1`
//! (clip effects), `ffcine.audio.sync/1` (clip metadata written by `ffcine sync`) and the track
//! `bus`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use ffcine_core::{Error, Result};

use crate::mix::ClipDefaults;
use ffcine_timeline::{Clip, Effect, GenericEffect, HasMetadata, RationalTime, Track};

pub const CLEAN: &str = "ffcine.audio.clean";
pub const LEVEL: &str = "ffcine.audio.level";
pub const SYNC: &str = "ffcine.audio.sync";
pub const PAN: &str = "ffcine.audio.pan";
pub const LOOP: &str = "ffcine.audio.loop";

/// Keyframe shaping of the segment to the next key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Interp {
    #[default]
    Linear,
    Ease,
    Hold,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Key {
    /// Clip source time.
    pub t: RationalTime,
    pub v: f64,
    #[serde(default)]
    pub interp: Interp,
}

/// A number, or keyframes over the clip's source time (`docs/timeline.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Param {
    Value(f64),
    Keys(Vec<Key>),
}

impl Default for Param {
    fn default() -> Self {
        Param::Value(0.0)
    }
}

impl From<f64> for Param {
    fn from(v: f64) -> Self {
        Param::Value(v)
    }
}

impl Param {
    pub fn is_constant(&self) -> bool {
        matches!(self, Param::Value(_)) || matches!(self, Param::Keys(k) if k.len() < 2)
    }

    /// The value at source time `t` (s); held before the first and after the last key.
    pub fn at(&self, t: f64) -> f64 {
        let keys = match self {
            Param::Value(v) => return *v,
            Param::Keys(k) => k,
        };
        let Some(first) = keys.first() else {
            return 0.0;
        };
        let i = keys.partition_point(|k| k.t.to_seconds() <= t);
        if i == 0 {
            return first.v;
        }
        let a = &keys[i - 1];
        let Some(b) = keys.get(i) else { return a.v };
        let (ta, tb) = (a.t.to_seconds(), b.t.to_seconds());
        let u = if tb > ta {
            ((t - ta) / (tb - ta)).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let u = match a.interp {
            Interp::Linear => u,
            Interp::Ease => u * u * (3.0 - 2.0 * u),
            Interp::Hold => 0.0,
        };
        a.v + (b.v - a.v) * u
    }
}

/// `ffcine.audio.pan/1`: balance, -1 (left) .. 1 (right).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Pan {
    pub pan: Param,
}

/// Which bus a track feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Bus {
    #[default]
    Dialogue,
    Music,
    Ambience,
    Sfx,
}

impl Bus {
    pub const ALL: [Bus; 4] = [Bus::Dialogue, Bus::Music, Bus::Ambience, Bus::Sfx];

    /// `metadata.ffcine.bus` on the track, else guessed from its name (`music`, `amb`, `sfx`),
    /// else dialogue.
    pub fn of_track(track: &Track) -> Self {
        if let Some(Ok(b)) = track.ffcine_get::<Bus>("bus") {
            return b;
        }
        let n = track.name.to_ascii_lowercase();
        if n.contains("music") {
            Bus::Music
        } else if n.contains("amb") {
            Bus::Ambience
        } else if n.contains("sfx") || n.contains("fx") {
            Bus::Sfx
        } else {
            Bus::Dialogue
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Bus::Dialogue => "dialogue",
            Bus::Music => "music",
            Bus::Ambience => "ambience",
            Bus::Sfx => "sfx",
        }
    }
}

/// Denoiser of the clean chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Denoise {
    Off,
    /// `afftdn nr=6+10c nf=-34+6c tn=1` (the prototype's).
    #[default]
    Afftdn,
    /// `arnndn` with an `RNNoise` model file (`model`); speech only.
    Arnndn,
}

/// `ffcine.audio.clean/1`: per-clip clean chain, run through libavfilter in this order:
/// high-pass, wind, de-click, denoise, dialogue polish (EQ, de-esser, compressor). Dialogue-bus
/// clips without one get [`Clean::for_mode`], the prototype's `clean_filters`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Clean {
    /// Strength `c` in 0..=1 (`strength` in `edit.yaml` imports; the prototype's `clean`).
    /// 0 turns denoise and the EQ/de-esser off.
    #[serde(alias = "strength")]
    pub amount: f64,
    /// High-pass corner in Hz (12 dB/oct); `null` for none.
    pub highpass: Option<f64>,
    /// Outdoor wind preset: 24 dB/oct high-pass at 150 Hz, low shelf -6 dB at 300 Hz, de-click
    /// for gust pops.
    pub wind: bool,
    pub declick: bool,
    pub denoise: Denoise,
    /// `RNNoise` model for `arnndn`.
    pub model: Option<String>,
    /// Dialogue polish: presence EQ (+1.8c dB at 3.2 kHz), `deesser i=0.35c`, `acompressor`
    /// -24 dB 2.5:1 8/160 ms. Default: on for dialogue (`orig`), off otherwise.
    pub dialogue: Option<bool>,
}

impl Default for Clean {
    fn default() -> Self {
        Self {
            amount: 0.6,
            highpass: Some(80.0),
            wind: false,
            declick: false,
            denoise: Denoise::Afftdn,
            model: None,
            dialogue: None,
        }
    }
}

impl Clean {
    /// The prototype's chain for a clip of this mode: `orig` at 0.6 with the polish, `nat` only
    /// the high-pass.
    pub fn for_mode(mode: LevelMode) -> Self {
        Self::for_mode_in(mode, &ClipDefaults::default())
    }

    /// [`Clean::for_mode`] with the strengths and high-pass of `d` (`project.mix`).
    pub fn for_mode_in(mode: LevelMode, d: &ClipDefaults) -> Self {
        match mode {
            LevelMode::Dialogue => Self {
                amount: d.clean,
                highpass: d.highpass,
                ..Self::default()
            },
            _ => Self {
                amount: if mode == LevelMode::Nat {
                    d.clean_nat
                } else {
                    0.0
                },
                highpass: d.highpass,
                dialogue: Some(false),
                ..Self::default()
            },
        }
    }

    /// The libavfilter chain (without format conversion); empty for a no-op. `mode` decides
    /// the dialogue polish when `dialogue` is unset.
    pub fn filter_spec(&self, mode: LevelMode) -> String {
        let c = self.amount.clamp(0.0, 1.0);
        let mut f: Vec<String> = Vec::new();
        if let Some(hz) = self.highpass {
            f.push(format!("highpass=f={hz}"));
        }
        if self.wind {
            f.push("highpass=f=150:p=2,highpass=f=150:p=2,lowshelf=f=300:g=-6".into());
        }
        if self.declick || self.wind {
            f.push("adeclick".into());
        }
        let afftdn = format!(
            "afftdn=nr={:.1}:nf={:.0}:tn=1",
            6.0 + 10.0 * c,
            -34.0 + 6.0 * c
        );
        if c > 0.0 {
            match (&self.denoise, &self.model) {
                (Denoise::Off, _) => {}
                (Denoise::Arnndn, Some(m)) => {
                    f.push(format!("arnndn=m='{}':mix={c:.2}", m.replace('\'', "\\'")));
                }
                _ => f.push(afftdn),
            }
        }
        if self.dialogue.unwrap_or(mode == LevelMode::Dialogue) {
            if c > 0.0 {
                f.push(format!("equalizer=f=3200:t=q:w=1.1:g={:.2}", 1.8 * c));
                f.push(format!("deesser=i={:.2}:m=0.5:f=0.55", 0.35 * c));
            }
            f.push("acompressor=threshold=-24dB:ratio=2.5:attack=8:release=160:makeup=1".into());
        }
        f.join(",")
    }
}

/// Loudness target of `ffcine.audio.level/1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LevelMode {
    /// Dialogue, -18 LUFS (`orig` in `edit.yaml`).
    #[default]
    #[serde(alias = "orig")]
    Dialogue,
    /// Natural sound, -22 LUFS.
    Nat,
    /// Music, -20 LUFS. On a dialogue track (`edit.yaml` `audio: music`): muted unless
    /// `nat_db` lifts it to -22 + `nat_db`.
    Music,
    /// -36 LUFS (the prototype's `level_lufs` default for beds).
    Ambience,
    /// SFX: the loudest 100 ms at `ref_dbfs` RMS (-18, about dialogue level), peaks under
    /// -1 dBFS, so `gain_db` 0 is as loud as the talking.
    Sfx,
    /// No R128 levelling, only `gain_db`.
    Off,
}

/// `ffcine.audio.level/1`: per-clip gain, R128 levelling, ducking and fades.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Level {
    /// Defaults to the bus's: dialogue, music, ambience, sfx.
    pub mode: Option<LevelMode>,
    /// Explicit target in LUFS, overriding `mode`.
    pub lufs: Option<f64>,
    /// Extra gain after levelling (may be keyframed).
    pub gain_db: Param,
    /// `sfx` mode reference (dBFS RMS of the loudest 100 ms).
    pub ref_dbfs: Option<f64>,
    /// Music level under a dialogue-track clip in `music` mode (dB from -22 LUFS).
    pub nat_db: Option<f64>,
    /// How far the music dips while this clip plays (dialogue: -14, nat: -7; SFX: 0, for
    /// punchlines like a record scratch).
    pub duck_db: Option<f64>,
    /// Fades in seconds (default: dialogue 0.125 crossfade, ambience 0.8, others 0.01).
    pub fade_in: Option<f64>,
    pub fade_out: Option<f64>,
}

impl Level {
    pub fn mode(&self, bus: Bus) -> LevelMode {
        self.mode.unwrap_or(match bus {
            Bus::Dialogue => LevelMode::Dialogue,
            Bus::Music => LevelMode::Music,
            Bus::Ambience => LevelMode::Ambience,
            Bus::Sfx => LevelMode::Sfx,
        })
    }

    /// Whether the clip is heard at all (`audio: music` shots without `nat_db` aren't).
    pub fn audible(&self, bus: Bus) -> bool {
        !(bus == Bus::Dialogue && self.mode(bus) == LevelMode::Music && self.nat_db.is_none())
    }

    /// Integrated loudness target (LUFS), `None` for no levelling.
    pub fn target(&self, bus: Bus) -> Option<f64> {
        self.target_in(bus, &ClipDefaults::default())
    }

    /// [`Level::target`] with the dialogue and nat targets of `d` (`project.mix`).
    pub fn target_in(&self, bus: Bus, d: &ClipDefaults) -> Option<f64> {
        if let Some(l) = self.lufs {
            return Some(l);
        }
        match self.mode(bus) {
            LevelMode::Dialogue => Some(d.dialogue_lufs),
            LevelMode::Nat => Some(d.nat_lufs + self.nat_db.unwrap_or(0.0)),
            LevelMode::Music if bus == Bus::Dialogue => {
                Some(d.nat_lufs + self.nat_db.unwrap_or(0.0))
            }
            LevelMode::Music => Some(-20.0),
            LevelMode::Ambience => Some(-36.0),
            LevelMode::Sfx | LevelMode::Off => None,
        }
    }

    /// The music duck while this clip plays (dB, <= 0).
    pub fn duck(&self, bus: Bus) -> f64 {
        self.duck_in(bus, &ClipDefaults::default())
    }

    /// [`Level::duck`] with the dips of `d` (`project.mix`).
    pub fn duck_in(&self, bus: Bus, d: &ClipDefaults) -> f64 {
        self.duck_db.unwrap_or(match (bus, self.mode(bus)) {
            (Bus::Dialogue, LevelMode::Dialogue) => d.duck_db,
            (Bus::Dialogue, LevelMode::Nat) => d.nat_duck_db,
            _ => 0.0,
        })
    }
}

/// `ffcine.audio.loop/1`: loop the source range `in`..`out` (s, media time) with `crossfade`
/// equal-power seams to fill the clip (ambience beds). Without `out`, the clip's own length + 1 s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Loop {
    pub r#in: Option<f64>,
    pub out: Option<f64>,
    pub crossfade: f64,
}

impl Default for Loop {
    fn default() -> Self {
        Self {
            r#in: None,
            out: None,
            crossfade: 1.0,
        }
    }
}

/// `ffcine.audio.sync/1`, under `metadata.ffcine.sync` of an audio clip `ffcine sync` wrote:
/// where the recorder file lines up with the camera clip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SyncInfo {
    /// `"ffcine.audio.sync/1"`.
    pub schema: String,
    /// The camera clip's `ffcine.id` (or name).
    pub video: String,
    /// Recorder time (s) of camera media time 0.
    pub offset: f64,
    /// Recorder clock rate error: recorder time = offset + camera time * (1 + `drift_ppm` / 1e6).
    pub drift_ppm: f64,
    /// 0..1: fine correlation x peak margin.
    pub confidence: f64,
}

/// Reads one of our clip effects (`Effect.1` named `name`, parameters under
/// `metadata.ffcine`). `Ok(None)` if absent.
pub fn clip_effect<T: for<'de> Deserialize<'de>>(clip: &Clip, name: &str) -> Result<Option<T>> {
    for e in &clip.effects {
        let Effect::Effect(g) = e else { continue };
        if g.effect_name != name {
            continue;
        }
        let Some(params) = g.ffcine() else {
            return Ok(Some(serde_json::from_value(Value::Object(Map::new()))?));
        };
        if let Some(schema) = params.get("schema").and_then(Value::as_str) {
            if schema != format!("{name}/1") {
                return Err(Error::Invalid {
                    what: "audio effect schema",
                    value: schema.to_owned(),
                });
            }
        }
        return Ok(Some(serde_json::from_value(Value::Object(params.clone()))?));
    }
    Ok(None)
}

/// An `Effect.1` carrying `params` as `ffcine.<name>/1`.
pub fn make_effect<T: Serialize>(label: &str, name: &str, params: &T) -> Result<Effect> {
    let mut e = GenericEffect::new(label, name);
    let Value::Object(mut m) = serde_json::to_value(params)? else {
        return Err(Error::Invalid {
            what: "effect parameters",
            value: name.to_owned(),
        });
    };
    m.insert("schema".into(), format!("{name}/1").into());
    e.metadata
        .insert(ffcine_timeline::FFCINE.into(), Value::Object(m));
    Ok(Effect::Effect(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_reads_resolve_keyframes() {
        let v = serde_json::json!({
            "schema": "ffcine.audio.level/1",
            "gain_db": [
                { "t": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 }, "v": -6.0 },
                { "t": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 48.0 }, "v": 0.0, "interp": "hold" }
            ],
            "fade_in": 0.5
        });
        let l: Level = serde_json::from_value(v).expect("level");
        assert!((l.gain_db.at(1.0) + 3.0).abs() < 1e-9);
        assert!((l.gain_db.at(5.0)).abs() < 1e-9);
        assert_eq!(l.fade_in, Some(0.5));
        assert_eq!(l.target(Bus::Music), Some(-20.0));
        let orig: Level =
            serde_json::from_value(serde_json::json!({"mode": "orig"})).expect("orig");
        assert_eq!(
            (orig.target(Bus::Dialogue), orig.duck(Bus::Dialogue)),
            (Some(-18.0), -14.0)
        );
        let muted: Level =
            serde_json::from_value(serde_json::json!({"mode": "music"})).expect("music");
        assert!(!muted.audible(Bus::Dialogue) && muted.audible(Bus::Music));
    }

    #[test]
    fn clean_spec_matches_prototype() {
        assert_eq!(
            Clean::default().filter_spec(LevelMode::Dialogue),
            "highpass=f=80,afftdn=nr=12.0:nf=-30:tn=1,equalizer=f=3200:t=q:w=1.1:g=1.08,\
             deesser=i=0.21:m=0.5:f=0.55,\
             acompressor=threshold=-24dB:ratio=2.5:attack=8:release=160:makeup=1"
        );
        assert_eq!(
            Clean::for_mode(LevelMode::Nat).filter_spec(LevelMode::Nat),
            "highpass=f=80"
        );
    }
}
