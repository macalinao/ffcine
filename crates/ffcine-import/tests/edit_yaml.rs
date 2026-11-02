//! `edit.yaml` importer: a small fixture with known timing, and every `edit*.yaml` in
//! `FFCINE_EDIT_YAML_DIR` when set.

#![allow(clippy::many_single_char_names)]

use std::path::Path;

use ffcine_import::{self as import, Format, ImportOptions, Imported};
use ffcine_timeline::{Clip, Effect, HasMetadata, Item, MediaReference, Timeline, Track};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/edit_yaml/edit.yaml"
);

fn frames(item: &Item) -> f64 {
    item.source_range().map_or(0.0, |r| r.duration.value)
}

fn track_frames(t: &Track) -> f64 {
    t.children
        .iter()
        .filter(|i| !matches!(i, Item::Transition(_)))
        .map(frames)
        .sum()
}

fn track<'a>(t: &'a Timeline, name: &str) -> &'a Track {
    t.track_list()
        .find(|tr| tr.name == name)
        .unwrap_or_else(|| panic!("no track {name}"))
}

fn clip<'a>(t: &'a Timeline, id: &str) -> &'a Clip {
    t.clips()
        .into_iter()
        .find(|c| c.ffcine_id() == Some(id))
        .unwrap_or_else(|| panic!("no clip {id}"))
}

fn effect<'a>(c: &'a Clip, name: &str) -> Option<&'a Effect> {
    c.effects.iter().find(|e| e.effect_name() == Some(name))
}

fn params(e: &Effect) -> Value {
    Value::Object(e.ffcine().cloned().unwrap_or_default())
}

/// JSON out, back in, and out again: same model, same bytes.
fn assert_round_trips(t: &Timeline) {
    let json = t.to_json().expect("write");
    let back = Timeline::from_json(&json).expect("read");
    assert_eq!(&back, t);
    assert_eq!(back.to_json().expect("write"), json);
}

fn fixture() -> Imported {
    import::import(Path::new(FIXTURE), None, &ImportOptions::default()).expect("import")
}

