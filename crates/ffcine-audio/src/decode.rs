//! Loading media audio: a time range of any file, resampled to the mix rate, optionally through
//! a libavfilter chain (the clean chain). With the `ffmpeg` feature everything goes through
//! libav*; without it only WAV loads, and filter chains are refused.

use std::path::Path;

use ffcine_core::{Error, Result};

use crate::buffer::Audio;

/// What to load.
#[derive(Debug, Clone, Default)]
pub struct Request<'a> {
    /// Media time (s) of the first sample; 0 is the first audio sample in the file.
    pub start: f64,
    /// Seconds to read; `None` to the end. Short files are padded with silence.
    pub duration: Option<f64>,
    pub rate: u32,
    /// 1 (mono mix) or 2.
    pub channels: usize,
    /// libavfilter chain run after resampling (e.g. [`crate::Clean::filter_spec`]).
    pub filter: Option<&'a str>,
}

/// Seconds decoded before `start` so filters (denoise profile, compressor) settle.
const PREROLL: f64 = 0.5;

/// Loads `req` from `path`.
pub fn load(path: &Path, req: &Request<'_>) -> Result<Audio> {
    let mut stream = Stream::open(path, req)?;
    let mut channels = vec![Vec::new(); req.channels];
    while let Some(c) = stream.read()? {
        for (o, c) in channels.iter_mut().zip(c) {
            o.extend(c);
        }
    }
    Ok(Audio {
        rate: req.rate,
        channels,
    })
}

/// [`load`] a chunk at a time, for media too long to hold.
pub struct Stream {
    raw: Raw,
    channels: usize,
    rate: f64,
    /// Media time (s) the decoder starts from, and frames to drop past that (pre-roll, latency).
    from: f64,
    extra: usize,
    /// Frames still to drop once the first decoded time is known; negative: silence to add.
    head: Option<i64>,
    /// Frames left to return; `None` to the end of the file.
    left: Option<usize>,
}

impl Stream {
    pub fn open(path: &Path, req: &Request<'_>) -> Result<Self> {
        let filter = req.filter.filter(|f| !f.is_empty());
        let pre = if filter.is_some() {
            PREROLL.min(req.start)
        } else {
            0.0
        };
        // Filters with lookahead (afftdn: 25 ms) delay their output; read further and drop it.
        let lat = match filter {
            Some(f) => latency(f, req.rate, req.channels)?,
            None => 0,
        };
        let rate = f64::from(req.rate);
        let from = req.start - pre;
        Ok(Self {
            raw: Raw::open(path, from, req)?,
            channels: req.channels,
            rate,
            from,
            extra: (pre * rate).round() as usize + lat,
            head: None,
            left: req.duration.map(|d| (d * rate).round().max(0.0) as usize),
        })
    }

    /// The next frames, or `None` at the end. Short files pad with silence up to `duration`.
    pub fn read(&mut self) -> Result<Option<Vec<Vec<f32>>>> {
        loop {
            if self.left == Some(0) {
                return Ok(None);
            }
            let Some(mut c) = self.raw.next()? else {
                let Some(left) = self.left else {
                    return Ok(None);
                };
                let n = left.min(crate::spool::CHUNK);
                self.left = Some(left - n);
                return Ok(Some(vec![vec![0.0; n]; self.channels]));
            };
            let (from, extra, rate) = (self.from, self.extra as i64, self.rate);
            let first = self.raw.first();
            let head = self
                .head
                .get_or_insert_with(|| ((from - first) * rate).round() as i64 + extra);
            if *head > 0 {
                let n = (*head as usize).min(c[0].len());
                for ch in &mut c {
                    ch.drain(..n);
                }
                *head -= n as i64;
            } else if *head < 0 {
                for ch in &mut c {
                    ch.splice(0..0, std::iter::repeat_n(0.0, (-*head) as usize));
                }
                *head = 0;
            }
            if c[0].is_empty() {
                continue;
            }
            if let Some(left) = &mut self.left {
                let n = c[0].len().min(*left);
                for ch in &mut c {
                    ch.truncate(n);
                }
                *left -= n;
            }
            return Ok(Some(c));
        }
    }
}

