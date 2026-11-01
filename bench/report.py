#!/usr/bin/env python3
"""results.jsonl + quality.jsonl -> markdown tables (stdout) and bench/results/*.csv."""
import csv
import json
import os
import sys

B = os.environ.get("BENCH_TMP", "/tmp/ffcine-bench")
HERE = os.path.dirname(os.path.abspath(__file__))
runs = {}
for ln in open(os.path.join(B, "results.jsonl")):
    r = json.loads(ln)
    runs[r["label"]] = r            # last run of a label wins
qual = [json.loads(ln) for ln in open(os.path.join(B, "quality.jsonl"))] if os.path.exists(
    os.path.join(B, "quality.jsonl")) else []

os.makedirs(os.path.join(HERE, "results"), exist_ok=True)
cols = ["label", "rc", "frames", "wall_s", "fps", "cpu_pct", "user_s", "sys_s", "max_rss_mb",
        "gpu_ours_sm", "gpu_ours_enc", "gpu_ours_dec", "gpu_all_sm", "gpu_all_enc", "gpu_all_dec",
        "gpu_idle_sm", "gpu_idle_dec", "gpu_others", "load1_before", "load1_after", "steady_fps"]


def steady(r):
    one = runs.get(r["label"] + "_1f")
    if not one or not r.get("frames") or r["wall_s"] <= one["wall_s"]:
        return None
    return round((r["frames"] - 1) / (r["wall_s"] - one["wall_s"]), 1)


rows = []
for lab, r in runs.items():
    rows.append(dict(
        label=lab, rc=r["rc"], frames=r["frames"], wall_s=r["wall_s"], fps=r["fps"], cpu_pct=r["cpu_pct"],
        user_s=r["user_s"], sys_s=r["sys_s"], max_rss_mb=r["max_rss_mb"],
        gpu_ours_sm=r["gpu_ours"]["sm"], gpu_ours_enc=r["gpu_ours"]["enc"], gpu_ours_dec=r["gpu_ours"]["dec"],
        gpu_all_sm=r["gpu_during"]["sm"], gpu_all_enc=r["gpu_during"]["enc"], gpu_all_dec=r["gpu_during"]["dec"],
        gpu_idle_sm=r["gpu_idle"]["sm"], gpu_idle_dec=r["gpu_idle"]["dec"],
        gpu_others="; ".join(f"{k} sm{v['sm']}/enc{v['enc']}/dec{v['dec']}" for k, v in r["gpu_others_during"].items()),
        load1_before=r["load_before"][0], load1_after=r["load_after"][0], steady_fps=steady(r)))
with open(os.path.join(HERE, "results", "runs.csv"), "w", newline="") as f:
    w = csv.DictWriter(f, cols)
    w.writeheader()
    w.writerows(rows)
if qual:
    with open(os.path.join(HERE, "results", "quality.csv"), "w", newline="") as f:
        w = csv.DictWriter(f, list(qual[0].keys()))
        w.writeheader()
        w.writerows(qual)

R = {r["label"]: r for r in rows}
print("| run | frames | wall s | fps | steady fps | speedup vs render.py | CPU % | GPU SM/enc/dec % (ours) | load1 before/after |")
print("|---|---|---|---|---|---|---|---|---|")
for lab, r in R.items():
    if lab.endswith("_1f"):
        continue
    clip = lab.split("_")[0]
    base = R.get(f"{clip}_proto_cpu")
    sp = f"{base['wall_s'] / r['wall_s']:.2f}x" if base and r["wall_s"] else ""
    print(f"| {lab} | {r['frames']} | {r['wall_s']:.2f} | {r['fps']} | {r['steady_fps'] or ''} | {sp} | {r['cpu_pct']} "
          f"| {r['gpu_ours_sm']}/{r['gpu_ours_enc']}/{r['gpu_ours_dec']} | {r['load1_before']}/{r['load1_after']} |")
if qual:
    print()
    print("| clip | candidate vs render.py CPU | SSIM Y | SSIM all | PSNR Y dB | PSNR avg dB |")
    print("|---|---|---|---|---|---|")
    for q in qual:
        print(f"| {q['clip']} | {q['candidate']} | {q['ssim_y']:.4f} | {q['ssim_all']:.4f} | {q['psnr_y']:.2f} | {q['psnr_avg']:.2f} |")
print("\nfixed start-up (1-frame runs):", file=sys.stderr)
for lab, r in R.items():
    if lab.endswith("_1f"):
        print(f"  {lab}: {r['wall_s']} s", file=sys.stderr)
