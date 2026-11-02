//! End to end through libav*: WAV files on disk, sync into a timeline, render the mix.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    clippy::unwrap_used
)]

use std::f64::consts::PI;
use std::path::{Path, PathBuf};

use ffcine_audio::decode::{self, Request};
use ffcine_audio::schema::{self, LevelMode};
use ffcine_audio::{Audio, Bus, Clean, Level, MixOptions, SyncOptions, WriteOptions, render_audio};
use ffcine_timeline::{
    Clip, ExternalReference, HasMetadata, Item, MediaReference, RationalTime, TimeRange, Timeline,
    Track, track_kind,
};

const R: u32 = 48000;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-audio-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("tmp dir");
    d
}

fn noise(seed: u64, n: usize) -> Vec<f32> {
    let mut x = seed.max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        })
        .collect()
}

/// Gated noise, a syllable every ~40 ms.
fn scene(seed: u64, seconds: f64) -> Vec<f32> {
    let n = (seconds * f64::from(R)) as usize;
    let mut x = noise(seed, n);
    let g = noise(seed ^ 0x55, n / 2000 + 1);
    for (i, s) in x.iter_mut().enumerate() {
        *s *= g[i / 2000].max(0.0) * 0.5;
    }
    x
}

fn sine(f: f64, amp: f32, seconds: f64) -> Vec<f32> {
    (0..(seconds * f64::from(R)) as usize)
        .map(|i| amp * (2.0 * PI * f * i as f64 / f64::from(R)).sin() as f32)
        .collect()
}

fn write(path: &Path, x: Vec<f32>) {
    Audio::mono(R, x).write_wav(path).expect("write wav");
}

fn secs(s: f64) -> usize {
    (s * f64::from(R)) as usize
}

fn rms_db(x: &[f32]) -> f64 {
    let ms = x.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / x.len().max(1) as f64;
    10.0 * ms.max(1e-20).log10()
}

#[test]
fn decode_range_is_sample_exact() {
    let d = dir("range");
    let x = noise(3, secs(6.0));
    let p = d.join("n.wav");
    write(&p, x.clone());
    let a = decode::load(
        &p,
        &Request {
            start: 3.25,
            duration: Some(1.0),
            rate: R,
            channels: 1,
            filter: None,
        },
    )
    .expect("load");
    assert_eq!(a.frames(), secs(1.0));
    let want = &x[secs(3.25)..secs(4.25)];
    let err = a.channels[0]
        .iter()
        .zip(want)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max);
    assert!(err < 1e-5, "err {err}");
}

#[test]
fn clean_chain_cuts_rumble() {
    let d = dir("clean");
    let p = d.join("rumble.wav");
    let x: Vec<f32> = sine(40.0, 0.3, 4.0)
        .iter()
        .zip(sine(1000.0, 0.3, 4.0))
        .map(|(a, b)| a + b)
        .collect();
    write(&p, x);
    let clean = Clean {
        wind: true,
        declick: true,
        ..Clean::default()
    };
    let spec = clean.filter_spec(LevelMode::Dialogue);
    let a = decode::load(
        &p,
        &Request {
            start: 1.0,
            duration: Some(2.0),
            rate: R,
            channels: 2,
            filter: Some(&spec),
        },
    )
    .expect("clean");
    assert_eq!(a.frames(), secs(2.0));
    // Project onto each tone.
    let tone = |f: f64| {
        let c = &a.channels[0];
        let (mut s, mut co) = (0.0, 0.0);
        for (i, v) in c.iter().enumerate() {
            let t = 2.0 * PI * f * i as f64 / f64::from(R);
            s += f64::from(*v) * t.sin();
            co += f64::from(*v) * t.cos();
        }
        (s * s + co * co).sqrt() / c.len() as f64
    };
    let drop = 20.0 * (tone(1000.0) / tone(40.0)).log10();
    assert!(drop > 30.0, "40 Hz only {drop:.1} dB under 1 kHz");
}

