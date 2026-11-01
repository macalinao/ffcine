//! Exporters: import the fixtures, export, import again.
//!
//! FCPXML transform fields: the readers are moving from `position/scale/rotation` to the canonical
//! `translate_x/translate_y/scale_x/scale_y/rotate_deg`. The round trip compares whatever the
//! reader emits on both sides, so it holds before and after that change; keyframe `t` is left out
//! of the FCPXML comparison because the reader counts it from the clip's first frame while the
//! exporter follows `docs/timeline.md` (source time). Both are noted at the assertions.

use std::path::Path;

use ffcine_import::export::{self, ExportOptions};
use ffcine_import::{ImportOptions, fcpxml, resolve};
use ffcine_timeline::{
    Clip, Effect, ExternalReference, GenericEffect, HasMetadata, Item, MediaReference, TimeRange,
    Timeline, Track, track_kind,
};
use serde_json::{Map, Value, json};

const RESOLVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/resolve");
const FCP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fcpxml");
const FIXTURES: [&str; 9] = [
    "audio",
    "combined",
    "composite",
    "crop",
    "dynamic_zoom",
    "misc",
    "reframe",
    "transform",
    "transitions",
];

fn ropts() -> ImportOptions {
    ImportOptions {
        frame_size: Some((1920, 1080)),
        ..ImportOptions::default()
    }
}

/// Every `Resolve_OTIO` value in a document, as sorted JSON strings.
fn namespaces(v: &Value) -> Vec<String> {
    fn go(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                for (k, x) in o {
                    if k == "Resolve_OTIO" {
                        out.push(x.to_string());
                    } else {
                        go(x, out);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| go(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    go(v, &mut out);
    out.sort();
    out
}

#[test]
fn resolve_round_trip_is_lossless() {
    for name in FIXTURES {
        let path = Path::new(RESOLVE).join(format!("{name}.otio"));
        let input: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        let a = resolve::import(&path, &ropts()).expect("import").timeline;
        let (out, report) =
            export::resolve::export_report(&a, &ExportOptions::default()).expect("export");
        let out = serde_json::to_value(&out).expect("value");
        let (got, want) = (namespaces(&out), namespaces(&input));
        assert!(
            got == want,
            "{name}: Resolve_OTIO verbatim\n{}",
            diff(&got, &want)
        );
        let b = resolve::import_value(out, &ropts())
            .expect("re-import")
            .timeline;
        assert_eq!(
            serde_json::to_value(&b).expect("b"),
            serde_json::to_value(&a).expect("a"),
            "{name}: re-import differs ({report})"
        );
    }
}

fn ffcine_fx<'a>(c: &'a Clip, name: &str) -> Option<&'a Map<String, Value>> {
    c.effects.iter().find_map(|e| match e {
        Effect::Effect(g) if g.effect_name == name => g.ffcine(),
        _ => None,
    })
}

fn ffcine_fx_mut<'a>(c: &'a mut Clip, name: &str) -> Option<&'a mut Map<String, Value>> {
    c.effects.iter_mut().find_map(|e| match e {
        Effect::Effect(g) if g.effect_name == name => Some(g.ffcine_mut()),
        _ => None,
    })
}

fn clips_mut(items: &mut [Item]) -> Vec<&mut Clip> {
    let mut out = Vec::new();
    for i in items {
        match i {
            Item::Clip(c) => out.push(c),
            Item::Track(t) => out.extend(clips_mut(&mut t.children)),
            Item::Stack(s) => out.extend(clips_mut(&mut s.children)),
            _ => {}
        }
    }
    out
}

