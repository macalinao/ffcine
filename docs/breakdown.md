# Breakdown

`ffcine breakdown VIDEO` turns a reference video (a reel, a TikTok, a trailer) into a shot list to
recreate it from: where it cuts, what each shot looks like, how the camera moves, whether the cuts
follow the music, and what is said and written on screen. It replaces doing that by hand with a
scene filter, a frame grab per shot and a stopwatch.

```sh
yt-dlp -o ref.mp4 'https://...'   # ffcine reads local files only
ffcine breakdown ref.mp4          # -> ref-breakdown/
```

For agents, [skills/shot-breakdown](../skills/shot-breakdown/SKILL.md) goes from a link to a
shot list with how to recreate each shot.

Output goes to `NAME-breakdown/` beside the video (`-o` to change it):

| File | What |
|---|---|
| `breakdown.json` | every shot's analysis ([schema](#breakdownjson)), for agents and scripts |
| `shotlist.md` | `#`, time, length, motion, speech/text, an empty notes column, keyframe path |
| `contact-sheet.jpg` | the keyframes in a grid, numbered, with each shot's length; under 5 MB for chat; past 12 rows, `contact-sheet-2.jpg`... |
| `keyframes/NNN.jpg` | each shot's representative frame (960 px long side); `NNN-first.jpg`, `NNN-last.jpg` |

## What it does

- **Cuts.** One decode at 160 px on the long side: NVDEC (`*_cuvid`) with its hardware resize
  when it takes the codec, else the CPU decoder (`--cpu-decode` forces it). Every frame goes
  through triage's scene detector (luma difference plus histogram distance, against the recent
  median so a steady pan doesn't fire); `--threshold` and `--min-shot` tune it. Dissolves and
  whip-pan transitions can be missed or split; check the contact sheet.
- **Keyframes.** A second decode at keyframe size writes, per shot, the frame one in from each end
  (so a soft cut doesn't leak in) and the sharpest frame of the middle 60 % (Laplacian variance,
  favouring the centre) as the representative.
- **Camera motion.** Between consecutive frames: a global shift found coarse-to-fine over a
  three-level pyramid, refined per textured 16 px block, the shift taken as the blocks' median and
  the zoom as the median of their divergence. Per shot the steps are summed into pan (frame
  widths), tilt (frame heights) and zoom (scale ratio); the path minus its ~0.4 s moving average
  is the shake. The biggest of pan, tilt and 2 x ln(zoom) over 0.08 names the move (`pan
  left/right`, `tilt up/down`, `push`, `pull`); otherwise shake over 0.25 % of the width is
  `handheld` and less is `static`. A moving shot with shake is marked `(handheld)`. It is global
  motion: a subject crossing a locked-off frame can read as a pan, and a walk forward is a push.
- **Beat.** The soundtrack's tempo and beats (librosa's tracker, as `ffcine beats`). A cut within
  `--beat-tolerance` (0.07 s) of a beat is on it; `chance` is the share that would be by luck at
  that tempo, so `on_beat / cuts` well above it means the edit is cut to the music. Speech-only
  audio still gets a tempo; read it with that in mind.
- **Colour.** Per shot from the analysis frames: mean colour, mean luma, luma 5/50/95th
  percentiles, saturation and warmth (mean R minus mean B).
- **Speech.** faster-whisper with word times, as triage (`--model`, `--whisper-python`,
  `--language`, `--whisper-cpu`); each shot gets the words whose middle falls in it. Skipped
  quietly when the video has no audio or no Python with faster-whisper (`--no-transcribe`).
- **On-screen text.** `tesseract` on each representative keyframe when it is on `PATH`, with
  noise lines dropped (`--no-ocr`).

Phone files with a rotation tag are analysed unrotated; downloads from the platforms are upright.

## breakdown.json

```json
{
  "version": 1,
  "source": "ref.mp4", "duration": 14.2, "fps": 30.0, "width": 1080, "height": 1920,
  "frames": 426, "decoder": "nvdec",
  "pacing": {"shots": 9, "mean": 1.58, "median": 1.4, "shortest": 0.6, "longest": 3.1,
             "cuts_per_minute": 33.8},
  "cuts": [1.4, 2.0],
  "shots": [{
    "n": 2, "start": 1.4, "end": 2.0, "duration": 0.6, "frames": [42, 60],
    "keyframes": {"key": "keyframes/002.jpg", "key_time": 1.7,
                  "first": "keyframes/002-first.jpg", "last": "keyframes/002-last.jpg"},
    "motion": {"kind": "pan", "direction": "right", "handheld": true, "pan": 0.31,
               "tilt": -0.02, "zoom": 1.01, "shake": 0.42, "speed": 0.55},
    "colour": {"mean": "#8a6f55", "luma": 0.45, "p5": 0.08, "p50": 0.44, "p95": 0.9,
               "saturation": 0.33, "warmth": 0.13},
    "cut_on_beat": true, "beat_offset": -0.012, "beats": 1.2,
    "speech": "okay here we go", "text": "DAY 1"
  }],
  "music": {"tempo": 120.2, "beats": [0.49, 0.99], "tolerance": 0.07, "on_beat": 7,
            "cuts": 8, "chance": 0.28},
  "transcript": {"language": "en", "model": "large-v3-turbo", "segments": []},
  "contact_sheets": ["contact-sheet.jpg"],
  "shotlist": "shotlist.md"
}
```

Times are seconds from the first frame (beats and words shifted onto it when the audio starts
elsewhere); `end` is the next shot's `start`; `frames` is the first frame and one past the last. `cut_on_beat` and `beat_offset` (nearest beat minus the cut) are
absent for the first shot and without music; `beats` is the shot's length in beats. `speech` and
`text` are absent when empty. `transcript` is triage's format.
