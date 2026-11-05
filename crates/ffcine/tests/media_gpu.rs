//! Media layers on the GPU (`cargo test -p ffcine --test media_gpu -- --ignored`; needs a Vulkan
//! GPU and `ffmpeg` on PATH, as in the dev shell): HEIC stills against ffmpeg's CPU decode
//! (tile grid, and `irot` upright), a P3-tagged still against ffmpeg's `libplacebo` filter, a
//! nested comp against the same layers flattened, and text hidden under a layer.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::assembly::{CompMedia, FsFingerprinter, Progress, Renderer};
use ffcine::color::Transfer;
use ffcine::comp::Comp;
use ffcine::{ColorSpace, Decode, Device, EncoderSettings, Size, backend};
use serde_json::{Value, json};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-media-gpu-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn heic(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ffcine-render/testdata/heic")
        .join(name)
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

fn rgb(path: &Path, extra: &[&str]) -> Vec<u8> {
    let p = path.to_str().expect("utf-8");
    let mut a = vec!["-i", p];
    a.extend_from_slice(extra);
    a.extend_from_slice(&["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
    ffmpeg(&a)
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64.powi(2) / mse).log10()
    }
}

fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

/// Renders frame `at` of a `w`x`h` comp of `layers` (media in `d`) as BT.709 RGB.
fn render(d: &Path, size: [u32; 2], layers: Value, at: i64, name: &str) -> Vec<u8> {
    render_to(d, size, layers, at, name, ColorSpace::BT709_RGB)
}

/// [`render`] to `output`.
fn render_to(
    d: &Path,
    [w, h]: [u32; 2],
    layers: Value,
    at: i64,
    name: &str,
    output: ColorSpace,
) -> Vec<u8> {
    let comp = Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [w, h], "fps": 30, "duration": 1.0 },
            "layers": layers,
        }),
        d,
    )
    .expect("comp");
    let ff = backend::Ffmpeg::new().expect("backend");
    let dims = |p: &Path| {
        let info = ffcine::Backend::probe(&ff, p)?;
        let (_, v) = info.video().expect("video");
        Ok((v.width, v.height))
    };
    let media = CompMedia {
        fp: &FsFingerprinter,
        dims: &dims,
        profile: None,
    };
    let size = Size {
        width: w,
        height: h,
    };
    let mut piece = comp.lower(at..at + 1, size, &media).expect("lower");
    piece.grade.dither = None;
    let r = Renderer {
        backend: &ff,
        cache: d.join("cache"),
        ffmpeg: "ffmpeg".into(),
        device: Device::Auto,
        decode: Decode::default(),
        progress: Progress::Quiet,
        grader: None,
        detect: None,
    };
    let out = d.join(name);
    r.render_piece(
        &piece,
        comp.canvas.fps,
        &out,
        EncoderSettings::png_still(),
        Some(output),
    )
    .expect("render");
    rgb(&out, &[])
}

/// ffmpeg's CPU decode of a HEIC (the CLI assembles the tile grid), its chroma upsampled as a
/// still's swscale conversion does (not by the unscaled fast path, which replicates it), then
/// `vf` (after a comma), as RGB.
fn cpu_decode(d: &Path, src: &Path, vf: &str) -> Vec<u8> {
    let yuv = d.join("cpu.nut");
    let (src, yuv_s) = (src.to_str().expect("utf-8"), yuv.to_str().expect("utf-8"));
    ffmpeg(&["-i", src, "-frames:v", "1", "-c:v", "rawvideo", yuv_s]);
    let vf =
        format!("scale=flags=bicubic+accurate_rnd+full_chroma_int:in_range=pc,format=rgba64le{vf}");
    rgb(&yuv, &["-vf", &vf])
}

/// A tiled HEIC still is the picture ffmpeg's CPU decode assembles, and one with `irot` comes
/// out upright (a grid or a single item): the unrotated decode turned 90 degrees clockwise. The fixtures are tagged sRGB,
/// so the output is too: the pixels pass through unconverted.
#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn heic_stills_match_the_cpu_decode() {
    let d = dir("heic");
    let srgb = ColorSpace {
        transfer: Transfer::Srgb,
        ..ColorSpace::BT709_RGB
    };
    let still = |name: &str| json!([{ "source": { "still": heic(name) } }]);
    let got = render_to(&d, [200, 120], still("grid.heic"), 0, "grid.png", srgb);
    let want = cpu_decode(&d, &heic("grid.heic"), "");
    let db = psnr(&got, &want);
    eprintln!("heic grid: {db:.1} dB");
    assert!(db >= 45.0, "heic grid: {db:.1} dB");

    let got = render_to(&d, [120, 200], still("grid-rot.heic"), 0, "rot.png", srgb);
    let want = cpu_decode(&d, &heic("grid.heic"), ",transpose=clock");
    let db = psnr(&got, &want);
    eprintln!("heic irot: {db:.1} dB");
    assert!(db >= 45.0, "heic irot: {db:.1} dB");

    // A single (untiled) item with `irot`: upright too, at its turned size.
    let got = render_to(
        &d,
        [120, 200],
        still("whole-rot.heic"),
        0,
        "whole-rot.png",
        srgb,
    );
    let want = cpu_decode(&d, &heic("whole.heic"), ",transpose=clock");
    let db = psnr(&got, &want);
    eprintln!("heic item irot: {db:.1} dB");
    assert!(db >= 45.0, "heic item irot: {db:.1} dB");
    let _ = std::fs::remove_dir_all(&d);
}

