//! `ffcine triage`: a searchable clip index from proxies, before anything is rendered.
//!
//! Per clip, from the DJI `.LRF` proxy when there is one: a contact strip and scene cuts
//! ([`scene`]), a speech map ([`vad`]), runaway/dead/dark flags ([`flags`]), and a word-timed
//! transcript from one batched GPU whisper run ([`transcribe`]). The result is `index.json`
//! ([`index`]), searchable with [`search`](mod@search), and a static clip page ([`page`]) for picking clips.
//! [`snap`](mod@snap) moves proposed in/out points onto word boundaries and silence.
//!
//! Clips come from a directory scan or a list ([`clips`]) until the `ffcine-ingest` catalog lands;
//! see `docs/triage.md`.

// Signal and frame maths on sample counts, pixel sizes and seconds.
#![allow(
    clippy::many_single_char_names,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

pub mod clips;
#[cfg(feature = "ffmpeg")]
pub mod decode;
pub mod flags;
pub mod index;
pub mod location;
pub mod page;
pub mod scene;
pub mod search;
pub mod snap;
pub mod strip;
pub mod transcribe;
pub mod vad;

pub use index::{Clip, Flag, FlagKind, Index, Span, Transcript, Word, timecode};
pub use search::{Hit, search};
pub use snap::{Prefer, SnapParams, snap};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("decode: {0}")]
    Decode(String),
    #[error("whisper: {0}")]
    Whisper(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(feature = "ffmpeg")]
pub use run::{Options, Summary, run};

#[cfg(feature = "ffmpeg")]
mod run {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde::{Deserialize, Serialize};

    use crate::clips::{self, ClipFile};
    use crate::decode::{self, ScanParams};
    use crate::flags::FlagParams;
    use crate::index::{self, Clip, Index};
    use crate::transcribe::{Whisper, WhisperParams};
    use crate::vad::{self, VadParams};
    use crate::{Result, page, strip};

    #[derive(Debug, Clone)]
    pub struct Options {
        /// Footage directory.
        pub dir: PathBuf,
        /// Clip list instead of the catalog or a scan of `dir` (one path per line).
        pub list: Option<PathBuf>,
        /// Scan `dir` even when it has an ingest `catalog.json`.
        pub no_catalog: bool,
        /// Output directory (index.json, index.html, strips/, clips/).
        pub out: PathBuf,
        /// Clips analysed at once.
        pub jobs: usize,
        pub scan: ScanParams,
        pub vad: VadParams,
        pub flags: FlagParams,
        /// Look up clip locations in Immich.
        pub immich: Option<crate::location::ImmichParams>,
        /// `None` skips transcription.
        pub whisper: Option<WhisperParams>,
        /// Clips with less VAD speech than this (seconds) aren't transcribed.
        pub min_speech: f64,
        /// Re-analyse clips even when the cache matches.
        pub force: bool,
        /// Page title.
        pub title: String,
    }

    #[derive(Debug, Default)]
    pub struct Summary {
        pub clips: usize,
        pub analysed: usize,
        pub transcribed: usize,
        pub failed: Vec<(String, String)>,
        pub index: PathBuf,
        pub page: PathBuf,
    }

    #[derive(Serialize, Deserialize)]
    struct Cached {
        version: u32,
        clip: Clip,
    }

    fn mtime(p: &Path) -> Result<(u64, i64)> {
        let m = std::fs::metadata(p)?;
        let t = m
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        Ok((m.len(), t))
    }

    fn load_cached(path: &Path, f: &ClipFile, size: u64, mt: i64) -> Option<Clip> {
        let c: Cached = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        (c.version == index::VERSION
            && c.clip.size == size
            && c.clip.mtime == mt
            && c.clip.analysed == f.analysed())
        .then_some(c.clip)
    }

    fn save_cached(path: &Path, clip: &Clip) -> Result<()> {
        index::write_json(
            path,
            &Cached {
                version: index::VERSION,
                clip: clip.clone(),
            },
        )
    }

