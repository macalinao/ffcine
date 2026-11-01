//! Recorder sync: lines up long, timecode-less recorder WAVs (Zoom H2essential) with the camera
//! clips' scratch audio by cross-correlation.
//!
//! 1. Both sides are decoded to 8 kHz mono and high-passed (150 Hz, wind and handling rumble).
//! 2. **Coarse**: the log-RMS envelope (5 ms hops, 200 Hz) of the clip is correlated against
//!    each recording's (FFT, normalised over the overlap, so clips that start before or run past
//!    the recording still match). Best peak wins; `margin` is how far it beats the runner-up
//!    more than 1 s away.
//! 3. **Fine**: up to 24 windows of 4 s across the clip are each correlated on the waveform
//!    within +-100 ms of the coarse lag, to sub-sample precision.
//! 4. A weighted line through the window offsets (outliers over 1 ms dropped) gives the offset
//!    at clip time 0 and the drift (recorder clock vs camera clock) in ppm. Clips under 20 s
//!    can't measure drift; they take the median drift of the other clips on the same recording.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ffcine_core::{Error, Result};
use ffcine_timeline::{
    Clip, ExternalReference, Gap, HasMetadata, Item, MediaReference, RationalTime, TimeRange,
    Timeline, Track, track_kind,
};

use crate::decode::{self, Request};
use crate::dsp::{Biquad, ncc, parabolic};
use crate::place::{self, media_path};
use crate::schema::{self, Bus, Clean, Level, SyncInfo};

/// Analysis sample rate.
pub const RATE: u32 = 8000;
/// Envelope hop (5 ms).
const HOP: usize = 40;

#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Matches below this are reported but not written.
    pub min_confidence: f64,
    /// Fine window length (s).
    pub window: f64,
    pub max_windows: usize,
    /// Fine search radius around the coarse lag (s).
    pub search: f64,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            min_confidence: 0.15,
            window: 4.0,
            max_windows: 24,
            search: 0.1,
        }
    }
}

/// A signal ready for matching.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// High-passed waveform at [`RATE`].
    pub signal: Vec<f32>,
    /// Standardised log-RMS envelope, one value per [`HOP`].
    pub envelope: Vec<f32>,
}

impl Prepared {
    /// From mono samples at [`RATE`].
    pub fn new(mut signal: Vec<f32>) -> Self {
        let hp = Biquad::highpass(f64::from(RATE), 150.0, std::f64::consts::FRAC_1_SQRT_2);
        hp.run(&mut signal);
        hp.run(&mut signal);
        let mut envelope: Vec<f32> = signal
            .chunks(HOP)
            .map(|c| {
                let ms = c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32;
                (ms + 1e-9).ln() * 0.5
            })
            .collect();
        let n = envelope.len().max(1) as f32;
        let mean = envelope.iter().sum::<f32>() / n;
        let sd = (envelope.iter().map(|e| (e - mean).powi(2)).sum::<f32>() / n)
            .sqrt()
            .max(1e-6);
        for e in &mut envelope {
            *e = (*e - mean) / sd;
        }
        Self { signal, envelope }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let a = decode::load(
            path,
            &Request {
                rate: RATE,
                channels: 1,
                ..Request::default()
            },
        )?;
        Ok(Self::new(a.to_mono()))
    }

    fn seconds(&self) -> f64 {
        self.signal.len() as f64 / f64::from(RATE)
    }
}

/// Where a clip sits in a recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// Index into the recordings.
    pub recording: usize,
    /// Recorder time (s) of clip time 0: recorder = offset + clip * (1 + `drift_ppm` / 1e6).
    pub offset: f64,
    pub drift_ppm: f64,
    /// Whether the drift was measured on this clip (else borrowed or 0).
    pub drift_measured: bool,
    /// Coarse envelope correlation peak (0..1).
    pub coarse: f64,
    /// 1 - runner-up / peak of the coarse correlation.
    pub margin: f64,
    /// Mean fine correlation of the inlier windows (0..1).
    pub fine: f64,
    pub windows: usize,
    pub inliers: usize,
    /// RMS residual of the inlier windows against the fitted line (ms).
    pub residual_ms: f64,
    /// `fine * margin * inliers / windows`.
    pub confidence: f64,
}

