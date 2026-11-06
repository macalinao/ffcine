//! Backward compatibility of timelines across the compositing refactor (`docs/compositing.md`,
//! step 7): a synthetic timeline using every existing unit (shots with fades and a reframe, a
//! gap, each transition kind, a split-screen with stagger and radius, split dissolves in and
//! out, overlay clips with a rotation, a blend and a still, a caption) must plan the same keys
//! and render the same frames (`framemd5`) as the build before the refactor, recorded in
//! `tests/data/timeline_parity.json`.
//!
//! `cargo test -p ffcine --test timeline_parity -- --ignored --test-threads=1` (needs a Vulkan
//! GPU, NVENC and `ffmpeg` on PATH). `FFCINE_PARITY_RECORD=1` rewrites the fixture: do that only
//! on a build whose output is known good, and say why in the commit. The keys part needs no GPU
//! and always runs.

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::assembly::recipe::{Fingerprint, Fingerprinter};
use ffcine::assembly::{self, Profile, Renderer};
use ffcine::timeline::{
    Clip, Effect, ExternalReference, Gap, GeneratorReference, GenericEffect, HasMetadata, Item,
    MediaReference, Project, RationalTime, Stack, TimeRange, Timeline, Track, Transition,
    track_kind,
};
use ffcine::{Decode, Device, backend};
use serde_json::{Value, json};

const RATE: f64 = 24000.0 / 1001.0;
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/timeline_parity.json"
);

/// Fingerprints from the file name alone, so the keys don't depend on the temp directory.
struct ByName;
impl Fingerprinter for ByName {
    fn fingerprint(&self, path: &Path) -> Fingerprint {
        Fingerprint {
            path: PathBuf::from(path.file_name().unwrap_or_default()),
            size: Some(1),
            mtime_ns: Some(1),
        }
    }
    fn content_hash(&self, path: &Path) -> Option<String> {
        Some(format!(
            "hash-of-{}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
    }
}

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg on PATH");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn source(d: &Path, name: &str, filter: &str) -> PathBuf {
    let p = d.join(format!("{name}.mp4"));
    if !p.exists() {
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &format!("{filter}size=1920x1080:rate=24000/1001,format=yuv420p"),
            "-t",
            "4",
            "-c:v",
            "libx264",
            "-crf",
            "12",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
            p.to_str().expect("utf-8"),
        ]);
    }
    p
}

fn effect(schema: &str, v: &Value) -> Effect {
    Effect::Effect(GenericEffect::ffcine(
        schema,
        v.as_object().cloned().unwrap_or_default(),
    ))
}

fn clip(path: &Path, start: f64, frames: f64, fx: Vec<Effect>) -> Item {
    let mut c = Clip::new(
        path.file_stem().unwrap_or_default().to_string_lossy(),
        MediaReference::External(ExternalReference::from_path(path, None)),
        Some(TimeRange::from_frames(start, start + frames, RATE)),
    );
    c.effects = fx;
    Item::Clip(c)
}

fn transition(frames: f64, meta: Option<Value>) -> Item {
    let mut t = Transition::dissolve(
        RationalTime::new(0.0, RATE),
        RationalTime::new(frames, RATE),
    );
    if let Some(Value::Object(m)) = meta {
        t.ffcine_mut().extend(m);
    }
    Item::Transition(t)
}

fn kind(k: &str) -> Value {
    json!({"schema": "ffcine.transition/1", "kind": k})
}

fn gap(frames: f64) -> Item {
    Item::Gap(Gap::new(RationalTime::new(frames, RATE)))
}

fn split(panels: &[&Path], frames: f64, layout: &Value) -> Item {
    let mut s = Stack {
        name: "split".into(),
        ..Stack::default()
    };
    for p in panels {
        let mut t = Track::new("panel", track_kind::VIDEO);
        t.children = vec![clip(p, 0.0, frames, Vec::new())];
        s.children.push(Item::Track(t));
    }
    s.effects.push(effect("ffcine.comp.layout/1", layout));
    Item::Stack(s)
}

