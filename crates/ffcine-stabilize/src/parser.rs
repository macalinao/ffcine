//! telemetry-parser (feature `telemetry`): `GoPro`, Sony, Insta360, DJI action cameras and others.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use telemetry_parser::Input;
use telemetry_parser::tags_impl::{GroupId, TagId, TagValue};
use telemetry_parser::util;

use super::gyro::{Attitude, Quat, RAW_GYRO_REFUSAL, Rate, Telemetry, integrate};

/// telemetry-parser's camera axes (Gyroflow's: `x` right, `y` up, `z` back) to image axes: a half
/// turn about `x`.
fn to_image(x: f64, y: f64, z: f64) -> [f64; 3] {
    [x, -y, -z]
}

fn quaternions(input: &Input) -> Vec<Attitude> {
    let mut out = Vec::new();
    for s in input.samples.iter().flatten() {
        let tag = s
            .tag_map
            .as_ref()
            .and_then(|m| m.get(&GroupId::Quaternion))
            .and_then(|g| g.get(&TagId::Data));
        if let Some(TagValue::Vec_TimeQuaternion_f64(v)) = tag.map(|t| &t.value) {
            out.extend(v.get().iter().map(|q| {
                let [x, y, z] = to_image(q.v.x, q.v.y, q.v.z);
                Attitude {
                    t: q.t / 1000.0,
                    q: Quat { w: q.v.w, x, y, z }.normalized(),
                }
            }));
        }
    }
    out.sort_by(|a, b| a.t.total_cmp(&b.t));
    out
}

fn rates(input: &Input) -> Vec<Rate> {
    util::normalized_imu_interpolated(input, None)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| {
            let g = s.gyro?;
            Some(Rate {
                t: s.timestamp_ms / 1000.0,
                w: to_image(g[0].to_radians(), g[1].to_radians(), g[2].to_radians()),
            })
        })
        .collect()
}

/// Focal length from the lens profile the file states, if any.
fn focal(input: &Input) -> Option<f64> {
    input.samples.iter().flatten().find_map(|s| {
        let tag = s.tag_map.as_ref()?.get(&GroupId::Lens)?.get(&TagId::Data)?;
        let TagValue::Json(v) = &tag.value else {
            return None;
        };
        v.get()
            .pointer("/fisheye_params/camera_matrix/0/0")?
            .as_f64()
    })
}

/// Reads `path` with telemetry-parser. It parses arbitrary container data, so a panic counts as
/// no telemetry, not a crash of the render.
pub fn read(path: &Path) -> Result<Telemetry, String> {
    let parse = || -> Result<Telemetry, String> {
        let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let size = f.metadata().map_err(|e| e.to_string())?.len();
        let cancel = Arc::new(AtomicBool::new(false));
        let input = Input::from_stream(&mut f, size, path, |_| (), cancel)
            .map_err(|e| format!("telemetry-parser: {e}"))?;
        let camera = input.camera_model().cloned().or(Some(input.camera_type()));
        let focal_px = focal(&input);
        let attitude = quaternions(&input);
        if !attitude.is_empty() {
            return Ok(Telemetry {
                camera,
                attitude,
                focal_px,
                auto_refusal: None,
            });
        }
        let rates = rates(&input);
        if rates.is_empty() {
            return Err(format!(
                "telemetry-parser: no gyro in this {} file",
                input.camera_type()
            ));
        }
        Ok(Telemetry {
            camera,
            attitude: integrate(&rates),
            focal_px,
            auto_refusal: Some(RAW_GYRO_REFUSAL),
        })
    };
    catch_unwind(AssertUnwindSafe(parse))
        .unwrap_or_else(|_| Err("telemetry-parser panicked".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_telemetry_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("ffcine-gyro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("plain.mp4");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        assert!(read(&f).is_err());
        assert!(read(&dir.join("missing.mp4")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn axes_are_a_proper_rotation() {
        // Rays rotating about Gyroflow's y (up) rotate about the image's y (down) the other way.
        assert_eq!(to_image(0.0, 1.0, 0.0), [0.0, -1.0, 0.0]);
        assert_eq!(to_image(1.0, 0.0, 0.0), [1.0, 0.0, 0.0]);
        assert_eq!(to_image(0.0, 0.0, 1.0), [0.0, 0.0, -1.0]);
    }
}
