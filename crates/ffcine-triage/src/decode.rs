//! libav* side of triage: low-res frames and 16 kHz mono audio from a clip.
//!
//! DJI `.LRF` proxies (720p HEVC) decode on the CPU, which is cheap and leaves the shared GPU
//! alone. Clips without a proxy decode on NVDEC (`*_cuvid`) with its hardware `resize`, so only
//! thumbnail-sized frames ever leave the GPU; if NVDEC can't take the clip, the CPU decoder is
//! used. Clips over [`KEYFRAMES_ONLY_AFTER`] decode keyframes only (DJI puts one every ~1.25 s),
//! which is plenty for runaway detection and keeps a 4 h take to minutes.

use std::io::Write as _;
use std::path::Path;

use ff::codec::threading;
use ff::format::Sample;
use ff::software::scaling;
use ffmpeg_next as ff;

use crate::scene::{SceneDetector, SceneParams, small_luma};
use crate::vad::{Energy, RATE};
use crate::{Error, Result};

/// Thumbnail height; the width follows the picture's aspect ratio.
pub const THUMB_H: u32 = 144;
pub const KEYFRAMES_ONLY_AFTER: f64 = 600.0;

#[derive(Debug, Clone)]
pub struct ScanParams {
    /// Analysis samples per second.
    pub sample_fps: f64,
    /// Evenly spaced strip frames: one per this many seconds, clamped to 4..=10.
    pub strip_every: f64,
    /// Extra strip frames at scene cuts.
    pub strip_cuts: usize,
    /// Use NVDEC for clips without a proxy.
    pub gpu: bool,
    /// CPU decoder threads per clip (0 = libavcodec's choice).
    pub threads: usize,
    pub scene: SceneParams,
}

impl Default for ScanParams {
    fn default() -> Self {
        Self {
            sample_fps: 4.0,
            strip_every: 6.0,
            strip_cuts: 6,
            gpu: true,
            threads: 0,
            scene: SceneParams::default(),
        }
    }
}

pub struct Thumb {
    pub t: f64,
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
}

pub struct VideoScan {
    pub duration: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub cuts: Vec<f64>,
    pub activity: Vec<f32>,
    pub luma: Vec<f32>,
    pub thumbs: Vec<Thumb>,
    /// Decoded on NVDEC.
    pub gpu: bool,
}

pub fn init() -> Result<()> {
    ff::init().map_err(|e| Error::Decode(format!("ffmpeg init: {e}")))?;
    ff::util::log::set_level(ff::util::log::Level::Error);
    Ok(())
}

fn err(path: &Path, what: &str, e: impl std::fmt::Display) -> Error {
    Error::Decode(format!("{}: {what}: {e}", path.display()))
}

/// (width, height, fps) of the first video stream, without decoding.
pub fn probe_video(path: &Path) -> Result<(u32, u32, f64)> {
    let ictx = ff::format::input(path).map_err(|e| err(path, "open", e))?;
    let st = ictx
        .streams()
        .best(ff::media::Type::Video)
        .ok_or_else(|| err(path, "probe", "no video stream"))?;
    let p = st.parameters();
    // SAFETY: codecpar of a live stream.
    let (w, h) = unsafe { ((*p.as_ptr()).width, (*p.as_ptr()).height) };
    Ok((
        w.unsigned_abs(),
        h.unsigned_abs(),
        rate(st.avg_frame_rate()),
    ))
}

fn rate(r: ff::Rational) -> f64 {
    if r.denominator() == 0 {
        0.0
    } else {
        f64::from(r.numerator()) / f64::from(r.denominator())
    }
}

