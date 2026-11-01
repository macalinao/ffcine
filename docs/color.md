# Colour in

Every source gets a **camera profile** before the grade: either a camera LUT (log -> Rec.709) on
the decoded picture, or libplacebo's own tone and gamut mapping from the stream's tags. Code:
`crates/ffcine-core/src/camera.rs` (profiles, detection, analytic LUTs), `Grade::input_lut` and
`Grade::color_map` in `crates/ffcine-render`. Checks: `bench/color.sh`. Run on 2026-11-01, RTX
5060 Ti, driver 615.78.08, shared box, everything at `nice -n 19`.

## Profiles

| profile | picked when | colour-in |
|---|---|---|
| `dlog` | 10-bit, SDR-tagged, `encoder` starts with `DJI` (Osmo Pocket 4) | DJI's Pocket 4 D-Log -> Rec.709 V2.0 LUT; without it the analytic D-Log / D-Gamut transform |
| `dlog-vivid` | override only | DJI's vivid V2.0 LUT (vendor file only) |
| `dlogm`, `dlogm-vivid` | the same on an Osmo Pocket 3 (`encoder` has `Pocket3`) | DJI's D-Log M LUTs (vendor file only: DJI publishes no D-Log M curve) |
| `clog3` | BT.2020, untagged transfer, Canon file (`CAEP` brand); else override | analytic Canon Log 3 / Cinema Gamut |
| `applelog` | BT.2020, untagged transfer (iPhone ProRes Log; render.py's rule) | analytic Apple Log (render.py's `applelog_lut.py`, same constants) |
| `hdr` | HLG or PQ transfer (iPhone HLG + Dolby Vision 8.4, Canon HDR PQ), or BT.2020/P3 primaries with a bt709 transfer | libplacebo: DV RPU applied, BT.2390 tone map with per-frame peak detect, perceptual gamut mapping. No LUT |
| `rec709` | anything else | honour the tags |

