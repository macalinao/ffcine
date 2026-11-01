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
| `fade_in`, `fade_out` | seconds; `animation.in/out.duration` wins. Other animation kinds fade for now |
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

## Subtitles from `.ass`

`ffcine subs SUBS.ass --into edit.otio [-o out.otio] [--offset S]` adds an `.ass`/`.ssa` file as
text clips: each ASS style becomes `project.styles."ass:<Name>"` (font, size scaled from
`PlayResY` to `frame_size`, colours, bold/italic, numpad alignment and margins, outline or opaque
box, shadow, spacing), each `Dialogue` a clip on a `Subtitles` track (overlaps spill onto
`Subtitles 2`, ...). Line-wide overrides at the start of a line are kept (`\pos`, `\an`, `\fad`,
`\b`, `\i`); `\N` breaks lines. Other override tags (per-span colours, karaoke, transforms,
drawings) are dropped and counted.

## Not yet

- `slide`, `pop`, `typewriter` animations (they fade); per-span styles beyond `*highlight*`.
- Keyframed text parameters.
- The prototype's designed title sequence and end screen (word stagger, rules) are not text
  clips; they need their own generator.
