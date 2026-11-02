//! A breakdown of a made-up clip with known cuts, moves and beat: four shots (static, pan,
//! push, dark static) cut on the beat of a 120 BPM click. Skips without `ffmpeg` on `PATH`.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use ffcine_breakdown::{Breakdown, MotionKind, Options, run};

const W: usize = 320;
const H: usize = 180;
const FPS: usize = 25;

/// A textured picture (no repeats over the frame) tinted `tint`, sampled at `(x, y)`.
fn texture(x: f32, y: f32, seed: f32, tint: [f32; 3]) -> [u8; 3] {
    let v = (x * 0.11 + seed).sin() * (y * 0.083 - seed).cos() * 0.35
        + (x * 0.023 + y * 0.041 + seed * 2.0).sin() * 0.3
        + ((x * 0.007).sin() + (y * 0.013 + seed).cos()) * 0.15;
    let l = (0.5 + v).clamp(0.0, 1.0);
    tint.map(|t| (l * t * 255.0).round() as u8)
}

/// Frame `i` of the clip.
fn frame(i: usize) -> Vec<u8> {
    let t = i as f32 / FPS as f32;
    let mut out = Vec::with_capacity(W * H * 3);
    for y in 0..H {
        for x in 0..W {
            let (x, y) = (x as f32, y as f32);
            let px = if t < 2.0 {
                texture(x, y, 0.0, [1.0, 0.9, 0.8])
            } else if t < 4.0 {
                // Content moves left 3 px a frame: the camera pans right.
                let s = (t - 2.0) * FPS as f32 * 3.0;
                texture(x + s, y, 1.7, [0.7, 0.9, 1.0])
            } else if t < 6.0 {
                // Content grows 1 % a frame: a push in.
                let z = 1.01f32.powf((t - 4.0) * FPS as f32);
                let (cx, cy) = (W as f32 / 2.0, H as f32 / 2.0);
                texture(cx + (x - cx) / z, cy + (y - cy) / z, 3.1, [0.9, 1.0, 0.7])
            } else {
                texture(x, y, 4.4, [0.25, 0.22, 0.3])
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

fn make_clip(path: &Path) -> bool {
    let size = format!("{W}x{H}");
    let rate = FPS.to_string();
    let Ok(mut child) = Command::new("ffmpeg")
        .args([
            "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s",
        ])
        .args([
            size.as_str(),
            "-r",
            rate.as_str(),
            "-i",
            "-",
            "-f",
            "lavfi",
            "-i",
        ])
        // A 1 kHz click every 0.5 s (120 BPM).
        .arg("aevalsrc=0.8*sin(2*PI*1000*t)*exp(-60*mod(t\\,0.5)):s=44100:d=7.5")
        .args(["-c:v", "mpeg4", "-q:v", "2", "-c:a", "aac", "-shortest"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
    else {
        return false;
    };
    let mut stdin = child.stdin.take().expect("stdin");
    for i in 0..(FPS * 15 / 2) {
        stdin.write_all(&frame(i)).expect("write frame");
    }
    drop(stdin);
    assert!(child.wait().expect("ffmpeg").success(), "ffmpeg failed");
    true
}

#[test]
fn breaks_down_a_synthetic_clip() {
    let dir = std::env::temp_dir().join(format!("ffcine-breakdown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let clip = dir.join("clip.mp4");
    if !make_clip(&clip) {
        eprintln!("skipped: no ffmpeg");
        return;
    }
    let out = dir.join("out");
    let mut o = Options::new(&clip, &out);
    o.gpu = false;
    o.whisper = None;
    o.ocr = false;
    o.keyframe_size = 320;
    let b = run(&o).expect("breakdown");

    let cuts: Vec<f64> = b.cuts.iter().map(|c| (c * 10.0).round() / 10.0).collect();
    assert_eq!(cuts, [2.0, 4.0, 6.0], "{:?}", b.cuts);
    assert_eq!(b.shots.len(), 4);
    assert_eq!(b.frames, 187);
    assert!((b.shots[0].duration - 2.0).abs() < 0.05);
    let kinds: Vec<MotionKind> = b.shots.iter().map(|s| s.motion.kind).collect();
    assert_eq!(
        kinds,
        [
            MotionKind::Static,
            MotionKind::Pan,
            MotionKind::Push,
            MotionKind::Static
        ],
        "{:#?}",
        b.shots.iter().map(|s| &s.motion).collect::<Vec<_>>()
    );
    assert_eq!(b.shots[1].motion.direction.as_deref(), Some("right"));
    // 3 px x 50 frames over 320 px.
    assert!(
        (b.shots[1].motion.pan - 0.47).abs() < 0.05,
        "{:?}",
        b.shots[1].motion
    );
    assert!(b.shots[2].motion.zoom > 1.4, "{:?}", b.shots[2].motion);
    assert!(b.shots[3].colour.luma < b.shots[0].colour.luma / 2.0);

    let m = b.music.as_ref().expect("music");
    assert!((m.tempo - 120.0).abs() < 3.0, "{m:?}");
    assert_eq!((m.on_beat, m.cuts), (3, 3), "{m:?}");
    assert_eq!(b.shots[1].cut_on_beat, Some(true));
    assert!((b.shots[0].beats.expect("beats") - 4.0).abs() < 0.2);

    for s in &b.shots {
        for f in [&s.keyframes.key, &s.keyframes.first, &s.keyframes.last] {
            assert!(out.join(f).is_file(), "{f}");
        }
    }
    assert_eq!(b.contact_sheets, ["contact-sheet.jpg"]);
    let sheet = image::open(out.join("contact-sheet.jpg")).expect("sheet");
    assert!(sheet.width() > 1000);
    let md = std::fs::read_to_string(out.join("shotlist.md")).expect("shotlist");
    assert!(
        md.contains("| 2 | 0:02.00 ♪ | 2.00 s | pan right |"),
        "{md}"
    );
    assert_eq!(md.lines().filter(|l| l.starts_with("| ")).count(), 5);
    let back = Breakdown::load(&out.join("breakdown.json")).expect("json");
    assert_eq!(back, b);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
