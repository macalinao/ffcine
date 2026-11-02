//! The mix's memory doesn't grow with the timeline: heap high-water mark of a long mix, counted by
//! the allocator. In its own test binary, so nothing else allocates at the same time.
#![cfg(feature = "ffmpeg")]
#![allow(clippy::cast_possible_truncation, clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ffcine_audio::{Bus, Level, MixOptions, schema, stream_audio};
use ffcine_timeline::{
    Clip, ExternalReference, HasMetadata, Item, MediaReference, RationalTime, TimeRange, Timeline,
    Track, track_kind,
};

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: defers to `System`; the counters don't touch the allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let n = NOW.fetch_add(l.size(), Ordering::Relaxed) + l.size();
        PEAK.fetch_max(n, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        NOW.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const RATE: u32 = 8000;
/// 10 min: 38 MB per stereo buffer at this rate, and a handful would be alive at once in memory.
const SECONDS: u32 = 600;

fn tone_wav(path: &std::path::Path) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..RATE * SECONDS {
        let t = f64::from(i) / f64::from(RATE);
        w.write_sample((t * 330.0 * std::f64::consts::TAU).sin() as f32 * 0.2)
            .unwrap();
    }
    w.finalize().unwrap();
}

#[test]
fn long_mix_stays_small() {
    let d = std::env::temp_dir().join(format!("ffcine-audio-memory-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join("long.wav");
    tone_wav(&p);

    // One long clip each on the music and ambience buses, a reverb send on top.
    let clip = |name: &str, lufs: f64| {
        let mut c = Clip::new(
            name,
            MediaReference::External(ExternalReference::from_path(&p, None)),
            Some(TimeRange::new(
                RationalTime::from_seconds(0.0, 48000.0),
                RationalTime::from_seconds(f64::from(SECONDS), 48000.0),
            )),
        );
        let level = Level {
            lufs: Some(lufs),
            ..Level::default()
        };
        c.effects
            .push(schema::make_effect("level", schema::LEVEL, &level).unwrap());
        Item::Clip(c)
    };
    let mut tl = Timeline::new("long");
    for (name, bus, lufs) in [("m", Bus::Music, -20.0), ("a", Bus::Ambience, -30.0)] {
        let mut t = Track::new(name, track_kind::AUDIO);
        t.ffcine_mut().insert("bus".into(), bus.name().into());
        t.children.push(clip(name, lufs));
        tl.tracks.children.push(Item::Track(t));
    }
    let mut opts = MixOptions {
        rate: RATE,
        scratch: Some(d.clone()),
        buses: serde_json::from_value(serde_json::json!({
            "music": {"sends": [{"to": "room", "gain_db": -12.0}]},
            "room": {"reverb": {}},
        }))
        .unwrap(),
        ..MixOptions::default()
    };
    opts.master.lufs = None;

    PEAK.store(NOW.load(Ordering::Relaxed), Ordering::Relaxed);
    let before = NOW.load(Ordering::Relaxed);
    let mix = stream_audio(&tl, &opts, true).unwrap();
    let peak = PEAK.load(Ordering::Relaxed) - before;
    assert!((mix.seconds() - f64::from(SECONDS)).abs() < 1e-6);
    drop(mix);
    std::fs::remove_dir_all(&d).unwrap();
    // A few chunks of 64k stereo frames, and the duck envelopes (60k points).
    assert!(peak < 16 << 20, "peak heap {} MB", peak >> 20);
}
