//! FCP7 / Premiere XML importer on hand-written fixtures.

#![allow(clippy::many_single_char_names, clippy::float_cmp)]

use std::path::Path;

use ffcine_import::{Format, ImportOptions, ImportReport, fcp7, import};
use ffcine_timeline::{Clip, Effect, HasMetadata, Item, MediaReference, Timeline, Track};
use serde_json::{Value, json};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fcp7");

fn load(name: &str) -> (Timeline, ImportReport) {
    let r = fcp7::import(&Path::new(DATA).join(name), &ImportOptions::default())
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
            Item::Gap(g) => format!(
                "-{}",
                g.source_range.as_ref().map_or(0.0, |r| r.duration.value)
            ),
            _ => i.name().into(),
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
        .unwrap_or_else(|| panic!("no clip {name}"))
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

fn range(c: &Clip) -> (f64, f64) {
    let r = c.source_range.as_ref().expect("source_range");
    (r.start_time.value, r.duration.value)
}

fn warp(c: &Clip) -> Option<f64> {
    c.effects.iter().find_map(|e| match e {
        Effect::LinearTimeWarp(w) => Some(w.time_scalar),
        _ => None,
    })
}

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
fn editorial() {
    let (t, report) = load("features.xml");
    assert_eq!(t.name, "Features");
    let names: Vec<_> = t.track_list().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["V1", "V2", "A1"]);
    assert_eq!(t.ffcine_value("frame_size"), Some(&json!([1920, 1080])));
    assert_eq!(t.global_start_time.as_ref().map(|g| g.value), Some(86400.0));
    assert_eq!(t.tracks.markers[0].name, "Chapter");
    assert_eq!(t.tracks.markers[0].marked_range.start_time.value, 24.0);

    let v1 = track(&t, "V1");
    assert_eq!(labels(v1), ["A", "T", "B", "T", "-24", "Red"]);
    let a = clip(v1, "A");
    assert_eq!(range(a), (10.0, 48.0));
    let b = clip(v1, "B");
    assert_eq!(range(b), (100.0, 48.0));
    let Item::Transition(x) = &v1.children[1] else {
        panic!("transition")
    };
    assert_eq!((x.in_offset.value, x.out_offset.value), (4.0, 4.0));
    assert!(
        x.ffcine_value("kind").is_none(),
        "cross dissolve has no kind"
    );
    let Item::Transition(iris) = &v1.children[3] else {
        panic!("transition")
    };
    assert_eq!((iris.in_offset.value, iris.out_offset.value), (8.0, 0.0));
    assert_eq!(
        iris.ffcine_value("kind"),
        Some(&json!("Iris Round")),
        "{iris:?}"
    );
    let Item::Transition(fade) = &track(&t, "A1").children[1] else {
        panic!("transition")
    };
    assert_eq!(fade.ffcine_value("crossfade_db"), Some(&json!(3.0)));

    // Media: one file declared once, referenced by id thereafter.
    for c in [a, b, clip(track(&t, "A1"), "B")] {
        let Some(MediaReference::External(e)) = c.media_reference() else {
            panic!("{}: external media", c.name)
        };
        assert_eq!(e.target_url, "file:///media/a%20b.mov");
        assert_eq!(e.path().as_deref(), Some(Path::new("/media/a b.mov")));
    }
    let src = a.ffcine_value("source").expect("source");
    assert_eq!(src["reel"], "A001");
    assert_eq!(src["timecode"], "10:00:00:00");
    assert_eq!(
        src["logginginfo"],
        json!({"description": "wide", "scene": "3"})
    );
    // Linked A/V pair.
    let a_audio = clip(track(&t, "A1"), "A");
    assert_eq!(a.ffcine_value("link_group"), Some(&json!(1)));
    assert_eq!(a_audio.ffcine_value("link_group"), Some(&json!(1)));
    assert!(b.ffcine_value("link_group").is_none());
    // Clip marker in source time.
    assert_eq!(a.markers[0].marked_range.start_time.value, 20.0);
    assert_eq!(a.markers[0].marked_range.duration.value, 2.0);
    // Unmapped bits stay verbatim.
    assert!(
        a.foreign("fcp_xml").expect("foreign")["children"]
            .to_string()
            .contains("label2")
    );
    for u in [
        "clipitem/labels",
        "filter dropshadow",
        "basic/aspectratio",
        "track/locked",
    ] {
        assert!(
            report.unmapped.iter().any(|x| x == u),
            "{u}: {:?}",
            report.unmapped
        );
    }
    assert_eq!(report.skipped, Vec::<String>::new());

    // Generators.
    let red = clip(v1, "Red");
    let Some(MediaReference::Generator(g)) = red.media_reference() else {
        panic!("solid")
    };
    assert_eq!(g.generator_kind, "ffcine.solid");
    assert_eq!(g.parameters["colour"], json!([1.0, 0.0, 0.0, 1.0]));
    let title = clip(track(&t, "V2"), "Title");
    let Some(MediaReference::Generator(g)) = title.media_reference() else {
        panic!("text")
    };
    assert_eq!(g.generator_kind, "ffcine.text");
    assert_eq!(g.parameters["text"], "Hi there");
    assert_eq!(g.parameters["font"], "Lucida Grande");
    assert_eq!(g.parameters["size"], 36.0);
    assert_eq!(g.parameters["bold"], true);
    assert_eq!(g.parameters["alignment"], "center");
    assert_eq!(g.parameters["position"], json!([0.0, 0.3]));
    assert_eq!(labels(track(&t, "V2")), ["Title", "C", "D"]);
    // A file without pathurl.
    assert!(matches!(
        clip(track(&t, "V2"), "C").media_reference(),
        Some(MediaReference::Missing(_))
    ));
}

