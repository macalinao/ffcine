use std::fmt::Write as _;
use std::path::Path;

use ffcine_core::Result;
use ffcine_render::{BlendMode, Crop, LayerSource, Size};
use serde_json::{Value, json};

use crate::{Base, Comp, Media};

/// Every video is 1920x1080; identities are their paths.
struct Fake;

impl Media for Fake {
    fn dims(&self, _: &Path) -> Result<(u32, u32)> {
        Ok((1920, 1080))
    }
    fn identity(&self, path: &Path) -> Value {
        json!(path)
    }
    fn video_rate(&self, _: &Path) -> Result<(f64, u64)> {
        Ok((30.0, 300))
    }
}

const OUT: Size = Size {
    width: 1080,
    height: 1920,
};

fn comp(layers: Value) -> Comp {
    try_comp(layers).expect("comp")
}

#[allow(clippy::needless_pass_by_value)] // json! literals at every call
fn try_comp(layers: Value) -> Result<Comp> {
    Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [1080, 1920], "fps": "30", "duration": 6.0, "background": "black" },
            "vars": { "slide": 0.4 },
            "layers": layers,
        }),
        Path::new("/media"),
    )
}

fn keys(c: &Comp) -> Vec<Value> {
    c.pieces()
        .into_iter()
        .map(|r| c.lower(r, OUT, &Fake).expect("lower").recipe)
        .collect()
}

#[test]
fn reads_and_rejects() {
    let c = comp(json!([{ "id": "a", "source": { "video": "a.mov" }, "x_note": "kept" }]));
    assert_eq!(c.layers.len(), 1);
    assert_eq!(c.frames(), 180);
    for (layers, want) in [
        (
            json!([{ "source": { "video": "a.mov" }, "opactiy": 1 }]),
            "layers[0].opactiy: unknown key",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "filter": {} }]),
            "compositing step 6",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "grade": { "denoise": 1 } }]),
            "layers[0].grade.denoise: not supported yet (per-layer denoise",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "time": { "speed": 1000 } }]),
            "layers[0].time.speed: 1000: at most 100",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "time": { "speed": 2, "freeze": 1 } }]),
            "speed and freeze given: use one",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "time": { "freeze": 1, "reverse": true } }]),
            "reverse goes with speed",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "time": { "freeze": -1 } }]),
            "layers[0].time.freeze: expected source seconds",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "time": { "reverse": 1 } }]),
            "layers[0].time.reverse: expected true or false",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "time": { "sped": 2 } }]),
            "layers[0].time.sped: unknown key",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "time": {} }]),
            "expected one of speed",
        ),
        (
            json!([{ "source": { "solid": "white" }, "time": { "speed": 2 } }]),
            "only video layers have a source time",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "in": 2, "out": 1 }]),
            "must be after",
        ),
        (
            json!([{ "source": { "solid": "white" }, "blend": "nope" }]),
            "unknown blend mode",
        ),
        (
            json!([{ "source": { "solid": "white" }, "transform": { "opacity": "1 +" } }]),
            "layers[0].transform.opacity: col 4",
        ),
    ] {
        let e = try_comp(layers).expect_err(want).to_string();
        assert!(e.contains(want), "{want}: {e}");
    }
    let v2 = json!({"schema": "ffcine.comp/2", "canvas": {}});
    assert!(
        Comp::from_value(&v2, Path::new("."))
            .unwrap_err()
            .to_string()
            .contains("unsupported version")
    );
    // `in`/`out` take expressions over the vars, evaluated once.
    let c = comp(json!([{ "source": { "solid": "white" }, "in": "slide * 5", "out": "T - 1" }]));
    assert!((c.layers[0].in_ - 2.0).abs() < 1e-9 && (c.layers[0].out - 5.0).abs() < 1e-9);
}

#[test]
fn tiles_land_where_the_transform_says() {
    let c = comp(json!([
        { "source": { "solid": [1, 0, 0] }, "size": [1080, 632],
          "transform": { "anchor": [0, 0], "position": [0, 644] }, "radius": 24 },
        { "source": { "solid": "#00ff0080" }, "size": [200, 100],
          "transform": { "position": [540, 960], "rotation": 90, "scale": 2 },
          "blend": "multiply" },
    ]));
    let p = c.lower(0..30, OUT, &Fake).expect("lower");
    assert_eq!(p.base, Base::Black);
    assert_eq!(p.layers.len(), 2);
    let a = &p.layers[0];
    assert_eq!(
        a.rects[0],
        Crop {
            x: 0.0,
            y: 644.0,
            width: 1080.0,
            height: 632.0
        }
    );
    assert_eq!(a.rotation, [] as [f32; 0]);
    let alpha = a
        .grade
        .hooks
        .iter()
        .find(|h| h.name == "alpha")
        .expect("alpha");
    assert!(alpha.params.contains(&("radius".into(), 24.0)));
    // Solid colours go through the solid hook.
    let solid = &a.grade.hooks[0];
    assert_eq!(solid.name, "solid");
    assert!(solid.params.contains(&("solid_r".into(), 1.0)));
    // Scaled about its middle: twice the box, centred; the colour's alpha is the opacity.
    let b = &p.layers[1];
    assert_eq!(
        b.rects[0],
        Crop {
            x: 340.0,
            y: 860.0,
            width: 400.0,
            height: 200.0
        }
    );
    assert_eq!(
        b.grade.size,
        Some(Size {
            width: 400,
            height: 200
        })
    );
    assert!((b.rotation[0] - 90.0).abs() < 1e-6);
    assert_eq!(b.blend, BlendMode::Multiply);
    let alpha = b
        .grade
        .hooks
        .iter()
        .find(|h| h.name == "alpha")
        .expect("alpha");
    let op = alpha
        .params
        .iter()
        .find(|(n, _)| n == "opacity")
        .expect("opacity")
        .1;
    assert!((op - 128.0 / 255.0).abs() < 1e-6);
    // The background is the base's hook; a render at twice the size scales every length.
    assert_eq!(p.grade.hooks[0].name, "background");
    let big = c
        .lower(
            0..30,
            Size {
                width: 2160,
                height: 3840,
            },
            &Fake,
        )
        .expect("4k");
    assert_eq!(
        big.layers[0].rects[0],
        Crop {
            x: 0.0,
            y: 1288.0,
            width: 2160.0,
            height: 1264.0
        }
    );
}

