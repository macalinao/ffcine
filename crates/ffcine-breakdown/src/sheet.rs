//! The contact sheet: every shot's keyframe in a grid, labelled with its number and length.

use std::path::Path;

use ffcine_text::{Align, TextStyle, Typesetter};
use image::{Rgb, RgbImage};

use crate::{Error, Result};

/// Most columns in a sheet.
pub const MAX_COLUMNS: u32 = 24;
/// Most rows on one page, so a long video's sheet stays a sane image (and under JPEG's 65535 px).
pub const MAX_ROWS: u32 = 12;

/// Columns for `n` cells of `aspect` (width / height) in a sheet: more for portrait frames.
pub fn columns(n: usize, aspect: f64) -> u32 {
    let n = u32::try_from(n.max(1)).unwrap_or(1);
    let want = if aspect < 0.8 {
        8
    } else if aspect < 1.2 {
        6
    } else {
        5
    };
    n.min(want)
}

/// Draws `text` centred in the `w`×`h` box at `(x, y)` of `canvas`; nothing without fonts.
#[allow(clippy::cast_possible_wrap)]
fn label(
    ts: &mut Typesetter,
    canvas: &mut RgbImage,
    text: &str,
    (x, y, w, h): (u32, u32, u32, u32),
) {
    let mut s = TextStyle::default_for(h as f32);
    s.size = h as f32 * 0.6;
    s.alignment = Align::Center;
    s.max_width = 0.96;
    let Ok(Some(r)) = ts.render(text, &s, 1.0, [w, h]) else {
        return;
    };
    for l in &r.layers {
        for part in &l.parts {
            let (px, py) = (part.pos[0] as i64, part.pos[1] as i64);
            for my in 0..l.mask.height {
                for mx in 0..l.mask.width {
                    let a = f32::from(l.mask.data[(my * l.mask.width + mx) as usize]) / 255.0
                        * part.colour[3];
                    let (cx, cy) = (px + i64::from(mx), py + i64::from(my));
                    if a <= 0.0 || cx < 0 || cy < 0 || cx >= i64::from(w) || cy >= i64::from(h) {
                        continue;
                    }
                    let Some(p) = canvas.get_pixel_mut_checked(x + cx as u32, y + cy as u32) else {
                        continue;
                    };
                    for c in 0..3 {
                        let v = f32::from(p.0[c]) * (1.0 - a) + 255.0 * part.colour[c] * a;
                        p.0[c] = v.round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }
}

/// A grid of `cells` (label, image), `width` px wide, `cols` across, with `title` on top.
pub fn render(title: &str, cells: &[(String, RgbImage)], width: u32, cols: u32) -> RgbImage {
    let mut ts = Typesetter::new(".");
    let cols = cols.clamp(1, MAX_COLUMNS);
    let gap = (width / 160).max(4);
    let cell_w = (width.saturating_sub(gap * (cols + 1)) / cols).max(16);
    let (sw, sh) = cells
        .first()
        .map_or((16, 9), |(_, i)| (i.width().max(1), i.height().max(1)));
    let cell_h = (f64::from(cell_w) * f64::from(sh) / f64::from(sw)).round() as u32;
    let label_h = (cell_w / 9).clamp(20, 48);
    let head = (width / 40).clamp(28, 64);
    let rows = u32::try_from(cells.len().max(1))
        .unwrap_or(1)
        .div_ceil(cols);
    let height = head + gap + rows * (cell_h + label_h + gap);
    let mut canvas = RgbImage::from_pixel(width, height, Rgb([18, 18, 18]));
    label(&mut ts, &mut canvas, title, (0, 0, width, head));
    for (i, (name, img)) in cells.iter().enumerate() {
        let i = u32::try_from(i).unwrap_or(u32::MAX);
        let (c, r) = (i % cols, i / cols);
        let x = gap + c * (cell_w + gap);
        let y = head + gap + r * (cell_h + label_h + gap);
        let thumb =
            image::imageops::resize(img, cell_w, cell_h, image::imageops::FilterType::Triangle);
        image::imageops::replace(&mut canvas, &thumb, i64::from(x), i64::from(y));
        label(&mut ts, &mut canvas, name, (x, y + cell_h, cell_w, label_h));
    }
    canvas
}

/// Writes `img` as a JPEG at `quality`; returns its size.
pub fn save_jpeg(img: &RgbImage, path: &Path, quality: u8) -> Result<u64> {
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality)
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| Error::Invalid(format!("jpeg: {e}")))?;
    std::fs::write(path, &buf)?;
    Ok(buf.len() as u64)
}

/// Saves at the best quality (from 88 down) under `cap` bytes, so chat uploads take it; an error
/// (and no file) if even the lowest quality is over.
pub fn save_under(img: &RgbImage, path: &Path, cap: u64) -> Result<u8> {
    let mut q = 88;
    let mut size = save_jpeg(img, path, q)?;
    while size > cap && q > 40 {
        q -= 12;
        size = save_jpeg(img, path, q)?;
    }
    if size > cap {
        let _ = std::fs::remove_file(path);
        return Err(Error::Invalid(format!(
            "{}: {size} bytes at quality {q}, over {cap}",
            path.display()
        )));
    }
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grids_portrait_cells() {
        let cells: Vec<(String, RgbImage)> = (0..10)
            .map(|i| {
                (
                    format!("{i}"),
                    RgbImage::from_pixel(90, 160, Rgb([200, 40, 40])),
                )
            })
            .collect();
        let cols = columns(cells.len(), 90.0 / 160.0);
        assert_eq!(cols, 8);
        let s = render("x", &cells, 800, cols);
        assert_eq!(s.width(), 800);
        // Two rows of 9:16 cells plus labels and the title.
        assert!(s.height() > 2 * 160);
        assert_eq!(columns(2, 1.78), 2);
    }

    #[test]
    fn many_columns_and_a_tiny_cap() {
        let cells: Vec<(String, RgbImage)> = (0..80)
            .map(|i| (format!("{i}"), RgbImage::from_pixel(16, 9, Rgb([9, 9, 9]))))
            .collect();
        // Clamped, and labels never draw past the edge.
        let s = render("x", &cells, 1800, 200);
        assert_eq!(s.width(), 1800);
        let dir = std::env::temp_dir().join(format!("ffcine-sheet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let p = dir.join("s.jpg");
        assert!(save_under(&s, &p, 10).is_err());
        assert!(!p.exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
