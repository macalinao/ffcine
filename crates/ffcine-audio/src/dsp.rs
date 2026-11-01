//! Small DSP kit: windowed-sinc resampling, FFT cross-correlation, biquads, the 4x true-peak
//! estimate and a lookahead limiter.

use std::f64::consts::PI;
use std::sync::OnceLock;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// Zero crossings on each side of the sinc kernel.
const SINC_HALF: usize = 16;
/// Kernel table points per zero crossing (linearly interpolated between).
const SINC_RES: usize = 256;

/// Blackman-windowed sinc, tabulated over `0..=SINC_HALF` zero crossings.
fn sinc_table() -> &'static [f32] {
    static T: OnceLock<Vec<f32>> = OnceLock::new();
    T.get_or_init(|| {
        let n = SINC_HALF * SINC_RES;
        (0..=n + 1)
            .map(|i| {
                let x = i as f64 / SINC_RES as f64;
                if x >= SINC_HALF as f64 {
                    return 0.0;
                }
                let s = if i == 0 {
                    1.0
                } else {
                    (PI * x).sin() / (PI * x)
                };
                let w = x / SINC_HALF as f64; // 0..1 from the centre
                let win = 0.42 + 0.5 * (PI * w).cos() + 0.08 * (2.0 * PI * w).cos();
                (s * win) as f32
            })
            .collect()
    })
}

/// Band-limited fractional read: `n` samples of `x` at positions `start + k * step` (in samples
/// of `x`; outside `x` is silence). `step > 1` low-passes to the new Nyquist. Used for sample-rate
/// conversion and for drift correction (a step of `1 + ppm * 1e-6`).
pub fn warp(x: &[f32], start: f64, step: f64, n: usize) -> Vec<f32> {
    let table = sinc_table();
    let cutoff = (1.0 / step).min(1.0);
    let half = (SINC_HALF as f64 / cutoff).ceil() as isize;
    let scale = cutoff * SINC_RES as f64;
    let len = x.len() as isize;
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let pos = start + k as f64 * step;
        let centre = pos.floor() as isize;
        let frac = pos - centre as f64;
        let lo = (centre - half + 1).max(0);
        let hi = (centre + half).min(len - 1);
        let mut acc = 0.0f32;
        for j in lo..=hi {
            let d = ((j - centre) as f64 - frac).abs() * scale;
            let idx = d as usize;
            if idx + 1 >= table.len() {
                continue;
            }
            let f = (d - idx as f64) as f32;
            let w = table[idx] + (table[idx + 1] - table[idx]) * f;
            acc += x[j as usize] * w;
        }
        out.push(acc * cutoff as f32);
    }
    out
}

/// Sample-rate conversion of one channel.
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return x.to_vec();
    }
    let step = f64::from(from) / f64::from(to);
    let n = (x.len() as f64 / step).ceil() as usize;
    warp(x, 0.0, step, n)
}

/// Full linear cross-correlation: `out[k] = sum_i a[i] * b[i + lag]` for
/// `lag = k - (a.len() - 1)`, i.e. lags `-(a.len()-1) ..= b.len()-1`.
pub fn xcorr(a: &[f32], b: &[f32]) -> Vec<f64> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let total = a.len() + b.len() - 1;
    let n = total.next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let pad = |x: &[f32]| {
        let mut v: Vec<Complex<f64>> = x.iter().map(|&s| Complex::new(f64::from(s), 0.0)).collect();
        v.resize(n, Complex::new(0.0, 0.0));
        v
    };
    let mut fa = pad(a);
    let mut fb = pad(b);
    fwd.process(&mut fa);
    fwd.process(&mut fb);
    for (x, y) in fa.iter_mut().zip(&fb) {
        *x = x.conj() * y;
    }
    inv.process(&mut fa);
    let k = 1.0 / n as f64;
    // Circular index of lag L is L mod n.
    (0..total)
        .map(|i| {
            let lag = i as isize - (a.len() as isize - 1);
            fa[lag.rem_euclid(n as isize) as usize].re * k
        })
        .collect()
}

