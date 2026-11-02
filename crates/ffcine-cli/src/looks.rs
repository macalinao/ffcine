//! `ffcine looks`: every shot × grade variant as a full-res still through one warm GPU pipeline,
//! plus a labelled contact sheet per shot for posting in chat. Also the timeline grade -> GPU
//! [`Grade`] mapping `still` uses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bpaf::Bpaf;
use ffcine::delivery;
use ffcine::shaders::{self, LookParams};
use ffcine::text::{Align, TextStyle, Typesetter};
use ffcine::timeline::{
    Clip, ClipGrade, Effect, GradeParams, HasMetadata, Project, SourceKind, Timeline,
};
use ffcine::{
    Backend, Device, EncoderSettings, Grade, Hook, Lut, LutStage, LutType, Pipeline, Size,
};
use image::{ImageEncoder as _, Rgb, RgbImage};
use serde_json::{Map, Value};

/// A clip's grade effect and source kind.
pub fn clip_grade(clip: &Clip) -> (Option<ClipGrade>, SourceKind) {
    let cg = clip.effects.iter().find_map(|e| match e {
        Effect::Effect(g) => g
            .ffcine_params::<ClipGrade>("ffcine.color.grade", 1)
            .and_then(Result::ok),
        _ => None,
    });
    let kind = match clip
        .ffcine()
        .and_then(|f| f.get("source"))
        .and_then(|s| s.get("kind"))
        .and_then(Value::as_str)
    {
        Some("video" | "photo" | "iphone" | "ai") => SourceKind::Display,
        _ => SourceKind::Log,
    };
    (cg, kind)
}

/// Loaded LUTs by path and strength.
#[derive(Default)]
pub struct Luts(HashMap<(PathBuf, u32), Lut>);

impl Luts {
    /// The LUT at `path` mixed with identity by `strength` (`lut_strength`).
    fn get(&mut self, path: &Path, strength: f64) -> anyhow::Result<Lut> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let key = (path.to_owned(), (strength.clamp(0.0, 1.0) * 1000.0) as u32);
        if let Some(l) = self.0.get(&key) {
            return Ok(l.clone());
        }
        let mut lut =
            Lut::from_cube_file(path).with_context(|| format!("LUT {}", path.display()))?;
        if strength < 1.0 {
            #[allow(clippy::cast_possible_truncation)]
            let s = strength.clamp(0.0, 1.0) as f32;
            let id = Lut::identity(lut.size);
            let span: Vec<f32> = (0..3)
                .map(|c| lut.domain_max[c] - lut.domain_min[c])
                .collect();
            for (v, i) in lut.data.iter_mut().zip(&id.data) {
                for c in 0..3 {
                    let ident = lut.domain_min[c] + i[c] * span[c];
                    v[c] = ident + s * (v[c] - ident);
                }
            }
        }
        self.0.insert(key, lut.clone());
        Ok(lut)
    }
}

