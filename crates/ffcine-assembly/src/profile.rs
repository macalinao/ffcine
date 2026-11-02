//! Render profiles: output size and aspect, and the one encoder config every segment shares, so
//! segments join by stream copy.

use std::path::PathBuf;

use ffcine_reframe::Profile as Aspect;
use ffcine_render::{EncoderSettings, PixelFormat, RateControl, Size};
use ffcine_timeline::Output;
use serde_json::{Value, json};

/// A render profile. Part of every segment key.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// `preview`, `master` or `review`.
    pub name: String,
    pub size: Size,
    /// The output frame clips are reframed into: its name picks the reframe's and the text's
    /// `profiles.<name>` overrides; its size is `size`.
    pub aspect: Aspect,
    /// Shared by every segment: same codec, profile, GOP and colour tags, so the encoded
    /// parameter sets match and concat is a plain stream copy.
    pub encoder: EncoderSettings,
    /// AAC bitrate of the master's audio.
    pub audio_bitrate: u32,
    /// Shots decode their proxies ([`crate::proxy`]) instead of the sources.
    pub proxies: bool,
    /// Where made proxies are ([`crate::proxy::cached`]); camera proxies are found without it.
    pub proxy_dir: Option<PathBuf>,
}

/// Short side of a preset aspect at each quality.
const PREVIEW_SHORT: u32 = 1080;
const MASTER_SHORT: u32 = 2160;
const REVIEW_SHORT: u32 = 720;

/// `aspect` sized to `short` px on its short side (even dimensions).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn sized(aspect: &Aspect, short: u32) -> Size {
    let ar = aspect.aspect();
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    if ar >= 1.0 {
        Size {
            width: even(f64::from(short) * ar),
            height: short,
        }
    } else {
        Size {
            width: short,
            height: even(f64::from(short) / ar),
        }
    }
}

impl Profile {
    fn landscape(short: u32) -> Aspect {
        let mut a = Aspect::preset("landscape").unwrap_or_else(|| Aspect::new("landscape", 16, 9));
        let s = sized(&a, short);
        (a.width, a.height) = (s.width, s.height);
        a
    }

    /// 1080p HEVC NVENC 10-bit, for review.
    pub fn preview() -> Self {
        Self {
            name: "preview".into(),
            size: Size {
                width: 1920,
                height: 1080,
            },
            aspect: Self::landscape(PREVIEW_SHORT),
            encoder: EncoderSettings {
                preset: Some("p4".into()),
                rate: RateControl::ConstQp(22),
                ..EncoderSettings::nvenc_hevc_master()
            },
            audio_bitrate: 192_000,
            proxies: false,
            proxy_dir: None,
        }
    }

    /// 4K HEVC NVENC 10-bit master.
    pub fn master() -> Self {
        Self {
            name: "master".into(),
            size: Size {
                width: 3840,
                height: 2160,
            },
            aspect: Self::landscape(MASTER_SHORT),
            encoder: EncoderSettings::nvenc_hevc_master(),
            audio_bitrate: 320_000,
            proxies: false,
            proxy_dir: None,
        }
    }

    /// 720p 8-bit HEVC NVENC from proxies: a fast, small copy to watch an edit.
    pub fn review() -> Self {
        Self {
            name: "review".into(),
            size: Size {
                width: 1280,
                height: 720,
            },
            aspect: Self::landscape(REVIEW_SHORT),
            encoder: EncoderSettings {
                preset: Some("p1".into()),
                tune: None,
                profile: Some("main".into()),
                rate: RateControl::ConstQp(26),
                pixel_format: PixelFormat::Nv12,
                ..EncoderSettings::nvenc_hevc_master()
            },
            audio_bitrate: 128_000,
            proxies: true,
            proxy_dir: None,
        }
    }