#[test]
fn anchor_and_rotation_turn_about_the_anchor() {
    // A 100x100 box anchored at its top left, at (500, 500), turned 90° counter-clockwise: its
    // middle (50, 50) from the anchor lands at (550, 450).
    let c = comp(json!([{ "source": { "solid": "white" }, "size": [100, 100],
        "transform": { "anchor": [0, 0], "position": [500, 500], "rotation": 90 } }]));
    let r = c.lower(0..1, OUT, &Fake).expect("lower").layers[0].rects[0];
    assert!(
        (r.x - 500.0).abs() < 1e-3 && (r.y - 400.0).abs() < 1e-3,
        "{r:?}"
    );
}

#[test]
fn base_promotion_on_and_off() {
    let video = json!({ "source": { "video": "a.mov" } });
    let p = comp(json!([video, { "source": { "solid": "white" }, "size": [10, 10] }]))
        .lower(0..30, OUT, &Fake)
        .expect("lower");
    let Base::Video { input, start } = &p.base else {
        panic!("not promoted: {:?}", p.base)
    };
    assert_eq!(input, Path::new("/media/a.mov"));
    assert!(start.is_zero());
    assert_eq!(p.layers.len(), 1);
    assert_eq!(p.shown, vec![1]);
    // Cover: the 16:9 source cropped to 9:16 in the middle.
    let crop = p.grade.crop.expect("cover crop");
    assert!((crop.width - 607.5).abs() < 1e-3 && (crop.height - 1080.0).abs() < 1e-3);
    // Not promoted: translucent, rounded, turned, or not there for the whole piece.
    for tweak in [
        json!({ "transform": { "opacity": 0.5 } }),
        json!({ "radius": 10 }),
        json!({ "transform": { "rotation": 5 } }),
        json!({ "in": 0.5 }),
        json!({ "fit": "contain" }),
        json!({ "blend": "screen" }),
    ] {
        let mut l = video.clone();
        for (k, v) in tweak.as_object().expect("obj") {
            l[k] = v.clone();
        }
        let p = comp(json!([l])).lower(0..30, OUT, &Fake).expect("lower");
        assert_eq!(p.base, Base::Black, "{tweak}");
        assert!(
            matches!(p.layers[0].source, LayerSource::Video { .. }),
            "{tweak}"
        );
    }
    // A layer starting later reads its source from `source_in` at its own first frame.
    let p = comp(json!([{ "source": { "video": "a.mov" }, "in": 1.0, "source_in": 12.0 }]))
        .lower(30..60, OUT, &Fake)
        .expect("lower");
    let Base::Video { start, .. } = p.base else {
        panic!("promoted")
    };
    assert!((start.as_secs_f64() - 12.0).abs() < 1e-9);
}

#[test]
fn contain_fits_the_picture_inside_the_box() {
    let c = comp(json!([{ "source": { "video": "a.mov" }, "fit": "contain" }]));
    let l = &c.lower(0..1, OUT, &Fake).expect("lower").layers[0];
    let r = l.rects[0];
    assert!(
        (r.width - 1080.0).abs() < 1e-3 && (r.height - 607.5).abs() < 1e-3,
        "{r:?}"
    );
    assert!((r.y - (1920.0 - 607.5) / 2.0).abs() < 1e-3);
    assert_eq!(l.grade.crop, None);
}

#[test]
fn pieces_cut_at_in_and_out_never_under_a_second() {
    let c = comp(json!([
        { "source": { "solid": "white" }, "in": 2.0, "out": 4.0 },
        { "source": { "solid": "white" }, "in": 2.5 },
        { "source": { "solid": "white" }, "out": 5.6 },
    ]));
    assert_eq!(c.pieces(), vec![0..60, 60..120, 120..180]);
}

#[test]
fn equivalent_rewrites_keep_keys() {
    let keyed = comp(json!([{ "source": { "solid": "white" },
        "transform": { "opacity": [{ "t": 0, "v": 0 }, { "t": 2, "v": 1 }] } }]));
    let expr = comp(json!([{ "source": { "solid": "white" },
        "transform": { "opacity": "clip(lt / 2, 0, 1)" } }]));
    assert_eq!(keys(&keyed), keys(&expr));
    // Shared vars and constants written as expressions too.
    let a = comp(json!([{ "source": { "solid": "white" }, "radius": 12 }]));
    let b = comp(
        json!([{ "source": { "solid": "white" }, "radius": "slide * 30", "vars": {"unused": 1} }]),
    );
    assert_eq!(keys(&a), keys(&b));
}

#[test]
fn an_edit_after_a_cut_rerenders_only_later_pieces() {
    let layers = |fade: &str| {
        json!([
            { "source": { "solid": "white" }, "out": 2.0 },
            { "source": { "solid": "#ff0000" }, "in": 2.0, "out": 4.0 },
            { "source": { "solid": "white" }, "in": 4.0, "fade": fade },
        ])
    };
    let a = keys(&comp(layers("1")));
    let b = keys(&comp(layers("clip(1 - (t - 5), 0, 1)")));
    assert_eq!(a.len(), 3);
    assert_eq!(a[..2], b[..2]);
    assert_ne!(a[2], b[2]);
}

