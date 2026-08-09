#!/usr/bin/env bash
# Builds the LOCAL model VOLUMES (model-volume/<name>/) for development
# runs. The app embeds NO weights - in production the platform attaches
# Tinfoil Modelwrap volumes at /models/<name>; locally, mount a dir the same
# way (see README.md).
#
# Both volumes serve through the host's stable-diffusion.cpp wasi-nn backend
# (component layout: diffusion gguf + LLM text-encoder gguf + VAE). All
# sources are pinned to exact HuggingFace revisions and sha256s - this file
# doubles as the volume recipes:
#
#   z-image-turbo    repo: EnclaveHost/z-image-turbo-sd@d7aeefc33a479d183cedf4dce3c294ea71db29ab
#                    Z-Image-Turbo (Tongyi-MAI, 6B, Apache-2.0) curated for
#                    the sdcpp backend: Q8_0 transformer + Qwen3-4B Q8_0 text
#                    encoder + FLUX AE (~11 GB). cfg 1.0, 4-8 steps, ~13 GB
#                    peak VRAM at 1024px with tiled VAE decode. This revision
#                    carries the GENERIC component names (rename of 07cb261e).
#
#   qwen-image-2512  repo: EnclaveHost/qwen-image-2512-sd@a82dcb53ef8ff3c675dd0b636788ebb332fa973f
#                    Qwen-Image-2512 (Qwen, 20B MMDiT, Apache-2.0) with the
#                    lightx2v Lightning 8-step LoRA MERGED into the Q8_0
#                    weights - the flagship. cfg 1.0, 8 steps, ~34 GB peak
#                    VRAM at 1024px (tiled VAE). Curated 2026-07-15 by
#                    tools/merge-lightning.sh (bf16 merge, quantized by the
#                    fleet's pinned sd.cpp b5d81200); every upstream source
#                    is pinned in the curated repo's README and in that
#                    script - rerun it to reproduce diffusion.gguf
#                    bit-for-bit provenance.
#
#   realesrgan-x4plus  the stock UPSCALER volume: Real-ESRGAN x4plus
#                    (xinntao, BSD-3-Clause official weights, RRDBNet
#                    23-block 4x - the architecture sd.cpp's upscaler engine
#                    runs; heavier archs like DAT/HAT don't, and the better
#                    community finetunes carry non-commercial licenses).
#                    Built HERE, not fetched: the official release .pth
#                    converts torch-free to a plain safetensors
#                    (tools/convert_esrgan.py, deterministic - the pinned
#                    output sha256 IS the provenance chain). To be wrapped
#                    as EnclaveHost/realesrgan-x4plus-sd.
#
# Component filenames are GENERIC in both volumes (diffusion.gguf /
# llm.gguf / vae.safetensors): the platform's ENCLAVE_SD_*_FILE envs are
# node-global, so every SD volume on a node must use the same relative names
# for the volumes to COEXIST. Both curated repos carry the generic names
# (z-image at rev d7aeefc3, a pure rename of the original 07cb261e); the
# production volumes need re-wrapping from these revisions before both
# preload on one node (see enclaves/gpu/tinfoil-config.yml in the platform
# repo). The upscaler component is generic the same way
# (upscaler.safetensors, the node's ENCLAVE_SD_UPSCALE_FILE) - and its
# PRESENCE is what marks a volume as an upscaler volume, so it must never
# appear inside a generation volume.
#
# Usage: ./fetch-model.sh [z-image-turbo|qwen-image-2512|realesrgan-x4plus|all]   (default: all)
set -euo pipefail
cd "$(dirname "$0")"
want="${1:-all}"

fetch() { # <repo> <rev> <repo-path> <sha256> <dest-dir> [<dest-name>]
    local out="$5/${6:-$3}"
    mkdir -p "$(dirname "$out")"
    if [ -f "$out" ] && echo "$4  $out" | sha256sum -c --quiet - 2>/dev/null; then
        echo "$out: cached, checksum ok"
        return
    fi
    echo "fetching $1/$3 ..."
    curl -fSL --retry 3 -C - -o "$out" "https://huggingface.co/$1/resolve/$2/$3"
    echo "$4  $out" | sha256sum -c -
}

if [ "$want" = z-image-turbo ] || [ "$want" = all ]; then
    REPO=EnclaveHost/z-image-turbo-sd
    REV=d7aeefc33a479d183cedf4dce3c294ea71db29ab
    DEST=model-volume/z-image-turbo-sd
    fetch $REPO $REV diffusion.gguf df1c5baa86d1398c979495a6072dbcee79444fdb884a2445582ba0769c44e9a1 $DEST
    fetch $REPO $REV llm.gguf 391c1e410fd9f4cf2de2b510273b56a84c19ce18f4fa3bfb3774031dac4ef068 $DEST
    fetch $REPO $REV vae.safetensors afc8e28272cd15db3919bacdb6918ce9c1ed22e96cb12c4d5ed0fba823529e38 $DEST
fi

if [ "$want" = qwen-image-2512 ] || [ "$want" = all ]; then
    REPO=EnclaveHost/qwen-image-2512-sd
    REV=a82dcb53ef8ff3c675dd0b636788ebb332fa973f
    DEST=model-volume/qwen-image-2512-sd
    fetch $REPO $REV diffusion.gguf 86aafc37f65dfb4da7be8436ff6a3d91b12203ab9b855448d3d452398fb99ff2 $DEST
    fetch $REPO $REV llm.gguf ee770c700d7429cc6f0c74d6c7ab3c063bf521312fc36e80776d1d79bc9fa4ad $DEST
    fetch $REPO $REV vae.safetensors a70580f0213e67967ee9c95f05bb400e8fb08307e017a924bf3441223e023d1f $DEST
fi

if [ "$want" = realesrgan-x4plus ] || [ "$want" = all ]; then
    DEST=model-volume/realesrgan-x4plus-sd
    OUT=$DEST/upscaler.safetensors
    OUT_SHA=d30241986d4f29502949e1b46558a8c2a42067605a78428229f758429c94dda4
    if [ -f "$OUT" ] && echo "$OUT_SHA  $OUT" | sha256sum -c --quiet - 2>/dev/null; then
        echo "$OUT: cached, checksum ok"
    else
        mkdir -p "$DEST"
        PTH=$DEST/RealESRGAN_x4plus.pth.src
        curl -fSL --retry 3 -C - -o "$PTH" \
            https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth
        echo "4fa0d38905f75ac06eb49a7951b426670021be3018265fd191d2125df9d682f1  $PTH" | sha256sum -c -
        python3 tools/convert_esrgan.py "$PTH" "$OUT"
        echo "$OUT_SHA  $OUT" | sha256sum -c -
        rm -f "$PTH"
    fi
fi

echo
du -sh model-volume/* 2>/dev/null
