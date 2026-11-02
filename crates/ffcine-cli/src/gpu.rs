//! The GPU commands (feature `gpu`): probe, grade, batch, stabilize, jitter, import, and the
//! delivery commands.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context as _;
use bpaf::{Bpaf, Parser as _};
use ffcine::camera::{self, ProfileChoice};
use ffcine::reframe::{Profile, Reframe, Shot};
use ffcine::stabilize::{self, DetectParams, FlowQuality, SourceRange, StabilizeParams, Zoom};
use ffcine::{
    Backend as _, Codec, ColorMapTuning, ColorOverride, Crop, Deband, Decode, Denoise, Device,
    Dither, DitherMethod, EncoderSettings, GamutMapping, Grade, Grain, Lut, LutStage, LutType,
    Pipeline, PixelFormat, RateControl, Rational, Scaler, Scaling, Size, Spatial, Temporal,
    ToneMapping, backend,
};

#[path = "assemble.rs"]
mod assemble;
#[path = "deliver.rs"]
mod deliver;
#[path = "looks.rs"]
mod looks;

/// `--dither`: a method or `none`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DitherArg(Option<DitherMethod>);

impl std::fmt::Display for DitherArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(m) => write!(f, "{m}"),
            None => f.write_str("none"),
        }
    }
}

