# GPU stabilizer

`ffcine stabilize` replaces render.py's two-pass vidstab (`vidstabdetect` + `vidstabtransform`,
CPU) with optical-flow detect on the GPU and a warp in the libplacebo pass. Code:
`crates/ffcine-stabilize/`, `crates/ffcine-core/src/geom.rs`. Harness: `bench/stab.sh`. Run on
2026-10-31, RTX 5060 Ti (driver 615.78.08), shared box, everything at `nice -n 19`.

Spelled `stabilise` until 2026-11-01: `ffcine stabilise`, `ffcine grade --stabilise` and the
edit.yaml `stabilise` key still work, and motion caches under `~/.cache/ffcine/stabilise` are
still read (new ones go to `~/.cache/ffcine/stabilize`).

## Method

1. **Detect** (`stabilize::detect`, feature `nvof`): NVDEC -> libavfilter
   `setpts=PTS-STARTPTS,fps=RATE,scale_cuda=W:H:format=nv12` (1920 wide by default) -> luma copied
   device to device -> NVIDIA Optical Flow (SDK 2.0 API, `slow` preset, 4x4 grid, cost output,
   temporal hints) between consecutive frames. The range mirrors render.py: `-ss start`, then
   `frames` frames at `rate`. Nothing leaves VRAM except the flow field (480x270 vectors + costs).
   `libnvidia-opticalflow.so.1` and `libcuda.so.1` are dlopened, so the binary runs without them
   until detect is asked for.
2. **Global motion** (`motion::estimate`): drop the border and high-cost blocks, 2-point
   similarity RANSAC, then IRLS (Cauchy weights inside a shrinking gate) on a **free affine**. The
   frame's rotation and scale come from the affine's first column: horizontal pans under a rolling
   shutter shear rows, and a similarity fit reads part of that shear as rotation (it doubled the
   per-frame angle noise). Frames under 10 % inliers become the identity.
3. **Smoothing** (`path`): absolute camera path from the frame-to-frame motions, vidstab's Gaussian
   (window `2s+1`, `mu = s`, weights `exp(-(i-mu)^2/(mu/2)^2)` renormalised at the ends), warp
   `W_i = A_i S_i^-1` per frame.
4. **Zoom**: the exact smallest static zoom for which every output frame's four corners land in
   its source (closed form per corner and edge), capped by `--max-zoom`, or `--zoom` fixed.
