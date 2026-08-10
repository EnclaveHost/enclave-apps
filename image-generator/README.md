# image-generator: text-to-image on an Enclave GPU share

A wasm component that turns prompts into images through Enclave's wasi-nn
interface on an H200 share, serving **host-preloaded
stable-diffusion.cpp checkpoints**. Ships **no weights**: models arrive as
attached Modelwrap volumes; the ~450 KB component carries only the request
plumbing, the wasi-nn orchestration, a PNG encoder, and a self-contained
web playground at `/`. The stock catalog:

| model | what | steps | peak VRAM |
|---|---|---|---|
| `qwen-image-2512` | Qwen-Image-2512 (20B MMDiT, Apache-2.0) + lightx2v Lightning 8-step merge; the flagship: SOTA open-weights quality, legible text rendering | 8 | ~34 GB @1024px (tiled VAE) |
| `z-image-turbo` | Tongyi-MAI Z-Image-Turbo (6B, Apache-2.0); the fast one | 4–8 | ~13 GB @1024px (tiled VAE) |

Plus an **upscale option** over the same interface: Real-ESRGAN x4plus
(official BSD-3 weights, 4x) as its own tiny upscaler volume, run by
sd.cpp's ESRGAN engine behind one wasi-nn `compute()` - a `POST /upscale`
route and a per-image "upscale 4x" link in the playground.

```
prompt ──▶ load_by_name(volume) ──▶ ONE wasi-nn compute() ──▶ RGB ──▶ PNG
           (host: sd.cpp runs text encode, denoise, VAE decode) (guest)
```

The node preloads every SD volume's checkpoint components at server startup
(`-S nn-graph=sd::<dir>` + the `ENCLAVE_SD_*_FILE` envs); the guest opens
the graph by volume name and one `compute()` runs the whole pipeline
host-side. Weights never enter guest memory, so model size is bounded by
the GPU share, not wasm32's 4 GiB, and per-request traffic is the prompt
in, raw RGB out.

## Routes