#[test]
fn reorder_changes_keys() {
    let a = json!({ "source": { "solid": "white" }, "size": [100, 100] });
    let b = json!({ "source": { "solid": "#ff0000" }, "size": [200, 200] });
    assert_ne!(keys(&comp(json!([a, b]))), keys(&comp(json!([b, a]))));
    // Ids aren't keyed.
    let named = json!({ "id": "x", "source": { "solid": "white" }, "size": [100, 100] });
    assert_eq!(keys(&comp(json!([a]))), keys(&comp(json!([named]))));
}

#[test]
fn fades_and_masks_become_hooks() {
    let c = comp(json!([{ "source": { "solid": "white" }, "size": [100, 50],
        "fade": "clip(t, 0, 1)", "fade_colour": [1, 0, 0],
        "mask": { "shape": "ellipse", "rect": [0, 0, 50, 50], "feather": 4, "invert": true } }]));
    let p = c.lower(0..30, OUT, &Fake).expect("lower");
    let names: Vec<&str> = p.layers[0]
        .grade
        .hooks
        .iter()
        .map(|h| h.name.as_str())
        .collect();
    assert_eq!(names, ["solid", "fade", "alpha"]);
    let fade = &p.layers[0].grade.hooks[1];
    assert_eq!(fade.tracks[0].name, "fade");
    assert_eq!(fade.tracks[0].values.len(), 30);
    assert!(fade.params.contains(&("fade_r".into(), 1.0)));
    let alpha = &p.layers[0].grade.hooks[2];
    for (n, v) in [
        ("shape", 2.0),
        ("mask_w", 0.5),
        ("feather", 4.0),
        ("invert", 1.0),
    ] {
        assert!(
            alpha.params.contains(&(n.into(), v)),
            "{n}: {:?}",
            alpha.params
        );
    }
    // The canvas fade is one top-most normal solid of the fade colour at opacity 1 - fade,
    // whatever the layers' blend modes; the background and the layers don't fade.
    let c = Comp::from_value(
        &json!({ "schema": "ffcine.comp/1",
            "canvas": { "size": [1080, 1920], "fps": 30, "duration": 2,
                "fade": "1 - clip(t - 1, 0, 1)", "fade_colour": "white" },
            "layers": [{ "source": { "solid": "#808080" }, "blend": "difference" }] }),
        Path::new("."),
    )
    .expect("comp");
    let p = c.lower(0..60, OUT, &Fake).expect("lower");
    assert_eq!(p.grade.hooks.len(), 1);
    assert_eq!(p.layers.len(), 2);
    assert_eq!(p.shown, vec![0]);
    assert!(
        p.layers[0]
            .grade
            .hooks
            .iter()
            .all(|h| h.name != "canvas_fade")
    );
    let top = &p.layers[1];
    assert_eq!(top.blend, BlendMode::Normal);
    assert!(top.rects.is_empty() && (top.from, top.to) == (0, 60));
    assert!(top.grade.hooks[0].params.contains(&("solid_r".into(), 1.0)));
    let op = &top.grade.hooks[1].tracks[0];
    assert_eq!(op.name, "opacity");
    assert!(op.values[0] == 0.0 && op.values[59] > 0.9);
    // No fade, no extra layer.
    let c = comp(json!([{ "source": { "solid": "white" } }]));
    assert_eq!(c.lower(0..1, OUT, &Fake).expect("lower").layers.len(), 1);
}

#[test]
fn uneven_scale_keeps_corners_round() {
    let c = comp(
        json!([{ "source": { "solid": "white" }, "size": [100, 100], "radius": 10,
        "transform": { "scale": [2, 1] } }]),
    );
    let p = c.lower(0..1, OUT, &Fake).expect("lower");
    let alpha = p.layers[0]
        .grade
        .hooks
        .iter()
        .find(|h| h.name == "alpha")
        .expect("alpha");
    // 2 texture px per box px across, 1 down: the radius is 20 px across, 10 down.
    assert!(alpha.params.contains(&("radius".into(), 20.0)));
    assert!(alpha.params.contains(&("ky".into(), 0.5)));
    // Comp files are known by name, so a broken one reports a comp error.
    assert!(crate::is_comp_file(Path::new("/nonexistent/a.comp.yaml")));
    assert!(!crate::is_comp_file(Path::new("/nonexistent/a.otio")));
    let big =
        json!({"schema": "ffcine.comp/1", "canvas": {"size": [4e9, 1], "fps": 30, "duration": 1}});
    assert!(Comp::from_value(&big, Path::new(".")).is_err());
}

/// The error message of a comp with `canvas` and `layers` that should fail to read or lower.
#[allow(clippy::needless_pass_by_value)] // json! literals at every call
fn rejected(canvas: Value, layers: Value) -> String {
    let c = Comp::from_value(
        &json!({ "schema": "ffcine.comp/1", "canvas": canvas, "layers": layers }),
        Path::new("/media"),
    );
    let e = match c {
        Err(e) => e,
        Ok(c) => match c
            .pieces()
            .into_iter()
            .try_for_each(|r| c.lower(r, OUT, &Fake).map(drop))
        {
            Err(e) => e,
            Ok(()) => panic!("accepted: {canvas} {layers}"),
        },
    };
    e.to_string()
}

