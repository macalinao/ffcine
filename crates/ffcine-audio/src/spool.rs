//! Scratch sample files: what keeps the mix out of RAM. Buses, the sum and the master are sparse
//! files (a bus with few clips stays small), read and written in [`CHUNK`]s.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ffcine_core::{Error, Result};

use crate::buffer::Audio;

/// Frames per read and write. A multiple of the 64-sample keyframe step of `gain_db`.
pub const CHUNK: usize = 1 << 16;

const SAMPLE: usize = size_of::<f32>();

fn io_err(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// A temporary directory of [`Spool`]s, removed on drop.
#[derive(Debug)]
pub struct Scratch {
    dir: PathBuf,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    /// Under `base`, else the system's temporary directory.
    pub fn new(base: Option<&Path>) -> Result<Self> {
        let base = base.map_or_else(std::env::temp_dir, Path::to_owned);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(".ffcine-mix-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        Ok(Self { dir })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Fixed-length planar audio in a file, silent until written. Reads past the end come back short;
/// writes past the end are dropped.
#[derive(Debug)]
pub struct Spool {
    path: PathBuf,
    file: File,
    rate: u32,
    channels: usize,
    frames: usize,
}

impl Spool {
    pub fn new(scratch: &Scratch, rate: u32, channels: usize, frames: usize) -> Result<Self> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = scratch.dir.join(format!("{n}.f32"));
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_err(&path))?;
        file.set_len((frames * channels * SAMPLE) as u64)
            .map_err(io_err(&path))?;
        Ok(Self {
            path,
            file,
            rate,
            channels,
            frames,
        })
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    fn seek(&mut self, at: usize) -> Result<()> {
        self.file
            .seek(SeekFrom::Start((at * self.channels * SAMPLE) as u64))
            .map_err(io_err(&self.path))?;
        Ok(())
    }

    /// Up to `len` frames from `at`, one `Vec` per channel.
    pub fn read(&mut self, at: usize, len: usize) -> Result<Vec<Vec<f32>>> {
        let at = at.min(self.frames);
        let len = len.min(self.frames - at);
        self.seek(at)?;
        let mut bytes = vec![0u8; len * self.channels * SAMPLE];
        self.file
            .read_exact(&mut bytes)
            .map_err(io_err(&self.path))?;
        let mut out = vec![Vec::with_capacity(len); self.channels];
        for frame in bytes.chunks_exact(self.channels * SAMPLE) {
            for (c, s) in out.iter_mut().zip(frame.chunks_exact(SAMPLE)) {
                c.push(f32::from_ne_bytes([s[0], s[1], s[2], s[3]]));
            }
        }
        Ok(out)
    }

    /// Overwrites from `at`.
    pub fn write(&mut self, at: usize, chunk: &[Vec<f32>]) -> Result<()> {
        let at = at.min(self.frames);
        let len = chunk.first().map_or(0, Vec::len).min(self.frames - at);
        self.seek(at)?;
        let mut bytes = Vec::with_capacity(len * self.channels * SAMPLE);
        for i in 0..len {
            for c in chunk {
                bytes.extend_from_slice(&c[i].to_ne_bytes());
            }
        }
        self.file.write_all(&bytes).map_err(io_err(&self.path))
    }

    /// Adds into what is there from `at`.
    pub fn add(&mut self, at: usize, chunk: &[Vec<f32>]) -> Result<()> {
        let len = chunk.first().map_or(0, Vec::len);
        let mut have = self.read(at, len)?;
        for (dst, src) in have.iter_mut().zip(chunk) {
            for (d, s) in dst.iter_mut().zip(src) {
                *d += *s;
            }
        }
        self.write(at, &have)
    }

    /// The whole file as a 32-bit float WAV.
    pub fn write_wav(&mut self, path: &Path) -> Result<()> {
        let bytes = self.frames as u64 * self.channels as u64 * SAMPLE as u64;
        if bytes > u64::from(u32::MAX) - 44 {
            return Err(Error::Invalid {
                what: "WAV length (over 4 GiB)",
                value: format!("{} frames", self.frames),
            });
        }
        let spec = hound::WavSpec {
            channels: u16::try_from(self.channels).map_err(|_| Error::Invalid {
                what: "channel count",
                value: self.channels.to_string(),
            })?,
            sample_rate: self.rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let err = |e: hound::Error| crate::buffer::io_err(path, e);
        let file = File::create(path).map_err(io_err(path))?;
        let mut w = hound::WavWriter::new(BufWriter::new(file), spec).map_err(err)?;
        for at in (0..self.frames).step_by(CHUNK) {
            let c = self.read(at, CHUNK)?;
            for i in 0..c[0].len() {
                for ch in &c {
                    w.write_sample(ch[i]).map_err(err)?;
                }
            }
        }
        w.finalize().map_err(err)
    }

    /// The whole file in memory.
    pub fn to_audio(&mut self) -> Result<Audio> {
        Ok(Audio {
            rate: self.rate,
            channels: self.read(0, self.frames)?,
        })
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_round_trips_and_cleans_up() {
        let s = Scratch::new(None).expect("scratch");
        let dir = s.dir.clone();
        let mut a = Spool::new(&s, 48000, 2, 10).expect("spool");
        assert_eq!(a.read(0, 100).expect("read")[0], vec![0.0; 10]);
        a.write(2, &[vec![1.0, 2.0], vec![3.0, 4.0]])
            .expect("write");
        a.add(3, &[vec![1.0, 1.0], vec![1.0, 1.0]]).expect("add");
        let c = a.read(1, 4).expect("read");
        assert_eq!(c[0], vec![0.0, 1.0, 3.0, 1.0]);
        assert_eq!(c[1], vec![0.0, 3.0, 5.0, 1.0]);
        // Past the end: dropped.
        a.add(9, &[vec![5.0, 5.0], vec![5.0, 5.0]]).expect("add");
        assert_eq!(a.read(9, 5).expect("read")[0], vec![5.0]);
        assert_eq!(a.read(50, 5).expect("read")[0].len(), 0);
        drop(a);
        drop(s);
        assert!(!dir.exists());
    }
}
