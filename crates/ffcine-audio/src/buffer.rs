//! Planar `f32` sample buffers and WAV I/O.

use std::path::Path;

use ffcine_core::{Error, Result};

/// Planar audio: one `Vec<f32>` per channel, all the same length.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Audio {
    pub rate: u32,
    pub channels: Vec<Vec<f32>>,
}

impl Audio {
    /// `channels` channels of `frames` samples of silence.
    pub fn silence(rate: u32, channels: usize, frames: usize) -> Self {
        Self {
            rate,
            channels: vec![vec![0.0; frames]; channels],
        }
    }

    pub fn mono(rate: u32, samples: Vec<f32>) -> Self {
        Self {
            rate,
            channels: vec![samples],
        }
    }

    /// Samples per channel.
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / f64::from(self.rate.max(1))
    }

    /// The average of all channels.
    pub fn to_mono(&self) -> Vec<f32> {
        match self.channels.as_slice() {
            [] => Vec::new(),
            [one] => one.clone(),
            chans => {
                let k = 1.0 / chans.len() as f32;
                (0..self.frames())
                    .map(|i| chans.iter().map(|c| c[i]).sum::<f32>() * k)
                    .collect()
            }
        }
    }

    /// Mono is copied to both sides; more than two channels fold down to the first two.
    #[must_use]
    pub fn to_stereo(&self) -> Self {
        let channels = match self.channels.as_slice() {
            [] => vec![Vec::new(), Vec::new()],
            [one] => vec![one.clone(), one.clone()],
            [l, r, ..] => vec![l.clone(), r.clone()],
        };
        Self {
            rate: self.rate,
            channels,
        }
    }

    /// Multiplies every sample by `10^(db/20)`.
    pub fn gain_db(&mut self, db: f64) {
        let g = db_to_lin(db) as f32;
        for c in &mut self.channels {
            for s in c {
                *s *= g;
            }
        }
    }

    /// Adds `other` (same channel count) into `self` starting at frame `at`, growing as needed.
    pub fn mix_in(&mut self, other: &Audio, at: usize) {
        let need = at + other.frames();
        for (dst, src) in self.channels.iter_mut().zip(&other.channels) {
            if dst.len() < need {
                dst.resize(need, 0.0);
            }
            for (d, s) in dst[at..].iter_mut().zip(src) {
                *d += *s;
            }
        }
    }

    /// Pads (with silence) or truncates to `frames`.
    pub fn set_frames(&mut self, frames: usize) {
        for c in &mut self.channels {
            c.resize(frames, 0.0);
        }
    }

    /// Reads a WAV (PCM 8/16/24/32-bit or 32-bit float).
    pub fn read_wav(path: &Path) -> Result<Self> {
        let err = |e: hound::Error| io_err(path, e);
        let mut r = hound::WavReader::open(path).map_err(err)?;
        let spec = r.spec();
        let n = usize::from(spec.channels.max(1));
        let interleaved: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => {
                r.samples::<f32>().collect::<Result<_, _>>().map_err(err)?
            }
            hound::SampleFormat::Int => {
                let k = 1.0 / f64::from(1u32 << (spec.bits_per_sample - 1)) as f32;
                r.samples::<i32>()
                    .map(|s| s.map(|s| s as f32 * k))
                    .collect::<Result<_, _>>()
                    .map_err(err)?
            }
        };
        let mut channels = vec![Vec::with_capacity(interleaved.len() / n); n];
        for frame in interleaved.chunks_exact(n) {
            for (c, s) in channels.iter_mut().zip(frame) {
                c.push(*s);
            }
        }
        Ok(Self {
            rate: spec.sample_rate,
            channels,
        })
    }

    /// Writes a 32-bit float WAV.
    pub fn write_wav(&self, path: &Path) -> Result<()> {
        let err = |e: hound::Error| io_err(path, e);
        let spec = hound::WavSpec {
            channels: u16::try_from(self.channels.len()).map_err(|_| Error::Invalid {
                what: "channel count",
                value: self.channels.len().to_string(),
            })?,
            sample_rate: self.rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).map_err(err)?;
        for i in 0..self.frames() {
            for c in &self.channels {
                w.write_sample(c[i]).map_err(err)?;
            }
        }
        w.finalize().map_err(err)
    }
}

fn io_err(path: &Path, e: hound::Error) -> Error {
    let source = match e {
        hound::Error::IoError(e) => e,
        e => std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
    };
    Error::Io {
        path: path.to_owned(),
        source,
    }
}

pub fn db_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

pub fn lin_to_db(lin: f64) -> f64 {
    20.0 * lin.max(1e-12).log10()
}