#[test]
#[allow(clippy::too_many_lines)] // one case per limit
fn limits_are_checked_before_rendering() {
    let canvas = json!({ "size": [1080, 1920], "fps": 30, "duration": 2 });
    let solid = |extra: Value| {
        let mut l = json!({ "source": { "solid": "white" }, "size": [100, 100] });
        l.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        l
    };
    let bad = |canvas: Value, layers: Value, want: &str| {
        let e = rejected(canvas, layers);
        assert!(e.contains(want), "{want:?} not in {e:?}");
    };
    // Layer textures, boxes, positions and rotations.
    bad(
        canvas.clone(),
        json!([solid(json!({ "transform": { "scale": 1e6 } }))]),
        "layers[0].transform.scale",
    );
    bad(
        canvas.clone(),
        json!([solid(json!({ "size": [1e300, 100] }))]),
        "layers[0].size",
    );
    bad(
        canvas.clone(),
        json!([solid(json!({ "transform": { "rotation": 1e300 } }))]),
        "layers[0].transform.rotation",
    );
    bad(
        canvas.clone(),
        json!([solid(json!({ "transform": { "position": [1e300, 0] } }))]),
        "layers[0].transform.position",
    );
    bad(
        canvas.clone(),
        json!([solid(
            json!({ "mask": { "shape": "rect", "rect": [0, 0, 1e300, 1e300] } })
        )]),
        "layers[0].mask.rect",
    );
    bad(
        canvas.clone(),
        json!([solid(json!({ "radius": 1e300 }))]),
        "layers[0].radius",
    );
    // The canvas: pixels, aspect, duration and rate.
    for size in [
        json!([32768, 1]),
        json!([16384, 16384]),
        json!([16385, 8000]),
    ] {
        bad(
            json!({ "size": size, "fps": 30, "duration": 1 }),
            json!([]),
            "canvas.size",
        );
    }
    bad(
        json!({ "size": [100, 100], "fps": 30, "duration": 1e7 }),
        json!([]),
        "canvas.duration",
    );
    for fps in [json!(1e9), json!(1e-9), json!("1/0")] {
        bad(
            json!({ "size": [100, 100], "fps": fps, "duration": 1 }),
            json!([]),
            "canvas.fps",
        );
    }
    // Layer counts.
    bad(
        canvas.clone(),
        Value::Array(vec![solid(json!({})); 65]),
        "layers: 65 layers",
    );
    bad(
        canvas.clone(),
        Value::Array(vec![json!({ "source": { "video": "a.mp4" } }); 17]),
        "layers[16].source",
    );
    // Ids and in points.
    bad(
        canvas.clone(),
        json!([solid(json!({ "id": "a" })), solid(json!({ "id": "a" }))]),
        "layers[1].id",
    );
    bad(
        canvas.clone(),
        json!([solid(json!({ "id": 3 }))]),
        "layers[0].id",
    );
    bad(
        canvas.clone(),
        json!([solid(json!({ "in": -1 }))]),
        "layers[0].in",
    );
    // Later steps' keys name the step; framing needs a picture.
    for (k, want) in [
        ("grade", "only video, still and comp"),
        ("time", "only video layers"),
        ("content", "only video, still and comp"),
        ("crop", "only video, still and comp"),
    ] {
        bad(canvas.clone(), json!([solid(json!({ k: {} }))]), want);
    }
    // At the limits: accepted.
    let ok = Comp::from_value(
        &json!({ "schema": "ffcine.comp/1",
            "canvas": { "size": [8192, 8192], "fps": 240, "duration": 1 },
            "layers": vec![json!({ "source": { "solid": "white" }, "size": [100, 100],
                "transform": { "rotation": 720, "scale": 10 } }); 64] }),
        Path::new("/media"),
    )
    .expect("comp at the limits");
    for r in ok.pieces() {
        ok.lower(r, OUT, &Fake).expect("lower");
    }
}

/// Piece 0's lowering of `layers`.
fn lowered(layers: Value) -> crate::Piece {
    let c = comp(layers);
    c.lower(c.pieces()[0].clone(), OUT, &Fake).expect("lower")
}

fn crop_of(l: &ffcine_render::Layer) -> Crop {
    l.grade.crop.expect("crop")
}

#[test]
fn content_moves_the_source_window() {
    let video = |extra: Value| {
        let mut l =
            json!({ "source": { "video": "a.mov" }, "size": [540, 960], "blend": "multiply" });
        l.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        json!([l])
    };
    // Cover: a 9:16 window of the 16:9 source, centred.
    let c = crop_of(&lowered(video(json!({ "content": {} }))).layers[0]);
    assert!((c.width - 607.5).abs() < 0.01 && (c.height - 1080.0).abs() < 0.01);
    assert!((c.x - 656.25).abs() < 0.01 && c.y.abs() < 0.01);
    // crop_x 0 puts it at the left edge; zoom 2 halves it about zoom_center.
    let c = crop_of(&lowered(video(json!({ "content": { "crop_x": 0 } }))).layers[0]);
    assert!(c.x.abs() < 0.01);
    let c = crop_of(&lowered(video(json!({ "content": { "zoom": 2 } }))).layers[0]);
    assert!((c.width - 303.75).abs() < 0.01 && (c.height - 540.0).abs() < 0.01);
    assert!((c.x + c.width / 2.0 - 960.0).abs() < 0.01 && (c.y - 270.0).abs() < 0.01);
    // A crop takes sides off the source first: the window stays inside what is left.
    let c = crop_of(
        &lowered(video(
            json!({ "crop": { "left": 0.5 }, "content": { "crop_x": 0 } }),
        ))
        .layers[0],
    );
    assert!((c.x - 960.0).abs() < 0.01, "{c:?}");
    // A punch-in zooms its frames only: the crop goes per frame.
    let p = lowered(video(
        json!({ "content": { "punch": [{ "at": 1, "to": 2 }] } }),
    ));
    let crops = &p.layers[0].grade.crops;
    assert_eq!(crops.len(), 180);
    assert!((crops[0].width - 607.5).abs() < 0.01);
    assert!((crops[45].width - 607.5 / 1.25).abs() < 0.01);
    assert!((crops[90].width - 607.5).abs() < 0.01);
    // A push zooms smoothly from one zoom to the other.
    let p = lowered(video(json!({ "content": { "push": [1, 2] } })));
    let w: Vec<f32> = p.layers[0].grade.crops.iter().map(|c| c.width).collect();
    assert!((w[0] - 607.5).abs() < 0.01 && (w[179] - 303.75).abs() < 0.01);
    assert!(w.windows(2).all(|p| p[1] <= p[0]));
    // A crop feather is a feathered rect mask.
    let p = lowered(video(json!({ "crop": { "top": 0.1, "feather": 8 } })));
    assert!(p.layers[0].grade.hooks.iter().any(|h| {
        h.params
            .iter()
            .any(|(n, v)| n == "shape" && (*v - 1.0).abs() < 1e-9)
    }));
    // Rejected: bad sides, animated crops with contain, framing a solid, unknown keys.
    for (l, want) in [
        (
            video(json!({ "crop": { "left": 0.6, "right": 0.5 } })),
            "under 1",
        ),
        (
            video(json!({ "crop": { "left": "t / 10" }, "fit": "contain" })),
            "contain",
        ),
        (video(json!({ "content": { "zom": 2 } })), "unknown key"),
        (
            video(json!({ "content": { "punch": [{ "at": 1, "to": 2, "zoom": 0 }] } })),
            "> 0",
        ),
        (
            video(json!({ "crop": { "feather": 4 }, "mask": { "shape": "ellipse" } })),
            "mask",
        ),
        // Zooms are capped, alone and with crops.
        (
            video(json!({ "content": { "zoom": 20000 } })),
            "at most 10000",
        ),
        (
            video(json!({ "content": { "punch": [{ "at": 0, "zoom": 2e4 }] } })),
            "at most 10000",
        ),
        (
            video(json!({ "content": { "push": [1, 2e4] } })),
            "at most 10000",
        ),
        (
            video(json!({ "content": { "zoom": 5000, "push": [1, 5000] } })),
            "go in over",
        ),
        (
            video(json!({ "crop": { "left": 0.49999, "right": 0.5 } })),
            "go in over",
        ),
    ] {
        let e = rejected(json!({ "size": [1080, 1920], "fps": 30, "duration": 6 }), l);
        assert!(e.contains(want), "{want:?} not in {e:?}");
    }
}

