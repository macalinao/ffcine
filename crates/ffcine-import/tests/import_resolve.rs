//! Resolve native OTIO import, on hand-built fixtures in the shape Resolve 21 writes.

#![allow(clippy::needless_pass_by_value, clippy::items_after_statements)]

use std::path::{Path, PathBuf};

use ffcine_import::resolve::{self, APP, NAMESPACE, PRESERVED_EFFECTS, PRESERVED_PARAMETERS};
use ffcine_import::{self as imp, Format, ImportOptions, ImportReport};
use ffcine_timeline::Timeline;
use serde_json::{Value, json};

const CORE: [&str; 17] = [
    "Timeline",
    "Stack",
    "Track",
    "Clip",
    "Gap",
    "Transition",
    "Effect",
    "LinearTimeWarp",
    "FreezeFrame",
    "TimeEffect",
    "ExternalReference",
    "MissingReference",
    "GeneratorReference",
    "ImageSequenceReference",
    "Marker",
    "RationalTime",
    "TimeRange",
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/resolve")
        .join(name)
}

fn input(name: &str) -> Value {
    let text = std::fs::read_to_string(fixture(name)).expect("fixture");
    serde_json::from_str(&text).expect("fixture json")
}

fn opts() -> ImportOptions {
    ImportOptions {
        frame_size: Some((1920, 1080)),
        ..ImportOptions::default()
    }
}

/// An import as JSON, checked against the typed model.
struct Out {
    timeline: Value,
    report: ImportReport,
}

fn run(path: &Path, opts: &ImportOptions) -> Out {
    let raw: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json");
    let (json, report) = resolve::import_json(raw.clone(), opts).expect("import");
    let typed = resolve::import(path, opts).expect("import");
    assert_eq!(typed.report, report);
    // The model keeps everything: unknown fields, foreign blocks, ffcine effects.
    let timeline = serde_json::to_value(&typed.timeline).expect("to_value");
    assert_eq!(timeline, json);
    let text = typed.timeline.to_json().expect("to_json");
    assert_eq!(
        Timeline::from_json(&text).expect("from_json"),
        typed.timeline
    );
    check_invariants(&raw, &timeline);
    Out { timeline, report }
}

fn load(name: &str) -> Out {
    run(&fixture(name), &opts())
}

/// Every object with an `OTIO_SCHEMA`, depth first, skipping metadata.
fn objects(v: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    fn go<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
        match v {
            Value::Object(o) => {
                if o.contains_key("OTIO_SCHEMA") {
                    out.push(v);
                }
                for (k, c) in o {
                    if k != "metadata" {
                        go(c, out);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|c| go(c, out)),
            _ => {}
        }
    }
    go(v, &mut out);
    out
}

fn schema(v: &Value) -> &str {
    v["OTIO_SCHEMA"]
        .as_str()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("")
}

fn named<'a>(tl: &'a Value, kind: &str, name: &str) -> &'a Value {
    objects(tl)
        .into_iter()
        .find(|o| schema(o) == kind && o["name"] == name)
        .unwrap_or_else(|| panic!("no {kind} {name}"))
}

fn clip<'a>(tl: &'a Value, name: &str) -> &'a Value {
    named(tl, "Clip", name)
}

/// `metadata.ffcine` of a clip's effects named `effect_name`.
fn ff<'a>(clip: &'a Value, effect_name: &str) -> Vec<&'a Value> {
    clip["effects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["effect_name"] == effect_name)
        .map(|e| &e["metadata"]["ffcine"])
        .collect()
}

fn one<'a>(clip: &'a Value, effect_name: &str) -> &'a Value {
    let found = ff(clip, effect_name);
    assert_eq!(
        found.len(),
        1,
        "{effect_name} on {}: {found:?}",
        clip["name"]
    );
    found[0]
}

fn t(v: f64) -> Value {
    json!({ "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": v })
}

fn key(at: f64, v: Value, interp: &str) -> Value {
    json!({ "t": t(at), "v": v, "interp": interp })
}

fn has_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(o) => o.contains_key(key) || o.values().any(|c| has_key(c, key)),
        Value::Array(a) => a.iter().any(|c| has_key(c, key)),
        _ => false,
    }
}

