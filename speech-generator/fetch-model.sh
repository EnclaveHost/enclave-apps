#!/usr/bin/env bash
# Builds the LOCAL model volume (model-volume/maya1-gguf/) - and doubles as the
# RECIPE for the production Tinfoil Modelwrap volume, since it pins the exact
# HuggingFace revisions and sha256 of every file that goes in.
#
# A speech volume is an ordinary ggml volume plus TWO extra files:
#
#   maya1-q8_0.gguf     the voice model (a Llama-3.2-3B that predicts SNAC
#                       audio tokens; the host serves it like any other LLM)
#   tokenizer.json      guest-side tokenization, as for every ggml volume
#   snac_decoder.bin    the SNAC 24 kHz decoder (13M params, f16), DERIVED
#                       from the official checkpoint by tools/export_snac.py
#                       and decoded by the guest on CPU - the host has no
#                       audio path and needs none
#
# Model: Maya1 (maya-research/maya1, Apache-2.0) - the best open-weights
# text-to-speech model that is also llama.cpp-shaped, which is the property
# that lets it ride the fleet's existing wasi-nn ggml backend unchanged.
# Natural-language voice design ("Male voice in his 40s, deep and calm...")
# plus 20+ inline emotion tags. The GGUF conversion is Mungert's Q8_0 (the
# model's own repo ships safetensors only); the tokenizer comes from the
# upstream model repo it was converted from.
#
# Q8_0 (3.5 GB) rather than a smaller quant: audio-token models degrade into
# audible artifacts - clicks, warbles, wrong prosody - before text models
# degrade into wrong words, and against a 3.5 GB total the KV cache is small
# (dense 3B, 56 KiB/token at f16), so the better weights are cheap here.
#
# The SNAC decoder is DERIVED, deterministically: same checkpoint in,
# byte-identical snac_decoder.bin out (its sha256 is pinned below like any
# fetched file). Deriving needs python3 with torch and numpy; every other step
# is curl. The Rust decoder in src/snac.rs reproduces the official
# implementation at 52-59 dB SNR (the f16 floor) - tools/export_snac.py
# --golden emits the vectors that assert this.
set -euo pipefail
APP_DIR="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$APP_DIR/model-volume/maya1-gguf"
cd "$APP_DIR/model-volume/maya1-gguf"

GGUF_REPO=Mungert/maya1-GGUF
GGUF_REV=83fad52f1b11f05a52f2274998bf733f7dad7974
BASE_REPO=maya-research/maya1
BASE_REV=21c682a0afef8c13a89b2512733c8bf5f0c52eb7
SNAC_REPO=hubertsiuzdak/snac_24khz
SNAC_REV=d73ad176a12188fcf4f360ba3bf2c2fbbe8f58ec
SNAC_DECODER_SHA=e424675f10ed783b1e9b5f8375913f0db2cec80d75170763e595ff45f6368d20

fetch() { # <repo> <rev> <repo-path> <sha256> [<local-name>]
    local out="${5:-${3##*/}}"
    if [ -f "$out" ] && echo "$4  $out" | sha256sum -c --quiet - 2>/dev/null; then
        echo "$out: cached, checksum ok"
        return
    fi
    echo "fetching $3 ..."
    curl -fsSL -o "$out" "https://huggingface.co/$1/resolve/$2/$3"
    echo "$4  $out" | sha256sum -c -
}

fetch "$GGUF_REPO" "$GGUF_REV" maya1-q8_0.gguf \
    09bb46736cbbe806d34b79cd1c6c00d3a5778fd2b90103e0e636834b7baa8e7e
fetch "$BASE_REPO" "$BASE_REV" tokenizer.json \
    6c5e5b1d89b7e3738e5a5a4f93c326d8f3292ea83f9c560b8dbb6d66fb851973

# the derived codec: fetch the official checkpoint, fuse + repack, verify, and
# drop the intermediate (79 MB of PyTorch pickle has no business in a volume)
if [ -f snac_decoder.bin ] \
    && echo "$SNAC_DECODER_SHA  snac_decoder.bin" | sha256sum -c --quiet - 2>/dev/null; then
    echo "snac_decoder.bin: cached, checksum ok"
else
    fetch "$SNAC_REPO" "$SNAC_REV" pytorch_model.bin \
        4b8164cc6606bfa627f1a784734c1e539891518f1191ed9194fe1e3b9b4bff40 snac_ckpt.bin
    python3 "$APP_DIR/tools/export_snac.py" snac_ckpt.bin snac_decoder.bin
    echo "$SNAC_DECODER_SHA  snac_decoder.bin" | sha256sum -c -
    rm -f snac_ckpt.bin
fi

cat <<'EOF'

Volume ready. Locally (needs a wasmtime with the ggml wasi-nn backend):

  export LD_LIBRARY_PATH=/path/to/llamacpp-lib
  export ENCLAVE_GGML_BACKEND_DIR=$LD_LIBRARY_PATH
  export ENCLAVE_GGML_N_GPU_LAYERS=0
  export ENCLAVE_GGML_N_CTX=4096
  wasmtime serve --addr 127.0.0.1:8170 -S cli -S http -S nn \
    -S nn-graph=ggml::model-volume/maya1-gguf \
    --dir model-volume::/models \
    --env ENCLAVE_MODELS=maya1-gguf \
    --env ENCLAVE_NN_PRELOADS=maya1-gguf \
    target/wasm32-wasip2/release/speech_generator.wasm

  curl -s '127.0.0.1:8170/health?probe=1' | jq .probe   # both halves ok?
  curl -s -o hello.wav '127.0.0.1:8170/speak?text=Hello+from+the+enclave'
EOF
