# jot: a notebook your agent keeps in your own bucket

jot is a note store for programs. An agent with the key can **list, read,
write, append, search and delete** notes through a small JSON API, and every
note is one plain object in an S3-compatible bucket you own (Cloudflare R2,
AWS S3, Wasabi, minio). Nothing to unlock, nothing that dies on a restart:
the durable copy is the object, and the app holds no state of its own.

It is the storage shape of [risc-box](../risc-box) (the app signs its own
SigV4 requests to your bucket) applied to the problem [keep](../keep) solves
for a person (a notebook in the enclave). keep is for you, in a browser, with
your wallet as the key; jot is for the agent, over HTTP, with a bearer key as
the key. Pick keep when the notes must be unreadable at rest; pick jot when a
program must be able to write them unattended.

```
your agent                  the enclave (jot)                     your bucket
  GET/PUT /api/notes/<n>  ──►  bearer key checked,      ──SigV4──►  <prefix>/<n>
  Authorization: Bearer        one object per note                  plain objects,
  (LangGraph tool, eyesoff-ai  read/written over wasi:http          readable with
   tools.http, curl…)          through the fleet's egress           any S3 tool
```

## The trust shape, stated plainly

- **The credentials and the key live only in the attested guest.** The App
  Config names the bucket; the S3 access key, secret and the API key are
  referenced as `$VAR` deployment secrets, injected as guest env by the
  enclave holding the lease. They never sit on-chain and never reach a
  browser.
- **The code holding them is the code the catalog pins.** Remote attestation
  covers this exact build, so "the thing with my S3 secret only does S3 GET,
  PUT, DELETE and LIST under one prefix" is a property you can verify.
- **The bucket sees plain objects and this deployment's egress IP.** Notes
  are deliberately NOT encrypted: the point is a notebook you can also open
  with `aws s3 cp`, `rclone`, or the R2 dashboard. If that is the wrong
  trade for the material, keep's encrypted volume is the other answer.
- **A read-only deployment cannot write.** `readOnly: true` in the config
  turns every write and delete into a 403 at the app, regardless of what the
  credentials allow; pair it with a read-only bucket token for defence in
  depth.

## Configuration

The deployment's App Config (`ENCLAVE_CONFIG`; locally `JOT_CONFIG`):

```json
{
  "title": "agent notebook",
  "endpoint": "https://<account>.r2.cloudflarestorage.com",
  "region": "auto",
  "bucket": "agent-notes",
  "prefix": "notes/",
  "credentials": { "accessKeyId": "$JOT_ACCESS_KEY_ID", "secretAccessKey": "$JOT_SECRET_ACCESS_KEY" },
  "api_key": "$JOT_API_KEY",
  "readOnly": false
}
```

- `endpoint` is the S3 API origin, no path; requests are path-style
  (`/bucket/key`). `region` is what the endpoint signs for (`auto` for R2,
  `us-east-1` for minio and most S3-compatibles, the real region on AWS).
- `prefix` scopes the notebook to one directory of the bucket (a trailing
  slash is added). Only keys under it are ever listed, read or written.
- Any string value may be written as `$NAME` (or `${NAME}`) and resolves
  from the app's environment at request time, which is where deployment
  secrets arrive. Whole-value references only. An unresolved reference reads
  as absent and is named in `/api/status` and the logs.
- **Credentials** are optional: a public-read bucket needs none for reads
  (requests go unsigned; writes will fail). Set them as secrets:
  `enclave secrets set <id> JOT_ACCESS_KEY_ID=… JOT_SECRET_ACCESS_KEY=… JOT_API_KEY=… --restart`.
- `api_key` is optional but **required on any deployment an agent reaches
  over the public URL**: without it, anyone who finds the URL can read and
  write the notebook. Clients send `Authorization: Bearer <key>` or
  `X-Api-Key: <key>`. With it set, only `/`, `/ping`, `/api/tools` and the
  public half of `/api/status` answer without the key.
- The app **always starts**, even unconfigured: it serves the UI, reports
  the missing fields in `/api/status`, and answers 503 on every note route
  until the config and secrets are set and the deployment restarted.

