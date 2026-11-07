//! Per-frame colour samples and their per-shot average.

use serde::{Deserialize, Serialize};

const BINS: usize = 64;

/// One frame's colour, from the analysis-size RGB.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Mean R, G, B, 0..1.
    pub rgb: [f32; 3],
    /// Mean HSV saturation, 0..1.
    pub saturation: f32,
    /// Luma histogram.
    pub hist: [u32; BINS],
}

impl Sample {
    /// Packed RGB24.
    #[allow(clippy::cast_precision_loss)]
    pub fn from_rgb(rgb: &[u8]) -> Self {
        let mut sum = [0f64; 3];
        let mut sat = 0f64;
        let mut hist = [0u32; BINS];
        let mut n = 0usize;
        for p in rgb.as_chunks::<3>().0 {
            let (r, g, b) = (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]));
            sum[0] += f64::from(r);
            sum[1] += f64::from(g);
            sum[2] += f64::from(b);
            let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
            if mx > 0.0 {
                sat += f64::from((mx - mn) / mx);
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let y = (0.2126 * r + 0.7152 * g + 0.0722 * b).clamp(0.0, 255.0) as usize;
            hist[y * BINS / 256] += 1;
            n += 1;
        }
        let n = n.max(1) as f64;
        #[allow(clippy::cast_possible_truncation)]
        Self {
            rgb: sum.map(|s| (s / n / 255.0) as f32),
            saturation: (sat / n) as f32,
            hist,
        }
    }
}

/// A shot's average colour and tonal range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Colour {
    /// Mean colour, `#rrggbb`.
    pub mean: String,
    /// Mean luma, 0..1.
    pub luma: f32,
    /// Luma percentiles 5, 50 and 95, 0..1.
    pub p5: f32,
    pub p50: f32,
    pub p95: f32,
    /// Mean saturation, 0..1.
    pub saturation: f32,
    /// Mean R minus mean B (+ warm, - cool), -1..1.
    pub warmth: f32,
}

impl Colour {
    /// `bright warm`, `dark cool muted`...: for the shot list.
    pub fn words(&self) -> String {
        let mut w = Vec::new();
        if self.luma < 0.25 {
            w.push("dark");
        } else if self.luma > 0.6 {
            w.push("bright");
        }
        if self.warmth > 0.06 {
            w.push("warm");
        } else if self.warmth < -0.06 {
            w.push("cool");
        }
        if self.saturation < 0.15 {
            w.push("muted");
        } else if self.saturation > 0.45 {
            w.push("saturated");
        }
        if self.p95 - self.p5 > 0.75 {
            w.push("contrasty");
        } else if self.p95 - self.p5 < 0.35 {
            w.push("flat");
        }
        w.join(" ")
    }
}

fn r3(v: f32) -> f32 {
    (v * 1000.0).round() / 1000.0
}

/// The average of `samples` (empty gives mid grey).
#[allow(clippy::cast_precision_loss)]
pub fn average(samples: &[Sample]) -> Colour {
    let n = samples.len().max(1) as f32;
    let mut rgb = [0f32; 3];
    let mut sat = 0f32;
    let mut hist = [0u64; BINS];
    for s in samples {
        for (m, v) in rgb.iter_mut().zip(s.rgb) {
            *m += v / n;
        }
        sat += s.saturation / n;
        for (h, v) in hist.iter_mut().zip(s.hist) {
            *h += u64::from(v);
        }
    }
    if samples.is_empty() {
        rgb = [0.5; 3];
    }
    let total: u64 = hist.iter().sum();
    let pct = |q: f64| -> f32 {
        if total == 0 {
            return 0.5;
        }
        let target = (q * total as f64).ceil().max(1.0);
        let mut acc = 0u64;
        for (i, &h) in hist.iter().enumerate() {
            acc += h;
            if acc as f64 >= target {
                return (i as f32 + 0.5) / BINS as f32;
            }
        }
        1.0
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hex = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    Colour {
        mean: format!("#{:02x}{:02x}{:02x}", hex(rgb[0]), hex(rgb[1]), hex(rgb[2])),
        luma: r3(0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]),
        p5: r3(pct(0.05)),
        p50: r3(pct(0.5)),
        p95: r3(pct(0.95)),
        saturation: r3(sat),
        warmth: r3(rgb[0] - rgb[2]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_a_warm_frame() {
        let px: Vec<u8> = [230u8, 140, 60].repeat(100);
        let c = average(&[Sample::from_rgb(&px)]);
        assert_eq!(c.mean, "#e68c3c");
        assert!(c.warmth > 0.5);
        assert!(c.saturation > 0.7);
        assert!(c.words().contains("warm"));
        assert!((c.p5 - c.p95).abs() < 0.02);
    }

    #[test]
    fn percentiles_span_a_ramp() {
        let px: Vec<u8> = (0..=255u8).flat_map(|v| [v, v, v]).collect();
        let c = average(&[Sample::from_rgb(&px)]);
        assert!(c.p5 < 0.1 && c.p95 > 0.9, "{c:?}");
        assert!((c.p50 - 0.5).abs() < 0.03, "{c:?}");
        assert!(c.saturation < 0.01);
    }
}
