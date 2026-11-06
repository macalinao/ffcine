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
    /// The graph text, verbatim. A graph of several chains (`;`), or one with link labels,
    /// names its input `[in]` and its output `[out]`.
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
    /// The files the graph reads ([`ALLOWED`]), absolute, in graph order: set by
    /// [`locate`](Self::locate).
    pub files: Vec<PathBuf>,
    /// The graph as ffmpeg runs it: `graph` with each file it reads made absolute.
    pub run: String,
    /// Whether `run` names its ends `[in]` and `[out]` (several chains, or link labels), as the
    /// tokenizer reads it: set by [`locate`](Self::locate).
    pub labelled: bool,
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

/// ffmpeg's whitespace (`WHITESPACES`).
const WS: &[u8] = b" \n\t\r";

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| WS.contains(c)) {
        i += 1;
    }
    i
}

/// libavutil's `av_get_token`, from `text[*at..]`: skips leading whitespace and reads up to an
/// unquoted, unescaped byte of `term`, removing one level of `'...'` quoting and `\` escapes and
/// trimming trailing whitespace that isn't escaped or quoted. Stricter than ffmpeg, which takes
/// both literally: an unterminated quote or a trailing backslash is an error.
fn token(text: &[u8], at: &mut usize, term: &[u8]) -> Result<String, String> {
    let mut pos = skip_ws(text, *at);
    let (mut out, mut end) = (Vec::new(), 0);
    while pos < text.len() && !term.contains(&text[pos]) {
        let ch = text[pos];
        pos += 1;
        if ch == b'\\' {
            let Some(&next) = text.get(pos) else {
                return Err("a trailing backslash".into());
            };
            out.push(next);
            pos += 1;
            end = out.len();
        } else if ch == b'\'' {
            while pos < text.len() && text[pos] != b'\'' {
                out.push(text[pos]);
                pos += 1;
            }
            if pos == text.len() {
                return Err("an unterminated quote".into());
            }
            pos += 1;
            end = out.len();
        } else {
            out.push(ch);
        }
    }
    while out.len() > end && out.last().is_some_and(|ch| WS.contains(ch)) {
        out.pop();
    }
    *at = pos;
    String::from_utf8(out).map_err(|_| "not UTF-8".into())
}

