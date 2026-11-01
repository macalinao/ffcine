//! Which clips to triage: the `ffcine-ingest` catalog (`DIR/catalog.json`) when there is one,
//! with its corrected clocks and proxy paths; otherwise a directory scan or a list of paths.

use std::path::{Path, PathBuf};

use crate::Result;

/// Video extensions picked up by [`discover`] (case-insensitive).
const VIDEO_EXT: &[&str] = &["mp4", "mov", "mkv", "m4v", "mts", "insv"];
/// Directory names [`discover`] never descends into: exports, DJI sidecars, our own output.
const SKIP_DIRS: &[&str] = &["06_exports", "MISC", "triage", ".ffcine"];

#[derive(Debug, Clone, PartialEq)]
pub struct ClipFile {
    pub id: String,
    pub source: PathBuf,
    /// DJI low-res proxy next to the source (`.LRF`).
    pub proxy: Option<PathBuf>,
    /// Corrected local start time (`YYYY-MM-DDTHH:MM:SS`) and UTC start, from the catalog.
    pub local: Option<String>,
    pub utc: Option<String>,
}

impl ClipFile {
    pub fn new(source: PathBuf) -> Self {
        let id = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let proxy = ["LRF", "lrf"]
            .iter()
            .map(|e| source.with_extension(e))
            .find(|p| p.is_file());
        Self {
            id,
            source,
            proxy,
            local: None,
            utc: None,
        }
    }

    /// The file to decode for analysis.
    pub fn analysed(&self) -> &Path {
        self.proxy.as_deref().unwrap_or(&self.source)
    }
}

/// All camera originals under `dir`, sorted by path. Clip ids are file stems; when two files share
/// a stem the later one gets its parent directory prefixed.
pub fn discover(dir: &Path, skip: &[&Path]) -> Result<Vec<ClipFile>> {
    let mut out = Vec::new();
    walk(dir, skip, &mut out)?;
    out.sort();
    let mut clips: Vec<ClipFile> = out.into_iter().map(ClipFile::new).collect();
    dedupe_ids(&mut clips);
    Ok(clips)
}

/// Clips from a list file: one path per line, relative to the list; `#` comments.
pub fn from_list(list: &Path) -> Result<Vec<ClipFile>> {
    let base = list.parent().unwrap_or(Path::new("."));
    let mut clips: Vec<ClipFile> = std::fs::read_to_string(list)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| ClipFile::new(base.join(l)))
        .collect();
    dedupe_ids(&mut clips);
    Ok(clips)
}

/// Video clips from the ingest catalog in `dir`, or `None` when `dir` has no `catalog.json`.
pub fn from_catalog(dir: &Path) -> Result<Option<Vec<ClipFile>>> {
    if !dir.join(ffcine_ingest::catalog::FILE).is_file() {
        return Ok(None);
    }
    let cat = ffcine_ingest::catalog::read(dir)
        .map_err(|e| crate::Error::Invalid(format!("catalog: {e}")))?;
    let mut clips: Vec<ClipFile> = cat
        .clips
        .into_iter()
        .filter(|c| c.kind == ffcine_ingest::camera::Kind::Video)
        .map(|c| {
            let mut f = ClipFile::new(dir.join(&c.path));
            if let Some(p) = &c.proxy {
                f.proxy = Some(dir.join(p));
            }
            f.local = c.clock.local.map(|t| t.to_string());
            f.utc = c.clock.utc.map(|t| t.to_string());
            f
        })
        .collect();
    dedupe_ids(&mut clips);
    Ok(Some(clips))
}

fn walk(dir: &Path, skip: &[&Path], out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if e.file_type()?.is_dir() {
            if !SKIP_DIRS.contains(&name.as_ref()) && !skip.iter().any(|s| *s == p) {
                walk(&p, skip, out)?;
            }
        } else if p
            .extension()
            .is_some_and(|x| VIDEO_EXT.iter().any(|v| x.eq_ignore_ascii_case(v)))
        {
            out.push(p);
        }
    }
    Ok(())
}

fn dedupe_ids(clips: &mut [ClipFile]) {
    let mut seen = std::collections::HashSet::new();
    for c in clips {
        if !seen.insert(c.id.clone()) {
            let parent = c
                .source
                .parent()
                .and_then(Path::file_name)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            c.id = format!("{parent}-{}", c.id);
            seen.insert(c.id.clone());
        }
    }
}

/// DJI file names carry the camera-clock start: `DJI_20261028120719_0292_D` ->
/// (`2026-10-28T12:07:19`, 292).
pub fn dji_name(id: &str) -> Option<(String, u32)> {
    let mut parts = id.split('_');
    if parts.next()? != "DJI" {
        return None;
    }
    let ts = parts.next()?;
    let seq = parts.next()?.parse().ok()?;
    if ts.len() != 14 || !ts.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let d = |r: std::ops::Range<usize>| &ts[r];
    Some((
        format!(
            "{}-{}-{}T{}:{}:{}",
            d(0..4),
            d(4..6),
            d(6..8),
            d(8..10),
            d(10..12),
            d(12..14)
        ),
        seq,
    ))
}

/// Seconds since 2000-01-01 for a `YYYY-MM-DDTHH:MM:SS` camera time (no zone; only differences
/// are used).
pub fn camera_seconds(t: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, s) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant), shifted to 2000-03-01.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

/// Groups clips the camera split from one recording: a clip whose camera start is within `slack`
/// seconds of the previous clip's end (by DJI sequence) joins its take. Returns the take id per
/// clip (the id of the take's first clip), in the order of `clips`.
pub fn takes(clips: &[(String, Option<String>, f64)], slack: f64) -> Vec<String> {
    // (index, start seconds)
    let mut timed: Vec<(usize, i64)> = clips
        .iter()
        .enumerate()
        .filter_map(|(i, (_, rec, _))| Some((i, camera_seconds(rec.as_deref()?)?)))
        .collect();
    timed.sort_by_key(|&(i, t)| (t, i));
    let mut take: Vec<String> = clips.iter().map(|c| c.0.clone()).collect();
    for w in timed.windows(2) {
        let (a, ta) = w[0];
        let (b, tb) = w[1];
        #[allow(clippy::cast_precision_loss)]
        let gap = (tb - ta) as f64 - clips[a].2;
        if gap.abs() <= slack {
            let first = take[a].clone();
            take[b] = first;
        }
    }
    take
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dji_names() {
        assert_eq!(
            dji_name("DJI_20261028120719_0292_D"),
            Some(("2026-10-28T12:07:19".into(), 292))
        );
        assert_eq!(dji_name("IMG_1234"), None);
    }

    #[test]
    fn camera_time_differences() {
        let a = camera_seconds("2026-10-28T23:59:30").expect("a");
        let b = camera_seconds("2026-10-29T00:00:10").expect("b");
        assert_eq!(b - a, 40);
        let c = camera_seconds("2024-02-28T00:00:00").expect("c");
        let d = camera_seconds("2024-03-01T00:00:00").expect("d");
        assert_eq!(d - c, 2 * 86_400);
    }

    #[test]
    fn splits_join_a_take() {
        let c = |id: &str, rec: &str, dur| (id.to_owned(), Some(rec.to_owned()), dur);
        let clips = [
            c("a", "2026-10-28T12:00:00", 1800.0),
            c("b", "2026-10-28T12:30:01", 1800.0),
            c("x", "2026-10-28T14:00:00", 10.0),
            (String::from("y"), None, 5.0),
        ];
        assert_eq!(takes(&clips, 3.0), ["a", "a", "x", "y"]);
    }
}
