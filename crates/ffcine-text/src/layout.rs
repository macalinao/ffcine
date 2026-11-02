//! Laying out and rasterising text with `cosmic-text`: one coverage mask per text, plus derived
//! masks for the shadow (blurred) and outline (dilated), as [`Layer`]s ready to be overlays.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cosmic_text::{
    Align as CAlign, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    Weight,
};
use ffcine_core::{Error, Result};
use ffcine_render::{Mask, OverlayPart};

use crate::style::{Align, TextStyle};

/// One mask and where to draw it, bottom layer first.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub mask: Arc<Mask>,
    pub parts: Vec<OverlayPart>,
    pub role: Role,
}

/// What a [`Layer`] draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A solid box behind the text (or a `rect` clip's rectangle): a 1x1 mask stretched.
    Box,
    Shadow,
    Outline,
    Fill,
    Highlight,
}

/// A grapheme cluster of the laid-out text: `[x0, x1]` along its line, in text box px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    pub line: usize,
    pub x: [f32; 2],
    /// Whitespace (ends a word).
    pub space: bool,
}

/// A laid-out text: its layers and the text box (top left, size) in output px.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub layers: Vec<Layer>,
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// Margin around the text box in every mask (blur, outline), px.
    pub pad: f32,
    /// Top of each line in the text box, px, plus the box's bottom.
    pub lines: Vec<f32>,
    /// Clusters in reading order.
    pub glyphs: Vec<Glyph>,
}

/// Font database, glyph cache and loaded font files. Building one scans the system fonts, so
/// keep it for every text of a render.
pub struct Typesetter {
    fonts: FontSystem,
    cache: SwashCache,
    /// (font file, face index) -> the face's family, weight and slant.
    files: HashMap<(PathBuf, u32), LoadedFace>,
    base_dir: PathBuf,
}

impl std::fmt::Debug for Typesetter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Typesetter")
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

/// A face loaded from a font file.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedFace {
    pub family: String,
    pub weight: Weight,
    pub italic: bool,
}

/// `Light`, `Medium`, `SemiBold Italic`, ... -> weight and slant.
fn face(name: &str) -> (Option<Weight>, bool) {
    let n = name.to_ascii_lowercase().replace([' ', '-'], "");
    let w = [
        ("thin", 100),
        ("extralight", 200),
        ("ultralight", 200),
        ("light", 300),
        ("regular", 400),
        ("book", 400),
        ("medium", 500),
        ("semibold", 600),
        ("demibold", 600),
        ("extrabold", 800),
        ("ultrabold", 800),
        ("black", 900),
        ("heavy", 900),
        ("bold", 700),
    ]
    .iter()
    .find(|(k, _)| n.contains(k))
    .map(|(_, w)| Weight(*w));
    (w, n.contains("italic") || n.contains("oblique"))
}

