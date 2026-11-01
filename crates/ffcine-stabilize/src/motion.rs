//! Global motion between two frames from a block flow field: a robust similarity fit (RANSAC on
//! two-point samples, then iteratively reweighted least squares).

use ffcine_core::geom::{self, Mat3};

/// Frame-to-frame camera motion: the similarity taking pixel positions in frame `k` to where the
/// same content was in frame `k - 1`, about the frame centre ([`geom::similarity`] parameters).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    pub tx: f64,
    pub ty: f64,
    /// Radians.
    pub angle: f64,
    pub scale: f64,
    /// Fraction of usable flow vectors that agreed with the fit (0 when the fit failed and this
    /// is the identity).
    pub confidence: f64,
}

impl Motion {
    pub const IDENTITY: Self = Self {
        tx: 0.0,
        ty: 0.0,
        angle: 0.0,
        scale: 1.0,
        confidence: 0.0,
    };

    pub fn to_mat3(&self, centre: [f64; 2]) -> Mat3 {
        geom::similarity(centre, self.tx, self.ty, self.angle, self.scale)
    }

    /// The same motion in a frame `kx` x `ky` times larger (analysis -> source pixels).
    #[must_use]
    pub fn rescaled(self, kx: f64, ky: f64) -> Self {
        Self {
            tx: self.tx * kx,
            ty: self.ty * ky,
            ..self
        }
    }
}

/// A block flow field between two frames: per `grid x grid` block (row-major, `cols x rows`),
/// the vector `f` from the block's centre `p` in frame `k` to `p + f` in frame `k - 1`, in pixels,
/// and the engine's match cost (lower is better).
#[derive(Debug, Clone, Copy)]
pub struct FlowField<'a> {
    pub width: u32,
    pub height: u32,
    pub grid: u32,
    pub cols: usize,
    pub rows: usize,
    pub flow: &'a [[f32; 2]],
    pub cost: &'a [u8],
}

/// Knobs for [`estimate`]; the defaults suit NVOF at grid 4 on a ~1080p analysis frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EstimateParams {
    /// Ignore blocks within this fraction of the frame size of each edge (borders have no
    /// counterpart after motion, and lens vignetting/black edges confuse matching).
    pub border: f64,
    /// Ignore blocks with an NVOF cost above this (flat or occluded areas).
    pub max_cost: u8,
    /// Use every `step`-th block in each direction.
    pub step: usize,
    /// Inlier distance in analysis pixels. Generous: depth parallax and lens distortion bend a
    /// handheld wide-angle shot's background flow away from a pure similarity by a pixel or two.
    pub threshold: f64,
    pub iterations: u32,
    /// Below this inlier fraction the fit is rejected and the motion is the identity.
    pub min_confidence: f64,
}

impl Default for EstimateParams {
    fn default() -> Self {
        Self {
            border: 0.04,
            max_cost: 48,
            step: 2,
            threshold: 2.0,
            iterations: 200,
            min_confidence: 0.1,
        }
    }
}

/// xorshift64*: deterministic sampling, so detect results (and the cache) are reproducible.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        ((self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as usize) % n
    }
}

/// `q = z * p + t` with `z = a + ib`, on centred coordinates.
#[derive(Debug, Clone, Copy)]
struct Sim {
    a: f64,
    b: f64,
    tx: f64,
    ty: f64,
}

impl Sim {
    fn residual2(&self, p: [f64; 2], q: [f64; 2]) -> f64 {
        let x = self.a * p[0] - self.b * p[1] + self.tx - q[0];
        let y = self.b * p[0] + self.a * p[1] + self.ty - q[1];
        x * x + y * y
    }

    /// Exact fit through two correspondences: `z = (q1 - q2) / (p1 - p2)`, `t = q1 - z p1`.
    fn from_pairs(p1: [f64; 2], q1: [f64; 2], p2: [f64; 2], q2: [f64; 2]) -> Option<Self> {
        let (dpx, dpy) = (p1[0] - p2[0], p1[1] - p2[1]);
        let (dqx, dqy) = (q1[0] - q2[0], q1[1] - q2[1]);
        let n = dpx * dpx + dpy * dpy;
        if n < 1e-6 {
            return None;
        }
        let a = (dqx * dpx + dqy * dpy) / n;
        let b = (dqy * dpx - dqx * dpy) / n;
        Some(Self {
            a,
            b,
            tx: q1[0] - (a * p1[0] - b * p1[1]),
            ty: q1[1] - (b * p1[0] + a * p1[1]),
        })
    }
}

/// `q = m * p + t`: the refinement model. A free affine absorbs rolling-shutter skew, which a
/// similarity would partly read as rotation.
#[derive(Debug, Clone, Copy)]
struct Affine {
    m: [[f64; 2]; 2],
    t: [f64; 2],
}

impl Affine {
    fn from_sim(s: &Sim) -> Self {
        Self {
            m: [[s.a, -s.b], [s.b, s.a]],
            t: [s.tx, s.ty],
        }
    }