    fn analyse(f: &ClipFile, o: &Options, pcm: Option<&Path>, size: u64, mt: i64) -> Result<Clip> {
        let analysed = f.analysed();
        let use_gpu = o.scan.gpu && f.proxy.is_none();
        let v = decode::scan_video(analysed, use_gpu, &o.scan)?;
        let (width, height, fps) = if f.proxy.is_some() {
            decode::probe_video(&f.source).unwrap_or((v.width, v.height, v.fps))
        } else {
            (v.width, v.height, v.fps)
        };
        let strip_name = format!("strips/{}.jpg", f.id);
        let frames: Vec<strip::Frame<'_>> = v
            .thumbs
            .iter()
            .map(|t| strip::Frame {
                w: t.w,
                h: t.h,
                rgb: &t.rgb,
            })
            .collect();
        let strip = if frames.is_empty() {
            None
        } else {
            strip::write_strip(&o.out.join(&strip_name), &frames)?;
            Some(strip_name)
        };
        let audio = decode::scan_audio(analysed, pcm)?;
        let recorded = f
            .local
            .clone()
            .or_else(|| clips::dji_name(&f.id).map(|(t, _)| t));
        Ok(Clip {
            id: f.id.clone(),
            source: f.source.clone(),
            analysed: analysed.to_owned(),
            size,
            mtime: mt,
            duration: (v.duration * 1000.0).round() / 1000.0,
            fps: (fps * 1000.0).round() / 1000.0,
            width: Some(width),
            height: Some(height),
            recorded,
            utc: f.utc.clone(),
            take: None,
            strip,
            strip_times: v
                .thumbs
                .iter()
                .map(|t| (t.t * 100.0).round() / 100.0)
                .collect(),
            cuts: v.cuts,
            activity: v.activity,
            luma: v.luma,
            has_audio: audio.is_some(),
            speech: audio
                .map(|a| vad::speech(&a.db, &o.vad))
                .unwrap_or_default(),
            transcript: None,
            flags: Vec::new(),
            location: None,
        })
    }