#[test]
fn sync_writes_timeline_and_renders() {
    let d = dir("sync");
    let rec = scene(7, 90.0);
    let mut cam1 = rec[secs(10.5)..secs(30.5)].to_vec();
    let mut cam2 = rec[secs(50.25)..secs(80.25)].to_vec();
    for (c, n) in cam1
        .iter_mut()
        .chain(cam2.iter_mut())
        .zip(noise(9, secs(50.0)))
    {
        *c = *c * 0.4 + n * 0.01;
    }
    let (rp, c1, c2) = (
        d.join("ZOOM0001.WAV"),
        d.join("C0001.wav"),
        d.join("C0002.wav"),
    );
    write(&rp, rec);
    write(&c1, cam1);
    write(&c2, cam2);
    let recs = vec![rp.clone()];
    let syncs = ffcine_audio::sync_files(&[c1, c2], &recs, &SyncOptions::default()).expect("sync");
    let offs: Vec<f64> = syncs
        .iter()
        .map(|s| s.found.as_ref().expect("found").offset)
        .collect();
    assert!(
        (offs[0] - 10.5).abs() < 1e-4 && (offs[1] - 50.25).abs() < 1e-4,
        "{offs:?}"
    );
    print!("{}", ffcine_audio::sync::report(&syncs, &recs));

    let tl = ffcine_audio::write_timeline(None, &syncs, &recs, &WriteOptions::default())
        .expect("timeline");
    let a1 = tl
        .track_list()
        .find(|t| t.kind == track_kind::AUDIO)
        .expect("audio track");
    let starts: Vec<f64> = a1
        .children
        .iter()
        .filter_map(|i| match i {
            Item::Clip(c) => c.source_range.as_ref().map(|r| r.start_time.to_seconds()),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2);
    assert!(
        (starts[0] - 10.5).abs() < 1e-4 && (starts[1] - 50.25).abs() < 1e-4,
        "{starts:?}"
    );

    // J/L cuts: overlapping neighbours go to alternate tracks.
    let opts = WriteOptions {
        lead: 0.5,
        tail: 0.5,
        ..WriteOptions::default()
    };
    let jl = ffcine_audio::write_timeline(None, &syncs, &recs, &opts).expect("jl");
    let audio: Vec<&Track> = jl
        .track_list()
        .filter(|t| t.kind == track_kind::AUDIO)
        .collect();
    assert_eq!(audio.len(), 2);

    // The mix is the recorder audio in place of each clip.
    let opts = MixOptions {
        master: ffcine_audio::Master {
            lufs: None,
            ..ffcine_audio::Master::default()
        },
        ..MixOptions::default()
    };
    let mix = render_audio(&tl, &opts).expect("render");
    assert_eq!(mix.master.frames(), secs(50.0));
    let path = d.join("mix.wav");
    ffcine_audio::write_mix(&mix, &path, Some(&d.join("stems"))).expect("write");
    assert!(d.join("stems/dialogue.wav").exists());
}

fn clip(path: &Path, start: f64, dur: f64, effects: Vec<ffcine_timeline::Effect>) -> Item {
    let mut c = Clip::new(
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("clip"),
        MediaReference::External(ExternalReference::from_path(path, None)),
        Some(TimeRange::new(
            RationalTime::from_seconds(start, 48000.0),
            RationalTime::from_seconds(dur, 48000.0),
        )),
    );
    c.effects = effects;
    Item::Clip(c)
}

#[test]
fn music_ducks_under_dialogue_and_master_hits_target() {
    let d = dir("duck");
    let (tp, mp) = (d.join("talk.wav"), d.join("music.wav"));
    write(&tp, scene(1, 3.0));
    write(&mp, sine(440.0, 0.3, 10.0));

    // Dialogue 4..7 s (mode orig: ducks the music 14 dB), music 0..10 s.
    let mut dia = Track::new("Dialogue 1", track_kind::AUDIO);
    dia.children.push(Item::Gap(ffcine_timeline::Gap::new(
        RationalTime::from_seconds(4.0, 48000.0),
    )));
    let orig = Level {
        mode: Some(LevelMode::Dialogue),
        ..Level::default()
    };
    dia.children.push(clip(
        &tp,
        0.0,
        3.0,
        vec![schema::make_effect("level", schema::LEVEL, &orig).expect("fx")],
    ));
    let mut music = Track::new("Score", track_kind::AUDIO);
    music.ffcine_mut().insert("bus".into(), "music".into());
    music.children.push(clip(&mp, 0.0, 10.0, vec![]));
    let mut tl = Timeline::new("duck");
    tl.tracks
        .children
        .extend([Item::Track(dia), Item::Track(music)]);

    let mix = render_audio(&tl, &MixOptions::default()).expect("render");
    let (_, m) = mix
        .buses
        .iter()
        .find(|b| b.0 == Bus::Music)
        .expect("music bus");
    let open = rms_db(&m.channels[0][secs(1.0)..secs(2.0)]);
    let ducked = rms_db(&m.channels[0][secs(5.0)..secs(6.0)]);
    assert!(
        (open - ducked - 14.0).abs() < 0.5,
        "open {open:.1} ducked {ducked:.1}"
    );
    // The duck starts ahead of the dialogue (look-ahead) and is gone a second after it.
    assert!(rms_db(&m.channels[0][secs(3.95)..secs(4.0)]) < open - 3.0);
    assert!((rms_db(&m.channels[0][secs(8.0)..secs(9.0)]) - open).abs() < 0.5);
    // Music levelled to -20 LUFS (on the whole track) before ducking.
    let l = ffcine_audio::mix::loudness(&Audio {
        rate: R,
        channels: m
            .channels
            .iter()
            .map(|c| c[secs(0.5)..secs(3.0)].to_vec())
            .collect(),
    })
    .expect("lufs")
    .expect("not silent");
    assert!((l + 20.0).abs() < 0.5, "music {l:.2} LUFS");

    assert!(
        (mix.lufs + 14.0).abs() <= 0.3,
        "master {:.2} LUFS",
        mix.lufs
    );
    // -1 dBTP with 2 dB headroom for the AAC encode.
    assert!(mix.true_peak <= -2.9, "true peak {:.2}", mix.true_peak);
}

#[test]
fn clean_chain_keeps_sync() {
    let d = dir("latency");
    let p = d.join("talk.wav");
    let x = scene(4, 6.0);
    write(&p, x.clone());
    let spec = Clean::default().filter_spec(LevelMode::Dialogue);
    let a = decode::load(
        &p,
        &Request {
            start: 2.0,
            duration: Some(3.0),
            rate: R,
            channels: 1,
            filter: Some(&spec),
        },
    )
    .expect("clean");
    let want = &x[secs(2.0)..secs(5.0)];
    let c = ffcine_audio::dsp::xcorr(want, &a.channels[0]);
    let (k, _) = c
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .expect("xcorr");
    assert_eq!(
        k as isize - (want.len() as isize - 1),
        0,
        "clean chain shifted the audio"
    );
}

/// `project.mix` in the timeline sets the duck and the master; `length` (a render's picture
/// length) cuts or pads the mix.
#[test]
fn project_mix_and_length() {
    let d = dir("project-mix");
    let (tp, mp) = (d.join("talk.wav"), d.join("music.wav"));
    write(&tp, scene(2, 3.0));
    write(&mp, sine(440.0, 0.3, 10.0));
    let mut dia = Track::new("Dialogue 1", track_kind::AUDIO);
    dia.children.push(Item::Gap(ffcine_timeline::Gap::new(
        RationalTime::from_seconds(4.0, 48000.0),
    )));
    dia.children.push(clip(&tp, 0.0, 3.0, vec![]));
    let mut music = Track::new("Music", track_kind::AUDIO);
    music.children.push(clip(&mp, 0.0, 10.0, vec![]));
    let mut tl = Timeline::new("mix");
    tl.tracks
        .children
        .extend([Item::Track(dia), Item::Track(music)]);
    let project = ffcine_timeline::Project {
        mix: ffcine_timeline::MixParams {
            duck_db: Some(-8.0),
            lufs: Some(-16.0),
            ..ffcine_timeline::MixParams::default()
        },
        ..ffcine_timeline::Project::default()
    };
    tl.set_project(&project).expect("project");

    let mut opts = MixOptions::for_timeline(&tl, None).expect("options");
    assert_eq!(opts.master.lufs, Some(-16.0));
    opts.length = Some(8.5);
    let mix = render_audio(&tl, &opts).expect("render");
    assert_eq!(mix.master.frames(), secs(8.5));
    let (_, m) = mix
        .buses
        .iter()
        .find(|b| b.0 == Bus::Music)
        .expect("music bus");
    let open = rms_db(&m.channels[0][secs(1.0)..secs(2.0)]);
    let ducked = rms_db(&m.channels[0][secs(5.0)..secs(6.0)]);
    assert!(
        (open - ducked - 8.0).abs() < 0.5,
        "open {open:.1} ducked {ducked:.1}"
    );
    assert!((mix.lufs + 16.0).abs() <= 0.3, "master {:.2}", mix.lufs);
}

/// Dialogue 4..7 s over a music track 0..10 s, with `project.mix.buses` from `buses`.
fn routed(name: &str, buses: serde_json::Value) -> (Timeline, MixOptions) {
    let d = dir(name);
    let (tp, mp) = (d.join("talk.wav"), d.join("music.wav"));
    write(&tp, scene(2, 3.0));
    write(&mp, sine(440.0, 0.3, 10.0));
    let mut dia = Track::new("Dialogue 1", track_kind::AUDIO);
    dia.children.push(Item::Gap(ffcine_timeline::Gap::new(
        RationalTime::from_seconds(4.0, 48000.0),
    )));
    let quiet = Level {
        duck_db: Some(0.0),
        ..Level::default()
    };
    dia.children.push(clip(
        &tp,
        0.0,
        3.0,
        vec![schema::make_effect("level", schema::LEVEL, &quiet).expect("fx")],
    ));
    let mut music = Track::new("Music", track_kind::AUDIO);
    music.children.push(clip(&mp, 0.0, 10.0, vec![]));
    let mut tl = Timeline::new("buses");
    tl.tracks
        .children
        .extend([Item::Track(dia), Item::Track(music)]);
    let project = ffcine_timeline::Project {
        mix: ffcine_timeline::MixParams {
            normalize: Some(false),
            buses: serde_json::from_value(buses).expect("buses"),
            ..ffcine_timeline::MixParams::default()
        },
        ..ffcine_timeline::Project::default()
    };
    tl.set_project(&project).expect("project");
    let opts = MixOptions::for_timeline(&tl, None).expect("options");
    (tl, opts)
}

fn stem(mix: &ffcine_audio::Mix, bus: Bus) -> &Audio {
    &mix.buses.iter().find(|b| b.0 == bus).expect("bus").1
}

fn aux<'a>(mix: &'a ffcine_audio::Mix, name: &str) -> &'a Audio {
    &mix.aux.iter().find(|b| b.0 == name).expect("aux").1
}