#[allow(clippy::too_many_lines)] // one decode loop
pub fn scan_video(path: &Path, use_gpu: bool, p: &ScanParams) -> Result<VideoScan> {
    let mut ictx = ff::format::input(path).map_err(|e| err(path, "open", e))?;
    let st = ictx
        .streams()
        .best(ff::media::Type::Video)
        .ok_or_else(|| err(path, "probe", "no video stream"))?;
    let index = st.index();
    let tb = st.time_base();
    let tbf = f64::from(tb.numerator()) / f64::from(tb.denominator().max(1));
    let fps = rate(st.avg_frame_rate());
    #[allow(clippy::cast_precision_loss)]
    let duration = if st.duration() > 0 {
        st.duration() as f64 * tbf
    } else {
        ictx.duration().max(0) as f64 / f64::from(ff::ffi::AV_TIME_BASE)
    };
    let params = st.parameters();
    // SAFETY: codecpar of a live stream.
    let (src_w, src_h, codec_id) = unsafe {
        let c = &*params.as_ptr();
        (c.width.unsigned_abs(), c.height.unsigned_abs(), c.codec_id)
    };
    let thumb_w =
        ((u64::from(THUMB_H) * u64::from(src_w) / u64::from(src_h.max(1))) as u32 + 1) & !1;
    let keyframes_only = duration > KEYFRAMES_ONLY_AFTER;

    let open = |gpu: bool| -> Result<ff::decoder::Video> {
        let mut ctx = ff::codec::context::Context::from_parameters(params.clone())
            .map_err(|e| err(path, "decoder params", e))?;
        if gpu {
            let name = format!("{}_cuvid", ff::codec::Id::from(codec_id).name());
            let codec =
                ff::decoder::find_by_name(&name).ok_or_else(|| err(path, "nvdec", "no decoder"))?;
            let mut opts = ff::Dictionary::new();
            opts.set("resize", &format!("{thumb_w}x{THUMB_H}"));
            opts.set("surfaces", "8");
            let mut d = ctx.decoder();
            if keyframes_only {
                d.skip_frame(ff::Discard::NonKey);
            }
            d.open_as_with(codec, opts)
                .and_then(ff::decoder::Opened::video)
                .map_err(|e| err(path, "nvdec", e))
        } else {
            ctx.set_threading(threading::Config {
                kind: threading::Type::Frame,
                count: p.threads,
            });
            let mut d = ctx.decoder();
            if keyframes_only {
                d.skip_frame(ff::Discard::NonKey);
            }
            d.video().map_err(|e| err(path, "decoder", e))
        }
    };
    let (mut dec, gpu) = match use_gpu.then(|| open(true)) {
        Some(Ok(d)) => (d, true),
        _ => (open(false)?, false),
    };

    let mut det = SceneDetector::new(p.scene.clone());
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n_even = ((duration / p.strip_every).round() as usize).clamp(4, 10);
    #[allow(clippy::cast_precision_loss)]
    let even: Vec<f64> = (0..n_even)
        .map(|i| (i as f64 + 0.5) * duration / n_even as f64)
        .collect();
    let mut next_even = 0;
    let mut thumbs: Vec<Thumb> = Vec::new();
    let mut cut_thumbs: Vec<Thumb> = Vec::new();
    let mut next_sample = f64::NEG_INFINITY;
    let mut scaler: Option<(scaling::Context, ff::format::Pixel, u32, u32)> = None;
    let mut rgb = ff::frame::Video::empty();
    let mut frame = ff::frame::Video::empty();

    let mut handle = |frame: &ff::frame::Video| -> Result<()> {
        #[allow(clippy::cast_precision_loss)]
        let t = frame.timestamp().unwrap_or(0) as f64 * tbf;
        if t < next_sample && !(next_even < even.len() && t >= even[next_even]) {
            return Ok(());
        }
        next_sample = t + 1.0 / p.sample_fps;
        let (fmt, w, h) = (frame.format(), frame.width(), frame.height());
        if scaler
            .as_ref()
            .is_none_or(|s| (s.1, s.2, s.3) != (fmt, w, h))
        {
            let ctx = scaling::Context::get(
                fmt,
                w,
                h,
                ff::format::Pixel::RGB24,
                thumb_w,
                THUMB_H,
                scaling::Flags::AREA,
            )
            .map_err(|e| err(path, "scaler", e))?;
            scaler = Some((ctx, fmt, w, h));
        }
        if let Some((s, ..)) = scaler.as_mut() {
            s.run(frame, &mut rgb).map_err(|e| err(path, "scale", e))?;
        }
        let packed = packed_rgb(&rgb, thumb_w as usize, THUMB_H as usize);
        let cut = det.push(t, &small_luma(&packed, thumb_w as usize, THUMB_H as usize));
        let thumb = || Thumb {
            t,
            w: thumb_w,
            h: THUMB_H,
            rgb: packed.clone(),
        };
        if next_even < even.len() && t >= even[next_even] {
            thumbs.push(thumb());
            while next_even < even.len() && t >= even[next_even] {
                next_even += 1;
            }
        } else if cut && cut_thumbs.len() < p.strip_cuts {
            cut_thumbs.push(thumb());
        }
        Ok(())
    };

    for (stream, packet) in ictx.packets() {
        if stream.index() != index {
            continue;
        }
        if dec.send_packet(&packet).is_err() {
            continue; // a damaged packet (truncated offload) shouldn't end the scan
        }
        while dec.receive_frame(&mut frame).is_ok() {
            handle(&frame)?;
        }
    }
    let _ = dec.send_eof();
    while dec.receive_frame(&mut frame).is_ok() {
        handle(&frame)?;
    }

    thumbs.extend(cut_thumbs);
    thumbs.sort_by(|a, b| a.t.total_cmp(&b.t));
    let (activity, luma) = det.per_second();
    Ok(VideoScan {
        duration,
        fps,
        width: src_w,
        height: src_h,
        cuts: det
            .cuts
            .iter()
            .map(|t| (t * 100.0).round() / 100.0)
            .collect(),
        activity,
        luma,
        thumbs,
        gpu,
    })
}

