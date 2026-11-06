//! The `story` template (`docs/compositing.md#worked-example-the-story`): a vertical stack of
//! tiles with gutters, each tile a nested comp of its items. The second item of a tile slides in
//! over the first at `swap.at`, tiles `swap.stagger` apart; tiles fade in from black
//! `enter.stagger` apart; stills get a Ken Burns zoom; items may `punch` in and ramp their
//! `speed`; with `exit` the tiles slide out and the canvas fades to black, so a loop starts and
//! ends on black.

use serde_json::{Map, Value, json};

use super::{Ids, item_layer, jnum, num};
use crate::model::err;
use ffcine_core::Result;

const P: &str = "template.story";

fn obj<'a>(m: &'a Map<String, Value>, k: &str) -> Result<Option<&'a Map<String, Value>>> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::Object(o)) => Ok(Some(o)),
        Some(v) => Err(err(
            format!("{P}.{k}"),
            format!("expected an object, got {v}"),
        )),
    }
}

fn number(m: &Map<String, Value>, k: &str, path: &str, default: Option<f64>) -> Result<f64> {
    match m.get(k) {
        None => default.ok_or_else(|| err(format!("{path}.{k}"), "missing")),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && *x >= 0.0)
            .ok_or_else(|| {
                err(
                    format!("{path}.{k}"),
                    format!("{v}: expected a number >= 0"),
                )
            }),
    }
}

fn only(m: &Map<String, Value>, path: &str, known: &[&str]) -> Result<()> {
    match m.keys().find(|k| !known.contains(&k.as_str())) {
        Some(k) => Err(err(
            format!("{path}.{k}"),
            format!("unknown key (expected {})", known.join(", ")),
        )),
        None => Ok(()),
    }
}

/// `"black"`/`"white"` as `[r, g, b]`, anything else as given.
fn rgb(v: &Value) -> Value {
    match v.as_str() {
        Some("black") => json!([0, 0, 0]),
        Some("white") => json!([1, 1, 1]),
        _ => v.clone(),
    }
}

/// A slide's sign and axis: `left` (default), `right`, `up`, `down`.
fn direction(m: &Map<String, Value>, path: &str) -> Result<(&'static str, usize)> {
    Ok(match m.get("direction").and_then(Value::as_str) {
        None | Some("left") => ("-", 0),
        Some("right") => ("", 0),
        Some("up") => ("-", 1),
        Some("down") => ("", 1),
        Some(d) => {
            return Err(err(
                format!("{path}.direction"),
                format!("{d:?}: expected left, right, up or down"),
            ));
        }
    })
}

/// `[x, y]` with `e` on `axis` and 0 on the other.
fn on_axis(axis: usize, e: &str) -> Value {
    if axis == 0 {
        json!([e, 0])
    } else {
        json!([0, e])
    }
}

/// The swap: when, how far apart, how long, which way.
struct Swap {
    at: f64,
    stagger: f64,
    slide: f64,
    sign: &'static str,
    axis: usize,
}

/// The exit: when, how far apart and how long (as var names), which way, the final fade.
struct Exit {
    step: String,
    slide: String,
    sign: &'static str,
    axis: usize,
    fade: Option<f64>,
}

