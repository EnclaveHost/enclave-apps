# eyesoff-ai OpenAI-compatible API reference

The service-provider interface of the eyesoff-ai app (formerly llm-chat): everything a client can
send to the `/v1` endpoints and everything that comes back, as implemented in
`src/lib.rs` (accurate as of **0.53.0**). Point any OpenAI SDK at a
deployment's URL and it works; this document is the contract, including the
Enclave extensions the OpenAI schema has no words for.

```
base URL:  https://<id8>.app.enclave.host        (or the deployment's custom domain)
           └── /v1/models
           └── /v1/chat/completions
           └── /v1/keys
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

The same credential is also read from `x-api-key: <value>`, and where the
fleet's inbound TLS proxy eats `Authorization` (observed 2026-08-17: a
correct Bearer answered 401 while the same value as `X-Api-Key` answered
200), that spelling is the one that reliably arrives - send both when in
doubt, the server checks either.

A deployment configured for **Sign in with Enclave** (top-level `sso` block)
additionally accepts a platform sign-in token as the same credential, on
`/v1/*` and on the playground routes alike - one login, either surface. With
`sso.required` true the playground's `/chat` and `/title` demand one; a 401
there carries `"code": "sso_required"`, which is what tells the playground to
open its sign-in dialog. See `sso.rs` for the token format and
`PLATFORM-sso.md` for the mint side.

A deployment that also sets `api_key_seed` (a secret reference:
`"$API_KEY_SEED"`) accepts a third credential on `/v1/*`: a **derived
personal API key** (`EAK1.…`), minted at `POST /v1/keys` below. Three
credentials, one check, any of them opens the API.

---

## POST /v1/keys

Your own permanent API key, derived from your sign-in. Requires both the
`sso` block and `api_key_seed` in the deployment config (404 otherwise), and
a valid sign-in token as the request credential (401 with
`"code": "sso_required"` without one). No request body.

```json
{
  "object": "api_key",
  "key": "EAK1.eyJzdWIiOiJhY2N0XzBlNjRkMTg5N2YxMGIzMmQzYTFiYzg0ZSIsInYiOjF9.uZIhoolqA1yzEDBoI6RM-ckj5RjieCQvWmtPxbB1InI",
  "sub": "acct_0e64d1897f10b32d3a1bc84e",
  "deterministic": true
}
```

The key is not issued and stored, it is **derived**: a MAC, under the
deployment's seed, over the identity your wallet or passkey unlocked at
enclave.host (the sign-in token's `sub`). That buys three properties worth
scripting against:

- **Deterministic.** The same identity always derives the same key
  (`"deterministic": true` states it in the payload). Losing the key costs a
  fresh sign-in, not a rotation.
- **Stateless.** The deployment stores nothing; verification recomputes the
  MAC. There is no key list to read or leak.
- **No expiry.** The trade a permanent credential makes. Revocation is the
  operator rotating `api_key_seed`, which revokes *every* derived key at
  once - there is no per-key revocation, and this document will not pretend
  otherwise.

Format (`apikey.rs` is the implementation and carries a pinned test vector):
`EAK1.<base64url({"sub":…,"v":1})>.<base64url(Keccak-256(len(seed) || seed ||
"EAK1.<payload>"))>`. The sign-in token, with its expiry, stays the root of
the chain: a derived key cannot mint another key.

`GET /models` (the playground route) advertises availability as
`auth.keys: true`, and the playground's API dialog offers signed-in visitors
a Reveal button wired to exactly this endpoint.

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
      "id": "fable-fusion-27b-mtp",
      "object": "model",
      "owned_by": "enclave-deployment",
      "enclave": { "volume": "fable-fusion-27b-mtp-q4-gguf", "backend": "ggml",
                   "bytes": 18500000000, "default": true }
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
| `tools` | bool \| object \| array | **three modes told apart by shape**: a boolean arms or withholds the deployment's tools; an object switches them by **group** (`{"off": ["notes"]}` withholds the named groups, `{"on": ["search", "vm"]}` offers exactly those; names are what `GET /models` lists under `groups`, and `search`/`images` are groups too); an array is the client-tools passthrough — see [Tools](#tools). |
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

**Images may ride any role, including `assistant`.** That is how a picture
this deployment just made comes back into the conversation: take
`enclave.image.data_uri` from the previous response and send it as an image
part on that assistant message in the next request. Without it the model's
last picture is gone the moment it reaches you, and "upscale that" has no
bytes to bind to (see [Picture tools](#picture-tools)). The playground does
this for you from 0.45.0: every reply's generated image is folded back into
the history it sends, the oldest pictures dropped first to stay inside
`max_images`, and a generated picture bigger than `max_image_bytes` left out
rather than getting the whole request refused. A 4x upscale is often that big,
so upscaling an upscale is not something a client can count on.

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
- Both are also **tool groups**: `tools: {"off": ["search"]}` is
  `web_search: "off"`, `tools: {"on": ["images"]}` is `image_gen: "auto"`
  with everything else withheld. When a legacy switch and the group form
  disagree about its own group, the legacy switch wins, so an older client
  keeps its meaning. `GET /models` → `groups` lists every group with its
  functions and starting position; an http entry's `group` config field
  names its group, endpoints whose names share a family (`notes_list`,
  `notes_write`, ...) fall into one group by default, and picture-making or
  picture-reading entries join `images`.
- A user message starting `/search ` forces a search; `/image ` forces a
  picture — regardless of the switches.
- Search results are folded into the prompt before generation; the sources
  come back on `enclave.search` (non-streaming) or as a `: enclave-search`
  SSE comment (streaming), shaped
  `{"provider": ..., "ms": ..., "sources": [{"title": ..., "url": ...}]}`.
- A generated image arrives on `enclave.image` (non-streaming) or a
  `: enclave-image` comment (streaming):
  `{"data_uri": "data:image/png;base64,...", "prompt": ..., "model": ..., "seed": ..., "ms": ...}`.
  One envelope whichever leg made it: the router's `image` block, or a
  registry tool with an image result (`generate_image`, `upscale_image`, or
  whatever the deployment named its own). From a tool, `model` and `seed` are
  `null` and `prompt` carries the tool's `prompt` argument, or the rest of its
  arguments when it has none (`{"factor":2}` for an upscale). Arming an
  image-producing tool stands the router's image verdict down for that turn,
  so a deployment never pays for both deciders.

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

Drawing, looking and upscaling live here, as ordinary registry entries rather
than as built-ins: see [Picture tools](#picture-tools) below.

An `mcp` entry in that block points at a Model Context Protocol server, whose
tools join the same list and are called the same way. One shape worth knowing:
the [api-mcp-adapter](https://github.com/EnclaveHost/enclave-apps/tree/main/api-mcp-adapter)
app holds the `http` entries in ITS config and serves them as one MCP endpoint,
so a chat deployment carries a single `mcp` entry, the backends' API keys live
in that deployment rather than this one, and the same tools serve other agents
(Claude Code, Cursor) from the same URL. Nothing is lost on the way across:
pictures in and out, citations, the signed-in user, the settings switches and
the per-entry budgets all travel, so a tool behaves here exactly as it did when
its entry lived in this config.

**2. Client passthrough — `tools: [...]` (array, OpenAI).**
The client's own functions are offered to the model, and nothing here executes
one: the model's call comes back structured and the **client** runs it, then
sends the result as a `role: "tool"` message.

From 0.46 they are offered **alongside** the deployment's registry rather than
instead of it, so an agent that brings its own file tools still reaches
`web_search`. The model sees one list and is told nothing about which entry is
whose; a finished call is routed by the entry's **source**, never by its name.
A server entry runs here, in the loop, and the client never sees it; a client
entry ends the turn and comes back on `tool_calls`. A name declared on **both**
sides resolves to the client's, and the deployment's twin is dropped from the
list entirely — so a client-supplied name still cannot select a server-executed
capability. `"web_search": false` withholds the web builtins from the merged
list too, and `tool_choice: "none"` withholds everything.

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

#### Picture tools

Drawing, looking and upscaling are not built-ins: they are ordinary `http`
entries in the deployment registry (mode 1 above), so the same wiring drives
any API, not just the sibling Enclave apps. Three generic powers make that
work, and all three are visible from the client side.

**Entries that take the turn's pictures.** A body template may use the
reserved `"$image"` (the first attachment) or `"$images"` (all of them), which
the server fills with data URIs: bytes the model could never write into its
own arguments. Such an entry is offered **only on turns that actually carry a
picture**. On a model that reads pictures itself, readers (picture in, text
out, like `view_image`) are dropped while transformers (picture in, picture
out, like `upscale_image`) stay, because local vision cannot substitute for an
upscale. Calling one on a turn with no picture does not answer "no such tool",
which a model repeats to the user as the capability not existing; it answers
that the tool exists, that it needs a picture, and that the user should be
asked to attach one.

**Entries that produce a picture.** `result: {"image": "<dot path>"}` pulls
base64 (or a data URI) out of the response, delivers it to the client exactly
as a routed image is delivered, and hands the model a note saying a picture
was made, that it cannot see it, and to acknowledge it briefly. An
image-producing entry gets a 12 MB response cap by default, since a picture
arrives as megabytes of base64; a 4x upscale of a large attachment is tens of
MB, which is why the canonical upscale entry raises its own `max_bytes`.

**Optional arguments cost nothing to omit.** A declared parameter the model
leaves out leaves a `"$name"` hole in the body template, and the hole is
pruned from the request rather than posted as the literal string `"$name"`.
That is what makes optional knobs expressible at all: `factor` on an upscale,
`size` on a generation.

The canonical pair, as a client meets them at `GET /tools` (the names are the
operator's to choose; these are the ones the config template ships):

| tool | arguments | what comes back |
|---|---|---|
| `generate_image` | `prompt` (required), `size` (optional) | a new picture on `enclave.image` |
| `upscale_image` | `factor` (optional); the picture is bound from the turn | the enlarged picture on `enclave.image` |

**Aspect ratio.** `generate_image` takes an optional `size` whose enum the
deployment writes (`"1024x1024"`, `"1024x768"`, `"768x1024"`, `"1024x576"`,
`"576x1024"`, ...), kept inside the generator's own `min_size`/`max_size` so
no value silently snaps to a different shape. The model picks the shape the
request implies: wide for landscapes and banners, tall for portraits and
posters, square when nothing argues otherwise. Omitted, the size is pruned
from the request and the generator's `default_size` applies, exactly as before
the knob existed.

**Upscaling.** `upscale_image` reads the turn's picture and returns a bigger
one: Real-ESRGAN, where `factor` 4 quadruples each side (the upscaler's native
scale, and what an omitted argument serves), 2 doubles it (supersampled down
from the 4x pass), 1 cleans the picture up at its current size. There is no
shape argument, because ESRGAN preserves the input's aspect ratio by
construction. The picture comes from the turn rather than from the model's
arguments, so upscaling something the model itself drew depends on the client
having sent that picture back (see [Messages](#messages)); a picture the user
attached is already there.

The canonical entries live in `assets/deploy-config.template.json` under
`_tools_comment`, url/body/result and all: the operator wires them, and a
client only ever sees the resolved names and schemas.

#### The loop: `loop`, `wait`, and budgets

A turn with tools is a loop already: the model calls, the server runs the
call, the answer regenerates from the result, up to `max_calls` times. What
the `loop` field adds is **persistence**, and the `wait` builtin is what lets
a loop work on a job that takes longer than one call.

**`loop: true`** (Enclave extension; needs `tools` on) changes the rules the
model is given. When the goal comes with a check (tests, a harness, a build, a
command that must succeed) it is to keep going until the check passes: run
it, read what failed, change one thing, run it again; not stop to ask or to
report progress; keep state in files rather than in the conversation; stop
early only when the check passes, cannot pass, or the budget is nearly spent,
and then report exactly what passes and what does not. Every tool result in a
persisting loop carries a trailer, `[loop: call 7 of 32; 6 minutes elapsed of
1 hour]`, so the budget is a fact the model reads rather than a count it
keeps. The object form **lowers** the deployment's budgets for one answer and
never raises them: `"loop": {"max_calls": 8, "max_seconds": 600}` (add
`"persist": false` to lower the budget without asking for persistence).

**Budgets.** The config's `max_calls` bounds the calls in one answer and
`max_seconds` (default 3600) its wall-clock time; calls, waits and the
regenerations between them all count. Past either, the model is told once to
finish from what it has; a second call is refused and the answer ends (the
`: enclave-tool-note` comment, or the playground's notice, says which). `GET
/models` reports both under `tools`, and each `: enclave-tool` comment
carries `n`, `of`, `elapsed_s` and `max_seconds`.

**`wait`** (builtin; name it in the config's `tools.builtin`) sleeps
`seconds` inside the enclave and returns. Nothing leaves, nothing is
computed, the request is parked. Its result says how much of the answer's
time budget is left. One wait sleeps at most `wait_max_s` (default 600) and
never past the end of the budget; a longer ask is clamped and told so, and
the model calls again to keep waiting. While it sleeps the stream ticks a
status line every 5 seconds (`waiting 2 minutes: build · 85s left`), which is
what keeps every idle timeout between the enclave and the client from firing,
**so a turn that can wait must stream**. A non-streaming request that waits
past ~180 seconds is cut by the proxy before it answers. A client that
disconnects mid-wait ends the wait: the next tick notices the dead stream and
the turn stops there (see "Ending a turn" below).

**Long loops and the context window.** Every step re-prefills the whole
conversation, so the results the model has already acted on are most of what
a thirty-step loop pays for. The newest `keep_results` (default 3) results
stay in the prompt whole; older ones are condensed to their first and last
240 characters with a note saying so. A harness whose verdict is on its last
line survives condensing; one that buries it in the middle should be told to
print a summary.

#### Subagents: `spawn_agent`

A deployment whose `tools` block sets **`max_agents`** to a positive number
gives every loop a `spawn_agent` tool. A call spawns a **subagent**: a fresh
conversation on the same model, with the same tools and the same machine but
an empty context, given one `task` (plus optional `context` and `expect`),
that runs its own tool loop to completion. Its final message comes back as
the call's result, prefixed `Subagent #2 finished (5 calls, 3 minutes). Its
report:`. Nothing else crosses: the child never sees the parent's
conversation, and the parent reads nothing of the child's but that report
(truncated to `max_chars` like any result).

Subagents run **one at a time**, inside the request that spawned them (a
wasm component has no threads), and **may spawn their own**. Two limits bound
the tree, both the deployment's:

- **`max_agents`** is the total an answer may spawn, however they nest. A
  loop that could not spawn another is never shown the tool (the count is
  spent, or it sits at the depth limit); a call made after a sibling used the
  last slot is refused with a result that says so, and the model is told to
  do the task itself.
- **`max_agent_depth`** (default 3) is how deep the nesting goes: the answer
  is depth 0, its children 1, theirs 2.

A child's budget is **`agent_max_calls`** (default: the answer's
`max_calls`) of its own, and whatever is **left** of the answer's
`max_seconds`: no tree outlives the answer. A child inherits the answer's
persistence (`loop: true`), and the object form of `loop` may lower
`max_agents` and `max_agent_depth` for one answer (to zero, if the client
wants no subagents at all). `GET /models` reports both under `tools`.

On the stream, a child narrates as its own events, each carrying the
agent's id. `/chat` sends `{"agent": {id, parent, depth, task, n, of}}`
when one starts, `{"agent_delta": {id, delta}}` for its text (never as the
answer's own `delta`), `{"agent_note": {id, text}}` for a decision that
restarts its generation, `{"tool": {..., "agent": id}}` /
`{"tool_result": {..., "agent": id}}` for its calls, and
`{"agent_done": {id, ok, ms, calls, chars}}` when it returns; the done
frame's `loop` block counts `agents`. Streaming `/v1` carries the same as
`: enclave-agent`, `: enclave-agent-delta`, `: enclave-agent-note`,
`: enclave-agent-done` comments, and a child's calls appear in
`enclave.tools` with an `agent` field. The playground shows each subagent as
a card nested by depth, with its calls and its live text inside.

**Running a harness longer than one call.** The deployment's command tool
has a per-call timeout (60 s for the `run_vm_command` entry in the template).
A model working to a check starts the long job in the background on the
machine with its output going to a file (`nohup ./run-tests.sh > /tmp/t.log
2>&1 &`), calls `wait` for about as long as it expects the run to take, then
reads the file. The playground shows the step counter and the countdown; the
stop button ends the turn at any point, including mid-wait.

**Ending a turn.** Closing the response stream is how a turn is ended: the
playground's stop button aborts its request, a closed tab or a crashed
browser closes the connection, an API client cancels its request. The enclave
asks the stream whether its reader is still there before every forward pass
and every tool call, and stops at the next such boundary: the next prefill
chunk or decoded token, the next tool call (which is then not run), the next
5-second tick of a `wait`, the next 15-second tick of a slow outbound tool
request (which is then abandoned), a subagent's next step. What cannot be
interrupted is the one host call already in flight: milliseconds on a GPU
node, one prefill chunk of up to about 45 seconds on a CPU node. Nothing
outlives the stream - there is no way to detach a turn and collect it later,
so a client that wants the answer stays connected for it. A non-streaming
request has no stream to ask and runs to completion once it has started.

---

## Non-streaming responses

`stream` absent/false: the request runs to completion and answers one JSON
document with real HTTP status codes.

```json
{
  "id": "chatcmpl-enclave18c2f4a1b3e",
  "object": "chat.completion",
  "created": 1770000000,
  "model": "fable-fusion-27b-mtp",
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
    model="fable-fusion-27b-mtp",      # or omit: largest attached wins
    messages=[{"role": "user", "content": "What changed in Rust 1.90?"}],
    stream=True,
    extra_body={"web_search": "auto"}, # Enclave extension
)
for chunk in stream:
    print(chunk.choices[0].delta.content or "", end="")
```

The SDK ignores the SSE comments and the `enclave` envelope on its own;
`api_key` may be any non-empty string when the deployment configured none.

Draw a picture, then enlarge the one that came back. The second request has to
carry the picture: the model's own output reaches the client, not the server's
memory of it.

```python
draw = client.chat.completions.create(
    model="fable-fusion-27b-mtp",
    messages=[{"role": "user", "content": "Draw a tiger in tall grass, wide"}],
    extra_body={"tools": True},          # the deployment's registry
)
picture = draw.model_extra["enclave"]["image"]["data_uri"]   # data:image/png;base64,…

bigger = client.chat.completions.create(
    model="fable-fusion-27b-mtp",
    messages=[
        {"role": "user", "content": "Draw a tiger in tall grass, wide"},
        {"role": "assistant", "content": [
            {"type": "image_url", "image_url": {"url": picture}},
            {"type": "text", "text": draw.choices[0].message.content or ""}]},
        {"role": "user", "content": "Upscale it 2x"},
    ],
    extra_body={"tools": True},
)
enlarged = bigger.model_extra["enclave"]["image"]["data_uri"]
```

The shape of the first picture is the model's call (`generate_image`'s `size`,
picked from "wide"); the second keeps that shape whatever `factor` it asks
for. Both requests assume the deployment wired those entries, which
`GET /tools` answers, and both can run long: a real client streams them and
reads the `: enclave-image` comments instead, since three minutes is the
platform proxy's patience for a buffered response.

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
| `GET /legal`, `/privacy`, `/terms` | the deployment's legal document (one embedded page, linked from the playground) |
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
