#!/usr/bin/env python3
"""Synthetic scenes with ground truth for tuning the denoise presets (docs/denoise.md). Needs numpy
and ffmpeg.

    synth.py gen stars|stars-loud|stars-blotchy|snow OUT_DIR [FRAMES]   # clean.mkv, noisy.mkv, truth.npz
    synth.py score OUT.mkv REF.mkv TRUTH.npz                 # one line of metrics

stars: a dark sky (D-Log level, a soft moving glow) with point lights from 3 to 60 code values over
it, drifting slightly, and white noise (stars, stars-loud) or low-pass noise (stars-blotchy, what
in-camera NR and HEVC leave). snow: a bright slope of fine low-contrast texture with little noise. Everything is
in 8-bit code values of the 10-bit signal. REF is the clean clip through the same grade as OUT.
"""
import subprocess, sys
import numpy as np

W, H = 1920, 1080
BINS = ((3, 6), (6, 12), (12, 25), (25, 60))


def blur(a, sigma):
    """Gaussian blur through the FFT, unit gain."""
    fy, fx = np.fft.fftfreq(a.shape[0])[:, None], np.fft.rfftfreq(a.shape[1])[None, :]
    g = np.exp(-2 * (np.pi * sigma) ** 2 * (fx * fx + fy * fy))
    return np.fft.irfft2(np.fft.rfft2(a) * g, s=a.shape)


def shifted(spec, dx, dy, shape):
    fy, fx = np.fft.fftfreq(shape[0])[:, None], np.fft.rfftfreq(shape[1])[None, :]
    return np.fft.irfft2(spec * np.exp(-2j * np.pi * (fx * dx + fy * dy)), s=shape)


def writer(path):
    return subprocess.Popen(
        ["ffmpeg", "-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", "yuv420p10le",
         "-s", f"{W}x{H}", "-r", "24", "-i", "-", "-color_range", "tv", "-colorspace", "bt709",
         "-color_primaries", "bt709", "-color_trc", "bt709", "-c:v", "ffv1", path],
        stdin=subprocess.PIPE)