#[test]
fn bus_gain_moves_the_stem() {
    let (tl, opts) = routed("bus-gain", serde_json::json!({}));
    let flat = render_audio(&tl, &opts).expect("render");
    let (tl, opts) = routed("bus-gain2", serde_json::json!({"music": {"gain_db": -6.0}}));
    let cut = render_audio(&tl, &opts).expect("render");
    let span = secs(1.0)..secs(2.0);
    let (a, b) = (
        rms_db(&stem(&flat, Bus::Music).channels[0][span.clone()]),
        rms_db(&stem(&cut, Bus::Music).channels[0][span.clone()]),
    );
    assert!((a - b - 6.0).abs() < 0.01, "{a:.2} -> {b:.2}");
    // Only music plays there, and normalize is off: the master follows.
    let (a, b) = (
        rms_db(&flat.master.channels[0][span.clone()]),
        rms_db(&cut.master.channels[0][span]),
    );
    assert!((a - b - 6.0).abs() < 0.1, "master {a:.2} -> {b:.2}");
    // The dialogue isn't touched.
    let span = secs(5.0)..secs(6.0);
    let (a, b) = (
        rms_db(&stem(&flat, Bus::Dialogue).channels[0][span.clone()]),
        rms_db(&stem(&cut, Bus::Dialogue).channels[0][span]),
    );
    assert!((a - b).abs() < 0.01, "dialogue {a:.2} -> {b:.2}");
}

