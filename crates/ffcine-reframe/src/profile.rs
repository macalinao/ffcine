//! Render profiles: the output frame a timeline is reframed into. One timeline renders to several
//! aspects (16:9 master, 9:16 reels, 4:5 feed) by rendering it once per profile; the reframe
//! effect can carry per-profile overrides (`profiles: {name: {...}}`, [`crate::Reframe`]).

use std::str::FromStr;

use ffcine_core::Error;

/// An output frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Picks the reframe's `profiles.<name>` override.
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Cap on punch-in zoom (`edit.yaml` `output.<mode>.max_punch`, 1.4 in the prototype).
    pub max_punch: f64,
    /// Draw the safe area over the picture (review renders only).
    pub safe_area: Option<SafeArea>,
}

pub const DEFAULT_MAX_PUNCH: f64 = 1.4;

impl Profile {
    pub fn new(name: impl Into<String>, width: u32, height: u32) -> Self {
        Self {
            name: name.into(),
            width,
            height,
            max_punch: DEFAULT_MAX_PUNCH,
            safe_area: None,
        }
    }

    /// `landscape` (`16:9`, 3840x2160), `hd` (1920x1080), `reel` (`9:16`, 1080x1920), `feed`
    /// (`4:5`, 1080x1350), `square` (`1:1`, 1080x1080).
    pub fn preset(name: &str) -> Option<Self> {
        let (n, w, h) = match name {
            "landscape" | "16:9" => ("landscape", 3840, 2160),
            "hd" => ("hd", 1920, 1080),
            "reel" | "9:16" => ("reel", 1080, 1920),
            "feed" | "4:5" => ("feed", 1080, 1350),
            "square" | "1:1" => ("square", 1080, 1080),
            _ => return None,
        };
        Some(Self::new(n, w, h))
    }

    pub fn aspect(&self) -> f64 {
        f64::from(self.width) / f64::from(self.height)
    }

    /// The platform safe area for this aspect: reels for 9:16, the feed for 4:5, a title-safe
    /// margin otherwise.
    pub fn default_safe_area(&self) -> SafeArea {
        let ar = self.aspect();
        if (ar - 9.0 / 16.0).abs() < 0.01 {
            SafeArea::REELS
        } else if (ar - 4.0 / 5.0).abs() < 0.01 {
            SafeArea::FEED
        } else {
            SafeArea::TITLE
        }
    }
}

impl FromStr for Profile {
    type Err = Error;
    /// A preset name, `WxH`, or `name=WxH`.
    fn from_str(s: &str) -> Result<Self, Error> {
        if let Some(p) = Self::preset(s) {
            return Ok(p);
        }
        let invalid = || Error::Invalid {
            what: "profile (preset, WxH or name=WxH)",
            value: s.to_owned(),
        };
        let (name, size) = s.split_once('=').unwrap_or((s, s));
        let (w, h) = size.split_once('x').ok_or_else(invalid)?;
        let (w, h): (u32, u32) = (
            w.parse().map_err(|_| invalid())?,
            h.parse().map_err(|_| invalid())?,
        );
        if w == 0 || h == 0 {
            return Err(invalid());
        }
        Ok(Self::new(name, w, h))
    }
}

/// Margins kept clear of platform UI, as fractions of the output width (left, right) and height
/// (top, bottom).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SafeArea {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl SafeArea {
    /// Instagram Reels, `TikTok` and Shorts at 1080x1920: status and header at the top (~220 px),
    /// caption, audio and buttons at the bottom (~420 px), the action column on the right.
    pub const REELS: Self = Self {
        top: 0.115,
        right: 0.11,
        bottom: 0.22,
        left: 0.055,
    };
    /// 4:5 feed posts: the profile grid shows the middle 3:4, so keep text off the side 3.1%.
    pub const FEED: Self = Self {
        top: 0.05,
        right: 0.0625,
        bottom: 0.05,
        left: 0.0625,
    };
    /// Broadcast title safe (90%).
    pub const TITLE: Self = Self {
        top: 0.05,
        right: 0.05,
        bottom: 0.05,
        left: 0.05,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profiles() {
        let p: Profile = "9:16".parse().unwrap();
        assert_eq!((p.name.as_str(), p.width, p.height), ("reel", 1080, 1920));
        assert_eq!(p.default_safe_area(), SafeArea::REELS);
        let p: Profile = "story=720x1280".parse().unwrap();
        assert_eq!((p.name.as_str(), p.width), ("story", 720));
        assert_eq!(p.default_safe_area(), SafeArea::REELS);
        assert_eq!(
            "1080x1350".parse::<Profile>().unwrap().default_safe_area(),
            SafeArea::FEED
        );
        assert!("reels".parse::<Profile>().is_err());
        assert!("0x10".parse::<Profile>().is_err());
    }
}