/// Expands the `story` parameters into a comp document (the worked example's shape).
///
/// # Errors
/// A missing or malformed parameter, or a tile with more than two items.
#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::many_single_char_names
)]
pub fn expand(s: &Map<String, Value>) -> Result<Value> {
    only(
        s,
        P,
        &[
            "size",
            "fps",
            "duration",
            "tiles",
            "gutter",
            "radius",
            "background",
            "enter",
            "swap",
            "exit",
            "ken_burns",
            "grade",
            "panels",
        ],
    )?;
    let [w, h] = super::size(s.get("size"), &format!("{P}.size"))?;
    let duration = number(s, "duration", P, None)?;
    let panels = s
        .get("panels")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            err(
                format!("{P}.panels"),
                "expected a list of tiles, each a list of items",
            )
        })?;
    let tiles = panels.len();
    if let Some(n) = s.get("tiles")
        && n.as_u64() != Some(tiles as u64)
    {
        return Err(err(
            format!("{P}.tiles"),
            format!("{n}, but `panels` has {tiles} tiles"),
        ));
    }
    let gutter = number(s, "gutter", P, Some(0.0))?;
    let radius = number(s, "radius", P, Some(0.0))?;
    let th = ((f64::from(h) - (tiles as f64 - 1.0) * gutter) / tiles as f64).floor();
    if th < 1.0 {
        return Err(err(format!("{P}.gutter"), "no room left for the tiles"));
    }

    let mut vars = Map::new();
    let enter = match obj(s, "enter")? {
        Some(e) => {
            let p = format!("{P}.enter");
            only(e, &p, &["fade", "stagger"])?;
            vars.insert("enter".into(), json!(number(e, "stagger", &p, Some(0.0))?));
            Some(number(e, "fade", &p, Some(0.4))?)
        }
        None => None,
    };
    let swap = match obj(s, "swap")? {
        Some(m) => {
            let p = format!("{P}.swap");
            only(m, &p, &["at", "stagger", "slide", "direction"])?;
            let (sign, axis) = direction(m, &p)?;
            let sw = Swap {
                at: number(m, "at", &p, None)?,
                stagger: number(m, "stagger", &p, Some(0.0))?,
                slide: number(m, "slide", &p, Some(0.4))?.max(1e-3),
                sign,
                axis,
            };
            vars.insert("swap".into(), json!(sw.at));
            vars.insert("swap_step".into(), json!(sw.stagger));
            vars.insert("slide".into(), json!(sw.slide));
            Some(sw)
        }
        None => None,
    };
    let exit = match obj(s, "exit")? {
        Some(m) => {
            let p = format!("{P}.exit");
            only(m, &p, &["at", "stagger", "slide", "fade", "direction"])?;
            let at = number(m, "at", &p, None)?;
            let stagger = number(m, "stagger", &p, Some(0.0))?;
            let slide = number(m, "slide", &p, Some(0.4))?.max(1e-3);
            let (sign, axis) = direction(m, &p)?;
            // Share the swap's vars when the values are the same, as one would by hand.
            let mut name = |v: f64, shared: Option<f64>, same: &str, own: &str| {
                if shared == Some(v) {
                    same.to_owned()
                } else {
                    vars.insert(own.into(), json!(v));
                    own.to_owned()
                }
            };
            let step = name(
                stagger,
                swap.as_ref().map(|s| s.stagger),
                "swap_step",
                "exit_step",
            );
            let slide = name(slide, swap.as_ref().map(|s| s.slide), "slide", "exit_slide");
            vars.insert("exit".into(), json!(at));
            let fade = match m.get("fade") {
                None => None,
                Some(_) => Some(number(m, "fade", &p, None)?.max(1e-3)),
            };
            Some(Exit {
                step,
                slide,
                sign,
                axis,
                fade,
            })
        }
        None => None,
    };
    let ken_burns = match s.get("ken_burns") {
        None => None,
        Some(v) => match v
            .as_array()
            .map(|a| a.iter().map(Value::as_f64).collect::<Vec<_>>())
        {
            Some(a) if a.len() == 2 && a.iter().all(Option::is_some) => {
                Some([a[0].unwrap_or(1.0), a[1].unwrap_or(1.0)])
            }
            _ => {
                return Err(err(
                    format!("{P}.ken_burns"),
                    "expected [from_zoom, to_zoom]",
                ));
            }
        },
    };

    // Canvas.
    let mut canvas = Map::new();
    canvas.insert("size".into(), json!([w, h]));
    canvas.insert(
        "fps".into(),
        s.get("fps").cloned().unwrap_or_else(|| "30".into()),
    );
    canvas.insert("duration".into(), json!(duration));
    let background = match s.get("background") {
        None => json!("black"),
        Some(Value::Object(b)) => {
            let p = format!("{P}.background");
            only(b, &p, &["from", "to", "duration"])?;
            let from = b.get("from").map_or_else(|| json!([0, 0, 0]), rgb);
            let to = b.get("to").map_or_else(|| from.clone(), rgb);
            let d = number(b, "duration", &p, Some(duration))?;
            json!([{"t": 0, "v": from, "interp": "in_out"}, {"t": d, "v": to}])
        }
        Some(v) => v.clone(),
    };
    canvas.insert("background".into(), background);
    if let Some(f) = exit.as_ref().and_then(|e| e.fade) {
        canvas.insert(
            "fade".into(),
            format!("1 - clip((t - {}) / {}, 0, 1)", num(duration - f), num(f)).into(),
        );
        canvas.insert("fade_colour".into(), "black".into());
    }

    // Tiles.
    let mut ids = Ids::default();
    let mut comps = Map::new();
    let mut layers = Vec::new();
    for (i, tile) in panels.iter().enumerate() {
        let tp = format!("{P}.panels[{i}]");
        let items = tile
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| err(&tp, "expected a list of items"))?;
        if items.len() > 2 {
            return Err(err(
                &tp,
                format!("{} items: a tile takes one or two (one swap)", items.len()),
            ));
        }
        // The tile's swap time: `swap` itself on the first tile, else a tile var `s`.
        let mut tile_vars = Map::new();
        let at = match i {
            0 => "swap".to_owned(),
            1 => {
                tile_vars.insert("s".into(), "swap + swap_step".into());
                "s".to_owned()
            }
            _ => {
                tile_vars.insert("s".into(), format!("swap + {i} * swap_step").into());
                "s".to_owned()
            }
        };
        if items.len() < 2 || swap.is_none() {
            tile_vars.clear();
        }
        if items.len() == 2 && swap.is_none() {
            return Err(err(&tp, "two items need a `swap`"));
        }
        let mut tl = Vec::new();
        for (k, it) in items.iter().enumerate() {
            let ip = format!("{tp}[{k}]");
            let m = it
                .as_object()
                .ok_or_else(|| err(&ip, format!("expected an object, got {it}")))?;
            only(
                m,
                &ip,
                &[
                    "video",
                    "still",
                    "source_in",
                    "punch",
                    "speed",
                    "id",
                    "grade",
                ],
            )?;
            let path = m
                .get("video")
                .or_else(|| m.get("still"))
                .and_then(Value::as_str)
                .ok_or_else(|| err(&ip, "expected `video` or `still` (a path)"))?;
            let still = m.contains_key("still");
            let stem = std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let fallback = format!("t{i}_{k}");
            let stem = if stem.is_empty() {
                fallback.as_str()
            } else {
                stem.as_str()
            };
            let mut l = Map::new();
            l.insert(
                "id".into(),
                ids.take(m.get("id").and_then(Value::as_str), stem).into(),
            );
            let base = item_layer(&json!({ if still { "still" } else { "video" }: path }), &ip)?;
            l.extend(base);
            if let Some(v) = m.get("source_in") {
                if still {
                    return Err(err(format!("{ip}.source_in"), "a still has no source time"));
                }
                l.insert("source_in".into(), v.clone());
            }
            if swap.is_some() && items.len() == 2 {
                if k == 0 {
                    l.insert("out".into(), format!("{at} + slide").into());
                } else {
                    l.insert("in".into(), at.clone().into());
                }
            }
            if let Some(g) = m.get("grade").or_else(|| s.get("grade")) {
                l.insert("grade".into(), g.clone());
            }
            // Content zoom: the punch-in keys, the Ken Burns ramp on stills, or both.
            let punch = match m.get("punch") {
                Some(Value::Object(p)) => {
                    let pp = format!("{ip}.punch");
                    only(p, &pp, &["at", "zoom", "duration"])?;
                    let t0 = number(p, "at", &pp, None)?;
                    let z = number(p, "zoom", &pp, None)?;
                    let d = number(p, "duration", &pp, Some(0.15))?.max(1e-3);
                    Some(json!([{"t": t0, "v": 1.0, "interp": "out"}, {"t": t0 + d, "v": z}]))
                }
                Some(v) => {
                    return Err(err(
                        format!("{ip}.punch"),
                        format!("expected an object, got {v}"),
                    ));
                }
                None => None,
            };
            let kb = ken_burns
                .filter(|_| still)
                .map(|[a, b]| format!("lerp({}, {}, lt / dur)", num(a), num(b)));
            let zoom = match (punch, kb) {
                (Some(p), Some(e)) => Some(json!({"keys": p, "expr": format!("value * {e}")})),
                (Some(p), None) => Some(p),
                (None, Some(e)) => Some(e.into()),
                (None, None) => None,
            };
            if let Some(z) = zoom {
                l.insert("content".into(), json!({ "zoom": z }));
            }
            if let Some(v) = m.get("speed") {
                if still {
                    return Err(err(format!("{ip}.speed"), "a still has no speed"));
                }
                let pts = v
                    .as_array()
                    .filter(|a| !a.is_empty())
                    .ok_or_else(|| err(format!("{ip}.speed"), "expected [[t, speed], ...]"))?;
                let n = pts.len();
                let mut keys = Vec::new();
                for (j, pt) in pts.iter().enumerate() {
                    let pair = pt
                        .as_array()
                        .filter(|a| a.len() == 2 && a.iter().all(Value::is_number))
                        .ok_or_else(|| err(format!("{ip}.speed[{j}]"), "expected [t, speed]"))?;
                    let mut key = json!({"t": pair[0], "v": pair[1]});
                    if j + 1 < n {
                        key["interp"] = "in_out".into();
                    }
                    keys.push(key);
                }
                l.insert("time".into(), json!({ "speed": keys }));
            }
            let position = match (swap.as_ref(), items.len(), k) {
                (Some(sw), 2, 0) => on_axis(
                    sw.axis,
                    &format!(
                        "{}{} * ease_out((t - {at}) / slide)",
                        sw.sign,
                        ["w", "h"][sw.axis]
                    ),
                ),
                (Some(sw), 2, _) => on_axis(
                    sw.axis,
                    &format!(
                        "{}{} * (1 - ease_out(lt / slide))",
                        if sw.sign == "-" { "" } else { "-" },
                        ["w", "h"][sw.axis]
                    ),
                ),
                _ => json!([0, 0]),
            };
            l.insert(
                "transform".into(),
                json!({"anchor": [0, 0], "position": position}),
            );
            tl.push(Value::Object(l));
        }
        let mut c = Map::new();
        c.insert(
            "canvas".into(),
            json!({"size": [w, jnum(th)], "background": "black"}),
        );
        if !tile_vars.is_empty() {
            c.insert("vars".into(), Value::Object(tile_vars));
        }
        c.insert("layers".into(), Value::Array(tl));
        let name = format!("tile{i}");
        comps.insert(name.clone(), Value::Object(c));

        let y = (i as f64) * (th + gutter);
        let mut t = Map::new();
        t.insert("id".into(), format!("t{i}").into());
        t.insert("source".into(), json!({ "comp": name }));
        t.insert("size".into(), json!([w, jnum(th)]));
        if radius > 0.0 {
            t.insert("radius".into(), jnum(radius));
        }
        t.insert("vars".into(), json!({ "i": i }));
        if let Some(f) = enter {
            t.insert(
                "fade".into(),
                format!("ease_out((t - i * enter) / {})", num(f)).into(),
            );
            t.insert("fade_colour".into(), "black".into());
        }
        let x = match &exit {
            Some(e) => {
                let u = format!("(t - exit - i * {}) / {}", e.step, e.slide);
                let travel = format!("{}{} * ease_in({u})", e.sign, ["W", "H"][e.axis]);
                json!(format!("if(lt(t, exit + i * {}), 0, {travel})", e.step))
            }
            None => json!(0),
        };
        let pos = match exit.as_ref().map_or(0, |e| e.axis) {
            0 => json!([x, jnum(y)]),
            _ if exit.is_some() => {
                return Err(err(
                    format!("{P}.exit.direction"),
                    "tiles are stacked vertically: exit left or right",
                ));
            }
            _ => json!([0, jnum(y)]),
        };
        t.insert(
            "transform".into(),
            json!({"anchor": [0, 0], "position": pos}),
        );
        layers.push(Value::Object(t));
    }

    let mut doc = Map::new();
    doc.insert("canvas".into(), Value::Object(canvas));
    if !vars.is_empty() {
        doc.insert("vars".into(), Value::Object(vars));
    }
    doc.insert("comps".into(), Value::Object(comps));
    doc.insert("layers".into(), Value::Array(layers));
    Ok(Value::Object(doc))
}

