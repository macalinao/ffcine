#!/usr/bin/env bash
# bench/mlt/quality.sh: SSIM/PSNR of each MLT render vs the render.py CPU render (as ../quality.sh
# does for the ffmpeg candidates) and a side-by-side PNG per clip (render.py | melt | Movit).
# Writes $BENCH_TMP/quality-mlt.jsonl and $BENCH_TMP/sbs/<clip>_mlt.png.
set -euo pipefail
cd "$(dirname "$0")"
source ../env.sh
O=$BENCH_TMP/out
mkdir -p "$BENCH_TMP/sbs"
: > "$BENCH_TMP/quality-mlt.jsonl"
# Pair frames by index: ssim/psnr sync on timestamps, and melt's mkv timestamps round differently
# from the references' (settb=1001/24000,setpts=N), which pairs neighbouring frames every few frames.
N="settb=1001/24000,setpts=N"

cmp() {  # cmp <clip> <candidate> <reference>
  local clip=$1 a=$O/${1}_$2.mkv b=$O/${1}_$3.mkv
  [[ -f $a && -f $b ]] || { echo "skip $clip $2" >&2; return; }
  local log
  log=$(nice -n 19 "$FF" -hide_banner -nostdin -i "$a" -i "$b" -filter_complex \
    "[0:v]$N,format=yuv420p10le,split[a0][a1];[1:v]$N,format=yuv420p10le,split[b0][b1];[a0][b0]ssim;[a1][b1]psnr" \
    -f null - 2>&1 | grep -E "Parsed_(ssim|psnr)")
  python3 - "$clip" "$2" "$3" "$log" >> "$BENCH_TMP/quality-mlt.jsonl" <<'EOF'
import json, re, sys
clip, cand, ref, log = sys.argv[1:]
s = re.search(r"SSIM Y:([\d.]+) .*?U:([\d.]+) .*?V:([\d.]+) .*?All:([\d.]+)", log)
p = re.search(r"PSNR y:([\d.inf]+) u:([\d.inf]+) v:([\d.inf]+) average:([\d.inf]+)", log)
print(json.dumps(dict(clip=clip, candidate=cand, reference=ref,
                      ssim_y=float(s[1]), ssim_all=float(s[4]),
                      psnr_y=float(p[1]), psnr_avg=float(p[4]))))
EOF
  tail -1 "$BENCH_TMP/quality-mlt.jsonl"
}

sbs() {  # sbs <clip> <frame>: render.py CPU | melt CPU | melt + Movit, each 1280x720
  local clip=$1 n=$2 f=""
  for i in 0 1 2; do f+="[$i:v]select=eq(n\,$n),scale=1280:720:flags=lanczos,format=rgb24[v$i];"; done
  nice -n 19 "$FF" -v error -nostdin -y -i "$O/${clip}_proto_cpu.mkv" -i "$O/${clip}_mlt_cpu.mkv" \
    -i "$O/${clip}_mlt_movit.mkv" -filter_complex "${f}[v0][v1][v2]hstack=3" \
    -frames:v 1 "$BENCH_TMP/sbs/${clip}_mlt.png"
  echo "$BENCH_TMP/sbs/${clip}_mlt.png"
}

for c in payday nightpath r10; do
  for cand in mlt_cpu mlt_grain mlt_nvdec mlt_movit; do cmp "$c" "$cand" proto_cpu; done
done

for c in payday nightpath r10; do
  n=$(python3 -c 'import json,sys; print({x["id"]:x for x in json.load(open(sys.argv[1]))}[sys.argv[2]]["n"] // 2)' "$BENCH_TMP/proto.json" "$c")
  sbs "$c" "$n"
done
