# Shared settings for the bench scripts (sourced).
# ffmpeg: an ffmpeg-full-gpu 9.0.1 build (libplacebo git API 375 + glslang, so nlmeans_vulkan
# exists), e.g. `nix build .#ffmpeg-gpu`. Most system ffmpeg builds lack nlmeans_vulkan.
export FF=${FF:-ffmpeg}
export FFPROBE=${FFPROBE:-${FF%/ffmpeg}/ffprobe}
export GNUTIME=${GNUTIME:-/usr/bin/time}
# The prototype's project dir (edit.yaml, pipeline/render.py, luts/), for the comparison benches.
export PROJ=${PROJ:-/media/footage/sample-project/05_project}

export PROTO_SHELL=${PROTO_SHELL:-$PROJ/.nix/dev}      # python + the same ffmpeg, for render.py
export BENCH_TMP=${BENCH_TMP:-/tmp/ffcine-bench}
export BENCH_DIR=${BENCH_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}