/// Finds `clip` in the best-matching recording.
pub fn align(clip: &Prepared, recordings: &[Prepared], opts: &SyncOptions) -> Option<Match> {
    let na = clip.envelope.len();
    if na < 20 {
        return None;
    }
    let min_overlap = (na / 2).max(200.min(na));
    let mut best: Option<(usize, f64, f64, f64)> = None; // (rec, lag s, peak, margin)
    for (ri, rec) in recordings.iter().enumerate() {
        let c = ncc(&clip.envelope, &rec.envelope, min_overlap);
        let Some((k, &peak)) = c.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else {
            continue;
        };
        if peak <= 0.0 {
            continue;
        }
        let guard = 200; // 1 s
        let second = c
            .iter()
            .enumerate()
            .filter(|(i, _)| i.abs_diff(k) > guard)
            .map(|(_, v)| *v)
            .fold(0.0f64, f64::max);
        let margin = (1.0 - second.max(0.0) / peak).clamp(0.0, 1.0);
        let lag = (k as f64 + parabolic(&c, k) - (na as f64 - 1.0)) * HOP as f64 / f64::from(RATE);
        if best.is_none_or(|b| peak * margin > b.2 * b.3) {
            best = Some((ri, lag, peak, margin));
        }
    }
    let mut found =
        best.map(|(ri, lag, peak, margin)| refine(clip, recordings, ri, lag, peak, margin, opts));
    // Steady sound (wind, water, traffic) has no envelope to lock onto: search on the waveform.
    if found.as_ref().is_none_or(|m| m.confidence < 0.3) {
        let wave = recordings
            .iter()
            .enumerate()
            .filter_map(|(ri, r)| coarse_waveform(clip, r).map(|(lag, p, m)| (ri, lag, p, m)))
            .max_by(|a, b| (a.2 * a.3).total_cmp(&(b.2 * b.3)));
        if let Some((ri, lag, peak, margin)) = wave {
            let m = refine(clip, recordings, ri, lag, peak, margin, opts);
            if found.as_ref().is_none_or(|f| m.confidence > f.confidence) {
                found = Some(m);
            }
        }
    }
    found
}

/// Fine stage and fit around a coarse lag.
fn refine(
    clip: &Prepared,
    recordings: &[Prepared],
    ri: usize,
    lag: f64,
    coarse: f64,
    margin: f64,
    opts: &SyncOptions,
) -> Match {
    let fine = fine_windows(clip, &recordings[ri], lag, opts);
    let (offset, drift, measured, fine_mean, inliers, residual) = fit(&fine, lag);
    let windows = fine.len().max(1);
    let confidence = fine_mean * margin * inliers as f64 / windows as f64;
    Match {
        recording: ri,
        offset,
        drift_ppm: drift * 1e6,
        drift_measured: measured,
        coarse,
        margin,
        fine: fine_mean,
        windows: fine.len(),
        inliers,
        residual_ms: residual * 1e3,
        confidence,
    }
}

