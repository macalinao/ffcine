//! `ffcine triage`, `ffcine search`, `ffcine snap`.
#![allow(clippy::doc_markdown)] // doc comments are --help text

use std::path::PathBuf;

use anyhow::Context as _;
use bpaf::Bpaf;
use ffcine::triage::{self, Index, SnapParams, timecode};

const DEFAULT_INDEX: &str = "triage/index.json";

#[cfg(feature = "gpu")]
#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::struct_excessive_bools)] // flags
pub struct TriageArgs {
    /// Output directory (default: DIR/triage).
    #[bpaf(short('o'), long("out"), argument("DIR"))]
    out: Option<PathBuf>,
    /// Triage the clips listed in FILE (one path per line) instead of scanning DIR.
    #[bpaf(argument("FILE"))]
    list: Option<PathBuf>,
    /// Scan DIR even if it has an ingest catalog.json.
    no_catalog: bool,
    /// Clips analysed at once.
    #[bpaf(short('j'), long("jobs"), argument("N"), fallback(4), display_fallback)]
    jobs: usize,
    /// Skip transcription.
    no_transcribe: bool,
    /// Whisper model: a faster-whisper model name or a CTranslate2 model directory.
    #[bpaf(argument("NAME"), fallback("large-v3-turbo".to_owned()), display_fallback)]
    model: String,
    /// Python with faster-whisper on CUDA (default: $FFCINE_WHISPER_PYTHON, else python3).
    #[bpaf(argument("PATH"))]
    whisper_python: Option<PathBuf>,
    /// Transcription language code, e.g. en or ja (default: detected per clip).
    #[bpaf(argument("LANG"))]
    language: Option<String>,
    /// Words whisper should prefer, comma-separated, e.g. "gelato, piazza".
    #[bpaf(argument("WORDS"))]
    hotwords: Option<String>,
    /// Run whisper on the CPU.
    whisper_cpu: bool,
    /// Decode clips without a proxy on the CPU instead of NVDEC.
    cpu_decode: bool,
    /// Flag takes at least this long as runaway, in minutes.
    #[bpaf(argument("MIN"), fallback(20.0), display_fallback)]
    runaway_minutes: f64,
    /// Don't transcribe clips with less speech than this, in seconds.
    #[bpaf(argument("SEC"), fallback(1.0), display_fallback)]
    min_speech: f64,
    /// Immich API base (plain http://) for clip locations from phone-photo GPS, e.g.
    /// http://photos.example/api (default: no locations).
    #[bpaf(argument("URL"))]
    immich: Option<String>,
    /// File holding the Immich API key (default: $FFCINE_IMMICH_KEY_FILE, else publish's
    /// $FFCINE_IMMICH_API_KEY_FILE).
    #[bpaf(argument("FILE"))]
    immich_key_file: Option<PathBuf>,
    /// Zone the camera clock was set to, +HH:MM (default: UTC); for --immich.
    #[bpaf(argument::<String>("ZONE"), parse(|s| parse_zone(&s)), fallback(0))]
    camera_tz: i32,
    /// Re-analyse every clip, ignoring the cache.
    force: bool,
    /// Clip page title (default: the directory name).
    #[bpaf(argument("TEXT"))]
    title: Option<String>,
    /// Card dump or project directory: its ingest catalog.json, else every clip under it.
    #[bpaf(positional("DIR"))]
    dir: PathBuf,
}

