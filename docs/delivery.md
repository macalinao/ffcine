# Delivery

Extras generated from the timeline, and encode profiles for the copies people actually watch.
Code: `ffcine-delivery` (chapters, credits, frame lookup, MP4 tags in `mp4`),
`ffcine_render::profile` (`DeliveryProfile`), CLI in `ffcine-cli/src/deliver.rs`.

## YouTube chapters

```sh
ffcine chapters edit.otio [--intro Intro] [-o chapters.txt]
```

One `M:SS Title` line per chapter (`H:MM:SS` past an hour), from the timeline's markers: those
with `metadata.ffcine.chapter` set (the `edit.yaml` importer flags chapter cards), else every
marker. Markers on clips are placed through the clip (their time is in the clip's source time),
markers on tracks, gaps and the timeline stack directly. Markers within a second of the previous
one are merged. If the first chapter isn't at 0:00, an `--intro` chapter is added there, as
YouTube requires. Warnings (stderr) for fewer than 3 chapters or any shorter than 10 s.

## Chapters and cover in the MP4

```sh
ffcine tag master.mp4 --timeline edit.otio [--intro Intro] [--cover thumb.png | --cover 2:47]
ffcine tag master.mp4 --clear
ffcine render -o master.mp4 edit.otio [--cover 2:47] [--no-chapters]
```

The same chapters as `ffcine chapters`, written into the file with `mp4-atom`, without
re-encoding. They go in twice, as ffmpeg's muxer writes them: a Nero `moov/udta/chpl` list
(ffmpeg, VLC, mpv) and a QuickTime chapter track, a disabled `text` track every other track
points at with `tref/chap` (Apple players read only that one). The cover is an iTunes
`moov/udta/meta/ilst/covr` item: a JPEG or PNG file, or with a time, the frame of the video
there as a JPEG (`$FFMPEG`). ffprobe lists the chapters (`-show_chapters`) and the cover as an
attached picture stream.

The file is rewritten once: `ftyp`, the new `moov`, a small `mdat` with the chapter samples, then
every other top-level box byte for byte, with the chunk offsets (`stco`, `co64` past 4 GiB) moved
along. So the result is `+faststart` too. A previous run's chapter track, `chpl` and `covr` are
replaced, so tagging twice is tagging once. `tag` keeps what it isn't given (`--cover` alone
keeps the chapters); `--clear` starts from nothing. The video and audio packets are untouched
(`framemd5` before and after match; `crates/ffcine-delivery/tests/mp4_ffprobe.rs`).

`ffcine render` writes the timeline's chapters (when it has markers) and `--cover` into the
master after the mux, and only rewrites when they differ from what the file has.

## Music credits

```sh
ffcine credits edit.otio [--public] [-o credits.txt]
```

One line per music track in order of first use: the clip's `metadata.ffcine.attribution` as
written (the CC BY line), else its `music` name with a warning. Duplicates are dropped.
`--public` is for the public cut: `private` clips are left out, or credited by their
`public_alt` when that is an object with its own `music`/`attribution`.

## Stills

```sh
ffcine still edit.otio --at 2:47 [-o thumb.png] [--size WxH] [--no-text] [--cuda N]
```

