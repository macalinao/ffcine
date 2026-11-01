//! The clip catalog: `DIR/catalog.json`, one entry per media file with its format, picture
//! profile (which camera LUT the grade takes) and corrected clock, plus per-camera summaries,
//! frame-rate mix warnings, clock issues and cross-camera overlaps. Schema `ffcine.catalog/1`;
//! changes stay additive (new optional fields), documented in `docs/ingest.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use ffcine_core::bmff::{Colr, Mp4Info};
use ffcine_core::color::{Matrix, Primaries, Range, Transfer};
use ffcine_core::profile::{self, Confidence, PictureProfile};
use serde::{Deserialize, Serialize};

use crate::camera::{self, Kind, Meta};
use crate::clock::{self, Clock, ClockConfig, ClockIssue, Overlap, Timed};
use crate::stats::{self, FrameStats};
use crate::time::WallTime;
use crate::{Error, Result};

pub const SCHEMA: &str = "ffcine.catalog/1";
pub const FILE: &str = "catalog.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub schema: String,
    /// UTC, when written.
    pub generated: WallTime,
    pub clips: Vec<Clip>,
    pub cameras: BTreeMap<String, CameraSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clock_issues: Vec<ClockIssue>,
    /// Clips from different cameras recorded at the same time (audio sync candidates).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlaps: Vec<Overlap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    /// Relative to the catalog root, `/`-separated.
    pub path: String,
    pub kind: Kind,
    pub camera: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub size: u64,
    /// File mtime, Unix seconds (with `size`, the key for incremental re-cataloguing).
    pub mtime: i64,
    pub day: String,
    pub clock: Clock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<Video>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<Audio>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// The DJI `.LRF` proxy next to this clip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Video {
    /// Sample entry fourcc (`hvc1`, `avc1`, `apch`).
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Exact rate as `NUM/DEN` (`60000/1001` is 59.94, not 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    /// `4:2:0`, `4:2:2`, `4:4:4`, `4:0:0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chroma: Option<String>,
    /// Container colour tags, ffmpeg names (`bt709`, `arib-std-b67`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primaries: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Audio {
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// `rec709`, `dji-dlog`, `dji-dlog-m`, `dji-dlog2`, `hlg`, `pq`, `clog3`, `apple-log`, `unknown`.
    pub name: String,
    /// `high`, `medium`, `low`.
    pub confidence: String,
    pub evidence: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dolby_vision: bool,
    /// `log` (takes the camera LUT), `hdr`, or `display` (the light rule).
    pub source_kind: String,
    /// The camera LUT the grade should use (`dji-pocket4-dlog`, `canon-clog3`, `apple-log`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_lut: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<FrameStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CameraSummary {
    pub clips: usize,
    pub seconds: f64,
    pub bytes: u64,
    /// Clip count per exact frame rate.
    pub fps: BTreeMap<String, usize>,
    /// Clip count per profile.
    pub profiles: BTreeMap<String, usize>,
    pub days: BTreeSet<String>,
}

/// When to run the `ffmpeg` frame-statistics check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Histogram {
    /// Only for clips whose metadata verdict isn't high confidence.
    #[default]
    Auto,
    Always,
    Never,
}

