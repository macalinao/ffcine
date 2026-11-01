//! Delivery render profiles: the encoder and output size for a kind of copy ([`DeliveryProfile`]).
//!
//! `master` and `preview` are the prototype's encodes; `review` fits a size budget (a typical chat
//! app cap is 50 MB) at the best resolution the bitrate carries; `phone` is 1080p HEVC that iOS
//! plays and streams. See `docs/delivery.md`.

use std::time::Duration;

use crate::{EncoderSettings, Error, PixelFormat, RateControl, Size};
use ffcine_core::color::named_enum;
use std::fmt;
use std::str::FromStr;

named_enum!(
    /// A delivery profile.
    #[derive(Default)]
    DeliveryProfile {
        /// 10-bit HEVC constqp 12 (the prototype's master).
        #[default]
        Master = "master",
        /// 8-bit x264 crf 10 (the prototype's preview).
        Preview = "preview",
        /// HEVC under a size budget (default 50 MB), resolution picked from the bitrate.
        Review = "review",
        /// 1080p 8-bit HEVC (`hvc1`), constant quality capped at 16 Mb/s, for iPhones.
        Phone = "phone",
    }
);

/// Default `review` budget: a 50 MB chat-app cap, in bytes (decimal MB, as chat apps count).
pub const REVIEW_BUDGET: u64 = 50_000_000;

/// Bits kept back for the audio stream that is muxed in afterwards (AAC 160 kb/s).
pub const AUDIO_BITRATE: u64 = 160_000;

/// What a profile resolved to for one render.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub encoder: EncoderSettings,
    /// Output size, `None` = the grade's (source) size.
    pub size: Option<Size>,
}

/// Smallest bits per pixel per frame the `review` profile accepts before dropping a resolution
/// step (HEVC holds up to about here on handheld travel footage).
const MIN_BPP: f64 = 0.035;

/// Short-side steps the `review` profile tries, largest first.
const REVIEW_STEPS: [u32; 4] = [1080, 720, 540, 360];

/// Fits `src` into `short` px on its short side, keeping the aspect, even dimensions, never
/// upscaling.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn fit_short_side(src: Size, short: u32) -> Size {
    let s = src.width.min(src.height);
    if s <= short {
        return src;
    }
    let k = f64::from(short) / f64::from(s);
    let even = |v: u32| ((f64::from(v) * k / 2.0).round() as u32 * 2).max(2);
    Size {
        width: even(src.width),
        height: even(src.height),
    }
}

impl DeliveryProfile {
    /// Encoder and size for a `duration` render of `src`-sized frames at `fps`. `budget` is the
    /// whole file's size cap in bytes (only `review` uses it; `None` = [`REVIEW_BUDGET`]).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn plan(self, src: Size, fps: f64, duration: Duration, budget: Option<u64>) -> Plan {
        let gop = Some((fps * 2.0).round().max(1.0) as u32);
        match self {
            Self::Master => Plan {
                encoder: EncoderSettings::nvenc_hevc_master(),
                size: None,
            },
            Self::Preview => Plan {
                encoder: EncoderSettings::x264_preview(),
                size: None,
            },
            Self::Phone => {
                let mut e = hevc_8bit(gop);
                e.rate = RateControl::Crf(23.0);
                e.extra
                    .extend(opts(&[("maxrate", "16000000"), ("bufsize", "32000000")]));
                Plan {
                    encoder: e,
                    size: Some(fit_short_side(src, 1080)),
                }
            }
            Self::Review => {
                let bps = review_bitrate(budget.unwrap_or(REVIEW_BUDGET), duration);
                let size = REVIEW_STEPS
                    .iter()
                    .map(|&s| fit_short_side(src, s))
                    .find(|s| {
                        bps as f64 / (f64::from(s.width) * f64::from(s.height) * fps.max(1.0))
                            >= MIN_BPP
                    })
                    .unwrap_or_else(|| fit_short_side(src, 360));
                let mut e = hevc_8bit(gop);
                e.rate = RateControl::Bitrate(bps);
                let peak = (bps * 2).to_string();
                e.extra.extend(opts(&[
                    ("multipass", "fullres"),
                    ("maxrate", &peak),
                    ("bufsize", &peak),
                ]));
                Plan {
                    encoder: e,
                    size: Some(size),
                }
            }
        }
    }
}

/// Video bitrate that fits `budget` bytes over `duration` with the audio and ~4 % container and
/// rate-control margin; at least 200 kb/s.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn review_bitrate(budget: u64, duration: Duration) -> u64 {
    let secs = duration.as_secs_f64().max(1.0);
    let total = budget as f64 * 8.0 * 0.96 / secs;
    ((total - AUDIO_BITRATE as f64) as u64).max(200_000)
}

fn opts(kv: &[(&str, &str)]) -> Vec<(String, String)> {
    kv.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// 8-bit HEVC NVENC (`main`), VBR with lookahead and adaptive quantisation.
fn hevc_8bit(gop: Option<u32>) -> EncoderSettings {
    EncoderSettings {
        preset: Some("p7".into()),
        tune: Some("hq".into()),
        profile: Some("main".into()),
        pixel_format: PixelFormat::Nv12,
        gop,
        extra: opts(&[
            ("rc", "vbr"),
            ("rc-lookahead", "32"),
            ("spatial-aq", "1"),
            ("temporal-aq", "1"),
        ]),
        ..EncoderSettings::nvenc_hevc_master()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UHD: Size = Size {
        width: 3840,
        height: 2160,
    };

    #[test]
    fn review_fits_the_budget() {
        // A 10 min video: ~480 kb/s left for video, 540p.
        let d = Duration::from_secs(600);
        let p = DeliveryProfile::Review.plan(UHD, 23.976, d, None);
        let RateControl::Bitrate(b) = p.encoder.rate else {
            panic!("review is bitrate-driven");
        };
        assert!((b + AUDIO_BITRATE) * 600 / 8 < REVIEW_BUDGET);
        assert_eq!(p.size.map(|s| s.height), Some(540));
        // A 90 s reel gets 1080p.
        let p = DeliveryProfile::Review.plan(UHD, 23.976, Duration::from_secs(90), None);
        assert_eq!(
            p.size,
            Some(Size {
                width: 1920,
                height: 1080
            })
        );
    }

    #[test]
    fn phone_is_1080p_hevc_and_never_upscales() {
        let p = DeliveryProfile::Phone.plan(UHD, 60.0, Duration::from_secs(60), None);
        assert_eq!(p.size.map(|s| (s.width, s.height)), Some((1920, 1080)));
        assert_eq!(p.encoder.profile.as_deref(), Some("main"));
        let vertical = Size {
            width: 1080,
            height: 1920,
        };
        assert_eq!(fit_short_side(vertical, 1080), vertical);
    }
}