impl FromStr for DitherArg {
    type Err = ffcine::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "none" {
            Ok(Self(None))
        } else {
            s.parse().map(|m| Self(Some(m)))
        }
    }
}

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::large_enum_variant, clippy::doc_markdown)] // parsed once; doc comments are --help
pub(crate) enum Cmd {
    /// Show a file's container, streams, codecs and colour tags.
    ///
    /// Prints `PATH: FORMAT, DURATIONs, BITRATE kb/s`, then one line per stream: `#N video CODEC
    /// WxH PIXFMT COLOUR, FPS fps, N frames` or `#N audio CODEC RATE Hz Nch FORMAT`.
    #[bpaf(command, footer(crate::help::PROBE))]
    Probe {
        /// Media file to inspect (video, audio or image).
        #[bpaf(positional("FILE"))]
        file: PathBuf,
    },
    /// Grade one clip on the GPU: tone mapping, LUT, crop/scale, sharpen, encode.
    ///
    /// Decodes with Vulkan or NVDEC, grades in libplacebo and encodes on NVENC, frames staying in
    /// VRAM. Defaults match the prototype: camera LUT from the probed profile, bt.2390 tone
    /// mapping, perceptual gamut mapping, lanczos scaling in gamma light, 10-bit HEVC NVENC
    /// master (p6, hq, qp=12, main10, p010le, GOP 24); --preview is 8-bit libx264 (crf 10).
    /// Prints frames, wall time and fps.
    #[bpaf(command, footer(crate::help::GRADE))]
    Grade(#[bpaf(external(grade_args))] GradeArgs),
    /// Grade many shots through one warm GPU pipeline (device, renderer, shader cache).
    ///
    /// FILE has one shot per line, written as `grade` arguments; see the format below.
    #[bpaf(command, footer(crate::help::BATCH))]
    Batch {
        /// libplacebo shader cache directory (default: ~/.cache/ffcine/placebo).
        #[bpaf(argument("DIR"))]
        cache_dir: Option<PathBuf>,
        /// Don't use an on-disk shader cache.
        no_cache: bool,
        /// Batch file: one shot per line as `grade` arguments.
        #[bpaf(positional("FILE"))]
        file: PathBuf,
    },
    /// Stabilize a shot: GPU optical-flow motion detect, smoothing and auto zoom.
    ///
    /// Detects frame-to-frame motion with NVIDIA optical flow (cached per input and range),
    /// smooths it vidstab-style and picks the zoom that hides the borders. Prints the detect time
    /// and the zoom; the warps are only written with --hook, --transforms or --motions. To
    /// stabilize while grading, use `grade --stabilize`.
    #[bpaf(command, long("stabilise"), footer(crate::help::STABILIZE))]
    Stabilize(#[bpaf(external(stabilize_args))] StabilizeArgs),
    /// Measure a clip's residual jitter: the motion left after smoothing.
    ///
    /// RMS of the frame-to-frame motion minus a smoothed version of it, e.g. to compare
    /// stabilizers. Prints `jitter tx PX ty PX angle DEG (N pairs, mean confidence C)`.
    #[bpaf(command, footer(crate::help::JITTER))]
    Jitter(#[bpaf(external(jitter_args))] JitterArgs),
    /// Download the camera vendors' LUTs ffcine knows into the LUT directory.
    ///
    /// Currently DJI Osmo Pocket 4 D-Log -> Rec.709 (normal and vivid). Each download is checked to be a .cube LUT;
    /// files already there are kept. `grade --camera` and the catalog find them there.
    #[bpaf(command, footer(crate::help::FETCH_LUTS))]
    FetchLuts {
        /// LUT directory (default: the first of $FFCINE_LUT_DIR, else
        /// $XDG_DATA_HOME/ffcine/luts, i.e. ~/.local/share/ffcine/luts).
        #[bpaf(argument("DIR"))]
        dir: Option<PathBuf>,
        /// Download again even if the file is there.
        force: bool,
    },
    /// Convert another editor's project to an ffcine timeline (.otio).
    ///
    /// Reads edit.yaml, OTIO (including Resolve's), FCPXML, Kdenlive/Shotcut MLT, FCP7/Premiere
    /// XML, CMX3600 EDL and CapCut drafts. Prints what it mapped and what it skipped.
    #[bpaf(command, footer(crate::help::IMPORT))]
    Import(#[bpaf(external(import_args))] ImportArgs),
    /// Index a card before editing: strips, scene cuts, speech, transcripts, flags.
    ///
    /// Per clip: a contact strip, scene cuts, a speech map, GPU whisper transcripts with word
    /// times, runaway/dead/dark flags and optional locations. Writes OUT/index.json (what search
    /// and snap read), OUT/index.html (the clip page), OUT/strips/<clip>.jpg and a per-clip cache
    /// in OUT/clips/; re-running only analyses what changed.
    #[bpaf(command, footer(crate::help::TRIAGE))]
    Triage(#[bpaf(external(crate::triage::triage_args))] crate::triage::TriageArgs),
    /// YouTube chapter lines from the timeline's markers.
    ///
    /// Uses the markers flagged `chapter`, else all markers; prints `M:SS Title` lines (H:MM:SS
    /// past an hour), with a 0:00 chapter added when the first marker is later.
    #[bpaf(command, footer(crate::help::CHAPTERS))]
    #[allow(clippy::doc_markdown)]
    Chapters {
        /// Title of the 0:00 chapter added when the first marker is later.
        #[bpaf(argument("TITLE"), fallback("Intro".to_owned()), display_fallback)]
        intro: String,
        /// Write here instead of stdout.
        #[bpaf(short('o'), argument("FILE"))]
        output: Option<PathBuf>,
        /// Timeline (.otio).
        #[bpaf(positional("TIMELINE"))]
        timeline: PathBuf,
    },
    /// Music credits (CC BY attribution lines) from the music clips' metadata.
    ///
    /// One line per track from the clips' `music` and `attribution` metadata, for a video
    /// description.
    #[bpaf(command, footer(crate::help::CREDITS))]
    Credits {
        /// The public cut: leave out `private` sounds (or credit their `public_alt`).
        public: bool,
        /// Write here instead of stdout.
        #[bpaf(short('o'), argument("FILE"))]
        output: Option<PathBuf>,
        /// Timeline (.otio).
        #[bpaf(positional("TIMELINE"))]
        timeline: PathBuf,
    },
    /// Write a full-resolution PNG of the timeline at a time, text included.
    ///
    /// The shot under TIME with its grade and the timeline's text clips, e.g. for thumbnails.
    #[bpaf(command, footer(crate::help::STILL))]
    Still(#[bpaf(external(deliver::still_args))] deliver::StillArgs),
    /// Compare grade variants (looks, LUTs, overrides) on shots, with contact sheets.
    ///
    /// Writes OUT/<shot>_<variant>.jpg at full resolution and OUT/<shot>_sheet.jpg (variants side
    /// by side, labelled, under 5 MB), all through one warm GPU pipeline. Shots come from --at,
    /// --sample and --clip.
    #[bpaf(command, footer(crate::help::LOOKS))]
    Looks(#[bpaf(external(looks::looks_args))] looks::LooksArgs),
    /// Add .ass/.ssa subtitles to a timeline as ffcine.text clips with their styles.
    ///
    /// Prints how many lines, styles and tracks were added, and override tags it dropped.
    #[bpaf(command, footer(crate::help::SUBS))]
    Subs {
        /// Shift the subtitles by this many seconds (may be negative).
        #[bpaf(argument("SECONDS"), fallback(0.0), display_fallback)]
        offset: f64,
        /// Timeline to add them to (.otio).
        #[bpaf(argument("TIMELINE"))]
        into: PathBuf,
        /// Output timeline (default: overwrite --into).
        #[bpaf(short('o'), argument("FILE"))]
        output: Option<PathBuf>,
        /// Subtitle file (.ass or .ssa).
        #[bpaf(positional("SUBS"))]
        subs: PathBuf,
    },
    /// Render a whole timeline into one master, with a segment cache.
    ///
    /// Applies each clip's grade, stabilize, denoise, reframe and speed ramp from the timeline,
    /// in its project.output aspect (or --render-profile), and draws its text clips. Segments are
    /// cached by content: after an edit only changed ones render, the master joins them by stream
    /// copy, and an audio-only change is a remux. Re-run after an interruption to resume. The
    /// audio is the finished mix, as `ffcine audio`; the mix flags override project.mix.
    #[bpaf(command, footer(crate::help::RENDER))]
    Render(#[bpaf(external(assemble::render_args))] assemble::RenderArgs),
    /// List which segments of NEW would render that OLD's render already has.
    ///
    /// Compares the segment keys of two versions of a timeline (and whether the audio changed),
    /// i.e. what `render` would redo after the edit. Renders nothing.
    #[bpaf(command, footer(crate::help::DIFF))]
    Diff {
        /// Render profile: preview (1080 px short side) or master (2160 px).
        #[bpaf(argument("NAME"), fallback("preview".to_owned()), display_fallback)]
        profile: String,
        /// Output aspect (overrides the timeline's project.output.aspect): landscape (16:9), reel
        /// (9:16), feed (4:5), square (1:1), WxH or NAME=WxH.
        #[bpaf(argument("ASPECT"))]
        render_profile: Option<String>,
        /// List unchanged segments too.
        all: bool,
        /// The timeline as last rendered (.otio).
        #[bpaf(positional("OLD"))]
        old: PathBuf,
        /// The edited timeline (.otio).
        #[bpaf(positional("NEW"))]
        new: PathBuf,
    },
}

/// Splits a batch line into words: whitespace-separated, `'...'` and `"..."` quote.
fn split_words(line: &str) -> anyhow::Result<Vec<String>> {
    let mut words = Vec::new();
    let mut cur: Option<String> = None;
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.get_or_insert_default();
            }
            (None, '#') if cur.is_none() => break,
            (None, c) if c.is_whitespace() => words.extend(cur.take()),
            (_, c) => cur.get_or_insert_default().push(c),
        }
    }
    anyhow::ensure!(quote.is_none(), "unterminated quote");
    words.extend(cur);
    Ok(words)
}

fn report(output: &std::path::Path, s: &ffcine::RunStats) {
    let wall = s.wall.as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let fps = s.frames as f64 / wall;
    eprintln!(
        "{}: {} frames in {wall:.2} s ({fps:.1} fps, steady {:.1}, setup {:.2} s)",
        output.display(),
        s.frames,
        s.steady_fps().unwrap_or(fps),
        s.setup.as_secs_f64()
    );
}

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::doc_markdown)] // --help text
pub(crate) struct ImportArgs {
    /// Source format: edit-yaml, otio, resolve, fcpxml, mlt (Kdenlive, Shotcut), fcp7 (Premiere
    /// XML), edl or capcut (default: from the file; Resolve's .otio is detected).
    #[bpaf(argument("FORMAT"))]
    format: Option<ffcine::import::Format>,
    /// Timeline frame rate, e.g. 24000/1001 (default: the source's).
    #[bpaf(argument::<String>("FPS"), parse(|s: String| parse_rate(&s)), optional)]
    rate: Option<f64>,
    /// Timeline frame size in px, for sources without one (Resolve OTIO), e.g. 3840x2160.
    #[bpaf(long("size"), long("frame-size"), argument("WxH"))]
    size: Option<Size>,
    /// Fail on anything the importer can't read instead of skipping it.
    strict: bool,
    /// Output timeline (.otio).
    #[bpaf(short('o'), long("output"), argument("FILE"))]
    output: PathBuf,
    /// Project file to convert (.yaml, .otio, .fcpxml/.fcpxmld, .kdenlive/.mlt, .xml, .edl,
    /// a CapCut draft or its folder).
    #[bpaf(positional("FILE"))]
    input: PathBuf,
}

fn parse_rate(s: &str) -> anyhow::Result<f64> {
    let r = match s.split_once('/') {
        Some((n, d)) => n.trim().parse::<f64>()? / d.trim().parse::<f64>()?,
        None => s.trim().parse()?,
    };
    anyhow::ensure!(r.is_finite() && r > 0.0, "bad frame rate {s:?}");
    Ok(r)
}

/// Media length via libav, for importers that leave lengths to the media.
fn probe_duration(path: &Path) -> Option<f64> {
    let info = backend::Ffmpeg::new().ok()?.probe(path).ok()?;
    info.duration.map(|d| d.as_secs_f64())
}

fn import(args: &ImportArgs) -> anyhow::Result<()> {
    let opts = ffcine::import::ImportOptions {
        frame_size: args.size.map(|s| (s.width, s.height)),
        rate: args.rate,
        media_duration: Some(probe_duration),
        beats: Some(crate::audio::beat_times),
        strict: args.strict,
    };
    let imported = ffcine::import::import(&args.input, args.format, &opts)
        .with_context(|| format!("importing {}", args.input.display()))?;
    eprint!("{}", imported.report);
    imported.timeline.to_file(&args.output)?;
    Ok(())
}

/// A frame rate as `NUM/DEN` or an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rate(Rational);

impl std::str::FromStr for Rate {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (n, d) = s.split_once('/').unwrap_or((s, "1"));
        match (n.parse(), d.parse()) {
            (Ok(num), Ok(den)) if num > 0 && den > 0 => Ok(Self(Rational { num, den })),
            _ => Err(format!("invalid rate {s:?}, expected e.g. 24000/1001")),
        }
    }
}

impl std::fmt::Display for Rate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

const NTSC_FILM: Rate = Rate(Rational {
    num: 24000,
    den: 1001,
});

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::doc_markdown)] // --help text
pub(crate) struct StabilizeArgs {
    /// Range start in seconds (like ffmpeg -ss on the input).
    #[bpaf(argument("SECONDS"), fallback(0.0), display_fallback)]
    start: f64,
    /// Frames to stabilize, counted at --rate (required).
    #[bpaf(argument("N"))]
    frames: usize,
    /// Frame rate the range is read at (fps filter), NUM/DEN or N.
    #[bpaf(argument("NUM/DEN"), fallback(NTSC_FILM), display_fallback)]
    rate: Rate,
    /// Gaussian smoothing half-window in frames (vidstab smoothing).
    #[bpaf(argument("FRAMES"), fallback(20), display_fallback)]
    smoothing: u32,
    /// Width in px the frames are scaled to for optical flow.
    #[bpaf(argument("PX"), fallback(1920), display_fallback)]
    analysis_width: u32,
    /// Optical flow quality: slow, medium or fast.
    #[bpaf(argument("Q"), fallback(FlowQuality::Slow), display_fallback)]
    quality: FlowQuality,
    /// Cap on the automatic zoom, e.g. 1.1 (borders may show beyond it; default: no cap).
    #[bpaf(argument("Z"))]
    max_zoom: Option<f64>,
    /// Fixed zoom instead of the automatic one, e.g. 1.05 (1 = none).
    #[bpaf(argument("Z"))]
    zoom: Option<f64>,
    /// Ignore and overwrite the detect cache.
    no_cache: bool,
    /// Write the warps as an mpv/libplacebo .hook shader, for ffmpeg's libplacebo filter.
    #[bpaf(argument("FILE"))]
    hook: Option<PathBuf>,
    /// Hook passes run per frame in the final shader file (libplacebo's `frame` counts passes).
    #[bpaf(argument("N"), fallback(1), display_fallback)]
    hook_passes: u32,
    /// Write the warps as text, one row-major 3x3 matrix per frame.
    #[bpaf(argument("FILE"))]
    transforms: Option<PathBuf>,
    /// Write the detected frame-to-frame motions as text.
    #[bpaf(argument("FILE"))]
    motions: Option<PathBuf>,
    /// Clip to stabilize.
    #[bpaf(positional("INPUT"))]
    input: PathBuf,
}

