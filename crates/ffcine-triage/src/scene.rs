//! Scene cuts and picture activity from small luma frames, online so a 4 h clip never sits in
//! memory.
//!
//! Each sample is a [`SMALL_W`] x [`SMALL_H`] luma frame. The cut score mixes the mean absolute
//! pixel difference with the L1 distance of 32-bin histograms; a cut needs a score over
//! `threshold` and well above the recent median, so a steady walking pan doesn't fire.

/// Analysis frame size.
pub const SMALL_W: usize = 64;
pub const SMALL_H: usize = 36;
const BINS: usize = 32;
const WINDOW: usize = 24;

#[derive(Debug, Clone)]
pub struct SceneParams {
    /// Minimum score (0..1) for a cut.
    pub threshold: f32,
    /// A cut's score must also be this many times the median of the last samples.
    pub ratio: f32,
    /// Minimum seconds between cuts.
    pub min_gap: f64,
}

impl Default for SceneParams {
    fn default() -> Self {
        Self {
            threshold: 0.30,
            ratio: 3.0,
            min_gap: 1.0,
        }
    }
}

#[derive(Debug)]
pub struct SceneDetector {
    params: SceneParams,
    prev: Option<(Vec<u8>, [f32; BINS])>,
    recent: std::collections::VecDeque<f32>,
    last_cut: f64,
    pub cuts: Vec<f64>,
    /// Per-second (activity sum, luma sum, samples).
    seconds: Vec<(f32, f32, u32)>,
}

impl SceneDetector {
    pub fn new(params: SceneParams) -> Self {
        Self {
            params,
            prev: None,
            recent: std::collections::VecDeque::with_capacity(WINDOW),
            last_cut: f64::NEG_INFINITY,
            cuts: Vec::new(),
            seconds: Vec::new(),
        }
    }

    /// Feeds the luma sample at `t` seconds; returns whether it starts a new scene.
    pub fn push(&mut self, t: f64, luma: &[u8]) -> bool {
        debug_assert_eq!(luma.len(), SMALL_W * SMALL_H);
        let hist = histogram(luma);
        #[allow(clippy::cast_precision_loss)]
        let mean = luma.iter().map(|&v| f32::from(v)).sum::<f32>() / luma.len() as f32 / 255.0;
        let mut cut = false;
        let mut diff = 0.0;
        if let Some((pl, ph)) = &self.prev {
            #[allow(clippy::cast_precision_loss)]
            let mad = pl
                .iter()
                .zip(luma)
                .map(|(&a, &b)| f32::from(a.abs_diff(b)))
                .sum::<f32>()
                / luma.len() as f32
                / 255.0;
            let hd = ph
                .iter()
                .zip(&hist)
                .map(|(a, b)| (a - b).abs())
                .sum::<f32>()
                / 2.0;
            // MAD of a hard cut is ~0.15-0.4; scale it to the histogram's 0..1.
            let score = 0.5 * (mad * 3.0).min(1.0) + 0.5 * hd;
            diff = mad;
            let median = median(&self.recent);
            if score > self.params.threshold
                && score > median * self.params.ratio
                && t - self.last_cut >= self.params.min_gap
            {
                cut = true;
                self.last_cut = t;
                self.cuts.push(t);
            }
            if self.recent.len() == WINDOW {
                self.recent.pop_front();
            }
            self.recent.push_back(score);
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let sec = t.max(0.0) as usize;
        if self.seconds.len() <= sec {
            self.seconds.resize(sec + 1, (0.0, 0.0, 0));
        }
        let s = &mut self.seconds[sec];
        // Activity: MAD scaled so ordinary handheld motion lands near 0.5.
        s.0 += (diff * 10.0).min(1.0);
        s.1 += mean;
        s.2 += 1;
        self.prev = Some((luma.to_vec(), hist));
        cut
    }

    /// (activity, mean luma) per second; seconds without samples repeat the previous value.
    pub fn per_second(&self) -> (Vec<f32>, Vec<f32>) {
        let mut act = Vec::with_capacity(self.seconds.len());
        let mut luma = Vec::with_capacity(self.seconds.len());
        let (mut a, mut l) = (0.0, 0.0);
        for &(sa, sl, n) in &self.seconds {
            if n > 0 {
                #[allow(clippy::cast_precision_loss)]
                let n = n as f32;
                (a, l) = (sa / n, sl / n);
            }
            act.push(round3(a));
            luma.push(round3(l));
        }
        (act, luma)
    }
}

fn round3(v: f32) -> f32 {
    (v * 1000.0).round() / 1000.0
}

fn histogram(luma: &[u8]) -> [f32; BINS] {
    let mut h = [0f32; BINS];
    for &v in luma {
        h[usize::from(v) * BINS / 256] += 1.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = luma.len() as f32;
    for v in &mut h {
        *v /= n;
    }
    h
}

fn median(v: &std::collections::VecDeque<f32>) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s: Vec<f32> = v.iter().copied().collect();
    s.sort_by(f32::total_cmp);
    s[s.len() / 2]
}

/// Box-downsamples a packed RGB24 frame to [`SMALL_W`] x [`SMALL_H`] BT.709 luma.
pub fn small_luma(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; SMALL_W * SMALL_H];
    for (oy, row) in out.chunks_mut(SMALL_W).enumerate() {
        let (y0, y1) = (
            oy * h / SMALL_H,
            ((oy + 1) * h / SMALL_H).max(oy * h / SMALL_H + 1),
        );
        for (ox, px) in row.iter_mut().enumerate() {
            let (x0, x1) = (
                ox * w / SMALL_W,
                ((ox + 1) * w / SMALL_W).max(ox * w / SMALL_W + 1),
            );
            let (mut sum, mut n) = (0u32, 0u32);
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    let i = (y * w + x) * 3;
                    let (r, g, b) = (
                        u32::from(rgb[i]),
                        u32::from(rgb[i + 1]),
                        u32::from(rgb[i + 2]),
                    );
                    sum += (r * 54 + g * 183 + b * 19) >> 8;
                    n += 1;
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            {
                *px = (sum / n.max(1)) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(f: impl Fn(usize, usize) -> u8) -> Vec<u8> {
        (0..SMALL_W * SMALL_H)
            .map(|i| f(i % SMALL_W, i / SMALL_W))
            .collect()
    }

    #[test]
    fn hard_cut_fires_once() {
        let mut d = SceneDetector::new(SceneParams::default());
        for i in 0..40 {
            let t = f64::from(i) * 0.25;
            // A slowly panning gradient, then a dark scene from t = 5.
            #[allow(clippy::cast_possible_truncation)]
            let f = if t < 5.0 {
                frame(|x, _| ((x + i as usize) * 3 % 200) as u8 + 40)
            } else {
                frame(|_, y| (y * 2) as u8)
            };
            d.push(t, &f);
        }
        assert_eq!(d.cuts, [5.0]);
        let (act, luma) = d.per_second();
        assert_eq!(act.len(), 10);
        assert!(luma[0] > luma[9]);
    }

    #[test]
    fn static_picture_has_no_activity() {
        let mut d = SceneDetector::new(SceneParams::default());
        let f = frame(|x, y| (x + y) as u8);
        for i in 0..20 {
            d.push(f64::from(i) * 0.5, &f);
        }
        assert_eq!(d.cuts, [] as [f64; 0]);
        assert!(d.per_second().0.iter().all(|&a| a == 0.0));
    }

    #[test]
    fn downsamples_rgb() {
        let rgb = vec![255u8; 128 * 72 * 3];
        assert!(small_luma(&rgb, 128, 72).iter().all(|&v| v == 255));
    }
}