/// Delay (samples) a filter chain adds: a noise burst through it, located by
/// cross-correlation. Measured once per chain, rate and channel count.
#[cfg(feature = "ffmpeg")]
pub fn latency(spec: &str, rate: u32, channels: usize) -> Result<usize> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    type Cache = HashMap<(String, u32, usize), usize>;
    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
    let key = (spec.to_owned(), rate, channels);
    if let Some(&l) = CACHE
        .lock()
        .ok()
        .as_ref()
        .and_then(|c| c.as_ref()?.get(&key))
    {
        return Ok(l);
    }
    let r = rate as usize;
    let mut x = vec![0.0f32; 2 * r];
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    for v in &mut x[r / 2..r / 2 + r] {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *v = ((s >> 40) as f32 / (1u64 << 23) as f32 - 1.0) * 0.3;
    }
    let input = Audio {
        rate,
        channels: vec![x.clone(); channels],
    };
    let y = filter(&input, spec)?.to_mono();
    let c = crate::dsp::xcorr(&x, &y);
    // Lags 0 ..= rate/4 sit at index x.len()-1 onwards.
    let zero = x.len() - 1;
    let l = c[zero..(zero + r / 4).min(c.len())]
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(k, _)| k);
    if let Ok(mut g) = CACHE.lock() {
        g.get_or_insert_with(HashMap::new).insert(key, l);
    }
    Ok(l)
}

#[cfg(not(feature = "ffmpeg"))]
pub fn latency(_spec: &str, _rate: u32, _channels: usize) -> Result<usize> {
    Ok(0)
}

/// Runs in-memory audio (mono or stereo) through a libavfilter chain; same rate and layout out.
pub fn filter(input: &Audio, spec: &str) -> Result<Audio> {
    let n = input.channels.len();
    let mut chain = FilterChain::new(spec, input.rate, n)?;
    let mut channels = chain.push(&input.channels)?;
    for (o, c) in channels.iter_mut().zip(chain.finish()?) {
        o.extend(c);
    }
    Ok(Audio {
        rate: input.rate,
        channels,
    })
}

/// A libavfilter chain over audio fed in chunks (mono or stereo); same rate and layout out.
#[cfg(feature = "ffmpeg")]
pub struct FilterChain {
    tail: String,
    rate: u32,
    channels: usize,
    graph: Option<ffmpeg_next::filter::Graph>,
    pts: i64,
}

#[cfg(feature = "ffmpeg")]
impl FilterChain {
    pub fn new(spec: &str, rate: u32, channels: usize) -> Result<Self> {
        ffmpeg_next::init()?;
        let lname = if channels == 1 { "mono" } else { "stereo" };
        Ok(Self {
            tail: format!(
                "{spec},aformat=sample_fmts=fltp:sample_rates={rate}:channel_layouts={lname}"
            ),
            rate,
            channels,
            graph: None,
            pts: 0,
        })
    }

    fn drain(&mut self, out: &mut [Vec<f32>]) {
        let Some(mut o) = self.graph.as_mut().and_then(|g| g.get("out")) else {
            return;
        };
        let mut sink = ffmpeg_next::frame::Audio::empty();
        while o.sink().frame(&mut sink).is_ok() {
            for (c, v) in out.iter_mut().enumerate() {
                v.extend_from_slice(&sink.plane::<f32>(c)[..sink.samples()]);
            }
        }
    }

    /// Feeds `chunk`; returns what the chain has ready.
    pub fn push(&mut self, chunk: &[Vec<f32>]) -> Result<Vec<Vec<f32>>> {
        use ffmpeg_next as ff;
        let layout = if self.channels == 1 {
            ff::ChannelLayout::MONO
        } else {
            ff::ChannelLayout::STEREO
        };
        let fmt = ff::format::Sample::F32(ff::format::sample::Type::Planar);
        let frames = chunk.first().map_or(0, Vec::len);
        let mut out = vec![Vec::with_capacity(frames); self.channels];
        let mut pos = 0;
        while pos < frames {
            let len = 1024.min(frames - pos);
            let mut f = ff::frame::Audio::new(fmt, len, layout);
            f.set_rate(self.rate);
            f.set_pts(Some(self.pts));
            self.pts += len as i64;
            for (c, src) in chunk.iter().enumerate() {
                f.plane_mut::<f32>(c)[..len].copy_from_slice(&src[pos..pos + len]);
            }
            if self.graph.is_none() {
                self.graph = Some(build_graph(&f, &self.tail)?);
            }
            self.graph
                .as_mut()
                .and_then(|g| g.get("in"))
                .ok_or(Error::Pipeline("filter graph input".into()))?
                .source()
                .add(&f)?;
            self.drain(&mut out);
            pos += len;
        }
        Ok(out)
    }

    /// The input is over: the rest of the output.
    pub fn finish(&mut self) -> Result<Vec<Vec<f32>>> {
        let mut out = vec![Vec::new(); self.channels];
        if let Some(mut src) = self.graph.as_mut().and_then(|g| g.get("in")) {
            src.source().flush()?;
        }
        self.drain(&mut out);
        Ok(out)
    }
}

#[cfg(not(feature = "ffmpeg"))]
pub struct FilterChain;

#[cfg(not(feature = "ffmpeg"))]
impl FilterChain {
    pub fn new(_spec: &str, _rate: u32, _channels: usize) -> Result<Self> {
        Err(Error::Unimplemented(
            "audio filter chains without the ffmpeg feature",
        ))
    }

