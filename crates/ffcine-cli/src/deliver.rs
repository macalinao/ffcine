//! Delivery commands: `chapters`, `tag`, `credits`, `still`, `subs`, and `grade --deliver` /
//! `--text`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use bpaf::Bpaf;
use ffcine::delivery;
use ffcine::text::TextLayer;
use ffcine::timeline::Timeline;
use ffcine::{
    Backend, Device, EncoderSettings, Grade, MediaInfo, Pipeline, RateControl, Size, backend,
};

use super::{GradeArgs, report};

fn write_out(text: &str, output: Option<&Path>) -> anyhow::Result<()> {
    if let Some(p) = output {
        std::fs::write(p, text).with_context(|| format!("writing {}", p.display()))
    } else {
        print!("{text}");
        Ok(())
    }
}

fn load(path: &Path) -> anyhow::Result<Timeline> {
    Timeline::from_file(path).with_context(|| format!("reading {}", path.display()))
}

fn dir_of(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_owned)
}

pub fn chapters(timeline: &Path, intro: &str, output: Option<&Path>) -> anyhow::Result<()> {
    let c = delivery::chapters(&load(timeline)?, intro);
    for w in &c.warnings {
        eprintln!("warning: {w}");
    }
    write_out(&c.youtube(), output)
}

#[derive(Debug, Clone, Bpaf)]
pub struct TagArgs {
    /// Chapters from this timeline's markers (those flagged `chapter`, else all).
    #[bpaf(argument("TIMELINE"))]
    timeline: Option<PathBuf>,
    /// Title of the 0:00 chapter added when the first marker is later.
    #[bpaf(argument("TITLE"), fallback("Intro".to_owned()), display_fallback)]
    intro: String,
    /// Cover art: a JPEG or PNG file, or a time in the video (S, M:SS or H:MM:SS).
    #[bpaf(argument("FILE|TIME"))]
    cover: Option<String>,
    /// Remove the chapters and cover first.
    clear: bool,
    /// The finished MP4, rewritten in place.
    #[bpaf(positional("MP4"))]
    mp4: PathBuf,
}