/// Coarse search on the waveform: the loudest 8 s of the clip against the whole recording, in
/// FFT blocks. Returns `(lag s, peak, margin)`; the margin is against the best other second.
fn coarse_waveform(clip: &Prepared, rec: &Prepared) -> Option<(f64, f64, f64)> {
    let r = RATE as usize;
    let w = (8 * r).min(clip.signal.len());
    if w < r / 2 || rec.signal.len() < w {
        return None;
    }
    // Loudest window, on a 1 s grid.
    let energy = |s: usize| clip.signal[s..s + w].iter().map(|x| x * x).sum::<f32>();
    let ws = (0..=clip.signal.len() - w)
        .step_by(r)
        .max_by(|a, b| energy(*a).total_cmp(&energy(*b)))?;
    let a = &clip.signal[ws..ws + w];
    let block = (1usize << 20).max(4 * w);
    let mut buckets = vec![(f64::MIN, 0.0f64); rec.signal.len() / r + 1];
    let mut start = 0;
    while start + w <= rec.signal.len() {
        let end = (start + block).min(rec.signal.len());
        let seg = &rec.signal[start..end];
        let c = ncc(a, seg, w);
        let full = &c[w - 1..=w - 1 + (seg.len() - w)];
        for (k, &v) in full.iter().enumerate() {
            let b = &mut buckets[(start + k) / r];
            if v > b.0 {
                *b = (v, (start + k) as f64 + parabolic(full, k));
            }
        }
        if end == rec.signal.len() {
            break;
        }
        start = end - w + 1;
    }
    let (bi, &(peak, pos)) = buckets
        .iter()
        .enumerate()
        .max_by(|x, y| x.1.0.total_cmp(&y.1.0))?;
    if peak <= 0.0 {
        return None;
    }
    let second = buckets
        .iter()
        .enumerate()
        .filter(|(i, _)| i.abs_diff(bi) > 1)
        .map(|(_, b)| b.0)
        .fold(0.0f64, f64::max);
    let margin = (1.0 - second.max(0.0) / peak).clamp(0.0, 1.0);
    Some(((pos - ws as f64) / f64::from(RATE), peak, margin))
}

/// `(clip time of window centre, offset recorder - clip, score)` per usable window.
fn fine_windows(
    clip: &Prepared,
    rec: &Prepared,
    lag: f64,
    opts: &SyncOptions,
) -> Vec<(f64, f64, f64)> {
    let r = f64::from(RATE);
    let len = clip.signal.len();
    let w = ((opts.window * r) as usize).min(len);
    if w < RATE as usize / 2 {
        return Vec::new();
    }
    let n = (len / w).clamp(1, opts.max_windows);
    let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt();
    let floor = rms(&clip.signal) * 0.05;
    let m = (opts.search * r) as isize;
    let mut out = Vec::new();
    for i in 0..n {
        let s = if n == 1 { 0 } else { i * (len - w) / (n - 1) };
        let a = &clip.signal[s..s + w];
        if rms(a) <= floor {
            continue;
        }
        let p = s as isize + (lag * r).round() as isize;
        let lo = (p - m).max(0);
        let hi = (p + w as isize + m).min(rec.signal.len() as isize);
        if hi - lo < w as isize {
            continue;
        }
        let seg = &rec.signal[lo as usize..hi as usize];
        let c = ncc(a, seg, w);
        // Lags 0..=seg.len()-w (full overlap) sit at index w-1 onwards.
        let full = &c[w - 1..=w - 1 + (seg.len() - w)];
        let Some((k, &v)) = full.iter().enumerate().max_by(|x, y| x.1.total_cmp(y.1)) else {
            continue;
        };
        let pos = lo as f64 + k as f64 + parabolic(full, k);
        let t = (s as f64 + w as f64 / 2.0) / r;
        out.push((t, (pos - s as f64) / r, v));
    }
    out
}

/// Weighted line `offset(t) = a + b t` through the windows, with outlier rejection.
/// Returns `(a, b, b measured, mean score, inliers, rms residual s)`.
fn fit(points: &[(f64, f64, f64)], coarse: f64) -> (f64, f64, bool, f64, usize, f64) {
    let mut pts: Vec<(f64, f64, f64)> = points.iter().copied().filter(|p| p.2 > 0.1).collect();
    if pts.is_empty() {
        return (coarse, 0.0, false, 0.0, 0, 0.0);
    }
    // Start from the median offset so one bad window can't drag the first fit.
    let mut offs: Vec<f64> = pts.iter().map(|p| p.1).collect();
    offs.sort_by(f64::total_cmp);
    let med = offs[offs.len() / 2];
    pts.retain(|p| (p.1 - med).abs() < 0.005);
    for _ in 0..3 {
        let line = line_fit(&pts, med);
        let before = pts.len();
        pts.retain(|p| (p.1 - (line.0 + line.1 * p.0)).abs() < 0.001);
        if pts.len() == before || pts.len() < 2 {
            break;
        }
    }
    if pts.is_empty() {
        return (med, 0.0, false, 0.0, 0, 0.0);
    }
    let line = line_fit(&pts, med);
    let n = pts.len() as f64;
    let score = pts.iter().map(|p| p.2).sum::<f64>() / n;
    let res = (pts
        .iter()
        .map(|p| (p.1 - (line.0 + line.1 * p.0)).powi(2))
        .sum::<f64>()
        / n)
        .sqrt();
    (line.0, line.1, line.2, score.min(1.0), pts.len(), res)
}

