#!/usr/bin/env bash
# Denoise benchmark (docs/denoise.md). Run inside `nix develop`, from the repo root, after
# `cargo build --release`. Synthetic set: a clean 1080p daylight clip plus known noise, every
# output graded by the same ffcine pass and compared to the graded clean clip.
#   bench/denoise/run.sh setup            # clean.mkv, noisy.mkv (heavy), noisy_mid.mkv
#   bench/denoise/run.sh ffcine NAME NOISY [ffcine grade flags...]
#   bench/denoise/run.sh nlmeans NAME NOISY S [P R]
#   bench/denoise/run.sh score NAME       # PSNR/SSIM vs ref
set -euo pipefail
W=${W:-/tmp/ffcine-denoise/syn}
FF=${FF:-./target/release/ffcine}
SRC=${SRC:?set SRC to a clean daylight DJI D-Log clip}

N=${N:-48}
# An ffmpeg with nlmeans_vulkan (see bench/env.sh).
NLFF=${NLFF:-ffmpeg}

mkdir -p "$W"
grade() { nice -n 19 "$FF" grade --no-cache --rate qp=4 --frames "$N" "$@"; }
case $1 in
setup)
  nice -n 19 ffmpeg -loglevel error -y -ss 0.25 -i "$SRC" -frames:v "$N" \
    -vf "scale=1920:1080:flags=area,format=yuv420p10le" -c:v ffv1 "$W/clean.mkv"
  for x in "noisy 24 16" "noisy_mid 12 8"; do
    set -- $x
    nice -n 19 ffmpeg -loglevel error -y -i "$W/clean.mkv" \
      -vf "format=yuv420p16le,noise=c0s=$2:c1s=$3:c2s=$3:allf=t,format=yuv420p10le" \
      -c:v ffv1 "$W/$1.mkv"
  done
  grade "$W/clean.mkv" "$W/ref.mkv" ;;
ffcine)
  name=$2 in=$3; shift 3
  grade "$@" "$W/$in.mkv" "$W/$name.mkv" 2>&1 | grep -E 'denoise:|frames in' ;;
nlmeans)
  name=$2 in=$3 s=$4 p=${5:-7} r=${6:-15}
  t0=$(date +%s.%N)
  nice -n 19 "${NLFF:-ffmpeg}" -loglevel error -y -init_hw_device vulkan=vk -filter_hw_device vk \
    -i "$W/$in.mkv" -vf "hwupload,nlmeans_vulkan=s=$s:p=$p:r=$r,hwdownload,format=yuv420p10le" \
    -c:v ffv1 "$W/$name.ffv1.mkv"
  t1=$(date +%s.%N)
  echo "nlmeans_vulkan s=$s p=$p r=$r: $(echo "$N / ($t1 - $t0)" | bc -l | cut -c1-5) fps (incl. FFV1 decode/encode)"
  grade "$W/$name.ffv1.mkv" "$W/$name.mkv" 2>&1 | grep -E 'frames in' ;;
score)
  ffmpeg -hide_banner -i "$W/$2.mkv" -i "$W/ref.mkv" \
    -lavfi "[0]format=yuv420p10le[a];[1]format=yuv420p10le[b];[a][b]psnr;[0]format=yuv420p10le[c];[1]format=yuv420p10le[d];[c][d]ssim" \
    -f null - 2>&1 | grep -oE '(PSNR y:[0-9.]+ u:[0-9.]+ v:[0-9.]+ average:[0-9.]+)|(SSIM Y:[0-9.]+ .*All:[0-9.]+)' | tr '\n' ' '
  echo ;;
esac
