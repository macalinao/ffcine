//! The libavfilter escape hatch (`docs/compositing.md#libavfilter-escape-hatch`): a raw
//! libavfilter graph run over a layer's frames in its prep, before the GPU pass.
//!
//! This module is the backend-neutral half: the node ([`Node`]), the statefulness table
//! ([`reach`], under [`FILTER_TABLE_REVISION`]) that says how many frames before and after a
//! piece a graph reads, the `n` lint, and the [`Prep`] a lowered layer carries for the renderer
//! to run (`ffcine-assembly` runs it through the `ffmpeg` CLI into a lossless intermediate).

use std::ops::Range;
use std::path::PathBuf;
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
/// `mpdecimate`, `decimate`).
pub fn changes_timing(name: &str) -> bool {
    matches!(
        name,
        "minterpolate" | "framerate" | "mpdecimate" | "decimate"
    )
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
        "tblend" => w(1, 0),
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
        "lagfun" | "hqdn3d" | "vidstabtransform" | "deshake" | "normalize" | "photosensitivity" => {
            Reach {
                whole_clip: true,
                ..Reach::default()
            }
        }
        n if changes_timing(n) => Reach {
            whole_clip: true,
            ..Reach::default()
        },
        _ => Reach::default(),
    }
}

/// Whether an option value reads libavfilter's frame counter (`n`, or `N` as `setpts` and
/// `select` spell it), which counts from the first frame a piece feeds, not the layer's.
fn uses_n(value: &str) -> bool {
    value
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|t| t == "n" || t == "N")
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
            && let Some(f) = filters
                .iter()
                .find(|f| f.args.iter().any(|(_, v)| uses_n(v)))
        {
            return Err(format!(
                "{}: the frame counter n restarts in every piece; use t*FRAME_RATE (the node's \
                 clock) or \"whole_clip\": true",
                f.name
            ));
        }
        Ok(r)
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
}
