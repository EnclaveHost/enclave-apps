# image-reader: a vision model on an Enclave GPU share

A wasm component that answers questions about images. Drop in a screenshot and
ask what the error says; post a photo of a form and get the fields back; hand it
a chart and ask for the numbers. Ships **no weights**: the model arrives as an
attached Modelwrap volume carrying a GGUF, its `mmproj` vision projector and a
tokenizer, and the ~5 MB component carries only the request plumbing, the prompt
assembly, the wasi-nn orchestration and a self-contained playground at `/`.

```
image bytes ──▶ host "image" verb ──▶ vision encoder + projector ──▶ KV cache
                (llama.cpp mtmd)                                       │
question ────▶ tokenizer ──▶ "tokens" verb ─────────────────────────▶ decode ──▶ text
```

The picture crosses to the host as the **file**, not as pixels: decoding,
resizing, the encoder, the projector and the model's own marker tokens all live
behind one wasi-nn verb, and the host answers with the number of sequence
POSITIONS the image consumed. So this component contains no image-format code at
all, and supporting a new VLM architecture is a llama.cpp change rather than a
change here.

The stock catalog:

| model | what | window | image cost |
|---|---|---|---|
| `qwen3-vl-8b` | Qwen3-VL-8B-Instruct Q8_0 + F16 projector (Apache-2.0). Reads documents, screenshots and charts; dynamic resolution, M-RoPE | 32K prompt budget | ~250-1600 positions per image, by grid |

## Why this is its own app

Seeing and chatting have different lifecycles. A vision model is idle most of
the time and expensive while it runs, its VRAM is sized by one dense KV window
rather than by a conversation backlog, and the thing an operator wants is to
**start it, stop it, resize it or restart it without touching the chat everyone
is using**. Two deployments, two funding rates, two lifecycles.

The sibling [llm-chat](../llm-chat/) app then points its `vision` config block
at this deployment and folds the answer into its own reply, which is what lets
the chat model be the biggest thing the fleet holds while the eyes stay small
and separate. llm-chat can also read images *itself* when a vision volume is
attached to it directly - that path still exists and is the right one when the
image IS the subject (transcription, "read this table"). Use this app when the
picture is one input to a larger conversation, or when something other than a
chat needs to look at something.

## Routes

| route | what |
|---|---|
| `GET /` | the playground: drop, paste or pick an image, ask, watch the answer stream |
| `GET /ping` | liveness; touches no wasi-nn, never authenticated |
| `GET /health` | attached volumes, which carry a projector, the node's ggml tuning, the VRAM budget, and whether each model fits it |
| `GET /health?probe=1` | **the check worth having**: opens a real session and asks the HOST whether it can see. Config and volume can both say "vision" while the node's llama.cpp predates projector support; this is the only way to know without sending a picture. One session open, no generation |
| `GET /v1/models` | OpenAI-shaped catalog, with `enclave` extras (volume, weights/projector bytes, `vision`, `fits`) |
| `POST /v1/vision` | `{image \| images, question?, context?, model?, system?, max_tokens?, temperature?}` → `{answer, image_tokens, prompt_tokens, tokens, ms, ...}` |
| `POST /v1/chat/completions` | OpenAI-compatible, `stream: true` supported, images as content parts (three spellings accepted) |
| `POST /ask` | the playground's SSE endpoint: `{status}` lines while the session opens and the encoder runs, `{delta}` as the answer streams, then `{done, ...stats}` |

`POST /v1/vision` exists next to the OpenAI route because the caller this app
was built for is a **program**, and a program asking "what does this screenshot
say" should not have to assemble a messages array, wrap a data URI in two levels
of object, and dig the answer out of `choices[0].message.content`:

```bash
curl -sS https://<id8>.app.enclave.host/v1/vision \
  -H 'authorization: Bearer $IMAGE_READER_KEY' \
  -H 'content-type: application/json' \
  -d '{"question": "What does the error say, exactly?",
       "image": "data:image/png;base64,'"$(base64 -w0 shot.png)"'"}' \
  | jq -r .answer
```

`context` is background the answer should be read against but which is not
itself a question ("this is our checkout page", or the spec the picture is
supposed to match). It is the field that makes a **model-authored query** work:
the caller's own model knows what matters about the picture and writes the
question and context itself, so nobody has to ship a whole conversation to a
second model to get one detail checked.

Remote image URLs are **refused, not fetched**. This component does not import
`wasi:http/outgoing-handler` at all, so it has no outbound socket of any kind:
an image sent here cannot be forwarded anywhere, and a URL is something the
binary is structurally incapable of resolving. The attestation covers the
component, so that is verifiable rather than promised. Nothing is written to
disk either (the model volume is read-only and there is no other mount), so a
picture exists in enclave RAM for the length of one request.

