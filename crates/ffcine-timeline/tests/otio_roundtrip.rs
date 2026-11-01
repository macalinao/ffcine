//! Round-trips `OpenTimelineIO`'s own sample files (`tests/sample_data` at 80c5ff3) through the
//! timeline model.
//!
//! Lossless means: every value in the input comes back unchanged (numbers compared as f64,
//! since `24` is written back as `24.0`), and the only additions are fields OTIO itself fills in
//! on read (`enabled`, `effects`, ... with their defaults, `Clip.1` -> `Clip.2`, `Sequence.1` ->
//! `Track.1`).
//! A second cycle is byte-identical.

use std::path::Path;

use ffcine_timeline::{Item, Timeline};
use serde_json::{Map, Value};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/otio");

// Left out: big_int.otio (128/256-bit ints, `Infinity`, `NaN`: not JSON).
const SAMPLES: &[&str] = &[
    "clip_example.otio",
    "effects.otio",
    "generator_reference_test.otio",
    "multiple_track.otio",
    "multitrack.otio",
    "nested_example.otio",
    "preflattened.otio",
    "premiere_example.otio",
    "screening_example.otio",
    "simple_cut.otio",
    "transition.otio",
    "transition_test.otio",
];

/// OTIO's in-memory form of an input file: `Clip.1` upgraded, `Sequence.1` read as `Track.1`, `"name": null` read as `""`.
fn as_otio_reads(v: &mut Value) {
    match v {
        Value::Object(m) => {
            if m.get("OTIO_SCHEMA").and_then(Value::as_str) == Some("Clip.1") {
                let media = match m.shift_remove("media_reference") {
                    Some(v @ Value::Object(_)) => v,
                    // OTIO puts a MissingReference where there was none.
                    _ => serde_json::json!({ "OTIO_SCHEMA": "MissingReference.1" }),
                };
                m.insert("OTIO_SCHEMA".into(), "Clip.2".into());
                m.insert(
                    "media_references".into(),
                    Value::Object(Map::from_iter([("DEFAULT_MEDIA".to_owned(), media)])),
                );
                m.insert("active_media_reference_key".into(), "DEFAULT_MEDIA".into());
            }
            if m.get("OTIO_SCHEMA").and_then(Value::as_str) == Some("Sequence.1") {
                m.insert("OTIO_SCHEMA".into(), "Track.1".into());
            }
            if m.contains_key("OTIO_SCHEMA") && m.get("name") == Some(&Value::Null) {
                m.insert("name".into(), "".into());
            }
            m.values_mut().for_each(as_otio_reads);
        }
        Value::Array(a) => a.iter_mut().for_each(as_otio_reads),
        _ => {}
    }
}

/// Every value in `want` is in `got` (objects may have extra keys); numbers compared as f64.
fn assert_contains(got: &Value, want: &Value, path: &str) {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            for (k, wv) in w {
                let gv = g.get(k).unwrap_or_else(|| panic!("{path}.{k}: missing"));
                assert_contains(gv, wv, &format!("{path}.{k}"));
            }
        }
        (Value::Array(g), Value::Array(w)) => {
            assert_eq!(g.len(), w.len(), "{path}: length");
            for (i, (gv, wv)) in g.iter().zip(w).enumerate() {
                assert_contains(gv, wv, &format!("{path}[{i}]"));
            }
        }
        (Value::Number(g), Value::Number(w)) => {
            assert_eq!(g.as_f64(), w.as_f64(), "{path}");
            if w.is_i64() || w.is_u64() {
                // Ints outside the modelled f64 fields stay ints.
                assert!(g.as_f64() == w.as_f64(), "{path}");
            }
        }
        _ => assert_eq!(got, want, "{path}"),
    }
}

#[test]
fn otio_samples_round_trip() {
    for name in SAMPLES {
        let text = std::fs::read_to_string(Path::new(DATA).join(name)).expect("read sample");
        let t = Timeline::from_json(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let out = t.to_json().expect("write");

        let mut want: Value = serde_json::from_str(&text).expect("json");
        as_otio_reads(&mut want);
        let got: Value = serde_json::from_str(&out).expect("json");
        assert_contains(&got, &want, name);

        let again = Timeline::from_json(&out).expect("reread");
        assert_eq!(again, t, "{name}: model differs after a second read");
        assert_eq!(again.to_json().expect("write"), out, "{name}: not stable");
    }
}

#[test]
fn unknown_schemas_and_fields_survive() {
    let json = r#"{
        "OTIO_SCHEMA": "Timeline.1", "name": "t", "metadata": {}, "global_start_time": null,
        "tracks": { "OTIO_SCHEMA": "Stack.1", "name": "", "metadata": {}, "source_range": null,
            "effects": [ { "OTIO_SCHEMA": "Vendor.Blur.3", "radius": 2 } ],
            "markers": [], "enabled": true, "color": null,
            "children": [ { "OTIO_SCHEMA": "Track.1", "name": "V1", "kind": "Video",
                "metadata": {}, "source_range": null, "effects": [], "markers": [],
                "enabled": true, "future_field": [1, 2],
                "children": [ { "OTIO_SCHEMA": "SomethingNew.7", "x": { "y": 1 } } ] } ] } }"#;
    let t = Timeline::from_json(json).expect("parse");
    let Item::Track(track) = &t.tracks.children[0] else {
        panic!("track")
    };
    assert!(matches!(track.children[0], Item::Unknown(_)));
    assert_eq!(track.extra["future_field"], serde_json::json!([1, 2]));
    let got: Value = serde_json::from_str(&t.to_json().expect("write")).expect("json");
    let want: Value = serde_json::from_str(json).expect("json");
    assert_eq!(got, want);
}

#[test]
fn rejects_newer_core_versions() {
    let json = r#"{ "OTIO_SCHEMA": "Timeline.2", "tracks": { "OTIO_SCHEMA": "Stack.1" } }"#;
    let err = Timeline::from_json(json).expect_err("Timeline.2");
    assert!(err.to_string().contains("Timeline"), "{err}");
}
