# image-generator HTTP API reference

The service-provider interface of the image-generator app: every route, every
field a client can send, and everything that comes back, as implemented in
`src/app.rs` (accurate as of **0.5.0**). Two surfaces share one component: an
OpenAI-compatible `/v1` pair for SDK clients, and the app's own routes
(`/generate`, `/upscale`, `/image`, `/info`, `/warmup`) that the built-in
playground uses and that curl reaches without a JSON envelope.

```
base URL:  https://<id8>.app.enclave.host      (or the deployment's custom domain)
           ├── POST /v1/images/generations     OpenAI-compatible text to image
           ├── POST /v1/images/upscale         ESRGAN upscaling, /v1-shaped (nonstandard)
           ├── POST /generate                  text to image over SSE, status while it runs
           ├── POST /upscale                   raw PNG in, upscaled PNG out
           ├── GET  /image                     one GET, one PNG
           ├── GET  /info                      the catalog and its limits
           ├── GET  /warmup                    put weights in VRAM before the first prompt
           ├── GET  /ping                      liveness
           └── GET  /                          the playground
```

Everything here is served **by the enclave holding the weights**. The prompt
is parsed inside the CVM, the model runs on that deployment's GPU share, and
the pixels come back on the same connection. No third party sees the prompt,
and the app **cannot** call one: its world (`wit/world.wit`) imports wasi:nn
and exports the HTTP handler, with no outbound HTTP interface to reach for.
The app ships **no weights**; what it can serve is whatever model volumes the
deployment attached, which `GET /info` reports.

---

## Authentication

Deployment policy, and only on `/v1`: when the config sets a top-level
`api_key`, `POST /v1/images/generations` and `POST /v1/images/upscale` require

```
Authorization: Bearer <api_key>
```

and answer `401 {"error":{"message":"missing or invalid API key","type":"invalid_request_error"}}`
without it. **No key configured = open.** The gate reads the **default
model's** `api_key` before the body is parsed, so a per-model key in a catalog
entry does not change who may call `/v1`; put the key at the top level.

The app's own routes (`/`, `/generate`, `/image`, `/upscale`, `/info`,
`/warmup`, `/ping`) are **never** key-gated: the playground has to work
in a browser that holds no secret. Gate those with a private deployment when
that is the intent.

---

## Models and the catalog

The config's `models` catalog is a map keyed by **volume name**; each entry
carries the display `name` a request selects, overlaid on the top-level
template (see `src/config.rs` and the README). What that means at the API:

- A request's `model` matches either the display `name` or the volume name,
  so `"qwen-image-2512"` and `"qwen-image-2512-sd"` both resolve.
- Absent or empty `model` means the deployment's default: the **largest
  attached model** by `max_size`, later catalog entries winning ties. With
  nothing attached at all, the same rule runs over the whole catalog, so the
  error you get names a real model instead of failing to resolve.
- An unknown name is a **400** listing what is available.
- Resolution does **not** check attachment. Naming a configured but unattached
  model succeeds here and fails at generation time with the `load_by_name`
  error, which names the volume and what the host needed to preload.

Every model serves through the host's stable-diffusion.cpp wasi-nn backend.
Weights never enter guest memory: one `compute()` runs text encode, denoise
and VAE decode host-side, and the component sees the prompt going in and raw
RGB coming back.

---

## Generation parameters

One parameter set, shared by `/generate`, `/v1/images/generations` and (under
shorter query names) `GET /image`. Request value, then the model's config
default, then the clamp:

| field | type | meaning |
|---|---|---|
| `prompt` | string | **required**; trimmed, and an empty one is a 400. |
| `model` | string | a display name or volume from `/info`. Absent = the largest attached model. |
| `negative_prompt` | string | what to steer away from. Needs `cfg > 1` to do anything, which is off-recipe for the distilled stock models. |
| `steps` | int | denoise steps. Default `default_steps`, clamped `1..=max_steps`. |
| `seed` | int | default is the wall clock in milliseconds, so absent means "different every time". Same seed and parameters reproduce an image within one sd.cpp build; seeds are not torch-compatible. |
| `width`, `height` | int | pixels. Each is clamped into the model's `min_size..=max_size`, then rounded **down** to a multiple of 64 (sd.cpp's requirement). Default `default_size`. |
| `size` | string | OpenAI's `"WxH"` spelling (`x` or `X`). Present, it **overrides** `width`/`height` and takes the same clamp and snap. Unparseable is a 400. |
| `cfg` | float | classifier-free guidance. Default `cfg_scale` (1.0 for the distilled stock models), clamped `0.0..=15.0`. |
| `ancestral` | bool | default true. Picks `euler_a` over `euler`, and only when the config leaves `sample_method` unset. |
| `target` | string | `"gpu"` or `"cpu"`. **Validated, then ignored**: a bad string is a 400, but placement is the node's, decided by which volumes it preloaded. The one place it does something is the `/warmup` ladder (below). |

