//! Time remapping (`docs/compositing.md#time-remapping`) against ffmpeg (`cargo test -p
//! ffcine --test time_remap -- --ignored`; `ffmpeg` on PATH, as in the dev shell; the render
//! cases also need a Vulkan GPU and NVENC):
//!
//! - the frame maps a comp lowers for a speed, keyed speeds and a source expression pick the
//!   source frames `setpts` + `fps` do, read back from a source whose frames carry their index;
//! - a reversed layer renders the frames of the source run through ffmpeg's `reverse`, also when
//!   the source has a display matrix (the prep keeps it, so the decoder turns both alike);
//! - a ramped layer rendered in two pieces equals the one-piece render.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::single_range_in_vec_init
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::assembly::{CompMedia, FsFingerprinter, Progress, Renderer};
use ffcine::comp::{Comp, Media};
use ffcine::{ColorSpace, Decode, Device, EncoderSettings, Size, backend};
use serde_json::{Value, json};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-time-{name}-{}", std::process::id()));
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

fn comp(fps: u32, frames: u32, layer: &Value) -> Comp {
    Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [320, 180], "fps": fps.to_string(),
                "duration": f64::from(frames) / f64::from(fps), "background": "black" },
            "layers": [layer],
        }),
        Path::new("/"),
    )
    .expect("comp")
}

/// A 30 fps source of `frames` frames, known to the comp without probing.
struct Known(u64);

impl Media for Known {
    fn dims(&self, _: &Path) -> ffcine::Result<(u32, u32)> {
        Ok((320, 180))
    }
    fn identity(&self, p: &Path) -> Value {
        json!(p)
    }
    fn video_rate(&self, _: &Path) -> ffcine::Result<(f64, u64)> {
        Ok((30.0, self.0))
    }
}

/// The absolute source frames a comp's single video layer shows over `0..frames`.
fn picks(c: &Comp, frames: i64, media: &Known) -> Vec<i64> {
    let size = Size {
        width: 320,
        height: 180,
    };
    let p = c.lower(0..frames, size, media).expect("lower");
    let ffcine::LayerSource::Video { start, map, .. } = &p.layers[0].source else {
        panic!("video layer");
    };
    let lo = if start.is_zero() {
        0
    } else {
        (start.as_secs_f64() * 30.0 + 0.5).round() as i64
    };
    map.as_ref().expect("map").iter().map(|k| k + lo).collect()
}

/// The frame indices `setpts=<expr>,fps=<fps>` shows of a 30 fps, `n`-frame source whose
/// frames carry their index as their 16-bit grey level.
fn oracle(n: u32, setpts: &str, fps: u32) -> Vec<i64> {
    let src = format!(
        "nullsrc=s=16x16:r=30:d={},format=gray16le,geq=lum=N",
        f64::from(n) / 30.0
    );
    let vf = format!("settb=1/30000,setpts={setpts},fps={fps}");
    let raw = ffmpeg(&[
        "-f", "lavfi", "-i", &src, "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "gray16le", "-",
    ]);
    raw.as_chunks::<{ 16 * 16 * 2 }>()
        .0
        .iter()
        .map(|f| i64::from(u16::from_le_bytes([f[0], f[1]])))
        .collect()
}

#[test]
#[ignore = "needs ffmpeg"]
fn frame_maps_match_setpts_and_fps() {
    let n = 300;
    let media = Known(u64::from(n));
    for (time, setpts, fps) in [
        (json!({ "speed": 2 }), "PTS/2", 30),
        (json!({ "speed": 3 }), "PTS/3", 30),
        (json!({ "speed": 0.5 }), "PTS*2", 30),
        (json!({ "speed": 0.4 }), "PTS/0.4", 24),
        (json!({ "speed": 1.7 }), "PTS/1.7", 60),
        (json!({ "source": "t * 2.5" }), "PTS/2.5", 30),
        // speed 1 → 3 over 2 s, then 3: s(L) = L + L²/2, then 4 + 3 (L − 2); its inverse.
        (
            json!({ "speed": [{ "t": 0, "v": 1 }, { "t": 2, "v": 3 }] }),
            "if(lte(T\\,4)\\,-1+sqrt(1+2*T)\\,2+(T-4)/3)/TB",
            30,
        ),
        // The same ramp as an expression (numeric integral).
        (
            json!({ "speed": "1 + min(lt, 2)" }),
            "if(lte(T\\,4)\\,-1+sqrt(1+2*T)\\,2+(T-4)/3)/TB",
            30,
        ),
    ] {
        let want = oracle(n, setpts, fps);
        let frames = want.len().min(240) as i64;
        let c = comp(
            fps,
            frames as u32,
            &json!({ "source": { "video": "src.mkv" }, "time": time }),
        );
        let got = picks(&c, frames, &media);
        assert_eq!(got, want[..frames as usize], "{time}");
        eprintln!("{time}: {frames} frames match");
    }
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
    let ff = backend::Ffmpeg::new().expect("backend");
    let probe = |p: &Path| {
        let info = ffcine::Backend::probe(&ff, p)?;
        let (_, v) = info.video().expect("video");
        Ok((v.width, v.height))
    };
    let rate = |_: &Path| Ok((30.0, 60));
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
            .expect("render");
        let o = out.to_str().expect("utf-8");
        let lines = ffmpeg(&["-i", o, "-map", "0:v", "-f", "framemd5", "-"]);
        md5.extend(
            String::from_utf8_lossy(&lines)
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| l.rsplit(',').next().unwrap_or("").trim().to_owned()),
        );
    }
    md5
}

