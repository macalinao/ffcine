//! Detect on the GPU: NVDEC into CUDA frames, `setpts,fps,scale_cuda` (libavfilter, in VRAM) to
//! 8-bit NV12 at the analysis size, the luma plane copied device-to-device into NVIDIA optical
//! flow, and a robust similarity per frame pair on the CPU.

use std::path::Path;
use std::ptr;

use ff::codec::context::Context as CodecContext;
use ff::ffi;
use ff::media::Type as Media;
use ffmpeg_next as ff;

use super::cuda::{CuContext, Mem};
use super::motion::{self, FlowField, Motion};
use super::nvof::{OpticalFlow, PerfLevel};
use super::{DetectParams, Detection, FlowQuality, SourceRange};
use ffcine_core::{Error, Result};

/// The analysis frame for a `w x h` source at `width` (clamped to the source): aspect kept,
/// both sides even (NV12).
pub fn analysis_size(w: u32, h: u32, width: u32) -> (u32, u32) {
    let aw = (width.min(w) / 2 * 2).max(2);
    let ah = ((f64::from(h) * f64::from(aw) / f64::from(w) / 2.0).round() as u32 * 2).max(2);
    (aw, ah)
}

/// An `AVBufferRef` to a CUDA `AVHWDeviceContext`.
struct HwDevice(*mut ffi::AVBufferRef);