/// Normalised cross-correlation over the overlap of `a` shifted by each lag against `b`
/// (indexing as [`xcorr`]); lags whose overlap is under `min_overlap` samples get 0.
pub fn ncc(a: &[f32], b: &[f32], min_overlap: usize) -> Vec<f64> {
    let raw = xcorr(a, b);
    let prefix = |x: &[f32]| {
        let mut p = Vec::with_capacity(x.len() + 1);
        p.push(0.0f64);
        let mut acc = 0.0;
        for &s in x {
            acc += f64::from(s) * f64::from(s);
            p.push(acc);
        }
        p
    };
    let (pa, pb) = (prefix(a), prefix(b));
    let (na, nb) = (a.len() as isize, b.len() as isize);
    raw.iter()
        .enumerate()
        .map(|(k, &c)| {
            let lag = k as isize - (na - 1);
            let i0 = (-lag).max(0);
            let i1 = na.min(nb - lag);
            if i1 - i0 < min_overlap as isize {
                return 0.0;
            }
            let ea = pa[i1 as usize] - pa[i0 as usize];
            let eb = pb[(i1 + lag) as usize] - pb[(i0 + lag) as usize];
            let d = (ea * eb).sqrt();
            if d > 1e-12 { c / d } else { 0.0 }
        })
        .collect()
}

/// Vertex of the parabola through `y[i-1], y[i], y[i+1]`, as an offset in `-0.5..=0.5`.
pub fn parabolic(y: &[f64], i: usize) -> f64 {
    if i == 0 || i + 1 >= y.len() {
        return 0.0;
    }
    let (a, b, c) = (y[i - 1], y[i], y[i + 1]);
    let d = a - 2.0 * b + c;
    if d.abs() < 1e-18 {
        0.0
    } else {
        (0.5 * (a - c) / d).clamp(-0.5, 0.5)
    }
}

/// RBJ biquad, direct form I.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
}

impl Biquad {
    pub fn highpass(rate: f64, f: f64, q: f64) -> Self {
        let w = 2.0 * PI * f / rate;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b: [
                (1.0 + c) / (2.0 * a0),
                -(1.0 + c) / a0,
                (1.0 + c) / (2.0 * a0),
            ],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
        }
    }

    pub fn lowpass(rate: f64, f: f64, q: f64) -> Self {
        let w = 2.0 * PI * f / rate;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b: [
                (1.0 - c) / (2.0 * a0),
                (1.0 - c) / a0,
                (1.0 - c) / (2.0 * a0),
            ],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
        }
    }

    pub fn run(&self, x: &mut [f32]) {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for s in x {
            let x0 = f64::from(*s);
            let y0 =
                self.b[0] * x0 + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
            (x2, x1, y2, y1) = (x1, x0, y1, y0);
            *s = y0 as f32;
        }
    }
}

/// Per-sample peak estimate with 4x oversampling (inter-sample peaks), max over channels.
pub fn true_peak_trace(channels: &[Vec<f32>]) -> Vec<f32> {
    // 4x polyphase interpolation, Hann-windowed sinc with 16 taps per phase.
    const TAPS: isize = 16;
    let n = channels.first().map_or(0, Vec::len);
    let mut peak = vec![0.0f32; n];
    let phases: Vec<Vec<f32>> = (1..4)
        .map(|p| {
            let frac = f64::from(p) / 4.0;
            (-TAPS / 2 + 1..=TAPS / 2)
                .map(|j| {
                    let d = j as f64 - frac;
                    let win = 0.5 + 0.5 * (PI * d / (TAPS as f64 / 2.0)).cos();
                    ((PI * d).sin() / (PI * d) * win) as f32
                })
                .collect()
        })
        .collect();
    for ch in channels {
        let len = ch.len() as isize;
        for i in 0..n {
            let mut m = ch[i].abs();
            for ph in &phases {
                let mut acc = 0.0f32;
                for (t, w) in ph.iter().enumerate() {
                    let j = i as isize + (t as isize - TAPS / 2 + 1) - 1;
                    if (0..len).contains(&j) {
                        acc += ch[j as usize] * w;
                    }
                }
                m = m.max(acc.abs());
            }
            peak[i] = peak[i].max(m);
        }
    }
    peak
}

