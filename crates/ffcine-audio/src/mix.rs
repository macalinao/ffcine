//! The per-timeline mix, after the prototype's `build_mix`: every audio clip loaded (clean
//! chain applied), drift-corrected, R128-levelled, faded and summed into its bus; music ducked
//! under dialogue and punchline SFX, ambience a little under dialogue; buses summed to the
//! master, brought to the target loudness under a 4x-oversampled true-peak limiter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ebur128::{EbuR128, Mode};
use ffcine_core::{Error, Result};
use ffcine_timeline::{HasMetadata, MixParams, Timeline, track_kind};
use serde::Serialize;

use crate::buffer::{Audio, db_to_lin, lin_to_db};
use crate::decode::{self, Request};
use crate::dsp::{limit, warp};
use crate::place::{Placed, layout};
use crate::schema::{self, Bus, Clean, Level, LevelMode, Loop, Pan, Param, SyncInfo};

/// Master loudness: -14 LUFS, true peak -1 dBTP (YouTube/Instagram).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Master {
    /// Integrated loudness target; `None` leaves the sum as is (still limited).
    pub lufs: Option<f64>,
    pub true_peak: f64,
    /// The limiter sits this far under `true_peak`: the AAC encode overshoots by 1.5-2 dB.
    pub headroom: f64,
    /// Fade-out at the end of the timeline (s).
    pub fade_out: f64,
}

impl Default for Master {
    fn default() -> Self {
        Self {
            lufs: Some(-14.0),
            true_peak: -1.0,
            headroom: 2.0,
            fade_out: 0.6,
        }
    }
}

/// What clips get when their own effects don't say (`project.mix`, [`MixParams`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClipDefaults {
    /// R128 targets (LUFS) of `dialogue` and `nat` clips.
    pub dialogue_lufs: f64,
    pub nat_lufs: f64,
    /// Music dip under `dialogue` and `nat` clips (dB).
    pub duck_db: f64,
    pub nat_duck_db: f64,
    /// Clean-chain strength of dialogue-bus clips without `ffcine.audio.clean/1`, by mode.
    pub clean: f64,
    pub clean_nat: f64,
    /// High-pass of that chain (Hz).
    pub highpass: Option<f64>,
}

impl Default for ClipDefaults {
    fn default() -> Self {
        Self {
            dialogue_lufs: -18.0,
            nat_lufs: -22.0,
            duck_db: -14.0,
            nat_duck_db: -7.0,
            clean: 0.6,
            clean_nat: 0.0,
            highpass: Some(80.0),
        }
    }
}

/// Everything about the mix but the clips. Serialises to its recipe (for cache keys), without
/// `base`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixOptions {
    pub rate: u32,
    pub master: Master,
    pub defaults: ClipDefaults,
    /// Relative media paths resolve against this (the timeline's directory).
    #[serde(skip)]
    pub base: Option<PathBuf>,
    /// With no audio tracks, use the first video track's clip audio as dialogue.
    pub video_audio: bool,
    /// Clamp of the per-clip R128 gain (dB).
    pub gain_range: (f64, f64),
    /// Crossfade at dialogue cuts (s): each dialogue clip reads half of it past either end.
    pub xfade: f64,
    /// Music duck envelope under dialogue: look-ahead and hold (s).
    pub duck_attack: f64,
    pub duck_release: f64,
    /// Duck only while the dialogue bus is actually loud (10 ms RMS over `duck_threshold`
    /// dBFS), not for the whole span of each dialogue clip.
    pub sidechain: bool,
    pub duck_threshold: f64,
    /// Mix length (s); default: the end of the longest track. A render passes the picture's
    /// length, so the master fade-out ends on its last frame.
    pub length: Option<f64>,
}

impl Default for MixOptions {
    fn default() -> Self {
        Self {
            rate: 48000,
            master: Master::default(),
            defaults: ClipDefaults::default(),
            base: None,
            video_audio: true,
            gain_range: (-10.0, 14.0),
            xfade: 0.125,
            duck_attack: 0.25,
            duck_release: 0.45,
            sidechain: false,
            duck_threshold: -40.0,
            length: None,
        }
    }
}

