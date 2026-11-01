#!/usr/bin/env python3
"""MLT runs (results.jsonl, labels <clip>_mlt_<variant>[_r2|_r3|_1f]) + quality-mlt.jsonl ->
markdown tables (stdout) and bench/results/mlt_runs.csv (timings and quality per row).

The row is the run with the (lower) median wall time, with its CPU/GPU/load.
Steady fps = (n - 1) / (median wall_n - wall_1f), as ../report.py.
Runs that started at load1 > MAX_LOAD (default 80; for shared machines)
 are left out, and so are
results.jsonl lines before MLT_SINCE (1-based line number; older runs used a different XML).
"""
import csv
import json
import os
import re

B = os.environ.get("BENCH_TMP", "/tmp/ffcine-bench")
HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "results")
CLIPS = ["payday", "nightpath", "r10"]
MLT_SINCE = int(os.environ.get("MLT_SINCE", 1))
MAX_LOAD = float(os.environ.get("MAX_LOAD", 80))
VARIANTS = ["plain", "cpu", "grain", "nvdec", "movit"]
NAMES = {"plain": "melt, no filters", "cpu": "melt (CPU: hqdn3d + lut3d)", "grain": "melt + grain",
         "nvdec": "melt, NVDEC decode", "movit": "melt + Movit (EGL), NVDEC"}

runs = {}
for i, ln in enumerate(open(os.path.join(B, "results.jsonl")), 1):
    r = json.loads(ln)
    if i < MLT_SINCE and "_mlt_" in r["label"]:
        continue
    if r["rc"] == 0 and float(r["load_before"][0]) <= MAX_LOAD:
        runs[r["label"]] = r
qual = {}
qp = os.path.join(B, "quality-mlt.jsonl")
if os.path.exists(qp):
    for ln in open(qp):
        q = json.loads(ln)
        qual[(q["clip"], q["candidate"])] = q

rows = []
for c in CLIPS:
    ref = runs.get(f"{c}_proto_cpu")
    for v in VARIANTS:
        base = f"{c}_mlt_{v}"
        reps = [runs[k] for k in (base, base + "_r2", base + "_r3") if k in runs]
        if not reps:
            continue
        reps.sort(key=lambda r: r["wall_s"])
        m = reps[(len(reps) - 1) // 2]  # lower median: with 2 runs, the faster
        one = runs.get(base + "_1f")
        steady = (round((m["frames"] - 1) / (m["wall_s"] - one["wall_s"]), 1)
                  if one and m["wall_s"] > one["wall_s"] else None)
        q = qual.get((c, f"mlt_{v}"), {})
        rows.append(dict(
            clip=c, variant=v, reps=len(reps), frames=m["frames"], wall_s=m["wall_s"],
            wall_min=reps[0]["wall_s"], wall_max=reps[-1]["wall_s"], fps=m["fps"], steady_fps=steady,
            startup_s=one["wall_s"] if one else None, cpu_pct=m["cpu_pct"],
            vs_renderpy=round(ref["wall_s"] / m["wall_s"], 2) if ref else None,
            gpu_sm=m["gpu_ours"]["sm"], gpu_enc=m["gpu_ours"]["enc"], gpu_dec=m["gpu_ours"]["dec"],
            load1_before=m["load_before"][0], load1_after=m["load_after"][0],
            ssim_y=q.get("ssim_y"), ssim_all=q.get("ssim_all"), psnr_y=q.get("psnr_y"),
            psnr_avg=q.get("psnr_avg")))

os.makedirs(OUT, exist_ok=True)
with open(os.path.join(OUT, "mlt_runs.csv"), "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=list(rows[0]))
    w.writeheader()
    w.writerows(rows)


def f(x, fmt="{}"):
    return "" if x is None else fmt.format(x)


print("| clip | pipeline | runs | frames | wall s (min-max) | fps | steady fps | start-up s | speedup vs "
      "render.py | CPU % | GPU SM/NVENC/NVDEC % (ours) | load1 before/after |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|")
for r in rows:
    print(f"| {r['clip']} | {NAMES[r['variant']]} | {r['reps']} | {r['frames']} | {r['wall_s']:.2f} "
          f"({r['wall_min']:.1f}-{r['wall_max']:.1f}) | {r['fps']:.1f} | {f(r['steady_fps'])} | "
          f"{f(r['startup_s'], '{:.1f}')} | {f(r['vs_renderpy'], '{:.2f}x')} | {r['cpu_pct']} | "
          f"{r['gpu_sm']:.0f}/{r['gpu_enc']:.0f}/{r['gpu_dec']:.0f} | "
          f"{float(r['load1_before']):.0f}/{float(r['load1_after']):.0f} |")
print()
print("| clip | candidate | SSIM Y | SSIM all | PSNR Y dB | PSNR avg dB |")
print("|---|---|---|---|---|---|")
for r in rows:
    if r["ssim_y"] is not None:
        print(f"| {r['clip']} | {NAMES[r['variant']]} | {r['ssim_y']:.4f} | {r['ssim_all']:.4f} | "
              f"{r['psnr_y']:.2f} | {r['psnr_avg']:.2f} |")