/// Lookahead brickwall limiter on the 4x true-peak trace: gain ramps down linearly over
/// `lookahead` seconds before a peak and recovers exponentially over `release` seconds.
pub fn limit(channels: &mut [Vec<f32>], rate: u32, ceiling_db: f64, lookahead: f64, release: f64) {
    let ceiling = 10f64.powf(ceiling_db / 20.0) as f32;
    let peak = true_peak_trace(channels);
    let n = peak.len();
    if n == 0 {
        return;
    }
    let mut g: Vec<f32> = peak
        .iter()
        .map(|&p| if p > ceiling { ceiling / p } else { 1.0 })
        .collect();
    // The interpolated peak at sample i spans samples i-1..=i+1 of the input.
    let spread = g.clone();
    for (i, gi) in g.iter_mut().enumerate() {
        let lo = i.saturating_sub(1);
        let hi = (i + 1).min(n - 1);
        *gi = spread[lo..=hi].iter().copied().fold(1.0, f32::min);
    }
    let la = (lookahead * f64::from(rate)).max(1.0);
    let step = (1.0 / la) as f32;
    for i in (0..n - 1).rev() {
        g[i] = g[i].min(g[i + 1] + step);
    }
    let coeff = (1.0 - (-1.0 / (release * f64::from(rate))).exp()) as f32;
    let mut e = 1.0f32;
    for gi in &mut g {
        e = (e + (1.0 - e) * coeff).min(*gi);
        *gi = e;
    }
    for ch in channels {
        for (s, gi) in ch.iter_mut().zip(&g) {
            *s *= gi;
        }
    }
}

/// One-pole smoothing toward `target[i]`, with separate rise (`attack`) and fall (`release`)
/// time constants in samples.
pub fn smooth(target: &[f32], attack: f64, release: f64) -> Vec<f32> {
    let ca = (1.0 - (-1.0 / attack.max(1e-9)).exp()) as f32;
    let cr = (1.0 - (-1.0 / release.max(1e-9)).exp()) as f32;
    let mut y = 0.0f32;
    target
        .iter()
        .map(|&t| {
            let c = if t > y { ca } else { cr };
            y += (t - y) * c;
            y
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(f: f64, rate: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * f * i as f64 / rate).sin() as f32)
            .collect()
    }

    #[test]
    fn xcorr_finds_lag() {
        let a: Vec<f32> = (0..100).map(|i| ((i * 37) % 11) as f32 - 5.0).collect();
        let mut b = vec![0.0f32; 300];
        b[57..157].copy_from_slice(&a);
        let c = ncc(&a, &b, 10);
        let (k, _) = c
            .iter()
            .enumerate()
            .max_by(|x, y| x.1.total_cmp(y.1))
            .expect("non-empty");
        assert_eq!(k as isize - 99, 57);
        assert!((c[k] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn resample_keeps_tone() {
        let x = sine(440.0, 48000.0, 48000);
        let y = resample(&x, 48000, 8000);
        assert_eq!(y.len(), 8000);
        let want = sine(440.0, 8000.0, 8000);
        let err = y[200..7800]
            .iter()
            .zip(&want[200..7800])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 2e-3, "err {err}");
    }

    #[test]
    fn limiter_holds_ceiling() {
        let mut x = vec![sine(997.0, 48000.0, 48000)];
        for s in &mut x[0] {
            *s *= 2.0;
        }
        limit(&mut x, 48000, -1.0, 0.0015, 0.05);
        let tp = true_peak_trace(&x).into_iter().fold(0.0f32, f32::max);
        assert!(f64::from(tp) <= 10f64.powf(-1.0 / 20.0) * 1.02, "tp {tp}");
    }
}
