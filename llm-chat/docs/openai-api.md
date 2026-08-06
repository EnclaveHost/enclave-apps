# llm-chat OpenAI-compatible API reference

The service-provider interface of the llm-chat app: everything a client can
send to the `/v1` endpoints and everything that comes back, as implemented in
`src/lib.rs` (accurate as of llm-chat **0.40.0**). Point any OpenAI SDK at a
deployment's URL and it works; this document is the contract, including the
Enclave extensions the OpenAI schema has no words for.

```
base URL:  https://<id8>.app.enclave.host        (or the deployment's custom domain)
           └── /v1/models
           └── /v1/chat/completions
```

Everything here is served **by the model's own enclave**. There is no gateway
rewriting requests: the JSON you POST is parsed inside the CVM, and the
attestation for the hardware answering you is at `GET /attestation` on the
same origin.

---

## Authentication

Deployment policy, not per-model: when the deployment's config sets a
top-level `api_key`, both `/v1/*` routes require

```
Authorization: Bearer <api_key>
```

and answer `401 {"error":{"message":"missing or invalid API key","type":"invalid_request_error"}}`
without it. **No key configured = open.** Gate with a private deployment when
that is the intent; the chat playground and the legacy `/chat` route stay open
either way (see `authorized()` and the `api_key` doc in `config.rs`).

---

## GET /v1/models

OpenAI list shape. One entry per **servable model**: an attached model volume
the config's `models` catalog describes. Largest first; the largest is the
default when a request names no model.

```json
{
  "object": "list",
  "data": [
    {
      "id": "qwen3.5-122b",
      "object": "model",
      "owned_by": "enclave-deployment",
      "enclave": { "volume": "qwen3.5-122b-a10b", "backend": "ggml",
                   "bytes": 61203283968, "default": true }
    }
  ]
}
```

- `id` is what you put in a request's `model` field (the volume name works
  too).
- `enclave` is an extension field: the backing volume, the inference backend
  (`ggml` or `onnx`), the weights size, and whether this entry is the
  default.
- With **nothing servable attached**, the list still advertises the
  configured `name` so SDK bootstrap flows see a model id; completions
  against it explain what to attach.
- The open, unauthenticated variant is `GET /models` (non-OpenAI shape, used
  by the playground; also reports GPU presence and vision capability).

---

## POST /v1/chat/completions

One endpoint, streaming and non-streaming. Unknown OpenAI fields are
**accepted and ignored** (`n`, `presence_penalty`, `frequency_penalty`,
`logprobs`, `response_format`, `seed`, `user`, ...): you get one choice,
sampled the one way the server samples. The fields that exist:

