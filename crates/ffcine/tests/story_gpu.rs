//! The `story` template rendered end to end (`cargo test -p ffcine --test story_gpu -- --ignored
//! --test-threads=1`; needs a Vulkan GPU, NVENC and `ffmpeg` on PATH): three tiles of synthetic
//! media (solid colours, and a grey ramp whose level is its frame number), planned with
//! `plan_comp` and rendered like `ffcine comp render`. Sampled frames are checked against the
//! tile contents the template's timing implies: black at the fade-in start, a slide half done
//! mid-swap, the speed ramp's source frame mid-ramp, and black on the loop's last frame.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::assembly::{self, CompMedia, FsFingerprinter, Profile, Progress, Renderer};
use ffcine::comp::Comp;
use ffcine::{Backend, Decode, Device, backend};
use serde_json::json;

/// The grey ramp's level per source frame.
const STEP: f64 = 2.0;

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

/// A lossless RGB video of `filter`, 30 fps, `secs` long.
fn video(d: &Path, name: &str, filter: &str, secs: u32) -> PathBuf {
    let p = d.join(format!("{name}.mkv"));
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("{filter},format=gbrp"),
        "-t",
        &secs.to_string(),
        "-c:v",
        "ffv1",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-colorspace",
        "rgb",
        p.to_str().expect("utf-8"),
    ]);
    p
}

fn still(d: &Path, name: &str, colour: &str) -> PathBuf {
    let p = d.join(format!("{name}.png"));
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c={colour}:size=320x180"),
        "-frames:v",
        "1",
        p.to_str().expect("utf-8"),
    ]);
    p
}

/// Frame `n` of `path` as RGB, and its width.
fn frame(path: &Path, n: i64) -> (Vec<u8>, usize) {
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe");
    let w = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse()
        .expect("width");
    let f = ffmpeg(&[
        "-i",
        path.to_str().expect("utf-8"),
        "-vf",
        &format!("select=eq(n\\,{n})"),
        "-frames:v",
        "1",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "-",
    ]);
    (f, w)
}

/// The pixel at `(x, y)` given as fractions of the canvas.
fn at(f: &(Vec<u8>, usize), x: f64, y: f64) -> [i32; 3] {
    let (buf, w) = f;
    let h = buf.len() / 3 / w;
    let (x, y) = ((x * *w as f64) as usize, (y * h as f64) as usize);
    let i = (y * w + x) * 3;
    [buf[i].into(), buf[i + 1].into(), buf[i + 2].into()]
}

