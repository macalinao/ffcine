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

/// Mean of `luma` (one frame, `w` wide) over rows `y0..y1`, columns `x0..x1`.
#[allow(clippy::cast_precision_loss)]
fn mean(luma: &[u8], w: usize, (x0, x1): (usize, usize), (y0, y1): (usize, usize)) -> f64 {
    let mut sum = 0.0;
    for y in y0..y1 {
        sum += luma[y * w + x0..y * w + x1]
            .iter()
            .map(|v| f64::from(*v))
            .sum::<f64>();
    }
    sum / ((x1 - x0) * (y1 - y0)) as f64
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
#[allow(clippy::many_single_char_names)]
fn contain_fill_and_nle_geometry() {
    use ffcine::reframe::geo::{Crop, Transform};
    let d = dir();
    let src = d.join("hd.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=24,format=yuv420p",
        "-frames:v",
        &N.to_string(),
        "-c:v",
        "libx264",
        "-qp",
        "0",
        src.to_str().expect("utf-8"),
    ]);
    let ff = backend::Ffmpeg::new().expect("backend");
    let shot = Shot {
        width: 1920,
        height: 1080,
        frames: N,
        rate: 24.0,
        start: 0.0,
    };
    let frame = |p: &Path, w: usize, h: usize| luma(p)[..w * h].to_vec();

    // fit: contain into 9:16: blurred picture above and below, black with fill_blur 0.
    let reel = Profile::preset("reel").expect("preset");
    let mut top = Vec::new();
    for (name, v) in [
        ("blur", serde_json::json!({"fit": "contain"})),
        (
            "black",
            serde_json::json!({"fit": "contain", "fill_blur": 0}),
        ),
    ] {
        let r = Reframe::parse(v.as_object().expect("object"), None).expect("reframe");
        let frames = r.frames(&shot, &reel);
        let out = d.join(format!("contain-{name}.mkv"));
        render(&ff, &src, &out, |g| {
            pass::apply(g, &frames, &[], (1920, 1080), &reel)
        });
        let y = frame(&out, 1080, 1920);
        top.push(mean(&y, 1080, (0, 1080), (0, 200)));
    }
    eprintln!("contain fill: blur {:.1}, black {:.1}", top[0], top[1]);
    assert!(top[1] < 20.0, "black fill: {}", top[1]);
    assert!(top[0] > 40.0, "blurred fill: {}", top[0]);

    // An identity transform: the mapped hook matches the plain path.
    let hd = Profile::preset("hd").expect("preset");
    let r = Reframe::default();
    let plain = d.join("plain.mkv");
    render(&ff, &src, &plain, |g| {
        pass::apply(g, &r.frames(&shot, &hd), &[], (1920, 1080), &hd);
    });
    let geo = |crop: Option<serde_json::Value>, tf: serde_json::Value| ffcine::reframe::Geo {
        crop: crop.map(|c| Crop::parse(c.as_object().expect("object")).expect("crop")),
        transform: Some(Transform::parse(tf.as_object().expect("object")).expect("tf")),
    };
    let mapped = |g: &ffcine::reframe::Geo| -> Vec<_> {
        (0..N).map(|i| g.frame(&r, i, &shot, &hd).1).collect()
    };
    let ident = d.join("ident.mkv");
    let m = mapped(&geo(None, serde_json::json!({"scale_x": 1.0})));
    render(&ff, &src, &ident, |g| {
        pass::apply_mapped(g, &m, &[], (1920, 1080), &hd);
    });
    let p = psnr(&luma(&plain), &luma(&ident));
    eprintln!("identity transform vs plain: {p:.2} dB");
    assert!(p > 45.0, "identity transform: {p:.2} dB");

    // Crop a quarter off the left and scale to half: black left of the kept part and around it.
    let cut = d.join("cut.mkv");
    let m = mapped(&geo(
        Some(serde_json::json!({"left": 0.25})),
        serde_json::json!({"scale_x": 0.5, "scale_y": 0.5}),
    ));
    render(&ff, &src, &cut, |g| {
        pass::apply_mapped(g, &m, &[], (1920, 1080), &hd);
    });
    let y = frame(&cut, 1920, 1080);
    // The half-size picture spans x 480..1440; its left quarter (480..720) is cut.
    let (outside, cut_off, kept) = (
        mean(&y, 1920, (0, 400), (100, 980)),
        mean(&y, 1920, (500, 700), (300, 780)),
        mean(&y, 1920, (800, 1400), (300, 780)),
    );
    eprintln!("cut: outside {outside:.1}, cut {cut_off:.1}, kept {kept:.1}");
    assert!(outside < 20.0 && cut_off < 20.0 && kept > 40.0);
}
