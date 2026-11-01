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
(20), `--stab-zoom` (fixed), `--stab-max-zoom`. The library entry is
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
- Gyro (`telemetry-parser`) can feed the same smoothing later.
