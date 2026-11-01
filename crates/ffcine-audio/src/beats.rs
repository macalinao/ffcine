//! Music beats for cut snapping (`docs/audio.md#beats`): librosa's `beat_track` as the prototype
//! ran it (`pipeline/audio.py` `beats()`), on mono 22.05 kHz.
//!
//! Onset strength is the median over 128 mel bands of the positive frame-to-frame rise of the
//! log-mel spectrogram (2048-sample FFT, 512 hop). Tempo is the peak of the mean windowed onset
//! autocorrelation (8 s windows) under a log-normal prior around 120 BPM. Beats come from Ellis'
//! dynamic programme (tightness 100) over the onset envelope, then weak leading and trailing beats
//! are trimmed. [`cached`] keeps results as JSON keyed by path, size and mtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use serde::{Deserialize, Serialize};

use ffcine_core::{Error, Result};

use crate::decode::{self, Request};

/// Analysis sample rate.
pub const RATE: u32 = 22050;
const N_FFT: usize = 2048;
const HOP: usize = 512;
const N_MELS: usize = 128;
/// Tempogram window, `time_to_frames(8 s)`.
const AC_FRAMES: usize = 344;
const START_BPM: f64 = 120.0;
const MAX_BPM: f64 = 320.0;
const TIGHTNESS: f64 = 100.0;

/// A track's beat analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Beats {
    /// BPM.
    pub tempo: f64,
    /// Beat times, seconds into the file.
    pub beats: Vec<f64>,
    /// Seconds.
    pub duration: f64,
}

/// Decodes `path` (mono, 22.05 kHz) and finds its beats.
pub fn detect(path: &Path) -> Result<Beats> {
    let a = decode::load(
        path,
        &Request {
            rate: RATE,
            channels: 1,
            ..Request::default()
        },
    )?;
    Ok(analyse(&a.to_mono(), RATE))
}

/// [`detect`], cached as JSON in `cache_dir` under a key of the canonical path, size and mtime.
pub fn cached(path: &Path, cache_dir: &Path) -> Result<Beats> {
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
        .map_or(0, |d| d.as_nanos());
    let key = fnv1a(format!("beats1\0{}\0{}\0{mtime}", canon.display(), meta.len()).as_bytes());
    let file = cache_dir.join(format!("{key:016x}.json"));
    if let Some(b) = std::fs::read(&file)
        .ok()
        .and_then(|j| serde_json::from_slice::<Beats>(&j).ok())
    {
        return Ok(b);
    }
    let b = detect(&canon)?;
    // A cache that can't be written only costs a re-analysis.
    if std::fs::create_dir_all(cache_dir).is_ok() {
        let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
        if let Ok(j) = serde_json::to_vec(&b)
            && std::fs::write(&tmp, j).is_ok()
        {
            let _ = std::fs::rename(&tmp, &file);
        }
    }
    Ok(b)
}

/// `$XDG_CACHE_HOME/ffcine/beats`, else `~/.cache/ffcine/beats`.
pub fn default_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("ffcine")
        .join("beats")
}