fn line_fit(pts: &[(f64, f64, f64)], fallback: f64) -> (f64, f64, bool) {
    let sw: f64 = pts.iter().map(|p| p.2).sum();
    if sw <= 0.0 {
        return (fallback, 0.0, false);
    }
    let mt = pts.iter().map(|p| p.2 * p.0).sum::<f64>() / sw;
    let mo = pts.iter().map(|p| p.2 * p.1).sum::<f64>() / sw;
    let (tmin, tmax) = pts
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
    if pts.len() < 3 || tmax - tmin < 20.0 {
        return (mo, 0.0, false);
    }
    let stt: f64 = pts.iter().map(|p| p.2 * (p.0 - mt).powi(2)).sum();
    let sto: f64 = pts.iter().map(|p| p.2 * (p.0 - mt) * (p.1 - mo)).sum();
    let b = sto / stt;
    (mo - b * mt, b, true)
}

/// One camera clip's result.
#[derive(Debug, Clone)]
pub struct ClipSync {
    pub video: PathBuf,
    pub seconds: f64,
    pub found: Option<Match>,
}

/// Aligns every video against every recording. Clips without a measured drift take the median
/// of the measured ones on the same recording.
pub fn sync_files(
    videos: &[PathBuf],
    recordings: &[PathBuf],
    opts: &SyncOptions,
) -> Result<Vec<ClipSync>> {
    let recs = recordings
        .iter()
        .map(|p| Prepared::load(p))
        .collect::<Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for v in videos {
        let clip = Prepared::load(v)?;
        out.push(ClipSync {
            video: v.clone(),
            seconds: clip.seconds(),
            found: align(&clip, &recs, opts),
        });
    }
    borrow_drift(&mut out);
    Ok(out)
}

fn borrow_drift(clips: &mut [ClipSync]) {
    let recs: Vec<usize> = clips
        .iter()
        .filter_map(|c| c.found.as_ref().map(|m| m.recording))
        .collect();
    for r in recs {
        let mut d: Vec<f64> = clips
            .iter()
            .filter_map(|c| c.found.as_ref())
            .filter(|m| m.recording == r && m.drift_measured)
            .map(|m| m.drift_ppm)
            .collect();
        if d.is_empty() {
            continue;
        }
        d.sort_by(f64::total_cmp);
        let med = d[d.len() / 2];
        for m in clips.iter_mut().filter_map(|c| c.found.as_mut()) {
            if m.recording == r && !m.drift_measured {
                m.drift_ppm = med;
            }
        }
    }
}

/// How `ffcine sync` writes the result.
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// J-cut: recorder audio starts this long before its video (s).
    pub lead: f64,
    /// L-cut: recorder audio runs this long past its video (s).
    pub tail: f64,
    pub min_confidence: f64,
    /// Track name (default `H2`).
    pub track: Option<String>,
    /// Effects added to each new clip.
    pub clean: Option<Clean>,
}