    fn residual2(&self, p: [f64; 2], q: [f64; 2]) -> f64 {
        let x = self.m[0][0] * p[0] + self.m[0][1] * p[1] + self.t[0] - q[0];
        let y = self.m[1][0] * p[0] + self.m[1][1] * p[1] + self.t[1] - q[1];
        x * x + y * y
    }

    /// Weighted least squares: with centroids removed, `m = C_qp * C_pp^-1`.
    fn fit(pts: &[([f64; 2], [f64; 2])], w: &[f64]) -> Option<Self> {
        let sw: f64 = w.iter().sum();
        if sw <= 0.0 {
            return None;
        }
        let (mut pc, mut qc) = ([0.0; 2], [0.0; 2]);
        for ((p, q), &wi) in pts.iter().zip(w) {
            for i in 0..2 {
                pc[i] += wi * p[i] / sw;
                qc[i] += wi * q[i] / sw;
            }
        }
        let (mut cpp, mut cqp) = ([[0.0; 2]; 2], [[0.0; 2]; 2]);
        for ((p, q), &wi) in pts.iter().zip(w) {
            let d = [p[0] - pc[0], p[1] - pc[1]];
            let e = [q[0] - qc[0], q[1] - qc[1]];
            for i in 0..2 {
                for j in 0..2 {
                    cpp[i][j] += wi * d[i] * d[j];
                    cqp[i][j] += wi * e[i] * d[j];
                }
            }
        }
        let det = cpp[0][0] * cpp[1][1] - cpp[0][1] * cpp[1][0];
        if det.abs() <= 1e-9 * (cpp[0][0] * cpp[1][1]).max(1e-30) {
            return None;
        }
        let inv = [
            [cpp[1][1] / det, -cpp[0][1] / det],
            [-cpp[1][0] / det, cpp[0][0] / det],
        ];
        let mut m = [[0.0; 2]; 2];
        for i in 0..2 {
            for j in 0..2 {
                m[i][j] = cqp[i][0] * inv[0][j] + cqp[i][1] * inv[1][j];
            }
        }
        let t = [
            qc[0] - m[0][0] * pc[0] - m[0][1] * pc[1],
            qc[1] - m[1][0] * pc[0] - m[1][1] * pc[1],
        ];
        Some(Self { m, t })
    }
}

