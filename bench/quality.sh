#!/usr/bin/env bash
# quality.sh: SSIM/PSNR of each candidate vs the render.py CPU render (the current output), and a
# side-by-side PNG (render.py | ffmpeg one-pass) per clip. Same frames (both start at the shot's
# `in`, same -frames:v, same 23.976 timebase), same 3840x2160 yuv420p10le.
# Writes $BENCH_TMP/quality.jsonl and $BENCH_TMP/sbs/<clip>.png.
set -euo pipefail
cd "$(dirname "$0")"
source ./env.sh
O=$BENCH_TMP/out
mkdir -p "$BENCH_TMP/sbs"
: > "$BENCH_TMP/quality.jsonl"

cmp() {  # cmp <clip> <candidate> <reference>
  local clip=$1 a=$O/${1}_$2.mkv b=$O/${1}_$3.mkv
  [[ -f $a && -f $b ]] || { echo "skip $clip $2" >&2; return; }
  local log
  log=$(nice -n 19 "$FF" -hide_banner -nostdin -i "$a" -i "$b" -filter_complex \
    "[0:v]format=yuv420p10le,split[a0][a1];[1:v]format=yuv420p10le,split[b0][b1];[a0][b0]ssim;[a1][b1]psnr" \
    -f null - 2>&1 | grep -E "Parsed_(ssim|psnr)")
  python3 - "$clip" "$2" "$3" "$log" >> "$BENCH_TMP/quality.jsonl" <<'EOF'
import json, re, sys
clip, cand, ref, log = sys.argv[1:]
s = re.search(r"SSIM Y:([\d.]+) .*?U:([\d.]+) .*?V:([\d.]+) .*?All:([\d.]+)", log)
p = re.search(r"PSNR y:([\d.inf]+) u:([\d.inf]+) v:([\d.inf]+) average:([\d.inf]+)", log)
print(json.dumps(dict(clip=clip, candidate=cand, reference=ref,
                      ssim_y=float(s[1]), ssim_all=float(s[4]),
                      psnr_y=float(p[1]), psnr_avg=float(p[4]))))
EOF
  tail -1 "$BENCH_TMP/quality.jsonl"
}

sbs() {  # sbs <clip> <frame>: render.py CPU | one-pass full, each 1920x1080
  local clip=$1 n=$2
  nice -n 19 "$FF" -v error -nostdin -y -i "$O/${clip}_proto_cpu.mkv" -i "$O/${clip}_onepass_full.mkv" -filter_complex \
    "[0:v]select=eq(n\,$n),scale=1920:1080:flags=lanczos,format=rgb24[a];[1:v]select=eq(n\,$n),scale=1920:1080:flags=lanczos,format=rgb24[b];[a][b]hstack" \
    -frames:v 1 "$BENCH_TMP/sbs/$clip.png"
  echo "$BENCH_TMP/sbs/$clip.png"
}

for c in payday nightpath r10 verdict; do
  for cand in onepass_full onepass_compat proto_gpu; do cmp "$c" "$cand" proto_cpu; done
done
cmp verdict onepass_stab proto_cpu

for c in payday nightpath r10; do
  n=$(python3 -c 'import json,sys; print({x["id"]:x for x in json.load(open(sys.argv[1]))}[sys.argv[2]]["n"] // 2)' "$BENCH_TMP/proto.json" "$c")
  sbs "$c" "$n"
done
