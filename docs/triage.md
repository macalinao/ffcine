# Triage

`ffcine triage DIR` turns a card dump into a searchable clip index before anyone edits or renders.
It replaces a manual process: tens of thousands of frame grabs for contact sheets, an ad-hoc VAD
speech map, whisper run clip by clip through a job queue (one hour of waiting for one second of
work per clip), and a hand-built clip list for the editor.

```sh
ffcine triage /media/footage/trip-card-dump \
  --immich https://photos.example/api --immich-key-file /path/to/immich-api-key --camera-tz +09:00
ffcine search konbini                    # clip + timecode per hit
ffcine snap DJI_20261028114331_0285_D 2.7 7.3   # in/out moved onto word boundaries
```

Output goes to `DIR/triage/` (`-o` to change it):

| File | What |
|---|---|
| `index.json` | every clip's analysis ([schema](#index-schema)) |
| `index.html` | the clip page, self-contained apart from `strips/` |
| `strips/<clip>.jpg` | contact strip, 144 px high |
| `clips/<clip>.json` | per-clip cache, keyed by source size + mtime |

Re-running is cheap: cached clips are skipped, missing transcripts and locations are filled in,
and flags are recomputed. `--force` re-analyses everything.

## What it does per clip

- **Proxy first.** DJI `.LRF` proxies (720p HEVC beside each `.MP4`) are decoded on the CPU.
  Clips without one (phone `.MOV`s) decode on NVDEC (`hevc_cuvid`/`h264_cuvid`) with its hardware
  `resize`, so only thumbnail-sized frames leave the GPU; the CPU decoder is the fallback
  (`--cpu-decode` forces it). Clips over 10 minutes decode keyframes only (DJI: one per 1.25 s).
- **Contact strip.** 4-10 evenly spaced frames plus up to 6 at scene cuts, side by side.
- **Scene cuts.** 4 samples/s at 64x36 luma; the score mixes mean absolute difference with a
  32-bin histogram distance, and a cut must also be 3x the recent median, so walking pans don't
  fire. Per-second picture activity and brightness are kept for the flags.
- **Speech map.** An energy VAD on 16 kHz mono, band-passed to 200-3800 Hz so wind rumble counts
  less, 12 dB over the clip's noise floor, pauses under 0.35 s bridged.
- **Transcripts.** Clips with at least `--min-speech` (1 s) of speech go through one
  faster-whisper process for the whole run: the model loads once, then each clip's audio is fed
  in turn (`large-v3-turbo`, `int8_float16`, CUDA, Silero VAD on, word timestamps). No tool
  queue, no per-clip model load. The worker is a small Python script embedded in ffcine; it needs
  a Python with `faster_whisper` and CUDA `ctranslate2`: `--whisper-python` or
  `FFCINE_WHISPER_PYTHON` (with Nix, e.g. `pkgsCuda.python3.withPackages (p: [ p.faster-whisper ])`).
  `--whisper-cpu` and `--language` exist; `--hotwords "konbini, ramen"` biases it towards words it otherwise spells out; `--no-transcribe`
  skips it.
- **Flags.**
  - `runaway`: a take of 20 min or more (`--runaway-minutes`), summed over the camera's file
    splits (consecutive DJI clips whose start is within 3 s of the previous end form one take).
    When it ends dead, the flag's span is the tail to drop.
  - `dead`: 3+ minutes of near-frozen or black picture with no speech (pocket, table).
  - `dark`: over 80% black. `blip`: under 1.5 s. `no-audio`.
- **Location** (optional). With `--immich`, the GPS of the phone photo nearest the clip's
  middle (within the clip ±15 min) in Immich, with its city. The camera clock has no zone, so
  `--camera-tz` says which one it was on; clips from an ingest catalog use its corrected UTC time
  instead, so its per-camera clock fixes carry over. The Immich URL can be anything
  reachable, and `ALL_PROXY`/`HTTP_PROXY`/`NO_PROXY` are honoured; the client has no TLS, so the
  URL (or the proxy) must be plain `http://`. The key file also defaults to
  `$FFCINE_IMMICH_API_KEY_FILE`, as `ffcine publish` (which uses `curl`, so the same proxy
  variables work there too, `https` included).

## Search

`ffcine search QUERY` reads `triage/index.json` (or `--index`, or `FFCINE_TRIAGE_INDEX`). Words are
folded (case, accents, punctuation) and each query word must prefix consecutive transcript words:
`konbini` finds "Konbini's", `"the konbini"` only that phrase. A few crew words also search what
whisper writes instead (`konbini` -> "convenience store", "Family Mart", ...; `search::ALIASES`). `--paths` prints the source path
instead of the clip id. The clip page has the same search, client-side.

## The clip page

`index.html` lists every clip with its strip, camera time, length, language, place, flags and
transcript. It searches and filters (speech, flagged, undecided, used, skipped), and each clip
has Use / Skip and a note. Picks stay in the browser (localStorage, per footage root) and leave
through **Export** as `approvals.json`:

```json
{"root": "/media/footage/trip-card-dump",
 "clips": [{"id": "DJI_..._0285_D", "source": "DJI_..._0285_D.MP4", "pick": "use", "note": ""}]}
```

**Import** loads one back, e.g. on another device. `ffcine_triage::page::Approvals` reads it
for the next steps (assembly, edit.yaml). The page and `strips/` can be copied anywhere
(Immich album, a share); nothing else is needed.

## Snapping in/out points

`ffcine snap CLIP IN OUT` (times in seconds or `M:SS.ss`) and `ffcine_triage::snap` move proposed
points so a line is never cut mid-word (like a cut that ends on "in ni-"). Units are the
transcript's words, or the VAD speech spans for clips without one.

- A point inside a word moves to its nearer edge: an out-point late in a word keeps the word, one
  early in it drops the word. `--keep` always keeps the word.
- The point then sits `--pad` (0.12 s) into the pause next to that word, but never past the
  middle of the pause.
- A point already in a pause stays, unless it's closer than the pad to a word.

Assembly and the importers should call `snap::snap` on every cut that comes from a transcript
pick.

## Index schema

`index.json` is `{version, root, clips: [Clip]}`. Version 1 fields per clip:

| Field | |
|---|---|
| `id` | file stem, unique in the index (parent dir prefixed on a clash) |
| `source`, `analysed` | camera original, and the file actually decoded (the `.LRF`) |
| `size`, `mtime` | cache key |
| `duration`, `fps`, `width`, `height` | of the source |
| `recorded` | local start, `YYYY-MM-DDTHH:MM:SS`, no zone: the catalog's corrected clock, else the DJI name's camera clock |
| `utc` | UTC start from the catalog, when known |
| `take` | id of the first clip of the take, when the camera split a recording |
| `strip`, `strip_times` | strip path (relative to the index) and the time of each frame |
| `cuts` | scene cuts, seconds |
| `activity`, `luma` | per second, 0..1 |
| `has_audio`, `speech` | speech spans `{start, end}` |
| `transcript` | `{language, model, segments: [{start, end, text, words: [{start, end, text, prob}]}]}` |
| `flags` | `[{kind, detail, span?}]`, kind one of `runaway`, `dead`, `dark`, `blip`, `no-audio` |
| `location` | `{lat, lon, name?, source}` (`immich:<asset id>`) |

Changes are additive: new fields are optional and readers ignore unknown ones. `version` bumps
only if a field changes meaning, which also invalidates the per-clip caches.

## Clips: the ingest catalog

When `DIR/catalog.json` exists (`ffcine ingest` / `ffcine catalog`), triage takes its video clips,
their `.LRF` proxies and their corrected clocks: `recorded` is the catalog's fixed local time and
`utc` its UTC instant, which the Immich lookup uses directly (no `--camera-tz` needed). Without a
catalog (e.g. a dump made before ingest existed) it scans `DIR` for video files, skipping `06_exports`,
`MISC`, `triage` and dot-directories, and reads camera times from DJI file names. `--list FILE`
(one path per line) or `--no-catalog` override that. `clips::ClipFile` is the seam.

## Sample run

A 2-day travel card dump (2026-11-01, release build, `nice -n 19`, on a shared machine while
other jobs ran; timings are indicative only):

- 338 clips, 5.4 h: 308 Pocket clips with `.LRF` proxies, 29 phone `.MOV`s and 1 `.MP4` (NVDEC).
- 13 min 25 s wall in total, including transcribing 263 clips (30.7k words) in one whisper
  process. No failures. Output: 21 MB (`index.json` 2.6 MB, `index.html` 290 KB, the rest strips).
- 146 scene cuts. One runaway (the 22 min dinner, `DJI_20261028120719_0292_D`, which is real
  footage: the flag is a prompt, not a verdict), 5 blips, 1 clip without audio. The card has no
  4.5 h take; the camera split none of its recordings.
- Languages: 250 clips `en`, 8 `ja`; 5 short clips were misdetected (`es`, `fr`, `ko`, `zh`).
  Use `--language en` for an English-led card.
- Search: `konbini` finds 15 hits (whisper writes "convenience store" / "Family Mart", so search
  has an alias table), `udon` 13, `espresso` 2.
- Locations: a cached re-run with `--immich ... --camera-tz +09:00` placed 287 of 338 clips in
  1.5 s (spread over 5+ towns, the largest with 72 clips).
- Re-runs with everything cached take about a second.

## Not done yet

- Speaker labels (whose line is it), and translating non-English lines.

- Dawarich as a location source (better than photo GPS when nobody took photos).
- Feeding `approvals.json` into assembly/edit.yaml.
- Playing the proxy from the clip page (browsers can't all play HEVC `.LRF`).