#[cfg(feature = "gpu")]
impl TriageArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let out = self.out.clone().unwrap_or_else(|| self.dir.join("triage"));
        let title = self.title.clone().unwrap_or_else(|| {
            let d = std::fs::canonicalize(&self.dir).unwrap_or_else(|_| self.dir.clone());
            d.file_name()
                .map_or_else(|| "Clips".into(), |s| s.to_string_lossy().into_owned())
        });
        let whisper = (!self.no_transcribe).then(|| {
            let mut w = triage::transcribe::WhisperParams {
                model: self.model.clone(),
                language: self.language.clone().unwrap_or_default(),
                hotwords: self.hotwords.clone().unwrap_or_default(),
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
        let immich = match &self.immich {
            Some(url) => {
                let file = self
                    .immich_key_file
                    .clone()
                    .or_else(|| {
                        std::env::var_os("FFCINE_IMMICH_KEY_FILE")
                            .or_else(|| std::env::var_os("FFCINE_IMMICH_API_KEY_FILE"))
                            .map(PathBuf::from)
                    })
                    .context("--immich needs --immich-key-file")?;
                let key = std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?
                    .trim()
                    .to_owned();
                Some(triage::location::ImmichParams {
                    url: url.clone(),
                    key,
                    tz_minutes: self.camera_tz,
                    slack: 15.0 * 60.0,
                })
            }
            None => None,
        };
        let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let jobs = self.jobs.max(1);
        let opts = triage::Options {
            dir: self.dir,
            list: self.list,
            no_catalog: self.no_catalog,
            out,
            jobs,
            scan: triage::decode::ScanParams {
                gpu: !self.cpu_decode,
                threads: (cores / jobs).max(1),
                ..Default::default()
            },
            vad: triage::vad::VadParams::default(),
            flags: triage::flags::FlagParams {
                runaway: self.runaway_minutes * 60.0,
                ..Default::default()
            },
            immich,
            whisper,
            min_speech: self.min_speech,
            force: self.force,
            title,
        };
        let s = triage::run(&opts)?;
        eprintln!(
            "{} clips ({} analysed, {} transcribed), {} failed\n{}\n{}",
            s.clips,
            s.analysed,
            s.transcribed,
            s.failed.len(),
            s.index.display(),
            s.page.display()
        );
        for (id, e) in &s.failed {
            eprintln!("  {id}: {e}");
        }
        Ok(())
    }
}

fn load_index(path: Option<PathBuf>) -> anyhow::Result<Index> {
    let path = path
        .or_else(|| std::env::var_os("FFCINE_TRIAGE_INDEX").map(PathBuf::from))
        .unwrap_or_else(|| DEFAULT_INDEX.into());
    Index::load(&path).with_context(|| format!("reading {}", path.display()))
}

#[derive(Debug, Clone, Bpaf)]
pub struct SearchArgs {
    /// Triage index (default: $FFCINE_TRIAGE_INDEX, else triage/index.json).
    #[bpaf(argument("FILE"))]
    index: Option<PathBuf>,
    /// Print the full source path instead of the clip id.
    paths: bool,
    /// Words to find: case-insensitive word prefixes; several words match as a phrase.
    #[bpaf(positional("QUERY"), some("a query"))]
    query: Vec<String>,
}

impl SearchArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let ix = load_index(self.index)?;
        let hits = triage::search(&ix, &self.query.join(" "));
        for h in &hits {
            let name = if self.paths {
                h.clip.source.display().to_string()
            } else {
                h.clip.id.clone()
            };
            println!("{name}  {}  {}", timecode(h.start), h.context);
        }
        eprintln!("{} hits", hits.len());
        Ok(())
    }
}

/// `+09:00`, `-0330`, `+9` -> minutes east of UTC.
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
fn parse_zone(s: &str) -> Result<i32, String> {
    let bad = || format!("bad zone {s:?} (want +HH:MM)");
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'+') => (1, &s[1..]),
        Some(b'-') => (-1, &s[1..]),
        _ => (1, s),
    };
    let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
    let (h, m) = match digits.len() {
        1 | 2 => (digits.as_str(), "0"),
        4 => digits.split_at(2),
        _ => return Err(bad()),
    };
    let h: i32 = h.parse().map_err(|_| bad())?;
    let m: i32 = m.parse().map_err(|_| bad())?;
    if h > 14 || m > 59 {
        return Err(bad());
    }
    Ok(sign * (h * 60 + m))
}

/// Seconds, or `[H:]M:SS[.ss]`.
fn parse_time(s: &str) -> Result<f64, String> {
    let mut t = 0.0;
    for part in s.split(':') {
        let v: f64 = part.parse().map_err(|_| format!("bad time {s:?}"))?;
        t = t * 60.0 + v;
    }
    Ok(t)
}

#[derive(Debug, Clone, Bpaf)]
pub struct SnapArgs {
    /// Triage index (default: $FFCINE_TRIAGE_INDEX, else triage/index.json).
    #[bpaf(argument("FILE"))]
    index: Option<PathBuf>,
    /// Room kept around words, in seconds.
    #[bpaf(argument("SEC"), fallback(0.12), display_fallback)]
    pad: f64,
    /// Always keep a word that the point falls in, instead of moving to its nearer edge.
    keep: bool,
    /// Clip id in the index (as `ffcine search` prints it).
    #[bpaf(positional("CLIP"))]
    clip: String,
    /// Proposed in point: seconds or [H:]M:SS[.ss].
    #[bpaf(positional::<String>("IN"), parse(|s| parse_time(&s)))]
    cut_in: f64,
    /// Proposed out point: seconds or [H:]M:SS[.ss].
    #[bpaf(positional::<String>("OUT"), parse(|s| parse_time(&s)))]
    cut_out: f64,
}

impl SnapArgs {
    pub fn run(self) -> anyhow::Result<()> {
        let ix = load_index(self.index)?;
        let clip = ix
            .clip(&self.clip)
            .with_context(|| format!("no clip {} in the index", self.clip))?;
        let p = SnapParams {
            pad: self.pad,
            prefer: if self.keep {
                triage::Prefer::Keep
            } else {
                triage::Prefer::Nearest
            },
        };
        let (i, o) = triage::snap(&triage::snap::units(clip), self.cut_in, self.cut_out, p);
        println!("{i:.3} {o:.3}  ({} - {})", timecode(i), timecode(o));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_time, parse_zone};

    #[test]
    fn zones() {
        assert_eq!(parse_zone("+09:00"), Ok(540));
        assert_eq!(parse_zone("-0330"), Ok(-210));
        assert_eq!(parse_zone("8"), Ok(480));
        assert!(parse_zone("+9:0").is_err());
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("12.5"), Ok(12.5));
        assert_eq!(parse_time("1:02.5"), Ok(62.5));
        assert_eq!(parse_time("1:00:00"), Ok(3600.0));
        assert!(parse_time("x").is_err());
    }
}
