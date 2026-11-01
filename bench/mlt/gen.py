#!/usr/bin/env python3
"""MLT XML + melt/movit-egl argv for a shot from proto.json (bench/proto_cmd.py output).

    gen.py <proto.json> <shot id> <variant> <out.mkv>   -> writes <out>.mlt, prints argv JSON

Variants (all 23.976 UHD profile, same frames as the render.py references):
  plain   melt with no filters: decode -> rgba64 -> encode, MLT's own floor
  cpu     melt: avformat decode -> [avfilter.hqdn3d] -> avfilter.lut3d (render.py's baked grade
          .cube, tetrahedral) in rgba64 -> avformat consumer hevc_nvenc, render.py's encoder flags
  grain   cpu + render.py's assembly grain (avfilter.noise, same strengths) as a shot filter
  nvdec   cpu with NVDEC decode (MLT_AVFORMAT_HWACCEL=cuda; frames are downloaded to RAM)
  movit   nvdec, then a Movit stage on the GPU (movit.lift_gamma_gain, identity) so the frame
          goes RAM -> GL texture -> Movit -> glReadPixels -> RAM -> NVENC. Movit has no LUT,
          denoise or grain effect, so the grade itself stays on the CPU. Runs under movit-egl.
"""
import json
import os
import sys
from xml.sax.saxutils import escape, quoteattr

PROFILE = "uhd_2160p_2398"
FPS = 24000 / 1001
THREADS = int(os.environ.get("MLT_THREADS", "8"))   # real_time=-N: N parallel image threads
BIN = os.environ.get("MLT_BIN", "/tmp/ffcine-bench-mlt/bin")
MELT = os.environ.get("MELT", "melt")

# render.py inter_codec() (final, NVENC) as avformat consumer properties
ENC = ["f=matroska", "vcodec=hevc_nvenc", "preset=p6", "tune=hq", "rc=constqp", "qp=12",
       "vprofile=main10", "g=24", "pix_fmt=p010le", "mlt_image_format=rgba64", "an=1",
       "color_primaries=bt709", "color_trc=bt709", "colorspace=709", "color_range=tv"]


def filt(service, **props):
    p = "".join(f'<property name={quoteattr(k.replace("__", "."))}>{escape(str(v))}</property>'
                for k, v in props.items())
    return f'<filter><property name="mlt_service">{service}</property>{p}</filter>'


def build(s, variant, n):
    src_fps = s.get("src_fps", FPS)
    first = int(-(-round(s["ss"] * src_fps * 1e6) // 1e6))        # ceil, as -ss + fps picks it
    pos = round(first / src_fps * FPS)
    res = s["src"]
    fl = []
    if variant == "plain":                                      # decode -> rgba64 -> encode only
        return xml_for(res, pos, pos + n - 1, fl)
    if s["denoise"]:
        d = s["denoise"]                                         # render.py's hqdn3d strengths
        fl.append(filt("avfilter.hqdn3d", av__luma_spatial=f"{d:.2f}", av__chroma_spatial=f"{d * .75:.2f}",
                       av__luma_tmp=f"{d * 1.5:.2f}", av__chroma_tmp=f"{d * 1.2:.2f}"))
    fl.append(filt("avfilter.lut3d", av__file=s["lut"], av__interp="tetrahedral"))
    if variant == "grain":
        g = 3.0
        fl.append(filt("avfilter.noise", av__c0s=f"{g:.2f}", av__c0f="t+u", av__c1s=f"{g * .4:.2f}",
                       av__c1f="t+u", av__c2s=f"{g * .4:.2f}", av__c2f="t+u"))
    if variant == "movit":
        fl.append(filt("movit.lift_gamma_gain"))
    return xml_for(res, pos, pos + n - 1, fl)


def xml_for(res, pos, out, fl):
    # in/out only on the playlist entry: on the <producer> too, MLT applies the in point twice
    return (f'<?xml version="1.0"?>\n<mlt>\n<producer id="shot">'
            f'<property name="resource">{escape(res)}</property>'
            f'<property name="mlt_service">avformat</property>{"".join(fl)}</producer>\n'
            f'<tractor><multitrack><playlist><entry producer="shot" in="{pos}" out="{out}"/>'
            f'</playlist></multitrack></tractor>\n</mlt>\n')


def main():
    proto, sid, variant, out = sys.argv[1:5]
    s = {x["id"]: x for x in json.load(open(proto))}[sid]
    if s["kind"] != "dji":
        s["src_fps"] = 25.0                                      # the R10 clip
    xml = out.removesuffix(".mkv") + ".mlt"
    n = int(os.environ.get("MLT_FRAMES", s["n"]))           # startup.sh: 1
    open(xml, "w").write(build(s, variant, n))
    # NVDEC: MLT_AVFORMAT_HWACCEL (the resource ?hwaccel= query only parses with a format prefix)
    env = ["env", "MLT_AVFORMAT_HWACCEL=cuda"] if variant in ("nvdec", "movit") else []
    if variant == "movit":
        argv = env + [os.path.join(BIN, "movit-egl"), PROFILE, xml, out] + ENC + ["real_time=-1"]
    else:
        argv = env + [MELT, "-silent", "-profile", PROFILE, xml, "-consumer", f"avformat:{out}"] + ENC + \
               [f"real_time=-{THREADS}"]
    print(json.dumps(dict(argv=argv, frames=n, xml=xml)))


if __name__ == "__main__":
    main()
