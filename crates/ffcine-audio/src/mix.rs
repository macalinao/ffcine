//! The per-timeline mix, after the prototype's `build_mix`: every audio clip loaded (clean
//! chain applied), drift-corrected, R128-levelled, faded and summed into its bus; music ducked
//! under dialogue and punchline SFX, ambience a little under dialogue; buses summed to the
//! master, brought to the target loudness under a 4x-oversampled true-peak limiter.
//!
//! All of it runs in chunks over scratch files ([`crate::spool`]), a clip or a bus at a time, so
//! memory doesn't grow with the timeline.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use ebur128::{EbuR128, Mode};
use ffcine_core::{Error, Result};
use ffcine_timeline::{BusParams, BusSend, HasMetadata, MixParams, Timeline, track_kind};
use serde::Serialize;

use crate::buffer::{Audio, db_to_lin, lin_to_db};
use crate::decode::{self, FilterChain, Request, Stream};
use crate::dsp::{Limiter, Reverb, Warper};
use crate::place::{Placed, layout};
use crate::schema::{self, Bus, Clean, Level, LevelMode, Loop, Pan, Param, SyncInfo};
use crate::spool::{CHUNK, Scratch, Spool};

/// Master loudness: -14 LUFS, true peak -1 dBTP (YouTube/Instagram).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Master {
    /// Integrated loudness target; `None` leaves the sum as is (still limited).
    pub lufs: Option<f64>,
    pub true_peak: f64,
    /// The limiter sits this far under `true_peak`: the AAC encode overshoots by 1.5-2 dB.
    pub headroom: f64,
    /// Fade-out at the end of the timeline (s).
    pub fade_out: f64,
}

impl Default for Master {
    fn default() -> Self {
        Self {
            lufs: Some(-14.0),
            true_peak: -1.0,
            headroom: 2.0,
            fade_out: 0.6,
        }
    }
}

/// What clips get when their own effects don't say (`project.mix`, [`MixParams`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClipDefaults {
    /// R128 targets (LUFS) of `dialogue` and `nat` clips.
    pub dialogue_lufs: f64,
    pub nat_lufs: f64,
    /// Music dip under `dialogue` and `nat` clips (dB).
    pub duck_db: f64,
    pub nat_duck_db: f64,
    /// Clean-chain strength of dialogue-bus clips without `ffcine.audio.clean/1`, by mode.
    pub clean: f64,
    pub clean_nat: f64,
    /// High-pass of that chain (Hz).
    pub highpass: Option<f64>,
}

impl Default for ClipDefaults {
    fn default() -> Self {
        Self {
            dialogue_lufs: -18.0,
            nat_lufs: -22.0,
            duck_db: -14.0,
            nat_duck_db: -7.0,
            clean: 0.6,
            clean_nat: 0.0,
            highpass: Some(80.0),
        }
    }
}

/// Everything about the mix but the clips. Serialises to its recipe (for cache keys), without
/// `base`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixOptions {
    pub rate: u32,
    pub master: Master,
    pub defaults: ClipDefaults,
    /// Relative media paths resolve against this (the timeline's directory).
    #[serde(skip)]
    pub base: Option<PathBuf>,
    /// Scratch files go under this; default: the system's temporary directory. Plan on about
    /// 1.4 GB per hour for each bus, the sum and the master.
    #[serde(skip)]
    pub scratch: Option<PathBuf>,
    /// With no audio tracks, use the first video track's clip audio as dialogue.
    pub video_audio: bool,
    /// Clamp of the per-clip R128 gain (dB).
    pub gain_range: (f64, f64),
    /// Crossfade at dialogue cuts (s): each dialogue clip reads half of it past either end.
    pub xfade: f64,
    /// Music duck envelope under dialogue: look-ahead and hold (s).
    pub duck_attack: f64,
    pub duck_release: f64,
    /// Duck only while the dialogue bus is actually loud (10 ms RMS over `duck_threshold`
    /// dBFS), not for the whole span of each dialogue clip.
    pub sidechain: bool,
    pub duck_threshold: f64,
    /// Mix length (s); default: the end of the longest track. A render passes the picture's
    /// length, so the master fade-out ends on its last frame.
    pub length: Option<f64>,
    /// Bus gains, filters and sends, and the aux buses (`project.mix.buses`).
    pub buses: BTreeMap<String, BusParams>,
}

impl Default for MixOptions {
    fn default() -> Self {
        Self {
            rate: 48000,
            master: Master::default(),
            defaults: ClipDefaults::default(),
            base: None,
            scratch: None,
            video_audio: true,
            gain_range: (-10.0, 14.0),
            xfade: 0.125,
            duck_attack: 0.25,
            duck_release: 0.45,
            sidechain: false,
            duck_threshold: -40.0,
            length: None,
            buses: BTreeMap::new(),
        }
    }
}

impl MixOptions {
    /// The defaults with the timeline's `project.mix` over them, and `base` its directory
    /// (`timeline_dir`).
    pub fn for_timeline(timeline: &Timeline, timeline_dir: Option<&Path>) -> Result<Self> {
        let mut o = Self {
            base: timeline_dir.map(Path::to_owned),
            ..Self::default()
        };
        if let Some(p) = timeline.project().transpose()? {
            o.apply(&p.mix);
        }
        Ok(o)
    }

