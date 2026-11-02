//! `ffcine sync`, `ffcine audio` and `ffcine beats`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use bpaf::Bpaf;
use ffcine::audio::{self, Clean, MixOptions, SyncOptions, WriteOptions};
use ffcine::timeline::Timeline;

const AUDIO_EXT: &[&str] = &["wav", "bwf", "flac", "aif", "aiff"];
const VIDEO_EXT: &[&str] = &["mp4", "mov", "mxf", "mkv", "m4v", "mts", "avi", "lrf"];

#[derive(Debug, Clone, Bpaf)]
pub struct SyncArgs {
    /// Recorder audio (WAV, BWF, FLAC, AIFF): a directory or one file.
    #[bpaf(argument("DIR"))]
    audio: PathBuf,
    /// Camera clips with scratch audio (MP4, MOV, MXF, MKV, ...): a directory or one file.
    #[bpaf(argument("DIR"))]
    video: PathBuf,
    /// Add the recorder track to this timeline (default: a new one, clips in name order).
    #[bpaf(argument("FILE"))]
    timeline: Option<PathBuf>,
    /// J-cut: recorder audio starts this many seconds before its video.
    #[bpaf(argument("SECONDS"), fallback(0.0), display_fallback)]
    lead: f64,
    /// L-cut: recorder audio runs this many seconds past its video.
    #[bpaf(argument("SECONDS"), fallback(0.0), display_fallback)]
    tail: f64,
    /// Leave matches under this confidence (0..1) out of the timeline.
    #[bpaf(argument("C"), fallback(0.15), display_fallback)]
    min_confidence: f64,
    /// Clean chain with the outdoor wind preset on every recorder clip.
    wind: bool,
    /// Output timeline (.otio).
    #[bpaf(short('o'), long("output"), argument("FILE"))]
    output: PathBuf,
}

impl SyncArgs {
    pub fn run(&self) -> anyhow::Result<()> {
        let recs = files(&self.audio, AUDIO_EXT)?;
        let videos = files(&self.video, VIDEO_EXT)?;
        anyhow::ensure!(
            !recs.is_empty(),
            "no recordings in {}",
            self.audio.display()
        );
        anyhow::ensure!(!videos.is_empty(), "no clips in {}", self.video.display());
        let syncs = audio::sync_files(&videos, &recs, &SyncOptions::default())?;
        print!("{}", audio::sync::report(&syncs, &recs));
        let timeline = match &self.timeline {
            Some(p) => {
                Some(Timeline::from_file(p).with_context(|| format!("reading {}", p.display()))?)
            }
            None => None,
        };
        let opts = WriteOptions {
            lead: self.lead,
            tail: self.tail,
            min_confidence: self.min_confidence,
            track: None,
            clean: Some(Clean {
                wind: self.wind,
                ..Clean::default()
            }),
        };
        let tl = audio::write_timeline(timeline, &syncs, &recs, &opts)?;
        tl.to_file(&self.output)?;
        let ok = syncs
            .iter()
            .filter(|s| {
                s.found
                    .as_ref()
                    .is_some_and(|m| m.confidence >= self.min_confidence)
            })
            .count();
        eprintln!(
            "{}: {ok}/{} clips synced",
            self.output.display(),
            syncs.len()
        );
        Ok(())
    }
}

/// Mix, overriding the timeline's project.mix:
#[derive(Debug, Clone, Bpaf)]
pub struct MixFlags {
    /// Master loudness in LUFS (default: -14).
    #[bpaf(argument("LUFS"))]
    lufs: Option<f64>,
    /// Master true-peak ceiling in dBTP (default: -1).
    #[bpaf(argument("DB"))]
    true_peak: Option<f64>,
    /// Limiter ceiling this many dB under the true peak, for the AAC encode's overshoot
    /// (default: 2).
    #[bpaf(argument("DB"))]
    headroom: Option<f64>,
    /// Leave the master loudness as mixed (still limited).
    no_normalize: bool,
    /// Duck the music only while dialogue is audible, not for each dialogue clip's whole span.
    sidechain: bool,
}

