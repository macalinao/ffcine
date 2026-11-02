//! Layer compositing on the GPU (`cargo test -p ffcine --test layers_gpu -- --ignored`; needs a
//! Vulkan GPU and `ffmpeg` on PATH, as in the dev shell): a video layer and a still image drawn
//! over the main picture with opacity and rounded corners; every blend mode against the CPU
//! reference; turned layers; a still on a PQ output. Read back as RGB (or 10-bit luma).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use ffcine::shaders;
use ffcine::{
    BlendMode, Crop, EncoderSettings, Grade, Hook, Image, Layer, LayerSource, ParamTrack, Pipeline,
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
                rotation: Vec::new(),
                blend: BlendMode::Normal,
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
                rotation: Vec::new(),
                blend: BlendMode::Normal,
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

/// A solid `colour` clip of `frames` frames, `W x H`.
fn solid_n(path: &Path, colour: &str, frames: u64) {
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c={colour}:size={W}x{H}:rate=24,format=yuv420p"),
        "-frames:v",
        &frames.to_string(),
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

fn image_layer(data: Vec<u8>, width: u32, from: i64, rect: Option<Crop>) -> Layer {
    let height = u32::try_from(data.len()).expect("size") / 4 / width;
    let (tw, th) = rect.map_or((W, H), |r| (r.width as u32, r.height as u32));
    Layer {
        source: LayerSource::Image(Arc::new(Image {
            width,
            height,
            data,
        })),
        grade: Grade {
            dither: None,
            peak_detect: false,
            size: Some(Size {
                width: tw,
                height: th,
            }),
            ..Grade::default()
        },
        from,
        to: from + 1,
        rects: rect.into_iter().collect(),
        rotation: Vec::new(),
        blend: BlendMode::Normal,
    }
}

fn unit(c: [i32; 3]) -> [f32; 3] {
    c.map(|v| v as f32 / 255.0)
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn blend_modes_match_the_cpu_reference() {
    let d = dir();
    let modes = BlendMode::ALL;
    let n = modes.len() as u64 + 1;
    let base = d.join("base.mkv");
    solid_n(&base, "0x2880C8", n);
    let s = [240u8, 90, 30];
    // One mode per frame, full frame and opaque; the last frame is the backdrop alone.
    let layers = modes
        .iter()
        .enumerate()
        .map(|(i, m)| Layer {
            blend: *m,
            ..image_layer([s[0], s[1], s[2], 255].repeat(4), 2, i as i64, None)
        })
        .collect();
    let out = d.join("blend.mkv");
    Pipeline::new(&base, &out)
        .range(Duration::ZERO, Some(n))
        .grade(Grade {
            dither: None,
            peak_detect: false,
            ..Grade::default()
        })
        .layers(layers)
        .encoder(EncoderSettings {
            rate: RateControl::Crf(0.0),
            pixel_format: ffcine::PixelFormat::Yuv420p,
            ..EncoderSettings::x264_preview()
        })
        .run(&backend::Ffmpeg::new().expect("backend"))
        .expect("render");
    let b = px(&rgb(&out, n - 1), W / 2, H / 2);
    let src = unit(s.map(i32::from));
    for (i, m) in modes.iter().enumerate() {
        let got = px(&rgb(&out, i as u64), W / 2, H / 2);
        let want = m
            .composite(unit(b), src, 1.0)
            .map(|v| (v * 255.0).round() as i32);
        assert!(
            near(got, want, 4),
            "{m}: got {got:?}, want {want:?} over {b:?}"
        );
    }
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
fn rotated_layer_keeps_corners_transparent() {
    let d = dir();
    let base = d.join("grey.mkv");
    solid_n(&base, "0x808080", 2);
    // Red on the left half, green on the right, 100 x 40 in the middle of the frame.
    let data = [
        [255, 0, 0, 255],
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 255, 0, 255],
    ]
    .concat();
    let rect = Crop {
        x: 110.0,
        y: 70.0,
        width: 100.0,
        height: 40.0,
    };
    let turned = Layer {
        rotation: vec![90.0],
        ..image_layer(data.clone(), 4, 0, Some(rect))
    };
    let half = Layer {
        rotation: vec![45.0],
        blend: BlendMode::Multiply,
        ..image_layer(data, 4, 1, Some(rect))
    };
    let out = d.join("rot.mkv");
    Pipeline::new(&base, &out)
        .range(Duration::ZERO, Some(2))
        .grade(Grade {
            dither: None,
            peak_detect: false,
            ..Grade::default()
        })
        .layers(vec![turned, half])
        .encoder(EncoderSettings {
            rate: RateControl::Crf(0.0),
            pixel_format: ffcine::PixelFormat::Yuv420p,
            ..EncoderSettings::x264_preview()
        })
        .run(&backend::Ffmpeg::new().expect("backend"))
        .expect("render");
    let f0 = rgb(&out, 0);
    let (cx, cy) = (W / 2, H / 2);
    let grey = px(&f0, 2, 2);
    // Turned a quarter counter-clockwise: the left (red) end points down, the right up.
    assert!(
        near(px(&f0, cx, cy + 35), [255, 0, 0], 8),
        "{:?}",
        px(&f0, cx, cy + 35)
    );
    assert!(
        near(px(&f0, cx, cy - 35), [0, 255, 0], 8),
        "{:?}",
        px(&f0, cx, cy - 35)
    );
    // Inside the unturned rectangle but outside the turned one: the picture shows through.
    assert!(
        near(px(&f0, cx + 40, cy), grey, 4),
        "{:?}",
        px(&f0, cx + 40, cy)
    );
    assert!(near(px(&f0, cx - 40, cy), grey, 4));
    // Multiplied at 45°: red times grey along the diagonal, grey past the corners.
    let f1 = rgb(&out, 1);
    let m = BlendMode::Multiply
        .composite(unit(grey), [1.0, 0.0, 0.0], 1.0)
        .map(|v| (v * 255.0).round() as i32);
    assert!(
        near(px(&f1, cx - 20, cy + 20), m, 8),
        "{:?}",
        px(&f1, cx - 20, cy + 20)
    );
    assert!(
        near(px(&f1, cx - 45, cy - 15), grey, 4),
        "{:?}",
        px(&f1, cx - 45, cy - 15)
    );
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn hdr_output_puts_stills_at_reference_white() {
    let d = dir();
    let base = d.join("black2.mkv");
    solid_n(&base, "black", 1);
    let pq = ffcine::ColorSpace {
        primaries: ffcine::color::Primaries::Bt2020,
        transfer: ffcine::color::Transfer::Pq,
        matrix: ffcine::color::Matrix::Bt2020Ncl,
        range: ffcine::color::Range::Limited,
    };
    let out = d.join("hdr.mkv");
    Pipeline::new(&base, &out)
        .range(Duration::ZERO, Some(1))
        .grade(Grade {
            dither: None,
            peak_detect: false,
            output: pq,
            ..Grade::default()
        })
        .layers(vec![image_layer([255; 4].repeat(4), 2, 0, None)])
        .encoder(EncoderSettings {
            color: pq,
            ..EncoderSettings::nvenc_hevc_master()
        })
        .run(&backend::Ffmpeg::new().expect("backend"))
        .expect("render");
    // 10-bit luma of the middle pixel.
    let y = ffmpeg(&[
        "-i",
        out.to_str().expect("utf-8"),
        "-f",
        "rawvideo",
        "-pix_fmt",
        "gray10le",
        "-",
    ]);
    let i = ((H / 2 * W + W / 2) * 2) as usize;
    let v = i32::from(u16::from_le_bytes([y[i], y[i + 1]]));
    // libplacebo's SDR reference white (PQ 0.58-0.61; an SDR white video lands at 593 here),
    // not PQ 1.0 (940, 10000 nits) as when the still was taken as the output's space.
    assert!((560..=620).contains(&v), "luma {v}");
}
