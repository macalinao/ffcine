//! Image-statistics check for the picture profile, through the `ffmpeg` CLI when it is on
//! `PATH` (Homebrew's works on macOS). Optional: the catalog falls back to metadata alone.
//!
//! Log curves lift black and flatten saturation: measured on Osmo Pocket 4 footage, D-Log
//! frames have a 10-bit `YMIN` of 155-205 and `SATAVG` of 4-14, Normal frames 56-64 and 43-59.
//! Scenes do this too (fog, snow, night), so the verdict is only ever [`Confidence::Low`].
//!
//! [`Confidence::Low`]: ffcine_core::Confidence::Low

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Frame statistics, normalised to 0..1 of the code range.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameStats {
    /// Darkest code value (`YMIN`).
    pub black: f64,
    /// 10th percentile luma (`YLOW`).
    pub low: f64,
    /// Mean chroma saturation (`SATAVG`).
    pub saturation: f64,
}

impl FrameStats {
    /// Looks like an ungraded log curve.
    pub fn looks_log(self) -> bool {
        // Normal: black ~0.06, sat ~0.05; Pocket 4 D-Log: black >= 0.15, sat <= 0.014.
        self.black > 0.11 && self.saturation < 0.025
    }
}

/// `signalstats` on one frame at `at` seconds. `None` if ffmpeg is missing or fails.
pub fn frame_stats(path: &Path, at: f64, bit_depth: u8) -> Option<FrameStats> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-nostdin", "-ss"])
        .arg(format!("{at:.2}"))
        .arg("-i")
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-an",
            "-vf",
            "scale=480:-2,signalstats,metadata=print:file=-",
            "-f",
            "null",
            "-",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&out.stdout), bit_depth)
}

fn parse(text: &str, bit_depth: u8) -> Option<FrameStats> {
    let get = |k: &str| -> Option<f64> {
        text.lines().find_map(|l| {
            l.split_once(&format!("signalstats.{k}="))?
                .1
                .trim()
                .parse()
                .ok()
        })
    };
    let full = f64::from((1u32 << bit_depth.clamp(8, 16)) - 1);
    Some(FrameStats {
        black: get("YMIN")? / full,
        low: get("YLOW")? / full,
        saturation: get("SATAVG")? / full,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_signalstats() {
        let dlog = "frame:0\nlavfi.signalstats.YMIN=155\nlavfi.signalstats.YLOW=251\nlavfi.signalstats.SATAVG=3.72\n";
        let s = parse(dlog, 10).unwrap();
        assert!(s.looks_log());
        let normal = "lavfi.signalstats.YMIN=64\nlavfi.signalstats.YLOW=111\nlavfi.signalstats.SATAVG=59.2\n";
        assert!(!parse(normal, 10).unwrap().looks_log());
        assert!(parse("", 10).is_none());
    }
}
