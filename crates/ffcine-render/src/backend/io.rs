//! Demux + decode (NVDEC, software fallback) and encode + mux, on raw libav*.

use std::os::raw::c_int;
use std::path::Path;
use std::ptr;
use std::time::Duration;

use ffmpeg_next::ffi as ff;

use super::av::{BufferRef, CodecCtx, Dict, Frame, check, cstr, gpu_err};
use super::retime::Q;
use crate::Result;
use crate::encode::{Codec, EncoderSettings, PixelFormat, RateControl};
use ffcine_core::color::{ColorSpace, HdrMetadata, Matrix, Primaries, Range, Transfer};

const AVERROR_EOF: c_int = -0x2046_4F45; // FFERRTAG('E','O','F',' ')
const EAGAIN: c_int = -11;

fn path_cstr(p: &Path) -> Result<std::ffi::CString> {
    cstr(&p.to_string_lossy())
}

/// An input file's video stream with a decoder on it.
#[derive(Debug)]
pub(crate) struct Input {
    fmt: *mut ff::AVFormatContext,
    stream: c_int,
    dec: CodecCtx,
    pkt: *mut ff::AVPacket,
    eof: bool,
    pub tb: Q,
    /// First pts to keep after the seek (stream time base).
    pub trim: i64,
}

// SAFETY: used by one thread at a time.
unsafe impl Send for Input {}

/// Picks the hw format of the decoder's device (CUDA or Vulkan), else the first software format.
unsafe extern "C" fn get_format(
    ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    // SAFETY: libav passes its context and a NONE-terminated list.
    unsafe {
        let dev = (*ctx).hw_device_ctx;
        let hw = if !dev.is_null()
            && (*(*dev).data.cast::<ff::AVHWDeviceContext>()).type_
                == ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN
        {
            ff::AVPixelFormat::AV_PIX_FMT_VULKAN
        } else {
            ff::AVPixelFormat::AV_PIX_FMT_CUDA
        };
        let mut p = fmts;
        let mut sw = ff::AVPixelFormat::AV_PIX_FMT_NONE;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == hw {
                return *p;
            }
            let d = ff::av_pix_fmt_desc_get(*p);
            if sw == ff::AVPixelFormat::AV_PIX_FMT_NONE
                && !d.is_null()
                && (*d).flags & ff::AV_PIX_FMT_FLAG_HWACCEL as u64 == 0
            {
                sw = *p;
            }
            p = p.add(1);
        }
        sw
    }
}

