# ffcine

[![CI](https://github.com/talaagent/ffcine/actions/workflows/ci.yml/badge.svg)](https://github.com/talaagent/ffcine/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ffcine.svg)](https://crates.io/crates/ffcine)
[![docs.rs](https://docs.rs/ffcine/badge.svg)](https://docs.rs/ffcine)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://github.com/talaagent/ffcine/blob/master/Cargo.toml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/talaagent/ffcine/blob/master/LICENSE)

GPU video finishing in Rust. ffcine decodes camera footage, grades it (HDR tone mapping, camera
LUTs, crop/scale, 3D LUTs, denoise, stabilization, user shaders) and encodes it with the frames
kept in VRAM end to end: libav* (ffmpeg) for demux/decode/encode/mux on NVDEC/NVENC, libplacebo
on Vulkan for the processing. Around that it renders whole timelines (OTIO, imported from
DaVinci Resolve, Final Cut Pro, Kdenlive/Shotcut, CapCut or EDL) with a content-keyed segment
cache (animated effect parameters take keyframes or ffmpeg-style expressions), mixes audio to
EBU R128, and handles the jobs before and after the edit: card offload, clip catalog, triage
with transcripts, chapters and stills.

It is a set of library crates (`ffcine` is the facade) with the `ffcine` CLI on top. Design:
[docs/design.md](https://github.com/talaagent/ffcine/blob/master/docs/design.md).

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
| `render`, `diff`, `comp` | render a timeline or a comp (video, still, text and nested-comp layers with keyframes, expressions, time remaps and libavfilter graphs) to a master with a segment cache; what changed between renders; check a comp, write out its template, or start one from the `story` template |
| `audio`, `sync`, `beats` | the timeline's audio mix (R128, buses, ducking); line up recorder WAVs; a song's tempo and beats |
| `ingest`, `catalog`, `fetch-luts`, `bake` | verified card offload; clip catalog with picture profiles; camera LUTs; camera LUT + LUT + look baked to one `.cube` |
| `triage`, `search`, `snap` | index a card (strips, scene cuts, transcripts); find words; snap cuts to words |
| `breakdown` | shot list of a reference video: cuts, keyframes, camera motion, beat, colour, speech, contact sheet |
| `chapters`, `tag`, `credits`, `still`, `publish` | YouTube chapters, chapters and cover art in the MP4, music credits, stills, upload to Immich |

`ffcine <command> --help` has every flag with its default, examples and the docs page to read
next (`-h` is the brief form); [docs/](https://github.com/talaagent/ffcine/blob/master/docs) has one page per area.
[skills/](https://github.com/talaagent/ffcine/blob/master/skills) has agent skills: [shot-breakdown](https://github.com/talaagent/ffcine/blob/master/skills/shot-breakdown/SKILL.md) turns a
TikTok, reel or YouTube link into a shot list to recreate it.

## Rust library quickstart

For timeline manipulation, LUT parsing, and pipeline construction without native
libraries, disable the default GPU features:

```toml
[dependencies]
ffcine = { version = "0.4", default-features = false }
```

```rust
use ffcine::{Pipeline, Rational, timeline::Timeline};

let rate: Rational = "24000/1001".parse()?;
assert_eq!(rate.num, 24_000);
let pipeline = Pipeline::new("input.mov", "output.mkv");
assert!(pipeline.stages.is_empty());
let timeline = Timeline::new("Example edit");
let json = timeline.to_json()?;
assert_eq!(Timeline::from_json(&json)?, timeline);
# Ok::<(), ffcine::Error>(())
```

This constructs values and serializes JSON; it does not open media or use a GPU.
To run a render, enable default features and install the native requirements above:

```rust,no_run
# #[cfg(all(feature = "ffmpeg", feature = "placebo"))]
# fn render() -> ffcine::Result<()> {
use ffcine::{Backend, Grade, Pipeline, backend::Ffmpeg};

let backend = Ffmpeg::new()?;
println!("{}", backend.probe("input.mov".as_ref())?);
Pipeline::new("input.mov", "output.mkv")
    .grade(Grade::default())
    .run(&backend)?;
# Ok(()) }
```

The [API reference](https://docs.rs/ffcine) describes the portable API; docs.rs
builds without default features because its builders lack the matching native
libraries. Generate the complete backend docs locally with
`nix develop --command cargo doc --workspace --no-deps --open`.
See [CONTRIBUTING.md](https://github.com/talaagent/ffcine/blob/master/CONTRIBUTING.md)
for checks, examples, native dependencies, and release instructions.

## Workspace packages

All crates share a version. The published release may lag behind `master`;
use the Git repository when you need changes that have not reached crates.io.

| Crate | crates.io | rustdoc |
| --- | --- | --- |
| `ffcine` | [package](https://crates.io/crates/ffcine) | [API docs](https://docs.rs/ffcine) |
| `ffcine-assembly` | [package](https://crates.io/crates/ffcine-assembly) | [API docs](https://docs.rs/ffcine-assembly) |
| `ffcine-audio` | [package](https://crates.io/crates/ffcine-audio) | [API docs](https://docs.rs/ffcine-audio) |
| `ffcine-breakdown` | [package](https://crates.io/crates/ffcine-breakdown) | [API docs](https://docs.rs/ffcine-breakdown) |
| `ffcine-cli` | [package](https://crates.io/crates/ffcine-cli) | [API docs](https://docs.rs/ffcine-cli) |
| `ffcine-comp` | [package](https://crates.io/crates/ffcine-comp) | [API docs](https://docs.rs/ffcine-comp) |
| `ffcine-core` | [package](https://crates.io/crates/ffcine-core) | [API docs](https://docs.rs/ffcine-core) |
| `ffcine-delivery` | [package](https://crates.io/crates/ffcine-delivery) | [API docs](https://docs.rs/ffcine-delivery) |
| `ffcine-filters` | [package](https://crates.io/crates/ffcine-filters) | [API docs](https://docs.rs/ffcine-filters) |
| `ffcine-hooks` | [package](https://crates.io/crates/ffcine-hooks) | [API docs](https://docs.rs/ffcine-hooks) |
| `ffcine-import` | [package](https://crates.io/crates/ffcine-import) | [API docs](https://docs.rs/ffcine-import) |
| `ffcine-ingest` | [package](https://crates.io/crates/ffcine-ingest) | [API docs](https://docs.rs/ffcine-ingest) |
| `ffcine-reframe` | [package](https://crates.io/crates/ffcine-reframe) | [API docs](https://docs.rs/ffcine-reframe) |
| `ffcine-render` | [package](https://crates.io/crates/ffcine-render) | [API docs](https://docs.rs/ffcine-render) |
| `ffcine-stabilize` | [package](https://crates.io/crates/ffcine-stabilize) | [API docs](https://docs.rs/ffcine-stabilize) |
| `ffcine-text` | [package](https://crates.io/crates/ffcine-text) | [API docs](https://docs.rs/ffcine-text) |
| `ffcine-timeline` | [package](https://crates.io/crates/ffcine-timeline) | [API docs](https://docs.rs/ffcine-timeline) |
| `ffcine-triage` | [package](https://crates.io/crates/ffcine-triage) | [API docs](https://docs.rs/ffcine-triage) |
| `ffcine-libplacebo-sys` | [package](https://crates.io/crates/ffcine-libplacebo-sys) | [API docs](https://docs.rs/ffcine-libplacebo-sys) |

## Development

```sh
nix develop            # or direnv; installs the git hooks
cargo build && cargo test
cargo clippy --workspace --all-targets -- -D warnings
nix fmt
```

Tests that need a GPU skip themselves when there is none, so `cargo test` passes on CI runners.

## Security

Report vulnerabilities privately using the instructions in
[SECURITY.md](https://github.com/talaagent/ffcine/blob/master/SECURITY.md).
Use [GitHub Issues](https://github.com/talaagent/ffcine/issues) for other bugs and
feature requests, with the command, native library versions, and a minimal example.

## License

ffcine is licensed under the [Apache License, Version 2.0](https://github.com/talaagent/ffcine/blob/master/LICENSE).

No binaries are distributed; build from source. A built `ffcine` links ffmpeg, so its licence
terms follow the ffmpeg you link: the flake's ffmpeg is configured with `--enable-gpl
--enable-version3` (GPL-3.0). If you build ffmpeg with `--enable-nonfree` (e.g. for `libnpp` or
`libfdk-aac`), the result can't be redistributed at all. NVENC/NVDEC, CUDA and optical flow come
from the proprietary NVIDIA driver at run time and are not part of ffcine.