    pub fn push(&mut self, _chunk: &[Vec<f32>]) -> Result<Vec<Vec<f32>>> {
        Ok(Vec::new())
    }

    pub fn finish(&mut self) -> Result<Vec<Vec<f32>>> {
        Ok(Vec::new())
    }
}

/// Without libav*, a WAV in memory, handed out in chunks.
#[cfg(not(feature = "ffmpeg"))]
struct Raw {
    audio: Audio,
    pos: usize,
    start: f64,
}

#[cfg(not(feature = "ffmpeg"))]
impl Raw {
    fn open(path: &Path, start: f64, req: &Request<'_>) -> Result<Self> {
        if req.filter.is_some_and(|f| !f.is_empty()) {
            return Err(Error::Unimplemented(
                "audio filter chains without the ffmpeg feature",
            ));
        }
        let a = Audio::read_wav(path)?;
        let lo = ((start * f64::from(a.rate)).round().max(0.0)) as usize;
        let channels: Vec<Vec<f32>> = a
            .channels
            .iter()
            .map(|c| crate::dsp::resample(&c[lo.min(c.len())..], a.rate, req.rate))
            .collect();
        let a = Audio {
            rate: req.rate,
            channels,
        };
        Ok(Self {
            audio: if req.channels == 1 {
                Audio::mono(req.rate, a.to_mono())
            } else {
                a.to_stereo()
            },
            pos: 0,
            start,
        })
    }

    fn first(&self) -> f64 {
        self.start
    }

    fn next(&mut self) -> Result<Option<Vec<Vec<f32>>>> {
        let n = crate::spool::CHUNK.min(self.audio.frames() - self.pos);
        if n == 0 {
            return Ok(None);
        }
        let pos = self.pos;
        self.pos += n;
        Ok(Some(
            self.audio
                .channels
                .iter()
                .map(|c| c[pos..pos + n].to_vec())
                .collect(),
        ))
    }
}

/// The decoder and resampler (and clean chain) of a [`Stream`], frames as they come.
#[cfg(feature = "ffmpeg")]
struct Raw {
    ictx: ffmpeg_next::format::context::Input,
    dec: ffmpeg_next::decoder::Audio,
    stream: usize,
    time_base: f64,
    t0: f64,
    start: f64,
    tail: String,
    channels: usize,
    graph: Option<ffmpeg_next::filter::Graph>,
    first: Option<f64>,
    next_pts: i64,
    eof: bool,
}

#[cfg(feature = "ffmpeg")]
impl Raw {
    fn open(path: &Path, start: f64, req: &Request<'_>) -> Result<Self> {
        use ffmpeg_next as ff;

        static QUIET: std::sync::Once = std::sync::Once::new();
        ff::init()?;
        QUIET.call_once(|| ff::util::log::set_level(ff::util::log::Level::Error));
        let mut ictx = ff::format::input(path)?;
        // The first audio stream: the camera's main track (iPhones add a 4-channel APAC one that
        // `best` would pick).
        let stream = ictx
            .streams()
            .find(|s| s.parameters().medium() == ff::media::Type::Audio)
            .ok_or_else(|| Error::Import {
                path: path.to_owned(),
                msg: "no audio stream".to_owned(),
            })?;
        let idx = stream.index();
        let time_base = f64::from(stream.time_base());
        let t0 = match stream.start_time() {
            ff::ffi::AV_NOPTS_VALUE => 0.0,
            s => s as f64 * time_base,
        };
        let dec = ff::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        if start > 1.0 {
            let ts = ((t0 + start - 1.0) * 1e6) as i64;
            ictx.seek(ts, ..ts)?;
        }
        let layout = if req.channels == 1 { "mono" } else { "stereo" };
        let mut tail = format!(
            "aresample={},aformat=sample_fmts=fltp:channel_layouts={layout}",
            req.rate
        );
        if let Some(f) = req.filter.filter(|f| !f.is_empty()) {
            tail = format!(
                "{tail},{f},aformat=sample_fmts=fltp:sample_rates={}:channel_layouts={layout}",
                req.rate
            );
        }
        Ok(Self {
            ictx,
            dec,
            stream: idx,
            time_base,
            t0,
            start,
            tail,
            channels: req.channels,
            graph: None,
            first: None,
            next_pts: 0,
            eof: false,
        })
    }

    /// Media time (s) of the first frame that went in.
    fn first(&self) -> f64 {
        self.first.unwrap_or(self.start)
    }

