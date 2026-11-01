//! Which camera a card file came from, what kind of file it is, and what its clock said.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

use ffcine_core::bmff::{self, Mp4Info};
use ffcine_core::wav::{self, WavInfo};
use serde::{Deserialize, Serialize};

use crate::time::{WallTime, Zone};

/// File kinds ingest copies. Anything else on a card is listed and left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Video,
    /// DJI `.LRF` low-res proxy (an H.264 MP4): kept for triage and review renders.
    Proxy,
    /// `.SRT` telemetry, Canon `.THM`, XML sidecars.
    Sidecar,
    Still,
    Audio,
}

impl Kind {
    /// From the extension; `None` for files ingest doesn't copy.
    pub fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "mp4" | "mov" | "mxf" | "m4v" => Self::Video,
            "lrf" => Self::Proxy,
            "srt" | "thm" | "xml" | "lrv" => Self::Sidecar,
            "jpg" | "jpeg" | "heic" | "heif" | "cr3" | "cr2" | "dng" | "png" => Self::Still,
            "wav" | "bwf" | "m4a" => Self::Audio,
            _ => return None,
        })
    }
}

/// What the file says about its origin and clock.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Origin {
    /// Short camera id (`pocket`, `r10`, `r50`, `iphone`, `h2e`, ...): the folder name and the
    /// key in the clock config.
    pub camera: String,
    pub model: Option<String>,
    /// The camera's own wall clock at the start of the recording.
    pub camera_time: Option<WallTime>,
    /// The zone that clock was in, when the file records it (iPhone; DJI via UTC `mvhd`).
    pub camera_zone: Option<Zone>,
    /// A UTC instant, when the file has one but no local time (iPhone without the Apple key,
    /// file mtimes).
    pub utc: Option<WallTime>,
    /// Where the time came from: `dji-filename`, `mvhd-local`, `apple-creationdate`, `bext`,
    /// `exif`, `mtime`.
    pub time_source: &'static str,
}

/// Parsed container metadata, for files that have it.
#[derive(Debug, Clone, PartialEq)]
pub enum Meta {
    Mp4(Box<Mp4Info>),
    Wav(WavInfo),
    None,
}

/// Reads a file's metadata (cheap: headers only).
pub fn read_meta(path: &Path, kind: Kind) -> Meta {
    let ext = ext(path);
    match kind {
        Kind::Video | Kind::Proxy => {
            bmff::read(path).map_or(Meta::None, |m| Meta::Mp4(Box::new(m)))
        }
        Kind::Still if ext == "cr3" => {
            bmff::read(path).map_or(Meta::None, |m| Meta::Mp4(Box::new(m)))
        }
        Kind::Audio if ext == "wav" || ext == "bwf" => {
            wav::read(path).map_or(Meta::None, Meta::Wav)
        }
        _ => Meta::None,
    }
}

fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Camera id for a model string.
pub fn camera_id(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase().replace([' ', '-', '_'], "");
    Some(if m.contains("pocket3") || m.contains("op03") {
        "pocket3"
    } else if m.contains("osmopocket") || m.contains("op04") {
        "pocket"
    } else if m.contains("osmoaction") {
        "action"
    } else if m.contains("eosr10") {
        "r10"
    } else if m.contains("eosr50") {
        "r50"
    } else if m.starts_with("canon") {
        "canon"
    } else if m.contains("iphone") {
        "iphone"
    } else if m.contains("h2essential") || m.contains("h2e") {
        "h2e"
    } else if m.contains("zoom") {
        "zoom"
    } else if m.starts_with("dji") || m.starts_with("fc") {
        "dji"
    } else {
        return None;
    })
}

/// Camera id from path conventions, when no model is recorded.
fn camera_from_path(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_uppercase();
    let dirs = path.to_string_lossy().to_ascii_uppercase();
    if name.starts_with("DJI_") {
        "dji"
    } else if dirs.contains("CANON") || name.starts_with("MVI_") {
        "canon"
    } else if name.starts_with("ZOOM") {
        "zoom"
    } else if dirs.contains("APPLE") || ext(path) == "heic" {
        "iphone"
    } else if ext(path) == "wav" {
        "audio"
    } else {
        "other"
    }
}

/// `DJI_YYYYMMDDhhmmss_NNNN_D.MP4`.
fn dji_filename_time(path: &Path) -> Option<WallTime> {
    let name = path.file_name()?.to_str()?;
    let stamp = name.strip_prefix("DJI_")?;
    WallTime::parse(stamp.get(..14)?).map(|(t, _)| t)
}

