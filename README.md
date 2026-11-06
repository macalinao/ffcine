# ffcine

GPU video finishing in Rust. ffcine decodes camera footage, grades it (HDR tone mapping, camera
LUTs, crop/scale, 3D LUTs, denoise, stabilization, user shaders) and encodes it with the frames
kept in VRAM end to end: libav* (ffmpeg) for demux/decode/encode/mux on NVDEC/NVENC, libplacebo
on Vulkan for the processing. Around that it renders whole timelines (OTIO, imported from
DaVinci Resolve, Final Cut Pro, Kdenlive/Shotcut, CapCut or EDL) with a content-keyed segment
cache (animated effect parameters take keyframes or ffmpeg-style expressions), mixes audio to
EBU R128, and handles the jobs before and after the edit: card offload, clip catalog, triage
with transcripts, chapters and stills.

It is a set of library crates (`ffcine` is the facade) with the `ffcine` CLI on top. Design:
[docs/design.md](docs/design.md).

## Requirements

- Linux with an NVIDIA GPU and driver: CUDA, NVDEC/NVENC, Vulkan, and optical flow (NVOFA,
  Turing or newer) for stabilization and temporal denoise.
- ffmpeg 9 (libav*) built with CUDA, NVENC/NVDEC and Vulkan, and libplacebo >= 7.365 (API 375).
- [Nix](https://nixos.org) with flakes is the supported way to get all of the above: the flake
  pins libplacebo and builds a matching ffmpeg.

Ingest and catalog need no GPU and build on macOS too
(`cargo install ffcine-cli --no-default-features`).

## Install

```sh
nix build github:talaagent/ffcine      # ./result/bin/ffcine
nix run github:talaagent/ffcine -- doctor
```

Or with the system libraries above installed: `cargo install ffcine-cli`.

`ffcine doctor` checks the GPU path (CUDA, Vulkan, NVENC, NVDEC, NVOF, the linked libav and
libplacebo) and exits non-zero if anything is missing.

## Quickstart

```sh
ffcine probe MVI_2884.MP4                                  # Canon EOS R10
ffcine grade MVI_2884.MP4 out.mkv                          # Rec.709 from the probe
ffcine grade --lut show.cube --sharpen 0.6 DJI_0001_D.MP4 out.mkv   # Osmo Pocket 4, a baked LUT

ffcine import project.otio -o timeline.otio     # from Resolve, FCPXML, Kdenlive, ...
ffcine render --profile master -o master.mp4 timeline.otio
```

`grade` defaults: bt.2390 tone mapping, perceptual gamut mapping, lanczos scaling in gamma light,
10-bit HEVC NVENC (`p6`, `hq`, `qp=12`, `main10`, `p010le`, GOP 24). `--preview` switches to
8-bit libx264; `--hdr pq` (or `hlg`) makes a BT.2020 HDR10 / HLG master instead.

## Commands

| | |
|---|---|
| `probe`, `doctor` | inspect a clip; check the GPU path |
| `grade`, `batch`, `looks` | grade one shot, many shots on one warm pipeline, or compare looks |
| `stabilize`, `jitter` | optical-flow or gyro stabilization; measure residual jitter |
| `import`, `subs`, `title` | other editors' projects to an ffcine timeline; `.ass` subtitles as text clips; title sequences and end screens |
| `render`, `diff`, `comp` | render a timeline or a comp (video, still, text and nested-comp layers with keyframes, expressions, time remaps and libavfilter graphs) to a master with a segment cache; what changed between renders; check a comp |
| `audio`, `sync`, `beats` | the timeline's audio mix (R128, buses, ducking); line up recorder WAVs; a song's tempo and beats |
| `ingest`, `catalog`, `fetch-luts`, `bake` | verified card offload; clip catalog with picture profiles; camera LUTs; camera LUT + LUT + look baked to one `.cube` |
| `triage`, `search`, `snap` | index a card (strips, scene cuts, transcripts); find words; snap cuts to words |
| `breakdown` | shot list of a reference video: cuts, keyframes, camera motion, beat, colour, speech, contact sheet |
| `chapters`, `tag`, `credits`, `still`, `publish` | YouTube chapters, chapters and cover art in the MP4, music credits, stills, upload to Immich |

`ffcine <command> --help` has every flag with its default, examples and the docs page to read
next (`-h` is the brief form); [docs/](docs) has one page per area.
[skills/](skills) has agent skills: [shot-breakdown](skills/shot-breakdown/SKILL.md) turns a
TikTok, reel or YouTube link into a shot list to recreate it.

## Development

```sh
nix develop            # or direnv; installs the git hooks
cargo build && cargo test
cargo clippy --workspace --all-targets -- -D warnings
nix fmt
```

Tests that need a GPU skip themselves when there is none, so `cargo test` passes on CI runners.

## License

ffcine is licensed under the [Apache License, Version 2.0](LICENSE).

No binaries are distributed; build from source. A built `ffcine` links ffmpeg, so its licence
terms follow the ffmpeg you link: the flake's ffmpeg is configured with `--enable-gpl
--enable-version3` (GPL-3.0). If you build ffmpeg with `--enable-nonfree` (e.g. for `libnpp` or
`libfdk-aac`), the result can't be redistributed at all. NVENC/NVDEC, CUDA and optical flow come
from the proprietary NVIDIA driver at run time and are not part of ffcine.
