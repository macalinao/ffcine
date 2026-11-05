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
            json!([{ "source": { "still": "b.heic" } }]),
            "compositing step 3",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "grade": {} }]),
            "compositing step 4",
        ),
        (
            json!([{ "source": { "video": "a.mov" }, "time": { "speed": 2 } }]),
            "compositing step 5",
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
    // Later steps' keys name the step.
    for k in ["content", "crop"] {
        bad(
            canvas.clone(),
            json!([solid(json!({ k: {} }))]),
            "compositing step 3",
        );
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
