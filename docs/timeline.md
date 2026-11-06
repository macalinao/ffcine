# Timeline metadata

The ffcine timeline is plain OTIO JSON ([design.md](design.md), timeline format). This is the
reference for what ffcine puts in `metadata.ffcine` and for its `ffcine.*` effect schemas. The
Rust side is `ffcine-timeline` (model, `HasMetadata`, `Project`, `GradeParams`) and
`ffcine-import`, re-exported as `ffcine::timeline` and `ffcine::import`.

## Model

- Every OTIO object keeps the `OTIO_SCHEMA` it was read with and the fields the model doesn't name
  (`extra`), so a read/write cycle loses nothing. Objects of unknown schemas in `children`,
  `effects` and `media_references` are kept verbatim. Reads apply OTIO's own upgrades only:
  `Clip.1` -> `Clip.2`, `Sequence.1` -> `Track.1`.
- New objects are written at OTIO 0.17-readable versions (`Clip.2`, `Marker.2`, ...).
- A clip's `source_range.start_time` is the media in point; `duration` is its length on the track.
  With a `LinearTimeWarp` the media consumed is `duration × time_scalar`.
- Media paths are `file://` URLs (`ExternalReference::from_path` / `path`).

## `metadata.ffcine`

| Key | On | Meaning |
|---|---|---|
| `v` | timeline | namespace version, `1` |
| `project` | timeline | `ffcine.project/1`, below |
| `frame_size` | timeline | `[w, h]` output frame in pixels as the source app had it; what `"units": "frame"` and text `size` are relative to. Absent when the source has none (Resolve OTIO without `--size`); renders at another size scale with it |
| `id` | clips, stacks | stable id; editorial changes made elsewhere merge back by it |
| `link_group` | clips, stacks | A/V link group (Resolve `Link Group ID`) |
| `label` | clips | human description |
| `source` | clips | `{src, kind}`: the source as written (`dji:0024`) and its kind (`dji`, `video`, `photo`; `image` for overlay stills and stickers) |
| `ai` | clips | AI-generated shot (disclosed) |
| `hold_of` | clips | this clip is the freeze or still ending clip `<id>` |
| `snap` | clips, stacks | `{moved}`: frames the shot's end moved to land its cut on a music beat (`edit.yaml` import) |
| `chapter` | markers | the marker is a chapter |
| `music`, `attribution` | clips | music track name and its credit line |
| `private`, `public_alt` | clips | unlicensed sound for private cuts; the public cut swaps in `public_alt` (path) or drops it (`null`) |
| `foreign.<app>` | any | what an importer didn't map, verbatim ([import.md](import.md)) |
| `schema` + parameters | effects | an `ffcine.*` effect, below |

## `ffcine.project/1`

```json
{ "schema": "ffcine.project/1", "rate": 23.976023976023978,
  "grade": { "lut": "normal", "gamma": 1.0 }, "default_look": "day",
  "looks": { "day": { "warmth": 0.022, "contrast": 0.16 } },
  "luts": { "normal": "/abs/DJI ... .cube" }, "shaders": {},
  "rules": { "light": 0.6, "iphone_tame": { "lut": "...", "rot": 12, "knee": 0.09, "amount": 0.7 } },
  "styles": { "base": {...}, "title": {...}, "card": {...}, "location": {...}, "caption": {...},
              "endcard": {...}, "endscreen": {...} },
  "output": { "aspect": "reel", "preview": { "size": [1080, 1920], "max_punch": 1.12 },
              "master": { "max_punch": 1.25 } } }
```

### `output`