fn fnv1a(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| {
        (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Beats of mono samples `y` at `sr`.
pub fn analyse(y: &[f32], sr: u32) -> Beats {
    let duration = y.len() as f64 / f64::from(sr);
    let env = onset_strength(y, sr);
    let frame_rate = f64::from(sr) / HOP as f64;
    if !env.iter().any(|&x| x != 0.0) {
        return Beats {
            tempo: 0.0,
            beats: Vec::new(),
            duration,
        };
    }
    let tempo = tempo(&env, frame_rate);
    let beats = track(&env, tempo, frame_rate)
        .into_iter()
        .map(|f| f as f64 / frame_rate)
        .collect();
    Beats {
        tempo,
        beats,
        duration,
    }
}

/// Periodic Hann window.
fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos())
        .collect()
}

fn hz_to_mel(f: f64) -> f64 {
    let lin = f / (200.0 / 3.0);
    if f >= 1000.0 {
        15.0 + (f / 1000.0).ln() / (6.4f64.ln() / 27.0)
    } else {
        lin
    }
}

fn mel_to_hz(m: f64) -> f64 {
    if m >= 15.0 {
        1000.0 * ((m - 15.0) * (6.4f64.ln() / 27.0)).exp()
    } else {
        m * 200.0 / 3.0
    }
}

/// Slaney mel filterbank, `[mel][bin]`, area-normalised.
fn mel_filters(sr: u32) -> Vec<Vec<f64>> {
    let bins = N_FFT / 2 + 1;
    let nyq = f64::from(sr) / 2.0;
    let freqs: Vec<f64> = (0..bins)
        .map(|k| nyq * k as f64 / (bins - 1) as f64)
        .collect();
    let (m0, m1) = (hz_to_mel(0.0), hz_to_mel(nyq));
    let pts: Vec<f64> = (0..N_MELS + 2)
        .map(|i| mel_to_hz(m0 + (m1 - m0) * i as f64 / (N_MELS + 1) as f64))
        .collect();
    (0..N_MELS)
        .map(|i| {
            let enorm = 2.0 / (pts[i + 2] - pts[i]);
            freqs
                .iter()
                .map(|&f| {
                    let lower = (f - pts[i]) / (pts[i + 1] - pts[i]);
                    let upper = (pts[i + 2] - f) / (pts[i + 2] - pts[i + 1]);
                    lower.min(upper).max(0.0) * enorm
                })
                .collect()
        })
        .collect()
}

/// librosa `onset_strength(aggregate=np.median)`: one value per 512-sample frame, frame `i`
/// centred on sample `512 i`.
pub fn onset_strength(y: &[f32], sr: u32) -> Vec<f64> {
    let pad = N_FFT / 2;
    let n_frames = 1 + y.len() / HOP;
    let mut padded = vec![0.0f64; y.len() + 2 * pad];
    for (d, &s) in padded[pad..].iter_mut().zip(y) {
        *d = f64::from(s);
    }
    let win = hann(N_FFT);
    let fb = mel_filters(sr);
    let fft = FftPlanner::new().plan_fft_forward(N_FFT);
    let mut buf = vec![Complex::new(0.0, 0.0); N_FFT];
    let mut db: Vec<Vec<f64>> = Vec::with_capacity(n_frames);
    let mut top = f64::NEG_INFINITY;
    for f in 0..n_frames {
        let s = f * HOP;
        for (k, c) in buf.iter_mut().enumerate() {
            let x = padded.get(s + k).copied().unwrap_or(0.0);
            *c = Complex::new(x * win[k], 0.0);
        }
        fft.process(&mut buf);
        let power: Vec<f64> = buf[..=N_FFT / 2].iter().map(Complex::norm_sqr).collect();
        let row: Vec<f64> = fb
            .iter()
            .map(|w| {
                let m: f64 = w.iter().zip(&power).map(|(a, b)| a * b).sum();
                10.0 * m.max(1e-10).log10()
            })
            .collect();
        top = row.iter().copied().fold(top, f64::max);
        db.push(row);
    }
    let floor = top - 80.0;
    for row in &mut db {
        for v in row.iter_mut() {
            *v = v.max(floor);
        }
    }
    // lag 1, then shifted by lag + n_fft / (2 hop) frames as librosa centres it.
    let shift = 1 + N_FFT / (2 * HOP);
    let mut env = vec![0.0; n_frames];
    let mut rise = vec![0.0; N_MELS];
    for j in 1..n_frames {
        let i = j - 1 + shift;
        if i >= n_frames {
            break;
        }
        for (m, r) in rise.iter_mut().enumerate() {
            *r = (db[j][m] - db[j - 1][m]).max(0.0);
        }
        env[i] = median(&mut rise);
    }
    env
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        f64::midpoint(v[n / 2 - 1], v[n / 2])
    }
}

