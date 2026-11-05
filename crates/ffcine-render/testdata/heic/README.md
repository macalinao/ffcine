Synthetic HEIF fixtures for `backend/tiles.rs`, from ffmpeg's `testsrc2` (no camera images):

```sh
ffmpeg -f lavfi -i testsrc2=s=200x120 -frames:v 1 src.png
heif-enc -e x265 -q 90 --cut-tiles 64 -o grid.heic src.png                   # 4x2 grid of 64x64, cropped to 200x120
heif-enc -e x265 -q 90 --cut-tiles 64 --rotate-cw 90 -o grid-rot.heic src.png # the same with irot
heif-enc -e x265 -q 90 -o whole.heic src.png                                  # untiled reference
heif-enc -e x265 -q 90 --rotate-cw 90 -o whole-rot.heic src.png               # untiled, with irot
```

`heif-enc` is libheif's (1.23).