/// The cover from `spec`: an image file's bytes, or the frame of `video` at a time as a JPEG.
pub fn cover(spec: &str, video: &Path) -> anyhow::Result<Vec<u8>> {
    let path = Path::new(spec);
    if path.is_file() {
        return std::fs::read(path).with_context(|| format!("reading {}", path.display()));
    }
    let at = delivery::parse_time(spec)
        .with_context(|| format!("--cover {spec:?}: not a file or a time"))?;
    let ffmpeg = std::env::var_os("FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let out = std::process::Command::new(&ffmpeg)
        .args(["-v", "error", "-ss", &format!("{at:.3}"), "-i"])
        .arg(video)
        .args([
            "-frames:v",
            "1",
            "-q:v",
            "2",
            "-c:v",
            "mjpeg",
            "-f",
            "image2",
            "pipe:1",
        ])
        .output()
        .with_context(|| format!("running {}", ffmpeg.to_string_lossy()))?;
    anyhow::ensure!(
        out.status.success() && !out.stdout.is_empty(),
        "cover frame at {spec} of {}: {}",
        video.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

/// Chapters from `timeline`'s markers, warnings printed.
pub fn timeline_chapters(timeline: &Path, intro: &str) -> anyhow::Result<Vec<delivery::Chapter>> {
    let c = delivery::chapters(&load(timeline)?, intro);
    for w in &c.warnings {
        eprintln!("warning: {w}");
    }
    Ok(c.chapters)
}

/// Writes `tags` into `mp4` unless it already has them; says what it wrote.
pub fn write_tags(mp4: &Path, tags: &delivery::mp4::Tags) -> anyhow::Result<()> {
    if delivery::mp4::read(mp4).ok().as_ref() == Some(tags) {
        return Ok(());
    }
    delivery::mp4::write_in_place(mp4, tags)
        .with_context(|| format!("tagging {}", mp4.display()))?;
    eprintln!(
        "{}: {} chapters{}",
        mp4.display(),
        tags.chapters.len(),
        tags.cover.as_ref().map_or_else(String::new, |c| format!(
            ", cover ({} KB)",
            c.len().div_ceil(1000)
        ))
    );
    Ok(())
}

pub fn tag(a: &TagArgs) -> anyhow::Result<()> {
    let mut tags = if a.clear {
        delivery::mp4::Tags::default()
    } else {
        delivery::mp4::read(&a.mp4).with_context(|| format!("reading {}", a.mp4.display()))?
    };
    if let Some(t) = &a.timeline {
        tags.chapters = timeline_chapters(t, &a.intro)?;
    }
    if let Some(c) = &a.cover {
        tags.cover = Some(cover(c, &a.mp4)?);
    }
    write_tags(&a.mp4, &tags)
}

pub fn credits(timeline: &Path, public: bool, output: Option<&Path>) -> anyhow::Result<()> {
    let (c, warnings) = delivery::credits(&load(timeline)?, public);
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    write_out(&delivery::credits_text(&c), output)
}

pub fn subs(subs: &Path, into: &Path, output: Option<&Path>, offset: f64) -> anyhow::Result<()> {
    let src = std::fs::read(subs).with_context(|| format!("reading {}", subs.display()))?;
    let mut t = load(into)?;
    let r = ffcine::text::ass::import(&String::from_utf8_lossy(&src), &mut t, offset)?;
    let out = output.unwrap_or(into);
    t.to_file(out)
        .with_context(|| format!("writing {}", out.display()))?;
    eprintln!(
        "{}: {} lines, {} styles on {} track(s){}",
        out.display(),
        r.events,
        r.styles,
        r.tracks,
        if r.dropped_tags > 0 {
            format!("; {} unsupported override tags dropped", r.dropped_tags)
        } else {
            String::new()
        }
    );
    Ok(())
}

/// Source size, frame rate and duration of a file's video stream.
fn video(info: &MediaInfo) -> anyhow::Result<(Size, f64, Option<Duration>)> {
    let (_, v) = info
        .video()
        .with_context(|| format!("{}: no video stream", info.path.display()))?;
    let fps = v
        .frame_rate
        .map_or(24000.0 / 1001.0, ffcine::Rational::as_f64);
    Ok((
        Size {
            width: v.width,
            height: v.height,
        },
        fps,
        info.duration,
    ))
}

/// `grade`, with `--deliver` (profile encoder and size; review re-encodes once if over budget)
/// and `--text` overlays.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn grade(mut args: GradeArgs, ff: &dyn Backend) -> anyhow::Result<()> {
    let (profile, budget_mb, text, text_at) = (
        args.deliver.take(),
        args.budget_mb.take(),
        args.text.take(),
        args.text_at,
    );
    let user_size = args.size;
    let ss = args.ss;
    let mut p = args.build(ff)?;
    let budget = budget_mb.map(|mb| (mb * 1e6) as u64);
    let mut out_fps = p.frame_rate.map_or(24.0, ffcine::Rational::as_f64);
    if profile.is_some() || text.is_some() {
        let (src, src_fps, dur) = video(&ff.probe(&p.input)?)?;
        let fps = p.frame_rate.map_or(src_fps, ffcine::Rational::as_f64);
        out_fps = fps;
        let duration = p.frames.map_or_else(
            || {
                dur.map_or(Duration::ZERO, |d| {
                    d.saturating_sub(Duration::from_secs_f64(ss))
                })
            },
            |n| Duration::from_secs_f64(n as f64 / fps),
        );
        let mut grade = match p.stages.first() {
            Some(ffcine::Stage::Grade(g)) => g.clone(),
            _ => Grade::default(),
        };
        // What the grade outputs before the profile: --size / --render-profile, else the crop.
        let base = grade.size.unwrap_or_else(|| {
            grade.crop.map_or(src, |c| Size {
                width: c.width as u32,
                height: c.height as u32,
            })
        });
        if let Some(prof) = profile {
            let plan = prof.plan(base, fps, duration, budget);
            p.encoder = plan.encoder;
            grade.size = user_size.or(plan.size);
            eprintln!(
                "{prof}: {} {}x{}, {}",
                p.encoder.codec,
                grade.size.unwrap_or(base).width,
                grade.size.unwrap_or(base).height,
                p.encoder.rate
            );
        }
        if let Some(t) = &text {
            let out = grade.size.unwrap_or(base);
            let frames = (duration.as_secs_f64() * fps).ceil() as u64;
            let mut layer = TextLayer::from_timeline(&load(t)?, dir_of(t))?;
            p.overlays = layer.overlays(text_at, frames, fps, out)?;
        }
        p.stages = vec![ffcine::Stage::Grade(grade)];
    }
    let s = p.run(ff)?;
    report(&p.output, &s);
    let faststart = |out: &Path| -> anyhow::Result<()> {
        let mp4 = out
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "mov" | "m4v"));
        if profile.is_some() && mp4 {
            delivery::mp4::faststart(out)
                .with_context(|| format!("faststart {}", out.display()))?;
        }
        Ok(())
    };
    if profile == Some(ffcine::DeliveryProfile::Review) {
        let cap = budget.unwrap_or(ffcine::profile::REVIEW_BUDGET);
        // The muxed audio comes later; check the video against its share.
        let secs = s.frames as f64 / out_fps;
        let audio = (ffcine::profile::AUDIO_BITRATE as f64 * secs / 8.0) as u64;
        let size = std::fs::metadata(&p.output)?.len();
        if size + audio > cap {
            if let RateControl::Bitrate(b) = p.encoder.rate {
                let k = cap.saturating_sub(audio) as f64 / size as f64 * 0.97;
                p.encoder.rate = RateControl::Bitrate((b as f64 * k) as u64);
                eprintln!(
                    "{:.2} MB (with audio) is over the {:.2} MB budget: second pass at {}",
                    (size + audio) as f64 / 1e6,
                    cap as f64 / 1e6,
                    p.encoder.rate
                );
                let s = p.run(ff)?;
                report(&p.output, &s);
            }
        }
    }
    faststart(&p.output)
}