impl MixOptions {
    /// The defaults with the timeline's `project.mix` over them, and `base` its directory
    /// (`timeline_dir`).
    pub fn for_timeline(timeline: &Timeline, timeline_dir: Option<&Path>) -> Result<Self> {
        let mut o = Self {
            base: timeline_dir.map(Path::to_owned),
            ..Self::default()
        };
        if let Some(p) = timeline.project().transpose()? {
            o.apply(&p.mix);
        }
        Ok(o)
    }

    /// Sets what `m` has.
    pub fn apply(&mut self, m: &MixParams) {
        let set = |dst: &mut f64, v: Option<f64>| {
            if let Some(v) = v {
                *dst = v;
            }
        };
        if let Some(l) = m.lufs {
            self.master.lufs = Some(l);
        }
        if m.normalize == Some(false) {
            self.master.lufs = None;
        }
        set(&mut self.master.true_peak, m.true_peak);
        set(&mut self.master.headroom, m.headroom);
        set(&mut self.master.fade_out, m.fade_out);
        let d = &mut self.defaults;
        set(&mut d.dialogue_lufs, m.dialogue_lufs);
        set(&mut d.nat_lufs, m.nat_lufs);
        set(&mut d.duck_db, m.duck_db);
        set(&mut d.nat_duck_db, m.nat_duck_db);
        set(&mut d.clean, m.clean);
        set(&mut d.clean_nat, m.clean_nat);
        if m.highpass.is_some() {
            d.highpass = m.highpass.filter(|f| *f > 0.0);
        }
        if let Some([lo, hi]) = m.gain_range {
            self.gain_range = (lo, hi);
        }
        set(&mut self.xfade, m.xfade);
        set(&mut self.duck_attack, m.duck_attack);
        set(&mut self.duck_release, m.duck_release);
        set(&mut self.duck_threshold, m.duck_threshold);
        if let Some(s) = m.sidechain {
            self.sidechain = s;
        }
    }
}

/// The rendered mix.
#[derive(Debug, Clone)]
pub struct Mix {
    /// Stereo master at `MixOptions::rate`.
    pub master: Audio,
    /// Per-bus stems (ducked, before the master gain and limiter), only buses with clips.
    pub buses: Vec<(Bus, Audio)>,
    /// Measured on the master.
    pub lufs: f64,
    pub true_peak: f64,
}

/// Duck envelope rate (Hz).
const ENV_RATE: f64 = 100.0;

