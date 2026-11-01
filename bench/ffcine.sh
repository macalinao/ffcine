#!/usr/bin/env bash
# ffcine.sh [label ...]: `ffcine grade` on payday and r10 at the bench's compat settings, each run
# wrapped by run.sh (same measurements as bench.sh), then SSIM/PSNR vs render.py's CPU render.
# Needs bench.sh's proto.json and reference renders. Run from the ffcine dev shell after
# `cargo build --release`:  nix develop -c bench/ffcine.sh
#
#   <clip>_ffcine_cold   empty shader cache (first run on a machine)
#   <clip>_ffcine        warm cache: Vulkan decode -> libplacebo -> NVENC (default)
#   <clip>_ffcine_cuda   NVDEC + one CUDA->Vulkan copy per frame (--decode cuda)
#   <clip>_ffcine_vkenc  hevc_vulkan, no copies at all
#   batch_ffcine         both shots through one process (`ffcine batch`)
# Results: $BENCH_TMP/results.jsonl, $BENCH_TMP/quality-ffcine.jsonl.
set -euo pipefail
cd "$(dirname "$0")"
source ./env.sh
FFCINE=${FFCINE:-$(cd .. && pwd)/target/release/ffcine}
O=$BENCH_TMP/out
CACHE=$BENCH_TMP/ffcine-cache
mkdir -p "$O"

args() {  # args <clip>: grade flags matching proto.json's GPU command for the clip
  python3 - "$BENCH_TMP/proto.json" "$1" <<'PY'
import json, sys
s = {x["id"]: x for x in json.load(open(sys.argv[1]))}[sys.argv[2]]
inc = "bt709/bt709/bt709/tv" if s["kind"] == "dji" else "-/bt709/bt709/-"
print(f'--ss {s["ss"]} --frames {s["n"]} --fps 24000/1001 --in-color {inc} '
      f'--size {s["W"]}x{s["H"]} --lut {s["lut"]} {s["src"]}')
PY
}

ONLY="${*:-}"
go() {  # go <label> <ffcine args...>
  local label=$1; shift
  if [[ -n $ONLY && " $ONLY " != *" $label "* ]]; then return; fi
  ./run.sh "$label" -- "$FFCINE" "$@" || echo "FAILED: $label" >&2
  grep -h "frames in" "$BENCH_TMP/runs/$label/stderr.txt" || true
}

for c in payday r10; do
  read -ra a <<< "$(args "$c")"
  rm -rf "$CACHE"
  go "${c}_ffcine_cold" grade --cache-dir "$CACHE" "${a[@]}" "$O/${c}_ffcine_cold.mkv"
  go "${c}_ffcine" grade --cache-dir "$CACHE" "${a[@]}" "$O/${c}_ffcine.mkv"
  go "${c}_ffcine_cuda" grade --decode cuda --cache-dir "$CACHE" "${a[@]}" "$O/${c}_ffcine_cuda.mkv"
  go "${c}_ffcine_vkenc" grade --codec hevc_vulkan --cache-dir "$CACHE" "${a[@]}" "$O/${c}_ffcine_vkenc.mkv"
done

shots=$BENCH_TMP/ffcine-shots.txt
for c in payday r10; do echo "$(args "$c") $O/${c}_ffcine_batch.mkv"; done > "$shots"
go batch_ffcine batch --cache-dir "$CACHE" "$shots"

: > "$BENCH_TMP/quality-ffcine.jsonl"
for c in payday r10; do
  for cand in ffcine ffcine_cuda ffcine_vkenc ffcine_batch; do
    a=$O/${c}_$cand.mkv b=$O/${c}_proto_cpu.mkv
    [[ -f $a && -f $b ]] || continue
    log=$(nice -n 19 "$FF" -hide_banner -nostdin -i "$a" -i "$b" -filter_complex \
      "[0:v]format=yuv420p10le,split[a0][a1];[1:v]format=yuv420p10le,split[b0][b1];[a0][b0]ssim;[a1][b1]psnr" \
      -f null - 2>&1 | grep -E "Parsed_(ssim|psnr)")
    python3 - "$c" "$cand" "$log" >> "$BENCH_TMP/quality-ffcine.jsonl" <<'PY'
import json, re, sys
clip, cand, log = sys.argv[1:]
s = re.search(r"SSIM Y:([\d.]+) .*?All:([\d.]+)", log)
p = re.search(r"PSNR y:([\d.inf]+) .*?average:([\d.inf]+)", log)
print(json.dumps(dict(clip=clip, candidate=cand, reference="proto_cpu", ssim_y=float(s[1]),
                      ssim_all=float(s[2]), psnr_y=float(p[1]), psnr_avg=float(p[2]))))
PY
    tail -1 "$BENCH_TMP/quality-ffcine.jsonl"
  done
done
