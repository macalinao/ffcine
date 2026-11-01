# `.hook` shaders

mpv user shaders run inside the libplacebo grade pass (`pl_mpv_user_shader_parse`, passed as
`pl_render_params.hooks`). Code: `crates/ffcine-hooks/` (`ffcine::hooks`), and the plumbing in
`ffcine-render` (`Grade::hooks`, `Hook`, `ParamTrack`; `backend/placebo.rs` `Hooks::set`).

## Where hooks run

libplacebo's order inside one `pl_render_image` (checked against `src/renderer.c`):

```text
source planes   LUMA / CHROMA / ALPHA hooks   (KrigBilateral: CHROMA)
merge -> RGB, source LUT (the grade's .cube)
MAIN            ffcine:geometry (reframe + stabilizer warp, docs/reframe.md)
LINEAR, SIGMOID, PREKERNEL
main scaler     ewa_lanczos / lanczos (crop -> output size)
POSTKERNEL, SCALED
colour mapping  tone + gamut mapping to the output space
PREOUTPUT       unsharp (compat) / adaptive-sharpen, user shaders, ffcine:safe-area
encode          RGB -> the target's YCbCr
OUTPUT          (encoded YCbCr)
dither
```

`OUTPUT` comes after the encode to the target's colour system. mpv's `OUTPUT` is RGB, so an mpv
shader written for it would, in ffcine (YUV outputs), sharpen chroma and tint the picture. The
first GPU run of the safe-area overlay at `OUTPUT` turned the dimmed area green. So:

- the vendored adaptive-sharpen hooks `PREOUTPUT` (RGB, after scaling and tone mapping, which is
  what "tuned for use post-resize" means);
- shaders loaded through `ffcine.fx.shader/1` get `//!HOOK OUTPUT` rewritten to `PREOUTPUT`
  (`hooks::retarget_output`). Files passed with `ffcine grade --shader` are used as written.

Within a stage, libplacebo runs hooks in array order. `hooks::order` sorts a grade's hooks by
their first stage, then: geometry, sharpening, user shaders (in timeline order, so grain after
sharpening), the safe-area overlay last. The generated unsharp (`Grade::sharpen`) always comes
first.

## Sharpening

`adaptive-sharpen` (bacondither, mpv port by igv) is the default sharpener. It replaces the
generated `unsharp=5:5:amount` hook, which stays for diffs against the prototype:

| Use | API | CLI |
|---|---|---|
| default | `hooks::sharpen(&mut grade, amount, Sharpen::Adaptive)`: `curve_height = amount` | `--adaptive-sharpen 0.8` |
| prototype compat | `Sharpen::Unsharp` (= `Grade::sharpen`) | `--sharpen 0.6` |

`curve_height` and unsharp's amount are different scales. adaptive-sharpen's reasonable range is
0.3–2.0 and it barely halos, so tune by eye. The prototype's `sharpen: 0.6` and `sharpen_up:
0.45` are a starting point, not an equivalent.

### Vendoring and licence

`crates/ffcine-hooks/shaders/adaptive-sharpen.glsl`, from
<https://gist.github.com/igv/8a77e4eb8276753b54bb94c1c50c317e> (version 2021-10-17). It is
BSD-2-Clause: the copyright and licence header is kept in place and unchanged (clause 1). The
changes are listed in a comment below the header: `curve_height` and `overshoot_ctrl` became
`//!PARAM`s, and `//!HOOK OUTPUT` became `PREOUTPUT`. Binary distributions must reproduce the
notice (clause 2). ffcine is private for now; reproduce it in the release notes if that changes.

## `ffcine.fx.shader/1`

```json
{ "OTIO_SCHEMA": "Effect.1", "name": "shader", "effect_name": "ffcine.fx.shader",
  "metadata": { "ffcine": { "schema": "ffcine.fx.shader/1", "shader": "sharp",
    "params": { "curve_height": [
      { "t": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24, "value": 0 }, "v": 0.4, "interp": "ease" },
      { "t": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24, "value": 48 }, "v": 1.0 } ] } } } }
```

- `shader`: an alias in `metadata.ffcine.project.shaders` (`{alias: path}`, relative to the
  timeline, like `luts`), a vendored name (`adaptive-sharpen`), or a path.
- `params`: `//!PARAM` values, as constants or keyframes (`t` in the clip's source time).
  Constants are set once before the first frame. Keyframes become a per-frame track that the
  renderer writes into the parameter's live value (`pl_hook_par.data`) before each frame, which
  works for any `//!PARAM` mode. Names must be `//!PARAM`s of the shader and values must be within
  its `//!MINIMUM`/`//!MAXIMUM`, otherwise the timeline is rejected before rendering.

`ShaderEffect::from_effects(clip.effects)` reads a clip's shader effects in order, and
`ShaderEffect::hook(project, timeline_dir, (frames, rate, start))` turns one into a `Hook`.

## Generated hooks with `//!PARAM`s

`ffcine-reframe` drives its geometry hook the same way: nine `DYNAMIC float` params carry each
frame's matrix. Per-frame data goes in params, not in an embedded `//!TEXTURE` indexed by
libplacebo's `frame` counter (which counts hook passes, not video frames).

## Not yet

- KrigBilateral (LGPL-3.0+), FSRCNNX (GPL-3.0) and ArtCNN (MIT) aren't vendored yet. They load
  today through `ffcine.fx.shader/1` with a path. KrigBilateral hooks `CHROMA` and needs the source
  to be 4:2:0.
- `ffcine.fx.sharpen/1` and `GradeParams.sharpen` aren't resolved into a `Grade` yet, because
  there is no timeline renderer; `hooks::sharpen` is the call it will make.
