//! Wall-clock times, UTC offsets and clock corrections, without a timezone database.
//!
//! Cameras record a local wall time (their clock, in whatever zone it was set to); some also
//! record UTC. A [`WallTime`] is seconds since 1970-01-01 00:00 *on that wall clock*, so dates
//! and offsets are plain integer arithmetic. Zones are fixed UTC offsets ([`Zone`]): enough for
//! a trip (one offset per trip, outside DST changes) and portable to macOS without tzdata.

use std::fmt::{self, Write as _};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;

/// Seconds since 1970-01-01T00:00:00 on some wall clock (UTC when the zone is +00:00).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WallTime(pub i64);

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

impl WallTime {
    pub fn from_civil(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<Self> {
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
            return None;
        }
        Some(Self(
            days_from_civil(y, mo, d) * 86_400
                + i64::from(h) * 3600
                + i64::from(mi) * 60
                + i64::from(s),
        ))
    }

    /// `(year, month, day, hour, minute, second)`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn civil(self) -> (i64, u32, u32, u32, u32, u32) {
        let (y, m, d) = civil_from_days(self.0.div_euclid(86_400));
        let s = self.0.rem_euclid(86_400) as u32;
        (y, m, d, s / 3600, s / 60 % 60, s % 60)
    }

    /// `YYYY-MM-DD`: the day folder.
    pub fn date(self) -> String {
        let (y, m, d, ..) = self.civil();
        format!("{y:04}-{m:02}-{d:02}")
    }

    pub fn year(self) -> i64 {
        self.civil().0
    }

    /// Parses `YYYYMMDDhhmmss` (DJI file names), `YYYY-MM-DD[T ]hh:mm:ss` and EXIF's
    /// `YYYY:MM:DD hh:mm:ss`; a trailing zone (`Z`, `+0800`, `+08:00`) is returned separately.
    pub fn parse(s: &str) -> Option<(Self, Option<Zone>)> {
        let s = s.trim();
        let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
        if digits.len() >= 14 {
            let n = |a: usize, b: usize| digits[a..b].parse::<u32>().ok();
            let t = Self::from_civil(
                i64::from(n(0, 4)?),
                n(4, 6)?,
                n(6, 8)?,
                n(8, 10)?,
                n(10, 12)?,
                n(12, 14)?,
            )?;
            return Some((t, None));
        }
        let b = s.as_bytes();
        if b.len() < 19 {
            return None;
        }
        let n = |a: usize, c: usize| s.get(a..c)?.parse::<u32>().ok();
        let t = Self::from_civil(
            i64::from(n(0, 4)?),
            n(5, 7)?,
            n(8, 10)?,
            n(11, 13)?,
            n(14, 16)?,
            n(17, 19)?,
        )?;
        let mut rest = &s[19..];
        if let Some(r) = rest.strip_prefix('.') {
            rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
        }
        let zone = rest.trim().parse::<Zone>().ok();
        Some((t, zone))
    }
}

impl fmt::Display for WallTime {
    #[allow(clippy::many_single_char_names)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d, h, mi, s) = self.civil();
        write!(f, "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
    }
}

impl Serialize for WallTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WallTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s)
            .map(|(t, _)| t)
            .ok_or_else(|| serde::de::Error::custom(format!("bad time {s:?}")))
    }
}

/// A fixed UTC offset, in seconds east of UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Zone(pub i32);

impl Zone {
    pub const UTC: Self = Self(0);

    /// The UTC instant of a wall time on this zone's clock.
    pub fn to_utc(self, t: WallTime) -> WallTime {
        WallTime(t.0 - i64::from(self.0))
    }

    pub fn from_utc(self, t: WallTime) -> WallTime {
        WallTime(t.0 + i64::from(self.0))
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { '-' } else { '+' };
        let a = self.0.unsigned_abs();
        write!(f, "{sign}{:02}:{:02}", a / 3600, a / 60 % 60)
    }
}

impl FromStr for Zone {
    type Err = Error;
    /// `Z`, `UTC`, `+08:00`, `+0800`, `-3`, `+5:30`.
    fn from_str(s: &str) -> Result<Self, Error> {
        let bad = || Error::Config(format!("bad UTC offset {s:?} (want +HH:MM)"));
        let s = s.trim();
        if s.eq_ignore_ascii_case("z") || s.eq_ignore_ascii_case("utc") || s == "+0" {
            return Ok(Self::UTC);
        }
        let (sign, body) = match s.as_bytes().first() {
            Some(b'+') => (1, &s[1..]),
            Some(b'-') => (-1, &s[1..]),
            _ => return Err(bad()),
        };
        let (h, m) = match body.split_once(':') {
            Some((h, m)) => (h, m),
            None if body.len() == 4 => body.split_at(2),
            None => (body, "0"),
        };
        let h: i32 = h.parse().map_err(|_| bad())?;
        let m: i32 = m.parse().map_err(|_| bad())?;
        if h > 14 || m > 59 {
            return Err(bad());
        }
        Ok(Self(sign * (h * 3600 + m * 60)))
    }
}

