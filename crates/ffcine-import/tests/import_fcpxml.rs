//! FCPXML importer on hand-written fixtures and OTIO's `fcpx_example.fcpxml`.

#![allow(clippy::many_single_char_names)]

use std::path::Path;

use ffcine_import::{ImportOptions, fcpxml};
use ffcine_timeline::{
    Clip, Effect, HasMetadata, Item, MediaReference, Stack, Timeline, Track, track_kind,
};
use serde_json::{Value, json};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fcpxml");

fn load(name: &str) -> (Timeline, ffcine_import::ImportReport) {
    let r = fcpxml::import(&Path::new(DATA).join(name), &ImportOptions::default())
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    // Whatever we build must survive the OTIO JSON round trip.
    let json = r.timeline.to_json().expect("to_json");
    assert_eq!(Timeline::from_json(&json).expect("from_json"), r.timeline);
    (r.timeline, r.report)
}

fn track<'a>(t: &'a Timeline, name: &str) -> &'a Track {
    t.track_list()
        .find(|x| x.name == name)
        .unwrap_or_else(|| panic!("no track {name}"))
}

fn label(i: &Item) -> String {
    match i {
        Item::Transition(_) => "T".into(),
        Item::Gap(_) => "-".into(),
        _ => i.name().into(),
    }
}

fn labels(t: &Track) -> Vec<String> {
    t.children.iter().map(label).collect()
}

