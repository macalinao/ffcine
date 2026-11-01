//! Camera path smoothing (vidstab's Gaussian `cameraPathGaussian`) and the static zoom that keeps
//! every corrected frame inside the source.

use super::motion::Motion;
use ffcine_core::geom::{self, Mat3};

/// Per-component camera path, about the frame centre: where reference-frame (frame 0) content
/// lands in frame `i`. Angles are unwrapped, zoom is `ln(scale)` so it smooths multiplicatively.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    pub tx: f64,
    pub ty: f64,
    pub angle: f64,
    pub log_scale: f64,
}

impl Pose {
    fn to_mat3(self, centre: [f64; 2]) -> Mat3 {
        geom::similarity(centre, self.tx, self.ty, self.angle, self.log_scale.exp())
    }

    fn components(self) -> [f64; 4] {
        [self.tx, self.ty, self.angle, self.log_scale]
    }

    fn from_components([tx, ty, angle, log_scale]: [f64; 4]) -> Self {
        Self {
            tx,
            ty,
            angle,
            log_scale,
        }
    }
}

/// Accumulates frame-to-frame motions (`motions[k]` takes frame `k` to `k - 1`; `motions[0]` is
/// ignored) into absolute poses `A_i` (frame 0 -> frame `i`) by composition:
/// `A_i = M_i^-1 * A_{i-1}`. vidstab adds the components instead, which is the same to first
/// order.
pub fn camera_path(motions: &[Motion], centre: [f64; 2]) -> Vec<Pose> {
    let mut abs = geom::IDENTITY;
    let mut out = Vec::with_capacity(motions.len());
    let mut prev_angle = 0.0;
    for (k, m) in motions.iter().enumerate() {
        if k > 0 {
            // A similarity's inverse is a similarity; fall back to "no motion" if degenerate.
            let inv = geom::inverse(&m.to_mat3(centre)).unwrap_or(geom::IDENTITY);
            abs = geom::mul(&inv, &abs);
        }
        let (tx, ty, angle, scale) = geom::decompose_similarity(&abs, centre);
        // Unwrap: keep the angle continuous with the previous frame's.
        let tau = std::f64::consts::TAU;
        let angle = angle + tau * ((prev_angle - angle) / tau).round();
        prev_angle = angle;
        out.push(Pose {
            tx,
            ty,
            angle,
            log_scale: scale.ln(),
        });
    }
    out
}

/// vidstab's Gaussian path filter: window `2 * smoothing + 1`, `mu = smoothing`,
/// `w(i) = exp(-(i - mu)^2 / (mu / 2)^2)`, truncated and renormalised at the clip ends, per
/// component. `smoothing == 0` returns the path unchanged.
pub fn gaussian(path: &[Pose], smoothing: u32) -> Vec<Pose> {
    if smoothing == 0 {
        return path.to_vec();
    }
    let mu = f64::from(smoothing);
    let sigma2 = (mu / 2.0).powi(2);
    let s = 2 * smoothing as usize + 1;
    let kernel: Vec<f64> = (0..s)
        .map(|i| (-(i as f64 - mu).powi(2) / sigma2).exp())
        .collect();
    let mu = smoothing as usize;
    (0..path.len())
        .map(|i| {
            let (mut sum, mut wsum) = ([0.0; 4], 0.0);
            for (k, &w) in kernel.iter().enumerate() {
                let Some(idx) = (i + k).checked_sub(mu).filter(|&j| j < path.len()) else {
                    continue;
                };
                wsum += w;
                for (acc, v) in sum.iter_mut().zip(path[idx].components()) {
                    *acc += w * v;
                }
            }
            Pose::from_components(sum.map(|v| v / wsum))
        })
        .collect()
}

/// How the static zoom (about the frame centre, applied to every frame) is chosen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zoom {
    /// The smallest zoom with no black border on any frame (vidstab's `optzoom=1`, but exact:
    /// it accounts for rotation and scale), capped at `max` if set.
    Auto { max: Option<f64> },
    /// Fixed magnification (1.0 = none); borders may show.
    Fixed(f64),
}

