//! Comps on the GPU against a CPU reference (`cargo test -p ffcine --test comp_gpu --
//! --ignored`; needs a Vulkan GPU and `ffmpeg` on PATH, as in the dev shell): solids over a
//! promoted `testsrc2` base (normal at half opacity, multiply), solids over the canvas
//! background with a layer fade, and a canvas fade over multiply and difference layers, each
//! lowered by `ffcine-comp`, rendered as a PNG and compared at ≥ 50 dB PSNR; and an ellipse
//! mask's feather, the same width on both axes.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::assembly::{CompMedia, FsFingerprinter, Progress, Renderer};
use ffcine::comp::{Base, Comp};
use ffcine::{BlendMode, ColorSpace, Decode, Device, EncoderSettings, Size, backend};
use serde_json::json;

const W: u32 = 360;
const H: u32 = 640;
/// Frames per bench case.
const FRAMES: i64 = 120;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-comp-gpu-{name}-{}", std::process::id()));
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

/// Draws `colour` over `img` in `[x, y, w, h]` with `blend` at opacity `a`.
fn over(img: &mut [u8], r: [u32; 4], colour: [f32; 3], a: f32, blend: BlendMode) {
    for y in r[1]..r[1] + r[3] {
        for x in r[0]..r[0] + r[2] {
            let i = ((y * W + x) * 3) as usize;
            let b = [0, 1, 2].map(|c| f32::from(img[i + c]) / 255.0);
            let o = blend.composite(b, colour, a);
            for c in 0..3 {
                img[i + c] = (o[c] * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

fn render(comp: &Comp, at: i64, out: &Path, d: &Path) -> Base {
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
        rate: None,
    };
    let size = Size {
        width: W,
        height: H,
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
    r.render_piece(
        &piece,
        comp.canvas.fps,
        out,
        EncoderSettings::png_still(),
        Some(ColorSpace::BT709_RGB),
    )
    .expect("render");
    piece.base
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn solids_over_video_and_background_match_the_cpu() {
    let d = dir("ref");
    let src = d.join("src.mkv");
    // RGB, lossless: the reference reads the same values the GPU does.
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={W}x{H}:rate=30,format=gbrp"),
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
        src.to_str().expect("utf-8"),
    ]);

    // Solids over the promoted video, at frame 12.
    let comp = Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [W, H], "fps": 30, "duration": 1.0 },
            "layers": [
                { "source": { "video": "src.mkv" } },
                { "source": { "solid": "#ff0000" }, "size": [100, 60],
                  "transform": { "anchor": [0, 0], "position": [20, 20], "opacity": "0.5" } },
                { "source": { "solid": "#3399ff" }, "size": [120, 100], "blend": "multiply",
                  "transform": { "anchor": [0, 0], "position": [200, "300 + 100 * clip(t - 0.4, 0, 0)"] } },
            ],
        }),
        &d,
    )
    .expect("comp");
    let out = d.join("a.png");
    let base = render(&comp, 12, &out, &d);
    assert!(matches!(base, Base::Video { .. }), "promoted");
    let mut want = rgb(&src, &["-vf", "select=eq(n\\,12)"]);
    over(
        &mut want,
        [20, 20, 100, 60],
        [1.0, 0.0, 0.0],
        0.5,
        BlendMode::Normal,
    );
    over(
        &mut want,
        [200, 300, 120, 100],
        [0.2, 0.6, 1.0],
        1.0,
        BlendMode::Multiply,
    );
    let got = rgb(&out, &[]);
    let db = psnr(&got, &want);
    eprintln!("video + solids: {db:.1} dB");
    assert!(db >= 50.0, "video + solids: {db:.1} dB");

    // Solids over the canvas background, one faded halfway to white.
    let comp = Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [W, H], "fps": 30, "duration": 1.0, "background": "#336699" },
            "layers": [
                { "source": { "solid": "#00cc00" }, "size": [200, 200], "fade": 0.5,
                  "fade_colour": "white", "transform": { "anchor": [0, 0], "position": [40, 100] } },
                { "source": { "solid": [1, 1, 0] }, "size": [80, 40], "blend": "screen",
                  "transform": { "anchor": [0, 0], "position": [100, 160] } },
            ],
        }),
        &d,
    )
    .expect("comp");
    let out = d.join("b.png");
    assert_eq!(render(&comp, 0, &out, &d), Base::Black);
    let bg = [0x33, 0x66, 0x99].map(|v: u8| v);
    let mut want: Vec<u8> = bg.repeat((W * H) as usize);
    over(
        &mut want,
        [40, 100, 200, 200],
        [0.5, 0.9, 0.5],
        1.0,
        BlendMode::Normal,
    );
    over(
        &mut want,
        [100, 160, 80, 40],
        [1.0, 1.0, 0.0],
        1.0,
        BlendMode::Screen,
    );
    let got = rgb(&out, &[]);
    let db = psnr(&got, &want);
    eprintln!("background + solids: {db:.1} dB");
    assert!(db >= 50.0, "background + solids: {db:.1} dB");
    let _ = std::fs::remove_dir_all(&d);
}