A full-resolution 8-bit RGB PNG of the timeline at `--at` (seconds, `M:SS` or `H:MM:SS`): the
top-most enabled video clip with media there, at the matching source time (speed changes and
freezes included), through the GPU grade with the clip's resolved grade (LUT and look, as in `looks`), at the timeline's `frame_size`
(else the source's size), with the text clips on that frame composited.

## Look comparisons

```sh
ffcine looks edit.otio --at 2:47 --sample 4 -v cut -v day -v "cool:warmth=-0.02" \
  -v "pop:saturation=1.2,contrast=0.25" -v teal.cube -v "log:lut=none" -o looks/
ffcine looks --clip A001.MP4@12.5 -v normal.cube -v alt.cube
```

Every shot × variant as a full-res JPEG (`<shot>_<variant>.jpg`) and, per shot, a contact sheet
(`<shot>_sheet.jpg`, `--sheet-width` 2400) with the variant names under each frame, saved at the
best JPEG quality under 5 MB, for posting in chat. Shots come from `--at` times and `--sample N`
(evenly spaced) on the timeline, or `--clip FILE@SECONDS`. Photos are taken at time 0; a shot
that fails to decode is skipped with a warning.

Tiled HEIF stills (iPhone HEICs are a grid of 512x512 HEVC tiles) decode as the whole picture:
libavformat lists each tile as a stream plus a tile-grid stream group, and the decoder
(`ffcine-render` `backend/tiles.rs`) decodes every tile in software, places it on the canvas,
crops to the grid's size (`clap`) and turns it upright (`irot`, its display matrix), matching
the ffmpeg CLI. `probe` reports the grid as one video stream at the displayed size. Mirroring
(`imir`) isn't applied.

A variant is `[NAME:]SPEC`: a project look name, a `.cube` file or LUT alias, `cut` (the clip's
own grade), or comma-separated `look=NAME`, `lut=ALIAS|FILE|none` and `GradeParams` fields
(`warmth=0`, `saturation=1.1`, `lut_strength=0.7`, ...). Each one layers like a clip grade:
project grade <- look <- source-kind rule <- the clip's override <- the variant's fields. With no
`-v`, it compares the cut against every project look.

The grade is the LUT (mixed by `lut_strength`) plus one generated `PREOUTPUT` hook reproducing
the prototype's post-LUT chain (`colorbalance` warmth/tint, `curves` contrast/gamma/lift/gain,
saturation) with `//!PARAM`s, so all variants share one compiled shader. `rgb_gain` is folded
into the LUT and `extra` filters run as hooks after the look ([assembly.md](assembly.md)); `denoise`
and unsupported `extra` filters are warned and skipped. Everything runs through one warm
GPU pipeline (as `batch`), with JPEG encoding on worker threads while the GPU renders the next
variant: 15 4K stills and 3 sheets take about 10 s. `ffcine still` uses the same grade.

## Render profiles

`ffcine grade --deliver PROFILE` picks the encoder and output size (`--size` still wins;
`--render-profile`'s size is the starting point). `--text TIMELINE --text-at S` draws a
timeline's text over the clip, `S` being the timeline time of the first output frame.

| Profile | Encode | Size |
|---|---|---|
| `master` | 10-bit HEVC NVENC constqp 12, p6 hq (the prototype's) | source |
| `preview` | 8-bit x264 crf 10 (the prototype's) | source |
| `review` | 8-bit HEVC NVENC, VBR to a bitrate that fits `--budget-mb` (default 50), `multipass=fullres`, lookahead, spatial/temporal AQ, 2 s GOP | largest of 1080/720/540/360 (short side) that gets ≥ 0.035 bits per pixel |
| `phone` | 8-bit HEVC NVENC `main`, constant quality 23 capped at 16 Mb/s, AQ, 2 s GOP | 1080 on the short side (never upscaled) |

`review`: the video bitrate is `budget × 8 × 0.96 / duration − 160 kb/s` (audio is muxed in later;
the 4 % covers the container and rate-control error). After encoding, if the file plus the audio
share is over the budget, it encodes once more at the bitrate scaled down by the overshoot: a
second pass that only runs when needed. A 10 min video in 50 MB gets about 480 kb/s at 540p; a
90 s reel 4 Mb/s at 1080p.

HEVC in `.mp4`/`.mov` is tagged `hvc1` (always, not only for these profiles): Apple players
refuse ffmpeg's default `hev1`. With `--deliver`, an `.mp4`/`.mov` output is made `+faststart`
afterwards (the `moov` moved ahead of the media, as `tag` does), so the phone copy plays over
HTTP before it has downloaded.

## Not yet

- Audio in `grade --deliver`: `grade` writes video only (`AUDIO_BITRATE` is reserved in the
  budget); `ffcine render` muxes the timeline's mix.
