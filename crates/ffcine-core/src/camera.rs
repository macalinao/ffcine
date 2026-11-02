//! Camera input profiles: which colour-in a source needs, picked from the probe, and the camera
//! LUTs (log -> Rec.709) that go with them.
//!
//! | profile | detected from | colour-in |
//! |---|---|---|
//! | `hdr` | HLG/PQ transfer (iPhone HLG + Dolby Vision 8.4, Canon HDR PQ), or BT.2020/P3 primaries with a bt709 transfer | libplacebo tone + gamut mapping from the tags, no LUT |
//! | `applelog` | BT.2020 primaries, untagged transfer (iPhone `ProRes` Log) | analytic Apple Log -> Rec.709 |
//! | `clog3` | the same on a Canon file (`CAEP` brand); else only by override | analytic Canon Log 3 / Cinema Gamut -> Rec.709 |
//! | `dlog`, `dlog-vivid` | 10-bit, SDR-tagged, DJI encoder tag (Osmo Pocket 4) | DJI's official Pocket 4 D-Log -> Rec.709 LUT (vendor file, [`VENDOR_LUTS`]); `dlog` falls back to the analytic D-Log / D-Gamut transform |
//! | `dlogm`, `dlogm-vivid` | the same from an Osmo Pocket 3 | DJI's D-Log M -> Rec.709 LUT (vendor file only: no published curve) |
//! | `rec709` | anything else | honour the tags |
//!
//! DJI files are tagged bt709 whether they're log or "normal" 10-bit; the colour mode is in the
//! `djmd` CAM meta protobuf, which [`crate::profile`] reads ([`Profile::detect_file`]). Without
//! it (not ISO BMFF, or no CAM meta) a 10-bit DJI clip is taken as log (how DJI log footage is usually shot).

// Colour maths: matrix entries and chromaticities read best as single letters.
#![allow(clippy::many_single_char_names)]

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::color::{Primaries, Transfer, named_enum};
use crate::lut::Lut;
use crate::probe::MediaInfo;
use crate::profile::{Confidence, PictureProfile};
use crate::{Error, Result};

named_enum!(
    /// A source's camera colour profile.
    Profile {
        /// Display-referred SDR: honour the tags.
        Rec709 = "rec709",
        /// HLG / PQ / Dolby Vision or wide-gamut SDR: libplacebo tone and gamut mapping.
        Hdr = "hdr",
        /// DJI D-Log / D-Gamut (Osmo Pocket 4) through DJI's neutral Rec.709 LUT, else analytic.
        DjiDlog = "dlog",
        /// DJI D-Log through DJI's vivid Rec.709 LUT.
        DjiDlogVivid = "dlog-vivid",
        /// DJI D-Log M (Osmo Pocket 3) through DJI's neutral Rec.709 LUT.
        DjiDlogM = "dlogm",
        /// DJI D-Log M through DJI's vivid Rec.709 LUT.
        DjiDlogMVivid = "dlogm-vivid",
        /// Canon Log 3 (Cinema Gamut).
        CanonClog3 = "clog3",
        /// Apple Log (BT.2020).
        AppleLog = "applelog",
    }
);

/// Where vendor LUT files are looked up, in order: `$FFCINE_LUT_DIR` (`:`-separated), then
/// `$XDG_DATA_HOME/ffcine/luts` (or `~/.local/share/ffcine/luts`).
pub fn lut_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var_os("FFCINE_LUT_DIR")
        .map(|d| std::env::split_paths(&d).collect())
        .unwrap_or_default();
    if let Some(d) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    {
        v.push(d.join("ffcine").join("luts"));
    }
    v
}

/// What a profile does to the source before the grade.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraLut {
    /// Log -> Rec.709 (BT.1886) on the decoded, range-expanded R'G'B' (`LutType::Normalized`).
    pub lut: Lut,
    /// Re-tag the source as Rec.709 primaries and transfer, so libplacebo treats the LUT's output
    /// as Rec.709 and converts nothing more (the matrix and range tags stay).
    pub primaries: Primaries,
    pub transfer: Transfer,
}

