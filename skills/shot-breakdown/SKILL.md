---
name: shot-breakdown
description: Turn a reference video into a shot list to recreate it. Use when asked for a "shot list of this tiktok/instagram/youtube video", to "break down this reel", "how was this edited/shot", or "recreate this video" with a link or a local file.
---

# Shot breakdown

Turns a reference video into a shot list someone can go out and shoot: per shot, what it is, what
happens, how the camera moves, and how to get it on a Canon R10 or a DJI Osmo Pocket 4. ffcine does
the measuring (cuts, keyframes, motion, beat, speech); you do the reading.

## Steps

1. **Fresh temp dir**, removed at the end whatever happens:

   ```sh
   dir=$(mktemp -d) && trap 'rm -rf "$dir"' EXIT
   ```

   Run everything below in one shell so the trap fires, or `rm -rf "$dir"` yourself once the
   reply is sent.

2. **Download** (skip for a local file):

   ```sh
   yt-dlp -q -f 'bv*[height<=1080]+ba/b[height<=1080]/b' --merge-output-format mp4 \
     -o "$dir/ref.mp4" 'URL'
   ```

   Private, friends-only, age-gated and login-walled posts fail here (`login required`, `private`,
   `Sign in to confirm your age`). Say so and ask for the file itself; don't try to get around it.

3. **Analyse:**

   ```sh
   ffcine breakdown -o "$dir/out" "$dir/ref.mp4"
   ```

   Add `--no-transcribe` when there is no speech worth having (it is the slow part). For very fast
   cutting, `--min-shot 0.1 --threshold 0.22`.

4. **Read** `$dir/out/breakdown.json` (`pacing`, `music`, each shot's `start`, `duration`,
   `motion`, `cut_on_beat`, `speech`, `text`) and **look at** `$dir/out/contact-sheet.jpg` (and
   `contact-sheet-2.jpg`... for long videos). Open a shot's `keyframes/NNN-first.jpg` /
   `NNN-last.jpg` when the move or the action isn't clear from one frame. If the sheet shows a
   missed cut (two different shots in one cell) or a split (the same shot twice), say so in the
   list rather than re-running blindly.

5. **Write the shot list**, one row per shot:

   | # | Time | Dur | Shot | Action | Camera | Recreate |
   |---|---|---|---|---|---|---|

   - **Shot**: wide / medium / close / insert (a detail: hands, object, texture), from the keyframe.
   - **Action**: what happens, in a few words; quote on-screen text or speech when it matters.
   - **Camera**: the `motion` label (`static`, `pan left`, `push`, `handheld`...), corrected by
     what you see: a subject crossing a locked frame can read as a pan, a walk forward as a push.
   - **Recreate**: the kit, framing and move, e.g. "R10, 18-45 at 18, waist height, slow push on
     foot" or "Pocket 4, follow mode, low angle, walk backwards".

   Above the table, one line on the edit: shot count, median shot length, and the BPM with
   `on_beat`/`cuts` against `chance` (well above chance means it is cut to the music: tell them to
   cut on the beat). Under it, the frame rate to shoot at.

6. **Reply** to whoever asked with the shot list and the contact sheet image attached, then clean
   up the temp dir.

## Recreating it

- **Canon R10** (APS-C, 1.6x crop, no in-body stabilisation): wide = RF-S 18-45 at 18 mm; medium =
  ~30-45 mm; close and insert = RF 50 mm f/1.8 or the kit lens at the long end, close in. Static
  shots on a tripod or a solid surface. Handheld moves need lens IS plus movie digital IS, or a
  gimbal; fast pans want a shutter near 1/50 at 25p or 1/60 at 30p for natural blur.
- **DJI Osmo Pocket 4** (gimbal, one wide lens): the default for moving shots: follow mode for
  walks and pushes, tilt-locked for pans, FPV for rolls and whips. Close-ups mean getting
  physically close; inserts from a hand's length.
- **Frame rate**: match the source (`fps`: 24/25/30). Shots that look slowed down: shoot 50/60 or
  higher and conform in the edit.
- **Cut on the beat** when `music` says the edit does: note which shots land on it (`♪` in
  `shotlist.md`).

## Keep it short

A table and two or three lines, not an essay. Don't re-describe what the contact sheet already
shows. Don't keep or re-post the downloaded video.
