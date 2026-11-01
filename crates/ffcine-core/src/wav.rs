//! WAV (RIFF / RF64 / BWF) header reader, without libav: format, length and the `bext`
//! (Broadcast Wave) origination stamp that recorders like the Zoom H2essential write.

#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::cast_possible_truncation,
    clippy::doc_markdown
)] // binary parsing; box sizes are small; QuickTime, BMFF are names

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WavInfo {
    /// `fmt ` format tag: 1 PCM, 3 IEEE float, 0xFFFE extensible.
    pub format: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits: u16,
    pub duration: Option<f64>,
    /// `bext` originator (`ZOOM H2essential`), if any.
    pub originator: Option<String>,
    /// `bext` origination date and time (`yyyy-mm-dd`, `hh:mm:ss`), local recorder clock.
    pub origination: Option<(String, String)>,
}

/// Reads a WAV header. Errors if the file isn't RIFF/RF64 WAVE.
pub fn read(path: &Path) -> Result<WavInfo> {
    read_io(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_owned()
}

#[allow(clippy::cast_precision_loss)]
fn read_io(path: &Path) -> io::Result<WavInfo> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut h = [0u8; 12];
    f.read_exact(&mut h)?;
    if !matches!(&h[0..4], b"RIFF" | b"RF64" | b"BW64") || &h[8..12] != b"WAVE" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a WAVE file",
        ));
    }
    let mut info = WavInfo::default();
    let mut data_len: Option<u64> = None;
    let mut ds64_data: Option<u64> = None;
    let mut off = 12u64;
    while off + 8 <= len {
        f.seek(SeekFrom::Start(off))?;
        let mut ch = [0u8; 8];
        f.read_exact(&mut ch)?;
        let size = u64::from(le32(&ch[4..8]));
        let id = &ch[0..4];
        let small = size <= 64 * 1024;
        let body = |f: &mut File| -> io::Result<Vec<u8>> {
            #[allow(clippy::cast_possible_truncation)] // small
            let mut b = vec![0; size as usize];
            f.read_exact(&mut b)?;
            Ok(b)
        };
        match id {
            b"fmt " if small && size >= 16 => {
                let b = body(&mut f)?;
                info.format = le16(&b[0..2]);
                info.channels = le16(&b[2..4]);
                info.sample_rate = le32(&b[4..8]);
                info.bits = le16(&b[14..16]);
            }
            b"ds64" if small && size >= 16 => {
                let b = body(&mut f)?;
                ds64_data = Some(le64(&b[8..16]));
            }
            b"bext" if small && size >= 346 => {
                let b = body(&mut f)?;
                let orig = text(&b[256..288]);
                if !orig.is_empty() {
                    info.originator = Some(orig);
                }
                let (d, t) = (text(&b[320..330]), text(&b[330..338]));
                if !d.is_empty() {
                    info.origination = Some((d, t));
                }
            }
            b"data" => {
                data_len = Some(if size == 0xffff_ffff {
                    ds64_data.unwrap_or(len - off - 8)
                } else {
                    size
                });
            }
            _ => {}
        }
        let size = if id == b"data" && size == 0xffff_ffff {
            data_len.unwrap_or(0)
        } else {
            size
        };
        off += 8 + size + (size & 1);
    }
    let frame = u64::from(info.channels) * u64::from(info.bits / 8);
    if let Some(n) = data_len
        && frame > 0
        && info.sample_rate > 0
    {
        info.duration = Some(n as f64 / frame as f64 / f64::from(info.sample_rate));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 48 kHz stereo 32-bit float WAV with a `bext` chunk, `seconds` long.
    pub fn wav_bytes(seconds: u32, originator: &str, date: &str, time: &str) -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&3u16.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&48_000u32.to_le_bytes());
        fmt.extend_from_slice(&(48_000u32 * 8).to_le_bytes());
        fmt.extend_from_slice(&8u16.to_le_bytes());
        fmt.extend_from_slice(&32u16.to_le_bytes());
        let mut bext = vec![0u8; 602];
        bext[256..256 + originator.len()].copy_from_slice(originator.as_bytes());
        bext[320..330].copy_from_slice(date.as_bytes());
        bext[330..338].copy_from_slice(time.as_bytes());
        let data = vec![0u8; (48_000 * 8 * seconds) as usize];
        let mut body = b"WAVE".to_vec();
        for (id, c) in [(b"fmt ", &fmt), (b"bext", &bext), (b"data", &data)] {
            body.extend_from_slice(id);
            body.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
            body.extend_from_slice(c);
        }
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn reads_bext_and_length() {
        let dir = std::env::temp_dir().join(format!("ffcine-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ZOOM0001.WAV");
        std::fs::write(
            &p,
            wav_bytes(2, "ZOOM H2essential", "2026-11-30", "09:15:02"),
        )
        .unwrap();
        let w = read(&p).unwrap();
        assert_eq!(
            (w.format, w.channels, w.sample_rate, w.bits),
            (3, 2, 48_000, 32)
        );
        assert!((w.duration.unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(w.originator.as_deref(), Some("ZOOM H2essential"));
        assert_eq!(
            w.origination,
            Some(("2026-11-30".into(), "09:15:02".into()))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
