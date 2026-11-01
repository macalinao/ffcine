//! Contact strips: a clip's thumbnails side by side in one JPEG.

use std::path::Path;

use image::codecs::jpeg::JpegEncoder;

use crate::{Error, Result};

/// One thumbnail: packed RGB24, `w` x `h`.
pub struct Frame<'a> {
    pub w: u32,
    pub h: u32,
    pub rgb: &'a [u8],
}

/// Writes the frames left to right (all the height of the first) as a JPEG.
pub fn write_strip(path: &Path, frames: &[Frame<'_>]) -> Result<()> {
    let Some(first) = frames.first() else {
        return Err(Error::Invalid("empty strip".into()));
    };
    let h = first.h as usize;
    let widths: Vec<usize> = frames
        .iter()
        .filter(|f| f.h == first.h)
        .map(|f| f.w as usize)
        .collect();
    let total: usize = widths.iter().sum();
    let mut img = vec![0u8; total * h * 3];
    let mut x0 = 0;
    for f in frames.iter().filter(|f| f.h == first.h) {
        let w = f.w as usize;
        for y in 0..h {
            let src = &f.rgb[y * w * 3..(y + 1) * w * 3];
            let dst = (y * total + x0) * 3;
            img[dst..dst + w * 3].copy_from_slice(src);
        }
        x0 += w;
    }
    let tmp = path.with_extension("jpg.tmp");
    let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    #[allow(clippy::cast_possible_truncation)]
    JpegEncoder::new_with_quality(file, 80)
        .encode(&img, total as u32, h as u32, image::ExtendedColorType::Rgb8)
        .map_err(|e| Error::Invalid(format!("jpeg: {e}")))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_jpeg() {
        let dir = std::env::temp_dir().join(format!("ffcine-strip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let a = vec![200u8; 16 * 9 * 3];
        let b = vec![20u8; 16 * 9 * 3];
        let p = dir.join("s.jpg");
        write_strip(
            &p,
            &[
                Frame {
                    w: 16,
                    h: 9,
                    rgb: &a,
                },
                Frame {
                    w: 16,
                    h: 9,
                    rgb: &b,
                },
            ],
        )
        .expect("strip");
        let bytes = std::fs::read(&p).expect("read");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
