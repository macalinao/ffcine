//! The breakdown, top to bottom.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ffcine_triage::scene::{SceneDetector, SceneParams, small_luma};
use ffcine_triage::transcribe::{Whisper, WhisperParams};
use image::RgbImage;

use crate::colour::{self, Sample};
use crate::motion::{self, Luma, MotionParams, Pyramid, Step};
use crate::{
    Breakdown, Error, Keyframes, Result, Shot, VERSION, decode, ocr, pacing, rhythm, sheet,
    shotlist, spans, words_in,
};

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // switches
pub struct Options {
    pub input: PathBuf,
    /// Output directory.
    pub out: PathBuf,
    /// Decode on NVDEC when it takes the clip.
    pub gpu: bool,
    pub scene: SceneParams,
    pub motion: MotionParams,
    /// Long side of the analysis frames, px.
    pub analysis_size: u32,
    /// Long side of the keyframe JPEGs, px.
    pub keyframe_size: u32,
    /// Detect the music's tempo and beats.
    pub beats: bool,
    /// A cut within this many seconds of a beat is on it.
    pub beat_tolerance: f64,
    /// `None` skips transcription.
    pub whisper: Option<WhisperParams>,
    /// Read on-screen text with `tesseract` if it is on `PATH`.
    pub ocr: bool,
    /// Contact sheet width, px.
    pub sheet_width: u32,
    /// Contact sheet columns (default: by the frame's shape).
    pub sheet_columns: Option<u32>,
}

impl Options {
    pub fn new(input: impl Into<PathBuf>, out: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            out: out.into(),
            gpu: true,
            scene: SceneParams {
                threshold: 0.3,
                ratio: 3.0,
                min_gap: 0.2,
            },
            motion: MotionParams::default(),
            analysis_size: 160,
            keyframe_size: 960,
            beats: true,
            beat_tolerance: 0.07,
            whisper: Some(WhisperParams::default()),
            ocr: true,
            sheet_width: 1800,
            sheet_columns: None,
        }
    }
}

/// Per-frame analysis.
struct Frames {
    times: Vec<f64>,
    /// Motion from the previous frame (zero for the first).
    steps: Vec<Step>,
    sharp: Vec<f32>,
    colour: Vec<Sample>,
    cuts: Vec<usize>,
}

fn analyse(o: &Options) -> Result<(Frames, decode::Info)> {
    let mut det = SceneDetector::new(o.scene.clone());
    let mut fr = Frames {
        times: Vec::new(),
        steps: Vec::new(),
        sharp: Vec::new(),
        colour: Vec::new(),
        cuts: Vec::new(),
    };
    let mut prev: Option<Pyramid> = None;
    let info = decode::frames(
        &o.input,
        o.gpu,
        o.analysis_size,
        |_| true,
        |i, t, rgb, w, h| {
            let (w, h) = (w as usize, h as usize);
            if det.push(t, &small_luma(rgb, w, h)) {
                fr.cuts.push(i);
            }
            let cur = Pyramid::new(Luma::from_rgb(rgb, w, h));
            fr.steps.push(
                prev.as_ref()
                    .map_or_else(Step::default, |p| motion::estimate(p, &cur)),
            );
            fr.sharp.push(cur.base().sharpness());
            fr.colour.push(Sample::from_rgb(rgb));
            fr.times.push(t);
            prev = Some(cur);
            Ok(())
        },
    )?;
    Ok((fr, info))
}

/// Writes the wanted frames as JPEGs; returns small copies of those in `thumbs` for the sheet.
fn keyframes(
    o: &Options,
    names: &HashMap<usize, Vec<String>>,
    thumbs: &[usize],
) -> Result<HashMap<usize, RgbImage>> {
    let mut small = HashMap::new();
    decode::frames(
        &o.input,
        o.gpu,
        o.keyframe_size,
        |i| names.contains_key(&i),
        |i, _, rgb, w, h| {
            let img = RgbImage::from_raw(w, h, rgb.to_vec())
                .ok_or_else(|| Error::Invalid("frame size".into()))?;
            for n in names.get(&i).into_iter().flatten() {
                sheet::save_jpeg(&img, &o.out.join(n), 88)?;
            }
            if thumbs.contains(&i) {
                let (tw, th) = decode::fit(w, h, 400);
                small.insert(
                    i,
                    image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle),
                );
            }
            Ok(())
        },
    )?;
    Ok(small)
}

fn transcribe(
    input: &Path,
    wp: &WhisperParams,
    scratch: &Path,
) -> Result<Option<crate::Transcript>> {
    std::fs::create_dir_all(scratch)?;
    let pcm = scratch.join("audio.f32");
    if ffcine_triage::decode::scan_audio(input, Some(&pcm))?.is_none() {
        return Ok(None);
    }
    let mut w = Whisper::start(wp, scratch)?;
    let t = w.transcribe("breakdown", &pcm)?;
    Ok((t.words().next().is_some() || !t.segments.is_empty()).then_some(t))
}