/// A still tagged Display P3 is converted to the output's BT.709 as libplacebo converts it.
#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn p3_still_matches_libplacebo() {
    let d = dir("p3");
    let src = d.join("p3.png");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=128x128,format=rgb24,\
         setparams=color_primaries=smpte432:color_trc=iec61966-2-1:colorspace=gbr:range=pc",
        "-frames:v",
        "1",
        src.to_str().expect("utf-8"),
    ]);
    let got = render(
        &d,
        [128, 128],
        json!([{ "source": { "still": "p3.png" } }]),
        0,
        "out.png",
    );
    let want = rgb(
        &src,
        &[
            "-init_hw_device",
            "vulkan",
            "-vf",
            "libplacebo=color_primaries=bt709:color_trc=bt709:colorspace=gbr:range=pc:\
             format=rgb24:dithering=none",
        ],
    );
    let untouched = rgb(&src, &[]);
    let db = psnr(&got, &want);
    eprintln!(
        "p3 still: {db:.1} dB against libplacebo, {:.1} dB against the unconverted source",
        psnr(&got, &untouched)
    );
    assert!(db >= 40.0, "p3 still: {db:.1} dB");
    assert!(psnr(&want, &untouched) < 40.0, "the conversion shows");
    let _ = std::fs::remove_dir_all(&d);
}

/// A nested comp of a video and solids renders as the same layers flattened, within 1 code value.
#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn nested_comp_matches_flattened_layers() {
    let d = dir("nested");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30,format=gbrp",
        "-frames:v",
        "30",
        "-c:v",
        "ffv1",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-colorspace",
        "rgb",
        "-color_range",
        "pc",
        d.join("src.mkv").to_str().expect("utf-8"),
    ]);
    let inner = json!([
        { "source": { "video": "src.mkv" } },
        { "source": { "solid": "#ff0000" }, "size": [100, 60],
          "transform": { "anchor": [0, 0], "position": [20, 20], "opacity": 0.5 } },
        { "source": { "solid": "#3399ff" }, "size": [120, 100], "blend": "multiply",
          "transform": { "anchor": [0, 0], "position": [150, 100] } },
    ]);
    let top = json!({ "source": { "solid": "#00cc00" }, "size": [80, 80], "blend": "screen",
        "transform": { "anchor": [0, 0], "position": [200, 20] } });
    let mut flat = inner.as_array().expect("array").clone();
    flat.push(top.clone());
    let nested = json!([{ "source": { "comp": { "layers": inner } } }, top]);
    let want = render(&d, [320, 240], Value::Array(flat), 12, "flat.png");
    let got = render(&d, [320, 240], nested, 12, "nested.png");
    let m = max_diff(&got, &want);
    eprintln!("nested vs flattened: max {m} code values");
    assert!(m <= 1, "nested vs flattened: max {m} code values");
    let _ = std::fs::remove_dir_all(&d);
}

/// Text under an opaque layer is hidden; on top it shows.
#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn text_under_a_layer_is_hidden() {
    let d = dir("text");
    let text = json!({ "source": { "text": { "text": "HIDDEN", "size": 60 } } });
    let cover = json!({ "source": { "solid": "#204060" } });
    let shown = render(&d, [320, 240], json!([cover, text]), 0, "shown.png");
    let hidden = render(&d, [320, 240], json!([text, cover]), 0, "hidden.png");
    let bare = render(&d, [320, 240], json!([cover]), 0, "bare.png");
    assert!(max_diff(&shown, &bare) > 100, "text on top shows");
    let m = max_diff(&hidden, &bare);
    eprintln!("text under a layer: max {m} code values from the bare layer");
    assert_eq!(m, 0, "text under an opaque layer is hidden");
    let _ = std::fs::remove_dir_all(&d);
}
