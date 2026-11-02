//! Camera motion from small luma frames: a global shift per frame pair (coarse-to-fine SAD over a
//! three-level pyramid), refined per textured block, and a zoom fitted to the blocks' divergence.
//! Per shot the frame-pair motion is summed into pan, tilt and zoom, and its high-frequency part
//! is the handheld shake.

use serde::{Deserialize, Serialize};

/// Block size at the analysis level, pixels.
const BLOCK: usize = 16;
/// Block search radius around the global shift, pixels.
const BLOCK_R: i32 = 3;
/// Global search radius at the coarsest level, pixels.
const COARSE_R: i32 = 4;
/// Blocks flatter than this luma variance carry no motion.
const MIN_VARIANCE: f32 = 12.0;
/// A block's match cost must rise at least this much (mean abs luma) one pixel off, both ways.
const MIN_CURVATURE: f32 = 1.0;

/// A luma frame, row-major.
#[derive(Debug, Clone)]
pub struct Luma {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Luma {
    /// BT.709 luma of packed RGB24.
    pub fn from_rgb(rgb: &[u8], w: usize, h: usize) -> Self {
        let px = rgb
            .chunks_exact(3)
            .take(w * h)
            .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
            .collect();
        Self { w, h, px }
    }

    fn at(&self, x: usize, y: usize) -> f32 {
        self.px[y * self.w + x]
    }

    /// 2x2 box downsample.
    fn half(&self) -> Self {
        let (w, h) = ((self.w / 2).max(1), (self.h / 2).max(1));
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let (x0, y0) = ((2 * x).min(self.w - 1), (2 * y).min(self.h - 1));
                let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
                px.push(
                    (self.at(x0, y0) + self.at(x1, y0) + self.at(x0, y1) + self.at(x1, y1)) / 4.0,
                );
            }
        }
        Self { w, h, px }
    }

    /// Variance of the 4-neighbour Laplacian: higher is sharper.
    pub fn sharpness(&self) -> f32 {
        if self.w < 3 || self.h < 3 {
            return 0.0;
        }
        let (mut sum, mut sq, mut n) = (0f64, 0f64, 0f64);
        for y in 1..self.h - 1 {
            for x in 1..self.w - 1 {
                let l = f64::from(
                    4.0 * self.at(x, y)
                        - self.at(x - 1, y)
                        - self.at(x + 1, y)
                        - self.at(x, y - 1)
                        - self.at(x, y + 1),
                );
                sum += l;
                sq += l * l;
                n += 1.0;
            }
        }
        let mean = sum / n;
        #[allow(clippy::cast_possible_truncation)]
        let v = (sq / n - mean * mean) as f32;
        v
    }
}

/// The `w` x `h` window of `l` at `(x, y)`, row by row.
fn rows(l: &Luma, (x, y, w, h): (usize, usize, usize, usize)) -> impl Iterator<Item = &[f32]> {
    (0..h).map(move |j| &l.px[(y + j) * l.w + x..][..w])
}

/// Mean absolute difference, means removed, of `a` at `(x, y)` and `b` at `(x + dx, y + dy)`
/// over the `w` x `h` window; `None` if it falls outside `b`.
#[allow(clippy::too_many_arguments)]
fn sad(
    a: &Luma,
    b: &Luma,
    (x0, y0, w, h): (usize, usize, usize, usize),
    dx: i32,
    dy: i32,
) -> Option<f32> {
    let bx = i64::try_from(x0).ok()? + i64::from(dx);
    let by = i64::try_from(y0).ok()? + i64::from(dy);
    if bx < 0 || by < 0 {
        return None;
    }
    let (bx, by) = (usize::try_from(bx).ok()?, usize::try_from(by).ok()?);
    if bx + w > b.w || by + h > b.h || x0 + w > a.w || y0 + h > a.h {
        return None;
    }
    let rows = |l, x, y| rows(l, (x, y, w, h));
    #[allow(clippy::cast_precision_loss)]
    let n = (w * h) as f32;
    // Each side's mean removed, so a fade or exposure change isn't motion.
    let ma = rows(a, x0, y0).flatten().sum::<f32>() / n;
    let mb = rows(b, bx, by).flatten().sum::<f32>() / n;
    let d = mb - ma;
    let s: f32 = rows(a, x0, y0)
        .zip(rows(b, bx, by))
        .map(|(ra, rb)| {
            ra.iter()
                .zip(rb)
                .map(|(p, q)| (q - p - d).abs())
                .sum::<f32>()
        })
        .sum();
    Some(s / n)
}

