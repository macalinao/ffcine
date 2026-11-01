//! Speech map: an energy VAD on 16 kHz mono audio, band-limited to the voice range so wind rumble
//! and hiss count less. Rough by design: it drives the speech map, the dead-stretch flags and
//! silence snapping; whisper runs its own Silero VAD for transcription.

use crate::index::Span;

pub const RATE: u32 = 16_000;
/// 20 ms analysis frames.
pub const FRAME: usize = 320;
pub const FRAME_SECONDS: f64 = FRAME as f64 / RATE as f64;

/// Streams samples into per-frame band energies (dBFS).
#[derive(Debug)]
pub struct Energy {
    hp: Biquad,
    lp: Biquad,
    acc: f64,
    n: usize,
    pub db: Vec<f32>,
}

impl Default for Energy {
    fn default() -> Self {
        let fs = f64::from(RATE);
        Self {
            hp: Biquad::highpass(200.0, fs),
            lp: Biquad::lowpass(3800.0, fs),
            acc: 0.0,
            n: 0,
            db: Vec::new(),
        }
    }
}

impl Energy {
    pub fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            let v = self.lp.run(self.hp.run(f64::from(s)));
            self.acc += v * v;
            self.n += 1;
            if self.n == FRAME {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let db = (10.0 * (self.acc / FRAME as f64 + 1e-12).log10()) as f32;
                self.db.push(db);
                self.acc = 0.0;
                self.n = 0;
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn new(f: f64, fs: f64, high: bool) -> Self {
        // RBJ cookbook, Q = 1/sqrt(2).
        let w = 2.0 * std::f64::consts::PI * f / fs;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / std::f64::consts::SQRT_2;
        let a0 = 1.0 + alpha;
        let b = if high {
            let k = f64::midpoint(1.0, cos);
            [k, -(1.0 + cos), k]
        } else {
            [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0]
        };
        Self {
            b: b.map(|v| v / a0),
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }
    fn highpass(f: f64, fs: f64) -> Self {
        Self::new(f, fs, true)
    }
    fn lowpass(f: f64, fs: f64) -> Self {
        Self::new(f, fs, false)
    }
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

#[derive(Debug, Clone)]
pub struct VadParams {
    /// Speech is this many dB over the clip's noise floor (its 15th percentile frame).
    pub over_floor: f32,
    /// ... and never below this absolute level (dBFS).
    pub min_db: f32,
    /// Shortest speech run kept, and the longest pause bridged inside one run (seconds).
    pub min_speech: f64,
    pub bridge: f64,
}

impl Default for VadParams {
    fn default() -> Self {
        Self {
            over_floor: 12.0,
            min_db: -50.0,
            min_speech: 0.25,
            bridge: 0.35,
        }
    }
}

/// Speech spans from per-frame energies.
pub fn speech(db: &[f32], p: &VadParams) -> Vec<Span> {
    if db.is_empty() {
        return Vec::new();
    }
    let mut sorted = db.to_vec();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() * 15 / 100];
    let thr = (floor + p.over_floor).max(p.min_db);
    let mut spans: Vec<Span> = Vec::new();
    let mut start = None;
    #[allow(clippy::cast_precision_loss)]
    let t = |i: usize| i as f64 * FRAME_SECONDS;
    for (i, &v) in db.iter().enumerate() {
        match (v > thr, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                spans.push(Span {
                    start: t(s),
                    end: t(i),
                });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push(Span {
            start: t(s),
            end: t(db.len()),
        });
    }
    let mut merged: Vec<Span> = Vec::new();
    for s in spans {
        match merged.last_mut() {
            Some(last) if s.start - last.end <= p.bridge => last.end = s.end,
            _ => merged.push(s),
        }
    }
    merged.retain(|s| s.len() >= p.min_speech);
    for s in &mut merged {
        s.start = (s.start * 100.0).round() / 100.0;
        s.end = (s.end * 100.0).round() / 100.0;
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_burst_over_noise() {
        let mut e = Energy::default();
        // 1 s faint hiss, 1 s of a 440 Hz tone, 1 s hiss.
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            #[allow(clippy::cast_precision_loss)]
            let v = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            v * 0.002
        };
        let mut s: Vec<f32> = (0..16_000).map(|_| noise()).collect();
        #[allow(clippy::cast_precision_loss)]
        s.extend(
            (0..16_000).map(|i| 0.3 * (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin()),
        );
        s.extend((0..16_000).map(|_| noise()));
        e.push(&s);
        assert_eq!(e.db.len(), 150);
        let sp = speech(&e.db, &VadParams::default());
        assert_eq!(sp.len(), 1, "{sp:?}");
        assert!(
            (sp[0].start - 1.0).abs() < 0.05 && (sp[0].end - 2.0).abs() < 0.05,
            "{sp:?}"
        );
    }

    #[test]
    fn rumble_is_filtered() {
        let mut e = Energy::default();
        #[allow(clippy::cast_precision_loss)]
        let s: Vec<f32> = (0..16_000)
            .map(|i| 0.5 * (i as f32 * 30.0 * std::f32::consts::TAU / 16_000.0).sin())
            .collect();
        e.push(&s);
        assert!(e.db[25..].iter().all(|&d| d < -35.0), "{:?}", &e.db[25..30]);
    }
}