- **Per clip**: `ffcine grade --camera PROFILE` (`auto` is the default), or `camera` in a
  timeline `GradeParams` (project grade, look, or the clip's `ffcine.color.grade/1`).
- **With `--lut`** (a baked grade, which in render.py's bake already contains the camera LUT),
  `auto` adds no camera LUT; an explicit `--camera` does. With both, the camera LUT goes on the
  source (`pl_frame.lut`, `PL_LUT_NORMALIZED`) and the grade LUT moves to the target
  (`target.lut`), so it grades the converted Rec.709 picture.
- A camera LUT re-tags the source as BT.709 primaries and transfer (matrix and range stay), so
  libplacebo converts nothing more after it.
- `dlog*` turn libplacebo's deband on (the LUT stretches the log curve into visible bands);
  `--no-deband` turns it off.
- The LUTs are `pl_custom_lut`s built from our own `.cube` parser or generated in Rust (33^3), not
  CPU `lut3d`. libplacebo interpolates them itself (not tetrahedral), as render.py's GPU path did.

**Detection** (`Profile::detect_file`): first `ffcine_core::profile::detect` on the file's ISO
BMFF metadata, the same detection the ingest catalog records: DJI's colour mode from the `djmd`
CAM meta protobuf (DJI tags bt709 for log and "normal" alike), Canon and Apple tags, `colr`. A
medium or high confidence answer wins; otherwise the probe rules in the table (a 10-bit DJI clip
counts as log). D-Log2 (Pocket 4P) has no camera profile yet and falls through to the probe rules.
Canon C-Log 3 tagging is unconfirmed (no C-Log 3 sample clip; the R10 has no Canon Log at all,
only HDR PQ).

## Vendor LUTs

Looked up in `$FFCINE_LUT_DIR` (`:`-separated), then `~/.local/share/ffcine/luts`, by the names in
`Profile::vendor_names`. None are vendored: DJI states no redistribution terms.

| LUT | where | in ffcine |
|---|---|---|
| DJI OSMO Pocket 4 D-Log to Rec.709 V2.0 (+ vivid) | <https://www.dji.com/downloads/products/osmo-pocket-4>, direct `.cube` links on `terra-1-g.djicdn.com` | `ffcine fetch-luts` downloads both (`camera::VENDOR_LUTS`) |
| the same V1.0 (what render.py used) | same page | found by name, after V2.0 |
| DJI Pocket 3 D-Log M to Rec.709 (+ vivid) | <https://www.dji.com/downloads/products/osmo-pocket-3> | drop into the LUT dir |
| Apple Log to Rec.709 | Apple developer downloads (Apple Log profile, licence restricts redistribution) | optional; else analytic |
| Canon Cinema Gamut / C-Log 3 to BT.709 | Canon's LUT downloads (`CinemaGamut_CanonLog3-to-BT709_WideDR_65_FF_Ver.2.0.cube`) | optional; else analytic |

Pocket 4P also has D-Log2 (DJI publishes `DJI OSMO Pocket 4P D-Log2 to Rec.709 V1.0 size65.cube`);
not a profile yet.

## Analytic transforms

Decode the log curve to scene linear, 3x3 gamut matrix to BT.709 (derived from the primaries,
SMPTE RP 177, no adaptation: all are D65), exposure, Narkowicz's ACES filmic fit, BT.1886 (2.4)
encode. That is render.py's `applelog_lut.py`, generalised.

| curve | source | gamut | exposure |
|---|---|---|---|
| Apple Log | Apple Log Profile white paper | BT.2020 (render.py's 4-decimal matrix) | 0.8 (render.py) |
| DJI D-Log | DJI "D-Log and D-Gamut" white paper (the curve Resolve's CST uses) | D-Gamut (0.71, 0.31), (0.21, 0.88), (0.09, -0.08) | 0.53: mid grey where DJI's V2.0 LUT puts it |
| Canon Log 3 | Canon's formula (as colour-science `log_decoding_CanonLog3`, reflectance in) | Cinema Gamut | 0.8 |

The analytic D-Log is a fallback: its shoulder is harder than DJI's (highlights brighter, e.g.
linear 1.44 -> 0.88 vs DJI's 0.78) and blues a little more saturated; 31.7 dB Y against DJI's V2.0
LUT on payday after the exposure match.

## Tone and gamut mapping

`Grade` exposes every libplacebo curve and gamut mapping by name (`--tonemap`, `--gamut`):
tone `bt.2390` (default), `bt.2446a`, `st2094-40`, `st2094-10`, `spline`, `reinhard`, `mobius`,
`hable`, `gamma`, `linear`, `clip`, `auto`; gamut `perceptual` (default), `softclip`,
`highlight`, `relative`, `saturation`, `absolute`, `desaturate`, `darken`, `linear`, `clip`.
`ColorMapTuning` sets contrast recovery (`--contrast-recovery`, 0.3) and the perceptual
mapping's strength and deadzone (`--perceptual-strength`, 0.8; deadzone 0.3). Peak detection
(`pl_peak_detect`, scene-cut reset) is on unless `--no-peak-detect`. Dolby Vision and HDR side data
are stripped from SDR output frames (libplacebo would otherwise try to re-apply DV to the target).

## Checks (`bench/color.sh`)

SSIM / PSNR on yuv420p10le against a reference running render.py's colour-in chain on ffmpeg.

| check | source | reference | SSIM Y | PSNR Y dB |
|---|---|---|---|---|
| `dlog` auto, DJI V2.0 LUT | payday (`dji:0004`, Pocket 4 D-Log 10-bit HEVC), 70 f | ffmpeg `lut3d` (tetrahedral), same LUT | 0.9972 | 54.3 |
| grade LUT moved to the target | same, `--camera dlog --lut identity` | `dlog` auto | 0.9982 | 56.5 |
| DJI V1.0 vs V2.0 LUT | same | | 0.9948 | 41.9 |
| analytic D-Log vs V2.0 (exposure 0.53; 0.9715 / 23.3 dB at 0.8) | same | | 0.9895 | 31.7 |
| prototype match (roadmap step 1) | payday, render.py's baked grade LUT | render.py CPU render | 0.9973 | 54.4 |
| iPhone HLG + DV 8.4 auto (`hdr`) | `IMG_9124.MOV`, 1080p HEVC 10-bit, 120 f | vf_libplacebo `tonemapping=bt.2390:gamut_mode=perceptual` (render.py's colour-in) | 0.9977 | 50.9 |
| Apple Log auto (analytic `pl_custom_lut`) | `IMG_9130.MOV`, ProRes HQ 4:2:2, 60 f | render.py's `scale` + `lut3d` with `applelog_to_rec709.cube` | 0.9949 | 49.4 |
| R10 prototype match | `MVI_2884.MP4` (H.264 8-bit 4:2:0 full range), 96 f | render.py CPU render | 0.9965 | 51.7 |
| 10-bit 4:2:2 HEVC, `--decode vulkan` | synthetic 4K25 `testsrc2` (yuv422p10le, bt709 tv), 100 f | ffmpeg software decode | 0.9997 | 58.4 |
| the same, `--decode cuda` | | | 0.9997 | 58.4 |

There is no iPhone HLG shot in render.py's bench and its cached renders are stale, so the HLG
reference is render.py's colour-in filter on its own. The look on top of it matches as payday does
(same baked-LUT path).

### 4:2:2 decode on Blackwell

The R10 records 10-bit 4:2:2 HEVC; no such sample clip was available (the only R10 video is 8-bit
H.264), so a synthetic one stands in. On this RTX 5060 Ti neither hardware path takes it:

- Vulkan video: `format (yuv422p10le) not supported`, the hwaccel fails to initialise.
- NVDEC (`--decode cuda`): the capability query says yes, `cuvidCreateDecoder` then fails with
  `CUDA_ERROR_NOT_SUPPORTED` (ffmpeg 9.0.1, driver 615.78).

ffmpeg falls back to software decode on its own, and the frames are uploaded to Vulkan per frame,
so the render is right (0.9997 SSIM) but decode-bound: 30 fps steady at 4K25 on a shared machine
against 50+ for hardware-decoded 4:2:0. ffcine now says so (`decode: software (yuv422p10le; ...)`).
Blackwell's NVDEC lists HEVC 4:2:2 in NVIDIA's support matrix, so this is likely a driver/ffmpeg
gap worth retrying on a newer driver; until then 4K60 R10 footage decodes at roughly half real
time.

## Fixed along the way

- Probe failed on iPhone 16 Pro files: the APAC spatial-audio stream has no ffmpeg decoder. Streams
  without a decoder are now listed as `other`.
- Output frame rate: with no `--fps`, MP4/MOV sources (decoder rate unset) were written as
  23.976 fps whatever they were; it now comes from the stream (`av_guess_frame_rate`), which
  matters for 60p / mixed-rate footage.
- `MediaInfo.tags` (container metadata) is new, for profile detection.

## Left

- A `dlog2` profile for the Pocket 4P (DJI publishes a 65^3 D-Log2 LUT).
- A real R10 4:2:2 clip and a C-Log 3 clip, to confirm tags and decode; Canon HDR PQ goes through
  the `hdr` path untested on real footage.
- HDR deliverables (HLG passthrough)
: lower priority, SDR Rec.709 is the deliverable.
- The Rust LUT baker (camera LUT + look in one LUT) can drop the target-LUT split.
