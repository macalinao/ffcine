# Ingest, catalog and doctor

`ffcine ingest` offloads camera cards with verified copies, `ffcine catalog` indexes the clips
(format, exact frame rate, picture profile, corrected clock), and `ffcine doctor` checks the GPU
path before a render. Ingest and catalog live in `crates/ffcine-ingest` and the libav-free
probes in `ffcine-core` (`bmff`, `wav`, `profile`). Neither needs a GPU, libav or Linux, so they
run on the Mac in the van.

## Build

```sh
nix develop -c nice -n 19 cargo build --release -p ffcine-cli                         # full (Linux, GPU)
cargo build --release -p ffcine-cli --no-default-features                             # ingest/catalog/doctor only (macOS)
```

The `gpu` feature (on by default) adds probe/grade/batch/stabilize/jitter/import and the GPU
doctor checks, and links libav* and libplacebo. Without it the binary needs only a Rust
toolchain: no system libraries (`ldd` shows none of libav, libplacebo or Vulkan). The optional
frame-statistics check runs the `ffmpeg` CLI if it is on `PATH` (Homebrew's works).

## `ffcine ingest`

```sh
ffcine ingest /Volumes/CARD --to ~/footage/trip --backup /media/backup/footage/trip \
  --zone +00:00 --clock r10=-2m
```

- **Layout**: `DEST/01_footage/<camera>/<YYYY-MM-DD>/<original name>`, audio in
  `DEST/02_audio/<camera>/<day>/`. The day comes from the
  corrected clock (below); `undated` if no time is known. DJI `.LRF` proxies, `.SRT` and Canon
  `.THM` sidecars go next to their clip (they take its camera and day). Stills (JPEG, HEIC, CR3,
  DNG) go next to the footage.
- **Cameras**: the id comes from metadata: `pocket` (Osmo Pocket 4), `pocket3`, `action`,
  `dji` (other DJI, drones), `r10`, `r50`, `canon`, `iphone`, `h2e` (Zoom H2essential, from the
  WAV `bext` originator), `zoom`, `audio`, `other`. Path conventions are the fallback
  (`DJI_*`, `*CANON*`, `MVI_*`, `ZOOM*`). `--camera ID` forces one id for a whole card.
- **Verification**: each file streams from the card into `NAME.part` while BLAKE3 hashes what
  was read. The copy is fsynced, re-read and hashed, and renamed into place only if both hashes
  match (one retry, then the file fails). The camera's mtime is kept.
- **Backup**: `--backup DIR` makes a second copy with the same layout, from the verified
  destination (so the card is read once), verified against the card's hash. It runs in a
  second thread while the card copies.
- **Manifest**: `DEST/.ffcine/ingest.jsonl`, one JSON line per verified file, fsynced:
  `{"event":"copy"|"backup","source","size","mtime","blake3","dest","camera","kind","at"}`
  (`dest` is relative to DEST and to the backup root; times are Unix seconds).
- **Idempotent and resumable**: a card file whose name, size and mtime match a manifest entry,
  and whose copy is still there at that size, is skipped (`--verify-existing` re-hashes it).
  A missing or damaged copy is copied again in place. An interrupted copy leaves only a `.part`,
  which the next run overwrites. A torn last manifest line is ignored. Backups that never
  finished are queued again.
- **Collisions**: a different clip with the same name and day (Canon's counter reset across
  cards) is kept as `NAME_<blake3[..8]>.EXT`. If the destination already has the same content
  (an earlier run died before the manifest line), it is recorded without copying.
- **The card is read-only**: card files are only ever opened for reading. Nothing on the card
  is written, renamed or deleted, and DEST or `--backup` inside the card is refused. Hidden
  entries (`.Trashes`, `._*`, `.DS_Store`) are skipped; other unknown files (`*.CTG`, `MISC/`)
  are listed as ignored, not copied.
- `--dry-run` prints the plan. Per-file failures don't stop the run: the summary lists them and
  the exit code is non-zero, so re-run to retry. Afterwards DEST is catalogued (`--no-catalog`
  skips that).

## Clocks (`clocks.yaml`, `--clock`, `--zone`)

Files are never rewritten. The fix is config, applied to catalog timestamps and to the day
folder chosen at ingest:

```yaml
zone: "+00:00"            # cameras whose files don't record a zone (here UTC+0, no DST)
reference: iphone         # the camera the sync check trusts (network time)
cameras:
  pocket: { shift: "+0h" }
  r10:    { shift: "-2m" }               # clock 2 min fast
  h2e:    { shift: "+0s", zone: "+00:00" }
sync:                                    # one moment seen by two clips (a clap; audio sync later)
  - { a: "DJI_20261201101500_0003_D.MP4@12.4", b: "MVI_0042.MP4@3.1" }
```

`DEST/clocks.yaml` is read by default; `--clocks FILE`, `--clock CAM=SHIFT[@ZONE]` (repeatable)
and `--zone` override it. Shifts look like `+1h20m`, `-90s` or `+9497d` (for a clock reset to
2000). Zones are fixed offsets (`+08:00`), so no tz database is needed.

What each camera records, i.e. its `clock.source`:

| Camera | Wall time from | Zone from |
|---|---|---|
| DJI | file name `DJI_YYYYMMDDhhmmss_*` (`dji-filename`) | file name minus the UTC `mvhd` time |
| iPhone | `com.apple.quicktime.creationdate` (`apple-creationdate`) | the same key (`+0900`) |
| Canon | `mvhd`, which is local time (`mvhd-local`); CR3 EXIF `DateTime` | config |
| Zoom H2essential | WAV `bext` origination date/time (`bext`) | config |
| JPEG | EXIF IFD0 `DateTime` (`exif`) | config |
| anything else | file mtime, a UTC instant (`mtime`) | config |

The corrected local time is `camera time + shift`; the zone is the config's camera zone, else
the file's, else the default zone; the UTC time follows from the zone.

The checks land in the catalog's `clock_issues` (and in the CLI output):
- **Clock not set**: clips dated before 2015 or in the future (e.g. clips dated 2000 after a battery swap).
- **Zone disagreement**: one camera changing zone between clips, or cameras on different zones
  (e.g. the Pocket still on home time while the iPhone is on local time).
- **Sync points**: for each `sync` pair, the two clips' corrected start times plus the offsets
  must agree within 1.5 s. If not, the issue names the non-reference camera and suggests the
  `shift` that fixes it.
- **Overlaps**: clips from different cameras whose corrected times overlap are listed in
  `overlaps`. Those are the pairs for audio sync to cross-correlate. Its measured
  offsets go in as `sync` points.

## `ffcine catalog DIR`

Writes `DIR/catalog.json` (schema `ffcine.catalog/1`, written atomically). In a project root it
indexes only `01_footage/` and `02_audio/`, not renders or music. Re-runs reuse entries
whose size and mtime are unchanged (`--full` re-probes everything) and re-apply the current
clock config. Per clip:

```json
{
  "path": "01_footage/pocket/2026-12-01/DJI_20261201101500_0001_D.MP4",
  "kind": "video", "camera": "pocket", "model": "DJI OsmoPocket4",
  "size": 35278129, "mtime": 1783135541, "day": "2026-12-01",
  "clock": { "camera_time": "...", "camera_zone": "+08:00", "shift": "+0s",
             "local": "...", "zone": "+00:00", "utc": "...", "source": "dji-filename" },
  "duration": 12.012,
  "video": { "codec": "hvc1", "width": 3840, "height": 2160, "fps": "60000/1001",
             "fps_value": 59.94, "frames": 720, "bit_depth": 10, "chroma": "4:2:0",
             "primaries": "bt709", "transfer": "bt709", "matrix": "bt709", "range": "tv" },
  "audio": { "codec": "mp4a", "sample_rate": 48000, "channels": 2 },
  "profile": { "name": "dji-dlog", "confidence": "high",
               "evidence": "dji cam-meta color mode 2 on DJI OsmoPocket4 (D-Log)",
               "source_kind": "log", "camera_lut": "dji-pocket4-dlog" },
  "proxy": "01_footage/pocket/2026-12-01/DJI_20261201101500_0001_D.LRF"
}
```

Plus `cameras` (per camera: clips, seconds, bytes, clip count per exact fps and per profile,
days), `warnings`, `clock_issues` and `overlaps`. The frame rate is exact (`NUM/DEN` from the
`stts` timing): 59.94 is `60000/1001`, not 60. `warnings` flags any camera-day with more than
one rate, and any day where cameras mix NTSC (`x/1001`) and integer rates. It also lists every
clip whose profile is only a guess. JSON only for now: no SQLite.

Schema changes stay additive (new optional fields); readers must ignore unknown keys.

### Picture profile detection

The grade picks the camera LUT from `profile.camera_lut`, or uses `source_kind` (`log` takes
the camera LUT; `display` takes the light rule; `hdr` is passthrough or tone-mapped).

| Profile (`name`) | Detected from | Confidence |
|---|---|---|
| `hlg` | `colr` transfer 18 (arib-std-b67): DJI HLG, iPhone HDR; `dolby_vision` if a `dvcC`/`dvvC` box is present (iPhone DV 8.4) | high |
| `pq` | `colr` transfer 16 (smpte2084): Canon HDR PQ | high |
| `dji-dlog` | DJI `CAM meta` colour mode 2 on an Osmo Pocket 4 (D-Log, D-Gamut) | high |
| `dji-dlog-m` | colour mode 2 on an Osmo Pocket 3, Mini 4 Pro or Osmo Action | medium |
| `dji-dlog2` | colour mode 2 on an Osmo Pocket 4P | medium |
| `rec709` (DJI) | no colour mode in `CAM meta` (Normal) | medium |
| `clog3` | Canon, 10-bit, no HDR tag (R10/R50 only shoot 10-bit in HDR PQ, so this is for other bodies) | medium |
| `rec709` (Canon) | Canon 8-bit (log needs 10-bit) | high |
| `apple-log` | Apple metadata naming Apple Log (high); iPhone BT.2020 with a non-HDR, non-709 transfer (medium) | high/medium |
| `rec709` (iPhone) | iPhone SDR tags | medium |
| `rec709` / `unknown` | 8-bit with no profile metadata / anything else | low |

Why DJI needs more than the tags: Osmo Pocket files are tagged `bt709/bt709/bt709` 10-bit in
both D-Log and Normal. The mode lives in the `CAM meta` (`djmd`) track, whose first sample is a
protobuf clip header (`dvtm_osmo_pocket_4.proto`). Field 2 → 2 → 3 → 1 is `2` on log clips and
absent on Normal ones. This was measured on a library of Pocket 4 files, 2026: 428 with mode 2
and 22 without. Nine Mini 4 Pro clips have no mode either. Frame statistics agree: mode-2 frames have 10-bit black at
155–205 and mean saturation 4–14, while Normal frames have black at 56–64 and saturation 43–59.
DJI publishes no schema for this field, so the HLG value (`1`) is assumed. HLG files carry the
HLG tag anyway, and the tag wins.

**Frame statistics** (`--histogram auto|always|never`, default `auto`): when the metadata
verdict isn't high confidence, the catalog runs `ffmpeg … signalstats` on one frame (a third of
the way in, at most 30 s) and records `profile.stats` (black, 10th-percentile luma, saturation,
0–1). A lifted black (> 0.11) with low saturation (< 0.025) reads as log. The stats only confirm
or flag ("disagrees with metadata, check by eye"), or name the curve when nothing else does.
That result is always `low` confidence, because fog, snow and night scenes look flat too.

