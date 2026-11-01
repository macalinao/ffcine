//! CMX 3600 EDL + ASC CDL import (`ffcine_import::edl`) on the hand-written fixtures.

use std::path::Path;

use ffcine_import::{Format, ImportOptions, edl, import};
use ffcine_timeline::{Clip, Effect, HasMetadata, Item, Timeline, Track};
use serde_json::json;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/edl");

fn opts() -> ImportOptions {
    ImportOptions {
        rate: Some(25.0),
        ..ImportOptions::default()
    }
}

fn load(name: &str) -> ffcine_import::Imported {
    edl::import(&Path::new(DATA).join(name), &opts()).expect("import")
}

fn track<'a>(t: &'a Timeline, name: &str) -> &'a Track {
    t.track_list().find(|x| x.name == name).expect(name)
}

/// `name(start+dur)` per clip, `T(in+out)` per transition, `-dur` per gap.
fn labels(t: &Track) -> Vec<String> {
    t.children
        .iter()
        .map(|i| match i {
            Item::Clip(c) => {
                let r = c.source_range.as_ref().expect("range");
                format!("{}({}+{})", c.name, r.start_time.value, r.duration.value)
            }
            Item::Transition(x) => format!("T({}+{})", x.in_offset.value, x.out_offset.value),
            Item::Gap(g) => format!("-{}", g.source_range.as_ref().expect("gap").duration.value),
            _ => "?".into(),
        })
        .collect()
}

fn clip<'a>(t: &'a Track, name: &str) -> &'a Clip {
    t.children
        .iter()
        .find_map(|i| match i {
            Item::Clip(c) if c.name == name => Some(c),
            _ => None,
        })
        .expect(name)
}

fn cdl(c: &Clip) -> Option<serde_json::Value> {
    c.effects.iter().find_map(|e| match e {
        Effect::Effect(g) if g.effect_name == "ffcine.color.cdl" => {
            g.ffcine().cloned().map(serde_json::Value::Object)
        }
        _ => None,
    })
}

#[test]
fn editorial() {
    let r = load("features.edl");
    let t = &r.timeline;
    assert_eq!(t.name, "Sample Day 3");
    assert_eq!(
        t.global_start_time.as_ref().map(|g| g.value),
        Some(90_000.0)
    );
    let names: Vec<_> = t.track_list().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["V1", "A1", "A2"]);
    assert_eq!(
        labels(track(t, "V1")),
        [
            "beach.mov(90000+100)",
            "T(0+25)",
            "market.mov(270000+75)",
            "talk.mov(360000+50)",
            "T(0+10)",
            "sunset.mov(450000+50)",
        ]
    );
    assert_eq!(labels(track(t, "A1")), ["-175", "talk.mov(360000+50)"]);
    assert_eq!(
        labels(track(t, "A2")),
        ["-175", "talk.mov(360000+50)", "-75", "wind.wav(630000+25)"]
    );

    let v1 = track(t, "V1");
    let beach = clip(v1, "beach.mov");
    assert_eq!(
        beach.media_reference().map(|m| match m {
            ffcine_timeline::MediaReference::External(e) => e.target_url.clone(),
            _ => String::new(),
        }),
        Some("file:///media/beach.mov".to_owned())
    );
    assert_eq!(
        beach.ffcine_value("source"),
        Some(&json!({"src": "A001", "kind": "reel"}))
    );
    // LOC 01:00:01:00 is 25 frames into the event: the marker sits in the clip's source time.
    assert_eq!(beach.markers.len(), 1);
    assert_eq!(beach.markers[0].name, "wave");
    assert_eq!(beach.markers[0].marked_range.start_time.value, 90_025.0);

    // M2 50 fps on a 25 fps timeline: 2x; the A/V channels share a link group.
    let talk = clip(v1, "talk.mov");
    assert!(talk.effects.iter().any(|e| matches!(e,
        Effect::LinearTimeWarp(w) if (w.time_scalar - 2.0).abs() < 1e-9)));
    assert_eq!(talk.ffcine_value("link_group"), Some(&json!("edl-003")));
    assert_eq!(
        clip(track(t, "A1"), "talk.mov").ffcine_value("link_group"),
        Some(&json!("edl-003"))
    );
    assert_eq!(
        talk.foreign("cmx_3600").and_then(|f| f.get("comments")),
        Some(&json!(["* COMMENT: interview"]))
    );

    // The wipe renders as a dissolve; its SMPTE code is kept.
    let wipe = v1
        .children
        .iter()
        .filter_map(|i| match i {
            Item::Transition(x) => Some(x),
            _ => None,
        })
        .nth(1)
        .expect("wipe");
    assert_eq!(wipe.ffcine_value("kind"), Some(&json!("wipe")));
    assert_eq!(wipe.ffcine_value("smpte"), Some(&json!("001")));

    // The key event is skipped (lenient) and reported.
    assert!(r.report.skipped.iter().any(|s| s.contains("key")));
    assert!(r.report.warnings.iter().any(|w| w.contains("W001")));
}