    /// Sets what `m` has.
    pub fn apply(&mut self, m: &MixParams) {
        let set = |dst: &mut f64, v: Option<f64>| {
            if let Some(v) = v {
                *dst = v;
            }
        };
        if let Some(l) = m.lufs {
            self.master.lufs = Some(l);
        }
        if m.normalize == Some(false) {
            self.master.lufs = None;
        }
        set(&mut self.master.true_peak, m.true_peak);
        set(&mut self.master.headroom, m.headroom);
        set(&mut self.master.fade_out, m.fade_out);
        let d = &mut self.defaults;
        set(&mut d.dialogue_lufs, m.dialogue_lufs);
        set(&mut d.nat_lufs, m.nat_lufs);
        set(&mut d.duck_db, m.duck_db);
        set(&mut d.nat_duck_db, m.nat_duck_db);
        set(&mut d.clean, m.clean);
        set(&mut d.clean_nat, m.clean_nat);
        if m.highpass.is_some() {
            d.highpass = m.highpass.filter(|f| *f > 0.0);
        }
        if let Some([lo, hi]) = m.gain_range {
            self.gain_range = (lo, hi);
        }
        set(&mut self.xfade, m.xfade);
        set(&mut self.duck_attack, m.duck_attack);
        set(&mut self.duck_release, m.duck_release);
        set(&mut self.duck_threshold, m.duck_threshold);
        if let Some(s) = m.sidechain {
            self.sidechain = s;
        }
        if !m.buses.is_empty() {
            self.buses.clone_from(&m.buses);
        }
    }
}

/// The rendered mix.
#[derive(Debug, Clone)]
pub struct Mix {
    /// Stereo master at `MixOptions::rate`.
    pub master: Audio,
    /// Per-bus stems (faded and ducked, before the master gain and limiter), only buses with
    /// clips.
    pub buses: Vec<(Bus, Audio)>,
    /// Stems of the aux buses that are heard and have a feed.
    pub aux: Vec<(String, Audio)>,
    /// Measured on the master.
    pub lufs: f64,
    pub true_peak: f64,
}

/// The rendered mix in scratch files, gone when this drops.
#[derive(Debug)]
pub struct StreamedMix {
    master: Spool,
    buses: Vec<(Bus, Spool)>,
    aux: Vec<(String, Spool)>,
    /// Measured on the master.
    pub lufs: f64,
    pub true_peak: f64,
    // Last: the spools above live in it.
    _scratch: Scratch,
}

impl StreamedMix {
    pub fn seconds(&self) -> f64 {
        self.master.frames() as f64 / f64::from(self.master.rate().max(1))
    }

    /// Writes the master as a 32-bit float WAV.
    pub fn write_master(&mut self, path: &Path) -> Result<()> {
        self.master.write_wav(path)
    }

    /// Writes one WAV per bus and heard aux bus (`<dir>/<name>.wav`); needs `stems` on
    /// [`stream_audio`].
    pub fn write_stems(&mut self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
        for (bus, a) in &mut self.buses {
            a.write_wav(&dir.join(format!("{}.wav", bus.name())))?;
        }
        for (name, a) in &mut self.aux {
            a.write_wav(&dir.join(format!("{name}.wav")))?;
        }
        Ok(())
    }

    /// The whole mix in memory.
    pub fn into_mix(mut self) -> Result<Mix> {
        Ok(Mix {
            master: self.master.to_audio()?,
            buses: self
                .buses
                .iter_mut()
                .map(|(b, a)| Ok((*b, a.to_audio()?)))
                .collect::<Result<_>>()?,
            aux: self
                .aux
                .iter_mut()
                .map(|(n, a)| Ok((n.clone(), a.to_audio()?)))
                .collect::<Result<_>>()?,
            lufs: self.lufs,
            true_peak: self.true_peak,
        })
    }
}

/// Duck envelope rate (Hz).
const ENV_RATE: f64 = 100.0;

/// Renders the timeline's audio on its own (no video), so it can be remuxed onto an existing
/// render without re-encoding. [`stream_audio`] with the result read into memory.
pub fn render_audio(timeline: &Timeline, opts: &MixOptions) -> Result<Mix> {
    stream_audio(timeline, opts, true)?.into_mix()
}

/// A bus's fader and ducking, applied to chunks by their position.
struct Fader {
    gain: f32,
    /// Linear gain at [`ENV_RATE`]; `None` for a bus that isn't ducked.
    env: Option<Vec<f32>>,
}

impl Fader {
    fn duck(&self, chunk: &mut [Vec<f32>], at: usize, rate: u32) {
        let Some(lin) = &self.env else { return };
        let per = f64::from(rate) / ENV_RATE;
        let last = lin.len().saturating_sub(1);
        for c in chunk {
            for (i, s) in c.iter_mut().enumerate() {
                let x = (at + i) as f64 / per;
                let b = (x as usize).min(last);
                let f = (x - b as f64) as f32;
                *s *= lin[b] + (lin[(b + 1).min(last)] - lin[b]) * f;
            }
        }
    }

    /// Fades `a` in place, a chunk at a time, and adds it to `sum`.
    fn commit(&self, a: &mut Spool, sum: &mut Spool) -> Result<()> {
        for at in (0..a.frames()).step_by(CHUNK) {
            let mut c = a.read(at, CHUNK)?;
            for s in c.iter_mut().flatten() {
                *s *= self.gain;
            }
            self.duck(&mut c, at, a.rate());
            a.write(at, &c)?;
            sum.add(at, &c)?;
        }
        Ok(())
    }
}

/// Checks `project.mix.buses`: every name means something, and the aux buses they point at exist.
fn check_buses(buses: &BTreeMap<String, BusParams>) -> Result<()> {
    let bad = |what, value: String| Err(Error::Invalid { what, value });
    for (name, b) in buses {
        if Bus::from_name(name).is_some() {
            if b.reverb.is_some() || b.duck.is_some() {
                return bad("bus: reverb and duck are for aux buses", name.clone());
            }
            for s in &b.sends {
                if Bus::from_name(&s.to).is_some() || !buses.contains_key(&s.to) {
                    return bad("bus send: no such aux bus", s.to.clone());
                }
            }
            continue;
        }
        // Its stem is named after it.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return bad("aux bus name", name.clone());
        }
        if !b.sends.is_empty() {
            return bad("bus: an aux bus can't send", name.clone());
        }
        if let Some(d) = &b.duck {
            if b.reverb.is_some() || b.filter.is_some() {
                return bad("bus: a key bus isn't heard", name.clone());
            }
            if let Some(t) = d.targets.iter().find(|t| Bus::from_name(t).is_none()) {
                return bad("bus duck: no such target", t.clone());
            }
        }
    }
    Ok(())
}