/// The JPEG EXIF IFD0 `Model` and `DateTime`.
fn jpeg_exif(path: &Path) -> (Option<String>, Option<WallTime>) {
    let mut buf = vec![0u8; 256 * 1024];
    let n = fs::File::open(path)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap_or(0);
    buf.truncate(n);
    let Some(i) = buf.windows(6).position(|w| w == b"Exif\0\0") else {
        return (None, None);
    };
    let tiff = &buf[i + 6..];
    let model = bmff::tiff_ascii(tiff, 0x0110);
    let time = bmff::tiff_ascii(tiff, 0x0132)
        .and_then(|s| WallTime::parse(&s))
        .map(|(t, _)| t);
    (model, time)
}

fn mtime(path: &Path) -> Option<WallTime> {
    let m = fs::metadata(path).ok()?.modified().ok()?;
    let s = m.duration_since(UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(s).ok().map(WallTime)
}

/// Rounds a zone guess to the nearest 15 minutes; `None` if outside ±14 h.
fn zone_from_diff(secs: i64) -> Option<Zone> {
    let q = (secs + 450).div_euclid(900) * 900;
    (q.abs() <= 14 * 3600).then(|| Zone(i32::try_from(q).unwrap_or_default()))
}

/// Identifies a card file's camera and clock.
#[allow(clippy::too_many_lines)]
pub fn origin(path: &Path, kind: Kind, meta: &Meta) -> Origin {
    let mut o = Origin::default();
    match meta {
        Meta::Mp4(m) => {
            let model = m.model().map(str::to_owned);
            if let Some(h) = &m.dji_meta
                && let Some((dji_model, ..)) = ffcine_core::profile::dji_header(h)
                && !dji_model.is_empty()
                && model.is_none()
            {
                o.model = Some(dji_model);
            } else {
                o.model = model;
            }
            if let Some(t) = dji_filename_time(path) {
                o.camera_time = Some(t);
                o.time_source = "dji-filename";
                // DJI's mvhd is UTC: the difference is the zone the camera clock was set to.
                if let Some(u) = m.creation_time {
                    o.camera_zone = zone_from_diff(t.0 - u);
                }
            } else if let Some(s) = m.keys.get("com.apple.quicktime.creationdate")
                && let Some((t, z)) = WallTime::parse(s)
            {
                o.camera_time = Some(t);
                o.camera_zone = z;
                o.time_source = "apple-creationdate";
            } else if o.model.as_deref().is_some_and(|x| x.contains("iPhone"))
                && let Some(u) = m.creation_time
            {
                o.utc = Some(WallTime(u));
                o.time_source = "mvhd-utc";
            } else if let Some(s) = m.canon.get("DateTime")
                && let Some((t, _)) = WallTime::parse(s)
            {
                o.camera_time = Some(t);
                o.time_source = "exif";
            } else if let Some(u) = m.creation_time {
                // Canon and most cameras without a zone setting write local time here.
                o.camera_time = Some(WallTime(u));
                o.time_source = "mvhd-local";
            }
        }
        Meta::Wav(w) => {
            o.model.clone_from(&w.originator);
            if let Some((d, t)) = &w.origination
                && let Some((wt, _)) = WallTime::parse(&format!("{d}T{t}"))
            {
                o.camera_time = Some(wt);
                o.time_source = "bext";
            }
        }
        Meta::None => {
            if kind == Kind::Still {
                let (model, time) = jpeg_exif(path);
                o.model = model;
                if let Some(t) = time {
                    o.camera_time = Some(t);
                    o.time_source = "exif";
                }
            }
            if o.camera_time.is_none()
                && let Some(t) = dji_filename_time(path)
            {
                o.camera_time = Some(t);
                o.time_source = "dji-filename";
            }
        }
    }
    if o.camera_time.is_none() && o.utc.is_none() {
        o.utc = mtime(path);
        o.time_source = "mtime";
    }
    o.camera = o
        .model
        .as_deref()
        .and_then(camera_id)
        .unwrap_or_else(|| camera_from_path(path))
        .to_owned();
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_ids() {
        assert_eq!(Kind::of(Path::new("a/DJI_1_D.LRF")), Some(Kind::Proxy));
        assert_eq!(Kind::of(Path::new("MVI_0001.MP4")), Some(Kind::Video));
        assert_eq!(Kind::of(Path::new("IMG_0001.CR3")), Some(Kind::Still));
        assert_eq!(Kind::of(Path::new(".DS_Store")), None);
        assert_eq!(camera_id("DJI OsmoPocket4"), Some("pocket"));
        assert_eq!(camera_id("Canon EOS R10"), Some("r10"));
        assert_eq!(camera_id("Canon EOS R50"), Some("r50"));
        assert_eq!(camera_id("iPhone 16 Pro"), Some("iphone"));
        assert_eq!(camera_id("ZOOM H2essential"), Some("h2e"));
        assert_eq!(
            camera_from_path(Path::new("DCIM/100CANON/MVI_0001.MP4")),
            "canon"
        );
    }

    #[test]
    fn zone_guess() {
        assert_eq!(zone_from_diff(8 * 3600 + 3), Some(Zone(28_800)));
        assert_eq!(zone_from_diff(-20 * 3600), None);
    }
}