/// The GPU grade for resolved `params`: the LUT (alias or path, relative to `dir`, mixed by
/// `lut_strength`) and the post-LUT look hook (warmth, tint, contrast, gamma, lift, gain,
/// saturation), plus `sharpen`. Returns what isn't applied yet as warnings.
#[allow(clippy::cast_possible_truncation)]
pub fn grade_for(
    project: &Project,
    dir: &Path,
    g: &GradeParams,
    luts: &mut Luts,
) -> anyhow::Result<(Grade, Vec<String>)> {
    let lut = project
        .lut_path(g.lut.as_deref())
        .map(|p| luts.get(&dir.join(p), g.lut_strength.unwrap_or(1.0)))
        .transpose()?
        .map(|lut| LutStage {
            lut,
            kind: LutType::Normalized,
        });
    let f = |v: Option<f64>, d: f32| v.map_or(d, |v| v as f32);
    let look = LookParams {
        exposure: f(g.exposure, 0.0),
        warmth: f(g.warmth, 0.0),
        tint: f(g.tint, 0.0),
        contrast: f(g.contrast, 0.0),
        gamma: f(g.gamma, 1.0),
        lift: f(g.lift, 0.0),
        gain: f(g.gain, 1.0),
        saturation: f(g.saturation, 1.0),
    };
    let mut skipped = Vec::new();
    for (k, set) in [
        (
            "rgb_gain",
            g.rgb_gain
                .is_some_and(|v| v.iter().any(|c| (c - 1.0).abs() > 1e-9)),
        ),
        ("denoise", g.denoise.is_some_and(|v| v != 0.0)),
        ("extra", g.extra.as_ref().is_some_and(|v| !v.is_empty())),
    ] {
        if set {
            skipped.push(format!("{k} not applied yet"));
        }
    }
    let hooks = if look.is_identity() {
        Vec::new()
    } else {
        vec![Hook {
            name: "look".into(),
            source: shaders::look().to_owned(),
            params: look.params(),
            tracks: Vec::new(),
        }]
    };
    Ok((
        Grade {
            lut,
            hooks,
            sharpen: f(g.sharpen, 0.0),
            ..Grade::default()
        },
        skipped,
    ))
}

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::doc_markdown)]
pub struct LooksArgs {
    /// Timeline time of a shot to compare: seconds, M:SS or H:MM:SS. Repeatable.
    #[bpaf(argument("TIME"), many)]
    at: Vec<String>,
    /// Also compare N shots spread evenly over the timeline.
    #[bpaf(argument("N"))]
    sample: Option<usize>,
    /// A clip outside the timeline at a time, FILE@SECONDS (photos: any time). Repeatable.
    #[bpaf(argument("FILE@SECONDS"), many)]
    clip: Vec<String>,
    /// A variant, [NAME:]SPEC; see the variant format below (default: the cut's own grade and
    /// every project look). Repeatable.
    #[bpaf(short('v'), argument("VARIANT"), many)]
    variant: Vec<String>,
    /// Output directory (default: looks).
    #[bpaf(short('o'), argument("DIR"), fallback(PathBuf::from("looks")))]
    output: PathBuf,
    /// Contact sheet width in px.
    #[bpaf(argument("PX"), fallback(2400), display_fallback)]
    sheet_width: u32,
    /// Still size in px, e.g. 1920x1080 (default: the source's).
    #[bpaf(argument("WxH"))]
    size: Option<Size>,
    /// CUDA device index (default: picked automatically).
    #[bpaf(argument("N"))]
    cuda: Option<u32>,
    /// Don't use an on-disk shader cache.
    pub no_cache: bool,
    /// Timeline (.otio): shots by time, looks, LUT aliases and each clip's own grade; optional
    /// with --clip.
    #[bpaf(positional("TIMELINE"))]
    timeline: Option<PathBuf>,
}

struct Shot {
    label: String,
    path: PathBuf,
    time: f64,
    grade: Option<ClipGrade>,
    kind: SourceKind,
}

#[derive(Debug, Clone, PartialEq)]
struct Variant {
    name: String,
    look: Option<String>,
    lut: Option<String>,
    params: Map<String, Value>,
}

fn parse_variant(spec: &str, project: &Project) -> anyhow::Result<Variant> {
    let (name, body) = match spec.split_once(':') {
        Some((n, b)) if !n.contains('=') && !n.contains('/') => (n.to_owned(), b),
        _ => (spec.to_owned(), spec),
    };
    let mut v = Variant {
        name,
        look: None,
        lut: None,
        params: Map::new(),
    };
    for tok in body.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match tok.split_once('=') {
            Some(("look", l)) => v.look = Some(l.to_owned()),
            Some(("lut", l)) => v.lut = Some(l.to_owned()),
            Some((k, val)) => {
                let x: f64 = val
                    .trim_start_matches('+')
                    .parse()
                    .with_context(|| format!("variant {spec}: {k}={val} is not a number"))?;
                v.params.insert(k.to_owned(), x.into());
            }
            None if project.looks.contains_key(tok) => v.look = Some(tok.to_owned()),
            None if Path::new(tok)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("cube"))
                || project.luts.contains_key(tok)
                || tok == "none" =>
            {
                v.lut = Some(tok.to_owned());
            }
            None if tok == "cut" => {}
            None => anyhow::bail!("variant {spec}: {tok} is not a look, LUT or k=v"),
        }
    }
    Ok(v)
}