#[test]
fn effects() {
    let (t, report) = load("features.xml");
    let v1 = track(&t, "V1");
    let a = clip(v1, "A");
    let tr = fx(a, "ffcine.geo.transform");
    assert_eq!(tr["schema"], "ffcine.geo.transform/1");
    assert_eq!(tr["units"], "frame");
    assert_eq!(
        keys(&tr["scale_x"]),
        [
            (10.0, json!(1.0), "linear".into()),
            (34.0, json!(1.5), "ease".into())
        ]
    );
    assert_eq!(tr["rotate_deg"], -15.0);
    assert_eq!(tr["translate_x"], 0.25);
    assert_eq!(tr["translate_y"], -0.1);
    assert_eq!(tr["anchor"], json!([0.0, 0.0]));
    assert!(
        a.effects
            .iter()
            .any(|e| e.effect_name() == Some("dropshadow"))
    );

    let b = clip(v1, "B");
    let crop = fx(b, "ffcine.geo.crop");
    assert_eq!(
        (&crop["left"], &crop["right"], &crop["top"], &crop["bottom"]),
        (&json!(0.1), &json!(0.0), &json!(0.05), &json!(0.05))
    );
    assert_eq!(fx(b, "ffcine.comp.blend")["opacity"], 0.5);
    assert_eq!(warp(b), Some(0.5));

    let v2 = track(&t, "V2");
    let c = clip(v2, "C");
    assert_eq!(warp(c), Some(1.5));
    let ramp = fx(c, "ffcine.time.ramp");
    let pts: Vec<(f64, f64)> = ramp["points"]
        .as_array()
        .expect("points")
        .iter()
        .map(|p| (p[0].as_f64().expect("u"), p[1].as_f64().expect("speed")))
        .collect();
    assert_eq!(pts.len(), 4);
    assert!((pts[1].0 - 1.0 / 3.0).abs() < 1e-6 && pts[1].1 == 1.0 && pts[2].1 == 2.0);
    assert_eq!(warp(clip(v2, "D")), Some(-1.0));
    assert!(report.warnings.iter().any(|w| w.contains("reverse")));

    let au = clip(track(&t, "A1"), "A");
    let lv = keys(&fx(au, "ffcine.audio.level")["gain_db"]);
    assert_eq!(lv[0].1, json!(0.0));
    assert!((lv[1].1.as_f64().expect("db") + 6.0206).abs() < 1e-3);
    assert_eq!(fx(au, "ffcine.audio.pan")["pan"], -1.0);
}

#[test]
fn premiere_graphic() {
    let (t, _) = load("premiere.xml");
    let v1 = track(&t, "V1");
    let title = clip(v1, "Title");
    let Some(MediaReference::Generator(g)) = title.media_reference() else {
        panic!("text")
    };
    assert_eq!(g.generator_kind, "ffcine.text");
    assert_eq!(g.parameters["text"], "Hello\nWorld");
    assert_eq!(g.parameters["font"], "Helvetica Neue");
    assert_eq!(g.parameters["size"], 64);
    assert_eq!(t.ffcine_value("frame_size"), Some(&json!([1080, 1920])));
    let rate = range(title);
    assert_eq!(rate, (0.0, 60.0));
    let r = title.source_range.as_ref().expect("range").duration.rate;
    assert!((r - 30000.0 / 1001.0).abs() < 1e-9);
}

#[test]
fn ids_stable() {
    let ids = |t: &Timeline| -> Vec<String> {
        t.clips()
            .iter()
            .map(|c| c.ffcine_id().expect("id").to_owned())
            .collect()
    };
    let (a, _) = load("features.xml");
    let (b, _) = load("features.xml");
    let x = ids(&a);
    assert_eq!(x, ids(&b));
    assert!(x.iter().all(|i| i.starts_with("fcp7-")));
    let mut u = x.clone();
    u.sort();
    u.dedup();
    assert_eq!(u.len(), x.len());
}

#[test]
fn format_detection() {
    for f in ["features.xml", "premiere.xml"] {
        let p = Path::new(DATA).join(f);
        assert_eq!(Format::detect(&p), Some(Format::Fcp7));
        let r = import(&p, None, &ImportOptions::default()).expect("import");
        assert!(r.timeline.track_list().count() > 0);
    }
    assert_eq!("premiere".parse::<Format>().ok(), Some(Format::Fcp7));
}

#[test]
fn lenient_and_strict() {
    let p = Path::new(DATA).join("broken.xml");
    let r = import(&p, None, &ImportOptions::default()).expect("lenient");
    assert_eq!(r.report.skipped.len(), 2, "{:?}", r.report.skipped);
    let v1 = track(&r.timeline, "V1");
    assert_eq!(labels(v1), ["Ok", "-25", "Ref"]);
    assert!(matches!(
        clip(v1, "Ref").media_reference(),
        Some(MediaReference::Missing(_))
    ));
    let strict = ImportOptions {
        strict: true,
        ..ImportOptions::default()
    };
    let err = import(&p, None, &strict).expect_err("strict");
    assert!(err.to_string().contains("hologramitem"), "{err}");
    assert!(fcp7::import_str("<fcpxml/>", &ImportOptions::default()).is_err());
}