/// The canvas fade covers the composite whatever the blend modes: multiply and difference layers
/// faded to 0.6 towards black are 0.6 of their composite, and fade 0 to white is all white.
#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn canvas_fade_covers_every_blend_mode() {
    let d = dir("fade");
    let bg = [0x33u8, 0x66, 0x99];
    let faded = |fade: f64, colour: &str, name: &str| {
        let comp = Comp::from_value(
            &json!({
                "schema": "ffcine.comp/1",
                "canvas": { "size": [W, H], "fps": 30, "duration": 1.0, "background": "#336699",
                    "fade": fade, "fade_colour": colour },
                "layers": [
                    { "source": { "solid": "#808080" }, "size": [200, 200], "blend": "multiply",
                      "transform": { "anchor": [0, 0], "position": [40, 100] } },
                    { "source": { "solid": "#808080" }, "size": [80, 40], "blend": "difference",
                      "transform": { "anchor": [0, 0], "position": [260, 160] } },
                ],
            }),
            &d,
        )
        .expect("comp");
        let out = d.join(name);
        render(&comp, 0, &out, &d);
        rgb(&out, &[])
    };
    let mut want: Vec<u8> = bg.repeat((W * H) as usize);
    over(
        &mut want,
        [40, 100, 200, 200],
        [128.0 / 255.0; 3],
        1.0,
        BlendMode::Multiply,
    );
    over(
        &mut want,
        [260, 160, 80, 40],
        [128.0 / 255.0; 3],
        1.0,
        BlendMode::Difference,
    );
    over(&mut want, [0, 0, W, H], [0.0; 3], 0.4, BlendMode::Normal);
    let got = faded(0.6, "black", "c.png");
    let db = psnr(&got, &want);
    eprintln!("canvas fade over blends: {db:.1} dB");
    assert!(db >= 50.0, "canvas fade over blends: {db:.1} dB");
    let got = faded(0.0, "white", "d.png");
    assert!(
        got.iter().all(|v| *v == 255),
        "fade 0 to white is all white"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// An ellipse mask's soft edge is `feather` px wide on both axes, not only the short one: on a
/// 200x100 white ellipse feathered 20 px over black, 4.5 px inside the edge reads the same on
/// the long axis as on the short one (0.725).
#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn ellipse_feather_is_even_all_round() {
    let d = dir("ellipse");
    let comp = Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [W, H], "fps": 30, "duration": 1.0 },
            "layers": [
                { "source": { "solid": "white" }, "size": [200, 100],
                  "mask": { "shape": "ellipse", "feather": 20 },
                  "transform": { "anchor": [0, 0], "position": [40, 100] } },
            ],
        }),
        &d,
    )
    .expect("comp");
    let out = d.join("e.png");
    render(&comp, 0, &out, &d);
    let got = rgb(&out, &[]);
    let at = |x: u32, y: u32| f64::from(got[((y * W + x) * 3) as usize]);
    // The box middle is (140, 150); pixel centres 95.5 px across and 45.5 px down from it.
    let (long, short) = (at(235, 150), at(140, 195));
    let want = 0.725 * 255.0;
    eprintln!("ellipse feather: {long} across, {short} down, want {want:.0}");
    assert!((long - want).abs() <= 4.0 && (short - want).abs() <= 4.0);
    let _ = std::fs::remove_dir_all(&d);
}