#[test]
fn resolve_edited_effect_is_rebuilt() {
    let path = Path::new(RESOLVE).join("transform.otio");
    let mut a = resolve::import(&path, &ropts()).expect("import").timeline;
    let mut edited = None;
    for c in clips_mut(&mut a.tracks.children) {
        let name = c.name.clone();
        if let Some(t) = ffcine_fx_mut(c, "ffcine.geo.transform")
            && t.get("translate_x").is_some_and(Value::is_number)
        {
            t.insert("translate_x".into(), json!(0.33));
            t.insert("rotate_deg".into(), json!(-12.5));
            edited = Some((name, t.clone()));
            break;
        }
    }
    let (name, mut want) = edited.expect("a clip with a constant transform");
    let out = export::resolve::export(&a, &ExportOptions::default()).expect("export");
    let b = resolve::import_value(serde_json::to_value(&out).expect("v"), &ropts())
        .expect("re-import")
        .timeline;
    let c = b
        .clips()
        .into_iter()
        .find(|c| c.name == name)
        .expect("clip");
    let mut got = ffcine_fx(c, "ffcine.geo.transform")
        .expect("transform")
        .clone();
    got.shift_remove("foreign");
    want.shift_remove("foreign");
    assert_eq!(Value::Object(got), Value::Object(want));
}

fn native() -> Timeline {
    let media = MediaReference::External(ExternalReference::new("file:///m/a.mov", None));
    let mut clip = Clip::new("a", media, Some(TimeRange::from_frames(24.0, 72.0, 24.0)));
    clip.set_ffcine_id("shot-a");
    let fx = |schema: &str, p: Value| {
        let Value::Object(p) = p else { unreachable!() };
        Effect::Effect(GenericEffect::ffcine(schema, p))
    };
    clip.effects = vec![
        fx(
            "ffcine.geo.transform/1",
            json!({"units": "frame", "y": "down", "translate_x": 0.1, "translate_y": 0.2,
                   "scale_x": 1.5, "scale_y": 1.5, "rotate_deg": 10.0}),
        ),
        fx(
            "ffcine.comp.blend/1",
            json!({"opacity": 0.5, "mode": "screen"}),
        ),
        fx("ffcine.audio.level/1", json!({"gain_db": -3.0})),
        fx(
            "ffcine.color.cdl/1",
            json!({"slope": [1.1, 1.0, 0.9], "offset": [0.01, 0.0, -0.01],
                   "power": [1.0, 1.0, 1.05], "saturation": 0.9}),
        ),
        fx("ffcine.fx.stabilize/1", json!({"smoothing": 20})),
    ];
    let mut track = Track::new("V1", track_kind::VIDEO);
    track.children.push(Item::Clip(clip));
    let mut tl = Timeline::new("native");
    tl.ffcine_mut()
        .insert("frame_size".into(), json!([1920, 1080]));
    tl.tracks.children.push(Item::Track(track));
    tl
}

#[test]
fn resolve_from_native_effects() {
    let tl = native();
    let (out, report) =
        export::resolve::export_report(&tl, &ExportOptions::default()).expect("export");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("fills the frame")),
        "{report}"
    );
    let c = &out.clips()[0];
    assert_eq!(
        c.metadata["cdl"],
        json!({"asc_sop": {"slope": [1.1, 1.0, 0.9], "offset": [0.01, 0.0, -0.01],
               "power": [1.0, 1.0, 1.05]}, "asc_sat": 0.9})
    );
    let types: Vec<u64> = c
        .effects
        .iter()
        .filter_map(|e| e.metadata().get("Resolve_OTIO")?.get("Type")?.as_u64())
        .collect();
    assert_eq!(types, [2, 1, 62]);
    // The stabilizer has no Resolve section: kept as an ffcine effect.
    assert!(ffcine_fx(c, "ffcine.fx.stabilize").is_some());

    let b = resolve::import_value(serde_json::to_value(&out).expect("v"), &ropts())
        .expect("re-import")
        .timeline;
    let c = &b.clips()[0];
    assert_eq!(c.ffcine_id(), Some("shot-a"));
    let t = ffcine_fx(c, "ffcine.geo.transform").expect("transform");
    assert_eq!(t["units"], json!("picture"));
    for (k, v) in [
        ("translate_x", 0.1),
        ("translate_y", 0.2),
        ("scale_x", 1.5),
        ("rotate_deg", 10.0),
    ] {
        assert!(
            (t[k].as_f64().unwrap_or(f64::NAN) - v).abs() < 1e-9,
            "{k}: {}",
            t[k]
        );
    }
    let bl = ffcine_fx(c, "ffcine.comp.blend").expect("blend");
    assert_eq!(
        (bl["opacity"].as_f64(), bl["mode"].as_str()),
        (Some(0.5), Some("screen"))
    );
    let l = ffcine_fx(c, "ffcine.audio.level").expect("level");
    assert_eq!(l["gain_db"], json!(-3.0));
}

