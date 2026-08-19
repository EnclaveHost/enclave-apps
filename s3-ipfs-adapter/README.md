# s3-ipfs-adapter: an S3 bucket, exposed as IPFS content from an enclave

Point it at an S3-compatible bucket (the risc-box S3 path: SigV4 over the
platform's transparent egress) and it becomes an IPFS gateway for that
bucket: every object gets the exact CID that `ipfs add --cid-version 1`
would mint, the whole bucket gets a root directory CID, and everything is
served over the standard gateway surface, path and trustless alike.

```
      S3 bucket                    the enclave                      any client
  objects, ranged GETs   <──SigV4──  index: dag-pb skeleton   ──>  /ipfs/<cid>/path
  (bytes never copied)              + 32 B digest per 256 KiB      ?format=raw | car
                                      chunk; bytes re-hashed        HTTP ranges, HEAD
                                      before every serve            UI with per-file CIDs
```

## Why CIDs from `ipfs add`, exactly

The import parameters are pinned to kubo's defaults and not configurable:
256 KiB chunks, raw leaves, balanced DAG with 174 links per node, CIDv1,
sha2-256. Pinning them is the point: the CID this app publishes for an
object is the same CID anyone gets adding the same file anywhere else, so
content can be cross-checked, pinned on any other node, or fetched and
verified without trusting this gateway or the bucket. `scripts/e2e.sh`
proves it block-for-block against a real kubo (root CID equality is a
merkle proof for every block underneath; the CAR it serves imports into
kubo, which re-verifies every hash on the way in).

The enclave supplies the other half of the trust story: the attested build
is what listed the bucket and computed the index, so the published root CID
is a faithful commitment to the bucket's contents at index time; a client
that checks hashes (any IPFS client) needs to trust neither this app nor S3
for integrity, only for availability.

## What it holds, what it fetches

Only the merkle skeleton lives in memory: dag-pb nodes plus one 32-byte
digest per chunk (about 13 MB of index per 100 GB of bucket). File bytes
stay in S3 and are fetched by byte range on demand, then re-hashed against
the index before a single byte leaves the app; an object that changed under
the index surfaces as a truncated response and a log line, never as wrong
bytes under a valid CID. Indexing shares the single-threaded event loop,
one bounded S3 request per tick, so the UI and gateway serve while a large
bucket indexes; a refresh (timer or `POST /api/refresh`) re-hashes only
objects whose size or ETag changed, and in-flight downloads keep streaming
from the snapshot they started in.

## API

| route | what |
|---|---|
| `GET /ipfs/<cid>` | file bytes by CID (any file root, chunk, or dag node) |
| `GET /ipfs/<cid>/<path>` | path resolution through UnixFS directories; `index.html` served for dirs that have one, an HTML listing otherwise |
| `?format=raw` (or `Accept: application/vnd.ipld.raw`) | one verified block |
| `?format=car` (or `Accept: application/vnd.ipld.car`) | the DAG as a CARv1 stream, DFS, deduplicated |
| `?filename=x&download=1` | content-disposition helpers |
| `GET /api/status` | index state, progress, root CID |
| `GET /api/files` | `[{path, size, cid}]` |
| `POST /api/refresh` | re-list now |
| `POST /api/upload?path=<path>` | body = raw bytes; PUT into the bucket (32 MiB cap), then re-index |
| `POST /api/delete` | body `path=<path>`; DELETE from the bucket, then re-index |
| `GET /` UI, `GET /ping` liveness | |

With an `upload` config block (below), the app additionally serves the
Enclave pin routes — the wire contract of the platform's validating upload
gateway, so `ipfs.enclave.host` can point at this app and every publish
client keeps working unchanged:

| route | what |
|---|---|
| `POST /add-wasm` | a wasm component (Tier-1 validated: preamble + layer; wasm64 core-module carve-out), streamed through an S3 multipart upload — a 2 GiB body never lives in guest memory. Answers `{cid, wasi, world, threads, set, mem64}` (the wasi-world classifier rides back for claim routing) |
| `POST /add-json` | an app-config JSON OBJECT (catalog rev-7 large configs), 1 MiB cap in lockstep with the runner's `CONFIG_MAX_BYTES` |
| `POST /add-image` | an app thumbnail/banner: raster by magic bytes, or SVG through the strict fail-closed validator (reject, never sanitize). Answers `{cid, svg}` |
| `GET /healthz` | `{"ok":true}` — the gateway liveness contract deploy tooling waits on |

All three pin routes require the platform's wallet-signed upload token
(`x-upload-address` / `x-upload-expiry` / `x-upload-token`, minted by the
api-relay as `HMAC-SHA256(uploadKey, "<address>:<sha256(bytes)>:<expiry>")`),
enforce per-wallet and global daily byte budgets, and echo CORS for the
configured origins. Pins land in the bucket at `pins/<cid>` — the CID is
computed by the same code that serves it, and a freshly pinned CID resolves
immediately (the snapshot is patched without waiting for a LIST). Gateway
responses carry `Content-Security-Policy: sandbox` + nosniff, the second
layer behind the SVG validator.

The UI is also a small bucket browser: folder navigation with breadcrumbs,
upload-into-the-current-folder, per-file delete, and a whole-bucket search.
The three mutating routes (refresh, upload, delete) honor `api_key`.

Gateway responses carry `etag`, immutable `cache-control`, `x-ipfs-path`,
`x-ipfs-roots`, `accept-ranges`; single absolute HTTP ranges and HEAD are
supported on file responses.

## Config

`ENCLAVE_CONFIG` (or `S3IPFS_CONFIG` locally), JSON. `$NAME` string values
resolve from the environment, which is how deployment secrets arrive:

```json
{
  "title": "machine images",
  "endpoint": "https://<account>.r2.cloudflarestorage.com",
  "region": "auto",
  "bucket": "machines",
  "prefix": "public/",
  "credentials": {
    "accessKeyId": "$S3_ACCESS_KEY_ID",
    "secretAccessKey": "$S3_SECRET_ACCESS_KEY"
  },
  "refreshSecs": 300,
  "maxKeys": 50000,
  "api_key": "$API_KEY",
  "upload": {
    "uploadKey": "$UPLOAD_KEY",
    "allowOrigins": ["https://enclave.host"],
    "maxWasmBytes": 2147483648,
    "maxImageBytes": 4194304,
    "maxJsonBytes": 1048576,
    "perAddrDailyBytes": 4294967296,
    "globalDailyBytes": 17179869184,
    "jsonPerIpHourly": 60
  }
}
```

The `upload` block is optional; without it the pin routes answer 503.
`uploadKey` is the HMAC secret shared with the api-relay (reference a
deployment secret, never inline it). All other keys default to the values
shown. `allowUnsigned: true` opens the routes with no token — dev/e2e only,
never on a public deployment.

`endpoint` and `bucket` are required; the app still starts without them and
says so in the UI. Omit `credentials` for a public bucket (requests go
unsigned). `refreshSecs: 0` disables the timer (manual refresh only).
`api_key` is a shared secret of your choosing; when set, it protects the
mutating routes: refresh, upload, delete (reads stay open, they only serve
content-addressed data). Present it as `X-Api-Key: <key>` or `?key=<key>`;
on the fleet the TLS proxy strips `Authorization`, so Bearer only works
locally. A public deployment without an `api_key` leaves the bucket
writable by anyone: set one.

## Limits, by design

- **This is the HTTP shape of an IPFS node, not a libp2p peer.** wasip2 is
  single-threaded with no inbound TCP, so there is no DHT to announce to;
  content is fetched from THIS gateway by URL (or CAR-imported into any
  node, after which it travels the network normally). Because the CIDs are
  kubo-identical, the same data added anywhere else is the same content.
- Directories are plain UnixFS. Past ~1 MiB of directory block (thousands
  of entries in one dir) kubo would HAMT-shard and CIDs would diverge; the
  app logs a warning when that line is crossed.
- `maxKeys` caps the index (default 50k objects); the status reports
  `truncated: true` when the cap hit.
- Changed objects are detected by size+ETag at refresh time, and by hash
  verification at serve time.

## Build & test

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/s3-ipfs-adapter.wasm

cargo test                 # unit: CID vectors, dag-pb, balanced layout, SigV4 query
scripts/e2e.sh             # full proof: minio + wasmtime + kubo comparison
```

Local run against any S3:

```bash
wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8000=18400' \
  --env 'ENCLAVE_CONFIG={"endpoint":"http://127.0.0.1:9000","region":"us-east-1","bucket":"testbkt","credentials":{"accessKeyId":"...","secretAccessKey":"..."}}' \
  target/wasm32-wasip2/release/s3-ipfs-adapter.wasm
# open http://127.0.0.1:18400
```

## Deploy on enclave.host

Publish the component (guide topic "publish"), then deploy CPU-only with
port `http:8000` (fleet policy: never 8080), egress enabled for the S3
endpoint, and the config above with `$VAR` secrets set on the deployment.
The engine (`src/httpd.rs`) reads `ENCLAVE_PORTS` and binds the actual
port, as ever.