/// Stills (one frame, no time to seek to).
fn is_photo(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "heic" | "heif" | "jpg" | "jpeg" | "png" | "dng" | "tif" | "tiff" | "webp" | "avif"
        )
    })
}

fn file_label(p: &Path) -> String {
    p.file_stem()
        .map_or_else(|| "clip".into(), |s| s.to_string_lossy().into_owned())
}

#[allow(clippy::cast_precision_loss)]
fn shots(a: &LooksArgs, t: Option<&Timeline>) -> anyhow::Result<Vec<Shot>> {
    let mut times = Vec::new();
    for s in &a.at {
        times.push(delivery::parse_time(s)?);
    }
    let mut out = Vec::new();
    if let Some(t) = t {
        if let Some(n) = a.sample.filter(|n| *n > 0) {
            let d = t.duration();
            times.extend((0..n).map(|i| (i as f64 + 0.5) * d / n as f64));
        }
        for at in times {
            let f =
                delivery::frame_at(t, at).with_context(|| format!("no video clip at {at:.2} s"))?;
            if out
                .iter()
                .any(|s: &Shot| s.path == f.path && (s.time - f.source_time).abs() < 0.5)
            {
                continue;
            }
            let (grade, kind) = clip_grade(f.clip);
            out.push(Shot {
                label: format!(
                    "{}-{}",
                    delivery::timestamp(at, at >= 3600.0).replace(':', "."),
                    file_label(&f.path)
                ),
                time: if is_photo(&f.path) {
                    0.0
                } else {
                    f.source_time
                },
                path: f.path,
                grade,
                kind,
            });
        }
    } else if !times.is_empty() || a.sample.is_some() {
        anyhow::bail!("--at and --sample need a TIMELINE");
    }
    for c in &a.clip {
        let (p, s) = c
            .rsplit_once('@')
            .with_context(|| format!("--clip {c}: expected FILE@SECONDS"))?;
        let time = delivery::parse_time(s)?;
        let path = PathBuf::from(p);
        out.push(Shot {
            label: format!("{}-{}", file_label(&path), s.replace(':', ".")),
            path,
            time,
            grade: None,
            kind: SourceKind::Log,
        });
    }
    anyhow::ensure!(
        !out.is_empty(),
        "nothing to compare: give --at, --sample or --clip"
    );
    Ok(out)
}

fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    s.trim_matches('_').chars().take(48).collect()
}

