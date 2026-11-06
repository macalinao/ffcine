//! Runs a layer's libavfilter node (`docs/compositing.md#libavfilter-escape-hatch`) into a
//! lossless intermediate the GPU pass reads as a plain video.
//!
//! Two `ffmpeg` processes: the first decodes the source (turned upright) to YUV4MPEG on its
//! stdout; this module reads whole frames and writes the ones the node's frame map asks for, in
//! map order, to the second's stdin, which runs the graph and writes FFV1. The second numbers
//! its input frames on the node's clock (`setpts=PTS+<first>`), so `t` (and `T`) in the graph
//! read as in an unsplit render, then trims the pre-roll and post-roll and restarts the
//! timestamps.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

use ffcine_core::ColorSpace;
use ffcine_core::color::{Matrix, Range};
use ffcine_render::filter_node::{Hw, Prep};

use crate::exec::rational;

/// The prep's format, in the intermediate's name: change it when the arguments below change.
pub(crate) const FILTER_PREP: &str = "y4m-ffv1-l3-g1";

/// Frames a backward frame map holds in memory, at most (bytes).
const MAX_HELD: usize = 4 << 30;

/// The YUV4MPEG carrier for a source pixel format: itself when the format has a YUV4MPEG tag,
/// else a 4:4:4 one that loses nothing (8-bit with alpha: YUV4MPEG has no deeper alpha).
pub(crate) fn carrier(pix_fmt: &str) -> &'static str {
    const Y4M: [&str; 21] = [
        "gray",
        "gray10le",
        "gray12le",
        "gray16le",
        "yuv420p",
        "yuv420p10le",
        "yuv420p12le",
        "yuv420p16le",
        "yuv422p",
        "yuv422p10le",
        "yuv422p12le",
        "yuv422p16le",
        "yuv444p",
        "yuv444p10le",
        "yuv444p12le",
        "yuv444p16le",
        "yuva444p",
        "yuv411p",
        "yuv420p9le",
        "yuv422p9le",
        "yuv444p9le",
    ];
    if let Some(f) = Y4M.iter().find(|f| **f == pix_fmt) {
        return f;
    }
    match pix_fmt {
        "yuvj420p" | "nv12" => "yuv420p",
        "yuvj422p" => "yuv422p",
        "yuvj444p" => "yuv444p",
        "p010le" | "p010be" => "yuv420p10le",
        f if f.starts_with("yuva")
            || f.contains("rgba")
            || f.contains("bgra")
            || f.contains("gbrap")
            || f.starts_with("argb")
            || f.starts_with("abgr")
            || f.starts_with("ya") =>
        {
            "yuva444p"
        }
        _ => "yuv444p16le",
    }
}

/// Whether `pix_fmt` is RGB (carried as full-range BT.709 YCbCr).
fn is_rgb(pix_fmt: &str) -> bool {
    [
        "rgb", "bgr", "gbr", "argb", "abgr", "0rgb", "0bgr", "x2rgb", "x2bgr",
    ]
    .iter()
    .any(|f| pix_fmt.starts_with(f))
}

/// The upload format for a hardware node: what `hwupload` takes for the carrier.
fn upload_format(carrier: &str) -> &'static str {
    match carrier {
        "yuv420p" => "nv12",
        "yuv420p10le" => "p010le",
        "yuv444p" => "yuv444p",
        _ => "yuv444p16le",
    }
}

/// `setparams` options tagging the fed frames as the source is tagged (the raw pipe drops the
/// tags). The graph may re-tag them; the encoder writes what comes out.
fn tags(c: ColorSpace) -> String {
    let mut v = Vec::new();
    for (k, s) in [
        ("range", c.range.as_str()),
        ("color_primaries", c.primaries.as_str()),
        ("color_trc", c.transfer.as_str()),
        ("colorspace", c.matrix.as_str()),
    ] {
        if s != "unknown" {
            v.push(format!("{k}={s}"));
        }
    }
    v.join(":")
}