The aspect ratio is whatever `width`/`height` (or `size`) say, inside the
model's limits: the shape is yours to pick, and the clamp is per side, so
`"1792x1024"` against a 1024 `max_size` model becomes 1024x1024 rather than an
error. Ask `/info` for each model's `min_size`, `default_size`, `max_size` and
the 64 px step if the client wants to offer only shapes that survive the snap.

---

## Upscaling

An upscaler is a separate small volume (Real-ESRGAN x4plus in the stock
catalog) served through the same backend, and it appears three ways: as its
own routes (`POST /upscale`, `POST /v1/images/upscale`), and as an option on
generation (`upscale: true` on `/generate` and `/v1/images/generations`).
Shared semantics:

- **Selection.** `upscaler` (body field, or `?upscaler=` on `/upscale`)
  matches a display name or a volume name. Absent means the **first attached**
  catalog entry. With none attached the error says which are configured and
  that one needs ticking in the volume picker; with none configured it says
  the deployment has no upscalers at all. Both are 400s.
- **Factor.** `factor` (`upscale_factor` on the generation routes,
  `?factor=` on `/upscale`) must be a **divisor of the upscaler's native
  scale**: 4, 2 or 1 on the stock 4x model. Absent means native. The model
  always runs at native scale and a sub-native factor comes from an exact
  integer box average of that output, which is supersampling: it cancels the
  upscaler's hallucination noise rather than resampling it, so `factor=2` off
  the 4x model meets or beats a native 2x model. `factor=1` is a same-size
  cleanup pass.
- **Aspect ratio is preserved by construction.** ESRGAN scales both sides by
  the same integer and the box average divides both by the same integer, so
  there is no shape argument and none is needed.
- **Input limits.** The upscaler's `max_input` (default 2048) caps the input's
  long edge, checked against the PNG header before any buffer is allocated, so
  an oversized or decompression-bomb upload fails on its header. The floor is
  16 px a side. 2048 at 4x is an 8192 px output, roughly 200 MB of RGB moving
  through the component, which is why the cap is where it is.
- **Input format is PNG.** 8-bit RGB, RGBA (alpha dropped), grayscale and
  gray+alpha are accepted; 16-bit and palette images normalize to 8-bit colour
  first. Output is always 8-bit RGB PNG, no alpha.
- **Where the check happens.** Everything checkable is checked before GPU
  time: catalog entry, factor divisibility, input geometry. The factor is
  re-checked afterwards against the real output geometry, because the weights
  are the truth about the native scale; a mismatch discovered there is a 500,
  not a 400.

---

## POST /v1/images/generations

OpenAI's images API, plus the extensions the schema has no words for. Request
body cap: **64 KB** (this is a prompt, not an upload).

```json
{ "prompt": "a red barn at sunset, wide", "model": "z-image-turbo",
  "n": 1, "size": "1024x576", "seed": 7, "steps": 4 }
```

