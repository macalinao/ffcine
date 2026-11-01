//! The triage index: one JSON file (`index.json`) with every clip's analysis. Fields are only ever
//! added; readers ignore unknown ones, so an older index stays readable (`version` bumps when a
//! field changes meaning).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Schema version of [`Index`] and of the per-clip cache files.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub version: u32,
    /// The footage directory that was triaged.
    pub root: PathBuf,
    pub clips: Vec<Clip>,
}

/// A time range in seconds from the start of the clip.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

impl Span {
    pub fn len(self) -> f64 {
        self.end - self.start
    }
    pub fn contains(self, t: f64) -> bool {
        self.start <= t && t < self.end
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    /// File stem of the source (`DJI_20261028120719_0292_D`), unique within an index.
    pub id: String,
    /// The camera original.
    pub source: PathBuf,
    /// The file that was decoded for analysis: the DJI `.LRF` proxy when there is one.
    pub analysed: PathBuf,
    /// Source size and mtime (Unix seconds): the per-clip cache key.
    pub size: u64,
    pub mtime: i64,
    /// Seconds.
    pub duration: f64,
    pub fps: f64,
    /// Source frame size (not the proxy's), when the source could be probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Local start time (`YYYY-MM-DDTHH:MM:SS`, no zone): the ingest catalog's corrected clock,
    /// else the camera clock from the DJI file name (which can be unset or on the wrong zone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<String>,
    /// UTC start from the ingest catalog's corrected clock (`YYYY-MM-DDTHH:MM:SS`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc: Option<String>,
    /// Clips recorded back to back (camera file splits) share a take id: the id of the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<String>,
    /// Contact strip JPEG, relative to the index directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip: Option<String>,
    /// Time of each strip frame, left to right.
    #[serde(default)]
    pub strip_times: Vec<f64>,
    /// Scene cuts (seconds).
    #[serde(default)]
    pub cuts: Vec<f64>,
    /// Picture activity per second, 0 (frozen) to 1, and mean luma per second (0..1).
    #[serde(default)]
    pub activity: Vec<f32>,
    #[serde(default)]
    pub luma: Vec<f32>,
    /// Whether the clip has an audio stream at all.
    #[serde(default)]
    pub has_audio: bool,
    /// Speech map from the energy VAD.
    #[serde(default)]
    pub speech: Vec<Span>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<Transcript>,
    #[serde(default)]
    pub flags: Vec<Flag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
}

impl Clip {
    pub fn speech_seconds(&self) -> f64 {
        self.speech.iter().map(|s| s.len()).sum()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Detected language (`en`, `tl`, `ja`, ...).
    pub language: String,
    /// Model that produced it (`large-v3-turbo`).
    pub model: String,
    pub segments: Vec<Segment>,
}

impl Transcript {
    pub fn words(&self) -> impl Iterator<Item = &Word> {
        self.segments.iter().flat_map(|s| &s.words)
    }
    pub fn text(&self) -> String {
        let mut s = String::new();
        for seg in &self.segments {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(seg.text.trim());
        }
        s
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    #[serde(default)]
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub start: f64,
    pub end: f64,
    /// As whisper wrote it, usually with a leading space.
    pub text: String,
    #[serde(default)]
    pub prob: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flag {
    pub kind: FlagKind,
    /// What and where, for people.
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlagKind {
    /// Left recording: a very long take, usually with a dead tail (`span` = the part to drop).
    Runaway,
    /// Minutes with a near-frozen picture and no speech (pocket, table).
    Dead,
    /// Mostly black (lens cap, pocket).
    Dark,
    /// Under a second and a half: an accidental tap.
    Blip,
    NoAudio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    /// Place name when the source gave one (`Lisbon, Portugal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where it came from (`immich:<asset id>`).
    pub source: String,
}

impl Index {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&s)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }

    pub fn clip(&self, id: &str) -> Option<&Clip> {
        self.clips.iter().find(|c| c.id == id)
    }
}

/// Writes JSON through a temporary file, so a killed run never leaves a torn file.
pub(crate) fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `H:MM:SS.ss` (or `M:SS.ss` under an hour).
pub fn timecode(t: f64) -> String {
    let t = t.max(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = t as u64;
    let frac = t - whole as f64;
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    let mut out = String::new();
    if h > 0 {
        let _ = write!(out, "{h}:{m:02}:");
    } else {
        let _ = write!(out, "{m}:");
    }
    let _ = write!(out, "{:05.2}", s as f64 + frac);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecodes() {
        assert_eq!(timecode(0.0), "0:00.00");
        assert_eq!(timecode(61.5), "1:01.50");
        assert_eq!(timecode(16_200.25), "4:30:00.25");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let j = r#"{"version":1,"root":"/x","clips":[{"id":"a","source":"a.mp4","analysed":"a.lrf",
            "size":1,"mtime":2,"duration":3.0,"fps":24.0,"future":true}]}"#;
        let ix: Index = serde_json::from_str(j).expect("parse");
        assert_eq!(ix.clips[0].id, "a");
        assert_eq!(ix.clips[0].speech, []);
    }
}
