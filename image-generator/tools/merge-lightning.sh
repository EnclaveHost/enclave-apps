#!/usr/bin/env bash
# Produces model-volume/qwen-image-2512-sd/diffusion.gguf: the Qwen-Image-2512
# transformer with the lightx2v Lightning 8-step LoRA merged, quantized to
# Q8_0 by the SAME stable-diffusion.cpp revision the fleet serves with -
# quantizer/serving-engine skew is a real failure mode, keep them pinned
# together (wasm/sd-shim/README.md in the platform repo).
#
# Pipeline (all steps resumable; ~62 GB scratch under model-volume/.qwen-image-2512-src):
#   1. download the bf16 base transformer shards (Qwen/Qwen-Image-2512)
#   2. merge the Lightning LoRA shard-by-shard      (tools/merge_lora.py)
#   3. build sd.cpp @b5d81200 (CPU is fine for convert) and quantize Q8_0
#   4. smoke-test: load the gguf and render one 8-step 512px image
#
# After it succeeds, upload diffusion.gguf + llm.gguf + vae.safetensors
# (fetch-model.sh fetches the latter two) to the curated HF repo
# (EnclaveHost/qwen-image-2512-sd) and wrap it as a Modelwrap volume.
set -euo pipefail
cd "$(dirname "$0")/.."

BASE_REPO=Qwen/Qwen-Image-2512
BASE_REV=25468b98e3276ca6700de15c6628e51b7de54a26   # resolved 2026-07-15
SD_COMMIT=b5d812008eb7082a238fc589444544b3278187ae   # = fleet llamacpp-toolchain SD_COMMIT
SRC=model-volume/.qwen-image-2512-src
DEST=model-volume/qwen-image-2512-sd
LORA=$SRC/Qwen-Image-2512-Lightning-8steps-V1.0-bf16.safetensors
# torch+safetensors interpreter (a venv under $SRC/venv wins if present)
PY="${PYTHON:-$([ -x "$SRC/venv/bin/python3" ] && echo "$SRC/venv/bin/python3" || echo python3)}"

[ -f "$LORA" ] || { echo "run ./fetch-model.sh qwen-image-2512 first (fetches the LoRA + TE + VAE)"; exit 1; }
mkdir -p "$SRC/base" "$DEST"

# -- 1. base transformer shards (bf16, ~41 GB) ------------------------------
echo "== downloading $BASE_REPO transformer (bf16 shards)"
python3 - "$BASE_REPO" "$BASE_REV" "$SRC/base" <<'EOF'
import json, pathlib, sys, urllib.request
repo, rev, dest = sys.argv[1], sys.argv[2], pathlib.Path(sys.argv[3])
api = f"https://huggingface.co/api/models/{repo}/tree/{rev}/transformer"
files = json.load(urllib.request.urlopen(api))
wanted = [f for f in files if f["path"].endswith((".safetensors", ".json"))]
print(f"resolved {repo}@{rev}: {len(wanted)} files")
for f in wanted:
    out = dest / pathlib.Path(f["path"]).name
    if out.exists() and out.stat().st_size == f.get("size", -1):
        print(f"  {out.name}: cached")
        continue
    print(f"  {out.name} ({f.get('size',0)/2**30:.1f} GiB)...")
    urllib.request.urlretrieve(f"https://huggingface.co/{repo}/resolve/{rev}/{f['path']}", out)
EOF

# -- 2. merge the LoRA -------------------------------------------------------
MERGED=$SRC/qwen-image-2512-lightning8-bf16.safetensors
if [ ! -f "$MERGED" ]; then
    echo "== merging Lightning LoRA (streaming, needs torch+safetensors)"
    "$PY" tools/merge_lora.py "$SRC/base" "$LORA" "$MERGED"
else
    echo "== merge cached: $MERGED"
fi

# -- 3. quantize with pinned sd.cpp ------------------------------------------
SD=$SRC/sd.cpp
SDBIN=$SD/build/bin/sd-cli
if [ ! -x "$SDBIN" ]; then
    echo "== building stable-diffusion.cpp @${SD_COMMIT:0:8} (CPU build; convert only)"
    [ -d "$SD" ] || git clone https://github.com/leejet/stable-diffusion.cpp "$SD"
    git -C "$SD" checkout "$SD_COMMIT"
    git -C "$SD" submodule update --init ggml
    cmake -S "$SD" -B "$SD/build" -DCMAKE_BUILD_TYPE=Release >/dev/null
    cmake --build "$SD/build" --config Release -j"$(nproc)" >/dev/null
fi
if [ ! -f "$DEST/diffusion.gguf" ]; then
    echo "== quantizing Q8_0"
    "$SDBIN" -M convert --diffusion-model "$MERGED" \
        -o "$DEST/diffusion.gguf" --type q8_0 -v
fi

# -- 4. smoke test ------------------------------------------------------------
echo "== smoke test: 8-step 512px render (CPU - slow, ~minutes on 16 cores)"
"$SDBIN" --diffusion-model "$DEST/diffusion.gguf" \
    --llm "$DEST/llm.gguf" --vae "$DEST/vae.safetensors" \
    -p "a red fox in snow, studio lighting" --cfg-scale 1.0 --steps 8 \
    -W 512 -H 512 -s 7 -o "$SRC/smoke.png" -v
echo "== wrote $SRC/smoke.png - INSPECT IT before curating the volume"
du -sh "$DEST"/* "$SRC"/smoke.png