impl Default for Zoom {
    fn default() -> Self {
        Self::Auto { max: None }
    }
}

/// Per frame `i`, the warp `W_i = A_i * S_i^-1` (output pixel -> source pixel) that shows content
/// on the smoothed path `S_i` instead of the actual `A_i`.
pub fn corrections(actual: &[Pose], smoothed: &[Pose], centre: [f64; 2]) -> Vec<Mat3> {
    actual
        .iter()
        .zip(smoothed)
        .map(|(a, s)| {
            let s_inv = geom::inverse(&s.to_mat3(centre)).unwrap_or(geom::IDENTITY);
            geom::mul(&a.to_mat3(centre), &s_inv)
        })
        .collect()
}

/// The smallest zoom `z >= 1` such that, for every affine warp, the output frame's corners scaled
/// by `1 / z` about the centre and then warped land inside the `w x h` source. Exact for affine
/// warps: the warped output rectangle is convex, so its corners decide. `f64::INFINITY` if some
/// warp moves the frame centre outside the source (no zoom helps).
pub fn min_zoom(warps: &[Mat3], width: f64, height: f64) -> f64 {
    let c = [width / 2.0, height / 2.0];
    let corners = [[0.0, 0.0], [width, 0.0], [0.0, height], [width, height]];
    let mut z: f64 = 1.0;
    for m in warps {
        let p0 = geom::apply(m, c);
        if !(0.0..=width).contains(&p0[0]) || !(0.0..=height).contains(&p0[1]) {
            return f64::INFINITY;
        }
        // Corner k at zoom z lands at p0 + L (corner - c) / z, L = linear part; each bound gives
        // an upper limit on u = 1 / z.
        let mut u_max: f64 = 1.0;
        for k in corners {
            let d = [k[0] - c[0], k[1] - c[1]];
            let v = [
                m[0][0] * d[0] + m[0][1] * d[1],
                m[1][0] * d[0] + m[1][1] * d[1],
            ];
            for (p, v, hi) in [(p0[0], v[0], width), (p0[1], v[1], height)] {
                if v > 0.0 {
                    u_max = u_max.min((hi - p) / v);
                } else if v < 0.0 {
                    u_max = u_max.min(p / -v);
                }
            }
        }
        z = z.max(1.0 / u_max);
    }
    z
}

/// The zoom to apply, given what [`min_zoom`] found.
pub fn choose_zoom(zoom: Zoom, needed: f64) -> f64 {
    match zoom {
        Zoom::Auto { max } => max.map_or(needed, |m| needed.min(m)),
        Zoom::Fixed(z) => z,
    }
}