#[test]
fn cdl_inline_and_sidecar() {
    let r = load("features.edl");
    let v1 = track(&r.timeline, "V1");
    let beach = clip(v1, "beach.mov");
    assert_eq!(
        cdl(beach),
        Some(
            json!({"schema": "ffcine.color.cdl/1", "slope": [1.1, 1.0, 0.9],
                    "offset": [0.01, 0.0, -0.01], "power": [1.0, 1.0, 1.05], "saturation": 0.9})
        )
    );
    assert_eq!(
        beach.metadata["cdl"],
        json!({"asc_sop": {"slope": [1.1, 1.0, 0.9], "offset": [0.01, 0.0, -0.01],
               "power": [1.0, 1.0, 1.05]}, "asc_sat": 0.9})
    );
    // ASC_CC_XML id into the sidecar .ccc.
    let market = clip(v1, "market.mov");
    assert_eq!(
        market.metadata["cdl"]["asc_sop"]["slope"],
        json!([0.95, 1.0, 1.1])
    );
    assert_eq!(market.metadata["cdl"]["asc_sat"], json!(1.2));
    // Clip-name match.
    let sunset = clip(v1, "sunset.mov");
    assert_eq!(
        sunset.metadata["cdl"]["asc_sop"]["slope"],
        json!([1.2, 1.0, 0.8])
    );
    // No CDL on audio, none where nothing matched.
    assert!(cdl(clip(track(&r.timeline, "A1"), "talk.mov")).is_none());
    assert!(cdl(clip(v1, "talk.mov")).is_none());

    // Without the sidecar only the inline CDL is there.
    let src = std::fs::read_to_string(Path::new(DATA).join("features.edl")).expect("read");
    let bare = edl::import_str(&src, &[], &opts()).expect("import");
    let v1 = track(&bare.timeline, "V1");
    assert!(cdl(clip(v1, "market.mov")).is_none());
    assert!(cdl(clip(v1, "beach.mov")).is_some());
    assert!(bare.report.warnings.iter().any(|w| w.contains("sh030")));
}

#[test]
fn stable_ids_and_round_trip() {
    let a = load("features.edl").timeline;
    let b = load("features.edl").timeline;
    let ids = |t: &Timeline| -> Vec<String> {
        t.track_list()
            .flat_map(|x| x.children.iter())
            .filter_map(|i| match i {
                Item::Clip(c) => c.ffcine_id().map(str::to_owned),
                _ => None,
            })
            .collect()
    };
    let ia = ids(&a);
    assert_eq!(ia, ids(&b));
    assert_eq!(ia.len(), 7);
    let mut uniq = ia.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), ia.len());
    assert!(ia.iter().all(|i| i.starts_with("edl-")));
    let json = a.to_json().expect("json");
    assert_eq!(Timeline::from_json(&json).expect("parse"), a);
}

#[test]
fn strict_fails_on_skipped_events() {
    let path = Path::new(DATA).join("features.edl");
    let strict = ImportOptions {
        strict: true,
        ..opts()
    };
    let err = import(&path, None, &strict).expect_err("strict");
    assert!(err.to_string().contains("key"), "{err}");
    assert!(import(&path, None, &opts()).is_ok());
}

#[test]
fn drop_frame_and_detection() {
    let path = Path::new(DATA).join("dropframe.edl");
    assert_eq!(Format::detect(&path), Some(Format::Edl));
    assert_eq!("cmx3600".parse::<Format>().ok(), Some(Format::Edl));
    let r = import(&path, None, &ImportOptions::default()).expect("import");
    assert!(r.report.warnings.iter().any(|w| w.contains("29.970")));
    let v1 = track(&r.timeline, "V1");
    assert_eq!(labels(v1), ["TAPE1(1798+6)"]);
}
