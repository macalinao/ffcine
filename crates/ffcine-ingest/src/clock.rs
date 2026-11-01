//! Per-camera clock and zone fixes, and the checks that catch cameras whose clocks disagree.
//!
//! Files are never rewritten: the fix lives in the config and is applied to catalog timestamps
//! (and to the day folder chosen at ingest).
//!
//! ```yaml
//! # clocks.yaml
//! zone: "+00:00"          # the trip's local time (UTC here), for cameras that don't say
//! reference: iphone       # the camera other clocks are checked against (network time)
//! cameras:
//!   pocket: { shift: "+0h" }
//!   r10: { shift: "-2m" }             # its clock runs 2 min fast
//!   h2e: { shift: "+0s", zone: "+00:00" }
//! sync:                               # the same moment seen by two cameras (clap, audio sync)
//!   - { a: "DJI_20261201101500_0003_D.MP4@12.4", b: "MVI_0042.MP4@3.1" }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::camera::Origin;
use crate::time::{Shift, WallTime, Zone};
use crate::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraClock {
    /// Added to the camera's clock (`-2m` for a clock 2 minutes fast).
    pub shift: Shift,
    /// The zone the camera's clock was set to, overriding what the file says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone: Option<Zone>,
}

/// A moment seen by two clips: `NAME@SECONDS` each (file name, seconds into the clip).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncPoint {
    pub a: String,
    pub b: String,
}

impl SyncPoint {
    pub fn parse_side(s: &str) -> Result<(&str, f64)> {
        let (name, at) = s.rsplit_once('@').unwrap_or((s, "0"));
        let at = at
            .parse()
            .map_err(|_| Error::Config(format!("bad sync point {s:?} (want NAME@SECONDS)")))?;
        Ok((name, at))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClockConfig {
    /// Default zone for cameras whose files don't record one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone: Option<Zone>,
    /// Camera to measure the others against in the checks (default: `iphone` if present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub cameras: BTreeMap<String, CameraClock>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sync: Vec<SyncPoint>,
}

impl ClockConfig {
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        yaml_serde::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
    }

    /// Adds `CAMERA=SHIFT[@ZONE]` from the command line (`r10=-2m`, `pocket=+0h@+00:00`).
    pub fn set(&mut self, arg: &str) -> Result<()> {
        let (cam, rest) = arg
            .split_once('=')
            .ok_or_else(|| Error::Config(format!("bad --clock {arg:?} (want CAMERA=SHIFT)")))?;
        let (shift, zone) = match rest.split_once('@') {
            Some((s, z)) => (s, Some(z.parse()?)),
            None => (rest, None),
        };
        let c = self.cameras.entry(cam.to_owned()).or_default();
        c.shift = shift.parse()?;
        if zone.is_some() {
            c.zone = zone;
        }
        Ok(())
    }

    /// Corrected time for a file from `camera`.
    pub fn apply(&self, o: &Origin) -> Clock {
        let cam = self.cameras.get(&o.camera).cloned().unwrap_or_default();
        let shift = cam.shift.secs();
        let zone = cam.zone.or(o.camera_zone).or(self.zone);
        let (local, utc) = if let Some(t) = o.camera_time {
            let local = WallTime(t.0 + shift);
            (Some(local), zone.map(|z| z.to_utc(local)))
        } else if let Some(u) = o.utc {
            let utc = WallTime(u.0 + shift);
            (zone.map(|z| z.from_utc(utc)), Some(utc))
        } else {
            (None, None)
        };
        Clock {
            camera_time: o.camera_time,
            camera_zone: o.camera_zone,
            shift: cam.shift,
            local,
            zone,
            utc,
            source: o.time_source.to_owned(),
        }
    }
}

/// A clip's timestamps: what the camera said, and the corrected values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_time: Option<WallTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_zone: Option<Zone>,
    pub shift: Shift,
    /// Corrected local wall time (the day folder and the edit's notion of "when").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<WallTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone: Option<Zone>,
    /// Corrected UTC instant, when the zone is known: what cross-camera ordering uses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utc: Option<WallTime>,
    pub source: String,
}

impl Clock {
    /// `YYYY-MM-DD`, or `undated`.
    pub fn day(&self) -> String {
        self.local
            .or(self.utc)
            .map_or_else(|| "undated".to_owned(), WallTime::date)
    }

    /// A timeline position for ordering across cameras: UTC if known, else local.
    pub fn instant(&self) -> Option<WallTime> {
        self.utc.or(self.local)
    }
}