/// Renders the timeline's audio on its own (no video), so it can be remuxed onto an existing
/// render without re-encoding.
pub fn render_audio(timeline: &Timeline, opts: &MixOptions) -> Result<Mix> {
    let rate = opts.rate;
    let (clips, end) = sources(timeline, opts.video_audio);
    let length = opts.length.unwrap_or(end).max(0.0);
    let frames = (length * f64::from(rate)).round() as usize;
    let blocks = (length * ENV_RATE).ceil() as usize + 1;

    // Music dips: under dialogue clips (spans), and briefly under punchline SFX.
    let mut dia_env = vec![0.0f32; blocks];
    let mut sfx_env = vec![0.0f32; blocks];
    let mut buses: Vec<(Bus, Audio)> = Vec::new();
    let mut music_lufs: HashMap<PathBuf, Option<f64>> = HashMap::new();
    for bus in [Bus::Dialogue, Bus::Sfx, Bus::Ambience, Bus::Music] {
        let mine: Vec<&Placed<'_>> = clips.iter().filter(|c| c.0 == bus).map(|c| &c.1).collect();
        if mine.is_empty() {
            continue;
        }
        let mut out = Audio::silence(rate, 2, frames);
        for p in mine {
            let level = schema::clip_effect::<Level>(p.clip, schema::LEVEL)?.unwrap_or_default();
            if !level.audible(bus) {
                continue;
            }
            let duck = level.duck_in(bus, &opts.defaults);
            let (t0, t1) = match bus {
                Bus::Sfx => (p.start - 0.05, p.start + p.duration.min(2.5)),
                _ => (p.start, p.end()),
            };
            if duck < 0.0 {
                let env = if bus == Bus::Sfx {
                    &mut sfx_env
                } else {
                    &mut dia_env
                };
                dip(env, t0, t1, duck as f32);
            }
            let (a, at) = render_clip(p, bus, &level, opts, &mut music_lufs)?;
            out.mix_in(&a, at);
        }
        out.set_frames(frames);
        if bus == Bus::Dialogue && opts.sidechain {
            let on = presence(&out, opts.duck_threshold, blocks);
            for (e, o) in dia_env.iter_mut().zip(on) {
                *e *= o;
            }
        }
        buses.push((bus, out));
    }

    let dia_env = smooth_db_envelope(&dia_env, opts.duck_attack, opts.duck_release);
    let sfx_env = smooth_db_envelope(&sfx_env, 0.04, 0.35);
    for (bus, a) in &mut buses {
        let env: Vec<f32> = match bus {
            Bus::Music => dia_env.iter().zip(&sfx_env).map(|(d, s)| d + s).collect(),
            Bus::Ambience => dia_env.iter().map(|d| d.max(-6.0)).collect(),
            _ => continue,
        };
        apply_env(a, &env);
    }

    let mut sum = Audio::silence(rate, 2, frames);
    for (_, b) in &buses {
        sum.mix_in(b, 0);
    }
    let k = ((opts.master.fade_out * f64::from(rate)) as usize).min(frames);
    for c in &mut sum.channels {
        for (i, s) in c[frames - k..].iter_mut().enumerate() {
            *s *= 1.0 - i as f32 / k as f32;
        }
    }
    let (master, lufs, true_peak) = master_bus(&sum, &opts.master)?;
    Ok(Mix {
        master,
        buses,
        lufs,
        true_peak,
    })
}

/// The clips the mix reads, each with its bus, and the end of the longest track (s): every
/// enabled audio track's clips, or with none and `video_audio`, the first video track's (camera
/// audio, as dialogue).
pub fn sources(timeline: &Timeline, video_audio: bool) -> (Vec<(Bus, Placed<'_>)>, f64) {
    let mut length = 0.0f64;
    let mut clips: Vec<(Bus, Placed<'_>)> = Vec::new();
    let tracks: Vec<_> = timeline.track_list().collect();
    for t in &tracks {
        let (placed, end) = layout(t);
        length = length.max(end);
        if t.kind == track_kind::AUDIO && t.enabled {
            let bus = Bus::of_track(t);
            clips.extend(placed.into_iter().map(|p| (bus, p)));
        }
    }
    if clips.is_empty()
        && video_audio
        && let Some(v) = tracks.iter().find(|t| t.kind == track_kind::VIDEO)
    {
        clips.extend(layout(v).0.into_iter().map(|p| (Bus::Dialogue, p)));
    }
    (clips, length)
}

/// Lowers `env` (dB at [`ENV_RATE`]) to at most `db` over `t0..t1` (s).
fn dip(env: &mut [f32], t0: f64, t1: f64, db: f32) {
    let a = ((t0 * ENV_RATE).max(0.0) as usize).min(env.len());
    let b = ((t1 * ENV_RATE).max(0.0) as usize).min(env.len());
    for e in &mut env[a..b] {
        *e = e.min(db);
    }
}

/// The prototype's `smooth_db_envelope`: a dip starts `attack` early (look-ahead), holds for
/// `release / 2` after it ends, then a Hann smoothing over `(attack + release) / 2`.
pub fn smooth_db_envelope(points: &[f32], attack: f64, release: f64) -> Vec<f32> {
    let n = points.len();
    let la = ((attack * ENV_RATE) as usize).max(1);
    let out: Vec<f32> = (0..n)
        .map(|i| {
            points[i..(i + la).min(n)]
                .iter()
                .copied()
                .fold(points[i], f32::min)
        })
        .collect();
    let rl = ((release * ENV_RATE * 0.5) as usize).max(1);
    let rel: Vec<f32> = (0..n)
        .map(|i| {
            out[i.saturating_sub(rl)..=i]
                .iter()
                .copied()
                .fold(out[i], f32::min)
        })
        .collect();
    let len = ((f64::midpoint(attack, release) * ENV_RATE) as usize).max(3) | 1;
    let w: Vec<f32> = (0..len)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (len - 1) as f32).cos())
        .collect();
    let sum: f32 = w.iter().sum();
    let pad = len / 2;
    (0..n)
        .map(|i| {
            w.iter()
                .enumerate()
                .map(|(k, wk)| rel[(i + k).saturating_sub(pad).min(n - 1)] * wk)
                .sum::<f32>()
                / sum
        })
        .collect()
}