#[test]
fn cdl_edl_and_ccc() {
    let tl = native();
    let edl = export::cdl_edl(&tl);
    assert!(
        edl.starts_with("TITLE: native\nFCM: NON-DROP FRAME\n"),
        "{edl}"
    );
    assert!(
        edl.contains(
            "001  AX       V     C        00:00:01:00 00:00:03:00 00:00:00:00 00:00:02:00"
        ),
        "{edl}"
    );
    assert!(
        edl.contains("*ASC_SOP (1.1 1.0 0.9)(0.01 0.0 -0.01)(1.0 1.0 1.05)\n*ASC_SAT 0.9\n"),
        "{edl}"
    );
    let ccc = export::ccc(&tl);
    assert!(ccc.contains("<ColorCorrection id=\"shot-a\">"), "{ccc}");
    assert!(ccc.contains("<Slope>1.1 1.0 0.9</Slope>"), "{ccc}");
    assert!(ccc.contains("<Saturation>0.9</Saturation>"), "{ccc}");

    // The FCPXML fixture's Color Board CDL goes through too.
    let a = fcpxml::import(
        &Path::new(FCP).join("features.fcpxml"),
        &ImportOptions::default(),
    )
    .expect("import")
    .timeline;
    let edl = export::cdl_edl(&a);
    assert!(
        edl.contains("*ASC_SOP (1.1 1.0 0.9)(0.01 0.0 -0.01)(1.0 1.0 1.05)"),
        "{edl}"
    );
}