#[derive(Debug, Clone, Bpaf)]
pub(crate) struct JitterArgs {
    /// Range start in seconds.
    #[bpaf(argument("SECONDS"), fallback(0.0), display_fallback)]
    start: f64,
    /// Frames to measure, counted at --rate (required).
    #[bpaf(argument("N"))]
    frames: usize,
    /// Frame rate the range is read at, NUM/DEN or N.
    #[bpaf(argument("NUM/DEN"), fallback(NTSC_FILM), display_fallback)]
    rate: Rate,
    /// Smoothing sigma in frames: motion slower than this counts as intended.
    #[bpaf(argument("FRAMES"), fallback(5.0), display_fallback)]
    sigma: f64,
    /// Skip the frame pair ending at this frame (a cut or punch-in); repeatable.
    #[bpaf(argument("FRAME"), many)]
    exclude: Vec<usize>,
    /// Width in px the frames are scaled to for optical flow.
    #[bpaf(argument("PX"), fallback(1920), display_fallback)]
    analysis_width: u32,
    /// Clip to measure (e.g. a stabilized render).
    #[bpaf(positional("INPUT"))]
    input: PathBuf,
}

impl StabilizeArgs {
    fn run(self) -> anyhow::Result<()> {
        let range = SourceRange {
            start: self.start,
            frames: self.frames,
            rate: self.rate.0,
        };
        let detect = DetectParams {
            analysis_width: self.analysis_width,
            quality: self.quality,
            ..DetectParams::default()
        };
        let t = std::time::Instant::now();
        let (det, cached) = if self.no_cache {
            (stabilize::detect(&self.input, &range, &detect)?, false)
        } else {
            stabilize::detect_cached(&self.input, &range, &detect)?
        };
        println!(
            "detect {:.2} s{} ({}x{} analysed at {}x{})",
            t.elapsed().as_secs_f64(),
            if cached { " (cached)" } else { "" },
            det.width,
            det.height,
            det.analysis_width,
            det.analysis_height,
        );
        let params = StabilizeParams {
            smoothing: self.smoothing,
            zoom: match self.zoom {
                Some(z) => Zoom::Fixed(z),
                None => Zoom::Auto { max: self.max_zoom },
            },
        };
        let st = stabilize::stabilize(&det, &params);
        println!("zoom {:.4} (needed {:.4})", st.zoom, st.min_zoom);
        let write = |path: &PathBuf, text: String| {
            std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
        };
        if let Some(p) = &self.motions {
            write(p, stabilize::motions_text(&det.motions))?;
        }
        if let Some(p) = &self.transforms {
            write(p, stabilize::transforms_text(&st.transforms))?;
        }
        if let Some(p) = &self.hook {
            let hook =
                stabilize::hook::warp_hook(&st.transforms, det.width, det.height, self.hook_passes);
            write(p, hook)?;
        }
        Ok(())
    }
}

