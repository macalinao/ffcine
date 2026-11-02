# ffcine design

ffcine is GPU video finishing: decode a clip, grade it (tone mapping, crop/scale, 3D LUT, user
shaders) and encode it, without the frames leaving VRAM. It replaces a Python prototype
(`gpu_grade.py` plus the GPU path in `render.py`), which drives
the `ffmpeg` CLI with `-init_hw_device cuda=cu:0 -init_hw_device vulkan=vk@cu` and a
`libplacebo=...` filter string.

## What the prototype does

Per shot, on one NVIDIA card:

1. **Decode**: NVDEC (DJI HEVC) or CPU decode + `hwupload` to Vulkan (iPhone).
2. **Colour in**: iPhone HLG / Dolby Vision 8.4 and Display-P3 go through libplacebo with
   `tonemapping=bt.2390:gamut_mode=perceptual` to Rec.709. Apple Log, DJI D-Log and Canon Log 3 are camera LUTs (`pl_custom_lut`) picked by the probe ([color.md](color.md)).
3. **Grade**: one libplacebo pass with
   - crop + scale: `w/h`, `crop_w/h/x/y` as per-frame expressions (static zoom, timed punch-ins,
     eased push-ins, vertical-output pans), `upscaler=lanczos:downscaler=lanczos:disable_linear=1:sigmoid=0`
     so the geometry matches swscale;
   - the grade as **one baked 3D LUT** (`lut=...:lut_type=normalized`). `gpu_grade.bake()` runs
     the exact CPU filter chain (camera LUT, tame LUT, colorbalance, curves, colorchannelmixer,
     huesaturation, split tone) once over a 65^3 identity lattice and writes a `.cube`, cached by
     a hash of the chain + ffmpeg version;
   - **sharpen** as an mpv `.hook` user shader (`PREOUTPUT`, ffmpeg's `unsharp=5:5:amount` on
     BT.601 luma; `gpu_shaders.py`).
4. **Optional Vulkan filters** before the grade: `nlmeans_vulkan` (denoise), `blend_vulkan`
   chains (tmix).
5. **Encode**: `hwupload=derive_device=cuda` then `hevc_nvenc -preset p6 -tune hq -rc constqp
   -qp 12 -profile:v main10 -pix_fmt p010le -g 24`, tagged Rec.709 tv range.

Concurrency is bounded by a VRAM budget (~1.9 GB per 4K job, more for denoise/blend).

## Features to port

From the fuller prototype, `05_project/render.py` (driven by `edit*.yaml`). Defaults and parameter
mappings should carry over exactly so ffcine renders can be diffed against the prototype's.

