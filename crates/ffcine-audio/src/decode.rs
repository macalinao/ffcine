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
    let dur = req.duration.map(|d| d + pre + lat as f64 / rate);
    let mut a = load_raw(path, from, dur, req)?;
    let skip = (pre * rate).round() as usize + lat;
    for c in &mut a.channels {
        c.drain(..skip.min(c.len()));
    }
    if let Some(d) = req.duration {
        a.set_frames((d * rate).round() as usize);
    }
    Ok(a)
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
#[cfg(feature = "ffmpeg")]
pub fn filter(input: &Audio, spec: &str) -> Result<Audio> {
    use ffmpeg_next as ff;
    ff::init()?;
    let n = input.channels.len();
    let layout = if n == 1 {
        ff::ChannelLayout::MONO
    } else {
        ff::ChannelLayout::STEREO
    };
    let lname = if n == 1 { "mono" } else { "stereo" };
    let tail = format!(
        "{spec},aformat=sample_fmts=fltp:sample_rates={}:channel_layouts={lname}",
        input.rate
    );
    let fmt = ff::format::Sample::F32(ff::format::sample::Type::Planar);
    let mut out = vec![Vec::with_capacity(input.frames()); n];
    let mut graph: Option<ff::filter::Graph> = None;
    let mut sink = ff::frame::Audio::empty();
    let mut drain = |g: &mut ff::filter::Graph, out: &mut Vec<Vec<f32>>| {
        if let Some(mut o) = g.get("out") {
            while o.sink().frame(&mut sink).is_ok() {
                for (c, v) in out.iter_mut().enumerate() {
                    v.extend_from_slice(&sink.plane::<f32>(c)[..sink.samples()]);
                }
            }
        }
    };
    let chunk = 1024;
    let mut pos = 0;
    while pos < input.frames() {
        let len = chunk.min(input.frames() - pos);
        let mut f = ff::frame::Audio::new(fmt, len, layout);
        f.set_rate(input.rate);
        f.set_pts(Some(pos as i64));
        for (c, src) in input.channels.iter().enumerate() {
            f.plane_mut::<f32>(c)[..len].copy_from_slice(&src[pos..pos + len]);
        }
        if graph.is_none() {
            graph = Some(build_graph(&f, &tail)?);
        }
        let g = graph
            .as_mut()
            .ok_or(Error::Pipeline("filter graph".into()))?;
        g.get("in")
            .ok_or(Error::Pipeline("filter graph input".into()))?
            .source()
            .add(&f)?;
        drain(g, &mut out);
        pos += len;
    }
    if let Some(g) = graph.as_mut() {
        if let Some(mut src) = g.get("in") {
            src.source().flush()?;
        }
        drain(g, &mut out);
    }
    Ok(Audio {
        rate: input.rate,
        channels: out,
    })
}

#[cfg(not(feature = "ffmpeg"))]
fn load_raw(path: &Path, start: f64, dur: Option<f64>, req: &Request<'_>) -> Result<Audio> {
    if req.filter.is_some_and(|f| !f.is_empty()) {
        return Err(Error::Unimplemented(
            "audio filter chains without the ffmpeg feature",
        ));
    }
    let a = Audio::read_wav(path)?;
    let lo = ((start * f64::from(a.rate)).round().max(0.0)) as usize;
    let hi = dur.map_or(a.frames(), |d| {
        lo + (d * f64::from(a.rate)).round() as usize
    });
    let channels: Vec<Vec<f32>> = a
        .channels
        .iter()
        .map(|c| {
            let mut v = c[lo.min(c.len())..hi.min(c.len())].to_vec();
            v.resize(hi - lo, 0.0);
            crate::dsp::resample(&v, a.rate, req.rate)
        })
        .collect();
    let a = Audio {
        rate: req.rate,
        channels,
    };
    Ok(if req.channels == 1 {
        Audio::mono(req.rate, a.to_mono())
    } else {
        a.to_stereo()
    })
}

