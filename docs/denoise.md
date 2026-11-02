# Denoise and grain

`ffcine-filters` (kernels, parameters, timeline effects) and `ffcine-render/src/backend/denoise.rs`
(running them). Roadmap step 7 ([design.md](design.md#frame-filters)). Run on 2026-11-01, RTX
5060 Ti, shared box (other renders, whisper and llama.cpp on the same GPU, 20-60 % busy):
GPU times come from libplacebo timers and are a ceiling, wall-clock fps are noisy.

```sh
ffcine grade --denoise-preset night --grain 2 in.mp4 out.mkv
ffcine grade --temporal-radius 2 --temporal-strength 3 --denoise 1.5 in.mp4 out.mkv
```

| flag | meaning (strengths are noise sigmas in 8-bit code values, on 10/16-bit data) |
|---|---|
| `--denoise-preset night\|snow` | starting values ([presets](#presets)); the flags below override them one by one |
| `--temporal-radius N` | frames on each side, 1-5 (0 = off) |
| `--temporal-motion nvof\|blocks` | motion source (default nvof, falls back to blocks) |
| `--temporal-strength S`, `--temporal-chroma S` | how different a warped neighbour may be and still count as "the same pixel" |
| `--denoise S`, `--denoise-chroma S`, `--denoise-radius R` | spatial NLM after the temporal pass (chroma defaults to 1.5x) |
| `--denoise-protect K` | [point protection](#point-protection) in both passes (0 = off, the default) |
| `--grain S`, `--grain-size PX`, `--grain-chroma K` | film grain on the graded output |

Timeline: `ffcine.fx.denoise/1 {preset, luma, chroma, low_light, protect, temporal: {radius,
strength, chroma, protect}}` (`temporal: null` turns a preset's pass off) and `ffcine.fx.grain/1 {strength, size, chroma}` ([timeline.md](timeline.md#effect-schemas)).

## Presets

Explicit values, nothing is detected from the footage. Both assume camera noise of a sigma or two
in 8-bit code values: well above that, set `--temporal-strength` to the noise sigma.

| preset | temporal | spatial NLM | protect | for |
|---|---|---|---|---|
| `night` | r4, s2, chroma 3 | R2, 1 / 2.5 | 2 | flat dark sky, stars, point lights |
| `snow` | r2, s1.5 | off | off | bright, little noise, fine texture |

- **`night`** against what used to be the recommendation (r2 s3 + NLM 1.5/2.5): about the same sky
  on moving or white-noise clips, up to 30 % less boiling on still ones, and a star of 6-12 code
  values keeps 3.6x as much of its height. r4 over r3 buys 6-14 % less boiling, for twice the NVOF
  calls of r2.
- **`snow`** drops the NLM. On fine low-contrast texture the NLM is what does the damage: with it
  the output is further from the truth than the noisy input (44.9 vs 48.6 dB).
- A strength in a preset that is too low does nothing rather than something bad. `night` on sigma 4
  white noise leaves the sky at 3.84; `--temporal-strength 4 --denoise 2` gets 0.56, at the cost
  of the faint stars (table below).

The `grade` defaults stay as they were (denoise off until asked for, and `ffcine render`'s mapping
of the prototype's `denoise` number is unchanged), with one exception: the default temporal strength
goes from 4 to 3. There is no prototype flag it has to match. On low-light noise 3 flattens the same
(sky sigma 0.855 vs 0.856, real night clips within 3 %) and keeps more of what stands out (a 12-25
star 0.85 vs 0.73, 0.86 vs 0.70 on low-pass noise). It loses on loud white noise (mid noise below:
32.2 vs 34.1 dB), where the strength should be set to the noise anyway. Point protection is not a
default: it costs 0.45-0.5 dB on that same noise.

### Point protection

The patch distance divides one pixel's difference by 9, so at h = 1 a star 4 code values above a
flat sky is within `2h^2` of every empty patch, and at 6 each of the NLM's 24 other samples still
weighs 0.14: both passes average it away. With protection,
a sample more than `K h` below a brighter centre pixel fades out by `exp(-max(d^2 - (K h)^2, 0) / h^2)`,
whatever its patch looks like. Only downwards: dark outliers still average away. Luma only.

In the merge it needs evidence, or every noise outlier would keep its height: the factor applies in
full only where the warped neighbour's 3x3 patch reaches within `K h` of the centre pixel, so a
point that moved a pixel counts and a lone bright noise pixel doesn't. By then most outliers are
gone, so the NLM needs no such test. Protecting without the evidence keeps a little more of the faint
stars (6-12: 0.79 vs 0.57 on low-pass noise) but leaves salt in white noise (the sky's 99.9th
percentile: 5.7 vs 2.7 code values, 1.24 with the old settings).

Luma-adaptive strengths weren't tried: a faint star barely moves its patch's mean, which is why the
test looks at the pixel.

### Before and after

Synthetic, with ground truth (`bench/denoise/synth.py`, 1080p, 24 frames): point lights of 3-60 code
values with a 0.8 px PSF on a dark sky with a moving glow, drifting 0.15 px a frame. `peak` is the
height kept against the clean clip, by star height. `sky` is the sigma and the frame-to-frame
difference of the error between the stars.

| noise | settings | PSNR Y | peak 3-6 / 6-12 / 12-25 / 25-60 | sky sigma / temporal |
|---|---|---|---|---|
| low-pass, sigma 1.5 | none | 43.24 | 0.99 / 1.00 / 1.00 / 1.00 | 1.76 / 1.98 |
| | before: r2 s3 + NLM 1.5/2.5 | 52.52 | 0.08 / 0.15 / 0.81 / 0.98 | 0.58 / 0.57 |
| | **`night`** | **52.70** | **0.20 / 0.57 / 0.96 / 1.00** | **0.57 / 0.56** |
| white, sigma 2 | none | 40.75 | 0.98 / 0.99 / 1.00 / 1.00 | 2.34 / 2.63 |
| | before | 57.07 | 0.09 / 0.20 / 0.82 / 0.96 | 0.32 / 0.24 |
| | **`night`** | 56.63 | **0.28 / 0.71 / 0.95 / 0.99** | 0.34 / **0.24** |
| white, sigma 4 | none | 34.74 | 0.99 / 0.98 / 1.00 / 1.00 | 4.67 / 5.27 |
| | before | 44.79 | 0.33 / 0.56 / 0.94 / 0.99 | 1.46 / 1.24 |
| | `night` | 36.36 | 0.85 / 0.91 / 0.98 / 1.00 | 3.84 / 4.19 |
| | `night`, s4, NLM 2 | 52.03 | 0.12 / 0.25 / 0.76 / 0.97 | 0.56 / 0.46 |

Fine bright texture (luma 205-220, contrast 2.5, drifting, noise sigma 0.8; `hp` is high-pass energy
against the clean clip, and the noisy input's 1.06 is the noise):

| settings | PSNR Y | hp | SSIM Y | temporal |
|---|---|---|---|---|
| none | 48.61 | 1.06 | 0.9892 | 1.06 |
| before: r2 s3 + NLM 1.5/2.5 | 44.95 | 0.58 | 0.9707 | 0.70 |
| `night` | 47.64 | 0.72 | 0.9840 | 0.56 |
| **`snow`** | **52.71** | **0.90** | **0.9957** | **0.38** |

Real clips, no ground truth (`bench/denoise/patches.py`, 24 frames, 10-bit lossless output). 4K clips
are native-resolution 1080p crops, because full 4K ran out of VRAM on the shared card. Noise comes
from the flattest 512x512 tile, as above, full resolution / 4x4 bins; texture from the most detailed
tile, against the undenoised render. Sigmas bottom out near 0.17: that's the output dither.

| clip | settings | sigma | temporal | texture hp, SSIM |
|---|---|---|---|---|
| 4K DJI night, walking | none | 0.321 / 0.739 | 1.275 / 0.873 | |
| | before | 0.166 / 0.422 | 0.633 / 0.547 | 0.769, 0.9865 |
| | **`night`** | 0.166 / **0.404** | 0.641 / 0.555 | **0.803, 0.9888** |
| 4K DJI night, still | none | 0.397 / 0.692 | 1.512 / 1.049 | |
| | before | 0.166 / 0.267 | 0.613 / 0.538 | 0.370, 0.9756 |
| | **`night`** | 0.166 / **0.208** | **0.496 / 0.433** | **0.407, 0.9776** |
| 4K DJI night, third clip | none | 0.325 / 0.513 | 1.155 / 0.756 | |
| | before | 0.166 / 0.192 | 0.446 / 0.382 | 0.484, 0.9843 |
| | **`night`** | **0.114 / 0.136** | **0.313 / 0.270** | 0.485, 0.9829 |

Bright clips all move, so their temporal numbers are the motion, not noise:

| clip | settings | sigma, 4x4 bins | texture hp, SSIM |
|---|---|---|---|
| 1080p HLG 10-bit, white walls | none / before / **`snow`** | 0.180 / 0.121 / 0.139 | 0.937, 0.9850 / **0.991, 0.9966** |
| 1080p 10-bit, bright fine texture | none / before / **`snow`** | 3.199 / 2.721 / 3.072 | 0.984, 0.9930 / **0.998, 0.9984** |
| 4K DJI D-Log M, overcast sky | none / before / **`snow`** | 0.449 / 0.368 / 0.425 | 0.899, 0.9925 / **0.970, 0.9973** |
| 1080p 8-bit drone, clouds | none / before / **`snow`** | 1.685 / 1.517 / 1.655 | 0.928, 0.9937 / **0.988, 0.9989** |
| 4K R10 8-bit, bright daylight | none / before / **`snow`** | 2.076 / 1.797 / 1.987 | 0.986, 0.9927 / **0.997, 0.9978** |

(The texture columns give before / `snow`.) `snow` keeps 97-99.8 % of the high-pass energy where the
old settings kept 90-98 %, and leaves most of the noise: these clips have little to remove.

**Pending** (no footage, or no VRAM to measure it):

- real snow, aurora and starry-sky clips: stars and snow texture are measured on the synthetic
  scenes only;
- low-light Canon R10 (8-bit H.264 / 10-bit HEVC): the R10 is measured in daylight only. Noisy
  high-ISO clips are the likely case for raising `--temporal-strength`;
- full-frame 4K: the shared card's VRAM was full, so 4K clips were measured as 1080p crops.

`bench/denoise/presets.sh all CLIP[@SS]...` re-runs the preset tables above in one go (the known-noise one is `run.sh` with `ENC` set as in `presets.sh`).

Known noise (`bench/denoise/run.sh`, as below, on a different daylight clip, 24 frames, lossless):

| noise | settings | PSNR Y / U | SSIM Y |
|---|---|---|---|
| heavy | r2 s16 + NLM 8/12 R3 | 37.98 / 47.36 | 0.935 |
| | same, protect 2 | 37.52 / 47.35 | 0.928 |
| | `night` | 25.55 / 30.88 | 0.369 |
| mid | r2 s8 + NLM 4/6 | 41.83 / 49.55 | 0.969 |
| | same, protect 2 | 41.38 / 49.55 | 0.966 |
| | r2 s8 / s4 (old default) / s3 (default) | 39.00 / 34.10 / 32.22 | 0.923 / 0.789 / 0.705 |
| | `night` | 31.83 / 46.41 | 0.685 |
| | `snow` | 31.82 / 37.22 | 0.684 |

Lossless output matters for these numbers: at any lossy rate, even x265 at CRF 0, the encoder alone
takes more than half the height off a faint point, so `presets.sh` encodes with lossless x264.

## How it runs

Per output frame, all compute on the grade's `pl_gpu` (ffmpeg's `VkDevice`), queued on
libplacebo's dispatch right before `pl_render_image`, which reads the result as its source frame.
Frames never leave VRAM and the CPU never waits on the GPU.

1. **Ingest** (each decoded frame): copy the planes into a ring of `2r+1` slots (r16/rg16, so the
   decoder's pool is released at once) and write a quarter-size luma.
2. **Motion** (per neighbour, `2r`): NVOF or Vulkan block matching (below). Each vector carries
   its block's mean absolute difference at full resolution: the match confidence.
3. **Merge** (one dispatch per plane): each neighbour is warped onto the centre with bilinear
   vectors and samples, and weighted by `exp(-max(d2 - 2h^2, 0) / h^2)` on the 3x3 patch distance
   `d2`, times a penalty when the block's MAD is above what noise alone explains (and
   [point protection](#point-protection), if on). The centre has
   weight 1, so a pixel whose neighbours all mismatch (occlusion, a cut, a wrong vector) keeps its
   own value: no ghosting, just less denoising there.
4. **Spatial** (one dispatch per plane): NLM, 3x3 patch, `(2R+1)^2` search, from a shared-memory
   tile. Optional.
5. **Grain**: an mpv `.hook` on `OUTPUT`: luma-weighted Gaussian value noise, new every frame. It
   fuses into the render pass, so it costs no dispatch.

The output is `r` frames behind the input; the render thread renders frame `c` once `c+r` is
decoded and flushes the tail at the end of the shot (missing neighbours are skipped), so the
encoder gets the same frames and timestamps.

### Motion: NVOF (default) or Vulkan block matching

`--temporal-motion nvof|blocks`, `temporal.motion` in the timeline.

- **NVOF** (default): NVIDIA's optical-flow engine through ffcine-stabilize's session
  (`OpticalFlow::with_slots`, a ring of `2r+1` input frames, temporal hints off since the pairs
  aren't consecutive), medium preset, half-resolution 8-bit luma, 4x4 grid (one vector per 8x8
  luma pixels). The hop from Vulkan to CUDA stays on the GPU:
  - a Vulkan pass writes the half-res luma into an `r8` texture, downloaded into a `pl_buf`
    exported as an opaque fd and imported once by CUDA (`cuImportExternalMemory`);
  - per frame, `pl_buf_export` + poll, then a device-to-device `cuMemcpy2D` into the NVOF slot;
  - per neighbour, `nvOFExecute`, and the flow (S10.5 shorts, ~130 KB at 4K) comes back
    through the host and is uploaded as an `rg32f` texture (as in the stabilizer).
  - A Vulkan **confidence** pass then rescores it: for each 8x8 cell, NVOF's vector, its four
    neighbours' vectors and zero, keeping the lowest full-resolution MAD. NVOF regularises flat
    dark areas into arbitrary vectors; without the rescoring it lost to block matching
    everywhere (0038: 0.544 vs 0.406 temporal noise), with it it wins on ground truth.
  - Cost: GPU 4.6 ms/frame at 4K r2 (half of block matching), but the NVOF calls and the poll
    are synchronous on the render thread: 16-25 ms wall/frame at 4K. Making them asynchronous
    (CUDA stream + external semaphores) is the next step.
  - Falls back to block matching (with a log line) when there is no CUDA device or NVOF.
- **Blocks**: Vulkan block matching, one vector per 16x16 block: exhaustive SAD at quarter
  resolution, +-6 quarter pixels around a predictor (the vector to the nearer neighbour on that
  side, scaled by distance) plus +-1 around zero, then a full-resolution refine (2 px then 1 px).
  3.3 ms/frame at 4K r2, nothing outside Vulkan.

| motion | heavy noise PSNR Y/U/V, SSIM Y | mid noise PSNR Y/U/V, SSIM Y | DJI 0037 / 0038 temporal noise (4x4 bins) | 4K r2 GPU ms + NVOF wall ms |
|---|---|---|---|---|
| **nvof** (rescored) | **41.21 / 49.02 / 48.90**, 0.964 | **44.19 / 51.15 / 51.31**, 0.976 | 0.381 / 0.484 | 4.6 + 23-25 |
| blocks | 40.34 / 48.16 / 47.89, 0.965 | 43.37 / 50.25 / 50.31, 0.974 | **0.378 / 0.406** | 7.9 |
| nvof, NVOF vector only | 39.90 / 48.89 / 48.81, 0.953 | 43.55 / 51.14 / 51.31, 0.974 | 0.392 / 0.544 | 4.1 + 16 |

(temporal r2 + NLM, same settings as below.) NVOF wins where there is ground truth, by 0.8-0.9 dB
luma and ~0.9 dB chroma; on 0038 (the stillest, darkest window) block matching flattens the
boiling more. Slow preset, a sqrt contrast stretch before the 8 bits, a +-1 px refine and a
16x16 scoring window were tried and didn't help.

## Results

### Synthetic: known noise, PSNR/SSIM

`bench/denoise/run.sh`: DJI `0004` (daylight, `payday`) 0.25 s, 48 frames, downscaled to 1080p
(area) as the clean reference, plus ffmpeg `noise` (temporal, per pixel) at two levels: heavy
(PSNR 25.7 dB, luma sigma ~13 in 8-bit) and mid. Every output goes through the same ffcine grade
(no LUT, BT.709 in and out, NVENC QP 4) and is compared with the clean clip through the same
grade. White Gaussian noise is a stand-in: real low-light noise is lower-frequency after in-camera
NR and HEVC.

Heavy noise (input: Y 25.7 dB):

| filter | GPU ms/frame (1080p) | PSNR Y / U / V (dB) | SSIM Y / all |
|---|---|---|---|
| none | - | 25.70 / 30.92 / 31.07 | 0.299 / 0.380 |
| ffcine temporal r2, s16 | 1.29 | 33.88 / 39.74 / 39.94 | 0.746 / 0.800 |
| ffcine temporal r3, s16 | 1.59 | 35.02 / 41.26 / 41.43 | 0.803 / 0.847 |
| ffcine NLM only, 12/16, R4 | 1.52 | 38.22 / 47.03 / 46.92 | 0.900 / 0.929 |
| **ffcine temporal r2 s16 + NLM 8/12 R3** | 2.02 | **40.34 / 48.16 / 47.89** | **0.965 / 0.974** |
| ffcine temporal r3 s16 + NLM 6/12 R3 | 2.42 | 40.02 / 48.27 / 47.88 | 0.965 / 0.974 |
| nlmeans_vulkan s=4 p=7 r=15 | | 35.79 / 45.81 / 45.70 | 0.943 / 0.958 |
| nlmeans_vulkan s=4 p=7 r=7 | | 38.61 / 44.35 / 44.44 | 0.943 / 0.955 |
| nlmeans_vulkan s=1 p=7 r=7 (design default) | | 38.72 / 44.33 / 44.43 | 0.943 / 0.955 |

Mid noise (input: Y 31.9 dB):

| filter | GPU ms/frame | PSNR Y / U / V (dB) | SSIM Y / all |
|---|---|---|---|
| none | - | 31.94 / 37.44 / 37.66 | 0.628 / 0.699 |
| ffcine temporal r2, s8 | 1.45 | 39.48 / 45.73 / 46.01 | 0.917 / 0.937 |
| **ffcine temporal r2 s8 + NLM 4/6** | 1.59 | **43.37 / 50.25 / 50.31** | **0.974 / 0.981** |
| ffcine temporal r3 s8 + NLM 3/6 | 2.71 | 43.07 / 50.04 / 50.06 | 0.974 / 0.981 |
| nlmeans_vulkan s=2 r=15 | | 36.35 / 48.58 / 48.38 | 0.949 / 0.963 |
| nlmeans_vulkan s=1 r=7 | | 41.05 / 49.42 / 49.57 | 0.968 / 0.976 |

- Temporal + spatial beats nlmeans_vulkan alone by 1.6-2.3 dB luma and ~3.5 dB chroma, at a
  higher SSIM, and keeps edges and texture that nlmeans flattens (crops).
- Temporal alone at r2 is +8 dB; r3 adds 1.1 dB. White noise at this level needs the spatial
  pass on top: the merge only averages 2r+1 samples.
- r3 over r2 doesn't pay once the NLM runs (the clip moves; more distant frames match less).

The tables below were measured with block matching (the PR #15 version); NVOF's rows are above.

![noisy | ffcine temporal r2 + NLM / nlmeans_vulkan s=4 r=15 | clean](denoise/synthetic-crops.jpg)

*1080p crop at 2x, frame 24: noisy, ffcine (temporal r2 + NLM) / nlmeans_vulkan, clean.*

### Real: DJI Osmo Pocket night clips

`bench/denoise/real.sh`: DJI D-Log M HEVC 10-bit 4K (`0037` at 428 s, the darkest; `0038` at
60 s, the stillest night window), 48 frames, slow walking. No ground truth, so noise is measured
on a dark flat 512x512 patch (top left): `sigma` is the robust high-pass sigma, `temporal` the mean
absolute frame-to-frame difference (what reads as "boiling"), both in 8-bit code values of the log
signal, at full resolution and after 4x4 binning (the blotchy noise HEVC leaves). nlmeans_vulkan
runs with the design's settings (`s=1:p=7:r=7:t=1`; `r=15` runs out of VRAM at 4K on this shared
card).

| clip | filter | sigma (full / 4x4 bins) | temporal (full / 4x4 bins) |
|---|---|---|---|
| 0037 | none | 0.309 / 0.665 | 1.145 / 0.748 |
| 0037 | ffcine temporal r2 s3 | 0.178 / 0.425 | 0.693 / 0.499 |
| 0037 | **ffcine temporal r2 s3 + NLM 1.5/2.5** | 0.090 / 0.250 | **0.432 / 0.377** |
| 0037 | ffcine temporal r3 s3 + NLM 1.5/2.5 | 0.083 / 0.223 | 0.383 / 0.340 |
| 0037 | ffcine NLM only 2/3 R3 | 0.098 / 0.250 | 0.507 / 0.455 |
| 0037 | nlmeans_vulkan s=1 r=7 | 0.083 / 0.254 | 0.545 / 0.484 |
| 0038 | none | 0.368 / 0.746 | 1.540 / 1.047 |
| 0038 | ffcine temporal r2 s3 | 0.188 / 0.335 | 0.698 / 0.522 |
| 0038 | **ffcine temporal r2 s3 + NLM 1.5/2.5** | 0.083 / 0.195 | **0.458 / 0.405** |
| 0038 | ffcine temporal r3 s3 + NLM 1.5/2.5 | 0.083 / 0.174 | 0.392 / 0.347 |
| 0038 | ffcine NLM only 2/3 R3 | 0.112 / 0.295 | 0.761 / 0.684 |
| 0038 | nlmeans_vulkan s=1 r=7 | 0.085 / 0.314 | 0.777 / 0.697 |

- Spatially, nlmeans and ffcine's temporal + NLM end up as flat as each other; frame to frame,
  ffcine cuts the "boiling" to 0.38-0.41 (from 0.75-1.05, 4x4 bins) where nlmeans leaves
  0.48-0.70, and it does so without the smear.
- These DJI clips are already smoothed in camera, so the absolute numbers are small in log code
  values; the night look's gamma and LUT multiply them several times.

![ffcine none / temporal r2 + NLM / nlmeans_vulkan](denoise/dji-night-crops.jpg)

*`0037` frame 24, 480x270 crop at 2x, lifted (gamma 1.7, contrast 1.4) to show the noise: no
denoise, ffcine temporal r2 s3 + NLM 1.5, nlmeans_vulkan s=1 r=7. nlmeans smears the leaves;
the temporal pass keeps them.*

No Canon R10 low-light footage was available: the only R10 clip (8-bit H.264, daylight) has
nothing to denoise. Re-run `real.sh` on R10 night clips when there are some.

### Speed

GPU time per frame from libplacebo timers (shared GPU, so upper bounds):

| | 1080p | 4K (DJI 0037) |
|---|---|---|
| ffcine temporal r2 + NLM | 1.6-2.0 ms | 7.9 ms (motion 3.3, merge 2.2, NLM 2.2, copies 0.2) |
| ffcine temporal r3 + NLM | 2.4-2.7 ms | ~11 ms |
| ffcine NLM only | 1.5 ms | 3.5 ms |
| nlmeans_vulkan s=1 p=7 r=7 t=1 | | ~56 ms (one frame uploaded once, looped 96x: 8.1 s vs 2.75 s without the filter) |

Whole `ffcine grade` at 4K (Vulkan decode, grade, NVENC), 48 frames, steady-state fps: 48 without
denoise, 39-47 with temporal r2 + NLM, 36 with r3, while other jobs used the GPU. At ~8 ms the
denoiser alone would run at ~125 fps at 4K, so a denoised render stays decode/encode bound; the
same nlmeans_vulkan pass caps a 4K render near 18 fps.

## Left to do

- `ffcine render` applies `ffcine.fx.denoise/1` and the grade's `denoise` number
  ([assembly.md](assembly.md#per-clip-filters)); `ffcine.fx.grain/1` isn't wired into it yet.
- NVOF asynchronous (CUDA stream, Vulkan/CUDA semaphores instead of the poll) so its wall time
  overlaps the render.
- Scene cuts within a shot are handled by the merge's rejection; a ring reset on detected cuts
  would save the work.
- Dolby Vision metadata is carried over per frame; ICC profiles and AV1 film-grain side data
  are dropped on the denoised path.
