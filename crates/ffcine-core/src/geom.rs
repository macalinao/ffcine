//! Plane geometry for per-frame warps: 3x3 homogeneous matrices in pixel coordinates.
//!
//! Coordinates are continuous pixel coordinates: pixel `(i, j)` covers `[i, i + 1) x [j, j + 1)`,
//! so its centre is `(i + 0.5, j + 0.5)` and a `w x h` frame's centre is `(w / 2, h / 2)`, the GPU
//! texture convention. A warp [`Mat3`] maps an **output** pixel to the **source** pixel it samples,
//! both in source-frame coordinates, and applies before the grade pass's crop/scale.

// Matrix maths reads best with the usual one-letter names.
#![allow(clippy::many_single_char_names)]

/// Row-major homogeneous 3x3 matrix: `[x', y', w'] = m * [x, y, 1]`, point `(x' / w', y' / w')`.
pub type Mat3 = [[f64; 3]; 3];

pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub fn identity() -> Mat3 {
    IDENTITY
}

/// `a * b`: applies `b` first, then `a`.
pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

/// `None` if `m` is singular (or nearly).
pub fn inverse(m: &Mat3) -> Option<Mat3> {
    let c = |i: usize, j: usize| {
        let (r0, r1) = ((i + 1) % 3, (i + 2) % 3);
        let (c0, c1) = ((j + 1) % 3, (j + 2) % 3);
        m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0]
    };
    let det = m[0][0] * c(0, 0) + m[0][1] * c(0, 1) + m[0][2] * c(0, 2);
    let scale = m.iter().flatten().fold(0.0_f64, |a, v| a.max(v.abs()));
    if !det.is_finite() || det.abs() <= 1e-12 * scale.powi(3) {
        return None;
    }
    // Inverse = adjugate / det; the adjugate is the transposed cofactor matrix.
    let mut inv = [[0.0; 3]; 3];
    for (i, row) in inv.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = c(j, i) / det;
        }
    }
    Some(inv)
}

/// Maps a point (with the perspective divide).
pub fn apply(m: &Mat3, [x, y]: [f64; 2]) -> [f64; 2] {
    let w = m[2][0] * x + m[2][1] * y + m[2][2];
    [
        (m[0][0] * x + m[0][1] * y + m[0][2]) / w,
        (m[1][0] * x + m[1][1] * y + m[1][2]) / w,
    ]
}

pub fn translation(tx: f64, ty: f64) -> Mat3 {
    [[1.0, 0.0, tx], [0.0, 1.0, ty], [0.0, 0.0, 1.0]]
}

/// Similarity about `centre`: `p' = centre + scale * R(angle) * (p - centre) + (tx, ty)`.
/// `angle` is in radians, positive from +x towards +y (clockwise on screen, y points down).
pub fn similarity(centre: [f64; 2], tx: f64, ty: f64, angle: f64, scale: f64) -> Mat3 {
    let (s, c) = angle.sin_cos();
    let (a, b) = (scale * c, scale * s);
    let [cx, cy] = centre;
    [
        [a, -b, cx + tx - a * cx + b * cy],
        [b, a, cy + ty - b * cx - a * cy],
        [0.0, 0.0, 1.0],
    ]
}

/// Uniform scale by `scale` about `centre`.
pub fn scale_about(centre: [f64; 2], scale: f64) -> Mat3 {
    similarity(centre, 0.0, 0.0, 0.0, scale)
}

/// Inverse of [`similarity`] for an affine `m`: `(tx, ty, angle, scale)` about `centre`, taking the
/// closest similarity (the mean of the linear part's rotation/scale components) if `m` isn't one.
pub fn decompose_similarity(m: &Mat3, centre: [f64; 2]) -> (f64, f64, f64, f64) {
    let a = f64::midpoint(m[0][0], m[1][1]);
    let b = 0.5 * (m[1][0] - m[0][1]);
    let [cx, cy] = centre;
    // Where the centre goes, minus the centre, is the translation about it.
    let [x, y] = apply(m, centre);
    (x - cx, y - cy, b.atan2(a), a.hypot(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &Mat3, b: &Mat3) -> bool {
        a.iter()
            .flatten()
            .zip(b.iter().flatten())
            .all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn inverse_roundtrip() {
        let m = mul(
            &similarity([960.0, 540.0], 12.5, -3.0, 0.02, 1.07),
            &[[1.0, 0.1, 3.0], [0.05, 0.9, -2.0], [1e-5, 2e-5, 1.0]],
        );
        let inv = inverse(&m).unwrap();
        assert!(close(&mul(&m, &inv), &IDENTITY));
        assert!(close(&mul(&inv, &m), &IDENTITY));
        assert!(inverse(&[[1.0, 2.0, 0.0], [2.0, 4.0, 0.0], [0.0, 0.0, 1.0]]).is_none());
    }

    #[test]
    fn similarity_about_centre() {
        let c = [1920.0, 1080.0];
        let m = similarity(c, 5.0, -7.0, 0.3, 1.2);
        // The centre only moves by the translation.
        let [x, y] = apply(&m, c);
        assert!((x - 1925.0).abs() < 1e-9 && (y - 1073.0).abs() < 1e-9);
        // +x maps towards +y for a positive angle.
        let [x, y] = apply(&m, [c[0] + 100.0, c[1]]);
        assert!((x - (1925.0 + 120.0 * 0.3_f64.cos())).abs() < 1e-9);
        assert!((y - (1073.0 + 120.0 * 0.3_f64.sin())).abs() < 1e-9);
        let (tx, ty, a, s) = decompose_similarity(&m, c);
        assert!((tx - 5.0).abs() < 1e-9 && (ty + 7.0).abs() < 1e-9);
        assert!((a - 0.3).abs() < 1e-12 && (s - 1.2).abs() < 1e-12);
        // Translation commutes with the centre choice.
        assert!(close(
            &mul(&translation(5.0, -7.0), &scale_about(c, 1.2)),
            &similarity(c, 5.0, -7.0, 0.0, 1.2)
        ));
    }
}