/// Adds the recorder audio to `timeline` as clips on a new audio track (two, checkerboarded,
/// when `lead`/`tail` make neighbours overlap), mirroring the first video track. With no
/// timeline, a new one is made with the videos in order on `V1`.
pub fn write_timeline(
    timeline: Option<Timeline>,
    syncs: &[ClipSync],
    recordings: &[PathBuf],
    opts: &WriteOptions,
) -> Result<Timeline> {
    let mut tl = match timeline {
        Some(t) => t,
        None => video_timeline(syncs),
    };
    let rates: Vec<f64> = recordings.iter().map(|p| native_rate(p)).collect();
    let video = tl
        .track_list()
        .find(|t| t.kind == track_kind::VIDEO)
        .ok_or_else(|| Error::Invalid {
            what: "timeline",
            value: "no video track".into(),
        })?
        .clone();
    let (placed, _) = place::layout(&video);
    let name = opts.track.clone().unwrap_or_else(|| "H2".into());
    let split = opts.lead > 0.0 || opts.tail > 0.0;
    let mut tracks = vec![audio_track(&name)];
    if split {
        tracks[0].name = format!("{name} A");
        tracks.push(audio_track(&format!("{name} B")));
    }
    let mut cursor = vec![0.0f64; tracks.len()];
    let mut n = 0usize;
    for p in &placed {
        let Some(path) = media_path(p.clip, None) else {
            continue;
        };
        let Some(s) = syncs.iter().find(|s| same_file(&s.video, &path)) else {
            continue;
        };
        let Some(m) = s
            .found
            .as_ref()
            .filter(|m| m.confidence >= opts.min_confidence)
        else {
            continue;
        };
        let rate = rates[m.recording];
        let k = 1.0 + m.drift_ppm * 1e-6;
        let ti = n % tracks.len();
        n += 1;
        let mut start = p.start - opts.lead;
        let mut src = m.offset + (p.source_start - opts.lead) * k;
        let mut dur = p.duration + opts.lead + opts.tail;
        // Not before the track's last clip, nor before the recording began.
        let cut = (cursor[ti] - start).max(-src / k).max(0.0);
        start += cut;
        src += cut * k;
        dur -= cut;
        if dur <= 0.0 {
            continue;
        }
        if start > cursor[ti] + 1e-9 {
            tracks[ti]
                .children
                .push(Item::Gap(Gap::new(RationalTime::from_seconds(
                    start - cursor[ti],
                    rate,
                ))));
        }
        let wav = &recordings[m.recording];
        let mut clip = Clip::new(
            format!("{} {}", stem(wav), p.clip.name),
            MediaReference::External(ExternalReference::from_path(wav, None)),
            Some(TimeRange::new(
                RationalTime::from_seconds(src, rate),
                RationalTime::from_seconds(dur, rate),
            )),
        );
        let id = p.clip.ffcine_id().unwrap_or(&p.clip.name).to_owned();
        clip.set_ffcine_id(format!("{id}.audio"));
        clip.ffcine_set(
            "sync",
            &SyncInfo {
                schema: format!("{}/1", schema::SYNC),
                video: id,
                offset: m.offset,
                drift_ppm: m.drift_ppm,
                confidence: m.confidence,
            },
        )?;
        if let Some(c) = &opts.clean {
            clip.effects
                .push(schema::make_effect("clean", schema::CLEAN, c)?);
        }
        clip.effects.push(schema::make_effect(
            "level",
            schema::LEVEL,
            &Level::default(),
        )?);
        tracks[ti].children.push(Item::Clip(clip));
        cursor[ti] = start + dur;
    }
    tl.tracks
        .children
        .extend(tracks.into_iter().map(Item::Track));
    Ok(tl)
}

fn audio_track(name: &str) -> Track {
    let mut t = Track::new(name, track_kind::AUDIO);
    t.ffcine_mut()
        .insert("bus".into(), Bus::Dialogue.name().into());
    t
}

/// A timeline with the synced videos in order on `V1`, each at its own frame rate.
fn video_timeline(syncs: &[ClipSync]) -> Timeline {
    let mut track = Track::new("V1", track_kind::VIDEO);
    for s in syncs {
        let rate = decode::video_rate(&s.video).unwrap_or(48000.0);
        let range = TimeRange::new(
            RationalTime::new(0.0, rate),
            RationalTime::from_seconds(s.seconds, rate),
        );
        let mut clip = Clip::new(
            stem(&s.video),
            MediaReference::External(ExternalReference::from_path(&s.video, Some(range.clone()))),
            Some(range),
        );
        clip.set_ffcine_id(stem(&s.video));
        track.children.push(Item::Clip(clip));
    }
    let mut tl = Timeline::new("sync");
    tl.tracks.children.push(Item::Track(track));
    tl
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.file_name() == b.file_name(),
    }
}