What `ffcine render` renders into ([assembly.md](assembly.md#aspects)); `--render-profile`
overrides `aspect` (and with it the sizes).

| Field | Meaning |
|---|---|
| `aspect` | `landscape` (16:9, default), `reel` (9:16), `feed` (4:5), `square` (1:1), `WxH` or `name=WxH`; the name picks `profiles.<name>` in reframe effects and text parameters |
| `preview`, `master` | per render quality: `size [w, h]` (default: the aspect at 1080 / 2160 px on the short side), `max_punch` (cap on reframe punch-ins, 1.4) |

### `mix`

The timeline's mix settings, read by `ffcine render` and `ffcine audio` (whose flags override
them; [audio.md](audio.md)). Every field is optional; the default is in brackets. Clip effects
(`ffcine.audio.level/1` and the rest) override the clip defaults.

| Field | Meaning |
|---|---|
| `lufs`, `true_peak`, `headroom` | master loudness [−14 LUFS], true-peak ceiling [−1 dBTP], limiter this far under it [2 dB] |
| `normalize` | `false` leaves the master loudness as mixed, still limited [true] |
| `fade_out` | master fade-out at the end, s [0.6] |
| `dialogue_lufs`, `nat_lufs` | R128 targets of `dialogue` [−18] and `nat` [−22] clips |
| `gain_range` | `[lo, hi]` clamp of a dialogue clip's R128 gain, dB [−10, 14] |
| `xfade` | crossfade at dialogue cuts, s [0.125] |
| `duck_db`, `nat_duck_db` | music dip under `dialogue` [−14] and `nat` [−7] clips without their own `duck_db` |
| `duck_attack`, `duck_release` | duck look-ahead and hold, s [0.25, 0.45] |
| `sidechain`, `duck_threshold` | duck only while the dialogue is above `duck_threshold` dBFS [false, −40] |
| `clean`, `clean_nat`, `highpass` | default clean chain of dialogue-bus clips without `ffcine.audio.clean/1`: strength for `dialogue` [0.6] and `nat` [0] clips, high-pass Hz [80] |
| `buses` | bus faders, filters and sends, and the aux buses: [below](#buses) |

### `buses`

A map by bus name: `dialogue`, `music`, `ambience`, `sfx`, or any other name (letters, digits, `-`,
`_`) for an aux bus fed by sends. Worked example in [audio.md](audio.md#bus-gain-filters-and-sends).

| Field | Meaning |
|---|---|
| `gain_db` | fader, dB [0] |
| `filter` | libavfilter chain over the bus (on an aux, after `reverb`) |
| `sends` | not on an aux: `[{to, gain_db, pre}]`, level dB [0], `pre` taps before the fader and ducking [false] |
| `reverb` | aux only: `{room, damp}`, 0..1 [0.5, 0.5] |
| `duck` | aux only, makes it an unheard key bus: `{targets, depth_db, threshold_db, attack, release}`, [−12 dB, −40 dBFS, 0.04 s, 0.35 s] |

### `GradeParams`

All fields optional; a layer only sets what it has. Units follow the prototype's ffmpeg chain
(`render.py` `grade_filters`).

| Field | Meaning |
|---|---|
| `camera` | camera profile (`auto`, `dlog`, `dlog-vivid`, `dlogm`, `dlogm-vivid`, `clog3`, `applelog`, `hdr`, `rec709`): the camera LUT before the grade ([color.md](color.md)); added 2026-11-01 |
| `lut` | alias in `project.luts`, a `.cube` path, or `"none"` |
| `lut_strength` | mix with identity, 0..1 |
| `rgb_gain` | `[r, g, b]` multiplier before the LUT |
| `warmth`, `tint` | colour balance: + warmer / + magenta |
| `exposure` | stops; `ffcine render` applies it as a gain in display-linear light (2.4) ahead of the look |
| `contrast` | S-curve strength, 0 = off |
| `gamma`, `lift`, `gain`, `saturation` | |
| `denoise` | `hqdn3d` strength `d`, 0 = off; `ffcine render` maps it to the GPU denoiser (temporal NVOF at strength `d`, spatial `0.2 d`) unless the clip has `ffcine.fx.denoise/1` |
| `sharpen` | unsharp amount, 0 = off |
| `extra` | raw ffmpeg filters after the grade |
| `extra_light` | replaces `extra` under the light rule |

Unknown fields are kept.

### Layering

```text
project.grade  <-  looks[clip look or project.default_look]  <-  light rule  <-  clip override
```

The light rule applies to display-referred sources (iPhone, AI, stills; everything but log
footage): no LUT, `gamma` 1, `lift` 0, `gain` 1, `denoise` 0, no `exposure`/`rgb_gain`/
`lut_strength`, and `warmth`, `contrast` and `saturation - 1` scaled by `rules.light`
(default 0.6); `extra_light` replaces `extra`. The clip override comes last, as in the prototype.
`Project::resolve_grade` implements it.

## Effect schemas

`Effect.1` with `effect_name = "ffcine.<domain>.<name>"` and the parameters in `metadata.ffcine`
next to `"schema": "ffcine.<domain>.<name>/<major>"`. Times are seconds unless noted. Any
parameter may instead be keyframes, `[{t: RationalTime, v, interp: linear | ease | hold}]`, with `t`
in the clip's source time (like a marker's range: `source_range.start_time` + frames since the
clip's first frame on the track) and `interp` shaping the segment to the next key (also `in`,
`out`, `in_out`, `back`, `smoothstep` or a CSS `[x1, y1, x2, y2]` bezier; `ease` is accepted as
the key's name). Vector parameters (`anchor`, `colour`, ...) keyframe as a whole; scalar ones
(`translate_x`) on their own. A parameter may also be an expression string over the source time
`st` (`"0.5 + 0.1*sin(st*2)"`), an array of constants and expressions for a vector, or
`{"keys": [...], "expr": "value * ..."}` ([compositing.md](compositing.md#expressions)).

Geometry `units`:
- `"picture"`: fractions of the source picture as sized into the frame by `ffcine.geo.reframe/1`
  (Resolve, which stores no frame or source size).
- `"frame"`: fractions of the output frame (`metadata.ffcine.frame_size`); FCPXML, FCP7, MLT,
  CapCut. Crop fractions are always of the source picture, whatever `units` says.

Positions are always y down, offsets from the middle; angles counter-clockwise on screen.

| Schema | Parameters |
|---|---|
| `ffcine.color.grade/1` | `look` + any `GradeParams` field (the clip override) |
| `ffcine.fx.denoise/1` | `preset` (`night`, `snow`: starting values the other fields override one by one, [denoise.md](denoise.md#presets)), `luma`, `chroma` (spatial NLM strengths, 8-bit code values; chroma defaults to 1.5x luma), `low_light` (bigger NLM search), `protect` (spatial point protection), `temporal: {radius (1-5, 2), motion (`nvof` | `blocks`), strength (3), chroma, protect}` or `null` for off (motion-compensated temporal denoise, [denoise.md](denoise.md)); read by `ffcine_filters::effect::denoise` |
| `ffcine.fx.grain/1` | `strength` (sigma at mid-grey, 8-bit code values, 3), `size` (cell size in output pixels, 1.5), `chroma` (0), `profiles {name: {fields}}` overriding them per render profile (`preview`, `master`); read by `ffcine_filters::effect::grain_for`. On a clip, or on the timeline's top-level stack (`tracks.effects`) for the whole picture after everything is composited |
| `ffcine.fx.vignette/1` | `amount`: ffmpeg `vignette` angle = `amount`·π/2 (`render.py`) |
| `ffcine.fx.darken/1` | `amount`: RGB multiplier (`colorchannelmixer rr=gg=bb=amount`) |
| `ffcine.fx.fade/1` | `in`, `out` (s from the clip's first / to its last frame; absent = none), `colour` `[r, g, b]` 0..1 to fade from/to (black) |
| `ffcine.fx.flash/1` | `at` (s from the clip's first frame), `levels [l0, l1, ...]`: on frame k from `at`, out = l_k + (1 − l_k)·in (ffmpeg `colorlevels romin`), a shutter flash |
| `ffcine.fx.stabilize/1` | `smoothing` (20), `shakiness` (6), `accuracy` (15), `zoomspeed` (0.25); ffcine's GPU stabilizer reads `smoothing`, `zoom` (`"auto"` or a factor) and `max_zoom` ([stabilize.md](stabilize.md#in-the-grade-pass)) and ignores the vidstab-only rest |
| `ffcine.geo.reframe/1` | prototype fields, same meaning as in `edit.yaml`: `zoom`, `zoom_center`, `punch [{at, to, zoom, center}]`, `push [from, to]`, `push_center`, `pan`, `fit` (`cover`, `contain`), `fit_y`, `crop_x`, `crop_y`, `focus` (panels); `zoom`, `zoom_center`, `crop_x`, `crop_y` may be keyframes; `profiles {name: {fields}}` overrides per render profile ([reframe.md](reframe.md)) |
| `ffcine.geo.reframe/1` (Resolve) | `mode`: `fit`, `fill`, `stretch`, `crop` (one source pixel per timeline pixel at `timeline_size [w, h]`); sizes the picture that crop and transform are relative to |
| `ffcine.geo.crop/1` | `units`; `left`, `right`, `top`, `bottom` as fractions of the source picture, cut before the transform; `feather` in picture widths; `fill: true` scales the cropped picture back up to the frame (FCPXML/FCP7 crop mode) |
| `ffcine.geo.transform/1` | `units` (`picture` or `frame`), `y: "down"`; `anchor [x, y]` (pivot, offset from the picture's middle), `translate_x`, `translate_y` (offsets from the frame's middle), `scale_x`, `scale_y` (1 = unscaled), `rotate_deg` (counter-clockwise), `flip_x`, `flip_y`. Picture point q lands at A + R·S·(q − A) + T ([`import::resolve`](../crates/ffcine-import/src/resolve.rs)) |
| `ffcine.comp.blend/1` | `opacity` 0..1, `radius` (rounded corners, fraction of the frame height, 0), `mode`: one of Resolve's 27 names in `import::resolve::BLEND_MODES` (`normal`, `add`, `subtract`, `difference`, `multiply`, `screen`, `overlay`, `hardlight`, `softlight`, ..., `color_dodge`, `darker_color`); a mode with no equivalent is `<app>:<raw>` (`qt:7`) and renders as `normal` |
| `ffcine.color.cdl/1` | `slope`, `offset`, `power` (`[r, g, b]`), `saturation`: ASC CDL on the clip, before the look. Mirrored as `metadata.cdl = {asc_sop: {slope, offset, power}, asc_sat}` on the clip (the CMX adapter's shape), which exporters write; on read, the effect wins |
| `ffcine.fx.shader/1` | `shader` (alias in `project.shaders`, vendored name, or path), `params {name: value or keyframes}` setting its `//!PARAM`s ([hooks.md](hooks.md)) |
| `ffcine.audio.pan/1` | `pan` −1 (left) .. 1 (right) |
| `ffcine.audio.clean/1` | `amount` (alias `strength`, 0.6), `highpass` (80 Hz), `wind`, `declick`, `denoise` (`afftdn`, `arnndn` + `model`, `off`), `dialogue` ([audio.md](audio.md)) |
| `ffcine.audio.level/1` | `mode` (`dialogue` alias `orig`, `nat`, `music`, `ambience`, `sfx`, `off`), `lufs`, `ref_dbfs`, `gain_db`, `nat_db`, `duck_db`, `fade_in`, `fade_out` |
| `ffcine.audio.loop/1` | `in`, `out` (s, media time), `crossfade` (1 s) |
| `ffcine.audio.sync/1` | not an effect: an audio clip's `metadata.ffcine.sync` from `ffcine sync`, `video` (camera clip id), `offset`, `drift_ppm`, `confidence` |
| `ffcine.transition/1` | flat in a `Transition`'s own `metadata.ffcine` (not an effect): `schema`, `kind` (`dissolve`, `cross_fade` + `crossfade_db`, `wipe` + `angle_deg`, `dip` + `colour` (black), `flash` + `colour` (white: a dip to a light colour), `whip` + `direction` (`left`, `right`, `up`, `down`: the way the picture moves; a motion-blurred push), or the source's name for anything else, rendered as a dissolve), `resource` (wipe matte file), `ease` (`in`, `out`, `in_out`). Absent = dissolve. `transition_type` stays `SMPTE_Dissolve` so other OTIO readers take it |
| `ffcine.time.ramp/1` | `points [[u, speed]]` piecewise linear over the source range (u 0..1), `source_duration`; the clip also gets a `LinearTimeWarp` with the average speed for other readers. `ffcine render` retimes through the ramp itself |
| `ffcine.time.blend/1` | `frames`: temporal blend (tmix) |
| `ffcine.comp.layout/1` | on a split-screen `Stack` (one track per panel): `layout` (`split` or `row`: side by side, as `render.py`; `column`: stacked; `1+2`: the first panel the left half, the other two stacked on the right), `gap`, `margin`, `radius`, `weights`, `stagger`, `slide`, `enter`, `first_static`, `audio_panel` |

An audio track's bus is `metadata.ffcine.bus`: `dialogue`, `music`, `ambience` or `sfx`.

## Generators

Clips whose media is a `GeneratorReference`; the parameters sit in `parameters` (no `schema` key,
the `generator_kind` names it). Anything a reader couldn't map goes to the generator's
`metadata.ffcine.foreign.<app>`.

### `ffcine.text`

Subtitles, cards, location stamps, titles, end cards. Every parameter but `text` is optional and
overrides the `style`.

| Parameter | Meaning |
|---|---|
| `text` | UTF-8, `\n` for line breaks |
| `style` | name in `project.styles` (the base the rest override) |
| `font`, `face` | family (`Helvetica Neue`) and face (`Bold`) |
| `size` | font size in px at `frame_size` (scaled with the render size) |
| `colour` | `[r, g, b, a]` sRGB 0..1 |
| `position` | `[x, y]` middle of the text box as an offset from the frame's middle, fractions of the frame, y down |
| `alignment` | `left`, `center`, `right` |
| `bold`, `italic` | booleans |
| `animation` | `{in: {kind, duration, ...}, out: {...}}`, `kind` `fade`, `slide`, `pop`, `typewriter`, `wipe`, `none`, or the source's name (rendered as a fade, kept); seconds. Options: [text.md](text.md#animation) |
| `keyframes` | `{position, scale, opacity, colour}`: `[{t, v, interp}]` per parameter ([text.md](text.md#keyframes)) |
| `profiles` | `{name: {parameters}}`: overrides for one output aspect (`reel`, `feed`, `square`, ...), merged over the rest when rendering it; also allowed in `project.styles` |

Rendering, the style cascade (`base` <- `style` <- `based_on` chain <- clip) and the additive
style keys (`anchor`, `font_file`, `shadow`, `outline`, `box`, `highlight`, `fade_in`, ...):
[text.md](text.md).

### `ffcine.title`

A title sequence or end screen: `kind` (`title`, `end`), `title`, `subtitle`, `lines`, `style`,
`title_style` / `subtitle_style` / `lines_style`, `rule`, `background`, `stagger`, `duration`,
`fade_out`, `position`, `alignment`, `gap`. Rendered as `ffcine.text` pieces:
[text.md](text.md#titles-and-end-screens). `ffcine title` adds one.

### `ffcine.solid`

Solid colour: `colour [r, g, b, a]`, sRGB 0..1.

### `ffcine.comp`

A composition ([compositing.md](compositing.md)) as a clip on the picture track. Unlike the other
generators its parameters carry `schema`:

| Parameter | Meaning |
|---|---|
| `schema` | `ffcine.comp/1` (required) |
| `src` | path to a comp file (`.comp.json`, `.comp.yaml`), relative to the timeline |
| anything else | without `src`, the parameters are the comp document itself, inline (`canvas`, `layers`, `template`, ...; relative media resolve against the timeline) |

The clip's `source_range` is comp time (frame 0 is the comp's start, and a start before it is an
error; past its end is black), and
the comp's `canvas.fps` must be the timeline's rate. Its pieces render as segments like a comp
file's ([assembly.md](assembly.md#keys)), scaled to the timeline's output size, so the canvas
must have the output's aspect (else it's an error). The timeline's audio tracks are its sound.
On another video track, in a split-screen panel or next to a transition it isn't composited yet
(warned: left out, or a cut). Its effects (grade, transform, blend, fades) are not applied yet
(one warning per effect), and the timeline's grain isn't drawn on it:
[compositing.md](compositing.md#not-yet).

## `edit.yaml` import

`ffcine import edit.yaml -o edit.otio`. Tracks, bottom first: `V1` (shots), `PiP N` (per-shot
`overlays`), `Overlays N` (title, cards, locations, end card/screen), `Captions N`, `Dialogue N`,
`Ambience N`, `Music N`, `SFX N`; overlapping items spill onto further tracks of the same kind.

- Shot timing is `render.py`'s frame maths: dissolves overlap, ramps integrate, freezes and Live
  Photo stills add hold clips, and cuts snap to the music's beats (`music.snap_cuts`, default on;
  `snap_max` 0.45 s; per shot `snap: false`; `music`/`nat` shots with a next shot; the beats of
  the cue playing at the cut, cues being timed with the shots). A snap moves the hold's end, or
  the end of a shot without one (more or less of its source); the shot gets
  `metadata.ffcine.snap`. Cards and locations land on the nearest beat within 0.25 s, so do the
  title's `*_at` times when `snap_to_beats` (the snapped ones go on the title clip), and
  `snap_in` cues start on the beat nearest `in`. Beats come from `ffcine_audio::beats`
  ([audio.md](audio.md#beats)) through `ImportOptions::beats`; without them (or for a track that
  has none) cuts land unsnapped, with a warning. Sample edits convert to the prototype's
  exact shot starts, lengths, overlay and cue timing.
- Transitions: `dissolve: s`, or `transition: dissolve | dip | flash | whip` or
  `{kind, colour, direction, dur}`, lasting `dur`, else `dissolve`, else 0.5 s (dissolve, dip),
  0.2 s (flash), 0.25 s (whip). The outgoing item is trimmed by the duration and a
  `SMPTE_Dissolve` with `in_offset 0`, `out_offset = duration` follows it, so the mix uses the
  outgoing shot's last frames as `render.py` does; kinds other than a dissolve carry
  `ffcine.transition/1` (`colour` `black`, `white` or `[r, g, b]`).
- Finishing, as `render.py` runs it over the whole shot: `darken` (`ffcine.fx.darken/1`) and
  `vignette` (`ffcine.fx.vignette/1`) on the main clip and the hold; `fade_in` on the main clip,
  `fade_out` on the hold if there is one (`ffcine.fx.fade/1`, black). Split screens take them on
  the layout `Stack`. `flash: true` on a freeze puts `ffcine.fx.flash/1 {at: 0, levels: [0.5,
  0.5, 0.2]}` on the hold; a Live Photo still always gets `[0.55, 0.55, 0.22]`.
- `grain: {preview, final}` (ffmpeg `noise` uniform amplitude, chroma 0.4x) becomes
  `ffcine.fx.grain/1` on the top-level stack: `strength` = amplitude / √3 (the preview's),
  `chroma` 0.4, `profiles: {preview, master}`.
- `overlays` on a shot: `image` (png/jpg/webp) or `src` (video, from `in`, optional `look`), `at`
  (s after the shot's start, 0), `dur` (s, the rest of the shot), `position [x, y]` (the
  overlay's middle as an offset from the frame's middle, frame fractions, y down), `scale` (1 =
  the picture fitted inside the frame), `opacity`, `radius` (corners, fraction of the frame
  height), `fade_in`, `fade_out` (s). `position`, `scale` and `opacity` may be keyframes,
  `[[t, v], ...]` or `[{t, v, interp}]` with `t` in s from the overlay's start. Each becomes a
  clip `<shot>/ov<k>` on a `PiP N` track (source kind `image` or `video`) with
  `ffcine.geo.transform/1` (`units: frame`, `translate_x/y`, `scale_x/y`; keyframes in source
  time) and `ffcine.comp.blend/1` (`opacity`, `mode: normal`, `radius`); fades become opacity
  keys.
- Chapters (`chapter`, or a capitalised `card`) are markers on `V1`.
- Mix settings go to `project.mix`: `audio` (`dialogue_lufs`, `nat_lufs`, `xfade`, `highpass`,
  `clean`, `clean_nat`), `music` (`duck_db`, `nat_duck_db`, `duck_attack`, `duck_release`) and
  `output.audio` (`lufs`, `true_peak`). Per-shot `gain_db`, `duck_db`, `nat_db`, `clean`, music
  cue gains and fades, ambience levels and SFX gains are clip effects. An ambience bed runs
  `pre` and `post` past its shots ([0.3] s each, within the edit), its `level_lufs` and `fade` go to
  `ffcine.audio.level/1`, and its `highpass` and `lowpass` to `ffcine.audio.clean/1`.
- Paths are made absolute against the `edit.yaml` directory.
- `output.preview`/`output.final` `width`, `height` and `max_punch` become `project.output`
  (`aspect` from the preview size, `final` -> `master`), and the preview size becomes
  `metadata.ffcine.frame_size` (what text sizes are relative to) unless `--size` is given.
- Per shot: `stabilize` (or `stabilise`: `true`, a smoothing number, or an object) ->
  `ffcine.fx.stabilize/1`; `denoise` (an `ffcine.fx.denoise/1` object, or a number: the temporal
  strength) -> `ffcine.fx.denoise/1`; `zoom`, `zoom_center`, `punch`, `push`, `push_center`,
  `pan`, `fit`, `fit_y`, `crop_x`, `crop_y` -> `ffcine.geo.reframe/1`; `ramp` ->
  `ffcine.time.ramp/1`; `look` and `grade` (including `exposure`, `denoise`) ->
  `ffcine.color.grade/1`.
- Not mapped (kept in `foreign.edit_yaml` and listed in the report): the rest of `output` (but
  `output.audio`), `sfx.duck_files`/`public_map`, `review`, `youtube`, `ingest`, `sources`,
  `cache_dir`; per shot `blur`, `print`, `sharpen_up`, `still_push`; skipped
  shots.