#[test]
fn fixture_timing_follows_render_py() {
    let Imported { timeline: t, .. } = fixture();
    let v1 = track(&t, "V1");
    // a 48 - 12 (dissolve) + b 48 + freeze 24 + c 48 + e 50 (ramp 2->4 over 6 s).
    assert!((track_frames(v1) - 206.0).abs() < 1e-9);
    let kinds: Vec<&str> = v1
        .children
        .iter()
        .map(|i| match i {
            Item::Clip(_) => "clip",
            Item::Transition(_) => "transition",
            Item::Stack(_) => "stack",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        ["clip", "transition", "clip", "clip", "stack", "clip"]
    );
    let Item::Transition(tr) = &v1.children[1] else {
        unreachable!()
    };
    assert!(tr.in_offset.value.abs() < 1e-9 && (tr.out_offset.value - 12.0).abs() < 1e-9);
    let chapters: Vec<(&str, f64)> = v1
        .markers
        .iter()
        .map(|m| (m.name.as_str(), m.marked_range.start_time.value))
        .collect();
    assert_eq!(chapters, [("Intro", 0.0), ("Day 1", 42.0)]);
}

#[test]
fn fixture_clips() {
    let Imported {
        timeline: t,
        report,
    } = fixture();
    let a = clip(&t, "a");
    assert_eq!(a.ffcine_value("label"), Some(&json!("first")));
    assert_eq!(a.foreign("edit_yaml"), None);
    assert_eq!(
        params(effect(a, "ffcine.fx.vignette").expect("vignette")),
        json!({"schema": "ffcine.fx.vignette/1", "amount": 0.3})
    );
    let g = effect(a, "ffcine.color.grade").expect("grade");
    assert_eq!(
        params(g),
        json!({"schema": "ffcine.color.grade/1", "look": "night"})
    );
    let MediaReference::External(r) = a.media_reference().expect("media") else {
        panic!("external")
    };
    assert!(
        r.path()
            .expect("path")
            .ends_with("media/DJI_20260704100000_0001_D.MP4")
    );

    let b = clip(&t, "b");
    let Some(Effect::LinearTimeWarp(w)) = effect(b, "LinearTimeWarp") else {
        panic!("speed")
    };
    assert!((w.time_scalar - 2.0).abs() < 1e-12);
    let hold = clip(&t, "b/hold");
    assert!(matches!(hold.effects[0], Effect::FreezeFrame(_)));

    let e = clip(&t, "e");
    let ramp = params(effect(e, "ffcine.time.ramp").expect("ramp"));
    assert_eq!(ramp["points"], json!([[0.0, 2.0], [1.0, 4.0]]));
    assert_eq!(
        params(effect(e, "ffcine.fx.stabilize").expect("stab"))["smoothing"],
        json!(30.0)
    );
    assert_eq!(
        params(effect(e, "ffcine.geo.reframe").expect("reframe"))["punch"],
        json!([{"at": 1.0, "zoom": 1.3}])
    );

    // Split screen: Live Photo MOV then its HEIC still; a short clip then a freeze.
    let p1 = clip(&t, "c/p1");
    assert!((p1.source_range.as_ref().expect("range").duration.value - 24.0).abs() < 1e-9);
    clip(&t, "c/p1/still");
    let p2 = clip(&t, "c/p2");
    assert!((p2.source_range.as_ref().expect("range").duration.value - 12.0).abs() < 1e-9);

    // Audio, captions, SFX, music, ambience.
    let audio = clip(&t, "a/audio");
    assert_eq!(
        params(effect(audio, "ffcine.audio.level").expect("level"))["mode"],
        json!("orig")
    );
    let sub = clip(&t, "a/sub1");
    let MediaReference::Generator(g) = sub.media_reference().expect("media") else {
        panic!("generator")
    };
    assert_eq!(g.parameters["text"], json!("hi *there*"));
    let giggle = clip(&t, "b/sfx2");
    assert_eq!(giggle.ffcine_value("private"), Some(&json!(true)));
    assert!(clip(&t, "music1").source_range.is_some());
    let bed = clip(&t, "ambience1");
    assert_eq!(bed.foreign("edit_yaml"), None);
    let clean = bed
        .effects
        .iter()
        .find_map(|e| match e {
            ffcine_timeline::Effect::Effect(g) if g.effect_name == "ffcine.audio.clean" => {
                g.ffcine()
            }
            _ => None,
        })
        .expect("clean");
    assert_eq!(clean.get("lowpass"), Some(&json!(9000.0)));

    let p = t.project().expect("project").expect("valid");
    assert_eq!(p.default_look.as_deref(), Some("day"));
    assert_eq!(p.rules.light, Some(0.6));
    assert!(p.luts["normal"].ends_with("edit_yaml/luts/normal.cube"));
    assert!(!report.unmapped.iter().any(|u| u.contains("vignette")));
    assert!(report.unmapped.contains(&"sfx.ref_dbfs".to_owned()));
    assert!(report.warnings.iter().any(|w| w.contains("skip")));
    let foreign = t.foreign("edit_yaml").expect("foreign");
    assert_eq!(foreign["skipped_shots"][0]["id"], json!("d"));
    assert_round_trips(&t);
}

#[test]
fn every_sample_edit_converts() {
    let Some(root) = std::env::var_os("FFCINE_EDIT_YAML_DIR") else {
        return;
    };
    let Ok(dir) = std::fs::read_dir(&root) else {
        eprintln!("{} not readable; skipped", root.display());
        return;
    };
    let mut n = 0;
    for entry in dir.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !(name.starts_with("edit") && name.ends_with(".yaml")) {
            continue;
        }
        let Imported { timeline, report } =
            import::import(&path, Some(Format::EditYaml), &ImportOptions::default())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            !report.warnings.iter().any(|w| w.contains("missing")),
            "{name}: {report}"
        );
        assert!(!timeline.clips().is_empty(), "{name}");
        assert_round_trips(&timeline);
        n += 1;
    }
    assert!(n >= 5, "found {n} edit*.yaml");
}

const EFFECTS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/edit_yaml/effects.yaml"
);

/// A beat every half second (120 BPM) from 0.
#[allow(clippy::unnecessary_wraps)]
fn half_second_beats(_: &Path) -> Option<Vec<f64>> {
    Some((0..400).map(|k| f64::from(k) * 0.5).collect())
}

fn effects(beats: bool) -> Imported {
    let opts = ImportOptions {
        beats: beats.then_some(half_second_beats as import::BeatsFn),
        ..ImportOptions::default()
    };
    import::import(Path::new(EFFECTS), None, &opts).expect("import")
}

fn v1(t: &Timeline) -> &Track {
    track(t, "V1")
}

