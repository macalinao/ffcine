//! The libavfilter escape hatch (`docs/compositing.md#libavfilter-escape-hatch`): a raw
//! libavfilter graph run over a layer's frames in its prep, before the GPU pass.
//!
//! This module is the backend-neutral half: the node ([`Node`]), the statefulness table
//! ([`reach`], under [`FILTER_TABLE_REVISION`]) that says how many frames before and after a
//! piece a graph reads, the `n` lint, and the [`Prep`] a lowered layer carries for the renderer
//! to run (`ffcine-assembly` runs it through the `ffmpeg` CLI into a lossless intermediate).

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::extra::{self, Filter};

/// The statefulness table's revision, in the key of every stateful node: bump it when
/// [`reach`] changes, which re-renders stateful nodes only.
pub const FILTER_TABLE_REVISION: u32 = 1;

/// The largest pre-roll or post-roll, frames.
pub const MAX_ROLL: u32 = 10_000;

/// Where the graph's frames live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hw {
    /// Software frames (the default).
    #[default]
    Cpu,
    /// Uploaded to CUDA for `*_cuda` filters, downloaded after.
    Cuda,
    /// Uploaded to Vulkan for `*_vulkan` filters, downloaded after.
    Vulkan,
}

impl Hw {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "cpu" => Self::Cpu,
            "cuda" => Self::Cuda,
            "vulkan" => Self::Vulkan,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
            Self::Vulkan => "vulkan",
        }
    }
}

/// Which time the graph's frames are stamped with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clock {
    /// Layer time `lt` (0 at the layer's `in`): moving the layer changes nothing.
    #[default]
    Layer,
    /// Comp time `t`.
    Comp,
}

impl Clock {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "layer" => Self::Layer,
            "comp" => Self::Comp,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Layer => "layer",
            Self::Comp => "comp",
        }
    }
}

/// A raw libavfilter graph on a layer (`filter`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The graph text, verbatim. A graph of several chains (`;`) names its input `[in]` and
    /// its output `[out]`.
    pub graph: String,
    pub hw: Hw,
    pub clock: Clock,
    /// Overrides of the table's reach.
    pub preroll: Option<u32>,
    pub postroll: Option<u32>,
    pub whole_clip: Option<bool>,
    /// `timing: free`: the graph may change frame timing or count; the layer takes what comes
    /// (run over the whole clip, no guarantee across pieces).
    pub free_timing: bool,
    /// The files the graph reads ([`READS`]), absolute, in graph order: set by
    /// [`locate`](Self::locate).
    pub files: Vec<PathBuf>,
    /// The graph as ffmpeg runs it: `graph` with each file it reads made absolute.
    pub run: String,
}

/// How far a graph reads around the frames it outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reach {
    /// Frames before a piece the graph needs to have seen.
    pub preroll: u32,
    /// Frames after a piece.
    pub postroll: u32,
    /// Only the whole layer in one run matches an unsplit render.
    pub whole_clip: bool,
}

impl Reach {
    /// Whether any frame's output depends on another frame.
    pub fn stateful(self) -> bool {
        self.preroll > 0 || self.postroll > 0 || self.whole_clip
    }
}

/// The filters of a graph, link labels dropped.
///
/// # Errors
/// Text that isn't a filter graph.
pub fn parse(graph: &str) -> Result<Vec<Filter>, String> {
    let mut out = Vec::new();
    for chain in extra::split(graph, ';') {
        out.extend(extra::parse(&chain)?);
    }
    if out.is_empty() {
        return Err("an empty graph".into());
    }
    Ok(out)
}

/// A number option of `f` (a constant expression), or `max` if it isn't one (generous: extra
/// frames never change the output), or `default` if absent.
fn opt(f: &Filter, names: &[&str], pos: Option<usize>, default: u32, max: u32) -> u32 {
    match f.get(names, pos) {
        None => default,
        Some(v) => extra::eval(v)
            .filter(|x| x.is_finite() && *x >= 0.0)
            .map_or(max, |x| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let x = x.ceil().min(f64::from(max)) as u32;
                x
            }),
    }
}