/// A plain option or filter name: `[A-Za-z0-9_]+`.
fn ident(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// The second pass, libavfilter's `ff_filter_opt_parse` over a filter's options (already
/// through [`token`] once): `key=value` pairs split at `:`, each value unescaped and unquoted
/// again, positional values first. Stricter than ffmpeg (fails closed): a key must be a plain
/// name ([`ident`]; ffmpeg also takes `-`, `.` and `/`, and its CLI reads a `/key`'s value from a
/// file), a key may be set once, and a positional value may not hold `=`.
fn options(opts: &str) -> Result<Vec<(Option<String>, String)>, String> {
    let bytes = opts.as_bytes();
    let (mut at, mut out) = (0, Vec::<(Option<String>, String)>::new());
    while at < bytes.len() {
        // libavutil's `get_key`: key characters, whitespace, `=`.
        let ks = skip_ws(bytes, at);
        let mut pos = ks;
        while bytes
            .get(pos)
            .is_some_and(|&ch| ch.is_ascii_alphanumeric() || b"-_/.".contains(&ch))
        {
            pos += 1;
        }
        let ke = pos;
        let pos = skip_ws(bytes, pos);
        let key = (bytes.get(pos) == Some(&b'=')).then(|| {
            at = pos + 1;
            &opts[ks..ke]
        });
        let start = at;
        let val = token(bytes, &mut at, b":").map_err(|err| format!("{opts:?}: {err}"))?;
        if let Some(name) = key {
            if !ident(name) {
                return Err(format!("{name:?}: not a plain option name"));
            }
            if out.iter().any(|(other, _)| other.as_deref() == Some(name)) {
                return Err(format!("{name}: set twice"));
            }
        } else {
            if out.iter().any(|(name, _)| name.is_some()) {
                return Err(format!("{val:?}: a positional value after a named one"));
            }
            if bytes[start..at].contains(&b'=') {
                return Err(format!(
                    "{:?}: an `=` in a positional value (name the option with a plain name)",
                    &opts[start..at]
                ));
            }
        }
        out.push((key.map(str::to_owned), val));
        // The `:`.
        at += 1;
    }
    Ok(out)
}

/// One filter of a graph as libavfilter reads it.
#[derive(Debug, Clone, PartialEq)]
struct Parsed {
    /// Its name (`@instance` dropped) and options, both passes applied.
    filter: Filter,
    /// Where its options are in the graph text (after the `=`).
    opts: Option<Range<usize>>,
    /// Its chain's index.
    chain: usize,
    /// The link labels before it and after it.
    labels_in: Vec<String>,
    labels_out: Vec<String>,
}

/// Link labels (`[a][b]`), as libavfilter's `linklabels_parse` reads them.
fn labels(b: &[u8], i: &mut usize) -> Result<Vec<String>, String> {
    let mut v = Vec::new();
    while b.get(*i) == Some(&b'[') {
        *i += 1;
        let l = token(b, i, b"]")?;
        if l.is_empty() || b.get(*i) != Some(&b']') {
            return Err("an empty or unclosed link label".into());
        }
        v.push(l);
        *i = skip_ws(b, *i + 1);
    }
    Ok(v)
}

/// A graph's filters, as libavfilter's `avfilter_graph_segment_parse` splits it: chains at `;`,
/// filters at `,`, the name and the options token through [`token`], then the options through
/// [`options`].
fn parse_graph(graph: &str) -> Result<Vec<Parsed>, String> {
    let b = graph.as_bytes();
    let mut i = skip_ws(b, 0);
    if b[i..].starts_with(b"sws_flags=") {
        return Err("sws_flags: not allowed in a filter node".into());
    }
    let (mut out, mut chain) = (Vec::new(), 0);
    while i < b.len() {
        loop {
            let labels_in = labels(b, &mut i)?;
            let name = token(b, &mut i, b"=,;[")?;
            let name = name.split_once('@').map_or(name.as_str(), |(n, _)| n);
            if !ident(name) {
                return Err(format!("{name:?}: not a filter"));
            }
            let (mut opts, mut args) = (None, Vec::new());
            if b.get(i) == Some(&b'=') {
                i += 1;
                let start = i;
                let t = token(b, &mut i, b"[],;").map_err(|e| format!("{name}: {e}"))?;
                opts = Some(start..i);
                args = options(&t).map_err(|e| format!("{name}: {e}"))?;
            }
            let labels_out = labels(b, &mut i)?;
            i = skip_ws(b, i);
            out.push(Parsed {
                filter: Filter {
                    name: name.to_owned(),
                    args,
                },
                opts,
                chain,
                labels_in,
                labels_out,
            });
            match b.get(i) {
                None => break,
                Some(b',') => i = skip_ws(b, i + 1),
                Some(b';') => {
                    i = skip_ws(b, i + 1);
                    break;
                }
                Some(_) => {
                    return Err(format!("{:?}: trailing text after a filter", &graph[i..]));
                }
            }
            if i == b.len() {
                break;
            }
        }
        i = skip_ws(b, i);
        chain += 1;
    }
    if out.is_empty() {
        return Err("an empty graph".into());
    }
    Ok(out)
}

/// Whether a graph, read as libavfilter reads it, has several chains or link labels: then it
/// names its ends `[in]` and `[out]` and runs in the multi-chain wrapper.
fn is_labelled(parsed: &[Parsed]) -> bool {
    parsed.last().is_some_and(|p| p.chain > 0)
        || parsed
            .iter()
            .any(|p| !p.labels_in.is_empty() || !p.labels_out.is_empty())
}

/// Whether `graph` names its ends `[in]` and `[out]`: it has several chains or link labels, as
/// libavfilter reads it (a quoted or escaped `;` or `[` is text, not structure).
///
/// # Errors
/// Text that isn't a filter graph.
pub fn labelled(graph: &str) -> Result<bool, String> {
    Ok(is_labelled(&parse_graph(graph)?))
}

/// The filters of a graph, link labels dropped, read as libavfilter reads them.
///
/// # Errors
/// Text that isn't a filter graph.
pub fn parse(graph: &str) -> Result<Vec<Filter>, String> {
    Ok(parse_graph(graph)?.into_iter().map(|p| p.filter).collect())
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

/// What a filter does with the file an option names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// Reads it: resolved against the comp's directory, its contents keyed.
    Read,
    /// Reads the files under that directory, recursively (`fontsdir`): each one's contents keyed.
    ReadDir,
    /// Writes it: refused.
    Write,
}

/// An option naming a file: its names (aliases), its positional index (counting an option and
/// its aliases once, as libavfilter does), what the filter does with it.
pub type FileOpt = (&'static [&'static str], Option<usize>, Use);

