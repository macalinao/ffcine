//! Temporal denoise across a cut (`cargo test -p ffcine --test denoise_context_gpu -- --ignored`;
//! needs a Vulkan GPU and `ffmpeg` on PATH): a clip rendered in two pieces, each given its
//! neighbouring frames as context, matches the clip rendered whole, frame for frame.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ffcine::filters::Motion;
use ffcine::{Denoise, EncoderSettings, Grade, Pipeline, RateControl, Temporal, backend};

const N: u64 = 48;
const CUT: u64 = 24;

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-denoise-ctx-{}", std::process::id()));
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

/// Per-frame hashes of a video.
fn frames(path: &Path) -> Vec<String> {
    let out = ffmpeg(&["-i", path.to_str().expect("utf-8"), "-f", "framemd5", "-"]);
    String::from_utf8_lossy(&out)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap_or("").trim().to_owned())
        .collect()
}

#[test]
#[ignore = "needs a Vulkan GPU and ffmpeg"]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn pieces_with_context_match_the_whole_clip() {
    let d = dir();
    let src = d.join("src.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=24,noise=alls=24:allf=t,format=yuv420p",
        "-frames:v",
        &N.to_string(),
        "-g",
        "12",
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
        src.to_str().expect("utf-8"),
    ]);
    let ff = backend::Ffmpeg::new().expect("backend");
    let render = |name: &str, start: u64, n: u64, before: u64, after: u64| {
        let out = d.join(format!("{name}.mkv"));
        Pipeline::new(&src, &out)
            .range(Duration::from_secs_f64(start as f64 / 24.0), Some(n))
            .context(before, after)
            .grade(Grade {
                dither: None,
                peak_detect: false,
                denoise: Some(Denoise {
                    temporal: Some(Temporal {
                        radius: 2,
                        motion: Motion::Blocks,
                        strength: 8.0,
                        chroma: None,
                        protect: 0.0,
                    }),
                    spatial: None,
                }),
                ..Grade::default()
            })
            .encoder(EncoderSettings {
                rate: RateControl::Crf(0.0),
                pixel_format: ffcine::PixelFormat::Yuv420p,
                ..EncoderSettings::x264_preview()
            })
            .run(&ff)
            .expect("render");
        frames(&out)
    };
    let whole = render("whole", 0, N, 0, 0);
    assert_eq!(whole.len(), N as usize);
    // At the source's start there's no history to read: fewer frames, no error.
    let a = render("a", 0, CUT, 2, 2);
    let b = render("b", CUT, N - CUT, 2, 2);
    let cut = CUT as usize;
    assert_eq!(a, whole[..cut]);
    assert_eq!(b, whole[cut..]);
    // Without context the piece's first frame has no history: it differs.
    let bare = render("bare", CUT, N - CUT, 0, 0);
    assert_ne!(bare[0], whole[cut]);
    let _ = std::fs::remove_dir_all(&d);
}
