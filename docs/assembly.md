# Assembly and incremental rendering

`ffcine render TIMELINE -o master.mp4` turns a whole OTIO timeline into one master. Each part of
the picture is rendered once into a cache keyed by its content. After an edit, only the changed
parts render again. The master is joined by stream copy, and an audio-only change is a remux.
This is roadmap step 8.

```sh
ffcine import -o v1.otio edit.yaml                  # or Resolve OTIO, FCPXML, MLT
ffcine render -o v1.mp4 v1.otio                     # preview profile: 1080p HEVC 10-bit
ffcine render --profile master -o v1-4k.mp4 v1.otio # 4K master
ffcine render --render-profile reel -o v1-reel.mp4 v1.otio  # 9:16 (also feed 4:5, square 1:1)
ffcine diff v1.otio v2.otio                         # which segments v2 re-renders
ffcine render --dry-run -o v2.mp4 v2.otio           # same, against what's in the cache
ffcine render --json -o v2.mp4 v2.otio              # progress as JSON lines on stdout
ffcine publish --immich "Trip review" v2.mp4        # review copy to an Immich album
```

Re-running a render resumes it. Every cache entry is written to a `.partial` file and renamed
once it's complete, so a killed render (Ctrl-C, OOM, reboot) keeps every finished segment. Nothing
is ever pruned, and old masters stay where they are.

## Pipeline