/// The filters a filter node may run, with the options of each that name files: everything
/// else is refused at comp check. Listed are pixel filters with no file, URL, device or plugin
/// options (`ffmpeg -h filter=NAME`), plus the readers whose files are keyed. Sources (`movie`),
/// command feeds (`sendcmd`, `zmq`), writers (`vidstabdetect`, `deshake`, `psnr`, `ssim*`,
/// `metadata`, `paletteuse`), and plugin or code loaders (`frei0r`, `ladspa`, `lv2`, `dnn_*`,
/// `libvmaf`, `libplacebo`, `program_opencl`, ...) aren't, so they're refused.
pub const ALLOWED: &[(&str, &[FileOpt])] = &[
    ("amplify", &[]),
    (
        "ass",
        &[
            (&["filename", "f"], Some(0), Use::Read),
            (&["fontsdir"], Some(2), Use::ReadDir),
        ],
    ),
    ("atadenoise", &[]),
    ("avgblur", &[]),
    ("avgblur_vulkan", &[]),
    ("backgroundkey", &[]),
    ("bilateral", &[]),
    ("bilateral_cuda", &[]),
    ("blend", &[]),
    ("blend_vulkan", &[]),
    ("bm3d", &[]),
    ("boxblur", &[]),
    ("bwdif", &[]),
    ("bwdif_cuda", &[]),
    ("bwdif_vulkan", &[]),
    ("cas", &[]),
    ("chromaber_vulkan", &[]),
    ("chromahold", &[]),
    ("chromakey", &[]),
    ("chromakey_cuda", &[]),
    ("chromashift", &[]),
    ("colorbalance", &[]),
    ("colorchannelmixer", &[]),
    ("colorcontrast", &[]),
    ("colorcorrect", &[]),
    ("colorhold", &[]),
    ("colorize", &[]),
    ("colorkey", &[]),
    ("colorlevels", &[]),
    ("colormatrix", &[]),
    ("colorspace", &[]),
    ("colorspace_cuda", &[]),
    ("colortemperature", &[]),
    ("convolution", &[]),
    ("copy", &[]),
    ("crop", &[]),
    (
        "curves",
        &[
            (&["psfile"], Some(6), Use::Read),
            (&["plot"], Some(7), Use::Write),
        ],
    ),
    ("dblur", &[]),
    ("dctdnoiz", &[]),
    ("deband", &[]),
    ("deblock", &[]),
    ("decimate", &[]),
    ("deflate", &[]),
    ("deflicker", &[]),
    ("despill", &[]),
    ("dilation", &[]),
    ("drawbox", &[]),
    ("drawgrid", &[]),
    (
        "drawtext",
        &[
            (&["fontfile"], Some(0), Use::Read),
            (&["textfile"], Some(2), Use::Read),
        ],
    ),
    ("edgedetect", &[]),
    ("eq", &[]),
    ("erosion", &[]),
    ("estdif", &[]),
    ("exposure", &[]),
    ("extractplanes", &[]),
    ("fade", &[]),
    ("fftdnoiz", &[]),
    ("fftfilt", &[]),
    ("fieldmatch", &[]),
    ("fillborders", &[]),
    ("flip_vulkan", &[]),
    ("format", &[]),
    ("framerate", &[]),
    ("freezedetect", &[]),
    ("fspp", &[]),
    ("gblur", &[]),
    ("gblur_vulkan", &[]),
    ("geq", &[]),
    ("gradfun", &[]),
    ("grayworld", &[]),
    ("guided", &[]),
    ("haldclut", &[]),
    ("hflip", &[]),
    ("hflip_vulkan", &[]),
    ("histeq", &[]),
    ("hqdn3d", &[]),
    ("hsvhold", &[]),
    ("hsvkey", &[]),
    ("hue", &[]),
    ("huesaturation", &[]),
    ("hwdownload", &[]),
    ("hwupload", &[]),
    ("hwupload_cuda", &[]),
    ("inflate", &[]),
    ("kerndeint", &[]),
    ("kirsch", &[]),
    ("lagfun", &[]),
    ("lenscorrection", &[]),
    ("limiter", &[]),
    ("lumakey", &[]),
    ("lut", &[]),
    ("lut1d", &[(&["file"], Some(0), Use::Read)]),
    ("lut2", &[]),
    ("lut3d", &[(&["file"], Some(0), Use::Read)]),
    ("lutrgb", &[]),
    ("lutyuv", &[]),
    ("maskedmerge", &[]),
    ("maskfun", &[]),
    ("median", &[]),
    ("mergeplanes", &[]),
    ("minterpolate", &[]),
    ("monochrome", &[]),
    ("mpdecimate", &[]),
    ("negate", &[]),
    ("nlmeans", &[]),
    ("nlmeans_vulkan", &[]),
    ("noise", &[]),
    ("normalize", &[]),
    ("null", &[]),
    ("overlay", &[]),
    ("overlay_cuda", &[]),
    ("overlay_vulkan", &[]),
    ("owdenoise", &[]),
    ("pad", &[]),
    ("perspective", &[]),
    ("photosensitivity", &[]),
    ("pp7", &[]),
    ("prewitt", &[]),
    ("pseudocolor", &[]),
    ("pullup", &[]),
    ("removegrain", &[]),
    ("removelogo", &[(&["filename", "f"], Some(0), Use::Read)]),
    ("rgbashift", &[]),
    ("roberts", &[]),
    ("rotate", &[]),
    ("sab", &[]),
    ("scale", &[]),
    ("scale_cuda", &[]),
    ("scale_vulkan", &[]),
    ("scdet", &[]),
    ("scharr", &[]),
    ("selectivecolor", &[(&["psfile"], Some(10), Use::Read)]),
    ("setdar", &[]),
    ("setparams", &[]),
    ("setrange", &[]),
    ("setsar", &[]),
    ("shuffleplanes", &[]),
    ("smartblur", &[]),
    ("sobel", &[]),
    ("split", &[]),
    ("spp", &[]),
    (
        "subtitles",
        &[
            (&["filename", "f"], Some(0), Use::Read),
            (&["fontsdir"], Some(2), Use::ReadDir),
        ],
    ),
    ("swapuv", &[]),
    ("tblend", &[]),
    ("threshold", &[]),
    ("tlut2", &[]),
    ("tmedian", &[]),
    ("tmidequalizer", &[]),
    ("tmix", &[]),
    ("tonemap", &[]),
    ("transpose", &[]),
    ("transpose_vulkan", &[]),
    ("unsharp", &[]),
    ("vaguedenoiser", &[]),
    ("varblur", &[]),
    ("vflip", &[]),
    ("vflip_vulkan", &[]),
    ("vibrance", &[]),
    ("vidstabtransform", &[(&["input"], Some(0), Use::Read)]),
    ("vignette", &[]),
    ("w3fdif", &[]),
    ("yadif", &[]),
    ("yadif_cuda", &[]),
    ("zscale", &[]),
];