5. **Warp** (`hook::warp_hook`): the `geom::Mat3` per frame (output pixel -> source pixel, source
   pixel coordinates, before the grade's crop/scale) as an mpv `.hook`: `MAIN` stage, Catmull-Rom
   bicubic (16 taps, vidstab's `interpol=bicubic`), black outside, matrices in an embedded rgba32f
   `//!TEXTURE` indexed by libplacebo's `frame` counter.
6. **Cache**: detect results in `$XDG_CACHE_HOME/ffcine/stabilize/<fnv1a>.txt`, keyed by canonical
   path, size, mtime, range and every detect/estimate parameter (the key is stored in the entry
   too). Smoothing and zoom are recomputed every time; they take microseconds.

## Results (`verdict`: dji:0048 2.6 s, 199 frames, smoothing 22)

Residual jitter: `ffcine jitter` on each render (the same NVOF estimator), RMS of the frame-to-frame
motion minus its Gaussian (sigma 5) smoothing, the pair at the 1.2x punch-in (frame 153) excluded.
Lower is steadier.

| render | tx px | ty px | angle deg |
|---|---|---|---|
| unstabilized | 13.33 | 3.27 | 0.051 |
| render.py `--gpu` (vidstab) | 5.02 | 2.65 | 0.070 |
| bench one-pass + vidstab | 4.41 | 2.41 | 0.076 |
| **ffcine** | **4.24** | **2.34** | 0.079 |

- Translation: ffcine is at least as steady as vidstab, a little steadier.
- Rotation: all three are at the metric's floor (~0.05-0.08 deg, about 2 px at the frame edge). Both
  stabilizers come out slightly above the unstabilized clip, likely because correcting rotation
  measured with that much noise adds some of it back. Before the affine refinement ffcine was at
  0.091 deg (similarity-only fit).
- Zoom: ffcine auto 1.177 (exact minimum); vidstab `optzoom=1` reported 14.68 %, i.e. 1.172.
- Side-by-side still (frame 100) and 8 s video in `/tmp/ffcine-stab/verdict_sbs.*`: framing
  matches render.py's, no black borders.

Timings (wall, whole process):

| step | ffcine | vidstab (bench) |
|---|---|---|
| detect, cold | 1.2 s | 17.7 s (`vidstabdetect`) |
| detect, cached | 0.01 s | |
| render, one pass | 5.9 s (5.5 s without the warp) | 76.8 s (one-pass + `vidstabtransform`) |

The bench's no-stab one pass was 5.1 s; the warp costs ~0.4 s for 199 4K frames.

## In the grade pass

`ffcine grade --stabilize` (and `batch` lines with it) stabilizes in the same libplacebo pass as
the grade: detect over the shot's range through the cache, smoothing and zoom, then the warps as a
`MAIN` hook ahead of the LUT's output, crop, scaling and dither. Options: `--stab-smoothing`
(20), `--stab-zoom` (fixed), `--stab-max-zoom`, `--stab-source`, `--stab-focal`,
`--stab-gyro-offset`. The library entry is
`ffcine::stabilize_pipeline(backend, &mut pipeline, &params, cache)`, which sets
`Grade::warp`; a timeline renderer calls it for clips with `ffcine.fx.stabilize/1`, whose
parameters `stabilize::fx::params` turns into `StabilizeParams` (`smoothing`, `zoom` (`"auto"` or a
factor), `max_zoom`; render.py's vidstab-only `shakiness`, `accuracy`, `zoomspeed` are ignored).

Two things differ from the bench's two-pass `vf_libplacebo` setup:

- **Frame index**: the hook reads a `DYNAMIC int` parameter (`ffcine_stab_frame`,
  `hook::warp_hook_dynamic`) that the renderer sets through the parsed hook's `pl_hook_par` before
  each frame, instead of libplacebo's `frame` counter. So it doesn't depend on how many passes the
  file has or what the hook saw before.
- **Crop**: libplacebo only shows `MAIN` the source crop, so with a warp the grade doesn't set
  `pl_frame.crop`; the crop is folded into the matrices (`stabilize::fold_crop`) and the pass
  renders the full frame through them. Crop and stabilization therefore cost one resample.

```sh
ffcine grade --ss 2.6 --frames 199 --fps 24000/1001 --in-color bt709/bt709/bt709/tv \
  --lut grade_d200d83e8d3a468a.cube --upscaler ewa_lanczossharp --downscaler ewa_lanczos --deband \
  --size 3840x2160 --stabilize --stab-smoothing 22 DJI_20260705101105_0048_D.MP4 verdict_stab.mkv
```

Results on `verdict` (2026-11-01, same box, GPU otherwise idle at the time, frames 0-151 since this
render has no punch-in; jitter as above):

| render | tx px | ty px | angle deg |
|---|---|---|---|
| unstabilized (same grade) | 12.34 | 2.29 | 0.045 |
| render.py `--gpu` (vidstab) | 5.29 | 2.01 | 0.076 |
| bench two-pass (ffcine warp in its own `vf_libplacebo`) | 4.32 | 1.80 | 0.083 |
| **ffcine one pass** | **4.31** | **1.80** | **0.069** |

- Same steadiness as the two-pass warp; 0.9849 SSIM Y / 46.5 dB against it (the bench pass also
  ran render.py's hooks and resampled through a yuv444p16le intermediate).
- With `--crop 3200:1800:320:180` the crop folds in correctly (framing checked on frame 100).

| step | ffcine one pass | vidstab (bench) |
|---|---|---|
| detect, cold | 4.5 s (1.2 s on 2026-10-31; shared GPU) | 17.7 s |
| detect, cached | < 0.01 s | |
| render, 199 4K frames | 4.65 s wall (4.74 s for the same grade unstabilized) | 76.8 s (one pass + `vidstabtransform`) |

The warp's cost is below the run-to-run noise here: a stabilized shot renders at plain-grade speed,
16x faster than render.py's vidstab render, and about 10x end to end with a cold detect (9.6 s vs
94.5 s).

## Gyro

`--source auto|gyro|flow` on `ffcine stabilize`, `--stab-source` on `ffcine grade --stabilize`
(default `auto`): the frame-to-frame motions of step 1 can come from what the camera recorded
instead of optical flow. Smoothing, zoom and warp are the same code. Code:
`crates/ffcine-stabilize/src/{gyro,read,parser}.rs`.

1. **Read**: DJI's `dbgi` track (below), else, with the optional `telemetry` feature,
   [telemetry-parser](https://github.com/AdrianEddy/telemetry-parser): the quaternions of the file
   if it has them, else raw gyro readings integrated at their timestamps (trapezoidal).
2. **Frame rotation**: attitude interpolated at each frame's time (`start + k / rate`, plus
   `--gyro-offset` ms), `R = q[k-1]^-1 q[k]` in image axes (x right, y down, z forward).
3. **Motion**: `R` through a pinhole of focal length `f`: where the frame centre lands is
   `(tx, ty)`, the turn about the optical axis is `angle`, scale stays 1. These are ordinary
   `Motion`s, so `stabilize()` can't tell them from flow's.
4. **Cache**: in the same directory under its own key (`gyro` line with version, source choice, focal and offset),
   so the two sources never mix; flow keys are unchanged, so existing caches still hit.

`--source flow` never reads telemetry. `--source gyro` fails if the file has no usable gyro. `auto`
uses gyro only where it is trusted, which today means quaternions read through telemetry-parser.
Otherwise it goes to flow and logs why: no track, a parse failure or panic, readings that don't
cover the range, no focal length, raw gyro (below), or a Pocket 4 (Results: flow is steadier on it).

Focal length, first found wins: `--focal` / `--stab-focal` (source pixels), the lens profile
telemetry-parser reports, then a per-camera table measured against flow (`gyro::camera_focal`,
Pocket 4 only: `f = 0.625 w`, 2400 px at 3840 wide. Fits on clips where flow is confident gave
2250-2870 px, 2336 px on the cleanest pan). An unknown camera with no stated focal length goes to flow under `auto`; there
is no generic default FOV because a wrong `f` scales every correction. The focal length is assumed
fixed for the camera: digital zoom isn't read.

Raw gyro: its axes only come from telemetry-parser's orientation convention, and no such camera was
available to check them against the picture. A wrong sign would double the shake with no error, so
`auto` uses quaternions only, and raw gyro needs an explicit `--source gyro`.

Rolling shutter isn't corrected from the gyro: one attitude per frame, one similarity per frame, as
with flow. Rotation is all gyro sees, so translation (walking bob, parallax) stays in the picture.

### What the cameras record

Checked on the cameras at hand (sampled Pocket 4 clips, one clip of each other camera) with
telemetry-parser's current git version (0.4.0) and the crates.io release:

| camera | what the file has | result |
|---|---|---|
| DJI Osmo Pocket 4 | `djmd` has no IMU data per frame (57 bytes), so telemetry-parser finds nothing. `dbgi` (about 50 kB per frame, undocumented) has two unit quaternions per frame | **gyro**, read directly |
| DJI Mini 4 Pro | `djmd` and `dbgi` with no quaternion anywhere; telemetry-parser returns no samples | flow |
| older DJI (AVC, `DJI.Meta` track) | telemetry-parser: unsupported format; no `dbgi` | flow |
| Canon EOS R10 | MP4 with video, audio and timecode only: there is no metadata track to read. telemetry-parser recognises the file and returns no samples; its Canon support starts at cinema bodies and the R5 Mk2 / R6 Mk3 | flow, can't work |

The Pocket 4's attitude was identified from the data, not from documentation: field path
`2.1.10.7.{3,4}` of each `dbgi` sample, `w x y z` as little-endian `f32`, unit norm. The two sit
about a quarter frame apart (their middle is used). Its axes are x forward, y right, z down, the
same body frame telemetry-parser assumes for other DJI cameras. Its pan and tilt
match image motion at zero lag (correlation up to 0.97 after high-passing both, CPU phase
correlation at 480 px). A firmware that moves the field yields no attitude, and `auto` goes to flow.

### Dependency

telemetry-parser (MIT OR Apache-2.0) is behind the non-default `telemetry` feature and comes from
crates.io at 0.2.6, the last release there (2022). Current versions are git only, and `cargo publish`
rejects a git dependency even when it is optional. Since neither version gets gyro from any camera
tested here, 0.2.6 costs nothing today: it covers GoPro, Sony, Insta360 and older DJI action cameras
(the newer ones need a newer release). No such file was at hand, so this path is only tested on
errors and synthetic data, including the axis conversion `auto` relies on for quaternions. It pulls in
MPL-2.0 `mp4parse` and a few unmaintained crates (`deny.toml`), and `nom` 6 has a future-incompat
warning, which is why it isn't on by default. The Pocket 4 reader needs none of it.

### Results

Pocket 4, six clips picked at random, 240 frames each, default smoothing, `--stab-max-zoom
1.5`, rendered at 1080p with `ffcine grade --stabilize --stab-source ...`, then `ffcine jitter` on
each render (flow's estimator, so it favours flow slightly). Lower is steadier.

| clip (flow confidence) | unstabilized tx / ty / angle | flow tx / ty / angle (zoom) | gyro tx / ty / angle (zoom) |
|---|---|---|---|
| pan (0.59) | 1.55 / 0.49 / 0.017 | **0.62 / 0.24** / 0.021 (1.09) | 0.69 / 0.43 / **0.016** (1.07) |
| small motion (0.58) | 1.52 / 1.37 / 0.034 | **1.49 / 1.13 / 0.032** (1.05) | 1.77 / 1.65 / 0.038 (1.04) |
| hard for flow (0.34) | 4.27 / 2.29 / 0.132 | 4.12 / **3.12** / 0.140 (1.07) | **4.07** / 3.45 / **0.138** (1.09) |
| hard for flow (0.26) | 6.64 / 2.24 / 0.213 | **4.22 / 2.28 / 0.162** (1.16) | 5.72 / 2.61 / 0.209 (1.16) |
| large motion (0.35) | 9.91 / 4.25 / 0.112 | **6.69 / 2.57 / 0.065** (1.17) | 14.36 / 7.15 / 0.096 (1.50, needed 2.09) |
| large motion (0.46) | 8.20 / 4.97 / 0.083 | 12.20 / 7.10 / **0.106** (1.28) | **6.34 / 3.28** / 0.134 (1.50, needed 2.71) |

Flow wins four of the six, ties on one and loses on one, so `auto` stays on flow for this camera:

- A gimbal has already taken out most rotation, so what is left is probably largely translation,
  which an attitude can't see and flow can.
- On frames where flow is confident (above 0.3) the two agree: correlation of horizontal motion
  0.94-0.99 on three of the four clips checked this way, 0.70 on the fourth. The gyro reads the
  rotation correctly; rotation just isn't the whole shake.
- The one gyro win is a clip where flow gives up on some frames (5 identity frames) and leaves the
  shot worse than no stabilization. Gyro follows the motion there, which also asks for a much larger
  zoom to hide the borders (2.71x, against flow's 1.28x): use `--stab-max-zoom` with it.
- Cost: 0.5 s and no GPU for 240 frames, against several seconds of NVDEC and NVOF. `--source gyro`
  also works when the GPU has no memory free for detect.


## Caveats

- With libavfilter (`bench/stab.sh`, `ffcine stabilize --hook`), libplacebo only shows `MAIN`
  the source crop and its `frame` counts every executed pass of the shader file, so the warp runs
  as its own `libplacebo` pass ahead of the grade's, with `--hook-passes`, and a fresh filter per
  shot. ffcine's own pass avoids both (above).
- Keyframed crops (punch-ins, reframe) will fold into the matrices the same way, per frame.
- `//!` anywhere in a hook, even inside a GLSL comment, is parsed as a directive.
- Sources NVDEC can't decode fail detect with ffmpeg's error (no software fallback yet).
- Rolling shutter is only kept out of the rotation estimate, not corrected in the warp.

## Left

- A timeline renderer (not written yet) calls `stabilize_pipeline` per `ffcine.fx.stabilize/1` clip.
- Timeline renders (`ffcine assemble`) stay on flow: their recipe key doesn't include a motion
  source yet.
