#!/usr/bin/env bash
# NVOF before/after (docs/denoise.md#motion-nvof-default-or-vulkan-block-matching): wall fps and the
# denoiser's report (GPU ms, wall ms/frame) for no denoise, block matching and NVOF at r2, repeated,
# for one or two ffcine binaries; then whether their NVOF outputs match (NVENC qp 0, no dither; the old path itself varies run to run, so repeat on a mismatch).
# Inside `nix develop`, from the repo root.
#   bench/denoise/nvof.sh CLIP [SS]
#   BEFORE=/path/to/old/ffcine bench/denoise/nvof.sh CLIP 428
set -euo pipefail
W=${W:-/tmp/ffcine-denoise/nvof}
FF=${FF:-./target/release/ffcine}
BEFORE=${BEFORE:-}
N=${N:-96}
RUNS=${RUNS:-3}
clip=$1 ss=${2:-0}
mkdir -p "$W"

modes=(none blocks nvof)
args() {
  case $1 in
  none) ;;
  *) echo "--temporal-radius 2 --temporal-strength 3 --temporal-motion $1" ;;
  esac
}

bins=("after:$FF")
[ -n "$BEFORE" ] && bins=("before:$BEFORE" "${bins[@]}")
for b in "${bins[@]}"; do
  tag=${b%%:*} bin=${b#*:}
  for mode in "${modes[@]}"; do
    for run in $(seq "$RUNS"); do
      # A shared GPU may be out of memory for a moment: retry a few times.
      for _ in 1 2 3 4 5; do
        # shellcheck disable=SC2046
        out=$(nice -n 19 "$bin" grade --no-cache --rate qp=4 --ss "$ss" --frames "$N" $(args "$mode") \
          "$clip" "$W/$tag-$mode.mkv" 2>&1 || true)
        if grep -q 'frames in' <<<"$out" && { [ "$mode" != nvof ] || grep -q 'motion nvof' <<<"$out"; }; then
          grep -E 'denoise:|frames in' <<<"$out" | sed -E 's/ Denoise \{.*//' | sed "s/^/  $tag $mode $run: /"
          break
        fi
        sleep 30
      done
    done
  done
  # Lossless and undithered, so two binaries can be compared frame by frame.
  # shellcheck disable=SC2046
  for _ in 1 2 3 4 5; do
    # shellcheck disable=SC2046
    nice -n 19 "$bin" grade --no-cache --dither none --rate qp=0 --ss "$ss" --frames "$N" \
      $(args nvof) "$clip" "$W/$tag-exact.mkv" 2>&1 | grep -q 'motion nvof' && break
    sleep 30
  done
done
if [ -n "$BEFORE" ]; then
  h() { ffmpeg -loglevel error -i "$1" -f framemd5 - | grep -v '^#' | md5sum | cut -c1-12; }
  echo "  nvof output md5 (NVENC qp 0, no dither): before $(h "$W/before-exact.mkv"), after $(h "$W/after-exact.mkv")"
  ffmpeg -hide_banner -i "$W/before-exact.mkv" -i "$W/after-exact.mkv" -lavfi psnr -f null - 2>&1 |
    grep -o 'PSNR.*' | sed 's/^/  before vs after: /'
fi