/// Every `Resolve_OTIO` namespace in `input`, verbatim.
fn namespaces(input: &Value) -> Vec<Value> {
    fn go(v: &Value, out: &mut Vec<Value>) {
        match v {
            Value::Object(o) => {
                if let Some(ns) = o.get("metadata").and_then(|m| m.get(NAMESPACE)) {
                    out.push(ns.clone());
                }
                if let Some(ns) = o.get("parameters").and_then(|m| m.get(NAMESPACE)) {
                    out.push(ns.clone());
                }
                o.values().for_each(|c| go(c, out));
            }
            Value::Array(a) => a.iter().for_each(|c| go(c, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    go(input, &mut out);
    out
}

/// Everything stored under any `metadata.ffcine.foreign`.
fn foreign(output: &Value) -> Vec<Value> {
    fn go(v: &Value, out: &mut Vec<Value>) {
        match v {
            Value::Object(o) => {
                if let Some(f) = o.get("ffcine").and_then(|f| f.get("foreign")) {
                    for k in [APP, PRESERVED_EFFECTS, PRESERVED_PARAMETERS] {
                        if let Some(v) = f.get(k) {
                            out.push(v.clone());
                            if let Some(a) = v.as_array() {
                                out.extend(a.iter().cloned());
                            }
                        }
                    }
                }
                o.values().for_each(|c| go(c, out));
            }
            Value::Array(a) => a.iter().for_each(|c| go(c, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    go(output, &mut out);
    out
}

fn check_invariants(input: &Value, output: &Value) {
    assert!(!has_key(output, NAMESPACE), "a Resolve_OTIO key is left");
    for o in objects(output) {
        assert!(
            CORE.contains(&schema(o)),
            "non-core schema {}",
            o["OTIO_SCHEMA"]
        );
    }
    let kept = foreign(output);
    for ns in namespaces(input) {
        assert!(kept.contains(&ns), "not preserved verbatim: {ns}");
    }
    let text = serde_json::to_string(output).expect("to_json");
    let back: Value = serde_json::from_str(&text).expect("from_json");
    assert_eq!(&back, output);
}

#[test]
fn reframe_modes() {
    let out = load("reframe.otio");
    let tl = &out.timeline;
    let mode = |n: &str| one(clip(tl, n), "ffcine.geo.reframe")["mode"].clone();
    assert_eq!(mode("scale0.mov"), "fit");
    assert_eq!(mode("scale1.mov"), "crop");
    assert_eq!(mode("scale2.mov"), "fit");
    assert_eq!(mode("scale3.mov"), "fill");
    assert_eq!(mode("scale4.mov"), "stretch");
    let crop = one(clip(tl, "scale1.mov"), "ffcine.geo.reframe");
    assert_eq!(crop["timeline_size"], json!([1920, 1080]));
    assert_eq!(crop["schema"], "ffcine.geo.reframe/1");
    assert_eq!(ff(clip(tl, "unscaled.mov"), "ffcine.geo.reframe").len(), 0);
    let warned = |s: &str| out.report.warnings.iter().any(|w| w.contains(s));
    assert!(warned("project setting"), "{:?}", out.report.warnings);
    assert!(!warned("--frame-size"));
    // Reframe comes before the transform it sizes.
    let names: Vec<_> = clip(tl, "scale3.mov")["effects"]
        .as_array()
        .expect("effects")
        .iter()
        .map(|e| e["effect_name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, ["ffcine.geo.reframe", "ffcine.geo.transform"]);

    let bare = run(&fixture("reframe.otio"), &ImportOptions::default());
    assert!(
        bare.report
            .warnings
            .iter()
            .any(|w| w.contains("--frame-size"))
    );
    assert!(
        one(clip(&bare.timeline, "scale1.mov"), "ffcine.geo.reframe")
            .get("timeline_size")
            .is_none()
    );
}

#[test]
fn transform_constants() {
    let out = load("transform.otio");
    let tf = one(clip(&out.timeline, "const.mov"), "ffcine.geo.transform");
    assert_eq!(
        *tf,
        json!({
            "schema": "ffcine.geo.transform/1",
            "units": "picture",
            "y": "down",
            "anchor": [0.25, -0.125],
            "translate_x": 0.2,
            "translate_y": -0.1,
            "scale_x": 1.5,
            "scale_y": 1.25,
            "rotate_deg": 30.0,
            "flip_x": true,
            "flip_y": true,
            "foreign": { "resolve": input("transform.otio")["tracks"]["children"][0]["children"][0]["effects"][0]["metadata"][NAMESPACE] },
        })
    );
    assert!(
        out.report
            .unmapped
            .contains(&"Transform.transformationPitch".to_owned())
    );
}

#[test]
fn transform_keyframes() {
    let out = load("transform.otio");
    let tf = one(clip(&out.timeline, "keyed.mov"), "ffcine.geo.transform");
    // Keys are in the clip's source time: start_time 100 + Resolve's frame.
    assert_eq!(
        tf["scale_x"],
        json!([
            key(100.0, json!(1.0), "ease"),
            key(124.0, json!(2.0), "linear")
        ])
    );
    assert_eq!(
        tf["scale_y"],
        json!([
            key(100.0, json!(1.0), "linear"),
            key(124.0, json!(2.0), "linear")
        ])
    );
    assert_eq!(
        tf["translate_y"],
        json!([key(112.0, json!(-0.5), "linear")])
    );
    assert_eq!(
        tf["anchor"],
        json!([
            key(100.0, json!([0.0, 0.0]), "linear"),
            key(124.0, json!([0.1, -0.2]), "linear")
        ])
    );
    assert!(tf.get("translate_x").is_none());
    assert!(
        out.report
            .warnings
            .iter()
            .any(|w| w.contains("transformationZoomX: Bezier"))
    );
}

#[test]
fn crop_sides_and_softness() {
    let out = load("crop.otio");
    let crop = one(clip(&out.timeline, "crop.mov"), "ffcine.geo.crop");
    assert_eq!(crop["units"], "picture");
    assert_eq!(crop["left"], 0.1);
    assert_eq!(crop["right"], 0.2);
    assert_eq!(crop["top"], 0.05);
    assert_eq!(
        crop["bottom"],
        json!([
            key(0.0, json!(0.0), "linear"),
            key(24.0, json!(0.3), "linear")
        ])
    );
    assert_eq!(crop["feather"], 0.02);
    assert!(
        out.report
            .warnings
            .iter()
            .any(|w| w.contains("cropSoftness"))
    );
}

#[test]
fn composite_opacity_modes_and_faders() {
    let out = load("composite.otio");
    let tl = &out.timeline;
    let blend = |n: &str| one(clip(tl, n), "ffcine.comp.blend");
    assert_eq!(blend("half.mov")["opacity"], 0.5);
    assert!(blend("half.mov").get("mode").is_none());
    assert_eq!(
        blend("keyed.mov")["opacity"],
        json!([
            key(0.0, json!(0.0), "ease"),
            key(12.0, json!(1.0), "linear"),
            key(36.0, json!(1.0), "linear"),
            key(47.0, json!(0.0), "linear"),
        ])
    );
    assert_eq!(blend("screen.mov")["mode"], "screen");
    assert!(blend("screen.mov").get("opacity").is_none());
    assert_eq!(blend("darker.mov")["mode"], "darker_color");
    assert_eq!(blend("alphakey.mov")["opacity"], 0.8);
    assert!(blend("alphakey.mov").get("mode").is_none());
    assert!(
        out.report
            .unmapped
            .contains(&"Composite.composite mode=alpha".to_owned())
    );
    assert_eq!(
        blend("faded.mov")["opacity"],
        json!([
            key(0.0, json!(0.0), "linear"),
            key(12.0, json!(0.8), "linear"),
            key(42.0, json!(0.8), "linear"),
            key(48.0, json!(0.0), "linear"),
        ])
    );
    // Composite and Video Faders both fed it: both blocks are kept, in order.
    let kept = blend("faded.mov")["foreign"]["resolve"]
        .as_array()
        .expect("two blocks");
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0]["Effect Name"], "Composite");
    assert_eq!(kept[1]["Effect Name"], "Video Faders");
    assert_eq!(
        blend("fadeonly.mov")["opacity"],
        json!([
            key(0.0, json!(0.0), "linear"),
            key(24.0, json!(1.0), "linear")
        ])
    );
    // Empty sections stay on the clip, compactly, and make no ffcine effect.
    let half = clip(tl, "half.mov");
    assert_eq!(half["effects"].as_array().map(Vec::len), Some(1));
    let rest = half["metadata"]["ffcine"]["foreign"][PRESERVED_EFFECTS]
        .as_array()
        .expect("kept");
    let names: Vec<_> = rest
        .iter()
        .map(|b| b["Effect Name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        names,
        [
            "Transform",
            "Cropping",
            "Dynamic Zoom",
            "Lens Correction",
            "Retime and Scaling",
            "Video Faders"
        ]
    );
}

#[test]
fn dynamic_zoom() {
    let out = load("dynamic_zoom.otio");
    let tf = one(clip(&out.timeline, "kenburns.mov"), "ffcine.geo.transform");
    // Start (key -90): box (0, 0) at 1. End (key 910): box (0.1, 0.2 up) at 0.5 -> zoom 2,
    // translate -c'/s = (-0.2, +0.4). Keyed from the clip's start (source 90) to its end (138).
    assert_eq!(
        tf["scale_x"],
        json!([
            key(90.0, json!(1.0), "linear"),
            key(138.0, json!(2.0), "linear")
        ])
    );
    assert_eq!(tf["scale_y"], tf["scale_x"]);
    assert_eq!(
        tf["translate_x"],
        json!([
            key(90.0, json!(0.0), "linear"),
            key(138.0, json!(-0.2), "linear")
        ])
    );
    assert_eq!(
        tf["translate_y"],
        json!([
            key(90.0, json!(0.0), "linear"),
            key(138.0, json!(0.4), "linear")
        ])
    );
    assert!(
        out.report
            .warnings
            .iter()
            .any(|w| w.contains("Dynamic Zoom"))
    );
    // Disabled: nothing mapped, the block stays.
    let off = clip(&out.timeline, "disabled.mov");
    assert_eq!(ff(off, "ffcine.geo.transform").len(), 0);
    assert!(
        off["metadata"]["ffcine"]["foreign"][PRESERVED_EFFECTS]
            .as_array()
            .expect("kept")
            .iter()
            .any(|b| b["Effect Name"] == "Dynamic Zoom" && b["Enabled"] == false)
    );
}

#[test]
fn audio_level_and_pan() {
    let out = load("audio.otio");
    let tl = &out.timeline;
    let quiet = clip(tl, "quiet.wav");
    assert_eq!(one(quiet, "ffcine.audio.level")["gain_db"], -6.0);
    assert_eq!(quiet["metadata"]["ffcine"]["link_group"], 1);
    assert_eq!(
        one(clip(tl, "ride.wav"), "ffcine.audio.level")["gain_db"],
        json!([
            key(0.0, json!(-20.0), "ease"),
            key(24.0, json!(0.0), "linear")
        ])
    );
    let fades = one(clip(tl, "fades.wav"), "ffcine.audio.level");
    assert!(fades.get("gain_db").is_none());
    assert_eq!(fades["fade_in"], 0.5);
    assert_eq!(fades["fade_out"], 1.0);
    let panned = clip(tl, "panned.wav");
    assert_eq!(one(panned, "ffcine.audio.pan")["pan"], -0.5);
    assert!(
        out.report
            .unmapped
            .contains(&"Fairlight Clip Pitch".to_owned())
    );
    assert!(
        !out.report.unmapped.iter().any(|u| u.contains("Equaliser")),
        "{:?}",
        out.report.unmapped
    );
}

#[test]
fn transitions() {
    let out = load("transitions.otio");
    let tl = &out.timeline;
    let ffm = |n: &str| named(tl, "Transition", n)["metadata"]["ffcine"].clone();
    let dissolve = named(tl, "Transition", "Cross Dissolve");
    assert_eq!(dissolve["transition_type"], "SMPTE_Dissolve");
    assert_eq!(dissolve["in_offset"], t(12.0));
    assert_eq!(dissolve["out_offset"], t(12.0));
    assert_eq!(ffm("Cross Dissolve")["kind"], "dissolve");
    assert_eq!(ffm("Cross Dissolve")["ease"], "in_out");
    assert_eq!(ffm("Cross Dissolve")["schema"], "ffcine.transition/1");
    assert_eq!(ffm("Edge Wipe")["kind"], "wipe");
    assert_eq!(ffm("Edge Wipe")["angle_deg"], 90.0);
    assert!(ffm("Push").get("kind").is_none());
    assert!(out.report.unmapped.contains(&"Transition.Push".to_owned()));
    let xf = named(tl, "Transition", "Cross Fade 0 dB");
    assert_eq!(xf["in_offset"], t(30.0));
    assert_eq!(ffm("Cross Fade 0 dB")["kind"], "cross_fade");
    assert_eq!(ffm("Cross Fade 0 dB")["crossfade_db"], 0.0);
    assert_eq!(ffm("Cross Fade +3 dB")["crossfade_db"], 3.0);
}

#[test]
fn speed_markers_references_compounds_generators() {
    let out = load("misc.otio");
    let tl = &out.timeline;
    let raw = input("misc.otio");

    let fast = clip(tl, "fast.mov");
    assert_eq!(
        fast["effects"][0],
        raw["tracks"]["children"][0]["children"][0]["effects"][0]
    );
    assert_eq!(fast["effects"][0]["time_scalar"], 2.0);
    assert_eq!(fast["effects"][1]["effect_name"], "ffcine.comp.blend");
    assert!(out.report.warnings.iter().any(|w| w.contains("retimed")));
    assert_eq!(fast["x_unknown_clip_field"], json!({ "kept": true }));
    let marker = &fast["markers"][0];
    assert_eq!(marker["color"], "RED");
    assert_eq!(
        marker["metadata"]["ffcine"]["foreign"]["resolve"]["Note"],
        "check focus"
    );

    assert_eq!(
        clip(tl, "frozen.mov")["effects"][0]["OTIO_SCHEMA"],
        "FreezeFrame.1"
    );

    let missing = clip(tl, "clip.mp4");
    let r = &missing["media_references"]["DEFAULT_MEDIA"];
    assert_eq!(r["OTIO_SCHEMA"], "MissingReference.1");
    assert_eq!(
        r["metadata"]["ffcine"]["foreign"]["resolve"]["Media Url"],
        "/Volumes/A/clip.mp4"
    );
    assert_eq!(
        missing["metadata"]["ffcine"]["source"]["src"],
        "/Volumes/A/clip.mp4"
    );
    assert_eq!(missing["metadata"]["ffcine"]["link_group"], 3);
    assert!(out.report.unmapped.contains(&"Lens Correction".to_owned()));

    let compound = named(tl, "Stack", "Compound Clip 1");
    assert_eq!(one(compound, "ffcine.comp.blend")["opacity"], 0.25);
    assert_eq!(
        compound["metadata"]["ffcine"]["foreign"]["resolve"]["Sequence Type"],
        "Compound Clip"
    );

    let text = clip(tl, "Text");
    let g = &text["media_references"]["DEFAULT_MEDIA"];
    assert!(g["parameters"].get(NAMESPACE).is_none());
    assert_eq!(
        g["metadata"]["ffcine"]["foreign"][PRESERVED_PARAMETERS][0]["Effect Name"],
        "Rich Text"
    );
    assert!(
        out.report
            .unmapped
            .contains(&"Generator.Rich Text".to_owned())
    );

    assert_eq!(tl["x_unknown_timeline_field"], json!([1, 2, 3]));
    assert_eq!(
        tl["metadata"]["ffcine"]["foreign"]["resolve"]["Resolve OTIO Meta Version"],
        "1.0"
    );
    assert_eq!(
        named(tl, "Track", "Video 1")["metadata"]["ffcine"]["foreign"]["resolve"],
        json!({ "Locked": false })
    );
}

#[test]
fn combined() {
    let out = load("combined.otio");
    let tl = &out.timeline;
    let a = clip(tl, "a.mov");
    let names: Vec<_> = a["effects"]
        .as_array()
        .expect("effects")
        .iter()
        .map(|e| e["effect_name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        names,
        [
            "ffcine.geo.reframe",
            "ffcine.geo.crop",
            "ffcine.geo.transform",
            "ffcine.comp.blend"
        ]
    );
    let blend = one(a, "ffcine.comp.blend");
    assert_eq!(blend["mode"], "multiply");
    assert_eq!(
        blend["opacity"],
        json!([
            key(84.0, json!(0.9), "linear"),
            key(96.0, json!(0.0), "linear")
        ])
    );
    let b = clip(tl, "b.mov");
    assert_eq!(b["effects"][0]["OTIO_SCHEMA"], "LinearTimeWarp.1");
    assert_eq!(one(b, "ffcine.geo.reframe")["mode"], "crop");
    assert_eq!(ff(b, "ffcine.geo.transform").len(), 1);
    let logo = clip(tl, "logo.png");
    assert_eq!(one(logo, "ffcine.geo.transform")["flip_x"], true);
    assert!(
        out.report
            .unmapped
            .contains(&"Composite.composite mode=inverted_alpha".to_owned())
    );
    let wav = clip(tl, "a.wav");
    assert_eq!(one(wav, "ffcine.audio.level")["gain_db"], -3.0);
    assert_eq!(one(wav, "ffcine.audio.level")["fade_in"], 0.25);
    assert_eq!(one(wav, "ffcine.audio.pan")["pan"], 0.25);
    assert_eq!(wav["metadata"]["ffcine"]["link_group"], 1);
}

#[test]
fn rejects_non_timelines() {
    assert!(resolve::import_str("{\"OTIO_SCHEMA\": \"Clip.2\"}", &opts()).is_err());
    assert!(resolve::import_str("not json", &opts()).is_err());
    assert!(resolve::is_resolve(&input("combined.otio")));
}

#[test]
fn format_dispatch() {
    assert_eq!(
        "resolve".parse::<Format>().expect("format"),
        Format::Resolve
    );
    assert!(resolve::detect(&fixture("combined.otio")));
    // A plain .otio of Resolve's goes through the Resolve importer.
    let auto = imp::import(&fixture("combined.otio"), None, &opts()).expect("import");
    let otio = imp::import(&fixture("combined.otio"), Some(Format::Otio), &opts()).expect("import");
    let explicit =
        imp::import(&fixture("combined.otio"), Some(Format::Resolve), &opts()).expect("import");
    assert_eq!(auto.timeline, explicit.timeline);
    assert_eq!(otio.timeline, explicit.timeline);
    assert_ne!(auto.report.unmapped.len(), 0);
}

/// A real Resolve export, e.g. Skynse/Fig `bench/otio/effects.otio` (GPL, not in the repo):
/// `FFCINE_RESOLVE_SAMPLE=/path/effects.otio cargo test -p ffcine --test import_resolve`
/// (`FFCINE_RESOLVE_DUMP=out.otio` writes the import).
#[test]
fn real_export() {
    let Some(path) = std::env::var_os("FFCINE_RESOLVE_SAMPLE") else {
        eprintln!("FFCINE_RESOLVE_SAMPLE unset: skipped");
        return;
    };
    let path = PathBuf::from(path);
    let out = run(&path, &opts());
    if let Some(dump) = std::env::var_os("FFCINE_RESOLVE_DUMP") {
        let text = serde_json::to_string_pretty(&out.timeline).expect("to_json");
        std::fs::write(dump, text).expect("dump");
    }
    let all = objects(&out.timeline);
    let clips: Vec<_> = all.iter().filter(|o| schema(o) == "Clip").collect();
    assert_ne!(clips.len(), 0);
    let effects = |name: &str| clips.iter().flat_map(|c| ff(c, name)).count();
    eprintln!(
        "{} clips; transforms {}, blends {}, levels {}; warnings {:?}; unmapped {:?}",
        clips.len(),
        effects("ffcine.geo.transform"),
        effects("ffcine.comp.blend"),
        effects("ffcine.audio.level"),
        out.report.warnings,
        out.report.unmapped
    );
    // Every effect is either an ffcine one or core OTIO; no Resolve Effect wrapper is left.
    for c in &clips {
        for e in c["effects"].as_array().into_iter().flatten() {
            let name = e["effect_name"].as_str().unwrap_or("");
            assert!(name.starts_with("ffcine.") || schema(e) != "Effect", "{e}");
        }
    }
    // Keyframes land inside or near their clip and have sane values.
    for c in &clips {
        for tf in ff(c, "ffcine.geo.transform") {
            for k in ["scale_x", "scale_y", "translate_x", "translate_y"] {
                for key in tf[k].as_array().into_iter().flatten() {
                    let v = key["v"].as_f64().expect("number");
                    assert!(v.is_finite() && v.abs() < 100.0, "{k} {v}");
                    assert!(["linear", "ease"].contains(&key["interp"].as_str().unwrap_or("")));
                }
            }
        }
    }
}
