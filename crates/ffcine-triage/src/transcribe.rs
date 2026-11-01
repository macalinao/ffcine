//! GPU transcripts with word times: one faster-whisper process per run, model loaded once, every
//! clip with speech fed through it as a batch (no tool queue, no per-clip model load).
//!
//! The worker is a short Python script embedded in ffcine (`whisper_worker.py`); it needs a Python
//! with `faster_whisper` and a CUDA `ctranslate2` (`--whisper-python`, or `FFCINE_WHISPER_PYTHON`).

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::Deserialize;

use crate::index::{Segment, Transcript};
use crate::{Error, Result};

const WORKER: &str = include_str!("whisper_worker.py");

#[derive(Debug, Clone)]
pub struct WhisperParams {
    /// Python interpreter with faster-whisper.
    pub python: PathBuf,
    /// Model name or `CTranslate2` directory (`large-v3-turbo`).
    pub model: String,
    /// `cuda` or `cpu`.
    pub device: String,
    /// `int8_float16` on the GPU.
    pub compute: String,
    /// Force a language (`en`); empty detects per clip.
    pub language: String,
    /// Words to bias towards (`konbini, onsen`); empty for none.
    pub hotwords: String,
}

impl Default for WhisperParams {
    fn default() -> Self {
        Self {
            python: std::env::var_os("FFCINE_WHISPER_PYTHON")
                .map_or_else(|| PathBuf::from("python3"), PathBuf::from),
            model: "large-v3-turbo".into(),
            device: "cuda".into(),
            compute: "int8_float16".into(),
            language: String::new(),
            hotwords: String::new(),
        }
    }
}

pub struct Whisper {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    model: String,
    _script: PathBuf,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    id: String,
    #[serde(default)]
    ready: bool,
    #[serde(default)]
    language: String,
    #[serde(default)]
    segments: Vec<Segment>,
    #[serde(default)]
    error: Option<String>,
}

impl Whisper {
    /// Starts the worker and waits for the model to load.
    pub fn start(p: &WhisperParams, scratch: &Path) -> Result<Self> {
        let script = scratch.join("whisper_worker.py");
        std::fs::write(&script, WORKER)?;
        let mut child = Command::new(&p.python)
            .arg("-I")
            .arg(&script)
            .args([&p.model, &p.device, &p.compute, &p.language, &p.hotwords])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::Whisper(format!("{}: {e}", p.python.display())))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Whisper("no stdin".into()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| Error::Whisper("no stdout".into()))?,
        );
        let mut w = Self {
            child,
            stdin: Some(stdin),
            stdout,
            model: p.model.clone(),
            _script: script,
        };
        let r = w.read()?;
        if !r.ready {
            return Err(Error::Whisper("worker did not start".into()));
        }
        Ok(w)
    }

    fn read(&mut self) -> Result<Reply> {
        let mut line = String::new();
        if self.stdout.read_line(&mut line)? == 0 {
            let status = self.child.wait()?;
            return Err(Error::Whisper(format!(
                "worker exited ({status}); see its stderr above"
            )));
        }
        Ok(serde_json::from_str(&line)?)
    }

    /// Transcribes raw 16 kHz mono f32le audio.
    pub fn transcribe(&mut self, id: &str, pcm: &Path) -> Result<Transcript> {
        let job = serde_json::json!({ "id": id, "path": pcm });
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::Whisper("worker closed".into()))?;
        writeln!(stdin, "{job}")?;
        stdin.flush()?;
        let r = self.read()?;
        if let Some(e) = r.error {
            return Err(Error::Whisper(format!("{id}: {e}")));
        }
        if r.id != id {
            return Err(Error::Whisper(format!("worker answered {} for {id}", r.id)));
        }
        Ok(Transcript {
            language: r.language,
            model: self.model.clone(),
            segments: r.segments,
        })
    }
}

impl Drop for Whisper {
    fn drop(&mut self) {
        // EOF on stdin ends the worker's loop.
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}
