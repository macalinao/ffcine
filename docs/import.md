# Importing other editors' projects

Importers turn a foreign project into an ffcine timeline (`docs/design.md`, timeline format):
editorial goes to core OTIO, effects to `ffcine.*` schemas.


## Foreign data round-trips

Anything an importer doesn't map is kept verbatim under `metadata.ffcine.foreign.<app>`
(`resolve`, `fcp_xml`, `fcpx`, `aaf`, `cmx_3600`, `mlt`, `capcut`) on the object it came from, and
written back on export to that app. Mapped fields are the source of truth once imported; the
foreign block is never re-read on render. For OTIO inputs the block is the adapter's namespace
(`Resolve_OTIO`, `fcp_xml`, `AAF`, `cmx_3600`) moved under `ffcine.foreign`.

## Importers, in priority order

1. **DaVinci Resolve native OTIO** (18.5+, seen to 21.x). Effects arrive as
   `Effect(effect_name = "Resolve Effect")` with `metadata.Resolve_OTIO = {Effect Name, Type,
   Parameters[]}`; dispatch on `Type`. Only non-default parameters are listed; keyframe keys are
   frames from clip start with optional `InBez`/`OutBez` handles.
   Implemented in `import::resolve` (`ffcine import --format resolve`, or any `.otio` with
   `Resolve_OTIO` timeline metadata); geometry stays in picture units, see its module docs.
   - The timeline has no frame size: the importer must be given it.
   - Pan/tilt/anchor/crop are fractions of the source as sized by `Retime and Scaling.scale`, not
     of the frame; zoom and rotation pivot on the anchor (maths in vidcut `docs/gotchas.md`).
   - No grade, CDL or LUT. Fusion titles, Text+ and Fusion comps are `MissingReference`s with just
     a name. Speed ramps don't appear (probably flattened; unverified).
   - Resolve drops unknown metadata, `ffcine` included, on re-import: round trips go through
     `metadata.ffcine.id` merges, not Resolve.
2. **FCP7 / Premiere XML** via `otio-fcp-adapter`, `metadata.fcp_xml`. `Effect.effect_name` is
   empty: dispatch on `fcp_xml.effectid` (`basic`, `crop`, `opacity`, `audiolevels`, `audiopan`,
   `timeremap`, `GraphicAndType`).
   - `timeremap` stays an opaque effect, not a `LinearTimeWarp`; `graphdict` keyframes map
     timeline frame -> source frame.
   - Every transition is written as `SMPTE_Dissolve`; the real effectid is only in its metadata.
   - Premiere titles: `GraphicAndType`'s `Source Text` is base64 of an 8-byte LE length plus
     UTF-16LE JSON (`mText`, `mFontName`, ...); unofficial format.
   - `audiolevels` is linear gain (0–3.98), `basic.center` is ±0.5 of the frame, scale in %.
     Lumetri isn't decodable (unverified blob); there is no CDL.
   Implemented in `import::fcp7` (`ffcine import --format fcp7`, or any `.xml` with `<xmeml`),
   reading the XML directly rather than through the adapter: `timeremap` does become a
   `LinearTimeWarp` (or `ffcine.time.ramp/1`), and transitions keep their effectid as
   `ffcine.transition/1 kind`. Units it assumes are listed in its module docs.
3. **CMX 3600 EDL + CDL** via `otio-cmx3600-adapter`, the colour path for Resolve and anything
   else: `*ASC_SOP`/`*ASC_SAT` -> `metadata.cdl`. Which Resolve node the CDL comes from is
   unverified (probably primaries only). `W###` wipe codes are dropped and `K`/`KB`/`KO` key
   events fail the parse. Implemented in `import::edl` (our own parser, not the adapter): wipes keep
   their code as `ffcine.transition/1 {kind: "wipe", smpte}`, key events are skipped (an error
   with `--strict`), and a `<stem>.ccc`/`.cdl` sidecar is matched to events by `ASC_CC_XML` id,
   clip name, reel or event number (`import::cdl` reads the files). AAF uses the same adapter family (`metadata.AAF`, `OperationGroup`s
   with `AvidEffectID`) and slots in here if needed.