impl Profile {
    /// The camera profile for a container-level picture profile ([`crate::profile`]); `None`
    /// where ffcine has none yet (D-Log2, unknown).
    pub fn from_picture(p: PictureProfile) -> Option<Self> {
        Some(match p {
            PictureProfile::Rec709 => Self::Rec709,
            PictureProfile::Dlog => Self::DjiDlog,
            PictureProfile::DlogM => Self::DjiDlogM,
            PictureProfile::Hlg | PictureProfile::Pq => Self::Hdr,
            PictureProfile::CLog3 => Self::CanonClog3,
            PictureProfile::AppleLog => Self::AppleLog,
            _ => return None,
        })
    }

    /// The profile for a probed file: [`crate::profile::detect`] on its ISO BMFF metadata (DJI's
    /// CAM meta colour mode, Canon and Apple tags; what the ingest catalog records) when that is
    /// at least medium confidence, else [`Profile::detect`] on the probe.
    pub fn detect_file(info: &MediaInfo) -> Self {
        crate::bmff::read(&info.path)
            .ok()
            .map(|m| crate::profile::detect(&m))
            .filter(|g| g.confidence != Confidence::Low)
            .and_then(|g| Self::from_picture(g.profile))
            .unwrap_or_else(|| Self::detect(info))
    }

    /// The profile the probe suggests (see the module docs).
    pub fn detect(info: &MediaInfo) -> Self {
        let Some((_, v)) = info.video() else {
            return Self::Rec709;
        };
        let c = v.color;
        let tag = |k: &str| info.tags.get(k).map_or("", String::as_str);
        let canon = tag("compatible_brands").contains("CAEP") || tag("major_brand") == "CAEP";
        let dji = tag("encoder").starts_with("DJI");
        if c.is_hdr() {
            return Self::Hdr;
        }
        let wide = matches!(c.primaries, Primaries::Bt2020 | Primaries::DisplayP3);
        if c.primaries == Primaries::Bt2020 && c.transfer == Transfer::Unknown {
            return if canon {
                Self::CanonClog3
            } else {
                Self::AppleLog
            };
        }
        if wide && c.transfer == Transfer::Bt709 {
            return Self::Hdr;
        }
        if dji && v.pixel_format.contains("10") {
            let enc = tag("encoder").replace(' ', "");
            return if enc.contains("Pocket3") {
                Self::DjiDlogM
            } else {
                Self::DjiDlog
            };
        }
        Self::Rec709
    }

    /// The camera LUT for this profile, `None` for profiles libplacebo handles from the tags.
    /// Vendor files are searched in `dirs` (see [`lut_dirs`]); Apple Log and Canon Log 3 fall
    /// back to analytic LUTs, D-Log M has no published curve and needs DJI's file.
    pub fn camera_lut(self, dirs: &[PathBuf]) -> Result<Option<CameraLut>> {
        let lut = match self {
            Self::Rec709 | Self::Hdr => return Ok(None),
            Self::DjiDlog => match find(dirs, self.vendor_names()) {
                Some(p) => Lut::from_cube_file(p)?,
                None => analytic(
                    33,
                    dlog_decode,
                    &rgb_to_rgb(&D_GAMUT, &BT709),
                    0.53,
                    "DJI D-Log / D-Gamut",
                ),
            },
            Self::DjiDlogVivid | Self::DjiDlogM | Self::DjiDlogMVivid => {
                let names = self.vendor_names();
                let path = find(dirs, names).ok_or_else(|| Error::Invalid {
                    what: "camera LUT (DJI's .cube is not in the LUT dirs: run `ffcine fetch-luts`, \
                           or download it from https://www.dji.com/downloads and set FFCINE_LUT_DIR)",
                    value: format!("{self}: tried {names:?} in {dirs:?}"),
                })?;
                Lut::from_cube_file(path)?
            }
            Self::AppleLog => match find(dirs, self.vendor_names()) {
                Some(p) => Lut::from_cube_file(p)?,
                None => analytic(33, apple_log_decode, &BT2020_TO_BT709, 0.8, "Apple Log"),
            },
            _ => match find(dirs, self.vendor_names()) {
                Some(p) => Lut::from_cube_file(p)?,
                None => analytic(
                    33,
                    clog3_decode,
                    &rgb_to_rgb(&CINEMA_GAMUT, &BT709),
                    0.8,
                    "Canon Log 3",
                ),
            },
        };
        Ok(Some(CameraLut {
            lut,
            primaries: Primaries::Bt709,
            transfer: Transfer::Bt709,
        }))
    }