/// The timeline (in `d`, whose media it makes) and its directory.
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn timeline(d: &Path) -> Timeline {
    let a = source(d, "a", "testsrc2=");
    let b = source(d, "b", "smptebars=");
    let c = source(d, "c", "testsrc=");
    let e = source(d, "e", "rgbtestsrc=");
    let logo = d.join("logo.png");
    if !logo.exists() {
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=240x160",
            "-frames:v",
            "1",
            logo.to_str().expect("utf-8"),
        ]);
    }

    let mut v1 = Track::new("V1", track_kind::VIDEO);
    v1.children = vec![
        clip(
            &a,
            0.0,
            36.0,
            vec![
                effect("ffcine.fx.fade/1", &json!({"in": 0.5})),
                effect("ffcine.fx.vignette/1", &json!({"amount": 0.4})),
                effect(
                    "ffcine.geo.reframe/1",
                    &json!({"zoom": 1.2, "zoom_center": [0.3, 0.4]}),
                ),
            ],
        ),
        transition(12.0, None),
        clip(&b, 10.0, 36.0, Vec::new()),
        transition(10.0, Some(kind("dip"))),
        clip(&c, 0.0, 36.0, Vec::new()),
        transition(10.0, Some(kind("flash"))),
        clip(&e, 0.0, 36.0, Vec::new()),
        transition(8.0, Some(kind("whip"))),
        clip(&a, 40.0, 30.0, Vec::new()),
        gap(12.0),
        clip(&b, 0.0, 30.0, Vec::new()),
        // Split dissolve in: shot -> split.
        transition(10.0, None),
        split(
            &[&a, &b, &c],
            60.0,
            &json!({"layout": "split", "stagger": 0.2, "slide": 0.4, "radius": 0.03}),
        ),
        // Split dissolve out: split -> shot.
        transition(10.0, None),
        clip(&e, 10.0, 30.0, Vec::new()),
        split(
            &[&c, &e],
            40.0,
            &json!({"layout": "column", "gap": 0.02, "enter": "none", "radius": 0.0}),
        ),
    ];

    let place = |x: f64, s: f64, rot: f64| {
        effect(
            "ffcine.geo.transform/1",
            &json!({"units": "frame", "y": "down", "translate_x": x, "scale_x": s,
                    "scale_y": s, "rotate_deg": rot}),
        )
    };
    let mut pip = Track::new("PiP 1", track_kind::VIDEO);
    pip.children = vec![
        gap(20.0),
        clip(&logo, 0.0, 40.0, vec![place(0.3, 0.2, 0.0)]),
        gap(60.0),
        clip(
            &c,
            0.0,
            40.0,
            vec![
                place(-0.25, 0.4, 12.0),
                effect(
                    "ffcine.comp.blend/1",
                    &json!({"opacity": 0.8, "radius": 0.03, "mode": "multiply"}),
                ),
            ],
        ),
    ];

    let mut text = Track::new("Text", track_kind::VIDEO);
    let mut params = serde_json::Map::new();
    params.insert("text".into(), json!("Parity"));
    text.children = vec![
        gap(30.0),
        Item::Clip(Clip::new(
            "caption",
            MediaReference::Generator(GeneratorReference::new("ffcine.text", params)),
            Some(TimeRange::from_frames(0.0, 48.0, RATE)),
        )),
    ];

    let mut t = Timeline::new("parity");
    t.set_project(&Project {
        rate: Some(RATE),
        ..Project::default()
    })
    .expect("project");
    t.tracks.children = vec![Item::Track(v1), Item::Track(pip), Item::Track(text)];
    t
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-parity-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn record() -> bool {
    std::env::var_os("FFCINE_PARITY_RECORD").is_some()
}

fn fixture() -> Value {
    std::fs::read_to_string(FIXTURE)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save(key: &str, v: Value) {
    let mut f = fixture();
    f[key] = v;
    std::fs::create_dir_all(Path::new(FIXTURE).parent().expect("dir")).expect("mkdir");
    std::fs::write(
        FIXTURE,
        serde_json::to_string_pretty(&f).expect("json") + "\n",
    )
    .expect("write fixture");
}

/// The plan (with the caption) and its keys.
fn plan(d: &Path, t: &Timeline) -> (assembly::Plan, assembly::TextLayer) {
    let text = assembly::TextLayer::from_timeline(t, d).expect("text");
    let p = assembly::plan(t, d, &Profile::preview(), &ByName, Some(&text)).expect("plan");
    (p, text)
}

#[test]
fn timeline_plans_the_recorded_keys() {
    // Keys only: the media needn't exist (names are fingerprinted), but the timeline is built
    // the same way, so make the files when ffmpeg is there and fall back to names otherwise.
    let d = dir();
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let t = timeline(&d);
    let (p, _) = plan(&d, &t);
    let keys: Vec<Value> = p
        .segments
        .iter()
        .map(|s| json!([s.name, s.frames, s.key.0]))
        .collect();
    let got = json!({"segments": keys, "master": p.master_key().0, "warnings": p.warnings});
    if record() {
        save("plan", got);
        return;
    }
    let want = fixture()["plan"].clone();
    assert_eq!(got, want, "plan keys changed (see the module docs)");
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
fn timeline_renders_the_recorded_frames() {
    let d = dir();
    let t = timeline(&d);
    let (p, mut text) = plan(&d, &t);
    let ff = backend::Ffmpeg::new().expect("backend");
    let r = Renderer {
        backend: &ff,
        cache: d.join("cache"),
        ffmpeg: "ffmpeg".into(),
        device: Device::Auto,
        decode: Decode::default(),
        progress: assembly::Progress::Human,
        grader: None,
        detect: None,
    };
    let out = d.join("master.mp4");
    r.render(&p, Some(&mut text), &out).expect("render");
    let md5 = ffmpeg(&[
        "-i",
        out.to_str().expect("utf-8"),
        "-map",
        "0:v:0",
        "-f",
        "framemd5",
        "-",
    ]);
    let frames: Vec<Value> = String::from_utf8_lossy(&md5)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.rsplit(',').next().map(|h| json!(h.trim())))
        .collect();
    assert_eq!(i64::try_from(frames.len()).expect("len"), p.frames());
    if record() {
        save("framemd5", Value::Array(frames));
        return;
    }
    let want = fixture()["framemd5"].clone();
    let want = want.as_array().expect("recorded framemd5s");
    let bad: Vec<usize> = (0..frames.len().max(want.len()))
        .filter(|&i| frames.get(i) != want.get(i))
        .collect();
    assert!(
        bad.is_empty(),
        "{} of {} frames differ from the recorded build, first at {:?}",
        bad.len(),
        want.len(),
        bad.first()
    );
}
