#!/usr/bin/env bash
# Builds a LOCAL vision model VOLUME (model-volume/qwen3-vl-8b-gguf/) - and
# doubles as the RECIPE for the production Tinfoil Modelwrap volume, since it
# pins the exact HuggingFace revisions and sha256 of every file that goes in.
#
# A vision volume is an ordinary ggml volume plus ONE extra file: the mmproj
# GGUF holding the vision encoder and its projector. The host finds it by
# name (anything matching *mmproj*.gguf) and loads it lazily, on the first
# image a deployment is actually sent - so a chat that never attaches a
# picture never pays for it.
#
#   <model>.gguf        the language model (what decodes text)
#   mmproj-*.gguf       the vision tower (what turns pixels into embeddings)
#   tokenizer.json      guest-side tokenization, as for every ggml volume
#
# Model: Qwen3-VL-8B-Instruct, official Qwen GGUF conversion. Published as the
# curated volume EnclaveHost/qwen3-vl-8b-gguf; this script rebuilds it byte for
# byte from the same pins. Chosen for this fleet because llama.cpp has a
# first-class graph for it (tools/mtmd/models/qwen3vl.cpp), it is causal rather
# than non-causal (so it needs no ENCLAVE_GGML_N_UBATCH raise), and it reads
# documents, screenshots and charts well enough that "what does this error
# say" is a question worth asking it.
#
# Its tokenizer is NOT the one the qwen3.5/3.6 volumes share: vocab 151936 here
# against their 248320. So this model cannot draft for them and they cannot
# draft for it - leave `draft` unset on its catalog entry.
#
# Q8_0 weights (8.7 GB) with the F16 projector (1.2 GB) - the same pair the
# curated EnclaveHost/qwen3-vl-8b-gguf volume carries. Q8_0 rather than Q4_K_M
# because serving cost here is dominated by the KV cache (dense model, ~144 KiB
# per token at f16), so better weights cost only ~3.7 GB of a ~25 GB total, and
# vision is where quantization damage hurts most: misreading a number off a
# chart is a worse failure than writing a clumsy sentence. Swap in
# Qwen3VL-8B-Instruct-Q4_K_M.gguf (sha
# 67d1659bfe71b89d50b45a4ad1a9e5b997e5bb16ce5da66a6a6167abd569e9e2) if you are
# aiming at a smaller share or a larger context window.
#
# SIZING, because this one surprises people: unlike the hybrid qwen3.5/3.6
# models, Qwen3-VL is DENSE - every one of its 36 layers holds KV. At the
# fleet's node window that is ~144 KB per token of KV, so the deployment's
# share has to cover the weights AND a full window of it. Check the app's own
# arithmetic with GET /models (the `fits` flag) before publishing a config.
set -euo pipefail
mkdir -p "$(dirname "$0")/model-volume/qwen3-vl-8b-gguf"
cd "$(dirname "$0")/model-volume/qwen3-vl-8b-gguf"

# the GGUF conversion and the base repo are pinned separately: the weights
# come from Qwen's own GGUF repo, the tokenizer from the model repo it was
# converted from (the GGUF carries its own copy internally, but the guest
# tokenizes prompts itself and needs the file)
GGUF_REPO=Qwen/Qwen3-VL-8B-Instruct-GGUF
GGUF_REV=f982a07559d4a2f6c8744d840bf6fccab30eea96
BASE_REPO=Qwen/Qwen3-VL-8B-Instruct
BASE_REV=0c351dd01ed87e9c1b53cbc748cba10e6187ff3b

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

# Upstream file names are kept, as in every other curated volume: the quant is
# visible in the name, and the two ggufs stay unambiguous because the projector
# is identified by its "mmproj" name and taken out of the model pick before
# "exactly one gguf" is decided.
fetch "$GGUF_REPO" "$GGUF_REV" Qwen3VL-8B-Instruct-Q8_0.gguf \
    0d264b3941185d00a74f75c4245521dae088ff1efc90ab8d1754e83f5844adb0
fetch "$GGUF_REPO" "$GGUF_REV" mmproj-Qwen3VL-8B-Instruct-F16.gguf \
    ca524100ebf825c9a870db1c580d03879e0da0ab2541697e2458e64891cf9d38
fetch "$BASE_REPO" "$BASE_REV" tokenizer.json \
    a5d85b6dcc535e6b93115a9ef287e6132fdbf30270da6218194ba742261173c7

cat <<'EOF'

Volume ready. Locally:

  wasmtime serve -S nn -S nn-graph=ggml::$PWD \
    --dir "$PWD"::/models/qwen3-vl-8b-gguf \
    --env ENCLAVE_MODELS=qwen3-vl-8b-gguf \
    --env ENCLAVE_NN_PRELOADS=qwen3-vl-8b-gguf \
    --env ENCLAVE_CONFIG='{"model_volume":"qwen3-vl-8b-gguf","backend":"ggml",
      "name":"qwen3-vl-8b","vision":true,"template":"chatml","n_layers":36,
      "n_kv_heads":8,"head_dim":128,"vocab":151936,"eos":[151645,151643]}' \
    target/wasm32-wasip2/release/llm_chat.wasm

In production: wrap this directory as a Modelwrap volume, attach it, and add
the matching `models` catalog entry (see assets/deploy-config.template.json).
EOF