| Feature | Prototype | ffcine |
|---|---|---|
| Colour in | probe decides: HLG/PQ trc, or P3/BT.2020 primaries with a bt709 trc -> libplacebo `tonemapping=bt.2390:gamut_mode=perceptual` to Rec.709 gbrp16le (DV 8.4 RPU applied); BT.2020 + untagged trc = Apple Log -> `lut3d` (analytic `applelog_to_rec709.cube`, tetrahedral); else SDR range conversion | same rules on `MediaInfo`, all in the libplacebo pass; Apple Log becomes a `pl_custom_lut` before the grade instead of a CPU `lut3d` |
| Grade | camera LUT (DJI D-Log M `normal`/`vivid`, `lut_strength` mix), `rgb_gain` mixer, warmth/tint `colorbalance` (always `pl=0`), 17-point curve (exposure in stops with soft shoulder, gamma, sine S-contrast, lift/gain), Rec.709-luma saturation matrix, raw `extra` filters; iPhone `tame` OKLCh LUT; baked to one 65^3 LUT on the GPU path | evaluate the same maths in Rust over the lattice (no ffmpeg needed); only `extra` falls back to a libavfilter bake (`ffcine bake`). libplacebo samples the LUT itself (not tetrahedral), as the prototype's GPU path already does |
| Denoise | CPU `hqdn3d=d:0.75d:1.5d:1.2d` (yuv444p16le, before the grade); GPU `nlmeans_vulkan=s=0.2d:p=3:r=5:t=1`, spatial only (no GPU temporal denoiser in ffmpeg) | `Stage::Denoise { luma, chroma, radius }` before the grade: `nlmeans_vulkan` through libavfilter on the shared `AVHWDeviceContext`, per-plane strengths with chroma harder than luma, `r=7` for previews and normal renders and `r=15` for low-light shots in the final render, `p` at the filter default; previews denoise the downscaled proxy. `hqdn3d` stays only as a CPU fallback; a temporal Vulkan denoiser of our own later |
| Tone + gamut map | fixed `tonemapping=bt.2390:gamut_mode=perceptual` | same BT.2390 curve and perceptual gamut mapping, with `pl_peak_detect` measuring the peak per frame and resetting it on scene cuts instead of trusting a fixed/static peak |
| Scaling | `lanczos` both ways, no linear light or sigmoid (matches swscale) | `ewa_lanczos` family (`ewa_lanczossharp` up for reframes, `ewa_lanczos` down for 4K -> 1080p); `scaler: compat` keeps the prototype's for diffing |
| Deband | none | new: `pl_deband` on the source planes; on by default for DJI D-Log M |
| Dither | not set (libavfilter defaults) | `pl_dither` blue noise to the output depth; on by default for DJI D-Log M |
| Sharpen | CPU `unsharp=5:5:amount`; GPU mpv `.hook` (PREOUTPUT, BT.601 luma) matching it; `sharpen_up` (0.45) on upscaled shots | `Grade::sharpen` as a generated hook (`ffcine::shaders::unsharp`, already in the API) for prototype diffs; `adaptive-sharpen` `.hook` supersedes it as the default |
| Chroma / AI upscale | none | `.hook` shaders: KrigBilateral (CHROMA) for 4:2:0 sources; FSRCNNX or ArtCNN for 1080p clips in a 4K timeline |
| Stabilize | two-pass `vidstabdetect` (shakiness 6, accuracy 15) -> cached `.trf`; `vidstabtransform` (smoothing 20, optzoom 1, zoomspeed 0.25, bicubic, crop black) on CPU after the grade, before crop/scale | a GPU stabilizer, right after the grade path: NVOFA optical flow to detect, trajectory smoothing on the CPU, warp in the libplacebo pass, transforms cached by source range. CPU `vidstab` via libavfilter only as the tuning reference (it makes a stabilized shot about 18x slower). Gyro (`telemetry-parser`) can feed the same smoothing later |
| Grain | `noise=c0s=g:c1s=0.4g:c2s=0.4g` (temporal+uniform) once at assembly, after overlays/captions, in yuv444p16le (final) | libplacebo's AV1/H.274 grain synthesis (`pl_frame.film_grain`, params generated from `g` with chroma at 0.4) in the grade pass, replacing the CPU `noise`; `filmgrain.glsl` as a hook alternative. It applies to the source planes per shot, so captions/cards stay clean and the strength needs retuning against the prototype |
| Audio | per clip: `highpass=80` -> `afftdn nr=6+10c nf=-34+6c tn=1` -> (dialogue) `equalizer 3.2 kHz +1.8c`, `deesser i=0.35c`, `acompressor -24 dB 2.5:1 8/160 ms`; `atempo` for speed; EBU R128 levelling (dialogue -18, nat -22 LUFS, gain clamped -10..+14); ambience beds with 1 s loop crossfades; music cues at -20 LUFS with a 100 Hz duck envelope (attack 0.25, release 0.45); SFX bus; master `volume` to -14 LUFS, 4x-oversampled `alimiter` at true peak -2 dB, up to 3 passes to land within 0.25 LU | a separate audio graph rendered per timeline, not per shot: the per-clip chain via libavfilter (all LGPL filters), mixing/ducking/fades in Rust, `ebur128` for loudness and true peak, `rubato` for the oversampled limiter |

Also in `render.py`, ported after these: punch/push/pan crops (already libplacebo crop expressions),
tmix `blend`, speed ramps, vertical `fit: contain` blur-fill, split-screen layouts, captions and
title cards.

## Pipeline in ffcine

```
demux (libavformat)
  -> decode (libavcodec, hwaccel: Vulkan video or NVDEC/CUDA)
  -> AVHWFramesContext (Vulkan) ............ frames stay in VRAM from here
  -> pl_vulkan_wrap / pl_map_avframe (libplacebo on ffmpeg's VkDevice)
  -> pl_render_image: deband, film grain, colour conversion, tone + gamut mapping (peak detect),
                     crop/scale, LUT, hooks, dither
  -> Vulkan output frame (p010 / nv12)
  -> encode (NVENC via CUDA<->Vulkan interop, or Vulkan video encode)
  -> mux (libavformat)
```

