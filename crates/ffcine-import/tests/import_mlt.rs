//! MLT importer on hand-written Kdenlive / Shotcut projects, `melt -mix` output and the OTIO
//! Kdenlive adapter's samples.

#![allow(clippy::many_single_char_names)]

use std::collections::HashSet;
use std::path::Path;

use ffcine_import::{ImportOptions, mlt};
use ffcine_timeline::{
    Clip, Effect, HasMetadata, Item, Marker, MediaReference, Timeline, Track, Transition,
    track_kind,
};
use serde_json::{Value, json};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/mlt");

fn load(name: &str) -> (Timeline, ffcine_import::ImportReport) {
    let r = mlt::import(&Path::new(DATA).join(name), &ImportOptions::default())
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

/// `name(start+dur)` for clips, `-dur` for gaps, `T(in+out)` for transitions.
fn labels(t: &Track) -> Vec<String> {
    t.children
        .iter()
        .map(|i| match i {
            Item::Transition(x) => format!("T({}+{})", x.in_offset.value, x.out_offset.value),
            Item::Gap(_) => format!("-{}", i.source_range().map_or(0.0, |r| r.duration.value)),
            _ => {
                let r = i.source_range().expect("range");
                format!("{}({}+{})", i.name(), r.start_time.value, r.duration.value)
            }
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

fn transition(t: &Track) -> &Transition {
    t.children
        .iter()
        .find_map(|i| match i {
            Item::Transition(x) => Some(x),
            _ => None,
        })
        .expect("transition")
}

/// The ffcine params of every `name` effect on `c`.
fn fx(c: &Clip, name: &str) -> Vec<Value> {
    c.effects
        .iter()
        .filter_map(|e| match e {
            Effect::Effect(g) if g.effect_name == name => g.ffcine().cloned().map(Value::Object),
            _ => None,
        })
        .collect()
}

fn one_fx(c: &Clip, name: &str) -> Value {
    let v = fx(c, name);
    assert_eq!(v.len(), 1, "{}: {name}", c.name);
    v[0].clone()
}

/// `<name>_x` and `<name>_y` of a transform zipped back into `[x, y]` (plain or keyframed).
fn pair(tf: &Value, name: &str) -> Value {
    let (x, y) = (&tf[format!("{name}_x")], &tf[format!("{name}_y")]);
    match (x.as_array(), y.as_array()) {
        (Some(xs), Some(ys)) => xs
            .iter()
            .zip(ys)
            .map(|(a, b)| json!({"t": a["t"], "v": [a["v"], b["v"]], "interp": a["interp"]}))
            .collect(),
        _ => json!([x, y]),
    }
}

/// The clip's in point, in frames.
fn start(c: &Clip) -> f64 {
    c.source_range.as_ref().expect("range").start_time.value
}

/// `[(t, v, interp)]` of a keyframe list.
fn keys(v: &Value) -> Vec<(f64, Value, String)> {
    v.as_array()
        .unwrap_or_else(|| panic!("not keyframes: {v}"))
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

fn approx(a: &Value, b: &[f64]) {
    let a: Vec<f64> = a
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {a}"))
        .iter()
        .map(|x| x.as_f64().expect("number"))
        .collect();
    assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
    for (x, y) in a.iter().zip(b) {
        assert!((x - y).abs() < 1e-3, "{a:?} vs {b:?}");
    }
}

fn generator(c: &Clip) -> (&str, &serde_json::Map<String, Value>) {
    match c.media_reference() {
        Some(MediaReference::Generator(g)) => (g.generator_kind.as_str(), &g.parameters),
        other => panic!("{}: not a generator: {other:?}", c.name),
    }
}

fn url(c: &Clip) -> &str {
    match c.media_reference() {
        Some(MediaReference::External(e)) => &e.target_url,
        other => panic!("{}: not external: {other:?}", c.name),
    }
}

fn colour(m: &Marker) -> &str {
    m.extra
        .get("color")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn assert_ids(t: &Timeline) {
    let mut seen = HashSet::new();
    for c in t.clips() {
        let id = c
            .ffcine_value("id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{}: no id", c.name));
        assert!(id.starts_with("mlt-"), "{id}");
        assert!(seen.insert(id.to_owned()), "duplicate id {id}");
    }
}

#[test]
fn kdenlive_editorial() {
    let (t, r) = load("features.kdenlive");
    let names: Vec<_> = t
        .track_list()
        .map(|x| (x.name.as_str(), x.kind.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("Music", track_kind::AUDIO),
            ("Main", track_kind::VIDEO),
            ("Titles", track_kind::VIDEO),
        ]
    );
    // The mix trims both clips to the cut (5 frames into it); the timewarp clip's source
    // range is in media frames at 2x.
    let main = track(&t, "Main");
    assert_eq!(
        labels(main),
        ["beach.mp4(50+95)", "T(5+5)", "street.mp4(50+95)"]
    );
    assert_eq!(
        transition(main).ffcine().cloned().map(|mut m| {
            m.shift_remove("foreign");
            Value::Object(m)
        }),
        Some(
            json!({"schema": "ffcine.transition/1", "kind": "wipe", "resource": "/usr/share/kdenlive/lumas/HD/luma03.pgm",
                     "softness": "0.2", "reverse": "0"})
        )
    );
    let street = clip(main, "street.mp4");
    assert!(
        matches!(&street.effects[..], [Effect::LinearTimeWarp(w)] if (w.time_scalar - 2.0).abs() < 1e-12)
    );
    assert_eq!(url(street), "file:///media/project/footage/street.mp4");
    let beach = clip(main, "beach.mp4");
    assert_eq!(url(beach), "file:///media/project/footage/beach.mp4");
    let m: Vec<_> = beach
        .markers
        .iter()
        .map(|m| (m.name.as_str(), m.marked_range.start_time.value, colour(m)))
        .collect();
    assert_eq!(m, [("wave", 40.0, "GREEN")]);
    assert_eq!(
        labels(track(&t, "Titles")),
        ["Slate(0+25)", "-25", "Lower third(0+75)"]
    );
    // Guides -> timeline markers.
    let g: Vec<_> = t
        .tracks
        .markers
        .iter()
        .map(|m| (m.name.as_str(), m.marked_range.start_time.value, colour(m)))
        .collect();
    assert_eq!(g, [("Act 2", 100.0, "RED"), ("End", 240.0, "PURPLE")]);
    // The geometry composite is kept but not rendered; Kdenlive's internal ones are silent.
    assert!(r.unmapped.contains(&"composition composite".to_owned()));
    let kept = t.tracks.foreign("mlt").expect("stack foreign")["children"]
        .as_array()
        .expect("children")
        .len();
    assert_eq!(kept, 3);
    assert_eq!(t.ffcine_value("frame_size"), Some(&json!([1920, 1080])));
    assert!(t.foreign("mlt").is_some());
    assert_ids(&t);
}

#[test]
fn kdenlive_effects() {
    let (t, r) = load("features.kdenlive");
    let beach = clip(track(&t, "Main"), "beach.mp4");
    let tf = one_fx(beach, "ffcine.geo.transform");
    let pos = keys(&pair(&tf, "translate"));
    assert_eq!(pos.len(), 2);
    let s0 = start(beach);
    assert_eq!((pos[0].0, pos[1].0), (s0, s0 + 50.0));
    assert_eq!((pos[0].2.as_str(), pos[1].2.as_str()), ("linear", "ease"));
    approx(&pos[1].1, &[0.0, 0.0]);
    approx(&keys(&pair(&tf, "scale"))[1].1, &[0.5, 0.5]);
    assert_eq!(keys(&tf["rotate_deg"])[1].1, json!(-90.0));

    let blends = fx(beach, "ffcine.comp.blend");
    assert_eq!(blends.len(), 2);
    assert_eq!(blends[0]["mode"], "multiply");
    let op: Vec<_> = keys(&blends[0]["opacity"])
        .into_iter()
        .map(|k| k.1)
        .collect();
    assert_eq!(op, [json!(1.0), json!(0.5)]);
    // fade_from_black (filter in=50 on an entry starting at 50) -> opacity 0..1 over 24 frames.
    let fade: Vec<_> = keys(&blends[1]["opacity"])
        .into_iter()
        .map(|k| (k.0, k.1))
        .collect();
    assert_eq!(fade, [(s0, json!(0.0)), (s0 + 24.0, json!(1.0))]);

    let crop = one_fx(beach, "ffcine.geo.crop");
    assert_eq!(crop["left"], json!(0.1));
    assert_eq!(crop["top"], json!(0.1));
    assert_eq!(crop["bottom"], json!(0.05));
    assert_eq!(
        one_fx(beach, "ffcine.color.grade")["lut"],
        "/media/project/luts/look.cube"
    );
    let glow = beach
        .effects
        .iter()
        .find(|e| e.effect_name() == Some("frei0r.glow"))
        .expect("glow kept");
    assert!(glow.foreign("mlt").is_some());
    assert!(r.unmapped.contains(&"filter frei0r.glow".to_owned()));

    let music = clip(track(&t, "Music"), "music.wav");
    let levels = fx(music, "ffcine.audio.level");
    assert_eq!(levels[0]["gain_db"], json!(-6.0));
    assert_eq!(levels[1]["fade_in"], json!(1.0));

    let titles = track(&t, "Titles");
    let (kind, p) = generator(clip(titles, "Slate"));
    assert_eq!(kind, "ffcine.solid");
    approx(
        &p["colour"],
        &[32.0 / 255.0, 64.0 / 255.0, 96.0 / 255.0, 1.0],
    );
    let (kind, p) = generator(clip(titles, "Lower third"));
    assert_eq!(kind, "ffcine.text");
    assert_eq!(p["text"], "Old Town");
    assert_eq!(p["font"], "Noto Sans");
    assert_eq!(
        (p["size"].clone(), p["bold"].clone()),
        (json!(80.0), json!(true))
    );
    approx(
        &p["position"],
        &[560.0 / 1920.0 - 0.5, 930.0 / 1080.0 - 0.5],
    );
}

#[test]
fn shotcut_project() {
    let (t, r) = load("features.mlt");
    let names: Vec<_> = t
        .track_list()
        .map(|x| (x.name.as_str(), x.kind.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("V1", track_kind::VIDEO),
            ("V2", track_kind::VIDEO),
            ("A1", track_kind::AUDIO),
        ]
    );
    // A 15-frame transition tractor: the outgoing clip runs 7 frames into it, the incoming one
    // starts 8 frames before its end.
    let v1 = track(&t, "V1");
    assert_eq!(labels(v1), ["one.mp4(0+67)", "T(7+8)", "two.mp4(7+98)"]);
    assert_eq!(transition(v1).name, "luma");
    assert_eq!(transition(v1).ffcine_value("kind"), None);

    let one = clip(v1, "one.mp4");
    let tf = one_fx(one, "ffcine.geo.transform");
    let pos = keys(&pair(&tf, "translate"));
    let s0 = start(one);
    assert_eq!((pos[0].0, pos[1].0), (s0, s0 + 30.0));
    approx(&pos[0].1, &[0.0, 0.0]);
    approx(&pos[1].1, &[0.25, -0.25]);
    approx(&keys(&pair(&tf, "scale"))[1].1, &[0.5, 0.5]);
    assert_eq!(one_fx(one, "ffcine.comp.blend")["opacity"], json!(0.6));

    let two = clip(v1, "two.mp4");
    let ramp = one_fx(two, "ffcine.time.ramp");
    let fps = 30000.0 / 1001.0;
    let (s0, s1) = (fps / 44.0, 5.0 * fps / 45.0);
    let pts = ramp["points"].as_array().expect("points");
    assert_eq!(pts.len(), 4);
    for (p, want) in pts
        .iter()
        .zip([[0.0, s0], [1.0 / 6.0, s0], [1.0 / 6.0, s1], [1.0, s1]])
    {
        approx(p, &want);
    }
    assert_eq!(ramp["source_duration"], json!(6.0));
    let avg = two.effects.iter().find_map(|e| match e {
        Effect::LinearTimeWarp(w) => Some(w.time_scalar),
        _ => None,
    });
    assert!((avg.expect("average speed") - 6.0 * fps / 89.0).abs() < 1e-9);

    let v2 = track(&t, "V2");
    assert_eq!(labels(v2), ["-30", "transparent(0+90)"]);
    let (kind, p) = generator(clip(v2, "transparent"));
    assert_eq!(kind, "ffcine.text");
    assert_eq!(p["text"], "Hello Shotcut");
    assert_eq!(
        (p["size"].clone(), p["bold"].clone()),
        (json!(96.0), json!(true))
    );
    approx(&p["colour"], &[1.0, 1.0, 0.0, 1.0]);
    approx(&p["position"], &[0.0, 945.0 / 1080.0 - 0.5]);

    let music = clip(track(&t, "A1"), "music.wav");
    let fade: Vec<_> = keys(&one_fx(music, "ffcine.audio.level")["gain_db"])
        .into_iter()
        .map(|k| (k.0, k.1))
        .collect();
    assert_eq!(fade, [(0.0, json!(-60.0)), (30.0, json!(0.0))]);
    assert_eq!(r.unmapped, ["filter unknownThing"]);

    let m: Vec<_> = t
        .tracks
        .markers
        .iter()
        .map(|m| {
            (
                m.name.as_str(),
                m.marked_range.start_time.value,
                m.marked_range.duration.value,
                colour(m),
            )
        })
        .collect();
    assert_eq!(
        m,
        [("Beat", 30.0, 0.0, "GREEN"), ("Range", 60.0, 30.0, "RED")]
    );
    // Shotcut's per-track mix / blend transitions are not compositions.
    assert!(!r.warnings.iter().any(|w| w.contains("composition")));
    assert_ids(&t);
}

#[test]
fn melt_mix() {
    let (t, r) = load("melt_mix.mlt");
    let tracks: Vec<_> = t.track_list().collect();
    assert_eq!(tracks.len(), 2);
    assert_eq!(labels(tracks[0]), ["color(0+45)", "T(5+5)", "color(5+45)"]);
    let solid = |i: usize| match &tracks[0].children[i] {
        Item::Clip(c) => generator(c).1["colour"].clone(),
        _ => panic!("not a clip"),
    };
    assert_eq!(
        (solid(0), solid(2)),
        (json!([1.0, 0.0, 0.0, 1.0]), json!([0.0, 0.0, 1.0, 1.0]))
    );
    // The affine filter sits on the playlist: a track effect.
    let tf = tracks[1]
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::Effect(g) if g.effect_name == "ffcine.geo.transform" => g.ffcine(),
            _ => None,
        })
        .expect("track transform");
    let tf_v = &Value::Object(tf.clone());
    approx(&pair(tf_v, "translate"), &[-0.15, -0.15]);
    approx(&pair(tf_v, "scale"), &[0.5, 0.5]);
    assert_eq!(r.unmapped, Vec::<String>::new());
}

#[test]
fn kdenlive_24_sequence() {
    let (t, _) = load("kdenlive_example_v240770.kdenlive");
    let names: Vec<_> = t.track_list().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["Ambience", "A2", "A3", "Wide", "Close"]);
    let title = t
        .clips()
        .into_iter()
        .find(|c| matches!(c.media_reference(), Some(MediaReference::Generator(g)) if g.generator_kind == "ffcine.text"))
        .expect("title");
    assert_eq!(generator(title).1["text"], "Testing Kdenlive...");
    assert_eq!(t.tracks.markers.len(), 2);
    assert_ids(&t);
}

#[test]
fn kdenlive_mixes() {
    let (t, _) = load("kdenlive_mixes_markers.kdenlive");
    let transitions: usize = t
        .track_list()
        .map(|x| {
            x.children
                .iter()
                .filter(|i| matches!(i, Item::Transition(_)))
                .count()
        })
        .sum();
    assert_eq!(transitions, 10);
    // Every track still spans the same timeline once mixes are folded into cuts.
    let end = |x: &Track| -> f64 {
        x.children
            .iter()
            .filter(|i| !matches!(i, Item::Transition(_)))
            .map(|i| i.source_range().map_or(0.0, |r| r.duration.value))
            .sum()
    };
    let mix = track(&t, "With Mix");
    let plain = track(&t, "Nothing");
    assert!(end(mix) > 0.0 && end(plain) > 0.0);
    assert!(t.clips().iter().any(|c| {
        c.markers
            .iter()
            .any(|m| m.name == "Lila" && colour(m) == "PURPLE")
    }));
    assert_ids(&t);
}

#[test]
fn format_detection() {
    use ffcine_import::{Format, import};
    let fcp = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/fcpxml/bundle.fcpxmld"
    );
    for (path, format) in [
        (Path::new(DATA).join("features.kdenlive"), Format::Mlt),
        (Path::new(DATA).join("features.mlt"), Format::Mlt),
        (Path::new(fcp).to_owned(), Format::Fcpxml),
    ] {
        assert_eq!(Format::detect(&path), Some(format));
        let r = import(&path, None, &ImportOptions::default()).expect("import");
        assert!(r.timeline.track_list().count() > 0);
    }
    assert_eq!("fcpxml".parse::<Format>().ok(), Some(Format::Fcpxml));
    assert_eq!("kdenlive".parse::<Format>().ok(), Some(Format::Mlt));
    // A rate override can't retime an MLT project: ignored with a warning.
    let opts = ImportOptions {
        rate: Some(24.0),
        ..ImportOptions::default()
    };
    let r = mlt::import(&Path::new(DATA).join("features.kdenlive"), &opts).expect("import");
    assert!(
        r.report
            .warnings
            .iter()
            .any(|w| w.starts_with("rate 24 ignored"))
    );
}