    /// The camera LUT for an HDR deliverable: log -> BT.2020 PQ, mastered to `peak` cd/m², from
    /// the analytic curves ([`analytic_pq`]); vendor LUTs only reach Rec.709. `None` for
    /// profiles libplacebo maps from the tags; an error for D-Log M (no published curve).
    pub fn camera_lut_hdr(self, peak: f64) -> Result<Option<CameraLut>> {
        let lut = match self {
            Self::Rec709 | Self::Hdr => return Ok(None),
            Self::DjiDlog => analytic_pq(
                33,
                dlog_decode,
                &rgb_to_rgb(&D_GAMUT, &BT2020),
                0.53 / 0.8,
                peak,
                "DJI D-Log / D-Gamut",
            ),
            Self::AppleLog => analytic_pq(
                33,
                apple_log_decode,
                &rgb_to_rgb(&BT2020, &BT2020),
                1.0,
                peak,
                "Apple Log",
            ),
            Self::CanonClog3 => analytic_pq(
                33,
                clog3_decode,
                &rgb_to_rgb(&CINEMA_GAMUT, &BT2020),
                1.0,
                peak,
                "Canon Log 3",
            ),
            _ => {
                return Err(Error::Invalid {
                    what: "camera profile for HDR (DJI publishes no D-Log M curve, only Rec.709 LUTs)",
                    value: self.to_string(),
                });
            }
        };
        Ok(Some(CameraLut {
            lut,
            primaries: Primaries::Bt2020,
            transfer: Transfer::Pq,
        }))
    }

    /// File names a vendor LUT for this profile may have in a LUT dir, preferred first.
    pub fn vendor_names(self) -> &'static [&'static str] {
        match self {
            Self::DjiDlog => &[
                "dlog_to_rec709.cube",
                "DJI OSMO Pocket 4 D-Log to Rec.709 V2.0.cube",
                "DJI OSMO Pocket 4 D-Log to Rec.709 V1.0.cube",
                "DJI Osmo Pocket 4 D-Log to Rec.709 V1.0.cube",
            ],
            Self::DjiDlogVivid => &[
                "dlog_to_rec709_vivid.cube",
                "DJI OSMO Pocket 4 D-Log to Rec.709 vivid V2.0.cube",
                "DJI OSMO Pocket 4 D-Log to Rec.709 vivid V1.0.cube",
                "DJI Osmo Pocket 4 D-Log to Rec.709 vivid V1.0.cube",
            ],
            Self::DjiDlogM => &[
                "dlogm_to_rec709.cube",
                "DJI OSMO Pocket 3 D-Log M to Rec.709 V1.cube",
            ],
            Self::DjiDlogMVivid => &[
                "dlogm_to_rec709_vivid.cube",
                "DJI OSMO Pocket 3 D-Log M to Rec.709 vivid V1.cube",
            ],
            Self::AppleLog => &["applelog_to_rec709.cube", "AppleLogToRec709-v1.0.cube"],
            Self::CanonClog3 => &[
                "clog3_to_rec709.cube",
                "CinemaGamut_CanonLog3-to-BT709_WideDR_65_FF_Ver.2.0.cube",
            ],
            _ => &[],
        }
    }
}

