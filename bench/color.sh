#!/usr/bin/env bash
# Colour-in acceptance checks (docs/color.md): camera LUTs, HDR tone mapping, 4:2:2 decode, and the
# prototype-match regression, each ffcine render against an ffmpeg CPU/libplacebo reference that
# runs render.py's own colour-in chain. SSIM/PSNR on yuv420p10le.
#   bench/color.sh            # all
#   bench/color.sh dlog hlg   # some
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/env.sh"

OUT=${COLOR_TMP:-/tmp/ffcine-color}
FFCINE=${FFCINE:-$PWD/target/release/ffcine}
# Sample clips: set these to your own footage.
DJI=${DJI:?set DJI to a DJI Osmo Pocket 4 D-Log clip}
LUTS=$HOME/.local/share/ffcine/luts # `ffcine fetch-luts`
DJI_LUT="$LUTS/DJI OSMO Pocket 4 D-Log to Rec.709 V2.0.cube"
DJI_LUT1=${DJI_LUT1:?set DJI_LUT1 to DJI's V1.0 D-Log to Rec.709 LUT}
HLG=${HLG:?set HLG to an iPhone HLG clip}
ALOG=${ALOG:?set ALOG to an iPhone Apple Log clip}
ALOG_LUT=${ALOG_LUT:-$PROJ/luts/applelog_to_rec709.cube}
R10=${R10:?set R10 to a Canon R10 clip}
R422=$OUT/r422.mp4
mkdir -p "$OUT"
export FFCINE_LUT_DIR=$LUTS

ENC=(-c:v hevc_nvenc -preset p6 -tune hq -rc constqp -qp 12 -profile:v main10 -pix_fmt p010le
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv)

ff() { nice -n 19 "$FF" -v error -nostdin -y "$@"; }
cine() { # label args...: ffcine grade, wall time to $OUT/<label>.log
  local label=$1
  shift
  { "$GNUTIME" -f '%e s wall' nice -n 19 "$FFCINE" grade "$@"; } >"$OUT/$label.log" 2>&1 ||
    { cat "$OUT/$label.log"; return 1; }
  echo "$label: $(grep -h 'frames in\|profile:\|wall' "$OUT/$label.log" | tr '\n' ' ')"
}
cmp() { # label a b [frames]: SSIM / PSNR
  local f=${4:-100000} s p
  local g="[0:v]trim=end_frame=$f,setpts=N,format=yuv420p10le[a];[1:v]trim=end_frame=$f,setpts=N,format=yuv420p10le[b];[a][b]"
  s=$(nice -n 19 "$FF" -nostdin -noautorotate -i "$2" -noautorotate -i "$3" -filter_complex "${g}ssim" -f null - 2>&1 | grep -o 'SSIM Y:[0-9.]*.*All:[0-9.]*' | sed 's/ ([0-9.inf]*)//g')
  p=$(nice -n 19 "$FF" -nostdin -noautorotate -i "$2" -noautorotate -i "$3" -filter_complex "${g}psnr" -f null - 2>&1 | grep -o 'PSNR y:[0-9.inf]*.*average:[0-9.inf]*')
  echo "$1	$s	$p" | tee -a "$OUT/quality.tsv"
}

dlog() {
  # Camera LUT alone: ffcine auto profile (Pocket 4 D-Log, DJI's V2.0 LUT) vs ffmpeg lut3d
  # (tetrahedral) with the same LUT.
  cine dlogm_auto --no-deband --ss 0.25 --frames 70 --fps 24000/1001 "$DJI" "$OUT/dlogm_auto.mkv"
  ff -ss 0.25 -i "$DJI" -vf "fps=24000/1001,format=gbrp16le,lut3d=file='$DJI_LUT':interp=tetrahedral" \
    -frames:v 70 "${ENC[@]}" "$OUT/dlogm_ref.mkv"
  cmp dlog_lut "$OUT/dlogm_ref.mkv" "$OUT/dlogm_auto.mkv"
  # DJI's V1.0 LUT (what render.py used) and the analytic D-Log / D-Gamut path against V2.0.
  ff -ss 0.25 -i "$DJI" -vf "fps=24000/1001,format=gbrp16le,lut3d=file='$DJI_LUT1':interp=tetrahedral" \
    -frames:v 70 "${ENC[@]}" "$OUT/dlog_v1.mkv"
  cmp dlog_v1_vs_v2 "$OUT/dlogm_ref.mkv" "$OUT/dlog_v1.mkv"
  FFCINE_LUT_DIR=/nonexistent XDG_DATA_HOME=/nonexistent cine dlog_analytic --no-deband \
    --camera dlog --ss 0.25 --frames 70 --fps 24000/1001 "$DJI" "$OUT/dlog_analytic.mkv"
  cmp dlog_analytic_vs_v2 "$OUT/dlogm_ref.mkv" "$OUT/dlog_analytic.mkv"
  # The grade LUT moves to the target after a camera LUT: an identity grade changes nothing.
  printf 'LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n' >"$OUT/identity.cube"
  cine dlogm_target --no-deband --camera dlog --lut "$OUT/identity.cube" --ss 0.25 --frames 70 --fps 24000/1001 \
    "$DJI" "$OUT/dlogm_target.mkv"
  cmp dlog_target_identity "$OUT/dlogm_auto.mkv" "$OUT/dlogm_target.mkv"
  # Prototype match (roadmap step 1): payday with render.py's baked grade vs render.py's CPU render.
  cine payday --ss 0.25 --frames 70 --fps 24000/1001 --in-color bt709/bt709/bt709/tv --size 3840x2160 \
    --lut "$BENCH_TMP/proto-cache/gpu_luts/grade_9f04d9a751102f0e.cube" "$DJI" "$OUT/payday.mkv"
  cmp payday_vs_renderpy "$BENCH_TMP/out/payday_proto_cpu.mkv" "$OUT/payday.mkv"
}

hlg() {
  # iPhone HLG + DV 8.4: auto (hdr) vs render.py's colour-in (vf_libplacebo bt.2390 + perceptual).
  cine hlg_auto --frames 120 "$HLG" "$OUT/hlg_auto.mkv"
  ff -noautorotate -init_hw_device vulkan=vk -filter_hw_device vk -i "$HLG" -vf \
    "hwupload,libplacebo=tonemapping=bt.2390:gamut_mode=perceptual:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=p010le:dithering=blue,hwdownload,format=p010le" \
    -frames:v 120 "${ENC[@]}" "$OUT/hlg_ref.mkv"
  cmp hlg_tonemap "$OUT/hlg_ref.mkv" "$OUT/hlg_auto.mkv"
}

applelog() {
  # Apple Log ProRes: analytic pl_custom_lut vs render.py's scale(bt2020) + lut3d(applelog cube).
  cine applelog_auto --frames 60 "$ALOG" "$OUT/applelog_auto.mkv"
  ff -noautorotate -i "$ALOG" -vf \
    "scale=in_color_matrix=bt2020:out_range=full,format=gbrp16le,lut3d=file='$ALOG_LUT':interp=tetrahedral" \
    -frames:v 60 "${ENC[@]}" "$OUT/applelog_ref.mkv"
  cmp applelog_lut "$OUT/applelog_ref.mkv" "$OUT/applelog_auto.mkv"
}

r10() {
  # The real R10 clip
 (H.264 8-bit 4:2:0, full range): prototype match as payday.
  cine r10 --ss 3.0 --frames 96 --fps 24000/1001 --in-color -/bt709/bt709/- --size 3840x2160 \
    --lut "$BENCH_TMP/proto-cache/gpu_luts/grade_d0e6aecb7bd510db.cube" "$R10" "$OUT/r10.mkv"
  cmp r10_vs_renderpy "$BENCH_TMP/out/r10_proto_cpu.mkv" "$OUT/r10.mkv"
  # 10-bit 4:2:2 HEVC as the R10 records it (synthetic: no such sample clip), both decoders.
  [[ -f $R422 ]] || ff -f lavfi -i testsrc2=size=3840x2160:rate=25 -t 4 -pix_fmt yuv422p10le \
    -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv \
    -c:v libx265 -x265-params log-level=error "$R422"
  ff -i "$R422" "${ENC[@]}" "$OUT/r422_ref.mkv"
  for d in vulkan cuda; do
    cine "r422_$d" --decode "$d" "$R422" "$OUT/r422_$d.mkv"
    cmp "r422_$d" "$OUT/r422_ref.mkv" "$OUT/r422_$d.mkv"
  done
}

[[ $# -gt 0 ]] || : >"$OUT/quality.tsv"
for t in "${@:-dlog hlg applelog r10}"; do
  for x in $t; do "$x"; done
done
