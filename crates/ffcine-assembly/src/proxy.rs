//! Proxies for review renders: low-res copies of the sources that segments decode instead.
//!
//! A clip's proxy is the camera's own when there is one (a DJI `.LRF` next to the `.MP4`), else
//! one made once with ffmpeg ([`make`]) into the cache's `proxy/` directory, named by the
//! source's fingerprint, so an edited or replaced source gets a new one. Segments rendered from a
//! proxy are keyed by the proxy's fingerprint and the `review` profile, never mixed up with
//! full-resolution ones.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use crate::recipe::{Fingerprinter, Key};

/// Short side of a made proxy, px.
pub const SHORT_SIDE: u32 = 720;

/// The camera's proxy next to `source`: a DJI `.LRF` with the same stem.
pub fn sibling(source: &Path) -> Option<PathBuf> {
    ["LRF", "lrf"]
        .iter()
        .map(|ext| source.with_extension(ext))
        .find(|p| p.is_file() && p.as_path() != source)
}

/// Where [`make`] puts `source`'s proxy in `dir`.
pub fn cached(dir: &Path, source: &Path, fp: &dyn Fingerprinter) -> PathBuf {
    let key = Key::of(&json!({"proxy": fp.fingerprint(source), "short": SHORT_SIDE}));
    dir.join(format!("{key}.mp4"))
}

/// The proxy to decode for `source`: its [`sibling`], else a made one in `dir` if it exists.
pub fn find(source: &Path, dir: Option<&Path>, fp: &dyn Fingerprinter) -> Option<PathBuf> {
    sibling(source).or_else(|| dir.map(|d| cached(d, source, fp)).filter(|p| p.is_file()))
}

/// Extensions of sources that get a proxy (video, not stills or audio).
fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "mp4" | "mov" | "m4v" | "mkv" | "mts" | "mxf" | "avi" | "webm"
            )
        })
}

/// What [`make`] needs to do for `source`: `None` when it has a proxy or isn't a video file.
pub fn needed(source: &Path, dir: &Path, fp: &dyn Fingerprinter) -> Option<PathBuf> {
    if !is_video(source) || !source.is_file() || sibling(source).is_some() {
        return None;
    }
    Some(cached(dir, source, fp)).filter(|p| !p.is_file())
}

/// `-vf` scaling the short side to [`SHORT_SIDE`] (never up), `filter` being `scale` or
/// `scale_cuda`.
fn scale(filter: &str) -> String {
    let s = SHORT_SIDE;
    format!("{filter}=w='if(gt(iw,ih),-2,min({s},iw))':h='if(gt(iw,ih),min({s},ih),-2)'")
}

/// ffmpeg arguments for a proxy of `source` at `out`: NVDEC + `scale_cuda` + NVENC with
/// `cuda`, else libx264 on the CPU. Picture only, colour tags and rotation kept.
pub fn args(source: &Path, out: &Path, cuda: bool) -> Vec<OsString> {
    let mut v: Vec<OsString> = ["-v", "error", "-y", "-noautorotate"]
        .iter()
        .map(OsString::from)
        .collect();
    if cuda {
        v.extend(
            ["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]
                .iter()
                .map(OsString::from),
        );
    }
    v.push("-i".into());
    v.push(source.into());
    v.extend(
        ["-map", "0:v:0", "-an", "-sn", "-dn", "-vf"]
            .iter()
            .map(OsString::from),
    );
    if cuda {
        v.push(scale("scale_cuda").into());
        v.extend(
            [
                "-c:v",
                "hevc_nvenc",
                "-preset",
                "p1",
                "-rc",
                "constqp",
                "-qp",
                "20",
            ]
            .iter()
            .map(OsString::from),
        );
    } else {
        v.push(scale("scale").into());
        v.extend(
            ["-c:v", "libx264", "-preset", "veryfast", "-crf", "18"]
                .iter()
                .map(OsString::from),
        );
    }
    v.extend(["-g", "24", "-f", "mp4"].iter().map(OsString::from));
    v.push(out.into());
    v
}

/// Makes `source`'s proxy at `out` (atomically) with `ffmpeg`: on the GPU, falling back to the
/// CPU if that fails.
pub fn make(ffmpeg: &OsStr, source: &Path, out: &Path) -> Result<(), String> {
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let partial = out.with_extension("partial.mp4");
    let mut last = String::new();
    for cuda in [true, false] {
        match Command::new(ffmpeg)
            .args(args(source, &partial, cuda))
            .output()
        {
            Ok(o) if o.status.success() => {
                return std::fs::rename(&partial, out)
                    .map_err(|e| format!("{}: {e}", out.display()));
            }
            Ok(o) => String::from_utf8_lossy(&o.stderr)
                .trim()
                .clone_into(&mut last),
            Err(e) => last = format!("{}: {e}", ffmpeg.to_string_lossy()),
        }
    }
    let _ = std::fs::remove_file(&partial);
    Err(format!("proxy of {}: {last}", source.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FsFingerprinter;

    #[test]
    fn lrf_beside_the_clip_wins() {
        let dir = std::env::temp_dir().join(format!("ffcine-proxy-{}", std::process::id()));
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).expect("dir");
        let (a, b) = (dir.join("A001.MP4"), dir.join("B001.MOV"));
        for p in [&a, &b, &dir.join("A001.LRF")] {
            std::fs::write(p, b"x").expect("write");
        }
        let fp = FsFingerprinter;
        assert_eq!(find(&a, Some(&cache), &fp), Some(dir.join("A001.LRF")));
        assert_eq!(needed(&a, &cache, &fp), None);
        // No camera proxy: made into the cache, found there once it exists.
        let made = needed(&b, &cache, &fp).expect("needs one");
        assert!(made.starts_with(&cache));
        assert_eq!(find(&b, Some(&cache), &fp), None);
        std::fs::write(&made, b"x").expect("write");
        assert_eq!(find(&b, Some(&cache), &fp), Some(made.clone()));
        assert_eq!(needed(&b, &cache, &fp), None);
        // A changed source wants a new one.
        std::fs::write(&b, b"longer").expect("write");
        assert_ne!(cached(&cache, &b, &fp), made);
        assert_eq!(needed(&dir.join("song.wav"), &cache, &fp), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn makes_a_small_proxy() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipped: no ffmpeg");
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffcine-proxy-make-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let src = dir.join("tall.mp4");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=s=1080x1920:r=24:d=1")
            .args(["-c:v", "mpeg4"])
            .arg(&src)
            .status()
            .expect("ffmpeg");
        assert!(ok.success());
        let out = dir.join("proxy.mp4");
        make(OsStr::new("ffmpeg"), &src, &out).expect("proxy");
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
            .args(["stream=width,height", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .expect("ffprobe");
        assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "720,1280");
        let _ = std::fs::remove_dir_all(dir);
    }
}
