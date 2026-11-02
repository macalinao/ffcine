#!/usr/bin/env bash
# The presets against the settings they replaced (docs/denoise.md#presets). Inside `nix develop` with
# numpy on PATH, from the repo root, after `cargo build --release`. Lossless x264: at any lossy
# rate, even x265 at CRF 0, the encoder itself takes half the height off a faint point.
#   bench/denoise/presets.sh real NAME CLIP SS   # patches.py against the undenoised render
#   bench/denoise/presets.sh synth stars|stars-loud|stars-blotchy|snow   # synth.py vs ground truth
#   bench/denoise/presets.sh all [CLIP[@SS]...]  # every synthetic scene, then each clip (SS 0)
set -euo pipefail
W=${W:-/tmp/ffcine-denoise/presets}
FF=${FF:-./target/release/ffcine}
N=${N:-24}
D=bench/denoise
ENC=(--no-cache --codec libx264 --preset ultrafast --tune psnr --profile high444 --rate qp=0
  --pix-fmt yuv420p10le --frames "$N")
OLD=(--temporal-radius 2 --temporal-strength 3 --denoise 1.5 --denoise-chroma 2.5)

grade() { nice -n 19 "$FF" grade "${ENC[@]}" "$@" </dev/null 2>&1 | grep -E 'denoise:|Error' | cut -c1-60 || true; }

case $1 in
real)
  name=$2 clip=$3 ss=$4
  mkdir -p "$W"
  grade --ss "$ss" "$clip" "$W/$name-base.mkv"
  run() {
    local o=$1
    shift
    grade --ss "$ss" "$@" "$clip" "$W/$name-$o.mkv"
    echo "$name $o: $(nice -n 19 "$D/patches.py" "$W/$name-base.mkv" "$W/$name-$o.mkv")"
    rm -f "$W/$name-$o.mkv"
  }
  echo "$name base: $(nice -n 19 "$D/patches.py" "$W/$name-base.mkv" "$W/$name-base.mkv")"
  run old "${OLD[@]}"
  run night --denoise-preset night
  run snow --denoise-preset snow
  rm -f "$W/$name-base.mkv" ;;
synth)
  scene=$2
  S=$W/$scene
  [ -f "$S/truth.npz" ] || nice -n 19 "$D/synth.py" gen "$scene" "$S"
  grade "$S/clean.mkv" "$S/ref.mkv"
  run() {
    local o=$1
    shift
    grade "$@" "$S/noisy.mkv" "$S/$o.mkv"
    echo "$scene $o: $(nice -n 19 "$D/synth.py" score "$S/$o.mkv" "$S/ref.mkv" "$S/truth.npz")"
    rm -f "$S/$o.mkv"
  }
  run none
  run old "${OLD[@]}"
  run night --denoise-preset night
  run snow --denoise-preset snow ;;
all)
  shift
  for scene in stars stars-loud stars-blotchy snow; do "$0" synth "$scene"; done
  for c in "$@"; do "$0" real "$(basename "${c%@*}" | tr -c 'A-Za-z0-9\n' _)" "${c%@*}" "$([[ $c == *@* ]] && echo "${c##*@}" || echo 0)"; done ;;
esac