impl JitterArgs {
    fn run(self) -> anyhow::Result<()> {
        let range = SourceRange {
            start: self.start,
            frames: self.frames,
            rate: self.rate.0,
        };
        let detect = DetectParams {
            analysis_width: self.analysis_width,
            ..DetectParams::default()
        };
        let det = stabilize::detect(&self.input, &range, &detect)?;
        let j = stabilize::jitter(&det.motions, self.sigma, &self.exclude);
        println!(
            "jitter tx {:.3} px  ty {:.3} px  angle {:.4} deg  ({} pairs, mean confidence {:.2})",
            j.tx, j.ty, j.angle, j.pairs, j.confidence
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Bpaf)]
// Doc comments are --help text, not rustdoc.
#[allow(clippy::struct_excessive_bools, clippy::doc_markdown)]
pub(crate) struct GradeArgs {
    /// Start time in seconds (accurate seek, like ffmpeg's input -ss).
    #[bpaf(argument("SECONDS"), fallback(0.0), display_fallback)]
    ss: f64,
    /// Frames to output (default: to the end of the source; the last frame repeats if the
    /// source ends first).
    #[bpaf(argument("N"))]
    frames: Option<u64>,
    /// Output frame rate, N/D or N, e.g. 24000/1001, like ffmpeg's fps filter (default: the
    /// source's).
    #[bpaf(argument("RATE"))]
    fps: Option<Rational>,
    /// Override the source's colour tags as MATRIX/PRIMARIES/TRANSFER/RANGE, `-` keeping one,
    /// e.g. -/bt2020/arib-std-b67/- (values as ffmpeg names them: bt709, bt2020nc, smpte2084,
    /// tv, pc, ...).
    #[bpaf(argument("M/P/T/R"))]
    in_color: Option<ColorOverride>,
    /// Camera profile: auto (from the probe), rec709, hdr, dlog, dlog-vivid, dlogm, dlogm-vivid,
    /// clog3 or applelog (default: auto without --lut, none with it). Log profiles add their
    /// camera LUT before the grade; a --lut is taken as a baked grade that already includes it.
    /// Vendor LUTs come from the LUT dir (`ffcine fetch-luts`).
    #[bpaf(argument("PROFILE"))]
    camera: Option<ProfileChoice>,
    /// 3D LUT (.cube) to apply after the camera LUT.
    #[bpaf(argument("FILE"))]
    lut: Option<PathBuf>,
    /// LUT input space: native, normalized or conversion.
    #[bpaf(argument("TYPE"), fallback(LutType::Normalized), display_fallback)]
    lut_type: LutType,
    /// HDR -> SDR tone curve: bt.2390, bt.2446a, st2094-40, st2094-10, spline, hable, mobius,
    /// reinhard, gamma, linear, clip or auto.
    #[bpaf(argument("CURVE"), fallback(ToneMapping::Bt2390), display_fallback)]
    tonemap: ToneMapping,
    /// Gamut mapping: perceptual, softclip, highlight, relative, saturation, absolute,
    /// desaturate, darken, linear or clip.
    #[bpaf(argument("MODE"), fallback(GamutMapping::Perceptual), display_fallback)]
    gamut: GamutMapping,
    /// HDR contrast recovery strength (0 = off).
    #[bpaf(argument("S"), fallback(0.30), display_fallback)]
    contrast_recovery: f32,
    /// Perceptual gamut mapping strength, 0..1.
    #[bpaf(argument("S"), fallback(0.80), display_fallback)]
    perceptual_strength: f32,
    /// Don't measure the HDR peak per frame.
    no_peak_detect: bool,
    /// Deband the source planes (libplacebo deband, ffmpeg libplacebo defaults). On by default
    /// for D-Log M (the LUT stretches the log curve into visible bands).
    deband: bool,
    /// No deband, even for D-Log M.
    no_deband: bool,
    /// Output dither: blue, ordered, white or none.
    #[bpaf(
        argument("METHOD"),
        fallback(DitherArg(Some(DitherMethod::Blue))),
        display_fallback
    )]
    dither: DitherArg,
    /// Vary the dither pattern per frame.
    temporal_dither: bool,
    /// Source crop in input pixels, W:H:X:Y as ffmpeg's crop filter, e.g. 3840:1608:0:276.
    #[bpaf(argument("W:H:X:Y"))]
    crop: Option<Crop>,
    /// Output size in px, e.g. 1920x1080 (default: the crop's or the input's).
    #[bpaf(argument("WxH"))]
    size: Option<Size>,
    /// Upscaling filter: lanczos, ewa_lanczos, ewa_lanczossharp, spline36, mitchell, bicubic,
    /// bilinear or nearest.
    #[bpaf(argument("FILTER"), fallback(Scaler::Lanczos), display_fallback)]
    upscaler: Scaler,
    /// Downscaling filter (as --upscaler).
    #[bpaf(argument("FILTER"), fallback(Scaler::Lanczos), display_fallback)]
    downscaler: Scaler,
    /// Scale in linear light (default: gamma light).
    linear: bool,
    /// Sigmoidized upscaling.
    sigmoid: bool,
    /// Luma unsharp amount after the LUT, as ffmpeg unsharp=5:5:AMOUNT, e.g. 0.6; 0 = off.
    #[bpaf(argument("AMOUNT"), fallback(0.0), display_fallback)]
    sharpen: f32,
    /// Adaptive-sharpen strength after scaling (its curve_height, 0.3..2.0); replaces
    /// --sharpen.
    #[bpaf(argument("STRENGTH"))]
    adaptive_sharpen: Option<f32>,
    /// Render profile: landscape, hd, reel (9:16), feed (4:5), square, WxH or NAME=WxH. Sets the
    /// output size and picks the reframe's per-profile overrides.
    #[bpaf(argument("PROFILE"))]
    render_profile: Option<Profile>,
    /// Reframe: ffcine.geo.reframe/1 parameters as a JSON object (zoom, punch, push, crop_x,
    /// ...) or @FILE holding one; see docs/reframe.md.
    #[bpaf(argument("JSON"))]
    reframe: Option<String>,
    /// Draw the render profile's safe area over the picture (review renders).
    safe_area: bool,
    /// Extra mpv/libplacebo .hook shader, run in the grade pass. Repeatable.
    #[bpaf(argument("FILE"), many)]
    shader: Vec<PathBuf>,
    /// Stabilize in the grade pass: GPU optical-flow detect (cached), vidstab-style smoothing,
    /// auto zoom, warp before the crop.
    #[bpaf(long("stabilize"), long("stabilise"))]
    stabilize: bool,
    /// --stabilize smoothing half-window in frames (vidstab smoothing).
    #[bpaf(argument("FRAMES"), fallback(20), display_fallback)]
    stab_smoothing: u32,
    /// --stabilize fixed zoom instead of the automatic one, e.g. 1.05.
    #[bpaf(argument("Z"))]
    stab_zoom: Option<f64>,
    /// --stabilize cap on the automatic zoom, e.g. 1.1 (default: no cap).
    #[bpaf(argument("Z"))]
    stab_max_zoom: Option<f64>,
    /// Temporal denoise: frames on each side, 1..5, motion-compensated; 0 = off.
    #[bpaf(argument("N"), fallback(0), display_fallback)]
    temporal_radius: u32,
    /// Temporal denoise motion: nvof (NVIDIA optical flow) or blocks (Vulkan block matching)
    /// (default: nvof).
    #[bpaf(argument("SRC"), fallback(ffcine::filters::Motion::Nvof))]
    temporal_motion: ffcine::filters::Motion,
    /// Temporal denoise luma strength: noise sigma in 8-bit code values.
    #[bpaf(argument("S"), fallback(4.0), display_fallback)]
    temporal_strength: f32,
    /// Temporal denoise chroma strength (default: the luma strength).
    #[bpaf(argument("S"))]
    temporal_chroma: Option<f32>,
    /// Spatial (NLM) denoise luma strength, after the temporal one; 0 = off.
    #[bpaf(argument("S"), fallback(0.0), display_fallback)]
    denoise: f32,
    /// Spatial denoise chroma strength (default: 1.5x luma).
    #[bpaf(argument("S"))]
    denoise_chroma: Option<f32>,
    /// Spatial denoise search radius in px, 1..4.
    #[bpaf(argument("R"), fallback(2), display_fallback)]
    denoise_radius: u32,
    /// Film grain sigma at mid-grey (8-bit code values) on the graded output; 0 = off.
    #[bpaf(argument("S"), fallback(0.0), display_fallback)]
    grain: f32,
    /// Grain cell size in output pixels.
    #[bpaf(argument("PX"), fallback(1.5), display_fallback)]
    grain_size: f32,
    /// Chroma grain relative to luma grain, 0..1.
    #[bpaf(argument("K"), fallback(0.0), display_fallback)]
    grain_chroma: f32,
    /// 8-bit libx264 preview (crf 10) instead of the 10-bit HEVC NVENC master.
    preview: bool,
    /// Encoder: hevc_nvenc, h264_nvenc, av1_nvenc, hevc_vaapi, hevc_vulkan, h264_vulkan,
    /// av1_vulkan, libx264, libx265 or png (default: hevc_nvenc; libx264 with --preview).
    #[bpaf(argument("CODEC"))]
    codec: Option<Codec>,
    /// Encoder preset: p1..p7 for NVENC, x264's names for libx264 (default: p6; veryfast with
    /// --preview).
    #[bpaf(argument("PRESET"))]
    preset: Option<String>,
    /// Encoder tune, e.g. hq or ll (default: hq; none with --preview).
    #[bpaf(argument("TUNE"))]
    tune: Option<String>,
    /// Encoder profile, e.g. main10 or high10 (default: main10; none with --preview).
    #[bpaf(argument("PROFILE"))]
    profile: Option<String>,
    /// Rate control: qp=N, crf=N or bitrate=BITS per second (default: qp=12; crf=10 with
    /// --preview).
    #[bpaf(argument("RC"))]
    rate: Option<RateControl>,
    /// Encoder pixel format: p010le, nv12, yuv420p, yuv420p10le, gbrp16le or rgba (default:
    /// p010le; yuv420p with --preview).
    #[bpaf(argument("FMT"))]
    pix_fmt: Option<PixelFormat>,
    /// Keyframe interval in frames (default: 24).
    #[bpaf(argument("N"))]
    gop: Option<u32>,
    /// Delivery profile: master, preview, review (under --budget-mb) or phone (1080p HEVC);
    /// sets the encoder and size (--size still wins).
    #[bpaf(argument("PROFILE"))]
    deliver: Option<ffcine::DeliveryProfile>,
    /// Size cap of the review profile in MB (default: 50).
    #[bpaf(argument("MB"))]
    budget_mb: Option<f64>,
    /// Draw this timeline's ffcine.text clips over the output.
    #[bpaf(argument("TIMELINE"))]
    text: Option<PathBuf>,
    /// Timeline time in seconds of the first output frame (with --text).
    #[bpaf(argument("SECONDS"), fallback(0.0), display_fallback)]
    text_at: f64,
    /// CUDA device index; Vulkan is derived from it (default: picked automatically).
    #[bpaf(argument("N"))]
    cuda: Option<u32>,
    /// Vulkan device index (instead of --cuda).
    #[bpaf(argument("N"))]
    vulkan: Option<u32>,
    /// Hardware decode API: vulkan (frames go straight to the renderer) or cuda (NVDEC, one
    /// copy).
    #[bpaf(argument("API"), fallback(Decode::Vulkan), display_fallback)]
    decode: Decode,
    /// libplacebo shader cache directory (default: ~/.cache/ffcine/placebo).
    #[bpaf(argument("DIR"))]
    cache_dir: Option<PathBuf>,
    /// Don't use an on-disk shader cache.
    no_cache: bool,
    /// Source clip.
    #[bpaf(positional("INPUT"))]
    input: PathBuf,
    /// Output file; the container follows the extension (.mkv, .mov, .mp4, ...).
    #[bpaf(positional("OUTPUT"))]
    output: PathBuf,
}