def put(proc, y):
    chroma = np.full((H // 2, W // 2), 512, np.uint16).tobytes()
    proc.stdin.write(np.clip(np.round(y * 4), 0, 1023).astype(np.uint16).tobytes())
    proc.stdin.write(chroma + chroma)


def noise(rng, sigma, lowpass):
    n = rng.standard_normal((H, W))
    if lowpass:
        n = blur(n, lowpass)
        n /= n.std()
    return sigma * n


def gen(kind, out, frames):
    rng = np.random.default_rng(7)
    clean, noisy = writer(f"{out}/clean.mkv"), writer(f"{out}/noisy.mkv")
    yy, xx = np.mgrid[0:H, 0:W].astype(np.float32)
    drift = np.cumsum(rng.normal(0.15, 0.08, (frames, 2)), axis=0)
    truth = {"kind": kind, "drift": drift}
    if kind.startswith("stars"):
        n = 900
        sx = rng.uniform(16, W - 16, n)
        sy = rng.uniform(16, H - 16, n)
        amp = np.exp(rng.uniform(np.log(3), np.log(60), n))
        # Stars far enough apart to measure each one alone.
        keep = []
        for i in range(n):
            if all((sx[i] - sx[j]) ** 2 + (sy[i] - sy[j]) ** 2 > 18 ** 2 for j in keep):
                keep.append(i)
        sx, sy, amp = sx[keep], sy[keep], amp[keep]
        truth.update(sx=sx, sy=sy, amp=amp)
        for f in range(frames):
            glow = 42 + 3 * xx / W + 8 * np.exp(-(((yy - 300 - 0.15 * xx - 2 * f) / 180) ** 2))
            y = glow.copy()
            for x0, y0, a in zip(sx + drift[f, 0], sy + drift[f, 1], amp):
                ix, iy = int(x0), int(y0)
                gx, gy = xx[iy - 4:iy + 5, ix - 4:ix + 5] - x0, yy[iy - 4:iy + 5, ix - 4:ix + 5] - y0
                y[iy - 4:iy + 5, ix - 4:ix + 5] += a * np.exp(-(gx * gx + gy * gy) / (2 * 0.8 ** 2))
            put(clean, y)
            put(noisy, y + {"stars": noise(rng, 2.0, 0), "stars-loud": noise(rng, 4.0, 0)}.get(kind, noise(rng, 1.5, 1.2)))
    elif kind == "snow":
        tex = blur(rng.standard_normal((H, W)), 0.9) + 0.5 * blur(rng.standard_normal((H, W)), 3)
        tex /= tex.std()
        spec = np.fft.rfft2(tex)
        truth.update(texture=2.5)
        for f in range(frames):
            y = 205 + 15 * yy / H + 2.5 * shifted(spec, drift[f, 0] * 3, drift[f, 1] * 3, (H, W))
            put(clean, y)
            put(noisy, y + noise(rng, 0.8, 0.6))
    clean.stdin.close(), noisy.stdin.close()
    clean.wait(), noisy.wait()
    np.savez(f"{out}/truth.npz", **truth)


def luma(path):
    raw = subprocess.run(["ffmpeg", "-loglevel", "error", "-i", path, "-vf", "format=gray10le",
                          "-f", "rawvideo", "-"], check=True, capture_output=True).stdout
    return np.frombuffer(raw, np.uint16).reshape(-1, H, W).astype(np.float32) / 4


def ssim(a, b):
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    ma, mb = blur(a, 1.5), blur(b, 1.5)
    va, vb, cov = blur(a * a, 1.5) - ma * ma, blur(b * b, 1.5) - mb * mb, blur(a * b, 1.5) - ma * mb
    return float((((2 * ma * mb + c1) * (2 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))).mean())


def score(out, ref, truth):
    o, r = luma(out), luma(ref)
    t = np.load(truth)
    n = min(len(o), len(r))
    o, r = o[:n], r[:n]
    err = o - r
    psnr = 10 * np.log10(255 ** 2 / np.mean(err[2:-2] ** 2))
    line = [f"psnr {psnr:.2f}"]
    if str(t["kind"]).startswith("stars"):
        sx, sy, amp, drift = t["sx"], t["sy"], t["amp"], t["drift"]
        got, want, halo_o, halo_r = [np.zeros_like(amp) for _ in range(4)]
        # Measured on the middle frames, at the true position; noise averages out over frames.
        fr = range(4, n - 4)
        for f in fr:
            ix = np.round(sx + drift[f, 0]).astype(int)
            iy = np.round(sy + drift[f, 1]).astype(int)
            bg = lambda a: (a[iy - 6, ix - 6] + a[iy - 6, ix + 6] + a[iy + 6, ix - 6] + a[iy + 6, ix + 6]) / 4
            got += o[f, iy, ix] - bg(o[f])
            want += r[f, iy, ix] - bg(r[f])
            ring = lambda a: (a[iy - 3, ix] + a[iy + 3, ix] + a[iy, ix - 3] + a[iy, ix + 3]) / 4 - bg(a)
            halo_o += ring(o[f])
            halo_r += ring(r[f])
        for lo, hi in BINS:
            m = (amp >= lo) & (amp < hi)
            line.append(f"peak{lo}-{hi} {got[m].sum() / want[m].sum():.3f}")
        line.append(f"halo {(halo_o - halo_r).sum() / want.sum():+.4f}")
        # Sky away from any star, measured against the clean clip.
        mask = np.ones((H, W), bool)
        for x0, y0 in zip(np.round(sx + drift[n // 2, 0]).astype(int), np.round(sy + drift[n // 2, 1]).astype(int)):
            mask[max(y0 - 12, 0):y0 + 13, max(x0 - 12, 0):x0 + 13] = False
        res = err[4:n - 4]
        res = res - res.mean(axis=(1, 2), keepdims=True)
        line.append(f"sky_sigma {res[:, mask].std():.3f}")
        line.append(f"sky_temporal {np.abs(np.diff(err[4:n - 4], axis=0))[:, mask].mean():.3f}")
        line.append(f"sky_p999 {np.quantile(np.abs(res[n // 2 - 4][mask]), 0.999):.2f}")
    else:
        hp = lambda a: a - blur(a, 2.0)
        f = n // 2
        ratio = float(np.mean([hp(o[k]).std() / hp(r[k]).std() for k in range(f - 4, f + 4)]))
        line.append(f"hp_energy {ratio:.3f}")
        line.append(f"ssim {np.mean([ssim(o[k], r[k]) for k in range(f - 2, f + 2)]):.4f}")
        line.append(f"temporal {np.abs(np.diff(err[4:n - 4], axis=0)).mean():.3f}")
    print(" ".join(line))


if sys.argv[1] == "gen":
    import os
    os.makedirs(sys.argv[3], exist_ok=True)
    gen(sys.argv[2], sys.argv[3], int(sys.argv[4]) if len(sys.argv) > 4 else 48)
else:
    score(*sys.argv[2:5])
