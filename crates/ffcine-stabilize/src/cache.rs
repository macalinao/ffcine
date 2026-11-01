//! On-disk cache of detect results, keyed by the source file (canonical path, size, mtime), the
//! range and the detect parameters. Plain text in `$XDG_CACHE_HOME/ffcine/stabilize/`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::{DetectParams, Detection, Motion, SourceRange};
use ffcine_core::{Error, Result};

/// Bump when detect's output for the same key changes (estimator, flow handling).
pub const FORMAT_VERSION: u32 = 2;

#[cfg_attr(not(feature = "nvof"), allow(dead_code))] // used by detect_cached
const MAGIC: &str = "ffcine-stabilize-detect";
/// The header before the US spelling; entries with it are still read.
const LEGACY_MAGIC: &str = "ffcine-stabilise-detect";

/// 64-bit FNV-1a: stable across builds and platforms, unlike `std`'s `DefaultHasher`.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// `$XDG_CACHE_HOME/ffcine/stabilize`, else `~/.cache/ffcine/stabilize`.
pub fn dir() -> Option<PathBuf> {
    Some(cache_base()?.join("stabilize"))
}

/// Where entries were written before the US spelling; still read.
fn legacy_dir() -> Option<PathBuf> {
    Some(cache_base()?.join("stabilise"))
}

fn cache_base() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("ffcine"))
}

/// The cache key as text (also stored in the entry, to rule out hash collisions).
pub fn key(path: &Path, range: &SourceRange, params: &DetectParams) -> Result<String> {
    let io = |source| Error::Io {
        path: path.to_owned(),
        source,
    };
    let canon = path.canonicalize().map_err(io)?;
    let meta = std::fs::metadata(&canon).map_err(io)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();
    let e = &params.estimate;
    Ok(format!(
        "version {FORMAT_VERSION}\nsource {}\nsize {}\nmtime {}.{:09}\n\
         range {} {} {}\ndetect {} {} {}\n\
         estimate {} {} {} {} {} {}\n",
        canon.display(),
        meta.len(),
        mtime.as_secs(),
        mtime.subsec_nanos(),
        range.start,
        range.frames,
        range.rate,
        params.analysis_width,
        params.quality,
        params.grid,
        e.border,
        e.max_cost,
        e.step,
        e.threshold,
        e.iterations,
        e.min_confidence,
    ))
}

/// Motions as text: `k tx ty angle scale confidence` per line (shortest round-trip floats).
pub fn motions_text(motions: &[Motion]) -> String {
    let mut s = String::from("# frame tx ty angle(rad) scale confidence; frame k -> k-1, px\n");
    for (k, m) in motions.iter().enumerate() {
        let _ = writeln!(
            s,
            "{k} {} {} {} {} {}",
            m.tx, m.ty, m.angle, m.scale, m.confidence
        );
    }
    s
}

#[cfg_attr(not(feature = "nvof"), allow(dead_code))] // used by detect_cached
fn to_text(key: &str, det: &Detection) -> String {
    let mut s = format!("{MAGIC} {FORMAT_VERSION}\n");
    for line in key.lines() {
        let _ = writeln!(s, "key {line}");
    }
    let _ = writeln!(
        s,
        "size {} {}\nanalysis {} {}\nframes {}",
        det.width,
        det.height,
        det.analysis_width,
        det.analysis_height,
        det.motions.len()
    );
    s + &motions_text(&det.motions)
}

/// `None` unless `text` is a complete entry for exactly `key`.
#[cfg_attr(not(feature = "nvof"), allow(dead_code))] // used by detect_cached
fn from_text(key: &str, text: &str) -> Option<Detection> {
    let mut lines = text.lines();
    let head = lines.next()?;
    if head != format!("{MAGIC} {FORMAT_VERSION}")
        && head != format!("{LEGACY_MAGIC} {FORMAT_VERSION}")
    {
        return None;
    }
    for want in key.lines() {
        if lines.next()?.strip_prefix("key ")? != want {
            return None;
        }
    }
    let mut pair = |tag: &str| -> Option<(u32, u32)> {
        let rest = lines.next()?.strip_prefix(tag)?;
        let (a, b) = rest.trim().split_once(' ')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    };
    let (width, height) = pair("size ")?;
    let (analysis_width, analysis_height) = pair("analysis ")?;
    let n: usize = lines.next()?.strip_prefix("frames ")?.parse().ok()?;
    let motions: Vec<Motion> = lines
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let v: Vec<f64> = l
                .split_whitespace()
                .skip(1)
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            let &[tx, ty, angle, scale, confidence] = v.as_slice() else {
                return None;
            };
            Some(Motion {
                tx,
                ty,
                angle,
                scale,
                confidence,
            })
        })
        .collect::<Option<_>>()?;
    (motions.len() == n).then_some(Detection {
        width,
        height,
        analysis_width,
        analysis_height,
        motions,
    })
}

/// [`super::detect`], through the cache: `(detection, true)` on a hit. A cache that can't be
/// read or written is skipped, not an error.
#[cfg(feature = "nvof")]
pub fn detect_cached(
    path: &Path,
    range: &SourceRange,
    params: &DetectParams,
) -> Result<(Detection, bool)> {
    let key = key(path, range, params)?;
    let name = format!("{:016x}.txt", fnv1a(key.as_bytes()));
    let file = dir().map(|d| d.join(&name));
    for f in file
        .iter()
        .chain(legacy_dir().map(|d| d.join(&name)).iter())
    {
        if let Some(det) = std::fs::read_to_string(f)
            .ok()
            .and_then(|t| from_text(&key, &t))
        {
            return Ok((det, true));
        }
    }
    let det = super::detect(path, range, params)?;
    if let Some(f) = &file
        && let Some(parent) = f.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        // Write then rename, so a concurrent reader never sees half an entry.
        let tmp = f.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, to_text(&key, &det)).is_ok() && std::fs::rename(&tmp, f).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Ok((det, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_core::probe::Rational;

    #[test]
    fn fnv1a_reference_values() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn roundtrip_and_key_checks() {
        let det = Detection {
            width: 3840,
            height: 2160,
            analysis_width: 1920,
            analysis_height: 1080,
            motions: vec![
                Motion::IDENTITY,
                Motion {
                    tx: -1.234_567_890_123,
                    ty: 0.1 + 0.2,
                    angle: -3e-5,
                    scale: 1.000_2,
                    confidence: 0.93,
                },
            ],
        };
        let key = "version 1\nsource /x.mp4\n";
        let text = to_text(key, &det);
        assert_eq!(from_text(key, &text), Some(det.clone()));
        let legacy = text.replacen(MAGIC, LEGACY_MAGIC, 1);
        assert_eq!(from_text(key, &legacy), Some(det));
        assert_eq!(from_text("version 1\nsource /y.mp4\n", &text), None);
        assert_eq!(from_text(key, &text[..text.len() - 10]), None);
    }

    #[test]
    fn key_tracks_inputs() {
        let me = std::env::current_exe().unwrap();
        let r = SourceRange {
            start: 2.6,
            frames: 199,
            rate: Rational {
                num: 24000,
                den: 1001,
            },
        };
        let p = DetectParams::default();
        let k = key(&me, &r, &p).unwrap();
        assert!(k.contains("range 2.6 199 24000/1001\n"));
        assert_ne!(k, key(&me, &SourceRange { frames: 200, ..r }, &p).unwrap());
        let p2 = DetectParams {
            analysis_width: 1280,
            ..p
        };
        assert_ne!(k, key(&me, &r, &p2).unwrap());
    }
}