1. **Plan** (`ffcine_assembly::plan`). The first enabled video track is the picture. It flattens
   into render units:
   - one per **shot**: a clip's source range at its speed (`LinearTimeWarp`, `FreezeFrame`,
     `ffcine.time.ramp/1`), with the grade resolved (base <- look <- source-kind rule <- clip)
     and its picture filters read ([Per-clip filters](#per-clip-filters));
   - one per **transition overlap**: a `Transition` takes `in_offset` frames from the outgoing
     shot and `out_offset` from the incoming one, and the overlap (both sides, `in + out` frames)
     is its own unit. The total length is unchanged. Its `ffcine.transition/1` `kind` picks the
     mix: `dissolve` (default; also anything unknown), `dip` (out to `colour` over the first half,
     in from it over the second), `flash` (blacks lifted to white and back), `whip` (both
     pictures slide the way of `direction`, motion-blurred by their speed);
   - one per **split-screen** (`Stack` with more than one child, `ffcine.comp.layout/1`): every
     panel, each flattened to its own shots and holds;
   - one per run of **gap**: black.

   Shots, split-screens and gaps are cut at every `ffcine.text` in and out point
   (`ffcine_text::TextLayer::cuts`) and at every overlay clip's, unless that would leave a piece
   shorter than a second. Each segment's key includes the text it shows
   (`TextLayer::fingerprint`) and the overlay clips over it, so a caption or logo edit
   re-renders only the seconds under it. Overlay clips are the non-text clips of the other video
   tracks (`PiP N` from `edit.yaml`), bottom track first. Audio tracks become one mix.
2. **Segments** (`exec`). A unit missing from `<cache>/seg/<key>.mp4` renders in one GPU
   `Pipeline` pass (decode, denoise, stabilize warp, reframe crop, LUT, look hook, finish, scale to the profile size, sharpen, grain,
   layers, text overlays, NVENC). MP4 keeps exact `1001/24000` timestamps; MKV's millisecond ones
   made frame durations alternate between 41 and 42 ms:
   - the unit's main picture is the pass's input: the shot, a transition's outgoing side, or a
     tiny black clip the pass scales up (gaps, split-screens, whips);
   - everything else is a **layer** (`ffcine_render::Layer`): its own decoder, grade and
     libplacebo renderer, drawn into an RGBA texture whose `OUTPUT` hook writes alpha (opacity,
     rounded corners; `shaders::alpha`), then composited over the picture at its per-frame
     rectangle as a `pl_overlay`, under the text. Transition sides, split-screen panels (cover
     crop around `focus`, `push`, staggered slide-in, `radius`, over the `render.py` backdrop),
     picture-in-picture videos and PNG/JPEG stills are all layers, so there is no CPU blend;
   - per-shot finishing (`ffcine.fx.vignette/1`, `darken/1`, `fade/1`, `flash/1`) is one
     `PREOUTPUT` hook after the look (`shaders::finish`), its fade and flash per frame from the
     piece's place in its clip; transitions fold their fades into it;
   - film grain (`ffcine.fx.grain/1` on the timeline's top-level stack, `profiles.<name>`
     overriding per render profile; a clip's own overrides it) is the `ffcine-filters` grain hook
     on the picture and on video layers;
   - speed changes, ramps, freezes and temporal blends are retimed first with ffmpeg on NVDEC, into
     a lossless NVENC intermediate (for layers too). Frames stay in VRAM, so a 20x timelapse reads 20x its length
     of 4K quickly;
   - a pass that fails (iPhone HLG with Dolby Vision RPUs) is retried once on stream copies of
     every source with the RPUs stripped (`dovi_rpu=strip=1`).
3. **Audio**. The finished mix, as `ffcine audio` makes it ([audio.md](audio.md)): clean chain,
   R128 levelling, buses with music ducked under dialogue, clip gain/pan keyframes and fades,
   synced recorder clips with drift correction, master at the target loudness under a true-peak
   limiter. It is cut to the picture's length (the master fade-out ends on the last frame),
   encoded to AAC once into `<cache>/audio/<key>-aac<bitrate>.m4a`, and keyed by its own recipe.
   Its settings come from the timeline (`project.mix`, clip effects); `--lufs`, `--true-peak`,
   `--headroom`, `--no-normalize` and `--sidechain` override them for a run.
4. **Join and mux** in one stream-copy pass: the concat demuxer over the segment files plus the
   audio (`-c copy -tag:v hvc1 -movflags +faststart`). Every segment comes out of the same encoder
   with the same settings (codec, profile, GOP, colour tags) and starts on an IDR. Their parameter
   sets are byte-identical (checked: one extradata hash across all segments), so the joined
   stream decodes cleanly with `-xerror`, has exactly the planned frame count, and every frame
   lasts 1001/24000 s. The output gets a manifest, `<output>.ffcine.json`, with the keys it was
   made from. If the master key matches, the render does nothing.

## Keys

A key is the first 128 bits of SHA-256 over canonical JSON (object keys sorted). For a segment
it covers:

- the **media fingerprint**: canonical path, size and mtime in ns (no content read, so a 15-minute
  4K timeline plans instantly);
- the **source range** (start in source seconds, frame count, speed) and the timeline rate;
- the **resolved grade** (`GradeParams` after layering), with each LUT by the hash of its file
  contents rather than its path or alias;
- the clip's **filters** (`plan::Filters`), as read for this render profile: stabilize
  parameters plus the inputs the warp derives from (the detect settings and the motion cache's
  `FORMAT_VERSION`, and the clip frames analysed), denoise, the reframe after its
  `profiles.<aspect>` override, the ramp and the temporal blend. Whole-clip filters (stabilize, reframe, ramp) also
  key the clip's start and length and the piece's offset in it, since a piece's pixels depend on
  the rest of the clip;
- the clip's **other `ffcine.*` effects** (vignette, fade, ...), so
  turning them on later invalidates the right segments; with a fade or flash, also the piece's
  offset in its clip and the clip's length;
- a transition's **kind** (a plain dissolve keys as before), a split-screen's layout, panels and
  offset, the **overlay clips** over the segment (source, transform, blend, offset) and the
  timeline's **grain**;
- the **render profile** (size, aspect name, punch cap and the full encoder config), the ffcine
  version and `RENDER_REVISION`. Bump `RENDER_REVISION` whenever the render path changes its
  output (5: `rgb_gain`, `extra` and temporal blend).

Changing one filter re-renders that clip's segments (and a dissolve it is part of) only, and an
unchanged timeline renders nothing (`plan::tests::filter_edits_rerender_only_their_clip`). A
reel-only reframe override leaves the landscape render's keys alone
(`plan::tests::reframe_override_is_keyed_per_aspect`). Importing `edit.yaml`, writing the OTIO and
reading it back plans the same keys (`crates/ffcine/tests/render_plan.rs`).

`ffcine.audio.*` effects are left out of segment keys: they belong to the mix.

The **audio key** covers every clip the mix reads (bus, record placement, source start, speed,
crossfades, media fingerprint, its `ffcine.audio.*` effects, `metadata.ffcine.sync`, the hash of
an `arnndn` model), the resolved mix options (`project.mix` with the CLI overrides, the picture's
length), the ffcine version and `AUDIO_REVISION`, and nothing of the picture. So a music swap,
a gain keyframe, a duck, a re-sync or `--lufs` re-mixes and remuxes without re-encoding a frame,
and a grade edit on a camera clip whose audio is the dialogue keeps the mix. The manifest records
the audio key and the mix options next to the segment keys.

A key never includes position, clip name or `metadata.ffcine.id`. A pale clip a reviewer
reported came from a cache keyed by shot position: inserting rows gave five shots other clips'
grades.
`plan::tests::insert_or_remove_changes_no_other_key` guards against that. Inserting two clips or
removing one changes no other clip's key, and the diff renders exactly the two new ones. Two
clips with the same content share one segment, and moving a clip renders nothing. A dissolve's
key includes both sides, so changing a dissolve or its neighbours re-renders only that overlap
and the trimmed shot next to it. Keys don't depend on how the timeline got there:
`edit.yaml` imported to OTIO, written and read back plans the same keys.

## Per-clip filters

Everything is read from the OTIO timeline (effects on the clip, `project.output`), so `ffcine
import` output renders as is; the CLI only overrides. All of it runs on the GPU, as `ffcine grade`
does, with no CPU fallback:

| Effect | In the render |
|---|---|
| `ffcine.fx.stabilize/1` | NVOF motion detect over the clip (cached on disk, shared by every piece of the clip and by `ffcine grade --stabilize`), smoothed, warped in the grade pass with the reframe window folded in. A retimed or ramped clip is analysed on its source frames, mapped to output frames through its speed or ramp. Split-screen panels and overlays aren't stabilized or reframed, and only the main picture is denoised. Freeze frames aren't stabilized |
| `ffcine.fx.denoise/1` | temporal (NVOF or block motion) and spatial NLM denoise on the decoded planes ([denoise.md](denoise.md)). Without the effect the grade's `denoise` number `d` (edit.yaml) maps to temporal strength `d`, radius 2, NVOF, plus spatial `0.2 d` |
| `ffcine.geo.reframe/1` | per-frame source windows (zoom, punch, push, pan, `crop_x`/`crop_y`, keyframes) evaluated over the whole clip, with the `profiles.<aspect>` override and the profile's punch cap |
| `ffcine.time.ramp/1` | the ramp itself (`render.py`'s `ramp_setpts`), not the average speed: NVDEC, `setpts`, `fps`, this piece's frames, lossless NVENC |
| `ffcine.time.blend/1` | `frames` N: each frame is the mean of the last N at the timeline rate (the prototype's `tmix`), a running mean of `blend_vulkan` steps in the retime prep (NVDEC, Vulkan, lossless NVENC). It reads N - 1 frames from before the piece (the source's first frame stands in at its start), so split pieces match an unsplit render. Freeze frames aren't blended |
| `ffcine.color.grade/1` `exposure` | stops, a gain in display-linear light (2.4) ahead of the look hook |
| `ffcine.color.grade/1` `rgb_gain` | per-channel gain on the decoded source ahead of the LUT (`colorchannelmixer` before `lut3d`), folded into the first LUT (an identity one without) |
| `ffcine.color.grade/1` `extra` | ffmpeg filter strings after the look (`extra_light` under the light rule), parsed (`name=k=v:k=v`, positional arguments, comma chains) and run as `PREOUTPUT` hooks in order (`ffcine_render::extra`): `eq`, `hue`, `colorbalance`, `curves` (presets and point lists), `unsharp`, `vignette` (angle). Other filters, options and non-constant expressions are warned and skipped |

Each shot's GPU grade is built with the same code as `ffcine grade --clip` (`looks::grade_for`,
passed in as `Renderer::grader`): `rgb_gain`, the LUT with `lut_strength`, the post-LUT look hook
(exposure, warmth, tint, contrast, gamma, lift, gain, saturation), `extra`, and sharpen.

## Aspects

`--render-profile` (`landscape`, `reel` 9:16, `feed` 4:5, `square` 1:1, `WxH`, `NAME=WxH`) or the
timeline's `project.output.aspect` picks the output frame. Preset aspects render at 1080 px
(preview) or 2160 px (master) on the short side; `project.output.{preview,master}.size` sets
another size when the CLI doesn't override the aspect, and `max_punch` caps punch-ins. Each clip
is cropped to the aspect by its reframe (or centred, `fit: cover`, without one). Text clips take
their `profiles.<aspect>` parameters; when the aspect differs from the timeline's `frame_size`,
text scales with the short side and each text box is moved inside the aspect's safe area
(Reels/TikTok margins for 9:16, the feed grid for 4:5, title safe otherwise).

## Benchmark

Timeline: a 4-minute travel video (`05_project
/edit.yaml` imported). It is 245.7 s and 5892 frames
in 107 segments: shots cut at 70 text clips, freezes, speed ramps, timelapses, 6 dissolves, and
7 split-screens rendered as their first panel. Sources are mixed DJI D-Log M 4K and iPhone HLG,
with 11 audio tracks. Release build on an RTX 5060 Ti, `nice -n 19`. Wall times include planning
and the ffmpeg steps. GPU utilisation from other jobs was 0-17% when each run started.

| Run | Preview (1080p) | Master (4K) |
|---|---|---|
| v1: full render, empty cache | **75.3 s** (107 segments) | 248.9 s |
| v2: one clip's grade changed (`payday` gamma) | **1.7 s** (2 segments, 70 frames) | 12.4 s (segments 3.8 s, join+mux 8.2 s) |
| v3: music swap on v2 | **3.8 s** (0 segments; mix 3.4 s, join+mux 0.3 s) | 21.8 s (mix 9.6 s, join+mux 12.0 s) |
| v4: one caption's text changed on v1 | **1.3 s** (1 segment, 48 frames) | 3.6 s |
| re-run, nothing changed | 0.15 s (manifest matches) | |

The v3 mix times are the plain sum assembly started with; the `ffcine-audio` mix it has now takes
about 12 s on this edit (`ffcine audio`, [audio.md](audio.md)), still with no video encode.

A one-clip edit costs 2-5% of a full render. A music swap costs 6-9%, with no video encode; at
4K, most of that is the stream copy of a 2-3 GB file. Before this, every version re-encoded the
whole master, including two earlier versions of a project that changed only the music. The preview column is the merged build, with the full GPU grade
(LUT + look hook), MP4 segments and the GPU idle. The 4K column is from the build just before it,
with the LUT and sharpen only and MKV segments. The GPU work is the same apart from the look
shader. Earlier, with 4K renders from other jobs pinning the GPU at 94-100%, the 4K
runs took 262 s for the full render, 22.6 s for the edit and 32 s for the music swap.

**Resume**: a preview render was killed with SIGINT after 30 s (37 of 107 segments done) and
then re-run. The re-run took 56.6 s: 37 cached, 70 rendered. Its output decoded cleanly, and
its packets were byte-for-byte the same sizes as the uninterrupted render's.

`ffcine diff` for the same edits:

```
$ ffcine diff v1.otio v2.otio
+   0:18.518    34f  payday                           375529a89f
+   0:19.937    36f  payday                           831d000fd3
2 of 107 segments render (2.9 s of 245.7 s), 2 old dropped; audio same
$ ffcine diff v2.otio v3.otio
0 of 107 segments render (0.0 s of 245.7 s), 0 old dropped; audio changed
picture unchanged: audio remux only (no video encode)
$ ffcine diff v1.otio v4.otio
+   0:00.000    48f  cold                             01d18d69f2
1 of 107 segments render (2.0 s of 245.7 s), 1 old dropped; audio same
```

## Frame rates

Clips are conformed to the timeline rate (`project.rate`) one segment at a time. The pipeline's
`fps` filter picks frames, so 60 -> 30 drops frames, and a `LinearTimeWarp` below 1 is slow
motion from the source's own frames. Each segment has an exact frame count from its own source
time, so errors never add up along the timeline, and 59.94 or 23.976 never drift. Rates map to
exact rationals (`24000/1001`, `60000/1001`).

## Not done yet

- Overlay `rotate_deg` and `anchor` aren't applied (warned); blend modes other than `normal`
  render as `normal`. Overlay layers render at their largest size in the segment and are scaled
  down by the compositor when they shrink.
- Layers are composited in the output's BT.709 SDR space; HDR masters with layers aren't
  handled yet.
- A stabilized or ramped clip split into pieces (text cuts, dissolves) decodes its analysis
  range or ramp from the clip start for each piece; a temporal denoiser starts each piece
  without history.
- **Audio**: the mix is held in RAM and written as a float WAV to `<cache>/tmp` before the AAC
  encode (about 350 MB per bus for 15 min). Stems aren't kept; `ffcine audio --stems` writes
  them.
- Libplacebo logs its Dolby Vision errors from the first try even when the stripped retry
  succeeds.
- Joins, retime prep, blends and mux use the `ffmpeg` CLI (`$FFMPEG`, default `ffmpeg`). They
  could move into the libav backend.
- Chapters and cover (`mp4-atom`), review renditions from proxies.

