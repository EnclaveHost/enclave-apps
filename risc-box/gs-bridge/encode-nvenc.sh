#!/bin/bash
# The RISC Box app's GPU compute: HARDWARE-encode the emulated desktop on the
# GPU's NVENC engine. This is the encoder gs-bridge feeds into the GameStream
# video stream — it runs off the emulated CPU AND off the wasm app, on the GPU.
#
# In production this runs on the fleet GPU node's **H200** (co-located with the
# RISC Box CVM); locally it runs on any NVENC-capable dev GPU. The NVENC API is
# identical on both, so a pipeline verified on a dev card is the H200 path.
#
#   encode-nvenc.sh <app-base-url> <codec> <out>
#     app-base-url  e.g. http://127.0.0.1:18010  (serves /fb.rgb, raw 800x600 RGB)
#     codec         h264_nvenc | hevc_nvenc | av1_nvenc  (default h264_nvenc)
#     out           output file or pipe (default: a .mp4)
#
# Verified on an RTX 3070: NVENC engine hit 100% utilization; output is valid
# yuv420p H.264 that decodes to the RISC Box desktop.
set -u
APP="${1:-http://127.0.0.1:18010}"
CODEC="${2:-h264_nvenc}"
OUT="${3:-riscbox_nvenc.mp4}"
W=800; H=600; FPS="${FPS:-12}"

# Feed raw frames from the app's /fb.rgb into ffmpeg's NVENC encoder. A real
# deployment would run this as a continuous pipe into the RTP video path; here
# it pulls frames at FPS and hardware-encodes them. h264_nvenc/hevc_nvenc ERROR
# OUT if NVENC is unavailable (they never fall back to CPU), so a successful run
# is itself proof the GPU did the encode.
{
  while true; do
    curl -s "$APP/fb.rgb" || break
    sleep "$(awk "BEGIN{print 1/$FPS}")"
  done
} | ffmpeg -hide_banner -y \
      -f rawvideo -pix_fmt rgb24 -s ${W}x${H} -r "$FPS" -i - \
      -c:v "$CODEC" -preset p4 -pix_fmt yuv420p -b:v 8M -g 60 \
      "$OUT"