/// The second process's filtergraph: tag, clock, (upload,) the user's graph, (download,) trim.
/// A graph of several chains, or with link labels, names its ends `[in]` and `[out]`.
pub(crate) fn filtergraph(p: &Prep, color: ColorSpace, carrier: &str) -> String {
    let mut pre = Vec::new();
    let t = tags(color);
    if !t.is_empty() {
        pre.push(format!("setparams={t}"));
    }
    pre.push(format!("setpts=PTS+{}", p.pts));
    let mut post = Vec::new();
    if p.hw != Hw::Cpu {
        let f = upload_format(carrier);
        pre.push(format!("format={f},hwupload"));
        post.push(format!("hwdownload,format={f}"));
    }
    if !p.free_timing {
        post.push(format!(
            "trim=start_frame={}:end_frame={}",
            p.store.start, p.store.end
        ));
    }
    post.push("setpts=PTS-STARTPTS".into());
    let (pre, post) = (pre.join(","), post.join(","));
    if p.labelled {
        format!("[0:v]{pre}[in];{};[out]{post}[fo]", p.graph)
    } else {
        format!("[0:v]{pre},{},{post}[fo]", p.graph)
    }
}

/// Arguments for the decoder process: `frames` frames of the source from `p.start`, upright, as
/// YUV4MPEG in `carrier` on stdout.
fn decode_args(p: &Prep, carrier: &str, frames: i64, rgb: bool) -> Vec<OsString> {
    let mut v: Vec<OsString> = [
        "-protocol_whitelist",
        "file",
        "-ss",
        &p.start.as_secs_f64().to_string(),
        "-i",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    v.push(p.input.clone().into());
    if rgb {
        // YUV4MPEG has no RGB: full-range BT.709 YCbCr,, as tagged.
        v.extend(["-vf", "scale=out_color_matrix=bt709:out_range=full"].map(Into::into));
    }
    for a in [
        "-map",
        "0:v:0",
        "-frames:v",
        &frames.to_string(),
        "-fps_mode",
        "passthrough",
        "-pix_fmt",
        carrier,
        "-strict",
        "-1",
        "-f",
        "yuv4mpegpipe",
        "-",
    ] {
        v.push(a.into());
    }
    v
}

/// Arguments for the filter process: YUV4MPEG on stdin through `graph` to FFV1 in `out`.
fn filter_args(p: &Prep, graph: &str, out: &Path) -> Vec<OsString> {
    let mut v: Vec<OsString> = Vec::new();
    match p.hw {
        Hw::Cpu => {}
        Hw::Cuda => {
            v.extend(["-init_hw_device", "cuda=hw", "-filter_hw_device", "hw"].map(Into::into));
        }
        Hw::Vulkan => {
            v.extend(["-init_hw_device", "vulkan=hw", "-filter_hw_device", "hw"].map(Into::into));
        }
    }
    for a in [
        "-protocol_whitelist",
        "pipe",
        "-f",
        "yuv4mpegpipe",
        "-i",
        "pipe:0",
        "-filter_complex",
        graph,
        "-map",
        "[fo]",
        "-fps_mode",
        "passthrough",
        "-c:v",
        "ffv1",
        "-level",
        "3",
        "-g",
        "1",
        "-slices",
        "16",
        "-an",
    ] {
        v.push(a.into());
    }
    v.push(out.into());
    v
}

/// A YUV4MPEG stream's frame size in bytes, from its header line.
fn frame_bytes(header: &str) -> Result<usize, String> {
    let (mut w, mut h, mut c) = (0usize, 0usize, "420jpeg");
    for t in header.split_ascii_whitespace().skip(1) {
        match t.split_at(1) {
            ("W", v) => w = v.parse().map_err(|_| format!("bad YUV4MPEG width {v}"))?,
            ("H", v) => h = v.parse().map_err(|_| format!("bad YUV4MPEG height {v}"))?,
            ("C", v) => c = v,
            _ => {}
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let depth = c
        .strip_prefix("mono")
        .or_else(|| c.rsplit_once('p').map(|(_, d)| d))
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(8);
    let bytes = if depth > 8 { 2 } else { 1 };
    let samples = if c.starts_with("mono") {
        w * h
    } else if c.starts_with("444alpha") {
        4 * w * h
    } else if c.starts_with("444") {
        3 * w * h
    } else if c.starts_with("422") {
        w * h + 2 * cw * h
    } else if c.starts_with("411") {
        w * h + 2 * w.div_ceil(4) * h
    } else if c.starts_with("420") {
        w * h + 2 * cw * ch
    } else {
        return Err(format!("unsupported YUV4MPEG colour space C{c}"));
    };
    Ok(samples * bytes)
}

/// The header for the filter process: the source's, at the fed rate.
fn retimed(header: &str, rate: f64) -> String {
    let r = rational(rate);
    header
        .split_ascii_whitespace()
        .map(|t| {
            if t.starts_with('F') {
                format!("F{}:{}", r.num, r.den)
            } else {
                t.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drains a child's stderr on a thread (a full pipe would stall it), keeping the tail.
fn drain(child: &mut Child) -> Option<JoinHandle<String>> {
    let mut e = child.stderr.take()?;
    Some(std::thread::spawn(move || {
        let mut s = String::new();
        let _ = e.read_to_string(&mut s);
        let tail: Vec<&str> = s.lines().rev().take(8).collect();
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    }))
}

fn spawn(ffmpeg: &OsStr, args: &[OsString], stdin: Stdio, stdout: Stdio) -> Result<Child, String> {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running {}: {e}", ffmpeg.to_string_lossy()))
}

/// Runs `p`'s node into `out` (FFV1). `pix_fmt` and `color`: the source's pixel format and tags.
pub(crate) fn run(
    ffmpeg: &OsStr,
    p: &Prep,
    pix_fmt: &str,
    color: ColorSpace,
    out: &Path,
) -> Result<(), String> {
    let carrier = carrier(pix_fmt);
    let frames = p.map.iter().copied().max().unwrap_or(0) + 1;
    let rgb = is_rgb(pix_fmt);
    let mut color = color;
    if rgb {
        color.matrix = Matrix::Bt709;
        color.range = Range::Full;
    }
    let graph = filtergraph(p, color, carrier);
    let mut dec = spawn(
        ffmpeg,
        &decode_args(p, carrier, frames, rgb),
        Stdio::null(),
        Stdio::piped(),
    )?;
    let dec_log = drain(&mut dec);
    let mut fil = match spawn(
        ffmpeg,
        &filter_args(p, &graph, out),
        Stdio::piped(),
        Stdio::null(),
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = dec.kill();
            let _ = dec.wait();
            return Err(e);
        }
    };
    let fil_log = drain(&mut fil);
    let fed = feed(&mut dec, &mut fil, p);
    if fed.is_err() {
        let _ = dec.kill();
    }
    drop(fil.stdin.take());
    let dec_ok = dec.wait().is_ok_and(|s| s.success());
    let fil_ok = fil.wait().is_ok_and(|s| s.success());
    let dec_log = dec_log.and_then(|h| h.join().ok()).unwrap_or_default();
    let fil_log = fil_log.and_then(|h| h.join().ok()).unwrap_or_default();
    let with_log = |e: String| {
        if dec_log.is_empty() {
            e
        } else {
            format!("{e}: {dec_log}")
        }
    };
    // A short or failed decode first; a failed write is the filter process's failure.
    if let Err(e) = &fed
        && !e.starts_with("feeding")
    {
        return Err(with_log(e.clone()));
    }
    if !fil_ok {
        return Err(format!("filter graph `{}`: {fil_log}", p.graph));
    }
    fed?;
    if !dec_ok {
        return Err(with_log(format!("decoding {}", p.input.display())));
    }
    Ok(())
}

/// Fails unless the intermediate's size `got` is the layer's source size `want`: the layer's
/// `fit` and `crop` place the source, so a graph keeps its size.
pub(crate) fn check_dims(got: Option<(u32, u32)>, want: (u32, u32)) -> Result<(), String> {
    match got {
        Some(g) if g == want => Ok(()),
        Some((w, h)) => Err(format!(
            "the graph output {w}x{h} from a {}x{} source: a filter node must keep the layer's \
             size (scale, crop and pad with the layer's fit, crop and transform instead)",
            want.0, want.1
        )),
        None => Err("the graph wrote no video".into()),
    }
}

/// Copies the mapped frames from the decoder to the filter process.
fn feed(dec: &mut Child, fil: &mut Child, p: &Prep) -> Result<(), String> {
    let src = dec.stdout.take().ok_or("decoder stdout")?;
    let mut src = BufReader::with_capacity(1 << 22, src);
    let mut dst = fil.stdin.take().ok_or("filter stdin")?;
    let mut header = String::new();
    src.read_line(&mut header)
        .map_err(|e| format!("reading the decoded source: {e}"))?;
    if !header.starts_with("YUV4MPEG2 ") {
        return Err("the source decoded to no frames".into());
    }
    let size = frame_bytes(header.trim_end())?;
    let wr = |e: std::io::Error| format!("feeding the filter graph: {e}");
    writeln!(dst, "{}", retimed(header.trim_end(), p.rate)).map_err(wr)?;
    // The next decoded frame, or `None` at the end of the stream.
    let mut next = |buf: &mut Vec<u8>| -> Result<bool, String> {
        let mut line = Vec::new();
        let n = src
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("reading the decoded source: {e}"))?;
        if n == 0 {
            return Ok(false);
        }
        if !line.starts_with(b"FRAME") {
            return Err("unexpected data in the decoded source".into());
        }
        buf.resize(size, 0);
        src.read_exact(buf)
            .map_err(|e| format!("reading the decoded source: {e}"))?;
        Ok(true)
    };
    let mut put = |frame: &[u8]| -> Result<(), String> {
        dst.write_all(b"FRAME\n").map_err(wr)?;
        dst.write_all(frame).map_err(wr)
    };
    let short = |got: i64| {
        format!(
            "the source decoded to {got} frames from {:.3} s, the frame map needs {}",
            p.start.as_secs_f64(),
            p.map.iter().copied().max().unwrap_or(0) + 1
        )
    };
    if p.map.windows(2).all(|w| w[0] <= w[1]) {
        // Forward: one frame held.
        let (mut cur, mut at) = (Vec::new(), -1i64);
        for &k in p.map.iter() {
            while at < k {
                let mut b = Vec::new();
                if !next(&mut b)? {
                    return Err(short(at + 1));
                }
                cur = b;
                at += 1;
            }
            put(&cur)?;
        }
    } else {
        // Steps back: every frame the map covers, held.
        let frames = usize::try_from(p.map.iter().copied().max().unwrap_or(0) + 1).unwrap_or(0);
        if frames.saturating_mul(size) > MAX_HELD {
            return Err(format!(
                "a frame map stepping back over {frames} frames of {size} bytes needs more than {} GiB in memory",
                MAX_HELD >> 30
            ));
        }
        let mut held: Vec<Vec<u8>> = Vec::with_capacity(frames);
        while held.len() < frames {
            let mut b = Vec::new();
            if !next(&mut b)? {
                break;
            }
            held.push(b);
        }
        if held.len() < frames {
            return Err(short(i64::try_from(held.len()).unwrap_or(i64::MAX)));
        }
        for &k in p.map.iter() {
            put(&held[usize::try_from(k).unwrap_or(0)])?;
        }
    }
    dst.flush().map_err(wr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn prep(graph: &str) -> Prep {
        Prep {
            graph: graph.into(),
            labelled: ffcine_render::filter_node::labelled(graph).unwrap(),
            hw: Hw::Cpu,
            free_timing: false,
            input: "a.mov".into(),
            dims: (4, 2),
            start: Duration::ZERO,
            map: Arc::from(vec![0, 1, 2]),
            rate: 24.0,
            pts: 7,
            store: 2..5,
            show: 0..3,
            key: String::new(),
        }
    }

    #[test]
    fn graphs() {
        let c = ColorSpace::BT709_TV;
        assert_eq!(
            filtergraph(&prep("tmix=frames=3"), c, "yuv420p"),
            "[0:v]setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709,\
             setpts=PTS+7,tmix=frames=3,trim=start_frame=2:end_frame=5,setpts=PTS-STARTPTS[fo]"
        );
        // A quoted `;` or label is text: the single-chain wrapper.
        for g in ["drawtext=text='a;b'", "drawtext=text='[in] [out]'"] {
            assert_eq!(
                filtergraph(&prep(g), c, "yuv420p"),
                format!(
                    "[0:v]setparams=range=tv:color_primaries=bt709:color_trc=bt709:\
                     colorspace=bt709,setpts=PTS+7,{g},trim=start_frame=2:end_frame=5,\
                     setpts=PTS-STARTPTS[fo]"
                )
            );
        }
        let mut p = prep("[in]split[a][b];[a][b]blend=all_mode=average[out]");
        p.hw = Hw::Cuda;
        p.free_timing = true;
        let g = filtergraph(&p, c, "yuv420p10le");
        assert!(g.ends_with("format=p010le,hwupload[in];[in]split[a][b];[a][b]blend=all_mode=average[out];[out]hwdownload,format=p010le,setpts=PTS-STARTPTS[fo]"), "{g}");
    }

    #[test]
    fn dims_must_match() {
        assert!(check_dims(Some((1920, 1080)), (1920, 1080)).is_ok());
        let e = check_dims(Some((960, 540)), (1920, 1080)).unwrap_err();
        assert!(
            e.contains("960x540") && e.contains("keep the layer's size"),
            "{e}"
        );
        assert!(check_dims(None, (4, 2)).is_err());
    }

    #[test]
    fn processes_take_local_inputs_only() {
        let p = prep("eq");
        let d = decode_args(&p, "yuv420p", 3, false);
        assert_eq!(d[..2], ["-protocol_whitelist", "file"].map(OsString::from));
        let f = filter_args(&p, "eq", Path::new("o.mkv"));
        assert_eq!(f[..2], ["-protocol_whitelist", "pipe"].map(OsString::from));
    }

    #[test]
    fn y4m() {
        assert_eq!(
            frame_bytes("YUV4MPEG2 W4 H2 F24:1 Ip C420jpeg").unwrap(),
            12
        );
        assert_eq!(
            frame_bytes("YUV4MPEG2 W3 H3 F24:1 C420p10").unwrap(),
            2 * (9 + 8)
        );
        assert_eq!(frame_bytes("YUV4MPEG2 W2 H2 C444alpha").unwrap(), 16);
        assert_eq!(frame_bytes("YUV4MPEG2 W2 H2 C444p16").unwrap(), 24);
        assert_eq!(frame_bytes("YUV4MPEG2 W2 H2 C422p12").unwrap(), 16);
        assert_eq!(frame_bytes("YUV4MPEG2 W2 H2 Cmono").unwrap(), 4);
        assert_eq!(frame_bytes("YUV4MPEG2 W2 H2 Cmono16").unwrap(), 8);
        assert_eq!(
            retimed("YUV4MPEG2 W2 H2 F30:1 Ip", 30_000.0 / 1001.0),
            "YUV4MPEG2 W2 H2 F30000:1001 Ip"
        );
        assert_eq!(carrier("yuv420p10le"), "yuv420p10le");
        assert_eq!(carrier("rgba"), "yuva444p");
        assert_eq!(carrier("gbrp10le"), "yuv444p16le");
    }
}
