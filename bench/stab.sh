#!/usr/bin/env bash
# GPU stabilizer on `verdict` vs render.py's vidstab (docs/stabilize.md).
#
# 1. `ffcine stabilize` detect, cold (empty cache) and warm, writing the warp .hook.
# 2. The bench's one-pass graph (runs/verdict_onepass_full) twice: as is, and with the warp as its
#    own libplacebo pass in front (MAIN only sees libplacebo's crop, and the warp must come first).
# 3. `ffcine jitter` on the unstabilized, render.py vidstab, one-pass vidstab and ffcine renders.
# 4. render.py | ffcine side-by-side still (frame 100) and video.
#
# Run from the repo after `cargo build --release` in the dev shell; needs bench/run.sh's outputs
# in $BENCH_TMP (verdict_*.mkv, the grade LUT and onepass.hook).
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/env.sh"

OUT=${STAB_TMP:-/tmp/ffcine-stab}
FFCINE=${FFCINE:-$PWD/target/release/ffcine}
SRC=${SRC:?set SRC to a handheld DJI D-Log clip}

START=2.6
FRAMES=199
SMOOTHING=22
PUNCH=153 # render.py's 1.2x punch-in starts here: a deliberate jump, not jitter
LUT=$BENCH_TMP/proto-cache/gpu_luts/grade_d200d83e8d3a468a.cube
HOOKS=$BENCH_TMP/shaders/onepass.hook
mkdir -p "$OUT"
export XDG_CACHE_HOME=$OUT/cache

timed() { # label cmd...: wall seconds and CPU % into $OUT/<label>.time
  local label=$1
  shift
  "$GNUTIME" -f '%e s, %P CPU' -o "$OUT/$label.time" nice -n 19 "$@"
  echo "$label: $(cat "$OUT/$label.time")"
}

# 1. detect
rm -rf "$XDG_CACHE_HOME"
for run in cold warm; do
  timed "detect_$run" "$FFCINE" stabilize --start $START --frames $FRAMES --smoothing $SMOOTHING \
    --hook "$OUT/verdict_warp.hook" --transforms "$OUT/verdict_transforms.txt" \
    --motions "$OUT/verdict_motions.txt" "$SRC" | tee "$OUT/detect_$run.txt"
done

# 2. render. The graph is runs/verdict_onepass_full/cmd.txt's; `warp` is the extra pass.
q() { printf "if(gte(n,$PUNCH),%s,%s)" "$1" "$2"; }
z=$(q 1.2 1.0)
crop="crop_w='(trunc(iw/($z)/2)*2)':crop_h='(trunc(ih/($z)/2)*2)'"
crop+=":crop_x='(0+(trunc(round((iw-iw/($z))*($(q 0.5 0.5)))/2)*2))'"
crop+=":crop_y='(0+(trunc(round((ih-ih/($z))*($(q 0.4 0.5)))/2)*2))'"
tags=colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv
grade="libplacebo=w=3840:h=2160:$crop:upscaler=ewa_lanczossharp:downscaler=ewa_lanczos"
grade+=":lut='$LUT':lut_type=normalized:tonemapping=bt.2390:gamut_mode=perceptual:dithering=blue"
grade+=":deband=1:custom_shader_path='$HOOKS':$tags:format=p010le"
warp="libplacebo=custom_shader_path='$OUT/verdict_warp.hook':$tags:format=yuv444p16le,"
render() { # label warp-filter
  timed "render_$1" "$FF" -v error -nostdin -y -init_hw_device cuda=cu:0 \
    -init_hw_device vulkan=vk@cu -filter_hw_device vk -hwaccel cuda -hwaccel_device cu \
    -hwaccel_output_format cuda -ss $START -i "$SRC" -filter_complex \
    "[0:v]setpts=PTS-STARTPTS,setparams=$tags,hwupload=derive_device=vulkan,fps=24000/1001,$2$grade,hwupload=derive_device=cuda,tpad=stop_mode=clone:stop=-1,setsar=1,settb=1001/24000,setpts=N[v]" \
    -map '[v]' -frames:v $FRAMES -an -c:v hevc_nvenc -preset p6 -tune hq -rc constqp -qp 12 \
    -profile:v main10 -g 24 -color_primaries bt709 -color_trc bt709 -colorspace bt709 \
    -color_range tv "$OUT/verdict_$1.mkv"
}
render nostab ""
render ffcine_stab "$warp"

# 3. residual jitter (frame pair ending at the punch-in excluded)
for v in "$OUT/verdict_nostab.mkv" "$BENCH_TMP/out/verdict_proto_gpu.mkv" \
  "$BENCH_TMP/out/verdict_onepass_stab.mkv" "$OUT/verdict_ffcine_stab.mkv"; do
  printf '%s\t' "$(basename "$v")"
  nice -n 19 "$FFCINE" jitter --frames $FRAMES --exclude $PUNCH "$v"
done | tee "$OUT/jitter.txt"

# 4. side by side: render.py (vidstab) | ffcine
pair=(-i "$BENCH_TMP/out/verdict_proto_gpu.mkv" -i "$OUT/verdict_ffcine_stab.mkv")
nice -n 19 "$FF" -v error -nostdin -y "${pair[@]}" -filter_complex \
  "[0:v]select='eq(n,100)',scale=960:540[a];[1:v]select='eq(n,100)',scale=960:540[b];[a][b]hstack,format=rgb24" \
  -frames:v 1 -update 1 "$OUT/verdict_sbs.png"
nice -n 19 "$FF" -v error -nostdin -y "${pair[@]}" -filter_complex \
  "[0:v]scale=960:540[a];[1:v]scale=960:540[b];[a][b]hstack,format=yuv420p" \
  -c:v libx264 -preset medium -crf 20 -movflags +faststart "$OUT/verdict_sbs.mp4"
echo "wrote $OUT/verdict_sbs.png $OUT/verdict_sbs.mp4"
