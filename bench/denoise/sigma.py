#!/usr/bin/env python3
"""Noise of a flat patch, in 8-bit code values, averaged over frames. No numpy needed.

Prints `spatial temporal`: the robust (MAD) sigma of the luma residual against its 4-neighbour
mean, and the mean absolute frame-to-frame difference. BIN > 1 box-averages BINxBIN first, to
measure the blotchy low-frequency noise that in-camera NR and HEVC leave behind.

    sigma.py FILE X Y [SIZE] [FRAMES] [SS] [BIN]   (needs ffmpeg on PATH)
"""
import array, statistics, subprocess, sys

f, x, y = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
n = int(sys.argv[4]) if len(sys.argv) > 4 else 128
frames = int(sys.argv[5]) if len(sys.argv) > 5 else 24
ss = sys.argv[6] if len(sys.argv) > 6 else "0"
b = int(sys.argv[7]) if len(sys.argv) > 7 else 1
raw = subprocess.run(
    ["ffmpeg", "-loglevel", "error", "-ss", ss, "-i", f, "-frames:v", str(frames),
     "-vf", f"crop={n}:{n}:{x}:{y},scale={n // b}:{n // b}:flags=area,format=gray10le", "-f", "rawvideo", "-"],
    check=True, capture_output=True).stdout
a = array.array("H", raw)
n //= b
sig, tmp = [], []
for k in range(len(a) // (n * n)):
    p = a[k * n * n:(k + 1) * n * n]
    r = [p[i * n + j] - (p[i * n + j - 1] + p[i * n + j + 1] + p[(i - 1) * n + j] + p[(i + 1) * n + j]) / 4
         for i in range(1, n - 1) for j in range(1, n - 1)]
    med = statistics.median(abs(v) for v in r)
    sig.append(1.4826 * med / 1.25 ** 0.5 / 4)
    if k:
        q = a[(k - 1) * n * n:k * n * n]
        tmp.append(sum(abs(u - v) for u, v in zip(p, q)) / (n * n) / 4)
print(f"{statistics.mean(sig):.3f} {statistics.mean(tmp) if tmp else 0:.3f}")