fn packed_rgb(f: &ff::frame::Video, w: usize, h: usize) -> Vec<u8> {
    let stride = f.stride(0);
    let data = f.data(0);
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        out.extend_from_slice(&data[y * stride..y * stride + w * 3]);
    }
    out
}

pub struct AudioScan {
    /// Per-frame band energy (see [`crate::vad`]).
    pub db: Vec<f32>,
}

/// Decodes the first audio stream to 16 kHz mono: energies for the VAD and, with `pcm`, the raw
/// f32le samples for whisper. `None` when the clip has no audio.
pub fn scan_audio(path: &Path, pcm: Option<&Path>) -> Result<Option<AudioScan>> {
    let mut ictx = ff::format::input(path).map_err(|e| err(path, "open", e))?;
    let Some(st) = ictx.streams().best(ff::media::Type::Audio) else {
        return Ok(None);
    };
    let index = st.index();
    let ctx = ff::codec::context::Context::from_parameters(st.parameters())
        .map_err(|e| err(path, "audio params", e))?;
    let mut dec = ctx
        .decoder()
        .audio()
        .map_err(|e| err(path, "audio decoder", e))?;
    let mut energy = Energy::default();
    let mut rs: Option<Resampler> = None;
    let mut out = match pcm {
        Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p)?)),
        None => None,
    };
    let mut frame = ff::frame::Audio::empty();
    let mut mono = Vec::new();
    let mut res = Vec::new();
    let mut handle = |frame: &ff::frame::Audio| -> Result<()> {
        downmix(frame, &mut mono);
        let r = rs.get_or_insert_with(|| Resampler::new(frame.rate(), RATE));
        res.clear();
        r.push(&mono, &mut res);
        energy.push(&res);
        if let Some(o) = out.as_mut() {
            for s in &res {
                o.write_all(&s.to_le_bytes())?;
            }
        }
        Ok(())
    };
    for (stream, packet) in ictx.packets() {
        if stream.index() != index || dec.send_packet(&packet).is_err() {
            continue;
        }
        while dec.receive_frame(&mut frame).is_ok() {
            handle(&frame)?;
        }
    }
    let _ = dec.send_eof();
    while dec.receive_frame(&mut frame).is_ok() {
        handle(&frame)?;
    }
    if let Some(mut o) = out {
        o.flush()?;
    }
    Ok(Some(AudioScan { db: energy.db }))
}

type ReadSample = fn(&[u8]) -> f32;