/// A 2 s, 30 fps `testsrc2` source (H.264, long GOP: the decoder has to seek) at `d/src.mp4`.
fn source(d: &Path) -> PathBuf {
    let src = d.join("src.mp4");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=320x180:r=30:d=2",
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

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn reverse_matches_ffmpeg_reverse() {
    let d = dir("reverse");
    let src = source(&d);
    let rev = d.join("rev.mkv");
    ffmpeg(&[
        "-i",
        src.to_str().expect("utf-8"),
        "-vf",
        "reverse",
        "-c:v",
        "ffv1",
        rev.to_str().expect("utf-8"),
    ]);
    let s = |p: &Path| p.to_str().expect("utf-8").to_owned();
    // From the last frame back: source_in is on frame 59.
    let reversed = comp(
        30,
        60,
        &json!({ "source": { "video": s(&src) }, "source_in": 59.0 / 30.0,
            "time": { "reverse": true } }),
    );
    // Speed 1: a layer too (a plain one would be promoted to the base, another path).
    let forward = comp(
        30,
        60,
        &json!({ "source": { "video": s(&rev) }, "time": { "speed": 1 } }),
    );
    let a = render(&reversed, &[0..60], &d, "reversed");
    let b = render(&forward, &[0..60], &d, "forward");
    assert_eq!(a.len(), 60);
    assert_eq!(a, b, "reversed layer vs ffmpeg reverse");
    let preps = std::fs::read_dir(d.join("cache/prep"))
        .expect("prep dir")
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".intra.mkv"))
        })
        .count();
    assert_eq!(preps, 1, "one all-intra prep");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn reverse_of_a_rotated_source_matches_ffmpeg_reverse() {
    let d = dir("rotated");
    let src = source(&d);
    let s = |p: &Path| p.to_str().expect("utf-8").to_owned();
    let rot = d.join("rot.mp4");
    ffmpeg(&[
        "-display_rotation",
        "90",
        "-i",
        &s(&src),
        "-c",
        "copy",
        &s(&rot),
    ]);
    // ffmpeg's reverse of the stored pixels, the matrix kept.
    let rev = d.join("rev.mkv");
    ffmpeg(&[
        "-noautorotate",
        "-i",
        &s(&rot),
        "-vf",
        "reverse",
        "-c:v",
        "ffv1",
        &s(&rev),
    ]);
    let layer = |p: &Path, time: Value, at: f64| {
        comp(
            30,
            60,
            &json!({ "source": { "video": s(p) }, "source_in": at, "time": time }),
        )
    };
    // The reversed layer reads the prep (stored pixels and the matrix); the decoder turns it
    // as it turns the source itself, so it shows the source upright, reversed.
    let a = render(
        &layer(&rot, json!({ "reverse": true }), 59.0 / 30.0),
        &[0..60],
        &d,
        "reversed",
    );
    let b = render(
        &layer(&rev, json!({ "speed": 1 }), 0.0),
        &[0..60],
        &d,
        "want",
    );
    assert_eq!(a.len(), 60);
    assert_eq!(a, b, "reversed rotated layer vs ffmpeg reverse");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn split_ramp_equals_the_whole() {
    let d = dir("split");
    let src = source(&d);
    let c = comp(
        30,
        40,
        &json!({ "source": { "video": src.to_str().expect("utf-8") },
            "time": { "speed": [{ "t": 0, "v": 0.5 }, { "t": 1, "v": 1.5 }] } }),
    );
    let whole = render(&c, &[0..40], &d, "whole");
    let split = render(&c, &[0..17, 17..40], &d, "split");
    assert_eq!(whole.len(), 40);
    assert_eq!(whole, split, "two pieces vs one");
    let _ = std::fs::remove_dir_all(&d);
}
