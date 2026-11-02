#!/usr/bin/env bash
# Gyro vs optical flow on one clip (docs/stabilize.md#results): renders it unstabilized, with
# `--stab-source flow` and with `--stab-source gyro`, then `ffcine jitter` on each render.
#
#   bench/stab-gyro.sh CLIP [FRAMES]
#
# Run from the repo in the dev shell after `cargo build`. FFCINE overrides the binary, OUT the
# directory renders go to (default a fresh temp dir, removed at the end). Needs the GPU.
set -euo pipefail

CLIP=${1:?usage: bench/stab-gyro.sh CLIP [FRAMES]}
FRAMES=${2:-240}
FFCINE=${FFCINE:-$PWD/target/debug/ffcine}
if [[ -n ${OUT:-} ]]; then
  mkdir -p "$OUT"
else
  OUT=$(mktemp -d)
  trap 'rm -rf "$OUT"' EXIT
fi

RATE=$(ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate -of csv=p=0 "$CLIP" | head -1)
for src in none flow gyro; do
  args=()
  [[ $src != none ]] && args=(--stabilize --stab-source "$src" --stab-max-zoom 1.5)
  render="$OUT/$src.mkv"
  nice -n 19 "$FFCINE" grade --frames "$FRAMES" --fps "$RATE" --size 1920x1080 "${args[@]}" \
    "$CLIP" "$render" 2>"$OUT/$src.log"
  zoom=$(grep -o 'zoom [0-9.]* (needed [0-9.]*)' "$OUT/$src.log" | head -1 || true)
  jitter=$(nice -n 19 "$FFCINE" jitter --frames $((FRAMES - 1)) --rate "$RATE" "$render" | tail -1)
  printf '%-5s %-28s %s\n' "$src" "${zoom:-}" "$jitter"
done
