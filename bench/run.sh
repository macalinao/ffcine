#!/usr/bin/env bash
# run.sh <label> -- <command...>
#
# Runs one encode at nice 19 and records, under $BENCH_TMP/runs/<label>/:
#   load_before/after   /proc/loadavg
#   apps_before.csv     nvidia-smi compute apps (pid, name, VRAM) before the run
#   idle_dmon.txt       3 s of whole-GPU dmon before the run (what everyone else is doing)
#   dmon.txt            whole-GPU dmon (-s um, 1 s) during the run
#   pmon.txt            per-process GPU use (-s u, 1 s) during the run, to split ours from others
#   time.txt            GNU time -v (wall, user, sys, max RSS)
#   cmd.txt, stderr.txt
# then appends one JSON line to $BENCH_TMP/results.jsonl (bench/summarize.py).
set -euo pipefail
source "$(dirname "$0")/env.sh"
label=$1; shift
[[ ${1:-} == -- ]] && shift
d=$BENCH_TMP/runs/$label
rm -rf "$d"; mkdir -p "$d"
printf '%q ' "$@" > "$d/cmd.txt"

cat /proc/loadavg > "$d/load_before"
nvidia-smi --query-compute-apps=pid,process_name,used_memory --format=csv,noheader > "$d/apps_before.csv" || true
nvidia-smi dmon -s um -d 1 -c 3 > "$d/idle_dmon.txt" 2>/dev/null || true

nvidia-smi dmon -s um -d 1 > "$d/dmon.txt" 2>/dev/null & dm=$!
nvidia-smi pmon -s u -d 1 > "$d/pmon.txt" 2>/dev/null & pm=$!
sleep 1
rc=0
nice -n 19 "$GNUTIME" -v -o "$d/time.txt" "$@" 2> "$d/stderr.txt" & tp=$!
sleep 0.3
pgrep -P $tp > "$d/pid" || true
wait $tp || rc=$?
sleep 1
kill $dm $pm 2>/dev/null || true
wait $dm $pm 2>/dev/null || true
cat /proc/loadavg > "$d/load_after"
echo $rc > "$d/rc"
python3 "$BENCH_DIR/summarize.py" "$d" >> "$BENCH_TMP/results.jsonl"
tail -1 "$BENCH_TMP/results.jsonl"
exit $rc
