//! The `layout` template (`ffcine.comp.layout/1`): split-screen geometry, and a comp of panels
//! placed by it. The timeline's split `Stack`s use [`Layout`] (`ffcine-assembly`); a comp file
//! uses the template as `"template": {"layout": {...}, "panels": [...], "size": [W, H], "fps":
//! ..., "duration": ...}`.

use serde_json::{Map, Value, json};

use super::{item_layer, jnum, num};
use crate::model::err;
use ffcine_core::Result;

fn f(m: &Map<String, Value>, k: &str) -> Option<f64> {
    m.get(k).and_then(Value::as_f64)
}

/// The split-screen backdrop's colour, (22, 20, 18). The timeline's split also darkens its
/// corners (vignette 0.35), which a comp's canvas background can't express.
pub const BACKDROP: &str = "#161412";

/// A rectangle in output pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Split-screen layout (`ffcine.comp.layout/1`), `render.py` `layout_geom` and `layout_chain`.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// `split`/`row` (side by side), `column` (stacked), `1+2` (one left, two stacked right).
    pub kind: String,
    pub gap: f64,
    pub margin: f64,
    pub radius: f64,
    pub weights: Vec<f64>,
    pub stagger: f64,
    pub slide: f64,
    pub enter: bool,
    pub first_static: bool,
}