#[test]
#[allow(clippy::many_single_char_names)]
fn stills_text_and_nested_comps_lower() {
    // A still: its identity in the key.
    let still = |p: &str| json!([{ "source": { "still": p } , "size": [540, 540] }]);
    let a = lowered(still("a.png"));
    assert!(matches!(&a.layers[0].source, LayerSource::Still(p) if p.ends_with("a.png")));
    assert_ne!(a.recipe, lowered(still("b.png")).recipe);
    // Text lies under the next layer, or above all; its params are in the key.
    let text = |s: &str| json!({ "source": { "text": { "text": s, "size": 80 } } });
    let solid = json!({ "source": { "solid": "white" }, "size": [100, 100] });
    let p = lowered(json!([text("hi"), solid]));
    assert_eq!(p.layers.len(), 1);
    assert_ne!(p.overlays.len(), 0);
    assert!(p.overlays.iter().all(|o| o.under == Some(0)));
    let q = lowered(json!([solid, text("hi")]));
    assert!(q.overlays.iter().all(|o| o.under == Some(1)));
    assert_ne!(p.recipe, q.recipe);
    assert_ne!(q.recipe, lowered(json!([solid, text("ho")])).recipe);
    // A nested comp: its own layers in a scene, its values in the key.
    let nested = |colour: &str| {
        json!([{ "source": { "comp": { "layers": [
            { "source": { "solid": colour }, "size": [200, 200] },
            { "source": { "video": "a.mov" } },
        ] } }, "size": [540, 960] }])
    };
    let n = lowered(nested("#ff0000"));
    let LayerSource::Scene(s) = &n.layers[0].source else {
        panic!("not a scene: {:?}", n.layers[0].source);
    };
    assert_eq!((s.size.width, s.size.height), (540, 960));
    assert_eq!(s.layers.len(), 2);
    assert_eq!(s.layers[0].to - s.layers[0].from, 180);
    assert_ne!(n.recipe, lowered(nested("#0000ff")).recipe);
    assert_eq!(n.recipe, lowered(nested("#ff0000")).recipe);
}

#[test]
fn nested_comps_are_named_capped_and_counted() {
    let doc = |comps: Value, layers: Value| {
        Comp::from_value(
            &json!({
                "schema": "ffcine.comp/1",
                "canvas": { "size": [1080, 1920], "fps": 30, "duration": 2 },
                "comps": comps,
                "layers": layers,
            }),
            Path::new("/media"),
        )
    };
    let use_ = |name: &str| json!([{ "source": { "comp": name } }]);
    let card = json!({ "layers": [{ "source": { "solid": "white" } }] });
    assert!(doc(json!({ "card": card }), use_("card")).is_ok());
    for (comps, layers, want) in [
        (json!({}), use_("card"), "no comp \"card\""),
        (
            json!({ "loop": { "layers": use_("loop") } }),
            use_("loop"),
            "levels deep",
        ),
        (
            json!({ "c": { "comps": {}, "layers": [] } }),
            use_("c"),
            "top level only",
        ),
        (
            json!({ "c": { "canvas": { "fps": 25 }, "layers": [] } }),
            use_("c"),
            "time remapping",
        ),
        (
            json!({ "v": { "layers": vec![json!({ "source": { "video": "a.mov" } }); 9] } }),
            json!([{ "source": { "comp": "v" } }, { "source": { "comp": "v" } }]),
            "at most 16 video layers",
        ),
    ] {
        let e = doc(comps, layers).expect_err(want).to_string();
        assert!(e.contains(want), "{want:?} not in {e:?}");
    }
}