    #[allow(clippy::too_many_lines)] // the run, top to bottom
    pub fn run(o: &Options) -> Result<Summary> {
        decode::init()?;
        let catalog = if o.list.is_none() && !o.no_catalog {
            clips::from_catalog(&o.dir)?
        } else {
            None
        };
        let files = match (&o.list, catalog) {
            (Some(l), _) => clips::from_list(l)?,
            (None, Some(c)) => {
                eprintln!("{} clips from the ingest catalog", c.len());
                c
            }
            (None, None) => clips::discover(&o.dir, &[o.out.as_path()])?,
        };
        let cache_dir = o.out.join("clips");
        let scratch = o.out.join(".scratch");
        for d in [&o.out, &o.out.join("strips"), &cache_dir, &scratch] {
            std::fs::create_dir_all(d)?;
        }
        let n = files.len();
        let slots: Vec<Mutex<Option<Clip>>> = (0..n).map(|_| Mutex::new(None)).collect();
        let failed = Mutex::new(Vec::new());
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let analysed = AtomicUsize::new(0);
        let pcm_path = |id: &str| scratch.join(format!("{id}.f32"));
        let want_pcm = o.whisper.is_some();

        std::thread::scope(|s| {
            for _ in 0..o.jobs.max(1) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(f) = files.get(i) else { break };
                        let cache = cache_dir.join(format!("{}.json", f.id));
                        let r = mtime(&f.source).and_then(|(size, mt)| {
                            if !o.force
                                && let Some(c) = load_cached(&cache, f, size, mt)
                            {
                                return Ok(c);
                            }
                            let pcm = want_pcm.then(|| pcm_path(&f.id));
                            let c = analyse(f, o, pcm.as_deref(), size, mt)?;
                            save_cached(&cache, &c)?;
                            analysed.fetch_add(1, Ordering::Relaxed);
                            Ok(c)
                        });
                        let k = done.fetch_add(1, Ordering::Relaxed) + 1;
                        match r {
                            Ok(c) => {
                                eprintln!(
                                    "[{k}/{n}] {} {} cuts {} speech {:.1} s",
                                    c.id,
                                    index::timecode(c.duration),
                                    c.cuts.len(),
                                    c.speech_seconds()
                                );
                                if let Ok(mut slot) = slots[i].lock() {
                                    *slot = Some(c);
                                }
                            }
                            Err(e) => {
                                eprintln!("[{k}/{n}] {}: {e}", f.id);
                                if let Ok(mut fl) = failed.lock() {
                                    fl.push((f.id.clone(), e.to_string()));
                                }
                            }
                        }
                    }
                });
            }
        });

        let mut clips: Vec<Clip> = slots
            .into_iter()
            .filter_map(|m| m.into_inner().ok().flatten())
            .collect();
        let mut failed = failed.into_inner().unwrap_or_default();

        // Takes and flags are cheap and depend on the whole set: always recomputed.
        let keys: Vec<_> = clips
            .iter()
            .map(|c| (c.id.clone(), c.recorded.clone(), c.duration))
            .collect();
        let takes = clips::takes(&keys, 3.0);
        for (c, t) in clips.iter_mut().zip(&takes) {
            c.take =
                (*t != c.id || takes.iter().filter(|x| *x == t).count() > 1).then(|| t.clone());
        }
        let take_len = |t: &Option<String>, own: f64| {
            t.as_ref().map_or(own, |t| {
                clips
                    .iter()
                    .filter(|c| c.take.as_ref() == Some(t))
                    .map(|c| c.duration)
                    .sum()
            })
        };
        let lens: Vec<f64> = clips
            .iter()
            .map(|c| take_len(&c.take, c.duration))
            .collect();
        for (c, len) in clips.iter_mut().zip(lens) {
            c.flags = crate::flags::flags(c, len, &o.flags);
        }

        if let Some(ip) = &o.immich {
            let before: Vec<bool> = clips.iter().map(|c| c.location.is_some()).collect();
            match crate::location::locate(&mut clips, ip) {
                Ok(n) => eprintln!("located {n} clips"),
                Err(e) => eprintln!("no locations: {e}"),
            }
            for (c, had) in clips.iter().zip(before) {
                if !had && c.location.is_some() {
                    save_cached(&cache_dir.join(format!("{}.json", c.id)), c)?;
                }
            }
        }

        let mut transcribed = 0;
        if let Some(wp) = &o.whisper {
            let todo: Vec<usize> = (0..clips.len())
                .filter(|&i| {
                    clips[i].transcript.is_none() && clips[i].speech_seconds() >= o.min_speech
                })
                .collect();
            if !todo.is_empty() {
                eprintln!(
                    "transcribing {} clips ({}, {})",
                    todo.len(),
                    wp.model,
                    wp.device
                );
                match Whisper::start(wp, &scratch) {
                    Ok(mut w) => {
                        for (k, &i) in todo.iter().enumerate() {
                            let c = &mut clips[i];
                            let pcm = pcm_path(&c.id);
                            let r = (if pcm.is_file() {
                                Ok(())
                            } else {
                                decode::scan_audio(&c.analysed, Some(&pcm)).map(drop)
                            })
                            .and_then(|()| w.transcribe(&c.id, &pcm));
                            match r {
                                Ok(t) => {
                                    eprintln!(
                                        "[{}/{}] {} {}: {} words",
                                        k + 1,
                                        todo.len(),
                                        c.id,
                                        t.language,
                                        t.words().count()
                                    );
                                    c.transcript = Some(t);
                                    save_cached(&cache_dir.join(format!("{}.json", c.id)), c)?;
                                    transcribed += 1;
                                    let _ = std::fs::remove_file(&pcm);
                                }
                                Err(e) => {
                                    eprintln!("{}: {e}", c.id);
                                    failed.push((c.id.clone(), e.to_string()));
                                    if matches!(e, crate::Error::Whisper(ref m) if m.starts_with("worker exited"))
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("no transcripts: {e}");
                        failed.push(("whisper".into(), e.to_string()));
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&scratch);

        let index = Index {
            version: index::VERSION,
            root: o.dir.clone(),
            clips,
        };
        let index_path = o.out.join("index.json");
        index.save(&index_path)?;
        let page_path = o.out.join("index.html");
        std::fs::write(&page_path, page::render(&index, &o.title)?)?;
        Ok(Summary {
            clips: index.clips.len(),
            analysed: analysed.into_inner(),
            transcribed,
            failed,
            index: index_path,
            page: page_path,
        })
    }
}
