//! `ffcine render` and `ffcine diff`: whole-timeline assembly from the segment cache
//! (`docs/assembly.md`).

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use bpaf::Bpaf;
use ffcine::assembly;
use ffcine::{Decode, Device, backend};

#[derive(Debug, Clone, Bpaf)]
// Doc comments are --help text, not rustdoc.
#[allow(clippy::doc_markdown)]
pub(crate) struct RenderArgs {
    /// Render profile: preview (1080 px short side) or master (2160 px).
    #[bpaf(argument("NAME"), fallback("preview".to_owned()), display_fallback)]
    profile: String,
    /// Output aspect: landscape (16:9), reel (9:16), feed (4:5), square (1:1), WxH or NAME=WxH.
    /// Overrides the timeline's project.output.aspect; each clip's reframe and text take their
    /// profiles.<NAME> overrides.
    #[bpaf(argument("ASPECT"))]
    render_profile: Option<String>,
    /// Segment cache (default: .ffcine-cache next to the timeline).
    #[bpaf(argument("DIR"))]
    cache: Option<PathBuf>,
    /// Print the plan and what would render, render nothing.
    dry_run: bool,
    /// Leave out ffcine.text clips (titles, cards, captions).
    no_text: bool,
    #[bpaf(external(crate::audio::mix_flags))]
    mix: crate::audio::MixFlags,
    /// Progress as JSON lines on stdout.
    json: bool,
    /// CUDA device; Vulkan is derived from it.
    #[bpaf(argument("N"))]
    cuda: Option<u32>,
    /// Hardware decode API: vulkan or cuda.
    #[bpaf(argument("API"), fallback(Decode::Vulkan), display_fallback)]
    decode: Decode,
    /// Output master (.mp4).
    #[bpaf(short('o'), long("output"), argument("FILE"))]
    output: PathBuf,
    #[bpaf(positional("TIMELINE"))]
    timeline: PathBuf,
}

/// The `name` render profile of timeline `t`, its aspect overridden by `aspect`.
fn profile(
    t: &ffcine::timeline::Timeline,
    name: &str,
    aspect: Option<&str>,
) -> anyhow::Result<assembly::Profile> {
    let output = t.project().transpose()?.unwrap_or_default().output;
    assembly::Profile::resolve(name, &output, aspect).map_err(|e| anyhow::anyhow!(e))
}