/// What feeds aux bus `name`: (index into `buses`, send).
fn feeds<'a>(
    name: &str,
    buses: &[(Bus, Spool)],
    params: &'a BTreeMap<String, BusParams>,
) -> Vec<(usize, &'a BusSend)> {
    let mut out = Vec::new();
    for (i, (bus, _)) in buses.iter().enumerate() {
        if let Some(b) = params.get(bus.name()) {
            out.extend(b.sends.iter().filter(|s| s.to == name).map(|s| (i, s)));
        }
    }
    out
}

type RenderedBuses = (Vec<(Bus, Spool)>, Vec<f32>, Vec<f32>);

/// Renders every clip into its bus, each bus through its `filter`. Returns the buses and the
/// dialogue and SFX duck points (dB at [`ENV_RATE`]).
fn render_buses(
    clips: &[(Bus, Placed<'_>)],
    opts: &MixOptions,
    scratch: &Scratch,
    frames: usize,
    blocks: usize,
) -> Result<RenderedBuses> {
    let rate = opts.rate;
    // Music dips: under dialogue clips (spans), and briefly under punchline SFX.
    let mut dia_env = vec![0.0f32; blocks];
    let mut sfx_env = vec![0.0f32; blocks];
    let mut buses = Vec::new();
    let mut music_lufs: HashMap<PathBuf, Option<f64>> = HashMap::new();
    for bus in [Bus::Dialogue, Bus::Sfx, Bus::Ambience, Bus::Music] {
        let mine: Vec<&Placed<'_>> = clips.iter().filter(|c| c.0 == bus).map(|c| &c.1).collect();
        if mine.is_empty() {
            continue;
        }
        let mut out = Spool::new(scratch, rate, 2, frames)?;
        for p in mine {
            let level = schema::clip_effect::<Level>(p.clip, schema::LEVEL)?.unwrap_or_default();
            if !level.audible(bus) {
                continue;
            }
            let duck = level.duck_in(bus, &opts.defaults);
            let (t0, t1) = match bus {
                Bus::Sfx => (p.start - 0.05, p.start + p.duration.min(2.5)),
                _ => (p.start, p.end()),
            };
            if duck < 0.0 {
                let env = if bus == Bus::Sfx {
                    &mut sfx_env
                } else {
                    &mut dia_env
                };
                dip(env, t0, t1, duck as f32);
            }
            render_clip(p, bus, &level, opts, &mut music_lufs, scratch, &mut out)?;
        }
        if let Some(spec) = opts.buses.get(bus.name()).and_then(|b| b.filter.as_deref()) {
            let mut chain = ChainWriter::new(spec, rate)?;
            let mut filtered = Spool::new(scratch, rate, 2, frames)?;
            for at in (0..frames).step_by(CHUNK) {
                chain.push(&out.read(at, CHUNK)?, &mut filtered)?;
            }
            chain.finish(&mut filtered)?;
            out = filtered;
        }
        if bus == Bus::Dialogue && opts.sidechain {
            let mut on = Presence::new(rate, opts.duck_threshold);
            for at in (0..frames).step_by(CHUNK) {
                on.push(&out.read(at, CHUNK)?);
            }
            for (e, o) in dia_env.iter_mut().zip(on.finish(blocks)) {
                *e *= o;
            }
        }
        buses.push((bus, out));
    }
    Ok((buses, dia_env, sfx_env))
}

/// The bus fader of `name`, linear.
fn fader_gain(opts: &MixOptions, name: &str) -> f32 {
    let g = opts.buses.get(name).and_then(|b| b.gain_db).unwrap_or(0.0);
    db_to_lin(g) as f32
}

/// Adds what the key buses dip (dB at [`ENV_RATE`]) to `envs`. They read the buses undipped, so
/// one can't feed back into another.
fn key_dips(
    buses: &mut [(Bus, Spool)],
    opts: &MixOptions,
    frames: usize,
    blocks: usize,
    envs: &mut HashMap<Bus, Vec<f32>>,
) -> Result<()> {
    for (name, aux) in &opts.buses {
        let Some(d) = &aux.duck else { continue };
        let sends = feeds(name, buses, &opts.buses);
        let mut key = Presence::new(opts.rate, d.threshold_db);
        for at in (0..frames).step_by(CHUNK) {
            let mut sum = vec![vec![0.0; CHUNK.min(frames - at)]; 2];
            for (i, send) in &sends {
                let g = send_gain(send, fader_gain(opts, buses[*i].0.name()));
                add_scaled(&mut sum, &buses[*i].1.read(at, CHUNK)?, g);
            }
            key.push(&sum);
        }
        let dips: Vec<f32> = key
            .finish(blocks)
            .iter()
            .map(|o| o * d.depth_db as f32)
            .collect();
        let dips = smooth_db_envelope(&dips, d.attack, d.release);
        for t in d.targets.iter().filter_map(|t| Bus::from_name(t)) {
            let env = envs.entry(t).or_insert_with(|| vec![0.0; blocks]);
            for (e, k) in env.iter_mut().zip(&dips) {
                *e += k;
            }
        }
    }
    Ok(())
}

/// The aux buses that are heard and have a feed, each through its reverb and `filter`. Reads the
/// buses without their faders, so a `pre` send has them.
fn render_aux(
    buses: &mut [(Bus, Spool)],
    faders: &[Fader],
    opts: &MixOptions,
    scratch: &Scratch,
    frames: usize,
) -> Result<Vec<(String, Spool)>> {
    let rate = opts.rate;
    let mut aux = Vec::new();
    for (name, params) in &opts.buses {
        if Bus::from_name(name).is_some() || params.duck.is_some() {
            continue;
        }
        let sends = feeds(name, buses, &opts.buses);
        if sends.is_empty() {
            continue;
        }
        let mut out = Spool::new(scratch, rate, 2, frames)?;
        let mut verb = params
            .reverb
            .as_ref()
            .map(|r| Reverb::new(rate, 2, r.room, r.damp));
        let mut chain = match &params.filter {
            Some(spec) => Some(ChainWriter::new(spec, rate)?),
            None => None,
        };
        for at in (0..frames).step_by(CHUNK) {
            let mut sum = vec![vec![0.0; CHUNK.min(frames - at)]; 2];
            for (i, send) in &sends {
                let mut c = buses[*i].1.read(at, CHUNK)?;
                if !send.pre {
                    faders[*i].duck(&mut c, at, rate);
                }
                add_scaled(&mut sum, &c, send_gain(send, faders[*i].gain));
            }
            if let Some(v) = &mut verb {
                v.process(&mut sum);
            }
            match &mut chain {
                Some(f) => f.push(&sum, &mut out)?,
                None => out.write(at, &sum)?,
            }
        }
        if let Some(f) = &mut chain {
            f.finish(&mut out)?;
        }
        aux.push((name.clone(), out));
    }
    Ok(aux)
}

/// Like [`render_audio`], with memory bounded however long the timeline: a few chunks, the 100 Hz
/// duck envelopes (about 1.4 MB an hour each) and a little bookkeeping per clip. `stems` keeps the
/// bus stems' scratch files for [`StreamedMix::write_stems`].
pub fn stream_audio(timeline: &Timeline, opts: &MixOptions, stems: bool) -> Result<StreamedMix> {
    check_buses(&opts.buses)?;
    let scratch = Scratch::new(opts.scratch.as_deref())?;
    let rate = opts.rate;
    let (clips, end) = sources(timeline, opts.video_audio);
    let length = opts.length.unwrap_or(end).max(0.0);
    let frames = (length * f64::from(rate)).round() as usize;
    let blocks = (length * ENV_RATE).ceil() as usize + 1;

    let (mut buses, dia_env, sfx_env) = render_buses(&clips, opts, &scratch, frames, blocks)?;
    let dia_env = smooth_db_envelope(&dia_env, opts.duck_attack, opts.duck_release);
    let sfx_env = smooth_db_envelope(&sfx_env, 0.04, 0.35);
    let mut envs: HashMap<Bus, Vec<f32>> = HashMap::new();
    envs.insert(
        Bus::Music,
        dia_env.iter().zip(&sfx_env).map(|(d, s)| d + s).collect(),
    );
    envs.insert(Bus::Ambience, dia_env.iter().map(|d| d.max(-6.0)).collect());
    key_dips(&mut buses, opts, frames, blocks, &mut envs)?;
    let faders: Vec<Fader> = buses
        .iter()
        .map(|(bus, _)| Fader {
            gain: fader_gain(opts, bus.name()),
            env: envs
                .get(bus)
                .map(|e| e.iter().map(|d| db_to_lin(f64::from(*d)) as f32).collect()),
        })
        .collect();
    let mut aux = render_aux(&mut buses, &faders, opts, &scratch, frames)?;

    let mut sum = Spool::new(&scratch, rate, 2, frames)?;
    for ((_, b), fader) in buses.iter_mut().zip(&faders) {
        fader.commit(b, &mut sum)?;
    }
    for (name, a) in &mut aux {
        let fader = Fader {
            gain: fader_gain(opts, name),
            env: None,
        };
        fader.commit(a, &mut sum)?;
    }
    if !stems {
        buses.clear();
        aux.clear();
    }
    let k = ((opts.master.fade_out * f64::from(rate)) as usize).min(frames);
    for at in (frames - k..frames).step_by(CHUNK) {
        let mut c = sum.read(at, CHUNK)?;
        for ch in &mut c {
            for (i, s) in ch.iter_mut().enumerate() {
                *s *= 1.0 - (at + i - (frames - k)) as f32 / k as f32;
            }
        }
        sum.write(at, &c)?;
    }
    let (master, lufs, true_peak) = master_bus(&mut sum, &opts.master, &scratch)?;
    Ok(StreamedMix {
        master,
        buses,
        aux,
        lufs,
        true_peak,
        _scratch: scratch,
    })
}

/// The linear level of `send`, given its bus's fader.
fn send_gain(send: &BusSend, fader: f32) -> f32 {
    let g = db_to_lin(send.gain_db) as f32;
    if send.pre { g } else { g * fader }
}

fn add_scaled(sum: &mut [Vec<f32>], chunk: &[Vec<f32>], g: f32) {
    for (d, s) in sum.iter_mut().zip(chunk) {
        for (d, s) in d.iter_mut().zip(s) {
            *d += *s * g;
        }
    }
}

/// A libavfilter chain writing into a spool from its start, with the chain's latency taken out.
struct ChainWriter {
    chain: FilterChain,
    skip: usize,
    at: usize,
}

impl ChainWriter {
    fn new(spec: &str, rate: u32) -> Result<Self> {
        Ok(Self {
            chain: FilterChain::new(spec, rate, 2)?,
            skip: decode::latency(spec, rate, 2)?,
            at: 0,
        })
    }

    fn put(&mut self, mut c: Vec<Vec<f32>>, out: &mut Spool) -> Result<()> {
        let n = self.skip.min(c.first().map_or(0, Vec::len));
        for ch in &mut c {
            ch.drain(..n);
        }
        self.skip -= n;
        out.write(self.at, &c)?;
        self.at += c.first().map_or(0, Vec::len);
        Ok(())
    }

    fn push(&mut self, chunk: &[Vec<f32>], out: &mut Spool) -> Result<()> {
        let c = self.chain.push(chunk)?;
        self.put(c, out)
    }

    fn finish(&mut self, out: &mut Spool) -> Result<()> {
        let c = self.chain.finish()?;
        self.put(c, out)
    }
}

/// The clips the mix reads, each with its bus, and the end of the longest track (s): every
/// enabled audio track's clips, or with none and `video_audio`, the first video track's (camera
/// audio, as dialogue).
pub fn sources(timeline: &Timeline, video_audio: bool) -> (Vec<(Bus, Placed<'_>)>, f64) {
    let mut length = 0.0f64;
    let mut clips: Vec<(Bus, Placed<'_>)> = Vec::new();
    let tracks: Vec<_> = timeline.track_list().collect();
    for t in &tracks {
        let (placed, end) = layout(t);
        length = length.max(end);
        if t.kind == track_kind::AUDIO && t.enabled {
            let bus = Bus::of_track(t);
            clips.extend(placed.into_iter().map(|p| (bus, p)));
        }
    }
    if clips.is_empty()
        && video_audio
        && let Some(v) = tracks.iter().find(|t| t.kind == track_kind::VIDEO)
    {
        clips.extend(layout(v).0.into_iter().map(|p| (Bus::Dialogue, p)));
    }
    (clips, length)
}

/// Lowers `env` (dB at [`ENV_RATE`]) to at most `db` over `t0..t1` (s).
fn dip(env: &mut [f32], t0: f64, t1: f64, db: f32) {
    let a = ((t0 * ENV_RATE).max(0.0) as usize).min(env.len());
    let b = ((t1 * ENV_RATE).max(0.0) as usize).min(env.len());
    for e in &mut env[a..b] {
        *e = e.min(db);
    }
}

/// The prototype's `smooth_db_envelope`: a dip starts `attack` early (look-ahead), holds for
/// `release / 2` after it ends, then a Hann smoothing over `(attack + release) / 2`.
pub fn smooth_db_envelope(points: &[f32], attack: f64, release: f64) -> Vec<f32> {
    let n = points.len();
    let la = ((attack * ENV_RATE) as usize).max(1);
    let out: Vec<f32> = (0..n)
        .map(|i| {
            points[i..(i + la).min(n)]
                .iter()
                .copied()
                .fold(points[i], f32::min)
        })
        .collect();
    let rl = ((release * ENV_RATE * 0.5) as usize).max(1);
    let rel: Vec<f32> = (0..n)
        .map(|i| {
            out[i.saturating_sub(rl)..=i]
                .iter()
                .copied()
                .fold(out[i], f32::min)
        })
        .collect();
    let len = ((f64::midpoint(attack, release) * ENV_RATE) as usize).max(3) | 1;
    let w: Vec<f32> = (0..len)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (len - 1) as f32).cos())
        .collect();
    let sum: f32 = w.iter().sum();
    let pad = len / 2;
    (0..n)
        .map(|i| {
            w.iter()
                .enumerate()
                .map(|(k, wk)| rel[(i + k).saturating_sub(pad).min(n - 1)] * wk)
                .sum::<f32>()
                / sum
        })
        .collect()
}

#[allow(clippy::too_many_lines)] // one clip, start to finish
/// Loads, cleans, drift-corrects, levels and fades one clip into its bus `out`.
fn render_clip(
    placed: &Placed<'_>,
    bus: Bus,
    level: &Level,
    opts: &MixOptions,
    music_lufs: &mut HashMap<PathBuf, Option<f64>>,
    scratch: &Scratch,
    out: &mut Spool,
) -> Result<()> {
    let rate = opts.rate;
    let fr = f64::from(rate);
    let mode = level.mode(bus);
    // Dialogue reads half the crossfade past each end (the prototype's handles).
    let mut p = placed.clone();
    let hh = if bus == Bus::Dialogue {
        opts.xfade / 2.0
    } else {
        0.0
    };
    p.start -= hh;
    p.duration += 2.0 * hh;
    p.source_start -= hh * p.speed;
    let head = (-p.start).max(0.0);
    let at = (p.start.max(0.0) * fr).round() as usize;
    let n = ((p.duration - head) * fr).round().max(0.0) as usize;
    let Some(path) = p.path(opts.base.as_deref()) else {
        return Ok(());
    };
    // Freeze frames, and stills or silent clips standing in for camera audio, are silent.
    if p.speed == 0.0 || !decode::has_audio(&path) {
        return Ok(());
    }
    let clean = schema::clip_effect::<Clean>(p.clip, schema::CLEAN)?;
    let mut filter = match (clean, bus) {
        (Some(c), _) => c.filter_spec(mode),
        (None, Bus::Dialogue) => Clean::for_mode_in(mode, &opts.defaults).filter_spec(mode),
        (None, Bus::Ambience) => "highpass=f=60,lowpass=f=14000".into(),
        (None, _) => String::new(),
    };
    let drift = p
        .clip
        .ffcine_get::<SyncInfo>("sync")
        .and_then(Result::ok)
        .map_or(0.0, |s| s.drift_ppm);
    if (p.speed - 1.0).abs() > 1e-9 {
        filter = join(&filter, &atempo(p.speed));
    }
    let k = 1.0 + drift * 1e-6;
    let src_start = p.source_start + head * p.speed * k;
    let read = |start: f64, dur: f64| {
        Stream::open(
            &path,
            &Request {
                start: start.max(0.0),
                duration: Some(dur),
                rate,
                channels: 2,
                filter: (!filter.is_empty()).then_some(filter.as_str()),
            },
        )
    };
    let lp = schema::clip_effect::<Loop>(p.clip, schema::LOOP)?;
    let mut a = Spool::new(scratch, rate, 2, n)?;
    if let Some(lp) = &lp {
        let lo = lp.r#in.unwrap_or(src_start);
        let hi = lp.out.unwrap_or(lo + p.duration + 1.0);
        let len = ((hi - lo) * fr).round().max(0.0) as usize;
        let mut seg = Spool::new(scratch, rate, 2, len)?;
        let mut stream = read(lo, hi - lo)?;
        let mut pos = 0;
        while let Some(c) = stream.read()? {
            seg.write(pos, &c)?;
            pos += c[0].len();
        }
        looped(&mut seg, &mut a, (lp.crossfade * fr) as usize)?;
    } else {
        // atempo turns media time into record time; the drift is resampled here.
        let mut stream = read(src_start, (n as f64 / fr) * k + 0.01)?;
        let mut warper = ((k - 1.0).abs() > 1e-12).then(|| Warper::new(2, k));
        let mut pos = 0;
        while let Some(c) = stream.read()? {
            let c = match &mut warper {
                Some(w) => w.push(&c),
                None => c,
            };
            a.write(pos, &c)?;
            pos += c[0].len();
        }
        if let Some(w) = &mut warper {
            a.write(pos, &w.finish(n))?;
        }
    }

    let mut gain_db = 0.0;
    if let Some(target) = level.target_in(bus, &opts.defaults) {
        // Music levels on the whole track, as the prototype did, so quiet intros stay quiet.
        let l = if bus == Bus::Music && lp.is_none() {
            if !music_lufs.contains_key(&path) {
                let whole = file_loudness(&path, rate)?;
                music_lufs.insert(path.clone(), whole);
            }
            music_lufs.get(&path).copied().flatten()
        } else {
            spool_loudness(&mut a)?
        };
        if let Some(l) = l.filter(|l| *l > -60.0) {
            let g = target - l;
            gain_db = if bus == Bus::Dialogue {
                g.clamp(opts.gain_range.0, opts.gain_range.1)
            } else {
                g
            };
        }
    }
    if mode == LevelMode::Sfx {
        gain_db += sfx_gain_db(&mut a, db_to_lin(gain_db), level.ref_dbfs.unwrap_or(-18.0))?;
    }
    let pan = schema::clip_effect::<Pan>(p.clip, schema::PAN)?.map(|p| p.pan);

    let default_fade = match bus {
        Bus::Dialogue => opts.xfade,
        Bus::Ambience => 0.8,
        _ => 0.01,
    };
    let fi = if p.xfade_in > 0.0 {
        p.xfade_in
    } else {
        level.fade_in.unwrap_or(default_fade)
    };
    let fo = if p.xfade_out > 0.0 {
        p.xfade_out
    } else {
        level.fade_out.unwrap_or(default_fade)
    };
    let fades = Fades::new(fr, n, fi - head, fo, p.xfade_in > 0.0, p.xfade_out > 0.0);
    let g = db_to_lin(gain_db) as f32;
    for pos in (0..n).step_by(CHUNK) {
        let mut c = a.read(pos, CHUNK)?;
        for s in c.iter_mut().flatten() {
            *s *= g;
        }
        automate(
            &mut c,
            pos,
            fr,
            &level.gain_db,
            pan.as_ref(),
            src_start,
            p.speed,
        );
        fades.apply(&mut c, pos);
        out.add(at + pos, &c)?;
    }
    Ok(())
}

/// The prototype's `sfx_wav`: the gain (dB) that brings the loudest 100 ms (mono RMS) of `a`
/// times `pre` to `ref_dbfs`, with peaks under -1 dBFS.
fn sfx_gain_db(a: &mut Spool, pre: f64, ref_dbfs: f64) -> Result<f64> {
    let frames = a.frames();
    let n = (a.rate() as usize / 10).max(1);
    let (mut acc, mut best, mut pk) = (0.0f64, 0.0f64, 0.0f32);
    let mut window = std::collections::VecDeque::with_capacity(n + 1);
    let pre32 = pre as f32;
    for at in (0..frames).step_by(CHUNK) {
        let c = a.read(at, CHUNK)?;
        for (l, r) in c[0].iter().zip(&c[1]) {
            let (l, r) = (l * pre32, r * pre32);
            pk = pk.max(l.abs()).max(r.abs());
            let m = f64::from(f32::midpoint(l, r)).powi(2);
            acc += m;
            window.push_back(m);
            if window.len() > n {
                acc -= window.pop_front().unwrap_or(0.0);
            }
            if window.len() == n {
                best = best.max(acc);
            }
        }
    }
    let rms = if frames > n {
        (best.max(0.0) / n as f64).sqrt()
    } else {
        (acc / frames.max(1) as f64).sqrt()
    };
    let mut g = db_to_lin(ref_dbfs) / rms.max(1e-6);
    let lim = db_to_lin(-1.0);
    if f64::from(pk) * g > lim {
        g = lim / f64::from(pk);
    }
    Ok(lin_to_db(g))
}

/// Applies `gain_db` and the stereo balance `pan` to a chunk starting at clip frame `at`; either
/// may be keyframed over source time (`t0` at the first sample, advancing `speed` per record
/// second).
fn automate(
    a: &mut [Vec<f32>],
    at: usize,
    rate: f64,
    gain_db: &Param,
    pan: Option<&Param>,
    t0: f64,
    speed: f64,
) {
    let balance = |p: f64| {
        let p = p.clamp(-1.0, 1.0);
        [(1.0 - p).min(1.0), (1.0 + p).min(1.0)]
    };
    let still = gain_db.is_constant() && pan.is_none_or(Param::is_constant);
    let gains = |t: f64| {
        let g = db_to_lin(gain_db.at(t));
        let b = pan.map_or([1.0, 1.0], |p| balance(p.at(t)));
        [(g * b[0]) as f32, (g * b[1]) as f32]
    };
    if still {
        let g = gains(t0);
        for (c, ch) in a.iter_mut().enumerate() {
            for s in ch {
                *s *= g[c.min(1)];
            }
        }
        return;
    }
    // Keyframes: evaluate every 64 samples (1.3 ms), linear in between.
    let n = a.first().map_or(0, Vec::len);
    let step = 64;
    for start in (0..n).step_by(step) {
        let prev = gains(t0 + (at + start) as f64 / rate * speed);
        let next = gains(t0 + (at + start + step) as f64 / rate * speed);
        let len = step.min(n - start);
        for (c, ch) in a.iter_mut().enumerate() {
            let (g0, g1) = (prev[c.min(1)], next[c.min(1)]);
            for (i, s) in ch[start..start + len].iter_mut().enumerate() {
                *s *= g0 + (g1 - g0) * i as f32 / step as f32;
            }
        }
    }
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{a},{b}"),
    }
}

/// `atempo` chain for any speed (each stage takes 0.5..=2 for best quality).
fn atempo(speed: f64) -> String {
    let mut s = speed;
    let mut f = Vec::new();
    while s > 2.0 {
        f.push("atempo=2".to_owned());
        s /= 2.0;
    }
    while s < 0.5 {
        f.push("atempo=0.5".to_owned());
        s /= 0.5;
    }
    f.push(format!("atempo={s:.6}"));
    f.join(",")
}

/// `src` repeated to fill `out` with `x`-frame equal-power crossfades at the seams.
fn looped(src: &mut Spool, out: &mut Spool, x: usize) -> Result<()> {
    let (len, n) = (src.frames(), out.frames());
    let x = x.min(len / 2);
    if len >= n || len == 0 || x == 0 {
        for at in (0..len.min(n)).step_by(CHUNK) {
            out.write(at, &src.read(at, CHUNK)?)?;
        }
        return Ok(());
    }
    let quarter = std::f32::consts::FRAC_PI_2;
    let mut copy = 0;
    loop {
        let more = copy + len < n;
        for at in (0..len).step_by(CHUNK) {
            let mut c = src.read(at, CHUNK)?;
            for ch in &mut c {
                for (i, s) in ch.iter_mut().enumerate() {
                    let i = at + i;
                    if copy > 0 && i < x {
                        *s *= (quarter * i as f32 / x as f32).sin();
                    }
                    if more && i >= len - x {
                        *s *= (quarter * (i - (len - x)) as f32 / x as f32).cos();
                    }
                }
            }
            out.add(copy + at, &c)?;
        }
        if !more {
            return Ok(());
        }
        copy += len - x;
    }
}

/// The fades at a clip's ends, applied to chunks by their position: equal-power for crossfades,
/// linear otherwise.
struct Fades {
    n: usize,
    fi: usize,
    fo: usize,
    eq_in: bool,
    eq_out: bool,
}

impl Fades {
    fn new(rate: f64, n: usize, fade_in: f64, fade_out: f64, eq_in: bool, eq_out: bool) -> Self {
        Self {
            n,
            fi: ((fade_in.max(0.0) * rate) as usize).min(n),
            fo: ((fade_out.max(0.0) * rate) as usize).min(n),
            eq_in,
            eq_out,
        }
    }

    fn apply(&self, chunk: &mut [Vec<f32>], at: usize) {
        let curve = |x: f32, eq: bool| {
            if eq {
                (x * std::f32::consts::FRAC_PI_2).sin()
            } else {
                x
            }
        };
        for c in chunk {
            for (i, s) in c.iter_mut().enumerate() {
                let i = at + i;
                if i < self.fi {
                    *s *= curve(i as f32 / self.fi as f32, self.eq_in);
                }
                let back = self.n - 1 - i;
                if back < self.fo {
                    *s *= curve(back as f32 / self.fo as f32, self.eq_out);
                }
            }
        }
    }
}

fn r128_err(e: ebur128::Error) -> Error {
    Error::Pipeline(format!("ebur128: {e}"))
}

fn meter(a: &Audio) -> Result<EbuR128> {
    let mut m = EbuR128::new(
        u32::try_from(a.channels.len()).unwrap_or(2),
        a.rate,
        Mode::I | Mode::TRUE_PEAK,
    )
    .map_err(r128_err)?;
    let planes: Vec<&[f32]> = a.channels.iter().map(Vec::as_slice).collect();
    m.add_frames_planar_f32(&planes).map_err(r128_err)?;
    Ok(m)
}

/// Integrated loudness (LUFS); `None` for silence or under 400 ms.
pub fn loudness(a: &Audio) -> Result<Option<f64>> {
    if a.frames() < (a.rate as usize * 2) / 5 {
        return Ok(None);
    }
    let l = meter(a)?.loudness_global().map_err(r128_err)?;
    Ok(l.is_finite().then_some(l))
}

/// Integrated loudness and true peak (dBTP).
pub fn measure(a: &Audio) -> Result<(f64, f64)> {
    let m = meter(a)?;
    let l = m.loudness_global().map_err(r128_err)?;
    let tp = (0..a.channels.len())
        .map(|c| m.true_peak(u32::try_from(c).unwrap_or(0)).unwrap_or(0.0))
        .fold(0.0f64, f64::max);
    Ok((l, lin_to_db(tp)))
}

fn new_meter(channels: usize, rate: u32) -> Result<EbuR128> {
    EbuR128::new(
        u32::try_from(channels).unwrap_or(2),
        rate,
        Mode::I | Mode::TRUE_PEAK,
    )
    .map_err(r128_err)
}

fn meter_add(m: &mut EbuR128, chunk: &[Vec<f32>]) -> Result<()> {
    let planes: Vec<&[f32]> = chunk.iter().map(Vec::as_slice).collect();
    m.add_frames_planar_f32(&planes).map_err(r128_err)
}

/// [`loudness`] of a spool.
fn spool_loudness(a: &mut Spool) -> Result<Option<f64>> {
    if a.frames() < (a.rate() as usize * 2) / 5 {
        return Ok(None);
    }
    let mut m = new_meter(2, a.rate())?;
    for at in (0..a.frames()).step_by(CHUNK) {
        meter_add(&mut m, &a.read(at, CHUNK)?)?;
    }
    let l = m.loudness_global().map_err(r128_err)?;
    Ok(l.is_finite().then_some(l))
}

/// [`loudness`] of a whole file, decoded as it is measured.
fn file_loudness(path: &Path, rate: u32) -> Result<Option<f64>> {
    let mut stream = Stream::open(
        path,
        &Request {
            rate,
            channels: 2,
            ..Request::default()
        },
    )?;
    let mut m = new_meter(2, rate)?;
    let mut frames = 0;
    while let Some(c) = stream.read()? {
        frames += c[0].len();
        meter_add(&mut m, &c)?;
    }
    if frames < (rate as usize * 2) / 5 {
        return Ok(None);
    }
    let l = m.loudness_global().map_err(r128_err)?;
    Ok(l.is_finite().then_some(l))
}

/// Gain to the target and limit; up to 4 passes until within 0.25 LU and under the ceiling
/// (as `ebur128` measures it; the limiter's own estimate can read a little low on noise).
fn master_bus(sum: &mut Spool, m: &Master, scratch: &Scratch) -> Result<(Spool, f64, f64)> {
    let ceiling = m.true_peak - m.headroom.max(0.0);
    let mut lim = ceiling;
    let mut gain = match (m.lufs, spool_loudness(sum)?) {
        (Some(t), Some(l)) => t - l,
        _ => 0.0,
    };
    let (rate, frames) = (sum.rate(), sum.frames());
    let mut out = Spool::new(scratch, rate, 2, frames)?;
    let (mut l, mut tp) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    if frames == 0 {
        return Ok((out, l, tp));
    }
    for _ in 0..4 {
        let g = db_to_lin(gain) as f32;
        let mut limiter = Limiter::new(rate, lim, 0.002, 0.06);
        let (before, after) = limiter.context();
        let mut meter = new_meter(2, rate)?;
        for at in (0..frames).step_by(CHUNK) {
            let lo = at.saturating_sub(before);
            let len = CHUNK.min(frames - at);
            let mut w = sum.read(lo, at - lo + len + after)?;
            for s in w.iter_mut().flatten() {
                *s *= g;
            }
            limiter.process(&mut w, at - lo, len);
            for c in &mut w {
                c.truncate(at - lo + len);
                c.drain(..at - lo);
            }
            out.write(at, &w)?;
            meter_add(&mut meter, &w)?;
        }
        l = meter.loudness_global().map_err(r128_err)?;
        tp = (0..2)
            .map(|c| meter.true_peak(c).unwrap_or(0.0))
            .fold(0.0f64, f64::max);
        tp = lin_to_db(tp);
        let loud_ok = m
            .lufs
            .is_none_or(|t| !l.is_finite() || (t - l).abs() <= 0.25);
        if loud_ok && tp <= ceiling + 0.05 {
            break;
        }
        if tp > ceiling {
            lim -= tp - ceiling + 0.05;
        }
        if let Some(t) = m.lufs.filter(|_| l.is_finite()) {
            gain += t - l;
        }
    }
    Ok((out, l, tp))
}

/// 1 where a bus is louder than the threshold (10 ms RMS), else 0, at [`ENV_RATE`].
struct Presence {
    block: usize,
    thr: f64,
    acc: f64,
    have: usize,
    out: Vec<f32>,
}

impl Presence {
    fn new(rate: u32, threshold: f64) -> Self {
        Self {
            block: ((f64::from(rate) / ENV_RATE) as usize).max(1),
            thr: db_to_lin(threshold),
            acc: 0.0,
            have: 0,
            out: Vec::new(),
        }
    }

    fn close(&mut self) {
        let ms = self.acc / (self.have * 2) as f64;
        self.out.push(if ms.sqrt() > self.thr { 1.0 } else { 0.0 });
        self.acc = 0.0;
        self.have = 0;
    }

    fn push(&mut self, chunk: &[Vec<f32>]) {
        for i in 0..chunk.first().map_or(0, Vec::len) {
            self.acc += chunk.iter().map(|c| f64::from(c[i]).powi(2)).sum::<f64>();
            self.have += 1;
            if self.have == self.block {
                self.close();
            }
        }
    }

    /// `blocks` values, 0 past the end of what was pushed.
    fn finish(mut self, blocks: usize) -> Vec<f32> {
        if self.have > 0 {
            self.close();
        }
        self.out.resize(blocks, 0.0);
        self.out
    }
}

/// Writes the master and, with `stems`, one WAV per bus (`<stem dir>/<bus>.wav`).
pub fn write_mix(mix: &Mix, out: &Path, stems: Option<&Path>) -> Result<()> {
    mix.master.write_wav(out)?;
    if let Some(dir) = stems {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
        for (bus, a) in &mix.buses {
            a.write_wav(&dir.join(format!("{}.wav", bus.name())))?;
        }
        for (name, a) in &mix.aux {
            a.write_wav(&dir.join(format!("{name}.wav")))?;
        }
    }
    Ok(())
}
