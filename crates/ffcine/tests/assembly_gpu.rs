//! `ffcine render` of a timeline using every compositing feature (`cargo test -p ffcine --test
//! assembly_gpu -- --ignored`; needs a Vulkan GPU, NVENC and `ffmpeg` on PATH): vignette and
//! fades, dip/whip/dissolve transitions, a 3-up split-screen, a PNG logo and a picture-in-picture
//! over the picture, film grain. Checks the frame count and a few pixels of the master.

use std::path::{Path, PathBuf};
use std::process::Command;

use ffcine::assembly::{self, FsFingerprinter, Profile, Renderer};
use ffcine::timeline::{
    Clip, Effect, ExternalReference, GenericEffect, HasMetadata, Item, MediaReference, Project,
    RationalTime, Stack, TimeRange, Timeline, Track, Transition, track_kind,
};
use ffcine::{Decode, Device, backend};
use serde_json::{Value, json};

const RATE: f64 = 24000.0 / 1001.0;

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-assembly-gpu-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
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
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("{filter}size=1920x1080:rate=24000/1001,format=yuv420p"),
        "-t",
        "6",
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

fn rgb_frame(path: &Path, n: i64) -> Vec<u8> {
    ffmpeg(&[
        "-i",
        path.to_str().expect("utf-8"),
        "-vf",
        &format!("select=eq(n\\,{n})"),
        "-frames:v",
        "1",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "-",
    ])
}

fn px(f: &[u8], x: usize, y: usize) -> [i32; 3] {
    let i = (y * 1920 + x) * 3;
    [f[i].into(), f[i + 1].into(), f[i + 2].into()]
}

fn near(a: [i32; 3], b: [i32; 3], tol: i32) -> bool {
    a.iter().zip(&b).all(|(x, y)| (x - y).abs() <= tol)
}

#[test]
#[ignore = "needs a Vulkan GPU, NVENC and ffmpeg"]
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn renders_effects_splits_and_overlays() {
    let d = dir();
    let a = source(&d, "a", "testsrc2=");
    let b = source(&d, "b", "color=c=blue:");
    let c = source(&d, "c", "smptebars=");
    let e = source(&d, "e", "color=c=green:");
    let logo = d.join("logo.png");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=red:size=200x200",
        "-frames:v",
        "1",
        logo.to_str().expect("utf-8"),
    ]);

    // V1: a (vignette, fade in) | dip to white | b | whip | c | dissolve | e | 3-up split.
    let mut v1 = Track::new("V1", track_kind::VIDEO);
    let mut split = Stack {
        name: "split".into(),
        ..Stack::default()
    };
    for p in [&a, &b, &c] {
        let mut t = Track::new("panel", track_kind::VIDEO);
        t.children = vec![clip(p, 0.0, 48.0, Vec::new())];
        split.children.push(Item::Track(t));
    }
    split
        .effects
        .push(effect("ffcine.comp.layout/1", &json!({"layout": "split"})));
    v1.children = vec![
        clip(
            &a,
            0.0,
            48.0,
            vec![
                effect("ffcine.fx.vignette/1", &json!({"amount": 0.5})),
                effect("ffcine.fx.fade/1", &json!({"in": 0.5})),
            ],
        ),
        transition(
            12.0,
            Some(json!({"schema": "ffcine.transition/1", "kind": "dip", "colour": "white"})),
        ),
        clip(&b, 0.0, 48.0, Vec::new()),
        transition(
            8.0,
            Some(json!({"schema": "ffcine.transition/1", "kind": "whip"})),
        ),
        clip(&c, 0.0, 48.0, Vec::new()),
        transition(12.0, None),
        clip(&e, 0.0, 48.0, Vec::new()),
        Item::Stack(split),
    ];
    // PiP 1: the logo over frames 10..40, a picture-in-picture of `a` over `e`.
    let mut pip = Track::new("PiP 1", track_kind::VIDEO);
    let place = |x: f64, s: f64| {
        effect(
            "ffcine.geo.transform/1",
            &json!({"units": "frame", "y": "down", "translate_x": x, "scale_x": s, "scale_y": s}),
        )
    };
    pip.children = vec![
        Item::Gap(ffcine::timeline::Gap::new(RationalTime::new(10.0, RATE))),
        clip(&logo, 0.0, 30.0, vec![place(0.3, 0.2)]),
        Item::Gap(ffcine::timeline::Gap::new(RationalTime::new(124.0, RATE))),
        clip(
            &a,
            0.0,
            30.0,
            vec![
                place(-0.25, 0.4),
                effect(
                    "ffcine.comp.blend/1",
                    &json!({"opacity": 1.0, "radius": 0.03}),
                ),
            ],
        ),
    ];
    let mut t = Timeline::new("effects");
    t.set_project(&Project {
        rate: Some(RATE),
        ..Project::default()
    })
    .expect("project");
    t.tracks.children = vec![Item::Track(v1), Item::Track(pip)];

    let profile = Profile::preview();
    let plan = assembly::plan(&t, &d, &profile, &FsFingerprinter, None).expect("plan");
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
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
    r.render(&plan, None, &out).expect("render");

    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-count_packets", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"])
        .arg(&out)
        .output()
        .expect("ffprobe");
    let n: i64 = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse()
        .expect("count");
    assert_eq!(n, plan.frames());

    // Timeline frames (each transition overlaps the incoming clip's head): a 0..48, dip 48..60,
    // b 60..96, whip 96..104, c 104..144, dissolve 144..156, e 156..192, split 192..240.
    let f = rgb_frame(&out, 0);
    assert!(
        near(px(&f, 960, 540), [0, 0, 0], 12),
        "fade in starts black"
    );
    let f = rgb_frame(&out, 20);
    assert!(
        near(px(&f, 1536, 540), [255, 0, 0], 24),
        "logo: {:?}",
        px(&f, 1536, 540)
    );
    assert!(
        px(&f, 4, 4).iter().all(|v| *v < 40),
        "vignette corner: {:?}",
        px(&f, 4, 4)
    );
    let f = rgb_frame(&out, 54);
    assert!(
        px(&f, 960, 540).iter().all(|v| *v > 200),
        "dip to white: {:?}",
        px(&f, 960, 540)
    );
    let f = rgb_frame(&out, 170);
    let pip_px = px(&f, 480, 540);
    assert!(
        !near(pip_px, px(&f, 1700, 900), 30),
        "pip over e: {pip_px:?}"
    );
    let f = rgb_frame(&out, 238);
    // The gap between panels 1 and 2 shows the backdrop.
    assert!(
        near(px(&f, 638, 540), [22, 20, 18], 14),
        "backdrop: {:?}",
        px(&f, 638, 540)
    );
    assert!(
        near(px(&f, 960, 540), [0, 0, 255], 40),
        "middle panel (b): {:?}",
        px(&f, 960, 540)
    );

    // Re-planning the same timeline renders nothing.
    let again = r.render(&plan, None, &out).expect("re-render");
    assert_eq!(again.rendered, 0);
}
