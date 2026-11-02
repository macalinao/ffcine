//! Reads what a video file records about camera rotation: DJI's `dbgi` track, else (feature
//! `telemetry`) whatever telemetry-parser understands.

use std::path::Path;

use ff::ffi;
use ff::media::Type as Media;
use ffmpeg_next as ff;

use super::SourceRange;
use super::gyro::{Attitude, GIMBAL_REFUSAL, Telemetry, dbgi};

/// The video's size and the camera it names (the `encoder` tag on DJI files).
struct Video {
    size: (u32, u32),
    camera: Option<String>,
}

fn dbgi_stream(ictx: &ff::format::context::Input) -> Option<usize> {
    ictx.streams().find_map(|s| {
        let data = s.parameters().medium() == Media::Data;
        let tagged = s
            .metadata()
            .get("handler_name")
            .is_some_and(|h| h.contains("dbgi"));
        (data && tagged).then_some(s.index())
    })
}

/// The attitude samples of the range from DJI's `dbgi` track, demuxing nothing else, with the
/// margin of a second either side. `None` if the file has no such track or its samples hold no
/// attitude. `auto` won't use it, so unless `strict` the first attitude settles it.
fn read_dbgi(
    ictx: &mut ff::format::context::Input,
    range: &SourceRange,
    camera: Option<String>,
    strict: bool,
) -> Option<Telemetry> {
    let index = dbgi_stream(ictx)?;
    let n = ictx.nb_streams() as usize;
    for i in 0..n {
        // SAFETY: `i` is in range; the discard flag only tells the demuxer what to skip.
        unsafe {
            (**(*ictx.as_mut_ptr()).streams.add(i)).discard = if i == index {
                ffi::AVDiscard::AVDISCARD_DEFAULT
            } else {
                ffi::AVDiscard::AVDISCARD_ALL
            };
        }
    }
    let (from, to) = (
        range.start - 1.0,
        range.start + range.frames as f64 / range.rate.as_f64() + 1.0,
    );
    if from > 0.0 {
        let target = (from * f64::from(ffi::AV_TIME_BASE)) as i64;
        // SAFETY: a plain seek on an open context. On failure reading starts at the file's start.
        unsafe { ffi::avformat_seek_file(ictx.as_mut_ptr(), -1, i64::MIN, target, target, 0) };
    }
    let mut attitude = Vec::new();
    for (stream, packet) in ictx.packets() {
        if stream.index() != index {
            continue;
        }
        let tb = f64::from(stream.time_base());
        let t = packet.pts()? as f64 * tb;
        if t > to {
            break;
        }
        if t < from {
            continue;
        }
        attitude.push(Attitude {
            t,
            q: dbgi::sample_attitude(packet.data()?)?,
        });
        if !strict {
            break;
        }
    }
    (!attitude.is_empty()).then_some(Telemetry {
        camera,
        attitude,
        focal_px: None,
        auto_refusal: Some(GIMBAL_REFUSAL),
    })
}

fn video(ictx: &ff::format::context::Input) -> Option<Video> {
    let par = ictx.streams().best(Media::Video)?.parameters();
    // SAFETY: the parameters of an open stream.
    let size = unsafe { ((*par.as_ptr()).width, (*par.as_ptr()).height) };
    Some(Video {
        size: (u32::try_from(size.0).ok()?, u32::try_from(size.1).ok()?),
        camera: ictx.metadata().get("encoder").map(str::to_owned),
    })
}

/// The file's telemetry for `range` and its video's size, or why there is none. Unless `strict`
/// (`--source gyro`) it may stop as soon as [`Telemetry::auto_refusal`] is known.
pub fn read(
    path: &Path,
    range: &SourceRange,
    strict: bool,
) -> Result<(Telemetry, (u32, u32)), String> {
    ff::init().map_err(|e| e.to_string())?;
    let mut ictx = ff::format::input(path).map_err(|e| e.to_string())?;
    let Video { size, camera } = video(&ictx).ok_or("no video stream")?;
    if let Some(t) = read_dbgi(&mut ictx, range, camera, strict) {
        return Ok((t, size));
    }
    #[cfg(feature = "telemetry")]
    let parsed = super::parser::read(path);
    #[cfg(not(feature = "telemetry"))]
    let parsed: Result<Telemetry, String> =
        Err("no attitude track (built without the telemetry feature)".into());
    parsed.map(|t| (t, size))
}