impl Layout {
    pub fn parse(m: &Map<String, Value>) -> Self {
        Self {
            kind: m
                .get("layout")
                .and_then(Value::as_str)
                .unwrap_or("split")
                .to_owned(),
            gap: f(m, "gap").unwrap_or(0.008),
            margin: f(m, "margin").unwrap_or(0.0),
            radius: f(m, "radius").unwrap_or(0.012),
            weights: m
                .get("weights")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default(),
            stagger: f(m, "stagger").unwrap_or(0.22),
            slide: f(m, "slide").unwrap_or(0.42),
            enter: m.get("enter").and_then(Value::as_str) != Some("none"),
            first_static: m
                .get("first_static")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }

    /// Corner radius in pixels for a frame `h` tall.
    pub fn radius_px(&self, h: u32) -> f64 {
        (self.radius * f64::from(h)).round()
    }

    /// Panel rectangles for `k` panels in a `w x h` frame (even sizes, as `render.py`).
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    #[allow(clippy::many_single_char_names)]
    pub fn rects(&self, k: usize, w: u32, h: u32) -> Vec<Rect> {
        let (fw, fh) = (f64::from(w), f64::from(h));
        let gap = (self.gap * fh).round();
        let mar = (self.margin * fh).round();
        let even = |x: f64| (x / 2.0).floor() * 2.0;
        // Sizes along one axis: `avail` split by the weights, the last one taking the rest.
        let split = |avail: f64, k: usize, weights: &[f64]| -> Vec<f64> {
            let wts: Vec<f64> = if weights.len() == k {
                weights.to_vec()
            } else {
                vec![1.0; k]
            };
            let sum: f64 = wts.iter().sum();
            let mut v: Vec<f64> = wts
                .iter()
                .map(|x| even((avail * x / sum).floor()))
                .collect();
            if v.pop().is_some() {
                let rest: f64 = v.iter().sum();
                v.push(even(avail - rest));
            }
            v
        };
        let row = |x0: f64, y0: f64, avail_w: f64, ph: f64, k: usize, wts: &[f64]| {
            let mut x = x0;
            split(avail_w, k, wts)
                .into_iter()
                .map(|pw| {
                    let r = Rect {
                        x,
                        y: y0,
                        w: pw,
                        h: ph,
                    };
                    x += pw + gap;
                    r
                })
                .collect::<Vec<_>>()
        };
        let col = |x0: f64, y0: f64, pw: f64, avail_h: f64, k: usize, wts: &[f64]| {
            let mut y = y0;
            split(avail_h, k, wts)
                .into_iter()
                .map(|ph| {
                    let r = Rect {
                        x: x0,
                        y,
                        w: pw,
                        h: ph,
                    };
                    y += ph + gap;
                    r
                })
                .collect::<Vec<_>>()
        };
        let kf = k.max(1) as f64;
        match self.kind.as_str() {
            "column" => col(
                mar,
                mar,
                even(fw - 2.0 * mar),
                fh - 2.0 * mar - gap * (kf - 1.0),
                k,
                &self.weights,
            ),
            "1+2" if k == 3 => {
                let wts = if self.weights.len() == 2 {
                    self.weights.clone()
                } else {
                    vec![1.0, 1.0]
                };
                let cols = row(
                    mar,
                    mar,
                    fw - 2.0 * mar - gap,
                    even(fh - 2.0 * mar),
                    2,
                    &wts,
                );
                let mut v = vec![cols[0]];
                v.extend(col(cols[1].x, mar, cols[1].w, fh - 2.0 * mar - gap, 2, &[]));
                v
            }
            _ => row(
                mar,
                mar,
                fw - 2.0 * mar - gap * (kf - 1.0),
                even(fh - 2.0 * mar),
                k,
                &self.weights,
            ),
        }
    }

    /// Panel `i`'s slide-in offset in pixels at `t` seconds into the layout: panels enter
    /// `stagger` apart, alternating from below and above (from the right and left in a column),
    /// easing out over `slide` (`1 − (1 − u)³`).
    #[allow(clippy::many_single_char_names)]
    pub fn enter_offset(&self, i: usize, t: f64, w: u32, h: u32) -> [f64; 2] {
        if !self.enter || (i == 0 && self.first_static) {
            return [0.0; 2];
        }
        #[allow(clippy::cast_precision_loss)]
        let t0 = i as f64 * self.stagger - if self.first_static { 0.0 } else { 0.16 };
        let u = ((t - t0) / self.slide.max(1e-6)).clamp(0.0, 1.0);
        let e = 1.0 - (1.0 - u).powi(3);
        let d = if i % 2 == 0 { 1.0 } else { -1.0 };
        if self.kind == "column" {
            [d * f64::from(w) * (1.0 - e), 0.0]
        } else {
            [0.0, d * f64::from(h) * (1.0 - e)]
        }
    }
}

/// `a + b` with `b`'s sign folded in.
fn plus(a: &str, b: f64) -> String {
    if b < 0.0 {
        format!("{a} - {}", num(-b))
    } else {
        format!("{a} + {}", num(b))
    }
}

/// Expands `{"layout": {...}, "panels": [...], "size", "fps", "duration"}` into a comp: the
/// canvas the backdrop colour, each panel a layer at [`Layout::rects`] with the layout's radius,
/// sliding in as [`Layout::enter_offset`] does (`ease_out` is the same cubic).
///
/// # Errors
/// A missing or malformed key.
#[allow(clippy::cast_precision_loss, clippy::many_single_char_names)]
pub fn expand(t: &Map<String, Value>) -> Result<Value> {
    const P: &str = "template";
    for k in t.keys() {
        if !["layout", "panels", "size", "fps", "duration"].contains(&k.as_str()) {
            return Err(err(format!("{P}.{k}"), "unknown key"));
        }
    }
    let empty = Map::new();
    let l = match t.get("layout") {
        Some(Value::Object(m)) => Layout::parse(m),
        None => Layout::parse(&empty),
        Some(v) => {
            return Err(err(
                format!("{P}.layout"),
                format!("expected an object, got {v}"),
            ));
        }
    };
    let size = super::size(t.get("size"), &format!("{P}.size"))?;
    let panels = t
        .get("panels")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| err(format!("{P}.panels"), "expected a list of panels"))?;
    let (w, h) = (size[0], size[1]);
    let rects = l.rects(panels.len(), w, h);
    let mut layers = Vec::new();
    let mut ids = super::Ids::default();
    for (i, (p, r)) in panels.iter().zip(&rects).enumerate() {
        let path = format!("{P}.panels[{i}]");
        let mut m = item_layer(p, &path)?;
        let id = ids.take(m.get("id").and_then(Value::as_str), &format!("p{i}"));
        m.insert("id".into(), id.into());
        m.insert("size".into(), json!([jnum(r.w), jnum(r.h)]));
        let radius = l.radius_px(h);
        if radius > 0.0 {
            m.insert("radius".into(), jnum(radius));
        }
        // `enter_offset` written as an expression over `t`.
        let at = l.enter_offset(i, -1e9, w, h);
        let pos = |base: f64, amp: f64| -> Value {
            if amp == 0.0 {
                return jnum(base);
            }
            let t0 = i as f64 * l.stagger - if l.first_static { 0.0 } else { 0.16 };
            let u = format!("({}) / {}", plus("t", -t0), num(l.slide.max(1e-6)));
            let e = format!("{} * (1 - ease_out({u}))", num(amp));
            if base == 0.0 {
                e.into()
            } else {
                format!("{} + {e}", num(base)).into()
            }
        };
        m.insert(
            "transform".into(),
            json!({"anchor": [0, 0], "position": [pos(r.x, at[0]), pos(r.y, at[1])]}),
        );
        layers.push(Value::Object(m));
    }
    let mut canvas = Map::new();
    canvas.insert("size".into(), json!([w, h]));
    for k in ["fps", "duration"] {
        if let Some(v) = t.get(k) {
            canvas.insert(k.into(), v.clone());
        }
    }
    canvas.insert("background".into(), BACKDROP.into());
    Ok(json!({"canvas": canvas, "layers": layers}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)]
    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn layout_rects_match_render_py() {
        let l = Layout::parse(&obj(json!({"layout": "split"})));
        // render.py: gap round(0.008 * 1080) = 9, avail = 1920 - 9 = 1911, 954 + 956.
        let r = l.rects(2, 1920, 1080);
        assert_eq!((r[0].x, r[0].w, r[0].h), (0.0, 954.0, 1080.0));
        assert_eq!((r[1].x, r[1].w), (963.0, 956.0));
        let r3 = l.rects(3, 1920, 1080);
        assert_eq!(r3.len(), 3);
        assert!(r3[2].x + r3[2].w <= 1920.0);
        let one_two =
            Layout::parse(&obj(json!({"layout": "1+2", "gap": 0.01}))).rects(3, 1920, 1080);
        assert_eq!(one_two[0].h, 1080.0);
        assert_eq!(one_two[1].x, one_two[2].x);
        assert!(one_two[2].y > one_two[1].y);
        let col = Layout::parse(&obj(json!({"layout": "column"}))).rects(2, 1080, 1920);
        assert_eq!(col[0].w, 1080.0);
        assert!(col[1].y > col[0].y);
    }