    /// `preview`, `master` or `review`, landscape.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "preview" => Some(Self::preview()),
            "master" | "final" => Some(Self::master()),
            "review" => Some(Self::review()),
            _ => None,
        }
    }

    /// The `name` quality (`preview`, `master`, `review`) in the timeline's `project.output`, its
    /// aspect replaced by `aspect` if given (`landscape`, `reel`, `feed`, `square`, `9:16`, `WxH`,
    /// `name=WxH`). A preset aspect renders at 1080 (preview), 2160 (master) or 720 (review) px
    /// on its short side unless the timeline sets the size (and no `aspect` overrides it); `WxH`
    /// is exact. `review` takes the preview's settings, scaled down to 720 px.
    pub fn resolve(name: &str, output: &Output, aspect: Option<&str>) -> Result<Self, String> {
        let mut p = Self::by_name(name)
            .ok_or_else(|| format!("unknown profile {name:?} (preview, master, review)"))?;
        let mode = if p.name == "master" {
            &output.master
        } else {
            &output.preview
        };
        let spec = aspect.or(output.aspect.as_deref()).unwrap_or("landscape");
        let mut a: Aspect = spec.parse().map_err(|e| format!("render profile: {e}"))?;
        let preset = Aspect::preset(spec).is_some();
        let short = match p.name.as_str() {
            "master" => MASTER_SHORT,
            "review" => REVIEW_SHORT,
            _ => PREVIEW_SHORT,
        };
        let size = match mode.size {
            Some([w, h]) if aspect.is_none() && w > 0 && h > 0 && p.name == "review" => {
                let scale = (f64::from(REVIEW_SHORT) / f64::from(w.min(h))).min(1.0);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let even = |v: u32| ((f64::from(v) * scale / 2.0).round() as u32 * 2).max(2);
                Size {
                    width: even(w),
                    height: even(h),
                }
            }
            Some([w, h]) if aspect.is_none() && w > 0 && h > 0 => Size {
                width: w,
                height: h,
            },
            _ if preset => sized(&a, short),
            _ => Size {
                width: a.width,
                height: a.height,
            },
        };
        (a.width, a.height) = (size.width, size.height);
        if let Some(m) = mode.max_punch {
            a.max_punch = m;
        }
        p.size = size;
        p.aspect = a;
        Ok(p)
    }

    /// What of the profile changes pixels (not its name).
    pub fn recipe(&self) -> Value {
        let mut v = json!({
            "size": [self.size.width, self.size.height],
            "aspect": self.aspect.name,
            "max_punch": self.aspect.max_punch,
            "encoder": format!("{:?}", self.encoder),
        });
        // Only when on, so the other profiles' keys stay as they were.
        if self.proxies {
            v["proxies"] = true.into();
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspects_and_sizes() {
        let out = Output::default();
        let p = Profile::resolve("preview", &out, None).unwrap();
        assert_eq!(p, Profile::preview());
        let size = |q: &str, a: &str| {
            let p = Profile::resolve(q, &out, Some(a)).unwrap();
            (p.aspect.name.clone(), p.size.width, p.size.height)
        };
        assert_eq!(size("preview", "reel"), ("reel".into(), 1080, 1920));
        assert_eq!(size("master", "9:16"), ("reel".into(), 2160, 3840));
        assert_eq!(size("preview", "4:5"), ("feed".into(), 1080, 1350));
        assert_eq!(size("master", "square"), ("square".into(), 2160, 2160));
        assert_eq!(
            size("master", "story=720x1280"),
            ("story".into(), 720, 1280)
        );
        // The timeline's aspect, size and punch cap; a CLI aspect replaces the size.
        let out: Output = serde_json::from_value(json!({
            "aspect": "reel", "preview": {"size": [720, 1280], "max_punch": 1.12}
        }))
        .unwrap();
        let p = Profile::resolve("preview", &out, None).unwrap();
        assert_eq!(
            (p.size.width, p.size.height, p.aspect.max_punch),
            (720, 1280, 1.12)
        );
        let p = Profile::resolve("preview", &out, Some("square")).unwrap();
        assert_eq!((p.size.width, p.size.height), (1080, 1080));
        assert!(Profile::resolve("preview", &out, Some("bogus")).is_err());
        // review: 720 px, the timeline's preview size scaled down, from proxies.
        let p = Profile::resolve("review", &out, Some("reel")).unwrap();
        assert_eq!((p.size.width, p.size.height, p.proxies), (720, 1280, true));
        let out: Output =
            serde_json::from_value(json!({"preview": {"size": [3840, 1600]}})).unwrap();
        let p = Profile::resolve("review", &out, None).unwrap();
        assert_eq!((p.size.width, p.size.height), (1728, 720));
        assert_eq!(Profile::review().recipe()["proxies"], true);
        assert!(Profile::preview().recipe().get("proxies").is_none());
        assert_ne!(
            Profile::preview().recipe(),
            Profile::resolve("preview", &Output::default(), Some("reel"))
                .unwrap()
                .recipe()
        );
    }
}