impl Serialize for Zone {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Zone {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A signed clock correction: `+0h`, `-2m`, `+1h20m`, `-90s`, `+9497d`, `+1d2h3m4.5s`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Shift(pub f64);

impl Shift {
    /// Whole seconds, rounded (wall times are whole seconds).
    #[allow(clippy::cast_possible_truncation)]
    pub fn secs(self) -> i64 {
        self.0.round() as i64
    }
}

impl fmt::Display for Shift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0.0 { '-' } else { '+' };
        let mut a = self.0.abs();
        let mut out = String::new();
        for (unit, n) in [('d', 86_400.0), ('h', 3600.0), ('m', 60.0)] {
            let k = (a / n).floor();
            if k >= 1.0 {
                let _ = write!(out, "{k}{unit}");
                a -= k * n;
            }
        }
        if a > 1e-9 || out.is_empty() {
            let a = (a * 1000.0).round() / 1000.0;
            let _ = write!(out, "{a}s");
        }
        write!(f, "{sign}{out}")
    }
}

impl FromStr for Shift {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        let bad = || {
            Error::Config(format!(
                "bad clock shift {s:?} (want e.g. +0h, -2m, +1h20m)"
            ))
        };
        let t = s.trim();
        let (sign, mut rest) = match t.as_bytes().first() {
            Some(b'-') => (-1.0, &t[1..]),
            Some(b'+') => (1.0, &t[1..]),
            _ => (1.0, t),
        };
        if rest.is_empty() {
            return Err(bad());
        }
        let mut total = 0.0;
        while !rest.is_empty() {
            let end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .ok_or_else(bad)?;
            let n: f64 = rest[..end].parse().map_err(|_| bad())?;
            let unit = rest[end..].chars().next().ok_or_else(bad)?;
            total += n * match unit {
                'd' => 86_400.0,
                'h' => 3600.0,
                'm' => 60.0,
                's' => 1.0,
                _ => return Err(bad()),
            };
            rest = &rest[end + unit.len_utf8()..];
        }
        Ok(Self(sign * total))
    }
}

impl Serialize for Shift {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Shift {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2026, 11, 30), (1969, 12, 31)] {
            let t = WallTime::from_civil(y, m, d, 13, 4, 5).unwrap();
            assert_eq!(t.civil(), (y, m, d, 13, 4, 5));
        }
        assert_eq!(
            WallTime::from_civil(1970, 1, 2, 0, 0, 0),
            Some(WallTime(86_400))
        );
    }

    #[test]
    fn parses_camera_stamps() {
        let (t, z) = WallTime::parse("20260605164744").unwrap();
        assert_eq!((t.to_string().as_str(), z), ("2026-06-05T16:47:44", None));
        let (t, z) = WallTime::parse("2026-06-05T16:47:44+0800").unwrap();
        assert_eq!(t.to_string(), "2026-06-05T16:47:44");
        assert_eq!(z, Some(Zone(8 * 3600)));
        let (t, _) = WallTime::parse("2026:06:05 16:47:44").unwrap();
        assert_eq!(t.date(), "2026-06-05");
        assert!(WallTime::parse("2026-13-05T16:47:44").is_none());
    }

    #[test]
    fn zones_and_shifts() {
        assert_eq!("+08:00".parse::<Zone>().unwrap(), Zone(28_800));
        assert_eq!("-0330".parse::<Zone>().unwrap(), Zone(-12_600));
        assert_eq!("Z".parse::<Zone>().unwrap().to_string(), "+00:00");
        assert!("8".parse::<Zone>().is_err());
        assert!(("-2m".parse::<Shift>().unwrap().0 + 120.0).abs() < 1e-9);
        assert!(("+1h20m".parse::<Shift>().unwrap().0 - 4800.0).abs() < 1e-9);
        assert!(("+0h".parse::<Shift>().unwrap().0).abs() < 1e-9);
        assert!("2x".parse::<Shift>().is_err());
        assert_eq!(Shift(-120.0).to_string(), "-2m");
        assert_eq!(Shift(4801.5).to_string(), "+1h20m1.5s");
        assert_eq!(Shift(0.0).to_string(), "+0s");
    }
}