4. **Our own FCPXML and Kdenlive/MLT XML readers.** The OTIO adapters keep only clips, gaps and
   markers: on `fcpx_example.fcpxml` the FCPXML adapter outputs 0 of 21 transitions and 0 of 21
   filters, and the Kdenlive adapter ignores `timewarp`, transforms, titles, LUTs and real wipe
   types. Our readers parse the XML directly.
   - FCPXML: `adjust-transform`, `adjust-crop`, `adjust-volume`, `adjust-blend`,
     `keyframeAnimation`, `timeMap`/`timept` (ramps), `title` + `text-style-def`,
     `info-asc-cdl` (colour board only). `position` units unverified.
   - MLT (Kdenlive and Shotcut): keyframe strings `time<op>=value`, where the op picks the
     interpolation (none linear, `|`/`!` hold, `~` smooth, `a`–`D` easings); rects are
     `"x y w h opacity"` in px or %. Shotcut filters are named by `shotcut:filter`.
5. **CapCut / JianYing `draft_content.json`**, last. No official format; structure from
   `pyJianYingDraft`/`pyCapCut`. Newer versions encrypt the draft (plaintext needs JianYing <= 5.9).
   Time is in µs; segments reference `materials` pools by id; `transform` is in half-canvas units,
   +y up. Transitions, filters and effects are store asset ids, so they can't be rendered.
   Implemented in `import::capcut` (`ffcine import --format capcut`, a draft `.json`, or the
   project directory); mapping and units in its module docs. Text size, `curve_speed` and the
   saturation/contrast/brightness scales are *unverified* guesses.

## Export

`import::export` writes a timeline back out for finishing in DaVinci Resolve; each exporter is
the inverse of its importer and reports what it approximated or left out.

- **Resolve OTIO** (`export::resolve`, `File > Import > Timeline`): core OTIO editorial,
  `foreign.resolve` restored to `metadata.Resolve_OTIO` everywhere (unedited sections verbatim),
  and transform, crop, reframe, blend/opacity, audio level and pan written as `Resolve Effect`s.
  `"units": "frame"` geometry is converted assuming the picture fills the frame. Other ffcine
  effects stay as they are (Resolve drops them); `metadata.ffcine` stays too, so ids merge back
  on our side.
- **FCPXML 1.10** (`export::fcpxml`), the second route: spine plus connected storylines per
  track, transitions as Cross Dissolves, `timeMap` for retimes and ramps, `adjust-*`, the CDL as
  `info-asc-cdl`, titles, markers, `foreign.fcpx` restored. Units are the reader's (unverified).
- **CDL**: Resolve's OTIO import ignores grades. `export::cdl_edl` writes a CMX 3600 EDL with
  `*ASC_SOP`/`*ASC_SAT` per event and `export::ccc` a `ColorCorrectionCollection` keyed by
  `metadata.ffcine.id`. A clip's CDL is its `ffcine.color.cdl/1`, else `metadata.cdl`, else its
  resolved grade approximated (gain/exposure, lift, gamma, saturation; LUT and contrast left out).

## Field map

Fidelity:
- **exact**: same result after a closed-form unit conversion.
- **approx**: rendered, but the curve, easing or maths differs.
- **preserved**: not rendered; kept in `ffcine.foreign.<app>` for export.
- **lost**: absent from what the app exports, so there is nothing to keep.

`(own)` = only through our own reader; *unverified* entries are from specs, not samples.