- **One device**: ffmpeg owns the `VkInstance`/`VkDevice` (`AVVulkanDeviceContext`, optionally
  derived from a CUDA device so the same physical GPU is used). libplacebo imports it with
  `pl_vulkan_import`, including the queue families and their flags (needs libplacebo API >= 365
  for ffmpeg 9's internally synchronized queues; the flake pins libplacebo git, API 375).
- **No CPU round trip**: decoded frames are mapped into libplacebo as `pl_frame`s
  (`pl_map_avframe_ex`, from `libplacebo/utils/libav.h`), rendered into a pooled Vulkan output
  frame, and handed to the encoder as a hw frame. Semaphores from `AVVkFrame` order the work.
- **As built (step 1)**: the Vulkan device comes first, with CUDA derived from it. ffmpeg's Vulkan
  encoders crash on CUDA-derived devices, which have multiplane disabled. Only frames shared with
  CUDA are made one image per plane (`AV_VK_FRAME_FLAG_DISABLE_MULTIPLANE`).
  - Vulkan video decode feeds libplacebo with no copy.
  - NVENC gets one Vulkan -> CUDA copy per frame. `hevc_vulkan` needs no copy, but loses about
    2 dB at the same QP.
  - Decode, render and encode run on three threads, with bounded queues between them.
  - `ffcine batch` keeps the device, renderer and `pl_cache` across shots.
  - Measurements and the zero-copy routes: [bench-ffmpeg-gpu.md](bench-ffmpeg-gpu.md#ffcine-grade-step-1).
- **LUTs** are parsed on the CPU (`ffcine::Lut`) and uploaded once as `pl_custom_lut`.
- **One pass**: deband, AV1/H.274 film grain, tone/gamut mapping with `pl_peak_detect`, EWA
  scaling, the LUT, hooks and blue-noise dither are all `pl_render_params` of the same
  `pl_render_image` call as the grade, so none of them costs an extra round trip.
- **Shaders**: generated hooks (`ffcine::shaders::unsharp`, the reframe/warp geometry, the
  safe-area overlay) and user `.hook` files are parsed with `pl_mpv_user_shader_parse` and passed
  as `pl_render_params.hooks`, with `//!PARAM`s set per frame from the timeline. libplacebo's
  `OUTPUT` stage is after the encode to YCbCr, so RGB shaders run at `PREOUTPUT`
  ([hooks.md](hooks.md)). Permissive third-party hooks are vendored under
  `crates/ffcine-hooks/shaders/` with their licence headers: `adaptive-sharpen` (BSD-2-Clause),
  ArtCNN (MIT). Copyleft ones (KrigBilateral, LGPL-3.0+; FSRCNNX, GPL-3.0) are loaded by name from
  user-downloaded files. `filmgrain.glsl` (haasn, no licence stated) isn't used: grain is our own
  compute pass in `ffcine-filters`.
- **Per-frame crop expressions** (punch-ins/push) become keyframed crops evaluated in Rust per
  frame rather than ffmpeg expression strings: `Grade::crops`, sub-pixel and unrounded, or with a
  stabilizer warp one `MAIN` hook sampling `warp * window` ([reframe.md](reframe.md)).

Later stages (denoise, tmix/blend, overlays) slot in as more `Stage`s, each a Vulkan pass between
decode and encode. Denoise runs before the grade (on the downscaled proxy for previews); the
GPU stabilizer ends up as a warp in the libplacebo pass. LUT baking from an ffmpeg CPU filter chain can become `ffcine bake` via
libavfilter.

## Crates

One workspace, one crate per concern, so roadmap steps land in their own crate. `ffcine` is a
facade that re-exports the library crates under the module names they had before the split
(`ffcine::timeline`, `ffcine::import`, `ffcine::stabilize`, `ffcine::backend`, ...); the CLI
uses it.

| Crate | What | Depends on |
|---|---|---|
| `ffcine-core` | `Error`/`Result`, `ColorSpace`, `Lut` (.cube), `MediaInfo`/`Rational`, `geom` (`Mat3`), libav-free probes (`bmff` MP4/MOV/CR3, `wav`) and picture profile detection (`profile`); no GPU, feature `ffmpeg` adds `Error::Ffmpeg` | - |
| `ffcine-ingest` | card offload (verified copy, manifest, backup), per-camera clock fixes and checks, `catalog.json` ([ingest.md](ingest.md)); no GPU or libav, runs on macOS | core |
| `ffcine-timeline` | OTIO JSON model, `ffcine` metadata namespace, `Project`/`GradeParams` layering | core |
| `ffcine-import` | edit.yaml, Resolve OTIO, FCPXML, Kdenlive/Shotcut MLT importers | core, timeline |
| `ffcine-stabilize` | NVOF detect (feature `nvof`), motion, path smoothing, warp hook | core |
| `ffcine-render` | `Pipeline`, `Stage`, `Grade`, `EncoderSettings`, generated shaders, and the ffmpeg + libplacebo backend (features `ffmpeg`, `placebo`); runs the filters' kernels (NVOF motion through stabilize) | core, filters, stabilize, libplacebo-sys |
| `ffcine-filters` | frame filters: motion-compensated temporal denoise, spatial NLM, film grain, as generated GLSL compute kernels ([Frame filters](#frame-filters), [denoise.md](denoise.md)); no GPU dependency | - |
| `ffcine-reframe` | step 5: `ffcine.geo.reframe/1` keyframed crop windows, render profiles (9:16, 4:5, ...), safe-area overlay, composition with the stabilizer warp ([reframe.md](reframe.md)) | core, timeline, render, stabilize |
| `ffcine-audio` | step 6: recorder sync, clean chain (libavfilter), R128 levelling, buses with ducking, master limiter, `render_audio` ([audio.md](audio.md)); feature `ffmpeg` | core, timeline |
| `ffcine-assembly` | step 8: segment planner, content-keyed recipe cache, stream-copy assembly, text per segment, audio mix/remux, `diff` ([assembly.md](assembly.md)) | core, timeline, render, text |
| `ffcine-text` | `ffcine.text` clips: style cascade, `cosmic-text` layout, per-segment GPU overlays, `.ass` import ([text.md](text.md)) | core, timeline, render |
| `ffcine-delivery` | YouTube chapters from markers, music credits, the frame under a timeline time ([delivery.md](delivery.md)) | core, timeline |
| `ffcine-hooks` | step 9: vendored adaptive-sharpen, `ffcine.fx.shader/1` with `//!PARAM`s, hook order ([hooks.md](hooks.md)) | core, timeline, render, reframe |
| `ffcine-triage` | `ffcine triage`/`search`/`snap`: clip index from proxies (strips, scene cuts, speech map, batched GPU whisper, flags, Immich locations), clip page, word-boundary snapping ([triage.md](triage.md)); feature `ffmpeg` decodes | ingest |
| `ffcine` | facade: re-exports core, timeline, import, ingest, stabilize, render, reframe, hooks, text, delivery, filters, triage, audio, assembly; features `ffmpeg`, `audio` (libav* only), `placebo`, `nvof` forward to them | all of the above but the skeletons |
| `ffcine-cli` | the `ffcine` binary (bpaf); feature `gpu` (default) adds the GPU commands, without it only ingest/catalog/doctor/search/snap build; feature `audio` adds `sync` and `audio` (libav*, no GPU, so the Mac build can have them) | ffcine |
| `libplacebo-sys` | bindgen FFI over libplacebo | - |

```
ffcine-cli -> ffcine -> ffcine-assembly -> ffcine-text, ffcine-render, ffcine-timeline
                     -> ffcine-import -> ffcine-timeline -> ffcine-core
                     -> ffcine-ingest --------------------> ffcine-core
                     -> ffcine-stabilize -----------------> ffcine-core
                     -> ffcine-audio -> ffcine-timeline
                     -> ffcine-render -> libplacebo-sys
                                      -> ffcine-filters
                     -> ffcine-triage -> ffcine-ingest
                                      -----------------> ffcine-core
```

New crates take dependencies only on crates below them; when a skeleton gains code, add it to
the facade (and a feature if it pulls in GPU libraries).

## Library API

Backend-neutral types in `ffcine-core` and `ffcine-render`, all re-exported from `ffcine` (no
ffmpeg/libplacebo types leak):

- `ColorSpace { primaries, transfer, matrix, range }`, with `BT709_TV` (encode) and `BT709_RGB`.
- `Lut` (`from_cube_file`, `identity`) + `LutType { Native, Normalized, Conversion }`.
- `Grade { lut, tone_mapping, gamut_mapping, peak_detect, crop, size, scaling, deband, film_grain,
  sharpen, shaders, dither, output }`;
  defaults mirror the prototype (bt.2390, perceptual, lanczos without linear light/sigmoid,
  Rec.709 tv out).
- `EncoderSettings { codec, preset, tune, profile, rate, pixel_format, gop, color, extra }`, with
  `nvenc_hevc_master()` (default) and `x264_preview()`.
- `Overlay { mask, parts, from, to, fade_in, fade_out }`: coverage masks composited in the grade
  pass as `pl_overlay`s over a frame range (text, [text.md](text.md)); `DeliveryProfile`
  (`master`, `preview`, `review`, `phone`) plans encoder and size ([delivery.md](delivery.md)).
- `Pipeline { input, output, stages: Vec<Stage>, encoder, device }`, built with
  `Pipeline::new(in, out).grade(g).encoder(e).device(Device::Cuda(0))`.
- `trait Backend { probe(path) -> MediaInfo; run(&Pipeline) }`.

Backends sit behind cargo features: `ffmpeg` (libav* via `ffmpeg-next`, provides
`backend::Ffmpeg`) and `placebo` (`libplacebo-sys`). Both are on by default; a different backend
(e.g. GStreamer, or a pure-Vulkan decoder) would implement `Backend` without touching the types.

```rust
use ffcine::{Backend, Device, Grade, Lut, LutStage, LutType, Pipeline, backend};

let ff = backend::Ffmpeg::new()?;
let grade = Grade {
    lut: Some(LutStage { lut: Lut::from_cube_file("grade.cube")?, kind: LutType::Normalized }),
    sharpen: 0.6,
    ..Grade::default()
};
Pipeline::new("in.mov", "out.mkv").grade(grade).device(Device::Cuda(0)).run(&ff)?;
```

## Frame filters

Decision: our own filters (denoise, grain; deband/sharpen later if libplacebo's aren't
enough) live in `ffcine-filters`, and should run in as few GPU passes as possible. As built:
[denoise.md](denoise.md) (method, numbers, crops).

- A `Filter` has a `Point` (where relative to the grade pass), a `Footprint` (`Pixel`,
  `Spatial { radius }`, `Temporal { past, future, radius }`) and whether it needs motion.
  `FilterChain::plan()` groups filters into dispatches: a pointwise filter folds into the
  dispatch before it, a gathering one starts a new dispatch. Kernels are generated GLSL
  (`ffcine_filters::denoise::*_kernel`, `grain::hook`); the crate has no GPU dependency.
- `Point::Source` (denoise) runs on the decoded planes as compute dispatches on the grade's
  `pl_gpu` (`pl_dispatch_compute`, so ffmpeg's `VkDevice`), right before `pl_render_image`, into
  textures that the render reads as its source frame. Not a `pl_hook`: temporal denoise needs
  frames after the current one, and a hook can't delay its output. The render thread holds a ring
  of `2r+1` frames (copied, so the decoder's pool isn't pinned) and renders frame `c` once frame
  `c+r` is in; the encoder sees the same frames and timestamps, `r` frames later.
- `Point::Output` (grain) is pointwise and becomes an mpv `.hook` on `OUTPUT`, so it fuses into
  the render pass itself: no dispatch of its own.
- Temporal denoise: motion from NVOF by default (ffcine-stabilize's session, half-res 8-bit
  luma handed from Vulkan to CUDA through an exported buffer, flow back as a texture, rescored
  in Vulkan) or Vulkan block matching (`--temporal-motion blocks`, also the fallback); then a
  per-pixel weighted merge (patch distance + block confidence, falling back to the centre
  pixel), then a light spatial NLM. Strengths are noise sigmas in 8-bit code values on
  10/16-bit data. Measurements: [denoise.md](denoise.md#motion-nvof-default-or-vulkan-block-matching).
- Timeline: `ffcine.fx.denoise/1` (`temporal: {radius, strength}`) and `ffcine.fx.grain/1`;
  CLI: `grade --temporal-radius N --temporal-strength S --denoise S --grain S` (and `batch`).
- `nlmeans_vulkan` through libavfilter stays the reference to compare against.

## Timeline format (proposed)

The ffcine timeline is plain OTIO JSON (`Timeline.1`, core schemas only), readable by any OTIO
0.17+ consumer.
Importing Resolve, FCP/Premiere, EDL, Kdenlive and CapCut projects: [import.md](import.md).

- **Editorial is native OTIO**: tracks, `Clip`s with source ranges, `Gap`, `Stack` for compounds and
  split-screens, `LinearTimeWarp`/`FreezeFrame`, `Transition(SMPTE_Dissolve)`, `Marker` for chapters.
- **ffcine semantics live in namespaced metadata**: `Effect.1` with
  `effect_name = "ffcine.<domain>.<name>"` and its parameters under `metadata.ffcine`, tagged
  `"schema": "<name>/<major>"`. Each schema is versioned on its own, documented, and published as
  JSON Schema; readers have a strict mode (reject unknown versions) and a lenient one (skip with a
  warning).
- **Never a custom `OTIO_SCHEMA` in a typed slot** (`clip.effects`, `track.children`, ...): other OTIO
  readers fail the whole file with `expected Effect, found UnknownSchema`. Custom data only ever
  appears as stock objects plus metadata.
- **Keyframes** have one form everywhere: `[{ "t": RationalTime, "v": ..., "interp": "linear" | "ease" | "hold" }]`.
- **Text** (titles, cards, captions) = clips on their own track with
  `GeneratorReference(generator_kind = "ffcine.text", parameters = {...})`.
- **Settled 2026-11-01** (open questions from PR #5; details in [timeline.md](timeline.md)):
  `metadata.ffcine.frame_size [w, h]` on the timeline; solids are `ffcine.solid {colour}`
  generators; `ffcine.text` takes `font`, `face`, `size` (px at `frame_size`), `colour`,
  `position`, `alignment`, `bold`, `italic`, `style` (a `project.styles` name) and `animation`;
  a transition's kind is flat in the `Transition`'s own `metadata.ffcine` (`ffcine.transition/1`);
  geometry is one `ffcine.geo.transform/1` / `ffcine.geo.crop/1` shape for every importer with
  `units: "picture" | "frame"`, y down, counter-clockwise; `ffcine.comp.blend/1` modes use
  Resolve's names; `ffcine.color.cdl/1` is mirrored to `metadata.cdl`; keyframe `t` is the clip's
  source time.
- **Speed ramps**: curve in `ffcine.time.ramp`, plus a `LinearTimeWarp` with the average speed so
  other tools get the clip length right.
- **Interop mirrors**: primary grade also written as `metadata.cdl` (CMX adapter shape); every clip
  carries a stable `metadata.ffcine.id` so editorial changes from Resolve can be merged back by id.
- **Rust model**: our own serde types for the core schemas, each with
  `#[serde(flatten)] extra: Map<String, Value>` so foreign metadata (`Resolve_OTIO`, ...) round-trips.

**Rendering reads, never writes, the timeline** ([assembly.md](assembly.md)): the first video
track is the picture, `ffcine.text` clips on other video tracks are text, audio tracks are mixed.
Render units are keyed by content (media fingerprint, source range, speed, resolved grade with LUTs
by hash, the clip's other `ffcine.*` effects, profile, ffcine version), never by position or
`metadata.ffcine.id`. No schema changes were needed for assembly.

### Config model

`edit.yaml` already has the shape; it maps onto the timeline like this:

| `edit.yaml` | Timeline |
|---|---|
| `grade` (base), `looks.<name>`, `luts`, `iphone.tame` | `Timeline.metadata.ffcine.project`: `grade` (base `GradeParams`), `looks: {name: GradeParams}`, `luts: {alias: path}`, per-source-kind rules (`light` strength, no camera LUT, `tame`) |
| `output.preview` / `final`, `grain`, `output.audio` | render profiles, outside the timeline (CLI/config): size, encoder, `max_punch`, grain (AV1 grain strength), dither, master loudness, quality (`preview` / `normal` / `final`, which with `low_light` picks the denoise `r`) |
| `shots[]` | `Clip`s; `src`/`in`/`out` -> media reference + `source_range` |
| shot `look`, `grade: {...}` | `ffcine.color.grade/1`: `look` + a partial `GradeParams` override |
| `denoise`, `sharpen` | `ffcine.fx.denoise/1 {luma, chroma, low_light}`, `ffcine.fx.sharpen/1` (also settable in looks) |
| `stabilize` | `ffcine.fx.stabilize/1 {smoothing, shakiness, accuracy, zoomspeed}` |
| (new) deband, tone map, scaler | `ffcine.fx.deband/1 {iterations, threshold, radius, grain}`, `ffcine.color.tonemap/1 {peak_detect, ...}`, `ffcine.geo.scaler/1 {up, down}` (also settable in looks) |
| (new) `.hook` shaders | `metadata.ffcine.project.shaders: {alias: path}` (like `luts`, relative to the timeline), applied by `ffcine.fx.shader/1 {shader, params}`; `params` set the hook's `//!PARAM`s |
| `zoom`, `punch`, `push`, `pan`, `fit` | `ffcine.geo.reframe/1`, keyframed crop window |
| `speed`, `ramp`, `freeze`, `blend` | `LinearTimeWarp` / `ffcine.time.ramp/1`, `FreezeFrame`, `ffcine.time.blend/1` |
| shot `audio` (orig/nat/music), `gain_db`, `clean`, `duck_db`, `audio_lead`/`tail` | audio clip on an audio track: `ffcine.audio.clean/1`, `ffcine.audio.level/1 {mode, gain_db, duck_db}`; J/L cuts are just offset source ranges |
| `music.cues`, `ambience`, `sfx` | clips on music/ambience/SFX tracks (`metadata.ffcine.bus`) with `ffcine.audio.level/1` |
| (new) recorder audio (H2essential) | `ffcine sync`: audio clips on their own track, `metadata.ffcine.sync` = `ffcine.audio.sync/1 {offset, drift_ppm, confidence}` |
| `subs`, `card`, `location`, `title` | `ffcine.text` generator clips; styles in `metadata.ffcine.project.styles` |
| `chapter` | `Marker` |

`GradeParams` is all-`Option` (`lut`, `lut_strength`, `rgb_gain`, `warmth`, `tint`, `exposure`,
`contrast`, `gamma`, `lift`, `gain`, `saturation`, `extra`) and merges base -> look -> source-kind
rules -> clip override (the prototype's order) into the concrete `Grade` the pipeline takes
(schemas: [timeline.md](timeline.md)). Source-kind rules key
off the same probe that picks the camera LUT: DJI D-Log M turns deband and dither on (the log
curve stretched by the LUT bands in skies and walls); other kinds leave them off unless a look or
clip sets them. The library API stays the resolved form; the timeline crate does the layering.

```json
{
  "OTIO_SCHEMA": "Clip.2",
  "name": "payday",
  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 23.976023976023978, "value": 6 },
    "duration": { "OTIO_SCHEMA": "RationalTime.1", "rate": 23.976023976023978, "value": 70 } },
  "effects": [
    { "OTIO_SCHEMA": "Effect.1", "name": "grade", "effect_name": "ffcine.color.grade",
      "metadata": { "ffcine": { "schema": "ffcine.color.grade/1", "look": "day", "gamma": 1.2 } } }
  ],
  "metadata": { "ffcine": { "id": "payday" } }
}
```

## Dependencies

- **ffmpeg**: `ffmpeg-next` / `ffmpeg-sys-next` 9 (tracks ffmpeg 9). `ffmpeg-the-third` and `rsmpeg`
  were considered; rsmpeg lags (ffmpeg 8). `ffmpeg-sys-next` 9.0.1 already binds the Vulkan and
  CUDA hwcontext structs (`AVVulkanDeviceContext`, `AVVkFrame`, `AVVulkanFramesContext`), so no
  extra bindgen is needed.
- **libplacebo**: our own bindgen `crates/libplacebo-sys` against libplacebo v7 system headers
  (`libplacebo-sys` 0.1.0 on crates.io is dead since 2019).
- **Vulkan**: no crate so far. ffmpeg's `VkInstance`/`VkDevice`/queues pass to `pl_vulkan_import`
  as raw handles, and `libplacebo/utils/libav.h` maps frames (compiled in `libplacebo-sys`, feature
  `libav`). `ash` joins if we need our own Vulkan calls (`ffcine-filters` dispatches through
  libplacebo's `pl_gpu` instead). Not `vulkano`: it wants to own the device
  and its sync model.
- **Audio**: `ebur128` (integrated loudness, true peak), `hound` (WAV), `rustfft` (sync
  cross-correlation); resampling (windowed sinc) and the oversampled limiter are our own, libav*
  decodes and runs the clean chain.
- **Analysis**: `av-scenechange` (cut detection, triage), `palette` (OKLCh/colour maths for the LUT
  baker and `tame`).
- **Text**: `cosmic-text` (shaping and layout for captions and cards).
- **Container/time**: `mp4-atom` (chapters, cover art, tags without a remux), `vtc` (timecode at
  NTSC rates).
- **Later**: `telemetry-parser` (git dep) for DJI/iPhone gyro, to drive stabilization.
- **Our own**: `.cube` parser (`ffcine_core::Lut`), and a serde model of the OTIO JSON schema in
  `ffcine-timeline` (there is no production Rust OTIO crate; `mediaio-timeline` is a reference
  only).
- **Avoid**: `vulkano`. `gyroflow-core` isn't planned: GPL is fine while ffcine is private, but the
  NVOFA stabilizer doesn't need it.

Crates join `Cargo.toml` when the code that needs them lands, as a dependency of the ffcine
crate that uses them ([Crates](#crates)).

## Roadmap

1. **Grade path**: decode (NVDEC) -> libplacebo (colour-in rules, tone map, static crop/scale, baked
   LUT, sharpen hook) -> NVENC. Every shot goes through it, the prototype already proved it on
   libplacebo, and everything else layers on it. Done when a DJI D-Log M and an iPhone
   HLG shot match the prototype's cached renders within LUT rounding. Includes the Rust LUT baker
   (`ffcine bake` keeps the libavfilter path for `extra`). Once it matches: BT.2390 + gamut
   mapping with `pl_peak_detect`, deband and blue-noise dither (on for D-Log M), as switches in
   the same pass so the prototype diff can still run with them off.
2. **Stabilize**: a GPU stabilizer right after the grade path, since the bench showed CPU
   `vidstab` makes a stabilized shot about 18x slower than a plain one
   ([bench-ffmpeg-gpu.md](bench-ffmpeg-gpu.md)): NVOFA optical-flow detect, CPU trajectory
   smoothing, warp in the libplacebo pass, transforms cached by source range. libavfilter
   `vidstab` stays only as the reference to tune against.
   Done: `ffcine grade --stabilize` warps in the grade pass ([stabilize.md](stabilize.md)).
   Colour in: camera profiles from the probe, camera LUTs as `pl_custom_lut`, every libplacebo
   tone/gamut mapping ([color.md](color.md)).
3. **Timeline**: OTIO serde model, `GradeParams` layering, and an `edit.yaml` -> OTIO importer, so
   a prototype edit renders shot by shot
 through ffcine with the prototype as reference.
4. **Importers** ([import.md](import.md)), in order: Resolve OTIO (`Resolve_OTIO`), FCP7/Premiere
   XML (`fcp_xml`, by `effectid`), EDL + CDL, our own FCPXML and Kdenlive/MLT readers, CapCut
   drafts last. Unmapped blocks round-trip under `ffcine.foreign.<app>`. Each mapped field
   renders once the step that owns it lands (transforms with 5, audio with 6).
5. **Reframe**: keyframed crops for punch/push/pan and vertical aspect windows, in the same
   libplacebo pass (the prototype already does this as libplacebo expressions), with
   `ewa_lanczos` scaling for punch-ins and 4K -> 1080p. Done ([reframe.md](reframe.md)):
   windows per profile, composed with the stabilizer warp in the same pass, `ffcine render` calling
   it per clip (main picture, split-screen panels, picture-in-picture), contain's blurred fill,
   and the NLE `ffcine.geo.crop/1` / `ffcine.geo.transform/1` on the main picture.
6. **Audio** as its own track: per-clip clean chain, R128 levelling, music/ambience/SFX buses with
   ducking, master limiter. It needs no GPU and is rendered once per timeline, so it can land in
   parallel with 1-5. Done ([audio.md](audio.md)), with Zoom H2essential auto-sync to the camera
   audio (`ffcine sync`) and the mix rendered on its own (`ffcine audio`) for remuxing.
7. **Denoise, grain** (built: [denoise.md](denoise.md)): `nlmeans_vulkan` stage before the grade (per-plane strengths, `r` 7 or 15 by
   quality and low light; `hqdn3d` as CPU fallback), libplacebo AV1/H.274 film grain in the
   grade pass replacing the CPU `noise`. Denoise is only on night/dinner looks and grain needs
   retuning against the prototype, so they wait until the grade matches. Both live in
   `ffcine-filters` as fused compute passes in the grade pass ([Frame filters](#frame-filters)),
   with an NVOF-guided temporal denoiser of our own.
8. **Assembly**: concat with dissolves, captions/cards via `cosmic-text`, chapters and cover via
   `mp4-atom`, replacing the prototype's final ffmpeg pass.
   `ffcine render` assembles a whole timeline from a content-keyed segment cache: only changed
   segments render, the master joins them by stream copy, an audio-only change is a remux, and
   renders resume ([assembly.md](assembly.md)). Still to come: text, chapters, cover, split-screen
   compositing, the full audio graph (step 6).
9. **`.hook` shaders**: vendored `adaptive-sharpen` (replacing the unsharp hook as default),
   KrigBilateral chroma for 4:2:0 sources, FSRCNNX/ArtCNN for 1080p clips in a 4K timeline,
   `filmgrain.glsl`, all through `pl_mpv_user_shader_parse` in the grade pass. adaptive-sharpen
   and `ffcine.fx.shader/1` with `//!PARAM`s are done ([hooks.md](hooks.md)), ArtCNN is vendored,
   and KrigBilateral/FSRCNNX load by name from user-supplied files (their licences keep them out).