On R2: create the bucket, then an API token scoped to **Object Read & Write**
on that bucket only; the token's access key id and secret are the two
credentials. The S3 endpoint is
`https://<account id>.r2.cloudflarestorage.com` with region `auto`. R2
publishes AAAA records, which matters: a deployment's outbound egress is
IPv6-only, so an endpoint with no AAAA record cannot be reached at all (the
error says so and names the host to `dig`).

## Routes

Everything is JSON except where noted. Note names are relative paths of
letters, digits, `- _ . space`, joined by single slashes (`projects/enclave.md`,
`meeting notes/2026-09-01.md`); no `.` or `..` segments, at most 200 bytes.
A note caps at 1 MiB.

| route | what |
|---|---|
| `GET /` | the notebook UI (self-contained HTML; paste the key, browse, edit) |
| `GET /ping` | liveness |
| `GET /api/status` | `{configured, missing, auth, readOnly}`; with the key also `{endpoint, region, bucket, prefix, signed}` |
| `GET /api/tools` | the six verbs as OpenAI function schemas and as an eyesoff-ai `tools.http` block (see below) |
| `GET /api/notes?prefix=&limit=` | `{notes: [{name, size, modified, etag}], truncated}` (limit 1..1000, default 200) |
| `GET /api/notes/<name>` | `{name, content, size, etag, modified}`; `?raw=1` (or `Accept: text/plain`) returns the bytes with a content type from the extension |
| `PUT /api/notes/<name>` | write: JSON `{content, ifMatch?}`, or any non-JSON body as the note text (an `If-Match` header works too). `POST` is an alias. Answers `{ok, name, size, etag}`; a stale `ifMatch` is 412 |
| `POST /api/notes/<name>/append` | `{content}`: append a paragraph (a newline is inserted between), creating the note if needed. Conditional on the ETag it just read: a concurrent change answers 409, retry |
| `DELETE /api/notes/<name>` | delete (idempotent). `POST /api/notes/<name>/delete` is an alias for clients without DELETE |
| `GET /api/search?q=&prefix=&limit=` | case-insensitive substring search over note bodies: `{hits: [{name, line, text}], scanned, skipped, truncated}`. Scans up to 200 notes of at most 256 KiB each |

