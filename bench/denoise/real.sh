#!/usr/bin/env bash
# Real low-light clips (docs/denoise.md): ffcine without/with denoise, and nlmeans_vulkan with the
# prototype's settings, then noise on a dark flat patch. Inside `nix develop`, from the repo root.
#   bench/denoise/real.sh NAME CLIP SS X Y
set -euo pipefail
W=${W:-/tmp/ffcine-denoise/real}
FF=${FF:-./target/release/ffcine}
NLFF=${NLFF:-ffmpeg}

N=${N:-48}
name=$1 clip=$2 ss=$3 x=$4 y=$5
mkdir -p "$W"
S="bench/denoise/sigma.py"
g() { local o=$1; shift; nice -n 19 "$FF" grade --no-cache --rate qp=4 --ss "$ss" --frames "$N" "$@" "$clip" "$W/$name-$o.mkv" 2>&1 | grep -E 'denoise:|frames in' | sed "s/^/  $o: /"; }
[ -n "${NLONLY:-}" ] || {
g base
g t2 --temporal-radius 2 --temporal-strength 3
g t2s --temporal-radius 2 --temporal-strength 3 --denoise 1.5 --denoise-chroma 2.5
g t3s --temporal-radius 3 --temporal-strength 3 --denoise 1.5 --denoise-chroma 2.5
g s --denoise 2 --denoise-chroma 3 --denoise-radius 3
}
t0=$(date +%s.%N)
nice -n 19 "$NLFF" -loglevel error -y -init_hw_device vulkan=vk -filter_hw_device vk \
  -ss "$ss" -i "$clip" -frames:v "$N" -vf "hwupload,nlmeans_vulkan=s=1.0:p=7:r=7:t=1,hwdownload,format=yuv420p10le" \
  -c:v ffv1 "$W/$name-nl.mkv"
t1=$(date +%s.%N)
echo "  nl: nlmeans_vulkan s=1 p=7 r=7 t=1, $(echo "$N / ($t1 - $t0)" | bc -l | cut -c1-5) fps incl. FFV1 encode"
for o in base t2 t2s t3s s nl; do
  echo "  $o: sigma/temporal bin1 $($S "$W/$name-$o.mkv" "$x" "$y" 512 "$N" 0 1)  bin4 $($S "$W/$name-$o.mkv" "$x" "$y" 512 "$N" 0 4)"
done
