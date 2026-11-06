//! The libavfilter node (`docs/compositing.md#libavfilter-escape-hatch`) rendered in one piece
//! and in three: the same frames (framemd5), for a `t`-dependent graph, a stateful one with a
//! pre-roll (`tmix`) and a whole-clip one (`deflicker`). `cargo test -p ffcine --test
//! filter_node -- --ignored --test-threads=1`; needs `ffmpeg` on PATH, a Vulkan GPU and NVENC.
#![allow(clippy::single_range_in_vec_init)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::assembly::{CompMedia, FsFingerprinter, Progress, Renderer};
use ffcine::comp::Comp;
use ffcine::{ColorSpace, Decode, Device, EncoderSettings, Size, backend};
use serde_json::{Value, json};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-filter-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg on PATH");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// A 2 s, 30 fps comp of one layer, in `dir`.
fn comp(layer: &Value, dir: &Path) -> Comp {
    Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [320, 180], "fps": "30", "duration": 2, "background": "black" },
            "layers": [layer],
        }),
        dir,
    )
    .expect("comp")
}

fn renderer<'a>(d: &Path, ff: &'a backend::Ffmpeg) -> Renderer<'a> {
    Renderer {
        backend: ff,
        cache: d.join("cache"),
        ffmpeg: "ffmpeg".into(),
        device: Device::Auto,
        decode: Decode::default(),
        progress: Progress::Quiet,
        grader: None,
        detect: None,
    }
}

/// Renders `c` over `pieces` (consecutive ranges) into `out` and returns the framemd5 lines of
/// all of them, in order.
fn render(c: &Comp, pieces: &[std::ops::Range<i64>], d: &Path, tag: &str) -> Vec<String> {
    try_render(c, pieces, d, tag).expect("render")
}

fn try_render(
    c: &Comp,
    pieces: &[std::ops::Range<i64>],
    d: &Path,
    tag: &str,
) -> Result<Vec<String>, String> {
    let ff = backend::Ffmpeg::new().expect("backend");
    let probe = |p: &Path| {
        let info = ffcine::Backend::probe(&ff, p)?;
        let (_, v) = info.video().expect("video");
        Ok((v.width, v.height))
    };
    let rate = |_: &Path| Ok((30.0, 90));
    let media = CompMedia {
        fp: &FsFingerprinter,
        dims: &probe,
        profile: None,
        rate: Some(&rate),
    };
    let size = Size {
        width: 320,
        height: 180,
    };
    let r = renderer(d, &ff);
    let mut md5 = Vec::new();
    for (i, range) in pieces.iter().enumerate() {
        let mut piece = c.lower(range.clone(), size, &media).expect("lower");
        piece.grade.dither = None;
        let out = d.join(format!("{tag}-{i}.mkv"));
        let mut enc = EncoderSettings::x264_preview();
        enc.rate = ffcine::RateControl::ConstQp(0);
        enc.color = ColorSpace::BT709_TV;
        r.render_piece(&piece, 30.0, &out, enc, None)
            .map_err(|e| e.to_string())?;
        let o = out.to_str().expect("utf-8");
        let lines = ffmpeg(&["-i", o, "-map", "0:v", "-f", "framemd5", "-"]);
        md5.extend(
            String::from_utf8_lossy(&lines)
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| l.rsplit(',').next().unwrap_or("").trim().to_owned()),
        );
    }
    Ok(md5)
}

/// A 3 s, 30 fps `testsrc2` source (H.264, long GOP: the decoder has to seek) at `d/src.mp4`.
fn source(d: &Path) -> PathBuf {
    let src = d.join("src.mp4");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=320x180:r=30:d=3",
        "-c:v",
        "libx264",
        "-g",
        "30",
        "-pix_fmt",
        "yuv420p",
        src.to_str().expect("utf-8"),
    ]);
    src
}

/// Renders the layer `src` with `filter` (and `extra` layer keys) whole and in three pieces;
/// asserts they match and returns the frames.
fn whole_vs_split(name: &str, filter: &Value, extra: &Value) -> Vec<String> {
    let d = dir(name);
    let src = source(&d);
    let mut layer = json!({
        "source": { "video": src.to_str().expect("utf-8") },
        "source_in": 0.4,
        "filter": filter,
    });
    for (k, v) in extra.as_object().expect("object") {
        layer[k] = v.clone();
    }
    let c = comp(&layer, &d);
    let whole = render(&c, &[0..60], &d, "whole");
    let split = render(&c, &[0..17, 17..41, 41..60], &d, "split");
    assert_eq!(whole.len(), 60);
    assert_eq!(whole, split, "{name}: three pieces vs one");
    let _ = std::fs::remove_dir_all(&d);
    whole
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn t_dependent_eq_splits() {
    whole_vs_split(
        "eq",
        &json!({ "graph": "eq=eval=frame:brightness=0.25*sin(2*PI*t)" }),
        &json!({}),
    );
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn tmix_splits_and_applies() {
    let mixed = whole_vs_split("tmix", &json!({ "graph": "tmix=frames=5" }), &json!({}));
    let plain = whole_vs_split("tmix-null", &json!({ "graph": "null" }), &json!({}));
    assert_ne!(mixed, plain, "tmix changed nothing");
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn whole_clip_deflicker_splits() {
    whole_vs_split(
        "deflicker",
        &json!({ "graph": "eq=eval=frame:brightness=0.1*mod(n\\,2),deflicker=size=7", "whole_clip": true }),
        &json!({ "time": { "speed": 1.5 } }),
    );
}

#[test]
#[ignore = "needs a Vulkan GPU, CUDA, NVENC and ffmpeg built with them"]
fn hw_nodes_split() {
    let plain = whole_vs_split("hw-null", &json!({ "graph": "null" }), &json!({}));
    let cuda = whole_vs_split(
        "cuda",
        &json!({ "graph": "scale_cuda=w=160:h=90,scale_cuda=w=320:h=180", "hw": "cuda" }),
        &json!({}),
    );
    let vulkan = whole_vs_split(
        "vulkan",
        &json!({ "graph": "avgblur_vulkan=sizeX=3", "hw": "vulkan" }),
        &json!({}),
    );
    assert_ne!(cuda, plain);
    assert_ne!(vulkan, plain);
}

/// A 2-point 3D LUT that inverts.
const INVERT: &str = "LUT_3D_SIZE 2
1 1 1
0 1 1
1 0 1
0 0 1
1 1 0
0 1 0
1 0 0
0 0 0
";

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn a_relative_lut_reads_from_the_comps_dir() {
    // A directory name the graph text must escape.
    let d = dir("lut");
    std::fs::create_dir_all(d.join("my luts, v1")).expect("lut dir");
    std::fs::write(d.join("my luts, v1/invert.cube"), INVERT).expect("lut");
    let plain = whole_vs_split("lut-null", &json!({ "graph": "null" }), &json!({}));
    let inverted = whole_vs_split(
        "lut",
        &json!({ "graph": "lut3d=file='my luts, v1/invert.cube'" }),
        &json!({}),
    );
    assert_ne!(inverted, plain, "the LUT changed nothing");
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn a_graph_that_resizes_is_an_error() {
    let d = dir("dims");
    let src = source(&d);
    let c = comp(
        &json!({
            "source": { "video": src.to_str().expect("utf-8") },
            "filter": { "graph": "scale=160:90" },
        }),
        &d,
    );
    let e = try_render(&c, &[0..60], &d, "dims").expect_err("a resized intermediate");
    assert!(e.contains("keep the layer's size"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}