/// Runs the breakdown of `o.input` into `o.out`.
#[allow(clippy::too_many_lines)] // the run, top to bottom
pub fn run(o: &Options) -> Result<Breakdown> {
    ffcine_triage::decode::init()?;
    let kf_dir = o.out.join("keyframes");
    std::fs::create_dir_all(&kf_dir)?;

    let (fr, info) = analyse(o)?;
    if info.frames == 0 {
        return Err(Error::Decode(format!("{}: no frames", o.input.display())));
    }
    let spans = spans(info.frames, &fr.cuts, &fr.sharp);
    eprintln!(
        "{} frames on {}: {} shots",
        info.frames,
        if info.gpu { "nvdec" } else { "cpu" },
        spans.len()
    );

    let mut names: HashMap<usize, Vec<String>> = HashMap::new();
    let name = |k: usize, tag: &str| format!("keyframes/{k:03}{tag}.jpg");
    for (k, s) in spans.iter().enumerate() {
        for (f, tag) in [(s.key, ""), (s.first, "-first"), (s.last, "-last")] {
            names.entry(f).or_default().push(name(k + 1, tag));
        }
    }
    let keys: Vec<usize> = spans.iter().map(|s| s.key).collect();
    let thumbs = keyframes(o, &names, &keys)?;

    let end_of = |b: usize| fr.times.get(b).copied().unwrap_or(info.duration);
    let cut_times: Vec<f64> = spans.iter().skip(1).map(|s| fr.times[s.a]).collect();

    let music = if o.beats && ffcine_audio::decode::has_audio(&o.input) {
        match ffcine_audio::beats::detect(&o.input) {
            Ok(b) if b.tempo > 0.0 && b.beats.len() >= 4 => Some(rhythm::align(
                b.tempo,
                &b.beats,
                &cut_times,
                o.beat_tolerance,
            )),
            Ok(_) => None,
            Err(e) => {
                eprintln!("no beats: {e}");
                None
            }
        }
    } else {
        None
    };

    let transcript = match &o.whisper {
        Some(wp) => {
            let scratch = o.out.join(".scratch");
            let t = transcribe(&o.input, wp, &scratch);
            let _ = std::fs::remove_dir_all(&scratch);
            t.unwrap_or_else(|e| {
                eprintln!("no transcript: {e}");
                None
            })
        }
        None => None,
    };

    let tesseract = if o.ocr { ocr::find() } else { None };

    let r3 = |v: f64| (v * 1000.0).round() / 1000.0;
    let mut shots = Vec::with_capacity(spans.len());
    for (k, s) in spans.iter().enumerate() {
        let (start, end) = (fr.times[s.a], end_of(s.b));
        let steps = &fr.steps[(s.a + 1).min(s.b)..s.b];
        let kf = Keyframes {
            key: name(k + 1, ""),
            key_time: r3(fr.times[s.key]),
            first: name(k + 1, "-first"),
            last: name(k + 1, "-last"),
        };
        let (cut_on_beat, beat_offset, beats) = match &music {
            Some(m) => {
                let off = (k > 0).then(|| rhythm::nearest(&m.beats, start)).flatten();
                (
                    off.map(|d| d.abs() <= m.tolerance),
                    off.map(r3),
                    Some((end - start) * m.tempo / 60.0).map(|b| (b * 100.0).round() / 100.0),
                )
            }
            None => (None, None, None),
        };
        let text = match &tesseract {
            Some(t) => ocr::read(t, &o.out.join(&kf.key)),
            None => String::new(),
        };
        shots.push(Shot {
            n: k + 1,
            start: r3(start),
            end: r3(end),
            duration: r3(end - start),
            frames: [s.a, s.b],
            keyframes: kf,
            motion: motion::classify(steps, info.fps, &o.motion),
            colour: colour::average(&fr.colour[s.a..s.b]),
            cut_on_beat,
            beat_offset,
            beats,
            speech: transcript
                .as_ref()
                .map(|t| words_in(t, start, end))
                .unwrap_or_default(),
            text,
        });
    }

    let mut b = Breakdown {
        version: VERSION,
        source: o.input.clone(),
        duration: r3(info.duration),
        fps: r3(info.fps),
        width: info.width,
        height: info.height,
        frames: info.frames,
        decoder: if info.gpu { "nvdec" } else { "cpu" }.into(),
        pacing: pacing(&shots, info.duration),
        cuts: cut_times.iter().copied().map(r3).collect(),
        shots,
        music,
        transcript,
        contact_sheet: None,
        shotlist: "shotlist.md".into(),
    };

    let cells: Vec<(String, RgbImage)> = b
        .shots
        .iter()
        .zip(&keys)
        .filter_map(|(s, k)| {
            thumbs
                .get(k)
                .map(|img| (format!("{}  ·  {:.1}s", s.n, s.duration), img.clone()))
        })
        .collect();
    if !cells.is_empty() {
        let aspect = f64::from(info.width) / f64::from(info.height.max(1));
        let cols = o
            .sheet_columns
            .unwrap_or_else(|| sheet::columns(cells.len(), aspect));
        let title = format!(
            "{}  ·  {} shots  ·  {}",
            o.input
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            b.shots.len(),
            crate::clock(b.duration)
        );
        let img = sheet::render(&title, &cells, o.sheet_width, cols);
        sheet::save_under(&img, &o.out.join("contact-sheet.jpg"), 5_000_000)?;
        b.contact_sheet = Some("contact-sheet.jpg".into());
    }
    std::fs::write(o.out.join(&b.shotlist), shotlist::render(&b))?;
    let json = o.out.join("breakdown.json");
    let tmp = json.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&b)?)?;
    std::fs::rename(&tmp, &json)?;
    Ok(b)
}
