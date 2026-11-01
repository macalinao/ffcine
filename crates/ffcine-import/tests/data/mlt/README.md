# MLT fixtures

- `kdenlive_example_v240770.kdenlive` (Kdenlive 24.07 sequence layout),
  `kdenlive_mixes_markers.kdenlive` (Kdenlive 22.11, same-track mixes and markers): KDE's
  `kdenlive-opentimelineio` adapter, `tests/sample_data/` at 9a39b3bb402b726a3113268b558fe6f886595f56
  (https://invent.kde.org/multimedia/kdenlive-opentimelineio). Sample data is CC0 per the repo's
  `.reuse/dep5`; the repo licence is MIT (`LICENSE-kdenlive-opentimelineio.txt`).
- `melt_mix.mlt`: written by `melt` 7.42 (nixpkgs) from
  `melt -profile atsc_1080p_25 color:red out=49 color:blue out=49 -mix 10 -mixer luma -mixer mix:-1
  -track color:green out=99 -attach-track affine transition.rect=10%/10%:50%x50%:80 -consumer xml`.
- `features.kdenlive`, `features.mlt`: hand-written for ffcine in Kdenlive 23 and Shotcut 24
  layouts, not saved by either application.
