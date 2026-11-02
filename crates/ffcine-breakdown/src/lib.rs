//! `ffcine breakdown`: the shot list of a reference video (a reel, a trailer), for recreating it.
//!
//! One decode at analysis size (NVDEC with its hardware resize when it takes the clip, else the
//! CPU) feeds the scene detector of `ffcine triage` frame by frame, the camera motion estimate
//! ([`motion`]), per-frame sharpness and colour ([`colour`]). From the cuts come the shots; a
//! second decode at keyframe size writes each shot's first, sharpest-middle and last frame. The
//! soundtrack's beats ([`ffcine_audio::beats`]) say whether the cuts land on them ([`rhythm`]),
//! batched faster-whisper (as triage) transcribes the speech, and `tesseract`, when on `PATH`,
//! reads on-screen text off the keyframes. Out come `breakdown.json`, `contact-sheet.jpg`
//! ([`sheet`]) and `shotlist.md` ([`shotlist`]); see `docs/breakdown.md`.

// Frame maths on pixel counts and seconds.
#![allow(
    clippy::many_single_char_names,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

pub mod colour;
#[cfg(feature = "ffmpeg")]
mod decode;
pub mod motion;
pub mod ocr;
pub mod rhythm;
#[cfg(feature = "ffmpeg")]
mod run;
pub mod sheet;
pub mod shotlist;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use colour::Colour;
pub use ffcine_triage::Transcript;
pub use motion::{Motion, MotionKind, MotionParams};
pub use rhythm::Music;
#[cfg(feature = "ffmpeg")]
pub use run::{Options, run};

/// `breakdown.json` format version.
pub const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("decode: {0}")]
    Decode(String),
    #[error("{0}")]
    Invalid(String),
}

impl From<ffcine_triage::Error> for Error {
    fn from(e: ffcine_triage::Error) -> Self {
        match e {
            ffcine_triage::Error::Io(e) => Self::Io(e),
            ffcine_triage::Error::Json(e) => Self::Json(e),
            ffcine_triage::Error::Decode(m) => Self::Decode(m),
            e => Self::Invalid(e.to_string()),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A shot's stills, relative to the output directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keyframes {
    /// The representative frame: the sharpest of the middle of the shot.
    pub key: String,
    /// Its time, seconds.
    pub key_time: f64,
    pub first: String,
    pub last: String,
}

/// One shot, between two cuts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shot {
    /// 1-based.
    pub n: usize,
    /// In and out, seconds (out is the next shot's in).
    pub start: f64,
    pub end: f64,
    pub duration: f64,
    /// First frame and one past the last, counted from 0.
    pub frames: [usize; 2],
    pub keyframes: Keyframes,
    pub motion: Motion,
    pub colour: Colour,
    /// Whether the cut into this shot is on a beat (none for the first shot or without music).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cut_on_beat: Option<bool>,
    /// Nearest beat minus the cut, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beat_offset: Option<f64>,
    /// Length in beats.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beats: Option<f64>,
    /// Words spoken in the shot.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub speech: String,
    /// On-screen text read off the keyframe.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub text: String,
}

/// Shot lengths over the whole video.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pacing {
    pub shots: usize,
    /// Seconds.
    pub mean: f64,
    pub median: f64,
    pub shortest: f64,
    pub longest: f64,
    pub cuts_per_minute: f64,
}

/// `breakdown.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breakdown {
    pub version: u32,
    pub source: PathBuf,
    /// Seconds.
    pub duration: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub frames: usize,
    /// `nvdec` or `cpu`.
    pub decoder: String,
    pub pacing: Pacing,
    /// Cut times, seconds.
    pub cuts: Vec<f64>,
    pub shots: Vec<Shot>,
    /// None without audio or with `--no-beats`.
    pub music: Option<Music>,
    /// None without speech or with `--no-transcribe`.
    pub transcript: Option<Transcript>,
    /// Paths relative to the output directory.
    pub contact_sheet: Option<String>,
    pub shotlist: String,
}

impl Breakdown {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }
}

