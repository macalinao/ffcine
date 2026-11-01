//! Picture profile detection: which camera curve a clip was shot in (DJI D-Log, D-Log M,
//! D-Log2, HLG, PQ, Canon Log, Apple Log, or plain Rec.709), from container metadata. The grade picks the camera
//! LUT from this, so each answer carries a [`Confidence`] and the evidence it came from.
//!
//! Why not just the colour tags: DJI writes `bt709/bt709/bt709` in `colr` for D-Log and
//! Normal alike, so the tags only settle HDR transfers (HLG, PQ). For DJI the colour mode lives
//! in the `CAM meta` (`djmd`) track's protobuf clip header; see [`dji_color_mode`].

#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::cast_possible_truncation,
    clippy::doc_markdown
)] // binary parsing; box sizes are small; QuickTime, BMFF are names

use std::fmt;
use std::str::FromStr;

use crate::Error;
use crate::bmff::{Mp4Info, Track};

crate::named_enum!(
    /// The tone curve / gamut a clip was recorded in.
    PictureProfile {
        /// Display-referred SDR (DJI "Normal", Canon standard picture styles, iPhone SDR).
        Rec709 = "rec709",
        /// DJI D-Log with D-Gamut, 10-bit (Osmo Pocket 4).
        Dlog = "dji-dlog",
        /// DJI D-Log M (Osmo Pocket 3, Action, Mini drones).
        DlogM = "dji-dlog-m",
        /// DJI D-Log2 (Osmo Pocket 4P).
        Dlog2 = "dji-dlog2",
        /// ARIB STD-B67 hybrid log-gamma, BT.2020 (DJI HLG, iPhone HDR / Dolby Vision 8.4).
        Hlg = "hlg",
        /// SMPTE ST 2084, BT.2020 (Canon HDR PQ).
        Pq = "pq",
        /// Canon Log 3 (needs a 10-bit body; the R10 and R50 don't have it).
        CLog3 = "clog3",
        /// Apple Log (iPhone 15 Pro and later, ProRes or HEVC).
        AppleLog = "apple-log",
        Unknown = "unknown",
    }
);

crate::named_enum!(
    /// How sure the detection is.
    Confidence {
        /// Explicit metadata: an HDR transfer tag or the maker's own colour-mode field.
        High = "high",
        /// Inferred from codec, bit depth and model rules (e.g. Canon 10-bit without PQ).
        Medium = "medium",
        /// A guess: image statistics or no evidence at all. Check before grading.
        Low = "low",
    }
);

impl PictureProfile {
    /// Log footage takes the camera LUT; the rest takes the display-referred ("light") rule.
    pub const fn is_log(self) -> bool {
        matches!(
            self,
            Self::Dlog | Self::DlogM | Self::Dlog2 | Self::CLog3 | Self::AppleLog
        )
    }

    /// HDR transfer (passthrough or tone-map to SDR).
    pub const fn is_hdr(self) -> bool {
        matches!(self, Self::Hlg | Self::Pq)
    }
}

/// A detection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileGuess {
    pub profile: PictureProfile,
    pub confidence: Confidence,
    /// Dolby Vision configuration present (iPhone DV is profile 8.4 on an HLG base).
    pub dolby_vision: bool,
    /// What it was decided from, for the catalog (`dji cam-meta color mode 2`, `colr transfer 18`).
    pub evidence: String,
}

impl ProfileGuess {
    fn new(profile: PictureProfile, confidence: Confidence, evidence: impl Into<String>) -> Self {
        Self {
            profile,
            confidence,
            dolby_vision: false,
            evidence: evidence.into(),
        }
    }
}

/// ITU-T H.273 transfer characteristics.
const TRC_PQ: u16 = 16;
const TRC_HLG: u16 = 18;
const PRIM_BT2020: u16 = 9;

/// Detects the picture profile from container metadata alone.
pub fn detect(info: &Mp4Info) -> ProfileGuess {
    let Some(v) = info.video() else {
        return ProfileGuess::new(PictureProfile::Unknown, Confidence::Low, "no video track");
    };
    let mut g = detect_video(info, v);
    g.dolby_vision = v.dolby_vision;
    g
}