| field | type | meaning |
|---|---|---|
| `messages` | array | required. See [Messages](#messages). |
| `model` | string | a `name` or volume from `/v1/models`. Absent **or unknown** = the largest attached model. |
| `stream` | bool | default false. See [Streaming](#streaming-responses). |
| `max_tokens` | int | completion budget. Clamped to the config's `max_new_cap`, floor 1. |
| `max_completion_tokens` | int | newer OpenAI spelling; used when `max_tokens` is absent. |
| `temperature` | float | default = config (`0.7` unless overridden). Clamped `0.0..=2.0`; `0` = greedy. |
| `top_p` | float | default = config (`0.9`). Clamped `0.05..=1.0`; `1.0` = off. |
| `top_k` | int | extension (common in OSS servers). Default = config; `0` = off. |
| `stop` | string \| [string] | extra stop strings; the first **4** of an array are honoured, on top of the chat template's own stops. |
| `enable_thinking` | bool | extension: `false` disables `<think>` reasoning on thinking models. See [Thinking](#thinking). |
| `chat_template_kwargs` | object | vLLM/SGLang spelling: `{"enable_thinking": false}`. Top-level wins when both present. |
| `target` | string | extension: `"cpu"` \| `"gpu"` \| `"auto"` (default auto: GPU then CPU fallback). ggml deployments ignore it (offload is the node's call). |
| `web_search` | bool \| string | extension, needs the deployment's search config. See [Web search](#web-search-image-generation-and-the-router). |
| `image_gen` | bool \| string | extension, needs the deployment's `image` block. Same section. |
| `tools` | bool \| array | **two modes told apart by shape** — see [Tools](#tools). |
| `tool_choice` | string \| object | OpenAI switch: `"none"`, `"auto"`, `"required"`, or `{"type":"function","function":{"name":...}}`. |

Request body cap: **40 MB** (`MAX_BODY_BYTES`) — sized for base64 image
attachments, not for prompts that long.

### Messages

Each element: `{"role": ..., "content": ...}` with roles `system`, `user`,
`assistant`, `tool`. `content` is either a **string** or OpenAI's **array of
typed parts**. Three part spellings are accepted for images, because SDKs in
the wild disagree:

```json
{"type": "image_url",   "image_url": {"url": "data:image/png;base64,..."}}   // OpenAI chat
{"type": "input_image", "image_url": "data:..."}                             // Responses API
{"type": "image",       "source": {"type": "base64", "data": "..."}}         // Anthropic
```

Text parts are joined with newlines; image parts become the turn's
attachments.

**Images are inline-only.** A `data:` URI (base64) or bare base64 is decoded;
an `http(s)://` URL is **refused with a 400**, never fetched — resolving it
would tell a third-party host what this deployment is looking at, which is
the thing the app exists to avoid (and fleet egress is IPv6-only besides).
Accepted formats by magic bytes: png, jpeg, webp, gif, bmp; webp is
transcoded to JPEG in-app (the vision encoder has no VP8). Limits, all
config-tunable: `max_images` per request across all turns (default **4**),
`max_image_bytes` per image (default **6 MiB**), and each image is budgeted
at `image_tokens` (default **1024**) prompt tokens for admission against
`max_prompt_tokens`. Whether pictures are *read* at all depends on the
deployment: a `vision` model reads them itself, a configured
`vision_service` delegates to a sibling deployment — see the VISION sections
of the `lib.rs` header. Requests with images sit out speculative decoding.

**Tool history** (the agent-framework round trip) is understood regardless of
whether the current request declares tools:

```json
{"role": "assistant", "content": null,
 "tool_calls": [{"id": "call_abc", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}]}
{"role": "tool", "tool_call_id": "call_abc", "name": "get_weather", "content": "{\"temp\": 3}"}
```

`arguments` per spec is a JSON-encoded string; a client that sends the object
itself is accepted rather than corrected. `name` on the tool turn is
OpenAI-deprecated but still read. `fold_tool_history()` renders both into
the model's trained text forms before prompting.

### Sampling and lengths

Request value → config default → clamp, in that order (see `gen_params()`).
The config also applies `rep_penalty`/`rep_window` (no request field) and a
`repeat_guard` degeneration stop: a block of tokens repeating back-to-back
more than N times (default 4) hard-stops the reply. Completion length is
`max_tokens`/`max_completion_tokens`, else the config's `default_max_new`,
always capped by `max_new_cap`.

### Thinking

On models whose config marks them `thinking` (qwen3.x-style, chatml
template), the prompt **force-opens** the `<think>` block and the server
re-emits `<think>\n` at the head of the reply so clients always see a
complete block; the model generates the reasoning and the closing
`</think>`. Reasoning arrives **in `content`** — there is no separate
`reasoning` field. History replays strip prior think blocks server-side, so
you may send assistant turns back verbatim.

- `enable_thinking: false` (top-level, or inside `chat_template_kwargs`)
  swaps in the trained no-think form. On non-thinking models the switch is
  ignored.
- The config's `think_budget` (and the `effort` scaling block, when
  configured — a one-line classifier rates each turn low/medium/high) caps
  the tokens a reply may spend inside the block; at the budget the server
  closes the block for the model and the answer starts. What a turn got is
  reported in the response's `enclave.effort` / `enclave.think_budget` /
  `enclave.think_forced`.

### Web search, image generation, and the router

Extensions over the OpenAI body, so API clients get the same retrieval the
built-in UI does. All three follow one contract: **explicit request value
wins; an absent field takes the deployment's default** (`default_on` in the
respective config block). A deployment without the block never searches,
never draws, and never advertises either.

- `web_search: true` — search **every** turn. `"auto"` — a cheap router
  generation decides per turn whether the question needs the web (and what
  to ask). `false`/`"off"` — never. Absent — the deployment's
  `search.default_on` (on ⇒ auto). Beware the probe trap: absent does *not*
  mean off on a `default_on` deployment; send an explicit `false` to bypass
  the router when benchmarking.
- `image_gen: true`/`"auto"` lets the router decide the turn wants a
  picture; `false`/`"off"` withholds it; absent takes `image.default_on`
  (default **true** when the `image` block exists). There is no "always".
- A user message starting `/search ` forces a search; `/image ` forces a
  picture — regardless of the switches.
- Search results are folded into the prompt before generation; the sources
  come back on `enclave.search` (non-streaming) or as a `: enclave-search`
  SSE comment (streaming), shaped
  `{"provider": ..., "ms": ..., "sources": [{"title": ..., "url": ...}]}`.
- A generated image arrives on `enclave.image` (non-streaming) or a
  `: enclave-image` comment (streaming):
  `{"data_uri": "data:image/png;base64,...", "prompt": ..., "model": ..., "seed": ..., "ms": ...}`.

### Tools

The `tools` field means two different things **told apart by its JSON
shape**. In both modes `tool_choice: "none"` withholds everything.

**1. Deployment registry — `tools: true` (boolean, Enclave extension).**
The model may call the HTTP endpoints and MCP servers **this deployment
configured** (the config's `tools` block; probe them at `GET /tools`). The
server runs the calls mid-answer, up to the config's `max_calls` per turn,
and regenerates from the results. The client cannot add an entry or change a
URL — the registry is the deployment's, never the request's. `false`
withholds it; absent takes the deployment's `tools.default_on`. What ran is
reported on `enclave.tools` (non-streaming) or `: enclave-tool` /
`: enclave-tool-result` / `: enclave-tools-ran` comments (streaming).

**2. Client passthrough — `tools: [...]` (array, OpenAI).**
The client's own functions are offered to the model **instead of** the
deployment's registry (never merged — a client-supplied name must not select
a server-executed capability that shares it). Nothing here executes a
caller's tool: the model's call comes back structured and the **client**
runs it, then sends the result as a `role: "tool"` message.

```json
"tools": [{"type": "function",
           "function": {"name": "get_weather", "description": "...",
                        "parameters": {"type": "object", "properties": {...}}}}]
```

The flat `{"name": ..., "description": ..., "input_schema": {...}}` spelling
(Anthropic / Responses) is also accepted. An array entry naming no function
is a 400 — a client waiting for `tool_calls` has to hear why it will never
get one. A call comes back as:

```json
"finish_reason": "tool_calls",
"message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": "call_18c2f4a1b3e_0", "type": "function",
                            "function": {"name": "get_weather",
                                         "arguments": "{\"city\":\"Oslo\"}"}}]}
```

`arguments` is a JSON-encoded string, per spec. Reasoning/prose the model
produced before the call stays in `content` (null when there was nothing but
the call). Streaming sends the whole call as **one** delta (each entry also
carries `index`), then a `finish_reason: "tool_calls"` chunk — the protocol
allows argument fragments; this server has no reason to slice what is
already complete.

`tool_choice`: `"auto"` and absent are the default behaviour; `"required"`
and the named-function form `{"type":"function","function":{"name":...}}`
are honoured **as an instruction in the prompt** — there is no grammar
constraint, so a model can still disobey; check `finish_reason` rather than
assuming.

---

## Non-streaming responses

`stream` absent/false: the request runs to completion and answers one JSON
document with real HTTP status codes.

```json
{
  "id": "chatcmpl-enclave18c2f4a1b3e",
  "object": "chat.completion",
  "created": 1770000000,
  "model": "qwen3.5-122b",
  "choices": [{
    "index": 0,
    "message": { "role": "assistant", "content": "<think>\n...\n</think>\n\n..." },
    "finish_reason": "stop"
  }],
  "usage": { "prompt_tokens": 412, "completion_tokens": 128, "total_tokens": 540 },
  "enclave": {
    "target": "gpu", "load_ms": 3, "prefill_ms": 361, "decode_ms": 1954,
    "draft_tokens": 96, "draft_accepted": 71,
    "think_forced": false, "effort": "medium", "think_budget": 4096
  }
}
```

- `finish_reason`: `"stop"` (EOS or a stop string), `"length"` (`max_tokens`
  exhausted), `"tool_calls"` (passthrough call pending).
- `usage.prompt_tokens` counts **text** tokens. Images are priced in
  positions by the host and reported beside it as `enclave.images` /
  `enclave.image_tokens`, so an OpenAI client's arithmetic still adds up.
- `enclave` is the extension envelope; everything OpenAI has no field for
  rides here: timing, speculation counters, `effort`/`think_budget`/
  `think_forced`, and when the legs ran: `search`, `vision`
  (`{"model", "question", "images", "image_tokens", "ms"}` — what the
  delegated vision leg asked about your picture), `image` (a generated
  picture as a `data_uri`), `tools` (the registry-call log).

**Use non-streaming only for short turns.** The platform proxy path
(`*.app.enclave.host`) has a response-timeout budget of roughly three
minutes that this app cannot influence; a turn that can run long — image
generation, a big vision read, a large think budget — belongs on
`stream: true`, where comments keep the connection warm.

## Streaming responses

`stream: true`: `Content-Type: text/event-stream`, the OpenAI chunk
protocol.

```
: loading model                                        ← SSE comments: leg progress
: enclave-search {"provider":"brave","ms":812,"sources":[...]}
data: {"id":"chatcmpl-enclave…","object":"chat.completion.chunk","created":…,"model":"…","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}
data: {"…","choices":[{"index":0,"delta":{"content":"<think>\n"},"finish_reason":null}]}
data: {"…","choices":[{"index":0,"delta":{"content":"…"},"finish_reason":null}]}
data: {"…","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}
data: [DONE]
```

The contract, in order:

1. **The response opens before the search/image/vision legs run.** Those
   legs can hold a turn for minutes; headers held back that long are how
   proxies and SDK read-timeouts kill a request before its first byte.
   Consequence: a leg failure arrives as an **in-stream error event on a
   200** (see [Errors](#errors)), not a status code.
2. **SSE comments** (lines starting `:`) carry everything the chunk schema
   has no field for. Conforming SSE parsers are required to ignore them, so
   strict OpenAI clients are never confused; clients that want the extras
   parse them. Prefixes: bare comments are progress/heartbeat text;
   `: enclave-search {...}`, `: enclave-vision {...}`,
   `: enclave-image {...}` (same JSON shapes as the non-streaming envelope
   fields), `: enclave-tools <note>`, `: enclave-tool {...}` (a registry
   call), `: enclave-tool-result {...}`, `: enclave-tool-note <note>`,
   `: enclave-tools-ran [...]` (the end-of-turn log).
3. First data chunk is the **role preamble** (`delta: {"role":"assistant"}`);
   OpenAI clients expect it.
4. On a thinking turn, the first content delta is the re-emitted
   `<think>\n`; the reasoning streams as ordinary `content` deltas.
5. A passthrough tool call streams as one delta carrying the complete
   `tool_calls` array (entries carry `index`), then an empty delta with
   `finish_reason: "tool_calls"`.
6. Terminal chunk: empty delta with the `finish_reason`, then
   `data: [DONE]`.

There is **no usage object in the stream** (no `stream_options` support);
if you need token accounting, use non-streaming or count client-side.

---

## Errors

One JSON shape everywhere:

```json
{ "error": { "message": "…", "type": "invalid_request_error", "code": "sessions_busy" } }
```

`code` appears when the condition is one the server names, so clients can
switch on it instead of pattern-matching prose:

| code | meaning |
|---|---|
| `model_not_loaded` | the graph is not in the host's registry (boot preload missed it) |
| `host_load_failed` | the boot preload failed — look at the tenant log |
| `volume_not_attached` | the config names a model volume this deployment did not attach |
| `sessions_busy` | every inference session is taken; retry |
| `no_vision` | images sent to a model with no projector (pick a vision model) |
| `too_many_images` | over `max_images` for one request |
| `image_too_large` | over `max_image_bytes` |
| `vision_unsupported` | the node's engine predates the vision toolchain |
| `vision_unavailable` | the vision leg exists but cannot serve right now |
| `image_undecodable` | the bytes are not an image the encoder can read |
| `image_too_wide` | image exceeds the encoder's dimension limit |

Status mapping (non-streaming): **400** bad JSON, bad tool array, refused
attachment, prompt too big; **401** bad/missing API key; **500**
configuration errors and generation failure after all targets; **502** a
search/vision leg failure. Streaming: everything after the headers is an
in-stream event on the 200 —

```
data: {"error":{"message":"…","type":"server_error"}}
```

— possibly after content deltas (mid-generation failure), and with no
`[DONE]` after an error terminator. A registry tool call that was attempted
but ran nothing (unparseable, over `max_calls`) is not an error: the raw
call block is withheld and a bracketed note `[…]` arrives as content.

---

## Examples

```bash
# curl, streaming, thinking off, explicit no-search
curl -N https://<id8>.app.enclave.host/v1/chat/completions \
  -H 'Authorization: Bearer <key>' -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"Say hi"}],
       "stream":true, "enable_thinking":false, "web_search":false}'
```

```python
from openai import OpenAI
client = OpenAI(base_url="https://<id8>.app.enclave.host/v1", api_key="<key or any string>")
stream = client.chat.completions.create(
    model="qwen3.5-122b",              # or omit: largest attached wins
    messages=[{"role": "user", "content": "What changed in Rust 1.90?"}],
    stream=True,
    extra_body={"web_search": "auto"}, # Enclave extension
)
for chunk in stream:
    print(chunk.choices[0].delta.content or "", end="")
```

The SDK ignores the SSE comments and the `enclave` envelope on its own;
`api_key` may be any non-empty string when the deployment configured none.

---

## Adjacent (non-/v1) endpoints

Not OpenAI-shaped, no bearer auth, but part of the same service and useful
around API integrations — see the `lib.rs` header for each:

| route | what |
|---|---|
| `GET /ping` | liveness; touches no wasi-nn |
| `GET /models` | open model list + `gpu` presence + vision capability (the playground's) |
| `GET /warmup[?model=…]` | load + one forward pass; bare = smallest-first ladder over every servable model |
| `GET /attestation` | this deployment's SEV-SNP quote, measurement, GPU CC mode |
| `GET /search?q=…` / `?url=…` | web-search probe: provider leg / fetch-extract leg, separately |
| `GET /tools[?call=…&args=…]` | resolve the tool registry; run one entry |
| `POST /chat` | legacy SSE endpoint the playground uses (its own event schema) |
| `POST /title` | name a chat from its opening exchange; failures answer `title: null` |

## Compatibility notes

- One choice per request: `n` is ignored, `choices` is always length 1.
- No `logprobs`, no `response_format`/JSON mode, no `seed`, no
  `presence_penalty`/`frequency_penalty` (the config's `rep_penalty` applies
  instead). Unknown fields never 400.
- `tool_choice: "required"` / named-function are prompt instructions, not
  grammar constraints.
- Reasoning arrives inside `content` as a `<think>…</think>` block, not in a
  separate field.
- Streaming carries no `usage`.
- Image parts must be inline base64; remote image URLs are refused by
  design.
