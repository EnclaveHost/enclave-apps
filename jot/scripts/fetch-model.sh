#!/usr/bin/env bash
# Fetch the static embedding model jot's hybrid search is built on and
# convert it to the two files the build embeds (model/embeddings.i8 and
# model/vocab.txt). Pinned to one revision and verified by digest, so the
# wasm the catalog pins reproduces from source plus this script.
#
# The model: minishlab/potion-base-8M (MIT), a Model2Vec distillation of
# bge-base-en-v1.5: a WordPiece vocabulary of 29,528 tokens, each with one
# 256-dimensional vector; a text's embedding is the mean of its tokens'
# vectors, normalised. No neural network runs at query time, which is what
# lets a wasip2 component with no GPU and no wasi-nn embed a note in
# microseconds. Table stored int8 with one scale per row (7.6 MB in the wasm
# instead of 30 MB); the loss is well inside the model's own noise.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=minishlab/potion-base-8M
REV=bf8b056651a2c21b8d2565580b8569da283cab23
SAFETENSORS_SHA=f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2
TOKENIZER_SHA=e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50
mkdir -p model
fetch() { # file sha
  local f=model/$1
  if [ ! -f "$f" ] || [ "$(sha256sum "$f" | cut -c1-64)" != "$2" ]; then
    echo "fetching $1 @ $REV"
    curl -sfL -o "$f" "https://huggingface.co/$REPO/resolve/$REV/$1"
  fi
  [ "$(sha256sum "$f" | cut -c1-64)" = "$2" ] || { echo "digest mismatch for $1" >&2; exit 1; }
}
fetch model.safetensors "$SAFETENSORS_SHA"
fetch tokenizer.json "$TOKENIZER_SHA"
python3 - <<'PY'
import json, struct, array, math
with open("model/model.safetensors", "rb") as f:
    n = struct.unpack("<Q", f.read(8))[0]
    hdr = json.loads(f.read(n))
    base = 8 + n
    meta = hdr["embeddings"]
    assert meta["dtype"] == "F32", meta
    vocab_n, dim = meta["shape"]
    off0, off1 = meta["data_offsets"]
    f.seek(base + off0)
    raw = f.read(off1 - off0)
vals = array.array("f")
vals.frombytes(raw)
assert len(vals) == vocab_n * dim
tok = json.load(open("model/tokenizer.json"))
assert tok["model"]["type"] == "WordPiece" and tok["model"]["continuing_subword_prefix"] == "##"
vocab = tok["model"]["vocab"]
assert len(vocab) == vocab_n, (len(vocab), vocab_n)
by_id = [None] * vocab_n
for t, i in vocab.items():
    by_id[i] = t
assert all(t is not None for t in by_id)
with open("model/vocab.txt", "w") as out:
    out.write("\n".join(by_id) + "\n")
with open("model/embeddings.i8", "wb") as out:
    out.write(b"JOTV1")
    out.write(struct.pack("<II", vocab_n, dim))
    for r in range(vocab_n):
        row = vals[r * dim:(r + 1) * dim]
        m = max(abs(x) for x in row) or 1.0
        scale = m / 127.0
        out.write(struct.pack("<f", scale))
        out.write(bytes((max(-127, min(127, int(round(x / scale)))) & 0xFF) for x in row))
print(f"model/embeddings.i8: {vocab_n} x {dim} int8 rows; model/vocab.txt: {vocab_n} tokens")
PY
sha256sum model/embeddings.i8 model/vocab.txt