/// A clip as the checks see it.
#[derive(Debug, Clone)]
pub struct Timed<'a> {
    pub name: &'a str,
    pub camera: &'a str,
    pub clock: &'a Clock,
    pub duration: Option<f64>,
}

/// Clock problems for the catalog report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockIssue {
    pub camera: String,
    pub message: String,
    /// The `cameras.<id>.shift` that would fix it, when measurable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggest_shift: Option<Shift>,
}

/// Two clips from different cameras recorded at the same time: where audio sync should look.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overlap {
    pub a: String,
    pub b: String,
    pub seconds: f64,
}

/// Plausible recording years; outside means the clock was reset (DJI's year 2000).
const MIN_YEAR: i64 = 2015;

/// Runs the clock checks: unset clocks, zone disagreement, sync points.
#[allow(clippy::cast_precision_loss)]
pub fn check(cfg: &ClockConfig, clips: &[Timed<'_>], now: WallTime) -> Result<Vec<ClockIssue>> {
    let mut issues = Vec::new();
    let mut seen_zone: BTreeMap<&str, BTreeMap<String, usize>> = BTreeMap::new();
    let mut unset: BTreeMap<&str, usize> = BTreeMap::new();
    for c in clips {
        if let Some(t) = c.clock.local
            && (t.year() < MIN_YEAR || t.0 > now.0 + 2 * 86_400)
        {
            *unset.entry(c.camera).or_default() += 1;
        }
        if let Some(z) = c.clock.zone {
            *seen_zone
                .entry(c.camera)
                .or_default()
                .entry(z.to_string())
                .or_default() += 1;
        }
    }
    for (cam, n) in unset {
        issues.push(ClockIssue {
            camera: cam.to_owned(),
            message: format!(
                "{n} clip(s) dated before {MIN_YEAR} or in the future: clock not set; add a shift"
            ),
            suggest_shift: None,
        });
    }
    let zones: BTreeMap<&str, String> = seen_zone
        .iter()
        .map(|(cam, z)| {
            let top = z.iter().max_by_key(|(_, n)| **n).map(|(z, _)| z.clone());
            (*cam, top.unwrap_or_default())
        })
        .collect();
    for (cam, z) in &seen_zone {
        if z.len() > 1 {
            let list: Vec<_> = z.iter().map(|(z, n)| format!("{z} ({n})")).collect();
            issues.push(ClockIssue {
                camera: (*cam).to_owned(),
                message: format!("clock zone changed between clips: {}", list.join(", ")),
                suggest_shift: None,
            });
        }
    }
    let distinct: std::collections::BTreeSet<&String> = zones.values().collect();
    if distinct.len() > 1 {
        let list: Vec<_> = zones.iter().map(|(c, z)| format!("{c} {z}")).collect();
        issues.push(ClockIssue {
            camera: "*".into(),
            message: format!(
                "cameras are on different zones: {}; set each camera's zone or fix its clock",
                list.join(", ")
            ),
            suggest_shift: None,
        });
    }
    let reference = cfg.reference.clone().unwrap_or_else(|| "iphone".into());
    for s in &cfg.sync {
        let (an, aat) = SyncPoint::parse_side(&s.a)?;
        let (bn, bat) = SyncPoint::parse_side(&s.b)?;
        let find = |n: &str| clips.iter().find(|c| c.name == n);
        let (Some(a), Some(b)) = (find(an), find(bn)) else {
            issues.push(ClockIssue {
                camera: "*".into(),
                message: format!("sync point {} = {}: clip not in catalog", s.a, s.b),
                suggest_shift: None,
            });
            continue;
        };
        let (Some(ta), Some(tb)) = (a.clock.instant(), b.clock.instant()) else {
            continue;
        };
        // The same moment: a.start + aat == b.start + bat on a true clock.
        let diff = (ta.0 as f64 + aat) - (tb.0 as f64 + bat);
        if diff.abs() < 1.5 {
            continue;
        }
        // Fix the camera that isn't the reference; b by default.
        let (fix, by) = if a.camera == reference {
            (b.camera, diff)
        } else if b.camera == reference {
            (a.camera, -diff)
        } else {
            (b.camera, diff)
        };
        let current = cfg.cameras.get(fix).map_or(0.0, |c| c.shift.0);
        issues.push(ClockIssue {
            camera: fix.to_owned(),
            message: format!(
                "{} vs {}: clocks disagree by {}",
                a.camera,
                b.camera,
                Shift(diff.abs())
            ),
            suggest_shift: Some(Shift(((current + by) * 10.0).round() / 10.0)),
        });
    }
    Ok(issues)
}

/// Clips from different cameras whose corrected time ranges overlap.
#[allow(clippy::cast_precision_loss)]
pub fn overlaps(clips: &[Timed<'_>]) -> Vec<Overlap> {
    let mut spans: Vec<(f64, f64, &Timed<'_>)> = clips
        .iter()
        .filter_map(|c| {
            let t = c.clock.instant()?.0 as f64;
            Some((t, t + c.duration?, c))
        })
        .collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::new();
    for (i, (s0, e0, a)) in spans.iter().enumerate() {
        for (s1, e1, b) in &spans[i + 1..] {
            if s1 >= e0 {
                break;
            }
            if a.camera != b.camera {
                out.push(Overlap {
                    a: a.name.to_owned(),
                    b: b.name.to_owned(),
                    seconds: (e0.min(*e1) - s0.max(*s1)).max(0.0),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(camera: &str, t: &str, zone: Option<&str>) -> Origin {
        Origin {
            camera: camera.into(),
            camera_time: WallTime::parse(t).map(|(t, _)| t),
            camera_zone: zone.map(|z| z.parse().unwrap()),
            time_source: "test",
            ..Origin::default()
        }
    }

    #[test]
    fn shift_and_zone() {
        let mut cfg = ClockConfig::default();
        cfg.set("r10=-2m").unwrap();
        cfg.set("pocket=+0h@+00:00").unwrap();
        cfg.zone = Some(Zone::UTC);
        let c = cfg.apply(&origin("r10", "2026-12-01T10:02:00", None));
        assert_eq!(c.local.unwrap().to_string(), "2026-12-01T10:00:00");
        assert_eq!(c.utc, c.local);
        // DJI file says +08:00 (clock left on another zone); the config pins it to UTC.
        let c = cfg.apply(&origin("pocket", "2026-12-01T18:00:00", Some("+08:00")));
        assert_eq!(c.zone, Some(Zone::UTC));
        let c = cfg.apply(&origin("h2e", "2026-12-01T00:30:00", None));
        assert_eq!(c.day(), "2026-12-01");
    }

    #[test]
    fn yaml_config() {
        let cfg: ClockConfig = yaml_serde::from_str(
            "zone: '+00:00'\ncameras:\n  r10: { shift: '-2m' }\nsync:\n  - { a: 'A.MP4@10', b: 'B.MP4@4' }\n",
        )
        .unwrap();
        assert!((cfg.cameras["r10"].shift.0 + 120.0).abs() < 1e-9);
        assert_eq!(cfg.sync.len(), 1);
        assert!(yaml_serde::from_str::<ClockConfig>("bogus: 1").is_err());
    }

    #[test]
    fn checks_flag_disagreement() {
        let mut cfg = ClockConfig {
            zone: Some(Zone::UTC),
            reference: Some("pocket".into()),
            ..ClockConfig::default()
        };
        // The clap is 12 s into the pocket clip and 3 s into the r10 clip; r10's clock reads
        // 2 min fast, so its clip seems to start 2 min later than it did.
        cfg.sync.push(SyncPoint {
            a: "P.MP4@12".into(),
            b: "R.MP4@3".into(),
        });
        let p = cfg.apply(&origin("pocket", "2026-12-01T10:00:00", None));
        let r = cfg.apply(&origin("r10", "2026-12-01T10:02:09", None));
        let old = cfg.apply(&origin("pocket", "2000-01-01T00:00:00", None));
        let clips = [
            Timed {
                name: "P.MP4",
                camera: "pocket",
                clock: &p,
                duration: Some(200.0),
            },
            Timed {
                name: "R.MP4",
                camera: "r10",
                clock: &r,
                duration: Some(30.0),
            },
            Timed {
                name: "O.MP4",
                camera: "pocket",
                clock: &old,
                duration: Some(5.0),
            },
        ];
        let now = WallTime::parse("2026-12-02T00:00:00").unwrap().0;
        let issues = check(&cfg, &clips, now).unwrap();
        let sync = issues.iter().find(|i| i.camera == "r10").unwrap();
        assert_eq!(sync.suggest_shift, Some(Shift(-120.0)));
        assert!(issues.iter().any(|i| i.message.contains("clock not set")));
        let ov = overlaps(&clips);
        assert_eq!(ov.len(), 1);
        assert!((ov[0].seconds - 30.0).abs() < 1e-9);
    }
}
