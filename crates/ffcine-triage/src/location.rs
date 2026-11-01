//! Where each clip was shot, from the GPS of phone photos in Immich taken around the same time.
//! The Pocket has no GPS; the phones always do.
//!
//! The camera clock has no zone, so the caller says which zone it was set to (`tz_minutes`); a
//! wrong clock gives a wrong place, which the ingest catalog's offset will fix.

use std::time::Duration;

use serde::Deserialize;

use crate::clips::camera_seconds;
use crate::index::{Clip, Location};
use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct ImmichParams {
    /// API base, `https://photos.example/api`.
    pub url: String,
    pub key: String,
    /// Camera clock zone, minutes east of UTC (Japan: 540), for clips without a catalog UTC time.
    pub tz_minutes: i32,
    /// Photos up to this far (seconds) outside the clip still count.
    pub slack: f64,
}

#[derive(Deserialize)]
struct Search {
    assets: Assets,
}

#[derive(Deserialize)]
struct Assets {
    items: Vec<Asset>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Asset {
    id: String,
    file_created_at: String,
    #[serde(default)]
    exif_info: Option<Exif>,
}

#[derive(Deserialize)]
struct Exif {
    latitude: Option<f64>,
    longitude: Option<f64>,
    city: Option<String>,
    country: Option<String>,
}

/// `YYYY-MM-DDTHH:MM:SS.000Z` for seconds on the [`camera_seconds`] scale.
fn iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Inverse of camera_seconds' days-from-civil (Howard Hinnant).
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Seconds on the [`camera_seconds`] scale for an Immich UTC timestamp.
fn parse_utc(s: &str) -> Option<i64> {
    camera_seconds(s.get(..19)?)
}

/// Fills `location` for clips that have a recorded time and none yet. Returns how many were found.
pub fn locate(clips: &mut [Clip], p: &ImmichParams) -> Result<usize> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let url = format!("{}/search/metadata", p.url.trim_end_matches('/'));
    let mut found = 0;
    for c in clips.iter_mut().filter(|c| c.location.is_none()) {
        // The catalog's corrected UTC; else the camera clock in the given zone.
        let utc = match c.utc.as_deref().and_then(camera_seconds) {
            Some(t) => t,
            None => match c.recorded.as_deref().and_then(camera_seconds) {
                Some(t) => t - i64::from(p.tz_minutes) * 60,
                None => continue,
            },
        };
        let mid = utc + (c.duration / 2.0) as i64;
        let reach = (c.duration / 2.0 + p.slack) as i64;
        let body = serde_json::json!({
            "takenAfter": iso(mid - reach),
            "takenBefore": iso(mid + reach),
            "withExif": true,
            "size": 100,
        });
        let r: Search = agent
            .post(&url)
            .header("x-api-key", &p.key)
            .send_json(&body)
            .map_err(|e| Error::Invalid(format!("immich: {e}")))?
            .body_mut()
            .read_json()
            .map_err(|e| Error::Invalid(format!("immich: {e}")))?;
        let best = r
            .assets
            .items
            .into_iter()
            .filter_map(|a| {
                let e = a.exif_info?;
                let (lat, lon) = (e.latitude?, e.longitude?);
                let dt = (parse_utc(&a.file_created_at)? - mid).abs();
                Some((dt, a.id, lat, lon, e.city, e.country))
            })
            .min_by_key(|t| t.0);
        if let Some((_, id, lat, lon, city, country)) = best {
            let name = match (city, country) {
                (Some(a), Some(b)) => Some(format!("{a}, {b}")),
                (a, b) => a.or(b),
            };
            c.location = Some(Location {
                lat,
                lon,
                name,
                source: format!("immich:{id}"),
            });
            found += 1;
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trips_camera_seconds() {
        for t in [
            "2026-10-28T12:07:19",
            "2000-01-01T00:00:00",
            "2024-02-29T23:59:59",
        ] {
            let s = camera_seconds(t).expect("parse");
            assert_eq!(iso(s), format!("{t}.000Z"));
        }
        assert_eq!(
            parse_utc("2026-10-28T03:17:27.593Z"),
            camera_seconds("2026-10-28T03:17:27")
        );
    }
}