/// Multiplies `a` by `10^(env/20)`, `env` at [`ENV_RATE`] interpolated to samples.
fn apply_env(a: &mut Audio, env: &[f32]) {
    let per = f64::from(a.rate) / ENV_RATE;
    let lin: Vec<f32> = env
        .iter()
        .map(|d| db_to_lin(f64::from(*d)) as f32)
        .collect();
    let last = lin.len().saturating_sub(1);
    for c in &mut a.channels {
        for (i, s) in c.iter_mut().enumerate() {
            let x = i as f64 / per;
            let b = (x as usize).min(last);
            let f = (x - b as f64) as f32;
            *s *= lin[b] + (lin[(b + 1).min(last)] - lin[b]) * f;
        }
    }
}

#[allow(clippy::too_many_lines)] // one clip, start to finish
/// Loads, cleans, drift-corrects, levels and fades one clip. Returns the stereo audio and the
/// frame it starts at.
fn render_clip(
    placed: &Placed<'_>,
    bus: Bus,
    level: &Level,
    opts: &MixOptions,
    music_lufs: &mut HashMap<PathBuf, Option<f64>>,
) -> Result<(Audio, usize)> {
    let rate = opts.rate;
    let fr = f64::from(rate);
    let mode = level.mode(bus);
    // Dialogue reads half the crossfade past each end (the prototype's handles).
    let mut p = placed.clone();
    let hh = if bus == Bus::Dialogue {
        opts.xfade / 2.0
    } else {
        0.0
    };
    p.start -= hh;
    p.duration += 2.0 * hh;
    p.source_start -= hh * p.speed;
    let head = (-p.start).max(0.0);
    let at = (p.start.max(0.0) * fr).round() as usize;
    let n = ((p.duration - head) * fr).round().max(0.0) as usize;
    let Some(path) = p.path(opts.base.as_deref()) else {
        return Ok((Audio::silence(rate, 2, n), at));
    };
    // Freeze frames, and stills or silent clips standing in for camera audio, are silent.
    if p.speed == 0.0 || !decode::has_audio(&path) {
        return Ok((Audio::silence(rate, 2, n), at));
    }
    let clean = schema::clip_effect::<Clean>(p.clip, schema::CLEAN)?;
    let mut filter = match (clean, bus) {
        (Some(c), _) => c.filter_spec(mode),
        (None, Bus::Dialogue) => Clean::for_mode_in(mode, &opts.defaults).filter_spec(mode),
        (None, Bus::Ambience) => "highpass=f=60,lowpass=f=14000".into(),
        (None, _) => String::new(),
    };
    let drift = p
        .clip
        .ffcine_get::<SyncInfo>("sync")
        .and_then(Result::ok)
        .map_or(0.0, |s| s.drift_ppm);
    if (p.speed - 1.0).abs() > 1e-9 {
        filter = join(&filter, &atempo(p.speed));
    }
    let k = 1.0 + drift * 1e-6;
    let src_start = p.source_start + head * p.speed * k;
    let read = |start: f64, dur: f64| {
        decode::load(
            &path,
            &Request {
                start: start.max(0.0),
                duration: Some(dur),
                rate,
                channels: 2,
                filter: (!filter.is_empty()).then_some(filter.as_str()),
            },
        )
    };
    let lp = schema::clip_effect::<Loop>(p.clip, schema::LOOP)?;
    let mut a = if let Some(lp) = &lp {
        let lo = lp.r#in.unwrap_or(src_start);
        let hi = lp.out.unwrap_or(lo + p.duration + 1.0);
        let seg = read(lo, hi - lo)?;
        looped(&seg, n, (lp.crossfade * fr) as usize)
    } else {
        // atempo turns media time into record time; the drift is resampled here.
        let mut a = read(src_start, (n as f64 / fr) * k + 0.01)?;
        if (k - 1.0).abs() > 1e-12 {
            for c in &mut a.channels {
                *c = warp(c, 0.0, k, n);
            }
        }
        a.set_frames(n);
        a
    };

    if let Some(target) = level.target_in(bus, &opts.defaults) {
        // Music levels on the whole track, as the prototype did, so quiet intros stay quiet.
        let l = if bus == Bus::Music && lp.is_none() {
            if !music_lufs.contains_key(&path) {
                let whole = decode::load(
                    &path,
                    &Request {
                        rate,
                        channels: 2,
                        ..Request::default()
                    },
                )?;
                music_lufs.insert(path.clone(), loudness(&whole)?);
            }
            music_lufs.get(&path).copied().flatten()
        } else {
            loudness(&a)?
        };
        if let Some(l) = l.filter(|l| *l > -60.0) {
            let g = target - l;
            a.gain_db(if bus == Bus::Dialogue {
                g.clamp(opts.gain_range.0, opts.gain_range.1)
            } else {
                g
            });
        }
    }
    if mode == LevelMode::Sfx {
        sfx_normalise(&mut a, level.ref_dbfs.unwrap_or(-18.0));
    }
    let pan = schema::clip_effect::<Pan>(p.clip, schema::PAN)?.map(|p| p.pan);
    automate(&mut a, &level.gain_db, pan.as_ref(), src_start, p.speed);

    let default_fade = match bus {
        Bus::Dialogue => opts.xfade,
        Bus::Ambience => 0.8,
        _ => 0.01,
    };
    let fi = if p.xfade_in > 0.0 {
        p.xfade_in
    } else {
        level.fade_in.unwrap_or(default_fade)
    };
    let fo = if p.xfade_out > 0.0 {
        p.xfade_out
    } else {
        level.fade_out.unwrap_or(default_fade)
    };
    fade(&mut a, fi - head, fo, p.xfade_in > 0.0, p.xfade_out > 0.0);
    Ok((a, at))
}