impl std::str::FromStr for Histogram {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(Self::Auto),
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            _ => Err(Error::Config(format!(
                "--histogram {s:?}: want auto, always or never"
            ))),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CatalogOptions {
    pub clocks: ClockConfig,
    pub histogram: Histogram,
    /// Re-probe every file instead of reusing unchanged entries from the existing catalog.
    pub full: bool,
}

fn chroma_name(c: u8) -> String {
    match c {
        0 => "4:0:0",
        1 => "4:2:0",
        2 => "4:2:2",
        _ => "4:4:4",
    }
    .to_owned()
}

#[allow(clippy::many_single_char_names)]
/// H.273 code point to the ffmpeg name, via ffcine-core's colour enums where they cover it.
fn colr_names(c: Colr) -> (String, String, String, Option<String>) {
    let prim = match c.primaries {
        1 => Primaries::Bt709.as_str(),
        9 => Primaries::Bt2020.as_str(),
        12 => Primaries::DisplayP3.as_str(),
        11 => Primaries::DciP3.as_str(),
        6 => Primaries::Bt601.as_str(),
        _ => Primaries::Unknown.as_str(),
    };
    let trc = match c.transfer {
        1 | 6 | 14 | 15 => Transfer::Bt709.as_str(),
        13 => Transfer::Srgb.as_str(),
        16 => Transfer::Pq.as_str(),
        18 => Transfer::Hlg.as_str(),
        8 => Transfer::Linear.as_str(),
        _ => Transfer::Unknown.as_str(),
    };
    let mat = match c.matrix {
        1 => Matrix::Bt709.as_str(),
        9 => Matrix::Bt2020Ncl.as_str(),
        5 | 6 => Matrix::Bt601.as_str(),
        0 => Matrix::Rgb.as_str(),
        _ => Matrix::Unknown.as_str(),
    };
    let range = c.full_range.map(|f| {
        if f { Range::Full } else { Range::Limited }
            .as_str()
            .to_owned()
    });
    (prim.to_owned(), trc.to_owned(), mat.to_owned(), range)
}

#[allow(clippy::many_single_char_names)]
fn video_of(m: &Mp4Info) -> Option<Video> {
    let v = m.video()?;
    let (primaries, transfer, matrix, range) = match v.colr.map(colr_names) {
        Some((p, t, m, r)) => (Some(p), Some(t), Some(m), r),
        None => (None, None, None, None),
    };
    Some(Video {
        codec: v.codec.clone(),
        width: v.width,
        height: v.height,
        fps: v.frame_rate.map(|r| r.to_string()),
        fps_value: v.frame_rate.map(|r| (r.as_f64() * 1000.0).round() / 1000.0),
        frames: (v.samples > 0).then_some(v.samples),
        bit_depth: v.bit_depth,
        chroma: v.chroma.map(chroma_name),
        primaries,
        transfer,
        matrix,
        range,
    })
}

fn audio_of(meta: &Meta) -> Option<Audio> {
    match meta {
        Meta::Mp4(m) => {
            let a = m.audio()?;
            Some(Audio {
                codec: a.codec.clone(),
                sample_rate: a.sample_rate,
                channels: a.channels,
                bits: None,
            })
        }
        Meta::Wav(w) => Some(Audio {
            codec: match w.format {
                1 => "pcm",
                3 => "pcm_float",
                0xfffe => "pcm_extensible",
                _ => "wav",
            }
            .into(),
            sample_rate: Some(w.sample_rate),
            channels: Some(w.channels),
            bits: Some(w.bits),
        }),
        Meta::None => None,
    }
}

fn profile_of(
    path: &Path,
    m: &Mp4Info,
    model: Option<&str>,
    video: Option<&Video>,
    hist: Histogram,
) -> Profile {
    let g = profile::detect(m);
    let mut name = g.profile;
    let mut confidence = g.confidence;
    let mut evidence = g.evidence;
    let run = match hist {
        Histogram::Never => false,
        Histogram::Always => true,
        Histogram::Auto => g.confidence != Confidence::High && !g.profile.is_hdr(),
    };
    let mut fs = None;
    if run {
        let depth = video.and_then(|v| v.bit_depth).unwrap_or(8);
        let at = m.duration.map_or(1.0, |d| (d / 3.0).min(30.0));
        fs = stats::frame_stats(path, at, depth);
        if let Some(s) = fs {
            let log = s.looks_log();
            let _ = write!(
                evidence,
                "; frame stats black {:.3} sat {:.3} ({})",
                s.black,
                s.saturation,
                if log { "log-like" } else { "display-like" }
            );
            if name == PictureProfile::Unknown {
                // Stats alone: name the curve only when the maker is known.
                let dji = m.dji_meta.is_some();
                name = match (log, dji) {
                    (true, true) => PictureProfile::Dlog,
                    (false, _) => PictureProfile::Rec709,
                    _ => PictureProfile::Unknown,
                };
                confidence = Confidence::Low;
            } else if name.is_log() != log {
                evidence.push_str("; disagrees with metadata, check by eye");
                confidence = Confidence::Low;
            }
        }
    }
    let source_kind = if name.is_log() {
        "log"
    } else if name.is_hdr() {
        "hdr"
    } else {
        "display"
    };
    Profile {
        name: name.as_str().to_owned(),
        confidence: confidence.as_str().to_owned(),
        evidence,
        dolby_vision: g.dolby_vision,
        source_kind: source_kind.into(),
        camera_lut: profile::camera_lut(name, model),
        stats: fs,
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Reads `DIR/catalog.json`.
pub fn read(dir: &Path) -> Result<Catalog> {
    let p = dir.join(FILE);
    let text = fs::read_to_string(&p).map_err(|e| Error::io(&p, e))?;
    Ok(serde_json::from_str(&text)?)
}

/// Builds the catalog of every media file under `dir` (only `01_footage/` and `02_audio/` when
/// it is a project root; hidden dirs like `.ffcine` are skipped).
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub fn build(dir: &Path, opts: &CatalogOptions, now: WallTime) -> Result<Catalog> {
    let old: HashMap<String, Clip> = if opts.full {
        HashMap::new()
    } else {
        read(dir)
            .map(|c| c.clips.into_iter().map(|c| (c.path.clone(), c)).collect())
            .unwrap_or_default()
    };
    // In a project root, only the camera media: not renders in 06_exports or music.
    let roots: Vec<PathBuf> = ["01_footage", "02_audio"]
        .iter()
        .map(|d| dir.join(d))
        .filter(|d| d.is_dir())
        .collect();
    let mut files = Vec::new();
    for r in if roots.is_empty() {
        vec![dir.to_owned()]
    } else {
        roots
    } {
        files.extend(
            crate::ingest::walk(&r)?
                .into_iter()
                .filter(|p| p.file_name().is_none_or(|n| n != FILE)),
        );
    }
    let mut clips = Vec::new();
    let mut proxies: HashMap<(String, String), String> = HashMap::new();
    for f in &files {
        let Some(kind) = Kind::of(f) else { continue };
        let md = fs::metadata(f).map_err(|e| Error::io(f, e))?;
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or_default());
        let path = rel(dir, f);
        if kind == Kind::Proxy {
            let parent = path.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
            let stem = f
                .file_stem()
                .map(|s| s.to_string_lossy().to_ascii_uppercase())
                .unwrap_or_default();
            proxies.insert((parent, stem), path);
            continue;
        }
        if kind == Kind::Sidecar {
            continue;
        }
        if let Some(c) = old.get(&path)
            && c.size == md.len()
            && c.mtime == mtime
        {
            let mut c = c.clone();
            // Clock config may have changed: re-derive from what the camera said.
            let o = camera::Origin {
                camera: c.camera.clone(),
                camera_time: c.clock.camera_time,
                camera_zone: c.clock.camera_zone,
                utc: if c.clock.camera_time.is_none() {
                    c.clock.utc.map(|u| WallTime(u.0 - c.clock.shift.secs()))
                } else {
                    None
                },
                model: c.model.clone(),
                time_source: "",
            };
            let src = std::mem::take(&mut c.clock.source);
            c.clock = opts.clocks.apply(&o);
            c.clock.source = src;
            c.day = c.clock.day();
            clips.push(c);
            continue;
        }
        let meta = camera::read_meta(f, kind);
        let origin = camera::origin(f, kind, &meta);
        let clock = opts.clocks.apply(&origin);
        let (video, profile, duration) = match &meta {
            Meta::Mp4(m) if kind == Kind::Video => {
                let v = video_of(m);
                let p = v
                    .as_ref()
                    .map(|_| profile_of(f, m, origin.model.as_deref(), v.as_ref(), opts.histogram));
                (v, p, m.duration)
            }
            Meta::Wav(w) => (None, None, w.duration),
            _ => (None, None, None),
        };
        clips.push(Clip {
            path,
            kind,
            camera: origin.camera.clone(),
            model: origin.model.clone(),
            size: md.len(),
            mtime,
            day: clock.day(),
            clock,
            duration,
            audio: audio_of(&meta),
            video,
            profile,
            proxy: None,
        });
    }
    for c in &mut clips {
        let (parent, file) = c.path.rsplit_once('/').unwrap_or(("", &c.path));
        let stem = file
            .rsplit_once('.')
            .map_or(file, |(s, _)| s)
            .to_ascii_uppercase();
        c.proxy = proxies.get(&(parent.to_owned(), stem)).cloned();
    }
    clips.sort_by(|a, b| (a.clock.instant(), &a.path).cmp(&(b.clock.instant(), &b.path)));

    let mut cameras: BTreeMap<String, CameraSummary> = BTreeMap::new();
    let mut fps_by_day: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for c in &clips {
        let s = cameras.entry(c.camera.clone()).or_default();
        s.clips += 1;
        s.seconds += c.duration.unwrap_or(0.0);
        s.bytes += c.size;
        s.days.insert(c.day.clone());
        if let Some(fps) = c.video.as_ref().and_then(|v| v.fps.clone()) {
            *s.fps.entry(fps.clone()).or_default() += 1;
            fps_by_day
                .entry((c.day.clone(), c.camera.clone()))
                .or_default()
                .insert(fps);
        }
        if let Some(p) = &c.profile {
            *s.profiles.entry(p.name.clone()).or_default() += 1;
        }
    }
    for s in cameras.values_mut() {
        s.seconds = (s.seconds * 1000.0).round() / 1000.0;
    }
    let mut warnings = Vec::new();
    for ((day, cam), rates) in &fps_by_day {
        if rates.len() > 1 {
            warnings.push(format!(
                "{day} {cam}: mixed frame rates {}",
                rates
                    .iter()
                    .map(|r| fps_label(r))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let mut all: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for ((day, _), rates) in &fps_by_day {
        all.entry(day)
            .or_default()
            .extend(rates.iter().map(String::as_str));
    }
    for (day, rates) in all {
        if rates.len() > 1 {
            let integer: BTreeSet<_> = rates.iter().filter(|r| !r.ends_with("/1001")).collect();
            let ntsc: BTreeSet<_> = rates.iter().filter(|r| r.ends_with("/1001")).collect();
            if !integer.is_empty() && !ntsc.is_empty() {
                warnings.push(format!(
                    "{day}: cameras mix NTSC (x/1001) and integer rates: {}; conform before cutting",
                    rates.iter().map(|r| fps_label(r)).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    for c in &clips {
        if let Some(p) = &c.profile
            && p.confidence == Confidence::Low.as_str()
        {
            warnings.push(format!(
                "{}: profile {} is a guess ({})",
                c.path, p.name, p.evidence
            ));
        }
    }

    let timed: Vec<Timed<'_>> = clips
        .iter()
        .filter(|c| matches!(c.kind, Kind::Video | Kind::Audio))
        .map(|c| Timed {
            name: c.path.rsplit('/').next().unwrap_or(&c.path),
            camera: &c.camera,
            clock: &c.clock,
            duration: c.duration,
        })
        .collect();
    let clock_issues = clock::check(&opts.clocks, &timed, now)?;
    let overlaps = clock::overlaps(&timed);
    Ok(Catalog {
        schema: SCHEMA.into(),
        generated: now,
        clips,
        cameras,
        warnings,
        clock_issues,
        overlaps,
    })
}

/// `60000/1001` -> `59.94 (60000/1001)`.
fn fps_label(r: &str) -> String {
    r.parse::<ffcine_core::Rational>().map_or_else(
        |_| r.to_owned(),
        |q| {
            let v = (q.as_f64() * 1000.0).round() / 1000.0;
            if q.den == 1 {
                format!("{v}")
            } else {
                format!("{v} ({r})")
            }
        },
    )
}

/// Writes `DIR/catalog.json` atomically.
pub fn write(dir: &Path, c: &Catalog) -> Result<PathBuf> {
    let p = dir.join(FILE);
    let tmp = dir.join(format!("{FILE}.tmp"));
    let mut text = serde_json::to_string_pretty(c)?;
    text.push('\n');
    fs::write(&tmp, text).map_err(|e| Error::io(&tmp, e))?;
    fs::rename(&tmp, &p).map_err(|e| Error::io(&p, e))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(fps_label("60000/1001"), "59.94 (60000/1001)");
        assert_eq!(fps_label("60/1"), "60");
        assert_eq!(chroma_name(2), "4:2:2");
    }
}
