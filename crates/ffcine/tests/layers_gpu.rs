//! Layer compositing on the GPU (`cargo test -p ffcine --test layers_gpu -- --ignored`; needs a
//! Vulkan GPU and `ffmpeg` on PATH, as in the dev shell): a video layer and a still image drawn
//! over the main picture with opacity and rounded corners, read back as RGB.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use ffcine::shaders;
use ffcine::{
    Crop, EncoderSettings, Grade, Hook, Image, Layer, LayerSource, ParamTrack, Pipeline,
    RateControl, Size, backend,
};

const W: u32 = 320;
const H: u32 = 180;
const N: u64 = 4;

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-layers-gpu-{}", std::process::id()));
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

fn solid(path: &Path, colour: &str, w: u32, h: u32) {
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c={colour}:size={w}x{h}:rate=24,format=yuv420p"),
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
        path.to_str().expect("utf-8"),
    ]);
}

/// Frame `n` as RGB24.
#[allow(clippy::cast_possible_truncation)]
fn rgb(path: &Path, n: u64) -> Vec<u8> {
    let all = ffmpeg(&[
        "-i",
        path.to_str().expect("utf-8"),
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "-",
    ]);
    let size = (W * H * 3) as usize;
    all[size * n as usize..size * (n as usize + 1)].to_vec()
}

fn px(f: &[u8], x: u32, y: u32) -> [i32; 3] {
    let i = ((y * W + x) * 3) as usize;
    [f[i].into(), f[i + 1].into(), f[i + 2].into()]
}

fn near(a: [i32; 3], b: [i32; 3], tol: i32) -> bool {
    a.iter().zip(&b).all(|(x, y)| (x - y).abs() <= tol)
}

fn alpha(opacity: Vec<f32>, radius: f64) -> Hook {
    Hook {
        name: "alpha".into(),
        source: shaders::alpha().into(),
        params: vec![("radius".into(), radius)],
        tracks: vec![ParamTrack {
            name: "opacity".into(),
            values: opacity,
        }],
    }
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn video_and_image_layers_with_alpha() {
    let d = dir();
    let base = d.join("black.mkv");
    let red = d.join("red.mkv");
    solid(&base, "black", W, H);
    solid(&red, "red", 160, 90);
    let ff = backend::Ffmpeg::new().expect("backend");
    let layer_grade = |hooks| Grade {
        dither: None,
        peak_detect: false,
        size: Some(Size {
            width: 160,
            height: 90,
        }),
        hooks,
        ..Grade::default()
    };
    // Green, half transparent.
    let image = Image {
        width: 2,
        height: 2,
        data: [0, 255, 0, 128].repeat(4),
    };
    let out = d.join("out.mkv");
    Pipeline::new(&base, &out)
        .range(Duration::ZERO, Some(N))
        .grade(Grade {
            dither: None,
            peak_detect: false,
            ..Grade::default()
        })
        .layers(vec![
            Layer {
                source: LayerSource::Video {
                    input: red,
                    start: Duration::ZERO,
                },
                // Opaque on frame 0, half from frame 1; corners rounded by 20 px.
                grade: layer_grade(vec![alpha(vec![1.0, 0.5], 20.0)]),
                from: 0,
                to: 4,
                rects: vec![Crop {
                    x: 0.0,
                    y: 0.0,
                    width: 160.0,
                    height: 90.0,
                }],
            },
            Layer {
                source: LayerSource::Image(Arc::new(image)),
                grade: layer_grade(Vec::new()),
                from: 2,
                to: 4,
                rects: vec![Crop {
                    x: 160.0,
                    y: 90.0,
                    width: 160.0,
                    height: 90.0,
                }],
            },
        ])
        .encoder(EncoderSettings {
            rate: RateControl::Crf(0.0),
            pixel_format: ffcine::PixelFormat::Yuv420p,
            ..EncoderSettings::x264_preview()
        })
        .run(&ff)
        .expect("render");
    let f0 = rgb(&out, 0);
    let f1 = rgb(&out, 1);
    let f2 = rgb(&out, 2);
    eprintln!(
        "f0 mid {:?} corner {:?}; f1 mid {:?}; f2 image {:?} outside {:?}",
        px(&f0, 80, 45),
        px(&f0, 1, 1),
        px(&f1, 80, 45),
        px(&f2, 240, 135),
        px(&f2, 240, 40)
    );
    assert!(near(px(&f0, 80, 45), [255, 0, 0], 6), "opaque layer");
    assert!(near(px(&f0, 1, 1), [0, 0, 0], 6), "rounded corner");
    assert!(near(px(&f1, 80, 45), [128, 0, 0], 8), "half opacity");
    assert!(near(px(&f0, 240, 135), [0, 0, 0], 6), "image not yet shown");
    assert!(near(px(&f2, 240, 135), [0, 128, 0], 8), "image alpha");
    assert!(near(px(&f2, 240, 40), [0, 0, 0], 6), "outside the layers");
}
