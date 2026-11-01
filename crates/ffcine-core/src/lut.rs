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