impl Typesetter {
    /// System fonts plus files given relative to `base_dir` (the timeline's directory).
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            fonts: FontSystem::new(),
            cache: SwashCache::new(),
            files: HashMap::new(),
            base_dir: base_dir.into(),
        }
    }

    /// Whether any font is loaded (tests skip without one).
    #[cfg(test)]
    pub(crate) fn has_fonts(&self) -> bool {
        self.fonts.db().faces().next().is_some()
    }

    /// Loads a font file (once) and returns face `index` of it (0 unless a `.ttc`).
    pub fn load(&mut self, file: &Path, index: u32) -> Result<LoadedFace> {
        let path = if file.is_absolute() {
            file.to_owned()
        } else {
            self.base_dir.join(file)
        };
        if let Some(f) = self.files.get(&(path.clone(), index)) {
            return Ok(f.clone());
        }
        let data = std::fs::read(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let db = self.fonts.db_mut();
        let before: Vec<_> = db.faces().map(|f| f.id).collect();
        db.load_font_data(data);
        for f in db.faces().filter(|f| !before.contains(&f.id)) {
            if let Some((family, _)) = f.families.first() {
                self.files.insert(
                    (path.clone(), f.index),
                    LoadedFace {
                        family: family.clone(),
                        weight: Weight(f.weight.0),
                        italic: f.style != Style::Normal,
                    },
                );
            }
        }
        self.files
            .get(&(path.clone(), index))
            .cloned()
            .ok_or_else(|| Error::Invalid {
                what: "font file and index",
                value: format!("{} #{index}", path.display()),
            })
    }

    /// Lays out `text` in `style` for an `out` = `[w, h]` px frame, lengths scaled by `scale`
    /// (output height / `frame_size` height).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::too_many_lines
    )]
    pub fn render(
        &mut self,
        text: &str,
        style: &TextStyle,
        scale: f32,
        out: [u32; 2],
    ) -> Result<Option<Rendered>> {
        let text = if style.uppercase {
            text.to_uppercase()
        } else {
            text.to_owned()
        };
        if text.trim().is_empty() {
            return Ok(None);
        }
        let loaded = match &style.font_file {
            Some(f) => Some(self.load(f, style.font_index)?),
            None => None,
        };
        let family_name = loaded
            .as_ref()
            .map(|l| l.family.clone())
            .or_else(|| style.font.clone());
        let (fw, fh) = (out[0] as f32, out[1] as f32);
        let size = (style.size * scale).max(1.0);
        let (face_weight, face_italic) = match (&loaded, style.face.as_deref()) {
            (_, Some(f)) => face(f),
            (Some(l), None) => (Some(l.weight), l.italic),
            (None, None) => (None, false),
        };
        let mut attrs = Attrs::new()
            .family(
                family_name
                    .as_deref()
                    .map_or(Family::SansSerif, Family::Name),
            )
            .weight(if style.bold {
                Weight::BOLD
            } else {
                face_weight.unwrap_or(Weight::NORMAL)
            })
            .style(if style.italic || face_italic {
                Style::Italic
            } else {
                Style::Normal
            });
        if style.tracking != 0.0 {
            attrs = attrs.letter_spacing(style.tracking);
        }
        let align = match style.alignment {
            Align::Left => CAlign::Left,
            Align::Center => CAlign::Center,
            Align::Right => CAlign::Right,
        };
        let mut buf = Buffer::new(
            &mut self.fonts,
            Metrics::new(size, size * style.line_height),
        );
        buf.set_size(Some(style.max_width * fw), None);
        let spans = spans(&text, style.highlight.is_some());
        buf.set_rich_text(
            spans.iter().map(|(t, hi)| {
                (
                    t.as_str(),
                    if *hi {
                        attrs.clone().color(MARK)
                    } else {
                        attrs.clone()
                    },
                )
            }),
            &attrs,
            Shaping::Advanced,
            Some(align),
        );
        buf.shape_until_scroll(&mut self.fonts, false);
        // Shrink the box to the widest line so alignment is within the text, not the wrap width.
        let width = buf
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0, f32::max)
            .ceil();
        buf.set_size(Some(width.max(1.0)), None);
        buf.shape_until_scroll(&mut self.fonts, false);
        let height = buf
            .layout_runs()
            .map(|r| r.line_top + r.line_height)
            .fold(0.0, f32::max)
            .ceil();
        let mut lines = Vec::new();
        let mut glyphs = Vec::new();
        for (line, run) in buf.layout_runs().enumerate() {
            lines.push(run.line_top);
            let mut gs: Vec<Glyph> = Vec::new();
            let mut last = None;
            for g in run.glyphs {
                let space = run
                    .text
                    .get(g.start..g.end)
                    .is_some_and(|t| t.trim().is_empty());
                match gs.last_mut() {
                    // One cluster shaped into several glyphs.
                    Some(c) if last == Some(g.start) => {
                        c.x = [c.x[0].min(g.x), c.x[1].max(g.x + g.w)];
                    }
                    _ => gs.push(Glyph {
                        line,
                        x: [g.x, g.x + g.w],
                        space,
                    }),
                }
                last = Some(g.start);
            }
            gs.sort_by(|a, b| a.x[0].total_cmp(&b.x[0]));
            glyphs.extend(gs);
        }
        lines.push(height);

        let outline = style.outline.as_ref().map_or(0.0, |o| o.width * scale);
        let blur = style.shadow.as_ref().map_or(0.0, |s| s.blur * scale);
        let pad = (outline + 2.0 * blur + 0.5 * size).ceil() as u32 + 2;
        let (mw, mh) = (width as u32 + 2 * pad, height as u32 + 2 * pad);
        let mut fill = Mask::new(mw, mh);
        let mut marked = style.highlight.map(|_| Mask::new(mw, mh));
        buf.draw(
            &mut self.fonts,
            &mut self.cache,
            Color::rgb(255, 255, 255),
            |x, y, w, h, c| {
                let a = c.a();
                let fill = match &mut marked {
                    Some(m) if (c.r(), c.g(), c.b()) == (MARK.r(), MARK.g(), MARK.b()) => m,
                    _ => &mut fill,
                };
                for dy in 0..h as i32 {
                    for dx in 0..w as i32 {
                        let (px, py) = (x + dx + pad as i32, y + dy + pad as i32);
                        if px >= 0 && py >= 0 && (px as u32) < mw && (py as u32) < mh {
                            let i = py as usize * mw as usize + px as usize;
                            fill.data[i] = fill.data[i].max(a);
                        }
                    }
                }
            },
        );

        let anchor = [
            (style.position[0] + 0.5) * fw,
            (style.position[1] + 0.5) * fh,
        ];
        let origin = [
            (anchor[0] - style.anchor[0] * width).round(),
            (anchor[1] - style.anchor[1] * height).round(),
        ];
        let at = [origin[0] - pad as f32, origin[1] - pad as f32];
        let part = |pos: [f32; 2], colour: [f32; 4]| OverlayPart {
            pos,
            colour,
            ..OverlayPart::default()
        };
        let mut layers = Vec::new();
        if let Some(b) = &style.box_ {
            let p = b.padding * scale;
            layers.push(Layer {
                mask: Arc::new(Mask::solid()),
                parts: vec![OverlayPart {
                    pos: [origin[0] - p, origin[1] - p],
                    size: Some([width + 2.0 * p, height + 2.0 * p]),
                    colour: b.colour,
                    src: None,
                }],
                role: Role::Box,
            });
        }
        if let Some(s) = &style.shadow {
            let all = union(&fill, marked.as_ref());
            let src = match &style.outline {
                Some(o) => dilate(&all, (o.width * scale).round() as usize),
                None => all,
            };
            let m = blur_mask(&src, blur.round() as usize);
            layers.push(Layer {
                mask: Arc::new(m),
                parts: vec![part(
                    [at[0] + s.offset[0] * scale, at[1] + s.offset[1] * scale],
                    s.colour,
                )],
                role: Role::Shadow,
            });
        }
        if let Some(o) = &style.outline {
            layers.push(Layer {
                mask: Arc::new(dilate(
                    &union(&fill, marked.as_ref()),
                    (o.width * scale).round() as usize,
                )),
                parts: vec![part(at, o.colour)],
                role: Role::Outline,
            });
        }
        layers.push(Layer {
            mask: Arc::new(fill),
            parts: vec![part(at, style.colour)],
            role: Role::Fill,
        });
        if let (Some(m), Some(c)) = (marked, style.highlight) {
            layers.push(Layer {
                mask: Arc::new(m),
                parts: vec![part(at, c)],
                role: Role::Highlight,
            });
        }
        Ok(Some(Rendered {
            layers,
            origin,
            size: [width, height],
            pad: pad as f32,
            lines,
            glyphs,
        }))
    }
}

