# Bench: render.py's chain as one ffmpeg GPU pass

Question: how fast does plain ffmpeg get if render.py's per-shot chain becomes one GPU pass, and
what does that mean for roadmap step 1 (the grade path)? Run on 2026-10-31, RTX 5060 Ti, Ryzen 9
9950X (32 threads), shared machine. Scripts and raw numbers: `bench/` (`results/runs.csv`,
`results/quality.csv`).

## Method

- **ffmpeg**: the prototype dev shell's `ffmpeg-full-gpu` 9.0.1 (libplacebo git, API 375, built with
  glslang). It is the only build here with `nlmeans_vulkan`: the system ffmpeg 9.0.1 and the ffcine
  dev shell's 9.0.2 have `libplacebo`, `vidstab*`, `hevc_nvenc` and `-hwaccel vulkan|cuda`, but no
  `*_vulkan` filters (no SPIR-V compiler at build time). Nothing was built from source.
- **Reference**: render.py itself. `bench/proto_cmd.py` imports a copy of render.py with its cache
  redirected to `/tmp/ffcine-bench` and prints the exact per-shot commands. It covers the CPU chain
  (`output.gpu: false`, what edit.yaml renders today), the `--gpu` chain, and the vidstab detect
  pass. The cached final renders in the project are stale (params hashes no longer match), so every
  reference was re-rendered into `/tmp` the same way. Final mode: 3840x2160, `hevc_nvenc -preset p6
  -tune hq -rc constqp -qp 12 main10 -g 24`, the same flags in every run.