impl HwDevice {
    fn cuda() -> Result<Self> {
        let mut dev = ptr::null_mut();
        // SAFETY: out-pointer; default device, no options.
        let r = unsafe {
            ffi::av_hwdevice_ctx_create(
                &raw mut dev,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        };
        if r < 0 {
            return Err(Error::Gpu {
                api: "CUDA",
                msg: format!("av_hwdevice_ctx_create: {}", ff::Error::from(r)),
            });
        }
        Ok(Self(dev))
    }

    // `data` is av_malloc'd, so suitably aligned.
    #[allow(clippy::cast_ptr_alignment)]
    fn cuda_ctx(&self) -> CuContext {
        // SAFETY: a CUDA device context's `hwctx` is an `AVCUDADeviceContext`.
        unsafe {
            let dev = (*self.0).data.cast::<ffi::AVHWDeviceContext>();
            (*(*dev).hwctx.cast::<ffi::AVCUDADeviceContext>())
                .cuda_ctx
                .cast()
        }
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: our reference, released once.
        unsafe { ffi::av_buffer_unref(&raw mut self.0) };
    }
}

/// Picks CUDA frames when the decoder offers them (NVDEC); otherwise none, so opening fails.
unsafe extern "C" fn cuda_format(
    _ctx: *mut ffi::AVCodecContext,
    mut fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: libavcodec passes an AV_PIX_FMT_NONE-terminated list.
    unsafe {
        while *fmts != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmts == ffi::AVPixelFormat::AV_PIX_FMT_CUDA {
                return *fmts;
            }
            fmts = fmts.add(1);
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

/// `buffer -> setpts=PTS-STARTPTS,fps=rate,scale_cuda=W:H:format=nv12 -> buffersink` over the
/// decoder's CUDA frames context (known once the first frame is out).
fn build_graph(
    first: &ff::frame::Video,
    tb: ff::Rational,
    range: &SourceRange,
    (aw, ah): (u32, u32),
) -> Result<ff::filter::Graph> {
    let mut g = ff::filter::Graph::new();
    // A hardware-format buffer source needs its frames context before init, which
    // `Graph::add` (avfilter_graph_create_filter) doesn't allow: allocate, set, then init.
    // SAFETY: `par` is freed here; buffersrc takes its own ref on `hw_frames_ctx`.
    unsafe {
        let filt = ffi::avfilter_get_by_name(c"buffer".as_ptr());
        let src = if filt.is_null() {
            ptr::null_mut()
        } else {
            ffi::avfilter_graph_alloc_filter(g.as_mut_ptr(), filt, c"in".as_ptr())
        };
        let par = ffi::av_buffersrc_parameters_alloc();
        if src.is_null() || par.is_null() {
            ffi::av_free(par.cast());
            return Err(Error::Unimplemented("libavfilter buffer source"));
        }
        (*par).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA as i32;
        (*par).time_base = tb.into();
        (*par).width = (*first.as_ptr()).width;
        (*par).height = (*first.as_ptr()).height;
        (*par).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
        (*par).hw_frames_ctx = (*first.as_ptr()).hw_frames_ctx;
        let r = ffi::av_buffersrc_parameters_set(src, par);
        ffi::av_free(par.cast());
        let r = if r < 0 {
            r
        } else {
            ffi::avfilter_init_str(src, ptr::null())
        };
        if r < 0 {
            return Err(ff::Error::from(r).into());
        }
    }
    let sink =
        ff::filter::find("buffersink").ok_or(Error::Unimplemented("libavfilter buffersink"))?;
    g.add(&sink, "out", "")?;
    let spec = format!(
        "setpts=PTS-STARTPTS,fps={},scale_cuda={aw}:{ah}:format=nv12",
        range.rate
    );
    g.output("in", 0)?.input("out", 0)?.parse(&spec)?;
    g.validate()?;
    Ok(g)
}

/// Per analysed frame: flow against the previous frame, then the global motion.
struct Analyser {
    of: OpticalFlow,
    params: DetectParams,
    scale: (f64, f64),
    want: usize,
    motions: Vec<Motion>,
    flow: Vec<[i16; 2]>,
    flow_px: Vec<[f32; 2]>,
    cost: Vec<u8>,
}

impl Analyser {
    fn done(&self) -> bool {
        self.motions.len() >= self.want
    }

    fn frame(&mut self, f: &ff::frame::Video) -> Result<()> {
        if self.done() {
            return Ok(());
        }
        let k = self.motions.len();
        // SAFETY: an NV12 CUDA frame of the session's size; data[0] is the luma device pointer.
        unsafe {
            let raw = f.as_ptr();
            let pitch = usize::try_from((*raw).linesize[0]).map_err(|_| Error::Invalid {
                what: "CUDA frame pitch",
                value: (*raw).linesize[0].to_string(),
            })?;
            self.of
                .upload(k % 2, Mem::Device((*raw).data[0] as u64), pitch)?;
        }
        if k == 0 {
            self.motions.push(Motion::IDENTITY);
            return Ok(());
        }
        self.of
            .run(k % 2, (k - 1) % 2, &mut self.flow, &mut self.cost)?;
        self.flow_px.clear();
        self.flow_px.extend(
            self.flow
                .iter()
                .map(|v| [f32::from(v[0]) / 32.0, f32::from(v[1]) / 32.0]),
        );
        let field = FlowField {
            width: self.of.width,
            height: self.of.height,
            grid: self.of.grid,
            cols: self.of.grid_w as usize,
            rows: self.of.grid_h as usize,
            flow: &self.flow_px,
            cost: &self.cost,
        };
        let m = motion::estimate(&field, &self.params.estimate);
        self.motions.push(m.rescaled(self.scale.0, self.scale.1));
        Ok(())
    }

    /// Pulls everything the graph has ready.
    fn pull(&mut self, graph: &mut ff::filter::Graph, out: &mut ff::frame::Video) -> Result<()> {
        let Some(mut sink) = graph.get("out") else {
            return Ok(());
        };
        loop {
            match sink.sink().frame(out) {
                Ok(()) => self.frame(out)?,
                Err(
                    ff::Error::Other {
                        errno: ff::error::EAGAIN,
                    }
                    | ff::Error::Eof,
                ) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// The input positioned at the range start, with an NVDEC decoder.
struct Source {
    ictx: ff::format::context::Input,
    dec: ff::decoder::Video,
    index: usize,
    tb: ff::Rational,
    /// Decoded frames before this pts (stream time base) are dropped.
    start_pts: i64,
    device: HwDevice,
}

fn open(path: &Path, range: &SourceRange) -> Result<Source> {
    let mut ictx = ff::format::input(path)?;
    let (index, tb, codecpar) = {
        let s = ictx.streams().best(Media::Video).ok_or(Error::Invalid {
            what: "input",
            value: format!("{}: no video stream", path.display()),
        })?;
        (s.index(), s.time_base(), s.parameters())
    };
    let device = HwDevice::cuda()?;
    let mut cctx = CodecContext::from_parameters(codecpar)?;
    // SAFETY: set before avcodec_open2; the codec context takes its own device reference.
    unsafe {
        let c = cctx.as_mut_ptr();
        (*c).hw_device_ctx = ffi::av_buffer_ref(device.0);
        (*c).get_format = Some(cuda_format);
    }
    let dec = cctx.decoder().video()?;

    // Like the ffmpeg CLI's `-ss`: seek to the keyframe before, then drop frames before `start`
    // (file start time included).
    // SAFETY: plain field read.
    let file_start = match unsafe { (*ictx.as_ptr()).start_time } {
        ffi::AV_NOPTS_VALUE => 0,
        t => t,
    };
    let target = file_start + (range.start * f64::from(ffi::AV_TIME_BASE)).round() as i64;
    if range.start > 0.0 {
        // SAFETY: valid context; any timestamp at or before the target.
        let r =
            unsafe { ffi::avformat_seek_file(ictx.as_mut_ptr(), -1, i64::MIN, target, target, 0) };
        if r < 0 {
            return Err(ff::Error::from(r).into());
        }
    }
    let av_time_base = ffi::AVRational {
        num: 1,
        den: ffi::AV_TIME_BASE,
    };
    // SAFETY: pure arithmetic.
    let start_pts = unsafe { ffi::av_rescale_q(target, av_time_base, tb.into()) };
    Ok(Source {
        ictx,
        dec,
        index,
        tb,
        start_pts,
        device,
    })
}

/// Detects frame-to-frame motion over `range` of `path`'s best video stream.
pub fn detect(path: &Path, range: &SourceRange, params: &DetectParams) -> Result<Detection> {
    ff::init()?;
    if range.frames == 0 || range.rate.num <= 0 || range.rate.den <= 0 {
        return Err(Error::Invalid {
            what: "stabilize range",
            value: format!("{range:?}"),
        });
    }
    let Source {
        mut ictx,
        mut dec,
        index,
        tb,
        start_pts,
        device,
    } = open(path, range)?;
    let (w, h) = (dec.width(), dec.height());
    let (aw, ah) = analysis_size(w, h, params.analysis_width);

    let perf = match params.quality {
        FlowQuality::Slow => PerfLevel::Slow,
        FlowQuality::Medium => PerfLevel::Medium,
        FlowQuality::Fast => PerfLevel::Fast,
    };
    // SAFETY: the context belongs to `device`, which `dec` keeps alive past `an` (dropped first).
    let of = unsafe { OpticalFlow::new(device.cuda_ctx(), aw, ah, params.grid, perf)? };
    let mut an = Analyser {
        of,
        params: *params,
        scale: (f64::from(w) / f64::from(aw), f64::from(h) / f64::from(ah)),
        want: range.frames,
        motions: Vec::with_capacity(range.frames),
        flow: Vec::new(),
        flow_px: Vec::new(),
        cost: Vec::new(),
    };

    let mut graph: Option<ff::filter::Graph> = None;
    let mut decoded = ff::frame::Video::empty();
    let mut out = ff::frame::Video::empty();
    let mut push = |dec: &mut ff::decoder::Video,
                    graph: &mut Option<ff::filter::Graph>,
                    an: &mut Analyser|
     -> Result<()> {
        while dec.receive_frame(&mut decoded).is_ok() {
            if decoded.timestamp().or(decoded.pts()).unwrap_or(0) < start_pts {
                continue;
            }
            if graph.is_none() {
                *graph = Some(build_graph(&decoded, tb, range, (aw, ah))?);
            }
            let Some(g) = graph.as_mut() else { continue };
            g.get("in")
                .ok_or(Error::Unimplemented("filter graph input"))?
                .source()
                .add(&decoded)?;
            an.pull(g, &mut out)?;
        }
        Ok(())
    };
    for (s, packet) in ictx.packets() {
        if an.done() {
            break;
        }
        if s.index() != index {
            continue;
        }
        dec.send_packet(&packet)?;
        push(&mut dec, &mut graph, &mut an)?;
    }
    if !an.done() {
        dec.send_eof()?;
        push(&mut dec, &mut graph, &mut an)?;
        if let Some(g) = graph.as_mut() {
            if let Some(mut src) = g.get("in") {
                src.source().flush()?;
            }
            an.pull(g, &mut out)?;
        }
    }
    if an.motions.is_empty() {
        return Err(Error::Invalid {
            what: "stabilize range",
            value: format!("no frames at {} s in {}", range.start, path.display()),
        });
    }
    // render.py pads short sources by cloning the last frame: no motion.
    an.motions.resize(range.frames, Motion::IDENTITY);
    Ok(Detection {
        width: w,
        height: h,
        analysis_width: aw,
        analysis_height: ah,
        motions: std::mem::take(&mut an.motions),
    })
}

#[cfg(test)]
mod tests {
    use super::analysis_size;

    #[test]
    fn analysis_sizes() {
        assert_eq!(analysis_size(3840, 2160, 1920), (1920, 1080));
        assert_eq!(analysis_size(1920, 1080, 4000), (1920, 1080));
        assert_eq!(analysis_size(3840, 1606, 1280), (1280, 536));
        assert_eq!(analysis_size(1080, 1920, 960), (960, 1706));
    }
}
