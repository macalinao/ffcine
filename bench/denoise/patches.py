#!/usr/bin/env python3
"""Compares a denoised render with the same clip undenoised, without ground truth (docs/denoise.md).
Needs numpy and ffmpeg.

    patches.py BASE.mkv OUT.mkv [SIZE]

Picks the flattest and the most textured SIZE tile of BASE (middle frame) and prints, in 8-bit code
values: flat sigma and temporal (the frame-to-frame "boiling"), full-resolution and after 4x4
binning, then on the textured tile the high-pass energy relative to BASE and the SSIM against BASE.
"""
import subprocess, sys
import numpy as np


def read(path, crop=None):
    cmd = ["ffmpeg", "-loglevel", "error", "-i", path]
    probe = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
                            "stream=width,height", "-of", "csv=p=0", path],
                           check=True, capture_output=True, text=True).stdout
    w, h = map(int, probe.strip().split(",")[:2])
    vf = "format=gray10le"
    if crop:
        x, y, n = crop
        vf = f"crop={n}:{n}:{x}:{y},{vf}"
        w, h = n, n
    raw = subprocess.run(cmd + ["-vf", vf, "-f", "rawvideo", "-"], check=True, capture_output=True).stdout
    return np.frombuffer(raw, np.uint16).reshape(-1, h, w).astype(np.float32) / 4


def lap(a):
    return a[1:-1, 1:-1] - (a[1:-1, :-2] + a[1:-1, 2:] + a[:-2, 1:-1] + a[2:, 1:-1]) / 4


def sigma(a):
    return 1.4826 * np.median(np.abs(lap(a))) / 1.25 ** 0.5


def box(a, k):
    n = a.shape[-1] // k * k
    a = a[..., :n, :n]
    shape = a.shape[:-2] + (n // k, k, n // k, k)
    return a.reshape(shape).mean(axis=(-1, -3))


def ssim(a, b):
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    ma, mb = box(a, 8), box(b, 8)
    va, vb = box(a * a, 8) - ma * ma, box(b * b, 8) - mb * mb
    cov = box(a * b, 8) - ma * mb
    return float((((2 * ma * mb + c1) * (2 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))).mean())


def tiles(frame, n):
    """(score, x, y) of each tile, from the Laplacian's energy; ignores clipped tiles."""
    h, w = frame.shape
    out = []
    for y in range(0, h - n + 1, n // 2):
        for x in range(0, w - n + 1, n // 2):
            t = frame[y:y + n, x:x + n]
            if t.mean() > 2 and np.mean(t > 253) < 0.2:
                out.append((float(np.abs(lap(t)).mean()), x, y))
    return out


base_path, out_path = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 512
base = read(base_path)
ts = tiles(base[len(base) // 2], n)
_, fx, fy = min(ts)
_, tx, ty = max(ts)
o = read(out_path, (fx, fy, n))
k = min(len(o), len(base))
flat = o[:k]
fmt = lambda v: f"{np.mean(v):.3f}"
print(f"flat@luma{flat.mean():.0f} sigma {fmt([sigma(f) for f in flat])} temporal {np.abs(np.diff(flat, axis=0)).mean():.3f}",
      f"bin4 sigma {fmt([sigma(box(f, 4)) for f in flat])} temporal {np.abs(np.diff(box(flat, 4), axis=0)).mean():.3f}",
      end=" ")
bt = base[:k, ty:ty + n, tx:tx + n]
ot = read(out_path, (tx, ty, n))[:k]
hp = lambda a: lap(a).std()
print(f"tex@luma{bt.mean():.0f} hp {np.mean([hp(x) / hp(y) for x, y in zip(ot, bt)]):.3f} ssim {np.mean([ssim(x, y) for x, y in zip(ot, bt)]):.4f}")