#[cfg(feature = "ffmpeg")]
#[allow(clippy::too_many_lines)] // one decode loop with its closures
fn load_raw(path: &Path, start: f64, dur: Option<f64>, req: &Request<'_>) -> Result<Audio> {
    use ffmpeg_next as ff;

    static QUIET: std::sync::Once = std::sync::Once::new();
    ff::init()?;
    QUIET.call_once(|| ff::util::log::set_level(ff::util::log::Level::Error));
    let open_err = |msg: &str| Error::Import {
        path: path.to_owned(),
        msg: msg.to_owned(),
    };
    let mut ictx = ff::format::input(path)?;
    // The first audio stream: the camera's main track (iPhones add a 4-channel APAC one that
    // `best` would pick).
    let stream = ictx
        .streams()
        .find(|s| s.parameters().medium() == ff::media::Type::Audio)
        .ok_or_else(|| open_err("no audio stream"))?;
    let idx = stream.index();
    let tb = f64::from(stream.time_base());
    let t0 = match stream.start_time() {
        ff::ffi::AV_NOPTS_VALUE => 0.0,
        s => s as f64 * tb,
    };
    let mut dec = ff::codec::context::Context::from_parameters(stream.parameters())?
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
    let mut graph: Option<ff::filter::Graph> = None;
    let mut first: Option<f64> = None;
    let mut out = vec![Vec::new(); req.channels];
    let want = dur.map(|d| (d * f64::from(req.rate)).ceil() as usize);
    let mut next_pts = 0i64;

    let mut frame = ff::frame::Audio::empty();
    let mut sink_frame = ff::frame::Audio::empty();
    let drain =
        |graph: &mut ff::filter::Graph, out: &mut Vec<Vec<f32>>, f: &mut ff::frame::Audio| {
            let Some(mut sink) = graph.get("out") else {
                return;
            };
            while sink.sink().frame(f).is_ok() {
                for (c, o) in out.iter_mut().enumerate() {
                    o.extend_from_slice(&f.plane::<f32>(c)[..f.samples()]);
                }
            }
        };
    let enough = |out: &Vec<Vec<f32>>, first: Option<f64>| match (want, first) {
        (Some(w), Some(f)) => {
            let skip = ((start - f) * f64::from(req.rate)).round().max(0.0) as usize;
            out[0].len() >= skip + w
        }
        _ => false,
    };
    let mut push = |frame: &mut ff::frame::Audio,
                    graph: &mut Option<ff::filter::Graph>,
                    first: &mut Option<f64>,
                    out: &mut Vec<Vec<f32>>|
     -> Result<()> {
        if graph.is_none() {
            let g = build_graph(frame, &tail)?;
            *graph = Some(g);
        }
        if first.is_none() {
            *first = Some(frame.pts().map_or(0.0, |p| p as f64 * tb - t0));
        }
        let g = graph
            .as_mut()
            .ok_or(Error::Pipeline("filter graph".into()))?;
        frame.set_pts(Some(next_pts));
        next_pts += frame.samples() as i64;
        g.get("in")
            .ok_or(Error::Pipeline("filter graph input".into()))?
            .source()
            .add(frame)?;
        drain(g, out, &mut sink_frame);
        Ok(())
    };

    for (s, packet) in ictx.packets() {
        if s.index() != idx {
            continue;
        }
        if dec.send_packet(&packet).is_err() {
            continue;
        }
        while dec.receive_frame(&mut frame).is_ok() {
            // Skip frames entirely before the range (the seek lands on an earlier keyframe).
            let t = frame.pts().map_or(0.0, |p| p as f64 * tb - t0);
            let end = t + frame.samples() as f64 / f64::from(frame.rate().max(1));
            if first.is_none() && end < start - 0.1 {
                continue;
            }
            push(&mut frame, &mut graph, &mut first, &mut out)?;
        }
        if enough(&out, first) {
            break;
        }
    }
    let _ = dec.send_eof();
    while dec.receive_frame(&mut frame).is_ok() {
        push(&mut frame, &mut graph, &mut first, &mut out)?;
    }
    if let Some(g) = graph.as_mut() {
        if let Some(mut src) = g.get("in") {
            src.source().flush()?;
        }
        let mut f = ff::frame::Audio::empty();
        drain(g, &mut out, &mut f);
    }

    let first = first.unwrap_or(start);
    let skip = ((start - first) * f64::from(req.rate)).round();
    let rate = req.rate;
    let channels = out
        .into_iter()
        .map(|mut c| {
            if skip >= 0.0 {
                c.drain(..(skip as usize).min(c.len()));
            } else {
                let pad = (-skip) as usize;
                c.splice(0..0, std::iter::repeat_n(0.0, pad));
            }
            if let Some(w) = want {
                c.resize(w, 0.0);
            }
            c
        })
        .collect();
    Ok(Audio { rate, channels })
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