impl GradeArgs {
    /// `--stabilize` settings, if on.
    fn stabilize(&self) -> Option<StabilizeParams> {
        self.stabilize.then_some(StabilizeParams {
            smoothing: self.stab_smoothing,
            zoom: match self.stab_zoom {
                Some(z) => Zoom::Fixed(z),
                None => Zoom::Auto {
                    max: self.stab_max_zoom,
                },
            },
        })
    }

    /// The pipeline, with its camera LUT and stabilized on `ff` if asked.
    fn build(self, ff: &dyn ffcine::Backend) -> anyhow::Result<Pipeline> {
        let stab = self.stabilize();
        let choice = match self.camera {
            Some(c) => Some(c),
            None if self.lut.is_none() => Some(ProfileChoice::Auto),
            None => None,
        };
        let no_deband = self.no_deband;
        let mut p = self.into_pipeline()?;
        if let Some(choice) = choice {
            let info = ff
                .probe(&p.input)
                .with_context(|| format!("probing {}", p.input.display()))?;
            let profile = choice.resolve(&info);
            eprintln!("profile: {profile} ({choice})");
            if let Some(cam) = profile.camera_lut(&camera::lut_dirs())? {
                if let Some(ffcine::Stage::Grade(g)) = p.stages.first_mut() {
                    if profile.as_str().starts_with("dlog") && !no_deband {
                        g.deband.get_or_insert_with(Deband::default);
                    }
                    g.input_color.primaries.get_or_insert(cam.primaries);
                    g.input_color.transfer.get_or_insert(cam.transfer);
                    g.input_lut = Some(LutStage {
                        lut: cam.lut,
                        kind: LutType::Normalized,
                    });
                }
            }
        }
        if let Some(params) = stab {
            let t = std::time::Instant::now();
            let (st, cached) = ffcine::stabilize_pipeline(ff, &mut p, &params, true)
                .with_context(|| format!("stabilizing {}", p.input.display()))?;
            eprintln!(
                "stabilize: detect {:.2} s{}, {} frames, zoom {:.4} (needed {:.4})",
                t.elapsed().as_secs_f64(),
                if cached { " (cached)" } else { "" },
                st.transforms.len(),
                st.zoom,
                st.min_zoom
            );
        }
        Ok(p)
    }