/// Glyph colour that marks highlighted spans while rasterising (never drawn as is).
const MARK: Color = Color::rgb(255, 0, 255);

/// `a *b* c` -> `[("a ", false), ("b", true), (" c", false)]` when `marks`, else one span.
fn spans(text: &str, marks: bool) -> Vec<(String, bool)> {
    if !marks {
        return vec![(text.to_owned(), false)];
    }
    text.split('*')
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, s)| (s.to_owned(), i % 2 == 1))
        .collect()
}

/// Per-pixel max of two masks of the same size.
fn union(a: &Mask, b: Option<&Mask>) -> Mask {
    let mut out = a.clone();
    if let Some(b) = b {
        for (o, v) in out.data.iter_mut().zip(&b.data) {
            *o = (*o).max(*v);
        }
    }
    out
}

/// Separable max filter, radius `r` (a square outline).
fn dilate(m: &Mask, r: usize) -> Mask {
    separable(m, r, |w| w.iter().copied().max().unwrap_or(0))
}

/// Three box blurs of radius `r / 2` (close to a Gaussian of that radius).
fn blur_mask(m: &Mask, r: usize) -> Mask {
    let k = r.div_ceil(2);
    let mut out = m.clone();
    for _ in 0..3 {
        out = separable(&out, k, |w| {
            let sum: usize = w.iter().map(|&v| usize::from(v)).sum();
            u8::try_from(sum / w.len()).unwrap_or(u8::MAX)
        });
    }
    out
}