#[test]
fn reverb_send_rings_past_the_dialogue() {
    let (tl, opts) = routed(
        "reverb",
        serde_json::json!({
            "dialogue": {"sends": [{"to": "room", "gain_db": -6.0}]},
            "room": {"reverb": {"room": 0.8}, "gain_db": -3.0},
        }),
    );
    let mix = render_audio(&tl, &opts).expect("render");
    let wet = &aux(&mix, "room").channels[0];
    assert_eq!(wet.len(), mix.master.frames());
    // Nothing before the dialogue, sound during it, and a tail after it.
    assert!(rms_db(&wet[secs(1.0)..secs(3.0)]) < -100.0);
    let during = rms_db(&wet[secs(5.0)..secs(6.0)]);
    let tail = rms_db(&wet[secs(7.2)..secs(7.6)]);
    assert!(during > -60.0, "during {during:.1}");
    assert!(tail > -80.0 && tail < during, "{during:.1} {tail:.1}");
    let dry = &stem(&mix, Bus::Dialogue).channels[0];
    assert!(rms_db(&dry[secs(7.2)..secs(7.6)]) < -100.0);
    // The master has the return.
    assert!(rms_db(&mix.master.channels[0][secs(8.2)..secs(8.4)]) > -60.0);
}

/// A post-fader send follows the fader; a pre-fader one doesn't. And the return's level is the
/// bus's level plus the send.
#[test]
fn send_taps_before_or_after_the_fader() {
    let level = |pre: bool, gain: f64| {
        let (tl, opts) = routed(
            &format!("send-{pre}-{gain}"),
            serde_json::json!({
                "music": {"gain_db": gain, "sends": [{"to": "copy", "gain_db": -6.0, "pre": pre}]},
                "copy": {},
            }),
        );
        let mix = render_audio(&tl, &opts).expect("render");
        let m = rms_db(&stem(&mix, Bus::Music).channels[0][secs(1.0)..secs(2.0)]);
        let c = rms_db(&aux(&mix, "copy").channels[0][secs(1.0)..secs(2.0)]);
        (m, c)
    };
    let (m, c) = level(false, 0.0);
    assert!((m - c - 6.0).abs() < 0.01, "{m:.2} {c:.2}");
    let (m2, c2) = level(false, -10.0);
    assert!((m - m2 - 10.0).abs() < 0.01 && (c - c2 - 10.0).abs() < 0.01);
    let (m3, c3) = level(true, -10.0);
    assert!((m - m3 - 10.0).abs() < 0.01 && (c - c3).abs() < 0.01);
}