/// Robust global similarity from a flow field, in the field's pixels.
pub fn estimate(field: &FlowField<'_>, params: &EstimateParams) -> Motion {
    let (w, h) = (f64::from(field.width), f64::from(field.height));
    let centre = [w / 2.0, h / 2.0];
    let g = f64::from(field.grid);
    let (mx, my) = (params.border * w, params.border * h);
    let step = params.step.max(1);

    let mut pts = Vec::with_capacity(field.cols * field.rows / (step * step) + 1);
    for r in (0..field.rows).step_by(step) {
        for c in (0..field.cols).step_by(step) {
            let i = r * field.cols + c;
            if field.cost[i] > params.max_cost {
                continue;
            }
            let (x, y) = ((c as f64 + 0.5) * g, (r as f64 + 0.5) * g);
            if x < mx || x > w - mx || y < my || y > h - my {
                continue;
            }
            let [fx, fy] = field.flow[i];
            let p = [x - centre[0], y - centre[1]];
            pts.push((p, [p[0] + f64::from(fx), p[1] + f64::from(fy)]));
        }
    }
    if pts.len() < 16 {
        return Motion::IDENTITY;
    }

    // RANSAC on 2-point samples, scored on a fixed subset for speed. Pairs closer than a quarter
    // of the frame are skipped: they pin rotation and scale poorly.
    let thr2 = params.threshold * params.threshold;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let score_set: Vec<usize> = (0..pts.len().min(1500))
        .map(|_| rng.below(pts.len()))
        .collect();
    let min_d2 = (w.min(h) / 4.0).powi(2);
    let mut best: Option<(usize, Sim)> = None;
    for _ in 0..params.iterations {
        let (i, j) = (rng.below(pts.len()), rng.below(pts.len()));
        let (p1, q1) = pts[i];
        let (p2, q2) = pts[j];
        if (p1[0] - p2[0]).powi(2) + (p1[1] - p2[1]).powi(2) < min_d2 {
            continue;
        }
        let Some(s) = Sim::from_pairs(p1, q1, p2, q2) else {
            continue;
        };
        let n = score_set
            .iter()
            .filter(|&&k| s.residual2(pts[k].0, pts[k].1) < thr2)
            .count();
        if best.is_none_or(|(b, _)| n > b) {
            best = Some((n, s));
        }
    }
    let Some((_, sim)) = best else {
        return Motion::IDENTITY;
    };

    // IRLS with Cauchy-like weights inside a shrinking hard gate (moving subjects drop out).
    let mut fit = Affine::from_sim(&sim);
    let mut weights = vec![0.0; pts.len()];
    for gate in [4.0, 2.0, 1.0, 1.0, 1.0] {
        let g2 = thr2 * gate * gate;
        for (wi, (p, q)) in weights.iter_mut().zip(&pts) {
            let r2 = fit.residual2(*p, *q);
            *wi = if r2 < g2 {
                1.0 / (1.0 + r2 / thr2)
            } else {
                0.0
            };
        }
        match Affine::fit(&pts, &weights) {
            Some(f) => fit = f,
            None => return Motion::IDENTITY,
        }
    }
    let inliers = pts
        .iter()
        .filter(|(p, q)| fit.residual2(*p, *q) < thr2)
        .count();
    let confidence = inliers as f64 / pts.len() as f64;
    if confidence < params.min_confidence {
        return Motion::IDENTITY;
    }
    let m = fit.m;
    // Rotation and scale from the first column: horizontal pans skew rows (m01) under a rolling
    // shutter, and the x axis stays clean of that.
    Motion {
        tx: fit.t[0],
        ty: fit.t[1],
        angle: m[1][0].atan2(m[0][0]),
        scale: m[0][0].hypot(m[1][0]),
        confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A field generated by `truth` (frame k -> k-1), with deterministic noise, an outlier
    /// "subject" moving on its own, and high-cost blocks carrying garbage.
    /// `shear` adds rolling-shutter skew about the centre: x += shear * (y - h/2).
    fn field(truth: &Motion, shear: f64, w: u32, h: u32) -> (Vec<[f32; 2]>, Vec<u8>, usize, usize) {
        let grid = 4;
        let (cols, rows) = ((w / grid) as usize, (h / grid) as usize);
        let cy = f64::from(h) / 2.0;
        let skew = [[1.0, shear, -shear * cy], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let m = geom::mul(&skew, &truth.to_mat3([f64::from(w) / 2.0, cy]));
        let mut rng = Rng(42);
        let mut noise = || (rng.below(1000) as f64 / 1000.0 - 0.5) * 0.5;
        let (mut flow, mut cost) = (Vec::new(), Vec::new());
        for r in 0..rows {
            for c in 0..cols {
                let p = [(c as f64 + 0.5) * 4.0, (r as f64 + 0.5) * 4.0];
                let q = geom::apply(&m, p);
                let mut f = [q[0] - p[0] + noise(), q[1] - p[1] + noise()];
                let mut k = 10;
                // A subject in the middle third, moving 12 px right on its own.
                if (cols / 3..cols / 2).contains(&c) && (rows / 3..2 * rows / 3).contains(&r) {
                    f[0] += 12.0;
                }
                // Flat sky: garbage vectors, flagged by cost.
                if r < rows / 6 {
                    f = [noise() * 80.0, noise() * 80.0];
                    k = 200;
                }
                flow.push([f[0] as f32, f[1] as f32]);
                cost.push(k);
            }
        }
        (flow, cost, cols, rows)
    }

    #[test]
    fn recovers_similarity_with_outliers() {
        let truth = Motion {
            tx: 6.3,
            ty: -2.1,
            angle: 0.004,
            scale: 1.003,
            confidence: 1.0,
        };
        let (w, h) = (1920, 1080);
        let (flow, cost, cols, rows) = field(&truth, 0.0, w, h);
        let f = FlowField {
            width: w,
            height: h,
            grid: 4,
            cols,
            rows,
            flow: &flow,
            cost: &cost,
        };
        let m = estimate(&f, &EstimateParams::default());
        assert!((m.tx - truth.tx).abs() < 0.02, "{m:?}");
        assert!((m.ty - truth.ty).abs() < 0.02, "{m:?}");
        assert!((m.angle - truth.angle).abs() < 2e-5, "{m:?}");
        assert!((m.scale - truth.scale).abs() < 2e-5, "{m:?}");
        // The subject is about 1/18 of the usable blocks.
        assert!(m.confidence > 0.85 && m.confidence < 0.97, "{m:?}");
    }

    #[test]
    fn rolling_shutter_skew_is_not_rotation() {
        let truth = Motion {
            tx: -14.0,
            ty: 1.2,
            angle: 0.001,
            scale: 1.0,
            confidence: 1.0,
        };
        let (w, h) = (1920, 1080);
        let (flow, cost, cols, rows) = field(&truth, 0.004, w, h);
        let f = FlowField {
            width: w,
            height: h,
            grid: 4,
            cols,
            rows,
            flow: &flow,
            cost: &cost,
        };
        let m = estimate(&f, &EstimateParams::default());
        assert!((m.tx - truth.tx).abs() < 0.05, "{m:?}");
        assert!((m.ty - truth.ty).abs() < 0.05, "{m:?}");
        assert!((m.angle - truth.angle).abs() < 3e-5, "{m:?}");
    }

    #[test]
    fn rejects_garbage() {
        let flow = vec![[0.0_f32; 2]; 480 * 270];
        let cost = vec![255_u8; 480 * 270];
        let f = FlowField {
            width: 1920,
            height: 1080,
            grid: 4,
            cols: 480,
            rows: 270,
            flow: &flow,
            cost: &cost,
        };
        assert_eq!(estimate(&f, &EstimateParams::default()), Motion::IDENTITY);
    }
}