#[derive(Debug, Clone, Bpaf)]
pub struct StillArgs {
    /// Timeline or comp time: seconds, M:SS or H:MM:SS (required; past a comp's end is an
    /// error).
    #[bpaf(argument("TIME"))]
    at: String,
    /// Output size in px, e.g. 1280x720 (default: the timeline's `frame_size`, else the source's).
    #[bpaf(argument("WxH"))]
    size: Option<Size>,
    /// Leave the text out.
    no_text: bool,
    /// Output PNG (default: still-<TIME>.png, `:` written as `.`).
    #[bpaf(short('o'), argument("FILE"))]
    output: Option<PathBuf>,
    /// CUDA device index (default: picked automatically).
    #[bpaf(argument("N"))]
    cuda: Option<u32>,
    /// Don't use an on-disk shader cache.
    pub no_cache: bool,
    /// Timeline (.otio), or a comp (.comp.json, .comp.yaml).
    #[bpaf(positional("TIMELINE"))]
    timeline: PathBuf,
}

/// `still` of a comp: every layer at `--at`, at the canvas size (or `--size`).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::needless_pass_by_value,
    clippy::many_single_char_names
)]
fn comp_still(a: StillArgs, ff: &backend::Ffmpeg) -> anyhow::Result<()> {
    let c = super::comp::load(&a.timeline)?;
    let at = delivery::parse_time(&a.at)?;
    // The frame showing at `at` (frames start at i / fps).
    let n = (at * c.canvas.fps + 1e-6).floor() as i64;
    if n >= c.frames() {
        anyhow::bail!(
            "--at {}: past the comp's end ({} s, {} frames)",
            a.at,
            c.canvas.duration,
            c.frames()
        );
    }
    let [w, h] = c.canvas.size;
    let out = a.size.unwrap_or(Size {
        width: w,
        height: h,
    });
    let dims = super::comp::dims(ff);
    let camera = super::comp::camera(ff);
    let rate = super::comp::rate(ff);
    let media = ffcine::assembly::CompMedia {
        fp: &ffcine::assembly::FsFingerprinter,
        dims: &dims,
        profile: Some(&camera),
        rate: Some(&rate),
    };
    let piece = c.lower(n..n + 1, out, &media)?;
    let output = a
        .output
        .unwrap_or_else(|| PathBuf::from(format!("still-{}.png", a.at.replace(':', "."))));
    let cache = std::env::temp_dir().join(format!("ffcine-still-{}", std::process::id()));
    let r = super::comp::renderer(ff, cache.clone(), a.cuda, ffcine::Decode::default(), false);
    let res = r.render_piece(
        &piece,
        c.canvas.fps,
        &output,
        EncoderSettings::png_still(),
        Some(ffcine::ColorSpace::BT709_RGB),
    );
    let _ = std::fs::remove_dir_all(&cache);
    res?;
    eprintln!(
        "{}: {}x{} at {:.3} s (frame {n}), {} layers",
        output.display(),
        out.width,
        out.height,
        n as f64 / c.canvas.fps,
        piece.shown.len() + usize::from(matches!(piece.base, ffcine::comp::Base::Video { .. }))
    );
    Ok(())
}