#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn looks(a: LooksArgs, ff: &dyn Backend) -> anyhow::Result<()> {
    let t0 = Instant::now();
    let t = a.timeline.as_deref().map(Timeline::from_file).transpose()?;
    let dir = a
        .timeline
        .as_deref()
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_owned);
    let project = t
        .as_ref()
        .and_then(Timeline::project)
        .transpose()?
        .unwrap_or_default();
    let mut variants = a
        .variant
        .iter()
        .map(|v| parse_variant(v, &project))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if variants.is_empty() {
        variants.push(parse_variant("cut", &project)?);
        for name in project.looks.keys() {
            variants.push(parse_variant(name, &project)?);
        }
    }
    let shots = shots(&a, t.as_ref())?;
    std::fs::create_dir_all(&a.output)
        .with_context(|| format!("creating {}", a.output.display()))?;

    let mut luts = Luts::default();
    let mut ts = Typesetter::new(".");
    let mut warned = Vec::new();
    let cell_w = grid(variants.len(), a.sheet_width).1;
    let mut n = 0usize;
    for shot in &shots {
        // The GPU renders the next variant while workers write JPEGs and thumbnails.
        let done = std::thread::scope(|sc| -> anyhow::Result<usize> {
            let mut workers = Vec::new();
            for v in &variants {
                let mut cg = shot.grade.clone().unwrap_or_default();
                if let Some(l) = &v.look {
                    cg.look = Some(l.clone());
                }
                let mut g = project
                    .resolve_grade(Some(&cg), shot.kind)
                    .with_context(|| format!("variant {}", v.name))?;
                let over: GradeParams = serde_json::from_value(Value::Object(v.params.clone()))
                    .with_context(|| format!("variant {}", v.name))?;
                g.overlay(&over);
                if let Some(l) = &v.lut {
                    g.lut = Some(l.clone());
                }
                let (mut grade, skipped) = grade_for(&project, &dir, &g, &mut luts)?;
                for s in skipped {
                    let w = format!("{}: {s}", v.name);
                    if !warned.contains(&w) {
                        eprintln!("warning: {w}");
                        warned.push(w);
                    }
                }
                grade.size = a.size;
                grade.output = ffcine::ColorSpace::BT709_RGB;
                n += 1;
                let png = a.output.join(format!(".still-{n}.png"));
                let mut enc = EncoderSettings::png_still();
                enc.extra.push(("compression_level".into(), "0".into()));
                let mut p = Pipeline::new(&shot.path, &png)
                    .range(Duration::from_secs_f64(shot.time.max(0.0)), Some(1))
                    .grade(grade)
                    .encoder(enc);
                if let Some(d) = a.cuda {
                    p = p.device(Device::Cuda(d));
                }
                if let Err(e) = p.run(ff) {
                    eprintln!("warning: {} / {}: {e}; skipped", shot.label, v.name);
                    continue;
                }
                let jpg = a
                    .output
                    .join(format!("{}_{}.jpg", slug(&shot.label), slug(&v.name)));
                let name = v.name.clone();
                workers.push(sc.spawn(move || -> anyhow::Result<(String, RgbImage)> {
                    let img = image::open(&png)
                        .with_context(|| format!("reading {}", png.display()))?
                        .to_rgb8();
                    let _ = std::fs::remove_file(&png);
                    save_jpeg(&img, &jpg, 92)?;
                    let h = (f64::from(cell_w) * f64::from(img.height()) / f64::from(img.width()))
                        .round() as u32;
                    let thumb = image::imageops::resize(
                        &img,
                        cell_w,
                        h.max(1),
                        image::imageops::FilterType::Triangle,
                    );
                    Ok((name, thumb))
                }));
            }
            let mut thumbs = Vec::new();
            for w in workers {
                thumbs.push(
                    w.join()
                        .map_err(|_| anyhow::anyhow!("JPEG worker panicked"))??,
                );
            }
            if thumbs.is_empty() {
                return Ok(0);
            }
            let sheet = contact_sheet(&mut ts, &shot.label, &thumbs, a.sheet_width);
            let path = a.output.join(format!("{}_sheet.jpg", slug(&shot.label)));
            let q = save_under(&sheet, &path, 5_000_000)?;
            eprintln!(
                "{}: {} variants, sheet {}x{} q{q}",
                path.display(),
                thumbs.len(),
                sheet.width(),
                sheet.height()
            );
            Ok(thumbs.len())
        })?;
        if done == 0 {
            eprintln!("warning: {}: nothing rendered", shot.label);
        }
    }
    eprintln!(
        "{} shots x {} variants in {:.1} s",
        shots.len(),
        variants.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn save_jpeg(img: &RgbImage, path: &Path, quality: u8) -> anyhow::Result<u64> {
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality).write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    std::fs::write(path, &buf).with_context(|| format!("writing {}", path.display()))?;
    Ok(buf.len() as u64)
}

/// Saves at the best quality (from 90 down) that is under `cap` bytes.
fn save_under(img: &RgbImage, path: &Path, cap: u64) -> anyhow::Result<u8> {
    let mut q = 90;
    while save_jpeg(img, path, q)? > cap && q > 40 {
        q -= 10;
    }
    Ok(q)
}