- **Clips** (edit `in`/`out`, render.py's frame counts):

  | clip | source | why |
  |---|---|---|
  | `payday` | `dji:0004` 0.25-3.15 s, 70 f, look `day` + gamma 1.2 | plain DJI Osmo Pocket 4 D-Log M 10-bit HEVC, the most common shot type |
  | `nightpath` | `dji:0037` 318.0-321.2 s, 82 f, look `night`, gamma 1.3, denoise 5 | the strongest low-light shot (heaviest denoise in edit.yaml) |
  | `r10` | `photos/MVI_2884.MP4` 3.0-7.0 s, 96 f, default look | **no edit*.yaml uses an R10 clip**, so this is the only R10 video in the sample set (4K
 25p H.264 8-bit, full range) added as an extra shot. render.py treats it as non-log `video`: light look, no camera LUT, 25 -> 23.976 fps |
  | `verdict` | `dji:0048` 2.6-10.9 s, 199 f, `stabilize: {smoothing: 22}`, punch at 6.4 s | the edit's stabilized shot, for vidstab |

- **One-pass graph** (`bench/onepass.py`): NVDEC -> CUDA -> `hwupload` to Vulkan -> `fps` ->
  [`nlmeans_vulkan s=0.2d:r=7:t=1`, p at its default 7] -> **one `libplacebo`** -> `hwupload` to CUDA ->
  `hevc_nvenc`. That libplacebo does crop/punch (render.py's own `gpu_geo` expressions), EWA scaling
  (`ewa_lanczossharp`/`ewa_lanczos`), the grade baked to a 65^3 LUT by `gpu_grade.bake`
  (`lut_type=normalized`), `tonemapping=bt.2390:gamut_mode=perceptual`, `deband=1`, the hooks and
  `dithering=blue`, out to Rec.709 tv p010. The hooks are `bench/shaders/unsharp.hook` (PREOUTPUT,
  `gpu_shaders.unsharp` at amount 0.30, so it has a cost; these shots have `sharpen: 0`) and
  `grain.hook` (OUTPUT, a port of the assembly `noise`, see below), concatenated into one file.
  - `full`: everything above. Used for timing and the side-by-sides.
  - `compat`: no deband, no hooks, render.py's lanczos scaler options. Used to check fidelity.
- **Measurement** (`bench/run.sh`): every run is at `nice -n 19`, one at a time, wrapped in GNU `time
  -v`, `nvidia-smi dmon -s um` (whole GPU) and `pmon -s u` (per process, to split our use from other
  processes'). Loadavg is recorded before and after, plus compute apps and 3 s of GPU idle before each
  run. `bench/startup.sh` reruns each command with `-frames:v 1` to separate the fixed start-up
  cost. Steady fps = (n-1) / (wall_n - wall_1).
- **Quality** (`bench/quality.sh`): SSIM and PSNR (ffmpeg `ssim`/`psnr`, yuv420p10le, same frames and
  size) against render.py's CPU render. A mid-clip side-by-side (render.py | one-pass full) was
  checked by eye for each clip.


### Command lines

Paths shortened. DJI, full (payday; nightpath is the same plus `nlmeans_vulkan=s=1.00:r=7:t=1` before
libplacebo):

```sh
ffmpeg -init_hw_device cuda=cu:0 -init_hw_device vulkan=vk@cu -filter_hw_device vk \
  -hwaccel cuda -hwaccel_device cu -hwaccel_output_format cuda -ss 0.25 -i DJI_..._0004_D.MP4 \
  -filter_complex "[0:v]setpts=PTS-STARTPTS,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv,\
hwupload=derive_device=vulkan,fps=24000/1001,\
libplacebo=w=3840:h=2160:crop_w='...':crop_h='...':crop_x='...':crop_y='...':\
upscaler=ewa_lanczossharp:downscaler=ewa_lanczos:lut='grade_9f04d9a751102f0e.cube':lut_type=normalized:\
tonemapping=bt.2390:gamut_mode=perceptual:dithering=blue:deband=1:custom_shader_path='onepass.hook':\
colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=p010le,\
hwupload=derive_device=cuda,tpad=stop_mode=clone:stop=-1,setsar=1,settb=1001/24000,setpts=N[v]" \
  -map "[v]" -frames:v 70 -an -c:v hevc_nvenc -preset p6 -tune hq -rc constqp -qp 12 -profile:v main10 -g 24 \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv payday_onepass_full.mkv
```

Stabilized (verdict). The graph has to split around vidstab:

```sh
... -filter_complex "[0:v]setpts=PTS-STARTPTS,setparams=...,hwupload=derive_device=vulkan,fps=24000/1001,\
libplacebo=lut='grade_d200d83e8d3a468a.cube':lut_type=normalized:deband=1:range=pc:format=gbrp16le:...,\
hwdownload,format=gbrp16le,format=yuv444p,\
vidstabtransform=input='verdict.trf':smoothing=22:optzoom=1:zoomspeed=0.25:interpol=bicubic:tripod=0:crop=black,\
format=yuv444p16le,hwupload=derive_device=vulkan,\
libplacebo=w=3840:h=2160:crop_*='<punch at n>=153>':upscaler=ewa_lanczossharp:...:custom_shader_path='onepass.hook':range=tv:format=p010le,\
hwupload=derive_device=cuda,tpad=...,setpts=N[v]" ...
```

The exact argv of every run is in `/tmp/ffcine-bench/runs/<label>/cmd.txt`. The render.py reference
commands are `bench/proto_cmd.py`'s output.

## Results

Wall time includes process start, device init, shader compile and seek (start-up column, from
1-frame runs). "Ours" GPU % is the encode process's own share from `pmon`.

| clip | pipeline | frames | wall s | fps | steady fps | start-up s | speedup vs render.py | CPU % | GPU SM/NVENC % (ours) | load1 before/after |
|---|---|---|---|---|---|---|---|---|---|---|
| payday | render.py (CPU) | 70 | 5.40 | 13.0 | 16 | 1.1 | 1.00x | 1072 | 2/4 | 17.0/17.2 |
| payday | render.py --gpu | 70 | 2.28 | 30.7 | 70 | 1.3 | 2.37x | 50 | 7/39 | 17.2/17.1 |
| payday | **one-pass full** | 70 | 2.50 | 28.0 | 65 | 1.4 | **2.16x** | 53 | 12/32 | 17.1/16.0 |
| payday | one-pass compat | 70 | 2.22 | 31.5 | | | 2.43x | 48 | 8/11 | 16.0/15.8 |
| nightpath | render.py (CPU, hqdn3d) | 82 | 13.49 | 6.1 | 7 | 1.6 | 1.00x | 407 | 0/1 | 15.8/14.8 |
| nightpath | render.py --gpu (nlmeans p3 r5) | 82 | 3.27 | 25.1 | 43 | 1.4 | 4.13x | 38 | 53/39 | 14.8/16.9 |
| nightpath | **one-pass full** (nlmeans p7 r7) | 82 | 4.70 | 17.5 | 28 | 1.8 | **2.87x** | 32 | 67/22 | 16.9/19.6 |
| nightpath | one-pass compat | 82 | 5.05 | 16.2 | | | 2.67x | 27 | 71/22 | 19.6/24.2 |
| r10 | render.py (CPU) | 96 | 5.76 | 16.7 | 19 | 0.9 | 1.00x | 999 | 4/10 | 24.2/32.6 |
| r10 | render.py --gpu (CPU decode) | 96 | 3.11 | 30.9 | | 3.3 | 1.85x | 450 | 11/33 | 32.6/31.6 |
| r10 | **one-pass full** | 96 | 2.96 | 32.4 | 67 | 1.5 | **1.95x** | 45 | 9/30 | 31.6/29.8 |
| r10 | one-pass compat | 96 | 2.83 | 33.9 | | | 2.04x | 41 | 5/37 | 29.8/28.7 |

Steady state, the one-pass graph runs at 65-67 fps on 4K DJI and R10 (about 4x render.py's CPU chain)
and 28 fps with denoise. On real 3-4 s shots, though, 1.4-1.8 s of start-up is 40-60% of the wall
time, so a shot only gets 2-3x faster end to end. It also isn't faster than render.py's own `--gpu`
path, which is already nearly one pass for DJI shots. Deband, two hooks and EWA add about 8% (full vs
compat, 2.50 vs 2.22 s). The GPU is mostly idle: our SM use is 8-12% and NVENC 30-40% without
denoise. The graph is serial (one filter thread, a sync at each CUDA<->Vulkan hop), not GPU-bound.
nlmeans at the design's `r=7` with the default `p=7` is the one heavy kernel: SM 67%, and 35% slower
than the prototype's `p=3:r=5`.

**Load context.** Load1 sat at 15-40 on 32 threads throughout. Four other CUDA processes (Python,
about 8.4 GB VRAM) were resident but at 0% SM. The 3 s idle samples were 0% SM/NVENC/NVDEC before
every run except the `verdict` one-pass runs and the detect pass. Those overlapped an Immich
(jellyfin-ffmpeg) transcode using 65-77% NVDEC, 15-20% SM and 20% NVENC, so their numbers are
pessimistic.

### Quality vs render.py's CPU render

| clip | candidate | SSIM Y | SSIM all | PSNR Y dB | PSNR avg dB |
|---|---|---|---|---|---|
| payday | render.py --gpu | 0.9973 | 0.9975 | 54.42 | 54.49 |
| payday | one-pass compat | 0.9973 | 0.9975 | 54.41 | 54.49 |
| payday | one-pass full | 0.9860 | 0.9882 | 47.85 | 48.48 |
| nightpath | render.py --gpu | 0.9843 | 0.9852 | 42.70 | 43.35 |
| nightpath | one-pass compat | 0.9837 | 0.9851 | 42.57 | 43.29 |
| nightpath | one-pass full | 0.9772 | 0.9807 | 41.97 | 42.88 |
| r10 | render.py --gpu | 0.9965 | 0.9970 | 50.92 | 52.01 |
| r10 | one-pass compat | 0.9965 | 0.9971 | 51.73 | 52.67 |
| r10 | one-pass full | 0.9879 | 0.9911 | 45.04 | 46.54 |
| verdict | render.py --gpu (stabilized) | 0.9962 | 0.9964 | 49.99 | 50.80 |
| verdict | one-pass stab | 0.9856 | 0.9858 | 46.31 | 46.54 |

- `compat` matches render.py's GPU path to the third decimal on every clip (compat vs `--gpu`
  directly: 52-55 dB). The one-pass graph is the prototype's GPU picture, and both are within LUT
  interpolation of the CPU chain (54 dB on payday).
- `full` drops to 0.986 / 48 dB on purpose: deband, sharpen 0.30 and per-shot grain (the reference has
  no grain; render.py adds it at assembly).
- nightpath is about 0.984 for every GPU variant. That gap is the denoiser (CPU `hqdn3d` is temporal,
  `nlmeans_vulkan` is spatial only), not the pipeline.
- Side-by-sides: no visible grade or geometry shift on any clip. The night clip is slightly smoother
  and less smeared on motion with nlmeans.

## Vidstab cost (verdict, 199 frames)

| run | wall s | fps | CPU % |
|---|---|---|---|
| render.py vidstabdetect (CPU decode, shakiness 6, accuracy 15) | 17.7 | 11.2 | 745 |
| one-pass, no stabilization | 5.1 | 39.2 | 66 |
| one-pass with vidstabtransform spliced in | 76.8 | 2.6 | 113 |
| render.py --gpu (stabilized: CPU decode, GPU grade, CPU transform/crop) | 64.9 | 3.1 | 164 |
| render.py CPU (stabilized) | 83.5 | 2.4 | 273 |

Stabilizing a 4K shot costs 17.7 s to detect plus about 72 s to transform, against 5 s for the whole
GPU shot: about 18x. `vidstabtransform` at 4K runs effectively single-threaded (113% CPU) and sets
the pace for the whole graph. GPU decode/grade/encode buys nothing once it is in the chain.

## What didn't fit in one pass

- **vidstab**: two passes (detect writes a `.trf`), CPU only, 8-bit `yuv444p`. It forces
  `hwdownload` -> CPU -> `hwupload`, and since render.py stabilizes before the punch crop, a
  **second libplacebo pass** after it. Gotcha: an uploaded `gbrp16le` Vulkan frame is not sampleable,
  so libplacebo fails validation and renders black. Upload `yuv444p16le` instead (that is what
  `bench/onepass.py` does).
- **`-hwaccel vulkan` can't feed NVENC**: Vulkan-decoded frames are multiplane images, which can't be
  mapped to CUDA ("create the Vulkan device with disable_multiplane=1"). Setting that option on the
  device doesn't help, because the decoder's frames stay multiplane. So the graph is NVDEC -> CUDA ->
  `hwupload` (device copy) -> Vulkan -> libplacebo -> `hwupload` (device copy) -> CUDA -> NVENC:
  **two GPU-side copies per frame**, and the same as render.py's `--gpu` path. A Vulkan encoder
  (`hevc_vulkan`) or importing NVDEC surfaces would avoid them. (ffcine can feed NVENC from Vulkan
  decode, because it renders into its own one-image-per-plane frames: see
  [ffcine grade](#ffcine-grade-step-1) below.)
- **Grade**: everything pointwise fits as one baked LUT, but the bake still runs the CPU ffmpeg chain
  once (cached). libplacebo takes one `lut`. Apple Log's input LUT would have to be baked into the
  grade LUT or run as a CPU `lut3d` (render.py does the latter).
- **Grain**: render.py adds it at assembly, after captions/cards. Here it is a per-shot OUTPUT hook,
  so titles stay clean and the look changes slightly. Measured on this ffmpeg: `vf_noise` only takes
  8-bit formats. So the "16-bit" assembly `noise` converts the master to 8-bit `yuv444p` (banding
  risk), luma noise is +-1 code value, and the chroma strengths (`1.20`) truncate to int 1 and add
  nothing. The hook reproduces the luma std (0.0032) without the 8-bit step.
- **Hook parameters**: libplacebo takes only one `custom_shader_path`, so all hooks go in one
  concatenated file. `//!PARAM` blocks do work, but the parser treats any `//!` it finds, even
  inside a comment, as a directive. ffmpeg has no way to set a PARAM per run, so the amount is a
  `#define` per file here; ffcine sets hook params through the API.
- **Temporal denoise**: no GPU temporal denoiser in ffmpeg. `nlmeans_vulkan` is spatial.
- **Per-shot CPU tails** stay outside the pass in render.py: freeze/still holds, fades, vignette,
  blur/darken, tmix (`blend_vulkan` chains are extra passes), `fit: contain` blur-fill, layouts,
  dissolves, captions and cards (assembly).
- **Audio**: `-an`. The per-clip clean chain and the mix are a separate graph.
- **No real scaling in these runs**: 4K to 4K, so EWA only runs on punch-ins (`verdict` after 6.4 s).

## MLT (melt)

MLT is the engine behind Kdenlive and Shotcut, and the open-source timeline renderer ffcine has to
beat. Same clips, frames, reference (render.py's CPU render) and `bench/run.sh` measurement as
above. Scripts: `bench/mlt/` (`gen.py` writes the MLT XML, `bench.sh` runs it, `quality.sh`,
`report.py`, `movit-egl.c`); raw numbers in `bench/results/mlt_runs.csv`.

### Method

- **MLT**: nixpkgs `mlt` 7.42.0 (its own ffmpeg 8.1.2, Movit 1.7.2), the `melt` CLI. Nothing built
  except the small Movit driver below.
- **Grade**: MLT has no grading of its own that can express render.py's look, so every variant
  applies the same baked 65^3 `.cube` that render.py's GPU path uses (`gpu_grade.bake`) with
  `avfilter.lut3d` (`interp=tetrahedral`), which is also what Shotcut's and Kdenlive's "3D LUT"
  filters are. nightpath adds render.py's CPU denoise, `avfilter.hqdn3d=5:3.75:7.5:6`, before it.
  There is no scaling (4K to 4K, as above) and no sharpen (these shots have `sharpen: 0`).
- **Bit depth**: for a `p010le` encoder melt's avformat consumer processes in 8-bit `yuv422` unless
  told otherwise. All runs set `mlt_image_format=rgba64`, so decode, LUT and encode conversion run
  at 16 bits.
- **Encoder**: avformat consumer `vcodec=hevc_nvenc preset=p6 tune=hq rc=constqp qp=12
  vprofile=main10 g=24 pix_fmt=p010le`, Rec.709 tv tags: render.py's flags.
- **Threads**: `real_time=-8` (8 frame-parallel image threads, Shotcut's "parallel processing").
  Movit runs need `real_time=-1`: one GL context, one thread.
- **Variants**:
  - `plain`: no filters, so decode -> rgba64 -> encode. MLT's own floor.
  - `cpu`: [hqdn3d] + lut3d. What a Kdenlive/Shotcut CPU render of this grade does.
  - `grain`: `cpu` + render.py's assembly grain as a clip filter (`avfilter.noise`, same strengths).
  - `nvdec`: `cpu` with `MLT_AVFORMAT_HWACCEL=cuda` (NVDEC; frames are downloaded to RAM).
  - `movit`: `nvdec` plus a Movit stage (`movit.lift_gamma_gain` at identity), through
    `movit-egl`. Movit can't run the grade itself (below), so this measures what MLT's GPU mode
    costs with the grade still on the CPU.
- **Runs**: one at a time at `nice -n 19`, each source read into the page cache first (on a cold
  cache the first MLT run of a clip took 2-13x longer; r10: 290 s cold, 22 s warm). Repeats and
  1-frame start-up runs were made, but other sessions' builds pushed load1 to 100-310 during them
  (2-5x slower walls), so `report.py` drops runs that started above load 80 and the table has one
  clean run per variant. Even at load 30-70, repeats of the same variant varied up to 2x, so read
  the walls as +-30%.

Command line (payday `cpu`; `bench/mlt/gen.py` prints every argv):

```sh
melt -silent -profile uhd_2160p_2398 payday_mlt_cpu.mlt \
  -consumer avformat:payday_mlt_cpu.mkv f=matroska vcodec=hevc_nvenc preset=p6 tune=hq rc=constqp qp=12 \
  vprofile=main10 g=24 pix_fmt=p010le mlt_image_format=rgba64 an=1 \
  color_primaries=bt709 color_trc=bt709 colorspace=709 color_range=tv real_time=-8
```

```xml
<mlt>
<producer id="shot">
  <property name="resource">DJI_20260704113124_0004_D.MP4</property>
  <property name="mlt_service">avformat</property>
  <filter><property name="mlt_service">avfilter.lut3d</property>
    <property name="av.file">grade_9f04d9a751102f0e.cube</property>
    <property name="av.interp">tetrahedral</property></filter>
  <!-- nightpath: avfilter.hqdn3d av.luma_spatial=5 av.chroma_spatial=3.75 av.luma_tmp=7.5 av.chroma_tmp=6 first -->
  <!-- movit: + <filter><property name="mlt_service">movit.lift_gamma_gain</property></filter> -->
</producer>
<tractor><multitrack><playlist><entry producer="shot" in="6" out="75"/></playlist></multitrack></tractor>
</mlt>
```

Movit: `MLT_AVFORMAT_HWACCEL=cuda movit-egl uhd_2160p_2398 payday_mlt_movit.mlt out.mkv <same consumer
properties> real_time=-1`.

Gotcha: put the in/out on the playlist `<entry>` only. With `in=` on the `<producer>` as well, MLT
applies the in point twice and renders a different part of the clip.

### Results

Rows to read next to the ffmpeg table above. Speedup is against the render.py CPU row there
(5.40 / 13.49 / 5.76 s). Those ran at load 15-40 and these at 28-69, so MLT is penalised somewhat by
load. GPU % is melt's own share (`pmon`) as SM/NVENC/NVDEC.

| clip | pipeline | frames | wall s | fps | speedup vs render.py | CPU % | GPU SM/NVENC/NVDEC % (ours) | load1 before/after |
|---|---|---|---|---|---|---|---|---|
| payday | melt, no filters | 70 | 8.52 | 8.2 | 0.63x | 198 | 1/2/0 | 32/33 |
| payday | melt CPU (lut3d) | 70 | 8.83 | 7.9 | 0.61x | 522 | 1/1/0 | 33/30 |
| payday | melt CPU + grain | 70 | 10.30 | 6.8 | 0.52x | 762 | 1/2/0 | 30/28 |
| payday | melt, NVDEC | 70 | 8.16 | 8.6 | 0.66x | 471 | 2/4/0 | 28/29 |
| payday | melt + Movit (EGL), NVDEC | 70 | 7.10 | 9.9 | 0.76x | 293 | 11/6/0 | 29/37 |
| nightpath | melt, no filters | 82 | 9.66 | 8.5 | (1.40x, no denoise) | 214 | 1/2/0 | 47/55 |
| nightpath | melt CPU (hqdn3d + lut3d) | 82 | 21.58 | 3.8 | 0.63x | 384 | 0/2/0 | 62/69 |
| nightpath | melt CPU + grain | 82 | 16.56 | 5.0 | 0.81x | 642 | 0/1/0 | 69/59 |
| nightpath | melt, NVDEC | 82 | 10.13 | 8.1 | 1.33x | 681 | 2/2/0 | 59/53 |
| nightpath | melt + Movit (EGL), NVDEC | 82 | 10.32 | 8.0 | 1.31x | 319 | 9/5/0 | 53/45 |
| r10 | melt, no filters | 96 | 8.00 | 12.0 | 0.72x | 186 | 1/9/0 | 45/40 |
| r10 | melt CPU (lut3d) | 96 | 9.27 | 10.4 | 0.62x | 576 | 1/5/0 | 40/36 |
| r10 | melt CPU + grain | 96 | 15.05 | 6.4 | 0.38x | 588 | 0/2/0 | 36/35 |
| r10 | melt, NVDEC | 96 | 8.46 | 11.3 | 0.68x | 567 | 2/4/1 | 35/33 |
| r10 | melt + Movit (EGL), NVDEC | 96 | 7.00 | 13.7 | 0.82x | 354 | 14/10/6 | 33/32 |

Start-up (1-frame runs, load 60-100): about 2 s for `plain`, `nvdec` and `movit` (1.8-2.5 s), 2-5 s
when hqdn3d or noise is in the chain. nightpath's `cpu` row (21.6 s) ran at the highest load of the
batch and its `nvdec` twin, the same chain with NVDEC, took 10.1 s; treat about 10-15 s as melt's
nightpath figure.

Quality vs render.py's CPU render (frames paired by index):

| clip | pipeline | SSIM Y | SSIM all | PSNR Y dB | PSNR avg dB |
|---|---|---|---|---|---|
| payday | melt CPU | 0.9973 | 0.9975 | 54.82 | 55.23 |
| payday | melt CPU + grain | 0.9963 | 0.9968 | 53.52 | 54.13 |
| payday | melt + Movit | 0.9965 | 0.9969 | 52.23 | 53.02 |
| nightpath | melt CPU | 0.9974 | 0.9973 | 52.57 | 52.98 |
| nightpath | melt CPU + grain | 0.9970 | 0.9970 | 52.10 | 52.53 |
| nightpath | melt + Movit | 0.9972 | 0.9968 | 51.79 | 52.02 |
| r10 | melt CPU | 0.9742 | 0.9812 | 29.83 | 31.54 |
| r10 | melt + Movit | 0.9734 | 0.9806 | 29.80 | 31.52 |

`nvdec` matches `cpu` to the fourth decimal. On DJI, melt's picture equals render.py's (0.997 SSIM,
55 dB, the same as one-pass compat). r10 is not a grade difference: it is 25p conformed to 23.976,
and melt picks a different source frame than render.py's `fps` filter for 6 of the 96 output frames
(n = 11, 12, 35, 58, 59, 82; SSIM about 0.73 each). The other 90 frames are 0.994-0.999. Movit costs
about 2 dB on every clip; that is the 8-bit readback described below, not Movit's maths.

Side-by-sides (render.py | melt CPU | melt + Movit, mid-shot) were posted to the project thread
(`bench/mlt/quality.sh` writes them to `$BENCH_TMP/sbs/<clip>_mlt.png`); they are indistinguishable
at 1:1.

### Movit, MLT's GPU mode

- **API**: OpenGL 3.2 core + GLSL (Movit 1.7.2), not Vulkan, no CUDA interop. It can't share images
  with NVDEC, NVENC or libplacebo without a copy through RAM or a GL interop layer MLT doesn't have.
- **Headless**: not out of the box. melt only creates a GL context in the Qt `qglsl` consumer
  (needs X11 or Wayland; `QT_QPA_PLATFORM=offscreen` gives Movit "OpenGL version 0.0", `minimalegl`
  dumps core) or the deprecated X11 `xgl` display consumer. Shotcut/Kdenlive render through
  `qglsl` on the desktop's GL. For this bench, `bench/mlt/movit-egl.c` (120 lines) creates an
  EGL device-platform context on the 5060 Ti (`EGL_PLATFORM_DEVICE_EXT`, NVIDIA's EGL vendor only),
  fires MLT's `init glsl` in the consumer's render thread and drives the avformat consumer. That
  works headless and reliably, but it is our code, not something a user gets from melt.
- **One thread**: Movit needs one GL context per render thread, and MLT's GLSL mode runs with
  `real_time=-1`; the CPU variants use 8 frame threads.
- **Frames round-trip through RAM, twice.** With `MLT_LOG_DEBUG` the chain is: NVDEC decode, download
  to RAM (`yuv420p10`), upload to GL, `glsl -> rgba` readback because `avfilter.lut3d` is a CPU
  filter, re-upload, the Movit effect, `glsl -> rgba64` readback (glReadPixels via PBO), swscale to
  p010 on the CPU, upload to NVENC. At 4K that is six full-frame host<->device transfers per frame.
  The GPU sits at 9-14% SM and 5-10% NVENC; melt is CPU- and copy-bound.
- **10-bit**: MLT's GLSL path is float/16-bit internally and the final readback is `rgba64`, but the
  hop out to any CPU filter is **8-bit `rgba`**, so in Movit mode the LUT runs on 8-bit input. That
  is the 2 dB loss above and a banding risk on D-Log M skies. The CPU path is 16-bit only with
  `mlt_image_format=rgba64`; MLT's default for a p010 encode is 8-bit `yuv422`.
- **LUT**: Movit 1.7 has no 3D LUT effect, so the grade (all of render.py's look) can't run on the
  GPU in MLT at all. `avfilter.lut3d` (what Shotcut's and Kdenlive's "3D LUT" filters use) works and
  matches render.py exactly at 16 bits.
- **D-Log M / HDR**: nothing log-aware. D-Log M is only handled by the baked `.cube`; MLT has no
  colour management beyond 601/709/2020 matrices and no PQ/HLG handling on either path. With the
  LUT applied the result matches render.py, because the LUT does all the work.
- **What it can't do on our footage**: no GPU LUT, no GPU denoise (`hqdn3d`/`nlmeans` are CPU
  avfilters; render.py's `nlmeans` would be far slower than hqdn3d here), no deband, no GPU
  stabilization (MLT's vidstab is CPU), and grain via `avfilter.noise` is 8-bit only. Movit does
  have blur, deconvolution sharpen, lift/gamma/gain, white balance, saturation, glow, vignette and
  resample/resize, so a punch-in scale and a sharpen are the only render.py steps it could take on.
  In this bench Movit runs an identity `lift_gamma_gain`, so the `movit` rows are what turning on
  GPU mode costs with the grade still on the CPU. It came out the fastest MLT variant on every clip
  (fewer CPU threads fighting over a busy machine,
 and decode on NVDEC), not because the GPU did work.

### What ffcine has to beat

melt is the baseline a Kdenlive/Shotcut user gets: **7-10 s for a 3-4 s 4K shot with the LUT
(8-14 fps), 10-22 s with denoise or grain**, using 3-6 CPU cores and almost no GPU. Its best case
(Movit + NVDEC) is 7.0-10.3 s, about 0.8x render.py's CPU chain on DJI/R10 and 1.3x on nightpath.
Against that, render.py's CPU chain is 5.4-13.5 s, its `--gpu` path 2.3-3.3 s and the one-pass
ffmpeg graph 2.2-5.1 s at the same picture (0.997 SSIM). So melt is 2.5-3.5x slower than plain
ffmpeg on the GPU and no better than the prototype it would replace; ffcine's step-1 target (one
process, frames kept in VRAM, ~65 fps steady) is 5-8x melt. MLT's ~2 s start-up per render is
comparable to ffmpeg's 1.1-1.8 s per shot, but melt renders a whole timeline in one process, so
ffcine needs the shared device and shader cache (implication 1 below) to keep that edge.


## Implications for ffcine step 1

1. **Kill per-shot process start-up.** It is 1.1-1.8 s per shot (CUDA + Vulkan device init, libplacebo
   shader compile, LUT upload, seek), half the wall time of a typical 3 s shot. ffcine should keep
   one Vulkan device, one `pl_renderer` and a `pl_cache` (shader cache on disk) across all shots of
   a render. On its own that brings shots from about 30 fps to about 65 fps.
2. **Pipeline frames; don't chase kernels.** The one-pass graph uses ~10% SM and ~35% NVENC at 4K.
   ffcine should keep several frames in flight (decode N+1 while rendering N and encoding N-1, with
   semaphores rather than per-frame waits) and run a few shots concurrently in one process under
   the VRAM budget, instead of N ffmpeg processes.
3. **Cut the CUDA<->Vulkan copies.** Both pipelines pay two device copies per frame. Import NVDEC
   output into Vulkan without a copy, or decode with Vulkan video into non-multiplane/exportable
   images, and give NVENC an exported Vulkan image. The fallback is Vulkan encode.
4. **Match first with `compat` settings.** The plain ffmpeg graph already equals render.py's GPU
   picture (0.997 SSIM, 54 dB on DJI), so step 1's "within LUT rounding" acceptance test is
   realistic. Keep deband/EWA/hooks/grain behind switches, as planned: together they cost about 8%.
5. **Denoise defaults need a cost check.** `r=7` with nlmeans' default `p=7` is 35% slower than the
   prototype's `p=3:r=5` and was SM-bound here. Pick `p` explicitly (3 looked the same to the
   prototype) before making `r=15` the low-light final default.
6. **Stabilization is the real bottleneck,** not the grade: a stabilized shot is about 18x a plain
   one, all of it CPU vidstab. Caching transforms by source range (as planned) and the NVOFA/warp
   stabilizer are worth more than any further grade-path tuning. Until then, ffcine should run the
   vidstab shot's CPU stage concurrently with other shots' GPU work, not in series.
7. **Fix grain while porting.** Doing it per shot in RGB at 10+ bits avoids the prototype's
   accidental 8-bit master conversion, and its chroma grain was a no-op, so retune `g` rather than
   copy it.

## ffcine grade (step 1)

`ffcine grade` / `ffcine batch` on payday and r10 at the `compat` settings: same LUTs, crop, size,
lanczos, `hevc_nvenc -preset p6 -tune hq -rc constqp -qp 12 main10 -g 24`. Script:
`bench/ffcine.sh` (wraps `run.sh`, so wall time includes process start), run on 2026-10-31.

Pipeline: one Vulkan device (CUDA derived from it for NVENC), one `pl_renderer` and an on-disk
`pl_cache`, kept across shots. Three threads with bounded queues keep frames in flight: decode
N+1, render N, encode N-1. Ordering is by the `AVVkFrame` semaphores, so there are no per-frame
waits. Decode is Vulkan video by default; its frames go straight into libplacebo with no copy. The
one remaining copy per frame is the Vulkan -> CUDA hop into NVENC.

### Quality vs render.py's CPU render

| clip | candidate | SSIM Y | SSIM all | PSNR Y dB | PSNR avg dB |
|---|---|---|---|---|---|
| payday | ffcine (Vulkan decode, NVENC) | 0.9973 | 0.9975 | 54.41 | 54.49 |
| payday | ffcine `--decode cuda` | 0.9973 | 0.9975 | 54.41 | 54.49 |
| payday | ffcine `--codec hevc_vulkan` | 0.9956 | 0.9960 | 52.56 | 52.45 |
| r10 | ffcine (Vulkan decode, NVENC) | 0.9965 | 0.9971 | 51.73 | 52.68 |
| r10 | ffcine `--decode cuda` | 0.9965 | 0.9971 | 51.73 | 52.68 |
| r10 | ffcine `--codec hevc_vulkan` | 0.9944 | 0.9954 | 49.65 | 50.45 |

- The default path meets the target (0.997 / 54 dB on payday). It matches the one-pass compat
  graph and render.py `--gpu` to the fourth decimal on both clips.
- Vulkan and CUDA decode give the same output; their decoded frames are bit-identical (framemd5).
  `ffcine batch` output equals the single-shot output.
- `hevc_vulkan` loses 1.8-2 dB at the same QP and makes files about 7% larger. The cause is the
  encoder, not the render: the same Vulkan render sent to NVENC gives 0.997257 / 54.41.

### Wall time

Load1 was 34-51 on 32 threads, above the 15-40 of the table above. So the reference rows were
rerun twice, interleaved with ffcine, under the same load. The table gives both rounds.

| shots | pipeline | wall s (round 1 / 2) | fps |
|---|---|---|---|
| payday (70 f) | render.py --gpu | 2.88 / 2.46 | 24-28 |
| payday | one-pass compat | 2.36 / 2.38 | 29-30 |
| payday | **ffcine grade** | **2.18 / 2.44** | 29-32 |
| r10 (96 f) | render.py --gpu | 3.50 / 5.55 | 17-27 |
| r10 | one-pass compat | 2.95 / 3.64 | 26-33 |
| r10 | **ffcine grade** | **2.86 / 2.93** | 33-34 |
| both (166 f) | render.py --gpu, 2 processes | 6.38 / 8.01 | 21-26 |
| both | one-pass compat, 2 processes | 5.31 / 6.02 | 28-31 |
| both | **ffcine batch, 1 process** | **4.28 / 4.68** | 35-39 |

An earlier, quieter run (load1 22-30) gave these wall times:

| run | payday | r10 |
|---|---|---|
| ffcine grade, cold shader cache | 2.20 s | 3.27 s |
| ffcine grade, warm cache | 2.10 s | 2.75 s |
| `--decode cuda` | 2.28 s | 2.75 s |
| `--codec hevc_vulkan` | 2.13 s | 2.62 s |
| ffcine batch, both shots | 4.32 s total | |

- **Per shot, ffcine is 5-20% faster than the one-pass ffmpeg graph** and is the most stable under
  load. The win comes from the persistent pipeline. The first shot pays 0.45-0.6 s of setup
  (device, `pl_vulkan_import`, input open and seek, LUT upload). Every later shot in a batch pays
  0.10-0.18 s. A warm `pl_cache` saves 0.1-0.5 s on the first shot. In the batch, r10 finishes in
  2.0 s against 2.6-2.9 s standalone.
- **Steady state is NVENC-bound.** ffcine's internal steady fps, (n-1)/(wall - first frame), is
  48-53 with NVENC p6 and 62-63 with `hevc_vulkan`. `hevc_nvenc -preset p6` alone, decoding the
  same clip under the same load, does about 51 fps. In the stage trace (`FFCINE_TRACE=1`, payday),
  render takes 0.5-0.9 ms per frame, the Vulkan -> CUDA download plus NVENC submit 0.4-1.1 ms, and
  the Vulkan-decode upload nothing. The rest is backpressure from the encoder. The bench's 65-70
  steady fps for the ffmpeg graphs were measured at load1 about 17 and from 1-frame runs, so they
  are not comparable with these.
- So step 1's remaining time is the encoder at p6 and the first-shot setup, not copies or
  kernels. NVENC p1 alone does about 170 fps (worth checking for previews).

### Zero-copy routes

| route | status |
|---|---|
| Vulkan video decode -> libplacebo | **landed, default** (`--decode vulkan`). Removes the CUDA -> Vulkan copy (0.15-0.2 ms/frame of decode-thread time, 25 MB/frame of VRAM traffic). It doesn't change wall time, because the run is encoder-bound. |
| Import NVDEC (CUDA) surfaces into Vulkan | not done. Vulkan can't import CUDA allocations; interop only goes Vulkan -> CUDA. It would need a CUDA frame pool backed by Vulkan-exported memory. Vulkan decode makes it unnecessary. |
| Rendered Vulkan frame -> Vulkan video encode | **landed, opt-in** (`--codec hevc_vulkan`, also `h264_vulkan`, `av1_vulkan`). No copies end to end, steady 62 fps against NVENC's 50. Not the default: 0.9956 / 52.6 dB misses the quality target. |
| NVENC fed from an exported Vulkan image | blocked, deferred. ffmpeg's `hevc_nvenc` only registers CUDA device pointers from its own `AV_PIX_FMT_CUDA` frames, with chroma at pitch x height. Vulkan -> CUDA export of optimal-tiled images gives `CUarray`s, not pointers. It would need linear Vulkan render targets exported into a custom CUDA frame pool at NVENC's layout, or our own NVENC session. The copy it would save is 0.4-1.1 ms/frame on the encode thread, off the critical path. |

Gotchas found on the way (ffmpeg 9.0.1/9.0.2, driver 615.78):
- The Vulkan encoders **segfault on a device with `disable_multiplane=1`**. A Vulkan device derived
  from CUDA always has it, even if you pass `disable_multiplane=0` to the derive. So ffcine now
  creates the Vulkan device first and derives CUDA from it. It disables multiplane per frames
  context (`AV_VK_FRAME_FLAG_DISABLE_MULTIPLANE`) only for frames that CUDA touches. `--cuda N`
  keeps the CUDA-first order, and refuses Vulkan encoders.
- Vulkan decode does work on a CUDA-derived device: the decoder makes its own multiplane frames.
  It only fails in the CLI graph because libplacebo's output frames are multiplane too.
- `pl_cache_set_file`/`get_file` append the hex key to the directory string as is, so the
  directory needs a trailing `/`.
- libplacebo render targets need `VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT` added to the frames
  context's usage before `av_hwframe_ctx_init`.

