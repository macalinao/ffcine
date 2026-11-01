#!/usr/bin/env python3
"""Print render.py's exact per-shot ffmpeg commands (CPU chain, GPU chain, vidstab detect) as JSON.

Imports a *copy* of render.py (PROTO_DIR) and points its cache at PROTO_CACHE, so nothing is
written into the prototype's project.
 Extra shots that are not in the edit (the R10 clip) can be
appended with --extra-shot '<yaml/json mapping>'.

    proto_cmd.py --edit .../edit.yaml --shot payday [--shot nightpath ...] [--extra-shot '{...}']
"""
from __future__ import annotations

import argparse
import json
import os
import sys

sys.dont_write_bytecode = True
B = os.environ.get("BENCH_TMP", "/tmp/ffcine-bench")
PROTO_DIR = os.environ.get("PROTO_DIR", os.path.join(B, "proto"))
PROTO_CACHE = os.environ.get("PROTO_CACHE", os.path.join(B, "proto-cache"))
OUT = os.path.join(B, "out")
sys.path.insert(0, PROTO_DIR)

import yaml  # noqa: E402

import render as R  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--edit", required=True)
ap.add_argument("--mode", default="final")
ap.add_argument("--shot", action="append", default=[])
ap.add_argument("--extra-shot", action="append", default=[])
args = ap.parse_args()

_load = yaml.safe_load


def patched(f):
    cfg = _load(f)
    cfg["cache_dir"] = PROTO_CACHE          # absolute: os.path.join(base, abs) == abs
    for e in args.extra_shot:
        cfg["shots"].append(_load(e))
    return cfg


R.yaml.safe_load = patched
P = R.Project(args.edit, args.mode)
P.gpu = True                            # so use_gpu() reports the --gpu path
T = R.Timeline(P)
SR = R.ShotRenderer(P, T)
os.makedirs(OUT, exist_ok=True)


def cmd_for(s, gpu, trf, out):
    """render.py ShotRenderer.render() cmd_for(), output redirected."""
    inp, fc, hwout = SR.chain(s, stab_trf=trf, gpu=gpu)
    venc = SR.inter_codec()
    if hwout:
        i = venc.index("-pix_fmt")
        venc = venc[:i] + venc[i + 2:]
    return (["ffmpeg", "-v", "error", "-nostdin", "-y"] + inp +
            ["-filter_complex", fc, "-map", "[v]", "-frames:v", str(s["_n"]), "-an"] +
            venc + ["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
                    "-color_range", "tv", out])


res = []
for sid in args.shot:
    s = P.by_id[sid]
    g = P.grade(s)
    d = dict(id=sid, src=s["_path"], kind=s["_kind"], ss=float(s["in"]), out_s=float(s["out"]),
             n=s["_n"], base_n=s["_base_n"], W=P.W, H=P.H, look=g.get("look"),
             denoise=float(g.get("denoise", 0) or 0), sharpen=float(g.get("sharpen", 0) or 0),
             grain=float((P.cfg.get("grain") or {}).get(P.mode, 0) or 0),
             punch=s.get("punch"), push=s.get("push"), freeze=s.get("freeze"),
             cached=SR.out_path(s), cached_exists=os.path.exists(SR.out_path(s)))
    # the baked grade LUT exactly as the GPU path builds it (tame only for iPhone sources)
    fl = [f for f in R.grade_filters(P, g) if not f.startswith("unsharp")]
    pre = SR.tame(s["_path"]) if s["_kind"] == "video" else []
    d["lut"] = R.gpu_grade.bake(list(pre) + fl, os.path.join(P.cache, "gpu_luts"))
    d["grade_filters"] = R.grade_filters(P, g)
    # crop/punch/push + scale as libplacebo options, as the GPU path builds them (includes GPU_SCALE)
    d["geo"] = SR.gpu_geo(s, P.W, P.H, push=bool(s.get("push")))
    trf = None
    sp = R.stab_params(s)
    if sp and s["_kind"] == "dji":
        trf = os.path.join(OUT, f"{sid}.trf")
        inp, fc, _ = SR.chain(s, stab_trf=trf, detect_only=True)
        d["stab"] = sp
        d["trf"] = trf
        d["detect_cmd"] = (["ffmpeg", "-v", "error", "-nostdin", "-y"] + inp +
                           ["-filter_complex", fc, "-frames:v", str(s["_base_n"]), "-f", "null", "-"])
    d["cpu_out"] = os.path.join(OUT, f"{sid}_proto_cpu.mkv")
    d["gpu_out"] = os.path.join(OUT, f"{sid}_proto_gpu.mkv")
    d["cpu_cmd"] = cmd_for(s, False, trf, d["cpu_out"])
    d["gpu_cmd"] = cmd_for(s, SR.use_gpu(s), trf, d["gpu_out"])
    d["gpu_path_used"] = SR.use_gpu(s)
    res.append(d)
print(json.dumps(res, indent=1, default=str))