### Errors name the thing to change

Every failure carries a machine-readable `error.code` alongside the sentence:
`no_image`, `too_many_images`, `image_too_large`, `image_undecodable`,
`url_image`, `prompt_too_long` (the caller's request), against `no_vision`,
`vision_unsupported`, `sessions_busy`, `volume_not_attached`,
`model_not_loaded`, `host_load_failed` (the deployment). A client that sees the
first group should change its request; one that sees the second should retry or
tell an operator. `/v1/vision` maps them onto 400 / 501 / 503 accordingly.

A request carrying **no image is refused by default** (`require_image`). This
app exists to look at pictures; a caller that sends none has almost certainly
lost the attachment somewhere in its own plumbing, and answering from the text
alone would hand it a confident hallucination instead of the bug report it
needs. Set `"require_image": false` to allow text-only follow-ups.

## Model volumes

`fetch-model.sh` builds the reference volume locally and doubles as the recipe
for the production Modelwrap volume (it pins every file's revision and sha256).
A vision volume is an ordinary ggml volume plus **one extra file**:

```
Qwen3VL-8B-Instruct-Q8_0.gguf        the language model (decodes text)
mmproj-Qwen3VL-8B-Instruct-F16.gguf  the vision tower (turns pixels into embeddings)
tokenizer.json                       guest-side tokenization, as for every ggml volume
```

The host finds the projector by name (anything matching `*mmproj*.gguf`) and
loads it **lazily**, on the first image the deployment is actually sent. Two
ggufs in one volume stay unambiguous because the projector is taken out of the
model pick before "exactly one gguf" is decided - the same convention on both
sides of the boundary.

The curated volume is published as
[`EnclaveHost/qwen3-vl-8b-gguf`](https://huggingface.co/EnclaveHost/qwen3-vl-8b-gguf).
A volume with no projector is not skipped from the listing: it appears with
`vision: false` and a `why_no_vision` line, because "attached but cannot see" is
a thing an operator needs to be told rather than left to infer.

## Sizing, which is where this app surprises people

Unlike the hybrid qwen3.5/3.6 models, **Qwen3-VL is dense**: all 36 layers hold
KV. At 8 KV heads x 128 head_dim that is 36,864 elements per token, so at the
fleet's 176K node window (`ENCLAVE_GGML_N_CTX` 180224):

| term | bytes |
|---|---|
| weights (Q8_0) | 8.1 GiB |
| projector (F16) + encode workspace | 1.6 GiB |
| KV cache at the node window, `q8_0` K/V | ~13.1 GiB |
| working set (compute buffers, FA workspace, CUDA context) | 1.5 GiB |
| **total** | **~24 GiB ≈ 26 GB** |

which is **gpu ≈ 0.19** of a 141 GB H200. At `f16` K/V the KV term nearly
doubles (~24.7 GiB) and the same model wants ~0.28. Do not guess: the app does
this arithmetic from the node's own environment and reports it. `GET /health`
shows `fits` per model with the shortfall spelled out, and it refuses a model it
cannot serve rather than attempting it, because a CUDA OOM inside `compute()`
calls `ggml_abort` and takes the whole tenant down with no error reaching the
guest.

**The projector is priced into that budget even though it loads lazily.** A
share that fits the language model alone would start fine, answer `/health`
fine, and then die on the first picture.

## Publish & deploy

```bash
cargo component build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/image_reader.wasm

enclave publish target/wasm32-wasip2/release/image_reader.wasm \
  --slug image-reader --config @assets/deploy-config.template.json
enclave deploy image-reader:1 --gpu 0.20 --cpu 0.02 --fund 5
```

Declare `http:8000` for the port, as everywhere on this fleet. Set an
`api_key`: the normal caller is another deployment reaching this one over the
network, which means anything else that can dial an IPv6 address can reach it
too, and inference is the expensive kind of open door. Reference it as a
**secret by name** (`"$IMAGE_READER_API_KEY"`), never as the literal - the app
config is published on-chain by CID and world-readable.

Vision needs a node whose llama.cpp toolchain includes `libmtmd`. On an older
node the volume still loads and the deployment **refuses images with that
reason** rather than silently ignoring them; `GET /health?probe=1` says which
kind of node you landed on.

## Wiring it into llm-chat

In the chat deployment's config:

```json
"vision": {
  "endpoint": "https://<image-reader-id8>.app.enclave.host",
  "api_key": "$VISION_API_KEY",
  "timeout_s": 120
}
```

llm-chat then routes an attached image here, has its own model write the
question, and folds the answer into the turn it is about to answer. See
`llm-chat/src/vision.rs` for what crosses and what does not.

**Check reachability before you wire it.** A deployment's outbound egress is
IPv6-ONLY, so app-to-app depends on the gateway answering over v6 for the
target's name (as of 2026-07-29 the wildcard `*.app.enclave.host` does publish
an AAAA, `2a01:4f9:c013:9b52::1`, alongside its A). Test it from the chat
deployment with one request that involves no inference at all:

```bash
curl -s "https://<chat-id8>.app.enclave.host/search?url=https://<reader-id8>.app.enclave.host/ping"
```

That probe dials the URL from the chat deployment's own egress identity, which
is exactly the path the vision leg uses. If it fails, the vision leg will fail
the same way and say so - it passes the platform's egress diagnosis straight
through rather than reporting "vision failed".

## Local development

Needs a wasmtime with the Enclave production patches and a `libenclave_llama.so`
built with `libmtmd` (see the platform repo's `wasm/Dockerfile.wasmtime` and
`.github/workflows/llamacpp-toolchain.yml`):

```bash
./fetch-model.sh                      # ~10 GB: weights + projector + tokenizer
cargo component build --release --target wasm32-wasip2

export LD_LIBRARY_PATH=/path/to/llamacpp-lib
export ENCLAVE_GGML_BACKEND_DIR=$LD_LIBRARY_PATH
export ENCLAVE_GGML_N_GPU_LAYERS=0    # CPU box
export ENCLAVE_GGML_N_CTX=4096
wasmtime serve --addr 127.0.0.1:8160 -S cli -S http -S nn \
  -S nn-graph=ggml::model-volume/qwen3-vl-8b-gguf \
  --dir model-volume::/models \
  --env ENCLAVE_MODELS=qwen3-vl-8b-gguf \
  --env ENCLAVE_NN_PRELOADS=qwen3-vl-8b-gguf \
  target/wasm32-wasip2/release/image_reader.wasm

curl -s 127.0.0.1:8160/health?probe=1 | jq .probe
```

CPU inference is slow by nature: the encoder pass over one 640x360 image plus a
short answer is ~25 s on a gemma-3-4b-class model, minutes on the 8B. Local runs
verify plumbing, not latency. `cargo test` runs the config, catalog, image
decoding and prompt logic natively.

**NON-CAUSAL MODELS:** gemma-style VLMs decode an image with a non-causal mask,
which requires the whole image inside one physical batch. On such a model the
node needs `ENCLAVE_GGML_N_UBATCH` >= the image's token count, or the request is
refused with a message naming that variable rather than answered from a wrongly
masked image. Qwen3-VL is causal and needs none of that, which is part of why it
is the stock model.

## Config reference

Embedded defaults in `assets/app-config.json`; every field is overridable per
deployment through `ENCLAVE_CONFIG` (the platform passes the version's
CID-verified App Config JSON), and the `models` catalog merges per volume key
and per field so a deployment adds one model without restating the rest.

| field | what |
|---|---|
| `model_volume`, `model_file`, `tokenizer_file` | which volume, and which files inside it (an absolute `tokenizer_file` reads from a sibling volume) |
| `n_layers`, `n_kv_heads`, `head_dim`, `kv_layers`, `vocab`, `eos` | geometry, from the GGUF header. `kv_layers` counts only full-attention layers on a hybrid model |
| `template` | `chatml` \| `llama3` \| `gemma` \| `phi3` \| `raw` |
| `system_prompt` | the instruction carried into every answer. **The field that most changes what this app is**: describer, transcriber, captioner, document reader. The default asks for what is visible, and for "illegible" instead of a guess |
| `default_question` | what a request with an image but no question is taken to be asking |
| `max_prompt_tokens`, `default_max_new`, `max_new_cap` | budgets |
| `temperature`, `top_p`, `top_k`, `rep_penalty`, `rep_window` | sampling defaults. Temperature is **0.2** here against a chat app's 0.7: a warm sampler on a vision model is how "the total is 47" becomes "the total is 41", and a confident wrong number is worse than a clumsy sentence |
| `repeat_guard` | identical consecutive token blocks that end a degenerate answer (default 4). Dense images - tables, legends, label lists - are exactly what provokes a loop |
| `image_tokens` | what ONE image is BUDGETED at against `max_prompt_tokens`. Admission control only; the host reports the true cost per image in every answer |
| `max_image_bytes`, `max_images` | per image, and per request |
| `require_image` | refuse a request with no image (default true) |
| `api_key` | Bearer for every route except `GET /` and `GET /ping` |

## License

[Enclave Source-Available License v1.0](../LICENSE).