/// Mean of all channels, as f32.
fn downmix(f: &ff::frame::Audio, out: &mut Vec<f32>) {
    let n = f.samples();
    let ch = usize::from(f.channels()).max(1);
    out.clear();
    out.resize(n, 0.0);
    let (planar, size, read): (bool, usize, ReadSample) = match f.format() {
        Sample::F32(t) => (t == ff::format::sample::Type::Planar, 4, |b| {
            f32::from_le_bytes([b[0], b[1], b[2], b[3]])
        }),
        Sample::I16(t) => (t == ff::format::sample::Type::Planar, 2, |b| {
            f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0
        }),
        #[allow(clippy::cast_precision_loss)]
        Sample::I32(t) => (t == ff::format::sample::Type::Planar, 4, |b| {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0
        }),
        #[allow(clippy::cast_possible_truncation)]
        Sample::F64(t) => (t == ff::format::sample::Type::Planar, 8, |b| {
            f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32
        }),
        Sample::U8(t) => (t == ff::format::sample::Type::Planar, 1, |b| {
            (f32::from(b[0]) - 128.0) / 128.0
        }),
        _ => return,
    };
    #[allow(clippy::cast_precision_loss)]
    let scale = 1.0 / ch as f32;
    for c in 0..ch {
        let (data, step, off) = if planar {
            (f.data(c), size, 0)
        } else {
            (f.data(0), size * ch, c * size)
        };
        for (i, o) in out.iter_mut().enumerate() {
            let at = i * step + off;
            if let Some(b) = data.get(at..at + size) {
                *o += read(b) * scale;
            }
        }
    }
}

/// Streaming windowed-sinc resampler (low-pass at 0.45 of the output rate, linear phase
/// interpolation between filtered input samples). Good enough for speech analysis.
#[derive(Debug)]
pub struct Resampler {
    step: f64,
    taps: Vec<f32>,
    ring: Vec<f32>,
    n: u64,
    next: f64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let step = f64::from(from) / f64::from(to);
        let cutoff = 0.45 / step.max(1.0);
        let half = 16i32;
        let taps: Vec<f32> = (-half..=half)
            .map(|k| {
                let x = f64::from(k);
                let sinc = if k == 0 {
                    2.0 * cutoff
                } else {
                    (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let win = 0.5 + 0.5 * (std::f64::consts::PI * x / f64::from(half + 1)).cos();
                #[allow(clippy::cast_possible_truncation)]
                let v = (sinc * win) as f32;
                v
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        Self {
            step,
            taps: taps.iter().map(|t| t / sum).collect(),
            ring: vec![0.0; 64],
            n: 0,
            next: 0.0,
        }
    }

    /// Filtered input at index `i` (needs `i + half < n`); samples before the start are zero.
    fn at(&self, i: u64) -> f32 {
        let half = (self.taps.len() / 2) as u64;
        let len = self.ring.len() as u64;
        self.taps
            .iter()
            .zip(i..)
            .map(|(&t, j)| {
                if j < half {
                    0.0
                } else {
                    t * self.ring[((j - half) % len) as usize]
                }
            })
            .sum()
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let half = (self.taps.len() / 2) as u64;
        let len = self.ring.len() as u64;
        for &x in input {
            self.ring[(self.n % len) as usize] = x;
            self.n += 1;
            // Emit every output whose two neighbours are fully filtered.
            #[allow(clippy::cast_precision_loss)]
            while self.next + 1.0 + half as f64 <= (self.n - 1) as f64 {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let i = self.next as u64;
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let frac = (self.next - i as f64) as f32;
                let (a, b) = (self.at(i), self.at(i + 1));
                out.push(a + (b - a) * frac);
                self.next += self.step;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_a_tone_and_rate() {
        let mut r = Resampler::new(48_000, 16_000);
        #[allow(clippy::cast_precision_loss)]
        let x: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin())
            .collect();
        let mut y = Vec::new();
        r.push(&x, &mut y);
        assert!((15_990..=16_000).contains(&y.len()), "{}", y.len());
        let peak = y[100..].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((peak - 1.0).abs() < 0.05, "{peak}");
        // 1 kHz at 16 kHz: 16 samples per cycle.
        assert!((y[200] - y[216]).abs() < 0.05);
    }

    #[test]
    fn resampler_removes_alias() {
        let mut r = Resampler::new(48_000, 16_000);
        #[allow(clippy::cast_precision_loss)]
        let x: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 12_000.0 * std::f32::consts::TAU / 48_000.0).sin())
            .collect();
        let mut y = Vec::new();
        r.push(&x, &mut y);
        let peak = y[100..].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.05, "{peak}");
    }
}