/// A send to a key bus ducks its target while the key is loud, and isn't heard itself.
#[test]
fn key_bus_ducks_its_target() {
    let (tl, opts) = routed(
        "key",
        serde_json::json!({
            "dialogue": {"sends": [{"to": "key"}]},
            "key": {"duck": {"targets": ["music"], "depth_db": -9.0}},
        }),
    );
    let mix = render_audio(&tl, &opts).expect("render");
    assert!(mix.aux.is_empty());
    let m = &stem(&mix, Bus::Music).channels[0];
    let open = rms_db(&m[secs(1.0)..secs(2.0)]);
    let ducked = rms_db(&m[secs(5.0)..secs(6.0)]);
    // Gated noise, so the key opens and closes: well under the open level, at most the depth.
    assert!(
        open - ducked > 3.0 && open - ducked <= 9.1,
        "{open:.1} {ducked:.1}"
    );
    assert!((rms_db(&m[secs(8.5)..secs(9.0)]) - open).abs() < 0.2);
}

#[test]
fn bus_filter_runs_in_sync() {
    let (tl, plain) = routed("bus-filter", serde_json::json!({}));
    let flat = render_audio(&tl, &plain).expect("render");
    let (tl, opts) = routed(
        "bus-filter2",
        serde_json::json!({"music": {"filter": "afftdn=nr=1:nf=-80,highpass=f=4000"},
                           "dialogue": {"filter": "lowpass=f=20000"}}),
    );
    let mix = render_audio(&tl, &opts).expect("render");
    let cut = rms_db(&stem(&mix, Bus::Music).channels[0][secs(1.0)..secs(2.0)]);
    let open = rms_db(&stem(&flat, Bus::Music).channels[0][secs(1.0)..secs(2.0)]);
    assert!(open - cut > 20.0, "{open:.1} -> {cut:.1}");
    // Still lined up with the unfiltered dialogue.
    let (a, b) = (
        &stem(&flat, Bus::Dialogue).channels[0][secs(4.5)..secs(6.5)],
        &stem(&mix, Bus::Dialogue).channels[0][secs(4.5)..secs(6.5)],
    );
    let c = ffcine_audio::dsp::xcorr(a, b);
    let (k, _) = c
        .iter()
        .enumerate()
        .max_by(|x, y| x.1.total_cmp(y.1))
        .expect("xcorr");
    assert_eq!(k as isize - (a.len() as isize - 1), 0);
}

