//! Whether the cuts land on the music's beats.

use serde::{Deserialize, Serialize};

/// The soundtrack's tempo and how the cuts sit on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Music {
    /// BPM.
    pub tempo: f64,
    /// Beat times, seconds.
    pub beats: Vec<f64>,
    /// A cut within this many seconds of a beat is on it.
    pub tolerance: f64,
    /// Cuts on a beat, of `cuts`.
    pub on_beat: usize,
    pub cuts: usize,
    /// Share of cuts that would land on a beat by chance at this tempo and tolerance.
    pub chance: f64,
}

impl Music {
    /// Share of cuts on a beat.
    #[allow(clippy::cast_precision_loss)]
    pub fn ratio(&self) -> f64 {
        if self.cuts == 0 {
            0.0
        } else {
            self.on_beat as f64 / self.cuts as f64
        }
    }

    /// Whether the cutting follows the beat clearly more than chance.
    pub fn cut_to_beat(&self) -> bool {
        self.cuts >= 3 && self.ratio() >= (2.0 * self.chance).max(0.5)
    }
}

/// Signed distance from `t` to the nearest beat (beat minus `t`).
pub fn nearest(beats: &[f64], t: f64) -> Option<f64> {
    let i = beats.partition_point(|&b| b < t);
    [i.checked_sub(1), Some(i)]
        .into_iter()
        .flatten()
        .filter_map(|j| beats.get(j))
        .map(|&b| b - t)
        .min_by(|a, b| a.abs().total_cmp(&b.abs()))
}

/// [`Music`] for `cuts` against `beats` at `tempo`.
#[allow(clippy::cast_precision_loss)]
pub fn align(tempo: f64, beats: &[f64], cuts: &[f64], tolerance: f64) -> Music {
    let on_beat = cuts
        .iter()
        .filter(|&&c| nearest(beats, c).is_some_and(|d| d.abs() <= tolerance))
        .count();
    let chance = (2.0 * tolerance * tempo / 60.0).min(1.0);
    Music {
        tempo: (tempo * 10.0).round() / 10.0,
        beats: beats
            .iter()
            .map(|b| (b * 1000.0).round() / 1000.0)
            .collect(),
        tolerance,
        on_beat,
        cuts: cuts.len(),
        chance: (chance * 1000.0).round() / 1000.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_on_the_beat() {
        let beats: Vec<f64> = (0..20).map(|i| f64::from(i) * 0.5).collect();
        let m = align(120.0, &beats, &[1.02, 2.0, 3.49, 4.25], 0.05);
        assert_eq!((m.on_beat, m.cuts), (3, 4));
        assert!((m.chance - 0.2).abs() < 1e-9);
        assert!(m.cut_to_beat());
        assert_eq!(
            nearest(&beats, 4.3).map(|d| (d * 100.0).round()),
            Some(20.0)
        );
        assert_eq!(nearest(&[], 1.0), None);
    }
}
