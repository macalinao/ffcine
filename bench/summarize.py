#!/usr/bin/env python3
"""Summarise one run directory written by run.sh into a JSON line (stdout)."""
import json
import os
import re
import sys

d = sys.argv[1]
label = os.path.basename(d)


def read(name, default=""):
    try:
        return open(os.path.join(d, name)).read()
    except FileNotFoundError:
        return default


def rows(txt):
    """nvidia-smi dmon/pmon text -> (header, [row dicts])."""
    hdr, out = None, []
    for ln in txt.splitlines():
        if ln.startswith("# gpu"):
            hdr = ln[1:].split()
            continue
        if ln.startswith("#") or not ln.strip() or hdr is None:
            continue
        f = ln.split()
        if len(f) < len(hdr):        # last line cut off when dmon/pmon are killed
            continue
        out.append(dict(zip(hdr, f)))
    return out


def num(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return 0.0


def mean(xs):
    xs = list(xs)
    return round(sum(xs) / len(xs), 1) if xs else None


t = read("time.txt")
m = re.search(r"Elapsed \(wall clock\) time.*: (?:(\d+):)?(\d+):([\d.]+)", t)
wall = (int(m.group(1) or 0) * 3600 + int(m.group(2)) * 60 + float(m.group(3))) if m else None
user = num((re.search(r"User time \(seconds\): ([\d.]+)", t) or [0, 0])[1])
sys_ = num((re.search(r"System time \(seconds\): ([\d.]+)", t) or [0, 0])[1])
rss = num((re.search(r"Maximum resident set size \(kbytes\): (\d+)", t) or [0, 0])[1])

dm = rows(read("dmon.txt"))
idle = rows(read("idle_dmon.txt"))
pm = rows(read("pmon.txt"))
pid = read("pid").split()
pid = pid[0] if pid else None
ours = [r for r in pm if r.get("pid") == pid]
others = {}
for r in pm:
    if r.get("pid") in (pid, None, "-"):
        continue
    if any(r.get(k, "-") != "-" for k in ("sm", "enc", "dec")):
        others.setdefault(r["pid"] + ":" + r.get("command", "?"), []).append(r)

# frames: the n the command asked for (-frames:v N, ffcine --frames N)
cmd = read("cmd.txt")
fm = re.search(r"(?:-frames:v|--frames) (\d+)", cmd)
frames = int(fm.group(1)) if fm else None

res = dict(
    label=label, rc=int(read("rc", "0").strip() or 0), wall_s=wall, frames=frames,
    fps=round(frames / wall, 2) if frames and wall else None,
    user_s=user, sys_s=sys_, cpu_pct=round(100 * (user + sys_) / wall) if wall else None,
    max_rss_mb=round(rss / 1024),
    load_before=read("load_before").split()[:3], load_after=read("load_after").split()[:3],
    gpu_idle=dict(sm=mean(num(r["sm"]) for r in idle), enc=mean(num(r["enc"]) for r in idle),
                  dec=mean(num(r["dec"]) for r in idle), fb_mb=mean(num(r["fb"]) for r in idle)),
    gpu_during=dict(sm=mean(num(r["sm"]) for r in dm), enc=mean(num(r["enc"]) for r in dm),
                    dec=mean(num(r["dec"]) for r in dm), fb_mb=mean(num(r["fb"]) for r in dm)),
    gpu_ours=dict(sm=mean(num(r["sm"]) for r in ours), enc=mean(num(r["enc"]) for r in ours),
                  dec=mean(num(r["dec"]) for r in ours), samples=len(ours)),
    gpu_others_during={k: dict(sm=mean(num(r["sm"]) for r in v), enc=mean(num(r["enc"]) for r in v),
                               dec=mean(num(r["dec"]) for r in v)) for k, v in others.items()},
    apps_before=[ln.strip() for ln in read("apps_before.csv").splitlines() if ln.strip()],
)
print(json.dumps(res))