/// DJI's official camera LUTs: (file name, URL). DJI states no redistribution terms, so they
/// aren't vendored; `ffcine fetch-luts` downloads them into the first [`lut_dirs`] entry.
pub const VENDOR_LUTS: &[(&str, &str)] = &[
    (
        "DJI OSMO Pocket 4 D-Log to Rec.709 V2.0.cube",
        "https://terra-1-g.djicdn.com/851d20f7b9f64838a34cd02351370894/HG214%20Lut/DJI%20OSMO%20Pocket%204%20D-Log%20to%20Rec.709%20V2.0.cube",
    ),
    (
        "DJI OSMO Pocket 4 D-Log to Rec.709 vivid V2.0.cube",
        "https://terra-1-g.djicdn.com/851d20f7b9f64838a34cd02351370894/HG214%20Lut/DJI%20OSMO%20Pocket%204%20D-Log%20to%20Rec.709%20vivid%20V2.0.cube",
    ),
];

fn find(dirs: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .flat_map(|n| dirs.iter().map(move |d| d.join(n)))
        .find(|p| p.is_file())
}

/// A chromaticity set: red, green, blue, white (CIE xy).
pub type Chromaticities = [[f64; 2]; 4];

const D65: [f64; 2] = [0.3127, 0.3290];
pub const BT709: Chromaticities = [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06], D65];
pub const BT2020: Chromaticities = [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046], D65];
/// DJI D-Gamut.
pub const D_GAMUT: Chromaticities = [[0.71, 0.31], [0.21, 0.88], [0.09, -0.08], D65];
/// Canon Cinema Gamut.
pub const CINEMA_GAMUT: Chromaticities = [[0.74, 0.27], [0.17, 1.14], [0.08, -0.10], D65];

type M3 = [[f64; 3]; 3];

/// BT.2020 -> BT.709 as the prototype's `applelog_lut.py` has it (4 decimals).
const BT2020_TO_BT709: M3 = [
    [1.6605, -0.5876, -0.0728],
    [-0.1246, 1.1329, -0.0083],
    [-0.0182, -0.1006, 1.1187],
];

