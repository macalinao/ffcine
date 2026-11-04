# Text

Place cards, time cards, subtitles, titles and end cards are `ffcine.text` generator clips on
their own tracks ([timeline.md](timeline.md#ffcinetext)). `ffcine-text` lays them out with
`cosmic-text` and the render pipeline composites them on the GPU, in the same libplacebo pass as
the grade, only on the frames they cover. This replaces the prototype's one `.ass` burnt over the
whole 4K body, where any text change meant a full re-render.

## How it renders

1. `TextLayer::from_timeline` collects the enabled text clips with their timeline times and
   merged parameters: `project.styles.base`, then the clip's `style` (and that style's
   `based_on` chain, parent first), then the clip's own parameters. Objects merge key by key, so
   a clip can set `shadow.colour` and keep the style's `shadow.blur`.
2. `TextLayer::overlays(start, frames, rate, size)` lays out the clips that overlap a segment at
   the output size (lengths scale by output height / `frame_size` height) and returns
   `ffcine_render::Overlay`s: an 8-bit coverage mask per layer (box, shadow, outline, fill,
   highlight) with a colour, the frame range relative to the segment, and fade lengths.
3. `Pipeline::overlays(...)`: the backend uploads each mask once as an `r8` texture and, on each
   frame, hands the visible ones to `pl_render_image` as target `pl_overlay`s
   (`PL_OVERLAY_MONOCHROME`, sRGB). The part colour's alpha carries the fade. No extra pass, no
   copies, and frames with no text are untouched.

For incremental renders (`ffcine-assembly`), split segments at `TextLayer::cuts()` (every text in
and out point) and put `TextLayer::fingerprint(start, end)` in the segment's cache key: it hashes
the overlapping clips' parameters and their times relative to the segment, so editing a card only
invalidates the seconds it covers. Frame ranges may start before 0 or end after the segment, so a
fade that spans a cut continues across it.

```rust
let mut text = TextLayer::from_timeline(&timeline, timeline_dir)?;
let overlays = text.overlays(seg_start_s, seg_frames, rate, Size { width: 3840, height: 2160 })?;
Pipeline::new(src, seg_out).range(src_in, Some(seg_frames)).grade(grade).overlays(overlays);
```

## Style parameters

Anything in a style can also be set on a clip. The schema in [timeline.md](timeline.md) is
unchanged; these keys are additive.

| Key | Meaning |
|---|---|
| `font`, `face` | family and face (`Light`, `SemiBold Italic`, ...); system fonts via fontconfig |
| `font_file`, `font_index` | a font file (relative to `fonts_dir`, else the timeline's directory) and its face in a `.ttc` |
| `size` | px at `frame_size`; a value ≤ 1 is a fraction of the frame height (the prototype's) |
| `colour` | `[r, g, b, a]` 0..1 (`color` 0..255 from `edit.yaml` works too); `opacity` multiplies alpha |
| `highlight` | colour of `*marked*` words; without it asterisks stay text |
| `position`, `anchor` | `anchor` (fractions of the text box, `[0, 0]` top left, default `[0.5, 0.5]`) lands on `position` (offset from the frame's middle, fractions of the frame) |
| `alignment` | lines within the box: `left`, `center`, `right` |
| `max_width` | wrap width, fraction of the frame width (0.9) |
| `line_height` | multiple of `size` (1.2) |
| `tracking` | extra letter spacing, ems |
| `uppercase` | draw in capitals |
| `shadow` | `{colour, offset [x, y] px, blur px}`; drop shadow of the text (and its outline) |
| `outline` | `{colour, width px}` |
| `box` | `{colour, padding px}`: a box behind the text (caption pill, card scrim) |
| `fade_in`, `fade_out` | seconds; a plain fade. `animation.in` / `animation.out` win ([Animation](#animation)) |
| `animation` | `{in: {...}, out: {...}}`: entrances and exits, see [Animation](#animation) |
| `keyframes` | `{position, scale, opacity, colour}`: keyframed parameters, see [Keyframes](#keyframes) |
| `rect` | `[w, h]` fractions of the frame: draw a filled rectangle in `colour` instead of the text (bars, rules, backdrops); placed, animated and keyframed like text |
| `delay` | seconds after the clip starts before the text shows |
| `based_on` | parent style name |

Imported `edit.yaml` styles keep the prototype's keys and render as before: `x`/`y` (fractions
from the top left; `y` is the baseline for cards, the block bottom for captions, and locations
measure `x` from the right), `shadow_opacity`/`shadow_blur`, `pill_opacity`/`scrim`, and the
font files in `base` (`serif`, `serif_card`, `serif_sc`, `serif_italic`, `sans` + index) by
element (card, location, title, end card, caption).

Place cards top left, for example:

```json
"styles": {
  "base": { "fonts_dir": "/media/footage/fonts",
 "colour": [0.98, 0.96, 0.93, 1],
            "shadow": { "colour": [0, 0, 0, 0.5], "offset": [0, 4], "blur": 12 } },
  "place": { "font_file": "CormorantGaramond-Regular.ttf", "size": 0.08,
             "position": [-0.45, -0.42], "anchor": [0, 0], "alignment": "left",
             "fade_in": 0.6, "fade_out": 0.5 },
  "time": { "based_on": "place", "size": 0.04, "position": [-0.45, -0.33] },
  "sub": { "font_file": "Inter.ttc", "font_index": 14, "size": 0.043,
           "position": [0, 0.42], "anchor": [0.5, 1], "box": { "colour": [0, 0, 0, 0.4] } }
}
```

## Animation

`animation.in` and `animation.out` take `{kind, duration, by, stagger, ease, direction, distance,
scale}`; only `kind` is needed (`duration` defaults to 0.5 s).

| `kind` | Does |
|---|---|
| `fade` | fades (the default; also any name ffcine doesn't know, e.g. another app's) |
| `slide` | moves `distance` (fraction of the frame height, 0.04) along `direction` (`up`, `down`, `left`, `right`: the way the text moves) while fading; eases `out` |
| `pop` | scales up from `scale` (0.6) with an overshoot (`back` ease) while fading |
| `typewriter` | shows one character after another (by `char` unless `by` says otherwise), no fade; `duration` is the whole typing time. As an exit it deletes from the end |
| `wipe` | reveals along `direction` (`right` default, `left`, `up`, `down`, `center`: from the middle out); an exit hides the same way |
| `none` | no entrance or exit |

`by` animates the text in pieces, each `stagger` seconds after the previous: `whole` (default),
`line`, `word` or `char` (default staggers 0.2, 0.1, 0.03 s; a typewriter's is `duration` / pieces).
An exit's pieces leave in the same order, the last one finishing at the clip's end. `ease` is
`linear`, `in`, `out`, `in_out`, `ease` (cosine), `back` (overshoot), `hold`, or a CSS
`cubic-bezier` as `[x1, y1, x2, y2]`. A box (`box`) moves with the text when it animates whole and
shows with its first piece otherwise.

```json
"animation": {
  "in": {"kind": "slide", "by": "word", "duration": 0.8, "stagger": 0.1},
  "out": {"kind": "fade", "duration": 0.4}
}
```

## Keyframes

`keyframes.<param>` is a list of `{t, v, interp}` (the reframe keyframes' form):
`t` in seconds from the clip's start (negative: from its end; a `RationalTime` works too),
`interp` (any `ease` above, `linear` default) shaping the move to the next key. Before the first
key the first value holds, after the last the last. A param may instead be an expression over
`lt` (seconds from the clip's start) and `dur` (the clip's length), e.g. `"opacity":
"0.5 + 0.5*sin(lt*6)"`, an array of constants and expressions (`"position": [0, "0.3 - 0.05*lt"]`),
or `{"keys": [...], "expr": "value * ..."}` ([compositing.md](compositing.md#expressions)).

| Param | `v` |
|---|---|
| `position` | `[x, y]` like `position`: the anchor moves there |
| `scale` | a number or `[x, y]`, about the anchor; text is rasterised at the largest scale (up to 4; an expression's is sampled on every frame) so it stays sharp |
| `opacity` | multiplies the alpha (shadow, outline and box too) |
| `colour` | `[r, g, b]` or `[r, g, b, a]`: replaces the text's colour (a `rect`'s fill) |

```json
"keyframes": {
  "position": [{"t": 0, "v": [-0.3, 0.35], "interp": "in_out"}, {"t": 2, "v": [0.3, 0.35]}],
  "scale": [{"t": 0, "v": 1}, {"t": -1, "v": 1, "interp": "out"}, {"t": -0.01, "v": 1.2}]
}
```

Animations and keyframes combine (an entrance on a moving caption). The text is still laid out
once per segment; each frame only moves, scales, crops and tints pieces of its masks
(`Overlay::motion`, the placements per frame), so it costs no extra GPU pass.

## Titles and end screens

An `ffcine.title` generator clip is a designed title sequence or end screen. It renders as a set
of text pieces stacked around `position` (the frame's middle by default) on the clip's track:
the title, a rule, the subtitle, the credit lines, and with `kind: "end"` a backdrop behind them.

```sh
ffcine title --into edit.otio "The Long Road" --subtitle "A short film"
ffcine title --into edit.otio --end "Thanks for watching" --line "Music: Example Artist"
```

| Parameter | Meaning |
|---|---|
| `kind` | `title` (default) or `end` |
| `title`, `subtitle` | text; `\n` breaks lines, `*marks*` highlight with a `highlight` colour |
| `lines` | credit lines: an array or a string with `\n` |
| `style` | project style for every piece's font, colour, shadow, ... (layout keys like `size` and `position` are the title's) |
| `title_style`, `subtitle_style`, `lines_style` | a style name or an object of text parameters merged over that piece's defaults (anything, including `size`, `animation`, `keyframes`) |
| `position`, `alignment` | where the stack sits and how it lines up (`center`; `left`/`right` hang it from `position`) |
| `gap` | space between pieces, fraction of the frame height (0.03) |
| `stagger` | seconds between the title's words (0.12; 0.15 for `end`) |
| `duration` | seconds each piece takes to come in (0.9) |
| `fade_out` | seconds everything takes to fade out at the clip's end (0.8) |
| `rule` | `false`, or `{width, thickness, colour, delay}` (0.12 of the frame width, 0.0025 of its height, the text's colour at 80 %); on by default when there's something under the title |
| `background` | a colour behind it all (`false` for none); `end` defaults to `[0, 0, 0, 0.6]` |

Defaults: the title at 8.5 % of the frame height (7 % for `end`), its words sliding up one by one
(fading in for `end`); the rule wiping out from the middle; the subtitle in spaced capitals at
3.2 % sliding in after the title; lines at 3.4 % coming in line by line. Each piece is an
`ffcine.text` clip internally (ids `<clip id>:title`, `:rule`, ...), so the cache, profiles'
safe areas and `ffcine still` treat it like any text.

## Subtitles from `.ass`

`ffcine subs SUBS.ass --into edit.otio [-o out.otio] [--offset S]` adds an `.ass`/`.ssa` file as
text clips: each ASS style becomes `project.styles."ass:<Name>"` (font, size scaled from
`PlayResY` to `frame_size`, colours, bold/italic, numpad alignment and margins, outline or opaque
box, shadow, spacing), each `Dialogue` a clip on a `Subtitles` track (overlaps spill onto
`Subtitles 2`, ...). Line-wide overrides at the start of a line are kept (`\pos`, `\an`, `\fad`,
`\b`, `\i`); `\N` breaks lines. Other override tags (per-span colours, karaoke, transforms,
drawings) are dropped and counted.

## Not yet

- Per-span styles beyond `*highlight*`.
- Keyframed rotation, and keyframes of parameters that need a new layout (`size`, `tracking`,
  the text itself): scale stands in for size.
- `profiles` on an `ffcine.title` clip: its pieces take the output profile's safe area but not
  per-profile overrides.
