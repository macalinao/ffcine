//! Every frame of a video at a small size: NVDEC (`*_cuvid`) with its hardware resize, so only
//! small frames leave the GPU, else the CPU decoder and swscale.

use std::path::Path;

use ff::codec::threading;
use ff::software::scaling;
use ffmpeg_next as ff;

use crate::{Error, Result};

pub struct Info {
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    /// Frames decoded.
    pub frames: usize,
    /// Seconds, from the frames (the last one's time plus its length).
    pub duration: f64,
    pub gpu: bool,
}

fn err(path: &Path, what: &str, e: impl std::fmt::Display) -> Error {
    Error::Decode(format!("{}: {what}: {e}", path.display()))
}

fn rate(r: ff::Rational) -> f64 {
    if r.denominator() == 0 {
        0.0
    } else {
        f64::from(r.numerator()) / f64::from(r.denominator())
    }
}

/// Even size with the long side `long` and the source's aspect ratio.
pub fn fit(w: u32, h: u32, long: u32) -> (u32, u32) {
    let (w, h) = (u64::from(w.max(1)), u64::from(h.max(1)));
    let long = u64::from(long.max(2));
    let (ow, oh) = if w >= h {
        (long, (long * h / w).max(2))
    } else {
        ((long * w / h).max(2), long)
    };
    let even = |v: u64| u32::try_from(v & !1).unwrap_or(u32::MAX).max(2);
    (even(ow), even(oh))
}

/// Decodes `path`, calling `f(index, seconds, rgb24, w, h)` for each frame index `want` accepts,
/// at the size [`fit`] gives for `long`. Times start at 0 at the first frame.
#[allow(clippy::too_many_lines)] // one decode loop
pub fn frames(
    path: &Path,
    use_gpu: bool,
    long: u32,
    mut want: impl FnMut(usize) -> bool,
    mut f: impl FnMut(usize, f64, &[u8], u32, u32) -> Result<()>,
) -> Result<Info> {
    let mut ictx = ff::format::input(path).map_err(|e| err(path, "open", e))?;
    let st = ictx
        .streams()
        .best(ff::media::Type::Video)
        .ok_or_else(|| err(path, "probe", "no video stream"))?;
    let index = st.index();
    let tb = st.time_base();
    let tbf = f64::from(tb.numerator()) / f64::from(tb.denominator().max(1));
    let fps = rate(st.avg_frame_rate());
    let params = st.parameters();
    // SAFETY: codecpar of a live stream.
    let (src_w, src_h, codec_id) = unsafe {
        let c = &*params.as_ptr();
        (c.width.unsigned_abs(), c.height.unsigned_abs(), c.codec_id)
    };
    let (ow, oh) = fit(src_w, src_h, long);

    let open = |gpu: bool| -> Result<ff::decoder::Video> {
        let mut ctx = ff::codec::context::Context::from_parameters(params.clone())
            .map_err(|e| err(path, "decoder params", e))?;
        if gpu {
            let name = format!("{}_cuvid", ff::codec::Id::from(codec_id).name());
            let codec =
                ff::decoder::find_by_name(&name).ok_or_else(|| err(path, "nvdec", "no decoder"))?;
            let mut opts = ff::Dictionary::new();
            opts.set("resize", &format!("{ow}x{oh}"));
            opts.set("surfaces", "8");
            ctx.decoder()
                .open_as_with(codec, opts)
                .and_then(ff::decoder::Opened::video)
                .map_err(|e| err(path, "nvdec", e))
        } else {
            ctx.set_threading(threading::Config {
                kind: threading::Type::Frame,
                count: 0,
            });
            ctx.decoder().video().map_err(|e| err(path, "decoder", e))
        }
    };
    let (mut dec, gpu) = match use_gpu.then(|| open(true)) {
        Some(Ok(d)) => (d, true),
        _ => (open(false)?, false),
    };

    let mut scaler: Option<(scaling::Context, ff::format::Pixel, u32, u32)> = None;
    let mut rgb = ff::frame::Video::empty();
    let mut frame = ff::frame::Video::empty();
    let mut packed = Vec::with_capacity(ow as usize * oh as usize * 3);
    let mut n = 0usize;
    let mut t0: Option<f64> = None;
    let mut last_t = 0.0;

    let mut handle = |frame: &ff::frame::Video| -> Result<()> {
        let i = n;
        n += 1;
        let raw = frame
            .timestamp()
            .map_or(i as f64 / fps.max(1.0), |p| p as f64 * tbf);
        let t = raw - *t0.get_or_insert(raw);
        last_t = t;
        if !want(i) {
            return Ok(());
        }
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
                ow,
                oh,
                scaling::Flags::AREA,
            )
            .map_err(|e| err(path, "scaler", e))?;
            scaler = Some((ctx, fmt, w, h));
        }
        if let Some((s, ..)) = scaler.as_mut() {
            s.run(frame, &mut rgb).map_err(|e| err(path, "scale", e))?;
        }
        let stride = rgb.stride(0);
        let data = rgb.data(0);
        packed.clear();
        for y in 0..oh as usize {
            packed.extend_from_slice(&data[y * stride..y * stride + ow as usize * 3]);
        }
        f(i, t, &packed, ow, oh)
    };

    for (stream, packet) in ictx.packets() {
        if stream.index() != index {
            continue;
        }
        if dec.send_packet(&packet).is_err() {
            continue; // a damaged packet shouldn't end the scan
        }
        while dec.receive_frame(&mut frame).is_ok() {
            handle(&frame)?;
        }
    }
    let _ = dec.send_eof();
    while dec.receive_frame(&mut frame).is_ok() {
        handle(&frame)?;
    }
    let fps = if fps > 0.0 {
        fps
    } else if last_t > 0.0 {
        (n.saturating_sub(1)) as f64 / last_t
    } else {
        30.0
    };
    Ok(Info {
        fps,
        width: src_w,
        height: src_h,
        frames: n,
        duration: if n == 0 { 0.0 } else { last_t + 1.0 / fps },
        gpu,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn fits() {
        assert_eq!(super::fit(1920, 1080, 160), (160, 90));
        assert_eq!(super::fit(1080, 1920, 160), (90, 160));
        assert_eq!(super::fit(720, 1280, 960), (540, 960));
    }
}
