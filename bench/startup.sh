#!/usr/bin/env bash
# startup.sh: the same commands as bench.sh with -frames:v 1, to split wall time into a fixed
# start-up cost (process, CUDA/Vulkan device init, shader compile, LUT upload, seek) and a
# per-frame rate: steady fps ~= (n - 1) / (wall_n - wall_1). Labels get a _1f suffix.
set -euo pipefail
cd "$(dirname "$0")"
source ./env.sh

go() {  # go <label> <argv json>
  mapfile -t argv < <(python3 -c '
import json, sys
a = json.loads(sys.argv[1]); a[0] = sys.argv[2]
a[a.index("-frames:v") + 1] = "1"
a[-1] = a[-1].replace(".mkv", "_1f.mkv")
print("\n".join(a))' "$2" "$FF")
  ./run.sh "$1_1f" -- "${argv[@]}" || echo "FAILED: $1_1f" >&2
}
proto() { python3 -c 'import json,sys; s={x["id"]:x for x in json.load(open(sys.argv[1]))}[sys.argv[2]]; print(json.dumps(s[sys.argv[3]]))' "$BENCH_TMP/proto.json" "$1" "$2"; }
onepass() { python3 onepass.py "$BENCH_TMP/proto.json" "$1" "$2"; }

for c in payday nightpath r10; do
  go "${c}_proto_cpu" "$(proto "$c" cpu_cmd)"
  go "${c}_proto_gpu" "$(proto "$c" gpu_cmd)"
  go "${c}_onepass_full" "$(onepass "$c" full)"
done
