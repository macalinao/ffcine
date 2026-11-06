//! Per-layer grades on the GPU (`cargo test -p ffcine --test layer_grade_gpu -- --ignored`; needs
//! a Vulkan GPU and `ffmpeg` on PATH): a comp layer's `rgb_gain`, project LUT and `filters` against
//! the same chain run by ffmpeg's CPU filters on the same synthetic source, with the layer over a
//! solid so it composites rather than standing in for the base.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::assembly::{CompMedia, FsFingerprinter, Progress, Renderer};
use ffcine::comp::Comp;
use ffcine::{ColorSpace, Decode, Device, EncoderSettings, Size, backend};
use serde_json::{Value, json};

const W: u32 = 320;
const H: u32 = 180;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-layer-grade-{name}-{}", std::process::id()));
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

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8")
}

/// Mean and max absolute difference of two RGB24 buffers.
#[allow(clippy::cast_precision_loss)]
fn diff(a: &[u8], b: &[u8]) -> (f64, i32) {
    assert_eq!(a.len(), b.len());
    let mut sum = 0u64;
    let mut max = 0;
    for (x, y) in a.iter().zip(b) {
        let d = (i32::from(*x) - i32::from(*y)).abs();
        sum += u64::try_from(d).unwrap_or(0);
        max = max.max(d);
    }
    (sum as f64 / a.len() as f64, max)
}

/// Frame 0 of a comp (a black solid under `layer`, with `project`) as BT.709 RGB24.
fn render(d: &Path, project: &Value, layer: &Value, name: &str) -> Vec<u8> {
    let comp = Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [W, H], "fps": 24, "duration": 1.0 },
            "project": project,
            "layers": [{ "source": { "solid": "#000000" } }, layer],
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
        width: W,
        height: H,
    };
    let mut piece = comp.lower(0..1, size, &media).expect("lower");
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
        Some(ColorSpace::BT709_RGB),
    )
    .expect("render");
    ffmpeg(&["-i", s(&out), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
}

/// The source's first frame through ffmpeg filter `vf` on the CPU, as RGB24.
fn cpu(src: &Path, vf: &str) -> Vec<u8> {
    ffmpeg(&[
        "-i",
        s(src),
        "-vf",
        &format!("{vf},format=rgb24"),
        "-frames:v",
        "1",
        "-f",
        "rawvideo",
        "-",
    ])
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn layer_grade_matches_ffmpeg() {
    let d = dir("cpu");
    let src = d.join("src.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={W}x{H}:rate=24,gblur=sigma=3,format=gbrp"),
        "-frames:v",
        "24",
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
        s(&src),
    ]);
    // A 17^3 LUT squaring red and inverting blue.
    let mut text = String::from("LUT_3D_SIZE 17\n");
    for b in 0..17 {
        for g in 0..17 {
            for r in 0..17 {
                let (r, g, b) = (
                    f64::from(r) / 16.0,
                    f64::from(g) / 16.0,
                    f64::from(b) / 16.0,
                );
                let _ = writeln!(text, "{} {} {}", r * r, g, 1.0 - b);
            }
        }
    }
    let cube = d.join("t.cube");
    std::fs::write(&cube, text).expect("cube");
    let project = json!({ "luts": { "test": "t.cube" } });
    let video = |grade: Value| json!({ "source": { "video": "src.mkv" }, "grade": grade });

    let plain = render(
        &d,
        &json!({}),
        &json!({ "source": { "video": "src.mkv" } }),
        "plain.png",
    );
    let base = diff(&plain, &cpu(&src, "null"));
    eprintln!("ungraded layer: {base:?}");
    assert!(base.0 < 0.6, "ungraded layer {base:?}");

    let g = render(
        &d,
        &project,
        &video(json!({ "rgb_gain": [1.25, 1.0, 0.7], "lut": "test" })),
        "lut.png",
    );
    let want = cpu(
        &src,
        &format!(
            "colorchannelmixer=rr=1.25:gg=1:bb=0.7,lut3d=file={}:interp=trilinear",
            s(&cube)
        ),
    );
    let r = diff(&g, &want);
    eprintln!(
        "layer rgb_gain + LUT: {r:?} (changed {:?})",
        diff(&g, &plain)
    );
    assert!(r.0 < 1.0, "layer rgb_gain + LUT {r:?}");

    let g = render(
        &d,
        &project,
        &video(json!({ "filters": [
            "eq=contrast=1.2:saturation=1.3",
            { "colorlevels": { "rimax": 0.8, "bomin": 0.1 } },
        ] })),
        "filters.png",
    );
    let want = cpu(
        &src,
        "eq=contrast=1.2:saturation=1.3,colorlevels=rimax=0.8:bomin=0.1",
    );
    let r = diff(&g, &want);
    let moved = diff(&g, &plain);
    eprintln!("layer filters: {r:?} (changed {moved:?})");
    assert!(
        r.0 < 1.0 && moved.0 > 5.0 * r.0,
        "layer filters {r:?}, changed {moved:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
