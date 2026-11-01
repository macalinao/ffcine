#!/usr/bin/env python3
"""Build the one-pass ffmpeg-gpu command for a shot from proto.json (bench/proto_cmd.py output).

    onepass.py <proto.json> <shot id> <variant>     -> JSON list (argv) on stdout

Variants:
  full    NVDEC -> Vulkan -> [nlmeans_vulkan r=7] -> ONE libplacebo pass (crop/scale with EWA
          scalers, baked grade LUT, BT.2390 tone map + perceptual gamut map, deband, unsharp +
          grain hooks, blue-noise dither, Rec.709 tv p010) -> CUDA -> hevc_nvenc
  compat  the same graph minus deband and the hooks, with the prototype's scaler options, so the
          picture should equal render.py's up to LUT interpolation (for SSIM/PSNR)
  stab    full, but with render.py's vidstabtransform spliced in: libplacebo grade -> hwdownload ->
          yuv444p vidstabtransform (CPU, 8-bit as in render.py) -> yuv444p16le -> hwupload
          (an uploaded gbrp16le Vulkan frame is not sampleable: libplacebo renders black) -> libplacebo crop/scale/hooks -> NVENC
"""
import json
import os
import re
import sys

B = os.environ.get("BENCH_TMP", "/tmp/ffcine-bench")
HOOK = os.path.join(B, "shaders", "onepass.hook")     # bench/shaders/unsharp.hook + grain.hook
FPS = "24000/1001"
HW = ["-init_hw_device", "cuda=cu:0", "-init_hw_device", "vulkan=vk@cu", "-filter_hw_device", "vk"]
DEC = ["-hwaccel", "cuda", "-hwaccel_device", "cu", "-hwaccel_output_format", "cuda"]
# render.py inter_codec() (final, NVENC), minus -pix_fmt: CUDA p010 frames go in as they are
ENC = ["-c:v", "hevc_nvenc", "-preset", "p6", "-tune", "hq", "-rc", "constqp", "-qp", "12",
       "-profile:v", "main10", "-g", "24",
       "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"]
OUT_TV = "colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=p010le"
OUT_RGB = "colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=pc:format=gbrp16le"
TAIL = f"hwupload=derive_device=cuda,tpad=stop_mode=clone:stop=-1,setsar=1,settb=1001/24000,setpts=N"


def esc(p):
    return p.replace("\\", "\\\\").replace("'", "\\'").replace(":", "\\:")


def build(s, variant, out):
    geo = s["geo"]
    if variant != "compat":
        # ffcine's planned scalers instead of the swscale-matching lanczos
        geo = re.sub(r":upscaler=.*$", ":upscaler=ewa_lanczossharp:downscaler=ewa_lanczos", geo)
    grade = [f"lut='{esc(s['lut'])}'", "lut_type=normalized", "tonemapping=bt.2390", "gamut_mode=perceptual",
             "dithering=blue"]
    extras = ["deband=1", f"custom_shader_path='{esc(HOOK)}'"] if variant != "compat" else []
    # DJI tags as render.py's GPU path sets them; the R10 file is tagged correctly (pc range) itself
    tag = "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv," if s["kind"] == "dji" else ""
    pre = f"[0:v]setpts=PTS-STARTPTS,{tag}hwupload=derive_device=vulkan,fps={FPS}"
    if s["denoise"]:
        pre += f",nlmeans_vulkan=s={0.2 * s['denoise']:.2f}:r=7:t=1"
    if variant == "stab":
        st = s["stab"]
        vs = (f"vidstabtransform=input='{esc(s['trf'])}':smoothing={st['smoothing']}:optzoom=1"
              f":zoomspeed={st['zoomspeed']}:interpol=bicubic:tripod=0:crop=black")
        lp1 = ":".join(grade[:2] + ["deband=1", OUT_RGB])
        lp2 = ":".join([geo] + grade[2:] + [f"custom_shader_path='{esc(HOOK)}'", OUT_TV])
        fc = (f"{pre},libplacebo={lp1},hwdownload,format=gbrp16le,format=yuv444p,{vs},"
              f"format=yuv444p16le,hwupload=derive_device=vulkan,libplacebo={lp2},{TAIL}[v]")
        dec = HW + DEC
    else:
        lp = ":".join([geo] + grade + extras + [OUT_TV])
        fc = f"{pre},libplacebo={lp},{TAIL}[v]"
        dec = HW + DEC
    return (["ffmpeg", "-v", "error", "-nostdin", "-y"] + dec + ["-ss", f"{s['ss']:.6f}", "-i", s["src"],
            "-filter_complex", fc, "-map", "[v]", "-frames:v", str(s["n"]), "-an"] + ENC + [out])


if __name__ == "__main__":
    shots = {s["id"]: s for s in json.load(open(sys.argv[1]))}
    sid, variant = sys.argv[2], sys.argv[3]
    out = os.path.join(B, "out", f"{sid}_onepass_{variant}.mkv")
    print(json.dumps(build(shots[sid], variant, out)))