/// The prototype's `sfx_wav`: the loudest 100 ms (mono RMS) at `ref_dbfs`, peaks under -1 dBFS.
fn sfx_normalise(a: &mut Audio, ref_dbfs: f64) {
    let mono = a.to_mono();
    let n = (a.rate as usize / 10).max(1);
    let rms = if mono.len() > n {
        let mut acc: f64 = mono[..n].iter().map(|s| f64::from(*s).powi(2)).sum();
        let mut best = acc;
        for i in n..mono.len() {
            acc += f64::from(mono[i]).powi(2) - f64::from(mono[i - n]).powi(2);
            best = best.max(acc);
        }
        (best.max(0.0) / n as f64).sqrt()
    } else {
        (mono.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / mono.len().max(1) as f64).sqrt()
    };
    let mut g = db_to_lin(ref_dbfs) / rms.max(1e-6);
    let pk = a
        .channels
        .iter()
        .flatten()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    let lim = db_to_lin(-1.0);
    if f64::from(pk) * g > lim {
        g = lim / f64::from(pk);
    }
    a.gain_db(lin_to_db(g));
}

/// Applies `gain_db` and the stereo balance `pan`, either of which may be keyframed over source
/// time (`t0` at the first sample, advancing `speed` per record second).
fn automate(a: &mut Audio, gain_db: &Param, pan: Option<&Param>, t0: f64, speed: f64) {
    let balance = |p: f64| {
        let p = p.clamp(-1.0, 1.0);
        [(1.0 - p).min(1.0), (1.0 + p).min(1.0)]
    };
    let still = gain_db.is_constant() && pan.is_none_or(Param::is_constant);
    let rate = f64::from(a.rate);
    let gains = |t: f64| {
        let g = db_to_lin(gain_db.at(t));
        let b = pan.map_or([1.0, 1.0], |p| balance(p.at(t)));
        [(g * b[0]) as f32, (g * b[1]) as f32]
    };
    if still {
        let g = gains(t0);
        for (c, ch) in a.channels.iter_mut().enumerate() {
            for s in ch {
                *s *= g[c.min(1)];
            }
        }
        return;
    }
    // Keyframes: evaluate every 64 samples (1.3 ms), linear in between.
    let n = a.frames();
    let step = 64;
    let mut prev = gains(t0);
    for start in (0..n).step_by(step) {
        let next = gains(t0 + (start + step) as f64 / rate * speed);
        let len = step.min(n - start);
        for (c, ch) in a.channels.iter_mut().enumerate() {
            let (g0, g1) = (prev[c.min(1)], next[c.min(1)]);
            for (i, s) in ch[start..start + len].iter_mut().enumerate() {
                *s *= g0 + (g1 - g0) * i as f32 / step as f32;
            }
        }
        prev = next;
    }
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{a},{b}"),
    }
}