/// librosa `beat.tempo`: BPM of the mean tempogram's peak under a log-normal prior.
pub fn tempo(env: &[f64], frame_rate: f64) -> f64 {
    let w = AC_FRAMES;
    let half = w / 2;
    let n = env.len();
    // np.pad(mode="linear_ramp", end_values=0)
    let mut padded = vec![0.0; n + 2 * half];
    padded[half..half + n].copy_from_slice(env);
    if n > 0 {
        let (a, b) = (env[0], env[n - 1]);
        for k in 0..half {
            padded[k] = a * k as f64 / half as f64;
            padded[half + n + k] = b * (half - k) as f64 / half as f64;
        }
    }
    let win = hann(w);
    let size = (2 * w - 1).next_power_of_two();
    let mut planner = FftPlanner::new();
    let fwd = planner.plan_fft_forward(size);
    let inv = planner.plan_fft_inverse(size);
    let mut mean = vec![0.0; w];
    let mut buf = vec![Complex::new(0.0, 0.0); size];
    for f in 0..n {
        for c in &mut *buf {
            *c = Complex::new(0.0, 0.0);
        }
        for k in 0..w {
            buf[k].re = padded[f + k] * win[k];
        }
        autocorrelate(&mut buf, &fwd, &inv);
        let peak = buf[..w].iter().map(|c| c.re.abs()).fold(0.0, f64::max);
        if peak > 0.0 {
            for (m, c) in mean.iter_mut().zip(&buf) {
                *m += c.re / peak;
            }
        }
    }
    for m in &mut mean {
        *m /= n.max(1) as f64;
    }
    let mut best = (f64::NEG_INFINITY, 0);
    for (lag, &tg) in mean.iter().enumerate().skip(1) {
        let bpm = 60.0 * frame_rate / lag as f64;
        if bpm >= MAX_BPM {
            continue;
        }
        let prior = -0.5 * (bpm.log2() - START_BPM.log2()).powi(2);
        let score = (1e6 * tg).ln_1p() + prior;
        if score > best.0 {
            best = (score, lag);
        }
    }
    if best.1 == 0 {
        return START_BPM;
    }
    60.0 * frame_rate / best.1 as f64
}

/// In-place autocorrelation of a zero-padded buffer (result in `re`).
fn autocorrelate(
    buf: &mut [Complex<f64>],
    fwd: &Arc<dyn rustfft::Fft<f64>>,
    inv: &Arc<dyn rustfft::Fft<f64>>,
) {
    fwd.process(buf);
    for c in &mut *buf {
        *c = Complex::new(c.norm_sqr(), 0.0);
    }
    inv.process(buf);
    let k = 1.0 / buf.len() as f64;
    for c in &mut *buf {
        c.re *= k;
    }
}

/// numpy's round half to even.
#[allow(clippy::float_cmp)] // exact halves are the point
fn round_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - x.signum()
    } else {
        r
    }
}

/// `x` convolved with an odd-length symmetric `w`, same length (scipy `mode="same"`).
fn convolve_same(x: &[f64], w: &[f64]) -> Vec<f64> {
    let h = (w.len() / 2) as isize;
    (0..x.len() as isize)
        .map(|i| {
            w.iter()
                .enumerate()
                .filter_map(|(k, wk)| {
                    let j = i + h - k as isize;
                    usize::try_from(j)
                        .ok()
                        .and_then(|j| x.get(j))
                        .map(|v| v * wk)
                })
                .sum()
        })
        .collect()
}