fn near(a: [i32; 3], b: [i32; 3], tol: i32) -> bool {
    a.iter().zip(&b).all(|(x, y)| (x - y).abs() <= tol)
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
#[allow(clippy::too_many_lines)]
fn story_frames_show_the_expected_tiles() {
    let d = std::env::temp_dir().join(format!("ffcine-story-gpu-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    let red = video(&d, "red", "color=c=red:size=640x360:rate=30", 6);
    let yellow = video(&d, "yellow", "color=c=yellow:size=640x360:rate=30", 6);
    // Level 2n on frame n: the speed ramp's pick is readable off the picture.
    let ramp = video(
        &d,
        "ramp",
        "color=c=black:size=640x360:rate=30,geq=r='2*N':g='2*N':b='2*N'",
        4,
    );
    let blue = still(&d, "blue", "blue");
    let green = still(&d, "green", "lime");
    let magenta = still(&d, "magenta", "magenta");
    let p = |p: &Path| p.to_str().expect("utf-8").to_owned();
    let doc = json!({
        "schema": "ffcine.comp/1",
        "template": { "story": {
            "size": [360, 640], "fps": "30", "duration": 6.0,
            "tiles": 3, "gutter": 4, "radius": 8,
            "background": { "from": "black", "to": "white", "duration": 1.0 },
            "enter": { "fade": 0.4, "stagger": 0.25 },
            "swap": { "at": 2.5, "stagger": 0.12, "slide": 0.4, "direction": "left" },
            "exit": { "at": 5.2, "stagger": 0.12, "slide": 0.4, "fade": 0.4 },
            "panels": [
                [ { "video": p(&red) }, { "still": p(&blue) } ],
                [ { "still": p(&green) }, { "video": p(&yellow) } ],
                [ { "video": p(&ramp), "speed": [[0, 1], [1.0, 2], [2.0, 1]] },
                  { "still": p(&magenta) } ] ] } }
    });
    let comp = Comp::from_value(&doc, &d).expect("comp");

    let ff = backend::Ffmpeg::new().expect("backend");
    let dims = |p: &Path| {
        let info = ff.probe(p)?;
        let (_, v) = info.video().expect("video");
        Ok((v.width, v.height))
    };
    let rate = |p: &Path| {
        let info = ff.probe(p)?;
        let (_, v) = info.video().expect("video");
        let fps = v.frame_rate.expect("rate").as_f64();
        let n = info.duration.expect("duration").as_secs_f64() * fps;
        Ok((fps, n.round() as u64))
    };
    let media = CompMedia {
        fp: &FsFingerprinter,
        dims: &dims,
        profile: None,
        rate: Some(&rate),
    };
    let profile = Profile::for_canvas("preview", 360, 640).expect("profile");
    let plan = assembly::plan_comp(&comp, &profile, &media).expect("plan");
    assert_eq!(plan.frames(), 180);
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
    let out = d.join("story.mp4");
    r.render(&plan, None, &out).expect("render");

    // Tile centres (3 tiles of 210 px with 4 px gutters on 640): 105, 319, 533.
    let rows = [105.0 / 640.0, 319.0 / 640.0, 533.0 / 640.0];
    let black = [0, 0, 0];

    // Fade-in start: background black, every tile faded out.
    let f = frame(&out, 0);
    for y in rows {
        for x in [0.1, 0.5, 0.9] {
            assert!(
                near(at(&f, x, y), black, 12),
                "frame 0 at {x},{y}: {:?}",
                at(&f, x, y)
            );
        }
    }

    // Before the swap: each tile's first item, faded in, over a white background.
    let f = frame(&out, 45);
    assert!(
        near(at(&f, 0.5, rows[0]), [255, 0, 0], 30),
        "{:?}",
        at(&f, 0.5, rows[0])
    );
    assert!(
        near(at(&f, 0.5, rows[1]), [0, 255, 0], 30),
        "{:?}",
        at(&f, 0.5, rows[1])
    );
    assert!(
        near(at(&f, 0.5, 212.0 / 640.0), [255, 255, 255], 30),
        "gutter: {:?}",
        at(&f, 0.5, 212.0 / 640.0)
    );

    // Mid-swap, t = 2.7: tile 0 is 0.2 s into its 0.4 s slide, ease_out(0.5) = 0.875, so the
    // outgoing red keeps the left 12.5 % and the incoming blue the rest. Tile 1 starts 0.12 s
    // later (ease_out(0.2) = 0.488): green up to 51 %, yellow after. Tile 2 hasn't started.
    let f = frame(&out, 81);
    assert!(
        near(at(&f, 0.05, rows[0]), [255, 0, 0], 30),
        "red: {:?}",
        at(&f, 0.05, rows[0])
    );
    assert!(
        near(at(&f, 0.3, rows[0]), [0, 0, 255], 30),
        "blue: {:?}",
        at(&f, 0.3, rows[0])
    );
    assert!(
        near(at(&f, 0.3, rows[1]), [0, 255, 0], 30),
        "green: {:?}",
        at(&f, 0.3, rows[1])
    );
    assert!(
        near(at(&f, 0.8, rows[1]), [255, 255, 0], 30),
        "yellow: {:?}",
        at(&f, 0.8, rows[1])
    );

    // Mid-ramp: speed 1 -> 2 over the first second and 2 -> 1 over the next, each eased
    // symmetrically, so 1 s in is 1.5 s of source (frame 45) and 2 s in is 3 s (frame 90);
    // at 1x they'd be frames 30 and 60.
    for (n, src) in [(30, 45.0), (60, 90.0)] {
        let f = frame(&out, n);
        let v = at(&f, 0.5, rows[2]);
        let want = (src * STEP) as i32;
        assert!(
            near(v, [want; 3], 8),
            "frame {n}: {v:?}, want source frame {src} ({want})"
        );
    }

    // Loop end: the tiles have slid out and the canvas fade reaches black on the last frame.
    let f = frame(&out, 179);
    for y in rows {
        for x in [0.1, 0.5, 0.9] {
            assert!(
                near(at(&f, x, y), black, 12),
                "frame 179 at {x},{y}: {:?}",
                at(&f, x, y)
            );
        }
    }
}