/// Best whole-frame shift of `b` against `a` within `r` of `(cx, cy)`, comparing the central
/// region that stays inside both.
fn global(a: &Luma, b: &Luma, (cx, cy): (i32, i32), r: i32) -> (i32, i32) {
    let reach = usize::try_from(cx.abs().max(cy.abs()) + r).unwrap_or(0);
    let m = reach.min(a.w / 3).min(a.h / 3);
    let win = (m, m, a.w.saturating_sub(2 * m), a.h.saturating_sub(2 * m));
    let mut best = (f32::INFINITY, (cx, cy));
    for dy in cy - r..=cy + r {
        for dx in cx - r..=cx + r {
            if let Some(s) = sad(a, b, win, dx, dy)
                && s < best.0
            {
                best = (s, (dx, dy));
            }
        }
    }
    best.1
}

/// Sub-pixel offset of a minimum from its two neighbours' costs.
fn parabola(l: f32, c: f32, r: f32) -> f32 {
    let d = l - 2.0 * c + r;
    if d <= 1e-6 {
        0.0
    } else {
        (0.5 * (l - r) / d).clamp(-0.5, 0.5)
    }
}

/// The motion between two frames.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Step {
    /// Content shift, fractions of the frame width (+ right) and height (+ down).
    pub dx: f32,
    pub dy: f32,
    /// Content scale per frame minus 1 (+ is growing, a push in).
    pub zoom: f32,
}

/// A frame's pyramid, kept between frames.
#[derive(Debug, Clone)]
pub struct Pyramid([Luma; 3]);

impl Pyramid {
    pub fn new(l: Luma) -> Self {
        let l1 = l.half();
        let l2 = l1.half();
        Self([l, l1, l2])
    }

    pub fn base(&self) -> &Luma {
        &self.0[0]
    }
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

/// Motion from `prev` to `cur`.
#[allow(clippy::cast_precision_loss)]
pub fn estimate(prev: &Pyramid, cur: &Pyramid) -> Step {
    let [a0, a1, a2] = &prev.0;
    let [b0, b1, b2] = &cur.0;
    let g2 = global(a2, b2, (0, 0), COARSE_R);
    let g1 = global(a1, b1, (g2.0 * 2, g2.1 * 2), 1);
    let (gx, gy) = global(a0, b0, (g1.0 * 2, g1.1 * 2), 1);

    let (cx, cy) = (a0.w as f32 / 2.0, a0.h as f32 / 2.0);
    let mut vecs: Vec<(f32, f32, f32, f32)> = Vec::new();
    let pad = usize::try_from(gx.abs().max(gy.abs()) + BLOCK_R).unwrap_or(0);
    let mut y = pad;
    while y + BLOCK + pad <= a0.h {
        let mut x = pad;
        while x + BLOCK + pad <= a0.w {
            let block = (x, y, BLOCK, BLOCK);
            let mean = (0..BLOCK)
                .flat_map(|j| (0..BLOCK).map(move |i| (i, j)))
                .map(|(i, j)| a0.at(x + i, y + j))
                .sum::<f32>()
                / (BLOCK * BLOCK) as f32;
            let var = (0..BLOCK)
                .flat_map(|j| (0..BLOCK).map(move |i| (i, j)))
                .map(|(i, j)| (a0.at(x + i, y + j) - mean).powi(2))
                .sum::<f32>()
                / (BLOCK * BLOCK) as f32;
            if var >= MIN_VARIANCE {
                let mut best = (f32::INFINITY, (gx, gy));
                for dy in gy - BLOCK_R..=gy + BLOCK_R {
                    for dx in gx - BLOCK_R..=gx + BLOCK_R {
                        if let Some(s) = sad(a0, b0, block, dx, dy)
                            && s < best.0
                        {
                            best = (s, (dx, dy));
                        }
                    }
                }
                let (bx, by) = best.1;
                let cost = |dx, dy| sad(a0, b0, block, dx, dy).unwrap_or(best.0);
                let (xl, xr) = (cost(bx - 1, by), cost(bx + 1, by));
                let (yl, yr) = (cost(bx, by - 1), cost(bx, by + 1));
                // A block that only has edges one way (a bar, a horizon) can't tell motion
                // along them: it needs a clear minimum in both directions.
                if (xl + xr - 2.0 * best.0).min(yl + yr - 2.0 * best.0) < MIN_CURVATURE {
                    x += BLOCK;
                    continue;
                }
                let fx = parabola(xl, best.0, xr);
                let fy = parabola(yl, best.0, yr);
                let (px, py) = (
                    x as f32 + BLOCK as f32 / 2.0 - cx,
                    y as f32 + BLOCK as f32 / 2.0 - cy,
                );
                vecs.push((px, py, bx as f32 + fx, by as f32 + fy));
            }
            x += BLOCK;
        }
        y += BLOCK;
    }
    let (tx, ty) = if vecs.len() >= 3 {
        let mut xs: Vec<f32> = vecs.iter().map(|v| v.2).collect();
        let mut ys: Vec<f32> = vecs.iter().map(|v| v.3).collect();
        (median(&mut xs), median(&mut ys))
    } else {
        // A flat picture (a title card, black) shows no motion either way.
        return Step::default();
    };
    // Divergence: each block's outward motion over its distance from the centre.
    let min_r = a0.w.min(a0.h) as f32 / 6.0;
    let mut zs: Vec<f32> = vecs
        .iter()
        .filter(|v| v.0.hypot(v.1) >= min_r)
        .map(|v| ((v.2 - tx) * v.0 + (v.3 - ty) * v.1) / (v.0 * v.0 + v.1 * v.1))
        .collect();
    let zoom = if zs.len() >= 4 { median(&mut zs) } else { 0.0 };
    Step {
        dx: tx / a0.w as f32,
        dy: ty / a0.h as f32,
        zoom,
    }
}

/// What the camera does over a shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MotionKind {
    Static,
    Pan,
    Tilt,
    Push,
    Pull,
    Handheld,
}

