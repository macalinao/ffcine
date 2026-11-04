# Compositing and keyframes (design)

Status: design. Steps 1 and 2 of the [phased plan](#phased-plan) (param tracks and expressions,
`ffcine_core::anim`; the compositor core, `ffcine-comp`) are built; the rest is not. It turns what assembly already composites (split-screens,
picture-in-picture, transitions, stills, text) into one general model: a **composition** of
ordered **layers** whose parameters are **tracks**. Anything one would write today as a
hand-made ffmpeg filter graph should be expressible in ffcine itself, and layouts and
templates are sugar that lowers to it.

The yardstick is a 9:16 story: a 1080x1920 canvas, three 1080x632 tiles with 12 px gutters, mixed
stills and video. The panels fade in from black 0.25 s apart and the background animates from
black to white. Each panel swaps its picture with a slide, 0.12 s after the panel above it.
Stills get a Ken Burns 1.0 -> 1.06, there is a 0.15 s punch-in, and one panel has a
1x -> 2x -> 1x speed ramp. At the end the panels slide out and the frame fades to black, so the
clip loops. Log footage is graded with its camera LUT next to phone HEIC stills.
[Worked example](#worked-example-the-story).

What exists and is reused (read before changing anything):

| Piece | Where | Role here |
|---|---|---|
| `Layer { source, grade, from, to, rects, rotation, blend }` | `ffcine-render/src/layer.rs` | the GPU layer; each comp layer lowers to one |
| per-layer `pl_render_image` into an RGBA texture, then `pl_overlay` or the composite pass (ping-pong, `ffcine_render::blend`, rotation, HDR) | `backend/gpu.rs` | the renderer, unchanged in shape |
| `Overlay`, `Mask`, `Motion` (text masks, per-frame placements) | `overlay.rs`, `ffcine-text` | text layers |
| `Hook` + `ParamTrack` (per-frame `//!PARAM` values) | `grade.rs`, `backend/placebo.rs` | how every animated value reaches the GPU |
| `shaders::{alpha, finish, background, look, motion_blur}` | `shaders.rs` | opacity/radius, fades, canvas background, grade, whip |
| `extra` parser (`eq`, `hue`, `colorbalance`, `curves`, `unsharp`, `vignette` as hooks) | `extra.rs` | per-layer filters on the GPU |
| `ffcine_reframe::{Keyed, Interp}`, `ffcine_text::anim::{Ease, Keys}`, `ffcine_audio::schema::Interp` | three keyframe implementations | merged into one in [Param tracks](#param-tracks) |
| `comp::{Placement, Layout, Finish}`, `plan::{Unit, Shot, Split, SegLayer, Ramp}`, `exec::Renderer::{split, overlay_layer, input_ctx}` | `ffcine-assembly` | what the existing features lower through |
| `TileGrid` (HEIF grids, `clap`, `irot`) | `backend/tiles.rs` | HEIC stills |
| retime prep (NVDEC, `setpts`/`fps`, lossless NVENC) | `exec::Renderer::input_ctx` | time remapping |
| segment keys, `RENDER_REVISION` (8) | `recipe.rs`, `plan.rs` | caching of comps |

## Decisions in short

- **One model.** `ffcine.comp/1` is a canvas plus layers, bottom first. The OTIO timeline is
  lowered into it per segment: the existing units become comps and keep their keys and pixels. A
  comp can also be a standalone file or a generator clip on a timeline.
- **One track type** for every numeric, vector and colour parameter: a constant, keyframes with
  easing, or an **expression**. It lives in `ffcine-core` and replaces the three keyframe
  implementations. Tracks are sampled on the CPU per frame into the per-frame vectors and
  `ParamTrack`s the GPU code already takes.
- **Expressions** are our own small pure-Rust evaluator with ffmpeg's function names. It is
  compiled once, deterministic, a pure function of time, and has no state, so it never needs
  pre-roll.
- **Rendering keeps today's shape.** Each layer is one `pl_render_image` into its own RGBA
  texture, with its grade, camera LUT, reframe and hooks in that pass. The layer is then drawn
  as a `pl_overlay`, or by the composite pass when it is rotated, blended or HDR. A nested comp
  renders in-pass into its own texture. The bottom full-frame video is promoted to the pass
  input, as the main picture is today.
- **Time remapping** is a per-layer source-time map, and speed, ramps, freezes and reverse are
  sugar for it. The renderer picks frames through a frame map; reverse reads an all-intra
  intermediate.
- **libavfilter escape hatch** per layer, run in the prep step with a PTS offset so `t` matches
  an unsplit render. Stateful filters get pre-roll, post-roll or whole-clip, from a known table
  plus overrides, and everything the node reads goes into the key.
- **Keys** of comp segments hash the sampled per-frame values over the piece, not the track
  definitions. Rewriting keyframes as an equivalent expression re-renders nothing.

## Layer model

### Composition

```text
Comp   = { canvas, vars, project?, comps{name: Comp}, layers: [Layer] (bottom first) }
canvas = { size [w, h] px, fps (rational), duration s, background (colour track),
           fade (track, 1 = picture), fade_colour, grain? }
Layer  = { id, source, in, out, source_in, time?, size?, fit, content?, crop?,
           transform, radius, mask?, blend, fade, fade_colour, grade?, filter?, vars }
```

- **Coordinates**: canvas pixels, origin top left, y down, angles counter-clockwise on screen
  (the timeline's convention, in pixels rather than fractions, since a layout is specified in
  pixels: "12 px gutters"). A render at another size (a 2160x3840 master of a 1080x1920 comp)
  scales every length by the render/canvas ratio. Lowering from OTIO converts `units: frame` /
  `picture` fractions into pixels.
- **Time**: seconds. A layer is visible for comp times `in <= t < out` and shows source time
  `source_in` at `in` (then its [time map](#time-remapping)). Defaults: `in` 0, `out` the
  canvas duration, `source_in` 0.
- **Z-order** is array order, bottom first, as tracks are today (`PiP N` bottom track first).
  There is no separate `z`: a reorder is a different array, which keys handle (z is the index).

### Sources

| `source` | What | Renders as |
|---|---|---|
| `{"video": PATH}` | any file libav decodes; NVDEC where possible | `LayerSource::Video` |
| `{"still": PATH}` | PNG, JPEG, WebP, HEIC/HEIF (tile grids, `clap`, `irot`), decoded once | `LayerSource::Image`, colour-managed (below) |
| `{"solid": COLOUR}` | a colour track (`[r, g, b, a]` sRGB 0..1, `"black"`, `"white"`, `"#rrggbb"`) | a 1x1 image stretched to the box, tinted per frame in the alpha hook |
| `{"text": {...}}` | `ffcine.text` parameters exactly as on a generator clip ([text.md](text.md)) | text masks (`Overlay`); a layer only if something is above it ([Rendering](#rendering)) |
| `{"comp": NAME or {...}}` | a nested comp (from `comps` or inline), with its own canvas (`fps` and `duration` default to the parent's, `size` is the layer's default box) and its own clock (0 at the layer's `source_in`) | rendered in-pass into its own RGBA texture |

Paths are relative to the comp file (or to the timeline that embeds it). Stills decoded by libav
carry their tags: an iPhone HEIC is Display P3 with an sRGB curve, and libplacebo converts it to
the output's gamut. PNG/JPEG without tags keep today's behaviour (their values are taken as the
output's space for SDR, sRGB at 203 nits for HDR), so a logo's colours don't move. A layer is
decoded through `backend/io.rs` whatever the extension, so `TileGrid` assembles HEICs and
`.webp`, `.avif` and HEIC all work without the `image` crate.

### Placement

```text
box       = size [w, h] (default: the canvas) — the layer's rectangle before transform
fit       = cover (default) | contain | fill | none   — how the source fills the box
content   = ffcine.geo.reframe/1 parameters (zoom, zoom_center, punch, push, pan, crop_x, crop_y,
            focus, fit_y), with the box as the profile: the picture *inside* the box
crop      = {left, right, top, bottom, feather} fractions of the source (ffcine.geo.crop/1)
transform = {position [x, y] (where the anchor lands, px; default: canvas middle),
             anchor [ax, ay] (fraction of the box, default [0.5, 0.5]),
             scale (number or [sx, sy]), rotation (deg), opacity (0..1)}
radius    = corner radius, px of the box
mask      = {shape: rect | ellipse, rect [x, y, w, h] (box px), feather px, invert}
blend     = one of BlendMode's 27 Resolve names (ffcine.comp.blend/1)
fade      = 1 = picture, 0 = fade_colour (the finish hook's fade), so "fade in from black"
            doesn't reveal what is under the layer
```

`fit: contain` and `fit: none` draw the picture centred inside the box (smaller than it), and
`radius` and `mask` then apply to that picture; with `none` a source pixel is a canvas pixel
and the part outside the box is cropped. `scale` below 0 is taken as 0 (no flips yet). A mask's
`rect` defaults to the whole box, and `feather` is the width in box pixels of its soft edge,
centred on the shape's edge.

`content` and `transform` are kept apart on purpose. A Ken Burns or a punch-in zooms the
picture inside a fixed tile: that is `content.zoom`, one sub-pixel source window
(`Grade::crops`, no wobble, [reframe.md](reframe.md#sub-pixel-no-wobble)). Scaling the tile
itself is `transform.scale`. A box point `q` (px from the anchor) lands at
`position + R·S·q`, which is `Placement::rect`'s `A + R·S·(q − A) + T` with the anchor folded
into the position.

Masks are the cheap kind only: shapes in the layer's own alpha hook (`shaders::alpha` already
does opacity and the rounded rectangle; ellipse, feather and invert are a few lines more), so a
mask costs nothing extra. Track mattes, where one layer's alpha or luma cuts another, are
[Not yet](#not-yet).

### Limits

`ffcine comp check` (and `render`, `still`) reject a comp past these before any GPU work,
naming the parameter (`ffcine_comp::limits`):

| what | limit |
|------|-------|
| canvas `size` | at most 16384 px a side and 8192x8192 px in all; the long side at most 4 times the short |
| canvas `fps` | 1 to 240 |
| canvas `duration`, layer `in`, `out`, `source_in` | 0 to 3600 s (`duration` > 0) |
| `layers` | at most 64, of which at most 16 `video` (each is a decoder while it shows) |
| layer `size` | at most 65536 px a side |
| a layer's texture (its picture at its largest `scale`, in render px) | at most 16384 px a side, so a big `scale` fails at the render size that needs it |
| `transform.position`, `anchor` | finite, at most 1e9 either way |
| `transform.rotation` | finite, at most 1e6 degrees either way |
| layer `id` | a string, unique in the comp |

The render size a profile plans for a canvas is at most 16384 px a side.

### Serialisation

**Standalone recipe**: `NAME.comp.json` (or `.comp.yaml`, the same serde model;
`yaml_serde` is already a workspace dependency):

```json
{ "schema": "ffcine.comp/1",
  "canvas": { "size": [1080, 1920], "fps": "30", "duration": 6.0, "background": "black" },
  "layers": [ { "id": "a", "source": { "video": "a.mov" }, "grade": { "camera": "auto" } } ] }
```

- `schema` is versioned like every `ffcine.*` schema: a strict reader rejects unknown versions,
  a lenient one skips them with a warning. The file is hand-written, so unknown keys are
  errors (typos must not render silently). The exception is `x_*` keys, which are kept and
  ignored.
- `project` is optional: an inline `ffcine.project/1` (looks, LUT and shader aliases, rules), or
  `"project": "edit.otio"` to borrow a timeline's. Grades layer as in timelines
  ([timeline.md](timeline.md#layering)).
- `template` instead of `layers`: [Templates](#templates).

**In OTIO** a comp is a generator clip, like `ffcine.text`, `ffcine.title` and `ffcine.solid`:

```json
{ "OTIO_SCHEMA": "Clip.2", "name": "story",
  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 30, "value": 0 },
    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 30, "value": 180 } },
  "media_references": { "DEFAULT_MEDIA": { "OTIO_SCHEMA": "GeneratorReference.1",
    "generator_kind": "ffcine.comp",
    "parameters": { "schema": "ffcine.comp/1", "src": "story.comp.json" } } },
  "active_media_reference_key": "DEFAULT_MEDIA",
  "metadata": { "ffcine": { "id": "story" } } }
```

`parameters` is either `src` (a file) or the comp inline. `source_range.start_time` is comp time,
so trimming works like any clip. The clip's effects (`ffcine.color.grade/1`,
`ffcine.geo.transform/1`, `ffcine.comp.blend/1`, fades) apply to the comp as one layer. Other
OTIO readers see a generator clip of the right length. The comp is not spread over OTIO tracks
and Stacks: OTIO has no per-layer keyframes, other tools couldn't render our effects anyway, and
deeply nested Stacks would be unreadable.

**The timeline is a comp too.** The planner already turns each segment into a main picture plus
layers. After the refactor it builds a `Comp` per segment: the canvas is the profile size and
rate, and the layers are the shot, the transition sides, the split panels, the overlay clips,
the comp clips and the text ([Mapping](#mapping-of-existing-features)).

### CLI

```sh
ffcine render story.comp.json -o story.mp4             # by schema: a comp, not a timeline
ffcine render --profile master -o story-4k.mp4 story.comp.json
ffcine still story.comp.json --at 2.6 -o f.png
ffcine comp check story.comp.json      # parse, compile expressions, sample every frame, report
ffcine comp lower story.comp.json      # templates expanded: the explicit layers, as JSON
ffcine comp new --template story --media a.mov b.heic c.mov d.heic e.mov f.heic -o story.comp.json
```

Built so far: `render`, `still` and `comp check` (step 2); `comp lower` and `comp new` come
with templates (step 7). `render` and `still` take a comp wherever they take a timeline (`still` composites every
layer, where today's still of a timeline draws the top-most clip and the text). It goes through the same
planner, segment cache, profiles and manifest. A comp has no audio of its own yet
([Not yet](#not-yet)); as a clip in a timeline, it uses the timeline's audio tracks.

## Param tracks

### One type

```rust
// ffcine-core::anim (no GPU, no libav; serde_json is already a dependency)
pub enum Track<T: Animatable> {
    Const(T),
    Keys(Vec<Key<T>>),                    // sorted; before the first / after the last: hold
    Expr(Vec<Compiled>),                  // one per component
    KeysExpr(Vec<Key<T>>, Vec<Compiled>), // expression over its own keys (`value`)
    Sampled(Vec<T>),                      // per frame, internal only (exact lowering)
}
pub struct Key<T> { pub t: f64, pub v: T, pub ease: Ease }
pub enum Ease { Linear, Hold, Ease, In, Out, InOut, Back, Smoothstep, Bezier([f64; 4]) }
pub trait Animatable: Copy { const N: usize; fn lerp(a: Self, b: Self, k: f64) -> Self; ... }
// f64, [f64; N]
impl<T> Track<T> {
    pub fn parse(v: &Value, clock: Clock, what: &str) -> Result<Self>;
    pub fn parse_in(v: &Value, scope: &Scope, what: &str) -> Result<Self>; // clock + vars
    pub fn eval(&self, ctx: &Frame) -> Result<T>;                          // keys on `lt`
    pub fn try_eval(&self, clock: Clock, ctx: &Frame) -> Result<Option<T>>;
    pub fn at(&self, clock: Clock, t: f64, dur: f64) -> Option<T>;         // timeline effects
    pub fn sample(&self, clock: Clock, frames: Range<i64>, env: &Env) -> Result<Vec<T>>;
}
```

- `Ease` is text's set, which already contains reframe's (`linear`, `ease` the cosine, `hold`)
  with the same maths. On top of those it has `in`, `out`, `in_out` (cubic), `back` and CSS
  `cubic-bezier` `[x1, y1, x2, y2]` (`x1` and `x2` in 0..1, as in CSS). Audio's `ease` is a different curve, smoothstep
  (`3u² − 2u³`), so `Ease` also has `smoothstep`, and the audio schemas keep mapping their
  `ease` to it: changing it would move existing level automation. A key's `ease` shapes the
  segment to the next key. The JSON key is `interp`, as everywhere now; `ease` is accepted as an
  alias (text accepts it already).
- `Clock` says what `t` means. That is what differs between today's schemas, and each keeps its
  meaning: reframe, transform, crop and shader keys use the clip's source time; text keys use
  seconds from the clip's start (negative counts from its end); a comp layer's keys use seconds
  from the layer's `in` (negative from its `out`). A comp layer's clock is not its source
  time, so a speed ramp doesn't warp its animation. The clock also says which
  [variables](#expressions) an expression sees: a comp layer's (`Clock::Layer`) sees all of
  them. Until the timeline planner lowers through comps (step 7), a timeline effect's expression sees
  only its own clock's time: `st` on source-time schemas (`Clock::Source`), `lt` and `dur` on
  text (`Clock::Clip`). Any other name is a compile error, so nothing changes meaning later.
- Sampling is at the frame's start time, `i / fps`, as reframe and ffmpeg's expressions do.
  `Sampled` exists so lowered features (a dissolve's `comp::progress`, sampled at the frame
  middle) keep their exact values.
- `ffcine_reframe::Keyed`/`Interp`, `ffcine_text::anim::Keys`/`Ease` and
  `ffcine_audio::schema::Interp` become re-exports or thin wrappers. `Keyed` keeps its `Const`
  and `Keys` variants (matched on across the workspace) and gains `Track` for expressions;
  `ffcine_audio::schema::Param` gains `Expr`, which serialises back to the JSON as written. `ParamTrack` stays as the
  render side's sampled form (per-frame `f32`).
- Vector and colour params animate as a whole (keys of `[x, y]`), or per component with an
  array whose items are constants or expressions (`[0, "H*0.4 + 20*sin(t)"]`). A colour string
  (`"black"`, `"#rrggbb"`) is a constant colour, never an expression.

### What becomes animatable

Everything numeric that is per frame on the GPU or the CPU sampler; nothing that changes
allocation or decoding.

| Animatable (track) | Held per layer (constant) |
|---|---|
| `transform.*`, `radius`, `mask.*`, `fade`, `fade_colour`, `crop.*` | `source`, `size`, `fit`, `blend` mode |
| `content.*` (zoom, centre, crop_x/y; punch/push are sugar for zoom keys) | `in`, `out`, z-order |
| canvas `background`, `fade`, `fade_colour` | canvas `size`, `fps`, `duration` |
| grade look params: `exposure`, `warmth`, `tint`, `contrast`, `gamma`, `lift`, `gain`, `saturation`, `rgb_gain`, CDL `slope`/`offset`/`power`/`saturation` | `camera`, `lut` file, `look` name |
| `lut_strength` (the LUT mix) | denoise, stabilize settings |
| `extra` filter numbers (`grade.filters`, [below](#per-layer-grade-and-filters)), shader `//!PARAM`s, `sharpen`, grain `strength` | the filter chain's shape, shader files |
| `time.speed`, `time.source` ([Time remapping](#time-remapping)) | libavfilter graph text |
| solid `colour`, text `position`/`scale`/`opacity`/`colour` (as today) | text content, font, size |

To change a held value mid-layer, split the layer in two at that time (a template does it for
you).

### Expressions

Required. Every animatable parameter takes a constant, keyframes or an expression. An
expression is a JSON string. It can also be `{"keys": [...], "expr": "..."}`, an expression
over the parameter's own keyframed value (`value`).

```json
"transform": { "position": ["if(lt(t,4.7), 0, -W*(t-4.7)/0.4)", 644] },
"content":   { "zoom": "lerp(1, 1.06, lt/dur)" },
"opacity":   { "keys": [{"t": 0, "v": 0}, {"t": 0.4, "v": 1}], "expr": "value * (0.9 + 0.1*noise(7, t*3))" }
```

**Evaluator: our own**, ~500 lines in `ffcine-core::anim::expr`, no dependencies. Tokens go
through a Pratt parser to an AST, which is constant-folded and flattened to stack ops with
variable slots, then evaluated per frame on the CPU at plan time. Each parameter's expression
is compiled once, and evaluating it costs about a hundred nanoseconds, so 30 params x 1800
frames is nothing. Alternatives and why not:

- ffmpeg's `av_expr_parse`/`av_expr_eval` over FFI. It would put libav into crates that are
  libav-free (core, timeline, reframe, text; the Mac build). Its `random`, `st`/`ld` are
  stateful, and it can't see our tracks except through a fixed variable array. We keep its
  function names and semantics instead, so ffmpeg expressions paste in.
- `evalexpr`: dynamic types, strings, assignment, and different function names. `meval`,
  `fasteval`: unmaintained. `rhai`, Lua: full languages with loops and state, which breaks
  "always terminates" and "a pure function of time" (which the cache keys and pre-roll rules
  rely on) and is far more than needed.
- On the GPU: values are per frame, not per pixel, and per-pixel maths is what user shaders
  are for.

**Grammar** (numbers are `f64`; booleans are 1 and 0, as in ffmpeg):

```text
expr  = cond
cond  = or [ "?" expr ":" expr ]
or    = and { "||" and }
and   = cmp { "&&" cmp }
cmp   = sum [ ( "<" | "<=" | ">" | ">=" | "==" | "!=" ) sum ]
sum   = prod { ( "+" | "-" ) prod }
prod  = unary { ( "*" | "/" | "%" ) unary }
unary = ( "-" | "+" | "!" ) unary | pow
pow   = atom [ "^" unary ]                       (right-associative)
atom  = number | name | name "(" [ expr { "," expr } ] ")" | "(" expr ")"
number = digits [ "." digits ] [ ( "e" | "E" ) [ "+" | "-" ] digits ]
name  = ( letter | "_" ) { letter | digit | "_" }
```

**Variables**

| Name | Meaning |
|---|---|
| `t` | comp time, s (the timeline's time for the comp a timeline lowers to) |
| `lt` | layer time: `t − in` |
| `st` | source time shown on this frame, after the time map (media layers) |
| `n`, `ln` | comp frame index, layer frame index |
| `fps` | canvas rate |
| `W`, `H` | canvas size, px |
| `w`, `h` | layer box size, px |
| `sw`, `sh` | source picture size after `crop` and rotation (display size) |
| `in`, `out`, `dur` | layer in/out on the comp clock and `out − in`, s |
| `T` | comp duration, s |
| `value` | the parameter's own keyframed value (`{keys, expr}` form only) |
| `PI`, `E`, `PHI` | constants |
| any `vars` name | a comp or layer variable: a constant, keyframes, or another expression |

**Functions**: ffmpeg's names and semantics, `if(c, a[, b])`, `ifnot(c, a[, b])`, `lt`, `lte`,
`gt`, `gte`, `eq`, `between(x, a, b)` (inclusive), `clip(x, lo, hi)`, `min`, `max`
(2 to 255 arguments), `abs`, `floor`, `ceil`, `round`, `trunc`, `mod(a, b)` and `%`
(ffmpeg's: `a − floor(a / b)·b`, so `mod(-7, 3)` is 2), `not`, `sqrt`, `pow`, `exp`, `log`,
`sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `hypot`, `sign` (and ffmpeg's `sgn`),
`squish`, `gauss`. ffmpeg has no infix comparisons, `&&`, `||` or `?:`; they are ours. Its `^`
is left-associative (`2^3^2` is 64 there); ours is right-associative as in the grammar (512). Ours: `lerp(a, b, k)`,
`smoothstep(e0, e1, x)`, the easing curves on a progress `u` (clamped to 0..1): `ease`,
`ease_in`, `ease_out`, `ease_in_out`, `back`, `bezier(x1, y1, x2, y2, u)` (`x1` and `x2`
clamped to 0..1). Also
`random(seed)`, uniform in [0, 1) from a hash (SplitMix64) of `seed` and `n`, so it is the same
on every render and every piece (ffmpeg's `random` keeps state; ours doesn't), and
`noise(seed, x)`, smooth value noise in [-1, 1] for wiggles. There is no `st`/`ld`, `while` or
`print`: no state, no loops. Every expression terminates, and its value depends only on the
frame.

**Vars**: `vars` on a comp or a layer, `{name: track}`. Lookup goes layer, then its comp, then
the parent comps. A var may be keyframed, which is how an expression references a keyframed
track (`"vars": {"kick": [{"t": 2, "v": 0, "interp": "out"}, {"t": 2.2, "v": 1}]}`, then
`"scale": "1 + 0.05*kick"`). Vars that reference each other are ordered topologically when the
comp is read, and a cycle is an error naming it. A var can't read an outer var of its own name
(`"x": "x * 2"` on a layer whose comp has `x`): within a level a name means that level's var,
so that is a cycle too; give it another name. Referring to another layer's parameters is
[Not yet](#not-yet); a shared var does the job.

**Errors**: all of them are reported before any GPU work. `ffcine comp check` and the planner
compile every expression and sample every frame.

- Compile: a syntax error gives the parameter path and the column, with a caret
  (`layers[2].transform.position[0]: col 14: expected ")"`). An unknown variable or function, a
  wrong argument count, a var that shadows a built-in, a cycle, or an expression over 4 KiB or
  nested deeper than 64 levels is also an error. Each operator of a chain counts as a level
  (`a + b + c` nests to the left) and the whole expression takes one, so a chain is at most 63
  operators (64 terms) long.
  An expression that is constant and not finite (`1/0`, `log(-1)`) is an error here.
- Evaluate: a non-finite result (`log(-1)`, `1/0`) is an error with the parameter, layer, frame
  and time. Values outside a parameter's range are clamped as constants are today (opacity
  0..1, zoom ≥ 1), with no error.

**Where expressions are allowed**: wherever a track is. That includes the timeline's effect
schemas, since they parse through the same `Track`. Today a string where a number is expected
is rejected, so no existing file changes meaning. In a timeline effect, `t` is timeline time,
`lt` is seconds since the clip's first frame and `st` is the effect's own keyframe clock
(source time).

## Time remapping

```json
"time": { "speed": 2 }                                   // constant (LinearTimeWarp)
"time": { "speed": [{"t": 0, "v": 1, "interp": "in_out"}, {"t": 1, "v": 2, "interp": "in_out"}, {"t": 2, "v": 1}] }
"time": { "source": "12 + 0.5*lt" }                      // source time directly, s
"time": { "freeze": 1.2 }                                // hold source time 1.2 s
"time": { "reverse": true }                              // source_in counts down
"time": { "speed": -1 }                                  // also reverse
```

- One meaning: per layer frame `i`, a **source time** `s(i)`. `speed` is integrated
  (`s = source_in + ∫ speed dlt`, closed form per segment for linear and constant keys, as in
  `Ramp::segments`; numeric otherwise). `source` gives `s` directly, `freeze` is a constant `s`,
  and `reverse` is `speed: -1`. `s` is clamped to the source's range; holding the last frame
  past the end is today's behaviour for layers.
- **Frame picking**: `s(i)` maps to the nearest source frame with the `fps` filter's default rounding
  (`round=near`), so ramps pick the same frames as today's
  `setpts` + `fps` prep. The result is a **frame map**, a source frame index per layer frame.
  `LayerSource::Video` gains `map: Option<Arc<[i64]>>`.
- **Decoding** follows the map. The decode thread already pairs frames to output indices. A
  non-decreasing map (speed ≥ 0, freezes, ramps, timelapses) decodes forward and repeats or
  drops frames: NVDEC, no intermediate, frames stay in VRAM. A map that goes backwards
  (reverse, boomerang) reads an **all-intra lossless intermediate** of the source span it
  covers (NVDEC -> `hevc_nvenc -tune lossless -g 1`, the retime prep's settings plus `-g 1`,
  cached under `<cache>/prep/` by source fingerprint and span), where each backward step is
  one seek to one frame.
- **Existing schemas are sugar**: `LinearTimeWarp` is `speed`, `FreezeFrame` is `freeze`, and
  `ffcine.time.ramp/1` `points [[u, speed]]` over `source_duration` is speed keys at
  `u·source_duration` mapped to layer time through the same integral. Timeline clips keep
  today's retime prep until a frame-pick parity test passes on the ramp fixtures
  (`exec::tests::ramp_pieces_match_the_whole_clip`). Then the prep is dropped for those clips,
  and if the pixels differ at all (the prep converts to `p010le` through a lossless encode),
  `RENDER_REVISION` is bumped.
- `ffcine.time.blend/1` (tmix) stays a prep step; it is a stateful filter, in the
  [escape hatch's](#libavfilter-escape-hatch) terms with pre-roll `frames − 1`. Optical-flow
  slow motion (NVOF) is [Not yet](#not-yet).
- A piece of a remapped layer keys the frame map slice it shows (which source frames), so
  editing a ramp after a cut re-renders only the pieces whose frames changed.

## Per-layer grade and filters

Each media layer has a `grade`: any `GradeParams` field (`look`, `camera`, `lut`,
`lut_strength`, `rgb_gain`, `warmth` ... `saturation`, `exposure`, `sharpen`, `denoise`), plus
`cdl` (`ffcine.color.cdl/1`), `filters`, `shaders` (`ffcine.fx.shader/1` entries), `grain`,
`stabilize`. It is resolved with the timeline's layering (project grade <- look <- source-kind
rule <- layer). The grade is built by the same `looks::grade_for` path as a shot
(`Renderer::grader`). So:

- **Camera transforms are automatic per source**: `camera: auto` (the default) probes each layer's
  source (`Profile::detect_file`): D-Log gets its camera LUT, an iPhone HEIC still is
  display-referred and gets the light rule and libplacebo's gamut conversion, and HLG gets tone
  mapping. A log clip and a HEIC sit side by side without per-layer settings.
- **eq / curves / colour balance / levels**: `filters` is the `extra` list. Each entry is an ffmpeg
  filter string, run through `ffcine_render::extra` as `PREOUTPUT` hooks (`eq`, `hue`,
  `colorbalance`, `curves`, `unsharp`, `vignette`), or an object form whose numbers can be
  tracks: `{"eq": {"contrast": 1.1, "saturation": [{"t": 0, "v": 1}, {"t": 2, "v": 1.3}]}}`.
  Levels becomes one more parsed filter (`colorlevels`: black and white points in and out,
  per channel), about 40 lines in `extra.rs`. Anything `extra` can't parse goes to the
  [escape hatch](#libavfilter-escape-hatch), explicitly. It is never silently dropped, as `extra`
  does today with a warning.
- **LUT with mix**: a constant `lut_strength` stays baked into the LUT, so it renders as today.
  An animated one moves the grade LUT out of `pl_custom_lut` into a generated hook that samples
  the same lattice as a 3D `//!TEXTURE` and outputs `mix(c, lut(c), strength)`, with `strength`
  a `ParamTrack`.
- **User shaders**: `shaders` with `//!PARAM` tracks, as in `ffcine.fx.shader/1`.
- **Order** inside the layer's pass is today's: source LUT (camera + grade merged), geometry,
  scaler, tone/gamut map, `PREOUTPUT` look, `filters`, `shaders`, sharpen, finish (`fade`), then
  alpha at `OUTPUT`.

Cost per layer, on top of the layer's own pass at its texture size:

| Item | GPU cost |
|---|---|
| layer pass (decode-mapped source -> crop/scale -> tone map -> RGBA texture) | one `pl_render_image` at box size |
| camera/grade LUT, look, finish, alpha | free: inside that pass (LUT) or one fused hook each |
| each `filters` entry, each shader | one hook pass at box size (adaptive-sharpen: several) |
| animated `lut_strength` | one hook pass |
| denoise (temporal NVOF) | heavy: the main picture's denoiser per layer; allowed but warned above two |
| stabilize | NVOF detect once per clip (cached), warp free in the pass |
| composite (rotated, non-normal blend, HDR) | one canvas-size dispatch per such layer |
| video decode | one NVDEC session and its surface pool per video layer |

## libavfilter escape hatch

For anything ffcine has no native primitive for, a layer gets a raw libavfilter graph:

```json
"filter": { "graph": "deflicker=size=7,eq=gamma=1.05", "hw": "cpu", "clock": "layer" }
```

- **Where it runs**: in the layer's **prep**, the retime step's place, after the time map and
  before the GPU pass, into a lossless intermediate the layer then decodes. So the graph sees
  the layer's frames at the canvas rate: the same frames, in order, that the layer will show.
  `hw: "cpu"` (default) decodes to software frames, runs the graph and uploads; `hw: "cuda"`
  keeps NVDEC frames in VRAM for graphs of `*_cuda` filters; `hw: "vulkan"` derives Vulkan for
  `*_vulkan` filters (`nlmeans_vulkan`, `blend_vulkan`). ffcine doesn't guess the mode from the
  graph. The graph must output one video stream at the input rate with the same frame count,
  and the frames are re-tagged with the source's colour tags unless the graph sets them
  (`setparams`).
- **Per comp output**: `canvas.filter` runs the same way over the composited canvas. The
  segment renders lossless, the graph runs, then the profile's encoder. That costs one extra
  lossless encode per segment, so use it only when it's needed.
- **Time**: the graph is fed frames whose PTS is the frame's time on the node's clock:
  `clock: "layer"` (default) uses `lt`, `clock: "comp"` uses `t`. A piece starting 3 s into the
  layer starts at PTS 3 s, not 0, so `t`-based options (`eq=eval=frame:brightness='0.1*sin(t)'`,
  `drawtext` timing) render the same split or unsplit. `layer` is the default because moving the
  layer on the timeline then changes nothing: the key needs no position. libavfilter's `n` (a
  filter's own frame counter) can't be offset. ffcine lints expressions that use `n` and tells
  you to use `t*FRAME_RATE` or `whole_clip`.
- **State**: a graph is stateful if any filter in it reads other frames. `ffcine-render` keeps
  a table of known filters and their reach in frames, under its own `FILTER_TABLE_REVISION`:

  | Filter | Pre-roll | Post-roll | Or |
  |---|---|---|---|
  | `tmix` | `frames − 1` | 0 | |
  | `tblend` | 1 | 0 | |
  | `deflicker` | `size` (5) | `size` | |
  | `atadenoise` | `s/2` (4) | `s/2` | |
  | `tmedian`, `tmidequalizer` | `radius` | `radius` | |
  | `fftdnoiz` | `prev` | `next` | |
  | `yadif`, `bwdif`, `w3fdif` (and `_cuda`) | 1 | 1 | |
  | `lagfun`, `hqdn3d` (recursive: every past frame counts), `vidstabtransform`, `deshake`, `normalize`, `photosensitivity` | | | whole clip |
  | `minterpolate`, `framerate`, `mpdecimate`, `decimate` | | | change timing: rejected unless `timing: free`, then whole clip |

  Windowed filters get pre/post-roll; recursive ones (whose output depends on every earlier
  frame) can only match an unsplit render by running over the whole clip. Rolls in the table
  are generous on purpose, since extra frames never change the output. Unknown filters count
  as stateless. The node's own `preroll`, `postroll` and `whole_clip`
  override the table. A piece decodes `preroll` layer frames before its first frame and
  `postroll` after its last (clamped at the layer's ends, where a whole render sees nothing
  either) and drops their outputs, the same rule as the denoiser's `Pipeline::context`. With
  `whole_clip`, the node runs once over the whole layer into one cached intermediate that every
  piece slices, as the stabilizer's motion is analysed per clip.
- **Expressions need none of this**: they are pure functions of time and frame, so a piece
  samples its own frames and gets the values a whole render would.
- **Key**: the node's recipe is the graph text verbatim, `hw`, `clock`, the resolved
  `preroll`/`postroll`/`whole_clip`, `FILTER_TABLE_REVISION`, the ffmpeg build (`av_version_info`
  plus the libavfilter version), and its inputs: the source fingerprint and the frame-map slice
  it reads, including pre-roll and post-roll. For a whole-clip node that is the whole layer's
  map; with `clock: "comp"` it also includes the layer's `in`. It goes into the layer's recipe,
  so the segment key changes when any of these do. `RENDER_REVISION` is bumped when ffcine
  changes how nodes run (the PTS rule, re-tagging); a change to the table bumps only
  `FILTER_TABLE_REVISION`, which re-renders only stateful nodes.
- **Caveats**: CPU graphs are slow at 4K (a download, the filter, an upload, a lossless encode)
  and run on the prep thread pool, not the GPU pipeline. GPL filters exist only if the ffmpeg
  in use was built with them, and their licence is then the user's concern. Audio is ignored.
  Frame timing must be kept (`timing: free` lets the layer take whatever comes, with no
  guarantee across pieces).

**Cache invalidation tests** (plan level, no GPU, in `plan::tests` style):

- editing the graph re-renders exactly the layer's segments, with every other key unchanged;
- moving a `clock: layer` node's layer re-renders nothing; with `clock: comp` it re-renders its
  segments;
- a stateless node: trimming the layer's tail re-renders only the last piece; a `whole_clip`
  node: it re-renders every piece of the layer;
- `tmix=frames=5`: the key includes 4 frames of pre-roll, and moving a cut moves which frames
  are keyed;
- a changed ffmpeg version string (injected `Fingerprinter`) or `FILTER_TABLE_REVISION`
  re-renders stateful nodes only (and the version, all nodes);
- expressions: replacing keys with an equivalent expression keeps every key; an edit that
  changes values only after a cut re-renders only the later pieces.

GPU/ffmpeg tests (`--ignored`): a `t`-dependent `eq` and a `tmix=frames=5` graph rendered as
one segment and as three give identical `framemd5`s; a `whole_clip` `deflicker` too.

## Rendering

The renderer keeps its current shape. The change is that the planner hands it a tree instead of
hand-built layer lists.

1. **Lowering** (`ffcine-comp`, CPU, plan time): a comp piece becomes a `Pipeline` with a base
   grade and `Vec<Layer>`. Every track is sampled over the piece into what the layer already
   takes: `rects`, `rotation`, `crops` (content), hook `ParamTrack`s (opacity, radius, mask,
   fade, look, filters, shaders), and the background hook's `bg_*` tracks.
2. **Base promotion**: if the bottom layer is a normal-blend, fully opaque, unrotated video that
   covers the canvas for the whole piece, it becomes the pass's input with the canvas grade, as
   the main picture is today. That saves a texture and a pass, and it is what makes existing
   timelines lower to the same GPU work. Otherwise the input is the tiny black clip and the
   canvas `background` is `shaders::background` with per-frame `bg_r/g/b`.
3. **Layers**: each is one `pl_render_image` into an RGBA texture at its box size, times the
   largest `transform.scale` in the piece (today's "largest size in the segment"), then drawn
   as a `pl_overlay` (normal, unrotated, SDR) or by the composite pass (rotated, blended, HDR),
   bottom first. A layer whose inputs are the same as on the previous frame (a still with no
   animated content, grade or alpha) reuses its texture instead of re-rendering, which makes
   static stills and solids free after the first frame (`Grade::same_frame`: the same crop and
   hook values, and nothing that moves by itself such as temporal dither, grain or a warp).
4. **Nested comps** render in-pass: the sub-comp's background and layers go into the nested
   layer's texture through the same code (the composite pass's target becomes a parameter, not
   always the output), then that texture is a layer of the parent. No intermediate file and no
   alpha loss. Nesting depth is capped at 4.
5. **Text** is drawn as `pl_overlay` masks on the output, as today, when no media layer is above
   it (the common case). A text layer with media above it is drawn into its own RGBA texture
   (the masks as overlays on a transparent target) and composited in order, at one extra pass.

**Performance budget** (to measure in phase 2's bench on the reference RTX 5060 Ti; these are
targets, not measurements):

- The story at 1080x1920: at most ~8 visible layers and 3-6 decoding videos. Layer passes total
  about 2-4 Mpx of fragment work per frame, so the decoders bound it. Target: faster than real
  time at 30 fps.
- Per video layer: one NVDEC session with its surface pool (the dominant VRAM, ~0.3 GB for a
  4K 10-bit source) and an RGBA16F texture of the box (1080x632: 5.5 MB). The planner adds
  this to the VRAM budget that already bounds concurrent jobs, and warns over 8 decoding layers
  in a piece.
- Composite pass: one canvas-sized dispatch per composited layer per frame, cheap at 1080x1920
  and worth measuring at 4K with many rotated layers.

Measured (step 2, `cargo test -p ffcine --release --test comp_gpu -- --ignored --nocapture
bench`: 120 frames of N rounded, fading, moving 1080p H.264 video tiles in a column, NVENC HEVC
out, wall time including setup). The reference GPU was shared with other jobs at the time
(about 13 of its 16 GB and much of its compute in use), so these are lower bounds, not the
budget's verdict; re-run on an idle GPU:

| Output | 2 layers | 4 layers | 8 layers |
|---|---|---|---|
| 1080x1920 | 3.7-6.4 fps, +0.85 GB | 3.0-30 fps, +0.83 GB | 3.7-25 fps, +1.3 GB |
| 2160x3840 | 7.6-24 fps, +1.5 GB | 8.5-30 fps, +1.2-1.6 GB | out of VRAM (~2.8 GB free) |

The ranges are two runs minutes apart under different load from the other jobs; the VRAM is the
peak rise of the GPU's `memory.used` during the render.

**Caching** (incremental renders):

- A comp is cut into pieces like a shot: at layer in/out points and text cuts, never leaving a
  piece under 1 s (`plan`'s rule).
- A piece's key is canonical JSON of the canvas (size, fps, constants and the sampled
  background and fade), plus for each visible layer in order:
  - the source recipe (fingerprint, the frame-map slice);
  - the resolved grade (LUTs by content hash);
  - the held parameters;
  - each animated parameter as a hash of its sampled `f32` bits over the piece;
  - the filter node's recipe.

  Then the profile, the ffcine version and `RENDER_REVISION`. Sampled values instead of track
  definitions mean that only frames whose values change re-render, and that an equivalent
  rewrite (keys to an expression) re-renders nothing. A layer's position in the array is part of
  the key; its `id` and name are not, as clip ids aren't today.
- A comp generator clip keys by comp time (its `source_range`), never its timeline position, so
  moving it renders nothing.
- `RENDER_REVISION` policy: phases that only add schema don't bump it, because new content gets
  new keys. Phases that re-route existing features must render bit-identically (the parity tests
  below) and keep their keys. If one can't, it bumps the revision with a line in
  [assembly.md](assembly.md#keys). Step 1 bumped it to 8: lenient inputs that the old parsers
  read differently (a bare number for a text keyframe, a numeric string for a comp's opacity,
  the `ease` alias on reframe keys) render differently under the same recipe.

## Mapping of existing features

Each existing feature lowers to the model. Its key recipe stays exactly as it is
(`Unit::recipe`, `SegLayer::recipe`), so nothing re-renders; only the code that builds the
`Layer`s moves into the lowering.

| Feature | Lowers to |
|---|---|
| main picture (shot) | bottom video layer, canvas box, `fit: cover`, `content` = its reframe, promoted to the base |
| `ffcine.geo.crop/1` + `ffcine.geo.transform/1` on the main picture | stays in the base's `MAIN` hook (`pass::apply_mapped`), which can show past the window; not converted to a layer transform |
| gap | canvas background black, no layers |
| dissolve | `a` base, `b` a layer with `opacity` `Sampled(progress)` |
| dip / flash | both sides' `fade` + `fade_colour` (dip) or the finish `lift` (flash) as `Sampled` tracks, `b` from the half |
| whip | canvas black, both sides layers with `position` tracks (smoothstep travel) and the `motion_blur` shader with its `blur` track |
| split-screen `ffcine.comp.layout/1` | the `layout` [template](#templates): backdrop background (22, 20, 18, vignette 0.35), panels as layers at `Layout::rects`, `enter_offset` as position keys (it is `ease: out` exactly), `radius`, the stack's finish as canvas `fade` |
| split dissolve | the split comp, with the shot as a layer over it (or under it), `opacity` `Sampled` |
| picture-in-picture / overlay clips | layers: `Placement` turned into `position`, `anchor`, `scale`, `rotation`, `opacity` (px from frame fractions), `blend`, `radius`; the clip's reframe as `content` |
| stills (PNG/JPEG overlays) | `still` layers, colour as today for untagged files |
| text, titles, cards | `text` layers on top (`pl_overlay` masks, unchanged) |
| finish (`vignette`, `darken`, `fade`, `flash`) | the layer's (or canvas's) finish hook, as now |
| grain on the top-level stack | `canvas.grain` (applied to the base and video layers, as now) |

**Backward compatibility**: an existing timeline plans the same keys, renders the same pixels,
and keeps its cache. Two checks: the existing plan tests (`insert_or_remove_changes_no_other_key`,
`filter_edits_rerender_only_their_clip`, `crates/ffcine/tests/render_plan.rs`) unchanged, and a
GPU parity test over synthetic fixtures (`testsrc2` clips: a split-screen with stagger and
radius, a PiP with rotation and `multiply`, each transition kind, a still, a caption) whose
`framemd5`s must match the pre-refactor build's, recorded as a fixture file.

## Templates

A template is a Rust function `fn(&Params) -> Result<Comp>` in `ffcine-comp::templates`, run at
read time. Its output is an ordinary comp (`ffcine comp lower` prints it), so templates add no
render path and the keys only see the lowered layers.

- **`layout`** = `ffcine.comp.layout/1`: `layout` (`split`/`row`, `column`, `1+2`), `gap`,
  `margin`, `radius`, `weights`, `stagger`, `slide`, `enter`, `first_static`, as today, with
  `Layout::rects` and `enter_offset` moved into the template. The planner calls it for every
  split `Stack`; a comp file can use it too (`"template": {"layout": {...}, "panels": [...]}`).
- **`story`**: a vertical stack of `tiles` tiles with gutters, each tile a nested comp of its
  items. Items swap with a slide `swap.stagger` apart. There are staggered fade-ins from black,
  a background track, Ken Burns on stills, optional per-item `punch` and `speed`, and a
  staggered slide-out with a fade to black when `loop` is set.

```json
{ "schema": "ffcine.comp/1",
  "template": { "story": {
    "size": [1080, 1920], "fps": "30", "duration": 6.0,
    "tiles": 3, "gutter": 12, "radius": 24,
    "background": { "from": "black", "to": "white", "duration": 1.0 },
    "enter": { "fade": 0.4, "stagger": 0.25 },
    "swap": { "at": 2.5, "stagger": 0.12, "slide": 0.4, "direction": "left" },
    "exit": { "at": 5.2, "stagger": 0.12, "slide": 0.4, "fade": 0.4 },
    "ken_burns": [1.0, 1.06],
    "grade": { "camera": "auto" },
    "panels": [
      [ { "video": "a.mov", "source_in": 12.0, "punch": { "at": 1.6, "zoom": 1.12, "duration": 0.15 } },
        { "still": "d.heic" } ],
      [ { "still": "b.heic" }, { "video": "e.mov", "source_in": 3.5 } ],
      [ { "video": "c.mov", "speed": [[0, 1], [1.0, 2], [2.0, 1]] }, { "still": "f.heic" } ] ] } } }
```

Tile height is `(H − (tiles − 1)·gutter) / tiles`, which is 632 here.

### Worked example: the story

What the template lowers to, written out. This is also what one would write by hand.

```json
{
  "schema": "ffcine.comp/1",
  "canvas": {
    "size": [1080, 1920], "fps": "30", "duration": 6.0,
    "background": [ { "t": 0, "v": [0, 0, 0], "interp": "in_out" }, { "t": 1.0, "v": [1, 1, 1] } ],
    "fade": "1 - clip((t - 5.6) / 0.4, 0, 1)", "fade_colour": "black"
  },
  "vars": { "enter": 0.25, "swap": 2.5, "swap_step": 0.12, "slide": 0.4, "exit": 5.2 },
  "comps": {
    "tile0": { "canvas": { "size": [1080, 632], "background": "black" }, "layers": [
      { "id": "a", "source": { "video": "a.mov" }, "source_in": 12.0, "out": "swap + slide",
        "grade": { "camera": "auto" },
        "content": { "zoom": [ { "t": 1.6, "v": 1.0, "interp": "out" }, { "t": 1.75, "v": 1.12 } ] },
        "transform": { "anchor": [0, 0], "position": ["-w * ease_out((t - swap) / slide)", 0] } },
      { "id": "d", "source": { "still": "d.heic" }, "in": "swap",
        "grade": { "camera": "auto" }, "content": { "zoom": "lerp(1, 1.06, lt / dur)" },
        "transform": { "anchor": [0, 0], "position": ["w * (1 - ease_out(lt / slide))", 0] } } ] },
    "tile1": { "canvas": { "size": [1080, 632], "background": "black" },
      "vars": { "s": "swap + swap_step" }, "layers": [
      { "id": "b", "source": { "still": "b.heic" }, "out": "s + slide",
        "grade": { "camera": "auto" }, "content": { "zoom": "lerp(1, 1.06, lt / dur)" },
        "transform": { "anchor": [0, 0], "position": ["-w * ease_out((t - s) / slide)", 0] } },
      { "id": "e", "source": { "video": "e.mov" }, "source_in": 3.5, "in": "s",
        "grade": { "camera": "auto" },
        "transform": { "anchor": [0, 0], "position": ["w * (1 - ease_out(lt / slide))", 0] } } ] },
    "tile2": { "canvas": { "size": [1080, 632], "background": "black" },
      "vars": { "s": "swap + 2 * swap_step" }, "layers": [
      { "id": "c", "source": { "video": "c.mov" }, "out": "s + slide",
        "grade": { "camera": "auto" },
        "time": { "speed": [ { "t": 0, "v": 1, "interp": "in_out" },
                             { "t": 1.0, "v": 2, "interp": "in_out" }, { "t": 2.0, "v": 1 } ] },
        "transform": { "anchor": [0, 0], "position": ["-w * ease_out((t - s) / slide)", 0] } },
      { "id": "f", "source": { "still": "f.heic" }, "in": "s",
        "grade": { "camera": "auto" }, "content": { "zoom": "lerp(1, 1.06, lt / dur)" },
        "transform": { "anchor": [0, 0], "position": ["w * (1 - ease_out(lt / slide))", 0] } } ] }
  },
  "layers": [
    { "id": "t0", "source": { "comp": "tile0" }, "size": [1080, 632], "radius": 24,
      "vars": { "i": 0 }, "fade": "ease_out((t - i * enter) / 0.4)", "fade_colour": "black",
      "transform": { "anchor": [0, 0],
        "position": ["if(lt(t, exit + i * swap_step), 0, -W * ease_in((t - exit - i * swap_step) / slide))", 0] } },
    { "id": "t1", "source": { "comp": "tile1" }, "size": [1080, 632], "radius": 24,
      "vars": { "i": 1 }, "fade": "ease_out((t - i * enter) / 0.4)", "fade_colour": "black",
      "transform": { "anchor": [0, 0],
        "position": ["if(lt(t, exit + i * swap_step), 0, -W * ease_in((t - exit - i * swap_step) / slide))", 644] } },
    { "id": "t2", "source": { "comp": "tile2" }, "size": [1080, 632], "radius": 24,
      "vars": { "i": 2 }, "fade": "ease_out((t - i * enter) / 0.4)", "fade_colour": "black",
      "transform": { "anchor": [0, 0],
        "position": ["if(lt(t, exit + i * swap_step), 0, -W * ease_in((t - exit - i * swap_step) / slide))", 1288] } }
  ]
}
```

How each requirement lands:

- **Canvas and tiles**: three 1080x632 boxes at y = 0, 644 and 1288 (632 + 12). The 12 px gutters
  show the canvas background.
- **Mixed media**: `a`, `c`, `e` are videos, `b`, `d`, `f` are HEIC stills, and each still
  decodes once.
- **Fade-in from black, staggered 0.25 s**: each tile's `fade`. The easing functions clamp
  their argument, so `fade` is 0 before the tile's start.
- **Background black to white**: the canvas `background` keys over the first second.
- **Slide swaps 0.12 s apart**: inside each tile comp, the outgoing item slides left and the
  incoming one follows. The tile's canvas clips both, and its `radius` rounds the result.
- **Ken Burns**: `content.zoom` 1.0 -> 1.06 over each still's life, a moving source window,
  not a scaled box.
- **Punch-in**: `a`'s `content.zoom` 1.0 -> 1.12 in 0.15 s from 1.6 s, then holding.
- **Speed ramp**: `c`'s `time.speed` 1x -> 2x -> 1x. It is integrated into a frame map, so
  NVDEC decodes forward.
- **Loop**: the tiles slide out from 5.2 s, 0.12 s apart, in ffmpeg-style `if(lt(t, ...))`
  expressions, and the canvas `fade` goes to black over the last 0.4 s. The last frame is black,
  like the first.
- **Colour**: `camera: auto` gives the log clips their camera LUT, and the HEICs (Display P3)
  get the light rule and a gamut conversion.

`in`/`out` accept expressions over the vars (evaluated once, as constants), as here. Cost: three
tile textures (1080x632), at most six media layers, up to three decoding videos during the
swaps.

## Phased plan

Each step is one PR, in dependency order. Each keeps the existing tests green and has its own.

1. **Param tracks and expressions** (`ffcine-core::anim`). `Track`, `Key`, `Ease`,
   `Animatable`, `Clock`, the expression compiler and evaluator, and `vars` resolution with
   cycle detection. `Keyed`, text `Keys`/`Ease`, audio `Interp` and hooks' shader params
   are ported onto it, so expressions work in every existing effect schema. **Built.** The
   ffmpeg fixture is `crates/ffcine-core/src/anim/testdata/ffmpeg_eval.json`, regenerated by the
   `gen_ffmpeg_eval.py` next to it.
   *Tests*: today's keyframe tests in reframe, text, hooks and audio unchanged; golden eval
   tables for each ease (bezier against CSS values); a grammar table (precedence,
   associativity, every function's ffmpeg value against `av_expr_eval`, generated once and
   checked in as a fixture so the test needs no libav); error messages with columns;
   `random`/`noise` stable across runs; `render_plan.rs` keys unchanged.
2. **Compositor core** (new crate `ffcine-comp`: model, serde, validation, lowering to
   `Pipeline` + `Layer`s; `ffcine render`/`still` accept comp files; `ffcine comp check`).
   Canvas with animated background and fade, `video` layers without remapping, `solid`, the
   transform/anchor/scale/rotation/opacity, `radius`, shape `mask`, `blend`, `fade`, base
   promotion, comp segment keys with sampled values, and the unchanged-input texture reuse.
   **Built.** `ffcine_comp::Comp` reads and validates a comp (JSON or YAML, within the
   [limits](#limits); keys of later steps,
   `still`/`text`/`comp` sources, `content`, `crop`, `grade`, `time`, `filter`, `project`,
   `comps`, `template`, are errors naming the step), `Comp::pieces` cuts it at layer in/out
   points, and `Comp::lower` turns a piece into a base input and grade plus `Layer`s. Solids
   are a 1x1 image with `shaders::solid`; a layer's fade is its `shaders::finish` hook, and the
   canvas fade is one more layer on top of the others, a normal-blend solid of `fade_colour` at
   opacity `1 - fade`, so it fades the composite whatever the layers' blend modes; opacity,
   corners and the mask are `shaders::layer_alpha`. `ffcine_assembly::plan_comp` makes the pieces segments (`Unit::Comp`)
   keyed by the lowered values (rects, rotation, every hook value as an `f32` digest, the source
   fingerprint and start), so the segment cache, `--dry-run`, the manifest and `render_piece`
   (stills) are shared with timelines. Existing timelines plan and render as before
   (`RENDER_REVISION` stays 8). The GPU test is `crates/ffcine/tests/comp_gpu.rs` (78, 61 and 60 dB
   against the CPU reference, the last a canvas fade over multiply and difference layers, and
   an ellipse feather even on both axes); the bench is its `bench_layers`.
   *Tests*: lowering unit tests (rects and tracks from a comp, base promotion on/off); key tests
   (equivalent rewrites keep keys, an edit after a cut re-renders only later pieces, reorder
   changes keys); a GPU test (`--ignored`) compositing solids and a `testsrc2` clip against a
   CPU reference image at ≥ 50 dB; a bench recording fps and VRAM for 2, 4 and 8 layers at
   1080x1920 and 4K.
3. **Media layers**: `content` (the picture inside the box) and `crop`; `still` through libav (HEIC tile grids, `irot`, colour tags -> libplacebo
   conversion; untagged PNG/JPEG as today), decoded once and cached per piece; `text` layers in
   z-order (masks into a texture when covered); nested `comp` in-pass (parametric composite
   target, depth cap).
   *Tests*: a HEIC fixture (a small grid file made with ffmpeg's HEIF muxer, or a synthetic
   grid) decodes to the same pixels as `ffmpeg -i x.heic` CPU-side; a rotated HEIC is upright; a
   P3-tagged still converts against libplacebo's reference; a nested comp renders the same as the
   flattened layers it is equivalent to (within 1 code value); text under a layer is hidden
   where expected.
4. **Per-layer grade**: `grade` resolved through `Project` layering, `camera: auto` per
   source, animated look params as `ParamTrack`s, `filters` (string and object form with
   tracks, plus `colorlevels`), `shaders`, the animated `lut_strength` hook, per-layer denoise,
   grain and stabilize.
   *Tests*: a layer with the same grade as a shot renders the same pixels as the shot does
   today; the animated LUT-mix hook at constant strength matches the baked LUT within LUT
   rounding; the `colorlevels` hook against ffmpeg's CPU filter (PSNR, as the other `extra`
   tests do); a log clip and a HEIC in one comp pick different camera profiles.
5. **Time remap**: `time` (`speed`, `source`, `freeze`, `reverse`), the integrator, frame maps
   in `LayerSource::Video` and the decode thread, the all-intra prep for backward maps, and
   timeline `LinearTimeWarp`/`FreezeFrame`/`ffcine.time.ramp/1` as sugar (switched over only
   after parity).
   *Tests*: the frame map against ffmpeg's `setpts`+`fps` picks on the ramp fixtures (same
   indices); reverse equals ffmpeg's `reverse` on a short clip (`framemd5`); split pieces of a
   ramped layer equal the unsplit render; keys follow the map slice.
6. **libavfilter node**: `filter` on layers and `canvas.filter` in the prep (CPU, CUDA,
   Vulkan), the PTS clock, the statefulness table and `FILTER_TABLE_REVISION`, pre-roll,
   post-roll, whole clip, and the `n` lint.
   *Tests*: the [cache invalidation tests](#libavfilter-escape-hatch) listed above, and split
   vs unsplit `framemd5` for a `t`-based, a `tmix` and a whole-clip graph.
7. **Layout lowering and the story template**: `templates::{layout, story}`; the planner lowers
   every existing unit (shot, gap, transitions, split, split dissolve, overlay clips) through
   `ffcine-comp`, with `exec`'s hand-built layer code removed; the `ffcine.comp` generator clip
   in timelines; `ffcine comp lower` and `ffcine comp new --template story`; docs
   ([assembly.md](assembly.md), [timeline.md](timeline.md) schema rows).
   *Tests*: the backward-compatibility `framemd5` parity suite and the unchanged plan keys;
   `story` lowered to the worked example's JSON (golden file); a story render whose sampled
   frames (fade-in start, mid-swap, mid-ramp, loop end = black) are checked against expected
   tile contents.

## Not yet

- **Track mattes** (alpha or luma of another layer), and referencing another layer's
  parameters from an expression.
- **Comp audio**: standalone comps render picture only; the plan is `audio: [{source, in,
  source_in, gain_db}]` through `ffcine-audio`. As a timeline clip, the timeline's audio is used.
- **Time remapping of nested comps** and comp-level speed; frame blending or optical-flow
  (NVOF) interpolation for slow motion.
- Motion blur for animated transforms (only the whip has it), 3D/perspective transforms,
  corner pinning.
- Layer styles (drop shadow, glow, stroke) and per-layer blur, except as user shaders.
- Stills: ICC profiles (only CICP/nclx tags are read), HEIC HDR gain maps (the SDR base image
  is used), animated GIF/WebP, Lottie.
- Text rotation and keyframes that need a new layout (as in [text.md](text.md#not-yet)).
- Keeping libavfilter context across segments for `canvas.filter` (it gets pre-roll from the
  composited canvas only if that is rendered, so stateful output graphs are whole-comp).
- Running the prep graphs in-process through the libav backend instead of the `ffmpeg` CLI (as
  for the other prep steps, [assembly.md](assembly.md#not-done-yet)).