fn detect_video(info: &Mp4Info, v: &Track) -> ProfileGuess {
    use Confidence::{High, Low, Medium};
    use PictureProfile as P;
    if let Some(c) = v.colr {
        match c.transfer {
            TRC_HLG => return ProfileGuess::new(P::Hlg, High, "colr transfer 18 (arib-std-b67)"),
            TRC_PQ => return ProfileGuess::new(P::Pq, High, "colr transfer 16 (smpte2084)"),
            _ => {}
        }
    }
    if let Some(meta) = &info.dji_meta {
        return match dji_color_mode(meta) {
            Some(2) => dji_log(info, meta),
            Some(1) => {
                ProfileGuess::new(P::Hlg, Medium, "dji cam-meta color mode 1 (HLG, untagged)")
            }
            None | Some(0) => {
                ProfileGuess::new(P::Rec709, Medium, "dji cam-meta: no color mode (Normal)")
            }
            Some(n) => ProfileGuess::new(
                P::Unknown,
                Low,
                format!("dji cam-meta color mode {n} (unmapped)"),
            ),
        };
    }
    let model = info.model().unwrap_or_default();
    let depth = v.bit_depth.unwrap_or(8);
    if model.contains("iPhone") || info.keys.keys().any(|k| k.starts_with("com.apple.")) {
        if info
            .keys
            .iter()
            .any(|(k, val)| has_apple_log(k) || has_apple_log(val))
        {
            return ProfileGuess::new(P::AppleLog, High, "apple metadata names Apple Log");
        }
        if let Some(c) = v.colr
            && c.primaries == PRIM_BT2020
            && !matches!(c.transfer, 1 | 6 | 14 | 15)
        {
            return ProfileGuess::new(
                P::AppleLog,
                Medium,
                format!("iphone bt2020 with non-standard transfer {}", c.transfer),
            );
        }
        return ProfileGuess::new(P::Rec709, Medium, "iphone SDR tags");
    }
    if model.starts_with("Canon") || info.udta.get("manu").is_some_and(|m| m == "Canon") {
        if depth >= 10 {
            return ProfileGuess::new(P::CLog3, Medium, "canon 10-bit without an HDR tag");
        }
        return ProfileGuess::new(P::Rec709, High, "canon 8-bit (log needs 10-bit)");
    }
    match v.colr {
        Some(c) if depth == 8 && c.transfer == 1 => {
            ProfileGuess::new(P::Rec709, Medium, "8-bit, colr bt709")
        }
        _ if depth == 8 => ProfileGuess::new(P::Rec709, Low, "8-bit, no profile metadata"),
        _ => ProfileGuess::new(P::Unknown, Low, format!("{depth}-bit, no profile metadata")),
    }
}

/// DJI colour mode 2 is the body's log curve, which depends on the model.
fn dji_log(info: &Mp4Info, meta: &[u8]) -> ProfileGuess {
    let model = dji_header(meta)
        .map(|(m, ..)| m)
        .filter(|m| !m.is_empty())
        .or_else(|| info.model().map(str::to_owned))
        .unwrap_or_default();
    let m = model.to_ascii_lowercase().replace([' ', '-', '_'], "");
    let (p, c, what) = if m.contains("pocket4p") || m.contains("pocket4pro") {
        (PictureProfile::Dlog2, Confidence::Medium, "D-Log2")
    } else if m.contains("pocket4") {
        (PictureProfile::Dlog, Confidence::High, "D-Log")
    } else if m.contains("pocket3") || m.contains("mini4") || m.contains("osmoaction") {
        (PictureProfile::DlogM, Confidence::Medium, "D-Log M")
    } else {
        (
            PictureProfile::Dlog,
            Confidence::Medium,
            "DJI log, model not mapped",
        )
    };
    ProfileGuess::new(
        p,
        c,
        format!("dji cam-meta color mode 2 on {model} ({what})"),
    )
}