/// Whether filter `name` changes frame timing or count (`minterpolate`, `framerate`,
/// `mpdecimate`, `decimate`, `pullup`).
pub fn changes_timing(name: &str) -> bool {
    matches!(
        name,
        "minterpolate" | "framerate" | "mpdecimate" | "decimate" | "pullup"
    )
}

/// An option naming a file: the filter, the option's names, its positional index.
type FileOpt = (&'static str, &'static [&'static str], Option<usize>);

/// The options through which a graph may read files: each is resolved against the comp's
/// directory and its file's contents keyed. Other file reads are refused ([`WRITES`] and
/// [`REFUSED`]) or not known to the table (a graph naming a file elsewhere reads it relative to
/// the renderer's working directory, unkeyed: don't).
pub const READS: &[FileOpt] = &[
    ("lut3d", &["file"], Some(0)),
    ("lut1d", &["file"], Some(0)),
    ("subtitles", &["filename", "f"], Some(0)),
    ("ass", &["filename", "f"], Some(0)),
    ("drawtext", &["fontfile"], Some(0)),
    ("drawtext", &["textfile"], None),
    ("vidstabtransform", &["input"], Some(0)),
    ("curves", &["psfile"], None),
    ("removelogo", &["filename", "f"], Some(0)),
    ("find_rect", &["object"], Some(0)),
    ("cover_rect", &["cover"], Some(0)),
    ("dnn_processing", &["model"], None),
    ("dnn_detect", &["model", "labels"], None),
    ("dnn_classify", &["model", "labels"], None),
    ("sr", &["model"], None),
    ("derain", &["model"], None),
    ("libplacebo", &["custom_shader_path"], None),
];

/// Options that write files (or read a directory, which can't be keyed): refused.
pub const WRITES: &[FileOpt] = &[
    ("metadata", &["file"], None),
    ("ametadata", &["file"], None),
    ("psnr", &["stats_file", "f"], Some(0)),
    ("ssim", &["stats_file", "f"], Some(0)),
    ("xpsnr", &["stats_file", "f"], Some(0)),
    ("vmafmotion", &["stats_file"], None),
    ("libvmaf", &["log_path"], None),
    ("signature", &["filename"], None),
    ("curves", &["plot"], None),
    ("subtitles", &["fontsdir"], None),
    ("ass", &["fontsdir"], None),
];

/// Filters refused outright: sources that open arbitrary inputs (`movie`), command feeds
/// (`sendcmd`, `zmq`), writers (`vidstabdetect` writes its transforms file even unasked) and
/// plugin loaders (`frei0r`).
pub const REFUSED: &[&str] = &[
    "movie",
    "amovie",
    "sendcmd",
    "asendcmd",
    "zmq",
    "azmq",
    "vidstabdetect",
    "frei0r",
    "frei0r_src",
];

/// The indices into `f.args` of the options `table` lists for it.
fn file_args(f: &Filter, table: &[FileOpt]) -> Vec<usize> {
    let mut out = Vec::new();
    for (name, keys, pos) in table {
        if *name != f.name {
            continue;
        }
        let named = f
            .args
            .iter()
            .position(|(k, _)| k.as_deref().is_some_and(|k| keys.contains(&k)));
        let positional = || {
            pos.and_then(|p| {
                f.args
                    .iter()
                    .enumerate()
                    .filter(|(_, (k, _))| k.is_none())
                    .nth(p)
                    .map(|(i, _)| i)
            })
        };
        if let Some(i) = named.or_else(positional)
            && !out.contains(&i)
        {
            out.push(i);
        }
    }
    out.sort_unstable();
    out
}

/// Escapes `chars` (and `\`) in `s` with backslashes, one level of ffmpeg's quoting.
fn escape(s: &str, chars: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || chars.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A path as a value in graph text: escaped for the option parser, then for the graph parser.
fn graph_value(p: &Path) -> String {
    escape(&escape(&p.to_string_lossy(), "':="), "'[],;")
}

/// `graph` with each [`READS`] value replaced by `with(value)` (already graph text), and those
/// values, in order. The rest of the text is kept byte for byte.
fn rewrite(
    graph: &str,
    mut with: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut chains = Vec::new();
    for chain in extra::split(graph, ';') {
        let mut segs = Vec::new();
        for seg in extra::split(&chain, ',') {
            let filters = extra::parse(&seg)?;
            let Some(f) = filters.first() else {
                segs.push(seg);
                continue;
            };
            let idx = file_args(f, READS);
            let Some(eq) = seg.find('=').filter(|_| !idx.is_empty()) else {
                segs.push(seg);
                continue;
            };
            // The arguments as `extra::parse` splits them, trailing labels kept aside.
            let rest = &seg[eq + 1..];
            let mut args = rest.trim_end();
            while args.ends_with(']') {
                args = args.rsplit_once('[').map_or("", |(l, _)| l).trim_end();
            }
            let tail = &rest[args.len()..];
            let mut parts = extra::split(args, ':');
            for i in idx {
                let (k, v) = &f.args[i];
                let v = with(v)?;
                parts[i] = match k {
                    Some(k) => format!("{k}={v}"),
                    None => v,
                };
            }
            segs.push(format!("{}={}{tail}", &seg[..eq], parts.join(":")));
        }
        chains.push(segs.join(","));
    }
    Ok(chains.join(";"))
}

/// One filter's reach, from the statefulness table (unknown filters are stateless).
pub fn reach(f: &Filter) -> Reach {
    let w = |pre, post| Reach {
        preroll: pre,
        postroll: post,
        whole_clip: false,
    };
    match f.name.as_str() {
        "tmix" => w(opt(f, &["frames"], Some(0), 3, 1024).saturating_sub(1), 0),
        "tblend" | "scdet" => w(1, 0),
        "deflicker" => {
            let s = opt(f, &["size", "s"], Some(0), 5, 129);
            w(s, s)
        }
        "atadenoise" => {
            let s = opt(f, &["s"], None, 9, 99).div_ceil(2);
            w(s, s)
        }
        "tmedian" => {
            let r = opt(f, &["radius"], Some(0), 1, 127);
            w(r, r)
        }
        "tmidequalizer" => {
            let r = opt(f, &["radius"], Some(0), 5, 127);
            w(r, r)
        }
        "fftdnoiz" => w(opt(f, &["prev"], None, 0, 1), opt(f, &["next"], None, 0, 1)),
        "yadif" | "bwdif" | "w3fdif" | "yadif_cuda" | "bwdif_cuda" | "bwdif_vulkan" => w(1, 1),
        "fieldmatch" => w(2, 2),
        "lagfun" | "hqdn3d" | "vidstabtransform" | "deshake" | "normalize" | "photosensitivity"
        | "freezedetect" => Reach {
            whole_clip: true,
            ..Reach::default()
        },
        n if changes_timing(n) => Reach {
            whole_clip: true,
            ..Reach::default()
        },
        _ => Reach::default(),
    }
}

/// Whether option values of filter `f` read state that restarts in every piece: the frame
/// counter (`n`, `N` as `setpts` and `select` spell it, drawtext's `%{n}` and `%{frame_num}`), or
/// `select`'s memory of earlier frames (`prev_*`, `scene`).
fn uses_n(f: &Filter) -> bool {
    let select = matches!(f.name.as_str(), "select" | "aselect");
    f.args.iter().any(|(_, v)| {
        v.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|t| {
                t == "n"
                    || t == "N"
                    || t == "frame_num"
                    || (select && (t == "scene" || t.starts_with("prev_")))
            })
    })
}

/// Whether `v` names something by URL (`scheme://`).
fn is_url(v: &str) -> bool {
    v.split_once("://").is_some_and(|(s, _)| {
        let s = s
            .rsplit(|c: char| !(c.is_ascii_alphanumeric() || "+-.".contains(c)))
            .next();
        s.is_some_and(|s| !s.is_empty())
    })
}

/// Refuses filters and options that write files, open arbitrary inputs or name URLs
/// (`docs/compositing.md#libavfilter-escape-hatch`).
fn policy(f: &Filter) -> Result<(), String> {
    if REFUSED.contains(&f.name.as_str()) {
        return Err(format!(
            "{}: not allowed in a filter node (it opens inputs or writes files outside the \
             render); file reads go through the options listed in docs/compositing.md",
            f.name
        ));
    }
    if let Some(&i) = file_args(f, WRITES).first() {
        let k = f.args[i].0.as_deref().unwrap_or("its first argument");
        return Err(format!(
            "{}: {k} writes (or reads a directory of) files outside the render: not allowed in \
             a filter node",
            f.name
        ));
    }
    // drawtext's text is drawn, never opened. Values joined too: an unescaped `scheme://`
    // splits at its colon.
    let vals: Vec<(Option<&str>, &str)> = f
        .args
        .iter()
        .filter(|(k, _)| !(f.name == "drawtext" && k.as_deref() == Some("text")))
        .map(|(k, v)| (k.as_deref(), v.as_str()))
        .collect();
    let joined = vals.iter().map(|(_, v)| *v).collect::<Vec<_>>().join(":");
    if let Some((k, _)) = vals.iter().find(|(_, v)| is_url(v)) {
        return Err(format!(
            "{}: {} names a URL: a filter node reads local files only",
            f.name,
            k.unwrap_or("an argument")
        ));
    }
    if is_url(&joined) {
        return Err(format!(
            "{}: names a URL: a filter node reads local files only",
            f.name
        ));
    }
    Ok(())
}

impl Node {
    /// The node's reach: the table's over its filters (summed, generously), overridden by its
    /// own `preroll`, `postroll` and `whole_clip`.
    ///
    /// # Errors
    /// A graph that doesn't parse, a timing-changing filter without `timing: free`, or a
    /// frame counter (`n`) outside a whole-clip node.
    pub fn resolve(&self) -> Result<Reach, String> {
        let filters = parse(&self.graph)?;
        if self.graph.contains(';')
            && !(self.graph.contains("[in]") && self.graph.contains("[out]"))
        {
            return Err("a graph of several chains names its input [in] and output [out]".into());
        }
        let mut r = Reach::default();
        for f in &filters {
            policy(f)?;
            if changes_timing(&f.name) && !self.free_timing {
                return Err(format!(
                    "{} changes frame timing: set \"timing\": \"free\" to take whatever it \
                     outputs (run over the whole clip)",
                    f.name
                ));
            }
            let x = reach(f);
            r.preroll = r.preroll.saturating_add(x.preroll).min(MAX_ROLL);
            r.postroll = r.postroll.saturating_add(x.postroll).min(MAX_ROLL);
            r.whole_clip |= x.whole_clip;
        }
        r.whole_clip |= self.free_timing;
        if let Some(p) = self.preroll {
            r.preroll = p;
        }
        if let Some(p) = self.postroll {
            r.postroll = p;
        }
        if let Some(w) = self.whole_clip {
            r.whole_clip = w;
        }
        if !r.whole_clip
            && let Some(f) = filters.iter().find(|f| uses_n(f))
        {
            return Err(format!(
                "{}: the frame counter n (and select's prev_*/scene) restarts in every piece; \
                 use t*FRAME_RATE (the node's clock) or \"whole_clip\": true",
                f.name
            ));
        }
        Ok(r)
    }

    /// Resolves the files the graph reads ([`READS`]) against `dir` (the comp file's
    /// directory), into [`files`](Self::files) and [`run`](Self::run).
    ///
    /// # Errors
    /// A graph that doesn't parse, or a file that isn't there.
    pub fn locate(&mut self, dir: &Path) -> Result<(), String> {
        let mut files = Vec::new();
        if parse(&self.graph)?
            .iter()
            .any(|f| f.name == "vidstabtransform" && file_args(f, READS).is_empty())
        {
            return Err(
                "vidstabtransform: name its transforms file with input= (resolved \
                        against the comp's directory)"
                    .into(),
            );
        }
        let run = rewrite(&self.graph, |v| {
            let p = dir.join(v.trim());
            if !p.is_file() {
                return Err(format!("{}: no such file", p.display()));
            }
            let p = std::path::absolute(&p).unwrap_or(p);
            let g = graph_value(&p);
            files.push(p);
            Ok(g)
        })?;
        self.files = files;
        self.run = run;
        Ok(())
    }
}

/// A filter node's run for one piece of a layer, for the renderer to make before the GPU pass:
/// feed `map`'s source frames at `rate`, stamped from `pts`, through `graph`; keep its outputs
/// `store` in a lossless intermediate named by `key`; show the intermediate's frames `show`.
#[derive(Debug, Clone, PartialEq)]
pub struct Prep {
    pub graph: String,
    pub hw: Hw,
    pub free_timing: bool,
    /// The source video.
    pub input: PathBuf,
    /// Its display size (upright), px: the graph must keep it.
    pub dims: (u32, u32),
    /// Where decoding starts (half a frame before the first frame `map` reads).
    pub start: Duration,
    /// The source frame of each fed frame, counted from the first frame at or after `start`.
    pub map: Arc<[i64]>,
    /// The fed (canvas) frame rate.
    pub rate: f64,
    /// The first fed frame's index on the node's clock (its PTS in frames).
    pub pts: i64,
    /// The graph's outputs kept (pre-roll and post-roll dropped; all of them for a whole-clip
    /// node, whose intermediate every piece shares).
    pub store: Range<usize>,
    /// The intermediate's frames the piece shows.
    pub show: Range<usize>,
    /// The intermediate's name: a digest of the node's recipe and `store`.
    pub key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(g: &str) -> Node {
        Node {
            graph: g.into(),
            hw: Hw::Cpu,
            clock: Clock::Layer,
            preroll: None,
            postroll: None,
            whole_clip: None,
            free_timing: false,
            files: Vec::new(),
            run: g.into(),
        }
    }

    #[test]
    fn table() {
        let r = |g: &str| node(g).resolve().unwrap();
        assert_eq!(r("eq=gamma=1.1"), Reach::default());
        assert!(!r("eq=gamma=1.1,hue=s=0").stateful());
        assert_eq!(r("tmix=frames=5").preroll, 4);
        assert_eq!(r("tmix=5").preroll, 4);
        assert_eq!(r("tmix").preroll, 2);
        assert_eq!(r("tmix=frames=nope").preroll, 1023);
        let d = r("deflicker=size=7,eq=gamma=1.05");
        assert_eq!((d.preroll, d.postroll, d.whole_clip), (7, 7, false));
        assert_eq!(r("deflicker").postroll, 5);
        assert_eq!(r("atadenoise").preroll, 5);
        assert_eq!(r("tmedian=radius=3").postroll, 3);
        assert_eq!(r("fftdnoiz=prev=1").preroll, 1);
        assert_eq!(r("yadif,tblend").preroll, 2);
        assert!(r("hqdn3d").whole_clip);
        assert!(r("lagfun=decay=0.9").whole_clip);
        // Overrides.
        let mut n = node("tmix=frames=5");
        n.preroll = Some(10);
        n.postroll = Some(2);
        n.whole_clip = Some(true);
        assert_eq!(
            n.resolve().unwrap(),
            Reach {
                preroll: 10,
                postroll: 2,
                whole_clip: true
            }
        );
    }

    #[test]
    fn timing_and_n() {
        let e = node("minterpolate=fps=60").resolve().unwrap_err();
        assert!(e.contains("timing"), "{e}");
        let mut n = node("minterpolate=fps=60");
        n.free_timing = true;
        assert!(n.resolve().unwrap().whole_clip);
        let e = node("eq=eval=frame:brightness='0.01*n'")
            .resolve()
            .unwrap_err();
        assert!(e.contains("t*FRAME_RATE"), "{e}");
        assert!(node("setpts=N/30/TB").resolve().is_err());
        assert!(
            node("eq=eval=frame:brightness='0.1*sin(t)'")
                .resolve()
                .is_ok()
        );
        let mut n = node("eq=eval=frame:brightness='0.01*n'");
        n.whole_clip = Some(true);
        assert!(n.resolve().is_ok());
        assert!(node("split[a][b];[a][b]blend").resolve().is_err());
        assert!(
            node("[in]split[a][b];[a][b]blend=all_mode=average[out]")
                .resolve()
                .is_ok()
        );
        assert!(node("").resolve().is_err());
    }

    #[test]
    fn policy_refuses_writers_sources_and_urls() {
        let e = |g: &str| node(g).resolve().unwrap_err();
        assert!(e("movie=a.mov[m];[in][m]overlay[out]").contains("not allowed"));
        assert!(e("[in]eq,movie=b.mkv[out]").contains("movie"));
        assert!(e("metadata=mode=print:file=log.txt").contains("file writes"));
        assert!(e("ssim=stats.log").contains("its first argument"));
        assert!(e("vidstabdetect=result=t.trf").contains("not allowed"));
        assert!(e("sendcmd=f=cmds.txt,eq").contains("not allowed"));
        assert!(e("curves=preset=vintage:plot=p.json").contains("plot"));
        assert!(e("lut3d=file=https\\://example.com/a.cube").contains("URL"));
        assert!(e("subtitles=http://example.com/a.srt").contains("URL"));
        assert!(e("scdet,select='gt(scene,0.3)'").contains("prev_*/scene"));
        assert!(e("drawtext=text='%{frame_num}'").contains("restarts"));
        assert!(
            node("drawtext=text='see https\\://example.com'")
                .resolve()
                .is_ok()
        );
        assert!(node("metadata=mode=print").resolve().is_ok());
        assert_eq!(node("scdet").resolve().unwrap().preroll, 1);
        assert!(node("freezedetect").resolve().unwrap().whole_clip);
        assert_eq!(node("fieldmatch").resolve().unwrap().postroll, 2);
        assert!(e("pullup").contains("timing"));
    }

    #[test]
    fn locate_makes_reads_absolute() {
        let dir = std::env::temp_dir().join(format!("ffcine-locate-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("my luts")).unwrap();
        std::fs::write(dir.join("my luts/a,b.cube"), "LUT_3D_SIZE 2\n").unwrap();
        std::fs::write(dir.join("t.txt"), "hi").unwrap();
        let mut n = node(
            "[in]lut3d='my luts/a,b.cube':interp=tetrahedral[x];[x]drawtext=textfile=t.txt:x=1[out]",
        );
        n.locate(&dir).unwrap();
        assert_eq!(n.files, [dir.join("my luts/a,b.cube"), dir.join("t.txt")]);
        let d = escape(&escape(&dir.to_string_lossy(), "':="), "'[],;");
        assert_eq!(
            n.run,
            format!(
                "[in]lut3d={d}/my luts/a\\,b.cube:interp=tetrahedral[x];\
                 [x]drawtext=textfile={d}/t.txt:x=1[out]"
            )
        );
        // Unchanged without reads.
        let mut n = node("eq=gamma=1.1, hue=s=0");
        n.locate(&dir).unwrap();
        assert_eq!(n.run, "eq=gamma=1.1, hue=s=0");
        let e = node("lut3d=missing.cube").locate(&dir).unwrap_err();
        assert!(e.contains("no such file"), "{e}");
        let e = node("vidstabtransform=smoothing=10")
            .locate(&dir)
            .unwrap_err();
        assert!(e.contains("input="), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
