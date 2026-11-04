# Audio

`ffcine-audio` (roadmap step 6): recorder sync, the per-clip clean chain, R128 levelling, buses
with ducking and the master. CPU only (libav* for decode and the clean chain, DSP in Rust), so it
runs anywhere ffmpeg does, macOS included. One mix per timeline, not per shot.

```sh
# Recorder WAVs -> audio clips on a new track (a new timeline with the clips in name order,
# or --timeline to add to an existing one).
ffcine sync --audio H2/ --video CARD/ -o synced.otio
ffcine sync --audio H2/ --video CARD/ --timeline edit.otio --lead 0.3 --tail 0.5 --wind -o edit.otio

# The mix on its own: 32-bit float stereo WAV at -14 LUFS / -1 dBTP, plus one stem per bus.
ffcine audio edit.otio -o mix.wav --stems stems/

# Or as part of the render: the master gets this mix ([assembly.md](assembly.md)).
ffcine render edit.otio -o edit.mp4
```

Library: `ffcine_audio::render_audio(&timeline, &MixOptions) -> Mix` (`master`, per-bus stems,
measured `lufs` and `true_peak`), `write_mix`. Defaults follow the prototype's `build_mix`.
`MixOptions::for_timeline` puts the timeline's `project.mix` over them.

## Settings live in the timeline