/// Ellis' dynamic-programming beat tracker (librosa `__beat_tracker`): beat frames.
pub fn track(env: &[f64], bpm: f64, frame_rate: f64) -> Vec<usize> {
    if bpm <= 0.0 || env.is_empty() {
        return Vec::new();
    }
    let period = round_even(60.0 * frame_rate / bpm).max(1.0);
    // Local score: onsets (unit std) smoothed by a Gaussian of period / 32.
    let n = env.len();
    let mean = env.iter().sum::<f64>() / n as f64;
    let sd = if n > 1 {
        (env.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
    } else {
        0.0
    };
    let norm: Vec<f64> = env
        .iter()
        .map(|x| if sd > 0.0 { x / sd } else { *x })
        .collect();
    let p = period as isize;
    let g: Vec<f64> = (-p..=p)
        .map(|k| (-0.5 * (k as f64 * 32.0 / period).powi(2)).exp())
        .collect();
    let local = convolve_same(&norm, &g);

    // Dynamic programme.
    let w0 = -2 * p;
    let w1 = -(round_even(period / 2.0) as isize);
    let offsets: Vec<isize> = (w0..=w1).collect();
    let txwt: Vec<f64> = offsets
        .iter()
        .map(|&o| -TIGHTNESS * ((-o as f64) / period).ln().powi(2))
        .collect();
    let thresh = 0.01 * local.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut cum = vec![0.0; n];
    let mut back = vec![-1isize; n];
    let mut first = true;
    for i in 0..n {
        let mut best = (f64::NEG_INFINITY, 0usize);
        for (k, &o) in offsets.iter().enumerate() {
            let j = i as isize + o;
            let c = txwt[k] + if j >= 0 { cum[j as usize] } else { 0.0 };
            if c > best.0 {
                best = (c, k);
            }
        }
        cum[i] = local[i] + best.0;
        if first && local[i] < thresh {
            back[i] = -1;
        } else {
            back[i] = i as isize + offsets[best.1];
            first = false;
        }
    }

    // Last beat: the last local max of the cumulative score above half the median local max.
    let is_max = |i: usize| {
        let prev = if i == 0 { cum[0] } else { cum[i - 1] };
        let next = if i + 1 == n { cum[i] } else { cum[i + 1] };
        cum[i] > prev && cum[i] >= next
    };
    let mut maxima: Vec<f64> = (0..n).filter(|&i| is_max(i)).map(|i| cum[i]).collect();
    let med = median(&mut maxima);
    let Some(last) = (0..n).rev().find(|&i| is_max(i) && 2.0 * cum[i] > med) else {
        return Vec::new();
    };
    let mut beats = vec![last];
    while let Some(&b) = beats.last() {
        let prev = back[b];
        if prev < 0 {
            break;
        }
        beats.push(prev as usize);
    }
    beats.reverse();

    // Trim weak beats at the ends.
    let at: Vec<f64> = beats.iter().map(|&b| local[b]).collect();
    let smooth = convolve_same(&at, &[0.0, 0.5, 1.0, 0.5, 0.0]);
    let thresh = 0.5 * (smooth.iter().map(|x| x * x).sum::<f64>() / smooth.len() as f64).sqrt();
    let valid: Vec<usize> = (0..smooth.len()).filter(|&k| smooth[k] > thresh).collect();
    match (valid.first(), valid.last()) {
        (Some(&a), Some(&b)) => beats[a..b].to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clicks (5 ms decaying noise bursts) at `bpm` from `t0`, with quieter off-beat ticks.
    fn clicks(bpm: f64, t0: f64, secs: f64) -> (Vec<f32>, Vec<f64>) {
        let sr = f64::from(RATE);
        let mut y = vec![0.0f32; (secs * sr) as usize];
        let mut truth = Vec::new();
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f64::from(seed >> 8) / f64::from(1u32 << 24)) * 2.0 - 1.0
        };
        let period = 60.0 / bpm;
        let mut k = 0;
        loop {
            let t = t0 + f64::from(k) * period / 2.0;
            if t > secs - 0.1 {
                break;
            }
            let amp = if k % 2 == 0 { 0.8 } else { 0.15 };
            if k % 2 == 0 {
                truth.push(t);
            }
            let s0 = (t * sr) as usize;
            for i in 0..(0.03 * sr) as usize {
                let e = (-(i as f64) / (0.005 * sr)).exp();
                y[s0 + i] += (amp * e * noise()) as f32;
            }
            k += 1;
        }
        (y, truth)
    }

    #[test]
    fn click_track() {
        for bpm in [96.0, 120.0, 128.0] {
            let (y, truth) = clicks(bpm, 0.31, 30.0);
            let b = analyse(&y, RATE);
            assert!(
                (b.tempo - bpm).abs() / bpm < 0.03,
                "{bpm}: tempo {}",
                b.tempo
            );
            assert!(
                b.beats.len() >= truth.len() - 4,
                "{bpm}: {} beats",
                b.beats.len()
            );
            let frame = 1.0 / 24.0;
            for t in &b.beats {
                let d = truth
                    .iter()
                    .map(|x| (x - t).abs())
                    .fold(f64::INFINITY, f64::min);
                assert!(d < frame, "{bpm}: beat {t} is {d} s off");
            }
        }
    }

    #[test]
    fn silence_has_no_beats() {
        let b = analyse(&vec![0.0; RATE as usize * 3], RATE);
        assert_eq!(b.beats, Vec::<f64>::new());
        assert!((b.duration - 3.0).abs() < 1e-9);
    }

    #[test]
    fn rounding() {
        assert!((round_even(2.5) - 2.0).abs() < 1e-12);
        assert!((round_even(3.5) - 4.0).abs() < 1e-12);
        assert!((round_even(-2.5) + 2.0).abs() < 1e-12);
    }

    #[test]
    fn cache_round_trip() {
        let dir = std::env::temp_dir().join(format!("ffcine-beats-{}", std::process::id()));
        let wav = dir.join("clicks.wav");
        std::fs::create_dir_all(&dir).unwrap();
        let (y, _) = clicks(120.0, 0.5, 8.0);
        crate::Audio::mono(RATE, y).write_wav(&wav).unwrap();
        let a = cached(&wav, &dir.join("cache")).unwrap();
        let entries = std::fs::read_dir(dir.join("cache")).unwrap().count();
        assert_eq!(entries, 1);
        assert_eq!(cached(&wav, &dir.join("cache")).unwrap(), a);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
