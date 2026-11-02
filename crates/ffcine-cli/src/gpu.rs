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
    /// Show container, streams, codecs and colour tags.
    #[bpaf(command)]
    Probe {
        #[bpaf(positional("FILE"))]
        file: PathBuf,
    },
    /// Grade a clip on the GPU: tone mapping, crop/scale, 3D LUT, sharpen, encode.
    #[bpaf(command)]
    Grade(#[bpaf(external(grade_args))] GradeArgs),
    /// Grade many shots through one GPU pipeline (device, renderer and shader cache stay warm).
    /// FILE has one shot per line, written as `grade` arguments; `#` starts a comment.
    #[bpaf(command)]
    Batch {
        /// libplacebo shader cache directory (default: ~/.cache/ffcine/placebo).
        #[bpaf(argument("DIR"))]
        cache_dir: Option<PathBuf>,
        /// Don't use an on-disk shader cache.
        no_cache: bool,
        #[bpaf(positional("FILE"))]
        file: PathBuf,
    },
    /// Stabilize a shot: optical-flow motion detect on the GPU (cached), vidstab-style smoothing,
    /// auto zoom. Writes the per-frame warps.
    #[bpaf(command, long("stabilise"))]
    Stabilize(#[bpaf(external(stabilize_args))] StabilizeArgs),
    /// Residual jitter of a clip: RMS of its frame-to-frame motion minus a smoothed version.
    #[bpaf(command)]
    Jitter(#[bpaf(external(jitter_args))] JitterArgs),
    /// Download the camera vendors' LUTs ffcine knows (DJI Osmo Pocket 4 D-Log -> Rec.709) into
    /// the LUT dir (default: the first of $FFCINE_LUT_DIR, ~/.local/share/ffcine/luts).
    #[bpaf(command)]
    FetchLuts {
        #[bpaf(argument("DIR"))]
        dir: Option<PathBuf>,
        /// Download again even if the file is there.
        force: bool,
    },
    /// Bake a camera LUT, a grade LUT and a look into one .cube (camera, then LUT, then look),
    /// evaluated in Rust: for other tools, or as a single --lut.
    #[bpaf(command)]
    Bake(#[bpaf(external(bake_args))] BakeArgs),
    /// Convert another editor's project (edit.yaml, OTIO) to an ffcine timeline (.otio).
    #[bpaf(command)]
    Import(#[bpaf(external(import_args))] ImportArgs),
    /// Index a card before editing: contact strips, scene cuts, speech map, GPU transcripts with
    /// word times, runaway flags, and a clip page to pick from.
    #[bpaf(command)]
    Triage(#[bpaf(external(crate::triage::triage_args))] crate::triage::TriageArgs),
    /// YouTube chapter lines from the timeline's markers (those flagged `chapter`, else all).
    #[bpaf(command)]
    #[allow(clippy::doc_markdown)]
    Chapters {
        /// Title of the 0:00 chapter added when the first marker is later.
        #[bpaf(argument("TITLE"), fallback("Intro".to_owned()))]
        intro: String,
        /// Write here instead of stdout.
        #[bpaf(short('o'), argument("FILE"))]
        output: Option<PathBuf>,
        #[bpaf(positional("TIMELINE"))]
        timeline: PathBuf,
    },
    /// Music credits (CC BY attribution lines) from the music clips' metadata.
    #[bpaf(command)]
    Credits {
        /// The public cut: leave out `private` sounds.
        public: bool,
        /// Write here instead of stdout.
        #[bpaf(short('o'), argument("FILE"))]
        output: Option<PathBuf>,
        #[bpaf(positional("TIMELINE"))]
        timeline: PathBuf,
    },
    /// A full-resolution PNG of the timeline at a time (thumbnails), text included.
    #[bpaf(command)]
    Still(#[bpaf(external(deliver::still_args))] deliver::StillArgs),
    /// Compare grade variants (looks, LUTs, overrides) on shots: full-res JPEGs and a labelled
    /// contact sheet per shot, through one warm GPU pipeline.
    #[bpaf(command)]
    Looks(#[bpaf(external(looks::looks_args))] looks::LooksArgs),
    /// Add .ass/.ssa subtitles to a timeline as ffcine.text clips with their styles.
    #[bpaf(command)]
    Subs {
        /// Shift the subtitles by this many seconds.
        #[bpaf(argument("SECONDS"), fallback(0.0))]
        offset: f64,
        /// Timeline to add them to.
        #[bpaf(argument("TIMELINE"))]
        into: PathBuf,
        /// Output timeline (default: overwrite --into).
        #[bpaf(short('o'), argument("FILE"))]
        output: Option<PathBuf>,
        #[bpaf(positional("SUBS"))]
        subs: PathBuf,
    },
    /// Render a whole timeline into one master, applying each clip's grade, stabilize, denoise,
    /// reframe and speed ramp from the timeline, in its project.output aspect (or
    /// --render-profile). Segments are cached by content: after an edit only changed ones render,
    /// the master joins them by stream copy, and an audio-only change is a remux. Re-run after an
    /// interruption to resume. The audio is the finished mix (`ffcine audio`): clean chain, R128
    /// levelling, music ducked under dialogue, clip gain and fade keyframes, synced recorder
    /// clips, master limiter; its settings come from the timeline (project.mix, clip effects), the
    /// mix flags override them.
    #[bpaf(command)]
    Render(#[bpaf(external(assemble::render_args))] assemble::RenderArgs),
    /// Which segments of NEW render that OLD's did not (and whether the audio changed).
    #[bpaf(command)]
    Diff {
        /// Render profile: preview (1080p) or master (4K).
        #[bpaf(argument("NAME"), fallback("preview".to_owned()))]
        profile: String,
        /// Output aspect (overrides the timeline's project.output.aspect): landscape (16:9), reel
        /// (9:16), feed (4:5), square (1:1), WxH or NAME=WxH.
        #[bpaf(argument("ASPECT"))]
        render_profile: Option<String>,
        /// List unchanged segments too.
        all: bool,
        #[bpaf(positional("OLD"))]
        old: PathBuf,
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
pub(crate) struct ImportArgs {
    /// Source format (edit-yaml, otio, resolve, fcpxml, mlt, fcp7, edl, capcut); default: from
    /// the file (Resolve's .otio is detected).
    #[bpaf(argument("FORMAT"))]
    format: Option<ffcine::import::Format>,
    /// Timeline frame rate, e.g. 24000/1001 (default: the source's).
    #[bpaf(argument::<String>("FPS"), parse(|s: String| parse_rate(&s)), optional)]
    rate: Option<f64>,
    /// Timeline frame size, for sources without one (Resolve OTIO).
    #[bpaf(long("size"), long("frame-size"), argument("WxH"))]
    size: Option<Size>,
    /// Fail on anything the importer can't read instead of skipping it.
    strict: bool,
    /// Output .otio file.
    #[bpaf(short('o'), long("output"), argument("FILE"))]
    output: PathBuf,
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
    #[bpaf(argument("SECONDS"), fallback(0.0))]
    start: f64,
    /// Frames to stabilize.
    #[bpaf(argument("N"))]
    frames: usize,
    /// Output frame rate (fps filter).
    #[bpaf(argument("NUM/DEN"), fallback(NTSC_FILM), display_fallback)]
    rate: Rate,
    /// Gaussian smoothing half-window in frames (vidstab smoothing).
    #[bpaf(argument("FRAMES"), fallback(20), display_fallback)]
    smoothing: u32,
    /// Optical flow analysis width.
    #[bpaf(argument("PX"), fallback(1920), display_fallback)]
    analysis_width: u32,
    /// Optical flow quality: slow, medium or fast.
    #[bpaf(argument("Q"), fallback(FlowQuality::Slow), display_fallback)]
    quality: FlowQuality,
    /// Cap the automatic zoom (borders may show beyond it).
    #[bpaf(argument("Z"))]
    max_zoom: Option<f64>,
    /// Fixed zoom instead of the automatic one.
    #[bpaf(argument("Z"))]
    zoom: Option<f64>,
    /// Ignore and overwrite the detect cache.
    no_cache: bool,
    /// Write the warps as a libplacebo/mpv .hook shader.
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
    #[bpaf(positional("INPUT"))]
    input: PathBuf,
}

#[derive(Debug, Clone, Bpaf)]
pub(crate) struct JitterArgs {
    /// Range start in seconds.
    #[bpaf(argument("SECONDS"), fallback(0.0))]
    start: f64,
    #[bpaf(argument("N"))]
    frames: usize,
    #[bpaf(argument("NUM/DEN"), fallback(NTSC_FILM), display_fallback)]
    rate: Rate,
    /// Smoothing sigma in frames: motion slower than this counts as intended.
    #[bpaf(argument("FRAMES"), fallback(5.0), display_fallback)]
    sigma: f64,
    /// Skip the frame pair ending at this frame (a cut or punch-in); repeatable.
    #[bpaf(argument("FRAME"), many)]
    exclude: Vec<usize>,
    #[bpaf(argument("PX"), fallback(1920), display_fallback)]
    analysis_width: u32,
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

/// `--hdr`: the HDR deliverable's transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HdrOut {
    Pq,
    Hlg,
}

impl FromStr for HdrOut {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pq" | "hdr10" => Ok(Self::Pq),
            "hlg" => Ok(Self::Hlg),
            _ => Err(format!("unknown HDR output {s:?}, expected pq or hlg")),
        }
    }
}

impl HdrOut {
    fn color(self) -> ffcine::ColorSpace {
        match self {
            Self::Pq => ffcine::ColorSpace::BT2020_PQ,
            Self::Hlg => ffcine::ColorSpace::BT2020_HLG,
        }
    }
}

#[derive(Debug, Clone, Bpaf)]
// Doc comments are --help text, not rustdoc.
#[allow(clippy::struct_excessive_bools, clippy::doc_markdown)]
pub(crate) struct GradeArgs {
    /// Start time in seconds (accurate seek, like ffmpeg's input -ss).
    #[bpaf(argument("SECONDS"), fallback(0.0))]
    ss: f64,
    /// Number of frames to output (the last frame repeats if the source ends first).
    #[bpaf(argument("N"))]
    frames: Option<u64>,
    /// Output frame rate (N/D or N), like ffmpeg's fps filter.
    #[bpaf(argument("RATE"))]
    fps: Option<Rational>,
    /// Override source colour tags: matrix/primaries/transfer/range, '-' keeps one.
    #[bpaf(argument("M/P/T/R"))]
    in_color: Option<ColorOverride>,
    /// Camera profile: auto (from the probe), rec709, hdr, dlogm, dlogm-vivid, clog3, applelog.
    /// Log profiles add their camera LUT before the grade; auto adds none when --lut is given
    /// (a baked grade already includes it). Vendor LUTs come from $FFCINE_LUT_DIR.
    #[bpaf(argument("PROFILE"))]
    camera: Option<ProfileChoice>,
    /// 3D LUT (.cube) to apply.
    #[bpaf(argument("FILE"))]
    lut: Option<PathBuf>,
    /// LUT input space: native, normalized or conversion.
    #[bpaf(argument("TYPE"), fallback(LutType::Normalized), display_fallback)]
    lut_type: LutType,
    /// HDR -> SDR tone curve (bt.2390, bt.2446a, st2094-40, st2094-10, spline, hable, mobius,
    /// reinhard, gamma, linear, clip, auto).
    #[bpaf(argument("CURVE"), fallback(ToneMapping::Bt2390), display_fallback)]
    tonemap: ToneMapping,
    /// Gamut mapping (perceptual, softclip, highlight, relative, saturation, absolute,
    /// desaturate, darken, linear, clip).
    #[bpaf(argument("MODE"), fallback(GamutMapping::Perceptual), display_fallback)]
    gamut: GamutMapping,
    /// HDR contrast recovery strength (0 = off).
    #[bpaf(argument("S"), fallback(0.30), display_fallback)]
    contrast_recovery: f32,
    /// Perceptual gamut mapping strength (0..1).
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
    #[bpaf(argument("METHOD"), fallback(DitherArg(Some(DitherMethod::Blue))))]
    dither: DitherArg,
    /// Vary the dither pattern per frame.
    temporal_dither: bool,
    /// Source crop in input pixels.
    #[bpaf(argument("W:H:X:Y"))]
    crop: Option<Crop>,
    /// Output size (default: input size).
    #[bpaf(argument("WxH"))]
    size: Option<Size>,
    /// Upscaling filter.
    #[bpaf(argument("FILTER"), fallback(Scaler::Lanczos), display_fallback)]
    upscaler: Scaler,
    /// Downscaling filter.
    #[bpaf(argument("FILTER"), fallback(Scaler::Lanczos), display_fallback)]
    downscaler: Scaler,
    /// Scale in linear light.
    linear: bool,
    /// Sigmoidized upscaling.
    sigmoid: bool,
    /// Luma unsharp amount after the LUT (ffmpeg unsharp=5:5:AMOUNT).
    #[bpaf(argument("AMOUNT"), fallback(0.0))]
    sharpen: f32,
    /// Adaptive-sharpen strength (curve_height, 0.3 to 2.0) after scaling; replaces --sharpen.
    #[bpaf(argument("STRENGTH"))]
    adaptive_sharpen: Option<f32>,
    /// Render profile: landscape, hd, reel (9:16), feed (4:5), square, WxH or NAME=WxH. Sets the
    /// output size and picks the reframe's per-profile overrides.
    #[bpaf(argument("PROFILE"))]
    render_profile: Option<Profile>,
    /// Reframe: ffcine.geo.reframe/1 parameters as JSON (zoom, punch, push, crop_x, ...), or @FILE.
    #[bpaf(argument("JSON"))]
    reframe: Option<String>,
    /// Draw the render profile's safe area over the picture (review renders).
    safe_area: bool,
    /// Extra mpv .hook shader (repeatable): a file, or a vendored or external shader name
    /// (adaptive-sharpen, artcnn-c4f16, artcnn-c4f16-ds, krigbilateral, fsrcnnx, fsrcnnx-16).
    #[bpaf(argument("FILE|NAME"), many)]
    shader: Vec<PathBuf>,
    /// Stabilize in the grade pass: GPU optical-flow detect (cached), vidstab-style smoothing,
    /// auto zoom, warp before the crop.
    #[bpaf(long("stabilize"), long("stabilise"))]
    stabilize: bool,
    /// --stabilize smoothing half-window in frames (vidstab smoothing).
    #[bpaf(argument("FRAMES"), fallback(20), display_fallback)]
    stab_smoothing: u32,
    /// --stabilize fixed zoom instead of the automatic one.
    #[bpaf(argument("Z"))]
    stab_zoom: Option<f64>,
    /// --stabilize cap on the automatic zoom.
    #[bpaf(argument("Z"))]
    stab_max_zoom: Option<f64>,
    /// Temporal denoise: frames on each side (1-5), motion-compensated; 0 = off.
    #[bpaf(argument("N"), fallback(0))]
    temporal_radius: u32,
    /// Temporal denoise motion: nvof (NVIDIA optical flow) or blocks (Vulkan block matching).
    #[bpaf(argument("SRC"), fallback(ffcine::filters::Motion::Nvof))]
    temporal_motion: ffcine::filters::Motion,
    /// Temporal denoise luma strength (noise sigma, 8-bit code values).
    #[bpaf(argument("S"), fallback(4.0))]
    temporal_strength: f32,
    /// Temporal denoise chroma strength (default: the luma strength).
    #[bpaf(argument("S"))]
    temporal_chroma: Option<f32>,
    /// Spatial (NLM) denoise luma strength, after the temporal one; 0 = off.
    #[bpaf(argument("S"), fallback(0.0))]
    denoise: f32,
    /// Spatial denoise chroma strength (default: 1.5x luma).
    #[bpaf(argument("S"))]
    denoise_chroma: Option<f32>,
    /// Spatial denoise search radius (1-4).
    #[bpaf(argument("R"), fallback(2))]
    denoise_radius: u32,
    /// Film grain sigma at mid-grey (8-bit code values) on the graded output; 0 = off.
    #[bpaf(argument("S"), fallback(0.0))]
    grain: f32,
    /// Grain cell size in output pixels.
    #[bpaf(argument("PX"), fallback(1.5))]
    grain_size: f32,
    /// Chroma grain relative to luma grain.
    #[bpaf(argument("K"), fallback(0.0))]
    grain_chroma: f32,
    /// HDR deliverable: pq (HDR10, with mastering display and content light metadata) or hlg,
    /// BT.2020, 10-bit. HDR sources keep their range; log sources use the analytic HDR camera
    /// transform; SDR sources sit at BT.2408 reference white.
    #[bpaf(argument("pq|hlg"))]
    hdr: Option<HdrOut>,
    /// Mastering peak of the HDR deliverable, cd/m².
    #[bpaf(argument("NITS"), fallback(1000.0), display_fallback)]
    hdr_peak: f64,
    /// 8-bit libx264 preview instead of the 10-bit HEVC NVENC master.
    preview: bool,
    /// Encoder (hevc_nvenc, h264_nvenc, av1_nvenc, hevc_vulkan, libx264, ...).
    #[bpaf(argument("CODEC"))]
    codec: Option<Codec>,
    /// Encoder preset (p1..p7 for NVENC).
    #[bpaf(argument("PRESET"))]
    preset: Option<String>,
    /// Encoder tune (hq, ll, ...).
    #[bpaf(argument("TUNE"))]
    tune: Option<String>,
    /// Encoder profile (main10, high10, ...).
    #[bpaf(argument("PROFILE"))]
    profile: Option<String>,
    /// Rate control: qp=N, crf=N or bitrate=BITS.
    #[bpaf(argument("RC"))]
    rate: Option<RateControl>,
    /// Encoder pixel format (p010le, nv12, yuv420p, ...).
    #[bpaf(argument("FMT"))]
    pix_fmt: Option<PixelFormat>,
    /// Keyframe interval in frames.
    #[bpaf(argument("N"))]
    gop: Option<u32>,
    /// Delivery profile: master, preview, review (under --budget-mb) or phone (1080p HEVC);
    /// sets the encoder and size (--size still wins).
    #[bpaf(argument("PROFILE"))]
    deliver: Option<ffcine::DeliveryProfile>,
    /// Size cap of the review profile, MB (default 50).
    #[bpaf(argument("MB"))]
    budget_mb: Option<f64>,
    /// Draw this timeline's ffcine.text clips over the output.
    #[bpaf(argument("TIMELINE"))]
    text: Option<PathBuf>,
    /// Timeline second the first output frame shows (with --text).
    #[bpaf(argument("SECONDS"), fallback(0.0))]
    text_at: f64,
    /// CUDA device; Vulkan is derived from it (no Vulkan encode).
    #[bpaf(argument("N"))]
    cuda: Option<u32>,
    /// Vulkan device index (instead of --cuda).
    #[bpaf(argument("N"))]
    vulkan: Option<u32>,
    /// Hardware decode API: vulkan (frames go straight to the renderer) or cuda (NVDEC, one copy).
    #[bpaf(argument("API"), fallback(Decode::Vulkan), display_fallback)]
    decode: Decode,
    /// libplacebo shader cache directory (default: ~/.cache/ffcine/placebo).
    #[bpaf(argument("DIR"))]
    cache_dir: Option<PathBuf>,
    /// Don't use an on-disk shader cache.
    no_cache: bool,
    #[bpaf(positional("INPUT"))]
    input: PathBuf,
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
        let hdr_peak = self.hdr.map(|_| self.hdr_peak);
        let mut p = self.into_pipeline()?;
        if let Some(choice) = choice {
            let info = ff
                .probe(&p.input)
                .with_context(|| format!("probing {}", p.input.display()))?;
            let profile = choice.resolve(&info);
            eprintln!("profile: {profile} ({choice})");
            let cam = match hdr_peak {
                Some(peak) => profile.camera_lut_hdr(peak)?,
                None => profile.camera_lut(&camera::lut_dirs())?,
            };
            if let Some(cam) = cam {
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

    /// The master or preview encoder; with `--hdr`, BT.2020 PQ or HLG output, its metadata and
    /// the encoder's tags.
    fn base_encoder(&self, grade: &mut Grade) -> anyhow::Result<EncoderSettings> {
        let mut enc = if self.preview {
            EncoderSettings::x264_preview()
        } else {
            EncoderSettings::nvenc_hevc_master()
        };
        let Some(h) = self.hdr else {
            return Ok(enc);
        };
        if self.preview || self.pix_fmt.is_some_and(|f| !f.as_str().contains("10")) {
            anyhow::bail!("--hdr needs a 10-bit encode (no --preview, a 10-bit --pix-fmt)");
        }
        grade.output = h.color();
        grade.output_hdr =
            (h == HdrOut::Pq).then(|| ffcine::HdrMetadata::P3_D65_1000.with_peak(self.hdr_peak));
        enc.color = grade.output;
        Ok(enc)
    }

    #[allow(clippy::too_many_lines)] // one flag-to-field mapping
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
        let (shaders, named) = split_shaders(std::mem::take(&mut self.shader))?;
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
            shaders,
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
        grade.hooks.extend(named);
        if let Some(a) = self.adaptive_sharpen {
            ffcine::hooks::sharpen(&mut grade, a, ffcine::hooks::Sharpen::Adaptive);
        }
        ffcine::hooks::order(&mut grade.hooks);
        if self.reframe.is_some() || self.render_profile.is_some() || self.safe_area {
            self.apply_reframe(&mut grade)?;
        }

        let mut enc = self.base_encoder(&mut grade)?;
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

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::doc_markdown)]
pub(crate) struct BakeArgs {
    /// Camera profile whose LUT goes first (dlog, dlog-vivid, dlogm, dlogm-vivid, clog3,
    /// applelog); vendor LUTs come from $FFCINE_LUT_DIR, else the analytic transform.
    #[bpaf(argument("PROFILE"))]
    camera: Option<camera::Profile>,
    /// With --camera: the HDR (BT.2020 PQ) camera transform mastered to this peak, cd/m².
    #[bpaf(argument("NITS"))]
    hdr_peak: Option<f64>,
    /// Grade LUT (.cube, normalized input), after the camera LUT.
    #[bpaf(argument("FILE"))]
    lut: Option<PathBuf>,
    /// Look: exposure in stops.
    #[bpaf(argument("STOPS"), fallback(0.0))]
    exposure: f32,
    #[bpaf(argument("W"), fallback(0.0))]
    warmth: f32,
    #[bpaf(argument("T"), fallback(0.0))]
    tint: f32,
    #[bpaf(argument("C"), fallback(0.0))]
    contrast: f32,
    #[bpaf(argument("G"), fallback(1.0))]
    gamma: f32,
    #[bpaf(argument("L"), fallback(0.0))]
    lift: f32,
    #[bpaf(argument("G"), fallback(1.0))]
    gain: f32,
    #[bpaf(argument("S"), fallback(1.0))]
    saturation: f32,
    /// Lattice size.
    #[bpaf(argument("N"), fallback(ffcine::bake::DEFAULT_SIZE), display_fallback)]
    size: usize,
    #[bpaf(positional("OUTPUT"))]
    output: PathBuf,
}

impl BakeArgs {
    fn run(self) -> anyhow::Result<()> {
        if !(2..=256).contains(&self.size) {
            anyhow::bail!("--size must be 2..256");
        }
        let cam = match (self.camera, self.hdr_peak) {
            (Some(p), Some(peak)) => p.camera_lut_hdr(peak)?,
            (Some(p), None) => p.camera_lut(&camera::lut_dirs())?,
            (None, Some(_)) => anyhow::bail!("--hdr-peak needs --camera"),
            (None, None) => None,
        };
        let lut = self
            .lut
            .as_ref()
            .map(|p| Lut::from_cube_file(p).with_context(|| format!("LUT {}", p.display())))
            .transpose()?;
        let look = ffcine::shaders::LookParams {
            exposure: self.exposure,
            warmth: self.warmth,
            tint: self.tint,
            contrast: self.contrast,
            gamma: self.gamma,
            lift: self.lift,
            gain: self.gain,
            saturation: self.saturation,
        };
        let baked = ffcine::Bake {
            camera: cam.as_ref().map(|c| &c.lut),
            lut: lut.as_ref(),
            look,
        }
        .bake(self.size);
        baked.write_cube(&self.output)?;
        println!(
            "{}: {}^3, {}",
            self.output.display(),
            baked.size,
            baked.title.unwrap_or_default()
        );
        Ok(())
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
        | Cmd::Bake(_)
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
        Cmd::Bake(args) => args.run()?,
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

/// `--shader` values: files (used as written), and vendored or external shader names as hooks.
/// A path that exists wins over a name.
fn split_shaders(shaders: Vec<PathBuf>) -> anyhow::Result<(Vec<PathBuf>, Vec<ffcine::Hook>)> {
    let mut files = Vec::new();
    let mut named = Vec::new();
    for s in shaders {
        match s
            .to_str()
            .filter(|_| !s.exists())
            .and_then(ffcine::hooks::named)
        {
            Some(hook) => named.push(hook?),
            None => files.push(s),
        }
    }
    Ok((files, named))
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