impl MotionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Pan => "pan",
            Self::Tilt => "tilt",
            Self::Push => "push",
            Self::Pull => "pull",
            Self::Handheld => "handheld",
        }
    }
}

/// A shot's camera motion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Motion {
    pub kind: MotionKind,
    /// `left`/`right` for a pan, `up`/`down` for a tilt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// Shake over the movement (handheld, walking).
    pub handheld: bool,
    /// Net camera pan, frame widths (+ right).
    pub pan: f32,
    /// Net camera tilt, frame heights (+ up).
    pub tilt: f32,
    /// Picture scale at the end over the start (> 1 pushes in).
    pub zoom: f32,
    /// RMS shake, % of the frame width.
    pub shake: f32,
    /// Mean movement, frame widths per second.
    pub speed: f32,
}

impl Motion {
    /// `pan right`, `push (handheld)`, `static`.
    pub fn label(&self) -> String {
        let mut s = self.kind.as_str().to_owned();
        if let Some(d) = &self.direction {
            s.push(' ');
            s.push_str(d);
        }
        if self.handheld && self.kind != MotionKind::Handheld {
            s.push_str(" (handheld)");
        }
        s
    }
}

#[derive(Debug, Clone)]
pub struct MotionParams {
    /// Net pan/tilt (frame fraction) or 2 x ln(zoom) that makes a move.
    pub move_min: f32,
    /// RMS shake (% of width) that makes a shot handheld.
    pub shake_min: f32,
}

impl Default for MotionParams {
    fn default() -> Self {
        Self {
            move_min: 0.08,
            shake_min: 0.25,
        }
    }
}