fn mul(a: &M3, b: &M3) -> M3 {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

fn inv(m: &M3) -> M3 {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    [
        [
            (e * i - f * h) / det,
            (c * h - b * i) / det,
            (b * f - c * e) / det,
        ],
        [
            (f * g - d * i) / det,
            (a * i - c * g) / det,
            (c * d - a * f) / det,
        ],
        [
            (d * h - e * g) / det,
            (b * g - a * h) / det,
            (a * e - b * d) / det,
        ],
    ]
}

/// Linear RGB -> XYZ for a set of primaries (SMPTE RP 177).
pub fn rgb_to_xyz(c: &Chromaticities) -> [[f64; 3]; 3] {
    let xyz = |[x, y]: [f64; 2]| [x / y, 1.0, (1.0 - x - y) / y];
    let [r, g, b, w] = c.map(xyz);
    let p = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    let pi = inv(&p);
    let s: [f64; 3] = std::array::from_fn(|i| (0..3).map(|k| pi[i][k] * w[k]).sum());
    std::array::from_fn(|i| std::array::from_fn(|j| p[i][j] * s[j]))
}

/// Linear RGB in `from` -> linear RGB in `to` (same white, no adaptation).
pub fn rgb_to_rgb(from: &Chromaticities, to: &Chromaticities) -> [[f64; 3]; 3] {
    mul(&inv(&rgb_to_xyz(to)), &rgb_to_xyz(from))
}

/// Apple Log code value -> scene linear (Apple Log Profile white paper).
pub fn apple_log_decode(p: f64) -> f64 {
    const R0: f64 = -0.056_410_88;
    const RT: f64 = 0.01;
    const C: f64 = 47.287_112_36;
    const BETA: f64 = 0.009_640_52;
    const GAMMA: f64 = 0.085_504_79;
    const DELTA: f64 = 0.693_369_45;
    let pt = C * (RT - R0).powi(2);
    if p >= pt {
        2f64.powf((p - DELTA) / GAMMA) - BETA
    } else if p > 0.0 {
        (p / C).sqrt() + R0
    } else {
        R0
    }
}

/// DJI D-Log code value -> scene linear (DJI's "D-Log and D-Gamut" white paper, the curve
/// Resolve's colour space transform uses).
pub fn dlog_decode(y: f64) -> f64 {
    if y <= 0.14 {
        (y - 0.0929) / 6.025
    } else {
        (10f64.powf(3.896_16 * y - 2.277_52) - 0.0108) / 0.9892
    }
}

/// Canon Log 3 (range-expanded) code value -> scene linear, 0.18 = mid grey (Canon's formula,
/// reflectance in, as colour-science's `log_decoding_CanonLog3`).
pub fn clog3_decode(y: f64) -> f64 {
    let x = if y < 0.097_465_473 {
        -(10f64.powf((0.127_839_01 - y) / 0.367_268_45) - 1.0) / 14.983_25
    } else if y <= 0.152_778_91 {
        (y - 0.125_122_19) / 1.975_479_8
    } else {
        (10f64.powf((y - 0.122_405_37) / 0.367_268_45) - 1.0) / 14.983_25
    };
    x * 0.9
}

/// Narkowicz's ACES filmic fit: the prototype's gentle display tone map for log footage.
fn filmic(x: f64) -> f64 {
    let x = x.max(0.0);
    ((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14)).clamp(0.0, 1.0)
}

/// A log -> Rec.709 LUT: decode, gamut matrix to BT.709, exposure, filmic tone map, BT.1886
/// (2.4) encode. The prototype's `applelog_lut.py`, generalised over the curve and gamut.
/// `exposure` scales scene linear into the curve: 0.8 is the prototype's `applelog_lut.py`
/// default; D-Log uses 0.53, which puts mid grey where DJI's own V2.0 LUT does.
fn analytic(n: usize, decode: fn(f64) -> f64, gamut: &M3, exposure: f64, title: &str) -> Lut {
    let mut lut = Lut::identity(n);
    for e in &mut lut.data {
        let lin = e.map(|v| decode(f64::from(v)));
        *e = std::array::from_fn(|i| {
            let v: f64 = (0..3).map(|k| gamut[i][k] * lin[k]).sum();
            #[allow(clippy::cast_possible_truncation)]
            let out = filmic(v * exposure).powf(1.0 / 2.4) as f32;
            out
        });
    }
    lut.title = Some(format!("{title} to Rec.709 (ffcine analytic)"));
    lut
}

/// SMPTE ST 2084 (PQ) inverse EOTF: cd/m² -> signal.
pub fn pq_encode(nits: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let y = (nits / 10_000.0).clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

/// Scene-linear 18 % grey on an HDR display: 26 cd/m² (ITU-R BT.2408, with diffuse white at
/// 203 cd/m²).
const HDR_GREY_NITS: f64 = 26.0;
/// Where the highlight roll-off starts: BT.2408's diffuse white.
const HDR_KNEE_NITS: f64 = 203.0;

/// Scene linear (0.18 = grey) -> display cd/m²: grey at [`HDR_GREY_NITS`], linear to the knee,
/// then an exponential shoulder that reaches `peak` asymptotically.
pub fn hdr_display_nits(x: f64, peak: f64) -> f64 {
    let l = x.max(0.0) * HDR_GREY_NITS / 0.18;
    let knee = HDR_KNEE_NITS.min(0.75 * peak);
    if l <= knee {
        l
    } else {
        let room = peak - knee;
        knee + room * (1.0 - (-(l - knee) / room).exp())
    }
}

/// A log -> BT.2020 PQ LUT: decode, gamut matrix to BT.2020, `exposure`, [`hdr_display_nits`],
/// PQ encode. The HDR twin of the Rec.709 analytic LUTs; `exposure` 1 keeps 18 % grey at grey.
fn analytic_pq(
    n: usize,
    decode: fn(f64) -> f64,
    gamut: &M3,
    exposure: f64,
    peak: f64,
    title: &str,
) -> Lut {
    let mut lut = Lut::identity(n);
    for e in &mut lut.data {
        let lin = e.map(|v| decode(f64::from(v)));
        *e = std::array::from_fn(|i| {
            let v: f64 = (0..3).map(|k| gamut[i][k] * lin[k]).sum();
            #[allow(clippy::cast_possible_truncation)]
            let out = pq_encode(hdr_display_nits(v * exposure, peak)) as f32;
            out
        });
    }
    lut.title = Some(format!(
        "{title} to BT.2020 PQ {peak} nits (ffcine analytic)"
    ));
    lut
}

/// Per-clip profile choice: `auto` (probe) or a fixed [`Profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileChoice {
    #[default]
    Auto,
    Fixed(Profile),
}

impl ProfileChoice {
    pub fn resolve(self, info: &MediaInfo) -> Profile {
        match self {
            Self::Auto => Profile::detect_file(info),
            Self::Fixed(p) => p,
        }
    }
}

impl FromStr for ProfileChoice {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        if s == "auto" {
            Ok(Self::Auto)
        } else {
            s.parse().map(Self::Fixed)
        }
    }
}