| Source | ffcine | Fidelity |
|---|---|---|
| Tracks, clips, gaps, source ranges, media refs (all) | OTIO core | exact |
| `LinearTimeWarp`, `FreezeFrame` (Resolve, AAF, EDL M2, Hiero) | same | exact |
| MLT `timewarp warp_speed` (own); CapCut `speed` | `LinearTimeWarp` | exact |
| FCP7 `timeremap` const speed | `LinearTimeWarp` | exact |
| FCP7 `graphdict`; FCPXML `timeMap` (own); Shotcut `timeremap`; AAF `PARAM_SPEED_*_MAP_U` | `ffcine.time.ramp/1` + average `LinearTimeWarp` | approx (curve interpolation; AAF needs Avid Bezier decoding) |
| CapCut `curve_speed` | `ffcine.time.ramp/1` | approx, *unverified* |
| Resolve speed ramps | n/a | lost |
| Resolve Transform; FCP7 `basic`; FCPXML `adjust-transform` (own); MLT `affine`/`qtblend` rect; CapCut `clip` | `ffcine.geo.transform/1` (keyframed) | exact after unit conversion (Resolve needs media and timeline size) |
| AAF DVE `DVE_POS/SCALE/ROT_*` | `ffcine.geo.transform/1` | approx (per-effect tables, units *unverified*) |
| Resolve Dynamic Zoom | `ffcine.geo.transform/1` keyframes | approx |
| Resolve `Retime and Scaling.scale` | `ffcine.geo.reframe/1` fit/fill/stretch | exact |
| Resolve Cropping; FCP7 `crop`; FCPXML `adjust-crop` (own) | `ffcine.geo.crop/1` | exact (Resolve `cropSoftness` approx) |
| Opacity: Resolve Composite, FCP7 `opacity`, CapCut `alpha`, MLT rect opacity | `ffcine.comp.blend/1 {opacity}` | exact |
| Blend modes: Resolve 1–26, FCPXML `adjust-blend` (own) | `ffcine.comp.blend/1 {mode}` | approx (edge cases differ) |
| Resolve Composite 27–31 (keys); EDL `K`/`KB`/`KO` | n/a | preserved / lost (EDL parse fails) |
| Resolve Video Faders | opacity keyframes | exact |
| Kdenlive `fadein`/`fadeout`, `fade_*_black` | opacity or `ffcine.audio.level/1` ramp | approx (audio vs video ambiguous) |
| Keyframes: AAF Linear/Constant, MLT linear/hold | `interp: linear` / `hold` | exact |
| Keyframes: Resolve/CapCut/AAF Bezier, FCPXML easings, MLT `~`/`a`–`D` | `interp: ease` | approx until ffcine has more curves |
| Volume: Resolve Clip Volume, FCP7 `audiolevels`, FCPXML `adjust-volume` (own), CapCut `volume`/`audio_fades`, Kdenlive `volume` | `ffcine.audio.level/1 {gain_db}` keyframes | exact (Kdenlive OTIO loses interpolation) |
| Pan: Resolve Clip Pan, FCP7 `audiopan`, AAF `Pan.ControlPoints` | `ffcine.audio.pan/1` | exact |
| AAF clip gain automation; Resolve EQ, Pitch | `ffcine.audio.*` / n/a | *unverified* / preserved |
| `SMPTE_Dissolve` + offsets (all) | `Transition(SMPTE_Dissolve)` | exact |
| Resolve transition ease/custom curve | transition easing | approx |
| Wipes: EDL `W###`, AAF `OperationGroup`, FCP7 effectid, MLT `luma` | `ffcine.transition/1 {kind}` on the `Transition` | approx (EDL code lost) |
| FCPXML `transition` (own) | `Transition` | exact for dissolves |
| CapCut transitions, filters, effects; MLT frei0r/movit; Avid CC, plugin filters | n/a | preserved |
| Resolve Rich Text; FCP7 text; Premiere `GraphicAndType`; FCPXML `title` (own); Shotcut `dynamictext`; CapCut `texts[].content` | `ffcine.text` generator | approx (text, font, colour, position; animation and templates lost) |
| Resolve Fusion titles, Text+, comps | placeholder `ffcine.text` clip | lost |
| Solid colour (Resolve, FCP7, Kdenlive `color`) | `ffcine.solid` generator | exact (Resolve drops alpha) |
| EDL `*ASC_SOP`/`*ASC_SAT`; FCPXML `info-asc-cdl` (own) | `ffcine.color.cdl/1` + `metadata.cdl` | exact (primaries only) |
| Shotcut `avfilter.lut3d` | `GradeParams.lut` | exact |
| CapCut `KFTypeSaturation/Contrast/Brightness` | `ffcine.color.grade/1` keyframes | approx |
| Lumetri, FCP colour wheels/curves, CapCut `hsl`/`color_curves` | n/a | preserved |
| Resolve Color page | n/a | lost |
| Markers: name, range, comment, colour (all; FCP7 has no colour) | `Marker` | exact |
| Resolve Link Group ID, compound `Stack` | link groups, nested `Stack` | exact |
| Reel, timecode, IDs: `cmx_3600.reel`/`comments`, `fcp_xml` file/logginginfo, AAF `MobID`/`UserComments`, `sg`, Resolve `Media Url` | `metadata.ffcine.source` | exact (relink hints) |
| Any other block | `ffcine.foreign.<app>` | preserved |
