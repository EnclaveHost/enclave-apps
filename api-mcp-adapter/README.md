# api-mcp-adapter: any HTTP API as an MCP server, from an enclave

api-mcp-adapter turns a list of HTTP endpoints into one **Model Context
Protocol** server. Describe the endpoints in the app config exactly as an
[eyesoff-ai](../eyesoff-ai) `tools.http` block does (a name, a description,
JSON-Schema parameters, a URL with `{arg}` placeholders, a body template
with `$arg` holes, headers that reference `$SECRET`s), reference the API
keys as deployment secrets, and every MCP client gets a streamable-HTTP
endpoint whose `tools/call` is one templated request the enclave makes on
the caller's behalf.

It is [s3-ipfs-adapter](../s3-ipfs-adapter)'s shape applied to tools: that
app takes a storage API and speaks a protocol clients already know; this
one takes any HTTP API and speaks the protocol agents already know.

```
 MCP clients                     the enclave (api-mcp-adapter)              the APIs
  eyesoff-ai (one mcp entry)  ──►  POST /mcp: tools/list from the config  ──►  image generator
  Claude Code, Cursor, …           tools/call = ONE templated request           risc-box /exec
  X-Api-Key: <key>                 with $SECRET headers from guest env           jot notebook
  X-User / X-Sso-Token             $user filled from the caller's identity       any HTTP API
```

## Why this instead of the block in eyesoff-ai

- **One place owns the tools.** The image generator, the VM, the notebook
  and whatever else are described once, here, and served to every agent
  with the same key: eyesoff-ai, Claude Code, Cursor, a LangGraph belt.
