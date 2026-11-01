//! What a media file contains, as reported by a [`Backend`](crate::Backend).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::ColorSpace;

/// An exact ratio (frame rates, time bases).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

impl Rational {
    pub fn as_f64(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }
}

impl std::str::FromStr for Rational {
    type Err = crate::Error;
    /// `24000/1001` or `25`.
    fn from_str(s: &str) -> Result<Self, crate::Error> {
        let invalid = || crate::Error::Invalid {
            what: "rational (N/D)",
            value: s.to_owned(),
        };
        let (n, d) = s.split_once('/').unwrap_or((s, "1"));
        let r = Self {
            num: n.parse().map_err(|_| invalid())?,
            den: d.parse().map_err(|_| invalid())?,
        };
        if r.num <= 0 || r.den <= 0 {
            return Err(invalid());
        }
        Ok(r)
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub path: PathBuf,
    /// Container (`mov,mp4,m4a,3gp,3g2,mj2`, `matroska,webm`, ...).
    pub format: String,
    pub duration: Option<Duration>,
    pub bit_rate: Option<u64>,
    pub streams: Vec<StreamInfo>,
    /// Container metadata (`encoder`, `major_brand`, `compatible_brands`,
    /// `com.apple.quicktime.make`, ...): what [`Profile::detect`](crate::camera::Profile::detect)
    /// reads besides the colour tags.
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub index: usize,
    pub codec: String,
    pub bit_rate: Option<u64>,
    pub kind: StreamKind,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum StreamKind {
    Video(VideoInfo),
    Audio(AudioInfo),
    Subtitle,
    Data,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub pixel_format: String,
    pub frame_rate: Option<Rational>,
    pub frames: Option<u64>,
    pub color: ColorSpace,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: String,
}

impl fmt::Display for MediaInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.format)?;
        if let Some(d) = self.duration {
            write!(f, ", {:.3}s", d.as_secs_f64())?;
        }
        if let Some(b) = self.bit_rate {
            write!(f, ", {} kb/s", b / 1000)?;
        }
        for s in &self.streams {
            write!(f, "\n  #{} {s}", s.index)?;
        }
        Ok(())
    }
}

impl fmt::Display for StreamInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            StreamKind::Video(v) => {
                write!(
                    f,
                    "video {} {}x{} {}",
                    self.codec, v.width, v.height, v.pixel_format
                )?;
                write!(f, " {}", v.color)?;
                if let Some(r) = v.frame_rate {
                    write!(f, ", {:.3} fps", r.as_f64())?;
                }
                if let Some(n) = v.frames {
                    write!(f, ", {n} frames")?;
                }
            }
            StreamKind::Audio(a) => write!(
                f,
                "audio {} {} Hz {}ch {}",
                self.codec, a.sample_rate, a.channels, a.sample_format
            )?,
            StreamKind::Subtitle => write!(f, "subtitle {}", self.codec)?,
            StreamKind::Data => write!(f, "data {}", self.codec)?,
            StreamKind::Other => write!(f, "other {}", self.codec)?,
        }
        if let Some(b) = self.bit_rate {
            write!(f, ", {} kb/s", b / 1000)?;
        }
        Ok(())
    }
}

impl MediaInfo {
    /// The first video stream, if any.
    pub fn video(&self) -> Option<(&StreamInfo, &VideoInfo)> {
        self.streams.iter().find_map(|s| match &s.kind {
            StreamKind::Video(v) => Some((s, v)),
            _ => None,
        })
    }
}
