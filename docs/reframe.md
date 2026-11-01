# Reframe

Keyframed crop windows for punch-ins, push-ins, pans and vertical deliverables, in the grade pass.
Code: `crates/ffcine-reframe/` (`ffcine::reframe`), the per-frame crop and hook plumbing in
`ffcine-render` (`Grade::crops`, `Grade::place`, `Grade::hooks`). It replaces the prototype's
libplacebo crop expressions (`render.py` `gpu_geo`, `aspect_crop`, `push_filter`).

## Model

A shot renders into a **profile** (the output frame). The reframe effect gives, per output frame,
a **window** in source pixels; libplacebo resamples that window to the profile's size in the same
`pl_render_image` call as the grade.

The window is built in the prototype's order, each step inside the previous one:

1. **Aspect window**: the largest rectangle of the output's aspect inside the source, placed by
   `crop_x` (wider sources) or `crop_y` (taller ones): 0 = left/top edge, 0.5 = centred,
   1 = right/bottom edge. Skipped when the aspects match within 1% (a 16:9 shot into a 16:9
   profile). The prototype only did this for portrait outputs; here any mismatch crops, so a 4:3
   source into 16:9 is cropped instead of stretched.
2. **Zoom / punch**: `zoom` (base, ≥ 1) about `zoom_center`, or the active `punch` (its `zoom`
   capped at the profile's `max_punch`, times the base zoom, about its `center`). Positions are
   fractions of the slack, as `crop=iw/z:ih/z:(iw-iw/z)*cx:(ih-ih/z)*cy`.
3. **Push**: `push [from, to]` eased over the shot (half linear, half cosine, `push_filter`'s
   curve), about `push_center` (else `zoom_center`), or along `pan [[x0, y0], [x1, y1]]` with the
   same easing.

`fit: contain` (or Resolve's `mode: fit`) instead shows the whole (zoomed) picture at the output's
width, centred at `fit_y` (0.42 of the height), black around it.

Zooms below 1 count as 1, so the window never leaves the picture.

### Sub-pixel, no wobble

Windows stay `f64` until they become libplacebo's `pl_rect2df` crop (`f32`, under 0.001 px at 4K).
Nothing is rounded. The prototype truncated the size to even pixels and rounded the position
separately (`crop_round`), so during a push the size stepped 2 px at a time while the position
moved smoothly, and the picture shimmied by up to a pixel. Here position and size come from the same
continuous zoom, and `tests::push_is_smooth_and_keeps_aspect` checks the second differences.

## `ffcine.geo.reframe/1`

Parameters in `metadata.ffcine` (the prototype's `edit.yaml` shot fields, which the importer copies
over unchanged), all optional:

| Field | Value | Default |
|---|---|---|
| `zoom` | number ≥ 1, or keyframes | 1 |
| `zoom_center` | `[x, y]` (0..1 of the slack), or keyframes | `[0.5, 0.5]` |
| `punch` | `[{at, to?, zoom?, center?}]`, seconds from the clip start; `zoom` 1.25 | none |
| `push` | `[from, to]` zoom over the clip | none |
| `push_center` | `[x, y]` | `zoom_center` |
| `pan` | `[[x0, y0], [x1, y1]]`: the push centre's path | none |
| `crop_x`, `crop_y` | number; `[from, to]` (eased pan over the clip); the prototype's `[[t, x], ...]` (eased, held outside); or keyframes | 0.5 |
| `fit` | `cover` (crop to the output aspect), `contain` | `cover` |
| `fit_y` | `contain`: picture centre, 0..1 of the output height | 0.42 |
| `profiles` | `{profile: {any field above}}`: replaces those fields when rendering that profile | none |
| `mode` | Resolve's sizing: `fit` = `contain`, else `cover` | |

Keyframes use the one form, `[{"t": RationalTime, "v": ..., "interp": "linear" | "ease" | "hold"}]`.
`t` is the clip's source time (`source_range.start_time` plus the time since the clip's first
frame, [timeline.md](timeline.md)); the prototype's `[[t, x], ...]` and `punch` times stay seconds
from the clip's first frame. `ease` is `0.5 - 0.5 cos(πu)`, the prototype's pan curve.
The first value holds before the first key and the last after the last key. A keyframed `zoom` and
`zoom_center` give an arbitrary keyframed crop window that still adapts to every profile's aspect,
which is why the window isn't stored as a rectangle.

Additions to `/1` (all optional, so older files still read the same): keyframes on `zoom`,
`zoom_center`, `crop_x`, `crop_y`; the `profiles` map; `fit: cover`; `crop_y` on clips (the
`edit.yaml` importer doesn't copy it yet).

```json
{ "OTIO_SCHEMA": "Effect.1", "name": "reframe", "effect_name": "ffcine.geo.reframe",
  "metadata": { "ffcine": { "schema": "ffcine.geo.reframe/1",
    "push": [1.0, 1.08], "crop_x": 0.5,
    "punch": [{ "at": 2.0, "to": 3.5, "zoom": 1.3, "center": [0.6, 0.4] }],
    "profiles": { "reel": { "crop_x": [0.35, 0.6] }, "feed": { "crop_x": 0.45 } } } } }
```

## Profiles: one timeline, several deliverables

`Profile` is the output frame, picked per render (`ffcine grade --render-profile`; for
`ffcine render`, the timeline's `project.output.aspect` or `--render-profile`,
[assembly.md](assembly.md#aspects)): `landscape`
(`16:9`, 3840x2160), `hd` (1920x1080), `reel` (`9:16`, 1080x1920), `feed` (`4:5`, 1080x1350),
`square` (`1:1`), `WxH`, or `name=WxH`. It also carries `max_punch` (1.4) and an optional safe
area. Rendering the same timeline once per profile gives the 16:9 master and the vertical cuts;
each clip's `profiles.<name>` override adjusts the window for that cut. The edit itself
(clips, timing) stays the same; a reel that is cut differently is a separate timeline.

## Safe area (review)

`Profile::safe_area` adds an overlay hook at `PREOUTPUT` (RGB, after sharpening, see
[hooks.md](hooks.md)). It dims the picture outside the platform safe area and outlines it:

| Aspect | Margins (top, right, bottom, left) | Why |
|---|---|---|
| 9:16 | 11.5%, 11%, 22%, 5.5% | Reels/TikTok/Shorts header, caption and button column |
| 4:5 | 5%, 6.25%, 5%, 6.25% | the profile grid shows the middle 3:4 |
| other | 5% each | title safe |

Only for review renders: it is drawn into the picture.

## With the stabilizer

The stabilizer's warps (`ffcine_core::geom::Mat3`, output pixel -> source pixel, before the crop)
compose with the windows in the same pass (`pass::apply` with non-empty `warps`):

- the image crop becomes the whole frame, so the warp can reach outside the window;
- one `MAIN` hook (`ffcine:geometry`) resizes `MAIN` to the largest window's size and samples the
  source at `warp_i · window_i` (Catmull-Rom, the stabilizer's kernel, black outside the source).
  The nine matrix entries are `//!PARAM`s (`DYNAMIC float`) set per frame, so frame order doesn't
  depend on libplacebo's `frame` counter;
- libplacebo's scaler (`ewa_lanczos` etc.) then takes it to the output size.

That is two resamples (a ~1:1 Catmull-Rom, then the main scaler) against one on the crop-only path.
At whole-pixel offsets the first is exact.

## CLI

```sh
ffcine grade --render-profile reel --reframe '{"crop_x": [0.2, 0.8], "push": [1.0, 1.1]}' \
  --safe-area --adaptive-sharpen 0.8 in.mov reel.mkv
ffcine grade --render-profile feed --reframe @shot12.json in.mov feed.mkv
```

`--reframe` takes the effect's parameters (JSON or `@file`); the shot length comes from `--frames`
or the probed duration, the rate from `--fps` or the stream. A static `--crop` still wins.

## Tests

- `ffcine-reframe` unit tests: prototype equivalence of the zoom crop, push smoothness and aspect,
  punch timing and capping, aspect windows and pans (both keyframe forms agree), keyframed zoom and
  centre, per-profile overrides, contain placement, warp composition, the generated hooks.
- `crates/ffcine/tests/reframe_gpu.rs` (`--ignored`, needs the GPU and `ffmpeg`): luma PSNR of a
  9:16 window against ffmpeg's CPU `crop,scale=lanczos` (55.6 dB), and of the warp path against
  the crop path: identity (82 dB), a half-pixel translation against the moved window (55.7 dB),
  and an animated push and pan frame by frame (≥ 45 dB).

## Not yet

- `fit: contain`'s blurred, darkened fill (`render.py` `contain`): black bars for now.
- A timeline renderer that walks clips and calls `Reframe::from_effects` + `pass::apply` per shot
  (the CLI does it per shot).
- `ffcine.geo.transform/1` / `ffcine.geo.crop/1` from the NLE importers are not applied yet; they
  map onto the same window + `Mat3` machinery.
