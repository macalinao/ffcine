//! Comps (`ffcine.comp/1`, docs/compositing.md): `ffcine comp check`, and the comp side of
//! `ffcine render` and `ffcine still`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use bpaf::Bpaf;
use ffcine::assembly;
use ffcine::comp::{Base, Comp, Source};
use ffcine::{Backend, backend};

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::doc_markdown)] // help text
pub(crate) enum CompCmd {
    /// Read a comp, compile its expressions and sample every frame; report its pieces.
    ///
    /// Probes each video and still layer's size, lowers every piece as `render` would and prints the
    /// canvas, the layers and one line per piece (frames, layers drawn, whether the bottom
    /// video is the pass input). Exits non-zero on the first error, with the parameter's path.
    /// A layer entirely outside the canvas is never drawn, so its tracks aren't sampled (a
    /// warning says so).
    #[bpaf(command, footer(crate::help::COMP_CHECK))]
    Check {
        /// Render profile the pieces are lowered for: preview (1080 px short side), master or
        /// final (2160 px) or review (720 px).
        #[bpaf(argument("NAME"), fallback("preview".to_owned()), display_fallback)]
        profile: String,
        /// Comp file (.comp.json or .comp.yaml).
        #[bpaf(positional("COMP"))]
        comp: PathBuf,
    },
}

/// Whether `path` holds a comp (`"schema": "ffcine.comp/N"`) rather than a timeline.
pub(crate) fn is_comp_file(path: &Path) -> bool {
    ffcine::comp::is_comp_file(path)
}

pub(crate) fn load(path: &Path) -> anyhow::Result<Comp> {
    Comp::from_file(path).with_context(|| format!("reading {}", path.display()))
}

/// Probed video sizes for lowering.
pub(crate) fn dims(ff: &dyn Backend) -> impl Fn(&Path) -> ffcine::Result<(u32, u32)> + '_ {
    move |p: &Path| {
        let info = ff.probe(p)?;
        let (_, v) = info.video().ok_or_else(|| ffcine::Error::Invalid {
            what: "layer source (no picture stream)",
            value: p.display().to_string(),
        })?;
        Ok((v.width, v.height))
    }
}

/// The `name` profile at the comp's canvas aspect.
pub(crate) fn profile(c: &Comp, name: &str) -> anyhow::Result<assembly::Profile> {
    let [w, h] = c.canvas.size;
    assembly::Profile::for_canvas(name, w, h).map_err(|e| anyhow::anyhow!(e))
}

/// Plans a comp file for the `name` profile.
pub(crate) fn plan(
    path: &Path,
    name: &str,
    ff: &dyn Backend,
) -> anyhow::Result<(Comp, assembly::Plan)> {
    let c = load(path)?;
    let p = profile(&c, name)?;
    let dims = dims(ff);
    let media = assembly::CompMedia {
        fp: &assembly::FsFingerprinter,
        dims: &dims,
    };
    let plan = assembly::plan_comp(&c, &p, &media)
        .with_context(|| format!("planning {}", path.display()))?;
    Ok((c, plan))
}

/// A renderer for comps (no grader or stabilizer: comp layers have no grade yet).
pub(crate) fn renderer(
    ff: &backend::Ffmpeg,
    cache: PathBuf,
    cuda: Option<u32>,
    decode: ffcine::Decode,
    json: bool,
) -> assembly::Renderer<'_> {
    assembly::Renderer {
        grader: None,
        detect: None,
        backend: ff,
        cache,
        ffmpeg: std::env::var_os("FFMPEG").unwrap_or_else(|| "ffmpeg".into()),
        device: cuda.map_or(ffcine::Device::Auto, ffcine::Device::Cuda),
        decode,
        progress: if json {
            assembly::Progress::Json
        } else {
            assembly::Progress::Human
        },
    }
}

#[allow(clippy::cast_precision_loss)]
pub(crate) fn run(cmd: CompCmd, ff: &backend::Ffmpeg) -> anyhow::Result<()> {
    match cmd {
        CompCmd::Check { profile, comp } => {
            let (c, plan) = plan(&comp, &profile, ff)?;
            let cv = &c.canvas;
            println!(
                "canvas {}x{} at {} fps, {} s ({} frames); rendered at {}x{} ({})",
                cv.size[0],
                cv.size[1],
                cv.fps,
                cv.duration,
                c.frames(),
                plan.profile.size.width,
                plan.profile.size.height,
                plan.profile.name
            );
            let probe = dims(ff);
            for (i, l) in c.layers.iter().enumerate() {
                if c.span(l).is_empty() {
                    eprintln!(
                        "warning: layer {i} is outside the canvas (in {} s): never drawn, its \
                         tracks aren't sampled",
                        l.in_
                    );
                }
                let src = match &l.source {
                    Source::Video(p) => {
                        let (w, h) = probe(p).with_context(|| format!("layer {i}"))?;
                        format!("video {} ({w}x{h})", p.display())
                    }
                    Source::Still(p) => {
                        let (w, h) = probe(p).with_context(|| format!("layer {i}"))?;
                        format!("still {} ({w}x{h})", p.display())
                    }
                    Source::Solid(_) => "solid".to_owned(),
                    Source::Text { .. } => "text".to_owned(),
                    Source::Comp(n) => format!(
                        "comp ({}x{}, {} layers)",
                        n.canvas.size[0],
                        n.canvas.size[1],
                        n.layers.len()
                    ),
                };
                println!(
                    "layer {i}{}: {src}, {:.3}-{:.3} s, {}x{} box",
                    l.id.as_deref()
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default(),
                    l.in_,
                    l.out,
                    l.size[0],
                    l.size[1]
                );
            }
            for s in &plan.segments {
                let assembly::Unit::Comp(p) = &s.unit else {
                    continue;
                };
                println!(
                    "piece {} ({} frames): {} layers{}  {}",
                    s.name,
                    s.frames,
                    p.shown.len(),
                    match p.base {
                        Base::Video { .. } => ", bottom video promoted to the pass input",
                        Base::Black => "",
                    },
                    s.key.short()
                );
            }
            println!("ok: {} pieces, every frame sampled", plan.segments.len());
        }
    }
    Ok(())
}
