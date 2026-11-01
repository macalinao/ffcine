//! CapCut importer on hand-written drafts.

#![allow(clippy::doc_markdown)]

use std::path::Path;

use ffcine_import::{Format, ImportOptions, capcut};
use ffcine_timeline::{Clip, Effect, HasMetadata, Item, MediaReference, Timeline, Track};
use serde_json::{Value, json};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/capcut");

fn load(name: &str) -> (Timeline, ffcine_import::ImportReport) {
    let r = capcut::import(&Path::new(DATA).join(name), &ImportOptions::default())
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    let json = r.timeline.to_json().expect("to_json");
    assert_eq!(Timeline::from_json(&json).expect("from_json"), r.timeline);
    (r.timeline, r.report)
}

fn track<'a>(t: &'a Timeline, name: &str) -> &'a Track {
    t.track_list()
        .find(|x| x.name == name)
        .unwrap_or_else(|| panic!("no track {name}"))
}

fn labels(t: &Track) -> Vec<String> {
    t.children
        .iter()
        .map(|i| match i {
            Item::Transition(_) => "T".into(),
            Item::Gap(_) => "-".into(),
            _ => i.name().into(),
        })
        .collect()
}

fn clips(t: &Track) -> Vec<&Clip> {
    t.children
        .iter()
        .filter_map(|i| match i {
            Item::Clip(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn fx<'a>(c: &'a Clip, name: &str) -> &'a serde_json::Map<String, Value> {
    c.effects
        .iter()
        .find_map(|e| match e {
            Effect::Effect(g) if g.effect_name == name => g.ffcine(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{}: no {name}", c.name))
}

fn frames(c: &Clip) -> (f64, f64) {
    let r = c.source_range.as_ref().expect("source_range");
    (r.start_time.value, r.duration.value)
}

#[test]
fn editorial_timing_in_frames() {
    let (t, _) = load("features.json");
    assert_eq!(t.name, "features");
    assert_eq!(t.ffcine_value("frame_size"), Some(&json!([1920, 1080])));
    let names: Vec<_> = t.track_list().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["V1", "V2", "V3", "A1"]);

    let v1 = track(&t, "V1");
    assert_eq!(labels(v1), ["a.mp4", "T", "b.jpg", "-", "a.mp4"]);
    let c = clips(v1);
    assert_eq!(frames(c[0]), (30.0, 60.0));
    assert!((c[0].source_range.as_ref().expect("r").start_time.rate - 30.0).abs() < 1e-9);
    assert_eq!(frames(c[1]), (0.0, 60.0));
    assert_eq!(frames(c[2]), (0.0, 60.0));
    match &v1.children[3] {
        Item::Gap(g) => assert_eq!(
            g.source_range.as_ref().map(|r| r.duration.value),
            Some(30.0)
        ),
        _ => panic!("no gap"),
    }
    let Item::Transition(tr) = &v1.children[1] else {
        panic!("no transition");
    };
    assert_eq!((tr.in_offset.value, tr.out_offset.value), (7.5, 7.5));
    assert_eq!(tr.transition_type, "SMPTE_Dissolve");
    assert_eq!(
        tr.ffcine_value("schema"),
        Some(&json!("ffcine.transition/1"))
    );
    assert_eq!(tr.ffcine_value("kind"), Some(&json!("dissolve")));

    // Media: placeholder resolved against the draft dir; photos keep their kind.
    let Some(MediaReference::External(e)) = c[0].media_reference() else {
        panic!("no external ref");
    };
    assert_eq!(e.path(), Some(Path::new(DATA).join("media/a.mp4")));
    assert_eq!(
        c[1].ffcine_value("source"),
        Some(&json!({"src": "/abs/b.jpg", "kind": "photo"}))
    );
    let a1 = clips(track(&t, "A1"));
    assert_eq!(a1[0].name, "Song");
    let Some(MediaReference::External(e)) = a1[0].media_reference() else {
        panic!("no external ref");
    };
    assert_eq!(e.path(), Some(Path::new(DATA).join("music.mp3")));
}

#[test]
fn effects() {
    let (t, report) = load("features.json");
    let v1 = clips(track(&t, "V1"));

    let tf = fx(v1[0], "ffcine.geo.transform");
    assert_eq!(tf["units"], json!("frame"));
    assert_eq!(tf["y"], json!("down"));
    assert_eq!(tf["translate_y"], json!(-0.25));
    assert_eq!(tf["scale_x"], json!(1.2));
    assert_eq!(tf["rotate_deg"], json!(-30.0));
    assert_eq!(tf["flip_x"], json!(true));
    // Keyframe t in source time: source start (30) + frames since the clip's first frame.
    let kx = tf["translate_x"].as_array().expect("keyframes");
    assert_eq!(kx[0]["t"]["value"], json!(30.0));
    assert_eq!(kx[1]["t"]["value"], json!(60.0));
    assert_eq!(kx[1]["v"], json!(0.5));
    assert_eq!(kx[1]["interp"], json!("linear"));
    assert_eq!(fx(v1[0], "ffcine.comp.blend")["opacity"], json!(0.8));
    assert!(v1[0].foreign("capcut").is_some_and(|f| {
        f["common_keyframes"][0]["property_type"] == "KFTypeMask"
            && f["materials"]["canvases"][0]["id"] == "c1"
    }));

    let speed = |c: &Clip| {
        c.effects.iter().find_map(|e| match e {
            Effect::LinearTimeWarp(w) => Some(w.time_scalar),
            _ => None,
        })
    };
    assert_eq!(speed(v1[0]), None);
    assert_eq!(speed(v1[1]), Some(2.0));
    // curve 1 -> 3 over the clip, scaled to the 4 s of source: average 2.
    assert!((speed(v1[2]).expect("warp") - 2.0).abs() < 1e-9);
    let ramp = fx(v1[2], "ffcine.time.ramp");
    assert!((ramp["source_duration"].as_f64().expect("dur") - 4.0).abs() < 1e-9);
    let pts = ramp["points"].as_array().expect("points");
    assert!(pts[0][1].as_f64() < pts.last().expect("p")[1].as_f64());

    let grade = fx(v1[2], "ffcine.color.grade");
    assert_eq!(grade["exposure"], json!(0.1));
    let sat = grade["saturation"].as_array().expect("keyframes");
    assert_eq!(sat[0]["v"], json!(1.2));
    assert_eq!(sat[0]["interp"], json!("ease"));
    assert_eq!(sat[1]["t"]["value"], json!(30.0));
    assert_eq!(fx(v1[2], "ffcine.audio.level")["gain_db"], json!(-96.0));
    assert!(v1[2].effects.iter().any(|e| matches!(e,
        Effect::Effect(g) if g.effect_name == "capcut.video_effects" && g.name == "Glitch")));

    let a1 = clips(track(&t, "A1"));
    let lvl = fx(a1[0], "ffcine.audio.level");
    assert_eq!(
        (lvl["fade_in"].clone(), lvl["fade_out"].clone()),
        (json!(1.0), json!(2.0))
    );
    let g = lvl["gain_db"].as_array().expect("keyframes");
    assert_eq!(g[1]["t"]["value"], json!(90.0));
    assert!((g[1]["v"].as_f64().expect("v") + 6.0206).abs() < 1e-3);

    // Text generator.
    let v2 = track(&t, "V2");
    assert_eq!(labels(v2), ["-", "Hello"]);
    let text = clips(v2)[0];
    assert_eq!(frames(text), (0.0, 60.0));
    let Some(MediaReference::Generator(g)) = text.media_reference() else {
        panic!("no generator");
    };
    assert_eq!(g.generator_kind, "ffcine.text");
    let p = &g.parameters;
    assert_eq!(p["text"], json!("Hello\nWorld"));
    assert_eq!(p["size"], json!(67.5));
    assert_eq!(p["font"], json!("Inter-Bold"));
    assert_eq!(p["colour"], json!([1.0, 0.0, 0.0, 1.0]));
    assert_eq!(p["bold"], json!(true));
    assert_eq!(p["alignment"], json!("center"));
    assert_eq!(p["position"], json!([0.0, 0.4]));
    assert_eq!(
        p["animation"],
        json!({"in": {"kind": "fade", "duration": 0.5}})
    );

    // Filter track: a gap carrying the store filter.
    let v3 = track(&t, "V3");
    let Item::Gap(f) = &v3.children[0] else {
        panic!("no filter gap");
    };
    assert_eq!(f.name, "Warm");
    assert_eq!(
        f.ffcine_id(),
        Some("capcut-F5555555-5555-5555-5555-555555555555")
    );
    assert!(matches!(&f.effects[0], Effect::Effect(g) if g.effect_name == "capcut.filters"));

    let foreign = t.foreign("capcut").expect("timeline foreign");
    assert_eq!(foreign["platform"]["app_version"], json!("5.9.0"));
    assert_eq!(foreign["materials"]["unused_pool"][0]["id"], json!("u1"));
    for u in [
        "platform",
        "materials.canvases",
        "materials.videos[].crop_ratio",
        "materials.transitions[].effect_id",
        "segments[].common_keyframes.KFTypeMask",
        "materials.video_effects",
        "materials.filters",
    ] {
        assert!(
            report.unmapped.iter().any(|x| x == u),
            "{u} not in {:?}",
            report.unmapped
        );
    }
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
}

#[test]
fn stable_ids() {
    let (a, _) = load("features.json");
    let (b, _) = load("features.json");
    let ids = |t: &Timeline| -> Vec<String> {
        t.clips()
            .iter()
            .filter_map(|c| c.ffcine_id().map(str::to_owned))
            .collect()
    };
    let ia = ids(&a);
    assert_eq!(ia, ids(&b));
    assert!(ia.contains(&"capcut-A1111111-1111-1111-1111-111111111111".to_owned()));
    assert!(ia.contains(&"capcut-T4444444-4444-4444-4444-444444444444".to_owned()));
    assert_eq!(ia.len(), 5);
}

#[test]
fn detection_and_directories() {
    let dir = Path::new(DATA).join("project");
    assert_eq!(Format::detect(&dir), Some(Format::CapCut));
    assert_eq!(
        Format::detect(&Path::new(DATA).join("features.json")),
        Some(Format::CapCut)
    );
    assert_eq!("jianying".parse::<Format>().ok(), Some(Format::CapCut));
    let r = ffcine_import::import(&dir, None, &ImportOptions::default()).expect("import dir");
    assert_eq!(r.timeline.name, "project");
    assert_eq!(
        r.timeline.ffcine_value("frame_size"),
        Some(&json!([1280, 720]))
    );
    let c = r.timeline.clips();
    assert_eq!(c.len(), 1);
    let range = c[0].source_range.as_ref().expect("range");
    assert_eq!((range.duration.value, range.duration.rate), (48.0, 24.0));
    assert_eq!(c[0].ffcine_id(), Some("capcut-S1"));
}

#[test]
fn strict_vs_lenient() {
    let path = Path::new(DATA).join("broken.json");
    let r = ffcine_import::import(&path, None, &ImportOptions::default()).expect("lenient");
    assert_eq!(r.report.skipped.len(), 3, "{:?}", r.report.skipped);
    assert!(r.report.skipped.iter().any(|s| s.contains("\"nope\"")));
    assert!(r.report.skipped.iter().any(|s| s.contains("\"missing\"")));
    assert!(r.report.skipped.iter().any(|s| s.contains("hologram")));
    assert_eq!(r.timeline.clips().len(), 1);
    assert_eq!(
        r.timeline.ffcine_value("frame_size"),
        Some(&json!([1080, 1920]))
    );

    let strict = ImportOptions {
        strict: true,
        ..ImportOptions::default()
    };
    let e = ffcine_import::import(&path, None, &strict).expect_err("strict");
    assert!(e.to_string().contains("missing"), "{e}");
}

#[test]
fn encrypted_drafts_are_refused() {
    let dir = std::env::temp_dir().join(format!("ffcine-capcut-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp");
    let p = dir.join("draft_content.json");
    std::fs::write(&p, [0x8a, 0x01, 0xff, 0x10, 0x42]).expect("write");
    let e = capcut::import(&dir, &ImportOptions::default()).expect_err("encrypted");
    assert!(e.to_string().contains("encrypted"), "{e}");
    std::fs::remove_dir_all(&dir).ok();
}