/// Plans a timeline file; also returns its text layer (`None` with `no_text`).
fn plan_file(
    path: &Path,
    profile_name: &str,
    aspect: Option<&str>,
    no_text: bool,
) -> anyhow::Result<(assembly::Plan, Option<assembly::TextLayer>)> {
    let t = ffcine::timeline::Timeline::from_file(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let profile = &profile(&t, profile_name, aspect)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let text = if no_text {
        None
    } else {
        Some(assembly::TextLayer::from_timeline(&t, dir).context("text clips")?)
    };
    let p = assembly::plan(&t, dir, profile, &assembly::FsFingerprinter, text.as_ref())
        .with_context(|| format!("planning {}", path.display()))?;
    for w in &p.warnings {
        eprintln!("warning: {w}");
    }
    Ok((p, text))
}

pub(crate) fn diff(
    profile: &str,
    aspect: Option<&str>,
    all: bool,
    old: &Path,
    new: &Path,
) -> anyhow::Result<()> {
    let (old, new) = (
        plan_file(old, profile, aspect, false)?.0,
        plan_file(new, profile, aspect, false)?.0,
    );
    print!("{}", assembly::Diff::new(&old, &new).report(&new, all));
    Ok(())
}

impl RenderArgs {
    /// The plan, with the mix flags over the timeline's mix settings.
    fn plan(&self) -> anyhow::Result<(assembly::Plan, Option<assembly::TextLayer>)> {
        let (mut plan, text) = plan_file(
            &self.timeline,
            &self.profile,
            self.render_profile.as_deref(),
            self.no_text,
        )?;
        if let Some(a) = &mut plan.audio {
            let mut o = a.options.clone();
            self.mix.apply(&mut o);
            a.set_options(o);
        }
        Ok((plan, text))
    }

    pub(crate) fn run(self, ff: &backend::Ffmpeg) -> anyhow::Result<()> {
        let (plan, mut text) = self.plan()?;
        let profile = &plan.profile;
        let cache = self.cache.unwrap_or_else(|| {
            self.timeline
                .parent()
                .unwrap_or(Path::new("."))
                .join(".ffcine-cache")
        });
        if self.dry_run {
            let cached = |k: &assembly::Key| cache.join("seg").join(format!("{k}.mp4")).exists();
            let empty = assembly::Plan {
                segments: plan
                    .segments
                    .iter()
                    .filter(|s| cached(&s.key))
                    .cloned()
                    .collect(),
                audio: plan.audio.clone().filter(|a| {
                    cache
                        .join("audio")
                        .join(format!("{}-aac{}.m4a", a.key, profile.audio_bitrate))
                        .exists()
                }),
                ..plan.clone()
            };
            print!("{}", assembly::Diff::new(&empty, &plan).report(&plan, true));
            return Ok(());
        }
        // The full GPU grade (LUT strength, look hook, sharpen), as `ffcine grade --clip`.
        let t = ffcine::timeline::Timeline::from_file(&self.timeline)?;
        let project = t.project().transpose()?.unwrap_or_default();
        let dir = self.timeline.parent().unwrap_or(Path::new("."));
        let luts = RefCell::new(super::looks::Luts::default());
        let warned = RefCell::new(HashSet::new());
        let grader = |s: &assembly::Shot| {
            let (g, skipped) =
                super::looks::grade_for(&project, dir, &s.grade, &mut luts.borrow_mut())
                    .map_err(|e| format!("{e:#}"))?;
            // The renderer denoises (Shot::filters), not the grade.
            for w in skipped.into_iter().filter(|w| !w.starts_with("denoise")) {
                if warned.borrow_mut().insert(w.clone()) {
                    eprintln!("warning: {w}");
                }
            }
            Ok(g)
        };
        let detect = |p: &Path, r: &ffcine::stabilize::SourceRange| {
            let t = std::time::Instant::now();
            let (det, cached) =
                ffcine::stabilize::detect_cached(p, r, &ffcine::stabilize::DetectParams::default())
                    .map_err(|e| format!("{e:#}"))?;
            if !self.json {
                eprintln!(
                    "stabilize: {} frames of {}, detect {:.1} s{}",
                    r.frames,
                    p.display(),
                    t.elapsed().as_secs_f64(),
                    if cached { " (cached)" } else { "" }
                );
            }
            Ok(det)
        };
        let r = assembly::Renderer {
            grader: Some(&grader),
            detect: Some(&detect),
            backend: ff,
            cache,
            ffmpeg: std::env::var_os("FFMPEG").unwrap_or_else(|| "ffmpeg".into()),
            device: self.cuda.map_or(Device::Auto, Device::Cuda),
            decode: self.decode,
            progress: if self.json {
                assembly::Progress::Json
            } else {
                assembly::Progress::Human
            },
        };
        let rep = r.render(&plan, text.as_mut(), &self.output)?;
        if !self.json {
            let s = |d: Option<std::time::Duration>| {
                d.map_or_else(
                    || "cached".to_owned(),
                    |d| format!("{:.1} s", d.as_secs_f64()),
                )
            };
            eprintln!(
                "segments {:.1} s ({} of {}, {} frames), audio {}, join+mux {}, total {:.1} s",
                rep.render.as_secs_f64(),
                rep.rendered,
                rep.segments,
                rep.rendered_frames,
                s(rep.audio),
                s(rep.mux),
                rep.total.as_secs_f64()
            );
        }
        Ok(())
    }
}