#[test]
fn text_keys_cover_the_piece_and_fonts() {
    struct Fonts(std::cell::Cell<u32>);
    impl Media for Fonts {
        fn dims(&self, _: &Path) -> Result<(u32, u32)> {
            Ok((1920, 1080))
        }
        fn identity(&self, path: &Path) -> Value {
            assert_eq!(path, Path::new("/media/fonts/a.ttf"));
            json!([path, self.0.get()])
        }
    }
    // Fading text 0-6 s around a solid 2-4 s: the first and last pieces draw the same text
    // params at different points of its fades, so their keys differ.
    let c = comp(json!([
        { "source": { "text": { "text": "hi", "size": 80, "fade_in": 1, "fade_out": 1 } } },
        { "source": { "solid": "white" }, "size": [100, 100], "in": 2, "out": 4 },
    ]));
    assert_eq!(c.pieces(), [0..60, 60..120, 120..180]);
    let k = keys(&c);
    assert_ne!(k[0], k[2]);
    // Font files: resolved against `fonts_dir` and the comp's directory, their identities in
    // the key (blank text draws nothing, so no font is loaded here).
    let c = comp(json!([
        { "source": { "text": { "text": " ", "font_file": "a.ttf", "fonts_dir": "fonts" } } },
    ]));
    let m = Fonts(std::cell::Cell::new(1));
    let a = c.lower(0..180, OUT, &m).expect("lower").recipe;
    assert_eq!(a, c.lower(0..180, OUT, &m).expect("lower").recipe);
    m.0.set(2);
    assert_ne!(a, c.lower(0..180, OUT, &m).expect("lower").recipe);
}

#[test]
fn huge_stills_are_refused() {
    struct Big;
    impl Media for Big {
        fn dims(&self, _: &Path) -> Result<(u32, u32)> {
            Ok((20000, 100))
        }
        fn identity(&self, path: &Path) -> Value {
            json!(path)
        }
    }
    let c = comp(json!([{ "source": { "still": "a.heic" } }]));
    let e = c.lower(0..30, OUT, &Big).expect_err("too big").to_string();
    assert!(
        e.contains("layers[0].source.still") && e.contains("at most 16384"),
        "{e}"
    );
}

/// A fresh directory for files a test writes.
fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-comp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// A 2-point `.cube`, red scaled by `r`.
fn write_cube(path: &Path, r: f64) {
    let mut s = String::from("LUT_3D_SIZE 2\n");
    for b in [0.0, 1.0] {
        for g in [0.0, 1.0] {
            for x in [0.0, 1.0] {
                let _ = writeln!(s, "{} {g} {b}", x * r);
            }
        }
    }
    std::fs::write(path, s).expect("write cube");
}

fn hook_names(l: &ffcine_render::Layer) -> Vec<&str> {
    l.grade.hooks.iter().map(|h| h.name.as_str()).collect()
}

#[test]
fn layer_grades_lower_to_hooks() {
    let video = |grade: Value| json!([{ "source": { "video": "a.mov" }, "size": [540, 960], "grade": grade }]);
    // Ungraded layers keep their keys (no `layer_grade`), and a grade is keyed.
    let plain = lowered(json!([{ "source": { "video": "a.mov" }, "size": [540, 960] }]));
    assert!(!plain.recipe.to_string().contains("layer_grade"));
    let g = lowered(video(json!({ "warmth": 0.02, "rgb_gain": [1.1, 1, 0.9] })));
    assert_eq!(hook_names(&g.layers[0]), ["look"]);
    assert_eq!(g.layers[0].grade.rgb_gain, Some([1.1, 1.0, 0.9]));
    assert!(g.recipe.to_string().contains("layer_grade"));
    assert_ne!(
        g.recipe,
        lowered(video(json!({ "warmth": 0.03, "rgb_gain": [1.1, 1, 0.9] }))).recipe
    );
    // An identity grade adds no hook, but the layer's fade and alpha still come after it.
    let id = lowered(json!([{ "source": { "video": "a.mov" }, "size": [540, 960],
        "grade": { "warmth": 0.02 }, "fade": 0.5, "transform": { "opacity": 0.5 } }]));
    assert_eq!(hook_names(&id.layers[0]), ["look", "fade", "alpha"]);

    // Animated look params become tracks; the constant ones stay params.
    let a = lowered(video(json!({ "contrast": 0.1,
        "warmth": [{ "t": 0, "v": 0 }, { "t": 6, "v": 0.1 }] })));
    let look = &a.layers[0].grade.hooks[0];
    assert_eq!(look.name, "look");
    assert_eq!(look.tracks.len(), 1);
    assert_eq!(look.tracks[0].name, "warmth");
    assert_eq!(look.tracks[0].values.len(), 180);
    assert!(
        look.params
            .contains(&("contrast".into(), f64::from(0.1_f32)))
    );

    // Filters: chains (strict) and the object form, whose numbers may move.
    let f = lowered(video(json!({ "filters": ["eq=contrast=1.1",
        { "colorlevels": { "rimin": 0.05, "rimax": [{ "t": 0, "v": 1 }, { "t": 6, "v": 0.9 }] } }] })));
    assert_eq!(hook_names(&f.layers[0]), ["extra eq", "extra colorlevels"]);
    let levels = &f.layers[0].grade.hooks[1];
    assert!(levels.tracks.iter().any(|t| t.name == "rimax"));
    assert!(
        levels
            .params
            .iter()
            .any(|(k, v)| k == "rimin" && (v - 0.05).abs() < 1e-6)
    );
    for (grade, want) in [
        (
            json!({ "filters": ["hqdn3d"] }),
            "layers[0].grade.filters[0]",
        ),
        (
            json!({ "filters": [{ "eq": { "bogus": 1 } }] }),
            "layers[0].grade.filters[0]",
        ),
        (
            json!({ "lut_strength": [{ "t": 0, "v": 0 }] }),
            "animated lut_strength",
        ),
        (json!({ "camera": "nope" }), "layers[0].grade.camera"),
        (json!({ "cdl": {} }), "per-layer CDL"),
    ] {
        let e = try_comp(video(grade.clone()))
            .and_then(|c| c.lower(0..30, OUT, &Fake))
            .expect_err(&grade.to_string())
            .to_string();
        assert!(e.contains(want), "{grade}: {e}");
    }
    // Only pictures are graded; a camera is for video.
    for (layers, want) in [
        (
            json!([{ "source": { "solid": "white" }, "grade": {} }]),
            "only video, still and comp",
        ),
        (
            json!([{ "source": { "still": "a.png" }, "grade": { "camera": "dlog" } }]),
            "video layers only",
        ),
    ] {
        let e = try_comp(layers).expect_err("rejected").to_string();
        assert!(e.contains(want), "{e}");
    }
}