impl Input {
    /// Opens `path`, seeks to `start` like ffmpeg's input `-ss`, and sets up hardware decode on
    /// `device` (CUDA or Vulkan); codecs it can't decode fall back to software.
    pub(crate) fn open(path: &Path, start: Duration, device: &BufferRef) -> Result<Self> {
        let p = path_cstr(path)?;
        let mut fmt = ptr::null_mut();
        // SAFETY: standard libavformat open sequence; everything is freed in Drop.
        unsafe {
            check(
                ff::avformat_open_input(&raw mut fmt, p.as_ptr(), ptr::null(), ptr::null_mut()),
                &format!("open {}", path.display()),
            )?;
            let mut this = Self {
                fmt,
                stream: 0,
                dec: CodecCtx::new(ptr::null())?,
                pkt: ff::av_packet_alloc(),
                eof: false,
                tb: Q::new(1, 1),
                trim: i64::MIN,
            };
            check(
                ff::avformat_find_stream_info(fmt, ptr::null_mut()),
                "find stream info",
            )?;
            let mut codec = ptr::null();
            this.stream = check(
                ff::av_find_best_stream(
                    fmt,
                    ff::AVMediaType::AVMEDIA_TYPE_VIDEO,
                    -1,
                    -1,
                    &raw mut codec,
                    0,
                ),
                "no video stream",
            )?;
            let st = *(*fmt)
                .streams
                .add(usize::try_from(this.stream).unwrap_or(0));
            this.tb = Q::new(
                i64::from((*st).time_base.num),
                i64::from((*st).time_base.den),
            );
            // Only the video stream is demuxed.
            for i in 0..(*fmt).nb_streams as usize {
                let s = *(*fmt).streams.add(i);
                if (*s).index != this.stream {
                    (*s).discard = ff::AVDiscard::AVDISCARD_ALL;
                }
            }

            this.dec = CodecCtx::new(codec)?;
            let d = this.dec.get_mut();
            check(
                ff::avcodec_parameters_to_context(d, (*st).codecpar),
                "decoder params",
            )?;
            d.pkt_timebase = (*st).time_base;
            // As the ffmpeg CLI does: the decoder doesn't know the rate of MP4/MOV streams, and
            // the output rate (with no --fps) comes from here.
            d.framerate = ff::av_guess_frame_rate(fmt, st, ptr::null_mut());
            d.hw_device_ctx = device.new_ref();
            d.get_format = Some(get_format);
            // Frames we hold while the GPU works on them.
            d.extra_hw_frames = 8;
            check(ff::avcodec_open2(d, codec, ptr::null_mut()), "open decoder")?;

            // ffmpeg -ss: seek to the keyframe at or before start (+ container start time), then
            // drop frames before it.
            let start_us = i64::try_from(start.as_micros()).unwrap_or(i64::MAX);
            let base = if (*fmt).start_time == ff::AV_NOPTS_VALUE {
                0
            } else {
                (*fmt).start_time
            };
            if start_us > 0 {
                let ts = base + start_us;
                check(ff::avformat_seek_file(fmt, -1, i64::MIN, ts, ts, 0), "seek")?;
                this.trim = super::retime::rescale_near(ts, Q::new(1, 1_000_000), this.tb);
            }
            Ok(this)
        }
    }

    pub(crate) fn decoder(&self) -> &ff::AVCodecContext {
        // SAFETY: valid context.
        unsafe { &*self.dec.as_ptr() }
    }

    /// The next decoded frame, or `None` at end of stream.
    pub(crate) fn next_frame(&mut self) -> Result<Option<Frame>> {
        let mut f = Frame::new()?;
        loop {
            // SAFETY: valid decoder and frame.
            let r = unsafe { ff::avcodec_receive_frame(self.dec.as_ptr(), f.as_ptr()) };
            if r >= 0 {
                let fr = f.get_mut();
                if fr.pts == ff::AV_NOPTS_VALUE {
                    fr.pts = fr.best_effort_timestamp;
                }
                return Ok(Some(f));
            }
            if r == AVERROR_EOF {
                return Ok(None);
            }
            if r != EAGAIN {
                check(r, "decode")?;
            }
            self.feed()?;
        }
    }

    fn feed(&mut self) -> Result<()> {
        // SAFETY: valid contexts and packet.
        unsafe {
            loop {
                if self.eof {
                    // Drain; a second flush reports EOF, which receive_frame then returns too.
                    let r = ff::avcodec_send_packet(self.dec.as_ptr(), ptr::null());
                    return if r == AVERROR_EOF {
                        Ok(())
                    } else {
                        check(r, "flush decoder").map(drop)
                    };
                }
                let r = ff::av_read_frame(self.fmt, self.pkt);
                if r == AVERROR_EOF {
                    self.eof = true;
                    continue;
                }
                check(r, "read packet")?;
                let mine = (*self.pkt).stream_index == self.stream;
                let s = if mine {
                    ff::avcodec_send_packet(self.dec.as_ptr(), self.pkt)
                } else {
                    0
                };
                ff::av_packet_unref(self.pkt);
                if mine {
                    check(s, "send packet")?;
                    return Ok(());
                }
            }
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: we own these.
        unsafe {
            ff::av_packet_free(&raw mut self.pkt);
            ff::avformat_close_input(&raw mut self.fmt);
        }
    }
}

fn is_mov(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "mov" | "m4v"))
}