/// The recorder's sample rate (WAV header), for sample-exact source ranges.
fn native_rate(p: &Path) -> f64 {
    hound::WavReader::open(p).map_or(48000.0, |r| f64::from(r.spec().sample_rate))
}

/// Report lines, one per clip.
pub fn report(syncs: &[ClipSync], recordings: &[PathBuf]) -> String {
    let mut s = String::from(
        "clip\trecording\toffset_s\tdrift_ppm\tconfidence\tfine\tmargin\twindows\tresidual_ms\n",
    );
    for c in syncs {
        let name = c
            .video
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        match &c.found {
            Some(m) => {
                let rec = recordings[m.recording]
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                let _ = writeln!(
                    s,
                    "{name}\t{rec}\t{:.6}\t{:.1}{}\t{:.2}\t{:.2}\t{:.2}\t{}/{}\t{:.3}",
                    m.offset,
                    m.drift_ppm,
                    if m.drift_measured { "" } else { "*" },
                    m.confidence,
                    m.fine,
                    m.margin,
                    m.inliers,
                    m.windows,
                    m.residual_ms
                );
            }
            None => {
                let _ = writeln!(s, "{name}\t-\t-\t-\t0\t-\t-\t-\t-");
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::warp;

    /// xorshift noise in -1..1.
    fn noise(seed: u64, n: usize) -> Vec<f32> {
        let mut x = seed.max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 40) as f32 / (1u64 << 23) as f32 - 1.0
            })
            .collect()
    }

    /// Speech-ish: noise gated by a random syllable envelope (~4 Hz), plus a quiet floor.
    fn scene(seed: u64, seconds: f64) -> Vec<f32> {
        let n = (seconds * f64::from(RATE)) as usize;
        let mut x = noise(seed, n);
        let gates = noise(seed ^ 0xabc, n / 2000 + 1);
        for (i, s) in x.iter_mut().enumerate() {
            let g = gates[i / 2000].max(0.0);
            *s *= g;
        }
        x
    }

    #[test]
    fn finds_offset_and_drift() {
        let rec = scene(7, 300.0);
        // The camera saw recorder seconds 100.0375.. with a clock 40 ppm slow relative to the
        // recorder: recorder = 100.0375 + t * (1 + 40e-6).
        let k = 1.0 + 40e-6;
        let start = 100.0375 * f64::from(RATE);
        let mut cam = warp(&rec, start, k, (60.0 * f64::from(RATE)) as usize);
        let n2 = noise(99, cam.len());
        for (c, n) in cam.iter_mut().zip(n2) {
            *c = *c * 0.3 + n * 0.02;
        }
        let other = Prepared::new(scene(11, 200.0));
        let m = align(
            &Prepared::new(cam),
            &[other, Prepared::new(rec)],
            &SyncOptions::default(),
        )
        .expect("match");
        assert_eq!(m.recording, 1);
        assert!((m.offset - 100.0375).abs() < 2e-4, "offset {}", m.offset);
        assert!(m.drift_measured);
        assert!((m.drift_ppm - 40.0).abs() < 8.0, "drift {}", m.drift_ppm);
        assert!(m.confidence > 0.5, "{m:?}");
    }

    #[test]
    fn clip_before_recording_start() {
        let rec = scene(3, 120.0);
        // Camera rolled 2 s before the recorder: first 2 s of camera audio aren't recorded.
        let mut cam = scene(5, 2.0);
        cam.extend_from_slice(&rec[..(10.0 * f64::from(RATE)) as usize]);
        let m = align(
            &Prepared::new(cam),
            &[Prepared::new(rec)],
            &SyncOptions::default(),
        )
        .expect("match");
        assert!((m.offset + 2.0).abs() < 2e-4, "offset {}", m.offset);
    }

    #[test]
    fn unrelated_audio_has_low_confidence() {
        let rec = scene(3, 120.0);
        let cam = scene(42, 15.0);
        let m = align(
            &Prepared::new(cam),
            &[Prepared::new(rec)],
            &SyncOptions::default(),
        );
        assert!(m.as_ref().is_none_or(|m| m.confidence < 0.15), "{m:?}");
    }
}