/// Draws `text` centred in the `w`×`h` box at `(x, y)` of `canvas`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names
)]
fn label(
    ts: &mut Typesetter,
    canvas: &mut RgbImage,
    text: &str,
    (x, y, w, h): (u32, u32, u32, u32),
) {
    let mut s = TextStyle::default_for(h as f32);
    s.size = h as f32 * 0.55;
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
                    let p = canvas.get_pixel_mut(x + cx as u32, y + cy as u32);
                    for c in 0..3 {
                        let v = f32::from(p.0[c]) * (1.0 - a) + 255.0 * part.colour[c] * a;
                        p.0[c] = v.round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }
}

/// Columns, cell width and gap for `n` cells in a sheet `width` px wide.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn grid(n: usize, width: u32) -> (u32, u32, u32) {
    let n = u32::try_from(n.max(1)).unwrap_or(1);
    let cols = if n <= 3 {
        n
    } else {
        f64::from(n).sqrt().ceil() as u32
    };
    let gap = (width / 120).max(4);
    (cols, (width - gap * (cols + 1)) / cols, gap)
}

/// A grid of the variants (thumbnails `cell_w` wide, see [`grid`]) with their names under each, the shot's label on top.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::many_single_char_names
)]
fn contact_sheet(
    ts: &mut Typesetter,
    title: &str,
    thumbs: &[(String, RgbImage)],
    width: u32,
) -> RgbImage {
    let (cols, cell_w, gap) = grid(thumbs.len(), width);
    let rows = u32::try_from(thumbs.len().max(1))
        .unwrap_or(1)
        .div_ceil(cols);
    let (sw, sh) = thumbs
        .first()
        .map_or((16, 9), |(_, i)| (i.width(), i.height()));
    let cell_h = (f64::from(cell_w) * f64::from(sh) / f64::from(sw)).round() as u32;
    let label_h = (cell_w / 14).clamp(24, 72);
    let head = label_h;
    let height = head + gap + rows * (cell_h + label_h + gap);
    let mut canvas = RgbImage::from_pixel(width, height, Rgb([20, 20, 20]));
    label(ts, &mut canvas, title, (0, 0, width, head));
    for (i, (name, img)) in thumbs.iter().enumerate() {
        let (c, r) = (i as u32 % cols, i as u32 / cols);
        let x = gap + c * (cell_w + gap);
        let y = head + gap + r * (cell_h + label_h + gap);
        image::imageops::replace(&mut canvas, img, i64::from(x), i64::from(y));
        label(ts, &mut canvas, name, (x, y + cell_h, cell_w, label_h));
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants() {
        let mut p = Project::default();
        p.looks.insert("day".into(), GradeParams::default());
        p.luts.insert("normal".into(), "/x.cube".into());
        let v = parse_variant("warm:look=day,warmth=+0.03,exposure=0.3", &p).expect("v");
        assert_eq!(v.name, "warm");
        assert_eq!(v.look.as_deref(), Some("day"));
        assert_eq!(v.params["warmth"], 0.03);
        let v = parse_variant("day", &p).expect("v");
        assert_eq!((v.name.as_str(), v.look.as_deref()), ("day", Some("day")));
        let v = parse_variant("/l/teal.cube", &p).expect("v");
        assert_eq!(v.lut.as_deref(), Some("/l/teal.cube"));
        assert!(parse_variant("nope", &p).is_err());
    }

    #[test]
    fn grade_maps_the_look() {
        let p = Project::default();
        let g = GradeParams {
            warmth: Some(0.02),
            exposure: Some(0.3),
            ..GradeParams::default()
        };
        let (grade, skipped) =
            grade_for(&p, Path::new("."), &g, &mut Luts::default()).expect("grade");
        assert_eq!(grade.hooks.len(), 1);
        assert!(
            grade.hooks[0]
                .params
                .contains(&("exposure".to_owned(), f64::from(0.3_f32)))
        );
        assert_eq!(skipped, Vec::<String>::new());
        let (grade, _) = grade_for(
            &p,
            Path::new("."),
            &GradeParams::default(),
            &mut Luts::default(),
        )
        .expect("grade");
        assert_eq!(grade.hooks, []);
    }
}