pub(crate) fn av_pix_fmt(p: PixelFormat) -> ff::AVPixelFormat {
    use ff::AVPixelFormat as F;
    match p {
        PixelFormat::Nv12 => F::AV_PIX_FMT_NV12,
        PixelFormat::Yuv420p => F::AV_PIX_FMT_YUV420P,
        PixelFormat::Yuv420p10 => F::AV_PIX_FMT_YUV420P10LE,
        PixelFormat::Gbrp16 => F::AV_PIX_FMT_GBRP16LE,
        PixelFormat::Rgba => F::AV_PIX_FMT_RGBA,
        _ => F::AV_PIX_FMT_P010LE,
    }
}

pub(crate) fn av_primaries(p: Primaries) -> ff::AVColorPrimaries {
    use ff::AVColorPrimaries as P;
    match p {
        Primaries::Bt709 => P::AVCOL_PRI_BT709,
        Primaries::Bt2020 => P::AVCOL_PRI_BT2020,
        Primaries::DisplayP3 => P::AVCOL_PRI_SMPTE432,
        Primaries::DciP3 => P::AVCOL_PRI_SMPTE431,
        Primaries::Bt601 => P::AVCOL_PRI_SMPTE170M,
        _ => P::AVCOL_PRI_UNSPECIFIED,
    }
}

pub(crate) fn av_transfer(t: Transfer) -> ff::AVColorTransferCharacteristic {
    use ff::AVColorTransferCharacteristic as T;
    match t {
        Transfer::Bt709 => T::AVCOL_TRC_BT709,
        Transfer::Srgb => T::AVCOL_TRC_IEC61966_2_1,
        Transfer::Pq => T::AVCOL_TRC_SMPTE2084,
        Transfer::Hlg => T::AVCOL_TRC_ARIB_STD_B67,
        Transfer::Linear => T::AVCOL_TRC_LINEAR,
        _ => T::AVCOL_TRC_UNSPECIFIED,
    }
}

pub(crate) fn av_matrix(m: Matrix) -> ff::AVColorSpace {
    use ff::AVColorSpace as S;
    match m {
        Matrix::Bt709 => S::AVCOL_SPC_BT709,
        Matrix::Bt2020Ncl => S::AVCOL_SPC_BT2020_NCL,
        Matrix::Bt601 => S::AVCOL_SPC_SMPTE170M,
        Matrix::Rgb => S::AVCOL_SPC_RGB,
        _ => S::AVCOL_SPC_UNSPECIFIED,
    }
}

pub(crate) fn av_range(r: Range) -> ff::AVColorRange {
    use ff::AVColorRange as R;
    match r {
        Range::Limited => R::AVCOL_RANGE_MPEG,
        Range::Full => R::AVCOL_RANGE_JPEG,
        _ => R::AVCOL_RANGE_UNSPECIFIED,
    }
}

/// Tags `f` with `c`.
pub(crate) fn tag_frame(f: &mut ff::AVFrame, c: ColorSpace) {
    f.color_primaries = av_primaries(c.primaries);
    f.color_trc = av_transfer(c.transfer);
    f.colorspace = av_matrix(c.matrix);
    f.color_range = av_range(c.range);
}

/// libavutil's `AVMasteringDisplayMetadata` (`mastering_display_metadata.h`, not in the
/// bindings).
#[repr(C)]
struct MasteringDisplay {
    display_primaries: [[ff::AVRational; 2]; 3],
    white_point: [ff::AVRational; 2],
    min_luminance: ff::AVRational,
    max_luminance: ff::AVRational,
    has_primaries: c_int,
    has_luminance: c_int,
}

/// libavutil's `AVContentLightMetadata`.
#[repr(C)]
struct ContentLight {
    max_cll: std::os::raw::c_uint,
    max_fall: std::os::raw::c_uint,
}