Everything the mix reads is in the OTIO file: clip effects (`ffcine.audio.clean/1`,
`ffcine.audio.level/1` with keyframable `gain_db`, `ffcine.audio.pan/1`, `ffcine.audio.loop/1`),
`metadata.ffcine.sync` from `ffcine sync`, track buses, and the timeline-wide settings in
`project.mix` (master loudness, clip targets, ducking, default clean chain;
[timeline.md](timeline.md#mix)). `ffcine import edit.yaml` fills them from the prototype's
`audio`, `music` and `output.audio` sections. The `ffcine audio` and `ffcine render` flags
(`--lufs`, `--true-peak`, `--headroom`, `--no-normalize`, `--sidechain`) only override them for
one run.

## Sync (Zoom H2essential)

The H2essential writes 32-bit float WAVs with no timecode, often one long file spanning many
camera clips. `ffcine sync` finds each camera clip in the recordings by its scratch audio:

1. Both sides decode to 8 kHz mono (first audio stream; iPhones' extra APAC track is skipped)
   and are high-passed at 150 Hz (wind, handling).
2. **Coarse**: the clip's log-RMS envelope (5 ms hops) is cross-correlated (FFT) against each
   recording's, normalised over the overlap, so clips that start before the recorder or run past
   its end still match. Steady sound (wind, water) has no envelope, so when that doesn't give a
   confident match the loudest 8 s of the clip is searched on the waveform, in FFT blocks.
3. **Fine**: up to 24 windows of 4 s across the clip are correlated on the waveform within
   ±100 ms of the coarse lag, with parabolic sub-sample peaks.
4. A weighted line through the window offsets (outliers over 1 ms dropped, refitted) gives the
   offset at clip time 0 and the **drift** in ppm: recorder time = `offset + t * (1 + ppm/1e6)`.
   Clips under 20 s can't measure drift and take the median of the others on the same recording.

The report has one line per clip: recording, offset, drift (`*` = borrowed), confidence, mean
fine correlation, coarse margin, inlier windows and the RMS residual. `confidence` is fine
correlation x margin (1 − runner-up / peak) x inlier share; under `--min-confidence` (0.15) a clip
is reported but gets no audio clip. Offsets are on the media's pts timeline, so AAC priming and
edit lists are accounted for.

Each matched clip becomes an audio clip on track `H2` (bus `dialogue`) at the same record time
as its video, its `source_range` in recorder samples at the WAV's rate (clips at mixed frame rates
each keep their own). `metadata.ffcine.sync` holds the `ffcine.audio.sync/1` result, and the
renderer resamples by the drift. `--lead`/`--tail` make J/L cuts: the recorder audio starts before
/ runs past its video, on two checkerboarded tracks (`H2 A`, `H2 B`) so neighbours can overlap. A
J/L cut is just an offset source range, so it can be edited by hand too. Each clip gets
`ffcine.audio.clean/1` (`--wind` for the outdoor preset) and `ffcine.audio.level/1`.

Checked on real iPhone clips (AAC, 48 kHz) planted in synthetic recordings with pink noise, a
different EQ and a +30 ppm clock: 4/4 found, and the rendered mix lines up with the camera audio
within 0.4 ms.

## Clean chain: `ffcine.audio.clean/1`

Runs through libavfilter while the clip loads, at the mix rate, with 0.5 s pre-roll so adaptive
filters settle. The chain's latency (afftdn: 25 ms) is measured once per chain and compensated.
Dialogue-track clips without the effect get the prototype's `clean_filters` for their mode;
ambience gets `highpass=f=60,lowpass=f=14000`; music and SFX nothing.

| Field | Default | Filter |
|---|---|---|
| `amount` (`strength`) | 0.6 (`nat`: 0) | `c` below; 0 turns denoise and EQ/de-esser off |
| `highpass` | 80 | `highpass=f=80` (12 dB/oct), `null` for none |
| `lowpass` | none | `lowpass=f=...` (12 dB/oct) |
| `wind` | false | `highpass=f=150:p=2` twice (24 dB/oct), `lowshelf=f=300:g=-6`, `adeclick` |
| `declick` | false | `adeclick` |
| `denoise` | `afftdn` | `afftdn nr=6+10c nf=-34+6c tn=1`; `arnndn` with `model` (RNNoise file, `mix=c`); `off` |
| `dialogue` | on for `orig` | `equalizer f=3200 w=1.1 +1.8c dB`, `deesser i=0.35c`, `acompressor -24 dB 2.5:1 8/160 ms makeup 1` |

## Levels: `ffcine.audio.level/1`

Per clip, after cleaning and drift correction: R128 integrated loudness (`ebur128`) to the target
(dialogue gain clamped to −10..+14 dB), then `gain_db`, `ffcine.audio.pan/1`, fades. The
`edit.yaml` importer writes these from the prototype's fields.

| Field | Default |
|---|---|
| `mode` | by bus: `dialogue` (`orig`) −18 LUFS, `music` −20 (on the whole track, so quiet intros stay quiet), `ambience` −36, `sfx`; also `nat` −22, `off`. `music` on a dialogue track (a shot without its own audio) is muted unless `nat_db` gives it −22 + `nat_db` |
| `lufs` | explicit target, overrides `mode` |
| `ref_dbfs` | `sfx`: the loudest 100 ms at −18 dBFS RMS, peaks under −1 dBFS, so `gain_db` 0 is as loud as the talking |
| `gain_db` | 0; number, keyframes or an expression over `st` (`docs/timeline.md`), e.g. Resolve clip volume; a key's `ease` is smoothstep |
| `duck_db` | how far the music dips while this clip plays: `orig` −14, `nat` −7, others 0 (SFX: punchlines like a record scratch) |
| `fade_in`, `fade_out` | dialogue 0.125 s (the cut crossfade: each dialogue clip reads 62.5 ms past both ends), ambience 0.8 s, others 10 ms; a `Transition` on the track is an equal-power crossfade instead |

`ffcine.audio.loop/1 {in, out, crossfade}` loops the source range with equal-power seams (1 s)
to fill the clip (ambience beds). `LinearTimeWarp` speeds go through `atempo` (pitch kept); a
freeze frame is silent.

## Buses and master

A track's bus is `metadata.ffcine.bus` (`dialogue`, `music`, `ambience`, `sfx`), else its name
(`music`, `amb`, `sfx`/`fx`), else dialogue. With no audio tracks, the first video track's clips
are the dialogue (camera audio; clips without an audio stream, stills say, are silent); once
there is one, camera scratch audio is not mixed.

Ducking (the prototype's 100 Hz envelope): each dialogue clip's `duck_db` (default
`project.mix.duck_db`, −14; `nat` clips −7) dips the music over the clip; the dip starts 0.25 s early, holds 0.225 s after and is Hann-smoothed. SFX dips are short
(0.04/0.35 s). Ambience follows the dialogue dips, at most −6 dB. `--sidechain` ducks only while
the dialogue bus is above −40 dBFS (10 ms RMS) instead of over whole clips.

Master: the sum fades out over the last 0.6 s, is gained to −14 LUFS and run through a
lookahead limiter (2 ms, 60 ms release) on a 4x-oversampled peak estimate, its ceiling 2 dB
(`--headroom`) under the −1 dBTP target because the AAC encode overshoots by 1.5–2 dB. Up to 4
passes until within 0.25 LU and under the ceiling as `ebur128` measures it. `--no-normalize`
keeps the mix level (still limited).

On a 4-minute travel edit (`ffcine
 import edit.yaml`, 4 min) `ffcine audio` takes 12 s and every
stem lands within 0.4 LU of the prototype's: dialogue −18.7 (−18.8), music −22.9 (−23.3),
ambience −39.9 (−39.8), SFX −22.4 (−22.2), master −14.1 LUFS / −3.1 dBTP (−14.1 / −3.0).

### Bus gain, filters and sends

`project.mix.buses` ([timeline.md](timeline.md#buses)) sets a fader and a libavfilter chain per bus,
and routes sends to aux buses, which are any other name in the map. A bus runs clips, `filter`,
`gain_db`, ducking; the stem and the master take it after that. A send taps it after the ducking,
or with `pre: true` before the fader and the ducking.

```json
"buses": {
  "music": { "gain_db": -2 },
  "ambience": { "filter": "lowpass=f=9000" },
  "dialogue": { "sends": [{ "to": "room", "gain_db": -14 }, { "to": "key" }] },
  "room": { "reverb": { "room": 0.7, "damp": 0.4 }, "gain_db": -3 },
  "key": { "duck": { "targets": ["music", "ambience"], "depth_db": -9 } }
}
```

An aux bus sums its sends, then runs a `reverb` (Freeverb, wet only) and/or a `filter`, then its
`gain_db`, into the master; it gets a stem of its own (`room.wav`). The tail stops at the end of the
timeline. An aux with `duck` is a key bus: it isn't heard, and while its 10 ms RMS is over
`threshold_db` its targets dip by `depth_db`, on top of the ducking above, with the same envelope
(`attack` look-ahead, `release` hold, Hann-smoothed). Keys read the buses before any ducking, so
keys can't chain or feed back. An aux can't send on. A send to a bus that isn't defined, or a name
that doesn't fit a file name, fails the mix.

## Memory

The mix runs in 64k-frame chunks over scratch files, one clip then one bus at a time: clips decode,
clean and resample as a stream, are measured and levelled from a file, and each pass of the master
reads the sum back through a streaming limiter. What stays in memory is a few chunks, the 100 Hz duck
envelopes and the decoders, so a mix doesn't grow with the timeline:

| Synthetic mix, four buses, `ffcine audio` | Peak RSS in memory | Peak RSS streaming | Time |
|---|---|---|---|
| 15 min, with stems | 2336 MB | 106 MB | 30 s -> 44 s |
| 60 min | 9119 MB | 123 MB | 132 s -> 155 s |

The 15-minute master and stems are bit-identical to the in-memory mix. A test holds the Rust heap
under 16 MB for a 10-minute mix at 8 kHz.

The cost is disk: about 1.4 GB per hour for each bus, the sum and the master, less for buses with
few clips (sparse files). `ffcine audio` keeps them in a hidden directory next to the output and
`ffcine render` under its working directory; the library uses `MixOptions::scratch`, else the system
temporary directory, which may be RAM. `render_audio` still returns the mix in memory;
`stream_audio` hands back a `StreamedMix` to write out. A WAV stops at 4 GiB, about 3.1 hours of
32-bit stereo at 48 kHz: longer mixes fail with an error. `ffcine sync` and `ffcine beats` analyse
whole recordings and tracks in memory.

## macOS

`nix develop .#audio` (Apple Silicon or Linux) has cargo and libav*, no GPU stack:

```sh
nix develop .#audio -c cargo build -p ffcine-cli --no-default-features --features audio
nix develop .#audio -c cargo test -p ffcine-audio
```

That binary has ingest, catalog, `sync`, `audio` and `beats`. CI builds it and runs the tests on a
macOS runner.

## Beats

`ffcine_audio::beats` finds a music track's tempo and beats for cut snapping: librosa's
`beat_track` as the prototype ran it (`pipeline/audio.py` `beats()`), ported. Mono at 22.05 kHz;
onset strength is the median over 128 mel bands of the positive frame-to-frame rise of the log-mel
spectrogram (2048 FFT, 512 hop); tempo is the peak of the mean onset autocorrelation over 8 s
windows under a log-normal prior around 120 BPM; beats come from Ellis' dynamic programme
(tightness 100), with weak beats at either end trimmed.

```sh
ffcine beats music.mp3            # tempo, then one beat time (s) per line
ffcine beats --json music.mp3     # {tempo, beats, duration}
```

Results are cached as JSON in `$XDG_CACHE_HOME/ffcine/beats` (else `~/.cache/ffcine/beats`),
keyed by canonical path, size and mtime; `--no-cache` re-analyses. `ffcine import` reads the same
cache for `edit.yaml`'s beat snapping ([timeline.md](timeline.md#edityaml-import)). On the three
sample music tracks the tempo equals librosa's and every beat is within 23 ms of it; the trimmed
ends differ by one beat on two of them (resampler differences at the trim threshold).
