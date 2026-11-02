//! Golden-frame checks for reframe on the GPU (`cargo test -p ffcine --test reframe_gpu --
//! --ignored`; needs a Vulkan GPU and `ffmpeg` on PATH, as in the dev shell):
//!
//! - a 9:16 window rendered by libplacebo matches ffmpeg's CPU `crop,scale=lanczos` of the same
//!   window (the prototype's geometry, without its rounding);
//! - the warp path (one `MAIN` hook resampling `warp * window`) matches the crop path, with an
//!   identity warp and with a translation folded into the window instead.

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::reframe::{Frame, Profile, Reframe, Shot, pass};
use ffcine::{EncoderSettings, Grade, Pipeline, RateControl, backend, geom};

const W: u32 = 3840;
const H: u32 = 2160;
const N: u64 = 6;

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-reframe-gpu-{}", std::process::id()));
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

/// All frames' luma (the geometry; chroma only adds the 4:2:0 round trip).
fn luma(path: &Path) -> Vec<u8> {
    let p = path.to_str().expect("utf-8 path");
    ffmpeg(&["-i", p, "-f", "rawvideo", "-pix_fmt", "gray", "-"])
}

#[allow(clippy::cast_precision_loss)]
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let se: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum();
    10.0 * (255.0_f64.powi(2) / (se / a.len() as f64).max(1e-9)).log10()
}

fn render(ff: &backend::Ffmpeg, src: &Path, out: &Path, setup: impl FnOnce(&mut Grade)) {
    let mut grade = Grade {
        dither: None,
        peak_detect: false,
        ..Grade::default()
    };
    setup(&mut grade);
    let enc = EncoderSettings {
        rate: RateControl::Crf(0.0),
        ..EncoderSettings::x264_preview()
    };
    Pipeline::new(src, out)
        .range(std::time::Duration::ZERO, Some(N))
        .grade(grade)
        .encoder(enc)
        .run(ff)
        .expect("render");
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
#[allow(clippy::many_single_char_names, clippy::too_many_lines)]
fn reel_window_matches_cpu_and_warp_path() {
    let d = dir();
    let src = d.join("src.mkv");
    let s = src.to_str().expect("utf-8");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={W}x{H}:rate=24,format=yuv420p"),
        "-frames:v",
        &N.to_string(),
        "-c:v",
        "libx264",
        "-qp",
        "0",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-color_range",
        "tv",
        s,
    ]);
    let ff = backend::Ffmpeg::new().expect("backend");
    let reel = Profile::preset("reel").expect("preset");
    let shot = Shot {
        width: W,
        height: H,
        frames: N,
        rate: 24.0,
        start: 0.0,
    };
    // x = (3840 - 1215) * 0.4 = 1050: whole pixels, which ffmpeg's crop needs.
    let params = serde_json::json!({"crop_x": 0.4});
    let r = Reframe::parse(params.as_object().expect("object"), None).expect("reframe");
    let frames = r.frames(&shot, &reel);

    // 1. libplacebo crop vs ffmpeg's CPU crop + lanczos.
    let a = d.join("crop.mkv");
    render(&ff, &src, &a, |g| {
        pass::apply(g, &frames, &[], (W, H), &reel);
    });
    let c = frames[0].crop;
    let cpu = d.join("cpu.mkv");
    ffmpeg(&[
        "-i",
        s,
        "-vf",
        &format!(
            "crop={}:{}:{}:{}:exact=1,scale=1080:1920:flags=lanczos",
            c.w, c.h, c.x, c.y
        ),
        "-c:v",
        "libx264",
        "-qp",
        "0",
        "-pix_fmt",
        "yuv420p",
        cpu.to_str().expect("utf-8"),
    ]);
    let (ga, gc) = (luma(&a), luma(&cpu));
    assert_eq!(ga.len(), 1080 * 1920 * usize::try_from(N).expect("n"));
    let p = psnr(&ga, &gc);
    eprintln!("crop vs cpu: {p:.2} dB");
    assert!(p > 45.0, "crop vs cpu: {p:.2} dB");

    // 2. Identity warp: the MAIN hook path against the crop path.
    let b = d.join("warp.mkv");
    render(&ff, &src, &b, |g| {
        pass::apply(g, &frames, &[geom::IDENTITY], (W, H), &reel);
    });
    let p = psnr(&ga, &luma(&b));
    eprintln!("warp(identity) vs crop: {p:.2} dB");
    assert!(p > 45.0, "warp(identity) vs crop: {p:.2} dB");

    // 3. A translation warp equals the window moved by it (warp applies before the crop).
    let shifted: Vec<Frame> = frames
        .iter()
        .map(|f| Frame {
            crop: ffcine::reframe::Rect {
                x: f.crop.x + 40.5,
                ..f.crop
            },
            ..*f
        })
        .collect();
    let e = d.join("shifted.mkv");
    render(&ff, &src, &e, |g| {
        pass::apply(g, &shifted, &[], (W, H), &reel);
    });
    let t = d.join("translated.mkv");
    render(&ff, &src, &t, |g| {
        pass::apply(g, &frames, &[geom::translation(40.5, 0.0)], (W, H), &reel);
    });
    let p = psnr(&luma(&e), &luma(&t));
    eprintln!("warp(translate) vs moved window: {p:.2} dB");
    assert!(p > 45.0, "warp(translate) vs moved window: {p:.2} dB");

    // 4. An animated push and pan: per-frame crops against per-frame hook //!PARAMs. Sub-pixel
    //    windows, so Catmull-Rom + lanczos against lanczos alone: a looser bound.
    let params = serde_json::json!({"crop_x": [0.2, 0.6], "push": [1.0, 1.08]});
    let r = Reframe::parse(params.as_object().expect("object"), None).expect("reframe");
    let frames = r.frames(&shot, &reel);
    assert!(frames[1].crop.x.fract() != 0.0);
    let (pc, pw) = (d.join("push-crop.mkv"), d.join("push-warp.mkv"));
    render(&ff, &src, &pc, |g| {
        pass::apply(g, &frames, &[], (W, H), &reel);
    });
    render(&ff, &src, &pw, |g| {
        pass::apply(g, &frames, &[geom::IDENTITY], (W, H), &reel);
    });
    let (lc, lw) = (luma(&pc), luma(&pw));
    let frame = 1080 * 1920;
    for i in 0..usize::try_from(N).expect("n") {
        let p = psnr(
            &lc[i * frame..(i + 1) * frame],
            &lw[i * frame..(i + 1) * frame],
        );
        eprintln!("push frame {i}: warp vs crop {p:.2} dB");
        assert!(p > 35.0, "push frame {i}: {p:.2} dB");
    }
    std::fs::remove_dir_all(&d).ok();
}