    /// `--reframe` / `--render-profile` / `--safe-area`: per-frame windows from the probed shot.
    fn apply_reframe(&self, grade: &mut Grade) -> anyhow::Result<()> {
        let info = backend::Ffmpeg::new()?
            .probe(&self.input)
            .with_context(|| format!("probing {}", self.input.display()))?;
        let video = info
            .streams
            .iter()
            .find_map(|s| match &s.kind {
                ffcine::probe::StreamKind::Video(v) => Some(v.clone()),
                _ => None,
            })
            .context("no video stream")?;
        let rate = self
            .fps
            .or(video.frame_rate)
            .map_or(24000.0 / 1001.0, |r| f64::from(r.num) / f64::from(r.den));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frames = self.frames.unwrap_or_else(|| {
            let d = info.duration.map_or(0.0, |d| d.as_secs_f64());
            ((d - self.ss).max(0.0) * rate).ceil() as u64
        });
        let mut out = self.render_profile.clone().unwrap_or_else(|| {
            let s = self.size.unwrap_or(Size {
                width: video.width,
                height: video.height,
            });
            Profile::new("custom", s.width, s.height)
        });
        if self.safe_area {
            out.safe_area = Some(out.default_safe_area());
        }
        let reframe = match self.reframe.as_deref() {
            None => Reframe::default(),
            Some(s) => {
                let text = match s.strip_prefix('@') {
                    Some(f) => {
                        std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?
                    }
                    None => s.to_owned(),
                };
                let v: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(&text).context("--reframe: expected a JSON object")?;
                Reframe::parse(&v, Some(&out.name))?
            }
        };
        let shot = Shot {
            width: video.width,
            height: video.height,
            frames,
            rate,
            start: self.ss,
        };
        let windows = reframe.frames(&shot, &out);
        ffcine::reframe::pass::apply(grade, &windows, &[], (video.width, video.height), &out);
        if let Some(c) = self.crop {
            // A static --crop wins over the windows.
            grade.crops.clear();
            grade.crop = Some(c);
        }
        ffcine::hooks::order(&mut grade.hooks);
        Ok(())
    }

