//! `rgb_gain`, `extra` filters and temporal blend on the GPU against ffmpeg's CPU filters (`cargo
//! test -p ffcine --test grade_extras_gpu -- --ignored`; needs a Vulkan GPU, NVENC and `ffmpeg` on
//! PATH): each graded frame is compared with the same filter run by ffmpeg on the same synthetic
//! source.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ffcine::assembly::{self, FsFingerprinter, Profile, Renderer};
use ffcine::timeline::{
    Clip, Effect, ExternalReference, GenericEffect, Item, MediaReference, Project, TimeRange,
    Timeline, Track, track_kind,
};
use ffcine::{
    ColorSpace, Decode, Device, EncoderSettings, Grade, Lut, LutStage, LutType, Pipeline, backend,
};
use serde_json::json;

const W: usize = 320;
const H: usize = 180;
const RATE: f64 = 24000.0 / 1001.0;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-{name}-{}", std::process::id()));
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

/// The source's first frame through `grade` on the GPU, as RGB24.
fn graded(ff: &backend::Ffmpeg, src: &Path, d: &Path, grade: Grade) -> Vec<u8> {
    let png = d.join("out.png");
    Pipeline::new(src, &png)
        .range(Duration::ZERO, Some(1))
        .grade(Grade {
            output: ColorSpace::BT709_RGB,
            dither: None,
            peak_detect: false,
            ..grade
        })
        .encoder(EncoderSettings::png_still())
        .run(ff)
        .expect("render");
    ffmpeg(&["-i", s(&png), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
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
fn rgb_gain_and_extra_match_ffmpeg() {
    let d = dir("grade-extras");
    // Smooth RGB colours (the blur keeps edges out of the comparison), lossless RGB.
    let src = d.join("src.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={W}x{H}:rate=24,gblur=sigma=3,format=gbrp"),
        "-frames:v",
        "1",
        "-c:v",
        "ffv1",
        s(&src),
    ]);
    let ff = backend::Ffmpeg::new().expect("backend");
    let plain = graded(&ff, &src, &d, Grade::default());
    let base = diff(&plain, &cpu(&src, "null"));
    eprintln!("identity: {base:?}");
    assert!(base.0 < 0.6, "identity {base:?}");

    // rgb_gain without a LUT, and ahead of one (a 17^3 LUT squaring red, inverting blue).
    let gain = [1.25, 1.0, 0.7];
    let g = graded(
        &ff,
        &src,
        &d,
        Grade {
            rgb_gain: Some(gain),
            ..Grade::default()
        },
    );
    let r = diff(&g, &cpu(&src, "colorchannelmixer=rr=1.25:gg=1:bb=0.7"));
    eprintln!("rgb_gain: {r:?}");
    assert!(r.0 < 1.0, "rgb_gain {r:?}");
    let mut lut = Lut::identity(17);
    for v in &mut lut.data {
        *v = [v[0] * v[0], v[1], 1.0 - v[2]];
    }
    let cube = d.join("t.cube");
    let mut text = String::from("LUT_3D_SIZE 17\n");
    for v in &lut.data {
        let _ = writeln!(text, "{} {} {}", v[0], v[1], v[2]);
    }
    std::fs::write(&cube, text).expect("cube");
    let g = graded(
        &ff,
        &src,
        &d,
        Grade {
            rgb_gain: Some(gain),
            lut: Some(LutStage {
                lut,
                kind: LutType::Normalized,
            }),
            ..Grade::default()
        },
    );
    let r = diff(
        &g,
        &cpu(
            &src,
            &format!(
                "colorchannelmixer=rr=1.25:gg=1:bb=0.7,lut3d=file={}:interp=trilinear",
                s(&cube)
            ),
        ),
    );
    eprintln!("rgb_gain + LUT: {r:?}");
    assert!(r.0 < 1.0, "rgb_gain + LUT {r:?}");

    for f in [
        "eq=contrast=1.2:brightness=0.03:saturation=1.3",
        "eq=gamma=1.3:gamma_weight=0.8",
        "hue=h=40:s=0.7",
        "hue=b=0.5",
        "colorbalance=rs=0.2:gm=-0.1:bh=0.15",
        "colorbalance=rs=0.2:bm=0.2:pl=1",
        "curves=preset=vintage",
        "curves=m=0/0 0.5/0.6 1/1:r=0/0.05 1/1",
        "unsharp=5:5:1.5",
        "vignette=PI/4",
        "eq=saturation=0.5,hue=h=90",
    ] {
        let (hooks, warn) = ffcine::extra::hooks(&[f.to_owned()]);
        assert!(warn.is_empty(), "{f}: {warn:?}");
        let g = graded(
            &ff,
            &src,
            &d,
            Grade {
                hooks,
                ..Grade::default()
            },
        );
        let r = diff(&g, &cpu(&src, &f.replace(' ', "\\ ")));
        let moved = diff(&g, &plain);
        eprintln!("{f}: {r:?} (changed {moved:?})");
        assert!(moved.0 > 1.0, "{f} changes the picture");
        assert!(r.0 < 1.5 && r.1 < 24, "{f}: {r:?}");
    }
}

fn clip(path: &Path, start: f64, frames: f64, blend: Option<u32>) -> Item {
    let mut c = Clip::new(
        "src",
        MediaReference::External(ExternalReference::from_path(path, None)),
        Some(TimeRange::from_frames(start, start + frames, RATE)),
    );
    if let Some(n) = blend {
        c.effects.push(Effect::Effect(GenericEffect::ffcine(
            "ffcine.time.blend/1",
            json!({"frames": n})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        )));
    }
    Item::Clip(c)
}

/// Every frame of `path` as RGB24 frames of `w x h`.
fn frames(path: &Path, w: usize, h: usize, vf: &str) -> Vec<Vec<u8>> {
    ffmpeg(&[
        "-i",
        s(path),
        "-vf",
        &format!("{vf}scale={w}:{h}"),
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "-",
    ])
    .chunks(w * h * 3)
    .map(<[u8]>::to_vec)
    .collect()
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn temporal_blend_matches_tmix_across_pieces() {
    let d = dir("blend");
    let src = d.join("src.mp4");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=24000/1001,format=yuv420p",
        "-t",
        "3",
        "-c:v",
        "libx264",
        "-crf",
        "10",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-color_range",
        "tv",
        s(&src),
    ]);
    let ff = backend::Ffmpeg::new().expect("backend");
    let render = |name: &str, items: Vec<Item>| {
        let mut v1 = Track::new("V1", track_kind::VIDEO);
        v1.children = items;
        let mut t = Timeline::new(name);
        t.set_project(&Project {
            rate: Some(RATE),
            ..Project::default()
        })
        .expect("project");
        t.tracks.children = vec![Item::Track(v1)];
        let profile = Profile::preview();
        let plan = assembly::plan(&t, &d, &profile, &FsFingerprinter, None).expect("plan");
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        let r = Renderer {
            backend: &ff,
            cache: d.join("cache"),
            ffmpeg: "ffmpeg".into(),
            device: Device::Auto,
            decode: Decode::default(),
            progress: assembly::Progress::Human,
            grader: None,
            detect: None,
        };
        let out = d.join(format!("{name}.mp4"));
        r.render(&plan, None, &out).expect("render");
        frames(&out, 480, 270, "")
    };
    let whole = render("whole", vec![clip(&src, 10.0, 30.0, Some(3))]);
    // The same frames as two pieces: the second's first frame averages frames before it.
    let split = render(
        "split",
        vec![
            clip(&src, 10.0, 15.0, Some(3)),
            clip(&src, 25.0, 15.0, Some(3)),
        ],
    );
    let plain = render("plain", vec![clip(&src, 10.0, 30.0, None)]);
    let tmix = frames(&src, 480, 270, "tmix=frames=3,");
    assert_eq!(whole.len(), 30);
    assert_eq!(split.len(), 30);
    for i in [0, 1, 14, 15, 16, 29] {
        let to_tmix = diff(&whole[i], &tmix[10 + i]);
        let pieces = diff(&whole[i], &split[i]);
        let unblended = diff(&whole[i], &plain[i]);
        eprintln!("frame {i}: tmix {to_tmix:?}, split {pieces:?}, unblended {unblended:?}");
        assert!(to_tmix.0 < 2.0, "frame {i} vs tmix {to_tmix:?}");
        assert!(pieces.0 < 1.5, "frame {i} split vs whole {pieces:?}");
        assert!(unblended.0 > 2.0 * to_tmix.0, "frame {i} is blended");
    }
}
