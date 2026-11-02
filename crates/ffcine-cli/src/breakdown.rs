//! `ffcine breakdown`.
#![allow(clippy::doc_markdown)] // doc comments are --help text

use std::path::PathBuf;

use bpaf::Bpaf;
use ffcine::breakdown::{self, Options};
use ffcine::triage::transcribe::WhisperParams;

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::struct_excessive_bools)] // flags
pub struct BreakdownArgs {
    /// Output directory (default: NAME-breakdown next to the video).
    #[bpaf(short('o'), long("out"), argument("DIR"))]
    out: Option<PathBuf>,
    /// Cut score threshold, 0..1; lower finds softer cuts.
    #[bpaf(argument("SCORE"), fallback(0.3), display_fallback)]
    threshold: f32,
    /// Shortest shot, seconds.
    #[bpaf(argument("SEC"), fallback(0.2), display_fallback)]
    min_shot: f64,
    /// A cut this close to a beat is on it, seconds.
    #[bpaf(argument("SEC"), fallback(0.07), display_fallback)]
    beat_tolerance: f64,
    /// Long side of the keyframe JPEGs, px.
    #[bpaf(argument("PX"), fallback(960), display_fallback)]
    keyframe_size: u32,
    /// Contact sheet width, px.
    #[bpaf(argument("PX"), fallback(1800), display_fallback)]
    sheet_width: u32,
    /// Contact sheet columns (default: 8 for portrait, 6 square, 5 landscape).
    #[bpaf(argument("N"))]
    columns: Option<u32>,
    /// Skip the music tempo and beat check.
    no_beats: bool,
    /// Skip transcription.
    no_transcribe: bool,
    /// Whisper model, a faster-whisper name or CTranslate2 directory (default: large-v3-turbo).
    #[bpaf(argument("NAME"), fallback("large-v3-turbo".to_owned()))]
    model: String,
    /// Python with faster-whisper (default: $FFCINE_WHISPER_PYTHON, else python3).
    #[bpaf(argument("PATH"))]
    whisper_python: Option<PathBuf>,
    /// Transcription language, e.g. en (default: detected).
    #[bpaf(argument("LANG"))]
    language: Option<String>,
    /// Run whisper on the CPU.
    whisper_cpu: bool,
    /// Don't read on-screen text (default: read it when tesseract is on PATH).
    no_ocr: bool,
    /// Decode on the CPU instead of NVDEC.
    cpu_decode: bool,
    /// The video, a local file (a reel, TikTok, trailer).
    #[bpaf(positional("VIDEO"))]
    video: PathBuf,
}

impl BreakdownArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let out = self.out.clone().unwrap_or_else(|| {
            let stem = self
                .video
                .file_stem()
                .map_or_else(|| "video".into(), |s| s.to_string_lossy().into_owned());
            self.video.with_file_name(format!("{stem}-breakdown"))
        });
        let mut o = Options::new(&self.video, &out);
        o.gpu = !self.cpu_decode;
        o.scene.threshold = self.threshold;
        o.scene.min_gap = self.min_shot;
        o.beats = !self.no_beats;
        o.beat_tolerance = self.beat_tolerance;
        o.keyframe_size = self.keyframe_size;
        o.sheet_width = self.sheet_width;
        o.sheet_columns = self.columns;
        o.ocr = !self.no_ocr;
        o.whisper = (!self.no_transcribe).then(|| {
            let mut w = WhisperParams {
                model: self.model.clone(),
                language: self.language.clone().unwrap_or_default(),
                ..Default::default()
            };
            if let Some(p) = &self.whisper_python {
                w.python.clone_from(p);
            }
            if self.whisper_cpu {
                w.device = "cpu".into();
                w.compute = "int8".into();
            }
            w
        });
        let b = breakdown::run(&o)?;
        let p = &b.pacing;
        eprint!(
            "{} shots, mean {:.2} s, {:.1} cuts/min",
            p.shots, p.mean, p.cuts_per_minute
        );
        if let Some(m) = &b.music {
            eprint!(
                ", {:.0} BPM, {}/{} cuts on the beat",
                m.tempo, m.on_beat, m.cuts
            );
        }
        eprintln!();
        for f in ["breakdown.json", "shotlist.md", "contact-sheet.jpg"] {
            let path = out.join(f);
            if path.exists() {
                println!("{}", path.display());
            }
        }
        Ok(())
    }
}