/// Still-image extensions [`starter`] files under `still` (anything else is a `video`).
pub const STILLS: &[&str] = &[
    "png", "jpg", "jpeg", "heic", "heif", "avif", "webp", "tif", "tiff", "exr",
];

/// Seconds rounded to the millisecond, at least 0 (so 6 * 0.4 writes as 2.4).
fn ms(s: f64) -> f64 {
    (s.max(0.0) * 1000.0).round() / 1000.0
}

/// A story comp document to start from (`ffcine comp new --template story`): the worked
/// example's parameters with `media` dealt round `tiles` tiles, the first round first, so a
/// tile's second item swaps in. Paths are written as given.
///
/// # Errors
/// No media, no tiles, or more than two items a tile.
pub fn starter(media: &[String], tiles: usize, duration: f64) -> Result<Value> {
    let tiles = tiles.min(media.len());
    if tiles == 0 {
        return Err(err(
            "media",
            "a story needs at least one picture and one tile",
        ));
    }
    if media.len() > 2 * tiles {
        return Err(err(
            "media",
            format!(
                "{} files for {tiles} tiles: at most two a tile",
                media.len()
            ),
        ));
    }
    let item = |p: &String| {
        let still = std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| STILLS.contains(&e.to_ascii_lowercase().as_str()));
        json!({ if still { "still" } else { "video" }: p })
    };
    let panels: Vec<Value> = (0..tiles)
        .map(|i| Value::Array(media.iter().skip(i).step_by(tiles).map(item).collect()))
        .collect();
    let swaps = media.len() > tiles;
    let mut s = json!({
        "size": [1080, 1920], "fps": "30", "duration": duration,
        "tiles": tiles, "gutter": 12, "radius": 24,
        "background": "black",
        "enter": { "fade": 0.4, "stagger": 0.25 },
        "exit": { "at": ms(duration - 0.8), "stagger": 0.12, "slide": 0.4, "fade": 0.4 },
        "ken_burns": [1.0, 1.06],
        "grade": { "camera": "auto" },
        "panels": panels,
    });
    if swaps {
        s["swap"] = json!({ "at": ms(duration * 0.4), "stagger": 0.12, "slide": 0.4,
                            "direction": "left" });
    }
    Ok(json!({ "schema": format!("{}/1", crate::SCHEMA), "template": { "story": s } }))
}