/// The per-camera LUT key the grade looks up: log curves are per body.
pub fn camera_lut(profile: PictureProfile, model: Option<&str>) -> Option<String> {
    let m = model
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace([' ', '-', '_'], "");
    Some(match profile {
        PictureProfile::Dlog if m.contains("pocket4") => "dji-pocket4-dlog".into(),
        PictureProfile::Dlog => "dji-dlog".into(),
        PictureProfile::DlogM if m.contains("pocket3") => "dji-pocket3-dlog-m".into(),
        PictureProfile::DlogM => "dji-dlog-m".into(),
        PictureProfile::Dlog2 => "dji-pocket4p-dlog2".into(),
        PictureProfile::CLog3 => "canon-clog3".into(),
        PictureProfile::AppleLog => "apple-log".into(),
        _ => return None,
    })
}

fn has_apple_log(s: &str) -> bool {
    let s = s.to_ascii_lowercase().replace([' ', '-', '_', '.'], "");
    s.contains("applelog")
}

/// DJI's colour mode from a `CAM meta` clip header (protobuf `dvtm_*.proto`, first `djmd`
/// sample): field 2 (stream) -> 2 (device) -> 3 (picture settings) -> 1. Measured on Osmo
/// Pocket 4 footage (2026): `2` on every D-Log clip (lifted blacks, ~5-15% of normal
/// saturation), absent on Normal clips. `1` is assumed HLG (not yet seen untagged).
pub fn dji_color_mode(header: &[u8]) -> Option<u64> {
    let stream = pb_field(header, 2)?;
    let device = pb_field(stream, 2)?;
    let pic = pb_field(device, 3)?;
    pb_varint_field(pic, 1)
}

/// DJI clip header fields, for the catalog: (model, fps, bit depth) from field 1.1.10 and 2.3.
pub fn dji_header(header: &[u8]) -> Option<(String, Option<f32>, Option<u64>)> {
    let model = pb_field(header, 1)
        .and_then(|h| pb_field(h, 1))
        .and_then(|h| pb_field(h, 10))
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let video = pb_field(header, 2).and_then(|s| pb_field(s, 3));
    let fps = video
        .and_then(|v| pb_fixed32_field(v, 3))
        .map(f32::from_bits);
    let depth = video.and_then(|v| pb_varint_field(v, 5));
    Some((model, fps, depth))
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut r = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b.get(*i)?;
        *i += 1;
        r |= u64::from(c & 0x7f) << shift;
        if c < 0x80 {
            return Some(r);
        }
    }
    None
}

enum Wire<'a> {
    Varint(u64),
    Fixed32(u32),
    Bytes(&'a [u8]),
    Other,
}

/// Walks top-level protobuf fields; `None` on malformed input.
fn pb_fields(b: &[u8]) -> Option<Vec<(u64, Wire<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let key = varint(b, &mut i)?;
        let w = match key & 7 {
            0 => Wire::Varint(varint(b, &mut i)?),
            1 => {
                i += 8;
                Wire::Other
            }
            2 => {
                let n = usize::try_from(varint(b, &mut i)?).ok()?;
                let s = b.get(i..i.checked_add(n)?)?;
                i += n;
                Wire::Bytes(s)
            }
            5 => {
                let s = b.get(i..i + 4)?;
                i += 4;
                Wire::Fixed32(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            }
            _ => return None,
        };
        if i > b.len() {
            return None;
        }
        out.push((key >> 3, w));
    }
    Some(out)
}

fn pb_field(b: &[u8], n: u64) -> Option<&[u8]> {
    pb_fields(b)?.into_iter().find_map(|(k, w)| match w {
        Wire::Bytes(s) if k == n => Some(s),
        _ => None,
    })
}

fn pb_varint_field(b: &[u8], n: u64) -> Option<u64> {
    pb_fields(b)?.into_iter().find_map(|(k, w)| match w {
        Wire::Varint(v) if k == n => Some(v),
        _ => None,
    })
}

