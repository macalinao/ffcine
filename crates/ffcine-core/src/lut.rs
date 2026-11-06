//! 3D LUTs (`.cube`), parsed on the CPU and uploaded by the backend.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::color::named_enum;
use crate::{Error, Result};

named_enum!(
    /// What colour space a LUT's input/output live in (libplacebo's `lut_type`).
    LutType {
        /// Applied to the source's native (encoded) values, before any conversion.
        Native = "native",
        /// Applied to normalized, decoded RGB (the prototype's baked grade).
        Normalized = "normalized",
        /// A full colour space conversion; replaces libplacebo's own.
        Conversion = "conversion",
    }
);

/// A 3D LUT: `size`^3 RGB entries, red varying fastest (`.cube` order).
#[derive(Clone, PartialEq)]
pub struct Lut {
    pub title: Option<String>,
    pub size: usize,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
    /// `size`^3 entries, index `r + g * size + b * size * size`.
    pub data: Vec<[f32; 3]>,
    /// Where it was loaded from, if anywhere.
    pub path: Option<PathBuf>,
}

impl fmt::Debug for Lut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lut")
            .field("title", &self.title)
            .field("size", &self.size)
            .field("domain_min", &self.domain_min)
            .field("domain_max", &self.domain_max)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Lut {
    /// Reads an Adobe/Resolve `.cube` 3D LUT.
    pub fn from_cube_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        let mut lut = Self::parse_cube(&text).map_err(|(line, msg)| Error::CubeParse {
            path: path.to_owned(),
            line,
            msg,
        })?;
        lut.path = Some(path.to_owned());
        Ok(lut)
    }

    /// This LUT mixed with identity by `strength` (0..1, `lut_strength`): each lattice entry
    /// moved from its identity value (in the LUT's domain) towards its own by `strength`.
    #[must_use]
    pub fn mixed(mut self, strength: f32) -> Self {
        let s = strength.clamp(0.0, 1.0);
        if s >= 1.0 {
            return self;
        }
        let id = Self::identity(self.size);
        let span: Vec<f32> = (0..3)
            .map(|c| self.domain_max[c] - self.domain_min[c])
            .collect();
        for (v, i) in self.data.iter_mut().zip(&id.data) {
            for c in 0..3 {
                let ident = self.domain_min[c] + i[c] * span[c];
                v[c] = ident + s * (v[c] - ident);
            }
        }
        self
    }

    /// The identity LUT of the given size.
    pub fn identity(size: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let v = |i: usize| i as f32 / (size - 1) as f32;
        let mut data = Vec::with_capacity(size.pow(3));
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    data.push([v(r), v(g), v(b)]);
                }
            }
        }
        Self {
            title: None,
            size,
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            data,
            path: None,
        }
    }

    /// Nearest-entry lookup at lattice coordinates (for tests and debugging; the GPU interpolates).
    pub fn at(&self, r: usize, g: usize, b: usize) -> [f32; 3] {
        self.data[r + g * self.size + b * self.size * self.size]
    }

    /// Trilinear lookup of `rgb` (in the LUT's domain, clamped to it): how libplacebo samples a
    /// `pl_custom_lut` (a linearly filtered 3D texture), so baked LUTs agree with the GPU.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::many_single_char_names
    )]
    pub fn sample(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size - 1;
        let (mut i0, mut t) = ([0usize; 3], [0f32; 3]);
        for c in 0..3 {
            let span = self.domain_max[c] - self.domain_min[c];
            let u = ((rgb[c] - self.domain_min[c]) / span).clamp(0.0, 1.0) * n as f32;
            let i = (u.floor() as usize).min(n.saturating_sub(1));
            i0[c] = i;
            t[c] = u - i as f32;
        }
        let mut out = [0f32; 3];
        for corner in 0..8 {
            let d = [corner & 1, (corner >> 1) & 1, (corner >> 2) & 1];
            let w: f32 = (0..3)
                .map(|c| if d[c] == 1 { t[c] } else { 1.0 - t[c] })
                .product();
            if w == 0.0 {
                continue;
            }
            let v = self.at(i0[0] + d[0], i0[1] + d[1], i0[2] + d[2]);
            for c in 0..3 {
                out[c] += w * v[c];
            }
        }
        out
    }

    /// This LUT after a per-channel `gain` on its input, clamped to 0..1 (ffmpeg's
    /// `colorchannelmixer rr:gg:bb` ahead of `lut3d`), resampled on the same lattice.
    #[must_use]
    pub fn with_input_gain(&self, gain: [f32; 3]) -> Self {
        let id = Self::identity(self.size);
        let data = id
            .data
            .iter()
            .map(|p| {
                let mut x = [0f32; 3];
                for c in 0..3 {
                    let v = self.domain_min[c] + p[c] * (self.domain_max[c] - self.domain_min[c]);
                    x[c] = (v * gain[c]).clamp(0.0, 1.0);
                }
                self.sample(x)
            })
            .collect();
        Self {
            data,
            path: None,
            ..self.clone()
        }
    }

    /// The LUT as `.cube` text (`TITLE`, `LUT_3D_SIZE`, the domain if not 0..1, then entries).
    #[allow(clippy::float_cmp)] // exact defaults
    pub fn to_cube(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        if let Some(t) = &self.title {
            let _ = writeln!(s, "TITLE \"{t}\"");
        }
        let _ = writeln!(s, "LUT_3D_SIZE {}", self.size);
        if self.domain_min != [0.0; 3] || self.domain_max != [1.0; 3] {
            let [a, b, c] = self.domain_min;
            let _ = writeln!(s, "DOMAIN_MIN {a} {b} {c}");
            let [a, b, c] = self.domain_max;
            let _ = writeln!(s, "DOMAIN_MAX {a} {b} {c}");
        }
        for [r, g, b] in &self.data {
            let _ = writeln!(s, "{r:.6} {g:.6} {b:.6}");
        }
        s
    }

    /// Writes the LUT as a `.cube` file.
    pub fn write_cube(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_cube()).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })
    }

    fn parse_cube(text: &str) -> Result<Self, (usize, String)> {
        let mut title = None;
        let mut size = None;
        let mut domain_min = [0.0; 3];
        let mut domain_max = [1.0; 3];
        let mut data = Vec::new();
        let mut last_line = 0;

        for (i, raw) in text.lines().enumerate() {
            let line_no = i + 1;
            last_line = line_no;
            let line = raw.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            let (key, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();
            match key {
                "TITLE" => title = Some(rest.trim_matches('"').to_owned()),
                "LUT_3D_SIZE" => {
                    let n: usize = rest
                        .parse()
                        .map_err(|_| (line_no, format!("bad LUT_3D_SIZE {rest:?}")))?;
                    if !(2..=256).contains(&n) {
                        return Err((line_no, format!("LUT_3D_SIZE {n} out of range")));
                    }
                    size = Some(n);
                    data.reserve(n.pow(3));
                }
                "LUT_1D_SIZE" => return Err((line_no, "1D LUTs are not supported".into())),
                "DOMAIN_MIN" => domain_min = triple(rest).map_err(|m| (line_no, m))?,
                "DOMAIN_MAX" => domain_max = triple(rest).map_err(|m| (line_no, m))?,
                // Resolve/other vendor keywords we don't use.
                k if k.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {}
                _ => data.push(triple(line).map_err(|m| (line_no, m))?),
            }
        }

        let size = size.ok_or((last_line, "missing LUT_3D_SIZE".to_owned()))?;
        if data.len() != size.pow(3) {
            return Err((
                last_line,
                format!("expected {} entries, found {}", size.pow(3), data.len()),
            ));
        }
        Ok(Self {
            title,
            size,
            domain_min,
            domain_max,
            data,
            path: None,
        })
    }
}