/// `atempo` chain for any speed (each stage takes 0.5..=2 for best quality).
fn atempo(speed: f64) -> String {
    let mut s = speed;
    let mut f = Vec::new();
    while s > 2.0 {
        f.push("atempo=2".to_owned());
        s /= 2.0;
    }
    while s < 0.5 {
        f.push("atempo=0.5".to_owned());
        s /= 0.5;
    }
    f.push(format!("atempo={s:.6}"));
    f.join(",")
}

/// `src` repeated to `n` frames with `x`-frame equal-power crossfades at the seams.
fn looped(src: &Audio, n: usize, x: usize) -> Audio {
    let len = src.frames();
    let x = x.min(len / 2);
    if len >= n || len == 0 || x == 0 {
        let mut a = src.clone();
        a.set_frames(n);
        return a;
    }
    let mut out = src.clone();
    while out.frames() < n {
        let at = out.frames() - x;
        let mut next = src.clone();
        for c in &mut next.channels {
            for (i, s) in c.iter_mut().take(x).enumerate() {
                *s *= (std::f32::consts::FRAC_PI_2 * i as f32 / x as f32).sin();
            }
        }
        for c in &mut out.channels {
            let l = c.len();
            for (i, s) in c[l - x..].iter_mut().enumerate() {
                *s *= (std::f32::consts::FRAC_PI_2 * i as f32 / x as f32).cos();
            }
        }
        out.mix_in(&next, at);
    }
    out.set_frames(n);
    out
}

/// Fades the ends: equal-power for crossfades, linear otherwise.
fn fade(a: &mut Audio, fade_in: f64, fade_out: f64, eq_in: bool, eq_out: bool) {
    let rate = f64::from(a.rate);
    let n = a.frames();
    let fi = ((fade_in.max(0.0) * rate) as usize).min(n);
    let fo = ((fade_out.max(0.0) * rate) as usize).min(n);
    let curve = |x: f32, eq: bool| {
        if eq {
            (x * std::f32::consts::FRAC_PI_2).sin()
        } else {
            x
        }
    };
    for c in &mut a.channels {
        for (i, s) in c.iter_mut().take(fi).enumerate() {
            *s *= curve(i as f32 / fi as f32, eq_in);
        }
        for i in 0..fo {
            c[n - 1 - i] *= curve(i as f32 / fo as f32, eq_out);
        }
    }
}