/// Clips are D-Log, stills display-referred.
struct Camera;

impl Media for Camera {
    fn dims(&self, _: &Path) -> Result<(u32, u32)> {
        Ok((1920, 1080))
    }
    fn identity(&self, path: &Path) -> Value {
        json!(path)
    }
    fn profile(&self, path: &Path) -> Result<ffcine_core::camera::Profile> {
        Ok(if path.extension().is_some_and(|e| e == "mov") {
            ffcine_core::camera::Profile::DjiDlog
        } else {
            ffcine_core::camera::Profile::Rec709
        })
    }
    fn camera_lut(
        &self,
        p: ffcine_core::camera::Profile,
    ) -> Result<Option<ffcine_core::camera::CameraLut>> {
        p.camera_lut(&[])
    }
}

#[test]
fn camera_auto_picks_each_sources_profile() {
    let c = comp(json!([
        { "source": { "video": "a.mov" }, "size": [540, 960], "grade": {} },
        { "source": { "still": "b.heic" }, "size": [540, 960], "grade": {} },
    ]));
    let p = c.lower(0..30, OUT, &Camera).expect("lower");
    let (log, still) = (&p.layers[0].grade, &p.layers[1].grade);
    assert!(log.input_lut.is_some() && log.deband.is_some());
    assert!(log.input_color.transfer.is_some());
    assert!(still.input_lut.is_none() && still.deband.is_none());
    let r = p.recipe.to_string();
    assert!(
        r.contains(r#""profile":"dlog""#) && r.contains(r#""profile":"rec709""#),
        "{r}"
    );
    // Without a grade (and no project) nothing is applied.
    let c = comp(json!([{ "source": { "video": "a.mov" }, "size": [540, 960] }]));
    let p = c.lower(0..30, OUT, &Camera).expect("lower");
    assert!(p.layers[0].grade.input_lut.is_none());
}

#[test]
#[allow(clippy::many_single_char_names)]
fn projects_and_luts_resolve_and_key() {
    let dir = scratch("luts");
    write_cube(&dir.join("a.cube"), 1.0);
    let doc = |project: Value, layers: Value| {
        let mut d = json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [1080, 1920], "fps": 30, "duration": 2 },
            "layers": layers,
        });
        if !project.is_null() {
            d["project"] = project;
        }
        Comp::from_value(&d, &dir)
    };
    let layers = json!([
        { "source": { "video": "a.mov" }, "size": [540, 960], "grade": { "lut": "base" } },
        { "source": { "solid": "white" }, "size": [100, 100] },
    ]);
    // An inline project grades every picture layer (solids aren't), its LUT aliases resolve
    // against the comp's directory, and the key follows the LUT's contents. (A display-referred
    // source drops the project's own LUT under the light rule, as a timeline clip does; the
    // layer's `lut` overrides that.)
    let project = json!({ "grade": { "warmth": 0.05, "lut": "base" },
        "luts": { "base": "a.cube" } });
    let key = || {
        let c = doc(project.clone(), layers.clone()).expect("comp");
        let p = c.lower(0..30, OUT, &Fake).expect("lower");
        assert_eq!(hook_names(&p.layers[0]), ["look"]);
        assert!(p.layers[0].grade.lut.is_some());
        assert_eq!(hook_names(&p.layers[1]), ["solid"]);
        p.recipe
    };
    let a = key();
    write_cube(&dir.join("a.cube"), 0.5);
    assert_ne!(a, key(), "a LUT edit changes the key");
    // A nested comp shares it, and can't name its own.
    let nested = json!([{ "source": { "comp": { "layers": [
        { "source": { "video": "a.mov" } }] } }, "size": [540, 960] }]);
    let c = doc(project.clone(), nested).expect("comp");
    let p = c.lower(0..30, OUT, &Fake).expect("lower");
    let LayerSource::Scene(s) = &p.layers[0].source else {
        panic!("not a scene");
    };
    assert_eq!(hook_names(&s.layers[0]), ["look"]);
    let e = doc(
        project.clone(),
        json!([{ "source": { "comp": { "project": {}, "layers": [] } } }]),
    )
    .expect_err("nested project")
    .to_string();
    assert!(e.contains("parent's project"), "{e}");
    // A timeline's project, borrowed.
    let mut t = ffcine_timeline::Timeline::new("t");
    let mut pr = ffcine_timeline::Project::default();
    pr.grade.saturation = Some(1.2);
    t.set_project(&pr).expect("set project");
    t.to_file(dir.join("t.otio")).expect("write timeline");
    let plain = json!([{ "source": { "video": "a.mov" }, "size": [540, 960] }]);
    let c = doc(json!("t.otio"), plain.clone()).expect("comp");
    let p = c.lower(0..30, OUT, &Fake).expect("lower");
    assert_eq!(hook_names(&p.layers[0]), ["look"]);
    let e = doc(json!("missing.otio"), plain)
        .expect_err("missing")
        .to_string();
    assert!(e.starts_with("project") || e.contains("project"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One video layer with `time`, lowered over `range`: its start (s) and absolute frame map.
fn remapped(time: &Value, range: std::ops::Range<i64>) -> (f64, Vec<i64>, Value) {
    let c = comp(json!([{ "source": { "video": "a.mov" }, "time": time }]));
    let p = c.lower(range, OUT, &Fake).expect("lower");
    let LayerSource::Video { start, map, .. } = &p.layers[0].source else {
        panic!("video");
    };
    let map = map.as_ref().expect("map");
    // Back to absolute source frames: start is half a frame before the lowest pick.
    #[allow(clippy::cast_possible_truncation)]
    let lo = if start.is_zero() {
        0
    } else {
        (start.as_secs_f64() * 30.0 + 0.5).round() as i64
    };
    (
        start.as_secs_f64(),
        map.iter().map(|k| k + lo).collect(),
        p.recipe,
    )
}

#[test]
fn time_remaps_pick_source_frames() {
    // Speed 2: every other frame, ties to the earlier one (as `setpts=PTS/2,fps`).
    let (_, m, _) = remapped(&json!({ "speed": 2 }), 0..10);
    assert_eq!(m, [0, 2, 4, 6, 8, 10, 12, 14, 16, 18]);
    // Half speed: each frame twice.
    let (_, m, _) = remapped(&json!({ "speed": 0.5 }), 0..6);
    assert_eq!(m, [0, 0, 1, 1, 2, 2]);
    // Reverse from source_in: back from frame 0 clamps at it.
    let (_, m, _) = remapped(&json!({ "reverse": true }), 0..3);
    assert_eq!(m, [0, 0, 0]);
    let c = comp(json!([{ "source": { "video": "a.mov" }, "source_in": 5,
        "time": { "reverse": true } }]));
    let p = c.lower(0..30, OUT, &Fake).expect("lower");
    let LayerSource::Video { start, map, .. } = &p.layers[0].source else {
        panic!("video");
    };
    let map = map.as_ref().expect("map");
    // Frames 150 down to 121: the lowest is 121, the start half a frame before it.
    assert!((start.as_secs_f64() - 120.5 / 30.0).abs() < 1e-9);
    let abs: Vec<i64> = map.iter().map(|k| k + 121).collect();
    assert_eq!(abs, (121..=150).rev().collect::<Vec<_>>());
    // Freeze: one frame.
    let (_, m, _) = remapped(&json!({ "freeze": 2 }), 0..4);
    assert_eq!(m, [60; 4]);
    // An explicit source time (3x: as `setpts=PTS/3,fps`, the last frame before mid-frame),
    // clamped to the 300 frames there are.
    let (_, m, _) = remapped(&json!({ "source": "t * 3" }), 0..4);
    assert_eq!(m, [1, 4, 7, 10]);
    let (_, m, _) = remapped(&json!({ "freeze": 100 }), 0..2);
    assert_eq!(m, [299, 299]);
}

#[test]
fn time_remap_keys_follow_the_map_and_split_cleanly() {
    let ramp = json!({ "speed": [{ "t": 0, "v": 1 }, { "t": 2, "v": 3 }] });
    let (_, whole, _) = remapped(&ramp, 0..120);
    let (_, a, ka) = remapped(&ramp, 0..50);
    let (_, b, kb) = remapped(&ramp, 50..120);
    assert_eq!(
        [a, b].concat(),
        whole,
        "split pieces show the frames the whole does"
    );
    assert_ne!(ka, kb);
    // The same slice, the same key; another speed, another key.
    assert_eq!(remapped(&ramp, 0..50).2, ka);
    assert_ne!(remapped(&json!({ "speed": 2 }), 0..50).2, ka);
    // A layer without `time` keeps its old key shape (no map).
    let plain = keys(&comp(json!([{ "source": { "video": "a.mov" } }])));
    assert!(!plain[0].to_string().contains("digest"));
    assert!(ka.to_string().contains("digest"));
}

#[test]
fn time_remap_numeric_matches_the_closed_form() {
    // A linear ramp as keys (closed form) and as an expression (Simpson): the same frames.
    let (_, keys, _) = remapped(
        &json!({ "speed": [{ "t": 0, "v": 1 }, { "t": 2, "v": 3 }, { "t": 6, "v": 3 }] }),
        0..120,
    );
    let (_, expr, _) = remapped(&json!({ "speed": "1 + min(t, 2)" }), 0..120);
    assert_eq!(keys, expr);
    // Integrated: s(t) = t + t²/2 up to 2 s, then 4 + 3 (t - 2).
    // Frame 30 shows the last source frame before s(1 + 1/60) = 1.533 s, frame 90 the last
    // before s(3 + 1/60) = 7.05 s.
    assert_eq!((keys[30], keys[90]), (46, 211));
}

#[test]
fn time_remap_speeds_are_bounded_while_evaluated() {
    let e = try_comp(json!([{ "source": { "video": "a.mov" }, "time": { "speed": "t * 1000" } }]))
        .and_then(|c| c.lower(0..30, OUT, &Fake).map(|_| ()))
        .expect_err("too fast")
        .to_string();
    assert!(e.contains("at most 100"), "{e}");
    let e = try_comp(json!([{ "source": { "video": "a.mov" }, "time": { "source": "-t * 1e9" } }]))
        .and_then(|c| c.lower(0..30, OUT, &Fake).map(|_| ()))
        .expect_err("too far")
        .to_string();
    assert!(e.contains("a source time is"), "{e}");
    // Negative source times are rejected too, as the message says.
    let e = try_comp(json!([{ "source": { "video": "a.mov" }, "time": { "source": "t - 0.5" } }]))
        .and_then(|c| c.lower(0..30, OUT, &Fake).map(|_| ()))
        .expect_err("negative")
        .to_string();
    assert!(e.contains("a source time is 0 to 3600 s"), "{e}");
}