| field | notes |
|---|---|
| `prompt`, `model`, `size` | OpenAI. See [Generation parameters](#generation-parameters). |
| `n` | how many images. Clamped `1..=max_images` (default cap 4). Image *i* uses `seed + i`, so one request gives a comparable set rather than n identical pictures. |
| `response_format` | `"b64_json"` (the default, and what you always get). `"url"` is a **400**: an ephemeral enclave has nowhere durable to host a file. Any other value is ignored. |
| `seed`, `steps`, `cfg`, `negative_prompt`, `ancestral`, `width`, `height`, `target` | extensions; same meaning as everywhere else. |
| `upscale`, `upscaler`, `upscale_factor` | extensions: run each generated image through an upscaler before returning it. See [Upscaling](#upscaling). |

Response:

```json
{
  "created": 1770000000,
  "data": [
    { "b64_json": "iVBORw0KGgo…", "seed": 7, "width": 1024, "height": 576 }
  ]
}
```

`b64_json` is a PNG. `seed`, `width` and `height` are extensions on the data
entry, and `width`/`height` are the **final** geometry, after the snap and
after any upscale, so a client never has to decode the image to lay it out.

Unlike `/generate`, this route is **all or nothing**: with `upscale: true`,
everything checkable is validated up front and a failure at any stage is an
error response, because an API caller asked for an upscaled image and half of
one is not it. A failure partway through a multi-image request answers
`500 "image 2/4: <reason>"`.

## POST /v1/images/upscale

Nonstandard (OpenAI has no upscale concept) but `/v1`-shaped and key-gated
like its neighbour: standalone upscaling for JSON clients. The binary-friendly
twin is `POST /upscale`. Request body cap: **44 MB**, which is a 32 MB PNG
base64-encoded plus scaffolding.

```json
{ "image": "iVBORw0KGgo…", "upscaler": "realesrgan-x4plus", "factor": 2 }
```

`image` is a base64 PNG; a `data:image/png;base64,…` wrapper is tolerated and
the payload after `;base64,` is what gets decoded. `upscaler` and `factor` are
optional, with the defaults from [Upscaling](#upscaling).

```json
{
  "created": 1770000000,
  "data": [ { "b64_json": "iVBORw0KGgo…", "width": 2048, "height": 1152, "factor": 2 } ]
}
```

`factor` on the response is the **achieved** ratio (output width over input
width), not what was asked for, so a client can log what it actually got.

---

## POST /generate

The playground's route, and the right one for any client that wants to show
progress: `Content-Type: text/event-stream`, one JSON object per `data:` line.
Same body as `/v1/images/generations`, except that `n` and `response_format`
are parsed and ignored (one image per request), and it is **not** key-gated.
Request body cap: **64 KB**.

```
:<4000 spaces>                                    ← padding comment, see below
data: {"status":"opening the preloaded model"}
data: {"status":"generating 1024x576 in 4 steps (host-side pipeline; blocks until done)"}
data: {"done":true,"image":"iVBORw0KGgo…","model":"z-image-turbo","width":1024,
       "height":576,"seed":7,"steps":4,
       "timings":{"load_ms":12,"gen_ms":9310,"png_ms":48,"upscale_ms":0}}
```

The contract, in order:

1. **A ~4 KB SSE comment goes out first.** TLS shims and proxies between the
   enclave and the browser buffer small chunks, and the generation blocks
   inside one long `compute()` with nothing further to flush them. Without the
   padding the early status lines sit invisible in a buffer and the client
   watches a silent "starting" for the whole run. Conforming SSE parsers
   ignore comment lines, so it costs a client nothing.
2. **`{"status": "…"}` events** carry human-readable progress: opening the
   model, generating (with the geometry and step count), and on an upscale
   pass, opening the upscaler and upscaling. They are also the keepalive
   through the long first-load silence.
3. **One terminal event.** Either `{"done": true, …}` as above, or
   `{"error": "<reason>"}`. Note the shape: this is a **bare string field**,
   not the JSON error envelope the buffered routes use, and it arrives on a
   200 because the headers left before the model did. There is no `[DONE]`
   sentinel.
4. Writing to a disconnected client aborts the run at the next status point,
   which is what stops a hung browser tab from paying for a full generation.

With `upscale: true`, this route is **best effort**, the opposite of the `/v1`
policy: a failed upscale reports beside the finished base image rather than
discarding a multi-second generation over a two-second step. The done event
then carries one of:

```json
"upscaled": { "upscaler": "realesrgan-x4plus", "from": [1024, 576] }
"upscale_error": "1024x1024 exceeds upscaler 'realesrgan-x4plus' max_input 2048px"
```

When `upscaled` is present, `width`/`height` (and `timings.upscale_ms`) are
the upscaled result's; when `upscale_error` is, the image is the base one at
its original size.

## POST /upscale

Raw PNG in, raw PNG out, for clients that would rather not base64 anything:

```bash
curl -X POST --data-binary @in.png \
  "https://<id8>.app.enclave.host/upscale?upscaler=realesrgan-x4plus&factor=2" -o out.png
```

Query parameters `?upscaler=` and `?factor=` do what the body fields do
elsewhere. Body cap: **32 MB**; an empty body is a 400. The response is
`image/png` plus three headers, so the geometry is readable without decoding:

| header | value |
|---|---|
| `x-width` | output width in pixels |
| `x-height` | output height |
| `x-upscale-factor` | achieved ratio (output width over input width) |

Not key-gated, and nothing streams here: it is one request, one image.

## GET /image

One GET, one PNG, for curl and for `<img src>`:

```
GET /image?prompt=a+red+barn+at+sunset&steps=4&seed=7&w=512&h=512&model=z-image-turbo
```

Query names are shorter than the JSON fields: `w`/`h` for width/height and
`negative` for `negative_prompt`; `prompt`, `model`, `steps`, `seed`, `cfg`,
`ancestral` and `target` keep their names. There is **no `size` and no
upscale** on this route: it is the plain one. Answers `image/png`, or a JSON
error with a real status code, since nothing has been sent when generation
fails. A generation that outlives the platform's proxy budget is the risk
here (see [Timing](#timing-and-the-proxy-budget)); `/generate` is the answer.

---

## GET /info

Everything a client needs to build a request, resolved for this deployment:

```json
{
  "name": "qwen-image-2512",
  "volume": "qwen-image-2512-sd",
  "volume_attached": true,
  "attached": ["z-image-turbo-sd", "qwen-image-2512-sd", "realesrgan-x4plus-sd"],
  "default_steps": 8, "max_steps": 8,
  "default_size": 1024, "min_size": 512, "max_size": 1024,
  "default_target": "gpu",
  "backend": "sdcpp",
  "models": [
    { "name": "z-image-turbo", "volume": "z-image-turbo-sd", "backend": "sdcpp",
      "attached": true, "default_steps": 4, "max_steps": 8, "default_size": 1024,
      "min_size": 256, "max_size": 1024, "size_step": 64 }
  ],
  "upscalers": [
    { "name": "realesrgan-x4plus", "volume": "realesrgan-x4plus-sd",
      "attached": true, "factor": 4, "max_input": 2048 }
  ],
  "default_upscaler": "realesrgan-x4plus"
}
```

- The **top-level** fields mirror the default model, which is the shape this
  route had before there was a catalog; old clients keep working.
- `models` is the catalog in config order, each entry with its own limits and
  whether its volume is mounted. `size_step` is 64 everywhere, so a UI can
  build its size list without knowing about sd.cpp.
- `attached` (top level) is the deployment's volume list as the platform
  reported it, upscaler volumes included; `volume_attached` and each entry's
  `attached` are the per-model answers.
- `upscalers` is empty when none are configured, and `default_upscaler` is
  `null` whenever none is **attached** (a configured entry still appears in
  the array with `attached: false`). That pair is what a client should read
  before offering an upscale button.

Open, cheap, and touches no wasi-nn.

## GET /warmup

Weights and kernels into device memory before the first real prompt, by
running a tiny 1-step generation. Two modes.

**`?model=<name>`** warms that one entry:

```json
{ "ok": true, "model": "z-image-turbo", "volume": "z-image-turbo-sd",
  "target": "gpu", "size": 256, "total_ms": 41230,
  "timings": { "load_ms": 39900, "gen_ms": 1330, "png_ms": 0 } }
```

`?size=` overrides the warm size (default `min_size`, snapped like any other
size). Failure is a 500 carrying the backend's own reason.

**Bare `/warmup`** is the **ladder**, which is what the boot warmup and the
playground's page load call: every **attached** catalog model, smallest volume
first, warmed one at a time.

```json
{
  "ok": true,
  "ladder": [
    { "model": "z-image-turbo", "volume": "z-image-turbo-sd", "bytes": 11000000000,
      "ok": true, "size": 256, "total_ms": 41230, "load_ms": 39900, "gen_ms": 1330 },
    { "model": "qwen-image-2512", "volume": "qwen-image-2512-sd", "bytes": 29000000000,
      "ok": false, "skipped": true,
      "error": "27.0 GB of weights cannot fit the deployment's 20.0 GB VRAM budget …" }
  ],
  "default": "z-image-turbo",
  "upscaler": { "upscaler": "realesrgan-x4plus", "volume": "realesrgan-x4plus-sd",
                "ok": true, "factor": 4, "total_ms": 890 }
}
```

- Smallest first is deliberate: residency inside a GPU share is
  first-come-first-served, so the models most likely to fit become resident
  before a larger sibling claims (or fails to claim) the rest.
- A model that does not fit is reported and skipped, never fatal. `default` is
  the largest model that **did** warm, which is what a picker should select;
  entries with `ok: false` are what it should disable.
- `skipped: true` means the deployment's VRAM budget (`ENCLAVE_VRAM_BYTES`,
  from the GPU share) says the weights certainly cannot fit, so no multi-GB
  load was started to prove it. The accounting is cumulative and weights-only,
  so borderline models still get an honest probe. `?target=cpu` disables this
  gate, which is the one thing `target` actually changes.
- `upscaler` sits **outside** the ladder and never affects `ok`: generation is
  the contract, upscaling is the option. It is probed with a 64x64 image, so
  its cost is the load.
- Status: **200** while at least one model warmed, **500** when none did (with
  nothing attached, the response is the volume-not-attached error that tells
  an operator what to attach).

Slow by design when cold, and repeat calls coalesce on the host's model cache.
Defaults to GPU: warmup exists to put weights in VRAM, so a failed GPU should
read as a failed warmup. Pass `?target=cpu` on a dev box.

## GET /ping

```json
{ "ok": true, "pong": true, "t": 1770000000123 }
```

Liveness only: it touches no wasi-nn, so it answers while a generation is
running and says nothing about whether a model is loaded. `GET /info` for
that, or `/warmup` to make it true.

## GET /

The playground: one self-contained HTML page, no external assets. Model
dropdown when the catalog lists several, an aspect picker that shows the exact
WxH each ratio produces (disabling ratios whose short edge would fall under
the model's `min_size`), and per-image upscale links when an upscaler volume
is attached.

---

## Errors

Every buffered route answers one shape:

```json
{ "error": { "message": "unknown model 'nope' (available: z-image-turbo, qwen-image-2512)",
             "type": "invalid_request_error" } }
```

`type` is always the literal `invalid_request_error`, on 4xx and 5xx alike, so
**switch on the HTTP status, not on `type`**. There are no error codes; the
message is the contract, and it is written to say what to do next.

| status | when |
|---|---|
| **400** | bad JSON, body over the route's cap, empty prompt, unparseable `size`, unknown model or upscaler, no upscaler attached, a `factor` that does not divide the native scale, an input image too large or under 16 px, an undecodable PNG, `response_format: "url"`, a `target` that is neither gpu nor cpu |
| **401** | `/v1` with a configured `api_key` and a missing or wrong bearer token |
| **404** | any other route or method; the message lists every route this app serves |
| **500** | configuration errors, the backend refusing to load a volume, a generation or upscale failure, a factor that survived the pre-check but not the real output geometry, PNG encoding |

`POST /generate` is the exception, because its headers leave before the model
does: after the stream opens, a failure arrives as `data: {"error": "…"}` on
the 200. Only the parse and resolve steps, which run before the stream opens,
can still answer a status code there.

---

## Timing and the proxy budget

Generation is seconds to minutes, and a cold model is dominated by the load:
tens of seconds to bring weights into VRAM the first time. That matters for
route choice, because the platform's proxy path (`*.app.enclave.host`) has a
response budget of roughly **three minutes with no bytes flowing**, which this
app cannot influence.

- `POST /generate` streams status lines, so a long run keeps the connection
  warm. Use it for anything interactive.
- `GET /image` and the `/v1` routes are **buffered**: nothing is written until
  the image is done. They are fine for a warmed model at ordinary sizes, and
  they are the wrong choice for a cold flagship at 1024 px with `n: 4` and an
  upscale on top.
- Call `GET /warmup` after a deployment starts (the boot warmup does this, and
  the playground repeats it on load). It converts the first user's minute of
  loading into nobody's.
- `timings` on `/generate` and `/warmup` split `load_ms` (weights, cache miss
  or hit), `gen_ms` (the denoise loop), `png_ms` (encoding) and `upscale_ms`,
  which is usually enough to tell "the model is cold" from "the model is slow".

---

## Limits

| limit | value | where |
|---|---|---|
| JSON body | 64 KB | `/generate`, `/v1/images/generations` |
| PNG body | 32 MB | `/upscale` |
| JSON body | 44 MB | `/v1/images/upscale` (a 32 MB PNG in base64) |
| image sides | `min_size`..`max_size`, multiples of 64 | every generation route |
| images per request | `max_images` (default 4) | `n` on `/v1/images/generations` |
| steps | `1..=max_steps` | every generation route |
| upscaler input | `max_input` long edge (default 2048), 16 px floor | every upscale route |

---

## Examples

```bash
# OpenAI-compatible, wide aspect, upscaled 2x on the way out
curl https://<id8>.app.enclave.host/v1/images/generations \
  -H 'Authorization: Bearer <key>' -H 'Content-Type: application/json' \
  -d '{"prompt":"a red barn at sunset","size":"1024x576",
       "upscale":true,"upscale_factor":2}' \
  | jq -r '.data[0].b64_json' | base64 -d > barn.png
```

```bash
# the app's own routes: watch it work, then upscale the result as raw bytes
curl -N https://<id8>.app.enclave.host/generate \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"a lighthouse on a cliff at dawn","steps":4,"seed":7}'

curl -X POST --data-binary @barn.png \
  'https://<id8>.app.enclave.host/upscale?factor=4' -o barn-4x.png
```

```python
import base64
from openai import OpenAI
client = OpenAI(base_url="https://<id8>.app.enclave.host/v1",
                api_key="<key or any string>")

img = client.images.generate(
    model="z-image-turbo",             # or omit: largest attached wins
    prompt="a red barn at sunset",
    n=1,
    size="1024x576",                   # clamped into the model's min/max, snapped to /64
    response_format="b64_json",        # the only format here
    extra_body={"steps": 4, "seed": 7, "upscale": True},   # extensions
)
open("barn.png", "wb").write(base64.b64decode(img.data[0].b64_json))
```

`api_key` may be any non-empty string when the deployment configured none. The
SDK ignores the extension fields on the response entries (`seed`, `width`,
`height`) rather than tripping over them.

---

## Wiring it into a chat app

The [eyesoff-ai](../../eyesoff-ai/) chat app calls this one as a plain tool,
which is how a conversation gets pictures without either app knowing anything
special about the other. Both entries are HTTP tools in that deployment's
registry, documented in full in
[eyesoff-ai/docs/openai-api.md](../../eyesoff-ai/docs/openai-api.md) under
"Picture tools", and reproduced in its config template:

```json
{ "name": "generate_image", "url": "${IMAGE_ENDPOINT}/v1/images/generations",
  "method": "POST", "headers": { "authorization": "Bearer $IMAGE_API_KEY" },
  "body": { "prompt": "$prompt", "n": 1, "size": "$size" },
  "timeout_s": 180, "result": { "image": "data.0.b64_json" } }

{ "name": "upscale_image", "url": "https://<id8>.app.enclave.host/v1/images/upscale",
  "method": "POST", "body": { "image": "$image", "factor": "$factor" },
  "timeout_s": 120, "max_bytes": 67108864,
  "result": { "image": "data.0.b64_json" } }
```

Two things about this app make those entries work: `size` and `factor` are
optional, so a model that omits them gets `default_size` and the native scale
rather than an error, and both routes answer the same
`data[0].b64_json` shape, so one `result` mapping reads either. Keep the
`size` enum on the chat side inside this deployment's `min_size`/`max_size`,
or a shape the model picks will quietly snap to something else.

---

## Compatibility notes

- **b64 only.** `response_format: "url"` is refused rather than faked. An
  enclave that stops when it runs out of funds is not a file host.
- **No `/v1/models`, no edits, no variations, no masks.** This is a generation
  and upscale service; a client that lists models should call `GET /info`.
- **OpenAI fields with no meaning here are ignored**, not rejected (`quality`,
  `style`, `user`, and friends). `response_format: "url"` is the single
  exception, because silently returning something else would be worse.
- **`n` is capped by config**, and the images are seed-adjacent rather than
  independent draws.
- **No CORS headers.** A browser page on another origin cannot read these
  responses; the playground is same-origin, and server-side callers are
  unaffected.
- **PNG in, PNG out.** No JPEG or WebP on either side, and output carries no
  alpha channel.
- **`target` does not choose a device.** Placement follows the node's preload.
  There is no silent GPU to CPU fallback: generation on CPU is minutes, and
  failing loudly beats surprising whoever is paying by the second.
- **Seeds are sd.cpp seeds.** Reproducible against the same build and
  parameters, not portable to a torch pipeline.
