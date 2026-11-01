#!/usr/bin/env bash
# bench/mlt/bench.sh [label ...]: MLT (melt, and Movit via movit-egl) on the bench clips, one
# encode at a time through ../run.sh (nice 19, load, GNU time, nvidia-smi dmon/pmon).
#
# Needs $BENCH_TMP/proto.json and the render.py references from ../bench.sh. Builds movit-egl
# (headless EGL Movit driver) into $MLT_BIN on first use. Results: $BENCH_TMP/results.jsonl
# (frames/fps patched in, since melt has no -frames:v), then run quality.sh.
# Variants: gen.py. MLT_FRAMES=1 SUFFIX=_1f for the start-up runs.
set -euo pipefail
cd "$(dirname "$0")"
source ../env.sh
export MLT_PKG=${MLT_PKG:-$(nix build --no-link --print-out-paths nixpkgs#mlt)}
export MELT=$MLT_PKG/bin/melt
export MLT_BIN=${MLT_BIN:-$BENCH_TMP/mlt-bin}
# NVIDIA's EGL only, so the EGL device is the 5060 Ti and not the iGPU through Mesa
export __EGL_VENDOR_LIBRARY_FILENAMES=/run/opengl-driver/share/glvnd/egl_vendor.d/10_nvidia.json
export LD_LIBRARY_PATH=/run/opengl-driver/lib
O=$BENCH_TMP/out

if [[ ! -x $MLT_BIN/movit-egl ]]; then
  mkdir -p "$MLT_BIN"
  dev=$(nix build --no-link --print-out-paths nixpkgs#mlt.dev)
  gl=$(nix build --no-link --print-out-paths nixpkgs#libglvnd.dev nixpkgs#libglvnd | tr '\n' ' ')
  read -r gldev glout <<< "$gl"
  nice -n 19 nix shell nixpkgs#gcc -c gcc -O2 -Wall -o "$MLT_BIN/movit-egl" movit-egl.c \
    -I"$dev/include/mlt-7" -I"$dev/include" -I"$gldev/include" -L"$MLT_PKG/lib" -L"$glout/lib" \
    -lmlt-7 -lEGL -Wl,-rpath,"$MLT_PKG/lib:$glout/lib"
fi

go() {  # go <clip> <variant>
  local label=${1}_mlt_$2${SUFFIX:-}
  if [[ -n ${ONLY:-} && " $ONLY " != *" $label "* ]]; then return; fi
  local j frames
  j=$(python3 gen.py "$BENCH_TMP/proto.json" "$1" "$2" "$O/$label.mkv")
  frames=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["frames"])' "$j")
  mapfile -t argv < <(python3 -c 'import json,sys; print("\n".join(json.loads(sys.argv[1])["argv"]))' "$j")
  ../run.sh "$label" -- "${argv[@]}" > /dev/null || echo "FAILED: $label" >&2
  # run.sh takes frames from -frames:v; patch the line it appended
  python3 - "$BENCH_TMP/results.jsonl" "$frames" <<'EOF'
import json, sys
path, n = sys.argv[1], int(sys.argv[2])
lines = open(path).read().splitlines()
r = json.loads(lines[-1])
r["frames"], r["fps"] = n, round(n / r["wall_s"], 2) if r["wall_s"] else None
lines[-1] = json.dumps(r)
open(path, "w").write("\n".join(lines) + "\n")
print(lines[-1])
EOF
}

warm() {  # source into the page cache first: a cold read from spinning disk made
 the first run 2-13x slower
  local src
  src=$(python3 -c 'import json,sys; print({x["id"]:x for x in json.load(open(sys.argv[1]))}[sys.argv[2]]["src"])' "$BENCH_TMP/proto.json" "$1")
  nice -n 19 cat "$src" > /dev/null
}

ONLY="${*:-}"
# MLT_FRAMES=1 SUFFIX=_1f bench.sh: start-up runs, as ../startup.sh
for c in payday nightpath r10; do
  warm "$c"
  for v in plain cpu grain nvdec movit; do go "$c" "$v"; done
done