Errors are `{"error": {"message": …}}` with the obvious statuses: 400 bad
name or body, 401 no or wrong key, 403 read-only, 404 no such note, 409/412
conditional write lost, 413 over 1 MiB, 502 the bucket refused or was
unreachable (the message quotes S3's own reason), 503 not configured.

## Using it from an agent

**Any OpenAI-style tool binding.** `GET /api/tools` returns `openai`, a list
of six function schemas (`notes_list`, `notes_read`, `notes_write`,
`notes_append`, `notes_search`, `notes_delete`) to hand to the model, plus
`base_url` and the auth line. Execute each call as the matching route.

**The sibling [agent](../agent) (LangGraph).** Its tool belt already carries
the six tools; they are offered when the environment names a notebook:

```sh
ENCLAVE_AGENT_NOTES_URL=https://<id8>.app.enclave.host \
ENCLAVE_AGENT_NOTES_KEY=<the api_key> \
.venv/bin/enclave-agent -p "Note down that the fleet's egress is IPv6-only, under infra/egress.md"
```

**An eyesoff-ai deployment, server-side.** eyesoff-ai's tool registry calls
plain HTTP endpoints named in ITS config, with `{arg}` URL placeholders and
`$SECRET` headers, so the whole wiring is configuration: `GET /api/tools`
returns an `eyesoff_ai` block, already filled with this deployment's URL,
that you merge into the eyesoff-ai deployment's App Config, then set
`JOT_API_KEY` as that deployment's secret. The model then reads and writes
this notebook from inside its own enclave; the notes travel enclave to
enclave and land in your bucket, and the conversation never leaves eyesoff-ai
(only the arguments the model chose cross). One entry, for the shape:

```json
{ "tools": { "http": [
  { "name": "notes_append",
    "description": "Append a paragraph to a note, creating it if needed.",
    "method": "POST",
    "url": "https://<id8>.app.enclave.host/api/notes/{name}/append",
    "headers": { "authorization": "Bearer $JOT_API_KEY" },
    "parameters": { "type": "object",
      "properties": { "name": { "type": "string" }, "content": { "type": "string" } },
      "required": ["name", "content"] },
    "body": { "content": "$content" } }
] } }
```

**curl**, for everything else:

```sh
J=https://<id8>.app.enclave.host; K="authorization: Bearer $JOT_API_KEY"
curl -s -H "$K" $J/api/notes
curl -s -H "$K" -X PUT -H 'content-type: application/json' \
     -d '{"content":"# Enclave\n\nthe fleet egress is IPv6-only"}' $J/api/notes/infra/egress.md
curl -s -H "$K" -X POST -H 'content-type: application/json' \
     -d '{"content":"2026-09-01: R2 has AAAA records, so it works"}' $J/api/notes/infra/egress.md/append
curl -s -H "$K" "$J/api/search?q=ipv6"
curl -s -H "$K" $J/api/notes/infra/egress.md?raw=1
curl -s -H "$K" -X DELETE $J/api/notes/scratch.md
```

## Try it locally

```sh
cargo component build --release --target wasm32-wasip2
# a bucket to talk to: minio, an R2 token, anything S3-compatible
export JOT_ACCESS_KEY_ID=… JOT_SECRET_ACCESS_KEY=… JOT_API_KEY=devkey
wasmtime serve -Scommon --addr 127.0.0.1:8080 \
  --env ENCLAVE_CONFIG='{"endpoint":"http://127.0.0.1:9000","region":"us-east-1","bucket":"notes","prefix":"agent/","credentials":{"accessKeyId":"$JOT_ACCESS_KEY_ID","secretAccessKey":"$JOT_SECRET_ACCESS_KEY"},"api_key":"$JOT_API_KEY"}' \
  --env JOT_ACCESS_KEY_ID --env JOT_SECRET_ACCESS_KEY --env JOT_API_KEY \
  target/wasm32-wasip2/release/jot.wasm
```

`scripts/e2e.sh` runs the whole contract against a local minio through the
real `wasmtime serve`: every route, the key gate, the name filter, the size
cap, conditional writes, read-only and unconfigured deployments, and the
claim that matters (a note written through the API is byte-for-byte the
object in the bucket). Needs `minio`, `wasmtime`, `curl` and `python3` on
PATH. The in-crate unit tests run under wasmtime too:
`CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -Shttp" cargo test --target wasm32-wasip2`.

## Publish and deploy

```sh
enclave publish target/wasm32-wasip2/release/jot.wasm --slug jot --version 0.1.0 \
  --name jot --desc "A notebook your agent keeps in your own bucket" \
  --mem 128 --cpu-gflops 1 \
  --config '{"endpoint":"https://<account>.r2.cloudflarestorage.com","region":"auto","bucket":"agent-notes","prefix":"notes/","credentials":{"accessKeyId":"$JOT_ACCESS_KEY_ID","secretAccessKey":"$JOT_SECRET_ACCESS_KEY"},"api_key":"$JOT_API_KEY"}'
enclave deploy jot --fund 5 --secrets JOT_ACCESS_KEY_ID=… --secrets JOT_SECRET_ACCESS_KEY=… --secrets JOT_API_KEY=…
```

The version's config is a template: a deployer overrides `endpoint`,
`bucket` and `prefix` per deployment (console Config panel or `enclave config
set`) and keeps the `$VAR` references, so one catalog entry serves every
bucket. The app needs no GPU, no volumes, no ports beyond its HTTP port, and
it makes no request except to the configured endpoint.

## Limits and honest notes

- Notes are whole objects: a write replaces, an append is read + write with
  an `If-Match`. There is no history and no locking beyond that ETag; two
  agents appending to the same note at once is safe (one gets a 409 and
  retries), two agents *writing* the same note race unless they pass
  `ifMatch`.
- Search is a scan (up to 200 notes, 256 KiB each, one GET per note). It is
  right for a notebook and wrong for an archive; keep the prefix small or
  give the agent several deployments.
- Minio, R2 and AWS S3 all honour `If-Match` on PUT. A store that ignores it
  makes conditional writes unconditional; the e2e prints a WARN if yours does.
- The bucket is part of your trust base: it can serve a different note than
  the one written. Prefer an `https` endpoint and a token scoped to the one
  bucket and prefix.

Verify the enclave before trusting it: [enclave.host](https://enclave.host).