/// `v` as a fraction over `den` (the units HEVC's SEI uses: 0.00002 for xy, 0.0001 cd/m²).
#[allow(clippy::cast_possible_truncation)]
fn q(v: f64, den: i32) -> ff::AVRational {
    ff::AVRational {
        num: (v * f64::from(den)).round() as i32,
        den,
    }
}

impl MasteringDisplay {
    #[allow(clippy::many_single_char_names)]
    fn new(m: &HdrMetadata) -> Self {
        let xy = |[x, y]: [f64; 2]| [q(x, 50_000), q(y, 50_000)];
        let [r, g, b, w] = m.primaries;
        Self {
            display_primaries: [xy(r), xy(g), xy(b)],
            white_point: xy(w),
            min_luminance: q(m.min_luminance, 10_000),
            max_luminance: q(m.max_luminance, 10_000),
            has_primaries: 1,
            has_luminance: 1,
        }
    }
}

/// `m`'s two side data entries as `(frame type, packet type, bytes)`.
fn hdr_side_data(
    m: &HdrMetadata,
) -> [(ff::AVFrameSideDataType, ff::AVPacketSideDataType, Vec<u8>); 2] {
    fn bytes<T>(v: &T) -> Vec<u8> {
        // SAFETY: plain-old-data repr(C) structs.
        unsafe { std::slice::from_raw_parts(ptr::from_ref(v).cast::<u8>(), size_of::<T>()) }
            .to_vec()
    }
    use ff::AVFrameSideDataType as F;
    use ff::AVPacketSideDataType as P;
    [
        (
            F::AV_FRAME_DATA_MASTERING_DISPLAY_METADATA,
            P::AV_PKT_DATA_MASTERING_DISPLAY_METADATA,
            bytes(&MasteringDisplay::new(m)),
        ),
        (
            F::AV_FRAME_DATA_CONTENT_LIGHT_LEVEL,
            P::AV_PKT_DATA_CONTENT_LIGHT_LEVEL,
            bytes(&ContentLight {
                max_cll: m.max_cll,
                max_fall: m.max_fall,
            }),
        ),
    ]
}

/// Attaches `m` to `f` as mastering display and content light side data (read by libplacebo
/// for the target's peak, and by encoders that write the SEI per frame).
pub(crate) fn attach_hdr(f: *mut ff::AVFrame, m: &HdrMetadata) -> Result<()> {
    for (t, _, data) in hdr_side_data(m) {
        // SAFETY: a valid frame; the new entry has `data.len()` bytes.
        unsafe {
            let sd = ff::av_frame_new_side_data(f, t, data.len());
            if sd.is_null() {
                return Err(gpu_err("av_frame_new_side_data"));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*sd).data, data.len());
        }
    }
    Ok(())
}

/// An encoder writing one video stream into a container.
#[derive(Debug)]
pub(crate) struct Output {
    fmt: *mut ff::AVFormatContext,
    enc: CodecCtx,
    st: *mut ff::AVStream,
    pkt: *mut ff::AVPacket,
    header: bool,
    pub frames: Option<BufferRef>,
}

// SAFETY: used by one thread at a time.
unsafe impl Send for Output {}