fn clip<'a>(t: &'a Track, name: &str) -> &'a Clip {
    t.children
        .iter()
        .find_map(|i| match i {
            Item::Clip(c) if c.name == name => Some(c),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no clip {name}"))
}

fn ffcine_fx<'a>(c: &'a Clip, name: &str) -> &'a serde_json::Map<String, Value> {
    c.effects
        .iter()
        .find_map(|e| match e {
            Effect::Effect(g) if g.effect_name == name => g.ffcine(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{}: no {name}", c.name))
}

fn v(x: &serde_json::Map<String, Value>) -> Value {
    Value::Object(x.clone())
}

#[test]
fn features_editorial() {
    let (t, report) = load("features.fcpxml");
    assert_eq!(t.name, "Features");
    let names: Vec<_> = t.track_list().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["V1", "V2", "A1", "A2"]);
    assert_eq!(track(&t, "A1").kind, track_kind::AUDIO);

    let v1 = track(&t, "V1");
    assert_eq!(labels(v1), ["beach", "T", "street", "-", "T", "slowmo"]);
    let Item::Transition(x) = &v1.children[1] else {
        unreachable!()
    };
    assert_eq!((x.in_offset.value, x.out_offset.value), (25.0, 25.0));
    assert!(x.ffcine_value("kind").is_none());
    let Item::Transition(slide) = &v1.children[4] else {
        unreachable!()
    };
    assert_eq!(slide.ffcine_value("kind"), Some(&json!("Slide")));
    assert_eq!(
        slide.ffcine_value("schema"),
        Some(&json!("ffcine.transition/1"))
    );

    let beach = clip(v1, "beach");
    let r = beach.source_range.as_ref().expect("range");
    assert_eq!(
        (r.start_time.value, r.duration.value, r.start_time.rate),
        (90250.0, 250.0, 25.0)
    );
    let Some(MediaReference::External(m)) = beach.media_reference() else {
        panic!("beach media")
    };
    assert_eq!(m.target_url, "file:///media/beach.mov");
    assert_eq!(
        m.ffcine_value("source"),
        Some(&json!({"uid": "A1", "reel": "A001"}))
    );
    assert!(beach.ffcine_id().is_some_and(|i| i.starts_with("fcpx-")));
    assert_eq!(beach.markers.len(), 2);
    assert_eq!(beach.markers[0].name, "Wave");
    assert_eq!(beach.markers[0].marked_range.start_time.value, 90300.0);
    assert_eq!(
        beach.markers[0].extra.get("comment"),
        Some(&json!("big one"))
    );
    assert_eq!(beach.markers[1].extra.get("color"), Some(&json!("ORANGE")));

    // Title on V2 after a 1 s gap.
    let v2 = track(&t, "V2");
    assert_eq!(labels(v2), ["-", "Hello"]);
    let title = clip(v2, "Hello");
    let Some(MediaReference::Generator(g)) = title.media_reference() else {
        panic!("title media")
    };
    assert_eq!(g.generator_kind, "ffcine.text");
    assert_eq!(g.parameters["text"], json!("Hello beach"));
    assert_eq!(g.parameters["font"], json!("Helvetica"));
    assert_eq!(g.parameters["size"], json!(72.0));
    assert_eq!(g.parameters["colour"], json!([1.0, 0.9, 0.2, 1.0]));
    assert_eq!(g.parameters["bold"], json!(true));
    let pos = g.parameters["position"].as_array().expect("position");
    assert!((pos[1].as_f64().unwrap_or_default() - 400.0 / 1080.0).abs() < 1e-9);

    // Music under the whole first clip; the street clip's detached audio on a second lane.
    assert_eq!(labels(track(&t, "A1")), ["music"]);
    let a2 = track(&t, "A2");
    assert_eq!(labels(a2), ["-", "street"]);
    let street_a = clip(a2, "street");
    let r = street_a.source_range.as_ref().expect("range");
    assert_eq!((r.start_time.value, r.duration.value), (50.0, 200.0));

    assert!(report.unmapped.iter().any(|u| u == "adjust-stabilization"));
    assert!(report.unmapped.iter().any(|u| u.contains("Gaussian Blur")));
    assert!(report.warnings.iter().any(|w| w.contains("Slide")));
}

#[test]
fn features_effects() {
    let (t, _) = load("features.fcpxml");
    let beach = clip(track(&t, "V1"), "beach");
    let xf = ffcine_fx(beach, "ffcine.geo.transform");
    assert_eq!(xf["schema"], json!("ffcine.geo.transform/1"));
    assert_eq!((&xf["units"], &xf["y"]), (&json!("frame"), &json!("down")));
    assert_eq!((&xf["scale_x"], &xf["scale_y"]), (&json!(1.2), &json!(1.2)));
    assert_eq!(xf["rotate_deg"], json!(15.0));
    // Keyframe times are in the clip's source time: its in point plus the frame offset.
    let start = beach.source_range.as_ref().expect("range").start_time.value;
    assert_eq!(
        xf["translate_x"],
        json!([
            {"t": {"OTIO_SCHEMA": "RationalTime.1", "rate": 25.0, "value": start}, "v": 0.0, "interp": "linear"},
            {"t": {"OTIO_SCHEMA": "RationalTime.1", "rate": 25.0, "value": start + 125.0}, "v": 0.05625, "interp": "ease"},
        ])
    );
    assert_eq!(xf["translate_y"][1]["v"], json!(0.05));
    let crop = ffcine_fx(beach, "ffcine.geo.crop");
    assert!((crop["left"].as_f64().unwrap_or_default() - 0.05625).abs() < 1e-9);
    assert_eq!(crop["top"], json!(0.05));
    assert_eq!(
        v(ffcine_fx(beach, "ffcine.comp.blend")),
        json!({"schema": "ffcine.comp.blend/1", "opacity": 0.8, "mode": "multiply"})
    );
    let level = ffcine_fx(beach, "ffcine.audio.level");
    assert_eq!(level["gain_db"][0]["v"], json!(-96.0));
    assert_eq!(level["gain_db"][1]["t"]["value"], json!(start + 25.0));
    let cdl = ffcine_fx(beach, "ffcine.color.cdl");
    assert_eq!(cdl["slope"], json!([1.1, 1.0, 0.9]));
    assert_eq!(
        beach.metadata["cdl"],
        json!({"asc_sop": {"slope": [1.1, 1.0, 0.9], "offset": [0.01, 0.0, -0.01], "power": [1.0, 1.0, 1.05]}})
    );
    // The blur is kept as a foreign effect.
    let blur = beach
        .effects
        .iter()
        .find(|e| e.effect_name() == Some("FFGaussianBlur"))
        .expect("blur");
    assert_eq!(
        blur.foreign("fcpx").and_then(|f| f.get("name")),
        Some(&json!("filter-video"))
    );

    // conform-rate 50 -> 25: half speed.
    let street = clip(track(&t, "V1"), "street");
    assert!(street.effects.iter().any(|e| matches!(e,
        Effect::LinearTimeWarp(w) if (w.time_scalar - 0.5).abs() < 1e-9)));

    // timeMap ramp: curve + average speed.
    let slowmo = clip(track(&t, "V1"), "slowmo");
    let ramp = ffcine_fx(slowmo, "ffcine.time.ramp");
    // 0.5x for the first half of the clip (a quarter of the source), then 1.5x.
    assert_eq!(
        v(ramp),
        json!({"schema": "ffcine.time.ramp/1",
               "points": [[0.0, 0.5], [0.25, 0.5], [0.25, 1.5], [1.0, 1.5]],
               "source_duration": 10.0})
    );
    assert!(slowmo.effects.iter().any(|e| matches!(e,
        Effect::LinearTimeWarp(w) if (w.time_scalar - 1.0).abs() < 1e-9)));
    // Unmapped adjustment kept on the clip.
    assert_eq!(
        slowmo
            .foreign("fcpx")
            .and_then(|f| f["children"][0].get("name")),
        Some(&json!("adjust-stabilization"))
    );
}

#[test]
fn bundle_directory() {
    let (t, _) = load("bundle.fcpxmld");
    assert_eq!(t.name, "Bundle Project");
    let gs = t.global_start_time.as_ref().expect("start");
    assert!((gs.to_seconds() - 3600.0).abs() < 1e-9);
    let v1 = track(&t, "V1");
    assert_eq!(labels(v1), ["A001", "A001 again"]);
    let again = clip(v1, "A001 again");
    assert!(!again.enabled);
    let r = again.source_range.as_ref().expect("range");
    assert!((r.start_time.value - 500.0).abs() < 1e-6);
    assert_eq!(t.ffcine_value("frame_size"), Some(&json!([3840, 2160])));
}

fn count(items: &[Item], f: &dyn Fn(&Item) -> bool) -> usize {
    items
        .iter()
        .map(|i| {
            usize::from(f(i))
                + match i {
                    Item::Track(t) => count(&t.children, f),
                    Item::Stack(Stack { children, .. }) => count(children, f),
                    _ => 0,
                }
        })
        .sum()
}

#[test]
fn otio_fcpx_example() {
    let (t, report) = load("fcpx_example.fcpxml");
    assert_eq!(t.name, "OpenTimeline_Project");
    assert!(report.warnings.iter().any(|w| w.contains("2 projects")));
    // The first project has 3 transitions (1 on the primary storyline, 2 on a connected
    // storyline); the file has 6 in all, across two projects.
    let kids = &t.tracks.children;
    assert_eq!(count(kids, &|i| matches!(i, Item::Transition(_))), 3);
    // Their filters are kept on the transitions.
    let trans: Vec<_> = t
        .track_list()
        .flat_map(|x| &x.children)
        .filter_map(|i| match i {
            Item::Transition(x) => Some(x),
            _ => None,
        })
        .collect();
    assert!(
        trans
            .iter()
            .all(|x| x.foreign("fcpx").and_then(|f| f.get("children")).is_some())
    );
    // Compound clips become nested stacks: 2 ref-clips plus the audio-only connected one.
    assert_eq!(count(kids, &|i| matches!(i, Item::Stack(_))), 3);
    // Every clip has an id, ids are unique.
    let clips = t.clips();
    assert!(clips.len() > 10);
    let mut ids: Vec<_> = clips.iter().filter_map(|c| c.ffcine_id()).collect();
    assert_eq!(ids.len(), clips.len());
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), clips.len());
    // Markers land on clips.
    let markers: usize = clips.iter().map(|c| c.markers.len()).sum();
    assert!(markers >= 5);
    // V1 is the primary storyline and lasts the whole sequence (21602243/144000 s at 30 fps).
    let v1 = track(&t, "V1");
    let dur: f64 = v1
        .children
        .iter()
        .filter_map(|i| i.source_range().map(|r| r.duration.value))
        .sum();
    assert!((dur - 4500.0).abs() < 1.0, "V1 is {dur} frames");
}
