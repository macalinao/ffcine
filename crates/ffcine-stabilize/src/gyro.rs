//! Camera motion from recorded attitude or gyro readings instead of optical flow: per-frame
//! rotation between frames, projected through a focal length to the same [`Motion`]s
//! [`detect`](crate::detect) produces, so smoothing, zoom and warp don't change.
//!
//! Everything here works in image axes: `x` right, `y` down, `z` forward (right-handed), with an
//! attitude `q` taking camera rays to the world. The rotation from frame `k` to `k - 1` is then
//! `q[k-1]^-1 * q[k]`. The readers (`read`, behind [`detect_motion`](crate::detect_motion), and
//! feature `nvof`) convert to these axes.

use crate::motion::Motion;
use crate::{Detection, SourceRange};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quat {
    pub w: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Quat {
    pub const IDENTITY: Self = Self {
        w: 1.0,
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    #[must_use]
    pub fn conj(self) -> Self {
        Self {
            w: self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }

    pub fn norm(self) -> f64 {
        (self.w * self.w + self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    #[must_use]
    pub fn normalized(self) -> Self {
        let n = self.norm();
        Self {
            w: self.w / n,
            x: self.x / n,
            y: self.y / n,
            z: self.z / n,
        }
    }

    /// The rotation by `|v|` radians about `v`.
    #[must_use]
    pub fn from_rotvec(v: [f64; 3]) -> Self {
        let a = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        // sin(a/2)/a -> 1/2 at 0.
        let k = if a < 1e-9 { 0.5 } else { (a / 2.0).sin() / a };
        Self {
            w: (a / 2.0).cos(),
            x: k * v[0],
            y: k * v[1],
            z: k * v[2],
        }
    }

    pub fn rotate(self, v: [f64; 3]) -> [f64; 3] {
        let p =
            self * Self {
                w: 0.0,
                x: v[0],
                y: v[1],
                z: v[2],
            } * self.conj();
        [p.x, p.y, p.z]
    }

    /// Normalised linear interpolation along the shorter arc: readings are close together.
    #[must_use]
    pub fn nlerp(self, o: Self, t: f64) -> Self {
        let dot = self.w * o.w + self.x * o.x + self.y * o.y + self.z * o.z;
        let s = if dot < 0.0 { -1.0 } else { 1.0 };
        Self {
            w: self.w + t * (s * o.w - self.w),
            x: self.x + t * (s * o.x - self.x),
            y: self.y + t * (s * o.y - self.y),
            z: self.z + t * (s * o.z - self.z),
        }
        .normalized()
    }
}

/// `self * o`: `o` applied first.
impl std::ops::Mul for Quat {
    type Output = Self;

    fn mul(self, o: Self) -> Self {
        Self {
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
        }
    }
}

/// A camera attitude at a time in the file, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Attitude {
    pub t: f64,
    pub q: Quat,
}

/// An angular velocity in image axes (rad/s) at a time in the file, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    pub t: f64,
    pub w: [f64; 3],
}

/// What a file records about the camera's rotation.
#[derive(Debug, Clone, PartialEq)]
pub struct Telemetry {
    /// Camera model, as the file names it.
    pub camera: Option<String>,
    /// Sorted by time.
    pub attitude: Vec<Attitude>,
    /// Focal length in source pixels, if the file states it.
    pub focal_px: Option<f64>,
    /// Why `auto` shouldn't use this even though it is readable; `--source gyro` still does.
    pub auto_refusal: Option<&'static str>,
}

/// Why `auto` prefers flow on DJI's gimbal cameras: see docs/stabilize.md.
pub const GIMBAL_REFUSAL: &str = "flow is steadier on this gimbal camera; --source gyro to force";
/// Why `auto` doesn't use raw gyro: its axes only come from the parser's orientation convention,
/// not checked against a camera, and a wrong sign would double the shake.
pub const RAW_GYRO_REFUSAL: &str = "raw gyro axes aren't verified; --source gyro to force";

/// Knobs of the gyro path.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GyroParams {
    /// Focal length in source pixels; overrides the file's and the per-camera default.
    pub focal_px: Option<f64>,
    /// Added to each frame's time before looking up its attitude, in milliseconds.
    pub offset_ms: f64,
}

/// Integrates angular velocity to attitudes, from the identity at the first reading (trapezoidal).
pub fn integrate(rates: &[Rate]) -> Vec<Attitude> {
    let mut q = Quat::IDENTITY;
    let mut out = Vec::with_capacity(rates.len());
    for (i, r) in rates.iter().enumerate() {
        if i > 0 {
            let p = &rates[i - 1];
            let dt = r.t - p.t;
            let step = [0, 1, 2].map(|k| f64::midpoint(p.w[k], r.w[k]) * dt);
            q = (q * Quat::from_rotvec(step)).normalized();
        }
        out.push(Attitude { t: r.t, q });
    }
    out
}

/// The attitude at time `t`, interpolated; clamped to the ends of the track.
pub fn attitude_at(track: &[Attitude], t: f64) -> Option<Quat> {
    let i = track.partition_point(|a| a.t <= t);
    match (i.checked_sub(1).map(|j| track[j]), track.get(i)) {
        (Some(a), Some(b)) => Some(a.q.nlerp(b.q, (t - a.t) / (b.t - a.t))),
        (Some(a), None) => Some(a.q),
        (None, Some(b)) => Some(b.q),
        (None, None) => None,
    }
}

/// The frame-to-frame [`Motion`] of a rotation `rel` taking rays of frame `k` to rays of frame
/// `k - 1`, for a pinhole of focal length `focal` pixels: where the frame centre lands, and the
/// rotation about the optical axis. Scale stays 1: a rotation has no zoom to speak of at the
/// centre, and the warp is a similarity.
pub fn rotation_motion(rel: Quat, focal: f64) -> Motion {
    let c = rel.rotate([0.0, 0.0, 1.0]);
    let x = rel.rotate([1.0, 0.0, 0.0]);
    let y = rel.rotate([0.0, 1.0, 0.0]);
    Motion {
        tx: focal * c[0] / c[2],
        ty: focal * c[1] / c[2],
        angle: (x[1] - y[0]).atan2(x[0] + y[1]),
        scale: 1.0,
        confidence: 1.0,
    }
}

/// Motions of the frames of `range` from an attitude track: `[0]` is the identity, as in detect.
pub fn motions(track: &[Attitude], range: &SourceRange, focal: f64, offset_ms: f64) -> Vec<Motion> {
    let dt = 1.0 / range.rate.as_f64();
    let at = |k: usize| {
        attitude_at(track, range.start + k as f64 * dt + offset_ms / 1000.0)
            .unwrap_or(Quat::IDENTITY)
    };
    let mut prev = at(0);
    (0..range.frames)
        .map(|k| {
            if k == 0 {
                return Motion::IDENTITY;
            }
            let q = at(k);
            let m = rotation_motion(prev.conj() * q, focal);
            prev = q;
            m
        })
        .collect()
}

/// Why `track` can't drive `range`, if so: too few readings, or they stop short of the range.
/// Frames past the end of the file are allowed (the last one repeats), so only the start and most
/// of the length have to be covered.
pub fn coverage_problem(track: &[Attitude], range: &SourceRange) -> Option<String> {
    let (Some(first), Some(last)) = (track.first(), track.last()) else {
        return Some("no readings".into());
    };
    if track.len() < 2 {
        return Some("a single reading".into());
    }
    let dt = 1.0 / range.rate.as_f64();
    let end = range.start + (range.frames.max(1) - 1) as f64 * dt;
    // Half a second of slack at the start: the first reading can lag the first frame a little.
    if first.t > range.start + 0.5 {
        return Some(format!(
            "readings start at {:.2} s, after the range's {:.2} s",
            first.t, range.start
        ));
    }
    let spanned = (last.t.min(end) - range.start.max(first.t)).max(0.0);
    if spanned < 0.5 * (end - range.start) {
        return Some(format!(
            "readings end at {:.2} s, the range at {end:.2} s",
            last.t
        ));
    }
    None
}

/// Focal length in pixels of a known camera, for the long side of its video. Measured against
/// optical flow; see docs/stabilize.md.
pub fn camera_focal(camera: &str, long_side: u32) -> Option<f64> {
    camera
        .contains("OsmoPocket4")
        .then_some(POCKET4_FOCAL * f64::from(long_side))
}

/// Pinhole focal length over the frame's long side, default lens at 16:9.
const POCKET4_FOCAL: f64 = 0.625;

/// The motions of `range` as a [`Detection`] for a `width x height` source.
pub fn detection(
    telemetry: &Telemetry,
    range: &SourceRange,
    params: &GyroParams,
    (width, height): (u32, u32),
) -> Result<Detection, String> {
    if let Some(problem) = coverage_problem(&telemetry.attitude, range) {
        return Err(problem);
    }
    let focal = params
        .focal_px
        .or(telemetry.focal_px)
        .or_else(|| {
            telemetry
                .camera
                .as_deref()
                .and_then(|c| camera_focal(c, width.max(height)))
        })
        .ok_or_else(|| {
            format!(
                "no focal length for {}",
                telemetry.camera.as_deref().unwrap_or("this camera")
            )
        })?;
    Ok(Detection {
        width,
        height,
        analysis_width: width,
        analysis_height: height,
        motions: motions(&telemetry.attitude, range, focal, params.offset_ms),
    })
}

/// DJI's debug track (`dbgi`, protobuf): a gimbal camera's per-frame attitude. telemetry-parser
/// doesn't read this track, and the `djmd` track it does read has no IMU data on these cameras.
pub mod dbgi {
    use super::Quat;

    /// The next field of a protobuf message: its number, and its bytes for a length-delimited
    /// one.
    fn next_field<'a>(msg: &mut &'a [u8]) -> Option<(u64, Option<&'a [u8]>)> {
        fn varint(msg: &mut &[u8]) -> Option<u64> {
            let mut v = 0u64;
            for shift in (0..64).step_by(7) {
                let (&b, rest) = msg.split_first()?;
                *msg = rest;
                v |= u64::from(b & 0x7f) << shift;
                if b < 0x80 {
                    return Some(v);
                }
            }
            None
        }
        let key = varint(msg)?;
        let skip = |msg: &mut &'a [u8], n: usize| -> Option<&'a [u8]> {
            let (head, rest) = msg.split_at_checked(n)?;
            *msg = rest;
            Some(head)
        };
        let bytes = match key & 7 {
            0 => {
                varint(msg)?;
                None
            }
            1 => {
                skip(msg, 8)?;
                None
            }
            2 => {
                let n = usize::try_from(varint(msg)?).ok()?;
                Some(skip(msg, n)?)
            }
            5 => {
                skip(msg, 4)?;
                None
            }
            _ => return None,
        };
        Some((key >> 3, bytes))
    }

    /// The first length-delimited field `number` of `msg`.
    fn field(mut msg: &[u8], number: u64) -> Option<&[u8]> {
        while let Some((n, bytes)) = next_field(&mut msg) {
            if n == number && bytes.is_some() {
                return bytes;
            }
        }
        None
    }

    fn path<'a>(msg: &'a [u8], numbers: &[u64]) -> Option<&'a [u8]> {
        numbers.iter().try_fold(msg, |m, &n| field(m, n))
    }

    /// Four little-endian `f32`s `w x y z`, if they are a unit quaternion. Body axes: `x`
    /// forward, `y` right, `z` down.
    fn body_quat(bytes: &[u8]) -> Option<Quat> {
        let v: [f32; 4] = le_f32s(bytes)?;
        let q = Quat {
            w: f64::from(v[0]),
            x: f64::from(v[1]),
            y: f64::from(v[2]),
            z: f64::from(v[3]),
        };
        ((q.norm() - 1.0).abs() < 1e-3).then_some(q)
    }

    fn le_f32s(bytes: &[u8]) -> Option<[f32; 4]> {
        let b: &[u8; 16] = bytes.try_into().ok()?;
        Some([0, 4, 8, 12].map(|i| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])))
    }

    /// Body axes (forward, right, down) to image axes (right, down, forward): a proper rotation,
    /// so the quaternion's vector part just cycles.
    fn to_image(q: Quat) -> Quat {
        Quat {
            w: q.w,
            x: q.y,
            y: q.z,
            z: q.x,
        }
    }

    /// The camera attitude of one `dbgi` sample, in image axes: the middle of its two per-frame
    /// quaternions, which sit about a quarter frame apart on the camera's readout.
    pub fn sample_attitude(sample: &[u8]) -> Option<Quat> {
        let pair = path(sample, &[2, 1, 10, 7])?;
        let a = body_quat(field(pair, 3)?)?;
        let b = body_quat(field(pair, 4)?)?;
        Some(to_image(a.nlerp(b, 0.5)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffcine_core::probe::Rational;

    const RANGE: SourceRange = SourceRange {
        start: 0.0,
        frames: 25,
        rate: Rational { num: 25, den: 1 },
    };

    #[test]
    fn integrating_constant_rate_gives_the_angle() {
        // 0.5 rad/s about y for 1 s at 1 kHz.
        let rates: Vec<Rate> = (0..=1000)
            .map(|i| Rate {
                t: f64::from(i) / 1000.0,
                w: [0.0, 0.5, 0.0],
            })
            .collect();
        let q = integrate(&rates).last().unwrap().q;
        let want = Quat::from_rotvec([0.0, 0.5, 0.0]);
        assert!(
            (q.w - want.w).abs() < 1e-12 && (q.y - want.y).abs() < 1e-12,
            "{q:?}"
        );
    }

    #[test]
    fn rotation_projects_to_the_expected_motion() {
        let f = 2000.0;
        // Rotating rays about +y moves the frame centre to +x by f * tan(angle).
        let m = rotation_motion(Quat::from_rotvec([0.0, 0.01, 0.0]), f);
        assert!((m.tx - f * 0.01_f64.tan()).abs() < 1e-9, "{m:?}");
        assert!(m.ty.abs() < 1e-9 && m.angle.abs() < 1e-9);
        // About +x: the centre goes up (y points down).
        let m = rotation_motion(Quat::from_rotvec([0.01, 0.0, 0.0]), f);
        assert!((m.ty + f * 0.01_f64.tan()).abs() < 1e-9, "{m:?}");
        // About the optical axis: the image rotates, positive from +x towards +y as in
        // geom::similarity, and the centre stays.
        let m = rotation_motion(Quat::from_rotvec([0.0, 0.0, 0.02]), f);
        assert!((m.angle - 0.02).abs() < 1e-12 && m.tx.abs() < 1e-9 && m.ty.abs() < 1e-9);
    }

    #[test]
    fn angular_velocity_to_frame_motions() {
        // 0.25 rad/s about +y at 1 kHz: every frame at 25 fps turns 0.01 rad.
        let rates: Vec<Rate> = (0..2000)
            .map(|i| Rate {
                t: f64::from(i) / 1000.0,
                w: [0.0, 0.25, 0.0],
            })
            .collect();
        let m = motions(&integrate(&rates), &RANGE, 1500.0, 0.0);
        assert_eq!(m.len(), 25);
        assert_eq!(m[0], Motion::IDENTITY);
        for x in &m[1..] {
            assert!((x.tx - 1500.0 * 0.01_f64.tan()).abs() < 1e-6, "{x:?}");
            assert!(x.ty.abs() < 1e-6 && x.angle.abs() < 1e-6 && x.confidence > 0.0);
        }
    }

    #[test]
    fn offset_shifts_the_lookup() {
        // A step of the rate at 0.5 s: a 40 ms offset moves it one frame earlier.
        let rates: Vec<Rate> = (0..2000)
            .map(|i| Rate {
                t: f64::from(i) / 1000.0,
                w: [0.0, if i < 500 { 0.0 } else { 0.25 }, 0.0],
            })
            .collect();
        let track = integrate(&rates);
        let first = |ms| {
            motions(&track, &RANGE, 1000.0, ms)
                .iter()
                .position(|m| m.tx.abs() > 1.0)
        };
        assert_eq!(first(0.0).unwrap(), first(40.0).unwrap() + 1);
    }

    #[test]
    fn stabilizes_a_synthetic_shake() {
        // Shake about y and x, turned to motions, then through the normal smoothing.
        let rates: Vec<Rate> = (0..4000)
            .map(|i| {
                let t = f64::from(i) / 1000.0;
                let s = (t * 30.0).sin();
                Rate {
                    t,
                    w: [0.1 * (t * 41.0).cos(), 0.2 * s, 0.05 * s],
                }
            })
            .collect();
        let telemetry = Telemetry {
            camera: None,
            attitude: integrate(&rates),
            focal_px: Some(2400.0),
            auto_refusal: None,
        };
        let range = SourceRange {
            frames: 75,
            ..RANGE
        };
        let det = detection(&telemetry, &range, &GyroParams::default(), (3840, 2160)).unwrap();
        assert_eq!(det.motions.len(), 75);
        assert_eq!((det.analysis_width, det.analysis_height), (3840, 2160));
        let st = crate::stabilize(&det, &crate::StabilizeParams::default());
        assert!(st.zoom > 1.0 && st.zoom < 1.2, "{}", st.zoom);
    }

    #[test]
    fn unusable_telemetry_says_why() {
        let track = |from: f64, to: f64| {
            vec![
                Attitude {
                    t: from,
                    q: Quat::IDENTITY,
                },
                Attitude {
                    t: to,
                    q: Quat::IDENTITY,
                },
            ]
        };
        assert!(coverage_problem(&[], &RANGE).is_some());
        assert!(coverage_problem(&track(0.0, 0.0)[..1], &RANGE).is_some());
        assert!(
            coverage_problem(&track(3.0, 9.0), &RANGE)
                .unwrap()
                .contains("start")
        );
        assert!(
            coverage_problem(&track(0.0, 0.1), &RANGE)
                .unwrap()
                .contains("end")
        );
        assert_eq!(coverage_problem(&track(0.0, 2.0), &RANGE), None);
        // No focal length from anywhere.
        let mut t = Telemetry {
            camera: Some("Some Camera".into()),
            attitude: track(0.0, 2.0),
            focal_px: None,
            auto_refusal: None,
        };
        let size = (3840, 2160);
        let err = detection(&t, &RANGE, &GyroParams::default(), size).unwrap_err();
        assert!(err.contains("Some Camera"), "{err}");
        // An override, or a known camera, gives one.
        let p = GyroParams {
            focal_px: Some(1000.0),
            offset_ms: 0.0,
        };
        assert!(detection(&t, &RANGE, &p, size).is_ok());
        t.camera = Some("DJI OsmoPocket4".into());
        assert!(detection(&t, &RANGE, &GyroParams::default(), size).is_ok());
    }

    /// A length-delimited protobuf field.
    fn pb(number: u8, bytes: &[u8]) -> Vec<u8> {
        let mut v = vec![(number << 3) | 2];
        let mut n = bytes.len();
        while n >= 0x80 {
            v.push((n & 0x7f) as u8 | 0x80);
            n >>= 7;
        }
        v.push(n as u8);
        v.extend_from_slice(bytes);
        v
    }

    fn quat_bytes(q: Quat) -> Vec<u8> {
        [q.w, q.x, q.y, q.z]
            .iter()
            .flat_map(|v| (*v as f32).to_le_bytes())
            .collect()
    }

    /// A `dbgi` sample holding `a` and `b`, between unrelated fields of every wire type.
    fn sample(a: &[u8], b: &[u8]) -> Vec<u8> {
        let pair = [
            vec![0x08, 0xf0, 0x02], // varint field 1
            pb(3, a),
            pb(4, b),
        ]
        .concat();
        let frame = [
            vec![0x0d, 0, 0, 0, 0], // 32-bit field 1
            pb(10, &[pb(2, a), pb(7, &pair)].concat()),
            vec![0x79, 0, 0, 0, 0, 0, 0, 0, 0], // 64-bit field 15
        ]
        .concat();
        [pb(1, b"header"), pb(2, &pb(1, &frame))].concat()
    }

    #[test]
    fn dbgi_attitude_in_image_axes() {
        // A turn about the body's down axis (a pan to the right) ...
        let down = Quat::from_rotvec([0.0, 0.0, 0.02]);
        let q =
            dbgi::sample_attitude(&sample(&quat_bytes(Quat::IDENTITY), &quat_bytes(down))).unwrap();
        // ... is half done at the middle, about the image's y axis ...
        let half = Quat::from_rotvec([0.0, 0.01, 0.0]);
        assert!(
            (q.w - half.w).abs() < 1e-6 && (q.y - half.y).abs() < 1e-6,
            "{q:?}"
        );
        assert!(q.x.abs() < 1e-9 && q.z.abs() < 1e-9);
        // ... and moves content in the frame to +x going back a frame.
        assert!(rotation_motion(q, 1000.0).tx > 0.0);
        // Body forward is the optical axis, body right the image's x.
        let m = rotation_motion(
            dbgi::sample_attitude(&sample(
                &quat_bytes(Quat::from_rotvec([0.02, 0.0, 0.0])),
                &quat_bytes(Quat::from_rotvec([0.02, 0.0, 0.0])),
            ))
            .unwrap(),
            1000.0,
        );
        assert!((m.angle - 0.02).abs() < 1e-6 && m.tx.abs() < 1e-6);
    }

    #[test]
    fn dbgi_rejects_what_isnt_an_attitude() {
        let unit = quat_bytes(Quat::IDENTITY);
        let not_unit = quat_bytes(Quat {
            w: 0.5,
            x: 0.0,
            y: 0.0,
            z: 0.0,
        });
        assert!(dbgi::sample_attitude(&sample(&unit, &unit)).is_some());
        assert!(dbgi::sample_attitude(&sample(&unit, &not_unit)).is_none());
        assert!(dbgi::sample_attitude(&sample(&unit, &unit[..12])).is_none());
        assert!(dbgi::sample_attitude(b"\xff\xff not protobuf").is_none());
        assert!(dbgi::sample_attitude(&[]).is_none());
    }
}