/// The clip's resolved grade (LUT and look) for the GPU.
fn clip_grade(t: &Timeline, dir: &Path, clip: &ffcine::timeline::Clip) -> anyhow::Result<Grade> {
    let project = t.project().transpose()?.unwrap_or_default();
    let (cg, kind) = super::looks::clip_grade(clip);
    let g = project.resolve_grade(cg.as_ref(), kind)?;
    let (grade, skipped) =
        super::looks::grade_for(&project, dir, &g, &mut super::looks::Luts::default())?;
    for s in skipped {
        eprintln!("warning: {s}");
    }
    Ok(grade)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn still(a: StillArgs, ff: &backend::Ffmpeg) -> anyhow::Result<()> {
    if super::comp::is_comp_file(&a.timeline) {
        return comp_still(a, ff);
    }
    let t = load(&a.timeline)?;
    let dir = dir_of(&a.timeline);
    let at = delivery::parse_time(&a.at)?;
    let f = delivery::frame_at(&t, at).with_context(|| format!("no video clip at {} s", a.at))?;
    let (src, _, _) = video(&ff.probe(&f.path)?)?;
    let frame_size = t
        .metadata
        .get("ffcine")
        .and_then(|m| m.get("frame_size"))
        .and_then(|s| Some((s.get(0)?.as_u64()?, s.get(1)?.as_u64()?)))
        .map(|(w, h)| Size {
            width: w as u32,
            height: h as u32,
        });
    let out = a.size.or(frame_size).unwrap_or(src);
    let grade = Grade {
        size: (out != src).then_some(out),
        output: ffcine::ColorSpace::BT709_RGB,
        ..clip_grade(&t, &dir, f.clip)?
    };
    let rate = t.rate().unwrap_or(24.0);
    let overlays = if a.no_text {
        Vec::new()
    } else {
        TextLayer::from_timeline(&t, &dir)?.overlays(at, 1, rate, out)?
    };
    let output = a
        .output
        .unwrap_or_else(|| PathBuf::from(format!("still-{}.png", a.at.replace(':', "."))));
    let mut p = Pipeline::new(&f.path, &output)
        .range(Duration::from_secs_f64(f.source_time.max(0.0)), Some(1))
        .grade(grade)
        .encoder(EncoderSettings::png_still())
        .overlays(overlays);
    if let Some(n) = a.cuda {
        p = p.device(Device::Cuda(n));
    }
    p.run(ff)?;
    eprintln!(
        "{}: {}x{} from {} at {:.3} s ({} text layers)",
        output.display(),
        out.width,
        out.height,
        f.path.display(),
        f.source_time,
        p.overlays.len()
    );
    Ok(())
}