fn r128_err(e: ebur128::Error) -> Error {
    Error::Pipeline(format!("ebur128: {e}"))
}

fn meter(a: &Audio) -> Result<EbuR128> {
    let mut m = EbuR128::new(
        u32::try_from(a.channels.len()).unwrap_or(2),
        a.rate,
        Mode::I | Mode::TRUE_PEAK,
    )
    .map_err(r128_err)?;
    let planes: Vec<&[f32]> = a.channels.iter().map(Vec::as_slice).collect();
    m.add_frames_planar_f32(&planes).map_err(r128_err)?;
    Ok(m)
}

/// Integrated loudness (LUFS); `None` for silence or under 400 ms.
pub fn loudness(a: &Audio) -> Result<Option<f64>> {
    if a.frames() < (a.rate as usize * 2) / 5 {
        return Ok(None);
    }
    let l = meter(a)?.loudness_global().map_err(r128_err)?;
    Ok(l.is_finite().then_some(l))
}

/// Integrated loudness and true peak (dBTP).
pub fn measure(a: &Audio) -> Result<(f64, f64)> {
    let m = meter(a)?;
    let l = m.loudness_global().map_err(r128_err)?;
    let tp = (0..a.channels.len())
        .map(|c| m.true_peak(u32::try_from(c).unwrap_or(0)).unwrap_or(0.0))
        .fold(0.0f64, f64::max);
    Ok((l, lin_to_db(tp)))
}

/// Gain to the target and limit; up to 4 passes until within 0.25 LU and under the ceiling
/// (as `ebur128` measures it; the limiter's own estimate can read a little low on noise).
fn master_bus(sum: &Audio, m: &Master) -> Result<(Audio, f64, f64)> {
    let ceiling = m.true_peak - m.headroom.max(0.0);
    let mut lim = ceiling;
    let mut gain = match (m.lufs, loudness(sum)?) {
        (Some(t), Some(l)) => t - l,
        _ => 0.0,
    };
    let mut out = sum.clone();
    let (mut l, mut tp) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for _ in 0..4 {
        out = sum.clone();
        out.gain_db(gain);
        limit(&mut out.channels, out.rate, lim, 0.002, 0.06);
        if out.frames() == 0 {
            break;
        }
        (l, tp) = measure(&out)?;
        let loud_ok = m
            .lufs
            .is_none_or(|t| !l.is_finite() || (t - l).abs() <= 0.25);
        if loud_ok && tp <= ceiling + 0.05 {
            break;
        }
        if tp > ceiling {
            lim -= tp - ceiling + 0.05;
        }
        if let Some(t) = m.lufs.filter(|_| l.is_finite()) {
            gain += t - l;
        }
    }
    Ok((out, l, tp))
}

/// 1 where the bus is louder than `threshold` dBFS (10 ms RMS), else 0, at [`ENV_RATE`].
fn presence(a: &Audio, threshold: f64, blocks: usize) -> Vec<f32> {
    let block = (f64::from(a.rate) / ENV_RATE) as usize;
    let n = a.frames();
    let thr = db_to_lin(threshold) as f32;
    (0..blocks)
        .map(|b| {
            let lo = (b * block).min(n);
            let hi = (lo + block).min(n);
            if hi == lo {
                return 0.0;
            }
            let ms = a
                .channels
                .iter()
                .map(|c| c[lo..hi].iter().map(|s| s * s).sum::<f32>())
                .sum::<f32>()
                / ((hi - lo) * a.channels.len()) as f32;
            if ms.sqrt() > thr { 1.0 } else { 0.0 }
        })
        .collect()
}

/// Writes the master and, with `stems`, one WAV per bus (`<stem dir>/<bus>.wav`).
pub fn write_mix(mix: &Mix, out: &Path, stems: Option<&Path>) -> Result<()> {
    mix.master.write_wav(out)?;
    if let Some(dir) = stems {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
        for (bus, a) in &mix.buses {
            a.write_wav(&dir.join(format!("{}.wav", bus.name())))?;
        }
    }
    Ok(())
}