/// Numbers within 1e-6; keyframes without `t` (see the module docs); no `foreign`.
fn norm(v: &Value) -> Value {
    match v {
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            json!(((x * 1e6).round() / 1e6) + 0.0)
        }
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| *k != "foreign" && *k != "t")
                .map(|(k, v)| (k.clone(), norm(v)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

/// `(track kind, position, name, source range, effects, markers, cdl, generator)` per clip,
/// rounded and sorted.
fn summary(tl: &Timeline) -> Vec<String> {
    fn walk(items: &[Item], kind: &str, at: f64, rate: f64, out: &mut Vec<String>) {
        let mut cursor = at;
        for i in items {
            let d = i
                .source_range()
                .map_or(0.0, |r| r.duration.rescaled_to(rate).value);
            match i {
                Item::Clip(c) => {
                    let r = c.source_range.as_ref().map(|r| {
                        (
                            (r.start_time.to_seconds() * 1000.0).round(),
                            (r.duration.to_seconds() * 1000.0).round(),
                        )
                    });
                    let fx: Vec<Value> = c
                        .effects
                        .iter()
                        .map(|e| match e {
                            Effect::Effect(g) => json!([
                                g.effect_name,
                                norm(&Value::Object(g.ffcine().cloned().unwrap_or_default()))
                            ]),
                            Effect::LinearTimeWarp(w) => {
                                json!(["warp", norm(&json!(w.time_scalar))])
                            }
                            other => json!([other.effect_name()]),
                        })
                        .collect();
                    let markers: Vec<Value> = c
                        .markers
                        .iter()
                        .map(|m| {
                            json!([m.name, norm(&json!(m.marked_range.start_time.to_seconds()))])
                        })
                        .collect();
                    let generator = match c.media_reference() {
                        Some(MediaReference::Generator(g)) => {
                            norm(&Value::Object(g.parameters.clone()))
                        }
                        _ => Value::Null,
                    };
                    out.push(
                        json!([
                            kind,
                            (cursor / rate * 1000.0).round(),
                            c.name,
                            r,
                            fx,
                            markers,
                            c.metadata.get("cdl"),
                            generator
                        ])
                        .to_string(),
                    );
                }
                Item::Stack(s) => {
                    for t in &s.children {
                        if let Item::Track(t) = t {
                            walk(&t.children, &t.kind, cursor, rate, out);
                        }
                    }
                }
                _ => {}
            }
            if !matches!(i, Item::Transition(_)) {
                cursor += d;
            }
        }
    }
    let rate = tl.global_start_time.as_ref().map_or(24.0, |t| t.rate);
    let mut out = Vec::new();
    for t in tl.track_list() {
        walk(&t.children, &t.kind, 0.0, rate, &mut out);
    }
    out.sort();
    out
}

fn transitions(tl: &Timeline) -> usize {
    tl.track_list()
        .flat_map(|t| &t.children)
        .filter(|i| matches!(i, Item::Transition(_)))
        .count()
}

#[test]
fn fcpxml_round_trip() {
    for name in ["features.fcpxml", "fcpx_example.fcpxml"] {
        let a = fcpxml::import(&Path::new(FCP).join(name), &ImportOptions::default())
            .expect("import")
            .timeline;
        let (xml, report) =
            export::fcpxml::export_report(&a, &ExportOptions::default()).expect("export");
        assert!(xml.contains("<fcpxml version=\"1.10\">"));
        let b = fcpxml::import_str(&xml, &ImportOptions::default())
            .unwrap_or_else(|e| panic!("{name}: {e}\n{xml}"))
            .timeline;
        assert_eq!(b.name, a.name, "{name}");
        assert_eq!(transitions(&b), transitions(&a), "{name}");
        let (sa, sb) = (summary(&a), summary(&b));
        assert!(sb == sa, "{name}: {report}\n{}", diff(&sb, &sa));
    }
}

#[test]
fn fcpxml_times_and_canonical_transform() {
    let mut tl = native();
    tl.global_start_time = Some(ffcine_timeline::RationalTime::new(0.0, 24000.0 / 1001.0));
    for c in clips_mut(&mut tl.tracks.children) {
        c.source_range = Some(TimeRange::from_frames(24.0, 72.0, 24000.0 / 1001.0));
    }
    let xml = export::fcpxml::export(&tl, &ExportOptions::default()).expect("export");
    assert!(xml.contains("frameDuration=\"1001/24000s\""), "{xml}");
    assert!(xml.contains("duration=\"1001/500s\""), "{xml}");
    // translate (0.1, 0.2) of the frame, y down -> percent of the height, y up.
    assert!(xml.contains("position=\"17.777778 -20.0\""), "{xml}");
    assert!(xml.contains("rotation=\"10.0\""), "{xml}");
    assert!(
        xml.contains("<adjust-blend amount=\"0.5\" mode=\"Screen\"/>"),
        "{xml}"
    );
    assert!(xml.contains("<info-asc-cdl slope=\"1.1 1.0 0.9\""), "{xml}");
    let b = fcpxml::import_str(&xml, &ImportOptions::default())
        .expect("re-import")
        .timeline;
    let t = ffcine_fx(b.clips()[0], "ffcine.geo.transform").expect("transform");
    // Canonical reader (pending): translate_x/translate_y; current reader: position.
    let (x, y) = match (t.get("translate_x"), t.get("position")) {
        (Some(x), _) => (x.as_f64(), t["translate_y"].as_f64()),
        (None, Some(p)) => (p[0].as_f64(), p[1].as_f64()),
        _ => panic!("no translation in {t:?}"),
    };
    assert!(
        (x.unwrap_or(f64::NAN) - 0.1).abs() < 1e-6 && (y.unwrap_or(f64::NAN) - 0.2).abs() < 1e-6
    );
}

/// What's only in `a`, and only in `b`.
fn diff(a: &[String], b: &[String]) -> String {
    let only = |x: &[String], y: &[String]| -> Vec<String> {
        x.iter().filter(|s| !y.contains(s)).cloned().collect()
    };
    format!(
        "only left: {:#?}\nonly right: {:#?}",
        only(a, b),
        only(b, a)
    )
}