    #[test]
    fn panels_slide_in() {
        let l = Layout::parse(&obj(json!({})));
        assert_eq!(l.enter_offset(0, 0.0, 1920, 1080), [0.0; 2]);
        let start = l.enter_offset(1, 0.0, 1920, 1080);
        assert_eq!(start, [0.0, -1080.0]);
        assert_eq!(l.enter_offset(1, 0.22 + 0.42, 1920, 1080), [0.0, 0.0]);
        let mid = l.enter_offset(2, 0.44 + 0.21, 1920, 1080)[1];
        assert!(mid > 0.0 && mid < 1080.0 * 0.2);
    }

    #[test]
    fn template_places_panels_at_the_rects() {
        let v = expand(&obj(json!({
            "layout": {"layout": "split"}, "size": [1920, 1080], "fps": 24, "duration": 2,
            "panels": [{"video": "a.mp4"}, {"still": "b.png"}]
        })))
        .expect("expand");
        assert_eq!(v["canvas"]["background"], BACKDROP);
        let l = &v["layers"];
        assert_eq!(l[0]["source"], json!({"video": "a.mp4"}));
        assert_eq!(l[0]["size"], json!([954, 1080]));
        assert_eq!(l[0]["radius"], json!(13));
        assert_eq!(l[0]["transform"]["position"], json!([0, 0]));
        assert_eq!(l[1]["id"], "p1");
        assert_eq!(
            l[1]["transform"]["position"],
            json!([963, "-1080 * (1 - ease_out((t - 0.22) / 0.42))"])
        );
    }
}