fn stack<'a>(t: &'a Timeline, id: &str) -> &'a ffcine_timeline::Stack {
    v1(t)
        .children
        .iter()
        .find_map(|i| match i {
            Item::Stack(s) if s.ffcine_id() == Some(id) => Some(s),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no stack {id}"))
}

fn names(effects: &[Effect]) -> Vec<&str> {
    effects.iter().filter_map(Effect::effect_name).collect()
}

fn dur(c: &Clip) -> f64 {
    c.source_range.as_ref().expect("range").duration.value
}

fn snap(c: &impl HasMetadata) -> Option<i64> {
    c.ffcine_value("snap").and_then(|s| s["moved"].as_i64())
}

/// `[(t in frames of source time, v, interp)]` of a keyframed parameter.
fn keys(v: &Value) -> Vec<(f64, Value, String)> {
    v.as_array()
        .expect("keyframes")
        .iter()
        .map(|k| {
            (
                k["t"]["value"].as_f64().expect("t"),
                k["v"].clone(),
                k["interp"].as_str().expect("interp").to_owned(),
            )
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)] // one fixture, checked top to bottom
fn effects_and_transitions() {
    let Imported {
        timeline: t,
        report,
    } = effects(true);
    let grain = params(&t.tracks.effects[0]);
    assert_eq!(
        grain,
        json!({"schema": "ffcine.fx.grain/1", "strength": 1.155, "chroma": 0.4,
               "profiles": {"preview": {"strength": 1.155}, "master": {"strength": 0.693}}})
    );
    let names_of: Vec<&str> = t.track_list().map(|tr| tr.name.as_str()).collect();
    assert_eq!(
        names_of,
        [
            "V1",
            "PiP 1",
            "PiP 2",
            "Overlays 1",
            "Overlays 2",
            "Dialogue 1",
            "Music 1"
        ]
    );

    let trans: Vec<(f64, Value)> = v1(&t)
        .children
        .iter()
        .filter_map(|i| match i {
            Item::Transition(tr) => Some((
                tr.out_offset.value,
                Value::Object(tr.ffcine().cloned().unwrap_or_default()),
            )),
            _ => None,
        })
        .collect();
    let tr = |kind: &str, extra: Value| {
        let mut m = json!({"schema": "ffcine.transition/1", "kind": kind});
        m.as_object_mut()
            .expect("obj")
            .extend(extra.as_object().expect("obj").clone());
        m
    };
    assert_eq!(
        trans,
        [
            (12.0, tr("dip", json!({"colour": [0.0, 0.0, 0.0]}))),
            (6.0, tr("whip", json!({"direction": "right"}))),
            (7.0, tr("flash", json!({"colour": [1.0, 1.0, 1.0]}))),
            (10.0, tr("dip", json!({"colour": [1.0, 1.0, 1.0]}))),
            (12.0, json!({})),
        ]
    );

    let a = clip(&t, "a");
    assert_eq!(
        names(&a.effects),
        ["ffcine.fx.darken", "ffcine.fx.vignette", "ffcine.fx.fade"]
    );
    assert_eq!(params(&a.effects[0])["amount"], json!(0.8));
    assert_eq!(params(&a.effects[1])["amount"], json!(0.35));
    assert_eq!(
        params(&a.effects[2]),
        json!({"schema": "ffcine.fx.fade/1", "in": 0.5, "colour": [0.0, 0.0, 0.0]})
    );
    // 50 frames, snapped to 48 on the beat, minus the 12-frame dip.
    assert_eq!(snap(a), Some(-2));
    assert!((dur(a) - 36.0).abs() < 1e-9);

    // Freeze with flash: the fade out goes on the hold, vignette on both.
    let b = clip(&t, "b");
    assert_eq!(names(&b.effects), ["ffcine.fx.vignette"]);
    let hold = clip(&t, "b/hold");
    assert_eq!(
        names(&hold.effects),
        [
            "FreezeFrame",
            "ffcine.fx.flash",
            "ffcine.fx.vignette",
            "ffcine.fx.fade"
        ]
    );
    assert_eq!(
        params(&hold.effects[1]),
        json!({"schema": "ffcine.fx.flash/1", "at": 0, "levels": [0.5, 0.5, 0.2]})
    );
    assert_eq!(
        params(&hold.effects[3]),
        json!({"schema": "ffcine.fx.fade/1", "out": 0.8, "colour": [0.0, 0.0, 0.0]})
    );
    // Live Photo: the still always flashes; the hold takes the snap (+6).
    let still = clip(&t, "c/hold");
    assert_eq!(
        params(effect(still, "ffcine.fx.flash").expect("flash"))["levels"],
        json!([0.55, 0.55, 0.22])
    );
    assert_eq!(snap(clip(&t, "c")), Some(6));
    assert!((dur(still) - 23.0).abs() < 1e-9);

    let d = stack(&t, "d");
    assert_eq!(
        names(&d.effects),
        ["ffcine.comp.layout", "ffcine.fx.vignette", "ffcine.fx.fade"]
    );
    let e = stack(&t, "e");
    assert_eq!(params(&e.effects[0])["layout"], json!("1+2"));
    assert_eq!(e.children.len(), 3);

    // Beats: the card and title land on beats, the cue's `in` on the nearest beat.
    let title = clip(&t, "title");
    let MediaReference::Generator(g) = title.media_reference().expect("media") else {
        panic!("generator")
    };
    assert!(
        (g.parameters["title_at"].as_f64().expect("at") - 12.0 * 1001.0 / 24000.0).abs() < 1e-9
    );
    assert!((dur(title) - 50.0).abs() < 1e-9);
    let music = clip(&t, "music1");
    assert!(
        music
            .source_range
            .as_ref()
            .expect("range")
            .start_time
            .value
            .abs()
            < 1e-9
    );
    assert!(report.warnings.is_empty(), "{report}");
    assert_eq!(report.unmapped, ["output", "sources"]);
    assert_round_trips(&t);
}

#[test]
fn overlays() {
    let Imported { timeline: t, .. } = effects(true);
    let rate = 24000.0 / 1001.0;
    let sticker = clip(&t, "f/ov1");
    assert_eq!(
        sticker.ffcine_value("source"),
        Some(&json!({"src": "iphone:logo.png", "kind": "image"}))
    );
    let MediaReference::External(r) = sticker.media_reference().expect("media") else {
        panic!("external")
    };
    assert!(r.path().expect("path").ends_with("media/logo.png"));
    assert!((dur(sticker) - 48.0).abs() < 1e-9);
    let tf = params(effect(sticker, "ffcine.geo.transform").expect("transform"));
    assert_eq!(
        (&tf["units"], &tf["y"], &tf["scale_x"], &tf["scale_y"]),
        (&json!("frame"), &json!("down"), &json!(0.2), &json!(0.2))
    );
    let tx = keys(&tf["translate_x"]);
    assert_eq!(tx.len(), 2);
    assert!(tx[0].0.abs() < 1e-9 && (tx[1].0 - rate).abs() < 1e-9);
    assert_eq!((&tx[0].1, &tx[1].1), (&json!(0.3), &json!(0.35)));
    let blend = params(effect(sticker, "ffcine.comp.blend").expect("blend"));
    assert_eq!(
        (&blend["mode"], &blend["radius"]),
        (&json!("normal"), &json!(0.0))
    );
    let op = keys(&blend["opacity"]);
    assert_eq!(op[1].2, "ease");
    assert!((op[1].0 - 0.5 * rate).abs() < 1e-9);

    // Video PiP from 1 s in: keys in source time; fades as opacity keys.
    let pip = clip(&t, "f/ov2");
    assert_eq!(
        pip.ffcine_value("source").expect("source")["kind"],
        json!("video")
    );
    let start = pip.source_range.as_ref().expect("range").start_time.value;
    assert!((start - rate).abs() < 1e-9);
    assert_eq!(
        names(&pip.effects),
        [
            "ffcine.color.grade",
            "ffcine.geo.transform",
            "ffcine.comp.blend"
        ]
    );
    let blend = params(&pip.effects[2]);
    assert_eq!(blend["radius"], json!(0.03));
    let op: Vec<(f64, f64)> = keys(&blend["opacity"])
        .into_iter()
        .map(|(t, v, _)| ((t - start) / rate, v.as_f64().expect("v")))
        .collect();
    let want = [
        (0.0, 0.0),
        (0.3, 1.0),
        (48.0 / rate - 0.3, 1.0),
        (48.0 / rate, 0.0),
    ];
    for ((t, v), (wt, wv)) in op.iter().zip(want) {
        assert!((t - wt).abs() < 1e-9 && (v - wv).abs() < 1e-12, "{op:?}");
    }
    // Both start 0.5 s (12 frames) into shot f, which starts at 228.
    let pip1 = track(&t, "PiP 1");
    assert!((frames(&pip1.children[0]) - 240.0).abs() < 1e-9);
}

#[test]
fn without_beats_cuts_stay_unsnapped() {
    let Imported {
        timeline: t,
        report,
    } = effects(false);
    assert!(t.clips().iter().all(|c| snap(*c).is_none()));
    assert!((dur(clip(&t, "a")) - 38.0).abs() < 1e-9);
    assert!(
        report.warnings.iter().any(|w| w.contains("beat snapping")),
        "{report}"
    );
    assert!(report.unmapped.contains(&"music.snap_max".to_owned()));
}