/// Applies `f` over a `2r+1` window along rows, then columns; outside the mask is 0.
#[allow(clippy::many_single_char_names)]
fn separable(m: &Mask, r: usize, f: impl Fn(&[u8]) -> u8) -> Mask {
    if r == 0 {
        return m.clone();
    }
    let (w, h) = (m.width as usize, m.height as usize);
    let pass = |src: &[u8], len: usize, lines: usize, at: &dyn Fn(usize, usize) -> usize| {
        let mut dst = vec![0u8; src.len()];
        let mut win = vec![0u8; 2 * r + 1];
        for l in 0..lines {
            for i in 0..len {
                for (k, slot) in win.iter_mut().enumerate() {
                    let j = (i + k).checked_sub(r).filter(|j| *j < len);
                    *slot = j.map_or(0, |j| src[at(l, j)]);
                }
                dst[at(l, i)] = f(&win);
            }
        }
        dst
    };
    let rows = pass(&m.data, w, h, &|y, x| y * w + x);
    let data = pass(&rows, h, w, &|x, y| y * w + x);
    Mask {
        width: m.width,
        height: m.height,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_spans() {
        assert_eq!(
            spans("on this *payday,* babe", true),
            [
                ("on this ".to_owned(), false),
                ("payday,".to_owned(), true),
                (" babe".to_owned(), false)
            ]
        );
        assert_eq!(spans("a*b", false), [("a*b".to_owned(), false)]);
    }

    #[test]
    fn faces() {
        assert_eq!(face("SemiBold Italic"), (Some(Weight(600)), true));
        assert_eq!(face("Light"), (Some(Weight(300)), false));
        assert_eq!(face("ExtraBold"), (Some(Weight(800)), false));
    }

    #[test]
    fn dilate_and_blur() {
        let mut m = Mask::new(7, 7);
        m.data[3 * 7 + 3] = 255;
        let d = dilate(&m, 1);
        assert_eq!(d.data.iter().map(|v| u32::from(*v)).sum::<u32>(), 9 * 255);
        let b = blur_mask(&d, 2);
        assert!(b.data[3 * 7 + 3] > 0 && b.data[3 * 7 + 3] < 255);
        assert!(b.data[0] < b.data[3 * 7 + 3]);
    }

    #[test]
    fn lays_out_with_system_fonts() {
        let mut t = Typesetter::new(".");
        if t.fonts.db().faces().next().is_none() {
            eprintln!("no system fonts; skipped");
            return;
        }
        let mut s = TextStyle::default_for(1080.0);
        s.position = [-0.45, -0.4];
        s.anchor = [0.0, 0.0];
        s.shadow = Some(crate::style::Shadow {
            colour: [0.0, 0.0, 0.0, 0.5],
            offset: [0.0, 2.0],
            blur: 4.0,
        });
        let r = t
            .render("Arrival\nOld Town", &s, 1.0, [1920, 1080])
            .expect("render")
            .expect("text");
        assert_eq!(r.origin, [96.0, 108.0]);
        assert!(r.size[1] > 2.0 * 54.0 * 0.9, "two lines: {:?}", r.size);
        assert_eq!(r.layers.len(), 2);
        let fill = &r.layers[1].mask;
        assert!(fill.data.iter().any(|v| *v > 200), "glyphs drawn");
    }
}