    /// Takes every frame the decoder has into the filter graph, and what comes out into `out`.
    fn frames(&mut self, out: &mut [Vec<f32>]) -> Result<()> {
        use ffmpeg_next as ff;
        let mut frame = ff::frame::Audio::empty();
        while self.dec.receive_frame(&mut frame).is_ok() {
            // Skip frames entirely before the range (the seek lands on an earlier keyframe).
            let t = frame
                .pts()
                .map_or(0.0, |p| p as f64 * self.time_base - self.t0);
            let end = t + frame.samples() as f64 / f64::from(frame.rate().max(1));
            if self.first.is_none() && end < self.start - 0.1 {
                continue;
            }
            if self.graph.is_none() {
                self.graph = Some(build_graph(&frame, &self.tail)?);
            }
            if self.first.is_none() {
                self.first = Some(t);
            }
            frame.set_pts(Some(self.next_pts));
            self.next_pts += frame.samples() as i64;
            self.graph
                .as_mut()
                .and_then(|g| g.get("in"))
                .ok_or(Error::Pipeline("filter graph input".into()))?
                .source()
                .add(&frame)?;
            self.drain(out);
        }
        Ok(())
    }

    fn drain(&mut self, out: &mut [Vec<f32>]) {
        let Some(mut sink) = self.graph.as_mut().and_then(|g| g.get("out")) else {
            return;
        };
        let mut f = ffmpeg_next::frame::Audio::empty();
        while sink.sink().frame(&mut f).is_ok() {
            for (c, o) in out.iter_mut().enumerate() {
                o.extend_from_slice(&f.plane::<f32>(c)[..f.samples()]);
            }
        }
    }

    /// The next frames; `None` once the file is done.
    fn next(&mut self) -> Result<Option<Vec<Vec<f32>>>> {
        use ffmpeg_next as ff;
        let mut out = vec![Vec::new(); self.channels];
        let mut packet = ff::Packet::empty();
        while !self.eof && out[0].is_empty() {
            match packet.read(&mut self.ictx) {
                Err(ff::Error::Eof) => {
                    self.eof = true;
                    let _ = self.dec.send_eof();
                    self.frames(&mut out)?;
                    if let Some(mut src) = self.graph.as_mut().and_then(|g| g.get("in")) {
                        src.source().flush()?;
                    }
                    self.drain(&mut out);
                    break;
                }
                // A damaged packet: on to the next.
                Err(_) => continue,
                Ok(()) => {}
            }
            if packet.stream() == self.stream && self.dec.send_packet(&packet).is_ok() {
                self.frames(&mut out)?;
            }
        }
        Ok((!out[0].is_empty()).then_some(out))
    }
}

#[cfg(feature = "ffmpeg")]
fn build_graph(
    frame: &ffmpeg_next::frame::Audio,
    tail: &str,
) -> Result<ffmpeg_next::filter::Graph> {
    use ffmpeg_next as ff;
    let missing = |n: &str| Error::Pipeline(format!("libavfilter has no {n}"));
    let layout = frame.channel_layout();
    let layout = if layout.bits() != 0 {
        format!("0x{:x}", layout.bits())
    } else {
        format!("{}c", frame.channels())
    };
    let args = format!(
        "time_base=1/{r}:sample_rate={r}:sample_fmt={}:channel_layout={layout}",
        frame.format().name(),
        r = frame.rate()
    );
    let mut g = ff::filter::Graph::new();
    g.add(
        &ff::filter::find("abuffer").ok_or_else(|| missing("abuffer"))?,
        "in",
        &args,
    )?;
    g.add(
        &ff::filter::find("abuffersink").ok_or_else(|| missing("abuffersink"))?,
        "out",
        "",
    )?;
    g.output("in", 0)?
        .input("out", 0)?
        .parse(tail)
        .map_err(|e| Error::Pipeline(format!("audio filter chain {tail:?}: {e}")))?;
    g.validate()?;
    Ok(g)
}

/// Whether `path` has an audio stream. Files that don't open count as having one, so loading
/// them reports the real error.
#[cfg(feature = "ffmpeg")]
pub fn has_audio(path: &Path) -> bool {
    use ffmpeg_next as ff;
    if ff::init().is_err() {
        return true;
    }
    ff::format::input(path).map_or(true, |ictx| {
        ictx.streams()
            .any(|s| s.parameters().medium() == ff::media::Type::Audio)
    })
}

#[cfg(not(feature = "ffmpeg"))]
pub fn has_audio(_path: &Path) -> bool {
    true
}

/// Frame rate of the first video stream (for clip ranges `ffcine sync` writes), if any.
#[cfg(feature = "ffmpeg")]
pub fn video_rate(path: &Path) -> Option<f64> {
    use ffmpeg_next as ff;
    ff::init().ok()?;
    let ictx = ff::format::input(path).ok()?;
    let s = ictx.streams().best(ff::media::Type::Video)?;
    let r = f64::from(s.avg_frame_rate());
    (r.is_finite() && r > 0.0).then_some(r)
}

#[cfg(not(feature = "ffmpeg"))]
pub fn video_rate(_path: &Path) -> Option<f64> {
    None
}
