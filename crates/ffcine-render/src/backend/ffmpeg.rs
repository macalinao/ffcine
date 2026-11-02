//! ffmpeg (libav*) + libplacebo backend.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::Duration;

use ff::codec::context::Context as CodecContext;
use ff::media::Type as Media;
use ff::util::color;
use ffmpeg_next as ff;

use crate::{Backend, Pipeline, Result, RunStats};
use ffcine_core::color::{ColorSpace, Matrix, Primaries, Range, Transfer};
use ffcine_core::probe::{AudioInfo, MediaInfo, Rational, StreamInfo, StreamKind, VideoInfo};

/// Demux/decode/encode/mux with libav*; GPU processing with libplacebo on ffmpeg's Vulkan device.
///
/// The GPU state (devices, libplacebo renderer, shader cache) is created by the first
/// [`Backend::run`] and reused by later ones, so a process rendering many shots pays device init
/// and shader compilation once.
#[derive(Debug)]
pub struct Ffmpeg {
    #[cfg(feature = "placebo")]
    gpu: Mutex<Option<super::gpu::Gpu>>,
    cache_dir: Option<PathBuf>,
}

impl Ffmpeg {
    /// Initializes libav* (once per process) and quiets its logging to errors.
    pub fn new() -> Result<Self> {
        static INIT: Once = Once::new();
        let mut res = Ok(());
        INIT.call_once(|| {
            res = ff::init();
            ff::util::log::set_level(ff::util::log::Level::Error);
        });
        res?;
        Ok(Self {
            #[cfg(feature = "placebo")]
            gpu: Mutex::new(None),
            #[cfg(feature = "placebo")]
            cache_dir: super::gpu::default_cache_dir(),
            #[cfg(not(feature = "placebo"))]
            cache_dir: None,
        })
    }

    /// Where libplacebo's shader cache lives (`None`: no on-disk cache). Takes effect before the
    /// first run.
    #[must_use]
    pub fn with_cache_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.cache_dir = dir;
        self
    }
}

impl Backend for Ffmpeg {
    fn name(&self) -> &'static str {
        "ffmpeg+libplacebo"
    }

    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        let ictx = ff::format::input(path)?;
        let mut streams = ictx
            .streams()
            .map(|s| stream_info(&s))
            .collect::<Result<Vec<_>>>()?;
        // A tiled HEIF still: one video stream at the displayed size, not one per tile.
        // SAFETY: an opened context with stream info.
        if let Some(g) = unsafe { super::tiles::TileGrid::find(ictx.as_ptr()) } {
            let first = g.tiles.first().and_then(|t| usize::try_from(t.0).ok());
            streams.retain(|s| {
                let i = std::os::raw::c_int::try_from(s.index).unwrap_or(-1);
                !g.has_stream(i) || Some(s.index) == first
            });
            for s in streams.iter_mut().filter(|s| Some(s.index) == first) {
                if let StreamKind::Video(v) = &mut s.kind {
                    (v.width, v.height) = g.size();
                }
            }
        }
        Ok(MediaInfo {
            path: path.to_owned(),
            format: ictx.format().name().to_owned(),
            // AV_TIME_BASE is microseconds.
            duration: positive(ictx.duration()).map(Duration::from_micros),
            bit_rate: positive(ictx.bit_rate()),
            streams,
            tags: ictx
                .metadata()
                .iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        })
    }

    #[cfg(feature = "placebo")]
    fn run(&self, pipeline: &Pipeline) -> Result<RunStats> {
        let mut guard = self
            .gpu
            .lock()
            .map_err(|_| crate::Error::Pipeline("GPU state poisoned by an earlier panic".into()))?;
        let t0 = std::time::Instant::now();
        let gpu = match guard.take() {
            Some(g) if g.device == pipeline.device => g,
            _ => super::gpu::Gpu::new(pipeline.device, self.cache_dir.clone())?,
        };
        let init = t0.elapsed();
        let gpu = guard.insert(gpu);
        let mut stats = gpu.run(pipeline)?;
        stats.setup += init;
        stats.first_frame += init;
        stats.wall += init;
        Ok(stats)
    }

    #[cfg(not(feature = "placebo"))]
    fn run(&self, _pipeline: &Pipeline) -> Result<RunStats> {
        Err(crate::Error::Unimplemented(
            "the GPU grade pipeline (placebo feature)",
        ))
    }
}