## `ffcine doctor`

It opens the real thing for each check, prints `ok` / `FAIL` per check, and exits 1 if any fail:

- the `ffmpeg` on `PATH` (what the prototype `render.py` runs): version, `libplacebo` and
  `nlmeans_vulkan` filters, `cuda` and `vulkan` hwaccels, `hevc_nvenc`
- the linked libav: versions, a CUDA device, a Vulkan device, CUDA↔Vulkan interop (the
  zero-copy grade), NVDEC configs for HEVC and H.264, real NVENC sessions (HEVC, H.264), and the
  `libplacebo` and `nlmeans_vulkan` filters
- libplacebo's API version, and NVOF (optical-flow API version, for stabilize)

In a build without `gpu` (the Mac), the GPU checks print `info` lines and the exit code stays 0,
because that binary has no render commands that could fall back to the CPU.

As of 2026-11-01 on the render box: inside `nix develop` (the GPU ffmpeg from #8) every check
passes. Outside it, the user-profile `ffmpeg` 9.0.1 on `PATH` has no `nlmeans_vulkan`, so
`doctor` fails there. That is the binary the prototype's denoise calls.

## Not done yet

- Audio cross-correlation between cameras and the H2essential. Ingest
 gives it the
  `overlaps` and takes back `sync` points.
- A Pocket 3 (D-Log M) or Pocket 4P (D-Log2) sample to confirm that colour mode 2 means the
  same on those bodies, an untagged DJI HLG sample, and Apple Log and Canon C-Log / HDR PQ
  samples. Only Pocket 4 D-Log/Normal, Canon R10 8-bit and iPhone 16 Pro Max HLG were on disk.
- Free-space check before copying. Copying stops with per-file errors when the disk fills; the
  re-run resumes.
- SQLite output, if a query tool ever needs it.