impl Output {
    /// `frames`: a CUDA or Vulkan frames context for hardware encoders (frames are sent as hw frames);
    /// `None` sends software frames in `settings.pixel_format`. `hdr`: static HDR metadata.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        path: &Path,
        settings: &EncoderSettings,
        width: u32,
        height: u32,
        rate: Q,
        chroma: ff::AVChromaLocation,
        frames: Option<&BufferRef>,
        hdr: Option<&HdrMetadata>,
    ) -> Result<Self> {
        let p = path_cstr(path)?;
        let name = cstr(settings.codec.as_str())?;
        // SAFETY: standard libavformat/libavcodec output sequence; freed in Drop.
        unsafe {
            let mut fmt = ptr::null_mut();
            check(
                ff::avformat_alloc_output_context2(
                    &raw mut fmt,
                    ptr::null(),
                    ptr::null(),
                    p.as_ptr(),
                ),
                &format!("output format for {}", path.display()),
            )?;
            let codec = ff::avcodec_find_encoder_by_name(name.as_ptr());
            if codec.is_null() {
                ff::avformat_free_context(fmt);
                return Err(gpu_err(format!("encoder {} not available", settings.codec)));
            }
            let mut this = Self {
                fmt,
                enc: CodecCtx::new(codec)?,
                st: ptr::null_mut(),
                pkt: ff::av_packet_alloc(),
                header: false,
                frames: frames.cloned(),
            };
            let e = this.enc.get_mut();
            e.width = i32::try_from(width).unwrap_or(0);
            e.height = i32::try_from(height).unwrap_or(0);
            let (num, den) = (
                i32::try_from(rate.num).unwrap_or(1),
                i32::try_from(rate.den).unwrap_or(1),
            );
            e.time_base = ff::AVRational { num: den, den: num };
            e.framerate = ff::AVRational { num, den };
            e.sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };
            let sw = av_pix_fmt(settings.pixel_format);
            if let Some(fr) = frames {
                e.pix_fmt = (*fr.data::<ff::AVHWFramesContext>()).format;
                e.sw_pix_fmt = sw;
                e.hw_frames_ctx = fr.new_ref();
            } else {
                e.pix_fmt = sw;
            }
            e.color_primaries = av_primaries(settings.color.primaries);
            e.color_trc = av_transfer(settings.color.transfer);
            e.colorspace = av_matrix(settings.color.matrix);
            e.color_range = av_range(settings.color.range);
            e.chroma_sample_location = chroma;
            if let Some(g) = settings.gop {
                e.gop_size = i32::try_from(g).unwrap_or(i32::MAX);
            }
            if (*(*fmt).oformat).flags & ff::AVFMT_GLOBALHEADER as c_int != 0 {
                e.flags |= c_int::try_from(ff::AV_CODEC_FLAG_GLOBAL_HEADER).unwrap_or(0);
            }
            let mut opts = Dict::default();
            // Vulkan encoders have no presets.
            let preset = settings
                .preset
                .as_ref()
                .filter(|_| !settings.codec.is_vulkan());
            for (k, v) in [
                ("preset", &preset.cloned()),
                ("tune", &settings.tune),
                ("profile", &settings.profile),
            ] {
                if let Some(v) = v {
                    opts.set(k, v)?;
                }
            }
            let nvenc = matches!(
                settings.codec,
                Codec::HevcNvenc | Codec::H264Nvenc | Codec::Av1Nvenc
            );
            match settings.rate {
                RateControl::ConstQp(_) if settings.codec == Codec::Png => {}
                RateControl::ConstQp(q) => {
                    if nvenc {
                        opts.set("rc", "constqp")?;
                    }
                    opts.set("qp", &q.to_string())?;
                }
                RateControl::Crf(c) => {
                    opts.set(if nvenc { "cq" } else { "crf" }, &c.to_string())?;
                }
                RateControl::Bitrate(b) => e.bit_rate = i64::try_from(b).unwrap_or(i64::MAX),
            }
            for (k, v) in &settings.extra {
                opts.set(k, v)?;
            }
            // Static HDR metadata for encoders that write it from the context (libx265, NVENC).
            for (t, _, data) in hdr.map(hdr_side_data).into_iter().flatten() {
                let sd = ff::av_frame_side_data_new(
                    &raw mut e.decoded_side_data,
                    &raw mut e.nb_decoded_side_data,
                    t,
                    data.len(),
                    0,
                );
                if sd.is_null() {
                    return Err(gpu_err("av_frame_side_data_new"));
                }
                ptr::copy_nonoverlapping(data.as_ptr(), (*sd).data, data.len());
            }
            check(
                ff::avcodec_open2(e, codec, opts.as_mut_ptr()),
                "open encoder",
            )?;
            let left = opts.leftover();
            if !left.is_empty() {
                return Err(gpu_err(format!(
                    "encoder ignored options: {}",
                    left.join(", ")
                )));
            }

            this.st = ff::avformat_new_stream(fmt, ptr::null());
            if this.st.is_null() {
                return Err(gpu_err("avformat_new_stream"));
            }
            check(
                ff::avcodec_parameters_from_context((*this.st).codecpar, e),
                "stream params",
            )?;
            // ... and for the container (`mdcv` / `clli` boxes in MP4 and MOV).
            let par = (*this.st).codecpar;
            for (_, t, data) in hdr.map(hdr_side_data).into_iter().flatten() {
                let sd = ff::av_packet_side_data_new(
                    &raw mut (*par).coded_side_data,
                    &raw mut (*par).nb_coded_side_data,
                    t,
                    data.len(),
                    0,
                );
                if sd.is_null() {
                    return Err(gpu_err("av_packet_side_data_new"));
                }
                ptr::copy_nonoverlapping(data.as_ptr(), (*sd).data, data.len());
            }
            // Apple players only take HEVC in MP4/MOV tagged `hvc1` (ffmpeg writes `hev1`).
            if matches!(
                settings.codec,
                Codec::HevcNvenc | Codec::HevcVaapi | Codec::HevcVulkan | Codec::Libx265
            ) && is_mov(path)
            {
                (*(*this.st).codecpar).codec_tag = u32::from_le_bytes(*b"hvc1");
            }
            (*this.st).time_base = e.time_base;
            (*this.st).avg_frame_rate = e.framerate;
            (*this.st).sample_aspect_ratio = e.sample_aspect_ratio;
            if (*(*fmt).oformat).flags & ff::AVFMT_NOFILE as c_int == 0 {
                check(
                    ff::avio_open(&raw mut (*fmt).pb, p.as_ptr(), ff::AVIO_FLAG_WRITE as c_int),
                    &format!("create {}", path.display()),
                )?;
            }
            check(
                ff::avformat_write_header(fmt, ptr::null_mut()),
                "write header",
            )?;
            this.header = true;
            Ok(this)
        }
    }

    /// Encodes `frame` (`None` flushes) and writes the packets.
    pub(crate) fn encode(&mut self, frame: Option<&Frame>) -> Result<()> {
        // SAFETY: valid encoder, muxer and packet.
        unsafe {
            let f = frame.map_or(ptr::null(), |f| f.as_ptr().cast_const());
            check(ff::avcodec_send_frame(self.enc.as_ptr(), f), "encode")?;
            loop {
                let r = ff::avcodec_receive_packet(self.enc.as_ptr(), self.pkt);
                if r == EAGAIN || r == AVERROR_EOF {
                    return Ok(());
                }
                check(r, "encode")?;
                (*self.pkt).stream_index = 0;
                ff::av_packet_rescale_ts(
                    self.pkt,
                    (*self.enc.as_ptr()).time_base,
                    (*self.st).time_base,
                );
                check(
                    ff::av_interleaved_write_frame(self.fmt, self.pkt),
                    "write packet",
                )?;
            }
        }
    }

    /// Flushes the encoder and finalizes the file.
    pub(crate) fn finish(mut self) -> Result<()> {
        self.encode(None)?;
        // SAFETY: header was written.
        check(unsafe { ff::av_write_trailer(self.fmt) }, "write trailer")?;
        self.header = false;
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: we own these.
        unsafe {
            ff::av_packet_free(&raw mut self.pkt);
            if !self.fmt.is_null() {
                if (*(*self.fmt).oformat).flags & ff::AVFMT_NOFILE as c_int == 0 {
                    ff::avio_closep(&raw mut (*self.fmt).pb);
                }
                ff::avformat_free_context(self.fmt);
            }
        }
    }
}