/// Sums a shot's frame-pair steps (at `fps`) into its [`Motion`].
#[allow(clippy::cast_precision_loss)]
pub fn classify(steps: &[Step], fps: f64, p: &MotionParams) -> Motion {
    // Camera moves opposite to the content.
    let pan = -steps.iter().map(|s| s.dx).sum::<f32>();
    let tilt = steps.iter().map(|s| s.dy).sum::<f32>();
    let ln_zoom: f32 = steps.iter().map(|s| (1.0 + s.zoom).max(1e-3).ln()).sum();
    // Shake: the content path minus its moving average (~0.4 s), RMS.
    let mut path = Vec::with_capacity(steps.len() + 1);
    let (mut x, mut y) = (0f32, 0f32);
    path.push((x, y));
    for s in steps {
        x += s.dx;
        y += s.dy;
        path.push((x, y));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let half = ((fps * 0.2).round() as usize).max(2);
    let mut sq = 0f32;
    for i in 0..path.len() {
        // Symmetric window, narrower at the ends, so a steady move has no shake.
        let r = half.min(i).min(path.len() - 1 - i);
        let (lo, hi) = (i - r, i + r);
        let n = (hi - lo + 1) as f32;
        let (mx, my) = path[lo..=hi]
            .iter()
            .fold((0.0, 0.0), |a, b| (a.0 + b.0 / n, a.1 + b.1 / n));
        // Heights in widths, assuming a 16:9-ish frame either way is fine for a threshold.
        sq += (path[i].0 - mx).powi(2) + (path[i].1 - my).powi(2);
    }
    let shake = 100.0 * (sq / path.len().max(1) as f32).sqrt();
    let dur = (steps.len() as f64 / fps.max(1.0)) as f32;
    let travel: f32 = steps.iter().map(|s| s.dx.hypot(s.dy)).sum();
    let speed = if dur > 0.0 { travel / dur } else { 0.0 };
    let handheld = shake >= p.shake_min;

    let scores = [
        (MotionKind::Pan, pan.abs()),
        (MotionKind::Tilt, tilt.abs()),
        (
            if ln_zoom >= 0.0 {
                MotionKind::Push
            } else {
                MotionKind::Pull
            },
            2.0 * ln_zoom.abs(),
        ),
    ];
    let (kind, best) =
        scores.iter().copied().fold(
            (MotionKind::Static, 0.0),
            |a, b| if b.1 > a.1 { b } else { a },
        );
    let (kind, direction) = if best >= p.move_min {
        let dir = match kind {
            MotionKind::Pan => Some(if pan > 0.0 { "right" } else { "left" }),
            MotionKind::Tilt => Some(if tilt > 0.0 { "up" } else { "down" }),
            _ => None,
        };
        (kind, dir.map(str::to_owned))
    } else if handheld {
        (MotionKind::Handheld, None)
    } else {
        (MotionKind::Static, None)
    };
    let r3 = |v: f32| (v * 1000.0).round() / 1000.0;
    Motion {
        kind,
        direction,
        handheld,
        pan: r3(pan),
        tilt: r3(tilt),
        zoom: r3(ln_zoom.exp()),
        shake: r3(shake),
        speed: r3(speed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A textured, non-repeating test picture sampled at a continuous position.
    fn texture(x: f32, y: f32) -> f32 {
        let v = (x * 0.31).sin() * (y * 0.23).cos() * 60.0
            + (x * 0.071 + y * 0.113).sin() * 50.0
            + ((x * 0.017).sin() + (y * 0.029).cos()) * 30.0;
        128.0 + v
    }

    fn frame(w: usize, h: usize, f: impl Fn(f32, f32) -> f32) -> Pyramid {
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                #[allow(clippy::cast_precision_loss)]
                px.push(f(x as f32, y as f32));
            }
        }
        Pyramid::new(Luma { w, h, px })
    }

    #[test]
    fn finds_a_shift() {
        let a = frame(160, 90, texture);
        let b = frame(160, 90, |x, y| texture(x - 5.0, y + 2.0));
        let s = estimate(&a, &b);
        assert!((s.dx * 160.0 - 5.0).abs() < 0.3, "{s:?}");
        assert!((s.dy * 90.0 + 2.0).abs() < 0.3, "{s:?}");
        assert!(s.zoom.abs() < 0.01, "{s:?}");
    }

    #[test]
    fn finds_a_large_shift() {
        let a = frame(160, 90, texture);
        let b = frame(160, 90, |x, y| texture(x + 14.0, y));
        let s = estimate(&a, &b);
        assert!((s.dx * 160.0 + 14.0).abs() < 0.5, "{s:?}");
    }

    #[test]
    fn finds_a_zoom() {
        let a = frame(160, 90, texture);
        let b = frame(160, 90, |x, y| {
            texture(80.0 + (x - 80.0) / 1.03, 45.0 + (y - 45.0) / 1.03)
        });
        let s = estimate(&a, &b);
        assert!((s.zoom - 0.03).abs() < 0.01, "{s:?}");
        assert!(s.dx.abs() < 0.01, "{s:?}");
    }

    #[test]
    fn classifies() {
        let p = MotionParams::default();
        let still = vec![Step::default(); 60];
        assert_eq!(classify(&still, 30.0, &p).kind, MotionKind::Static);
        let pan = vec![
            Step {
                dx: -0.01,
                ..Step::default()
            };
            60
        ];
        let m = classify(&pan, 30.0, &p);
        assert_eq!(
            (m.kind, m.direction.as_deref()),
            (MotionKind::Pan, Some("right"))
        );
        assert!(!m.handheld);
        let tilt = vec![
            Step {
                dy: 0.01,
                ..Step::default()
            };
            60
        ];
        assert_eq!(classify(&tilt, 30.0, &p).label(), "tilt up");
        let push = vec![
            Step {
                zoom: 0.003,
                ..Step::default()
            };
            60
        ];
        assert_eq!(classify(&push, 30.0, &p).kind, MotionKind::Push);
        let shake: Vec<Step> = (0..60)
            .map(|i| Step {
                dx: if i % 4 < 2 { 0.008 } else { -0.008 },
                dy: if i % 6 < 3 { 0.006 } else { -0.006 },
                zoom: 0.0,
            })
            .collect();
        let m = classify(&shake, 30.0, &p);
        assert_eq!(m.kind, MotionKind::Handheld, "{m:?}");
    }

    #[test]
    fn sharper_is_higher() {
        let sharp = frame(64, 36, |x, _| if (x as i32) % 2 == 0 { 0.0 } else { 255.0 });
        let flat = frame(64, 36, |_, _| 128.0);
        assert!(sharp.base().sharpness() > flat.base().sharpness());
    }
}