#[test]
fn bad_routing_is_refused() {
    for buses in [
        serde_json::json!({"music": {"sends": [{"to": "nowhere"}]}}),
        serde_json::json!({"music": {"sends": [{"to": "dialogue"}]}, "dialogue": {}}),
        serde_json::json!({"music": {"reverb": {}}}),
        serde_json::json!({"room": {"sends": [{"to": "room"}]}}),
        serde_json::json!({"../room": {}}),
        serde_json::json!({"key": {"duck": {"targets": ["nowhere"]}}}),
    ] {
        let (tl, opts) = routed("bad-routing", buses.clone());
        assert!(render_audio(&tl, &opts).is_err(), "{buses}");
    }
}

/// The clip paths through the scratch files give what they should: a loop of a short file fills a
/// clip, with its seams, and drift correction still reads the right samples.
#[test]
fn looped_bed_fills_the_clip() {
    let d = dir("loop");
    let p = d.join("bed.wav");
    write(&p, sine(300.0, 0.2, 2.0));
    let lp = schema::make_effect(
        "loop",
        schema::LOOP,
        &schema::Loop {
            r#in: Some(0.0),
            out: Some(2.0),
            crossfade: 0.5,
        },
    )
    .expect("fx");
    let off = Level {
        mode: Some(LevelMode::Off),
        fade_in: Some(0.0),
        fade_out: Some(0.0),
        ..Level::default()
    };
    let lv = schema::make_effect("level", schema::LEVEL, &off).expect("fx");
    let clean = schema::make_effect(
        "clean",
        schema::CLEAN,
        &Clean {
            amount: 0.0,
            highpass: None,
            dialogue: Some(false),
            ..Clean::default()
        },
    )
    .expect("fx");
    let mut t = Track::new("Amb", track_kind::AUDIO);
    t.children.push(clip(&p, 0.0, 7.0, vec![lp, lv, clean]));
    let mut tl = Timeline::new("loop");
    tl.tracks.children.push(Item::Track(t));
    let mut opts = MixOptions::default();
    opts.master.lufs = None;
    opts.master.fade_out = 0.0;
    let mix = render_audio(&tl, &opts).expect("render");
    let a = &stem(&mix, Bus::Ambience).channels[0];
    assert_eq!(a.len(), secs(7.0));
    for s in 0..7 {
        let l = rms_db(&a[secs(f64::from(s))..secs(f64::from(s) + 1.0)]);
        // 0.2 peak, -3 dB for the sine, -3 dB per side going from mono to stereo.
        assert!((l + 20.0).abs() < 2.0, "second {s}: {l:.1} dB");
    }
}