impl MixFlags {
    pub fn apply(&self, o: &mut MixOptions) {
        if let Some(l) = self.lufs {
            o.master.lufs = Some(l);
        }
        if self.no_normalize {
            o.master.lufs = None;
        }
        if let Some(tp) = self.true_peak {
            o.master.true_peak = tp;
        }
        if let Some(h) = self.headroom {
            o.master.headroom = h;
        }
        if self.sidechain {
            o.sidechain = true;
        }
    }
}

#[derive(Debug, Clone, Bpaf)]
pub struct AudioArgs {
    #[bpaf(external(mix_flags))]
    mix: MixFlags,
    /// Also write one WAV per bus (dialogue, music, ambience, sfx) into DIR.
    #[bpaf(argument("DIR"))]
    stems: Option<PathBuf>,
    /// Sample rate in Hz.
    #[bpaf(argument("HZ"), fallback(48000), display_fallback)]
    rate: u32,
    /// Output WAV (32-bit float, stereo).
    #[bpaf(short('o'), long("output"), argument("FILE"))]
    output: PathBuf,
    /// Timeline to mix (.otio).
    #[bpaf(positional("TIMELINE"))]
    timeline: PathBuf,
}

impl AudioArgs {
    pub fn run(&self) -> anyhow::Result<()> {
        let tl = Timeline::from_file(&self.timeline)
            .with_context(|| format!("reading {}", self.timeline.display()))?;
        let mut opts = MixOptions::for_timeline(&tl, self.timeline.parent())?;
        opts.rate = self.rate;
        self.mix.apply(&mut opts);
        let mix = audio::render_audio(&tl, &opts)?;
        audio::write_mix(&mix, &self.output, self.stems.as_deref())?;
        eprintln!(
            "{}: {:.1} s, {:.1} LUFS, {:.1} dBTP",
            self.output.display(),
            mix.master.seconds(),
            mix.lufs,
            mix.true_peak
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::doc_markdown)] // --help text
pub struct BeatsArgs {
    /// Re-analyse instead of reading the cache ($XDG_CACHE_HOME/ffcine/beats).
    no_cache: bool,
    /// Print JSON (`{tempo, beats, duration}`).
    json: bool,
    /// Music file (anything ffmpeg decodes).
    #[bpaf(positional("FILE"))]
    file: PathBuf,
}

impl BeatsArgs {
    pub fn run(&self) -> anyhow::Result<()> {
        let b = if self.no_cache {
            audio::beats::detect(&self.file)
        } else {
            audio::beats::cached(&self.file, &audio::beats::default_cache_dir())
        }
        .with_context(|| format!("analysing {}", self.file.display()))?;
        if self.json {
            println!("{}", serde_json::to_string(&b)?);
            return Ok(());
        }
        println!(
            "{}: {:.1} BPM, {} beats over {:.2} s",
            self.file.display(),
            b.tempo,
            b.beats.len(),
            b.duration
        );
        for t in &b.beats {
            println!("{t:.3}");
        }
        Ok(())
    }
}

/// The cached beat times of `path`, for importers (`ImportOptions::beats`).
pub fn beat_times(path: &Path) -> Option<Vec<f64>> {
    audio::beats::cached(path, &audio::beats::default_cache_dir())
        .map_err(|e| eprintln!("warning: beats of {}: {e}", path.display()))
        .ok()
        .map(|b| b.beats)
}

/// `p` itself, or the files in directory `p` with one of `ext` (any case), sorted.
fn files(p: &Path, ext: &[&str]) -> anyhow::Result<Vec<PathBuf>> {
    if p.is_file() {
        return Ok(vec![p.to_owned()]);
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .with_context(|| format!("reading {}", p.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|f| {
            f.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ext.contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    v.sort();
    Ok(v)
}