| route | what |
|---|---|
| `GET /` | image playground (self-contained HTML; model dropdown when the config lists several; auto-selects the largest attached model and warms it) |
| `GET /ping` | liveness; touches no wasi-nn |
| `GET /info` | volume attachment, step/size limits, and the `models` catalog (name/limits per entry) |
| `GET /warmup?model=&size=` | load the weights + one tiny 1-step generation (at `min_size` unless `?size=` says otherwise); the playground fires it on page load |
| `GET /image?prompt=...&steps=8&seed=7&w=1024&h=1024&model=` | → `image/png` (curl-friendly) |
| `POST /generate` | `{prompt, model?, steps?, seed?, width?, height?, negative_prompt?, cfg?, ancestral?, upscale?, upscaler?, upscale_factor?}` → SSE status lines, then `{done, image: <b64 png>, model, seed, timings}`. `upscale: true` runs the result through an upscaler volume before returning (best effort: a failed upscale reports as `upscale_error` NEXT TO the finished base image) |
| `POST /upscale` | raw PNG body → ESRGAN-upscaled PNG (`curl --data-binary @in.png`); `?upscaler=` picks a catalog entry by name or volume, absent = the first attached; `?factor=` asks for a divisor of the model's native scale (see below); output geometry rides `x-width` / `x-height` / `x-upscale-factor` response headers. The playground's per-image "upscale 2x / 4x" links |
| `POST /v1/images/generations` | OpenAI-compatible: `{prompt, model?, n?, size?, seed?}` → `{created, data:[{b64_json, seed, width, height}]}`; `Authorization: Bearer` enforced when the config sets `api_key`. Extension fields `upscale: true`, `upscaler?`, `upscale_factor?` upscale each image before returning (validated up front: an API caller gets what they asked for or an error, unlike `/generate`'s best-effort) |
| `POST /v1/images/upscale` | the `/v1`-shaped, `api_key`-gated twin of `/upscale` (nonstandard: OpenAI has no upscale concept): `{image: <base64 png>, upscaler?, factor?}` → `{created, data:[{b64_json, width, height, factor}]}` |

`model` names an entry from the config's `models` catalog (matched by
display `name` **or** volume name); absent/empty means the deployment's
default: the **largest attached model** (by `max_size`, later catalog
entries win ties, so the flagship when both are attached). Sizes snap to
sd.cpp's multiple of 64 inside each model's min/max; the playground offers
512 / 1024 / 2048 as the long edge plus an aspect picker (1:1, 4:3, 3:4,
3:2, 2:3, 16:9, 9:16; each option shows the exact WxH it produces, and
ratios whose short edge would fall below the model's min are disabled).
Same seed + params → same image (per sd.cpp build; seeds are not
torch-compatible).

Both stock models are step-distilled: `cfg` defaults to 1.0 and the
useful step counts are 4–8. `negative_prompt` needs `cfg > 1` to have any
effect (that's how CFG works), which costs a second denoise pass per step
and is off-recipe for distilled checkpoints.

## Model volumes

Component-layout sdcpp volumes (diffusion gguf + LLM text-encoder gguf +
VAE), pinned in `fetch-model.sh` (revision + sha256 per file; the volume
recipes):

- **`z-image-turbo`**, the curated `EnclaveHost/z-image-turbo-sd@07cb261e`
  volume: Z-Image 6B Q8_0 + Qwen3-4B Q8_0 (sd.cpp's `--llm` text-encoder
  slot) + FLUX AE, ~11 GB.
- **`qwen-image-2512`**: Qwen-Image-2512 20B Q8_0 **with the lightx2v
  Lightning 8-step LoRA merged** + Qwen2.5-VL-7B Q8_0 text encoder + the
  Qwen-Image 16-ch VAE, ~29 GB. The merged `diffusion.gguf` is produced by
  `tools/merge-lightning.sh` (streaming LoRA merge, then Q8_0 quantization
  by the SAME `stable-diffusion.cpp@b5d81200` revision the fleet serves
  with); the other components fetch directly. Undistilled base output at
  8 steps/cfg 1 is mush; the merge is what makes the flagship interactive.

Component filenames are **generic** (`diffusion.gguf` / `llm.gguf` /
`vae.safetensors`): the platform's `ENCLAVE_SD_*_FILE` envs are node-global
and validate against every SD volume on the node, so volumes must share
relative names to coexist. Both curated repos carry the generic names
(z-image at rev `d7aeefc3`, a byte-identical rename of the original
`07cb261e`); the production volumes must be **re-wrapped from these
revisions** before both models can preload on one node.

### The upscaler volume

- **`realesrgan-x4plus`**, to be wrapped as `EnclaveHost/realesrgan-x4plus-sd`:
  Real-ESRGAN x4plus (xinntao, official BSD-3-Clause weights), the stock
  upscaler behind `/upscale`. RRDBNet 23-block 4x, one ~64 MB
  `upscaler.safetensors`, built by `fetch-model.sh realesrgan-x4plus`: the
  pinned release `.pth` converts torch-free through
  `tools/convert_esrgan.py` (deterministic, output sha256 pinned in the
  recipe).

  Why this model: sd.cpp's upscaler engine runs the RRDBNet/ESRGAN
  architecture, and within it the official Real-ESRGAN x4plus is the
  strongest cleanly-licensed general model. The popular community finetunes
  that beat it (4x-UltraSharp, UltraSharpV2) are CC-BY-NC (non-commercial),
  and UltraSharpV2 is DAT2 anyway; transformer/diffusion upscalers
  (DAT/HAT/SeedVR2) would need new architectures in the substrate. The
  anime 6B variant and Nomos8kSC (CC-BY-4.0) are drop-in alternatives: any
  RRDBNet `.pth` through `convert_esrgan.py` makes a volume, scale (1/2/4)
  read from the weights.

  The `upscaler.safetensors` name is the node's `ENCLAVE_SD_UPSCALE_FILE`,
  and its **presence is what marks an upscaler volume** (the platform
  serves such a volume as an upscale graph, not a checkpoint), so the file
  must never appear inside a generation volume.

### Serving several models at once

The config's `models` catalog is a **map keyed by volume name**. The key is
the volume the platform mounts at `/models/<key>`; the entry sets the
display `name` (what the UI shows and a request's `model` selects) plus any
field overrides on the top-level template. That map **is** the
volume→model mapping: explicit, no name matching. The embedded catalog:

```json
"models": {
  "z-image-turbo-sd":   { "name": "z-image-turbo" },
  "qwen-image-2512-sd": { "name": "qwen-image-2512", "default_steps": 8, "min_size": 512 }
}
```

So a volume wrapped as `qwen-image-2512-sd` is served as `qwen-image-2512`
because the config says so; the `-sd` suffix is just part of the key you
write. Each entry's effective config is the top-level fields with the
entry's overlaid, `model_volume` pinned to the key. A deployment attaches
the volumes by their real (key) names:

```json
{ "volumes": ["z-image-turbo-sd", "qwen-image-2512-sd"] }
```

An entry whose volume isn't mounted shows in the UI as "volume missing" and
never wins the default; the rest keep working. `ENCLAVE_CONFIG`'s `models`
merges **per key** (and per field within an entry), so a deployment can add
one model or tweak one field without restating the catalog:

```json
{ "models": { "flux2-klein-sd": { "name": "flux2-klein", "default_steps": 4 } } }
```

Written map order is the UI/catalog order and the default's tie-break; drop
the `models` key entirely to serve just the top-level `name`/`model_volume`
as a single model.

`upscalers` is the same shape for the upscale option - a map keyed by
volume, small self-contained entries (`name`, `factor` default 4,
`max_input` default 2048 - the input long-edge cap; 2048 at 4x is an
8192 px output, ~200 MB of RGB through the guest). The embedded catalog:

```json
"upscalers": {
  "realesrgan-x4plus-sd": { "name": "realesrgan-x4plus" }
}
```

Requests select by `name` or volume; absent means the first attached entry.
It merges per key under `ENCLAVE_CONFIG` like `models`, an entry whose
volume isn't attached just hides the playground's upscale links, and
dropping the key disables the option entirely.

Sub-native factors ride the native pass: `factor` (the `?factor=` query, or
`upscale_factor` on `/generate`) accepts any divisor of the model's native
scale. The model always runs at native scale and the output box-averages
down by exact integer blocks - supersampling, which cancels upscaler
hallucination noise instead of resampling it, so a `factor=2` from the 4x
model meets or beats a native 2x model (chaining a 2x model UP would do the
opposite: each pass amplifies the previous one's artifacts). `factor=1` is
a same-size cleanup pass. The playground offers `native/2` and `native`.

## Platform pieces

See `wasm/sd-shim/README.md` in the platform repo: `wasmtime-nn-sdcpp.patch`
(after `wasmtime-nn-ggml.patch`), `libenclave_sd.so` +
`libstable-diffusion.so` (pinned `leejet/stable-diffusion.cpp@b5d81200`),
the volume names listed in the enclave's `MODEL_VOLUMES_SD` env, and the
component-file envs. Node envs that matter here:

```yaml
- MODEL_VOLUMES_SD: "z-image-turbo,qwen-image-2512,realesrgan-x4plus"
- ENCLAVE_SD_DIFFUSION_FILE: "diffusion.gguf"      # generic names: see above
- ENCLAVE_SD_LLM_FILE: "llm.gguf"
- ENCLAVE_SD_VAE_FILE: "vae.safetensors"
- ENCLAVE_SD_UPSCALE_FILE: "upscaler.safetensors"  # marks upscaler volumes
- ENCLAVE_SD_VAE_TILING: "64:0.25"   # 512px tiles: <=512 single-tile, 3x3 @1024
- ENCLAVE_SD_FLASH_ATTN: "1"         # REQUIRED: unfused attention fails >1024px
# ENCLAVE_SD_UPSCALE_TILE defaults to 256 (px, input-side); the upscale
# path needs no flash attention and no VAE - it is a separate tiny engine
# NO ENCLAVE_SD_WTYPE: the volumes ship Q8_0 - forcing f16 UP-converts and
# doubles VRAM
```

The upscale verb needs a toolchain tarball whose `libenclave_sd` carries the
`esd_load_upscaler`/`esd_upscale` entry points (dlsym-gated: an older
tarball keeps serving txt2img, upscaler volumes fail preload with a
"rebuild the toolchain" error).

**Fleet provisioning**: deployments can only attach volumes the enclave
carries: Modelwrap entries in `enclaves/gpu/tinfoil-config.yml`:

```yaml
models:
  - name: "z-image-turbo"        # generic component names (rename of 07cb261e)
    repo: "EnclaveHost/z-image-turbo-sd@d7aeefc33a479d183cedf4dce3c294ea71db29ab"
  - name: "qwen-image-2512"      # from tools/merge-lightning.sh output
    repo: "EnclaveHost/qwen-image-2512-sd@a82dcb53ef8ff3c675dd0b636788ebb332fa973f"
  - name: "realesrgan-x4plus"    # upscaler; from fetch-model.sh realesrgan-x4plus
    repo: "EnclaveHost/realesrgan-x4plus-sd@f7b73b7949e01f9129c9be9f27337b87dceee51c"
```

## Publish & deploy

```bash
cargo component build --release --target wasm32-wasip2
# → target/wasm32-wasip1/release/image_generator.wasm (~450 KB)

enclave publish target/wasm32-wasip1/release/image_generator.wasm \
  --slug image-generator \
  --config '{"volumes":["z-image-turbo-sd","qwen-image-2512-sd","realesrgan-x4plus-sd"]}'
enclave deploy image-generator:1 --gpu 0.36 --cpu 0.02 --fund 5
```

Attach volumes by the **exact names the enclave carries** (the `-sd`
repo-slug names; `enclave apps` / the console volume picker lists them);
vmmanager matches attach names literally. Those names are the catalog keys,
so each mounts at `/models/<key>` and serves under its entry's display name
(`z-image-turbo`, `qwen-image-2512`); the UI names stay clean.

VRAM budget (the dial that matters): z-image ~13 GB peak, qwen-image-2512
~34 GB peak (Q8 weights 20 GB + Qwen2.5-VL 8 GB + compute with tiled VAE),
plus headroom: **both resident wants ~50 GB ≈ gpu 0.36** on the 141 GB
H200 (2048px headroom: 0.40); qwen alone ≈ 0.28; z-image alone ≈ 0.10.
The upscaler is noise in this budget: ~64 MB of weights plus a sub-GB
tiled compute buffer (estimate; input tiles are 256px by default).
Suggested published minimums: **VRAM 48 GB · GPU 49 TFLOPS · RAM 512 MB ·
CPU 10 GFLOPS**; the floor must make the DEFAULT config (both volumes)
work, because models are resident **first-come-first-served** within a
share: warm order decides what fits, and the first failed CUDA alloc
aborts the process. A z-image-only republish can floor at 14 GB.

There is **no silent GPU→CPU fallback** (`default_target` is `gpu`): image
generation on CPU is minutes, not seconds. Failures are loud by design;
pass `?target=cpu` / `"target":"cpu"` explicitly on dev boxes.

## Local development

Needs a wasmtime with the Enclave production patches and the sdcpp backend
(the `nn-ggml` + `nn-sdcpp` patch chain plus `libenclave_sd.so` /
`libstable-diffusion.so` on the library path; see the platform repo's
`wasm/Dockerfile.wasmtime` for the exact recipe):

```bash
./fetch-model.sh z-image-turbo       # ~11 GB
cargo component build --release --target wasm32-wasip2

ENCLAVE_SD_USE_GPU=0 \
ENCLAVE_SD_DIFFUSION_FILE=diffusion.gguf \
ENCLAVE_SD_LLM_FILE=llm.gguf \
ENCLAVE_SD_VAE_FILE=vae.safetensors \
wasmtime serve -Scli -Shttp -Snn \
  -S nn-graph=sd::model-volume/z-image-turbo-sd \
  --dir model-volume/z-image-turbo-sd::/models/z-image-turbo-sd \
  --env ENCLAVE_MODELS=z-image-turbo-sd \
  --env ENCLAVE_CONFIG='{"default_target":"cpu"}' \
  --addr 127.0.0.1:8080 target/wasm32-wasip1/release/image_generator.wasm

curl "127.0.0.1:8080/image?prompt=a+red+barn+at+sunset&steps=4&w=512&h=512&seed=7" -o out.png
```

The upscaler serves the same way - build the volume with
`./fetch-model.sh realesrgan-x4plus` (64 MB, no GPU needed), then add
`ENCLAVE_SD_UPSCALE_FILE=upscaler.safetensors`, a second
`-S nn-graph=sd::model-volume/realesrgan-x4plus-sd`, its `--dir` mount, and
the volume in `ENCLAVE_MODELS`:

```bash
curl -X POST --data-binary @out.png 127.0.0.1:8080/upscale -o out-4x.png
```

(96px to 384px runs ~4 s on CPU; generation stays the slow part.)

CPU generation is slow by nature (z-image 512px 4-step ≈ minutes on 16
cores); local runs verify plumbing, not latency. `cargo test` runs the
config/catalog logic natively.

## Config reference

Embedded defaults in `assets/app-config.json`; every field overridable per
deployment through `ENCLAVE_CONFIG` (the platform passes the version's
CID-verified App Config JSON). Highlights: `model_volume`, `default_steps`,
`max_steps`, `default_size`/`min_size`/`max_size` (multiples of 64),
`default_target`, `cfg_scale` (1.0 for distilled models),
`sample_method`/`scheduler` (sd.cpp sampler names; unset = euler_a/euler by
the request's `ancestral` flag), `max_images` (n cap for the OpenAI route),
`api_key`, `models` (the catalog: a map keyed by volume name, each value
a field-overlay carrying the display `name`; requests select by `name` or
volume, absent `model` = the largest attached entry), and `upscalers` (the
upscale catalog: same map shape, entries `name`/`factor`/`max_input`;
absent `upscaler` = the first attached entry).
