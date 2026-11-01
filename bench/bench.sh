#!/usr/bin/env bash
# bench.sh [run-label ...]: render.py's chain vs a one-pass ffmpeg GPU graph, one encode at a time.
#
#   1. proto_cmd.py (render.py copy, cache in $BENCH_TMP) -> $BENCH_TMP/proto.json
#   2. per clip: render.py CPU chain (what edit.yaml renders today, output.gpu: false),
#      render.py --gpu chain, one-pass full, one-pass compat; each wrapped by run.sh
#   3. vidstab on `verdict`: render.py's detect pass, then the one-pass graph without and with
#      vidstabtransform spliced in, plus render.py's own stabilized chains
# Results: $BENCH_TMP/results.jsonl (one line per run). Then run quality.sh.
set -euo pipefail
cd "$(dirname "$0")"
source ./env.sh
mkdir -p "$BENCH_TMP/out" "$BENCH_TMP/shaders"
cat shaders/unsharp.hook shaders/grain.hook > "$BENCH_TMP/shaders/onepass.hook"

CLIPS=(payday nightpath r10)
R10_SRC=${R10_SRC:?set R10_SRC to a Canon R10 clip}
R10='{id: r10, src: '"$R10_SRC"', in: 3.0,
 out: 7.0, audio: music, snap: false, label: R10 test}'
if [[ ! -s $BENCH_TMP/proto.json ]]; then
  nice -n 19 nix develop "$PROTO_SHELL" -c python3 proto_cmd.py --edit "$PROJ/edit.yaml" \
    --shot payday --shot nightpath --shot verdict --shot r10 --extra-shot "$R10" > "$BENCH_TMP/proto.json"
fi

# argv JSON -> run.sh, with "ffmpeg" swapped for the pinned binary
go() {  # go <label> <argv json>
  local label=$1 json=$2
  if [[ -n ${ONLY:-} && " $ONLY " != *" $label "* ]]; then return; fi
  mapfile -t argv < <(python3 -c 'import json,sys; a=json.loads(sys.argv[1]); a[0]=sys.argv[2]; print("\n".join(a))' "$json" "$FF")
  ./run.sh "$label" -- "${argv[@]}" || echo "FAILED: $label" >&2
}
proto() { python3 -c 'import json,sys; s={x["id"]:x for x in json.load(open(sys.argv[1]))}[sys.argv[2]]; print(json.dumps(s[sys.argv[3]]))' "$BENCH_TMP/proto.json" "$1" "$2"; }
onepass() { python3 onepass.py "$BENCH_TMP/proto.json" "$1" "$2"; }

ONLY="${*:-}"
for c in "${CLIPS[@]}"; do
  go "${c}_proto_cpu" "$(proto "$c" cpu_cmd)"
  go "${c}_proto_gpu" "$(proto "$c" gpu_cmd)"
  go "${c}_onepass_full" "$(onepass "$c" full)"
  go "${c}_onepass_compat" "$(onepass "$c" compat)"
done

# vidstab (verdict): detect writes the .trf every stabilized render needs
go verdict_vidstabdetect "$(proto verdict detect_cmd)"
go verdict_onepass_full "$(onepass verdict full)"
go verdict_onepass_stab "$(onepass verdict stab)"
go verdict_proto_gpu "$(proto verdict gpu_cmd)"
go verdict_proto_cpu "$(proto verdict cpu_cmd)"