- **The keys move out of the chat deployment.** eyesoff-ai holds one secret
  (this adapter's key) instead of one per backend. The backends' keys live
  in this deployment, and the code holding them is the code the catalog
  pins: it can only make the requests the config describes.
- **Nothing is lost on the way.** Pictures in (the turn's attachments ride
  the `$image` / `$images` holes) and out (an image result is MCP image
  content, delivered to the chat), citations (`structuredContent.sources`),
  the signed-in user (`$user`), the switches eyesoff-ai shows (groups), and
  the prompt-side settings (`format`, `max_chars`) all cross, through the
  tool's `_meta`.
- **One round trip per call.** The server is stateless and the handshake is
  optional; eyesoff-ai's entry says `"handshake": false` and pays one
  request for discovery and one per call.

## The trust shape, stated plainly

- **The caller chooses arguments, never a URL or a header.** The adapter is
  not an open fetcher. Every request it makes is one the config describes,
  with the caller's arguments substituted into the places the entry names.
  A `{arg}` placeholder in the URL's **host** is refused outright, and
  arguments are percent-encoded into the path, so no argument can move the
  call to a different server.
- **`$user` is a header value and nothing else.** Mixed into a larger header,
  put in the URL, or referenced from a `body` template, it is refused —
  because in a template it would resolve against the *caller's own
  arguments*, carrying an identity the caller chose while looking like one
  the enclave verified.
- **The secrets exist only in the attested guest**, referenced as `$VAR`
  deployment secrets and injected as guest env by the enclave holding the
  lease. They never sit on-chain and never reach a client. A header whose
  secret is not set is refused **by name** rather than sent as the literal.
- **What crosses to an endpoint** is the arguments and this deployment's
  egress IP. Not the conversation, not the caller's key, not the model.
- **Per-user tools are never reached nameless.** An entry with a `"$user"`
  header needs a caller identity: a verified sign-in token, or the API key
  plus `X-User`. Without one the tool is absent from `tools/list` and refused
  by `tools/call`.

## Configuration

The deployment's App Config (`ENCLAVE_CONFIG`; locally `MCP_ADAPTER_CONFIG`):

```json
{
  "title": "eyesoff tools",
  "api_key": "$MCP_ADAPTER_API_KEY",
  "sso": { "signer": "0x…", "audience": "<this deployment id>", "accept": ["<an eyesoff-ai deployment id>"] },
  "timeout_s": 20,
  "max_bytes": 262144,
  "http": [
    {
      "name": "generate_image",
      "description": "Generate an image from a text prompt. Pick a size whose shape fits the request.",
      "parameters": { "type": "object", "properties": { "prompt": { "type": "string" },
                      "size": { "type": "string", "enum": ["1024x1024", "1024x768", "768x1024"] } },
                      "required": ["prompt"] },
      "url": "https://<generator-id8>.app.enclave.host/v1/images/generations",
      "method": "POST",
      "headers": { "authorization": "Bearer $IMAGE_API_KEY" },
      "body": { "prompt": "$prompt", "n": 1, "size": "$size" },
      "timeout_s": 180,
      "result": { "image": "data.0.b64_json" }
    },
    {
      "name": "notes_read",
      "description": "Read one note's full text by name.",
      "parameters": { "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] },
      "url": "https://<jot-id8>.app.enclave.host/api/notes/{name}",
      "headers": { "x-api-key": "$JOT_API_KEY", "x-user": "$user" },
      "result": { "text": "content" }
    }
  ]
}
```

- `api_key`: the key clients present as `X-Api-Key` (the platform's app
  gateway consumes `Authorization`, so on enclave.host a bearer never
  arrives). Reference a secret. If the secret is not set the deployment is
  **locked** (503 everywhere) rather than silently open. Omit it only for a
  deployment whose tools you would let anyone call.
- `sso` (optional): lets a client name the caller with an `X-Sso-Token`
  the app verifies itself, rather than the service asserting it with
  `X-User`. `signer` is the platform SSO signer address, `audience` this
  deployment's id, `accept` the ids of the eyesoff-ai deployments whose
  tokens are also good here. **A token names a caller; it does not open the
  key gate** — a deployment with an `api_key` still requires it, and
  `accept` is a statement about whose identities mean something here, not
  about who may call. Not needed for the eyesoff-ai wiring below, which
  names the user with the key and `X-User`.
- `timeout_s` (default 20) and `max_bytes` (default 256 KB) are the
  deployment-wide defaults an entry overrides.
- `http`: the entries. A whole eyesoff-ai `tools` block pastes as-is too
  (`{"tools": {"http": [...]}}`): its `http` array is what counts, and its
  budgets stay the client's business.

### Entry fields

Byte-for-byte the eyesoff-ai contract (its deploy-config template's tools
comment is the long form):

| field | executed here | meaning |
| --- | --- | --- |
| `name`, `description`, `parameters` | listed | the tool as the model sees it; `parameters` becomes `inputSchema` |
| `url` | yes | absolute; `{arg}` placeholders substituted percent-encoded; `$SECRET` references resolved |
| `method` | yes | GET (default), POST, PUT, PATCH, DELETE |
| `headers` | yes | `$SECRET` anywhere in a value; the whole value `$user` is the caller's identity |
| `body` | yes | JSON template; a whole-string `"$arg"` is filled, an unfilled declared hole is pruned; `"$images"` / `"$image"` take the caller's attached pictures |
| `query` | yes | send leftover arguments as a query string even on a POST (default: on GET and DELETE). Arguments the schema does not declare ride along too, exactly as they do in eyesoff-ai — so a fixed query parameter in the `url` can be shadowed by one a caller sends. Put anything that must not move in the path, or in the `body` template |
| `timeout_s`, `max_bytes` | yes | per call; an image-producing entry defaults to a 12 MB cap |
| `result` | yes | `{"image": <path>}` returns MCP image content; `{"text": <path>}` extracts one field |
| `sources` | yes | `{"list", "title", "url"}` dot paths; rows go to `structuredContent.sources` |
| `group` | `_meta` | the switch the tool sits under in eyesoff-ai; absent follows its family rule |
| `max_chars`, `format` | `_meta` | prompt-side: eyesoff-ai truncates and applies the format pass on its side |
| `route`, `route_arg` | `_meta` | carried through; eyesoff-ai's routed pre-pass only runs its own http entries |

Argument names `image` and `images` are reserved for the picture holes: an
MCP client passes data URIs under those names and the template receives
them. eyesoff-ai does this automatically for tools whose `_meta` says
`"images": true`.

## The MCP surface

`POST /mcp` (and `POST /`): JSON-RPC 2.0 over streamable HTTP, **stateless**.

- `initialize` answers with the client's protocol version when it is one of
  `2025-06-18`, `2025-03-26`, `2024-11-05` (else the newest), `capabilities:
  {tools: {listChanged: false}}`, `serverInfo` and `instructions`. It is
  **not required**: a client may send `tools/list` or `tools/call` cold.
- `notifications/*` get `202` with no body. No `Mcp-Session-Id` is minted;
  `DELETE /mcp` is `405`.
- `tools/list`: every entry this caller may see, with `annotations`
  (`readOnlyHint` for GET, `destructiveHint` for DELETE) and the `_meta` key
  `enclave.host/tool` (below).
- `tools/call`: the templated request. Failures are **in-band** (`isError:
  true` with a message that names the missing argument, the missing secret,
  the endpoint's HTTP status and a hint of its body, or the egress cause);
  JSON-RPC errors are reserved for protocol faults (`-32700` parse, `-32600`
  invalid, `-32601` unknown method, `-32602` bad params).
- A JSON array is accepted as a batch (the 2025-03-26 dialect) and answered
  as an array.
- `GET /mcp` is an info page for people and `405` for an event-stream
  `Accept`; `OPTIONS` answers CORS preflight so browser-hosted clients can
  dial.

Results: a text tool's content is one `text` part (the extracted field, or
the body as-is, or the body plus `[response was cut off at N bytes]`). An
image tool's content is an `image` part (`data`, `mimeType`) followed by a
one-line text note. Citation rows ride `structuredContent.sources` as
`[{title, url}]`.

### `_meta["enclave.host/tool"]`

What eyesoff-ai needs beyond the schema, on every listed tool; any client
that does not know the key ignores it:

```json
{ "group": "images", "images": true, "result": "image", "timeout_s": 120,
  "max_chars": 6000, "format": "…", "route": "…", "route_arg": "q", "user": true }
```

Only the keys that apply are present. `group` is always there (the entry's
own, else eyesoff-ai's rule: pictures are `images`, a family prefix shared
with a sibling is the family, else the function name).

## Identity

Three ways a request reaches per-user tools; every other request is
nameless and sees only the shared ones.

| header | who | when |
| --- | --- | --- |
| `X-Api-Key: <key>` + `X-User: <sub>` | a service asserting the account | eyesoff-ai (its `"x-user": "$user"` slot), Claude Code with `--header` |
| `X-Api-Key: <key>` + `X-Sso-Token: EST1…` | the signed-in person, verified here | needs the `sso` block; the token's audience must be this deployment or one in `accept` |
| `Authorization: Bearer EST1…` | the same, off-platform | the enclave.host gateway consumes this header |

The key is the gate in every row: a sign-in token establishes **who**, never
**whether**. A deployment with no `api_key` has no gate, and a token there is
simply a name.

`sub` is a lowercase `0x` wallet address or an `acct_…` id; anything else is
a 400. The identity reaches an endpoint only through a header the entry
wrote as `"$user"`, never through the arguments.

## Wiring eyesoff-ai

`GET /api/tools` (with the key) answers with a ready-to-paste entry:

```json
{ "tools": { "mcp": [ {
  "url": "https://<adapter-id8>.app.enclave.host/mcp",
  "handshake": false,
  "headers": { "x-api-key": "$MCP_ADAPTER_API_KEY", "x-user": "$user" },
  "groups": { "images": ["generate_image", "upscale_image"], "notes": ["notes_read", "notes_write"] }
} ] } }
```

Put it in the eyesoff-ai deployment's `tools` block **in place of** the
`http` array (keep `builtin`, `max_calls`, `default_on` and the rest), set
the `MCP_ADAPTER_API_KEY` secret on the eyesoff-ai deployment to this
adapter's key, and check `GET /tools` on the eyesoff-ai deployment: it
dials this server and lists what the model will be offered.

`scripts/from-eyesoff-config.py` does the split mechanically, so an existing
chat config does not have to be edited by hand:

```sh
./scripts/from-eyesoff-config.py eyesoff.json --url https://<adapter-id8>.app.enclave.host
# wrote eyesoff.adapter.json  (8 entries, 3 groups: images, virtual_machine, notebook)
# wrote eyesoff.eyesoff.json  (tools.http replaced by one tools.mcp entry)
# secrets the ADAPTER deployment needs: IMAGE_ENDPOINT, JOT_API_KEY, …
```

The entries move across byte for byte (the two apps take the same entry
shape), the groups map is computed by eyesoff-ai's own rule so the switches
do not move, and the script prints which secrets go to which deployment.

- `handshake: false` skips `initialize`, so discovery is one request per
  armed turn and a call is one request. The default (`true`) is the
  protocol's three-trip handshake, for servers that need it.
- `groups` is the static map eyesoff-ai's settings panel needs before any
  turn dials the server: the same switches the `http` block would have
  produced (the adapter computes them by eyesoff-ai's own rule). At turn
  time the config's map wins, then a tool's `_meta.group`, then the server's
  own group.
- `"x-user": "$user"` is filled from the sign-in eyesoff-ai verified for the
  turn and omitted when there is none; this server then lists no per-user
  tool, so the model is never shown one it cannot use.
- Picture tools work as before: eyesoff-ai reads `_meta.images` and
  `_meta.result`, hands the turn's attachments to the call, delivers an
  image result to the chat, and stands its own image pre-pass down.
- `timeout_s` on an entry is honoured by eyesoff-ai per call (an image
  generation's 180 s is not cut by the server-wide 20).

## Other clients

```sh
claude mcp add --transport http eyesoff-tools https://<id8>.app.enclave.host/mcp \
  --header "X-Api-Key: <key>" --header "X-User: 0x<your wallet>"
```

```sh
curl -s https://<id8>.app.enclave.host/mcp -H 'content-type: application/json' -H 'x-api-key: <key>' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
curl -s https://<id8>.app.enclave.host/mcp -H 'content-type: application/json' -H 'x-api-key: <key>' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"notes_read","arguments":{"name":"a.md"}}}' \
  -H 'x-user: 0x…'
```

`GET /api/tools?call=<name>&args=<json>` runs one tool outside MCP, which
separates "can the adapter see the tool" from "does the endpoint work"; the
page at `/` wraps it.

## Try it locally

```sh
cargo component build --release --target wasm32-wasip2
export MCP_ADAPTER_API_KEY=devkey IMAGE_API_KEY=… JOT_API_KEY=…
wasmtime serve -Scommon --addr 127.0.0.1:8080 \
  --env ENCLAVE_CONFIG="$(cat my-config.json)" \
  --env MCP_ADAPTER_API_KEY --env IMAGE_API_KEY --env JOT_API_KEY \
  target/wasm32-wasip2/release/api_mcp_adapter.wasm
```

Three harnesses, all against the real `wasmtime serve`:

- **`scripts/e2e.sh`** — the whole contract, with a stub backend
  (`scripts/stub_backend.py`) and an MCP client (`scripts/mcp_client.py`):
  the protocol surface, the key gate, per-user visibility and fail-closed
  identity, sign-in tokens minted the way the platform mints them (when
  `cast` is on PATH), every templating rule, pictures in and out, sources,
  caps, HTTP errors, missing secrets, and a locked deployment.
- **`scripts/interop.sh`** — the cross-app proof: **eyesoff-ai's own MCP
  client**, unchanged, driven over a socket against a live adapter. e2e.sh
  can only show this server is self-consistent; this shows the two apps
  actually agree, which is the claim that matters.
- **`scripts/ui.sh`** — the page at `/` in a real browser (skips cleanly
  when playwright is not installed; point `PLAYWRIGHT_DIR` at a checkout
  that has it).

Needs `wasmtime`, `python3`, `cargo-component`, and `node` for the last one.
The unit tests run on the host with no network: `cargo test --lib`.

## Publish and deploy

```sh
enclave publish target/wasm32-wasip2/release/api_mcp_adapter.wasm --slug api-mcp-adapter --version 1.0.0 \
  --name api-mcp-adapter --desc "Any HTTP API as an MCP server" \
  --mem 128 --cpu-gflops 1 --config "$(cat assets/deploy-config.template.json)"
enclave deploy api-mcp-adapter --fund 5 --secrets MCP_ADAPTER_API_KEY=… --secrets IMAGE_API_KEY=… \
  --secrets RISCBOX_API_KEY=… --secrets JOT_API_KEY=…
```

The version's config is a template; a deployer overrides the entries per
deployment through the config override (the console's Config panel or
`enclave config set`). Outbound egress on the fleet is IPv6-only, so a tool
host needs an AAAA record (every Enclave app URL has one); `dig AAAA <host>`
is the check, and a refused connection says so in the tool's error.

## Limits

- No MCP resources or prompts, no server-initiated stream, no upstream MCP
  servers (this adapter fronts HTTP APIs; an MCP server is already an MCP
  server).
- One response is buffered whole before it is looked at (`max_bytes`); a
  streaming endpoint is read to the end or the cap.
- The routed pre-pass (`route`) is eyesoff-ai's own and runs only its http
  entries; the fields are carried through for a client that wants them.
- A tool a client discovers that its `groups` map does not name falls back to
  the tool's own `_meta.group` and then to the server's own. eyesoff-ai gives
  a discovering server a **catch-all switch** named for its host so such a
  tool is still controllable; this adapter always writes a complete map
  (`GET /api/tools`), so that switch normally governs nothing and exists for
  the case where this server grows a tool the pasted entry predates.