/// A shot's frame range and its chosen stills, before the analysis is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// First frame and one past the last.
    pub a: usize,
    pub b: usize,
    pub first: usize,
    pub key: usize,
    pub last: usize,
}

/// Splits `n` frames at `cuts` (frame indices where a shot starts, ascending) and picks each
/// shot's stills: one frame in from each end (so a soft cut doesn't leak in), and the sharpest
/// frame of the middle 60 %, favouring the centre.
pub fn spans(n: usize, cuts: &[usize], sharpness: &[f32]) -> Vec<Span> {
    let mut starts = vec![0];
    starts.extend(cuts.iter().copied().filter(|&c| c > 0 && c < n));
    starts.dedup();
    let mut out = Vec::with_capacity(starts.len());
    for (i, &a) in starts.iter().enumerate() {
        let b = starts.get(i + 1).copied().unwrap_or(n);
        if b <= a {
            continue;
        }
        let len = b - a;
        let inset = usize::from(len > 4);
        let (lo, hi) = (a + len / 5, (a + len * 4 / 5).max(a + len / 5 + 1).min(b));
        let key = (lo..hi)
            .max_by(|&x, &y| {
                let score = |f: usize| {
                    let rel = (f - a) as f32 / len as f32;
                    sharpness.get(f).copied().unwrap_or(0.0) * (1.0 - (rel - 0.5).abs())
                };
                score(x).total_cmp(&score(y))
            })
            .unwrap_or(a + len / 2);
        out.push(Span {
            a,
            b,
            first: a + inset,
            key,
            last: b - 1 - inset,
        });
    }
    out
}

/// Shot-length statistics.
pub fn pacing(shots: &[Shot], duration: f64) -> Pacing {
    let mut d: Vec<f64> = shots.iter().map(|s| s.duration).collect();
    d.sort_by(f64::total_cmp);
    let r2 = |v: f64| (v * 100.0).round() / 100.0;
    let n = d.len();
    Pacing {
        shots: n,
        mean: r2(if n == 0 {
            0.0
        } else {
            d.iter().sum::<f64>() / n as f64
        }),
        median: r2(d.get(n / 2).copied().unwrap_or(0.0)),
        shortest: r2(d.first().copied().unwrap_or(0.0)),
        longest: r2(d.last().copied().unwrap_or(0.0)),
        cuts_per_minute: r2(if duration > 0.0 {
            n.saturating_sub(1) as f64 * 60.0 / duration
        } else {
            0.0
        }),
    }
}

/// The words of `t` whose middle falls in `[start, end)`.
pub fn words_in(t: &Transcript, start: f64, end: f64) -> String {
    let mut s = String::new();
    for w in t.words() {
        let mid = f64::midpoint(w.start, w.end);
        if mid >= start && mid < end {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(w.text.trim());
        }
    }
    if s.is_empty() {
        // No word times: whole segments.
        for seg in &t.segments {
            let mid = f64::midpoint(seg.start, seg.end);
            if seg.words.is_empty() && mid >= start && mid < end {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(seg.text.trim());
            }
        }
    }
    s
}

/// `m:ss.cc`.
pub fn clock(t: f64) -> String {
    let cs = (t.max(0.0) * 100.0).round() as u64;
    format!("{}:{:02}.{:02}", cs / 6000, cs / 100 % 60, cs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_cuts() {
        let mut sharp = vec![1.0; 30];
        sharp[14] = 9.0;
        let s = spans(30, &[0, 10, 20, 20], &sharp);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].a, s[0].b, s[0].first, s[0].last), (0, 10, 1, 8));
        assert_eq!(s[1].key, 14);
        assert_eq!(s[2].b, 30);
        let one = spans(3, &[], &[]);
        assert_eq!((one[0].first, one[0].key, one[0].last), (0, 1, 2));
        let tiny = spans(1, &[], &[]);
        assert_eq!((tiny[0].first, tiny[0].key, tiny[0].last), (0, 0, 0));
    }

    #[test]
    fn clocks() {
        assert_eq!(clock(0.0), "0:00.00");
        assert_eq!(clock(83.456), "1:23.46");
    }
}