    fn into_pipeline(mut self) -> anyhow::Result<Pipeline> {
        let lut = self
            .lut
            .take()
            .map(|p| {
                Lut::from_cube_file(&p).map(|lut| LutStage {
                    lut,
                    kind: self.lut_type,
                })
            })
            .transpose()?;
        let mut grade = Grade {
            input_color: self.in_color.unwrap_or_default(),
            lut,
            tone_mapping: self.tonemap,
            gamut_mapping: self.gamut,
            color_map: ColorMapTuning {
                contrast_recovery: self.contrast_recovery,
                perceptual_strength: self.perceptual_strength,
                ..ColorMapTuning::default()
            },
            peak_detect: !self.no_peak_detect,
            deband: (self.deband && !self.no_deband).then(Deband::default),
            dither: self.dither.0.map(|method| Dither {
                method,
                temporal: self.temporal_dither,
            }),
            crop: self.crop,
            size: self.size,
            scaling: Scaling {
                upscaler: self.upscaler,
                downscaler: self.downscaler,
                linear: self.linear,
                sigmoid: self.sigmoid,
            },
            sharpen: self.sharpen,
            shaders: std::mem::take(&mut self.shader),
            denoise: Some(
                Denoise {
                    temporal: (self.temporal_radius > 0).then_some(Temporal {
                        radius: self.temporal_radius,
                        motion: self.temporal_motion,
                        strength: self.temporal_strength,
                        chroma: self.temporal_chroma,
                    }),
                    spatial: (self.denoise > 0.0 || self.denoise_chroma.is_some_and(|c| c > 0.0))
                        .then_some(Spatial {
                            radius: self.denoise_radius,
                            luma: self.denoise,
                            chroma: self.denoise_chroma.unwrap_or(1.5 * self.denoise),
                        }),
                }
                .clamped(),
            )
            .filter(|d| !d.is_off()),
            grain: (self.grain > 0.0).then_some(Grain {
                strength: self.grain,
                size: self.grain_size,
                chroma: self.grain_chroma,
            }),
            ..Grade::default()
        };
        if let Some(a) = self.adaptive_sharpen {
            ffcine::hooks::sharpen(&mut grade, a, ffcine::hooks::Sharpen::Adaptive);
        }
        if self.reframe.is_some() || self.render_profile.is_some() || self.safe_area {
            self.apply_reframe(&mut grade)?;
        }

        let mut enc = if self.preview {
            EncoderSettings::x264_preview()
        } else {
            EncoderSettings::nvenc_hevc_master()
        };
        if let Some(c) = self.codec {
            enc.codec = c;
        }
        enc.preset = self.preset.or(enc.preset);
        enc.tune = self.tune.or(enc.tune);
        enc.profile = self.profile.or(enc.profile);
        enc.rate = self.rate.unwrap_or(enc.rate);
        enc.pixel_format = self.pix_fmt.unwrap_or(enc.pixel_format);
        enc.gop = self.gop.or(enc.gop);

        let device = match (self.cuda, self.vulkan) {
            (Some(_), Some(_)) => anyhow::bail!("--cuda and --vulkan are mutually exclusive"),
            (Some(n), None) => Device::Cuda(n),
            (None, Some(n)) => Device::Vulkan(n),
            (None, None) => Device::Auto,
        };

        if !(self.ss.is_finite() && self.ss >= 0.0) {
            anyhow::bail!("--ss must be a non-negative number of seconds");
        }
        let mut p = Pipeline::new(self.input, self.output)
            .range(Duration::from_secs_f64(self.ss), self.frames)
            .grade(grade)
            .encoder(enc)
            .device(device)
            .decode(self.decode);
        if let Some(r) = self.fps {
            p = p.frame_rate(r);
        }
        Ok(p)
    }
}