fn positive(v: i64) -> Option<u64> {
    u64::try_from(v).ok().filter(|&v| v > 0)
}

fn stream_info(s: &ff::format::stream::Stream<'_>) -> Result<StreamInfo> {
    let params = s.parameters();
    let medium = params.medium();
    let codec_id = params.id();
    let bit_rate = positive(params.bit_rate());
    let ctx = CodecContext::from_parameters(params)?;
    let kind = match medium {
        Media::Video => {
            let v = ctx.decoder().video()?;
            let rate = s.avg_frame_rate();
            StreamKind::Video(VideoInfo {
                width: v.width(),
                height: v.height(),
                pixel_format: v
                    .format()
                    .descriptor()
                    .map_or_else(|| "unknown".to_owned(), |d| d.name().to_owned()),
                frame_rate: (rate.denominator() != 0 && rate.numerator() != 0).then(|| Rational {
                    num: rate.numerator(),
                    den: rate.denominator(),
                }),
                frames: positive(s.frames()),
                color: ColorSpace {
                    primaries: primaries(v.color_primaries()),
                    transfer: transfer(v.color_transfer_characteristic()),
                    matrix: matrix(v.color_space()),
                    range: range(v.color_range()),
                },
            })
        }
        // Codecs ffmpeg can't decode (iPhone's APAC spatial audio) are still listed.
        Media::Audio => match ctx.decoder().audio() {
            Ok(a) => StreamKind::Audio(AudioInfo {
                sample_rate: a.rate(),
                channels: a.channels(),
                sample_format: a.format().name().to_owned(),
            }),
            Err(_) => StreamKind::Other,
        },
        Media::Subtitle => StreamKind::Subtitle,
        Media::Data => StreamKind::Data,
        _ => StreamKind::Other,
    };
    Ok(StreamInfo {
        index: s.index(),
        codec: codec_id.name().to_owned(),
        bit_rate,
        kind,
    })
}

fn primaries(p: color::Primaries) -> Primaries {
    match p {
        color::Primaries::BT709 => Primaries::Bt709,
        color::Primaries::BT2020 => Primaries::Bt2020,
        color::Primaries::SMPTE432 => Primaries::DisplayP3,
        color::Primaries::SMPTE431 => Primaries::DciP3,
        color::Primaries::SMPTE170M => Primaries::Bt601,
        _ => Primaries::Unknown,
    }
}

fn transfer(t: color::TransferCharacteristic) -> Transfer {
    use color::TransferCharacteristic as T;
    match t {
        T::BT709 => Transfer::Bt709,
        T::IEC61966_2_1 => Transfer::Srgb,
        T::SMPTE2084 => Transfer::Pq,
        T::ARIB_STD_B67 => Transfer::Hlg,
        T::Linear => Transfer::Linear,
        _ => Transfer::Unknown,
    }
}

fn matrix(m: color::Space) -> Matrix {
    match m {
        color::Space::BT709 => Matrix::Bt709,
        color::Space::BT2020NCL => Matrix::Bt2020Ncl,
        color::Space::SMPTE170M | color::Space::BT470BG => Matrix::Bt601,
        color::Space::RGB => Matrix::Rgb,
        _ => Matrix::Unknown,
    }
}

fn range(r: color::Range) -> Range {
    match r {
        color::Range::MPEG => Range::Limited,
        color::Range::JPEG => Range::Full,
        color::Range::Unspecified => Range::Unknown,
    }
}