fn pb_fixed32_field(b: &[u8], n: u64) -> Option<u32> {
    pb_fields(b)?.into_iter().find_map(|(k, w)| match w {
        Wire::Fixed32(v) if k == n => Some(v),
        _ => None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bmff::{Colr, Track};

    fn len_field(n: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![(n << 3) | 2, u8::try_from(body.len()).expect("short")];
        v.extend_from_slice(body);
        v
    }

    /// A DJI clip header with `2.2.3.1 = mode` (or no 2.2.3 content when `None`), shaped like
    /// the Osmo Pocket 4's.
    pub fn dji_header_bytes(mode: Option<u8>) -> Vec<u8> {
        let model = len_field(1, &len_field(10, b"DJI OsmoPocket4"));
        let pic = mode.map(|m| vec![0x08, m]).unwrap_or_default();
        let device = [len_field(4, b"DJI OP-041"), len_field(3, &pic)].concat();
        let mut video = vec![0x08, 0x80, 0x1e, 0x10, 0x90, 0x10, 0x1d];
        video.extend_from_slice(&59.94f32.to_le_bytes());
        video.extend_from_slice(&[0x28, 10]);
        let stream = [len_field(2, &device), len_field(3, &video)].concat();
        [len_field(1, &model), len_field(2, &stream)].concat()
    }

    fn info(track: Track) -> Mp4Info {
        Mp4Info {
            tracks: vec![track],
            ..Mp4Info::default()
        }
    }

    fn video(depth: u8, transfer: u16, primaries: u16) -> Track {
        Track {
            handler: "vide".into(),
            codec: "hvc1".into(),
            bit_depth: Some(depth),
            colr: Some(Colr {
                primaries,
                transfer,
                matrix: 1,
                full_range: Some(false),
            }),
            ..Track::default()
        }
    }

    #[test]
    fn dji_mode_field() {
        assert_eq!(dji_color_mode(&dji_header_bytes(Some(2))), Some(2));
        assert_eq!(dji_color_mode(&dji_header_bytes(None)), None);
        let (model, fps, depth) = dji_header(&dji_header_bytes(None)).unwrap();
        assert_eq!(model, "DJI OsmoPocket4");
        assert!((fps.unwrap() - 59.94).abs() < 1e-3);
        assert_eq!(depth, Some(10));
        assert_eq!(dji_color_mode(b"\xff\xff"), None);
    }

    #[test]
    fn dji_log_is_tagged_bt709_but_detected() {
        let mut i = info(video(10, 1, 1));
        i.dji_meta = Some(dji_header_bytes(Some(2)));
        let g = detect(&i);
        assert_eq!(
            (g.profile, g.confidence),
            (PictureProfile::Dlog, Confidence::High)
        );
        assert_eq!(
            camera_lut(g.profile, Some("DJI OsmoPocket4")).as_deref(),
            Some("dji-pocket4-dlog")
        );
        i.dji_meta = Some(dji_header_bytes(None));
        assert_eq!(detect(&i).profile, PictureProfile::Rec709);
    }

    #[test]
    fn hdr_tags_win() {
        let mut i = info(video(10, TRC_HLG, 9));
        i.dji_meta = Some(dji_header_bytes(Some(2)));
        assert_eq!(detect(&i).profile, PictureProfile::Hlg);
        let mut i = info(video(10, TRC_PQ, 9));
        i.udta.insert("modl".into(), "Canon EOS R10".into());
        assert_eq!(detect(&i).profile, PictureProfile::Pq);
    }

    #[test]
    fn canon_and_iphone_rules() {
        let mut i = info(video(8, 1, 1));
        i.udta.insert("modl".into(), "Canon EOS R10".into());
        assert_eq!(detect(&i).profile, PictureProfile::Rec709);
        i.tracks[0].bit_depth = Some(10);
        assert_eq!(detect(&i).profile, PictureProfile::CLog3);

        let mut i = info(video(10, 2, 9));
        i.keys
            .insert("com.apple.quicktime.model".into(), "iPhone 16 Pro".into());
        assert_eq!(detect(&i).profile, PictureProfile::AppleLog);
        i.tracks[0].dolby_vision = true;
        i.tracks[0].colr = Some(Colr {
            primaries: 9,
            transfer: TRC_HLG,
            matrix: 9,
            full_range: None,
        });
        let g = detect(&i);
        assert_eq!(g.profile, PictureProfile::Hlg);
        assert!(g.dolby_vision);
    }
}