fn fetch_luts(dir: Option<PathBuf>, force: bool) -> anyhow::Result<()> {
    let dir = dir
        .or_else(|| camera::lut_dirs().into_iter().next())
        .context("no LUT dir: set FFCINE_LUT_DIR or HOME")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, url) in camera::VENDOR_LUTS {
        let path = dir.join(name);
        if path.is_file() && !force {
            println!("have {}", path.display());
            continue;
        }
        let tmp = path.with_extension("part");
        let ok = std::process::Command::new("curl")
            .args(["-fsSL", "--retry", "2", "-o"])
            .arg(&tmp)
            .arg(url)
            .status()
            .context("running curl")?
            .success();
        if !ok {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("downloading {url} failed");
        }
        // Make sure it is a LUT, not an error page.
        Lut::from_cube_file(&tmp).with_context(|| format!("{url} is not a .cube LUT"))?;
        std::fs::rename(&tmp, &path)?;
        println!("got {}", path.display());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // one arm per command
pub(crate) fn run(cmd: Cmd) -> anyhow::Result<()> {
    let mut ff = backend::Ffmpeg::new()?;
    let (no_cache, cache_dir) = match &cmd {
        Cmd::Grade(a) => (a.no_cache, a.cache_dir.clone()),
        Cmd::Batch {
            no_cache,
            cache_dir,
            ..
        } => (*no_cache, cache_dir.clone()),
        Cmd::Still(a) => (a.no_cache, None),
        Cmd::Looks(a) => (a.no_cache, None),
        Cmd::Probe { .. }
        | Cmd::FetchLuts { .. }
        | Cmd::Stabilize(_)
        | Cmd::Jitter(_)
        | Cmd::Import(_)
        | Cmd::Render(_)
        | Cmd::Diff { .. }
        | Cmd::Chapters { .. }
        | Cmd::Credits { .. }
        | Cmd::Subs { .. }
        | Cmd::Triage(_) => (false, None),
    };
    if no_cache {
        ff = ff.with_cache_dir(None);
    } else if let Some(d) = cache_dir {
        ff = ff.with_cache_dir(Some(d));
    }
    match cmd {
        Cmd::Probe { file } => {
            let info = ff
                .probe(&file)
                .with_context(|| format!("probing {}", file.display()))?;
            println!("{info}");
        }
        Cmd::Grade(args) => deliver::grade(args, &ff)?,
        Cmd::Chapters {
            intro,
            output,
            timeline,
        } => deliver::chapters(&timeline, &intro, output.as_deref())?,
        Cmd::Credits {
            public,
            output,
            timeline,
        } => deliver::credits(&timeline, public, output.as_deref())?,
        Cmd::Still(args) => deliver::still(args, &ff)?,
        Cmd::Looks(args) => looks::looks(args, &ff)?,
        Cmd::Subs {
            offset,
            into,
            output,
            subs,
        } => deliver::subs(&subs, &into, output.as_deref(), offset)?,
        Cmd::Batch { file, .. } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let mut shots = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let at = || format!("{}:{}", file.display(), i + 1);
                let words = split_words(line).with_context(at)?;
                if words.is_empty() {
                    continue;
                }
                let words: Vec<&str> = words.iter().map(String::as_str).collect();
                let args = grade_args()
                    .to_options()
                    .run_inner(&words[..])
                    .map_err(|e| {
                        let msg = match e {
                            bpaf::ParseFailure::Stderr(d) | bpaf::ParseFailure::Stdout(d, _) => {
                                d.monochrome(false)
                            }
                            bpaf::ParseFailure::Completion(c) => c,
                        };
                        anyhow::anyhow!("{}: {msg}", at())
                    })?;
                shots.push(args.build(&ff).with_context(at)?);
            }
            let t0 = std::time::Instant::now();
            let mut frames = 0;
            for p in &shots {
                let s = p
                    .run(&ff)
                    .with_context(|| format!("grading {}", p.input.display()))?;
                report(&p.output, &s);
                frames += s.frames;
            }
            let wall = t0.elapsed().as_secs_f64();
            #[allow(clippy::cast_precision_loss)]
            let fps = frames as f64 / wall;
            eprintln!(
                "{} shots, {frames} frames in {wall:.2} s ({fps:.1} fps)",
                shots.len()
            );
        }
        Cmd::FetchLuts { dir, force } => fetch_luts(dir, force)?,
        Cmd::Stabilize(args) => args.run()?,
        Cmd::Jitter(args) => args.run()?,
        Cmd::Import(args) => import(&args)?,
        Cmd::Render(args) => args.run(&ff)?,
        Cmd::Diff {
            profile,
            render_profile,
            all,
            old,
            new,
        } => assemble::diff(&profile, render_profile.as_deref(), all, &old, &new)?,
        Cmd::Triage(args) => args.run()?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_invariants() {
        cmd().to_options().check_invariants(false);
    }

    #[test]
    fn batch_lines_split_like_a_shell() {
        assert_eq!(
            split_words(r#"--lut "a b.cube" in.mov 'o ut.mkv' # note"#).unwrap(),
            ["--lut", "a b.cube", "in.mov", "o ut.mkv"]
        );
        assert_eq!(
            split_words("   # only a comment").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(split_words("a#b ''").unwrap(), ["a#b", ""]);
        assert!(split_words("'open").is_err());
    }

    #[test]
    fn grade_defaults_match_prototype() {
        let Cmd::Grade(args) = cmd()
            .to_options()
            .run_inner(&[
                "grade",
                "--sharpen",
                "0.6",
                "--cuda",
                "0",
                "in.mov",
                "out.mkv",
            ])
            .unwrap()
        else {
            panic!("expected grade");
        };
        let p = args.into_pipeline().unwrap();
        assert_eq!(p.device, Device::Cuda(0));
        assert_eq!(p.encoder, EncoderSettings::nvenc_hevc_master());
        let ffcine::Stage::Grade(g) = &p.stages[0] else {
            panic!("expected grade stage");
        };
        assert_eq!(g.tone_mapping, ToneMapping::Bt2390);
        assert_eq!(g.scaling, Scaling::default());
        assert!((g.sharpen - 0.6).abs() < f32::EPSILON);
        assert!(g.denoise.is_none() && g.grain.is_none());
    }

    #[test]
    fn stabilise_spelling_still_parses() {
        for word in ["--stabilize", "--stabilise"] {
            let words = ["grade", word, "in.mov", "out.mkv"];
            let Cmd::Grade(args) = cmd().to_options().run_inner(&words[..]).unwrap() else {
                panic!("not grade")
            };
            assert!(args.stabilize);
        }
        for word in ["stabilize", "stabilise"] {
            let words = [word, "--frames", "10", "in.mov"];
            let parsed = cmd().to_options().run_inner(&words[..]);
            assert!(
                matches!(parsed, Ok(Cmd::Stabilize(_))),
                "{word}: {parsed:?}"
            );
        }
    }

    #[test]
    fn grade_denoise_flags() {
        let Cmd::Grade(args) = cmd()
            .to_options()
            .run_inner(&[
                "grade",
                "--temporal-radius",
                "7",
                "--denoise",
                "2",
                "--grain",
                "3",
                "in.mov",
                "out.mkv",
            ])
            .unwrap()
        else {
            panic!("expected grade");
        };
        let p = args.into_pipeline().unwrap();
        let ffcine::Stage::Grade(g) = &p.stages[0] else {
            panic!("expected grade stage");
        };
        let d = g.denoise.unwrap();
        assert_eq!(d.temporal.unwrap().radius, 5);
        assert!((d.spatial.unwrap().chroma - 3.0).abs() < f32::EPSILON);
        assert!((g.grain.unwrap().strength - 3.0).abs() < f32::EPSILON);
    }
}
