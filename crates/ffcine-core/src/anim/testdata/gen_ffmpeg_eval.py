#!/usr/bin/env python3
"""Regenerates ffmpeg_eval.json: each row is [ours, ffmpeg's spelling, av_expr_eval's value].

Run from the dev shell (it needs libavutil):
    nix develop -c python3 crates/ffcine-core/src/anim/testdata/gen_ffmpeg_eval.py
Variables: t = 1.25. The test (`anim::tests::matches_av_expr_eval`) needs no libav.
"""
import ctypes, ctypes.util, json, math, os, subprocess, sys

ROWS = [
    # Functions, spelled the same in both.
    "if(1, 2, 3)", "if(0, 2, 3)", "if(0, 2)", "if(5, 2)", "ifnot(0, 2, 3)", "ifnot(1, 2)",
    "ifnot(0, 2)", "lt(1, 2)", "lt(2, 1)", "lt(2, 2)", "lte(2, 2)", "lte(3, 2)", "gt(3, 2)",
    "gt(2, 2)", "gte(2, 2)", "gte(1, 2)", "eq(2, 2)", "eq(2, 3)", "not(0)", "not(2)",
    "between(2, 1, 3)", "between(1, 1, 3)", "between(3, 1, 3)", "between(4, 1, 3)",
    "clip(5, 0, 3)", "clip(-1, 0, 3)", "clip(1.5, 0, 3)", "min(3, -2)", "max(3, -2)",
    "abs(-2.5)", "floor(-2.5)", "floor(2.5)", "ceil(-2.5)", "ceil(2.1)", "round(2.5)",
    "round(-2.5)", "round(2.4)", "trunc(-2.7)", "trunc(2.7)", "mod(7, 3)", "mod(-7, 3)",
    "mod(7, -3)", "mod(7.5, 2)", "mod(-0.5, 1)", "sqrt(2)", "pow(2, 10)", "pow(2, 0.5)",
    "exp(1)", "log(10)", "sin(1)", "cos(1)", "tan(1)", "asin(0.5)", "acos(0.5)", "atan(1)",
    "atan2(1, 2)", "atan2(-1, -2)", "hypot(3, 4)", "squish(0.3)", "squish(-1)", "gauss(0.5)",
    "gauss(0)", "lerp(1, 3, 0.25)", "lerp(2, -2, 1.5)", "PI", "E", "PHI",
    # Precedence and the operators ffmpeg shares.
    "1 + 2 * 3", "(1 + 2) * 3", "10 - 4 - 3", "64 / 4 / 2", "2 * 3 / 4", "-2^2", "2^-1",
    "-(3 - 5)", "2 ^ 0.5 * 2", "--3", "+3", "1e3 + 2.5e-1", "t * 2", "sin(t) + cos(t)^2",
    "if(lt(t, 4.7), 0, -1080 * (t - 4.7) / 0.4)", "1 - clip((t - 0.6) / 0.4, 0, 1)",
]
# Ours, ffmpeg's spelling: what ffmpeg has only as a function.
PAIRS = [
    ("7 % 3", "mod(7, 3)"), ("-7 % 3", "mod(-7, 3)"), ("7.5 % 2", "mod(7.5, 2)"),
    ("1 < 2", "lt(1, 2)"), ("2 <= 2", "lte(2, 2)"), ("3 > 4", "gt(3, 4)"),
    ("4 >= 4", "gte(4, 4)"), ("2 == 2", "eq(2, 2)"), ("2 != 2", "not(eq(2, 2))"),
    ("!0", "not(0)"), ("!3", "not(3)"), ("1 && 0", "not(not(1))*not(not(0))"),
    ("0 || 2", "not(not(0) * not(2))"), ("1 ? 2 : 3", "if(1, 2, 3)"),
    ("0 ? 2 : 3", "if(0, 2, 3)"), ("sign(-3)", "sgn(-3)"), ("sgn(2)", "sgn(2)"),
    ("sign(0)", "sgn(0)"), ("1 + 2 < 4", "lt(1 + 2, 4)"),
    ("t < 2 && t > 1 ? t * 10 : -1", "if(lt(t, 2) * gt(t, 1), t * 10, -1)"),
]

def libavutil():
    out = subprocess.run(["pkg-config", "--libs-only-L", "libavutil"], capture_output=True,
                         text=True).stdout.split()
    for d in [o[2:] for o in out if o.startswith("-L")]:
        for f in sorted(os.listdir(d)):
            if f.startswith("libavutil.so"):
                return os.path.join(d, f)
    return ctypes.util.find_library("avutil")

lib = ctypes.CDLL(libavutil())
lib.av_version_info.restype = ctypes.c_char_p
names = (ctypes.c_char_p * 2)(b"t", None)
values = (ctypes.c_double * 1)(1.25)

def ev(s):
    r = ctypes.c_double()
    rc = lib.av_expr_parse_and_eval(ctypes.byref(r), s.encode(), names, values, None, None,
                                    None, None, None, 0, None)
    if rc < 0:
        sys.exit(f"ffmpeg rejected {s!r}: {rc}")
    assert math.isfinite(r.value), s
    return r.value

rows = [[s, s, ev(s)] for s in ROWS] + [[a, b, ev(b)] for a, b in PAIRS]
doc = {"ffmpeg": lib.av_version_info().decode(), "vars": {"t": 1.25}, "rows": rows}
here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "ffmpeg_eval.json"), "w") as f:
    f.write('{"ffmpeg": %s, "vars": {"t": 1.25}, "rows": [\n' % json.dumps(doc["ffmpeg"]))
    f.write(",\n".join("  " + json.dumps(r) for r in rows))
    f.write("\n]}\n")