fn triple(s: &str) -> Result<[f32; 3], String> {
    let mut it = s.split_whitespace().map(f32::from_str);
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some(Ok(a)), Some(Ok(b)), Some(Ok(c)), None) => Ok([a, b, c]),
        _ => Err(format!("expected three numbers, got {s:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY: &str = "# comment\nTITLE \"tiny\"\nLUT_3D_SIZE 2\nDOMAIN_MIN 0.0 0.0 0.0\n\
        DOMAIN_MAX 1.0 1.0 1.0\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";

    #[test]
    fn parses_cube() {
        let lut = Lut::parse_cube(TINY).unwrap();
        assert_eq!(lut.title.as_deref(), Some("tiny"));
        assert_eq!(lut.size, 2);
        assert_eq!(
            lut,
            Lut {
                title: Some("tiny".into()),
                ..Lut::identity(2)
            }
        );
        assert_eq!(lut.at(1, 0, 1), [1.0, 0.0, 1.0]);
    }

    #[test]
    fn input_gain() {
        // On the identity a gain is the gain, clamped at 1 (exact at lattice points).
        let g = Lut::identity(17).with_input_gain([2.0, 1.0, 0.5]);
        assert_eq!(g.at(4, 4, 4), [0.5, 0.25, 0.125]);
        assert_eq!(g.at(12, 0, 0), [1.0, 0.0, 0.0]);
        assert_eq!(g.at(16, 16, 16), [1.0, 1.0, 0.5]);
        // Through a LUT: the LUT read at the gained input.
        let mut l = Lut::identity(9);
        for v in &mut l.data {
            *v = [v[0] * v[0], v[1], 1.0 - v[2]];
        }
        let lg = l.with_input_gain([0.5, 1.0, 1.0]);
        let s = lg.at(8, 2, 8);
        assert!((s[0] - l.sample([0.5, 0.0, 0.0])[0]).abs() < 1e-6, "{s:?}");
        assert!((s[0] - 0.25).abs() < 1e-6);
        assert!((s[1] - 0.25).abs() < 1e-6);
        assert!(s[2].abs() < 1e-6);
        // Unit gain is the LUT itself.
        assert_eq!(l.with_input_gain([1.0; 3]).data, l.data);
    }

    #[test]
    fn samples_trilinear() {
        let id = Lut::identity(5);
        for v in [[0.0, 0.5, 1.0], [0.13, 0.77, 0.4], [1.2, -0.1, 0.99]] {
            let s = id.sample(v);
            for c in 0..3 {
                assert!((s[c] - v[c].clamp(0.0, 1.0)).abs() < 1e-6, "{v:?} -> {s:?}");
            }
        }
        // Halfway between two entries is their mean.
        let mut l = Lut::identity(2);
        l.data[1] = [0.5, 0.0, 0.0];
        assert!((l.sample([0.5, 0.0, 0.0])[0] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn cube_round_trip() {
        let mut l = Lut::identity(3);
        l.title = Some("t".into());
        l.data[4] = [0.25, 0.5, 0.125];
        assert_eq!(Lut::parse_cube(&l.to_cube()).unwrap(), l);
    }

    #[test]
    fn rejects_short_cube() {
        let err = Lut::parse_cube("LUT_3D_SIZE 2\n0 0 0\n").unwrap_err();
        assert!(err.1.contains("expected 8 entries"), "{err:?}");
    }

    #[test]
    fn rejects_garbage_row() {
        let err = Lut::parse_cube("LUT_3D_SIZE 2\n0 0\n").unwrap_err();
        assert_eq!(err.0, 2);
    }
}