/// The table's file options of `f` that it sets: their index into `f.args` and use, in order.
///
/// # Errors
/// One set twice (named and positional, or through two aliases).
fn file_args(f: &Filter) -> Result<Vec<(usize, Use)>, String> {
    let Some((_, opts)) = ALLOWED.iter().find(|(n, _)| *n == f.name) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (keys, pos, u) in *opts {
        let mut at = f
            .args
            .iter()
            .enumerate()
            .filter(|(_, (k, _))| k.as_deref().is_some_and(|k| keys.contains(&k)))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        if let Some(p) = pos
            && f.args.get(*p).is_some_and(|(k, _)| k.is_none())
        {
            at.push(*p);
        }
        match at[..] {
            [] => {}
            [i] => out.push((i, *u)),
            _ => return Err(format!("{}: {} set twice", f.name, keys[0])),
        }
    }
    out.sort_unstable_by_key(|(i, _)| *i);
    Ok(out)
}

/// Escapes `chars`, `\` and whitespace at either end of `s` with backslashes: one level of
/// ffmpeg's quoting, which [`token`] undoes.
fn escape(s: &str, chars: &str) -> String {
    let n = s.chars().count();
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        if c == '\\' || chars.contains(c) || ((i == 0 || i + 1 == n) && " \n\t\r".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `graph` with each file option's value `v` replaced by `with(v)` (a plain value, escaped
/// here). Filters without one keep their text byte for byte; those with one get their options
/// rewritten, every value escaped for both passes. Checked by reading the result back.
fn rewrite(
    graph: &str,
    mut with: impl FnMut(&Filter, &str, Use) -> Result<String, String>,
) -> Result<String, String> {
    let parsed = parse_graph(graph)?;
    let (mut out, mut want, mut last) = (String::new(), Vec::new(), 0);
    for p in parsed {
        let mut f = p.filter;
        let idx = file_args(&f)?;
        if let Some(r) = p.opts.filter(|_| !idx.is_empty()) {
            for (i, u) in idx {
                let v = with(&f, &f.args[i].1, u)?;
                f.args[i].1 = v;
            }
            let text = f
                .args
                .iter()
                .map(|(k, v)| {
                    let v = escape(v, "':=");
                    k.as_ref().map_or(v.clone(), |k| format!("{k}={v}"))
                })
                .collect::<Vec<_>>()
                .join(":");
            out.push_str(&graph[last..r.start]);
            out.push_str(&escape(&text, "'[],;"));
            last = r.end;
        }
        want.push(f);
    }
    out.push_str(&graph[last..]);
    if parse(&out)? != want {
        return Err("the graph's file paths can't be written back into it".into());
    }
    Ok(out)
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

/// Refuses filters not on the allowlist ([`ALLOWED`]), options that write files, and values
/// naming URLs (`docs/compositing.md#libavfilter-escape-hatch`).
fn policy(f: &Filter) -> Result<(), String> {
    if !ALLOWED.iter().any(|(n, _)| *n == f.name) {
        return Err(format!(
            "{}: not on the filter node's allowlist (pixel filters, and readers whose files are \
             keyed; see docs/compositing.md#libavfilter-escape-hatch)",
            f.name
        ));
    }
    if let Some((i, _)) = file_args(f)?.into_iter().find(|(_, u)| *u == Use::Write) {
        let k = f.args[i].0.as_deref().unwrap_or("its positional argument");
        return Err(format!(
            "{}: {k} writes a file outside the render: not allowed in a filter node",
            f.name
        ));
    }
    // drawtext's text is drawn, never opened.
    if let Some((k, _)) = f
        .args
        .iter()
        .filter(|(k, _)| !(f.name == "drawtext" && k.as_deref() == Some("text")))
        .find(|(_, v)| is_url(v))
    {
        return Err(format!(
            "{}: {} names a URL: a filter node reads local files only",
            f.name,
            k.as_deref().unwrap_or("an argument")
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
        let parsed = parse_graph(&self.graph)?;
        if is_labelled(&parsed)
            && !(parsed.iter().any(|p| p.labels_in.iter().any(|l| l == "in"))
                && parsed
                    .iter()
                    .any(|p| p.labels_out.iter().any(|l| l == "out")))
        {
            return Err(
                "a graph of several chains, or with link labels, names its input [in] and \
                 output [out]"
                    .into(),
            );
        }
        let filters: Vec<Filter> = parsed.into_iter().map(|p| p.filter).collect();
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

    /// Resolves the files the graph reads ([`ALLOWED`]) against `dir` (the comp file's
    /// directory), into [`files`](Self::files) and [`run`](Self::run).
    ///
    /// # Errors
    /// A graph that doesn't parse, or a file that isn't there.
    pub fn locate(&mut self, dir: &Path) -> Result<(), String> {
        for f in parse(&self.graph)? {
            if f.name == "vidstabtransform" && file_args(&f)?.is_empty() {
                return Err(
                    "vidstabtransform: name its transforms file with input= (resolved \
                            against the comp's directory)"
                        .into(),
                );
            }
        }
        let mut files = Vec::new();
        let run = rewrite(&self.graph, |f, v, u| {
            let p = dir.join(v.trim());
            let p = std::path::absolute(&p).unwrap_or(p);
            match u {
                Use::Read if p.is_file() => files.push(p.clone()),
                Use::ReadDir if p.is_dir() => {
                    files.extend(tree(&p).map_err(|e| format!("{}: {e}", f.name))?);
                }
                Use::Read => return Err(format!("{}: {}: no such file", f.name, p.display())),
                Use::ReadDir => {
                    return Err(format!("{}: {}: no such directory", f.name, p.display()));
                }
                Use::Write => return Err(format!("{}: writes files", f.name)),
            }
            p.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{}: {}: not UTF-8", f.name, p.display()))
        })?;
        self.labelled = is_labelled(&parse_graph(&run)?);
        self.files = files;
        self.run = run;
        Ok(())
    }
}

/// Files under a directory a filter reads (`fontsdir`), at most.
const MAX_TREE_FILES: usize = 4096;
/// Their total size, at most (bytes).
const MAX_TREE_BYTES: u64 = 1 << 30;

/// The files under `dir`, recursively (libass and fontconfig search subdirectories), sorted,
/// files only (links followed).
///
/// # Errors
/// An unreadable directory, or more than [`MAX_TREE_FILES`] files or [`MAX_TREE_BYTES`] bytes.
fn tree(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let (mut out, mut todo, mut bytes, mut dirs) = (Vec::new(), vec![dir.to_owned()], 0u64, 0);
    while let Some(d) = todo.pop() {
        // Counts directories too: a link loop ends here.
        dirs += 1;
        if dirs > MAX_TREE_FILES {
            return Err(format!(
                "{}: more than {MAX_TREE_FILES} directories",
                dir.display()
            ));
        }
        let rd = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        for e in rd {
            let p = e.map_err(|e| format!("{}: {e}", d.display()))?.path();
            let m = std::fs::metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            if m.is_dir() {
                todo.push(p);
            } else if m.is_file() {
                bytes += m.len();
                out.push(p);
                if out.len() > MAX_TREE_FILES || bytes > MAX_TREE_BYTES {
                    return Err(format!(
                        "{}: more than {MAX_TREE_FILES} files or {} MiB: point it at a \
                         smaller directory",
                        dir.display(),
                        MAX_TREE_BYTES >> 20
                    ));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A filter node's run for one piece of a layer, for the renderer to make before the GPU pass:
/// feed `map`'s source frames at `rate`, stamped from `pts`, through `graph`; keep its outputs
/// `store` in a lossless intermediate named by `key`; show the intermediate's frames `show`.
#[derive(Debug, Clone, PartialEq)]
pub struct Prep {
    pub graph: String,
    /// The graph names its ends `[in]` and `[out]` ([`Node::labelled`]).
    pub labelled: bool,
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
            labelled: labelled(g).unwrap_or_default(),
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
    fn chains_and_labels_come_from_the_tokenizer() {
        for g in [
            "drawtext=text='a;b'",
            "drawtext=text='[in] [out]'",
            "drawtext=text=a\\;b",
            "eq=gamma=1.1,drawtext=text='x;[y]'",
        ] {
            assert!(!labelled(g).unwrap(), "{g}");
            node(g).resolve().unwrap_or_else(|e| panic!("{g}: {e}"));
        }
        for g in [
            "[in]split[a][b];[a][b]blend=all_mode=average[out]",
            "[in]eq[out]",
        ] {
            assert!(labelled(g).unwrap(), "{g}");
            node(g).resolve().unwrap_or_else(|e| panic!("{g}: {e}"));
        }
        // Quoted labels aren't labels.
        let e = node("split[a][b];[a][b]blend,drawtext=text='[in] [out]'")
            .resolve()
            .unwrap_err();
        assert!(e.contains("[in]"), "{e}");
        assert!(node("[x]eq[out]").resolve().is_err());
    }

    #[test]
    fn policy_refuses_writers_sources_and_urls() {
        let e = |g: &str| {
            node(g)
                .resolve()
                .err()
                .unwrap_or_else(|| panic!("{g}: accepted"))
        };
        let ok = |g: &str| {
            node(g).resolve().unwrap_or_else(|e| panic!("{g}: {e}"));
        };
        assert!(e("movie=a.mov[m];[in][m]overlay[out]").contains("allowlist"));
        assert!(e("[in]eq,movie=b.mkv[out]").contains("movie"));
        assert!(e("metadata=mode=print:file=log.txt").contains("allowlist"));
        assert!(e("metadata=mode=print").contains("allowlist"));
        assert!(e("ssim=stats.log").contains("allowlist"));
        assert!(e("vidstabdetect=result=t.trf").contains("allowlist"));
        assert!(e("sendcmd=f=cmds.txt,eq").contains("allowlist"));
        for g in [
            "paletteuse=debug_kdtree=k.dot",
            "deshake=filename=d.log",
            "deshake_opencl=filename=d.log",
            "ssim360=stats_file=s.log",
            "frei0r=filter_name=x",
            "ladspa=file=x",
            "lv2=plugin=x",
            "dnn_processing=model=m",
            "libvmaf",
            "libplacebo=custom_shader_path=s.hook",
            "program_opencl=source=k.cl",
            "showcqt",
            "fsync=file=f",
            // An escaped or instance-named name is still the name.
            "mov\\ie=a.mov",
            "'movie'=a.mov",
            "movie@x=a.mov",
        ] {
            assert!(e(g).contains("allowlist"), "{g}: {}", e(g));
        }
        assert!(e("curves=preset=vintage:plot=p.json").contains("plot"));
        assert!(e("curves=vintage:::::::p.json").contains("positional"));
        assert!(e("lut3d=file=https\\\\\\://example.com/a.cube").contains("URL"));
        assert!(e("lut3d=file=https\\://example.com/a.cube").contains("positional"));
        assert!(e("subtitles='http\\://example.com/a.srt'").contains("URL"));
        assert!(e("scdet,select='gt(scene,0.3)'").contains("allowlist"));
        assert!(e("drawtext=text='%{frame_num}'").contains("restarts"));
        ok("drawtext=text='see https\\://example.com'");
        assert_eq!(node("scdet").resolve().unwrap().preroll, 1);
        assert!(node("freezedetect").resolve().unwrap().whole_clip);
        assert_eq!(node("fieldmatch").resolve().unwrap().postroll, 2);
        assert!(e("pullup").contains("timing"));
    }

    /// Option text libavfilter reads differently from a one-pass parser: the graph parser
    /// unescapes once, the option parser again.
    #[test]
    fn two_passes_like_libavfilter() {
        let e = |g: &str| {
            node(g)
                .resolve()
                .err()
                .unwrap_or_else(|| panic!("{g}: accepted"))
        };
        let ok = |g: &str| {
            node(g).resolve().unwrap_or_else(|e| panic!("{g}: {e}"));
        };
        // The bypasses: a hidden `file=` (pass 1 turns `\:` into `:`) and an escaped key.
        for g in [
            "metadata=mode=print\\:file=/tmp/x",
            "metadata=mode=print:fi\\le=/tmp/x",
            "metadata=mode=print:'file'=/tmp/x",
            "metadata=mode=print:'fi'le=/tmp/x",
            "metadata=mode=print\\\\:file=/tmp/x",
            "metadata='mode=print:file=/tmp/x'",
        ] {
            assert!(e(g).contains("allowlist"), "{g}: {}", e(g));
        }
        // The same tricks on an allowed filter reach its write option, or a key that isn't
        // a plain name.
        for g in [
            "curves=preset=vintage\\:plot=/tmp/x",
            "curves=preset=vintage:pl\\ot=/tmp/x",
            "curves=preset=vintage:'plot'=/tmp/x",
            "curves='preset=vintage:plot=/tmp/x'",
        ] {
            assert!(e(g).contains("plot"), "{g}: {}", e(g));
        }
        for g in [
            "eq=gamma=1.1:/gamma=g.txt",
            "eq=gamma=1.1:\\/gamma=g.txt",
            "eq=gamma=1.1:ga-mma=1",
            "eq=gamma=1.1:g.amma=1",
            "eq=gamma=1.1:=1",
        ] {
            assert!(e(g).contains("plain option name"), "{g}: {}", e(g));
        }
        // Pass 2 sees a quoted key (pass 1 kept the quotes): ffmpeg takes it as a value.
        assert!(e("eq=gamma=1.1:\\'brightness\\'=0.1").contains("positional"));
        assert!(e("eq=\\'brightness\\'=0.1").contains("`=`"));
        assert!(e("eq=gamma=1.1:gamma=1.2").contains("set twice"));
        assert!(e("lut3d=a.cube:file=b.cube").contains("set twice"));
        assert!(e("subtitles=filename=a.srt:f=b.srt").contains("set twice"));
        // Unterminated quotes and trailing backslashes, in either pass.
        assert!(e("eq=brightness='0.1").contains("unterminated quote"));
        assert!(e("eq=brightness=\\'0.1").contains("unterminated quote"));
        assert!(e("eq=brightness=0.1\\").contains("trailing backslash"));
        assert!(e("eq=brightness=0.1\\\\").contains("trailing backslash"));
        assert!(e("sws_flags=bicubic;eq").contains("sws_flags"));
        assert!(e("eq=gamma=1]").contains("trailing"));
        // Legit quoting still passes, and values come out as ffmpeg sees them.
        ok("eq=brightness='0.1*sin(2*PI*t)'");
        ok("eq=eval=frame:brightness=0.1*sin(2*PI*t):contrast='1.1'");
        ok("eq=eval=frame:brightness=0.1*mod(t\\,2)");
        ok("drawtext=text='10\\:00, [ok]; fine':x=10:y=h-20");
        ok("drawtext=text=a\\\\\\:b");
        ok("[in] eq = gamma = 1.1 [x] ; [x] hue@h=s=0 [out]");
        ok("eq=gamma=1.1,");
        let f = parse("eq=brightness='0.1*sin(2*PI*t)':contrast=1\\,1").unwrap();
        assert_eq!(
            f[0].args,
            [
                (Some("brightness".into()), "0.1*sin(2*PI*t)".into()),
                (Some("contrast".into()), "1,1".into())
            ]
        );
        let f = parse("drawtext=text='10\\:00, [ok]'").unwrap();
        assert_eq!(f[0].args[0].1, "10:00, [ok]");
        let f = parse("drawtext=text=a\\\\\\:b:x=1").unwrap();
        assert_eq!(f[0].args[0].1, "a:b");
        let f = parse("[in]hue@h=s=0[out]").unwrap();
        assert_eq!(f[0].name, "hue");
        // Positional indices: drawtext's third is textfile, subtitles' third fontsdir.
        let f = parse("drawtext=f.ttf:hi:t.txt").unwrap();
        assert_eq!(file_args(&f[0]).unwrap(), [(0, Use::Read), (2, Use::Read)]);
        let f = parse("subtitles=a.srt:640x480:fonts").unwrap();
        assert_eq!(
            file_args(&f[0]).unwrap(),
            [(0, Use::Read), (2, Use::ReadDir)]
        );
    }

    #[test]
    fn escape_round_trips() {
        for v in [
            "plain",
            "/a b/c,d[e]f;g:h=i'j\\k",
            " lead",
            "trail ",
            " both\t",
            "\\",
            "''",
        ] {
            let text = format!("lut3d=file={}", escape(&escape(v, "':="), "'[],;"));
            let f = parse(&text).unwrap();
            assert_eq!(f[0].args, [(Some("file".into()), v.into())], "{text}");
        }
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
        assert!(n.labelled);
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
        assert!(!n.labelled);
        // fontsdir: every file under it, subdirectories too, sorted.
        std::fs::create_dir_all(dir.join("fonts/sub")).unwrap();
        std::fs::write(dir.join("fonts/a.ttf"), "a").unwrap();
        std::fs::write(dir.join("fonts/sub/b.otf"), "b").unwrap();
        std::fs::write(dir.join("s.srt"), "").unwrap();
        let mut n = node("subtitles=s.srt:fontsdir=fonts");
        n.locate(&dir).unwrap();
        assert_eq!(
            n.files,
            [
                dir.join("s.srt"),
                dir.join("fonts/a.ttf"),
                dir.join("fonts/sub/b.otf")
            ]
        );
        let e = node("lut3d=missing.cube").locate(&dir).unwrap_err();
        assert!(e.contains("no such file"), "{e}");
        let e = node("vidstabtransform=smoothing=10")
            .locate(&dir)
            .unwrap_err();
        assert!(e.contains("input="), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
