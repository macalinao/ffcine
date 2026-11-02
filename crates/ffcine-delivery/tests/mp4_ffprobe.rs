//! Chapters and cover written into ffmpeg-made MP4s, read back by ffprobe. Skips without
//! `ffmpeg`/`ffprobe` on `PATH`.

use std::path::Path;
use std::process::Command;

use ffcine_delivery::Chapter;
use ffcine_delivery::mp4::{self, Tags};
use serde_json::Value;

fn run(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(cmd).args(args).output().ok()?;
    assert!(
        out.status.success(),
        "{cmd} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(out.stdout)
}

fn probe(path: &Path) -> Value {
    let out = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_chapters",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
            path.to_str().expect("path"),
        ],
    )
    .expect("ffprobe");
    serde_json::from_slice(&out).expect("json")
}

/// Per-frame hashes of the video and audio, to show no packet changed.
fn md5s(path: &Path) -> String {
    let out = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-i",
            path.to_str().expect("path"),
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-c",
            "copy",
            "-f",
            "framemd5",
            "-",
        ],
    )
    .expect("ffmpeg");
    String::from_utf8(out).expect("utf8")
}

#[test]
#[allow(clippy::too_many_lines)]
fn ffprobe_reads_chapters_and_cover() {
    if Command::new("ffprobe").arg("-version").output().is_err() {
        eprintln!("skipped: no ffprobe");
        return;
    }
    let dir = std::env::temp_dir().join(format!("ffcine-mp4-ffprobe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let s = |p: &Path| p.to_str().expect("path").to_owned();
    let (src, cover, out) = (
        dir.join("src.mp4"),
        dir.join("cover.png"),
        dir.join("out.mp4"),
    );
    // Synthetic picture and tone; mdat before moov (no faststart), as the muxer leaves it.
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x180:r=24000/1001:d=4",
            "-f",
            "lavfi",
            "-i",
            "sine=d=4",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
            "-shortest",
            &s(&src),
        ],
    );
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=64x36",
            "-frames:v",
            "1",
            &s(&cover),
        ],
    );
    let tags = Tags {
        chapters: vec![
            Chapter {
                start: 0.0,
                title: "Intro".into(),
            },
            Chapter {
                start: 1.5,
                title: "Middle".into(),
            },
            Chapter {
                start: 3.0,
                title: "End".into(),
            },
        ],
        cover: Some(std::fs::read(&cover).expect("cover")),
    };
    mp4::write(&src, &out, &tags).expect("tag");
    // Twice: replaces what the first run wrote.
    mp4::write_in_place(&out, &tags).expect("retag");

    let p = probe(&out);
    let chapters = p["chapters"].as_array().expect("chapters");
    let got: Vec<(String, String)> = chapters
        .iter()
        .map(|c| {
            (
                c["start_time"].as_str().unwrap_or_default().to_owned(),
                c["tags"]["title"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("0.000000".into(), "Intro".into()),
            ("1.500000".into(), "Middle".into()),
            ("3.000000".into(), "End".into())
        ],
        "{p:#}"
    );
    let streams = p["streams"].as_array().expect("streams");
    let pics: Vec<&Value> = streams
        .iter()
        .filter(|s| s["disposition"]["attached_pic"] == 1)
        .collect();
    assert_eq!(pics.len(), 1, "{p:#}");
    assert_eq!(pics[0]["codec_name"], "png");
    assert_eq!(md5s(&src), md5s(&out), "packets unchanged");

    // faststart: moov ahead of the media.
    let bytes = std::fs::read(&out).expect("read");
    assert_eq!(&bytes[4..8], b"ftyp");
    let ftyp = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    assert_eq!(&bytes[ftyp + 4..ftyp + 8], b"moov");
    let _ = std::fs::remove_dir_all(dir);
}
