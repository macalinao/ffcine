//! Comps (`ffcine.comp/1`, docs/compositing.md): `ffcine comp check`, `comp lower`, `comp new`,
//! and the comp side of `ffcine render` and `ffcine still`.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

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
    /// Probes each video and still layer's size, lowers every piece as `render` would and
    /// prints the canvas, the layers and one line per piece (frames, layers drawn, whether the
    /// bottom video is the pass input). Exits non-zero on the first error, with the parameter's
    /// path.
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
    /// Print a comp with its template written out: explicit canvas, vars, comps and layers.
    ///
    /// The result is an ordinary comp (JSON), the same one `render` and `check` read, so it can
    /// be edited by hand and rendered instead. It is checked first (expressions compiled,
    /// sources and limits validated; media not probed).
    #[bpaf(command, footer(crate::help::COMP_LOWER))]
    Lower {
        /// Write it here instead of stdout.
        #[bpaf(short('o'), long("output"), argument("FILE"))]
        output: Option<PathBuf>,
        /// Comp file (.comp.json or .comp.yaml).
        #[bpaf(positional("COMP"))]
        comp: PathBuf,
    },
    /// Write a new comp file from a template and a list of media.
    #[bpaf(command, footer(crate::help::COMP_NEW))]
    New {
        /// Template: story.
        #[bpaf(argument("NAME"))]
        template: String,
        /// Tiles (story; default 3, or one a file with fewer); at most one a file.
        #[bpaf(argument("N"))]
        tiles: Option<usize>,
        /// Length in seconds.
        #[bpaf(argument("S"), fallback(6.0), display_fallback)]
        duration: f64,
        /// A picture (video or still); repeat it, in order.
        #[bpaf(argument("FILE"), some("at least one --media FILE"))]
        media: Vec<PathBuf>,
        /// The comp file to write (.comp.json); refuses to overwrite one.
        #[bpaf(short('o'), long("output"), argument("FILE"))]
        output: PathBuf,
    },
}

/// `comp lower`: the document with its template expanded, checked as a comp.
fn lower(path: &Path) -> anyhow::Result<serde_json::Value> {
    let ctx = || format!("reading {}", path.display());
    let doc = ffcine::comp::read_document(path).with_context(ctx)?;
    let out = ffcine::comp::templates::expand(&doc).with_context(ctx)?;
    Comp::from_value(&out, path.parent().unwrap_or(Path::new("."))).with_context(ctx)?;
    Ok(out)
}

/// `p` as written in a comp file at `dir`: relative to `dir` (with `../` when it lies outside
/// it), or absolute when there is no relative path (another drive).
fn relative(p: &Path, dir: &Path) -> anyhow::Result<String> {
    let cwd = std::env::current_dir()?;
    let (p, dir) = (normalize(&cwd.join(p)), normalize(&cwd.join(dir)));
    let (mut pc, mut dc) = (p.components().peekable(), dir.components().peekable());
    while let (Some(a), Some(b)) = (pc.peek(), dc.peek())
        && a == b
    {
        pc.next();
        dc.next();
    }
    let shared = p.components().count() - pc.clone().count();
    let rel: PathBuf = if shared == 0 {
        p.clone()
    } else {
        dc.map(|_| Component::ParentDir).chain(pc).collect()
    };
    rel.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{}: not UTF-8", p.display()))
}

/// `p` with `.` dropped and `..` taken off the path before it, without touching the
/// filesystem.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(out.components().next_back(), Some(Component::Normal(_))) =>
            {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn new(
    template: &str,
    tiles: Option<usize>,
    duration: f64,
    media: &[PathBuf],
    out: &Path,
) -> anyhow::Result<()> {
    if template != "story" {
        anyhow::bail!("unknown template {template:?} for comp new (story)");
    }
    let dir = out
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let media = media
        .iter()
        .map(|p| relative(p, dir))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let tiles = tiles.unwrap_or(media.len().min(3));
    let doc = ffcine::comp::templates::story::starter(&media, tiles, duration)?;
    Comp::from_value(&doc, dir).context("checking the new comp")?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                anyhow::anyhow!("{} exists; not overwriting it", out.display())
            }
            _ => anyhow::Error::new(e).context(format!("writing {}", out.display())),
        })?;
    f.write_all((serde_json::to_string_pretty(&doc)? + "\n").as_bytes())
        .with_context(|| format!("writing {}", out.display()))?;
    println!("wrote {}", out.display());
    Ok(())
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

/// A video's average frame rate and frame count (its stream's count, else its duration's),
/// for time remaps.
pub(crate) fn rate(ff: &dyn Backend) -> impl Fn(&Path) -> ffcine::Result<(f64, u64)> + '_ {
    move |p: &Path| {
        let info = ff.probe(p)?;
        let bad = || ffcine::Error::Invalid {
            what: "time remap source (no frame rate or length)",
            value: p.display().to_string(),
        };
        let (_, v) = info.video().ok_or_else(bad)?;
        let fps = v
            .frame_rate
            .map(ffcine::Rational::as_f64)
            .filter(|r| r.is_finite() && *r > 0.0)
            .ok_or_else(bad)?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frames = v
            .frames
            .filter(|n| *n > 0)
            .or_else(|| {
                info.duration
                    .map(|d| (d.as_secs_f64() * fps).round() as u64)
                    .filter(|n| *n > 0)
            })
            .ok_or_else(bad)?;
        Ok((fps, frames))
    }
}

/// A layer source's camera profile, for `camera: auto` in a layer's grade.
pub(crate) fn camera(
    ff: &dyn Backend,
) -> impl Fn(&Path) -> ffcine::Result<ffcine::camera::Profile> + '_ {
    move |p: &Path| Ok(ffcine::camera::Profile::detect_file(&ff.probe(p)?))
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
    let camera = camera(ff);
    let rate = rate(ff);
    let media = assembly::CompMedia {
        fp: &assembly::FsFingerprinter,
        dims: &dims,
        profile: Some(&camera),
        rate: Some(&rate),
    };
    let plan = assembly::plan_comp(&c, &p, &media)
        .with_context(|| format!("planning {}", path.display()))?;
    Ok((c, plan))
}

/// A renderer for comps (no grader or stabilizer: layer grades are lowered into each layer's
/// own [`ffcine::Grade`]).
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
        CompCmd::Lower { output, comp } => {
            let text = serde_json::to_string_pretty(&lower(&comp)?)? + "\n";
            match output {
                Some(o) => {
                    std::fs::write(&o, text).with_context(|| format!("writing {}", o.display()))?;
                }
                None => print!("{text}"),
            }
        }
        CompCmd::New {
            template,
            tiles,
            duration,
            media,
            output,
        } => new(&template, tiles, duration, &media, &output)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_paths_are_relative_to_the_output() {
        let rel = |p: &str, d: &str| relative(Path::new(p), Path::new(d)).expect("utf-8");
        assert_eq!(rel("/a/b/c.mp4", "/a/b"), "c.mp4");
        assert_eq!(rel("/a/b/c.mp4", "/a/out"), "../b/c.mp4");
        assert_eq!(rel("/a/b/c.mp4", "/x/y"), "../../a/b/c.mp4");
        assert_eq!(rel("/a/./b/../c.mp4", "/a/d/."), "../c.mp4");
        assert_eq!(rel("c.mp4", "."), "c.mp4");
        assert_eq!(rel("c.mp4", "out"), "../c.mp4");
    }
}