/// Peak `memory.used` (MiB) over the GPU while `f` runs, minus the level before it.
fn vram_peak(f: impl FnOnce()) -> u64 {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let used = || -> u64 {
        Command::new("nvidia-smi")
            .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()?
                    .trim()
                    .parse()
                    .ok()
            })
            .unwrap_or(0)
    };
    let before = used();
    let (done, peak) = (AtomicBool::new(false), AtomicU64::new(before));
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                peak.fetch_max(used(), Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        f();
        done.store(true, Ordering::Relaxed);
    });
    peak.load(Ordering::Relaxed).saturating_sub(before)
}

/// fps and VRAM for 2, 4 and 8 video layers (1080p `testsrc2` H.264, each its own decoder) at
/// 1080x1920 and 2160x3840, NVENC HEVC out (`cargo test -p ffcine --release --test comp_gpu --
/// --ignored --nocapture bench`). Prints one line per case; results go in docs/compositing.md.
#[test]
#[ignore = "bench: needs a Vulkan GPU, NVENC and ffmpeg"]
fn bench_layers() {
    let d = dir("bench");
    let src = d.join("src.mp4");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=30",
        "-frames:v",
        "150",
        "-pix_fmt",
        "yuv420p",
        "-c:v",
        "libx264",
        "-g",
        "30",
        src.to_str().expect("utf-8"),
    ]);
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
        rate: None,
    };
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
    let (mut tried, mut failed) = (0, 0);
    for size in [[1080u32, 1920u32], [2160, 3840]] {
        for n in [2usize, 4, 8] {
            // A column of n tiles with gutters, rounded, sliding and fading in.
            let gap = 12.0;
            let th = (1920.0 - (n as f64 - 1.0) * gap) / n as f64;
            let layers: Vec<_> = (0..n)
                .map(|i| {
                    json!({ "source": { "video": "src.mp4" }, "source_in": i as f64 * 0.1,
                        "size": [1080, th], "radius": 24,
                        "fade": format!("ease_out((t - {}) / 0.4)", i as f64 * 0.1),
                        "transform": { "anchor": [0, 0],
                            "position": [format!("40 * sin(t + {i})"), i as f64 * (th + gap)] } })
                })
                .collect();
            let comp = Comp::from_value(
                &json!({ "schema": "ffcine.comp/1",
                    "canvas": { "size": [1080, 1920], "fps": 30, "duration": FRAMES as f64 / 30.0,
                                "background": "white" },
                    "layers": layers }),
                &d,
            )
            .expect("comp");
            let out_size = Size {
                width: size[0],
                height: size[1],
            };
            let piece = comp.lower(0..FRAMES, out_size, &media).expect("lower");
            let out = d.join(format!("bench-{n}-{}.mp4", size[1]));
            let t0 = std::time::Instant::now();
            let mut res = Ok(());
            tried += 1;
            let vram = vram_peak(|| {
                res = r.render_piece(
                    &piece,
                    30.0,
                    &out,
                    EncoderSettings::nvenc_hevc_master(),
                    None,
                );
            });
            let secs = t0.elapsed().as_secs_f64();
            if let Err(e) = res {
                // Out of VRAM on a shared GPU, say: report it and go on.
                eprintln!("{}x{}: {n} video layers: failed: {e}", size[0], size[1]);
                failed += 1;
                continue;
            }
            eprintln!(
                "{}x{}: {n} video layers, {FRAMES} frames in {secs:.2} s = {:.1} fps (with setup), VRAM +{vram} MiB",
                size[0],
                size[1],
                FRAMES as f64 / secs
            );
        }
    }
    let _ = std::fs::remove_dir_all(&d);
    assert!(failed < tried, "no case rendered");
}