/// Folds a static zoom into each warp: `W_i * scale_about(centre, 1 / z)`.
pub fn apply_zoom(warps: &mut [Mat3], centre: [f64; 2], zoom: f64) {
    let s = geom::scale_about(centre, 1.0 / zoom);
    for m in warps {
        *m = geom::mul(m, &s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: [f64; 2] = [960.0, 540.0];

    fn motion(tx: f64, ty: f64, angle: f64, scale: f64) -> Motion {
        Motion {
            tx,
            ty,
            angle,
            scale,
            confidence: 1.0,
        }
    }

    #[test]
    fn path_composes_motions() {
        // Content moves +2 px in x per frame: frame k -> k-1 is -2 px.
        let m = vec![motion(-2.0, 0.0, 0.0, 1.0); 5];
        let p = camera_path(&m, C);
        assert_eq!(p.len(), 5);
        assert!((p[4].tx - 8.0).abs() < 1e-9 && p[4].ty.abs() < 1e-9);
        // Rotations add up about the centre.
        let m = vec![motion(0.0, 0.0, -0.01, 1.0); 4];
        let p = camera_path(&m, C);
        assert!((p[3].angle - 0.03).abs() < 1e-12 && p[3].tx.abs() < 1e-9);
    }

    #[test]
    fn gaussian_matches_vidstab_kernel() {
        // A constant path is unchanged, including at the truncated ends.
        let flat = vec![
            Pose {
                tx: 3.0,
                ty: -1.0,
                angle: 0.1,
                log_scale: 0.01
            };
            30
        ];
        for (a, b) in gaussian(&flat, 10).iter().zip(&flat) {
            assert!((a.tx - b.tx).abs() < 1e-12 && (a.angle - b.angle).abs() < 1e-12);
        }
        // An impulse spreads as the kernel exp(-(k - mu)^2 / (mu/2)^2) / sum.
        let mut imp = vec![Pose::default(); 41];
        imp[20].tx = 1.0;
        let out = gaussian(&imp, 4);
        let k: Vec<f64> = (0..9)
            .map(|i| (-(f64::from(i) - 4.0).powi(2) / 4.0).exp())
            .collect();
        let sum: f64 = k.iter().sum();
        for d in 0..=4 {
            assert!((out[20 + d].tx - k[4 + d] / sum).abs() < 1e-12);
        }
        assert!(out[25].tx.abs() < 1e-15);
        // A linear pan is kept away from the ends (symmetric kernel).
        let pan: Vec<Pose> = (0..60)
            .map(|i| Pose {
                tx: f64::from(i) * 2.0,
                ..Pose::default()
            })
            .collect();
        let out = gaussian(&pan, 20);
        assert!((out[30].tx - 60.0).abs() < 1e-9);
    }

    #[test]
    fn corrections_cancel_shake_and_zoom_hides_borders() {
        // Shake of +-4 px in x and +-0.2 deg on a static shot.
        let m: Vec<Motion> = (0..50)
            .map(|i| {
                let s = if i % 2 == 0 { 1.0 } else { -1.0 };
                motion(8.0 * s, 0.0, 0.0035 * 2.0 * s, 1.0)
            })
            .collect();
        let actual = camera_path(&m, C);
        let smooth = gaussian(&actual, 15);
        let w = corrections(&actual, &smooth, C);
        // The correction is actual - smoothed per component (vidstab), to second order.
        for (i, wi) in w.iter().enumerate() {
            let (tx, ty, a, _) = geom::decompose_similarity(wi, C);
            let d = actual[i].tx - smooth[i].tx;
            assert!(
                d.abs() > 3.0 && (tx - d).abs() < 0.1,
                "frame {i}: {tx} vs {d}"
            );
            assert!((ty - (actual[i].ty - smooth[i].ty)).abs() < 0.1);
            assert!((a - (actual[i].angle - smooth[i].angle)).abs() < 1e-6);
        }
        let z = min_zoom(&w, 1920.0, 1080.0);
        assert!(z > 1.0 && z < 1.03, "{z}");
        let mut zw = w.clone();
        apply_zoom(&mut zw, C, z);
        let tol = 1e-6;
        for m in &zw {
            for k in [[0.0, 0.0], [1920.0, 0.0], [0.0, 1080.0], [1920.0, 1080.0]] {
                let [x, y] = geom::apply(m, k);
                assert!((-tol..=1920.0 + tol).contains(&x) && (-tol..=1080.0 + tol).contains(&y));
            }
        }
        // Just below the minimum, some corner leaves the source.
        let mut zw = w;
        apply_zoom(&mut zw, C, z * 0.999);
        let out = zw.iter().any(|m| {
            [[0.0, 0.0], [1920.0, 0.0], [0.0, 1080.0], [1920.0, 1080.0]]
                .iter()
                .any(|&k| {
                    let [x, y] = geom::apply(m, k);
                    !(0.0..=1920.0).contains(&x) || !(0.0..=1080.0).contains(&y)
                })
        });
        assert!(out);
        assert!((choose_zoom(Zoom::Auto { max: Some(1.001) }, z) - 1.001).abs() < 1e-12);
        assert!((choose_zoom(Zoom::Fixed(1.1), z) - 1.1).abs() < 1e-12);
        assert!((min_zoom(&[geom::IDENTITY], 1920.0, 1080.0) - 1.0).abs() < 1e-12);
    }
}
