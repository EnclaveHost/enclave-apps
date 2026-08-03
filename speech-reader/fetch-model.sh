#!/usr/bin/env bash
# Builds the LOCAL model volume (model-volume/granite-speech-4.1-2b-gguf/) -
# and doubles as the RECIPE for the production Tinfoil Modelwrap volume, since
# it pins the exact HuggingFace revisions and sha256 of every file that goes in.
#
# A speech-INPUT volume is an ordinary ggml volume plus the audio encoder,
# and it looks exactly like a vision volume on disk - the encoder rides the
# same *mmproj* naming convention the host picks a projector out by:
#
#   granite-speech-4.1-2b-Q8_0.gguf   the LM (dense granite, adapters merged)
#   mmproj-model-f16.gguf             the conformer audio encoder + projector
#   tokenizer.json                    guest-side tokenization, as always
#
# Model: Granite Speech 4.1 2B (ibm-granite, Apache-2.0) - 5.33% mean WER on
# the Open ASR leaderboard, the best open-weights figure among models that
# are also llama.cpp-shaped (which is the property that lets it ride the
# fleet's wasi-nn ggml backend; the leaderboard's NeMo-runtime entries
# cannot). Six languages (en fr de es pt ja), punctuated transcription, and
# speech translation to English. The GGUFs are IBM'S OWN conversion.
#
# Q8_0 (2.0 GB) with the F16 encoder (1.2 GB): the whole volume is smaller
# than one vision model, and quantizing the ENCODER is where transcription
# accuracy dies, so it stays f16 (as IBM ships it).
set -euo pipefail
APP_DIR="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$APP_DIR/model-volume/granite-speech-4.1-2b-gguf"
cd "$APP_DIR/model-volume/granite-speech-4.1-2b-gguf"

GGUF_REPO=ibm-granite/granite-speech-4.1-2b-GGUF
GGUF_REV=8267dad2adc84209b0efd2702ec68a98356125eb
BASE_REPO=ibm-granite/granite-speech-4.1-2b
BASE_REV=de575db64086f84fdc79da4932d1076e965bc546

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

fetch "$GGUF_REPO" "$GGUF_REV" granite-speech-4.1-2b-Q8_0.gguf \
    87320ee80cc6e638837bd2e9b7920252a87148613930bcdc11fcafad0b00beea
fetch "$GGUF_REPO" "$GGUF_REV" mmproj-model-f16.gguf \
    0d3615076cbe1d35c3f60c43a60a4047b3e2eeee1b2c233580be60186faab5c5
fetch "$BASE_REPO" "$BASE_REV" tokenizer.json \
    43ca88fd0519c64ef93fa0a90cbc4e560fe485b5ba60348a86bc3c624f37918e

cat <<'EOF'

Volume ready. Locally (needs a wasmtime whose shim carries the audio verb -
PLATFORM.md - and a llama.cpp lib with libmtmd):

  export LD_LIBRARY_PATH=/path/to/llamacpp-lib
  export ENCLAVE_GGML_BACKEND_DIR=$LD_LIBRARY_PATH
  export ENCLAVE_GGML_N_GPU_LAYERS=0
  export ENCLAVE_GGML_N_CTX=4096
  wasmtime serve --addr 127.0.0.1:8180 -S cli -S http -S nn \
    -S nn-graph=ggml::model-volume/granite-speech-4.1-2b-gguf \
    --dir model-volume::/models \
    --env ENCLAVE_MODELS=granite-speech-4.1-2b-gguf \
    --env ENCLAVE_NN_PRELOADS=granite-speech-4.1-2b-gguf \
    target/wasm32-wasip2/release/speech_reader.wasm

  curl -s '127.0.0.1:8180/health?probe=1' | jq .probe    # can this node hear?
  curl -s --data-binary @clip.wav 127.0.0.1:8180/transcribe
EOF