impl fmt::Display for ProfileChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Fixed(p) => p.fmt(f),
        }
    }
}

/// Writes `lut` as a `.cube` file (for inspection, or to hand an analytic LUT to other tools).
pub fn write_cube(lut: &Lut, path: &Path) -> Result<()> {
    lut.write_cube(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorSpace;
    use crate::color::{Matrix, Range};
    use crate::probe::{StreamInfo, StreamKind, VideoInfo};

    fn info(color: ColorSpace, pix: &str, tags: &[(&str, &str)]) -> MediaInfo {
        MediaInfo {
            path: "x".into(),
            format: "mov".into(),
            duration: None,
            bit_rate: None,
            streams: vec![StreamInfo {
                index: 0,
                codec: "hevc".into(),
                bit_rate: None,
                kind: StreamKind::Video(VideoInfo {
                    width: 3840,
                    height: 2160,
                    pixel_format: pix.into(),
                    frame_rate: None,
                    frames: None,
                    color,
                }),
            }],
            tags: tags
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn detects_profiles() {
        let sdr = ColorSpace::BT709_TV;
        let dji = [("encoder", "DJI OsmoPocket4")];
        assert_eq!(
            Profile::detect(&info(sdr, "yuv420p10le", &dji)),
            Profile::DjiDlog
        );
        assert_eq!(
            Profile::detect(&info(sdr, "yuv420p10le", &[("encoder", "DJI OsmoPocket3")])),
            Profile::DjiDlogM
        );
        assert_eq!(
            Profile::detect(&info(sdr, "yuv420p", &dji)),
            Profile::Rec709
        );
        let hlg = ColorSpace {
            primaries: Primaries::Bt2020,
            transfer: Transfer::Hlg,
            matrix: Matrix::Bt2020Ncl,
            range: Range::Limited,
        };
        assert_eq!(
            Profile::detect(&info(hlg, "yuv420p10le", &dji)),
            Profile::Hdr
        );
        let log = ColorSpace {
            transfer: Transfer::Unknown,
            ..hlg
        };
        assert_eq!(
            Profile::detect(&info(log, "yuv422p10le", &[])),
            Profile::AppleLog
        );
        assert_eq!(
            Profile::detect(&info(
                log,
                "yuv422p10le",
                &[("compatible_brands", "mp42CAEP")]
            )),
            Profile::CanonClog3
        );
        let pq = ColorSpace {
            transfer: Transfer::Pq,
            ..hlg
        };
        assert_eq!(
            Profile::detect(&info(pq, "yuv422p10le", &[("compatible_brands", "CAEP")])),
            Profile::Hdr
        );
        assert_eq!(
            "dlogm-vivid".parse::<ProfileChoice>().unwrap(),
            ProfileChoice::Fixed(Profile::DjiDlogMVivid)
        );
    }

    #[test]
    fn picture_profiles_map() {
        assert_eq!(
            Profile::from_picture(PictureProfile::Dlog),
            Some(Profile::DjiDlog)
        );
        assert_eq!(Profile::from_picture(PictureProfile::Dlog2), None);
        // Not a file: falls back to the probe.
        let i = info(
            ColorSpace::BT709_TV,
            "yuv420p10le",
            &[("encoder", "DJI OsmoPocket4")],
        );
        assert_eq!(Profile::detect_file(&i), Profile::DjiDlog);
    }

    #[test]
    fn curves() {
        // Apple Log: 18 % grey encodes to about 0.4882 (white paper table).
        assert!((apple_log_decode(0.488_2) - 0.18).abs() < 0.002);
        // D-Log: 18 % grey is about 0.398, continuous at the knee.
        assert!((dlog_decode(0.3983) - 0.18).abs() < 0.002);
        assert!((dlog_decode(0.14 - 1e-9) - dlog_decode(0.14 + 1e-9)).abs() < 1e-3);
        // Canon Log 3: 18 % grey is 34.3 % (full range), 90 % white 56.5 %.
        assert!((clog3_decode(0.343_4) - 0.18).abs() < 0.001);
        assert!((clog3_decode(0.564_5) - 0.9).abs() < 0.002);
        // Continuous at the linear segment's ends.
        for y in [0.097_465_473, 0.152_778_91] {
            assert!((clog3_decode(y - 1e-9) - clog3_decode(y + 1e-9)).abs() < 1e-6);
        }
    }

    #[test]
    fn gamut_matrices() {
        let m = rgb_to_rgb(&BT2020, &BT709);
        for i in 0..3 {
            for j in 0..3 {
                assert!((m[i][j] - BT2020_TO_BT709[i][j]).abs() < 1e-3, "{m:?}");
            }
            // White stays white.
            assert!((m[i].iter().sum::<f64>() - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn hdr_luts() {
        // PQ: 100 cd/m² is about 0.508, 1000 about 0.752, 10000 is 1.
        assert!((pq_encode(100.0) - 0.508).abs() < 0.001);
        assert!((pq_encode(1000.0) - 0.752).abs() < 0.001);
        assert!((pq_encode(10_000.0) - 1.0).abs() < 1e-9);
        // Grey at 26 nits, monotonic, never past the peak.
        assert!((hdr_display_nits(0.18, 1000.0) - 26.0).abs() < 1e-9);
        let mut last = 0.0;
        for i in 0..200 {
            let n = hdr_display_nits(f64::from(i) * 0.5, 1000.0);
            assert!(n >= last && n < 1000.0);
            last = n;
        }
        let c = Profile::CanonClog3.camera_lut_hdr(1000.0).unwrap().unwrap();
        assert_eq!((c.primaries, c.transfer), (Primaries::Bt2020, Transfer::Pq));
        // Canon Log 3 grey (0.3434) lands on 26 nits.
        let grey = c.lut.sample([0.343_4; 3]);
        assert!(
            (f64::from(grey[1]) - pq_encode(26.0)).abs() < 0.01,
            "{grey:?}"
        );
        assert!(Profile::DjiDlog.camera_lut_hdr(1000.0).unwrap().is_some());
        assert!(Profile::DjiDlogM.camera_lut_hdr(1000.0).is_err());
        assert!(Profile::Hdr.camera_lut_hdr(1000.0).unwrap().is_none());
    }

    #[test]
    fn analytic_luts() {
        let lut = Profile::AppleLog.camera_lut(&[]).unwrap().unwrap().lut;
        assert_eq!(lut.size, 33);
        // Monotonic on the grey axis, black near 0, mid grey around the middle.
        let grey: Vec<f32> = (0..33).map(|i| lut.at(i, i, i)[1]).collect();
        assert!(grey.windows(2).all(|w| w[1] >= w[0]));
        assert!(grey[0] < 0.02);
        let c = Profile::CanonClog3.camera_lut(&[]).unwrap().unwrap();
        assert_eq!(c.primaries, Primaries::Bt709);
        assert!(Profile::Hdr.camera_lut(&[]).unwrap().is_none());
        assert!(Profile::DjiDlogM.camera_lut(&[]).is_err());
        assert!(Profile::DjiDlog.camera_lut(&[]).unwrap().is_some());
    }
}
